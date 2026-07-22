//! OpenAI Codex (ChatGPT) OAuth wire protocol.
//!
//! Ported from official Pi `packages/ai/src/auth/oauth/openai-codex.ts`:
//! browser PKCE flow against `https://auth.openai.com`, device-code flow for
//! headless environments, token exchange/refresh, and `chatgpt_account_id`
//! extraction from the access-token JWT.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use serde::Deserialize;
use sha2::Digest as _;

use crate::auth::model::{AuthMode, GrokAuth};

/// Fixed client id shared with the official Codex CLI / Pi.
pub(crate) const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

const AUTHORIZE_PATH: &str = "/oauth/authorize";
const TOKEN_PATH: &str = "/oauth/token";
/// Per-attempt total timeout (connect + response + body) for token-refresh
/// POSTs. Bounds a stalled network path so the refresh — and the
/// `auth.json.lock` it holds — can never block subsequent launches
/// indefinitely. Matches the 15s budget the xAI OIDC refresh path uses.
const REFRESH_REQUEST_TIMEOUT_SECS: u64 = 15;
const DEVICE_USER_CODE_PATH: &str = "/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_PATH: &str = "/api/accounts/deviceauth/token";

/// Loopback redirect registered for the Codex OAuth client. The browser flow
/// listens on `127.0.0.1:1455` (the host portion is `localhost` on the wire,
/// matching Pi / Codex CLI).
pub(crate) const BROWSER_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub(crate) const BROWSER_CALLBACK_PORT: u16 = 1455;
pub(crate) const BROWSER_CALLBACK_PATH: &str = "/auth/callback";

/// Redirect used when exchanging a device-flow authorization code.
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
/// Page the user opens to approve a device code.
pub(crate) const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";

/// Device-flow hard timeout (Pi: 15 minutes).
pub(crate) const DEVICE_CODE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

const SCOPE: &str = "openid profile email offline_access";
/// JWT claim namespace carrying the ChatGPT account id.
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

/// `originator` query/header attribution sent on the authorize URL and every
/// backend request (Pi uses `pi`; Codex CLI uses `codex_cli_rs`).
pub(crate) const ORIGINATOR: &str = "grok-build";

/// RFC 8628 §3.2 fallback poll interval when the server omits `interval`.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// RFC 8628 §3.5: `slow_down` grows the poll interval by 5 seconds.
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;

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

/// Random hex state (16 bytes → 32 chars), matching Pi's `createState`.
pub(crate) fn create_state() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// =============================================================================
// Authorize URL
// =============================================================================

/// Build the browser-flow authorize URL (Pi `createAuthorizationFlow`).
pub(crate) fn build_authorize_url(host: &str, challenge: &str, state: &str) -> String {
    let mut url = url::Url::parse(&format!("{}{AUTHORIZE_PATH}", host_trim(host)))
        .expect("authorize URL parses");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", OPENAI_CODEX_CLIENT_ID)
        .append_pair("redirect_uri", BROWSER_REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);
    url.to_string()
}

fn host_trim(host: &str) -> &str {
    host.trim_end_matches('/')
}

// =============================================================================
// Token exchange / refresh
// =============================================================================

/// Tokens from `POST /oauth/token` (exchange or refresh).
#[derive(Debug)]
pub(crate) struct CodexToken {
    pub access: String,
    pub refresh: String,
    pub expires_in_secs: i64,
    /// Present on `openid` scope grants; used for email display only.
    #[allow(dead_code)]
    pub id_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    id_token: Option<String>,
}

/// Fallback access-token TTL when the token endpoint omits `expires_in`
/// (common on some refresh responses). Conservative enough to refresh early.
const DEFAULT_ACCESS_TOKEN_TTL_SECS: i64 = 3600;

/// Parse a successful token-endpoint JSON body.
///
/// `access_token` is required. `refresh_token` is often omitted on refresh
/// when the server reuses the existing one — leave empty so
/// [`credentials_from_token`] can keep `previous_refresh`. `expires_in` may
/// also be omitted; default rather than failing a valid HTTP 200.
fn codex_token_from_response(json: TokenResponse) -> anyhow::Result<CodexToken> {
    let Some(access) = json.access_token.filter(|s| !s.trim().is_empty()) else {
        anyhow::bail!("OpenAI Codex token response missing access_token");
    };
    let refresh = json.refresh_token.unwrap_or_default();
    let expires_in_secs = json
        .expires_in
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_ACCESS_TOKEN_TTL_SECS);
    Ok(CodexToken {
        access,
        refresh,
        expires_in_secs,
        id_token: json.id_token,
    })
}

async fn read_token_response(
    response: reqwest::Response,
    operation: &str,
) -> anyhow::Result<CodexToken> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI Codex token {operation} failed (HTTP {status}): {body}");
    }
    let json: TokenResponse = response.json().await?;
    codex_token_from_response(json).map_err(|e| {
        anyhow::anyhow!("OpenAI Codex token {operation} response invalid: {e}")
    })
}

/// Exchange an authorization code for tokens (`grant_type=authorization_code`).
pub(crate) async fn exchange_authorization_code(
    host: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> anyhow::Result<CodexToken> {
    let response = crate::http::shared_client()
        .post(format!("{}{TOKEN_PATH}", host_trim(host)))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", OPENAI_CODEX_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?;
    read_token_response(response, "exchange").await
}

/// Exchange a device-flow code (fixed device redirect URI).
pub(crate) async fn exchange_device_authorization_code(
    host: &str,
    code: &str,
    verifier: &str,
) -> anyhow::Result<CodexToken> {
    exchange_authorization_code(host, code, verifier, DEVICE_REDIRECT_URI).await
}

/// Refresh an access token (`grant_type=refresh_token`).
///
/// The POST is bounded by `tokio::time::timeout` (not reqwest's `.timeout()`,
/// which does not reliably abort an in-flight request against a stalled
/// peer). The refresh holds `auth.json.lock` for its whole duration, so a
/// reliable bound prevents one hung request from wedging subsequent
/// launches on the 45s lock timeout.
pub(crate) async fn refresh_access_token(host: &str, refresh: &str) -> anyhow::Result<CodexToken> {
    let request = crate::http::shared_client()
        .post(format!("{}{TOKEN_PATH}", host_trim(host)))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", OPENAI_CODEX_CLIENT_ID),
        ]);
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(REFRESH_REQUEST_TIMEOUT_SECS),
        request.send(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "token refresh timed out after {}s (network path stalled)",
            REFRESH_REQUEST_TIMEOUT_SECS
        )
    })??;
    read_token_response(response, "refresh").await
}

// =============================================================================
// Device authorization flow
// =============================================================================

/// Result of `POST /api/accounts/deviceauth/usercode`.
#[derive(Debug, Clone)]
pub(crate) struct DeviceAuthInfo {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: std::time::Duration,
}

#[derive(Deserialize)]
struct DeviceAuthResponse {
    device_auth_id: Option<String>,
    user_code: Option<String>,
    /// Server sends either a number or a stringified number (Pi accepts both).
    interval: Option<serde_json::Value>,
}

/// `POST {host}/api/accounts/deviceauth/usercode` (JSON body).
pub(crate) async fn start_device_auth(host: &str) -> anyhow::Result<DeviceAuthInfo> {
    let response = crate::http::shared_client()
        .post(format!("{}{DEVICE_USER_CODE_PATH}", host_trim(host)))
        .json(&serde_json::json!({ "client_id": OPENAI_CODEX_CLIENT_ID }))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "OpenAI Codex device code login is not enabled for this server. Use browser login."
            );
        }
        anyhow::bail!("OpenAI Codex device code request failed (HTTP {status}): {body}");
    }
    let json: DeviceAuthResponse = response.json().await?;
    let (Some(device_auth_id), Some(user_code)) = (json.device_auth_id, json.user_code) else {
        anyhow::bail!("Invalid OpenAI Codex device code response");
    };
    Ok(DeviceAuthInfo {
        device_auth_id,
        user_code,
        interval: device_auth_poll_interval(json.interval.as_ref()),
    })
}

/// RFC 8628 §3.2: `interval` is optional; default when omitted / unparseable.
/// Values below 1s are clamped to 1s so we never spin the poll loop.
fn device_auth_poll_interval(interval: Option<&serde_json::Value>) -> std::time::Duration {
    let interval_secs = interval
        .and_then(|v| match v {
            serde_json::Value::Number(n) => n.as_f64(),
            serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        })
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS as f64);
    std::time::Duration::from_secs_f64(interval_secs.max(1.0))
        .max(std::time::Duration::from_secs(1))
}

/// One tick of `POST /api/accounts/deviceauth/token` (Pi `pollOpenAICodexDeviceAuth`).
#[derive(Debug)]
pub(crate) enum DevicePollTick {
    /// Approved: exchange this code with the returned verifier.
    Complete {
        authorization_code: String,
        code_verifier: String,
    },
    /// Not yet approved — keep polling at the current interval.
    Pending,
    /// Server asked us to back off — grow the interval (RFC 8628 §3.5).
    SlowDown,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    authorization_code: Option<String>,
    code_verifier: Option<String>,
}

pub(crate) async fn poll_device_auth_once(
    host: &str,
    info: &DeviceAuthInfo,
) -> anyhow::Result<DevicePollTick> {
    let response = crate::http::shared_client()
        .post(format!("{}{DEVICE_TOKEN_PATH}", host_trim(host)))
        .json(&serde_json::json!({
            "device_auth_id": info.device_auth_id,
            "user_code": info.user_code,
        }))
        .send()
        .await?;
    let status = response.status();
    if status.is_success() {
        let json: DeviceTokenResponse = response.json().await?;
        return match (json.authorization_code, json.code_verifier) {
            (Some(authorization_code), Some(code_verifier)) => Ok(DevicePollTick::Complete {
                authorization_code,
                code_verifier,
            }),
            _ => anyhow::bail!("Invalid OpenAI Codex device auth token response"),
        };
    }
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
        return Ok(DevicePollTick::Pending);
    }
    let body = response.text().await.unwrap_or_default();
    let error_code = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            let error = v.get("error")?.clone();
            match error {
                serde_json::Value::String(s) => Some(s),
                serde_json::Value::Object(_) => error
                    .get("code")
                    .and_then(|c| c.as_str())
                    .map(str::to_owned),
                _ => None,
            }
        });
    match error_code.as_deref() {
        Some("deviceauth_authorization_pending") => Ok(DevicePollTick::Pending),
        Some("slow_down") => Ok(DevicePollTick::SlowDown),
        _ => anyhow::bail!("OpenAI Codex device auth failed (HTTP {status}): {body}"),
    }
}

// =============================================================================
// JWT account id
// =============================================================================

/// Extract `chatgpt_account_id` from the access-token JWT payload.
pub(crate) fn extract_account_id(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let account_id = json
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()?;
    (!account_id.is_empty()).then(|| account_id.to_owned())
}

/// Best-effort email from the id_token payload (display only).
fn extract_email(id_token: Option<&str>) -> Option<String> {
    let payload = id_token?.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    json.get("email")?.as_str().map(str::to_owned)
}

/// Map a fresh token set to a storable credential.
pub(crate) fn credentials_from_token(
    token: CodexToken,
    previous_refresh: Option<&str>,
) -> anyhow::Result<GrokAuth> {
    let account_id = extract_account_id(&token.access)
        .ok_or_else(|| anyhow::anyhow!("Failed to extract accountId from token"))?;
    let now = Utc::now();
    Ok(GrokAuth {
        key: token.access,
        auth_mode: AuthMode::OpenAiCodex,
        create_time: now,
        // Refresh responses may omit a replacement refresh token; keep the old one.
        refresh_token: Some(if token.refresh.is_empty() {
            previous_refresh.unwrap_or_default().to_owned()
        } else {
            token.refresh
        }),
        expires_at: Some(now + Duration::seconds(token.expires_in_secs)),
        email: extract_email(token.id_token.as_deref()),
        account_id: Some(account_id),
        oidc_issuer: Some(
            xai_grok_models::PlatformId::OpenAiCodex
                .oauth_host()
                .unwrap_or_else(|| "https://auth.openai.com".into()),
        ),
        oidc_client_id: Some(OPENAI_CODEX_CLIENT_ID.to_owned()),
        ..Default::default()
    })
}

/// Default poll interval accessor (server override preferred).
pub(crate) fn default_poll_interval() -> std::time::Duration {
    std::time::Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS)
}

/// `slow_down` increment accessor (RFC 8628 §3.5).
pub(crate) fn slow_down_increment() -> std::time::Duration {
    std::time::Duration::from_secs(SLOW_DOWN_INCREMENT_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt_with_claim(claim_json: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(
            format!(r#"{{"{JWT_CLAIM_PATH}": {claim_json}, "sub": "user-1"}}"#).as_bytes(),
        );
        format!("header.{payload}.sig")
    }

    #[test]
    fn pkce_s256_challenge_matches_verifier() {
        let pkce = generate_pkce();
        assert_eq!(pkce.verifier.len(), 43);
        let expected = URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn state_is_32_hex_chars() {
        let state = create_state();
        assert_eq!(state.len(), 32);
        assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn authorize_url_matches_pi_shape() {
        let url = build_authorize_url("https://auth.openai.com", "challenge-x", "state-y");
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("auth.openai.com"));
        assert_eq!(parsed.path(), "/oauth/authorize");
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().collect();
        assert_eq!(params.get("response_type").map(|v| v.as_ref()), Some("code"));
        assert_eq!(
            params.get("client_id").map(|v| v.as_ref()),
            Some(OPENAI_CODEX_CLIENT_ID)
        );
        assert_eq!(
            params.get("redirect_uri").map(|v| v.as_ref()),
            Some(BROWSER_REDIRECT_URI)
        );
        assert_eq!(
            params.get("scope").map(|v| v.as_ref()),
            Some("openid profile email offline_access")
        );
        assert_eq!(
            params.get("code_challenge").map(|v| v.as_ref()),
            Some("challenge-x")
        );
        assert_eq!(
            params.get("code_challenge_method").map(|v| v.as_ref()),
            Some("S256")
        );
        assert_eq!(params.get("state").map(|v| v.as_ref()), Some("state-y"));
        assert_eq!(
            params.get("id_token_add_organizations").map(|v| v.as_ref()),
            Some("true")
        );
        assert_eq!(
            params.get("codex_cli_simplified_flow").map(|v| v.as_ref()),
            Some("true")
        );
        assert_eq!(
            params.get("originator").map(|v| v.as_ref()),
            Some(ORIGINATOR)
        );
    }

    #[test]
    fn account_id_extracts_from_jwt_claim() {
        let token = jwt_with_claim(r#"{"chatgpt_account_id": "acct-123"}"#);
        assert_eq!(extract_account_id(&token).as_deref(), Some("acct-123"));
    }

    #[test]
    fn account_id_missing_claim_returns_none() {
        let token = jwt_with_claim(r#"{"other": "x"}"#);
        assert!(extract_account_id(&token).is_none());
        assert!(extract_account_id("not-a-jwt").is_none());
    }

    #[test]
    fn device_auth_interval_defaults_when_omitted() {
        assert_eq!(
            device_auth_poll_interval(None),
            std::time::Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS)
        );
        assert_eq!(
            device_auth_poll_interval(Some(&serde_json::json!(null))),
            std::time::Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS)
        );
        assert_eq!(
            device_auth_poll_interval(Some(&serde_json::json!(3))),
            std::time::Duration::from_secs(3)
        );
        assert_eq!(
            device_auth_poll_interval(Some(&serde_json::json!("2.5"))),
            std::time::Duration::from_secs_f64(2.5)
        );
        // Sub-second values clamp to 1s.
        assert_eq!(
            device_auth_poll_interval(Some(&serde_json::json!(0))),
            std::time::Duration::from_secs(1)
        );

    }

    #[test]
    fn token_response_tolerates_missing_refresh_and_expires() {
        let access_only = TokenResponse {
            access_token: Some("at".into()),
            refresh_token: None,
            expires_in: None,
            id_token: None,
        };
        let token = codex_token_from_response(access_only).unwrap();
        assert_eq!(token.access, "at");
        assert!(token.refresh.is_empty());
        assert_eq!(token.expires_in_secs, DEFAULT_ACCESS_TOKEN_TTL_SECS);

        // Empty refresh + previous_refresh reuses the prior RT.
        let token = CodexToken {
            access: jwt_with_claim(r#"{"chatgpt_account_id": "acct-1"}"#),
            refresh: String::new(),
            expires_in_secs: DEFAULT_ACCESS_TOKEN_TTL_SECS,
            id_token: None,
        };
        let auth = credentials_from_token(token, Some("prev-rt")).unwrap();
        assert_eq!(auth.refresh_token.as_deref(), Some("prev-rt"));
        assert_eq!(auth.account_id.as_deref(), Some("acct-1"));

        assert!(
            codex_token_from_response(TokenResponse {
                access_token: None,
                refresh_token: Some("rt".into()),
                expires_in: Some(60),
                id_token: None,
            })
            .is_err()
        );
    }

    #[test]
    fn credentials_require_account_id() {
        let token = CodexToken {
            access: jwt_with_claim(r#"{"chatgpt_account_id": "acct-9"}"#),
            refresh: "rt".into(),
            expires_in_secs: 3600,
            id_token: None,
        };
        let auth = credentials_from_token(token, None).unwrap();
        assert_eq!(auth.account_id.as_deref(), Some("acct-9"));
        assert_eq!(auth.auth_mode, AuthMode::OpenAiCodex);
        assert_eq!(auth.refresh_token.as_deref(), Some("rt"));
        assert!(auth.expires_at.is_some());

        let bad = CodexToken {
            access: "no-claim".into(),
            refresh: "rt".into(),
            expires_in_secs: 3600,
            id_token: None,
        };
        assert!(credentials_from_token(bad, None).is_err());
    }
}
