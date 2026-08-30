use super::mcp_failed_reminder::{classify_failed_servers, render_failed_section};
use super::*;
use crate::session::mcp_servers::McpOauthDiscovery;
use xai_grok_telemetry::instrument_task;
use xai_grok_telemetry::region::Parent;
/// Wire the session's elicitation inbox into a freshly built client so its
/// `elicitation/create` requests reach the coordinator. Takes the
/// already-locked `McpState` so each caller keeps its own lock scope.
fn attach_elicitation_tx(
    state: &crate::session::mcp_servers::McpState,
    client: &crate::session::mcp_servers::McpClient,
) {
    if let Some(tx) = state.elicitation_tx() {
        client.set_elicitation_tx(Some(tx));
    }
}
impl SessionActor {
    /// Wait for MCP tools to be initialized.
    /// If initialization is in progress by another task, this will poll until complete.
    pub(super) async fn wait_for_mcp_initialized(&self) {
        loop {
            {
                let mcp_state = self.mcp_state.lock().await;
                if mcp_state.is_initialized() {
                    return;
                }
                if !mcp_state.is_initializing() {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        self.ensure_mcp_tools_initialized().await;
    }
    /// Register tools from shared (inherited) MCP clients on this session's ToolBridge.
    ///
    /// Shared clients are already connected (Arc-shared from parent), so
    /// `get_tool_registrations` reuses the existing transport — no new handshakes.
    async fn register_shared_client_tools(&self) {
        let shared_clients: Vec<(
            String,
            std::sync::Arc<crate::session::mcp_servers::McpClient>,
        )> = {
            let st = self.mcp_state.lock().await;
            if st.shared_clients.is_empty() {
                return;
            }
            st.shared_clients
                .iter()
                .map(|(n, c)| (n.clone(), std::sync::Arc::clone(c)))
                .collect()
        };
        tracing::info!(
            session_id = %self.session_info.id.0,
            count = shared_clients.len(),
            "Registering tools from shared MCP clients"
        );
        let mcp_state_arc = std::sync::Arc::clone(&self.mcp_state);
        let mut ui_tools: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        > = std::collections::HashMap::new();
        for (server_name, client) in &shared_clients {
            let regs = match client
                .get_tool_registrations(std::sync::Arc::clone(&mcp_state_arc))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "Failed to list tools from shared MCP client, skipping"
                    );
                    continue;
                }
            };
            let mut mcp_state = self.mcp_state.lock().await;
            for reg in regs {
                self.register_mcp_tool(server_name, reg, &mut mcp_state, &mut ui_tools)
                    .await;
            }
        }
        self.refresh_mcp_snapshot_and_schedule_reminder().await;
        if !ui_tools.is_empty() {
            self.emit_mcp_tools_changed_notifications(ui_tools);
        }
    }
    pub(super) async fn register_mcp_tool(
        &self,
        server_name: &str,
        reg: crate::session::mcp_servers::McpToolRegistration,
        mcp_state: &mut crate::session::mcp_servers::McpState,
        ui_tools_by_server: &mut std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        >,
    ) {
        let qualified_name = reg.name.clone();
        let prefix = format!(
            "{}{}",
            server_name,
            crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
        );
        let unqualified = qualified_name
            .strip_prefix(&prefix)
            .unwrap_or(&qualified_name)
            .to_string();
        mcp_state.record_tool_icons(qualified_name.clone(), reg.icons.clone());
        if let Some(meta) = reg.meta.as_ref() {
            mcp_state
                .mcp_tool_meta
                .insert(qualified_name.clone(), meta.clone());
            if meta
                .get("ui")
                .and_then(|ui| ui.get("resourceUri"))
                .is_some()
            {
                ui_tools_by_server
                    .entry(server_name.to_string())
                    .or_default()
                    .push(crate::extensions::mcp::McpToolEntry {
                        name: unqualified.clone(),
                        display_name: None,
                        description: Some(reg.description.clone()),
                        meta: Some(meta.clone()),
                        icons: reg.icons.clone(),
                        enabled: !mcp_state.is_tool_disabled(server_name, &unqualified),
                    });
            }
        }
        if mcp_state.is_tool_disabled(server_name, &unqualified) {
            tracing::info!(
                "Stashing disabled MCP tool '{}' from '{}'",
                qualified_name,
                server_name
            );
            mcp_state
                .disabled_tool_registrations
                .insert(qualified_name, reg);
            return;
        }
        if reg.model_visible {
            if let Err(e) = self
                .agent
                .borrow()
                .tool_bridge()
                .register_mcp_tools(reg.name, reg.tool, Some(reg.input_schema))
                .await
            {
                tracing::warn!(
                    "Failed to register tool '{}' from MCP server '{}': {}",
                    qualified_name,
                    server_name,
                    e
                );
            } else {
                tracing::debug!(
                    "Registered MCP tool '{}' from server '{}'",
                    qualified_name,
                    server_name
                );
            }
        } else {
            tracing::debug!(
                "Skipping app-only MCP tool '{}' from '{}'",
                qualified_name,
                server_name
            );
        }
    }
    /// Emit per-server `x.ai/mcp/tools_changed` notifications.
    ///
    /// Each emission carries the owning
    /// `sessionId` so the pager can route via `find_session_match`
    /// instead of falling back to `app.active_view`. Without that
    /// field, a background-agent push would silently land on the
    /// foregrounded agent's modal — the exact multi-agent bug this
    /// routing is meant to close.
    pub(super) fn emit_mcp_tools_changed_notifications(
        &self,
        ui_tools_by_server: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        >,
    ) {
        let session_id = self.session_id_string();
        for (server_name, tools) in ui_tools_by_server {
            let payload = crate::extensions::mcp::McpToolsChanged {
                session_id: session_id.clone(),
                server_name,
                tools,
            };
            if let Ok(params) = serde_json::value::to_raw_value(&payload) {
                self.notifications
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        crate::extensions::mcp::mcp_methods::TOOLS_CHANGED,
                        params.into(),
                    ));
            }
        }
    }
    /// Handle explicit auth trigger from the client (x.ai/mcp/auth_trigger).
    ///
    /// Runs force_reauth (browser flow), then re-initializes the server
    /// and registers its tools.
    pub(super) async fn handle_mcp_auth_trigger(&self, server_name: &str) -> Result<(), String> {
        let client = match self.mcp_state.lock().await.get_client(server_name).cloned() {
            Some(c) if c.has_auth() => c,
            _ => {
                self.rebuild_http_client_with_oauth(server_name, McpOauthDiscovery::Network)
                    .await?
            }
        };
        if !client.force_reauth(true).await {
            return Err(format!(
                "Authentication failed for MCP server '{}'",
                server_name
            ));
        }
        let mcp_state_arc = self.mcp_state.clone();
        let registrations = client
            .get_tool_registrations(mcp_state_arc)
            .await
            .map_err(|e| format!("Failed to get tools after auth: {}", e))?;
        let mut mcp_state = self.mcp_state.lock().await;
        mcp_state.auth_required.remove(server_name);
        mcp_state.clear_init_failed(server_name);
        let mut ui_tools: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        > = std::collections::HashMap::new();
        for reg in registrations {
            self.register_mcp_tool(server_name, reg, &mut mcp_state, &mut ui_tools)
                .await;
        }
        drop(mcp_state);
        self.refresh_mcp_snapshot_and_schedule_reminder().await;
        self.emit_mcp_tools_changed_notifications(ui_tools);
        self.refresh_goal_harness_enabled().await;
        tracing::info!(
            server = server_name,
            "MCP server authenticated and tools registered via auth_trigger"
        );
        Ok(())
    }
    async fn rebuild_http_client_with_oauth(
        &self,
        server_name: &str,
        discovery: McpOauthDiscovery,
    ) -> Result<std::sync::Arc<crate::session::mcp_servers::McpClient>, String> {
        let (server_config, meta_config, event_tx) = {
            let mcp_state = self.mcp_state.lock().await;
            let server_config = mcp_state
                .configs
                .iter()
                .find(|c| crate::session::mcp_servers::mcp_server_name(c) == server_name)
                .cloned()
                .ok_or_else(|| format!("MCP server '{}' not found in config", server_name))?;
            match &server_config {
                acp::McpServer::Http(_) | acp::McpServer::Sse(_) => {}
                _ => {
                    return Err(format!("MCP server '{}' does not use OAuth", server_name));
                }
            }
            let meta_config = mcp_state.meta_config_map.get(server_name).cloned();
            let event_tx = mcp_state.client_event_tx();
            (server_config, meta_config, event_tx)
        };
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let session_id = self.session_info.id.0.as_ref();
        let (_, oauth_config_map) =
            crate::util::config::load_mcp_servers_with_oauth(cwd, &self.rebuild_spec.compat);
        let byo_config = oauth_config_map.get(server_name).cloned();
        let event_writer = self.events.writer();
        let ctx = crate::session::mcp_servers::McpSpawnCtx::for_session(
            session_id,
            &event_writer,
            crate::session::mcp_servers::OauthInteractivity::Interactive,
            self.tool_context.process_scope.as_ref(),
        )
        .with_oauth_discovery(discovery);
        let new_client = crate::session::mcp_servers::start_mcp_server(
            server_config,
            Some(cwd),
            meta_config.as_ref(),
            byo_config.as_ref(),
            &ctx,
        )
        .await
        .map_err(|e| format!("Failed to prepare OAuth for '{}': {}", server_name, e))?;
        if !new_client.has_auth() {
            return Err(match discovery {
                McpOauthDiscovery::Network => {
                    format!(
                        "MCP server '{}' does not support OAuth (discovery found no authorization support)",
                        server_name
                    )
                }
                McpOauthDiscovery::Disk => {
                    format!(
                        "MCP server '{}' has no stored OAuth credentials",
                        server_name
                    )
                }
            });
        }
        if let Some(tx) = event_tx {
            new_client.set_event_tx(Some(tx));
        }
        attach_elicitation_tx(&*self.mcp_state.lock().await, &new_client);
        let arc = std::sync::Arc::new(new_client);
        {
            let mut mcp_state = self.mcp_state.lock().await;
            mcp_state
                .owned_clients
                .insert(server_name.to_string(), arc.clone());
            mcp_state.auth_required.insert(server_name.to_string());
            mcp_state.clear_init_failed(server_name);
        }
        tracing::info!(
            server = server_name,
            discovery = ?discovery,
            "Rebuilt MCP HTTP client with OAuth manager"
        );
        Ok(arc)
    }
    pub(super) async fn retry_auth_required_servers(&self) {
        let servers_to_retry: Vec<String> = {
            let state = self.mcp_state.lock().await;
            state.auth_required.iter().cloned().collect()
        };
        if servers_to_retry.is_empty() {
            return;
        }
        let mut recovered = false;
        let mut all_ui_tools: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        > = std::collections::HashMap::new();
        for server_name in &servers_to_retry {
            let existing = {
                let state = self.mcp_state.lock().await;
                state.get_client(server_name).cloned()
            };
            let client = match existing {
                Some(c) if c.has_auth() => {
                    if !c.try_reauth_from_disk().await {
                        continue;
                    }
                    c
                }
                _ => {
                    match self
                        .rebuild_http_client_with_oauth(server_name, McpOauthDiscovery::Disk)
                        .await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::debug!(
                                server = server_name.as_str(),
                                %e,
                                "retry_auth_required: no stored-credential rebuild"
                            );
                            continue;
                        }
                    }
                }
            };
            let init_budget = std::time::Duration::from_secs(
                client
                    .startup_timeout_sec()
                    .saturating_mul(2)
                    .saturating_add(5),
            );
            let mcp_state_arc = self.mcp_state.clone();
            let registrations = match tokio::time::timeout(
                init_budget,
                client.get_tool_registrations(mcp_state_arc),
            )
            .await
            .unwrap_or_else(|_| {
                Err(crate::session::mcp_servers::McpError::Timeout {
                    server: server_name.clone(),
                    timeout_secs: init_budget.as_secs(),
                })
            }) {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(
                        server = server_name.as_str(),
                        %e,
                        "retry_auth_required: handshake still failing"
                    );
                    continue;
                }
            };
            let mut mcp_state = self.mcp_state.lock().await;
            mcp_state.auth_required.remove(server_name);
            let mut ui_tools: std::collections::HashMap<
                String,
                Vec<crate::extensions::mcp::McpToolEntry>,
            > = std::collections::HashMap::new();
            for reg in registrations {
                self.register_mcp_tool(server_name, reg, &mut mcp_state, &mut ui_tools)
                    .await;
            }
            drop(mcp_state);
            all_ui_tools.extend(ui_tools);
            tracing::info!(
                server = server_name.as_str(),
                "MCP server recovered via retry_auth_required (tokens found on disk)"
            );
            recovered = true;
        }
        if recovered {
            self.refresh_mcp_snapshot_and_schedule_reminder().await;
            self.emit_mcp_tools_changed_notifications(all_ui_tools);
        }
    }
    /// The OAuth config map every spawn path must use: config-file BYO
    /// settings merged with plugin-provided OAuth (client IDs, callbacks) for
    /// plugin MCP servers. Shared by initial init and the unreachable
    /// respawn so a recovered plugin server keeps its OAuth identity.
    pub(super) fn spawn_oauth_config_map(
        &self,
        cwd: &std::path::Path,
    ) -> crate::util::config::McpOAuthConfigMap {
        let (_, mut oauth_config_map) =
            crate::util::config::load_mcp_servers_with_oauth(cwd, &self.rebuild_spec.compat);
        let plugin_registry_snapshot = self.plugin_registry.borrow().clone();
        let plugin_oauth = crate::session::managed_mcp::collect_plugin_oauth_configs(
            plugin_registry_snapshot.as_deref(),
        );
        let toml_mcp_names = crate::util::config::all_toml_mcp_server_names(cwd);
        crate::session::managed_mcp::merge_plugin_oauth_into(
            &mut oauth_config_map,
            plugin_oauth,
            &toml_mcp_names,
        );
        oauth_config_map
    }
    /// Attempt to respawn MCP servers whose last spawn failed as unreachable
    /// ([`xai_grok_mcp::servers::McpError::Unreachable`]): a transient
    /// connectivity loss during one startup probe must not strip the session
    /// of the server's tools for its remaining lifetime (init runs once per
    /// config generation, so nothing else ever re-attempts the spawn).
    ///
    /// Runs on MCP-surface tool batches, `x.ai/mcp/list` refreshes, and the
    /// explicit retry command. Concurrency and staleness are governed by the
    /// attempt-token protocol in `McpState`: `take_unreachable_retry_candidates`
    /// hands each due server to exactly one attempt (in-flight servers are not
    /// candidates), every settle requires the token, and config teardown
    /// invalidates it — so parallel triggers cannot double-spawn and a stale
    /// attempt cannot overwrite a newer client or re-pollute cleaned records.
    /// The next cooldown starts when an attempt settles as failed.
    pub(super) async fn retry_unreachable_servers(&self) {
        let (attempts, meta_config_map, configs) = {
            let mut state = self.mcp_state.lock().await;
            let attempts = state.take_unreachable_retry_candidates();
            let configs: Vec<acp::McpServer> = state
                .configs
                .iter()
                .filter(|c| attempts.iter().any(|(n, _)| n == mcp_server_name(c)))
                .cloned()
                .collect();
            (attempts, state.meta_config_map.clone(), configs)
        };
        if attempts.is_empty() || configs.is_empty() {
            return;
        }
        let attempt_tokens: std::collections::HashMap<String, u64> = attempts.into_iter().collect();
        let mut unsettled: std::collections::HashMap<String, u64> = attempt_tokens.clone();
        tracing::info!(
            servers = ?attempt_tokens.keys().collect::<Vec<_>>(),
            "Retrying spawn of unreachable MCP servers"
        );
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let oauth_config_map = self.spawn_oauth_config_map(cwd);
        let spawn_writer = self.events.writer();
        let ctx = crate::session::mcp_servers::McpSpawnCtx::for_session(
            self.session_info.id.0.as_ref(),
            &spawn_writer,
            OauthInteractivity::from_non_interactive(self.attach_non_interactive.get()),
            self.tool_context.process_scope.as_ref(),
        );
        let results = crate::session::mcp_servers::start_mcp_servers(
            configs,
            Some(cwd),
            &meta_config_map,
            &oauth_config_map,
            &ctx,
        )
        .await;
        let mut recovered = false;
        let mut all_ui_tools: std::collections::HashMap<
            String,
            Vec<crate::extensions::mcp::McpToolEntry>,
        > = std::collections::HashMap::new();
        for result in results {
            match result {
                Ok(client) => {
                    let server_name = client.server_name().to_string();
                    let Some(&token) = attempt_tokens.get(&server_name) else {
                        continue;
                    };
                    unsettled.remove(&server_name);
                    {
                        let state = self.mcp_state.lock().await;
                        if let Some(tx) = state.client_event_tx() {
                            client.set_event_tx(Some(tx));
                        }
                        attach_elicitation_tx(&state, &client);
                    }
                    let arc = std::sync::Arc::new(client);
                    let init_budget = std::time::Duration::from_secs(
                        arc.startup_timeout_sec()
                            .saturating_mul(2)
                            .saturating_add(5),
                    );
                    let registrations = match tokio::time::timeout(
                        init_budget,
                        arc.get_tool_registrations(self.mcp_state.clone()),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(crate::session::mcp_servers::McpError::Timeout {
                            server: server_name.clone(),
                            timeout_secs: init_budget.as_secs(),
                        })
                    }) {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::debug!(
                                server = server_name.as_str(),
                                %e,
                                "retry_unreachable: handshake still failing"
                            );
                            self.settle_failed_unreachable_attempt(
                                &server_name,
                                token,
                                &e,
                                Some(arc.clone()),
                            )
                            .await;
                            continue;
                        }
                    };
                    let _ = arc
                        .arm_liveness_watcher(xai_grok_mcp::liveness::DEFAULT_POLL_INTERVAL)
                        .await;
                    let mut mcp_state = self.mcp_state.lock().await;
                    if !mcp_state.finish_unreachable_attempt(&server_name, token) {
                        tracing::info!(
                            server = server_name.as_str(),
                            "retry_unreachable: attempt token stale; discarding client"
                        );
                        continue;
                    }
                    mcp_state
                        .owned_clients
                        .insert(server_name.clone(), arc.clone());
                    let mut ui_tools: std::collections::HashMap<
                        String,
                        Vec<crate::extensions::mcp::McpToolEntry>,
                    > = std::collections::HashMap::new();
                    for reg in registrations {
                        self.register_mcp_tool(&server_name, reg, &mut mcp_state, &mut ui_tools)
                            .await;
                    }
                    drop(mcp_state);
                    all_ui_tools.extend(ui_tools);
                    tracing::info!(
                        server = server_name.as_str(),
                        "MCP server recovered via retry_unreachable (respawn succeeded)"
                    );
                    recovered = true;
                }
                Err(e) => {
                    let Some(sname) = e.server_name().map(str::to_string) else {
                        continue;
                    };
                    let Some(&token) = attempt_tokens.get(&sname) else {
                        continue;
                    };
                    unsettled.remove(&sname);
                    tracing::debug!(
                        server = sname.as_str(),
                        %e,
                        "retry_unreachable: spawn still failing"
                    );
                    self.settle_failed_unreachable_attempt(&sname, token, &e, None)
                        .await;
                }
            }
        }
        for (name, token) in unsettled {
            tracing::debug!(
                server = name.as_str(),
                "retry_unreachable: spawn produced no attributable result"
            );
            self.mcp_state
                .lock()
                .await
                .settle_unreachable_attempt_failed(
                    &name,
                    token,
                    "respawn attempt produced no attributable result".to_string(),
                );
        }
        if recovered {
            self.refresh_mcp_snapshot_and_schedule_reminder().await;
            self.emit_mcp_tools_changed_notifications(all_ui_tools);
        }
    }
    /// Settle a failed respawn attempt by error class:
    /// - auth rejection → hand off to the auth-required flow, keeping the
    ///   fresh client (when one exists) for its recovery paths;
    /// - transient connectivity failures (typed `Unreachable` / `Timeout`, or
    ///   a transport-level handshake error like connection refused/reset) →
    ///   stay on the cooldown-gated retry schedule;
    /// - anything else (protocol rejection, malformed `tools/list`, redirect
    ///   loops, …) → terminal init failure: recorded as `Unavailable`, no
    ///   more automatic respawns.
    ///
    /// Every transition is token-gated: a stale attempt records nothing.
    async fn settle_failed_unreachable_attempt(
        &self,
        server_name: &str,
        token: u64,
        error: &crate::session::mcp_servers::McpError,
        client_for_auth: Option<std::sync::Arc<crate::session::mcp_servers::McpClient>>,
    ) {
        let detail =
            || xai_grok_tools::util::truncate_str_with_marker(&error.to_string(), 200).into_owned();
        let mut state = self.mcp_state.lock().await;
        if error.is_auth_rejection() {
            if !state.settle_unreachable_attempt_unretryable(server_name, token) {
                return;
            }
            if let Some(client) = client_for_auth {
                state.owned_clients.insert(server_name.to_string(), client);
            }
            state.clear_init_failed(server_name);
            state.record_init_failure(server_name, true, None);
        } else if error.is_transient_connectivity() {
            state.settle_unreachable_attempt_failed(server_name, token, detail());
        } else {
            if !state.settle_unreachable_attempt_unretryable(server_name, token) {
                return;
            }
            state.record_init_failure(server_name, false, Some(detail()));
        }
    }
    /// Refresh the MCP tool/search snapshot from current tool bridge state.
    /// Called after MCP init and after auth_trigger/retry recovers new servers.
    ///
    /// This updates the model-visible MCP snapshot and marks reminder emission
    /// dirty so `maybe_inject_mcp_reminder` can inject the next
    /// `<system-reminder>` at a turn boundary. The `search_tool` description
    /// itself stays static (cacheable).
    pub(super) async fn refresh_mcp_snapshot_and_schedule_reminder(&self) {
        let disabled_gateway_tools = crate::util::config::get_all_mcp_disabled_tools(
            std::path::Path::new(&self.session_info.cwd),
        );
        self.refresh_mcp_snapshot_and_schedule_reminder_with_disabled(&disabled_gateway_tools)
            .await;
    }
    pub(super) async fn refresh_mcp_snapshot_and_schedule_reminder_with_disabled(
        &self,
        disabled_gateway_tools: &std::collections::HashMap<
            String,
            std::collections::HashSet<String>,
        >,
    ) {
        let mcp_initialized = self.mcp_state.lock().await.is_initialized();
        refresh_mcp_snapshot_and_schedule_reminder_with(
            self.agent.borrow().tool_bridge().clone(),
            Arc::clone(&self.mcp_state),
            self.managed_mcp_handle.clone(),
            self.tool_metadata_snapshot.clone(),
            Arc::clone(&self.mcp_reminder_dirty),
            mcp_initialized,
            disabled_gateway_tools,
            self.cursor_mcps_root(),
        )
        .await;
    }
    /// `None` twin: descriptor materialization is unavailable in this build.
    fn cursor_mcps_root(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// Snapshot both MCP and skill announcement tracking state and send it
    /// to the persistence channel for atomic write to `announcement_state.json`.
    ///
    /// Called after MCP fingerprint changes, skill update effects, and
    /// compaction so that resumed sessions start with accurate tracking state.
    pub(super) async fn persist_announcement_state(&self) {
        let skill_names = self.tool_bridge_handle().get_announced_skill_names().await;
        let (mcp_server_fingerprints, announced_failed) = {
            let announced = self.mcp_announcements.lock();
            (
                crate::session::announcement_state::to_persisted_fingerprints(
                    &announced.fingerprints,
                ),
                announced.persisted_failed(),
            )
        };
        let state = crate::session::announcement_state::AnnouncementState {
            mcp_server_fingerprints,
            announced_skill_names: skill_names,
            announced_failed_servers: announced_failed,
        };
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::AnnouncementState(state));
    }
    /// Inject an MCP server system-reminder if the set changed since the
    /// last announcement. Skips if not dirty; clears the dirty flag up
    /// front so a cancelled run degrades to a missed (re-triggerable)
    /// injection, never an in-session duplicate. (A cancel landing between
    /// the push and the trailing persist can still yield one duplicate
    /// after a crash-resume — the benign direction of that tradeoff.)
    ///
    /// Connected servers are deduped by fingerprint, failed servers by
    /// episode (see [`crate::session::announcement_state::McpAnnounced`]).
    ///
    /// Called at turn-start (`handle_prompt`) and inside the agentic loop
    /// (before `build_request`) so that mid-turn MCP connections (Progressive
    /// mode) are announced before the model's next inference call.
    ///
    /// Suppressed when the active template manages MCP context elsewhere. The
    /// dirty flag is still cleared.
    pub(super) async fn maybe_inject_mcp_reminder(&self) {
        if !self
            .mcp_reminder_dirty
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        use xai_grok_tools::implementations::search_tool::fingerprint_servers;
        self.mcp_reminder_dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
        struct RearmOnDrop<'a>(Option<&'a std::sync::atomic::AtomicBool>);
        impl Drop for RearmOnDrop<'_> {
            fn drop(&mut self) {
                if let Some(dirty) = self.0.take() {
                    dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        let mut rearm_on_drop = RearmOnDrop(Some(&self.mcp_reminder_dirty));
        let server_summaries = self.connected_server_summaries();
        let new_fingerprints = fingerprint_servers(&server_summaries);
        let (currently_failed, unconnected_configured) = {
            let mcp_state = self.mcp_state.lock().await;
            let connected_names: std::collections::HashSet<&str> =
                server_summaries.iter().map(|s| s.name.as_str()).collect();
            classify_failed_servers(&mcp_state, &connected_names)
        };
        let hint = self.rendered_mcp_hint().await;
        let announcements_changed = self.latch_and_push_mcp_reminder(
            &server_summaries,
            new_fingerprints,
            currently_failed,
            &unconnected_configured,
            hint.as_deref(),
        );
        rearm_on_drop.0 = None;
        if announcements_changed {
            self.persist_announcement_state().await;
        }
    }
    /// Latch fingerprints and failure episodes under one lock (so a
    /// concurrent persist cannot snapshot half an update) and push the
    /// resulting reminder, if any. Returns whether the announced state
    /// changed (the caller persists on change).
    ///
    /// Deliberately sync: a latched episode announces exactly once, so no
    /// await point may separate the latch from the push — a future dropped
    /// there by a turn cancel would swallow the announcement for good.
    fn latch_and_push_mcp_reminder(
        &self,
        server_summaries: &[xai_grok_tools::types::tool_index::ServerSummary],
        new_fingerprints: std::collections::HashMap<
            String,
            xai_grok_tools::implementations::search_tool::ServerFingerprint,
        >,
        currently_failed: Vec<crate::session::announcement_state::FailedServer>,
        unconnected_configured: &std::collections::HashSet<String>,
        hint: Option<&str>,
    ) -> bool {
        use xai_grok_tools::implementations::search_tool::{
            build_delta_reminder, build_server_reminder,
        };
        let (mut reminder_text, announcements_changed, to_announce) = {
            let mut announced = self.mcp_announcements.lock();
            let text = match self.mcp_reminder_mode {
                McpReminderMode::Delta => {
                    build_delta_reminder(&announced.fingerprints, server_summaries)
                }
                McpReminderMode::Full => {
                    if announced.fingerprints == new_fingerprints {
                        None
                    } else if server_summaries.is_empty() {
                        Some("All MCP servers have disconnected.".to_string())
                    } else {
                        build_server_reminder(server_summaries)
                    }
                }
            };
            let fingerprints_changed = announced.fingerprints != new_fingerprints;
            if fingerprints_changed {
                announced.fingerprints = new_fingerprints;
            }
            let (to_announce, failed_changed) =
                announced.note_failures(currently_failed, unconnected_configured);
            (text, fingerprints_changed || failed_changed, to_announce)
        };
        let has_failed = !to_announce.is_empty();
        if has_failed {
            reminder_text
                .get_or_insert_with(String::new)
                .push_str(&render_failed_section(&to_announce));
        }
        if let (Some(text), Some(hint)) = (reminder_text.as_mut(), hint) {
            text.push_str(hint);
        }
        if let Some(text) = reminder_text {
            self.push_system_reminder(&text);
            tracing::info!(
                servers = server_summaries.len(),
                has_failed,
                mode = ?self.mcp_reminder_mode,
                "Injected MCP server system-reminder"
            );
        } else {
            tracing::debug!(
                servers = server_summaries.len(),
                "MCP servers unchanged, skipping reminder injection"
            );
        }
        announcements_changed
    }
    /// Re-arm failure announcements after an event that dropped reminders
    /// from context (compaction, rewind): clear the announced episodes and
    /// mark the reminder dirty so the next injection re-announces servers
    /// that are still down. Persists the cleared tracking so a resume
    /// starts from it.
    ///
    /// Connected fingerprints stay latched: compaction carries the listing
    /// in its context, a rewind's kept prefix usually retains the initial
    /// listing (clearing would inject a duplicate), and connected tools
    /// remain visible in the tool definitions regardless.
    pub(crate) async fn rearm_failed_server_announcements(&self) {
        self.mcp_announcements.lock().rearm_failed();
        self.mcp_reminder_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.persist_announcement_state().await;
    }
    /// Returns `true` iff `server` has a `Stdio` entry in
    /// [`McpState::configs`] and is not on the per-cwd disabled list
    /// (`util::config::disabled_mcp_server_names`). Used by the
    /// auto-restart task to gate on the live configuration each
    /// backoff iteration — the user may have toggled the server off
    /// or removed it from `~/.grok/config.toml` while we were
    /// sleeping.
    ///
    /// HTTP / HttpAuth entries always return `false` here, which is
    /// what the auto-restart task wants: HTTP recovery is via
    /// `reset_transport`, not respawn.
    ///
    /// ## Cost
    ///
    /// Performs one synchronous read of the per-cwd disabled-MCP
    /// list (`crate::util::config::disabled_mcp_server_names`,
    /// which parses `~/.grok/config.toml` + the project
    /// `.grok/config.toml`) on every call. The auto-restart task
    /// calls this at most:
    ///   - once at schedule time (`maybe_schedule_restart`), and
    ///   - once per backoff iteration (≤3 per restart window).
    ///
    /// Worst case ~4 disk reads per crashed server, bounded by the
    /// 21 s window. Acceptable here; cache invalidation
    /// is tracked as a follow-up if this ever moves into a hotter
    /// path.
    pub(crate) async fn is_stdio_server_configured(&self, server: &str) -> bool {
        let mcp_state = self.mcp_state.lock().await;
        let is_stdio_in_configs = mcp_state
            .configs
            .iter()
            .any(|c| {
                matches!(c, acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) if name == server)
            });
        if !is_stdio_in_configs {
            return false;
        }
        drop(mcp_state);
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let disabled = crate::util::config::disabled_mcp_server_names(cwd);
        !disabled.contains(server)
    }
    /// HTTP analog of [`Self::is_stdio_server_configured`]: `true` iff
    /// `server` has an enabled `Http` / `Sse` config entry.
    /// Gates [`crate::session::mcp_restart::maybe_schedule_http_recovery`].
    pub(crate) async fn is_http_server_configured(&self, server: &str) -> bool {
        let mcp_state = self.mcp_state.lock().await;
        let is_http_in_configs = mcp_state
            .configs
            .iter()
            .any(|c| {
                matches!(
                c,
                acp::McpServer::Http(acp::McpServerHttp { name, .. }) | acp::McpServer::Sse(acp::McpServerSse { name, .. }) if name == server
            )
            });
        if !is_http_in_configs {
            return false;
        }
        drop(mcp_state);
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let disabled = crate::util::config::disabled_mcp_server_names(cwd);
        !disabled.contains(server)
    }
    /// Recover a dead HTTP client in place via
    /// [`McpClient::recover`] (reset + re-handshake + re-arm liveness).
    /// Unlike [`Self::respawn_stdio`] the existing `Arc<McpClient>` is kept, so
    /// its tools stay valid; `ensure_initialized` emits the status, so this
    /// emits none.
    ///
    /// Post-handshake TOCTOU re-check: `ensure_initialized` can take
    /// several seconds, during which a `ConfigRemoved` / toggle-off can
    /// evict or replace this client (the dispatcher evicts HTTP clients on
    /// `ConfigRemoved`). If the looked-up client is no longer the live,
    /// enabled entry, tear down the watcher we just re-armed and report the
    /// race instead of a false success on a detached client.
    pub(crate) async fn reset_http_client(&self, server: &str) -> Result<(), String> {
        let client = {
            let mcp_state = self.mcp_state.lock().await;
            mcp_state.get_client(server).cloned()
        };
        let Some(client) = client else {
            return Err(format!("no client for server '{server}'"));
        };
        if !client.is_http() {
            return Err(format!("server '{server}' is not an HTTP client"));
        }
        client.recover().await.map_err(|e| e.to_string())?;
        let still_current = {
            let mcp_state = self.mcp_state.lock().await;
            mcp_state
                .get_client(server)
                .is_some_and(|c| std::sync::Arc::ptr_eq(c, &client))
        };
        if !still_current || !self.is_http_server_configured(server).await {
            client.set_liveness_handle(None);
            return Err(format!(
                "server '{server}' was removed or disabled during HTTP recovery"
            ));
        }
        Ok(())
    }
    /// Unregister `server`'s tools from the bridge after stdio restart
    /// exhaustion, so the model stops calling a now-absent client.
    pub(crate) fn unregister_server_tools(&self, server: &str) {
        let prefix = format!(
            "{}{}",
            server,
            crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
        );
        let removed = self
            .agent
            .borrow()
            .tool_bridge()
            .unregister_tools_by_prefix(&prefix);
        if removed > 0 {
            tracing::info!(
                server = %server,
                tools_removed = removed,
                "unregistered tools for MCP server after auto-restart exhaustion",
            );
        }
    }
    /// Re-run [`crate::session::mcp_servers::start_mcp_server`]
    /// against the current config entry for `server`, drive the
    /// handshake, arm the liveness watcher, and atomically install
    /// the resulting `Arc<McpClient>` into
    /// [`McpState::owned_clients`].
    ///
    /// **Stdio-only.** Callers must gate on
    /// [`Self::is_stdio_server_configured`] first — this function
    /// returns `Err` for HTTP / HttpAuth or unknown servers.
    ///
    /// Failure modes (returned as a stringified, sanitized `Err`):
    /// - No matching stdio entry in `McpState::configs` (the entry
    ///   was removed mid-restart).
    /// - `start_mcp_server` failed (spawn / OAuth-discovery /
    ///   transport-build failure).
    /// - `ensure_initialized` returned `Err` (handshake failure).
    ///
    /// On success: the new `Arc<McpClient>` is in
    /// `mcp_state.owned_clients[server]` with `ClientState::Ready`,
    /// the dispatcher's `notify_tx` is wired to its
    /// `GrokClientHandler`, and the liveness watcher is armed —
    /// matching the post-handshake state produced by
    /// [`Self::ensure_mcp_tools_initialized`] for a fresh server.
    ///
    /// Tools that were previously registered against this server
    /// remain in `ToolBridge` and resolve transparently through
    /// `McpTool::mcp_state` — there's no per-tool re-registration
    /// step. `tools/list_changed` notifications from the respawned
    /// server flow through the normal dispatcher path.
    ///
    /// ## Event-tx wiring order
    ///
    /// Unlike the first-time handshake path (which wires
    /// `set_event_tx` before `ensure_initialized` so the dispatcher
    /// gets the `Ready → Initialized` push), the **restart** path
    /// wires `set_event_tx` after `ensure_initialized`. Reason: the
    /// auto-restart task is the SOLE emitter of restart status —
    /// it pushes `Reason::RestartSucceeded` directly. Letting
    /// `ensure_initialized` also emit `McpClientEvent::Ready` would
    /// produce two wire pushes for one restart (one
    /// `Reason::Initialized` from the dispatcher's mapping, one
    /// `Reason::RestartSucceeded` from the restart task).
    ///
    /// The `GrokClientHandler` constructed inside `try_handshake`
    /// holds the shared `Arc<Mutex<Option<Sender>>>` slot
    /// (`SharedEventTx`), so wiring the sender after the handshake
    /// still routes subsequent `tools/list_changed` /
    /// `resources/list_changed` server pushes through the
    /// dispatcher — the handler re-reads the slot on every emit.
    ///
    /// **Contract:**
    /// [`xai_grok_mcp::servers`] test
    /// `client_handler_observes_post_handshake_set_event_tx`
    /// builds a handler from a
    /// client whose slot is `None`, then installs a sender via
    /// `client.set_event_tx(Some(_))` and verifies the next emit
    /// reaches the new receiver. If that test regresses — i.e. a
    /// future refactor snapshots `notify_tx` at handler
    /// construction instead of re-reading via the `Arc<Mutex<_>>` —
    /// the restart path here will silently fail to deliver
    /// `tools/list_changed` for respawned servers. Keep that test
    /// and this comment together.
    ///
    /// ## TOCTOU re-check
    ///
    /// `start_mcp_server` plus `ensure_initialized` can take O(1) s
    /// (npm package fetch, OAuth dance, handshake). A concurrent
    /// `ToggleMcpServer enabled=false` or config-diff removal during
    /// that window must not result in a freshly-installed client for
    /// a server the user just disabled. After `ensure_initialized`
    /// succeeds and before the `owned_clients.insert`, this function
    /// re-checks [`Self::is_stdio_server_configured`]. On `false` it
    /// drops the new `Arc<McpClient>` — `kill_on_drop(true)` then
    /// SIGKILLs the spawned child — and returns an explicit error so
    /// the auto-restart loop can emit `Reason::Disabled`.
    pub(crate) async fn respawn_stdio(&self, server: &str) -> Result<(), String> {
        let (server_config, meta_config, event_tx) = {
            let mcp_state = self.mcp_state.lock().await;
            let server_config = mcp_state
                .configs
                .iter()
                .find(|c| {
                    matches!(c, acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) if name == server)
                })
                .cloned()
                .ok_or_else(|| format!("no stdio config entry for server '{server}'"))?;
            let meta_config = mcp_state.meta_config_map.get(server).cloned();
            let event_tx = mcp_state.client_event_tx();
            (server_config, meta_config, event_tx)
        };
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let session_id = self.session_info.id.0.as_ref();
        let (_, oauth_config_map) =
            crate::util::config::load_mcp_servers_with_oauth(cwd, &self.rebuild_spec.compat);
        let byo_config = oauth_config_map.get(server).cloned();
        let event_writer = self.events.writer();
        let ctx = crate::session::mcp_servers::McpSpawnCtx::for_session(
            session_id,
            &event_writer,
            OauthInteractivity::from_non_interactive(self.attach_non_interactive.get()),
            self.tool_context.process_scope.as_ref(),
        );
        let new_client = crate::session::mcp_servers::start_mcp_server(
            server_config.clone(),
            Some(cwd),
            meta_config.as_ref(),
            byo_config.as_ref(),
            &ctx,
        )
        .await
        .map_err(|e| e.to_string())?;
        attach_elicitation_tx(&*self.mcp_state.lock().await, &new_client);
        new_client
            .ensure_initialized()
            .await
            .map_err(|e| e.to_string())?;
        if !self.is_stdio_server_configured(server).await {
            drop(new_client);
            return Err(format!(
                "server '{server}' was disabled or removed during respawn"
            ));
        }
        let current_config = {
            let mcp_state = self.mcp_state.lock().await;
            mcp_state
                .configs
                .iter()
                .find(|c| {
                    matches!(c, acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) if name == server)
                })
                .cloned()
        };
        let config_unchanged = match (
            serde_json::to_string(&server_config),
            current_config.as_ref().map(serde_json::to_string),
        ) {
            (Ok(snapshot), Some(Ok(current))) => snapshot == current,
            _ => false,
        };
        if !config_unchanged {
            drop(new_client);
            return Err(format!(
                "config for server '{server}' changed during respawn"
            ));
        }
        if let Some(tx) = event_tx {
            new_client.set_event_tx(Some(tx.clone()));
            let _ = tx.send(xai_grok_mcp::servers::McpClientEvent::ToolsChanged {
                server: server.to_string(),
            });
        }
        let arc_client = std::sync::Arc::new(new_client);
        let _ = arc_client
            .arm_liveness_watcher(xai_grok_mcp::liveness::DEFAULT_POLL_INTERVAL)
            .await;
        {
            let mut mcp_state = self.mcp_state.lock().await;
            mcp_state
                .owned_clients
                .insert(server.to_string(), arc_client);
        }
        Ok(())
    }
    pub(super) async fn maybe_inject_mcp_connecting_reminder(&self) {
        if self.mcp_connecting_reminder_injected.get() {
            return;
        }
        let connecting: Vec<String> = {
            let mcp_state = self.mcp_state.lock().await;
            let mut names: Vec<String> = mcp_state.handshaking_servers_iter().cloned().collect();
            names.sort_unstable();
            names
        };
        if connecting.is_empty() {
            return;
        }
        self.mcp_connecting_reminder_injected.set(true);
        let delivery_tools = self.delivery_tools.borrow().clone();
        let text = format_mcp_connecting_reminder(&connecting, &delivery_tools);
        self.push_system_reminder(&text);
        tracing::info!(
            servers = ?connecting,
            ?delivery_tools,
            "Injected MCP connecting system-reminder"
        );
    }
    /// Re-apply the attaching client's per-attachment policy. Sent by the
    /// resident `session/load` rail when the request carries explicit
    /// `startupHints`, so an actor spawned under one client's policy serves a
    /// later-attaching client under *its* policy (a headless attachment gets
    /// Blocking MCP init and its delivery-tool guidance even when the actor
    /// was spawned by an interactive client, and vice versa). Only policy
    /// fields are touched — structural spawn-time hints (subagent identity,
    /// inherited prefix, preserved system head) stay frozen.
    pub(super) fn apply_attach_policy(&self, hints: &crate::session::StartupHints) {
        let strategy = hints.resolve_mcp_strategy();
        let changed = self.mcp_strategy.get() != strategy
            || self.attach_non_interactive.get() != hints.non_interactive
            || *self.delivery_tools.borrow() != hints.delivery_tools;
        self.mcp_strategy.set(strategy);
        self.attach_non_interactive.set(hints.non_interactive);
        *self.delivery_tools.borrow_mut() = hints.delivery_tools.clone();
        if changed {
            self.mcp_connecting_reminder_injected.set(false);
        }
        tracing::info!(
            ?strategy,
            non_interactive = hints.non_interactive,
            delivery_tools = ?hints.delivery_tools,
            changed,
            "apply_attach_policy: updated per-attachment policy from session request startupHints"
        );
    }
    /// Ensure MCP tools are initialized (spawns processes and performs handshakes on first call)
    pub(super) async fn ensure_mcp_tools_initialized(&self) {
        let (mcp_server_configs, meta_config_map, generation, existing_client_names, has_acp) = {
            let mut mcp_state = self.mcp_state.lock().await;
            if !mcp_state.try_start_init() {
                tracing::debug!(
                    session_id = %self.session_info.id.0,
                    "ensure_mcp_tools_initialized: skipped (already initialized or in progress)"
                );
                return;
            }
            tracing::info!(
                session_id = %self.session_info.id.0,
                config_count = mcp_state.configs.len(),
                config_names = ?mcp_state.configs.iter().map(crate::session::mcp_servers::mcp_server_name).collect::<Vec<_>>(),
                existing_client_count = mcp_state.owned_clients.len() + mcp_state.shared_clients.len(),
                generation = mcp_state.generation(),
                "ensure_mcp_tools_initialized: starting MCP init"
            );
            mcp_state.set_event_writer(self.events.writer());
            if mcp_state.disabled_tools.is_empty() {
                let cwd = std::path::Path::new(&self.session_info.cwd);
                let dt = crate::util::config::get_all_mcp_disabled_tools(cwd);
                if !dt.is_empty() {
                    tracing::info!(servers = dt.len(), "Loaded disabled_tools from config");
                    mcp_state.disabled_tools = dt;
                }
            }
            let existing: std::collections::HashSet<String> =
                mcp_state.owned_clients.keys().cloned().collect();
            (
                mcp_state.configs.clone(),
                mcp_state.meta_config_map.clone(),
                mcp_state.generation(),
                existing,
                mcp_state.has_acp_servers(),
            )
        };
        if mcp_server_configs.is_empty() && !has_acp {
            let mut mcp_state = self.mcp_state.lock().await;
            if mcp_state.generation() == generation {
                mcp_state.finish_init();
            } else {
                mcp_state.cancel_init();
                self.events
                    .emit(xai_grok_session_events::Event::McpInitCancelled {
                        reason: MCP_INIT_CANCELLED_CONFIG_CHANGED.to_string(),
                    });
            }
            drop(mcp_state);
            self.register_shared_client_tools().await;
            self.refresh_mcp_snapshot_and_schedule_reminder().await;
            if let Ok(params) = serde_json::value::to_raw_value(&serde_json::json!({
                "sessionId": self.session_info.id.0.as_ref(),
                "mcpToolCount": 0_u32,
                "elapsedMs": 0_u64,
            })) {
                self.notifications
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/mcp_initialized",
                        params.into(),
                    ));
            }
            self.mcp_handshakes_done.notify_waiters();
            return;
        }
        {
            let cwd = std::path::Path::new(&self.session_info.cwd);
            self.events
                .emit(crate::session::mcp_servers::build_config_resolved_event(
                    &mcp_server_configs,
                    cwd,
                ));
        }
        let configs_to_start: Vec<_> = mcp_server_configs
            .iter()
            .filter(|c| !existing_client_names.contains(mcp_server_name(c)))
            .cloned()
            .collect();
        let acp_pending_names = {
            let mcp_state = self.mcp_state.lock().await;
            mcp_state.pending_acp_server_names()
        };
        {
            let mut mcp_state = self.mcp_state.lock().await;
            let names: Vec<String> = configs_to_start
                .iter()
                .map(|c| mcp_server_name(c).to_string())
                .chain(acp_pending_names.iter().cloned())
                .collect();
            for name in &names {
                tracing::info!(server = %name, "Added server to handshaking set");
            }
            mcp_state.mark_servers_initializing(names);
        }
        self.mcp_connecting_reminder_injected.set(false);
        let init_total = (configs_to_start.len() + acp_pending_names.len()) as u32;
        if let Ok(params) = serde_json::value::to_raw_value(&serde_json::json!({
            "total": init_total,
            "connected": 0,
            "sessionId": self.session_info.id.0.as_ref(),
        })) {
            self.notifications
                .gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    crate::extensions::mcp::mcp_methods::INIT_PROGRESS,
                    params.into(),
                ));
        }
        if configs_to_start.is_empty() && acp_pending_names.is_empty() {
            let mut mcp_state = self.mcp_state.lock().await;
            if mcp_state.generation() == generation {
                mcp_state.finish_init();
            } else {
                mcp_state.cancel_init();
                self.events
                    .emit(xai_grok_session_events::Event::McpInitCancelled {
                        reason: MCP_INIT_CANCELLED_CONFIG_CHANGED.to_string(),
                    });
            }
            drop(mcp_state);
            self.register_shared_client_tools().await;
            self.refresh_mcp_snapshot_and_schedule_reminder().await;
            if let Ok(params) = serde_json::value::to_raw_value(&serde_json::json!({
                "sessionId": self.session_info.id.0.as_ref(),
                "mcpToolCount": 0_u32,
                "elapsedMs": 0_u64,
            })) {
                self.notifications
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/mcp_initialized",
                        params.into(),
                    ));
            }
            self.mcp_handshakes_done.notify_waiters();
            return;
        }
        let mut timer = crate::instrumentation_timer!("session.mcp_init");
        timer.with_field("session_id", self.session_info.id.0.as_ref());
        timer.with_field("server_count", configs_to_start.len() as u64);
        tracing::info!(
            "Starting MCP initialization ({} new servers, {} already initialized, strategy: {:?})",
            configs_to_start.len(),
            existing_client_names.len(),
            self.mcp_strategy.get()
        );
        let session_id = self.session_info.id.0.as_ref();
        tokio::task::yield_now().await;
        let cwd = std::path::Path::new(&self.session_info.cwd);
        let oauth_config_map = self.spawn_oauth_config_map(cwd);
        let spawn_writer = self.events.writer();
        let ctx = crate::session::mcp_servers::McpSpawnCtx::for_session(
            session_id,
            &spawn_writer,
            OauthInteractivity::from_non_interactive(self.attach_non_interactive.get()),
            self.tool_context.process_scope.as_ref(),
        );
        let mcp_results = build_pending_clients(
            &self.mcp_state,
            configs_to_start,
            Some(cwd),
            &meta_config_map,
            &oauth_config_map,
            &ctx,
        )
        .await;
        tokio::task::yield_now().await;
        let mut spawn_auth_failures: Vec<String> = Vec::new();
        let mut spawn_unreachable_failures: Vec<(String, String)> = Vec::new();
        let mcp_clients: Vec<_> = mcp_results
            .into_iter()
            .filter_map(|result| match result {
                Ok(client) => {
                    tracing::debug!("MCP server '{}' spawned", client.server_name());
                    Some(client)
                }
                Err(e) => {
                    tracing::warn!("Failed to spawn MCP server: {}", e);
                    let sname = e.server_name().unwrap_or("unknown").to_string();
                    if e.is_auth_rejection() && sname != "unknown" {
                        spawn_auth_failures.push(sname.clone());
                    } else if e.is_unreachable() && sname != "unknown" {
                        spawn_unreachable_failures.push((sname.clone(), e.to_string()));
                    }
                    let cfg = mcp_server_configs
                        .iter()
                        .find(|c| mcp_server_name(c) == sname.as_str());
                    self.events
                        .emit(xai_grok_session_events::Event::McpServerFailed {
                            server_name: sname,
                            transport: cfg.map(|c| mcp_transport_str(c).to_string()),
                            target: cfg.map(mcp_target_str),
                            error_type: e.error_category(),
                            error_message: e.to_string(),
                            duration_ms: None,
                            timeout_sec: None,
                        });
                    None
                }
            })
            .collect();
        let spawned_names: std::collections::HashSet<String> = mcp_clients
            .iter()
            .map(|c| c.server_name().to_string())
            .collect();
        {
            let mut mcp_state = self.mcp_state.lock().await;
            if mcp_state.generation() != generation {
                mcp_state.cancel_init();
                self.events
                    .emit(xai_grok_session_events::Event::McpInitCancelled {
                        reason: MCP_INIT_CANCELLED_CONFIG_CHANGED.to_string(),
                    });
                return;
            }
            let failed_spawns: Vec<String> = mcp_state
                .handshaking_servers_iter()
                .filter(|name| !spawned_names.contains(name.as_str()))
                .cloned()
                .collect();
            for name in &failed_spawns {
                tracing::warn!(
                    server = name.as_str(),
                    "MCP server spawn failed, removing from initializing set"
                );
                if spawn_auth_failures.iter().any(|n| n == name) {
                    mcp_state.record_init_failure(name, true, None);
                } else if let Some((_, detail)) =
                    spawn_unreachable_failures.iter().find(|(n, _)| n == name)
                {
                    mcp_state.record_unreachable_failure(name, detail.clone());
                }
                mcp_state.mark_server_ready(name);
            }
            mcp_state.finish_init();
        }
        let shared_clients_for_bg: Vec<(
            String,
            std::sync::Arc<crate::session::mcp_servers::McpClient>,
        )> = {
            let st = self.mcp_state.lock().await;
            st.shared_clients
                .iter()
                .map(|(n, c)| (n.clone(), std::sync::Arc::clone(c)))
                .collect()
        };
        let mcp_state_bg = std::sync::Arc::clone(&self.mcp_state);
        let tool_bridge = self.agent.borrow().tool_bridge().clone();
        let gateway = self.notifications.gateway.clone();
        let tool_snapshot = self.tool_metadata_snapshot.clone();
        let managed_mcp_handle = self.managed_mcp_handle.clone();
        let mcp_reminder_dirty = Arc::clone(&self.mcp_reminder_dirty);
        let mcp_handshakes_done = Arc::clone(&self.mcp_handshakes_done);
        let session_id_owned = self.session_info.id.0.clone();
        let mcps_root_bg = self.cursor_mcps_root();
        let disabled_gateway_tools_bg = crate::util::config::get_all_mcp_disabled_tools(
            std::path::Path::new(&self.session_info.cwd),
        );
        let server_transport_map: std::collections::HashMap<String, &'static str> =
            mcp_server_configs
                .iter()
                .map(|c| (mcp_server_name(c).to_string(), mcp_transport_str(c)))
                .collect();
        let server_target_map: std::collections::HashMap<String, String> = mcp_server_configs
            .iter()
            .map(|c| (mcp_server_name(c).to_string(), mcp_target_str(c)))
            .collect();
        let scope_cwd = std::path::Path::new(self.session_info.cwd.as_str());
        let server_scope_map: std::collections::HashMap<String, &'static str> = mcp_server_configs
            .iter()
            .map(|c| {
                let n = mcp_server_name(c);
                (
                    n.to_string(),
                    crate::util::config::mcp_server_scope(n, scope_cwd),
                )
            })
            .collect();
        let server_count = (mcp_server_configs.len() + acp_pending_names.len()) as u32;
        let mcp_strategy = self.mcp_strategy.get();
        let is_reinit = !existing_client_names.is_empty();
        let event_writer = self.events.writer();
        let init_total_bg = init_total;
        tokio::task::spawn_local(instrument_task!(
            "session.mcp_init_task",
            Parent::Root,
            async move {
                let handshake_start = std::time::Instant::now();

                // Must precede the handshakes: notifications during them are
                // otherwise lost for good.
                let dispatcher_event_tx = mcp_state_bg.lock().await.client_event_tx();

                // Run all handshakes in parallel (outside lock), emitting progress
                // notifications as each server completes so the pager can show
                // incremental "MCP (3/7)" status in the top bar.
                use futures::stream::StreamExt;
                let mut futs = futures::stream::FuturesUnordered::new();
                for client in mcp_clients.iter() {
                    let mcp_state = std::sync::Arc::clone(&mcp_state_bg);
                    let ew = event_writer.clone();
                    let transport = server_transport_map
                        .get(client.server_name())
                        .copied()
                        .unwrap_or("unknown")
                        .to_string();
                    let target = server_target_map
                        .get(client.server_name())
                        .cloned()
                        .unwrap_or_default();
                    let task_event_tx = dispatcher_event_tx.clone();
                    futs.push(async move {
                        let server_name = client.server_name().to_string();
                        let server_start = std::time::Instant::now();
                        let timeout_sec = client.startup_timeout_sec();
                        ew.emit(xai_grok_session_events::Event::McpServerStarting {
                            server_name: server_name.clone(),
                            transport: transport.clone(),
                            target,
                            timeout_sec,
                        });
                        // Must precede the handshake: the handler built during
                        // it snapshots this slot.
                        if let Some(tx) = task_event_tx {
                            client.set_event_tx(Some(tx));
                        }
                        attach_elicitation_tx(&*mcp_state.lock().await, client);
                        // `try_handshake` already bounds the connect with
                        // `startup_timeout_sec`, but the post-handshake
                        // `tools/list` round-trip inside `get_tool_registrations`
                        // is otherwise unbounded. The progress loop below only
                        // finishes once every future resolves, so a server that
                        // connects then stalls on `tools/list` would block
                        // `mcp_initialized` forever and wedge the pager's
                        // "Connecting MCPs (N/M)…" spinner. Budget the whole
                        // per-server init (handshake + initial list) so one hung
                        // server can't hold up the others' completion signal.
                        let init_budget = std::time::Duration::from_secs(
                            timeout_sec.saturating_mul(2).saturating_add(5),
                        );
                        let registrations = match tokio::time::timeout(
                            init_budget,
                            client.get_tool_registrations(mcp_state),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(crate::session::mcp_servers::McpError::Timeout {
                                server: server_name.clone(),
                                timeout_secs: init_budget.as_secs(),
                            }),
                        };
                        match registrations {
                            Ok(handles) => {
                                Ok((server_name, handles, server_start.elapsed(), timeout_sec))
                            }
                            Err(e) => {
                                // Login can only rebuild HTTP clients; other transports keep init_failed.
                                let needs_auth = client.has_auth()
                                    || (client.is_http()
                                        && !client.has_configured_auth_header()
                                        && e.is_auth_rejection());
                                tracing::warn!(
                                    server = server_name.as_str(),
                                    elapsed_ms = server_start.elapsed().as_millis() as u64,
                                    timeout_sec,
                                    error = %e,
                                    needs_auth,
                                    "MCP server failed to initialize"
                                );
                                Err((
                                    server_name,
                                    e,
                                    needs_auth,
                                    server_start.elapsed(),
                                    timeout_sec,
                                ))
                            }
                        }
                    });
                }

                let mut handle_results = Vec::with_capacity(futs.len());
                while let Some(result) = futs.next().await {
                    handle_results.push(result);
                    if let Ok(params) = serde_json::value::to_raw_value(&serde_json::json!({
                        "total": init_total_bg,
                        "connected": handle_results.len() as u32,
                        "sessionId": session_id_owned.as_ref(),
                    })) {
                        gateway.forward_fire_and_forget(acp::ExtNotification::new(
                            crate::extensions::mcp::mcp_methods::INIT_PROGRESS,
                            params.into(),
                        ));
                    }
                }
                drop(futs);

                let mut ui_tools_by_server: std::collections::HashMap<
                    String,
                    Vec<crate::extensions::mcp::McpToolEntry>,
                > = std::collections::HashMap::new();

                {
                    let mut mcp_state = mcp_state_bg.lock().await;

                    if mcp_state.generation() != generation {
                        tracing::info!(
                            "MCP configs changed during background handshakes (gen {} -> {}), discarding",
                            generation,
                            mcp_state.generation()
                        );
                        event_writer.emit(xai_grok_session_events::Event::McpInitCancelled {
                            reason: MCP_INIT_CANCELLED_CONFIG_CHANGED.to_string(),
                        });
                        return;
                    }

                    let mut servers_succeeded: u32 = 0;
                    let mut servers_failed: u32 = 0;
                    let mut servers_auth_required: u32 = 0;
                    let mut total_tools_registered: u32 = 0;
                    let mut failed_server_names: Vec<String> = Vec::new();
                    for result in handle_results {
                        match result {
                            Ok((server_name, registrations, elapsed, timeout_sec)) => {
                                tracing::info!(
                                    server = %server_name,
                                    elapsed_ms = elapsed.as_millis() as u64,
                                    timeout_sec,
                                    tool_count = registrations.len(),
                                    "MCP handshake succeeded",
                                );
                                let tool_count = registrations.len() as u32;
                                let registered_tool_names: Vec<String> = registrations
                                    .iter()
                                    .map(|r| {
                                        let prefix = format!(
                                            "{}{}",
                                            server_name,
                                            crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
                                        );
                                        r.name.strip_prefix(&prefix).unwrap_or(&r.name).to_string()
                                    })
                                    .collect();
                                for reg in registrations {
                                    // Inline register_mcp_tool logic (we don't have &self here).
                                    let qualified_name = reg.name.clone();
                                    let prefix = format!(
                                        "{}{}",
                                        server_name,
                                        crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
                                    );
                                    let unqualified = qualified_name
                                        .strip_prefix(&prefix)
                                        .unwrap_or(&qualified_name)
                                        .to_string();

                                    mcp_state.record_tool_icons(
                                        qualified_name.clone(),
                                        reg.icons.clone(),
                                    );
                                    if let Some(meta) = reg.meta.as_ref() {
                                        mcp_state
                                            .mcp_tool_meta
                                            .insert(qualified_name.clone(), meta.clone());

                                        if meta
                                            .get("ui")
                                            .and_then(|ui| ui.get("resourceUri"))
                                            .is_some()
                                        {
                                            ui_tools_by_server
                                                .entry(server_name.clone())
                                                .or_default()
                                                .push(crate::extensions::mcp::McpToolEntry {
                                                    name: unqualified.clone(),
                                                    display_name: None,
                                                    description: Some(reg.description.clone()),
                                                    meta: Some(meta.clone()),
                                                    icons: reg.icons.clone(),
                                                    enabled: !mcp_state.is_tool_disabled(
                                                        &server_name,
                                                        &unqualified,
                                                    ),
                                                });
                                        }
                                    }

                                    if mcp_state.is_tool_disabled(&server_name, &unqualified) {
                                        tracing::info!(
                                            "Stashing disabled MCP tool '{}' from '{}'",
                                            qualified_name,
                                            server_name
                                        );
                                        mcp_state
                                            .disabled_tool_registrations
                                            .insert(qualified_name, reg);
                                        continue;
                                    }

                                    if reg.model_visible {
                                        if let Err(e) = tool_bridge
                                            .register_mcp_tools(
                                                reg.name,
                                                reg.tool,
                                                Some(reg.input_schema),
                                            )
                                            .await
                                        {
                                            tracing::warn!(
                                                "Failed to register tool '{}' from MCP server '{}': {}",
                                                qualified_name,
                                                server_name,
                                                e
                                            );
                                            event_writer.emit(
                                            xai_grok_session_events::Event::McpToolRegistrationFailed {
                                                server_name: server_name.clone(),
                                                tool_name: qualified_name.clone(),
                                                error: e.to_string(),
                                            },
                                        );
                                        } else {
                                            tracing::debug!(
                                                "Registered MCP tool '{}' from server '{}'",
                                                qualified_name,
                                                server_name
                                            );
                                        }
                                    }
                                }
                                let transport_enum = match server_transport_map
                                    .get(server_name.as_str())
                                    .copied()
                                    .unwrap_or("unknown")
                                {
                                    "stdio" => xai_grok_telemetry::events::McpTransport::Stdio,
                                    "sse" => xai_grok_telemetry::events::McpTransport::Sse,
                                    _ => xai_grok_telemetry::events::McpTransport::Http,
                                };
                                debug_assert!(
                                    xai_grok_telemetry::activity::MCP_SERVERS_CONNECTED.get() >= 1,
                                    "McpServerConnected must stamp a self-inclusive count"
                                );
                                xai_grok_telemetry::session_ctx::log_event(
                                    xai_grok_telemetry::events::McpServerConnected {
                                        server_name: server_name.clone(),
                                        tool_count,
                                        transport: transport_enum,
                                        duration_ms: elapsed.as_millis() as u64,
                                    },
                                );
                                let transport_str = server_transport_map
                                    .get(server_name.as_str())
                                    .copied()
                                    .unwrap_or("unknown");
                                event_writer.emit(
                                    xai_grok_session_events::Event::McpServerConnected {
                                        server_name: server_name.clone(),
                                        transport: transport_str.to_string(),
                                        tool_count,
                                        duration_ms: elapsed.as_millis() as u64,
                                        tools: registered_tool_names,
                                    },
                                );
                                crate::session::telemetry::emit_mcp_connection_span(
                                    "connected",
                                    server_name.as_str(),
                                    transport_str,
                                    server_scope_map
                                        .get(server_name.as_str())
                                        .copied()
                                        .unwrap_or("unknown"),
                                    Some(elapsed.as_millis() as i64),
                                    Some(tool_count as i64),
                                    None,
                                );
                                servers_succeeded += 1;
                                total_tools_registered += tool_count;
                                mcp_state.mark_server_ready(&server_name);
                            }
                            Err((server_name, ref e, needs_auth, elapsed, timeout_sec)) => {
                                let error_cat = if needs_auth {
                                    xai_grok_session_events::McpErrorCategory::AuthRequired
                                } else {
                                    e.error_category()
                                };
                                let error_type_label = match error_cat {
                                    xai_grok_session_events::McpErrorCategory::AuthRequired => {
                                        xai_grok_telemetry::events::McpErrorType::Auth
                                    }
                                    xai_grok_session_events::McpErrorCategory::Timeout => {
                                        xai_grok_telemetry::events::McpErrorType::Timeout
                                    }
                                    _ => xai_grok_telemetry::events::McpErrorType::HandshakeFailed,
                                };
                                xai_grok_telemetry::session_ctx::log_event(
                                    xai_grok_telemetry::events::McpServerFailed {
                                        server_name: server_name.clone(),
                                        error_type: error_type_label,
                                        duration_ms: elapsed.as_millis() as u64,
                                        timeout_sec,
                                    },
                                );
                                let transport_str = server_transport_map
                                    .get(server_name.as_str())
                                    .copied()
                                    .unwrap_or("unknown");
                                crate::session::telemetry::emit_mcp_connection_span(
                                    "failed",
                                    server_name.as_str(),
                                    transport_str,
                                    server_scope_map
                                        .get(server_name.as_str())
                                        .copied()
                                        .unwrap_or("unknown"),
                                    Some(elapsed.as_millis() as i64),
                                    None,
                                    Some(error_type_label.as_str()),
                                );
                                event_writer.emit(
                                    xai_grok_session_events::Event::McpServerFailed {
                                        server_name: server_name.clone(),
                                        transport: Some(transport_str.to_string()),
                                        target: server_target_map
                                            .get(server_name.as_str())
                                            .cloned(),
                                        error_type: error_cat,
                                        error_message: e.to_string(),
                                        duration_ms: Some(elapsed.as_millis() as u64),
                                        timeout_sec: Some(timeout_sec),
                                    },
                                );
                                servers_failed += 1;
                                failed_server_names.push(server_name.clone());
                                if needs_auth {
                                    servers_auth_required += 1;
                                }
                                // Record the failure for status reporting. Auth
                                // failures are owned by `auth_required` (the auth
                                // recovery path clears them); every other failure —
                                // including a handshake that succeeded but timed out
                                // on `tools/list` — goes to `init_failed` so the
                                // server surfaces as Unavailable. Keeping the two
                                // disjoint means a server that later authenticates is
                                // not left stuck Unavailable with zero tools.
                                // Stash the real cause for the model-facing MCP
                                // reminder (vs a bare "connection failed").
                                let detail = (!needs_auth).then(|| {
                                    xai_grok_tools::util::truncate_str_with_marker(
                                        &e.to_string(),
                                        200,
                                    )
                                    .into_owned()
                                });
                                if !needs_auth && e.is_connect_failure() {
                                    // Disk-only spawn can't observe connectivity;
                                    // the unreachable-retry schedule starts here.
                                    mcp_state.record_unreachable_failure(
                                        &server_name,
                                        detail.unwrap_or_default(),
                                    );
                                } else {
                                    mcp_state.record_init_failure(&server_name, needs_auth, detail);
                                }
                                mcp_state.mark_server_ready(&server_name);
                            }
                        }
                    }

                    let inserted_names: Vec<String> = mcp_clients
                        .iter()
                        .map(|c| c.server_name().to_string())
                        .collect();
                    // The clone already carries the sender wired earlier.
                    for c in mcp_clients {
                        let arc = std::sync::Arc::new(c);
                        // `arm_liveness_watcher` self-gates (not-`Ready`, already-armed, ACP).
                        let _ = arc
                            .arm_liveness_watcher(xai_grok_mcp::liveness::DEFAULT_POLL_INTERVAL)
                            .await;
                        mcp_state
                            .owned_clients
                            .insert(arc.server_name().to_string(), arc);
                    }
                    mcp_state.mark_all_servers_ready();
                    tracing::info!(
                        session_id = %session_id_owned,
                        inserted = ?inserted_names,
                        total_clients = mcp_state.owned_clients.len() + mcp_state.shared_clients.len(),
                        elapsed_ms = handshake_start.elapsed().as_millis() as u64,
                        "mcp_bg_handshake: clients inserted, calling notify_waiters"
                    );
                    // Wake `wait_for_mcp_templated_prefix_ready`.
                    mcp_handshakes_done.notify_waiters();

                    xai_grok_telemetry::session_ctx::log_event(
                        xai_grok_telemetry::events::McpInitCompleted {
                            total_duration_ms: handshake_start.elapsed().as_millis() as u64,
                            server_count,
                            servers_succeeded,
                            servers_failed,
                            servers_auth_required,
                            total_tools_registered,
                            strategy: mcp_strategy,
                            is_reinit,
                        },
                    );
                    event_writer.emit(xai_grok_session_events::Event::McpInitCompleted {
                        total_servers: server_count,
                        succeeded: servers_succeeded,
                        failed: servers_failed,
                        auth_required: servers_auth_required,
                        total_tools: total_tools_registered,
                        duration_ms: handshake_start.elapsed().as_millis() as u64,
                        is_reinit,
                        failed_servers: failed_server_names,
                    });
                }

                // Register tools from shared (inherited) MCP clients.
                for (server_name, client) in &shared_clients_for_bg {
                    let regs = match client
                        .get_tool_registrations(Arc::clone(&mcp_state_bg))
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(
                                server = %server_name,
                                error = %e,
                                "Failed to list tools from shared MCP client in bg task"
                            );
                            continue;
                        }
                    };
                    let mut mcp_state = mcp_state_bg.lock().await;
                    for reg in regs {
                        let qualified_name = reg.name.clone();
                        let prefix = format!(
                            "{}{}",
                            server_name,
                            crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
                        );
                        let unqualified = qualified_name
                            .strip_prefix(&prefix)
                            .unwrap_or(&qualified_name)
                            .to_string();

                        mcp_state.record_tool_icons(qualified_name.clone(), reg.icons.clone());
                        if let Some(meta) = reg.meta.as_ref() {
                            mcp_state
                                .mcp_tool_meta
                                .insert(qualified_name.clone(), meta.clone());
                        }

                        if mcp_state.is_tool_disabled(server_name, &unqualified) {
                            mcp_state
                                .disabled_tool_registrations
                                .insert(qualified_name, reg);
                            continue;
                        }

                        if reg.model_visible
                            && let Err(e) = tool_bridge
                                .register_mcp_tools(reg.name, reg.tool, Some(reg.input_schema))
                                .await
                        {
                            tracing::warn!(
                                server = %server_name,
                                tool = %qualified_name,
                                error = %e,
                                "Failed to register shared MCP tool"
                            );
                        }
                    }
                }

                refresh_mcp_snapshot_and_schedule_reminder_with(
                    tool_bridge.clone(),
                    Arc::clone(&mcp_state_bg),
                    managed_mcp_handle.clone(),
                    tool_snapshot,
                    mcp_reminder_dirty,
                    true,
                    &disabled_gateway_tools_bg,
                    mcps_root_bg,
                )
                .await;

                // Emit tools-changed notifications. Each payload
                // carries `sessionId` so the
                // pager routes via `find_session_match` rather than
                // falling back to `app.active_view`.
                for (server_name, tools) in ui_tools_by_server {
                    let payload = crate::extensions::mcp::McpToolsChanged {
                        session_id: session_id_owned.to_string(),
                        server_name,
                        tools,
                    };
                    if let Ok(params) = serde_json::value::to_raw_value(&payload) {
                        gateway.forward_fire_and_forget(acp::ExtNotification::new(
                            crate::extensions::mcp::mcp_methods::TOOLS_CHANGED,
                            params.into(),
                        ));
                    }
                }

                let elapsed = handshake_start.elapsed();
                let elapsed_us = elapsed.as_micros() as u64;
                tracing::info!(
                    target: crate::instrumentation::TARGET,
                    event = "timing",
                    name = "session.mcp_handshakes_bg",
                    elapsed_us,
                );
                tracing::info!("MCP background handshakes completed in {:?}", elapsed);

                let mcp_tool_count = tool_bridge
                    .tool_definitions()
                    .await
                    .iter()
                    .filter(|t| t.function.name.contains("__"))
                    .count();
                if let Ok(params) = serde_json::value::to_raw_value(&serde_json::json!({
                    "sessionId": session_id_owned,
                    "mcpToolCount": mcp_tool_count,
                    "elapsedMs": elapsed.as_millis() as u64,
                })) {
                    gateway.forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/mcp_initialized",
                        params.into(),
                    ));
                }
            }
        ));
    }
    /// Summaries of the currently connected MCP servers, from the live
    /// tool-metadata snapshot. The single source for every consumer of
    /// the server list.
    pub(crate) fn connected_server_summaries(
        &self,
    ) -> Vec<xai_grok_tools::types::tool_index::ServerSummary> {
        use xai_grok_tools::types::tool_index::ToolSearchIndex;
        crate::session::tool_index::Bm25ToolSearchIndex::new(self.tool_metadata_snapshot.clone())
            .list_server_summaries()
    }
    /// Render the tool usage hint appended to every injected MCP reminder
    /// body, with the session's tool names substituted. Shared by the
    /// injector and the `/context` estimate. `None` when the template
    /// fails to render.
    async fn rendered_mcp_hint(&self) -> Option<String> {
        let hint_template = "\nTo use MCP tools, you MUST call `${{ tools.by_kind.search_tool }}` first to retrieve the tool's input schema before calling `${{ tools.by_kind.use_tool }}`. NEVER guess parameter names — always use the exact schema returned by `${{ tools.by_kind.search_tool }}`.";
        self.tool_bridge_handle()
            .render_prompt(hint_template, &serde_json::json!({}))
            .await
    }
    /// The full MCP announcement for the current server set, for
    /// `/context` accounting: the server listing plus the tool usage hint,
    /// as [`Self::maybe_inject_mcp_reminder`] injects in `Full` mode.
    ///
    /// Returns `None` when no servers are connected, or when the active
    /// template carries MCP in its first user message rather than in
    /// reminders. Known approximations: the default reminder mode is
    /// `Delta`, which injects incremental texts (each carrying its own
    /// copy of the hint) rather than this full listing; and the transient
    /// failed or connecting sections and the `<system-reminder>` wrapper
    /// are not counted.
    pub(super) async fn mcp_announcement_snapshot(&self) -> Option<McpAnnouncementSnapshot> {
        let server_summaries = self.connected_server_summaries();
        let mut text =
            xai_grok_tools::implementations::search_tool::build_server_reminder(&server_summaries)?;
        if let Some(hint) = self.rendered_mcp_hint().await {
            text.push_str(&hint);
        }
        Some(McpAnnouncementSnapshot {
            text,
            server_count: server_summaries.len(),
        })
    }
}
/// The MCP server announcement as rendered by `mcp_announcement_snapshot`.
/// The MCP counterpart of `SkillListingSnapshot`.
pub(super) struct McpAnnouncementSnapshot {
    /// The announcement body: server listing plus the tool usage hint.
    pub(super) text: String,
    pub(super) server_count: usize,
}
/// Render the "MCP servers currently connecting" system reminder
/// (pure rendering — see `maybe_inject_mcp_connecting_reminder`).
///
/// Sessions that declared `startupHints.deliveryTools` get different
/// guidance. On surfaces that declare delivery tools the user sees output
/// only through those MCP tools (e.g. a message-posting tool), and the
/// servers providing them are exactly the ones listed as still connecting.
/// The default wording ("proceed with what you can do in the meantime")
/// steers the model into answering in plain text and ending the turn, i.e.
/// replying into a void on such surfaces. Keying on the explicit opt-in —
/// not on `nonInteractive` — keeps the default for every other client
/// (SDK/stdio consumers DO read plain-text responses; subagents report to
/// their parent). Declaring sessions typically also run
/// `McpInitStrategy::Blocking`, so handshakes are awaited at turn start and
/// the servers are expected to become ready while the model works — the
/// right guidance is to keep working and deliver through the tool, not to
/// skip it.
pub(super) fn format_mcp_connecting_reminder(
    connecting: &[String],
    delivery_tools: &[String],
) -> String {
    let mut text =
        "MCP servers currently connecting (tools will become available shortly):\n".to_string();
    for name in connecting {
        text.push_str(&format!("- {name}\n"));
    }
    if delivery_tools.is_empty() {
        text.push_str(
            "\nDo not attempt to use tools from these servers yet. \
             If the user's request likely requires one of these servers, \
             mention that the server is still connecting and proceed with \
             what you can do in the meantime.",
        );
    } else {
        text.push_str(&format!(
            "\nThese servers are being awaited and their tools are expected \
             to become available as you work — use them normally, and if a \
             call reports the tool as unavailable, retry it after your other \
             work rather than giving up. User-visible output from this \
             session is delivered ONLY through: {}. Do NOT end the turn \
             without delivering your answer through the appropriate \
             delivery tool.",
            delivery_tools.join(", ")
        ));
    }
    text
}
