use anyhow::{Context, Result};
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::AsyncWriteExt;

use crate::version::{
    UpdateConfig, fetch_latest_version, get_installed_grok_version, get_latest_version,
    is_version_cache_fresh, try_fetch_stable_pointer, write_version_cache,
};
use xai_grok_shell::util::config;
use xai_grok_shell::util::grok_home::{grok_application, grok_home};

#[derive(Clone, Copy, Debug)]
pub enum UpdateRunMode {
    Blocking,
    NonBlocking,
}

const PROMPT_UPDATE_NOW: &str = "Update now? [Y/n/d]";
const MSG_AUTO_UPDATE_BACKGROUND: &str = "Auto-update running in background.";
const MSG_RUN_UPDATE_MANUAL: &str = "Run `grok update` to get the latest version.";
/// Manual-install one-liner for this platform's bootstrap installer.
fn manual_install_cmd() -> &'static str {
    if cfg!(windows) {
        "irm https://x.ai/cli/install.ps1 | iex"
    } else {
        "curl -fsSL https://x.ai/cli/install.sh | bash"
    }
}

/// Build a reinstall hint for a known installer type.
fn reinstall_hint(installer: &str) -> String {
    match installer {
        "npm" => "Please reinstall via npm:\n  npm i -g @xai-official/grok".to_string(),
        "gh-release" => "Please reinstall via GitHub Releases:\n  gh release download --repo xai-org-shared/grok-build --pattern 'grok-*' --output grok && chmod +x grok".to_string(),
        _ => format!("Please reinstall via:\n  {}", manual_install_cmd()),
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub installer: Option<String>,
    pub channel: String,
    pub auto_update: Option<bool>,
    pub error: Option<String>,
}

/// Format and print an [`UpdateStatus`] to stdout.
pub fn print_update_status(status: &UpdateStatus, json: bool) -> anyhow::Result<()> {
    if json {
        let payload = serde_json::to_string(status)?;
        println!("{payload}");
        return Ok(());
    }

    if let Some(error) = status.error.as_deref() {
        println!(
            "Grok Build - v{} [{}]",
            status.current_version, status.channel
        );
        println!("Update check failed: {error}");
        return Ok(());
    }

    let channel_label = format!(" [{}]", status.channel);

    if status.update_available {
        if let Some(latest_version) = status.latest_version.as_deref() {
            println!(
                "A new version of Grok Build is available: {} -> {}{}",
                status.current_version, latest_version, channel_label
            );
        } else {
            println!("A new version of Grok Build is available.");
        }
        return Ok(());
    }

    if let Some(latest_version) = status.latest_version.as_deref() {
        println!(
            "Grok Build - v{} (latest: {}){}",
            status.current_version, latest_version, channel_label
        );
        return Ok(());
    }

    println!("Grok Build - v{}{}", status.current_version, channel_label);
    Ok(())
}

pub async fn check_update_status(update_config: &UpdateConfig) -> UpdateStatus {
    let installer = get_installer().await.map(|value| value.to_string());
    let current_version = get_installed_grok_version();
    let current_config = config::load_config().await;
    let auto_update = current_config.cli.auto_update;
    let channel = update_config.channel.clone();

    let Some(ref inst) = installer else {
        return UpdateStatus {
            current_version,
            latest_version: None,
            update_available: false,
            installer,
            channel,
            auto_update,
            error: None,
        };
    };

    match get_latest_version(inst, update_config).await {
        // --check shares the updater's decision, so it never advertises a version
        // the policy would skip, clamp away, or can't satisfy.
        Ok(latest) => match plan_for(&config::VersionPolicy::resolve(), latest) {
            UpdatePlan::Install { target, .. } => {
                let mut error = None;
                let update_available = match needs_update(
                    &current_version,
                    &target,
                    &channel,
                    false,
                ) {
                    Some(value) => value,
                    None => {
                        // Distinguish parse failure from unsupported channel.
                        let parse_ok = semver::Version::parse(&current_version).is_ok()
                            && semver::Version::parse(&target).is_ok();
                        error = Some(if parse_ok {
                            format!(
                                "Unsupported release channel '{channel}' (current={current_version}, latest={target}). \
                                     Supported channels: stable, alpha, enterprise."
                            )
                        } else {
                            format!(
                                "Failed to parse versions (current={current_version}, latest={target})"
                            )
                        });
                        false
                    }
                };
                UpdateStatus {
                    current_version,
                    latest_version: Some(target),
                    update_available,
                    installer,
                    channel,
                    auto_update,
                    error,
                }
            }
            // Policy skips (anti-downgrade) or can't satisfy the floor: no upgrade.
            UpdatePlan::Skip { latest } | UpdatePlan::Unavailable { latest, .. } => UpdateStatus {
                current_version,
                latest_version: Some(latest),
                update_available: false,
                installer,
                channel,
                auto_update,
                error: None,
            },
        },
        Err(err) => UpdateStatus {
            current_version,
            latest_version: None,
            update_available: false,
            installer,
            channel,
            auto_update,
            error: Some(err.to_string()),
        },
    }
}

enum UpdatePlan {
    /// Anti-downgrade skip; `latest` is reported to the user.
    Skip {
        latest: String,
    },
    /// A hard `required_minimum` exceeds the latest release, so nothing satisfies it.
    Unavailable {
        latest: String,
        target: String,
    },
    Install {
        latest: String,
        target: String,
    },
}

/// Classify a fetched `latest` release under `policy`. Pure; `fetch_update_plan`
/// is the IO wrapper. `--check` shares this so it can't diverge from the updater.
fn plan_for(policy: &config::VersionPolicy, latest: String) -> UpdatePlan {
    let Some(target) = policy.resolve_target(&latest) else {
        return UpdatePlan::Skip { latest };
    };
    // A hard `required_minimum` can clamp above the latest release; that version
    // doesn't exist.
    if matches!(
        (semver::Version::parse(&target), semver::Version::parse(&latest)),
        (Ok(t), Ok(l)) if t > l
    ) {
        UpdatePlan::Unavailable { latest, target }
    } else {
        UpdatePlan::Install { latest, target }
    }
}

async fn fetch_update_plan(
    installer: &str,
    update_config: &UpdateConfig,
    policy: &config::VersionPolicy,
) -> Result<UpdatePlan> {
    let latest = fetch_latest_version(installer, update_config).await?;
    Ok(plan_for(policy, latest))
}

/// Installer + version the leader/background path should converge to: an
/// upgrade OR an authoritative-installer rollback. `None` means stay put. Gates
/// on the installer (via `installer_allows_downgrade`) so npm is never
/// downgraded — the decision depends on the installer, never the caller.
pub async fn auto_update_target(update_config: &UpdateConfig) -> Option<(&'static str, String)> {
    let installer = get_installer().await?;
    let current = get_installed_grok_version();
    let policy = config::VersionPolicy::resolve();
    let UpdatePlan::Install { target, .. } = fetch_update_plan(installer, update_config, &policy)
        .await
        .ok()?
    else {
        return None;
    };
    needs_update(
        &current,
        &target,
        &update_config.channel,
        installer_allows_downgrade(installer),
    )
    .unwrap_or(false)
    .then_some((installer, target))
}

/// Outcome of [`ensure_latest_on_disk`].
#[derive(Debug)]
pub struct EnsureLatestOutcome {
    /// Version this call downloaded and installed; `None` when the disk was
    /// already current (or there was no installer).
    pub installed: Option<String>,
    /// The running process differs from what is now on disk in the channel's
    /// update direction — the caller should relaunch onto the on-disk binary.
    pub relaunch_needed: bool,
}

/// One leader auto-update pass: converge the on-disk install to the channel
/// pointer (downloading **only** when the disk is actually behind it), then
/// report whether the running process should relaunch onto the on-disk binary.
///
/// Unlike [`run_update`] this never uses the compiled-in version for the
/// download decision — a binary already installed by another process (TUI
/// background download, explicit `grok update`) is reused as-is. This both
/// removes the duplicate download in leader mode and stops the pre-fix
/// hourly re-download while a busy leader keeps deferring its relaunch.
///
/// When the disk version is unknowable ([`disk_version_for_installer`]:
/// npm-managed installs, Windows copy-based installs, dev builds), this
/// degrades to the pre-fix behavior — download when the *running* process is
/// stale, relaunch only after a download this pass actually installed
/// something. Note the Windows consequence: the hourly busy-leader
/// re-download is NOT fixed there; only the symlink layout can prove the
/// disk is current without exec'ing the binary.
pub async fn ensure_latest_on_disk(update_config: &UpdateConfig) -> Result<EnsureLatestOutcome> {
    let mut outcome = EnsureLatestOutcome {
        installed: None,
        relaunch_needed: false,
    };
    let Some(installer) = get_installer().await else {
        return Ok(outcome);
    };
    heal_managed_install(installer).await;
    let allow_downgrade = installer_allows_downgrade(installer);
    let policy = config::VersionPolicy::resolve();
    let UpdatePlan::Install { target, .. } =
        fetch_update_plan(installer, update_config, &policy).await?
    else {
        return Ok(outcome);
    };

    let effective_current =
        disk_version_for_installer(installer).unwrap_or_else(get_installed_grok_version);
    if needs_update(
        &effective_current,
        &target,
        &update_config.channel,
        allow_downgrade,
    )
    .unwrap_or(false)
    {
        run_install_script(installer, Some(&target), update_config).await?;
        outcome.installed = Some(target.clone());
    }

    // Relaunch when the running binary differs from what's on disk in the
    // channel's update direction — covers binaries installed by other
    // processes, not just the install above.
    let running = get_installed_grok_version();
    if let Some(disk_now) =
        disk_version_for_installer(installer).or_else(|| outcome.installed.clone())
    {
        outcome.relaunch_needed =
            needs_update(&running, &disk_now, &update_config.channel, allow_downgrade)
                .unwrap_or(false);
    }
    Ok(outcome)
}

/// Disk-version probe gated on the installer actually maintaining the
/// managed `~/.grok/bin/grok` symlink.
///
/// Only the internal (install.sh / CDN) and gh-release installers write that
/// symlink. npm manages its own global install, so for npm a symlink left
/// over from a previous internal install would LIE about the npm install's
/// version — in the worst direction, a leftover symlink "newer" than the npm
/// registry would make every updater report "already up to date" and
/// silently suppress npm updates forever. Unknown installers are treated
/// like npm (no trustworthy disk version).
fn disk_version_for_installer(installer: &str) -> Option<String> {
    match installer {
        "internal" | "gh-release" => crate::version::installed_on_disk_version(),
        _ => None,
    }
}

fn env_installer() -> Option<&'static str> {
    if let Ok(v) = std::env::var("GROK_INSTALLER") {
        return match v.to_ascii_lowercase().as_str() {
            "npm" => Some("npm"),
            "internal" => Some("internal"),
            "gh-release" | "gh" => Some("gh-release"),
            _ => None,
        };
    }
    if std::env::var_os("GROK_MANAGED_BY_NPM").is_some() {
        return Some("npm");
    }
    if std::env::var_os("GROK_MANAGED_BY_INTERNAL").is_some() {
        return Some("internal");
    }
    if std::env::var_os("npm_config_user_agent").is_some() {
        return Some("npm");
    }
    None
}

pub async fn get_installer() -> Option<&'static str> {
    if let Some(i) = env_installer() {
        return Some(i);
    }
    let cfg = config::load_config().await;
    match cfg.cli.installer.as_deref() {
        Some("npm") => Some("npm"),
        Some("gh-release") => Some("gh-release"),
        _ => Some("internal"),
    }
}

fn needs_update(current: &str, target: &str, channel: &str, allow_downgrade: bool) -> Option<bool> {
    let current = semver::Version::parse(current).ok()?;
    let target = semver::Version::parse(target).ok()?;
    match channel {
        // NOTE: With the 0.2.X versioning scheme, all versions are plain
        // semver (no pre-release suffix). The pre-release checks in this
        // match are dead code but kept as a safety net.
        "stable" | "enterprise" => {
            if !target.pre.is_empty() {
                tracing::warn!(
                    %current, %target,
                    channel = %channel,
                    "stable/enterprise channel received pre-release candidate, rejecting"
                );
                return Some(false);
            }
            if !current.pre.is_empty() {
                return Some(true);
            }
        }
        "alpha" => {}
        _ => return None,
    }
    Some(if allow_downgrade {
        target != current
    } else {
        target > current
    })
}

/// Returns `true` for installer backends whose version source is authoritative
/// (managed by xAI directly), meaning a pointer rollback is intentional and
/// should trigger a client downgrade. Returns `false` for backends like npm
/// where stale corporate registries/proxies can return arbitrarily old versions.
///
/// Users who installed via `install.sh` are classified as `"internal"` by
/// `get_installer()`, so they also get rollback support.
fn installer_allows_downgrade(installer: &str) -> bool {
    match installer {
        "internal" | "gh-release" => true,
        "npm" => false,
        _ => false,
    }
}

/// Result of a background update availability check.
#[derive(Debug, Clone)]
pub struct UpdateAvailable {
    /// The latest version string (e.g. "0.1.200").
    pub latest_version: String,
}

/// Outcome of [`check_update_background`].
pub struct BackgroundUpdateCheck {
    /// `Some` when the *running* binary is older than the channel pointer —
    /// drives the in-TUI restart hint regardless of who downloads the binary.
    pub update: Option<UpdateAvailable>,
    /// Handle to the background `grok update` child, `Some` only when a
    /// download was actually started (the on-disk install was behind the
    /// pointer). The TUI parks this and `wait()`s on it at quit-for-update
    /// time instead of spawning a second downloader.
    pub download: Option<tokio::process::Child>,
}

impl BackgroundUpdateCheck {
    fn none() -> Self {
        Self {
            update: None,
            download: None,
        }
    }
}

/// Check for available updates without blocking the TUI startup.
///
/// Sets [`BackgroundUpdateCheck::update`] when the running binary is older
/// than the channel pointer. If `auto_update` is enabled **and the on-disk
/// install is also behind the pointer**, kicks off a non-blocking download
/// (spawns `grok update` as a detached child process) so the new binary is
/// ready when the user quits and relaunches. When another process (an earlier
/// TUI, the leader's hourly checker) already put the target version on disk,
/// no download is started — only the restart hint is surfaced.
pub async fn check_update_background(update_config: &UpdateConfig) -> BackgroundUpdateCheck {
    let Some(installer) = get_installer().await else {
        return BackgroundUpdateCheck::none();
    };

    heal_managed_install(installer).await;

    if is_version_cache_fresh().await {
        return BackgroundUpdateCheck::none();
    }

    let current_config = config::load_config().await;
    if current_config.cli.auto_update == Some(false) {
        return BackgroundUpdateCheck::none();
    }

    let current_version = get_installed_grok_version();
    let policy = config::VersionPolicy::resolve();
    let target_version = match fetch_update_plan(installer, update_config, &policy).await {
        Ok(UpdatePlan::Install { target, .. }) => target,
        Ok(UpdatePlan::Skip { .. } | UpdatePlan::Unavailable { .. }) | Err(_) => {
            return BackgroundUpdateCheck::none();
        }
    };

    let allow_downgrade = installer_allows_downgrade(installer);
    if !needs_update(
        &current_version,
        &target_version,
        &update_config.channel,
        allow_downgrade,
    )
    .unwrap_or(false)
    {
        let stable_ptr = try_fetch_stable_pointer().await;
        write_version_cache(&target_version, stable_ptr.as_deref()).await;
        return BackgroundUpdateCheck::none();
    }

    // Only download when the on-disk install is behind the pointer; the
    // running process being stale (checked above) just means "show the
    // restart hint". The quit-for-update path's `grok update` child resolves
    // to "Already up to date" against the same disk state. Gated on the
    // installer maintaining the managed symlink — for npm a leftover symlink
    // would wrongly suppress the download (see `disk_version_for_installer`).
    let disk_needs_download = match disk_version_for_installer(installer) {
        Some(disk) => needs_update(
            &disk,
            &target_version,
            &update_config.channel,
            allow_downgrade,
        )
        .unwrap_or(true),
        None => true,
    };

    // Kick off a non-blocking download so the binary is ready when the
    // user restarts (or accepts the in-TUI restart prompt).
    let download = if disk_needs_download {
        match run_update_subcommand(UpdateRunMode::NonBlocking).await {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!("Background update download failed to start: {e}");
                None
            }
        }
    } else {
        tracing::info!(
            target_version = %target_version,
            "Background update: target already on disk, skipping download"
        );
        None
    };

    BackgroundUpdateCheck {
        update: Some(UpdateAvailable {
            latest_version: target_version,
        }),
        download,
    }
}

/// Returns Ok(true) if a blocking update ran; otherwise Ok(false).
pub async fn run_update_if_available(
    run_mode: UpdateRunMode,
    interactive: bool,
    update_config: &UpdateConfig,
) -> Result<bool> {
    let Some(inst) = get_installer().await else {
        // Skip update check if no known installer.
        return Ok(false);
    };

    heal_managed_install(inst).await;

    if is_version_cache_fresh().await {
        return Ok(false);
    }

    let current_config = config::load_config().await;

    // Skip update check if auto-update is explicitly disabled.
    if current_config.cli.auto_update == Some(false) {
        return Ok(false);
    }

    // Resolve effective auto_update: None defaults to true (first-run).
    let auto_update = current_config.cli.auto_update.unwrap_or(true);

    if current_config.cli.auto_update.is_none()
        && let Err(e) = config::update_config(|st| {
            if st.cli.auto_update.is_none() {
                st.cli.auto_update = Some(true);
            }
        })
        .await
    {
        tracing::warn!("Failed to save auto-update setting: {}", e);
    }

    let current_version = get_installed_grok_version();
    let policy = config::VersionPolicy::resolve();
    // Don't write version.json here; only cache after confirming no update is
    // needed or after a successful install, so a failed background download
    // doesn't suppress retries for the TTL window.
    let latest_version = match fetch_update_plan(inst, update_config, &policy).await {
        Ok(UpdatePlan::Install { target, .. }) => target,
        Ok(UpdatePlan::Skip { .. } | UpdatePlan::Unavailable { .. }) | Err(_) => return Ok(false),
    };
    if !needs_update(
        &current_version,
        &latest_version,
        &update_config.channel,
        installer_allows_downgrade(inst),
    )
    .unwrap_or(false)
    {
        let stable_ptr = try_fetch_stable_pointer().await;
        write_version_cache(&latest_version, stable_ptr.as_deref()).await;
        return Ok(false);
    }

    let channel_label = format!(" [{}]", update_config.channel);
    if auto_update {
        eprintln!(
            "A new version of Grok Build is available: {} -> {}{}",
            current_version, latest_version, channel_label
        );
        if interactive {
            if let Err(e) = run_update_subcommand(run_mode).await {
                eprintln!("Update failed: {}", e);
            } else if matches!(run_mode, UpdateRunMode::Blocking) {
                return Ok(true);
            } else {
                eprintln!("{}", MSG_AUTO_UPDATE_BACKGROUND);
                return Ok(false);
            }
        } else if let Err(e) = run_update_subcommand(run_mode).await {
            eprintln!("Update failed: {}", e);
        } else if matches!(run_mode, UpdateRunMode::Blocking) {
            return Ok(true);
        }
        return Ok(false);
    } else {
        if current_config
            .cli
            .dismissed_version
            .as_deref()
            .is_some_and(|v| v == latest_version)
        {
            return Ok(false);
        }
        eprintln!(
            "A new version of Grok Build is available: {} -> {}{}",
            current_version, latest_version, channel_label
        );
        if interactive {
            eprintln!("{}", PROMPT_UPDATE_NOW);
            let mut line = String::new();
            if io::stdin().read_line(&mut line).is_ok() {
                let ans = line.trim().to_ascii_lowercase();
                if ans.is_empty() || ans == "y" || ans == "yes" {
                    if let Err(e) = run_update_subcommand(run_mode).await {
                        eprintln!("Update failed: {}", e);
                    } else if matches!(run_mode, UpdateRunMode::Blocking) {
                        return Ok(true);
                    } else {
                        eprintln!("{}", MSG_AUTO_UPDATE_BACKGROUND);
                        return Ok(false);
                    }
                } else if ans == "d" || ans == "dismiss" {
                    let dismissed = latest_version.clone();
                    if let Err(e) = config::update_config(|st| {
                        st.cli.dismissed_version = Some(dismissed);
                    })
                    .await
                    {
                        tracing::warn!("Failed to save dismissed version: {}", e);
                    }
                }
            }
        } else {
            eprintln!("{}", MSG_RUN_UPDATE_MANUAL);
        }
    }
    Ok(false)
}

/// Launch "grok update" in blocking or non-blocking mode.
///
/// In `NonBlocking` mode the spawned child's handle is returned so the caller
/// can later `wait()` on the in-flight download (e.g. the TUI's
/// quit-for-update path) instead of blind-spawning a second downloader.
/// Dropping the handle does not kill the child (`kill_on_drop` is off), so
/// callers that don't care can ignore it. `Blocking` mode returns `None`.
async fn run_update_subcommand(run_mode: UpdateRunMode) -> Result<Option<tokio::process::Child>> {
    let exe = std::env::current_exe()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("update");
    match run_mode {
        UpdateRunMode::Blocking => {
            // stderr must be null, not piped: `.status()` does not drain
            // pipes, so if the child writes more than the OS pipe buffer
            // (~16 KB macOS / ~64 KB Linux) to stderr (e.g. download
            // progress bars), the child blocks on the write while the
            // parent blocks on waitpid — deadlocking both processes.
            // With `panic = "abort"`, the blocked child eventually
            // receives SIGABRT.
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                // inherit, not piped: the TUI is already restored so the
                // parent's stderr fd is a normal terminal. inherit lets
                // the child's diagnostic output reach the user. piped +
                // status() would immediately close the read end → EPIPE
                // → panic → SIGABRT (signal 6) under panic=abort.
                .stderr(Stdio::inherit());
            // No detach: the child must stay in the foreground process group so Ctrl+C cancels it with the parent; the atomic install protocol makes mid-download kills safe.
            let status = cmd.status().await?;
            if !status.success() {
                anyhow::bail!("grok update failed with {}", status);
            }
            Ok(None)
        }
        UpdateRunMode::NonBlocking => {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // Detach = new session (Ctrl+C isolation), not handle abandonment:
            // the child is still ours to wait() on.
            xai_grok_tools::util::detach_command(&mut cmd);
            let child = cmd.spawn()?;
            Ok(Some(child))
        }
    }
}

/// Resolve the grok binary path for re-execution after an update.
///
/// `current_exe()` resolves symlinks via `/proc/self/exe` (see proc(5)),
/// so it returns the old versioned target after a symlink swap.
/// Prefer `~/.grok/bin/grok` which always points to the latest version.
fn resolve_restart_exe() -> Result<std::path::PathBuf> {
    let canonical = grok_application();
    if canonical.exists() {
        return Ok(canonical);
    }
    Ok(std::env::current_exe()?)
}

/// Restart grok with the original command-line arguments to pick up the update.
pub fn restart_grok() -> Result<()> {
    let exe = resolve_restart_exe()?;
    let mut cmd = Command::new(exe);
    for arg in std::env::args_os().skip(1) {
        cmd.arg(arg);
    }
    cmd.env_clear();
    cmd.envs(std::env::vars_os().filter(|(k, _)| k != "GROK_AUTO_UPDATE"));
    eprintln!("Restarting Grok...");

    // Use exec on Unix to replace the current process, avoiding stdio issues
    // when the parent exits. On Windows, fall back to spawn + exit.
    #[cfg(unix)]
    {
        // Flush output before exec to ensure messages are visible
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        let err = cmd.exec();
        // exec only returns if there was an error
        anyhow::bail!("Failed to exec: {}", err);
    }

    #[cfg(not(unix))]
    {
        // Flush output before exit to ensure messages are visible
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let _ = cmd.spawn()?;
        std::process::exit(0);
    }
}

pub async fn run_install_script(
    installer: &str,
    target: Option<&str>,
    update_config: &UpdateConfig,
) -> Result<()> {
    let result = match installer {
        "npm" => install_npm(
            target,
            &update_config.channel,
            update_config.npm_registry.as_deref(),
        ),
        "gh-release" => install_gh_release(target).await,
        _ => install_internal(target, update_config).await,
    };
    if result.is_ok() {
        remove_stale_models_cache().await;
    }
    result.map_err(|e| {
        anyhow::anyhow!(
            "Auto-update failed: {:#}\n\n{}",
            e,
            reinstall_hint(installer)
        )
    })
}

/// Detect the current platform (os, arch) for binary downloads.
pub(crate) fn detect_platform() -> Result<(&'static str, &'static str)> {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        anyhow::bail!("Unsupported OS");
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        anyhow::bail!("Unsupported architecture");
    };
    Ok((os, arch))
}

/// Age past which a leftover `.tmp` download file (or a freshly-renamed
/// versioned binary) is considered abandoned (crashed/killed updater) and
/// safe for `cleanup_old_downloads` to sweep. Generous compared to the
/// longest plausible download (per-request budget is
/// [`DOWNLOAD_REQUEST_TIMEOUT`]; the leader check+download pass matches) so
/// a concurrent updater's in-flight or just-landed file is never deleted
/// out from under it.
const STALE_TMP_AGE: Duration = Duration::from_secs(60 * 60);

/// Total timeout for a CLI artifact download request (including body).
/// Previously 5 minutes, which was too tight on slow links and caused the
/// transfer to abort and restart from zero repeatedly.
const DOWNLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Unique temp path for an in-flight download of `dest`.
///
/// Appends `.{pid}-{seq}.tmp` to the FULL file name instead of using
/// `Path::with_extension`, which treats everything after the last dot of the
/// versioned name as the extension (`grok-0.1.181-linux-x86_64` →
/// `grok-0.1.tmp`) and therefore collides for every `0.1.x` version. The PID
/// plus a per-process counter makes the name unique per download attempt —
/// across processes (two updaters racing in the same instant, the accepted
/// lock-free residual race) and within one process — so no racer can ever
/// rename another's half-written temp file into place. Leftovers older than
/// [`STALE_TMP_AGE`] are swept by `cleanup_old_downloads`.
fn tmp_download_path(dest: &std::path::Path) -> std::path::PathBuf {
    unique_temp_sibling(dest, "tmp")
}

/// Unique temp path `<base>.{pid}-{seq}.{ext}`, appended to the full name so a
/// versioned base like `grok-0.1.181` doesn't collide via `with_extension`.
/// PID + per-process counter keep racing updaters from clobbering each other.
fn unique_temp_sibling(base: &std::path::Path, ext: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut name = base
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(
        ".{}-{}.{ext}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    base.with_file_name(name)
}

/// Set `+x` on the temp file before renaming onto `dest`, so a concurrent
/// same-version installer never execs `dest` while it is still 0644.
async fn publish_downloaded_artifact(tmp: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755)).await?;
    }
    tokio::fs::rename(tmp, dest).await?;
    Ok(())
}

/// Files smaller than this are not worth fragmenting across parallel chunks.
const PARALLEL_DOWNLOAD_MIN_BYTES: u64 = 16 * 1024 * 1024;

/// Pick chunk count from file size: 1 chunk per 16 MiB, capped at 8.
fn parallel_chunk_count(size: u64) -> u64 {
    let size_mb = size / (1024 * 1024);
    (size_mb / 16).clamp(1, 8)
}

/// Try a parallel byte-range download to `dest`. Returns Err if the server
/// doesn't advertise a Content-Length, the file is too small to be worth
/// splitting, the range request is rejected, or any chunk transfer fails.
/// The caller is expected to fall back to a single-connection download on Err.
async fn try_parallel_download(
    url: &str,
    dest: &std::path::Path,
    with_progress: bool,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_REQUEST_TIMEOUT)
        .build()?;

    let head = client.head(url).send().await?;
    if !head.status().is_success() {
        anyhow::bail!("HEAD failed: HTTP {}", head.status());
    }
    let size = head
        .content_length()
        .ok_or_else(|| anyhow::anyhow!("response missing Content-Length"))?;
    if size < PARALLEL_DOWNLOAD_MIN_BYTES {
        anyhow::bail!("file too small for parallel download ({} bytes)", size);
    }

    let n_chunks = parallel_chunk_count(size);
    if n_chunks < 2 {
        anyhow::bail!(
            "file size yields {} chunk(s); not worth parallelizing",
            n_chunks
        );
    }
    let chunk_size = size.div_ceil(n_chunks);

    let pb = if with_progress {
        let pb = ProgressBar::new(size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("  {bar:30.cyan/dim} {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("━╸─"),
        );
        Some(pb)
    } else {
        None
    };

    let tmp = tmp_download_path(dest);
    // Pre-allocate so each task can seek+write to its own range concurrently.
    // One blocking-pool hop instead of two per tokio::fs call.
    let tmp_for_alloc = tmp.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let f = std::fs::File::create(&tmp_for_alloc)?;
        f.set_len(size)?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("blocking pre-allocate task panicked: {e}"))??;

    let tasks = (0..n_chunks).map(|i| {
        let start = i * chunk_size;
        let end = std::cmp::min(start + chunk_size, size) - 1;
        let url = url.to_string();
        let tmp = tmp.clone();
        let client = client.clone();
        let pb = pb.clone();
        async move { download_range(&client, &url, &tmp, start, end, pb.as_ref()).await }
    });
    let result = futures::future::try_join_all(tasks).await;

    if let Some(pb) = &pb {
        pb.finish_and_clear();
    }

    match result {
        Ok(_) => {
            publish_downloaded_artifact(&tmp, dest).await?;
            Ok(())
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

/// Fetch bytes `[start, end]` (inclusive) of `url` and write them at `start`
/// in `dest`. Errors if the server doesn't return `206 Partial Content`.
///
/// Streams from the network into a `Vec<u8>` (so progress ticks smoothly as
/// bytes arrive), then issues a single `spawn_blocking` per chunk to do the
/// open + seek + write_all in `std::fs`. This avoids the per-write hop into
/// tokio's blocking pool that `tokio::fs::File::write_all` performs on every
/// ~8 KiB Bytes item from `bytes_stream()`.
async fn download_range(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
    start: u64,
    end: u64,
    progress: Option<&ProgressBar>,
) -> Result<()> {
    let resp = client
        .get(url)
        .header("Range", format!("bytes={}-{}", start, end))
        .send()
        .await?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        anyhow::bail!("range request rejected: HTTP {}", resp.status());
    }
    let mut buf = Vec::with_capacity((end - start + 1) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if let Some(pb) = progress {
            pb.inc(chunk.len() as u64);
        }
        buf.extend_from_slice(&chunk);
    }
    let dest = dest.to_owned();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&dest)?;
        f.seek(SeekFrom::Start(start))?;
        f.write_all(&buf)?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("blocking write task panicked: {e}"))??;
    Ok(())
}

/// Download a file from `url` to `dest` with a terminal progress bar.
///
/// If the server provides a `Content-Length` header, a determinate bar is shown
/// with bytes downloaded, total size, and ETA. Otherwise a spinner with a byte
/// counter is used as a fallback.
#[doc(hidden)]
pub async fn download_with_progress(url: &str, dest: &std::path::Path) -> Result<()> {
    // Try parallel byte-range first. Falls through to single-connection on any
    // failure (HEAD missing Content-Length, ranges rejected, partial-fetch error).
    match try_parallel_download(url, dest, true).await {
        Ok(()) => return Ok(()),
        Err(e) => {
            tracing::debug!("parallel download failed, falling back to single connection: {e}")
        }
    }

    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_REQUEST_TIMEOUT)
        .build()?;
    let resp = client.get(url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Download failed: HTTP {}", resp.status());
    }

    let total_size = resp.content_length();

    let pb = if let Some(size) = total_size {
        let pb = ProgressBar::new(size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("  {bar:30.cyan/dim} {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("━╸─"),
        );
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("  {spinner:.cyan} {bytes} downloaded")
                .unwrap(),
        );
        pb.enable_steady_tick(Duration::from_millis(100));
        pb
    };

    // Stream to a temp file, then rename atomically
    let tmp = tmp_download_path(dest);
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        pb.inc(chunk.len() as u64);
    }
    file.flush().await?;
    drop(file);

    pb.finish_and_clear();

    publish_downloaded_artifact(&tmp, dest).await?;
    Ok(())
}

/// Download a file silently (no progress bar).
#[doc(hidden)]
pub async fn download_silent(url: &str, dest: &std::path::Path) -> Result<()> {
    match try_parallel_download(url, dest, false).await {
        Ok(()) => return Ok(()),
        Err(e) => {
            tracing::debug!("parallel download failed, falling back to single connection: {e}")
        }
    }

    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_REQUEST_TIMEOUT)
        .build()?;
    let resp = client.get(url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Download failed: HTTP {}", resp.status());
    }

    let tmp = tmp_download_path(dest);
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);

    publish_downloaded_artifact(&tmp, dest).await?;
    Ok(())
}

/// Delete `~/.grok/models_cache.json` after a successful update.
///
/// The cache embeds the binary version and will be treated as a miss by the
/// new binary anyway, but removing it eagerly avoids a wasted disk read +
/// deserialize on first launch.
async fn remove_stale_models_cache() {
    let cache = grok_home().join("models_cache.json");
    match tokio::fs::remove_file(&cache).await {
        Ok(()) => tracing::debug!("removed stale models_cache.json after update"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::debug!("failed to remove stale models cache: {e}"),
    }
}

/// Remove the stale `grok-pager` symlink/binary from `~/.grok/bin/` left by
/// older installations that shipped a separate pager binary.
async fn remove_stale_pager(bin_dir: &std::path::Path) {
    let name = if cfg!(windows) {
        "grok-pager.exe"
    } else {
        "grok-pager"
    };
    let link = bin_dir.join(name);
    if link.exists() || link.is_symlink() {
        let _ = tokio::fs::remove_file(&link).await;
    }
}

/// Fetch a CLI object from GCS. On Windows the public bucket may use a `.exe`
/// suffix; try that first, then the extensionless name used on macOS/Linux.
async fn download_cli_artifact_from_gcs(
    gcs_base_url: &str,
    object_name: &str,
    dest: &std::path::Path,
    with_progress: bool,
) -> Result<()> {
    let base = gcs_base_url.trim_end_matches('/');
    #[cfg(windows)]
    {
        let with_exe = format!("{}/{}.exe", base, object_name);
        let r = if with_progress {
            download_with_progress(&with_exe, dest).await
        } else {
            download_silent(&with_exe, dest).await
        };
        match r {
            Ok(()) => return Ok(()),
            Err(e) => tracing::debug!("{with_exe} not found, trying extensionless: {e}"),
        }
    }
    let url = format!("{}/{}", base, object_name);
    if with_progress {
        download_with_progress(&url, dest).await
    } else {
        download_silent(&url, dest).await
    }
}

async fn install_internal(target: Option<&str>, update_config: &UpdateConfig) -> Result<()> {
    install_internal_from_bases(target, update_config, crate::version::CLI_BASE_URLS).await
}

/// Try the base-dependent install phase ([`download_verified_from_base`]:
/// version resolution, download, smoke test) against each base URL in turn,
/// falling through to the next on any failure. Used to keep installs working
/// when the primary CDN endpoint (Cloudflare) is unreachable but the fallback
/// (direct GCS) still resolves.
///
/// Download-phase side effects (download dir creation, binary fetch) are
/// idempotent, so retrying with a different base after a partial failure is
/// safe. Local activation ([`activate_verified_download`]: link swap,
/// cleanup, config persist) runs once after the first successful download —
/// its failures are not base-dependent, so they abort the install instead of
/// triggering a pointless re-download from the next base.
#[doc(hidden)]
pub async fn install_internal_from_bases(
    target: Option<&str>,
    update_config: &UpdateConfig,
    bases: &[&str],
) -> Result<()> {
    let mut last_err: Option<anyhow::Error> = None;
    for (i, base) in bases.iter().enumerate() {
        match download_verified_from_base(target, update_config, base).await {
            Ok(download) => return activate_verified_download(&download).await,
            Err(e) => {
                if i + 1 < bases.len() {
                    tracing::warn!(
                        "install via {} failed ({:#}); trying next base URL",
                        base,
                        e
                    );
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no CLI base URLs to try")))
}

const SMOKE_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn smoke_test_binary(binary_path: &std::path::Path) -> bool {
    let mut cmd = tokio::process::Command::new(binary_path);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    xai_grok_tools::util::detach_command(&mut cmd);
    match tokio::time::timeout(SMOKE_TEST_TIMEOUT, cmd.status()).await {
        Ok(Ok(status)) => status.success(),
        _ => false,
    }
}

/// Test-only entry point: same as [`install_internal`] but reads from
/// `gcs_base_url` instead of the hardcoded GCS bucket. Persists installer
/// config and writes to `~/.grok/bin/`, so callers must isolate
/// `GROK_HOME`.
#[doc(hidden)]
pub async fn install_internal_from_base(
    target: Option<&str>,
    update_config: &UpdateConfig,
    gcs_base_url: &str,
) -> Result<()> {
    let download = download_verified_from_base(target, update_config, gcs_base_url).await?;
    activate_verified_download(&download).await
}

/// A downloaded and smoke-tested binary in `~/.grok/downloads/`, not yet
/// activated as the managed `grok`/`agent`.
struct VerifiedDownload {
    version: String,
    binary_path: std::path::PathBuf,
}

/// Base-dependent install phase: resolve the version (per base when no
/// target is pinned), download the binary, and smoke-test it. Failures here
/// are worth retrying against another base URL.
async fn download_verified_from_base(
    target: Option<&str>,
    update_config: &UpdateConfig,
    gcs_base_url: &str,
) -> Result<VerifiedDownload> {
    let (os, arch) = detect_platform()?;
    let platform = format!("{}-{}", os, arch);

    let version = match target {
        Some(v) => {
            semver::Version::parse(v)
                .map_err(|_| anyhow::anyhow!("invalid version format: '{}'", v))?;
            v.to_string()
        }
        None => {
            crate::version::fetch_gcs_version_from_base(&update_config.channel, gcs_base_url)
                .await?
        }
    };

    let grok_home = grok_home();
    let download_dir = grok_home.join("downloads");
    tokio::fs::create_dir_all(&download_dir).await?;

    let binary_name = format!("grok-{}-{}", version, platform);
    let binary_path = download_dir.join(&binary_name);

    eprintln!("  Downloading grok v{} ({})...", version, platform);

    // Published already +x (see `publish_downloaded_artifact`).
    download_cli_artifact_from_gcs(gcs_base_url, &binary_name, &binary_path, true).await?;

    // Smoke-test: run the binary before activating it. A truncated or
    // corrupt download is caught here and never becomes the active grok.
    if !smoke_test_binary(&binary_path).await {
        let _ = tokio::fs::remove_file(&binary_path).await;
        // No prefix: run_install_script's wrap adds "Auto-update failed:".
        anyhow::bail!(
            "downloaded binary failed to run.\n\
             Your current version is unchanged.\n\
             To update manually: {}",
            manual_install_cmd()
        );
    }

    Ok(VerifiedDownload {
        version,
        binary_path,
    })
}

/// Local activation phase: swap the managed bin links to the downloaded
/// binary and finish bookkeeping. Nothing here depends on which base URL
/// served the download, so callers must not retry another base on failure.
async fn activate_verified_download(download: &VerifiedDownload) -> Result<()> {
    let grok_home = grok_home();
    let download_dir = grok_home.join("downloads");
    let bin_dir = grok_home.join("bin");
    tokio::fs::create_dir_all(&bin_dir).await?;

    // Atomic swap of ~/.grok/bin/{grok,agent} -> downloaded binary.
    let link_path = swap_managed_bin_links(&download.binary_path, &bin_dir).await?;

    remove_stale_pager(&bin_dir).await;

    eprintln!();

    // Clean up old versioned binaries (keeps current + 1 previous).
    cleanup_old_downloads(&download_dir, "grok", &download.version).await;
    cleanup_old_downloads(&download_dir, "grok-pager", &download.version).await;

    // Persist installer to config.toml so future runs auto-detect internal.
    let _ = config::update_config(|st| {
        st.cli.installer = Some("internal".to_string());
    })
    .await;

    // Regenerate shell completions so they reflect the new binary's CLI surface.
    // Best-effort: failures are silently ignored (same as the installer).
    regenerate_completions(&link_path, &grok_home).await;

    Ok(())
}

/// Regenerate shell completions after a binary update (best-effort).
///
/// Spawns the newly-installed binary with `completions <shell>` for each
/// supported shell and writes the output to the standard completion paths.
/// Failures are silently ignored — completions are a nice-to-have, not a
/// requirement for a successful update.
async fn regenerate_completions(binary: &std::path::Path, grok_home: &std::path::Path) {
    // Derive $HOME independently — grok_home may be overridden via GROK_HOME
    // env var, so grok_home.parent() isn't necessarily the user's home dir.
    #[allow(deprecated)]
    let user_home = std::env::home_dir().unwrap_or_default();

    let completions: &[(&str, std::path::PathBuf)] = &[
        ("bash", grok_home.join("completions/bash/grok.bash")),
        ("zsh", grok_home.join("completions/zsh/_grok")),
        ("fish", user_home.join(".config/fish/completions/grok.fish")),
    ];

    for (shell, dest) in completions {
        if let Some(parent) = dest.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let mut cmd = tokio::process::Command::new(binary);
        cmd.args(["completions", shell])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        xai_grok_tools::util::detach_command(&mut cmd);
        let Ok(output) = cmd.output().await else {
            continue;
        };
        if output.status.success() && !output.stdout.is_empty() {
            let _ = tokio::fs::write(dest, &output.stdout).await;
        }
    }
}

/// Compute a relative symlink target from `link` to `target`.
///
/// When both paths share a grandparent (e.g. `~/.grok/bin/grok` and
/// `~/.grok/downloads/grok-0.1.203-linux-x86_64`), returns a relative path
/// like `../downloads/grok-0.1.203-linux-x86_64`.  When they share the same
/// parent directory, returns just the filename.  Falls back to the absolute
/// `target` path for any other layout.
///
/// Relative symlinks survive Docker bind-mounts where `~/.grok/` is mapped
/// into a container with a different `$HOME` (and thus a different absolute
/// prefix).
#[cfg(unix)]
fn relative_symlink_target(target: &std::path::Path, link: &std::path::Path) -> std::path::PathBuf {
    let (Some(target_parent), Some(link_parent)) = (target.parent(), link.parent()) else {
        return target.to_path_buf();
    };
    // Same directory — just the filename (e.g. grok-latest -> grok-0.1.203-…)
    if target_parent == link_parent
        && let Some(name) = target.file_name()
    {
        return std::path::PathBuf::from(name);
    }
    // Sibling directories — ../target_dir/filename (e.g. bin/grok -> ../downloads/grok-…)
    if let (Some(tp), Some(lp)) = (target_parent.parent(), link_parent.parent())
        && tp == lp
        && let (Some(dir_name), Some(file_name)) = (target_parent.file_name(), target.file_name())
    {
        return std::path::Path::new("..").join(dir_name).join(file_name);
    }
    target.to_path_buf()
}

/// Swap `~/.grok/bin/{grok,agent}` to point at `binary_path`. Returns the
/// `grok` link path (for [`regenerate_completions`]).
///
/// `grok` and `agent` are first-class entry points that the bootstrap
/// installers (`install.sh`, `install.ps1`, `install-enterprise.sh`)
/// maintain in lockstep, and so must the updater — otherwise `grok update`
/// leaves `agent` pinned at the previous version.
///
/// Unix: atomic symlink swap with relative target (survives Docker
/// bind-mounts of `~/.grok/`). Windows: [`windows_replace_exe`].
///
/// **All-or-nothing.** Each link's prior state is captured (Unix: prior
/// symlink target; Windows: `.rollback.bak`; or `Absent` marker via
/// `symlink_metadata`) before the swap, and any earlier successful swaps
/// are rolled back if a later one fails — including *removing* a link that
/// didn't exist before. Restore failures go to `tracing::warn!`; the swap
/// error itself propagates unwrapped so the caller's `reinstall_hint` wrap
/// stays the user-visible message.
async fn swap_managed_bin_links(
    binary_path: &std::path::Path,
    bin_dir: &std::path::Path,
) -> Result<std::path::PathBuf> {
    let grok_name = if cfg!(windows) { "grok.exe" } else { "grok" };
    let agent_name = if cfg!(windows) { "agent.exe" } else { "agent" };
    let grok_link = bin_dir.join(grok_name);
    let agent_link = bin_dir.join(agent_name);
    let link_paths: [std::path::PathBuf; 2] = [grok_link.clone(), agent_link];

    // Capture every link up-front so a 2nd-link capture failure can't
    // strand the 1st mid-swap.
    let mut captured: Vec<LinkRollback> = Vec::with_capacity(link_paths.len());
    for path in &link_paths {
        match LinkRollback::capture(path).await {
            Ok(rb) => captured.push(rb),
            Err(e) => {
                // Nothing swapped yet; drop any Windows .rollback.bak files.
                for prior in &captured {
                    prior.cleanup().await;
                }
                return Err(e)
                    .with_context(|| format!("capturing rollback state for {}", path.display()));
            }
        }
    }

    let mut completed: Vec<&LinkRollback> = Vec::with_capacity(captured.len());
    for (i, (link_path, rollback)) in link_paths.iter().zip(captured.iter()).enumerate() {
        #[cfg(unix)]
        let swap_result = {
            let rel_target = relative_symlink_target(binary_path, link_path);
            atomic_symlink_swap(&rel_target, link_path).await
        };
        #[cfg(windows)]
        let swap_result = windows_replace_exe(binary_path, link_path).await;
        #[cfg(not(any(unix, windows)))]
        let swap_result: Result<()> = {
            // No managed bin layout on this target; no-op.
            let _ = (binary_path, link_path);
            Ok(())
        };

        match swap_result {
            Ok(()) => completed.push(rollback),
            Err(e) => {
                // Restore each successful swap in reverse. On restore
                // failure keep the .rollback.bak as a recovery artifact
                // (Windows only) and warn!; the swap error propagates so
                // `reinstall_hint` is the user-visible message.
                for prior in completed.iter().rev() {
                    if let Err(restore_err) = prior.restore().await {
                        let backup_note = prior.backup_path().map_or(String::new(), |p| {
                            format!(" (prior binary preserved at {})", p.display())
                        });
                        tracing::warn!(
                            "failed to roll back managed bin link {}: {restore_err:#}{backup_note}",
                            prior.link_path().display(),
                        );
                        continue;
                    }
                    prior.cleanup().await;
                }
                // Failed swap had no active state to restore; drop its backup.
                rollback.cleanup().await;
                // Drop backups for never-attempted later captures (Windows orphans).
                for later in &captured[i + 1..] {
                    later.cleanup().await;
                }
                return Err(e);
            }
        }
    }

    for cap in &captured {
        cap.cleanup().await;
    }
    Ok(grok_link)
}

/// Snapshot of a managed-bin link's prior state for rollback in
/// [`swap_managed_bin_links`]. `Absent` vs `Present` is discriminated up
/// front via `symlink_metadata` so capture errors never get misread as
/// "link was absent".
enum LinkRollback {
    /// Link was absent before the swap; rollback removes the one we created.
    Absent { link_path: std::path::PathBuf },
    /// Link existed before the swap; rollback restores its prior contents.
    Present {
        link_path: std::path::PathBuf,
        /// Unix: prior symlink target (relative or absolute).
        #[cfg(unix)]
        prior_target: std::path::PathBuf,
        /// Windows: `.rollback.bak` copy of the previous binary.
        #[cfg(windows)]
        backup_path: std::path::PathBuf,
    },
}

impl LinkRollback {
    async fn capture(link_path: &std::path::Path) -> Result<Self> {
        let lp = link_path.to_path_buf();

        // `symlink_metadata` (lstat) handles valid symlinks, broken
        // symlinks, and regular files alike. Any IO error other than
        // NotFound aborts the swap before mutation.
        match tokio::fs::symlink_metadata(&lp).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LinkRollback::Absent { link_path: lp });
            }
            Err(e) => {
                return Err(e).with_context(|| format!("stat {} before swap", lp.display()));
            }
        }

        #[cfg(unix)]
        {
            let prior_target = tokio::fs::read_link(&lp)
                .await
                .with_context(|| format!("reading prior symlink target {}", lp.display()))?;
            Ok(LinkRollback::Present {
                link_path: lp,
                prior_target,
            })
        }
        #[cfg(windows)]
        {
            // Per-process+sequence backup name via `unique_temp_sibling`
            // so concurrent updaters can't clobber each other's backups.
            let backup_path = unique_temp_sibling(&lp, "rollback.bak");
            tokio::fs::copy(&lp, &backup_path).await.with_context(|| {
                format!(
                    "backing up {} to {} before swap",
                    lp.display(),
                    backup_path.display(),
                )
            })?;
            Ok(LinkRollback::Present {
                link_path: lp,
                backup_path,
            })
        }
    }

    fn link_path(&self) -> &std::path::Path {
        match self {
            LinkRollback::Absent { link_path } => link_path,
            LinkRollback::Present { link_path, .. } => link_path,
        }
    }

    /// Path to the on-disk backup (Windows only — Unix is in-memory).
    #[cfg(windows)]
    fn backup_path(&self) -> Option<&std::path::Path> {
        match self {
            LinkRollback::Present { backup_path, .. } => Some(backup_path),
            LinkRollback::Absent { .. } => None,
        }
    }
    #[cfg(unix)]
    fn backup_path(&self) -> Option<&std::path::Path> {
        None
    }

    async fn restore(&self) -> Result<()> {
        match self {
            LinkRollback::Absent { link_path } => {
                // Remove the link we created. NotFound (someone else
                // cleaned up) is fine; anything else is a real failure.
                match tokio::fs::remove_file(link_path).await {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e).with_context(|| {
                        format!("removing rolled-back link {}", link_path.display())
                    }),
                }
            }
            #[cfg(unix)]
            LinkRollback::Present {
                link_path,
                prior_target,
            } => atomic_symlink_swap(prior_target, link_path)
                .await
                .with_context(|| {
                    format!("restoring prior symlink target for {}", link_path.display())
                }),
            #[cfg(windows)]
            LinkRollback::Present {
                link_path,
                backup_path,
            } => {
                // Route through `windows_replace_exe` so rollback inherits
                // the same ERROR_SHARING_VIOLATION rename-aside fallback
                // as the forward path.
                windows_replace_exe(backup_path, link_path)
                    .await
                    .with_context(|| {
                        format!(
                            "restoring {} from {}",
                            link_path.display(),
                            backup_path.display()
                        )
                    })
            }
        }
    }

    async fn cleanup(&self) {
        #[cfg(windows)]
        if let LinkRollback::Present { backup_path, .. } = self {
            let _ = tokio::fs::remove_file(backup_path).await;
        }
        #[cfg(unix)]
        let _ = self; // no on-disk backup on Unix
    }
}

/// Atomically swap a symlink to point to a new target.
///
/// Creates a temporary symlink next to `link_path`, then renames it over the
/// old symlink.  This avoids the remove-then-create race where the path
/// briefly doesn't exist, and — crucially — never deletes the old target
/// file.  On macOS (especially Apple Silicon), deleting a binary that a
/// running process has mmap'd causes SIGKILL because the kernel can no longer
/// verify the code signature of the executable pages.
#[cfg(unix)]
async fn atomic_symlink_swap(target: &std::path::Path, link_path: &std::path::Path) -> Result<()> {
    // Per-racer temp name: a shared one makes remove_file → symlink racy
    // (EEXIST, or ENOENT when another racer renames the link away).
    sweep_stale_tmp_links(link_path, STALE_TMP_AGE).await;
    let tmp_link = unique_temp_sibling(link_path, "tmp-link");
    let _ = tokio::fs::remove_file(&tmp_link).await;
    tokio::fs::symlink(target, &tmp_link).await?;
    tokio::fs::rename(&tmp_link, link_path).await?;
    Ok(())
}

/// Remove `<link>.*.tmp-link` siblings left by a swap that crashed between
/// symlink and rename. Only those older than `max_age` are removed, so a
/// concurrent racer's in-flight link is never deleted out from under it.
#[cfg(unix)]
async fn sweep_stale_tmp_links(link_path: &std::path::Path, max_age: Duration) {
    let (Some(dir), Some(name)) = (
        link_path.parent(),
        link_path.file_name().and_then(|n| n.to_str()),
    ) else {
        return;
    };
    let prefix = format!("{name}.");
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let fname = entry.file_name();
        let Some(fname) = fname.to_str() else {
            continue;
        };
        if !fname.starts_with(&prefix) || !fname.ends_with(".tmp-link") {
            continue;
        }
        let stale = tokio::fs::symlink_metadata(entry.path())
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if stale {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Replace an executable that may be locked by a running process (Windows).
///
/// On Windows the kernel prevents writes to a running executable but allows
/// renames. If a direct copy fails with a sharing violation, this renames
/// `dest` aside and copies `src` into the freed path. If the copy then
/// fails, the rename is rolled back to avoid a broken install.
///
/// The aside target is normally `<dest>.old`, but a leftover `.old` can
/// itself still be a running image (the session that was live during the
/// previous update keeps executing the renamed-aside file), and a running
/// image can neither be deleted nor rename-replaced. In that case `dest` is
/// renamed to a unique `<dest>.old.{pid}-{seq}.old` sibling instead, so a
/// locked leftover can never block the update. All `.old` leftovers are
/// swept best-effort at the start of each cycle; still-locked ones survive
/// until a later update runs after those processes exit.
#[cfg(windows)]
async fn windows_replace_exe(src: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("destination has no filename: {}", dest.display()))?
        .to_string_lossy();
    let old = dest.with_file_name(format!("{file_name}.old"));

    sweep_old_exe_backups(&old).await;

    match tokio::fs::copy(src, dest).await {
        Ok(_) => return Ok(()),
        // ERROR_SHARING_VIOLATION (32) / ERROR_ACCESS_DENIED (5): exe is
        // locked by a running process. Fall through to rename-and-replace.
        Err(e) if matches!(e.raw_os_error(), Some(32) | Some(5)) => {
            tracing::debug!("exe locked, falling back to rename: {e}");
        }
        Err(e) => return Err(e.into()),
    }

    // A .old that survived the sweep is locked; renaming onto it would need
    // to delete-replace it and fail, so divert to a guaranteed-free name.
    let old_is_free = matches!(
        tokio::fs::symlink_metadata(&old).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    );
    let mut aside = if old_is_free {
        old.clone()
    } else {
        let diverted = unique_temp_sibling(&old, "old");
        tracing::debug!(
            "stale {} is locked; diverting aside to {}",
            old.display(),
            diverted.display()
        );
        diverted
    };

    // Move the locked file aside, then copy the new binary into place.
    let mut rename_result = tokio::fs::rename(dest, &aside).await;
    // Pid reuse can collide a diverted name with a dead updater's
    // still-locked leftover, and a racer can occupy a just-checked-free
    // .old; a fresh unique sibling clears both tails (3 attempts total).
    for _ in 0..2 {
        match &rename_result {
            Err(e) if matches!(e.raw_os_error(), Some(32) | Some(5)) => {
                tracing::debug!(
                    "rename aside to {} failed; retrying with a fresh name: {e}",
                    aside.display()
                );
                aside = unique_temp_sibling(&old, "old");
                rename_result = tokio::fs::rename(dest, &aside).await;
            }
            _ => break,
        }
    }
    rename_result.map_err(|e| {
        anyhow::anyhow!(
            "cannot rename locked executable {}: {e}\n\
             Close all running grok sessions and retry.",
            dest.display(),
        )
    })?;
    match tokio::fs::copy(src, dest).await {
        Ok(_) => Ok(()),
        Err(e) => {
            // Rollback: restore the old binary so the install isn't broken.
            let _ = tokio::fs::rename(&aside, dest).await;
            Err(e.into())
        }
    }
}

/// Best-effort removal of `<exe>.old` plus the unique
/// `<exe>.old.{pid}-{seq}.old` asides accumulated by prior update cycles.
/// Locked ones (still-running images) survive and are collected by a later
/// update once those processes exit. The `<exe>.old` prefix keeps the sweep
/// away from `<exe>` itself, other executables' leftovers, and the
/// `.rollback.bak` / `.tmp` sibling shapes.
///
/// Unlike `sweep_stale_tmp_links` there is deliberately no `max_age` gate:
/// rename preserves mtime, so a racer's seconds-old aside already looks
/// days old and age cannot distinguish it; in-use asides survive deletion
/// by being locked; and deleting a racer's fresh unlocked aside (its
/// rollback source while both racers converge on the same dest) is the
/// accepted lock-free residual race (see `tmp_download_path`).
#[cfg(windows)]
async fn sweep_old_exe_backups(old: &std::path::Path) {
    let _ = tokio::fs::remove_file(old).await;
    let (Some(dir), Some(old_name)) = (old.parent(), old.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    let prefix = format!("{old_name}.");
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(&prefix) && name.ends_with(".old") {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Best-effort cleanup of old versioned binaries for a given binary name.
///
/// Mirrors the npm `cleanupOldVersions()` policy: keeps the current version
/// plus one previous version (in case a process is still running the old binary
/// and hasn't fully loaded all pages yet — deleting it on macOS causes SIGKILL
/// because the kernel can no longer verify the code signature).
///
/// `bin_prefix` is the binary name prefix, e.g. `"grok"` or `"grok-pager"`.
/// Files must match `{bin_prefix}-{digit}*` to be considered versioned binaries
/// (this avoids `grok-*` matching `grok-pager-*` or `grok-latest`).
///
/// Temporary/partial files (containing `.tmp`) are deleted only once they
/// are **stale** (mtime older than [`STALE_TMP_AGE`]). A fresh `.tmp` may be
/// a concurrent updater's in-flight download — the same-instant race the
/// lock-free design accepts — and deleting it out from under that updater
/// would make its atomic rename fail.
async fn cleanup_old_downloads(dir: &std::path::Path, bin_prefix: &str, current_version: &str) {
    let prefix = format!("{}-", bin_prefix);
    let current_semver = match semver::Version::parse(current_version) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "cleanup_old_downloads: invalid current version '{}': {}",
                current_version,
                e
            );
            return;
        }
    };

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!(
                "cleanup_old_downloads: failed to read {}: {}",
                dir.display(),
                e
            );
            return;
        }
    };

    let mut versioned: Vec<(semver::Version, String)> = Vec::new();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        // Temp/partial files: sweep only STALE ones. A fresh `.tmp` may be a
        // concurrent updater's in-flight download — deleting it would make
        // that updater's atomic rename fail with ENOENT.
        if name.contains(".tmp") {
            let stale = match entry.metadata().await.and_then(|m| m.modified()) {
                Ok(modified) => std::time::SystemTime::now()
                    .duration_since(modified)
                    .map(|age| age > STALE_TMP_AGE)
                    // Future mtime (clock skew): can't tell — leave it.
                    .unwrap_or(false),
                // Unknown mtime: leave it; it is swept once readable+old.
                Err(_) => false,
            };
            if stale && let Err(e) = tokio::fs::remove_file(entry.path()).await {
                tracing::warn!("failed to remove stale temp file {}: {}", name, e);
            }
            continue;
        }
        // Skip symlinks (e.g. grok-latest).
        if let Ok(ft) = entry.file_type().await
            && ft.is_symlink()
        {
            continue;
        }
        // The suffix after the prefix must start with a digit to be a versioned
        // binary (avoids `grok-latest`, `grok-pager-*` when prefix is `grok`).
        let suffix = &name[prefix.len()..];
        if !suffix.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        // Extract the version portion via the shared parser (handles the
        // internal `grok-0.1.150-macos-aarch64`, pre-release, and npm
        // `grok-0.1.150` layouts — see `version_from_versioned_binary_name`).
        let Some(ver_str) = crate::version::version_from_versioned_binary_name(&name, bin_prefix)
        else {
            continue;
        };
        if let Ok(v) = semver::Version::parse(&ver_str) {
            // Skip the current version — never delete it.
            if v == current_semver {
                continue;
            }
            versioned.push((v, name));
        }
    }

    // Sort descending by version so the newest is first.
    versioned.sort_by(|a, b| b.0.cmp(&a.0));

    // Keep the most recent old version (index 0), delete the rest (index 1+).
    // This matches the npm policy: current + 1 previous.
    for (_, name) in versioned.iter().skip(1) {
        let path = dir.join(name);
        // Same freshness guard as the `.tmp` sweep: a versioned binary
        // written moments ago is likely a concurrent installer's
        // just-renamed download (its symlink swap hasn't happened yet) —
        // deleting it would leave that installer's swap pointing at
        // nothing. Old binaries from previous releases are days old.
        let fresh = tokio::fs::metadata(&path)
            .await
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age <= STALE_TMP_AGE);
        if fresh {
            continue;
        }
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!("failed to remove old binary {}: {}", name, e);
        }
    }
}

fn installer_manages_bin_entrypoints(installer: &str) -> bool {
    matches!(installer, "internal" | "gh-release")
}

#[cfg_attr(not(any(unix, windows)), allow(clippy::unused_async))]
async fn heal_managed_install(installer: &str) {
    if !installer_manages_bin_entrypoints(installer) {
        return;
    }

    #[cfg(any(unix, windows))]
    {
        let bin_dir = grok_home().join("bin");

        #[cfg(unix)]
        reconcile_agent_to_grok(&bin_dir).await;

        #[cfg(windows)]
        reconcile_agent_exe_to_grok(&bin_dir).await;
    }
}

#[cfg(unix)]
async fn reconcile_agent_to_grok(bin_dir: &std::path::Path) {
    let grok_link = bin_dir.join("grok");
    let agent_link = bin_dir.join("agent");

    let Ok(grok_target) = tokio::fs::read_link(&grok_link).await else {
        return;
    };
    if tokio::fs::metadata(&grok_link).await.is_err() {
        return;
    }
    if let Ok(agent_target) = tokio::fs::read_link(&agent_link).await
        && agent_target == grok_target
    {
        return;
    }
    match atomic_symlink_swap(&grok_target, &agent_link).await {
        Ok(()) => tracing::info!(
            grok_target = %grok_target.display(),
            "reconciled agent bin symlink to grok target"
        ),
        Err(e) => tracing::warn!("failed to reconcile agent bin symlink: {e:#}"),
    }
}

#[cfg(windows)]
async fn reconcile_agent_exe_to_grok(bin_dir: &std::path::Path) {
    let grok_exe = bin_dir.join("grok.exe");
    let agent_exe = bin_dir.join("agent.exe");

    if tokio::fs::metadata(&grok_exe).await.is_err() {
        return;
    }
    match agent_exe_differs(&grok_exe, &agent_exe).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::debug!("agent.exe reconcile: compare failed: {e:#}");
            return;
        }
    }
    match windows_replace_exe(&grok_exe, &agent_exe).await {
        Ok(()) => tracing::info!("reconciled agent.exe to grok.exe"),
        Err(e) => tracing::warn!("failed to reconcile agent.exe to grok.exe: {e:#}"),
    }
}

#[cfg(windows)]
async fn agent_exe_differs(
    grok: &std::path::Path,
    agent: &std::path::Path,
) -> std::io::Result<bool> {
    use tokio::io::{AsyncReadExt, BufReader};
    let grok_len = tokio::fs::metadata(grok).await?.len();
    match tokio::fs::metadata(agent).await {
        Ok(m) if m.len() != grok_len => return Ok(true),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e),
    }
    let mut rg = BufReader::new(tokio::fs::File::open(grok).await?);
    let mut ra = BufReader::new(tokio::fs::File::open(agent).await?);
    let mut bg = [0u8; 64 * 1024];
    let mut ba = [0u8; 64 * 1024];
    loop {
        let n = rg.read(&mut bg).await?;
        if n == 0 {
            return Ok(false);
        }
        ra.read_exact(&mut ba[..n]).await?;
        if bg[..n] != ba[..n] {
            return Ok(true);
        }
    }
}

/// Download a single asset from a GitHub release via `gh release download`.
async fn gh_release_download(tag: &str, pattern: &str, dest: &std::path::Path) -> Result<()> {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("  {spinner:.cyan} Downloading from GitHub Releases...")
            .unwrap(),
    );
    pb.enable_steady_tick(Duration::from_millis(100));

    let mut cmd = tokio::process::Command::new("gh");
    cmd.args([
        "release",
        "download",
        tag,
        "--repo",
        crate::version::GH_RELEASE_REPO,
        "--pattern",
        pattern,
        "--output",
        &dest.to_string_lossy(),
        "--clobber",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    xai_grok_tools::util::detach_command(&mut cmd);
    cmd.envs(xai_grok_tools::util::pager_env());
    let output = cmd.output().await?;

    pb.finish_and_clear();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "gh release download failed for {} tag {} from {}: {}",
            pattern,
            tag,
            crate::version::GH_RELEASE_REPO,
            stderr.trim()
        );
    }
    Ok(())
}

/// Download and install grok from GitHub Releases (xai-org-shared/grok-build).
///
/// Uses `gh release download` to fetch the binary matching the current platform.
/// This works anywhere the `gh` CLI is authenticated, without needing npm or
/// internal network access.
async fn install_gh_release(target: Option<&str>) -> Result<()> {
    let (os, arch) = detect_platform()?;
    let platform = format!("{}-{}", os, arch);

    let version = match target {
        Some(v) => v.to_string(),
        None => crate::version::fetch_gh_release_version("stable").await?,
    };

    let grok_home = grok_home();
    let download_dir = grok_home.join("downloads");
    let bin_dir = grok_home.join("bin");
    tokio::fs::create_dir_all(&download_dir).await?;
    tokio::fs::create_dir_all(&bin_dir).await?;

    let binary_name = format!("grok-{}-{}", version, platform);
    let binary_path = download_dir.join(&binary_name);
    let tag = format!("v{}", version);

    eprintln!(
        "  Downloading grok v{} ({}) from GitHub Releases...",
        version, platform
    );

    gh_release_download(&tag, &binary_name, &binary_path).await?;

    // chmod +x
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).await?;
    }

    // Atomic swap of ~/.grok/bin/{grok,agent} -> downloaded binary.
    swap_managed_bin_links(&binary_path, &bin_dir).await?;

    // Update grok-latest -> versioned binary so any existing symlinks that route
    // through it (e.g. /usr/local/bin/grok -> ~/.grok/downloads/grok-latest)
    // resolve to the newly installed version.
    #[cfg(unix)]
    {
        let latest_path = download_dir.join("grok-latest");
        let rel_target = relative_symlink_target(&binary_path, &latest_path);
        if let Err(e) = atomic_symlink_swap(&rel_target, &latest_path).await {
            tracing::warn!("Failed to update grok-latest symlink: {e}");
        }
    }

    // Also update /usr/local/bin/{grok,agent} if either points directly into
    // ~/.grok/downloads/ (legacy layout — skips the grok-latest indirection).
    // Permission errors ignored.
    #[cfg(unix)]
    for name in ["grok", "agent"] {
        let system_link = std::path::PathBuf::from(format!("/usr/local/bin/{name}"));
        if let Ok(existing_target) = tokio::fs::read_link(&system_link).await {
            let target_str = existing_target.to_string_lossy();
            if target_str.contains(".grok/downloads/") && !target_str.ends_with("grok-latest") {
                // Try to update; ignore permission errors
                let _ = atomic_symlink_swap(&binary_path, &system_link).await;
            }
        }
    }

    remove_stale_pager(&bin_dir).await;

    eprintln!();

    // Clean up old versioned binaries (keeps current + 1 previous).
    cleanup_old_downloads(&download_dir, "grok", &version).await;
    cleanup_old_downloads(&download_dir, "grok-pager", &version).await;

    // Persist installer to config.toml so future runs auto-detect gh-release.
    let _ = config::update_config(|st| {
        st.cli.installer = Some("gh-release".to_string());
    })
    .await;

    Ok(())
}

/// Creates a temporary .npmrc file with the NPM token if present.
/// Returns the path to the created file, or None if no token was set.
fn create_temp_npmrc(npm_registry: Option<&str>) -> Result<Option<std::path::PathBuf>> {
    if let Ok(token) = std::env::var("NPM_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            let dir = std::env::temp_dir();
            let npmrc_path = dir.join(format!(".npmrc-{}-install", std::process::id()));
            let registry_host = npm_registry
                .and_then(|r| reqwest::Url::parse(r).ok())
                .map(|u| {
                    let host = u.host_str().unwrap_or("registry.npmjs.org");
                    let port_suffix = u.port().map(|p| format!(":{}", p)).unwrap_or_default();
                    format!("{}{}{}", host, port_suffix, u.path().trim_end_matches('/'))
                })
                .unwrap_or_else(|| "registry.npmjs.org".to_string());
            let npmrc_content = format!("//{}/:_authToken={}\n", registry_host, token);
            std::fs::write(&npmrc_path, npmrc_content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&npmrc_path, std::fs::Permissions::from_mode(0o600))?;
            }
            return Ok(Some(npmrc_path));
        }
    }
    Ok(None)
}

/// Check if other grok processes are running (macOS only).
///
/// On macOS, `npm i -g` replaces the vendored binary in node_modules in-place.
/// Any grok process running from that vendored path will be SIGKILL'd by the
/// kernel because macOS (Apple Silicon in particular) can no longer verify
/// the code signature of the mmap'd executable pages once the backing file
/// inode is unlinked.
///
/// While our postinstall.js now uses versioned binaries under ~/.grok/bin/
/// (so processes launched from there are safe), older installations or npx
/// invocations may still be running the vendored binary directly.
#[cfg(target_os = "macos")]
fn warn_if_other_grok_processes_running() {
    let my_pid = std::process::id().to_string();
    let mut cmd = Command::new("pgrep");
    cmd.args(["-f", "grok"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    xai_grok_tools::util::detach_std_command(&mut cmd);
    if let Ok(output) = cmd.output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let other_pids: Vec<&str> = stdout
            .lines()
            .map(|l| l.trim())
            .filter(|pid| !pid.is_empty() && *pid != my_pid)
            .collect();
        if !other_pids.is_empty() {
            eprintln!(
                "  ⚠ Warning: {} other grok process(es) detected.",
                other_pids.len()
            );
            eprintln!("    Processes running from the npm vendored binary path may be");
            eprintln!("    killed by macOS when npm replaces the package files.");
            eprintln!("    Consider closing other grok sessions before updating.");
            eprintln!();
        }
    }
}

/// Test-only entry point: invokes the private [`install_npm`] for tests
/// that swap in a fake `npm` via PATH.
#[doc(hidden)]
pub fn install_npm_for_test(
    target: Option<&str>,
    channel: &str,
    npm_registry: Option<&str>,
) -> Result<()> {
    install_npm(target, channel, npm_registry)
}

fn install_npm(target: Option<&str>, channel: &str, npm_registry: Option<&str>) -> Result<()> {
    // Warn on macOS about potential impact on other running processes.
    #[cfg(target_os = "macos")]
    warn_if_other_grok_processes_running();

    let version_arg = match target {
        Some(ver) => format!("@xai-official/grok@{ver}"),
        None => {
            // All current callers resolve the version via get_latest_version
            // (which applies max(stable, alpha) for the alpha channel) before
            // reaching here.  Falling back to a raw dist-tag would bypass that
            // logic, so warn loudly if this path is ever hit.
            tracing::warn!(
                channel,
                "install_npm called without a resolved version, falling back to dist-tag"
            );
            format!(
                "@xai-official/grok@{}",
                if channel == "alpha" {
                    "alpha"
                } else {
                    "latest"
                }
            )
        }
    };

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("  {spinner:.cyan} Installing via npm...")
            .unwrap(),
    );
    pb.enable_steady_tick(Duration::from_millis(100));

    let mut cmd = Command::new("npm");
    cmd.args(["i", "-g", &version_arg]);
    if let Some(registry) = npm_registry {
        cmd.arg(format!("--registry={}", registry));
    }

    // Use a temporary .npmrc to avoid exposing the token in process lists or shell history.
    let temp_npmrc = create_temp_npmrc(npm_registry)?;
    if let Some(ref npmrc_path) = temp_npmrc {
        cmd.arg(format!("--userconfig={}", npmrc_path.display()));
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        // inherit, not piped — same rationale as run_update_subcommand.
        .stderr(Stdio::inherit());
    xai_grok_tools::util::detach_std_command(&mut cmd);
    let status = cmd.status()?;

    if let Some(path) = temp_npmrc
        && let Err(e) = std::fs::remove_file(&path)
    {
        tracing::warn!("Failed to remove temp .npmrc file: {}", e);
    }

    pb.finish_and_clear();

    if !status.success() {
        anyhow::bail!("npm install failed. Please try again.");
    }
    eprintln!();
    Ok(())
}

pub async fn apply_channel_switch(channel_switch: Option<&str>, update_config: &mut UpdateConfig) {
    if let Some(ch) = channel_switch
        && update_config.channel != ch
    {
        let _ = config::update_config(|st| {
            st.cli.channel = Some(ch.to_string());
        })
        .await;
        update_config.channel = ch.to_string();
        eprintln!("Switched to {} channel.", ch);
    }
}

/// Run the `grok update` command. Returns `Ok(Some(version))` when the target
/// version is present on disk afterwards — either installed by this call or
/// found already installed (e.g. by a concurrent background download); returns
/// `Ok(None)` when there is no installer or no applicable target. Callers use
/// the returned version to signal a running leader to relaunch onto the new
/// binary (see the pager's post-update leader relaunch) — that signal must
/// fire even when the download itself was skipped, so a stale leader still
/// picks up a binary someone else installed.
pub async fn run_update(
    force: bool,
    pinned_version: Option<&str>,
    channel_switch: Option<&str>,
    update_config: &mut UpdateConfig,
) -> Result<Option<String>> {
    apply_channel_switch(channel_switch, update_config).await;
    let installer = match get_installer().await {
        Some(i) => i,
        None => {
            eprintln!("Auto-update is not available for manual installations.");
            return Ok(None);
        }
    };

    // Persist installer if not already saved
    let cfg = config::load_config().await;
    if cfg.cli.installer.is_none() {
        let _ = config::update_config(|st| {
            st.cli.installer = Some(installer.to_string());
        })
        .await;
    }

    heal_managed_install(installer).await;

    let current_version = get_installed_grok_version();
    let policy = config::VersionPolicy::resolve();

    // When --version is given, skip the latest-version check and install directly
    if let Some(version) = pinned_version {
        if let Err(e) = crate::version_policy::check_install_target(&policy, version) {
            anyhow::bail!("{e}");
        }
        eprintln!(
            "Installing Grok {} (current: {})...",
            version, current_version
        );
        eprintln!();
        run_install_script(installer, Some(version), update_config).await?;
        refresh_deployment_config().await;
        if let Err(e) = config::update_config(|st| {
            st.cli.auto_update = Some(false);
        })
        .await
        {
            tracing::warn!("Failed to persist auto_update=false for pinned install: {e}");
        }
        eprintln!("  ✓ grok v{} installed successfully!", version);
        eprintln!("  Please restart Grok.");
        return Ok(Some(version.to_string()));
    }

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("  {spinner:.cyan} Checking for updates...")
            .unwrap(),
    );
    pb.enable_steady_tick(Duration::from_millis(100));
    let plan = fetch_update_plan(installer, update_config, &policy).await?;
    pb.finish_and_clear();

    let (latest_version, install_target) = match plan {
        UpdatePlan::Skip { latest } => {
            // Cache so an explicit `grok update` doesn't re-prompt every run.
            let stable_ptr = try_fetch_stable_pointer().await;
            write_version_cache(&latest, stable_ptr.as_deref()).await;
            eprintln!(
                "The latest release ({latest}) is not an allowed update; \
                 keeping the current version ({current_version})."
            );
            refresh_deployment_config().await;
            return Ok(None);
        }
        UpdatePlan::Unavailable { latest, target } => {
            anyhow::bail!(
                "The required minimum version ({target}) is newer than the latest \
                 available release ({latest}). Contact your administrator."
            );
        }
        UpdatePlan::Install { latest, target } => (latest, target),
    };
    if install_target != latest_version {
        eprintln!(
            "Latest available is {latest_version}, but your configured version range \
             allows {install_target}; installing that instead."
        );
    }

    // What's on disk wins over this process's compiled-in version: a
    // concurrent or earlier updater (TUI background download, leader hourly
    // checker) may already have installed the target, in which case there is
    // nothing to download. Gated on the installer maintaining the managed
    // symlink — for npm a leftover symlink would lie (see
    // `disk_version_for_installer`).
    let effective_current =
        disk_version_for_installer(installer).unwrap_or_else(|| current_version.clone());

    if !force {
        match needs_update(
            &effective_current,
            &install_target,
            &update_config.channel,
            installer_allows_downgrade(installer),
        ) {
            Some(true) => {}
            Some(false) => {
                // Explicit channel switch (--stable / --alpha) with a
                // different target version: install even though the current
                // version is "newer" by semver. This handles switching from
                // alpha 0.2.X back to stable 0.1.220 where 0.2.X > 0.1.220.
                if channel_switch.is_some() && effective_current != install_target {
                    // Fall through to install
                } else {
                    let stable_ptr = try_fetch_stable_pointer().await;
                    write_version_cache(&install_target, stable_ptr.as_deref()).await;
                    eprintln!("Already up to date ({}).", effective_current);
                    // Retry if a prior sync failed.
                    refresh_deployment_config().await;
                    // The target is on disk even though this call installed
                    // nothing — report it so the caller still signals stale
                    // leaders to relaunch onto it (signalling is directional
                    // and skips leaders already at/after this version).
                    return Ok(Some(install_target));
                }
            }
            None => {
                // Distinguish parse failure from unsupported channel.
                let parse_ok = semver::Version::parse(&effective_current).is_ok()
                    && semver::Version::parse(&install_target).is_ok();
                if parse_ok {
                    anyhow::bail!(
                        "Unsupported release channel '{}' (current={}, target={}). \
                         Supported channels: stable, alpha, enterprise. \
                         Use --stable or --alpha to override, or set [cli] channel in config.toml.",
                        update_config.channel,
                        effective_current,
                        install_target
                    );
                } else {
                    anyhow::bail!(
                        "Failed to parse versions (current={}, target={})",
                        effective_current,
                        install_target
                    );
                }
            }
        }
    }

    let target_version = if force
        && !needs_update(
            &effective_current,
            &install_target,
            &update_config.channel,
            installer_allows_downgrade(installer),
        )
        .unwrap_or(true)
    {
        eprintln!(
            "Forcing reinstall of Grok {} (already up to date)",
            effective_current
        );
        &effective_current
    } else {
        eprintln!("Updating Grok {} → {}", effective_current, install_target);
        &install_target
    };

    eprintln!();
    run_install_script(installer, Some(target_version), update_config).await?;
    // Fetch the stable pointer now so the new binary has it immediately
    // for channel_label() display, rather than waiting for the next
    // TTL-gated update check (~30 min).
    let stable_ptr = try_fetch_stable_pointer().await;
    write_version_cache(target_version, stable_ptr.as_deref()).await;
    refresh_deployment_config().await;
    eprintln!("  ✓ grok v{} installed successfully!", target_version);

    if !force && std::env::var_os("GROK_AUTO_UPDATE").is_none() {
        eprintln!("  Please restart Grok.");
    }
    Ok(Some(target_version.to_string()))
}

/// Refresh managed config post-update (best-effort, staleness-gated), for
/// deployment-key and team principals alike.
async fn refresh_deployment_config() {
    if !xai_grok_shell::managed_config::has_principal() {
        return;
    }
    if !xai_grok_shell::managed_config::is_fetch_enabled() {
        return;
    }
    // Clear a logged-out team's files before deciding to fetch (mirrors the loop).
    xai_grok_shell::managed_config::clear_orphan();
    if !xai_grok_shell::config::is_managed_config_stale_for(
        &xai_grok_shell::managed_config::current_serving_identity(),
    ) {
        return;
    }
    match xai_grok_shell::managed_config::sync().await {
        Ok(true) => eprintln!("  Applied managed configuration."),
        Ok(false) => tracing::debug!("no managed configuration to apply"),
        // Auth issues aren't actionable mid-update: quiet here, loud on `grok setup`.
        Err(e) if e.is_auth_rejection() => tracing::debug!("managed config not applied: {e}"),
        Err(e) if e.is_retryable() => {
            tracing::debug!("managed config refresh failed: {e}");
            eprintln!("  Couldn't apply managed configuration. Run `grok setup` to retry.");
        }
        Err(e) => eprintln!("  Couldn't apply managed configuration. {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tmp_download_path_is_unique_per_version_and_per_attempt() {
        // The old `with_extension("tmp")` collapsed every 0.1.x versioned
        // name onto a single `grok-0.1.tmp`; the helper must keep distinct
        // versions distinct AND make repeated attempts (same process, e.g.
        // concurrent tokio tasks) unique.
        let dest_181 = std::path::Path::new("/home/u/.grok/downloads/grok-0.1.181-linux-x86_64");
        let dest_182 = std::path::Path::new("/home/u/.grok/downloads/grok-0.1.182-linux-x86_64");

        let a = tmp_download_path(dest_181);
        let b = tmp_download_path(dest_182);
        assert_ne!(a, b, "different versions must not share a temp file");

        let a2 = tmp_download_path(dest_181);
        assert_ne!(
            a, a2,
            "two attempts for the same dest must not share a temp file"
        );

        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("grok-0.1.181-linux-x86_64."),
            "full versioned name must be preserved: {name}"
        );
        assert!(
            name.ends_with(".tmp") && name.contains(&std::process::id().to_string()),
            "temp name must embed the PID and end in .tmp (cleanup sweeps *.tmp*): {name}"
        );
        assert_eq!(
            a.parent(),
            std::path::Path::new("/home/u/.grok/downloads").into(),
            "temp file must stay in the destination directory for atomic rename"
        );
    }

    #[test]
    fn test_needs_update_same_version() {
        assert_eq!(
            needs_update("0.1.141", "0.1.141", "stable", false),
            Some(false)
        );
    }

    #[test]
    fn test_needs_update_invalid_versions() {
        assert_eq!(
            needs_update("not-a-version", "0.1.141", "stable", false),
            None
        );
        assert_eq!(needs_update("0.1.141", "garbage", "stable", false), None);
    }

    #[test]
    fn test_needs_update_unknown_channel() {
        assert_eq!(needs_update("0.1.140", "0.1.141", "beta", false), None);
    }

    #[test]
    fn test_needs_update_enterprise_channel_behaves_like_stable() {
        // Enterprise uses the same conservative pre-release rules as stable.
        // Same version: no update.
        assert_eq!(
            needs_update("0.1.206", "0.1.206", "enterprise", false),
            Some(false)
        );
        // Newer stable: update.
        assert_eq!(
            needs_update("0.1.205", "0.1.206", "enterprise", false),
            Some(true)
        );
        // Older stable: no downgrade (allow_downgrade=false).
        assert_eq!(
            needs_update("0.1.207", "0.1.206", "enterprise", false),
            Some(false)
        );
        // Pre-release candidate rejected on enterprise channel.
        assert_eq!(
            needs_update("0.1.205", "0.1.206-alpha.1", "enterprise", false),
            Some(false)
        );
        // Current pre-release on enterprise forces upgrade (even to equal base).
        assert_eq!(
            needs_update("0.1.206-alpha.3", "0.1.206", "enterprise", false),
            Some(true)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_creates_new_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("binary-v1");
        std::fs::write(&target, "v1").unwrap();

        let link = dir.path().join("grok");
        // No existing symlink — should create one.
        atomic_symlink_swap(&target, &link).await.unwrap();

        assert!(link.is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v1");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();

        let target_v1 = dir.path().join("binary-v1");
        std::fs::write(&target_v1, "v1").unwrap();
        let target_v2 = dir.path().join("binary-v2");
        std::fs::write(&target_v2, "v2").unwrap();

        let link = dir.path().join("grok");
        // Set up initial symlink to v1.
        std::os::unix::fs::symlink(&target_v1, &link).unwrap();
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v1");

        // Swap to v2.
        atomic_symlink_swap(&target_v2, &link).await.unwrap();

        assert!(link.is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), target_v2);
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v2");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_preserves_old_target() {
        let dir = tempfile::tempdir().unwrap();

        let target_v1 = dir.path().join("binary-v1");
        std::fs::write(&target_v1, "v1-content").unwrap();
        let target_v2 = dir.path().join("binary-v2");
        std::fs::write(&target_v2, "v2-content").unwrap();

        let link = dir.path().join("grok");
        std::os::unix::fs::symlink(&target_v1, &link).unwrap();

        // Swap to v2.
        atomic_symlink_swap(&target_v2, &link).await.unwrap();

        // The old target file must still exist on disk — this is the key
        // property that prevents SIGKILL on macOS.  Running processes that
        // have binary-v1 mmap'd can continue to page-fault from it.
        assert!(target_v1.exists(), "old binary must not be deleted");
        assert_eq!(std::fs::read_to_string(&target_v1).unwrap(), "v1-content");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_no_intermediate_missing_state() {
        // Verify that the link path always exists (is never absent) during
        // the swap.  We can't truly test atomicity without threads, but we
        // can at least verify the path exists before and after.
        let dir = tempfile::tempdir().unwrap();

        let target_v1 = dir.path().join("binary-v1");
        std::fs::write(&target_v1, "v1").unwrap();
        let target_v2 = dir.path().join("binary-v2");
        std::fs::write(&target_v2, "v2").unwrap();

        let link = dir.path().join("grok");
        std::os::unix::fs::symlink(&target_v1, &link).unwrap();
        assert!(link.exists(), "link should exist before swap");

        atomic_symlink_swap(&target_v2, &link).await.unwrap();
        assert!(link.exists(), "link should exist after swap");

        // No tmp-link file should be left behind.
        let tmp_link = link.with_extension("tmp-link");
        assert!(!tmp_link.exists(), "temp link should be cleaned up");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_replaces_regular_file() {
        // If the canonical path is a regular file (from an old non-symlink
        // installation), the swap should still work by replacing it.
        let dir = tempfile::tempdir().unwrap();

        let target = dir.path().join("binary-v2");
        std::fs::write(&target, "v2").unwrap();

        let link = dir.path().join("grok");
        // Simulate an old installation where grok is a regular file.
        std::fs::write(&link, "old-binary").unwrap();

        atomic_symlink_swap(&target, &link).await.unwrap();

        assert!(link.is_symlink());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v2");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_succeeds_despite_leftover_tmp_link() {
        // A leftover .tmp-link from a crashed swap must not block a new swap:
        // unique per-racer temp names mean no collision.
        let dir = tempfile::tempdir().unwrap();

        let target_v1 = dir.path().join("binary-v1");
        std::fs::write(&target_v1, "v1").unwrap();
        let target_v2 = dir.path().join("binary-v2");
        std::fs::write(&target_v2, "v2").unwrap();

        let link = dir.path().join("grok");
        std::os::unix::fs::symlink(&target_v1, &link).unwrap();
        std::os::unix::fs::symlink(&target_v1, link.with_extension("tmp-link")).unwrap();

        atomic_symlink_swap(&target_v2, &link).await.unwrap();

        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v2");
    }

    #[cfg(unix)]
    fn managed_layout() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let downloads = dir.path().join("downloads");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&downloads).unwrap();
        (dir, bin, downloads)
    }

    #[test]
    fn test_installer_manages_bin_entrypoints_gate() {
        assert!(installer_manages_bin_entrypoints("internal"));
        assert!(installer_manages_bin_entrypoints("gh-release"));
        assert!(!installer_manages_bin_entrypoints("npm"));
        assert!(!installer_manages_bin_entrypoints("unknown"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_reconcile_agent_repoints_diverged_agent() {
        let (_dir, bin, downloads) = managed_layout();
        std::fs::write(downloads.join("grok-0.2.101-macos-aarch64"), "new").unwrap();
        std::fs::write(downloads.join("grok-0.1.199-macos-aarch64"), "old").unwrap();

        std::os::unix::fs::symlink("../downloads/grok-0.2.101-macos-aarch64", bin.join("grok"))
            .unwrap();
        std::os::unix::fs::symlink("../downloads/grok-0.1.199-macos-aarch64", bin.join("agent"))
            .unwrap();

        reconcile_agent_to_grok(&bin).await;

        assert_eq!(
            std::fs::read_link(bin.join("agent")).unwrap(),
            std::path::PathBuf::from("../downloads/grok-0.2.101-macos-aarch64"),
        );
        assert_eq!(std::fs::read_to_string(bin.join("agent")).unwrap(), "new");
        assert!(downloads.join("grok-0.1.199-macos-aarch64").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_reconcile_agent_heals_legacy_unversioned_agent() {
        let (_dir, bin, downloads) = managed_layout();
        std::fs::write(downloads.join("grok-0.2.101-macos-aarch64"), "new").unwrap();
        std::fs::write(downloads.join("grok-macos-aarch64"), "legacy").unwrap();

        std::os::unix::fs::symlink("../downloads/grok-0.2.101-macos-aarch64", bin.join("grok"))
            .unwrap();
        std::os::unix::fs::symlink("../downloads/grok-macos-aarch64", bin.join("agent")).unwrap();

        reconcile_agent_to_grok(&bin).await;

        assert_eq!(
            std::fs::read_link(bin.join("agent")).unwrap(),
            std::path::PathBuf::from("../downloads/grok-0.2.101-macos-aarch64"),
        );
        assert_eq!(std::fs::read_to_string(bin.join("agent")).unwrap(), "new");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_reconcile_agent_creates_missing_agent() {
        let (_dir, bin, downloads) = managed_layout();
        std::fs::write(downloads.join("grok-0.2.101-macos-aarch64"), "new").unwrap();
        std::os::unix::fs::symlink("../downloads/grok-0.2.101-macos-aarch64", bin.join("grok"))
            .unwrap();

        reconcile_agent_to_grok(&bin).await;

        assert!(bin.join("agent").is_symlink());
        assert_eq!(std::fs::read_to_string(bin.join("agent")).unwrap(), "new");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_reconcile_agent_noop_when_consistent() {
        let (_dir, bin, downloads) = managed_layout();
        std::fs::write(downloads.join("grok-0.2.101-macos-aarch64"), "new").unwrap();
        let target = "../downloads/grok-0.2.101-macos-aarch64";
        std::os::unix::fs::symlink(target, bin.join("grok")).unwrap();
        std::os::unix::fs::symlink(target, bin.join("agent")).unwrap();

        reconcile_agent_to_grok(&bin).await;

        assert_eq!(
            std::fs::read_link(bin.join("agent")).unwrap(),
            std::path::PathBuf::from(target),
        );
        let leftovers = std::fs::read_dir(&bin)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-link"))
            .count();
        assert_eq!(leftovers, 0, "no temp links from a no-op reconcile");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_reconcile_agent_skips_when_grok_dangling() {
        let (_dir, bin, downloads) = managed_layout();
        std::os::unix::fs::symlink("../downloads/grok-0.2.101-macos-aarch64", bin.join("grok"))
            .unwrap();
        std::fs::write(downloads.join("grok-0.1.199-macos-aarch64"), "old").unwrap();
        std::os::unix::fs::symlink("../downloads/grok-0.1.199-macos-aarch64", bin.join("agent"))
            .unwrap();

        reconcile_agent_to_grok(&bin).await;

        assert_eq!(
            std::fs::read_link(bin.join("agent")).unwrap(),
            std::path::PathBuf::from("../downloads/grok-0.1.199-macos-aarch64"),
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_reconcile_agent_skips_when_grok_not_symlink() {
        let (_dir, bin, downloads) = managed_layout();
        std::fs::write(bin.join("grok"), "copy-binary").unwrap();
        std::fs::write(downloads.join("grok-0.1.199-macos-aarch64"), "old").unwrap();
        std::os::unix::fs::symlink("../downloads/grok-0.1.199-macos-aarch64", bin.join("agent"))
            .unwrap();

        reconcile_agent_to_grok(&bin).await;

        assert_eq!(
            std::fs::read_link(bin.join("agent")).unwrap(),
            std::path::PathBuf::from("../downloads/grok-0.1.199-macos-aarch64"),
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_sweep_stale_tmp_links_removes_stale_keeps_fresh_and_active() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("binary-v1");
        std::fs::write(&target, "v1").unwrap();
        let link = dir.path().join("grok");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Old- and new-style leftover temp links.
        let leftover_old = dir.path().join("grok.tmp-link");
        let leftover_new = dir.path().join("grok.123-0.tmp-link");
        std::os::unix::fs::symlink(&target, &leftover_old).unwrap();
        std::os::unix::fs::symlink(&target, &leftover_new).unwrap();

        // max_age = ZERO: every leftover is stale and removed; the active
        // `grok` link (no `.tmp-link` suffix) is untouched.
        sweep_stale_tmp_links(&link, Duration::ZERO).await;
        assert!(!leftover_old.exists() && !leftover_new.exists());
        assert!(link.is_symlink(), "active link must be preserved");

        // A fresh leftover under a real max_age is preserved — it could be a
        // concurrent racer's in-flight link.
        let fresh = dir.path().join("grok.999-9.tmp-link");
        std::os::unix::fs::symlink(&target, &fresh).unwrap();
        sweep_stale_tmp_links(&link, Duration::from_secs(3600)).await;
        assert!(fresh.exists(), "fresh tmp-link must be preserved");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_multiple_sequential_swaps() {
        // Simulate v1 -> v2 -> v3 -> v4 sequential swaps.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("grok");

        for i in 1..=4 {
            let target = dir.path().join(format!("binary-v{}", i));
            std::fs::write(&target, format!("content-v{}", i)).unwrap();
            atomic_symlink_swap(&target, &link).await.unwrap();

            assert!(link.is_symlink());
            assert_eq!(
                std::fs::read_to_string(&link).unwrap(),
                format!("content-v{}", i)
            );
        }

        // All old binaries should still be on disk.
        for i in 1..=4 {
            let target = dir.path().join(format!("binary-v{}", i));
            assert!(target.exists(), "binary-v{} should still exist", i);
        }

        // No temp files should remain.
        let tmp_link = link.with_extension("tmp-link");
        assert!(!tmp_link.exists(), "no temp link should remain");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_with_absolute_target() {
        // atomic_symlink_swap stores whatever path is given — if absolute,
        // readlink returns the absolute path.
        let dir = tempfile::tempdir().unwrap();

        let binary = dir.path().join("grok-0.1.141");
        std::fs::write(&binary, "v141").unwrap();

        let link = dir.path().join("grok");
        atomic_symlink_swap(&binary, &link).await.unwrap();

        assert!(link.is_symlink());
        // readlink returns the absolute path we passed.
        assert_eq!(std::fs::read_link(&link).unwrap(), binary);
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v141");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_with_relative_target() {
        // When given a relative path, the symlink stores a relative target.
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::create_dir_all(&bin).unwrap();

        std::fs::write(downloads.join("grok-0.1.203"), "v203").unwrap();

        let rel_target = std::path::Path::new("../downloads/grok-0.1.203");
        let link = bin.join("grok");
        atomic_symlink_swap(rel_target, &link).await.unwrap();

        assert!(link.is_symlink());
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::path::PathBuf::from("../downloads/grok-0.1.203")
        );
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v203");
    }

    #[cfg(unix)]
    #[test]
    fn test_relative_symlink_target_sibling_dirs() {
        // bin/grok -> ../downloads/grok-0.1.203
        let target = std::path::Path::new("/home/alice/.grok/downloads/grok-0.1.203");
        let link = std::path::Path::new("/home/alice/.grok/bin/grok");
        let result = relative_symlink_target(target, link);
        assert_eq!(
            result,
            std::path::PathBuf::from("../downloads/grok-0.1.203")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_relative_symlink_target_same_dir() {
        // downloads/grok-latest -> grok-0.1.203 (same directory)
        let target = std::path::Path::new("/home/alice/.grok/downloads/grok-0.1.203");
        let link = std::path::Path::new("/home/alice/.grok/downloads/grok-latest");
        let result = relative_symlink_target(target, link);
        assert_eq!(result, std::path::PathBuf::from("grok-0.1.203"));
    }

    #[cfg(unix)]
    #[test]
    fn test_relative_symlink_target_cross_tree_stays_absolute() {
        // /usr/local/bin/grok -> /home/alice/.grok/downloads/grok-0.1.203
        // Different grandparents — should stay absolute.
        let target = std::path::Path::new("/home/alice/.grok/downloads/grok-0.1.203");
        let link = std::path::Path::new("/usr/local/bin/grok");
        let result = relative_symlink_target(target, link);
        assert_eq!(
            result,
            std::path::PathBuf::from("/home/alice/.grok/downloads/grok-0.1.203")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_relative_symlink_survives_directory_move() {
        // Simulates Docker bind-mount: create ~/.grok/ layout at path A,
        // then move it to path B and verify the symlink still resolves.
        let dir = tempfile::tempdir().unwrap();

        // Create alice's layout
        let alice = dir.path().join("alice").join(".grok");
        let alice_downloads = alice.join("downloads");
        let alice_bin = alice.join("bin");
        std::fs::create_dir_all(&alice_downloads).unwrap();
        std::fs::create_dir_all(&alice_bin).unwrap();
        std::fs::write(alice_downloads.join("grok-0.1.203"), "binary-content").unwrap();

        // Create a relative symlink (what the fix produces)
        let rel_target = std::path::Path::new("../downloads/grok-0.1.203");
        let link = alice_bin.join("grok");
        atomic_symlink_swap(rel_target, &link).await.unwrap();

        // Verify it works at the original location
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "binary-content");

        // "Bind-mount" to bob: copy the entire .grok tree
        let bob_home = dir.path().join("bob");
        std::fs::create_dir_all(&bob_home).unwrap();
        let bob = bob_home.join(".grok");
        let copy_status = std::process::Command::new("cp")
            .args(["-a", alice.to_str().unwrap(), bob.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(copy_status.success());

        // Verify the symlink resolves at bob's path too
        let bob_link = bob.join("bin").join("grok");
        assert!(bob_link.is_symlink());
        assert_eq!(
            std::fs::read_link(&bob_link).unwrap(),
            std::path::PathBuf::from("../downloads/grok-0.1.203"),
            "symlink target should be relative"
        );
        assert_eq!(
            std::fs::read_to_string(&bob_link).unwrap(),
            "binary-content",
            "relative symlink should resolve at the new path"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_symlink_swap_broken_symlink_target() {
        // If the current symlink is broken (target deleted externally),
        // the swap should still succeed.
        let dir = tempfile::tempdir().unwrap();

        let link = dir.path().join("grok");
        // Create a broken symlink — points to a file that doesn't exist.
        std::os::unix::fs::symlink(dir.path().join("deleted-binary"), &link).unwrap();
        assert!(link.is_symlink());
        assert!(!link.exists(), "broken symlink should not 'exist'");

        // New target to swap to.
        let target = dir.path().join("binary-v2");
        std::fs::write(&target, "v2").unwrap();

        atomic_symlink_swap(&target, &link).await.unwrap();

        assert!(link.is_symlink());
        assert!(link.exists(), "symlink should now resolve");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "v2");
    }

    #[test]
    fn test_needs_update_prerelease_to_stable_forces_install() {
        // Inadmissible current (pre-release on stable channel) → install even
        // if the candidate is semver-lower.
        assert_eq!(
            needs_update("0.1.149-alpha.1", "0.1.148", "stable", false),
            Some(true)
        );
        assert_eq!(
            needs_update("0.1.148-alpha.3", "0.1.148", "stable", false),
            Some(true)
        );
    }

    #[test]
    fn test_needs_update_stable_to_alpha_no_install_when_candidate_equal() {
        // Server returns max(stable, alpha) for alpha channel. When the user's
        // stable version already IS the candidate, no install needed.
        assert_eq!(
            needs_update("0.1.148", "0.1.148", "alpha", false),
            Some(false)
        );
    }

    #[test]
    fn test_needs_update_stable_channel_never_gets_prerelease() {
        assert_eq!(
            needs_update("0.1.139", "0.1.140-alpha.1", "stable", false),
            Some(false)
        );
        assert_eq!(
            needs_update("0.1.0", "0.1.1-beta.1", "stable", false),
            Some(false)
        );
    }

    #[test]
    fn test_needs_update_valid_current_only_upgrades() {
        // Admissible current on the target channel → pure semver (allow_downgrade=false).
        assert_eq!(
            needs_update("0.1.140", "0.1.141", "stable", false),
            Some(true)
        );
        assert_eq!(
            needs_update("0.1.141", "0.1.140", "stable", false),
            Some(false)
        );
        assert_eq!(
            needs_update("0.1.140-alpha.8", "0.1.140", "alpha", false),
            Some(true)
        );
        assert_eq!(
            needs_update("0.1.140", "0.1.139-alpha.5", "alpha", false),
            Some(false)
        );
        // Alpha → newer alpha: upgrade.
        assert_eq!(
            needs_update("0.1.148-alpha.1", "0.1.148-alpha.3", "alpha", false),
            Some(true)
        );
        // Alpha → older alpha: no downgrade (allow_downgrade=false).
        assert_eq!(
            needs_update("0.1.148-alpha.3", "0.1.148-alpha.2", "alpha", false),
            Some(false)
        );
    }

    #[test]
    fn test_needs_update_large_version_numbers() {
        // Ensure no overflow on realistic version numbers
        assert_eq!(
            needs_update("0.1.140", "0.1.999", "stable", false),
            Some(true)
        );
        assert_eq!(
            needs_update("0.1.999", "0.2.0", "stable", false),
            Some(true)
        );
        assert_eq!(
            needs_update("99.99.99", "100.0.0", "stable", false),
            Some(true)
        );
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_keeps_current_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Simulate 5 old grok binaries in downloads dir.
        for v in ["0.1.140", "0.1.141", "0.1.142", "0.1.143", "0.1.144"] {
            std::fs::write(d.join(format!("grok-{}-macos-aarch64", v)), v).unwrap();
        }
        // Current version.
        std::fs::write(d.join("grok-0.1.145-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.145").await;

        // Current must survive.
        assert!(d.join("grok-0.1.145-macos-aarch64").exists(), "current");
        // Newest old version (0.1.144) must survive.
        assert!(d.join("grok-0.1.144-macos-aarch64").exists(), "N-1");
        // Everything else should be deleted.
        assert!(
            !d.join("grok-0.1.143-macos-aarch64").exists(),
            "0.1.143 should be deleted"
        );
        assert!(
            !d.join("grok-0.1.142-macos-aarch64").exists(),
            "0.1.142 should be deleted"
        );
        assert!(
            !d.join("grok-0.1.141-macos-aarch64").exists(),
            "0.1.141 should be deleted"
        );
        assert!(
            !d.join("grok-0.1.140-macos-aarch64").exists(),
            "0.1.140 should be deleted"
        );
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_does_not_touch_other_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // grok and grok-pager should not interfere with each other.
        std::fs::write(d.join("grok-0.1.140-macos-aarch64"), "old-grok").unwrap();
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current-grok").unwrap();
        std::fs::write(d.join("grok-pager-0.1.140-macos-aarch64"), "old-pager").unwrap();
        std::fs::write(d.join("grok-pager-0.1.141-macos-aarch64"), "current-pager").unwrap();

        // Cleanup only grok — pager files must be untouched.
        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(d.join("grok-0.1.141-macos-aarch64").exists());
        assert!(d.join("grok-0.1.140-macos-aarch64").exists()); // only old, kept as N-1
        assert!(
            d.join("grok-pager-0.1.140-macos-aarch64").exists(),
            "pager untouched"
        );
        assert!(
            d.join("grok-pager-0.1.141-macos-aarch64").exists(),
            "pager untouched"
        );
    }

    /// Backdate a file's mtime past [`STALE_TMP_AGE`] so cleanup treats it
    /// as an abandoned download / genuinely old binary.
    fn make_stale(path: &std::path::Path) {
        let old = std::time::SystemTime::now() - (STALE_TMP_AGE + Duration::from_secs(60));
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }

    /// Backdate every file in `dir`. Cleanup deliberately never deletes a
    /// freshly-written binary or temp file (it may belong to a concurrent
    /// in-flight install), so retention-policy tests must age their fixtures
    /// to look like real leftovers from previous releases.
    fn make_all_stale(dir: &std::path::Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                make_stale(&p);
            }
        }
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_removes_stale_tmp_keeps_fresh_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Stale tmp: abandoned by a crashed updater — swept.
        std::fs::write(d.join("grok-0.1.140-macos-aarch64.tmp"), "partial").unwrap();
        make_stale(&d.join("grok-0.1.140-macos-aarch64.tmp"));
        // Fresh tmp: a concurrent updater's in-flight download — kept, or
        // its atomic rename would fail with ENOENT.
        std::fs::write(d.join("grok-0.1.142-macos-aarch64.77-0.tmp"), "inflight").unwrap();
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current").unwrap();

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(
            !d.join("grok-0.1.140-macos-aarch64.tmp").exists(),
            "stale tmp cleaned up"
        );
        assert!(
            d.join("grok-0.1.142-macos-aarch64.77-0.tmp").exists(),
            "fresh in-flight tmp must NOT be swept"
        );
        assert!(
            d.join("grok-0.1.141-macos-aarch64").exists(),
            "current kept"
        );
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_keeps_fresh_versioned_binary() {
        // A versioned binary written moments ago may be a concurrent
        // installer's just-renamed download whose symlink swap hasn't
        // happened yet — even when the retention policy would otherwise
        // delete it, it must survive until it ages.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Three old versions + current: policy would delete .138 and .139.
        for v in ["0.1.138", "0.1.139", "0.1.140"] {
            std::fs::write(d.join(format!("grok-{v}-macos-aarch64")), v).unwrap();
        }
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current").unwrap();
        make_all_stale(d);
        // .138 is re-written NOW — simulating a racer that just renamed its
        // download into place (e.g. a rollback install racing an upgrade).
        std::fs::write(d.join("grok-0.1.138-macos-aarch64"), "in-flight").unwrap();

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(d.join("grok-0.1.141-macos-aarch64").exists(), "current");
        assert!(d.join("grok-0.1.140-macos-aarch64").exists(), "N-1 kept");
        assert!(
            d.join("grok-0.1.138-macos-aarch64").exists(),
            "fresh just-renamed binary must NOT be deleted"
        );
        assert!(
            !d.join("grok-0.1.139-macos-aarch64").exists(),
            "genuinely old binary still swept"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_cleanup_old_downloads_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // grok-latest is a symlink — must be skipped.
        let target = d.join("grok-0.1.141-macos-aarch64");
        std::fs::write(&target, "current").unwrap();
        std::os::unix::fs::symlink(&target, d.join("grok-latest")).unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(
            d.join("grok-latest").exists(),
            "symlink must not be deleted"
        );
        assert!(target.exists(), "current must not be deleted");
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Should not panic or error on empty directory.
        make_all_stale(dir.path());

        cleanup_old_downloads(dir.path(), "grok", "0.1.141").await;
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_version_prefix_collision() {
        // Regression test: version "0.1.14" must not protect "0.1.140", "0.1.141", etc.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        std::fs::write(d.join("grok-0.1.14-macos-aarch64"), "current").unwrap();
        std::fs::write(d.join("grok-0.1.140-macos-aarch64"), "old-140").unwrap();
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "old-141").unwrap();
        std::fs::write(d.join("grok-0.1.13-macos-aarch64"), "old-13").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.14").await;

        // Current must survive.
        assert!(
            d.join("grok-0.1.14-macos-aarch64").exists(),
            "current 0.1.14"
        );
        // Newest old version (0.1.141) must survive as N-1.
        assert!(
            d.join("grok-0.1.141-macos-aarch64").exists(),
            "N-1 is 0.1.141"
        );
        // 0.1.140 and 0.1.13 should be deleted.
        assert!(
            !d.join("grok-0.1.140-macos-aarch64").exists(),
            "0.1.140 should be deleted"
        );
        assert!(
            !d.join("grok-0.1.13-macos-aarch64").exists(),
            "0.1.13 should be deleted"
        );
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_pager_multi_version() {
        // Verify cleanup works for grok-pager with multiple old versions.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        for v in ["0.1.148", "0.1.149", "0.1.150"] {
            std::fs::write(d.join(format!("grok-pager-{}-linux-x64", v)), v).unwrap();
        }
        std::fs::write(d.join("grok-pager-0.1.151-linux-x64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok-pager", "0.1.151").await;

        assert!(d.join("grok-pager-0.1.151-linux-x64").exists(), "current");
        assert!(d.join("grok-pager-0.1.150-linux-x64").exists(), "N-1 kept");
        assert!(
            !d.join("grok-pager-0.1.149-linux-x64").exists(),
            "0.1.149 deleted"
        );
        assert!(
            !d.join("grok-pager-0.1.148-linux-x64").exists(),
            "0.1.148 deleted"
        );
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_npm_layout() {
        // npm layout: files are just `grok-{version}` (no platform suffix).
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        for v in ["0.1.138", "0.1.139", "0.1.140"] {
            std::fs::write(d.join(format!("grok-{}", v)), v).unwrap();
        }
        std::fs::write(d.join("grok-0.1.141"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(d.join("grok-0.1.141").exists(), "current");
        assert!(d.join("grok-0.1.140").exists(), "N-1 kept");
        assert!(!d.join("grok-0.1.139").exists(), "0.1.139 deleted");
        assert!(!d.join("grok-0.1.138").exists(), "0.1.138 deleted");
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_alpha_versions() {
        // Alpha version filenames include pre-release tags:
        //   grok-0.1.150-alpha.1-macos-aarch64
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        std::fs::write(d.join("grok-0.1.148-alpha.1-macos-aarch64"), "alpha-148-1").unwrap();
        std::fs::write(d.join("grok-0.1.148-alpha.2-macos-aarch64"), "alpha-148-2").unwrap();
        std::fs::write(d.join("grok-0.1.149-alpha.1-macos-aarch64"), "alpha-149-1").unwrap();
        // Current version is the newest alpha.
        std::fs::write(d.join("grok-0.1.150-alpha.1-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.150-alpha.1").await;

        // Current must survive.
        assert!(
            d.join("grok-0.1.150-alpha.1-macos-aarch64").exists(),
            "current alpha"
        );
        // Newest old (0.1.149-alpha.1) kept as N-1.
        assert!(
            d.join("grok-0.1.149-alpha.1-macos-aarch64").exists(),
            "N-1 alpha"
        );
        // Older alphas deleted.
        assert!(
            !d.join("grok-0.1.148-alpha.2-macos-aarch64").exists(),
            "0.1.148-alpha.2 deleted"
        );
        assert!(
            !d.join("grok-0.1.148-alpha.1-macos-aarch64").exists(),
            "0.1.148-alpha.1 deleted"
        );
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_mixed_stable_and_alpha() {
        // Mix of stable and alpha binaries in the same directory.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        std::fs::write(d.join("grok-0.1.148-macos-aarch64"), "stable-148").unwrap();
        std::fs::write(d.join("grok-0.1.149-alpha.1-macos-aarch64"), "alpha-149").unwrap();
        std::fs::write(d.join("grok-0.1.149-macos-aarch64"), "stable-149").unwrap();
        // Current is a stable release.
        std::fs::write(d.join("grok-0.1.150-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.150").await;

        // Current must survive.
        assert!(d.join("grok-0.1.150-macos-aarch64").exists(), "current");
        // Newest old is 0.1.149 stable (semver: 0.1.149 > 0.1.149-alpha.1).
        assert!(
            d.join("grok-0.1.149-macos-aarch64").exists(),
            "N-1 is stable 0.1.149"
        );
        // The rest should be deleted.
        assert!(
            !d.join("grok-0.1.149-alpha.1-macos-aarch64").exists(),
            "alpha 0.1.149-alpha.1 deleted"
        );
        assert!(
            !d.join("grok-0.1.148-macos-aarch64").exists(),
            "stable 0.1.148 deleted"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // reinstall_hint
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn test_reinstall_hint_npm_mentions_npm_command() {
        let hint = reinstall_hint("npm");
        assert!(hint.contains("npm i -g"), "should suggest npm i -g: {hint}");
        assert!(
            hint.contains("@xai-official/grok"),
            "should name the package: {hint}"
        );
    }

    #[test]
    fn test_reinstall_hint_gh_release_mentions_gh_command() {
        let hint = reinstall_hint("gh-release");
        assert!(
            hint.contains("gh release download"),
            "should suggest gh release download: {hint}"
        );
        assert!(
            hint.contains("xai-org-shared/grok-build"),
            "should name the repo: {hint}"
        );
    }

    #[test]
    fn test_reinstall_hint_internal_mentions_platform_installer() {
        let hint = reinstall_hint("internal");
        if cfg!(windows) {
            assert!(hint.contains("irm"), "should suggest irm install: {hint}");
            assert!(
                hint.contains("install.ps1"),
                "should reference install.ps1: {hint}"
            );
        } else {
            assert!(hint.contains("curl"), "should suggest curl install: {hint}");
            assert!(
                hint.contains("install.sh"),
                "should reference install.sh: {hint}"
            );
        }
    }

    #[test]
    fn test_reinstall_hint_unknown_falls_back_to_internal() {
        // Unknown installer falls back to the same hint as "internal".
        let unknown = reinstall_hint("homebrew");
        let internal = reinstall_hint("internal");
        assert_eq!(unknown, internal);
    }

    #[test]
    fn test_reinstall_hint_empty_falls_back_to_internal() {
        let hint = reinstall_hint("");
        assert_eq!(hint, reinstall_hint("internal"));
    }

    // ──────────────────────────────────────────────────────────────────────
    // UpdateStatus serialization (camelCase contract for --json clients)
    // ──────────────────────────────────────────────────────────────────────

    fn make_status() -> UpdateStatus {
        UpdateStatus {
            current_version: "0.1.150".to_string(),
            latest_version: Some("0.1.151".to_string()),
            update_available: true,
            installer: Some("npm".to_string()),
            channel: "stable".to_string(),
            auto_update: Some(true),
            error: None,
        }
    }

    #[test]
    fn test_update_status_serializes_camel_case_keys() {
        let s = make_status();
        let v = serde_json::to_value(&s).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("currentVersion"));
        assert!(obj.contains_key("latestVersion"));
        assert!(obj.contains_key("updateAvailable"));
        assert!(obj.contains_key("installer"));
        assert!(obj.contains_key("channel"));
        assert!(obj.contains_key("autoUpdate"));
        assert!(obj.contains_key("error"));
        // Snake-case names must NOT leak.
        assert!(!obj.contains_key("current_version"));
        assert!(!obj.contains_key("latest_version"));
        assert!(!obj.contains_key("update_available"));
        assert!(!obj.contains_key("auto_update"));
    }

    #[test]
    fn test_update_status_field_values_round_trip_through_json() {
        let s = make_status();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["currentVersion"], "0.1.150");
        assert_eq!(v["latestVersion"], "0.1.151");
        assert_eq!(v["updateAvailable"], true);
        assert_eq!(v["installer"], "npm");
        assert_eq!(v["channel"], "stable");
        assert_eq!(v["autoUpdate"], true);
        assert!(v["error"].is_null());
    }

    #[test]
    fn test_update_status_optional_none_serializes_to_null() {
        let s = UpdateStatus {
            current_version: "0.1.150".to_string(),
            latest_version: None,
            update_available: false,
            installer: None,
            channel: "stable".to_string(),
            auto_update: None,
            error: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert!(v["latestVersion"].is_null());
        assert!(v["installer"].is_null());
        assert!(v["autoUpdate"].is_null());
        assert!(v["error"].is_null());
        assert_eq!(v["updateAvailable"], false);
    }

    #[test]
    fn test_update_status_with_error_field_serialized() {
        let s = UpdateStatus {
            current_version: "0.1.150".to_string(),
            latest_version: None,
            update_available: false,
            installer: Some("npm".to_string()),
            channel: "stable".to_string(),
            auto_update: Some(true),
            error: Some("npm view failed: ENETUNREACH".to_string()),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["error"], "npm view failed: ENETUNREACH");
    }

    #[test]
    fn test_update_status_alpha_channel_serialized() {
        let s = UpdateStatus {
            current_version: "0.1.150-alpha.1".to_string(),
            latest_version: Some("0.1.150-alpha.2".to_string()),
            update_available: true,
            installer: Some("npm".to_string()),
            channel: "alpha".to_string(),
            auto_update: Some(true),
            error: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["channel"], "alpha");
        assert_eq!(v["currentVersion"], "0.1.150-alpha.1");
        assert_eq!(v["latestVersion"], "0.1.150-alpha.2");
    }

    #[test]
    fn test_update_status_json_is_valid_single_object() {
        // Whatever we add to UpdateStatus in the future, the serialization
        // must remain a single JSON object (not an array, primitive, etc.).
        let s = make_status();
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.starts_with('{'), "must be a JSON object: {json}");
        assert!(json.ends_with('}'), "must be a JSON object: {json}");
        // Single line: no embedded newlines (the wire format is one line).
        assert!(!json.contains('\n'), "must be single line: {json}");
    }

    // ──────────────────────────────────────────────────────────────────────
    // print_update_status — exercise both code paths via JSON serialization
    // (the human path writes to stdout/stderr which is hard to capture
    //  without altering the function signature).
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn test_print_update_status_json_returns_ok() {
        let s = make_status();
        // We can't easily capture stdout, but we can confirm the function
        // doesn't panic or return Err on a well-formed status.
        print_update_status(&s, true).unwrap();
    }

    #[test]
    fn test_print_update_status_human_returns_ok_when_update_available() {
        let s = make_status();
        print_update_status(&s, false).unwrap();
    }

    #[test]
    fn test_print_update_status_human_returns_ok_when_no_installer() {
        let s = UpdateStatus {
            current_version: "0.1.150".to_string(),
            latest_version: None,
            update_available: false,
            installer: None,
            channel: "stable".to_string(),
            auto_update: None,
            error: None,
        };
        print_update_status(&s, false).unwrap();
    }

    #[test]
    fn test_print_update_status_human_returns_ok_with_error() {
        let s = UpdateStatus {
            current_version: "0.1.150".to_string(),
            latest_version: None,
            update_available: false,
            installer: Some("npm".to_string()),
            channel: "stable".to_string(),
            auto_update: Some(true),
            error: Some("network down".to_string()),
        };
        print_update_status(&s, false).unwrap();
    }

    #[test]
    fn test_print_update_status_human_returns_ok_when_up_to_date() {
        let s = UpdateStatus {
            current_version: "0.1.150".to_string(),
            latest_version: Some("0.1.150".to_string()),
            update_available: false,
            installer: Some("npm".to_string()),
            channel: "stable".to_string(),
            auto_update: Some(true),
            error: None,
        };
        print_update_status(&s, false).unwrap();
    }

    // ──────────────────────────────────────────────────────────────────────
    // needs_update — additional edge cases
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn test_needs_update_empty_current_returns_none() {
        assert_eq!(needs_update("", "0.1.141", "stable", false), None);
    }

    #[test]
    fn test_needs_update_empty_latest_returns_none() {
        assert_eq!(needs_update("0.1.141", "", "stable", false), None);
    }

    #[test]
    fn test_needs_update_whitespace_returns_none() {
        // Leading/trailing whitespace is not stripped — semver::parse rejects.
        assert_eq!(needs_update("  0.1.141", "0.1.142", "stable", false), None);
        assert_eq!(needs_update("0.1.141", "0.1.142  ", "stable", false), None);
    }

    #[test]
    fn test_needs_update_channel_is_case_sensitive() {
        // "STABLE", "Stable", "ENTERPRISE" etc. are not recognized — must be exact lowercase.
        assert_eq!(needs_update("0.1.140", "0.1.141", "STABLE", false), None);
        assert_eq!(needs_update("0.1.140", "0.1.141", "Stable", false), None);
        assert_eq!(needs_update("0.1.140", "0.1.141", "ALPHA", false), None);
        assert_eq!(
            needs_update("0.1.140", "0.1.141", "ENTERPRISE", false),
            None
        );
    }

    #[test]
    fn test_needs_update_unknown_channels_return_none() {
        // Unknown channels (not stable/alpha/enterprise) return None.
        assert_eq!(needs_update("0.1.140", "0.1.141", "beta", false), None);
        assert_eq!(needs_update("0.1.140", "0.1.141", "nightly", false), None);
        assert_eq!(needs_update("0.1.140", "0.1.141", "", false), None);
        assert_eq!(needs_update("0.1.140", "0.1.141", "rc", false), None);
        // Enterprise is explicitly supported (behaves like stable).
        assert_eq!(
            needs_update("0.1.140", "0.1.141", "enterprise", false),
            Some(true)
        );
        // Unknown channels return None regardless of allow_downgrade.
        assert_eq!(needs_update("0.1.140", "0.1.141", "beta", true), None);
        assert_eq!(needs_update("0.1.140", "0.1.141", "", true), None);
    }

    #[test]
    fn test_needs_update_zero_versions() {
        assert_eq!(needs_update("0.0.0", "0.0.1", "stable", false), Some(true));
        assert_eq!(needs_update("0.0.0", "0.0.0", "stable", false), Some(false));
    }

    #[test]
    fn test_needs_update_major_version_jump() {
        assert_eq!(needs_update("0.9.99", "1.0.0", "stable", false), Some(true));
        assert_eq!(
            needs_update("1.99.99", "2.0.0", "stable", false),
            Some(true)
        );
        // Major downgrade: not an upgrade (allow_downgrade=false).
        assert_eq!(
            needs_update("2.0.0", "1.99.99", "stable", false),
            Some(false)
        );
    }

    #[test]
    fn test_needs_update_alpha_to_alpha_same_version_not_upgrade() {
        assert_eq!(
            needs_update("0.1.150-alpha.5", "0.1.150-alpha.5", "alpha", false),
            Some(false)
        );
    }

    #[test]
    fn test_needs_update_alpha_to_beta_same_base_is_upgrade_per_semver() {
        // semver: alpha.5 < beta.1 (lexicographic on identifiers per spec)
        assert_eq!(
            needs_update("0.1.150-alpha.5", "0.1.150-beta.1", "alpha", false),
            Some(true)
        );
    }

    #[test]
    fn test_needs_update_with_build_metadata_uses_semver_crate_ordering() {
        // SUBTLE: per the semver SPEC, build metadata (after `+`) MUST be
        // ignored when determining version precedence. However the `semver`
        // crate's `PartialOrd` impl compares build metadata lexicographically
        // for differing values. So `0.1.141+xyz > 0.1.141+abc` returns true
        // here even though spec-wise they are equal.
        //
        // This means CI publishers MUST NOT publish multiple builds of the
        // same version differing only in build metadata, or auto-update will
        // bounce users between them. Today our pipeline doesn't, so this is
        // latent — but the test locks in the surprising behavior so it can't
        // change silently.
        assert_eq!(
            needs_update("0.1.141+abc", "0.1.141+xyz", "stable", false),
            Some(true),
            "semver crate orders by build metadata lexicographically (contra spec)"
        );
        // No build metadata vs with build metadata: semver crate treats
        // a version with build > the same version without it.
        assert_eq!(
            needs_update("0.1.141", "0.1.141+abc", "stable", false),
            Some(true)
        );
    }

    #[test]
    fn test_needs_update_partial_versions_rejected() {
        assert_eq!(needs_update("0.1", "0.1.141", "stable", false), None);
        assert_eq!(needs_update("0", "0.1.141", "stable", false), None);
        assert_eq!(needs_update("0.1.141", "1", "stable", false), None);
    }

    #[test]
    fn test_needs_update_alpha_channel_with_invalid_versions_returns_none() {
        // Same parse-failure behavior on alpha as stable.
        assert_eq!(needs_update("garbage", "0.1.141", "alpha", false), None);
        assert_eq!(needs_update("0.1.141", "garbage", "alpha", false), None);
    }

    #[test]
    fn test_needs_update_alpha_channel_treats_release_as_higher_than_prerelease() {
        // On alpha channel, a release version is semver-higher than its
        // matching pre-release: 0.1.150 > 0.1.150-alpha.99.
        assert_eq!(
            needs_update("0.1.150-alpha.99", "0.1.150", "alpha", false),
            Some(true)
        );
    }

    #[test]
    fn test_needs_update_stable_does_not_install_when_pre_and_pre() {
        // current is pre-release, latest is also pre-release on stable channel:
        // latest is rejected as pre-release, so no install.
        assert_eq!(
            needs_update("0.1.150-alpha.1", "0.1.151-alpha.1", "stable", false),
            Some(false)
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // needs_update — allow_downgrade=true (rollback support)
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn test_needs_update_downgrade_stable_when_allowed() {
        // Rollback scenario: stable pointer moved from 0.2.7 → 0.2.5.
        // GCS/internal installer: allow_downgrade=true → triggers update.
        assert_eq!(needs_update("0.2.7", "0.2.5", "stable", true), Some(true));
    }

    #[test]
    fn test_needs_update_downgrade_stable_blocked_when_disallowed() {
        // Same rollback scenario but npm installer: allow_downgrade=false → no update.
        assert_eq!(needs_update("0.2.7", "0.2.5", "stable", false), Some(false));
    }

    #[test]
    fn test_needs_update_downgrade_alpha_when_allowed() {
        // Alpha rollback: pointer moved backward.
        assert_eq!(needs_update("0.2.7", "0.2.5", "alpha", true), Some(true));
        // Alpha pre-release downgrade.
        assert_eq!(
            needs_update("0.1.148-alpha.3", "0.1.148-alpha.2", "alpha", true),
            Some(true)
        );
    }

    #[test]
    fn test_needs_update_downgrade_enterprise_when_allowed() {
        assert_eq!(
            needs_update("0.1.207", "0.1.206", "enterprise", true),
            Some(true)
        );
    }

    #[test]
    fn test_needs_update_same_version_unaffected_by_allow_downgrade() {
        // Same version → no update regardless of allow_downgrade setting.
        assert_eq!(needs_update("0.2.5", "0.2.5", "stable", true), Some(false));
        assert_eq!(needs_update("0.2.5", "0.2.5", "stable", false), Some(false));
        assert_eq!(needs_update("0.2.5", "0.2.5", "alpha", true), Some(false));
    }

    #[test]
    fn test_needs_update_upgrade_unaffected_by_allow_downgrade() {
        // Upgrade works regardless of allow_downgrade setting.
        assert_eq!(needs_update("0.2.5", "0.2.7", "stable", true), Some(true));
        assert_eq!(needs_update("0.2.5", "0.2.7", "stable", false), Some(true));
        assert_eq!(needs_update("0.2.5", "0.2.7", "alpha", true), Some(true));
        assert_eq!(needs_update("0.2.5", "0.2.7", "alpha", false), Some(true));
    }

    #[test]
    fn test_needs_update_downgrade_major_version_when_allowed() {
        // Major version downgrade (e.g. v2 → v1 rollback).
        assert_eq!(needs_update("2.0.0", "1.99.99", "stable", true), Some(true));
    }

    #[test]
    fn test_needs_update_downgrade_prerelease_still_rejected_on_stable() {
        // Even with allow_downgrade=true, pre-release targets are rejected on
        // stable/enterprise channels (safety net).
        assert_eq!(
            needs_update("0.2.7", "0.2.5-alpha.1", "stable", true),
            Some(false)
        );
        assert_eq!(
            needs_update("0.2.7", "0.2.5-alpha.1", "enterprise", true),
            Some(false)
        );
    }

    #[test]
    fn test_needs_update_prerelease_current_forces_install_regardless_of_allow_downgrade() {
        // Pre-release current on stable channel → force-install, independent
        // of allow_downgrade.
        assert_eq!(
            needs_update("0.1.149-alpha.1", "0.1.148", "stable", true),
            Some(true)
        );
        assert_eq!(
            needs_update("0.1.149-alpha.1", "0.1.148", "stable", false),
            Some(true)
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // installer_allows_downgrade
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn test_installer_allows_downgrade_internal() {
        assert!(installer_allows_downgrade("internal"));
    }

    #[test]
    fn test_installer_allows_downgrade_gh_release() {
        assert!(installer_allows_downgrade("gh-release"));
    }

    #[test]
    fn test_installer_allows_downgrade_npm_blocked() {
        // npm registries can return stale/misconfigured versions — no downgrade.
        assert!(!installer_allows_downgrade("npm"));
    }

    #[test]
    fn test_installer_allows_downgrade_unknown_blocked() {
        assert!(!installer_allows_downgrade("unknown"));
        assert!(!installer_allows_downgrade(""));
        assert!(!installer_allows_downgrade("homebrew"));
    }

    // ──────────────────────────────────────────────────────────────────────
    // detect_platform
    // ──────────────────────────────────────────────────────────────────────

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn test_detect_platform_returns_known_os() {
        let (os, arch) = detect_platform().unwrap();
        assert!(
            os == "macos" || os == "linux" || os == "windows",
            "got os={os}"
        );
        assert!(arch == "x86_64" || arch == "aarch64", "got arch={arch}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn test_detect_platform_matches_compile_time_cfg() {
        let (os, arch) = detect_platform().unwrap();
        if cfg!(target_os = "macos") {
            assert_eq!(os, "macos");
        }
        if cfg!(target_os = "linux") {
            assert_eq!(os, "linux");
        }
        if cfg!(target_os = "windows") {
            assert_eq!(os, "windows");
        }
        if cfg!(target_arch = "x86_64") {
            assert_eq!(arch, "x86_64");
        }
        if cfg!(target_arch = "aarch64") {
            assert_eq!(arch, "aarch64");
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // cleanup_old_downloads — additional edge cases
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_cleanup_old_downloads_invalid_current_version_is_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("grok-0.1.140-macos-aarch64"), "v140").unwrap();
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "v141").unwrap();

        // Invalid version string → cleanup must early-return without deleting.
        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "not-a-version").await;
        assert!(d.join("grok-0.1.140-macos-aarch64").exists());
        assert!(d.join("grok-0.1.141-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_missing_dir_no_panic() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        // Must not panic when the directory doesn't exist.
        cleanup_old_downloads(&missing, "grok", "0.1.141").await;
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_files_with_non_digit_suffix_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // Files matching prefix but with a non-digit-leading suffix must be
        // ignored (e.g. grok-latest, grok-pager-* when prefix is grok).
        std::fs::write(d.join("grok-latest"), "alias").unwrap();
        std::fs::write(d.join("grok-pager-0.1.141-macos-aarch64"), "pager").unwrap();
        std::fs::write(d.join("grok-0.1.140-macos-aarch64"), "v140").unwrap();
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        // grok-latest and grok-pager-* must be untouched.
        assert!(d.join("grok-latest").exists());
        assert!(d.join("grok-pager-0.1.141-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_unparseable_version_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // Files with prefix + digit but unparseable as semver are ignored
        // (not deleted, not counted).
        std::fs::write(d.join("grok-9garbage-macos-aarch64"), "junk").unwrap();
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(
            d.join("grok-9garbage-macos-aarch64").exists(),
            "unparseable file must be ignored, not deleted"
        );
        assert!(d.join("grok-0.1.141-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_only_current_present_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(d.join("grok-0.1.141-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_only_one_old_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("grok-0.1.140-macos-aarch64"), "v140").unwrap();
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        // Only one old version → keep it as N-1.
        assert!(d.join("grok-0.1.140-macos-aarch64").exists(), "N-1 kept");
        assert!(d.join("grok-0.1.141-macos-aarch64").exists(), "current");
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_unrelated_files_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // Files that don't start with the prefix must never be touched.
        std::fs::write(d.join("README.md"), "readme").unwrap();
        std::fs::write(d.join("config.toml"), "config").unwrap();
        std::fs::write(d.join("other-tool-0.1.0"), "other").unwrap();
        std::fs::write(d.join("grok-0.1.140-macos-aarch64"), "v140").unwrap();
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(d.join("README.md").exists());
        assert!(d.join("config.toml").exists());
        assert!(d.join("other-tool-0.1.0").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_multiplatform_in_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // Same version, multiple platforms (uncommon, but possible).
        // Both should be considered "current" via the version equality check.
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "mac").unwrap();
        std::fs::write(d.join("grok-0.1.141-linux-x86_64"), "linux").unwrap();
        std::fs::write(d.join("grok-0.1.140-macos-aarch64"), "old-mac").unwrap();
        std::fs::write(d.join("grok-0.1.139-macos-aarch64"), "older-mac").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        // Both platform variants of current must survive.
        assert!(d.join("grok-0.1.141-macos-aarch64").exists());
        assert!(d.join("grok-0.1.141-linux-x86_64").exists());
        // N-1 (0.1.140) kept, older deleted.
        assert!(d.join("grok-0.1.140-macos-aarch64").exists());
        assert!(!d.join("grok-0.1.139-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_tmp_files_deleted_even_when_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // Stale tmp files are deleted regardless of version-parseability.
        std::fs::write(d.join("grok-junk.tmp"), "partial").unwrap();
        make_stale(&d.join("grok-junk.tmp"));
        std::fs::write(d.join("grok-0.1.140-macos-aarch64.tmp"), "partial2").unwrap();
        make_stale(&d.join("grok-0.1.140-macos-aarch64.tmp"));
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(!d.join("grok-junk.tmp").exists(), "junk tmp deleted");
        assert!(
            !d.join("grok-0.1.140-macos-aarch64.tmp").exists(),
            "versioned tmp deleted"
        );
        assert!(d.join("grok-0.1.141-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_three_olds_keeps_only_newest() {
        // Regression: keep exactly N-1, not N-2 or older.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for v in ["0.1.138", "0.1.139", "0.1.140"] {
            std::fs::write(d.join(format!("grok-{}-macos-aarch64", v)), v).unwrap();
        }
        std::fs::write(d.join("grok-0.1.141-macos-aarch64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(d.join("grok-0.1.141-macos-aarch64").exists(), "current");
        assert!(d.join("grok-0.1.140-macos-aarch64").exists(), "N-1 only");
        assert!(!d.join("grok-0.1.139-macos-aarch64").exists());
        assert!(!d.join("grok-0.1.138-macos-aarch64").exists());
    }

    #[tokio::test]
    async fn test_cleanup_old_downloads_darwin_platform_recognized() {
        // The `darwin` alias for macOS is in PLATFORM_OS — versions on
        // grok-X.Y.Z-darwin-* layouts must split correctly.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("grok-0.1.140-darwin-arm64"), "v140").unwrap();
        std::fs::write(d.join("grok-0.1.141-darwin-arm64"), "current").unwrap();

        make_all_stale(d);

        cleanup_old_downloads(d, "grok", "0.1.141").await;

        assert!(d.join("grok-0.1.141-darwin-arm64").exists(), "current");
        assert!(d.join("grok-0.1.140-darwin-arm64").exists(), "N-1");
    }

    // ──────────────────────────────────────────────────────────────────────
    // ──────────────────────────────────────────────────────────────────────
    // UpdateRunMode
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn test_update_run_mode_is_copy_clone_debug() {
        // The ergonomic Copy/Clone/Debug derives must not regress: we pass
        // `run_mode` by value through several layers.
        let m1 = UpdateRunMode::Blocking;
        let m2 = m1; // Copy
        let m3 = m1; // Copy again, m1 not moved
        assert!(matches!(m1, UpdateRunMode::Blocking));
        assert!(matches!(m2, UpdateRunMode::Blocking));
        assert!(matches!(m3, UpdateRunMode::Blocking));
        // Debug exists.
        let _ = format!("{:?}", UpdateRunMode::NonBlocking);
    }

    // ──────────────────────────────────────────────────────────────────────
    // Constants — lock them in so silent renames are caught.
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn test_user_facing_constants_are_stable() {
        assert_eq!(PROMPT_UPDATE_NOW, "Update now? [Y/n/d]");
        assert_eq!(
            MSG_AUTO_UPDATE_BACKGROUND,
            "Auto-update running in background."
        );
        assert_eq!(
            MSG_RUN_UPDATE_MANUAL,
            "Run `grok update` to get the latest version."
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // env_installer — env-var based, must run serially.
    //
    // Resolution order (matches function body):
    //   1. GROK_INSTALLER (npm | internal | gh-release | gh)
    //   2. GROK_MANAGED_BY_NPM       → npm
    //   3. GROK_MANAGED_BY_INTERNAL  → internal
    //   4. npm_config_user_agent      → npm
    //   5. None
    // ──────────────────────────────────────────────────────────────────────

    /// Snapshot every installer-related env var so the test can clear them
    /// at start and restore them at end. Without this, a parent shell that
    /// sets e.g. `npm_config_user_agent` (which happens whenever you run via
    /// `npm run`) silently makes every "no env vars" test misbehave.
    struct InstallerEnvGuard {
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl InstallerEnvGuard {
        fn isolate() -> Self {
            const VARS: &[&str] = &[
                "GROK_INSTALLER",
                "GROK_MANAGED_BY_NPM",
                "GROK_MANAGED_BY_INTERNAL",
                "npm_config_user_agent",
                "NPM_TOKEN",
            ];
            let prev: Vec<_> = VARS.iter().map(|k| (*k, std::env::var_os(k))).collect();
            unsafe {
                for k in VARS {
                    std::env::remove_var(k);
                }
            }
            Self { prev }
        }
    }

    impl Drop for InstallerEnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (k, v) in &self.prev {
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_no_vars_returns_none() {
        let _g = InstallerEnvGuard::isolate();
        assert_eq!(env_installer(), None);
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_explicit_npm() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_INSTALLER", "npm") };
        assert_eq!(env_installer(), Some("npm"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_explicit_internal() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_INSTALLER", "internal") };
        assert_eq!(env_installer(), Some("internal"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_explicit_gh_release() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_INSTALLER", "gh-release") };
        assert_eq!(env_installer(), Some("gh-release"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_explicit_gh_alias() {
        // `gh` is shorthand for `gh-release`.
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_INSTALLER", "gh") };
        assert_eq!(env_installer(), Some("gh-release"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_explicit_uppercase_normalized() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_INSTALLER", "NPM") };
        assert_eq!(env_installer(), Some("npm"));

        unsafe { std::env::set_var("GROK_INSTALLER", "Gh-Release") };
        assert_eq!(env_installer(), Some("gh-release"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_explicit_unknown_value_returns_none() {
        // CRITICAL: when the explicit env var is set to something we don't
        // recognize, we early-return None. This means we do NOT fall through
        // to the other env vars or to config. So `GROK_INSTALLER=brew`
        // disables the env-installer detection entirely.
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_INSTALLER", "brew") };
        // Even if MANAGED_BY_NPM is also set, the explicit var wins (and rejects).
        unsafe { std::env::set_var("GROK_MANAGED_BY_NPM", "1") };
        assert_eq!(
            env_installer(),
            None,
            "explicit GROK_INSTALLER=brew must early-return None, not fall through"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_explicit_empty_returns_none() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_INSTALLER", "") };
        assert_eq!(env_installer(), None);
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_managed_by_npm() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_MANAGED_BY_NPM", "1") };
        assert_eq!(env_installer(), Some("npm"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_managed_by_npm_any_value() {
        // The check is `is_some` — any value (including empty) wins.
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_MANAGED_BY_NPM", "") };
        assert_eq!(env_installer(), Some("npm"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_managed_by_internal() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("GROK_MANAGED_BY_INTERNAL", "1") };
        assert_eq!(env_installer(), Some("internal"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_npm_config_user_agent_implies_npm() {
        // npm sets npm_config_user_agent in the env of any process it spawns.
        // The trampoline relies on this fallback when MANAGED_BY_NPM was lost.
        let _g = InstallerEnvGuard::isolate();
        unsafe {
            std::env::set_var(
                "npm_config_user_agent",
                "npm/10.2.0 node/v20.11.0 darwin arm64 workspaces/false",
            )
        };
        assert_eq!(env_installer(), Some("npm"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_managed_by_npm_wins_over_npm_config_user_agent() {
        // Both set: the order in env_installer is MANAGED_BY_NPM checked first,
        // so MANAGED_BY_NPM wins. (Result is the same — both → npm — but the
        // resolution path matters for future maintainers.)
        let _g = InstallerEnvGuard::isolate();
        unsafe {
            std::env::set_var("GROK_MANAGED_BY_NPM", "1");
            std::env::set_var("npm_config_user_agent", "npm/10");
        }
        assert_eq!(env_installer(), Some("npm"));
    }

    #[test]
    #[serial_test::serial]
    fn test_env_installer_explicit_internal_wins_over_npm_managed() {
        // GROK_INSTALLER=internal must override an inherited MANAGED_BY_NPM.
        let _g = InstallerEnvGuard::isolate();
        unsafe {
            std::env::set_var("GROK_INSTALLER", "internal");
            std::env::set_var("GROK_MANAGED_BY_NPM", "1");
        }
        assert_eq!(env_installer(), Some("internal"));
    }

    // ──────────────────────────────────────────────────────────────────────
    // create_temp_npmrc — also env-var based (NPM_TOKEN), must run serially.
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_no_token_returns_none() {
        let _g = InstallerEnvGuard::isolate();
        let result = create_temp_npmrc(None).unwrap();
        assert!(result.is_none(), "no NPM_TOKEN must yield None");
    }

    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_empty_token_returns_none() {
        // An empty token is not a real token — must not write a file.
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("NPM_TOKEN", "") };
        let result = create_temp_npmrc(None).unwrap();
        assert!(result.is_none(), "empty NPM_TOKEN must yield None");
    }

    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_whitespace_only_token_returns_none() {
        // Whitespace-only is treated as empty after trim.
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("NPM_TOKEN", "   \t\n  ") };
        let result = create_temp_npmrc(None).unwrap();
        assert!(result.is_none(), "whitespace NPM_TOKEN must yield None");
    }

    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_default_registry() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("NPM_TOKEN", "secret123") };
        let path = create_temp_npmrc(None).unwrap().expect("file written");
        let body = std::fs::read_to_string(&path).unwrap();

        assert!(
            body.contains("registry.npmjs.org"),
            "default registry: {body}"
        );
        assert!(body.contains("_authToken=secret123"), "token: {body}");
        assert!(body.starts_with("//"), "must be // prefix: {body}");
        assert!(body.ends_with('\n'), "must end with newline: {body}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_token_trimmed() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("NPM_TOKEN", "  padded-token  ") };
        let path = create_temp_npmrc(None).unwrap().expect("file written");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("_authToken=padded-token"),
            "token must be trimmed: {body}"
        );
        assert!(
            !body.contains("padded-token  "),
            "trailing whitespace must be stripped: {body}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_custom_registry_extracts_host_and_path() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("NPM_TOKEN", "tok") };
        let path = create_temp_npmrc(Some("https://npm.example.com/repository/npm/"))
            .unwrap()
            .expect("file written");
        let body = std::fs::read_to_string(&path).unwrap();

        // Host + path must be preserved (trailing slash stripped per impl).
        assert!(
            body.contains("npm.example.com/repository/npm"),
            "registry host+path: {body}"
        );
        assert!(body.contains("_authToken=tok"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_custom_registry_with_port() {
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("NPM_TOKEN", "tok") };
        let path = create_temp_npmrc(Some("https://npm.example.com:8443/"))
            .unwrap()
            .expect("file written");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("npm.example.com:8443"),
            "port must be preserved: {body}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_invalid_registry_url_falls_back_to_default() {
        // If the registry string doesn't parse as a URL, fall back to the
        // public npm host so the auth token isn't silently lost.
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("NPM_TOKEN", "tok") };
        let path = create_temp_npmrc(Some("not a url"))
            .unwrap()
            .expect("file written");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("registry.npmjs.org"),
            "invalid URL falls back: {body}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_file_perms_are_0600() {
        // The file contains an auth token — must be readable only by owner.
        use std::os::unix::fs::PermissionsExt;
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("NPM_TOKEN", "secret") };
        let path = create_temp_npmrc(None).unwrap().expect("file written");

        let perms = std::fs::metadata(&path).unwrap().permissions();
        let mode = perms.mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "npmrc must be 0600 to protect the auth token, got {mode:o}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[serial_test::serial]
    fn test_create_temp_npmrc_unique_path_per_pid() {
        // Two parallel installs would clobber each other if the path didn't
        // include the PID. Verify the filename includes the current PID.
        let _g = InstallerEnvGuard::isolate();
        unsafe { std::env::set_var("NPM_TOKEN", "tok") };
        let path = create_temp_npmrc(None).unwrap().expect("file written");
        let pid = std::process::id().to_string();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.contains(&pid),
            "filename should include PID: {name} (pid={pid})"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ──────────────────────────────────────────────────────────────────────
    // windows_replace_exe — runs only on Windows CI
    // ──────────────────────────────────────────────────────────────────────

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_creates_dest_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new-binary.exe");
        std::fs::write(&src, "new content").unwrap();
        let dest = dir.path().join("grok.exe");

        windows_replace_exe(&src, &dest).await.unwrap();

        assert!(dest.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"new content");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_overwrites_unlocked_dest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new-binary.exe");
        std::fs::write(&src, "new content").unwrap();
        let dest = dir.path().join("grok.exe");
        std::fs::write(&dest, "old content").unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"new content");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_preserves_binary_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let body: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let src = dir.path().join("binary.exe");
        std::fs::write(&src, &body).unwrap();
        let dest = dir.path().join("grok.exe");

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_cleans_stale_old_backup() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "new").unwrap();
        let dest = dir.path().join("grok.exe");
        std::fs::write(&dest, "current").unwrap();
        let old = dir.path().join("grok.exe.old");
        std::fs::write(&old, "stale-from-prior-update").unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(!old.exists(), "stale .old must be removed");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_no_filename_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.exe");
        std::fs::write(&src, "data").unwrap();

        let bad_dest = dir.path().join("..");
        let err = windows_replace_exe(&src, &bad_dest).await.unwrap_err();
        assert!(format!("{err:#}").contains("no filename"), "error: {err:#}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_locked_file_renames_aside() {
        // Simulate a running .exe: blocks writes but allows rename.
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x00000001;
        const FILE_SHARE_DELETE: u32 = 0x00000004;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "updated binary").unwrap();
        let dest = dir.path().join("grok.exe");
        std::fs::write(&dest, "running binary").unwrap();

        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&dest)
            .unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "updated binary");

        let old = dir.path().join("grok.exe.old");
        assert!(old.exists(), ".old must exist after rename fallback");
        drop(_lock);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "running binary");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_rollback_on_copy_failure() {
        // No stale .old: the aside IS grok.exe.old, so this pins the
        // non-diverted rollback branch (rename .old back onto dest).
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x00000001;
        const FILE_SHARE_DELETE: u32 = 0x00000004;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "updated binary").unwrap();
        let dest = dir.path().join("grok.exe");
        std::fs::write(&dest, "original").unwrap();

        // Dest locked like a running exe: blocks writes but allows rename.
        let _dest_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&dest)
            .unwrap();
        // Exclusive src lock: both copies fail with a sharing violation, so
        // the rename runs and the second copy triggers the rollback.
        let _src_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&src)
            .unwrap();

        let result = windows_replace_exe(&src, &dest).await;
        drop(_src_lock);
        drop(_dest_lock);

        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "original",
            "rollback must restore the original binary"
        );
        let old = dir.path().join("grok.exe.old");
        assert!(!old.exists(), "rollback must consume the .old aside");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_idempotent_same_content() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("binary.exe");
        std::fs::write(&src, "same content").unwrap();
        let dest = dir.path().join("grok.exe");
        std::fs::write(&dest, "same content").unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "same content");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_empty_binary() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("empty.exe");
        std::fs::write(&src, b"").unwrap();
        let dest = dir.path().join("grok.exe");
        std::fs::write(&dest, "non-empty").unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 0);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_locked_stale_old_does_not_block_update() {
        // A leftover .old can still be a running image (the session live
        // during the previous update): undeletable, so the rename must
        // divert to a unique aside instead of failing on the locked name.
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x00000001;
        const FILE_SHARE_DELETE: u32 = 0x00000004;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "updated binary").unwrap();
        let dest = dir.path().join("grok.exe");
        std::fs::write(&dest, "running binary").unwrap();
        let old = dir.path().join("grok.exe.old");
        std::fs::write(&old, "previous binary").unwrap();

        // No FILE_SHARE_DELETE: .old cannot be deleted or rename-replaced.
        let _old_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&old)
            .unwrap();
        // Dest locked like a running exe: blocks writes but allows rename.
        let _dest_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&dest)
            .unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "updated binary");
        assert_eq!(
            std::fs::read_to_string(&old).unwrap(),
            "previous binary",
            "locked .old must be left in place"
        );
        let asides: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("grok.exe.old.") && n.ends_with(".old"))
            })
            .collect();
        assert_eq!(
            asides.len(),
            1,
            "dest must be renamed to a unique aside: {asides:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&asides[0]).unwrap(),
            "running binary"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_rollback_restores_from_diverted_aside() {
        // Copy failure after a divert must roll dest back from the unique
        // aside, not the hardcoded .old (which still holds the locked image).
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x00000001;
        const FILE_SHARE_DELETE: u32 = 0x00000004;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "updated binary").unwrap();
        let dest = dir.path().join("grok.exe");
        std::fs::write(&dest, "running binary").unwrap();
        let old = dir.path().join("grok.exe.old");
        std::fs::write(&old, "previous binary").unwrap();

        // No FILE_SHARE_DELETE: .old survives the sweep and forces a divert.
        let _old_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&old)
            .unwrap();
        // Dest locked like a running exe: blocks writes but allows rename.
        let _dest_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&dest)
            .unwrap();
        // Exclusive src lock: both copies fail with a sharing violation, so
        // the rename dance runs and the second copy triggers the rollback.
        let _src_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&src)
            .unwrap();

        let result = windows_replace_exe(&src, &dest).await;

        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "running binary",
            "rollback must restore dest from the diverted aside"
        );
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "previous binary");
        let leftover_asides = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("grok.exe.old.") && name.ends_with(".old")
            })
            .count();
        assert_eq!(leftover_asides, 0, "rollback must consume the aside");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_windows_replace_exe_sweeps_accumulated_asides() {
        // Asides pile up while superseded sessions keep running; a later
        // update must collect the no-longer-locked ones — but never another
        // executable's leftovers.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new.exe");
        std::fs::write(&src, "new").unwrap();
        let dest = dir.path().join("grok.exe");
        std::fs::write(&dest, "current").unwrap();
        let old = dir.path().join("grok.exe.old");
        std::fs::write(&old, "stale").unwrap();
        let aside_a = dir.path().join("grok.exe.old.1234-0.old");
        let aside_b = dir.path().join("grok.exe.old.1234-1.old");
        std::fs::write(&aside_a, "aside-a").unwrap();
        std::fs::write(&aside_b, "aside-b").unwrap();
        let agent_old = dir.path().join("agent.exe.old");
        std::fs::write(&agent_old, "agent-old").unwrap();

        windows_replace_exe(&src, &dest).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(!old.exists(), "legacy .old must be swept");
        assert!(!aside_a.exists(), "aside must be swept");
        assert!(!aside_b.exists(), "aside must be swept");
        assert!(
            agent_old.exists(),
            "other executables' leftovers must be untouched"
        );
    }
}
