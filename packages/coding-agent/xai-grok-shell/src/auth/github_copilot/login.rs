//! GitHub Copilot device-code login and lazy Copilot-token refresh.
//!
//! This module is intentionally not wired into CLI/ACP/model resolution yet;
//! it provides the authentication core used by a later integration pass.

use chrono::{Duration, Utc};

use super::oauth::{self, DeviceAuthorization, DevicePollTick};
use crate::auth::model::GrokAuth;
use crate::auth::storage::{
    auth_json_path, read_github_copilot_auth, store_github_copilot_auth,
    store_github_copilot_auth_after_refresh_locked,
};
use crate::auth::{AuthChannels, AuthUrlInfo, AuthUrlMode};

const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
const GITHUB_COPILOT_REFRESH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const GITHUB_COPILOT_REFRESH_LOCK_TIMEOUT_WAIT: std::time::Duration =
    std::time::Duration::from_secs(2);
const GITHUB_COPILOT_REFRESH_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Environment variable for an explicit, caller-supplied Copilot inference token.
///
/// This is a static path: the value is never written to auth.json and the OAuth
/// resolver below deliberately ignores it so env credentials cannot be confused
/// with refreshable stored OAuth state.
pub const COPILOT_GITHUB_TOKEN_ENV: &str = "COPILOT_GITHUB_TOKEN";

/// Read `COPILOT_GITHUB_TOKEN` as a direct inference bearer. Never persists it.
pub fn copilot_github_token_env() -> Option<String> {
    std::env::var(COPILOT_GITHUB_TOKEN_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Run GitHub Copilot device login against github.com unless an enterprise
/// domain/URL is explicitly supplied by the future UI integration.
pub async fn run_github_copilot_login(enterprise_domain: Option<&str>) -> anyhow::Result<GrokAuth> {
    run_github_copilot_login_with_channels(enterprise_domain, None).await
}

/// Run GitHub Copilot device login. When `channels` is supplied (ACP/TUI), the
/// verification URL is pushed to the client; CLI still prints the user code.
pub async fn run_github_copilot_login_with_channels(
    enterprise_domain: Option<&str>,
    channels: Option<AuthChannels>,
) -> anyhow::Result<GrokAuth> {
    let domain = oauth::domain_or_default(enterprise_domain);
    let enterprise = enterprise_domain.and_then(oauth::normalize_domain);
    let device = oauth::start_device_flow(&domain).await?;
    if let Some(channels) = channels {
        push_device_url(channels, &device).await;
    } else {
        prompt_on_stderr(&device).await;
    }
    let github_access_token = complete_device_code_login(&domain, &device).await?;
    let copilot =
        oauth::refresh_copilot_access_token(&github_access_token, enterprise.as_deref()).await?;
    let available_models =
        oauth::initialize_copilot_model_availability(&copilot.access, &copilot.base_url).await?;
    let mut auth = oauth::credentials_from_token(copilot, github_access_token, enterprise);
    auth.github_copilot_available_models = Some(available_models);

    let auth_path = auth_json_path();
    let home = auth_path.parent().unwrap_or(std::path::Path::new("."));
    store_github_copilot_auth(home, &auth)?;
    eprintln!("✓ Signed in to GitHub Copilot");
    Ok(auth)
}

/// Live bearer for GitHub Copilot OAuth inference. Stored auth contains a short
/// Copilot token plus a durable GitHub token in `refresh_token`; this resolver
/// refreshes only the stored OAuth path and never consults `COPILOT_GITHUB_TOKEN`.
#[derive(Debug, Default)]
pub struct GitHubCopilotBearerResolver;

impl xai_grok_sampler::BearerResolver for GitHubCopilotBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        ensure_github_copilot_access_token_blocking()
    }
}

/// Persisted token for catalog/startup gating without a network hop.
///
/// An expired Copilot access token with a non-empty durable GitHub token still
/// marks a valid login. The per-request resolver replaces this catalog marker;
/// if refresh fails the sampler strips construction-time auth.
pub fn github_copilot_catalog_access_token_cached() -> Option<String> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    catalog_access_token(read_github_copilot_auth(home)?)
}

/// Best-effort Copilot inference base URL for catalog/model stamping.
pub fn github_copilot_catalog_base_url_cached() -> Option<String> {
    copilot_github_token_env()
        .and_then(|token| oauth::base_url_from_copilot_token(&token))
        .or_else(|| {
            let path = auth_json_path();
            let home = path.parent().unwrap_or(&path);
            let auth = read_github_copilot_auth(home)?;
            Some(base_url_for_auth(&auth))
        })
}

pub(crate) fn github_domain_for_auth(auth: &GrokAuth) -> Option<String> {
    auth.github_domain
        .as_deref()
        .and_then(oauth::normalize_domain)
        .or_else(|| {
            auth.oidc_issuer
                .as_deref()
                .and_then(oauth::normalize_domain)
        })
}

pub(crate) fn base_url_for_auth(auth: &GrokAuth) -> String {
    oauth::base_url_from_copilot_token(&auth.key)
        .or_else(|| auth.github_copilot_base_url.clone())
        .unwrap_or_else(|| {
            oauth::github_copilot_base_url(None, github_domain_for_auth(auth).as_deref())
        })
}

pub fn github_copilot_available_models_cached() -> Option<Vec<String>> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    read_github_copilot_auth(home)?.github_copilot_available_models
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

/// Load a usable Copilot access token: cached if valid, otherwise refreshed and
/// persisted when possible.
pub(super) async fn ensure_github_copilot_access_token() -> Option<String> {
    ensure_github_copilot_auth().await.map(|auth| auth.key)
}

/// Like [`ensure_github_copilot_access_token`] but returns the whole credential.
pub(crate) async fn ensure_github_copilot_auth() -> Option<GrokAuth> {
    refresh_github_copilot_auth(false).await
}

/// Force a network refresh even if the local TTL still looks valid.
pub async fn force_refresh_github_copilot_auth() -> Option<GrokAuth> {
    refresh_github_copilot_auth(true).await
}

async fn refresh_github_copilot_auth(force: bool) -> Option<GrokAuth> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_github_copilot_auth(home)?;
    if !force && !crate::auth::is_expired(&auth) {
        return Some(auth);
    }
    let github_token = auth.refresh_token.as_deref()?.trim().to_owned();
    if github_token.is_empty() {
        return None;
    }

    let file_lock = match crate::auth::manager::lock::try_lock_auth_file_async(
        &path,
        GITHUB_COPILOT_REFRESH_LOCK_TIMEOUT,
    )
    .await
    {
        Some(lock) => lock,
        None => {
            tracing::warn!(
                "github-copilot auth: refresh lock timed out; waiting for sibling then adopting if possible"
            );
            tokio::time::sleep(GITHUB_COPILOT_REFRESH_LOCK_TIMEOUT_WAIT).await;
            return try_adopt_sibling_github_copilot_token(home, &github_token, force);
        }
    };

    if let Some(adopted) = try_adopt_sibling_github_copilot_token(home, &github_token, force) {
        return Some(adopted);
    }

    let file_lock = if file_lock.still_live(&path) {
        file_lock
    } else {
        tracing::warn!("github-copilot auth: refresh lock lost before token exchange");
        drop(file_lock);
        match crate::auth::manager::lock::try_lock_auth_file_async(
            &path,
            GITHUB_COPILOT_REFRESH_LOCK_TIMEOUT,
        )
        .await
        {
            Some(relock) => {
                if let Some(adopted) =
                    try_adopt_sibling_github_copilot_token(home, &github_token, force)
                {
                    return Some(adopted);
                }
                relock
            }
            None => return try_adopt_sibling_github_copilot_token(home, &github_token, force),
        }
    };

    let enterprise = github_domain_for_auth(&auth)
        .and_then(|domain| (domain != oauth::DEFAULT_GITHUB_DOMAIN).then_some(domain));
    let result = oauth::refresh_copilot_access_token(&github_token, enterprise.as_deref()).await;

    let file_lock = if file_lock.still_live(&path) {
        Some(file_lock)
    } else {
        tracing::warn!("github-copilot auth: refresh lock lost during token exchange");
        drop(file_lock);
        if let Some(adopted) = try_adopt_sibling_github_copilot_token(home, &github_token, force) {
            return Some(adopted);
        }
        if result.is_err() {
            None
        } else {
            match crate::auth::manager::lock::try_lock_auth_file_async(
                &path,
                GITHUB_COPILOT_REFRESH_LOCK_TIMEOUT,
            )
            .await
            {
                Some(relock) => Some(relock),
                None => {
                    tokio::time::sleep(GITHUB_COPILOT_REFRESH_LOCK_TIMEOUT_WAIT).await;
                    if let Some(adopted) =
                        try_adopt_sibling_github_copilot_token(home, &github_token, force)
                    {
                        return Some(adopted);
                    }
                    tracing::warn!(
                        "github-copilot auth: could not re-acquire live lock; token will not be persisted"
                    );
                    None
                }
            }
        }
    };

    // Model-availability is best-effort metadata. A failed catalog fetch must
    // not discard a successfully minted access token / durable GitHub token —
    // that would lose the rotation the IdP already performed and leave the
    // session with a stale bearer until the next force-refresh.
    let out = match result {
        Ok(token) => {
            let mut new_auth =
                oauth::credentials_from_token(token, github_token.clone(), enterprise);
            // Prefer a fresh model list; on failure keep any previously stored
            // catalog so a transient models.github.com blip is not a logout.
            match oauth::refresh_copilot_model_availability(
                &new_auth.key,
                new_auth
                    .github_copilot_base_url
                    .as_deref()
                    .unwrap_or_default(),
            )
            .await
            {
                Ok(available_models) => {
                    new_auth.github_copilot_available_models = Some(available_models);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "github-copilot auth: model availability refresh failed; \
                         keeping access token and prior model catalog"
                    );
                    new_auth.github_copilot_available_models =
                        auth.github_copilot_available_models.clone();
                }
            }
            match file_lock.as_ref() {
                Some(file_lock) => {
                    match store_github_copilot_auth_after_refresh_locked(
                        home,
                        &new_auth,
                        &github_token,
                        file_lock,
                    ) {
                        Ok(on_disk) => Some(on_disk),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "github-copilot auth: persist after refresh failed"
                            );
                            // Disk write failed but we hold a live access token —
                            // return it so this process can keep working until
                            // a sibling/later write lands.
                            Some(new_auth)
                        }
                    }
                }
                None => {
                    // Lock lost after IdP success: still hand the minted token
                    // to the caller so the access token is not dropped.
                    Some(new_auth)
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "github-copilot auth: token exchange failed");
            None
        }
    };
    drop(file_lock);
    out
}

fn try_adopt_sibling_github_copilot_token(
    home: &std::path::Path,
    spent_github_token: &str,
    force: bool,
) -> Option<GrokAuth> {
    let existing = read_github_copilot_auth(home)?;
    if existing.auth_mode != crate::auth::AuthMode::GitHubCopilot {
        return None;
    }
    let existing_gt = existing.refresh_token.as_deref().unwrap_or("");
    if existing_gt != spent_github_token {
        if !crate::auth::is_expired(&existing) || !existing_gt.is_empty() {
            tracing::info!("github-copilot auth: adopted sibling token family");
            return Some(existing);
        }
        return None;
    }
    if force {
        return None;
    }
    if !crate::auth::is_expired(&existing) {
        return Some(existing);
    }
    None
}

async fn ensure_with_op_timeout() -> Option<GrokAuth> {
    match tokio::time::timeout(
        GITHUB_COPILOT_REFRESH_OP_TIMEOUT,
        ensure_github_copilot_auth(),
    )
    .await
    {
        Ok(auth) => auth,
        Err(_) => {
            tracing::warn!("github-copilot auth: blocking refresh timed out");
            None
        }
    }
}

pub(crate) fn ensure_github_copilot_access_token_blocking() -> Option<String> {
    ensure_github_copilot_auth_blocking().map(|auth| auth.key)
}

pub(super) fn ensure_github_copilot_auth_blocking() -> Option<GrokAuth> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_github_copilot_auth(home)?;
    if !crate::auth::is_expired(&auth) {
        return Some(auth);
    }
    if auth.refresh_token.as_deref().is_none_or(str::is_empty) {
        return None;
    }

    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(ensure_with_op_timeout()))
        }
        Ok(_) => refresh_on_side_thread(),
        Err(_) => {
            if let Some(main) = crate::main_runtime::main_runtime_handle() {
                return main.block_on(ensure_with_op_timeout());
            }
            refresh_on_side_thread()
        }
    }
}

fn refresh_on_side_thread() -> Option<GrokAuth> {
    let main = crate::main_runtime::main_runtime_handle();
    match std::thread::Builder::new()
        .name("github-copilot-token-refresh".into())
        .spawn(move || {
            crate::main_runtime::block_on_main_or_new_current_thread(main, ensure_with_op_timeout())
                .flatten()
        }) {
        Ok(join) => match join.join() {
            Ok(auth) => auth,
            Err(panic) => {
                tracing::warn!(?panic, "github-copilot auth: refresh thread panicked");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "github-copilot auth: failed to spawn refresh thread");
            None
        }
    }
}

async fn complete_device_code_login(
    domain: &str,
    device: &DeviceAuthorization,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(device.expires_in);
    let mut interval = std::time::Duration::from_secs(device.interval.max(1));
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("GitHub Copilot device authorization timed out");
        }
        // Pi sets waitBeforeFirstPoll=true; do not immediately hit the token endpoint.
        tokio::time::sleep(remaining.min(interval)).await;
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("GitHub Copilot device authorization timed out");
        }
        match oauth::poll_device_token_once(domain, &device.device_code).await? {
            DevicePollTick::Complete {
                github_access_token,
            } => return Ok(github_access_token),
            DevicePollTick::Pending => {}
            DevicePollTick::SlowDown { interval: next } => {
                interval = next.map(std::time::Duration::from_secs).unwrap_or_else(|| {
                    interval + std::time::Duration::from_secs(SLOW_DOWN_INCREMENT_SECS)
                });
            }
            DevicePollTick::Failed { message } => anyhow::bail!(message),
        }
    }
}

async fn push_device_url(channels: AuthChannels, device: &DeviceAuthorization) {
    let display_uri = url::Url::parse(&device.verification_uri)
        .map(|mut url| {
            url.query_pairs_mut()
                .append_pair("user_code", &device.user_code);
            url.to_string()
        })
        .unwrap_or_else(|_| device.verification_uri.clone());
    if let Some(tx) = channels.url_tx {
        let _ = tx.send(AuthUrlInfo {
            url: display_uri.clone(),
            mode: AuthUrlMode::Device,
        });
    }
    crate::auth::device_code::open_browser_detached(&display_uri).await;
}

async fn prompt_on_stderr(device: &DeviceAuthorization) {
    eprintln!();
    eprintln!("To sign in to GitHub Copilot, open this URL in your browser:");
    eprintln!();
    eprintln!("  {}", device.verification_uri);
    eprintln!();
    if !open_browser_detached(&device.verification_uri).await {
        eprintln!("  (Could not open browser automatically — open the URL above manually.)");
        eprintln!();
    }
    eprintln!("Confirm this code in your browser:");
    eprintln!();
    eprintln!("  {}", device.user_code);
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
            tracing::info!(error = %e, "github-copilot auth: could not open browser");
            false
        }
        Err(e) => {
            tracing::info!(error = %e, "github-copilot auth: browser-open task failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_auth(expires_in: Duration, github_token: Option<&str>) -> GrokAuth {
        GrokAuth {
            key: "copilot-access".into(),
            auth_mode: crate::auth::AuthMode::GitHubCopilot,
            expires_at: Some(Utc::now() + expires_in),
            refresh_token: github_token.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn expired_access_with_github_token_remains_catalog_marker() {
        let token = catalog_access_token(cached_auth(Duration::hours(-1), Some("gho durable")));
        assert_eq!(token.as_deref(), Some("copilot-access"));
    }

    #[test]
    fn expired_access_without_github_token_is_not_catalog_marker() {
        assert!(catalog_access_token(cached_auth(Duration::hours(-1), None)).is_none());
        assert!(catalog_access_token(cached_auth(Duration::hours(-1), Some("  "))).is_none());
    }

    #[test]
    fn resolver_returns_empty_resolution_when_no_stored_oauth() {
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        let _guard =
            xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
        let resolver = GitHubCopilotBearerResolver;
        let resolution = xai_grok_sampler::BearerResolver::resolve_bearer(&resolver);
        assert!(resolution.bearer.is_none());
        assert!(resolution.headers.is_empty());
    }
}
