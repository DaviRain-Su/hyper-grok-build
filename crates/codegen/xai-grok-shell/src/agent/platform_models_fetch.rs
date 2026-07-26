//! Live `GET {base}/models` sync for built-in platforms (Kimi Code + Moonshot).
//!
//! After OAuth / API-key credentials are present, replace offline fallback
//! catalog entries with the server listing (K3 and other subscription models
//! appear here — the offline list is only a last resort).
//!
//! **Tokio safety:** all `reqwest::blocking` work runs via
//! [`run_blocking_io`] so it never panics on the ACP agent worker's
//! current-thread runtime ("Cannot drop a runtime in a context where
//! blocking is not allowed").

use indexmap::IndexMap;
use xai_grok_models::{
    NEXUS_BASE_URL_DEFAULT, PlatformId, WireModel, WireModelsResponse, WireThinkEfforts,
    nexus_chat_base, nexus_messages_base, nexus_normalize_root,
};
use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

use crate::agent::config::{
    EnvKeys, ModelEntry, ModelEntryConfig, PlatformsConfig, default_agent_type,
    resolve_platform_api_key,
};
use crate::sampling::ApiBackend;

/// Default context when the wire omits `context_length`.
const DEFAULT_CONTEXT_WINDOW: u64 = 256_000;

/// Errors from a single-platform or multi-platform models fetch.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PlatformModelsError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Request failed: {status} - {body}")]
    RequestFailed { status: u16, body: String },
    #[error("Auth error: {0}")]
    Auth(String),
}

/// Run blocking I/O without panicking inside a Tokio async context.
///
/// - **Multi-thread** runtime → `block_in_place` (allowed).
/// - **Current-thread** runtime (ACP `acp-agent-worker`) → dedicated OS
///   thread with **no** Tokio handle (reqwest blocking may create+drop its
///   own runtime there safely).
/// - **No** runtime → run inline.
pub(crate) fn run_blocking_io<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        Ok(_) => std::thread::scope(|s| match s.spawn(f).join() {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }),
        Err(_) => f(),
    }
}

/// Load `[platforms.*]` from the effective config (env keys still win).
pub(crate) fn load_platforms_config() -> PlatformsConfig {
    crate::config::load_effective_config()
        .ok()
        .and_then(|v| v.get("platforms")?.clone().try_into().ok())
        .unwrap_or_default()
}

/// Platforms with usable credentials, registry order (subscription first).
fn enabled_platforms(has_kimi_oauth: bool, platforms: &PlatformsConfig) -> Vec<PlatformId> {
    PlatformId::ALL
        .into_iter()
        .filter(|p| {
            if !p.live_models_list_enabled() {
                return false;
            }
            if *p == PlatformId::KimiCode {
                has_kimi_oauth
            } else {
                resolve_platform_api_key(*p, platforms).is_some()
            }
        })
        .collect()
}

/// Fetch live catalog entries for every platform that has credentials.
///
/// Returns `None` when no platform is enabled or every fetch fails (caller
/// keeps offline builtins). Never logs credential values.
///
/// Safe to call from the ACP agent async task (uses [`run_blocking_io`]).
pub(crate) fn fetch_enabled_platform_models_blocking(
    platforms: &PlatformsConfig,
) -> Option<IndexMap<String, ModelEntry>> {
    let platforms = platforms.clone();
    run_blocking_io(move || fetch_enabled_platform_models_inner(&platforms))
}

fn fetch_enabled_platform_models_inner(
    platforms: &PlatformsConfig,
) -> Option<IndexMap<String, ModelEntry>> {
    let kimi_bearer = crate::auth::kimi::kimi_code_access_token_cached();
    let enabled = enabled_platforms(kimi_bearer.is_some(), platforms);
    if enabled.is_empty() {
        tracing::debug!("platform models fetch skipped: no platform credentials");
        return None;
    }

    let mut map = IndexMap::new();
    let mut successes = 0usize;
    for platform in enabled {
        let bearer = if platform == PlatformId::KimiCode {
            kimi_bearer
                .clone()
                .expect("enabled_platforms gated on Kimi OAuth presence")
        } else {
            resolve_platform_api_key(platform, platforms)
                .expect("enabled_platforms gated on key presence")
        };
        match fetch_one_platform_models(platform, &bearer) {
            Ok(entries) => {
                tracing::info!(
                    platform = platform.as_str(),
                    count = entries.len(),
                    "platform models fetch succeeded"
                );
                successes += 1;
                for entry in entries {
                    let key = entry.id.clone().unwrap_or_else(|| entry.model.clone());
                    map.insert(key, ModelEntry::from_config_entry(&entry));
                }
            }
            Err(e) => {
                tracing::warn!(
                    platform = platform.as_str(),
                    error = %e,
                    "platform models fetch failed"
                );
            }
        }
    }
    if successes == 0 || map.is_empty() {
        return None;
    }
    Some(map)
}

/// `GET {platform.base}/models` with Bearer auth; map through the F4 wire
/// contract and the platform prefix filter.
fn fetch_one_platform_models(
    platform: PlatformId,
    bearer: &str,
) -> Result<Vec<ModelEntryConfig>, PlatformModelsError> {
    // Nexus exposes two model catalogs on different bases (OpenAI-style
    // `{R}/openai/v1/models` for chat/completions + Anthropic-style
    // `{R}/v1/models` for native Claude Messages). Discover both.
    if platform == PlatformId::Nexus {
        return fetch_nexus_models(bearer);
    }
    let client = crate::http::shared_blocking_client();
    let url = platform.models_list_url();
    tracing::info!(platform = platform.as_str(), %url, "fetching platform models");
    let mut request = client
        .get(&url)
        .header("Authorization", format!("Bearer {bearer}"));
    // Kimi Code subscription expects the same device-identity headers as
    // OAuth / inference; open platforms do not.
    if platform == PlatformId::KimiCode {
        match crate::auth::kimi::device_headers() {
            Ok(headers) => {
                for (name, value) in headers {
                    request = request.header(name, value);
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "platform models fetch: could not attach Kimi device headers"
                );
            }
        }
    }
    let response = request.send()?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().unwrap_or_default();
        return Err(PlatformModelsError::RequestFailed { status, body });
    }
    let listing: WireModelsResponse = response.json()?;
    let total = listing.data.len();
    let filtered = xai_grok_models::filter_allowed_models(platform, listing.data);
    if filtered.len() != total {
        tracing::info!(
            platform = platform.as_str(),
            total,
            kept = filtered.len(),
            "applied platform model-prefix filter"
        );
    }
    let base_url = platform.base_url();
    Ok(filtered
        .into_iter()
        .map(|wire| platform_wire_model_to_entry(platform, wire, &base_url))
        .collect())
}

/// Resolve the Nexus gateway root `R`.
///
/// Priority: `GROK_NEXUS_BASE_URL` env > per-account root persisted at login
/// (`~/.grok/auth.json` `platform/nexus`) > compiled default. Any client-view
/// shape is normalized to the bare root via [`nexus_normalize_root`].
fn resolve_nexus_root() -> String {
    let raw = std::env::var("GROK_NEXUS_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| crate::auth::read_platform_base_url(&xai_grok_config::grok_home(), "nexus"))
        .unwrap_or_else(|| NEXUS_BASE_URL_DEFAULT.to_string());
    nexus_normalize_root(&raw)
}

/// Discover Nexus models from both protocol catalogs and tag each entry with
/// the backend + base it must use. ChatCompletions entries keep the primary
/// key `nexus/<id>`; Claude Messages entries use `nexus/<id>@messages` so the
/// same `claude-*` model surfaces once per protocol without key collision.
///
/// Either endpoint succeeding is enough (partial catalogs beat none); only an
/// all-endpoints failure propagates `Err` so the caller keeps offline builtins.
fn fetch_nexus_models(bearer: &str) -> Result<Vec<ModelEntryConfig>, PlatformModelsError> {
    let root = resolve_nexus_root();
    let chat_base = nexus_chat_base(&root);
    let messages_base = nexus_messages_base(&root);

    let mut out: IndexMap<String, ModelEntryConfig> = IndexMap::new();
    let mut last_err: Option<PlatformModelsError> = None;
    let mut any_ok = false;

    // Endpoint A — OpenAI-style: {R}/openai/v1/models → chat/completions.
    match fetch_nexus_endpoint(bearer, &chat_base) {
        Ok(wires) => {
            any_ok = true;
            for wire in wires {
                let entry = nexus_wire_to_entry(wire, ApiBackend::ChatCompletions, &chat_base, "");
                out.insert(entry_key(&entry), entry);
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, base = %chat_base, "nexus chat models fetch failed");
            last_err = Some(e);
        }
    }

    // Endpoint B — Anthropic-style: {R}/v1/models → native Claude Messages.
    match fetch_nexus_endpoint(bearer, &messages_base) {
        Ok(wires) => {
            any_ok = true;
            for wire in wires {
                let entry =
                    nexus_wire_to_entry(wire, ApiBackend::Messages, &messages_base, "@messages");
                out.insert(entry_key(&entry), entry);
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, base = %messages_base, "nexus messages models fetch failed");
            last_err = Some(e);
        }
    }

    if any_ok {
        Ok(out.into_values().collect())
    } else {
        Err(last_err.unwrap_or_else(|| PlatformModelsError::Auth("no nexus endpoint".into())))
    }
}

/// `GET {base}/models` with Bearer auth; parse the wire listing.
fn fetch_nexus_endpoint(bearer: &str, base: &str) -> Result<Vec<WireModel>, PlatformModelsError> {
    let client = crate::http::shared_blocking_client();
    let url = format!("{}/models", base.trim_end_matches('/'));
    tracing::info!(%url, "fetching nexus models");
    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {bearer}"))
        .send()?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().unwrap_or_default();
        return Err(PlatformModelsError::RequestFailed { status, body });
    }
    let listing: WireModelsResponse = response.json()?;
    Ok(listing.data)
}

/// Catalog key for a live entry (id, falling back to model).
fn entry_key(entry: &ModelEntryConfig) -> String {
    entry.id.clone().unwrap_or_else(|| entry.model.clone())
}

/// Map one Nexus wire model to a catalog entry with an explicit backend/base.
///
/// Nexus speaks **Bearer** on every backend (unlike Anthropic/MiniMax, whose
/// Messages backend forces `x-api-key`), so `auth_scheme` stays `None`. The
/// `key_suffix` distinguishes the same model id across protocols
/// (`""` for chat, `"@messages"` for native Claude).
fn nexus_wire_to_entry(
    wire: WireModel,
    api_backend: ApiBackend,
    base_url: &str,
    key_suffix: &str,
) -> ModelEntryConfig {
    let platform = PlatformId::Nexus;
    let think_efforts = wire.think_efforts.as_ref().filter(|t| t.support);
    let context_window = std::num::NonZeroU64::new(wire.context_length)
        .unwrap_or_else(|| std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW).expect("non-zero"));
    let reasoning_efforts = think_efforts
        .map(think_efforts_to_options)
        .unwrap_or_default();
    let supports_reasoning = wire.supports_reasoning
        || wire.capabilities().iter().any(|c| {
            matches!(
                c,
                xai_grok_models::ModelCapability::Thinking
                    | xai_grok_models::ModelCapability::AlwaysThinking
            )
        });
    let display = wire.display_name.clone().unwrap_or_else(|| wire.id.clone());
    let name = if api_backend == ApiBackend::Messages {
        format!("{display} (Messages)")
    } else {
        display
    };
    let mut extra_headers = IndexMap::new();
    if api_backend == ApiBackend::Messages {
        extra_headers.insert(
            "anthropic-version".into(),
            xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE.into(),
        );
    }
    ModelEntryConfig {
        id: Some(format!(
            "{}{key_suffix}",
            platform.managed_model_key(&wire.id)
        )),
        name: Some(name),
        model: wire.id,
        base_url: base_url.to_owned(),
        description: None,
        max_completion_tokens: wire.max_output_tokens,
        temperature: None,
        top_p: None,
        api_key: None,
        env_key: Some(EnvKeys::new(platform.api_key_env_names().iter().copied())),
        api_backend,
        auth_scheme: None,
        reasoning_effort: think_efforts
            .and_then(|t| t.default_effort.as_deref())
            .and_then(|s| s.parse().ok()),
        supports_reasoning_effort: think_efforts.is_some() || supports_reasoning,
        reasoning_efforts,
        extra_headers,
        context_window,
        auto_compact_threshold_percent: None,
        system_prompt_label: None,
        api_base_url: None,
        use_concise: false,
        agent_type: default_agent_type(),
        inference_idle_timeout_secs: None,
        max_retries: None,
        hidden: false,
        supported_in_api: true,
        supports_backend_search: false,
        compactions_remaining: None,
        compaction_at_tokens: None,
        show_model_fingerprint: false,
        stream_tool_calls: None,
        laziness_detector: Default::default(),
    }
}

/// Map live `think_efforts` to catalog options. Wire tokens stay as option
/// ids (`"max"` → label `"Max"`); values parse via [`ReasoningEffort`]
/// (`"max"` → `Xhigh`). Unknown tokens are dropped.
pub(crate) fn think_efforts_to_options(think: &WireThinkEfforts) -> Vec<ReasoningEffortOption> {
    think
        .valid_efforts
        .iter()
        .filter_map(|token| {
            let value = match token.parse::<ReasoningEffort>() {
                Ok(v) => v,
                Err(error) => {
                    tracing::warn!(%token, %error, "unknown think_efforts token; dropping");
                    return None;
                }
            };
            let mut label = token.clone();
            if let Some(first) = label.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            Some(ReasoningEffortOption {
                id: token.clone(),
                value,
                label,
                description: None,
                default: think.default_effort.as_deref() == Some(token.as_str()),
            })
        })
        .collect()
}

/// Map one F4 wire model to a catalog entry.
///
/// SECURITY: open-platform entries carry only env-var NAMES (`env_key`) —
/// never key values — because fetched maps may be merged into the models
/// disk cache. Config-file keys are stamped later by
/// `apply_platform_credentials`.
pub(crate) fn platform_wire_model_to_entry(
    platform: PlatformId,
    wire: WireModel,
    base_url: &str,
) -> ModelEntryConfig {
    let think_efforts = wire.think_efforts.as_ref().filter(|t| t.support);
    let context_window = std::num::NonZeroU64::new(wire.context_length).unwrap_or_else(|| {
        tracing::debug!(
            model = %wire.id,
            default = DEFAULT_CONTEXT_WINDOW,
            "platform model missing context_length; using default"
        );
        std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW).expect("non-zero")
    });
    let env_key = (!platform.uses_oauth())
        .then(|| EnvKeys::new(platform.api_key_env_names().iter().copied()));
    let reasoning_efforts = think_efforts
        .map(think_efforts_to_options)
        .unwrap_or_default();
    let supports_reasoning = wire.supports_reasoning
        || wire.capabilities().iter().any(|c| {
            matches!(
                c,
                xai_grok_models::ModelCapability::Thinking
                    | xai_grok_models::ModelCapability::AlwaysThinking
            )
        });
    ModelEntryConfig {
        id: Some(platform.managed_model_key(&wire.id)),
        name: Some(wire.display_name.clone().unwrap_or_else(|| wire.id.clone())),
        model: wire.id,
        base_url: base_url.to_owned(),
        description: None,
        // Kimi thinking + tool loops need a large cap (docs: default 32k).
        max_completion_tokens: Some(xai_grok_models::KIMI_DEFAULT_MAX_TOKENS),
        // Fixed-sampling models error if non-default temperature/top_p is sent.
        temperature: None,
        top_p: None,
        api_key: None,
        env_key,
        // Official Pi kimi-coding uses anthropic-messages; open platforms stay
        // on OpenAI-compatible chat completions.
        api_backend: if platform == PlatformId::KimiCode {
            ApiBackend::Messages
        } else {
            ApiBackend::ChatCompletions
        },
        auth_scheme: None,
        reasoning_effort: think_efforts
            .and_then(|t| t.default_effort.as_deref())
            .and_then(|s| s.parse().ok()),
        // Live think levels, or any reasoning-capable model without levels.
        supports_reasoning_effort: think_efforts.is_some() || supports_reasoning,
        reasoning_efforts,
        extra_headers: {
            let mut headers = IndexMap::new();
            if platform == PlatformId::KimiCode {
                headers.insert("User-Agent".into(), "KimiCLI/1.5".into());
                headers.insert(
                    "anthropic-version".into(),
                    xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE.into(),
                );
            }
            headers
        },
        context_window,
        auto_compact_threshold_percent: None,
        system_prompt_label: None,
        api_base_url: None,
        use_concise: false,
        agent_type: default_agent_type(),
        inference_idle_timeout_secs: None,
        max_retries: None,
        hidden: false,
        // Subscription requires OAuth stamp; open platforms are API-key ready.
        supported_in_api: !platform.uses_oauth(),
        supports_backend_search: false,
        compactions_remaining: None,
        compaction_at_tokens: None,
        show_model_fingerprint: false,
        stream_tool_calls: None,
        laziness_detector: Default::default(),
    }
}

/// Merge live platform entries into a prefetched map (overwrites same keys).
pub(crate) fn merge_platform_models(
    map: &mut IndexMap<String, ModelEntry>,
    platform: IndexMap<String, ModelEntry>,
) {
    for (key, entry) in platform {
        tracing::debug!(
            model_key = %key,
            "merging live platform model into catalog prefetch"
        );
        map.insert(key, entry);
    }
}

/// Fetch + merge helper used by startup prefetch and post-login restamp.
pub(crate) fn fetch_and_merge_platform_models(
    map: Option<IndexMap<String, ModelEntry>>,
    platforms: &PlatformsConfig,
) -> Option<IndexMap<String, ModelEntry>> {
    let platform = fetch_enabled_platform_models_blocking(platforms);
    match (map, platform) {
        (Some(mut base), Some(p)) => {
            merge_platform_models(&mut base, p);
            Some(base)
        }
        (None, Some(p)) => Some(p),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_models::WireThinkEfforts;

    #[test]
    fn kimi_oauth_token_only_enables_kimi_live_discovery() {
        let enabled = enabled_platforms(true, &PlatformsConfig::default());
        assert!(enabled.contains(&PlatformId::KimiCode));
        assert!(
            !enabled.contains(&PlatformId::AnthropicClaude),
            "Claude live discovery must not be gated by or receive a Kimi token"
        );
        assert!(
            !enabled.contains(&PlatformId::OpenAiCodex),
            "Codex live discovery must not be gated by or receive a Kimi token"
        );
    }

    #[test]
    fn think_efforts_maps_max_token_to_max_variant() {
        // Note: the wire token `"max"` parses to `ReasoningEffort::Max`
        // directly (see `ReasoningEffort::from_str` in xai-grok-sampling-types).
        // Historically `"max"` was the wire alias for `Xhigh`; once `Max`
        // became a first-class variant the parse became identity. This test
        // pins the current mapping so a silent revert is caught.
        let think = WireThinkEfforts {
            support: true,
            valid_efforts: vec!["low".into(), "high".into(), "max".into()],
            default_effort: Some("max".into()),
        };
        let opts = think_efforts_to_options(&think);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].value, ReasoningEffort::Low);
        assert_eq!(opts[1].value, ReasoningEffort::High);
        assert_eq!(opts[2].value, ReasoningEffort::Max);
        assert_eq!(opts[2].id, "max");
        assert_eq!(opts[2].label, "Max");
        assert!(opts[2].default);
        assert!(!opts[0].default);
    }

    #[test]
    fn wire_k3_entry_gets_catalog_key_and_efforts() {
        let wire = WireModel {
            id: "k3".into(),
            context_length: 1_048_576,
            max_output_tokens: None,
            supports_reasoning: true,
            supports_image_in: true,
            supports_video_in: true,
            display_name: Some("K3".into()),
            supports_thinking_type: Some("only".into()),
            think_efforts: Some(WireThinkEfforts {
                support: true,
                valid_efforts: vec!["low".into(), "high".into(), "max".into()],
                default_effort: Some("max".into()),
            }),
        };
        let entry = platform_wire_model_to_entry(
            PlatformId::KimiCode,
            wire,
            "https://api.kimi.com/coding/v1",
        );
        assert_eq!(entry.id.as_deref(), Some("kimi-code/k3"));
        assert_eq!(entry.model, "k3");
        assert_eq!(entry.name.as_deref(), Some("K3"));
        assert_eq!(entry.context_window.get(), 1_048_576);
        assert!(entry.supports_reasoning_effort);
        assert_eq!(entry.reasoning_effort, Some(ReasoningEffort::Max));
        assert_eq!(entry.reasoning_efforts.len(), 3);
        assert!(!entry.supported_in_api, "OAuth-gated until stamp");
        assert_eq!(
            entry.api_backend,
            ApiBackend::Messages,
            "Kimi Code live listing must use Anthropic Messages (Pi)"
        );
        assert!(entry.env_key.is_none());
    }

    #[test]
    fn open_platform_entry_carries_env_key_not_secret() {
        let wire = WireModel {
            id: "kimi-k2-turbo-preview".into(),
            context_length: 262_144,
            max_output_tokens: None,
            supports_reasoning: true,
            supports_image_in: true,
            supports_video_in: true,
            display_name: None,
            supports_thinking_type: None,
            think_efforts: None,
        };
        let entry = platform_wire_model_to_entry(
            PlatformId::MoonshotCn,
            wire,
            "https://api.moonshot.cn/v1",
        );
        assert_eq!(
            entry.id.as_deref(),
            Some("moonshot-cn/kimi-k2-turbo-preview")
        );
        assert!(entry.supported_in_api);
        assert!(entry.env_key.is_some());
        assert!(entry.api_key.is_none());
        assert!(entry.supports_reasoning_effort);
    }

    #[test]
    fn merge_overwrites_offline_fallback_key() {
        let mut map = IndexMap::new();
        let offline = platform_wire_model_to_entry(
            PlatformId::KimiCode,
            WireModel {
                id: "k3".into(),
                context_length: 1000,
                max_output_tokens: None,
                supports_reasoning: false,
                supports_image_in: false,
                supports_video_in: false,
                display_name: Some("offline".into()),
                supports_thinking_type: None,
                think_efforts: None,
            },
            "https://api.kimi.com/coding/v1",
        );
        map.insert(
            "kimi-code/k3".into(),
            ModelEntry::from_config_entry(&offline),
        );
        let live = platform_wire_model_to_entry(
            PlatformId::KimiCode,
            WireModel {
                id: "k3".into(),
                context_length: 1_048_576,
                max_output_tokens: None,
                supports_reasoning: true,
                supports_image_in: true,
                supports_video_in: true,
                display_name: Some("K3".into()),
                supports_thinking_type: Some("only".into()),
                think_efforts: Some(WireThinkEfforts {
                    support: true,
                    valid_efforts: vec!["max".into()],
                    default_effort: Some("max".into()),
                }),
            },
            "https://api.kimi.com/coding/v1",
        );
        let mut platform = IndexMap::new();
        platform.insert("kimi-code/k3".into(), ModelEntry::from_config_entry(&live));
        merge_platform_models(&mut map, platform);
        let e = map.get("kimi-code/k3").unwrap();
        assert_eq!(e.info.context_window.get(), 1_048_576);
        assert_eq!(e.info.name.as_deref(), Some("K3"));
        assert!(e.info.supports_reasoning_effort);
    }

    fn nexus_wire(id: &str) -> WireModel {
        WireModel {
            id: id.into(),
            context_length: 200_000,
            max_output_tokens: Some(64_000),
            supports_reasoning: false,
            supports_image_in: false,
            supports_video_in: false,
            display_name: None,
            supports_thinking_type: None,
            think_efforts: None,
        }
    }

    #[test]
    fn nexus_chat_entry_is_bearer_chat_completions() {
        let entry = nexus_wire_to_entry(
            nexus_wire("claude-opus-4-8"),
            ApiBackend::ChatCompletions,
            "https://nexuscore.now/openai/v1",
            "",
        );
        assert_eq!(entry.id.as_deref(), Some("nexus/claude-opus-4-8"));
        assert_eq!(entry.model, "claude-opus-4-8");
        assert_eq!(entry.base_url, "https://nexuscore.now/openai/v1");
        assert_eq!(entry.api_backend, ApiBackend::ChatCompletions);
        // Bearer (auth_scheme None) + carries env key names, never a secret.
        assert!(entry.auth_scheme.is_none());
        assert!(entry.env_key.is_some());
        assert!(entry.api_key.is_none());
        assert!(entry.supported_in_api);
        assert_eq!(entry.max_completion_tokens, Some(64_000));
        assert_eq!(entry.context_window.get(), 200_000);
        assert!(!entry.extra_headers.contains_key("anthropic-version"));
    }

    #[test]
    fn nexus_messages_entry_is_bearer_with_suffix_and_version() {
        let entry = nexus_wire_to_entry(
            nexus_wire("claude-opus-4-8"),
            ApiBackend::Messages,
            "https://nexuscore.now/v1",
            "@messages",
        );
        // Distinct key from the chat entry → same model surfaces per protocol.
        assert_eq!(entry.id.as_deref(), Some("nexus/claude-opus-4-8@messages"));
        assert_eq!(entry.model, "claude-opus-4-8");
        assert_eq!(entry.base_url, "https://nexuscore.now/v1");
        assert_eq!(entry.api_backend, ApiBackend::Messages);
        // Nexus Messages still uses Bearer (NOT x-api-key like Anthropic).
        assert!(entry.auth_scheme.is_none());
        assert_eq!(
            entry
                .extra_headers
                .get("anthropic-version")
                .map(String::as_str),
            Some(xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE)
        );
    }
}
