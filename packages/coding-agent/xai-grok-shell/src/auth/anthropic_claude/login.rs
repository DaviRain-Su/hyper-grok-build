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
    store_anthropic_claude_auth_after_refresh_locked,
};

/// Hard timeout for the interactive login (matches Codex: 15 minutes).
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// Bound blocking resolver refreshes so a stalled network path cannot wedge
/// the caller.
const REFRESH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const REFRESH_LOCK_TIMEOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
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

#[derive(Clone)]
struct CallbackState {
    tx: tokio::sync::mpsc::Sender<CallbackResult>,
    flow_state: String,
}

fn validate_callback_params(
    params: &std::collections::HashMap<String, String>,
    flow_state: &str,
) -> CallbackResult {
    if let Some(error) = params.get("error") {
        let desc = params.get("error_description").cloned().unwrap_or_default();
        return Err(if desc.is_empty() {
            error.clone()
        } else {
            format!("{error}: {desc}")
        });
    }
    let Some(code) = params.get("code").filter(|s| !s.trim().is_empty()) else {
        return Err("Missing authorization code.".to_owned());
    };
    let Some(state) = params.get("state") else {
        return Err("Missing OAuth state.".to_owned());
    };
    if state != flow_state {
        return Err("OAuth state mismatch.".to_owned());
    }
    Ok(Callback {
        code: code.clone(),
        state: Some(state.clone()),
    })
}

async fn handle_callback(
    State(state): State<CallbackState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let result = validate_callback_params(&params, &state.flow_state);
    let ok = result.is_ok();
    if state.tx.try_send(result).is_err() {
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
    let flow_state_owned = flow_state.to_owned();

    let server = listener.map(|listener| {
        let callback_state = CallbackState {
            tx: tx.clone(),
            flow_state: flow_state_owned.clone(),
        };
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
            .with_state(callback_state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        })
    });

    // Manual paste — via the TUI code channel or stdin.
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
    let Some(state) = callback.state else {
        anyhow::bail!("Anthropic Claude login failed: missing OAuth state");
    };
    if state != flow_state_owned {
        anyhow::bail!("Anthropic Claude login failed: OAuth state mismatch");
    }
    Ok((callback.code, state))
}

async fn browser_login(channels: Option<AuthChannels>) -> anyhow::Result<GrokAuth> {
    let pkce = oauth::generate_pkce();
    let flow_state = oauth::create_state();
    let auth_url = oauth::build_authorize_url(&pkce.challenge, &flow_state);

    let listener = if oauth::validate_loopback_redirect_uri()? {
        match TcpListener::bind(("127.0.0.1", BROWSER_CALLBACK_PORT)).await {
            Ok(listener) => Some(listener),
            Err(e) => {
                tracing::warn!(error = %e, "anthropic-claude auth: could not bind loopback port");
                eprintln!(
                    "Note: could not listen on 127.0.0.1:{BROWSER_CALLBACK_PORT} ({e}); \
                     paste the redirect URL / code manually."
                );
                None
            }
        }
    } else {
        None
    };

    let (url_tx, code_rx) = match channels {
        Some(ch) => (ch.url_tx, Some(ch.code_rx)),
        None => (None, None),
    };
    if let Some(tx) = url_tx {
        let _ = tx.send(crate::auth::flow::AuthUrlInfo {
            url: auth_url.clone(),
            // Existing TUI wire mode for "copy URL and show a paste box".
            // With the bundled Claude client this is a provider-hosted manual
            // `code#state` callback, not a localhost listener.
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
        eprintln!(
            "Complete login in your browser, or paste the authorization code / redirect URL here:"
        );
    }
    {
        let url = auth_url.clone();
        match tokio::task::spawn_blocking(move || webbrowser::open(&url)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::info!(error = %e, "anthropic-claude auth: could not open browser")
            }
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

    let file_lock = match crate::auth::manager::lock::try_lock_auth_file_async(
        &path,
        REFRESH_LOCK_TIMEOUT,
        crate::auth::manager::lock::Heartbeat::Skip,
    )
    .await
    .into_guard()
    {
        Some(lock) => lock,
        None => {
            tracing::warn!(
                "anthropic-claude auth: refresh lock timed out; waiting for sibling then adopting if possible"
            );
            tokio::time::sleep(REFRESH_LOCK_TIMEOUT_WAIT).await;
            return try_adopt_sibling_anthropic_claude_token(home, &refresh, force);
        }
    };

    if let Some(adopted) = try_adopt_sibling_anthropic_claude_token(home, &refresh, force) {
        return Some(adopted);
    }

    let file_lock = if file_lock.still_live(&path) {
        file_lock
    } else {
        tracing::warn!("anthropic-claude auth: refresh lock lost before IdP; re-acquiring");
        drop(file_lock);
        match crate::auth::manager::lock::try_lock_auth_file_async(&path, REFRESH_LOCK_TIMEOUT, crate::auth::manager::lock::Heartbeat::Skip)
            .await
            .into_guard()
        {
            Some(relock) => {
                if let Some(adopted) =
                    try_adopt_sibling_anthropic_claude_token(home, &refresh, force)
                {
                    return Some(adopted);
                }
                relock
            }
            None => return try_adopt_sibling_anthropic_claude_token(home, &refresh, force),
        }
    };

    let result = oauth::refresh_access_token(&refresh).await;

    let file_lock = if file_lock.still_live(&path) {
        Some(file_lock)
    } else {
        tracing::warn!("anthropic-claude auth: refresh lock lost during IdP call");
        drop(file_lock);
        if let Some(adopted) = try_adopt_sibling_anthropic_claude_token(home, &refresh, force) {
            return Some(adopted);
        }
        if result.is_err() {
            None
        } else {
            tracing::warn!(
                "anthropic-claude auth: re-acquiring the live lock to persist refreshed credentials"
            );
            match crate::auth::manager::lock::try_lock_auth_file_async(&path, REFRESH_LOCK_TIMEOUT, crate::auth::manager::lock::Heartbeat::Skip)
                .await
                .into_guard()
            {
                Some(relock) => Some(relock),
                None => {
                    tokio::time::sleep(REFRESH_LOCK_TIMEOUT_WAIT).await;
                    if let Some(adopted) =
                        try_adopt_sibling_anthropic_claude_token(home, &refresh, force)
                    {
                        return Some(adopted);
                    }
                    tracing::warn!(
                        "anthropic-claude auth: could not re-acquire live lock; token will not be persisted"
                    );
                    None
                }
            }
        }
    };

    // IdP success must never be discarded because of a durable-write failure.
    // Prefer an adopted sibling family when present; otherwise return the fresh
    // candidate so this process keeps a usable access token.
    let out = match result {
        Ok(token) => {
            let refreshed = oauth::credentials_from_token(token, Some(&refresh));
            match file_lock.as_ref() {
                Some(file_lock) => match store_anthropic_claude_auth_after_refresh_locked(
                    home, &refreshed, &refresh, file_lock,
                ) {
                    Ok(on_disk) => Some(on_disk),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "anthropic-claude auth: persist after refresh failed; \
                             returning in-memory candidate (durable write error retained)"
                        );
                        // Sibling may still have rotated under a lost lock —
                        // prefer their family when present.
                        Some(
                            try_adopt_sibling_anthropic_claude_token(home, &refresh, force)
                                .unwrap_or(refreshed),
                        )
                    }
                },
                None => {
                    tracing::warn!(
                        "anthropic-claude auth: no live lock after IdP success; \
                         returning candidate (or adopted sibling) without durable write"
                    );
                    Some(
                        try_adopt_sibling_anthropic_claude_token(home, &refresh, force)
                            .unwrap_or(refreshed),
                    )
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "anthropic-claude auth: token refresh failed");
            None
        }
    };
    drop(file_lock);
    out
}

fn try_adopt_sibling_anthropic_claude_token(
    home: &std::path::Path,
    spent_refresh: &str,
    force: bool,
) -> Option<GrokAuth> {
    let existing = read_anthropic_claude_auth(home)?;
    let existing_rt = existing.refresh_token.as_deref().unwrap_or("");
    if existing_rt != spent_refresh {
        if !crate::auth::is_expired(&existing) || !existing_rt.is_empty() {
            tracing::info!("anthropic-claude auth: adopted sibling token family");
            return Some(existing);
        }
        return None;
    }
    if force {
        return None;
    }
    if !crate::auth::is_expired(&existing) {
        tracing::debug!("anthropic-claude auth: adopted unexpired disk token under lock");
        return Some(existing);
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn callback_validation_rejects_missing_or_mismatched_state() {
        let mut params = HashMap::from([
            ("code".to_string(), "abc".to_string()),
            ("state".to_string(), "flow".to_string()),
        ]);
        assert!(validate_callback_params(&params, "flow").is_ok());

        params.remove("state");
        assert!(validate_callback_params(&params, "flow").is_err());

        params.insert("state".to_string(), "wrong".to_string());
        assert!(validate_callback_params(&params, "flow").is_err());
    }
}
