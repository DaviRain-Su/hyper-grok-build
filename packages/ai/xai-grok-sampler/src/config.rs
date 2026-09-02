//! [`SamplerConfig`] is the per-request configuration handed to the sampler.
//! It deliberately does **not** alias `xai_grok_sampling_types::SamplingConfig`.
//! Aliasing would pull transitive dependencies on shell-specific types (`xai-grok-tools`, etc.) into the sampler crate.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use xai_grok_sampling_types::{
    AdapterKind, ApiBackend, CompactionAtTokens, CompactionsRemaining, DoomLoopRecoveryPolicy,
    ReasoningEffort, RequestCompat,
};

use crate::attribution::SharedAttributionCallback;
use crate::retry::{DEFAULT_MAX_RETRIES, RATE_LIMIT_RETRY_THRESHOLD};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    #[default]
    Bearer,
    XApiKey,
    /// Azure OpenAI's raw `api-key` header.
    ApiKey,
    /// Cloudflare AI Gateway's `cf-aig-authorization: Bearer …` header.
    CfAigAuthorization,
    /// Google REST `x-goog-api-key` request header.
    XGoogApiKey,
}

/// All knobs that control a single sampling request.
///
/// The session typically owns one `SamplerConfig` per active model and passes it (or a per-request override) to the actor on every submit.
///
/// # Construction in `xai-grok-shell`
///
/// `SamplerConfig` is the single source of truth for sampler configuration.
/// The shell builds it directly by composing chat-state's `xai_grok_sampling_types::SamplingConfig` with `Credentials` (api key, client version).
/// See `agent::config::resolve_model_to_sampling_config` and `session::acp_session::SessionActor::reconstruct_full_config`.
///
/// URL-derived request headers (e.g. `X-XAI-Token-Auth` for the cli-chat-proxy) land in [`Self::extra_headers`].
/// `agent::config::inject_url_derived_headers` folds them in before the `SamplerConfig` is handed to the actor.
/// Auth is selected separately via `auth_scheme`, while `api_backend` controls only the request/response protocol shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplerConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub api_backend: ApiBackend,
    /// Provider-specific adapter layered on the wire backend.
    #[serde(default)]
    pub adapter_kind: AdapterKind,
    /// Fully resolved Pi-derived compatibility for this concrete model route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_compat: Option<RequestCompat>,
    /// Relative endpoint path; backend defaults remain the legacy fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_path: Option<String>,
    #[serde(default)]
    pub auth_scheme: AuthScheme,
    /// Extra request headers applied verbatim. The sampler never inspects the URL to derive headers.
    /// Callers (the session) inject proxy auth and other access headers here before constructing the config.
    pub extra_headers: IndexMap<String, String>,
    /// Additional Responses API `include` values not represented by the typed client.
    #[serde(default)]
    pub extra_response_includes: Vec<String>,
    /// Query parameters folded into every request URL (percent-encoded).
    #[serde(default)]
    pub query_params: IndexMap<String, String>,
    /// Header name to environment variable, resolved into request headers at client build and never persisted.
    #[serde(default)]
    pub env_http_headers: IndexMap<String, String>,
    /// Total context window size in tokens.
    /// The sampler does not enforce it; the session uses it for compaction decisions.
    pub context_window: u64,
    pub force_http1: bool,
    pub max_retries: Option<u32>,
    pub stream_tool_calls: bool,
    pub idle_timeout_secs: Option<u64>,

    // Reasoning effort
    pub reasoning_effort: Option<ReasoningEffort>,

    // Client identity
    pub origin_client: Option<OriginClientInfo>,
    pub client_identifier: Option<String>,
    pub deployment_id: Option<String>,
    pub user_id: Option<String>,
    pub client_version: Option<String>,

    /// Hook invoked on every 401 response with the bearer that was actually sent on the wire.
    /// Implementations typically compare it against a live credential source to tell a stale token from a server-rejected live one.
    /// `None` (default) is a no-op; the 401 arm still returns `SamplingError::Auth`.
    ///
    /// serde skips this field; round-tripping a config drops the callback.
    /// Re-attach it before [`crate::SamplingClient::new`] when deserializing from disk, or 401 attribution is silently disabled.
    #[serde(skip)]
    pub attribution_callback: Option<SharedAttributionCallback>,

    /// Resolves a fresh bearer for each request. `None` uses the construction-time `api_key`.
    #[serde(skip)]
    pub bearer_resolver: Option<SharedBearerResolver>,

    #[serde(default)]
    pub supports_backend_search: bool,

    /// Per-model config for the `x-compactions-remaining` header; `None` disables it.
    #[serde(default)]
    pub compactions_remaining: Option<CompactionsRemaining>,

    /// Per-model config for the `x-compaction-at` header; `None` disables it.
    #[serde(default)]
    pub compaction_at_tokens: Option<CompactionAtTokens>,

    /// Server-side doom-loop check policy; `None` disables it.
    /// When set, the client sends both reporting headers on streaming Responses API requests.
    /// Those carry the configured tail window and the default exact-repetition minimum.
    /// It also absorbs the reported trigger events (unlike environment headers in [`Self::extra_headers`], this gates the client's decode behavior).
    #[serde(default)]
    pub doom_loop_recovery: Option<DoomLoopRecoveryPolicy>,

    /// Per-request header injector (e.g. OTel traceparent). Called in `post()`.
    #[serde(skip)]
    pub header_injector: Option<SharedHeaderInjector>,

    /// ChatGPT Codex backend dialect for Responses API requests. When true
    /// the client (a) moves the system prompt from `input` items into the
    /// top-level `instructions` field, (b) stamps `prompt_cache_key` from
    /// the session id for cache affinity, and (c) defaults `text.verbosity`
    /// to `low` — mirroring official Pi `openai-codex-responses.ts`.
    /// `store: false` and `include: ["reasoning.encrypted_content"]` are
    /// already unconditional sampler defaults.
    #[serde(default)]
    pub responses_codex_dialect: bool,

    /// Bedrock request metadata for AWS cost allocation tagging.
    #[serde(default)]
    pub bedrock_request_metadata: IndexMap<String, String>,
    /// Bedrock custom headers injected before SDK signing; reserved auth/SigV4 headers are blocked.
    #[serde(default)]
    pub bedrock_headers: IndexMap<String, String>,
    /// Optional AWS profile selected by `/login amazon-bedrock --profile`.
    /// Empty/None leaves the SDK default chain (including ambient AWS_PROFILE) untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_profile: Option<String>,

    /// When true, apply Moonshot/Kimi request shaping (`thinking` object,
    /// fixed-sampling strip, etc.). Must only be set for direct Moonshot /
    /// Kimi Code platforms — not for Ollama/OpenRouter/Together/Fireworks
    /// entries that happen to share the same bare model slug.
    #[serde(default)]
    pub kimi_dialect: bool,
}

impl Default for SamplerConfig {
    /// Empty defaults so callers can use `..Default::default()` and new fields don't ripple through every literal site.
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: String::new(),
            model: String::new(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::default(),
            adapter_kind: AdapterKind::default(),
            request_compat: None,
            endpoint_path: None,
            auth_scheme: AuthScheme::default(),
            extra_headers: IndexMap::new(),
            extra_response_includes: Vec::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window: 0,
            force_http1: false,
            max_retries: None,
            stream_tool_calls: false,
            idle_timeout_secs: None,
            reasoning_effort: None,
            origin_client: None,
            client_identifier: None,
            deployment_id: None,
            user_id: None,
            client_version: None,
            attribution_callback: None,
            bearer_resolver: None,
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            doom_loop_recovery: None,
            header_injector: None,
            responses_codex_dialect: false,
            bedrock_request_metadata: IndexMap::new(),
            bedrock_headers: IndexMap::new(),
            bedrock_profile: None,
            kimi_dialect: false,
        }
    }
}

/// One authoritative per-request auth resolution.
///
/// `headers` are inserted after `remove_headers` are cleared, allowing an
/// OAuth resolver to atomically align provider-specific headers with the
/// bearer it just refreshed. This avoids a second credential lookup and also
/// removes stale construction-time headers when live resolution fails.
#[derive(Debug, Clone, Default)]
pub struct BearerResolution {
    /// Current live token, or `None` when auth cannot be resolved.
    pub bearer: Option<String>,
    /// Companion headers derived from the exact same credential.
    pub headers: reqwest::header::HeaderMap,
    /// Construction-time headers that must be cleared before applying `headers`.
    pub remove_headers: Vec<reqwest::header::HeaderName>,
}

impl BearerResolution {
    /// Build a token-only resolution for providers without companion headers.
    pub fn from_bearer(bearer: Option<String>) -> Self {
        Self {
            bearer,
            ..Self::default()
        }
    }
}

/// Cheap sync read of the current bearer for [`SamplerConfig::bearer_resolver`].
pub trait BearerResolver: Send + Sync + std::fmt::Debug {
    fn current_bearer(&self) -> Option<String>;

    /// Resolve the bearer and any provider-specific companion headers once.
    /// Implementors that only provide a token inherit the legacy behavior.
    fn resolve_bearer(&self) -> BearerResolution {
        BearerResolution::from_bearer(self.current_bearer())
    }
}

pub type SharedBearerResolver = std::sync::Arc<dyn BearerResolver>;

/// Per-request header injection (e.g. OTel `traceparent`).
pub trait HeaderInjector: Send + Sync + std::fmt::Debug {
    fn inject(&self, headers: &mut reqwest::header::HeaderMap);
}

pub type SharedHeaderInjector = std::sync::Arc<dyn HeaderInjector>;

/// Retry knobs for the sampler's internal transport-error retry loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_retries: u32,
    /// After this many rate-limit (429) retries, escalate to the caller.
    /// Lower than `max_retries` because rate-limit waits can be long.
    pub rate_limit_retry_threshold: u32,
    #[serde(default)]
    pub retry_only_before_output: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            rate_limit_retry_threshold: RATE_LIMIT_RETRY_THRESHOLD,
            retry_only_before_output: false,
        }
    }
}

/// Identity of the client that originated the request, used for User-Agent rendering.
/// The shell layer composes this with platform info into a final UA string.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OriginClientInfo {
    pub product: String,
    pub version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configs serialized before the field existed must keep deserializing.
    #[test]
    fn config_without_doom_loop_recovery_deserializes_to_none() {
        let mut stripped = serde_json::to_value(SamplerConfig::default()).unwrap();
        let object = stripped.as_object_mut().unwrap();
        object.remove("doom_loop_recovery");
        object.remove("extra_response_includes");
        let config: SamplerConfig = serde_json::from_value(stripped).unwrap();
        assert!(config.doom_loop_recovery.is_none());
        assert!(config.extra_response_includes.is_empty());

        let with_policy = SamplerConfig {
            doom_loop_recovery: Some(DoomLoopRecoveryPolicy {
                max_threshold: 8,
                max_retries: 2,
                ..Default::default()
            }),
            ..Default::default()
        };
        let round_tripped: SamplerConfig =
            serde_json::from_value(serde_json::to_value(&with_policy).unwrap()).unwrap();
        assert_eq!(
            round_tripped.doom_loop_recovery,
            with_policy.doom_loop_recovery
        );
    }
}
