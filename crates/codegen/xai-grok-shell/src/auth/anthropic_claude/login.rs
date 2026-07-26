//! Anthropic Claude (Pro/Max) interactive login — browser PKCE + loopback.
//!
//! Credentials persist under [`crate::auth::model::ANTHROPIC_CLAUDE_OAUTH_SCOPE`]
//! in `~/.grok/auth.json`, independent of the primary xAI session. Mirrors the
//! Kimi/Codex subscription channels; the wire dialect is Anthropic Messages.

use std::io::IsTerminal as _;

use anyhow::Context as _;
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
};
use tokio::net::TcpListener;

use super::oauth::{self, BROWSER_CALLBACK_PATH, BROWSER_CALLBACK_PORT, OAUTH_BETA_HEADER_VALUE};
use crate::auth::flow::AuthChannels;
use crate::auth::model::GrokAuth;
use crate::auth::storage::{
    auth_json_path, read_anthropic_claude_auth, store_anthropic_claude_auth,
};

/// Hard timeout for the interactive login (matches Codex: 15 minutes).
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// Bound blocking resolver refreshes so a stalled network path cannot wedge
/// the caller.
const REFRESH_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Run interactive Anthropic Claude login and persist the token set.
///
/// * `channels` — `Some`: TUI mode (pushes the auth URL, receives pasted codes
///   through the client UI). `None`: CLI mode (stderr prompts, stdin paste).
pub async fn run_anthropic_claude_login(
    channels: Option<AuthChannels>,
) -> anyhow::Result<GrokAuth> {
    let auth = browser_login(channels).await?;
    let auth_path = auth_json_path();
    let home = auth_path.parent().unwrap_or(std::path::Path::new("."));
    store_anthropic_claude_auth(home, &auth)?;
    crate::auth::platform_refresh_sticky::clear_sticky_family(
        crate::auth::platform_refresh_sticky::PlatformRefreshFamily::AnthropicClaude,
    );
    eprintln!("✓ Signed in to Anthropic Claude (Pro/Max)");
    eprintln!("  Models:");
    eprintln!("    anthropic-claude/claude-opus-4-6");
    eprintln!("    anthropic-claude/claude-sonnet-4-6");
    eprintln!("  TUI:  /model anthropic-claude/claude-sonnet-4-6");
    Ok(auth)
}

// =============================================================================
// Browser flow (loopback callback + manual paste)
// =============================================================================

#[derive(Debug)]
struct Callback {
    code: String,
    state: Option<String>,
}

type CallbackResult = Result<Callback, String>;

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
        tracing::error!("anthropic-claude auth: callback channel send failed");
    }
    let (title, message) = if ok {
        (
            "Signed in",
            "Claude authentication completed. You can close this window and return to Hyper.",
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

/// Spawn the loopback server and a paste reader, then race them. Returns
/// `(code, state)`.
async fn wait_for_authorization_code(
    flow_state: &str,
    channels: Option<AuthChannels>,
    listener: Option<TcpListener>,
) -> anyhow::Result<(String, String)> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CallbackResult>(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

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

    // Manual paste — via the TUI code channel or stdin.
    let flow_state_owned = flow_state.to_owned();
    match channels {
        Some(AuthChannels { mut code_rx, .. }) => {
            let paste_tx = tx.clone();
            let st = flow_state_owned.clone();
            tokio::spawn(async move {
                while let Some(input) = code_rx.recv().await {
                    if let Some((code, state)) = oauth::parse_authorization_input(&input, &st) {
                        let _ = paste_tx
                            .send(Ok(Callback {
                                code,
                                state: Some(state),
                            }))
                            .await;
                        return;
                    }
                }
            });
        }
        None => {
            if std::io::stdin().is_terminal() {
                let paste_tx = tx.clone();
                let st = flow_state_owned.clone();
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
                        if let Some((code, state)) = oauth::parse_authorization_input(&line, &st) {
                            let _ = paste_tx.blocking_send(Ok(Callback {
                                code,
                                state: Some(state),
                            }));
                            return;
                        }
                    }
                });
            }
        }
    }
    drop(tx);

    let result = tokio::time::timeout(LOGIN_TIMEOUT, rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("Anthropic Claude login timed out after 15 minutes"))?
        .ok_or_else(|| anyhow::anyhow!("Anthropic Claude login cancelled"))?;

    let _ = shutdown_tx.send(());
    if let Some(server) = server {
        let _ = server.await;
    }

    let callback = result.map_err(|e| anyhow::anyhow!("Anthropic Claude login failed: {e}"))?;
    let state = callback.state.unwrap_or(flow_state_owned);
    Ok((callback.code, state))
}

async fn browser_login(channels: Option<AuthChannels>) -> anyhow::Result<GrokAuth> {
    let pkce = oauth::generate_pkce();
    let flow_state = oauth::create_state();
    let auth_url = oauth::build_authorize_url(&pkce.challenge, &flow_state);

    let listener = match TcpListener::bind(("127.0.0.1", BROWSER_CALLBACK_PORT)).await {
        Ok(listener) => Some(listener),
        Err(e) => {
            tracing::warn!(error = %e, "anthropic-claude auth: could not bind loopback port");
            eprintln!(
                "Note: could not listen on 127.0.0.1:{BROWSER_CALLBACK_PORT} ({e}); \
                 paste the redirect URL / code manually."
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
        eprintln!("To sign in to Anthropic Claude, open this URL in your browser:");
        eprintln!();
        eprintln!("  {auth_url}");
        eprintln!();
    }
    if code_rx.is_none() && std::io::stdin().is_terminal() {
        eprintln!("Complete login in your browser, or paste the authorization code / redirect URL here:");
    }
    {
        let url = auth_url.clone();
        match tokio::task::spawn_blocking(move || webbrowser::open(&url)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::info!(error = %e, "anthropic-claude auth: could not open browser"),
            Err(e) => tracing::info!(error = %e, "anthropic-claude auth: browser-open task failed"),
        }
    }

    let (code, state) = wait_for_authorization_code(
        &flow_state,
        code_rx.map(|rx| AuthChannels {
            url_tx: None,
            code_rx: rx,
        }),
        listener,
    )
    .await?;

    let token = oauth::exchange_authorization_code(&code, &state, &pkce.verifier)
        .await
        .context("Anthropic Claude token exchange failed")?;
    Ok(oauth::credentials_from_token(token, None))
}

// =============================================================================
// Per-request bearer resolver (Anthropic Messages)
// =============================================================================

/// Resolves the live Claude subscription bearer + the OAuth beta header for
/// every request against `anthropic-claude/*` models. Wired as the sampler's
/// `bearer_resolver` (mirrors `KimiCodeBearerResolver`).
#[derive(Debug, Default)]
pub struct AnthropicClaudeBearerResolver;

impl xai_grok_sampler::BearerResolver for AnthropicClaudeBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        ensure_anthropic_claude_access_token_blocking()
    }

    fn resolve_bearer(&self) -> xai_grok_sampler::BearerResolution {
        anthropic_claude_bearer_resolution(ensure_anthropic_claude_auth_blocking())
    }
}

/// Build one atomic resolution: bearer + `anthropic-beta: oauth-2025-04-20`.
/// The beta header is always removed first so a stale construction-time value
/// never survives a failed refresh.
fn anthropic_claude_bearer_resolution(
    auth: Option<GrokAuth>,
) -> xai_grok_sampler::BearerResolution {
    let beta = reqwest::header::HeaderName::from_static("anthropic-beta");
    let mut resolution = xai_grok_sampler::BearerResolution::default();
    resolution.remove_headers.push(beta.clone());
    let Some(auth) = auth else {
        return resolution;
    };
    resolution.bearer = Some(auth.key);
    if let Ok(value) = reqwest::header::HeaderValue::from_str(OAUTH_BETA_HEADER_VALUE) {
        resolution.headers.insert(beta, value);
    }
    resolution
}

// =============================================================================
// Cached / refreshed access token
// =============================================================================

/// Persisted access token for catalog/startup gating without a network hop.
/// An expired token with a non-empty refresh token still marks a valid login.
pub fn anthropic_claude_catalog_access_token_cached() -> Option<String> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    catalog_access_token(read_anthropic_claude_auth(home)?)
}

fn catalog_access_token(auth: GrokAuth) -> Option<String> {
    if auth.key.trim().is_empty() {
        return None;
    }
    let can_refresh = auth
        .refresh_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty());
    if crate::auth::is_expired(&auth) && !can_refresh {
        return None;
    }
    Some(auth.key)
}

/// Force a network refresh even when the local TTL still looks valid (401
/// recovery).
pub async fn force_refresh_anthropic_claude_auth() -> Option<GrokAuth> {
    refresh_anthropic_claude_auth(true).await
}

/// Load a usable Claude access token: cached if valid, otherwise refreshed.
pub async fn ensure_anthropic_claude_auth() -> Option<GrokAuth> {
    refresh_anthropic_claude_auth(false).await
}

/// `force`: always hit the token endpoint. Else return the cached credential
/// while it is within its local TTL.
async fn refresh_anthropic_claude_auth(force: bool) -> Option<GrokAuth> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_anthropic_claude_auth(home)?;
    if !force && !crate::auth::is_expired(&auth) {
        return Some(auth);
    }
    let refresh = auth.refresh_token.as_deref()?.to_owned();
    if refresh.is_empty() {
        return None;
    }
    match oauth::refresh_access_token(&refresh).await {
        Ok(token) => {
            let refreshed = oauth::credentials_from_token(token, Some(&refresh));
            if let Err(e) = store_anthropic_claude_auth(home, &refreshed) {
                tracing::warn!(error = %e, "anthropic-claude auth: persist after refresh failed");
            }
            Some(refreshed)
        }
        Err(e) => {
            tracing::warn!(error = %e, "anthropic-claude auth: token refresh failed");
            None
        }
    }
}

async fn ensure_with_op_timeout() -> Option<GrokAuth> {
    match tokio::time::timeout(REFRESH_OP_TIMEOUT, ensure_anthropic_claude_auth()).await {
        Ok(auth) => auth,
        Err(_) => {
            tracing::warn!("anthropic-claude auth: blocking refresh timed out");
            None
        }
    }
}

/// Sync-friendly wrapper. Prefers an unexpired disk cache with no runtime hop;
/// otherwise drives the refresh on the process-wide main runtime (mirrors the
/// Kimi/Codex resolvers).
pub fn ensure_anthropic_claude_access_token_blocking() -> Option<String> {
    ensure_anthropic_claude_auth_blocking().map(|auth| auth.key)
}

/// Blocking variant of [`ensure_anthropic_claude_auth`].
pub fn ensure_anthropic_claude_auth_blocking() -> Option<GrokAuth> {
    let path = auth_json_path();
    let home = path.parent().unwrap_or(&path);
    let auth = read_anthropic_claude_auth(home)?;
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
        .name("claude-token-refresh".into())
        .spawn(move || {
            crate::main_runtime::block_on_main_or_new_current_thread(main, ensure_with_op_timeout())
                .flatten()
        }) {
        Ok(handle) => handle.join().ok().flatten(),
        Err(e) => {
            tracing::warn!(error = %e, "anthropic-claude auth: refresh thread spawn failed");
            None
        }
    }
}
