use super::*;
use crate::remote::fetch_models_blocking;
use crate::remote::{ModelSource, active_model_source};

// ── Fetch ───────────────────────────────────────────────────────────────────

pub(crate) fn build_prefetched_map(
    models: Vec<config::ModelEntryConfig>,
    api_base_url_override: Option<String>,
) -> IndexMap<String, ModelEntry> {
    let mut map: IndexMap<String, ModelEntry> = IndexMap::with_capacity(models.len());
    for m in models {
        let key = m.id.clone().unwrap_or_else(|| m.model.clone());
        let info = config::ModelInfo::from_config(&m);
        let entry = ModelEntry {
            info,
            api_key: None,
            env_key: None,
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: m.api_base_url.clone().or(api_base_url_override.clone()),
        };
        map.insert(key, entry);
    }
    map
}

/// Fetch remote models. Checks disk cache first; persists after fetch.
pub(crate) fn prefetch_models_blocking(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
) -> Option<IndexMap<String, ModelEntry>> {
    prefetch_models_blocking_gated(
        endpoints,
        auth,
        fetch_auth,
        crate::util::config::resolve_remote_fetch_enabled(),
    )
}

/// Fetch models + settings without touching disk; the cache write is returned
/// for the caller to commit.
fn prefetch_uncommitted(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
) -> (
    ModelsPrefetch,
    Option<crate::util::config::RemoteSettings>,
    Option<SettingsCacheWrite>,
) {
    // Resolved once so both fetches see the same policy.
    let remote_fetch_enabled = crate::util::config::resolve_remote_fetch_enabled();
    let models = {
        let _timer = crate::instrumentation_timer!("startup.early_models_fetch");
        fetch_models_uncommitted(endpoints, auth, fetch_auth, remote_fetch_enabled)
    };
    let (settings, settings_write) = match auth {
        Some(auth) if remote_fetch_enabled => {
            let origin = endpoints.proxy_url();
            let alpha_test_key = endpoints.alpha_test_key.as_deref();
            SettingsCacheManager::new().load_or_fetch(auth, &origin, alpha_test_key, || {
                let _timer = crate::instrumentation_timer!("startup.early_settings_fetch");
                crate::remote::fetch_settings_blocking(&origin, auth, alpha_test_key).into_option()
            })
        }
        _ => (None, None),
    };
    (models, settings, settings_write)
}

fn prefetch_models_blocking_gated(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
    remote_fetch_enabled: bool,
) -> Option<IndexMap<String, ModelEntry>> {
    let xai_map = fetch_models_uncommitted(endpoints, auth, fetch_auth, remote_fetch_enabled)
        .commit();
    let platforms = crate::agent::platform_models_fetch::load_platforms_config();
    crate::agent::platform_models_fetch::fetch_and_merge_platform_models(xai_map, &platforms)
}

/// A models fetch not yet written to the disk cache; the commit point decides
/// whether any state lands.
pub(in crate::agent::models) enum ModelsPrefetch {
    Cached(IndexMap<String, ModelEntry>),
    Fetched(ModelsCacheWrite),
    Unavailable,
}

impl ModelsPrefetch {
    fn commit(self) -> Option<IndexMap<String, ModelEntry>> {
        match self {
            Self::Cached(models) => Some(models),
            Self::Fetched(write) => Some(write.commit()),
            Self::Unavailable => None,
        }
    }

    pub(in crate::agent::models) fn into_deferred_write(self) -> Option<ModelsCacheWrite> {
        match self {
            Self::Fetched(write) => Some(write),
            Self::Cached(_) | Self::Unavailable => None,
        }
    }
}

pub(in crate::agent::models) struct ModelsCacheWrite {
    models: IndexMap<String, ModelEntry>,
    etag: Option<String>,
    auth_method: CacheAuthMethod,
    origin: String,
}

impl ModelsCacheWrite {
    pub(in crate::agent::models) fn commit(self) -> IndexMap<String, ModelEntry> {
        ModelsCacheManager::new().persist(
            &self.models,
            self.etag.as_deref(),
            self.auth_method,
            &self.origin,
        );
        self.models
    }
}

fn fetch_models_uncommitted(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
    remote_fetch_enabled: bool,
) -> ModelsPrefetch {
    let cache_auth = fetch_auth.cache_auth_method();
    let source = active_model_source(endpoints, fetch_auth);
    let cache_origin = source.cache_origin();

    let cache = ModelsCacheManager::new();
    if let Some(cached) = cache.load_fresh(&cache_auth, &cache_origin) {
        return ModelsPrefetch::Cached(cached.models);
    }

    if !remote_fetch_enabled {
        tracing::info!("models fetch skipped: remote_fetch disabled");
        return ModelsPrefetch::Unavailable;
    }

    let _timer = crate::instrumentation_timer!("startup.fetch_models_blocking");
    match source.fetch(auth) {
        Ok(FetchModelsResult { models, etag }) if !models.is_empty() => {
            let api_base_url_override = match fetch_auth {
                ModelFetchAuth::ApiKey => Some(endpoints.xai_api_base_url.clone()),
                _ => None,
            };
            let map = build_prefetched_map(models, api_base_url_override);

            tracing::info!(count = map.len(), etag = ?etag, "Prefetched models");
            ModelsPrefetch::Fetched(ModelsCacheWrite {
                models: map,
                etag,
                auth_method: cache_auth,
                origin: cache_origin,
            })
        }
        Ok(FetchModelsResult { .. }) => {
            tracing::warn!("Models endpoint returned empty list");
            ModelsPrefetch::Unavailable
        }
        Err(e) => {
            tracing::warn!("Failed to fetch models: {:?}", e);
            ModelsPrefetch::Unavailable
        }
    }
}

/// Prefetch with an explicit cache manager (tests inject `with_path(TempDir)`).
pub(crate) fn prefetch_models_blocking_with_cache(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
    remote_fetch_enabled: bool,
    cache: ModelsCacheManager,
) -> Option<IndexMap<String, ModelEntry>> {
    let cache_auth = fetch_auth.cache_auth_method();
    let cache_origin = crate::remote::models_list_url(endpoints, fetch_auth);
    let platforms = crate::agent::platform_models_fetch::load_platforms_config();

    // xAI disk cache hit: still merge live platform listings so Kimi/Moonshot
    // models stay current without waiting for the xAI etag to change.
    if let Some(cached) = cache.load_fresh(&cache_auth, &cache_origin) {
        if !remote_fetch_enabled {
            return Some(cached.models);
        }
        return crate::agent::platform_models_fetch::fetch_and_merge_platform_models(
            Some(cached.models),
            &platforms,
        );
    }

    if !remote_fetch_enabled {
        tracing::info!("models fetch skipped: remote_fetch disabled");
        return None;
    }

    let _timer = crate::instrumentation_timer!("startup.fetch_models_blocking");
    let xai_map = match fetch_models_blocking(endpoints, auth, fetch_auth) {
        Ok(FetchModelsResult { models, etag }) if !models.is_empty() => {
            let api_base_url_override = match fetch_auth {
                ModelFetchAuth::ApiKey => Some(endpoints.xai_api_base_url.clone()),
                _ => None,
            };
            let map = build_prefetched_map(models, api_base_url_override);

            tracing::info!(count = map.len(), etag = ?etag, "Prefetched models");
            // Persist the xAI-only map so the cache origin stays tied to the
            // xAI list URL; platform entries are re-merged on every load.
            cache.persist(&map, etag.as_deref(), cache_auth, &cache_origin);
            Some(map)
        }
        Ok(FetchModelsResult { .. }) => {
            tracing::warn!("Models endpoint returned empty list");
            None
        }
        Err(e) => {
            tracing::warn!("Failed to fetch models: {:?}", e);
            None
        }
    };

    // Live platform catalogs merge in whether or not the xAI fetch succeeded —
    // platform-only logins still get a real list.
    crate::agent::platform_models_fetch::fetch_and_merge_platform_models(xai_map, &platforms)
}

pub(crate) struct PrefetchEnv {
    pub(crate) auth: Option<GrokAuth>,
    pub(crate) endpoints: config::EndpointsConfig,
    pub(crate) model_fetch_auth: ModelFetchAuth,
}

/// Resolves startup endpoints from the effective config rather than env vars alone, so the prefetch cannot leak the bearer to api.x.ai.
pub(in crate::agent::models) fn resolve_startup_endpoints() -> config::EndpointsConfig {
    let mut endpoints = config::EndpointsConfig::from_effective_config();
    if endpoints.deployment_key.is_none() {
        endpoints.deployment_key = crate::managed_config::resolve_deployment_key();
    }
    endpoints
}

/// Decides whether the startup prefetch runs; takes auth and endpoints as parameters so tests skip the config loading.
pub(crate) fn resolve_prefetch_env_from_parts(
    auth: Option<GrokAuth>,
    endpoints: config::EndpointsConfig,
    remote_fetch_enabled: bool,
) -> Option<PrefetchEnv> {
    if !remote_fetch_enabled {
        tracing::info!("startup model/settings prefetch skipped: remote_fetch disabled");
        return None;
    }

    let model_fetch_auth = ModelFetchAuth::resolve(&endpoints, auth.is_some());

    // No xAI session / custom endpoint / API key: still prefetch when a
    // built-in platform has credentials so live `/models` can run.
    if auth.is_none()
        && !endpoints.has_custom_endpoint()
        && model_fetch_auth == ModelFetchAuth::Session
        && !has_any_platform_credentials()
    {
        return None;
    }

    Some(PrefetchEnv {
        auth,
        endpoints,
        model_fetch_auth,
    })
}

/// True when any platform credential is available for a live platform catalog
/// fetch. Startup gating must stay disk/env-only and avoid token refresh I/O.
fn has_any_platform_credentials() -> bool {
    if crate::auth::kimi::kimi_code_access_token_cached().is_some() {
        return true;
    }
    if crate::auth::openai_codex::openai_codex_catalog_access_token_cached().is_some() {
        return true;
    }
    if crate::auth::anthropic_claude::anthropic_claude_catalog_access_token_cached().is_some() {
        return true;
    }
    let platforms = crate::agent::platform_models_fetch::load_platforms_config();
    xai_grok_models::provider_registry()
        .providers()
        .iter()
        .any(|provider| {
            provider.accepts_api_key()
                && crate::agent::config::resolve_provider_api_key(provider, &platforms).is_some()
        })
}

/// Never touches managed config: the refresh supervisor owns syncing, so a
/// live server cannot heal a tampered policy ahead of the fail-closed gate.
pub(in crate::agent::models) fn prefetch_env(auth: Option<GrokAuth>) -> Option<PrefetchEnv> {
    let _timer = crate::instrumentation_timer!("startup.early_prefetch_launch");
    resolve_prefetch_env_from_parts(
        auth,
        resolve_startup_endpoints(),
        crate::util::config::resolve_remote_fetch_enabled(),
    )
}

pub(in crate::agent::models) fn resolve_disk_auth(
    grok_com_config: Option<GrokComConfig>,
) -> Option<GrokAuth> {
    let grok_home = crate::util::grok_home::grok_home();
    AuthManager::new(&grok_home, grok_com_config.unwrap_or_default()).current()
}

pub(in crate::agent::models) fn run_prefetch(
    env: PrefetchEnv,
) -> (
    ModelsPrefetch,
    Option<crate::util::config::RemoteSettings>,
    Option<SettingsCacheWrite>,
) {
    let mut timer = crate::instrumentation_timer!("startup.early_prefetch");
    let proxy_endpoint = env.endpoints.proxy_url();
    timer.with_field("endpoint", proxy_endpoint.as_str());
    prefetch_uncommitted(&env.endpoints, env.auth.as_ref(), env.model_fetch_auth)
}
