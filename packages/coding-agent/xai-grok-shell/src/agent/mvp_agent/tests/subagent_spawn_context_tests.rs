//! Subagent spawn-context inheritance: a child session must inherit the parent's
//! permission handle, goal-loop gate, and configured tool-overrides cutoff so policy,
//! run-state, and a backtest bound can't be bypassed by delegating to a subagent.
use super::{build_minimal_agent_for_tests, make_test_handle};
use agent_client_protocol as acp;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
/// Subagents inherit the parent permission handle, so a managed `Read(**/.env)`
/// deny still blocks the child — direct read and the `cat .env` shell equivalent.
#[tokio::test]
async fn subagent_spawn_context_inherits_parent_permission_handle() {
    use xai_grok_workspace::permission::types::{
        PatternMode, PermissionConfig, PermissionRule, RuleAction, ToolFilter,
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = build_minimal_agent_for_tests();
            let sid = acp::SessionId::new("parent-permission");
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let gateway = GatewaySender::new(tx);
            let cwd = xai_grok_paths::AbsPathBuf::new(std::path::PathBuf::from("/tmp"))
                .expect("absolute cwd");
            let (permission_handle, _events_rx) = xai_grok_workspace::permission::spawn_permission_manager(
                sid.clone(),
                gateway,
                cwd,
                xai_grok_workspace::permission::types::ClientType::Generic,
                Some(
                    PermissionConfig::new(
                        vec![PermissionRule {
                        action: RuleAction::Deny,
                        tool: ToolFilter::Read,
                        pattern: Some("**/.env".to_owned()),
                        pattern_mode: PatternMode::Glob,
                    }],
                    ),
                ),
                Vec::new(),
                Vec::new(),
                false,
                None,
            );
            let mut handle = make_test_handle("test-model", false, None);
            handle.permission_handle = permission_handle;
            agent.insert_resident(&sid, handle);
            let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
            let inherited = ctx
                .permission_handle
                .expect("subagent context must inherit parent permission handle");
            for access in [
                xai_grok_workspace::permission::AccessKind::Read(Some(".env".into())),
                xai_grok_workspace::permission::AccessKind::Bash("cat .env".into()),
            ] {
                let decision = inherited
                    .request(
                        access.clone(),
                        acp::ToolCallUpdate::new(
                            acp::ToolCallId::new("tc"),
                            Default::default(),
                        ),
                        Some("child-session".to_owned()),
                        Some("general-purpose".to_owned()),
                        Some("permission inheritance regression".to_owned()),
                    )
                    .await;
                assert!(
                    matches!(
                        decision,
                        xai_grok_workspace::permission::Decision::PolicyDeny(_)
                    ),
                    "subagent-inherited handle must enforce parent deny for {access:?}, got {decision:?}"
                );
            }
        })
        .await;
}
/// A subagent shares the parent's `goal_loop_active_gate` Arc, so flipping the
/// parent gate is observed through the child context (same allocation).
#[tokio::test]
async fn subagent_spawn_context_shares_parent_goal_loop_gate() {
    use std::sync::atomic::Ordering::Relaxed;
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("parent-goal");
    let handle = make_test_handle("test-model", false, None);
    let parent_gate = handle.tool_context.goal_loop_active_gate.clone();
    agent.insert_resident(&sid, handle);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert!(!ctx.goal_loop_active.load(Relaxed));
    parent_gate.store(true, Relaxed);
    assert!(
        ctx.goal_loop_active.load(Relaxed),
        "subagent context must observe the parent's goal-loop gate (same Arc)"
    );
}
/// A parent may expose `ask_user_question`, but that setting must never cross
/// the subagent boundary.
#[tokio::test]
async fn subagent_spawn_context_disables_ask_user_question_from_enabled_parent() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("parent-ask-enabled");
    let mut handle = make_test_handle("test-model", false, None);
    handle.ask_user_question_enabled = true;
    agent.insert_resident(&sid, handle);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert!(
        !ctx.ask_user_question_enabled,
        "subagent must not inherit the enabled parent ask_user_question gate"
    );
}
/// A subagent copies the parent's `non_interactive` flag, so a headless (`-p`)
/// parent's children omit interactive prompt guidance.
#[tokio::test]
async fn subagent_spawn_context_copies_parent_non_interactive() {
    let agent = build_minimal_agent_for_tests();
    let sid_headless = acp::SessionId::new("parent-headless");
    let mut handle_headless = make_test_handle("test-model", false, None);
    handle_headless.non_interactive = true;
    agent.insert_resident(&sid_headless, handle_headless);
    let ctx_headless = agent.build_subagent_spawn_context(sid_headless.0.as_ref());
    assert!(
        ctx_headless.parent_non_interactive,
        "subagent must copy the parent's non_interactive flag (headless -p parent)"
    );
    let sid_tui = acp::SessionId::new("parent-tui");
    agent.insert_resident(&sid_tui, make_test_handle("test-model", false, None));
    let ctx_tui = agent.build_subagent_spawn_context(sid_tui.0.as_ref());
    assert!(
        !ctx_tui.parent_non_interactive,
        "an interactive parent must not mark its subagents non-interactive"
    );
}
#[tokio::test]
async fn subagent_spawn_context_inherits_parent_configured_cutoff() {
    let agent = build_minimal_agent_for_tests();
    let cutoff = xai_grok_sampling_types::ToolOverrides {
        x_search: Some(xai_grok_sampling_types::XSearchOptions {
            date_bound: Some(
                xai_grok_sampling_types::SearchDateBound::new(None, Some("2020-01-01".to_string()))
                    .unwrap(),
            ),
        }),
        web_search: None,
    };
    let sid = acp::SessionId::new("parent-cutoff");
    let handle = make_test_handle("test-model", false, None);
    handle
        .resolved_tool_overrides
        .store(Some(std::sync::Arc::new(cutoff.clone())));
    agent.insert_resident(&sid, handle);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert_eq!(
        ctx.inherited_tool_overrides,
        Some(cutoff),
        "subagent context must inherit the parent's configured cutoff for its first-turn update"
    );
    let sid_none = acp::SessionId::new("parent-unbounded");
    agent.insert_resident(&sid_none, make_test_handle("test-model", false, None));
    let ctx_none = agent.build_subagent_spawn_context(sid_none.0.as_ref());
    assert!(
        ctx_none.inherited_tool_overrides.is_none(),
        "an unbounded parent must not hand a subagent a cutoff"
    );
}
/// A subagent inherits the parent's `process_scope`, so an owner enrolled through it stays visible via the child.
/// End-to-end reaping is covered by the spine's `process_scope_reclaim` tests.
#[tokio::test]
async fn subagent_spawn_context_inherits_parent_process_scope() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("parent-process-scope");
    let mut handle = make_test_handle("test-model", false, None);
    let parent_scope = xai_tty_utils::ProcessScope::new();
    handle.tool_context.process_scope = Some(parent_scope.clone());
    agent.insert_resident(&sid, handle);
    let owner = std::sync::Arc::new(xai_tty_utils::ProcessGroup::new().expect("process group"));
    parent_scope.register(&owner);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    let inherited = ctx
        .process_scope
        .expect("subagent context must inherit the parent's process scope");
    assert_eq!(
        inherited.live_count(),
        1,
        "the child sees the owner enrolled through the parent scope"
    );
}
fn model_entry_with_rate_limit(
    slug: &str,
    attempts: Option<u32>,
) -> crate::agent::config::ModelEntry {
    let mut info = crate::agent::config::ModelInfo::fallback(slug);
    info.subagent_rate_limit_max_attempts = attempts;
    crate::agent::config::ModelEntry {
        info,
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}
#[tokio::test]
async fn subagent_spawn_context_resolves_rate_limit_attempts_against_child_model() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("parent-rate-limit");
    agent.insert_resident(&sid, make_test_handle("parent-model", false, None));
    let mut ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    let mut models = indexmap::IndexMap::new();
    models.insert(
        "parent-model".to_string(),
        model_entry_with_rate_limit("parent-model", Some(4)),
    );
    models.insert(
        "child-model".to_string(),
        model_entry_with_rate_limit("child-model", Some(0)),
    );
    ctx.available_models = models;
    assert_eq!(
        ctx.resolve_subagent_rate_limit_max_attempts("child-model"),
        0,
        "a subagent on a different model must honor that model's disable (0), not the parent's"
    );
    assert_eq!(
        ctx.resolve_subagent_rate_limit_max_attempts("parent-model"),
        4,
        "the per-model lookup keys on the passed model id"
    );
}
/// Subagents share the parent's env > config > remote > default chain
/// (compiled default is `Segments`). Not pinned to `Summary`.
#[test]
#[serial_test::serial]
fn subagent_spawn_context_resolves_compaction_mode_like_parent() {
    use crate::agent::config::Config;
    use xai_chat_state::{CompactionDetail, CompactionMode};
    use xai_grok_test_support::EnvGuard;
    let _mode = EnvGuard::unset("GROK_COMPACTION_MODE");
    let _detail = EnvGuard::unset("GROK_COMPACTION_DETAIL");
    let mut ctx = crate::test_support::lsp_runtime::ctx_with_toggle(Default::default());
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::default(),
        "empty spawn context must inherit the compiled Segments default, not Summary"
    );
    let mut summary_cfg = Config::default();
    summary_cfg.features.compaction_mode = Some("summary".into());
    ctx.agent_config = Some(summary_cfg);
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::Summary,
        "parent [features].compaction_mode must win over remote/default"
    );
    ctx.agent_config = None;
    ctx.remote_settings = Some(crate::util::config::RemoteSettings {
        compaction_mode: Some("segments".into()),
        compaction_detail: Some("minimal".into()),
        ..Default::default()
    });
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::Segments(CompactionDetail::Minimal),
        "remote mode+detail must attach via with_segment_detail"
    );
    let mut segments_cfg = Config::default();
    segments_cfg.features.compaction_mode = Some("segments".into());
    segments_cfg.features.compaction_detail = Some("balanced".into());
    ctx.agent_config = Some(segments_cfg);
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::Segments(CompactionDetail::Balanced),
        "parent config detail must win over remote detail"
    );
    let _env_mode = EnvGuard::set("GROK_COMPACTION_MODE", "transcript");
    assert_eq!(
        ctx.resolve_compaction_mode(),
        CompactionMode::Transcript,
        "GROK_COMPACTION_MODE must win over parent config and remote"
    );
}
#[test]
fn run_shell_child_passes_parent_compaction_pins_into_spawn() {
    use crate::agent::subagent::SubagentSpawnContext;
    use crate::session::CompactionPins;
    use xai_chat_state::CompactionMode;
    use xai_grok_agent::prompt::user_message::UserMessageTemplate;
    let default_child = UserMessageTemplate::Default;
    let mut ctx = crate::test_support::lsp_runtime::ctx_with_toggle(Default::default());
    ctx.parent_compaction = CompactionPins {
        mode: CompactionMode::default(),
        two_pass: true,
    };
    assert_eq!(
        ctx.compaction_pins_for_child(&default_child),
        CompactionPins {
            mode: CompactionMode::default(),
            two_pass: true,
        },
    );
    ctx.parent_compaction = CompactionPins {
        mode: CompactionMode::Summary,
        two_pass: false,
    };
    assert_eq!(
        ctx.compaction_pins_for_child(&default_child),
        CompactionPins {
            mode: CompactionMode::Summary,
            two_pass: false,
        },
    );
    ctx.parent_compaction = CompactionPins {
        mode: CompactionMode::default(),
        two_pass: true,
    };
    assert_eq!(
        SubagentSpawnContext::snapshot_parent_compaction_pins(
            CompactionMode::default(),
            true,
            Some("grok-build"),
            Some("grok-build"),
            std::path::Path::new("/tmp"),
        ),
        CompactionPins {
            mode: CompactionMode::default(),
            two_pass: true,
        },
    );
    let spawn_src = include_str!("../../subagent/handle_request.rs");
    assert!(
        spawn_src.contains("ctx.compaction_pins_for_child(&definition.user_message_template)"),
        "run_shell_child must pass compaction_pins_for_child into spawn"
    );
    assert!(
        spawn_src.contains("pins.two_pass"),
        "run_shell_child must pass pins.two_pass into spawn_session_on_thread"
    );
    assert!(
        !spawn_src.contains("CompactionMode::Summary"),
        "run_shell_child must not hard-pin Summary at the spawn site"
    );
    assert!(
        !spawn_src.contains("two_pass_enabled = false")
            && !spawn_src.contains("false, // two_pass"),
        "run_shell_child must not hard-pin two-pass off at the spawn site"
    );
}
