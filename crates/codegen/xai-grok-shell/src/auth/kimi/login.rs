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
    auth_json_path, read_kimi_code_auth, store_kimi_code_auth,
    store_kimi_code_auth_after_refresh_locked,
};

const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
/// Match Codex / AuthManager: wait long enough for a sibling IdP call.
const KIMI_REFRESH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const KIMI_REFRESH_LOCK_TIMEOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
/// Total bound for a blocking refresh operation driven on the main runtime
/// (lock acquire + IdP POST + persist). The per-request POST is separately
/// bounded to 15s inside [`oauth::refresh_token`]. This is intentionally
/// shorter than the 45s cross-process lock budget: an interactive request
/// degrades without a bearer after 20s rather than wedging its caller behind a
/// stalled sibling; the next request retries, and the timeout is logged.
const KIMI_REFRESH_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

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
                crate::auth::platform_refresh_sticky::clear_sticky_family(
                    crate::auth::platform_refresh_sticky::PlatformRefreshFamily::KimiCode,
                );
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

/// Read the cached Kimi access token **without any network refresh**.
///
/// For startup best-effort network work (especially the live `/models` fetch)
/// where blocking on a token refresh would wedge startup when a proxy stalls.
/// Returns `Some` only when the cached token is currently safe to put on the
/// wire. Catalog visibility is handled separately by
/// [`kimi_code_catalog_access_token_cached`], so an expired but refreshable
/// login still exposes `kimi-code/*`; its lazy [`KimiCodeBearerResolver`]
/// refreshes only when the model is actually used.
pub fn kimi_code_access_token_cached() -> Option<String> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_kimi_code_auth(home)?;
    if crate::auth::is_expired(&auth) {
        return None;
    }
    Some(auth.key)
}

/// Return the persisted Kimi access token for catalog/auth-method gating.
///
/// Unlike [`kimi_code_access_token_cached`], this deliberately keeps an
/// expired access token when a refresh token is present. Kimi access tokens
/// last only about 15 minutes; treating expiry as "not logged in" hides every
/// `kimi-code/*` model on the next launch and sends the user through device
/// login again before the per-request bearer resolver gets a chance to refresh.
///
/// The returned value is an in-memory catalog marker, not a network-ready
/// bearer. Every Kimi sampler has [`KimiCodeBearerResolver`] installed, and
/// the sampler removes the catalog-stamped header unless that resolver returns
/// a current token. Startup therefore remains network-free without ever
/// falling back to this expired value on the wire.
pub fn kimi_code_catalog_access_token_cached() -> Option<String> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    catalog_access_token(read_kimi_code_auth(home)?)
}

fn catalog_access_token(auth: GrokAuth) -> Option<String> {
    if auth.key.trim().is_empty() {
        return None;
    }
    let can_refresh = auth
        .refresh_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty());
    if crate::auth::is_expired(&auth) && !can_refresh {
        return None;
    }
    Some(auth.key)
}

/// Force a network refresh of the Kimi access token even when the local TTL
/// still looks valid. Used on 401 from the coding endpoint so recovery does
/// not no-op on a still-cached rejected bearer — mirrors
/// [`crate::auth::openai_codex::force_refresh_openai_codex_auth`].
pub async fn force_refresh_kimi_code_auth() -> Option<GrokAuth> {
    refresh_kimi_code_auth(true).await
}

async fn refresh_kimi_code_auth(force: bool) -> Option<GrokAuth> {
    use crate::auth::platform_refresh_sticky::{
        PlatformRefreshFamily, clear_sticky_for_refresh_token, kimi_refresh_error_is_permanent,
        record_sticky_permanent_failure, sticky_permanent_failure,
    };

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

    // Permanent failure (revoked RT / invalid_grant): do not re-hit the IdP
    // every turn. Cleared on successful refresh under a new RT or logout.
    if let Some(reason) =
        sticky_permanent_failure(PlatformRefreshFamily::KimiCode, &refresh)
    {
        tracing::warn!(
            %reason,
            "auth: Kimi refresh short-circuited by sticky permanent failure \
             (run `hyper login --kimi` or `hyper logout --kimi` then re-login)"
        );
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

    let file_lock = if file_lock.still_live(&path) {
        Some(file_lock)
    } else {
        tracing::warn!("auth: Kimi refresh lock lost during IdP call");
        drop(file_lock);
        if let Some(adopted) = try_adopt_sibling_kimi_token(home, &refresh, force) {
            return Some(adopted);
        }
        if result.is_err() {
            None
        } else {
            tracing::warn!(
                "auth: re-acquiring the live Kimi lock to persist refreshed credentials"
            );
            match crate::auth::manager::lock::try_lock_auth_file_async(
                &path,
                KIMI_REFRESH_LOCK_TIMEOUT,
            )
            .await
            {
                Some(relock) => Some(relock),
                None => {
                    tokio::time::sleep(KIMI_REFRESH_LOCK_TIMEOUT_WAIT).await;
                    if let Some(adopted) = try_adopt_sibling_kimi_token(home, &refresh, force) {
                        return Some(adopted);
                    }
                    tracing::warn!(
                        "auth: Kimi refresh could not re-acquire the live lock; token will not be persisted"
                    );
                    None
                }
            }
        }
    };

    let out = match result {
        Ok(new_auth) => {
            // Success under any RT family clears the spent RT's sticky verdict
            // (and the new RT if it differs, so a re-login path is clean).
            clear_sticky_for_refresh_token(PlatformRefreshFamily::KimiCode, &refresh);
            if let Some(new_rt) = new_auth.refresh_token.as_deref() {
                clear_sticky_for_refresh_token(PlatformRefreshFamily::KimiCode, new_rt);
            }
            match file_lock.as_ref() {
                Some(file_lock) => match store_kimi_code_auth_after_refresh_locked(
                    home, &new_auth, &refresh, file_lock,
                ) {
                    Ok(on_disk) => Some(on_disk),
                    Err(e) => {
                        tracing::warn!(error = %e, "auth: failed to persist refreshed Kimi token");
                        None
                    }
                },
                None => None,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth: Kimi token refresh failed");
            if kimi_refresh_error_is_permanent(&e) {
                record_sticky_permanent_failure(
                    PlatformRefreshFamily::KimiCode,
                    &refresh,
                    e.to_string(),
                );
            }
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
        if !crate::auth::is_expired(&existing) {
            tracing::info!("auth: Kimi refresh adopted sibling token (RT rotated)");
            return Some(existing);
        }
        if !existing_rt.is_empty() {
            tracing::info!(
                "auth: Kimi refresh adopted sibling RT family (access expired; will re-refresh later)"
            );
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

async fn ensure_kimi_code_access_token_with_op_timeout() -> Option<String> {
    match tokio::time::timeout(KIMI_REFRESH_OP_TIMEOUT, ensure_kimi_code_access_token()).await {
        Ok(token) => token,
        Err(_) => {
            tracing::warn!(
                timeout_secs = KIMI_REFRESH_OP_TIMEOUT.as_secs(),
                "auth: Kimi blocking refresh operation timed out"
            );
            xai_grok_telemetry::unified_log::warn(
                "auth.kimi.refresh_operation.timeout_fired",
                None,
                Some(serde_json::json!({
                    "timeout_secs": KIMI_REFRESH_OP_TIMEOUT.as_secs(),
                })),
            );
            None
        }
    }
}

/// Sync-friendly wrapper around [`ensure_kimi_code_access_token`].
///
/// Safe to call from:
/// - multi-thread Tokio workers (`block_in_place` + `block_on`)
/// - **current-thread** runtimes (ACP agent worker) — never uses
///   `block_in_place` there (it panics: "can call blocking only when running
///   on the multi-threaded runtime"); a plain side thread drives the work on
///   the process-wide main runtime instead
/// - no runtime (synchronous config/catalog resolution, early init, or tests)
///   — runs the refresh on the **main** runtime via the process-wide handle
///   recorded at startup, so it shares
///   the reactor with the warmed shared `reqwest` client and
///   `tokio::time::timeout` therefore fires. Falls back to a side-thread
///   runtime only if the main handle was never set.
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
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            // Same 20s outer bound as the no-runtime path — without it a
            // multi-thread bearer resolve can block for flock wait (45s) +
            // IdP budget (40s) on a single request thread.
            tokio::task::block_in_place(|| {
                handle.block_on(ensure_kimi_code_access_token_with_op_timeout())
            })
        }
        // Current-thread runtime (ACP `acp-agent-worker`): never `block_in_place`
        // / nested `block_on` on the caller's runtime.
        Ok(_) => refresh_kimi_token_on_side_thread(),
        // No runtime context (synchronous config/catalog resolution or early
        // init): run the refresh on the **main** runtime so it shares the
        // reactor with the shared reqwest client and `tokio::time::timeout`
        // fires. `Handle::block_on` from a non-runtime thread is the intended,
        // safe use. Bounds the whole op so a stalled proxy / lock contention
        // cannot block the caller long enough to wedge startup; on timeout we
        // give up the bearer (the next request retries naturally).
        Err(_) => {
            if let Some(main) = crate::main_runtime::main_runtime_handle() {
                return main.block_on(ensure_kimi_code_access_token_with_op_timeout());
            }
            // Main handle not set (very early init / tests): side-thread fallback.
            refresh_kimi_token_on_side_thread()
        }
    }
}

/// Run the async refresh from a dedicated OS thread. When startup has recorded
/// the process-wide main runtime, the side thread drives the future there;
/// only very-early init and tests build a private current-thread runtime.
fn refresh_kimi_token_on_side_thread() -> Option<String> {
    let main = crate::main_runtime::main_runtime_handle();
    match std::thread::Builder::new()
        .name("kimi-token-refresh".into())
        .spawn(move || {
            crate::main_runtime::block_on_main_or_new_current_thread(
                main,
                ensure_kimi_code_access_token_with_op_timeout(),
            )
            .flatten()
        }) {
        Ok(join) => match join.join() {
            Ok(token) => token,
            Err(panic) => {
                tracing::warn!(?panic, "auth: Kimi token refresh thread panicked");
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

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::*;

    fn cached_auth(expires_in: Duration, refresh_token: Option<&str>) -> GrokAuth {
        GrokAuth {
            key: "persisted-access".into(),
            auth_mode: crate::auth::AuthMode::KimiCode,
            expires_at: Some(Utc::now() + expires_in),
            refresh_token: refresh_token.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn expired_access_with_refresh_token_remains_a_catalog_credential() {
        let token = catalog_access_token(cached_auth(Duration::hours(-1), Some("refresh")));
        assert_eq!(token.as_deref(), Some("persisted-access"));
    }

    #[test]
    fn expired_access_without_refresh_token_is_not_a_catalog_credential() {
        assert!(catalog_access_token(cached_auth(Duration::hours(-1), None)).is_none());
        assert!(catalog_access_token(cached_auth(Duration::hours(-1), Some("  "))).is_none());
    }

    #[test]
    fn unexpired_access_is_a_catalog_credential_without_refresh_token() {
        let token = catalog_access_token(cached_auth(Duration::hours(1), None));
        assert_eq!(token.as_deref(), Some("persisted-access"));
    }

    #[test]
    fn adopts_rotated_refresh_family_even_when_sibling_access_is_expired() {
        let dir = tempfile::tempdir().unwrap();
        let auth = cached_auth(Duration::hours(-1), Some("refresh-new"));
        store_kimi_code_auth(dir.path(), &auth).unwrap();

        let adopted = try_adopt_sibling_kimi_token(dir.path(), "refresh-old", false)
            .expect("a rotated refresh family must supersede the spent token");
        assert_eq!(adopted.refresh_token.as_deref(), Some("refresh-new"));
    }
}
