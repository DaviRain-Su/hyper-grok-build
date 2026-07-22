//! Sampler-turn pipeline for `SessionActor`: tool definitions, model auth
//! facts/gates and retry, sampler config reconstruction, sampling-failure
//! recovery, and per-response usage recording.
use super::*;
/// Auth-failure detector for tool errors. Matches strictly on HTTP 401
/// when the error carries a structured status code, mirroring
/// `SamplingError::is_auth_error` in xai-grok-sampling-types: 403 is
/// deliberately excluded because it means "authenticated but forbidden"
/// (content-safety blocks, ZDR-gated requests, remote settings gates), where
/// a token refresh would be a no-op and would surface to the client as
/// a spurious auth_required teardown.
///
/// String fallbacks remain for tools that surface auth failures without
/// going through the structured `HttpFailure` path (e.g. JSON-only
/// `invalid_token` payloads, BYOK key-validation messages).
pub(super) fn is_auth_tool_error(err: &xai_tool_runtime::ToolError) -> bool {
    if let Some(details) = &err.details
        && let Some(status) = details
            .get(HTTP_STATUS_DETAILS_KEY)
            .and_then(|s| s.as_u64())
    {
        return status == 401;
    }
    let lower = err.to_string().to_ascii_lowercase();
    lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("invalid_token")
}
/// Gate inputs bundled with the composed decision so the 401-recovery log can
/// report the components.
#[derive(Clone, Copy)]
struct SessionTokenAuthGate {
    is_session_based: bool,
    model_byok: crate::agent::auth_method::ModelByok,
    /// Whether the request targets a first-party host. Lets an `Unknown`
    /// BYOK status still refresh against cli-chat-proxy / `*.x.ai` without
    /// risking a session-token leak to a third-party BYOK endpoint.
    endpoint_is_first_party: bool,
}
impl SessionTokenAuthGate {
    /// Single place `is_session_based` / `endpoint_is_first_party` are derived,
    /// so all call sites assemble the gate identically.
    fn new(
        auth_method_id: Option<&acp::AuthMethodId>,
        model_byok: crate::agent::auth_method::ModelByok,
        base_url: &str,
    ) -> Self {
        Self {
            is_session_based: auth_method_id
                .is_some_and(crate::agent::auth_method::is_session_based_method),
            model_byok,
            endpoint_is_first_party: crate::util::is_xai_api_url(base_url),
        }
    }
    fn active(self) -> bool {
        crate::agent::auth_method::session_token_auth_gate(
            self.is_session_based,
            self.model_byok,
            self.endpoint_is_first_party,
        )
    }
}
/// Run a tool call; on an auth-shaped failure, attempt recovery via
/// `AuthManager` and one retry. When `shared_recovery` is `Some`, concurrent
/// 401s in the same batch deduplicate via `OnceCell::get_or_init`.
pub(super) async fn call_with_auth_retry<F, Fut>(
    auth_manager: Option<&std::sync::Arc<crate::auth::AuthManager>>,
    shared_recovery: Option<&tokio::sync::OnceCell<bool>>,
    tool_name: &str,
    mut call: F,
) -> Result<xai_grok_tools::types::output::ToolRunResult, xai_tool_runtime::ToolError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<
            Output = Result<
                xai_grok_tools::types::output::ToolRunResult,
                xai_tool_runtime::ToolError,
            >,
        >,
{
    let result = call().await;
    let Err(ref err) = result else { return result };
    if !is_auth_tool_error(err) {
        return result;
    }
    let Some(am) = auth_manager else {
        return result;
    };
    let src = crate::auth::recovery::RecoverySource::Background;
    let recovered = match shared_recovery {
        Some(cell) => *cell.get_or_init(|| am.try_recover_unauthorized(src)).await,
        None => am.try_recover_unauthorized(src).await,
    };
    if recovered {
        tracing::info!(
            tool = tool_name,
            "auth recovery: tool 401, recovered, retrying"
        );
        call().await
    } else {
        tracing::warn!(tool = tool_name, "auth recovery: tool 401, refresh failed");
        xai_grok_telemetry::unified_log::warn(
            "auth recovery: tool 401, refresh failed",
            None,
            Some(serde_json::json!({ "tool" : tool_name })),
        );
        result
    }
}
impl SessionActor {
    pub(super) async fn prepare_tool_definitions_timed(&self) -> (Vec<ToolDefinition>, u64) {
        let mcp_wait_start = std::time::Instant::now();
        match self.mcp_strategy {
            McpInitStrategy::Blocking => {
                if !self.mcp_state.lock().await.is_initialized() {
                    tracing::info!(
                        "Blocking strategy: waiting for MCP initialization before first prompt..."
                    );
                    self.wait_for_mcp_initialized().await;
                }
            }
            McpInitStrategy::Progressive => {}
        }
        let mcp_wait_ms = mcp_wait_start.elapsed().as_millis() as u64;
        let defs = self.prepare_tool_definitions_inner().await;
        (defs, mcp_wait_ms)
    }
    pub(super) async fn prepare_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.prepare_tool_definitions_timed().await.0
    }
    /// The exact tool specs a turn sends, BEFORE the turn-specific
    /// structured-output append. Single source of truth shared by the turn
    /// (`acp_session_impl/turn.rs`) and the `SnapshotToolDefinitions` handler, so
    /// a verbatim-fork child's tool prefix can never silently drift from what the
    /// parent turn actually sends. `defs` is the already-resolved tool list
    /// (`prepare_tool_definitions_*`); this applies only the `web_search` drop
    /// under backend search and the `ToolSpec::from` mapping.
    pub(crate) fn turn_base_tool_specs(&self, defs: &[ToolDefinition]) -> Vec<ToolSpec> {
        let use_backend_search =
            self.agent.borrow().backend_search_enabled() && self.supports_backend_search.get();
        defs.iter()
            .filter(|td| !use_backend_search || td.function.name != "web_search")
            .cloned()
            .map(ToolSpec::from)
            .collect()
    }
    pub(super) async fn prepare_tool_definitions_inner(&self) -> Vec<ToolDefinition> {
        let bridge = self.agent.borrow().tool_bridge().clone();
        let defs = bridge.tool_definitions_builtins_only().await;
        let plan_active = self.plan_mode.lock().is_active();
        filter_cursor_tools_by_plan_mode(defs, plan_active)
    }
    pub(super) fn model_auth_facts(&self, model_id: &str) -> crate::agent::config::ModelAuthFacts {
        self.model_auth_state(model_id).0
    }
    pub(super) fn model_auth_provider(
        &self,
        model_id: &str,
    ) -> Option<crate::auth::AuthProviderRef> {
        self.model_auth_state(model_id).1
    }
    /// Drop the memoized per-model auth state; see [`Self::model_auth_memo`]
    /// for why each model/credential chokepoint must call this.
    pub(crate) fn invalidate_model_auth_memo(&self) {
        self.model_auth_memo.replace(None);
    }
    /// Auth facts from the live models-manager catalog, which includes
    /// platform `/models` live-synced entries (e.g. `ollama/glm-5.2`) that
    /// the offline-config resolver in
    /// [`crate::agent::config::resolve_model_auth_facts_and_provider`] cannot
    /// see — a miss there classifies the model `NotByok` and activates the
    /// xAI session-token gate for a third-party BYOK model. `None` when the
    /// catalog is empty or the model is absent (caller falls back to the
    /// offline resolver).
    fn live_catalog_auth_facts(
        &self,
        model_id: &str,
    ) -> Option<(
        crate::agent::config::ModelAuthFacts,
        Option<crate::auth::AuthProviderRef>,
    )> {
        use crate::agent::auth_method::ModelByok;
        if model_id.is_empty() {
            return None;
        }
        let models = self.models_manager.models();
        if models.is_empty() {
            return None;
        }
        let entry = crate::agent::config::find_model_by_id(&models, model_id)?;
        let facts = crate::agent::config::ModelAuthFacts {
            byok: if entry.has_own_credentials() {
                ModelByok::Byok
            } else {
                ModelByok::NotByok
            },
            auth_scheme: entry.info().auth_scheme,
        };
        let provider = entry.effective_auth_provider().cloned();
        Some((facts, provider))
    }
    /// Reads and populates [`Self::model_auth_memo`]; a fresh `Unknown`
    /// falls back to the last definite entry (see the field's contract).
    fn model_auth_state(
        &self,
        model_id: &str,
    ) -> (
        crate::agent::config::ModelAuthFacts,
        Option<crate::auth::AuthProviderRef>,
    ) {
        use crate::agent::auth_method::ModelByok;
        use crate::session::acp_session::ModelAuthMemo;
        if let Some(memo) = self.model_auth_memo.borrow().as_ref()
            && memo.model_id == model_id
            && memo.facts.byok != ModelByok::Unknown
        {
            return (memo.facts, memo.provider.clone());
        }
        let (fresh, provider) = self.live_catalog_auth_facts(model_id).unwrap_or_else(|| {
            crate::agent::config::resolve_model_auth_facts_and_provider(model_id)
        });
        if fresh.byok == ModelByok::Unknown {
            if let Some(memo) = self.model_auth_memo.borrow().as_ref()
                && memo.model_id == model_id
            {
                return (memo.facts, memo.provider.clone());
            }
            return (fresh, provider);
        }
        *self.model_auth_memo.borrow_mut() = Some(ModelAuthMemo {
            model_id: model_id.to_string(),
            facts: fresh,
            provider: provider.clone(),
        });
        (fresh, provider)
    }
    /// The single writer of a provider mint/rotation into chat-state credentials.
    async fn set_chat_api_key(&self, new_key: String) {
        let mut creds = self.chat_state_handle.get_credentials().await;
        creds.api_key = Some(new_key);
        self.chat_state_handle.update_credentials(creds);
    }
    /// Pre-turn arm for a provider-backed model: mint on a cold cache,
    /// re-mint near expiry, and adopt a rotation chat-state missed. No-op
    /// when `current_key` is already the fresh cached token.
    async fn refresh_provider_token_pre_turn(
        &self,
        provider: &crate::auth::AuthProviderRef,
        current_key: Option<&str>,
        model_id: &str,
    ) {
        match provider.ensure_fresh_token(current_key).await {
            crate::auth::ProviderRefreshOutcome::Rotated(new_key) => {
                tracing::info!(
                    model = % model_id, provider = % provider.name, cold = current_key
                    .is_none(), "auth provider token rotated pre-turn"
                );
                self.set_chat_api_key(new_key).await;
            }
            crate::auth::ProviderRefreshOutcome::Unchanged => {}
            crate::auth::ProviderRefreshOutcome::MintFailed => {
                tracing::warn!(
                    session_id = % self.session_info.id.0, provider = % provider.name,
                    model = % model_id, "auth provider pre-turn refresh failed"
                );
                xai_grok_telemetry::unified_log::warn(
                    "auth provider pre-turn refresh failed",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!(
                        { "provider" : provider.name, "model" : model_id, "cold" :
                        current_key.is_none(), }
                    )),
                );
            }
            crate::auth::ProviderRefreshOutcome::Unusable => {}
        }
    }
    /// 401 arm for a provider-backed model: re-run the helper once and
    /// resubmit. A missing key means the cold mint failed and the request
    /// went out unauthenticated, so mint instead. Returns `false` when the
    /// fresh-mint guard blocked the re-run or the helper failed; the 401
    /// then surfaces as a terminal error.
    async fn try_provider_401_recovery(&self, provider: &crate::auth::AuthProviderRef) -> bool {
        let rejected_key = self.chat_state_handle.get_credentials().await.api_key;
        let recovered = match rejected_key {
            Some(ref rejected_key) => provider.recover_rejected_token(rejected_key).await,
            None => provider.ensure_fresh_token(None).await.rotated(),
        };
        let Some(new_key) = recovered else {
            tracing::warn!(
                session_id = % self.session_info.id.0, provider = % provider.name,
                "auth recovery: sampler 401, provider re-mint declined or failed"
            );
            xai_grok_telemetry::unified_log::warn(
                "auth recovery: sampler 401, provider re-mint declined or failed",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({ "provider" : provider.name })),
            );
            return false;
        };
        tracing::info!(
            session_id = % self.session_info.id.0, provider = % provider.name,
            "auth recovery: sampler 401, auth provider re-mint, retrying"
        );
        xai_grok_telemetry::unified_log::info(
            "auth recovery: sampler 401, auth provider re-mint, retrying",
            Some(self.session_info.id.0.as_ref()),
            None,
        );
        self.set_chat_api_key(new_key).await;
        true
    }
    /// Gate inputs for `model_id` routed to `base_url`. See
    /// [`crate::agent::auth_method::session_token_auth_gate`] for the rationale
    /// (`base_url` keeps an `Unknown` BYOK status refreshable only
    /// against first-party xAI hosts).
    fn auth_gate(&self, model_id: &str, base_url: &str) -> SessionTokenAuthGate {
        let byok = self.model_auth_facts(model_id).byok;
        let auth_method = self.auth_method_id.load();
        SessionTokenAuthGate::new(auth_method.as_deref(), byok, base_url)
    }
    /// Emit a unified-log breadcrumb whenever the session-token refresh gate is
    /// evaluated with an **`Unknown`** per-model BYOK status on a session-based
    /// method — the condition that (pre-fix) silently demoted live sessions to
    /// stale-token 401s. The uploaded per-turn unified log then shows whether
    /// the first-party-endpoint fallback kept refresh active or withheld it, so
    /// we can confirm the fix works (or catch a residual demotion) per session
    /// even when server-side metrics only show the aggregate 401. No-op for a
    /// definite `Byok`/`NotByok`, so steady-state turns stay quiet — a burst of
    /// these is itself the signal that `Unknown` is being hit in the field.
    fn log_auth_gate_unknown(&self, site: &str, gate: SessionTokenAuthGate, base_url: &str) {
        use crate::agent::auth_method::ModelByok;
        if gate.model_byok != ModelByok::Unknown || !gate.is_session_based {
            return;
        }
        let refresh_active = gate.active();
        let ctx = serde_json::json!(
            { "site" : site, "model_byok" : gate.model_byok.as_str(), "is_session_based"
            : gate.is_session_based, "endpoint_is_first_party" : gate
            .endpoint_is_first_party, "refresh_active" : refresh_active, "base_url" :
            base_url, }
        );
        let sid = Some(self.session_info.id.0.as_ref());
        if refresh_active {
            xai_grok_telemetry::unified_log::info(
                "auth gate: Unknown BYOK on first-party endpoint — session-token refresh kept active",
                sid,
                Some(ctx),
            );
        } else {
            xai_grok_telemetry::unified_log::warn(
                "auth gate: Unknown BYOK on non-first-party endpoint — refresh withheld (may surface stale-token 401)",
                sid,
                Some(ctx),
            );
        }
    }
    /// Reconstruct a full `SamplerConfig` (with credentials) by combining
    /// the actor's `SamplingConfig` and `Credentials`. Folds in the
    /// URL-derived headers (cli-chat-proxy auth, the staging auth header)
    /// so the sampler crate stays URL-agnostic.
    pub(super) async fn reconstruct_full_config(&self) -> SamplingConfig {
        #[allow(clippy::items_after_statements)]
        #[derive(Debug)]
        struct SessionHeaderInjector {
            /// When true, refresh `chatgpt-account-id` from the live Codex
            /// credential so re-login mid-session cannot stick a stale account.
            codex_account: bool,
        }
        impl xai_grok_sampler::HeaderInjector for SessionHeaderInjector {
            fn inject(&self, headers: &mut reqwest::header::HeaderMap) {
                if let Some(tp) = xai_file_utils::trace_context::current_traceparent()
                    && let Ok(v) = reqwest::header::HeaderValue::from_str(&tp)
                {
                    headers.insert("traceparent", v);
                }
                if self.codex_account {
                    crate::auth::openai_codex::OpenAiCodexAccountHeaderInjector::apply(headers);
                }
            }
        }
        #[allow(clippy::items_after_statements)]
        struct AuthManagerBearerResolver(std::sync::Arc<crate::auth::AuthManager>);
        impl std::fmt::Debug for AuthManagerBearerResolver {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("AuthManagerBearerResolver").finish()
            }
        }
        impl xai_grok_sampler::BearerResolver for AuthManagerBearerResolver {
            fn current_bearer(&self) -> Option<String> {
                self.0.current_or_expired().map(|a| a.key)
            }
        }
        let cfg = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .unwrap_or_else(|| xai_grok_sampling_types::SamplingConfig {
                base_url: String::new(),
                model: String::new(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: Default::default(),
                extra_headers: Default::default(),
                context_window: std::num::NonZeroU64::new(256_000).unwrap(),
                reasoning_effort: None,
                stream_tool_calls: None,
            });
        let creds = self.chat_state_handle.get_credentials().await;
        let model_facts = self.model_auth_facts(cfg.model.as_str());
        let auth_method = self.auth_method_id.load();
        let gate =
            SessionTokenAuthGate::new(auth_method.as_deref(), model_facts.byok, &cfg.base_url);
        // A third-party BYOK platform endpoint must never carry the xAI
        // session bearer: a live-only catalog entry (e.g. `ollama/glm-5.2`
        // from the platform `/models` sync) is absent from the offline
        // catalog the BYOK gate consults, so it can classify `NotByok` and
        // activate the gate — the AuthManager resolver would then replace
        // the stamped platform key with the xAI session JWT and the
        // third-party host 401s (the false "recovery succeeded" loop).
        let open_platform = crate::agent::config::open_platform_endpoint(&cfg.base_url);
        let use_bearer_resolver = gate.active() && open_platform.is_none();
        self.log_auth_gate_unknown("reconstruct_full_config", gate, &cfg.base_url);
        let auth_scheme = model_facts.auth_scheme;
        // Capture before `cfg` fields are moved into `SamplingConfig`.
        let kimi_bearer_resolver =
            crate::agent::config::kimi_code_bearer_resolver_for_base_url(&cfg.base_url);
        let codex_bearer_resolver =
            crate::agent::config::openai_codex_bearer_resolver_for_base_url(&cfg.base_url);
        let responses_codex_dialect =
            xai_grok_models::PlatformId::OpenAiCodex.base_url_matches(&cfg.base_url);
        let kimi_dialect = xai_grok_models::PlatformId::KimiCode.base_url_matches(&cfg.base_url)
            || xai_grok_models::PlatformId::MoonshotCn.base_url_matches(&cfg.base_url)
            || xai_grok_models::PlatformId::MoonshotAi.base_url_matches(&cfg.base_url);
        let mut extra_headers = cfg.extra_headers;
        crate::agent::config::inject_url_derived_headers(
            &mut extra_headers,
            creds.alpha_test_key.as_deref(),
            &cfg.base_url,
        );
        let compaction_at_tokens = self.compaction_at_tokens.get();
        let compactions_remaining = self.compactions_remaining.get();
        if compactions_remaining.is_some() || compaction_at_tokens.is_some() {
            let has_compaction_summary = self
                .chat_state_handle
                .get_last_compaction_prompt_index()
                .await
                .is_some();
            if let Some(value) =
                compactions_remaining.and_then(|c| c.resolve(has_compaction_summary))
            {
                extra_headers.insert("x-compactions-remaining".to_string(), value.to_string());
            }
            if !has_compaction_summary
                && let Some(value) = compaction_at_tokens.and_then(|c| {
                    c.resolve(
                        cfg.context_window.get(),
                        self.compaction.threshold_percent.get(),
                    )
                })
            {
                extra_headers.insert("x-compaction-at".to_string(), value.to_string());
            }
        }
        // Platform OAuth (Kimi / OpenAI Codex) always wins over the xAI session
        // resolver. `session_token_auth_gate` is true whenever the user is on
        // cached_token/grok.com/oidc AND the model is NotByok — common for
        // openai-codex/* when the catalog stamp is not yet memoized as Byok.
        // Preferring AuthManager here previously sent the xAI OIDC JWT to
        // chatgpt.com → 401 "Could not parse your authentication token" while
        // recovery "succeeded" by refreshing the *wrong* token.
        let (api_key, bearer_resolver) = if responses_codex_dialect {
            (
                crate::auth::openai_codex::ensure_openai_codex_access_token_blocking(),
                codex_bearer_resolver,
            )
        } else if let Some(kimi) = kimi_bearer_resolver {
            (
                crate::auth::kimi::ensure_kimi_code_access_token_blocking(),
                Some(kimi),
            )
        } else if use_bearer_resolver {
            (
                creds.api_key,
                self.auth_manager
                    .as_ref()
                    .map(|am| -> xai_grok_sampler::SharedBearerResolver {
                        std::sync::Arc::new(AuthManagerBearerResolver(am.clone()))
                    }),
            )
        } else {
            (creds.api_key, None)
        };
        SamplingConfig {
            api_key,
            base_url: cfg.base_url,
            model: cfg.model,
            max_completion_tokens: cfg.max_completion_tokens,
            temperature: cfg.temperature,
            top_p: cfg.top_p,
            api_backend: cfg.api_backend,
            auth_scheme,
            extra_headers,
            context_window: cfg.context_window.get(),
            client_version: creds.client_version,
            reasoning_effort: cfg.reasoning_effort,
            force_http1: false,
            max_retries: Some(self.max_retries),
            stream_tool_calls: cfg.stream_tool_calls.unwrap_or(false),
            idle_timeout_secs: None,
            client_identifier: self.client_identifier.clone(),
            deployment_id: crate::managed_config::resolve_deployment_id(
                crate::managed_config::resolve_deployment_key().as_deref(),
            ),
            user_id: self
                .auth_manager
                .as_ref()
                .and_then(|am| am.current_or_expired())
                .filter(|a| a.is_xai_auth())
                .map(|a| a.user_id),
            origin_client: self.origin_client.clone(),
            attribution_callback: self.attribution_callback.clone(),
            bearer_resolver,
            supports_backend_search: self.supports_backend_search.get(),
            compactions_remaining: self.compactions_remaining.get(),
            compaction_at_tokens: self.compaction_at_tokens.get(),
            doom_loop_recovery: self.doom_loop_recovery,
            header_injector: Some(std::sync::Arc::new(SessionHeaderInjector {
                codex_account: responses_codex_dialect,
            })),
            responses_codex_dialect,
            kimi_dialect,
        }
    }
    /// Install auto-mode permission classifier with a live LLM side-query
    /// (laziness-classifier pattern: `prepare_chat_completion` +
    /// `conversation_collect` on a LocalSet task; channel bridges the
    /// `Send` permission actor). Heuristic runs only when the side-query
    /// errors or returns unparseable text.
    pub(crate) async fn wire_permission_auto_llm_classifier(self: &Arc<Self>) {
        if !self.permissions.is_auto_mode() {
            return;
        }
        if self.permissions.has_llm_side_query() {
            return;
        }
        let auto_cfg = crate::util::config::resolve_auto_mode_config_from_disk();
        let session_model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let aux_classifier_sampler = match auto_cfg.classifier_model.as_deref() {
            Some(slug) => self.resolve_auto_classifier_sampler(slug).await,
            None => None,
        };
        let models = self.models_manager.models();
        let effective_supports_re = crate::agent::config::effective_classifier_supports_re(
            aux_classifier_sampler
                .as_ref()
                .map(|(_, model)| model.as_str()),
            &session_model,
            &models,
        );
        let (prompt_type, classifier_reasoning_effort) =
            crate::util::config::auto_mode_classifier_defaults(&auto_cfg, effective_supports_re);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
            Vec<xai_grok_workspace::permission::ClassifierMessage>,
            tokio::sync::oneshot::Sender<Result<String, String>>,
        )>();
        let session = Arc::clone(self);
        tokio::task::spawn_local(async move {
            const TIMEOUT_MS: u64 = 15_000;
            while let Some((messages, respond_to)) = rx.recv().await {
                let result = async {
                    let (sampling_client, model) = match &aux_classifier_sampler {
                        Some((client, model)) => (client.clone(), model.clone()),
                        None => {
                            let client = session
                                .prepare_chat_completion(false)
                                .await
                                .map_err(|e| e.to_string())?;
                            let model = session
                                .chat_state_handle
                                .get_sampling_config()
                                .await
                                .map(|c| c.model)
                                .unwrap_or_default();
                            (client, model)
                        }
                    };
                    let session_id = session.session_info.id.to_string();
                    let items = messages
                        .into_iter()
                        .map(|m| match m.role {
                            xai_grok_workspace::permission::ClassifierMessageRole::System => {
                                ConversationItem::system(m.text)
                            }
                            xai_grok_workspace::permission::ClassifierMessageRole::User => {
                                ConversationItem::user(m.text)
                            }
                        })
                        .collect::<Vec<_>>();
                    let request = ConversationRequest {
                        items,
                        tools: vec![],
                        hosted_tools: vec![],
                        tool_choice: None,
                        model: Some(model),
                        temperature: None,
                        max_output_tokens: None,
                        json_schema: Some(
                            xai_grok_workspace::permission::classifier_output_json_schema(),
                        ),
                        reasoning_effort: classifier_reasoning_effort,
                        x_grok_conv_id: Some(session_id.clone()),
                        x_grok_req_id: Some(format!("xai-perm-auto-{}", uuid::Uuid::new_v4())),
                        x_grok_session_id: Some(session_id),
                        x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
                        ..ConversationRequest::default()
                    };
                    let fut = sampling_client.conversation_collect(request);
                    let response =
                        tokio::time::timeout(std::time::Duration::from_millis(TIMEOUT_MS), fut)
                            .await
                            .map_err(|_| "permission auto classifier timed out".to_string())?
                            .map_err(|e| e.to_string())?;
                    Ok(response.assistant_text())
                }
                .await;
                let _ = respond_to.send(result);
            }
        });
        let clf =
            xai_grok_workspace::permission::LlmPermissionClassifier::with_channel(tx, prompt_type);
        debug_assert!(
            clf.has_side_query(),
            "channel-wired classifier must report has_side_query"
        );
        self.permissions.set_classifier_with_side_query(clf, true);
        tracing::info!(
            session_id = % self.session_info.id,
            "Wired live LLM permission auto-mode classifier (session sampling channel)"
        );
    }
    /// Resolve a standalone aux-model `SamplerConfig` for `slug` via the shared
    /// catalog routing (Tier-1 catalog creds / Tier-2 xAI-proxy via session token
    /// / `XAI_API_KEY` / deployment key), gathering the session-local auth context
    /// once. Shared by image-describe and the classifier so the gather can't
    /// drift. `None` ⇒ caller falls back to the session model.
    pub(super) async fn resolve_aux_sampler_config(
        &self,
        slug: &str,
    ) -> Option<xai_grok_sampler::SamplerConfig> {
        let creds = self.chat_state_handle.get_credentials().await;
        let session_key = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired().map(|a| a.key.clone()));
        let models = self.models_manager.models();
        let endpoints = self.models_manager.endpoints();
        let disable_api_key_auth = self
            .auth_manager
            .as_ref()
            .map(|am| am.grok_com_config().api_key_auth_disabled())
            .unwrap_or(false);
        crate::agent::config::resolve_aux_model_sampling_config(
            slug,
            &models,
            &endpoints,
            session_key.as_deref(),
            disable_api_key_auth,
            creds.alpha_test_key.clone(),
            creds.client_version.clone(),
        )
    }
    /// Resolve a dedicated sampler for the Auto-mode classifier model `slug`,
    /// stamping session-local auth/attribution like image-describe (which relies
    /// on the resolver, not a config override, for `base_url`/`api_backend` so
    /// credentials stay consistent). `None` ⇒ caller falls back to the session
    /// client + model.
    async fn resolve_auto_classifier_sampler(
        &self,
        slug: &str,
    ) -> Option<(xai_grok_sampler::SamplingClient, String)> {
        let active_session_config = self.reconstruct_full_config().await;
        let mut cfg = self.resolve_aux_sampler_config(slug).await?;
        crate::agent::config::stamp_session_local_sampler_fields(
            &mut cfg,
            &active_session_config,
            self.client_identifier.clone(),
            Some(self.max_retries),
        );
        let model = cfg.model.clone();
        let client = xai_grok_sampler::SamplingClient::new(cfg)
            .map_err(|e| {
                tracing::warn!(
                    error = % e,
                    "auto classifier aux sampler build failed; using session model"
                )
            })
            .ok()?;
        Some((client, model))
    }
    #[tracing::instrument(
        name = "session.prepare_chat_completion",
        skip_all,
        fields(force_http1)
    )]
    pub(super) async fn prepare_chat_completion(
        &self,
        force_http1: bool,
    ) -> Result<xai_grok_sampler::SamplingClient, acp::Error> {
        self.refresh_token_if_expired().await;
        let mut full_config = self.reconstruct_full_config().await;
        full_config.force_http1 = force_http1;
        let sampling_client =
            xai_grok_sampler::SamplingClient::new(full_config).map_err(|e| self.to_acp_error(e))?;
        Ok(sampling_client)
    }
    /// Push a fresh `SamplerConfig` into the per-session sampler actor
    /// before each turn. Mirrors `prepare_chat_completion`'s
    /// auth-refresh + config rebuild, but routes the result to the
    /// `xai-grok-sampler` instead of constructing a new
    /// `OaiCompatClient`.
    ///
    /// Behaviour parity: we run the same `refresh_token_if_expired()`
    /// and `reconstruct_full_config()` so the sampler picks up any
    /// newly issued session token. The previous client cache inside
    /// the sampler actor is invalidated automatically by
    /// `update_config`.
    pub(crate) async fn prepare_sampler_for_turn(&self) {
        self.refresh_token_if_expired().await;
        let mut sampler_config = self.reconstruct_full_config().await;
        sampler_config.idle_timeout_secs = Some(self.inference_idle_timeout.as_secs());
        self.sampler_handle.update_config(sampler_config);
    }
    fn log_terminal_failure(&self, error_type: &str, status_code: Option<u16>, message: &str) {
        let auth = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired());
        let reauthable = is_reauthable_failure(Some(error_type), message);
        xai_grok_telemetry::unified_log::warn(
            "turn.terminal_failure",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!(
                { "error_type" : error_type, "status_code" : status_code,
                "reauthable" : reauthable, "auth_mode" : auth.as_ref().map(| a |
                format!("{:?}", a.auth_mode)), "key_prefix" : auth.as_ref().map(| a |
                crate ::auth::token_suffix(& a.key).to_owned()), "expires_at" : auth
                .as_ref().and_then(| a | a.expires_at.map(| e | e.to_rfc3339())),
                "message" : crate ::util::truncate(message, 300), }
            )),
        );
    }
    pub(crate) async fn handle_sampling_failure(
        self: &Arc<Self>,
        error: xai_grok_sampler::SamplingErrorInfo,
    ) -> Result<SamplerFailureRecovery, acp::Error> {
        use xai_grok_sampler::SamplingErrorKind;
        if self.should_compact_on_error(&error).await {
            let cw = error
                .model_metadata
                .as_ref()
                .and_then(|m| m.context_window)
                .expect("should_compact_on_error guarantees context_window");
            {
                let total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
                let percentage = xai_token_estimation::usage_percentage_u8(total_tokens, cw);
                if let Some(mut cfg) = self.chat_state_handle.get_sampling_config().await
                    && let Some(new_cw) = std::num::NonZeroU64::new(cw)
                    && self.compaction.context_window_override.is_none()
                {
                    cfg.context_window = new_cw;
                    self.chat_state_handle.update_sampling_config(cfg);
                }
                let trigger_info = compaction::AutoCompactTriggerInfo {
                    tokens_used: total_tokens,
                    context_window: cw,
                    percentage,
                };
                self.run_compact_only(trigger_info).await?;
                return Ok(SamplerFailureRecovery::CompactAndResubmit);
            }
        }
        let mut detailed_message = error.message.clone();
        if matches!(error.kind, SamplingErrorKind::Api)
            && error.status_code == Some(400)
            && error.message.contains("encrypted_content")
        {
            self.signals_handle()
                .record_error_typed("encrypted_content_mismatch");
            let friendly = "This session's conversation history is incompatible \
                            with the current model. Please start a new session."
                .to_string();
            self.log_terminal_failure("encrypted_content_mismatch", error.status_code, &friendly);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Failed {
                    error_type: "encrypted_content_mismatch".to_string(),
                    message: friendly.clone(),
                },
            ))
            .await;
            return Err(acp::Error::invalid_params().data(friendly));
        }
        if matches!(error.kind, SamplingErrorKind::RateLimited) {
            self.log_terminal_failure("rate_limited", error.status_code, &detailed_message);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Exhausted {
                    attempts: 0,
                    reason: detailed_message.clone(),
                    is_rate_limited: true,
                },
            ))
            .await;
            let acp_err = acp::Error::new(
                crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                "Rate limited".to_string(),
            )
            .data(detailed_message);
            return Err(acp_err);
        }
        let (failed_model_id, failed_base_url) = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| (c.model, c.base_url))
            .unwrap_or_default();
        let auth_provider =
            if matches!(error.kind, SamplingErrorKind::Auth) || error.status_code == Some(401) {
                self.model_auth_provider(&failed_model_id)
            } else {
                None
            };
        let auth_recovery_eligible = matches!(error.kind, SamplingErrorKind::Auth) && {
            let gate = self.auth_gate(&failed_model_id, &failed_base_url);
            let eligible = gate.active();
            self.log_auth_gate_unknown("handle_sampling_failure", gate, &failed_base_url);
            if !eligible && auth_provider.is_none() {
                tracing::warn!(
                    session_id = % self.session_info.id.0, is_session_based = gate
                    .is_session_based, model_byok = gate.model_byok.as_str(),
                    endpoint_is_first_party = gate.endpoint_is_first_party,
                    "auth recovery: sampler 401 not refreshable (api-key auth) — surfacing 401",
                );
                xai_grok_telemetry::unified_log::warn(
                    "auth recovery: sampler 401 not eligible (api-key auth)",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!(
                        { "kind" : error.kind.as_str(), "status_code" : error
                        .status_code, "is_session_based" : gate.is_session_based,
                        "model_byok" : gate.model_byok.as_str(),
                        "endpoint_is_first_party" : gate.endpoint_is_first_party, }
                    )),
                );
            }
            eligible
        };
        debug_assert!(
            !(auth_recovery_eligible && auth_provider.is_some()),
            "a provider-backed model must not be session-recovery-eligible"
        );
        if !matches!(error.kind, SamplingErrorKind::Auth)
            && error.status_code == Some(401)
            && auth_provider.is_none()
        {
            xai_grok_telemetry::unified_log::warn(
                "auth recovery: sampler 401 not eligible (non-auth error kind)",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!(
                    { "kind" : error.kind.as_str(), "status_code" : error
                    .status_code, }
                )),
            );
        }
        // Third-party OAuth platforms must never recover via xAI AuthManager —
        // that refreshes the wrong credential and reports "recovery succeeded"
        // while chatgpt.com/kimi still 401.
        let is_openai_codex =
            xai_grok_models::PlatformId::OpenAiCodex.base_url_matches(&failed_base_url);
        let is_kimi_code =
            xai_grok_models::PlatformId::KimiCode.base_url_matches(&failed_base_url);
        if (matches!(error.kind, SamplingErrorKind::Auth) || error.status_code == Some(401))
            && is_openai_codex
        {
            match crate::auth::openai_codex::force_refresh_openai_codex_auth().await {
                Some(_) => {
                    tracing::info!(
                        session_id = % self.session_info.id.0,
                        "auth recovery: sampler 401, openai-codex re-mint, retrying"
                    );
                    xai_grok_telemetry::unified_log::info(
                        "auth recovery: sampler 401, openai-codex re-mint, retrying",
                        Some(self.session_info.id.0.as_ref()),
                        None,
                    );
                    self.prepare_sampler_for_turn().await;
                    return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit);
                }
                None => {
                    tracing::warn!(
                        session_id = % self.session_info.id.0,
                        "auth recovery: sampler 401, openai-codex re-mint failed"
                    );
                    xai_grok_telemetry::unified_log::warn(
                        "auth recovery: sampler 401, openai-codex re-mint failed",
                        Some(self.session_info.id.0.as_ref()),
                        None,
                    );
                }
            }
        } else if (matches!(error.kind, SamplingErrorKind::Auth)
            || error.status_code == Some(401))
            && is_kimi_code
        {
            // Do not fall through to xAI AuthManager recovery (wrong credential).
            // Force a Kimi token refresh mirroring the openai-codex path; only
            // when that fails do we surface the 401 so the user can
            // `/login kimi` rather than looping a false recovery.
            match crate::auth::kimi::force_refresh_kimi_code_auth().await {
                Some(_) => {
                    tracing::info!(
                        session_id = % self.session_info.id.0,
                        "auth recovery: sampler 401, kimi-code re-mint, retrying"
                    );
                    xai_grok_telemetry::unified_log::info(
                        "auth recovery: sampler 401, kimi-code re-mint, retrying",
                        Some(self.session_info.id.0.as_ref()),
                        None,
                    );
                    self.prepare_sampler_for_turn().await;
                    return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit);
                }
                None => {
                    tracing::warn!(
                        session_id = % self.session_info.id.0,
                        "auth recovery: sampler 401, kimi-code re-mint failed"
                    );
                    xai_grok_telemetry::unified_log::warn(
                        "auth recovery: sampler 401, kimi-code re-mint failed",
                        Some(self.session_info.id.0.as_ref()),
                        None,
                    );
                }
            }
        } else if (matches!(error.kind, SamplingErrorKind::Auth) || error.status_code == Some(401))
            && let Some(platform) = crate::agent::config::open_platform_endpoint(&failed_base_url)
        {
            // Same wrong-credential trap as Kimi/Codex: refreshing the xAI
            // session cannot fix a third-party BYOK 401 — the retry re-sends
            // a credential the platform must reject again, looping to the
            // misleading "Auth recovery succeeded but ... 401" terminal
            // error. Surface the 401 with the platform's setup hint so the
            // user re-checks the platform key instead.
            tracing::warn!(
                session_id = % self.session_info.id.0,
                platform = platform.as_str(),
                "auth recovery: sampler 401 on open-platform endpoint — \
                 not refreshable via xAI session"
            );
            xai_grok_telemetry::unified_log::warn(
                "auth recovery: sampler 401 on open-platform endpoint — not refreshable via xAI session",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({ "platform" : platform.as_str() })),
            );
            detailed_message = format!(
                "{detailed_message}\n\n\
                 The request was rejected by {} — check the platform API key: {}.",
                platform.display_name(),
                platform.setup_hint()
            );
        } else if auth_recovery_eligible
            && crate::auth::devbox_login::is_devbox_environment()
            && let Some(ref am) = self.auth_manager
        {
            match am.try_devbox_recovery().await {
                Ok(auth) => {
                    tracing::info!(
                        session_id = % self.session_info.id.0, user_id = % auth.user_id,
                        "auth recovery: sampler 401, devbox re-mint, retrying"
                    );
                    self.prepare_sampler_for_turn().await;
                    return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit);
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = % self.session_info.id.0, error = % e,
                        "auth recovery: sampler 401, devbox re-mint failed"
                    );
                    xai_grok_telemetry::unified_log::warn(
                        "auth recovery: sampler 401, devbox re-mint failed",
                        Some(self.session_info.id.0.as_ref()),
                        Some(serde_json::json!({ "error" : format!("{e}") })),
                    );
                }
            }
        } else if auth_recovery_eligible && let Some(ref am) = self.auth_manager {
            if am
                .try_recover_unauthorized(crate::auth::recovery::RecoverySource::Turn)
                .await
            {
                tracing::info!(
                    session_id = % self.session_info.id.0,
                    "auth recovery: sampler 401, recovered, retrying"
                );
                xai_grok_telemetry::unified_log::info(
                    "auth recovery: sampler 401, recovered, retrying",
                    Some(self.session_info.id.0.as_ref()),
                    None,
                );
                self.prepare_sampler_for_turn().await;
                return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit);
            }
            tracing::warn!(
                session_id = % self.session_info.id.0,
                "auth recovery: sampler 401, refresh failed"
            );
            xai_grok_telemetry::unified_log::warn(
                "auth recovery: sampler 401, refresh failed",
                Some(self.session_info.id.0.as_ref()),
                None,
            );
        }
        if let Some(ref provider) = auth_provider
            && self.try_provider_401_recovery(provider).await
        {
            self.prepare_sampler_for_turn().await;
            return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit);
        }
        if matches!(error.kind, SamplingErrorKind::IdleTimeout) {
            self.signals_handle().record_idle_timeout();
        }
        if matches!(error.kind, SamplingErrorKind::EmptyResponse) {
            if let Some(ref ctx) = error.empty_response_context {
                tracing::warn!(
                    empty_response = true, empty_reason = ctx.reason.as_str(),
                    had_reasoning = ctx.had_reasoning, content_len = ctx.content_len,
                    tool_call_count = ctx.tool_call_count, completion_tokens = ctx
                    .completion_tokens.unwrap_or(0), reasoning_tokens = ctx
                    .reasoning_tokens.unwrap_or(0), finish_reason = ctx
                    .finish_reason_str(), first_choice_seen = ctx.first_choice_seen,
                    model = % ctx.model,
                    "empty response after retries exhausted: {reason}", reason = ctx
                    .reason,
                );
                {
                    let mut cap = self.streaming_turn_capture.lock();
                    cap.reasoning_tokens = ctx.reasoning_tokens;
                    cap.completion_tokens = ctx.completion_tokens;
                    cap.finish_reason = ctx.finish_reason.clone();
                    cap.empty_reason = Some(ctx.reason.as_str().to_owned());
                }
            }
            self.signals_handle().record_error_typed("empty_response");
        }
        let auth_mode = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current())
            .map(|a| a.auth_mode)
            .unwrap_or(crate::auth::AuthMode::ApiKey);
        let auth_mode_str = format!("{auth_mode:?}");
        let client_version = xai_grok_version::VERSION;
        if auth_mode == crate::auth::AuthMode::WebLogin {
            let msg = format!(
                "{detailed_message}\n\n\
                 You are using a deprecated authentication method (WebLogin).\n\
                 This auth method is no longer supported and will cause errors.\n\n\
                 To fix: run `grok logout` then `grok login` to re-authenticate with OAuth2.\n\n\
                 Version: {client_version}"
            );
            self.log_terminal_failure("legacy_auth", error.status_code, &msg);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Failed {
                    error_type: "legacy_auth".to_string(),
                    message: msg.clone(),
                },
            ))
            .await;
            return Err(acp::Error::internal_error().data(msg));
        }
        let is_model_404 =
            error.status_code == Some(404) && detailed_message.contains("does not exist");
        let is_auth_401 =
            error.status_code == Some(401) || matches!(error.kind, SamplingErrorKind::Auth);
        let detailed_message = if is_model_404 || is_auth_401 {
            let current_model = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_else(|| "unknown".to_string());
            let available: Vec<String> = self
                .models_manager
                .models()
                .values()
                .map(|m| m.model.clone())
                .collect();
            let mut msg = format!("{detailed_message}\n");
            msg.push_str(&format!("\n  Model:     {current_model}"));
            msg.push_str(&format!("\n  Auth:      {auth_mode_str}"));
            if let Some(ref provider) = auth_provider {
                msg.push_str(
                    &format!(
                        "\n  Provider:  [auth_provider.{}] (check the provider command and the debug log)",
                        provider.name
                    ),
                );
            }
            msg.push_str(&format!("\n  Version:   {client_version}"));
            if available.is_empty() {
                msg.push_str("\n  Available: (none)");
            } else {
                msg.push_str(&format!("\n  Available: {}", available.join(", ")));
            }
            if is_model_404 && !available.iter().any(|m| m == &current_model) {
                msg.push_str(&format!(
                    "\n\n  '{}' is not in your available models.",
                    current_model
                ));
                msg.push_str("\n  Switch models with /model or start a new session.");
            }
            msg
        } else {
            detailed_message
        };
        let error_type = if xai_grok_sampling_types::is_context_length_error(&error.message) {
            "context_length"
        } else {
            error.kind.as_str()
        };
        self.log_terminal_failure(error_type, error.status_code, &detailed_message);
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: error_type.to_string(),
                message: detailed_message.clone(),
            },
        ))
        .await;
        Err(
            acp::Error::internal_error().data(crate::sampling::error::terminal_error_data(
                detailed_message,
                error.status_code,
                error.kind,
            )),
        )
    }
    /// Drive a single turn through the sampler-based path.
    ///
    /// Calls `prepare_sampler_for_turn` first (auth refresh + config
    /// push), then submits via `SamplerHandle::submit_and_collect` and
    /// returns:
    /// * `Ok(SamplerTurnOutcome::Response(_))` - model responded.
    /// * `Ok(SamplerTurnOutcome::CompactAndResubmit)` - compaction
    ///    ran, the outer turn loop should `continue`.
    /// * `Ok(SamplerTurnOutcome::RefreshAuthAndResubmit)` - auth 401
    ///    recovery succeeded, credentials refreshed, retry once.
    /// * `Err(acp::Error)` - terminal failure already reported via
    ///    `send_xai_notification(RetryState::Failed)`.
    pub(crate) async fn run_turn_via_sampler(
        self: &Arc<Self>,
        request: ConversationRequest,
    ) -> Result<SamplerTurnOutcome, acp::Error> {
        self.prepare_sampler_for_turn().await;
        let stream_drained_rx = {
            let (tx, rx) = tokio::sync::oneshot::channel();
            *self.turn_stream_drained.lock() = Some(tx);
            rx
        };
        let request_id = xai_grok_sampler::RequestId::random();
        let request_id_str = request_id.as_str().to_string();
        match self
            .sampler_handle
            .submit_and_collect(request_id, request)
            .await
        {
            Ok((response, metrics)) => {
                let span = tracing::Span::current();
                span.record("request_id", request_id_str.as_str());
                if let Some(ttft) = metrics.time_to_first_token_ms {
                    span.record("ttft_ms", ttft as i64);
                }
                if metrics.attempts > 0 {
                    span.record("attempt", i64::from(metrics.attempts));
                }
                if tokio::time::timeout(std::time::Duration::from_secs(5), stream_drained_rx)
                    .await
                    .is_err()
                {
                    self.turn_stream_drained.lock().take();
                    tracing::warn!(
                        "stream-drain barrier timed out; proceeding to emit tool \
                         calls (eventId ordering may be imperfect this turn)"
                    );
                }
                Ok(SamplerTurnOutcome::Response(
                    Box::new(response),
                    Box::new(metrics),
                ))
            }
            Err(rich_err) => {
                self.turn_stream_drained.lock().take();
                let info = xai_grok_sampler::SamplingErrorInfo::from(&rich_err);
                match self.handle_sampling_failure(info).await? {
                    SamplerFailureRecovery::CompactAndResubmit => {
                        Ok(SamplerTurnOutcome::CompactAndResubmit)
                    }
                    SamplerFailureRecovery::RefreshAuthAndResubmit => {
                        Ok(SamplerTurnOutcome::RefreshAuthAndResubmit)
                    }
                }
            }
        }
    }
    /// Proactively refresh the auth token if near expiry.
    ///
    /// Session-token path is best-effort: on success, update credentials and
    /// return. On failure, do **not** fall through to the JWT/config.toml
    /// branch when the session gate was active — that path is for BYOK JWTs
    /// only. Falling through after a failed session refresh left hard-expired
    /// opaque tokens (External/OIDC) on the wire and guaranteed a 401.
    /// Soft failures with a still-usable access token still return here
    /// (grace / optimistic send); 401 recovery remains the safety net.
    pub(super) async fn refresh_token_if_expired(&self) {
        if let Some(ref am) = self.auth_manager {
            let creds = self.chat_state_handle.get_credentials().await;
            let (model_id, base_url) = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| (c.model, c.base_url))
                .unwrap_or_default();
            // Never refresh-then-rewrite chat-state credentials with the xAI
            // session token for a third-party BYOK platform endpoint: the
            // stamped platform key is the correct signer, and the BYOK gate
            // can misclassify live-only catalog models as `NotByok`.
            if self.auth_gate(&model_id, &base_url).active()
                && crate::agent::config::open_platform_endpoint(&base_url).is_none()
            {
                match am.get_valid_token().await {
                    Ok(key) => {
                        if creds.api_key.as_deref() != Some(&key) {
                            let mut creds = creds;
                            creds.api_key = Some(key);
                            self.chat_state_handle.update_credentials(creds);
                        }
                        return;
                    }
                    Err(e) => {
                        let hard_expired = !am.has_usable_token();
                        tracing::warn!(
                            error = % e, hard_expired, model = % model_id,
                            "auth: preflight get_valid_token failed"
                        );
                        xai_grok_telemetry::unified_log::warn(
                            "auth.preflight.refresh_failed",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!(
                                { "error" : format!("{e}"), "hard_expired" : hard_expired,
                                "model" : model_id, }
                            )),
                        );
                        return;
                    }
                }
            }
        } else {
            xai_grok_telemetry::unified_log::debug(
                "token refresh skipped: no auth manager",
                Some(self.session_info.id.0.as_ref()),
                None,
            );
        }
        use crate::auth::{is_jwt_expired_or_near, parse_jwt_expiration};
        const REFRESH_THRESHOLD: chrono::Duration = chrono::Duration::minutes(5);
        let creds = self.chat_state_handle.get_credentials().await;
        let current_key = creds.api_key;
        let current_model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        if let Some(provider) = self.model_auth_provider(&current_model_id) {
            self.refresh_provider_token_pre_turn(
                &provider,
                current_key.as_deref(),
                &current_model_id,
            )
            .await;
            return;
        }
        let Some(ref key) = current_key else { return };
        if !is_jwt_expired_or_near(key, REFRESH_THRESHOLD) {
            if let Some(exp) = parse_jwt_expiration(key) {
                let remaining_secs = (exp - chrono::Utc::now()).num_seconds();
                tracing::debug!(
                    model = % current_model_id, remaining_secs,
                    "JWT token valid, no refresh needed"
                );
            } else {
                tracing::debug!(
                    model = % current_model_id, key_len = key.len(),
                    "Token is not a JWT, expiry-based refresh not applicable"
                );
            }
            return;
        }
        let remaining_secs =
            parse_jwt_expiration(key).map_or(0, |exp| (exp - chrono::Utc::now()).num_seconds());
        tracing::info!(
            model = % current_model_id, remaining_secs,
            "JWT near expiry, refreshing from config.toml"
        );
        let Some(new_key) = self.reload_api_key_from_config(&current_model_id) else {
            return;
        };
        if key == &new_key {
            tracing::warn!(
                model = % current_model_id,
                "Config.toml returned same token (not yet rotated by external process?)"
            );
            return;
        }
        let new_remaining_secs = parse_jwt_expiration(&new_key)
            .map_or(0, |exp| (exp - chrono::Utc::now()).num_seconds());
        tracing::info!(
            model = % current_model_id, new_remaining_secs, key_len = new_key.len(),
            "Refreshed API token from config.toml"
        );
        let mut creds = self.chat_state_handle.get_credentials().await;
        creds.api_key = Some(new_key);
        self.chat_state_handle.update_credentials(creds);
    }
    fn reload_api_key_from_config(&self, current_model_id: &str) -> Option<String> {
        let raw_config = crate::config::load_effective_config()
            .map_err(|e| tracing::warn!(error = % e, "Failed to reload config"))
            .ok()?;
        let config = crate::agent::config::Config::new_from_toml_cfg(&raw_config)
            .map_err(|e| tracing::warn!(error = % e, "Failed to parse reloaded config.toml"))
            .ok()?;
        let config_model = config
            .config_models
            .iter()
            .find(|(k, v)| v.model.as_deref().unwrap_or(k.as_str()) == current_model_id)
            .map(|(_, v)| v);
        let Some(model) = config_model else {
            tracing::warn!(
                model = % current_model_id, available = ? config.config_models.keys()
                .collect::< Vec < _ >> (), "Model not found in config.toml [model.*]"
            );
            return None;
        };
        let key = crate::agent::config::first_own_credential(
            model.api_key.as_deref(),
            model.env_key.as_ref(),
        );
        if key.is_none() {
            tracing::warn!(
                model = % current_model_id, env_key = ? model.env_key,
                "No api_key or env_key resolved for model"
            );
        }
        key
    }
    /// Propagate the model-reported token usage from a turn response into
    /// chat state, the per-prompt usage ledger, and per-turn signals.
    ///
    /// This is the only place per-turn `total_tokens` is refreshed in the
    /// post-sampler-refactor path; without it `state.total_tokens` would
    /// stay frozen at the `estimate_conversation_tokens` seed from
    /// `ChatState::new`, freezing `/context` and corrupting the resume
    /// restore that reads `meta.totalTokens` from `updates.jsonl`.
    /// Resetting `estimated_tokens_since_model = 0` here also keeps the
    /// preflight-overflow guard accurate against the next turn's
    /// tool-result deltas.
    pub(crate) fn record_response_token_usage(
        &self,
        response: &ConversationResponse,
        api_duration_ms: Option<u64>,
    ) {
        if let Some(ref u) = response.usage {
            self.chat_state_handle
                .record_token_usage(u64::from(u.total_tokens));
            self.chat_state_handle.record_last_turn_usage(u.clone());
            self.chat_state_handle.record_model_call_usage(
                response.assistant().and_then(|a| a.model_id.clone()),
                u.clone(),
                api_duration_ms,
                response.cost_usd_ticks,
            );
            self.signals_handle()
                .record_token_usage(u.completion_tokens, u.reasoning_tokens);
        }
    }
    pub(super) async fn record_assistant_response(&self, assistant_item: ConversationItem) {
        self.signals_handle().record_assistant_message();
        if let ConversationItem::Assistant(ref a) = assistant_item {
            tracing::info!(
                model_id = ? a.model_id, "DEBUG record_assistant_response model_id"
            );
        }
        if let ConversationItem::Assistant(ref a) = assistant_item
            && let Some(first_call) = a.tool_calls.first()
        {
            tracing::info!("Assistant requested tool call: {}", first_call.id);
        }
        self.chat_state_handle
            .push_assistant_response(assistant_item);
    }
}
