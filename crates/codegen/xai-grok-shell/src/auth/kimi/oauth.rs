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

/// Per-attempt total timeout (connect + response) for token-refresh POSTs.
/// Bounds a stalled network path (e.g. a fake-ip proxy that accepts the TCP
/// handshake then never responds) so the refresh — and the `auth.json.lock`
/// it holds — can never block subsequent launches indefinitely. Driven by
/// `tokio::time::timeout` (not reqwest's `.timeout()`, which does not reliably
/// abort an in-flight request against a stalled peer).
const REFRESH_REQUEST_TIMEOUT_SECS: u64 = 15;

/// Convenience accessor for the per-attempt refresh timeout as a `Duration`.
fn refresh_request_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(REFRESH_REQUEST_TIMEOUT_SECS)
}

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
    refresh_token_with_timeout(host, refresh_token, refresh_request_timeout()).await
}

async fn refresh_token_with_timeout(
    host: &str,
    refresh_token: &str,
    request_timeout: std::time::Duration,
) -> Result<GrokAuth, RefreshError> {
    let url = oauth_url(host, "/api/oauth/token");
    let mut last_error = String::from("no attempt made");
    for attempt in 0..MAX_REFRESH_RETRIES {
        if attempt > 0 {
            let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
            tokio::time::sleep(backoff).await;
        }
        // Bound the whole request (connect + response headers) so a stalled
        // proxy (e.g. fake-ip TUN that accepts the TCP handshake then never
        // responds) cannot wedge the refresh. reqwest's per-request `.timeout()`
        // does not reliably abort an in-flight request against a stalled peer
        // in practice, so drive the deadline with `tokio::time::timeout` instead.
        // The refresh holds `auth.json.lock` for its whole duration; without a
        // reliable bound one hung request blocks every subsequent launch on
        // the 45s lock timeout and makes the TUI appear permanently stuck.
        let request = with_device_headers(crate::http::shared_client().post(&url))?.form(&[
            ("client_id", KIMI_CODE_CLIENT_ID),
            ("grant_type", REFRESH_GRANT_TYPE),
            ("refresh_token", refresh_token),
        ]);
        let send_result = tokio::time::timeout(request_timeout, request.send()).await;

        // A timeout means the network path is stalled. Treat it as terminal
        // (do NOT retry): retrying would hold `auth.json.lock` for up to
        // 3 × timeout and wedge any concurrent refresh — and TUI startup —
        // behind it. Fail fast so the lock is released; the next request that
        // needs a Kimi bearer re-invokes the refresh and retries naturally.
        let resp = match send_result {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                last_error = format!("network error: {e}");
                continue;
            }
            Err(_elapsed) => {
                xai_grok_telemetry::unified_log::warn(
                    "auth.kimi.refresh_token.timeout_fired",
                    None,
                    Some(serde_json::json!({
                        "timeout_millis": request_timeout.as_millis(),
                    })),
                );
                return Err(RefreshError::Fatal {
                    status: 0,
                    description: format!(
                        "token refresh timed out after {request_timeout:?} (network path stalled)"
                    ),
                });
            }
        };
        let status = resp.status().as_u16();
        let body = match tokio::time::timeout(request_timeout, resp.bytes()).await {
            Ok(Ok(b)) => b,
            Ok(Err(_)) | Err(_) => Default::default(),
        };
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

    async fn spawn_token_server(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
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

    async fn spawn_stalled_token_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/api/oauth/token",
            axum::routing::post({
                let hits = hits.clone();
                move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<()>().await;
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (host, hits, handle)
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

    #[tokio::test]
    async fn refresh_stalled_peer_times_out_without_retrying() {
        let (host, hits, server) = spawn_stalled_token_server().await;
        let result =
            refresh_token_with_timeout(&host, "refresh", std::time::Duration::from_millis(150))
                .await;
        server.abort();

        match result {
            Err(RefreshError::Fatal {
                status: 0,
                description,
            }) => assert!(description.contains("timed out"), "{description}"),
            other => panic!("expected terminal timeout, got {other:?}"),
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a stalled network path must not be retried while auth.json.lock is held"
        );
    }
}
