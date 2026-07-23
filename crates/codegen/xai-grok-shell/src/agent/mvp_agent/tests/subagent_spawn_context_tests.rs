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
            let (permission_handle, _events_rx) =
                xai_grok_workspace::permission::spawn_permission_manager(
                    sid.clone(),
                    gateway,
                    cwd,
                    xai_grok_workspace::permission::types::ClientType::Generic,
                    Some(PermissionConfig::new(vec![PermissionRule {
                        action: RuleAction::Deny,
                        tool: ToolFilter::Read,
                        pattern: Some("**/.env".to_owned()),
                        pattern_mode: PatternMode::Glob,
                    }])),
                    Vec::new(), // deny_read_globs
                    Vec::new(),
                    false,
                    None,
                );

            let mut handle = make_test_handle("test-model", false, None);
            handle.permission_handle = permission_handle;
            agent.sessions.borrow_mut().insert(sid.clone(), handle);

            let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
            let inherited = ctx
                .permission_handle
                .expect("subagent context must inherit parent permission handle");

            // Direct file read and the shell equivalent both hit the parent deny.
            for access in [
                xai_grok_workspace::permission::AccessKind::Read(Some(".env".into())),
                xai_grok_workspace::permission::AccessKind::Bash("cat .env".into()),
            ] {
                let decision = inherited
                    .request(
                        access.clone(),
                        acp::ToolCallUpdate::new(acp::ToolCallId::new("tc"), Default::default()),
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
    // Clone the parent's live gate before the handle moves into `sessions`.
    let parent_gate = handle.tool_context.goal_loop_active_gate.clone();
    agent.sessions.borrow_mut().insert(sid.clone(), handle);

    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());

    // Flipping the parent gate must surface through the child flag (shared Arc).
    assert!(!ctx.goal_loop_active.load(Relaxed));
    parent_gate.store(true, Relaxed);
    assert!(
        ctx.goal_loop_active.load(Relaxed),
        "subagent context must observe the parent's goal-loop gate (same Arc)"
    );
}

/// A subagent inherits the parent session's `ask_user_question` gate, so
/// `--no-ask-user` strips the tool from subagents too, while the default keeps it.
#[tokio::test]
async fn subagent_spawn_context_inherits_parent_ask_user_question_gate() {
    let agent = build_minimal_agent_for_tests();

    // Parent with the tool disabled (the `--no-ask-user` case) → child off.
    let sid_off = acp::SessionId::new("parent-no-ask");
    let mut handle_off = make_test_handle("test-model", false, None);
    handle_off.ask_user_question_enabled = false;
    agent
        .sessions
        .borrow_mut()
        .insert(sid_off.clone(), handle_off);
    let ctx_off = agent.build_subagent_spawn_context(sid_off.0.as_ref());
    assert!(
        !ctx_off.ask_user_question_enabled,
        "subagent must inherit the parent's disabled ask_user_question gate (--no-ask-user)"
    );

    // Parent with the tool enabled (the default) → child on.
    let sid_on = acp::SessionId::new("parent-ask");
    let handle_on = make_test_handle("test-model", false, None);
    agent
        .sessions
        .borrow_mut()
        .insert(sid_on.clone(), handle_on);
    let ctx_on = agent.build_subagent_spawn_context(sid_on.0.as_ref());
    assert!(
        ctx_on.ask_user_question_enabled,
        "subagent must inherit the parent's enabled ask_user_question gate"
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
    agent.sessions.borrow_mut().insert(sid.clone(), handle);
    let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
    assert_eq!(
        ctx.inherited_tool_overrides,
        Some(cutoff),
        "subagent context must inherit the parent's configured cutoff for its first-turn update"
    );

    // A parent with no configured cutoff must not fabricate one for the child.
    let sid_none = acp::SessionId::new("parent-unbounded");
    agent.sessions.borrow_mut().insert(
        sid_none.clone(),
        make_test_handle("test-model", false, None),
    );
    let ctx_none = agent.build_subagent_spawn_context(sid_none.0.as_ref());
    assert!(
        ctx_none.inherited_tool_overrides.is_none(),
        "an unbounded parent must not hand a subagent a cutoff"
    );
}
