use crate::agent::auth_method::ModelByok;
use crate::agent::model_providers::{
    ModelProviderConfig, auth_config_issues, model_provider_auth_name, parse_model_providers,
};
use crate::auth::{AuthManager, GrokComConfig, OidcAuthConfig};
use crate::remote::DEFAULT_CONTEXT_WINDOW;
use crate::{config::StorageMode, sampling::ApiBackend, tools::config::ShellToolsetConfig};
use agent_client_protocol as acp;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;
use xai_grok_agent::prompt::skills::SkillsConfig;
use xai_grok_sampler::{AuthScheme, SamplerConfig, SharedBearerResolver};
use xai_grok_sampling_types::{
    CompactionAtTokens, CompactionsRemaining, REASONING_EFFORT_META_KEY,
    REASONING_EFFORTS_META_KEY, ReasoningEffort, ReasoningEffortOption,
    reasoning_effort_meta_value, reasoning_efforts_meta_value,
};
use xai_grok_tools::types::compat::{
    COMPAT_CELLS, CompatConfig, CompatConfigToml, CompatRemoteKey, CompatSurface, CompatVendor,
};
/// The mode in which the agent is running.
/// Determines behavior like relay sync enablement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    /// TUI interactive mode - full UI with relay sync support
    Tui,
    /// Headless mode - no UI, connected to relay WebSocket
    Headless,
    /// Stdio mode - JSON-RPC over stdin/stdout
    Stdio,
    /// Server mode - WebSocket server for external clients
    Serve,
    /// Leader mode - IPC server for follower clients
    Leader,
    /// Generic/unknown mode
    #[default]
    Generic,
}
/// Default agent type when the server or user config doesn't specify one.
pub const DEFAULT_AGENT_TYPE: &str = "grok-build-plan";
/// Serde default for `ModelInfo.agent_type` and `ModelEntryConfig.agent_type`.
pub(crate) fn default_agent_type() -> String {
    DEFAULT_AGENT_TYPE.to_owned()
}
/// Default base URL for the cli chat proxy.
pub const CLI_CHAT_PROXY_BASE_URL_DEFAULT: &str = "https://cli-chat-proxy.grok.com/v1";
/// Default base URL for the public xAI API.
pub const XAI_API_BASE_URL_DEFAULT: &str = "https://api.x.ai/v1";
const NO_INLINE_CITATIONS_RESPONSE_INCLUDE: &str = "no_inline_citations";
/// One or more environment variable names that may hold a model API key.
///
/// Serde `untagged`: accepts a string or an array in TOML/JSON.
///
/// ```toml
/// env_key = "ANTHROPIC_AUTH_TOKEN"
/// # or
/// env_key = ["ANTHROPIC_AUTH_TOKEN", "LC_ANTHROPIC_AUTH_TOKEN"]
/// ```
///
/// At resolve time the **first set, non-blank** value wins (e.g. SSH
/// `AcceptEnv LC_*` forwarding of the Bottlerocket token).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EnvKeys {
    One(String),
    Many(Vec<String>),
}
impl EnvKeys {
    /// Single-name convenience constructor.
    pub fn single(name: impl Into<String>) -> Self {
        Self::One(name.into())
    }
    /// Construct from an ordered list (empty names dropped; 0/1/N → Many/One/Many).
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let names: Vec<String> = names
            .into_iter()
            .map(Into::into)
            .filter(|s| !s.is_empty())
            .collect();
        match names.as_slice() {
            [] => Self::Many(Vec::new()),
            [_] => Self::One(names.into_iter().next().expect("len 1")),
            _ => Self::Many(names),
        }
    }
    pub fn is_empty(&self) -> bool {
        match self {
            Self::One(s) => s.is_empty(),
            Self::Many(v) => v.is_empty(),
        }
    }
    /// Configured names in priority order.
    pub fn names(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
    /// First name only (useful for single-key assertions / display).
    pub fn primary(&self) -> Option<&str> {
        match self {
            Self::One(s) if !s.is_empty() => Some(s.as_str()),
            Self::One(_) => None,
            Self::Many(v) => v.iter().map(String::as_str).find(|s| !s.is_empty()),
        }
    }
    /// Resolve the first set, non-blank process env value among configured names.
    pub(crate) fn resolve_value(&self) -> Option<String> {
        self.resolve_value_with(|name| std::env::var(name).ok())
    }
    /// Testable resolve with an injected getenv.
    pub(crate) fn resolve_value_with(
        &self,
        mut getenv: impl FnMut(&str) -> Option<String>,
    ) -> Option<String> {
        self.resolve_value_with_source(getenv).map(|(v, _)| v)
    }
    /// Like [`Self::resolve_value_with`], but also returns the winning env var name.
    ///
    /// Callers that need per-source auth schemes (e.g. `ANTHROPIC_AUTH_TOKEN`
    /// is Bearer while `ANTHROPIC_API_KEY` is `x-api-key`) use the source name.
    pub fn resolve_value_with_source(
        &self,
        mut getenv: impl FnMut(&str) -> Option<String>,
    ) -> Option<(String, String)> {
        for name in self.names() {
            if let Some(value) = getenv(name)
                && !value.trim().is_empty()
            {
                return Some((value, name.to_owned()));
            }
        }
        None
    }
}
/// Semantic equality: compares the ordered name lists, so `One("X")` and
/// `Many(["X"])` (the shape serde produces for `["X"]`) compare equal.
impl PartialEq for EnvKeys {
    fn eq(&self, other: &Self) -> bool {
        self.names() == other.names()
    }
}
impl Eq for EnvKeys {}
impl std::fmt::Display for EnvKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.names().join(", "))
    }
}
/// Configuration for API endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointsConfig {
    /// cli chat proxy base URL. `None` = unset (resolvers apply the default);
    /// `Some` = explicitly configured. Tracking explicitness (vs comparing to the
    /// default value) lets an org pin the proxy to the default on purpose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_chat_proxy_base_url: Option<String>,
    /// Base URL for the public xAI API.
    pub xai_api_base_url: String,
    /// Optional extra access-header value (applied only with the optional
    /// non-production feature, and only for matching first-party hosts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alpha_test_key: Option<String>,
    /// Env: `GROK_MODELS_BASE_URL`. Enables custom endpoint mode.
    /// List URL defaults to `{models_base_url}/models`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_base_url: Option<String>,
    /// Env: `GROK_MODELS_LIST_URL`. Overrides the default `{base}/models` list URL.
    #[serde(alias = "models_endpoint", skip_serializing_if = "Option::is_none")]
    pub models_list_url: Option<String>,
    /// Env: `GROK_FEEDBACK_BASE_URL`. Where feedback submissions go.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_base_url: Option<String>,
    /// Env: `GROK_TRACE_UPLOAD_URL`. Where trace uploads go.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_upload_url: Option<String>,
    /// Env: `GROK_TRACE_UPLOAD_BUCKET`. Direct bucket (`gs://` or `s3://`), bypasses proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_upload_bucket: Option<String>,
    /// Env: `GROK_TRACE_UPLOAD_REGION`. AWS region (S3 only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_upload_region: Option<String>,
    /// Env: `GROK_TRACE_UPLOAD_CREDENTIALS_FILE`. Path to GCS SA key or AWS credentials file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_upload_credentials_file: Option<String>,
    /// Inline credentials (JSON/INI). Takes precedence over `credentials_file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_upload_credentials: Option<String>,
    /// Env: `GROK_TRACE_UPLOAD_ENDPOINT_URL`. Custom S3-compatible endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_upload_endpoint_url: Option<String>,
    /// Env: `GROK_DEPLOYMENT_KEY`. Management API key for enterprise deployments.
    /// Sent on telemetry and service requests for deployment-level attribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_key: Option<String>,
    /// Env: `GROK_MANAGED_CONFIG_URL`. Override the managed config endpoint.
    /// Defaults to `{proxy_url()}/deployment/config`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_config_url: Option<String>,
    /// Env: `OTEL_EXPORTER_OTLP_ENDPOINT`. OTLP collector base; `/v1/traces` is
    /// appended. Legacy repoint of the INTERNAL trace pipeline — deprecated in
    /// favor of `GROK_INTERNAL_OTLP_TRACES_ENDPOINT`, and ignored by the internal
    /// pipeline when `GROK_EXTERNAL_OTEL` is set (the standard `OTEL_*` vars then
    /// route the external stream only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otel_exporter_otlp_endpoint: Option<String>,
    /// Env: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`. Full traces endpoint, used
    /// verbatim; overrides `otel_exporter_otlp_endpoint`. Same legacy/deprecation
    /// semantics as `otel_exporter_otlp_endpoint`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otel_exporter_otlp_traces_endpoint: Option<String>,
    /// Env: `OTEL_EXPORTER_OTLP_HEADERS`. `k=v,k2=v2`; merged onto export headers.
    /// Same legacy/deprecation semantics as `otel_exporter_otlp_endpoint`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otel_exporter_otlp_headers: Option<String>,
    /// Env: `GROK_INTERNAL_OTLP_TRACES_ENDPOINT`. Full INTERNAL traces endpoint,
    /// used verbatim. Dev/debug repoint of the internal span firehose (replaces
    /// the legacy `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` behavior; used by
    /// local-ic-testing / internal dev flows). Wins over the legacy `OTEL_*` vars.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grok_internal_otlp_traces_endpoint: Option<String>,
    /// Env: `GROK_INTERNAL_OTLP_HEADERS`. `k=v,k2=v2` extra headers for the
    /// internal export (debug). Wins over the legacy `OTEL_EXPORTER_OTLP_HEADERS`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grok_internal_otlp_headers: Option<String>,
    /// External-OTEL master switch, captured at construction via
    /// [`external_otel_master_switch_resolved`] — the same layered resolution
    /// (requirement pin > `GROK_EXTERNAL_OTEL` env > `[telemetry].otel_enabled`
    /// config, managed layers included) that activates the external stream.
    /// When set, the standard `OTEL_EXPORTER_OTLP_*` vars are reserved for the
    /// external OTEL stream and the internal trace pipeline ignores them
    /// entirely — an admin who opts in (by *any* layer, including an org
    /// enable distributed via managed config with no env var) never receives
    /// the internally-authed firehose. Held as a field (not re-read in the
    /// resolvers) so the resolvers stay pure and testable without env races.
    #[serde(skip)]
    pub external_otel_master_switch: bool,
    /// Env: `OTEL_TRACES_EXPORTER`. `otlp` (default) or `none` to disable spans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otel_traces_exporter: Option<String>,
    /// Env: `OTEL_BSP_SCHEDULE_DELAY` (OTel) or `OTEL_TRACES_EXPORT_INTERVAL`
    /// (Claude alias). Batch flush interval (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otel_traces_export_interval: Option<u64>,
    /// Env: `OTEL_EXPORTER_OTLP_TIMEOUT`. Export HTTP timeout (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otel_exporter_otlp_timeout: Option<u64>,
    /// Read by `load_management_api_key_sync()`. Declared for `serde_ignored`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub management_api_key: Option<String>,
    /// Read by `load_gcs_service_account_key_sync()`. Declared for `serde_ignored`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gcs_service_account_key: Option<String>,
}
/// A blank or whitespace-only override counts as unset. Single source of truth
/// for the "empty value = not configured" rule shared by the endpoint resolvers.
fn blank_as_unset(opt: &Option<String>) -> Option<String> {
    opt.as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}
/// Parse a `k=v,k2=v2` OTLP header list (the `OTEL_EXPORTER_OTLP_HEADERS`
/// format, shared with `GROK_INTERNAL_OTLP_HEADERS`): split on `,`,
/// `split_once('=')`, trim key/value, skip blank keys, keep empty values.
fn parse_otlp_header_list(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}
impl EndpointsConfig {
    pub fn has_custom_endpoint(&self) -> bool {
        self.models_base_url.is_some() || self.models_list_url.is_some()
    }
    /// `default()` plus merged managed/requirements endpoint overrides, so
    /// startup fetches use the configured (not public) endpoints. Only merges
    /// layers — never derives one endpoint from another. Falls back to
    /// `default()` on load failure.
    pub(crate) fn from_effective_config() -> Self {
        match crate::config::load_effective_config() {
            Ok(cfg) => Self::from_config_value(&cfg),
            Err(_) => Self::default(),
        }
    }
    /// Layer the `[endpoints]` table from `config` over the env/default base.
    /// No field is derived from another — defaulting is done by the resolvers.
    /// `pub`: the pager resolves the voice STT base through this same path.
    pub fn from_config_value(config: &toml::Value) -> Self {
        let default = Self::default();
        let external_otel_master_switch = default.external_otel_master_switch;
        let mut base = match toml::Value::try_from(default) {
            Ok(v) => v,
            Err(_) => return Self::default(),
        };
        if let Some(endpoints) = config.get("endpoints") {
            crate::config::deep_merge_toml(&mut base, endpoints);
        }
        let mut resolved: Self = base.try_into().unwrap_or_default();
        resolved.external_otel_master_switch = external_otel_master_switch;
        resolved
    }
    /// The cli-chat-proxy base URL through which all auxiliary services (and
    /// OAuth/session inference) resolve: explicit `cli_chat_proxy_base_url`, else
    /// the public default. NEVER falls back to `xai_api_base_url` — that is the
    /// inference endpoint (API-key auth) only.
    pub fn proxy_url(&self) -> String {
        blank_as_unset(&self.cli_chat_proxy_base_url)
            .unwrap_or_else(|| CLI_CHAT_PROXY_BASE_URL_DEFAULT.to_owned())
    }
    pub(crate) fn resolve_inference_base_url(&self) -> String {
        self.models_base_url
            .clone()
            .unwrap_or_else(|| self.proxy_url())
    }
    /// Feedback endpoint — an auxiliary service, so it defaults to the
    /// cli-chat-proxy, never `xai_api_base_url`.
    pub(crate) fn resolve_feedback_base_url(&self) -> String {
        blank_as_unset(&self.feedback_base_url).unwrap_or_else(|| self.proxy_url())
    }
    /// Trace upload endpoint — an auxiliary service, so it defaults to the
    /// cli-chat-proxy, never `xai_api_base_url`.
    pub(crate) fn resolve_trace_upload_url(&self) -> String {
        blank_as_unset(&self.trace_upload_url).unwrap_or_else(|| self.proxy_url())
    }
    /// Managed deployment-config URL (`grok setup`): explicit `managed_config_url`,
    /// else `proxy_url` + `/deployment/config`. Never `xai_api_base_url`, so the
    /// deployment key reaches the proxy, not the inference host.
    pub(crate) fn resolve_managed_config_url(&self) -> String {
        blank_as_unset(&self.managed_config_url).unwrap_or_else(|| {
            format!(
                "{}/deployment/config",
                self.proxy_url().trim_end_matches('/')
            )
        })
    }
    /// INTERNAL OTLP traces endpoint. Precedence:
    /// 1. `grok_internal_otlp_traces_endpoint` (verbatim)
    /// 2. legacy `otel_exporter_otlp_traces_endpoint` (verbatim) >
    ///    `otel_exporter_otlp_endpoint` + `/v1/traces` — ONLY when the
    ///    external-OTEL master switch is unset (back-compat; deprecated)
    /// 3. `proxy_url` + `/traces`.
    /// Uses the proxy default (not the `xai_api_base_url` fallback) so
    /// telemetry reports to xAI even when inference is overridden. When the
    /// master switch IS set, the standard `OTEL_EXPORTER_OTLP_*` values are
    /// completely ignored here so the internally-authed firehose never lands
    /// at an external collector.
    pub(crate) fn resolve_otlp_traces_endpoint(&self) -> String {
        if let Some(full) = blank_as_unset(&self.grok_internal_otlp_traces_endpoint) {
            return full.trim_end_matches('/').to_string();
        }
        if !self.external_otel_master_switch
            && let Some(legacy) = self.legacy_internal_otlp_traces_endpoint()
        {
            tracing::warn!(
                "Repointing the internal trace pipeline via OTEL_EXPORTER_OTLP_ENDPOINT / \
                 OTEL_EXPORTER_OTLP_TRACES_ENDPOINT is deprecated; use \
                 GROK_INTERNAL_OTLP_TRACES_ENDPOINT instead — the standard OTEL_* vars will \
                 route the external OTEL stream only in a future release"
            );
            return legacy;
        }
        format!("{}/traces", self.proxy_url().trim_end_matches('/'))
    }
    /// Legacy (standard-OTEL-var) internal traces endpoint, if any:
    /// `otel_exporter_otlp_traces_endpoint` verbatim, else
    /// `otel_exporter_otlp_endpoint` + `/v1/traces`. Ignores the master switch.
    fn legacy_internal_otlp_traces_endpoint(&self) -> Option<String> {
        if let Some(full) = blank_as_unset(&self.otel_exporter_otlp_traces_endpoint) {
            return Some(full.trim_end_matches('/').to_string());
        }
        blank_as_unset(&self.otel_exporter_otlp_endpoint)
            .map(|base| format!("{}/v1/traces", base.trim_end_matches('/')))
    }
    /// Extra headers for the INTERNAL export: `grok_internal_otlp_headers`
    /// first; legacy fallback to `otel_exporter_otlp_headers` ONLY when the
    /// external-OTEL master switch is unset (back-compat for existing users).
    pub(crate) fn resolve_otlp_headers(&self) -> Vec<(String, String)> {
        if let Some(headers) = blank_as_unset(&self.grok_internal_otlp_headers) {
            return parse_otlp_header_list(&headers);
        }
        if !self.external_otel_master_switch {
            return parse_otlp_header_list(
                self.otel_exporter_otlp_headers.as_deref().unwrap_or(""),
            );
        }
        Vec::new()
    }
    /// Whether the legacy fallback actually supplied the internal endpoint OR
    /// internal headers from the standard `OTEL_EXPORTER_OTLP_*` vars — i.e.
    /// the master switch is unset AND (`otel_exporter_otlp_traces_endpoint` /
    /// `otel_exporter_otlp_endpoint` is non-blank for the endpoint, or
    /// `otel_exporter_otlp_headers` is non-blank for headers) AND no
    /// `grok_internal_otlp_*` override shadowed that half.
    ///
    /// CONTRACT: this flag is passed to the external OTEL stream's init, which
    /// MUST refuse to activate when it is true — the same standard vars cannot
    /// feed both pipelines (no-double-send invariant, enforced in code).
    pub(crate) fn internal_otlp_consumed_standard_vars(&self) -> bool {
        if self.external_otel_master_switch {
            return false;
        }
        let endpoint_consumed = blank_as_unset(&self.grok_internal_otlp_traces_endpoint).is_none()
            && self.legacy_internal_otlp_traces_endpoint().is_some();
        let headers_consumed = blank_as_unset(&self.grok_internal_otlp_headers).is_none()
            && blank_as_unset(&self.otel_exporter_otlp_headers).is_some();
        endpoint_consumed || headers_consumed
    }
    /// Trace export enabled unless `OTEL_TRACES_EXPORTER=none`. Deliberately
    /// still honored by the internal pipeline even with `GROK_EXTERNAL_OTEL`
    /// set: disabling internal span export is the safe direction.
    pub(crate) fn resolve_traces_export_enabled(&self) -> bool {
        !matches!(
            self.otel_traces_exporter.as_deref().map(str::trim),
            Some("none")
        )
    }
    /// `OTEL_BSP_SCHEDULE_DELAY` / `OTEL_TRACES_EXPORT_INTERVAL` — tuning-only,
    /// deliberately shared between the internal and external pipelines.
    pub(crate) fn resolve_otlp_export_interval(&self) -> Option<std::time::Duration> {
        self.otel_traces_export_interval
            .map(std::time::Duration::from_millis)
    }
    /// `OTEL_EXPORTER_OTLP_TIMEOUT` — tuning-only, deliberately shared between
    /// the internal and external pipelines.
    pub(crate) fn resolve_otlp_timeout(&self) -> Option<std::time::Duration> {
        self.otel_exporter_otlp_timeout
            .map(std::time::Duration::from_millis)
    }
    /// Resolve trace upload credentials: inline > file > `None` (ambient).
    pub(crate) fn resolve_trace_credentials(&self) -> Option<String> {
        if let Some(ref inline) = self.trace_upload_credentials {
            let trimmed = inline.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        self.trace_upload_credentials_file
            .as_deref()
            .and_then(|path| {
                std::fs::read_to_string(path)
                    .inspect_err(|e| {
                        tracing::warn!(
                            path = %path,
                            error = %e,
                            "Failed to read trace upload credentials file"
                        );
                    })
                    .ok()
            })
    }
    /// Resolve direct-to-bucket upload method from `trace_upload_bucket`.
    /// Returns `None` if no bucket is configured or scheme is unrecognized.
    pub fn resolve_direct_upload_method(
        &self,
    ) -> Option<crate::session::repo_changes::UploadMethod> {
        let bucket_url = self.trace_upload_bucket.as_deref()?.trim();
        if bucket_url.is_empty() {
            return None;
        }
        if let Some(bucket_name) = bucket_url
            .strip_prefix("s3://")
            .map(|s| s.trim_end_matches('/'))
        {
            let region = self
                .trace_upload_region
                .clone()
                .unwrap_or_else(|| "us-east-1".to_owned());
            return Some(crate::session::repo_changes::UploadMethod::S3 {
                bucket: bucket_name.to_owned(),
                region,
                credentials_file: None,
                credentials_content: self.resolve_trace_credentials(),
                endpoint_url: self.trace_upload_endpoint_url.clone(),
            });
        }
        if bucket_url.starts_with("gs://") {
            return Some(crate::session::repo_changes::UploadMethod::Direct {
                service_account_key: self.resolve_trace_credentials(),
            });
        }
        tracing::warn!(
            bucket = %bucket_url,
            "trace_upload_bucket has unrecognized scheme (expected gs:// or s3://), ignoring"
        );
        None
    }
    /// Whether trace upload can authenticate without an interactive login.
    pub fn has_noninteractive_upload_auth(&self) -> bool {
        self.deployment_key.is_some() || self.resolve_direct_upload_method().is_some()
    }
    /// Direct bucket → proxy (if `auth_token` or `deployment_key`) → ambient GCS → `None`.
    pub fn resolve_upload_method(
        &self,
        auth_token: Option<String>,
    ) -> Option<crate::session::repo_changes::UploadMethod> {
        if let Some(method) = self.resolve_direct_upload_method() {
            return Some(method);
        }
        if auth_token.is_some() || self.deployment_key.is_some() {
            return Some(crate::session::repo_changes::UploadMethod::Proxy {
                proxy_base_url: self.resolve_trace_upload_url(),
                user_token: auth_token.unwrap_or_default(),
                deployment_key: self.deployment_key.clone(),
                alpha_test_key: self.alpha_test_key.clone(),
            });
        }
        let service_account_key = crate::util::config::load_gcs_service_account_key_sync();
        if service_account_key.is_some() {
            return Some(crate::session::repo_changes::UploadMethod::Direct {
                service_account_key,
            });
        }
        None
    }
    /// Resolve trace bucket URL: env > config > compiled-in default.
    /// `None` disables direct GCS trace uploads.
    pub fn resolve_trace_bucket_url(&self) -> Option<Resolved<String>> {
        resolve_string_flag(
            None,
            "GROK_TELEMETRY_GCS_BUCKET",
            self.trace_upload_bucket.as_deref(),
            None,
        )
        .or_else(|| {
            crate::upload::gcs::SESSION_TRACES_BUCKET
                .map(|b| Resolved::new(format!("gs://{b}"), ConfigSource::Default))
        })
    }
    /// `models_list_url` > `{models_base_url}/models` > `{proxy_base_url}/models`.
    pub(crate) fn resolve_models_list_url(&self) -> String {
        if let Some(ref url) = self.models_list_url {
            return url.clone();
        }
        let base = self
            .models_base_url
            .clone()
            .unwrap_or_else(|| self.proxy_url());
        format!("{}/models", base)
    }
}
impl Default for EndpointsConfig {
    fn default() -> Self {
        Self {
            cli_chat_proxy_base_url: std::env::var("GROK_CLI_CHAT_PROXY_BASE_URL").ok(),
            xai_api_base_url: std::env::var("GROK_XAI_API_BASE_URL")
                .unwrap_or_else(|_| XAI_API_BASE_URL_DEFAULT.to_owned()),
            alpha_test_key: None,
            models_base_url: env_string("GROK_MODELS_BASE_URL"),
            models_list_url: env_string("GROK_MODELS_LIST_URL"),
            feedback_base_url: env_string("GROK_FEEDBACK_BASE_URL"),
            trace_upload_url: env_string("GROK_TRACE_UPLOAD_URL"),
            trace_upload_bucket: env_string("GROK_TRACE_UPLOAD_BUCKET"),
            trace_upload_region: env_string("GROK_TRACE_UPLOAD_REGION"),
            trace_upload_credentials_file: env_string("GROK_TRACE_UPLOAD_CREDENTIALS_FILE"),
            trace_upload_credentials: None,
            trace_upload_endpoint_url: env_string("GROK_TRACE_UPLOAD_ENDPOINT_URL"),
            deployment_key: env_string("GROK_DEPLOYMENT_KEY"),
            managed_config_url: env_string("GROK_MANAGED_CONFIG_URL"),
            otel_exporter_otlp_endpoint: env_string("OTEL_EXPORTER_OTLP_ENDPOINT"),
            otel_exporter_otlp_traces_endpoint: env_string("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"),
            otel_exporter_otlp_headers: env_string("OTEL_EXPORTER_OTLP_HEADERS"),
            grok_internal_otlp_traces_endpoint: env_string("GROK_INTERNAL_OTLP_TRACES_ENDPOINT"),
            grok_internal_otlp_headers: env_string("GROK_INTERNAL_OTLP_HEADERS"),
            external_otel_master_switch: external_otel_master_switch_resolved(),
            otel_traces_exporter: env_string("OTEL_TRACES_EXPORTER"),
            otel_traces_export_interval: env_string("OTEL_BSP_SCHEDULE_DELAY")
                .or_else(|| env_string("OTEL_TRACES_EXPORT_INTERVAL"))
                .and_then(|s| s.parse().ok()),
            otel_exporter_otlp_timeout: env_string("OTEL_EXPORTER_OTLP_TIMEOUT")
                .and_then(|s| s.parse().ok()),
            management_api_key: None,
            gcs_service_account_key: None,
        }
    }
}
pub use xai_grok_config_types::{BoolFlag, ConfigSource, LazinessDetectorPerModelConfig, Resolved};
/// Resolution result for a `/goal` role's model selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum GoalRoleModelChoice {
    /// Use the current (parent) model + the parent's agent type.
    #[default]
    InheritCurrent,
    /// Use this explicit pair (subject to auth/fail-open at spawn time).
    Explicit(crate::util::config::GoalRoleModel),
}
/// A requirement pin from `requirements.toml`. Wins over all other sources.
#[derive(Debug, Clone, Default)]
pub struct Constrained<T> {
    pin: Option<T>,
    source: Option<crate::config::RequirementSource>,
}
impl<T: Clone> Constrained<T> {
    pub fn pin(&mut self, value: T, source: crate::config::RequirementSource) {
        self.pin = Some(value);
        self.source = Some(source);
    }
    pub fn pinned(&self) -> Option<T> {
        self.pin.clone()
    }
    pub fn source(&self) -> Option<&crate::config::RequirementSource> {
        self.source.as_ref()
    }
}
/// Enforced requirements from `requirements.toml`. Pinned values win over all other sources.
#[derive(Debug, Clone, Default)]
pub struct Requirements {
    pub telemetry: Constrained<TelemetryMode>,
    pub trace_upload: Constrained<bool>,
    pub feedback: Constrained<bool>,
    pub lsp_tools: Constrained<bool>,
    pub web_fetch: Constrained<bool>,
    pub ask_user_question: Constrained<bool>,
    pub image_gen: Constrained<bool>,
    pub image_edit: Constrained<bool>,
    pub video_gen: Constrained<bool>,
    pub write_file: Constrained<bool>,
    /// Voice dictation (STT). Pin via requirements/managed `[features] voice_mode`.
    pub voice_mode: Constrained<bool>,
    /// The session search index. Pin via requirements/managed `[features] session_search`.
    pub session_search: Constrained<bool>,
    pub sandbox_auto_allow_bash: Constrained<bool>,
    pub sandbox_profile: Constrained<String>,
    pub respect_gitignore: Constrained<bool>,
    pub remote_fetch: Constrained<bool>,
}
/// Inputs for resolving `#[serde(skip)]` runtime fields after `new_from_toml_cfg()`.
///
/// Constructed by each binary from its CLI args and startup state, then passed
/// to [`Config::resolve_runtime_fields`].
pub struct RuntimeResolutionContext<'a> {
    pub raw_config: &'a toml::Value,
    pub remote_settings: Option<&'a crate::util::config::RemoteSettings>,
    pub is_headless: bool,
    /// `Some(true)` = CLI explicitly enabled, `None` = defer to config/env/remote.
    pub cli_subagents: Option<bool>,
    pub cli_web_search_model: Option<&'a str>,
    pub cli_session_summary_model: Option<&'a str>,
    /// CLI `--experimental-memory` flag. Enables cross-session memory.
    pub cli_experimental_memory: bool,
    /// CLI `--no-memory` flag. Overrides all other memory settings.
    pub cli_no_memory: bool,
    /// CLI `--disable-web-search` flag. ORed with config.toml value.
    pub disable_web_search: bool,
    /// CLI `--todo-gate` flag. Session-scoped — not persisted.
    pub todo_gate: bool,
    /// CLI `--laziness-debug-log <path>`. When `Some`, the Layer-3
    /// classifier fires after every turn (bypassing the idle wait /
    /// per-model gate / nudge cap) and writes a JSONL line per fire.
    /// Observation-only. Session-scoped — not persisted.
    pub laziness_debug_log: Option<&'a std::path::Path>,
    /// CLI `--storage-mode` override. `None` = defer to env/remote/default.
    pub storage_mode: Option<&'a str>,
}
/// First-party credential env vars scrubbed from a BYOK auth-provider helper's
/// environment so it can't inherit the keys Grok uses for its own first-party
/// requests. Keep in sync with every first-party credential env read across the
/// crate: `auth::manager` (`GROK_AUTH`/`GROK_AUTH_PATH`), `auth_method`
/// (`XAI_API_KEY`/legacy), and the credential-bearing `env_string(...)` reads in
/// `EndpointsConfig::default`. The `provider_helper_env_scrubs_first_party_credentials`
/// test pins this against an independent audited literal, so any change here must
/// be mirrored (and re-audited) there.
pub(crate) const FIRST_PARTY_CREDENTIAL_ENV_VARS: &[&str] = &[
    crate::agent::auth_method::XAI_API_KEY_ENV_VAR,
    crate::agent::auth_method::LEGACY_XAI_API_KEY_ENV_VAR,
    "GROK_AUTH",
    "GROK_AUTH_PATH",
    "GROK_DEPLOYMENT_KEY",
    "GROK_EXTRA_AUTH_KEY",
    "GROK_TRACE_UPLOAD_CREDENTIALS_FILE",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "GROK_INTERNAL_OTLP_HEADERS",
];
/// Read an env var as a trimmed string. Returns `None` if unset or empty/whitespace-only.
pub(crate) fn env_string(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
pub use xai_grok_config::env_bool;
/// Compaction-mode precedence (env > config > remote settings > default, with
/// unrecognized values at each source falling through). `remote` sits just
/// above the default, mirroring `feature_flag` in `resolve_bool_flag`. Pure so
/// it's unit-testable without mutating process env.
fn resolve_compaction_mode_from(
    env: Option<&str>,
    config: Option<&str>,
    remote: Option<&str>,
) -> xai_chat_state::CompactionMode {
    use xai_chat_state::CompactionMode;
    env.and_then(CompactionMode::parse)
        .or_else(|| config.and_then(CompactionMode::parse))
        .or_else(|| remote.and_then(CompactionMode::parse))
        .unwrap_or_default()
}
/// Compaction-detail precedence (env > config > remote settings > default). Pure.
/// Controls the per-turn verbatim detail in `segments` mode (default `verbose`).
fn resolve_compaction_detail_from(
    env: Option<&str>,
    config: Option<&str>,
    remote: Option<&str>,
) -> xai_chat_state::CompactionDetail {
    use xai_chat_state::CompactionDetail;
    env.and_then(CompactionDetail::parse)
        .or_else(|| config.and_then(CompactionDetail::parse))
        .or_else(|| remote.and_then(CompactionDetail::parse))
        .unwrap_or_default()
}
/// Resolve a single vendor-compat cell: env > `[compat]` TOML > remote settings
/// remote flag > default ON.
fn resolve_compat_cell(
    env: &str,
    cfg: Option<bool>,
    remote: Option<bool>,
    default: bool,
) -> Resolved<bool> {
    resolve_compat_cell_with_env(xai_grok_config::env_bool(env), cfg, remote, default)
}
pub(crate) fn resolve_compat_cell_with_env(
    env: Option<bool>,
    cfg: Option<bool>,
    remote: Option<bool>,
    default: bool,
) -> Resolved<bool> {
    if let Some(value) = env {
        Resolved::new(value, ConfigSource::Env)
    } else if let Some(value) = cfg {
        Resolved::new(value, ConfigSource::Config)
    } else if let Some(value) = remote {
        Resolved::new(value, ConfigSource::Remote)
    } else {
        Resolved::new(default, ConfigSource::Default)
    }
}
fn remote_compat_value(
    remote: Option<&crate::util::config::RemoteSettings>,
    key: Option<CompatRemoteKey>,
) -> Option<bool> {
    let remote = remote?;
    match key? {
        CompatRemoteKey::CursorSkills => remote.cursor_skills_enabled,
        CompatRemoteKey::CursorRules => remote.cursor_rules_enabled,
        CompatRemoteKey::CursorAgents => remote.cursor_agents_enabled,
        CompatRemoteKey::CursorMcps => remote.cursor_mcps_enabled,
        CompatRemoteKey::CursorHooks => remote.cursor_hooks_enabled,
        CompatRemoteKey::CursorSessions => remote.cursor_sessions_enabled,
        CompatRemoteKey::ClaudeSkills => remote.claude_skills_enabled,
        CompatRemoteKey::ClaudeRules => remote.claude_rules_enabled,
        CompatRemoteKey::ClaudeAgents => remote.claude_agents_enabled,
        CompatRemoteKey::ClaudeMcps => remote.claude_mcps_enabled,
        CompatRemoteKey::ClaudeHooks => remote.claude_hooks_enabled,
        CompatRemoteKey::ClaudeSessions => remote.claude_sessions_enabled,
        CompatRemoteKey::CodexSessions => remote.codex_sessions_enabled,
    }
}
/// Resolve vendor compatibility cells from TOML and remote settings.
fn resolve_compat_config(
    config: &CompatConfigToml,
    remote: Option<&crate::util::config::RemoteSettings>,
) -> CompatConfig {
    let defaults = CompatConfig::default();
    let mut resolved = defaults;
    for cell in COMPAT_CELLS {
        resolved.set(
            cell,
            resolve_compat_cell(
                cell.env_var(),
                config.value(cell),
                remote_compat_value(remote, cell.remote_key()),
                defaults.value(cell),
            )
            .value,
        );
    }
    resolved
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompatConfigCellError {
    Unavailable,
    Malformed,
}
pub(crate) fn compat_config_cell(
    raw_config: Result<&toml::Value, ()>,
    cell: xai_grok_tools::types::compat::CompatCell,
) -> Result<Option<bool>, CompatConfigCellError> {
    let raw = raw_config.map_err(|()| CompatConfigCellError::Unavailable)?;
    let Some(compat) = raw.get("compat") else {
        return Ok(None);
    };
    let compat = compat.as_table().ok_or(CompatConfigCellError::Malformed)?;
    let Some(vendor) = compat.get(cell.vendor().as_str()) else {
        return Ok(None);
    };
    let vendor = vendor.as_table().ok_or(CompatConfigCellError::Malformed)?;
    let Some(value) = vendor.get(cell.surface().as_str()) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or(CompatConfigCellError::Malformed)
}
/// Resolve only picker-facing session cells from raw config independently.
pub fn resolve_compat_sessions_from_raw(
    raw_config: Result<&toml::Value, ()>,
    remote: Option<&crate::util::config::RemoteSettings>,
) -> CompatConfig {
    let mut config = CompatConfigToml::default();
    for cell in COMPAT_CELLS
        .into_iter()
        .filter(|cell| cell.surface() == CompatSurface::Sessions)
    {
        let value = match compat_config_cell(raw_config, cell) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    vendor = cell.vendor().as_str(),
                    ?error,
                    "invalid compat config; disabling foreign sessions"
                );
                Some(false)
            }
        };
        match cell.vendor() {
            CompatVendor::Cursor => config.cursor.sessions = value,
            CompatVendor::Claude => config.claude.sessions = value,
            CompatVendor::Codex => config.codex.sessions = value,
            CompatVendor::Omp => config.omp.sessions = value,
        }
    }
    resolve_compat_config(&config, remote)
}
/// Resolve a string setting: cli > env > config > feature flag. `None` if no source provides a value.
pub(crate) fn resolve_string_flag(
    cli_arg: Option<&str>,
    env_var: &str,
    config_val: Option<&str>,
    feature_flag_val: Option<&str>,
) -> Option<Resolved<String>> {
    if let Some(val) = cli_arg.filter(|s| !s.is_empty()) {
        return Some(Resolved::new(val.to_owned(), ConfigSource::Cli));
    }
    if let Some(val) = env_string(env_var) {
        return Some(Resolved::new(val, ConfigSource::Env));
    }
    if let Some(val) = config_val.filter(|s| !s.is_empty()) {
        return Some(Resolved::new(val.to_owned(), ConfigSource::Config));
    }
    if let Some(val) = feature_flag_val.filter(|s| !s.is_empty()) {
        return Some(Resolved::new(val.to_owned(), ConfigSource::Remote));
    }
    None
}
/// Resolve `enabled` for section-based configs (memory, subagents, etc.).
/// Feature flag only applies when the TOML section is absent.
pub(crate) fn resolve_enabled(
    cli_flag: Option<bool>,
    env_var: &str,
    config_enabled: bool,
    has_local_section: bool,
    feature_flag_val: Option<bool>,
    default: bool,
) -> Resolved<bool> {
    let config_val = if has_local_section {
        Some(config_enabled)
    } else {
        None
    };
    BoolFlag::env(env_var)
        .cli(cli_flag)
        .config(config_val)
        .feature_flag(feature_flag_val)
        .default(default)
        .resolve()
}
pub(crate) use xai_grok_telemetry::config::env_telemetry_mode;
pub use xai_grok_telemetry::config::{TelemetryConfig, TelemetryMode};
/// Plugin system configuration from `[plugins]` section in config.toml.
///
/// ```toml
/// [plugins]
/// paths = ["~/my-plugins/custom-tools"]
/// disabled = ["user/a1b2c3d4/noisy-plugin"]
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PluginsConfig {
    /// Additional plugin directory paths to load.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Plugin IDs or names to disable. Disabled plugins are discovered
    /// but their components are not loaded into the session.
    #[serde(default)]
    pub disabled: Vec<String>,
    /// Plugin IDs or names to explicitly enable. Used for project-scope plugins
    /// which are disabled by default — adding a plugin here overrides that default.
    #[serde(default)]
    pub enabled: Vec<String>,
    /// CLI `--plugin-dir` paths (populated by CLI arg processing, not config file).
    #[serde(skip)]
    pub cli_plugin_dirs: Vec<std::path::PathBuf>,
}
impl PluginsConfig {
    /// Merge `enabledPlugins` from Claude settings files into this config.
    ///
    /// Reads `enabledPlugins` from `~/.claude/settings.json` only (user scope).
    /// Project-level `<git_root>/.claude/settings.json` is intentionally NOT
    /// read here: a malicious repo could pre-populate `enabledPlugins` to
    /// bypass the project-plugin auto-disable logic in `populate_plugin_lists`,
    /// enabling attacker-controlled hooks (e.g. SessionStart → RCE).
    /// Native `.grok/config.toml` entries already present take precedence:
    /// a name is only added if it isn't already in the opposite list.
    pub(crate) fn merge_claude_enabled_plugins(&mut self, _cwd: Option<&std::path::Path>) {
        if crate::claude_import::is_claude_import_marked_with_log("merge_claude_enabled_plugins") {
            return;
        }
        let mut paths = Vec::new();
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".claude").join("settings.json"));
        }
        for path in &paths {
            let (claude_enabled, claude_disabled) =
                xai_grok_agent::plugins::marketplace::load_enabled_disabled_plugins(path);
            for name in claude_enabled {
                if !self.disabled.contains(&name) && !self.enabled.contains(&name) {
                    self.enabled.push(name);
                }
            }
            for name in claude_disabled {
                if !self.enabled.contains(&name) && !self.disabled.contains(&name) {
                    self.disabled.push(name);
                }
            }
        }
    }
    /// Build a `DiscoveryConfig` from this plugins config.
    pub(crate) fn to_discovery_config(
        &self,
    ) -> xai_grok_agent::plugins::discovery::DiscoveryConfig {
        xai_grok_agent::plugins::discovery::DiscoveryConfig {
            cli_plugin_dirs: self.cli_plugin_dirs.clone(),
            config_paths: self.paths.iter().map(std::path::PathBuf::from).collect(),
            disabled: self.disabled.clone(),
            enabled: self.enabled.clone(),
        }
    }
}
/// Feedback submission configuration (`[feedback]` in config.toml).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedbackConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<FeedbackUserConfig>,
}
/// Self-reported feedback author identity (never used for authorization).
/// Merged only from trusted config tiers, so a cloned repo can't inject the
/// `command` escape hatch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedbackUserConfig {
    /// Sources tried in order for the name. `os_user` yields the OS user name;
    /// any other entry is a literal (`$VAR` expanded at load).
    pub name: Vec<String>,
    /// Sources tried in order for the email. `git_email` yields the global git
    /// email; any other entry is a literal (`$VAR` expanded at load) needing `@`.
    pub email: Vec<String>,
    /// Fallback domain for `<name>@<domain>` when no `email` source resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_domain: Option<String>,
    /// Optional `sh -c` script printing `{"name","email"}` JSON; its fields win
    /// over the lists above, with per-field fallback. Trusted config tiers only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    pub memory_flush: Option<crate::config::MemoryFlushConfig>,
    pub pruning: Option<crate::config::PruningConfig>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CliConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismissed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm_registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_leader: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_tips: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_registry: Option<bool>,
    /// Env `GROK_MINIMUM_VERSION`. See [`crate::util::config::VersionPolicy`] for
    /// the version-policy knobs. (Unrelated to
    /// `version_overrides[].maximum_version`, which gates config patches.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,
    /// Env `GROK_MAXIMUM_VERSION`. See [`crate::util::config::VersionPolicy`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_version: Option<String>,
    /// Env `GROK_REQUIRED_MINIMUM_VERSION`. See [`crate::util::config::VersionPolicy`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_minimum_version: Option<String>,
    /// Env `GROK_REQUIRED_MAXIMUM_VERSION`. See [`crate::util::config::VersionPolicy`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_maximum_version: Option<String>,
    /// Group sessions by repo in the picker and CLI listings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_picker_grouped: Option<bool>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiagnosticsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crash_handler: Option<bool>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// The pre-campaign `models.default` (merged user/managed/requirements)
    /// captured when a campaign is overriding the default, so model resolution can
    /// recover if the campaign points at a model missing from the catalog. `None`
    /// when there is nothing to recover to. Runtime-only; never serialized.
    #[serde(skip)]
    pub pre_campaign_default: Option<String>,
    /// Whether an active campaign is currently overriding `models.default`. The
    /// authoritative campaign-driven-default signal (set from the resolved active
    /// set), correct even when the user has no base default. Runtime-only.
    #[serde(skip)]
    pub default_is_campaign_driven: bool,
    /// Persisted effort for the default model; applied in `resolve_model_catalog`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_summary: Option<String>,
    /// Vision model used to transcribe user-supplied
    /// images via a separate endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_description: Option<String>,
    /// Model pin for next-prompt suggestions (tab-autocomplete ghost text).
    /// Unset = remote pin, then the client hint / built-in `grok-build-0.1`
    /// default with the catalog guard; see `ModelOverrideConfig::resolve`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_suggestion: Option<String>,
    /// Restricts which models are user-selectable for normal chat (picker,
    /// `/model`, `-m`). Non-matching models stay in the catalog but are never
    /// shown, defaulted to, or selectable. Special/internal models (web_search,
    /// image_description, subagents, fork secondary) are exempt.
    ///
    /// Glob patterns (`*`, `?`, `[...]`) match the model id or catalog key,
    /// case-sensitive. Empty = no restriction; an excluded explicit `default`/`-m`
    /// is rejected at startup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<Vec<String>>,
    /// Soft shortlist for fast model cycling (Pi-style **scoped models**).
    /// Glob patterns match catalog key or model slug. Empty/None = cycle all
    /// currently usable models. Does **not** hide or block other models in
    /// `/model` / Ctrl+M — use `allowed_models` / `hidden_models` for that.
    ///
    /// Alias `enabledModels` accepts Pi's camelCase settings key.
    #[serde(alias = "enabledModels", skip_serializing_if = "Option::is_none")]
    pub enabled_models: Option<Vec<String>>,
    /// Force `hidden = true` on these model IDs (still usable via `-m`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden_models: Option<Vec<String>>,
    /// Remove these model IDs from the catalog entirely. Wins over `hidden_models`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_models: Option<Vec<String>>,
    /// Fallback `agent_type` for models without a per-model override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Global default request headers applied to every model. A per-model
    /// `[model.<id>].extra_headers` entry overrides per key (case-insensitive).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub extra_headers: IndexMap<String, String>,
    /// Global default values applied to every model that leaves the field
    /// unset; a per-model `[model.<id>]` value always wins. A deliberately
    /// small, allow-listed subset of the per-model fields (only `Option` ones,
    /// so "unset" is unambiguous). Future: these could consolidate into a
    /// `[models.defaults]` sub-table mirroring the per-model schema 1:1; kept
    /// flat for now as that is a larger refactor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_idle_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_tool_calls: Option<bool>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_for_upload: Option<bool>,
    /// Budget (seconds) for the turn-end upload flush when
    /// `block_for_upload` is active. Default 60.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_flush_timeout_secs: Option<u64>,
}
impl HarnessConfig {}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}
/// `[hub]` section from config.toml.
///
/// Optional default Computer Hub URL for **workspace provider** exposure
/// (`grok workspace` / leader `with_default_hub_url`). Does **not** enable
/// agent-side harness/client connections or alter local session behavior.
///
/// ```toml
/// [hub]
/// url = "wss://hub.x.ai/ws"
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HubConfig {
    /// Hub WebSocket URL (`ws://` or `wss://`) used as the leader default for
    /// `grok workspace start` when the CLI does not pass `--hub-url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
impl HubConfig {
    /// Whether a non-empty hub URL is configured (workspace default only).
    pub fn is_enabled(&self) -> bool {
        self.url.as_ref().is_some_and(|u| !u.trim().is_empty())
    }
}
/// `[collab]` section — multi-user collab relay (OMP-inspired scaffold).
///
/// No websocket mesh is started yet; these fields are reserved for a future
/// `/collab` entry point and UI deep links.
///
/// ```toml
/// [collab]
/// enabled = false
/// relay_url = "wss://collab.example/ws"
/// web_url = "https://collab.example"
/// display_name = "alice"
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CollabConfigToml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}
impl CollabConfigToml {
    pub(crate) fn resolve(&self) -> crate::session::p2_scaffolds::CollabConfig {
        crate::session::p2_scaffolds::CollabConfig {
            enabled: self.enabled.unwrap_or(false),
            relay_url: self
                .relay_url
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            web_url: self
                .web_url
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            display_name: self
                .display_name
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreePoolConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_count_threshold: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<usize>,
}
/// `[worktree]` section from config.toml (auto-GC policy lives under `auto_gc`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeConfigSection {
    #[serde(default)]
    pub auto_gc: crate::util::config::WorktreeAutoGcSettings,
}
/// `[sandbox]` section from config.toml.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxSettingsConfig {
    /// "off", "workspace", "devbox", "read-only", "strict", or custom name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Skip bash permission prompts when sandbox is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_allow_bash: Option<bool>,
}
impl SandboxSettingsConfig {
    pub(crate) fn from_effective_config() -> Self {
        crate::config::load_effective_config()
            .ok()
            .and_then(|v| v.get("sandbox")?.clone().try_into().ok())
            .unwrap_or_default()
    }
    /// Resolve sandbox profile: requirement > CLI > env > config > "off".
    pub fn resolve_profile(
        &self,
        cli_arg: Option<&str>,
        requirement: Option<&str>,
    ) -> Resolved<String> {
        if let Some(val) = requirement {
            return Resolved::new(val.to_owned(), ConfigSource::Requirement);
        }
        resolve_string_flag(cli_arg, "GROK_SANDBOX", self.profile.as_deref(), None)
            .unwrap_or_else(|| Resolved::new("off".to_owned(), ConfigSource::Default))
    }
    /// Resolve auto_allow_bash: requirement > env > config > default (false).
    pub(crate) fn resolve_auto_allow_bash(&self, requirement: Option<bool>) -> Resolved<bool> {
        BoolFlag::env("GROK_SANDBOX_AUTO_ALLOW_BASH")
            .requirement(requirement)
            .config(self.auto_allow_bash)
            .resolve()
    }
}
/// `[marketplace]` section from config.toml (plugin marketplace sources).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MarketplaceConfig {
    /// `[[marketplace.sources]]` entries.
    #[serde(default)]
    pub sources: Vec<MarketplaceSourceEntry>,
    /// Written/read out-of-band by `extensions::marketplace`, opaque so a wrong-typed value can't fail load.
    #[serde(default)]
    pub official_marketplace_auto_installed: Option<toml::Value>,
    /// Written/read out-of-band by `extensions::marketplace`, opaque so a wrong-typed value can't fail load.
    #[serde(default)]
    pub default_skills_installs_purged: Option<toml::Value>,
}
/// A single `[[marketplace.sources]]` entry.
#[derive(Clone, Debug, Deserialize)]
pub struct MarketplaceSourceEntry {
    pub name: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub git: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}
/// `[storage]` section from config.toml.
///
/// Controls session persistence settings like cleanup TTL.
/// Read by `resolve_cleanup_ttl_days()` in `session/persistence.rs`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Number of days to keep stale sessions before cleanup. Default: 30.
    pub cleanup_ttl_days: Option<u32>,
}
/// `[paths]` configuration: extra directories to scan for skills, rules, etc.
///
/// These supplement the built-in scan locations (`.grok/skills/`,
/// `.agents/skills/`, `~/.grok/skills/`). They're written by `/import-claude`
/// to preserve previously-discovered Claude directories after the runtime
/// `.claude/` cutoff (see `[claude_compat] imported`).
///
/// Example:
/// ```toml
/// [paths]
/// extra_skill_dirs = ["~/.claude/skills", "/path/to/.claude/skills"]
/// extra_rule_dirs = ["~/.claude/rules"]
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Additional directories to scan for skills (each contains `<skill>/SKILL.md`).
    pub extra_skill_dirs: Vec<String>,
    /// Additional directories to scan for rules (each contains `*.md`).
    pub extra_rule_dirs: Vec<String>,
}
/// `[permission]` known keys, declared for the unrecognized-key scan only;
/// consumed out-of-band. Keys stay typed so a typo (e.g. `denny`) still warns.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PermissionKnownKeys {
    /// Compact rule arrays (`parse_toml_permission_section`).
    pub allow: Option<toml::Value>,
    pub deny: Option<toml::Value>,
    pub ask: Option<toml::Value>,
    /// Verbose `[[permission.rules]]` form.
    pub rules: Option<toml::Value>,
}
/// `[shell_environment_policy]` known keys, for the unrecognized-key scan only;
/// the value is parsed at spawn by [`crate::util::config::resolve_shell_env_policy`].
/// `Option<toml::Value>` (no `deny_unknown_fields`) keeps a typo a warning, not a
/// load failure, like [`PermissionKnownKeys`].
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ShellEnvironmentPolicyKnownKeys {
    pub inherit: Option<toml::Value>,
    pub ignore_default_excludes: Option<toml::Value>,
    pub exclude: Option<toml::Value>,
    pub set: Option<toml::Value>,
    pub include_only: Option<toml::Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub features: Features,
    /// `[goal]` section: canonical `/goal` configuration. See [`GoalConfig`].
    #[serde(default)]
    pub goal: GoalConfig,
    #[serde(default)]
    pub workflows: WorkflowsConfig,
    /// `[doom_loop_recovery]` section: the shared settings struct — ONE type
    /// serves this TOML table and the remote remote settings `doom_loop_recovery`
    /// object. See [`crate::util::config::DoomLoopRecoverySettings`].
    #[serde(default)]
    pub doom_loop_recovery: crate::util::config::DoomLoopRecoverySettings,
    /// `[worktree]` section (currently `[worktree.auto_gc]` only).
    #[serde(default)]
    pub worktree: WorktreeConfigSection,
    /// `[auto_mode]` section: Auto permission-mode configuration. See [`AutoModeConfig`].
    #[serde(default)]
    pub auto_mode: AutoModeConfig,
    /// `[model.*]` overrides from config.toml. Resolve via `resolve_model_list()`.
    #[serde(skip)]
    pub config_models: IndexMap<String, ConfigModelOverride>,
    #[serde(skip)]
    pub config_warnings: Vec<super::config_model_override_parse::ConfigWarning>,
    pub grok_com_config: GrokComConfig,
    /// `[auth_provider.<name>]` tables, populated by
    /// [`parse_auth_providers`] from trusted config layers only.
    #[serde(skip)]
    pub auth_providers: IndexMap<String, crate::auth::AuthProviderConfig>,
    #[serde(skip)]
    pub model_providers: IndexMap<String, ModelProviderConfig>,
    /// Written by the client via `config_toml_edit`; absorbed so it isn't
    /// flagged as an unrecognized key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<toml::Value>,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub toolset: ShellToolsetConfig,
    /// Validation only; the value is parsed at spawn by `resolve_shell_env_policy`.
    #[serde(default, skip_serializing)]
    pub shell_environment_policy: ShellEnvironmentPolicyKnownKeys,
    #[serde(default)]
    pub endpoints: EndpointsConfig,
    /// `[platforms.<id>]` — Moonshot (and future) open-platform API keys.
    /// Secrets; never re-serialized. See [`PlatformsConfig`].
    #[serde(default, skip_serializing)]
    pub platforms: PlatformsConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Session behavior configuration.
    #[serde(default)]
    pub session: SessionConfig,
    /// Agent definition selection configuration.
    /// Set in `config.toml` under `[agent]` to choose which agent definition
    /// is used for all sessions (unless overridden by CLI flag or ACP meta).
    #[serde(default)]
    pub agent: AgentSelectionConfig,
    #[serde(default)]
    pub repo_changes_dedup: RepoChangesDedupConfig,
    /// Skills discovery configuration.
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Raw `[compat]` vendor-compatibility config (per-vendor × per-surface
    /// toggles). Resolved into [`Config::compat_resolved`] by
    /// `resolve_runtime_fields`.
    #[serde(default)]
    pub compat: CompatConfigToml,
    /// Plugin system configuration.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Feedback submission configuration.
    #[serde(default)]
    pub feedback: FeedbackConfig,
    /// Filesystem path overrides (`[paths]` in config.toml).
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default, skip_serializing)]
    pub cli: CliConfig,
    #[serde(default, skip_serializing)]
    pub models: ModelsConfig,
    #[serde(default, skip_serializing)]
    pub harness: HarnessConfig,
    #[serde(default, skip_serializing)]
    pub relay: RelayConfig,
    /// Computer Hub configuration (`[hub]` in config.toml).
    #[serde(default, skip_serializing)]
    pub hub: HubConfig,
    /// Multi-user collab scaffold (`[collab]` in config.toml).
    #[serde(default, skip_serializing)]
    pub collab: CollabConfigToml,
    #[serde(default, skip_serializing)]
    pub worktree_pool: WorktreePoolConfig,
    #[serde(default, skip_serializing)]
    pub sandbox: SandboxSettingsConfig,
    #[serde(default, skip_serializing)]
    pub mcp_servers: std::collections::HashMap<String, crate::util::config::McpServerConfig>,
    #[serde(default, skip_serializing)]
    pub disabled_mcp_servers: Vec<String>,
    #[serde(default, skip_serializing)]
    pub disabled_mcp_tools: std::collections::HashMap<String, Vec<String>>,
    #[serde(default, skip_serializing)]
    pub subagents: crate::config::SubagentsConfig,
    #[serde(default, skip_serializing)]
    pub memory: crate::config::MemoryConfig,
    #[serde(default, skip_serializing)]
    pub compaction: CompactionConfig,
    #[serde(default, skip_serializing)]
    pub managed_mcps: crate::config::ManagedMcpsConfig,
    /// `[auth]` alias — consumed by `expand_auth_alias` before serde.
    /// Typed as `GrokComConfig` (same schema) so sub-field typos are caught.
    #[serde(default, skip_serializing)]
    pub auth: Option<GrokComConfig>,
    /// `[desktop]` section — owned by grok-desktop (Electron app), opaque to the CLI agent.
    #[serde(default, skip_serializing)]
    pub desktop: Option<toml::Value>,
    /// Top-level `announcements` array — consumed by `resolve_announcements`.
    #[serde(default, skip_serializing)]
    pub announcements: Vec<xai_grok_announcements::RemoteAnnouncement>,
    /// `[tips]` section — consumed by `merge_tips`.
    #[serde(default, skip_serializing)]
    pub tips: Option<crate::util::config::TipsOverride>,
    /// `[permission]` — consumed out-of-band; see [`PermissionKnownKeys`].
    #[serde(default, skip_serializing)]
    pub permission: PermissionKnownKeys,
    /// `[tools]` — also read by `ToolsConfig::resolve()`.
    #[serde(default, skip_serializing)]
    pub tools: crate::config::ToolsConfig,
    /// `[storage]` — also read by `resolve_cleanup_ttl_days()`.
    #[serde(default, skip_serializing)]
    pub storage: StorageConfig,
    /// `[marketplace]` — also read by `xai_grok_plugin_marketplace::load_sources()`.
    #[serde(default, skip_serializing)]
    pub marketplace: MarketplaceConfig,
    /// `[diagnostics]` — crash handler toggle (`load_crash_handler_enabled_sync`).
    #[serde(default, skip_serializing)]
    pub diagnostics: DiagnosticsConfig,
    /// Storage mode for session persistence.
    /// When running in relay/headless mode, this should be set to Writeback.
    /// Defaults to reading from GROK_STORAGE_MODE env var.
    #[serde(skip)]
    pub storage_mode: StorageMode,
    /// CLI override for the default model ID.
    #[serde(skip)]
    pub default_model_override: Option<String>,
    /// CLI override for reasoning effort.
    #[serde(skip)]
    pub reasoning_effort_override: Option<ReasoningEffort>,
    /// CLI override for the web search model ID.
    #[serde(skip)]
    pub web_search_model_override: Option<String>,
    /// CLI override for the session summary model ID.
    #[serde(skip)]
    pub session_summary_model_override: Option<String>,
    /// CLI override for YOLO mode (auto-approve all permissions).
    /// Takes precedence over default settings.
    #[serde(skip)]
    pub default_yolo_mode: bool,
    /// Start sessions in auto permission mode (classifier) when no per-session override.
    pub default_auto_mode: bool,
    /// CLI `--experimental-memory` flag. Stored for `ConfigReloader` hot-reload re-resolution.
    #[serde(skip)]
    pub cli_experimental_memory: bool,
    /// CLI `--no-memory` flag. Stored for `ConfigReloader` hot-reload re-resolution.
    #[serde(skip)]
    pub cli_no_memory: bool,
    /// Original CLI `--subagents` tri-state, preserved for re-resolution
    /// when remote settings settings are refreshed on /new.
    #[serde(skip)]
    pub cli_subagents: Option<bool>,
    /// Resolved memory configuration. `None` when memory is disabled.
    /// Resolved by [`RuntimeResolutionContext`] in [`Config::resolve_runtime_fields`].
    #[serde(skip)]
    pub memory_config: Option<crate::config::MemoryConfig>,
    /// CLI override: path to an agent profile (.md file with YAML frontmatter).
    #[serde(skip)]
    pub agent_profile_path: Option<PathBuf>,
    /// Client version string (e.g., "0.1.77 (abc1234)").
    /// Set by the TUI/CLI launcher and used as fallback when clients don't provide clientVersion.
    #[serde(skip)]
    pub client_version: Option<String>,
    /// The mode in which the agent is running.
    /// Determines behavior like relay sync enablement (only enabled in TUI mode).
    #[serde(skip)]
    pub mode: AgentMode,
    /// Remote settings fetched from cli-chat-proxy at startup.
    /// Used for upload limits (replaces on-demand /v1/storage/limits fetch).
    #[serde(skip)]
    pub remote_settings: Option<crate::util::config::RemoteSettings>,
    #[serde(skip)]
    pub cli_agents: Vec<xai_grok_agent::config::AgentDefinition>,
    #[serde(skip)]
    pub cli_agent_overrides: CliAgentOverrides,
    /// Whether subagent (task tool) support is enabled. Enabled by default;
    /// disabled only via `GROK_SUBAGENTS=0` or `[subagents] enabled = false`.
    /// Not remotely gated.
    #[serde(skip)]
    pub subagents_enabled: bool,
    /// Resolved max subagent nesting depth (see
    /// [`crate::config::SubagentsConfig::resolve_max_depth`]).
    #[serde(skip)]
    pub subagents_max_depth: u32,
    #[serde(skip)]
    pub subagents_max_concurrent: usize,
    #[serde(skip)]
    pub subagents_limit_behavior:
        xai_grok_tools::implementations::grok_build::task::admission::LimitBehavior,
    #[serde(skip)]
    pub workflow_max_concurrent_agents: usize,
    /// Per-subagent model ID overrides from `[subagents.models]` in config.toml.
    /// Keys are agent names, values are model IDs. Set alongside `subagents_enabled`
    /// from `SubagentsConfig::resolve()`.
    #[serde(skip)]
    pub subagent_model_overrides: std::collections::HashMap<String, String>,
    /// Per-subagent reasoning effort overrides from `[subagents.effort]` in
    /// config.toml. Keys are agent names, values are effort levels
    /// (`none`…`ultra`). Set alongside `subagent_model_overrides`.
    #[serde(skip)]
    pub subagent_effort_overrides: std::collections::HashMap<String, String>,
    /// Per-subagent enable/disable toggles from `[subagents.toggle]` in config.toml.
    /// Keys are agent names, values are booleans. Omitted agents default to enabled.
    #[serde(skip)]
    pub subagent_toggle: std::collections::HashMap<String, bool>,
    /// Trust-independent roles from inline, user, and bundled sources.
    #[serde(skip)]
    pub subagent_roles:
        std::collections::HashMap<String, xai_grok_subagent_resolution::config::SubagentRole>,
    /// Trust-independent personas from inline, user, and bundled sources.
    #[serde(skip)]
    pub subagent_personas:
        std::collections::HashMap<String, xai_grok_subagent_resolution::config::SubagentPersona>,
    /// Whether web search is force-disabled via `--disable-web-search` CLI flag.
    /// When true, the web search tool is never added to the agent toolset
    /// regardless of available credentials.
    #[serde(default)]
    pub disable_web_search: bool,
    /// Whether the runtime turn-end TodoGate is force-enabled via the
    /// `--todo-gate` CLI flag. Session-scoped — not persisted. When
    /// true, flips the runtime policy's `enabled` bit on regardless of
    /// remote settings or the built-in default (which is `false`).
    /// The gate runs only while a `/goal` is active (goal reminders
    /// inject `<task_completion_discipline>`); global built-in templates
    /// do not activate it.
    #[serde(skip)]
    pub todo_gate: bool,
    /// Path for the Layer-3 LazinessDetector debug log
    /// (`--laziness-debug-log`). When `Some`, the classifier fires
    /// after every turn (bypassing the idle wait, the per-model
    /// enable gate, and the nudge cap) and appends a JSONL line per
    /// fire to this file. Observation-only — no nudges are injected
    /// in this mode. Session-scoped, not persisted.
    #[serde(skip)]
    pub laziness_debug_log: Option<std::path::PathBuf>,
    /// Whether tools should respect `.gitignore` patterns.
    /// When `true`, all tools including `read_file` block gitignored files.
    /// When `false` (default), each tool applies its own default
    /// (`read_file` allows, others block).
    /// Resolved by [`crate::config::ToolsConfig::resolve`].
    #[serde(skip)]
    pub respect_gitignore: bool,
    /// When `true` (and no valid `zdr_video_output_s3` bucket is set),
    /// `MvpAgent::prepare_video_gen_config` marks the video tools
    /// zdr-restricted: they stay advertised but short-circuit at call time
    /// with setup guidance. Resolved by [`crate::config::ToolsConfig::resolve`].
    #[serde(skip)]
    pub disable_zdr_incompatible_tools: bool,
    /// S3 config for ZDR video output (presigned upload to team bucket).
    /// Only used when `disable_zdr_incompatible_tools` is `true` and the
    /// config is valid. Resolved by [`crate::config::ToolsConfig::resolve`].
    #[serde(skip)]
    pub zdr_video_output_s3:
        Option<xai_grok_tools::implementations::grok_build::video_gen::ZdrVideoOutputS3Config>,
    /// Whether to enrich path-not-found errors with CWD reminders,
    /// "dropped repo folder" correction, and similar-name suggestions.
    /// Default `false`. Enabled via remote settings.
    /// Serialized to `config.json` on GCS so traces can distinguish
    /// which sessions had path-not-found hints active.
    #[serde(default)]
    pub path_not_found_hints: bool,
    /// Whether to fetch managed MCP configs from the managed connectors service at startup.
    /// Resolved by [`crate::config::ManagedMcpsConfig::resolve`]: env var >
    /// config.toml > remote settings > default (off in headless, on in interactive).
    #[serde(skip)]
    pub managed_mcps_enabled: bool,
    #[serde(skip)]
    pub managed_mcp_gateway_tools_enabled: bool,
    /// Whether auto-wake is enabled: when a background task or subagent
    /// completes, immediately inject a synthetic prompt instead of waiting
    /// for the idle-gated notification drain.
    #[serde(skip)]
    pub auto_wake_enabled: bool,
    /// Resolved vendor-compat config (env → `[compat]` TOML → feature flag →
    /// default ON), built from `compat` + `remote_settings` in
    /// `resolve_runtime_fields`. Threaded into skills / rules / AGENTS.md
    /// discovery.
    #[serde(skip)]
    pub compat_resolved: CompatConfig,
    /// Enforced requirement pins from `requirements.toml`.
    #[serde(skip)]
    pub requirements: Requirements,
    /// Model ID for web_search.
    #[serde(skip)]
    pub web_search_model: String,
    /// Session title model. Resolved to the compiled default
    /// (`default_session_summary_model`) when unset; see `ModelOverrideConfig::resolve`.
    #[serde(skip)]
    pub session_summary_model: Option<String>,
    /// Image describe model (`grok-build` default via `ModelOverrideConfig::resolve`).
    #[serde(skip)]
    pub image_description_model: Option<String>,
    /// Next-prompt suggestion model pin (`env > [models] prompt_suggestion >
    /// remote`), consumed catalog-guarded by `handle_suggest_prompt`; see
    /// `ModelOverrideConfig::resolve`.
    #[serde(skip)]
    pub prompt_suggest_model_pin: crate::config::PromptSuggestModelPin,
}
#[derive(Debug, Clone, Default)]
pub struct CliAgentOverrides {
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    pub permission_rules: Vec<xai_grok_workspace::permission::types::PermissionRule>,
    pub max_turns: Option<u32>,
    pub permission_mode: Option<xai_grok_agent::config::PermissionMode>,
}
impl CliAgentOverrides {
    /// Apply to the *main-session* agent, which the operator defines directly:
    /// the flags are authoritative, so they replace the agent's own fields.
    /// Spawned subagents instead layer these on top of an author's definition —
    /// see [`Self::apply_to_subagent_definition`].
    pub(crate) fn apply_to_definition(&self, def: &mut xai_grok_agent::config::AgentDefinition) {
        if let Some(ref tools) = self.tools {
            def.tools = tools.clone();
        }
        if let Some(ref dt) = self.disallowed_tools {
            def.disallowed_tools = dt.clone();
        }
        if let Some(ref pm) = self.permission_mode {
            def.permission_mode = pm.clone();
        }
    }
    /// Subagent variant of [`Self::apply_to_definition`]: records the flags as
    /// session-clamp state (see [`AgentDefinition::session_tools_allowlist`])
    /// instead of overwriting the agent author's own fields.
    pub(crate) fn apply_to_subagent_definition(
        &self,
        def: &mut xai_grok_agent::config::AgentDefinition,
    ) {
        def.session_tools_allowlist = self.tools.clone();
        def.session_tools_denylist = self.disallowed_tools.clone();
        if let Some(ref parent_mode) = self.permission_mode
            && def.plugin_name.is_none()
        {
            def.permission_mode =
                resolve_subagent_permission_mode(def.permission_mode.clone(), parent_mode);
        }
    }
    pub(crate) fn has_definition_overrides(&self) -> bool {
        self.tools.is_some() || self.disallowed_tools.is_some() || self.permission_mode.is_some()
    }
}
/// Parent bypassPermissions/acceptEdits/auto override the subagent's own mode
/// (spec); any other parent mode keeps it.
fn resolve_subagent_permission_mode(
    own: PermissionMode,
    parent: &PermissionMode,
) -> PermissionMode {
    match parent {
        PermissionMode::BypassPermissions | PermissionMode::AcceptEdits | PermissionMode::Auto => {
            parent.clone()
        }
        _ => own,
    }
}
pub use xai_grok_agent::config::AgentDefinition;
pub use xai_grok_agent::config::Effort;
pub use xai_grok_agent::config::PermissionMode;
pub use xai_grok_shared::ui_config::{ContextualHints, UiConfig};
/// Configuration for selecting the agent definition.
///
/// Set in `config.toml` under `[agent]`:
///
/// ```toml
/// [agent]
/// # Use a named agent (looked up via discovery: .grok/agents/, ~/.grok/agents/, built-ins)
/// name = "my-custom-agent"
///
/// # OR: path to an agent definition file (.md with YAML frontmatter)
/// definition = "/path/to/my-agent.md"
/// ```
///
/// Priority (highest to lowest):
/// 1. ACP session-level `_meta.agentProfile`
/// 2. CLI `--agent-profile` flag
/// 3. `[agent]` config.toml section (this config)
/// 4. `GROK_AGENT` env var
/// 5. Default `grok-build` agent
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSelectionConfig {
    /// Name of a built-in or discovered agent definition.
    /// Looked up via `xai_grok_agent::discovery::by_name_in_cwd()`.
    /// Examples: "grok-build", "browser-use", or a custom agent name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Path to an agent definition file (.md with YAML frontmatter).
    /// When set, the agent is loaded from this file.
    /// Supports environment variable expansion (e.g., `$HOME/.grok/agents/my-agent.md`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<PathBuf>,
    /// Global system-prompt identity label. Per-model override wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_label: Option<String>,
}
/// Configuration for session behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionConfig {
    /// Context window usage percentage (0-100) at which auto-compact is triggered.
    /// When the session's token usage exceeds this percentage of the model's context window,
    /// the conversation will be automatically summarized to free up space.
    ///
    /// `None` means "user didn't set it"; the resolver in
    /// `crate::util::config::resolve_auto_compact_threshold_percent` falls
    /// through to remote tiers and ultimately the hardcoded default 85.
    /// Read this field via the resolver — not directly — to honor the full
    /// precedence chain (env, per-model, remote, default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold_percent: Option<u8>,
    /// Whether to load environment variables from .envrc files.
    /// When enabled, the session will parse .envrc in the workspace directory
    /// and inject the environment variables into bash commands.
    /// Defaults to `true` when unset. `Option<bool>` so `None`
    /// round-trips as absent on disk (managed config wins over default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_envrc: Option<bool>,
}
/// Configuration for change-archive deduplication.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoChangesDedupConfig {
    pub enabled: bool,
    /// Include inline content even when references exist.
    pub include_inline_fallback: bool,
    /// Omit inline content larger than this (0 = no limit).
    pub max_inline_bytes: usize,
    /// Deduplicate untracked file content.
    pub dedup_untracked: bool,
    /// Deduplicate binary file blobs.
    pub dedup_binary: bool,
    /// Skip untracked files larger than this (0 = no limit).
    pub untracked_max_bytes: usize,
    /// Optional glob patterns to exclude untracked paths.
    pub untracked_exclude_globs: Vec<String>,
}
impl RepoChangesDedupConfig {}
impl Default for RepoChangesDedupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            include_inline_fallback: false,
            max_inline_bytes: 0,
            dedup_untracked: true,
            dedup_binary: true,
            untracked_max_bytes: 0,
            untracked_exclude_globs: Vec::new(),
        }
    }
}
impl Default for Config {
    fn default() -> Self {
        let endpoints = EndpointsConfig::default();
        let mut cfg = Self {
            features: Features::default(),
            goal: GoalConfig::default(),
            workflows: WorkflowsConfig::default(),
            doom_loop_recovery: crate::util::config::DoomLoopRecoverySettings::default(),
            worktree: WorktreeConfigSection::default(),
            auto_mode: AutoModeConfig::default(),
            config_models: IndexMap::new(),
            config_warnings: Vec::new(),
            grok_com_config: GrokComConfig::default(),
            auth_providers: IndexMap::new(),
            model_providers: IndexMap::new(),
            hints: None,
            ui: UiConfig::default(),
            toolset: ShellToolsetConfig::default(),
            shell_environment_policy: ShellEnvironmentPolicyKnownKeys::default(),
            endpoints,
            platforms: PlatformsConfig::default(),
            telemetry: TelemetryConfig::default(),
            session: SessionConfig::default(),
            agent: AgentSelectionConfig::default(),
            repo_changes_dedup: RepoChangesDedupConfig::default(),
            skills: SkillsConfig::default(),
            compat: CompatConfigToml::default(),
            plugins: PluginsConfig::default(),
            feedback: FeedbackConfig::default(),
            paths: PathsConfig::default(),
            cli: CliConfig::default(),
            models: ModelsConfig::default(),
            harness: HarnessConfig::default(),
            relay: RelayConfig::default(),
            hub: HubConfig::default(),
            collab: CollabConfigToml::default(),
            worktree_pool: WorktreePoolConfig::default(),
            sandbox: SandboxSettingsConfig::default(),
            mcp_servers: std::collections::HashMap::new(),
            disabled_mcp_servers: Vec::new(),
            disabled_mcp_tools: std::collections::HashMap::new(),
            subagents: crate::config::SubagentsConfig::default(),
            memory: crate::config::MemoryConfig::default(),
            compaction: CompactionConfig::default(),
            managed_mcps: crate::config::ManagedMcpsConfig::default(),
            auth: None,
            desktop: None,
            announcements: Vec::new(),
            tips: None,
            permission: PermissionKnownKeys::default(),
            tools: crate::config::ToolsConfig::default(),
            storage: StorageConfig::default(),
            marketplace: MarketplaceConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            storage_mode: StorageMode::resolve(None, None),
            default_model_override: None,
            reasoning_effort_override: None,
            web_search_model_override: None,
            session_summary_model_override: None,
            default_yolo_mode: false,
            default_auto_mode: false,
            agent_profile_path: None,
            client_version: Some(xai_grok_version::VERSION.to_string()),
            mode: AgentMode::default(),
            remote_settings: None,
            cli_agents: Vec::new(),
            cli_agent_overrides: CliAgentOverrides::default(),
            subagents_enabled: true,
            subagents_max_depth: crate::config::SubagentsConfig::DEFAULT_MAX_DEPTH,
            subagents_max_concurrent:
                xai_grok_tools::implementations::grok_build::task::admission::DEFAULT_MAX_CONCURRENT,
            subagents_limit_behavior: Default::default(),
            workflow_max_concurrent_agents:
                crate::session::workflow::host_service::DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS,
            subagent_model_overrides: std::collections::HashMap::new(),
            subagent_effort_overrides: std::collections::HashMap::new(),
            subagent_toggle: std::collections::HashMap::new(),
            subagent_roles: std::collections::HashMap::new(),
            subagent_personas: std::collections::HashMap::new(),
            disable_web_search: false,
            todo_gate: false,
            laziness_debug_log: None,
            respect_gitignore: false,
            disable_zdr_incompatible_tools: false,
            zdr_video_output_s3: None,
            path_not_found_hints: false,
            cli_experimental_memory: false,
            cli_no_memory: false,
            cli_subagents: None,
            memory_config: None,
            managed_mcps_enabled: true,
            managed_mcp_gateway_tools_enabled: false,
            auto_wake_enabled: true,
            compat_resolved: CompatConfig::default(),
            requirements: Requirements::default(),
            web_search_model: crate::models::default_web_search_model().to_owned(),
            session_summary_model: None,
            image_description_model: None,
            prompt_suggest_model_pin: crate::config::PromptSuggestModelPin::Unpinned,
        };
        cfg.apply_env_overrides();
        cfg
    }
}
/// Config paths read by raw-layer resolvers, not [`Config`] serde fields, so
/// `serde_ignored` must not report them as unrecognized keys.
const NON_SERDE_CONFIG_PATHS: &[&str] = &[
    crate::util::config::REMOTE_FETCH_CONFIG_PATH,
    crate::util::config::SLASH_COMMAND_TAGS_CONFIG_PATH,
];
/// [`NON_SERDE_CONFIG_PATHS`] plus the multi-path groups.
fn is_non_serde_config_path(path: &str) -> bool {
    NON_SERDE_CONFIG_PATHS.contains(&path)
        || crate::util::config::WEB_SEARCH_DOMAIN_CONFIG_PATHS.contains(&path)
}
/// Parse `[auth_provider.<name>]` tables leniently: a malformed entry warns
/// (surfaced by `grok inspect`) and is skipped, so it fails closed for the
/// models referencing it instead of failing the whole config.
fn parse_auth_providers(
    raw_config: &toml::Value,
) -> (
    IndexMap<String, crate::auth::AuthProviderConfig>,
    Vec<super::config_model_override_parse::ConfigWarning>,
) {
    use super::config_model_override_parse::{ConfigWarning, ConfigWarningKind};
    let mut providers = IndexMap::new();
    let mut warnings = Vec::new();
    let Some(section) = raw_config.get("auth_provider") else {
        return (providers, warnings);
    };
    let Some(table) = section.as_table() else {
        warnings.push(ConfigWarning::auth_provider_section(
            ConfigWarningKind::NotATable,
            format!(
                "`auth_provider` must be a table of [auth_provider.<name>] entries, got {}; \
                 all auth providers ignored",
                section.type_str()
            ),
        ));
        return (providers, warnings);
    };
    for (name, value) in table {
        let mut unknown = Vec::new();
        match serde_ignored::deserialize::<_, _, crate::auth::AuthProviderConfig>(
            value.clone(),
            |path| unknown.push(path.to_string()),
        ) {
            Ok(provider) => {
                for key in unknown {
                    warnings.push(ConfigWarning::auth_provider(
                        name,
                        Some(key.as_str()),
                        ConfigWarningKind::UnknownField,
                        "unrecognized key; field ignored".to_owned(),
                    ));
                }
                for (field, kind, reason) in auth_config_issues(&provider) {
                    warnings.push(ConfigWarning::auth_provider(
                        name,
                        Some(field),
                        kind,
                        reason,
                    ));
                }
                providers.insert(name.clone(), provider);
            }
            Err(error) => {
                warnings.push(ConfigWarning::auth_provider(
                    name,
                    None,
                    ConfigWarningKind::InvalidValue,
                    format!(
                        "failed to parse ({error}); provider skipped, referencing models \
                         resolve with no credential"
                    ),
                ));
            }
        }
    }
    (providers, warnings)
}
impl Config {
    /// Reject invalid glob patterns in the model-filter lists at config load, so
    /// a typo fails loudly instead of silently changing availability.
    pub fn validate_model_filters(&self) -> Result<(), String> {
        for (field, list) in [
            ("allowed_models", &self.models.allowed_models),
            ("enabled_models", &self.models.enabled_models),
            ("disabled_models", &self.models.disabled_models),
            ("hidden_models", &self.models.hidden_models),
        ] {
            if let Err(bad) = crate::agent::models::ModelGlobSet::compile(list.as_ref()) {
                return Err(format!(
                    "{field} has an invalid pattern: {}. Patterns use * and ? wildcards.",
                    bad.join(", ")
                ));
            }
        }
        Ok(())
    }
    /// Build an `AuthManager` with the configured proxy URL applied.
    pub fn create_auth_manager(&self) -> AuthManager {
        AuthManager::new(
            &crate::util::grok_home::grok_home(),
            self.grok_com_config.clone(),
        )
        .with_proxy_base_url(&self.endpoints.proxy_url())
    }
    /// Deserialize the merged `base` document, also returning the ignored key
    /// paths whose top-level key appears in `user_config`. Paths outside it
    /// can only come from the serialized-defaults half of the merge and must
    /// not be blamed on the user.
    fn deserialize_collecting_unrecognized(
        base: toml::Value,
        user_config: &toml::Value,
    ) -> Result<(Self, Vec<String>), String> {
        let mut unused_keys = Vec::new();
        let config: Self = serde_ignored::deserialize(base, |path| {
            unused_keys.push(path.to_string());
        })
        .map_err(|e| e.to_string())?;
        let unrecognized_keys = match user_config.as_table() {
            Some(user_table) => unused_keys
                .into_iter()
                .filter(|path| {
                    let top_level = path.split('.').next().unwrap_or(path);
                    user_table.contains_key(top_level)
                })
                .filter(|path| !is_non_serde_config_path(path))
                .collect(),
            None => Vec::new(),
        };
        Ok((config, unrecognized_keys))
    }
    pub fn new_from_toml_cfg(raw_config: &toml::Value) -> Result<Self, String> {
        let raw_config = &Self::expand_auth_alias(raw_config);
        let super::config_model_override_parse::ParsedModelOverrides {
            models: config_models,
            warnings: config_warnings,
        } = super::config_model_override_parse::parse_model_overrides(raw_config);
        let (mut auth_providers, auth_provider_warnings) = parse_auth_providers(raw_config);
        let (model_providers, mut model_provider_warnings) = parse_model_providers(raw_config);
        for (id, provider) in &model_providers {
            if let Some(auth) = &provider.auth {
                let synthetic = model_provider_auth_name(id);
                if auth_providers.contains_key(&synthetic) {
                    model_provider_warnings
                        .push(
                            super::config_model_override_parse::ConfigWarning::model_provider(
                                id,
                                Some("auth"),
                                super::config_model_override_parse::ConfigWarningKind::ConflictingFields,
                                format!(
                                "inline auth overwrites a hand-written \
                                 [auth_provider.\"{synthetic}\"]; the `model_provider:` prefix is \
                                 a reserved namespace"
                            ),
                            ),
                        );
                }
                auth_providers.insert(synthetic, auth.clone());
            }
        }
        let mut base = toml::Value::try_from(Self::default()).map_err(|e| e.to_string())?;
        if let toml::Value::Table(ref mut t) = base {
            t.remove("model");
        }
        let mut raw_without_model_sections = raw_config.clone();
        if let toml::Value::Table(ref mut t) = raw_without_model_sections {
            t.remove("model");
            t.remove("auth_provider");
            t.remove("model_providers");
        }
        let parsed_mcp_servers =
            crate::util::config::parse_mcp_servers_from_toml(&raw_without_model_sections);
        if let toml::Value::Table(ref mut t) = raw_without_model_sections {
            t.remove("mcp_servers");
        }
        crate::config::deep_merge_toml(&mut base, &raw_without_model_sections);
        if let toml::Value::Table(ref mut t) = base {
            t.remove("mcp_servers");
        }
        let (mut config, mut unrecognized_keys) =
            Self::deserialize_collecting_unrecognized(base, &raw_without_model_sections)?;
        config.mcp_servers = parsed_mcp_servers.into_iter().collect();
        config.config_models = config_models;
        config.config_warnings = config_warnings;
        config.auth_providers = auth_providers;
        config.model_providers = model_providers;
        config.config_warnings.extend(auth_provider_warnings);
        config.config_warnings.extend(model_provider_warnings);
        unrecognized_keys.sort();
        for key in unrecognized_keys {
            config.config_warnings.push(
                super::config_model_override_parse::ConfigWarning::config_key(
                    key,
                    super::config_model_override_parse::ConfigWarningKind::UnknownField,
                    "unrecognized config key".to_owned(),
                ),
            );
        }
        let declared_provider_names: std::collections::HashSet<&str> = raw_config
            .get("auth_provider")
            .and_then(toml::Value::as_table)
            .map(|t| t.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let declared_model_provider_names: std::collections::HashSet<&str> = raw_config
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .map(|t| t.keys().map(String::as_str).collect())
            .unwrap_or_default();
        for (model_key, model) in &config.config_models {
            if let Some(ref name) = model.auth_provider
                && !config.auth_providers.contains_key(name)
                && !declared_provider_names.contains(name.as_str())
            {
                config.config_warnings.push(
                    super::config_model_override_parse::ConfigWarning::model(
                        model_key,
                        Some("auth_provider"),
                        super::config_model_override_parse::ConfigWarningKind::InvalidValue,
                        format!(
                            "references [auth_provider.{name}], which is not defined; \
                             the model resolves with no provider credential"
                        ),
                    ),
                );
            }
            if let Some(ref id) = model.model_provider
                && !config.model_providers.contains_key(id)
                && !declared_model_provider_names.contains(id.as_str())
            {
                config.config_warnings.push(
                    super::config_model_override_parse::ConfigWarning::model(
                        model_key,
                        Some("model_provider"),
                        super::config_model_override_parse::ConfigWarningKind::InvalidValue,
                        format!(
                            "references [model_providers.{id}], which is not defined; \
                             provider defaults are not applied — the model uses its own \
                             credential if set, otherwise fails closed on a custom endpoint"
                        ),
                    ),
                );
            }
        }
        for (id, provider) in &config.model_providers {
            if let Some(ref name) = provider.auth_provider
                && !config.auth_providers.contains_key(name)
                && !declared_provider_names.contains(name.as_str())
            {
                config.config_warnings.push(
                    super::config_model_override_parse::ConfigWarning::model_provider(
                        id,
                        Some("auth_provider"),
                        super::config_model_override_parse::ConfigWarningKind::InvalidValue,
                        format!(
                            "references [auth_provider.{name}], which is not defined; \
                             inheriting models fail closed with no provider credential"
                        ),
                    ),
                );
            }
        }
        super::config_model_override_parse::log_config_warnings(&config.config_warnings);
        config.platforms.warn_unknown_platforms();
        if config.grok_com_config.oidc.is_none() {
            config.grok_com_config.oidc = OidcAuthConfig::from_env();
        }
        if config.grok_com_config.oidc.is_none() && config.grok_com_config.oauth2.is_none() {
            config.grok_com_config.oauth2 = crate::auth::OAuth2ProviderConfig::from_env();
        }
        if config.client_version.is_none() {
            config.client_version = Self::default().client_version;
        }
        let model_overrides =
            crate::config::ModelOverrideConfig::resolve(None, None, raw_config, None);
        config.web_search_model = model_overrides.web_search;
        config.session_summary_model = model_overrides.session_summary;
        config.image_description_model = model_overrides.image_description;
        config.prompt_suggest_model_pin = model_overrides.prompt_suggestion;
        config.apply_env_overrides();
        Ok(config)
    }
    /// Populate trust-independent `#[serde(skip)]` subagent base fields.
    ///
    /// Must be called after `new_from_toml_cfg` on the **primary startup path**
    /// before the config is handed to `MvpAgent`. Project definitions are overlaid
    /// per cwd after that cwd's authoritative folder-trust resolve.
    pub(crate) fn resolve_subagents(&mut self, cli_flag: bool, raw_config: &toml::Value) {
        let sa = crate::config::SubagentsConfig::resolve(cli_flag, raw_config);
        let remote_settings = self.remote_settings.clone();
        self.resolve_subagent_limits(&sa, remote_settings.as_ref());
        self.subagents_enabled = sa.enabled;
        self.subagent_model_overrides = sa.models;
        self.subagent_effort_overrides = sa.effort;
        self.subagent_toggle = sa.toggle;
        self.subagent_roles = sa.roles;
        self.subagent_personas = sa.personas;
        let env = std::env::var(crate::config::SubagentsConfig::ENV_MAX_DEPTH).ok();
        let remote = self
            .remote_settings
            .as_ref()
            .and_then(|r| r.subagents_max_depth);
        self.subagents_max_depth =
            crate::config::SubagentsConfig::resolve_max_depth(env.as_deref(), sa.max_depth, remote);
    }
    fn resolve_subagent_limits(
        &mut self,
        sa: &crate::config::SubagentsConfig,
        remote: Option<&crate::util::config::RemoteSettings>,
    ) {
        use crate::config::SubagentsConfig;
        let env = |name: &str| std::env::var(name).ok();
        self.subagents_max_concurrent = SubagentsConfig::resolve_max_concurrent(
            env(SubagentsConfig::ENV_MAX_CONCURRENT).as_deref(),
            sa.max_concurrent,
            remote.and_then(|r| r.subagents_max_concurrent),
        );
        self.subagents_limit_behavior = SubagentsConfig::resolve_limit_behavior(
            env(SubagentsConfig::ENV_LIMIT_BEHAVIOR).as_deref(),
            sa.limit_behavior.as_deref(),
            remote.and_then(|r| r.subagents_limit_behavior.as_deref()),
        );
        self.workflow_max_concurrent_agents = SubagentsConfig::resolve_workflow_max_concurrent(
            env(SubagentsConfig::ENV_WORKFLOW_MAX_CONCURRENT).as_deref(),
            sa.workflow_max_concurrent,
            remote.and_then(|r| r.workflow_max_concurrent_agents),
        );
    }
    /// Resolve all `#[serde(skip)]` runtime fields that have resolver functions.
    ///
    /// Call immediately after `new_from_toml_cfg()`. Fields resolved:
    /// - subagents base layers (6 fields) via `SubagentsConfig::resolve`
    /// - respect_gitignore via `ToolsConfig::resolve`
    /// - disable_zdr_incompatible_tools via `ToolsConfig::resolve`
    /// - managed_mcps_enabled via `ManagedMcpsConfig::resolve`
    /// - web_search_model / session_summary_model / image_description_model /
    ///   prompt_suggest_model_pin via `ModelOverrideConfig::resolve`
    /// - memory_config via `MemoryConfig::resolve`
    /// - disable_web_search (CLI flag ORed with config.toml)
    /// - storage_mode via `StorageMode::resolve`
    /// - path_not_found_hints from remote_settings
    ///
    /// Note: `worktree_type` is resolved directly in `MvpAgent::new` via
    /// `resolve_worktree_type` since it's an agent-level field, not a Config field.
    pub fn resolve_runtime_fields(&mut self, ctx: &RuntimeResolutionContext<'_>) {
        self.cli_subagents = ctx.cli_subagents;
        self.web_search_model_override = ctx.cli_web_search_model.map(|s| s.to_owned());
        self.session_summary_model_override = ctx.cli_session_summary_model.map(|s| s.to_owned());
        let cli_flag = ctx.cli_subagents.unwrap_or(false);
        self.resolve_subagents(cli_flag, ctx.raw_config);
        let env = std::env::var(crate::config::SubagentsConfig::ENV_MAX_DEPTH).ok();
        let toml_max = ctx
            .raw_config
            .get("subagents")
            .and_then(|s| s.get("max_depth"))
            .and_then(|v| v.as_integer());
        let remote = ctx.remote_settings.and_then(|r| r.subagents_max_depth);
        self.subagents_max_depth =
            crate::config::SubagentsConfig::resolve_max_depth(env.as_deref(), toml_max, remote);
        let subagents_toml = crate::config::SubagentsConfig {
            max_concurrent: ctx
                .raw_config
                .get("subagents")
                .and_then(|s| s.get("max_concurrent"))
                .and_then(|v| v.as_integer()),
            limit_behavior: ctx
                .raw_config
                .get("subagents")
                .and_then(|s| s.get("limit_behavior"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            workflow_max_concurrent: ctx
                .raw_config
                .get("subagents")
                .and_then(|s| s.get("workflow_max_concurrent"))
                .and_then(|v| v.as_integer()),
            ..Default::default()
        };
        self.resolve_subagent_limits(&subagents_toml, ctx.remote_settings);
        let tools = crate::config::ToolsConfig::resolve(ctx.raw_config);
        self.respect_gitignore = match self.requirements.respect_gitignore.pinned() {
            Some(pinned) => pinned,
            None => tools.respect_gitignore,
        };
        self.disable_zdr_incompatible_tools = tools.disable_zdr_incompatible_tools;
        self.zdr_video_output_s3 = tools.zdr_video_output_s3;
        let mcps = crate::config::ManagedMcpsConfig::resolve(
            ctx.raw_config,
            ctx.remote_settings,
            ctx.is_headless,
        );
        self.managed_mcps_enabled = mcps.enabled;
        self.managed_mcp_gateway_tools_enabled = mcps.gateway_tools_enabled;
        let models = crate::config::ModelOverrideConfig::resolve(
            ctx.cli_web_search_model,
            ctx.cli_session_summary_model,
            ctx.raw_config,
            ctx.remote_settings,
        );
        self.web_search_model = models.web_search;
        self.session_summary_model = models.session_summary;
        self.image_description_model = models.image_description;
        self.prompt_suggest_model_pin = models.prompt_suggestion;
        self.cli_experimental_memory = ctx.cli_experimental_memory;
        self.cli_no_memory = ctx.cli_no_memory;
        let mem = crate::config::MemoryConfig::resolve(
            ctx.cli_experimental_memory,
            ctx.cli_no_memory,
            ctx.raw_config,
            ctx.remote_settings,
        );
        self.memory_config = if mem.enabled { Some(mem) } else { None };
        self.disable_web_search = self.disable_web_search || ctx.disable_web_search;
        self.todo_gate = ctx.todo_gate;
        self.laziness_debug_log = ctx.laziness_debug_log.map(std::path::Path::to_path_buf);
        self.storage_mode =
            crate::config::StorageMode::resolve(ctx.storage_mode, ctx.remote_settings);
        if let Some(v) = ctx.remote_settings.and_then(|s| s.path_not_found_hints) {
            self.path_not_found_hints = v;
        }
        self.auto_wake_enabled = BoolFlag::env("GROK_AUTO_WAKE")
            .config(self.features.auto_wake)
            .feature_flag(ctx.remote_settings.and_then(|r| r.auto_wake_enabled))
            .default(true)
            .resolve()
            .value;
        self.compat_resolved = resolve_compat_config(&self.compat, ctx.remote_settings);
    }
    /// Re-resolve eagerly-resolved runtime fields using the current `Config`
    /// state and fresh `raw_config`. Builds a [`RuntimeResolutionContext`] from
    /// the CLI flags already stored on this `Config`.
    ///
    /// Integration test coverage: `tests/test_settings_refresh.rs`.
    pub(crate) fn re_resolve_runtime_fields(&mut self, raw_config: &toml::Value) {
        let remote_settings = self.remote_settings.clone();
        let cli_web_search_model = self.web_search_model_override.clone();
        let cli_session_summary_model = self.session_summary_model_override.clone();
        let laziness_debug_log = self.laziness_debug_log.clone();
        let ctx = RuntimeResolutionContext {
            raw_config,
            remote_settings: remote_settings.as_ref(),
            is_headless: self.mode == AgentMode::Headless,
            cli_subagents: self.cli_subagents,
            cli_web_search_model: cli_web_search_model.as_deref(),
            cli_session_summary_model: cli_session_summary_model.as_deref(),
            cli_experimental_memory: self.cli_experimental_memory,
            cli_no_memory: self.cli_no_memory,
            disable_web_search: self.disable_web_search,
            todo_gate: self.todo_gate,
            laziness_debug_log: laziness_debug_log.as_deref(),
            storage_mode: None,
        };
        self.resolve_runtime_fields(&ctx);
        crate::util::config::set_remote_campaigns_from_settings(self.remote_settings.as_ref());
    }
    /// If the TOML contains `[auth]`, copy its contents under `[grok_com_config]`.
    /// `[grok_com_config]` takes precedence if both are present (explicit wins).
    ///
    /// This lets customers write the shorter `[auth.oidc]` instead of `[grok_com_config.oidc]`.
    fn expand_auth_alias(raw_config: &toml::Value) -> toml::Value {
        let mut config = raw_config.clone();
        if let toml::Value::Table(ref mut table) = config
            && let Some(auth) = table.remove("auth")
        {
            if let Some(gcc) = table.get_mut("grok_com_config") {
                if let (toml::Value::Table(gcc_table), toml::Value::Table(auth_table)) =
                    (gcc, &auth)
                {
                    for (k, v) in auth_table {
                        gcc_table.entry(k.clone()).or_insert(v.clone());
                    }
                }
            } else {
                table.insert("grok_com_config".to_owned(), auth);
            }
        }
        config
    }
    fn apply_env_overrides(&mut self) {
        self.telemetry.apply_env_overrides();
        if let Some(mode) = env_telemetry_mode("GROK_TELEMETRY_ENABLED") {
            self.features.telemetry = Some(mode);
        }
    }
    pub(crate) fn is_telemetry_enabled(&self) -> bool {
        self.resolve_telemetry_mode().value.is_enabled()
    }
    pub fn is_trace_upload_enabled(&self) -> bool {
        self.resolve_trace_upload().value
    }
    pub(crate) fn is_feedback_enabled(&self) -> bool {
        self.resolve_feedback().value
    }
    pub(crate) fn is_session_recap_enabled(&self) -> bool {
        self.resolve_session_recap().value
    }
    pub(crate) fn is_turn_summary_enabled(&self) -> bool {
        self.resolve_turn_summary().value
    }
    pub(crate) fn is_voice_mode_enabled(&self) -> bool {
        self.resolve_voice_mode().value
    }
    /// Two-pass (prefire) compaction gate. Default OFF (opt-in) — enable via
    /// remote settings `two_pass_compaction_enabled`, the `[features] two_pass_compaction`
    /// config.toml key, or `GROK_TWO_PASS_COMPACTION` env.
    pub(crate) fn is_two_pass_compaction_enabled(&self) -> bool {
        self.resolve_two_pass_compaction().value
    }
    pub(crate) fn resolve_telemetry_mode(&self) -> Resolved<TelemetryMode> {
        if let Some(mode) = self.requirements.telemetry.pinned() {
            return Resolved::new(mode, ConfigSource::Requirement);
        }
        if let Some(mode) = env_telemetry_mode("GROK_TELEMETRY_ENABLED") {
            return Resolved::new(mode, ConfigSource::Env);
        }
        if let Some(mode) = self.features.telemetry {
            return Resolved::new(mode, ConfigSource::Config);
        }
        if let Some(rs) = self.remote_settings.as_ref() {
            if let Some(mode_str) = rs.telemetry_mode.as_deref()
                && let Some(mode) = TelemetryMode::parse(mode_str)
            {
                return Resolved::new(mode, ConfigSource::Remote);
            }
            if let Some(val) = rs.telemetry_enabled {
                return Resolved::new(TelemetryMode::from(val), ConfigSource::Remote);
            }
        }
        Resolved::new(TelemetryMode::Disabled, ConfigSource::Default)
    }
    pub(crate) fn resolve_trace_upload(&self) -> Resolved<bool> {
        let mode = self.resolve_telemetry_mode();
        let ff = if mode.value.is_disabled() {
            None
        } else {
            self.remote_settings
                .as_ref()
                .and_then(|s| s.trace_upload_enabled)
        };
        BoolFlag::env("GROK_TELEMETRY_TRACE_UPLOAD")
            .requirement(self.requirements.trace_upload.pinned())
            .config(self.telemetry.trace_upload)
            .feature_flag(ff)
            .default(mode.value.is_enabled())
            .resolve()
    }
    /// Resolve jemalloc heap-profile config from stored remote settings + gates.
    pub fn resolve_jemalloc_heap_profile(
        &self,
        data_collection_disabled: bool,
    ) -> crate::heap_profile::JemallocHeapProfileConfig {
        let rs = self.remote_settings.as_ref();
        crate::heap_profile::resolve_jemalloc_heap_profile(
            rs.and_then(|s| s.jemalloc_heap_profile_enabled),
            rs.and_then(|s| s.jemalloc_heap_profile_thresholds_bytes.as_deref()),
            rs.and_then(|s| s.jemalloc_heap_profile_poll_interval_secs),
            data_collection_disabled,
            self.resolve_trace_upload().value,
            crate::heap_profile::prof_available(),
        )
    }
    /// K12 scoped resolve: fresh jemalloc fields + current gates (no remote rewrite).
    pub(crate) fn resolve_jemalloc_heap_profile_from_partial(
        &self,
        jemalloc_enabled: Option<bool>,
        jemalloc_thresholds: Option<&[u64]>,
        jemalloc_poll_interval_secs: Option<u64>,
        data_collection_disabled: bool,
    ) -> crate::heap_profile::JemallocHeapProfileConfig {
        crate::heap_profile::resolve_jemalloc_heap_profile(
            jemalloc_enabled,
            jemalloc_thresholds,
            jemalloc_poll_interval_secs,
            data_collection_disabled,
            self.resolve_trace_upload().value,
            crate::heap_profile::prof_available(),
        )
    }
    pub(crate) fn trace_upload_decision_debug(&self) -> serde_json::Value {
        let telemetry = self.resolve_telemetry_mode();
        let trace_upload = self.resolve_trace_upload();
        let req = &self.requirements.trace_upload;
        serde_json::json!({
            "trace_upload": trace_upload.value,
            "trace_upload_source": trace_upload.source.to_string(),
            "telemetry_mode": telemetry.value.to_string(),
            "telemetry_source": telemetry.source.to_string(),
            "in_requirement_pin": req.pinned(),
            "in_requirement_src": req.source().map(|s| s.to_string()),
            "in_env_trace_upload": std::env::var("GROK_TELEMETRY_TRACE_UPLOAD").ok(),
            "in_env_telemetry_enabled": std::env::var("GROK_TELEMETRY_ENABLED").ok(),
            "in_cfg_telemetry_trace_upload": self.telemetry.trace_upload,
            "in_cfg_features_telemetry": self.features.telemetry.map(|m| m.to_string()),
            "in_remote_trace_upload_enabled": self
                .remote_settings
                .as_ref()
                .and_then(|s| s.trace_upload_enabled),
            "has_remote_settings": self.remote_settings.is_some(),
        })
    }
    pub(crate) fn resolve_feedback(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.feedback_enabled);
        BoolFlag::env("GROK_FEEDBACK_ENABLED")
            .requirement(self.requirements.feedback.pinned())
            .config(self.features.feedback)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    pub(crate) fn resolve_two_pass_compaction(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.two_pass_compaction_enabled);
        BoolFlag::env("GROK_TWO_PASS_COMPACTION")
            .config(self.features.two_pass_compaction)
            .feature_flag(ff)
            .default(false)
            .resolve()
    }
    /// Server-side doom-loop check policy (the `x-grok-doom-loop-check`
    /// header, trigger parsing, and confident-signal resampling, all
    /// applied by the sampler). Merged
    /// PER-FIELD across the `[doom_loop_recovery]` TOML table and the
    /// remote settings `doom_loop_recovery` object (a partial remote object only
    /// overrides the fields it sets). Gate precedence: env
    /// `GROK_DOOM_LOOP_RECOVERY` > TOML `enabled` > remote `enabled` >
    /// default ON — each layer's `false` is an independent kill switch, and
    /// `None` IS the off state, so disabled has exactly one spelling.
    /// Tunables have no env layer (TOML > remote > default) and are clamped
    /// to their documented ranges. Returns the composite runtime policy
    /// rather than `Resolved` because each knob resolves from its own
    /// source (the `resolve_reminder_policy` pattern).
    pub(crate) fn resolve_doom_loop_recovery(
        &self,
    ) -> Option<xai_grok_sampling_types::DoomLoopRecoveryPolicy> {
        use xai_grok_sampling_types::DoomLoopRecoveryPolicy as Policy;
        let remote = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.doom_loop_recovery.as_ref());
        let enabled = BoolFlag::env("GROK_DOOM_LOOP_RECOVERY")
            .config(self.doom_loop_recovery.enabled)
            .feature_flag(remote.and_then(|s| s.enabled))
            .default(true)
            .resolve()
            .value;
        enabled.then(|| Policy {
            max_threshold: self
                .doom_loop_recovery
                .max_threshold
                .or(remote.and_then(|s| s.max_threshold))
                .map_or(Policy::DEFAULT_MAX_THRESHOLD, Policy::clamp_max_threshold),
            max_retries: self
                .doom_loop_recovery
                .max_retries
                .or(remote.and_then(|s| s.max_retries))
                .map_or(Policy::DEFAULT_MAX_RETRIES, Policy::clamp_max_retries),
            window_tokens: self
                .doom_loop_recovery
                .window_tokens
                .or(remote.and_then(|s| s.window_tokens))
                .map_or(
                    Policy::DEFAULT_RECOVERY_WINDOW_TOKENS,
                    Policy::clamp_window_tokens,
                ),
        })
    }
    /// Automatic worktree GC policy. Precedence: env kill/dry-run >
    /// `[worktree.auto_gc]` TOML > remote `worktree_auto_gc` > defaults.
    /// Platform age-expiry (non-Linux dead-only) is enforced inside
    /// `xai_fast_worktree::maybe_auto_gc`, not here.
    pub(crate) fn resolve_worktree_auto_gc(&self) -> xai_fast_worktree::ResolvedWorktreeAutoGc {
        crate::util::config::resolve_worktree_auto_gc_from_settings(
            Some(&self.worktree.auto_gc),
            self.remote_settings
                .as_ref()
                .and_then(|s| s.worktree_auto_gc.as_ref()),
        )
    }
    /// Gate first-run auto-registration of the official xAI marketplace source.
    /// Precedence: env `GROK_OFFICIAL_MARKETPLACE_AUTO_REGISTER` > remote settings >
    /// default off (so only remote settings-targeted teams get it pre-public). No
    /// managed `.requirement` pin: `marketplace_allowlist` already gates sources.
    pub(crate) fn resolve_official_marketplace_auto_register(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.official_marketplace_auto_register);
        BoolFlag::env("GROK_OFFICIAL_MARKETPLACE_AUTO_REGISTER")
            .feature_flag(ff)
            .default(false)
            .resolve()
    }
    pub(crate) fn resolve_lsp_tools(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.lsp_tools_enabled);
        BoolFlag::env("GROK_LSP_TOOLS")
            .requirement(self.requirements.lsp_tools.pinned())
            .config(self.features.lsp_tools)
            .feature_flag(ff)
            .resolve()
    }
    pub(crate) fn resolve_web_fetch(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.web_fetch_enabled);
        BoolFlag::env("GROK_WEB_FETCH")
            .requirement(self.requirements.web_fetch.pinned())
            .config(self.features.web_fetch)
            .feature_flag(ff)
            .resolve()
    }
    /// `ask_user_question` tool gate; default ON. remote settings
    /// `ask_user_question_enabled: false` (or `[features]` / env) is a remote
    /// kill-switch. The `_meta.askUserQuestion` override (`--no-ask-user`) is
    /// applied at the spawn site and outranks this resolver.
    pub(crate) fn resolve_ask_user_question(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.ask_user_question_enabled);
        BoolFlag::env("GROK_ASK_USER_QUESTION")
            .requirement(self.requirements.ask_user_question.pinned())
            .config(self.features.ask_user_question)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    /// Session recap gate (the `/recap` command + automatic return-from-away
    /// recap). Default ON — disable via remote settings `session_recap`, the
    /// `[features] session_recap` config.toml key, or `GROK_SESSION_RECAP` env.
    pub(crate) fn resolve_session_recap(&self) -> Resolved<bool> {
        let ff = self.remote_settings.as_ref().and_then(|s| s.session_recap);
        BoolFlag::env("GROK_SESSION_RECAP")
            .config(self.features.session_recap)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    /// Session search index. Default ON. Turn off with a `requirements.toml` or MDM pin, the
    /// `GROK_SESSION_SEARCH` env var, the `[features] session_search` config key, or remote
    /// settings, in that order. Only a pin outranks the environment.
    pub(crate) fn resolve_session_search(&self) -> Resolved<bool> {
        let ff = self.remote_settings.as_ref().and_then(|s| s.session_search);
        BoolFlag::env("GROK_SESSION_SEARCH")
            .requirement(self.requirements.session_search.pinned())
            .config(self.features.session_search)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    /// Per-turn dashboard summary gate. Default ON — disable via remote
    /// settings `turn_summary`, the `[features] turn_summary` config.toml key,
    /// or `GROK_TURN_SUMMARY` env.
    pub(crate) fn resolve_turn_summary(&self) -> Resolved<bool> {
        let ff = self.remote_settings.as_ref().and_then(|s| s.turn_summary);
        BoolFlag::env("GROK_TURN_SUMMARY")
            .config(self.features.turn_summary)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    /// Voice dictation gate. Default on.
    ///
    /// Precedence: requirements > `GROK_VOICE_MODE` > config/managed
    /// `[features] voice_mode` > remote `voice_mode_enabled` > default true.
    /// The pager may force API-key sessions on when only remote is off.
    pub(crate) fn resolve_voice_mode(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.voice_mode_enabled);
        BoolFlag::env("GROK_VOICE_MODE")
            .requirement(self.requirements.voice_mode.pinned())
            .config(self.features.voice_mode)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    /// `image_gen` (+ `/imagine`). Default on.
    ///
    /// `imagine_tools_disabled` is a remote force-off (env/config cannot
    /// re-enable). Otherwise: requirement > env > `[features]` > remote >
    /// default.
    pub(crate) fn resolve_image_gen(&self) -> Resolved<bool> {
        use xai_grok_tools::implementations::grok_build::IMAGE_GEN_TOOL_NAME;
        if let Some(pinned) = self.requirements.image_gen.pinned() {
            return Resolved::new(pinned, ConfigSource::Requirement);
        }
        if self
            .remote_settings
            .as_ref()
            .is_some_and(|s| s.imagine_tool_disabled(IMAGE_GEN_TOOL_NAME))
        {
            return Resolved::new(false, ConfigSource::Remote);
        }
        BoolFlag::env("GROK_IMAGE_GEN")
            .config(self.features.image_gen)
            .feature_flag(
                self.remote_settings
                    .as_ref()
                    .and_then(|s| s.image_gen_enabled),
            )
            .default(true)
            .resolve()
    }
    /// `image_edit` tool gate. Same denylist / requirement pattern as
    /// [`Self::resolve_image_gen`]; no `[features]` key (defaults on).
    pub(crate) fn resolve_image_edit(&self) -> Resolved<bool> {
        use xai_grok_tools::implementations::grok_build::IMAGE_EDIT_TOOL_NAME;
        if let Some(pinned) = self.requirements.image_edit.pinned() {
            return Resolved::new(pinned, ConfigSource::Requirement);
        }
        if self
            .remote_settings
            .as_ref()
            .is_some_and(|s| s.imagine_tool_disabled(IMAGE_EDIT_TOOL_NAME))
        {
            return Resolved::new(false, ConfigSource::Remote);
        }
        BoolFlag::env("GROK_IMAGE_EDIT").default(true).resolve()
    }
    /// `image_to_video` / `reference_to_video` (+ `/imagine-video`). Default on.
    ///
    /// Registered as a pair; denylisting either tool name (or `video_gen`)
    /// disables both. Otherwise same precedence as [`Self::resolve_image_gen`].
    pub(crate) fn resolve_video_gen(&self) -> Resolved<bool> {
        use xai_grok_tools::implementations::grok_build::{
            IMAGE_TO_VIDEO_TOOL_NAME, REFERENCE_TO_VIDEO_TOOL_NAME,
        };
        if let Some(pinned) = self.requirements.video_gen.pinned() {
            return Resolved::new(pinned, ConfigSource::Requirement);
        }
        if self.remote_settings.as_ref().is_some_and(|s| {
            s.imagine_tool_disabled(IMAGE_TO_VIDEO_TOOL_NAME)
                || s.imagine_tool_disabled(REFERENCE_TO_VIDEO_TOOL_NAME)
                || s.imagine_tool_disabled("video_gen")
        }) {
            return Resolved::new(false, ConfigSource::Remote);
        }
        BoolFlag::env("GROK_VIDEO_GEN")
            .config(self.features.video_gen)
            .feature_flag(
                self.remote_settings
                    .as_ref()
                    .and_then(|s| s.video_gen_enabled),
            )
            .default(true)
            .resolve()
    }
    /// Optional Imagine model override for `image_gen`. When set (non-empty),
    /// `image_gen` calls this model slug instead of the default quality model.
    /// Precedence: env `GROK_IMAGE_GEN_MODEL_OVERRIDE` > `[features]
    /// image_gen_model_override` config > remote settings `image_gen_model_override`.
    /// `None` → default model (`grok-imagine-image-quality`).
    pub(crate) fn resolve_image_gen_model_override(&self) -> Option<String> {
        resolve_string_flag(
            None,
            "GROK_IMAGE_GEN_MODEL_OVERRIDE",
            self.features.image_gen_model_override.as_deref(),
            self.remote_settings
                .as_ref()
                .and_then(|s| s.image_gen_model_override.as_deref()),
        )
        .map(|r| r.value)
    }
    pub(crate) fn resolve_image_edit_model_override(&self) -> Option<String> {
        resolve_string_flag(
            None,
            "GROK_IMAGE_EDIT_MODEL_OVERRIDE",
            self.features.image_edit_model_override.as_deref(),
            self.remote_settings
                .as_ref()
                .and_then(|s| s.image_edit_model_override.as_deref()),
        )
        .map(|r| r.value)
    }
    /// Goal mode (`/goal`) master switch. Default ON: deployments that can't
    /// reach cli-chat-proxy `/v1/settings` (custom `models_base_url`, external
    /// `auth_provider_command`, air-gapped proxies) never receive the
    /// remote settings `goal_enabled` flag, so the default must not carve them out.
    pub(crate) fn resolve_goal(&self) -> Resolved<bool> {
        let ff = self.remote_settings.as_ref().and_then(|s| s.goal_enabled);
        if ff == Some(false) {
            return Resolved::new(false, ConfigSource::Remote);
        }
        BoolFlag::env("GROK_GOAL")
            .config(self.goal.enabled)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    /// Background workflows (`workflow` tool, `.grok/workflows/*.rhai`,
    /// `/deep-research`, host-owned `/goal` driver). Default ON: deployments
    /// that never receive remote settings still get workflows; `Some(false)`
    /// remote / config / env remains a kill-switch.
    pub(crate) fn resolve_workflows(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.workflows_enabled);
        if ff == Some(false) {
            return Resolved::new(false, ConfigSource::Remote);
        }
        BoolFlag::env("GROK_WORKFLOWS")
            .config(self.workflows.enabled)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    /// Classifier, planner, and summary all default to goal mode itself: when
    /// `/goal` is on they are on unless config/env/remote says otherwise.
    /// `goal_enabled` is the session's already-resolved master switch (the same
    /// value the actor stores), passed in so a sub-role default can never
    /// disagree with whether `/goal` is on.
    pub(crate) fn resolve_goal_classifier_enabled(&self, goal_enabled: bool) -> Resolved<bool> {
        BoolFlag::env("GROK_GOAL_CLASSIFIER")
            .config(self.goal.classifier_enabled)
            .feature_flag(
                self.remote_settings
                    .as_ref()
                    .and_then(|s| s.goal_classifier_enabled),
            )
            .default(goal_enabled)
            .resolve()
    }
    pub(crate) fn resolve_goal_planner_enabled(&self, goal_enabled: bool) -> Resolved<bool> {
        BoolFlag::env("GROK_GOAL_PLANNER")
            .config(self.goal.planner_enabled)
            .feature_flag(
                self.remote_settings
                    .as_ref()
                    .and_then(|s| s.goal_planner_enabled),
            )
            .default(goal_enabled)
            .resolve()
    }
    pub(crate) fn resolve_goal_summary_enabled(&self, goal_enabled: bool) -> Resolved<bool> {
        BoolFlag::env("GROK_GOAL_SUMMARY")
            .config(self.goal.summary_enabled)
            .feature_flag(
                self.remote_settings
                    .as_ref()
                    .and_then(|s| s.goal_summary_enabled),
            )
            .default(goal_enabled)
            .resolve()
    }
    /// Goal count resolver: env(parse) > config > remote > default, then clamp.
    /// An unparseable env value falls through to the next source.
    fn resolve_goal_u32(
        env_var: &str,
        config: Option<u32>,
        remote: Option<u32>,
        default: u32,
        clamp: impl Fn(u32) -> u32,
    ) -> Resolved<u32> {
        if let Some(env_value) = env_string(env_var)
            && let Ok(parsed) = env_value.parse::<u32>()
        {
            return Resolved::new(clamp(parsed), ConfigSource::Env);
        }
        if let Some(v) = config {
            return Resolved::new(clamp(v), ConfigSource::Config);
        }
        if let Some(v) = remote {
            return Resolved::new(clamp(v), ConfigSource::Remote);
        }
        Resolved::new(default, ConfigSource::Default)
    }
    /// Per-attempt adversarial-skeptic count, clamped to
    /// `[GOAL_VERIFIER_SKEPTIC_MIN, GOAL_VERIFIER_SKEPTIC_MAX]`.
    pub(crate) fn resolve_goal_verifier_count(&self) -> Resolved<u32> {
        use crate::session::goal_classifier::{
            GOAL_VERIFIER_SKEPTIC_COUNT, GOAL_VERIFIER_SKEPTIC_MAX, GOAL_VERIFIER_SKEPTIC_MIN,
        };
        Self::resolve_goal_u32(
            "GROK_GOAL_VERIFIER_N",
            self.goal.verifier_count,
            self.remote_settings
                .as_ref()
                .and_then(|s| s.goal_verifier_count),
            GOAL_VERIFIER_SKEPTIC_COUNT,
            |v| v.clamp(GOAL_VERIFIER_SKEPTIC_MIN, GOAL_VERIFIER_SKEPTIC_MAX),
        )
    }
    /// Per-goal classifier run cap, floored at `GOAL_CLASSIFIER_MAX_RUNS_MIN`
    /// with no upper ceiling.
    pub(crate) fn resolve_goal_classifier_max_runs(&self) -> Resolved<u32> {
        use crate::session::goal_classifier::{
            GOAL_CLASSIFIER_MAX_RUNS_DEFAULT, GOAL_CLASSIFIER_MAX_RUNS_MIN,
        };
        Self::resolve_goal_u32(
            "GROK_GOAL_CLASSIFIER_MAX",
            self.goal.classifier_max_runs,
            self.remote_settings
                .as_ref()
                .and_then(|s| s.goal_classifier_max_runs),
            GOAL_CLASSIFIER_MAX_RUNS_DEFAULT,
            |v| v.max(GOAL_CLASSIFIER_MAX_RUNS_MIN),
        )
    }
    /// Stall-triggered strategist cadence N (fires every N consecutive
    /// `NotAchieved`). Default tracks the resolved classifier cap
    /// (`max(1, cap / 2)`); floored at 1 so it can never silently disable.
    pub(crate) fn resolve_goal_strategist_every(&self, classifier_max_runs: u32) -> Resolved<u32> {
        Self::resolve_goal_u32(
            "GROK_GOAL_STRATEGIST_EVERY",
            self.goal.strategist_every,
            self.remote_settings
                .as_ref()
                .and_then(|s| s.goal_strategist_every),
            (classifier_max_runs / 2).max(1),
            |v| v.max(1),
        )
    }
    /// Re-verify escalation threshold; floored at 1. No remote layer.
    pub(crate) fn resolve_goal_reverify_after(&self) -> Resolved<u32> {
        Self::resolve_goal_u32(
            "GROK_GOAL_REVERIFY_AFTER",
            self.goal.reverify_after,
            None,
            crate::session::acp_session::GOAL_REVERIFY_AFTER_DEFAULT,
            |v| v.max(1),
        )
    }
    /// When `true`, every `/goal` role inherits the current model regardless of
    /// configured pairs.
    pub(crate) fn resolve_goal_use_current_model_only(&self) -> Resolved<bool> {
        BoolFlag::env("GROK_GOAL_USE_CURRENT_MODEL_ONLY")
            .config(self.goal.use_current_model_only)
            .default(false)
            .resolve()
    }
    /// Shared single-pair resolution. Precedence: kill-switch ⇒
    /// `InheritCurrent`/`Config` > `config_pair` ⇒ `Explicit`/`Config` >
    /// `remote_pair` ⇒ `Explicit`/`Remote` > `InheritCurrent`/`Default`. The
    /// chosen pair is cloned only on its branch.
    fn resolve_single_role_model(
        use_current_only: bool,
        config_pair: Option<&crate::util::config::GoalRoleModel>,
        remote_pair: Option<&crate::util::config::GoalRoleModel>,
    ) -> Resolved<GoalRoleModelChoice> {
        if use_current_only {
            return Resolved::new(GoalRoleModelChoice::InheritCurrent, ConfigSource::Config);
        }
        if let Some(pair) = config_pair {
            return Resolved::new(
                GoalRoleModelChoice::Explicit(pair.clone()),
                ConfigSource::Config,
            );
        }
        match remote_pair {
            Some(pair) => Resolved::new(
                GoalRoleModelChoice::Explicit(pair.clone()),
                ConfigSource::Remote,
            ),
            None => Resolved::new(GoalRoleModelChoice::InheritCurrent, ConfigSource::Default),
        }
    }
    /// Planner role model: `[goal]` config then remote. No env layer (only the
    /// kill-switch reads env).
    ///
    /// An `Explicit` pair is applied as `runtime_overrides.model`, resolved before
    /// `resolve_subagent_sampling_config`, so it wins over a user
    /// `[subagents.models]` pin; `InheritCurrent` hands precedence back to that pin.
    pub(crate) fn resolve_goal_planner_model(
        &self,
        use_current_only: bool,
    ) -> Resolved<GoalRoleModelChoice> {
        Self::resolve_single_role_model(
            use_current_only,
            self.goal.planner_model.as_ref(),
            self.remote_settings
                .as_ref()
                .and_then(|s| s.goal_planner_model.as_ref()),
        )
    }
    /// Strategist role model; same precedence as [`Self::resolve_goal_planner_model`].
    pub(crate) fn resolve_goal_strategist_model(
        &self,
        use_current_only: bool,
    ) -> Resolved<GoalRoleModelChoice> {
        Self::resolve_single_role_model(
            use_current_only,
            self.goal.strategist_model.as_ref(),
            self.remote_settings
                .as_ref()
                .and_then(|s| s.goal_strategist_model.as_ref()),
        )
    }
    /// Skeptic pool; same precedence as [`Self::resolve_goal_planner_model`] but
    /// over a pool. Pool order is preserved for the round-robin expansion in
    /// `expand_skeptic_assignment`.
    pub(crate) fn resolve_goal_skeptic_models(
        &self,
        use_current_only: bool,
    ) -> Resolved<Vec<GoalRoleModelChoice>> {
        if use_current_only {
            return Resolved::new(Vec::new(), ConfigSource::Config);
        }
        let to_choices = |pool: &[crate::util::config::GoalRoleModel]| {
            pool.iter()
                .cloned()
                .map(GoalRoleModelChoice::Explicit)
                .collect::<Vec<_>>()
        };
        if !self.goal.skeptic_models.is_empty() {
            return Resolved::new(to_choices(&self.goal.skeptic_models), ConfigSource::Config);
        }
        match self
            .remote_settings
            .as_ref()
            .map(|s| s.goal_skeptic_models.as_slice())
        {
            Some(pool) if !pool.is_empty() => Resolved::new(to_choices(pool), ConfigSource::Remote),
            _ => Resolved::new(Vec::new(), ConfigSource::Default),
        }
    }
    pub(crate) fn resolve_write_file(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.write_file_enabled);
        BoolFlag::env("GROK_WRITE_FILE")
            .requirement(self.requirements.write_file.pinned())
            .config(self.features.write_file)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    pub(crate) fn resolve_backend_tools(&self) -> Resolved<bool> {
        BoolFlag::env("GROK_BACKEND_SEARCH")
            .config(self.features.backend_tools)
            .default(true)
            .resolve()
    }
    /// Resolve the mode (env `GROK_COMPACTION_MODE` > config > remote settings >
    /// default, unrecognized falling through) and, for `Segments`, attach the
    /// separately-resolved detail level.
    pub(crate) fn resolve_compaction_mode(&self) -> xai_chat_state::CompactionMode {
        resolve_compaction_mode_from(
            env_string("GROK_COMPACTION_MODE").as_deref(),
            self.features.compaction_mode.as_deref(),
            self.remote_settings
                .as_ref()
                .and_then(|r| r.compaction_mode.as_deref()),
        )
        .with_segment_detail(self.resolve_compaction_detail())
    }
    /// Resolve verbatim-input flag: env `GROK_COMPACTION_VERBATIM_INPUT` > config > remote settings > default `true`.
    pub(crate) fn resolve_compaction_verbatim_input(&self) -> bool {
        BoolFlag::env("GROK_COMPACTION_VERBATIM_INPUT")
            .config(self.features.compaction_verbatim_input)
            .feature_flag(
                self.remote_settings
                    .as_ref()
                    .and_then(|r| r.compaction_verbatim_input),
            )
            .default(true)
            .resolve()
            .value
    }
    pub(crate) fn resolve_compaction_tool_choice(
        &self,
    ) -> crate::util::config::CompactionToolChoice {
        crate::util::config::resolve_compaction_tool_choice_from(
            env_string(crate::util::config::ENV_COMPACTION_TOOL_CHOICE).as_deref(),
            self.features.compaction_tool_choice.as_deref(),
            self.remote_settings
                .as_ref()
                .and_then(|r| r.compaction_tool_choice.as_deref()),
        )
    }
    /// Precedence: env `GROK_COMPACTION_DETAIL`, then config
    /// `features.compaction_detail`, then remote settings
    /// `remote_settings.compaction_detail`, then default (`verbose`). Drives the
    /// `segments` verbatim detail level.
    fn resolve_compaction_detail(&self) -> xai_chat_state::CompactionDetail {
        resolve_compaction_detail_from(
            env_string("GROK_COMPACTION_DETAIL").as_deref(),
            self.features.compaction_detail.as_deref(),
            self.remote_settings
                .as_ref()
                .and_then(|r| r.compaction_detail.as_deref()),
        )
    }
    pub(crate) fn resolve_cancel_rewind(&self) -> Resolved<bool> {
        let ff = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.cancel_rewind_enabled);
        BoolFlag::env("GROK_CANCEL_REWIND")
            .config(self.features.cancel_rewind)
            .feature_flag(ff)
            .default(true)
            .resolve()
    }
    /// Resolve whether to use grok's default OAuth2 (xAI auth.x.ai).
    ///
    /// Enterprise OIDC (`oidc` in config.toml) always wins — this only gates
    /// the default xAI OAuth2 fallback when no enterprise OIDC is configured.
    ///
    /// Priority: `--oauth` > GROK_OAUTH_ENABLED env > default (true = OAuth).
    pub(crate) fn resolve_grok_oauth(&self, cli_oidc: Option<bool>) -> Resolved<bool> {
        BoolFlag::env("GROK_OAUTH_ENABLED")
            .cli(cli_oidc)
            .default(true)
            .resolve()
    }
}
/// Canonical resolver for `mcp.liveness_watchers`. Stacks the full
/// 7-step `BoolFlag` precedence:
///
/// `requirement > cli > env (GROK_MCP_LIVENESS_WATCHERS) > config >
/// managed > feature_flag > default (true)`.
///
/// `util::config::resolve_mcp_liveness_watchers` delegates here so the
/// precedence is single-sourced.
///
/// The default is `true` — it gates the watcher + dispatcher
/// default-on, with this flag existing primarily as a kill switch
/// during the rollout.
pub(crate) fn resolve_mcp_liveness_watchers(
    requirement: Option<bool>,
    cli: Option<bool>,
    config: Option<bool>,
    managed: Option<bool>,
    feature_flag: Option<bool>,
) -> Resolved<bool> {
    BoolFlag::env("GROK_MCP_LIVENESS_WATCHERS")
        .requirement(requirement)
        .cli(cli)
        .config(config)
        .managed(managed)
        .feature_flag(feature_flag)
        .default(true)
        .resolve()
}
/// Canonical resolver for `mcp.auto_restart`. Stacks the full 7-step
/// `BoolFlag` precedence:
///
/// `requirement > cli > env (GROK_MCP_AUTO_RESTART) > config >
/// managed > feature_flag > default (true)`.
///
/// Mirrors [`resolve_mcp_liveness_watchers`]. Both
/// `util::config::resolve_mcp_auto_restart` delegates here so the
/// precedence is single-sourced.
///
/// Recovery is on by default; opt out via `GROK_MCP_AUTO_RESTART=false`,
/// `[features] mcp_auto_restart`, or `requirements.toml`.
pub(crate) fn resolve_mcp_auto_restart(
    requirement: Option<bool>,
    cli: Option<bool>,
    config: Option<bool>,
    managed: Option<bool>,
    feature_flag: Option<bool>,
) -> Resolved<bool> {
    BoolFlag::env("GROK_MCP_AUTO_RESTART")
        .requirement(requirement)
        .cli(cli)
        .config(config)
        .managed(managed)
        .feature_flag(feature_flag)
        .default(true)
        .resolve()
}
/// Canonical resolver for `mcp.push_server_status`. Stacks the same
/// 7-step `BoolFlag` precedence as
/// [`resolve_mcp_liveness_watchers`]:
///
/// `requirement > cli > env (GROK_MCP_PUSH_SERVER_STATUS) > config >
/// managed > feature_flag > default (true)`.
///
/// `util::config::resolve_mcp_push_server_status` delegates here so
/// the precedence is single-sourced.
///
/// The default is `true` — the pager's subscription to
/// `x.ai/mcp/server_status` is wired default-on, with this
/// flag existing primarily as a kill switch.
pub fn resolve_mcp_push_server_status(
    requirement: Option<bool>,
    cli: Option<bool>,
    config: Option<bool>,
    managed: Option<bool>,
    feature_flag: Option<bool>,
) -> Resolved<bool> {
    BoolFlag::env("GROK_MCP_PUSH_SERVER_STATUS")
        .requirement(requirement)
        .cli(cli)
        .config(config)
        .managed(managed)
        .feature_flag(feature_flag)
        .default(true)
        .resolve()
}
/// Canonical resolver for `mcp.recursive_config_watch`. Stacks the
/// same 7-step `BoolFlag` precedence as
/// [`resolve_mcp_liveness_watchers`]:
///
/// `requirement > cli > env (GROK_MCP_RECURSIVE_CONFIG_WATCH) >
/// config > managed > feature_flag > default (true)`.
///
/// `util::config::resolve_mcp_recursive_config_watch` delegates here
/// so the precedence is single-sourced.
///
/// The default is `true`. It enables the two narrow
/// non-recursive cwd watches default-on. The flag exists primarily
/// as a kill switch during the rollout: if the FSEvents flakiness
/// on macOS or an inotify-quota issue on Linux causes a regression,
/// operators flip this flag (e.g. via `GROK_MCP_RECURSIVE_CONFIG_
/// WATCH=0`) and the leader falls back to the prior behavior (no cwd
/// watches; user-triggered refresh is the only project-config
/// reload path).
///
/// Note the **name is a slight misnomer**: the watches themselves
/// are non-recursive (by design, to avoid blowing through
/// `fs.inotify.max_user_watches` on large repos). The flag name
/// follows the rollout-gate naming convention.
pub(crate) fn resolve_mcp_recursive_config_watch(
    requirement: Option<bool>,
    cli: Option<bool>,
    config: Option<bool>,
    managed: Option<bool>,
    feature_flag: Option<bool>,
) -> Resolved<bool> {
    BoolFlag::env("GROK_MCP_RECURSIVE_CONFIG_WATCH")
        .requirement(requirement)
        .cli(cli)
        .config(config)
        .managed(managed)
        .feature_flag(feature_flag)
        .default(true)
        .resolve()
}
/// Sync analogue of [`BoolFlag`] for callers that run before the tokio
/// runtime (e.g. `init_sentry`). Loads from disk + env directly rather than
/// from a pre-built `Config`.
///
/// Same convention as [`BoolFlag`]: `resolve()` returns the *enabled* value.
/// `disable_env` is sugar for "force-off if this env is truthy" and does not
/// invert the convention.
///
/// Layer precedence:
/// 1. `requirements.toml`              (admin pin)
/// 2. `managed_settings.json` env      (Claude admin pin, force-off)
/// 3. process env via `disable_env`    (force-off)
/// 4. process env via `enable_env`     (either direction)
/// 5. merged config                    (user/managed defaults)
/// 6. `inherit`, then `default`
pub(crate) struct SyncBoolFlag {
    extract_toml: fn(&toml::Value) -> Option<bool>,
    disable_env: Option<&'static str>,
    enable_env: Option<fn() -> Option<bool>>,
    inherit: Option<fn() -> bool>,
    default: bool,
}
impl SyncBoolFlag {
    pub(crate) const fn new(extract_toml: fn(&toml::Value) -> Option<bool>) -> Self {
        Self {
            extract_toml,
            disable_env: None,
            enable_env: None,
            inherit: None,
            default: false,
        }
    }
    /// Force-off env name (e.g. `"DISABLE_TELEMETRY"`). Truthy at this name
    /// in `managed_settings.json` or process env disables the flag.
    pub(crate) const fn disable_env(mut self, name: &'static str) -> Self {
        self.disable_env = Some(name);
        self
    }
    /// Either-direction env resolver (typically `GROK_*`). Returns
    /// `Some(enabled)` for an explicit signal, `None` to fall through.
    pub(crate) const fn enable_env(mut self, resolver: fn() -> Option<bool>) -> Self {
        self.enable_env = Some(resolver);
        self
    }
    /// Fallback when no source above fires.
    pub(crate) const fn inherit(mut self, resolver: fn() -> bool) -> Self {
        self.inherit = Some(resolver);
        self
    }
    pub(crate) const fn default(mut self, val: bool) -> Self {
        self.default = val;
        self
    }
    pub(crate) fn resolve(&self) -> bool {
        if let Some(enabled) = read_requirements_toml()
            .as_ref()
            .and_then(|r| (self.extract_toml)(r))
        {
            return enabled;
        }
        if let Some(name) = self.disable_env
            && managed_settings_env_flag(name) == Some(true)
        {
            return false;
        }
        if let Some(name) = self.disable_env
            && env_bool(name) == Some(true)
        {
            return false;
        }
        if let Some(resolver) = self.enable_env
            && let Some(enabled) = resolver()
        {
            return enabled;
        }
        if let Some(enabled) = crate::config::load_effective_config()
            .ok()
            .as_ref()
            .and_then(|r| (self.extract_toml)(r))
        {
            return enabled;
        }
        self.inherit.map_or(self.default, |f| f())
    }
}
/// Sync slice of [`Config::resolve_telemetry_mode`] for use before the tokio
/// runtime (e.g. `init_sentry`). `true` only when explicitly off.
pub(crate) fn is_telemetry_disabled_sync() -> bool {
    !SyncBoolFlag::new(telemetry_enabled_from_toml)
        .disable_env("DISABLE_TELEMETRY")
        .enable_env(grok_telemetry_env_enabled)
        .resolve()
}
/// Like [`is_telemetry_disabled_sync`] but only `true` when telemetry is
/// *explicitly* off; absence is not disabled (`.default(true)`) so remote-only
/// enablement still builds the OTLP exporter (the runtime gate then governs it).
pub(crate) fn is_telemetry_explicitly_disabled_sync() -> bool {
    !SyncBoolFlag::new(telemetry_enabled_from_toml)
        .disable_env("DISABLE_TELEMETRY")
        .enable_env(grok_telemetry_env_enabled)
        .default(true)
        .resolve()
}
/// Sync sibling of [`is_telemetry_disabled_sync`] scoped to Sentry. Inherits
/// from telemetry when no Sentry-specific signal is set.
pub fn is_error_reporting_disabled_sync() -> bool {
    !SyncBoolFlag::new(error_reporting_enabled_from_toml)
        .disable_env("DISABLE_ERROR_REPORTING")
        .enable_env(|| env_bool("GROK_ERROR_REPORTING"))
        .inherit(|| !is_telemetry_disabled_sync())
        .resolve()
}
/// `[features] telemetry` as enabled bool. SessionMetrics counts as enabled
/// — see ERROR_REPORTING_PLAN.md. `None` for absent or unparseable.
fn telemetry_enabled_from_toml(root: &toml::Value) -> Option<bool> {
    match root.get("features")?.as_table()?.get("telemetry")? {
        toml::Value::Boolean(b) => Some(*b),
        toml::Value::String(s) => TelemetryMode::parse(s).map(|m| !m.is_disabled()),
        _ => None,
    }
}
/// `[diagnostics] error_reporting` as enabled bool. Bool-only; no
/// `session_metrics` equivalent. `None` falls through to inheritance.
fn error_reporting_enabled_from_toml(root: &toml::Value) -> Option<bool> {
    root.get("diagnostics")?
        .as_table()?
        .get("error_reporting")?
        .as_bool()
}
/// `GROK_TELEMETRY_ENABLED` resolved through `TelemetryMode::parse` so the
/// extended string forms (e.g. `"session_metrics"`) are accepted.
fn grok_telemetry_env_enabled() -> Option<bool> {
    env_telemetry_mode("GROK_TELEMETRY_ENABLED").map(|m| !m.is_disabled())
}
/// Load `~/.grok/requirements.toml` standalone so the admin pin can beat
/// env vars. The merged config layer can't express that — last-merge-wins
/// loses provenance.
pub(crate) fn read_requirements_toml() -> Option<toml::Value> {
    let path = crate::util::grok_home::grok_home().join("requirements.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}
/// Resolve the external-OTEL master switch exactly the way the external
/// stream's activation does: **requirement pin > `GROK_EXTERNAL_OTEL` env >
/// `[telemetry].otel_enabled` config layer (managed config included) > off**.
///
/// The internal trace pipeline keys its "ignore `OTEL_EXPORTER_OTLP_*`"
/// behavior off this value ([`EndpointsConfig::external_otel_master_switch`]),
/// so an org enable distributed via managed config / requirements (no env
/// var) flips **both** sides together. A desync here would leave the
/// internally-authed firehose honoring legacy `OTEL_*` repointing while
/// `internal_pipeline_consumed_otel_vars` simultaneously blocks the external
/// stream — exactly the split this design forbids.
pub(crate) fn external_otel_master_switch_resolved() -> bool {
    external_otel_master_switch_from(
        xai_grok_config::load_merged_requirements().as_ref(),
        env_bool("GROK_EXTERNAL_OTEL"),
        crate::config::load_effective_config().ok().as_ref(),
    )
}
/// Testable core of [`external_otel_master_switch_resolved`].
pub(crate) fn external_otel_master_switch_from(
    requirements: Option<&toml::Value>,
    env_switch: Option<bool>,
    effective_config: Option<&toml::Value>,
) -> bool {
    let table_enabled = |v: Option<&toml::Value>| -> Option<bool> {
        v?.get("telemetry")?.get("otel_enabled")?.as_bool()
    };
    if let Some(pinned) = table_enabled(requirements) {
        return pinned;
    }
    if let Some(env) = env_switch {
        return env;
    }
    table_enabled(effective_config).unwrap_or(false)
}
/// Resolve the external OTEL stream configuration at process startup
/// (env + local config only — remote settings are not yet available when
/// tracing init runs).
///
/// Layering follows `resolve_telemetry_mode`: **requirement > env > config >
/// remote > default**, where the `[telemetry]` `otel_*` keys from the
/// effective config (which already includes managed-config layers distributed
/// by `grok setup`) sit under the env vars, requirements pins are applied on
/// top, and the remote layer is restrictive-only + asynchronous
/// ([`apply_external_otel_remote_policy`]).
pub fn resolve_external_otel_config(
    client: xai_grok_telemetry::external::config::ExternalClientInfo,
) -> Option<xai_grok_telemetry::external::ExternalOtelConfig> {
    resolve_external_otel_config_with(
        crate::config::load_effective_config().ok().as_ref(),
        xai_grok_config::load_merged_requirements().as_ref(),
        |name| std::env::var(name).ok(),
        client,
        EndpointsConfig::default().internal_otlp_consumed_standard_vars(),
    )
}
/// Testable core of [`resolve_external_otel_config`]: all inputs injected so
/// tests don't race on process env / disk.
pub(crate) fn resolve_external_otel_config_with(
    effective_config: Option<&toml::Value>,
    requirements: Option<&toml::Value>,
    getenv: impl Fn(&str) -> Option<String>,
    client: xai_grok_telemetry::external::config::ExternalClientInfo,
    internal_pipeline_consumed_otel_vars: bool,
) -> Option<xai_grok_telemetry::external::ExternalOtelConfig> {
    let file_cfg: Option<xai_grok_telemetry::external::ExternalOtelFileConfig> = effective_config
        .and_then(|cfg| cfg.get("telemetry"))
        .map(|t| xai_grok_telemetry::external::ExternalOtelFileConfig {
            enabled: t.get("otel_enabled").and_then(toml::Value::as_bool),
            metrics_exporter: t
                .get("otel_metrics_exporter")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            logs_exporter: t
                .get("otel_logs_exporter")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            endpoint: t
                .get("otel_endpoint")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            protocol: t
                .get("otel_protocol")
                .or_else(|| t.get("otel_transport"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            log_user_prompts: t
                .get("otel_log_user_prompts")
                .and_then(toml::Value::as_bool),
            log_tool_details: t
                .get("otel_log_tool_details")
                .and_then(toml::Value::as_bool),
        });
    let req_get =
        |key: &str| -> Option<bool> { requirements?.get("telemetry")?.get(key)?.as_bool() };
    let req_enabled = req_get("otel_enabled");
    let req_prompts = req_get("otel_log_user_prompts");
    let req_details = req_get("otel_log_tool_details");
    let getenv_pinned = |name: &str| -> Option<String> {
        let pin = match name {
            xai_grok_telemetry::external::config::ENV_MASTER_SWITCH => req_enabled,
            "OTEL_LOG_USER_PROMPTS" => req_prompts,
            "OTEL_LOG_TOOL_DETAILS" => req_details,
            _ => None,
        };
        if let Some(v) = pin {
            return Some(if v { "1" } else { "0" }.to_owned());
        }
        getenv(name)
    };
    let mut resolved = xai_grok_telemetry::external::ExternalOtelConfig::resolve_with(
        getenv_pinned,
        file_cfg.as_ref(),
    )?;
    resolved.client = client;
    resolved.internal_pipeline_consumed_otel_vars = internal_pipeline_consumed_otel_vars;
    Some(resolved)
}
/// Apply the restrictive-only remote-settings policy for the external OTEL
/// stream (fleet kill switch + content-gate lock). Tighten-only by
/// construction — there is no remote enable direction — so it is safe to
/// call on every settings refresh.
pub(crate) fn apply_external_otel_remote_policy(
    settings: Option<&crate::util::config::RemoteSettings>,
) {
    let Some(settings) = settings else { return };
    let policy = xai_grok_telemetry::external::ExternalOtelRemotePolicy {
        force_disable: settings.external_otel_disabled.unwrap_or(false),
        lock_content_gates: settings.external_otel_content_gates_locked.unwrap_or(false),
    };
    if policy.force_disable || policy.lock_content_gates {
        xai_grok_telemetry::external::apply_remote_policy(policy);
    }
}
/// Seed free-function remote caches after writing `Config.remote_settings`.
///
/// Called from `init.rs` at boot and from the agent when backgrounded settings
/// arrive later, so every side effect here must be idempotent and safe to
/// re-apply. The emission-gate flip is owned by
/// [`crate::agent::otel_gate::OtelGate`], not here.
///
/// The `force_disable` write here is `Relaxed`; the synchronizing publish is
/// `OtelGate::apply_and_open`, which applies the same tighten-only policy and then
/// opens the gate with a `Release` swap. Removing that second application to
/// deduplicate would leave only the `Relaxed` store and reopen an ARM
/// visibility hole.
pub fn apply_remote_settings_side_effects(settings: Option<&crate::util::config::RemoteSettings>) {
    if let Some(s) = settings {
        let origin_trusted = crate::util::is_prod_cli_chat_proxy_url(
            &EndpointsConfig::from_effective_config().proxy_url(),
        );
        xai_grok_config::signed_policy::apply_remote_managed_config_signature_verification(
            s.managed_config_signature_verification,
            origin_trusted,
        );
    }
    crate::util::config::cache_remote_mcp_startup_timeout_secs(
        settings.and_then(|s| s.mcp_startup_timeout_secs),
    );
    crate::util::config::cache_remote_max_mcp_output_bytes(
        settings.and_then(|s| s.max_mcp_output_bytes),
    );
    crate::util::config::cache_remote_auto_mode(settings.and_then(|s| s.auto_mode.clone()));
    crate::util::config::cache_remote_remember_tool_approvals(
        settings.and_then(|s| s.remember_tool_approvals),
    );
    crate::util::config::cache_remote_crash_handler_enabled(
        settings.and_then(|s| s.crash_handler_enabled),
    );
    apply_external_otel_remote_policy(settings);
    let image_normalize_cache_enabled = settings
        .and_then(|r| r.image_normalize_cache_enabled)
        .unwrap_or(false);
    crate::session::normalize_cache::NormalizeCache::global()
        .set_enabled(image_normalize_cache_enabled);
}
/// Read `env.<key>` from Claude-compat `managed_settings.json`. `Some(true)`
/// indicates a force-off signal from a Mac-MDM-style admin policy.
fn managed_settings_env_flag(key: &str) -> Option<bool> {
    let path = xai_grok_config::claude_managed_settings_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    xai_grok_workspace::permission::resolution::json_env_flag(json.get("env"), key)
}
/// Assemble the final model map. Priority (highest wins):
/// config.toml `[model.*]` > prefetched (remote) > hardcoded defaults.
pub(crate) fn resolve_model_list(
    cfg: &Config,
    prefetched: Option<IndexMap<String, ModelEntry>>,
) -> IndexMap<String, ModelEntry> {
    let mut resolved: IndexMap<String, ModelEntry> = IndexMap::new();
    if cfg.endpoints.has_custom_endpoint() {
        tracing::info!(
            models_base_url = ?cfg.endpoints.models_base_url,
            models_list_url = ?cfg.endpoints.models_list_url,
            "custom models endpoint active, skipping built-in defaults",
        );
    } else {
        let defaults = default_model_entries(&cfg.endpoints);
        tracing::debug!(count = defaults.len(), "loaded default models");
        resolved.extend(defaults);
    }
    // xAI remote/cache catalog replaces first-party defaults entirely when
    // present. Platform builtins are re-injected *after* this so DeepSeek /
    // OpenAI / Anthropic offline rows are not wiped by the xAI list (Kimi /
    // Moonshot live merges already land in `prefetched` and win on key).
    if let Some(mut prefetched) = prefetched {
        tracing::debug!(count = prefetched.len(), "loaded prefetched models");
        let default_cw = DEFAULT_CONTEXT_WINDOW;
        for (key, entry) in prefetched.iter_mut() {
            let donor = resolved.get(key);
            if let Some(donor) = donor {
                if entry.info.context_window.get() == default_cw
                    && donor.info.context_window.get() != default_cw
                {
                    tracing::debug!(
                        model_key = %key,
                        model = %entry.info.model,
                        client_default = default_cw,
                        inherited = donor.info.context_window.get(),
                        donor_model = %donor.info.model,
                        "prefetched model missing context_window, inheriting from hardcoded default"
                    );
                    entry.info.context_window = donor.info.context_window;
                }
                if entry.info.agent_type == DEFAULT_AGENT_TYPE {
                    entry.info.agent_type.clone_from(&donor.info.agent_type);
                }
                if entry.info.api_backend == ApiBackend::default() {
                    entry.info.api_backend.clone_from(&donor.info.api_backend);
                }
            }
            if resolved.contains_key(key) {
                tracing::debug!(model_key = %key, "prefetched model overriding default");
            }
        }
        resolved = prefetched;
    }
    // Offline multi-provider catalog (Pi). Skip keys already present (live
    // Kimi/Moonshot merge or `[model.*]` donors). Runs after prefetch so an
    // xAI-only list cannot drop `deepseek/*` / `openai/*` / …
    inject_moonshot_builtin_models(&mut resolved);
    for (key, model_override) in &cfg.config_models {
        let had_base = resolved.contains_key(key);
        let base = resolved.shift_remove(key);
        if !had_base {
            tracing::debug!(model_key = %key, "config model adding new entry (not in defaults/prefetched)");
            if model_override.context_window.is_none() {
                tracing::debug!(
                    model_key = %key,
                    default = 200_000,
                    "new model missing context_window, defaulting to 200000 — set context_window in [model.{}] to override",
                    key,
                );
            }
        }
        let with_provider = model_override.model_provider.as_deref().map(|pid| {
            match cfg.model_providers.get(pid) {
                Some(provider) => model_override.with_provider_defaults(provider, pid),
                None => model_override.with_missing_provider(),
            }
        });
        let effective = with_provider.as_ref().unwrap_or(model_override);
        let mut entry = effective.apply(key, base, &cfg.endpoints);
        let session_bearer_unsafe = !crate::util::is_xai_api_bearer_url(&entry.info.base_url)
            || entry
                .api_base_url
                .as_deref()
                .is_some_and(|url| !crate::util::is_xai_api_bearer_url(url));
        if let Some(pid) = model_override.model_provider.as_deref()
            && entry.auth_provider.is_none()
            && session_bearer_unsafe
        {
            entry.auth_provider = Some(crate::auth::AuthProviderRef::fail_closed(format!(
                "model_provider:{pid} (fail-closed)"
            )));
        }
        tracing::debug!(
            model_key = %key,
            base_url = %entry.info.base_url,
            has_api_key = entry.api_key.is_some(),
            env_key = ?entry.env_key,
            auth_provider = entry.auth_provider.as_ref().map(|p| p.name.as_str()),
            model_provider = model_override.model_provider.as_deref(),
            had_base,
            "config model override applied"
        );
        resolved.insert(key.clone(), entry);
    }
    for (key, entry) in resolved.iter_mut() {
        if let Some(ref mut provider) = entry.auth_provider {
            if provider.is_fail_closed() {
                continue;
            }
            let config = cfg.auth_providers.get(&provider.name);
            if config.is_none() {
                tracing::debug!(
                    model_key = %key,
                    provider = %provider.name,
                    "provider ref has no trusted config; failing closed with an empty command"
                );
            }
            provider.attach_trusted_config(config);
        }
    }
    {
        let default_cw = DEFAULT_CONTEXT_WINDOW;
        let donors: std::collections::HashMap<String, std::num::NonZeroU64> = resolved
            .values()
            .filter(|e| e.info.context_window.get() != default_cw)
            .map(|e| (e.info.model.clone(), e.info.context_window))
            .collect();
        for entry in resolved.values_mut() {
            if let Some(donor_cw) = donors.get(&entry.info.model)
                && entry.info.context_window.get() == default_cw
            {
                tracing::debug!(
                    model = %entry.info.model,
                    from = default_cw,
                    to = donor_cw.get(),
                    "slug-match: inheriting context_window from sibling catalog entry"
                );
                entry.info.context_window = *donor_cw;
            }
        }
    }
    if let Some(ref global_agent_type) = cfg.models.agent_type {
        tracing::warn!(
            global_agent_type = %global_agent_type,
            "[models] agent_type is deprecated. Set agent_type on each [model.X] entry instead."
        );
        for entry in resolved.values_mut() {
            if entry.info.agent_type == DEFAULT_AGENT_TYPE {
                entry.info.agent_type = global_agent_type.clone();
            }
        }
    }
    apply_global_extra_headers(&mut resolved, &cfg.models);
    apply_global_scalar_defaults(&mut resolved, &cfg.models);
    apply_platform_credentials(&mut resolved, &cfg.platforms);
    for entry in resolved.values_mut() {
        entry.info.derive_reasoning_effort_fields();
    }
    resolved
}

/// `[platforms.<id>]` — API keys for the data-driven provider registry
/// ([`xai_grok_models::ProviderSpec`]).
///
/// ```toml
/// [platforms.moonshot-cn]
/// api_key = "sk-..."
///
/// [platforms.moonshot-ai]
/// api_key = "sk-..."
/// ```
///
/// Env vars win over the config file:
/// `GROK_MOONSHOT_CN_API_KEY` / `GROK_MOONSHOT_AI_API_KEY` (platform-scoped),
/// then `GROK_MOONSHOT_API_KEY` / `MOONSHOT_API_KEY` (both open platforms).
///
/// SECURITY: key values are never logged and never re-serialized
/// (`Config.platforms` is `skip_serializing`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlatformsConfig {
    #[serde(flatten)]
    pub entries: IndexMap<String, PlatformCredentialConfig>,
}

impl PlatformsConfig {
    /// Config-file API key for a canonical provider id, blank-as-unset.
    pub fn config_api_key_for_provider(&self, provider: &str) -> Option<String> {
        let spec = xai_grok_models::provider_spec(provider)?;
        for storage_id in provider_credential_storage_ids(spec) {
            let Some(storage_provider) = xai_grok_models::provider_spec(&storage_id) else {
                continue;
            };
            let entry = self.entries.get(storage_provider.id.as_str()).or_else(|| {
                storage_provider
                    .aliases
                    .iter()
                    .find_map(|alias| self.entries.get(alias))
            });
            if let Some(key) = entry
                .and_then(|entry| entry.api_key.as_deref())
                .filter(|key| !key.trim().is_empty())
            {
                return Some(key.to_owned());
            }
        }
        None
    }

    /// Compatibility wrapper for bespoke typed platforms.
    pub fn config_api_key(&self, platform: xai_grok_models::PlatformId) -> Option<String> {
        self.config_api_key_for_provider(platform.as_str())
    }

    /// Warn about `[platforms.<id>]` tables that don't name a registry
    /// provider (e.g. typo `moonshot_cn`). Key values are not logged.
    pub fn warn_unknown_platforms(&self) {
        for id in self.entries.keys() {
            if xai_grok_models::provider_spec(id).is_none() {
                tracing::warn!(
                    platform = %id,
                    known = ?xai_grok_models::provider_registry()
                        .providers()
                        .iter()
                        .map(|provider| provider.id.as_str())
                        .collect::<Vec<_>>(),
                    "[platforms.{id}] does not match any registry provider; its api_key is ignored"
                );
            }
        }
    }
}

/// One `[platforms.<id>]` table.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformCredentialConfig {
    /// API key for this platform. NEVER logged; never re-serialized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Canonical auth.json/config lookup order for a provider credential family.
/// The storage-group id wins, followed by legacy member ids for migration.
pub fn provider_credential_storage_ids(provider: &xai_grok_models::ProviderSpec) -> Vec<String> {
    let group = provider.credential_storage_group();
    let mut ids = vec![group.to_string()];
    for candidate in xai_grok_models::provider_registry().providers() {
        if candidate.credential_storage_group() == group && candidate.id.as_str() != group {
            ids.push(candidate.id.as_str().to_string());
        }
    }
    ids
}

/// Resolve the API key for an open-platform registry entry:
/// platform-scoped env > generic env aliases > auth.json (`platform/<id>`,
/// set via `/providers`) > config.toml `[platforms.<id>] api_key`.
/// The returned value must never be logged.
pub fn resolve_platform_api_key(
    platform: xai_grok_models::PlatformId,
    platforms: &PlatformsConfig,
) -> Option<String> {
    let spec = xai_grok_models::provider_spec(platform.as_str())
        .expect("every PlatformId has a validated provider registry row");
    resolve_provider_api_key(spec, platforms)
}

/// Testable core of [`resolve_platform_api_key`] with an injected getenv.
pub fn resolve_platform_api_key_with(
    platform: xai_grok_models::PlatformId,
    platforms: &PlatformsConfig,
    getenv: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let spec = xai_grok_models::provider_spec(platform.as_str())
        .expect("every PlatformId has a validated provider registry row");
    resolve_provider_api_key_with(spec, platforms, getenv)
}

/// Resolve a static API key for any data-driven provider registry row.
pub fn resolve_provider_api_key(
    provider: &xai_grok_models::ProviderSpec,
    platforms: &PlatformsConfig,
) -> Option<String> {
    resolve_provider_api_key_with(provider, platforms, |name| std::env::var(name).ok())
}

/// Testable core of [`resolve_provider_api_key`] with an injected getenv.
pub fn resolve_provider_api_key_with(
    provider: &xai_grok_models::ProviderSpec,
    platforms: &PlatformsConfig,
    mut getenv: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    for name in &provider.credentials.env_keys {
        if let Some(value) = getenv(name)
            && !value.trim().is_empty()
        {
            return Some(value);
        }
    }
    // UI-pasted keys live under `platform/<id>` in auth.json (env wins).
    // Shared provider families consult the canonical group first, then legacy
    // member scopes so existing OpenCode Go logins continue to work.
    let home = xai_grok_config::grok_home();
    for storage_id in provider_credential_storage_ids(provider) {
        if let Some(key) = crate::auth::read_platform_api_key(&home, &storage_id) {
            return Some(key);
        }
    }
    platforms.config_api_key_for_provider(provider.id.as_str())
}

fn env_trimmed(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn google_vertex_external_readiness(provider: &xai_grok_models::ProviderSpec) -> bool {
    let project_ready = provider
        .runtime
        .project_env_keys
        .iter()
        .any(|name| env_trimmed(name).is_some());
    let location_ready = provider
        .runtime
        .location_env_keys
        .iter()
        .any(|name| env_trimmed(name).is_some());
    if !project_ready || !location_ready {
        return false;
    }
    provider
        .runtime
        .external_readiness_env_keys
        .iter()
        .any(|name| {
            env_trimmed(name)
                .map(|path| std::path::Path::new(&path).exists())
                .unwrap_or(false)
        })
        || std::env::var("HOME")
            .map(|home| {
                std::path::Path::new(&home)
                    .join(".config/gcloud/application_default_credentials.json")
                    .exists()
            })
            .unwrap_or(false)
}

fn amazon_bedrock_external_readiness() -> bool {
    if env_trimmed("AWS_BEDROCK_SKIP_AUTH").as_deref() == Some("1") {
        return true;
    }
    if env_trimmed("AWS_ACCESS_KEY_ID").is_some() && env_trimmed("AWS_SECRET_ACCESS_KEY").is_some()
    {
        return true;
    }
    if env_trimmed("AWS_PROFILE").is_some() {
        return true;
    }
    if env_trimmed("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
        || env_trimmed("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
    {
        return true;
    }
    if let Some(path) = env_trimmed("AWS_WEB_IDENTITY_TOKEN_FILE")
        && std::path::Path::new(&path).exists()
    {
        return true;
    }
    crate::auth::read_bedrock_auth_marker(&xai_grok_config::grok_home()).is_some()
}

fn provider_external_readiness(provider: &xai_grok_models::ProviderSpec) -> bool {
    if provider.runtime.external_readiness_env_keys.is_empty() {
        return false;
    }
    match provider.id.as_str() {
        "google-vertex" => google_vertex_external_readiness(provider),
        "amazon-bedrock" => amazon_bedrock_external_readiness(),
        _ => provider
            .runtime
            .external_readiness_env_keys
            .iter()
            .any(|name| {
                env_trimmed(name)
                    .map(|value| std::path::Path::new(&value).exists())
                    .unwrap_or(false)
            }),
    }
}

/// Build a selectable effort row for a platform catalog entry.
fn platform_effort_option(
    value: ReasoningEffort,
    label: &str,
    description: &str,
    default: bool,
) -> ReasoningEffortOption {
    ReasoningEffortOption {
        id: value.as_str().to_string(),
        value,
        label: label.to_string(),
        description: Some(description.to_string()),
        default,
    }
}

/// Official OpenAI Codex catalog (`codex-rs/models-manager/models.json`)
/// `supported_reasoning_levels` for a model id (slug without platform prefix).
///
/// Source of truth for GPT-5.6 Sol/Terra/Luna effort menus (includes `max` /
/// `ultra` where the Codex CLI exposes them). Pi's thinkingLevelMap is a
/// partial projection (has `max` for 5.6, not `ultra`); we prefer the Codex
/// catalog so Sol/Terra users can pick Ultra. Note that `ultra` currently
/// maps to the same wire value as `max`; automatic task delegation is a
/// future capability and is not advertised in the option description.
fn openai_codex_catalog_efforts(
    model: &str,
) -> Option<(ReasoningEffort, Vec<(ReasoningEffort, &'static str)>)> {
    use ReasoningEffort as E;
    // Descriptions copied from Codex models.json.
    const LOW: (E, &str) = (E::Low, "Fast responses with lighter reasoning");
    const MEDIUM: (E, &str) = (
        E::Medium,
        "Balances speed and reasoning depth for everyday tasks",
    );
    const HIGH: (E, &str) = (E::High, "Greater reasoning depth for complex problems");
    const XHIGH: (E, &str) = (E::Xhigh, "Extra high reasoning depth for complex problems");
    const MAX: (E, &str) = (E::Max, "Maximum reasoning depth for the hardest problems");
    // Codex CLI catalog exposes an `ultra` level for Sol/Terra. The wire value
    // is currently identical to `max` (see `reasoning_effort_for_request`):
    // there is no distinct API effort string, and automatic task delegation
    // is a future client-side multi-agent policy, not a current capability.
    // The label is kept to match the Codex CLI menu, but the description must
    // not promise delegation that has not shipped.
    const ULTRA: (E, &str) = (
        E::Ultra,
        "Highest reasoning tier (wire: max; same payload as Max for now)",
    );
    // Base ladder shared by gpt-5.2 … gpt-5.5.
    let base = [LOW, MEDIUM, HIGH, XHIGH];
    // GPT-5.6 adds max; Sol/Terra also add ultra.
    let with_max = [LOW, MEDIUM, HIGH, XHIGH, MAX];
    let with_ultra = [LOW, MEDIUM, HIGH, XHIGH, MAX, ULTRA];

    let (default, rows) = match model {
        // Codex catalog (2026): sol default low; terra/luna default medium.
        "gpt-5.6-sol" => (E::Low, with_ultra.as_slice()),
        "gpt-5.6-terra" => (E::Medium, with_ultra.as_slice()),
        "gpt-5.6-luna" => (E::Medium, with_max.as_slice()), // max yes, ultra no
        "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.2" => (E::Medium, base.as_slice()),
        // Offline fallback not in current Codex models.json — same as 5.4.
        "gpt-5.3-codex-spark" => (E::Low, base.as_slice()),
        _ if model.starts_with("gpt-5.6") => (E::Medium, with_max.as_slice()),
        _ if model.starts_with("gpt-5") => (E::Medium, base.as_slice()),
        _ => return None,
    };
    Some((default, rows.to_vec()))
}

/// Per-model reasoning-effort menu for built-in platform catalogs.
///
/// OpenAI Codex: official Codex CLI `supported_reasoning_levels` (not a
/// single global low/medium/max). GPT-5.6 Sol/Terra expose **max** and
/// **ultra**; Luna exposes max but not ultra; 5.5/5.4 stop at xhigh.
///
/// Kimi K3: Pi `KIMI_K3_THINKING_LEVEL_MAP` → low / high / max only.
fn platform_builtin_reasoning_efforts(
    platform: Option<xai_grok_models::PlatformId>,
    model: &str,
) -> (bool, Option<ReasoningEffort>, Vec<ReasoningEffortOption>) {
    use ReasoningEffort as E;
    match platform {
        Some(xai_grok_models::PlatformId::OpenAiCodex) => {
            let Some((default, rows)) = openai_codex_catalog_efforts(model) else {
                return (
                    true,
                    Some(E::Medium),
                    vec![
                        platform_effort_option(E::Low, "Low", "Faster, lighter reasoning", false),
                        platform_effort_option(E::Medium, "Medium", "Balanced reasoning", true),
                        platform_effort_option(E::High, "High", "Heavy reasoning", false),
                        platform_effort_option(E::Xhigh, "X-High", "Extra-high reasoning", false),
                    ],
                );
            };
            let opts = rows
                .into_iter()
                .map(|(value, desc)| {
                    platform_effort_option(
                        value,
                        match value {
                            E::Low => "Low",
                            E::Medium => "Medium",
                            E::High => "High",
                            E::Xhigh => "X-High",
                            E::Max => "Max",
                            E::Ultra => "Ultra",
                            E::None => "Off",
                            E::Minimal => "Minimal",
                        },
                        desc,
                        value == default,
                    )
                })
                .collect();
            (true, Some(default), opts)
        }
        Some(xai_grok_models::PlatformId::KimiCode)
            if model == "k3" || model.starts_with("kimi-k3") =>
        {
            // Pi KIMI_K3_THINKING_LEVEL_MAP: off/minimal/medium/xhigh null;
            // low/high/max only.
            let opts = vec![
                platform_effort_option(E::Low, "Low", "Faster K3 thinking", false),
                platform_effort_option(E::High, "High", "Stronger K3 thinking", false),
                platform_effort_option(E::Max, "Max", "Maximum K3 thinking (wire `max`)", true),
            ];
            (true, Some(E::Max), opts)
        }
        _ => (false, None, Vec::new()),
    }
}

/// Insert built-in platform catalog entries when missing. Does not overwrite
/// prefetched / `[model.*]` / xAI defaults that already occupy the same key.
fn inject_moonshot_builtin_models(resolved: &mut IndexMap<String, ModelEntry>) {
    for builtin in xai_grok_models::platform_builtin_models() {
        let key = builtin.catalog_key();
        if resolved.contains_key(&key) {
            continue;
        }
        let provider = builtin.provider_spec();
        let legacy_platform = builtin.legacy_platform();
        let resolved_runtime = builtin.resolved_runtime();
        // A credential alone is insufficient for templated routes: keep the
        // model locked until account/resource placeholders produce a valid URL.
        // Google Vertex is the exception: Pi supports API-key Express Mode,
        // whose collection route does not require project/location expansion.
        let vertex_express_ready = provider.id.as_str() == "google-vertex"
            && provider.credentials.env_keys.iter().any(|key| {
                std::env::var(key)
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false)
            });
        let env_key = if provider.credentials.env_keys.is_empty()
            || (!resolved_runtime.ready && !vertex_express_ready)
        {
            None
        } else {
            Some(EnvKeys::new(provider.credentials.env_keys.iter().cloned()))
        };
        let catalog_backend = builtin.api_backend;
        // Kimi For Coding gray-release switch: default Anthropic Messages,
        // `GROK_KIMI_CODE_API_BACKEND=chat_completions` routes the same
        // models over the OpenAI-compatible endpoint while we validate
        // parity. Unset / unrecognized values keep the catalog backend.
        let effective_backend = if legacy_platform == Some(xai_grok_models::PlatformId::KimiCode) {
            std::env::var(xai_grok_models::KIMI_CODE_API_BACKEND_ENV)
                .ok()
                .and_then(|v| xai_grok_models::PlatformApiBackend::parse(v.trim()))
                .unwrap_or(catalog_backend)
        } else {
            catalog_backend
        };
        let api_backend = match effective_backend {
            xai_grok_models::PlatformApiBackend::ChatCompletions => ApiBackend::ChatCompletions,
            xai_grok_models::PlatformApiBackend::Responses => ApiBackend::Responses,
            xai_grok_models::PlatformApiBackend::Messages => ApiBackend::Messages,
            xai_grok_models::PlatformApiBackend::GoogleGenerateContent => {
                ApiBackend::GoogleGenerateContent
            }
            xai_grok_models::PlatformApiBackend::BedrockConverseStream => {
                ApiBackend::BedrockConverseStream
            }
            xai_grok_models::PlatformApiBackend::PiMessages => ApiBackend::PiMessages,
        };
        // Schema-v3 catalog routes are authoritative. The Kimi gray switch can
        // select a different protocol at runtime; only that compatibility path
        // falls back to the legacy backend-derived auth/header defaults.
        let uses_catalog_route = effective_backend == catalog_backend;
        let auth_scheme = if uses_catalog_route {
            Some(match builtin.route.auth {
                xai_grok_models::RouteAuth::Bearer => AuthScheme::Bearer,
                xai_grok_models::RouteAuth::XApiKey => AuthScheme::XApiKey,
                xai_grok_models::RouteAuth::ApiKey => AuthScheme::ApiKey,
                xai_grok_models::RouteAuth::CfAigAuthorization => AuthScheme::CfAigAuthorization,
                xai_grok_models::RouteAuth::XGoogApiKey => AuthScheme::XGoogApiKey,
            })
        } else if provider.uses_oauth() {
            None
        } else if provider.uses_x_api_key()
            || matches!(
                effective_backend,
                xai_grok_models::PlatformApiBackend::Messages
            )
        {
            Some(AuthScheme::XApiKey)
        } else {
            None
        };
        let mut extra_headers: IndexMap<String, String> = if uses_catalog_route {
            builtin
                .route
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        } else {
            IndexMap::new()
        };
        if legacy_platform == Some(xai_grok_models::PlatformId::Anthropic)
            || (matches!(
                effective_backend,
                xai_grok_models::PlatformApiBackend::Messages
            ) && !provider.uses_oauth())
        {
            extra_headers.insert(
                "anthropic-version".into(),
                xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE.into(),
            );
        }
        // Official Pi kimi-coding: User-Agent KimiCLI + anthropic-version
        // (Messages API). Device identity headers are injected per-request.
        // The anthropic-version header only applies on the Messages backend;
        // the chat_completions gray switch must not send it.
        if legacy_platform == Some(xai_grok_models::PlatformId::KimiCode) {
            extra_headers
                .entry("User-Agent".into())
                .or_insert_with(|| "KimiCLI/1.5".into());
            if matches!(
                effective_backend,
                xai_grok_models::PlatformApiBackend::Messages
            ) {
                extra_headers.insert(
                    "anthropic-version".into(),
                    xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE.into(),
                );
            }
        }
        // Anthropic Claude subscription (OAuth Bearer + Messages) needs
        // `anthropic-version`; the `anthropic-beta: oauth-2025-04-20` header is
        // stamped per-request by `AnthropicClaudeBearerResolver`.
        if legacy_platform == Some(xai_grok_models::PlatformId::AnthropicClaude) {
            extra_headers.insert(
                "anthropic-version".into(),
                xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE.into(),
            );
        }
        // Prefer an explicit per-platform effort menu over the legacy
        // low/medium/high/xhigh fallback so GPT-5 / Kimi show their real tiers.
        let (menu_supports, menu_default, menu_opts) =
            platform_builtin_reasoning_efforts(legacy_platform, &builtin.model);
        let supports_reasoning_effort = builtin.supports_reasoning_effort || menu_supports;
        let reasoning_effort = if supports_reasoning_effort {
            menu_default
        } else {
            None
        };
        let reasoning_efforts = if supports_reasoning_effort {
            menu_opts
        } else {
            Vec::new()
        };
        let config = ModelEntryConfig {
            id: Some(key.clone()),
            model: resolved_runtime.wire_model_id.clone(),
            base_url: resolved_runtime.base_url.clone(),
            api_base_url: None,
            name: Some(builtin.name.clone()),
            description: Some(builtin.description.clone()),
            context_window: builtin.context_window_nonzero(),
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            // Kimi fixed-sampling models error if temperature/top_p are set
            // to non-default values — leave unset and let the API defaults apply.
            temperature: None,
            top_p: None,
            max_completion_tokens: builtin.max_completion_tokens,
            api_backend,
            request_compat: uses_catalog_route.then(|| builtin.request_compat.clone()),
            endpoint_path: uses_catalog_route.then(|| builtin.route.path.clone()),
            auth_scheme,
            agent_type: default_agent_type(),
            inference_idle_timeout_secs: None,
            max_retries: None,
            api_key: None,
            env_key,
            extra_headers,
            query_params: if uses_catalog_route {
                resolved_runtime
                    .query_params
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            } else {
                IndexMap::new()
            },
            use_concise: false,
            hidden: false,
            supported_in_api: builtin.supported_in_api,
            reasoning_effort,
            supports_reasoning_effort,
            reasoning_efforts,
            supports_backend_search: provider.adapter == xai_grok_models::AdapterKind::OpenAiCodex,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: None,
            laziness_detector: LazinessDetectorPerModelConfig::default(),
        };
        tracing::debug!(
            model_key = %key,
            provider = builtin.provider.as_str(),
            "injected built-in provider catalog entry"
        );
        resolved.insert(key.clone(), ModelEntry::from_config_entry(&config));
    }
}

/// Wire `[platforms.*]` credentials into open-platform catalog entries
/// recognized by `{platform_id}/{model_id}` catalog ids.
///
/// - `env_key` defaults to the platform's env names when unset.
/// - an auth.json / config-file `api_key` is stamped only when no env name
///   currently resolves (env > auth.json `/providers` > config.toml).
///   Per-model `[model.*]` credentials always win.
///
/// In-memory only; key values are never logged.
fn apply_platform_credentials(
    resolved: &mut IndexMap<String, ModelEntry>,
    platforms: &PlatformsConfig,
) {
    // Keep persisted, refreshable OAuth sessions visible after their access
    // tokens expire without doing network I/O during catalog resolution. These
    // values are catalog markers only: each platform's live bearer resolver
    // replaces the stamp per request, and the sampler removes it entirely when
    // refresh fails. Kimi live `/models` fetches separately require the
    // currently wire-safe `kimi_code_access_token_cached()` value.
    let kimi_bearer = crate::auth::kimi::kimi_code_catalog_access_token_cached();
    let codex_bearer = crate::auth::openai_codex::openai_codex_catalog_access_token_cached();
    let claude_bearer =
        crate::auth::anthropic_claude::anthropic_claude_catalog_access_token_cached();
    let github_bearer = crate::auth::github_copilot::github_copilot_catalog_access_token_cached();
    let radius_bearer = crate::auth::radius::radius_catalog_access_token_cached();
    let github_available_models =
        crate::auth::github_copilot::github_copilot_available_models_cached();
    apply_platform_credentials_with_bearer(
        resolved,
        platforms,
        kimi_bearer,
        codex_bearer,
        claude_bearer,
        github_bearer,
        radius_bearer,
        github_available_models,
    );
}

/// Testable core of [`apply_platform_credentials`] with injected OAuth bearers.
fn apply_platform_credentials_with_bearer(
    resolved: &mut IndexMap<String, ModelEntry>,
    platforms: &PlatformsConfig,
    kimi_bearer: Option<String>,
    codex_bearer: Option<String>,
    claude_bearer: Option<String>,
    github_bearer: Option<String>,
    radius_bearer: Option<String>,
    github_available_models: Option<Vec<String>>,
) {
    for (key, entry) in resolved.iter_mut() {
        let id = entry.info.id.as_deref().unwrap_or(key.as_str());
        let Some((provider_id, catalog_model_id)) = xai_grok_models::parse_managed_model_key(id)
        else {
            continue;
        };
        let Some(provider) = xai_grok_models::provider_spec(provider_id.as_str()) else {
            continue;
        };
        let catalog_model_id = catalog_model_id.to_string();

        if entry.platform_oauth_active {
            // Drop a previous catalog-only OAuth marker before re-resolving;
            // it must never masquerade as a static key on a restamp.
            entry.api_key = None;
        }
        entry.platform_oauth_active = false;

        let route_query: std::collections::BTreeMap<String, String> = entry
            .info
            .query_params
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let runtime =
            provider.resolve_runtime(&entry.info.base_url, &catalog_model_id, &route_query);
        entry.info.base_url = runtime.base_url;
        if !runtime.ready {
            // Fail closed before credential stamping. Otherwise a Cloudflare
            // API key without account/gateway identity would appear selectable
            // and only fail after a request was sent.
            entry.api_key = None;
            entry.env_key = None;
            entry.auth_provider = None;
            entry.info.supported_in_api = false;
            continue;
        }
        // A user-defined managed row can occupy the catalog key and skip the
        // builtin injection path. Complete any missing runtime materialization
        // here while preserving an explicit wire model/query override.
        if entry.info.model == catalog_model_id {
            entry.info.model = runtime.wire_model_id;
        }
        for (name, value) in runtime.query_params {
            entry.info.query_params.entry(name).or_insert(value);
        }
        if provider.id.as_str() == "github-copilot"
            && let Some(base_url) =
                crate::auth::github_copilot::github_copilot_catalog_base_url_cached()
        {
            entry.info.base_url = base_url;
        }

        if provider.accepts_api_key() {
            if entry.env_key.is_none() {
                entry.env_key = Some(EnvKeys::new(provider.credentials.env_keys.iter().cloned()));
            }
            let env_resolves = entry
                .env_key
                .as_ref()
                .is_some_and(|keys| keys.resolve_value().is_some());
            let provider_key = if provider.id.as_str() == "github-copilot" {
                crate::auth::github_copilot::copilot_github_token_env()
                    .or_else(|| resolve_provider_api_key(provider, platforms))
            } else {
                resolve_provider_api_key(provider, platforms)
            };
            if entry.api_key.is_none()
                && !env_resolves
                && let Some(config_key) = provider_key
            {
                tracing::debug!(
                    model_key = %key,
                    provider = provider.id.as_str(),
                    "stamped provider api_key onto catalog entry"
                );
                entry.api_key = Some(config_key);
            }
            if entry.has_own_credentials() {
                entry.info.supported_in_api = true;
                // A static key is authoritative for hybrid providers. Never
                // install an OAuth resolver that could strip it on request.
                if provider.uses_oauth() {
                    continue;
                }
            }
        }

        if provider_external_readiness(provider) {
            entry.info.supported_in_api = true;
        }

        if provider.uses_oauth() {
            let is_github_copilot = provider.id.as_str() == "github-copilot";
            let is_radius = provider.id.as_str() == "radius";
            let bearer = match provider.legacy_platform() {
                Some(xai_grok_models::PlatformId::KimiCode) => kimi_bearer.as_ref(),
                Some(xai_grok_models::PlatformId::OpenAiCodex) => codex_bearer.as_ref(),
                Some(xai_grok_models::PlatformId::AnthropicClaude) => claude_bearer.as_ref(),
                _ if is_github_copilot => github_bearer.as_ref(),
                _ if is_radius => radius_bearer.as_ref(),
                _ => None,
            };
            let github_available = !is_github_copilot
                || github_available_models
                    .as_ref()
                    .is_none_or(|ids| ids.iter().any(|id| id == &catalog_model_id));
            if github_available
                && entry.api_key.is_none()
                && let Some(bearer) = bearer
            {
                tracing::debug!(
                    model_key = %key,
                    provider = provider.id.as_str(),
                    "stamped OAuth bearer onto subscription entry"
                );
                entry.api_key = Some(bearer.clone());
                entry.platform_oauth_active = true;
                // OAuth-gated models become selectable once a token is present.
                entry.info.supported_in_api = true;
            }
        }
    }
}

/// Layer 6 of [`resolve_model_list`]: fold the global `[models].extra_headers`
/// into every model as a base. The presence check is case-insensitive because
/// the sampler lowers these into an `http::HeaderMap`, so a global `X-Foo` must
/// not shadow a per-model `x-foo`; a per-model `[model.<id>].extra_headers`
/// (applied earlier) therefore wins per key.
fn apply_global_extra_headers(resolved: &mut IndexMap<String, ModelEntry>, models: &ModelsConfig) {
    if models.extra_headers.is_empty() {
        return;
    }
    tracing::debug!(
        header_keys = ?models.extra_headers.keys().collect::<Vec<_>>(),
        model_count = resolved.len(),
        "applying global [models].extra_headers default to all models"
    );
    for entry in resolved.values_mut() {
        for (k, v) in &models.extra_headers {
            let present = entry
                .info
                .extra_headers
                .keys()
                .any(|ek| ek.eq_ignore_ascii_case(k));
            if !present {
                entry.info.extra_headers.insert(k.clone(), v.clone());
            }
        }
    }
}
/// Layer 7 of [`resolve_model_list`]: fill scalar `[models]` defaults into any
/// model that left the field unset. Per-model (Layer 3) and remote-prefetched
/// (Layer 2) values already populated theirs, so they win via `get_or_insert`
/// (the global default is a fallback, not a clamp).
fn apply_global_scalar_defaults(
    resolved: &mut IndexMap<String, ModelEntry>,
    models: &ModelsConfig,
) {
    for entry in resolved.values_mut() {
        let info = &mut entry.info;
        if let Some(v) = models.temperature {
            info.temperature.get_or_insert(v);
        }
        if let Some(v) = models.top_p {
            info.top_p.get_or_insert(v);
        }
        if let Some(v) = models.max_completion_tokens {
            info.max_completion_tokens.get_or_insert(v);
        }
        if let Some(v) = models.max_retries {
            info.max_retries.get_or_insert(v);
        }
        if let Some(v) = models.inference_idle_timeout_secs {
            info.inference_idle_timeout_secs.get_or_insert(v);
        }
        if let Some(v) = models.stream_tool_calls {
            info.stream_tool_calls.get_or_insert(v);
        }
    }
}
/// Built-in default models. Prefer `resolve_model_list()`.
pub(crate) fn default_model_entries(endpoints: &EndpointsConfig) -> IndexMap<String, ModelEntry> {
    default_models(endpoints)
        .into_iter()
        .map(|(key, entry)| (key, ModelEntry::from_config_entry(&entry)))
        .collect()
}
/// Resolve a model against the available model map.
/// Checks the map key (id) first, then falls back to a slug scan.
pub(crate) fn find_model_by_id<'a>(
    models: &'a IndexMap<String, ModelEntry>,
    model_id: &str,
) -> Option<&'a ModelEntry> {
    models
        .get(model_id)
        .or_else(|| models.values().find(|m| m.model == model_id))
}
/// Whether the EFFECTIVE Auto-mode classifier model supports reasoning effort:
/// the model actually routed to (`aux_model` when the aux sampler resolved) else
/// the session model the worker falls back to. Not-found-in-catalog ⇒ `false`
/// (conservative; also covers the Tier-2 synthetic proxy entry). Drives the
/// built-in `low` effort default.
pub(crate) fn effective_classifier_supports_re(
    aux_model: Option<&str>,
    session_model: &str,
    models: &IndexMap<String, ModelEntry>,
) -> bool {
    find_model_by_id(models, aux_model.unwrap_or(session_model))
        .map(|e| e.info().supports_reasoning_effort)
        .unwrap_or(false)
}
/// JSON-only subset of `ModelEntryConfig`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DefaultModelJson {
    id: Option<String>,
    model: String,
    name: Option<String>,
    description: Option<String>,
    context_window: Option<NonZeroU64>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_completion_tokens: Option<u32>,
    api_backend: ApiBackend,
    #[serde(default = "default_agent_type")]
    agent_type: String,
    inference_idle_timeout_secs: Option<u64>,
    hidden: bool,
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    supports_reasoning_effort: bool,
    #[serde(default)]
    reasoning_efforts: Vec<ReasoningEffortOption>,
    /// When false, only OAuth users see this in the picker.
    #[serde(default = "default_true")]
    supported_in_api: bool,
    #[serde(default)]
    supports_backend_search: bool,
    #[serde(default)]
    compactions_remaining: Option<CompactionsRemaining>,
    #[serde(default)]
    compaction_at_tokens: Option<CompactionAtTokens>,
    #[serde(default)]
    show_model_fingerprint: bool,
    #[serde(default)]
    auto_compact_threshold_percent: Option<u8>,
    #[serde(default)]
    system_prompt_label: Option<String>,
}
fn default_models(endpoints: &EndpointsConfig) -> IndexMap<String, ModelEntryConfig> {
    let root: serde_json::Value = serde_json::from_str(crate::models::DEFAULT_MODELS_JSON)
        .expect("default_models.json: invalid JSON");
    let entries: Vec<DefaultModelJson> = serde_json::from_value(
        root.get("models")
            .expect("default_models.json: missing 'models' array")
            .clone(),
    )
    .expect("default_models.json: invalid 'models' array");
    tracing::debug!(
        count = entries.len(),
        "loaded default models from embedded JSON"
    );
    entries
        .into_iter()
        .map(|m| {
            assert!(
                !m.model.is_empty(),
                "default_models.json: entry id={:?} has empty `model` field",
                m.id
            );
            let key = m.id.clone().unwrap_or_else(|| m.model.clone());
            let context_window = m
                .context_window
                .unwrap_or_else(|| NonZeroU64::new(200_000).expect("200000 is non-zero"));
            let config = ModelEntryConfig {
                id: m.id,
                model: m.model,
                base_url: endpoints.resolve_inference_base_url(),
                api_base_url: Some(endpoints.xai_api_base_url.clone()),
                name: m.name,
                description: m.description,
                context_window,
                auto_compact_threshold_percent: m.auto_compact_threshold_percent,
                system_prompt_label: m.system_prompt_label,
                temperature: m.temperature,
                top_p: m.top_p,
                max_completion_tokens: m.max_completion_tokens,
                api_backend: m.api_backend,
                request_compat: None,
                endpoint_path: None,
                auth_scheme: None,
                agent_type: m.agent_type,
                inference_idle_timeout_secs: m.inference_idle_timeout_secs,
                max_retries: None,
                api_key: None,
                env_key: None,
                extra_headers: IndexMap::new(),
                query_params: IndexMap::new(),
                use_concise: false,
                hidden: m.hidden,
                supported_in_api: m.supported_in_api,
                reasoning_effort: m.reasoning_effort,
                supports_reasoning_effort: m.supports_reasoning_effort,
                reasoning_efforts: m.reasoning_efforts,
                supports_backend_search: m.supports_backend_search,
                compactions_remaining: m.compactions_remaining,
                compaction_at_tokens: m.compaction_at_tokens,
                show_model_fingerprint: m.show_model_fingerprint,
                stream_tool_calls: None,
                laziness_detector: LazinessDetectorPerModelConfig::default(),
            };
            (key, config)
        })
        .collect()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntryConfig {
    /// Stable unique identifier for this catalog entry. When present,
    /// used as the catalog map key. Falls back to `model` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The routing slug sent in API requests.
    pub model: String,
    /// The base URL of the model. e.g. "https://api.x.ai/v1"
    pub base_url: String,
    /// Human-readable display name of the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// The API key for this model's provider.
    /// If not set, falls back to env_key, then XAI_API_KEY.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Environment variable name(s) that hold the provider API key.
    /// Accepts a string or an array (first set, non-empty value wins).
    /// If not set, falls back to XAI_API_KEY.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_key: Option<EnvKeys>,
    /// Which API backend to use for this model.
    /// Values: `"chat_completions"` (default), `"responses"`, `"codex_responses"`
    /// (alias `"codex-responses"`: Responses wire + ChatGPT Codex dialect for
    /// official Codex or third-party Codex reverse proxies / 中转站),
    /// `"messages"`, `"google_generate_content"`, `"bedrock_converse_stream"`,
    /// `"pi_messages"`.
    #[serde(default)]
    pub api_backend: ApiBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_compat: Option<xai_grok_models::RequestCompat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_scheme: Option<AuthScheme>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub supports_reasoning_effort: bool,
    /// Per-model reasoning-effort menu (source of truth). The two legacy fields
    /// above are derived from this list when it is non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<ReasoningEffortOption>,
    /// Extra headers to send with requests to this model's endpoint.
    /// Useful for BYOK (Bring Your Own Key) scenarios.
    /// Example: { "x-anthropic-api-key" = "sk-ant-..." }
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub extra_headers: IndexMap<String, String>,
    /// Query parameters attached to the explicit model route.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub query_params: IndexMap<String, String>,
    /// The total context window size in tokens for this model.
    /// Used for auto-compact threshold calculations.
    /// Required — BYOK users must explicitly set this in config.toml.
    pub context_window: NonZeroU64,
    /// Per-model auto-compact threshold (0-100). When the session's token
    /// usage exceeds this percentage of `context_window`, the conversation
    /// is summarized. Resolver precedence:
    /// requirements > env > user (per-model > global) > managed (per-model > global)
    /// > remote per-model (this field) > remote global > 85.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold_percent: Option<u8>,
    /// Per-model system-prompt identity label (not UI `name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_label: Option<String>,
    /// The base URL to use when authenticating with an API key (non-session auth).
    /// When set, `base_url` is used for session-based auth and `api_base_url` for API key auth.
    /// When not set, `base_url` is used for all auth methods (e.g. BYOK / third-party models).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    /// When true, this model uses concise mode (compact system prompt,
    /// concise tool output, concise user message prefix, reduced toolset).
    /// Defaults to false — when omitted or false, nothing changes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_concise: bool,
    /// The type of system prompt to use for this model.
    /// e.g. "grok-build", "codex".
    #[serde(default = "default_agent_type")]
    pub agent_type: String,
    /// Maximum seconds to wait between SSE chunks during inference streaming.
    /// When no chunk is received within this duration, the request fails with
    /// a non-retryable `IdleTimeout` error. This is a per-chunk deadline that
    /// resets on every received chunk — NOT a total-turn timeout.
    /// Default: 300 seconds (5 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_idle_timeout_secs: Option<u64>,
    /// Maximum number of retries for transient API errors (429, 500, 502, etc.)
    /// during a single inference request. Default: 5.
    /// Can also be set via the `GROK_MAX_RETRIES` environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    /// Exclude from the client model picker; still usable internally (web_search, etc.).
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// When false, only OAuth users see this in the picker.
    #[serde(default = "default_true")]
    pub supported_in_api: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub supports_backend_search: bool,
    /// Per-model config for the `x-compactions-remaining` header; `None` disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactions_remaining: Option<CompactionsRemaining>,
    /// Per-model config for the `x-compaction-at` header; `None` disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_at_tokens: Option<CompactionAtTokens>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub show_model_fingerprint: bool,
    /// Inject `stream_tool_calls: true` into the request body
    /// so the upstream emits per-chunk `function_call_arguments.delta`
    /// Without this set, xAI API models send args as one delta
    /// event, defeating the purpose of streaming.
    ///
    /// Per-model opt-in -- BYOK endpoints that don't understand the
    /// flag should leave this unset to avoid request errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_tool_calls: Option<bool>,
    /// Per-model Layer-3 LazinessDetector configuration. Defaults to
    /// the all-disabled state via `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "is_default_laziness_detector")]
    pub laziness_detector: LazinessDetectorPerModelConfig,
}
/// True when `cfg` equals the all-disabled default. Derives `PartialEq`
/// on `f32`, which is fine for the current shape because both `f32`
/// fields default to `None` — there's no parsed-vs-literal `0.7` float
/// equality footgun. If a future default introduces `Some(0.7)`, this
/// helper must be reworked (e.g. compare on tolerance, or switch to a
/// bit-pattern compare) so `skip_serializing_if` doesn't start emitting
/// `[laziness_detector]` blocks for every model in `config.toml`.
fn is_default_laziness_detector(cfg: &LazinessDetectorPerModelConfig) -> bool {
    cfg == &LazinessDetectorPerModelConfig::default()
}
/// A `[model.foo]` entry from config.toml, parsed directly from raw TOML
/// (bypassing deep merge). Scalar fields are `Option` so absent means "inherit
/// from defaults/prefetched"; the collection fields (`extra_headers`,
/// `reasoning_efforts`) merge only when non-empty and so cannot express
/// "override to empty."
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ConfigModelOverride {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub api_key: Option<String>,
    /// Env var name(s) for the provider key — string or array in config.toml.
    pub env_key: Option<EnvKeys>,
    /// Name of a `[auth_provider.<name>]` credential helper that mints
    /// this model's bearer token. Static `api_key` / `env_key` win when both
    /// are set.
    pub auth_provider: Option<String>,
    pub model_provider: Option<String>,
    pub api_base_url: Option<String>,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub api_backend: Option<ApiBackend>,
    pub request_compat: Option<xai_grok_models::RequestCompat>,
    pub endpoint_path: Option<String>,
    #[serde(default)]
    pub extra_headers: IndexMap<String, String>,
    #[serde(default)]
    pub query_params: IndexMap<String, String>,
    #[serde(default)]
    pub env_http_headers: IndexMap<String, String>,
    pub context_window: Option<u64>,
    /// Per-model auto-compact threshold override (0-100) from `[model.<id>]`.
    /// Read directly by `resolve_auto_compact_threshold_percent`; intentionally
    /// NOT merged into `ModelInfo.auto_compact_threshold_percent` so the
    /// resolver can keep user-per-model distinct from GB-per-model.
    pub auto_compact_threshold_percent: Option<u8>,
    /// Per-model system-prompt identity; not merged into `ModelInfo` (tiered resolve).
    pub system_prompt_label: Option<String>,
    pub use_concise: Option<bool>,
    pub agent_type: Option<String>,
    pub inference_idle_timeout_secs: Option<u64>,
    pub max_retries: Option<u32>,
    pub hidden: Option<bool>,
    pub supported_in_api: Option<bool>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub supports_reasoning_effort: Option<bool>,
    pub reasoning_efforts: Vec<ReasoningEffortOption>,
    pub supports_backend_search: Option<bool>,
    /// Aliases must be registered in `config_model_override_parse::ALIASES`;
    /// serde rejects a table that contains both spellings otherwise.
    #[serde(alias = "send_compactions_remaining")]
    pub compactions_remaining: Option<CompactionsRemaining>,
    pub compaction_at_tokens: Option<CompactionAtTokens>,
    pub show_model_fingerprint: Option<bool>,
    pub stream_tool_calls: Option<bool>,
}
impl ConfigModelOverride {
    pub(crate) fn apply(
        &self,
        key: &str,
        base: Option<ModelEntry>,
        endpoints: &EndpointsConfig,
    ) -> ModelEntry {
        let mut entry = base.unwrap_or_else(|| ModelEntry::fallback(key, endpoints));
        if let Some(ref v) = self.model {
            entry.info.model = v.clone();
        }
        if let Some(ref v) = self.base_url {
            entry.info.base_url = v.clone();
            if self.api_base_url.is_none() {
                entry.api_base_url = None;
            }
        }
        if self.name.is_some() {
            entry.info.name.clone_from(&self.name);
        }
        if self.description.is_some() {
            entry.info.description.clone_from(&self.description);
        }
        if self.max_completion_tokens.is_some() {
            entry.info.max_completion_tokens = self.max_completion_tokens;
        }
        if self.temperature.is_some() {
            entry.info.temperature = self.temperature;
        }
        if self.top_p.is_some() {
            entry.info.top_p = self.top_p;
        }
        if let Some(ref v) = self.api_backend {
            entry.info.api_backend = v.clone();
        }
        if self.request_compat.is_some() {
            entry.info.request_compat.clone_from(&self.request_compat);
        }
        if self.endpoint_path.is_some() {
            entry.info.endpoint_path.clone_from(&self.endpoint_path);
        }
        if !self.extra_headers.is_empty() {
            entry.info.extra_headers = self.extra_headers.clone();
        }
        if !self.query_params.is_empty() {
            entry.info.query_params = self.query_params.clone();
        }
        if !self.env_http_headers.is_empty() {
            entry.info.env_http_headers = self.env_http_headers.clone();
        }
        if let Some(cw) = self.context_window.and_then(NonZeroU64::new) {
            entry.info.context_window = cw;
        }
        if let Some(v) = self.use_concise {
            entry.info.use_concise = v;
        }
        if let Some(ref at) = self.agent_type {
            entry.info.agent_type.clone_from(at);
        }
        if self.inference_idle_timeout_secs.is_some() {
            entry.info.inference_idle_timeout_secs = self.inference_idle_timeout_secs;
        }
        if self.max_retries.is_some() {
            entry.info.max_retries = self.max_retries;
        }
        if let Some(v) = self.hidden {
            entry.info.hidden = v;
        }
        if let Some(v) = self.supported_in_api {
            entry.info.supported_in_api = v;
        }
        if self.reasoning_effort.is_some() {
            entry.info.reasoning_effort = self.reasoning_effort;
        }
        if let Some(v) = self.supports_reasoning_effort {
            entry.info.supports_reasoning_effort = v;
        } else if !entry.info.supports_reasoning_effort
            && matches!(entry.info.api_backend, ApiBackend::Messages)
        {
            entry.info.supports_reasoning_effort = true;
        }
        if !self.reasoning_efforts.is_empty() {
            entry.info.reasoning_efforts = self.reasoning_efforts.clone();
        }
        if let Some(v) = self.supports_backend_search {
            entry.info.supports_backend_search = v;
        } else if self.api_backend == Some(ApiBackend::CodexResponses) {
            entry.info.supports_backend_search = true;
        }
        if self.compactions_remaining.is_some() {
            entry.info.compactions_remaining = self.compactions_remaining;
        }
        if self.compaction_at_tokens.is_some() {
            entry.info.compaction_at_tokens = self.compaction_at_tokens;
        }
        if let Some(v) = self.show_model_fingerprint {
            entry.info.show_model_fingerprint = v;
        }
        if self.stream_tool_calls.is_some() {
            entry.info.stream_tool_calls = self.stream_tool_calls;
        }
        if self.api_key.is_some() {
            entry.api_key.clone_from(&self.api_key);
        }
        if self.env_key.is_some() {
            entry.env_key.clone_from(&self.env_key);
        }
        if let Some(ref name) = self.auth_provider {
            entry.auth_provider = Some(crate::auth::AuthProviderRef::unresolved(name.clone()));
        }
        if self.api_base_url.is_some() {
            entry.api_base_url.clone_from(&self.api_base_url);
        }
        if self.supported_in_api.is_none()
            && (self.api_key.is_some() || self.env_key.is_some() || self.auth_provider.is_some())
        {
            entry.info.supported_in_api = true;
        }
        entry
    }
}
/// Shared model metadata — the common fields across all model sources.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    /// Stable unique identifier for this catalog entry.
    /// Falls back to `model` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The routing slug sent in API requests.
    pub model: String,
    /// The base URL of the model (session endpoint). e.g. "https://cli-chat-proxy.grok.com/v1"
    pub base_url: String,
    /// Human-readable name of the model. Honored by both the picker
    /// (`/model`) and `/session-info` -- when set, that's the label shown
    /// to users in either consumer.
    pub name: Option<String>,
    pub description: Option<String>,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub api_backend: ApiBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_compat: Option<xai_grok_models::RequestCompat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_path: Option<String>,
    pub auth_scheme: AuthScheme,
    pub extra_headers: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub query_params: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env_http_headers: IndexMap<String, String>,
    pub context_window: NonZeroU64,
    /// Per-model auto-compact threshold (0-100). `None` defers to the
    /// global / default tiers in `resolve_auto_compact_threshold_percent`.
    pub auto_compact_threshold_percent: Option<u8>,
    /// Per-model system-prompt identity (not UI picker `name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_label: Option<String>,
    /// When true, this model uses concise mode (compact system prompt,
    /// concise tool output, concise user message prefix, reduced toolset).
    pub use_concise: bool,
    /// The type of agent configuration to use for this model.
    /// Always has a value; defaults to `"grok-build-plan"` when the server
    /// or user config doesn't specify one.
    #[serde(default = "default_agent_type")]
    pub agent_type: String,
    /// Per-chunk idle timeout for inference streaming (see `ModelEntryConfig`).
    pub inference_idle_timeout_secs: Option<u64>,
    pub max_retries: Option<u32>,
    /// Never show in picker (any auth). See also `supported_in_api`.
    pub hidden: bool,
    /// May the user select this model for normal chat? Derived from
    /// `allowed_models` in `resolve_model_catalog`; never persisted.
    #[serde(skip_serializing, default = "default_true")]
    pub user_selectable: bool,
    /// When false, only OAuth users see this in the picker.
    #[serde(default = "default_true")]
    pub supported_in_api: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// When true, the UI shows effort controls for this model.
    pub supports_reasoning_effort: bool,
    /// Per-model reasoning-effort menu (source of truth); legacy fields derived from it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<ReasoningEffortOption>,
    pub supports_backend_search: bool,
    /// Per-model config for the `x-compactions-remaining` header; `None` disables it.
    pub compactions_remaining: Option<CompactionsRemaining>,
    /// Per-model config for the `x-compaction-at` header; `None` disables it.
    pub compaction_at_tokens: Option<CompactionAtTokens>,
    pub show_model_fingerprint: bool,
    /// When `Some(true)`, the sampler injects `stream_tool_calls: true`
    pub stream_tool_calls: Option<bool>,
    /// Per-model Layer-3 LazinessDetector configuration. Defaults to
    /// the all-disabled state — the feature is per-model opt-in with a
    /// second-step `max_nudges_per_session > 0` opt-in for actually
    /// injecting nudges. See [`LazinessDetectorPerModelConfig`].
    #[serde(default)]
    pub laziness_detector: LazinessDetectorPerModelConfig,
}
impl ModelInfo {
    /// Minimal fallback descriptor for an unknown model slug.
    /// Used when a configured model ID isn't found in presets or remote models.
    pub fn fallback(slug: &str) -> Self {
        ModelInfo {
            user_selectable: true,
            id: None,
            model: slug.to_owned(),
            base_url: String::new(),
            name: None,
            description: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::default(),
            request_compat: None,
            endpoint_path: None,
            auth_scheme: Default::default(),
            extra_headers: IndexMap::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window: NonZeroU64::new(200_000).unwrap(),
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            use_concise: false,
            agent_type: default_agent_type(),
            inference_idle_timeout_secs: None,
            max_retries: None,
            hidden: false,
            supported_in_api: true,
            reasoning_effort: None,
            supports_reasoning_effort: false,
            reasoning_efforts: Vec::new(),
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: None,
            laziness_detector: LazinessDetectorPerModelConfig::default(),
        }
    }
    /// Extract shared model metadata from a flat config entry.
    pub(crate) fn from_config(entry: &ModelEntryConfig) -> Self {
        ModelInfo {
            user_selectable: true,
            id: entry.id.clone(),
            model: entry.model.clone(),
            base_url: entry.base_url.clone(),
            name: entry.name.clone(),
            description: entry.description.clone(),
            max_completion_tokens: entry.max_completion_tokens,
            temperature: entry.temperature,
            top_p: entry.top_p,
            api_backend: entry.api_backend.clone(),
            request_compat: entry.request_compat.clone(),
            endpoint_path: entry.endpoint_path.clone(),
            auth_scheme: entry.auth_scheme.unwrap_or_default(),
            extra_headers: entry.extra_headers.clone(),
            query_params: entry.query_params.clone(),
            env_http_headers: IndexMap::new(),
            context_window: entry.context_window,
            auto_compact_threshold_percent: entry.auto_compact_threshold_percent,
            system_prompt_label: entry.system_prompt_label.clone(),
            use_concise: entry.use_concise,
            agent_type: entry.agent_type.clone(),
            inference_idle_timeout_secs: entry.inference_idle_timeout_secs,
            max_retries: entry.max_retries,
            hidden: entry.hidden,
            supported_in_api: entry.supported_in_api,
            reasoning_effort: entry.reasoning_effort,
            supports_reasoning_effort: entry.supports_reasoning_effort,
            reasoning_efforts: entry.reasoning_efforts.clone(),
            supports_backend_search: entry.supports_backend_search,
            compactions_remaining: entry.compactions_remaining,
            compaction_at_tokens: entry.compaction_at_tokens,
            show_model_fingerprint: entry.show_model_fingerprint,
            stream_tool_calls: entry.stream_tool_calls,
            laziness_detector: entry.laziness_detector.clone(),
        }
    }
    /// Derive the legacy effort gate/default from `reasoning_efforts` so the
    /// shell's internal reads (support gate, wire default, session modes) treat
    /// a menu-only model as supported. The single derive site; `to_acp_model_info`
    /// then just reads these fields. Idempotent (the remote/CCP path already sets
    /// them); the empty-list path leaves both legacy fields untouched.
    fn derive_reasoning_effort_fields(&mut self) {
        if self.reasoning_efforts.is_empty() {
            return;
        }
        self.supports_reasoning_effort = true;
        if self.reasoning_effort.is_none() {
            let default = self
                .reasoning_efforts
                .iter()
                .find(|opt| opt.default)
                .or_else(|| self.reasoning_efforts.first())
                .map(|opt| opt.value);
            self.reasoning_effort = default;
        }
    }
    /// Whether this model appears in the picker for the given auth mode.
    ///
    /// First-party xAI catalog semantics (`supported_in_api`):
    ///
    /// | `hidden` | `supported_in_api` | OAuth user | API-key user |
    /// |----------|--------------------|------------|--------------|
    /// | true     | _                  | hidden     | hidden       |
    /// | false    | true               | visible    | visible      |
    /// | false    | false              | visible    | **hidden**   |
    ///
    /// Multi-provider / managed keys (`openai/…`, `anthropic/…`, `kimi-code/…`)
    /// must **not** use the OAuth-session bypass — see
    /// [`ModelEntry::visible_for_auth`].
    pub fn visible_for_auth(&self, is_session_auth: bool) -> bool {
        !self.hidden && (is_session_auth || self.supported_in_api)
    }
}
/// Flat struct so credential and endpoint fields coexist after deep-merge.
/// Routing reads fields, not provenance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelEntry {
    pub info: ModelInfo,
    pub api_key: Option<String>,
    pub env_key: Option<EnvKeys>,
    /// Named credential helper (`[model.<id>] auth_provider = "<name>"`),
    /// resolved against `[auth_provider.<name>]` by `resolve_model_list`.
    /// Config-file models only: the built-in catalog never carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_provider: Option<crate::auth::AuthProviderRef>,
    /// True only when a managed hybrid provider selected its OAuth credential.
    /// Static API-key mode must not install an authoritative OAuth resolver.
    #[serde(default)]
    pub platform_oauth_active: bool,
    /// When set, `base_url` is used for session auth, `api_base_url` for API-key auth.
    pub api_base_url: Option<String>,
}
impl ModelEntry {
    /// Minimal fallback entry for an unknown model slug.
    pub fn fallback(slug: &str, endpoints: &EndpointsConfig) -> Self {
        let mut info = ModelInfo::fallback(slug);
        info.base_url = endpoints.resolve_inference_base_url();
        Self {
            info,
            api_key: None,
            env_key: None,
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: None,
        }
    }
    pub fn info(&self) -> &ModelInfo {
        &self.info
    }
    pub(crate) fn from_config_entry(entry: &ModelEntryConfig) -> Self {
        Self {
            info: ModelInfo::from_config(entry),
            api_key: entry.api_key.clone(),
            env_key: entry.env_key.clone(),
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: entry.api_base_url.clone(),
        }
    }
    /// Catalog id used for platform key detection (`{platform}/{model}`).
    fn catalog_id(&self) -> &str {
        self.info.id.as_deref().unwrap_or(self.info.model.as_str())
    }
    /// Built-in multi-provider entry (`openai/gpt-5`, `kimi-code/k3`, …).
    ///
    /// These models always need their own API key / platform OAuth token and
    /// must stay hidden when that credential is absent — even if the user has
    /// an xAI login session (which would otherwise unlock
    /// `supported_in_api: false` first-party models).
    pub fn is_managed_platform_model(&self) -> bool {
        self.managed_provider().is_some()
    }
    /// The registry provider this entry belongs to (`{provider}/{model}` ids).
    pub fn managed_provider(&self) -> Option<xai_grok_models::ProviderId> {
        xai_grok_models::parse_managed_model_key(self.catalog_id()).map(|(provider, _)| provider)
    }
    /// Bespoke typed platform, when this provider needs legacy runtime behavior.
    pub fn managed_platform(&self) -> Option<xai_grok_models::PlatformId> {
        self.managed_provider()
            .and_then(|provider| provider.platform_id())
    }
    /// Whether this model appears in the picker for the given auth mode.
    ///
    /// Prefer this over [`ModelInfo::visible_for_auth`] — it correctly gates
    /// multi-provider entries on credentials.
    pub fn visible_for_auth(&self, is_session_auth: bool) -> bool {
        if self.info.hidden {
            return false;
        }
        if self.is_managed_platform_model() {
            // Only show once BYOK / platform OAuth has been stamped.
            return self.has_own_credentials();
        }
        self.info.visible_for_auth(is_session_auth)
    }
    /// Non-empty `api_key`, else first non-empty resolved `env_key`.
    /// `None` → fall through to session / global key. Static only: never
    /// consults auth-provider tokens.
    pub(crate) fn own_credential(&self) -> Option<String> {
        first_own_credential(self.api_key.as_deref(), self.env_key.as_ref())
    }
    /// The provider governing this model's bearer: `None` when a static
    /// `api_key`/`env_key` resolves. The turn paths consult this, so a
    /// shadowed provider never runs.
    pub(crate) fn effective_auth_provider(&self) -> Option<&crate::auth::AuthProviderRef> {
        if self.own_credential().is_some() {
            return None;
        }
        self.auth_provider.as_ref()
    }
    /// `true` when the model has a non-empty `api_key`, an `env_key` that
    /// resolves to a non-empty value, or a named auth provider.
    /// Probes `std::env::var` at call time: result is not stable across env
    /// changes. Never executes a provider command.
    pub(crate) fn has_own_credentials(&self) -> bool {
        self.own_credential().is_some() || self.auth_provider.is_some()
    }
}
impl std::ops::Deref for ModelEntry {
    type Target = ModelInfo;
    fn deref(&self) -> &ModelInfo {
        &self.info
    }
}
fn is_false(v: &bool) -> bool {
    !v
}
fn default_true() -> bool {
    true
}
/// Codebase indexing setting for `[features] codebase_indexing`.
///
/// Patterns are matched against the git root when available, otherwise the cwd,
/// which allows explicitly indexing non-git directories.
///
/// ```toml
/// codebase_indexing = false                                          # disable
/// codebase_indexing = true                                           # any git repo (default)
/// codebase_indexing = ["/Users/*/xai*", "!/Users/*/old-*"]           # globs, ! to exclude
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodebaseIndexingSetting {
    Enabled(bool),
    Patterns(Vec<String>),
}
impl Default for CodebaseIndexingSetting {
    fn default() -> Self {
        Self::Enabled(true)
    }
}
impl CodebaseIndexingSetting {
    /// Should `path` be indexed? For `Enabled(true)`, always yes (caller gates on git-root).
    /// For `Patterns`, path must match an include and not match any `!exclude`.
    pub(crate) fn should_index(&self, path: &std::path::Path) -> bool {
        match self {
            Self::Enabled(b) => *b,
            Self::Patterns(patterns) => {
                let path_str = path.to_string_lossy();
                let matches_any = |pats: &[&str]| {
                    pats.iter()
                        .any(|p| glob::Pattern::new(p).is_ok_and(|pat| pat.matches(&path_str)))
                };
                let (excludes, includes): (Vec<_>, Vec<_>) =
                    patterns.iter().partition(|p| p.starts_with('!'));
                let excludes: Vec<&str> = excludes
                    .iter()
                    .map(|p| p.strip_prefix('!').unwrap_or(p.as_str()))
                    .collect();
                let includes: Vec<&str> = includes.iter().map(|p| p.as_str()).collect();
                let included = includes.is_empty() || matches_any(&includes);
                let excluded = matches_any(&excludes);
                included && !excluded
            }
        }
    }
}
/// Optional role pair that drops a malformed value to `None` (with a warn)
/// instead of failing the whole config parse — one typo must not wipe the
/// config. Mirrors the remote tolerance in `util::config::remote`.
fn de_tolerant_goal_role_model<'de, D>(
    deserializer: D,
) -> Result<Option<crate::util::config::GoalRoleModel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<toml::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| {
        v.try_into()
            .map_err(|e| tracing::warn!(error = %e, "[goal] role model: dropped malformed value"))
            .ok()
    }))
}
/// Skeptic pool variant of [`de_tolerant_goal_role_model`]: a non-array yields
/// an empty pool; malformed entries are dropped, survivor order preserved (the
/// skeptic round-robin depends on it).
fn de_tolerant_goal_role_models<'de, D>(
    deserializer: D,
) -> Result<Vec<crate::util::config::GoalRoleModel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<toml::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(toml::Value::Array(arr)) => arr
            .into_iter()
            .filter_map(|v| {
                v.try_into()
                    .map_err(|e| {
                        tracing::warn!(error = %e, "[goal] skeptic model: dropped malformed entry");
                    })
                    .ok()
            })
            .collect(),
        _ => Vec::new(),
    })
}
/// `[goal]` section: the canonical home for `/goal` configuration. Field names
/// mirror the remote `goal_*` keys with the prefix dropped, so config and remote
/// stay 1:1. Per-key precedence is env > this config > remote > default.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planner_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_current_model_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verifier_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier_max_runs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategist_every: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverify_after: Option<u32>,
    #[serde(
        default,
        deserialize_with = "de_tolerant_goal_role_model",
        skip_serializing_if = "Option::is_none"
    )]
    pub planner_model: Option<crate::util::config::GoalRoleModel>,
    #[serde(
        default,
        deserialize_with = "de_tolerant_goal_role_model",
        skip_serializing_if = "Option::is_none"
    )]
    pub strategist_model: Option<crate::util::config::GoalRoleModel>,
    #[serde(
        default,
        deserialize_with = "de_tolerant_goal_role_models",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub skeptic_models: Vec<crate::util::config::GoalRoleModel>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}
/// `[auto_mode]` section: server-side configuration for Auto permission mode.
/// ONE struct serves both the local `[auto_mode]` TOML table and the remote
/// remote settings `auto_mode` JSON object (coerced via `serde_json::from_value`), so
/// the two stay 1:1. All fields are plain scalars/enums, so they deserialize
/// cleanly from both formats (no custom tolerant deser needed). Unset fields stay
/// `None` here; the wire fn applies the built-in defaults once auto mode is
/// enabled (current model, `low` effort if the model supports it, `just_command`
/// prompt). Precedence: local config > remote > those built-in defaults.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoModeConfig {
    /// The Auto-mode gate. Lowest-precedence layer of the gate chain (env and
    /// local `[auto_mode] enabled` config win over this remote value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// How much context the classifier prompt includes. `None` ⇒ the wire fn's
    /// built-in default (`just_command`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_type: Option<xai_grok_workspace::permission::ClassifierPromptType>,
    /// Routing slug for a dedicated classifier model. `None` ⇒ inherit the
    /// session model. Resolved via `resolve_aux_model_sampling_config`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier_model: Option<String>,
    /// Classifier side-query duration in milliseconds; resolved with bounded defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classify_timeout_ms: Option<u64>,
    /// Classifier reasoning effort. Applies on BOTH the routed-model path and the
    /// inherited session-model path; `None` ⇒ the wire fn's built-in default
    /// (`low` if the effective model supports reasoning effort, else unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Features {
    /// when set, the agent may ask permission for tool executions
    #[serde(default)]
    pub support_permission: bool,
    /// `None` = defer to remote settings / default (off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryMode>,
    /// Codebase graph indexing for go-to-definition/references.
    /// Accepts: true | false | ["glob", "!negative-glob", ...]
    /// Default: true (index any git repo). Patterns can explicitly match non-git directories.
    #[serde(default)]
    pub codebase_indexing: CodebaseIndexingSetting,
    /// Show a blocking warning when Grok starts outside a Git repository.
    /// Default: false. Used as the local fallback when the `non_git_warning` remote settings
    /// flag in `grok_build_settings` is absent. When the remote flag is present it takes
    /// precedence — `Some(false)` from remote settings overrides `true` here.
    #[serde(default)]
    pub non_git_warning: bool,
    /// Feedback system (heuristic popups + `/feedback` slash command).
    /// `None` = defer to remote settings / default (false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<bool>,
    /// Managed config fetching (managed_config.toml + requirements.toml).
    /// `None` = defer to env / default (true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_config: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp_tools: Option<bool>,
    /// Web fetch tool. `None` = defer to remote settings / env / default (false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_fetch: Option<bool>,
    /// Ask-user-question tool. `None` = defer to remote settings / env / default (true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_user_question: Option<bool>,
    /// Session recap (`/recap` + automatic return-from-away recap).
    /// `None` = defer to remote settings / env / default (`true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_recap: Option<bool>,
    /// Full-text index of past sessions, behind `/load` deep search. `None` = defer to remote
    /// settings / env / default (`true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_search: Option<bool>,
    /// Per-turn dashboard summary generated at turn end.
    /// `None` = defer to remote settings / env / default (`true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_summary: Option<bool>,
    /// Voice dictation (STT). `None` = env / remote / default on.
    /// Set `false` in requirements or managed config to force off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_mode: Option<bool>,
    /// Two-pass (prefire) compaction: speculatively summarize the history
    /// prefix in the background, then summarize NOTE₁ + recent tail at
    /// compaction. `None` = defer to remote settings / env / default (`false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub two_pass_compaction: Option<bool>,
    /// `image_gen` / `/imagine`. `None` = env / remote / default (`true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_gen: Option<bool>,
    /// Video tools / `/imagine-video`. `None` = env / remote / default (`true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_gen: Option<bool>,
    /// `image_gen` Imagine model override. `None`/empty = defer to remote settings
    /// (`image_gen_model_override`) / env / default (`grok-imagine-image-quality`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_gen_model_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_edit_model_override: Option<String>,
    /// Write file tool. `None` = defer to remote settings / env / default (true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_file: Option<bool>,
    /// Cancel-rewind: Ctrl+C before first activity restores the prompt.
    /// `None` = defer to remote settings / env / default (true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_rewind: Option<bool>,
    /// Auto-wake: immediately inject a synthetic prompt when a background
    /// task or subagent completes, instead of waiting for the idle drain.
    /// `None` = defer to remote settings / env / default (true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_wake: Option<bool>,
    /// TTSR-lite: mid-stream rule match + one retry injection.
    /// `None` = defer to env `GROK_TTSR_ENABLED` / default (false).
    /// Set under `[features] ttsr = true` in config.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttsr: Option<bool>,
    /// Advertise / prepare the `dap_debug` tool path for future DAP adapters.
    /// Today the tool is always a stub; this flag is reserved for enablement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dap_debug: Option<bool>,
    /// Backend-executed tools (web_search, x_search run server-side).
    /// `None` = defer to env / default (true). Set `false` to force
    /// client-side tool execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_tools: Option<bool>,
    /// `summary` (default) | `transcript` | `segments`. `None` = defer to CLI /
    /// env (`GROK_COMPACTION_MODE`). Parsed via `CompactionMode::parse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_mode: Option<String>,
    /// `none` | `minimal` | `balanced` | `verbose` (default). `None` = defer to
    /// env (`GROK_COMPACTION_DETAIL`). The `segments` verbatim detail level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_detail: Option<String>,
    /// Feed the summarizer the verbatim conversation instead of the lossy rewrite; `None` = defer to env/remote settings/default (true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_verbatim_input: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_tool_choice: Option<String>,
    /// Snapshot a completed subagent's isolated worktree into a durable git ref
    /// and delete its directory (resume rehydrates from the ref). This is the
    /// per-deployment rollout lever (set in managed_config.toml `[features]`).
    /// `None` = defer to remote settings / default (false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_worktree_snapshot: Option<bool>,
    /// Per-`Ready`-client transport-liveness pollers + the
    /// session-actor `StatusDispatcher`.
    ///
    /// When `true` (default), each successfully-handshaken MCP
    /// client gets a poller that detects rmcp service-loop
    /// termination and pushes `x.ai/mcp/server_status` updates to
    /// the client. When `false`, neither watchers nor the
    /// dispatcher are spawned — useful as an emergency kill switch
    /// for the rollout. `None` = defer to env / default (true).
    ///
    /// Not read through this struct: the live resolver re-reads the
    /// `[features]` key out-of-band from raw TOML in
    /// `util::config::resolve::mcp`. Declared so `serde_ignored`
    /// does not report it as an unrecognized key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_liveness_watchers: Option<bool>,
    /// Bounded stdio auto-restart task.
    ///
    /// When `true`, the session-actor `StatusDispatcher` reacts to
    /// `TransportClosed` / `HandshakeFailed` events on stdio MCP
    /// servers by scheduling up to 3 respawn attempts with
    /// `[1s, 4s, 16s]` backoff. HTTP / HttpAuth servers are NOT
    /// auto-restarted (their existing `reset_transport` path
    /// covers the recovery). `None` = defer to env / default
    /// (recovery is on by default; set `false` here / via
    /// `GROK_MCP_AUTO_RESTART` to opt out).
    ///
    /// Not read through this struct: the live resolver re-reads the
    /// `[features]` key out-of-band from raw TOML in
    /// `util::config::resolve::mcp`. Declared so `serde_ignored`
    /// does not report it as an unrecognized key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_auto_restart: Option<bool>,
    /// Pager-side subscription to the `x.ai/mcp/server_status` push.
    ///
    /// When `true` (default), the pager subscribes to the per-server
    /// status delta the shell emits via the dispatcher and
    /// patches the MCP servers modal in-place (no re-fetch round
    /// trip). When `false`, the pager ignores the push and falls
    /// back to the legacy `x.ai/mcp/tools_changed` debounced refetch
    /// path. `None` = defer to env / default (true).
    ///
    /// Not read through this struct. The pager-side gate
    /// (`acp_handler::push_server_status_enabled`) uses an
    /// **env-only** OnceLock cache via
    /// [`crate::util::config::resolve_mcp_push_server_status(None, None, None)`],
    /// which consults `BoolFlag::env` and the default `true`. The
    /// `[features]` key itself is honoured out-of-band, re-read from
    /// raw TOML in `util::config::resolve::mcp`. This field is
    /// declared so `serde_ignored` does not report the key as
    /// unrecognized.
    ///
    /// Practical consequence: setting
    /// `[features] mcp_push_server_status = false` in
    /// `~/.grok/config.toml` will NOT disable the pager's
    /// subscription on a freshly-launched process. To disable the
    /// pager subscription, set `GROK_MCP_PUSH_SERVER_STATUS=0` in
    /// the env before launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_push_server_status: Option<bool>,
    /// Whether the leader's `ConfigFileWatcher` adds the two narrow
    /// non-recursive watches for `<cwd>/` and `<cwd>/.grok/`.
    ///
    /// When `true` (default), edits to `<cwd>/.mcp.json`,
    /// `<cwd>/.grok/config.toml`, or `<cwd>/.claude.json` flow
    /// through the watcher → reloader → `ConfigUpdate::
    /// ProjectMcpServersChanged { cwd }` → `app.rs` ACP-injection
    /// pipeline and the affected sessions reload their MCP servers
    /// within the debounce window (~ 1 s). When `false`, the leader
    /// skips the cwd watches entirely and the only way to pick up a
    /// project-config edit is the user-triggered refresh button.
    ///
    /// The watches are **always non-recursive** — the name follows
    /// the convention for the rollout-gate flag. See
    /// `crate::config::watcher::ConfigFileWatcher::watch_path` for
    /// the inotify-quota rationale.
    ///
    /// The name is a documented misnomer — it gates
    /// the existence of the **cwd** watches, NOT their recursion
    /// mode. A future rename to `mcp_cwd_config_watch` would align
    /// name and behavior; deferred to a follow-up to avoid widening
    /// the config surface across requirements.toml / managed configs.
    ///
    /// Not read through this struct: the live resolver re-reads the
    /// `[features]` key out-of-band from raw TOML in
    /// `util::config::resolve::mcp`. Declared so `serde_ignored`
    /// does not report it as an unrecognized key.
    /// `None` = defer to env / default (true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_recursive_config_watch: Option<bool>,
}
/// Resolved credentials for a model session.
pub(crate) struct ResolvedCredentials {
    pub api_key: Option<String>,
    pub base_url: String,
    pub auth_type: xai_chat_state::AuthType,
    pub auth_scheme: AuthScheme,
}
/// First usable BYOK credential: a non-empty (trimmed) api_key, else the first
/// set, non-empty env_key value. Single source of truth for has_own_credentials,
/// resolve_credentials, and the JWT-reload path.
pub(crate) fn first_own_credential(
    api_key: Option<&str>,
    env_key: Option<&EnvKeys>,
) -> Option<String> {
    first_own_credential_with_source(api_key, env_key).map(|(v, _)| v)
}

/// Like [`first_own_credential`], but also returns the winning env var name
/// when the credential came from `env_key` (`None` source when from `api_key`).
pub(crate) fn first_own_credential_with_source(
    api_key: Option<&str>,
    env_key: Option<&EnvKeys>,
) -> Option<(String, Option<String>)> {
    if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        return Some((key.to_owned(), None));
    }
    env_key
        .and_then(|keys| keys.resolve_value_with_source(|name| std::env::var(name).ok()))
        .map(|(v, name)| (v, Some(name)))
}

/// `ANTHROPIC_AUTH_TOKEN` is a bearer credential (Claude Code / Pi convention);
/// the Anthropic API-key variables use `x-api-key`.
fn auth_scheme_for_env_source(source: Option<&str>, platform_default: AuthScheme) -> AuthScheme {
    match source {
        Some(name) if name == xai_grok_models::ANTHROPIC_AUTH_TOKEN_ENV => AuthScheme::Bearer,
        _ => platform_default,
    }
}

/// Priority: model api_key/env_key > cached auth-provider token > session
/// token > XAI_API_KEY.
pub(crate) fn resolve_credentials(
    model: &ModelEntry,
    session_key: Option<&str>,
) -> ResolvedCredentials {
    let info = model.info();
    let mut env_source: Option<String> = None;
    let (api_key, mut base_url, auth_type) = if let Some((key, source)) =
        first_own_credential_with_source(model.api_key.as_deref(), model.env_key.as_ref())
    {
        env_source = source;
        (
            Some(key),
            info.base_url.clone(),
            xai_chat_state::AuthType::ApiKey,
        )
    } else if let Some(provider) = model.auth_provider.as_ref() {
        debug_assert!(model.effective_auth_provider().is_some());
        (
            provider.cached_token(),
            info.base_url.clone(),
            xai_chat_state::AuthType::ApiKey,
        )
    } else if model.is_managed_platform_model() {
        // Managed platform entry without its platform credential: do NOT fall
        // through to the xAI session token / global key — that would send xAI
        // credentials to a third-party base URL. These models are locked in
        // the catalog projection and rejected at set_session_model; this arm
        // is the defense-in-depth seam for residual paths (session restore,
        // config races). The request fails unauthenticated instead.
        tracing::warn!(
            model = %info.model,
            "managed platform model has no platform credential; \
             refusing session/global key fallthrough"
        );
        (
            None,
            info.base_url.clone(),
            xai_chat_state::AuthType::ApiKey,
        )
    } else if let Some(key) = session_key {
        (
            Some(key.to_owned()),
            info.base_url.clone(),
            xai_chat_state::AuthType::SessionToken,
        )
    } else if let Ok(key) = crate::agent::auth_method::read_xai_api_key_env() {
        let url = model
            .api_base_url
            .clone()
            .unwrap_or_else(|| info.base_url.clone());
        (Some(key), url, xai_chat_state::AuthType::ApiKey)
    } else {
        if let Some(ref env_keys) = model.env_key
            && !env_keys.is_empty()
        {
            tracing::warn!(
                model = % info.model, env_key = % env_keys,
                "model has env_key configured but none of the environment variables are set — \
                 requests will have no API key",
            );
        }
        (
            None,
            info.base_url.clone(),
            xai_chat_state::AuthType::ApiKey,
        )
    };
    // Pi-style `…/coding` base 404s as `…/coding/messages`; Grok needs `…/v1`.
    if xai_grok_models::PlatformId::KimiCode.base_url_matches(&base_url) {
        base_url = xai_grok_models::normalize_kimi_code_base_url(&base_url);
    }
    let auth_scheme = auth_scheme_for_env_source(env_source.as_deref(), info.auth_scheme);
    tracing::debug!(
        model = %info.model,
        auth_type = ?auth_type,
        "resolved credentials"
    );
    ResolvedCredentials {
        api_key,
        base_url,
        auth_type,
        auth_scheme,
    }
}
/// `disable_api_key_auth` at the credential seam: swap a first-party xAI API
/// key for the IdP session (absent => request fails => forces login). BYOK
/// (non-xAI `base_url`) is untouched; no-op when the switch is off.
pub(crate) fn enforce_disable_api_key_auth(
    creds: &mut ResolvedCredentials,
    disable_api_key_auth: bool,
    session_key: Option<&str>,
) {
    if disable_api_key_auth
        && creds.auth_type == xai_chat_state::AuthType::ApiKey
        && crate::util::is_xai_api_url(&creds.base_url)
    {
        creds.auth_type = xai_chat_state::AuthType::SessionToken;
        creds.api_key = session_key.map(str::to_owned);
        xai_grok_telemetry::unified_log::debug(
            "auth: kill switch blocked a first-party API key at the credential seam",
            None,
            Some(serde_json::json!({
                "replaced_with_session": session_key.is_some(),
                "base_url": creds.base_url,
            })),
        );
    }
}
/// Resolve credentials for an auxiliary sampling path (web search, image
/// description) with the first-party API-key kill switch applied, so these
/// paths honor `disable_api_key_auth` exactly like the main chat path.
fn resolve_credentials_enforced(
    entry: &ModelEntry,
    session_key: Option<&str>,
    disable_api_key_auth: bool,
) -> ResolvedCredentials {
    let mut credentials = resolve_credentials(entry, session_key);
    enforce_disable_api_key_auth(&mut credentials, disable_api_key_auth, session_key);
    credentials
}
pub use xai_grok_telemetry::config::deployment_id_from_key;
/// Try to resolve credentials for a model by loading the effective config.
/// Returns `None` (with a warning) if config loading, parsing, or model
/// lookup fails. `session_key` should only be passed when `auth_type` is
/// `SessionToken` — callers must guard this.
pub(crate) fn try_resolve_model_credentials(
    model_id: &str,
    session_key: Option<&str>,
) -> Option<ResolvedCredentials> {
    let raw = crate::config::load_effective_config()
        .map_err(|e| tracing::warn!(error = %e, "config load failed for credential resolution"))
        .ok()?;
    let cfg = Config::new_from_toml_cfg(&raw)
        .map_err(|e| tracing::warn!(error = %e, "config parse failed for credential resolution"))
        .ok()?;
    let models = resolve_model_list(&cfg, None);
    let entry = find_model_by_id(&models, model_id)?;
    let mut credentials = resolve_credentials(entry, session_key);
    enforce_disable_api_key_auth(
        &mut credentials,
        cfg.grok_com_config.api_key_auth_disabled(),
        session_key,
    );
    Some(credentials)
}
/// Per-model auth facts (BYOK status + auth scheme) from one effective-config
/// load, memoized by the session actor.
#[derive(Clone, Copy)]
pub(crate) struct ModelAuthFacts {
    pub byok: ModelByok,
    pub auth_scheme: AuthScheme,
    /// Stable catalog route identity for OAuth-capable providers.
    pub oauth_platform: Option<xai_grok_models::PlatformId>,
    /// Whether a hybrid provider selected OAuth rather than its static key.
    pub platform_oauth_active: bool,
}
/// Resolve `model_id` to its auth facts and auth-provider reference from one
/// effective-config load; both ride the same memo (see
/// `SessionActor::model_auth_memo`). Load/parse failure → `byok = Unknown`;
/// model absent from the catalog → `NotByok`. An empty `model_id` (no sampling
/// config yet) → `Unknown`, not `NotByok`, so the gate isn't activated for an
/// unidentified model.
pub(crate) fn resolve_model_auth_facts_and_provider(
    model_id: &str,
) -> (ModelAuthFacts, Option<crate::auth::AuthProviderRef>) {
    if model_id.is_empty() {
        return (
            ModelAuthFacts {
                byok: ModelByok::Unknown,
                auth_scheme: AuthScheme::default(),
                oauth_platform: None,
                platform_oauth_active: false,
            },
            None,
        );
    }
    with_resolved_model(model_id, |lookup| {
        let facts = ModelAuthFacts {
            byok: byok_from_lookup(&lookup),
            auth_scheme: match lookup {
                ModelLookup::Loaded(Some(e)) => e.info().auth_scheme,
                _ => AuthScheme::default(),
            },
            oauth_platform: match lookup {
                ModelLookup::Loaded(Some(e)) => oauth_platform_for_model(e),
                _ => None,
            },
            platform_oauth_active: match lookup {
                ModelLookup::Loaded(Some(e)) => e.platform_oauth_active,
                _ => false,
            },
        };
        let provider = match lookup {
            ModelLookup::Loaded(Some(e)) => e.effective_auth_provider().cloned(),
            _ => None,
        };
        (facts, provider)
    })
}
fn byok_from_lookup(lookup: &ModelLookup) -> ModelByok {
    match lookup {
        ModelLookup::ConfigUnavailable => ModelByok::Unknown,
        ModelLookup::Loaded(Some(e)) if e.has_own_credentials() => ModelByok::Byok,
        ModelLookup::Loaded(_) => ModelByok::NotByok,
    }
}
enum ModelLookup<'a> {
    /// `None` if `model_id` is absent from the catalog.
    Loaded(Option<&'a ModelEntry>),
    ConfigUnavailable,
}
/// Load + parse the effective config and hand the `model_id` lookup to `f`,
/// keeping "config unavailable" distinct from "model absent" so callers can
/// stay conservative on a transient config failure.
fn with_resolved_model<T>(model_id: &str, f: impl FnOnce(ModelLookup) -> T) -> T {
    with_resolved_model_loader(
        model_id,
        || {
            let raw = crate::config::load_effective_config()
                .map_err(|e| tracing::warn!(error = %e, "config load failed for model auth lookup"))
                .ok()?;
            Config::new_from_toml_cfg(&raw)
                .map_err(
                    |e| tracing::warn!(error = %e, "config parse failed for model auth lookup"),
                )
                .ok()
        },
        f,
    )
}

fn with_resolved_model_loader<T>(
    model_id: &str,
    load: impl FnOnce() -> Option<Config> + Send,
    f: impl FnOnce(ModelLookup) -> T,
) -> T {
    // Load + parse the effective config on a dedicated, enlarged-stack thread.
    // `Config` is a very large struct, and its serde-derived `Deserialize`
    // (wrapped by `serde_ignored` and driven by `toml`) allocates large stack
    // frames per nesting level — deserializing the full effective config
    // consumes well over the 2 MB stack of a default tokio worker / test
    // thread, overflowing it (observed as a stack overflow in the session
    // tests; a latent risk on any 2 MB production worker). Parsing on an
    // 8 MB-stack scoped thread keeps that cost off the caller's stack limit.
    let cfg = std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(s, load)
            .expect("spawn scoped config-parse thread")
            .join()
            .expect("config-parse thread panicked")
    });
    let Some(cfg) = cfg else {
        return f(ModelLookup::ConfigUnavailable);
    };
    let models = resolve_model_list(&cfg, None);
    f(ModelLookup::Loaded(find_model_by_id(&models, model_id)))
}
/// Resolve a standalone `SamplerConfig` for an auxiliary model slug (image
/// description, session summary, ...), resolved through the catalog so a
/// `[model.*]` override redirects it to its own endpoint, credentials, and
/// routing `model`. `None` → caller falls back to the active session's model.
pub(crate) fn resolve_aux_model_sampling_config(
    model_id: &str,
    models: &IndexMap<String, ModelEntry>,
    endpoints: &EndpointsConfig,
    session_key: Option<&str>,
    disable_api_key_auth: bool,
    alpha_test_key: Option<String>,
    client_version: Option<String>,
) -> Option<SamplerConfig> {
    let catalog_entry = find_model_by_id(models, model_id).cloned();
    if let Some(entry) = &catalog_entry {
        let credentials = resolve_credentials_enforced(entry, session_key, disable_api_key_auth);
        let sampler = sampling_config_for_model(
            entry,
            credentials,
            alpha_test_key.clone(),
            client_version.clone(),
            None,
            None,
        );
        if sampler.api_key.is_some() {
            return Some(sampler);
        }
        if entry.effective_auth_provider().is_some() {
            tracing::warn!(
                model = %model_id,
                "aux model uses an auth provider with no cached token; the caller falls back to its session default"
            );
            return None;
        }
    }
    let xai_bearer = session_key
        .map(|s| s.to_owned())
        .or_else(|| crate::agent::auth_method::read_xai_api_key_env().ok())
        .or_else(|| endpoints.deployment_key.clone());
    if let Some(bearer) = xai_bearer {
        let entry = ModelEntry {
            info: ModelInfo {
                user_selectable: true,
                id: None,
                model: catalog_entry
                    .map(|e| e.info.model)
                    .unwrap_or_else(|| model_id.to_owned()),
                base_url: endpoints.resolve_inference_base_url(),
                name: None,
                description: None,
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: ApiBackend::Responses,
                request_compat: None,
                endpoint_path: None,
                auth_scheme: Default::default(),
                extra_headers: IndexMap::new(),
                query_params: IndexMap::new(),
                env_http_headers: IndexMap::new(),
                context_window: NonZeroU64::new(200_000).unwrap(),
                auto_compact_threshold_percent: None,
                system_prompt_label: None,
                use_concise: false,
                agent_type: default_agent_type(),
                inference_idle_timeout_secs: None,
                max_retries: None,
                hidden: true,
                supported_in_api: true,
                reasoning_effort: None,
                supports_reasoning_effort: false,
                reasoning_efforts: Vec::new(),
                supports_backend_search: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                show_model_fingerprint: false,
                stream_tool_calls: None,
                laziness_detector: LazinessDetectorPerModelConfig::default(),
            },
            api_key: Some(bearer),
            env_key: None,
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: None,
        };
        let credentials = resolve_credentials_enforced(&entry, session_key, disable_api_key_auth);
        let sampler = sampling_config_for_model(
            &entry,
            credentials,
            alpha_test_key,
            client_version,
            None,
            None,
        );
        return Some(sampler);
    }
    tracing::warn!(
        aux_model = %model_id,
        "no credentials for auxiliary model; falling back to active model",
    );
    None
}
/// Stamp the session-local fields (client id, attribution, bearer resolver,
/// retries) from the active session onto a routed aux `SamplerConfig` so a
/// helper model keeps the session's auth/attribution. Shared by image-describe
/// and the auto-mode classifier so the two can't drift.
///
/// The resolver gate is host-based, stricter than `session_token_auth_gate`:
/// a session-token deployment on a custom `models_base_url` loses aux-sampler
/// refresh, rather than risk the session bearer on a third-party endpoint.
pub(crate) fn stamp_session_local_sampler_fields(
    cfg: &mut SamplerConfig,
    active_session_config: &SamplerConfig,
    client_identifier: Option<String>,
    max_retries: Option<u32>,
) {
    cfg.client_identifier = client_identifier;
    cfg.attribution_callback = active_session_config.attribution_callback.clone();
    if crate::util::is_xai_api_bearer_url(&cfg.base_url) {
        cfg.bearer_resolver = active_session_config.bearer_resolver.clone();
    }
    cfg.max_retries = max_retries;
}
/// Finalize image-describe model + sampler config for user attachments.
/// Shared so the aux resolve happy path and the `None` fallback cannot
/// diverge between those entry points.
///
/// On aux resolve `Some`, stamp session-local fields onto the helper config.
/// On `None`, fall back to the active session model and full config (not
/// forcing `image_description_model` onto the agent endpoint, which 404s on
/// BYOK / non-proxy routes for internal slugs like `grok-build`).
pub(crate) fn finalize_image_describe_sampler_config(
    resolved_aux: Option<SamplerConfig>,
    active_session_config: &SamplerConfig,
    client_identifier: Option<String>,
    max_retries: Option<u32>,
) -> (String, SamplerConfig) {
    match resolved_aux {
        Some(mut describe_cfg) => {
            stamp_session_local_sampler_fields(
                &mut describe_cfg,
                active_session_config,
                client_identifier,
                max_retries,
            );
            let model = describe_cfg.model.clone();
            (model, describe_cfg)
        }
        None => {
            let model = active_session_config.model.clone();
            (model, active_session_config.clone())
        }
    }
}
/// Re-derive `auth_type` from the model's own credentials so BYOK env-key
/// models stay on `ApiKey` even when a session token is present. Falls
/// back to `fallback` when the model isn't in the on-disk catalog.
pub(crate) fn resolve_chat_state_auth_type(
    model_id: &str,
    session_key: Option<&str>,
    fallback: xai_chat_state::AuthType,
) -> xai_chat_state::AuthType {
    try_resolve_model_credentials(model_id, session_key)
        .map(|r| r.auth_type)
        .unwrap_or(fallback)
}
/// Resolve provider adapter metadata from a stable managed catalog id.
/// User-defined and remote models fall back to the standard wire adapter.
pub fn adapter_kind_for_model(model: &ModelEntry) -> xai_grok_models::AdapterKind {
    if model.info.api_backend == ApiBackend::CodexResponses {
        return xai_grok_models::AdapterKind::OpenAiCodex;
    }
    let catalog_id = model
        .info
        .id
        .as_deref()
        .unwrap_or(model.info.model.as_str());
    let Some((provider, model_id)) = catalog_id.split_once('/') else {
        return xai_grok_models::AdapterKind::Standard;
    };
    if model_id.is_empty() {
        return xai_grok_models::AdapterKind::Standard;
    }
    xai_grok_models::provider_spec(provider)
        .filter(|spec| spec.id.as_str() == provider)
        .map(|spec| spec.adapter)
        .unwrap_or_default()
}

/// Selects xAI-only Responses extensions for trusted backend-search routes.
///
/// Third-party Responses providers reject `no_inline_citations`, so it must stay
/// on a trusted first-party route and apply only to models with backend search.
pub(crate) fn response_include_extensions(
    supports_backend_search: bool,
    api_backend: &ApiBackend,
    base_url: &str,
) -> Vec<String> {
    let is_trusted_route = crate::util::is_trusted_cli_chat_proxy_url(base_url)
        || crate::util::is_trusted_xai_https_url(base_url);
    if supports_backend_search && api_backend == &ApiBackend::Responses && is_trusted_route {
        vec![NO_INLINE_CITATIONS_RESPONSE_INCLUDE.to_owned()]
    } else {
        Vec::new()
    }
}

pub(crate) fn sampling_config_for_model(
    model: &ModelEntry,
    credentials: ResolvedCredentials,
    alpha_test_key: Option<String>,
    client_version: Option<String>,
    deployment_id: Option<String>,
    user_id: Option<String>,
) -> SamplerConfig {
    let info = model.info();
    let model_name = info.model.clone();
    let max_completion_tokens = info.max_completion_tokens;
    let temperature = info.temperature;
    let top_p = info.top_p;
    let mut extra_headers = info.extra_headers.clone();
    inject_url_derived_headers(
        &mut extra_headers,
        alpha_test_key.as_deref(),
        &credentials.base_url,
    );
    let route_oauth_platform = oauth_platform_for_model(model);
    align_oauth_headers_with_platform(
        &mut extra_headers,
        route_oauth_platform,
        &credentials.base_url,
    );
    // Hybrid Kimi: static own API key must not carry OAuth device identity.
    // Keep `anthropic-version` (Messages protocol); only strip x-msh-device-*.
    if route_oauth_platform == Some(xai_grok_models::PlatformId::KimiCode)
        && !model_uses_kimi_code_oauth(model)
    {
        remove_kimi_device_headers(&mut extra_headers);
    }
    let api_backend = info.api_backend.clone();
    // Custom `api_backend = "codex_responses"` forces the Codex adapter even
    // when the catalog id is not `openai-codex/*` (BYOK reverse proxies).
    let adapter_kind = if api_backend.uses_codex_dialect() {
        xai_grok_models::AdapterKind::OpenAiCodex
    } else {
        adapter_kind_for_model(model)
    };
    // Kimi Code access tokens ~15m; re-resolve (and refresh) on every request
    // so a catalog stamp from login is never sent after expiry.
    let bearer_resolver = kimi_code_bearer_resolver_for_model(model)
        .or_else(|| openai_codex_bearer_resolver_for_model(model))
        .or_else(|| anthropic_claude_bearer_resolver_for_model(model))
        .or_else(|| radius_bearer_resolver_for_model(model))
        .or_else(|| {
            (model_uses_github_copilot_oauth(model)
                && (model.platform_oauth_active || credentials.api_key.is_none()))
            .then(|| {
                Arc::new(crate::auth::github_copilot::GitHubCopilotBearerResolver)
                    as SharedBearerResolver
            })
        });
    let responses_codex_dialect =
        model_uses_openai_codex_oauth(model) || api_backend.uses_codex_dialect();
    let kimi_dialect = model_uses_kimi_request_dialect(model);
    let bedrock_profile = (adapter_kind == xai_grok_models::AdapterKind::BedrockConverseStream
        && credentials.api_key.is_none())
    .then(|| crate::auth::read_bedrock_profile(&xai_grok_config::grok_home()))
    .flatten();
    // The Codex bearer resolver returns `chatgpt-account-id` from the same
    // live credential resolution; no second per-request header lookup is needed.
    let extra_response_includes = response_include_extensions(
        info.supports_backend_search,
        &api_backend,
        &credentials.base_url,
    );

    SamplerConfig {
        api_key: credentials.api_key,
        model: model_name,
        base_url: credentials.base_url,
        max_completion_tokens,
        temperature,
        top_p,
        api_backend,
        adapter_kind,
        request_compat: info.request_compat.clone(),
        endpoint_path: info.endpoint_path.clone(),
        auth_scheme: credentials.auth_scheme,
        extra_headers,
        extra_response_includes,
        query_params: info.query_params.clone(),
        env_http_headers: info.env_http_headers.clone(),
        context_window: info.context_window.get(),
        client_version,
        reasoning_effort: info.reasoning_effort,
        force_http1: false,
        max_retries: info.max_retries,
        stream_tool_calls: info.stream_tool_calls.unwrap_or(false),
        idle_timeout_secs: None,
        client_identifier: None,
        deployment_id,
        user_id,
        origin_client: None,
        attribution_callback: None,
        bearer_resolver,
        supports_backend_search: info.supports_backend_search,
        compactions_remaining: info.compactions_remaining,
        compaction_at_tokens: info.compaction_at_tokens,
        doom_loop_recovery: None,
        header_injector: None,
        responses_codex_dialect,
        bedrock_request_metadata: Default::default(),
        bedrock_headers: Default::default(),
        bedrock_profile,
        kimi_dialect,
    }
}

/// Resolve the OAuth platform from stable catalog identity first.
///
/// URL matching is only a fallback for legacy unqualified entries, and is
/// rejected when Kimi and Codex share one configured reverse-proxy origin.
pub(crate) fn oauth_platform_for_model(model: &ModelEntry) -> Option<xai_grok_models::PlatformId> {
    let catalog_id = model
        .info
        .id
        .as_deref()
        .unwrap_or(model.info.model.as_str());
    if let Some((provider, _)) = xai_grok_models::parse_managed_model_key(catalog_id)
        && let Some(platform) = provider.platform_id()
    {
        return platform.uses_oauth().then_some(platform);
    }

    oauth_platform_for_base_url(&model.info.base_url)
}

/// Resolve an OAuth platform from a URL only when exactly one provider owns it.
/// Shared user-configured proxies deliberately return `None` rather than
/// guessing which credential family to send.
pub(crate) fn oauth_platform_for_base_url(base_url: &str) -> Option<xai_grok_models::PlatformId> {
    use xai_grok_models::PlatformId;

    // Anthropic Claude OAuth shares api.anthropic.com with the `Anthropic`
    // BYOK platform; the catalog id (`anthropic-claude/*`), not the URL,
    // distinguishes them, so this URL-only helper does not resolve Claude.
    match (
        PlatformId::KimiCode.base_url_matches(base_url),
        PlatformId::OpenAiCodex.base_url_matches(base_url),
    ) {
        (true, false) => Some(PlatformId::KimiCode),
        (false, true) => Some(PlatformId::OpenAiCodex),
        _ => None,
    }
}

/// Whether this catalog entry routes through Anthropic Claude (Pro/Max) OAuth.
pub fn model_uses_anthropic_claude_oauth(model: &ModelEntry) -> bool {
    let catalog_id = model
        .info
        .id
        .as_deref()
        .unwrap_or(model.info.model.as_str());
    xai_grok_models::parse_managed_model_key(catalog_id).is_some_and(|(provider, _)| {
        provider.platform_id() == Some(xai_grok_models::PlatformId::AnthropicClaude)
    })
}

/// Per-request bearer for Anthropic Claude models; `None` for everything else.
/// The resolver also stamps `anthropic-beta: oauth-2025-04-20`.
pub fn anthropic_claude_bearer_resolver_for_model(
    model: &ModelEntry,
) -> Option<SharedBearerResolver> {
    if !model_uses_anthropic_claude_oauth(model) {
        return None;
    }
    Some(
        Arc::new(crate::auth::anthropic_claude::AnthropicClaudeBearerResolver)
            as SharedBearerResolver,
    )
}

/// Whether this catalog entry routes through Radius OAuth.
pub fn model_uses_radius_oauth(model: &ModelEntry) -> bool {
    let catalog_id = model
        .info
        .id
        .as_deref()
        .unwrap_or(model.info.model.as_str());
    xai_grok_models::parse_managed_model_key(catalog_id)
        .is_some_and(|(provider, _)| provider.as_str() == "radius")
}

/// Per-request bearer for Radius models; only installed for OAuth catalog markers.
pub fn radius_bearer_resolver_for_model(model: &ModelEntry) -> Option<SharedBearerResolver> {
    if !model_uses_radius_oauth(model) || !model.platform_oauth_active {
        return None;
    }
    Some(Arc::new(crate::auth::radius::RadiusBearerResolver) as SharedBearerResolver)
}

/// Whether this catalog entry routes through GitHub Copilot OAuth.
pub fn model_uses_github_copilot_oauth(model: &ModelEntry) -> bool {
    let catalog_id = model
        .info
        .id
        .as_deref()
        .unwrap_or(model.info.model.as_str());
    xai_grok_models::parse_managed_model_key(catalog_id)
        .is_some_and(|(provider, _)| provider.as_str() == "github-copilot")
}

/// Whether this catalog entry should use Kimi Code OAuth (live bearer resolver).
///
/// Kimi is a **hybrid** provider (static API key *or* subscription OAuth), so
/// this is not pure catalog identity (unlike OpenAI Codex):
///
/// * **Static own credential** (`has_own_credentials && !platform_oauth_active`)
///   → `false`: keep the stamped API key, do not install
///   [`KimiCodeBearerResolver`](crate::auth::kimi::KimiCodeBearerResolver)
///   (the resolver would strip static auth and force OAuth device headers).
/// * **OAuth active** (`platform_oauth_active`) → `true`.
/// * **No own credential yet** (pre-login / empty auth.json) → `true` on
///   catalog identity alone, so a session that selected `kimi-code/*` before
///   `/login kimi` still installs the live resolver after restamp (avoids
///   the post-login stale-memo regression that pure `platform_oauth_active`
///   gating reintroduced for Codex and would reintroduce here).
pub fn model_uses_kimi_code_oauth(model: &ModelEntry) -> bool {
    if oauth_platform_for_model(model) != Some(xai_grok_models::PlatformId::KimiCode) {
        return false;
    }
    // Static BYOK path: own key stamped and OAuth not selected.
    if model.has_own_credentials() && !model.platform_oauth_active {
        return false;
    }
    true
}

/// Whether request bodies should use Moonshot/Kimi-specific shaping
/// (`thinking` object, fixed-sampling strip, etc.).
///
/// True only for Kimi Code subscription and direct Moonshot open-platform
/// entries. Ollama / OpenRouter / Together / Fireworks models that share
/// the same bare slug must not trigger this dialect.
pub fn model_uses_kimi_request_dialect(model: &ModelEntry) -> bool {
    use xai_grok_models::PlatformId;
    let catalog_id = model
        .info
        .id
        .as_deref()
        .unwrap_or(model.info.model.as_str());
    if let Some((provider, _)) = xai_grok_models::parse_managed_model_key(catalog_id) {
        return matches!(
            provider.platform_id(),
            Some(PlatformId::KimiCode | PlatformId::MoonshotCn | PlatformId::MoonshotAi)
        );
    }
    PlatformId::KimiCode.base_url_matches(&model.info.base_url)
        || PlatformId::MoonshotCn.base_url_matches(&model.info.base_url)
        || PlatformId::MoonshotAi.base_url_matches(&model.info.base_url)
}

/// Per-request bearer for Kimi Code models; `None` for everything else.
pub fn kimi_code_bearer_resolver_for_model(model: &ModelEntry) -> Option<SharedBearerResolver> {
    if !model_uses_kimi_code_oauth(model) {
        return None;
    }
    Some(Arc::new(crate::auth::kimi::KimiCodeBearerResolver) as SharedBearerResolver)
}

/// Same as [`kimi_code_bearer_resolver_for_model`] but from a bare routing
/// slug + base URL (session reconstruct path may not have the full entry).
pub fn kimi_code_bearer_resolver_for_base_url(base_url: &str) -> Option<SharedBearerResolver> {
    if oauth_platform_for_base_url(base_url) != Some(xai_grok_models::PlatformId::KimiCode) {
        return None;
    }
    Some(Arc::new(crate::auth::kimi::KimiCodeBearerResolver) as SharedBearerResolver)
}

/// Whether this catalog entry routes through OpenAI Codex (ChatGPT) OAuth.
///
/// Catalog identity alone — not `platform_oauth_active`. The live bearer
/// resolver re-reads `auth.json` per request; gating on the stamp flag
/// regressed post-login turns (see session `codex_oauth_route`).
pub fn model_uses_openai_codex_oauth(model: &ModelEntry) -> bool {
    oauth_platform_for_model(model) == Some(xai_grok_models::PlatformId::OpenAiCodex)
}

/// Per-request bearer for OpenAI Codex models; `None` for everything else.
pub fn openai_codex_bearer_resolver_for_model(model: &ModelEntry) -> Option<SharedBearerResolver> {
    if !model_uses_openai_codex_oauth(model) {
        return None;
    }
    Some(Arc::new(crate::auth::openai_codex::OpenAiCodexBearerResolver) as SharedBearerResolver)
}

/// Same as [`openai_codex_bearer_resolver_for_model`] but from a bare base URL.
pub fn openai_codex_bearer_resolver_for_base_url(base_url: &str) -> Option<SharedBearerResolver> {
    if oauth_platform_for_base_url(base_url) != Some(xai_grok_models::PlatformId::OpenAiCodex) {
        return None;
    }
    Some(Arc::new(crate::auth::openai_codex::OpenAiCodexBearerResolver) as SharedBearerResolver)
}

/// Managed catalog provider (`ollama/*`, `openrouter/*`, …) that authenticates
/// with a static API key (not OAuth subscription).
///
/// Used together with [`open_platform_endpoint`] so a customized `base_url`
/// (local Ollama `http://127.0.0.1:11434/v1`, reverse proxy, etc.) still
/// fail-closes the xAI session bearer: URL matching alone cannot recognize
/// those hosts, but the `provider/model` catalog id is unambiguous.
pub fn managed_api_key_provider(model_id: &str) -> Option<&'static xai_grok_models::ProviderSpec> {
    let (provider_id, _) = xai_grok_models::parse_managed_model_key(model_id)?;
    let spec = xai_grok_models::provider_spec(provider_id.as_str())?;
    // Static API-key family only. OAuth / hybrid providers have their own
    // resolver branches and must not be treated as plain BYOK here.
    if spec.accepts_api_key() && !spec.uses_oauth() {
        Some(spec)
    } else {
        None
    }
}

/// True when this turn must never install the xAI session [`WireValidBearerResolver`](crate::auth)
/// or fall through to the session JWT.
///
/// Official `[model.*]` BYOK is any non–first-party `base_url` plus the model's
/// own `api_key`/`env_key`. Hyper previously only recognized registry hosts
/// (`ollama.com`, OpenRouter, …) and managed catalog ids (`ollama/*`). A
/// user `[model.foo]` pointed at vLLM / LiteLLM / a reverse proxy then
/// classified `NotByok`, installed the xAI JWT, 401'd, and looped on a
/// false "recovery succeeded" refresh of the *wrong* credential.
pub fn is_third_party_api_key_route(model_id: &str, base_url: &str) -> bool {
    !crate::util::is_xai_api_url(base_url)
        || open_platform_endpoint(base_url).is_some()
        || managed_api_key_provider(model_id).is_some()
}

/// JWT access tokens start with a shared base64 header (`eyJ…`). Never forward
/// them as third-party BYOK credentials (OpenCode Go would answer 401
/// `Invalid API key`).
pub(crate) fn looks_like_jwt_access_token(key: &str) -> bool {
    let key = key.trim();
    key.starts_with("eyJ") && key.bytes().filter(|&b| b == b'.').count() >= 2
}

/// Whether a catalog entry's base URL is the same platform route as `request_base`.
///
/// Used when looking up credentials by bare wire model id so
/// `deepseek-v4-flash` under Ollama cannot supply `OLLAMA_API_KEY` for an
/// OpenCode Go request to `https://opencode.ai/zen/go/v1`.
pub(crate) fn catalog_base_matches_request(catalog_base: &str, request_base: &str) -> bool {
    let catalog_base = catalog_base
        .trim()
        .trim_end_matches('/')
        .to_ascii_lowercase();
    let request_base = request_base
        .trim()
        .trim_end_matches('/')
        .to_ascii_lowercase();
    if catalog_base.is_empty() || request_base.is_empty() {
        return false;
    }
    request_base == catalog_base
        || request_base
            .strip_prefix(&catalog_base)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || catalog_base
            .strip_prefix(&request_base)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Re-resolve a third-party platform API key from the **request base URL**
/// (auth.json `platform/<id>` + env), independent of the bare wire model slug.
///
/// `sampling_config.model` is the provider wire id (e.g. `deepseek-v4-flash`),
/// not the catalog key (`opencode-go/deepseek-v4-flash`). Looking up the bare
/// slug can hit a different catalog row (e.g. `ollama/deepseek-v4-flash`) that
/// has no OpenCode key, leaving reconstruct to reuse a stale chat-state JWT.
/// Matching by host/path (`opencode.ai/zen/go/v1`) recovers the correct key.
pub fn resolve_open_platform_api_key_from_endpoint(base_url: &str) -> Option<String> {
    let spec = open_platform_endpoint(base_url)?;
    // Prefer the live `$GROK_HOME` env over `grok_home()`'s process-once cache.
    // Tests (and rare mid-process overrides) set `GROK_HOME` per case; the
    // OnceLock would otherwise keep the first resolution and miss auth.json.
    let home = std::env::var_os("GROK_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(xai_grok_config::grok_home);
    for storage_id in provider_credential_storage_ids(spec) {
        if let Some(key) = crate::auth::read_platform_api_key(&home, &storage_id) {
            return Some(key);
        }
    }
    for name in &spec.credentials.env_keys {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

/// Third-party BYOK (api-key) platform owning `base_url`, if any.
///
/// Excludes first-party xAI Direct (`api.x.ai` — xAI session recovery is
/// legitimate there) and the OAuth platforms (Kimi Code / OpenAI Codex have
/// their own bearer-resolver branches upstream). The session reconstruct and
/// 401-recovery paths use this to keep the xAI session bearer off third-party
/// hosts: a live-only catalog entry (e.g. `ollama/glm-5.2` from the platform
/// `/models` sync) is absent from the offline catalog the BYOK memo consults,
/// so it misclassifies as `NotByok` and the session gate would otherwise sign
/// the request with the xAI session JWT → third-party 401 → a false
/// "auth recovery succeeded" loop refreshing the wrong credential.
pub fn open_platform_endpoint(base_url: &str) -> Option<&'static xai_grok_models::ProviderSpec> {
    fn prefix_score(provider_base: &str, request_base: &str) -> usize {
        let provider_base = provider_base
            .trim()
            .trim_end_matches('/')
            .to_ascii_lowercase();
        let request_base = request_base
            .trim()
            .trim_end_matches('/')
            .to_ascii_lowercase();
        if request_base == provider_base
            || request_base
                .strip_prefix(&provider_base)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            provider_base.len()
        } else {
            0
        }
    }

    xai_grok_models::provider_registry()
        .providers()
        .iter()
        .filter(|provider| {
            provider.id.as_str() != xai_grok_models::PlatformId::XaiDirect.as_str()
                && provider.status == xai_grok_models::ProviderStatus::Active
                && provider.accepts_api_key()
                && provider.base_url_matches(base_url)
        })
        // Multiple products may share a host (OpenCode Zen and Go). Prefer the
        // most specific configured path while retaining host-only fallback for
        // custom reverse proxies so the xAI session bearer remains fail-closed.
        .max_by_key(|provider| prefix_score(&provider.base_url(), base_url))
}
/// Fold URL-derived headers into `extra_headers`.
///
/// The sampler crate is intentionally URL-agnostic: it does not inspect
/// `base_url` to decide which auth or staging headers to add. Replicate the
/// URL-derived header logic at the shell boundary so callers downstream see a
/// single homogenous header bag.
///
/// * cli-chat-proxy bases get `X-XAI-Token-Auth` and
///   `x-authenticateresponse` headers (mirrors the inline match in the legacy
///   `sampling::Client::new` on `is_cli_chat_proxy_url`).
/// * With the optional non-production feature, matching first-party hosts may
///   get an extra access header from the corresponding key argument.
///
/// Existing entries are never overwritten so callers can pre-set a value.
pub(crate) fn inject_url_derived_headers(
    headers: &mut IndexMap<String, String>,
    alpha_test_key: Option<&str>,
    base_url: &str,
) {
    // Anthropic Messages (and Kimi Code Messages) require anthropic-version.
    if xai_grok_models::PlatformId::Anthropic.base_url_matches(base_url)
        || xai_grok_models::PlatformId::KimiCode.base_url_matches(base_url)
    {
        headers
            .entry("anthropic-version".to_string())
            .or_insert_with(|| xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE.to_string());
    }
    if crate::util::is_cli_chat_proxy_url(base_url) {
        headers
            .entry("X-XAI-Token-Auth".to_string())
            .or_insert_with(|| "xai-grok-cli".to_string());
        headers
            .entry("x-authenticateresponse".to_string())
            .or_insert_with(|| "authenticate-response".to_string());
        headers
            .entry(crate::http::CLIENT_MODE_HEADER.to_string())
            .or_insert_with(|| crate::http::process_client_mode().to_string());
    }
    // Kimi Code subscription inference expects device-identity headers
    // (same as OAuth). Best-effort: skip on failure (e.g. read-only home).
    if xai_grok_models::PlatformId::KimiCode.base_url_matches(base_url) {
        match crate::auth::kimi::device_headers() {
            Ok(device_headers) => {
                for (name, value) in device_headers {
                    headers.entry(name.to_string()).or_insert(value);
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "auth: could not attach Kimi device identity headers"
                );
            }
        }
    }
    // ChatGPT Codex requires the Responses-beta and originator headers. The
    // live bearer resolver adds `chatgpt-account-id` from the exact same auth
    // resolution as the token; remove any carried value here so a stale account
    // can never survive when live auth fails.
    if xai_grok_models::PlatformId::OpenAiCodex.base_url_matches(base_url) {
        headers
            .entry("OpenAI-Beta".to_string())
            .or_insert_with(|| "responses=experimental".to_string());
        headers
            .entry("originator".to_string())
            .or_insert_with(|| "grok-build".to_string());
        headers.shift_remove("chatgpt-account-id");
    }
    let _ = (alpha_test_key, base_url);
}

/// Remove Kimi OAuth device identity while retaining protocol headers needed
/// by Kimi's API-key Messages route.
pub(crate) fn remove_kimi_device_headers(headers: &mut IndexMap<String, String>) {
    const DEVICE_HEADERS: [&str; 3] =
        ["x-msh-device-name", "x-msh-device-model", "x-msh-device-id"];
    headers.retain(|name, _| {
        !DEVICE_HEADERS
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
    });
}

/// Remove OAuth-provider headers that do not belong to the selected catalog
/// platform. This is required when Kimi and Codex share a configured proxy:
/// URL-derived injection alone would otherwise combine both credential
/// families on one request.
pub(crate) fn align_oauth_headers_with_platform(
    headers: &mut IndexMap<String, String>,
    platform: Option<xai_grok_models::PlatformId>,
    base_url: &str,
) {
    use xai_grok_models::PlatformId;

    let is_oauth_origin = PlatformId::KimiCode.base_url_matches(base_url)
        || PlatformId::OpenAiCodex.base_url_matches(base_url);
    if !is_oauth_origin {
        return;
    }

    if platform != Some(PlatformId::KimiCode) {
        const KIMI_HEADERS: [&str; 4] = [
            "anthropic-version",
            "x-msh-device-name",
            "x-msh-device-model",
            "x-msh-device-id",
        ];
        headers.retain(|name, _| {
            !KIMI_HEADERS
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
        });
    }
    if platform != Some(PlatformId::OpenAiCodex) {
        const CODEX_HEADERS: [&str; 3] = ["openai-beta", "originator", "chatgpt-account-id"];
        headers.retain(|name, _| {
            !CODEX_HEADERS
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
        });
    }
}

pub fn resolve_model_to_sampling_config(
    model_id: &str,
    models: &IndexMap<String, ModelEntry>,
    session_key: Option<&str>,
    alpha_test_key: Option<String>,
    client_version: Option<String>,
    fallback_entry: Option<ModelEntry>,
) -> Option<SamplerConfig> {
    let entry = find_model_by_id(models, model_id)
        .cloned()
        .or(fallback_entry)?;
    let credentials = resolve_credentials(&entry, session_key);
    Some(sampling_config_for_model(
        &entry,
        credentials,
        alpha_test_key,
        client_version,
        None,
        None,
    ))
}
fn resolve_hidden_default_web_search_sampling_config(
    model_id: &str,
    session_key: Option<&str>,
    disable_api_key_auth: bool,
    alpha_test_key: Option<String>,
    client_version: Option<String>,
    endpoints: &EndpointsConfig,
) -> SamplerConfig {
    let entry = ModelEntry {
        info: ModelInfo {
            id: None,
            model: model_id.to_owned(),
            base_url: endpoints.resolve_inference_base_url(),
            name: None,
            description: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::Responses,
            request_compat: None,
            endpoint_path: None,
            auth_scheme: Default::default(),
            extra_headers: IndexMap::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window: NonZeroU64::new(200_000).unwrap(),
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            use_concise: false,
            agent_type: default_agent_type(),
            inference_idle_timeout_secs: None,
            max_retries: None,
            hidden: true,
            user_selectable: true,
            supported_in_api: true,
            reasoning_effort: None,
            supports_reasoning_effort: false,
            reasoning_efforts: Vec::new(),
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: None,
            laziness_detector: LazinessDetectorPerModelConfig::default(),
        },
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    let credentials = resolve_credentials_enforced(&entry, session_key, disable_api_key_auth);
    sampling_config_for_model(
        &entry,
        credentials,
        alpha_test_key,
        client_version,
        None,
        None,
    )
}
pub(crate) fn resolve_web_search_sampling_config(
    model_id: &str,
    models: &IndexMap<String, ModelEntry>,
    session_key: Option<&str>,
    disable_api_key_auth: bool,
    alpha_test_key: Option<String>,
    client_version: Option<String>,
    endpoints: &EndpointsConfig,
) -> Option<SamplerConfig> {
    let resolved = if let Some(entry) = find_model_by_id(models, model_id).cloned() {
        let credentials = resolve_credentials_enforced(&entry, session_key, disable_api_key_auth);
        if credentials.api_key.is_none() && entry.effective_auth_provider().is_some() {
            tracing::warn!(
                web_search_model = %model_id,
                "web search model uses an auth provider with no cached token; disabling web search"
            );
            return None;
        }
        Some(sampling_config_for_model(
            &entry,
            credentials,
            alpha_test_key,
            client_version,
            None,
            None,
        ))
    } else if model_id == crate::models::default_web_search_model() {
        Some(resolve_hidden_default_web_search_sampling_config(
            model_id,
            session_key,
            disable_api_key_auth,
            alpha_test_key,
            client_version,
            endpoints,
        ))
    } else {
        None
    };
    if resolved.is_none() {
        tracing::warn!(
            web_search_model = %model_id,
            "configured web_search model not found; disabling web search"
        );
    }
    resolved.map(crate::tools::config::web_search_sampling_config)
}
pub(crate) fn to_acp_model_info(
    models: &IndexMap<String, ModelEntry>,
) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    models
        .iter()
        .map(|(key, model)| {
            let info = model.info();
            let model_id = acp::ModelId::new(Arc::from(key.clone()));
            let total_context_tokens = info.context_window.get();
            let meta = {
                let mut map = serde_json::Map::new();
                map.insert(
                    "totalContextTokens".to_string(),
                    serde_json::Value::Number(total_context_tokens.into()),
                );
                map.insert(
                    "agentType".to_string(),
                    serde_json::Value::String(info.agent_type.clone()),
                );
                if info.supports_reasoning_effort {
                    map.insert(
                        "supportsReasoningEffort".to_string(),
                        serde_json::Value::Bool(true),
                    );
                    if let Some(effort) = info.reasoning_effort {
                        map.insert(
                            REASONING_EFFORT_META_KEY.to_string(),
                            reasoning_effort_meta_value(effort),
                        );
                    }
                }
                if !info.reasoning_efforts.is_empty() {
                    map.insert(
                        REASONING_EFFORTS_META_KEY.to_string(),
                        reasoning_efforts_meta_value(&info.reasoning_efforts),
                    );
                }
                if map.is_empty() { None } else { Some(map) }
            };
            (
                model_id.clone(),
                acp::ModelInfo::new(
                    model_id,
                    info.name.clone().unwrap_or_else(|| info.model.clone()),
                )
                .description(info.description.clone())
                .meta(meta),
            )
        })
        .collect()
}

/// ACP metadata flag identifying catalog rows sourced from `[model.*]` in
/// `config.toml`. Clients use it to replace only config-owned rows during an
/// explicit `/model` reload while retaining remote and provider catalogs.
pub const CONFIG_MODEL_META_KEY: &str = "xaiConfigModel";

/// Request metadata flag asking the shell to re-read model configuration
/// before resolving an explicitly selected model.
pub const RELOAD_MODEL_CONFIG_META_KEY: &str = "reloadModelConfig";

/// Error code for model switch rejection due to agent type mismatch.
pub const MODEL_SWITCH_INCOMPATIBLE_AGENT: &str = "MODEL_SWITCH_INCOMPATIBLE_AGENT";
/// Error code for model switch failure during the zero-turn full harness
/// rebuild path. Emitted when `RebuildAgentForDefinition` fails (definition
/// could not be resolved at handler time, `AgentBuilder::build()` errored,
/// or a turn started racing the rebuild).
pub const MODEL_SWITCH_REBUILD_FAILED: &str = "MODEL_SWITCH_REBUILD_FAILED";
/// Structured error payload for model switch rejection due to agent type
/// incompatibility. Serialized into `acp::Error.data` by the shell and
/// deserialized by the TUI for user-friendly error rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitchIncompatibleAgentError {
    /// Stable machine-readable error code (always `MODEL_SWITCH_INCOMPATIBLE_AGENT`).
    pub code: String,
    /// The agent type currently active in the session.
    pub active_agent_type: String,
    /// The agent type required by the target model.
    pub required_agent_type: String,
    /// The model ID that was requested.
    pub model_id: String,
    /// Remediation hint for the client.
    pub suggestion: String,
}
impl ModelSwitchIncompatibleAgentError {
    /// Build an `acp::Error` with this structured payload.
    pub(crate) fn into_acp_error(self) -> acp::Error {
        let message = format!(
            "Cannot switch to model '{}': it requires agent '{}' but the active agent is '{}'. \
             Start a new session to use this model.",
            self.model_id, self.required_agent_type, self.active_agent_type,
        );
        acp::Error::new(acp::ErrorCode::InvalidRequest.into(), message)
            .data(serde_json::to_value(&self).ok())
    }
    /// Try to parse from an `acp::Error.data` field.
    pub fn from_acp_error(err: &acp::Error) -> Option<Self> {
        let data = err.data.as_ref()?;
        let code = data.get("code")?.as_str()?;
        if code != MODEL_SWITCH_INCOMPATIBLE_AGENT {
            return None;
        }
        serde_json::from_value(data.clone()).ok()
    }
    /// Render a user-friendly error message for the TUI.
    pub fn user_message(&self) -> String {
        format!(
            "Cannot switch to '{}' — it requires agent '{}' but the active agent is '{}'. \
             Start /new to use this model.",
            self.model_id, self.required_agent_type, self.active_agent_type,
        )
    }
}
#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

#[cfg(test)]
#[allow(dead_code)]
mod hyper_tests {
    use super::*;
    use serial_test::serial;
    use xai_grok_test_support::EnvGuard;

    /// Point `GROK_AUTH_PATH` at a scratch `auth.json` and clear platform
    /// API-key env vars so platform credential tests don't depend on a dev
    /// box's real `~/.grok/auth.json` or exported keys.
    fn isolated_auth_home() -> (tempfile::TempDir, Vec<EnvGuard>) {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let mut guards = vec![EnvGuard::set("GROK_AUTH_PATH", auth.to_str().unwrap())];
        guards.extend(xai_grok_test_support::unset_all_byok_platform_api_key_envs());
        (dir, guards)
    }

    fn unset_bedrock_env() -> Vec<EnvGuard> {
        [
            "AWS_BEDROCK_SKIP_AUTH",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_PROFILE",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ]
        .into_iter()
        .map(EnvGuard::unset)
        .collect()
    }

    #[test]
    #[serial]
    fn bedrock_readiness_uses_aws_typed_sources_not_google_adc() {
        let (_home, mut guards) = isolated_auth_home();
        guards.extend(unset_bedrock_env());
        let adc_home = tempfile::tempdir().unwrap();
        let adc_dir = adc_home.path().join(".config/gcloud");
        std::fs::create_dir_all(&adc_dir).unwrap();
        std::fs::write(adc_dir.join("application_default_credentials.json"), "{}").unwrap();
        guards.push(EnvGuard::set("HOME", adc_home.path().to_str().unwrap()));
        let bedrock = xai_grok_models::provider_spec("amazon-bedrock").unwrap();
        assert!(!provider_external_readiness(bedrock));

        let _skip = EnvGuard::set("AWS_BEDROCK_SKIP_AUTH", "1");
        assert!(provider_external_readiness(bedrock));
    }

    #[test]
    #[serial]
    fn bedrock_readiness_accepts_profile_chain_pair_web_identity_and_marker() {
        let (home, mut guards) = isolated_auth_home();
        guards.extend(unset_bedrock_env());
        let bedrock = xai_grok_models::provider_spec("amazon-bedrock").unwrap();
        assert!(!provider_external_readiness(bedrock));

        let access = EnvGuard::set("AWS_ACCESS_KEY_ID", "akid");
        assert!(!provider_external_readiness(bedrock));
        let secret = EnvGuard::set("AWS_SECRET_ACCESS_KEY", "secret");
        assert!(provider_external_readiness(bedrock));
        drop((access, secret));

        let profile = EnvGuard::set("AWS_PROFILE", "dev");
        assert!(provider_external_readiness(bedrock));
        drop(profile);

        let ecs = EnvGuard::set("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/creds");
        assert!(provider_external_readiness(bedrock));
        drop(ecs);

        let token = home.path().join("token.jwt");
        std::fs::write(&token, "token").unwrap();
        let web = EnvGuard::set("AWS_WEB_IDENTITY_TOKEN_FILE", token.to_str().unwrap());
        assert!(provider_external_readiness(bedrock));
        drop(web);

        crate::auth::store_bedrock_credential_chain(home.path()).unwrap();
        assert!(provider_external_readiness(bedrock));
    }

    #[test]
    #[serial]
    fn bedrock_stored_profile_reaches_sampler_and_bearer_wins() {
        let (home, mut guards) = isolated_auth_home();
        guards.extend(unset_bedrock_env());
        crate::auth::store_bedrock_profile(home.path(), "dev-profile").unwrap();
        let model = resolve_model_list(&Config::default(), None)
            .get("amazon-bedrock/anthropic.claude-haiku-4-5-20251001-v1:0")
            .cloned()
            .expect("bedrock model");
        let cfg = sampling_config_for_model(
            &model,
            resolve_credentials(&model, None),
            None,
            None,
            None,
            None,
        );
        assert_eq!(cfg.api_key, None);
        assert_eq!(cfg.bedrock_profile.as_deref(), Some("dev-profile"));

        crate::auth::store_platform_api_key(home.path(), "amazon-bedrock", "bearer", None).unwrap();
        let model = resolve_model_list(&Config::default(), None)
            .get("amazon-bedrock/anthropic.claude-haiku-4-5-20251001-v1:0")
            .cloned()
            .expect("bedrock model");
        let cfg = sampling_config_for_model(
            &model,
            resolve_credentials(&model, None),
            None,
            None,
            None,
            None,
        );
        assert_eq!(cfg.api_key.as_deref(), Some("bearer"));
        assert_eq!(cfg.bedrock_profile, None);
    }

    #[test]
    fn inject_url_derived_headers_removes_stale_codex_account_id() {
        let mut headers = IndexMap::new();
        headers.insert("chatgpt-account-id".to_string(), "acct-stale".to_string());
        inject_url_derived_headers(&mut headers, None, "https://chatgpt.com/backend-api/codex");
        assert!(headers.get("chatgpt-account-id").is_none());
        assert_eq!(
            headers.get("OpenAI-Beta").map(String::as_str),
            Some("responses=experimental")
        );
    }

    #[test]
    fn malformed_mcp_entries_do_not_invalidate_valid_neighbors() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [mcp_servers.good]
            command = "node"
            args = ["server.js"]

            [mcp_servers.bad]
            enabled = true
            args = ["missing-transport"]
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config)
            .expect("malformed MCP entry must not fail the whole config");
        assert!(cfg.mcp_servers.contains_key("good"));
        assert!(
            !cfg.mcp_servers.contains_key("bad"),
            "only the malformed MCP entry should be dropped"
        );
    }

    /// The lenient parser warns per problem and never fails the whole
    /// config.
    fn test_model_entry(
        model: &str,
        base_url: &str,
        api_key: Option<&str>,
        env_key: Option<&str>,
        api_base_url: Option<&str>,
    ) -> ModelEntry {
        ModelEntry {
            info: ModelInfo {
                user_selectable: true,
                id: None,
                model: model.to_string(),
                base_url: base_url.to_string(),
                name: None,
                description: None,
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: ApiBackend::default(),
                request_compat: None,
                endpoint_path: None,
                auth_scheme: Default::default(),
                extra_headers: IndexMap::new(),
                query_params: IndexMap::new(),
                env_http_headers: IndexMap::new(),
                context_window: NonZeroU64::new(200_000).unwrap(),
                auto_compact_threshold_percent: None,
                system_prompt_label: None,
                use_concise: false,
                agent_type: default_agent_type(),
                inference_idle_timeout_secs: None,
                max_retries: None,
                hidden: false,
                supported_in_api: true,
                reasoning_effort: None,
                supports_reasoning_effort: false,
                reasoning_efforts: Vec::new(),
                supports_backend_search: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                show_model_fingerprint: false,
                stream_tool_calls: None,
                laziness_detector: LazinessDetectorPerModelConfig::default(),
            },
            api_key: api_key.map(|s| s.to_string()),
            env_key: env_key.map(EnvKeys::single),
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: api_base_url.map(|s| s.to_string()),
        }
    }

    /// Hybrid Kimi OAuth predicate truth table (static / OAuth active / pre-login)
    /// plus Codex identity-only (unchanged by the hybrid gate).
    #[test]
    fn model_uses_kimi_code_oauth_hybrid_truth_table() {
        let mut static_kimi = test_model_entry(
            "kimi-for-coding",
            "https://api.kimi.com/coding/v1",
            Some("static-kimi-key"),
            None,
            None,
        );
        static_kimi.info.id = Some("kimi-code/kimi-for-coding".into());
        static_kimi.platform_oauth_active = false;
        assert!(
            static_kimi.has_own_credentials() && !static_kimi.platform_oauth_active,
            "precondition: static own key + inactive OAuth stamp"
        );
        assert!(
            !model_uses_kimi_code_oauth(&static_kimi),
            "static own credential + !platform_oauth_active → no OAuth resolver"
        );
        assert!(kimi_code_bearer_resolver_for_model(&static_kimi).is_none());

        let mut oauth_kimi = test_model_entry(
            "kimi-for-coding",
            "https://api.kimi.com/coding/v1",
            Some("oauth-marker"),
            None,
            None,
        );
        oauth_kimi.info.id = Some("kimi-code/kimi-for-coding".into());
        oauth_kimi.platform_oauth_active = true;
        assert!(
            model_uses_kimi_code_oauth(&oauth_kimi),
            "platform_oauth_active → install live Kimi resolver"
        );
        assert!(kimi_code_bearer_resolver_for_model(&oauth_kimi).is_some());

        let mut pre_login_kimi = test_model_entry(
            "kimi-for-coding",
            "https://api.kimi.com/coding/v1",
            None,
            None,
            None,
        );
        pre_login_kimi.info.id = Some("kimi-code/kimi-for-coding".into());
        pre_login_kimi.platform_oauth_active = false;
        assert!(
            !pre_login_kimi.has_own_credentials(),
            "precondition: no own credential (pre-login)"
        );
        assert!(
            model_uses_kimi_code_oauth(&pre_login_kimi),
            "no own credential → identity-only install (avoid post-login stale memo)"
        );
        assert!(kimi_code_bearer_resolver_for_model(&pre_login_kimi).is_some());

        // Codex remains identity-only regardless of stamp / own key.
        for (api_key, oauth_active) in [(None, false), (None, true), (Some("codex-key"), false)] {
            let mut codex = test_model_entry(
                "gpt-5.1-codex",
                "https://chatgpt.com/backend-api/codex",
                api_key,
                None,
                None,
            );
            codex.info.id = Some("openai-codex/gpt-5.1-codex".into());
            codex.platform_oauth_active = oauth_active;
            assert!(
                model_uses_openai_codex_oauth(&codex),
                "Codex identity-only must not depend on stamp/own-key (active={oauth_active}, key={api_key:?})"
            );
            assert!(!model_uses_kimi_code_oauth(&codex));
            assert!(openai_codex_bearer_resolver_for_model(&codex).is_some());
            assert!(kimi_code_bearer_resolver_for_model(&codex).is_none());
        }
    }

    /// Codex and Kimi both use OAuth, but must not share bearer resolvers.
    /// Regression: `model_uses_kimi_code_oauth` used to match any `uses_oauth()`
    /// platform and installed `KimiCodeBearerResolver` on `openai-codex/*`.
    #[test]
    #[serial]
    fn oauth_platform_helpers_distinguish_kimi_and_codex() {
        let proxy = "https://oauth-proxy.example.test/v1";
        let _kimi_base =
            xai_grok_test_support::EnvGuard::set(xai_grok_models::KIMI_CODE_BASE_URL_ENV, proxy);
        let _codex_base = xai_grok_test_support::EnvGuard::set("GROK_OPENAI_CODEX_BASE_URL", proxy);

        let mut kimi = test_model_entry(
            "kimi-for-coding",
            "https://api.kimi.com/coding/v1",
            None,
            None,
            None,
        );
        kimi.info.id = Some("kimi-code/kimi-for-coding".into());
        kimi.info.base_url = proxy.to_string();
        kimi.platform_oauth_active = true;

        let mut codex = test_model_entry(
            "gpt-5.1-codex",
            "https://chatgpt.com/backend-api/codex",
            None,
            None,
            None,
        );
        codex.info.id = Some("openai-codex/gpt-5.1-codex".into());
        codex.info.base_url = proxy.to_string();
        codex.platform_oauth_active = true;

        assert!(model_uses_kimi_code_oauth(&kimi));
        assert!(!model_uses_openai_codex_oauth(&kimi));
        assert!(kimi_code_bearer_resolver_for_model(&kimi).is_some());
        assert!(openai_codex_bearer_resolver_for_model(&kimi).is_none());

        assert!(!model_uses_kimi_code_oauth(&codex));
        assert!(model_uses_openai_codex_oauth(&codex));
        assert!(kimi_code_bearer_resolver_for_model(&codex).is_none());
        assert!(openai_codex_bearer_resolver_for_model(&codex).is_some());
        assert!(
            oauth_platform_for_base_url(proxy).is_none(),
            "an ambiguous origin must never choose a credential family"
        );

        let all_provider_headers = || {
            IndexMap::from([
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("X-Msh-Device-Id".to_string(), "device".to_string()),
                (
                    "OpenAI-Beta".to_string(),
                    "responses=experimental".to_string(),
                ),
                ("originator".to_string(), "grok-build".to_string()),
                ("chatgpt-account-id".to_string(), "acct".to_string()),
            ])
        };
        let mut kimi_headers = all_provider_headers();
        align_oauth_headers_with_platform(
            &mut kimi_headers,
            Some(xai_grok_models::PlatformId::KimiCode),
            proxy,
        );
        assert!(kimi_headers.contains_key("anthropic-version"));
        assert!(kimi_headers.contains_key("X-Msh-Device-Id"));
        assert!(!kimi_headers.contains_key("OpenAI-Beta"));
        assert!(!kimi_headers.contains_key("originator"));
        assert!(!kimi_headers.contains_key("chatgpt-account-id"));

        let mut codex_headers = all_provider_headers();
        align_oauth_headers_with_platform(
            &mut codex_headers,
            Some(xai_grok_models::PlatformId::OpenAiCodex),
            proxy,
        );
        assert!(!codex_headers.contains_key("anthropic-version"));
        assert!(!codex_headers.contains_key("X-Msh-Device-Id"));
        assert!(codex_headers.contains_key("OpenAI-Beta"));
        assert!(codex_headers.contains_key("originator"));
        assert!(codex_headers.contains_key("chatgpt-account-id"));
    }

    #[test]
    #[serial]
    fn radius_hybrid_prefers_static_api_key_over_oauth_marker() {
        fn radius_catalog() -> IndexMap<String, ModelEntry> {
            let mut radius = test_model_entry(
                "dynamic-model",
                "https://inference.radius.test/v1",
                None,
                None,
                None,
            );
            radius.info.id = Some("radius/dynamic-model".into());
            radius.info.api_backend = ApiBackend::PiMessages;
            IndexMap::from([("radius/dynamic-model".to_string(), radius)])
        }

        let dir = tempfile::tempdir().unwrap();
        let _auth = EnvGuard::set(
            "GROK_AUTH_PATH",
            dir.path().join("auth.json").to_str().unwrap(),
        );
        let cfg = PlatformsConfig::default();
        {
            let _key = EnvGuard::set("GROK_RADIUS_API_KEY", "static-radius-key");
            let mut models = radius_catalog();
            apply_platform_credentials_with_bearer(
                &mut models,
                &cfg,
                None,
                None,
                None,
                None,
                Some("oauth-marker".into()),
                None,
            );
            let entry = &models["radius/dynamic-model"];
            assert!(entry.has_own_credentials());
            assert_eq!(
                resolve_credentials(entry, None).api_key.as_deref(),
                Some("static-radius-key")
            );
            assert!(!entry.platform_oauth_active);
        }
        {
            let _key = EnvGuard::unset("GROK_RADIUS_API_KEY");
            let _legacy_key = EnvGuard::unset("RADIUS_API_KEY");
            let mut models = radius_catalog();
            apply_platform_credentials_with_bearer(
                &mut models,
                &cfg,
                None,
                None,
                None,
                None,
                Some("oauth-marker".into()),
                None,
            );
            let entry = &models["radius/dynamic-model"];
            assert_eq!(entry.api_key.as_deref(), Some("oauth-marker"));
            assert!(entry.platform_oauth_active);
        }
    }

    #[test]
    fn radius_resolver_requires_catalog_oauth_marker_not_static_key() {
        let mut radius = test_model_entry(
            "dynamic-model",
            "https://inference.radius.test/v1",
            Some("static-radius-key"),
            None,
            None,
        );
        radius.info.id = Some("radius/dynamic-model".into());
        radius.info.api_backend = ApiBackend::PiMessages;

        assert!(model_uses_radius_oauth(&radius));
        assert!(radius_bearer_resolver_for_model(&radius).is_none());
        let static_sampler = sampling_config_for_model(
            &radius,
            resolve_credentials(&radius, None),
            None,
            None,
            None,
            None,
        );
        assert!(static_sampler.bearer_resolver.is_none());
        assert_eq!(static_sampler.api_key.as_deref(), Some("static-radius-key"));

        radius.platform_oauth_active = true;
        assert!(radius_bearer_resolver_for_model(&radius).is_some());
        let oauth_sampler = sampling_config_for_model(
            &radius,
            resolve_credentials(&radius, None),
            None,
            None,
            None,
            None,
        );
        assert!(oauth_sampler.bearer_resolver.is_some());
    }

    /// Pi stores Anthropic's SDK root (`api.anthropic.com`) on every catalog
    /// row, but Grok appends only `/messages`. The injected model must resolve
    /// through the platform so the default and Claude Code alias gain `/v1`.
    #[test]
    #[serial]
    fn anthropic_builtin_resolves_versioned_base_and_env_overrides() {
        fn injected_base() -> String {
            let mut catalog = IndexMap::new();
            inject_moonshot_builtin_models(&mut catalog);
            catalog
                .get("anthropic/claude-haiku-4-5")
                .expect("Anthropic builtin present")
                .info
                .base_url
                .clone()
        }

        {
            let _grok = EnvGuard::unset(xai_grok_models::ANTHROPIC_BASE_URL_ENV);
            let _claude = EnvGuard::unset(xai_grok_models::ANTHROPIC_BASE_URL_ALIAS_ENV);
            assert_eq!(injected_base(), "https://api.anthropic.com/v1");
        }
        {
            let _grok = EnvGuard::unset(xai_grok_models::ANTHROPIC_BASE_URL_ENV);
            let _claude = EnvGuard::set(
                xai_grok_models::ANTHROPIC_BASE_URL_ALIAS_ENV,
                "https://gateway.example.com/proxy/",
            );
            assert_eq!(injected_base(), "https://gateway.example.com/proxy/v1");
        }
        {
            let _claude = EnvGuard::set(
                xai_grok_models::ANTHROPIC_BASE_URL_ALIAS_ENV,
                "https://ignored.example.com/proxy",
            );
            let _grok = EnvGuard::set(
                xai_grok_models::ANTHROPIC_BASE_URL_ENV,
                "https://grok.example.com/exact",
            );
            assert_eq!(injected_base(), "https://grok.example.com/exact");
        }
        {
            let _claude = EnvGuard::set(
                xai_grok_models::ANTHROPIC_BASE_URL_ALIAS_ENV,
                "https://gateway.example.com/fallback",
            );
            let _grok = EnvGuard::set(xai_grok_models::ANTHROPIC_BASE_URL_ENV, "   ");
            assert_eq!(injected_base(), "https://gateway.example.com/fallback/v1");
        }
    }

    /// GPT-5.6 Sol/Terra expose max+ultra; Luna max only — matches Codex
    /// `models.json` `supported_reasoning_levels`.
    #[test]
    fn openai_codex_builtin_exposes_codex_catalog_effort_menu() {
        let mut catalog = IndexMap::new();
        inject_moonshot_builtin_models(&mut catalog);
        let sol = catalog
            .get("openai-codex/gpt-5.6-sol")
            .expect("sol builtin present");
        assert!(sol.info.supports_reasoning_effort);
        assert_eq!(sol.info.reasoning_effort, Some(ReasoningEffort::Low));
        let values: Vec<_> = sol.info.reasoning_efforts.iter().map(|o| o.value).collect();
        assert_eq!(
            values,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Xhigh,
                ReasoningEffort::Max,
                ReasoningEffort::Ultra,
            ]
        );
        let terra = catalog
            .get("openai-codex/gpt-5.6-terra")
            .expect("terra present");
        assert!(
            terra
                .info
                .reasoning_efforts
                .iter()
                .any(|o| o.value == ReasoningEffort::Ultra)
        );
        let luna = catalog
            .get("openai-codex/gpt-5.6-luna")
            .expect("luna present");
        let luna_vals: Vec<_> = luna
            .info
            .reasoning_efforts
            .iter()
            .map(|o| o.value)
            .collect();
        assert!(luna_vals.contains(&ReasoningEffort::Max));
        assert!(
            !luna_vals.contains(&ReasoningEffort::Ultra),
            "Luna has max but not ultra in Codex catalog"
        );
        // 5.5 stops at xhigh (no max/ultra).
        let g55 = catalog.get("openai-codex/gpt-5.5").expect("5.5 present");
        let g55_vals: Vec<_> = g55.info.reasoning_efforts.iter().map(|o| o.value).collect();
        assert_eq!(
            g55_vals,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Xhigh,
            ]
        );
    }

    /// Kimi K3 menu is low/high/max (not the Grok or Codex ladder).
    #[test]
    fn kimi_k3_builtin_exposes_low_high_max_effort_menu() {
        let mut catalog = IndexMap::new();
        inject_moonshot_builtin_models(&mut catalog);
        let k3 = catalog.get("kimi-code/k3").expect("k3 builtin present");
        assert!(k3.info.supports_reasoning_effort);
        assert_eq!(k3.info.reasoning_effort, Some(ReasoningEffort::Max));
        let values: Vec<_> = k3.info.reasoning_efforts.iter().map(|o| o.value).collect();
        assert_eq!(
            values,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ]
        );
    }

    /// The effective-model RE-support lookup must use the model ACTUALLY used:
    /// the resolved aux model when present, else the session model (an
    /// unresolvable slug ⇒ aux `None` ⇒ session model's capability wins).
    #[test]
    #[serial]
    fn resolve_credentials_managed_platform_model_never_falls_through() {
        use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
        use xai_chat_state::AuthType;
        use xai_grok_test_support::EnvGuard;
        // A credential-less managed platform entry must NOT fall through to
        // the xAI session token or the global xAI key — either would send xAI
        // credentials to the third-party base URL.
        let _global = EnvGuard::set(XAI_API_KEY_ENV_VAR, "xai-global-sentinel");
        let _legacy = EnvGuard::set(LEGACY_XAI_API_KEY_ENV_VAR, "xai-legacy-sentinel");
        let model = test_model_entry(
            "deepseek/deepseek-v4-flash",
            "https://api.deepseek.com",
            None,
            None,
            None,
        );
        assert!(model.is_managed_platform_model());
        assert!(!model.has_own_credentials());
        let creds = resolve_credentials(&model, Some("session-jwt"));
        assert_eq!(creds.api_key, None, "session token must not leak");
        assert_eq!(creds.auth_type, AuthType::ApiKey);
        let creds = resolve_credentials(&model, None);
        assert_eq!(creds.api_key, None, "global xAI key must not leak");
    }
    fn api_key_creds(base_url: &str) -> ResolvedCredentials {
        ResolvedCredentials {
            api_key: Some("xai-secret".to_string()),
            base_url: base_url.to_string(),
            auth_type: xai_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        }
    }
    /// `disable_api_key_auth` kill switch (Claude `forceLoginMethod` parity).
    #[test]
    #[serial]
    fn anthropic_auth_token_env_resolves_to_bearer() {
        use xai_grok_test_support::EnvGuard;
        let _a = EnvGuard::unset(xai_grok_models::ANTHROPIC_API_KEY_ENV);
        let _b = EnvGuard::set(
            xai_grok_models::ANTHROPIC_API_KEY_ALIAS_ENV,
            "must-not-beat-bearer",
        );
        let _c = EnvGuard::set(
            xai_grok_models::ANTHROPIC_AUTH_TOKEN_ENV,
            "sk-ant-bearer-token",
        );

        let mut model =
            test_model_entry("claude", "https://api.anthropic.com/v1", None, None, None);
        model.env_key = Some(EnvKeys::new(
            xai_grok_models::PlatformId::Anthropic
                .api_key_env_names()
                .iter()
                .copied(),
        ));
        model.info.api_backend = ApiBackend::Messages;
        model.info.auth_scheme = AuthScheme::XApiKey;

        let creds = resolve_credentials(&model, None);
        assert_eq!(creds.api_key.as_deref(), Some("sk-ant-bearer-token"));
        assert_eq!(
            creds.auth_scheme,
            AuthScheme::Bearer,
            "ANTHROPIC_AUTH_TOKEN must force Bearer, not x-api-key"
        );
        let config = sampling_config_for_model(&model, creds, None, None, None, None);
        assert_eq!(config.auth_scheme, AuthScheme::Bearer);
        let client = xai_grok_sampler::SamplingClient::new(config).expect("client should build");
        assert_eq!(client.auth_info().auth_type, "bearer");

        // The Grok-scoped API key remains the highest-priority override even
        // when both standard Claude Code credential variables are present.
        {
            let _scoped = EnvGuard::set(
                xai_grok_models::ANTHROPIC_API_KEY_ENV,
                "grok-scoped-api-key",
            );
            let creds = resolve_credentials(&model, None);
            assert_eq!(creds.api_key.as_deref(), Some("grok-scoped-api-key"));
            assert_eq!(creds.auth_scheme, AuthScheme::XApiKey);
        }
    }

    #[test]
    #[serial]
    fn anthropic_api_key_env_resolves_to_x_api_key() {
        use xai_grok_test_support::EnvGuard;
        let _a = EnvGuard::unset(xai_grok_models::ANTHROPIC_AUTH_TOKEN_ENV);
        let _b = EnvGuard::unset(xai_grok_models::ANTHROPIC_API_KEY_ENV);
        let _c = EnvGuard::set(
            xai_grok_models::ANTHROPIC_API_KEY_ALIAS_ENV,
            "sk-ant-api-key",
        );

        let mut model =
            test_model_entry("claude", "https://api.anthropic.com/v1", None, None, None);
        model.env_key = Some(EnvKeys::new(
            xai_grok_models::PlatformId::Anthropic
                .api_key_env_names()
                .iter()
                .copied(),
        ));
        model.info.api_backend = ApiBackend::Messages;
        model.info.auth_scheme = AuthScheme::XApiKey;

        let creds = resolve_credentials(&model, None);
        assert_eq!(creds.api_key.as_deref(), Some("sk-ant-api-key"));
        assert_eq!(creds.auth_scheme, AuthScheme::XApiKey);
        let config = sampling_config_for_model(&model, creds, None, None, None, None);
        assert_eq!(config.auth_scheme, AuthScheme::XApiKey);
        let client = xai_grok_sampler::SamplingClient::new(config).expect("client should build");
        assert_eq!(client.auth_info().auth_type, "x-api-key");
    }

    #[test]
    fn scoped_config_parse_failure_stays_config_unavailable() {
        let malformed: toml::Value = toml::from_str(
            r#"
[models]
default = 42
"#,
        )
        .unwrap();
        let byok = with_resolved_model_loader(
            "grok-4.5",
            move || Config::new_from_toml_cfg(&malformed).ok(),
            |lookup| byok_from_lookup(&lookup),
        );
        assert_eq!(
            byok,
            ModelByok::Unknown,
            "a parse failure on the enlarged-stack thread must not look like a missing/session model"
        );
    }

    #[test]
    fn codex_responses_defaults_backend_search_but_explicit_false_wins() {
        let endpoints = EndpointsConfig::default();
        let defaulted = ConfigModelOverride {
            api_backend: Some(ApiBackend::CodexResponses),
            ..Default::default()
        }
        .apply("codex-byok", None, &endpoints);
        assert!(defaulted.info.supports_backend_search);

        let disabled = ConfigModelOverride {
            api_backend: Some(ApiBackend::CodexResponses),
            supports_backend_search: Some(false),
            ..Default::default()
        }
        .apply("codex-byok", None, &endpoints);
        assert!(!disabled.info.supports_backend_search);
    }

    #[test]
    fn codex_responses_byok_keeps_provider_key_and_avoids_oauth_headers() {
        let mut model = test_model_entry(
            "gpt-codex",
            "https://codex-provider.example/v1",
            Some("sk-codex-byok"),
            None,
            None,
        );
        model.info.api_backend = ApiBackend::CodexResponses;
        model
            .info
            .extra_headers
            .insert("X-Tenant".into(), "tenant-a".into());

        let sampler = sampling_config_for_model(
            &model,
            resolve_credentials(&model, None),
            None,
            None,
            None,
            None,
        );
        assert_eq!(sampler.api_key.as_deref(), Some("sk-codex-byok"));
        assert!(sampler.bearer_resolver.is_none());
        assert!(sampler.responses_codex_dialect);
        assert_eq!(
            sampler.adapter_kind,
            xai_grok_models::AdapterKind::OpenAiCodex
        );
        assert_eq!(
            sampler.extra_headers.get("X-Tenant").map(String::as_str),
            Some("tenant-a")
        );
        assert!(!sampler.extra_headers.contains_key("OpenAI-Beta"));
        assert!(!sampler.extra_headers.contains_key("originator"));
        assert!(!model_uses_openai_codex_oauth(&model));
    }
    #[test]
    fn parses_model_api_backend_codex_responses_snake_and_kebab() {
        for backend in ["codex_responses", "codex-responses"] {
            let raw = format!(
                r#"
                [model.codex-proxy]
                model = "gpt-5.4"
                base_url = "https://codex-proxy.example.com/v1"
                context_window = 200000
                api_key = "sk-test"
                api_backend = "{backend}"
                "#
            );
            let raw_config: toml::Value = toml::from_str(&raw).unwrap();
            let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
            let resolved = resolve_model_list(&cfg, None);
            let model = resolved.get("codex-proxy").expect("model should exist");
            assert_eq!(
                model.info.api_backend,
                ApiBackend::CodexResponses,
                "backend string {backend}"
            );
            let sampling = sampling_config_for_model(
                model,
                resolve_credentials(model, None),
                None,
                None,
                None,
                None,
            );
            assert!(
                sampling.responses_codex_dialect,
                "codex dialect must be on for {backend}"
            );
            assert_eq!(
                sampling.adapter_kind,
                xai_grok_models::AdapterKind::OpenAiCodex,
                "adapter for {backend}"
            );
            assert_eq!(sampling.api_backend, ApiBackend::CodexResponses);
        }
    }

    fn resolve_models_from_toml(
        toml_str: &str,
        prefetched: Option<IndexMap<String, ModelEntry>>,
    ) -> (Config, IndexMap<String, ModelEntry>) {
        let raw: toml::Value = toml::from_str(toml_str).expect("test TOML should parse");
        let cfg = Config::new_from_toml_cfg(&raw).expect("config should parse");
        let resolved = resolve_model_list(&cfg, prefetched);
        (cfg, resolved)
    }
    fn resolve_sampling(model: &ModelEntry, session_key: Option<&str>) -> SamplerConfig {
        let credentials = resolve_credentials(model, session_key);
        sampling_config_for_model(model, credentials, None, None, None, None)
    }
    fn unset_endpoint_env_vars() {
        for k in [
            "GROK_CLI_CHAT_PROXY_BASE_URL",
            "GROK_XAI_API_BASE_URL",
            "GROK_FEEDBACK_BASE_URL",
            "GROK_TRACE_UPLOAD_URL",
            "GROK_MANAGED_CONFIG_URL",
            "GROK_MODELS_BASE_URL",
            "GROK_MODELS_LIST_URL",
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "OTEL_EXPORTER_OTLP_HEADERS",
            "GROK_INTERNAL_OTLP_TRACES_ENDPOINT",
            "GROK_INTERNAL_OTLP_HEADERS",
            "GROK_EXTERNAL_OTEL",
        ] {
            unsafe { std::env::remove_var(k) };
        }
    }
    /// INVARIANT: auxiliary-service resolvers resolve to the cli-chat-proxy, never
    /// `xai_api_base_url` — overriding ONLY inference keeps every aux endpoint on
    /// the proxy; explicit per-service overrides win verbatim.
    fn clear_goal_envs() {
        unsafe {
            std::env::remove_var("GROK_GOAL");
            std::env::remove_var("GROK_GOAL_CLASSIFIER");
            std::env::remove_var("GROK_GOAL_PLANNER");
            std::env::remove_var("GROK_GOAL_SUMMARY");
            std::env::remove_var("GROK_GOAL_VERIFIER_N");
            std::env::remove_var("GROK_GOAL_CLASSIFIER_MAX");
            std::env::remove_var("GROK_GOAL_STRATEGIST_EVERY");
            std::env::remove_var("GROK_GOAL_REVERIFY_AFTER");
        }
    }
    fn cfg_with_goal(goal: bool) -> Config {
        Config {
            goal: GoalConfig {
                enabled: Some(goal),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn cfg_with_goal_and_remote(goal: bool, remote: crate::util::config::RemoteSettings) -> Config {
        Config {
            goal: GoalConfig {
                enabled: Some(goal),
                ..Default::default()
            },
            remote_settings: Some(remote),
            ..Default::default()
        }
    }
    fn remote_classifier(v: bool) -> crate::util::config::RemoteSettings {
        crate::util::config::RemoteSettings {
            goal_classifier_enabled: Some(v),
            ..Default::default()
        }
    }
    fn remote_planner(v: bool) -> crate::util::config::RemoteSettings {
        crate::util::config::RemoteSettings {
            goal_planner_enabled: Some(v),
            ..Default::default()
        }
    }
    fn remote_summary(v: bool) -> crate::util::config::RemoteSettings {
        crate::util::config::RemoteSettings {
            goal_summary_enabled: Some(v),
            ..Default::default()
        }
    }
    fn cfg_with_goal_config(goal: GoalConfig) -> Config {
        Config {
            goal,
            ..Default::default()
        }
    }
    fn cfg_with_goal_config_and_remote(
        goal: GoalConfig,
        remote: crate::util::config::RemoteSettings,
    ) -> Config {
        Config {
            goal,
            remote_settings: Some(remote),
            ..Default::default()
        }
    }
    const GOAL_USE_CURRENT_ENV: &str = "GROK_GOAL_USE_CURRENT_MODEL_ONLY";
    fn clear_goal_model_env() {
        unsafe { std::env::remove_var(GOAL_USE_CURRENT_ENV) };
    }
    fn planner_pair() -> crate::util::config::GoalRoleModel {
        crate::util::config::GoalRoleModel {
            model: "grok-4".to_string(),
            agent_type: "general-purpose".to_string(),
        }
    }
    fn strategist_pair() -> crate::util::config::GoalRoleModel {
        crate::util::config::GoalRoleModel {
            model: "grok-4.5".to_string(),
            agent_type: "cursor".to_string(),
        }
    }
    fn remote_planner_model(
        p: crate::util::config::GoalRoleModel,
    ) -> crate::util::config::RemoteSettings {
        crate::util::config::RemoteSettings {
            goal_planner_model: Some(p),
            ..Default::default()
        }
    }
    fn remote_strategist_model(
        p: crate::util::config::GoalRoleModel,
    ) -> crate::util::config::RemoteSettings {
        crate::util::config::RemoteSettings {
            goal_strategist_model: Some(p),
            ..Default::default()
        }
    }
    fn unused_keys_from_toml(toml_str: &str) -> Vec<String> {
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let raw_without_models = {
            let mut r = raw.clone();
            if let toml::Value::Table(ref mut t) = r {
                t.remove("model");
            }
            r
        };
        let mut base = toml::Value::try_from(Config::default()).unwrap();
        if let toml::Value::Table(ref mut t) = base {
            t.remove("model");
        }
        crate::config::deep_merge_toml(&mut base, &raw_without_models);
        let (_config, unused) =
            Config::deserialize_collecting_unrecognized(base, &raw_without_models)
                .expect("config should deserialize");
        unused
    }
    fn internal_otlp_test_config() -> EndpointsConfig {
        EndpointsConfig {
            cli_chat_proxy_base_url: Some("https://proxy.example/v1".to_string()),
            otel_exporter_otlp_endpoint: None,
            otel_exporter_otlp_traces_endpoint: None,
            otel_exporter_otlp_headers: None,
            grok_internal_otlp_traces_endpoint: None,
            grok_internal_otlp_headers: None,
            external_otel_master_switch: false,
            ..Default::default()
        }
    }
    /// `grok_internal_otlp_traces_endpoint` wins over the legacy `OTEL_*`
    /// fields regardless of the master switch.
    fn ext_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }
    fn ext_client() -> xai_grok_telemetry::external::config::ExternalClientInfo {
        xai_grok_telemetry::external::config::ExternalClientInfo::default()
    }
    fn empty_config() -> toml::Value {
        toml::Value::Table(toml::map::Map::new())
    }
    fn clear_runtime_env_vars() {
        unsafe {
            std::env::remove_var("GROK_SUBAGENTS");
            std::env::remove_var("GROK_RESPECT_GITIGNORE");
            std::env::remove_var("GROK_WEB_SEARCH_MODEL");
            std::env::remove_var("GROK_SESSION_SUMMARY_MODEL");
            std::env::remove_var("GROK_CURSOR_SKILLS_ENABLED");
            std::env::remove_var("GROK_CURSOR_RULES_ENABLED");
            std::env::remove_var("GROK_CURSOR_AGENTS_ENABLED");
            std::env::remove_var("GROK_CLAUDE_SKILLS_ENABLED");
            std::env::remove_var("GROK_CLAUDE_RULES_ENABLED");
            std::env::remove_var("GROK_CLAUDE_AGENTS_ENABLED");
        }
    }
    fn clear_managed_mcp_env_vars() {
        unsafe {
            std::env::remove_var("GROK_MANAGED_MCPS_ENABLED");
            std::env::remove_var("GROK_MANAGED_MCP_GATEWAY_TOOLS_ENABLED");
        }
    }
    fn isolate_compat_env() -> Vec<EnvGuard> {
        COMPAT_CELLS
            .into_iter()
            .map(|cell| EnvGuard::unset(cell.env_var()))
            .collect()
    }
    fn parse_compat(source: &str) -> CompatConfigToml {
        let raw: toml::Value = toml::from_str(source).unwrap();
        raw.get("compat").unwrap().clone().try_into().unwrap()
    }
    fn assert_session_one_disabled(config: CompatConfig, expected: CompatVendor) {
        for cell in COMPAT_CELLS {
            if cell.surface() == CompatSurface::Sessions {
                assert_eq!(
                    config.value(cell),
                    cell.vendor() != expected,
                    "{}.sessions",
                    cell.vendor().as_str()
                );
            }
        }
    }
    fn remote_settings_with(
        key: CompatRemoteKey,
        value: bool,
    ) -> crate::util::config::RemoteSettings {
        let mut remote = crate::util::config::RemoteSettings::default();
        match key {
            CompatRemoteKey::CursorSkills => remote.cursor_skills_enabled = Some(value),
            CompatRemoteKey::CursorRules => remote.cursor_rules_enabled = Some(value),
            CompatRemoteKey::CursorAgents => remote.cursor_agents_enabled = Some(value),
            CompatRemoteKey::CursorMcps => remote.cursor_mcps_enabled = Some(value),
            CompatRemoteKey::CursorHooks => remote.cursor_hooks_enabled = Some(value),
            CompatRemoteKey::CursorSessions => {
                remote.cursor_sessions_enabled = Some(value);
            }
            CompatRemoteKey::ClaudeSkills => remote.claude_skills_enabled = Some(value),
            CompatRemoteKey::ClaudeRules => remote.claude_rules_enabled = Some(value),
            CompatRemoteKey::ClaudeAgents => remote.claude_agents_enabled = Some(value),
            CompatRemoteKey::ClaudeMcps => remote.claude_mcps_enabled = Some(value),
            CompatRemoteKey::ClaudeHooks => remote.claude_hooks_enabled = Some(value),
            CompatRemoteKey::ClaudeSessions => {
                remote.claude_sessions_enabled = Some(value);
            }
            CompatRemoteKey::CodexSessions => remote.codex_sessions_enabled = Some(value),
        }
        remote
    }
    fn prefetch_model_entry(
        slug: &str,
        context_window: u64,
        api_backend: ApiBackend,
    ) -> ModelEntry {
        ModelEntry {
            info: ModelInfo {
                user_selectable: true,
                id: None,
                model: slug.to_owned(),
                base_url: "https://test.example.com/v1".to_owned(),
                name: Some(slug.to_owned()),
                description: None,
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend,
                request_compat: None,
                endpoint_path: None,
                auth_scheme: Default::default(),
                extra_headers: IndexMap::new(),
                query_params: IndexMap::new(),
                env_http_headers: IndexMap::new(),
                context_window: NonZeroU64::new(context_window).unwrap(),
                use_concise: false,
                agent_type: default_agent_type(),
                inference_idle_timeout_secs: None,
                max_retries: None,
                hidden: false,
                supported_in_api: true,
                reasoning_effort: None,
                supports_reasoning_effort: false,
                reasoning_efforts: Vec::new(),
                supports_backend_search: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                show_model_fingerprint: false,
                stream_tool_calls: None,
                laziness_detector: LazinessDetectorPerModelConfig::default(),
                auto_compact_threshold_percent: None,
                system_prompt_label: None,
            },
            api_key: None,
            env_key: None,
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: None,
        }
    }
    #[test]
    fn resolve_model_list_empty_prefetch_yields_only_platform_builtins() {
        let cfg = Config::default();
        let resolved = resolve_model_list(&cfg, Some(IndexMap::new()));
        // No xAI first-party models without a prefetch/catalog — but the
        // built-in offline platform entries are always present (locked until
        // BYOK credentials resolve).
        assert!(
            !resolved.contains_key(crate::models::default_model()),
            "xAI default must not appear without a catalog"
        );
        assert!(
            resolved
                .keys()
                .all(|k| xai_grok_models::parse_managed_model_key(k).is_some()),
            "empty prefetch yields only managed platform builtins, got: {:?}",
            resolved.keys().take(5).collect::<Vec<_>>()
        );
    }
    /// Regression: enterprise managed config overlays env_key on an oauth-only
    /// catalog entry. BYOK must force visibility for API-key users so a
    /// base `supported_in_api: false` does not leak into the overlay.
    #[test]
    #[serial]
    fn prefetched_xai_catalog_keeps_platform_builtins_with_env_key() {
        use xai_grok_test_support::EnvGuard;
        let (_dir, _guards) = isolated_auth_home();
        let _env = EnvGuard::set("DEEPSEEK_API_KEY", "sk-test-deepseek-not-for-prod");

        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        // Simulate an xAI-only prefetch (what online `/v1/models` produces).
        let mut prefetched = IndexMap::new();
        prefetched.insert(
            "grok-4.5".into(),
            ModelEntry::fallback("grok-4.5", &cfg.endpoints),
        );
        let models = resolve_model_list(&cfg, Some(prefetched));

        assert!(
            models.contains_key("grok-4.5"),
            "prefetched xAI model present"
        );
        let ds = models
            .get("deepseek/deepseek-v4-flash")
            .or_else(|| models.get("deepseek/deepseek-v4-pro"))
            .expect("DeepSeek offline platform model must survive xAI prefetch");
        assert!(
            ds.env_key.is_some(),
            "DeepSeek entry carries env_key for DEEPSEEK_API_KEY"
        );
        assert!(
            ds.has_own_credentials(),
            "DEEPSEEK_API_KEY must unlock DeepSeek visibility"
        );
        assert!(
            ds.visible_for_auth(true),
            "DeepSeek must appear in /model when API key is set"
        );
        // Live-only platforms without keys stay gated.
        if let Some(oai) = models.get("openai/gpt-5") {
            assert!(
                !oai.visible_for_auth(true) || oai.has_own_credentials(),
                "openai without key stays hidden"
            );
        }
    }

    #[test]
    #[serial]
    fn platform_models_invisible_without_credentials_for_session_users() {
        use xai_grok_test_support::EnvGuard;
        let (_dir, _guards) = isolated_auth_home();
        // Clear common platform key envs so has_own_credentials is false even
        // on developer machines that export ANTHROPIC_API_KEY / OPENAI_API_KEY.
        let _env = [
            EnvGuard::unset("OPENAI_API_KEY"),
            EnvGuard::unset("GROK_OPENAI_API_KEY"),
            EnvGuard::unset("ANTHROPIC_API_KEY"),
            EnvGuard::unset("GROK_ANTHROPIC_API_KEY"),
            EnvGuard::unset("ANTHROPIC_AUTH_TOKEN"),
            EnvGuard::unset("GROK_MOONSHOT_API_KEY"),
            EnvGuard::unset("GROK_MOONSHOT_CN_API_KEY"),
            EnvGuard::unset("MOONSHOT_API_KEY"),
        ];

        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let models = resolve_model_list(&cfg, None);
        let mut checked = 0usize;
        for key in [
            "openai/gpt-5",
            "anthropic/claude-sonnet-4-5",
            "kimi-code/k3",
            "moonshot-cn/kimi-k3",
        ] {
            let Some(entry) = models.get(key) else {
                continue;
            };
            assert!(
                entry.is_managed_platform_model(),
                "{key} should be a managed platform key"
            );
            assert!(
                !entry.has_own_credentials(),
                "{key}: test env must not resolve credentials"
            );
            assert!(
                !entry.visible_for_auth(true),
                "{key} must stay hidden for session users without credentials"
            );
            assert!(
                !entry.visible_for_auth(false),
                "{key} must stay hidden for API-key users without credentials"
            );
            checked += 1;
        }
        assert!(
            checked >= 2,
            "expected at least two platform models in the offline catalog"
        );
    }

    #[test]
    #[serial]
    fn moonshot_builtins_injected_into_catalog() {
        let (_dir, _guards) = isolated_auth_home();
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let models = resolve_model_list(&cfg, None);
        // Open-platform lineup is present with the right backend/env wiring.
        for key in [
            "moonshot-cn/kimi-k3",
            "moonshot-cn/kimi-k2.7-code",
            "moonshot-cn/kimi-k2.7-code-highspeed",
            "moonshot-cn/kimi-k2.6",
            "moonshot-cn/kimi-k2.5",
            "moonshot-ai/kimi-k3",
            "moonshot-ai/kimi-k2.7-code-highspeed",
            "moonshot-ai/kimi-k2.6",
        ] {
            let entry = models.get(key).unwrap_or_else(|| panic!("missing {key}"));
            assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions, "{key}");
            assert!(
                !entry.info.supported_in_api || entry.has_own_credentials(),
                "{key} must be hidden until credentials are configured"
            );
            assert!(entry.env_key.is_some(), "{key} needs env_key for BYOK");
            assert!(
                entry
                    .info
                    .max_completion_tokens
                    .is_some_and(|n| n >= 16_384),
                "{key} should ship a large max_tokens from Pi catalog"
            );
        }
        let hs = models
            .get("moonshot-cn/kimi-k2.7-code-highspeed")
            .expect("HyperSpeed");
        assert_eq!(hs.info.model, "kimi-k2.7-code-highspeed");
        assert!(hs.info.base_url.contains("moonshot.cn"));
        // Deprecated aliases still present for older configs.
        assert!(models.contains_key("moonshot-cn/kimi-k2-turbo-preview"));
        // Kimi Code subscription models are Anthropic Messages (Pi) and hidden until OAuth.
        for key in [
            "kimi-code/k3",
            "kimi-code/k2p7",
            "kimi-code/kimi-for-coding-highspeed",
            "kimi-code/kimi-for-coding",
        ] {
            let kimi = models.get(key).unwrap_or_else(|| panic!("missing {key}"));
            assert!(kimi.info.base_url.contains("kimi.com"), "{key}");
            assert_eq!(
                kimi.info.api_backend,
                ApiBackend::Messages,
                "{key}: Kimi Code uses Anthropic Messages (official Pi)"
            );
            assert!(
                !kimi.info.supported_in_api || kimi.has_own_credentials(),
                "{key} must be hidden until Kimi Code login"
            );
            assert_eq!(
                kimi.info
                    .extra_headers
                    .get("User-Agent")
                    .map(String::as_str),
                Some("KimiCLI/1.5"),
                "{key}"
            );
            assert_eq!(
                kimi.info
                    .extra_headers
                    .get("anthropic-version")
                    .map(String::as_str),
                Some(xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE),
                "{key}: Messages requires anthropic-version"
            );
        }
        // Pi multi-provider catalog present.
        assert!(
            models.contains_key("openai/gpt-5")
                || models.contains_key("anthropic/claude-sonnet-4-5")
        );
        // xAI defaults still present
        assert!(models.contains_key("grok-4.5") || models.values().any(|m| m.model == "grok-4.5"));
    }

    #[test]
    #[serial]
    fn opencode_go_api_key_unlocks_chat_and_messages_models_with_correct_auth() {
        let (_dir, _guards) = isolated_auth_home();
        let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
        let _key = EnvGuard::set(xai_grok_models::OPENCODE_API_KEY_ENV, "oc-test-key");
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let models = resolve_model_list(&cfg, None);

        let opencode_go: Vec<_> = models
            .iter()
            .filter(|(id, _)| id.starts_with("opencode-go/"))
            .collect();
        assert_eq!(opencode_go.len(), 16);
        let mut chat_count = 0;
        let mut messages_count = 0;
        for (id, model) in opencode_go {
            assert!(model.info.supported_in_api, "{id} must be unlocked");
            assert!(model.has_own_credentials(), "{id} must resolve the key");
            assert!(
                !model.info.supports_reasoning_effort,
                "{id} must not expose an undocumented effort menu"
            );
            match &model.info.api_backend {
                ApiBackend::ChatCompletions => {
                    chat_count += 1;
                    assert_eq!(model.info.auth_scheme, AuthScheme::Bearer, "{id}");
                    assert!(
                        !model
                            .info
                            .extra_headers
                            .keys()
                            .any(|name| name.eq_ignore_ascii_case("anthropic-version")),
                        "{id} must not send an Anthropic version header"
                    );
                }
                ApiBackend::Messages => {
                    messages_count += 1;
                    assert_eq!(model.info.auth_scheme, AuthScheme::XApiKey, "{id}");
                    assert_eq!(
                        model
                            .info
                            .extra_headers
                            .get("anthropic-version")
                            .map(String::as_str),
                        Some(xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE),
                        "{id}"
                    );
                }
                other => panic!("{id} has unexpected backend {other:?}"),
            }
        }
        assert_eq!(chat_count, 11);
        assert_eq!(messages_count, 5);

        let chat = models
            .get("opencode-go/deepseek-v4-flash")
            .expect("OpenCode Go Chat Completions model");
        assert!(chat.info.supported_in_api);
        assert!(chat.has_own_credentials());
        assert_eq!(chat.info.api_backend, ApiBackend::ChatCompletions);
        assert_eq!(chat.info.auth_scheme, AuthScheme::Bearer);
        assert!(!chat.info.supports_reasoning_effort);
        let chat_config = sampling_config_for_model(
            chat,
            resolve_credentials(chat, None),
            None,
            None,
            None,
            None,
        );
        assert_eq!(chat_config.api_key.as_deref(), Some("oc-test-key"));
        assert_eq!(
            chat_config.base_url,
            xai_grok_models::OPENCODE_GO_BASE_URL_DEFAULT
        );
        assert_eq!(chat_config.auth_scheme, AuthScheme::Bearer);
        assert_eq!(
            xai_grok_sampler::SamplingClient::new(chat_config)
                .expect("OpenCode Go chat client")
                .auth_info()
                .auth_type,
            "bearer"
        );

        let messages = models
            .get("opencode-go/minimax-m3")
            .expect("OpenCode Go Messages model");
        assert!(messages.info.supported_in_api);
        assert!(messages.has_own_credentials());
        assert_eq!(messages.info.api_backend, ApiBackend::Messages);
        assert_eq!(messages.info.auth_scheme, AuthScheme::XApiKey);
        assert!(!messages.info.supports_reasoning_effort);
        assert_eq!(
            messages
                .info
                .extra_headers
                .get("anthropic-version")
                .map(String::as_str),
            Some(xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE)
        );
        let messages_config = sampling_config_for_model(
            messages,
            resolve_credentials(messages, None),
            None,
            None,
            None,
            None,
        );
        assert_eq!(messages_config.api_key.as_deref(), Some("oc-test-key"));
        assert_eq!(
            messages_config.base_url,
            xai_grok_models::OPENCODE_GO_BASE_URL_DEFAULT
        );
        assert_eq!(messages_config.auth_scheme, AuthScheme::XApiKey);
        assert_eq!(messages_config.api_backend, ApiBackend::Messages);
        assert_eq!(
            xai_grok_sampler::SamplingClient::new(messages_config)
                .expect("OpenCode Go Messages client")
                .auth_info()
                .auth_type,
            "x-api-key"
        );
    }

    #[test]
    #[serial]
    fn wave1_registry_only_keys_unlock_mixed_protocol_catalogs() {
        let (_dir, _guards) = isolated_auth_home();
        let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
        let raw: toml::Value = toml::from_str(
            r#"
[platforms.opencodego]
api_key = "opencode-test-key"

[platforms.vercel-ai-gateway]
api_key = "vercel-test-key"
"#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        for provider_id in ["opencode", "opencode-go"] {
            assert_eq!(
                provider_credential_storage_ids(
                    xai_grok_models::provider_spec(provider_id).unwrap()
                ),
                ["opencode".to_string(), "opencode-go".to_string()]
            );
        }
        assert_eq!(
            cfg.platforms
                .config_api_key_for_provider("opencode")
                .as_deref(),
            Some("opencode-test-key")
        );
        let models = resolve_model_list(&cfg, None);

        let opencode_go: Vec<_> = models
            .iter()
            .filter(|(id, _)| id.starts_with("opencode-go/"))
            .collect();
        assert_eq!(opencode_go.len(), 16);
        assert!(opencode_go.iter().all(|(_, model)| {
            model.info.supported_in_api
                && model.has_own_credentials()
                && model.api_key.as_deref() == Some("opencode-test-key")
        }));

        let opencode: Vec<_> = models
            .iter()
            .filter(|(id, _)| id.starts_with("opencode/"))
            .collect();
        assert_eq!(opencode.len(), 58);
        assert!(
            opencode
                .iter()
                .all(|(_, model)| { model.info.supported_in_api && model.has_own_credentials() })
        );
        assert_eq!(
            opencode
                .iter()
                .filter(|(_, model)| model.info.api_backend == ApiBackend::ChatCompletions)
                .count(),
            19
        );
        assert_eq!(
            opencode
                .iter()
                .filter(|(_, model)| model.info.api_backend == ApiBackend::Responses)
                .count(),
            20
        );
        assert_eq!(
            opencode
                .iter()
                .filter(|(_, model)| model.info.api_backend == ApiBackend::Messages)
                .count(),
            14
        );
        assert_eq!(
            opencode
                .iter()
                .filter(|(_, model)| model.info.api_backend == ApiBackend::GoogleGenerateContent)
                .count(),
            5
        );
        let chat = models.get("opencode/big-pickle").unwrap();
        assert_eq!(chat.info.auth_scheme, AuthScheme::Bearer);
        let responses = models.get("opencode/gpt-5.6-sol").unwrap();
        assert_eq!(responses.info.auth_scheme, AuthScheme::Bearer);
        let messages = models.get("opencode/claude-opus-4-8").unwrap();
        assert_eq!(messages.info.auth_scheme, AuthScheme::XApiKey);
        assert_eq!(
            messages.info.base_url, "https://opencode.ai/zen/v1",
            "Messages SDK root must normalize before the sampler appends /messages"
        );

        let vercel: Vec<_> = models
            .iter()
            .filter(|(id, _)| id.starts_with("vercel-ai-gateway/"))
            .collect();
        assert_eq!(vercel.len(), 192);
        assert!(vercel.iter().all(|(_, model)| {
            model.info.supported_in_api
                && model.has_own_credentials()
                && model.info.api_backend == ApiBackend::Messages
                && model.info.auth_scheme == AuthScheme::XApiKey
                && model
                    .info
                    .extra_headers
                    .get("anthropic-version")
                    .map(String::as_str)
                    == Some(xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE)
        }));
    }

    #[test]
    #[serial]
    fn opencode_zen_reads_legacy_go_auth_scope() {
        let (dir, _guards) = isolated_auth_home();
        let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
        crate::auth::store_platform_api_key(dir.path(), "opencode-go", "legacy-go-test-key", None)
            .unwrap();
        let key = resolve_provider_api_key_with(
            xai_grok_models::provider_spec("opencode").unwrap(),
            &PlatformsConfig::default(),
            |_| None,
        );
        assert_eq!(key.as_deref(), Some("legacy-go-test-key"));
    }

    #[test]
    #[serial]
    fn opencode_go_models_remain_locked_without_api_key() {
        let (_dir, _guards) = isolated_auth_home();
        let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let models = resolve_model_list(&cfg, None);

        let opencode_go: Vec<_> = models
            .iter()
            .filter(|(id, _)| id.starts_with("opencode-go/"))
            .collect();
        assert_eq!(opencode_go.len(), 16);
        for (id, model) in opencode_go {
            assert!(!model.info.supported_in_api, "{id} must stay locked");
            assert!(!model.has_own_credentials(), "{id} must have no credential");
        }
    }

    #[test]
    #[serial]
    fn apply_platform_credentials_reveals_with_bearer() {
        let (_dir, _guards) = isolated_auth_home();
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let mut models = resolve_model_list(&cfg, None);
        apply_platform_credentials_with_bearer(
            &mut models,
            &cfg.platforms,
            Some("fake-kimi-token".into()),
            Some("fake-codex-token".into()),
            None,
            None,
            None,
            None,
        );
        let entry = models.get("kimi-code/k2p7").expect("kimi-code entry");
        assert!(
            entry.info.supported_in_api,
            "bearer makes Kimi entry visible"
        );
        assert_eq!(entry.api_key.as_deref(), Some("fake-kimi-token"));
        assert!(entry.platform_oauth_active);
        let kimi_sampler = sampling_config_for_model(
            entry,
            resolve_credentials(entry, None),
            None,
            None,
            None,
            None,
        );
        assert!(kimi_sampler.bearer_resolver.is_some());
        assert!(kimi_sampler.extra_headers.keys().any(|name| {
            name.eq_ignore_ascii_case("x-msh-device-id")
                || name.eq_ignore_ascii_case("x-msh-device-name")
        }));

        // Codex bearer stamps only openai-codex/* entries.
        let codex_entry = models
            .get("openai-codex/gpt-5.6-sol")
            .expect("openai-codex entry");
        assert!(
            codex_entry.info.supported_in_api,
            "bearer makes Codex entry visible"
        );
        assert_eq!(codex_entry.api_key.as_deref(), Some("fake-codex-token"));
        // …and never leaks onto the Kimi entry.
        assert_eq!(entry.api_key.as_deref(), Some("fake-kimi-token"));
    }

    #[test]
    #[serial]
    fn wave_two_models_stay_locked_when_endpoint_templates_are_unresolved() {
        let (_dir, _guards) = isolated_auth_home();
        let _azure_base = EnvGuard::unset("GROK_AZURE_OPENAI_BASE_URL");
        let _azure_base_alias = EnvGuard::unset("AZURE_OPENAI_BASE_URL");
        let _azure_resource = EnvGuard::unset("AZURE_OPENAI_RESOURCE_NAME");
        let _cf_gateway_base = EnvGuard::unset("GROK_CLOUDFLARE_AI_GATEWAY_BASE_URL");
        let _cf_workers_base = EnvGuard::unset("GROK_CLOUDFLARE_WORKERS_AI_BASE_URL");
        let _cf_account = EnvGuard::unset("CLOUDFLARE_ACCOUNT_ID");
        let _cf_gateway = EnvGuard::unset("CLOUDFLARE_GATEWAY_ID");
        let raw: toml::Value = toml::from_str(
            r#"
[platforms.azure-openai-responses]
api_key = "azure-test-key"
[platforms.cloudflare-ai-gateway]
api_key = "cloudflare-gateway-test-key"
[platforms.cloudflare-workers-ai]
api_key = "cloudflare-workers-test-key"
"#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        let models = resolve_model_list(&cfg, None);
        for model_id in [
            "azure-openai-responses/gpt-4",
            "cloudflare-ai-gateway/claude-3-5-haiku",
            "cloudflare-workers-ai/@cf/google/gemma-4-26b-a4b-it",
        ] {
            let entry = models.get(model_id).expect("Wave 2 catalog entry");
            assert!(!entry.info.supported_in_api, "{model_id} must stay locked");
            assert!(
                !entry.has_own_credentials(),
                "{model_id} leaked a credential"
            );
            assert!(!entry.visible_for_auth(false));
        }
    }

    #[test]
    #[serial]
    fn wave_two_runtime_materializes_urls_query_and_deployment() {
        let (_dir, _guards) = isolated_auth_home();
        let _azure_base = EnvGuard::unset("GROK_AZURE_OPENAI_BASE_URL");
        let _azure_base_alias = EnvGuard::unset("AZURE_OPENAI_BASE_URL");
        let _azure_resource = EnvGuard::set("AZURE_OPENAI_RESOURCE_NAME", "unit-resource");
        let _azure_version = EnvGuard::set("AZURE_OPENAI_API_VERSION", "2026-07-01-preview");
        let _azure_deployments = EnvGuard::set(
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
            "gpt-4=unit-gpt4-deployment",
        );
        let _cf_gateway_base = EnvGuard::unset("GROK_CLOUDFLARE_AI_GATEWAY_BASE_URL");
        let _cf_workers_base = EnvGuard::unset("GROK_CLOUDFLARE_WORKERS_AI_BASE_URL");
        let _cf_account = EnvGuard::set("CLOUDFLARE_ACCOUNT_ID", "account-123");
        let _cf_gateway = EnvGuard::set("CLOUDFLARE_GATEWAY_ID", "gateway-456");
        let raw: toml::Value = toml::from_str(
            r#"
[platforms.azure-openai-responses]
api_key = "azure-test-key"
[platforms.cloudflare-ai-gateway]
api_key = "cloudflare-gateway-test-key"
[platforms.cloudflare-workers-ai]
api_key = "cloudflare-workers-test-key"
"#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        let models = resolve_model_list(&cfg, None);

        let azure = models
            .get("azure-openai-responses/gpt-4")
            .expect("Azure entry");
        assert!(azure.visible_for_auth(false));
        assert_eq!(azure.info.model, "unit-gpt4-deployment");
        assert_eq!(
            azure.info.base_url,
            "https://unit-resource.openai.azure.com/openai/v1"
        );
        assert_eq!(
            azure
                .info
                .query_params
                .get("api-version")
                .map(String::as_str),
            Some("2026-07-01-preview")
        );
        assert_eq!(azure.info.auth_scheme, AuthScheme::ApiKey);

        let gateway = models
            .get("cloudflare-ai-gateway/claude-3-5-haiku")
            .expect("Cloudflare gateway entry");
        assert!(gateway.visible_for_auth(false));
        assert_eq!(
            gateway.info.base_url,
            "https://gateway.ai.cloudflare.com/v1/account-123/gateway-456/anthropic/v1"
        );
        assert_eq!(gateway.info.auth_scheme, AuthScheme::CfAigAuthorization);
        assert_eq!(gateway.info.api_backend, ApiBackend::Messages);

        let workers = models
            .get("cloudflare-workers-ai/@cf/google/gemma-4-26b-a4b-it")
            .expect("Cloudflare Workers entry");
        assert!(workers.visible_for_auth(false));
        assert_eq!(
            workers.info.base_url,
            "https://api.cloudflare.com/client/v4/accounts/account-123/ai/v1"
        );
        assert_eq!(workers.info.auth_scheme, AuthScheme::Bearer);
    }

    #[test]
    #[serial]
    fn managed_azure_override_gets_runtime_mapping_once_without_losing_custom_query() {
        let (_dir, _guards) = isolated_auth_home();
        let _azure_version = EnvGuard::set("AZURE_OPENAI_API_VERSION", "2026-08-01-preview");
        let _azure_deployments = EnvGuard::set(
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
            "gpt-4=managed-gpt4-deployment",
        );
        let raw: toml::Value = toml::from_str(
            r#"
[platforms.azure-openai-responses]
api_key = "azure-test-key"

[model."azure-openai-responses/gpt-4"]
model = "gpt-4"
base_url = "https://override.openai.azure.com"
query_params = { custom = "preserved" }
"#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        let mut models = resolve_model_list(&cfg, None);
        let entry = models
            .get("azure-openai-responses/gpt-4")
            .expect("managed Azure entry");
        assert_eq!(entry.info.model, "managed-gpt4-deployment");
        assert_eq!(
            entry.info.base_url,
            "https://override.openai.azure.com/openai/v1"
        );
        assert_eq!(
            entry
                .info
                .query_params
                .get("api-version")
                .map(String::as_str),
            Some("2026-08-01-preview")
        );
        assert_eq!(
            entry.info.query_params.get("custom").map(String::as_str),
            Some("preserved")
        );

        apply_platform_credentials_with_bearer(
            &mut models,
            &cfg.platforms,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let restamped = models
            .get("azure-openai-responses/gpt-4")
            .expect("restamped Azure entry");
        assert_eq!(restamped.info.model, "managed-gpt4-deployment");
        assert_eq!(
            restamped
                .info
                .query_params
                .get("api-version")
                .map(String::as_str),
            Some("2026-08-01-preview")
        );
    }

    #[test]
    #[serial]
    fn kimi_static_api_key_wins_over_oauth_marker_without_device_identity() {
        let (_dir, _guards) = isolated_auth_home();
        let _static_key = EnvGuard::set(
            xai_grok_models::KIMI_CODE_API_KEY_ENV,
            "static-kimi-test-key",
        );
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let mut models = resolve_model_list(&cfg, None);
        apply_platform_credentials_with_bearer(
            &mut models,
            &cfg.platforms,
            Some("oauth-marker-must-lose".into()),
            None,
            None,
            None,
            None,
            None,
        );
        let entry = models.get("kimi-code/k2p7").expect("kimi-code entry");
        assert!(!entry.platform_oauth_active);
        assert!(entry.has_own_credentials());
        assert!(kimi_code_bearer_resolver_for_model(entry).is_none());

        let sampler = sampling_config_for_model(
            entry,
            resolve_credentials(entry, None),
            None,
            None,
            None,
            None,
        );
        assert_eq!(sampler.api_key.as_deref(), Some("static-kimi-test-key"));
        assert!(sampler.bearer_resolver.is_none());
        assert!(sampler.extra_headers.keys().all(|name| {
            !name.eq_ignore_ascii_case("x-msh-device-id")
                && !name.eq_ignore_ascii_case("x-msh-device-name")
                && !name.eq_ignore_ascii_case("x-msh-device-model")
        }));
        assert_eq!(
            sampler
                .extra_headers
                .get("anthropic-version")
                .map(String::as_str),
            Some(xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE)
        );
    }

    #[test]
    #[serial]
    fn expired_refreshable_kimi_session_skips_relogin_on_restart() {
        let (dir, _guards) = isolated_auth_home();
        crate::auth::store_kimi_code_auth(
            dir.path(),
            &crate::auth::GrokAuth {
                key: "expired-kimi-access".into(),
                auth_mode: crate::auth::AuthMode::KimiCode,
                create_time: chrono::Utc::now() - chrono::Duration::hours(2),
                expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
                refresh_token: Some("persisted-refresh".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let models = resolve_model_list(&cfg, None);
        let entry = models
            .get("kimi-code/k2p7")
            .expect("Kimi builtin must remain in the catalog");

        assert_eq!(entry.api_key.as_deref(), Some("expired-kimi-access"));
        assert!(entry.info.supported_in_api);
        assert!(entry.visible_for_auth(false));
        assert!(
            crate::agent::auth_method::should_advertise_xai_api_key(false, models.values()),
            "a persisted refreshable Kimi session must select a non-interactive startup auth path"
        );

        let sampler = sampling_config_for_model(
            entry,
            resolve_credentials(entry, None),
            None,
            None,
            None,
            None,
        );
        assert!(
            sampler.bearer_resolver.is_some(),
            "the expired catalog marker must be replaced by the live Kimi resolver"
        );
        assert_eq!(
            sampler.adapter_kind,
            xai_grok_models::AdapterKind::KimiCoding
        );
    }

    #[test]
    #[serial]
    fn expired_refreshable_codex_session_stays_visible_without_startup_refresh() {
        let (dir, _guards) = isolated_auth_home();
        crate::auth::store_openai_codex_auth(
            dir.path(),
            &crate::auth::GrokAuth {
                key: "expired-codex-access".into(),
                auth_mode: crate::auth::AuthMode::OpenAiCodex,
                create_time: chrono::Utc::now() - chrono::Duration::hours(2),
                expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
                refresh_token: Some("persisted-refresh".into()),
                account_id: Some("acct-test".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let models = resolve_model_list(&cfg, None);
        let entry = models
            .get("openai-codex/gpt-5.6-sol")
            .expect("Codex builtin must remain in the catalog");

        assert_eq!(entry.api_key.as_deref(), Some("expired-codex-access"));
        assert!(entry.info.supported_in_api);
        assert!(entry.visible_for_auth(false));

        let sampler = sampling_config_for_model(
            entry,
            resolve_credentials(entry, None),
            None,
            None,
            None,
            None,
        );
        assert!(sampler.bearer_resolver.is_some());
        assert!(sampler.responses_codex_dialect);
        assert_eq!(
            sampler.adapter_kind,
            xai_grok_models::AdapterKind::OpenAiCodex
        );
    }

    #[test]
    #[serial]
    fn apply_platform_credentials_without_bearer_keeps_codex_locked() {
        let (_dir, _guards) = isolated_auth_home();
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let mut models = resolve_model_list(&cfg, None);
        apply_platform_credentials_with_bearer(
            &mut models,
            &cfg.platforms,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let codex_entry = models
            .get("openai-codex/gpt-5.6-sol")
            .expect("openai-codex entry");
        assert!(
            !codex_entry.info.supported_in_api,
            "no credential → Codex entry stays locked"
        );
        assert!(codex_entry.api_key.is_none());
    }

    #[test]
    #[serial]
    fn github_copilot_oauth_stamping_honors_availability_list() {
        let (_dir, _guards) = isolated_auth_home();
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let github_ids: Vec<_> = xai_grok_models::platform_builtin_models()
            .iter()
            .filter(|model| model.provider.as_str() == "github-copilot")
            .take(2)
            .map(|model| model.model.clone())
            .collect();
        assert!(
            github_ids.len() >= 2,
            "github-copilot catalog must have at least two models"
        );

        let first_key = format!("github-copilot/{}", github_ids[0]);
        let second_key = format!("github-copilot/{}", github_ids[1]);
        let mut models = resolve_model_list(&cfg, None);
        apply_platform_credentials_with_bearer(
            &mut models,
            &cfg.platforms,
            None,
            None,
            None,
            Some("oauth-copilot-token".into()),
            None,
            Some(vec![github_ids[0].clone()]),
        );
        let first = models.get(&first_key).expect("first copilot entry");
        assert_eq!(first.api_key.as_deref(), Some("oauth-copilot-token"));
        assert!(first.platform_oauth_active);
        assert!(first.info.supported_in_api);
        let second = models.get(&second_key).expect("second copilot entry");
        assert!(
            second.api_key.is_none(),
            "unavailable Copilot model stays locked"
        );
        assert!(!second.platform_oauth_active);

        let mut compat_models = resolve_model_list(&cfg, None);
        apply_platform_credentials_with_bearer(
            &mut compat_models,
            &cfg.platforms,
            None,
            None,
            None,
            Some("oauth-copilot-token".into()),
            None,
            None,
        );
        assert_eq!(
            compat_models
                .get(&second_key)
                .and_then(|entry| entry.api_key.as_deref()),
            Some("oauth-copilot-token"),
            "old credentials without availability remain backward-compatible"
        );

        let _static_token = EnvGuard::set(
            crate::auth::github_copilot::COPILOT_GITHUB_TOKEN_ENV,
            "static-copilot-token",
        );
        let mut static_models = resolve_model_list(&cfg, None);
        apply_platform_credentials_with_bearer(
            &mut static_models,
            &cfg.platforms,
            None,
            None,
            None,
            Some("oauth-copilot-token".into()),
            None,
            Some(Vec::new()),
        );
        let static_second = static_models
            .get(&second_key)
            .expect("static copilot entry");
        assert!(
            static_second.info.supported_in_api,
            "static COPILOT_GITHUB_TOKEN is authoritative and ignores OAuth availability"
        );
        assert!(!static_second.platform_oauth_active);
    }

    #[test]
    #[serial]
    fn kimi_code_auth_storage_roundtrips() {
        let _auth_path = EnvGuard::unset("GROK_AUTH_PATH");
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let auth = crate::auth::GrokAuth {
            key: "kimi-access-token".into(),
            auth_mode: crate::auth::AuthMode::KimiCode,
            create_time: chrono::Utc::now(),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            refresh_token: Some("rt".into()),
            ..Default::default()
        };
        crate::auth::store_kimi_code_auth(home, &auth).unwrap();
        let loaded = crate::auth::read_kimi_code_auth(home).expect("stored");
        assert_eq!(loaded.key, "kimi-access-token");
        assert_eq!(loaded.auth_mode, crate::auth::AuthMode::KimiCode);
        // Sibling scopes must survive: write an API key too.
        crate::auth::store_api_key(home, "xai-key").unwrap();
        assert!(crate::auth::read_kimi_code_auth(home).is_some());
        assert_eq!(crate::auth::read_api_key(home).as_deref(), Some("xai-key"));
        crate::auth::clear_kimi_code_auth(home).unwrap();
        assert!(crate::auth::read_kimi_code_auth(home).is_none());
        assert_eq!(crate::auth::read_api_key(home).as_deref(), Some("xai-key"));
    }

    #[test]
    fn inject_url_derived_headers_adds_kimi_device_identity() {
        let mut headers = IndexMap::new();
        inject_url_derived_headers(&mut headers, None, "https://api.kimi.com/coding/v1");
        assert!(
            headers.get("X-Msh-Device-Id").is_some() || headers.get("X-Msh-Device-Name").is_some(),
            "expected Kimi device headers when home is writable: {headers:?}"
        );
        let mut external = IndexMap::new();
        inject_url_derived_headers(&mut external, None, "https://api.example.com/v1");
        assert!(external.get("X-Msh-Device-Id").is_none());
    }

    #[test]
    #[serial]
    fn platforms_config_stamps_api_key_when_env_absent() {
        let (_dir, _guards) = isolated_auth_home();
        let raw: toml::Value = toml::from_str(
            r#"
[platforms.moonshot-cn]
api_key = "sk-test-cn-key-not-for-prod"
"#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        assert_eq!(
            cfg.platforms
                .config_api_key(xai_grok_models::PlatformId::MoonshotCn)
                .as_deref(),
            Some("sk-test-cn-key-not-for-prod")
        );
        let models = resolve_model_list(&cfg, None);
        let entry = models
            .get("moonshot-cn/kimi-k2-turbo-preview")
            .expect("moonshot entry");
        // Without the env set, config key is stamped onto the entry.
        assert_eq!(
            entry.api_key.as_deref(),
            Some("sk-test-cn-key-not-for-prod")
        );
        assert!(entry.has_own_credentials());
        let creds = resolve_credentials(entry, None);
        assert_eq!(
            creds.api_key.as_deref(),
            Some("sk-test-cn-key-not-for-prod")
        );
        assert!(creds.base_url.contains("moonshot.cn"));
        assert!(
            entry.info.supported_in_api,
            "moonshot entry should become visible once credentials are stamped"
        );
    }

    #[test]
    #[serial]
    fn kimi_code_oauth_reveals_subscription_models() {
        let (dir, _guards) = isolated_auth_home();
        let auth = crate::auth::GrokAuth {
            key: "kimi-access-token".into(),
            auth_mode: crate::auth::AuthMode::KimiCode,
            create_time: chrono::Utc::now(),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            refresh_token: Some("rt".into()),
            ..Default::default()
        };
        crate::auth::store_kimi_code_auth(dir.path(), &auth).unwrap();

        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(Default::default())).unwrap();
        let models = resolve_model_list(&cfg, None);
        let entry = models.get("kimi-code/k2p7").expect("kimi-code k2p7 entry");
        assert_eq!(
            entry.info.api_backend,
            ApiBackend::Messages,
            "Kimi Code subscription uses Anthropic Messages (Pi)"
        );
        assert!(
            entry.info.supported_in_api,
            "Kimi Code entry visible after login"
        );
        assert_eq!(entry.api_key.as_deref(), Some("kimi-access-token"));
    }

    #[test]
    fn resolve_platform_api_key_env_wins_over_config() {
        let mut platforms = PlatformsConfig::default();
        platforms.entries.insert(
            "moonshot-cn".into(),
            PlatformCredentialConfig {
                api_key: Some("from-config".into()),
            },
        );
        let key = resolve_platform_api_key_with(
            xai_grok_models::PlatformId::MoonshotCn,
            &platforms,
            |name| {
                if name == xai_grok_models::MOONSHOT_CN_API_KEY_ENV {
                    Some("from-env".into())
                } else {
                    None
                }
            },
        );
        assert_eq!(key.as_deref(), Some("from-env"));
    }

    #[test]
    fn resolve_platform_api_key_falls_back_to_generic_env() {
        let platforms = PlatformsConfig::default();
        let key = resolve_platform_api_key_with(
            xai_grok_models::PlatformId::MoonshotAi,
            &platforms,
            |name| {
                if name == xai_grok_models::MOONSHOT_API_KEY_ENV {
                    Some("generic-key".into())
                } else {
                    None
                }
            },
        );
        assert_eq!(key.as_deref(), Some("generic-key"));
    }

    #[test]
    fn model_override_wins_over_platform_stamp() {
        let raw: toml::Value = toml::from_str(
            r#"
[platforms.moonshot-cn]
api_key = "platform-key"

[model."moonshot-cn/kimi-k2-turbo-preview"]
api_key = "per-model-key"
"#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        let models = resolve_model_list(&cfg, None);
        let entry = models
            .get("moonshot-cn/kimi-k2-turbo-preview")
            .expect("entry");
        assert_eq!(entry.api_key.as_deref(), Some("per-model-key"));
    }

    #[test]
    fn third_party_api_key_route_covers_arbitrary_custom_base_url() {
        assert!(
            is_third_party_api_key_route("my-vllm", "https://llm.example.com/v1"),
            "official-style [model.*] hosts must fail-close the xAI session bearer"
        );
        assert!(
            is_third_party_api_key_route("local", "http://127.0.0.1:8000/v1"),
            "loopback custom endpoints are third-party"
        );
        assert!(
            is_third_party_api_key_route("ollama/glm-5.2", "http://127.0.0.1:11434/v1"),
            "managed catalog id still matches even on an unrecognized host"
        );
        assert!(
            !is_third_party_api_key_route("grok-4", "https://api.x.ai/v1"),
            "first-party xAI remains a session-token route"
        );
    }

}

