#![allow(dead_code)]
use super::*;
/// Wrap `id` in a shared auth-method handle for `SessionActor` test literals
/// (the field is now a shared live handle, not an owned id).
pub(crate) fn test_auth_method_id(id: &str) -> crate::agent::auth_method::SharedAuthMethodId {
    crate::agent::auth_method::new_shared_auth_method_id(Some(acp::AuthMethodId::new(id)))
}
/// Harness contract sentence — both halves ("verifies what's complete" AND
/// "tells you what's missing").
pub(crate) const HARNESS_VERIFIES_SENTENCE: &str =
    "verifies what's complete and tells you what's missing on the next nudge";
/// Plan-aware seed-todos instruction (`goal_plan_block.md`).
pub(crate) const PLAN_SEED_TODOS_PHRASE: &str =
    "Seed todos from the plan's acceptance criteria via";
#[cfg(test)]
pub(crate) fn noop_observability_bridge() -> xai_computer_hub_sdk::ObservabilityBridge {
    xai_computer_hub_sdk::ObservabilityBridge::new(
        None,
        xai_tool_protocol::SessionId::new("test").expect("valid"),
    )
}
#[cfg(test)]
pub(crate) async fn test_agent_default() -> xai_grok_agent::Agent {
    test_agent_with_tools(vec![]).await
}
#[cfg(test)]
pub(crate) async fn test_agent_backend_search(
    hosted_tools: Vec<xai_grok_sampling_types::HostedTool>,
) -> xai_grok_agent::Agent {
    let base = test_agent_default().await;
    xai_grok_agent::Agent::new(
        base.definition().clone(),
        xai_grok_agent::PromptContext::default(),
        String::new(),
        base.tool_bridge().clone(),
        xai_grok_agent::ReminderPolicy::default(),
        xai_grok_agent::CompactionPolicy::default(),
        hosted_tools,
        true,
    )
}
/// Like [`test_agent_default`] but registers the `update_goal` tool so
/// `command_availability().goal` is satisfied and `/goal …` slash commands
/// resolve to their builtins when a turn is driven through `handle_prompt`.
#[cfg(test)]
pub(crate) async fn test_agent_with_goal_tool() -> xai_grok_agent::Agent {
    use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalTool;
    use xai_grok_tools::registry::types::ToolConfig;
    test_agent_with_tools(vec![ToolConfig::for_tool::<UpdateGoalTool>()]).await
}
/// Grok-build agent with the real `TodoWriteTool` (id `todo_write`, kind
/// `Plan`) registered, so `tool_for_kind(ToolKind::Plan)` resolves through the
/// live toolset instead of the literal fallback.
#[cfg(test)]
pub(crate) async fn test_grok_build_agent_with_todo() -> xai_grok_agent::Agent {
    use xai_grok_tools::implementations::grok_build::todo::TodoWriteTool;
    use xai_grok_tools::registry::types::ToolConfig;
    test_agent_with_tools(vec![ToolConfig::for_tool::<TodoWriteTool>()]).await
}
/// Agent with the real `enter_plan_mode` + `exit_plan_mode` tools registered so
/// `prepare_tool_call` can parse a genuine `exit_plan_mode` call.
/// `exit_plan_mode` only finalizes when `enter_plan_mode` is also present.
#[cfg(test)]
pub(crate) async fn test_agent_with_plan_tools() -> xai_grok_agent::Agent {
    use xai_grok_tools::implementations::grok_build::enter_plan_mode::EnterPlanModeTool;
    use xai_grok_tools::implementations::grok_build::exit_plan_mode::ExitPlanModeTool;
    use xai_grok_tools::registry::types::ToolConfig;
    test_agent_with_tools(vec![
        ToolConfig::for_tool::<EnterPlanModeTool>(),
        ToolConfig::for_tool::<ExitPlanModeTool>(),
    ])
    .await
}
#[cfg(test)]
pub(crate) async fn test_agent_with_tools(
    tools: Vec<xai_grok_tools::registry::types::ToolConfig>,
) -> xai_grok_agent::Agent {
    test_agent_from_config(
        xai_grok_tools::registry::types::ToolServerConfig {
            tools,
            behavior_preset: None,
        },
        xai_grok_agent::AgentDefinition::default_grok_build(),
        std::sync::Arc::new(xai_grok_tools::computer::local::LocalTerminalBackend::new()),
    )
    .await
}
#[cfg(test)]
pub(crate) async fn test_agent_with_user_message_template(
    template: xai_grok_agent::prompt::user_message::UserMessageTemplate,
) -> xai_grok_agent::Agent {
    let mut definition = xai_grok_agent::AgentDefinition::default_grok_build();
    definition.user_message_template = template;
    test_agent_from_config(
        xai_grok_tools::registry::types::ToolServerConfig {
            tools: vec![],
            behavior_preset: None,
        },
        definition,
        std::sync::Arc::new(xai_grok_tools::computer::local::LocalTerminalBackend::new()),
    )
    .await
}
#[cfg(test)]
async fn test_agent_from_config(
    config: xai_grok_tools::registry::types::ToolServerConfig,
    definition: xai_grok_agent::AgentDefinition,
    backend: std::sync::Arc<dyn xai_grok_tools::computer::types::TerminalBackend>,
) -> xai_grok_agent::Agent {
    use xai_grok_tools::computer::local::LocalFs;
    use xai_grok_tools::computer::types::AsyncFileSystem;
    use xai_grok_tools::notification::ToolNotificationHandle;
    use xai_grok_tools::registry::types::SessionContext;
    let builder = crate::tools::bridge::ToolBridge::get_builder();
    let fs: std::sync::Arc<dyn AsyncFileSystem> = std::sync::Arc::new(LocalFs);
    let ctx = SessionContext {
        backend,
        fs,
        cwd: std::path::PathBuf::from("/tmp"),
        session_folder: std::env::temp_dir().join("grok-test"),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: std::path::PathBuf::from("/tmp/tool_state.json"),
        memory_backend: None,
        web_search_config: Default::default(),
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let tool_bridge = crate::tools::bridge::ToolBridge::finalize_builder(builder, config, ctx)
        .await
        .expect("finalize_builder should succeed for tests");
    #[allow(clippy::arc_with_non_send_sync)]
    let tool_bridge = std::sync::Arc::new(tool_bridge);
    xai_grok_agent::Agent::new(
        definition,
        xai_grok_agent::PromptContext::default(),
        String::new(),
        tool_bridge,
        xai_grok_agent::ReminderPolicy::default(),
        xai_grok_agent::CompactionPolicy::default(),
        vec![],
        false,
    )
}
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct DummyTerminal;
#[cfg(test)]
#[async_trait::async_trait]
impl crate::terminal::AsyncTerminalRunner for DummyTerminal {
    async fn run(
        &self,
        _request: crate::terminal::runner::TerminalRunRequest,
    ) -> Result<crate::terminal::runner::TerminalRunResult, crate::terminal::runner::TerminalError>
    {
        Err(crate::terminal::runner::TerminalError::Other(
            "dummy terminal".into(),
        ))
    }
}
#[cfg(test)]
pub(crate) async fn create_test_actor_ex(
    total_tokens: u64,
    context_window: u64,
    threshold_percent: u8,
    gateway_tx: tokio::sync::mpsc::UnboundedSender<xai_acp_lib::AcpClientMessage>,
    persistence_tx: tokio::sync::mpsc::UnboundedSender<PersistenceMsg>,
) -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    let cwd = xai_grok_paths::AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap();
    let fs = Arc::new(xai_grok_workspace::file_system::MockFs::new(
        cwd.to_path_buf(),
    ));
    let terminal = Arc::new(DummyTerminal {});
    let (hunk_tx, _hunk_rx) = tokio::sync::mpsc::unbounded_channel();
    let hunk_tracker_handle = xai_hunk_tracker::HunkTrackerActor::spawn(
        "test-actor".to_string(),
        cwd.to_path_buf(),
        hunk_tx,
        xai_hunk_tracker::TrackingMode::AgentOnly,
        tokio_util::sync::CancellationToken::new(),
    );
    let mut tool_context =
        ToolContext::new(cwd.clone(), None, None, fs, terminal, hunk_tracker_handle);
    tool_context.task_completion_reservations =
        Some(xai_grok_tools::reminders::task_completion::TaskCompletionReservations::default());
    tool_context.task_wake_suppressed =
        Some(xai_grok_tools::reminders::task_completion::TaskWakeSuppressed::default());
    let state = TokioMutex::new(State {
        running_task: None,
        pending_inputs: VecDeque::new(),
        combine_edit_holds: std::collections::HashSet::new(),
        pending_notifications: Vec::new(),
        notifications_suppressed: false,
        rewindable: false,
        nudges_used_this_session: 0,
    });
    let (chat_event_tx, _chat_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
    let chat_state_handle = xai_chat_state::ChatStateActor::spawn(
        vec![],
        xai_grok_sampling_types::SamplingConfig {
            base_url: "http://localhost".to_string(),
            model: "test".to_string(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: Default::default(),
            extra_headers: Default::default(),
            query_params: Default::default(),
            env_http_headers: Default::default(),
            context_window: std::num::NonZeroU64::new(context_window)
                .expect("test context_window must be non-zero"),
            reasoning_effort: None,
            stream_tool_calls: None,
        },
        Box::new(xai_chat_state::NullChatPersistence),
        chat_event_tx,
        tokio_util::sync::CancellationToken::new(),
    );
    chat_state_handle.record_token_usage(total_tokens);
    let actor = SessionActor {
        session_info: SessionInfo {
            id: acp::SessionId::new("test-actor"),
            cwd: cwd.as_str().to_string(),
        },
        auth_method_id: test_auth_method_id("test-auth"),
        model_auth_memo: std::cell::RefCell::new(None),
        attribution_callback: None,
        auth_manager: None,
        state,
        notifications: NotificationSender {
            gateway: GatewaySender::new(gateway_tx),
            gateway_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            persistence_tx,
        },
        permissions: xai_grok_workspace::permission::PermissionHandle::allow_all(),
        tool_context,
        deny_read_globs: Vec::new(),
        mcp_state: Arc::new(TokioMutex::new(McpState::new(vec![]))),
        mcp_strategy: McpInitStrategy::Blocking,
        chat_state_handle,
        unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
        current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        pending_interactions: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        telemetry_enabled: false,
        supports_backend_search: std::cell::Cell::new(false),
        tool_overrides: std::cell::RefCell::new(None),
        resolved_tool_overrides: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
        compactions_remaining: std::cell::Cell::new(None),
        compaction_at_tokens: std::cell::Cell::new(None),
        doom_loop_recovery: None,
        doom_loop_turn_tally: Default::default(),
        file_state_tracker: Arc::new(FileStateTracker::new()),
        rewind_pending_prompt: std::sync::Mutex::new(None),
        startup_hints: StartupHints::default(),
        forked_tool_override: None,
        compaction: crate::session::compaction_config::CompactionConfig {
            threshold_percent: std::cell::Cell::new(threshold_percent),
            force_compact: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            context_window_override: None,
            count: std::sync::atomic::AtomicU64::new(0),
            auto_compact_suppressed: std::sync::atomic::AtomicU8::new(0),
            previous_model: std::cell::Cell::new(None),
            compaction_mode: xai_chat_state::CompactionMode::Transcript,
            verbatim_input: true,
            tool_choice: crate::util::config::CompactionToolChoice::Auto,
            prefire: crate::session::compaction_config::PrefireState::default(),
            prefix_released: std::sync::atomic::AtomicBool::new(false),
        },
        memory: crate::session::memory_state::SessionMemory {
            flush_config: crate::config::MemoryFlushConfig::default(),
            is_flushing: std::sync::atomic::AtomicBool::new(false),
            last_flush_compaction: std::sync::atomic::AtomicU64::new(0),
            storage: std::cell::RefCell::new(None),
            save_on_end: true,
            backend_params: None,
            initial_injection_config: Default::default(),
            context_injected: std::sync::atomic::AtomicBool::new(false),
            flush_count: std::sync::atomic::AtomicU64::new(0),
            last_flush_content: std::cell::RefCell::new(None),
            flush_success_count: std::sync::atomic::AtomicU64::new(0),
            flush_error_count: std::sync::atomic::AtomicU64::new(0),
            search_counter: std::cell::RefCell::new(None),
            injection_count: std::sync::atomic::AtomicU64::new(0),
            compaction_recovery_count: std::sync::atomic::AtomicU64::new(0),
            chunks_added: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dream_config: Default::default(),
            dream_count: std::sync::atomic::AtomicU64::new(0),
            dream_success_count: std::sync::atomic::AtomicU64::new(0),
            dream_error_count: std::sync::atomic::AtomicU64::new(0),
        },
        session_start: std::time::Instant::now(),
        inference_idle_timeout: Duration::from_secs(300),
        max_retries: 3,
        max_turns: None,
        pending_interjections: InterjectionBuffer::new(),
        pending_skill_reminders: Mutex::new(Vec::new()),
        idle_flush_timeout: None,
        dream_check_timeout: None,
        last_idle_flush_conversation_len: std::sync::atomic::AtomicUsize::new(0),
        event_tx,
        buffering_settings: None,
        client_identifier: None,
        origin_client: None,
        feedback_manager: Arc::new(FeedbackManager::local_only("test-session")),
        upload_queue: Arc::new(OnceLock::new()),
        sync_loop_cancel: None,
        agent: std::cell::RefCell::new(test_agent_default().await),
        last_reported_branch: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        git_head_enabled: false,
        models_manager: Default::default(),
        display_cwd: std::sync::OnceLock::new(),
        active_agent_type: parking_lot::Mutex::new(None),
        queue_exit_reminder_on_approved_exit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_skill: parking_lot::Mutex::new(None),
        current_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
        turn_start_prompt_mode: parking_lot::Mutex::new(PromptMode::Agent),
        turn_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
        plan_mode: Arc::new(parking_lot::Mutex::new(
            crate::session::plan_mode::PlanModeTracker::new(std::path::PathBuf::from(
                "/tmp/test-session",
            )),
        )),
        goal_enabled: false,
        background_workflows_enabled: false,
        goal_harness_enabled: std::sync::atomic::AtomicBool::new(false),
        goal_harness_availability_reconciled: std::sync::atomic::AtomicBool::new(false),
        goal_tracker: Arc::new(parking_lot::Mutex::new(
            crate::session::goal_tracker::GoalTracker::new(std::path::PathBuf::from(
                "/tmp/test-session",
            )),
        )),
        goal_turn_task_ids: parking_lot::Mutex::new(std::collections::HashSet::new()),
        goal_continuation_streak: std::sync::atomic::AtomicU32::new(0),
        goal_blocked_streak: std::sync::atomic::AtomicU32::new(0),
        goal_update_rx: std::cell::RefCell::new(None),
        goal_update_tx: tokio::sync::mpsc::unbounded_channel().0,
        workflow_manager: crate::session::workflow::manager::WorkflowManager::test_bundle().0,
        workflow_launch_tx: tokio::sync::mpsc::unbounded_channel().0,
        goal_classifier_enabled: false,
        goal_planner_enabled: false,
        goal_summary_enabled: false,
        goal_verifier_skeptic_count: 1,
        goal_role_models: Default::default(),
        goal_use_current_model_only: false,
        goal_classifier_max_runs: crate::session::goal_classifier::GOAL_CLASSIFIER_MAX_RUNS_DEFAULT,
        goal_strategist_every: 5,
        goal_reverify_after: crate::session::acp_session::GOAL_REVERIFY_AFTER_DEFAULT,
        goal_plan_reconciled: std::sync::atomic::AtomicBool::new(false),
        pending_classifier_completions: parking_lot::Mutex::new(std::collections::VecDeque::new()),
        goal_classifier_in_flight: std::sync::atomic::AtomicBool::new(false),
        managed_mcp_handle: Default::default(),
        managed_mcp_expires_at: std::sync::Mutex::new(None),
        initial_client_mcp_servers: vec![],
        tool_metadata_snapshot: Arc::new(std::sync::Mutex::new(Default::default())),
        mcp_announced_servers: Mutex::new(HashMap::new()),
        mcp_reminder_mode: McpReminderMode::Delta,
        mcp_reminder_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        mcp_connecting_reminder_injected: std::cell::Cell::new(false),
        mcp_handshakes_done: Arc::new(tokio::sync::Notify::new()),
        user_input_generation: std::sync::atomic::AtomicU64::new(0),
        laziness_debug_log: None,
        deferred_prefix: TaskSlot::new(),
        extension_registry: xai_agent_lifecycle::LocalExtensionRegistry::default(),
        last_announced_local_date: std::cell::Cell::new(chrono::Local::now().date_naive()),
        prefix_carries_fallback_date: std::cell::Cell::new(false),
        last_search_prompt_index: std::sync::atomic::AtomicI64::new(-1),
        last_api_request_at: std::sync::atomic::AtomicI64::new(0),
        hook_registry: std::cell::RefCell::new(None),
        client_hooks: Default::default(),
        hook_resolved_workspace_root: String::new(),
        vcs_kind: xai_grok_workspace::session::git::VcsKind::Git,
        hook_load_errors: std::cell::RefCell::new(Vec::new()),
        plugin_registry: std::cell::RefCell::new(None),
        plugin_registry_handle: None,
        events: crate::session::events::EventTracker::new(std::path::Path::new("/tmp")),
        observability_bridge: noop_observability_bridge(),
        current_turn_number: std::cell::Cell::new(0),
        last_recap_main_turn: std::cell::Cell::new(0),
        recap_in_flight: std::cell::Cell::new(false),
        recap_epoch: std::cell::Cell::new(0),
        session_turn_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        streaming_turn_capture: parking_lot::Mutex::new(StreamingTurnCapture::default()),
        turn_stream_drained: parking_lot::Mutex::new(None),
        sampler_handle: xai_grok_sampler::SamplerHandle::noop(),
        rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
        image_description_model: crate::test_support::TEST_MODEL.to_owned(),
        image_describe_cache: Arc::new(crate::session::image_describe::ImageDescribeCache::new()),
        subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
        workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
        trace_config_template: std::cell::RefCell::new(None),
    };
    if let Some(reservations) = actor.tool_context.task_completion_reservations.clone() {
        actor
            .agent
            .borrow()
            .tool_bridge()
            .update_resource(reservations)
            .await;
    }
    if let Some(gate) = actor.tool_context.task_wake_suppressed.clone() {
        actor
            .agent
            .borrow()
            .tool_bridge()
            .update_resource(gate)
            .await;
    }
    (actor, event_rx)
}
#[cfg(test)]
pub(crate) async fn create_test_actor(
    total_tokens: u64,
    context_window: u64,
    threshold_percent: u8,
    gateway_tx: tokio::sync::mpsc::UnboundedSender<xai_acp_lib::AcpClientMessage>,
    persistence_tx: tokio::sync::mpsc::UnboundedSender<PersistenceMsg>,
) -> SessionActor {
    create_test_actor_ex(
        total_tokens,
        context_window,
        threshold_percent,
        gateway_tx,
        persistence_tx,
    )
    .await
    .0
}
/// Build a user-originated `InputItem` carrying queue metadata, returning the
/// completion receiver so a test can assert the prompt's in-flight RPC is
/// resolved (not dropped) when the prompt is removed/cleared.
#[cfg(test)]
pub(crate) fn user_item_with_rx(
    id: &str,
    owner: &str,
) -> (InputItem, oneshot::Receiver<PromptTurnResult>) {
    let (respond_to, rx) = oneshot::channel();
    let text = format!("text for {id}");
    let item = InputItem {
        prompt_id: id.to_string(),
        prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(text.clone()))],
        prompt_mode: PromptMode::Agent,
        trace_gcs_config: None,
        artifact_tracker: None,
        client_identifier: Some(owner.to_string()),
        screen_mode: None,
        verbatim: false,
        json_schema: None,
        origin: crate::session::PromptOrigin::User,
        task_wake_fallback: None,
        tool_overrides_update: None,
        respond_to,
        persist_ack: None,
        parsed_prompt_tx: None,
        queue_meta: Some(crate::session::prompt_queue::QueueEntryMeta {
            id: id.to_string(),
            version: 0,
            owner: Some(owner.to_string()),
            last_editor: None,
            kind: "prompt".to_string(),
            text,
            combined_texts: None,
        }),
        send_now: false,
    };
    (item, rx)
}
/// Build a user-originated `InputItem` carrying queue metadata (dropping the
/// completion receiver — for tests that don't assert on the RPC result).
#[cfg(test)]
pub(crate) fn user_item(id: &str, owner: &str) -> InputItem {
    user_item_with_rx(id, owner).0
}
#[cfg(test)]
pub(crate) fn input_with_origin_rx(
    prompt_id: &str,
    origin: crate::session::PromptOrigin,
) -> (InputItem, oneshot::Receiver<PromptTurnResult>) {
    let (respond_to, rx) = oneshot::channel();
    let verbatim = origin.is_synthetic();
    let item = InputItem {
        prompt_id: prompt_id.to_string(),
        prompt_blocks: vec![],
        prompt_mode: PromptMode::Agent,
        trace_gcs_config: None,
        artifact_tracker: None,
        client_identifier: None,
        screen_mode: None,
        verbatim,
        json_schema: None,
        origin,
        task_wake_fallback: None,
        tool_overrides_update: None,
        respond_to,
        persist_ack: None,
        parsed_prompt_tx: None,
        queue_meta: None,
        send_now: false,
    };
    (item, rx)
}
/// A running-turn `AgentTask` stub: a 60s sleeper that keeps the turn "in
/// flight" until aborted. Assign to `state.running_task`; requires a
/// `LocalSet` (`spawn_local`).
#[cfg(test)]
pub(crate) fn running_task_stub(prompt_id: &str) -> AgentTask {
    AgentTask {
        prompt_id: prompt_id.to_string(),
        handle: tokio::task::spawn_local(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        })
        .abort_handle(),
    }
}
#[cfg(test)]
pub(crate) async fn build_actor() -> (
    std::sync::Arc<SessionActor>,
    tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) {
    let (gateway_tx, gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let actor =
        std::sync::Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
    (actor, gateway_rx)
}
/// A small valid inline PNG content block (survives normalization —
/// 32×32 = 1024 px clears the API's 512-total-pixel floor).
#[cfg(test)]
pub(crate) fn test_image_content() -> acp::ImageContent {
    use base64::Engine as _;
    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(32, 32, Rgba([128, 64, 32, 255]));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    acp::ImageContent::new(
        base64::engine::general_purpose::STANDARD.encode(&buf),
        "image/png".to_string(),
    )
}
#[cfg(test)]
pub(crate) fn set_goal_harness_for_tests(actor: &SessionActor) {
    actor
        .goal_harness_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(test)]
pub(crate) fn assert_goal_discipline_in_reminder(reminder: &str, site: &str) {
    let discipline_idx = reminder
        .find("<task_completion_discipline>")
        .unwrap_or_else(|| panic!("{site} must include <task_completion_discipline>:\n{reminder}"));
    let tracking_idx = reminder
        .find("TRACKING:")
        .unwrap_or_else(|| panic!("{site} must include TRACKING:\n{reminder}"));
    assert!(
        discipline_idx < tracking_idx,
        "{site} must place discipline before TRACKING (discipline={discipline_idx} tracking={tracking_idx}):\n{reminder}"
    );
    assert!(
        reminder.contains("</task_completion_discipline>\nTRACKING:"),
        "{site} must glue discipline directly before TRACKING:\n{reminder}"
    );
    for phrase in [
        "Tool-call first",
        "Don't ask permission to continue a task in flight",
        "Track multi-step work with a",
        "Don't stop with easy work left undone",
    ] {
        assert!(
            reminder.contains(phrase),
            "{site} must include discipline phrase `{phrase}`:\n{reminder}"
        );
    }
    assert_eq!(
        reminder.matches("</task_completion_discipline>").count(),
        1,
        "{site} must contain exactly one discipline closing tag:\n{reminder}"
    );
    assert!(
        !reminder.contains("{DISCIPLINE_BLOCK}"),
        "{site} must not leave {{DISCIPLINE_BLOCK}} unsubstituted:\n{reminder}"
    );
}
#[cfg(test)]
pub(crate) fn assert_resume_recap_discipline_tracking_order(text: &str, recap_marker: &str) {
    let recap_idx = text.find(recap_marker).unwrap_or_else(|| {
        panic!("resume reminder must include block recap `{recap_marker}`:\n{text}");
    });
    assert_goal_discipline_in_reminder(text, "goal_resume");
    let discipline_idx = text
        .find("<task_completion_discipline>")
        .expect("resume reminder must include discipline block");
    assert!(
        recap_idx < discipline_idx,
        "resume reminder must place recap before discipline (recap={recap_idx} discipline={discipline_idx}):\n{text}"
    );
}
