//! Radius gateway OAuth and dynamic-catalog helpers.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use serde::Deserialize;
use sha2::Digest as _;

use crate::auth::model::{AuthMode, GrokAuth};

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";
pub const CLIENT_ID: &str = "pi-gateway";
pub const SCOPE: &str = "gateway offline_access";
pub const REDIRECT_URI: &str = "http://127.0.0.1:1456/oauth/callback";
pub const CALLBACK_PORT: u16 = 1456;
pub const CALLBACK_PATH: &str = "/oauth/callback";
pub const DEVICE_SLOW_DOWN_INCREMENT_SECS: u64 = 5;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Pkce {
    let random_bytes: [u8; 32] = rand::random();
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

pub fn create_state() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn path_has_unsafe_segments(path: &str) -> bool {
    if path.contains('\\') {
        return true;
    }
    path.split('/').any(|segment| {
        let lower = segment.to_ascii_lowercase();
        if lower.contains("%2f") || lower.contains("%5c") || lower.contains("%00") {
            return true;
        }
        let dots = lower.replace("%2e", ".");
        dots == "." || dots == ".."
    })
}

fn raw_candidate_path_is_unsafe(candidate: &str) -> bool {
    if candidate.contains('\\') {
        return true;
    }
    let Some((_, authority_and_path)) = candidate.split_once("://") else {
        return true;
    };
    let Some(path_start) = authority_and_path.find('/') else {
        return false;
    };
    let raw_path = &authority_and_path[path_start..];
    let raw_path = raw_path.split(['?', '#']).next().unwrap_or(raw_path);
    path_has_unsafe_segments(raw_path)
}

/// Normalize the configured gateway/base URL without changing its path.
///
/// The normalized value retains a safe path for model `baseUrl` use. Radius
/// control-plane endpoint helpers intentionally replace that path with `/v1/*`
/// (matching Pi's `new URL("/v1/...", gateway)` semantics). Query strings,
/// fragments, userinfo, control characters, encoded separators, and dot
/// segments are rejected to keep every request on one explicit route family.
pub fn normalize_gateway_root(input: &str) -> anyhow::Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("invalid Radius gateway URL");
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    if raw_candidate_path_is_unsafe(&candidate) {
        anyhow::bail!("invalid Radius gateway URL");
    }
    let mut url = url::Url::parse(&candidate)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || path_has_unsafe_segments(url.path())
    {
        anyhow::bail!("invalid Radius gateway URL");
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn gateway_endpoint(gateway: &str, segments: &[&str]) -> anyhow::Result<url::Url> {
    let root = normalize_gateway_root(gateway)?;
    let mut url = url::Url::parse(&root)?;
    url.set_path("/");
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid Radius gateway URL"))?
        .pop_if_empty()
        .extend(segments);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

pub fn config_url(gateway: &str) -> anyhow::Result<String> {
    Ok(gateway_endpoint(gateway, &["v1", "config"])?.to_string())
}

/// Resolve an environment-selected gateway. Invalid explicit values are an
/// error rather than silently falling back to the public Radius service.
pub fn try_gateway_from_env_or_default() -> anyhow::Result<String> {
    let configured = std::env::var("GROK_RADIUS_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("RADIUS_GATEWAY_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
        });
    match configured {
        Some(value) => normalize_gateway_root(&value),
        None => Ok(DEFAULT_RADIUS_GATEWAY.to_string()),
    }
}

/// Compatibility helper for callers that cannot surface configuration errors.
/// Interactive login uses [`try_gateway_from_env_or_default`] instead.
pub fn gateway_from_env_or_default() -> String {
    try_gateway_from_env_or_default().unwrap_or_else(|error| {
        tracing::warn!(%error, "invalid Radius gateway environment; using compiled default");
        DEFAULT_RADIUS_GATEWAY.to_string()
    })
}

#[derive(Deserialize)]
struct DiscoveryResponse {
    #[serde(rename = "authorizationEndpoint")]
    authorization_endpoint: Option<String>,
}

fn validate_http_url(raw: &str, allow_loopback_http: bool) -> anyhow::Result<String> {
    if raw.chars().any(|c| c.is_ascii_control()) || raw_candidate_path_is_unsafe(raw) {
        anyhow::bail!("untrusted URL");
    }
    let url = url::Url::parse(raw)?;
    let loopback = matches!(
        url.host_str(),
        Some("127.0.0.1") | Some("localhost") | Some("::1")
    );
    if !(url.scheme() == "https" || (allow_loopback_http && url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || path_has_unsafe_segments(url.path())
    {
        anyhow::bail!("untrusted URL");
    }
    Ok(url.to_string())
}

pub async fn discover_authorization_endpoint(gateway: &str) -> anyhow::Result<String> {
    let response = crate::http::shared_client()
        .get(gateway_endpoint(gateway, &["v1", "oauth"])?)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("Radius OAuth discovery failed (HTTP {status})");
    }
    let json: DiscoveryResponse = response.json().await?;
    let endpoint = json
        .authorization_endpoint
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Radius OAuth discovery missing authorizationEndpoint"))?;
    validate_http_url(&endpoint, true)
}

pub fn build_authorize_url(endpoint: &str, challenge: &str, state: &str) -> anyhow::Result<String> {
    let mut url = url::Url::parse(&validate_http_url(endpoint, true)?)?;
    // Pi replaces (rather than appends to) discovery-supplied query params so
    // an endpoint cannot inject duplicate redirect/state/client values.
    url.set_query(None);
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("handoff", "url");
    Ok(url.to_string())
}

#[derive(Debug, Clone)]
pub struct RadiusToken {
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

fn token_from_response(json: TokenResponse) -> anyhow::Result<RadiusToken> {
    let access = json
        .access_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Radius token response missing access_token"))?;
    let refresh = json
        .refresh_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Radius token response missing refresh_token"))?;
    let expires_in_secs = json
        .expires_in
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow::anyhow!("Radius token response missing positive expires_in"))?;
    Ok(RadiusToken {
        access,
        refresh,
        expires_in_secs,
    })
}

#[derive(Deserialize, Default)]
struct OAuthErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
}

fn oauth_error_message(body: &str) -> Option<String> {
    let parsed: OAuthErrorResponse = serde_json::from_str(body).ok()?;
    let error = parsed.error?.trim().to_owned();
    if error.is_empty() {
        return None;
    }
    let description = parsed.error_description.unwrap_or_default();
    let description = description.trim();
    Some(if description.is_empty() {
        error
    } else {
        format!("{error}: {description}")
    })
}

async fn read_token_response(response: reqwest::Response, op: &str) -> anyhow::Result<RadiusToken> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let detail =
            oauth_error_message(&body).unwrap_or_else(|| "unparseable error response".into());
        anyhow::bail!("Radius token {op} failed (HTTP {status}): {detail}");
    }
    token_from_response(response.json().await?)
}

fn token_url(gateway: &str) -> anyhow::Result<url::Url> {
    gateway_endpoint(gateway, &["v1", "oauth", "token"])
}

pub async fn exchange_authorization_code(
    gateway: &str,
    code: &str,
    verifier: &str,
) -> anyhow::Result<RadiusToken> {
    let code = code.trim();
    if code.is_empty() || verifier.trim().is_empty() {
        anyhow::bail!("Radius authorization response is incomplete");
    }
    let response = crate::http::shared_client()
        .post(token_url(gateway)?)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .await?;
    read_token_response(response, "exchange").await
}

#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    interval: Option<u64>,
    expires_in: Option<u64>,
}

fn device_url(gateway: &str) -> anyhow::Result<url::Url> {
    gateway_endpoint(gateway, &["v1", "oauth", "device"])
}

pub async fn start_device_flow(gateway: &str) -> anyhow::Result<DeviceAuthorization> {
    let response = crate::http::shared_client()
        .post(device_url(gateway)?)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let detail =
            oauth_error_message(&body).unwrap_or_else(|| "unparseable error response".into());
        anyhow::bail!("Radius device authorization failed (HTTP {status}): {detail}");
    }
    let raw: DeviceResponse = response.json().await?;
    Ok(DeviceAuthorization {
        device_code: raw
            .device_code
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Radius device response missing device_code"))?,
        user_code: raw
            .user_code
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Radius device response missing user_code"))?,
        verification_uri: validate_http_url(
            &raw.verification_uri.ok_or_else(|| {
                anyhow::anyhow!("Radius device response missing verification_uri")
            })?,
            true,
        )?,
        interval: raw.interval.unwrap_or(5).max(1),
        expires_in: raw
            .expires_in
            .filter(|v| *v > 0)
            .ok_or_else(|| anyhow::anyhow!("Radius device response missing positive expires_in"))?,
    })
}

#[derive(Debug)]
pub enum DevicePollTick {
    Complete(RadiusToken),
    Pending,
    SlowDown { interval: Option<u64> },
}

pub async fn poll_device_token_once(
    gateway: &str,
    device_code: &str,
) -> anyhow::Result<DevicePollTick> {
    let response = crate::http::shared_client()
        .post(token_url(gateway)?)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await?;
    if response.status().is_success() {
        return read_token_response(response, "device")
            .await
            .map(DevicePollTick::Complete);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let parsed = serde_json::from_str::<OAuthErrorResponse>(&body).unwrap_or_default();
    match parsed.error.as_deref() {
        Some("authorization_pending") => Ok(DevicePollTick::Pending),
        Some("slow_down") => Ok(DevicePollTick::SlowDown {
            interval: parsed.interval.filter(|value| *value > 0),
        }),
        Some("expired_token") | Some("access_denied") => {
            let detail =
                oauth_error_message(&body).unwrap_or_else(|| "authorization failed".into());
            anyhow::bail!("Radius device authorization {detail}")
        }
        _ => {
            let detail =
                oauth_error_message(&body).unwrap_or_else(|| "unparseable error response".into());
            anyhow::bail!("Radius device token failed (HTTP {status}): {detail}")
        }
    }
}

pub async fn refresh_access_token(gateway: &str, refresh: &str) -> anyhow::Result<RadiusToken> {
    let refresh = refresh.trim();
    if refresh.is_empty() {
        anyhow::bail!("Radius refresh token is empty");
    }
    let response = crate::http::shared_client()
        .post(token_url(gateway)?)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await?;
    read_token_response(response, "refresh").await
}

pub fn credentials_from_token(
    token: RadiusToken,
    previous_refresh: Option<&str>,
    gateway: &str,
) -> GrokAuth {
    let refresh = if token.refresh.trim().is_empty() {
        previous_refresh.unwrap_or_default().to_string()
    } else {
        token.refresh
    };
    let now = Utc::now();
    let expires_at = now
        .checked_add_signed(Duration::seconds(token.expires_in_secs.saturating_sub(60)))
        .unwrap_or(now);
    GrokAuth {
        key: token.access,
        auth_mode: AuthMode::Radius,
        create_time: now,
        user_id: "radius".into(),
        email: None,
        first_name: None,
        last_name: None,
        profile_image_asset_id: None,
        principal_type: None,
        principal_id: None,
        team_id: None,
        team_name: None,
        team_role: None,
        organization_id: None,
        organization_name: None,
        organization_role: None,
        user_blocked_reason: None,
        team_blocked_reasons: Vec::new(),
        coding_data_retention_opt_out: crate::auth::default_coding_data_retention_opt_out(),
        has_grok_code_access: None,
        refresh_token: Some(refresh),
        expires_at: Some(expires_at),
        oidc_issuer: None,
        oidc_client_id: Some(CLIENT_ID.into()),
        account_id: None,
        platform_base_url: normalize_gateway_root(gateway).ok(),
        github_domain: None,
        github_copilot_base_url: None,
        github_copilot_available_models: None,
        aws_profile: None,
        aws_credential_chain: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_gateway_and_joins_config_path() {
        assert_eq!(
            normalize_gateway_root("radius.pi.dev/").unwrap(),
            "https://radius.pi.dev"
        );
        assert_eq!(
            config_url("https://example.com/base").unwrap(),
            "https://example.com/v1/config"
        );
        assert!(normalize_gateway_root("https://u@example.com").is_err());
        assert!(normalize_gateway_root("javascript:alert(1)").is_err());
        assert!(normalize_gateway_root("https://example.com/base/../evil").is_err());
        assert!(normalize_gateway_root("https://example.com/%2e%2e/evil").is_err());
        assert!(normalize_gateway_root("https://example.com/base?route=other").is_err());
    }

    #[test]
    fn authorize_url_contains_pkce_state_and_handoff() {
        let url = build_authorize_url(
            "http://127.0.0.1:9/auth?client_id=attacker&redirect_uri=https://evil.invalid",
            "challenge",
            "state",
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(
            parsed
                .query_pairs()
                .filter(|(key, _)| key == "client_id")
                .count(),
            1
        );
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(params.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some("challenge")
        );
        assert_eq!(params.get("state").map(String::as_str), Some("state"));
        assert_eq!(params.get("handoff").map(String::as_str), Some("url"));
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URI)
        );
    }

    #[test]
    fn token_response_requires_complete_rotating_credentials() {
        assert!(
            token_from_response(TokenResponse {
                access_token: Some("access".into()),
                refresh_token: None,
                expires_in: Some(3600),
            })
            .is_err()
        );
        assert!(
            token_from_response(TokenResponse {
                access_token: Some("access".into()),
                refresh_token: Some("refresh".into()),
                expires_in: Some(0),
            })
            .is_err()
        );
    }
}
