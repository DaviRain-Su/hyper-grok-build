//! Kimi Code OAuth wire protocol (device authorization + token poll/refresh).
//!
//! Ported from kimi-cli `auth/oauth.py` / Kigi-CLI `kimi_oauth.rs`.

use chrono::{Duration, Utc};
use serde::Deserialize;

use super::device::device_headers;
use crate::auth::model::{AuthMode, GrokAuth};

/// Fixed client id used by the official Kimi Code device-flow client.
pub(crate) const KIMI_CODE_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_GRANT_TYPE: &str = "refresh_token";
const MAX_REFRESH_RETRIES: u32 = 3;
const RETRYABLE_REFRESH_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

/// Per-attempt total timeout (connect + response + body) for token-refresh
/// POSTs. Bounds a stalled network path (e.g. a fake-ip proxy that accepts
/// the TCP handshake then never responds) so the refresh — and the
/// `auth.json.lock` it holds — can never block subsequent launches indefinitely.
/// Matches the 15s budget the xAI OIDC refresh path already uses.
const REFRESH_REQUEST_TIMEOUT_SECS: u64 = 15;

/// Result of `POST /api/oauth/device_authorization`.
#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: String,
    pub expires_in: Option<i64>,
    pub interval: i64,
}

#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    user_code: String,
    device_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    verification_uri_complete: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    interval: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
}

impl TokenResponse {
    fn into_auth(self) -> GrokAuth {
        let now = Utc::now();
        GrokAuth {
            key: self.access_token,
            auth_mode: AuthMode::KimiCode,
            create_time: now,
            user_id: String::new(),
            email: None,
            refresh_token: Some(self.refresh_token),
            expires_at: Some(now + Duration::seconds(self.expires_in)),
            oidc_issuer: Some(
                xai_grok_models::PlatformId::KimiCode
                    .oauth_host()
                    .unwrap_or_else(|| "https://auth.kimi.com".into()),
            ),
            oidc_client_id: Some(KIMI_CODE_CLIENT_ID.to_owned()),
            // Store raw scope string in team_name field? Better use nothing extra.
            // scope/token_type not on GrokAuth — drop silently.
            ..Default::default()
        }
    }
}

#[derive(Deserialize, Default)]
struct OAuthErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// One poll tick against the token endpoint.
#[derive(Debug)]
pub(crate) enum DevicePollResult {
    Success(Box<GrokAuth>),
    Expired,
    /// User rejected the authorization request — do not keep polling.
    AccessDenied {
        description: Option<String>,
    },
    /// Non-retryable OAuth error (malformed success, unknown 4xx, etc.).
    Fatal {
        error: String,
        description: Option<String>,
    },
    /// Still waiting (`authorization_pending` / `slow_down` / similar).
    Pending {
        error: String,
        description: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RefreshError {
    #[error("token refresh unauthorized (HTTP {status}): {description}")]
    Unauthorized { status: u16, description: String },
    #[error("token refresh failed (HTTP {status}): {description}")]
    Fatal { status: u16, description: String },
    #[error("token refresh exhausted retries: {0}")]
    Exhausted(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

fn oauth_url(host: &str, path: &str) -> String {
    format!("{}{path}", host.trim_end_matches('/'))
}

fn with_device_headers(
    mut builder: reqwest::RequestBuilder,
) -> anyhow::Result<reqwest::RequestBuilder> {
    for (name, value) in device_headers()? {
        builder = builder.header(name, value);
    }
    Ok(builder)
}

fn validate_verification_uri(uri: &str) -> anyhow::Result<()> {
    if uri.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("Server returned invalid verification URI");
    }
    let parsed = url::Url::parse(uri)
        .map_err(|_| anyhow::anyhow!("Server returned invalid verification URI"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if matches!(parsed.host_str(), Some("localhost") | Some("127.0.0.1")) => Ok(()),
        _ => anyhow::bail!("Server returned unsupported verification URI scheme"),
    }
}

/// `POST {host}/api/oauth/device_authorization`
pub(crate) async fn request_device_authorization(
    host: &str,
) -> anyhow::Result<DeviceAuthorization> {
    let url = oauth_url(host, "/api/oauth/device_authorization");
    tracing::info!(url = %url, "auth: requesting Kimi device authorization");
    let resp = with_device_headers(crate::http::shared_client().post(&url))?
        .form(&[("client_id", KIMI_CODE_CLIENT_ID)])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Device authorization failed (HTTP {status}): {body}");
    }
    let parsed: DeviceAuthorizationResponse = resp.json().await?;

    if !parsed
        .user_code
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        anyhow::bail!("Server returned invalid user_code format (expected [A-Z0-9-])");
    }
    validate_verification_uri(&parsed.verification_uri_complete)?;
    if let Some(ref uri) = parsed.verification_uri {
        validate_verification_uri(uri)?;
    }

    Ok(DeviceAuthorization {
        user_code: parsed.user_code,
        device_code: parsed.device_code,
        verification_uri: parsed.verification_uri.filter(|u| !u.is_empty()),
        verification_uri_complete: parsed.verification_uri_complete,
        expires_in: parsed.expires_in.filter(|&e| e > 0),
        interval: parsed.interval.unwrap_or(5),
    })
}

/// One poll of `POST {host}/api/oauth/token` with the device grant.
pub(crate) async fn poll_device_token(
    host: &str,
    device_code: &str,
) -> anyhow::Result<DevicePollResult> {
    let url = oauth_url(host, "/api/oauth/token");
    let resp = with_device_headers(crate::http::shared_client().post(&url))?
        .form(&[
            ("client_id", KIMI_CODE_CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", DEVICE_GRANT_TYPE),
        ])
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Token polling request failed: {e}"))?;

    let status = resp.status();
    if status.is_server_error() {
        anyhow::bail!("Token polling server error: {status}");
    }
    let body = resp.bytes().await?;
    if status.is_success() {
        if let Ok(tokens) = serde_json::from_slice::<TokenResponse>(&body) {
            tracing::info!("auth: Kimi device poll succeeded");
            return Ok(DevicePollResult::Success(Box::new(tokens.into_auth())));
        }
        // 200 with unparseable body is a terminal protocol error, not pending.
        return Ok(DevicePollResult::Fatal {
            error: "missing_access_token".to_owned(),
            description: None,
        });
    }
    let err: OAuthErrorBody = serde_json::from_slice(&body).unwrap_or_default();
    let error = err.error.unwrap_or_else(|| "unknown_error".to_owned());
    match error.as_str() {
        "expired_token" => Ok(DevicePollResult::Expired),
        "access_denied" => Ok(DevicePollResult::AccessDenied {
            description: err.error_description,
        }),
        // RFC 8628: only these two are retryable pending states.
        "authorization_pending" | "slow_down" => Ok(DevicePollResult::Pending {
            error,
            description: err.error_description,
        }),
        _ => Ok(DevicePollResult::Fatal {
            error,
            description: err.error_description,
        }),
    }
}

/// Refresh an access token with exponential backoff on retryable statuses.
pub(crate) async fn refresh_token(
    host: &str,
    refresh_token: &str,
) -> Result<GrokAuth, RefreshError> {
    let url = oauth_url(host, "/api/oauth/token");
    let mut last_error = String::from("no attempt made");
    for attempt in 0..MAX_REFRESH_RETRIES {
        if attempt > 0 {
            let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
            tokio::time::sleep(backoff).await;
        }
        // Bound the whole request (connect + headers + body) so a stalled
        // proxy (e.g. fake-ip TUN that accepts the TCP handshake then never
        // responds) cannot wedge the refresh. Without this, the refresh holds
        // `auth.json.lock` for the unbounded duration of a hung connection,
        // which blocks every subsequent launch on the 45s lock timeout and
        // makes the TUI appear permanently stuck. Mirrors the per-request
        // timeout the xAI OIDC refresh path already sets.
        let send_result = with_device_headers(crate::http::shared_client().post(&url))?
            .form(&[
                ("client_id", KIMI_CODE_CLIENT_ID),
                ("grant_type", REFRESH_GRANT_TYPE),
                ("refresh_token", refresh_token),
            ])
            .timeout(std::time::Duration::from_secs(REFRESH_REQUEST_TIMEOUT_SECS))
            .send()
            .await;

        let resp = match send_result {
            Ok(resp) => resp,
            Err(e) => {
                last_error = format!("network error: {e}");
                continue;
            }
        };
        let status = resp.status().as_u16();
        let body = resp.bytes().await.unwrap_or_default();
        if status == 401 || status == 403 {
            let err: OAuthErrorBody = serde_json::from_slice(&body).unwrap_or_default();
            return Err(RefreshError::Unauthorized {
                status,
                description: err
                    .error_description
                    .unwrap_or_else(|| "Token refresh unauthorized.".to_owned()),
            });
        }
        if status == 200 {
            return match serde_json::from_slice::<TokenResponse>(&body) {
                Ok(tokens) => Ok(tokens.into_auth()),
                Err(e) => Err(RefreshError::Fatal {
                    status,
                    description: format!("malformed token payload: {e}"),
                }),
            };
        }
        let err: OAuthErrorBody = serde_json::from_slice(&body).unwrap_or_default();
        let description = err
            .error_description
            .unwrap_or_else(|| format!("Token refresh failed (HTTP {status})."));
        if RETRYABLE_REFRESH_STATUSES.contains(&status) {
            last_error = description;
            continue;
        }
        return Err(RefreshError::Fatal {
            status,
            description,
        });
    }
    Err(RefreshError::Exhausted(last_error))
}

// silence unused field warning on TokenResponse.scope/token_type when into_auth drops them
impl TokenResponse {
    #[allow(dead_code)]
    fn _scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }
    #[allow(dead_code)]
    fn _token_type(&self) -> Option<&str> {
        self.token_type.as_deref()
    }
}

#[cfg(test)]
mod poll_error_mapping_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn spawn_token_server(responses: Vec<(u16, serde_json::Value)>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let counter = Arc::new(AtomicUsize::new(0));
        let responses = Arc::new(responses);
        let app = axum::Router::new().route(
            "/api/oauth/token",
            axum::routing::post(move || {
                let counter = counter.clone();
                let responses = responses.clone();
                async move {
                    let idx = counter
                        .fetch_add(1, Ordering::SeqCst)
                        .min(responses.len().saturating_sub(1));
                    let (status, body) = &responses[idx];
                    (
                        axum::http::StatusCode::from_u16(*status).unwrap(),
                        axum::Json(body.clone()),
                    )
                }
            }),
        );
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (host, handle)
    }

    #[tokio::test]
    async fn poll_maps_access_denied_to_access_denied() {
        let (host, server) = spawn_token_server(vec![(
            400,
            serde_json::json!({ "error": "access_denied", "error_description": "user said no" }),
        )])
        .await;
        let result = poll_device_token(&host, "dc").await.unwrap();
        server.abort();
        match result {
            DevicePollResult::AccessDenied { description } => {
                assert_eq!(description.as_deref(), Some("user said no"));
            }
            other => panic!("expected AccessDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_maps_expired_token_to_expired() {
        let (host, server) =
            spawn_token_server(vec![(400, serde_json::json!({ "error": "expired_token" }))]).await;
        let result = poll_device_token(&host, "dc").await.unwrap();
        server.abort();
        assert!(matches!(result, DevicePollResult::Expired));
    }

    #[tokio::test]
    async fn poll_maps_authorization_pending_to_pending() {
        let (host, server) = spawn_token_server(vec![(
            400,
            serde_json::json!({ "error": "authorization_pending" }),
        )])
        .await;
        let result = poll_device_token(&host, "dc").await.unwrap();
        server.abort();
        match result {
            DevicePollResult::Pending { error, .. } => {
                assert_eq!(error, "authorization_pending");
            }
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_maps_unknown_error_to_fatal() {
        let (host, server) =
            spawn_token_server(vec![(400, serde_json::json!({ "error": "invalid_grant" }))]).await;
        let result = poll_device_token(&host, "dc").await.unwrap();
        server.abort();
        match result {
            DevicePollResult::Fatal { error, .. } => assert_eq!(error, "invalid_grant"),
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_maps_malformed_success_to_fatal() {
        let (host, server) =
            spawn_token_server(vec![(200, serde_json::json!({ "not": "a token" }))]).await;
        let result = poll_device_token(&host, "dc").await.unwrap();
        server.abort();
        match result {
            DevicePollResult::Fatal { error, .. } => assert_eq!(error, "missing_access_token"),
            other => panic!("expected Fatal for malformed success, got {other:?}"),
        }
    }
}
