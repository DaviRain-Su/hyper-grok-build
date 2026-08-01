#![cfg_attr(rustfmt, rustfmt::skip)]
#![allow(unused_imports)]
//! [`acp::Agent`] trait implementation for [`MvpAgent`].
//! Co-located child of `mvp_agent` (`use super::*`).
use super::*;
/// Which `x_search` sub-tools enforce the date cutoff, sent in `initialize`. `x_user_search` and
/// `x_thread_fetch` are `false`: they don't honor it yet.
#[derive(serde::Serialize)]
struct ToolOverridesCapability {
    x_keyword_search: bool,
    x_semantic_search: bool,
    x_user_search: bool,
    x_thread_fetch: bool,
}
const TOOL_OVERRIDES_CAPABILITY: ToolOverridesCapability = ToolOverridesCapability {
    x_keyword_search: true,
    x_semantic_search: true,
    x_user_search: false,
    x_thread_fetch: false,
};
fn tool_overrides_capability() -> serde_json::Value {
    serde_json::to_value(TOOL_OVERRIDES_CAPABILITY)
        .expect("ToolOverridesCapability is always serializable")
}
async fn read_applied_tool_overrides(
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<SessionCommand>,
) -> Option<xai_grok_sampling_types::ToolOverrides> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(SessionCommand::GetToolOverrides {
            respond_to: tx,
        })
        .is_err()
    {
        tracing::warn!("tool-overrides echo: session actor command channel closed");
        return None;
    }
    match rx.await {
        Ok(overrides) => overrides,
        Err(_) => {
            tracing::warn!("tool-overrides echo: session actor dropped the response channel");
            None
        }
    }
}
fn insert_applied_tool_overrides(
    meta: &mut serde_json::Map<String, serde_json::Value>,
    echo: Option<&xai_grok_sampling_types::ToolOverrides>,
) {
    if let Some(overrides) = echo {
        meta.insert(
            "toolOverrides".to_string(),
            serde_json::to_value(overrides)
                .expect("ToolOverrides is always serializable"),
        );
    }
}
#[async_trait::async_trait(?Send)]
impl acp::Agent for MvpAgent {
    /// In the meta, we provide
    ///   - model_state: the model state, useful for the client to display available models and the default model.
    ///
    /// SINGLE-CALL INVARIANT: this method is the sole writer of
    /// `self.auth_method_id` during initialization. It is called exactly once
    /// per agent process by the ACP server before any session-creating
    /// requests, while `auth_method_id` is still `None` (initialized at
    /// `MvpAgent::new`). The auth-method block below relies on that
    /// invariant when it unconditionally writes the default id returned by
    /// `auth_method::build_auth_methods`. If you ever need to call
    /// `initialize()` more than once, restore an `is_none()` guard around
    /// the `auth_method_id` write at the call site so a re-init doesn't
    /// silently downgrade an api-key user to a session-token user.
    async fn initialize(
        &self,
        arguments: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        tracing::debug!(target: "sampling_log", "Received initialize request");
        xai_grok_telemetry::unified_log::info("agent initialized", None, None);
        self.start_subagent_coordinator();
        if self.cfg.borrow().remote_settings.is_none() {
            self.spawn_settings_reapply();
        }
        let (auto_gc_policy, run_auto_gc) = {
            let cfg = self.cfg.borrow();
            let has_remote = cfg.remote_settings.is_some();
            let run = has_remote || !crate::util::config::resolve_remote_fetch_enabled();
            (cfg.resolve_worktree_auto_gc(), run)
        };
        if !run_auto_gc {
            tracing::debug!(
                "auto worktree gc deferred until remote_settings are available"
            );
        }
        tokio::task::spawn_blocking(move || {
            crate::session::worktree_pool::cleanup_stale_pool_worktrees(None);
            if !run_auto_gc {
                return;
            }
            let opts = xai_fast_worktree::AutoGcOptions::from_resolved(auto_gc_policy);
            if let Err(e) = xai_fast_worktree::WorktreeDb::open_default()
                .and_then(|db| xai_fast_worktree::maybe_auto_gc(&db, &opts))
            {
                tracing::warn!(error = %e, "auto worktree gc failed");
            }
        });
        tokio::task::spawn_blocking(|| {
            crate::session::persistence::cleanup_stale_sessions(None);
        });
        {
            let root = crate::util::grok_home::grok_home();
            crate::session::storage::search::SEARCH_INDEX_MANAGER.bootstrap_once(root);
        }
        const PERMISSION_CLEANUP_TTL_DAYS: u64 = 30;
        static CLEANUP_PERMISSIONS_ONCE: std::sync::Once = std::sync::Once::new();
        CLEANUP_PERMISSIONS_ONCE
            .call_once(|| {
                tokio::task::spawn(
                    xai_grok_workspace::permission::cleanup_stale_permission_state(
                        std::time::Duration::from_secs(
                            PERMISSION_CLEANUP_TTL_DAYS * 24 * 60 * 60,
                        ),
                    ),
                );
            });
        xai_grok_workspace::trust::migrate_legacy_hook_trust();
        if let Some(auth) = self.auth_manager.current() {
            let user_id = auth.user_id.trim();
            let needs_user_info = user_id.is_empty()
                || user_id.eq_ignore_ascii_case("unknown");
            xai_grok_telemetry::unified_log::info(
                "auth init user_info check",
                None,
                Some(
                    serde_json::json!({
                    "user_id": user_id,
                    "needs_user_info": needs_user_info,
                    "key_prefix": crate::auth::token_suffix(&auth.key),
                    "rt_prefix": auth.refresh_token.as_deref().map(crate::auth::token_suffix),
                }),
                ),
            );
            if needs_user_info && let Err(e) = self.auth_manager.update(auth).await {
                tracing::warn!(
                    "Failed to refresh user info from proxy during new_session: {}",
                    e
                );
            }
        }
        if !self.tier_allowed.get() && let Some(auth) = self.auth_manager.current() {
            self.enforce_grok_code_access(&auth).await;
        }
        self.maybe_sync_bundle_in_background(false);
        let mut client_type = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientType"))
            .and_then(|v| serde_json::from_value::<ClientType>(v.clone()).ok())
            .unwrap_or_default();
        let client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(ref id) = client_identifier {
            tracing::info!("Client identifier set to: {}", id);
        }
        if client_type == ClientType::Generic {
            match client_identifier.as_deref() {
                Some("grok-web") => client_type = ClientType::GrokWeb,
                Some("nebula") => client_type = ClientType::Nebula,
                Some("grok-code-extension") => client_type = ClientType::Extension,
                Some("grok-desktop") => client_type = ClientType::Desktop,
                _ => {}
            }
        }
        *self.client_type.borrow_mut() = client_type;
        tracing::info!("Client type set to: {:?}", client_type);
        let code_nav_enabled = Self::parse_code_nav_capability(&arguments);
        self.code_nav_enabled.set(code_nav_enabled);
        tracing::info!(
            code_nav_enabled,
            client_type = ?client_type,
            event = "code_nav_capability_parsed",
            "code-nav capability initialized from initialize request; \
             index will start lazily on first x.ai/code/* request if eligible"
        );
        let interactive_trust_client = Self::parse_interactive_trust_capability(
            &arguments,
        );
        self.interactive_trust_client.set(interactive_trust_client);
        let client_supports_mcp_apps = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("mcpApps"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if client_supports_mcp_apps {
            tracing::info!("Client supports MCP Apps");
        }
        let buffering_settings = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("bufferingSettings"))
            .map(|value| serde_json::from_value::<
                update_chunk_merge::BufferingSettings,
            >(value.clone()))
            .transpose()
            .map_err(|err| {
                tracing::warn!(
                    error = ?err,
                    "Failed to parse buffering settings from init meta"
                );
                err
            })
            .unwrap_or(None);
        tracing::info!(?buffering_settings, "Buffering settings from init");
        *self.buffering_settings.borrow_mut() = buffering_settings;
        if self.initialize_request.set(arguments).is_err() {
            tracing::info!("Initialize called on reconnect (already initialized)");
        }
        let pre = self
            .auth_manager
            .current()
            .map(|a| (
                crate::auth::token_suffix(&a.key).to_owned(),
                a
                    .refresh_token
                    .as_deref()
                    .map(|t| crate::auth::token_suffix(t).to_owned()),
            ));
        self.auth_manager.force_reload_from_disk();
        let post = self
            .auth_manager
            .current()
            .map(|a| (
                crate::auth::token_suffix(&a.key).to_owned(),
                a
                    .refresh_token
                    .as_deref()
                    .map(|t| crate::auth::token_suffix(t).to_owned()),
            ));
        xai_grok_telemetry::unified_log::info(
            "auth init disk refresh",
            None,
            Some(
                serde_json::json!({
                "pre_key": pre.as_ref().map(|p| &p.0),
                "pre_rt": pre.as_ref().and_then(|p| p.1.as_deref()),
                "post_key": post.as_ref().map(|p| &p.0),
                "post_rt": post.as_ref().and_then(|p| p.1.as_deref()),
                "changed": pre.as_ref().map(|p| &p.0) != post.as_ref().map(|p| &p.0),
            }),
            ),
        );
        xai_grok_telemetry::unified_log::info(
            "auth: initialize() refreshed auth state from disk",
            None,
            Some(
                serde_json::json!({
                "has_current": self.auth_manager.current().is_some(),
                "is_expired": self.auth_manager.is_expired(),
                "auth_mode": self.auth_manager.current().map(|a| format!("{:?}", a.auth_mode)),
            }),
            ),
        );
        if !self.cfg.borrow().grok_com_config.api_key_auth_disabled()
            && auth_method::read_xai_api_key_env().is_err()
            && let Some(api_key) = crate::auth::read_api_key(
                &crate::util::grok_home::grok_home(),
            )
        {
            unsafe { std::env::set_var("XAI_API_KEY", &api_key) };
            tracing::info!("auth: loaded API key from auth.json (xai::api_key scope)");
            xai_grok_telemetry::unified_log::info(
                "auth: loaded API key from auth.json (xai::api_key scope)",
                None,
                None,
            );
        }
        let disable_api_key_auth = self
            .cfg
            .borrow()
            .grok_com_config
            .api_key_auth_disabled();
        {
            let cfg = self.cfg.borrow();
            let gc = &cfg.grok_com_config;
            if disable_api_key_auth || gc.force_login_team_uuid.is_some() {
                xai_grok_telemetry::unified_log::info(
                    "auth: enterprise login policy active",
                    None,
                    Some(
                        serde_json::json!({
                        "force_login_team_uuid": gc.force_login_team_uuid.as_ref().map(|t| format!("{t:?}")),
                        "disable_api_key_auth_knob": gc.disable_api_key_auth,
                        "api_key_auth_disabled": disable_api_key_auth,
                    }),
                    ),
                );
            }
        }
        let has_external_api_key = auth_method::should_advertise_xai_api_key(
            disable_api_key_auth,
            self.models_manager.models().values(),
        );
        let init_has_current = self.auth_manager.current().is_some();
        let init_is_expired = self.auth_manager.is_expired();
        xai_grok_telemetry::unified_log::info(
            "auth init token state",
            None,
            Some(
                serde_json::json!({
                "has_current": init_has_current,
                "is_expired": init_is_expired,
            }),
            ),
        );
        let mut has_cached_token = init_has_current;
        if !init_has_current && init_is_expired {
            let refreshed = matches!(
                tokio::time::timeout(
                    crate::http::STARTUP_AUTH_REFRESH_TIMEOUT,
                    self.auth_manager.auth(),
                )
                .await,
                Ok(Ok(_))
            );
            if refreshed {
                tracing::debug!(
                    auth_type = ?self.auth_type(),
                    "auth: initialize() silent refresh succeeded",
                );
                xai_grok_telemetry::unified_log::info(
                    "auth: initialize() silent refresh succeeded",
                    None,
                    Some(
                        serde_json::json!({ "auth_type": format!("{:?}", self.auth_type()) }),
                    ),
                );
                has_cached_token = true;
            } else if !self.auth_manager.requires_manual_reauth() {
                tracing::info!("auth: silent refresh failed transiently; advertising cached_token");
                xai_grok_telemetry::unified_log::info(
                    "auth: initialize() silent refresh failed transiently, keeping cached_token",
                    None,
                    None,
                );
                has_cached_token = true;
            } else {
                tracing::warn!(
                    "auth: token expired, silent refresh failed - re-authentication required"
                );
                xai_grok_telemetry::unified_log::warn(
                    "auth: token expired, silent refresh failed - re-authentication required",
                    None,
                    None,
                );
            }
        }
        let (
            login_label,
            has_auth_provider,
            has_enterprise_oidc,
            enterprise_oidc_issuer,
        ) = {
            let cfg = self.cfg.borrow();
            let issuer = cfg.grok_com_config.oidc.as_ref().map(|o| o.issuer.clone());
            (
                cfg.grok_com_config.auth_provider_label.clone(),
                cfg.grok_com_config.auth_provider_command.is_some(),
                cfg.grok_com_config.oidc.is_some(),
                issuer,
            )
        };
        if has_enterprise_oidc {
            let issuer = enterprise_oidc_issuer
                .as_deref()
                .expect(
                    "enterprise_oidc_issuer must be Some when has_enterprise_oidc is true",
                );
            tracing::info!(
                issuer = %issuer,
                "auth: advertising enterprise OIDC auth method",
            );
            xai_grok_telemetry::unified_log::info(
                "auth: advertising enterprise OIDC auth method",
                None,
                Some(serde_json::json!({ "issuer": issuer })),
            );
        } else {
            tracing::info!(
                label = ?login_label,
                has_auth_provider,
                "auth: advertising grok.com auth method",
            );
        }
        let preferred_method = self.cfg.borrow().grok_com_config.preferred_method;
        let has_external_api_key = match preferred_method {
            Some(crate::auth::PreferredAuthMethod::Oidc) => false,
            _ => has_external_api_key,
        };
        let has_cached_token = match preferred_method {
            Some(crate::auth::PreferredAuthMethod::ApiKey) => false,
            _ => has_cached_token,
        };
        let built = auth_method::build_auth_methods(auth_method::AuthMethodsBuildInputs {
            has_external_api_key,
            has_cached_token,
            has_enterprise_oidc,
            enterprise_oidc_issuer: enterprise_oidc_issuer.as_deref(),
            login_label: login_label.as_deref(),
            has_auth_provider_command: has_auth_provider,
            preferred_method,
        });
        let auth_methods = built.methods;
        xai_grok_telemetry::unified_log::info(
            "auth: initialize() built auth_methods for ACP response",
            None,
            Some(
                serde_json::json!({
                "grok_home": crate::util::grok_home::grok_home().display().to_string(),
                "HOME": std::env::var("HOME").unwrap_or_else(|_| "(unset)".into()),
                "has_external_api_key": has_external_api_key,
                "disable_api_key_auth": disable_api_key_auth,
                "has_cached_token": has_cached_token,
                "has_enterprise_oidc": has_enterprise_oidc,
                "init_has_current": init_has_current,
                "init_is_expired": init_is_expired,
                "auth_mode": self.auth_manager.current().map(|a| format!("{:?}", a.auth_mode)),
                "methods": auth_methods.iter().map(|m| m.id().0.as_ref()).collect::<Vec<_>>(),
                "default_auth_method_id": built.default_auth_method_id.as_ref().map(|id| id.0.as_ref()),
            }),
            ),
        );
        debug_assert!(
            !has_external_api_key
                || matches!(
                    auth_methods
                        .first()
                        .map(|m| auth_method::AuthMethodKind::from_id(m.id())),
                    Some(auth_method::AuthMethodKind::XaiApiKey)
                ),
            "BYOK invariant violated: xai.api_key MUST be auth_methods.first() \
             when has_external_api_key is true; got {:?}",
            auth_methods.first().map(|m| m.id()),
        );
        let default_auth_method_id_wire: Option<String> = built
            .default_auth_method_id
            .as_ref()
            .map(|id| id.0.to_string());
        if let Some(default_id) = built.default_auth_method_id {
            xai_grok_telemetry::unified_log::info(
                "auth method selection",
                None,
                Some(
                    serde_json::json!({
                    "default_auth_method_id": default_id.0.as_ref(),
                    "has_external_api_key": has_external_api_key,
                    "has_cached_token": has_cached_token,
                    "methods_first": auth_methods.first().map(|m| m.id().0.as_ref()),
                    "methods_count": auth_methods.len(),
                }),
                ),
            );
            self.set_auth_method(default_id);
        }
        self.sync_process_static_api_key(None);
        let current_working_directory = self.launch_cwd.clone();
        let hostname = gethostname::gethostname();
        let mcp_servers: Vec<crate::extensions::mcp::McpServerEntry> = Vec::new();
        let fetch_managed_mcps = self.cfg.borrow().managed_mcps_enabled
            && self.can_fetch_managed_mcps();
        if self.cfg.borrow().managed_mcps_enabled && !fetch_managed_mcps {
            tracing::info!("Managed MCP fetch: DISABLED");
        }
        self.spawn_initialize_launch_mcp_setup(fetch_managed_mcps);
        self.spawn_managed_gateway_tool_catalog_fetch();
        {
            let agent_ref = LocalRef::new(self);
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                agent_ref.get().emit_announcements(AnnouncementsPushMode::SeedNewClient);
            });
        }
        self.spawn_announcements_refresh();
        self.spawn_heap_profile_monitor();
        let init_model_state = if crate::agent::chat_modes::process_chat_mode_enabled() {
            self.chat_modes.model_state().await
        } else {
            self.model_state(None)
        };
        let session_capabilities = if crate::agent::chat_modes::process_chat_mode_enabled() {
            acp::SessionCapabilities::new()
        } else {
            acp::SessionCapabilities::new().list(acp::SessionListCapabilities::new())
        };
        Ok(
            acp::InitializeResponse::new(acp::ProtocolVersion::V1)
                .agent_capabilities(
                    acp::AgentCapabilities::new()
                        .load_session(true)
                        .meta(
                            serde_json::json!({
                    "x.ai/fs_notify": true,
                    // Advertised so SDKs can warn when a registration depends on
                    // hook behavior this agent doesn't honor.
                    "x.ai/hooks": {
                        "blockingEvents": crate::extensions::hooks::ADVERTISED_BLOCKING_EVENTS,
                        "decisions": crate::extensions::hooks::ADVERTISED_DECISIONS,
                        "stopSignals": crate::extensions::hooks::ADVERTISED_STOP_SIGNALS,
                    },
                    "x.ai/capabilities": {
                        "toolOverrides": tool_overrides_capability(),
                    },
                })
                                .as_object()
                                .cloned(),
                        )
                        .prompt_capabilities(
                            acp::PromptCapabilities::new().embedded_context(true),
                        )
                        .mcp_capabilities(
                            acp::McpCapabilities::new().http(true).sse(true),
                        )
                        .session_capabilities(session_capabilities),
                )
                .auth_methods(auth_methods)
                .meta({
                    let metadata = parse_json_object_env("GROK_AGENT_METADATA");
                    serde_json::json!({
                    "grokShell": true,
                    // Re-deriving this precedence client-side has regressed OIDC
                    // refresh, so clients consume the agent's choice from here.
                    "defaultAuthMethodId": default_auth_method_id_wire,
                    // The agent can drive in-process SDK MCP servers over the ACP reverse
                    // channel (`x.ai/mcp/sdk_call`); the SDK reads this to enable transport="acp".
                    (xai_grok_mcp::wire::MCP_SDK): true,
                    // `session/new` / `session/load` accept per-session plugin roots in
                    // `_meta.pluginDirs`; the SDKs gate `GrokOptions.plugins` on this.
                    (SESSION_PLUGIN_DIRS_CAPABILITY_KEY): true,
                    "currentWorkingDirectory": current_working_directory.to_string_lossy().to_string(),
                    "agentVersion": xai_grok_version::VERSION,
                    "agentId": agent_id(),
                    "agentInstanceId": agent_instance_id(),
                    "hostname": hostname.to_string_lossy().to_string(),
                    "modelState": init_model_state,
                    "mcpServers": mcp_servers,
                    "mcpApps": client_supports_mcp_apps,
                    "metadata": metadata,
                    "availableCommands": crate::session::slash_commands::builtin_commands(self.command_availability()),
                    "cancelRewind": self.cfg.borrow().resolve_cancel_rewind().value,
                    // Resolved session-recap state (remote settings / config / env;
                    // default ON). The client gates BOTH its automatic
                    // away-recap poll and the manual `/recap` on this so a
                    // disabled feature produces zero `x.ai/recap` traffic.
                    "sessionRecap": self.cfg.borrow().is_session_recap_enabled(),
                    "voiceMode": self.cfg.borrow().is_voice_mode_enabled(),
                })
                        .as_object()
                        .cloned()
                }),
        )
    }
    async fn authenticate(
        &self,
        arguments: acp::AuthenticateRequest,
    ) -> Result<AuthenticateResponse, acp::Error> {
        tracing::info!(method = %arguments.method_id.0, "auth: authenticate request");
        xai_grok_telemetry::unified_log::info(
            "auth started",
            None,
            Some(serde_json::json!({"method": arguments.method_id.0.as_ref()})),
        );
        if let Some(preferred) = self.cfg.borrow().grok_com_config.preferred_method {
            let kind = auth_method::AuthMethodKind::from_id(&arguments.method_id);
            // Kimi Code / Codex / Claude are independent third-party channels
            // and are always allowed alongside preferred xAI methods.
            let allowed = matches!(
                kind,
                auth_method::AuthMethodKind::KimiCode
                    | auth_method::AuthMethodKind::OpenAiCodex
                    | auth_method::AuthMethodKind::AnthropicClaude
                    | auth_method::AuthMethodKind::GitHubCopilot
                    | auth_method::AuthMethodKind::Radius
            ) || match preferred {
                    crate::auth::PreferredAuthMethod::ApiKey => kind.is_api_key(),
                    crate::auth::PreferredAuthMethod::Oidc => kind.is_session_based(),
                };
            if !allowed {
                let msg = match preferred {
                    crate::auth::PreferredAuthMethod::ApiKey => {
                        auth_method::PREFERRED_API_KEY_UNAVAILABLE
                    }
                    crate::auth::PreferredAuthMethod::Oidc => {
                        "preferred_method=oidc; API-key auth is not allowed."
                    }
                };
                emit_login_span(
                    false,
                    arguments.method_id.0.as_ref(),
                    None,
                    Some("preferred_method_mismatch"),
                );
                return Err(acp::Error::auth_required().data(msg));
            }
        }
        match arguments.method_id.0.as_ref() {
            auth_method::XAI_API_KEY_METHOD_ID => {
                if self.cfg.borrow().grok_com_config.api_key_auth_disabled() {
                    emit_login_span(false, "api_key", None, Some("disabled_by_admin"));
                    return Err(
                        acp::Error::auth_required()
                            .data("API-key auth is disabled by your administrator."),
                    );
                }
                let mut sampling_config = self.sampling_config.borrow_mut();
                if sampling_config.api_key.is_none() {
                    if let Ok(api_key) = auth_method::read_xai_api_key_env() {
                        sampling_config.api_key = Some(api_key.clone());
                        if let Err(e) = crate::auth::store_api_key(
                            &crate::util::grok_home::grok_home(),
                            &api_key,
                        ) {
                            tracing::warn!("failed to persist API key to auth.json: {e}");
                            xai_grok_telemetry::unified_log::warn(
                                "failed to persist API key to auth.json",
                                None,
                                Some(serde_json::json!({ "error": e.to_string() })),
                            );
                        }
                    } else if !self
                        .models_manager
                        .models()
                        .values()
                        .any(|m| m.has_own_credentials())
                    {
                        emit_login_span(false, "api_key", None, Some("no_credentials"));
                        return Err(
                            acp::Error::auth_required()
                                .data(
                                    "Set XAI_API_KEY or add api_key/env_key to config.toml.",
                                ),
                        );
                    }
                }
                self.set_auth_method(arguments.method_id.clone());
                self.sync_process_static_api_key(None);
                self.ensure_telemetry_client();
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                emit_login_span(true, "api_key", None, None);
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: "api_key".to_string(),
                    user_id: None,
                });
                Ok(Default::default())
            }
            auth_method::CACHED_TOKEN_AUTH_METHOD_ID => {
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                if auth_meta.force_interactive {
                    return self
                        .authenticate(
                            acp::AuthenticateRequest::new(
                                    acp::AuthMethodId::new(auth_method::OIDC_METHOD_ID),
                                )
                                .meta(arguments.meta),
                        )
                        .await;
                }
                let current_auth = self.auth_manager.current();
                let has_current = current_auth.is_some();
                let is_expired = self.auth_manager.is_expired();
                let is_devbox = crate::auth::devbox_login::is_devbox_environment();
                let is_legacy = current_auth
                    .as_ref()
                    .is_some_and(|a| a.auth_mode == crate::auth::AuthMode::WebLogin);
                xai_grok_telemetry::unified_log::info(
                    "auth cached_token check",
                    None,
                    Some(
                        serde_json::json!({
                        "has_current": has_current,
                        "is_expired": is_expired,
                        "is_devbox": is_devbox,
                        "is_legacy": is_legacy,
                    }),
                    ),
                );
                let pin_blocks_oidc_mint = matches!(
                    self.cfg.borrow().grok_com_config.preferred_method,
                    Some(crate::auth::PreferredAuthMethod::ApiKey)
                );
                if is_devbox && is_legacy && !pin_blocks_oidc_mint {
                    xai_grok_telemetry::unified_log::info(
                        "auth cached_token: devbox legacy migration starting",
                        None,
                        None,
                    );
                    match crate::auth::devbox_login::mint_devbox_auth(&self.auth_manager)
                        .await
                    {
                        Ok(new_auth) => {
                            match self
                                .auth_manager
                                .save_without_enrichment(new_auth)
                                .await
                            {
                                Ok(_) => {
                                    if let Err(e) = self
                                        .auth_manager
                                        .remove_scope(crate::auth::LEGACY_AUTH_SCOPE)
                                    {
                                        tracing::warn!(error = ?e, "auth: failed to remove legacy scope (non-fatal)");
                                    }
                                    xai_grok_telemetry::unified_log::info(
                                        "auth cached_token: devbox legacy migration succeeded",
                                        None,
                                        None,
                                    );
                                }
                                Err(e) => {
                                    xai_grok_telemetry::unified_log::warn(
                                        "auth cached_token: devbox migration save failed",
                                        None,
                                        Some(serde_json::json!({ "error": e.to_string() })),
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            xai_grok_telemetry::unified_log::warn(
                                "auth cached_token: devbox mint failed, will reject legacy token",
                                None,
                                Some(serde_json::json!({ "error": format!("{e}") })),
                            );
                        }
                    }
                }
                if self.auth_manager.current().is_none()
                    && self.auth_manager.is_expired()
                {
                    let am = self.auth_manager.clone();
                    let refresh = tokio::spawn(async move { am.auth().await });
                    match tokio::time::timeout(
                            crate::http::STARTUP_AUTH_REFRESH_TIMEOUT,
                            refresh,
                        )
                        .await
                    {
                        Ok(Ok(Ok(_))) => {}
                        outcome => {
                            tracing::debug!(
                            timed_out = outcome.is_err(),
                            "auth: cached_token pre-check refresh did not produce a token (yet)"
                        )
                        }
                    }
                }
                let resolved = self
                    .auth_manager
                    .current()
                    .or_else(|| {
                        if self.auth_manager.is_expired()
                            && !self.auth_manager.requires_manual_reauth()
                        {
                            xai_grok_telemetry::unified_log::info(
                                "auth cached_token: accepting expired-but-refreshable session",
                                None,
                                None,
                            );
                            self.auth_manager.current_or_expired()
                        } else {
                            None
                        }
                    });
                let Some(auth) = resolved else {
                    let message = if self.auth_manager.is_expired() {
                        "Session expired, re-authentication required"
                    } else {
                        "No cached auth token found"
                    };
                    tracing::info!(%message, "cached_token missing/expired, falling through");
                    xai_grok_telemetry::unified_log::warn(
                        "auth cached_token fallthrough",
                        None,
                        Some(serde_json::json!({ "reason": message })),
                    );
                    return self
                        .authenticate_after_cached_token_unavailable(arguments)
                        .await;
                };
                if auth.auth_mode == crate::auth::AuthMode::WebLogin {
                    tracing::info!("auth: rejecting legacy WebLogin token");
                    xai_grok_telemetry::unified_log::warn(
                        "auth cached_token legacy rejected",
                        None,
                        Some(
                            serde_json::json!({ "auth_mode": format!("{:?}", auth.auth_mode) }),
                        ),
                    );
                    self.auth_manager.clear_in_memory();
                    if let Err(e) = self
                        .auth_manager
                        .remove_scope(crate::auth::LEGACY_AUTH_SCOPE)
                    {
                        tracing::warn!(error = ?e, "auth: failed to remove legacy scope during WebLogin rejection (non-fatal)");
                    }
                    return self
                        .authenticate_after_cached_token_unavailable(arguments)
                        .await;
                }
                self.enforce_grok_code_access(&auth).await;
                self.maybe_sync_bundle_in_background(false);
                let auth_for_settings = auth.clone();
                {
                    let mut sampling_config = self.sampling_config.borrow_mut();
                    sampling_config.api_key = Some(auth.key);
                    tracing::debug!("auth: cached_token handler set api_key (SessionToken)");
                    xai_grok_telemetry::unified_log::debug(
                        "auth: cached_token handler set api_key (SessionToken)",
                        None,
                        None,
                    );
                }
                self.set_auth_method(arguments.method_id.clone());
                self.ensure_telemetry_client();
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                let uid = self.auth_manager.current().map(|a| a.user_id);
                emit_login_span(true, "cached_token", uid.as_deref(), None);
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: "cached_token".to_string(),
                    user_id: uid,
                });
                self.spawn_post_auth_settings(auth_for_settings);
                Ok(self.auth_response_with_meta())
            }
            auth_method::KIMI_CODE_METHOD_ID => {
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                let client_seq = auth_meta.request_seq;
                let mut cancelled = false;
                let (cancel, _guard) = self.interactive_auth.begin(None, client_seq);
                let auth_result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        Err(anyhow::anyhow!("Authentication cancelled"))
                    }
                    r = crate::auth::kimi::run_kimi_code_login() => r,
                };
                let auth = auth_result.map_err(|e| {
                    emit_login_span(
                        false,
                        "kimi-code",
                        None,
                        Some(if cancelled {
                            "login_cancelled"
                        } else {
                            "login_flow_failed"
                        }),
                    );
                    let mut err = acp::Error::auth_required();
                    err.message = e.to_string();
                    err
                })?;
                // Re-stamp kimi-code/* catalog entries with the new bearer.
                self.models_manager.restamp_platform_credentials();
                // Do not replace the primary xAI AuthManager session.
                emit_login_span(true, "kimi-code", None, None);
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: "kimi-code".to_string(),
                    user_id: None,
                });
                let _ = auth;
                Ok(Default::default())
            }
            auth_method::OPENAI_CODEX_METHOD_ID => {
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                let client_seq = auth_meta.request_seq;
                let mut cancelled = false;
                let auth_result = if !auth_meta.headless {
                    // Interactive (TUI): push the auth URL and accept a
                    // manually pasted code through the client UI channels.
                    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
                    let (code_tx, code_rx) = tokio::sync::mpsc::channel(1);
                    let (cancel, _guard) = self.interactive_auth.begin(
                        Some(crate::auth::single_flight::AttemptChannels::new(
                            code_tx, url_rx,
                        )),
                        client_seq,
                    );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::openai_codex::run_openai_codex_login(
                            Some(crate::auth::AuthChannels {
                                url_tx: Some(url_tx),
                                code_rx,
                            }),
                            crate::auth::openai_codex::CodexLoginMethod::Browser,
                        ) => r,
                    }
                } else {
                    // Headless ACP: device code is reliable over SSH/CI where
                    // loopback browser callbacks are unavailable (even if the
                    // agent process has a pseudo-TTY on stdin).
                    let (cancel, _guard) = self.interactive_auth.begin(None, client_seq);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::openai_codex::run_openai_codex_login(
                            None,
                            crate::auth::openai_codex::CodexLoginMethod::DeviceCode,
                        ) => r,
                    }
                };
                let auth = auth_result.map_err(|e| {
                    emit_login_span(
                        false,
                        "openai-codex",
                        None,
                        Some(if cancelled {
                            "login_cancelled"
                        } else {
                            "login_flow_failed"
                        }),
                    );
                    let mut err = acp::Error::auth_required();
                    err.message = e.to_string();
                    err
                })?;
                // Re-stamp openai-codex/* catalog entries with the new bearer.
                self.models_manager.restamp_platform_credentials();
                // Do not replace the primary xAI AuthManager session.
                emit_login_span(true, "openai-codex", None, None);
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: "openai-codex".to_string(),
                    user_id: None,
                });
                let _ = auth;
                Ok(Default::default())
            }
            auth_method::ANTHROPIC_CLAUDE_METHOD_ID => {
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                let client_seq = auth_meta.request_seq;
                let mut cancelled = false;
                // Interactive (TUI): push the auth URL + accept a pasted code.
                // Headless: no channels — the loopback/browser path is driven
                // by the CLI login binary, not this ACP method.
                let auth_result = if !auth_meta.headless {
                    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
                    let (code_tx, code_rx) = tokio::sync::mpsc::channel(1);
                    let (cancel, _guard) = self.interactive_auth.begin(
                        Some(crate::auth::single_flight::AttemptChannels::new(
                            code_tx, url_rx,
                        )),
                        client_seq,
                    );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::anthropic_claude::run_anthropic_claude_login(
                            Some(crate::auth::AuthChannels {
                                url_tx: Some(url_tx),
                                code_rx,
                            }),
                        ) => r,
                    }
                } else {
                    let (cancel, _guard) = self.interactive_auth.begin(None, client_seq);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::anthropic_claude::run_anthropic_claude_login(None) => r,
                    }
                };
                let auth = auth_result.map_err(|e| {
                    emit_login_span(
                        false,
                        "anthropic-claude",
                        None,
                        Some(if cancelled {
                            "login_cancelled"
                        } else {
                            "login_flow_failed"
                        }),
                    );
                    let mut err = acp::Error::auth_required();
                    err.message = e.to_string();
                    err
                })?;
                // Re-stamp anthropic-claude/* catalog entries with the new bearer.
                self.models_manager.restamp_platform_credentials();
                // Do not replace the primary xAI AuthManager session.
                emit_login_span(true, "anthropic-claude", None, None);
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: "anthropic-claude".to_string(),
                    user_id: None,
                });
                let _ = auth;
                Ok(Default::default())
            }
            auth_method::RADIUS_METHOD_ID => {
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                let client_seq = auth_meta.request_seq;
                let mut cancelled = false;
                let auth_result = if !auth_meta.headless {
                    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
                    let (code_tx, code_rx) = tokio::sync::mpsc::channel(1);
                    let (cancel, _guard) = self.interactive_auth.begin(
                        Some(crate::auth::single_flight::AttemptChannels::new(
                            code_tx, url_rx,
                        )),
                        client_seq,
                    );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        result = crate::auth::radius::run_radius_login_with_channels(
                            None,
                            crate::auth::radius::RadiusLoginMethod::Browser,
                            Some(crate::auth::AuthChannels {
                                url_tx: Some(url_tx),
                                code_rx,
                            }),
                        ) => result,
                    }
                } else {
                    let (cancel, _guard) = self.interactive_auth.begin(None, client_seq);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        result = crate::auth::radius::run_radius_login(
                            None,
                            crate::auth::radius::RadiusLoginMethod::DeviceCode,
                        ) => result,
                    }
                };
                let auth = auth_result.map_err(|error| {
                    emit_login_span(
                        false,
                        "radius",
                        None,
                        Some(if cancelled {
                            "login_cancelled"
                        } else {
                            "login_flow_failed"
                        }),
                    );
                    let mut error_response = acp::Error::auth_required();
                    error_response.message = format!("Radius login failed: {error}");
                    error_response
                })?;
                self.models_manager.restamp_platform_credentials();
                // Radius is an independent provider credential; do not replace
                // the primary xAI auth method used by existing sessions.
                emit_login_span(true, "radius", Some(&auth.user_id), None);
                Ok(Default::default())
            }
            auth_method::GITHUB_COPILOT_METHOD_ID => {
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                let client_seq = auth_meta.request_seq;
                let mut cancelled = false;
                let auth_result = if !auth_meta.headless {
                    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
                    let (code_tx, code_rx) = tokio::sync::mpsc::channel(1);
                    let (cancel, _guard) = self.interactive_auth.begin(
                        Some(crate::auth::single_flight::AttemptChannels::new(
                            code_tx, url_rx,
                        )),
                        client_seq,
                    );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::github_copilot::run_github_copilot_login_with_channels(
                            None,
                            Some(crate::auth::AuthChannels {
                                url_tx: Some(url_tx),
                                code_rx,
                            }),
                        ) => r,
                    }
                } else {
                    let (cancel, _guard) = self.interactive_auth.begin(None, client_seq);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::github_copilot::run_github_copilot_login(None) => r,
                    }
                };
                let auth = auth_result.map_err(|e| {
                    emit_login_span(
                        false,
                        "github-copilot",
                        None,
                        Some(if cancelled {
                            "login_cancelled"
                        } else {
                            "login_flow_failed"
                        }),
                    );
                    let mut err = acp::Error::auth_required();
                    err.message = e.to_string();
                    err
                })?;
                self.models_manager.restamp_platform_credentials();
                emit_login_span(true, "github-copilot", None, None);
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: "github-copilot".to_string(),
                    user_id: None,
                });
                let _ = auth;
                Ok(Default::default())
            }
            auth_method::GROK_COM_METHOD_ID | auth_method::OIDC_METHOD_ID => {
                let grok_ctx = self.auth_manager.grok_com_config();
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                tracing::info!(
                    method = arguments.method_id.0.as_ref(),
                    headless = auth_meta.headless,
                    reauth = auth_meta.reauth,
                    use_oauth = auth_meta.use_oauth,
                    "auth: inline auth flow",
                );
                xai_grok_telemetry::unified_log::info(
                    "auth: inline auth flow",
                    None,
                    Some(
                        serde_json::json!({
                        "method": arguments.method_id.0.as_ref(),
                        "headless": auth_meta.headless,
                        "reauth": auth_meta.reauth,
                        "use_oauth": auth_meta.use_oauth,
                    }),
                    ),
                );
                if auth_meta.reauth {
                    let _ = self.auth_manager.clear();
                }
                let cli_oauth = auth_meta.use_oauth.then_some(true);
                let use_oidc = self.cfg.borrow().resolve_grok_oauth(cli_oauth);
                tracing::debug!(resolved = use_oidc.value, source = ?use_oidc.source, "auth: method resolved");
                xai_grok_telemetry::unified_log::debug(
                    "auth: method resolved",
                    None,
                    Some(
                        serde_json::json!({
                        "use_oidc": use_oidc.value,
                        "source": format!("{:?}", use_oidc.source),
                    }),
                    ),
                );
                let login_override = auth_meta.login_override();
                let mut cancelled = false;
                let client_seq = auth_meta.request_seq;
                let auth_result = if !auth_meta.headless {
                    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
                    let (code_tx, code_rx) = tokio::sync::mpsc::channel(1);
                    let (cancel, _guard) = self
                        .interactive_auth
                        .begin(
                            Some(
                                crate::auth::single_flight::AttemptChannels::new(
                                    code_tx,
                                    url_rx,
                                ),
                            ),
                            client_seq,
                        );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::run_auth_flow_with_stderr_bridge(
                            &self.auth_manager,
                            grok_ctx,
                            crate::auth::AuthChannels {
                                url_tx: Some(url_tx),
                                code_rx,
                            },
                            auth_meta.reauth,
                            auth_meta.force_interactive,
                            login_override,
                        ) => r,
                    }
                } else {
                    let (cancel, _guard) = self.interactive_auth.begin(None, client_seq);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::run_auth_flow(
                            &self.auth_manager,
                            grok_ctx,
                            auth_meta.reauth,
                            None,
                            None,
                            None,
                            login_override,
                        ) => r,
                    }
                };
                let (auth, _did_auth) = auth_result
                    .map_err(|e| {
                        emit_login_span(
                            false,
                            arguments.method_id.0.as_ref(),
                            None,
                            Some(
                                if cancelled {
                                    "login_cancelled"
                                } else {
                                    "login_flow_failed"
                                },
                            ),
                        );
                        let mut err = acp::Error::auth_required();
                        err.message = e.to_string();
                        err
                    })?;
                {
                    let mut sampling_config = self.sampling_config.borrow_mut();
                    sampling_config.api_key = Some(auth.key.clone());
                    tracing::debug!("auth: grok.com/oidc handler set api_key (SessionToken)");
                    xai_grok_telemetry::unified_log::debug(
                        "auth: grok.com/oidc handler set api_key (SessionToken)",
                        None,
                        None,
                    );
                }
                self.auth_manager.hot_swap(auth.clone());
                self.enforce_grok_code_access(&auth).await;
                self.maybe_sync_bundle_in_background(false);
                tokio::task::spawn_local(
                    crate::managed_config::post_login_sync(Some(auth.clone())),
                );
                self.set_auth_method(arguments.method_id.clone());
                self.models_manager.on_auth_changed().await;
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                emit_login_span(
                    true,
                    arguments.method_id.0.as_ref(),
                    Some(auth.user_id.as_str()),
                    None,
                );
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: arguments.method_id.0.as_ref().to_string(),
                    user_id: Some(auth.user_id.clone()),
                });
                self.spawn_post_auth_settings(auth);
                Ok(self.auth_response_with_meta())
            }
            _ => {
                Err(
                    acp::Error::invalid_params()
                        .data(
                            format!(
                "unsupported auth method: {}",
                arguments.method_id.0
            ),
                        ),
                )
            }
        }
    }
    async fn new_session(
        &self,
        arguments: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        reject_chat_kind_without_feature(arguments.meta.as_ref())?;
        tracing::debug!(config = ?self.sampling_config, "Received new session request {arguments:?}");
        let init = self
            .initialize_request
            .get()
            .ok_or_else(|| {
                acp::Error::invalid_params()
                    .data("initialize must be called before new_session")
            })?;
        self.seed_client_config_auth_if_available();
        self.spawn_settings_reapply();
        let cwd = AbsPathBuf::new(arguments.cwd.clone())
            .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        folder_trust::resolve_and_record(cwd.as_path(), remote_settings.as_ref(), false);
        let initial_client_mcp_servers = arguments.mcp_servers.clone();
        let (mcp_servers, managed_mcp_expires_at) = self
            .resolve_mcp_servers(arguments.mcp_servers, cwd.as_path())
            .await;
        let mcp_meta_config_map = parse_mcp_meta_config(arguments.meta.as_ref());
        let client_session_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("sessionId"))
            .and_then(|v| v.as_str());
        let custom_model_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("modelId").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty());
        let reload_model_config = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get(crate::agent::config::RELOAD_MODEL_CONFIG_META_KEY))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if reload_model_config && custom_model_id.is_some() {
            crate::extensions::session_admin::reload_models_from_disk(self)?;
        }
        #[allow(unused_variables)]
        let session_computer_sessions = resolve_session_computer_sessions(
            arguments.meta.as_ref(),
        )?;
        let is_chat_kind = is_chat_session_kind(arguments.meta.as_ref());
        let session_yolo_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("yoloMode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(self.default_yolo_mode);
        let session_auto_mode = resolve_session_auto_mode(
            arguments.meta.as_ref(),
            self.default_auto_mode,
            session_yolo_mode,
        );
        let session_id = match client_session_id {
            Some(s) => {
                uuid::Uuid::try_parse(s)
                    .map_err(|e| {
                        acp::Error::invalid_params()
                            .data(
                                format!(
                        "Invalid UUID format for _meta.sessionId '{}': {}",
                        s, e
                    ),
                            )
                    })?;
                acp::SessionId::new(s.to_string())
            }
            None => acp::SessionId::new(uuid::Uuid::now_v7().to_string()),
        };
        let mut session_timer = crate::instrumentation_timer!("session.new_session");
        session_timer.with_field("session_id", session_id.0.as_ref());
        session_timer.with_field("cwd", cwd.as_str());
        let client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                self
                    .initialize_request
                    .get()
                    .and_then(|req| req.meta.as_ref())
                    .and_then(|m| m.get("clientIdentifier"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
        xai_grok_telemetry::session_ctx::log_session_event(crate::agent::session_metrics::SessionStarted {
            session_id: session_id.0.to_string(),
        });
        let session_info = SessionInfo {
            id: session_id.clone(),
            cwd: cwd.as_str().to_owned(),
        };
        let mut model_agent_type: Option<String> = None;
        let mut session_sampling_override: Option<SamplingConfig> = None;
        let mut disallowed_custom: Option<String> = None;
        let session_initial_model = chat_initial_model(is_chat_kind, custom_model_id);
        let build_custom_model_id = if is_chat_kind { None } else { custom_model_id };
        let campaign_nudge = if is_chat_kind {
            None
        } else {
            crate::util::config::campaign_driven_models_default()
                    .filter(|c| {
                        build_custom_model_id.is_none()
                            || build_custom_model_id == c.pre_campaign.as_deref()
                            || build_custom_model_id == Some(c.value.as_str())
                    })
        };
        let campaign_nudged = campaign_nudge.is_some();
        if let Some(c) = &campaign_nudge {
            tracing::info!(
                model = %c.value,
                requested = ?custom_model_id,
                "new_session: applying campaign-driven default model"
            );
        }
        let build_custom_model_id: Option<String> = campaign_nudge
            .map(|c| c.value)
            .or_else(|| build_custom_model_id.map(str::to_owned));
        let resolved_custom_model = build_custom_model_id
            .as_deref()
            .and_then(|custom_model| match self
                .resolve_model_id(&acp::ModelId::new(custom_model))
            {
                Ok(model) if model.info.user_selectable => {
                    model_agent_type = Some(model.info().agent_type.clone());
                    let origin_client = self
                        .origin_client_info_from_meta(arguments.meta.as_ref());
                    session_sampling_override = Some(
                        self.prepare_sampling_config_for_model(&model, origin_client),
                    );
                    Some(custom_model)
                }
                Ok(_) => {
                    tracing::warn!(
                        requested_model = custom_model,
                        "Requested model not allowed by allowed_models; falling back to current default model"
                    );
                    if !campaign_nudged {
                        disallowed_custom = Some(custom_model.to_string());
                    }
                    None
                }
                Err(_) => {
                    tracing::warn!(
                        requested_model = custom_model,
                        fallback_model = %self.models_manager.current_model_id().0,
                        "Requested model not found, falling back to current default model"
                    );
                    None
                }
            });
        if model_agent_type.is_none() && custom_model_id.is_none()
            && let Ok(default_model) = self
                .resolve_model_id(&self.models_manager.current_model_id())
        {
            model_agent_type = Some(default_model.info().agent_type.clone());
        } else if model_agent_type.is_none() && custom_model_id.is_some() {
            tracing::debug!(
                custom_model = ?custom_model_id,
                current_model_id = %self.models_manager.current_model_id().0,
                "Skipping current_model_id agent_type fallback: custom model was requested, \
                 avoiding cross-client agent_type contamination in leader mode"
            );
        }
        let origin_client = self.origin_client_info_from_meta(arguments.meta.as_ref());
        let mut session_sampling = session_sampling_override
            .unwrap_or_else(|| {
                self
                    .resolve_sampling_config_for_model(
                        &self.models_manager.current_model_id(),
                        origin_client.clone(),
                    )
            });
        if let Some(effort) = self.models_manager.current_reasoning_effort()
            && self
                .models_manager
                .model_supports_reasoning_effort(&session_sampling.model)
        {
            session_sampling.reasoning_effort = Some(effort);
        }
        let (summary_client, summary_model) = self
            .build_summary_client(&session_sampling)?;
        let relay_sync = if let Some(sync) = self
            .create_relay_sync(&session_id.0, &session_info)
        {
            Self::spawn_relay_state_forwarder(
                sync.subscribe_state(),
                sync.session_id().to_owned(),
                self.gateway.clone(),
            );
            Some(sync)
        } else {
            None
        };
        let model_id = match &session_initial_model {
            Some(chat_model) => acp::ModelId::new(chat_model.clone()),
            None => {
                resolved_custom_model
                    .map(acp::ModelId::new)
                    .unwrap_or_else(|| self.models_manager.current_model_id())
            }
        };
        let session_model_id = model_id.clone();
        let persistence = if is_chat_kind {
            crate::session::persistence::PersistenceHandle::noop()
        } else {
            let _timer = crate::instrumentation_timer!("session.persistence_init");
            let registry_title_sync = self
                .session_registry_client()
                .map(|client| crate::session::persistence::RegistryGeneratedTitleSync {
                    client,
                    suppress_for_zdr: self
                        .auth_manager
                        .current_or_expired()
                        .is_some_and(|a| a.is_zdr_team()),
                });
            crate::session::persistence::new(
                    &session_info,
                    model_id,
                    summary_client,
                    self.storage_mode.get(),
                    Some(self.auth_manager.clone()),
                    relay_sync,
                    Some(self.gateway.clone()),
                    summary_model,
                    registry_title_sync,
                )
                .await
                .map_err(|e| crate::session::persistence::io_error_to_acp(&e))?
        };
        self.set_turn_number(&session_id, 0u64);
        let chat_history = vec![];
        let client_code_nav_enabled = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("codeNavEnabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| self.code_nav_enabled.get());
        let (client_terminal, client_fs_read, client_fs_write) = Self::resolve_client_io_caps(
            arguments.meta.as_ref(),
            init,
        );
        let spawn_res = {
            let mut timer = crate::instrumentation_timer!("session.spawn_session_actor");
            timer.with_field("session_id", session_id.0.as_ref());
            let spawn_opts = if is_chat_kind {
                chat_session_spawn_options(
                    session_info.clone(),
                    cwd.clone(),
                    arguments.meta.as_ref(),
                    model_agent_type.as_deref(),
                    session_model_id,
                    session_yolo_mode,
                )
            } else {
                SessionSpawnOptions {
                        session_info: session_info.clone(),
                        cwd: cwd.clone(),
                        mcp_servers,
                        initial_client_mcp_servers,
                        mcp_meta_config_map,
                        persistence,
                        chat_history,
                        rewind_points_file_path: None,
                        initial_total_tokens: 0,
                        origin_client: origin_client.clone(),
                        client_code_nav_enabled,
                        client_terminal,
                        client_fs_read,
                        client_fs_write,
                        preloaded_envrc: None,
                        persisted_signals: None,
                        persisted_plan_mode: None,
                        persisted_goal_mode: None,
                        persisted_workflow_runs: Vec::new(),
                        persisted_announcement_state: None,
                        session_meta: arguments.meta.as_ref(),
                        managed_mcp_expires_at,
                        model_agent_type: model_agent_type.as_deref(),
                        session_model_id,
                        session_yolo_mode,
                        session_auto_mode: session_auto_mode && !session_yolo_mode,
                        prompt_display_cwd: None,
                        is_chat_kind: false,
                    }
            };
            self.spawn_and_register_session(init, spawn_opts).await
        };
        spawn_res?;
        tracing::debug!(session_id = %session_id.0, "new_session: spawn_session_actor");
        self.maybe_spawn_interactive_trust_prompt(
            &session_id,
            cwd.as_path(),
            remote_settings.as_ref(),
        );
        let bridge_attach = BridgeAttach::NotAttached;
        let product_analytics = self.product_analytics_enabled();
        if product_analytics || xai_grok_telemetry::external::is_active() {
            let sid = session_id.0.to_string();
            let ci = client_identifier.clone();
            let cv = self.client_version();
            let cwd_str = cwd.as_str().to_owned();
            let perm = if session_yolo_mode {
                xai_grok_telemetry::enums::PermissionMode::AlwaysApprove
            } else if session_auto_mode
                && crate::util::config::auto_permission_mode_enabled_from_disk()
            {
                xai_grok_telemetry::enums::PermissionMode::Auto
            } else {
                xai_grok_telemetry::enums::PermissionMode::Ask
            };
            tokio::spawn(async move {
                let git = xai_grok_telemetry::context::collect_git_context(&cwd_str);
                let ev = xai_grok_telemetry::events::SessionNew {
                    session_id: sid,
                    client_identifier: ci,
                    client_version: cv,
                    is_git_repo: git.is_git_repo,
                    permission_mode: perm,
                };
                xai_grok_telemetry::session_ctx::log_event_dual(product_analytics, ev);
            });
        }
        if let Some(model_id) = resolved_custom_model {
            let _ = crate::timed!(log: "new_session: set_session_model", {
                crate::agent::handlers::model_switch::apply(
                    self,
                    acp::SetSessionModelRequest::new(session_id.clone(), acp::ModelId::new(model_id)),
                )
                .await
            });
            tracing::debug!(session_id = %session_id.0, "new_session: set_session_model");
        }
        if let Some(requested) = disallowed_custom {
            let current = self.models_manager.current_model_id();
            let reason = format!(
                "\"{requested}\" isn't allowed by your allowed_models setting, so this session is using \"{}\".",
                current.0
            );
            self.send_model_auto_switched(
                    &session_id,
                    &acp::ModelId::new(requested),
                    &current,
                    &reason,
                )
                .await;
        }
        let indexed_roots = self.indexed_roots_for(cwd.as_path());
        let (git_root, is_git_repo, discovery_failed) = match xai_grok_workspace::session::git::discover_git_root(
            cwd.as_path(),
        ) {
            GitDiscoveryResult::Found(root) => {
                let root_str = root.to_string_lossy().trim_end_matches('/').to_string();
                (Some(root_str), true, false)
            }
            GitDiscoveryResult::NotARepo => {
                tracing::debug!("new_session: not a git repository");
                (None, false, false)
            }
            GitDiscoveryResult::DiscoveryFailed(e) => {
                tracing::warn!(
                        error = %e,
                        cwd = %cwd.as_str(),
                        "new_session: git repo discovery failed unexpectedly"
                    );
                (None, false, true)
            }
        };
        let (show_non_git_warning, feedback_enabled) = {
            let cfg = self.cfg.borrow();
            let show_non_git_warning = !is_git_repo && !discovery_failed
                && cfg
                    .remote_settings
                    .as_ref()
                    .and_then(|s| s.non_git_warning)
                    .unwrap_or(cfg.features.non_git_warning);
            let feedback_enabled = cfg.is_feedback_enabled();
            (show_non_git_warning, feedback_enabled)
        };
        xai_grok_telemetry::unified_log::info(
            "session created",
            Some(session_id.0.as_ref()),
            Some(serde_json::json!({"cwd": cwd.as_str()})),
        );
        let models = if is_chat_kind {
            chat_new_session_model_state(
                self.chat_modes.model_state().await,
                session_initial_model
                    .filter(|_| matches!(bridge_attach, BridgeAttach::Spawned)),
            )
        } else {
            self.model_state(Some(&session_id))
        };
        let applied_tool_overrides = match self
            .session_handle_waiting_for_load(&session_id)
            .await
        {
            Some(handle) => read_applied_tool_overrides(&handle.cmd_tx).await,
            None => {
                tracing::warn!(
                    session_id = %session_id.0,
                    "session/new toolOverrides echo: session handle not found"
                );
                None
            }
        };
        let mut meta = serde_json::json!({
            "currentWorkingDirectory": cwd.as_str().to_owned(),
            "codebaseIndexed": indexed_roots,
            "isGitRepo": is_git_repo,
            "gitRoot": git_root,
            "showNonGitWarning": show_non_git_warning,
            "feedbackEnabled": feedback_enabled,
        });
        if let Some(obj) = meta.as_object_mut() {
            self.insert_session_config_meta(
                obj,
                &session_id,
                cwd.as_str().to_owned(),
                None,
                &models,
            );
            insert_applied_tool_overrides(obj, applied_tool_overrides.as_ref());
        }
        Ok(
            acp::NewSessionResponse::new(session_id)
                .models(Some(models))
                .meta(meta.as_object().cloned()),
        )
    }
    async fn load_session(
        &self,
        arguments: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let _load_guard = self.begin_session_load(&arguments.session_id);
        reject_chat_kind_without_feature(arguments.meta.as_ref())?;
        self.sweep_dead_sessions();
        self.drain_old_session_thread(&arguments.session_id).await;
        tracing::debug!("Received load session request {arguments:?}");
        let init = self
            .initialize_request
            .get()
            .ok_or_else(|| {
                acp::Error::invalid_params()
                    .data("initialize must be called before load_session")
            })?;
        self.seed_client_config_auth_if_available();
        let persist_data = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/persist"))
            .cloned();
        let target_client_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/leaderClientId"))
            .cloned();
        let acp::LoadSessionRequest {
            session_id,
            cwd,
            mcp_servers: client_mcp_servers,
            meta: request_meta,
            ..
        } = arguments;
        let cwd = AbsPathBuf::new(cwd)
            .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        folder_trust::resolve_and_record(cwd.as_path(), remote_settings.as_ref(), false);
        let initial_client_mcp_servers = client_mcp_servers.clone();
        let (mcp_servers, managed_mcp_expires_at) = self
            .resolve_mcp_servers(client_mcp_servers, cwd.as_path())
            .await;
        let mcp_meta_config_map = parse_mcp_meta_config(request_meta.as_ref());
        let mut load_timer = crate::instrumentation_timer!("session.load_session");
        load_timer.with_field("session_id", session_id.0.as_ref());
        load_timer.with_field("cwd", cwd.as_str());
        let git_root = xai_grok_workspace::session::git::find_git_root_from_path(
                cwd.as_path(),
            )
            .ok();
        if let Some(root) = git_root {
            tokio::task::spawn_blocking(move || {
                crate::session::worktree_pool::cleanup_stale_pool_worktrees(Some(&root));
            });
        }
        xai_grok_telemetry::session_ctx::log_session_event(crate::agent::session_metrics::SessionStarted {
            session_id: session_id.0.to_string(),
        });
        let session_info = SessionInfo {
            id: session_id.clone(),
            cwd: cwd.as_str().to_owned(),
        };
        let current_session_dir = crate::session::persistence::session_dir(
            &session_info,
        );
        tokio::task::spawn_blocking(move || {
            crate::session::persistence::cleanup_stale_sessions(
                Some(&current_session_dir),
            );
        });
        let session_exists = self.sessions.borrow().contains_key(&session_id);
        if session_exists {
            tracing::info!(
                session_id = %session_id.0,
                "Reconnect detected: flushing persistence buffer before replay"
            );
            if let Some(handle) = self.sessions.borrow().get(&session_id) {
                handle
                    .gateway_enabled
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
            let mut flush_timer = crate::instrumentation_timer!("session.reconnect_flush");
            flush_timer.with_field("session_id", session_id.0.as_ref());
            if let Err(reason) = self.flush_session(&session_id).await {
                tracing::warn!(
                    session_id = %session_id.0,
                    reason,
                    "Reconnect flush failed"
                );
            }
            drop(flush_timer);
        }
        let origin_client = self.origin_client_info_from_meta(request_meta.as_ref());
        let load_session_sampling = self
            .resolve_sampling_config_for_model(
                &self.models_manager.current_model_id(),
                origin_client.clone(),
            );
        let (summary_client, summary_model) = self
            .build_summary_client(&load_session_sampling)?;
        let relay_sync = if let Some(sync) = self
            .create_relay_sync(&session_id.0, &session_info)
        {
            Self::spawn_relay_state_forwarder(
                sync.subscribe_state(),
                sync.session_id().to_owned(),
                self.gateway.clone(),
            );
            Some(sync)
        } else {
            None
        };
        let mut persistence_timer = crate::instrumentation_timer!("session.load_light");
        persistence_timer.with_field("session_id", session_id.0.as_ref());
        let backend = if self.build_registry_config().is_some() {
            Some(
                crate::remote::BackendClient::new()
                    .with_auth_manager(self.auth_manager.clone()),
            )
        } else {
            None
        };
        let registry_title_sync = self
            .session_registry_client()
            .map(|client| crate::session::persistence::RegistryGeneratedTitleSync {
                client,
                suppress_for_zdr: self
                    .auth_manager
                    .current_or_expired()
                    .is_some_and(|a| a.is_zdr_team()),
            });
        let (persistence_info, persistence) = crate::session::persistence::load_light(
                &session_info,
                summary_client,
                self.storage_mode.get(),
                Some(self.auth_manager.clone()),
                backend.as_ref(),
                relay_sync,
                Some(self.gateway.clone()),
                summary_model,
                registry_title_sync,
            )
            .await
            .map_err(|e| crate::session::persistence::io_error_to_acp(&e))?;
        drop(persistence_timer);
        let crate::session::persistence::PersistedInfoLight {
            summary,
            chat_history,
            plan_state: _,
            plan_mode_state: persisted_plan_mode,
            updates_file_path,
            rewind_points_file_path,
            signals: persisted_signals,
            announcement_state: persisted_announcement_state,
            goal_mode_state: _persisted_goal_mode,
            workflow_runs: persisted_workflow_runs,
        } = persistence_info;
        let restored_compaction_count = persisted_signals
            .as_ref()
            .map(|s| s.compaction_count as u64)
            .unwrap_or(0);
        let restored_turn_count = persisted_signals
            .as_ref()
            .map(|s| s.turn_count as u64)
            .unwrap_or(0);
        let restored_tool_call_count = persisted_signals
            .as_ref()
            .map(|s| s.tool_call_count as u64)
            .unwrap_or(0);
        let restored_plan_mode_state = match &persisted_plan_mode {
            Some(s) => {
                match s.state {
                    crate::session::plan_mode::PlanModeState::Inactive => {
                        xai_grok_telemetry::events::PlanModeState::Inactive
                    }
                    crate::session::plan_mode::PlanModeState::Pending => {
                        xai_grok_telemetry::events::PlanModeState::Pending
                    }
                    crate::session::plan_mode::PlanModeState::Active
                    | crate::session::plan_mode::PlanModeState::ExitPending => {
                        xai_grok_telemetry::events::PlanModeState::Active
                    }
                }
            }
            None => xai_grok_telemetry::events::PlanModeState::Inactive,
        };
        let restored_awaiting_plan_approval = persisted_plan_mode
            .as_ref()
            .is_some_and(|s| s.awaiting_plan_approval);
        self.set_turn_number(&session_id, summary.next_trace_turn);
        tracing::info!(
            session_id = %session_id.0,
            next_trace_turn = summary.next_trace_turn,
            "Loaded session telemetry turn counter from persistence"
        );
        let no_replay = parse_no_replay(request_meta.as_ref());
        let cursor = request_meta
            .as_ref()
            .and_then(|m| m.get("cursor"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let session_yolo_mode = request_meta
            .as_ref()
            .and_then(|m| m.get("yoloMode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(self.default_yolo_mode);
        let session_auto_mode = resolve_session_auto_mode(
            request_meta.as_ref(),
            self.default_auto_mode,
            session_yolo_mode,
        );
        #[allow(unused_variables)]
        let session_computer_sessions = resolve_session_computer_sessions(
            request_meta.as_ref(),
        )?;
        let restore_code_requested = request_meta
            .as_ref()
            .and_then(|m| m.get("x.ai/restore_code"))
            .and_then(|v| v.as_bool())
            .unwrap_or(self.restore_code);
        let registry_client_for_restore = self.session_registry_client();
        if restore_code_requested && registry_client_for_restore.is_none() {
            xai_grok_workspace::session::git::warn_registry_disabled_restore(
                session_id.0.as_ref(),
            );
        }
        let restore_checkout_allowed = xai_grok_workspace::session::git::restore_code_checkout_allowed(
            cwd.as_path(),
            Some(summary.info.cwd.as_str()),
        );
        if restore_code_requested && !restore_checkout_allowed
            && let Some(ref target_sha) = summary.head_commit
        {
            tracing::warn!(
                target: xai_grok_workspace::session::git::RESTORE_CODE_LOG,
                session_id = %session_id.0,
                supplied_cwd = %cwd.as_str(),
                persisted_cwd = %summary.info.cwd,
                target_sha = %target_sha,
                "restore_code: skipping session HEAD checkout — supplied cwd is neither a grok worktree nor the session's persisted cwd (refusing to detach the source repo)"
            );
            xai_grok_telemetry::unified_log::warn(
                "restore_code: skipped session HEAD checkout (unsafe cwd)",
                Some(session_id.0.as_ref()),
                Some(
                    serde_json::json!({
                    "supplied_cwd": cwd.as_str(),
                    "persisted_cwd": summary.info.cwd,
                    "target_sha": target_sha,
                }),
                ),
            );
        }
        let mut code_restore_info: Option<serde_json::Value> = None;
        if restore_code_requested && restore_checkout_allowed
            && let Some(ref target_sha) = summary.head_commit
        {
            use xai_grok_workspace::session::git::RestoreKind;
            let outcome = xai_grok_workspace::session::git::checkout_session_commit(
                    cwd.as_path(),
                    target_sha,
                    true,
                    session_id.0.as_ref(),
                )
                .await;
            let kind = if !outcome.checked_out {
                RestoreKind::CheckoutFailed
            } else {
                match registry_client_for_restore {
                        None => RestoreKind::RegistryOff,
                        Some(registry_client) => {
                            let _ = registry_client;
                            RestoreKind::RegistryOff
                        }
                    }
            };
            code_restore_info = crate::agent::restore_code::build_code_restore_meta(
                target_sha,
                &outcome,
                kind,
            );
        }
        let load_envrc = {
            let skip_envrc = request_meta
                .as_ref()
                .and_then(|m| m.get("x.ai/skip_envrc"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if skip_envrc {
                false
            } else {
                self.cfg.borrow().session.load_envrc.unwrap_or(true)
            }
        };
        let (initial_total_tokens, delta_completions, unfinished_subagents) = if no_replay {
            tracing::info!(
                session_id = %session_id.0,
                "Skipping session replay (noReplay flag set by relay)"
            );
            (
                Self::extract_initial_tokens_from_updates(&updates_file_path),
                Vec::new(),
                Vec::new(),
            )
        } else {
            let (tokens, replay_end_offset, unfinished_subagents) = self
                .replay_session_updates(
                    &session_id,
                    &cwd,
                    &updates_file_path,
                    persist_data.as_ref(),
                    target_client_id.as_ref(),
                    cursor.as_deref(),
                )
                .await?;
            let cursor_mark_replay = cursor.is_none();
            let _timer = crate::instrumentation_timer!("session.delta_flush_replay");
            let completions = match self.flush_session(&session_id).await {
                Ok(()) => {
                    self.replay_session_updates_from_offset_enqueue(
                        &session_id,
                        &updates_file_path,
                        replay_end_offset,
                        persist_data.as_ref(),
                        target_client_id.as_ref(),
                        cursor_mark_replay,
                    )
                }
                Err(reason) => {
                    tracing::warn!(
                        session_id = %session_id.0,
                        reason,
                        "Post-replay flush failed, skipping delta replay"
                    );
                    Vec::new()
                }
            };
            (tokens, completions, unfinished_subagents)
        };
        if let Some(handle) = self.sessions.borrow().get(&session_id) {
            handle.gateway_enabled.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        for rx in delta_completions {
            let _ = rx.await;
        }
        let reconcile_completions = {
            let _timer = crate::instrumentation_timer!("session.reconcile_stale_tasks");
            self.reconcile_stale_background_tasks(&session_id, &updates_file_path)
        };
        for rx in reconcile_completions {
            let _ = rx.await;
        }
        let preloaded_envrc = xai_grok_workspace::envrc::load_envrc_or_empty_when_trusted(
            cwd.as_path(),
            load_envrc && folder_trust::project_scope_allowed(cwd.as_path()),
        );
        let client_code_nav_enabled = request_meta
            .as_ref()
            .and_then(|m| m.get("codeNavEnabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| self.code_nav_enabled.get());
        let (client_terminal, client_fs_read, client_fs_write) = Self::resolve_client_io_caps(
            request_meta.as_ref(),
            init,
        );
        let prompt_display_cwd = request_meta
            .as_ref()
            .and_then(|m| m.get("x.ai/display_cwd"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| summary.prompt_display_cwd.clone());
        if self.sessions.borrow().get(&session_id).is_none() {
            tracing::info!(
                session_id = %session_id.0,
                "load_session: spawning new session actor (session not in memory)"
            );
            let mut spawn_timer = crate::instrumentation_timer!("session.spawn_and_register_session");
            spawn_timer.with_field("session_id", session_id.0.as_ref());
            let persisted_agent_name: Option<String> = summary
                .agent_name
                .clone()
                .or_else(|| {
                    self
                        .resolve_model_id(&summary.current_model_id)
                        .ok()
                        .map(|m| m.info().agent_type.clone())
                });
            self.spawn_and_register_session(
                    init,
                    SessionSpawnOptions {
                        session_info,
                        cwd: cwd.clone(),
                        mcp_servers,
                        initial_client_mcp_servers,
                        mcp_meta_config_map,
                        persistence,
                        chat_history,
                        rewind_points_file_path,
                        initial_total_tokens,
                        origin_client: origin_client.clone(),
                        client_code_nav_enabled,
                        client_terminal,
                        client_fs_read,
                        client_fs_write,
                        preloaded_envrc: Some(preloaded_envrc),
                        persisted_signals,
                        persisted_plan_mode,
                        persisted_goal_mode: _persisted_goal_mode,
                        persisted_workflow_runs,
                        persisted_announcement_state,
                        session_meta: request_meta.as_ref(),
                        managed_mcp_expires_at,
                        model_agent_type: persisted_agent_name.as_deref(),
                        session_model_id: summary.current_model_id.clone(),
                        session_yolo_mode,
                        session_auto_mode: session_auto_mode && !session_yolo_mode,
                        prompt_display_cwd,
                        is_chat_kind: false,
                    },
                )
                .await?;
            drop(spawn_timer);
        } else if !mcp_servers.is_empty() {
            tracing::info!(
                session_id = %session_id.0,
                mcp_server_count = mcp_servers.len(),
                "load_session: reconnecting to existing session, updating MCP servers"
            );
            if let Some(handle) = self.sessions.borrow_mut().get_mut(&session_id) {
                handle.initial_client_mcp_servers = initial_client_mcp_servers;
                let (tx, _rx) = tokio::sync::oneshot::channel();
                let _ = handle
                    .cmd_tx
                    .send(crate::session::SessionCommand::UpdateMcpServers {
                        mcp_servers,
                        respond_to: tx,
                    });
            }
        } else {
            tracing::info!(
                session_id = %session_id.0,
                "load_session: reconnecting to existing session (feedback manager already initialized)"
            );
        }
        {
            let init_meta = self
                .initialize_request
                .get()
                .and_then(|init| init.meta.as_ref());
            if let Some(handle) = self.sessions.borrow().get(&session_id) {
                enqueue_replace_system_prompt_override(
                    &handle.cmd_tx,
                    request_meta.as_ref(),
                    init_meta,
                );
            }
        }
        if session_exists
            && let Some(hooks) = crate::extensions::hooks::reconnect_client_hooks(
                request_meta.as_ref(),
            ) && let Some(handle) = self.sessions.borrow().get(&session_id)
        {
            handle.set_client_hooks(hooks);
        }
        #[allow(unused_variables)]
        let local_transcript_rendered = !no_replay
            && updates_file_path
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .is_some_and(|m| m.len() > 0);
        if let Some(handle) = self.sessions.borrow_mut().get_mut(&session_id) {
            handle.code_nav_enabled = client_code_nav_enabled;
            if session_yolo_mode && !handle.yolo_mode {
                tracing::debug!(
                    session_id = %session_id.0,
                    "Setting YOLO mode on reconnect from load_session request metadata"
                );
                handle.yolo_mode = true;
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::SetYoloMode {
                        enabled: true,
                    });
            }
            if session_auto_mode && !session_yolo_mode
                && crate::util::config::auto_permission_mode_enabled_from_disk()
            {
                tracing::debug!(
                    session_id = %session_id.0,
                    "Setting auto mode on reconnect from load_session request metadata"
                );
                handle.yolo_mode = false;
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::SetAutoMode {
                        enabled: true,
                    });
            }
        }
        self.maybe_spawn_interactive_trust_prompt(
            &session_id,
            cwd.as_path(),
            remote_settings.as_ref(),
        );
        let orphan_parent = {
            let sessions = self.sessions.borrow();
            sessions
                .get(&session_id)
                .map(|handle| (handle.cmd_tx.clone(), handle.info.cwd.clone()))
        };
        if let Some((parent_cmd_tx, session_cwd)) = orphan_parent {
            let session_dir = crate::session::persistence::session_dir(
                &SessionInfo {
                    id: session_id.clone(),
                    cwd: session_cwd,
                },
            );
            crate::agent::subagent::reconcile_orphaned_subagents_with_backend(
                    &unfinished_subagents,
                    &xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend::new(
                        self.subagent_event_tx.clone(),
                    ),
                    &session_dir,
                    session_id.0.as_ref(),
                    &self.gateway,
                    Some(&parent_cmd_tx),
                )
                .await;
        }
        let persisted_model = summary.current_model_id.clone();
        let models = self.models_manager.models();
        let available = self.models_manager.available();
        self.model_unavailable_sessions.borrow_mut().remove(session_id.0.as_ref());
        let resolved_catalog_key = resolve_catalog_key(&models, &persisted_model);
        tracing::debug!(
            session_id = %session_id.0,
            persisted = %persisted_model.0,
            resolved_catalog_key = ?resolved_catalog_key.as_ref().map(|k| k.0.as_ref()),
            available_count = available.len(),
            contains_persisted = available.contains_key(&persisted_model),
            available_keys = ?available.keys().take(10).collect::<Vec<_>>(),
            "load_session: restoring persisted model (debug)"
        );
        let is_grok_build = persisted_model.0.starts_with("grok-build");
        // Locked BYOK rows ride in `available` for discovery but can't sample —
        // fallbacks must skip them or a restored session breaks on first prompt.
        let usable =
            |id: &acp::ModelId| crate::agent::models::usable_in_projection(&available, id);
        let same_family_fallback = if is_grok_build {
            available
                .keys()
                .find(|id| usable(id) && id.0.starts_with("grok-build"))
                .cloned()
        } else {
            available
                .keys()
                .find(|id| usable(id) && !id.0.starts_with("grok-build"))
                .cloned()
        };
        let selectable_catalog_key = selectable_catalog_key_for_persisted(
            &models,
            &available,
            &persisted_model,
        );
        let model_id = if let Some(catalog_key) = selectable_catalog_key {
            if catalog_key != persisted_model {
                tracing::info!(
                    session_id = %session_id.0,
                    persisted = %persisted_model.0,
                    catalog_key = %catalog_key.0,
                    "load_session: mapped persisted routing slug to catalog key"
                );
                xai_grok_telemetry::unified_log::info(
                    "load_session: mapped persisted routing slug to catalog key",
                    Some(session_id.0.as_ref()),
                    Some(
                        serde_json::json!({
                        "persisted_model": persisted_model.0.as_ref(),
                        "catalog_key": catalog_key.0.as_ref(),
                    }),
                    ),
                );
            }
            catalog_key
        } else if available.is_empty() {
            tracing::warn!(
                session_id = %session_id.0,
                persisted = %persisted_model.0,
                "load_session: model catalog empty at load; keeping persisted model unverified (catalog fetch may still be in flight)"
            );
            xai_grok_telemetry::unified_log::warn(
                "load_session: model catalog empty, keeping persisted model unverified",
                Some(session_id.0.as_ref()),
                Some(
                    serde_json::json!({
                    "persisted_model": persisted_model.0.as_ref(),
                }),
                ),
            );
            persisted_model
        } else if let Some(fallback) = same_family_fallback {
            tracing::warn!(
                session_id = %session_id.0,
                previous = %persisted_model.0,
                new = %fallback.0,
                "Persisted model no longer available, auto-switching within family"
            );
            let reason = format!(
                "Model \"{}\" is no longer available for your account.",
                persisted_model.0,
            );
            self.send_model_auto_switched(
                    &session_id,
                    &persisted_model,
                    &fallback,
                    &reason,
                )
                .await;
            fallback
        } else {
            let fallback = available
                .keys()
                .find(|id| usable(id))
                .cloned()
                .unwrap_or_else(|| persisted_model.clone());
            tracing::warn!(
                session_id = %session_id.0,
                previous = %persisted_model.0,
                fallback = %fallback.0,
                available_count = available.len(),
                available_keys = ?available.keys().take(10).collect::<Vec<_>>(),
                "Persisted model no longer available, no same-family fallback — blocking prompts for this session"
            );
            xai_grok_telemetry::unified_log::warn(
                "load_session: persisted model unavailable, no same-family fallback",
                Some(session_id.0.as_ref()),
                Some(
                    serde_json::json!({
                    "persisted_model": persisted_model.0.as_ref(),
                    "fallback_model": fallback.0.as_ref(),
                    "available_count": available.len(),
                }),
                ),
            );
            let reason = format!(
                "Model \"{}\" is no longer available. Please start a new session.",
                persisted_model.0,
            );
            let empty_id = acp::ModelId::new(String::new());
            self.send_model_auto_switched(
                    &session_id,
                    &persisted_model,
                    &empty_id,
                    &reason,
                )
                .await;
            self.model_unavailable_sessions
                .borrow_mut()
                .insert(session_id.0.to_string(), persisted_model.clone());
            fallback
        };
        tracing::debug!(
            session_id = %session_id.0,
            final_model_id = %model_id.0,
            "load_session: resolved final model_id for set_session_model"
        );
        {
            let _timer = crate::instrumentation_timer!("session.restore_model");
            let restore_meta = summary
                .reasoning_effort
                .map(|effort| {
                    let mut map = acp::Meta::new();
                    map.insert(
                        REASONING_EFFORT_META_KEY.to_string(),
                        reasoning_effort_meta_value(effort),
                    );
                    map
                });
            let _ = crate::agent::handlers::model_switch::apply(
                    self,
                    acp::SetSessionModelRequest::new(session_id.to_owned(), model_id)
                        .meta(restore_meta),
                )
                .await;
        }
        let mut response_meta_map = serde_json::Map::new();
        response_meta_map.insert("sessionId".to_string(), serde_json::json!(session_id));
        if let Some(persist) = persist_data {
            response_meta_map.insert("x.ai/persist".to_string(), persist);
        }
        let session_cwd = self
            .sessions
            .borrow()
            .get(&session_id)
            .map(|h| h.info.cwd.clone());
        let indexed_roots = session_cwd
            .as_deref()
            .map(|c| self.indexed_roots_for(std::path::Path::new(c)))
            .unwrap_or_default();
        response_meta_map
            .insert("codebaseIndexed".to_string(), serde_json::json!(indexed_roots));
        if summary.head_commit.is_some() && let Some(ref cwd) = session_cwd
            && summary
                .git_root_dir
                .as_deref()
                .is_none_or(|root| {
                    xai_grok_workspace::session::git::find_git_root_from_path(
                            std::path::Path::new(cwd.as_str()),
                        )
                        .ok()
                        .is_some_and(|current_root| {
                            current_root == std::path::Path::new(root)
                        })
                })
        {
            let _timer = crate::instrumentation_timer!("session.git_divergence");
            let cwd_path = std::path::Path::new(cwd.as_str());
            let current_head = xai_grok_workspace::session::git::git_cli(
                    cwd_path,
                    &["rev-parse", "HEAD"],
                )
                .await
                .ok();
            if let Some(divergence) = xai_grok_workspace::session::git::detect_head_divergence(
                summary.head_commit.as_deref(),
                summary.head_branch.as_deref(),
                current_head.as_deref(),
            ) {
                response_meta_map
                    .insert("gitDivergence".to_string(), serde_json::json!(divergence));
            }
        }
        if let Some(info) = code_restore_info {
            response_meta_map.insert("codeRestore".to_string(), info);
        }
        if let Some(running_prompt_id) = self
            .sessions
            .borrow()
            .get(&session_id)
            .and_then(|h| h.current_prompt_id.lock().ok().and_then(|g| g.clone()))
        {
            response_meta_map
                .insert(
                    "x.ai/runningPromptId".to_string(),
                    serde_json::json!(running_prompt_id),
                );
        }
        let model_state = self.model_state(Some(&session_id));
        self.insert_session_config_meta(
            &mut response_meta_map,
            &session_id,
            session_cwd.clone().unwrap_or_default(),
            summary.display_title_opt(),
            &model_state,
        );
        let applied_tool_overrides = {
            let cmd_tx = self
                .sessions
                .borrow()
                .get(&session_id)
                .map(|handle| handle.cmd_tx.clone());
            match cmd_tx {
                Some(cmd_tx) => read_applied_tool_overrides(&cmd_tx).await,
                None => {
                    tracing::warn!(
                        session_id = %session_id.0,
                        "session/load toolOverrides echo: session handle not found"
                    );
                    None
                }
            }
        };
        insert_applied_tool_overrides(
            &mut response_meta_map,
            applied_tool_overrides.as_ref(),
        );
        let response_meta = serde_json::Value::Object(response_meta_map);
        xai_grok_telemetry::unified_log::info(
            "session loaded",
            Some(session_id.0.as_ref()),
            None,
        );
        let response = acp::LoadSessionResponse::new()
            .models(Some(model_state))
            .meta(response_meta.as_object().cloned());
        if let Some(handle) = self.sessions.borrow().get(&session_id) {
            let _ = handle.cmd_tx.send(SessionCommand::AdvertiseCommands);
            if restored_awaiting_plan_approval {
                let _ = handle.cmd_tx.send(SessionCommand::RestorePlanApproval);
            }
        }
        if self.product_analytics_enabled() {
            log_event(xai_grok_telemetry::events::SessionLoad {
                session_id: session_id.0.to_string(),
                compaction_count: restored_compaction_count,
                turn_count: restored_turn_count,
                tool_call_count: restored_tool_call_count,
                plan_mode_state: restored_plan_mode_state,
                permission_mode: if session_yolo_mode {
                    xai_grok_telemetry::enums::PermissionMode::AlwaysApprove
                } else if session_auto_mode
                    && crate::util::config::auto_permission_mode_enabled_from_disk()
                {
                    xai_grok_telemetry::enums::PermissionMode::Auto
                } else {
                    xai_grok_telemetry::enums::PermissionMode::Ask
                },
                model_id: summary.current_model_id.0.to_string(),
                restored_from_disk: true,
            });
        }
        Ok(response)
    }
    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        crate::agent::handlers::session::handle_list_sessions(self, args).await
    }
    #[tracing::instrument(
        name = "agent.prompt",
        skip_all,
        fields(session_id = %arguments.session_id.0, turn_number = tracing::field::Empty)
    )]
    #[allow(unused_mut)]
    async fn prompt(
        &self,
        mut arguments: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        use crate::session::plan_mode::PromptMode;
        if let Some(meta) = arguments.meta.as_ref() {
            xai_file_utils::trace_context::link_current_span_to_meta(
                &serde_json::Value::Object(meta.clone()),
            );
        }
        tracing::debug!(
            target: "sampling_log",
            session_id = %arguments.session_id.0,
            "Received prompt request"
        );
        xai_grok_telemetry::unified_log::info(
            "prompt received",
            Some(arguments.session_id.0.as_ref()),
            None,
        );
        let handle = self
            .session_handle_waiting_for_load(&arguments.session_id)
            .await
            .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
        if self.models_manager.allowlist_excludes_all() {
            self.send_model_auto_switched(
                    &arguments.session_id,
                    &acp::ModelId::new(String::new()),
                    &acp::ModelId::new(String::new()),
                    "None of your models are allowed by allowed_models. \
                 Broaden it or remove it from your config, then restart.",
                )
                .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        let latched_model = self
            .model_unavailable_sessions
            .borrow()
            .get(arguments.session_id.0.as_ref())
            .cloned();
        if let Some(unavailable_model) = latched_model {
            let models = self.models_manager.models();
            let available = self.models_manager.available();
            let restore_model_id = selectable_catalog_key_for_persisted(
                    &models,
                    &available,
                    &unavailable_model,
                )
                .unwrap_or(unavailable_model.clone());
            if crate::agent::models::usable_in_projection(&available, &restore_model_id) {
                tracing::info!(
                    session_id = %arguments.session_id.0,
                    model_id = %restore_model_id.0,
                    "prompt: previously-unavailable model is back in the catalog; restoring it and unblocking the session"
                );
                xai_grok_telemetry::unified_log::info(
                    "prompt: previously-unavailable model recovered, unblocking session",
                    Some(arguments.session_id.0.as_ref()),
                    Some(
                        serde_json::json!({
                        "model_id": restore_model_id.0.as_ref(),
                    }),
                    ),
                );
                self.model_unavailable_sessions
                    .borrow_mut()
                    .remove(arguments.session_id.0.as_ref());
                if let Err(e) = crate::agent::handlers::model_switch::apply(
                        self,
                        acp::SetSessionModelRequest::new(
                            arguments.session_id.clone(),
                            restore_model_id.clone(),
                        ),
                    )
                    .await
                {
                    tracing::warn!(
                        session_id = %arguments.session_id.0,
                        model_id = %restore_model_id.0,
                        error = ?e,
                        "prompt: failed to restore previously-unavailable model; continuing with the session's current model"
                    );
                }
            } else {
                tracing::warn!(
                    session_id = %arguments.session_id.0,
                    unavailable_model = %unavailable_model.0,
                    available_count = available.len(),
                    available_keys = ?available.keys().take(10).collect::<Vec<_>>(),
                    "prompt blocked: session model unavailable since load and still missing from the catalog"
                );
                xai_grok_telemetry::unified_log::warn(
                    "prompt blocked: model unavailable",
                    Some(arguments.session_id.0.as_ref()),
                    Some(
                        serde_json::json!({
                        "unavailable_model": unavailable_model.0.as_ref(),
                        "available_count": available.len(),
                    }),
                    ),
                );
                self.send_model_auto_switched(
                        &arguments.session_id,
                        &acp::ModelId::new(String::new()),
                        &acp::ModelId::new(String::new()),
                        "Your previous model is no longer available and could not \
                     be switched to a compatible model. Please start a new session.",
                    )
                    .await;
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }
        }
        let dispatch_lock = self.dispatch_lock(&arguments.session_id);
        let dispatch_guard = dispatch_lock.lock().await;
        let meta_prompt_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("mode"))
            .and_then(|v| v.as_str())
            .map(PromptMode::from_meta_str);
        let prompt_mode = if let Some(mode) = meta_prompt_mode {
            mode
        } else {
            let (mode_tx, mode_rx) = oneshot::channel();
            let _ = handle
                .cmd_tx
                .send(crate::session::SessionCommand::GetCurrentPromptMode {
                    responds_to: mode_tx,
                });
            mode_rx.await.unwrap_or_default()
        };
        let turn_started_at = chrono::Utc::now().to_rfc3339();
        let prompt_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("promptId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let turn_number = self.allocate_turn_number(&arguments.session_id);
        tracing::Span::current().record("turn_number", turn_number);
        tracing::info!("Setting up prompt tracing");
        let trace_context = self.get_trace_context(&handle.info, turn_number).await;
        let (harness_block_for_upload, upload_flush_timeout) = crate::util::config::load_blocking_upload_config_sync();
        let block_for_upload = self.cfg.borrow().mode == config::AgentMode::Headless
            || harness_block_for_upload;
        let (model_tx, model_rx) = oneshot::channel();
        let _ = handle
            .cmd_tx
            .send(crate::session::SessionCommand::GetCurrentModel {
                responds_to: model_tx,
            });
        let model = model_rx
            .await
            .unwrap_or_else(|_| self.sampling_config.borrow().model.clone());
        let mut parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>> = None;
        let verbatim = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("verbatim"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let send_now = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("sendNow"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(ctx) = trace_context.clone() {
            let (tx, parsed_prompt_rx) = oneshot::channel::<ParsedPromptInfo>();
            parsed_prompt_tx = Some(tx);
            let auth = self.auth_manager.current();
            let user_id = auth.as_ref().map(|a| a.user_id.clone());
            let team_id = auth.as_ref().and_then(|a| a.team_id.clone());
            let user_email = auth.and_then(|a| a.email);
            let init_meta = self
                .initialize_request
                .get()
                .and_then(|req| req.meta.as_ref());
            let client_source = init_meta
                .and_then(|meta| {
                    meta
                        .get("clientSource")
                        .or_else(|| meta.get("clientType"))
                        .or_else(|| meta.get("clientIdentifier"))
                })
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let client_version = init_meta
                .and_then(|meta| meta.get("clientVersion"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| self.cfg.borrow().client_version.clone());
            let plugin_registry = self.plugin_registry_snapshot();
            let prompt_images: Vec<agent_client_protocol::ImageContent> = arguments
                .prompt
                .iter()
                .filter_map(|block| {
                    if let agent_client_protocol::ContentBlock::Image(img) = block {
                        Some(img.clone())
                    } else {
                        None
                    }
                })
                .collect();
            let mut prompt_metadata = PromptMetadata {
                schema_version: GCS_SCHEMA_VERSION.to_string(),
                session_id: ctx.session_info.id.0.to_string(),
                turn_number: ctx.turn_number,
                request_id: prompt_id.clone(),
                turn_started_at: turn_started_at.clone(),
                repo_root: None,
                remote_url: None,
                user_id,
                user_email,
                team_id,
                client_source,
                client_version,
                model: model.to_owned(),
                reasoning_effort: ctx
                    .session_handle
                    .reasoning_effort
                    .map(|e| e.as_str().to_string()),
                experiment_id: None,
                host_os: std::env::consts::OS.to_string(),
                host_arch: std::env::consts::ARCH.to_string(),
                prompt_has_image: Some(!prompt_images.is_empty()),
                prompt_was_truncated: Some(false),
                prompt_verbatim: if verbatim { Some(true) } else { None },
                cwd: Some(ctx.session_info.cwd.clone()),
                agent_type: Some(ctx.session_handle.agent_name.clone()),
                shell_version: Some(xai_grok_version::VERSION.to_string()),
                workspace_type: None,
                sandbox: local_sandbox_telemetry(),
            };
            let (session_copy_tx, session_copy_rx) = oneshot::channel();
            let copy_sent = ctx
                .session_handle
                .cmd_tx
                .send(SessionCommand::CopyFile {
                    respond_to: session_copy_tx,
                })
                .is_ok();
            if !copy_sent {
                tracing::warn!(
                    session_id = %ctx.session_info.id.0,
                    turn_number = ctx.turn_number,
                    "Failed to send CopyFile command, skipping session state upload"
                );
            }
            tokio::spawn({
                let ctx = ctx.clone();
                async move {
                    if let Ok(Ok(info)) = tokio::time::timeout(
                            std::time::Duration::from_secs(120),
                            parsed_prompt_rx,
                        )
                        .await && !info.text.is_empty()
                    {
                        prompt_metadata.prompt_was_truncated = Some(
                            info.full_text.is_some(),
                        );
                        if let Some(full_text) = &info.full_text {
                            upload_full_prompt_txt(&ctx, full_text).await;
                        }
                    }
                    upload_metadata(&ctx, prompt_metadata).await;
                }
            });
            spawn_upload_task(
                "before_uploads",
                async move {
                    let before_workspace_fut = async {};
                    futures::join!(
                    upload_session_state(&ctx, "before", session_copy_rx, UploadWait::Confirm),
                    before_workspace_fut,
                    upload_images(&ctx, &prompt_images),
                    upload_plugin_state(&ctx, plugin_registry.as_deref()),
                );
                },
            );
        }
        let next_trace_turn = self
            .session_turn_number(&arguments.session_id)
            .unwrap_or_else(|| turn_number.saturating_add(1));
        let _ = handle
            .cmd_tx
            .send(crate::session::SessionCommand::SetNextTraceTurn {
                next_trace_turn,
                request_id: Some(prompt_id.clone()),
            });
        let (tx, rx) = oneshot::channel();
        let prompt_client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let prompt_screen_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("screenMode"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let json_schema = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("outputSchema"))
            .cloned();
        if json_schema.as_ref().is_some_and(|schema| !schema.is_object()) {
            return Err(
                acp::Error::invalid_params()
                    .data("outputSchema must be a JSON object describing a JSON Schema"),
            );
        }
        let tool_overrides_update = match arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("toolOverrides"))
        {
            None => None,
            Some(value) => {
                match xai_grok_sampling_types::ToolOverridesUpdate::parse(value) {
                    Ok(update) => Some(update),
                    Err(reason) => {
                        return Err(
                            acp::Error::invalid_params()
                                .data(format!("toolOverrides: {reason}")),
                        );
                    }
                }
            }
        };
        handle
            .cmd_tx
            .send(SessionCommand::Prompt {
                prompt_id: prompt_id.clone(),
                prompt_blocks: arguments.prompt.clone(),
                prompt_mode,
                artifact_upload_ctx: trace_context
                    .as_ref()
                    .map(|ctx| ctx.artifact_upload_context()),
                client_identifier: prompt_client_identifier,
                screen_mode: prompt_screen_mode,
                verbatim,
                traceparent: xai_file_utils::trace_context::current_traceparent(),
                json_schema,
                send_now,
                admission: None,
                tool_overrides_update,
                respond_to: tx,
                persist_ack: None,
                parsed_prompt_tx,
            })
            .map_err(|e| {
                acp::Error::internal_error()
                    .data(format!("failed to dispatch prompt to session: {e}"))
            })?;
        drop(dispatch_guard);
        self.push_roster_activity_delta(
            &arguments.session_id,
            crate::agent::roster::RosterActivity::Working,
        );
        let stop_result = rx
            .await
            .map_err(|_| {
                acp::Error::internal_error().data("session failed to respond")
            })?;
        let last_turn_usage_for_meta = handle
            .chat_state_handle
            .get_last_turn_usage()
            .await;
        let applied_tool_overrides = stop_result
            .as_ref()
            .ok()
            .and_then(|ok| ok.tool_overrides.clone());
        if matches!(
            stop_result,
            Ok(crate::session::commands::PromptTurnOk {
                completion_kind: crate::session::commands::PromptCompletionKind::RemovedFromQueue,
                ..
            })
        ) {
            return Ok(
                acp::PromptResponse::new(acp::StopReason::Cancelled)
                    .meta(
                        build_prompt_response_meta(PromptResponseMetaArgs {
                                session_id: &arguments.session_id.to_string(),
                                prompt_id: &prompt_id,
                                total_tokens: 0,
                                model_id: &model,
                                last_turn_usage: None,
                                prompt_usage: None,
                                cancellation_category: None,
                                cancel_trigger: None,
                                structured_output: None,
                                tool_overrides: applied_tool_overrides.clone(),
                            })
                            .as_object()
                            .cloned(),
                    ),
            );
        }
        let cancel_trigger: Option<String> = stop_result
            .as_ref()
            .ok()
            .and_then(|ok| match &ok.completion_kind {
                crate::session::commands::PromptCompletionKind::Cancelled {
                    context: Some(ctx),
                    ..
                } => ctx.trigger.clone(),
                _ => None,
            });
        {
            let mapped = stop_result
                .as_ref()
                .map(|ok| ok.stop_reason)
                .map_err(Clone::clone);
            let (stop_reason_value, agent_result_value) = crate::sampling::error::prompt_complete_fields(
                &mapped,
            );
            let turn_id = arguments
                .meta
                .as_ref()
                .and_then(|m| m.get("turnId"))
                .and_then(|v| v.as_u64());
            let mut payload = serde_json::json!({
                "sessionId": arguments.session_id.to_string(),
                "promptId": prompt_id.as_str(),
                "stopReason": stop_reason_value,
                "agentResult": agent_result_value,
            });
            if let Some(tid) = turn_id {
                payload["turnId"] = serde_json::json!(tid);
            }
            if let Some(ref t) = cancel_trigger {
                payload["cancelTrigger"] = serde_json::json!(t);
            }
            let params = serde_json::value::to_raw_value(&payload)
                .expect("prompt_complete params serialization");
            self.gateway
                .forward_fire_and_forget(
                    acp::ExtNotification::new(
                        "x.ai/session/prompt_complete",
                        params.into(),
                    ),
                );
        }
        {
            let end_activity = if handle
                .pending_interactions
                .lock()
                .map(|g| !g.is_empty())
                .unwrap_or(false)
            {
                crate::agent::roster::RosterActivity::NeedsInput
            } else {
                crate::agent::roster::RosterActivity::Idle
            };
            self.push_roster_activity_delta(&arguments.session_id, end_activity);
        }
        let resolved_model = handle.get_model_metadata().await.resolved_model_id;
        let harness_trace_turns = {
            let (tx, rx) = oneshot::channel();
            if handle
                .cmd_tx
                .send(SessionCommand::TakeHarnessTraceTurns {
                    respond_to: tx,
                })
                .is_ok()
            {
                rx.await.ok().unwrap_or_default()
            } else {
                Vec::new()
            }
        };
        if trace_context.is_some() && !harness_trace_turns.is_empty() {
            self.upload_harness_trace_turns(
                    &arguments.session_id,
                    &handle.info,
                    &handle.cmd_tx,
                    &model,
                    harness_trace_turns,
                )
                .await;
        }
        match stop_result {
            Ok(turn_ok) => {
                let crate::session::commands::PromptTurnOk {
                    stop_reason,
                    total_tokens,
                    turn_snapshot,
                    completion_kind,
                    structured_output,
                    usage: prompt_usage,
                    tool_overrides: _,
                } = turn_ok;
                let subagent_refs = self
                    .spawned_subagent_refs_for_prompt(
                        arguments.session_id.0.as_ref(),
                        &prompt_id,
                    )
                    .await;
                let permission_events = self
                    .collect_permission_events(&arguments.session_id);
                let turn_messages: Option<xai_chat_state::TurnCapture> = {
                    let (tx, rx) = oneshot::channel();
                    if handle
                        .cmd_tx
                        .send(SessionCommand::TakeTurnMessages {
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    }
                };
                let streaming_partial = crate::upload::turn::take_streaming_partial(
                        &handle.cmd_tx,
                        prompt_id.clone(),
                        matches!(stop_reason, acp::StopReason::EndTurn),
                        Some(model.clone()),
                    )
                    .await
                    .map(|mut cap| {
                        cap.reason
                            .get_or_insert_with(|| match &completion_kind {
                                crate::session::commands::PromptCompletionKind::Cancelled {
                                    category,
                                    ..
                                } => {
                                    match category {
                                        Some(cat) => format!("cancelled:{cat:?}"),
                                        None => "cancelled".to_string(),
                                    }
                                }
                                _ => "non_completed".to_string(),
                            });
                        cap
                    });
                let upload_deadline = block_for_upload
                    .then(|| tokio::time::Instant::now() + upload_flush_timeout);
                if let Some(ctx) = trace_context.clone() {
                    let request_id = prompt_id.clone();
                    let (input_tokens, cached_input_tokens, output_tokens) = turn_snapshot
                        .as_ref()
                        .map(|s| (
                            Some(s.turn_input_tokens),
                            Some(s.turn_cached_input_tokens),
                            Some(s.turn_output_tokens),
                        ))
                        .unwrap_or((None, None, None));
                    if let Some(deadline) = upload_deadline {
                        let completed = matches!(stop_reason, acp::StopReason::EndTurn);
                        let start_for_upload = turn_snapshot
                            .as_ref()
                            .and_then(|s| s.start_prompt_mode.clone())
                            .or_else(|| Some(prompt_mode.to_string()));
                        let end_for_upload = turn_snapshot
                            .as_ref()
                            .and_then(|s| s.end_prompt_mode.clone());
                        let result = TurnResultMetadata {
                            schema_version: GCS_SCHEMA_VERSION,
                            request_id,
                            completed,
                            stop_reason: Some(format!("{stop_reason:?}")),
                            total_tokens: Some(total_tokens),
                            input_tokens,
                            cached_input_tokens,
                            output_tokens,
                            error: None,
                            finished_at: chrono::Utc::now().to_rfc3339(),
                            signals: turn_snapshot.as_ref().map(|s| s.current.clone()),
                            turn_delta: turn_snapshot.as_ref().map(|s| s.delta.clone()),
                            start_prompt_mode: start_for_upload,
                            end_prompt_mode: end_for_upload,
                            resolved_model: resolved_model.clone(),
                            subagents_spawned: subagent_refs.clone(),
                        };
                        upload_turn_result(&ctx, &result, UploadWait::Defer { deadline })
                            .await;
                    } else {
                        let snapshot_clone = turn_snapshot.clone();
                        let resolved_model = resolved_model.clone();
                        tokio::spawn(async move {
                            let completed = matches!(stop_reason, acp::StopReason::EndTurn);
                            let start_for_upload = snapshot_clone
                                .as_ref()
                                .and_then(|s| s.start_prompt_mode.clone())
                                .or_else(|| Some(prompt_mode.to_string()));
                            let end_for_upload = snapshot_clone
                                .as_ref()
                                .and_then(|s| s.end_prompt_mode.clone());
                            let result = TurnResultMetadata {
                                schema_version: GCS_SCHEMA_VERSION,
                                request_id,
                                completed,
                                stop_reason: Some(format!("{stop_reason:?}")),
                                total_tokens: Some(total_tokens),
                                input_tokens,
                                cached_input_tokens,
                                output_tokens,
                                error: None,
                                finished_at: chrono::Utc::now().to_rfc3339(),
                                signals: snapshot_clone.as_ref().map(|s| s.current.clone()),
                                turn_delta: snapshot_clone
                                    .as_ref()
                                    .map(|s| s.delta.clone()),
                                start_prompt_mode: start_for_upload,
                                end_prompt_mode: end_for_upload,
                                resolved_model,
                                subagents_spawned: subagent_refs.clone(),
                            };
                            upload_turn_result(&ctx, &result, UploadWait::Confirm).await;
                        });
                    }
                }
                if let Some(ctx) = trace_context {
                    let (session_copy_tx, session_copy_rx) = oneshot::channel();
                    let copy_sent = ctx
                        .session_handle
                        .cmd_tx
                        .send(SessionCommand::CopyFile {
                            respond_to: session_copy_tx,
                        })
                        .is_ok();
                    if !copy_sent {
                        tracing::warn!(
                            session_id = %ctx.session_info.id.0,
                            turn_number = ctx.turn_number,
                            "Failed to send CopyFile command, skipping session state upload"
                        );
                    }
                    if turn_number == 0
                        && let Some(client) = self.session_registry_client()
                    {
                        let cwd_str = handle.info.cwd.clone();
                        let model = self.models_manager.current_model_id().0.to_string();
                        let hostname = gethostname::gethostname()
                            .to_string_lossy()
                            .to_string();
                        let suppress = self
                            .auth_manager
                            .current_or_expired()
                            .is_some_and(|a| a.is_zdr_team());
                        let device_id = if suppress { None } else { Some(agent_id()) };
                        let first_prompt = if suppress {
                            None
                        } else {
                            arguments
                                    .prompt
                                    .iter()
                                    .find_map(|b| {
                                        if let acp::ContentBlock::Text(t) = b {
                                            Some(t.text.clone())
                                        } else {
                                            None
                                        }
                                    })
                        };
                        let sid = arguments.session_id.to_string();
                        tokio::spawn(async move {
                            let git_out = |args: &[&str]| -> Option<String> {
                                xai_tty_utils::git_command()
                                    .current_dir(&cwd_str)
                                    .args(args)
                                    .output()
                                    .ok()
                                    .filter(|o| o.status.success())
                                    .map(|o| {
                                        String::from_utf8_lossy(&o.stdout).trim().to_string()
                                    })
                                    .filter(|s| !s.is_empty())
                            };
                            let repo_remote_url = git_out(
                                &["remote", "get-url", "origin"],
                            );
                            let repo_branch = git_out(
                                &["rev-parse", "--abbrev-ref", "HEAD"],
                            );
                            let repo_head_at_start = git_out(&["rev-parse", "HEAD"]);
                            let reg_req = crate::agent::session_registry_client::RegisterRequest {
                                session_id: sid.clone(),
                                cwd: cwd_str,
                                gcs_trace_prefix: sid,
                                model_id: Some(model),
                                repo_remote_url,
                                repo_branch,
                                repo_head_at_start,
                                hostname: Some(hostname),
                                device_id,
                                parent_session_id: None,
                                session_kind: None,
                                subagent_type: None,
                                subagent_persona: None,
                                subagent_role: None,
                                fork_context_source: None,
                                subagent_depth: None,
                            };
                            if let Err(e) = client.register(&reg_req).await {
                                tracing::warn!(
                                    error = %e,
                                    "session registry register failed (non-fatal)"
                                );
                            }
                            let info = crate::session::info::Info {
                                id: agent_client_protocol::SessionId::new(
                                    reg_req.session_id.clone(),
                                ),
                                cwd: reg_req.cwd.clone(),
                            };
                            let summary_path = crate::session::persistence::session_dir(
                                    &info,
                                )
                                .join("summary.json");
                            let summary = if suppress {
                                None
                            } else {
                                std::fs::read(&summary_path)
                                        .ok()
                                        .and_then(|bytes| {
                                            serde_json::from_slice::<
                                                crate::session::persistence::Summary,
                                            >(&bytes)
                                                .ok()
                                        })
                                        .map(|s| s.session_summary)
                                        .filter(|s| !s.is_empty())
                            };
                            if first_prompt.is_some() || summary.is_some() {
                                let upd_req = crate::agent::session_registry_client::UpdateRequest {
                                    summary,
                                    first_prompt,
                                    last_turn_number: None,
                                    repo_head_at_end: None,
                                    restorable_turn_number: None,
                                };
                                tracing::debug!(
                                    session_id = %reg_req.session_id,
                                    has_summary = upd_req.summary.is_some(),
                                    "session registry post-register update"
                                );
                                if let Err(e) = client
                                    .update(&reg_req.session_id, &upd_req)
                                    .await
                                {
                                    tracing::warn!(
                                        error = %e,
                                        "session registry first-prompt update failed (non-fatal)"
                                    );
                                }
                            }
                        });
                    }
                    let registry_turn = i32::try_from(turn_number).unwrap_or(i32::MAX);
                    let cwd_for_git = handle.info.cwd.clone();
                    /// Advances `last_turn_number` immediately after a turn completes.
                    ///
                    /// Fired right after the session turn finishes, before any artifact uploads.
                    /// Sets `last_turn_number` with `repo_head_at_end` and does not wait for
                    /// session-state uploads.
                    async fn advance_last_turn(
                        client: crate::agent::session_registry_client::SessionRegistryClient,
                        session_id: String,
                        turn: i32,
                        cwd: String,
                    ) {
                        let repo_head_at_end = xai_tty_utils::git_command()
                            .current_dir(&cwd)
                            .args(["rev-parse", "HEAD"])
                            .output()
                            .ok()
                            .filter(|o| o.status.success())
                            .map(|o| {
                                String::from_utf8_lossy(&o.stdout).trim().to_string()
                            })
                            .filter(|s| !s.is_empty());
                        let req = crate::agent::session_registry_client::UpdateRequest {
                            summary: None,
                            first_prompt: None,
                            last_turn_number: Some(turn),
                            repo_head_at_end,
                            restorable_turn_number: None,
                        };
                        if let Err(e) = client.update(&session_id, &req).await {
                            tracing::warn!(
                                error = %e,
                                "session registry last_turn_number update failed (non-fatal)"
                            );
                        }
                    }
                    /// Advances `restorable_turn_number` after required restore artifacts are
                    /// confirmed durable.
                    ///
                    /// Called after the post-turn session archive is confirmed in cloud storage.
                    async fn advance_restorable_turn(
                        client: crate::agent::session_registry_client::SessionRegistryClient,
                        session_id: String,
                        turn: i32,
                    ) {
                        let req = crate::agent::session_registry_client::UpdateRequest {
                            summary: None,
                            first_prompt: None,
                            last_turn_number: None,
                            repo_head_at_end: None,
                            restorable_turn_number: Some(turn),
                        };
                        if let Err(e) = client.update(&session_id, &req).await {
                            tracing::warn!(
                                error = %e,
                                "session registry restorable_turn_number update failed (non-fatal)"
                            );
                        }
                    }
                    if let Some(client) = self.session_registry_client() {
                        let sid = arguments.session_id.to_string();
                        let cwd = cwd_for_git.clone();
                        tokio::spawn(async move {
                            advance_last_turn(client, sid, registry_turn, cwd).await;
                        });
                    }
                    {
                        let cwd = cwd_for_git.clone();
                        let cmd_tx = handle.cmd_tx.clone();
                        tokio::spawn(async move {
                            let head = xai_grok_workspace::session::git::get_current_commit(
                                    std::path::Path::new(&cwd),
                                )
                                .await;
                            let branch = xai_grok_workspace::session::git::get_branch(
                                    std::path::Path::new(&cwd),
                                )
                                .await;
                            let _ = cmd_tx
                                .send(crate::session::SessionCommand::PersistGitHead {
                                    commit: head,
                                    branch,
                                });
                        });
                    }
                    let registry_client_for_restorable = self.session_registry_client();
                    let registry_sid_for_restorable = arguments.session_id.to_string();
                    let err_ctx = ctx.clone();
                    if let Some(deadline) = upload_deadline {
                        match complete_prompt_trace(
                                ctx,
                                permission_events,
                                session_copy_rx,
                                turn_messages,
                                streaming_partial,
                                UploadWait::Defer { deadline },
                            )
                            .await
                        {
                            Ok(true) => {
                                if let Some(client) = registry_client_for_restorable {
                                    advance_restorable_turn(
                                            client,
                                            registry_sid_for_restorable,
                                            registry_turn,
                                        )
                                        .await;
                                }
                            }
                            Ok(false) => {
                                tracing::debug!(
                                    "session state unconfirmed within the flush budget; \
                                     skipping restorable_turn_number advance"
                                );
                            }
                            Err(e) => {
                                tracing::warn!("Failed to complete prompt trace: {e:?}");
                                crate::upload::trace::flush_then_write_error_manifest(
                                        &err_ctx,
                                        deadline,
                                    )
                                    .await;
                            }
                        }
                    } else {
                        spawn_upload_task(
                            "after_uploads",
                            async move {
                                match complete_prompt_trace(
                                        ctx,
                                        permission_events,
                                        session_copy_rx,
                                        turn_messages,
                                        streaming_partial,
                                        UploadWait::Confirm,
                                    )
                                    .await
                                {
                                    Ok(true) => {
                                        if let Some(client) = registry_client_for_restorable {
                                            advance_restorable_turn(
                                                    client,
                                                    registry_sid_for_restorable,
                                                    registry_turn,
                                                )
                                                .await;
                                        }
                                    }
                                    Ok(false) => {
                                        tracing::warn!(
                                        "Session state upload failed; skipping registry \
                                         restorable_turn_number advance"
                                    );
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to complete prompt trace: {e:?}");
                                        write_error_manifest(&err_ctx).await;
                                    }
                                }
                            },
                        );
                    }
                }
                let last_turn_usage = last_turn_usage_for_meta;
                let cancellation_category = match &completion_kind {
                    crate::session::commands::PromptCompletionKind::Cancelled {
                        category: Some(cat),
                        ..
                    } => Some(format!("{cat:?}")),
                    crate::session::commands::PromptCompletionKind::MaxTurnsReached {
                        ..
                    } => Some("max_turns_reached".to_string()),
                    crate::session::commands::PromptCompletionKind::StationarityEnded => {
                        Some("action_stationarity".to_string())
                    }
                    _ => None,
                };
                Ok(
                    acp::PromptResponse::new(stop_reason)
                        .meta(
                            build_prompt_response_meta(PromptResponseMetaArgs {
                                    session_id: &arguments.session_id.to_string(),
                                    prompt_id: &prompt_id,
                                    total_tokens,
                                    model_id: &model,
                                    last_turn_usage: last_turn_usage.as_ref(),
                                    prompt_usage,
                                    cancellation_category,
                                    cancel_trigger,
                                    structured_output,
                                    tool_overrides: applied_tool_overrides,
                                })
                                .as_object()
                                .cloned(),
                        ),
                )
            }
            Err(err) => {
                let subagent_refs = self
                    .spawned_subagent_refs_for_prompt(
                        arguments.session_id.0.as_ref(),
                        &prompt_id,
                    )
                    .await;
                let turn_messages: Option<xai_chat_state::TurnCapture> = {
                    let (tx, rx) = oneshot::channel();
                    if handle
                        .cmd_tx
                        .send(SessionCommand::TakeTurnMessages {
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    }
                };
                let err_kind_str = format!("{:?}", err.code);
                let streaming_partial = crate::upload::turn::take_streaming_partial(
                        &handle.cmd_tx,
                        prompt_id.clone(),
                        false,
                        Some(model.clone()),
                    )
                    .await
                    .map(|mut cap| {
                        cap.reason = Some(format!("sampler_error:{err_kind_str}"));
                        cap
                    });
                if let Some(ctx) = trace_context.clone() {
                    let request_id = prompt_id.clone();
                    let err_str = format!("{err:?}");
                    let stop_reason = crate::sampling::error::stop_reason_for_turn_error(
                            &err,
                        )
                        .to_string();
                    let upload_unified = matches!(
                        crate::sampling::error::http_status_from_error(&err),
                        Some(401 | 404),
                    );
                    let upload_deadline = block_for_upload
                        .then(|| tokio::time::Instant::now() + upload_flush_timeout);
                    if let Some(deadline) = upload_deadline {
                        let result = TurnResultMetadata {
                            schema_version: GCS_SCHEMA_VERSION,
                            request_id,
                            completed: false,
                            stop_reason: Some(stop_reason),
                            total_tokens: None,
                            input_tokens: None,
                            cached_input_tokens: None,
                            output_tokens: None,
                            error: Some(err_str),
                            finished_at: chrono::Utc::now().to_rfc3339(),
                            signals: None,
                            turn_delta: None,
                            start_prompt_mode: Some(prompt_mode.to_string()),
                            end_prompt_mode: None,
                            resolved_model: resolved_model.clone(),
                            subagents_spawned: subagent_refs.clone(),
                        };
                        let wait = UploadWait::Defer { deadline };
                        upload_turn_result(&ctx, &result, wait).await;
                        if let Some(capture) = turn_messages {
                            upload_turn_messages(&ctx, capture, wait).await;
                        }
                        if let Some(ref capture) = streaming_partial {
                            crate::upload::trace::upload_streaming_partial(
                                    &ctx,
                                    capture,
                                    wait,
                                )
                                .await;
                        }
                        if upload_unified {
                            upload_unified_log(&ctx, wait).await;
                        }
                        crate::upload::trace::flush_then_write_error_manifest(
                                &ctx,
                                deadline,
                            )
                            .await;
                    } else {
                        let resolved_model = resolved_model.clone();
                        spawn_upload_task(
                            "error_turn_result",
                            async move {
                                let result = TurnResultMetadata {
                                    schema_version: GCS_SCHEMA_VERSION,
                                    request_id,
                                    completed: false,
                                    stop_reason: Some(stop_reason),
                                    total_tokens: None,
                                    input_tokens: None,
                                    cached_input_tokens: None,
                                    output_tokens: None,
                                    error: Some(err_str),
                                    finished_at: chrono::Utc::now().to_rfc3339(),
                                    signals: None,
                                    turn_delta: None,
                                    start_prompt_mode: Some(prompt_mode.to_string()),
                                    end_prompt_mode: None,
                                    resolved_model,
                                    subagents_spawned: subagent_refs.clone(),
                                };
                                upload_turn_result(&ctx, &result, UploadWait::Confirm)
                                    .await;
                                if let Some(capture) = turn_messages {
                                    upload_turn_messages(&ctx, capture, UploadWait::Confirm)
                                        .await;
                                }
                                if let Some(ref capture) = streaming_partial {
                                    crate::upload::trace::upload_streaming_partial(
                                            &ctx,
                                            capture,
                                            UploadWait::Confirm,
                                        )
                                        .await;
                                }
                                if upload_unified {
                                    upload_unified_log(&ctx, UploadWait::Confirm).await;
                                }
                                write_error_manifest(&ctx).await;
                            },
                        );
                    }
                }
                let err = if crate::sampling::error::prompt_usage_from_error(&err)
                    .is_some()
                {
                    err
                } else {
                    let prompt_id = handle
                        .current_prompt_id
                        .lock()
                        .ok()
                        .and_then(|g| g.clone());
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let usage = if handle
                        .cmd_tx
                        .send(crate::session::commands::SessionCommand::ErrorPathUsageFallback {
                            prompt_id,
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    };
                    crate::sampling::error::attach_prompt_usage(err, usage)
                };
                Err(err)
            }
        }
    }
    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        tracing::info!("Received cancel request {args:?}");
        let handle = self.session_handle_waiting_for_load(&args.session_id).await;
        let cancel_trigger = args
            .meta
            .as_ref()
            .and_then(|m| m.get("cancelTrigger"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        xai_grok_telemetry::unified_log::info(
            "shell.cancel.received",
            Some(args.session_id.0.as_ref()),
            Some(
                serde_json::json!({
                "session_found": handle.is_some(),
                "trigger": cancel_trigger,
            }),
            ),
        );
        if let Some(handle) = handle {
            let cancel_subagents = args
                .meta
                .as_ref()
                .and_then(|m| m.get("cancelSubagents"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let rewind_if_pristine = args
                .meta
                .as_ref()
                .and_then(|m| m.get("rewindIfPristine"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let dispatch_lock = self.dispatch_lock(&args.session_id);
            let _dispatch_guard = dispatch_lock.lock().await;
            let _ = handle
                .cmd_tx
                .send(SessionCommand::Cancel {
                    cancel_subagents,
                    kill_background_tasks: false,
                    rewind_if_pristine,
                    trigger: cancel_trigger,
                });
        }
        Ok(())
    }
    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        tracing::info!("Received set session mode request {args:?}");
        let handle = self.session_handle_waiting_for_load(&args.session_id).await;
        let (tx, rx) = oneshot::channel();
        if let Some(handle) = handle {
            let _ = handle
                .cmd_tx
                .send(SessionCommand::SessionMode {
                    session_mode: args.mode_id,
                    responds_to: tx,
                });
        }
        let _ = rx
            .await
            .map_err(|_| {
                acp::Error::internal_error().data("response to set session failed")
            })?;
        Ok(acp::SetSessionModeResponse::new())
    }
    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        let reload_model_config = args
            .meta
            .as_ref()
            .and_then(|m| m.get(crate::agent::config::RELOAD_MODEL_CONFIG_META_KEY))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if reload_model_config {
            crate::extensions::session_admin::reload_models_from_disk(self)?;
        }
        let model = self.resolve_model_id(&args.model_id)?;
        if !model.info.user_selectable {
            return Err(
                acp::Error::invalid_params()
                    .data("This model isn't allowed by your allowed_models setting."),
            );
        }
        // Locked platform model: present in the picker for BYOK discovery but
        // not usable until the provider credential is configured. Reject here
        // too (not just client-side) so a third-party base URL can never be
        // reached with the xAI session token via a crafted set_model call.
        if model.is_managed_platform_model() && !model.has_own_credentials() {
            let (provider_name, hint) = model
                .managed_provider()
                .and_then(|id| xai_grok_models::provider_spec(id.as_str()))
                .map(|provider| (provider.display_name.as_str(), provider.setup_hint()))
                .unwrap_or(("this provider", String::new()));
            return Err(acp::Error::invalid_params().data(format!(
                "Provider '{provider_name}' is not configured. To enable it: {hint}"
            )));
        }
        let session_id = args.session_id.clone();
        let res = crate::agent::handlers::model_switch::apply(self, args).await;
        if res.is_ok()
            && let Some(unavailable) = self
                .model_unavailable_sessions
                .borrow_mut()
                .remove(session_id.0.as_ref())
        {
            tracing::info!(
                session_id = %session_id.0,
                previously_unavailable_model = %unavailable.0,
                "set_session_model: user model switch cleared the model-unavailable block"
            );
        }
        res
    }
    #[tracing::instrument(
        name = "agent.ext_method",
        skip_all,
        fields(method = %args.method)
    )]
    async fn ext_method(
        &self,
        args: acp::ExtRequest,
    ) -> Result<acp::ExtResponse, acp::Error> {
        let request_meta = serde_json::from_str::<serde_json::Value>(args.params.get())
            .ok()
            .and_then(|v| v.get("_meta").cloned());
        if let Some(meta) = &request_meta {
            xai_file_utils::trace_context::link_current_span_to_meta(meta);
        }
        tracing::info!("Received extension method call: method={}", args.method);
        #[allow(unused_mut)]
        let mut backend_no_bridge_err: Option<acp::Error> = None;
        let method = args.method.clone();
        let result = match method.as_ref() {
            "x.ai/getApiKey" | "x.ai/setApiKey" => {
                crate::extensions::auth::handle(self, &args).await
            }
            "x.ai/session/info" | "x.ai/session/close" | "x.ai/session/list"
            | "x.ai/sessions/list" => {
                crate::agent::handlers::session::handle(self, &args).await
            }
            "x.ai/workspaces/list" => {
                crate::agent::handlers::workspaces::handle(self, &args).await
            }
            "x.ai/session/updates" => {
                crate::extensions::session_updates::handle(&args, &self.gateway).await
            }
            "x.ai/session/state" => {
                crate::extensions::session_state::handle_state(&args).await
            }
            "x.ai/session/import" => {
                crate::extensions::session_state::handle_import(&args).await
            }
            "x.ai/session/load_history" => {
                crate::extensions::chat_conversation_history::handle(self, &args).await
            }
            "x.ai/session/search" => {
                crate::extensions::session_search::handle(&args).await
            }
            "x.ai/session/resolve_local_for_worktree_resume"
            | "x.ai/session/rehydrate" => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::worktree::handle(self, &ops, &args).await
            }
            "x.ai/session/rename" | "x.ai/session/delete"
            | "x.ai/session/update_mcp_servers" | "x.ai/session/fork"
            | "x.ai/internal/reload_all_mcp_servers"
            | "x.ai/internal/reload_project_mcp_servers" | "x.ai/internal/reload_skills"
            | "x.ai/internal/reload_workflows" | "x.ai/internal/reload_models"
            | "x.ai/internal/reload_models_cache" | "x.ai/internal/reload_subagent_models"
            | "x.ai/internal/set_platform_api_key"
            | "x.ai/internal/auth_cleared" | "x.ai/plugins/reload"
            | "x.ai/commands/list" => {
                crate::extensions::session_admin::handle(self, &args).await
            }
            "x.ai/session/repair" => crate::extensions::repair::handle(self, &args).await,
            "x.ai/session/usage" => crate::extensions::usage::handle(self, &args).await,
            "x.ai/memory/flush" | "x.ai/memory/rewrite" => {
                crate::extensions::memory::handle(self, &args).await
            }
            "x.ai/skills/refresh-baseline" => {
                self.refresh_skill_baseline_for_all_sessions();
                crate::extensions::to_ext_response(
                    Ok(serde_json::json!({"ok": true})),
                )
            }
            "x.ai/interject" => crate::extensions::interject::handle(self, &args).await,
            "x.ai/feedback" | "x.ai/feedback/dismiss" | "x.ai/btw" => {
                crate::extensions::feedback::handle(self, &args).await
            }
            "x.ai/recap" => crate::extensions::recap::handle(self, &args).await,
            "x.ai/cloud/terminate" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let sandbox_id = params
                    .get("sandbox_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::invalid_params().data("missing sandbox_id")
                    })?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                sandbox_client
                    .terminate_session(
                        sandbox_id,
                        &crate::remote::SandboxTerminateRequest {
                            environment_id: None,
                        },
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to terminate sandbox: {e}"))
                    })?;
                crate::extensions::to_raw_response(&serde_json::json!({ "ok": true }))
            }
            "x.ai/cloud/env/list" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                let resp = sandbox_client
                    .list_environments(
                        &crate::remote::SandboxListEnvironmentsRequest::default(),
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to list environments: {e}"))
                    })?;
                crate::extensions::to_raw_response(
                    &serde_json::json!({
                    "environments": resp.environments,
                }),
                )
            }
            "x.ai/cloud/env/create" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                let resp = sandbox_client
                    .create_environment(
                        &crate::remote::SandboxCreateEnvironmentRequest {
                            name: params
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            description: params
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            repository: params
                                .get("repository")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            default_branch: params
                                .get("default_branch")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            container_image: params
                                .get("container_image")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            setup_script: params
                                .get("setup_script")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            workspace_directory: Some("/workspace".to_string()),
                            internet_enabled: Some(true),
                            domain_allowlist_preset: Some("common".to_string()),
                            allowed_http_methods: Some("all".to_string()),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to create environment: {e}"))
                    })?;
                crate::extensions::to_raw_response(
                    &serde_json::json!({
                    "environment": resp.environment,
                }),
                )
            }
            "x.ai/cloud/env/update" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let environment_id = params
                    .get("environment_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::invalid_params().data("missing environment_id")
                    })?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                let resp = sandbox_client
                    .update_environment(
                        environment_id,
                        &crate::remote::SandboxUpdateEnvironmentRequest {
                            name: params
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            description: params
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            repository: params
                                .get("repository")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            default_branch: params
                                .get("default_branch")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            container_image: params
                                .get("container_image")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            setup_script: params
                                .get("setup_script")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to update environment: {e}"))
                    })?;
                crate::extensions::to_raw_response(
                    &serde_json::json!({
                    "environment": resp.environment,
                }),
                )
            }
            "x.ai/cloud/env/delete" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    "Run `grok login` to authenticate.",
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let environment_id = params
                    .get("environment_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::invalid_params().data("missing environment_id")
                    })?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                sandbox_client
                    .delete_environment(environment_id)
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to delete environment: {e}"))
                    })?;
                crate::extensions::to_raw_response(&serde_json::json!({ "ok": true }))
            }
            "x.ai/billing" => crate::extensions::billing::handle(self, &args).await,
            "x.ai/auto-topup-rule" => {
                crate::extensions::billing::handle(self, &args).await
            }
            "x.ai/share_session" => crate::extensions::share::handle(self, &args).await,
            "x.ai/privacy/setCodingDataRetention" => {
                crate::extensions::privacy::handle(self, &args).await
            }
            "x.ai/rollout/survey" => {
                crate::extensions::rollout::handle(self, &args).await
            }
            "x.ai/prompt_history" => {
                crate::extensions::prompt_history::handle(self, &args).await
            }
            "x.ai/suggest" => crate::extensions::suggest::handle(self, &args).await,
            "x.ai/suggestPrompt" => crate::extensions::suggest::handle(self, &args).await,
            s if s.starts_with("x.ai/auth/") => {
                crate::extensions::auth::handle(self, &args).await
            }
            s if s.starts_with("x.ai/session_summaries/") => {
                crate::agent::handlers::session::handle(self, &args).await
            }
            s if s.starts_with("x.ai/git/worktree/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::worktree::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/git/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::git::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/compact_conversation") => {
                crate::extensions::memory::handle(self, &args).await
            }
            s if s.starts_with("x.ai/plugins/") => {
                crate::extensions::plugins::handle(self, &args).await
            }
            s if s.starts_with("x.ai/marketplace/") => {
                crate::extensions::marketplace::handle(self, &args).await
            }
            s if s.starts_with("x.ai/hooks/") => {
                crate::extensions::hooks::handle(self, &args).await
            }
            s if s.starts_with("x.ai/hunk-tracker/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::hunk_tracker::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/pr/") => {
                crate::extensions::pr::handle(self, &args).await
            }
            s if s.starts_with(crate::extensions::mcp::mcp_methods::PREFIX) => {
                crate::extensions::mcp::handle(self, &args).await
            }
            s if s.starts_with("x.ai/task/") => {
                crate::extensions::task::handle(self, &args).await
            }
            s if s.starts_with("x.ai/scheduler/") => {
                crate::extensions::task::handle_scheduler(self, &args).await
            }
            s if s.starts_with("x.ai/subagent/") => {
                crate::extensions::task::handle_subagent(self, &args).await
            }
            s if s.starts_with("x.ai/terminal/") => {
                crate::extensions::terminal::handle(self, &args).await
            }
            s if crate::extensions::fs::is_fs_method(s) => {
                crate::extensions::fs::handle(self, &args).await
            }
            s if s.starts_with("x.ai/search/") => {
                crate::extensions::search::handle(self, &args).await
            }
            s if s.starts_with("x.ai/bundle/") => {
                crate::extensions::bundle::handle(self, &args).await
            }
            s if s.starts_with("x.ai/code/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::code_nav::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/skills/") || s == "x.ai/workflows/list" => {
                let compat = self.cfg.borrow().compat_resolved;
                crate::extensions::skills::handle(
                        self,
                        &args,
                        self.plugin_registry_handle.snapshot().as_deref(),
                        compat,
                    )
                    .await
            }
            s if s.starts_with("x.ai/review") => {
                crate::extensions::feedback::handle(self, &args).await
            }
            s if s.starts_with("x.ai/debug/") => {
                crate::extensions::debug::handle(self, &args).await
            }
            s if s.starts_with("x.ai/rewind") => {
                crate::extensions::rewind::handle(self, &args).await
            }
            other => {
                Err(
                    acp::Error::method_not_found()
                        .data(format!("unknown ACP extension method: {other}")),
                )
            }
        };
        if let Some(err) = backend_no_bridge_err
            && matches!(&result, Err(e) if e.code == acp::Error::method_not_found().code)
        {
            return Err(err);
        }
        result
    }
    async fn ext_notification(
        &self,
        args: acp::ExtNotification,
    ) -> Result<(), acp::Error> {
        tracing::info!("Received extension notification: method={}", args.method);
        if args.method.as_ref() == "x.ai/yolo_mode_changed"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let sender_id = params.get("clientIdentifier").and_then(|v| v.as_str());
            let permission_mode = params
                .get("permission_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let yolo_signal = params.get("yolo_mode").and_then(|v| v.as_bool());
            if let Some(yolo_mode) = yolo_signal {
                let mut sessions = self.sessions.borrow_mut();
                let updated_sessions = apply_yolo_mode_to_matching_sessions(
                    &mut sessions,
                    sender_id,
                    yolo_mode,
                );
                tracing::info!(
                    yolo_mode,
                    sender = ?sender_id,
                    target_sessions = updated_sessions,
                    total_sessions = sessions.len(),
                    "Setting YOLO mode for matching sessions"
                );
            }
            let auto_mode_explicit = params.get("auto_mode").and_then(|v| v.as_bool());
            let want_auto = auto_mode_explicit == Some(true)
                || permission_mode == "auto";
            let clear_auto = auto_mode_explicit == Some(false)
                || (matches!(permission_mode, "always-approve" | "ask" | "default")
                    && !want_auto);
            let enable_auto = want_auto && yolo_signal != Some(true);
            if enable_auto || clear_auto {
                let enabled = enable_auto;
                let matches_sender = |h: &crate::session::SessionHandle| -> bool {
                    sender_id.is_none()
                        || h.origin_client.as_ref().map(|c| c.product.as_str())
                            == sender_id
                };
                let mut sessions = self.sessions.borrow_mut();
                let total_sessions = sessions.len();
                let mut updated = 0;
                for h in sessions.values_mut() {
                    if !matches_sender(h) {
                        continue;
                    }
                    if h
                        .cmd_tx
                        .send(crate::session::SessionCommand::SetAutoMode {
                            enabled,
                        })
                        .is_ok()
                    {
                        if enabled {
                            h.yolo_mode = false;
                        }
                        updated += 1;
                    }
                }
                tracing::info!(
                    auto_mode = enabled,
                    sender = ?sender_id,
                    target_sessions = updated,
                    total_sessions,
                    "Setting auto permission mode for matching sessions"
                );
            }
        }
        if args.method.as_ref() == "x.ai/permissions/reset" {
            let sessions = self.sessions.borrow();
            let updated = sessions
                .values()
                .filter(|h| {
                    h
                        .cmd_tx
                        .send(crate::session::SessionCommand::ResetPermissionState)
                        .is_ok()
                })
                .count();
            tracing::info!(
                target_sessions = updated,
                total_sessions = sessions.len(),
                "Permission state reset for matching sessions"
            );
        }
        if args.method.as_ref() == "x.ai/internal/evict_sessions" {
            self.handle_evict_sessions(&args.params).await;
        }
        if args.method.as_ref() == "x.ai/toggle_plan_mode"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let session_id_str = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let handle = self
                .sessions
                .borrow()
                .values()
                .find(|s| s.info.id.0.as_ref() == session_id_str)
                .cloned();
            if let Some(handle) = handle {
                let is_engaged = handle.plan_mode.lock().state()
                    != crate::session::plan_mode::PlanModeState::Inactive;
                let next_mode_id = acp::SessionModeId::new(
                    if is_engaged { "default" } else { "plan" },
                );
                let (tx, rx) = oneshot::channel();
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::SessionMode {
                        session_mode: next_mode_id.clone(),
                        responds_to: tx,
                    });
                if rx.await.is_err() {
                    tracing::warn!(
                        session_id = %session_id_str,
                        mode_id = %next_mode_id.0,
                        "toggle_plan_mode: session mode update failed"
                    );
                }
            } else {
                tracing::warn!(
                    session_id = %session_id_str,
                    "toggle_plan_mode: session not found"
                );
            }
        }
        if matches!(
            args.method.as_ref(),
            "x.ai/queue/remove"
                | "x.ai/queue/reorder"
                | "x.ai/queue/clear"
                | "x.ai/queue/edit"
                | "x.ai/queue/interject"
        )
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let session_id_str = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let owner = params
                .get("owner")
                .or_else(|| params.get("clientIdentifier"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let handle = self
                .sessions
                .borrow()
                .values()
                .find(|s| s.info.id.0.as_ref() == session_id_str)
                .cloned();
            if let Some(handle) = handle {
                let cmd = crate::agent::ext_parsers::parse_queue_edit_command(
                    args.method.as_ref(),
                    &params,
                    owner,
                );
                if let Some(cmd) = cmd && handle.cmd_tx.send(cmd).is_err() {
                    tracing::warn!(
                        session_id = %session_id_str,
                        method = %args.method,
                        "queue edit: failed to forward SessionCommand (session actor gone)"
                    );
                }
            } else {
                tracing::warn!(
                    session_id = %session_id_str,
                    method = %args.method,
                    "queue edit: session not found"
                );
            }
        }
        if args.method.as_ref() == "x.ai/terminal/pty/input"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            crate::extensions::terminal::handle_pty_input(&params).await;
        }
        if args.method.as_ref() == "_x.ai/session/update" {
            if let Ok(notification) = serde_json::from_str::<
                SessionNotification,
            >(args.params.get()) {
                tracing::info!(
                    "Storing xAI session notification: session_id={}",
                    notification.session_id.0
                );
                if let Some(handle) = self
                    .sessions
                    .borrow()
                    .get(&notification.session_id)
                {
                    let _ = handle
                        .cmd_tx
                        .send(crate::session::SessionCommand::XaiSessionNotification {
                            notification,
                        });
                } else {
                    tracing::warn!(
                        "Received xAI session notification for unknown session: {}",
                        notification.session_id.0
                    );
                }
            } else {
                tracing::warn!("Failed to parse xAI session notification params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/non_git_decision" {
            #[derive(serde::Deserialize)]
            struct NonGitDecisionParams {
                decision: String,
                session_id: String,
                #[serde(default)]
                client_version: Option<String>,
            }
            if let Ok(params) = serde_json::from_str::<
                NonGitDecisionParams,
            >(args.params.get()) {
                tracing::info!(
                    decision = %params.decision,
                    session_id = %params.session_id,
                    client_version = ?params.client_version,
                    "non_git_decision",
                );
                xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::NonGitDecisionEvent {
                    decision: params.decision,
                    session_id: params.session_id,
                    client_version: params.client_version,
                });
            } else {
                tracing::warn!("Failed to parse non_git_decision telemetry params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/multi_agent_followup" {
            #[derive(serde::Deserialize)]
            struct MultiAgentFollowupParams {
                preferred_agent_label: char,
                preferred_agent_session_id: Option<String>,
                preferred_agent_model_id: Option<String>,
                /// (label, session_id, model_id)
                other_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentFollowupParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent followup telemetry: preferred_agent={}",
                    params.preferred_agent_label
                );
                let total_agents = 1 + params.other_agents.len();
                xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::MultiAgentFollowup {
                    preferred_agent_label: params.preferred_agent_label.to_string(),
                    preferred_agent_session_id: params.preferred_agent_session_id,
                    preferred_agent_model_id: params.preferred_agent_model_id,
                    other_agents: params
                        .other_agents
                        .into_iter()
                        .map(|(l, s, m)| xai_grok_telemetry::events::AgentInfo {
                            label: l.to_string(),
                            session_id: s,
                            model_id: m,
                        })
                        .collect(),
                    total_agents,
                });
            } else {
                tracing::warn!("Failed to parse multi-agent followup telemetry params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/multi_agent_apply" {
            #[derive(serde::Deserialize)]
            struct MultiAgentApplyParams {
                applied_agent_label: char,
                applied_agent_session_id: Option<String>,
                applied_agent_model_id: Option<String>,
                /// (label, session_id, model_id)
                discarded_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentApplyParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent apply telemetry: applied_agent={}",
                    params.applied_agent_label
                );
                let total_agents = 1 + params.discarded_agents.len();
                xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::MultiAgentApply {
                    applied_agent_label: params.applied_agent_label.to_string(),
                    applied_agent_session_id: params.applied_agent_session_id,
                    applied_agent_model_id: params.applied_agent_model_id,
                    discarded_agents: params
                        .discarded_agents
                        .into_iter()
                        .map(|(l, s, m)| xai_grok_telemetry::events::AgentInfo {
                            label: l.to_string(),
                            session_id: s,
                            model_id: m,
                        })
                        .collect(),
                    total_agents,
                });
            } else {
                tracing::warn!("Failed to parse multi-agent apply telemetry params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/multi_agent_discard" {
            #[derive(serde::Deserialize)]
            struct MultiAgentDiscardParams {
                /// (label, session_id, model_id)
                discarded_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentDiscardParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent discard telemetry: {} agents discarded",
                    params.discarded_agents.len()
                );
                let total = params.discarded_agents.len();
                xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::MultiAgentDiscard {
                    discarded_agents: params
                        .discarded_agents
                        .into_iter()
                        .map(|(l, s, m)| xai_grok_telemetry::events::AgentInfo {
                            label: l.to_string(),
                            session_id: s,
                            model_id: m,
                        })
                        .collect(),
                    total_agents_discarded: total,
                });
            } else {
                tracing::warn!("Failed to parse multi-agent discard telemetry params");
            }
        }
        if args.method.as_ref() == xai_grok_telemetry::unified_log::LOG_METHOD
            && let Ok(params) = serde_json::from_str::<
                xai_grok_telemetry::unified_log::LogNotificationParams,
            >(args.params.get())
        {
            xai_grok_telemetry::unified_log::ingest_client_entries(
                params.src,
                &params.entries,
            );
        }
        Ok(())
    }
}
#[cfg(test)]
mod tool_overrides_capability_tests {
    use super::tool_overrides_capability;
    #[test]
    fn capability_wire_shape_is_pinned() {
        assert_eq!(
            tool_overrides_capability(),
            serde_json::json!({
                "x_keyword_search": true,
                "x_semantic_search": true,
                "x_user_search": false,
                "x_thread_fetch": false,
            }),
        );
    }
}
