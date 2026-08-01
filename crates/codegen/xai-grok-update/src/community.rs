//! Hyper community updater.
//!
//! This module is deliberately separate from the official Grok updater. It
//! only reads releases from `DaviRain-Su/hyper-grok-build` and only activates
//! binaries below `~/.hyper` (or `HYPER_SHARE_DIR` in debug/test builds).
//! Nothing here calls the x.ai/npm updater or writes `~/.grok/bin/grok`.

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
const MAX_ARCHIVE_ENTRIES: usize = 32;
const INSTALLER_NAME: &str = "community-github";

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

/// Removes a temporary artifact unless ownership was explicitly consumed.
struct TempArtifact {
    path: PathBuf,
    keep: bool,
}

impl TempArtifact {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn keep(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        if !self.keep {
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
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        bail!(
            "refusing to use symlinked Hyper {label}: {}",
            path.display()
        );
    }
    Ok(())
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

fn write_state_atomic(state: &UpdateState) -> Result<()> {
    ensure_safe_layout()?;
    let path = state_path();
    reject_symlink(&path, "update state")?;
    let tmp = unique_sibling(&path, "tmp");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let tmp_guard = TempArtifact::new(tmp.clone());
    serde_json::to_writer_pretty(&mut file, state)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);

    #[cfg(windows)]
    {
        let backup = unique_sibling(&path, "old");
        let had_old = path.exists();
        if had_old {
            std::fs::rename(&path, &backup).with_context(|| {
                format!("moving existing Hyper update state {}", path.display())
            })?;
        }
        if let Err(error) = std::fs::rename(&tmp, &path) {
            if had_old {
                let _ = std::fs::rename(&backup, &path);
            }
            return Err(error).with_context(|| format!("activating {}", path.display()));
        }
        let _ = std::fs::remove_file(backup);
    }
    #[cfg(not(windows))]
    std::fs::rename(&tmp, &path).with_context(|| format!("activating {}", path.display()))?;

    let _ = tmp_guard.keep();
    Ok(())
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

    Ok(Candidate {
        version: version.to_string(),
        asset_name,
        archive_url: archive_asset.browser_download_url.clone(),
        sha256,
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

fn normalized_root_entry(path: &Path) -> Result<Option<String>> {
    let raw = path.to_string_lossy();
    if raw.contains('\\') {
        bail!("archive entry uses a backslash path: {raw}");
    }
    let mut name: Option<String> = None;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) if name.is_none() => {
                name = Some(part.to_string_lossy().to_string());
            }
            Component::Normal(_) => bail!("archive entry is nested: {raw}"),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("archive entry escapes its root: {raw}")
            }
        }
    }
    Ok(name)
}

fn auxiliary_entry_allowed(name: &str) -> bool {
    matches!(
        name,
        "LICENSE" | "NOTICE" | "THIRD-PARTY-NOTICES" | "THIRD-PARTY-NOTICES.md"
    )
}

#[cfg(unix)]
fn extract_tar_binary(archive_path: &Path, destination: &Path, binary_entry: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let archive_file = File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let mut seen_names = HashSet::new();
    let mut found_binary = false;
    let mut entries = 0usize;

    for entry in archive.entries().context("reading Hyper tar archive")? {
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            bail!("Hyper archive contains too many entries");
        }
        let entry = entry.context("reading Hyper tar entry")?;
        let path = entry
            .path()
            .context("reading Hyper tar entry path")?
            .into_owned();
        let name = normalized_root_entry(&path)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() && name.is_none() {
            continue;
        }
        if !kind.is_file() {
            bail!(
                "Hyper archive contains a non-regular entry: {}",
                path.display()
            );
        }
        let Some(name) = name else {
            bail!("Hyper archive contains an unnamed regular entry");
        };
        if !seen_names.insert(name.clone()) {
            bail!("Hyper archive contains duplicate entry {name}");
        }
        if name == binary_entry {
            if found_binary {
                bail!("Hyper archive contains duplicate {binary_entry}");
            }
            if entry.size() > MAX_BINARY_BYTES {
                bail!("Hyper binary exceeds the decompressed size limit");
            }
            let mut out = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination)?;
            let copied = std::io::copy(&mut entry.take(MAX_BINARY_BYTES + 1), &mut out)?;
            if copied > MAX_BINARY_BYTES {
                bail!("Hyper binary exceeds the decompressed size limit");
            }
            out.sync_all()?;
            std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))?;
            found_binary = true;
        } else if auxiliary_entry_allowed(&name) {
            if entry.size() > MAX_AUXILIARY_BYTES {
                bail!("Hyper archive auxiliary entry {name} is too large");
            }
            // Drain to validate the compressed stream without unpacking it.
            let copied = std::io::copy(
                &mut entry.take(MAX_AUXILIARY_BYTES + 1),
                &mut std::io::sink(),
            )?;
            if copied > MAX_AUXILIARY_BYTES {
                bail!("Hyper archive auxiliary entry {name} is too large");
            }
        } else {
            bail!("Hyper archive contains unexpected entry {name}");
        }
    }
    if !found_binary {
        bail!("Hyper archive does not contain {binary_entry}");
    }
    Ok(())
}

#[cfg(windows)]
fn extract_zip_binary(archive_path: &Path, destination: &Path, binary_entry: &str) -> Result<()> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file).context("reading Hyper zip archive")?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("Hyper archive contains too many entries");
    }
    let mut seen_names = HashSet::new();
    let mut found_binary = false;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let path = Path::new(entry.name());
        let name = normalized_root_entry(path)?;
        if entry.is_dir() && name.is_none() {
            continue;
        }
        if entry.is_dir() {
            bail!(
                "Hyper archive contains an unexpected directory: {}",
                entry.name()
            );
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("Hyper archive contains a symlink: {}", entry.name());
        }
        let Some(name) = name else {
            bail!("Hyper archive contains an unnamed regular entry");
        };
        if !seen_names.insert(name.clone()) {
            bail!("Hyper archive contains duplicate entry {name}");
        }
        let max = if name == binary_entry {
            MAX_BINARY_BYTES
        } else if auxiliary_entry_allowed(&name) {
            MAX_AUXILIARY_BYTES
        } else {
            bail!("Hyper archive contains unexpected entry {name}");
        };
        if entry.size() > max {
            bail!("Hyper archive entry {name} exceeds the decompressed size limit");
        }
        if name == binary_entry {
            let mut out = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination)?;
            let copied = std::io::copy(&mut entry.take(max + 1), &mut out)?;
            if copied > max {
                bail!("Hyper binary exceeds the decompressed size limit");
            }
            out.sync_all()?;
            found_binary = true;
        } else {
            let copied = std::io::copy(&mut entry.take(max + 1), &mut std::io::sink())?;
            if copied > max {
                bail!("Hyper archive auxiliary entry {name} is too large");
            }
        }
    }
    if !found_binary {
        bail!("Hyper archive does not contain {binary_entry}");
    }
    Ok(())
}

async fn extract_binary(archive: &Path, destination: &Path, platform: Platform) -> Result<()> {
    let archive = archive.to_owned();
    let destination = destination.to_owned();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            extract_tar_binary(&archive, &destination, platform.binary_entry)
        }
        #[cfg(windows)]
        {
            extract_zip_binary(&archive, &destination, platform.binary_entry)
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

#[cfg(unix)]
fn activate_binary(versioned: &Path) -> Result<()> {
    let app = managed_application();
    let bin_dir = app.parent().context("Hyper application has no parent")?;
    let downloads = hyper_home().join("downloads");
    let name = versioned
        .file_name()
        .context("versioned Hyper binary has no filename")?;
    let relative = Path::new("..").join(
        downloads
            .file_name()
            .context("Hyper downloads directory has no filename")?,
    );
    let relative = relative.join(name);
    let tmp = unique_sibling(&app, "tmp-link");
    let tmp_guard = TempArtifact::new(tmp.clone());
    std::os::unix::fs::symlink(&relative, &tmp)?;
    std::fs::rename(&tmp, &app).with_context(|| {
        format!(
            "atomically activating Hyper at {} (bin dir {})",
            app.display(),
            bin_dir.display()
        )
    })?;
    let _ = tmp_guard.keep();
    Ok(())
}

#[cfg(windows)]
fn activate_binary(versioned: &Path) -> Result<()> {
    let app = managed_application();
    let staged = unique_sibling(&app, "new.exe");
    std::fs::copy(versioned, &staged)?;
    let staged_guard = TempArtifact::new(staged.clone());
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
        if had_old {
            let _ = std::fs::rename(&aside, &app);
        }
        return Err(error).context("activating downloaded Hyper executable");
    }
    let _ = staged_guard.keep();
    // A still-running old image may keep the aside locked. It is harmless and
    // can be removed by a later update after that process exits.
    let _ = std::fs::remove_file(aside);
    Ok(())
}

async fn install_candidate(candidate: &Candidate) -> Result<()> {
    ensure_safe_layout()?;
    let platform = platform()?;
    let downloads = hyper_home().join("downloads");
    let archive_tmp = unique_sibling(&downloads.join(&candidate.asset_name), "download");
    let archive_guard = TempArtifact::new(archive_tmp.clone());
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

    let stage = unique_sibling(&downloads.join("hyper-extracted"), "tmp");
    let stage_guard = TempArtifact::new(stage.clone());
    extract_binary(&archive_tmp, &stage, platform).await?;
    smoke_test(&stage).await?;

    let extension = if cfg!(windows) { ".exe" } else { "" };
    let binary_name = format!(
        "hyper-{}-{}-{}-sha256-{}{}",
        candidate.version, platform.local_os, platform.local_arch, candidate.sha256, extension
    );
    let versioned = downloads.join(&binary_name);
    publish_versioned_binary(&stage, &versioned)?;
    smoke_test(&versioned).await?;
    activate_binary(&versioned)?;

    let state = UpdateState {
        installed_version: Some(candidate.version.clone()),
        installed_asset: Some(candidate.asset_name.clone()),
        installed_sha256: Some(candidate.sha256.clone()),
        installed_binary: Some(binary_name),
        checked_at_unix: Some(now_unix()),
    };
    write_state_atomic(&state)?;
    drop(stage_guard);
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
            Ok(Some(command.spawn()?))
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
    fn archive_paths_are_root_only_and_never_escape() {
        assert_eq!(
            normalized_root_entry(Path::new("./hyper"))
                .unwrap()
                .as_deref(),
            Some("hyper")
        );
        assert!(normalized_root_entry(Path::new("../hyper")).is_err());
        assert!(normalized_root_entry(Path::new("nested/hyper")).is_err());
        assert!(normalized_root_entry(Path::new("nested\\hyper")).is_err());
    }

    #[cfg(unix)]
    fn write_test_tar(entries: &[(&str, tar::EntryType, &[u8])], path: &Path) {
        let file = File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, kind, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(0o755);
            if kind.is_symlink() {
                header.set_size(0);
                header.set_link_name("outside").unwrap();
            } else {
                header.set_size(body.len() as u64);
            }
            header.set_cksum();
            builder.append_data(&mut header, name, *body).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn strict_tar_extraction_rejects_links_and_duplicate_binary() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad.tar.gz");
        let destination = dir.path().join("hyper");
        write_test_tar(
            &[("hyper", tar::EntryType::Symlink, b"".as_slice())],
            &archive,
        );
        assert!(extract_tar_binary(&archive, &destination, "hyper").is_err());
        assert!(!destination.exists());

        std::fs::remove_file(&archive).unwrap();
        write_test_tar(
            &[
                ("hyper", tar::EntryType::Regular, b"one".as_slice()),
                ("hyper", tar::EntryType::Regular, b"two".as_slice()),
            ],
            &archive,
        );
        assert!(extract_tar_binary(&archive, &destination, "hyper").is_err());
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
