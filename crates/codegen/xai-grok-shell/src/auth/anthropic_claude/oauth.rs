//! Anthropic Claude (Pro/Max) OAuth wire protocol.
//!
//! Mirrors the Claude Code subscription OAuth flow: browser PKCE against
//! `https://claude.ai/oauth/authorize`, JSON token exchange/refresh against
//! `https://console.anthropic.com/v1/oauth/token`. The resulting access token
//! is used as a Bearer credential with `anthropic-beta: oauth-2025-04-20`
//! against Anthropic Messages (`https://api.anthropic.com/v1/messages`).
//!
//! Endpoints and the client id are compiled defaults, each overridable via a
//! `GROK_ANTHROPIC_CLAUDE_*` env var so the flow can be pointed at a staging
//! IdP or corrected without a rebuild.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use serde::Deserialize;
use sha2::Digest as _;

use crate::auth::model::{AuthMode, GrokAuth};

/// Public Claude Code OAuth client id (shared with the official Claude Code
/// CLI). Override with `GROK_ANTHROPIC_CLAUDE_CLIENT_ID`.
const CLIENT_ID_DEFAULT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL_DEFAULT: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL_DEFAULT: &str = "https://console.anthropic.com/v1/oauth/token";
/// Redirect registered for the bundled Claude Code client. It returns a
/// provider-hosted manual `code#state` page; loopback may be used only when
/// `GROK_ANTHROPIC_CLAUDE_REDIRECT_URI` points at a client registered for it.
const REDIRECT_URI_DEFAULT: &str = "https://console.anthropic.com/oauth/code/callback";
pub(crate) const BROWSER_CALLBACK_PORT: u16 = 1456;
pub(crate) const BROWSER_CALLBACK_PATH: &str = "/callback";
const SCOPE_DEFAULT: &str = "org:create_api_key user:profile user:inference";

/// `anthropic-beta` value that enables OAuth-bearer inference on the Messages
/// API. Sent on every request made with a Claude subscription token.
pub const OAUTH_BETA_HEADER_VALUE: &str = "oauth-2025-04-20";

const REFRESH_REQUEST_TIMEOUT_SECS: u64 = 15;
/// Fallback access-token TTL when the token endpoint omits `expires_in`.
const DEFAULT_ACCESS_TOKEN_TTL_SECS: i64 = 3600;

fn env_or(var: &str, default: &str) -> String {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => default.to_string(),
    }
}

pub(crate) fn client_id() -> String {
    env_or("GROK_ANTHROPIC_CLAUDE_CLIENT_ID", CLIENT_ID_DEFAULT)
}
fn authorize_url() -> String {
    env_or("GROK_ANTHROPIC_CLAUDE_AUTHORIZE_URL", AUTHORIZE_URL_DEFAULT)
}
fn token_url() -> String {
    env_or("GROK_ANTHROPIC_CLAUDE_TOKEN_URL", TOKEN_URL_DEFAULT)
}
pub(crate) fn redirect_uri() -> String {
    env_or("GROK_ANTHROPIC_CLAUDE_REDIRECT_URI", REDIRECT_URI_DEFAULT)
}

pub(crate) fn validate_loopback_redirect_uri() -> anyhow::Result<bool> {
    let redirect = redirect_uri();
    let url = url::Url::parse(&redirect)
        .map_err(|e| anyhow::anyhow!("invalid Anthropic Claude redirect URI: {e}"))?;
    let is_loopback = matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1")
    );
    if !is_loopback {
        return Ok(false);
    }
    let port = url.port_or_known_default();
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost") | Some("127.0.0.1"))
        || port != Some(BROWSER_CALLBACK_PORT)
        || url.path() != BROWSER_CALLBACK_PATH
    {
        anyhow::bail!(
            "unsupported Anthropic Claude loopback redirect URI '{redirect}'; \
             supported loopback redirect is http://localhost:{BROWSER_CALLBACK_PORT}{BROWSER_CALLBACK_PATH}"
        );
    }
    Ok(true)
}

fn scope() -> String {
    env_or("GROK_ANTHROPIC_CLAUDE_SCOPE", SCOPE_DEFAULT)
}

fn refresh_request_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(REFRESH_REQUEST_TIMEOUT_SECS)
}

// =============================================================================
// PKCE + state
// =============================================================================

/// PKCE verifier/challenge pair (S256).
pub(crate) struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub(crate) fn generate_pkce() -> Pkce {
    let random_bytes: [u8; 32] = rand::random();
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// Random hex state (16 bytes → 32 chars).
pub(crate) fn create_state() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// =============================================================================
// Authorize URL
// =============================================================================

/// Build the browser-flow authorize URL. `code=true` requests the Claude Code
/// manual-paste variant (`code#state`) in addition to the loopback redirect.
pub(crate) fn build_authorize_url(challenge: &str, state: &str) -> String {
    let mut url = url::Url::parse(&authorize_url()).expect("authorize URL parses");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id())
        .append_pair("redirect_uri", &redirect_uri())
        .append_pair("scope", &scope())
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.to_string()
}

// =============================================================================
// Token exchange / refresh
// =============================================================================

/// Tokens from `POST /v1/oauth/token` (exchange or refresh).
#[derive(Debug)]
pub(crate) struct ClaudeToken {
    pub access: String,
    pub refresh: String,
    pub expires_in_secs: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn claude_token_from_response(json: TokenResponse) -> anyhow::Result<ClaudeToken> {
    let Some(access) = json.access_token.filter(|s| !s.trim().is_empty()) else {
        anyhow::bail!("Anthropic Claude token response missing access_token");
    };
    let refresh = json.refresh_token.unwrap_or_default();
    let expires_in_secs = json
        .expires_in
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_ACCESS_TOKEN_TTL_SECS);
    Ok(ClaudeToken {
        access,
        refresh,
        expires_in_secs,
    })
}

async fn read_token_response(
    response: reqwest::Response,
    operation: &str,
) -> anyhow::Result<ClaudeToken> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic Claude token {operation} failed (HTTP {status}): {body}");
    }
    let json: TokenResponse = response.json().await?;
    claude_token_from_response(json)
        .map_err(|e| anyhow::anyhow!("Anthropic Claude token {operation} response invalid: {e}"))
}

/// Exchange an authorization code for tokens. Anthropic's token endpoint takes
/// a JSON body (not form-urlencoded).
pub(crate) async fn exchange_authorization_code(
    code: &str,
    state: &str,
    verifier: &str,
) -> anyhow::Result<ClaudeToken> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "state": state,
        "client_id": client_id(),
        "redirect_uri": redirect_uri(),
        "code_verifier": verifier,
    });
    let response = crate::http::shared_client()
        .post(token_url())
        .json(&body)
        .send()
        .await?;
    read_token_response(response, "exchange").await
}

/// Refresh an access token. Bounded by `tokio::time::timeout` so a stalled
/// request cannot wedge the `auth.json.lock` it holds.
pub(crate) async fn refresh_access_token(refresh: &str) -> anyhow::Result<ClaudeToken> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh,
        "client_id": client_id(),
    });
    let request = crate::http::shared_client().post(token_url()).json(&body);
    let response = tokio::time::timeout(refresh_request_timeout(), request.send())
        .await
        .map_err(|_| anyhow::anyhow!("Anthropic Claude token refresh timed out"))??;
    read_token_response(response, "refresh").await
}

/// Map a fresh token set to a storable credential.
pub(crate) fn credentials_from_token(
    token: ClaudeToken,
    previous_refresh: Option<&str>,
) -> GrokAuth {
    let now = Utc::now();
    GrokAuth {
        key: token.access,
        auth_mode: AuthMode::AnthropicClaude,
        create_time: now,
        // Refresh responses may omit a replacement refresh token; keep the old one.
        refresh_token: Some(if token.refresh.is_empty() {
            previous_refresh.unwrap_or_default().to_owned()
        } else {
            token.refresh
        }),
        expires_at: Some(now + Duration::seconds(token.expires_in_secs)),
        oidc_issuer: Some(authorize_url()),
        oidc_client_id: Some(client_id()),
        ..Default::default()
    }
}

/// Parse the pasted/loopback authorization input into `(code, state)`.
///
/// Accepts the Claude Code `code#state` fragment shape or a full redirect URL
/// with `?code=&state=`. The state must be present and must match the generated
/// flow state; missing/mismatched state is rejected rather than substituted.
pub(crate) fn parse_authorization_input(input: &str, flow_state: &str) -> Option<(String, String)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Full redirect URL form.
    if let Ok(url) = url::Url::parse(trimmed)
        && (url.scheme() == "http" || url.scheme() == "https")
    {
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if let (Some(code), Some(state)) = (code, state)
            && !code.is_empty()
            && state == flow_state
        {
            return Some((code, state));
        }
        return None;
    }
    // `code#state` fragment shape.
    if let Some((code, state)) = trimmed.split_once('#')
        && !code.is_empty()
        && state == flow_state
    {
        return Some((code.to_owned(), state.to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_test_support::EnvGuard;

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let url = build_authorize_url("chal", "st");
        assert!(url.starts_with("https://claude.ai/oauth/authorize"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st"));
        assert!(url.contains("response_type=code"));
    }

    #[test]
    fn parse_input_accepts_fragment_and_url_only_with_matching_state() {
        assert_eq!(
            parse_authorization_input("abc#flow", "flow"),
            Some(("abc".into(), "flow".into()))
        );
        assert_eq!(
            parse_authorization_input(
                "https://console.anthropic.com/oauth/code/callback?code=abc&state=flow",
                "flow"
            ),
            Some(("abc".into(), "flow".into()))
        );
        assert_eq!(parse_authorization_input("abc#wrong", "flow"), None);
        assert_eq!(
            parse_authorization_input(
                "https://console.anthropic.com/oauth/code/callback?code=abc",
                "flow"
            ),
            None
        );
        assert_eq!(parse_authorization_input("  rawcode  ", "flow"), None);
        assert_eq!(parse_authorization_input("   ", "flow"), None);
    }

    #[test]
    #[serial_test::serial(anthropic_claude_oauth_env)]
    fn loopback_redirect_validation_accepts_only_supported_local_callback() {
        let _g = EnvGuard::set(
            "GROK_ANTHROPIC_CLAUDE_REDIRECT_URI",
            "http://localhost:1456/callback",
        );
        assert!(validate_loopback_redirect_uri().unwrap());

        let _g = EnvGuard::set(
            "GROK_ANTHROPIC_CLAUDE_REDIRECT_URI",
            "http://127.0.0.1:1456/callback",
        );
        assert!(validate_loopback_redirect_uri().unwrap());

        let _g = EnvGuard::set(
            "GROK_ANTHROPIC_CLAUDE_REDIRECT_URI",
            "http://localhost:9999/callback",
        );
        assert!(validate_loopback_redirect_uri().is_err());

        let _g = EnvGuard::set(
            "GROK_ANTHROPIC_CLAUDE_REDIRECT_URI",
            "http://localhost:1456/other",
        );
        assert!(validate_loopback_redirect_uri().is_err());
    }

    #[test]
    #[serial_test::serial(anthropic_claude_oauth_env)]
    fn default_redirect_is_provider_hosted_manual_callback() {
        let _g = EnvGuard::unset("GROK_ANTHROPIC_CLAUDE_REDIRECT_URI");
        assert_eq!(
            redirect_uri(),
            "https://console.anthropic.com/oauth/code/callback"
        );
        assert!(!validate_loopback_redirect_uri().unwrap());
    }

    #[test]
    fn credentials_carry_mode_and_refresh_fallback() {
        let tok = ClaudeToken {
            access: "acc".into(),
            refresh: String::new(),
            expires_in_secs: 3600,
        };
        let auth = credentials_from_token(tok, Some("old-refresh"));
        assert_eq!(auth.auth_mode, AuthMode::AnthropicClaude);
        assert_eq!(auth.key, "acc");
        assert_eq!(auth.refresh_token.as_deref(), Some("old-refresh"));
        assert!(auth.expires_at.is_some());
    }
}
