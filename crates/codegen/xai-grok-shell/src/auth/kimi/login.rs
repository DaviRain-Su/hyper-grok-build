//! Kimi Code device-code login (`grok login --kimi`).
//!
//! Two-phase flow matching kimi-cli / Kigi:
//! 1. request device authorization
//! 2. poll until approved, then persist under [`crate::auth::model::KIMI_CODE_OAUTH_SCOPE`]

use super::oauth::{
    DeviceAuthorization, DevicePollResult, poll_device_token, request_device_authorization,
};
use crate::auth::model::GrokAuth;
use crate::auth::storage::{
    auth_json_path, read_kimi_code_auth, store_kimi_code_auth, store_kimi_code_auth_after_refresh,
};

const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
/// Match Codex / AuthManager: wait long enough for a sibling IdP call.
const KIMI_REFRESH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const KIMI_REFRESH_LOCK_TIMEOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

enum PollLoopOutcome {
    Done(Box<GrokAuth>),
    Restart,
}

/// Run interactive Kimi Code device login and persist the token set.
pub async fn run_kimi_code_login() -> anyhow::Result<GrokAuth> {
    let host = xai_grok_models::PlatformId::KimiCode
        .oauth_host()
        .ok_or_else(|| anyhow::anyhow!("Kimi Code OAuth host is not configured"))?;

    loop {
        let device_auth = request_device_authorization(&host).await?;
        prompt_on_stderr(&device_auth).await;

        match complete_device_code_login(&host, &device_auth).await? {
            PollLoopOutcome::Done(auth) => {
                let auth = *auth;
                // Honor GROK_AUTH_PATH (same path refresh/read use), not only ~/.grok.
                let auth_path = auth_json_path();
                let home = auth_path.parent().unwrap_or(std::path::Path::new("."));
                store_kimi_code_auth(home, &auth)?;
                eprintln!("✓ Signed in to Kimi For Coding");
                eprintln!(
                    "  Models:"
                );
                eprintln!("    kimi-code/k3");
                eprintln!("    kimi-code/k2p7                     # Kimi K2.7 Code");
                eprintln!("    kimi-code/kimi-for-coding-highspeed # Kimi K2.7 Hyper Speed");
                eprintln!("  e.g.  grok -m kimi-code/k3 -p \"ping\"");
                eprintln!("  TUI:  /model kimi-code/k3");
                return Ok(auth);
            }
            PollLoopOutcome::Restart => {
                tracing::info!("auth: Kimi device code expired, restarting");
                eprintln!("Device code expired — requesting a new one...");
                continue;
            }
        }
    }
}

/// Live bearer for Kimi Code inference. Kimi access tokens are short-lived
/// (~15 minutes); the sampler must re-resolve per request instead of using
/// the catalog stamp from login time.
///
/// Wired as [`xai_grok_sampler::SamplerConfig::bearer_resolver`] for
/// `kimi-code/*` models.
#[derive(Debug, Default)]
pub struct KimiCodeBearerResolver;

impl xai_grok_sampler::BearerResolver for KimiCodeBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        ensure_kimi_code_access_token_blocking()
    }
}

/// Load a usable Kimi Code access token: return cached if still valid,
/// otherwise refresh (when possible) and persist.
///
/// Cross-process single-flight (same as Codex): acquire `auth.json.lock`
/// before spending the refresh token so two processes cannot rotate the
/// same RT concurrently.
pub async fn ensure_kimi_code_access_token() -> Option<String> {
    refresh_kimi_code_auth(false).await.map(|a| a.key)
}

/// Force a network refresh of the Kimi access token even when the local TTL
/// still looks valid. Used on 401 from the coding endpoint so recovery does
/// not no-op on a still-cached rejected bearer — mirrors
/// [`crate::auth::openai_codex::force_refresh_openai_codex_auth`].
pub async fn force_refresh_kimi_code_auth() -> Option<GrokAuth> {
    refresh_kimi_code_auth(true).await
}

async fn refresh_kimi_code_auth(force: bool) -> Option<GrokAuth> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_kimi_code_auth(home)?;
    if !force && !crate::auth::is_expired(&auth) {
        return Some(auth);
    }
    let refresh = auth.refresh_token.as_deref()?.to_owned();
    if refresh.is_empty() {
        return None;
    }

    let file_lock = match crate::auth::manager::lock::try_lock_auth_file_async(
        &path,
        KIMI_REFRESH_LOCK_TIMEOUT,
    )
    .await
    {
        Some(lock) => lock,
        None => {
            tracing::warn!(
                "auth: Kimi refresh lock timed out; waiting for sibling then adopting if possible"
            );
            tokio::time::sleep(KIMI_REFRESH_LOCK_TIMEOUT_WAIT).await;
            return try_adopt_sibling_kimi_token(home, &refresh, force);
        }
    };

    if let Some(adopted) = try_adopt_sibling_kimi_token(home, &refresh, force) {
        return Some(adopted);
    }

    let file_lock = if file_lock.still_live(&path) {
        file_lock
    } else {
        tracing::warn!("auth: Kimi refresh lock lost before IdP; re-acquiring");
        drop(file_lock);
        match crate::auth::manager::lock::try_lock_auth_file_async(
            &path,
            KIMI_REFRESH_LOCK_TIMEOUT,
        )
        .await
        {
            Some(relock) => {
                if let Some(adopted) = try_adopt_sibling_kimi_token(home, &refresh, force) {
                    return Some(adopted);
                }
                relock
            }
            None => return try_adopt_sibling_kimi_token(home, &refresh, force),
        }
    };

    let host = xai_grok_models::PlatformId::KimiCode.oauth_host()?;
    let result = super::oauth::refresh_token(&host, &refresh).await;

    if !file_lock.still_live(&path) {
        tracing::warn!("auth: Kimi refresh lock lost during IdP call");
        if let Some(adopted) = try_adopt_sibling_kimi_token(home, &refresh, force) {
            drop(file_lock);
            return Some(adopted);
        }
    }

    let out = match result {
        Ok(new_auth) => match store_kimi_code_auth_after_refresh(home, &new_auth, &refresh) {
            Ok(on_disk) => Some(on_disk),
            Err(e) => {
                tracing::warn!(error = %e, "auth: failed to persist refreshed Kimi token");
                Some(new_auth)
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "auth: Kimi token refresh failed");
            None
        }
    };
    drop(file_lock);
    out
}

/// Prefer a sibling's on-disk Kimi credential when it already supersedes the
/// RT we were about to spend (rotated family or still-valid access).
fn try_adopt_sibling_kimi_token(
    home: &std::path::Path,
    spent_refresh: &str,
    force: bool,
) -> Option<GrokAuth> {
    let existing = read_kimi_code_auth(home)?;
    if existing.auth_mode != crate::auth::AuthMode::KimiCode {
        return None;
    }
    let existing_rt = existing.refresh_token.as_deref().unwrap_or("");
    if existing_rt != spent_refresh {
        if !crate::auth::is_expired(&existing) || !existing_rt.is_empty() {
            tracing::info!("auth: Kimi refresh adopted sibling token (RT rotated)");
            return Some(existing);
        }
        return None;
    }
    // A 401 force-refresh must not re-adopt the very token the server just
    // rejected; only a rotated sibling family (handled above) short-circuits.
    if force {
        return None;
    }
    if !crate::auth::is_expired(&existing) {
        tracing::debug!("auth: Kimi refresh adopted unexpired disk token under lock");
        return Some(existing);
    }
    None
}

/// Sync-friendly wrapper around [`ensure_kimi_code_access_token`].
///
/// Safe to call from:
/// - multi-thread Tokio workers (`block_in_place` + `block_on`)
/// - **current-thread** runtimes (ACP agent worker) — never uses
///   `block_in_place` there (it panics: "can call blocking only when running
///   on the multi-threaded runtime"); refreshes on a side thread instead
/// - no runtime (config load / tests) — side-thread refresh when needed
///
/// Always prefers an unexpired disk cache with **no** network / runtime hop.
pub fn ensure_kimi_code_access_token_blocking() -> Option<String> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_kimi_code_auth(home)?;
    if !crate::auth::is_expired(&auth) {
        return Some(auth.key);
    }
    // Nothing to refresh with — avoid spinning a runtime for a guaranteed miss.
    if auth.refresh_token.as_deref().is_none_or(str::is_empty) {
        return None;
    }

    match tokio::runtime::Handle::try_current() {
        Ok(handle)
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
        {
            tokio::task::block_in_place(|| handle.block_on(ensure_kimi_code_access_token()))
        }
        // Current-thread (ACP `acp-agent-worker`) or no runtime: never
        // `block_in_place` / nested `block_on` on the caller's runtime.
        Ok(_) | Err(_) => refresh_kimi_token_on_side_thread(),
    }
}

/// Run the async refresh on a dedicated OS thread with its own current-thread
/// runtime. Isolates blocking from the caller's Tokio context.
fn refresh_kimi_token_on_side_thread() -> Option<String> {
    match std::thread::Builder::new()
        .name("kimi-token-refresh".into())
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            rt.block_on(ensure_kimi_code_access_token())
        }) {
        Ok(join) => match join.join() {
            Ok(token) => token,
            Err(panic) => {
                tracing::warn!(
                    ?panic,
                    "auth: Kimi token refresh thread panicked"
                );
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "auth: failed to spawn Kimi token refresh thread");
            None
        }
    }
}

/// Defensive upper bound when the server omits `expires_in` (seconds).
const DEFAULT_DEVICE_CODE_TIMEOUT_SECS: u64 = 900;
/// Hard ceiling so a misbehaving server cannot keep us polling for hours.
const MAX_DEVICE_CODE_TIMEOUT_SECS: u64 = 1800;

async fn complete_device_code_login(
    host: &str,
    device_auth: &DeviceAuthorization,
) -> anyhow::Result<PollLoopOutcome> {
    let timeout_secs = device_auth
        .expires_in
        .map(|e| e.max(1) as u64)
        .unwrap_or(DEFAULT_DEVICE_CODE_TIMEOUT_SECS)
        .min(MAX_DEVICE_CODE_TIMEOUT_SECS);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut poll_interval = std::time::Duration::from_secs(device_auth.interval.max(1) as u64);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!(
                "Kimi device authorization timed out after {timeout_secs}s. \
                 Request a new code with `grok login --kimi`."
            );
        }
        tokio::time::sleep(remaining.min(poll_interval)).await;
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "Kimi device authorization timed out after {timeout_secs}s. \
                 Request a new code with `grok login --kimi`."
            );
        }

        match poll_device_token(host, &device_auth.device_code).await? {
            DevicePollResult::Success(auth) => return Ok(PollLoopOutcome::Done(auth)),
            DevicePollResult::Expired => return Ok(PollLoopOutcome::Restart),
            DevicePollResult::AccessDenied { description } => {
                if let Some(desc) = description.filter(|d| !d.is_empty()) {
                    anyhow::bail!("Kimi authorization denied: {desc}");
                }
                anyhow::bail!("Kimi authorization denied by the user");
            }
            DevicePollResult::Fatal { error, description } => {
                if let Some(desc) = description.filter(|d| !d.is_empty()) {
                    anyhow::bail!("Kimi device authorization failed ({error}): {desc}");
                }
                anyhow::bail!("Kimi device authorization failed ({error})");
            }
            DevicePollResult::Pending { error, description } => {
                if error == "slow_down" {
                    poll_interval += std::time::Duration::from_secs(SLOW_DOWN_INCREMENT_SECS);
                } else {
                    tracing::debug!(
                        error = %error,
                        description = ?description,
                        "auth: Kimi device authorization pending"
                    );
                }
            }
        }
    }
}

async fn prompt_on_stderr(device_auth: &DeviceAuthorization) {
    let display_uri = &device_auth.verification_uri_complete;
    eprintln!();
    eprintln!("To sign in to Kimi Code, open this URL in your browser:");
    eprintln!();
    eprintln!("  {display_uri}");
    eprintln!();
    if !open_browser_detached(display_uri).await {
        eprintln!("  (Could not open browser automatically — open the URL above manually.)");
        eprintln!();
    }
    eprintln!("Confirm this code in your browser:");
    eprintln!();
    eprintln!("  {}", device_auth.user_code);
    eprintln!();
    eprintln!(
        "\x1b[90mOnly continue with a code you requested. \
         Don't share it with anyone.\x1b[0m"
    );
    eprintln!();
    eprintln!("Waiting for authorization...");
}

async fn open_browser_detached(url: &str) -> bool {
    if cfg!(test) {
        return false;
    }
    let url = url.to_owned();
    match tokio::task::spawn_blocking(move || webbrowser::open(&url)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::info!(error = %e, "kimi auth: could not open browser");
            false
        }
        Err(e) => {
            tracing::info!(error = %e, "kimi auth: browser-open task failed");
            false
        }
    }
}
