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
use std::sync::{LazyLock, Mutex};
use xai_grok_models::{
    NEXUS_BASE_URL_DEFAULT, PlatformId, ProviderDiscoveryMode, WireModel, WireModelsResponse,
    WireThinkEfforts, nexus_chat_base, nexus_messages_base, nexus_normalize_root,
};
use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

use crate::agent::config::{
    EnvKeys, ModelEntry, ModelEntryConfig, PlatformsConfig, default_agent_type,
    resolve_platform_api_key, resolve_provider_api_key,
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
fn enabled_platforms(has_kimi_credential: bool, platforms: &PlatformsConfig) -> Vec<PlatformId> {
    PlatformId::ALL
        .into_iter()
        .filter(|p| {
            if !p.live_models_list_enabled() {
                return false;
            }
            if *p == PlatformId::KimiCode {
                has_kimi_credential
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
    let kimi_api_key = xai_grok_models::provider_spec(PlatformId::KimiCode.as_str())
        .and_then(|provider| resolve_provider_api_key(provider, platforms));
    let kimi_oauth_bearer = if kimi_api_key.is_none() {
        crate::auth::kimi::kimi_code_access_token_cached()
    } else {
        None
    };
    let radius = fetch_radius_models_if_enabled(platforms);
    let enabled = enabled_platforms(
        kimi_api_key.is_some() || kimi_oauth_bearer.is_some(),
        platforms,
    );
    if enabled.is_empty() && radius.is_none() {
        tracing::debug!("platform models fetch skipped: no platform credentials");
        return None;
    }

    let mut map = radius.unwrap_or_default();
    let mut successes = usize::from(!map.is_empty());
    for platform in enabled {
        let (bearer, attach_kimi_device_headers) = if platform == PlatformId::KimiCode {
            if let Some(api_key) = kimi_api_key.clone() {
                (api_key, false)
            } else {
                (
                    kimi_oauth_bearer
                        .clone()
                        .expect("enabled_platforms gated on Kimi credential presence"),
                    true,
                )
            }
        } else {
            (
                resolve_platform_api_key(platform, platforms)
                    .expect("enabled_platforms gated on key presence"),
                false,
            )
        };
        match fetch_one_platform_models(platform, &bearer, attach_kimi_device_headers) {
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

fn platform_models_request(
    client: &reqwest::blocking::Client,
    platform: PlatformId,
    bearer: &str,
    attach_kimi_device_headers: bool,
) -> reqwest::blocking::RequestBuilder {
    let url = platform.models_list_url();
    tracing::info!(platform = platform.as_str(), %url, "fetching platform models");
    let mut request = client
        .get(&url)
        .header("Authorization", format!("Bearer {bearer}"));
    // Kimi OAuth expects device identity; static API-key mode deliberately
    // omits those OAuth-device headers.
    if platform == PlatformId::KimiCode && attach_kimi_device_headers {
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
    request
}

/// `GET {platform.base}/models` with Bearer auth; map through the F4 wire
/// contract and the platform prefix filter.
fn fetch_one_platform_models(
    platform: PlatformId,
    bearer: &str,
    attach_kimi_device_headers: bool,
) -> Result<Vec<ModelEntryConfig>, PlatformModelsError> {
    // Nexus exposes two model catalogs on different bases (OpenAI-style
    // `{R}/openai/v1/models` for chat/completions + Anthropic-style
    // `{R}/v1/models` for native Claude Messages). Discover both.
    if platform == PlatformId::Nexus {
        return fetch_nexus_models(bearer);
    }
    let client = crate::http::shared_startup_blocking_client();
    let request = platform_models_request(&client, platform, bearer, attach_kimi_device_headers);
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
    let client = crate::http::shared_startup_blocking_client();
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
        request_compat: None,
        endpoint_path: None,
        auth_scheme: None,
        reasoning_effort: think_efforts
            .and_then(|t| t.default_effort.as_deref())
            .and_then(|s| s.parse().ok()),
        supports_reasoning_effort: think_efforts.is_some() || supports_reasoning,
        reasoning_efforts,
        extra_headers,
        query_params: IndexMap::new(),
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
        api_backend: match platform {
            PlatformId::KimiCode => ApiBackend::Messages,
            PlatformId::OpenAiCodex => ApiBackend::Responses,
            _ => ApiBackend::ChatCompletions,
        },
        request_compat: None,
        endpoint_path: None,
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
        query_params: IndexMap::new(),
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
        supports_backend_search: platform == PlatformId::OpenAiCodex,
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

const RADIUS_CACHE_TTL_SECS: i64 = 6 * 60 * 60;
const RADIUS_CACHE_MAX_STALE_SECS: i64 = 7 * 24 * 60 * 60;
const RADIUS_CONFIG_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const RADIUS_CACHE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const RADIUS_CONFIG_MAX_BYTES: u64 = 16 * 1024 * 1024;
const RADIUS_MAX_MODELS: usize = 10_000;
static RADIUS_FETCH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RadiusConfigResponse {
    base_url: String,
    models: Vec<RadiusWireModel>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RadiusWireModel {
    id: String,
    name: String,
    reasoning: bool,
    #[serde(default)]
    thinking_level_map: std::collections::BTreeMap<String, Option<String>>,
    input: Vec<String>,
    cost: RadiusWireCost,
    context_window: u64,
    max_tokens: u64,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RadiusWireCost {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tiers: Vec<RadiusWireCostTier>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RadiusWireCostTier {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    input_tokens_above: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RadiusCacheFile {
    version: u32,
    gateway: String,
    credential_scope: String,
    fetched_at: chrono::DateTime<chrono::Utc>,
    base_url: String,
    models: Vec<RadiusWireModel>,
}

fn radius_cache_path() -> std::path::PathBuf {
    std::env::var("GROK_RADIUS_MODELS_CACHE_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| xai_grok_config::grok_home().join("radius_models_cache.json"))
}

fn radius_credential_scope(kind: &str, bearer: &str) -> String {
    use sha2::Digest as _;

    let mut digest = sha2::Sha256::new();
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(bearer.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn lock_radius_cache() -> Option<std::fs::File> {
    use fs2::FileExt as _;

    let cache_path = radius_cache_path();
    let lock_path = cache_path.with_extension("lock");
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
        && std::fs::create_dir_all(parent).is_err()
    {
        return None;
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;
    let deadline = std::time::Instant::now() + RADIUS_CACHE_LOCK_TIMEOUT;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Some(lock),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

fn radius_cache_age_seconds(fetched_at: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    let age = chrono::Utc::now()
        .signed_duration_since(fetched_at)
        .num_seconds();
    // A small amount of clock skew is harmless; a cache timestamp far in the
    // future must not stay fresh indefinitely.
    (age >= -5 * 60).then_some(age.max(0))
}

fn load_radius_cache(
    gateway: &str,
    credential_scope: &str,
    fresh_only: bool,
) -> Option<IndexMap<String, ModelEntry>> {
    let raw = std::fs::read_to_string(radius_cache_path()).ok()?;
    let cache: RadiusCacheFile = serde_json::from_str(&raw).ok()?;
    if cache.version != 2 || cache.gateway != gateway || cache.credential_scope != credential_scope
    {
        return None;
    }
    let age = radius_cache_age_seconds(cache.fetched_at)?;
    let max_age = if fresh_only {
        RADIUS_CACHE_TTL_SECS
    } else {
        RADIUS_CACHE_MAX_STALE_SECS
    };
    if age > max_age {
        return None;
    }
    validate_radius_config(&cache.base_url, &cache.models).ok()?;
    Some(radius_models_to_entries(&cache.base_url, cache.models))
}

fn store_radius_cache(
    gateway: &str,
    credential_scope: &str,
    base_url: &str,
    models: Vec<RadiusWireModel>,
) {
    use std::io::Write as _;

    let path = radius_cache_path();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    if let Err(error) = std::fs::create_dir_all(parent) {
        tracing::warn!(%error, "radius cache directory creation failed");
        return;
    }
    let cache = RadiusCacheFile {
        version: 2,
        gateway: gateway.to_string(),
        credential_scope: credential_scope.to_string(),
        fetched_at: chrono::Utc::now(),
        base_url: base_url.to_string(),
        models,
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&cache) else {
        return;
    };
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("radius_models_cache.json");
    let tmp = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!(%error, "radius cache atomic write failed");
    }
}

fn validate_radius_config(base_url: &str, models: &[RadiusWireModel]) -> anyhow::Result<()> {
    let normalized_base = crate::auth::radius::normalize_gateway_root(base_url)?;
    if normalized_base != base_url {
        anyhow::bail!("Radius config baseUrl is not normalized");
    }
    if models.len() > RADIUS_MAX_MODELS {
        anyhow::bail!("Radius config has too many models");
    }
    let mut ids = std::collections::HashSet::with_capacity(models.len());
    for model in models {
        validate_radius_wire_model(model)?;
        if !ids.insert(model.id.as_str()) {
            anyhow::bail!("Radius config contains duplicate model id `{}`", model.id);
        }
    }
    Ok(())
}

fn validate_radius_wire_model(wire: &RadiusWireModel) -> anyhow::Result<()> {
    let id = wire.id.trim();
    let name = wire.name.trim();
    if id.is_empty()
        || id != wire.id
        || id.len() > 512
        || id.chars().any(|c| c.is_ascii_control())
        || name.is_empty()
        || name != wire.name
        || name.len() > 512
        || name.chars().any(|c| c.is_ascii_control())
    {
        anyhow::bail!("Radius model has an invalid id or name");
    }
    if wire.context_window == 0
        || wire.max_tokens == 0
        || wire.max_tokens > u32::MAX as u64
        || wire.max_tokens > wire.context_window
    {
        anyhow::bail!("Radius model `{id}` has invalid token limits");
    }
    let mut inputs = std::collections::HashSet::new();
    if wire.input.is_empty()
        || !wire.input.iter().all(|value| {
            matches!(value.as_str(), "text" | "image") && inputs.insert(value.as_str())
        })
        || !inputs.contains("text")
    {
        anyhow::bail!("Radius model `{id}` has invalid input capabilities");
    }
    if !radius_cost_valid(&wire.cost) {
        anyhow::bail!("Radius model `{id}` has invalid cost rates");
    }
    for (level, mapped) in &wire.thinking_level_map {
        if !matches!(
            level.as_str(),
            "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) || mapped.as_deref().is_some_and(|value| {
            value.trim().is_empty() || value.chars().any(|c| c.is_ascii_control())
        }) {
            anyhow::bail!("Radius model `{id}` has an invalid thinkingLevelMap");
        }
    }
    Ok(())
}

fn radius_models_to_entries(
    base_url: &str,
    models: Vec<RadiusWireModel>,
) -> IndexMap<String, ModelEntry> {
    let mut map = IndexMap::new();
    for model in models {
        if let Some(entry) = radius_wire_model_to_entry(base_url, model) {
            let key = entry.id.clone().unwrap_or_else(|| entry.model.clone());
            map.insert(key, ModelEntry::from_config_entry(&entry));
        }
    }
    map
}

fn radius_wire_model_to_entry(base_url: &str, wire: RadiusWireModel) -> Option<ModelEntryConfig> {
    validate_radius_wire_model(&wire).ok()?;
    let reasoning_efforts = if wire.reasoning {
        radius_thinking_options(&wire.thinking_level_map)
    } else {
        Vec::new()
    };
    Some(ModelEntryConfig {
        id: Some(format!("radius/{}", wire.id)),
        name: Some(wire.name),
        model: wire.id,
        base_url: base_url.to_string(),
        description: None,
        max_completion_tokens: Some(wire.max_tokens as u32),
        temperature: None,
        top_p: None,
        api_key: None,
        env_key: Some(EnvKeys::new(["GROK_RADIUS_API_KEY", "RADIUS_API_KEY"])),
        api_backend: ApiBackend::PiMessages,
        request_compat: None,
        endpoint_path: None,
        auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
        reasoning_effort: reasoning_efforts
            .iter()
            .find(|option| option.default)
            .map(|option| option.value),
        supports_reasoning_effort: wire.reasoning && !reasoning_efforts.is_empty(),
        reasoning_efforts,
        extra_headers: IndexMap::new(),
        query_params: IndexMap::new(),
        context_window: std::num::NonZeroU64::new(wire.context_window)?,
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
    })
}

fn radius_cost_valid(cost: &RadiusWireCost) -> bool {
    fn rates_valid(rates: [f64; 4]) -> bool {
        rates
            .into_iter()
            .all(|value| value.is_finite() && value >= 0.0)
    }

    if !rates_valid([cost.input, cost.output, cost.cache_read, cost.cache_write]) {
        return false;
    }
    let mut previous_threshold = 0;
    cost.tiers.iter().all(|tier| {
        let threshold_valid = tier.input_tokens_above > previous_threshold;
        previous_threshold = tier.input_tokens_above;
        threshold_valid && rates_valid([tier.input, tier.output, tier.cache_read, tier.cache_write])
    })
}

fn radius_thinking_options(
    map: &std::collections::BTreeMap<String, Option<String>>,
) -> Vec<ReasoningEffortOption> {
    let mut out = Vec::new();
    for (level, gateway_value) in map {
        if gateway_value.is_none() {
            continue;
        }
        let value = match level.as_str() {
            "off" => ReasoningEffort::None,
            "minimal" => ReasoningEffort::Minimal,
            "low" => ReasoningEffort::Low,
            "medium" => ReasoningEffort::Medium,
            "high" => ReasoningEffort::High,
            "xhigh" => ReasoningEffort::Xhigh,
            "max" => ReasoningEffort::Max,
            _ => continue,
        };
        let mut label = level.clone();
        if let Some(first) = label.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        out.push(ReasoningEffortOption {
            id: level.clone(),
            value,
            label,
            description: None,
            default: level == "medium",
        });
    }
    out
}

enum RadiusDiscoveryCredential {
    ApiKey { bearer: String, gateway: String },
    OAuth { marker: String, gateway: String },
}

fn radius_api_key_gateway() -> Option<String> {
    let env_gateway = std::env::var("GROK_RADIUS_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("RADIUS_GATEWAY_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    if let Some(value) = env_gateway {
        return match crate::auth::radius::normalize_gateway_root(&value) {
            Ok(gateway) => Some(gateway),
            Err(error) => {
                tracing::warn!(%error, "invalid Radius gateway environment");
                None
            }
        };
    }
    if let Some(value) =
        crate::auth::read_platform_base_url(&xai_grok_config::grok_home(), "radius")
    {
        return match crate::auth::radius::normalize_gateway_root(&value) {
            Ok(gateway) => Some(gateway),
            Err(error) => {
                tracing::warn!(%error, "invalid stored Radius API-key gateway");
                None
            }
        };
    }
    Some(crate::auth::radius::DEFAULT_RADIUS_GATEWAY.to_string())
}

fn read_radius_config_response(
    response: reqwest::blocking::Response,
) -> anyhow::Result<RadiusConfigResponse> {
    use std::io::Read as _;

    if response
        .content_length()
        .is_some_and(|length| length > RADIUS_CONFIG_MAX_BYTES)
    {
        anyhow::bail!("Radius config response exceeds size limit");
    }
    let mut body = Vec::new();
    response
        .take(RADIUS_CONFIG_MAX_BYTES + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > RADIUS_CONFIG_MAX_BYTES {
        anyhow::bail!("Radius config response exceeds size limit");
    }
    Ok(serde_json::from_slice(&body)?)
}

fn oauth_gateway_for_auth(auth: &crate::auth::GrokAuth) -> Option<String> {
    match auth.platform_base_url.as_deref() {
        Some(value) => crate::auth::radius::normalize_gateway_root(value).ok(),
        None => crate::auth::radius::try_gateway_from_env_or_default().ok(),
    }
}

fn fetch_radius_models_if_enabled(
    platforms: &PlatformsConfig,
) -> Option<IndexMap<String, ModelEntry>> {
    let provider = xai_grok_models::provider_spec("radius")?;
    if provider.discovery.mode != ProviderDiscoveryMode::Adapter {
        return None;
    }

    // Static API keys are authoritative for this hybrid provider. Their
    // gateway comes from the API-key platform scope; OAuth uses the gateway
    // persisted inside oauth/radius so token and issuer cannot be mixed.
    let credential = if let Some(bearer) = resolve_provider_api_key(provider, platforms) {
        RadiusDiscoveryCredential::ApiKey {
            bearer,
            gateway: radius_api_key_gateway()?,
        }
    } else {
        let (marker, gateway) = crate::auth::radius::radius_catalog_oauth_cached()?;
        RadiusDiscoveryCredential::OAuth { marker, gateway }
    };
    let (gateway, initial_scope) = match &credential {
        RadiusDiscoveryCredential::ApiKey { bearer, gateway } => {
            (gateway.clone(), radius_credential_scope("api-key", bearer))
        }
        RadiusDiscoveryCredential::OAuth { marker, gateway } => {
            (gateway.clone(), radius_credential_scope("oauth", marker))
        }
    };

    if let Some(fresh) = load_radius_cache(&gateway, &initial_scope, true) {
        return Some(fresh);
    }
    let _process_guard = RADIUS_FETCH_LOCK.lock().ok()?;
    if let Some(fresh) = load_radius_cache(&gateway, &initial_scope, true) {
        return Some(fresh);
    }
    let Some(_file_guard) = lock_radius_cache() else {
        tracing::warn!("radius config cache lock unavailable; trying stale cache");
        return load_radius_cache(&gateway, &initial_scope, false);
    };
    if let Some(fresh) = load_radius_cache(&gateway, &initial_scope, true) {
        return Some(fresh);
    }

    let (bearer, active_scope) = match credential {
        RadiusDiscoveryCredential::ApiKey { bearer, .. } => (bearer, initial_scope.clone()),
        RadiusDiscoveryCredential::OAuth { gateway, .. } => {
            let Some(auth) = crate::auth::radius::ensure_radius_auth_blocking() else {
                tracing::warn!("radius OAuth refresh unavailable; trying stale cache");
                return load_radius_cache(&gateway, &initial_scope, false);
            };
            if oauth_gateway_for_auth(&auth).as_deref() != Some(gateway.as_str()) {
                tracing::warn!(
                    "radius OAuth gateway changed during discovery; refusing to mix token and gateway"
                );
                return load_radius_cache(&gateway, &initial_scope, false);
            }
            let active_scope = radius_credential_scope("oauth", &auth.key);
            if active_scope != initial_scope
                && let Some(fresh) = load_radius_cache(&gateway, &active_scope, true)
            {
                return Some(fresh);
            }
            (auth.key, active_scope)
        }
    };

    let url = match crate::auth::radius::config_url(&gateway) {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(%error, "radius config URL invalid; trying stale cache");
            return load_radius_cache(&gateway, &initial_scope, false);
        }
    };
    let client = crate::http::shared_startup_blocking_client();
    let response = match client
        .get(url)
        .timeout(RADIUS_CONFIG_REQUEST_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header("Authorization", format!("Bearer {bearer}"))
        .send()
    {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "radius config fetch failed; trying stale cache");
            return load_radius_cache(&gateway, &initial_scope, false);
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            status = %response.status(),
            "radius config fetch failed; trying stale cache"
        );
        return load_radius_cache(&gateway, &initial_scope, false);
    }
    let parsed = match read_radius_config_response(response) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(%error, "radius config parse failed; trying stale cache");
            return load_radius_cache(&gateway, &initial_scope, false);
        }
    };
    let base_url = match crate::auth::radius::normalize_gateway_root(&parsed.base_url) {
        Ok(base_url) => base_url,
        Err(error) => {
            tracing::warn!(%error, "radius config baseUrl invalid; trying stale cache");
            return load_radius_cache(&gateway, &initial_scope, false);
        }
    };
    if let Err(error) = validate_radius_config(&base_url, &parsed.models) {
        tracing::warn!(%error, "radius config validation failed; trying stale cache");
        return load_radius_cache(&gateway, &initial_scope, false);
    }

    let entries = radius_models_to_entries(&base_url, parsed.models.clone());
    store_radius_cache(&gateway, &active_scope, &base_url, parsed.models);
    Some(entries)
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
    #[serial_test::serial]
    fn kimi_static_live_discovery_uses_bearer_without_oauth_device_headers() {
        let _base = xai_grok_test_support::EnvGuard::set(
            xai_grok_models::KIMI_CODE_BASE_URL_ENV,
            "https://unit.kimi.invalid/coding/v1",
        );
        let client = reqwest::blocking::Client::new();
        let request =
            platform_models_request(&client, PlatformId::KimiCode, "static-kimi-test-key", false)
                .build()
                .expect("request should build");
        assert_eq!(
            request.url().as_str(),
            "https://unit.kimi.invalid/coding/v1/models"
        );
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer static-kimi-test-key")
        );
        assert!(request.headers().keys().all(|name| {
            !name.as_str().eq_ignore_ascii_case("x-msh-device-id")
                && !name.as_str().eq_ignore_ascii_case("x-msh-device-name")
                && !name.as_str().eq_ignore_ascii_case("x-msh-device-model")
        }));
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

    fn radius_wire(id: &str) -> RadiusWireModel {
        RadiusWireModel {
            id: id.to_string(),
            name: format!("Radius {id}"),
            reasoning: true,
            thinking_level_map: std::collections::BTreeMap::from([
                ("off".to_string(), Some("disabled".to_string())),
                ("medium".to_string(), Some("medium".to_string())),
            ]),
            input: vec!["text".to_string(), "image".to_string()],
            cost: RadiusWireCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 0.2,
                tiers: Vec::new(),
            },
            context_window: 128_000,
            max_tokens: 16_000,
        }
    }

    #[test]
    fn radius_config_is_validated_atomically_and_maps_pi_messages() {
        let models = vec![radius_wire("model-a")];
        validate_radius_config("https://inference.radius.test/v1", &models).unwrap();
        let entries = radius_models_to_entries("https://inference.radius.test/v1", models);
        let entry = entries.get("radius/model-a").unwrap();
        assert_eq!(entry.info.api_backend, ApiBackend::PiMessages);
        assert_eq!(entry.info.base_url, "https://inference.radius.test/v1");
        assert_eq!(entry.info.context_window.get(), 128_000);
        assert_eq!(entry.info.reasoning_effort, Some(ReasoningEffort::Medium));
        assert!(entry.info.supports_reasoning_effort);
        assert!(entry.api_key.is_none());
    }

    #[test]
    fn radius_config_rejects_duplicates_invalid_cost_and_unknown_inputs() {
        let duplicate = radius_wire("same");
        assert!(
            validate_radius_config(
                "https://inference.radius.test/v1",
                &[duplicate.clone(), duplicate]
            )
            .is_err()
        );

        let mut invalid_cost = radius_wire("bad-cost");
        invalid_cost.cost.output = -1.0;
        assert!(
            validate_radius_config("https://inference.radius.test/v1", &[invalid_cost]).is_err()
        );

        let mut tiered = radius_wire("tiered");
        tiered.cost.tiers = vec![RadiusWireCostTier {
            input: 2.0,
            output: 4.0,
            cache_read: 0.2,
            cache_write: 0.4,
            input_tokens_above: 200_000,
        }];
        validate_radius_config("https://inference.radius.test/v1", &[tiered.clone()]).unwrap();
        tiered.cost.tiers.push(RadiusWireCostTier {
            input_tokens_above: 100_000,
            ..tiered.cost.tiers[0].clone()
        });
        assert!(validate_radius_config("https://inference.radius.test/v1", &[tiered]).is_err());

        let mut invalid_input = radius_wire("bad-input");
        invalid_input.input.push("audio".into());
        assert!(
            validate_radius_config("https://inference.radius.test/v1", &[invalid_input]).is_err()
        );

        let invalid_json = serde_json::json!({
            "baseUrl": "https://inference.radius.test/v1",
            "models": [{
                "id": "bad-json",
                "name": "Bad JSON",
                "reasoning": false,
                "thinkingLevelMap": {},
                "input": ["text"],
                "cost": null,
                "contextWindow": 128000,
                "maxTokens": 16000
            }]
        });
        assert!(serde_json::from_value::<RadiusConfigResponse>(invalid_json).is_err());
    }

    #[test]
    #[serial_test::serial]
    fn radius_cache_is_gateway_scoped_atomic_and_stale_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("radius-cache.json");
        let _cache = xai_grok_test_support::EnvGuard::set(
            "GROK_RADIUS_MODELS_CACHE_PATH",
            cache_path.to_str().unwrap(),
        );
        let gateway = "https://gateway.radius.test";
        let base = "https://inference.radius.test/v1";
        let scope = radius_credential_scope("api-key", "test-key");
        store_radius_cache(gateway, &scope, base, vec![radius_wire("cached")]);

        assert!(load_radius_cache(gateway, &scope, true).is_some());
        assert!(
            load_radius_cache(
                gateway,
                &radius_credential_scope("api-key", "different-key"),
                false
            )
            .is_none()
        );
        assert!(load_radius_cache("https://other.radius.test", &scope, false).is_none());
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp-"))
        );

        let stale = RadiusCacheFile {
            version: 2,
            gateway: gateway.into(),
            credential_scope: scope.clone(),
            fetched_at: chrono::Utc::now() - chrono::Duration::seconds(RADIUS_CACHE_TTL_SECS + 1),
            base_url: base.into(),
            models: vec![radius_wire("stale")],
        };
        std::fs::write(&cache_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(load_radius_cache(gateway, &scope, true).is_none());
        assert!(load_radius_cache(gateway, &scope, false).is_some());

        let expired = RadiusCacheFile {
            fetched_at: chrono::Utc::now()
                - chrono::Duration::seconds(RADIUS_CACHE_MAX_STALE_SECS + 1),
            ..stale
        };
        std::fs::write(&cache_path, serde_json::to_vec(&expired).unwrap()).unwrap();
        assert!(load_radius_cache(gateway, &scope, false).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn radius_live_discovery_uses_root_config_endpoint_and_fresh_cache() {
        use std::io::{Read as _, Write as _};

        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        let cache_path = dir.path().join("radius-cache.json");
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let gateway = format!("http://{address}/ignored-prefix");
        let inference_base = format!("http://{address}/inference/v1");
        let response_body = serde_json::json!({
            "baseUrl": inference_base.clone(),
            "models": [{
                "id": "live-model",
                "name": "Live Radius Model",
                "reasoning": true,
                "thinkingLevelMap": {"medium": "medium"},
                "input": ["text", "image"],
                "cost": {
                    "input": 1.0,
                    "output": 2.0,
                    "cacheRead": 0.1,
                    "cacheWrite": 0.2
                },
                "contextWindow": 128000,
                "maxTokens": 16000
            }]
        })
        .to_string();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            loop {
                let read = socket.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            let lower = request.to_ascii_lowercase();
            assert!(request.starts_with("GET /v1/config HTTP/1.1\r\n"));
            assert!(lower.contains("authorization: bearer live-static-key\r\n"));
            assert!(lower.contains("accept: application/json\r\n"));
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();
            socket.flush().unwrap();
        });

        let _auth =
            xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
        let _cache = xai_grok_test_support::EnvGuard::set(
            "GROK_RADIUS_MODELS_CACHE_PATH",
            cache_path.to_str().unwrap(),
        );
        let _key = xai_grok_test_support::EnvGuard::set("GROK_RADIUS_API_KEY", "live-static-key");
        let _legacy_key = xai_grok_test_support::EnvGuard::unset("RADIUS_API_KEY");
        let _gateway = xai_grok_test_support::EnvGuard::set("GROK_RADIUS_BASE_URL", &gateway);
        let _legacy_gateway = xai_grok_test_support::EnvGuard::unset("RADIUS_GATEWAY_URL");

        let first = fetch_radius_models_if_enabled(&PlatformsConfig::default()).unwrap();
        server.join().unwrap();
        let entry = first.get("radius/live-model").unwrap();
        assert_eq!(entry.info.api_backend, ApiBackend::PiMessages);
        assert_eq!(entry.info.base_url, inference_base);

        // The listener has been dropped. A second successful result therefore
        // proves the fresh, credential-scoped cache avoided another request.
        let second = fetch_radius_models_if_enabled(&PlatformsConfig::default()).unwrap();
        assert!(second.contains_key("radius/live-model"));
    }

    #[test]
    #[serial_test::serial]
    fn radius_without_credentials_does_not_attempt_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        let cache_path = dir.path().join("radius-cache.json");
        let _auth =
            xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
        let _cache = xai_grok_test_support::EnvGuard::set(
            "GROK_RADIUS_MODELS_CACHE_PATH",
            cache_path.to_str().unwrap(),
        );
        let _api_key = xai_grok_test_support::EnvGuard::unset("GROK_RADIUS_API_KEY");
        let _legacy_key = xai_grok_test_support::EnvGuard::unset("RADIUS_API_KEY");
        let _gateway =
            xai_grok_test_support::EnvGuard::set("GROK_RADIUS_BASE_URL", "http://127.0.0.1:9");

        assert!(fetch_radius_models_if_enabled(&PlatformsConfig::default()).is_none());
        assert!(!cache_path.exists());
    }
}
