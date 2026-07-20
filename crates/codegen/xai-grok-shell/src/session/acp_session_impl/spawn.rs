//! Session bring-up concern for `acp_session`: `spawn_session_actor`, the
//! per-session OS thread (`SessionThread` / `spawn_session_on_thread`), and
//! the MCP auto-restart wiring (`SessionRestartActions`).
#![allow(clippy::items_after_test_module)]
use super::*;
use crate::remote::DEFAULT_CONTEXT_WINDOW;
/// Partition CLI `--allow` rules under the pin: blanket catch-all allows
/// (`Allow(Any)` `*` / `**`, plus bare/match-all Bash/MCP/WebFetch grants — see
/// `resolution::is_catchall_allow`) substitute for the blocked `--yolo`, so drop them when
/// `policy_block` is set; keep everything else (and everything without a pin).
/// Pure (no I/O) so the wiring is unit-testable; the caller surfaces `dropped`.
fn drop_cli_catchall_allows(
    rules: Vec<xai_grok_workspace::permission::types::PermissionRule>,
    policy_block: Option<&'static str>,
) -> (
    Vec<xai_grok_workspace::permission::types::PermissionRule>,
    Vec<xai_grok_workspace::permission::types::PermissionRule>,
) {
    if policy_block.is_none() {
        return (rules, Vec::new());
    }
    let mut kept = Vec::with_capacity(rules.len());
    let mut dropped = Vec::new();
    for rule in rules {
        if xai_grok_workspace::permission::resolution::is_catchall_allow(&rule) {
            dropped.push(rule);
        } else {
            kept.push(rule);
        }
    }
    (kept, dropped)
}
#[cfg(test)]
mod cli_catchall_drop_tests {
    use super::drop_cli_catchall_allows;
    use xai_grok_workspace::permission::resolution::YOLO_PIN_REASON_REQUIREMENTS;
    use xai_grok_workspace::permission::rules::parse_permission_rule;
    use xai_grok_workspace::permission::types::{PermissionRule, RuleAction, ToolFilter};
    fn allow(rule: &str) -> PermissionRule {
        parse_permission_rule(rule, RuleAction::Allow).expect("rule parses")
    }
    /// Under the pin, CLI catch-all `--allow` rules (`*`, `**`) are dropped while
    /// a scoped rule (`Bash(touch *)`) survives.
    #[test]
    fn pin_drops_cli_catchalls_keeps_scoped() {
        let rules = vec![allow("*"), allow("Bash(touch *)"), allow("**")];
        let (kept, dropped) = drop_cli_catchall_allows(rules, Some(YOLO_PIN_REASON_REQUIREMENTS));
        assert_eq!(kept.len(), 1, "only the scoped Bash rule survives");
        assert_eq!(kept[0].tool, ToolFilter::Bash);
        assert_eq!(dropped.len(), 2, "both catch-alls are dropped");
    }
    /// Without the pin nothing is dropped, even catch-alls.
    #[test]
    fn no_pin_keeps_everything() {
        let rules = vec![allow("*"), allow("Bash(touch *)"), allow("**")];
        let (kept, dropped) = drop_cli_catchall_allows(rules, None);
        assert_eq!(kept.len(), 3);
        assert!(dropped.is_empty());
    }
    /// FIX 2: a bare `--allow Bash` and a `?*` Bash pattern are `--yolo`
    /// substitutes on the freeform-execution dimension, so the pin drops them
    /// while a scoped `Bash(git *)` survives.
    #[test]
    fn pin_drops_cli_bare_and_prefix_bash_keeps_scoped() {
        let rules = vec![allow("Bash"), allow("Bash(?*)"), allow("Bash(git *)")];
        let (kept, dropped) = drop_cli_catchall_allows(rules, Some(YOLO_PIN_REASON_REQUIREMENTS));
        assert_eq!(kept.len(), 1, "only the scoped Bash rule survives");
        assert_eq!(kept[0].pattern.as_deref(), Some("git *"));
        assert_eq!(dropped.len(), 2, "bare Bash and ?* are dropped");
        let rules = vec![allow("Bash"), allow("Bash(?*)"), allow("Bash(git *)")];
        let (kept, dropped) = drop_cli_catchall_allows(rules, None);
        assert_eq!(kept.len(), 3);
        assert!(dropped.is_empty());
    }
}
/// Spawns a session actor and returns the session handle plus a receiver for permission events.
///
/// The permission events receiver should be used to collect telemetry about permission
/// decisions (YOLO mode, user accept/reject, etc.) for upload to GCS.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "session.spawn",
    skip_all,
    fields(
        session_id = %session_info.id.0,
        client_type = ?client_type,
        start_type = if
        initial_prompt_texts.is_empty(){"new"}else{"resumed"},
    ),
)]
pub(crate) async fn spawn_session_actor(
    session_info: SessionInfo,
    gateway: GatewaySender,
    sampling_config: SamplingConfig,
    credentials: xai_chat_state::Credentials,
    auth_method_id: crate::agent::auth_method::SharedAuthMethodId,
    auth_manager: Option<Arc<AuthManager>>,
    attribution_callback: Option<xai_grok_sampler::SharedAttributionCallback>,
    mut tool_context: ToolContext,
    mcp_servers: Vec<acp::McpServer>,
    initial_client_mcp_servers: Vec<acp::McpServer>,
    mcp_meta_config_map: McpMetaConfigMap,
    parent_mcp_pool: Option<crate::session::mcp_servers::SharedMcpPool>,
    acp_mcp_servers: Vec<crate::session::mcp_servers::AcpServerEntry>,
    support_permission: bool,
    telemetry_enabled: bool,
    auto_update: Option<bool>,
    persistence: PersistenceHandle,
    mut conversation: Vec<ConversationItem>,
    rewind_points_path: Option<std::path::PathBuf>,
    initial_last_compaction: Option<usize>,
    initial_prompt_texts: Vec<String>,
    fs_notify_config: Option<ClientFsConfig>,
    initial_total_tokens: u64,
    mut startup_hints: StartupHints,
    client_type: ClientType,
    auto_compact_threshold_percent: u8,
    system_prompt_label: String,
    compaction_mode: xai_chat_state::CompactionMode,
    compaction_verbatim_input: bool,
    compaction_tool_choice: crate::util::config::CompactionToolChoice,
    two_pass_enabled: bool,
    buffering_settings: Option<BufferingSettings>,
    origin_client: Option<crate::http::OriginClientInfo>,
    codebase_indexes: std::sync::Arc<parking_lot::Mutex<CodebaseIndexManager>>,
    code_nav_enabled: bool,
    fs_watch_caps: fs_watch::FsWatchCapabilities,
    feedback_proxy_url: Option<String>,
    feedback_user_token: Option<String>,
    feedback_alpha_test_key: Option<String>,
    deployment_key: Option<String>,
    client_terminal_capable: bool,
    client_fs_capable: bool,
    gateway_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    agent_definition: AgentDefinition,
    session_default_agent_profile: Option<String>,
    skills_config: SkillsConfig,
    preloaded_skills: Option<Vec<xai_grok_tools::implementations::skills::types::SkillInfo>>,
    compat: CompatConfig,
    incremental_bash_output: bool,
    persisted_signals: Option<crate::session::signals::SessionSignals>,
    persisted_plan_mode: Option<crate::session::plan_mode::PlanModeSnapshot>,
    persisted_goal_mode: Option<crate::session::goal_tracker::GoalOrchestration>,
    persisted_announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    memory_config: Option<crate::config::MemoryConfig>,
    loc_tracking_enabled: bool,
    feedback_flags: crate::session::feedback_manager::FeedbackFlags,
    managed_mcp_handle: crate::session::managed_mcp::ManagedMcpStateHandle,
    managed_mcp_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    managed_mcp_proxy_base_url: String,
    session_model_id: acp::ModelId,
    session_yolo_mode: bool,
    session_auto_mode: bool,
    session_client_identifier: Option<String>,
    inference_idle_timeout_secs: u64,
    max_retries: Option<u32>,
    web_search_sampling_config: Option<xai_grok_sampler::SamplerConfig>,
    web_fetch_config: xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig,
    image_gen_config: xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig,
    video_gen_config: xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig,
    app_builder_deployer_config: xai_grok_tools::implementations::grok_build::deploy_app::AppBuilderDeployerConfig,
    write_file_enabled: bool,
    goal_enabled: bool,
    subagents_enabled: bool,
    ask_user_question_enabled: bool,
    client_hooks: crate::extensions::hooks::ClientHooks,
    prompt_display_cwd: Option<String>,
    subagent_toggle: std::collections::HashMap<String, bool>,
    persona_summaries: Vec<String>,
    prompt_audience: xai_grok_agent::prompt::context::PromptAudience,
    role_instructions: Option<String>,
    persona_instructions: Option<String>,
    disable_web_search: bool,
    backend_tools_enabled: bool,
    respect_gitignore: bool,
    path_not_found_hints: bool,
    tool_params_json: crate::session::agent_rebuild::ResolvedToolParamsJson,
    plugin_registry: Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>,
    plugin_registry_handle: Option<xai_grok_agent::plugins::SharedPluginRegistryHandle>,
    models_manager: crate::agent::models::ModelsManager,
    inherited_permission_handle: Option<xai_grok_workspace::permission::PermissionHandle>,
    api_key_provider: Option<xai_grok_tools::types::SharedApiKeyProvider>,
    image_description_model: String,
    hook_registry_override: Option<std::sync::Arc<xai_grok_hooks::discovery::HookRegistry>>,
    workspace_ops: xai_grok_workspace::WorkspaceOps,
    cli_permission_rules: Vec<xai_grok_workspace::permission::types::PermissionRule>,
    todo_gate: bool,
    remote_settings: Option<crate::util::config::RemoteSettings>,
    laziness_debug_log: Option<std::path::PathBuf>,
    parent_terminal_backend: Option<
        std::sync::Arc<dyn xai_grok_tools::computer::types::TerminalBackend>,
    >,
    parent_scheduler_handle: Option<
        xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerHandle,
    >,
    max_turns: Option<usize>,
    forked_tool_override: Option<Vec<ToolSpec>>,
) -> Result<
    (
        SessionHandle,
        mpsc::UnboundedReceiver<PermissionEvent>,
        String,
        tokio::sync::oneshot::Receiver<()>,
    ),
    xai_grok_agent::AgentBuildError,
> {
    if max_turns == Some(0) {
        return Err(xai_grok_agent::AgentBuildError::InvalidConfig(
            "max_turns must be greater than 0".to_string(),
        ));
    }
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    tracing::info!(
        "Session '{}' created with {} MCP servers",
        session_info.id.0,
        mcp_servers.len()
    );
    let _ = support_permission;
    let owns_permission_manager = inherited_permission_handle.is_none();
    let (permissions, permission_events_rx, deny_read_globs) = if let Some(handle) =
        inherited_permission_handle
    {
        let (_dummy_tx, dummy_rx) = mpsc::unbounded_channel::<PermissionEvent>();
        let deny_read_globs = handle.deny_read_globs();
        (handle, dummy_rx, deny_read_globs)
    } else {
        let web_fetch_allowed_domains = match &web_fetch_config {
            WebFetchConfig::Enabled { params } => params.allowed_domains(),
            WebFetchConfig::Disabled => vec![],
        };
        let mut permission_config =
            xai_grok_workspace::permission::resolution::resolve_permission_config_with_fallback(
                tool_context.cwd.as_path(),
            )
            .await;
        let yolo_pin = xai_grok_workspace::permission::resolution::yolo_disabled_by_policy();
        let (cli_permission_rules, dropped_catchalls) =
            drop_cli_catchall_allows(cli_permission_rules, yolo_pin);
        if let Some(reason) = yolo_pin
            && !dropped_catchalls.is_empty()
        {
            tracing::warn!(
                reason,
                dropped = dropped_catchalls.len(),
                "CLI --allow catch-all ignored: always-approve disabled by managed policy"
            );
            if startup_hints.non_interactive {
                eprintln!("grok: --allow catch-all ignored: {reason}");
            }
        }
        if !cli_permission_rules.is_empty() {
            match &mut permission_config {
                Some(config) => {
                    let mut merged = cli_permission_rules;
                    merged.append(&mut config.rules);
                    config.rules = merged;
                }
                None => {
                    permission_config = Some(
                        xai_grok_workspace::permission::types::PermissionConfig::new(
                            cli_permission_rules,
                        ),
                    );
                }
            }
        }
        let deny_read_globs = permission_config
            .as_ref()
            .map(xai_grok_workspace::permission::resolution::deny_read_globs_from_config)
            .unwrap_or_default();
        let hub_permission = if xai_grok_workspace::permission::hitl_permission_live_enabled() {
            let server = match workspace_ops.workspace_handle() {
                Some(handle) => handle.hub_server_blocking().await,
                None => None,
            };
            let transport = server
                .and_then(|server| {
                    xai_grok_workspace::permission::ToolServerPermissionTransport::from_session_id(
                        server,
                        session_info.id.0.as_ref(),
                    )
                })
                .map(|t| {
                    std::sync::Arc::new(t)
                        as std::sync::Arc<
                            dyn xai_grok_workspace::permission::PermissionHookTransport,
                        >
                });
            if transport.is_none() {
                tracing::debug!(
                    session_id = % session_info.id.0,
                    "hitl permission live enabled but no remote transport available; using local prompt"
                );
            }
            transport
        } else {
            None
        };
        let (permissions, permission_events_rx) =
            xai_grok_workspace::permission::spawn_permission_manager_with_hub(
                session_info.id.clone(),
                gateway.clone(),
                tool_context.cwd.clone(),
                client_type,
                permission_config,
                deny_read_globs.clone(),
                web_fetch_allowed_domains,
                session_yolo_mode,
                session_client_identifier.clone(),
                crate::util::config::remember_tool_approvals_from_disk(),
                hub_permission,
            );
        if crate::util::config::auto_mode_session_active(
            crate::util::config::auto_permission_mode_enabled_from_disk(),
            session_auto_mode,
            session_yolo_mode,
        ) {
            permissions.set_auto_mode(true);
            let turns = build_classifier_turns(&conversation, CLASSIFIER_SPAWN_SEED_TURNS);
            if !turns.is_empty() {
                permissions.set_classifier_transcript(turns);
            }
        }
        (permissions, permission_events_rx, deny_read_globs)
    };
    let initial_prompt_index = conversation
        .iter()
        .filter(|item| matches!(item, ConversationItem::User(_)))
        .count();
    let initial_conversation_len = conversation.len();
    let initial_user_count = initial_prompt_index.saturating_sub(1) as u32;
    let initial_assistant_count = conversation
        .iter()
        .filter(|item| matches!(item, ConversationItem::Assistant(_)))
        .count() as u32;
    let (initial_tool_call_count, initial_tools_used, initial_models_used) =
        if persisted_signals.is_none() {
            let mut tool_call_count: u32 = 0;
            let mut tools_used = std::collections::HashSet::new();
            let mut models_used = std::collections::HashSet::new();
            for item in &conversation {
                if let ConversationItem::Assistant(assistant) = item {
                    tool_call_count += assistant.tool_calls.len() as u32;
                    for tc in &assistant.tool_calls {
                        tools_used.insert(tc.name.clone());
                    }
                    if let Some(model_id) = &assistant.model_id {
                        models_used.insert(model_id.clone());
                    }
                }
            }
            (
                tool_call_count,
                tools_used.into_iter().collect::<Vec<_>>(),
                models_used.into_iter().collect::<Vec<_>>(),
            )
        } else {
            (0, Vec::new(), Vec::new())
        };
    let primary_model_id = sampling_config.model.clone();
    let web_search_config = if disable_web_search {
        xai_grok_tools::implementations::WebSearchConfig::Disabled
    } else if let Some(cfg) = web_search_sampling_config {
        if let Some(api_key) = cfg.api_key {
            xai_grok_tools::implementations::WebSearchConfig::Enabled {
                api_key,
                base_url: cfg.base_url,
                model: cfg.model,
                extra_headers: cfg.extra_headers,
                alpha_test_key: credentials.alpha_test_key.clone(),
            }
        } else {
            tracing::warn!("web_search disabled: resolved config has no API key");
            xai_grok_tools::implementations::WebSearchConfig::Disabled
        }
    } else {
        tracing::warn!("web_search disabled: configured model could not be resolved");
        xai_grok_tools::implementations::WebSearchConfig::Disabled
    };
    let embed_base_url = sampling_config.base_url.clone();
    let embed_api_key = sampling_config.api_key.clone();
    let session_pruning_config: crate::config::PruningConfig = memory_config.as_ref().map_or_else(
        || crate::config::PruningConfig {
            enabled: false,
            ..Default::default()
        },
        |mc| mc.pruning.clone(),
    );
    let context_window_override = std::env::var("GROK_DEBUG_CONTEXT_WINDOW")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .and_then(std::num::NonZeroU64::new);
    let baseline_context_window = std::num::NonZeroU64::new(sampling_config.context_window)
        .unwrap_or_else(|| {
            std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW)
                .expect("DEFAULT_CONTEXT_WINDOW is non-zero")
        });
    if let Some(cw) = context_window_override {
        tracing::warn!(
            override_context_window = cw.get(),
            original_context_window = baseline_context_window.get(),
            "GROK_DEBUG_CONTEXT_WINDOW override active"
        );
    }
    let chat_state_sampling_config = xai_grok_sampling_types::SamplingConfig {
        base_url: sampling_config.base_url.clone(),
        model: sampling_config.model.clone(),
        max_completion_tokens: sampling_config.max_completion_tokens,
        temperature: sampling_config.temperature,
        top_p: sampling_config.top_p,
        api_backend: sampling_config.api_backend.clone(),
        extra_headers: sampling_config.extra_headers.clone(),
        context_window: context_window_override.unwrap_or(baseline_context_window),
        reasoning_effort: sampling_config.reasoning_effort,
        stream_tool_calls: Some(sampling_config.stream_tool_calls),
    };
    let actor_pruning_config = xai_chat_state::PruningConfig {
        enabled: session_pruning_config.enabled,
        keep_last_n_turns: session_pruning_config.keep_last_n_turns,
        soft_trim_threshold: session_pruning_config.soft_trim_threshold,
        soft_trim_head: session_pruning_config.soft_trim_head,
        soft_trim_tail: session_pruning_config.soft_trim_tail,
        hard_clear_age_turns: session_pruning_config.hard_clear_age_turns,
    };
    let (chat_state_event_tx, chat_state_event_rx) = mpsc::unbounded_channel();
    let chat_state_handle = xai_chat_state::ChatStateActor::spawn_with_pruning(
        conversation.clone(),
        chat_state_sampling_config,
        actor_pruning_config,
        Box::new(super::chat_persistence::ChannelChatPersistence::new(
            persistence.tx.clone(),
        )),
        chat_state_event_tx,
        tokio_util::sync::CancellationToken::new(),
    );
    if (!initial_prompt_texts.is_empty()
        || initial_total_tokens > 0
        || initial_last_compaction.is_some())
        && let Some(mut snap) = chat_state_handle.snapshot().await
    {
        snap.prompt_index = initial_prompt_texts.len();
        snap.prompt_texts = initial_prompt_texts;
        if initial_total_tokens > 0 {
            snap.total_tokens = initial_total_tokens;
        }
        snap.last_compaction_prompt_index = initial_last_compaction;
        chat_state_handle.restore_snapshot(snap);
    }
    chat_state_handle.update_credentials(credentials);
    let state = TokioMutex::new(State {
        running_task: None,
        pending_inputs: VecDeque::new(),
        pending_notifications: Vec::new(),
        notifications_suppressed: false,
        rewindable: false,
        nudges_used_this_session: 0,
    });
    let mcp_strategy = match std::env::var("MCP_INIT_STRATEGY") {
        Ok(v) if !v.trim().is_empty() => McpInitStrategy::from(v),
        _ if startup_hints.non_interactive => McpInitStrategy::Blocking,
        _ => McpInitStrategy::Progressive,
    };
    let file_state_tracker = Arc::new(match rewind_points_path {
        Some(path) => FileStateTracker::with_lazy_source(path),
        None => FileStateTracker::new(),
    });
    let file_state_handle = FileStateHandle::new(file_state_tracker.clone());
    let task_completion_reservations =
        xai_grok_tools::reminders::task_completion::TaskCompletionReservations::default();
    let task_wake_suppressed =
        xai_grok_tools::reminders::task_completion::TaskWakeSuppressed::default();
    tool_context.task_completion_reservations = Some(task_completion_reservations.clone());
    tool_context.task_wake_suppressed = Some(task_wake_suppressed.clone());
    let synthetic_trace_tx_shared: std::sync::Arc<
        std::sync::Mutex<
            Option<
                tokio::sync::mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>,
            >,
        >,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    *synthetic_trace_tx_shared.lock().unwrap() = tool_context.synthetic_trace_tx.clone();
    tool_context.synthetic_trace_tx_shared = Some(synthetic_trace_tx_shared.clone());
    let mut tool_context = tool_context.with_file_state_handle(file_state_handle);
    let index_root_for_session =
        xai_grok_workspace::session::git::find_git_root_from_path(tool_context.cwd.as_path())
            .unwrap_or_else(|_| tool_context.cwd.to_path_buf());
    let chat_state_handle_for_handle = chat_state_handle.clone();
    let hunk_tracker_handle_for_bridge = tool_context.hunk_tracker_handle.clone();
    let hunk_tracker_handle = tool_context.hunk_tracker_handle.clone();
    let prompt_index_for_bridge = tool_context.prompt_index.clone();
    let plan_mode = {
        let session_dir = crate::session::persistence::session_dir(&session_info);
        let tracker = if let Some(snapshot) = persisted_plan_mode {
            crate::session::plan_mode::PlanModeTracker::from_snapshot(session_dir, snapshot)
        } else {
            crate::session::plan_mode::PlanModeTracker::new(session_dir)
        };
        Arc::new(parking_lot::Mutex::new(tracker))
    };
    let goal_tracker = {
        let session_dir = crate::session::persistence::session_dir(&session_info);
        let tracker = if let Some(snapshot) = persisted_goal_mode {
            crate::session::goal_tracker::GoalTracker::from_snapshot(session_dir, snapshot)
        } else {
            crate::session::goal_tracker::GoalTracker::new(session_dir)
        };
        Arc::new(parking_lot::Mutex::new(tracker))
    };
    let current_prompt_mode = Arc::new(parking_lot::Mutex::new(PromptMode::Agent));
    let turn_prompt_mode = Arc::new(parking_lot::Mutex::new(PromptMode::Agent));
    let task_output_tool_name = Arc::new(std::sync::OnceLock::new());
    let read_tool_name = Arc::new(std::sync::OnceLock::new());
    let queue_exit_reminder_on_approved_exit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tools_notification_handle = crate::tools::notification_bridge::spawn_notification_bridge(
        crate::tools::notification_bridge::NotificationBridgeConfig {
            gateway: gateway.clone(),
            session_id: session_info.id.clone(),
            hunk_tracker_handle: hunk_tracker_handle_for_bridge.clone(),
            file_state_tracker: file_state_tracker.clone(),
            prompt_index: prompt_index_for_bridge,
            cwd: tool_context.cwd.as_path().to_path_buf(),
            gateway_enabled: gateway_enabled.clone(),
            persistence_tx: persistence.tx.clone(),
            incremental_bash_output,
            plan_mode: plan_mode.clone(),
            current_prompt_mode: current_prompt_mode.clone(),
            turn_prompt_mode: turn_prompt_mode.clone(),
            session_cmd_tx: cmd_tx.clone(),
            task_completion_reservations: task_completion_reservations.clone(),
            task_wake_suppressed: task_wake_suppressed.clone(),
            synthetic_trace_tx: synthetic_trace_tx_shared.clone(),
            task_output_tool_name: task_output_tool_name.clone(),
            read_tool_name: read_tool_name.clone(),
            auto_wake_enabled: tool_context.auto_wake_enabled,
            queue_exit_reminder_on_approved_exit: queue_exit_reminder_on_approved_exit.clone(),
            goal_loop_active: tool_context.goal_loop_active_gate.clone(),
        },
    );
    let tool_context_for_handle = tool_context.clone();
    let resolve_search_shadows = || {
        let user_cfg = crate::config::load_effective_config().ok();
        let requirements = crate::config::load_merged_requirements();
        let (find_bfs, grep_ugrep) = crate::util::config::resolve_search_tools_enabled(
            requirements.as_ref(),
            user_cfg.as_ref(),
            None,
        );
        xai_grok_tools::computer::local::SearchShadowConfig {
            find_bfs,
            grep_ugrep,
        }
    };
    let cursor_harness = false;
    let terminal_backend_kind = select_terminal_backend_kind(
        startup_hints.is_subagent,
        parent_terminal_backend.is_some(),
        client_terminal_capable,
        tool_context.gateway.is_some(),
        cursor_harness,
    );
    let terminal_backend: std::sync::Arc<dyn xai_grok_tools::computer::types::TerminalBackend> =
        match terminal_backend_kind {
            TerminalBackendKind::ReuseParent => parent_terminal_backend
                .expect("ReuseParent is only selected when a parent backend is present"),
            TerminalBackendKind::AcpClient => {
                std::sync::Arc::new(crate::terminal::AcpTerminalAdapter::new(
                    tool_context.gateway.clone().unwrap(),
                    tool_context.session_id.clone().unwrap(),
                ))
                    as std::sync::Arc<dyn xai_grok_tools::computer::types::TerminalBackend>
            }
            TerminalBackendKind::LocalPersistent => std::sync::Arc::new(
                LocalTerminalBackend::new_local_with_persistent_shell(resolve_search_shadows()),
            ),
            TerminalBackendKind::LocalNonPersistent => {
                let login_shell_capture = crate::util::config::resolve_login_shell_capture(
                    remote_settings.as_ref().and_then(|r| r.login_shell_capture),
                );
                std::sync::Arc::new(LocalTerminalBackend::new_local_with_login_shell_capture(
                    resolve_search_shadows(),
                    login_shell_capture,
                ))
            }
        };
    if matches!(
        terminal_backend_kind,
        TerminalBackendKind::LocalPersistent | TerminalBackendKind::LocalNonPersistent
    ) {
        terminal_backend
            .warm_shell(tool_context.cwd.as_path())
            .await;
    }
    let fs_backend: std::sync::Arc<dyn xai_grok_tools::computer::types::AsyncFileSystem> =
        if client_fs_capable && tool_context.gateway.is_some() {
            std::sync::Arc::new(xai_grok_workspace::file_system::AcpFsAdapter::new(
                tool_context.gateway.clone().unwrap(),
                tool_context.session_id.clone().unwrap(),
            ))
        } else {
            std::sync::Arc::new(xai_grok_tools::computer::local::LocalFs)
        };
    let bridge_state_path =
        crate::session::persistence::session_dir(&session_info).join("tool_state.json");
    let initial_agent_type = Some(agent_definition.name.clone());
    let harness_metrics = if telemetry_enabled || xai_grok_telemetry::external::is_active() {
        let plugin_names = plugin_registry
            .as_ref()
            .map(|reg| {
                reg.active_plugins()
                    .iter()
                    .map(|p| p.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        Some(super::telemetry::SessionHarnessMetrics {
            session_id: session_info.id.0.to_string(),
            client_identifier: session_client_identifier.clone(),
            model_id: session_model_id.0.to_string(),
            agent_name: agent_definition.name.clone(),
            permission_mode: if session_yolo_mode {
                xai_grok_telemetry::enums::PermissionMode::AlwaysApprove
            } else if session_auto_mode
                && crate::util::config::auto_permission_mode_enabled_from_disk()
            {
                xai_grok_telemetry::enums::PermissionMode::Auto
            } else {
                xai_grok_telemetry::enums::PermissionMode::Ask
            },
            mcp_server_names: mcp_servers
                .iter()
                .map(|s| mcp_server_name(s).to_owned())
                .collect(),
            lsp_server_names: tool_context.lsp_server_names.clone(),
            memory_enabled: memory_config.is_some(),
            auto_update,
            cwd: tool_context.cwd.as_str().to_owned(),
            skills_config: skills_config.clone(),
            compat,
            plugin_registry: plugin_registry.clone(),
            plugin_names,
        })
    } else {
        None
    };
    let compaction_policy = xai_grok_agent::CompactionPolicy {
        auto_compact_threshold_percent: auto_compact_threshold_percent as u32,
        compact_model: None,
        memory_flush_enabled: memory_config.as_ref().is_some_and(|mc| mc.flush.enabled),
        wall_clock_budget_secs: crate::util::config::resolve_compaction_wall_clock_budget_secs(
            remote_settings
                .as_ref()
                .and_then(|r| r.compaction_wall_clock_budget_secs),
        ),
        two_pass_enabled,
    };
    let reminder_policy = resolve_reminder_policy(remote_settings.as_ref(), todo_gate);
    let (user_question_tx, user_question_rx) = tokio::sync::mpsc::unbounded_channel::<
        xai_grok_tools::implementations::grok_build::ask_user_question::types::UserQuestionRequest,
    >();
    let attribution_callback_for_spec = auth_manager.as_ref().map(|am| {
        crate::auth::attribution::ShellAttribution::new_tool_callback(
            am.clone(),
            Some(session_info.id.0.to_string()),
        )
    });
    let memory_storage_for_session = memory_config.as_ref().filter(|mc| mc.enabled).map(|mc| {
        if mc.flat_memory_root
            && let Some(ref root) = mc.root_dir_override
        {
            return crate::session::memory::MemoryStorage::new_flat(
                tool_context.cwd.as_path(),
                root,
            );
        }
        crate::session::memory::MemoryStorage::new(
            tool_context.cwd.as_path(),
            mc.root_dir_override.as_deref(),
        )
    });
    let memory_initial_injection_config = memory_config
        .as_ref()
        .map_or_else(Default::default, |mc| mc.initial_injection.clone());
    let mut memory_backend_params_for_session: Option<crate::session::memory::MemoryBackendParams> =
        None;
    let mut memory_search_counter: Option<std::sync::Arc<std::sync::atomic::AtomicU64>> = None;
    let memory_backend_for_spec: Option<
        std::sync::Arc<dyn xai_grok_tools::types::memory_backend::MemoryBackend>,
    > = if let Some(ref storage) = memory_storage_for_session {
        if let Err(e) = storage.ensure_initialized() {
            tracing::warn!(
                target : xai_grok_telemetry::memory_log::TARGET, error = % e,
                "MEMORY_INIT: ensure_initialized failed, continuing without template files"
            );
        }
        {
            let gc_storage = storage.clone();
            let gc_max_age = memory_config.as_ref().map_or(30, |mc| mc.gc.max_age_days);
            tokio::task::spawn_blocking(move || match gc_storage.gc(gc_max_age) {
                Ok(removed) if removed > 0 => {
                    tracing::info!(
                        target : xai_grok_telemetry::memory_log::TARGET, removed,
                        "MEMORY_GC: cleaned orphaned workspace directories"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        target : xai_grok_telemetry::memory_log::TARGET, error = % e,
                        "MEMORY_GC: failed"
                    );
                }
                _ => {}
            });
        }
        let watcher_config = memory_config
            .as_ref()
            .map(|mc| &mc.watcher)
            .cloned()
            .unwrap_or_default();
        let watcher = if watcher_config.enabled {
            crate::session::memory::watcher::MemoryFileWatcher::start(storage.global_dir())
                .map(std::sync::Arc::new)
        } else {
            None
        };
        let embed_credentials = crate::auth::credential_provider::embedding_session_credentials(
            &embed_base_url,
            auth_manager.as_ref(),
            api_key_provider.clone(),
        );
        let params = crate::session::memory::MemoryBackendParams {
            session_id: session_info.id.to_string(),
            embed_config: memory_config.as_ref().map(|mc| mc.embedding.clone()),
            embed_base_url: embed_base_url.clone(),
            embed_api_key: embed_api_key.clone(),
            search_config: memory_config
                .as_ref()
                .map_or_else(Default::default, |mc| mc.search.clone()),
            watcher,
            stale_claim_secs: watcher_config.stale_claim_secs,
            search_source: "tool",
            embedding_credentials: embed_credentials,
        };
        let backend = crate::session::memory::MemoryBackendImpl::from_session_params(
            storage.clone(),
            &params,
        );
        memory_search_counter = Some(backend.search_counter.clone());
        let watcher_started = params.watcher.is_some();
        let backend: std::sync::Arc<dyn xai_grok_tools::types::memory_backend::MemoryBackend> =
            std::sync::Arc::new(backend);
        memory_backend_params_for_session = Some(params);
        if watcher_config.enabled && !watcher_started {
            tracing::warn!(
                target : xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_INIT: watcher was configured but failed to start \
                 (directory may not exist or OS watcher unavailable)"
            );
        }
        tracing::info!(
            target : xai_grok_telemetry::memory_log::TARGET, workspace = % storage
            .workspace_dir().display(), global = % storage.global_dir().display(),
            watcher_config_enabled = watcher_config.enabled, watcher_started,
            "MEMORY_INIT: storage + backend created"
        );
        let mc = memory_config.as_ref();
        let total_chunks = storage.total_chunk_count();
        xai_grok_telemetry::session_ctx::log_event(
            xai_grok_telemetry::memory_telemetry::MemorySessionInit {
                session_id: session_info.id.to_string(),
                memory_enabled: true,
                watcher_config_enabled: watcher_config.enabled,
                watcher_started,
                temporal_decay_enabled: mc.is_none_or(|c| c.search.temporal_decay.enabled),
                mmr_enabled: mc.is_some_and(|c| c.search.mmr.enabled),
                mmr_lambda: mc.map_or(0.7, |c| c.search.mmr.lambda),
                half_life_days: mc.map_or(30.0, |c| c.search.temporal_decay.half_life_days),
                embedding_dimensions: mc.map_or(1024, |c| c.embedding.dimensions),
                total_chunks,
                total_files: storage.list_memory_files().map_or(0, |f| f.len()),
                has_global_memory_md: storage.global_memory_file().exists(),
                has_workspace_memory_md: storage.workspace_memory_file().exists(),
            },
        );
        Some(backend)
    } else {
        tracing::debug!(
            target : xai_grok_telemetry::memory_log::TARGET,
            "MEMORY_INIT: memory disabled, no storage created"
        );
        None
    };
    let context_window_tokens = context_window_override
        .map(|c| c.get())
        .unwrap_or(sampling_config.context_window);
    let managed_gateway_tool_client = auth_manager.as_ref().map(|am| {
        xai_grok_tools::types::resources::ManagedGatewayToolClient(Arc::new(
            ShellManagedGatewayToolClient {
                proxy_base_url: managed_mcp_proxy_base_url.clone(),
                auth_manager: am.clone(),
            },
        ))
    });
    let mcp_state = {
        let mut state = McpState::new_with_meta(mcp_servers.clone(), mcp_meta_config_map);
        if let Some(ref pool) = parent_mcp_pool {
            state.import_shared_clients(pool);
            tracing::info!(
                session_id = % session_info.id.0, shared_clients = state.shared_clients
                .len(), "Imported shared MCP clients from parent pool"
            );
        }
        if !acp_mcp_servers.is_empty() {
            let invoker = std::sync::Arc::new(crate::session::acp_mcp::GatewayAcpInvoker::new(
                gateway.clone(),
            ));
            let acp_server_count = acp_mcp_servers.len();
            state.set_acp_servers(acp_mcp_servers, invoker);
            tracing::info!(
                session_id = % session_info.id.0, acp_mcp_servers = acp_server_count,
                "Registered in-process SDK MCP servers (x.ai/mcp/sdk_call)"
            );
        }
        Arc::new(TokioMutex::new(state))
    };
    let rebuild_spec = std::sync::Arc::new(crate::session::agent_rebuild::AgentRebuildSpec {
        working_directory: tool_context.cwd.as_path().to_path_buf(),
        terminal_backend: terminal_backend.clone(),
        fs_backend: fs_backend.clone(),
        tools_notification_handle: tools_notification_handle.clone(),
        bridge_state_path: bridge_state_path.clone(),
        session_env: tool_context.session_env.clone(),
        models_manager: models_manager.clone(),
        compaction_policy,
        reminder_policy,
        memory_enabled: memory_config.as_ref().is_some_and(|mc| mc.enabled),
        memory_global_path: memory_storage_for_session
            .as_ref()
            .map(|s| s.global_memory_file().to_string_lossy().into_owned()),
        memory_workspace_path: memory_storage_for_session
            .as_ref()
            .map(|s| s.workspace_memory_file().to_string_lossy().into_owned()),
        memory_backend: memory_backend_for_spec,
        web_search_config: web_search_config.clone(),
        backend_search: backend_tools_enabled,
        web_fetch_config: web_fetch_config.clone(),
        image_gen_config: image_gen_config.clone(),
        video_gen_config: video_gen_config.clone(),
        app_builder_deployer_config: app_builder_deployer_config.clone(),
        write_file_enabled,
        subagents_enabled,
        subagent_toggle: subagent_toggle.clone(),
        ask_user_question_enabled,
        persona_summaries: persona_summaries.clone(),
        prompt_audience,
        role_instructions: role_instructions.clone(),
        persona_instructions: persona_instructions.clone(),
        skills_config: skills_config.clone(),
        compat,
        context_window_tokens,
        prompt_working_directory: prompt_display_cwd.clone(),
        lsp: tool_context.lsp.clone(),
        plugin_registry: plugin_registry.clone(),
        api_key_provider: api_key_provider.clone(),
        attribution_callback: attribution_callback_for_spec,
        tool_params_json: tool_params_json.clone(),
        subagent_event_tx: tool_context.subagent_event_tx.clone(),
        monitor_event_buffer: tool_context.monitor_event_buffer.clone(),
        user_question_tx: user_question_tx.clone(),
        subagent_depth: tool_context.subagent_depth,
        session_id_str: session_info.id.0.to_string(),
        respect_gitignore,
        path_not_found_hints,
        scheduler_background_loops: crate::util::config::resolve_scheduler_background_loops(
            remote_settings
                .as_ref()
                .and_then(|r| r.scheduler_background_loops),
        ),
        mcp_state: mcp_state.clone(),
        managed_gateway_tool_client: managed_gateway_tool_client.clone(),
        is_non_interactive: startup_hints.non_interactive,
        system_prompt_label,
        owner_session_id: Some(session_info.id.0.to_string()),
        parent_scheduler_handle: if startup_hints.is_subagent {
            parent_scheduler_handle
        } else {
            None
        },
    });
    let agent = rebuild_spec
        .build_agent_with_initial_overrides(
            agent_definition,
            persisted_announcement_state
                .as_ref()
                .filter(|s| !s.announced_skill_names.is_empty())
                .map(|s| s.announced_skill_names.clone()),
            preloaded_skills,
        )
        .await
        .map_err(|e| {
            tracing::error!(
                session_id = % session_info.id.0, error = % e,
                "Agent building failed, please check your config"
            );
            e
        })?;
    agent
        .tool_bridge()
        .update_resource(task_completion_reservations.clone())
        .await;
    agent
        .tool_bridge()
        .update_resource(task_wake_suppressed)
        .await;
    let resolved_task_output =
        xai_grok_tools::reminders::task_completion::resolve_task_output_tool_name(
            agent.tool_bridge(),
        )
        .await;
    let resolved_read =
        xai_grok_tools::reminders::task_completion::resolve_read_tool_name(agent.tool_bridge())
            .await;
    let _ = task_output_tool_name.set(resolved_task_output.clone());
    let _ = read_tool_name.set(resolved_read);
    tool_context.task_output_tool_name = resolved_task_output.unwrap_or_else(|| {
        xai_grok_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL.to_string()
    });
    let scheduler_handle_for_handle = {
        let toolset = agent.tool_bridge().toolset();
        let res = toolset.resources.lock().await;
        res.get::<xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerHandle>()
            .cloned()
    };
    if let Err(e) = workspace_ops.bind_local_session(
        &session_info.id.0,
        tool_context.cwd.as_path().to_path_buf(),
        tool_context.hunk_tracker_handle.clone(),
        agent.tool_bridge().toolset(),
        None,
    ) {
        tracing::warn!(error = % e, "failed to bind local session toolset");
    }
    let system_prompt = agent.system_prompt().to_string();
    let mut prompt_context = agent.prompt_context().clone();
    prompt_context.normalize_for_persistence();
    save_prompt_context(&session_info, &prompt_context);
    let is_subagent_spawn = startup_hints.is_subagent;
    install_system_prompt(
        &mut conversation,
        &mut startup_hints.inherited_prefix_len,
        is_subagent_spawn,
        startup_hints.preserve_inherited_system,
        &system_prompt,
    );
    if !startup_hints.preserve_inherited_system
        && !conversation_has_project_instructions(&conversation)
        && let Some(agents_md_reminder) = agent.agents_md_user_reminder()
    {
        let insert_at = conversation.len().min(1);
        conversation.insert(
            insert_at,
            ConversationItem::project_instructions(agents_md_reminder),
        );
        if let Some(ref mut len) = startup_hints.inherited_prefix_len {
            *len += 1;
        }
    }
    if let Some(section) = agent.agents_md_section()
        && should_set_classifier_project_instructions(
            owns_permission_manager,
            Some(section.as_str()),
        )
    {
        let body = agents_md_classifier_body(&section);
        if !body.is_empty() {
            permissions.set_project_instructions(Some(body));
        }
    }
    if let Some(ConversationItem::System(sys)) = conversation.first() {
        save_system_prompt(&session_info, &sys.content);
    } else {
        save_system_prompt(&session_info, &system_prompt);
    }
    persist_chat_history_jsonl_sync(&session_info, &conversation);
    chat_state_handle.replace_conversation(conversation);
    let feedback_client = feedback_proxy_url.map(|base_url| {
        let mut client =
            crate::agent::feedback_client::FeedbackClient::new(base_url, feedback_user_token)
                .with_alpha_test_key(feedback_alpha_test_key)
                .with_deployment_key(deployment_key);
        if let Some(am) = auth_manager.as_ref() {
            client = client.with_auth_manager(am.clone());
        }
        client
    });
    let has_feedback_client = feedback_client.is_some();
    tracing::info!(
        session_id = % session_info.id.0, has_feedback_client = has_feedback_client,
        "Creating feedback manager"
    );
    let feedback_client_type = match client_type {
        ClientType::GrokTUI => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Tui,
        ClientType::GrokWeb => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Web,
        ClientType::Nebula => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Nebula,
        ClientType::Extension => {
            prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Extension
        }
        ClientType::Generic => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Agent,
        ClientType::Desktop => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Desktop,
        ClientType::GrokPager => prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Tui,
    };
    let feedback_config = FeedbackManagerConfig {
        feedback_enabled: feedback_flags.enabled,
        telemetry_enabled,
        client_type: feedback_client_type,
        loc_tracking_enabled,
        ..Default::default()
    };
    let feedback_manager = Arc::new(FeedbackManager::new(
        session_info.id.0.to_string(),
        feedback_client,
        feedback_config,
    ));
    let signals_handle = feedback_manager.signals_handle();
    if let Some(persisted) = persisted_signals {
        signals_handle.restore_signals(persisted);
    } else {
        signals_handle.seed_counts(
            initial_user_count,
            initial_assistant_count,
            initial_tool_call_count,
            initial_tools_used,
            initial_models_used,
        );
    }
    signals_handle.set_primary_model(&primary_model_id);
    signals_handle.set_tracing_config(inference_idle_timeout_secs);
    let sync_loop_cancel = if has_feedback_client {
        Some(tokio_util::sync::CancellationToken::new())
    } else {
        None
    };
    let force_compact = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resolved_workspace_root = xai_grok_workspace::session::git::find_git_root_from_path(
        std::path::Path::new(&session_info.cwd),
    )
    .ok()
    .map(|p| p.to_string_lossy().to_string())
    .unwrap_or_else(|| session_info.cwd.clone());
    let current_prompt_id = std::sync::Arc::new(std::sync::Mutex::new(None));
    let pending_interactions: crate::session::pending_interaction::PendingInteractions =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let permissions_for_handle = permissions.clone();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let mut sampler_config_initial = sampling_config.clone();
    sampler_config_initial.idle_timeout_secs = Some(inference_idle_timeout_secs);
    let sampler_retry_policy = xai_grok_sampler::RetryPolicy {
        max_retries: max_retries.unwrap_or(5),
        rate_limit_retry_threshold: 2,
    };
    let (sampler_event_tx, sampler_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_grok_sampler::SamplingEvent>();
    let sampler_handle = xai_grok_sampler::SamplerActor::spawn(
        sampler_config_initial,
        sampler_retry_policy,
        sampler_event_tx,
    );
    let attribution_callback_for_handle = attribution_callback.clone();
    let agent_name_for_handle = initial_agent_type
        .as_deref()
        .unwrap_or(crate::agent::config::DEFAULT_AGENT_TYPE)
        .to_owned();
    let allowed_subagent_types_for_handle = agent.definition().allowed_subagent_types.clone();
    let mut hook_discovery_errors: Vec<xai_grok_hooks::error::HookError> = Vec::new();
    let built_hook_registry: Option<Arc<xai_grok_hooks::discovery::HookRegistry>> =
        if let Some(override_reg) = hook_registry_override {
            Some(override_reg)
        } else {
            let cwd_path = std::path::Path::new(&session_info.cwd);
            let project_trusted = crate::agent::folder_trust::resolve_and_record(
                cwd_path,
                remote_settings.as_ref(),
                false,
            );
            let git_root = xai_grok_workspace::session::git::find_git_root_from_path(cwd_path).ok();
            let (registry, errors) = crate::util::hooks::discover_hooks(
                git_root.as_deref(),
                &rebuild_spec.compat,
                project_trusted,
            );
            for e in &errors {
                tracing::warn!(error = ? e, "hook loading error");
            }
            hook_discovery_errors = errors;
            if registry.is_empty() {
                None
            } else {
                tracing::info!(hook_count = registry.len(), "loaded hooks");
                Some(Arc::new(registry))
            }
        };
    let hook_registry_for_handle = built_hook_registry.clone();
    let workspace_ops_for_handle = workspace_ops.clone();
    #[allow(clippy::arc_with_non_send_sync)]
    let mut _hook_load_errors: Vec<String> = hook_discovery_errors
        .iter()
        .map(|e| e.to_string())
        .collect();
    let upload_queue = Arc::new(std::sync::OnceLock::new());
    let (goal_update_tx, goal_update_rx) = tokio::sync::mpsc::unbounded_channel::<
        xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalEnvelope,
    >();
    let obs_bridge = {
        let sid = xai_tool_protocol::SessionId::new(&*session_info.id.0)
            .unwrap_or_else(|_| xai_tool_protocol::SessionId::new("unknown").expect("valid"));
        xai_computer_hub_sdk::ObservabilityBridge::new(None, sid)
    };
    let mut effective_config = crate::config::load_effective_config()
        .ok()
        .and_then(|raw| crate::agent::config::Config::new_from_toml_cfg(&raw).ok())
        .unwrap_or_default();
    effective_config.remote_settings = remote_settings.clone();
    let goal_classifier_max_runs = effective_config.resolve_goal_classifier_max_runs().value;
    let goal_strategist_every = effective_config
        .resolve_goal_strategist_every(goal_classifier_max_runs)
        .value;
    let goal_reverify_after = effective_config.resolve_goal_reverify_after().value;
    let goal_use_current_model_only = effective_config.resolve_goal_use_current_model_only().value;
    let goal_role_models = {
        let planner = effective_config
            .resolve_goal_planner_model(goal_use_current_model_only)
            .value;
        let strategist = effective_config
            .resolve_goal_strategist_model(goal_use_current_model_only)
            .value;
        let skeptic_pool = effective_config
            .resolve_goal_skeptic_models(goal_use_current_model_only)
            .value
            .into_iter()
            .filter_map(|c| match c {
                crate::agent::config::GoalRoleModelChoice::Explicit(p) => Some(p),
                crate::agent::config::GoalRoleModelChoice::InheritCurrent => None,
            })
            .collect();
        GoalRoleModelConfig {
            planner,
            strategist,
            skeptic_pool,
        }
    };
    let doom_loop_recovery = effective_config.resolve_doom_loop_recovery();
    let session = Arc::new_cyclic(|weak: &std::sync::Weak<SessionActor>| SessionActor {
        session_info: session_info.clone(),
        auth_method_id,
        model_auth_memo: std::cell::RefCell::new(None),
        attribution_callback,
        auth_manager,
        state,
        notifications: NotificationSender {
            gateway: gateway.clone(),
            gateway_enabled: gateway_enabled.clone(),
            persistence_tx: persistence.tx.clone(),
        },
        permissions,
        tool_context,
        deny_read_globs,
        mcp_state: mcp_state.clone(),
        mcp_strategy,
        initial_client_mcp_servers: initial_client_mcp_servers.clone(),
        chat_state_handle,
        current_prompt_id: current_prompt_id.clone(),
        pending_interactions: pending_interactions.clone(),
        telemetry_enabled,
        supports_backend_search: std::cell::Cell::new(sampling_config.supports_backend_search),
        compactions_remaining: std::cell::Cell::new(sampling_config.compactions_remaining),
        compaction_at_tokens: std::cell::Cell::new(sampling_config.compaction_at_tokens),
        doom_loop_recovery,
        doom_loop_turn_tally: Default::default(),
        file_state_tracker,
        rewind_pending_prompt: std::sync::Mutex::new(None),
        startup_hints,
        forked_tool_override,
        compaction: super::compaction_config::CompactionConfig {
            threshold_percent: std::cell::Cell::new(auto_compact_threshold_percent),
            force_compact: force_compact.clone(),
            context_window_override,
            count: std::sync::atomic::AtomicU64::new(0),
            auto_compact_suppressed: std::sync::atomic::AtomicU8::new(0),
            previous_model: std::cell::Cell::new(None),
            compaction_mode,
            verbatim_input: compaction_verbatim_input,
            tool_choice: compaction_tool_choice,
            prefire: crate::session::compaction_config::PrefireState::default(),
            prefix_released: std::sync::atomic::AtomicBool::new(false),
        },
        memory: super::memory_state::SessionMemory {
            flush_config: memory_config.as_ref().map_or_else(
                || crate::config::MemoryFlushConfig {
                    enabled: false,
                    ..Default::default()
                },
                |mc| mc.flush.clone(),
            ),
            is_flushing: std::sync::atomic::AtomicBool::new(false),
            last_flush_compaction: std::sync::atomic::AtomicU64::new(0),
            storage: std::cell::RefCell::new(memory_storage_for_session),
            save_on_end: memory_config
                .as_ref()
                .is_none_or(|mc| mc.session.save_on_end),
            backend_params: memory_backend_params_for_session,
            initial_injection_config: memory_initial_injection_config,
            context_injected: std::sync::atomic::AtomicBool::new(false),
            flush_count: std::sync::atomic::AtomicU64::new(0),
            last_flush_content: std::cell::RefCell::new(None),
            flush_success_count: std::sync::atomic::AtomicU64::new(0),
            flush_error_count: std::sync::atomic::AtomicU64::new(0),
            search_counter: std::cell::RefCell::new(memory_search_counter),
            injection_count: std::sync::atomic::AtomicU64::new(0),
            compaction_recovery_count: std::sync::atomic::AtomicU64::new(0),
            chunks_added: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dream_config: memory_config
                .as_ref()
                .map_or_else(Default::default, |mc| mc.dream),
            dream_count: std::sync::atomic::AtomicU64::new(0),
            dream_success_count: std::sync::atomic::AtomicU64::new(0),
            dream_error_count: std::sync::atomic::AtomicU64::new(0),
        },
        session_start: std::time::Instant::now(),
        inference_idle_timeout: Duration::from_secs(inference_idle_timeout_secs),
        max_turns,
        max_retries: xai_grok_sampler::resolve_max_retries(max_retries),
        pending_interjections: InterjectionBuffer::new(),
        pending_skill_reminders: Mutex::new(Vec::new()),
        idle_flush_timeout: memory_config
            .as_ref()
            .and_then(|mc| mc.flush.idle_timeout_secs)
            .map(std::time::Duration::from_secs),
        dream_check_timeout: memory_config
            .as_ref()
            .filter(|mc| mc.dream.enabled)
            .and_then(|mc| mc.dream.check_interval_secs)
            .filter(|&s| s > 0)
            .map(std::time::Duration::from_secs),
        last_idle_flush_conversation_len: std::sync::atomic::AtomicUsize::new(
            initial_conversation_len,
        ),
        event_tx,
        buffering_settings,
        client_identifier: session_client_identifier.clone(),
        origin_client: origin_client.clone(),
        feedback_manager: feedback_manager.clone(),
        upload_queue: upload_queue.clone(),
        sync_loop_cancel: sync_loop_cancel.clone(),
        agent: std::cell::RefCell::new(agent),
        last_reported_branch: Arc::new(Mutex::new(None)),
        git_head_enabled: fs_watch_caps.git_head,
        models_manager,
        display_cwd: {
            let lock = std::sync::OnceLock::new();
            if let Some(ref cwd) = prompt_display_cwd {
                let _ = lock.set(cwd.clone());
            }
            lock
        },
        active_agent_type: parking_lot::Mutex::new(initial_agent_type),
        queue_exit_reminder_on_approved_exit,
        active_skill: parking_lot::Mutex::new(None),
        current_prompt_mode: current_prompt_mode.clone(),
        turn_start_prompt_mode: parking_lot::Mutex::new(PromptMode::Agent),
        turn_prompt_mode: turn_prompt_mode.clone(),
        plan_mode: plan_mode.clone(),
        goal_enabled,
        goal_harness_enabled: std::sync::atomic::AtomicBool::new(false),
        goal_harness_availability_reconciled: std::sync::atomic::AtomicBool::new(false),
        goal_tracker,
        goal_turn_task_ids: parking_lot::Mutex::new(std::collections::HashSet::new()),
        goal_continuation_streak: std::sync::atomic::AtomicU32::new(0),
        goal_blocked_streak: std::sync::atomic::AtomicU32::new(0),
        goal_update_rx: std::cell::RefCell::new(Some(goal_update_rx)),
        goal_update_tx,
        goal_classifier_enabled: effective_config
            .resolve_goal_classifier_enabled(goal_enabled)
            .value,
        goal_planner_enabled: effective_config
            .resolve_goal_planner_enabled(goal_enabled)
            .value,
        goal_summary_enabled: effective_config
            .resolve_goal_summary_enabled(goal_enabled)
            .value,
        goal_verifier_skeptic_count: effective_config.resolve_goal_verifier_count().value,
        goal_role_models,
        goal_use_current_model_only,
        goal_classifier_max_runs,
        goal_strategist_every,
        goal_reverify_after,
        goal_plan_reconciled: std::sync::atomic::AtomicBool::new(false),
        pending_classifier_completions: parking_lot::Mutex::new(VecDeque::new()),
        goal_classifier_in_flight: std::sync::atomic::AtomicBool::new(false),
        managed_mcp_handle,
        managed_mcp_expires_at: std::sync::Mutex::new(managed_mcp_expires_at),
        tool_metadata_snapshot: Arc::new(std::sync::Mutex::new(Default::default())),
        mcp_announced_servers: Mutex::new(
            persisted_announcement_state
                .as_ref()
                .map(|s| {
                    crate::session::announcement_state::from_persisted_fingerprints(
                        &s.mcp_server_fingerprints,
                    )
                })
                .unwrap_or_default(),
        ),
        mcp_reminder_mode: McpReminderMode::from_env(),
        mcp_reminder_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        mcp_connecting_reminder_injected: std::cell::Cell::new(false),
        mcp_handshakes_done: Arc::new(tokio::sync::Notify::new()),
        user_input_generation: std::sync::atomic::AtomicU64::new(0),
        laziness_debug_log: laziness_debug_log.map(|p| std::sync::Arc::from(p.as_path())),
        deferred_prefix: TaskSlot::new(),
        extension_registry: session_extension_registry(weak.clone()),
        last_announced_local_date: std::cell::Cell::new(chrono::Local::now().date_naive()),
        last_search_prompt_index: std::sync::atomic::AtomicI64::new(-1),
        last_api_request_at: std::sync::atomic::AtomicI64::new(0),
        hook_registry: std::cell::RefCell::new(built_hook_registry),
        client_hooks: std::cell::RefCell::new(client_hooks),
        hook_resolved_workspace_root: resolved_workspace_root,
        vcs_kind: {
            let root = std::path::Path::new(&session_info.cwd);
            match xai_grok_workspace::session::git::discover_git_root(root) {
                xai_grok_workspace::session::git::GitDiscoveryResult::Found(git_root) => {
                    xai_grok_workspace::session::git::detect_vcs_kind(&git_root)
                }
                _ => xai_grok_workspace::session::git::VcsKind::None,
            }
        },
        hook_load_errors: std::cell::RefCell::new(_hook_load_errors),
        plugin_registry: std::cell::RefCell::new(plugin_registry.clone()),
        plugin_registry_handle,
        events: crate::session::events::EventTracker::new(
            &crate::session::persistence::session_dir(&session_info),
        ),
        observability_bridge: obs_bridge,
        current_turn_number: std::cell::Cell::new(0),
        last_recap_main_turn: std::cell::Cell::new(0),
        recap_in_flight: std::cell::Cell::new(false),
        recap_epoch: std::cell::Cell::new(0),
        session_turn_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        streaming_turn_capture: parking_lot::Mutex::new(StreamingTurnCapture::default()),
        turn_stream_drained: parking_lot::Mutex::new(None),
        sampler_handle,
        rebuild_spec: rebuild_spec.clone(),
        image_description_model,
        image_describe_cache: Arc::new(crate::session::image_describe::ImageDescribeCache::new()),
        subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
        workspace_ops: workspace_ops.clone(),
        trace_config_template: std::cell::RefCell::new(None),
    });
    {
        let drainer_session = session.clone();
        let mut sampler_event_rx = sampler_event_rx;
        tokio::task::spawn_local(async move {
            while let Some(event) = sampler_event_rx.recv().await {
                drainer_session.handle_sampling_event(event).await;
            }
            tracing::debug!("sampler event drainer exiting (channel closed)");
        });
    }
    {
        let drainer_session = session.clone();
        let Some(mut goal_update_rx) = session.goal_update_rx.borrow_mut().take() else {
            unreachable!("goal_update_rx must be Some at session spawn");
        };
        tokio::task::spawn_local(async move {
            while let Some(envelope) = goal_update_rx.recv().await {
                let current_tokens =
                    drainer_session.chat_state_handle.get_total_tokens().await as i64;
                drainer_session
                    .drain_goal_updates_with_extra(
                        current_tokens,
                        DrainPurpose::MidTurn,
                        vec![envelope],
                    )
                    .await;
            }
            tracing::debug!("goal update drainer exiting (channel closed)");
        });
    }
    {
        let snapshot = session.tool_metadata_snapshot.clone();
        let tool_index = crate::session::tool_index::Bm25ToolSearchIndex::new(snapshot);
        session
            .agent
            .borrow()
            .tool_bridge()
            .update_resource(xai_grok_tools::types::tool_index::ToolIndex(
                std::sync::Arc::new(tool_index),
            ))
            .await;
    }
    if let Some(client) = managed_gateway_tool_client.clone() {
        session
            .agent
            .borrow()
            .tool_bridge()
            .update_resource(client)
            .await;
    }
    {
        let plan_path = session.plan_mode.lock().plan_file_path().to_path_buf();
        session
            .agent
            .borrow()
            .tool_bridge()
            .update_resource(xai_grok_tools::types::resources::PlanFilePath(plan_path))
            .await;
    }
    session.inject_deny_read_globs().await;
    if session.permissions.is_auto_mode() {
        session.wire_permission_auto_llm_classifier().await;
    }
    session
        .agent
        .borrow()
        .tool_bridge()
        .update_resource(
            xai_grok_tools::implementations::grok_build::update_goal::GoalUpdateHandle(
                session.goal_update_tx.clone(),
            ),
        )
        .await;
    if let Some(ref display_cwd) = prompt_display_cwd {
        session
            .agent
            .borrow()
            .tool_bridge()
            .set_display_cwd(std::path::PathBuf::from(display_cwd))
            .await;
    }
    if let Some(storage) = session.memory.storage() {
        crate::session::memory::init_sqlite_vec();
        let index_config = memory_config
            .as_ref()
            .map_or_else(Default::default, |mc| mc.index.clone());
        let embed_config = memory_config
            .as_ref()
            .map(|mc| mc.embedding.clone())
            .unwrap_or_default();
        let embed_dims = embed_config.dimensions;
        let sampling_base_url = embed_base_url.clone();
        let sampling_api_key = embed_api_key.clone();
        let session_id_for_reindex = session_info.id.to_string();
        let chunks_added_counter = session.memory.chunks_added.clone();
        tokio::task::spawn_local(async move {
            let db_path = storage.workspace_dir().join("index.sqlite");
            if let Ok(mut index) = crate::session::memory::MemoryIndex::open_or_create(
                &db_path,
                storage.clone(),
                index_config,
                embed_dims,
            ) && let Ok(files) = storage.list_memory_files()
            {
                let reindex_start = std::time::Instant::now();
                let (mut total_added, mut total_updated, mut total_removed) = (0, 0, 0);
                for file in &files {
                    let source = storage.classify_source(file);
                    if let Ok(stats) = index.reindex_file(file, source) {
                        total_added += stats.added;
                        total_updated += stats.updated;
                        total_removed += stats.removed;
                    }
                }
                tracing::info!(
                    target : xai_grok_telemetry::memory_log::TARGET, files = files.len(),
                    "MEMORY_REINDEX: background reindex complete"
                );
                let embedded_count = if let Some(api_key) = sampling_api_key {
                    if let Some(provider) =
                        crate::session::memory::embedding::ApiEmbeddingProvider::from_session(
                            &embed_config,
                            sampling_base_url,
                            api_key,
                        )
                    {
                        crate::session::memory::embed_missing_chunks(&index, &provider).await
                    } else {
                        0
                    }
                } else {
                    0
                };
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::memory_telemetry::MemoryReindex {
                        session_id: session_id_for_reindex.clone(),
                        source: "init".to_owned(),
                        added: total_added,
                        updated: total_updated,
                        removed: total_removed,
                        embedded: embedded_count,
                        duration_ms: reindex_start.elapsed().as_millis() as u64,
                        trigger: "init".to_owned(),
                    },
                );
                chunks_added_counter
                    .fetch_add(total_added as u64, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    if let Some(cancel) = sync_loop_cancel {
        tracing::info!(session_id = % session_info.id.0, "Spawning feedback sync loop");
        let fm = feedback_manager.clone();
        tokio::spawn(async move {
            fm.run_sync_loop(cancel).await;
        });
    } else {
        tracing::debug!(
            session_id = % session_info.id.0,
            "No feedback client available, skipping sync loop"
        );
    }
    {
        use agent_client_protocol::Client as _;
        use xai_grok_tools::implementations::grok_build::ask_user_question::{
            AskUserQuestionExtRequest, AskUserQuestionExtResponse, UserQuestionError,
            UserQuestionResponse,
        };
        let gateway = session.notifications.gateway.clone();
        let session_id = session.session_info.id.clone();
        let current_prompt_mode = session.current_prompt_mode.clone();
        let pending_interactions = session.pending_interactions.clone();
        let session_for_hooks = session.clone();
        let mut user_question_rx = user_question_rx;
        tokio::task::spawn_local(async move {
            while let Some(mut request) = user_question_rx.recv().await {
                use xai_grok_tools::implementations::grok_build::ask_user_question::AskUserQuestionMode;
                let mode = match *current_prompt_mode.lock() {
                    PromptMode::Plan => AskUserQuestionMode::Plan,
                    _ => AskUserQuestionMode::Default,
                };
                let ext_req = AskUserQuestionExtRequest {
                    session_id: session_id.0.to_string(),
                    tool_call_id: request.tool_call_id.clone(),
                    questions: request.questions.clone(),
                    mode,
                };
                debug_assert!(
                    !ext_req.session_id.is_empty(),
                    "ask_user_question reverse-request must carry a non-empty sessionId (design §5.4)"
                );
                let ext_request = agent_client_protocol::ExtRequest::new(
                    "x.ai/ask_user_question",
                    serde_json::value::to_raw_value(&ext_req)
                        .expect("AskUserQuestionExtRequest serialization should not fail")
                        .into(),
                );
                session_for_hooks
                    .dispatch_notification_hook(
                        "elicitation_dialog",
                        Some("User question requested".into()),
                        None,
                        Some("info".into()),
                    )
                    .await;
                let questions_for_response = request.questions.clone();
                let tool_call_id = request.tool_call_id.clone();
                let result = {
                    let _pending_guard =
                        crate::session::pending_interaction::PendingInteractionGuard::new(
                            pending_interactions.clone(),
                            gateway.clone(),
                            session_id.clone(),
                            tool_call_id.clone(),
                            crate::session::pending_interaction::PendingKind::Question,
                        );
                    tokio::select! {
                        biased; () = request.result_tx.closed() => { tracing::info!(%
                        tool_call_id,
                        "ask_user_question tool receiver closed (timeout or cancel); abandoning ACP wait");
                        Ok(UserQuestionResponse::Cancelled) } acp_result = gateway
                        .ext_method(ext_request) => { match acp_result { Ok(raw) => {
                        match serde_json::from_str::< AskUserQuestionExtResponse > (raw.0
                        .get(),) { Ok(typed) => { Ok(typed
                        .into_response(questions_for_response)) } Err(e) =>
                        Err(UserQuestionError::MalformedResponse(e.to_string(),)), } }
                        Err(e) => Err(UserQuestionError::TransportError(e.to_string())),
                        } }
                    }
                };
                let _ = request.result_tx.send(result);
            }
        });
    }
    let (session_done_tx, session_done_rx) = tokio::sync::oneshot::channel::<()>();
    let telemetry_ctx = xai_grok_telemetry::session_ctx::TelemetryCtx::new(
        session.session_info.id.0.to_string(),
        session.tool_context.prompt_index.clone(),
    );
    if let Some(metrics) = harness_metrics {
        let hooks: Vec<super::telemetry::HookRegInfo> = session
            .hook_registry
            .borrow()
            .as_ref()
            .map(|reg| {
                reg.all_hooks()
                    .iter()
                    .map(|s| super::telemetry::HookRegInfo::from_spec(s))
                    .collect()
            })
            .unwrap_or_default();
        let telemetry_enabled = session.telemetry_enabled;
        tokio::spawn(async move {
            let ev = metrics.into_event(hooks).await;
            xai_grok_telemetry::session_ctx::log_event_dual(telemetry_enabled, ev);
        });
    }
    tokio::task::spawn_local(async move {
        xai_grok_telemetry::session_ctx::with_session_ctx(
            telemetry_ctx,
            run_session(
                session,
                cmd_rx,
                chat_state_event_rx,
                event_rx,
                fs_notify_config,
                codebase_indexes,
                index_root_for_session,
                fs_watch_caps,
            ),
        )
        .await;
        let _ = session_done_tx.send(());
    });
    Ok((
        SessionHandle {
            cmd_tx,
            persistence_tx: persistence.tx.clone(),
            current_prompt_id,
            pending_interactions,
            info: session_info,
            max_turns,
            hunk_tracker_handle,
            chat_state_handle: chat_state_handle_for_handle,
            signals_handle,
            gateway_enabled,
            mcp_servers,
            initial_client_mcp_servers,
            display_cwd: None,
            feedback_manager: feedback_manager.clone(),
            upload_queue: upload_queue.clone(),
            upload_failures_since_success: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            tool_context: tool_context_for_handle,
            model_id: session_model_id,
            reasoning_effort: sampling_config.reasoning_effort,
            yolo_mode: session_yolo_mode,
            origin_client: origin_client.clone(),
            code_nav_enabled,
            ask_user_question_enabled,
            plan_mode: plan_mode.clone(),
            force_compact,
            permission_handle: permissions_for_handle,
            attribution_callback: attribution_callback_for_handle,
            agent_name: agent_name_for_handle,
            managed_mcp_proxy_base_url,
            session_default_agent_profile,
            allowed_subagent_types: allowed_subagent_types_for_handle,
            hook_registry: hook_registry_for_handle,
            workspace_ops: workspace_ops_for_handle,
            terminal_backend: Some(terminal_backend.clone()),
            tools_notification_handle: Some(tools_notification_handle.clone()),
            scheduler_handle: scheduler_handle_for_handle,
        },
        permission_events_rx,
        system_prompt,
        session_done_rx,
    ))
}
/// Handle for a session's dedicated thread. Stored separately from `SessionHandle`
/// (which derives `Clone`) because `JoinHandle` is not `Clone`.
pub struct SessionThread {
    join_handle: std::thread::JoinHandle<()>,
}
impl SessionThread {
    /// Check if the session thread has exited (panicked or finished).
    pub fn is_finished(&self) -> bool {
        self.join_handle.is_finished()
    }
    /// Construct from a raw `JoinHandle`. Used in tests.
    #[cfg(test)]
    pub fn from_handle(handle: std::thread::JoinHandle<()>) -> Self {
        Self {
            join_handle: handle,
        }
    }
}
/// Return type from the session thread's initialization, sent via oneshot.
struct SessionInitResult {
    handle: SessionHandle,
    permission_events_rx: mpsc::UnboundedReceiver<PermissionEvent>,
    system_prompt: String,
}
/// Spawn a session actor on a dedicated thread with its own tokio runtime and `LocalSet`.
///
/// The entire `spawn_session_actor` body runs on the session thread — the `!Send`
/// `SessionActor` is constructed there and never crosses a thread boundary. The
/// `Send` construction parameters are moved into the thread, and the `Send` results
/// (`SessionHandle`, `permission_events_rx`, `system_prompt`) are sent back to the
/// caller via a oneshot channel.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_session_on_thread(
    session_info: SessionInfo,
    gateway: GatewaySender,
    sampling_config: SamplingConfig,
    credentials: xai_chat_state::Credentials,
    auth_method_id: crate::agent::auth_method::SharedAuthMethodId,
    auth_manager: Option<Arc<AuthManager>>,
    attribution_callback: Option<xai_grok_sampler::SharedAttributionCallback>,
    tool_context: ToolContext,
    mcp_servers: Vec<acp::McpServer>,
    initial_client_mcp_servers: Vec<acp::McpServer>,
    mcp_meta_config_map: McpMetaConfigMap,
    parent_mcp_pool: Option<crate::session::mcp_servers::SharedMcpPool>,
    acp_mcp_servers: Vec<crate::session::mcp_servers::AcpServerEntry>,
    support_permission: bool,
    telemetry_enabled: bool,
    auto_update: Option<bool>,
    persistence: PersistenceHandle,
    conversation: Vec<ConversationItem>,
    rewind_points_path: Option<std::path::PathBuf>,
    fs_notify_config: Option<ClientFsConfig>,
    initial_total_tokens: u64,
    startup_hints: StartupHints,
    client_type: ClientType,
    auto_compact_threshold_percent: u8,
    system_prompt_label: String,
    compaction_mode: xai_chat_state::CompactionMode,
    compaction_verbatim_input: bool,
    compaction_tool_choice: crate::util::config::CompactionToolChoice,
    two_pass_enabled: bool,
    buffering_settings: Option<BufferingSettings>,
    origin_client: Option<crate::http::OriginClientInfo>,
    codebase_indexes: std::sync::Arc<parking_lot::Mutex<CodebaseIndexManager>>,
    code_nav_enabled: bool,
    fs_watch_caps: fs_watch::FsWatchCapabilities,
    feedback_proxy_url: Option<String>,
    feedback_user_token: Option<String>,
    feedback_alpha_test_key: Option<String>,
    deployment_key: Option<String>,
    client_terminal_capable: bool,
    client_fs_capable: bool,
    gateway_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    agent_definition: AgentDefinition,
    session_default_agent_profile: Option<String>,
    skills_config: SkillsConfig,
    preloaded_skills: Option<Vec<xai_grok_tools::implementations::skills::types::SkillInfo>>,
    compat: CompatConfig,
    incremental_bash_output: bool,
    persisted_signals: Option<crate::session::signals::SessionSignals>,
    persisted_plan_mode: Option<crate::session::plan_mode::PlanModeSnapshot>,
    persisted_goal_mode: Option<crate::session::goal_tracker::GoalOrchestration>,
    persisted_announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    memory_config: Option<crate::config::MemoryConfig>,
    loc_tracking_enabled: bool,
    feedback_flags: crate::session::feedback_manager::FeedbackFlags,
    managed_mcp_handle: crate::session::managed_mcp::ManagedMcpStateHandle,
    managed_mcp_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    managed_mcp_proxy_base_url: String,
    session_model_id: acp::ModelId,
    session_yolo_mode: bool,
    session_auto_mode: bool,
    session_client_identifier: Option<String>,
    inference_idle_timeout_secs: u64,
    max_retries: Option<u32>,
    web_search_sampling_config: Option<xai_grok_sampler::SamplerConfig>,
    web_fetch_config: xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig,
    image_gen_config: xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig,
    video_gen_config: xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig,
    app_builder_deployer_config: xai_grok_tools::implementations::grok_build::deploy_app::AppBuilderDeployerConfig,
    write_file_enabled: bool,
    goal_enabled: bool,
    subagents_enabled: bool,
    ask_user_question_enabled: bool,
    client_hooks: crate::extensions::hooks::ClientHooks,
    prompt_display_cwd: Option<String>,
    subagent_toggle: std::collections::HashMap<String, bool>,
    persona_summaries: Vec<String>,
    prompt_audience: xai_grok_agent::prompt::context::PromptAudience,
    role_instructions: Option<String>,
    persona_instructions: Option<String>,
    disable_web_search: bool,
    backend_tools_enabled: bool,
    respect_gitignore: bool,
    path_not_found_hints: bool,
    tool_params_json: crate::session::agent_rebuild::ResolvedToolParamsJson,
    plugin_registry: Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>,
    plugin_registry_handle: Option<xai_grok_agent::plugins::SharedPluginRegistryHandle>,
    models_manager: crate::agent::models::ModelsManager,
    parent_traceparent: Option<String>,
    inherited_permission_handle: Option<xai_grok_workspace::permission::PermissionHandle>,
    api_key_provider: Option<xai_grok_tools::types::SharedApiKeyProvider>,
    image_description_model: String,
    hook_registry_override: Option<std::sync::Arc<xai_grok_hooks::discovery::HookRegistry>>,
    workspace_ops: xai_grok_workspace::WorkspaceOps,
    cli_permission_rules: Vec<xai_grok_workspace::permission::types::PermissionRule>,
    todo_gate: bool,
    remote_settings: Option<crate::util::config::RemoteSettings>,
    laziness_debug_log: Option<std::path::PathBuf>,
    parent_terminal_backend: Option<
        std::sync::Arc<dyn xai_grok_tools::computer::types::TerminalBackend>,
    >,
    parent_scheduler_handle: Option<
        xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerHandle,
    >,
    max_turns: Option<usize>,
    forked_tool_override: Option<Vec<ToolSpec>>,
) -> Result<
    (
        SessionHandle,
        mpsc::UnboundedReceiver<PermissionEvent>,
        String,
        SessionThread,
    ),
    acp::Error,
> {
    let (init_tx, init_rx) = tokio::sync::oneshot::channel::<
        Result<SessionInitResult, xai_grok_agent::AgentBuildError>,
    >();
    let sid = session_info.id.0.to_string();
    let thread_name = format!("ses-{}", &sid[..sid.len().min(8)]);
    const SESSION_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;
    let join_handle = std::thread::Builder::new()
        .name(thread_name)
        .stack_size(SESSION_THREAD_STACK_SIZE)
        .spawn(move || {
            let (initial_last_compaction, initial_prompt_texts) = {
                let session_dir = crate::session::persistence::session_dir(&session_info);
                let updates_path = session_dir.join("updates.jsonl");
                let initial_last_compaction = {
                    let _timer = crate::instrumentation_timer!(
                        "session.spawn_actor.find_compaction_checkpoint"
                    );
                    crate::session::helpers::replay::find_latest_compaction_checkpoint(
                        &updates_path,
                    )
                    .ok()
                    .flatten()
                    .map(|cp| cp.prompt_index_at_compaction)
                };
                let initial_prompt_texts = {
                    let _timer =
                        crate::instrumentation_timer!("session.spawn_actor.load_user_prompts");
                    SessionActor::load_user_prompts_from_updates(&updates_path).unwrap_or_default()
                };
                (initial_last_compaction, initial_prompt_texts)
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("session runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let _trace_span = parent_traceparent.as_ref().map(|tp| {
                    let meta = serde_json::json!({ "traceparent" : tp })
                        .as_object()
                        .cloned()
                        .unwrap_or_default();
                    let span = xai_file_utils::trace_context::span_from_meta_traceparent(&meta);
                    span.entered()
                });
                let (handle, permission_events_rx, system_prompt, session_done_rx) =
                    match spawn_session_actor(
                        session_info,
                        gateway,
                        sampling_config,
                        credentials,
                        auth_method_id,
                        auth_manager,
                        attribution_callback,
                        tool_context,
                        mcp_servers,
                        initial_client_mcp_servers,
                        mcp_meta_config_map,
                        parent_mcp_pool,
                        acp_mcp_servers,
                        support_permission,
                        telemetry_enabled,
                        auto_update,
                        persistence,
                        conversation,
                        rewind_points_path,
                        initial_last_compaction,
                        initial_prompt_texts,
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
                        origin_client,
                        codebase_indexes,
                        code_nav_enabled,
                        fs_watch_caps,
                        feedback_proxy_url,
                        feedback_user_token,
                        feedback_alpha_test_key,
                        deployment_key,
                        client_terminal_capable,
                        client_fs_capable,
                        gateway_enabled,
                        agent_definition,
                        session_default_agent_profile,
                        skills_config,
                        preloaded_skills,
                        compat,
                        incremental_bash_output,
                        persisted_signals,
                        persisted_plan_mode,
                        persisted_goal_mode,
                        persisted_announcement_state,
                        memory_config,
                        loc_tracking_enabled,
                        feedback_flags,
                        managed_mcp_handle,
                        managed_mcp_expires_at,
                        managed_mcp_proxy_base_url,
                        session_model_id,
                        session_yolo_mode,
                        session_auto_mode,
                        session_client_identifier,
                        inference_idle_timeout_secs,
                        max_retries,
                        web_search_sampling_config,
                        web_fetch_config,
                        image_gen_config,
                        video_gen_config,
                        app_builder_deployer_config,
                        write_file_enabled,
                        goal_enabled,
                        subagents_enabled,
                        ask_user_question_enabled,
                        client_hooks,
                        prompt_display_cwd,
                        subagent_toggle,
                        persona_summaries,
                        prompt_audience,
                        role_instructions,
                        persona_instructions,
                        disable_web_search,
                        backend_tools_enabled,
                        respect_gitignore,
                        path_not_found_hints,
                        tool_params_json,
                        plugin_registry,
                        plugin_registry_handle,
                        models_manager,
                        inherited_permission_handle,
                        api_key_provider,
                        image_description_model,
                        hook_registry_override,
                        workspace_ops,
                        cli_permission_rules,
                        todo_gate,
                        remote_settings,
                        laziness_debug_log,
                        parent_terminal_backend,
                        parent_scheduler_handle,
                        max_turns,
                        forked_tool_override,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(e) => {
                            let _ = init_tx.send(Err(e));
                            return;
                        }
                    };
                let _ = init_tx.send(Ok(SessionInitResult {
                    handle,
                    permission_events_rx,
                    system_prompt,
                }));
                let _ = session_done_rx.await;
            });
        })
        .expect("spawn session thread");
    let init = init_rx
        .await
        .map_err(|_| {
            tracing::error!("Session thread panicked during initialization");
            acp::Error::internal_error().data("session thread panicked during initialization")
        })?
        .map_err(|e| acp::Error::internal_error().data(format!("agent building failed: {e}")))?;
    Ok((
        init.handle,
        init.permission_events_rx,
        init.system_prompt,
        SessionThread { join_handle },
    ))
}
/// Production [`crate::session::mcp_restart::RestartActions`] impl.
///
/// Captured by the dispatcher task at session startup when
/// `mcp.auto_restart=true`. Holds an `Arc<SessionActor>` plus the
/// dispatcher's `SharedShutdownState` so:
///
/// - `is_stdio_server_configured` resolves against
///   [`SessionActor::is_stdio_server_configured`] (which reads
///   `McpState::configs`).
/// - `is_in_shutting_down` peeks at the dispatcher's set.
/// - `respawn_stdio` delegates to
///   [`SessionActor::respawn_stdio`] (re-runs `start_mcp_server`,
///   handshake, liveness arm, owned_clients swap).
/// - `push_status` forwards directly via the session's gateway.
pub(crate) struct SessionRestartActions {
    session: Arc<SessionActor>,
    shutdown: crate::session::mcp_dispatcher::SharedShutdownState,
}
impl SessionRestartActions {
    pub(crate) fn new(
        session: Arc<SessionActor>,
        shutdown: crate::session::mcp_dispatcher::SharedShutdownState,
    ) -> Self {
        Self { session, shutdown }
    }
}
#[async_trait::async_trait(?Send)]
impl crate::session::mcp_restart::RestartActions for SessionRestartActions {
    async fn is_stdio_server_configured(&self, server: &str) -> bool {
        self.session.is_stdio_server_configured(server).await
    }
    fn is_in_shutting_down(&self, server: &str) -> bool {
        self.shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_shutting_down(server)
    }
    async fn respawn_stdio(&self, server: &str) -> Result<(), String> {
        self.session.respawn_stdio(server).await
    }
    async fn is_http_server_configured(&self, server: &str) -> bool {
        self.session.is_http_server_configured(server).await
    }
    async fn reset_http_client(&self, server: &str) -> Result<(), String> {
        self.session.reset_http_client(server).await
    }
    fn unregister_server_tools(&self, server: &str) {
        self.session.unregister_server_tools(server);
    }
    fn push_status(&self, payload: &crate::session::mcp_dispatcher::McpServerStatusPayload) {
        crate::session::mcp_restart::forward_status(&self.session.notifications.gateway, payload);
    }
    fn begin_restart(&self, server: &str) -> bool {
        self.shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin_restart(server.to_string())
    }
    fn end_restart(&self, server: &str) {
        self.shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .end_restart(server);
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalBackendKind {
    ReuseParent,
    AcpClient,
    LocalPersistent,
    LocalNonPersistent,
}
fn select_terminal_backend_kind(
    is_subagent: bool,
    has_parent_backend: bool,
    client_terminal_capable: bool,
    has_gateway: bool,
    cursor_harness: bool,
) -> TerminalBackendKind {
    if is_subagent && has_parent_backend {
        TerminalBackendKind::ReuseParent
    } else if client_terminal_capable && has_gateway {
        TerminalBackendKind::AcpClient
    } else if cursor_harness {
        TerminalBackendKind::LocalPersistent
    } else {
        TerminalBackendKind::LocalNonPersistent
    }
}
#[cfg(test)]
mod terminal_backend_select_tests {
    use super::{TerminalBackendKind, select_terminal_backend_kind};
    #[test]
    fn subagent_with_parent_reuses_parent() {
        assert_eq!(
            select_terminal_backend_kind(true, true, true, true, true),
            TerminalBackendKind::ReuseParent
        );
    }
    #[test]
    fn subagent_without_parent_falls_through() {
        assert_eq!(
            select_terminal_backend_kind(true, false, true, true, true),
            TerminalBackendKind::AcpClient
        );
        assert_eq!(
            select_terminal_backend_kind(true, false, false, true, true),
            TerminalBackendKind::LocalPersistent
        );
    }
    #[test]
    fn non_subagent_never_reuses_parent() {
        assert_eq!(
            select_terminal_backend_kind(false, true, false, false, true),
            TerminalBackendKind::LocalPersistent
        );
    }
    #[test]
    fn client_terminal_uses_acp_only_with_gateway() {
        assert_eq!(
            select_terminal_backend_kind(false, false, true, true, true),
            TerminalBackendKind::AcpClient
        );
        assert_eq!(
            select_terminal_backend_kind(false, false, true, false, true),
            TerminalBackendKind::LocalPersistent
        );
    }
    #[test]
    fn local_session_cursor_harness_selects_persistent_backend() {
        assert_eq!(
            select_terminal_backend_kind(false, false, false, false, true),
            TerminalBackendKind::LocalPersistent
        );
        assert_eq!(
            select_terminal_backend_kind(false, false, false, false, false),
            TerminalBackendKind::LocalNonPersistent
        );
    }
}
