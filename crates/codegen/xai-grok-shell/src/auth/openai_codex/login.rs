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
    self, BROWSER_CALLBACK_PATH, BROWSER_CALLBACK_PORT, BROWSER_REDIRECT_URI,
    DEVICE_CODE_TIMEOUT, DEVICE_VERIFICATION_URI, DeviceAuthInfo, DevicePollTick,
};
use crate::auth::flow::AuthChannels;
use crate::auth::model::GrokAuth;
use crate::auth::storage::{
    auth_json_path, read_openai_codex_auth, store_openai_codex_auth,
    store_openai_codex_auth_after_refresh,
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
    store_openai_codex_auth(&xai_grok_config::grok_home(), &auth)?;
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
        let params: std::collections::HashMap<String, String> = url::form_urlencoded::parse(
            value.as_bytes(),
        )
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
                    Html(callback_page("Not found", "Callback route not found.", false)),
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
        eprintln!("Complete login in your browser, or paste the authorization code / redirect URL here:");
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

    let token = oauth::exchange_authorization_code(host, &code, &pkce.verifier, BROWSER_REDIRECT_URI)
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
async fn poll_device_auth(host: &str, device: &DeviceAuthInfo) -> anyhow::Result<DeviceCodeSuccess> {
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
                })
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
}

/// Per-request `chatgpt-account-id` aligned with the live Codex credential.
///
/// Bearer tokens refresh via [`OpenAiCodexBearerResolver`]; without this
/// injector the account header would stick from the first
/// `inject_url_derived_headers` stamp and break after re-login as a different
/// ChatGPT account mid-session.
#[derive(Debug, Default)]
pub struct OpenAiCodexAccountHeaderInjector;

impl OpenAiCodexAccountHeaderInjector {
    /// Apply the current account id (or remove the header when signed out).
    pub fn apply(headers: &mut reqwest::header::HeaderMap) {
        match ensure_openai_codex_auth_blocking().and_then(|a| a.account_id) {
            Some(account_id) => {
                if let Ok(v) = reqwest::header::HeaderValue::from_str(&account_id) {
                    headers.insert("chatgpt-account-id", v);
                }
            }
            None => {
                headers.remove("chatgpt-account-id");
            }
        }
    }
}

impl xai_grok_sampler::HeaderInjector for OpenAiCodexAccountHeaderInjector {
    fn inject(&self, headers: &mut reqwest::header::HeaderMap) {
        Self::apply(headers);
    }
}

/// Load a usable OpenAI Codex access token: cached if still valid, otherwise
/// refreshed (when possible) and persisted.
pub async fn ensure_openai_codex_access_token() -> Option<String> {
    refresh_openai_codex_auth().await.map(|auth| auth.key)
}

/// Like [`ensure_openai_codex_access_token`] but returns the whole credential
/// (bearer + account id) so callers can stamp the `chatgpt-account-id` header.
pub async fn ensure_openai_codex_auth() -> Option<GrokAuth> {
    refresh_openai_codex_auth().await
}

async fn refresh_openai_codex_auth() -> Option<GrokAuth> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_openai_codex_auth(home)?;
    if !crate::auth::is_expired(&auth) {
        return Some(auth);
    }
    let refresh = auth.refresh_token.as_deref()?.to_owned();
    let host = xai_grok_models::PlatformId::OpenAiCodex.oauth_host()?;
    // Network call outside the flock (do not hold auth.json.lock across I/O).
    match oauth::refresh_access_token(&host, &refresh).await {
        Ok(token) => match oauth::credentials_from_token(token, Some(&refresh)) {
            Ok(mut new_auth) => {
                // The refresh response has no id_token — carry the email over.
                if new_auth.email.is_none() {
                    new_auth.email = auth.email.clone();
                }
                // Under lock: re-read and adopt a sibling's fresher write if one
                // landed while we were on the network, else persist ours.
                match store_openai_codex_auth_after_refresh(home, &new_auth, &refresh) {
                    Ok(on_disk) => Some(on_disk),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "auth: failed to persist refreshed Codex token"
                        );
                        Some(new_auth)
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "auth: Codex token refresh parse failed");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "auth: Codex token refresh failed");
            None
        }
    }
}

/// Sync-friendly wrapper around [`ensure_openai_codex_access_token`].
///
/// Mirrors the Kimi resolver: safe on multi-thread workers (`block_in_place`),
/// current-thread runtimes and outside any runtime (side-thread refresh);
/// always prefers an unexpired disk cache with no runtime hop.
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
        Ok(handle)
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
        {
            tokio::task::block_in_place(|| handle.block_on(ensure_openai_codex_auth()))
        }
        Ok(_) | Err(_) => refresh_codex_token_on_side_thread(),
    }
}

/// Run the async refresh on a dedicated OS thread with its own current-thread
/// runtime. Isolates blocking from the caller's Tokio context.
fn refresh_codex_token_on_side_thread() -> Option<GrokAuth> {
    match std::thread::Builder::new()
        .name("codex-token-refresh".into())
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            rt.block_on(ensure_openai_codex_auth())
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

    #[test]
    fn parse_authorization_input_accepts_full_url() {
        let cb = parse_authorization_input(
            "http://localhost:1455/auth/callback?code=abc123&state=xyz",
        )
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
}
