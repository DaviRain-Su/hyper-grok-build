//! OpenAI Codex (ChatGPT) interactive login — browser PKCE + device code.
//!
//! Ported from official Pi `packages/ai/src/auth/oauth/openai-codex.ts`.
//! Credentials persist under [`super::model::OPENAI_CODEX_OAUTH_SCOPE`] in
//! `~/.grok/auth.json`, independent of the primary xAI session.

use std::io::IsTerminal as _;

use anyhow::{Context as _, bail};
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
};
use tokio::net::TcpListener;

use super::oauth::{
    self, BROWSER_CALLBACK_PATH, BROWSER_CALLBACK_PORT, BROWSER_REDIRECT_URI, DEVICE_CODE_TIMEOUT,
    DEVICE_VERIFICATION_URI, DeviceAuthInfo, DevicePollTick,
};
use crate::auth::flow::AuthChannels;
use crate::auth::model::GrokAuth;
use crate::auth::storage::{
    auth_json_path, read_openai_codex_auth, store_openai_codex_auth,
    store_openai_codex_auth_after_refresh_locked,
};

/// How the user wants to authenticate (Pi `Select OpenAI Codex login method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexLoginMethod {
    /// Browser login with a loopback callback (default).
    Browser,
    /// Device code login for headless/remote environments.
    DeviceCode,
}

/// Run interactive OpenAI Codex login and persist the token set.
///
/// * `channels` — `Some`: TUI mode, pushes the auth URL and receives pasted
///   codes through the client UI. `None`: CLI mode (stderr prompts, stdin paste).
/// * `method` — browser (default) or device code. In CLI mode without a
///   terminal on stdin, device code is selected automatically.
pub async fn run_openai_codex_login(
    channels: Option<AuthChannels>,
    method: CodexLoginMethod,
) -> anyhow::Result<GrokAuth> {
    let host = xai_grok_models::PlatformId::OpenAiCodex
        .oauth_host()
        .ok_or_else(|| anyhow::anyhow!("OpenAI Codex OAuth host is not configured"))?;

    let method = resolve_method(method, channels.is_some());
    let auth = match method {
        CodexLoginMethod::Browser => browser_login(&host, channels).await?,
        CodexLoginMethod::DeviceCode => device_code_login(&host, channels).await?,
    };
    // Honor GROK_AUTH_PATH (same path refresh/read use), not only ~/.grok.
    let auth_path = auth_json_path();
    let home = auth_path.parent().unwrap_or(std::path::Path::new("."));
    store_openai_codex_auth(home, &auth)?;
    crate::auth::platform_refresh_sticky::clear_sticky_family(
        crate::auth::platform_refresh_sticky::PlatformRefreshFamily::OpenAiCodex,
    );
    eprintln!("✓ Signed in to OpenAI Codex (ChatGPT)");
    if let Some(email) = auth.email.as_deref() {
        eprintln!("  Account: {email}");
    }
    eprintln!("  Models:");
    eprintln!("    openai-codex/gpt-5.6-sol");
    eprintln!("    openai-codex/gpt-5.6-terra");
    eprintln!("    openai-codex/gpt-5.5");
    eprintln!("  e.g.  grok -m openai-codex/gpt-5.6-sol -p \"ping\"");
    eprintln!("  TUI:  /model openai-codex/gpt-5.6-sol");
    Ok(auth)
}

fn resolve_method(method: CodexLoginMethod, has_client_ui: bool) -> CodexLoginMethod {
    // Without a paste path (no TUI, no terminal stdin) the browser flow cannot
    // complete on a remote/headless host — device code is the only viable flow.
    if method == CodexLoginMethod::Browser && !has_client_ui && !std::io::stdin().is_terminal() {
        return CodexLoginMethod::DeviceCode;
    }
    method
}

// =============================================================================
// Browser flow (loopback callback + manual paste)
// =============================================================================

/// Callback payload parsed from the loopback redirect or a manual paste.
#[derive(Debug)]
struct Callback {
    code: String,
    state: Option<String>,
}

type CallbackResult = Result<Callback, String>;

/// Parse user-pasted input into a [`Callback`] (Pi `parseAuthorizationInput`):
/// full redirect URL, `code#state`, `code=...&state=...`, or a bare code.
fn parse_authorization_input(input: &str) -> Option<Callback> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(url) = url::Url::parse(value) {
        let code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned())?;
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned());
        return Some(Callback { code, state });
    }
    if let Some((code, state)) = value.split_once('#') {
        return Some(Callback {
            code: code.to_owned(),
            state: Some(state.to_owned()),
        });
    }
    if value.contains("code=") {
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(value.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
        return params.get("code").cloned().map(|code| Callback {
            code,
            state: params.get("state").cloned(),
        });
    }
    Some(Callback {
        code: value.to_owned(),
        state: None,
    })
}

async fn handle_callback(
    State(tx): State<tokio::sync::mpsc::Sender<CallbackResult>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let result = if let Some(error) = params.get("error") {
        let desc = params.get("error_description").cloned().unwrap_or_default();
        Err(if desc.is_empty() {
            error.clone()
        } else {
            format!("{error}: {desc}")
        })
    } else {
        match params.get("code") {
            Some(code) => Ok(Callback {
                code: code.clone(),
                state: params.get("state").cloned(),
            }),
            None => Err("Missing authorization code.".to_owned()),
        }
    };
    let ok = result.is_ok();
    if tx.try_send(result).is_err() {
        tracing::error!("openai-codex auth: callback channel send failed");
    }
    let (title, message) = if ok {
        (
            "Signed in",
            "OpenAI authentication completed. You can close this window and return to Grok Build.",
        )
    } else {
        ("Sign-in failed", "Close this window and try again.")
    };
    (StatusCode::OK, Html(callback_page(title, message, ok)))
}

fn callback_page(title: &str, message: &str, is_success: bool) -> String {
    let color = if is_success { "#22c55e" } else { "#ef4444" };
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1"/>
<meta name="color-scheme" content="light dark"/>
<title>{title}</title>
<style>
  *{{margin:0;padding:0;box-sizing:border-box}}
  body{{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
    display:flex;align-items:center;justify-content:center;min-height:100vh;
    background:#0a0a0a;color:#e5e5e5}}
  .card{{text-align:center;display:flex;flex-direction:column;align-items:center;gap:16px;padding:48px}}
  h1{{font-size:18px;font-weight:600;color:{color}}}
  p{{font-size:14px;color:#a3a3a3;max-width:36em}}
  @media(prefers-color-scheme:light){{
    body{{background:#fafafa;color:#171717}}
    p{{color:#525252}}
  }}
</style>
</head>
<body>
  <div class="card">
    <h1>{title}</h1>
    <p>{message}</p>
  </div>
</body>
</html>"#
    )
}

/// Spawn the loopback server and a paste reader, then race them (Pi
/// `loginOpenAICodex`). Returns the authorization code.
async fn wait_for_authorization_code(
    expected_state: &str,
    channels: Option<AuthChannels>,
    listener: Option<TcpListener>,
) -> anyhow::Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CallbackResult>(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Path A: loopback callback server on 127.0.0.1:1455.
    let server = listener.map(|listener| {
        let app = Router::new()
            .route(BROWSER_CALLBACK_PATH, get(handle_callback))
            .fallback(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Html(callback_page(
                        "Not found",
                        "Callback route not found.",
                        false,
                    )),
                )
            })
            .with_state(tx.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        })
    });

    // Path B: manual paste — via the TUI code channel or stdin.
    let expected_state_owned = expected_state.to_owned();
    match channels {
        Some(AuthChannels { mut code_rx, .. }) => {
            let paste_tx = tx.clone();
            tokio::spawn(async move {
                while let Some(input) = code_rx.recv().await {
                    if let Some(callback) = parse_authorization_input(&input) {
                        let _ = paste_tx.send(Ok(callback)).await;
                        return;
                    }
                }
            });
        }
        None => {
            if std::io::stdin().is_terminal() {
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
                        if let Some(callback) = parse_authorization_input(&line) {
                            let _ = paste_tx.blocking_send(Ok(callback));
                            return;
                        }
                    }
                });
            }
        }
    }
    drop(tx);

    let result = tokio::time::timeout(DEVICE_CODE_TIMEOUT, rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("OpenAI Codex login timed out after 15 minutes"))?
        .ok_or_else(|| anyhow::anyhow!("OpenAI Codex login cancelled"))?;

    let _ = shutdown_tx.send(());
    if let Some(server) = server {
        let _ = server.await;
    }

    let callback = result.map_err(|e| anyhow::anyhow!("OpenAI Codex login failed: {e}"))?;
    if let Some(state) = callback.state.as_deref()
        && state != expected_state_owned
    {
        bail!("State mismatch");
    }
    Ok(callback.code)
}

async fn browser_login(host: &str, channels: Option<AuthChannels>) -> anyhow::Result<GrokAuth> {
    let pkce = oauth::generate_pkce();
    let state = oauth::create_state();
    let auth_url = oauth::build_authorize_url(host, &pkce.challenge, &state);

    // Bind the loopback port up-front; on conflict fall back to paste-only
    // (Pi resolves the server to a no-op and relies on manual code entry).
    let listener = match TcpListener::bind(("127.0.0.1", BROWSER_CALLBACK_PORT)).await {
        Ok(listener) => Some(listener),
        Err(e) => {
            tracing::warn!(error = %e, "openai-codex auth: could not bind loopback port");
            eprintln!(
                "Note: could not listen on 127.0.0.1:{BROWSER_CALLBACK_PORT} ({e}); \
                 paste the redirect URL manually."
            );
            None
        }
    };

    let (url_tx, code_rx) = match channels {
        Some(ch) => (ch.url_tx, Some(ch.code_rx)),
        None => (None, None),
    };
    if let Some(tx) = url_tx {
        let _ = tx.send(crate::auth::flow::AuthUrlInfo {
            url: auth_url.clone(),
            mode: crate::auth::flow::AuthUrlMode::Loopback,
        });
    } else {
        eprintln!();
        eprintln!("To sign in to OpenAI Codex (ChatGPT), open this URL in your browser:");
        eprintln!();
        eprintln!("  {auth_url}");
        eprintln!();
    }
    if code_rx.is_none() && std::io::stdin().is_terminal() {
        eprintln!(
            "Complete login in your browser, or paste the authorization code / redirect URL here:"
        );
    }
    // Always hand off to the browser: in TUI mode the client UI shows the
    // URL as a fallback; in CLI mode the URL was printed above.
    {
        let url = auth_url.clone();
        match tokio::task::spawn_blocking(move || webbrowser::open(&url)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::info!(error = %e, "openai-codex auth: could not open browser");
            }
            Err(e) => {
                tracing::info!(error = %e, "openai-codex auth: browser-open task failed");
            }
        }
    }

    let code = wait_for_authorization_code(
        &state,
        code_rx.map(|rx| AuthChannels {
            url_tx: None,
            code_rx: rx,
        }),
        listener,
    )
    .await?;

    let token =
        oauth::exchange_authorization_code(host, &code, &pkce.verifier, BROWSER_REDIRECT_URI)
            .await
            .context("OpenAI Codex token exchange failed")?;
    oauth::credentials_from_token(token, None)
}

// =============================================================================
// Device code flow (headless)
// =============================================================================

async fn device_code_login(host: &str, channels: Option<AuthChannels>) -> anyhow::Result<GrokAuth> {
    let device = oauth::start_device_auth(host).await?;

    match channels {
        Some(ch) => {
            // TUI shows the device URL; the user code travels in the URL so
            // the welcome screen can display it (its device UI derives the
            // code from the URL query).
            if let Some(tx) = ch.url_tx {
                let _ = tx.send(crate::auth::flow::AuthUrlInfo {
                    url: format!("{DEVICE_VERIFICATION_URI}?user_code={}", device.user_code),
                    mode: crate::auth::flow::AuthUrlMode::Device,
                });
            }
        }
        None => {
            eprintln!();
            eprintln!("To sign in to OpenAI Codex (ChatGPT), open this URL in your browser:");
            eprintln!();
            eprintln!("  {DEVICE_VERIFICATION_URI}");
            eprintln!();
            eprintln!("Confirm this code in your browser:");
            eprintln!();
            eprintln!("  {}", device.user_code);
            eprintln!();
            eprintln!("Waiting for authorization...");
        }
    }

    let code = poll_device_auth(host, &device).await?;
    let token = oauth::exchange_device_authorization_code(
        host,
        &code.authorization_code,
        &code.code_verifier,
    )
    .await
    .context("OpenAI Codex token exchange failed")?;
    oauth::credentials_from_token(token, None)
}

struct DeviceCodeSuccess {
    authorization_code: String,
    code_verifier: String,
}

/// Poll the device token endpoint until approval (Pi `pollOAuthDeviceCodeFlow`).
async fn poll_device_auth(
    host: &str,
    device: &DeviceAuthInfo,
) -> anyhow::Result<DeviceCodeSuccess> {
    let deadline = tokio::time::Instant::now() + DEVICE_CODE_TIMEOUT;
    let mut interval = if device.interval.is_zero() {
        oauth::default_poll_interval()
    } else {
        device.interval
    };
    let mut slow_downs = 0u32;

    loop {
        match oauth::poll_device_auth_once(host, device).await? {
            DevicePollTick::Complete {
                authorization_code,
                code_verifier,
            } => {
                return Ok(DeviceCodeSuccess {
                    authorization_code,
                    code_verifier,
                });
            }
            DevicePollTick::Pending => {}
            DevicePollTick::SlowDown => {
                slow_downs += 1;
                interval += oauth::slow_down_increment();
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(interval)).await;
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    if slow_downs > 0 {
        bail!(
            "Device flow timed out after one or more slow_down responses. This is often caused \
             by clock drift in WSL or VM environments. Please sync or restart the VM clock and \
             try again."
        );
    }
    bail!("Device flow timed out")
}

// =============================================================================
// Live bearer resolution (per-request, with refresh)
// =============================================================================

/// Live bearer for OpenAI Codex inference. Access tokens expire; the sampler
/// re-resolves (and refreshes) per request instead of using a stale stamp.
///
/// Wired as [`xai_grok_sampler::SamplerConfig::bearer_resolver`] for
/// `openai-codex/*` models.
#[derive(Debug, Default)]
pub struct OpenAiCodexBearerResolver;

impl xai_grok_sampler::BearerResolver for OpenAiCodexBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        ensure_openai_codex_access_token_blocking()
    }

    fn resolve_bearer(&self) -> xai_grok_sampler::BearerResolution {
        codex_bearer_resolution(ensure_openai_codex_auth_blocking())
    }
}

/// Build one atomic Codex auth result so the account header always comes from
/// the same credential lookup/refresh as the bearer. The explicit removal
/// clears any construction-time account id after logout or failed refresh.
fn codex_bearer_resolution(auth: Option<GrokAuth>) -> xai_grok_sampler::BearerResolution {
    let account_header = reqwest::header::HeaderName::from_static("chatgpt-account-id");
    let mut resolution = xai_grok_sampler::BearerResolution::default();
    resolution.remove_headers.push(account_header.clone());
    let Some(auth) = auth else {
        return resolution;
    };

    resolution.bearer = Some(auth.key);
    if let Some(account_id) = auth.account_id
        && let Ok(value) = reqwest::header::HeaderValue::from_str(&account_id)
    {
        resolution.headers.insert(account_header, value);
    }
    resolution
}

/// Load a usable OpenAI Codex access token: cached if still valid, otherwise
/// refreshed (when possible) and persisted.
pub async fn ensure_openai_codex_access_token() -> Option<String> {
    refresh_openai_codex_auth(false).await.map(|auth| auth.key)
}

/// Like [`ensure_openai_codex_access_token`] but returns the whole credential
/// (bearer + account id) so callers can stamp the `chatgpt-account-id` header.
pub async fn ensure_openai_codex_auth() -> Option<GrokAuth> {
    refresh_openai_codex_auth(false).await
}

/// Return the persisted Codex access token for catalog/startup gating without
/// performing a network refresh.
///
/// An expired access token remains a valid login marker when a non-empty
/// refresh token is persisted. It is never trusted directly on the wire:
/// [`OpenAiCodexBearerResolver`] replaces the catalog stamp per request, and
/// the sampler removes the stamp entirely when live resolution fails.
pub fn openai_codex_catalog_access_token_cached() -> Option<String> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    catalog_access_token(read_openai_codex_auth(home)?)
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

/// Force a network refresh of the Codex access token even when the local TTL
/// still looks valid. Used on 401 from `chatgpt.com/backend-api` so recovery
/// does not no-op on a still-cached rejected bearer.
pub async fn force_refresh_openai_codex_auth() -> Option<GrokAuth> {
    refresh_openai_codex_auth(true).await
}

/// How long to wait for the exclusive `auth.json.lock` before giving up and
/// trying to adopt a sibling's write. Must exceed typical IdP latency so a
/// follower waits for the leader instead of double-spending the RT.
const CODEX_REFRESH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
/// Bound blocking resolver calls so a stalled sibling/network path cannot
/// wedge the caller. Intentionally shorter than the lock budget: the next
/// request can retry or adopt the sibling's eventual write.
const CODEX_REFRESH_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Brief pause after a lock timeout so the holder can finish writing.
const CODEX_REFRESH_LOCK_TIMEOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// `force`: when true, always hit the token endpoint (401 recovery). When
/// false, return the cached credential if it is still within its local TTL.
///
/// Cross-process single-flight: only one process spends a given refresh token.
/// Mirrors the xAI [`crate::auth::manager::AuthManager`] refresh path —
/// acquire `auth.json.lock` *before* the IdP call, re-validate liveness, then
/// persist with compare/adopt so a sibling's newer family is never clobbered.
async fn refresh_openai_codex_auth(force: bool) -> Option<GrokAuth> {
    use crate::auth::platform_refresh_sticky::{
        PlatformRefreshFamily, clear_sticky_for_refresh_token, codex_refresh_error_is_permanent,
        record_sticky_permanent_failure, sticky_permanent_failure,
    };

    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_openai_codex_auth(home)?;
    if !force && !crate::auth::is_expired(&auth) {
        return Some(auth);
    }
    let refresh = auth.refresh_token.as_deref()?.to_owned();
    if refresh.is_empty() {
        return None;
    }

    if let Some(reason) = sticky_permanent_failure(PlatformRefreshFamily::OpenAiCodex, &refresh) {
        tracing::warn!(
            %reason,
            "auth: Codex refresh short-circuited by sticky permanent failure \
             (run `hyper login --openai` or `hyper logout --openai` then re-login)"
        );
        return None;
    }

    // 1. Exclusive flock (or wait + adopt sibling). Never fall through
    // unguarded: that is the "same RT used twice" race that triggers IdP
    // rotation reuse detection and can invalidate the whole token family.
    let file_lock = match crate::auth::manager::lock::try_lock_auth_file_async(
        &path,
        CODEX_REFRESH_LOCK_TIMEOUT,
    )
    .await
    {
        Some(lock) => lock,
        None => {
            tracing::warn!(
                "auth: Codex refresh lock timed out; waiting for sibling then adopting if possible"
            );
            tokio::time::sleep(CODEX_REFRESH_LOCK_TIMEOUT_WAIT).await;
            return try_adopt_sibling_codex_token(home, &refresh, force).or_else(|| {
                tracing::warn!(
                    "auth: Codex refresh could not acquire lock and no sibling token to adopt"
                );
                None
            });
        }
    };

    // 2. Re-read under the lock — a sibling may have already rotated.
    if let Some(adopted) = try_adopt_sibling_codex_token(home, &refresh, force) {
        return Some(adopted);
    }

    // 3. Re-validate that we still hold the *live* lock inode before the
    // irreversible IdP call (sleep/stale-break can move the flock to a dead
    // inode). If lost, re-acquire or adopt.
    let file_lock = if file_lock.still_live(&path) {
        file_lock
    } else {
        tracing::warn!("auth: Codex refresh lock lost before IdP; re-acquiring");
        drop(file_lock);
        match crate::auth::manager::lock::try_lock_auth_file_async(
            &path,
            CODEX_REFRESH_LOCK_TIMEOUT,
        )
        .await
        {
            Some(relock) => {
                if let Some(adopted) = try_adopt_sibling_codex_token(home, &refresh, force) {
                    return Some(adopted);
                }
                relock
            }
            None => {
                return try_adopt_sibling_codex_token(home, &refresh, force);
            }
        }
    };

    let host = xai_grok_models::PlatformId::OpenAiCodex.oauth_host()?;
    // 4. Network while holding the flock so only one process spends this RT.
    // (Same posture as AuthManager::refresh_chain — the lock is held across
    // the IdP call *and* the subsequent persist so a waiter cannot start a
    // second spend of the same RT in the gap between response and write.)
    let result = oauth::refresh_access_token(&host, &refresh).await;

    // If stale-lock recovery replaced the inode mid-call, our old guard no
    // longer protects auth.json. Re-acquire the live lock before compare/write;
    // if that fails, report refresh failure so retry logic cannot reload and
    // resend the stale on-disk credential under a false success signal.
    let file_lock = if file_lock.still_live(&path) {
        Some(file_lock)
    } else {
        tracing::warn!("auth: Codex refresh lock lost during IdP call");
        drop(file_lock);
        if let Some(adopted) = try_adopt_sibling_codex_token(home, &refresh, force) {
            return Some(adopted);
        }
        if result.is_err() {
            None
        } else {
            tracing::warn!(
                "auth: re-acquiring the live Codex lock to persist refreshed credentials"
            );
            match crate::auth::manager::lock::try_lock_auth_file_async(
                &path,
                CODEX_REFRESH_LOCK_TIMEOUT,
            )
            .await
            {
                Some(relock) => Some(relock),
                None => {
                    tokio::time::sleep(CODEX_REFRESH_LOCK_TIMEOUT_WAIT).await;
                    if let Some(adopted) = try_adopt_sibling_codex_token(home, &refresh, force) {
                        return Some(adopted);
                    }
                    tracing::warn!(
                        "auth: Codex refresh could not re-acquire the live lock; token will not be persisted"
                    );
                    None
                }
            }
        }
    };

    let out = match result {
        Ok(token) => match oauth::credentials_from_token(token, Some(&refresh)) {
            Ok(mut new_auth) => {
                // The refresh response has no id_token — carry the email over.
                if new_auth.email.is_none() {
                    new_auth.email = auth.email.clone();
                }
                clear_sticky_for_refresh_token(PlatformRefreshFamily::OpenAiCodex, &refresh);
                if let Some(new_rt) = new_auth.refresh_token.as_deref() {
                    clear_sticky_for_refresh_token(PlatformRefreshFamily::OpenAiCodex, new_rt);
                }
                // Compare/adopt while reusing the refresh guard. Opening a
                // second flock here can self-block on non-reentrant platforms.
                match file_lock.as_ref() {
                    Some(file_lock) => match store_openai_codex_auth_after_refresh_locked(
                        home, &new_auth, &refresh, file_lock,
                    ) {
                        Ok(on_disk) => Some(on_disk),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "auth: failed to persist refreshed Codex token"
                            );
                            None
                        }
                    },
                    None => None,
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "auth: Codex token refresh parse failed");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "auth: Codex token refresh failed");
            if codex_refresh_error_is_permanent(&e) {
                record_sticky_permanent_failure(
                    PlatformRefreshFamily::OpenAiCodex,
                    &refresh,
                    format!("{e:#}"),
                );
            }
            None
        }
    };
    drop(file_lock);
    out
}

/// After acquiring (or failing to acquire) the refresh lock, prefer a sibling's
/// on-disk credential when it already supersedes the RT we were about to spend.
///
/// * RT changed → sibling rotated the family; always adopt when usable.
/// * Same RT, not force, still valid → no network needed.
/// * Same RT + force → do **not** adopt (401 recovery needs a new access token
///   under the same RT; preferring the old access re-sends the rejected bearer).
fn try_adopt_sibling_codex_token(
    home: &std::path::Path,
    spent_refresh: &str,
    force: bool,
) -> Option<GrokAuth> {
    let existing = read_openai_codex_auth(home)?;
    if existing.auth_mode != crate::auth::AuthMode::OpenAiCodex {
        return None;
    }
    let existing_rt = existing.refresh_token.as_deref().unwrap_or("");
    if existing_rt != spent_refresh {
        // Sibling already rotated past the RT we held.
        if !crate::auth::is_expired(&existing) {
            tracing::info!("auth: Codex refresh adopted sibling token (RT rotated)");
            return Some(existing);
        }
        // Sibling wrote an already-expired access under a new RT — still prefer
        // their family over re-spending our dead RT.
        if !existing_rt.is_empty() {
            tracing::info!(
                "auth: Codex refresh adopted sibling RT family (access expired; will re-refresh later)"
            );
            return Some(existing);
        }
        return None;
    }
    if !force && !crate::auth::is_expired(&existing) {
        tracing::debug!("auth: Codex refresh adopted unexpired disk token under lock");
        return Some(existing);
    }
    None
}

async fn ensure_openai_codex_auth_with_op_timeout() -> Option<GrokAuth> {
    match tokio::time::timeout(CODEX_REFRESH_OP_TIMEOUT, ensure_openai_codex_auth()).await {
        Ok(auth) => auth,
        Err(_) => {
            tracing::warn!(
                timeout_secs = CODEX_REFRESH_OP_TIMEOUT.as_secs(),
                "auth: Codex blocking refresh operation timed out"
            );
            None
        }
    }
}

/// Sync-friendly wrapper around [`ensure_openai_codex_access_token`].
///
/// Mirrors the Kimi resolver: safe on multi-thread workers (`block_in_place`),
/// current-thread runtimes and outside any runtime. Non-multithreaded callers
/// drive refresh on the process-wide main runtime when available; an isolated
/// side-thread runtime is only an early-init/test fallback. Always prefers an
/// unexpired disk cache with no runtime hop.
pub fn ensure_openai_codex_access_token_blocking() -> Option<String> {
    ensure_openai_codex_auth_blocking().map(|auth| auth.key)
}

/// Blocking variant of [`ensure_openai_codex_auth`].
pub fn ensure_openai_codex_auth_blocking() -> Option<GrokAuth> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_openai_codex_auth(home)?;
    if !crate::auth::is_expired(&auth) {
        return Some(auth);
    }
    if auth.refresh_token.as_deref().is_none_or(str::is_empty) {
        return None;
    }

    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            // Same 20s outer bound as the no-runtime path (see Kimi resolver).
            tokio::task::block_in_place(|| {
                handle.block_on(ensure_openai_codex_auth_with_op_timeout())
            })
        }
        // A current-thread runtime cannot be blocked or nested. Hop to a plain
        // thread, which drives the future on the process-wide main runtime.
        Ok(_) => refresh_codex_token_on_side_thread(),
        // From a plain thread, drive the operation directly on the main runtime
        // so the shared reqwest client and timeout use the same reactor.
        Err(_) => {
            if let Some(main) = crate::main_runtime::main_runtime_handle() {
                return main.block_on(ensure_openai_codex_auth_with_op_timeout());
            }
            refresh_codex_token_on_side_thread()
        }
    }
}

/// Run the async refresh from a dedicated OS thread. When startup has recorded
/// the process-wide main runtime, the side thread drives the future there;
/// only very-early init and tests build a private current-thread runtime.
fn refresh_codex_token_on_side_thread() -> Option<GrokAuth> {
    let main = crate::main_runtime::main_runtime_handle();
    match std::thread::Builder::new()
        .name("codex-token-refresh".into())
        .spawn(move || {
            crate::main_runtime::block_on_main_or_new_current_thread(
                main,
                ensure_openai_codex_auth_with_op_timeout(),
            )
            .flatten()
        }) {
        Ok(join) => match join.join() {
            Ok(auth) => auth,
            Err(panic) => {
                tracing::warn!(?panic, "auth: Codex token refresh thread panicked");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "auth: failed to spawn Codex token refresh thread");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthMode;
    use chrono::{Duration, Utc};

    fn sample_codex(rt: &str, access: &str, expires_in_secs: i64) -> GrokAuth {
        GrokAuth {
            key: access.to_owned(),
            refresh_token: Some(rt.to_owned()),
            auth_mode: AuthMode::OpenAiCodex,
            expires_at: Some(Utc::now() + Duration::seconds(expires_in_secs)),
            email: Some("user@example.com".to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn expired_access_with_refresh_token_remains_a_catalog_credential() {
        let token = catalog_access_token(sample_codex("refresh", "persisted-access", -3_600));
        assert_eq!(token.as_deref(), Some("persisted-access"));
    }

    #[test]
    fn expired_access_without_refresh_token_is_not_a_catalog_credential() {
        let mut auth = sample_codex("refresh", "persisted-access", -3_600);
        auth.refresh_token = None;
        assert!(catalog_access_token(auth).is_none());

        let auth = sample_codex("   ", "persisted-access", -3_600);
        assert!(catalog_access_token(auth).is_none());
    }

    #[test]
    fn catalog_credential_rejects_an_empty_access_token() {
        assert!(catalog_access_token(sample_codex("refresh", "  ", 3_600)).is_none());
    }

    #[test]
    fn bearer_resolution_aligns_account_header_without_second_auth_lookup() {
        let mut auth = sample_codex("refresh", "live-access", 3_600);
        auth.account_id = Some("acct-123".to_string());
        let resolution = codex_bearer_resolution(Some(auth));

        assert_eq!(resolution.bearer.as_deref(), Some("live-access"));
        assert!(
            resolution
                .remove_headers
                .iter()
                .any(|name| name.as_str() == "chatgpt-account-id")
        );
        assert_eq!(
            resolution
                .headers
                .get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok()),
            Some("acct-123")
        );

        let signed_out = codex_bearer_resolution(None);
        assert!(signed_out.bearer.is_none());
        assert!(signed_out.headers.get("chatgpt-account-id").is_none());
        assert!(
            signed_out
                .remove_headers
                .iter()
                .any(|name| name.as_str() == "chatgpt-account-id"),
            "failed live auth must remove a stale account header"
        );
    }

    #[test]
    fn parse_authorization_input_accepts_full_url() {
        let cb =
            parse_authorization_input("http://localhost:1455/auth/callback?code=abc123&state=xyz")
                .unwrap();
        assert_eq!(cb.code, "abc123");
        assert_eq!(cb.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parse_authorization_input_accepts_code_hash_state() {
        let cb = parse_authorization_input("abc123#xyz").unwrap();
        assert_eq!(cb.code, "abc123");
        assert_eq!(cb.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parse_authorization_input_accepts_query_params() {
        let cb = parse_authorization_input("code=abc123&state=xyz").unwrap();
        assert_eq!(cb.code, "abc123");
        assert_eq!(cb.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parse_authorization_input_accepts_bare_code() {
        let cb = parse_authorization_input("  abc123  ").unwrap();
        assert_eq!(cb.code, "abc123");
        assert!(cb.state.is_none());
        assert!(parse_authorization_input("").is_none());
    }

    #[test]
    fn adopt_sibling_when_rt_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        store_openai_codex_auth(home, &sample_codex("rt-new", "access-new", 3600)).unwrap();
        let adopted = try_adopt_sibling_codex_token(home, "rt-old", false)
            .expect("must adopt sibling with rotated RT");
        assert_eq!(adopted.key, "access-new");
        assert_eq!(adopted.refresh_token.as_deref(), Some("rt-new"));
    }

    #[test]
    fn adopt_same_rt_when_unexpired_and_not_force() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        store_openai_codex_auth(home, &sample_codex("rt-same", "access-ok", 3600)).unwrap();
        let adopted = try_adopt_sibling_codex_token(home, "rt-same", false)
            .expect("unexpired same-RT token should be adopted without network");
        assert_eq!(adopted.key, "access-ok");
    }

    #[test]
    fn do_not_adopt_same_rt_when_force_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Still-valid access under the RT we are about to force-refresh after 401.
        store_openai_codex_auth(home, &sample_codex("rt-same", "access-rejected", 3600)).unwrap();
        assert!(
            try_adopt_sibling_codex_token(home, "rt-same", true).is_none(),
            "force refresh must not re-use the rejected access token under the same RT"
        );
    }

    #[test]
    fn adopt_sibling_rotated_rt_even_when_force() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        store_openai_codex_auth(home, &sample_codex("rt-new", "access-sibling", 3600)).unwrap();
        let adopted = try_adopt_sibling_codex_token(home, "rt-old", true)
            .expect("force path must still adopt a sibling's newer RT family");
        assert_eq!(adopted.refresh_token.as_deref(), Some("rt-new"));
    }
}
