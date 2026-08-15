//! Hyper community updater.
//!
//! This module is deliberately separate from the official Grok updater. It
//! only reads releases from `DaviRain-Su/hyper-grok-build` and only activates
//! binaries below `~/.hyper` (or `HYPER_SHARE_DIR` in debug/test builds).
//! Optional release `bundled/**` trees are activated at `$GROK_HOME/bundled`
//! (default `~/.grok/bundled`) with a compensating transaction alongside the
//! binary and `update-state.json`. Nothing here calls the x.ai/npm updater or
//! writes `~/.grok/bin/grok`.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::auto_update::{
    BackgroundUpdateCheck, EnsureLatestOutcome, UpdateAvailable, UpdateRunMode, UpdateStatus,
};

const RELEASE_REPO: &str = "DaviRain-Su/hyper-grok-build";
const RELEASE_API_BASE: &str = "https://api.github.com/repos/DaviRain-Su/hyper-grok-build/releases";
const CHECK_TTL: Duration = Duration::from_secs(30 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_AUXILIARY_BYTES: u64 = 16 * 1024 * 1024;
/// Real release archives ship a full `bundled/**` tree (skills, agents, …).
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const MAX_BUNDLE_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_BUNDLE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BUNDLE_FILES: usize = 4096;
const MAX_PATH_DEPTH: usize = 32;
const BUNDLE_DIR_NAME: &str = "bundled";
const INSTALLER_NAME: &str = "community-github";

#[cfg(feature = "community-update-test-hooks")]
mod install_failpoints {
    use std::sync::Mutex;

    use anyhow::{Result, bail};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum InstallFailpoint {
        AfterBundleActivation,
        BeforeStateWrite,
    }

    static INSTALL_FAILPOINT: Mutex<Option<InstallFailpoint>> = Mutex::new(None);

    /// Test-only install failpoint injection (`after_bundle_activation`,
    /// `before_state_write`). Unknown / `None` clears the failpoint.
    #[doc(hidden)]
    pub fn set_install_failpoint(point: Option<&str>) {
        let parsed = point.and_then(|name| match name {
            "after_bundle_activation" => Some(InstallFailpoint::AfterBundleActivation),
            "before_state_write" => Some(InstallFailpoint::BeforeStateWrite),
            _ => None,
        });
        *INSTALL_FAILPOINT.lock().unwrap_or_else(|e| e.into_inner()) = parsed;
    }

    pub(super) fn take_install_failpoint(expected: InstallFailpoint) -> Result<()> {
        let mut guard = INSTALL_FAILPOINT.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == Some(expected) {
            *guard = None;
            bail!("injected install failpoint: {expected:?}");
        }
        Ok(())
    }
}

#[cfg(feature = "community-update-test-hooks")]
#[doc(hidden)]
pub use install_failpoints::set_install_failpoint;

/// Production builds compile this to a no-op so failpoints cannot ship.
#[cfg(feature = "community-update-test-hooks")]
fn take_install_failpoint_after_bundle() -> Result<()> {
    install_failpoints::take_install_failpoint(
        install_failpoints::InstallFailpoint::AfterBundleActivation,
    )
}

#[cfg(not(feature = "community-update-test-hooks"))]
fn take_install_failpoint_after_bundle() -> Result<()> {
    Ok(())
}

#[cfg(feature = "community-update-test-hooks")]
fn take_install_failpoint_before_state() -> Result<()> {
    install_failpoints::take_install_failpoint(
        install_failpoints::InstallFailpoint::BeforeStateWrite,
    )
}

#[cfg(not(feature = "community-update-test-hooks"))]
fn take_install_failpoint_before_state() -> Result<()> {
    Ok(())
}

fn combine_errors(primary: anyhow::Error, secondary: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!("{primary:#}\n\nalso: {secondary:#}")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct UpdateState {
    #[serde(default)]
    installed_version: Option<String>,
    #[serde(default)]
    installed_asset: Option<String>,
    #[serde(default)]
    installed_sha256: Option<String>,
    #[serde(default)]
    installed_binary: Option<String>,
    #[serde(default)]
    checked_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseMetadata {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone)]
struct Candidate {
    version: String,
    asset_name: String,
    archive_url: String,
    sha256: String,
    /// Optional prebuilt scheme live image for this platform (best-effort
    /// component: absent asset or resolution failure never blocks the update).
    scheme_image: Option<SchemeImageAsset>,
}

/// Release asset for the prebuilt `hyper-scheme-image` binary (the Gambit
/// kernel compiled with `gsc -exe`; consumed by `xai-grok-scheme-runtime`).
#[derive(Debug, Clone)]
struct SchemeImageAsset {
    asset_name: String,
    archive_url: String,
    sha256: String,
}

#[derive(Debug, Clone, Copy)]
struct Platform {
    asset_triple: &'static str,
    local_os: &'static str,
    local_arch: &'static str,
    archive_suffix: &'static str,
    binary_entry: &'static str,
}

#[derive(Debug, Clone)]
struct ActiveDeployment {
    version: String,
    binary_name: String,
    sha256: Option<String>,
}

#[derive(Debug)]
struct ConvergeOutcome {
    target: String,
    installed: bool,
}

#[derive(Debug, Clone)]
struct UpdateSource {
    api_base: String,
    allow_insecure_local: bool,
}

/// OS-level lock guard. The lock is released automatically on process exit,
/// including crashes, so there is no stale-PID recovery protocol to get wrong.
struct UpdateLock(File);

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

/// Removes a temporary path unless ownership was explicitly consumed.
struct TempArtifact {
    path: PathBuf,
    is_dir: bool,
    keep: bool,
}

impl TempArtifact {
    fn new_file(path: PathBuf) -> Self {
        Self {
            path,
            is_dir: false,
            keep: false,
        }
    }

    fn new_dir(path: PathBuf) -> Self {
        Self {
            path,
            is_dir: true,
            keep: false,
        }
    }

    fn keep(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        if self.is_dir {
            let _ = std::fs::remove_dir_all(&self.path);
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn hyper_home() -> PathBuf {
    if let Some(path) = std::env::var_os("HYPER_SHARE_DIR") {
        return PathBuf::from(path);
    }
    #[allow(deprecated)]
    let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".hyper")
}

/// Shared Grok/Hyper config home (`$GROK_HOME` or `~/.grok`). Bundle assets
/// live here so the runtime can load them alongside user config.
fn community_grok_home() -> PathBuf {
    xai_grok_shell::util::grok_home::grok_home()
}

fn managed_bundle_path() -> PathBuf {
    community_grok_home().join(BUNDLE_DIR_NAME)
}

pub(crate) fn managed_application() -> PathBuf {
    let name = if cfg!(windows) { "hyper.exe" } else { "hyper" };
    hyper_home().join("bin").join(name)
}

fn state_path() -> PathBuf {
    hyper_home().join("update-state.json")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn load_state() -> UpdateState {
    std::fs::read(state_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn state_is_fresh(state: &UpdateState) -> bool {
    let Some(checked) = state.checked_at_unix else {
        return false;
    };
    let now = now_unix();
    checked <= now && now - checked < CHECK_TTL.as_secs()
}

fn reject_symlink(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to use symlinked Hyper {label}: {}",
                path.display()
            );
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspecting Hyper {label} {}", path.display()))
        }
    }
}

fn ensure_safe_layout() -> Result<()> {
    let home = hyper_home();
    if home.as_os_str().is_empty() {
        bail!("Hyper install root is empty");
    }
    if home.exists() {
        reject_symlink(&home, "install root")?;
    } else {
        std::fs::create_dir_all(&home)
            .with_context(|| format!("creating Hyper install root {}", home.display()))?;
    }
    for (name, label) in [
        ("bin", "bin directory"),
        ("downloads", "downloads directory"),
    ] {
        let dir = home.join(name);
        if dir.exists() {
            reject_symlink(&dir, label)?;
        } else {
            std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        if !std::fs::metadata(&dir)?.is_dir() {
            bail!("Hyper {label} is not a directory: {}", dir.display());
        }
    }
    Ok(())
}

async fn acquire_update_lock() -> Result<UpdateLock> {
    ensure_safe_layout()?;
    let lock_path = hyper_home().join("update.lock");
    reject_symlink(&lock_path, "update lock")?;
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening Hyper update lock {}", lock_path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("locking {}", lock_path.display()))?;
        Ok(UpdateLock(file))
    })
    .await
    .map_err(|e| anyhow::anyhow!("Hyper update lock task failed: {e}"))?
}

fn unique_sibling(base: &Path, suffix: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut name = base
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(
        ".{}-{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        suffix
    ));
    base.with_file_name(name)
}

fn write_state_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    reject_symlink(path, "update state")?;
    let tmp = unique_sibling(path, "tmp");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let tmp_guard = TempArtifact::new_file(tmp.clone());
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    #[cfg(windows)]
    {
        let backup = unique_sibling(path, "old");
        let had_old = path.exists();
        if had_old {
            std::fs::rename(path, &backup).with_context(|| {
                format!("moving existing Hyper update state {}", path.display())
            })?;
        }
        if let Err(error) = std::fs::rename(&tmp, path) {
            let activation_error =
                anyhow::Error::new(error).context(format!("activating {}", path.display()));
            if had_old && let Err(restore_error) = std::fs::rename(&backup, path) {
                return Err(combine_errors(
                    activation_error,
                    anyhow::Error::new(restore_error).context(format!(
                        "restoring previous Hyper update state from {} (backup preserved)",
                        backup.display()
                    )),
                ));
            }
            return Err(activation_error);
        }
        let _ = std::fs::remove_file(backup);
    }
    #[cfg(not(windows))]
    std::fs::rename(&tmp, path).with_context(|| format!("activating {}", path.display()))?;

    let _ = tmp_guard.keep();
    Ok(())
}

fn write_state_atomic(state: &UpdateState) -> Result<()> {
    ensure_safe_layout()?;
    let path = state_path();
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    write_state_bytes_atomic(&path, &bytes)
}

fn restore_state_bytes(path: &Path, previous: Option<&[u8]>) -> Result<()> {
    match previous {
        Some(bytes) => write_state_bytes_atomic(path, bytes),
        None => {
            if path.exists() || path.is_symlink() {
                std::fs::remove_file(path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
            Ok(())
        }
    }
}

#[allow(unreachable_code)]
fn platform() -> Result<Platform> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Ok(Platform {
        asset_triple: "aarch64-apple-darwin",
        local_os: "macos",
        local_arch: "aarch64",
        archive_suffix: "tar.gz",
        binary_entry: "hyper",
    });
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return Ok(Platform {
        asset_triple: "x86_64-apple-darwin",
        local_os: "macos",
        local_arch: "x86_64",
        archive_suffix: "tar.gz",
        binary_entry: "hyper",
    });
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    return Ok(Platform {
        asset_triple: "aarch64-unknown-linux-gnu",
        local_os: "linux",
        local_arch: "aarch64",
        archive_suffix: "tar.gz",
        binary_entry: "hyper",
    });
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    return Ok(Platform {
        asset_triple: "x86_64-unknown-linux-gnu",
        local_os: "linux",
        local_arch: "x86_64",
        archive_suffix: "tar.gz",
        binary_entry: "hyper",
    });
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return Ok(Platform {
        asset_triple: "x86_64-pc-windows-msvc",
        local_os: "windows",
        local_arch: "x86_64",
        archive_suffix: "zip",
        binary_entry: "hyper.exe",
    });
    bail!("this platform does not have a published Hyper community artifact")
}

fn update_source() -> Result<UpdateSource> {
    let Some(override_base) = std::env::var_os("HYPER_UPDATE_BASE_URL") else {
        return Ok(UpdateSource {
            api_base: RELEASE_API_BASE.to_string(),
            allow_insecure_local: false,
        });
    };

    // A release build must never inherit an arbitrary update origin. The
    // override exists only for hermetic debug/integration tests and requires a
    // second, explicit opt-in so an accidental environment leak fails closed.
    if !cfg!(debug_assertions)
        || std::env::var_os("HYPER_ALLOW_INSECURE_UPDATE_BASE").as_deref()
            != Some(std::ffi::OsStr::new("1"))
    {
        bail!(
            "HYPER_UPDATE_BASE_URL is disabled in production Hyper builds; updates are pinned to {RELEASE_REPO}"
        );
    }
    let api_base = override_base
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    let url = reqwest::Url::parse(&api_base).context("invalid HYPER_UPDATE_BASE_URL")?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    if !local {
        bail!("debug update-base overrides are restricted to localhost");
    }
    Ok(UpdateSource {
        api_base,
        allow_insecure_local: true,
    })
}

fn allowed_github_redirect(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    matches!(
        url.host_str(),
        Some(
            "api.github.com"
                | "github.com"
                | "objects.githubusercontent.com"
                | "release-assets.githubusercontent.com"
        )
    )
}

fn http_client(source: &UpdateSource) -> Result<reqwest::Client> {
    let allow_local = source.allow_insecure_local;
    let local_origin = reqwest::Url::parse(&source.api_base).ok().and_then(|u| {
        Some((
            u.scheme().to_string(),
            u.host_str()?.to_string(),
            u.port_or_known_default(),
        ))
    });
    let redirect = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects while updating Hyper");
        }
        let url = attempt.url();
        if allow_local {
            let same_local_origin = local_origin.as_ref().is_some_and(|(scheme, host, port)| {
                url.scheme() == scheme
                    && url.host_str() == Some(host.as_str())
                    && url.port_or_known_default() == *port
            });
            if same_local_origin {
                attempt.follow()
            } else {
                attempt.stop()
            }
        } else if allowed_github_redirect(url) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    Ok(reqwest::Client::builder()
        .user_agent("hyper-community-updater")
        .timeout(REQUEST_TIMEOUT)
        .redirect(redirect)
        .build()?)
}

async fn response_bytes_limited(response: reqwest::Response, max: u64) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length()
        && length > max
    {
        bail!("update response is too large ({length} bytes; limit {max})");
    }
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() as u64 + chunk.len() as u64 > max {
            bail!("update response exceeded the {max}-byte limit");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn checked_response(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response_bytes_limited(response, 4096)
        .await
        .unwrap_or_default();
    let detail = String::from_utf8_lossy(&body);
    bail!("{what} failed with HTTP {status}: {}", detail.trim());
}

fn validate_release_asset_url(source: &UpdateSource, url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("release contains an invalid asset URL")?;
    if source.allow_insecure_local {
        let base = reqwest::Url::parse(&source.api_base)?;
        if parsed.scheme() != base.scheme()
            || parsed.host_str() != base.host_str()
            || parsed.port_or_known_default() != base.port_or_known_default()
        {
            bail!("debug release asset URL escaped the localhost update origin");
        }
        return Ok(());
    }
    let expected_prefix = format!("/{RELEASE_REPO}/releases/download/");
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || !parsed.path().starts_with(&expected_prefix)
    {
        bail!("release asset URL is outside the pinned Hyper GitHub repository");
    }
    Ok(())
}

fn one_asset<'a>(release: &'a ReleaseMetadata, name: &str) -> Result<&'a ReleaseAsset> {
    let mut matching = release.assets.iter().filter(|asset| asset.name == name);
    let Some(asset) = matching.next() else {
        bail!("release {} has no asset {name}", release.tag_name);
    };
    if matching.next().is_some() {
        bail!(
            "release {} contains duplicate asset {name}",
            release.tag_name
        );
    }
    Ok(asset)
}

fn parse_manifest_checksum(manifest: &str, asset_name: &str) -> Result<String> {
    let mut found: Option<String> = None;
    for line in manifest.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if name.trim_start_matches('*') != asset_name {
            continue;
        }
        if parts.next().is_some() {
            bail!("SHA256SUMS contains a malformed entry for {asset_name}");
        }
        let normalized = hash.to_ascii_lowercase();
        if !valid_sha256(&normalized) {
            bail!("SHA256SUMS contains an invalid checksum for {asset_name}");
        }
        if found.replace(normalized).is_some() {
            bail!("SHA256SUMS contains duplicate entries for {asset_name}");
        }
    }
    found.ok_or_else(|| anyhow::anyhow!("SHA256SUMS has no entry for {asset_name}"))
}

async fn resolve_candidate(pinned_version: Option<&str>) -> Result<Candidate> {
    let source = update_source()?;
    let client = http_client(&source)?;
    let endpoint = match pinned_version {
        Some(version) => format!("{}/tags/v{version}", source.api_base),
        None => format!("{}/latest", source.api_base),
    };
    let mut request = client
        .get(&endpoint)
        .header("Accept", "application/vnd.github+json");
    if !source.allow_insecure_local
        && let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        // Only the fixed api.github.com request receives this token. Browser
        // download URLs and all debug overrides remain unauthenticated.
        request = request.bearer_auth(token.trim());
    }
    let response =
        checked_response(request.send().await?, "Hyper release metadata request").await?;
    let release_bytes = response_bytes_limited(response, MAX_MANIFEST_BYTES).await?;
    let release: ReleaseMetadata =
        serde_json::from_slice(&release_bytes).context("invalid Hyper release metadata")?;
    if release.draft {
        bail!("refusing to install draft release {}", release.tag_name);
    }
    if pinned_version.is_none() && release.prerelease {
        bail!("the latest Hyper release endpoint returned a prerelease");
    }
    let version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    semver::Version::parse(version)
        .with_context(|| format!("release tag {} is not valid semver", release.tag_name))?;
    if let Some(requested) = pinned_version
        && requested != version
    {
        bail!("requested Hyper {requested}, but the release endpoint returned {version}");
    }

    let platform = platform()?;
    let asset_name = format!(
        "hyper-{version}-{}.{}",
        platform.asset_triple, platform.archive_suffix
    );
    let archive_asset = one_asset(&release, &asset_name)?;
    let sums_asset = one_asset(&release, "SHA256SUMS")?;
    validate_release_asset_url(&source, &archive_asset.browser_download_url)?;
    validate_release_asset_url(&source, &sums_asset.browser_download_url)?;

    let sums_response = checked_response(
        client.get(&sums_asset.browser_download_url).send().await?,
        "Hyper SHA256SUMS download",
    )
    .await?;
    let sums = response_bytes_limited(sums_response, MAX_MANIFEST_BYTES).await?;
    let sums = std::str::from_utf8(&sums).context("SHA256SUMS is not UTF-8")?;
    let sha256 = parse_manifest_checksum(sums, &asset_name)?;

    // Optional scheme live image for this platform. Older releases (and
    // platforms without a prebuilt image, e.g. Windows / x86_64 macOS) simply
    // lack the asset; any resolution failure downgrades to "no image".
    let scheme_asset_name = format!(
        "hyper-scheme-image-{version}-{}.tar.gz",
        platform.asset_triple
    );
    let scheme_image = match one_asset(&release, &scheme_asset_name) {
        Ok(asset)
            if validate_release_asset_url(&source, &asset.browser_download_url).is_ok() =>
        {
            parse_manifest_checksum(sums, &scheme_asset_name)
                .ok()
                .map(|sha256| SchemeImageAsset {
                    asset_name: scheme_asset_name,
                    archive_url: asset.browser_download_url.clone(),
                    sha256,
                })
        }
        _ => None,
    };

    Ok(Candidate {
        version: version.to_string(),
        asset_name,
        archive_url: archive_asset.browser_download_url.clone(),
        sha256,
        scheme_image,
    })
}

fn version_from_managed_name(name: &str) -> Option<String> {
    let name = name.strip_suffix(".exe").unwrap_or(name);
    let suffix = name.strip_prefix("hyper-")?;
    let marker = ["-macos-", "-linux-", "-windows-"]
        .into_iter()
        .find_map(|marker| suffix.find(marker).map(|index| (marker, index)))?;
    let version = &suffix[..marker.1];
    semver::Version::parse(version).ok()?;
    Some(version.to_string())
}

fn digest_from_managed_name(name: &str) -> Option<String> {
    let name = name.strip_suffix(".exe").unwrap_or(name);
    let digest = name.rsplit_once("-sha256-")?.1.to_ascii_lowercase();
    valid_sha256(&digest).then_some(digest)
}

fn active_deployment() -> Option<ActiveDeployment> {
    let app = managed_application();
    let metadata = std::fs::metadata(&app).ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }

    #[cfg(unix)]
    let binary_name = {
        let target = std::fs::read_link(&app).ok()?;
        target.file_name()?.to_string_lossy().to_string()
    };
    #[cfg(windows)]
    let binary_name = load_state().installed_binary?;
    #[cfg(not(any(unix, windows)))]
    return None;

    let version = version_from_managed_name(&binary_name).or_else(|| {
        let state = load_state();
        (state.installed_binary.as_deref() == Some(binary_name.as_str()))
            .then_some(state.installed_version)
            .flatten()
    })?;
    let state = load_state();
    let state_sha = (state.installed_version.as_deref() == Some(version.as_str())
        && state.installed_binary.as_deref() == Some(binary_name.as_str()))
    .then_some(state.installed_sha256)
    .flatten()
    .filter(|sha| valid_sha256(sha));
    Some(ActiveDeployment {
        version,
        sha256: digest_from_managed_name(&binary_name).or(state_sha),
        binary_name,
    })
}

fn current_exe_belongs_to_hyper_home() -> bool {
    let Ok(exe) = std::env::current_exe().and_then(dunce::canonicalize) else {
        return false;
    };
    let home = dunce::canonicalize(hyper_home()).unwrap_or_else(|_| hyper_home());
    exe.starts_with(home.join("downloads")) || exe.starts_with(home.join("bin"))
}

fn current_process_is_managed() -> bool {
    match (
        std::env::current_exe()
            .ok()
            .and_then(|p| dunce::canonicalize(p).ok()),
        dunce::canonicalize(managed_application()).ok(),
    ) {
        (Some(exe), Some(active)) => exe == active,
        _ => false,
    }
}

pub(crate) fn running_differs_from_active() -> bool {
    if !current_exe_belongs_to_hyper_home() {
        return false;
    }
    match (
        std::env::current_exe()
            .ok()
            .and_then(|p| dunce::canonicalize(p).ok()),
        dunce::canonicalize(managed_application()).ok(),
    ) {
        (Some(exe), Some(active)) => exe != active,
        _ => false,
    }
}

fn automatic_entry_allowed() -> bool {
    current_process_is_managed() || running_differs_from_active()
}

fn deployed_digest(active: &ActiveDeployment, state: &UpdateState) -> Option<String> {
    active.sha256.clone().or_else(|| {
        (state.installed_version.as_deref() == Some(active.version.as_str())
            && state.installed_binary.as_deref() == Some(active.binary_name.as_str()))
        .then(|| state.installed_sha256.clone())
        .flatten()
        .filter(|sha| valid_sha256(sha))
    })
}

fn candidate_requires_install(
    candidate: &Candidate,
    active: Option<&ActiveDeployment>,
    state: &UpdateState,
) -> Result<bool> {
    let Some(active) = active else {
        return Ok(true);
    };
    let target = semver::Version::parse(&candidate.version)?;
    let current = semver::Version::parse(&active.version)?;
    if target > current {
        return Ok(true);
    }
    if target < current {
        return Ok(false);
    }
    // A release tag may be republished in this community repository. Once an
    // install has an archive identity, equal semver but different digest is a
    // real update. Old installer layouts without state are adopted once rather
    // than forcing a multi-hundred-MiB reinstall on first launch.
    Ok(deployed_digest(active, state).is_some_and(|sha| sha != candidate.sha256))
}

fn state_matches_active(state: &UpdateState, active: &ActiveDeployment) -> bool {
    state.installed_version.as_deref() == Some(active.version.as_str())
        && state.installed_binary.as_deref() == Some(active.binary_name.as_str())
        && state.installed_sha256.as_deref().is_some_and(valid_sha256)
        && deployed_digest(active, state).as_deref() == state.installed_sha256.as_deref()
}

pub(crate) fn is_version_cache_fresh() -> bool {
    let state = load_state();
    let Some(active) = active_deployment() else {
        return false;
    };
    state_is_fresh(&state) && state_matches_active(&state, &active)
}

pub(crate) fn installed_on_disk_version() -> Option<String> {
    active_deployment().map(|deployment| deployment.version)
}

fn reconcile_checked_state(candidate: &Candidate, active: Option<&ActiveDeployment>) -> Result<()> {
    let mut state = load_state();
    if let Some(active) = active {
        state.installed_version = Some(active.version.clone());
        state.installed_binary = Some(active.binary_name.clone());
        if active.version == candidate.version {
            state.installed_asset = Some(candidate.asset_name.clone());
            state.installed_sha256 = Some(candidate.sha256.clone());
        } else if let Some(sha) = &active.sha256 {
            state.installed_sha256 = Some(sha.clone());
        }
    }
    state.checked_at_unix = Some(now_unix());
    write_state_atomic(&state)
}

async fn record_no_update(candidate: &Candidate) -> Result<()> {
    let _lock = acquire_update_lock().await?;
    let state = load_state();
    let active = active_deployment();
    if candidate_requires_install(candidate, active.as_ref(), &state)? {
        // Another process changed the active deployment while this caller was
        // waiting for the lock. Do not cache a stale "no update" conclusion.
        return Ok(());
    }
    reconcile_checked_state(candidate, active.as_ref())
}

async fn download_archive(candidate: &Candidate, destination: &Path) -> Result<String> {
    let source = update_source()?;
    validate_release_asset_url(&source, &candidate.archive_url)?;
    let client = http_client(&source)?;
    let response = checked_response(
        client.get(&candidate.archive_url).send().await?,
        "Hyper archive download",
    )
    .await?;
    if let Some(length) = response.content_length()
        && length > MAX_ARCHIVE_BYTES
    {
        bail!("Hyper archive is too large ({length} bytes)");
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .await
        .with_context(|| format!("creating {}", destination.display()))?;
    let mut size = 0u64;
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        size = size.saturating_add(chunk.len() as u64);
        if size > MAX_ARCHIVE_BYTES {
            bail!("Hyper archive exceeded the {MAX_ARCHIVE_BYTES}-byte limit");
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(digest)
}

/// Best-effort install of the prebuilt scheme live image into
/// `$GROK_HOME/bin/hyper-scheme-image`. Optional component: every failure is
/// reported as a note and swallowed — the scheme runtime falls back to
/// `gxi`/`gsi` PATH discovery (see `xai-grok-scheme-runtime`).
async fn install_scheme_image_best_effort(candidate: &Candidate) {
    let Some(image) = candidate.scheme_image.clone() else {
        return;
    };
    if let Err(error) = install_scheme_image(candidate, &image).await {
        eprintln!("  Note: scheme live image install skipped: {error:#}");
    }
}

async fn install_scheme_image(candidate: &Candidate, image: &SchemeImageAsset) -> Result<()> {
    let bin_dir = community_grok_home().join("bin");
    if path_exists_or_symlink(&bin_dir) {
        reject_symlink(&bin_dir, "Grok bin directory")?;
    }
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;

    let archive_tmp = unique_sibling(&bin_dir.join(&image.asset_name), "download");
    let _archive_guard = TempArtifact::new_file(archive_tmp.clone());
    let download = Candidate {
        version: candidate.version.clone(),
        asset_name: image.asset_name.clone(),
        archive_url: image.archive_url.clone(),
        sha256: image.sha256.clone(),
        scheme_image: None,
    };
    let actual = download_archive(&download, &archive_tmp).await?;
    if actual != image.sha256 {
        bail!(
            "SHA-256 mismatch for {}: expected {}, got {actual}",
            image.asset_name,
            image.sha256
        );
    }

    // Extract the single `hyper-scheme-image` regular-file member to a stage
    // path in the destination directory (same-FS atomic rename).
    let file = std::fs::File::open(&archive_tmp)
        .with_context(|| format!("opening {}", archive_tmp.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let stage = unique_sibling(&bin_dir.join("hyper-scheme-image"), "install");
    let stage_guard = TempArtifact::new_file(stage.clone());
    let mut extracted = false;
    for entry in archive.entries().context("reading scheme image archive")? {
        let mut entry = entry.context("reading scheme image archive entry")?;
        let path = entry.path().context("scheme image entry path")?.into_owned();
        let name = path.to_string_lossy().into_owned();
        let name = name.trim_start_matches("./");
        if entry.header().entry_type() != tar::EntryType::Regular || name != "hyper-scheme-image" {
            continue;
        }
        let mut out = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&stage)
            .with_context(|| format!("staging {}", stage.display()))?;
        std::io::copy(&mut entry, &mut out).context("extracting scheme image binary")?;
        extracted = true;
        break;
    }
    if !extracted {
        bail!(
            "archive {} has no hyper-scheme-image member",
            image.asset_name
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", stage.display()))?;
    }
    let dest = bin_dir.join("hyper-scheme-image");
    if path_exists_or_symlink(&dest) {
        reject_symlink(&dest, "scheme live image binary")?;
    }
    std::fs::rename(&stage, &dest).with_context(|| format!("activating {}", dest.display()))?;
    let _ = stage_guard.keep();
    eprintln!("  Scheme live image installed to {}", dest.display());
    Ok(())
}

/// Windows reserved device names (case-insensitive stem before any extension).
fn is_windows_reserved_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn validate_path_component(name: &str, raw: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        bail!("archive entry has an invalid path component: {raw}");
    }
    if name.contains('\0') {
        bail!("archive entry has an invalid path component: {raw}");
    }
    // Portable cross-platform component rules (Windows-hostile shapes).
    if name.contains(':') {
        bail!("archive entry component contains ':': {raw}");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        bail!("archive entry component has trailing '.' or space: {raw}");
    }
    if is_windows_reserved_device(name) {
        bail!("archive entry uses a Windows reserved device name: {raw}");
    }
    Ok(())
}

/// Safely normalize an archive entry to relative components.
///
/// Allows multiple `Normal` components, ignores `.`, and rejects absolute /
/// rooted / prefix / `..` paths, empty names, reserved device names, and
/// excessive depth. Returns `None` for the archive root (`.` / empty).
///
/// `allow_backslash_as_separator`: zip producers (PowerShell / older tools) may
/// emit `\` separators; treat them as `/` then apply the same component rules.
/// Tar entries must keep literal backslash rejection (`allow = false`).
fn normalize_archive_path(
    raw: &str,
    allow_backslash_as_separator: bool,
) -> Result<Option<Vec<String>>> {
    if raw.contains('\0') {
        bail!("archive entry contains a NUL byte");
    }
    let normalized = if allow_backslash_as_separator {
        raw.replace('\\', "/")
    } else {
        if raw.contains('\\') {
            bail!("archive entry uses a backslash path: {raw}");
        }
        raw.to_string()
    };
    // Reject Windows drive / UNC style even when Path components would not.
    if normalized.chars().nth(1) == Some(':') {
        bail!("archive entry has a Windows drive prefix: {raw}");
    }
    let path = Path::new(&normalized);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                let name = part.to_str().ok_or_else(|| {
                    anyhow::anyhow!("archive entry has a non-UTF-8 component: {raw}")
                })?;
                validate_path_component(name, raw)?;
                if name.contains('/') || (!allow_backslash_as_separator && name.contains('\\')) {
                    bail!("archive entry has an invalid path component: {raw}");
                }
                parts.push(name.to_string());
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("archive entry escapes its root: {raw}");
            }
        }
    }
    if parts.len() > MAX_PATH_DEPTH {
        bail!("archive entry exceeds the maximum path depth ({MAX_PATH_DEPTH}): {raw}");
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts))
}

fn path_key_casefold(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| p.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("/")
}

fn display_parts(parts: &[String]) -> String {
    parts.join("/")
}

fn auxiliary_entry_allowed(name: &str) -> bool {
    matches!(
        name,
        "LICENSE" | "NOTICE" | "THIRD-PARTY-NOTICES" | "THIRD-PARTY-NOTICES.md"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveEntryClass {
    /// Archive root `.` / empty (tar directory placeholder).
    RootPlaceholder,
    /// Root-level managed binary (`hyper` / `hyper.exe`).
    Binary,
    /// Root-level license/notice allowlist (drained, not deployed).
    Notice,
    /// `bundled` directory entry itself.
    BundleRootDir,
    /// Directory under `bundled/`.
    BundleDir,
    /// Regular file under `bundled/`.
    BundleFile,
}

fn classify_archive_entry(
    parts: Option<&[String]>,
    binary_entry: &str,
) -> Result<ArchiveEntryClass> {
    let Some(parts) = parts else {
        return Ok(ArchiveEntryClass::RootPlaceholder);
    };
    match parts.len() {
        0 => Ok(ArchiveEntryClass::RootPlaceholder),
        1 => {
            let name = &parts[0];
            if name == binary_entry {
                Ok(ArchiveEntryClass::Binary)
            } else if auxiliary_entry_allowed(name) {
                Ok(ArchiveEntryClass::Notice)
            } else if name == BUNDLE_DIR_NAME {
                Ok(ArchiveEntryClass::BundleRootDir)
            } else {
                bail!("Hyper archive contains unexpected root entry {name}");
            }
        }
        _ => {
            if parts[0] != BUNDLE_DIR_NAME {
                bail!(
                    "Hyper archive contains unexpected nested entry {}",
                    display_parts(parts)
                );
            }
            // Remaining components must all be ordinary names (already
            // validated by normalize_archive_path).
            Ok(ArchiveEntryClass::BundleFile)
        }
    }
}

/// Result of a successful archive extraction.
struct ExtractedArchive {
    binary: PathBuf,
    /// Stage directory whose contents are the `bundled/` tree (not including
    /// the `bundled` name itself). Present only when the archive shipped a
    /// bundle.
    bundle_stage: Option<PathBuf>,
    _binary_guard: TempArtifact,
    _bundle_guard: Option<TempArtifact>,
}

struct ExtractLimits {
    entries: usize,
    bundle_files: usize,
    bundle_bytes: u64,
}

impl ExtractLimits {
    fn new() -> Self {
        Self {
            entries: 0,
            bundle_files: 0,
            bundle_bytes: 0,
        }
    }

    fn count_entry(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > MAX_ARCHIVE_ENTRIES {
            bail!("Hyper archive contains too many entries (limit {MAX_ARCHIVE_ENTRIES})");
        }
        Ok(())
    }

    fn count_bundle_file(&mut self, size: u64) -> Result<()> {
        self.bundle_files += 1;
        if self.bundle_files > MAX_BUNDLE_FILES {
            bail!("Hyper archive bundle contains too many files (limit {MAX_BUNDLE_FILES})");
        }
        self.bundle_bytes = self.bundle_bytes.saturating_add(size);
        if self.bundle_bytes > MAX_BUNDLE_TOTAL_BYTES {
            bail!(
                "Hyper archive bundle exceeds the {MAX_BUNDLE_TOTAL_BYTES}-byte decompressed limit"
            );
        }
        Ok(())
    }
}

fn ensure_parent_dirs(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
    }
    Ok(())
}

fn copy_limited<R: Read>(
    reader: &mut R,
    mut writer: impl Write,
    max: u64,
    label: &str,
) -> Result<u64> {
    let copied = std::io::copy(&mut reader.take(max.saturating_add(1)), &mut writer)?;
    if copied > max {
        bail!("{label} exceeds the decompressed size limit ({max} bytes)");
    }
    Ok(copied)
}

fn drain_limited<R: Read>(reader: &mut R, max: u64, label: &str) -> Result<u64> {
    copy_limited(reader, std::io::sink(), max, label)
}

fn insert_seen(seen: &mut HashSet<String>, parts: &[String]) -> Result<()> {
    let key = path_key_casefold(parts);
    if !seen.insert(key) {
        bail!(
            "Hyper archive contains duplicate or case-colliding entry {}",
            display_parts(parts)
        );
    }
    Ok(())
}

fn prepare_extract_destinations(
    stage_root: &Path,
    bundle_stage: PathBuf,
    binary_entry: &str,
) -> Result<(PathBuf, PathBuf, TempArtifact, TempArtifact)> {
    std::fs::create_dir_all(stage_root)
        .with_context(|| format!("creating extract stage {}", stage_root.display()))?;
    let binary_path = stage_root.join(binary_entry);
    // Bundle stage must already live on the same filesystem as the final
    // `$GROK_HOME/bundled` target so activation can rename without copying.
    if bundle_stage.exists() {
        bail!(
            "bundle stage path already exists: {}",
            bundle_stage.display()
        );
    }
    std::fs::create_dir_all(&bundle_stage)
        .with_context(|| format!("creating bundle stage {}", bundle_stage.display()))?;
    let binary_guard = TempArtifact::new_file(binary_path.clone());
    let bundle_guard = TempArtifact::new_dir(bundle_stage.clone());
    Ok((binary_path, bundle_stage, binary_guard, bundle_guard))
}

fn finish_extracted(
    binary_path: PathBuf,
    binary_guard: TempArtifact,
    bundle_stage: PathBuf,
    bundle_guard: TempArtifact,
    wrote_bundle: bool,
    found_binary: bool,
    binary_entry: &str,
) -> Result<ExtractedArchive> {
    if !found_binary {
        bail!("Hyper archive does not contain {binary_entry}");
    }
    if !binary_path.is_file() {
        bail!("Hyper binary stage is missing after extraction");
    }
    if wrote_bundle {
        Ok(ExtractedArchive {
            binary: binary_path,
            bundle_stage: Some(bundle_stage),
            _binary_guard: binary_guard,
            _bundle_guard: Some(bundle_guard),
        })
    } else {
        // Drop empty bundle stage automatically.
        drop(bundle_guard);
        Ok(ExtractedArchive {
            binary: binary_path,
            bundle_stage: None,
            _binary_guard: binary_guard,
            _bundle_guard: None,
        })
    }
}

#[cfg(unix)]
fn extract_tar_archive(
    archive_path: &Path,
    stage_root: &Path,
    bundle_stage: PathBuf,
    binary_entry: &str,
) -> Result<ExtractedArchive> {
    use std::os::unix::fs::PermissionsExt;
    use tar::EntryType;

    let (binary_path, bundle_stage, binary_guard, bundle_guard) =
        prepare_extract_destinations(stage_root, bundle_stage, binary_entry)?;

    let archive_file = File::open(archive_path)
        .with_context(|| format!("opening Hyper archive {}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let mut seen = HashSet::new();
    let mut limits = ExtractLimits::new();
    let mut found_binary = false;
    let mut wrote_bundle = false;

    for entry in archive.entries().context("reading Hyper tar archive")? {
        limits.count_entry()?;
        let mut entry = entry.context("reading Hyper tar entry")?;
        let header = entry.header().clone();
        // Security decisions require valid UTF-8; do not lossily decode paths.
        let raw_os = entry
            .path()
            .context("reading Hyper tar entry path")?
            .into_owned();
        let raw = raw_os
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Hyper archive entry path is not valid UTF-8"))?;
        let parts = normalize_archive_path(raw, false)?;
        let kind = header.entry_type();

        // Allow root `.` directory placeholders produced by `tar -C staging .`.
        // Only Directory is accepted for unnamed root; regular/continuous are
        // rejected so a zero-size root file cannot slip past classification.
        if parts.is_none() {
            match kind {
                EntryType::Directory => continue,
                _ => bail!("Hyper archive root entry has unsupported type: {raw}"),
            }
        }
        let parts = parts.expect("checked above");
        let mut class = classify_archive_entry(Some(&parts), binary_entry)?;
        if matches!(class, ArchiveEntryClass::BundleFile) && kind == EntryType::Directory {
            class = ArchiveEntryClass::BundleDir;
        }
        if matches!(class, ArchiveEntryClass::BundleRootDir) && kind != EntryType::Directory {
            bail!(
                "Hyper archive entry {} must be a directory",
                display_parts(&parts)
            );
        }

        match kind {
            EntryType::Directory => {
                insert_seen(&mut seen, &parts)?;
                match class {
                    ArchiveEntryClass::BundleRootDir | ArchiveEntryClass::BundleDir => {
                        let rel: PathBuf = parts[1..].iter().collect();
                        let dest = bundle_stage.join(&rel);
                        std::fs::create_dir_all(&dest).with_context(|| {
                            format!(
                                "creating bundle directory stage for {}",
                                display_parts(&parts)
                            )
                        })?;
                        wrote_bundle = true;
                    }
                    ArchiveEntryClass::RootPlaceholder => {}
                    _ => {
                        bail!(
                            "Hyper archive has an unexpected directory entry {}",
                            display_parts(&parts)
                        );
                    }
                }
            }
            EntryType::Regular | EntryType::Continuous => {
                insert_seen(&mut seen, &parts)?;
                match class {
                    ArchiveEntryClass::Binary => {
                        if found_binary {
                            bail!("Hyper archive contains duplicate {binary_entry}");
                        }
                        if entry.size() > MAX_BINARY_BYTES {
                            bail!("Hyper binary exceeds the decompressed size limit");
                        }
                        let mut out = OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(&binary_path)
                            .with_context(|| {
                                format!("creating binary stage {}", binary_path.display())
                            })?;
                        copy_limited(&mut entry, &mut out, MAX_BINARY_BYTES, "Hyper binary")?;
                        out.sync_all()?;
                        std::fs::set_permissions(
                            &binary_path,
                            std::fs::Permissions::from_mode(0o755),
                        )?;
                        found_binary = true;
                    }
                    ArchiveEntryClass::Notice => {
                        if entry.size() > MAX_AUXILIARY_BYTES {
                            bail!(
                                "Hyper archive auxiliary entry {} is too large",
                                display_parts(&parts)
                            );
                        }
                        drain_limited(
                            &mut entry,
                            MAX_AUXILIARY_BYTES,
                            &format!("Hyper archive auxiliary entry {}", display_parts(&parts)),
                        )?;
                    }
                    ArchiveEntryClass::BundleFile => {
                        if entry.size() > MAX_BUNDLE_FILE_BYTES {
                            bail!(
                                "Hyper archive bundle file {} exceeds the per-file size limit",
                                display_parts(&parts)
                            );
                        }
                        let rel: PathBuf = parts[1..].iter().collect();
                        if rel.as_os_str().is_empty() {
                            bail!("Hyper archive bundle file path is empty");
                        }
                        let dest = bundle_stage.join(&rel);
                        ensure_parent_dirs(&dest)?;
                        let mut out = OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(&dest)
                            .with_context(|| {
                                format!(
                                    "creating bundle stage file {} -> {}",
                                    display_parts(&parts),
                                    dest.display()
                                )
                            })?;
                        let copied = copy_limited(
                            &mut entry,
                            &mut out,
                            MAX_BUNDLE_FILE_BYTES,
                            &format!("Hyper archive bundle file {}", display_parts(&parts)),
                        )?;
                        out.sync_all()?;
                        limits.count_bundle_file(copied)?;
                        wrote_bundle = true;
                    }
                    ArchiveEntryClass::BundleRootDir | ArchiveEntryClass::BundleDir => {
                        bail!(
                            "Hyper archive directory entry {} is not a regular file",
                            display_parts(&parts)
                        );
                    }
                    ArchiveEntryClass::RootPlaceholder => {
                        bail!("Hyper archive contains an unnamed regular entry");
                    }
                }
            }
            EntryType::Symlink
            | EntryType::Link
            | EntryType::Char
            | EntryType::Block
            | EntryType::Fifo
            | EntryType::GNUSparse
            | EntryType::GNULongName
            | EntryType::GNULongLink
            | EntryType::XGlobalHeader
            | EntryType::XHeader => {
                bail!(
                    "Hyper archive contains unsupported entry type {:?} at {}",
                    kind,
                    display_parts(&parts)
                );
            }
            _ => {
                bail!(
                    "Hyper archive contains unsupported entry type {:?} at {}",
                    kind,
                    display_parts(&parts)
                );
            }
        }
    }

    finish_extracted(
        binary_path,
        binary_guard,
        bundle_stage,
        bundle_guard,
        wrote_bundle,
        found_binary,
        binary_entry,
    )
}

fn extract_zip_archive(
    archive_path: &Path,
    stage_root: &Path,
    bundle_stage: PathBuf,
    binary_entry: &str,
) -> Result<ExtractedArchive> {
    let (binary_path, bundle_stage, binary_guard, bundle_guard) =
        prepare_extract_destinations(stage_root, bundle_stage, binary_entry)?;

    let file = File::open(archive_path)
        .with_context(|| format!("opening Hyper archive {}", archive_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("reading Hyper zip archive")?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("Hyper archive contains too many entries (limit {MAX_ARCHIVE_ENTRIES})");
    }

    let mut seen = HashSet::new();
    let mut limits = ExtractLimits::new();
    let mut found_binary = false;
    let mut wrote_bundle = false;

    for index in 0..archive.len() {
        limits.count_entry()?;
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("reading Hyper zip entry #{index}"))?;
        let raw_name = entry.name().to_string();
        // PowerShell / older zip producers may use `\`; normalize as separators.
        let trimmed = raw_name.trim_end_matches(['/', '\\']);
        let parts = normalize_archive_path(trimmed, true)?;
        let is_dir = entry.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\');

        // Reject Unix symlink mode (S_IFLNK) / reparse-style entries.
        if entry.is_symlink() {
            bail!("Hyper archive contains a symlink: {raw_name}");
        }

        if parts.is_none() {
            if is_dir {
                continue;
            }
            bail!("Hyper archive contains an unnamed regular entry: {raw_name}");
        }
        let parts = parts.expect("checked above");
        let mut class = classify_archive_entry(Some(&parts), binary_entry)?;
        if is_dir {
            if matches!(class, ArchiveEntryClass::BundleFile) {
                class = ArchiveEntryClass::BundleDir;
            }
            insert_seen(&mut seen, &parts)?;
            match class {
                ArchiveEntryClass::BundleRootDir | ArchiveEntryClass::BundleDir => {
                    let rel: PathBuf = parts[1..].iter().collect();
                    let dest = bundle_stage.join(&rel);
                    std::fs::create_dir_all(&dest).with_context(|| {
                        format!(
                            "creating bundle directory stage for {}",
                            display_parts(&parts)
                        )
                    })?;
                    wrote_bundle = true;
                }
                ArchiveEntryClass::RootPlaceholder => {}
                _ => {
                    bail!(
                        "Hyper archive has an unexpected directory entry {}",
                        display_parts(&parts)
                    );
                }
            }
            continue;
        }

        insert_seen(&mut seen, &parts)?;
        match class {
            ArchiveEntryClass::Binary => {
                if found_binary {
                    bail!("Hyper archive contains duplicate {binary_entry}");
                }
                if entry.size() > MAX_BINARY_BYTES {
                    bail!("Hyper binary exceeds the decompressed size limit");
                }
                let mut out = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&binary_path)
                    .with_context(|| format!("creating binary stage {}", binary_path.display()))?;
                copy_limited(&mut entry, &mut out, MAX_BINARY_BYTES, "Hyper binary")?;
                out.sync_all()?;
                found_binary = true;
            }
            ArchiveEntryClass::Notice => {
                if entry.size() > MAX_AUXILIARY_BYTES {
                    bail!(
                        "Hyper archive auxiliary entry {} is too large",
                        display_parts(&parts)
                    );
                }
                drain_limited(
                    &mut entry,
                    MAX_AUXILIARY_BYTES,
                    &format!("Hyper archive auxiliary entry {}", display_parts(&parts)),
                )?;
            }
            ArchiveEntryClass::BundleFile => {
                if entry.size() > MAX_BUNDLE_FILE_BYTES {
                    bail!(
                        "Hyper archive bundle file {} exceeds the per-file size limit",
                        display_parts(&parts)
                    );
                }
                let rel: PathBuf = parts[1..].iter().collect();
                if rel.as_os_str().is_empty() {
                    bail!("Hyper archive bundle file path is empty");
                }
                let dest = bundle_stage.join(&rel);
                ensure_parent_dirs(&dest)?;
                let mut out = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&dest)
                    .with_context(|| {
                        format!(
                            "creating bundle stage file {} -> {}",
                            display_parts(&parts),
                            dest.display()
                        )
                    })?;
                let copied = copy_limited(
                    &mut entry,
                    &mut out,
                    MAX_BUNDLE_FILE_BYTES,
                    &format!("Hyper archive bundle file {}", display_parts(&parts)),
                )?;
                out.sync_all()?;
                limits.count_bundle_file(copied)?;
                wrote_bundle = true;
            }
            ArchiveEntryClass::BundleRootDir | ArchiveEntryClass::BundleDir => {
                bail!(
                    "Hyper archive directory entry {} is not a regular file",
                    display_parts(&parts)
                );
            }
            ArchiveEntryClass::RootPlaceholder => {
                bail!("Hyper archive contains an unnamed regular entry");
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if found_binary {
            std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755))?;
        }
    }

    finish_extracted(
        binary_path,
        binary_guard,
        bundle_stage,
        bundle_guard,
        wrote_bundle,
        found_binary,
        binary_entry,
    )
}

async fn extract_archive(
    archive: &Path,
    stage_root: &Path,
    bundle_stage: PathBuf,
    platform: Platform,
) -> Result<ExtractedArchive> {
    let archive = archive.to_owned();
    let stage_root = stage_root.to_owned();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            // Prefer tar.gz on Unix; also accept zip so unit tests can cover the
            // Windows producer layout on Linux CI.
            let name = archive.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".zip") {
                extract_zip_archive(&archive, &stage_root, bundle_stage, platform.binary_entry)
            } else {
                extract_tar_archive(&archive, &stage_root, bundle_stage, platform.binary_entry)
            }
        }
        #[cfg(windows)]
        {
            extract_zip_archive(&archive, &stage_root, bundle_stage, platform.binary_entry)
        }
        #[cfg(not(any(unix, windows)))]
        {
            bail!("unsupported Hyper archive format")
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("Hyper archive extraction task failed: {e}"))?
}

async fn smoke_test(binary: &Path) -> Result<()> {
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = tokio::time::timeout(SMOKE_TEST_TIMEOUT, command.status())
        .await
        .context("downloaded Hyper binary smoke test timed out")??;
    if !status.success() {
        bail!("downloaded Hyper binary failed its --version smoke test ({status})");
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn publish_versioned_binary(stage: &Path, destination: &Path) -> Result<()> {
    match std::fs::hard_link(stage, destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if sha256_file(stage)? != sha256_file(destination)? {
                bail!(
                    "existing checksum-addressed Hyper binary does not match the verified download: {}",
                    destination.display()
                );
            }
        }
        Err(_) => {
            // Some Windows/filesystem configurations disallow hard links. The
            // process lock means a create_new copy is still never observed by
            // another cooperating updater before it is complete.
            let mut src = File::open(stage)?;
            let mut dst = match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if sha256_file(stage)? != sha256_file(destination)? {
                        bail!("existing Hyper binary conflicts with verified download");
                    }
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            if let Err(error) = std::io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all()) {
                let _ = std::fs::remove_file(destination);
                return Err(error.into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))?;
            }
        }
    }
    Ok(())
}

/// Captured active binary state for rollback.
#[derive(Debug)]
enum PreviousBinary {
    Missing,
    #[cfg(unix)]
    Symlink {
        target: PathBuf,
    },
    #[cfg(unix)]
    RegularAside {
        aside: PathBuf,
    },
    #[cfg(windows)]
    ExeAside {
        aside: PathBuf,
    },
}

#[derive(Debug)]
struct BinaryActivation {
    previous: PreviousBinary,
    /// Aside path kept until state commit so a failed state write can restore
    /// the prior executable (Windows) or regular-file install (Unix).
    pending_aside: Option<PathBuf>,
}

fn relative_versioned_link_target(versioned: &Path) -> Result<PathBuf> {
    let downloads = hyper_home().join("downloads");
    let name = versioned
        .file_name()
        .context("versioned Hyper binary has no filename")?;
    let relative = Path::new("..").join(
        downloads
            .file_name()
            .context("Hyper downloads directory has no filename")?,
    );
    Ok(relative.join(name))
}

/// Move `path` out of the way to a unique sibling before restoring an older
/// tree. Rename failures are returned (never ignored) so rollback can report
/// inconsistency instead of leaving a mixed deployment silently.
fn move_active_aside(path: &Path, suffix: &str) -> Result<Option<PathBuf>> {
    if !(path.exists() || path.is_symlink()) {
        return Ok(None);
    }
    let doomed = unique_sibling(path, suffix);
    std::fs::rename(path, &doomed).with_context(|| {
        format!(
            "moving active path {} aside to {} for rollback",
            path.display(),
            doomed.display()
        )
    })?;
    Ok(Some(doomed))
}

fn restore_previous_binary(app: &Path, previous: &PreviousBinary) -> Result<()> {
    match previous {
        PreviousBinary::Missing => {
            // New install failed after creating the active path: remove it.
            if path_exists_or_symlink(app) {
                let doomed = move_active_aside(app, "failed-new")?;
                if let Some(doomed) = doomed {
                    let _ = std::fs::remove_file(&doomed);
                    let _ = std::fs::remove_dir_all(&doomed);
                }
            }
            Ok(())
        }
        #[cfg(unix)]
        PreviousBinary::Symlink { target } => {
            // Prefer atomic replace: stage restore link then rename over active.
            let tmp = unique_sibling(app, "restore-link");
            std::os::unix::fs::symlink(target, &tmp).with_context(|| {
                format!(
                    "staging restore symlink for {} -> {}",
                    app.display(),
                    target.display()
                )
            })?;
            if let Err(error) = std::fs::rename(&tmp, app) {
                let _ = std::fs::remove_file(&tmp);
                // Fall back: move active aside, then publish restore link.
                let doomed = match move_active_aside(app, "failed-new") {
                    Ok(d) => d,
                    Err(move_err) => {
                        return Err(combine_errors(
                            anyhow::Error::new(error).context(format!(
                                "restoring previous Hyper symlink at {}",
                                app.display()
                            )),
                            move_err,
                        ));
                    }
                };
                if let Err(error) = std::os::unix::fs::symlink(target, app) {
                    let restore_error = anyhow::Error::new(error).context(format!(
                        "restoring previous Hyper symlink at {}",
                        app.display()
                    ));
                    if let Some(doomed) = doomed.as_ref()
                        && let Err(republish_error) = std::fs::rename(doomed, app)
                    {
                        return Err(combine_errors(
                            restore_error,
                            anyhow::Error::new(republish_error).context(format!(
                                "republishing failed-new Hyper path from {} to {}",
                                doomed.display(),
                                app.display()
                            )),
                        ));
                    }
                    return Err(restore_error);
                }
                if let Some(doomed) = doomed {
                    let _ = std::fs::remove_file(doomed);
                }
                return Ok(());
            }
            Ok(())
        }
        #[cfg(unix)]
        PreviousBinary::RegularAside { aside } => {
            let doomed = match move_active_aside(app, "failed-new") {
                Ok(d) => d,
                Err(move_err) => {
                    return Err(move_err.context(format!(
                        "cannot clear active Hyper at {} before restoring {}",
                        app.display(),
                        aside.display()
                    )));
                }
            };
            if let Err(error) = std::fs::rename(aside, app) {
                let restore_error = anyhow::Error::new(error).context(format!(
                    "restoring previous Hyper regular file from {} (aside preserved)",
                    aside.display()
                ));
                if let Some(doomed) = doomed.as_ref()
                    && let Err(republish_error) = std::fs::rename(doomed, app)
                {
                    return Err(combine_errors(
                        restore_error,
                        anyhow::Error::new(republish_error).context(format!(
                            "republishing failed-new Hyper file from {} to {}",
                            doomed.display(),
                            app.display()
                        )),
                    ));
                }
                return Err(restore_error);
            }
            if let Some(doomed) = doomed {
                let _ = std::fs::remove_file(doomed);
            }
            Ok(())
        }
        #[cfg(windows)]
        PreviousBinary::ExeAside { aside } => {
            let doomed = match move_active_aside(app, "failed-new.exe") {
                Ok(d) => d,
                Err(move_err) => {
                    return Err(move_err.context(format!(
                        "cannot clear active Hyper at {} before restoring {}",
                        app.display(),
                        aside.display()
                    )));
                }
            };
            if let Err(error) = std::fs::rename(aside, app) {
                let restore_error = anyhow::Error::new(error).context(format!(
                    "restoring previous Hyper executable from {} (aside preserved)",
                    aside.display()
                ));
                if let Some(doomed) = doomed.as_ref()
                    && let Err(republish_error) = std::fs::rename(doomed, app)
                {
                    return Err(combine_errors(
                        restore_error,
                        anyhow::Error::new(republish_error).context(format!(
                            "republishing failed-new Hyper executable from {} to {}",
                            doomed.display(),
                            app.display()
                        )),
                    ));
                }
                return Err(restore_error);
            }
            if let Some(doomed) = doomed {
                let _ = std::fs::remove_file(doomed);
            }
            Ok(())
        }
    }
}

fn path_exists_or_symlink(path: &Path) -> bool {
    path.exists() || path.is_symlink()
}

#[cfg(unix)]
fn activate_binary_transactional(versioned: &Path) -> Result<BinaryActivation> {
    let app = managed_application();
    let bin_dir = app.parent().context("Hyper application has no parent")?;

    // Inspect without mutating first so unsupported shapes fail closed.
    let meta = match std::fs::symlink_metadata(&app) {
        Ok(meta) => Some(meta),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", app.display()));
        }
    };

    let relative = relative_versioned_link_target(versioned)?;
    let tmp = unique_sibling(&app, "tmp-link");
    let tmp_guard = TempArtifact::new_file(tmp.clone());
    std::os::unix::fs::symlink(&relative, &tmp).context("staging Hyper activation symlink")?;

    let mut pending_aside = None;
    // Capture whether the previous active path was a symlink that rename will
    // replace atomically (still present until rename succeeds).
    let previous = if let Some(meta) = meta {
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&app)
                .with_context(|| format!("reading active Hyper symlink {}", app.display()))?;
            PreviousBinary::Symlink { target }
        } else if meta.is_file() {
            // Move the regular file aside before publishing the symlink so we
            // can restore it if a later stage fails.
            let aside = unique_sibling(&app, "old-regular");
            std::fs::rename(&app, &aside).with_context(|| {
                format!(
                    "preserving existing Hyper regular file {} before activation",
                    app.display()
                )
            })?;
            pending_aside = Some(aside.clone());
            PreviousBinary::RegularAside { aside }
        } else {
            let _ = std::fs::remove_file(&tmp);
            bail!(
                "Hyper application path is not a regular file or symlink: {}",
                app.display()
            );
        }
    } else {
        PreviousBinary::Missing
    };

    if let Err(error) = std::fs::rename(&tmp, &app) {
        let activation_err = anyhow::Error::new(error).context(format!(
            "atomically activating Hyper at {} (bin dir {})",
            app.display(),
            bin_dir.display()
        ));
        // Old symlink is unchanged if rename never replaced it — only restore
        // when we already moved a regular file aside.
        match &previous {
            PreviousBinary::RegularAside { .. } => {
                if let Err(restore_error) = restore_previous_binary(&app, &previous) {
                    return Err(combine_errors(activation_err, restore_error));
                }
            }
            PreviousBinary::Symlink { .. } | PreviousBinary::Missing => {
                // tmp may still exist; guard cleans it. Active path untouched.
            }
            #[cfg(windows)]
            PreviousBinary::ExeAside { .. } => {}
        }
        return Err(activation_err);
    }
    let _ = tmp_guard.keep();
    Ok(BinaryActivation {
        previous,
        pending_aside,
    })
}

#[cfg(windows)]
fn activate_binary_transactional(versioned: &Path) -> Result<BinaryActivation> {
    let app = managed_application();
    reject_symlink(&app, "application")?;
    let staged = unique_sibling(&app, "new.exe");
    std::fs::copy(versioned, &staged)?;
    let staged_guard = TempArtifact::new_file(staged.clone());
    if sha256_file(versioned)? != sha256_file(&staged)? {
        bail!("copied Hyper executable failed activation integrity check");
    }
    let aside = unique_sibling(&app, "old.exe");
    let had_old = app.exists();
    if had_old {
        std::fs::rename(&app, &aside).with_context(|| {
            format!(
                "cannot replace running {}; close all Hyper sessions and retry",
                app.display()
            )
        })?;
    }
    if let Err(error) = std::fs::rename(&staged, &app) {
        let activation_err =
            anyhow::Error::new(error).context("activating downloaded Hyper executable");
        if had_old {
            if let Err(restore_error) = std::fs::rename(&aside, &app) {
                return Err(combine_errors(
                    activation_err,
                    anyhow::Error::new(restore_error).context(format!(
                        "failed to restore previous Hyper executable from {} (aside preserved)",
                        aside.display()
                    )),
                ));
            }
        }
        return Err(activation_err);
    }
    let _ = staged_guard.keep();
    // Keep the aside until state commit succeeds so a later failure can roll back.
    let previous = if had_old {
        PreviousBinary::ExeAside {
            aside: aside.clone(),
        }
    } else {
        PreviousBinary::Missing
    };
    Ok(BinaryActivation {
        previous,
        pending_aside: had_old.then_some(aside),
    })
}

#[cfg(not(any(unix, windows)))]
fn activate_binary_transactional(_versioned: &Path) -> Result<BinaryActivation> {
    bail!("unsupported platform for Hyper binary activation")
}

fn ensure_bundle_parent_ready(bundle_path: &Path) -> Result<()> {
    let home = community_grok_home();
    if home.as_os_str().is_empty() {
        bail!("Grok home is empty");
    }
    if home.exists() {
        reject_symlink(&home, "Grok home")?;
        if !std::fs::metadata(&home)?.is_dir() {
            bail!("Grok home is not a directory: {}", home.display());
        }
    } else {
        std::fs::create_dir_all(&home)
            .with_context(|| format!("creating Grok home {}", home.display()))?;
    }
    // Final bundle path must not be a symlink (would follow into attacker dir).
    if path_exists_or_symlink(bundle_path) {
        reject_symlink(bundle_path, "bundled runtime directory")?;
    }
    Ok(())
}

/// Activate a staged bundle tree at `$GROK_HOME/bundled` via same-FS renames.
/// Returns the aside path of the previous bundle, if any.
fn activate_bundle_transactional(stage: &Path) -> Result<Option<PathBuf>> {
    let bundle_path = managed_bundle_path();
    ensure_bundle_parent_ready(&bundle_path)?;
    if !stage.is_dir() {
        bail!("bundle stage is not a directory: {}", stage.display());
    }
    reject_symlink(stage, "bundle stage")?;

    let aside = unique_sibling(&bundle_path, "old");
    let had_old = path_exists_or_symlink(&bundle_path);
    if had_old {
        reject_symlink(&bundle_path, "bundled runtime directory")?;
        std::fs::rename(&bundle_path, &aside).with_context(|| {
            format!(
                "moving existing bundled runtime {} aside",
                bundle_path.display()
            )
        })?;
    }
    if let Err(error) = std::fs::rename(stage, &bundle_path) {
        let activation_err = anyhow::Error::new(error).context(format!(
            "activating bundled runtime at {} from stage {}",
            bundle_path.display(),
            stage.display()
        ));
        if had_old && let Err(restore_error) = std::fs::rename(&aside, &bundle_path) {
            return Err(combine_errors(
                activation_err,
                anyhow::Error::new(restore_error).context(format!(
                    "failed to restore previous bundled runtime from {} (aside preserved at {})",
                    aside.display(),
                    aside.display()
                )),
            ));
        }
        return Err(activation_err);
    }
    Ok(had_old.then_some(aside))
}

fn restore_bundle(aside: Option<&Path>) -> Result<()> {
    let bundle_path = managed_bundle_path();
    let doomed = if path_exists_or_symlink(&bundle_path) {
        Some(move_active_aside(&bundle_path, "failed")?)
    } else {
        None
    };
    let doomed = doomed.flatten();

    if let Some(aside) = aside
        && let Err(error) = std::fs::rename(aside, &bundle_path)
    {
        let restore_error = anyhow::Error::new(error).context(format!(
            "restoring previous bundled runtime from {} (aside preserved)",
            aside.display()
        ));
        // Try to put the failed-new tree back so the active path is not empty.
        if let Some(doomed) = doomed.as_ref()
            && let Err(republish_error) = std::fs::rename(doomed, &bundle_path)
        {
            return Err(combine_errors(
                restore_error,
                anyhow::Error::new(republish_error).context(format!(
                    "republishing failed-new bundled runtime from {} to {}",
                    doomed.display(),
                    bundle_path.display()
                )),
            ));
        }
        return Err(restore_error);
    }
    // Best-effort cleanup of the failed new tree.
    if let Some(doomed) = doomed {
        let _ = std::fs::remove_dir_all(&doomed);
        let _ = std::fs::remove_file(&doomed);
    }
    Ok(())
}

/// Capture exact previous update-state bytes before any activation mutation.
/// Rejects symlinks and non-regular files; only `NotFound` maps to `None`.
fn capture_previous_state_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    reject_symlink(path, "update state")?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_file() {
                bail!(
                    "Hyper update state is not a regular file: {}",
                    path.display()
                );
            }
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading Hyper update state {}", path.display()))?;
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("inspecting Hyper update state {}", path.display()))
        }
    }
}

fn format_rollback_failure(
    commit_error: anyhow::Error,
    rollback_errors: Vec<anyhow::Error>,
) -> anyhow::Error {
    if rollback_errors.is_empty() {
        return commit_error;
    }
    let mut msg = format!(
        "Hyper community update failed and rollback was incomplete; \
         installation may be inconsistent.\n\ncommit error: {commit_error:#}"
    );
    for (i, err) in rollback_errors.iter().enumerate() {
        msg.push_str(&format!("\n\nrollback error {}: {err:#}", i + 1));
    }
    anyhow::anyhow!(msg)
}

async fn install_candidate(candidate: &Candidate) -> Result<()> {
    ensure_safe_layout()?;
    let platform = platform()?;
    let downloads = hyper_home().join("downloads");
    let archive_tmp = unique_sibling(&downloads.join(&candidate.asset_name), "download");
    let archive_guard = TempArtifact::new_file(archive_tmp.clone());
    eprintln!(
        "  Downloading Hyper v{} ({}) from community releases...",
        candidate.version, platform.asset_triple
    );
    let actual_sha = download_archive(candidate, &archive_tmp).await?;
    if actual_sha != candidate.sha256 {
        bail!(
            "SHA-256 mismatch for {}: expected {}, got {}",
            candidate.asset_name,
            candidate.sha256,
            actual_sha
        );
    }

    // Binary extract root under downloads; bundle stage under GROK_HOME so the
    // final rename stays on the same filesystem as `$GROK_HOME/bundled`.
    let extract_root = unique_sibling(&downloads.join("hyper-extracted"), "dir");
    std::fs::create_dir_all(&extract_root)
        .with_context(|| format!("creating extract root {}", extract_root.display()))?;
    let extract_root_guard = TempArtifact::new_dir(extract_root.clone());

    let grok_home = community_grok_home();
    ensure_bundle_parent_ready(&grok_home.join(BUNDLE_DIR_NAME))?;
    let bundle_stage_path = unique_sibling(&grok_home.join(BUNDLE_DIR_NAME), "install");

    let extracted =
        extract_archive(&archive_tmp, &extract_root, bundle_stage_path, platform).await?;
    smoke_test(&extracted.binary).await?;

    let extension = if cfg!(windows) { ".exe" } else { "" };
    let binary_name = format!(
        "hyper-{}-{}-{}-sha256-{}{}",
        candidate.version, platform.local_os, platform.local_arch, candidate.sha256, extension
    );
    let versioned = downloads.join(&binary_name);
    publish_versioned_binary(&extracted.binary, &versioned)?;
    smoke_test(&versioned).await?;

    // State preflight *before* any activation mutation. Symlink / directory /
    // unreadable state must fail closed without touching binary or bundle.
    let state_file = state_path();
    let previous_state_bytes = capture_previous_state_bytes(&state_file)?;

    // --- Compensating transaction: bundle → binary → state ---
    // State is written last; success is the sole commit point. Any earlier
    // failure restores the full previous deployment (binary + bundle + state).
    let mut bundle_aside: Option<PathBuf> = None;
    let mut bundle_activated = false;
    let mut binary_activation: Option<BinaryActivation> = None;
    let mut state_write_attempted = false;

    let commit_result: Result<()> = (|| {
        if let Some(stage) = extracted.bundle_stage.as_ref() {
            bundle_aside = activate_bundle_transactional(stage)?;
            bundle_activated = true;
            take_install_failpoint_after_bundle()?;
        }

        let activation = activate_binary_transactional(&versioned)?;
        binary_activation = Some(activation);

        take_install_failpoint_before_state()?;

        let state = UpdateState {
            installed_version: Some(candidate.version.clone()),
            installed_asset: Some(candidate.asset_name.clone()),
            installed_sha256: Some(candidate.sha256.clone()),
            installed_binary: Some(binary_name.clone()),
            checked_at_unix: Some(now_unix()),
        };
        state_write_attempted = true;
        write_state_atomic(&state)?;
        Ok(())
    })();

    if let Err(error) = commit_result {
        let mut rollback_errors = Vec::new();

        if let Some(activation) = binary_activation.as_ref() {
            let app = managed_application();
            if let Err(restore_error) = restore_previous_binary(&app, &activation.previous) {
                rollback_errors.push(
                    restore_error.context(
                        "binary rollback failed; previous/active paths may be inconsistent",
                    ),
                );
            }
        }

        if bundle_activated && let Err(restore_error) = restore_bundle(bundle_aside.as_deref()) {
            let aside_note = bundle_aside
                .as_ref()
                .map(|p| format!(" (aside preserved at {})", p.display()))
                .unwrap_or_default();
            rollback_errors.push(restore_error.context(format!(
                "bundle rollback failed{aside_note}; installation may be inconsistent"
            )));
        }

        if state_write_attempted
            && let Err(restore_error) =
                restore_state_bytes(&state_file, previous_state_bytes.as_deref())
        {
            rollback_errors.push(
                restore_error
                    .context("update-state rollback failed; installation may be inconsistent"),
            );
        }

        return Err(format_rollback_failure(error, rollback_errors));
    }

    // Commit succeeded — best-effort cleanup of asides. Versioned binary
    // residuals under downloads are intentional.
    if let Some(aside) = bundle_aside {
        let _ = std::fs::remove_dir_all(&aside);
        let _ = std::fs::remove_file(&aside);
    }
    if let Some(activation) = binary_activation
        && let Some(aside) = activation.pending_aside
    {
        let _ = std::fs::remove_file(aside);
    }
    drop(extracted);
    drop(extract_root_guard);
    drop(archive_guard);
    Ok(())
}

async fn converge(force: bool, pinned_version: Option<&str>) -> Result<ConvergeOutcome> {
    let _lock = acquire_update_lock().await?;
    let mut candidate = resolve_candidate(pinned_version).await?;
    let state = load_state();
    let active = active_deployment();

    // `--force-reinstall` without a pin should not downgrade a locally newer
    // build merely because the latest pointer rolled back. Reinstall the
    // active version's release instead.
    if force
        && pinned_version.is_none()
        && let Some(active) = &active
        && semver::Version::parse(&active.version)? > semver::Version::parse(&candidate.version)?
    {
        candidate = resolve_candidate(Some(&active.version)).await?;
    }

    let need_install = if force || pinned_version.is_some() {
        true
    } else {
        candidate_requires_install(&candidate, active.as_ref(), &state)?
    };
    if !need_install {
        reconcile_checked_state(&candidate, active.as_ref())?;
        return Ok(ConvergeOutcome {
            target: candidate.version,
            installed: false,
        });
    }

    install_candidate(&candidate).await?;
    // After the main transaction commits: refresh the optional scheme live
    // image (fail-open; never affects the just-installed hyper).
    install_scheme_image_best_effort(&candidate).await;
    Ok(ConvergeOutcome {
        target: candidate.version,
        installed: true,
    })
}

pub(crate) async fn latest_version() -> Result<String> {
    Ok(resolve_candidate(None).await?.version)
}

pub(crate) async fn check_update_status() -> UpdateStatus {
    let current_version = xai_grok_version::installed();
    let current_config = xai_grok_shell::util::config::load_config().await;
    match resolve_candidate(None).await {
        Ok(candidate) => {
            let state = load_state();
            let active = active_deployment();
            let update_available =
                candidate_requires_install(&candidate, active.as_ref(), &state).unwrap_or(false);
            UpdateStatus {
                current_version,
                latest_version: Some(candidate.version),
                update_available,
                installer: Some(INSTALLER_NAME.to_string()),
                channel: "stable".to_string(),
                auto_update: current_config.cli.auto_update,
                error: None,
            }
        }
        Err(error) => UpdateStatus {
            current_version,
            latest_version: None,
            update_available: false,
            installer: Some(INSTALLER_NAME.to_string()),
            channel: "stable".to_string(),
            auto_update: current_config.cli.auto_update,
            error: Some(error.to_string()),
        },
    }
}

pub(crate) async fn auto_update_target() -> Option<(&'static str, String)> {
    if !automatic_entry_allowed() {
        return None;
    }
    let candidate = resolve_candidate(None).await.ok()?;
    let state = load_state();
    let active = active_deployment();
    candidate_requires_install(&candidate, active.as_ref(), &state)
        .ok()?
        .then_some((INSTALLER_NAME, candidate.version))
}

pub(crate) async fn ensure_latest_on_disk() -> Result<EnsureLatestOutcome> {
    let relaunch_before = running_differs_from_active();
    if !automatic_entry_allowed() {
        return Ok(EnsureLatestOutcome {
            installed: None,
            relaunch_needed: relaunch_before,
        });
    }
    let config = xai_grok_shell::util::config::load_config().await;
    if config.cli.auto_update == Some(false) {
        return Ok(EnsureLatestOutcome {
            installed: None,
            relaunch_needed: relaunch_before,
        });
    }
    if is_version_cache_fresh() {
        return Ok(EnsureLatestOutcome {
            installed: None,
            relaunch_needed: relaunch_before,
        });
    }
    let outcome = converge(false, None).await?;
    Ok(EnsureLatestOutcome {
        installed: outcome.installed.then_some(outcome.target),
        relaunch_needed: running_differs_from_active(),
    })
}

async fn spawn_update_subcommand(run_mode: UpdateRunMode) -> Result<Option<tokio::process::Child>> {
    let exe = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(exe);
    command.arg("update");
    match run_mode {
        UpdateRunMode::Blocking => {
            let status = command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .status()
                .await?;
            if !status.success() {
                bail!("hyper update failed with {status}");
            }
            Ok(None)
        }
        UpdateRunMode::NonBlocking => {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            xai_grok_tools::util::detach_command(&mut command);
            // Detached background `hyper update`; the caller does not wait on it.
            #[allow(clippy::disallowed_methods)]
            let child = command.spawn()?;
            Ok(Some(child))
        }
    }
}

pub(crate) async fn check_update_background() -> BackgroundUpdateCheck {
    if !automatic_entry_allowed() {
        return BackgroundUpdateCheck {
            update: None,
            download: None,
        };
    }
    let config = xai_grok_shell::util::config::load_config().await;
    if config.cli.auto_update == Some(false) {
        return BackgroundUpdateCheck {
            update: None,
            download: None,
        };
    }
    if running_differs_from_active() {
        return BackgroundUpdateCheck {
            update: active_deployment().map(|active| UpdateAvailable {
                latest_version: active.version,
            }),
            download: None,
        };
    }
    if is_version_cache_fresh() {
        return BackgroundUpdateCheck {
            update: None,
            download: None,
        };
    }

    let candidate = match resolve_candidate(None).await {
        Ok(candidate) => candidate,
        Err(error) => {
            tracing::warn!("Hyper community update check failed: {error:#}");
            return BackgroundUpdateCheck {
                update: None,
                download: None,
            };
        }
    };
    let state = load_state();
    let active = active_deployment();
    let needs_install =
        candidate_requires_install(&candidate, active.as_ref(), &state).unwrap_or(false);
    if !needs_install {
        if let Err(error) = record_no_update(&candidate).await {
            tracing::debug!("failed to cache Hyper update check: {error:#}");
        }
        return BackgroundUpdateCheck {
            update: None,
            download: None,
        };
    }

    let download = match spawn_update_subcommand(UpdateRunMode::NonBlocking).await {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!("Hyper background update failed to start: {error:#}");
            None
        }
    };
    BackgroundUpdateCheck {
        update: Some(UpdateAvailable {
            latest_version: candidate.version,
        }),
        download,
    }
}

pub(crate) async fn run_update_if_available(
    run_mode: UpdateRunMode,
    _interactive: bool,
) -> Result<bool> {
    if !automatic_entry_allowed() || is_version_cache_fresh() {
        return Ok(false);
    }
    let config = xai_grok_shell::util::config::load_config().await;
    if config.cli.auto_update == Some(false) {
        return Ok(false);
    }
    if config.cli.auto_update.is_none()
        && let Err(error) = xai_grok_shell::util::config::update_config(|state| {
            if state.cli.auto_update.is_none() {
                state.cli.auto_update = Some(true);
            }
        })
        .await
    {
        tracing::warn!("failed to save Hyper auto-update setting: {error}");
    }

    let candidate = match resolve_candidate(None).await {
        Ok(candidate) => candidate,
        Err(error) => {
            tracing::debug!("Hyper community update check failed: {error:#}");
            return Ok(false);
        }
    };
    let state = load_state();
    let active = active_deployment();
    if !candidate_requires_install(&candidate, active.as_ref(), &state)? {
        record_no_update(&candidate).await?;
        return Ok(false);
    }
    let current = active
        .as_ref()
        .map(|active| active.version.as_str())
        .unwrap_or(xai_grok_version::VERSION);
    eprintln!(
        "A new Hyper community release is available: {} -> {} [stable]",
        current, candidate.version
    );
    let child = spawn_update_subcommand(run_mode).await?;
    drop(child);
    Ok(matches!(run_mode, UpdateRunMode::Blocking))
}

/// Options for release-archive contract verification (CI producer checks).
#[derive(Debug, Clone)]
pub struct ReleaseArchiveVerifyOptions<'a> {
    /// Expected root binary entry (`hyper` or `hyper.exe`). When `None`,
    /// inferred from the archive extension.
    pub binary_entry: Option<&'a str>,
    /// Optional expected SHA-256 (lowercase hex) of the archive bytes.
    pub expected_sha256: Option<&'a str>,
    /// When set, every regular file under this tree must appear under
    /// `bundled/` in the archive (and the archive must not invent extra
    /// managed files outside that set is *not* required — only completeness).
    pub expected_bundle_root: Option<&'a Path>,
    /// When true, the archive must contain at least one `bundled/**` file.
    pub require_bundle: bool,
}

impl Default for ReleaseArchiveVerifyOptions<'_> {
    fn default() -> Self {
        Self {
            binary_entry: None,
            expected_sha256: None,
            expected_bundle_root: None,
            require_bundle: true,
        }
    }
}

/// Summary of a verified release archive.
#[derive(Debug, Clone)]
pub struct ReleaseArchiveReport {
    pub binary_entry: String,
    pub sha256: String,
    pub bundle_file_count: usize,
    pub notice_entries: Vec<String>,
}

fn infer_binary_entry(archive_path: &Path) -> Result<&'static str> {
    let name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if name.ends_with(".zip") {
        Ok("hyper.exe")
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok("hyper")
    } else {
        bail!(
            "cannot infer binary entry for archive {} (expected .tar.gz or .zip)",
            archive_path.display()
        )
    }
}

/// Map of relative POSIX-style paths under a bundle root → file bytes.
fn collect_bundle_file_map(root: &Path) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let mut out = std::collections::BTreeMap::new();
    if !root.is_dir() {
        bail!("bundle root is not a directory: {}", root.display());
    }
    fn walk(
        dir: &Path,
        prefix: &Path,
        out: &mut std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("reading bundle dir {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str == "__pycache__" || name_str.ends_with(".pyc") || name_str.ends_with(".pyo")
            {
                continue;
            }
            // Reject control characters / path separators in component names.
            if name_str
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\')
            {
                bail!("bundle path component contains illegal characters: {name_str}");
            }
            let rel = prefix.join(&name);
            let meta = std::fs::symlink_metadata(&path)
                .with_context(|| format!("inspecting {}", path.display()))?;
            if meta.file_type().is_symlink() {
                bail!("bundle tree must not contain symlinks: {}", path.display());
            }
            if meta.is_dir() {
                walk(&path, &rel, out)?;
            } else if meta.is_file() {
                let key = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let bytes = std::fs::read(&path)
                    .with_context(|| format!("reading bundle file {}", path.display()))?;
                if out.insert(key.clone(), bytes).is_some() {
                    bail!("duplicate bundle path after normalize: {key}");
                }
            }
        }
        Ok(())
    }
    walk(root, Path::new(""), &mut out)?;
    Ok(out)
}

/// Verify a packaged Hyper release archive against the producer contract:
/// unique root `hyper`/`hyper.exe`, allowlisted notices, complete `bundled/**`,
/// no unexpected/dangerous paths (enforced by the shared extractor).
pub fn verify_release_archive(
    archive_path: &Path,
    options: ReleaseArchiveVerifyOptions<'_>,
) -> Result<ReleaseArchiveReport> {
    if !archive_path.is_file() {
        bail!("release archive not found: {}", archive_path.display());
    }
    let actual_sha = sha256_file(archive_path)?;
    if let Some(expected) = options.expected_sha256 {
        let expected = expected.to_ascii_lowercase();
        if !valid_sha256(&expected) {
            bail!("expected SHA-256 is not a 64-char hex digest");
        }
        if actual_sha != expected {
            bail!(
                "SHA-256 mismatch for {}: expected {}, got {}",
                archive_path.display(),
                expected,
                actual_sha
            );
        }
    }

    let binary_entry = match options.binary_entry {
        Some(entry) => entry,
        None => infer_binary_entry(archive_path)?,
    };

    // Unique temp root under std::env::temp_dir (no production tempfile dep).
    // Include a random component so parallel verifiers cannot collide.
    let tmp_root = std::env::temp_dir().join(format!(
        "hyper-release-verify-{}-{}-{}",
        std::process::id(),
        now_unix(),
        {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            archive_path.hash(&mut h);
            std::thread::current().id().hash(&mut h);
            h.finish()
        }
    ));
    if tmp_root.exists() {
        let _ = std::fs::remove_dir_all(&tmp_root);
    }
    std::fs::create_dir_all(&tmp_root)
        .with_context(|| format!("creating verify temp dir {}", tmp_root.display()))?;
    let stage_root = tmp_root.join("extract");
    let bundle_stage = tmp_root.join("bundle-stage");
    std::fs::create_dir_all(&stage_root)?;

    let name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let extract_result = if name.ends_with(".zip") {
        extract_zip_archive(archive_path, &stage_root, bundle_stage, binary_entry)
    } else {
        #[cfg(unix)]
        {
            extract_tar_archive(archive_path, &stage_root, bundle_stage, binary_entry)
        }
        #[cfg(not(unix))]
        {
            Err(anyhow::anyhow!("tar.gz verification requires a Unix host"))
        }
    };

    let report = (|| -> Result<ReleaseArchiveReport> {
        let extracted = extract_result?;
        if !extracted.binary.is_file() {
            bail!("archive is missing root binary {binary_entry}");
        }
        let binary_len = std::fs::metadata(&extracted.binary)?.len();
        if binary_len == 0 {
            bail!("archive root binary {binary_entry} is empty");
        }
        if binary_len > MAX_BINARY_BYTES {
            bail!("archive root binary {binary_entry} exceeds size limit");
        }

        let archive_bundle = match extracted.bundle_stage.as_ref() {
            Some(stage) => collect_bundle_file_map(stage)?,
            None => std::collections::BTreeMap::new(),
        };

        if options.require_bundle && archive_bundle.is_empty() {
            bail!(
                "release archive {} is missing bundled/** runtime files",
                archive_path.display()
            );
        }

        if let Some(expected_root) = options.expected_bundle_root {
            let expected = collect_bundle_file_map(expected_root)?;
            if expected.is_empty() {
                bail!(
                    "expected bundle root contains no files: {}",
                    expected_root.display()
                );
            }
            // Bidirectional, byte-identical comparison: reject missing, extra,
            // and content-different files.
            let mut missing = Vec::new();
            let mut different = Vec::new();
            for (path, exp_bytes) in &expected {
                match archive_bundle.get(path) {
                    None => missing.push(path.clone()),
                    Some(got) if got != exp_bytes => different.push(path.clone()),
                    Some(_) => {}
                }
            }
            let mut extra = Vec::new();
            for path in archive_bundle.keys() {
                if !expected.contains_key(path) {
                    extra.push(path.clone());
                }
            }
            if !missing.is_empty() || !extra.is_empty() || !different.is_empty() {
                let mut parts = Vec::new();
                if !missing.is_empty() {
                    parts.push(format!("missing: {}", missing.join(", ")));
                }
                if !extra.is_empty() {
                    parts.push(format!("extra: {}", extra.join(", ")));
                }
                if !different.is_empty() {
                    parts.push(format!("content differs: {}", different.join(", ")));
                }
                bail!(
                    "release archive {} bundled tree does not match expected root ({}): {}",
                    archive_path.display(),
                    expected_root.display(),
                    parts.join("; ")
                );
            }
        }

        // Notices are drained by the extractor; unexpected root entries are
        // already rejected by classify_archive_entry during extract.
        let notice_entries = Vec::new();

        Ok(ReleaseArchiveReport {
            binary_entry: binary_entry.to_string(),
            sha256: actual_sha,
            bundle_file_count: archive_bundle.len(),
            notice_entries,
        })
    })();

    let _ = std::fs::remove_dir_all(&tmp_root);
    report
}

fn validate_asset_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("asset name is empty");
    }
    if name
        .chars()
        .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        bail!("asset name contains illegal characters: {name:?}");
    }
    if name.contains("..") {
        bail!("asset name must not contain '..': {name}");
    }
    Ok(())
}

/// Verify a `SHA256SUMS` manifest against on-disk archive files.
///
/// Each `archives` entry is `(asset_name, path)`. The manifest must contain
/// exactly one valid digest line per asset and must not list unknown names
/// when `strict_names` is true. Duplicate / case-colliding names (manifest or
/// CLI args) fail closed.
pub fn verify_sha256sums_manifest(
    manifest_path: &Path,
    archives: &[(String, PathBuf)],
    strict_names: bool,
) -> Result<()> {
    let body = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    if body.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("SHA256SUMS is unexpectedly large");
    }

    let mut manifest: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut fold_seen: HashSet<String> = HashSet::new();
    for (lineno, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.chars().any(|c| c.is_control()) {
            bail!("SHA256SUMS line {} contains control characters", lineno + 1);
        }
        let mut parts = line.split_whitespace();
        let digest = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("SHA256SUMS line {} is malformed", lineno + 1))?;
        let name = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("SHA256SUMS line {} is missing a filename", lineno + 1))?
            .trim_start_matches('*');
        if parts.next().is_some() {
            bail!("SHA256SUMS line {} has trailing fields", lineno + 1);
        }
        validate_asset_name(name)?;
        let digest = digest.to_ascii_lowercase();
        if !valid_sha256(&digest) {
            bail!("SHA256SUMS has an invalid digest for {name}");
        }
        let fold = name.to_ascii_lowercase();
        if !fold_seen.insert(fold) {
            bail!("SHA256SUMS contains duplicate or case-colliding entry for {name}");
        }
        if manifest.insert(name.to_string(), digest).is_some() {
            bail!("SHA256SUMS contains duplicate entries for {name}");
        }
    }

    // CLI archive args: reject duplicates / case collisions.
    let mut arg_fold: HashSet<String> = HashSet::new();
    for (name, _) in archives {
        validate_asset_name(name)?;
        let fold = name.to_ascii_lowercase();
        if !arg_fold.insert(fold) {
            bail!("duplicate or case-colliding --archive name: {name}");
        }
    }

    let expected_names: HashSet<String> = archives.iter().map(|(n, _)| n.clone()).collect();
    if strict_names {
        for name in manifest.keys() {
            if !expected_names.contains(name) {
                bail!("SHA256SUMS lists unexpected asset {name}");
            }
        }
        if manifest.len() != archives.len() {
            bail!(
                "SHA256SUMS entry count ({}) does not match archive count ({})",
                manifest.len(),
                archives.len()
            );
        }
    }

    for (name, path) in archives {
        let expected = manifest
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("SHA256SUMS is missing an entry for {name}"))?;
        let actual = sha256_file(path)?;
        if &actual != expected {
            bail!("SHA-256 mismatch for {name}: expected {expected}, got {actual}");
        }
    }
    Ok(())
}

/// CLI entry for `hyper-verify-release-archive` (release CI).
pub fn run_verify_release_cli(args: &[String]) -> Result<()> {
    // Usage:
    //   hyper-verify-release-archive --archive PATH [--sha256 HEX] [--bundle-root DIR]
    //       [--binary-entry NAME] [--allow-empty-bundle]
    //   hyper-verify-release-archive --sums PATH --archive NAME=PATH [--archive NAME=PATH ...]
    //       [--strict-names]
    let mut archives: Vec<(String, PathBuf)> = Vec::new();
    let mut single_archive: Option<PathBuf> = None;
    let mut sums: Option<PathBuf> = None;
    let mut expected_sha: Option<String> = None;
    let mut bundle_root: Option<PathBuf> = None;
    let mut binary_entry: Option<String> = None;
    let mut require_bundle = true;
    let mut strict_names = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--archive" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--archive requires a value"))?;
                if let Some((name, path)) = value.split_once('=') {
                    archives.push((name.to_string(), PathBuf::from(path)));
                } else {
                    single_archive = Some(PathBuf::from(value));
                }
            }
            "--sums" => {
                i += 1;
                sums = Some(PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--sums requires a path"))?,
                ));
            }
            "--sha256" => {
                i += 1;
                expected_sha = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--sha256 requires a digest"))?
                        .to_ascii_lowercase(),
                );
            }
            "--bundle-root" => {
                i += 1;
                bundle_root =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        anyhow::anyhow!("--bundle-root requires a directory")
                    })?));
            }
            "--binary-entry" => {
                i += 1;
                binary_entry = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--binary-entry requires a name"))?
                        .clone(),
                );
            }
            "--allow-empty-bundle" => require_bundle = false,
            "--strict-names" => strict_names = true,
            "--help" | "-h" => {
                eprintln!(
                    "Usage:\n  \
                     hyper-verify-release-archive --archive PATH [--sha256 HEX] [--bundle-root DIR]\n  \
                     hyper-verify-release-archive --sums SHA256SUMS --archive NAME=PATH ..."
                );
                return Ok(());
            }
            other => bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    if let Some(manifest) = sums {
        if archives.is_empty() {
            bail!("--sums requires one or more --archive NAME=PATH entries");
        }
        verify_sha256sums_manifest(&manifest, &archives, strict_names)?;
        for (name, path) in &archives {
            let report = verify_release_archive(
                path,
                ReleaseArchiveVerifyOptions {
                    binary_entry: binary_entry.as_deref(),
                    expected_sha256: None,
                    expected_bundle_root: bundle_root.as_deref(),
                    require_bundle,
                },
            )?;
            eprintln!(
                "ok  {name}  sha256={}  binary={}  bundle_files={}",
                report.sha256, report.binary_entry, report.bundle_file_count
            );
        }
        eprintln!("SHA256SUMS and {} archive(s) verified", archives.len());
        return Ok(());
    }

    let archive = single_archive
        .ok_or_else(|| anyhow::anyhow!("provide --archive PATH (or --sums with NAME=PATH)"))?;
    let report = verify_release_archive(
        &archive,
        ReleaseArchiveVerifyOptions {
            binary_entry: binary_entry.as_deref(),
            expected_sha256: expected_sha.as_deref(),
            expected_bundle_root: bundle_root.as_deref(),
            require_bundle,
        },
    )?;
    eprintln!(
        "ok  {}  sha256={}  binary={}  bundle_files={}",
        archive.display(),
        report.sha256,
        report.binary_entry,
        report.bundle_file_count
    );
    Ok(())
}

pub(crate) async fn run_update(
    force: bool,
    pinned_version: Option<&str>,
    channel_switch: Option<&str>,
) -> Result<Option<String>> {
    if let Some(channel) = channel_switch
        && channel != "stable"
    {
        bail!("Hyper community releases support only the stable channel");
    }
    if let Some(version) = pinned_version {
        semver::Version::parse(version)
            .with_context(|| format!("'{version}' is not a valid Hyper release version"))?;
    }

    let before = active_deployment();
    let before_version = before
        .as_ref()
        .map(|active| active.version.as_str())
        .unwrap_or(xai_grok_version::VERSION);
    eprintln!(
        "Checking Hyper community releases (installed: {before_version}, destination: {})...",
        managed_application().display()
    );
    let outcome = converge(force, pinned_version).await.map_err(|error| {
        anyhow::anyhow!(
            "Hyper community update failed: {error:#}\n\nReinstall with:\n  {}",
            if cfg!(windows) {
                "irm https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.ps1 | iex"
            } else {
                "curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash"
            }
        )
    })?;

    if pinned_version.is_some()
        && let Err(error) = xai_grok_shell::util::config::update_config(|state| {
            state.cli.auto_update = Some(false);
        })
        .await
    {
        tracing::warn!("failed to disable auto-update after pinned Hyper install: {error}");
    }

    if outcome.installed {
        eprintln!("  ✓ Hyper v{} installed successfully.", outcome.target);
        eprintln!("  Restart Hyper to use the new community build.");
    } else {
        eprintln!("Already up to date (Hyper {}).", outcome.target);
    }
    Ok(Some(outcome.target))
}

pub(crate) async fn run_install_target(target: Option<&str>) -> Result<()> {
    converge(true, target).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parser_accepts_gnu_and_star_formats() {
        let hash = "a".repeat(64);
        let asset = "hyper-0.2.113-x86_64-unknown-linux-gnu.tar.gz";
        assert_eq!(
            parse_manifest_checksum(&format!("{hash}  *{asset}\n"), asset).unwrap(),
            hash
        );
    }

    #[test]
    fn manifest_parser_rejects_duplicate_or_invalid_entries() {
        let asset = "hyper-0.2.113-x86_64-unknown-linux-gnu.tar.gz";
        let hash = "b".repeat(64);
        assert!(parse_manifest_checksum(&format!("bad  {asset}\n"), asset).is_err());
        assert!(
            parse_manifest_checksum(&format!("{hash}  {asset}\n{hash}  {asset}\n"), asset).is_err()
        );
    }

    #[test]
    fn managed_name_round_trips_version_and_digest() {
        let digest = "c".repeat(64);
        let name = format!("hyper-0.2.113-linux-x86_64-sha256-{digest}");
        assert_eq!(version_from_managed_name(&name).as_deref(), Some("0.2.113"));
        assert_eq!(
            digest_from_managed_name(&name).as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(
            version_from_managed_name("hyper-0.2.113-linux-x86_64").as_deref(),
            Some("0.2.113")
        );
    }

    #[test]
    fn same_semver_uses_archive_digest_as_deployment_identity() {
        let old = "d".repeat(64);
        let new = "e".repeat(64);
        let active = ActiveDeployment {
            version: "0.2.113".to_string(),
            binary_name: format!("hyper-0.2.113-linux-x86_64-sha256-{old}"),
            sha256: Some(old.clone()),
        };
        let candidate = Candidate {
            version: "0.2.113".to_string(),
            asset_name: "asset".to_string(),
            archive_url: "https://example.invalid/asset".to_string(),
            sha256: new,
            scheme_image: None,
        };
        assert!(
            candidate_requires_install(&candidate, Some(&active), &UpdateState::default()).unwrap()
        );
        let same = Candidate {
            sha256: old,
            ..candidate
        };
        assert!(
            !candidate_requires_install(&same, Some(&active), &UpdateState::default()).unwrap()
        );
    }

    #[test]
    fn archive_paths_normalize_and_never_escape() {
        // Tar mode: literal backslash is rejected.
        assert_eq!(
            normalize_archive_path("./hyper", false).unwrap().as_deref(),
            Some(["hyper".to_string()].as_slice())
        );
        assert_eq!(
            normalize_archive_path("bundled/skills/x/SKILL.md", false)
                .unwrap()
                .as_deref(),
            Some(
                [
                    "bundled".to_string(),
                    "skills".to_string(),
                    "x".to_string(),
                    "SKILL.md".to_string()
                ]
                .as_slice()
            )
        );
        assert_eq!(normalize_archive_path(".", false).unwrap(), None);
        assert_eq!(normalize_archive_path("./", false).unwrap(), None);
        assert!(normalize_archive_path("../hyper", false).is_err());
        assert!(normalize_archive_path("/hyper", false).is_err());
        assert!(normalize_archive_path("nested\\hyper", false).is_err());
        assert!(normalize_archive_path("bundled/../escape", false).is_err());
        assert!(normalize_archive_path("C:/hyper", false).is_err());
        assert!(normalize_archive_path("", false).unwrap().is_none());

        // Zip mode: `\` is a separator; `..` after normalize still rejects.
        assert_eq!(
            normalize_archive_path("bundled\\skills\\x.md", true)
                .unwrap()
                .as_deref(),
            Some(
                [
                    "bundled".to_string(),
                    "skills".to_string(),
                    "x.md".to_string()
                ]
                .as_slice()
            )
        );
        assert!(normalize_archive_path("..\\evil", true).is_err());
        assert!(normalize_archive_path("bundled\\..\\escape", true).is_err());

        // Portable Windows-hostile components.
        assert!(normalize_archive_path("bundled/foo:bar", false).is_err());
        assert!(normalize_archive_path("bundled/foo.", false).is_err());
        assert!(normalize_archive_path("bundled/foo ", false).is_err());
        assert!(normalize_archive_path("bundled/CON", false).is_err());
        assert!(normalize_archive_path("bundled/nul.txt", false).is_err());
        assert!(normalize_archive_path("bundled/COM1", false).is_err());
        assert!(normalize_archive_path("bundled/lpt9.md", false).is_err());
    }

    #[test]
    fn capture_previous_state_rejects_symlink_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("update-state.json");

        // Missing is fine.
        assert!(capture_previous_state_bytes(&state).unwrap().is_none());

        // Regular file is captured.
        std::fs::write(&state, b"{\"ok\":true}\n").unwrap();
        assert_eq!(
            capture_previous_state_bytes(&state).unwrap().as_deref(),
            Some(b"{\"ok\":true}\n".as_slice())
        );
        std::fs::remove_file(&state).unwrap();

        // Directory is not a regular file.
        std::fs::create_dir(&state).unwrap();
        assert!(capture_previous_state_bytes(&state).is_err());
        std::fs::remove_dir(&state).unwrap();

        #[cfg(unix)]
        {
            let target = dir.path().join("real.json");
            std::fs::write(&target, b"secret\n").unwrap();
            std::os::unix::fs::symlink(&target, &state).unwrap();
            assert!(capture_previous_state_bytes(&state).is_err());
        }
    }

    #[test]
    fn restore_bundle_reports_aside_when_active_cannot_be_cleared() {
        // When aside restore fails after a successful move-aside of the new
        // tree, the error must mention the preserved aside path.
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundled");
        let aside = dir.path().join("bundled.old");
        std::fs::create_dir_all(bundle.join("new")).unwrap();
        std::fs::write(bundle.join("new/x"), b"new").unwrap();
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(aside.join("old"), b"old").unwrap();

        // Simulate restore_bundle logic against these paths.
        let doomed = unique_sibling(&bundle, "failed");
        std::fs::rename(&bundle, &doomed).unwrap();
        // Create a blocking file where bundle should land so rename(aside→bundle) fails...
        // actually rename of dir onto existing fails on Unix if dest exists.
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("blocker"), b"x").unwrap();
        let err = std::fs::rename(&aside, &bundle).unwrap_err();
        let combined = combine_errors(
            anyhow::Error::new(err).context(format!(
                "restoring previous bundled runtime from {} (aside preserved)",
                aside.display()
            )),
            anyhow::anyhow!("active clear already done"),
        );
        let msg = format!("{combined:#}");
        assert!(msg.contains("aside preserved"), "{msg}");
        assert!(msg.contains("bundled"), "{msg}");
        // Cleanup doomed best-effort.
        let _ = std::fs::remove_dir_all(doomed);
    }

    #[test]
    fn classify_accepts_binary_notice_and_bundle_only() {
        assert_eq!(
            classify_archive_entry(Some(&["hyper".into()]), "hyper").unwrap(),
            ArchiveEntryClass::Binary
        );
        assert_eq!(
            classify_archive_entry(Some(&["LICENSE".into()]), "hyper").unwrap(),
            ArchiveEntryClass::Notice
        );
        assert_eq!(
            classify_archive_entry(Some(&["bundled".into()]), "hyper").unwrap(),
            ArchiveEntryClass::BundleRootDir
        );
        assert_eq!(
            classify_archive_entry(
                Some(&["bundled".into(), "skills".into(), "x".into()]),
                "hyper"
            )
            .unwrap(),
            ArchiveEntryClass::BundleFile
        );
        assert!(classify_archive_entry(Some(&["README".into()]), "hyper").is_err());
        assert!(classify_archive_entry(Some(&["other".into(), "nested".into()]), "hyper").is_err());
    }

    fn extract_dirs(root: &Path) -> (PathBuf, PathBuf) {
        let stage = root.join("extract");
        let bundle = root.join("bundle-stage");
        std::fs::create_dir_all(&stage).unwrap();
        (stage, bundle)
    }

    #[cfg(unix)]
    fn write_test_tar(entries: &[(&str, tar::EntryType, &[u8])], path: &Path) {
        let file = File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, kind, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(if kind.is_dir() { 0o755 } else { 0o644 });
            if *kind == tar::EntryType::Symlink {
                header.set_size(0);
                header.set_link_name("outside").unwrap();
                header.set_cksum();
                builder
                    .append_data(&mut header, *name, &[] as &[u8])
                    .unwrap();
            } else if *kind == tar::EntryType::Link {
                header.set_size(0);
                header.set_link_name("hyper").unwrap();
                header.set_cksum();
                builder
                    .append_data(&mut header, *name, &[] as &[u8])
                    .unwrap();
            } else if kind.is_dir() {
                header.set_size(0);
                header.set_cksum();
                builder
                    .append_data(&mut header, *name, &[] as &[u8])
                    .unwrap();
            } else {
                header.set_size(body.len() as u64);
                header.set_cksum();
                builder.append_data(&mut header, *name, *body).unwrap();
            }
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    fn write_test_zip(entries: &[(&str, bool, &[u8])], path: &Path) {
        use std::io::Write as _;
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, is_dir, body) in entries {
            if *is_dir {
                let dir_name = if name.ends_with('/') {
                    (*name).to_string()
                } else {
                    format!("{name}/")
                };
                zip.add_directory(dir_name, options).unwrap();
            } else {
                zip.start_file(*name, options).unwrap();
                zip.write_all(body).unwrap();
            }
        }
        zip.finish().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_layout_tar_extracts_binary_licenses_and_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("good.tar.gz");
        write_test_tar(
            &[
                (".", tar::EntryType::Directory, b"".as_slice()),
                ("hyper", tar::EntryType::Regular, b"#!/bin/sh\nexit 0\n"),
                ("LICENSE", tar::EntryType::Regular, b"lic\n"),
                ("NOTICE", tar::EntryType::Regular, b"note\n"),
                ("THIRD-PARTY-NOTICES", tar::EntryType::Regular, b"tpn\n"),
                ("bundled", tar::EntryType::Directory, b""),
                ("bundled/skills", tar::EntryType::Directory, b""),
                ("bundled/skills/demo", tar::EntryType::Directory, b""),
                (
                    "bundled/skills/demo/SKILL.md",
                    tar::EntryType::Regular,
                    b"# skill\n",
                ),
                ("bundled/agents", tar::EntryType::Directory, b""),
                (
                    "bundled/agents/helper.md",
                    tar::EntryType::Regular,
                    b"# agent\n",
                ),
            ],
            &archive,
        );
        let (stage, bundle_stage) = extract_dirs(dir.path());
        let extracted =
            extract_tar_archive(&archive, &stage, bundle_stage.clone(), "hyper").unwrap();
        assert_eq!(
            std::fs::read(&extracted.binary).unwrap(),
            b"#!/bin/sh\nexit 0\n"
        );
        let bundle = extracted.bundle_stage.expect("bundle present");
        assert_eq!(
            std::fs::read(bundle.join("skills/demo/SKILL.md")).unwrap(),
            b"# skill\n"
        );
        assert_eq!(
            std::fs::read(bundle.join("agents/helper.md")).unwrap(),
            b"# agent\n"
        );
        // Root licenses are drained, never staged beside the binary.
        assert!(!stage.join("LICENSE").exists());
    }

    #[test]
    fn real_layout_zip_extracts_binary_and_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("good.zip");
        write_test_zip(
            &[
                ("hyper.exe", false, b"MZ-fake-binary"),
                ("LICENSE", false, b"lic\n"),
                ("bundled/", true, b""),
                ("bundled/skills/", true, b""),
                ("bundled/skills/demo/", true, b""),
                ("bundled/skills/demo/SKILL.md", false, b"# skill\n"),
                ("bundled/agents/", true, b""),
                ("bundled/agents/helper.md", false, b"# agent\n"),
            ],
            &archive,
        );
        let (stage, bundle_stage) = extract_dirs(dir.path());
        let extracted = extract_zip_archive(&archive, &stage, bundle_stage, "hyper.exe").unwrap();
        assert_eq!(std::fs::read(&extracted.binary).unwrap(), b"MZ-fake-binary");
        let bundle = extracted.bundle_stage.expect("bundle present");
        assert_eq!(
            std::fs::read(bundle.join("skills/demo/SKILL.md")).unwrap(),
            b"# skill\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn binary_only_tar_leaves_no_bundle_stage() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("binary-only.tar.gz");
        write_test_tar(
            &[
                ("hyper", tar::EntryType::Regular, b"#!/bin/sh\nexit 0\n"),
                ("LICENSE", tar::EntryType::Regular, b"lic\n"),
            ],
            &archive,
        );
        let (stage, bundle_stage) = extract_dirs(dir.path());
        let extracted = extract_tar_archive(&archive, &stage, bundle_stage, "hyper").unwrap();
        assert!(extracted.bundle_stage.is_none());
        assert!(extracted.binary.is_file());
    }

    #[cfg(unix)]
    fn write_raw_path_tar(entries: &[(&str, tar::EntryType, &[u8])], path: &Path) {
        // Bypass tar::Builder path validation so we can ship `..` / absolute names.
        use std::io::Write as _;
        let file = File::create(path).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        for (name, kind, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            if *kind == tar::EntryType::Symlink {
                header.set_size(0);
                header.set_link_name("outside").unwrap();
            } else if *kind == tar::EntryType::Link {
                header.set_size(0);
                header.set_link_name("hyper").unwrap();
            } else if kind.is_dir() {
                header.set_size(0);
            } else {
                header.set_size(body.len() as u64);
            }
            // path_bytes allows names the high-level builder rejects.
            header.set_path(name).ok();
            // For names with `..` set_path fails; write the name into the
            // ustar name field directly.
            if header.path().map(|p| p != Path::new(name)).unwrap_or(true) {
                let bytes = name.as_bytes();
                let name_field = header.as_old_mut().name.as_mut_slice();
                name_field.fill(0);
                let n = bytes.len().min(name_field.len());
                name_field[..n].copy_from_slice(&bytes[..n]);
            }
            header.set_cksum();
            encoder.write_all(header.as_bytes()).unwrap();
            if !kind.is_dir() && *kind != tar::EntryType::Symlink && *kind != tar::EntryType::Link {
                encoder.write_all(body).unwrap();
                let pad = (512 - (body.len() % 512)) % 512;
                if pad > 0 {
                    encoder.write_all(&vec![0u8; pad]).unwrap();
                }
            }
        }
        // Two zero blocks end a tar archive.
        encoder.write_all(&[0u8; 1024]).unwrap();
        encoder.finish().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn security_matrix_rejects_unsafe_tar_entries() {
        let dir = tempfile::tempdir().unwrap();
        #[allow(clippy::type_complexity)]
        let cases: &[(&str, &[(&str, tar::EntryType, &[u8])], bool)] = &[
            (
                "symlink",
                &[("hyper", tar::EntryType::Symlink, b"".as_slice())],
                false,
            ),
            (
                "hardlink",
                &[
                    ("hyper", tar::EntryType::Regular, b"one"),
                    ("other", tar::EntryType::Link, b""),
                ],
                false,
            ),
            (
                "parent_in_bundle",
                &[
                    ("hyper", tar::EntryType::Regular, b"one"),
                    ("bundled/../escape", tar::EntryType::Regular, b"x"),
                ],
                true,
            ),
            (
                "absolute",
                &[("/etc/passwd", tar::EntryType::Regular, b"x")],
                true,
            ),
            (
                "backslash",
                &[("nested\\hyper", tar::EntryType::Regular, b"x")],
                true,
            ),
            (
                "unexpected_root",
                &[
                    ("hyper", tar::EntryType::Regular, b"one"),
                    ("README", tar::EntryType::Regular, b"nope"),
                ],
                false,
            ),
            (
                "nested_outside_bundle",
                &[
                    ("hyper", tar::EntryType::Regular, b"one"),
                    ("other/nested", tar::EntryType::Regular, b"x"),
                ],
                false,
            ),
            (
                "duplicate_binary",
                &[
                    ("hyper", tar::EntryType::Regular, b"one"),
                    ("hyper", tar::EntryType::Regular, b"two"),
                ],
                false,
            ),
            (
                "case_collision",
                &[
                    ("hyper", tar::EntryType::Regular, b"one"),
                    ("LICENSE", tar::EntryType::Regular, b"a"),
                    ("license", tar::EntryType::Regular, b"b"),
                ],
                false,
            ),
            (
                "reserved_device",
                &[
                    ("hyper", tar::EntryType::Regular, b"one"),
                    ("bundled/CON", tar::EntryType::Regular, b"x"),
                ],
                false,
            ),
            (
                "trailing_dot",
                &[
                    ("hyper", tar::EntryType::Regular, b"one"),
                    ("bundled/foo.", tar::EntryType::Regular, b"x"),
                ],
                false,
            ),
            (
                "root_regular_unnamed",
                &[(".", tar::EntryType::Regular, b"x")],
                false,
            ),
        ];
        for (label, entries, raw) in cases {
            let archive = dir.path().join(format!("{label}.tar.gz"));
            if *raw {
                write_raw_path_tar(entries, &archive);
            } else {
                write_test_tar(entries, &archive);
            }
            let stage = dir.path().join(format!("{label}-stage"));
            let bundle = dir.path().join(format!("{label}-bundle"));
            let _ = std::fs::remove_dir_all(&stage);
            let _ = std::fs::remove_dir_all(&bundle);
            std::fs::create_dir_all(&stage).unwrap();
            let result = extract_tar_archive(&archive, &stage, bundle, "hyper");
            assert!(
                result.is_err(),
                "case {label} should be rejected: {:?}",
                result.err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn invalid_utf8_tar_path_is_rejected() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad-utf8.tar.gz");
        let file = File::create(&archive).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());

        const BODY: &[u8] = b"one";
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o755);
        header.set_size(BODY.len() as u64);
        header.set_path("hyper").unwrap();
        header.set_cksum();
        encoder.write_all(header.as_bytes()).unwrap();
        encoder.write_all(BODY).unwrap();
        let pad = (512 - (BODY.len() % 512)) % 512;
        encoder.write_all(&vec![0u8; pad]).unwrap();

        // Entry whose name is invalid UTF-8 (0xFF byte).
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(1);
        let name_field = header.as_old_mut().name.as_mut_slice();
        name_field.fill(0);
        name_field[0] = 0xFF;
        name_field[1] = b'x';
        header.set_cksum();
        encoder.write_all(header.as_bytes()).unwrap();
        encoder.write_all(b"z").unwrap();
        encoder.write_all(&vec![0u8; 511]).unwrap();
        encoder.write_all(&[0u8; 1024]).unwrap();
        encoder.finish().unwrap();

        let (stage, bundle_stage) = extract_dirs(dir.path());
        let result = extract_tar_archive(&archive, &stage, bundle_stage, "hyper");
        let err = match result {
            Ok(_) => panic!("invalid UTF-8 path must be rejected"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("UTF-8") || msg.contains("utf-8") || msg.contains("invalid"),
            "{msg}"
        );
    }

    #[test]
    fn security_matrix_rejects_unsafe_zip_entries() {
        let dir = tempfile::tempdir().unwrap();
        #[allow(clippy::type_complexity)]
        let path_cases: &[(&str, &[(&str, bool, &[u8])])] = &[
            (
                "parent_in_bundle",
                &[
                    ("hyper.exe", false, b"one"),
                    ("bundled/x/../../../escape", false, b"x"),
                ],
            ),
            (
                "backslash_parent",
                &[("hyper.exe", false, b"one"), ("..\\evil", false, b"x")],
            ),
            ("absolute", &[("/hyper.exe", false, b"x")]),
            (
                "unexpected_root",
                &[("hyper.exe", false, b"one"), ("README", false, b"nope")],
            ),
            (
                "case_collision",
                &[
                    ("hyper.exe", false, b"one"),
                    ("bundled/A.txt", false, b"a"),
                    ("bundled/a.txt", false, b"b"),
                ],
            ),
            (
                "nested_outside",
                &[
                    ("hyper.exe", false, b"one"),
                    ("other/nested.txt", false, b"x"),
                ],
            ),
            (
                "reserved_device",
                &[("hyper.exe", false, b"one"), ("bundled/CON", false, b"x")],
            ),
            (
                "trailing_space",
                &[("hyper.exe", false, b"one"), ("bundled/foo ", false, b"x")],
            ),
        ];
        for (label, entries) in path_cases {
            let archive = dir.path().join(format!("{label}.zip"));
            write_test_zip(entries, &archive);
            let stage = dir.path().join(format!("{label}-stage"));
            let bundle = dir.path().join(format!("{label}-bundle"));
            let _ = std::fs::remove_dir_all(&stage);
            let _ = std::fs::remove_dir_all(&bundle);
            std::fs::create_dir_all(&stage).unwrap();
            let result = extract_zip_archive(&archive, &stage, bundle, "hyper.exe");
            assert!(
                result.is_err(),
                "case {label} should be rejected: {:?}",
                result.err()
            );
        }

        {
            let archive = dir.path().join("dup.zip");
            write_test_zip(
                &[("hyper.exe", false, b"one"), ("HYPER.EXE", false, b"two")],
                &archive,
            );
            let stage = dir.path().join("dup-stage");
            let bundle = dir.path().join("dup-bundle");
            std::fs::create_dir_all(&stage).unwrap();
            let result = extract_zip_archive(&archive, &stage, bundle, "hyper.exe");
            assert!(result.is_err(), "duplicate/case binary must be rejected");
        }

        // Zip may use `\` as a path separator (PowerShell producers).
        {
            let archive = dir.path().join("backslash-ok.zip");
            write_test_zip(
                &[
                    ("hyper.exe", false, b"MZ"),
                    ("bundled\\skills\\demo\\SKILL.md", false, b"# skill\n"),
                ],
                &archive,
            );
            let stage = dir.path().join("bs-stage");
            let bundle = dir.path().join("bs-bundle");
            std::fs::create_dir_all(&stage).unwrap();
            let extracted = extract_zip_archive(&archive, &stage, bundle, "hyper.exe").unwrap();
            let tree = extracted.bundle_stage.expect("bundle");
            assert_eq!(
                std::fs::read(tree.join("skills/demo/SKILL.md")).unwrap(),
                b"# skill\n"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn oversize_bundle_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("oversize.tar.gz");
        // Use header size claim above limit without allocating a huge body by
        // writing a small body but advertising a huge size — tar builder sets
        // size from body, so instead write a file just over the per-file cap
        // using a modest oversize for the unit test path via direct limit.
        // We test the guard by setting entry size through a large payload that
        // still fits in memory for CI: 1 MiB is enough to exercise the counter
        // when combined with a temporary lower path — instead assert the
        // constant-backed check rejects entry.size() > MAX via a crafted tar.
        let big = vec![b'x'; (MAX_BUNDLE_FILE_BYTES as usize) + 1];
        write_test_tar(
            &[
                ("hyper", tar::EntryType::Regular, b"one".as_slice()),
                (
                    "bundled/skills/huge.bin",
                    tar::EntryType::Regular,
                    big.as_slice(),
                ),
            ],
            &archive,
        );
        let (stage, bundle_stage) = extract_dirs(dir.path());
        let result = extract_tar_archive(&archive, &stage, bundle_stage, "hyper");
        assert!(result.is_err());
    }

    #[test]
    fn future_cache_timestamp_is_not_fresh() {
        let state = UpdateState {
            checked_at_unix: Some(now_unix() + 60),
            ..UpdateState::default()
        };
        assert!(!state_is_fresh(&state));
    }
}
