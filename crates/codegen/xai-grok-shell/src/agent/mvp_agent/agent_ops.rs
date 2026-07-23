#![cfg_attr(rustfmt, rustfmt::skip)]
#![allow(unused_imports)]
//! Inherent [`MvpAgent`] helpers (MCP/clients/gateway, settings/models, session ops, spawn).
//! Co-located child of `mvp_agent` (`use super::*`).
use super::*;
/// `preferred` model, else catalog `current`, else first with own credentials.
fn byok_from_models(
    models: &indexmap::IndexMap<String, ModelEntry>,
    preferred: Option<&str>,
    current: &str,
) -> Option<String> {
    preferred
        .and_then(|id| models.get(id))
        .and_then(|m| m.own_credential())
        .or_else(|| models.get(current).and_then(|m| m.own_credential()))
        .or_else(|| models.values().find_map(|m| m.own_credential()))
}
impl MvpAgent {
    pub fn reload_skills_all_sessions(&self) -> usize {
        let session_ids: Vec<agent_client_protocol::SessionId> = self
            .sessions
            .borrow()
            .keys()
            .cloned()
            .collect();
        for sid in &session_ids {
            if let Some(handle) = self.sessions.borrow().get(sid).cloned() {
                let _ = handle.cmd_tx.send(SessionCommand::ReloadSkills);
            }
        }
        session_ids.len()
    }
    pub fn advertise_commands_all_sessions(&self) -> usize {
        let session_ids: Vec<agent_client_protocol::SessionId> = self
            .sessions
            .borrow()
            .keys()
            .cloned()
            .collect();
        for session_id in &session_ids {
            if let Some(handle) = self.sessions.borrow().get(session_id).cloned() {
                let _ = handle.cmd_tx.send(SessionCommand::AdvertiseCommands);
            }
        }
        session_ids.len()
    }
    pub(super) fn resolve_image_description_model(&self) -> String {
        self.cfg
            .borrow()
            .image_description_model
            .as_deref()
            .unwrap_or(crate::models::default_image_description_model())
            .to_owned()
    }
    fn resolve_session_summary_model(&self) -> String {
        self.cfg
            .borrow()
            .session_summary_model
            .as_deref()
            .unwrap_or(crate::models::default_session_summary_model())
            .to_owned()
    }
    pub(super) fn build_summary_client(
        &self,
        primary: &SamplingConfig,
    ) -> Result<(OaiCompatClient, String), acp::Error> {
        let slug = self.resolve_session_summary_model();
        let session_key = self.auth_manager.current_or_expired().map(|a| a.key.clone());
        let models = self.models_manager.models();
        let endpoints = self.models_manager.endpoints();
        let (disable_api_key_auth, alpha_test_key, client_version) = {
            let cfg = self.cfg.borrow();
            (
                cfg.grok_com_config.api_key_auth_disabled(),
                cfg.endpoints.alpha_test_key.clone(),
                cfg.client_version.clone(),
            )
        };
        let config = match crate::agent::config::resolve_aux_model_sampling_config(
            &slug,
            &models,
            &endpoints,
            session_key.as_deref(),
            disable_api_key_auth,
            alpha_test_key,
            client_version,
        ) {
            Some(mut cfg) => {
                crate::agent::config::stamp_session_local_sampler_fields(
                    &mut cfg,
                    primary,
                    primary.client_identifier.clone(),
                    primary.max_retries,
                );
                cfg
            }
            None => {
                let mut fallback = primary.clone();
                fallback.model = slug;
                fallback
            }
        };
        let model = config.model.clone();
        let client = OaiCompatClient::new(config).map_err(map_sampling_err_to_acp)?;
        Ok((client, model))
    }
    fn has_proxy_credentials(&self) -> bool {
        self.cfg.borrow().endpoints.deployment_key.is_some()
            || self.auth_manager.current_or_expired().is_some_and(|a| a.is_xai_auth())
    }
    /// `true` for session-based ACP auth methods.
    fn is_session_based_auth(&self) -> bool {
        self.auth_method_id
            .load()
            .as_deref()
            .is_some_and(crate::agent::auth_method::is_session_based_method)
    }
    /// Publish the current ACP auth method into the shared live handle so every
    /// running session's per-turn auth gate observes it on its next turn.
    pub(super) fn set_auth_method(&self, id: acp::AuthMethodId) {
        self.auth_method_id.store(Some(std::sync::Arc::new(id)));
    }
    /// Publish model-owned credentials for voice/tools static fallthrough.
    /// Only [`ModelEntry::own_credential`] — not `sampling_config.api_key` (may be a session JWT).
    pub(crate) fn sync_process_static_api_key(&self, preferred_model_id: Option<&str>) {
        if self.cfg.borrow().grok_com_config.api_key_auth_disabled() {
            self.auth_manager.set_process_static_api_key(None);
            return;
        }
        let models = self.models_manager.models();
        let current = self.models_manager.current_model_id();
        self.auth_manager
            .set_process_static_api_key(
                byok_from_models(&models, preferred_model_id, current.0.as_ref()),
            );
    }
    /// Return auth for sync config construction.
    pub(super) fn current_or_buffered_auth(&self) -> Option<crate::auth::GrokAuth> {
        self.auth_manager
            .current()
            .or_else(|| {
                if self.is_session_based_auth() {
                    let auth = self.auth_manager.expired_auth();
                    if auth.is_some() {
                        xai_grok_telemetry::unified_log::info(
                            "auth buffered token fallback",
                            None,
                            None,
                        );
                    }
                    auth
                } else {
                    None
                }
            })
    }
    fn has_managed_mcp_auth(&self) -> bool {
        self.auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_managed_mcp_eligible())
    }
    /// Requires feature flag AND xAI authentication (OIDC or legacy WebLogin).
    pub(super) fn can_fetch_managed_mcps(&self) -> bool {
        let cfg = self.cfg.borrow();
        cfg.managed_mcps_enabled && !cfg.managed_mcp_gateway_tools_enabled
            && self.has_managed_mcp_auth()
    }
    fn can_fetch_managed_mcp_gateway_tools(&self) -> bool {
        self.cfg.borrow().managed_mcp_gateway_tools_enabled
            && self.has_managed_mcp_auth()
    }
    pub async fn get_managed_mcp_configs(
        &self,
    ) -> Vec<crate::session::managed_mcp::ManagedMcpConfig> {
        if !self.can_fetch_managed_mcps() {
            return vec![];
        }
        let proxy_url = self.cfg.borrow().endpoints.proxy_url();
        crate::session::managed_mcp::fetch_managed_mcp_configs(
                &self.managed_mcp_cache,
                &proxy_url,
                &self.auth_manager,
            )
            .await
    }
    pub async fn get_managed_mcp_gateway_tool_catalog(
        &self,
    ) -> Option<crate::session::managed_mcp::GatewayToolCatalog> {
        if !self.can_fetch_managed_mcp_gateway_tools() {
            self.managed_mcp_cache.lock().await.disable_gateway_tools();
            return None;
        }
        self.managed_mcp_cache.lock().await.enable_gateway_tools();
        let proxy_url = self.cfg.borrow().endpoints.proxy_url();
        let auth_key = self
            .auth_manager
            .get_valid_token()
            .await
            .ok()
            .or_else(|| self.auth_manager.current_or_expired().map(|a| a.key));
        crate::session::managed_mcp::get_or_fetch_gateway_tool_catalog(
                &self.managed_mcp_cache,
                &proxy_url,
                auth_key.as_deref(),
            )
            .await
    }
    pub fn managed_mcp_cache(
        &self,
    ) -> &crate::session::managed_mcp::ManagedMcpStateHandle {
        &self.managed_mcp_cache
    }
    pub(crate) fn disable_managed_gateway_tools_and_refresh_sessions(&self) {
        self.disable_managed_gateway_tools_and_refresh_sessions_with_txs(
            self.sessions.borrow().values().map(|handle| handle.cmd_tx.clone()).collect(),
        );
    }
    fn disable_managed_gateway_tools_and_refresh_sessions_with_txs(
        &self,
        session_txs: Vec<tokio::sync::mpsc::UnboundedSender<SessionCommand>>,
    ) {
        let cache = self.managed_mcp_cache.clone();
        tokio::task::spawn_local(async move {
            cache.lock().await.disable_gateway_tools();
            for tx in session_txs {
                let _ = tx.send(SessionCommand::RefreshMcpSearchIndex);
            }
        });
    }
    pub(crate) fn spawn_managed_gateway_tool_catalog_fetch(&self) {
        let session_txs: Vec<_> = self
            .sessions
            .borrow()
            .values()
            .map(|handle| handle.cmd_tx.clone())
            .collect();
        if !self.can_fetch_managed_mcp_gateway_tools() {
            self.disable_managed_gateway_tools_and_refresh_sessions_with_txs(
                session_txs,
            );
            return;
        }
        let cache = self.managed_mcp_cache.clone();
        let proxy_url = self.cfg.borrow().endpoints.proxy_url();
        let auth_manager = self.auth_manager.clone();
        tokio::task::spawn_local(async move {
            let auth_key = auth_manager
                .get_valid_token()
                .await
                .ok()
                .or_else(|| auth_manager.current_or_expired().map(|a| a.key));
            if !auth_manager
                .current_or_expired()
                .is_some_and(|a| a.is_managed_mcp_eligible())
            {
                cache.lock().await.disable_gateway_tools();
                for tx in session_txs {
                    let _ = tx.send(SessionCommand::RefreshMcpSearchIndex);
                }
                return;
            }
            cache.lock().await.enable_gateway_tools();
            crate::session::managed_mcp::get_or_fetch_gateway_tool_catalog(
                    &cache,
                    &proxy_url,
                    auth_key.as_deref(),
                )
                .await;
            for tx in session_txs {
                let _ = tx.send(SessionCommand::RefreshMcpSearchIndex);
            }
        });
    }
    /// Resolve the launch dir's project-scope trust verdict ONCE and return it
    /// with its path.
    ///
    /// Memoizes the single [`folder_trust::resolve_launch_dir_trust`] gather (see
    /// it for the dedup + TOCTOU contract) so the two one-shot init helpers
    /// (`ensure_plugin_registry` and `ensure_local_workspace_ops`) share it
    /// instead of each re-scanning. They share a single point-in-time verdict
    /// rather than two independent re-scans; the sub-millisecond, startup-only
    /// window between them is intentional (the cross-session TOCTOU re-scan is
    /// preserved per the contract).
    fn prime_launch_dir_trust(&self) -> (&std::path::Path, bool) {
        let trust = *self
            .launch_dir_trust
            .get_or_init(|| {
                let remote_settings = self.cfg.borrow().remote_settings.clone();
                folder_trust::resolve_launch_dir_trust(
                    &self.launch_cwd,
                    remote_settings.as_ref(),
                )
            });
        (&self.launch_cwd, trust)
    }
    /// Resolve folder trust and load launch-dir MCP configs after `initialize`
    /// returns. The walks are synchronous and expensive in large monorepos; they
    /// must not block the ACP response (grok-desktop sends `initialize` immediately).
    pub(super) fn spawn_initialize_launch_mcp_setup(&self, fetch_managed_mcps: bool) {
        let cwd = self.launch_cwd.clone();
        let compat = self.cfg.borrow().compat_resolved;
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        let gateway = self.gateway.clone();
        let agent_mcp_state = self.agent_mcp_state.clone();
        let managed_mcp_cache = self.managed_mcp_cache.clone();
        let proxy_url = self.cfg.borrow().endpoints.proxy_url();
        let auth_manager = self.auth_manager.clone();
        tokio::task::spawn_local(async move {
            let local_mcp_servers = match tokio::task::spawn_blocking(move || {
                    let local = crate::util::config::load_mcp_servers(&cwd, &compat);
                    folder_trust::resolve_and_record(
                        &cwd,
                        remote_settings.as_ref(),
                        false,
                    );
                    folder_trust::filter_untrusted_project_mcp(&cwd, local)
                })
                .await
            {
                Ok(servers) => servers,
                Err(e) => {
                    tracing::warn!(error = %e, "initialize MCP setup task failed");
                    return;
                }
            };
            if !local_mcp_servers.is_empty() {
                agent_mcp_state.lock().await.update_configs(local_mcp_servers.clone());
            }
            crate::extensions::mcp::notify_servers_updated(
                    &gateway,
                    &[],
                    &local_mcp_servers,
                )
                .await;
            if !fetch_managed_mcps {
                return;
            }
            let managed = crate::session::managed_mcp::fetch_managed_mcp_configs(
                    &managed_mcp_cache,
                    &proxy_url,
                    &auth_manager,
                )
                .await;
            if !managed.is_empty() {
                crate::extensions::mcp::notify_servers_updated(
                        &gateway,
                        &managed,
                        &local_mcp_servers,
                    )
                    .await;
            }
        });
    }
    pub fn agent_mcp_state(
        &self,
    ) -> std::sync::Arc<tokio::sync::Mutex<crate::session::mcp_servers::McpState>> {
        self.agent_mcp_state.clone()
    }
    /// Build the launch-dir plugin registry snapshot on first use.
    ///
    /// Boot-time discovery was deferred past ACP `initialize` (the cwd→git-root
    /// plus user/marketplace walks stalled grok-desktop's first `initialize`),
    /// leaving `plugin_registry_handle` empty. That shared snapshot still backs
    /// the launch-dir plugin MCP/LSP merges read in `resolve_mcp_servers` and
    /// the session LSP build, so populate it lazily — off the `initialize`
    /// critical path — on the first session-creating call. Runs the discovery
    /// walk once; per-session `build_for_cwd` still re-resolves project-scoped
    /// plugins for each session's own cwd.
    pub(super) fn ensure_plugin_registry(&self) {
        if self.plugin_registry_initialized.replace(true) {
            return;
        }
        let (cwd, trusted) = self.prime_launch_dir_trust();
        let mut plugins = self.cfg.borrow().plugins.clone();
        plugins.merge_claude_enabled_plugins(Some(cwd));
        let disk_config = plugins.to_discovery_config();
        let count = self
            .plugin_registry_handle
            .reload(Some(cwd), &disk_config, trusted, false);
        tracing::debug!(
            plugin_count = count,
            "lazily populated plugin registry snapshot"
        );
    }
    /// Fetch managed configs, merge with client servers, return merged list + earliest expiry.
    pub(super) async fn resolve_mcp_servers(
        &self,
        client_servers: Vec<acp::McpServer>,
        cwd: &std::path::Path,
    ) -> (Vec<acp::McpServer>, Option<chrono::DateTime<chrono::Utc>>) {
        self.ensure_plugin_registry();
        let managed = self.get_managed_mcp_configs().await;
        let expires_at = managed.iter().filter_map(|c| c.token_expires_at).min();
        let merged = crate::session::managed_mcp::merge_managed_mcp_servers(
            client_servers,
            cwd,
            &managed,
            self.plugin_registry_handle.snapshot().as_deref(),
            &self.cfg.borrow().compat_resolved,
        );
        (merged, expires_at)
    }
    /// Set the memory configuration (called from TUI after config resolution).
    pub fn set_memory_config(&mut self, config: crate::config::MemoryConfig) {
        self.memory_config = if config.enabled { Some(config) } else { None };
    }
    /// Adopt the leader's [`AgentActivity`] so the auto-update checker sees
    /// the agent's live view of running turns/subagents and can flush
    /// sessions at shutdown.
    ///
    /// Must be called right after construction: entries registered on the
    /// constructor-created default instance are NOT migrated.
    pub fn set_activity(&mut self, activity: crate::agent::activity::AgentActivity) {
        self.subagent_coordinator
            .borrow_mut()
            .set_running_gauge(activity.subagent_gauge());
        self.activity = activity;
    }
    /// Install the channel that fans new session cwds into the leader's
    /// `ConfigFileWatcher::watch_path`. Called once after
    /// the watcher is constructed in `agent/app.rs`. In simple /
    /// non-leader mode the channel is never wired and
    /// `notify_session_cwd_for_watch` is a no-op.
    pub fn set_config_watcher_path_tx(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<std::path::PathBuf>,
    ) {
        self.config_watcher_path_tx = Some(tx);
    }
    /// Best-effort fan-out of a new session's `cwd` to the leader's
    /// `ConfigFileWatcher` for dynamic non-recursive registration
    /// No-op if the channel was never installed
    /// (`set_config_watcher_path_tx` was not called — simple mode,
    /// tests) or if the receiver has been dropped. Watcher errors are
    /// logged inside the spawned task and do NOT propagate here.
    pub(crate) fn notify_session_cwd_for_watch(&self, cwd: &std::path::Path) {
        if let Some(tx) = self.config_watcher_path_tx.as_ref()
            && tx.send(cwd.to_path_buf()).is_err()
        {
            tracing::debug!(
                cwd = %cwd.display(),
                "config watcher path channel closed; session cwd not registered"
            );
        }
    }
    /// Extract feedback credentials when proxy credentials are available.
    ///
    /// Returns `(base_url, user_token, optional_extra_access_key, deployment_key)`.
    /// Used by both [`feedback_client`] and session spawning to avoid
    /// duplicating the credential assembly logic.
    #[allow(clippy::type_complexity)]
    fn feedback_credentials(
        &self,
    ) -> Option<(String, Option<String>, Option<String>, Option<String>)> {
        if !self.has_proxy_credentials() {
            return None;
        }
        let user_token = self
            .auth_manager
            .current_or_expired()
            .filter(|a| a.is_xai_auth())
            .map(|a| a.key.clone());
        let cfg = self.cfg.borrow();
        let base_url = cfg.endpoints.resolve_feedback_base_url();
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let deployment_key = cfg.endpoints.deployment_key.clone();
        Some((base_url, user_token, alpha_test_key, deployment_key))
    }
    pub(super) fn ensure_telemetry_client(&self) {
        crate::auth::credential_provider::sync_external_otel_identity();
        let cfg = self.cfg.borrow();
        let mode = cfg.resolve_telemetry_mode().value;
        if !mode.is_disabled() {
            let Some(auth) = self
                .auth_manager
                .current()
                .filter(|a| {
                    a.is_xai_auth() || a.auth_mode == crate::auth::AuthMode::ApiKey
                }) else {
                return;
            };
            let subscription_tier = resolve_subscription_tier_for_telemetry(
                cfg
                    .remote_settings
                    .as_ref()
                    .and_then(|rs| rs.subscription_tier_display.clone()),
                Some(&auth),
            );
            let (user_id, team_id) = if auth.is_xai_auth() {
                (Some(auth.user_id), auth.team_id)
            } else {
                (None, auth.team_id)
            };
            xai_grok_telemetry::client::init_if_needed(
                cfg.telemetry.clone(),
                mode,
                user_id,
                team_id,
                cfg.endpoints.deployment_key.clone(),
                self.origin_client_info_from_meta(None),
                xai_grok_version::VERSION.to_owned(),
                subscription_tier,
                crate::http::shared_client(),
            );
        }
    }
    /// Build a `FeedbackClient` with resolved feedback URL and credentials.
    pub(crate) fn feedback_client(&self) -> Option<FeedbackClient> {
        let (base_url, user_token, alpha_test_key, deployment_key) = self
            .feedback_credentials()?;
        Some(
            FeedbackClient::new(base_url, user_token)
                .with_alpha_test_key(alpha_test_key)
                .with_deployment_key(deployment_key)
                .with_auth_manager(self.auth_manager.clone()),
        )
    }
    /// Build a `RegistryConfig` if the feature is enabled (for passing to persistence actor).
    pub(super) fn build_registry_config(
        &self,
    ) -> Option<crate::session::RegistryConfig> {
        let remote = self
            .cfg
            .borrow()
            .remote_settings
            .as_ref()
            .and_then(|s| s.session_registry_enabled);
        if !self.session_registry_local.or(remote).unwrap_or(false) {
            return None;
        }
        let auth = self.auth_manager.current_or_expired()?;
        if !auth.is_xai_auth() {
            return None;
        }
        let key = auth.key.clone();
        let cfg = self.cfg.borrow();
        Some(crate::session::RegistryConfig {
            base_url: cfg.endpoints.proxy_url(),
            user_token: key,
            deployment_key: cfg.endpoints.deployment_key.clone(),
            alpha_test_key: cfg.endpoints.alpha_test_key.clone(),
        })
    }
    /// Build a `SessionRegistryClient` if the feature is enabled.
    /// Delegates to `build_registry_config()` for the enabled check + config.
    pub(crate) fn session_registry_client(
        &self,
    ) -> Option<crate::agent::session_registry_client::SessionRegistryClient> {
        let cfg = self.build_registry_config()?;
        Some(
            crate::agent::session_registry_client::SessionRegistryClient::new(
                    cfg.base_url,
                    cfg.user_token,
                )
                .with_deployment_key(cfg.deployment_key)
                .with_alpha_test_key(cfg.alpha_test_key)
                .with_auth(self.auth_manager.clone()),
        )
    }
    pub(crate) fn conversations_client(
        &self,
    ) -> Option<crate::remote::ConversationsClient> {
        if !crate::session::unified_list::conversations_lane_active() {
            return None;
        }
        Some(crate::remote::ConversationsClient::new(self.auth_manager.clone()))
    }
    pub(crate) fn workspaces_client(&self) -> crate::remote::WorkspacesClient {
        crate::remote::WorkspacesClient::new(self.auth_manager.clone())
    }
    /// Pre-session command availability snapshot.
    ///
    /// Used by the `x.ai/commands/list` ext method and the
    /// `InitializeResponse._meta` path (`builtin_commands()`), both of
    /// which fire before any session exists. The eventual agent's toolset
    /// is unknown (depends on the model the user picks), so we fail-closed
    /// for runtime/tool-dependent gates (`/flush`, `/loop`, `/memory`,
    /// …) and let the session-scoped `available_commands_update` in
    /// `acp_session.rs` fill in the real per-model gating as soon as a
    /// session starts.
    ///
    /// otherwise it wouldn't appear in the slash menu until after the
    pub(crate) fn command_availability(
        &self,
    ) -> crate::session::slash_commands::CommandAvailability {
        crate::session::slash_commands::CommandAvailability {
            goal: self.cfg.borrow().resolve_goal().value,
            workflows: self.cfg.borrow().resolve_workflows().value,
            ..crate::session::slash_commands::CommandAvailability::default()
        }
    }
    /// `true` when data collection should be suppressed (team ZDR or
    /// coding-data-retention opt-out). Delegates to
    /// [`AuthManager::is_data_collection_disabled`].
    pub(crate) fn is_data_collection_disabled(&self) -> bool {
        self.auth_manager.is_data_collection_disabled()
    }
    /// Telemetry enabled and not ZDR. Same gate as session `telemetry_enabled`.
    pub(crate) fn product_analytics_enabled(&self) -> bool {
        self.cfg.borrow().is_telemetry_enabled()
            && !self.auth_manager.current_or_expired().is_some_and(|a| a.is_zdr_team())
    }
    /// Re-sync the `Send` mirror of `cfg.is_trace_upload_enabled()` that the
    /// per-session collection gates read (`cfg` is `!Send`; the gates run on
    /// the tokio pool). Must be called after any mid-session config change
    /// that can flip the switch — i.e. every `remote_settings` rewrite.
    pub(super) fn sync_collection_config_gate(&self) {
        self.trace_upload_live
            .store(
                self.cfg.borrow().is_trace_upload_enabled(),
                std::sync::atomic::Ordering::Relaxed,
            );
    }
    /// Current client type as set by the most recent `initialize()` call.
    pub(crate) fn client_type(&self) -> ClientType {
        *self.client_type.borrow()
    }
    /// Most recently allocated turn number for `sid`, or `None` if the
    /// session has not started a turn yet.
    pub(crate) fn session_turn_number(&self, sid: &acp::SessionId) -> Option<u64> {
        self.session_turn_numbers.borrow().get(sid).copied()
    }
    /// Return the current GrokAuth credentials, if authenticated and not expired.
    pub(crate) fn current_auth(&self) -> Option<crate::auth::GrokAuth> {
        self.auth_manager.current()
    }
    /// Shared plugin registry handle used by extensions for snapshot/reload.
    pub(crate) fn plugin_registry_handle(
        &self,
    ) -> &xai_grok_agent::plugins::SharedPluginRegistryHandle {
        &self.plugin_registry_handle
    }
    /// `true` when the agent runs in writeback storage mode.
    pub(crate) fn is_writeback_storage(&self) -> bool {
        matches!(self.storage_mode, StorageMode::Writeback)
    }
    /// Resolved cli-chat-proxy base for session features (via
    /// `proxy_url`). Not for the deployment-config fetch.
    pub(crate) fn cli_chat_proxy_base_url(&self) -> String {
        self.cfg.borrow().endpoints.proxy_url()
    }
    pub(crate) fn alpha_test_key(&self) -> Option<String> {
        self.cfg.borrow().endpoints.alpha_test_key.clone()
    }
    /// Build the process-lifetime local `WorkspaceOps` on first use.
    ///
    /// Deferred past ACP wiring so `initialize` can respond before folder-trust
    /// scans and `WorkspaceHandle::new_minimal` run (same boot stall as plugin
    /// discovery on grok-desktop Windows).
    fn ensure_local_workspace_ops(
        &self,
    ) -> Result<xai_grok_workspace::WorkspaceOps, acp::Error> {
        if let Some(ops) = self.workspace_ops.borrow().clone() {
            return Ok(ops);
        }
        let (cwd, project_lsp_trusted) = self.prime_launch_dir_trust();
        let workspace_identity = self
            .auth_manager
            .current_or_expired()
            .map(|a| match a.team_id.filter(|t| !t.is_empty()) {
                Some(team) => {
                    xai_grok_workspace::WorkspaceIdentity::team(a.user_id, team)
                }
                None => {
                    xai_grok_workspace::WorkspaceIdentity::new(
                        a.user_id,
                        a.principal_type,
                        a.principal_id,
                    )
                }
            })
            .unwrap_or_default();
        let ops = match xai_grok_workspace::handle::WorkspaceHandle::new_minimal(
            cwd.to_path_buf(),
            workspace_identity,
            project_lsp_trusted,
        ) {
            Ok(handle) => xai_grok_workspace::WorkspaceOps::local(handle),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "failed to create local WorkspaceHandle"
                );
                return Err(
                    acp::Error::internal_error().data("workspace not initialized"),
                );
            }
        };
        *self.workspace_ops.borrow_mut() = Some(ops.clone());
        Ok(ops)
    }
    /// Resolve the workspace ops, returning `Err` if not yet initialized.
    ///
    /// Only `None` before the first lazy local build via
    /// [`Self::ensure_local_workspace_ops`]. Called at the `ext_method`
    /// dispatch boundary and in session spawn; extensions receive the
    /// resolved `&WorkspaceOps` directly.
    pub(crate) fn resolve_workspace_ops(
        &self,
    ) -> Result<xai_grok_workspace::WorkspaceOps, acp::Error> {
        let ops = self.ensure_local_workspace_ops()?;
        if let Some(handle) = ops.workspace_handle() && !handle.has_client_ext_sink() {
            let gw = self.gateway.clone();
            handle
                .set_client_ext_sink(
                    std::sync::Arc::new(move |method: String, params: serde_json::Value| {
                        if let Ok(raw) = serde_json::value::to_raw_value(&params) {
                            gw.forward_fire_and_forget(
                                acp::ExtNotification::new(method, raw.into()),
                            );
                        }
                    }),
                );
        }
        Ok(ops)
    }
    /// Derive the current `AuthType` from auth method + auth manager state.
    ///
    /// Conceptually, `AuthType` describes *which authentication mechanism this
    /// session uses*, not *whether we currently have a live bearer*. Bearer
    /// liveness is tracked by the auth manager; the mechanism is fixed by
    /// `auth_method_id`.
    ///
    /// Returns `SessionToken` when EITHER:
    ///   - `auth_manager` currently has a live (non-expired) credential, OR
    ///   - the active auth method is session-based (`cached_token`,
    ///     `grok.com`, `oidc`) -- even if the in-memory token is currently
    ///     expired or missing.
    ///
    /// Returns `ApiKey` only when the auth method is BYOK (`xai.api_key`) or
    ///   no auth method has been selected yet AND no live credential exists.
    ///
    /// The session-based clause is load-bearing: without it, chat_state can get
    /// locked into `auth_type = ApiKey` and skip token refresh on later prompts.
    pub(crate) fn auth_type(&self) -> xai_chat_state::AuthType {
        if self.auth_manager.current().is_some() || self.is_session_based_auth() {
            xai_chat_state::AuthType::SessionToken
        } else {
            xai_chat_state::AuthType::ApiKey
        }
    }
    /// When `cached_token` cannot proceed, prefer non-interactive `xai.api_key`
    /// iff `should_advertise_xai_api_key`; otherwise `grok.com`. Returns `None`
    /// when `preferred_method` is pinned (fail-closed — no cross-method fallthrough).
    pub(super) fn cached_token_fallthrough_method_id(
        &self,
    ) -> Option<acp::AuthMethodId> {
        let preferred = self.cfg.borrow().grok_com_config.preferred_method;
        let id = auth_method::method_id_after_cached_token_unavailable(
            auth_method::should_advertise_xai_api_key(
                self.cfg.borrow().grok_com_config.api_key_auth_disabled(),
                self.models_manager.models().values(),
            ),
            preferred,
        )?;
        Some(acp::AuthMethodId::new(id))
    }
    /// Shared exit for missing/expired/legacy `cached_token`: fall through with
    /// `use_oauth` only when the target is interactive `grok.com`. When
    /// `preferred_method` is pinned, fail instead of falling through.
    pub(super) async fn authenticate_after_cached_token_unavailable(
        &self,
        arguments: acp::AuthenticateRequest,
    ) -> Result<AuthenticateResponse, acp::Error> {
        let Some(method_id) = self.cached_token_fallthrough_method_id() else {
            let preferred = self.cfg.borrow().grok_com_config.preferred_method;
            let msg = match preferred {
                Some(crate::auth::PreferredAuthMethod::ApiKey) => {
                    auth_method::PREFERRED_API_KEY_UNAVAILABLE
                }
                _ => auth_method::PREFERRED_OIDC_UNAVAILABLE,
            };
            tracing::info!(%msg, "cached_token unavailable; preferred_method forbids fallthrough");
            xai_grok_telemetry::unified_log::warn(
                "auth cached_token fallthrough blocked by preferred_method",
                None,
                Some(
                    serde_json::json!({
                    "preferred_method": preferred.map(|p| format!("{p:?}")),
                }),
                ),
            );
            return Err(acp::Error::auth_required().data(msg));
        };
        let meta = if method_id.0.as_ref() == auth_method::GROK_COM_METHOD_ID {
            serde_json::json!({ "use_oauth": true }).as_object().cloned()
        } else {
            arguments.meta
        };
        tracing::info!(fallback = %method_id.0, "cached_token fallthrough");
        xai_grok_telemetry::unified_log::warn(
            "auth cached_token fallthrough",
            None,
            Some(serde_json::json!({ "fallback": method_id.0.as_ref() })),
        );
        acp::Agent::authenticate(
                self,
                acp::AuthenticateRequest::new(method_id).meta(meta),
            )
            .await
    }
    pub(crate) fn deployment_key(&self) -> Option<String> {
        self.cfg.borrow().endpoints.deployment_key.clone()
    }
    /// Re-fetch remote settings and re-init the telemetry client.
    ///
    /// Called unconditionally from both auth handlers so that:
    /// - First install / expired OIDC token: settings are fetched for
    ///   the first time (the early prefetch had no auth to use).
    /// - Reauth / account switch: settings are refreshed to reflect
    ///   the new user's remote settings targeting attributes.
    ///
    /// This only refreshes `cfg.remote_settings` and re-inits the
    /// telemetry client (the only global static). Other settings
    /// derived from `remote_settings` (`is_trace_upload_enabled`,
    /// `web_fetch_enabled`, etc.) are resolved lazily per-turn from
    /// `cfg` and pick up the new values automatically.
    /// Agent-level fields materialised at startup (`worktree_type`,
    /// `restore_code`) are NOT re-resolved here; that requires a
    /// broader refactor of the init path.
    pub(super) async fn refresh_remote_settings(&self, auth: &crate::auth::GrokAuth) {
        if !crate::util::config::resolve_remote_fetch_enabled() {
            tracing::debug!("post-auth settings refresh skipped: remote_fetch disabled");
            return;
        }
        let is_xai = auth.is_xai_auth();
        let user_id = auth.user_id.clone();
        let team_id = auth.team_id.clone();
        let remote_was_absent = self.cfg.borrow().remote_settings.is_none();
        let Some(settings) = self.fetch_remote_settings(auth.clone()).await else {
            tracing::warn!("post-auth settings refresh failed (HTTP or parse error)");
            return;
        };
        tracing::info!("post-auth settings refreshed");
        let (
            telemetry_config,
            telemetry_mode,
            grok_user_id,
            grok_team_id,
            deployment_key,
            subscription_tier,
        ) = {
            let mut cfg = self.cfg.borrow_mut();
            cfg.remote_settings = Some(settings);
            crate::util::config::sync_campaign_fields(&mut cfg);
            crate::agent::config::apply_remote_settings_side_effects(
                cfg.remote_settings.as_ref(),
            );
            let telemetry_mode = cfg.resolve_telemetry_mode();
            let trace_upload = cfg.resolve_trace_upload();
            tracing::info!(
                telemetry = %telemetry_mode,
                trace_upload = %trace_upload,
                "post-auth data capture config re-resolved",
            );
            let grok_user_id = is_xai.then(|| user_id.clone());
            let grok_team_id = is_xai.then(|| team_id.clone()).flatten();
            let telemetry_config = cfg.telemetry.clone();
            let deployment_key = cfg.endpoints.deployment_key.clone();
            let subscription_tier_display = cfg
                .remote_settings
                .as_ref()
                .and_then(|rs| rs.subscription_tier_display.clone());
            (
                telemetry_config,
                telemetry_mode.value,
                grok_user_id,
                grok_team_id,
                deployment_key,
                subscription_tier_display,
            )
        };
        self.sync_collection_config_gate();
        let subscription_tier = resolve_subscription_tier_for_telemetry(
            subscription_tier,
            self.auth_manager.current_or_expired().as_ref(),
        );
        xai_grok_telemetry::client::init(
            telemetry_config,
            telemetry_mode,
            grok_user_id,
            grok_team_id,
            deployment_key,
            self.origin_client_info_from_meta(None),
            xai_grok_version::VERSION.to_owned(),
            subscription_tier,
            crate::http::shared_client(),
        );
        crate::auth::credential_provider::sync_external_otel_identity();
        self.emit_announcements(AnnouncementsPushMode::IfChanged);
        self.reconfigure_heap_profile_monitor();
        if remote_was_absent {
            self.spawn_auto_worktree_gc();
        }
    }
    /// Refresh remote settings settings and re-resolve eagerly-resolved config fields.
    ///
    /// Called on `/new` session creation so feature flags reflect the latest
    /// remote settings state without requiring a TUI restart. Extends
    /// [`refresh_remote_settings`] by also re-running [`resolve_runtime_fields`]
    /// with the fresh settings.
    ///
    /// In-flight sessions are unaffected — they snapshot config at creation.
    pub(super) async fn refresh_settings_and_reapply(
        &self,
        auth: &crate::auth::GrokAuth,
    ) {
        self.refresh_remote_settings(auth).await;
        {
            let mut cfg = self.cfg.borrow_mut();
            crate::util::config::sync_campaign_fields(&mut cfg);
            let raw_config = crate::config::load_effective_config()
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "config reload failed during settings refresh");
                    toml::Value::Table(toml::map::Map::new())
                });
            cfg.re_resolve_runtime_fields(&raw_config);
        }
        self.sync_collection_config_gate();
        self.emit_settings_update_notification();
        self.emit_announcements(AnnouncementsPushMode::Force);
        self.reconfigure_heap_profile_monitor();
    }
    /// Spawn the periodic remote-settings poll that pushes mid-session
    /// announcement changes to connected clients. Idempotent; plain loop (no
    /// cancellation) like `ensure_session_supervisor` — the LocalSet drop at
    /// process exit ends it. Skipped under `cfg!(test)` like the
    /// managed-config sync (PTY e2e runs the real binary and is unaffected).
    pub(super) fn spawn_announcements_refresh(&self) {
        if cfg!(test) || self.announcements_refresh_started.replace(true) {
            return;
        }
        let agent_ref = LocalRef::new(self);
        tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(announcements_refresh_interval());
            interval.tick().await;
            loop {
                interval.tick().await;
                let result = futures::FutureExt::catch_unwind(
                        std::panic::AssertUnwindSafe(
                            agent_ref.get().poll_announcements_refresh_once(),
                        ),
                    )
                    .await;
                if result.is_err() {
                    tracing::error!("announcements refresh tick panicked; continuing");
                }
            }
        });
    }
    /// One poll cycle. With no settings baseline, first population is
    /// delegated to the sanctioned fill-if-missing path (which emits on
    /// success); otherwise refresh the stored announcements best-effort, then
    /// run the emit gate — even when the fetch was skipped or failed, so a
    /// pure expiry crossing still clears client banners on time.
    async fn poll_announcements_refresh_once(&self) {
        if self.cfg.borrow().remote_settings.is_none() {
            self.maybe_fetch_post_auth_settings().await;
            return;
        }
        self.fetch_and_store_polled_announcements().await;
        self.emit_announcements(AnnouncementsPushMode::IfChanged);
    }
    /// Fetch half of a poll cycle: fresh settings from the proxy, then the
    /// announcements-only apply. Every failure path is a silent skip — the
    /// next tick retries.
    async fn fetch_and_store_polled_announcements(&self) {
        let Ok(auth) = self.auth_manager.auth().await else {
            tracing::debug!("announcements refresh skipped: not authenticated");
            return;
        };
        let pre_fetch = self
            .cfg
            .borrow()
            .remote_settings
            .as_ref()
            .and_then(|s| s.announcements.clone());
        let Some(settings) = self.fetch_remote_settings(auth).await else {
            tracing::debug!("announcements refresh skipped: settings fetch failed");
            return;
        };
        self.apply_polled_announcements(settings, pre_fetch);
    }
    /// Store the polled announcements unless another writer (full refresh /
    /// paywall unblock) landed mid-fetch — then this fetch is stale and the
    /// next tick reconciles. Emission is `emit_announcements`'s job, not
    /// this store's.
    pub(super) fn apply_polled_announcements(
        &self,
        fresh: crate::util::config::RemoteSettings,
        pre_fetch: Option<Vec<xai_grok_announcements::RemoteAnnouncement>>,
    ) {
        let mut cfg = self.cfg.borrow_mut();
        let Some(stored) = cfg.remote_settings.as_mut() else {
            return;
        };
        if stored.announcements != pre_fetch {
            tracing::debug!("announcements poll apply skipped: settings changed mid-fetch");
            return;
        }
        stored.announcements = fresh.announcements;
    }
    /// The single announcements push gate — every `remote_settings` writer
    /// funnels through here. Emits `x.ai/announcements/update` and advances
    /// the last-emitted baseline per [`announcements_push_payload`] (`mode`
    /// decides when an unchanged list still pushes), but only once the
    /// gateway accepts the send — a failed enqueue leaves the baseline
    /// untouched so the next gate call re-diffs and re-pushes.
    ///
    /// Synchronous by design: the decide→send→advance sequence cannot
    /// interleave with another gate call on the LocalSet.
    pub(super) fn emit_announcements(&self, mode: AnnouncementsPushMode) {
        let payload_list = {
            let cfg = self.cfg.borrow();
            let last = self.last_emitted_announcements.borrow();
            announcements_push_payload(
                cfg.remote_settings.as_ref().and_then(|s| s.announcements.as_deref()),
                &last,
                chrono::Utc::now(),
                mode,
            )
        };
        let Some(announcements) = payload_list else {
            return;
        };
        let payload = serde_json::json!({
            "gen": self.next_announcements_gen(),
            "announcements": announcements,
        });
        let Ok(params) = serde_json::value::to_raw_value(&payload) else {
            return;
        };
        let accepted = self
            .gateway
            .forward_fire_and_forget(
                acp::ExtNotification::new("x.ai/announcements/update", params.into()),
            );
        if !accepted {
            return;
        }
        *self.last_emitted_announcements.borrow_mut() = announcements.clone();
        tracing::info!(
            count = announcements.len(),
            mode = ?mode,
            "pushing announcements update to clients"
        );
    }
    /// Next generation for an `x.ai/announcements/update` push. Strictly
    /// increasing within the process, and seeded from unix-epoch seconds so a
    /// restarted leader's pushes still clear pager watermarks that survived
    /// re-election (`AppView.announcements_last_gen` outlives the agent).
    pub(super) fn next_announcements_gen(&self) -> u64 {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let next = now_secs.max(self.announcements_gen.get() + 1);
        self.announcements_gen.set(next);
        next
    }
    /// Shared fetch half of every settings refresh: endpoint fields from a
    /// scoped `cfg` borrow, `fetch_settings_blocking` off-executor (it already
    /// retries transient errors internally), failures normalized to `None`.
    /// Callers own their miss logging; the apply halves deliberately stay
    /// separate (full reapply vs announcements-only).
    pub(super) async fn fetch_remote_settings(
        &self,
        auth: crate::auth::GrokAuth,
    ) -> Option<crate::util::config::RemoteSettings> {
        if !crate::util::config::resolve_remote_fetch_enabled() {
            tracing::debug!("settings fetch skipped: remote_fetch disabled");
            return None;
        }
        let (base_url, alpha_test_key) = {
            let cfg = self.cfg.borrow();
            (cfg.endpoints.proxy_url(), cfg.endpoints.alpha_test_key.clone())
        };
        match tokio::task::spawn_blocking(move || crate::remote::fetch_settings_blocking(
                &base_url,
                &auth,
                alpha_test_key.as_deref(),
            ))
            .await
        {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!(error = %e, "settings fetch task panicked");
                None
            }
        }
    }
    pub(super) async fn send_model_auto_switched(
        &self,
        session_id: &acp::SessionId,
        previous: &acp::ModelId,
        new: &acp::ModelId,
        reason: &str,
    ) {
        let notification = crate::extensions::notification::SessionNotification {
            session_id: session_id.clone(),
            update: crate::extensions::notification::SessionUpdate::ModelAutoSwitched {
                previous_model_id: previous.0.to_string(),
                new_model_id: new.0.to_string(),
                reason: reason.to_string(),
            },
            meta: None,
        };
        if let Ok(params) = serde_json::value::to_raw_value(&notification) {
            let _ = self
                .gateway
                .ext_notification(
                    acp::ExtNotification::new("x.ai/session_notification", params.into()),
                )
                .await;
        }
    }
    /// Pure id → entry resolver (the `allowed_models` gate lives in `set_session_model`).
    pub(crate) fn resolve_model_id(
        &self,
        requested: &acp::ModelId,
    ) -> Result<ModelEntry, acp::Error> {
        let requested_str = requested.0.as_ref();
        let models = self.models_manager.models();
        let Some(catalog_key) = resolve_catalog_key(&models, requested) else {
            tracing::debug!(
                requested = %requested_str,
                model_count = models.len(),
                "resolve_model_id: unknown model id (not in models() by key or .model field)"
            );
            return Err(acp::Error::invalid_params().data("unknown model id"));
        };
        let entry = models
            .get(catalog_key.0.as_ref())
            .expect("resolve_catalog_key returns a key present in models");
        let match_kind = if catalog_key.0.as_ref() == requested_str {
            "map key"
        } else {
            "model field scan"
        };
        tracing::debug!(
            "resolve_model_id: matched by {}: requested={} model={}",
            match_kind,
            requested_str,
            entry.info.model
        );
        Ok(entry.clone())
    }
    pub(crate) fn prepare_sampling_config_for_model(
        &self,
        model: &ModelEntry,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) -> SamplingConfig {
        let preferred = self.cfg.borrow().grok_com_config.preferred_method;
        let session = match preferred {
            Some(crate::auth::PreferredAuthMethod::ApiKey) => None,
            _ if self.is_session_based_auth() => self.auth_manager.current_or_expired(),
            _ => None,
        };
        let has_session_key = session.is_some();
        let mut credentials = resolve_credentials(
            model,
            session.as_ref().map(|a| a.key.as_str()),
        );
        if matches!(preferred, Some(crate::auth::PreferredAuthMethod::Oidc))
            && !model.has_own_credentials()
            && credentials.auth_type == xai_chat_state::AuthType::ApiKey
        {
            credentials.api_key = None;
            credentials.auth_type = xai_chat_state::AuthType::SessionToken;
        }
        crate::agent::config::enforce_disable_api_key_auth(
            &mut credentials,
            self.cfg.borrow().grok_com_config.api_key_auth_disabled(),
            session.as_ref().map(|a| a.key.as_str()),
        );
        if !has_session_key && credentials.auth_type == xai_chat_state::AuthType::ApiKey
            && !model.has_own_credentials() && self.is_session_based_auth()
        {
            tracing::info!(
                model = model.info().model.as_str(),
                "auth: overriding auth_type to SessionToken (session-based auth method)",
            );
            xai_grok_telemetry::unified_log::info(
                "auth auth_type override to SessionToken",
                None,
                Some(serde_json::json!({ "model": model.info().model.as_str() })),
            );
            credentials.auth_type = xai_chat_state::AuthType::SessionToken;
        }
        if !has_session_key && !model.has_own_credentials() {
            tracing::warn!(
                model = model.info().model.as_str(),
                is_expired = self.auth_manager.is_expired(),
                auth_type = ?credentials.auth_type,
                "auth: prepare_sampling_config has no session key",
            );
            xai_grok_telemetry::unified_log::warn(
                "auth: prepare_sampling_config has no session key",
                None,
                Some(
                    serde_json::json!({
                    "model": model.info().model.as_str(),
                    "is_expired": self.auth_manager.is_expired(),
                    "auth_type": format!("{:?}", credentials.auth_type),
                }),
                ),
            );
        }
        let cfg = self.cfg.borrow();
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let client_version = cfg.client_version.clone();
        let deployment_id = crate::managed_config::resolve_deployment_id(
            cfg.endpoints.deployment_key.as_deref(),
        );
        drop(cfg);
        let user_id = self
            .auth_manager
            .current_or_expired()
            .filter(|a| a.is_xai_auth())
            .map(|a| a.user_id);
        let mut config = crate::agent::config::sampling_config_for_model(
            model,
            credentials,
            alpha_test_key,
            client_version,
            deployment_id,
            user_id,
        );
        config.origin_client = origin_client;
        config
    }
    /// Resolve sampling config for a model by ID, falling back to the global
    /// default on resolution failure. This ensures API-key auth routes to
    /// the public API (via resolve_credentials) instead of the global config's
    /// cli-chat-proxy base_url.
    pub(super) fn resolve_sampling_config_for_model(
        &self,
        model_id: &acp::ModelId,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) -> SamplingConfig {
        if let Ok(model) = self.resolve_model_id(model_id) {
            self.prepare_sampling_config_for_model(&model, origin_client.clone())
        } else {
            let mut c = self.sampling_config.borrow().clone();
            c.origin_client = origin_client;
            c
        }
    }
    /// Resolve `AgentDefinition.model` override for the parent session.
    /// Apply a profile's pinned-model override to the session's sampling config.
    ///
    /// `pinned_model` is resolved once by the caller (shared with harness
    /// inheritance). `None` — no override, or model not in catalog — keeps the
    /// session defaults.
    fn apply_agent_model_override(
        &self,
        pinned_model: Option<&(acp::ModelId, ModelEntry)>,
        default_model_id: acp::ModelId,
        default_sampling: SamplingConfig,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) -> (acp::ModelId, SamplingConfig) {
        let Some((id, model)) = pinned_model else {
            return (default_model_id, default_sampling);
        };
        let new_config = self.prepare_sampling_config_for_model(model, origin_client);
        tracing::info!(
            model = %id.0,
            "agent profile model override applied to parent session"
        );
        (id.clone(), new_config)
    }
    /// Whether the current session is a personal grok.com account on a gated
    /// tier (free / X Basic). The Imagine tools stay advertised to the model but
    /// are flagged tier-restricted so they short-circuit at call time with the
    /// SuperGrok upsell prose (see `ImageGenConfig`/`VideoGenConfig`'s
    /// `tier_restricted`).
    ///
    /// Fails **open** (returns `false`) whenever we can't positively confirm a
    /// restricted personal tier — no auth yet, BYOK / API-key sessions, team
    /// accounts, and an unknown/absent tier all pass. The server
    /// authoritatively zero-limits Imagine for free & X Basic (429), so this
    /// client gate is a UX optimization (a clean in-chat upsell instead of a
    /// doomed request), never the security boundary — under-restricting is safe,
    /// over-restricting would wrongly disable a paid feature.
    ///
    /// Mirrors the pager's cosmetic slash-command gate
    /// ([`crate::tier::is_restricted_tier_name`]); the only difference is the
    /// absent-tier policy (the pager hides on `None`, we fail open on `None`).
    fn is_tier_restricted_capability(&self) -> bool {
        let Some(auth) = self.auth_manager.current() else {
            return false;
        };
        if !auth.is_xai_auth() || auth.team_id.is_some() {
            return false;
        }
        let tier = self
            .cfg
            .borrow()
            .remote_settings
            .as_ref()
            .and_then(|rs| rs.subscription_tier_display.clone())
            .or_else(|| jwt_tier_claim(&auth.key));
        tier.as_deref().is_some_and(crate::tier::is_restricted_tier_name)
    }
    /// Build image generation config.
    ///
    /// Both BYOK and session (OAuth) users go direct to `xai_api_base_url`.
    /// `sampling_config.api_key` carries the OAuth bearer for session users (the
    /// `api_key_provider` refreshes it per request), so IC authenticates and
    /// meters Imagine usage per-user.
    pub(super) fn prepare_image_gen_config(
        &self,
    ) -> xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig {
        use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
        let sampling_config = self.sampling_config.borrow();
        let Some(ref api_key) = sampling_config.api_key else {
            return ImageGenConfig::Disabled;
        };
        let tier_restricted = self.is_tier_restricted_capability();
        let cfg = self.cfg.borrow();
        let base_url = cfg.endpoints.xai_api_base_url.clone();
        let version = cfg
            .client_version
            .clone()
            .unwrap_or_else(|| xai_grok_version::VERSION.to_string());
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let mut headers = indexmap::IndexMap::new();
        headers.insert("user-agent".to_string(), format!("xai-grok-build/{version}"));
        inject_proxy_headers(
            &mut headers,
            cfg.client_version.as_deref(),
            alpha_test_key.as_deref(),
            &base_url,
        );
        ImageGenConfig::Enabled {
            api_key: api_key.clone(),
            base_url,
            extra_headers: headers,
            image_gen_enabled: cfg.resolve_image_gen().value,
            image_edit_enabled: cfg.resolve_image_edit().value,
            model_override: cfg.resolve_image_gen_model_override(),
            tier_restricted,
        }
    }
    /// Build deploy-service config. The tool talks directly to the deployer service.
    pub(super) fn prepare_app_builder_deployer_config(
        &self,
    ) -> xai_grok_tools::implementations::grok_build::deploy_app::AppBuilderDeployerConfig {
        use xai_grok_tools::implementations::grok_build::deploy_app::AppBuilderDeployerConfig;
        AppBuilderDeployerConfig::Disabled
    }
    /// Build video generation config. Video tools call the xAI API directly.
    pub(super) fn prepare_video_gen_config(
        &self,
    ) -> xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig {
        use xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig;
        let cfg = self.cfg.borrow();
        if !cfg.resolve_video_gen().value {
            return VideoGenConfig::Disabled;
        }
        let Some(api_key) = self.sampling_config.borrow().api_key.clone() else {
            return VideoGenConfig::Disabled;
        };
        let tier_restricted = self.is_tier_restricted_capability();
        let zdr_video_output_s3 = cfg
            .disable_zdr_incompatible_tools
            .then(|| cfg.zdr_video_output_s3.clone())
            .flatten()
            .filter(|s3| s3.is_valid());
        if cfg.disable_zdr_incompatible_tools && zdr_video_output_s3.is_none() {
            tracing::info!("video_gen disabled by tools.disable_zdr_incompatible_tools");
            return VideoGenConfig::Disabled;
        }
        let base_url = cfg.endpoints.xai_api_base_url.clone();
        let version = cfg
            .client_version
            .clone()
            .unwrap_or_else(|| xai_grok_version::VERSION.to_string());
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let mut headers = indexmap::IndexMap::new();
        headers.insert("user-agent".to_string(), format!("xai-grok-build/{version}"));
        inject_proxy_headers(
            &mut headers,
            cfg.client_version.as_deref(),
            alpha_test_key.as_deref(),
            &base_url,
        );
        VideoGenConfig::Enabled {
            api_key,
            base_url,
            extra_headers: headers,
            zdr_video_output_s3: zdr_video_output_s3.map(Box::new),
            tier_restricted,
        }
    }
    pub(super) fn prepare_web_search_sampling_config(&self) -> Option<SamplingConfig> {
        let model_id = self.cfg.borrow().web_search_model.clone();
        let models = self.models_manager.models();
        let session = self.current_or_buffered_auth();
        let alpha_test_key = self.cfg.borrow().endpoints.alpha_test_key.clone();
        let client_version = self.cfg.borrow().client_version.clone();
        let mut cfg = config::resolve_web_search_sampling_config(
            &model_id,
            &models,
            session.as_ref().map(|a| a.key.as_str()),
            self.cfg.borrow().grok_com_config.api_key_auth_disabled(),
            alpha_test_key.clone(),
            client_version,
            &self.cfg.borrow().endpoints,
        )?;
        inject_proxy_headers(
            &mut cfg.extra_headers,
            cfg.client_version.as_deref(),
            alpha_test_key.as_deref(),
            &cfg.base_url,
        );
        Some(cfg)
    }
    /// Returns `Err` with a user-facing message on invalid config; the caller at
    /// the process boundary prints it and exits.
    pub fn new(
        gateway: GatewaySender,
        cfg: &AgentConfig,
        auth_manager: Arc<AuthManager>,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
    ) -> Result<Self, String> {
        let (cfg, models_manager) = crate::agent::init::bootstrap(
            cfg,
            &auth_manager,
            prefetched_models,
        )?;
        Ok(Self::with_models(gateway, &cfg, auth_manager, models_manager))
    }
    /// Prepare the web fetch configuration based on feature flags.
    ///
    /// Enabled gate: `disable_web_search` kill-switch > `GROK_WEB_FETCH` env >
    /// remote settings `web_fetch_enabled` > default (false).
    ///
    /// Params resolution (TOML > env > remote settings > default):
    /// - `proxy_endpoint`: `[toolset.web_fetch] proxy_endpoint` > `GROK_WEB_FETCH_PROXY` > remote settings > None
    /// - `allowed_domains`: `[toolset.web_fetch] allowed_domains` > remote settings > built-in defaults
    /// - `allow_local`: `[toolset.web_fetch] allow_local` > `GROK_WEB_FETCH_ALLOW_LOCAL` > false
    pub(super) fn prepare_web_fetch_config(
        &self,
    ) -> xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig {
        use xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig;
        let cfg = self.cfg.borrow();
        if cfg.disable_web_search {
            return WebFetchConfig::Disabled;
        }
        let remote = cfg.remote_settings.as_ref();
        let enabled = cfg.resolve_web_fetch();
        if !enabled.value {
            return WebFetchConfig::Disabled;
        }
        let context_window = Some(self.sampling_config.borrow().context_window);
        let params = cfg
            .toolset
            .web_fetch
            .resolve_params(
                remote.and_then(|s| s.web_fetch_proxy.as_deref()),
                remote.and_then(|s| s.web_fetch_allowed_domains.as_deref()),
                context_window,
            );
        if params.allowed_domains.as_ref().is_some_and(Vec::is_empty) {
            tracing::info!("web_fetch disabled: allowed_domains is explicitly empty");
            return WebFetchConfig::Disabled;
        }
        WebFetchConfig::Enabled { params }
    }
    /// Construct from pre-built components. Use when the caller needs the
    /// `ModelsManager` handle externally (e.g. `run_leader` wires it to the
    /// config watcher). Otherwise prefer [`Self::new`].
    pub fn with_models(
        gateway: GatewaySender,
        cfg: &AgentConfig,
        auth_manager: Arc<AuthManager>,
        models_manager: crate::agent::models::ModelsManager,
    ) -> Self {
        models_manager.set_gateway(gateway.clone());
        let sampling_config = models_manager.sampling_config();
        if !cfg.grok_com_config.api_key_auth_disabled() {
            let models = models_manager.models();
            let current = models_manager.current_model_id();
            auth_manager
                .set_process_static_api_key(
                    byok_from_models(&models, None, current.0.as_ref()),
                );
        }
        crate::upload::trace::spawn_purge_stale_upload_scratch();
        let storage_mode = cfg.storage_mode;
        let default_yolo_mode = cfg.default_yolo_mode;
        let default_auto_mode = cfg.default_auto_mode;
        let tui_mode = cfg.mode == crate::agent::config::AgentMode::Tui;
        let relay_config_enabled = crate::util::config::load_relay_sync_enabled_sync();
        let has_xai_auth = auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_xai_auth());
        let relay_sync_enabled = tui_mode && relay_config_enabled && has_xai_auth;
        let config_root = crate::config::load_effective_config().ok();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let raw = config_root.as_ref().unwrap_or(&empty_config);
        let (worktree_type, wt_source) = crate::util::config::resolve_worktree_type(
            raw,
            cfg.remote_settings.as_ref(),
        );
        let restore_code = crate::util::config::resolve_restore_code(
            raw,
            cfg.remote_settings.as_ref(),
        );
        let session_registry_local = crate::util::config::session_registry_local_override(
            config_root.as_ref(),
        );
        tracing::info!(
            worktree_type = ?worktree_type,
            source = wt_source,
            "WORKTREE_CONFIG_SHELL: resolved worktree type at agent startup"
        );
        if relay_sync_enabled {
            tracing::info!("[grok] Relay sync: ENABLED");
        } else if tui_mode && relay_config_enabled && !has_xai_auth {
            tracing::info!("[grok] Relay sync: DISABLED (no auth - run 'grok login' first)");
        } else if tui_mode && !relay_config_enabled {
            tracing::debug!("Relay sync: DISABLED (not configured in config.toml or env)");
        } else {
            tracing::debug!("Relay sync: DISABLED (not in TUI mode)");
        }
        if cfg.telemetry.trace_upload == Some(false) {
            tracing::info!(
                enabled = false,
                reason = "feature_off",
                "trace_upload_status"
            );
        }
        let (subagent_event_tx, subagent_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let activity = crate::agent::activity::AgentActivity::default();
        let mut subagent_coordinator = crate::agent::subagent::SubagentCoordinator::new();
        subagent_coordinator.set_running_gauge(activity.subagent_gauge());
        let instance = Self {
            sessions: RefCell::new(HashMap::new()),
            activity,
            loading_sessions: RefCell::new(HashMap::new()),
            dispatch_locks: RefCell::new(HashMap::new()),
            session_threads: RefCell::new(HashMap::new()),
            resident_roster_titles: RefCell::new(HashMap::new()),
            initialize_request: OnceLock::new(),
            gateway,
            launch_cwd: std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from(".")),
            launch_dir_trust: std::cell::OnceCell::new(),
            plugin_registry_handle: xai_grok_agent::plugins::SharedPluginRegistryHandle::new(
                None,
                cfg.plugins.cli_plugin_dirs.clone(),
            ),
            plugin_registry_initialized: std::cell::Cell::new(false),
            models_manager,
            chat_modes: {
                let chat_modes = crate::agent::chat_modes::ChatModesManager::new(
                    auth_manager.clone(),
                );
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    chat_modes.warm_in_background();
                }
                chat_modes
            },
            cfg: RefCell::new(cfg.clone()),
            auth_method_id: crate::agent::auth_method::new_shared_auth_method_id(None),
            sampling_config: RefCell::new(sampling_config),
            auth_manager,
            interactive_auth: Default::default(),
            client_type: RefCell::new(ClientType::default()),
            code_nav_enabled: std::cell::Cell::new(false),
            interactive_trust_client: std::cell::Cell::new(false),
            interactive_trust_prompted: Rc::new(
                RefCell::new(std::collections::HashSet::new()),
            ),
            tier_allowed: std::cell::Cell::new(true),
            storage_mode,
            default_yolo_mode,
            default_auto_mode,
            trace_upload_live: Arc::new(
                std::sync::atomic::AtomicBool::new(cfg.is_trace_upload_enabled()),
            ),
            memory_config: None,
            config_watcher_path_tx: None,
            relay_sync_enabled,
            buffering_settings: RefCell::new(None),
            background_copy_context: BackgroundCopyContext::new(),
            session_turn_numbers: RefCell::new(HashMap::new()),
            permission_event_receivers: RefCell::new(HashMap::new()),
            codebase_indexes: Arc::new(
                parking_lot::Mutex::new(CodebaseIndexManager::new()),
            ),
            session_index_claims: RefCell::new(HashMap::new()),
            worktree_type,
            restore_code,
            session_registry_local,
            managed_mcp_cache: Default::default(),
            agent_mcp_state: std::sync::Arc::new(
                tokio::sync::Mutex::new(
                    crate::session::mcp_servers::McpState::new(vec![]),
                ),
            ),
            model_unavailable_sessions: RefCell::new(std::collections::HashMap::new()),
            subagent_event_tx,
            subagent_event_rx: RefCell::new(Some(subagent_event_rx)),
            subagent_coordinator: RefCell::new(subagent_coordinator),
            monitor_event_buffer: xai_grok_tools::implementations::grok_build::task::types::MonitorEventBuffer::default(),
            bundle_sync_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            post_unblock_jwt_retry_in_flight: Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            ),
            workspace_ops: RefCell::new(None),
            require_gateway_sessions: Rc::new(
                RefCell::new(std::collections::HashSet::new()),
            ),
            session_live_state: RefCell::new(HashMap::new()),
            supervisor_started: std::cell::Cell::new(false),
            announcements_gen: std::cell::Cell::new(0),
            last_emitted_announcements: RefCell::new(Vec::new()),
            announcements_refresh_started: std::cell::Cell::new(false),
            heap_profile_monitor: RefCell::new(
                crate::heap_profile::HeapProfileMonitor::new(),
            ),
            heap_profile_started: std::cell::Cell::new(false),
            #[cfg(test)]
            finalize_spy: RefCell::new(Vec::new()),
            #[cfg(test)]
            roster_delta_spy: RefCell::new(Vec::new()),
            #[cfg(test)]
            supervisor_spawn_count: std::cell::Cell::new(0),
        };
        instance
            .auth_manager
            .configure_refresher(
                instance.cfg.borrow().grok_com_config.auth_provider_command.clone(),
                instance.diagnostic_upload_config(),
            );
        crate::auth::credential_provider::wire_otel_auth_manager(
            instance.auth_manager.clone(),
        );
        if let Some(ref dk) = instance.cfg.borrow().endpoints.deployment_key {
            crate::auth::credential_provider::wire_otel_deployment_key(dk.clone());
        }
        instance
    }
    /// Handle `x.ai/internal/evict_sessions` — the leader server tells us a
    /// client disconnected and these sessions lost their IPC owner.
    ///
    /// **This is the no-evict keystone.** A disconnect must
    /// NOT destroy a session. The behavior is now *detach + keep-resident +
    /// idle-unload*:
    ///
    /// - **Sessions with live work stay resident.** We do NOT send `Shutdown`
    ///   and do NOT drop the `SessionHandle`, so the actor, its pending
    ///   permission oneshots, and its `KillOnDrop` tool subprocesses all
    ///   survive. The route/driver detach is groundwork for PR-3 (the
    ///   driver/subscriber maps don't exist yet), so for now we only mark the
    ///   live state.
    /// - **Fully idle sessions are unloaded to disk** to bound memory (the
    ///   `sessions`/`session_threads` maps are uncapped). This preserves the
    ///   legacy unload path — `Shutdown` the actor, drop the `SessionHandle`,
    ///   but KEEP the `SessionThread` so `drain_old_session_thread` can drain it
    ///   on reconnect — and crucially does **not** finalize the cloud replica
    ///   (the session remains resumable via `session/load`).
    ///
    /// The "live work" check is the coarse PR-2 stub (`session_has_live_work`);
    /// the full `SessionActivity` signal lands in PR-4.
    pub(super) async fn handle_evict_sessions(
        &self,
        params: &serde_json::value::RawValue,
    ) {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct EvictParams {
            session_ids: Vec<String>,
        }
        let Ok(p) = serde_json::from_str::<EvictParams>(params.get()) else {
            tracing::warn!("Failed to parse evict_sessions params");
            return;
        };
        if p.session_ids.is_empty() {
            return;
        }
        tracing::info!(
            count = p.session_ids.len(),
            sessions = ?p.session_ids,
            "Client disconnected; detaching sessions (no-evict keystone)"
        );
        let checks = p
            .session_ids
            .iter()
            .map(|sid| {
                let id = acp::SessionId::new(sid.clone());
                async move {
                    let busy = self.session_has_live_work(&id).await;
                    (id, busy)
                }
            });
        let resolved = futures::future::join_all(checks).await;
        let mut kept_resident: usize = 0;
        let mut unloaded: usize = 0;
        for (id, busy) in resolved {
            if busy {
                self.set_session_live_state(&id, SessionLiveState::Working);
                kept_resident += 1;
                tracing::info!(
                    session_id = %id.0,
                    "kept session resident across client disconnect (live work)"
                );
                continue;
            }
            self.request_session_shutdown(&id);
            if self.sessions.borrow_mut().remove(&id).is_some() {
                self.session_index_claims.borrow_mut().remove(&id);
                self.require_gateway_sessions.borrow_mut().remove(&id);
                self.set_session_live_state(&id, SessionLiveState::Dormant);
                unloaded += 1;
                tracing::debug!(session_id = %id.0, "idle session unloaded to disk on disconnect");
            }
        }
        tracing::info!(kept_resident, unloaded, "client-disconnect detach complete");
        self.sweep_dead_sessions();
    }
    /// Wait for an old session thread to finish before reloading the same session.
    ///
    /// When a client disconnects and a session is *idle*, `handle_evict_sessions`
    /// unloads it: sends `Shutdown`, drops the `SessionHandle`, and keeps the
    /// `SessionThread`. (Sessions with live work stay fully resident and skip
    /// this path.) If the client reconnects and loads the same session, we must
    /// wait for the old actor to finish flushing to disk before replaying
    /// `updates.jsonl`.
    ///
    /// Uses async polling (never blocks the `LocalSet` runtime) with a 5s deadline
    /// to handle slow shutdowns (e.g., embedding API timeouts).
    pub(super) async fn drain_old_session_thread(&self, session_id: &acp::SessionId) {
        let thread = self.session_threads.borrow_mut().remove(session_id);
        let Some(thread) = thread else { return };
        if thread.is_finished() {
            return;
        }
        tracing::info!(
            session_id = %session_id.0,
            "Waiting for old session thread to finish before reload"
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if thread.is_finished() {
                tracing::debug!(
                    session_id = %session_id.0,
                    "Old session thread finished cleanly"
                );
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    session_id = %session_id.0,
                    "Old session thread still running after 5s — proceeding with replay. \
                     Session data may be incomplete if the old actor is still writing."
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    /// Mark a `session/load` as in flight for `session_id`.
    ///
    /// Returns an RAII guard; while it is alive,
    /// [`Self::wait_for_in_flight_session_load`] blocks racing session-scoped
    /// requests for the same session. Dropping the guard (every exit path of
    /// `load_session`, success or error) removes the marker and wakes all
    /// waiters via watch-channel closure.
    pub(super) fn begin_session_load(
        &self,
        session_id: &acp::SessionId,
    ) -> SessionLoadGuard<'_> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        self.loading_sessions.borrow_mut().insert(session_id.clone(), rx.clone());
        SessionLoadGuard {
            agent: self,
            session_id: session_id.clone(),
            rx,
            _tx: tx,
        }
    }
    /// Session lookup that tolerates an in-flight `session/load`.
    ///
    /// THE chokepoint for the post-leader-crash error class: every
    /// user-facing session-scoped handler (`prompt`, `set_session_model`,
    /// `set_session_mode`, `interject`, ...) resolves its handle through
    /// this instead of a bare `sessions` lookup, so a request racing the
    /// reconnect-replayed `session/load` waits for the session to land
    /// rather than failing with "unknown session id" / "session not found".
    ///
    /// Returns `None` only when the session is genuinely absent — no load in
    /// flight (or the load failed / timed out), exactly the cases where the
    /// legacy error is correct.
    pub(crate) async fn session_handle_waiting_for_load(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<crate::session::SessionHandle> {
        let existing = self.sessions.borrow().get(session_id).cloned();
        if existing.is_some() {
            return existing;
        }
        self.wait_for_in_flight_session_load(session_id).await;
        self.sessions.borrow().get(session_id).cloned()
    }
    /// If a `session/load` for `session_id` is in flight, wait (bounded) for
    /// it to finish. Returns immediately when no load is in flight.
    ///
    /// This closes the load-vs-request race after a leader restart: clients
    /// replay `session/load` on reconnect, and a `session/prompt` arriving
    /// right behind it must wait for the session to land in `self.sessions`
    /// instead of failing with "unknown session id". The wait wakes when the
    /// load's [`SessionLoadGuard`] drops (success or failure) and re-checks;
    /// a failed load still surfaces the original error to the caller.
    pub(crate) async fn wait_for_in_flight_session_load(
        &self,
        session_id: &acp::SessionId,
    ) {
        const LOAD_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(
            60,
        );
        let deadline = tokio::time::Instant::now() + LOAD_WAIT_TIMEOUT;
        loop {
            if self.sessions.borrow().contains_key(session_id) {
                return;
            }
            let rx = self.loading_sessions.borrow().get(session_id).cloned();
            let Some(mut rx) = rx else { return };
            let now = tokio::time::Instant::now();
            if now >= deadline {
                tracing::warn!(
                    session_id = %session_id.0,
                    "timed out waiting for in-flight session/load"
                );
                return;
            }
            let _ = tokio::time::timeout(deadline - now, rx.changed()).await;
        }
    }
    /// Returns the default YOLO mode setting for new sessions
    pub fn default_yolo_mode(&self) -> bool {
        self.default_yolo_mode
    }
    /// Returns the storage mode configured for this agent
    pub fn storage_mode(&self) -> StorageMode {
        self.storage_mode
    }
    /// Returns the background copy context for managing background file copy tasks.
    pub fn background_copy_context(&self) -> BackgroundCopyContext {
        self.background_copy_context.clone()
    }
    /// Move a foreground bash command to background.
    /// Routes through the session's tool bridge to unblock the agent loop.
    pub async fn background_foreground_command(
        &self,
        session_id: &str,
        tool_call_id: &str,
    ) -> bool {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.background_foreground_command(tool_call_id).await
        } else {
            false
        }
    }
    /// Kill a background task by task_id.
    /// Routes through the session's tool bridge to the TerminalBackend.
    pub async fn kill_background_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<xai_grok_tools::types::KillOutcome, String> {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.kill_background_task(task_id).await
        } else {
            Err("session not found".to_string())
        }
    }
    pub async fn delete_scheduled_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<bool, String> {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.delete_scheduled_task(task_id).await
        } else {
            Err("session not found".to_string())
        }
    }
    /// Cancel a subagent by id, returning a typed outcome that backs the pager's
    /// `x.ai/subagent/cancel`. Active/pending → cancelled (a finish follows);
    /// already-finished → its terminal status; unknown id → `NotFound`.
    pub fn cancel_subagent(
        &self,
        subagent_id: &str,
    ) -> xai_grok_tools::implementations::grok_build::task::types::SubagentCancelOutcome {
        self.subagent_coordinator.borrow_mut().cancel_with_outcome(subagent_id)
    }
    /// List running subagent seeds for a given parent session.
    ///
    /// Synchronously collects seeds from the coordinator, suitable for
    /// async resolution via `resolve_running_list()` after the borrow is
    /// dropped.
    pub(crate) fn list_running_subagents(
        &self,
        parent_session_id: &str,
    ) -> Vec<crate::agent::subagent::RunningSubagentListSeed> {
        self.subagent_coordinator.borrow().list_running_for_parent(parent_session_id)
    }
    /// Return fork provenance metadata for a subagent.
    pub(crate) fn provenance_for_subagent(
        &self,
        subagent_id: &str,
    ) -> crate::agent::subagent::SubagentProvenance {
        self.subagent_coordinator.borrow().provenance_for(subagent_id)
    }
    /// Return `(parent_session_id, child_session_id)` for a subagent.
    pub(crate) fn session_ids_for_subagent(
        &self,
        subagent_id: &str,
    ) -> Option<(String, String)> {
        self.subagent_coordinator.borrow().session_ids_for(subagent_id)
    }
    /// Synchronous lookup of a single subagent by ID.
    ///
    /// Returns `Option<SnapshotLookup>` which must be resolved
    /// asynchronously via `resolve_snapshot()` after the borrow is dropped.
    pub(crate) fn lookup_subagent(
        &self,
        subagent_id: &str,
    ) -> Option<crate::agent::subagent::SnapshotLookup> {
        self.subagent_coordinator.borrow().lookup(subagent_id)
    }
    /// List all background tasks for a session.
    /// Routes through the session's tool bridge to the TerminalBackend.
    pub async fn list_tasks(
        &self,
        session_id: &str,
    ) -> Option<Vec<xai_grok_tools::types::TaskSnapshot>> {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.list_tasks().await
        } else {
            None
        }
    }
    /// Flush a session's persistence buffer with a 5-second timeout.
    ///
    /// Sends `FlushComplete` to the session actor, which chains through to
    /// `FlushAndAck` on the persistence actor — a true sync barrier that only
    /// resolves after all queued writes (chat messages, updates) hit disk.
    ///
    /// Returns `Ok(())` on success, `Err(reason)` on timeout or channel failure.
    pub(crate) async fn flush_session(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<(), &'static str> {
        let cmd_tx = self.sessions.borrow().get(session_id).map(|h| h.cmd_tx.clone());
        let Some(cmd_tx) = cmd_tx else {
            return Err("session not found");
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        if cmd_tx
            .send(SessionCommand::FlushComplete {
                respond_to: tx,
            })
            .is_err()
        {
            return Err("send failed");
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err("channel closed"),
            Err(_) => Err("timeout"),
        }
    }
    /// Create a RelaySync instance if enabled and auth is available.
    /// RelaySync is only enabled when:
    /// 1. Running in TUI interactive mode (cfg.enable_relay_sync)
    /// 2. Config file/env enables it ([relay] enabled or GROK_RELAY_SYNC_ENABLED)
    /// 3. User is authenticated
    ///
    /// Returns a `RelaySync` instance whose connection state can be observed
    /// via `connection_state()`.
    pub(super) fn create_relay_sync(
        &self,
        session_id: &str,
        session_info: &crate::session::info::Info,
    ) -> Option<crate::relay::RelaySync> {
        if !self.relay_sync_enabled {
            return None;
        }
        let auth = self.auth_manager.current_or_expired()?;
        if auth.is_zdr_team() {
            tracing::debug!("ZDR team: skipping relay sync");
            return None;
        }
        let cfg = self.cfg.borrow();
        let relay_config = crate::agent::relay::RelayConfig::for_session(
            &auth,
            &cfg.grok_com_config,
            cfg.endpoints.alpha_test_key.clone(),
            None,
        )?;
        let session_dir = crate::session::persistence::session_dir(session_info);
        Some(
            crate::relay::RelaySync::new(
                session_id.to_string(),
                relay_config,
                crate::relay::AgentType::Tui,
                Some(session_dir),
                None,
            ),
        )
    }
    /// Spawn a local task that watches `ConnectionState` changes and forwards
    /// them to the TUI as `ExtNotification`s containing `RelaySyncStatus`.
    ///
    /// This replaces the old `status_rx` channel that was removed when
    /// `RelaySyncWithStatus` was eliminated.
    pub(super) fn spawn_relay_state_forwarder(
        mut state_rx: tokio::sync::watch::Receiver<crate::relay::ConnectionState>,
        session_id: String,
        gateway: GatewaySender,
    ) {
        use crate::extensions::notification::RelaySyncStatus;
        let session_id = acp::SessionId::new(session_id);
        tokio::task::spawn_local(async move {
            while state_rx.changed().await.is_ok() {
                let state = *state_rx.borrow_and_update();
                let status = match state {
                    crate::relay::ConnectionState::Connected => {
                        let share_url = crate::relay::sync::build_share_url(
                            &session_id.0,
                        );
                        RelaySyncStatus::Connected {
                            share_url,
                        }
                    }
                    crate::relay::ConnectionState::Disconnected => {
                        RelaySyncStatus::Disconnected
                    }
                    crate::relay::ConnectionState::Connecting => {
                        RelaySyncStatus::Reconnecting {
                            attempt: 0,
                        }
                    }
                };
                let notification = SessionNotification {
                    session_id: session_id.clone(),
                    update: SessionUpdate::RelaySyncStatus(status),
                    meta: None,
                };
                if let Ok(params) = serde_json::value::to_raw_value(&notification) {
                    let ext_notification = acp::ExtNotification::new(
                        "x.ai/session_notification",
                        params.into(),
                    );
                    let _ = gateway.ext_notification(ext_notification).await;
                }
            }
        });
    }
    /// Get a session's cwd by session_id.
    /// Returns None if the session is not found.
    pub fn get_session_cwd(&self, session_id: &acp::SessionId) -> Option<PathBuf> {
        let sessions = self.sessions.borrow();
        sessions.get(session_id).map(|handle| PathBuf::from(&handle.info.cwd))
    }
    /// Get a session handle by session_id.
    /// Returns None if the session is not found.
    pub fn get_session_handle(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<crate::session::SessionHandle> {
        let sessions = self.sessions.borrow();
        sessions.get(session_id).cloned()
    }
    /// Get hooks list for a session (for `x.ai/hooks/list` extension).
    pub async fn list_hooks(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<xai_hooks_plugins_types::HooksListResponse> {
        let handle = self.get_session_handle(session_id)?;
        handle.get_hooks_list().await
    }
    /// Execute a hooks management action (for `x.ai/hooks/action`).
    pub async fn execute_hooks_action(
        &self,
        session_id: &acp::SessionId,
        action: xai_hooks_plugins_types::HooksAction,
    ) -> Option<xai_hooks_plugins_types::ActionOutcome> {
        if matches!(action, xai_hooks_plugins_types::HooksAction::Untrust)
            && let Some(cwd) = self.get_session_cwd(session_id)
        {
            self.interactive_trust_prompted
                .borrow_mut()
                .remove(&xai_grok_workspace::trust::workspace_key(&cwd));
        }
        let handle = self.get_session_handle(session_id)?;
        handle.execute_hooks_action(action).await
    }
    /// Execute a plugins management action (for `x.ai/plugins/action`).
    pub async fn execute_plugins_action(
        &self,
        session_id: &acp::SessionId,
        action: xai_hooks_plugins_types::PluginsAction,
    ) -> Option<xai_hooks_plugins_types::ActionOutcome> {
        let is_reload = matches!(action, xai_hooks_plugins_types::PluginsAction::Reload);
        let handle = self.get_session_handle(session_id)?;
        let outcome = handle.execute_plugins_action(action).await;
        let succeeded = matches!(
            outcome.as_ref().map(|o| &o.status),
            Some(xai_hooks_plugins_types::OutcomeStatus::Success)
        );
        if is_reload && succeeded {
            self.broadcast_plugin_registry_to_sessions(Some(session_id));
        }
        outcome
    }
    /// Get a snapshot of the shared plugin registry (for `x.ai/plugins/list`).
    pub fn plugin_registry_snapshot(
        &self,
    ) -> Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>> {
        self.plugin_registry_handle.snapshot()
    }
    /// Run content search at agent level.
    /// This allows content search to work with just a cwd, without requiring a session.
    /// Returns an upload method, or `None` when trace uploads are disabled.
    pub async fn trace_upload_config(
        &self,
    ) -> Option<crate::session::repo_changes::UploadMethod> {
        let (method, _reason) = self.trace_upload_config_with_reason().await;
        method
    }
    pub(super) fn trace_upload_config_snapshot(
        &self,
    ) -> Option<crate::session::repo_changes::UploadMethod> {
        if self.is_data_collection_disabled()
            || !self.cfg.borrow().is_trace_upload_enabled()
        {
            return None;
        }
        let cfg = self.cfg.borrow();
        let auth_token = if cfg.endpoints.deployment_key.is_none() {
            self.auth_manager
                .current_or_expired()
                .filter(|auth| auth.is_xai_auth())
                .map(|auth| auth.key)
        } else {
            None
        };
        cfg.endpoints.resolve_upload_method(auth_token)
    }
    pub(super) fn diagnostic_upload_config(
        &self,
    ) -> Option<crate::auth::DiagnosticUploader> {
        self.sync_collection_config_gate();
        let cfg = self.cfg.borrow();
        if !cfg.is_trace_upload_enabled() {
            return None;
        }
        let proxy_base_url = cfg.endpoints.resolve_trace_upload_url();
        let deployment_key = cfg.endpoints.deployment_key.clone();
        let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
        let auth_manager = self.auth_manager.clone();
        let trace_upload_live = self.trace_upload_live.clone();
        Some(
            std::sync::Arc::new(move |
                log_bytes: Vec<u8>,
                auth_token: String,
                user_id: String|
            {
                let proxy_base_url = proxy_base_url.clone();
                let deployment_key = deployment_key.clone();
                let alpha_test_key = alpha_test_key.clone();
                let auth_manager = auth_manager.clone();
                let trace_upload_live = trace_upload_live.clone();
                Box::pin(async move {
                    if !auth_manager.allows_data_collection()
                        || !trace_upload_live.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        tracing::debug!(
                            "skipping auth-diagnostics upload: data collection disabled"
                        );
                        return;
                    }
                    let upload_method = crate::session::repo_changes::UploadMethod::Proxy {
                        proxy_base_url,
                        user_token: auth_token,
                        deployment_key,
                        alpha_test_key,
                    };
                    crate::upload::gcs::upload_to_auth_diagnostics(
                            &log_bytes,
                            &user_id,
                            &upload_method,
                            auth_manager,
                        )
                        .await;
                })
            }),
        )
    }
    /// Like `trace_upload_config`, but also returns the reason why uploads
    /// are enabled or disabled for structured session events.
    async fn trace_upload_config_with_reason(
        &self,
    ) -> (
        Option<crate::session::repo_changes::UploadMethod>,
        crate::upload::turn::TraceUploadReason,
    ) {
        use crate::upload::turn::TraceUploadReason;
        if self.is_data_collection_disabled() {
            crate::upload::trace::spawn_startup_spill_reconcile(
                crate::util::grok_home::grok_home(),
                None,
            );
            return (None, TraceUploadReason::ZdrTeam);
        }
        if self.cfg.borrow().remote_settings.is_none()
            && let Ok(auth) = self.auth_manager.auth().await
        {
            self.refresh_remote_settings(&auth).await;
        }
        let (direct_method, has_deployment_key, endpoints) = {
            let cfg = self.cfg.borrow();
            if !cfg.is_trace_upload_enabled() {
                return (None, TraceUploadReason::FeatureOff);
            }
            (
                cfg.endpoints.resolve_direct_upload_method(),
                cfg.endpoints.deployment_key.is_some(),
                cfg.endpoints.clone(),
            )
        };
        let service_account_key = crate::util::config::load_gcs_service_account_key_sync();
        let method = if let Some(method) = direct_method {
            Some(method)
        } else {
            let auth_token = if has_deployment_key {
                None
            } else {
                self.auth_manager
                        .auth()
                        .await
                        .ok()
                        .filter(|auth| auth.is_xai_auth())
                        .map(|auth| auth.key)
            };
            if auth_token.is_some() || has_deployment_key {
                endpoints.resolve_upload_method(auth_token)
            } else if service_account_key.is_some() {
                Some(crate::session::repo_changes::UploadMethod::Direct {
                    service_account_key,
                })
            } else {
                None
            }
        };
        let reason = crate::upload::turn::TraceUploadReason::from_upload_method(&method);
        (method, reason)
    }
    /// Resolve client version: prefer the value from the initialize request _meta,
    /// fall back to the agent's own version (VERSION_WITH_COMMIT set by the TUI launcher).
    pub(super) fn client_version(&self) -> Option<String> {
        self.initialize_request
            .get()
            .and_then(|req| req.meta.as_ref())
            .and_then(|m| m.get("clientVersion"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| self.cfg.borrow().client_version.clone())
    }
    pub(super) fn origin_client_info_from_meta(
        &self,
        meta: Option<&acp::Meta>,
    ) -> Option<crate::http::OriginClientInfo> {
        crate::http::merge_origin_client_info(
                crate::http::origin_client_info_from_meta(meta),
                crate::http::origin_client_info_from_meta(
                        self.initialize_request.get().and_then(|req| req.meta.as_ref()),
                    )
                    .map(|mut origin| {
                        if origin.version.is_none() {
                            origin.version = self.client_version();
                        }
                        origin
                    }),
            )
            .map(|mut origin| {
                if origin.version.is_none() {
                    origin.version = self.client_version();
                }
                origin
            })
    }
    /// Returns the model state for a given session (or the agent default).
    ///
    /// When `session_id` is `Some`, looks up the session's per-session model.
    /// Falls back to `current_model_id` (startup default) when no session is
    /// found or `session_id` is `None` (e.g., during `initialize` before any
    /// session exists).
    pub fn model_state(
        &self,
        session_id: Option<&acp::SessionId>,
    ) -> acp::SessionModelState {
        let model_id = lookup_session_model(
            &self.sessions.borrow(),
            session_id,
            &self.models_manager.current_model_id(),
        );
        let mut available_models: Vec<acp::ModelInfo> = self
            .models_manager
            .available()
            .values()
            .cloned()
            .collect();
        let override_effort = session_id
            .and_then(|sid| self.sessions.borrow().get(sid).map(|h| h.reasoning_effort))
            .flatten()
            .or_else(|| self.models_manager.current_reasoning_effort());
        if let Some(override_effort) = override_effort
            && let Some(info) = available_models
                .iter_mut()
                .find(|info| info.model_id == model_id)
            && supports_reasoning_effort_meta(info.meta.as_ref())
        {
            let mut map = info.meta.clone().unwrap_or_default();
            map.insert(
                REASONING_EFFORT_META_KEY.to_string(),
                reasoning_effort_meta_value(override_effort),
            );
            info.meta = Some(map);
        }
        acp::SessionModelState::new(model_id, available_models)
    }
    pub(super) fn session_config_options(
        &self,
        session_id: Option<&acp::SessionId>,
        state: &acp::SessionModelState,
    ) -> Vec<session_config::SessionConfigOption> {
        let model_id = resolve_catalog_key(
                &self.models_manager.models(),
                &state.current_model_id,
            )
            .unwrap_or_else(|| state.current_model_id.clone());
        let supports_effort = self
            .models_manager
            .model_supports_reasoning_effort(model_id.0.as_ref());
        let effort_options: Vec<ReasoningEffortOption> = if supports_effort {
            let options = self
                .models_manager
                .model_reasoning_efforts(model_id.0.as_ref());
            if options.is_empty() {
                session_config::legacy_session_effort_options()
            } else {
                options
            }
        } else {
            Vec::new()
        };
        let current_effort = if supports_effort {
            session_id
                .and_then(|sid| {
                    self.sessions.borrow().get(sid).map(|h| h.reasoning_effort)
                })
                .flatten()
                .or_else(|| self.models_manager.current_reasoning_effort())
                .or_else(|| {
                    self
                        .models_manager
                        .model_default_reasoning_effort(model_id.0.as_ref())
                })
        } else {
            None
        };
        session_config::build_session_config_options(
            &state.available_models,
            &model_id,
            &effort_options,
            current_effort,
        )
    }
    /// Build the `x.ai/sessionConfig` and `x.ai/sessionDetail` `_meta` values
    /// shared by `new_session` and `load_session`, returned as
    /// `(sessionConfig, sessionDetail)`. Keeping both response paths on this one
    /// builder stops them drifting.
    pub(super) fn session_config_meta(
        &self,
        session_id: &acp::SessionId,
        cwd: String,
        title: Option<String>,
        model_state: &acp::SessionModelState,
    ) -> (serde_json::Value, serde_json::Value) {
        let config_options = self.session_config_options(Some(session_id), model_state);
        let detail = session_config::GrokSessionDetail::build(
            session_id.0.to_string(),
            cwd,
            model_state.current_model_id.0.to_string(),
            title,
        );
        (serde_json::json!({ "options": config_options }), serde_json::json!(detail))
    }
    /// Seed the global sampling config with login auth when available.
    ///
    /// Only sets the `api_key` if missing. Does NOT resolve `base_url` from
    /// `current_model_id` — that's deferred to session creation time to avoid
    /// cross-client contamination in leader mode (where `current_model_id` is
    /// shared mutable state).
    pub(super) fn seed_client_config_auth_if_available(&self) {
        let mut sampling_config = self.sampling_config.borrow_mut();
        if sampling_config.api_key.is_none() {
            if let Some(auth) = self.auth_manager.current_or_expired() {
                sampling_config.api_key = Some(auth.key);
                tracing::debug!("auth: seed_client_config set auth (SessionToken)");
                xai_grok_telemetry::unified_log::debug(
                    "auth: seed_client_config set auth (SessionToken)",
                    None,
                    None,
                );
            } else if !self
                .models_manager
                .models()
                .values()
                .any(|m| m.has_own_credentials())
            {
                tracing::warn!("No credentials found: no login token and no model api_key/env_key");
                xai_grok_telemetry::unified_log::warn(
                    "No credentials found: no login token and no model api_key/env_key",
                    None,
                    None,
                );
            }
        }
    }
    /// Build a `TraceExportConfig` for uploading JSON artifacts under a given prefix.
    ///
    /// Shared by comment uploads (`{session_id}/comments/...`),
    /// comparison metadata (`{session_id}/turn_{N}/...`), etc.
    pub(crate) async fn build_gcs_config(
        &self,
        gcs_prefix: String,
    ) -> Option<crate::session::repo_changes::TraceExportConfig> {
        let upload_method = self.trace_upload_config().await?;
        let bucket_url = {
            let cfg = self.cfg.borrow();
            match &upload_method {
                crate::session::repo_changes::UploadMethod::Direct { .. } => {
                    match cfg.endpoints.resolve_trace_bucket_url() {
                        Some(resolved) => Some(resolved.value),
                        None => {
                            tracing::debug!(
                                "no trace bucket configured; skipping direct GCS upload"
                            );
                            return None;
                        }
                    }
                }
                crate::session::repo_changes::UploadMethod::S3 { bucket, .. } => {
                    Some(format!("s3://{bucket}"))
                }
                crate::session::repo_changes::UploadMethod::Proxy { .. } => None,
            }
        };
        Some(crate::session::repo_changes::TraceExportConfig {
            bucket_url,
            service_account_key: None,
            prefix_dir: None,
            gcs_prefix: Some(gcs_prefix),
            absolute_paths: false,
            archive_name_override: None,
            upload_method,
        })
    }
    /// Allocate the next monotonic telemetry turn number for a session.
    ///
    /// Returns the current turn number and advances the counter. The counter is
    /// intentionally monotonic even across rewinds to avoid overwriting older
    /// telemetry docs in cloud storage.
    ///
    /// For sessions sharing a parent's trace counter, call this once with the
    /// **root session ID** and reuse the result so the root's counter does not
    /// advance more than once per logical turn. The cloud storage layout writes to
    /// `{session_id}/turn_{N}/`.
    pub(crate) fn allocate_turn_number(&self, session_id: &acp::SessionId) -> u64 {
        let turn = self.peek_turn_number(session_id);
        self.set_turn_number(session_id, turn.saturating_add(1));
        turn
    }
    /// Read a session's next trace turn number without advancing the counter.
    fn peek_turn_number(&self, session_id: &acp::SessionId) -> u64 {
        self.session_turn_numbers.borrow().get(session_id).copied().unwrap_or(0u64)
    }
    /// Set a session's next trace turn number. The sole writer of the
    /// `session_turn_numbers` counter, shared by `allocate_turn_number` and the
    /// batched harness-sibling allocation so both honor the same storage.
    fn set_turn_number(&self, session_id: &acp::SessionId, next: u64) {
        self.session_turn_numbers.borrow_mut().insert(session_id.clone(), next);
    }
    /// Upload each drained harness trace turn (the goal planner at setup, and
    /// each verifier skeptic panel) as its OWN sibling `turn_{N}` artifact.
    ///
    /// These phases run inside the single user-facing goal turn but are
    /// recorded out-of-band (synthetic `task` pairs in a side buffer), so the
    /// normal per-round `turn_messages.json` never references them. Giving each
    /// phase its own monotonic turn number — from the SAME `session_turn_numbers`
    /// counter the model turns use (see [`Self::allocate_turn_number`]), via
    /// [`Self::get_trace_context`] + [`upload_turn_messages`] — makes the
    /// subagents discoverable in remote/web clients
    /// via the `<subagent_result>` footer each synthetic `task` result carries.
    /// The advanced counter is persisted via `SetNextTraceTurn` so the siblings
    /// survive a restart. Best-effort and non-blocking.
    pub(super) async fn upload_harness_trace_turns(
        &self,
        session_id: &acp::SessionId,
        info: &crate::session::info::Info,
        cmd_tx: &tokio::sync::mpsc::UnboundedSender<crate::session::SessionCommand>,
        model: &str,
        turns: Vec<Vec<xai_grok_sampling_types::conversation::ConversationItem>>,
    ) {
        use crate::upload::manifest::{
            build_manifest, resolve_upload_method, write_upload_manifest,
        };
        let base = self.peek_turn_number(session_id);
        let uploads = self
            .build_harness_trace_uploads(session_id, info, model, base, turns)
            .await;
        if uploads.is_empty() {
            return;
        }
        let next_trace_turn = base.saturating_add(uploads.len() as u64);
        self.set_turn_number(session_id, next_trace_turn);
        let _ = cmd_tx
            .send(crate::session::SessionCommand::SetNextTraceTurn {
                next_trace_turn,
                request_id: None,
            });
        for (ctx, metadata, capture) in uploads {
            spawn_upload_task(
                "harness_trace_turn",
                async move {
                    let session_state = build_chat_history_session_state(
                        &capture.messages,
                    );
                    futures::join!(
                    upload_metadata(&ctx, metadata),
                    upload_turn_messages(&ctx, capture, UploadWait::Confirm),
                    upload_harness_session_archive(&ctx, session_state),
                );
                    let upload_method = resolve_upload_method(&ctx);
                    write_upload_manifest(
                            &ctx,
                            &build_manifest(&ctx.artifact_tracker, upload_method),
                        )
                        .await;
                },
            );
        }
    }
    /// Number the drained harness turns `base, base+1, …` and build their
    /// `(trace context, metadata, capture)` upload payloads. Stops at the first
    /// turn whose trace context is `None` — uploads are disabled (or the session
    /// is gone), a state uniform across the batch since all turns share one
    /// `session_id`. A `None` *after* a `Some` would be a broken invariant, so
    /// it is logged rather than dropped silently.
    pub(super) async fn build_harness_trace_uploads(
        &self,
        session_id: &acp::SessionId,
        info: &crate::session::info::Info,
        model: &str,
        base: u64,
        turns: Vec<Vec<xai_grok_sampling_types::conversation::ConversationItem>>,
    ) -> Vec<(PromptTraceContext, PromptMetadata, xai_chat_state::TurnCapture)> {
        let mut uploads = Vec::with_capacity(turns.len());
        for (offset, items) in turns.into_iter().enumerate() {
            let turn_number = base.saturating_add(offset as u64);
            let Some(ctx) = self.get_trace_context(info, turn_number).await else {
                if offset > 0 {
                    tracing::warn!(
                        turn_number,
                        "harness trace: trace context unexpectedly None mid-batch; \
                         dropping the remaining drained turns"
                    );
                }
                break;
            };
            let metadata = PromptMetadata {
                schema_version: GCS_SCHEMA_VERSION.to_string(),
                session_id: session_id.0.to_string(),
                turn_number,
                request_id: format!("harness-trace-{turn_number}"),
                turn_started_at: chrono::Utc::now().to_rfc3339(),
                repo_root: None,
                remote_url: None,
                user_id: None,
                user_email: None,
                team_id: None,
                client_source: None,
                client_version: None,
                model: model.to_string(),
                reasoning_effort: ctx
                    .session_handle
                    .reasoning_effort
                    .map(|e| e.as_str().to_string()),
                experiment_id: None,
                host_os: std::env::consts::OS.to_string(),
                host_arch: std::env::consts::ARCH.to_string(),
                prompt_has_image: Some(false),
                prompt_was_truncated: Some(false),
                prompt_verbatim: Some(true),
                cwd: Some(info.cwd.clone()),
                agent_type: None,
                shell_version: Some(xai_grok_version::VERSION.to_string()),
                workspace_type: None,
                sandbox: local_sandbox_telemetry(),
            };
            let capture = xai_chat_state::TurnCapture {
                messages: items,
                compaction_occurred: false,
            };
            uploads.push((ctx, metadata, capture));
        }
        uploads
    }
    /// Gets the trace context for a prompt using cloud storage.
    pub(crate) async fn get_trace_context(
        &self,
        session_info: &crate::session::info::Info,
        turn_number: u64,
    ) -> Option<PromptTraceContext> {
        let (upload_method, upload_reason) = self
            .trace_upload_config_with_reason()
            .await;
        {
            let mut decision = self.cfg.borrow().trace_upload_decision_debug();
            if let Some(obj) = decision.as_object_mut() {
                obj.insert(
                    "uploads_enabled".into(),
                    serde_json::json!(upload_method.is_some()),
                );
                obj.insert(
                    "upload_reason".into(),
                    serde_json::json!(upload_reason.as_str()),
                );
                obj.insert(
                    "data_collection_disabled".into(),
                    serde_json::json!(self.is_data_collection_disabled()),
                );
                obj.insert("turn_number".into(), serde_json::json!(turn_number));
            }
            xai_grok_telemetry::unified_log::info(
                "trace.upload.decision",
                Some(session_info.id.0.as_ref()),
                Some(decision),
            );
        }
        let upload_method = match upload_method {
            Some(method) => method,
            None => {
                xai_grok_telemetry::session_ctx::log_session_event(crate::agent::session_metrics::TraceUploadSkipped {
                    session_id: session_info.id.0.to_string(),
                    turn_number,
                    reason: upload_reason.as_str().to_owned(),
                });
                return None;
            }
        };
        let bucket_url = {
            let cfg = self.cfg.borrow();
            match &upload_method {
                crate::session::repo_changes::UploadMethod::Direct { .. } => {
                    match cfg.endpoints.resolve_trace_bucket_url() {
                        Some(resolved) => Some(resolved.value),
                        None => {
                            xai_grok_telemetry::session_ctx::log_session_event(crate::agent::session_metrics::TraceUploadSkipped {
                                session_id: session_info.id.0.to_string(),
                                turn_number,
                                reason: "no_trace_bucket_configured".to_owned(),
                            });
                            return None;
                        }
                    }
                }
                crate::session::repo_changes::UploadMethod::S3 { bucket, .. } => {
                    Some(format!("s3://{bucket}"))
                }
                crate::session::repo_changes::UploadMethod::Proxy { .. } => None,
            }
        };
        let gcs_config = crate::session::repo_changes::TraceExportConfig {
            bucket_url,
            service_account_key: None,
            prefix_dir: None,
            gcs_prefix: Some(format!("{}/turn_{}", session_info.id.0, turn_number)),
            absolute_paths: false,
            archive_name_override: None,
            upload_method,
        };
        let session_handle = match self.sessions.borrow().get(&session_info.id) {
            Some(h) => h.clone(),
            None => {
                return None;
            }
        };
        let queue = session_handle
            .upload_queue
            .get_or_init(|| {
                let grok_home = crate::util::grok_home::grok_home();
                let queue = crate::upload::trace::spawn_upload_queue(
                    &grok_home,
                    &gcs_config,
                    Some(xai_grok_version::VERSION),
                    self.auth_manager.clone(),
                );
                crate::upload::trace::spawn_startup_spill_reconcile(
                    grok_home,
                    Some(queue.clone()),
                );
                session_handle
                    .feedback_manager
                    .set_upload_queue_stats(queue.stats_arc());
                queue
            });
        let upload_queue = Some(queue.clone());
        let session_registry_enabled = self.build_registry_config().is_some();
        Some(PromptTraceContext {
            gcs_config,
            session_info: session_info.clone(),
            turn_number,
            session_handle,
            session_registry_enabled,
            upload_queue,
            artifact_tracker: crate::upload::manifest::new_artifact_tracker(),
            auth_manager: self.auth_manager.clone(),
        })
    }
    /// Resolve the agent definition for a session.
    ///
    /// Priority (highest to lowest):
    /// 1. Model `agent_type` if it names a strict harness (codex, …).
    /// 2. `acp_agent_profile` from ACP `_meta.agentProfile` (remote clients).
    /// 3. `agent_profile_path` from CLI `--agent-profile`.
    /// 4. `agent_config` from config.toml `[agent]`.
    /// 5. `GROK_AGENT` env var.
    /// 6. Built-in default agent.
    ///
    /// `GROK_AGENT` and an explicit `[agent] name` bypass step 1.
    /// Strict-harness classification is structural — see
    /// [`xai_grok_agent::config::is_strict_harness_agent_type`].
    ///
    /// Harness inheritance for a profile that pins its own model is applied by
    /// the caller via [`inherited_harness_template`], not here.
    pub fn resolve_agent_definition(
        cwd: &std::path::Path,
        agent_profile_path: Option<&std::path::Path>,
        agent_config: &config::AgentSelectionConfig,
        acp_agent_profile: Option<xai_grok_agent::AgentDefinition>,
        model_agent_type: Option<&str>,
    ) -> xai_grok_agent::AgentDefinition {
        use xai_grok_agent::AgentDefinition;
        let grok_agent_env_set = std::env::var("GROK_AGENT")
            .ok()
            .is_some_and(|s| !s.trim().is_empty());
        let config_agent_explicitly_set = agent_config.name.is_some();
        let model_requires_strict_harness = model_agent_type
            .is_some_and(xai_grok_agent::config::is_strict_harness_agent_type);
        if !grok_agent_env_set && !config_agent_explicitly_set
            && model_requires_strict_harness && let Some(required) = model_agent_type
            && let Some(def) = xai_grok_agent::discovery::by_name_in_cwd(required, cwd)
        {
            tracing::info!(
                agent_name = %def.name,
                "Using agent definition from model agent_type"
            );
            return def;
        }
        if let Some(def) = acp_agent_profile {
            tracing::info!(
                agent_name = %def.name,
                "Using agent profile from ACP _meta.agentProfile"
            );
            return def;
        }
        if let Some(path) = agent_profile_path {
            match AgentDefinition::from_file(path) {
                Ok(def) => return def,
                Err(e) => {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "Failed to load agent profile from --agent-profile path"
                    );
                    eprintln!(
                        "error: failed to load agent profile '{}': {}",
                        path.display(),
                        e
                    );
                    crate::instrumentation::finalize_and_exit(1);
                }
            }
        }
        if let Some(ref path) = agent_config.definition {
            match AgentDefinition::from_file(path) {
                Ok(def) => {
                    tracing::info!(
                        agent_name = %def.name,
                        path = %path.display(),
                        "Using agent definition from config.toml [agent] definition"
                    );
                    return def;
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to load agent definition from config.toml [agent] definition, \
                         falling through to next source"
                    );
                }
            }
        }
        if let Some(ref name) = agent_config.name {
            tracing::info!(
                agent_name = %name,
                "Resolving agent definition from config.toml [agent] name"
            );
            if let Some(def) = xai_grok_agent::discovery::by_name_in_cwd(name, cwd) {
                return def;
            }
            tracing::warn!(
                agent_name = %name,
                "Agent '{}' not found via discovery, falling through to next source",
                name
            );
        }
        let agent_name = std::env::var("GROK_AGENT").ok();
        let resolved = match agent_name.as_deref() {
            Some("browser-use") | Some("browser_use") => AgentDefinition::browser_use(),
            Some("grok-build-concise") | Some("grok_build_concise") => {
                AgentDefinition::grok_build_concise()
            }
            Some(path) if std::path::Path::new(path).is_absolute() => {
                match AgentDefinition::from_file(path) {
                    Ok(def) => def,
                    Err(e) => {
                        tracing::warn!(
                            path = path,
                            error = %e,
                            "Failed to load agent definition from file, falling back to default"
                        );
                        AgentDefinition::grok_build_plan()
                    }
                }
            }
            Some(name) => {
                xai_grok_agent::discovery::by_name_in_cwd(name, cwd)
                    .unwrap_or_else(AgentDefinition::grok_build_plan)
            }
            None => AgentDefinition::grok_build_plan(),
        };
        if !grok_agent_env_set && !config_agent_explicitly_set
            && model_requires_strict_harness && let Some(required) = model_agent_type
            && resolved.name != required
        {
            tracing::info!(
                resolved_agent = %resolved.name,
                model_agent_type = %required,
                "resolve_agent_definition: model requires different agent, re-resolving"
            );
            if let Some(def) = xai_grok_agent::discovery::by_name_in_cwd(required, cwd) {
                return def;
            }
            tracing::warn!(
                model_agent_type = %required,
                fallback_agent = %resolved.name,
                "resolve_agent_definition: model agent_type '{}' not found via discovery, \
                 keeping chain-resolved agent",
                required,
            );
        }
        resolved
    }
    /// Extract per-client terminal/fs capabilities from request `_meta`
    /// (injected by the leader). Falls back to the shared `init` OnceCell.
    pub(super) fn resolve_client_io_caps(
        meta: Option<&acp::Meta>,
        init: &acp::InitializeRequest,
    ) -> (bool, bool, bool) {
        let terminal = meta
            .and_then(|m| m.get("clientTerminal"))
            .and_then(|v| v.as_bool())
            .unwrap_or(init.client_capabilities.terminal);
        let fs_read = meta
            .and_then(|m| m.get("clientFsRead"))
            .and_then(|v| v.as_bool())
            .unwrap_or(init.client_capabilities.fs.read_text_file);
        let fs_write = meta
            .and_then(|m| m.get("clientFsWrite"))
            .and_then(|v| v.as_bool())
            .unwrap_or(init.client_capabilities.fs.write_text_file);
        (terminal, fs_read, fs_write)
    }
    /// Spawn and register a session actor given a session id and session parameters.
    ///
    /// Parameters are bundled in [`SessionSpawnOptions`] (named fields) rather than
    /// passed positionally: there are too many same-typed args (`bool`s,
    /// `Option<…>`s) for positional calls to be transposition-safe.
    pub(super) async fn spawn_and_register_session(
        &self,
        init: &acp::InitializeRequest,
        spec: SessionSpawnOptions<'_>,
    ) -> Result<(), acp::Error> {
        let SessionSpawnOptions {
            session_info,
            cwd,
            mcp_servers,
            initial_client_mcp_servers,
            mcp_meta_config_map,
            persistence,
            mut chat_history,
            rewind_points_file_path,
            initial_total_tokens,
            origin_client: _origin_client,
            client_code_nav_enabled,
            client_terminal,
            client_fs_read,
            client_fs_write,
            preloaded_envrc,
            persisted_signals,
            persisted_plan_mode,
            persisted_goal_mode,
            persisted_workflow_runs,
            persisted_announcement_state,
            session_meta,
            managed_mcp_expires_at,
            model_agent_type,
            session_model_id,
            session_yolo_mode,
            session_auto_mode,
            prompt_display_cwd,
        } = spec;
        let _timer = crate::instrumentation_timer!("session.spawn_and_register");
        reject_direct_hub_cloud_meta(session_meta)?;
        let spawn_remote_settings = self.cfg.borrow().remote_settings.clone();
        folder_trust::resolve_and_record(
            cwd.as_path(),
            spawn_remote_settings.as_ref(),
            false,
        );
        let use_acp_fs = client_fs_read && client_fs_write;
        let fs_notify_config = init
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/fs_notify"))
            .and_then(|v| {
                use crate::session::{ClientFsConfig, ClientFsMode};
                use xai_fsnotify::FsConfig;
                if v.as_bool() == Some(true) {
                    return Some(ClientFsConfig::default());
                }
                let obj = v.as_object()?;
                if obj.get("enabled").and_then(|e| e.as_bool()) == Some(false) {
                    return None;
                }
                let mode = if obj.get("index").and_then(|i| i.as_bool()) == Some(true) {
                    ClientFsMode::Index
                } else {
                    ClientFsMode::Events
                };
                let mut fs = FsConfig::default();
                if let Some(ms) = obj.get("debounce_ms").and_then(|v| v.as_u64()) {
                    fs.debounce_ms = ms;
                }
                if let Some(patterns) = obj.get("ignore").and_then(|v| v.as_array()) {
                    fs.ignore_patterns = patterns
                        .iter()
                        .filter_map(|p| p.as_str().map(String::from))
                        .collect();
                }
                Some(ClientFsConfig { fs, mode })
            });
        let fs: Arc<dyn xai_grok_workspace::file_system::AsyncFileSystem> = if use_acp_fs {
            let mut acp_fs = AcpSessionFs::new(
                cwd.to_path_buf(),
                session_info.id.clone(),
                self.gateway.clone(),
            );
            if let Some(ref display) = prompt_display_cwd {
                acp_fs = acp_fs.with_display_cwd(std::path::PathBuf::from(display));
            }
            Arc::new(acp_fs)
        } else {
            Arc::new(LocalFs::new(cwd.to_path_buf()))
        };
        let gateway_enabled = std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(true),
        );
        let terminal: std::sync::Arc<dyn crate::terminal::AsyncTerminalRunner> = if client_terminal {
            std::sync::Arc::new(AcpTerminalRunner {
                gateway: self.gateway.clone(),
                session_id: session_info.id.clone(),
            })
        } else {
            let notifier: std::sync::Arc<
                dyn crate::terminal::SessionNotificationSender,
            > = std::sync::Arc::new(
                crate::terminal::GatedNotifier::new(
                    std::sync::Arc::new(self.gateway.clone()),
                    gateway_enabled.clone(),
                ),
            );
            std::sync::Arc::new(TerminalRunner::new(notifier, session_info.id.clone()))
        };
        let load_envrc = self.cfg.borrow().session.load_envrc.unwrap_or(true);
        let startup_hints = init
            .meta
            .as_ref()
            .and_then(|m| m.get("startupHints"))
            .and_then(|v| {
                serde_json::from_value::<crate::session::StartupHints>(v.clone()).ok()
            })
            .unwrap_or_default();
        let hunk_plan = plan_hunk_tracking(
            init
                .client_capabilities
                .meta
                .as_ref()
                .and_then(|m| m.get("x.ai/hunkTracker"))
                .and_then(|v| v.get("mode"))
                .and_then(|v| v.as_str()),
        );
        let incremental_bash_output = init
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/incrementalBashOutput"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let no_color = init
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/bashOutputNoColor"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let hunk_tracking_enabled = hunk_plan.enabled();
        let (hunk_tracker_handle, hunk_event_rx) = match hunk_plan.actor_mode {
            Some(mode) => {
                let cancel = CancellationToken::new();
                let (hunk_event_tx, hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
                let handle = HunkTrackerActor::spawn(
                    session_info.id.0.to_string(),
                    cwd.as_path().to_path_buf(),
                    hunk_event_tx,
                    mode,
                    cancel.clone(),
                );
                (handle, Some((hunk_event_rx, cancel)))
            }
            None => (xai_hunk_tracker::HunkTrackerHandle::noop(), None),
        };
        let has_xai_auth = self.auth_manager.current().is_some_and(|a| a.is_xai_auth());
        let loc_tracking_enabled = hunk_tracking_enabled && has_xai_auth
            && (self
                .cfg
                .borrow()
                .remote_settings
                .as_ref()
                .and_then(|s| s.loc_tracking)
                .unwrap_or(false)
                || std::env::var("GROK_LOC_TRACKING")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false));
        let (feedback_resolved, feedback_flags) = {
            let cfg = self.cfg.borrow();
            let resolved = cfg.resolve_feedback();
            let flags = crate::session::feedback_manager::FeedbackFlags {
                enabled: resolved.value,
                user: cfg.feedback.user.clone(),
            };
            (resolved, flags)
        };
        tracing::info!(feedback = %feedback_resolved, "resolved feedback feature flag");
        let loc_aggregate_rx = match hunk_event_rx {
            Some((hunk_event_rx, loc_cancel)) if loc_tracking_enabled => {
                let (loc_agg_tx, loc_agg_rx) = tokio::sync::mpsc::unbounded_channel();
                let loc_path = crate::session::persistence::session_dir(&session_info)
                    .join("hunk_records.jsonl");
                let loc_writer = xai_hunk_tracker::JsonlHunkRecordWriter::new(loc_path);
                let loc_ctx = xai_hunk_tracker::LocSinkContext {
                    session_id: session_info.id.0.to_string(),
                    agent_id: agent_id(),
                    user_id: self.auth_manager.current().map(|a| a.user_id.clone()),
                    aggregate_tx: Some(loc_agg_tx),
                };
                tokio::spawn(
                    xai_hunk_tracker::run_loc_sink(
                        hunk_event_rx,
                        loc_writer,
                        loc_ctx,
                        loc_cancel,
                    ),
                );
                Some(loc_agg_rx)
            }
            _ => None,
        };
        let project_env_trusted = folder_trust::project_scope_allowed(cwd.as_path());
        let mut session_env = xai_grok_workspace::permission::claude_settings::load_claude_env_with_project(
            cwd.as_path(),
            project_env_trusted,
        );
        let envrc = match preloaded_envrc {
            Some(env) => env,
            None => {
                xai_grok_workspace::envrc::load_envrc_or_empty_when_trusted(
                    cwd.as_path(),
                    load_envrc && project_env_trusted,
                )
            }
        };
        session_env.extend(envrc);
        if no_color {
            session_env.extend(crate::terminal::no_color_env());
        } else {
            session_env.extend(crate::terminal::color_env());
        }
        let mut tool_ctx = ToolContext::with_preloaded_env(
                cwd.clone(),
                Some(self.gateway.clone()),
                Some(session_info.id.clone()),
                fs,
                terminal,
                hunk_tracker_handle,
                session_env,
            )
            .with_hunk_tracking_enabled(hunk_tracking_enabled);
        let workspace_ops = self
            .resolve_workspace_ops()
            .map_err(|_| {
                acp::Error::internal_error()
                    .data(
                        "Local workspace initialization failed; cannot create session. \
                 Check that a Tokio runtime is available.",
                    )
            })?;
        tool_ctx.subagent_event_tx = Some(self.subagent_event_tx.clone());
        tool_ctx.synthetic_trace_tx = self
            .subagent_coordinator
            .borrow()
            .synthetic_trace_tx
            .clone();
        if let Some(ref shared) = tool_ctx.synthetic_trace_tx_shared {
            *shared.lock().unwrap_or_else(|e| e.into_inner()) = self
                .subagent_coordinator
                .borrow()
                .synthetic_trace_tx
                .clone();
        }
        tool_ctx.is_turn_active = Some(
            self.subagent_coordinator.borrow().turn_active_flag(),
        );
        tool_ctx.monitor_event_buffer = Some(self.monitor_event_buffer.clone());
        tool_ctx.subagent_depth = 0;
        tool_ctx.auto_wake_enabled = self.cfg.borrow().auto_wake_enabled;
        let support_permission = self.cfg.borrow().features.support_permission;
        let telemetry_enabled = self.product_analytics_enabled();
        let origin_client = self.origin_client_info_from_meta(init.meta.as_ref());
        let sampling_config = self
            .resolve_sampling_config_for_model(&session_model_id, origin_client.clone());
        if self.auth_method_id.load().is_none() {
            return Err(acp::Error::auth_required().data("no auth method id provided"));
        }
        let auth_method_id = std::sync::Arc::clone(&self.auth_method_id);
        tracing::info!(
            session_id = %session_info.id.0,
            ?startup_hints,
            "startup hints"
        );
        let auto_compact_threshold_percent = {
            let cfg = self.cfg.borrow();
            let models = self.models_manager.models();
            let model = config::find_model_by_id(&models, &session_model_id.0);
            crate::util::config::resolve_auto_compact_threshold_percent(
                &cfg,
                &session_model_id.0,
                model.map(|e| &e.info),
            )
        };
        let system_prompt_label = {
            let cfg = self.cfg.borrow();
            let models = self.models_manager.models();
            let model = config::find_model_by_id(&models, &session_model_id.0);
            crate::util::config::resolve_system_prompt_label(
                &cfg,
                &session_model_id.0,
                model.map(|e| &e.info),
            )
        };
        let compaction_mode = self.cfg.borrow().resolve_compaction_mode();
        let compaction_verbatim_input = self
            .cfg
            .borrow()
            .resolve_compaction_verbatim_input();
        let compaction_tool_choice = self.cfg.borrow().resolve_compaction_tool_choice();
        let two_pass_enabled = self.cfg.borrow().is_two_pass_compaction_enabled();
        let auto_update = self.cfg.borrow().cli.auto_update;
        let client_type = *self.client_type.borrow();
        let buffering_settings = self.buffering_settings.borrow().clone();
        let (
            feedback_proxy_url,
            feedback_user_token,
            feedback_alpha_test_key,
            deployment_key,
        ) = if let Some((url, token, alpha, deploy)) = self.feedback_credentials() {
            (Some(url), token, alpha, deploy)
        } else {
            (None, None, None, None)
        };
        tracing::info!(
            session_id = %session_info.id.0,
            feedback_url = ?feedback_proxy_url,
            authenticated = feedback_user_token.is_some(),
            "Initializing feedback manager for session"
        );
        let skills = self.cfg.borrow().skills.clone();
        let compat = self.cfg.borrow().compat_resolved;
        let acp_agent_profile = parse_agent_profile_from_meta(session_meta);
        let session_default_agent_profile = acp_agent_profile
            .as_ref()
            .map(|d| d.name.clone());
        let mut agent_definition = {
            let cfg = self.cfg.borrow();
            Self::resolve_agent_definition(
                cwd.as_path(),
                cfg.agent_profile_path.as_deref(),
                &cfg.agent,
                acp_agent_profile,
                model_agent_type,
            )
        };
        {
            let cfg = self.cfg.borrow();
            let overrides = &cfg.cli_agent_overrides;
            overrides.apply_to_definition(&mut agent_definition);
            if overrides.has_definition_overrides() {
                tracing::debug!(
                    agent = %agent_definition.name,
                    tools = ?overrides.tools,
                    disallowed = ?overrides.disallowed_tools,
                    permission_mode = ?overrides.permission_mode,
                    "cli agent overrides applied"
                );
            }
        }
        let pinned_model: Option<(acp::ModelId, ModelEntry)> = match &agent_definition
            .model
        {
            xai_grok_agent::config::ModelOverride::Override(id) => {
                let mid = acp::ModelId::new(Arc::from(id.as_str()));
                match self.resolve_model_id(&mid) {
                    Ok(entry) => Some((mid, entry)),
                    Err(_) => {
                        tracing::warn!(
                            agent = %agent_definition.name,
                            model = %id,
                            "agent profile model not in catalog, keeping session default"
                        );
                        None
                    }
                }
            }
            xai_grok_agent::config::ModelOverride::Inherit => None,
        };
        if let Some(template) = inherited_harness_template(
            &agent_definition.user_message_template,
            pinned_model.as_ref().map(|(_, e)| e.info().agent_type.as_str()),
            cwd.as_path(),
        ) {
            tracing::info!(
                agent = %agent_definition.name,
                "Inheriting harness wire-format from the profile model's agent_type"
            );
            agent_definition.user_message_template = template;
        }
        let (session_model_id, sampling_config) = self
            .apply_agent_model_override(
                pinned_model.as_ref(),
                session_model_id,
                sampling_config,
                origin_client.clone(),
            );
        let max_turns = {
            let cfg = self.cfg.borrow();
            cfg.cli_agent_overrides
                .max_turns
                .or(agent_definition.max_turns)
                .map(|v| v as usize)
        };
        {
            let cfg = self.cfg.borrow();
            let effective = cfg
                .toolset
                .resolve_file_toolset(cfg.remote_settings.as_ref());
            if effective != crate::tools::FileToolset::Standard {
                let file_tools = effective
                    .tool_configs(&cfg.toolset.hashline)
                    .map_err(|e| {
                        acp::Error::invalid_params()
                            .data(format!("invalid [toolset.hashline] config: {e}"))
                    })?;
                agent_definition.override_file_tools(file_tools);
            }
        }
        let lsp_tools_enabled = self.cfg.borrow().resolve_lsp_tools().value;
        if lsp_tools_enabled && tool_ctx.lsp.is_none() {
            let snapshot = self.plugin_registry_handle.snapshot();
            let active: Vec<_> = snapshot
                .iter()
                .flat_map(|reg| reg.active_plugins())
                .collect();
            let (plugin_lsp_paths, plugin_names): (Vec<std::path::PathBuf>, Vec<&str>) = active
                .iter()
                .filter_map(|p| {
                    p.lsp_config_path.clone().map(|path| (path, p.name.as_str()))
                })
                .unzip();
            let (
                plugin_inline_lsp,
                inline_names,
            ): (Vec<&serde_json::Value>, Vec<&str>) = active
                .iter()
                .filter_map(|p| {
                    p.inline_lsp_servers.as_ref().map(|v| (v, p.name.as_str()))
                })
                .unzip();
            let sourced = xai_grok_tools::implementations::lsp::config::load_servers_with_plugins_sourced(
                tool_ctx.cwd.as_path(),
                &plugin_lsp_paths,
                &plugin_inline_lsp,
                &plugin_names,
                &inline_names,
            );
            let servers = folder_trust::filter_untrusted_project_lsp(
                tool_ctx.cwd.as_path(),
                sourced,
            );
            tool_ctx.lsp_server_names = servers.keys().cloned().collect();
            if servers.is_empty() {
                let user_path = xai_grok_tools::util::grok_home::grok_home()
                    .join("lsp.json");
                let project_path = tool_ctx.cwd.as_path().join(".grok").join("lsp.json");
                tracing::warn!(
                    cwd = %tool_ctx.cwd,
                    user_lsp_path = %user_path.display(),
                    project_lsp_path = %project_path.display(),
                    "LSP tools enabled, but no language servers are configured"
                );
            } else {
                use xai_grok_tools::implementations::lsp::{
                    LspBackend, LspBackendAdapter, LspManager,
                };
                let mgr = std::sync::Arc::new(
                    tokio::sync::Mutex::new(
                        LspManager::new(
                            servers,
                            tool_ctx.cwd.as_path().to_path_buf(),
                            true,
                            xai_grok_tools::notification::ToolNotificationHandle::noop(),
                        ),
                    ),
                );
                let adapter = std::sync::Arc::new(LspBackendAdapter::new(mgr));
                adapter.ensure_started_background();
                tool_ctx.lsp = Some(adapter as std::sync::Arc<dyn LspBackend>);
            }
        }
        let inference_idle_timeout_secs = {
            let models = self.models_manager.models();
            let cfg = self.cfg.borrow();
            resolve_inference_idle_timeout_secs(
                &models,
                &sampling_config.model,
                cfg.remote_settings.as_ref(),
            )
        };
        let model_max_retries = self
            .models_manager
            .models()
            .values()
            .find(|entry| entry.info.model == sampling_config.model)
            .and_then(|entry| entry.info.max_retries);
        let origin_client = self.origin_client_info_from_meta(init.meta.as_ref());
        let web_search_sampling_config = self.prepare_web_search_sampling_config();
        let image_gen_config = self.prepare_image_gen_config();
        let video_gen_config = self.prepare_video_gen_config();
        let app_builder_deployer_config = self.prepare_app_builder_deployer_config();
        let web_fetch_config = self.prepare_web_fetch_config();
        let write_file_enabled = self.cfg.borrow().resolve_write_file().value;
        let goal_enabled = self.cfg.borrow().resolve_goal().value;
        let background_workflows_enabled = self.cfg.borrow().resolve_workflows().value;
        let subagents_enabled = self.cfg.borrow().subagents_enabled;
        let ask_user_question_enabled = crate::upload::turn::parse_ask_user_question_from_meta(
                session_meta,
            )
            .unwrap_or_else(|| self.cfg.borrow().resolve_ask_user_question().value);
        let client_hooks = crate::extensions::hooks::parse_client_hooks(session_meta);
        let disable_web_search = self.cfg.borrow().disable_web_search;
        let todo_gate = self.cfg.borrow().todo_gate;
        let remote_settings_for_spawn = self.cfg.borrow().remote_settings.clone();
        let laziness_debug_log_for_spawn = self.cfg.borrow().laziness_debug_log.clone();
        let respect_gitignore = self.cfg.borrow().respect_gitignore;
        let path_not_found_hints = self.cfg.borrow().path_not_found_hints;
        let subagent_toggle = self.cfg.borrow().subagent_toggle.clone();
        let handle_display_cwd = prompt_display_cwd.clone();
        let auth_manager = Some(self.auth_manager.clone());
        let bash_params_json = {
            let cfg = self.cfg.borrow();
            let remote_auto_bg = cfg
                .remote_settings
                .as_ref()
                .and_then(|r| r.auto_background_on_timeout);
            let remote_allow_background_operator = cfg
                .remote_settings
                .as_ref()
                .and_then(|r| r.allow_background_operator);
            cfg.toolset
                .bash
                .to_bash_params_json(remote_auto_bg, remote_allow_background_operator)
        };
        let ask_user_question_params_json = {
            let cfg = self.cfg.borrow();
            let params = crate::util::config::resolve_ask_user_question_params_from_disk(
                cfg.remote_settings.as_ref(),
            );
            match serde_json::to_value(params) {
                Ok(serde_json::Value::Object(map)) => Some(map),
                _ => None,
            }
        };
        let tool_params_json = crate::session::agent_rebuild::ResolvedToolParamsJson {
            bash: Some(bash_params_json),
            ask_user_question: ask_user_question_params_json,
        };
        let backend_tools_enabled = {
            let cfg = self.cfg.borrow();
            cfg.resolve_backend_tools().value
        };
        let managed_mcp_proxy_url = self.cfg.borrow().endpoints.proxy_url();
        let init_meta = self
            .initialize_request
            .get()
            .and_then(|init| init.meta.as_ref());
        if let Some(override_prompt) = system_prompt_override_from_meta(
            session_meta,
            init_meta,
        ) && !chat_history.is_empty() && !startup_hints.preserve_inherited_system
        {
            let changed = replace_or_insert_system_head(
                &mut chat_history,
                override_prompt,
            );
            if changed {
                tracing::info!(
                    session_id = %session_info.id.0,
                    prompt_len = override_prompt.len(),
                    "cold-load: applied systemPromptOverride to loaded head"
                );
            } else {
                tracing::debug!(
                    session_id = %session_info.id.0,
                    "cold-load: systemPromptOverride already matches head, no-op"
                );
            }
        }
        let (mut handle, permission_events_rx, agent_system_prompt, session_thread) = {
            let _timer = crate::instrumentation_timer!("session.spawn_actor_call");
            let session_key = self.auth_manager.current_or_expired().map(|a| a.key);
            let credentials = xai_chat_state::Credentials {
                api_key: sampling_config.api_key.clone(),
                auth_type: crate::agent::config::resolve_chat_state_auth_type(
                    sampling_config.model.as_str(),
                    session_key.as_deref(),
                    self.auth_type(),
                ),
                alpha_test_key: self.alpha_test_key(),
                client_version: sampling_config.client_version.clone(),
            };
            let attribution_callback: Option<
                xai_grok_sampler::SharedAttributionCallback,
            > = Some(
                crate::auth::attribution::ShellAttribution::new(
                    self.auth_manager.clone(),
                    Some(session_info.id.0.to_string()),
                ),
            );
            let agent_hook_registry_override = agent_definition
                .hooks
                .as_ref()
                .and_then(|hooks_config| {
                    let hooks_val = hooks_config.as_value();
                    let (specs, errors) = xai_grok_hooks::config::parse_hooks_from_value_with_dir(
                        &hooks_val,
                        &format!("agent:{}", agent_definition.name),
                        std::path::Path::new(&session_info.cwd),
                    );
                    for e in &errors {
                        tracing::warn!(agent = %agent_definition.name, error = ?e, "agent hook parse error");
                    }
                    if specs.is_empty() {
                        return None;
                    }
                    let cwd = std::path::Path::new(&session_info.cwd);
                    let hooks_trusted = folder_trust::project_scope_allowed(cwd);
                    let git_root = xai_grok_workspace::session::git::find_git_root_from_path(
                            cwd,
                        )
                        .ok();
                    let (disk_registry, disk_errors) = crate::util::hooks::discover_hooks(
                        git_root.as_deref(),
                        &compat,
                        hooks_trusted,
                    );
                    for e in &disk_errors {
                        tracing::warn!(error = ?e, "hook loading error");
                    }
                    let mut merged = disk_registry;
                    if folder_trust::agent_inline_hooks_allowed(
                        agent_definition.scope,
                        || hooks_trusted,
                    ) {
                        merged.append_specs(specs);
                    }
                    Some(std::sync::Arc::new(merged))
                });
            let initial_reasoning_effort = chat_history
                .is_empty()
                .then_some(sampling_config.reasoning_effort);
            let _ = persistence
                .tx
                .send(crate::session::persistence::PersistenceMsg::CurrentModel {
                    model_id: session_model_id.clone(),
                    agent_name: Some(agent_definition.name.clone()),
                    reasoning_effort: initial_reasoning_effort,
                });
            let acp_mcp_servers = crate::session::acp_mcp::parse_acp_mcp_servers(
                session_meta,
            );
            let git_head_changed = init
                .client_capabilities
                .meta
                .as_ref()
                .and_then(|m| m.get("x.ai/gitHeadChanged"))
                .and_then(|v| v.as_bool());
            let session_cwd = std::path::Path::new(&session_info.cwd);
            let fs_watch_caps = crate::session::fs_watch::FsWatchCapabilities::resolve(crate::session::fs_watch::CapabilityInputs {
                client_notify: fs_notify_config.is_some(),
                hunk_tracking: hunk_plan.enabled(),
                code_nav: client_code_nav_enabled,
                git_head_changed,
            });
            spawn_session_on_thread(
                    session_info.clone(),
                    self.gateway.clone(),
                    sampling_config,
                    credentials,
                    auth_method_id,
                    auth_manager,
                    attribution_callback,
                    tool_ctx,
                    mcp_servers,
                    initial_client_mcp_servers,
                    mcp_meta_config_map,
                    None,
                    acp_mcp_servers,
                    support_permission,
                    telemetry_enabled,
                    auto_update,
                    persistence,
                    chat_history.clone(),
                    rewind_points_file_path,
                    fs_notify_config,
                    initial_total_tokens,
                    startup_hints,
                    client_type,
                    auto_compact_threshold_percent,
                    system_prompt_label,
                    compaction_mode,
                    compaction_verbatim_input,
                    compaction_tool_choice,
                    two_pass_enabled,
                    buffering_settings,
                    origin_client.clone(),
                    self.codebase_indexes.clone(),
                    client_code_nav_enabled,
                    fs_watch_caps,
                    feedback_proxy_url,
                    feedback_user_token,
                    feedback_alpha_test_key,
                    deployment_key,
                    client_terminal,
                    client_fs_read && client_fs_write,
                    gateway_enabled,
                    agent_definition,
                    session_default_agent_profile,
                    skills,
                    None,
                    compat,
                    incremental_bash_output,
                    persisted_signals,
                    persisted_plan_mode,
                    persisted_goal_mode,
                    persisted_workflow_runs,
                    persisted_announcement_state,
                    self.memory_config.clone(),
                    loc_tracking_enabled,
                    feedback_flags,
                    self.managed_mcp_cache.clone(),
                    managed_mcp_expires_at,
                    managed_mcp_proxy_url,
                    session_model_id,
                    session_yolo_mode,
                    session_auto_mode,
                    origin_client.as_ref().map(|o| o.product.clone()),
                    inference_idle_timeout_secs,
                    model_max_retries,
                    web_search_sampling_config,
                    web_fetch_config,
                    image_gen_config,
                    video_gen_config,
                    app_builder_deployer_config,
                    write_file_enabled,
                    goal_enabled,
                    background_workflows_enabled,
                    subagents_enabled,
                    ask_user_question_enabled,
                    client_hooks,
                    prompt_display_cwd,
                    subagent_toggle,
                    Vec::new(),
                    xai_grok_agent::prompt::context::PromptAudience::Primary,
                    None,
                    None,
                    disable_web_search,
                    backend_tools_enabled,
                    respect_gitignore,
                    path_not_found_hints,
                    tool_params_json,
                    {
                        let disk_cfg = crate::config::resolve_effective_plugins_config(
                                session_cwd,
                            )
                            .to_discovery_config();
                        self.plugin_registry_handle
                            .refresh_and_build_for_cwd(
                                session_cwd,
                                &disk_cfg,
                                &parse_session_plugin_dirs(session_meta),
                                folder_trust::project_scope_allowed(session_cwd),
                            )
                    },
                    Some(self.plugin_registry_handle.clone()),
                    self.models_manager.clone(),
                    None,
                    None,
                    Some(
                        Arc::new(
                            crate::auth::manager::SharedAuthKeyProvider(
                                self.auth_manager.clone(),
                            ),
                        ),
                    ),
                    self.resolve_image_description_model(),
                    agent_hook_registry_override,
                    workspace_ops.clone(),
                    {
                        let cfg = self.cfg.borrow();
                        cfg.cli_agent_overrides.permission_rules.clone()
                    },
                    todo_gate,
                    remote_settings_for_spawn,
                    laziness_debug_log_for_spawn,
                    None,
                    None,
                    max_turns,
                    None,
                )
                .await?
        };
        self.session_threads
            .borrow_mut()
            .insert(session_info.id.clone(), session_thread);
        tracing::debug!(session_id = %session_info.id.0, "spawn_session_on_thread complete");
        self.set_session_live_state(&session_info.id, SessionLiveState::IdleResident);
        self.ensure_session_supervisor();
        self.heap_profile_set_session_id(&session_info.id.0);
        self.push_roster_delta_upserted(&session_info.id);
        if chat_history.is_empty() {
            let _timer = crate::instrumentation_timer!("session.system_prompt_inject");
            let system_prompt = build_spawn_system_prompt(
                session_meta,
                init_meta,
                &agent_system_prompt,
            );
            tracing::debug!(
                session_id = %session_info.id.0,
                "built system prompt"
            );
            let _ = handle
                .cmd_tx
                .send(SessionCommand::Initialize {
                    system_prompt,
                });
            tracing::debug!(session_id = %session_info.id.0, "enqueued SessionCommand::Initialize");
        }
        let _ = handle.cmd_tx.send(SessionCommand::AdvertiseCommands);
        if let Some(mut loc_rx) = loc_aggregate_rx {
            let signals = handle.signals_handle.clone();
            tokio::spawn(async move {
                while let Some(agg) = loc_rx.recv().await {
                    match agg {
                        xai_hunk_tracker::LocAggregate::LinesChanged {
                            author_type,
                            lines_added,
                            lines_removed,
                            file_path,
                        } => {
                            let is_agent = author_type
                                == xai_hunk_tracker::AuthorType::Agent;
                            signals
                                .record_loc_change(
                                    is_agent,
                                    lines_added,
                                    lines_removed,
                                    file_path,
                                );
                        }
                        xai_hunk_tracker::LocAggregate::LinesReverted {
                            lines_added_reverted,
                            lines_removed_reverted,
                        } => {
                            signals
                                .record_loc_revert(
                                    lines_added_reverted,
                                    lines_removed_reverted,
                                );
                        }
                    }
                }
            });
        }
        self.permission_event_receivers
            .borrow_mut()
            .insert(session_info.id.clone(), permission_events_rx);
        if handle_display_cwd.is_some() {
            handle.display_cwd = handle_display_cwd;
        }
        let source = if chat_history.is_empty() { "new" } else { "load" };
        let _ = handle
            .cmd_tx
            .send(SessionCommand::DispatchSessionStartHook {
                source: source.to_string(),
            });
        self.notify_session_cwd_for_watch(std::path::Path::new(&session_info.cwd));
        self.activity.register_session(&session_info.id.0, &handle);
        self.sessions.borrow_mut().insert(session_info.id.clone(), handle);
        self.spawn_managed_gateway_tool_catalog_fetch();
        let cwd_for_maintenance = session_info.cwd.clone();
        tokio::spawn(async move {
            crate::session::prompt_history::truncate_if_needed_async(cwd_for_maintenance)
                .await;
        });
        Ok(())
    }
    /// Collects all pending permission events from a session's receiver.
    /// Returns only the events from the current turn (since last collection).
    pub(super) fn collect_permission_events(
        &self,
        session_id: &acp::SessionId,
    ) -> Vec<PermissionEvent> {
        let mut events = Vec::new();
        if let Some(rx) = self
            .permission_event_receivers
            .borrow_mut()
            .get_mut(session_id)
        {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        events
    }
}
