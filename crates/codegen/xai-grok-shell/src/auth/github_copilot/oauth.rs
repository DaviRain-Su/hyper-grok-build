//! GitHub Copilot OAuth wire protocol.
//!
//! Ported from pinned Pi `packages/ai/src/auth/oauth/github-copilot.ts` in
//! `@earendil-works/pi-ai` 0.82.1. The durable credential is a GitHub OAuth
//! access token from the device flow; every inference request uses the short
//! Copilot token returned by `GET /copilot_internal/v2/token`.

use chrono::{DateTime, TimeZone as _, Utc};
use futures::stream::{self, StreamExt as _};
use serde::Deserialize;
use std::collections::BTreeSet;

use crate::auth::model::{AuthMode, GrokAuth};

/// Pi decodes `SXYxLmI1MDdhMDhjODdlY2ZlOTg=` to this GitHub OAuth app id.
pub(crate) const GITHUB_COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub(crate) const DEFAULT_GITHUB_DOMAIN: &str = "github.com";
pub(crate) const DEFAULT_COPILOT_BASE_URL: &str = "https://api.individual.githubcopilot.com";

const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_REQUEST_TIMEOUT_SECS: u64 = 15;
const COPILOT_POLICY_TIMEOUT_SECS: u64 = 15;
const COPILOT_AVAILABILITY_TIMEOUT_SECS: u64 = 15;
const COPILOT_POLICY_CONCURRENCY: usize = 8;
const COPILOT_ERROR_BODY_LIMIT: usize = 512;
const COPILOT_TOKEN_EXPIRY_SKEW_SECS: i64 = 5 * 60;

pub(crate) const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
pub(crate) const COPILOT_EDITOR_VERSION: &str = "vscode/1.107.0";
pub(crate) const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
pub(crate) const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

fn refresh_request_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(REFRESH_REQUEST_TIMEOUT_SECS)
}

/// Normalize a GitHub Enterprise URL/domain to a bare hostname. Blank => None.
pub(crate) fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_ascii_control()) {
        return None;
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let url = url::Url::parse(&candidate).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.contains(['/', '\\', '@', ':']) {
        return None;
    }
    Some(host)
}

pub(crate) fn domain_or_default(domain: Option<&str>) -> String {
    domain
        .and_then(normalize_domain)
        .unwrap_or_else(|| DEFAULT_GITHUB_DOMAIN.to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitHubCopilotUrls {
    pub device_code_url: String,
    pub access_token_url: String,
    pub copilot_token_url: String,
}

pub(crate) fn get_urls(domain: &str) -> anyhow::Result<GitHubCopilotUrls> {
    let domain =
        normalize_domain(domain).ok_or_else(|| anyhow::anyhow!("invalid GitHub domain"))?;
    Ok(GitHubCopilotUrls {
        device_code_url: format!("https://{domain}/login/device/code"),
        access_token_url: format!("https://{domain}/login/oauth/access_token"),
        copilot_token_url: format!("https://api.{domain}/copilot_internal/v2/token"),
    })
}

pub(crate) fn add_copilot_headers(mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    builder = builder.header("User-Agent", COPILOT_USER_AGENT);
    builder = builder.header("Editor-Version", COPILOT_EDITOR_VERSION);
    builder = builder.header("Editor-Plugin-Version", COPILOT_EDITOR_PLUGIN_VERSION);
    builder.header("Copilot-Integration-Id", COPILOT_INTEGRATION_ID)
}

fn truncate_error_body(body: &str) -> String {
    body.chars().take(COPILOT_ERROR_BODY_LIMIT).collect()
}

pub(crate) fn copilot_models_url(base_url: &str) -> anyhow::Result<String> {
    let mut url = safe_copilot_base_url(base_url)?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid GitHub Copilot base URL"))?
        .push("models");
    Ok(url.to_string())
}

pub(crate) fn copilot_model_policy_url(base_url: &str, model_id: &str) -> anyhow::Result<String> {
    if model_id.trim().is_empty() || model_id.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("invalid GitHub Copilot model id");
    }
    let mut url = safe_copilot_base_url(base_url)?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid GitHub Copilot base URL"))?
        .extend(["models", model_id, "policy"]);
    Ok(url.to_string())
}

fn safe_copilot_base_url(base_url: &str) -> anyhow::Result<url::Url> {
    let mut url = url::Url::parse(base_url.trim())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!("invalid GitHub Copilot base URL");
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url)
}

fn validate_verification_uri(uri: &str) -> anyhow::Result<String> {
    if uri.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("Untrusted verification_uri in device code response");
    }
    let parsed = url::Url::parse(uri)
        .map_err(|_| anyhow::anyhow!("Untrusted verification_uri in device code response"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("Untrusted verification_uri in device code response");
    }
    Ok(parsed.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    interval: Option<u64>,
    expires_in: Option<u64>,
}

pub(crate) fn parse_device_authorization_json(bytes: &[u8]) -> anyhow::Result<DeviceAuthorization> {
    let raw: DeviceCodeResponse = serde_json::from_slice(bytes)?;
    let device_code = raw
        .device_code
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Invalid device code response fields"))?;
    let user_code = raw
        .user_code
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Invalid device code response fields"))?;
    let verification_uri = validate_verification_uri(
        raw.verification_uri
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Invalid device code response fields"))?,
    )?;
    let expires_in = raw
        .expires_in
        .filter(|v| *v > 0)
        .ok_or_else(|| anyhow::anyhow!("Invalid device code response fields"))?;
    Ok(DeviceAuthorization {
        device_code,
        user_code,
        verification_uri,
        interval: raw.interval.unwrap_or(5).max(1),
        expires_in,
    })
}

pub(crate) async fn start_device_flow(domain: &str) -> anyhow::Result<DeviceAuthorization> {
    let urls = get_urls(domain)?;
    let response = crate::http::shared_client()
        .post(&urls.device_code_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", COPILOT_USER_AGENT)
        .form(&[
            ("client_id", GITHUB_COPILOT_CLIENT_ID),
            ("scope", "read:user"),
        ])
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        anyhow::bail!("GitHub Copilot device code request failed (HTTP {status})");
    }
    parse_device_authorization_json(&body)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DevicePollTick {
    Complete { github_access_token: String },
    Pending,
    SlowDown { interval: Option<u64> },
    Failed { message: String },
}

#[derive(Deserialize, Default)]
struct DeviceTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
}

pub(crate) fn parse_device_poll_json(bytes: &[u8]) -> DevicePollTick {
    let Ok(raw) = serde_json::from_slice::<DeviceTokenResponse>(bytes) else {
        return DevicePollTick::Failed {
            message: "Invalid device token response".to_owned(),
        };
    };
    if let Some(access_token) = raw.access_token.filter(|v| !v.trim().is_empty()) {
        return DevicePollTick::Complete {
            github_access_token: access_token,
        };
    }
    match raw.error.as_deref() {
        Some("authorization_pending") => DevicePollTick::Pending,
        Some("slow_down") => DevicePollTick::SlowDown {
            interval: raw.interval.filter(|v| *v > 0),
        },
        Some(error) => {
            let suffix = raw
                .error_description
                .filter(|d| !d.trim().is_empty())
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            DevicePollTick::Failed {
                message: format!("Device flow failed: {error}{suffix}"),
            }
        }
        None => DevicePollTick::Failed {
            message: "Invalid device token response".to_owned(),
        },
    }
}

pub(crate) async fn poll_device_token_once(
    domain: &str,
    device_code: &str,
) -> anyhow::Result<DevicePollTick> {
    let urls = get_urls(domain)?;
    let response = crate::http::shared_client()
        .post(&urls.access_token_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", COPILOT_USER_AGENT)
        .form(&[
            ("client_id", GITHUB_COPILOT_CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", DEVICE_GRANT_TYPE),
        ])
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        return Ok(DevicePollTick::Failed {
            message: format!("Device flow failed (HTTP {status})"),
        });
    }
    Ok(parse_device_poll_json(&body))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopilotToken {
    pub access: String,
    pub expires_at: DateTime<Utc>,
    pub base_url: String,
}

#[derive(Deserialize)]
struct CopilotTokenResponse {
    token: Option<String>,
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<CopilotModelAvailability>,
}

#[derive(Deserialize)]
struct CopilotModelAvailability {
    id: Option<String>,
    #[serde(default)]
    model_picker_enabled: bool,
    #[serde(default)]
    policy: Option<CopilotModelPolicy>,
    #[serde(default)]
    capabilities: Option<CopilotModelCapabilities>,
}

#[derive(Deserialize)]
struct CopilotModelPolicy {
    state: Option<String>,
}

#[derive(Deserialize)]
struct CopilotModelCapabilities {
    #[serde(default)]
    supports: Option<CopilotModelSupports>,
}

#[derive(Deserialize)]
struct CopilotModelSupports {
    #[serde(default)]
    tool_calls: Option<bool>,
}

pub(crate) fn github_copilot_catalog_model_ids() -> Vec<String> {
    xai_grok_models::platform_builtin_models()
        .iter()
        .filter(|model| model.provider.as_str() == "github-copilot")
        .map(|model| model.model.clone())
        .collect()
}

pub(crate) fn base_url_from_copilot_token(token: &str) -> Option<String> {
    let proxy_host = token
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))?
        .trim();
    if proxy_host.is_empty()
        || proxy_host.chars().any(|c| c.is_ascii_control())
        || proxy_host.contains(['/', '\\', '@', ':'])
    {
        return None;
    }
    let api_host = proxy_host
        .strip_prefix("proxy.")
        .map(|rest| format!("api.{rest}"))
        .unwrap_or_else(|| proxy_host.to_owned());
    let url = format!("https://{api_host}");
    let parsed = url::Url::parse(&url).ok()?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() || parsed.path() != "/" {
        return None;
    }
    Some(url.trim_end_matches('/').to_owned())
}

pub fn github_copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    if let Some(token) = token
        && let Some(base) = base_url_from_copilot_token(token)
    {
        return base;
    }
    if let Some(domain) = enterprise_domain.and_then(normalize_domain) {
        return format!("https://copilot-api.{domain}");
    }
    DEFAULT_COPILOT_BASE_URL.to_owned()
}

pub(crate) fn parse_copilot_token_json(bytes: &[u8]) -> anyhow::Result<CopilotToken> {
    let raw: CopilotTokenResponse = serde_json::from_slice(bytes)?;
    let token = raw
        .token
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Invalid Copilot token response fields"))?;
    let expires_at_secs = raw
        .expires_at
        .filter(|v| *v > 0)
        .ok_or_else(|| anyhow::anyhow!("Invalid Copilot token response fields"))?;
    let adjusted = expires_at_secs.saturating_sub(COPILOT_TOKEN_EXPIRY_SKEW_SECS);
    let expires_at = Utc
        .timestamp_opt(adjusted, 0)
        .single()
        .ok_or_else(|| anyhow::anyhow!("Invalid Copilot token expires_at"))?;
    let base_url = github_copilot_base_url(Some(&token), None);
    Ok(CopilotToken {
        access: token,
        expires_at,
        base_url,
    })
}

pub(crate) fn parse_available_model_ids_json(bytes: &[u8]) -> anyhow::Result<Vec<String>> {
    let response: ModelsResponse = serde_json::from_slice(bytes)?;
    Ok(filter_available_models(response.data))
}

fn filter_available_models(models: Vec<CopilotModelAvailability>) -> Vec<String> {
    let catalog: BTreeSet<String> = github_copilot_catalog_model_ids().into_iter().collect();
    let mut ids = Vec::new();
    for model in models {
        let Some(id) = model.id.filter(|id| catalog.contains(id)) else {
            continue;
        };
        if !model.model_picker_enabled {
            continue;
        }
        if model
            .policy
            .as_ref()
            .and_then(|policy| policy.state.as_deref())
            == Some("disabled")
        {
            continue;
        }
        if model
            .capabilities
            .as_ref()
            .and_then(|cap| cap.supports.as_ref())
            .and_then(|supports| supports.tool_calls)
            == Some(false)
        {
            continue;
        }
        ids.push(id);
    }
    ids.sort();
    ids.dedup();
    ids
}

pub(crate) async fn refresh_copilot_access_token(
    github_access_token: &str,
    enterprise_domain: Option<&str>,
) -> anyhow::Result<CopilotToken> {
    let domain = domain_or_default(enterprise_domain);
    let urls = get_urls(&domain)?;
    let request = add_copilot_headers(
        crate::http::shared_client()
            .get(&urls.copilot_token_url)
            .header("Accept", "application/json")
            .bearer_auth(github_access_token),
    );
    let response = tokio::time::timeout(refresh_request_timeout(), request.send())
        .await
        .map_err(|_| anyhow::anyhow!("GitHub Copilot token refresh timed out"))??;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        anyhow::bail!("GitHub Copilot token refresh failed (HTTP {status})");
    }
    let mut token = parse_copilot_token_json(&body)?;
    token.base_url = github_copilot_base_url(Some(&token.access), enterprise_domain);
    Ok(token)
}

pub(crate) async fn initialize_copilot_model_availability(
    access_token: &str,
    base_url: &str,
) -> anyhow::Result<Vec<String>> {
    // Pi enables policies once during interactive login. Refreshes only reload
    // availability, avoiding 29 policy writes on every short-token rotation.
    enable_catalog_model_policies(access_token, base_url).await;
    fetch_available_model_ids(access_token, base_url).await
}

pub(crate) async fn refresh_copilot_model_availability(
    access_token: &str,
    base_url: &str,
) -> anyhow::Result<Vec<String>> {
    fetch_available_model_ids(access_token, base_url).await
}

async fn enable_catalog_model_policies(access_token: &str, base_url: &str) {
    let ids = github_copilot_catalog_model_ids();
    stream::iter(ids)
        .for_each_concurrent(COPILOT_POLICY_CONCURRENCY, |model_id| async move {
            if let Err(e) = enable_one_model_policy(access_token, base_url, &model_id).await {
                tracing::warn!(model_id = %model_id, error = %e, "github-copilot auth: model policy enable failed");
            }
        })
        .await;
}

async fn enable_one_model_policy(
    access_token: &str,
    base_url: &str,
    model_id: &str,
) -> anyhow::Result<()> {
    let url = copilot_model_policy_url(base_url, model_id)?;
    let request = add_copilot_headers(
        crate::http::shared_client()
            .post(url)
            .bearer_auth(access_token)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("openai-intent", "chat-policy")
            .header("x-interaction-type", "chat-policy")
            .json(&serde_json::json!({ "state": "enabled" })),
    );
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(COPILOT_POLICY_TIMEOUT_SECS),
        request.send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("GitHub Copilot model policy request timed out"))??;
    if !response.status().is_success() {
        let status = response.status();
        let body = truncate_error_body(&response.text().await.unwrap_or_default());
        anyhow::bail!("GitHub Copilot model policy request failed (HTTP {status}): {body}");
    }
    Ok(())
}

async fn fetch_available_model_ids(
    access_token: &str,
    base_url: &str,
) -> anyhow::Result<Vec<String>> {
    let url = copilot_models_url(base_url)?;
    let request = add_copilot_headers(
        crate::http::shared_client()
            .get(url)
            .bearer_auth(access_token)
            .header("Accept", "application/json")
            .header("X-GitHub-Api-Version", "2026-06-01"),
    );
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(COPILOT_AVAILABILITY_TIMEOUT_SECS),
        request.send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("GitHub Copilot model availability request timed out"))??;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        let body = truncate_error_body(&String::from_utf8_lossy(&body));
        anyhow::bail!("GitHub Copilot model availability request failed (HTTP {status}): {body}");
    }
    parse_available_model_ids_json(&body)
}

pub(crate) fn credentials_from_token(
    token: CopilotToken,
    github_access_token: String,
    enterprise_domain: Option<String>,
) -> GrokAuth {
    GrokAuth {
        key: token.access,
        auth_mode: AuthMode::GitHubCopilot,
        create_time: Utc::now(),
        refresh_token: Some(github_access_token),
        expires_at: Some(token.expires_at),
        oidc_issuer: None,
        oidc_client_id: Some(GITHUB_COPILOT_CLIENT_ID.to_owned()),
        github_domain: Some(domain_or_default(enterprise_domain.as_deref())),
        github_copilot_base_url: Some(token.base_url),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_matches_pi_base64_decode() {
        assert_eq!(GITHUB_COPILOT_CLIENT_ID, "Iv1.b507a08c87ecfe98");
    }

    #[test]
    fn catalog_model_policy_set_matches_locked_pi_count() {
        assert_eq!(github_copilot_catalog_model_ids().len(), 29);
    }

    #[test]
    fn normalizes_domains_and_builds_urls() {
        assert_eq!(
            normalize_domain(" github.com ").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            normalize_domain("https://company.ghe.com/path").as_deref(),
            Some("company.ghe.com")
        );
        assert!(normalize_domain("https://user@github.com").is_none());
        let urls = get_urls("github.com").unwrap();
        assert_eq!(
            urls.copilot_token_url,
            "https://api.github.com/copilot_internal/v2/token"
        );
    }

    #[test]
    fn validates_device_authorization_uri() {
        let ok = br#"{"device_code":"dev","user_code":"USER-CODE","verification_uri":"https://github.com/login/device","interval":5,"expires_in":900}"#;
        assert_eq!(
            parse_device_authorization_json(ok)
                .unwrap()
                .verification_uri,
            "https://github.com/login/device"
        );
        let bad = br#"{"device_code":"dev","user_code":"USER-CODE","verification_uri":"file:///tmp/x","expires_in":900}"#;
        assert!(parse_device_authorization_json(bad).is_err());
    }

    #[test]
    fn parses_device_poll_states() {
        assert_eq!(
            parse_device_poll_json(br#"{"error":"authorization_pending"}"#),
            DevicePollTick::Pending
        );
        assert_eq!(
            parse_device_poll_json(br#"{"error":"slow_down","interval":9}"#),
            DevicePollTick::SlowDown { interval: Some(9) }
        );
        assert_eq!(
            parse_device_poll_json(br#"{"access_token":"gho_x"}"#),
            DevicePollTick::Complete {
                github_access_token: "gho_x".into()
            }
        );
        assert!(
            matches!(parse_device_poll_json(br#"{"error":"access_denied","error_description":"no"}"#), DevicePollTick::Failed { message } if message.contains("access_denied: no"))
        );
    }

    #[test]
    fn parses_safe_proxy_endpoint_base_url() {
        assert_eq!(
            base_url_from_copilot_token("tid=1;proxy-ep=proxy.individual.githubcopilot.com;exp=2")
                .as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
        assert!(base_url_from_copilot_token("proxy-ep=evil.com/path;exp=2").is_none());
        assert!(base_url_from_copilot_token("proxy-ep=evil.com:443;exp=2").is_none());
    }

    #[test]
    fn parses_token_response_and_applies_expiry_skew() {
        let token = parse_copilot_token_json(
            br#"{"token":"tid=1;proxy-ep=proxy.individual.githubcopilot.com;","expires_at":2000}"#,
        )
        .unwrap();
        assert_eq!(
            token.access,
            "tid=1;proxy-ep=proxy.individual.githubcopilot.com;"
        );
        assert_eq!(token.expires_at.timestamp(), 1700);
        assert_eq!(token.base_url, "https://api.individual.githubcopilot.com");
    }

    #[test]
    fn credentials_use_github_fields_not_oidc_issuer() {
        let token = CopilotToken {
            access: "tid=1;proxy-ep=proxy.individual.githubcopilot.com;".into(),
            expires_at: Utc::now(),
            base_url: "https://api.individual.githubcopilot.com".into(),
        };
        let auth = credentials_from_token(token, "gho_refresh".into(), Some("GitHub.COM".into()));
        assert_eq!(auth.auth_mode, AuthMode::GitHubCopilot);
        assert!(auth.oidc_issuer.is_none());
        assert_eq!(auth.github_domain.as_deref(), Some("github.com"));
        assert_eq!(
            auth.github_copilot_base_url.as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
        assert!(auth.github_copilot_available_models.is_none());
    }

    #[test]
    fn builds_safe_models_and_policy_urls() {
        assert_eq!(
            copilot_models_url("https://api.individual.githubcopilot.com/").unwrap(),
            "https://api.individual.githubcopilot.com/models"
        );
        assert_eq!(
            copilot_model_policy_url("https://api.individual.githubcopilot.com", "claude/odd")
                .unwrap(),
            "https://api.individual.githubcopilot.com/models/claude%2Fodd/policy"
        );
        assert!(copilot_models_url("http://api.individual.githubcopilot.com").is_err());
        assert!(copilot_models_url("https://user@api.individual.githubcopilot.com").is_err());
        assert!(
            copilot_model_policy_url("https://api.individual.githubcopilot.com", "bad\n").is_err()
        );
    }

    #[test]
    fn filters_available_models_like_copilot_picker() {
        let first = github_copilot_catalog_model_ids()
            .into_iter()
            .next()
            .expect("github-copilot catalog has models");
        let json = format!(
            r#"{{"data":[
                {{"id":"{first}","model_picker_enabled":true,"policy":{{"state":"enabled"}},"capabilities":{{"supports":{{"tool_calls":true}}}}}},
                {{"id":"disabled-policy","model_picker_enabled":true,"policy":{{"state":"disabled"}},"capabilities":{{"supports":{{"tool_calls":true}}}}}},
                {{"id":"hidden","model_picker_enabled":false,"policy":{{"state":"enabled"}},"capabilities":{{"supports":{{"tool_calls":true}}}}}},
                {{"id":"no-tools","model_picker_enabled":true,"policy":{{"state":"enabled"}},"capabilities":{{"supports":{{"tool_calls":false}}}}}},
                {{"id":"unknown-to-catalog","model_picker_enabled":true,"policy":{{"state":"enabled"}},"capabilities":{{"supports":{{"tool_calls":true}}}}}}
            ]}}"#
        );
        assert_eq!(
            parse_available_model_ids_json(json.as_bytes()).unwrap(),
            vec![first]
        );
    }
}
