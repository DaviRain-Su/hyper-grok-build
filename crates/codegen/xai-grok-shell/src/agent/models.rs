//! Model fetching, resolution, and management.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;

use agent_client_protocol as acp;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use indexmap::IndexMap;

use crate::agent::config::{self, ModelEntry, resolve_credentials, sampling_config_for_model};
use crate::auth::{AuthManager, GrokAuth, GrokComConfig};
use crate::remote::{FetchModelsResult, fetch_models_blocking};
use crate::sampling::SamplerConfig as SamplingConfig;
use globset::{Glob, GlobSet, GlobSetBuilder};
use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

// ── Auth method for model fetching ──────────────────────────────────────────

/// Credential for `/v1/models` fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFetchAuth {
    Session,
    ApiKey,
    Deployment,
    CustomEndpoint,
}

impl ModelFetchAuth {
    /// custom_endpoint > session > deployment > API key.
    ///
    /// A `deployment_key` outranks an ambient `XAI_API_KEY` so a stray env key
    /// can't redirect model fetching from the deployment's entitlement-gated
    /// proxy to a raw `/v1/models` endpoint that lists the full model registry.
    pub(crate) fn resolve(endpoints: &config::EndpointsConfig, has_cached_session: bool) -> Self {
        if endpoints.has_custom_endpoint() {
            Self::CustomEndpoint
        } else if has_cached_session {
            Self::Session
        } else if endpoints.deployment_key.is_some() {
            Self::Deployment
        } else if crate::agent::auth_method::has_xai_api_key_env() {
            Self::ApiKey
        } else {
            Self::Session
        }
    }

    fn cache_auth_method(&self) -> CacheAuthMethod {
        match self {
            Self::CustomEndpoint | Self::ApiKey => CacheAuthMethod::ApiKey,
            Self::Session => CacheAuthMethod::Session,
            Self::Deployment => CacheAuthMethod::Deployment,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "snake_case")]
enum CacheAuthMethod {
    Session,
    ApiKey,
    Deployment,
}

pub(crate) fn task_model_error_for_catalog(
    requested: &str,
    available: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> Option<String> {
    let is_available =
        |entry: &ModelEntry| entry.info.user_selectable && entry.visible_for_auth(is_session_auth);
    if config::find_model_by_id(available, requested).is_some_and(&is_available) {
        return None;
    }

    let mut slugs = available
        .iter()
        .filter(|(_, entry)| is_available(entry))
        .map(|(slug, _)| slug.as_str())
        .collect::<Vec<_>>();
    slugs.sort_unstable();
    let guidance = if slugs.is_empty() {
        "No valid model slugs are currently available. Omit `model` to inherit the parent model."
            .to_string()
    } else {
        format!(
            "Valid model slugs: {}. Omit `model` to inherit the parent model.",
            slugs.join(", ")
        )
    };
    Some(format!("Unknown Task.model slug '{requested}'. {guidance}"))
}

/// Thread-safe model manager.
///
/// Owns the auth manager, config, and gateway needed to refresh models.
/// Uses `parking_lot::RwLock` for short clone-and-release access.
#[derive(Clone)]
pub struct ModelsManager {
    inner: Arc<Inner>,
}

struct Inner {
    prefetched: RwLock<Option<IndexMap<String, ModelEntry>>>,
    models: RwLock<IndexMap<String, ModelEntry>>,
    current_model_id: RwLock<acp::ModelId>,
    current_reasoning_effort: RwLock<Option<ReasoningEffort>>,
    etag: RwLock<Option<String>>,
    /// Set once a real catalog has been fetched; gates whether
    /// `apply_refresh_result` calls `reselect_default_model` (first
    /// time) or `reselect_current_model_if_missing` (subsequent).
    /// Reset in `clear()` for identity changes.
    has_fetched_real_catalog: RwLock<bool>,
    // ── Owned context for self-contained refresh ────────────────
    auth_manager: Arc<AuthManager>,
    cfg: RwLock<config::Config>,
    fetch_auth: RwLock<ModelFetchAuth>,
    gateway: RwLock<Option<xai_acp_lib::AcpAgentGatewaySender>>,
    cache: ModelsCacheManager,
    /// Guard to prevent overlapping retry loops.
    retry_in_flight: AtomicBool,
    /// `allowed_models` matched nothing in the fetched catalog; the prompt path
    /// blocks rather than run on the bundled default. Set in `apply_refresh_result`.
    allowlist_excludes_all: AtomicBool,
    /// Layer-3 LazinessDetector model-switch signal. Carries a
    /// monotonically-increasing generation counter (`u64`) that is
    /// bumped whenever the current model id actually changes via
    /// [`Self::set_current_model_id`].
    ///
    /// Two consumer patterns:
    /// 1. `subscribe_model_switch().changed().await` — used by the
    ///    `SessionActor` main loop to react to a switch (e.g. zero
    ///    the per-session nudge counter). Critically, `watch::Receiver`
    ///    only resolves `.changed()` on changes that happen **after**
    ///    subscription — there is no stored-permit hazard akin to
    ///    `tokio::sync::Notify::notify_one()`.
    /// 2. `model_switch_generation()` — cheap snapshot read used by
    ///    `maybe_fire_laziness_check`'s polling loop to detect a
    ///    switch that occurred during the idle wait or sampler call.
    ///
    /// `watch::Sender` natively fans out to every subscriber, so this
    /// replaces the previous `RwLock<Vec<Arc<Notify>>>` listener
    /// registry — no manual fan-out, no listener-leak risk, no
    /// `unregister` API to maintain.
    model_switch_watch: tokio::sync::watch::Sender<u64>,
}

impl Default for ModelsManager {
    fn default() -> Self {
        let grok_home = crate::util::grok_home::grok_home();
        let auth_manager = Arc::new(AuthManager::new(&grok_home, GrokComConfig::default()));
        Self::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("default"),
            auth_manager,
            config::Config::default(),
        )
    }
}

impl ModelsManager {
    pub(crate) fn new(
        prefetched: Option<IndexMap<String, ModelEntry>>,
        models: IndexMap<String, ModelEntry>,
        current_model_id: acp::ModelId,
        auth_manager: Arc<AuthManager>,
        cfg: config::Config,
    ) -> Self {
        let has_session = auth_manager.current_or_expired().is_some();
        let fetch_auth = ModelFetchAuth::resolve(&cfg.endpoints, has_session);
        let current_reasoning_effort = cfg.models.default_reasoning_effort;
        Self {
            inner: Arc::new(Inner {
                prefetched: RwLock::new(prefetched),
                models: RwLock::new(models),
                current_model_id: RwLock::new(current_model_id),
                current_reasoning_effort: RwLock::new(current_reasoning_effort),
                etag: RwLock::new(None),
                has_fetched_real_catalog: RwLock::new(false),
                auth_manager,
                cfg: RwLock::new(cfg),
                fetch_auth: RwLock::new(fetch_auth),
                gateway: RwLock::new(None),
                cache: ModelsCacheManager::new(),
                retry_in_flight: AtomicBool::new(false),
                allowlist_excludes_all: AtomicBool::new(false),
                model_switch_watch: tokio::sync::watch::channel(0u64).0,
            }),
        }
    }

    /// Subscribe to model-switch events. Returns a `watch::Receiver`
    /// carrying the monotonic generation counter. `.changed()` only
    /// resolves on switches that occur **after** subscription, so
    /// there is no stored-permit hazard (the bug that motivated
    /// replacing the previous `Arc<Notify>` design).
    pub fn subscribe_model_switch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.model_switch_watch.subscribe()
    }

    /// Cheap snapshot of the current model-switch generation. Used by
    /// `maybe_fire_laziness_check`'s polling loop to detect a switch
    /// that occurred during the idle wait or sampler call without
    /// having to allocate a fresh `Receiver` per fire.
    pub fn model_switch_generation(&self) -> u64 {
        *self.inner.model_switch_watch.borrow()
    }

    /// Build from a resolved config. Falls back to bundled default if no models available.
    ///
    /// When `prefetched_models` is `None`, the disk cache is consulted so that
    /// server-side models are available for default-model resolution even when
    /// the caller didn't do an explicit prefetch.
    pub fn from_config(
        cfg: &config::Config,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
        auth_manager: Arc<AuthManager>,
    ) -> Result<Self, String> {
        let has_session = auth_manager.current_or_expired().is_some();
        let is_session_auth = auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_session_auth());
        let fetch_auth = ModelFetchAuth::resolve(&cfg.endpoints, has_session);
        let prefetched_models = prefetched_models.or_else(|| {
            let cache = ModelsCacheManager::new();
            cache
                .load_fresh(
                    &fetch_auth.cache_auth_method(),
                    &crate::remote::models_list_url(&cfg.endpoints, fetch_auth),
                )
                .map(|c| c.models)
        });
        let has_prefetched = prefetched_models.is_some();
        let catalog = resolve_model_catalog(cfg, prefetched_models.clone());

        // Validate only against a real catalog; a bundled-only first run defers
        // to the async fetch (`apply_refresh_result`).
        if has_prefetched {
            validate_selectable(cfg, &catalog)?;
        }

        let (current_model_key, current_model, model_source) =
            resolve_default_model(cfg, &catalog, is_session_auth);

        tracing::info!(
            model_id = %current_model.model,
            source = %model_source,
            "default model resolved"
        );

        let current_model_id = acp::ModelId::new(Arc::from(current_model_key));

        let mgr = Self::new(
            prefetched_models,
            catalog,
            current_model_id,
            auth_manager,
            cfg.clone(),
        );
        if has_prefetched {
            *mgr.inner.has_fetched_real_catalog.write() = true;
        }
        Ok(mgr)
    }

    pub(crate) fn set_gateway(&self, gateway: xai_acp_lib::AcpAgentGatewaySender) {
        *self.inner.gateway.write() = Some(gateway);
    }

    /// Swap config, rebuild catalog, and reselect the model.
    ///
    /// Calls `reselect_default_model` when the preferred model changed
    /// (and is `Some`); otherwise `reselect_current_model_if_missing`.
    pub fn apply_config(&self, new_config: config::Config) {
        // Reject an invalid reload instead of mutating live state: bad globs or
        // (once a real catalog exists) an allowlist that excludes everything.
        if let Err(e) = new_config.validate_model_filters() {
            tracing::error!(error = %e, "ignoring config reload: invalid model filters");
            return;
        }
        let prefetched = self.inner.prefetched.read().clone();
        let new_catalog = resolve_model_catalog(&new_config, prefetched);
        let has_real_catalog = *self.inner.has_fetched_real_catalog.read();
        if has_real_catalog && let Err(e) = validate_selectable(&new_config, &new_catalog) {
            tracing::error!(error = %e, "ignoring config reload: allowed_models excludes all models");
            return;
        }

        let (old_preferred, old_default_is_campaign) = {
            let cfg = self.inner.cfg.read();
            (
                cfg.models.default.clone(),
                cfg.models.default_is_campaign_driven,
            )
        };
        let new_preferred = new_config.models.default.clone();
        let has_session = self.inner.auth_manager.current_or_expired().is_some();
        *self.inner.fetch_auth.write() =
            ModelFetchAuth::resolve(&new_config.endpoints, has_session);
        *self.inner.cfg.write() = new_config.clone();
        // Recompute the prompt-block flag so a corrective reload unblocks.
        if has_real_catalog {
            let excludes_all = allowlist_matches_nothing(&new_config, &new_catalog);
            self.inner
                .allowlist_excludes_all
                .store(excludes_all, Ordering::Relaxed);
        }
        *self.inner.models.write() = new_catalog;

        // A preferred-model flip caused only by a campaign overlay appearing or
        // disappearing must not yank an in-flight session whose current model is
        // still usable — the campaign applies to /new sessions only.
        let preferred_changed = new_preferred != old_preferred && new_preferred.is_some();
        // Recognize an appearing OR withdrawing campaign from the
        // `default_is_campaign_driven` flag on each config (no disk I/O); correct
        // even when the user has no base default (where a value compare would miss).
        let mut campaign_defaults = std::collections::HashSet::new();
        if new_config.models.default_is_campaign_driven
            && let Some(d) = &new_preferred
        {
            campaign_defaults.insert(d.clone());
        }
        if old_default_is_campaign && let Some(d) = &old_preferred {
            campaign_defaults.insert(d.clone());
        }
        let campaign_only_flip =
            is_campaign_only_flip(&old_preferred, &new_preferred, &campaign_defaults);
        let current_still_ok = {
            let models = self.inner.models.read();
            let cur = self.inner.current_model_id.read();
            models
                .get(cur.0.as_ref())
                .is_some_and(|e| e.info.user_selectable)
        };
        if preferred_changed && !(campaign_only_flip && current_still_ok) {
            self.reselect_default_model(&new_config);
        } else {
            self.reselect_current_model_if_missing(&new_config);
        }

        // Push the new catalog to connected clients (`x.ai/models/update`).
        // Without this, a long-running agent (leader mode) correctly swaps
        // its in-memory catalog on a config.toml `[model.*]`/`[models]` edit,
        // but already-connected clients keep rendering the stale model list
        // until they reconnect. No-op when no gateway is attached (tests,
        // pre-init).
        self.notify_models_updated();
    }

    // ── Accessors ───────────────────────────────────────────────────

    pub fn models(&self) -> IndexMap<String, ModelEntry> {
        self.inner.models.read().clone()
    }

    pub fn endpoints(&self) -> config::EndpointsConfig {
        self.inner.cfg.read().endpoints.clone()
    }

    /// Does the current credential grant access to OAuth-only models?
    fn is_session_auth(&self) -> bool {
        self.inner
            .auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_session_auth())
    }

    /// ACP-visible (non-hidden) projection of the catalog.
    /// The catalog coming from `resolve_model_catalog` already has
    /// allowed_models + disabled_models + hidden_models applied.
    ///
    /// Includes locked platform models (credential-less BYOK entries) as a
    /// trailing tier with `requiresApiKey`/`requiresOAuth` meta — clients
    /// render them dimmed for discovery; selection is rejected at
    /// `set_session_model` and they never resolve credentials.
    pub fn available(&self) -> IndexMap<acp::ModelId, acp::ModelInfo> {
        let snapshot = {
            let models = self.inner.models.read();
            models.clone()
        };

        let selectable: IndexMap<_, _> = snapshot
            .into_iter()
            .filter(|(_, e)| e.info.user_selectable)
            .collect();

        available_models(&selectable, self.is_session_auth())
    }

    pub(crate) fn task_model_error(&self, requested: &str) -> Option<String> {
        let is_session_auth = self.is_session_auth();
        let models = self.inner.models.read();
        task_model_error_for_catalog(requested, &models, is_session_auth)
    }

    pub fn current_model_id(&self) -> acp::ModelId {
        self.inner.current_model_id.read().clone()
    }

    pub fn set_current_model_id(&self, id: acp::ModelId) {
        // Only bump the model-switch generation on a real change.
        // The pager's `/model` handler can call this with the
        // already-active id during re-resolution; bumping the counter
        // in that case would needlessly cancel a healthy in-flight
        // classifier call and zero the per-session nudge counter.
        let changed = {
            let mut cur = self.inner.current_model_id.write();
            let changed = *cur != id;
            *cur = id;
            changed
        };
        if changed {
            self.inner
                .model_switch_watch
                .send_modify(|generation| *generation += 1);
        }
    }

    /// Look up the per-model Layer-3 LazinessDetector config for the
    /// model identified by `model_id`. Returns the default (disabled)
    /// config when the id isn't in the catalog — same fallback
    /// semantics as the `auto_compact_threshold_percent` lookup.
    pub fn laziness_detector_for(&self, model_id: &str) -> config::LazinessDetectorPerModelConfig {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().laziness_detector.clone())
            .unwrap_or_default()
    }

    /// Test-only catalog poke: inserts a `ModelEntry` keyed by `id`,
    /// allowing integration tests to enable Layer-3 features per
    /// model without spinning up the full config-merge pipeline.
    #[cfg(test)]
    pub(crate) fn insert_test_entry(&self, id: impl Into<String>, entry: ModelEntry) {
        self.inner.models.write().insert(id.into(), entry);
    }

    pub fn current_reasoning_effort(&self) -> Option<ReasoningEffort> {
        *self.inner.current_reasoning_effort.read()
    }

    pub fn set_current_reasoning_effort(&self, effort: Option<ReasoningEffort>) {
        *self.inner.current_reasoning_effort.write() = effort;
    }

    /// Whether the given model supports reasoning effort according to the catalog.
    pub fn model_supports_reasoning_effort(&self, model_id: &str) -> bool {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().supports_reasoning_effort)
            .unwrap_or(false)
    }

    /// The catalog default reasoning effort for `model_id`, if the catalog
    /// pins one. Used as the final fallback when neither the session handle
    /// nor the global config sets an explicit effort, so surfaced config stays
    /// consistent with the effort sampling actually uses.
    pub fn model_default_reasoning_effort(&self, model_id: &str) -> Option<ReasoningEffort> {
        self.inner
            .models
            .read()
            .get(model_id)
            .and_then(|e| e.info().reasoning_effort)
    }

    /// The raw catalog `reasoning_efforts` list for `model_id` with no fallback,
    /// empty when the catalog pins none (caller falls back to the built-in
    /// session modes). Distinct from the pager's gate-first, fallback-applied
    /// `ModelState::reasoning_effort_options`.
    pub fn model_reasoning_efforts(&self, model_id: &str) -> Vec<ReasoningEffortOption> {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().reasoning_efforts.clone())
            .unwrap_or_default()
    }

    pub fn model_supports_backend_search(&self, model_id: &str) -> bool {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().supports_backend_search)
            .unwrap_or(false)
    }

    pub fn model_compactions_remaining(
        &self,
        model_id: &str,
    ) -> Option<xai_grok_sampling_types::CompactionsRemaining> {
        self.inner
            .models
            .read()
            .get(model_id)
            .and_then(|e| e.info().compactions_remaining)
    }

    pub fn model_compaction_at_tokens(
        &self,
        model_id: &str,
    ) -> Option<xai_grok_sampling_types::CompactionAtTokens> {
        self.inner
            .models
            .read()
            .get(model_id)
            .and_then(|e| e.info().compaction_at_tokens)
    }

    /// Catalog opt-in to display the served-checkpoint fingerprint for this model.
    ///
    /// `model_id` may be a routing slug (`config.model`, e.g. `grok-4.5`)
    /// OR a catalog key; the catalog map is keyed by the config key, which can
    /// differ from the slug for custom/enterprise ids (e.g. key `enterprise-grok-build`
    /// → slug `grok-4.5`). Resolve to the catalog key first so a slug
    /// caller still finds the opted-in entry.
    pub fn model_show_model_fingerprint(&self, model_id: &str) -> bool {
        let models = self.inner.models.read();
        resolve_catalog_key(&models, &acp::ModelId::new(model_id))
            .and_then(|key| models.get(key.0.as_ref()))
            .map(|e| e.info().show_model_fingerprint)
            .unwrap_or(false)
    }

    /// Resolved next-prompt-suggestion model pin from the live config
    /// (`env > [models] prompt_suggestion > remote settings`); tracks config
    /// hot-reloads via [`Self::apply_config`]. Consumed catalog-guarded by
    /// `handle_suggest_prompt`.
    pub fn prompt_suggest_model_pin(&self) -> crate::config::PromptSuggestModelPin {
        self.inner.cfg.read().prompt_suggest_model_pin.clone()
    }

    /// Whether `model_id` resolves in the current catalog — as a config key
    /// or a routing slug (see [`resolve_catalog_key`]). Deliberately checks
    /// the full catalog rather than the user-selectable projection: auxiliary
    /// background calls need a *sampleable* model, and hidden or
    /// non-selectable entries are still sampleable.
    pub fn model_in_catalog(&self, model_id: &str) -> bool {
        let models = self.inner.models.read();
        resolve_catalog_key(&models, &acp::ModelId::new(model_id)).is_some()
    }

    #[cfg(test)]
    fn prefetched(&self) -> Option<IndexMap<String, ModelEntry>> {
        self.inner.prefetched.read().clone()
    }

    #[cfg(test)]
    fn has_fetched_real_catalog(&self) -> bool {
        *self.inner.has_fetched_real_catalog.read()
    }

    // ── Mutations ───────────────────────────────────────────────────

    fn rebuild(&self, cfg: &config::Config, prefetched: Option<IndexMap<String, ModelEntry>>) {
        *self.inner.models.write() = resolve_model_catalog(cfg, prefetched);
    }

    /// Re-run catalog resolution so platform credentials (e.g. a freshly
    /// minted Kimi Code OAuth bearer) are stamped onto `kimi-code/*` entries.
    ///
    /// Also attempts a live `GET {base}/models` per platform with credentials
    /// so K3 and the rest of the subscription listing replace offline
    /// fallbacks in the catalog.
    ///
    /// Safe from the ACP agent async task: the live fetch uses
    /// [`crate::agent::platform_models_fetch::run_blocking_io`] so
    /// `reqwest::blocking` never runs on the current-thread runtime.
    pub fn restamp_platform_credentials(&self) {
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        // First stamp credentials from disk (no network) so the picker
        // unlocks immediately even if the live listing fails.
        self.rebuild(&cfg, prefetched.clone());
        // Live listing (blocking HTTP) — must leave the async context.
        let platforms = cfg.platforms.clone();
        let merged = crate::agent::platform_models_fetch::fetch_and_merge_platform_models(
            prefetched, &platforms,
        );
        *self.inner.prefetched.write() = merged.clone();
        self.rebuild(&cfg, merged);
        self.notify_models_updated();
    }

    /// Refresh models when the etag changes.
    ///
    /// Writes etag optimistically before spawning the fetch to coalesce
    /// concurrent callers seeing the same new etag.
    pub async fn refresh_if_new_etag(&self, etag: String) {
        let same_etag = {
            let current = self.inner.etag.read();
            current.as_deref() == Some(etag.as_str())
        };
        if same_etag {
            let fetch_auth = *self.inner.fetch_auth.read();
            self.inner
                .cache
                .renew_ttl(&fetch_auth.cache_auth_method(), &self.cache_origin())
                .await;
            return;
        }
        *self.inner.etag.write() = Some(etag.clone());
        tracing::info!(etag = %etag, "models etag changed, refreshing");
        self.do_refresh(Some(etag), RefreshStrategy::Online);
    }

    /// Auth identity changed: invalidate disk cache and refresh the catalog.
    ///
    /// Safe on OIDC token recovery after idle: we never drop a successfully-fetched
    /// catalog on transient failure. Only fall back to the bundled default when
    /// we have never had a real catalog (`!has_fetched_real_catalog`), or via
    /// the genuine no-auth path (`clear()`).
    ///
    /// Respects the auth snapshot / hot-swap discipline.
    pub async fn on_auth_changed(&self) {
        let config = self.inner.cfg.read().clone();
        crate::agent::init::update_telemetry_config(&config, &self.inner.auth_manager);
        self.inner.cache.invalidate();
        let has_session = self.inner.auth_manager.current_or_expired().is_some();
        let fetch_auth = ModelFetchAuth::resolve(&config.endpoints, has_session);
        *self.inner.fetch_auth.write() = fetch_auth;
        if self.inner.auth_manager.current_or_expired().is_none()
            && fetch_auth == ModelFetchAuth::Session
        {
            self.clear();
            return;
        }

        // Never eagerly drop prefetched on auth recovery. Only fall back to
        // bundled defaults when we have never had a real catalog. Resolved once
        // so the fetch and the failure-vs-disabled classification below agree.
        let remote_fetch_enabled = crate::util::config::resolve_remote_fetch_enabled();
        self.fetch_and_apply_inner(remote_fetch_enabled).await;

        if !*self.inner.has_fetched_real_catalog.read() && self.inner.prefetched.read().is_none() {
            if remote_fetch_enabled {
                xai_grok_telemetry::unified_log::warn(
                    "model catalog: falling back to bundled defaults only",
                    None,
                    Some(serde_json::json!({
                        "trigger": "on_auth_changed",
                        "had_real_catalog": false,
                    })),
                );
            } else {
                // Deliberate no-fetch state, not a failure: no warn-class log.
                tracing::debug!("model catalog: bundled defaults in use (remote_fetch disabled)");
            }
            self.rebuild(&config, None); // first-time only: no fetched catalog, use bundled defaults
            self.reselect_current_model_if_missing(&config);

            // Schedule background retries so we recover once the network is
            // back (e.g. after sleep/resume when the first fetch races DNS).
            // With remote_fetch disabled a retry can never succeed, so none is
            // scheduled.
            if remote_fetch_enabled {
                self.spawn_catalog_retry();
            }
        }

        self.notify_models_updated();
    }

    /// Notify clients about the current model catalog.
    fn notify_models_updated(&self) {
        let available = self.available();
        let current = self.current_model_id();
        let count = available.len();
        xai_grok_telemetry::unified_log::info(
            "model catalog: notifying clients",
            None,
            Some(serde_json::json!({
                "model_count": count,
                "current_model_id": current.0.as_ref(),
            })),
        );
        if let Some(ref gw) = *self.inner.gateway.read() {
            let model_state =
                acp::SessionModelState::new(current, available.values().cloned().collect());
            if let Ok(params) = serde_json::value::to_raw_value(&model_state) {
                gw.forward_fire_and_forget(acp::ExtNotification::new(
                    "x.ai/models/update",
                    params.into(),
                ));
            }
        }
    }

    /// Hot-reload the catalog from `~/.grok/models_cache.json` after an
    /// external write (detected by the config file watcher).
    ///
    /// A long-running leader otherwise only refreshes its catalog from its
    /// *own* fetch paths (startup prefetch, auth change, response-header etag).
    /// When another grok process sharing `~/.grok` (a `--no-leader` run, a
    /// newer client, grok-desktop) fetches a fresher `/v1/models` catalog and
    /// persists it, this picks it up without a network round-trip.
    ///
    /// Guards, in order:
    /// 1. `load_fresh` — rejects stale (TTL), version-mismatched,
    ///    auth-method-mismatched, or origin-mismatched cache files (another
    ///    process running with different credentials or pointed at a
    ///    different backend must not poison this catalog).
    /// 2. Content dedup — the leader itself rewrites the cache file
    ///    (`persist` after fetch, `renew_ttl` on same-etag responses), and the
    ///    watcher has no self-write suppression. If the cached models match
    ///    the in-memory prefetched catalog this is a no-op (the etag is still
    ///    adopted so `refresh_if_new_etag` doesn't refetch needlessly).
    ///
    /// On a real change: swaps the prefetched catalog, rebuilds, re-resolves
    /// the configured default when this is the first real catalog (otherwise
    /// reselects the current model if it disappeared), and notifies clients.
    pub fn reload_from_disk_cache(&self) {
        self.reload_from_cache_manager(&self.inner.cache);
    }

    /// Core of [`Self::reload_from_disk_cache`], parameterized over the cache
    /// manager so tests can point it at a temp file (the production
    /// `ModelsCacheManager` path is fixed to `grok_home()`, a process-wide
    /// `OnceLock`).
    fn reload_from_cache_manager(&self, cache: &ModelsCacheManager) {
        let fetch_auth = *self.inner.fetch_auth.read();
        let Some(cached) = cache.load_fresh(&fetch_auth.cache_auth_method(), &self.cache_origin())
        else {
            tracing::debug!("models cache changed on disk but is not loadable; ignoring");
            return;
        };

        // Self-write / no-change dedup by content. `ModelEntry` doesn't impl
        // `PartialEq` (nested config types), so compare the serialized form —
        // catalogs are small (tens of entries) and writes are debounced.
        let same_content = {
            let prefetched = self.inner.prefetched.read();
            prefetched.as_ref().is_some_and(|current| {
                serde_json::to_string(current).ok() == serde_json::to_string(&cached.models).ok()
            })
        };
        if same_content {
            // Adopt the (possibly newer) etag without a rebuild so the next
            // response-header comparison in `refresh_if_new_etag` is accurate.
            if cached.etag.is_some() {
                *self.inner.etag.write() = cached.etag;
            }
            tracing::debug!("models cache changed on disk but catalog is identical; skipping");
            return;
        }

        let cfg = self.inner.cfg.read().clone();
        let count = cached.models.len();
        // Capture whether this is the first real catalog (mirrors
        // `apply_refresh_result`): if the leader bootstrapped on bundled
        // defaults, the configured default must be re-resolved against the
        // real catalog rather than left on a placeholder.
        let first_real_catalog = {
            let mut flag = self.inner.has_fetched_real_catalog.write();
            let was_first = !*flag;
            *flag = true;
            was_first
        };
        *self.inner.prefetched.write() = Some(cached.models.clone());
        self.rebuild(&cfg, Some(cached.models));
        *self.inner.etag.write() = cached.etag;
        if first_real_catalog {
            self.reselect_default_model(&cfg);
        } else {
            self.reselect_current_model_if_missing(&cfg);
        }

        // Recompute the prompt-block flag (mirrors `apply_refresh_result`) so
        // a corrective external cache write unlatches a previously latched
        // "allowlist excludes everything" state instead of keeping prompts
        // blocked against a stale catalog.
        let excludes_all = allowlist_matches_nothing(&cfg, &self.inner.models.read());
        self.inner
            .allowlist_excludes_all
            .store(excludes_all, Ordering::Relaxed);
        if excludes_all {
            tracing::error!("allowed_models excludes all fetched models; prompts will be blocked");
        }

        tracing::info!(count, "model catalog hot-reloaded from disk cache");
        xai_grok_telemetry::unified_log::info(
            "model catalog: reloaded from external disk-cache write",
            None,
            Some(serde_json::json!({ "model_count": count })),
        );
        self.notify_models_updated();
    }

    /// Retry model catalog fetch in the background with exponential backoff.
    ///
    /// Spawned when `on_auth_changed` falls back to bundled defaults. Uses the
    /// crate-standard `execute_with_backoff` (5 attempts, 5s base, 60s cap) and
    /// notifies clients on success so the UI recovers after sleep/resume without
    /// requiring a manual restart.
    fn spawn_catalog_retry(&self) {
        // Deliberate no-fetch state: a retry loop can never succeed, so don't
        // start one (defensive re-check; the spawn site already gates).
        if !crate::util::config::resolve_remote_fetch_enabled() {
            return;
        }
        // Prevent overlapping retry loops.
        if self
            .inner
            .retry_in_flight
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tracing::debug!("model catalog retry already in flight, skipping");
            return;
        }

        let mgr = self.clone();
        tokio::task::spawn(async move {
            let backoff = crate::tools::retry::BackoffConfig::new(5, 5_000, 60_000);

            let result = crate::tools::retry::execute_with_backoff(
                &backoff,
                || {
                    let mgr = mgr.clone();
                    async move {
                        // Bail out early if another code path already loaded a real catalog.
                        if *mgr.inner.has_fetched_real_catalog.read() {
                            return Ok(());
                        }

                        mgr.fetch_and_apply().await;

                        if *mgr.inner.has_fetched_real_catalog.read() {
                            Ok(())
                        } else {
                            Err("model catalog fetch returned no models")
                        }
                    }
                },
                |attempt, max_retries, delay| async move {
                    xai_grok_telemetry::unified_log::warn(
                        "model catalog: retry scheduled",
                        None,
                        Some(serde_json::json!({
                            "attempt": attempt,
                            "max_retries": max_retries,
                            "delay_ms": delay.as_millis() as u64,
                        })),
                    );
                },
            )
            .await;

            match result {
                Ok(()) => {
                    let count = mgr.available().len();
                    xai_grok_telemetry::unified_log::info(
                        "model catalog: retry succeeded",
                        None,
                        Some(serde_json::json!({ "model_count": count })),
                    );
                    mgr.notify_models_updated();
                }
                Err(e) => {
                    xai_grok_telemetry::unified_log::warn(
                        "model catalog: all retries exhausted",
                        None,
                        Some(serde_json::json!({ "error": e })),
                    );
                }
            }

            mgr.inner.retry_in_flight.store(false, Ordering::Release);
        });
    }

    /// Refresh the model catalog on every auth token refresh.
    ///
    /// Listens for [`AuthManager::refresh_notifier`] signals directly,
    /// bypassing the FSEvents file watcher which can silently stop
    /// delivering events on macOS after resume from sleep. On each
    /// notification the catalog is re-fetched from the server; if the
    /// fetch succeeds and the catalog changed, clients are notified
    /// via `x.ai/models/update`.
    pub fn start_auth_refresh_watcher(&self, notify: Arc<tokio::sync::Notify>) {
        let mgr = self.clone();
        let had_catalog_at_start = *self.inner.has_fetched_real_catalog.read();
        xai_grok_telemetry::unified_log::info(
            "model catalog: auth refresh watcher started",
            None,
            Some(serde_json::json!({
                "had_real_catalog": had_catalog_at_start,
                "model_count": self.available().len(),
            })),
        );
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                // Deliberate no-fetch state: skip the refresh entirely so the
                // failure-classifying logs below keep meaning "actually failed".
                if !crate::util::config::resolve_remote_fetch_enabled() {
                    tracing::debug!(
                        "model catalog: auth refresh watcher skipped (remote_fetch disabled)"
                    );
                    continue;
                }
                let had_catalog = *mgr.inner.has_fetched_real_catalog.read();
                let old_count = mgr.available().len();
                xai_grok_telemetry::unified_log::info(
                    "model catalog: auth refresh watcher triggered",
                    None,
                    Some(serde_json::json!({
                        "had_real_catalog": had_catalog,
                        "model_count_before": old_count,
                    })),
                );
                mgr.fetch_and_apply().await;
                let has_catalog = *mgr.inner.has_fetched_real_catalog.read();
                let new_count = mgr.available().len();
                if has_catalog {
                    if !had_catalog || new_count != old_count {
                        xai_grok_telemetry::unified_log::info(
                            "model catalog: auth refresh watcher updated catalog",
                            None,
                            Some(serde_json::json!({
                                "model_count_before": old_count,
                                "model_count_after": new_count,
                                "was_recovery": !had_catalog,
                            })),
                        );
                    }
                    mgr.notify_models_updated();
                } else {
                    xai_grok_telemetry::unified_log::warn(
                        "model catalog: auth refresh watcher fetch failed",
                        None,
                        Some(serde_json::json!({
                            "model_count": old_count,
                        })),
                    );
                }
            }
        });
    }

    /// Wipe in-memory state so a previous identity's catalog doesn't leak.
    fn clear(&self) {
        *self.inner.prefetched.write() = None;
        *self.inner.models.write() = IndexMap::new();
        *self.inner.etag.write() = None;
        *self.inner.has_fetched_real_catalog.write() = false;
        self.inner
            .allowlist_excludes_all
            .store(false, Ordering::Relaxed);
    }

    /// Build a `SamplingConfig` from the current model + auth state.
    pub fn sampling_config(&self) -> SamplingConfig {
        let config = self.inner.cfg.read().clone();
        let auth_manager = self.inner.auth_manager.as_ref();
        let current_model_id = self.current_model_id();
        let all_models = self.models();
        let fallback;
        let current_model = match all_models
            .get(current_model_id.0.as_ref())
            .or_else(|| all_models.values().next())
        {
            Some(m) => m,
            None => {
                tracing::warn!("no models available in catalog; defaulting to bundled model");
                let default_id = crate::models::default_model().to_string();
                fallback = ModelEntry::fallback(&default_id, &config.endpoints);
                &fallback
            }
        };

        let session_auth = auth_manager.current_or_expired();
        let credentials =
            resolve_credentials(current_model, session_auth.as_ref().map(|a| a.key.as_str()));

        sampling_config_for_model(
            current_model,
            credentials,
            config.endpoints.alpha_test_key.clone(),
            config.client_version.clone(),
            crate::managed_config::resolve_deployment_id(
                config.endpoints.deployment_key.as_deref(),
            ),
            None,
        )
    }

    /// Disk-cache origin key for this manager's current endpoints/auth shape
    /// (see [`ModelsCache::origin`]).
    fn cache_origin(&self) -> String {
        let endpoints = self.inner.cfg.read().endpoints.clone();
        let fetch_auth = *self.inner.fetch_auth.read();
        crate::remote::models_list_url(&endpoints, fetch_auth)
    }

    fn try_load_cache(&self) -> bool {
        let fetch_auth = *self.inner.fetch_auth.read();
        let Some(cached) = self
            .inner
            .cache
            .load_fresh(&fetch_auth.cache_auth_method(), &self.cache_origin())
        else {
            return false;
        };
        let cfg = self.inner.cfg.read().clone();
        *self.inner.has_fetched_real_catalog.write() = true;
        *self.inner.prefetched.write() = Some(cached.models.clone());
        self.rebuild(&cfg, Some(cached.models));
        *self.inner.etag.write() = cached.etag;
        true
    }

    fn spawn_fetch(&self, new_etag: Option<String>) {
        // Degrade to Offline: keep serving the current (cache/static) catalog.
        if !crate::util::config::resolve_remote_fetch_enabled() {
            tracing::info!("model catalog refresh skipped: remote_fetch disabled");
            return;
        }
        let cfg = self.inner.cfg.read().clone();
        let endpoints = cfg.endpoints.clone();
        let fetch_auth = *self.inner.fetch_auth.read();
        let auth_manager = self.inner.auth_manager.clone();
        let mgr = self.clone();

        tokio::task::spawn(async move {
            let auth = auth_manager.auth().await.ok();
            let new_prefetched = fetch_models_async(endpoints, auth, fetch_auth).await;
            if !mgr.apply_refresh_result(&cfg, new_prefetched, new_etag) {
                return;
            }
            tracing::info!("models manager refreshed");
            mgr.notify_models_updated();
        });
    }

    /// Fetch models, rebuild state, and notify clients.
    fn do_refresh(&self, new_etag: Option<String>, strategy: RefreshStrategy) {
        match strategy {
            RefreshStrategy::Offline => {
                if self.try_load_cache() {
                    tracing::info!("models manager refreshed from cache (offline)");
                }
            }
            RefreshStrategy::OnlineIfUncached => {
                if self.try_load_cache() {
                    tracing::info!("models manager refreshed from cache (online_if_uncached)");
                    return;
                }
                self.spawn_fetch(new_etag);
            }
            RefreshStrategy::Online => {
                self.spawn_fetch(new_etag);
            }
        }
    }

    /// Resolve the model list: tries cache first, then fetches from the network.
    pub async fn list_models(&self, strategy: RefreshStrategy) {
        match strategy {
            RefreshStrategy::Offline => {
                self.try_load_cache();
            }
            RefreshStrategy::OnlineIfUncached => {
                if self.try_load_cache() {
                    return;
                }
                self.fetch_and_apply().await;
            }
            RefreshStrategy::Online => {
                self.fetch_and_apply().await;
            }
        }
    }

    async fn fetch_and_apply(&self) {
        self.fetch_and_apply_inner(crate::util::config::resolve_remote_fetch_enabled())
            .await
    }

    /// `remote_fetch_enabled` is a parameter so tests can drive the gate
    /// without touching on-disk config layers.
    async fn fetch_and_apply_inner(&self, remote_fetch_enabled: bool) {
        // Degrade to Offline: keep serving the current (cache/static) catalog.
        if !remote_fetch_enabled {
            tracing::info!("model catalog refresh skipped: remote_fetch disabled");
            return;
        }
        let auth = self.inner.auth_manager.auth().await.ok();
        let has_auth = auth.is_some();
        let fetch_auth = *self.inner.fetch_auth.read();
        let cfg = self.inner.cfg.read().clone();
        xai_grok_telemetry::unified_log::info(
            "model catalog: fetching",
            None,
            Some(serde_json::json!({
                "has_auth": has_auth,
                "fetch_auth": format!("{fetch_auth:?}"),
            })),
        );
        let new_prefetched = fetch_models_async(cfg.endpoints.clone(), auth, fetch_auth).await;
        let success = self.apply_refresh_result(&cfg, new_prefetched, None);
        if success {
            xai_grok_telemetry::unified_log::info(
                "model catalog: fetch succeeded",
                None,
                Some(serde_json::json!({
                    "model_count": self.available().len(),
                })),
            );
        }
    }

    fn apply_refresh_result(
        &self,
        config: &config::Config,
        new_prefetched: Option<IndexMap<String, ModelEntry>>,
        new_etag: Option<String>,
    ) -> bool {
        let Some(new_prefetched) = new_prefetched else {
            tracing::warn!("model refresh failed, leaving existing models unchanged");
            xai_grok_telemetry::unified_log::warn(
                "model catalog refresh failed",
                None,
                Some(serde_json::json!({
                    "had_real_catalog": *self.inner.has_fetched_real_catalog.read(),
                })),
            );
            return false;
        };

        let first_real_catalog = {
            let mut flag = self.inner.has_fetched_real_catalog.write();
            let was_first = !*flag;
            *flag = true;
            was_first
        };
        *self.inner.prefetched.write() = Some(new_prefetched.clone());
        self.rebuild(config, Some(new_prefetched));
        *self.inner.etag.write() = new_etag;

        // Can't exit a running app; flag it so the prompt path blocks instead.
        let excludes_all = allowlist_matches_nothing(config, &self.inner.models.read());
        self.inner
            .allowlist_excludes_all
            .store(excludes_all, Ordering::Relaxed);
        if excludes_all {
            tracing::error!("allowed_models excludes all fetched models; prompts will be blocked");
        }

        if first_real_catalog {
            self.reselect_default_model(config);
        } else {
            self.reselect_current_model_if_missing(config);
        }
        true
    }

    pub fn allowlist_excludes_all(&self) -> bool {
        self.inner.allowlist_excludes_all.load(Ordering::Relaxed)
    }

    /// Re-pick the default if `current_model_id` is gone from the catalog *or*
    /// is no longer `user_selectable` (e.g. a config reload narrowed
    /// `allowed_models`), so UI and sampling don't disagree on the active model.
    fn reselect_current_model_if_missing(&self, config: &config::Config) {
        let current = self.inner.current_model_id.read().clone();
        let needs_reselection = {
            let models = self.inner.models.read();
            match models.get(current.0.as_ref()) {
                None => true,
                Some(entry) => !entry.info.user_selectable,
            }
        };
        if !needs_reselection {
            return;
        }
        let (key, _, source) = {
            let models = self.inner.models.read();
            resolve_default_model(config, &models, self.is_session_auth())
        };
        let new_id = acp::ModelId::new(Arc::from(key));
        tracing::info!(
            old = %current.0, new = %new_id.0, source = %source,
            "current model not in new catalog, reselecting default"
        );
        *self.inner.current_model_id.write() = new_id;
    }

    /// Re-resolve the default model against the current catalog.
    ///
    /// Called on first catalog fetch and when `apply_config` detects a
    /// preferred-model change.
    fn reselect_default_model(&self, config: &config::Config) {
        let (key, _, source) = {
            let models = self.inner.models.read();
            resolve_default_model(config, &models, self.is_session_auth())
        };
        let new_id = acp::ModelId::new(Arc::from(key));
        let current = self.inner.current_model_id.read().clone();
        if current.0.as_ref() != new_id.0.as_ref() {
            tracing::info!(
                old = %current.0, new = %new_id.0, source = %source,
                "re-resolved default model after catalog populated"
            );
            *self.inner.current_model_id.write() = new_id;
        }
    }
}

// ── Refresh strategy ────────────────────────────────────────────────────────

/// How to resolve the model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always fetch from network, ignore cache.
    Online,
    /// Only use cached data, never fetch.
    Offline,
    /// Use cache if fresh, otherwise fetch.
    OnlineIfUncached,
}

// ── Disk cache ──────────────────────────────────────────────────────────────

const MODELS_CACHE_FILE: &str = "models_cache.json";
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(serde::Serialize, serde::Deserialize)]
struct ModelsCache {
    fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grok_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_method: Option<CacheAuthMethod>,
    /// Models-list URL this catalog was fetched from
    /// ([`crate::remote::models_list_url`]). Compared on load so a cache
    /// written against one backend is a miss for another: entries embed
    /// absolute `base_url`s, so adopting a foreign-origin cache silently
    /// re-points inference (the windows lifecycle e2e failed exactly this
    /// way — test 1's mock-server catalog, cached in the shared profile,
    /// sent test 2's prompts to a dead port). `None` (legacy files) never
    /// matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    models: IndexMap<String, ModelEntry>,
}

impl ModelsCache {
    fn is_fresh(&self, ttl: std::time::Duration) -> bool {
        let Ok(ttl) = ChronoDuration::from_std(ttl) else {
            return false;
        };
        let age = Utc::now().signed_duration_since(self.fetched_at);
        age >= ChronoDuration::zero() && age < ttl
    }
}

struct CacheResult {
    models: IndexMap<String, ModelEntry>,
    etag: Option<String>,
}

struct ModelsCacheManager {
    path: std::path::PathBuf,
    ttl: std::time::Duration,
}

impl ModelsCacheManager {
    fn new() -> Self {
        Self {
            path: crate::util::grok_home::grok_home().join(MODELS_CACHE_FILE),
            ttl: CACHE_TTL,
        }
    }

    /// Sync; used by `prefetch_models_blocking`. Will be removed once startup
    /// prefetch is async.
    fn load_fresh(
        &self,
        expected_auth: &CacheAuthMethod,
        expected_origin: &str,
    ) -> Option<CacheResult> {
        let data = std::fs::read(&self.path).ok()?;
        let cache: ModelsCache = serde_json::from_slice(&data).ok()?;
        if cache.grok_version.as_deref() != Some(xai_grok_version::VERSION) {
            tracing::debug!("models cache version mismatch");
            return None;
        }
        if cache.auth_method.as_ref() != Some(expected_auth) {
            tracing::debug!("models cache auth method mismatch");
            return None;
        }
        if cache.origin.as_deref() != Some(expected_origin) {
            tracing::debug!(
                cached = ?cache.origin,
                expected = expected_origin,
                "models cache origin mismatch"
            );
            return None;
        }
        if !cache.is_fresh(self.ttl) {
            tracing::debug!("models cache is stale");
            return None;
        }
        tracing::debug!(count = cache.models.len(), "loaded models from disk cache");
        Some(CacheResult {
            models: cache.models,
            etag: cache.etag,
        })
    }

    /// Sync; see `load_fresh` note.
    fn persist(
        &self,
        models: &IndexMap<String, ModelEntry>,
        etag: Option<&str>,
        auth_method: CacheAuthMethod,
        origin: &str,
    ) {
        let cache = ModelsCache {
            fetched_at: Utc::now(),
            grok_version: Some(xai_grok_version::VERSION.to_string()),
            auth_method: Some(auth_method),
            origin: Some(origin.to_string()),
            etag: etag.map(|s| s.to_string()),
            models: models.clone(),
        };
        self.atomic_write(&cache);
    }

    async fn renew_ttl(&self, expected_auth: &CacheAuthMethod, expected_origin: &str) {
        let data = match tokio::fs::read(&self.path).await {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(error = %e, "models cache TTL renewal: read failed");
                return;
            }
        };
        let Ok(mut cache) = serde_json::from_slice::<ModelsCache>(&data) else {
            return;
        };
        if cache.auth_method.as_ref() != Some(expected_auth) {
            tracing::debug!("models cache TTL renewal skipped: auth method mismatch");
            return;
        }
        if cache.origin.as_deref() != Some(expected_origin) {
            tracing::debug!("models cache TTL renewal skipped: origin mismatch");
            return;
        }
        cache.fetched_at = Utc::now();
        self.atomic_write_async(&cache).await;
        tracing::debug!("models cache TTL renewed");
    }

    /// Sync; see `load_fresh` note.
    fn invalidate(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::info!("models disk cache invalidated"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, "failed to invalidate models disk cache"),
        }
    }

    /// Sync; see `load_fresh` note.
    fn atomic_write(&self, cache: &ModelsCache) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(json) = serde_json::to_vec_pretty(cache)
            && std::fs::write(&tmp, &json).is_ok()
        {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    async fn atomic_write_async(&self, cache: &ModelsCache) {
        if let Some(parent) = self.path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let tmp = self.path.with_extension("json.tmp");
        let Ok(json) = serde_json::to_vec_pretty(cache) else {
            return;
        };
        if tokio::fs::write(&tmp, &json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &self.path).await;
        }
    }
}

// ── Fetch ───────────────────────────────────────────────────────────────────

/// Build the prefetched model map from a flat list of entries.
///
/// Each entry is keyed by its `id` field (falling back to the `model` slug
/// when `id` is absent). This lets A/B experiments that share the same
/// routing slug (e.g. "Auto" and "Grok Build" both route to `grok-build`)
/// coexist in the catalog without collision.
fn build_prefetched_map(
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

/// Blocking models + `/v1/settings` prefetch pair, shared by the early
/// prefetch thread and the leader's startup phase so the settings gate lives
/// once. The remote_fetch knob is resolved a single time so the two fetch
/// decisions cannot disagree mid-startup.
pub(crate) fn prefetch_models_and_settings_blocking(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
) -> (
    Option<IndexMap<String, ModelEntry>>,
    Option<crate::util::config::RemoteSettings>,
) {
    let remote_fetch_enabled = crate::util::config::resolve_remote_fetch_enabled();
    let models = prefetch_models_blocking_gated(endpoints, auth, fetch_auth, remote_fetch_enabled);
    // Settings need a grok.com session; skip for BYOK.
    let settings = match auth {
        Some(auth) if remote_fetch_enabled => {
            let _timer = crate::instrumentation_timer!("startup.early_settings_fetch");
            crate::remote::fetch_settings_blocking(
                &endpoints.proxy_url(),
                auth,
                endpoints.alpha_test_key.as_deref(),
            )
        }
        _ => None,
    };
    (models, settings)
}

/// `remote_fetch_enabled` is a parameter so the pair helper above resolves the
/// knob once for both halves.
fn prefetch_models_blocking_gated(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
    remote_fetch_enabled: bool,
) -> Option<IndexMap<String, ModelEntry>> {
    let cache_auth = fetch_auth.cache_auth_method();
    // Same URL the fetch below will hit — the cache is only valid for it.
    let cache_origin = crate::remote::models_list_url(endpoints, fetch_auth);
    let cache = ModelsCacheManager::new();
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

    // Every catalog fetch in the product funnels through here, so this single
    // gate also covers callers that don't go through the prefetch-env check
    // (leader, headless, stdio, server). Cache above is local and stays usable.
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

            // NOTE: inheriting context_window / agent_type / api_backend
            // from hardcoded defaults is handled centrally in
            // `resolve_model_list` (config.rs), not here. Don't re-add it.

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

    // Live Kimi Code / Moonshot catalogs (K3, etc.) merge in whether or not
    // the xAI fetch succeeded — platform-only logins still get a real list.
    crate::agent::platform_models_fetch::fetch_and_merge_platform_models(xai_map, &platforms)
}

/// Startup prefetch result: models + remote settings.
pub struct EarlyPrefetchResult {
    pub models: Option<IndexMap<String, ModelEntry>>,
    pub settings: Option<crate::util::config::RemoteSettings>,
}

/// Handle for a startup prefetch thread.
pub type EarlyPrefetchHandle = std::thread::JoinHandle<EarlyPrefetchResult>;

struct PrefetchEnv {
    auth: Option<GrokAuth>,
    endpoints: config::EndpointsConfig,
    model_fetch_auth: ModelFetchAuth,
}

fn resolve_prefetch_env_with_auth(auth: Option<GrokAuth>) -> Option<PrefetchEnv> {
    let _timer = crate::instrumentation_timer!("startup.early_prefetch_launch");
    // Config-aware (not env-only) so the prefetch can't leak the bearer to api.x.ai.
    let mut endpoints = config::EndpointsConfig::from_effective_config();

    if endpoints.deployment_key.is_none() {
        endpoints.deployment_key = crate::managed_config::resolve_deployment_key();
    }

    resolve_prefetch_env_from_parts(
        auth,
        endpoints,
        crate::util::config::resolve_remote_fetch_enabled(),
    )
}

/// Decision core of [`resolve_prefetch_env_with_auth`], split from the config
/// loading so the gate is unit-testable.
///
/// `remote_fetch_enabled = false` wins over every credential shape AND over
/// `has_custom_endpoint()` (which otherwise forces the prefetch to run): the
/// explicit off switch must hold even when a stray login, `XAI_API_KEY`, or
/// `deployment_key` would re-arm the prefetch — and with it the `/v1/settings`
/// fetch and the deployment-config sync on the prefetch thread.
fn resolve_prefetch_env_from_parts(
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
    // built-in platform has credentials so Kimi Code / Moonshot live
    // `/models` can run (offline builtins alone are not enough for K3).
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

/// True when Kimi OAuth, Codex OAuth, or any Moonshot API key is available
/// for a live platform catalog fetch. Used to arm startup prefetch without
/// an xAI session.
fn has_any_platform_credentials() -> bool {
    if crate::auth::kimi::kimi_code_access_token_cached().is_some() {
        return true;
    }
    // Startup gating must stay disk-only. Codex live-model fetching is
    // currently disabled, and refreshing its access token here can otherwise
    // block startup merely because a persisted login exists.
    if crate::auth::openai_codex::openai_codex_catalog_access_token_cached().is_some() {
        return true;
    }
    let platforms = crate::agent::platform_models_fetch::load_platforms_config();
    xai_grok_models::PlatformId::ALL.iter().any(|p| {
        !p.uses_oauth() && crate::agent::config::resolve_platform_api_key(*p, &platforms).is_some()
    })
}

fn resolve_prefetch_env(grok_com_config: Option<GrokComConfig>) -> Option<PrefetchEnv> {
    let grok_home = crate::util::grok_home::grok_home();
    let auth_manager = AuthManager::new(&grok_home, grok_com_config.unwrap_or_default());
    let auth = auth_manager.current();
    resolve_prefetch_env_with_auth(auth)
}

/// Start model + settings prefetch on a background thread using pre-resolved auth.
///
/// When the caller has already obtained valid credentials (e.g. via
/// `try_ensure_fresh_auth`), pass them here to avoid re-reading stale cached
/// credentials from disk.
pub fn start_early_prefetch_with_auth(auth: Option<GrokAuth>) -> Option<EarlyPrefetchHandle> {
    let env = resolve_prefetch_env_with_auth(auth)?;
    Some(spawn_prefetch_thread(env))
}

/// Start model + settings prefetch on a background thread.
///
/// Convenience wrapper that reads cached auth from disk. Prefer
/// `start_early_prefetch_with_auth` when you have pre-resolved credentials.
pub fn start_early_prefetch(grok_com_config: Option<GrokComConfig>) -> Option<EarlyPrefetchHandle> {
    let env = resolve_prefetch_env(grok_com_config)?;
    Some(spawn_prefetch_thread(env))
}

fn spawn_prefetch_thread(env: PrefetchEnv) -> EarlyPrefetchHandle {
    std::thread::spawn(move || {
        let mut timer = crate::instrumentation_timer!("startup.early_prefetch");
        let proxy_endpoint = env.endpoints.proxy_url();
        timer.with_field("endpoint", proxy_endpoint.as_str());
        let (models, settings) = prefetch_models_and_settings_blocking(
            &env.endpoints,
            env.auth.as_ref(),
            env.model_fetch_auth,
        );
        if (env.endpoints.deployment_key.is_some() || crate::managed_config::has_active_team_auth())
            && crate::config::is_managed_config_stale_for(
                &crate::managed_config::current_serving_identity(),
            )
            && crate::managed_config::is_fetch_enabled()
            && let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        {
            crate::managed_config::clear_orphan();
            let _ = rt.block_on(crate::managed_config::sync());
        }

        EarlyPrefetchResult { models, settings }
    })
}

/// Map a model id (catalog key or routing slug) to its catalog key.
///
/// Sessions persist the routing slug (`[model.X].model`, e.g. `grok-4.5`);
/// the catalog and `/model` picker use config keys (e.g. `enterprise-grok-build`).
/// Last slug match wins so user overrides beat defaults (matches `MvpAgent::resolve_model_id`).
pub(crate) fn resolve_catalog_key(
    models: &IndexMap<String, ModelEntry>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    let id_str = id.0.as_ref();
    if models.contains_key(id_str) {
        return Some(id.clone());
    }
    models
        .iter()
        .rev()
        .find(|(_, entry)| entry.info.model == id_str)
        .map(|(key, _)| acp::ModelId::new(key.clone()))
}

/// Catalog key for a persisted session model id, restricted to **selectable**
/// entries. A selectable exact-key match wins (as in [`resolve_catalog_key`]);
/// otherwise the last selectable entry whose routing slug matches `id`, so a
/// non-selectable exact-key entry never shadows a selectable slug match.
/// Whether `id` is in the ACP projection AND actually usable — locked
/// platform models (credential-less BYOK entries shown for discovery) do not
/// count, so session restore / recovery falls back instead of latching onto a
/// model that cannot sample.
pub(crate) fn usable_in_projection(
    available: &IndexMap<acp::ModelId, acp::ModelInfo>,
    id: &acp::ModelId,
) -> bool {
    available.get(id).is_some_and(|info| {
        xai_grok_sampling_types::parse_platform_lock_meta(info.meta.as_ref()).is_none()
    })
}

pub(crate) fn selectable_catalog_key_for_persisted(
    models: &IndexMap<String, ModelEntry>,
    available: &IndexMap<acp::ModelId, acp::ModelInfo>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    if usable_in_projection(available, id) {
        return Some(id.clone());
    }
    let id_str = id.0.as_ref();
    if let Some((key, _)) = models.iter().rev().find(|(key, entry)| {
        usable_in_projection(available, &acp::ModelId::new((*key).clone()))
            && entry.info.model == id_str
    }) {
        return Some(acp::ModelId::new(key.clone()));
    }
    resolve_catalog_key(models, id).filter(|key| usable_in_projection(available, key))
}

/// A "campaign-only" preferred flip: the default changed and either side's value
/// is an active campaign default, i.e. the change is attributable to a campaign
/// overlay appearing/disappearing rather than a user/CLI/env edit.
fn is_campaign_only_flip(
    old_preferred: &Option<String>,
    new_preferred: &Option<String>,
    campaign_defaults: &std::collections::HashSet<String>,
) -> bool {
    if new_preferred == old_preferred || new_preferred.is_none() {
        return false;
    }
    new_preferred
        .as_ref()
        .is_some_and(|p| campaign_defaults.contains(p))
        || old_preferred
            .as_ref()
            .is_some_and(|p| campaign_defaults.contains(p))
}

/// Pick the default model: CLI > env > config > remote-settings hint, falling
/// back to the bundled default when the catalog is empty or the preferred
/// model isn't present.
pub(crate) fn resolve_default_model(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> (String, ModelEntry, config::ConfigSource) {
    let visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| e.visible_for_auth(is_session_auth) && e.info.user_selectable)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let model_pref = config::resolve_string_flag(
        cfg.default_model_override.as_deref(),
        "GROK_DEFAULT_MODEL",
        cfg.models.default.as_deref(),
        cfg.remote_settings
            .as_ref()
            .and_then(|rs| rs.default_model.as_deref()),
    );

    let first_or_fallback = || -> (String, ModelEntry) {
        if let Some((key, first)) = visible.first() {
            return (key.clone(), first.clone());
        }
        if let Some((key, entry)) = catalog.iter().find(|(_, e)| e.info.user_selectable) {
            tracing::warn!("no auth-visible selectable model; using first selectable entry");
            return (key.clone(), entry.clone());
        }
        // Pre-catalog/degenerate only: nothing selectable. Set the bundled
        // default's flag from `allowed_models` so no reader treats it as allowed.
        tracing::warn!("no selectable models; falling back to bundled default (pre-catalog)");
        let default_id = crate::models::default_model().to_string();
        let mut entry = ModelEntry::fallback(&default_id, &cfg.endpoints);
        entry.info.user_selectable = match ModelGlobSet::compile(cfg.models.allowed_models.as_ref())
        {
            Ok(None) => true,
            Ok(Some(set)) => set.matches(&default_id, &default_id),
            Err(_) => false,
        };
        (default_id, entry)
    };

    match &model_pref {
        None => {
            let (key, first) = first_or_fallback();
            (key, first, config::ConfigSource::Default)
        }
        Some(pref) => {
            let found = visible
                .get_key_value(&pref.value)
                .or_else(|| visible.iter().find(|(_, m)| m.model == pref.value));

            if let Some((key, entry)) = found {
                (key.clone(), entry.clone(), pref.source)
            } else {
                let is_explicit = matches!(
                    pref.source,
                    config::ConfigSource::Cli
                        | config::ConfigSource::Env
                        | config::ConfigSource::Config
                );
                if is_explicit {
                    tracing::warn!(
                        model_id = %pref.value, source = %pref.source,
                        "preferred model not in available models, falling back"
                    );
                } else {
                    tracing::debug!(
                        model_id = %pref.value, source = %pref.source,
                        "remote default_model not in available models, skipping"
                    );
                }
                // A campaign default missing from the catalog falls back to the
                // pre-campaign default before the first-visible fallback. Gated
                // on the missing pref actually being the campaign-driven config
                // value — a CLI/env pref that misses the catalog is not a
                // campaign problem and must not detour through campaign state.
                let campaign_pref_missing = cfg.models.default_is_campaign_driven
                    && matches!(pref.source, config::ConfigSource::Config);
                if campaign_pref_missing
                    && let Some(prev) = cfg
                        .models
                        .pre_campaign_default
                        .as_deref()
                        .filter(|s| !s.is_empty())
                    && let Some((key, entry)) = visible
                        .get_key_value(prev)
                        .or_else(|| visible.iter().find(|(_, m)| m.model == prev))
                {
                    tracing::info!(
                        unavailable = %pref.value, fallback = %prev,
                        "campaign-driven default unavailable in catalog; recovering the pre-campaign default"
                    );
                    return (key.clone(), entry.clone(), config::ConfigSource::Config);
                }
                let (key, first) = first_or_fallback();
                (key, first, config::ConfigSource::Default)
            }
        }
    }
}

/// Filter hidden and auth-gated entries out of `catalog` and convert to ACP wire format.
///
/// The projection has two tiers:
/// 1. **Usable** models — pass [`ModelEntry::visible_for_auth`].
/// 2. **Locked** models — managed platform entries (`{platform}/{model}`) whose
///    provider credentials are not configured. They ride along with
///    `requiresApiKey` / `requiresOAuth` meta so clients can render them dimmed
///    with a setup hint (BYOK discovery) instead of hiding them outright.
///    Selecting one is rejected at `set_session_model`; the credential seam
///    never falls through to the xAI session token for these entries.
///
/// `user_selectable` is the caller's gate (`ModelsManager::available` filters
/// before calling); `hidden_models` config wins over locked display here.
pub fn available_models(
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    let visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| e.visible_for_auth(is_session_auth))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut out = config::to_acp_model_info(&visible);

    let locked: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| {
            !e.info.hidden && e.is_managed_platform_model() && !e.has_own_credentials()
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.extend(locked_platform_acp_infos(&locked));
    out
}

/// ACP wire infos for locked platform models, stamped with lock/setup meta.
fn locked_platform_acp_infos(
    locked: &IndexMap<String, ModelEntry>,
) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    use xai_grok_sampling_types as meta_keys;
    let mut infos = config::to_acp_model_info(locked);
    for (key, info) in infos.iter_mut() {
        let Some((platform, _)) = xai_grok_models::parse_managed_model_key(key.0.as_ref()) else {
            continue;
        };
        let mut map = info.meta.clone().unwrap_or_default();
        map.insert(
            meta_keys::PLATFORM_ID_META_KEY.to_string(),
            platform.as_str().into(),
        );
        map.insert(
            meta_keys::PLATFORM_NAME_META_KEY.to_string(),
            platform.display_name().into(),
        );
        map.insert(
            meta_keys::SETUP_HINT_META_KEY.to_string(),
            platform.setup_hint().into(),
        );
        if platform.uses_oauth() {
            map.insert(meta_keys::REQUIRES_OAUTH_META_KEY.to_string(), true.into());
        } else {
            map.insert(
                meta_keys::REQUIRES_API_KEY_META_KEY.to_string(),
                true.into(),
            );
            map.insert(
                meta_keys::API_KEY_ENV_META_KEY.to_string(),
                serde_json::json!(platform.api_key_env_names()),
            );
        }
        info.meta = Some(map);
    }
    infos
}

/// Compiled glob matcher shared by `allowed_models`, `disabled_models`, and
/// `hidden_models`. Patterns (globset syntax: `*`, `?`, `[...]`) are matched
/// against either the catalog key or the model id.
pub(crate) struct ModelGlobSet(GlobSet);

impl ModelGlobSet {
    /// Compile a filter list (`Ok(None)` for `None`/empty). Fails **closed**: an
    /// invalid pattern returns `Err` listing every bad one for config to reject.
    pub(crate) fn compile(patterns: Option<&Vec<String>>) -> Result<Option<Self>, Vec<String>> {
        let patterns = match patterns {
            Some(p) if !p.is_empty() => p,
            _ => return Ok(None),
        };
        let mut builder = GlobSetBuilder::new();
        let mut invalid = Vec::new();
        for pat in patterns {
            match Glob::new(pat) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(_) => invalid.push(pat.clone()),
            }
        }
        if !invalid.is_empty() {
            return Err(invalid);
        }
        builder
            .build()
            .map(|set| Some(Self(set)))
            .map_err(|e| vec![e.to_string()])
    }

    fn matches(&self, key: &str, model: &str) -> bool {
        self.0.is_match(key) || self.0.is_match(model)
    }
}

/// Single source of truth for the catalog. Applies, in order: `disabled_models`
/// (remove), `allowed_models` (mark `user_selectable`), `hidden_models` (mark
/// `hidden`). Special/internal models (web_search, subagents, …) resolve via
/// `find_model_by_id`/`models()` and ignore `user_selectable`, so they need no
/// exemption. Globs are validated at load (`Config::validate_model_filters`);
/// the arms here fail closed if one slips through.
pub fn resolve_model_catalog(
    cfg: &config::Config,
    prefetched: Option<IndexMap<String, ModelEntry>>,
) -> IndexMap<String, ModelEntry> {
    let mut catalog: IndexMap<String, ModelEntry> = config::resolve_model_list(cfg, prefetched);

    if let Ok(Some(disabled)) = ModelGlobSet::compile(cfg.models.disabled_models.as_ref()) {
        let before = catalog.len();
        catalog.retain(|key, entry| !disabled.matches(key, &entry.model));
        let removed = before - catalog.len();
        if removed > 0 {
            tracing::info!(count = removed, "disabled_models: removed from catalog");
        }
    }

    // None/empty allowlist = allow all.
    match ModelGlobSet::compile(cfg.models.allowed_models.as_ref()) {
        Ok(None) => {
            for entry in catalog.values_mut() {
                entry.info.user_selectable = true;
            }
        }
        Ok(Some(allowed)) => {
            for (key, entry) in catalog.iter_mut() {
                entry.info.user_selectable = allowed.matches(key, &entry.model);
            }
        }
        Err(bad) => {
            tracing::error!(patterns = ?bad, "allowed_models: invalid glob(s); marking nothing selectable");
            for entry in catalog.values_mut() {
                entry.info.user_selectable = false;
            }
        }
    }

    if let Ok(Some(hidden)) = ModelGlobSet::compile(cfg.models.hidden_models.as_ref()) {
        for (key, entry) in catalog.iter_mut() {
            if hidden.matches(key, &entry.model) {
                entry.info.hidden = true;
            }
        }
    }

    // Persisted default first; CLI override below wins when set.
    // Only apply if the model supports reasoning effort.
    if let Some(effort) = cfg.models.default_reasoning_effort
        && let Some(default_id) = cfg.models.default.as_deref()
        && let Some(entry) = catalog.get_mut(default_id)
        && entry.info.supports_reasoning_effort
    {
        entry.info.reasoning_effort = Some(effort);
    }

    // Skip non-reasoning models so we don't send the field to providers that reject it.
    // Also skip models whose effort menu does not include the override (e.g. `--effort none`
    // must not stamp `none` onto grok-4.5, which only offers low/medium/high).
    if let Some(effort) = cfg.reasoning_effort_override {
        for entry in catalog.values_mut() {
            if model_offers_reasoning_effort(&entry.info, effort) {
                entry.info.reasoning_effort = Some(effort);
            }
        }
    }

    catalog
}

/// Whether `effort` is a value this model will accept on the wire.
///
/// Uses the server `reasoning_efforts` menu when present; otherwise the
/// built-in low/medium/high/xhigh set (same as the pager legacy menu — no
/// `none`/`minimal`).
fn model_offers_reasoning_effort(info: &config::ModelInfo, effort: ReasoningEffort) -> bool {
    if !info.supports_reasoning_effort {
        return false;
    }
    if info.reasoning_efforts.is_empty() {
        matches!(
            effort,
            ReasoningEffort::Low
                | ReasoningEffort::Medium
                | ReasoningEffort::High
                | ReasoningEffort::Xhigh
        )
    } else {
        info.reasoning_efforts.iter().any(|opt| opt.value == effort)
    }
}

/// True when an active `allowed_models` allowlist leaves no selectable model.
/// (An excluded *default* does not count — that is recoverable by reselection.)
pub(crate) fn allowlist_matches_nothing(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> bool {
    cfg.models
        .allowed_models
        .as_ref()
        .is_some_and(|a| !a.is_empty())
        && !catalog.values().any(|e| e.info.user_selectable)
}

/// Reject an `allowed_models` allowlist that leaves no selectable model, or that
/// excludes an explicitly configured default (`default`/`-m`). Run only against a
/// real catalog (cache/prefetch/fetched), not the bundled bootstrap set.
pub(crate) fn validate_selectable(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> Result<(), String> {
    let Some(allowed) = cfg.models.allowed_models.as_ref().filter(|a| !a.is_empty()) else {
        return Ok(());
    };
    let patterns = allowed.join(", ");
    if !catalog.values().any(|e| e.info.user_selectable) {
        return Err(format!(
            "None of your available models match allowed_models ({patterns}). \
             Broaden the patterns or remove allowed_models, then try again."
        ));
    }
    for (src, id) in [
        ("default", cfg.models.default.as_deref()),
        ("-m flag", cfg.default_model_override.as_deref()),
    ] {
        if let Some(id) = id
            && let Some(entry) = catalog
                .get(id)
                .or_else(|| catalog.values().find(|e| e.model == id))
            && !entry.info.user_selectable
        {
            return Err(format!(
                "\"{id}\" (your {src}) isn't allowed by allowed_models ({patterns}). \
                 Add it to allowed_models, or set a different model."
            ));
        }
    }
    Ok(())
}

/// Async wrapper around `prefetch_models_blocking`.
pub(crate) async fn fetch_models_async(
    endpoints: config::EndpointsConfig,
    auth: Option<GrokAuth>,
    fetch_auth: ModelFetchAuth,
) -> Option<IndexMap<String, ModelEntry>> {
    tokio::task::spawn_blocking(move || {
        prefetch_models_blocking(&endpoints, auth.as_ref(), fetch_auth)
    })
    .await
    .unwrap_or(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> ModelsManager {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        // Use a temp dir so AuthManager finds no credentials — ensures
        // refresh_async bails at the auth check without needing a tokio runtime.
        let tmp = std::env::temp_dir().join("grok-test-models-manager");
        let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
        ModelsManager::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("default"),
            auth_manager,
            config::Config::default(),
        )
    }

    fn config_from_toml(toml: &str) -> config::Config {
        config::Config::new_from_toml_cfg(&toml::from_str(toml).unwrap()).unwrap()
    }

    /// Hermetic platform-credential isolation for tests that assert
    /// no-credentials behavior: unset every registry platform's API-key env
    /// vars (dev boxes export real keys) and redirect `GROK_AUTH_PATH` to a
    /// scratch dir (the real `~/.grok/auth.json` may hold a Kimi OAuth token).
    fn isolate_platform_credentials() -> (tempfile::TempDir, Vec<xai_grok_test_support::EnvGuard>) {
        let dir = tempfile::tempdir().unwrap();
        let mut guards = vec![xai_grok_test_support::EnvGuard::set(
            "GROK_AUTH_PATH",
            dir.path().join("auth.json").to_str().unwrap(),
        )];
        for platform in xai_grok_models::PlatformId::ALL {
            for name in platform.api_key_env_names() {
                guards.push(xai_grok_test_support::EnvGuard::unset(name));
            }
        }
        (dir, guards)
    }

    #[test]
    fn model_show_model_fingerprint_reads_catalog_flag() {
        let mgr = test_manager();

        // Entry with the catalog flag set → accessor returns true.
        let mut flagged = ModelEntry {
            info: config::ModelInfo::fallback("fp-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        flagged.info.show_model_fingerprint = true;
        mgr.insert_test_entry("fp-model", flagged);

        // Entry without the flag → defaults false.
        mgr.insert_test_entry(
            "plain-model",
            ModelEntry {
                info: config::ModelInfo::fallback("plain-model"),
                api_key: None,
                env_key: None,
                auth_provider: None,
                api_base_url: None,
            },
        );

        // Catalog KEY differs from the routing SLUG (custom/enterprise id): the
        // map is keyed "enterprise-key" but the model slug is "enterprise-slug".
        let mut custom = ModelEntry {
            info: config::ModelInfo::fallback("enterprise-slug"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        custom.info.show_model_fingerprint = true;
        mgr.insert_test_entry("enterprise-key", custom);

        assert!(mgr.model_show_model_fingerprint("fp-model"));
        assert!(!mgr.model_show_model_fingerprint("plain-model"));
        // Unknown model id → false (no catalog entry).
        assert!(!mgr.model_show_model_fingerprint("missing-model"));
        // Lookup by the routing SLUG must resolve to the differing catalog KEY —
        // a direct `.get(slug)` would miss this entry and wrongly return false.
        assert!(
            mgr.model_show_model_fingerprint("enterprise-slug"),
            "slug lookup must resolve to the catalog key and read the flag",
        );
        // Lookup by the catalog KEY itself still works (exact-match path).
        assert!(mgr.model_show_model_fingerprint("enterprise-key"));
    }

    /// The active model must be selectable, not the first entry of the
    /// un-allowlisted catalog.
    #[test]
    fn default_model_honors_allowlist_when_no_default_set() {
        let cfg = config_from_toml(
            r#"
            [models]
            allowed_models = ["keep-*"]
            [model.zzz-first]
            model = "zzz-first"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.keep-one]
            model = "keep-one"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
        );
        let catalog = resolve_model_catalog(&cfg, None);
        let (_key, entry, _src) = resolve_default_model(&cfg, &catalog, true);
        assert!(
            entry.info.user_selectable,
            "picked non-selectable {}",
            entry.model
        );
    }

    #[test]
    fn available_models_projects_locked_platform_models_with_setup_meta() {
        use xai_grok_sampling_types::{
            API_KEY_ENV_META_KEY, PLATFORM_ID_META_KEY, REQUIRES_API_KEY_META_KEY,
            SETUP_HINT_META_KEY,
        };
        let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
        let platform_entry = |slug: &str, key: Option<&str>, hidden: bool| {
            let mut info = config::ModelInfo::fallback(slug);
            info.hidden = hidden;
            ModelEntry {
                info,
                api_key: key.map(str::to_owned),
                env_key: None,
                auth_provider: None,
                api_base_url: None,
            }
        };
        // Locked: managed platform id without credentials.
        catalog.insert(
            "deepseek/deepseek-v4-flash".to_string(),
            platform_entry("deepseek/deepseek-v4-flash", None, false),
        );
        // Usable: same platform with a stamped key → visible tier, no lock meta.
        catalog.insert(
            "openai/gpt-5".to_string(),
            platform_entry("openai/gpt-5", Some("sk-test"), false),
        );
        // Hidden wins over locked display.
        catalog.insert(
            "groq/llama-3.3-70b-versatile".to_string(),
            platform_entry("groq/llama-3.3-70b-versatile", None, true),
        );

        let out = available_models(&catalog, false);

        let locked_id = acp::ModelId::new("deepseek/deepseek-v4-flash");
        let locked = out
            .get(&locked_id)
            .expect("locked platform model must be projected");
        let meta = locked.meta.as_ref().expect("lock meta present");
        assert_eq!(
            meta.get(REQUIRES_API_KEY_META_KEY)
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            meta.get(PLATFORM_ID_META_KEY).and_then(|v| v.as_str()),
            Some("deepseek")
        );
        assert_eq!(
            meta.get(API_KEY_ENV_META_KEY)
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(2),
            "deepseek has two env aliases"
        );
        assert!(
            meta.get(SETUP_HINT_META_KEY)
                .and_then(|v| v.as_str())
                .is_some_and(|h| h.contains("[platforms.deepseek]")),
            "setup hint mentions the config table: {meta:?}"
        );

        let usable = out
            .get(&acp::ModelId::new("openai/gpt-5"))
            .expect("credentialed entry stays visible");
        let usable_meta = usable.meta.as_ref().expect("meta present");
        assert!(
            !usable_meta
                .get(REQUIRES_API_KEY_META_KEY)
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            "credentialed entry must not be marked locked"
        );

        assert!(
            out.get(&acp::ModelId::new("groq/llama-3.3-70b-versatile"))
                .is_none(),
            "hidden_models config wins over locked display"
        );

        // Locked rows sort after the usable tier.
        let keys: Vec<_> = out.keys().map(|k| k.0.as_ref().to_string()).collect();
        assert_eq!(keys, ["openai/gpt-5", "deepseek/deepseek-v4-flash"]);
    }

    #[test]
    fn validate_selectable_rejects_bad_allowlists() {
        // Excluded explicit default → error names the default.
        let excluded = config_from_toml(
            r#"
            [models]
            default = "grok-3"
            allowed_models = ["grok-4*"]
            [model.grok-3]
            model = "grok-3"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
        );
        let catalog = resolve_model_catalog(&excluded, None);
        assert!(
            validate_selectable(&excluded, &catalog)
                .unwrap_err()
                .contains("grok-3")
        );

        // Matches nothing → error.
        let zero = config_from_toml(
            r#"
            [models]
            allowed_models = ["nomatch-*"]
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
        );
        let catalog = resolve_model_catalog(&zero, None);
        assert!(validate_selectable(&zero, &catalog).is_err());
    }

    #[tokio::test]
    async fn refresh_if_new_etag_skips_when_same() {
        let mgr = test_manager();
        // Set initial etag
        *mgr.inner.etag.write() = Some("\"abc123\"".to_string());

        // Same etag — should be a no-op (etag stays the same)
        mgr.refresh_if_new_etag("\"abc123\"".to_string()).await;
        assert_eq!(
            mgr.inner.etag.read().as_deref(),
            Some("\"abc123\""),
            "etag should remain unchanged when same"
        );
    }

    #[tokio::test]
    async fn set_current_model_id_change_fires_watch_to_all_subscribers() {
        // Two subscribers (simulating two SessionActors sharing one
        // ModelsManager catalog) both observe the change. Fast-path
        // "same id" must NOT bump the generation.
        let mgr = test_manager();
        let mut rx_a = mgr.subscribe_model_switch();
        let mut rx_b = mgr.subscribe_model_switch();
        let initial_a = *rx_a.borrow_and_update();
        let initial_b = *rx_b.borrow_and_update();
        assert_eq!(initial_a, initial_b);

        // Same id is the fast path — no bump.
        mgr.set_current_model_id(acp::ModelId::new("default"));
        // Force-yield so any spurious wakeup would have a chance to
        // surface. `try_recv` on a watch channel: use a timeout-zero
        // race; if `.changed()` resolves within 25ms we have a bug.
        let same_id_ticked =
            tokio::time::timeout(std::time::Duration::from_millis(25), rx_a.changed())
                .await
                .is_ok();
        assert!(
            !same_id_ticked,
            "set_current_model_id(same id) must NOT bump the watch generation",
        );

        // Real switch: both subscribers see the change.
        mgr.set_current_model_id(acp::ModelId::new("grok-4"));
        tokio::time::timeout(std::time::Duration::from_millis(100), rx_a.changed())
            .await
            .expect("rx_a saw the switch")
            .expect("watch channel still open");
        tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.changed())
            .await
            .expect("rx_b saw the switch")
            .expect("watch channel still open");
        assert_ne!(*rx_a.borrow(), initial_a);
        assert_eq!(*rx_a.borrow(), *rx_b.borrow());
        assert!(mgr.model_switch_generation() > initial_a);
    }

    #[tokio::test]
    async fn model_switch_generation_snapshot_reflects_current_state() {
        let mgr = test_manager();
        let start = mgr.model_switch_generation();
        mgr.set_current_model_id(acp::ModelId::new("grok-4"));
        assert_eq!(mgr.model_switch_generation(), start + 1);
        // Idempotent: same id → no bump.
        mgr.set_current_model_id(acp::ModelId::new("grok-4"));
        assert_eq!(mgr.model_switch_generation(), start + 1);
        // Another real change: another bump.
        mgr.set_current_model_id(acp::ModelId::new("grok-3"));
        assert_eq!(mgr.model_switch_generation(), start + 2);
    }

    #[test]
    fn rebuild_updates_models_and_available() {
        let mgr = test_manager();
        assert!(mgr.models().is_empty());
        assert!(mgr.available().is_empty());

        let cfg = config::Config::default();
        let mut prefetched = IndexMap::new();
        prefetched.insert(
            "test-model".to_string(),
            ModelEntry {
                info: config::ModelInfo::fallback("test-model"),
                api_key: None,
                env_key: None,
                auth_provider: None,
                api_base_url: None,
            },
        );

        mgr.rebuild(&cfg, Some(prefetched));

        assert!(
            !mgr.models().is_empty(),
            "models should be populated after rebuild"
        );
    }

    #[test]
    fn current_reasoning_effort_round_trip() {
        let mgr = test_manager();
        assert_eq!(mgr.current_reasoning_effort(), None);

        mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));
        assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::High));

        mgr.set_current_reasoning_effort(None);
        assert_eq!(mgr.current_reasoning_effort(), None);
    }

    #[test]
    fn current_reasoning_effort_seeded_from_config() {
        let tmp = std::env::temp_dir().join("grok-test-models-manager-seed");
        let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
        let mut cfg = config::Config::default();
        cfg.models.default_reasoning_effort = Some(ReasoningEffort::Xhigh);
        let mgr = ModelsManager::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("default"),
            auth_manager,
            cfg,
        );
        assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::Xhigh),);
    }

    #[test]
    fn default_reasoning_effort_only_stamps_supporting_model() {
        use indexmap::IndexMap;

        // Model that supports reasoning effort — effort should be applied.
        let mut cfg = config::Config::default();
        cfg.models.default = Some("reasoning-model".to_string());
        cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

        let mut prefetched = IndexMap::new();
        let mut reasoning_entry = ModelEntry {
            info: config::ModelInfo::fallback("reasoning-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        reasoning_entry.info.supports_reasoning_effort = true;
        prefetched.insert("reasoning-model".to_string(), reasoning_entry);

        let catalog = resolve_model_catalog(&cfg, Some(prefetched));
        assert_eq!(
            catalog["reasoning-model"].info.reasoning_effort,
            Some(ReasoningEffort::High),
            "reasoning-supporting default model should be stamped",
        );

        // Model that does NOT support reasoning effort — effort must NOT be applied.
        let mut cfg = config::Config::default();
        cfg.models.default = Some("plain-model".to_string());
        cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

        let mut prefetched = IndexMap::new();
        let plain_entry = ModelEntry {
            info: config::ModelInfo::fallback("plain-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        prefetched.insert("plain-model".to_string(), plain_entry);

        let catalog = resolve_model_catalog(&cfg, Some(prefetched));
        assert_eq!(
            catalog["plain-model"].info.reasoning_effort, None,
            "non-reasoning default model must NOT be stamped with persisted effort",
        );
    }

    #[test]
    fn reasoning_effort_override_skips_models_that_do_not_offer_level() {
        use indexmap::IndexMap;
        use xai_grok_sampling_types::ReasoningEffortOption;

        let cfg = config::Config {
            reasoning_effort_override: Some(ReasoningEffort::None),
            ..Default::default()
        };

        let mut prefetched = IndexMap::new();
        // 4.5-style: supports effort, menu is high only (no none).
        let mut no_none = ModelEntry {
            info: config::ModelInfo::fallback("grok-4.5"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        no_none.info.supports_reasoning_effort = true;
        no_none.info.reasoning_efforts = vec![ReasoningEffortOption {
            id: "high".into(),
            value: ReasoningEffort::High,
            label: "High".into(),
            description: None,
            default: true,
        }];
        no_none.info.reasoning_effort = Some(ReasoningEffort::High);
        prefetched.insert("grok-4.5".to_string(), no_none);

        // Model that explicitly offers none.
        let mut with_none = ModelEntry {
            info: config::ModelInfo::fallback("legacy-none"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        with_none.info.supports_reasoning_effort = true;
        with_none.info.reasoning_efforts = vec![ReasoningEffortOption {
            id: "none".into(),
            value: ReasoningEffort::None,
            label: "None".into(),
            description: None,
            default: true,
        }];
        prefetched.insert("legacy-none".to_string(), with_none);

        let catalog = resolve_model_catalog(&cfg, Some(prefetched));
        assert_eq!(
            catalog["grok-4.5"].info.reasoning_effort,
            Some(ReasoningEffort::High),
            "--effort none must not stamp onto models that do not offer none"
        );
        assert_eq!(
            catalog["legacy-none"].info.reasoning_effort,
            Some(ReasoningEffort::None),
            "models that list none should still accept the override"
        );
    }

    #[test]
    fn config_menu_only_model_derives_support_and_default() {
        // The config-TOML path: a model configured with ONLY `reasoning_efforts`
        // (no `supports_reasoning_effort`, no scalar `reasoning_effort`) must read
        // as supported with the marked-default option's value on the internal
        // gates that BugBot flagged (support gate + wire default).
        let mut cfg = config::Config::default();
        cfg.config_models.insert(
            "menu-only".to_string(),
            config::ConfigModelOverride {
                reasoning_efforts: vec![
                    ReasoningEffortOption {
                        id: "balanced".to_string(),
                        value: ReasoningEffort::Medium,
                        label: "Balanced".to_string(),
                        description: None,
                        default: false,
                    },
                    ReasoningEffortOption {
                        id: "deep".to_string(),
                        value: ReasoningEffort::Xhigh,
                        label: "Deep".to_string(),
                        description: None,
                        default: true,
                    },
                ],
                ..Default::default()
            },
        );
        // A sibling with no menu must stay underived (empty-list path unchanged).
        cfg.config_models
            .insert("plain".to_string(), config::ConfigModelOverride::default());

        let catalog = resolve_model_catalog(&cfg, None);
        let info = &catalog["menu-only"].info;
        assert!(
            info.supports_reasoning_effort,
            "menu-only model must derive support"
        );
        assert_eq!(
            info.reasoning_effort,
            Some(ReasoningEffort::Xhigh),
            "derived default = marked-default option value"
        );
        assert!(!catalog["plain"].info.supports_reasoning_effort);
        assert_eq!(catalog["plain"].info.reasoning_effort, None);

        // The internal getters read those derived fields.
        let tmp = std::env::temp_dir().join("grok-test-models-manager-menu-only");
        let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
        let mgr = ModelsManager::new(
            None,
            catalog,
            acp::ModelId::new("menu-only"),
            auth_manager,
            cfg,
        );
        assert!(mgr.model_supports_reasoning_effort("menu-only"));
        assert_eq!(
            mgr.model_default_reasoning_effort("menu-only"),
            Some(ReasoningEffort::Xhigh)
        );
        assert_eq!(mgr.model_reasoning_efforts("menu-only").len(), 2);
        assert!(!mgr.model_supports_reasoning_effort("plain"));
        assert_eq!(mgr.model_default_reasoning_effort("plain"), None);
    }

    #[test]
    fn cli_reasoning_effort_override_only_stamps_supporting_models() {
        use indexmap::IndexMap;

        let cfg = config::Config {
            reasoning_effort_override: Some(ReasoningEffort::High),
            ..config::Config::default()
        };

        let mut prefetched = IndexMap::new();
        let mut reasoning_entry = ModelEntry {
            info: config::ModelInfo::fallback("reasoning-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        reasoning_entry.info.supports_reasoning_effort = true;
        prefetched.insert("reasoning-model".to_string(), reasoning_entry);

        let plain_entry = ModelEntry {
            info: config::ModelInfo::fallback("plain-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        prefetched.insert("plain-model".to_string(), plain_entry);

        let catalog = resolve_model_catalog(&cfg, Some(prefetched));
        assert_eq!(
            catalog["reasoning-model"].info.reasoning_effort,
            Some(ReasoningEffort::High),
            "reasoning-supporting model should be stamped",
        );
        assert_eq!(
            catalog["plain-model"].info.reasoning_effort, None,
            "non-reasoning model must NOT be stamped",
        );
    }

    #[test]
    fn apply_refresh_result_only_updates_etag_on_success() {
        let mgr = test_manager();
        let cfg = config::Config::default();
        *mgr.inner.etag.write() = Some("\"old\"".to_string());

        assert!(
            !mgr.apply_refresh_result(&cfg, None, Some("\"new\"".to_string())),
            "failed refresh should report no update"
        );
        assert_eq!(
            mgr.inner.etag.read().as_deref(),
            Some("\"old\""),
            "etag should remain unchanged when refresh fails"
        );
        assert!(
            mgr.prefetched().is_none(),
            "prefetched models should stay unchanged"
        );
    }

    fn make_model_entry(model_id: &str) -> ModelEntry {
        ModelEntry {
            info: config::ModelInfo::fallback(model_id),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        }
    }

    fn make_prefetched(ids: &[&str]) -> IndexMap<String, ModelEntry> {
        ids.iter()
            .map(|id| (id.to_string(), make_model_entry(id)))
            .collect()
    }

    // ── auth-change refresh: has_fetched_real_catalog flag ─────────────

    #[test]
    fn first_apply_refresh_reselects_default_model() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-3".to_string());

        assert!(!mgr.has_fetched_real_catalog());

        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        assert!(mgr.has_fetched_real_catalog());
        assert_eq!(mgr.current_model_id().0.as_ref(), "grok-3");
    }

    #[test]
    fn subsequent_apply_refresh_preserves_user_model() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-3".to_string());

        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        mgr.set_current_model_id(acp::ModelId::new("grok-4"));

        // Simulate on_auth_changed clearing prefetched + etag.
        *mgr.inner.prefetched.write() = None;
        *mgr.inner.etag.write() = None;

        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "grok-4",
            "user's model selection must survive auth-change refresh"
        );
    }

    #[test]
    fn subsequent_refresh_reselects_when_model_removed() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-3".to_string());

        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        mgr.set_current_model_id(acp::ModelId::new("grok-4"));

        // Second refresh with grok-4 removed.
        let prefetched = make_prefetched(&["grok-3", "grok-4.5"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "grok-3",
            "should fall back to config default when current is removed"
        );
    }

    #[test]
    fn failed_refresh_does_not_set_has_fetched_real_catalog() {
        let mgr = test_manager();
        let cfg = config::Config::default();

        mgr.apply_refresh_result(&cfg, None, None);

        assert!(
            !mgr.has_fetched_real_catalog(),
            "failed refresh must not flip has_fetched_real_catalog"
        );
    }

    // ── apply_config: honor changed preferred model from config ────────

    #[test]
    fn apply_config_honors_new_preferred_model() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-3".to_string());

        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        mgr.set_current_model_id(acp::ModelId::new("grok-4"));

        // Simulate stale inner cfg (no default) from a racing auth refresh.
        let mut stale_cfg = config::Config::default();
        stale_cfg.models.default = None;
        *mgr.inner.cfg.write() = stale_cfg;

        let mut new_cfg = config::Config::default();
        new_cfg.models.default = Some("grok-3".to_string());
        mgr.apply_config(new_cfg);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "grok-3",
            "apply_config must honor updated preferred model from config"
        );
    }

    #[test]
    fn apply_config_preserves_current_when_preferred_unchanged() {
        let mgr = test_manager();
        let cfg = config::Config::default();

        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        mgr.set_current_model_id(acp::ModelId::new("grok-4"));

        // Unrelated config change — preferred model unchanged.
        let new_cfg = config::Config::default();
        mgr.apply_config(new_cfg);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "grok-4",
            "apply_config must not reset model when preferred hasn't changed"
        );
    }

    #[test]
    fn apply_config_falls_back_when_preferred_not_in_catalog() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-3".to_string());

        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        mgr.set_current_model_id(acp::ModelId::new("grok-4"));

        // Preferred model not in catalog — falls back to first entry.
        let mut new_cfg = config::Config::default();
        new_cfg.models.default = Some("grok-nonexistent".to_string());
        mgr.apply_config(new_cfg);

        let current = mgr.current_model_id();
        let first_available = mgr.available().keys().next().unwrap().clone();
        assert_eq!(
            current.0.as_ref(),
            first_available.0.as_ref(),
            "should fall back to first visible model when preferred not in catalog"
        );
    }

    #[test]
    fn apply_config_both_none_preferred_preserves_current() {
        let mgr = test_manager();
        let cfg = config::Config::default();
        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        mgr.set_current_model_id(acp::ModelId::new("grok-4"));
        let new_cfg = config::Config::default();
        mgr.apply_config(new_cfg);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "grok-4",
            "both-None preferred must preserve user's runtime model"
        );
    }

    #[test]
    fn apply_config_old_some_new_none_preserves_current() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-3".to_string());

        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        assert_eq!(mgr.current_model_id().0.as_ref(), "grok-3");

        mgr.set_current_model_id(acp::ModelId::new("grok-4"));

        // [models] default removed — is_some() guard prevents reset.
        let new_cfg = config::Config::default();
        mgr.apply_config(new_cfg);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "grok-4",
            "old=Some new=None must not reset model (is_some guard)"
        );
    }

    // ── end-to-end: auth refresh + config reload compose correctly ───

    #[test]
    fn auth_refresh_then_config_reload_preserves_user_model() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-3".to_string());

        // Initial fetch.
        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        // User runs /model grok-4.
        mgr.set_current_model_id(acp::ModelId::new("grok-4"));

        // Auth refresh races — clears prefetched/etag.
        *mgr.inner.prefetched.write() = None;
        *mgr.inner.etag.write() = None;

        // Second fetch must preserve user's model.
        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");

        // Config reload with persisted preference.
        let mut new_cfg = config::Config::default();
        new_cfg.models.default = Some("grok-4".to_string());
        mgr.apply_config(new_cfg);
        assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");
    }

    // ── disk-cache hot-reload (external models_cache.json writes) ────

    fn test_cache_manager(dir: &std::path::Path) -> ModelsCacheManager {
        ModelsCacheManager {
            path: dir.join(MODELS_CACHE_FILE),
            ttl: CACHE_TTL,
        }
    }

    /// An external process persisting a fresh catalog must be picked up:
    /// catalog swapped, etag adopted, real-catalog flag set.
    #[test]
    fn reload_from_disk_cache_applies_external_catalog() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());

        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &make_prefetched(&["grok-4.5", "grok-4.3"]),
            Some("etag-ext"),
            auth_method,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(mgr.has_fetched_real_catalog());
        assert!(mgr.models().contains_key("grok-4.5"));
        assert!(mgr.models().contains_key("grok-4.3"));
        assert_eq!(mgr.inner.etag.read().as_deref(), Some("etag-ext"));
    }

    /// A latched "allowlist excludes everything" prompt block must clear when
    /// an external cache write delivers a catalog the allowlist matches —
    /// `reload_from_cache_manager` recomputes `allowlist_excludes_all` after
    /// the rebuild, like `apply_refresh_result` does.
    #[test]
    fn reload_from_disk_cache_recomputes_allowlist_excludes_all() {
        let mgr = test_manager();
        let cfg = config_from_toml("[models]\nallowed_models = [\"keep-*\"]");

        // Latch the flag: neither the fetched model nor the bundled defaults
        // merged by `resolve_model_catalog` match `keep-*`.
        mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["other-1"])), None);
        assert!(
            mgr.allowlist_excludes_all(),
            "setup: allowlist should exclude the entire catalog"
        );
        // `apply_refresh_result` borrows the config without storing it, while
        // `reload_from_cache_manager` reads `inner.cfg` — install it there.
        *mgr.inner.cfg.write() = cfg.clone();

        // External process persists a catalog containing an allowed model.
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &make_prefetched(&["keep-1"]),
            Some("etag-keep"),
            auth_method,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(mgr.models().contains_key("keep-1"));
        assert!(
            !mgr.allowlist_excludes_all(),
            "corrective external cache write must unlatch the prompt block"
        );
    }

    /// When the *first* real catalog arrives via an external cache write (the
    /// leader never completed its own fetch), the configured `[models]`
    /// default must be resolved — mirroring `apply_refresh_result`'s
    /// first-catalog branch — instead of staying on the bundled placeholder.
    #[test]
    fn reload_from_disk_cache_resolves_default_on_first_catalog() {
        let mgr = test_manager();
        assert!(!mgr.has_fetched_real_catalog());
        let cfg = config_from_toml("[models]\ndefault = \"keep-1\"");
        // `reload_from_cache_manager` reads the manager's stored config.
        *mgr.inner.cfg.write() = cfg.clone();

        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &make_prefetched(&["keep-1", "other-1"]),
            Some("etag-first"),
            auth_method,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(mgr.has_fetched_real_catalog());
        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "keep-1",
            "first real catalog must resolve the configured default"
        );
    }

    /// A cache write whose catalog matches the in-memory prefetched map (the
    /// leader's own `persist`/`renew_ttl` self-writes, or a same-content fetch
    /// by another process) must be a no-op apart from adopting the etag — no
    /// rebuild, no model reselection.
    #[test]
    fn reload_from_disk_cache_skips_identical_catalog_and_adopts_etag() {
        let mgr = test_manager();
        let cfg = config::Config::default();
        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched.clone()), Some("etag-a".into()));
        mgr.set_current_model_id(acp::ModelId::new("grok-4"));

        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &prefetched,
            Some("etag-b"),
            auth_method,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "grok-4",
            "identical catalog must not disturb the user's model"
        );
        assert_eq!(
            mgr.inner.etag.read().as_deref(),
            Some("etag-b"),
            "etag should be adopted so refresh_if_new_etag stays accurate"
        );
    }

    /// A cache file older than the TTL is rejected by `load_fresh` — the
    /// watcher event arrives within the debounce window of the write, so a
    /// stale file means the write was not a fresh fetch.
    #[test]
    fn reload_from_disk_cache_ignores_stale_cache() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        let stale = ModelsCache {
            fetched_at: Utc::now() - ChronoDuration::seconds(3600),
            grok_version: Some(xai_grok_version::VERSION.to_string()),
            auth_method: Some(auth_method),
            origin: Some(mgr.cache_origin()),
            etag: Some("etag-stale".into()),
            models: make_prefetched(&["grok-stale"]),
        };
        cache.atomic_write(&stale);

        mgr.reload_from_cache_manager(&cache);

        assert!(!mgr.models().contains_key("grok-stale"));
        assert!(mgr.inner.etag.read().is_none());
    }

    /// A cache persisted by a process running with different credentials
    /// (e.g. an API-key `--no-leader` run next to a session-auth leader)
    /// must not poison this manager's catalog.
    #[test]
    fn reload_from_disk_cache_ignores_auth_method_mismatch() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let current = mgr.inner.fetch_auth.read().cache_auth_method();
        let other = if current == CacheAuthMethod::Session {
            CacheAuthMethod::ApiKey
        } else {
            CacheAuthMethod::Session
        };
        cache.persist(
            &make_prefetched(&["grok-other-auth"]),
            Some("etag-x"),
            other,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(!mgr.models().contains_key("grok-other-auth"));
    }

    /// A cache persisted by a process pointed at a *different backend* (env
    /// override, another deployment, a test's mock server) must not poison
    /// this manager's catalog: cached entries embed absolute `base_url`s from
    /// their origin, so adopting them silently re-points inference. This is
    /// the windows-x86_64 lifecycle e2e failure mode — the shared-profile
    /// cache from test 1's mock sent test 2's prompts to a dead port.
    #[test]
    fn reload_from_disk_cache_ignores_origin_mismatch() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &make_prefetched(&["grok-other-origin"]),
            Some("etag-y"),
            auth_method,
            "http://127.0.0.1:49953/v1/models",
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(!mgr.models().contains_key("grok-other-origin"));
        assert!(mgr.inner.etag.read().is_none());
    }

    /// A legacy cache file written before the `origin` field existed must be
    /// treated as a miss (`None` origin never matches) — its entries could
    /// have come from anywhere.
    #[test]
    fn reload_from_disk_cache_ignores_legacy_cache_without_origin() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        let legacy = ModelsCache {
            fetched_at: Utc::now(),
            grok_version: Some(xai_grok_version::VERSION.to_string()),
            auth_method: Some(auth_method),
            origin: None,
            etag: Some("etag-legacy".into()),
            models: make_prefetched(&["grok-legacy"]),
        };
        cache.atomic_write(&legacy);

        mgr.reload_from_cache_manager(&cache);

        assert!(!mgr.models().contains_key("grok-legacy"));
    }

    // ── clear() resets has_fetched_real_catalog ──────────────────────

    #[test]
    fn clear_resets_has_fetched_real_catalog() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-3".to_string());

        let prefetched = make_prefetched(&["grok-3", "grok-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        assert!(mgr.has_fetched_real_catalog());

        mgr.clear();
        assert!(!mgr.has_fetched_real_catalog());

        // New identity fetch — resolves default via reselect_default_model.
        let prefetched = make_prefetched(&["grok-4.5", "grok-4.3"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        let first_available = mgr.available().keys().next().unwrap().clone();
        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            first_available.0.as_ref()
        );
    }

    /// A flip is "campaign-only" iff the preferred changed and either side is an
    /// active campaign default.
    #[test]
    fn is_campaign_only_flip_detects_campaign_driven_changes() {
        let camp: std::collections::HashSet<String> = ["beta".into()].into_iter().collect();
        // New side is the campaign default (campaign appearing) → campaign-only.
        assert!(is_campaign_only_flip(
            &Some("alpha".into()),
            &Some("beta".into()),
            &camp
        ));
        // Old side was the campaign default (campaign withdrawing) → campaign-only.
        assert!(is_campaign_only_flip(
            &Some("beta".into()),
            &Some("alpha".into()),
            &camp
        ));
        // Neither side a campaign default → ordinary user/CLI/env flip.
        assert!(!is_campaign_only_flip(
            &Some("alpha".into()),
            &Some("gamma".into()),
            &camp
        ));
        // No change, cleared default, or empty campaign set → never campaign-only.
        assert!(!is_campaign_only_flip(
            &Some("beta".into()),
            &Some("beta".into()),
            &camp
        ));
        assert!(!is_campaign_only_flip(&Some("beta".into()), &None, &camp));
        assert!(!is_campaign_only_flip(
            &Some("alpha".into()),
            &Some("beta".into()),
            &std::collections::HashSet::new()
        ));
    }

    /// A campaign-only flip must NOT reselect a live session whose current model
    /// is still selectable; a non-campaign flip must. "Campaign-driven" is marked
    /// by `default_is_campaign_driven` on the incoming config.
    #[test]
    fn campaign_only_flip_does_not_reselect_live_session() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("alpha".to_string());
        mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["alpha", "beta"])), None);
        *mgr.inner.cfg.write() = cfg.clone(); // old_preferred = "alpha"
        assert_eq!(mgr.current_model_id().0.as_ref(), "alpha");

        let mut new_cfg = config::Config::default();
        new_cfg.models.default = Some("beta".to_string());
        new_cfg.models.default_is_campaign_driven = true; // campaign overriding
        mgr.apply_config(new_cfg);
        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "alpha",
            "campaign-only flip must not yank a still-selectable live session"
        );

        // Control: same flip with no campaign (no pre_campaign_default) → reselect.
        let mgr2 = test_manager();
        let mut cfg2 = config::Config::default();
        cfg2.models.default = Some("alpha".to_string());
        mgr2.apply_refresh_result(&cfg2, Some(make_prefetched(&["alpha", "beta"])), None);
        *mgr2.inner.cfg.write() = cfg2.clone();
        let mut new_cfg2 = config::Config::default();
        new_cfg2.models.default = Some("beta".to_string());
        mgr2.apply_config(new_cfg2);
        assert_eq!(
            mgr2.current_model_id().0.as_ref(),
            "beta",
            "a non-campaign preferred change must reselect"
        );
    }

    /// A campaign default missing from the catalog falls back to
    /// `pre_campaign_default`, then to the first visible model — and only when
    /// the missing pref is actually the campaign-driven config value.
    #[test]
    fn unavailable_campaign_default_falls_back_to_config_default() {
        let catalog = make_prefetched(&["real-model", "other-model"]);

        let mut cfg = config::Config::default();
        cfg.models.default = Some("missing-model".to_string());
        cfg.models.default_is_campaign_driven = true;
        cfg.models.pre_campaign_default = Some("real-model".to_string());
        let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
        assert_eq!(
            key, "real-model",
            "must fall back to the pre-campaign default"
        );

        // Control: pre-campaign default also absent → first visible model.
        let mut cfg2 = config::Config::default();
        cfg2.models.default = Some("missing-model".to_string());
        cfg2.models.default_is_campaign_driven = true;
        cfg2.models.pre_campaign_default = Some("also-missing".to_string());
        let (key2, _, _) = resolve_default_model(&cfg2, &catalog, true);
        assert_eq!(&key2, catalog.keys().next().unwrap());

        // Control: not campaign-driven (e.g. stale recovery value alongside a
        // user-set default) → the campaign detour must NOT fire; a missing
        // config pref falls to the first visible model.
        let mut cfg3 = config::Config::default();
        cfg3.models.default = Some("missing-model".to_string());
        cfg3.models.pre_campaign_default = Some("real-model".to_string());
        let (key3, _, _) = resolve_default_model(&cfg3, &catalog, true);
        assert_eq!(
            &key3,
            catalog.keys().next().unwrap(),
            "non-campaign catalog miss must not recover via campaign state"
        );

        // Control: CLI override misses the catalog while campaign state is set
        // → CLI is not a campaign problem; no campaign detour.
        let mut cfg4 = config::Config {
            default_model_override: Some("missing-cli-model".to_string()),
            ..Default::default()
        };
        cfg4.models.default = Some("campaign-model".to_string());
        cfg4.models.default_is_campaign_driven = true;
        cfg4.models.pre_campaign_default = Some("real-model".to_string());
        let (key4, _, _) = resolve_default_model(&cfg4, &catalog, true);
        assert_eq!(
            &key4,
            catalog.keys().next().unwrap(),
            "a CLI pref miss must not detour through pre_campaign_default"
        );
    }

    // ── ModelFetchAuth::resolve priority tests ──────────────────────

    use serial_test::serial;
    use xai_grok_test_support::EnvGuard;

    #[test]
    #[serial]
    fn resolve_custom_endpoint_always_wins() {
        let _key = EnvGuard::set("XAI_API_KEY", "test-key");
        let endpoints = config::EndpointsConfig {
            models_base_url: Some("https://custom.example.com".to_owned()),
            ..config::EndpointsConfig::default()
        };
        assert_eq!(
            ModelFetchAuth::resolve(&endpoints, true),
            ModelFetchAuth::CustomEndpoint,
        );
        assert_eq!(
            ModelFetchAuth::resolve(&endpoints, false),
            ModelFetchAuth::CustomEndpoint,
        );
    }

    #[test]
    #[serial]
    fn resolve_cached_session_wins_over_api_key() {
        let _key = EnvGuard::set("XAI_API_KEY", "test-key");
        let endpoints = config::EndpointsConfig::default();
        assert_eq!(
            ModelFetchAuth::resolve(&endpoints, true),
            ModelFetchAuth::Session,
            "cached session should take priority over API key",
        );
    }

    #[test]
    #[serial]
    fn resolve_api_key_used_when_no_session() {
        let _key = EnvGuard::set("XAI_API_KEY", "test-key");
        let endpoints = config::EndpointsConfig::default();
        assert_eq!(
            ModelFetchAuth::resolve(&endpoints, false),
            ModelFetchAuth::ApiKey,
            "API key should be used when no cached session exists",
        );
    }

    #[test]
    #[serial]
    fn resolve_falls_back_to_session_when_nothing_set() {
        let _unset = EnvGuard::unset("XAI_API_KEY");
        let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let endpoints = config::EndpointsConfig::default();
        assert_eq!(
            ModelFetchAuth::resolve(&endpoints, false),
            ModelFetchAuth::Session,
            "should fall back to Session when nothing else is configured",
        );
    }

    #[test]
    #[serial]
    fn resolve_deployment_key_when_no_session_or_api_key() {
        let _unset = EnvGuard::unset("XAI_API_KEY");
        let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let endpoints = config::EndpointsConfig {
            deployment_key: Some("deploy-key".to_owned()),
            ..config::EndpointsConfig::default()
        };
        assert_eq!(
            ModelFetchAuth::resolve(&endpoints, false),
            ModelFetchAuth::Deployment,
        );
    }

    /// `deployment_key` outranks a stray `XAI_API_KEY`, but session wins over both.
    #[test]
    #[serial]
    fn resolve_deployment_key_outranks_ambient_api_key() {
        let _key = EnvGuard::set("XAI_API_KEY", "stray-env-key");
        let endpoints = config::EndpointsConfig {
            deployment_key: Some("deploy-key".to_owned()),
            ..config::EndpointsConfig::default()
        };
        assert_eq!(
            ModelFetchAuth::resolve(&endpoints, false),
            ModelFetchAuth::Deployment,
            "managed deployment_key should outrank an ambient XAI_API_KEY",
        );
        assert_eq!(
            ModelFetchAuth::resolve(&endpoints, true),
            ModelFetchAuth::Session,
            "an active session should still win over a managed deployment",
        );
    }

    // ── remote_fetch gate: resolve_prefetch_env_from_parts ───────────

    /// remote_fetch=false must return `None` against every re-arming shape at
    /// once — session auth, ambient `XAI_API_KEY`, `deployment_key`, AND a
    /// custom models endpoint (which normally forces the prefetch to run).
    #[test]
    #[serial]
    fn prefetch_env_none_when_remote_fetch_disabled_despite_credentials() {
        let _key = EnvGuard::set("XAI_API_KEY", "stray-env-key");
        let endpoints = config::EndpointsConfig {
            deployment_key: Some("deploy-key".to_owned()),
            models_base_url: Some("https://custom.example.com".to_owned()),
            ..config::EndpointsConfig::default()
        };
        assert!(
            resolve_prefetch_env_from_parts(
                Some(GrokAuth::test_default()),
                endpoints.clone(),
                false,
            )
            .is_none(),
            "session auth must not re-arm the prefetch when remote_fetch is off",
        );
        assert!(
            resolve_prefetch_env_from_parts(None, endpoints, false).is_none(),
            "API key / deployment key / custom endpoint must not re-arm it either",
        );
    }

    /// Inverse sanity: with remote_fetch enabled the same credential shapes DO
    /// arm the prefetch, and the credential-less default still doesn't.
    #[test]
    #[serial]
    fn prefetch_env_resolves_when_remote_fetch_enabled() {
        let _unset = EnvGuard::unset("XAI_API_KEY");
        let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        // Dev boxes export real platform keys (DEEPSEEK_API_KEY, …) and hold a
        // Kimi OAuth token in ~/.grok/auth.json — both arm the platform
        // prefetch path. Isolate so the no-credentials assertion is hermetic.
        let (_dir, _platform_guards) = isolate_platform_credentials();
        let endpoints = config::EndpointsConfig {
            deployment_key: Some("deploy-key".to_owned()),
            ..config::EndpointsConfig::default()
        };
        assert!(resolve_prefetch_env_from_parts(None, endpoints, true).is_some());
        assert!(
            resolve_prefetch_env_from_parts(None, config::EndpointsConfig::default(), true)
                .is_none(),
            "no credentials and no custom endpoint must stay a no-prefetch launch",
        );
    }

    /// remote_fetch=false: an online catalog refresh is a no-op — nothing is
    /// fetched, no real-catalog flag is set, and the static catalog keeps
    /// resolving. Covers `list_models`/`do_refresh` online strategies too,
    /// which funnel into `fetch_and_apply`/`spawn_fetch`.
    #[tokio::test]
    async fn fetch_and_apply_degrades_offline_when_remote_fetch_disabled() {
        let mgr = test_manager();
        mgr.insert_test_entry(
            "static-one",
            ModelEntry {
                info: config::ModelInfo::fallback("static-one"),
                api_key: None,
                env_key: None,
                auth_provider: None,
                api_base_url: None,
            },
        );

        mgr.fetch_and_apply_inner(false).await;

        assert!(
            !mgr.has_fetched_real_catalog(),
            "no catalog fetch may be recorded when remote_fetch is disabled",
        );
        assert!(
            mgr.models().contains_key("static-one"),
            "the static catalog must keep resolving",
        );
    }

    // ── supported_in_api tests ──────────────────────────────────────

    #[test]
    fn default_model_skips_oauth_only_for_api_key_users() {
        let cfg = config::Config::default();
        let mut catalog = IndexMap::new();

        let mut oauth_only = ModelEntry {
            info: config::ModelInfo::fallback("oauth-only"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        oauth_only.info.supported_in_api = false;
        catalog.insert("oauth-only".to_string(), oauth_only);

        let public = ModelEntry {
            info: config::ModelInfo::fallback("public-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        };
        catalog.insert("public-model".to_string(), public);

        // API-key user: default should NOT be the oauth-only model
        let (key, _, _) = resolve_default_model(&cfg, &catalog, false);
        assert_ne!(
            key, "oauth-only",
            "API-key default must not be an OAuth-only model"
        );
        assert_eq!(key, "public-model");

        // OAuth user: oauth-only is valid as default (it's first in the map)
        let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
        assert!(
            key == "oauth-only" || key == "public-model",
            "OAuth user should be able to use either model as default"
        );
    }

    #[test]
    fn visible_for_auth_logic() {
        let mut info = config::ModelInfo::fallback("test");

        // Default: visible to everyone
        assert!(info.visible_for_auth(true));
        assert!(info.visible_for_auth(false));

        // hidden = true: invisible to everyone
        info.hidden = true;
        assert!(!info.visible_for_auth(true));
        assert!(!info.visible_for_auth(false));

        // hidden = false, supported_in_api = false: visible to session only
        info.hidden = false;
        info.supported_in_api = false;
        assert!(info.visible_for_auth(true));
        assert!(!info.visible_for_auth(false));
    }

    #[test]
    fn managed_platform_models_hidden_without_credentials_even_for_session() {
        // Simulates injected openai/claude catalog entries: session auth must
        // not unlock them until OPENAI_API_KEY / ANTHROPIC_API_KEY is present.
        let mut entry = ModelEntry {
            info: config::ModelInfo::fallback("gpt-5"),
            api_key: None,
            env_key: Some(config::EnvKeys::new(["OPENAI_API_KEY_MUST_NOT_EXIST_XYZ"])),
            auth_provider: None,
            api_base_url: None,
        };
        entry.info.id = Some("openai/gpt-5".into());
        entry.info.supported_in_api = false;
        assert!(entry.is_managed_platform_model());
        assert!(
            !entry.visible_for_auth(true),
            "xAI session must not show openai/* without an API key"
        );
        assert!(!entry.visible_for_auth(false));

        entry.api_key = Some("sk-test".into());
        assert!(
            entry.visible_for_auth(true),
            "stamped API key makes the platform model selectable"
        );
        assert!(entry.visible_for_auth(false));
    }

    // ── duplicate model slug re-keying (A/B experiment "auto" alias) ──

    fn make_entry_config(model: &str, name: Option<&str>) -> config::ModelEntryConfig {
        make_entry_config_with_id(None, model, name)
    }

    fn make_entry_config_with_id(
        id: Option<&str>,
        model: &str,
        name: Option<&str>,
    ) -> config::ModelEntryConfig {
        config::ModelEntryConfig {
            id: id.map(|s| s.to_owned()),
            model: model.to_owned(),
            base_url: "https://test.api/v1".to_owned(),
            name: name.map(|n| n.to_owned()),
            description: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_key: None,
            env_key: None,
            api_backend: Default::default(),
            context_window: std::num::NonZeroU64::new(200_000).unwrap(),
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            extra_headers: IndexMap::new(),
            api_base_url: None,
            use_concise: false,
            agent_type: config::default_agent_type(),
            inference_idle_timeout_secs: None,
            max_retries: None,
            hidden: false,
            supported_in_api: true,
            auth_scheme: None,
            reasoning_effort: None,
            supports_reasoning_effort: false,
            reasoning_efforts: Vec::new(),
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: None,
            laziness_detector: config::LazinessDetectorPerModelConfig::default(),
        }
    }

    /// Experiment: two entries share the same routing slug but have distinct ids.
    /// Both survive, keyed by their respective ids.
    #[test]
    fn build_prefetched_map_distinct_ids_same_slug() {
        let entries = vec![
            make_entry_config_with_id(Some("auto"), "grok-build", Some("Auto")),
            make_entry_config_with_id(Some("grok-build"), "grok-build", Some("Grok Build")),
            make_entry_config_with_id(
                Some("grok-composer-2.5-fast"),
                "grok-composer-2.5-fast",
                Some("Grok Fast"),
            ),
        ];
        let map = build_prefetched_map(entries, None);

        assert_eq!(map.len(), 3, "all three entries should survive");
        assert!(map.contains_key("auto"));
        assert!(map.contains_key("grok-build"));
        assert!(map.contains_key("grok-composer-2.5-fast"));
        assert_eq!(
            map["auto"].info.model, "grok-build",
            "auto entry should still route to grok-build"
        );
        assert_eq!(map["grok-build"].info.model, "grok-build");
    }

    /// No id field — falls back to model slug as key.
    #[test]
    fn build_prefetched_map_no_id_falls_back_to_slug() {
        let entries = vec![
            make_entry_config("model-a", Some("Model A")),
            make_entry_config("model-b", Some("Model B")),
        ];
        let map = build_prefetched_map(entries, None);

        assert_eq!(map.len(), 2);
        assert!(map.contains_key("model-a"));
        assert!(map.contains_key("model-b"));
    }

    /// Duplicate ids — second overwrites first (same as duplicate slugs before).
    #[test]
    fn build_prefetched_map_duplicate_id_overwrites() {
        let entries = vec![
            make_entry_config_with_id(Some("grok-build"), "grok-build", Some("First")),
            make_entry_config_with_id(Some("grok-build"), "grok-build", Some("Second")),
        ];
        let map = build_prefetched_map(entries, None);

        assert_eq!(map.len(), 1, "duplicate id: second overwrites first");
        assert_eq!(map["grok-build"].info.name.as_deref(), Some("Second"));
    }

    /// Regression: resolve_default_model must match by id before scanning
    /// by model slug, otherwise entries sharing a slug resolve to whichever
    /// appears first in the catalog.
    #[test]
    fn resolve_default_model_prefers_id_over_model_slug() {
        let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
        catalog.insert(
            "auto-grok-build".to_string(),
            make_model_entry("grok-build"),
        );
        catalog.insert("grok-build".to_string(), make_model_entry("grok-build"));

        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-build".to_string());

        let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
        assert_eq!(key, "grok-build", "must match id, not first slug hit");
    }

    /// No id field — falls back to slug as key.
    #[test]
    fn build_prefetched_map_none_id_falls_back_to_slug() {
        let entries = vec![make_entry_config_with_id(
            None,
            "grok-build",
            Some("Grok Build"),
        )];
        let map = build_prefetched_map(entries, None);

        assert_eq!(map.len(), 1);
        assert!(map.contains_key("grok-build"));
    }

    // ── persisted model id → catalog key (session resume) ─────────────

    #[test]
    fn resolve_catalog_key_maps_routing_slug_to_config_key() {
        let mut models = IndexMap::new();
        models.insert(
            "enterprise-grok-build".to_string(),
            make_model_entry("grok-4.5"),
        );
        models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

        let persisted = acp::ModelId::new("grok-4.5");
        let key = resolve_catalog_key(&models, &persisted).expect("slug must resolve");
        assert_eq!(key.0.as_ref(), "enterprise-grok-build");
    }

    #[test]
    fn resolve_catalog_key_prefers_exact_key_match() {
        let mut models = IndexMap::new();
        models.insert("grok-4.5".to_string(), make_model_entry("grok-4.5"));

        let persisted = acp::ModelId::new("grok-4.5");
        let key = resolve_catalog_key(&models, &persisted).expect("exact key must resolve");
        assert_eq!(key.0.as_ref(), "grok-4.5");
    }

    #[test]
    fn resolve_catalog_key_last_slug_match_wins() {
        let mut models = IndexMap::new();
        models.insert(
            "default-grok-build".to_string(),
            make_model_entry("grok-4.5"),
        );
        models.insert("user-grok-build".to_string(), make_model_entry("grok-4.5"));

        let persisted = acp::ModelId::new("grok-4.5");
        let key = resolve_catalog_key(&models, &persisted).expect("slug must resolve");
        assert_eq!(key.0.as_ref(), "user-grok-build");
    }

    #[test]
    fn selectable_catalog_key_for_persisted_none_when_resolved_not_available() {
        let mut models = IndexMap::new();
        models.insert(
            "enterprise-grok-build".to_string(),
            make_model_entry("grok-4.5"),
        );

        let available: IndexMap<_, _> = IndexMap::new();
        let persisted = acp::ModelId::new("grok-4.5");
        assert!(selectable_catalog_key_for_persisted(&models, &available, &persisted).is_none());
    }

    #[test]
    fn selectable_prefers_available_identity_over_non_selectable_exact_key() {
        let mut models = IndexMap::new();
        models.insert("grok-build".to_string(), make_model_entry("grok-build"));
        models.insert(
            "enterprise-grok-build".to_string(),
            make_model_entry("grok-build"),
        );
        models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

        let available = test_available_keys(&["enterprise-grok-build", "grok-4.3"]);

        let persisted = acp::ModelId::new("grok-build");
        assert_eq!(
            resolve_catalog_key(&models, &persisted)
                .expect("exact key exists")
                .0
                .as_ref(),
            "grok-build"
        );
        let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
            .expect("must resolve to selectable section");
        assert_eq!(key.0.as_ref(), "enterprise-grok-build");
    }

    #[test]
    fn selectable_matches_routing_slug_when_no_exact_key() {
        let mut models = IndexMap::new();
        models.insert(
            "enterprise-grok-build".to_string(),
            make_model_entry("grok-build"),
        );
        models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

        let available = test_available_keys(&["enterprise-grok-build", "grok-4.3"]);

        let persisted = acp::ModelId::new("grok-build");
        let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
            .expect("slug must resolve to selectable key");
        assert_eq!(key.0.as_ref(), "enterprise-grok-build");
    }

    /// A persisted *selectable* catalog key binds to itself even when a later
    /// selectable section's routing slug equals that key (exact key wins).
    #[test]
    fn selectable_prefers_exact_key_over_later_slug_match() {
        let mut models = IndexMap::new();
        models.insert("grok-build".to_string(), make_model_entry("grok-4.5"));
        models.insert("other".to_string(), make_model_entry("grok-build"));

        let available = test_available_keys(&["grok-build", "other"]);

        let persisted = acp::ModelId::new("grok-build");
        let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
            .expect("exact selectable key must win");
        assert_eq!(key.0.as_ref(), "grok-build");
    }

    fn test_available_keys(keys: &[&str]) -> IndexMap<acp::ModelId, acp::ModelInfo> {
        keys.iter()
            .map(|k| {
                let id = acp::ModelId::new(*k);
                (id.clone(), acp::ModelInfo::new(id, (*k).to_string()))
            })
            .collect()
    }

    #[test]
    fn selectable_catalog_key_for_persisted_skips_locked_platform_models() {
        // Persisted a DeepSeek session, then the API key was removed: the
        // locked projection still lists the model (discovery) but restore must
        // not latch onto it.
        let mut models = IndexMap::new();
        models.insert(
            "deepseek/deepseek-v4-flash".to_string(),
            make_model_entry("deepseek-v4-flash"),
        );
        models.insert("grok-4.5".to_string(), make_model_entry("grok-4.5"));

        let mut available = test_available_keys(&["grok-4.5"]);
        let locked_id = acp::ModelId::new("deepseek/deepseek-v4-flash");
        let lock_meta = serde_json::json!({
            "requiresApiKey": true,
            "platform": "deepseek",
            "platformName": "DeepSeek",
            "apiKeyEnv": ["GROK_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
            "setupHint": "export DEEPSEEK_API_KEY=…",
        })
        .as_object()
        .cloned();
        available.insert(
            locked_id.clone(),
            acp::ModelInfo::new(locked_id.clone(), "DeepSeek V4 Flash".to_string()).meta(lock_meta),
        );

        let persisted = acp::ModelId::new("deepseek/deepseek-v4-flash");
        assert!(
            selectable_catalog_key_for_persisted(&models, &available, &persisted).is_none(),
            "locked platform model must not count as selectable for restore"
        );
        // Sanity: a usable row still resolves.
        let usable = acp::ModelId::new("grok-4.5");
        assert_eq!(
            selectable_catalog_key_for_persisted(&models, &available, &usable),
            Some(usable)
        );
    }
}
