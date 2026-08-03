//! Radius gateway OAuth login, credential access, and bearer resolution.

use std::io::IsTerminal as _;

use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
};
use tokio::net::TcpListener;
use xai_grok_sampler::{BearerResolution, BearerResolver};

use crate::auth::model::GrokAuth;
use crate::auth::storage::{
    auth_json_path, read_radius_auth, store_radius_auth, store_radius_auth_after_refresh_locked,
};
use crate::auth::{AuthChannels, AuthUrlInfo, AuthUrlMode};

use super::oauth::{self, DevicePollTick, RadiusToken};

const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const REFRESH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const REFRESH_LOCK_TIMEOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const REFRESH_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadiusLoginMethod {
    Browser,
    DeviceCode,
}

/// Run an interactive Radius login for the standalone CLI.
pub async fn run_radius_login(
    gateway: Option<String>,
    method: RadiusLoginMethod,
) -> anyhow::Result<GrokAuth> {
    run_radius_login_with_channels(gateway, method, None).await
}

/// Run Radius OAuth and optionally publish its URL through the ACP/TUI auth
/// channels. Browser login uses a real PKCE loopback callback; device login is
/// selected automatically when no client UI or terminal can receive a browser
/// callback fallback.
pub async fn run_radius_login_with_channels(
    gateway: Option<String>,
    method: RadiusLoginMethod,
    channels: Option<AuthChannels>,
) -> anyhow::Result<GrokAuth> {
    let gateway = match gateway {
        Some(value) => oauth::normalize_gateway_root(&value)?,
        None => oauth::try_gateway_from_env_or_default()?,
    };
    let method = resolve_method(method, channels.is_some());
    let token = match method {
        RadiusLoginMethod::Browser => browser_login(&gateway, channels).await?,
        RadiusLoginMethod::DeviceCode => device_code_login(&gateway, channels).await?,
    };
    let auth = oauth::credentials_from_token(token, None, &gateway);
    let path = auth_json_path();
    let home = path.parent().unwrap_or(std::path::Path::new("."));
    store_radius_auth(home, &auth)?;
    eprintln!("✓ Signed in to Radius");
    Ok(auth)
}

fn resolve_method(method: RadiusLoginMethod, has_client_ui: bool) -> RadiusLoginMethod {
    if method == RadiusLoginMethod::Browser && !has_client_ui && !std::io::stdin().is_terminal() {
        RadiusLoginMethod::DeviceCode
    } else {
        method
    }
}

// =============================================================================
// Browser PKCE + loopback callback
// =============================================================================

#[derive(Debug)]
struct Callback {
    code: String,
}

type CallbackResult = Result<Callback, String>;

#[derive(Debug)]
enum CallbackAttempt {
    /// A valid authorization code or an OAuth error carrying the expected state.
    Complete(CallbackResult),
    /// Malformed or state-mismatched input. It must not consume the login flow.
    Rejected(String),
}

#[derive(Clone)]
struct CallbackState {
    tx: tokio::sync::mpsc::Sender<CallbackResult>,
    expected_state: String,
}

fn validate_callback_params(
    params: &std::collections::HashMap<String, String>,
    expected_state: &str,
) -> CallbackResult {
    // State is authoritative for both success and OAuth-error callbacks. A
    // loopback request without it must not be able to terminate another local
    // process's in-flight login.
    let state = params
        .get("state")
        .ok_or_else(|| "Missing OAuth state.".to_string())?;
    if state != expected_state {
        return Err("OAuth state mismatch.".to_string());
    }
    if let Some(error) = params.get("error") {
        let description = params
            .get("error_description")
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        return Err(if description.is_empty() {
            error.clone()
        } else {
            format!("{error}: {description}")
        });
    }
    let code = params
        .get("code")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "Missing authorization code.".to_string())?;
    Ok(Callback { code: code.clone() })
}

fn classify_callback_params(
    params: &std::collections::HashMap<String, String>,
    expected_state: &str,
) -> CallbackAttempt {
    let result = validate_callback_params(params, expected_state);
    let state_matches = params
        .get("state")
        .is_some_and(|state| state == expected_state);
    if state_matches && (result.is_ok() || params.contains_key("error")) {
        CallbackAttempt::Complete(result)
    } else {
        CallbackAttempt::Rejected(
            result
                .err()
                .unwrap_or_else(|| "Invalid Radius callback.".to_string()),
        )
    }
}

fn parse_authorization_input(input: &str, expected_state: &str) -> Option<CallbackAttempt> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    let params: std::collections::HashMap<String, String> = if let Ok(url) = url::Url::parse(value)
    {
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port_or_known_default() != Some(oauth::CALLBACK_PORT)
            || url.path() != oauth::CALLBACK_PATH
        {
            return Some(CallbackAttempt::Rejected(
                "The pasted URL is not the Radius loopback callback.".to_string(),
            ));
        }
        url.query_pairs().into_owned().collect()
    } else if let Some((code, state)) = value.split_once('#') {
        std::collections::HashMap::from([
            ("code".to_string(), code.to_string()),
            ("state".to_string(), state.to_string()),
        ])
    } else if value.contains("code=") || value.contains("error=") {
        url::form_urlencoded::parse(value.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    } else {
        return Some(CallbackAttempt::Rejected(
            "Paste the full redirect URL or a code#state value; a bare code cannot be verified."
                .to_string(),
        ));
    };
    Some(classify_callback_params(&params, expected_state))
}

async fn handle_callback(
    State(state): State<CallbackState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let attempt = classify_callback_params(&params, &state.expected_state);
    let (ok, terminal) = match attempt {
        CallbackAttempt::Complete(result) => (result.is_ok(), Some(result)),
        CallbackAttempt::Rejected(message) => {
            tracing::debug!(%message, "radius auth: rejected non-terminal callback");
            (false, None)
        }
    };
    // Match Pi's loopback server: malformed/state-mismatched requests receive
    // 400 but do not consume the flow; a state-bound OAuth error is terminal.
    if let Some(result) = terminal
        && state.tx.try_send(result).is_err()
    {
        tracing::debug!("radius auth: callback arrived after flow completion");
    }
    let (status, title, message) = if ok {
        (
            StatusCode::OK,
            "Signed in",
            "Radius authentication completed. You can close this window and return to Hyper.",
        )
    } else {
        (
            StatusCode::BAD_REQUEST,
            "Sign-in failed",
            "The Radius callback was invalid. Close this window and try again.",
        )
    };
    (status, Html(callback_page(title, message, ok)))
}

fn callback_page(title: &str, message: &str, is_success: bool) -> String {
    let color = if is_success { "#22c55e" } else { "#ef4444" };
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1"/>
<meta name="color-scheme" content="light dark"/><title>{title}</title>
<style>*{{margin:0;padding:0;box-sizing:border-box}}
body{{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
display:flex;align-items:center;justify-content:center;min-height:100vh;background:#0a0a0a;color:#e5e5e5}}
.card{{text-align:center;display:flex;flex-direction:column;align-items:center;gap:16px;padding:48px}}
h1{{font-size:18px;font-weight:600;color:{color}}}p{{font-size:14px;color:#a3a3a3;max-width:36em}}
@media(prefers-color-scheme:light){{body{{background:#fafafa;color:#171717}}p{{color:#525252}}}}</style>
</head><body><div class="card"><h1>{title}</h1><p>{message}</p></div></body></html>"#
    )
}

async fn wait_for_authorization_code(
    expected_state: &str,
    channels: Option<AuthChannels>,
    listener: Option<TcpListener>,
) -> anyhow::Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CallbackResult>(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let server = listener.map(|listener| {
        let app = Router::new()
            .route(oauth::CALLBACK_PATH, get(handle_callback))
            .fallback(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Html(callback_page(
                        "Not found",
                        "This is not the Radius OAuth callback route.",
                        false,
                    )),
                )
            })
            .with_state(CallbackState {
                tx: tx.clone(),
                expected_state: expected_state.to_string(),
            });
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
            {
                tracing::warn!(%error, "radius auth: loopback callback server failed");
            }
        })
    });

    let expected_state = expected_state.to_string();
    match channels {
        Some(AuthChannels { mut code_rx, .. }) => {
            let paste_tx = tx.clone();
            tokio::spawn(async move {
                while let Some(input) = code_rx.recv().await {
                    match parse_authorization_input(&input, &expected_state) {
                        Some(CallbackAttempt::Complete(result)) => {
                            let _ = paste_tx.send(result).await;
                            return;
                        }
                        Some(CallbackAttempt::Rejected(message)) => {
                            tracing::debug!(%message, "radius auth: rejected pasted callback");
                        }
                        None => {}
                    }
                }
            });
        }
        None if std::io::stdin().is_terminal() => {
            let paste_tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                use std::io::BufRead as _;
                let stdin = std::io::stdin();
                let mut line = String::new();
                loop {
                    if paste_tx.is_closed() {
                        return;
                    }
                    line.clear();
                    match stdin.lock().read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    match parse_authorization_input(&line, &expected_state) {
                        Some(CallbackAttempt::Complete(result)) => {
                            let _ = paste_tx.blocking_send(result);
                            return;
                        }
                        Some(CallbackAttempt::Rejected(message)) => {
                            eprintln!("Invalid Radius callback: {message}");
                        }
                        None => {}
                    }
                }
            });
        }
        None => {}
    }
    drop(tx);

    let result = tokio::time::timeout(LOGIN_TIMEOUT, rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("Radius browser login timed out after 15 minutes"))?
        .ok_or_else(|| anyhow::anyhow!("Radius browser login cancelled"))?;

    let _ = shutdown_tx.send(());
    if let Some(server) = server {
        let _ = server.await;
    }

    result
        .map(|callback| callback.code)
        .map_err(|message| anyhow::anyhow!("Radius browser login failed: {message}"))
}

async fn browser_login(
    gateway: &str,
    channels: Option<AuthChannels>,
) -> anyhow::Result<RadiusToken> {
    let endpoint = oauth::discover_authorization_endpoint(gateway).await?;
    let pkce = oauth::generate_pkce();
    let state = oauth::create_state();
    let authorize = oauth::build_authorize_url(&endpoint, &pkce.challenge, &state)?;

    let listener = match TcpListener::bind(("127.0.0.1", oauth::CALLBACK_PORT)).await {
        Ok(listener) => Some(listener),
        Err(error) => {
            tracing::warn!(%error, "radius auth: could not bind loopback callback");
            if channels.is_none() && !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "could not listen on 127.0.0.1:{} for Radius OAuth ({error}); use --device-auth",
                    oauth::CALLBACK_PORT
                );
            }
            eprintln!(
                "Note: could not listen on 127.0.0.1:{} ({error}); paste the full redirect URL instead.",
                oauth::CALLBACK_PORT
            );
            None
        }
    };

    let (url_tx, code_rx) = match channels {
        Some(channels) => (channels.url_tx, Some(channels.code_rx)),
        None => (None, None),
    };
    if let Some(tx) = url_tx {
        let _ = tx.send(AuthUrlInfo {
            url: authorize.clone(),
            mode: AuthUrlMode::Loopback,
        });
    } else {
        eprintln!();
        eprintln!("To sign in to Radius, open this URL in your browser:");
        eprintln!();
        eprintln!("  {authorize}");
        eprintln!();
    }
    if code_rx.is_none() && std::io::stdin().is_terminal() {
        eprintln!(
            "Complete login in your browser, or paste the full redirect URL / code#state here:"
        );
    }
    if !crate::auth::device_code::open_browser_detached(&authorize).await && code_rx.is_none() {
        eprintln!("  (Could not open the browser automatically; open the URL above manually.)");
    }

    let code = wait_for_authorization_code(
        &state,
        code_rx.map(|code_rx| AuthChannels {
            url_tx: None,
            code_rx,
        }),
        listener,
    )
    .await?;
    oauth::exchange_authorization_code(gateway, &code, &pkce.verifier).await
}

// =============================================================================
// Device-code login
// =============================================================================

async fn device_code_login(
    gateway: &str,
    channels: Option<AuthChannels>,
) -> anyhow::Result<RadiusToken> {
    let device = oauth::start_device_flow(gateway).await?;
    let display_uri = url::Url::parse(&device.verification_uri)
        .map(|mut url| {
            url.query_pairs_mut()
                .append_pair("user_code", &device.user_code);
            url.to_string()
        })
        .unwrap_or_else(|_| device.verification_uri.clone());

    if let Some(channels) = channels {
        if let Some(tx) = channels.url_tx {
            let _ = tx.send(AuthUrlInfo {
                url: display_uri.clone(),
                mode: AuthUrlMode::Device,
            });
        }
        crate::auth::device_code::open_browser_detached(&display_uri).await;
    } else {
        eprintln!();
        eprintln!("To sign in to Radius, open this URL in your browser:");
        eprintln!();
        eprintln!("  {}", device.verification_uri);
        eprintln!();
        eprintln!("Confirm this code in your browser:");
        eprintln!();
        eprintln!("  {}", device.user_code);
        eprintln!();
        eprintln!("Waiting for authorization...");
        if !crate::auth::device_code::open_browser_detached(&display_uri).await {
            eprintln!("  (Could not open the browser automatically; open the URL above manually.)");
        }
    }

    complete_device_code_login(gateway, &device).await
}

fn slowed_poll_interval(
    current: std::time::Duration,
    server_interval: Option<u64>,
) -> std::time::Duration {
    server_interval
        .filter(|value| *value > current.as_secs())
        .map(std::time::Duration::from_secs)
        .unwrap_or(current + std::time::Duration::from_secs(oauth::DEVICE_SLOW_DOWN_INCREMENT_SECS))
}

async fn complete_device_code_login(
    gateway: &str,
    device: &oauth::DeviceAuthorization,
) -> anyhow::Result<RadiusToken> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(device.expires_in);
    let mut interval = std::time::Duration::from_secs(device.interval.max(1));
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("Radius device authorization expired");
        }
        tokio::time::sleep(remaining.min(interval)).await;
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("Radius device authorization expired");
        }
        match oauth::poll_device_token_once(gateway, &device.device_code).await? {
            DevicePollTick::Complete(token) => return Ok(token),
            DevicePollTick::Pending => {}
            DevicePollTick::SlowDown {
                interval: server_interval,
            } => {
                interval = slowed_poll_interval(interval, server_interval);
            }
        }
    }
}

// =============================================================================
// Cached marker + refresh
// =============================================================================

fn catalog_access_token(auth: &GrokAuth) -> Option<String> {
    if auth.key.trim().is_empty() {
        return None;
    }
    let can_refresh = auth
        .refresh_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty());
    if super::is_radius_auth_expired(auth) && !can_refresh {
        return None;
    }
    Some(auth.key.clone())
}

/// Catalog-only marker. An expired access token remains a valid login marker
/// when a refresh token exists; it is never sent by the per-request resolver.
pub fn radius_catalog_access_token_cached() -> Option<String> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_radius_auth(home)?;
    catalog_access_token(&auth)
}

/// Return the OAuth catalog marker together with the exact gateway that issued
/// it. This keeps dynamic discovery from reading the unrelated
/// `platform/radius` API-key scope.
pub(crate) fn radius_catalog_oauth_cached() -> Option<(String, String)> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_radius_auth(home)?;
    let marker = catalog_access_token(&auth)?;
    let gateway = match auth.platform_base_url.as_deref() {
        Some(value) => oauth::normalize_gateway_root(value).ok()?,
        None => oauth::try_gateway_from_env_or_default().ok()?,
    };
    Some((marker, gateway))
}

fn gateway_for_auth(auth: &GrokAuth) -> Option<String> {
    match auth.platform_base_url.as_deref() {
        Some(value) => oauth::normalize_gateway_root(value).ok(),
        None => oauth::try_gateway_from_env_or_default().ok(),
    }
}

async fn refresh_radius_auth_inner(force: bool) -> Option<GrokAuth> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_radius_auth(home)?;
    if !force && !super::is_radius_auth_expired(&auth) {
        return Some(auth);
    }
    let refresh = auth.refresh_token.as_deref()?.trim().to_string();
    if refresh.is_empty() {
        return None;
    }
    let gateway = gateway_for_auth(&auth)?;

    let file_lock = match crate::auth::manager::lock::try_lock_auth_file_async(
        &path,
        REFRESH_LOCK_TIMEOUT,
    )
    .await
    {
        Some(lock) => lock,
        None => {
            tracing::warn!(
                "radius auth: refresh lock timed out; waiting for sibling and adopting if possible"
            );
            tokio::time::sleep(REFRESH_LOCK_TIMEOUT_WAIT).await;
            return try_adopt_sibling_radius_token(home, &refresh, force);
        }
    };

    if let Some(adopted) = try_adopt_sibling_radius_token(home, &refresh, force) {
        return Some(adopted);
    }

    let file_lock = if file_lock.still_live(&path) {
        file_lock
    } else {
        tracing::warn!("radius auth: refresh lock lost before token exchange; re-acquiring");
        drop(file_lock);
        match crate::auth::manager::lock::try_lock_auth_file_async(&path, REFRESH_LOCK_TIMEOUT)
            .await
        {
            Some(relock) => {
                if let Some(adopted) = try_adopt_sibling_radius_token(home, &refresh, force) {
                    return Some(adopted);
                }
                relock
            }
            None => return try_adopt_sibling_radius_token(home, &refresh, force),
        }
    };

    let result = oauth::refresh_access_token(&gateway, &refresh).await;
    let file_lock = if file_lock.still_live(&path) {
        Some(file_lock)
    } else {
        tracing::warn!("radius auth: refresh lock lost during token exchange");
        drop(file_lock);
        if let Some(adopted) = try_adopt_sibling_radius_token(home, &refresh, force) {
            return Some(adopted);
        }
        if result.is_err() {
            None
        } else {
            match crate::auth::manager::lock::try_lock_auth_file_async(&path, REFRESH_LOCK_TIMEOUT)
                .await
            {
                Some(relock) => Some(relock),
                None => {
                    tokio::time::sleep(REFRESH_LOCK_TIMEOUT_WAIT).await;
                    if let Some(adopted) = try_adopt_sibling_radius_token(home, &refresh, force) {
                        return Some(adopted);
                    }
                    tracing::warn!(
                        "radius auth: could not re-acquire live lock; refreshed token will not be persisted"
                    );
                    None
                }
            }
        }
    };

    let out = match result {
        Ok(token) => {
            let refreshed = oauth::credentials_from_token(token, Some(&refresh), &gateway);
            match file_lock.as_ref() {
                Some(file_lock) => match store_radius_auth_after_refresh_locked(
                    home, &refreshed, &refresh, file_lock,
                ) {
                    Ok(on_disk) => Some(on_disk),
                    Err(error) => {
                        tracing::warn!(%error, "radius auth: failed to persist refreshed token");
                        None
                    }
                },
                None => None,
            }
        }
        Err(error) => {
            tracing::warn!(%error, "radius auth: token refresh failed");
            None
        }
    };
    drop(file_lock);
    out
}

async fn refresh_radius_auth(force: bool) -> Option<GrokAuth> {
    match tokio::time::timeout(REFRESH_OP_TIMEOUT, refresh_radius_auth_inner(force)).await {
        Ok(auth) => auth,
        Err(_) => {
            tracing::warn!("radius auth: refresh operation timed out");
            None
        }
    }
}

fn try_adopt_sibling_radius_token(
    home: &std::path::Path,
    spent_refresh: &str,
    force: bool,
) -> Option<GrokAuth> {
    let existing = read_radius_auth(home)?;
    let existing_refresh = existing.refresh_token.as_deref().unwrap_or("");
    if existing_refresh != spent_refresh {
        if !super::is_radius_auth_expired(&existing) || !existing_refresh.is_empty() {
            tracing::info!("radius auth: adopted sibling token family");
            return Some(existing);
        }
        return None;
    }
    if force {
        return None;
    }
    if !super::is_radius_auth_expired(&existing) {
        return Some(existing);
    }
    None
}

pub async fn ensure_radius_auth() -> Option<GrokAuth> {
    refresh_radius_auth(false).await
}

pub async fn force_refresh_radius_auth() -> Option<GrokAuth> {
    refresh_radius_auth(true).await
}

pub fn ensure_radius_auth_blocking() -> Option<GrokAuth> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_radius_auth(home)?;
    if !super::is_radius_auth_expired(&auth) {
        return Some(auth);
    }
    if auth
        .refresh_token
        .as_deref()
        .is_none_or(|token| token.trim().is_empty())
    {
        return None;
    }

    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(ensure_radius_auth()))
        }
        Ok(_) => refresh_on_side_thread(),
        Err(_) => {
            if let Some(main) = crate::main_runtime::main_runtime_handle() {
                return main.block_on(ensure_radius_auth());
            }
            refresh_on_side_thread()
        }
    }
}

fn refresh_on_side_thread() -> Option<GrokAuth> {
    let main = crate::main_runtime::main_runtime_handle();
    match std::thread::Builder::new()
        .name("radius-token-refresh".into())
        .spawn(move || {
            crate::main_runtime::block_on_main_or_new_current_thread(main, ensure_radius_auth())
                .flatten()
        }) {
        Ok(join) => match join.join() {
            Ok(auth) => auth,
            Err(panic) => {
                tracing::warn!(?panic, "radius auth: refresh thread panicked");
                None
            }
        },
        Err(error) => {
            tracing::warn!(%error, "radius auth: failed to spawn refresh thread");
            None
        }
    }
}

#[derive(Debug)]
pub struct RadiusBearerResolver;

impl BearerResolver for RadiusBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        ensure_radius_auth_blocking().map(|auth| auth.key)
    }

    fn resolve_bearer(&self) -> BearerResolution {
        BearerResolution::from_bearer(self.current_bearer())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    #[test]
    fn callback_requires_matching_state() {
        let good = std::collections::HashMap::from([
            ("code".to_string(), "authorization-code".to_string()),
            ("state".to_string(), "expected".to_string()),
        ]);
        assert_eq!(
            validate_callback_params(&good, "expected").unwrap().code,
            "authorization-code"
        );

        let mut bad = good.clone();
        bad.insert("state".into(), "attacker".into());
        assert!(validate_callback_params(&bad, "expected").is_err());
        bad.remove("state");
        assert!(validate_callback_params(&bad, "expected").is_err());

        let error_without_state =
            std::collections::HashMap::from([("error".to_string(), "access_denied".to_string())]);
        assert!(matches!(
            classify_callback_params(&error_without_state, "expected"),
            CallbackAttempt::Rejected(message) if message == "Missing OAuth state."
        ));
        let error_with_state = std::collections::HashMap::from([
            ("error".to_string(), "access_denied".to_string()),
            ("state".to_string(), "expected".to_string()),
        ]);
        assert!(matches!(
            classify_callback_params(&error_with_state, "expected"),
            CallbackAttempt::Complete(Err(message)) if message == "access_denied"
        ));
    }

    #[test]
    fn pasted_callback_must_include_verifiable_state() {
        assert!(matches!(
            parse_authorization_input("bare-code", "state"),
            Some(CallbackAttempt::Rejected(_))
        ));
        assert!(matches!(
            parse_authorization_input("code#wrong", "state"),
            Some(CallbackAttempt::Rejected(_))
        ));
        assert!(matches!(
            parse_authorization_input("error=access_denied&state=wrong", "state"),
            Some(CallbackAttempt::Rejected(_))
        ));
        assert!(matches!(
            parse_authorization_input("http://127.0.0.1:1456/wrong?code=code&state=state", "state"),
            Some(CallbackAttempt::Rejected(_))
        ));
        assert!(matches!(
            parse_authorization_input("code#state", "state"),
            Some(CallbackAttempt::Complete(Ok(Callback { code }))) if code == "code"
        ));
        assert!(matches!(
            parse_authorization_input("error=access_denied&state=state", "state"),
            Some(CallbackAttempt::Complete(Err(message))) if message == "access_denied"
        ));
    }

    #[tokio::test]
    async fn loopback_server_accepts_only_callback_route_and_matching_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let wait = wait_for_authorization_code("expected-state", None, Some(listener));
        let send = async move {
            tokio::task::yield_now().await;
            let wrong = crate::http::shared_client()
                .get(format!(
                    "http://{address}/wrong?code=ignored&state=expected-state"
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(wrong.status(), StatusCode::NOT_FOUND);

            for query in [
                "error=access_denied",
                "error=access_denied&state=attacker-state",
                "code=ignored&state=attacker-state",
            ] {
                let rejected = crate::http::shared_client()
                    .get(format!("http://{address}{}?{query}", oauth::CALLBACK_PATH))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
            }

            crate::http::shared_client()
                .get(format!(
                    "http://{address}{}?code=accepted&state=expected-state",
                    oauth::CALLBACK_PATH
                ))
                .send()
                .await
                .unwrap()
        };
        let (code, response) = tokio::join!(wait, send);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(code.unwrap(), "accepted");
    }

    #[test]
    fn slow_down_increases_device_poll_interval() {
        let current = std::time::Duration::from_secs(5);
        assert_eq!(
            slowed_poll_interval(current, None),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            slowed_poll_interval(current, Some(12)),
            std::time::Duration::from_secs(12)
        );
        assert_eq!(
            slowed_poll_interval(current, Some(3)),
            std::time::Duration::from_secs(10)
        );
    }

    #[test]
    fn radius_applies_pi_expiry_skew_only_once() {
        for expires_in_secs in [300, 360] {
            let auth = oauth::credentials_from_token(
                RadiusToken {
                    access: format!("access-{expires_in_secs}"),
                    refresh: format!("refresh-{expires_in_secs}"),
                    expires_in_secs,
                },
                None,
                oauth::DEFAULT_RADIUS_GATEWAY,
            );
            assert!(
                !crate::auth::radius::is_radius_auth_expired(&auth),
                "a newly issued {expires_in_secs}s Radius token must not immediately expire"
            );
        }
    }

    #[test]
    fn expired_refreshable_auth_remains_a_catalog_marker() {
        let auth = GrokAuth {
            key: "expired-marker".into(),
            auth_mode: crate::auth::AuthMode::Radius,
            expires_at: Some(Utc::now() - Duration::minutes(1)),
            refresh_token: Some("refresh".into()),
            ..GrokAuth::test_default()
        };
        assert_eq!(
            catalog_access_token(&auth).as_deref(),
            Some("expired-marker")
        );

        let without_refresh = GrokAuth {
            refresh_token: None,
            ..auth
        };
        assert!(catalog_access_token(&without_refresh).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn oauth_catalog_uses_its_own_gateway_not_api_key_scope() {
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        let _auth_path =
            xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
        let _gateway = xai_grok_test_support::EnvGuard::unset("GROK_RADIUS_BASE_URL");
        let _legacy_gateway = xai_grok_test_support::EnvGuard::unset("RADIUS_GATEWAY_URL");
        crate::auth::store_platform_api_key(
            dir.path(),
            "radius",
            "static-key",
            Some("https://api-key-gateway.example"),
        )
        .unwrap();
        let oauth = GrokAuth {
            key: "oauth-marker".into(),
            auth_mode: crate::auth::AuthMode::Radius,
            expires_at: Some(Utc::now() + Duration::hours(1)),
            refresh_token: Some("oauth-refresh".into()),
            platform_base_url: Some("https://oauth-gateway.example".into()),
            ..GrokAuth::test_default()
        };
        store_radius_auth(dir.path(), &oauth).unwrap();

        let (marker, gateway) = radius_catalog_oauth_cached().unwrap();
        assert_eq!(marker, "oauth-marker");
        assert_eq!(gateway, "https://oauth-gateway.example");
    }

    #[test]
    #[serial_test::serial]
    fn resolver_reads_radius_scope_only() {
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        let _guard =
            xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
        assert!(RadiusBearerResolver.current_bearer().is_none());
        let auth = GrokAuth {
            key: "radius-access".into(),
            auth_mode: crate::auth::AuthMode::Radius,
            expires_at: Some(Utc::now() + Duration::hours(1)),
            refresh_token: Some("rt".into()),
            platform_base_url: Some("http://127.0.0.1:1".into()),
            ..GrokAuth::test_default()
        };
        store_radius_auth(dir.path(), &auth).unwrap();
        assert_eq!(
            RadiusBearerResolver.current_bearer().as_deref(),
            Some("radius-access")
        );
    }
}
