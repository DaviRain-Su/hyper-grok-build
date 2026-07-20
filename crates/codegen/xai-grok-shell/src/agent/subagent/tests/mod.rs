#![cfg_attr(rustfmt, rustfmt::skip)]
use super::*;
use crate::test_support::lsp_runtime::{
    DummyLspDispatch, ctx_with_toggle, make_request, test_gateway,
};
/// Invariant: resolving a subagent applies the parent session's
/// `--tools`/`--disallowed-tools`/`--permission-mode` — driven through
/// `resolve_agent_definition` so the spawn path can't skip them.
#[tokio::test]
async fn subagent_inherits_session_cli_overrides() {
    use xai_grok_agent::config::{AgentDefinition, PermissionMode};
    let mut probe = AgentDefinition::general_purpose();
    probe.name = "session-override-probe".into();
    probe.permission_mode = PermissionMode::Plan;
    probe.disallowed_tools = vec!["write".into()];
    let mut config = crate::agent::config::Config::default();
    config.cli_agents = vec![probe];
    config.cli_agent_overrides = crate::agent::config::CliAgentOverrides {
        tools: Some(vec!["read_file".into(), "grep".into()]),
        disallowed_tools: Some(vec!["web_search".into(), "write".into()]),
        permission_mode: Some(PermissionMode::AcceptEdits),
        ..Default::default()
    };
    let mut ctx = ctx_with_toggle(std::collections::HashMap::new());
    ctx.agent_config = Some(config);
    let def = resolve_agent_definition("session-override-probe", &ctx)
        .expect("cli agent resolves");
    assert_eq!(
        def.session_tools_allowlist.as_deref(), Some(& ["read_file".into(), "grep"
        .into()] [..])
    );
    assert_eq!(
        def.session_tools_denylist.as_deref(), Some(& ["web_search".into(), "write"
        .into()] [..])
    );
    assert_eq!(def.disallowed_tools, vec!["write"]);
    assert_eq!(def.permission_mode, PermissionMode::AcceptEdits);
}
/// `permissionMode: bypassPermissions` is downgraded to `Default` under the
/// pin and honored without it; other modes and plugin stripping unaffected.
#[test]
fn subagent_bypass_permission_mode_gated_by_policy_pin() {
    use xai_grok_agent::config::PermissionMode;
    const PIN: &str = xai_grok_workspace::permission::resolution::YOLO_PIN_REASON_REQUIREMENTS;
    assert_eq!(
        resolve_subagent_permission_mode(PermissionMode::BypassPermissions, false, None),
        PermissionMode::BypassPermissions,
    );
    assert_eq!(
        resolve_subagent_permission_mode(PermissionMode::BypassPermissions, false,
        Some(PIN)), PermissionMode::Default,
    );
    assert_eq!(
        resolve_subagent_permission_mode(PermissionMode::Plan, false, Some(PIN)),
        PermissionMode::Plan,
    );
    assert_eq!(
        resolve_subagent_permission_mode(PermissionMode::BypassPermissions, true, None),
        PermissionMode::Default,
    );
}
/// Persisted⇒stamped chokepoint for the subagent emitter: the
/// `SessionCommand` persist hop and the live broadcast must carry the
/// SAME `eventId`, minted before the fork (divergent or missing ids
/// degrade cursor reconnects to full replays or re-applied lines).
#[tokio::test]
async fn emit_subagent_notification_stamps_one_event_id_on_both_paths() {
    use crate::test_support::lsp_runtime::test_gateway_with_receiver;
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    emit_subagent_notification(
        &gateway,
        "parent-sess",
        SessionUpdate::SubagentFinished {
            subagent_id: "sa-1".into(),
            child_session_id: "child-1".into(),
            status: "completed".into(),
            error: None,
            tool_calls: 0,
            turns: 0,
            duration_ms: 5,
            tokens_used: 0,
            output: None,
            will_wake: false,
        },
        Some(&cmd_tx),
    );
    let persisted_id = match cmd_rx.try_recv().expect("persist hop must fire") {
        SessionCommand::XaiSessionNotification { notification } => {
            notification
                .meta
                .as_ref()
                .and_then(|m| m.get("eventId"))
                .and_then(|v| v.as_str())
                .expect("persisted subagent lines must carry an eventId")
                .to_string()
        }
        _ => panic!("expected XaiSessionNotification"),
    };
    assert!(persisted_id.starts_with("parent-sess-"));
    let broadcast_id = match gateway_rx.try_recv().expect("broadcast must fire") {
        xai_acp_lib::AcpClientMessage::ExtNotification(args) => {
            let params: serde_json::Value = serde_json::from_str(
                    args.request.params.get(),
                )
                .unwrap();
            params["_meta"]["eventId"].as_str().unwrap().to_string()
        }
        _ => panic!("expected ExtNotification"),
    };
    assert_eq!(persisted_id, broadcast_id);
}
#[test]
fn subagent_max_turns_definition_wins_else_inherits_parent() {
    assert_eq!(super::resolve_subagent_max_turns(Some(2), Some(5)), Some(2));
    assert_eq!(super::resolve_subagent_max_turns(None, Some(5)), Some(5));
}
#[test]
fn resume_worktree_action_covers_three_outcomes() {
    use super::{ResumeWorktreeAction, resume_worktree_action};
    assert_eq!(
        resume_worktree_action(true, Some("refs/grok/subagents/x")),
        ResumeWorktreeAction::Rehydrate
    );
    assert_eq!(
        resume_worktree_action(false, Some("refs/grok/subagents/x")),
        ResumeWorktreeAction::Rehydrate
    );
    assert_eq!(resume_worktree_action(true, None), ResumeWorktreeAction::Reuse);
    assert_eq!(resume_worktree_action(false, None), ResumeWorktreeAction::Shared);
}
#[test]
fn subagent_inherits_parent_lsp_via_context() {
    let parent: std::sync::Arc<dyn xai_grok_tools::implementations::lsp::LspBackend> = Arc::new(
        DummyLspDispatch,
    );
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.lsp = Some(parent.clone());
    assert!(ctx.lsp.is_some());
    assert_eq!(
        Arc::as_ptr(& parent), Arc::as_ptr(ctx.lsp.as_ref().unwrap()),
        "child should inherit parent LSP via context"
    );
}
#[test]
fn subagent_inherits_managed_mcp_state_via_context() {
    let handle = crate::session::managed_mcp::ManagedMcpStateHandle::default();
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.managed_mcp_state = handle.clone();
    assert!(
        Arc::ptr_eq(& handle, & ctx.managed_mcp_state),
        "child should share parent's managed MCP state (Arc identity)"
    );
}
#[test]
fn no_parent_lsp_means_child_gets_none() {
    let ctx = ctx_with_toggle(HashMap::new());
    assert!(ctx.lsp.is_none());
}
#[test]
fn is_subagent_enabled_returns_true_for_absent_names() {
    let ctx = ctx_with_toggle(HashMap::from([("plan".to_string(), false)]));
    assert!(ctx.is_subagent_enabled("explore"), "absent key should default to enabled");
    assert!(
        ctx.is_subagent_enabled("general-purpose"),
        "absent key should default to enabled"
    );
    assert!(
        ctx.is_subagent_enabled("custom-agent"), "absent key should default to enabled"
    );
}
#[test]
fn is_subagent_enabled_returns_false_for_disabled_names() {
    let ctx = ctx_with_toggle(
        HashMap::from([
            ("plan".to_string(), false),
            ("code-reviewer".to_string(), false),
            ("explore".to_string(), true),
        ]),
    );
    assert!(! ctx.is_subagent_enabled("plan"), "plan = false should be disabled");
    assert!(
        ! ctx.is_subagent_enabled("code-reviewer"),
        "code-reviewer = false should be disabled"
    );
    assert!(ctx.is_subagent_enabled("explore"), "explore = true should be enabled");
}
#[test]
fn lookup_returns_none_for_unknown_id() {
    let coordinator = SubagentCoordinator::new();
    assert!(coordinator.lookup("nonexistent").is_none());
}
#[test]
fn lookup_returns_ready_for_completed_subagent() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .move_to_completed(
            "sub-1",
            "test task".to_string(),
            "explore".to_string(),
            SubagentResult {
                success: true,
                output: std::sync::Arc::from("found 3 files"),
                subagent_id: "sub-1".to_string(),
                child_session_id: "sub-1".to_string(),
                tool_calls: 5,
                turns: 2,
                duration_ms: 1234,
                ..Default::default()
            },
            None,
        );
    let lookup = coordinator.lookup("sub-1");
    assert!(lookup.is_some());
    assert!(
        matches!(lookup, Some(SnapshotLookup::Ready(ref snap)) if snap.subagent_id ==
        "sub-1"), "completed subagent should return Ready variant"
    );
}
#[tokio::test]
async fn resolve_snapshot_returns_none_for_none_input() {
    let result = resolve_snapshot(None).await;
    assert!(result.is_none());
}
#[tokio::test]
async fn resolve_snapshot_returns_ready_unchanged() {
    let snap = SubagentSnapshot {
        subagent_id: "sub-1".to_string(),
        description: "test".to_string(),
        subagent_type: "explore".to_string(),
        status: SubagentSnapshotStatus::Completed {
            output: "done".to_string(),
            tool_calls: 3,
            turns: 1,
            worktree_path: None,
        },
        started_at_epoch_ms: 0,
        duration_ms: 100,
        persona: None,
    };
    let result = resolve_snapshot(Some(SnapshotLookup::Ready(snap))).await;
    let result = result.expect("Ready should resolve to Some");
    assert_eq!(result.subagent_id, "sub-1");
    assert!(matches!(result.status, SubagentSnapshotStatus::Completed { .. }));
}
#[tokio::test]
async fn resolve_snapshot_populates_running_from_signals() {
    use crate::session::signals::SessionSignalsHandle;
    let signals_handle = SessionSignalsHandle::new();
    signals_handle.increment_turn();
    signals_handle.record_tool_call("bash");
    signals_handle.record_tool_call("read_file");
    signals_handle.record_tool_call("bash");
    tokio::task::yield_now().await;
    let seed = RunningSnapshotSeed {
        subagent_id: "sub-running".to_string(),
        description: "running task".to_string(),
        subagent_type: "general-purpose".to_string(),
        started_at_epoch_ms: 1000,
        duration_ms: 5000,
        persona: None,
        signals_handle,
    };
    let result = resolve_snapshot(Some(SnapshotLookup::NeedsSignals(seed))).await;
    let snap = result.expect("NeedsSignals should resolve to Some");
    assert_eq!(snap.subagent_id, "sub-running");
    assert_eq!(snap.duration_ms, 5000);
    match &snap.status {
        SubagentSnapshotStatus::Running {
            turn_count,
            tool_call_count,
            tools_used,
            ..
        } => {
            assert_eq!(* turn_count, 1, "should have 1 turn");
            assert_eq!(* tool_call_count, 3, "should have 3 tool calls");
            assert!(
                tools_used.contains(& "bash".to_string()),
                "tools_used should contain bash"
            );
            assert!(
                tools_used.contains(& "read_file".to_string()),
                "tools_used should contain read_file"
            );
        }
        other => panic!("expected Running, got {other:?}"),
    }
}
#[test]
fn is_running_returns_true_for_running_variant() {
    let snap = SubagentSnapshot {
        subagent_id: "s".to_string(),
        description: "d".to_string(),
        subagent_type: "t".to_string(),
        status: SubagentSnapshotStatus::Running {
            turn_count: 0,
            tool_call_count: 0,
            tokens_used: 0,
            context_window_tokens: 0,
            context_usage_pct: 0,
            tools_used: vec![],
            error_count: 0,
        },
        started_at_epoch_ms: 0,
        duration_ms: 0,
        persona: None,
    };
    assert!(is_running(& snap));
}
#[test]
fn is_running_returns_false_for_completed_variant() {
    let snap = SubagentSnapshot {
        subagent_id: "s".to_string(),
        description: "d".to_string(),
        subagent_type: "t".to_string(),
        status: SubagentSnapshotStatus::Completed {
            output: "done".to_string(),
            tool_calls: 0,
            turns: 0,
            worktree_path: None,
        },
        started_at_epoch_ms: 0,
        duration_ms: 0,
        persona: None,
    };
    assert!(! is_running(& snap));
}
#[test]
fn lookup_returns_initializing_for_pending_subagent() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-pending".to_string(),
            subagent_type: "general-purpose".to_string(),
            description: "pending task".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: false,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    let lookup = coordinator.lookup("sub-pending");
    assert!(
        matches!(lookup, Some(SnapshotLookup::Ready(ref snap)) if snap.subagent_id ==
        "sub-pending" && matches!(snap.status, SubagentSnapshotStatus::Initializing)),
        "pending subagent should return Ready(Initializing)"
    );
}
/// The running gauge must track `pending.len() + active.len()` through the
/// full lifecycle: it feeds `AgentActivity::is_busy`, which gates the
/// leader auto-update shutdown.
#[tokio::test]
async fn running_gauge_tracks_pending_and_active() {
    use std::sync::atomic::Ordering;
    let mut coordinator = SubagentCoordinator::new();
    let gauge = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    coordinator.set_running_gauge(gauge.clone());
    assert_eq!(gauge.load(Ordering::Relaxed), 0);
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-gauge".to_string(),
            subagent_type: "general-purpose".to_string(),
            description: "gauge task".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: true,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    assert_eq!(gauge.load(Ordering::Relaxed), 1, "pending counts as running");
    coordinator
        .insert(
            dummy_tracker("sub-gauge", "parent-session", "general-purpose", "gauge task"),
        );
    assert_eq!(gauge.load(Ordering::Relaxed), 1, "active counts as running");
    coordinator
        .move_to_completed(
            "sub-gauge",
            "gauge task".into(),
            "general-purpose".into(),
            SubagentResult::default(),
            None,
        );
    assert_eq!(gauge.load(Ordering::Relaxed), 0, "completed does not count");
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-gauge-2".to_string(),
            subagent_type: "general-purpose".to_string(),
            description: "gauge task 2".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: true,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    assert_eq!(gauge.load(Ordering::Relaxed), 1);
    coordinator.move_pending_to_failed("sub-gauge-2", "worktree setup failed");
    assert_eq!(gauge.load(Ordering::Relaxed), 0);
    let late_gauge = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-gauge-3".to_string(),
            subagent_type: "general-purpose".to_string(),
            description: "gauge task 3".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: true,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    coordinator.set_running_gauge(late_gauge.clone());
    assert_eq!(late_gauge.load(Ordering::Relaxed), 1);
}
#[test]
fn mark_block_waited_sets_flag_on_completed() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .move_to_completed(
            "sub-bw",
            "test".into(),
            "explore".into(),
            SubagentResult {
                success: true,
                subagent_id: "sub-bw".into(),
                child_session_id: "sub-bw".into(),
                ..Default::default()
            },
            None,
        );
    assert!(! coordinator.is_block_waited("sub-bw"));
    coordinator.mark_block_waited("sub-bw");
    assert!(coordinator.is_block_waited("sub-bw"));
}
#[test]
fn is_block_waited_returns_false_for_unknown_id() {
    let coordinator = SubagentCoordinator::new();
    assert!(! coordinator.is_block_waited("nonexistent"));
}
/// Race condition: caller cancels the blocking wait
/// (receiver dropped) and the subagent completes before the query poll
/// loop's next 200ms tick clears the flag. The completion handler's
/// decision-time check must see the dead slot, clear `block_waited`,
/// and let the auto-wake fire.
#[tokio::test]
async fn block_wait_decision_wakes_when_waiter_cancelled_before_poll_tick() {
    let mut coordinator = SubagentCoordinator::new();
    let tracker = dummy_tracker("sub-race", "session-A", "explore", "bg task");
    coordinator.insert(tracker);
    let (tx, rx) = oneshot::channel::<Option<SubagentSnapshot>>();
    let slot: BlockWaitSlot = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    coordinator.register_block_wait("sub-race", slot.clone());
    assert!(coordinator.is_block_waited("sub-race"));
    drop(rx);
    assert!(coordinator.is_block_waited("sub-race"));
    assert!(
        ! coordinator.block_wait_delivered_or_live("sub-race"),
        "cancelled waiter must not suppress the completion auto-wake"
    );
    assert!(
        ! coordinator.is_block_waited("sub-race"),
        "decision must clear the stale block_waited flag"
    );
}
/// A live waiter (receiver still open) keeps the wake suppressed — the
/// poll loop will deliver the result within one tick.
#[tokio::test]
async fn block_wait_decision_suppresses_for_live_waiter() {
    let mut coordinator = SubagentCoordinator::new();
    let tracker = dummy_tracker("sub-live", "session-A", "explore", "bg task");
    coordinator.insert(tracker);
    let (tx, _rx) = oneshot::channel::<Option<SubagentSnapshot>>();
    let slot: BlockWaitSlot = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    coordinator.register_block_wait("sub-live", slot.clone());
    assert!(
        coordinator.block_wait_delivered_or_live("sub-live"),
        "live waiter will receive the result — wake would be redundant"
    );
    assert!(coordinator.is_block_waited("sub-live"), "flag stays set for a live waiter");
}
/// A consumed sender (result already delivered) keeps the wake
/// suppressed even though the registration is gone.
#[tokio::test]
async fn block_wait_decision_suppresses_after_delivery() {
    let mut coordinator = SubagentCoordinator::new();
    let tracker = dummy_tracker("sub-dlv", "session-A", "explore", "bg task");
    coordinator.insert(tracker);
    let (tx, mut rx) = oneshot::channel::<Option<SubagentSnapshot>>();
    let slot: BlockWaitSlot = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    coordinator.register_block_wait("sub-dlv", slot.clone());
    let tx = slot.borrow_mut().take().expect("sender parked");
    let _ = tx.send(None);
    assert!(rx.try_recv().is_ok(), "receiver got the result");
    coordinator.unregister_block_wait("sub-dlv", &slot);
    assert!(
        coordinator.block_wait_delivered_or_live("sub-dlv"),
        "already-delivered result must keep the wake suppressed"
    );
}
#[tokio::test]
async fn mark_explicitly_killed_active_then_propagates_to_completed() {
    let mut coordinator = SubagentCoordinator::new();
    let tracker = dummy_tracker("sub-ek", "session-A", "explore", "bg task");
    coordinator.insert(tracker);
    assert!(! coordinator.is_explicitly_killed("sub-ek"));
    coordinator.mark_explicitly_killed("sub-ek");
    assert!(coordinator.is_explicitly_killed("sub-ek"));
    coordinator
        .move_to_completed(
            "sub-ek",
            "bg task".into(),
            "explore".into(),
            SubagentResult {
                success: false,
                cancelled: true,
                subagent_id: "sub-ek".into(),
                child_session_id: "sub-ek".into(),
                ..Default::default()
            },
            None,
        );
    assert!(
        coordinator.is_explicitly_killed("sub-ek"),
        "flag must propagate from active tracker to completed entry"
    );
}
#[test]
fn should_auto_wake_subagent_requires_background_and_enabled() {
    assert!(! should_auto_wake_subagent(false, false, true, false, false, false, true));
    assert!(! should_auto_wake_subagent(true, false, false, false, false, false, true));
    assert!(should_auto_wake_subagent(true, false, true, false, false, false, true));
}
/// A cancelled child never wakes the parent — most acutely the Ctrl+C
/// race where `ParentGone` backgrounds a foreground child moments before
/// the teardown cancel lands its token.
#[test]
fn should_auto_wake_subagent_refuses_cancelled_results() {
    assert!(! should_auto_wake_subagent(true, true, true, false, false, false, true));
}
#[test]
fn should_auto_wake_subagent_suppressed_by_block_waited_or_killed() {
    assert!(! should_auto_wake_subagent(true, false, true, true, false, false, true));
    assert!(! should_auto_wake_subagent(true, false, true, false, true, false, true));
    assert!(! should_auto_wake_subagent(true, false, true, true, true, false, true));
}
/// A goal loop active in the parent suppresses the subagent
/// auto-wake synthetic prompt — the structural sibling of the bash gate.
/// Skipping the inject here also skips its completion reservation, so the
/// per-tool-call / between-turn surfaces stay free to drain the completion.
#[test]
fn should_auto_wake_subagent_suppressed_by_goal_loop() {
    assert!(! should_auto_wake_subagent(true, false, true, false, false, true, true));
    assert!(should_auto_wake_subagent(true, false, true, false, false, false, true));
}
#[test]
fn should_auto_wake_subagent_requires_open_parent_channel() {
    assert!(! should_auto_wake_subagent(true, false, true, false, false, false, false));
}
fn auto_wake_test_request(id: &str) -> SubagentRequest {
    let (result_tx, _result_rx) = oneshot::channel();
    SubagentRequest {
        id: id.into(),
        prompt: String::new(),
        description: "explore".into(),
        subagent_type: "general-purpose".into(),
        parent_session_id: "parent".into(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: true,
        surface_completion: true,
        fork_context: false,
        result_tx,
    }
}
/// Behavior-level: the action half of the subagent auto-wake.
/// When the gate lets it run, `inject_subagent_completed_prompt` sends the
/// synthetic `Prompt` to the parent and reserves its completion ID.
/// Paired with `should_auto_wake_subagent_suppressed_by_goal_loop`, this
/// proves the full Gap-1 contract on the subagent surface: goal active →
/// gate false → this never runs (no prompt, not marked, so surfaces 2/3
/// drain it); goal inactive → gate true → it runs (today's behavior).
#[test]
fn inject_subagent_completed_prompt_sends_prompt_and_marks_delivered() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let reservations = xai_grok_tools::reminders::task_completion::TaskCompletionReservations::default();
    let request = auto_wake_test_request("sa-1");
    let result = SubagentResult {
        success: true,
        subagent_id: "sa-1".into(),
        child_session_id: "sa-1".into(),
        ..Default::default()
    };
    inject_subagent_completed_prompt(
        "sa-1",
        &result,
        &request,
        &Some(reservations.clone()),
        Some(&cmd_tx),
        "get_command_or_subagent_output",
        &None,
    );
    match cmd_rx.try_recv().expect("expected synthetic Prompt") {
        SessionCommand::Prompt { prompt_id, verbatim, .. } => {
            assert!(prompt_id.starts_with("subagent-completed-"));
            assert!(verbatim);
        }
        _ => panic!("expected SessionCommand::Prompt"),
    }
    assert_eq!(reservations.snapshot(), vec!["sa-1".to_string()]);
}
#[test]
fn inject_subagent_completed_prompt_releases_reservation_when_parent_closed() {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    drop(cmd_rx);
    let reservations = xai_grok_tools::reminders::task_completion::TaskCompletionReservations::default();
    reservations.reserve("sa-closed".into());
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    inject_subagent_completed_prompt(
        "sa-closed",
        &SubagentResult {
            success: true,
            subagent_id: "sa-closed".into(),
            child_session_id: "sa-closed".into(),
            ..Default::default()
        },
        &auto_wake_test_request("sa-closed"),
        &Some(reservations.clone()),
        Some(&cmd_tx),
        "get_command_or_subagent_output",
        &Some(trace_tx),
    );
    assert!(
        reservations.contains("sa-closed"),
        "send failure must release only the reservation acquired by this attempt"
    );
    reservations.release("sa-closed");
    assert!(! reservations.contains("sa-closed"));
    assert!(trace_rx.try_recv().is_err());
}
#[test]
fn mark_explicitly_killed_sets_flag_on_completed() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .move_to_completed(
            "sub-ek-c",
            "test".into(),
            "explore".into(),
            SubagentResult {
                success: true,
                subagent_id: "sub-ek-c".into(),
                child_session_id: "sub-ek-c".into(),
                ..Default::default()
            },
            None,
        );
    assert!(! coordinator.is_explicitly_killed("sub-ek-c"));
    coordinator.mark_explicitly_killed("sub-ek-c");
    assert!(coordinator.is_explicitly_killed("sub-ek-c"));
}
#[test]
fn is_explicitly_killed_returns_false_for_unknown_id() {
    let coordinator = SubagentCoordinator::new();
    assert!(! coordinator.is_explicitly_killed("nonexistent"));
}
#[tokio::test]
async fn block_waited_propagates_through_move_to_completed() {
    let mut coordinator = SubagentCoordinator::new();
    let mut tracker = dummy_tracker("sub-prop", "session-A", "explore", "bg task");
    tracker.block_waited = true;
    coordinator.insert(tracker);
    coordinator
        .move_to_completed(
            "sub-prop",
            "bg task".into(),
            "explore".into(),
            SubagentResult {
                success: true,
                subagent_id: "sub-prop".into(),
                child_session_id: "sub-prop".into(),
                ..Default::default()
            },
            None,
        );
    assert!(coordinator.is_block_waited("sub-prop"));
}
fn complete_dummy(coordinator: &mut SubagentCoordinator, id: &str, surface: bool) {
    let mut tracker = dummy_tracker(id, "session-A", "explore", "task");
    tracker.surface_completion = surface;
    coordinator.insert(tracker);
    coordinator
        .move_to_completed(
            id,
            "task".into(),
            "explore".into(),
            SubagentResult {
                success: true,
                subagent_id: id.into(),
                child_session_id: id.into(),
                ..Default::default()
            },
            None,
        );
}
#[tokio::test]
async fn move_to_completed_surfaces_when_flag_true() {
    let mut coordinator = SubagentCoordinator::new();
    complete_dummy(&mut coordinator, "sub-surface", true);
    let drained = coordinator.drain_pending_completions();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].subagent_id, "sub-surface");
}
#[tokio::test]
async fn move_to_completed_skips_buffer_when_flag_false() {
    let mut coordinator = SubagentCoordinator::new();
    complete_dummy(&mut coordinator, "sub-hidden", false);
    assert!(coordinator.drain_pending_completions().is_empty());
    assert!(coordinator.lookup("sub-hidden").is_some());
}
fn fail_pending(coordinator: &mut SubagentCoordinator, id: &str, surface: bool) {
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: id.to_string(),
            subagent_type: "explore".to_string(),
            description: "task".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: false,
            surface_completion: surface,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    coordinator.move_pending_to_failed(id, "boom");
}
#[test]
fn failure_completion_surfaces_when_flag_true() {
    let mut coordinator = SubagentCoordinator::new();
    fail_pending(&mut coordinator, "fail-surface", true);
    let drained = coordinator.drain_pending_completions();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].subagent_id, "fail-surface");
    assert!(! drained[0].success);
}
#[test]
fn failure_completion_skips_buffer_when_flag_false() {
    let mut coordinator = SubagentCoordinator::new();
    fail_pending(&mut coordinator, "fail-hidden", false);
    assert!(coordinator.drain_pending_completions().is_empty());
    assert!(coordinator.lookup("fail-hidden").is_some());
}
#[test]
fn is_running_returns_true_for_initializing_variant() {
    let snap = SubagentSnapshot {
        subagent_id: "s".to_string(),
        description: "d".to_string(),
        subagent_type: "t".to_string(),
        status: SubagentSnapshotStatus::Initializing,
        started_at_epoch_ms: 0,
        duration_ms: 0,
        persona: None,
    };
    assert!(is_running(& snap));
}
#[test]
fn remove_pending_clears_entry() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-1".to_string(),
            subagent_type: "explore".to_string(),
            description: "test".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: false,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    assert!(coordinator.lookup("sub-1").is_some());
    coordinator.remove_pending("sub-1");
    assert!(
        coordinator.lookup("sub-1").is_none(),
        "pending entry should be gone after remove_pending"
    );
}
#[test]
fn move_pending_to_failed_creates_completed_entry() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-fail".to_string(),
            subagent_type: "explore".to_string(),
            description: "will fail during init".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: true,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    coordinator.move_pending_to_failed("sub-fail", "Sampling client error: bad config");
    assert!(! coordinator.pending.contains_key("sub-fail"));
    let lookup = coordinator.lookup("sub-fail");
    assert!(lookup.is_some(), "failed subagent should be queryable");
    match lookup.unwrap() {
        SnapshotLookup::Ready(snap) => {
            assert_eq!(snap.subagent_id, "sub-fail");
            assert!(
                matches!(snap.status, SubagentSnapshotStatus::Failed { .. }),
                "status should be Failed"
            );
            if let SubagentSnapshotStatus::Failed { error } = &snap.status {
                assert!(
                    error.contains("Sampling client error"),
                    "error should contain specific message, got: {error}"
                );
            }
        }
        _ => panic!("expected Ready snapshot for completed-as-failed subagent"),
    }
}
#[test]
fn move_pending_to_failed_fires_completion_notify() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-notify".to_string(),
            subagent_type: "explore".to_string(),
            description: "notify test".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: true,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    coordinator.move_pending_to_failed("sub-notify", "test error");
    let summaries = coordinator.drain_pending_completions();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].subagent_id, "sub-notify");
    assert!(! summaries[0].success);
}
#[test]
fn move_pending_to_failed_noop_for_unknown_id() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator.move_pending_to_failed("nonexistent", "error");
    assert!(coordinator.completed.is_empty());
}
#[test]
fn move_pending_to_cancelled_creates_cancelled_entry() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-killed".to_string(),
            subagent_type: "explore".to_string(),
            description: "killed while initializing".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: true,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    coordinator.move_pending_to_cancelled("sub-killed", "Subagent was cancelled");
    assert!(! coordinator.pending.contains_key("sub-killed"));
    match coordinator.lookup("sub-killed") {
        Some(SnapshotLookup::Ready(snap)) => {
            assert!(
                matches!(snap.status, SubagentSnapshotStatus::Cancelled { .. }),
                "killed-while-pending should be Cancelled, got {:?}", snap.status
            )
        }
        _ => {
            panic!("expected Ready cancelled snapshot for killed-while-pending subagent")
        }
    }
}
fn completed_with_output(
    id: &str,
    text: &str,
    persisted_output_dir: Option<PathBuf>,
) -> CompletedSubagent {
    CompletedSubagent {
        subagent_id: id.into(),
        parent_session_id: String::new(),
        parent_prompt_id: None,
        child_session_id: String::new(),
        description: "task".into(),
        subagent_type: "explore".into(),
        persona: None,
        started_at: std::time::Instant::now(),
        completed_at: std::time::Instant::now(),
        result: SubagentResult {
            success: true,
            output: std::sync::Arc::from(text),
            ..Default::default()
        },
        resumed_from: None,
        child_cwd: String::new(),
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: String::new(),
        block_waited: false,
        explicitly_killed: false,
        completion_output_cap: None,
        persisted_output_dir,
    }
}
fn lookup_output(coordinator: &SubagentCoordinator, id: &str) -> String {
    match coordinator.lookup(id) {
        Some(SnapshotLookup::Ready(snap)) => {
            match snap.status {
                SubagentSnapshotStatus::Completed { output, .. } => output,
                other => panic!("expected Completed status, got {other:?}"),
            }
        }
        other => {
            panic!(
                "expected Ready lookup, got {:?}", other.map(| _ | "NeedsSignals/other")
            )
        }
    }
}
#[test]
fn lookup_degrades_to_placeholder_when_output_file_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .completed
        .insert(
            "sub-gone".to_string(),
            completed_with_output("sub-gone", "", Some(dir.path().to_path_buf())),
        );
    assert_eq!(
        lookup_output(& coordinator, "sub-gone"), OUTPUT_UNAVAILABLE_PLACEHOLDER,
        "an entry whose output.json is gone must degrade, not fail the query"
    );
}
#[test]
fn lookup_serves_unpersisted_output_from_memory() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .completed
        .insert("sub-mem".to_string(), completed_with_output("sub-mem", "output", None));
    assert_eq!(
        lookup_output(& coordinator, "sub-mem"), "output",
        "an entry with nothing on disk must serve its in-memory output"
    );
}
#[test]
fn completed_entries_are_capped_oldest_first() {
    let mut coordinator = SubagentCoordinator::new();
    let base = std::time::Instant::now();
    for i in 0..(MAX_COMPLETED_ENTRIES + 2) {
        let mut entry = completed_with_output(
            &format!("sub-{i}"),
            "",
            Some(std::path::PathBuf::from("/nonexistent")),
        );
        entry.completed_at = base + std::time::Duration::from_millis(i as u64);
        coordinator.completed.insert(format!("sub-{i}"), entry);
    }
    coordinator.enforce_completed_cap();
    assert_eq!(
        coordinator.completed.len(), MAX_COMPLETED_ENTRIES,
        "the completed map must be capped at MAX_COMPLETED_ENTRIES"
    );
    assert!(
        ! coordinator.completed.contains_key("sub-0") && ! coordinator.completed
        .contains_key("sub-1"), "the oldest completions must be evicted first"
    );
    assert!(
        coordinator.completed.contains_key("sub-2"),
        "entries within the cap must survive"
    );
}
#[test]
fn move_to_completed_clears_persisted_output_after_the_summary_clone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let full_output = "final report".repeat(100);
    assert!(write_subagent_output(dir.path(), & full_output));
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .move_to_completed(
            "sub-e2e",
            "task".into(),
            "explore".into(),
            SubagentResult {
                success: true,
                output: std::sync::Arc::from(full_output.as_str()),
                subagent_id: "sub-e2e".into(),
                child_session_id: "sub-e2e".into(),
                ..Default::default()
            },
            Some(dir.path().to_path_buf()),
        );
    let entry = coordinator.completed.get("sub-e2e").expect("entry inserted");
    assert!(
        entry.result.output.is_empty(),
        "a persisted entry must not keep the output in memory"
    );
    assert_eq!(
        lookup_output(& coordinator, "sub-e2e"), full_output,
        "lookup must serve the persisted output from disk"
    );
    let summaries = coordinator.drain_pending_completions();
    assert_eq!(
        &* summaries[0].output, full_output,
        "the completion summary must carry the full output"
    );
}
#[test]
fn persist_gate_only_persists_successful_nonempty_outputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ok = SubagentResult {
        success: true,
        output: std::sync::Arc::from("text"),
        ..Default::default()
    };
    assert_eq!(
        persist_subagent_output(dir.path(), & ok), Some(dir.path().to_path_buf())
    );
    let empty = SubagentResult {
        success: true,
        ..Default::default()
    };
    assert_eq!(persist_subagent_output(dir.path(), & empty), None);
    let failed = SubagentResult {
        success: false,
        output: std::sync::Arc::from("partial"),
        ..Default::default()
    };
    assert_eq!(persist_subagent_output(dir.path(), & failed), None);
}
#[test]
fn subagent_output_roundtrips_through_output_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = "line one\nline two with unicode ✓";
    assert!(write_subagent_output(dir.path(), output));
    assert_eq!(read_subagent_output(dir.path()).as_deref(), Some(output));
    assert_eq!(read_subagent_output(& dir.path().join("missing")), None);
    std::fs::write(dir.path().join("output.json"), "not json").expect("corrupt file");
    assert_eq!(read_subagent_output(dir.path()), None);
}
#[test]
fn cancel_with_outcome_fires_pending_token() {
    let mut coordinator = SubagentCoordinator::new();
    let token = CancellationToken::new();
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-cancel".to_string(),
            subagent_type: "explore".to_string(),
            description: "will be cancelled".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: false,
            surface_completion: true,
            color: None,
            cancel_token: token.clone(),
        });
    let outcome = coordinator.cancel_with_outcome("sub-cancel");
    assert!(
        matches!(outcome, SubagentCancelOutcome::Cancelled),
        "cancelling pending should return Cancelled"
    );
    assert!(token.is_cancelled(), "pending cancel must fire the spawn token");
    assert!(
        coordinator.lookup("sub-cancel").is_some(),
        "pending entry stays queryable until the spawn future tears it down"
    );
}
#[tokio::test]
async fn cancel_with_outcome_returns_variant_for_active_finished_unknown() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator.insert(dummy_tracker("sub-active", "session-A", "explore", "task"));
    assert!(
        matches!(coordinator.cancel_with_outcome("sub-active"),
        SubagentCancelOutcome::Cancelled)
    );
    coordinator
        .move_to_completed(
            "sub-done",
            "done".to_string(),
            "explore".to_string(),
            SubagentResult {
                success: true,
                subagent_id: "sub-done".to_string(),
                ..Default::default()
            },
            None,
        );
    assert!(
        matches!(coordinator.cancel_with_outcome("sub-done"),
        SubagentCancelOutcome::AlreadyFinished { status }
if status == "completed")
    );
    assert!(
        matches!(coordinator.cancel_with_outcome("nonexistent"),
        SubagentCancelOutcome::NotFound)
    );
}
#[test]
fn cancel_by_parent_prompt_id_fires_matching_pending_token() {
    let mut coordinator = SubagentCoordinator::new();
    let token_a = CancellationToken::new();
    let token_b = CancellationToken::new();
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-p1".to_string(),
            subagent_type: "explore".to_string(),
            description: "child of prompt-A".to_string(),
            persona: None,
            parent_prompt_id: Some("prompt-A".to_string()),
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: false,
            surface_completion: true,
            color: None,
            cancel_token: token_a.clone(),
        });
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-p2".to_string(),
            subagent_type: "explore".to_string(),
            description: "child of prompt-B".to_string(),
            persona: None,
            parent_prompt_id: Some("prompt-B".to_string()),
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: false,
            surface_completion: true,
            color: None,
            cancel_token: token_b.clone(),
        });
    coordinator.cancel_by_parent_prompt_id("prompt-A");
    assert!(token_a.is_cancelled(), "prompt-A token must fire");
    assert!(
        coordinator.lookup("sub-p1").is_some(),
        "prompt-A entry stays queryable until spawn teardown"
    );
    assert!(! token_b.is_cancelled(), "prompt-B token must not fire");
    assert!(coordinator.lookup("sub-p2").is_some());
}
#[test]
fn completed_takes_precedence_over_pending_in_lookup() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-dup".to_string(),
            subagent_type: "explore".to_string(),
            description: "duplicate".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: String::new(),
            started_at: std::time::Instant::now(),
            run_in_background: false,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    coordinator
        .move_to_completed(
            "sub-dup",
            "duplicate".to_string(),
            "explore".to_string(),
            SubagentResult {
                success: true,
                output: std::sync::Arc::from("done"),
                subagent_id: "sub-dup".to_string(),
                child_session_id: "child-dup".to_string(),
                ..Default::default()
            },
            None,
        );
    let lookup = coordinator.lookup("sub-dup");
    assert!(
        matches!(lookup, Some(SnapshotLookup::Ready(ref snap)) if matches!(snap.status,
        SubagentSnapshotStatus::Completed { .. })),
        "completed should take precedence over pending"
    );
}
#[test]
fn list_running_for_parent_returns_empty_when_no_active() {
    let coordinator = SubagentCoordinator::new();
    let seeds = coordinator.list_running_for_parent("parent-1");
    assert!(seeds.is_empty());
}
fn dummy_tracker(
    subagent_id: &str,
    parent_session_id: &str,
    subagent_type: &str,
    description: &str,
) -> SubagentTracker {
    use crate::session::feedback_manager::{FeedbackManager, FeedbackManagerConfig};
    use crate::session::handle::SessionHandle;
    use crate::session::info::Info;
    use crate::session::plan_mode::PlanModeTracker;
    use crate::session::signals::SessionSignalsHandle;
    use std::sync::atomic::AtomicBool;
    let gateway = test_gateway();
    let cwd = xai_grok_paths::AbsPathBuf::new(PathBuf::from("/tmp")).unwrap();
    let fs: Arc<dyn xai_grok_workspace::file_system::AsyncFileSystem> = Arc::new(
        xai_grok_workspace::file_system::LocalFs::new(PathBuf::from("/tmp")),
    );
    let terminal: Arc<dyn crate::terminal::AsyncTerminalRunner> = Arc::new(
        crate::terminal::TerminalRunner::new(
            Arc::new(test_gateway()),
            acp::SessionId::new("test"),
        ),
    );
    let tool_context = crate::tools::ToolContext::new(
        cwd,
        Some(gateway),
        Some(acp::SessionId::new("test")),
        fs,
        terminal,
        xai_hunk_tracker::HunkTrackerHandle::noop(),
    );
    let signals_handle = SessionSignalsHandle::new();
    let feedback_manager = FeedbackManager::new(
        "test",
        None,
        FeedbackManagerConfig::default(),
    );
    let handle = SessionHandle {
        cmd_tx: tokio::sync::mpsc::unbounded_channel().0,
        persistence_tx: tokio::sync::mpsc::unbounded_channel().0,
        current_prompt_id: Arc::new(std::sync::Mutex::new(None)),
        pending_interactions: Arc::new(
            std::sync::Mutex::new(std::collections::HashMap::new()),
        ),
        info: Info {
            id: acp::SessionId::new(subagent_id),
            cwd: "/tmp".into(),
        },
        max_turns: None,
        hunk_tracker_handle: xai_hunk_tracker::HunkTrackerHandle::noop(),
        chat_state_handle: xai_chat_state::ChatStateHandle::noop(),
        signals_handle,
        gateway_enabled: Arc::new(AtomicBool::new(false)),
        mcp_servers: vec![],
        initial_client_mcp_servers: vec![],
        display_cwd: None,
        feedback_manager: Arc::new(feedback_manager),
        upload_queue: Arc::new(OnceLock::new()),
        upload_failures_since_success: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        tool_context,
        model_id: acp::ModelId::new("test"),
        reasoning_effort: None,
        yolo_mode: false,
        origin_client: None,
        code_nav_enabled: false,
        ask_user_question_enabled: true,
        plan_mode: Arc::new(
            parking_lot::Mutex::new(PlanModeTracker::new(PathBuf::from("/tmp"))),
        ),
        force_compact: Arc::new(AtomicBool::new(false)),
        permission_handle: xai_grok_workspace::permission::PermissionHandle::allow_all(),
        attribution_callback: None,
        agent_name: "grok-build".to_string(),
        managed_mcp_proxy_base_url: String::new(),
        session_default_agent_profile: None,
        allowed_subagent_types: None,
        hook_registry: None,
        workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
        terminal_backend: None,
        tools_notification_handle: None,
        scheduler_handle: None,
    };
    SubagentTracker {
        subagent_id: subagent_id.into(),
        parent_session_id: parent_session_id.into(),
        parent_prompt_id: None,
        child_session_id: acp::SessionId::new(subagent_id),
        subagent_type: subagent_type.into(),
        persona: None,
        description: description.into(),
        started_at: std::time::Instant::now(),
        child_handle: handle,
        child_thread: crate::session::SessionThread::from_handle(
            std::thread::spawn(|| {}),
        ),
        cancel_token: tokio_util::sync::CancellationToken::new(),
        resumed_from: None,
        child_cwd: String::new(),
        worktree_path: None,
        effective_model_id: String::new(),
        run_in_background: false,
        surface_completion: true,
        completion_output_cap: None,
        color: None,
        block_waited: false,
        explicitly_killed: false,
    }
}
#[tokio::test]
async fn active_summaries_for_filters_by_parent_session_id() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator.insert(dummy_tracker("sub-1", "session-A", "explore", "task 1"));
    coordinator.insert(dummy_tracker("sub-2", "session-B", "plan", "task 2"));
    coordinator.insert(dummy_tracker("sub-3", "session-A", "general-purpose", "task 3"));
    let summaries_a = coordinator.active_summaries_for("session-A");
    assert_eq!(summaries_a.len(), 2);
    let ids_a: Vec<&str> = summaries_a.iter().map(|s| s.subagent_id.as_str()).collect();
    assert!(ids_a.contains(& "sub-1"));
    assert!(ids_a.contains(& "sub-3"));
    let summaries_b = coordinator.active_summaries_for("session-B");
    assert_eq!(summaries_b.len(), 1);
    assert_eq!(summaries_b[0].subagent_id, "sub-2");
    assert_eq!(summaries_b[0].subagent_type, "plan");
    assert_eq!(summaries_b[0].description, "task 2");
    let summaries_none = coordinator.active_summaries_for("session-C");
    assert!(summaries_none.is_empty());
}
#[tokio::test]
async fn active_summaries_returns_all_regardless_of_parent() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator.insert(dummy_tracker("sub-1", "session-A", "explore", "task 1"));
    coordinator.insert(dummy_tracker("sub-2", "session-B", "plan", "task 2"));
    let all = coordinator.active_summaries();
    assert_eq!(all.len(), 2);
}
/// Spawns issued from inside a child session (loop iterations) re-parent
/// to the root session via the running tracker's child→parent mapping.
#[tokio::test]
async fn parent_of_child_session_maps_to_root() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .insert(
            dummy_tracker(
                "iter-child-sess",
                "root-session",
                "general-purpose",
                "loop iteration",
            ),
        );
    assert_eq!(
        coordinator.parent_of_child_session("iter-child-sess").as_deref(),
        Some("root-session")
    );
    assert_eq!(coordinator.parent_of_child_session("unknown-sess"), None);
}
#[tokio::test]
async fn resolve_running_list_returns_empty_for_empty_seeds() {
    let resolved = resolve_running_list(vec![]).await;
    assert!(resolved.is_empty());
}
#[tokio::test]
async fn resolve_running_list_populates_fields_from_signals() {
    use crate::session::signals::SessionSignalsHandle;
    let signals = SessionSignalsHandle::new();
    signals.increment_turn();
    signals.record_tool_call("grep");
    tokio::task::yield_now().await;
    let seed = RunningSubagentListSeed {
        subagent_id: "sub-1".to_string(),
        parent_session_id: "parent-1".to_string(),
        child_session_id: "child-1".to_string(),
        subagent_type: "explore".to_string(),
        description: "find endpoints".to_string(),
        started_at_epoch_ms: 1000,
        duration_ms: 2000,
        signals_handle: signals,
    };
    let resolved = resolve_running_list(vec![seed]).await;
    assert_eq!(resolved.len(), 1);
    let r = &resolved[0];
    assert_eq!(r.subagent_id, "sub-1");
    assert_eq!(r.parent_session_id, "parent-1");
    assert_eq!(r.child_session_id, "child-1");
    assert_eq!(r.subagent_type, "explore");
    assert_eq!(r.turn_count, 1);
    assert_eq!(r.tool_call_count, 1);
    assert!(r.tools_used.contains(& "grep".to_string()));
}
#[test]
fn explicit_override_takes_precedence_over_role() {
    let overrides = SubagentRuntimeOverrides {
        model: Some("explicit-model".into()),
        capability_mode: Some(xai_tool_types::SubagentCapabilityMode::All),
        ..Default::default()
    };
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test role".into(),
        model: Some("role-model".into()),
        default_capability_mode: Some("read-only".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(resolved.model.as_deref(), Some("explicit-model"));
    assert_eq!(
        resolved.capability_mode, Some(xai_tool_types::SubagentCapabilityMode::All)
    );
}
#[test]
fn role_default_used_when_no_explicit_override() {
    let overrides = SubagentRuntimeOverrides::default();
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test role".into(),
        model: Some("role-model".into()),
        default_capability_mode: Some("read-only".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(resolved.model.as_deref(), Some("role-model"));
    assert_eq!(
        resolved.capability_mode, Some(xai_tool_types::SubagentCapabilityMode::ReadOnly)
    );
}
#[test]
fn no_role_no_override_returns_none() {
    let overrides = SubagentRuntimeOverrides::default();
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &HashMap::new(),
        None,
        None,
    );
    assert!(resolved.model.is_none());
    assert!(resolved.capability_mode.is_none());
    assert!(resolved.reasoning_effort.is_none());
}
#[test]
fn partial_override_fills_from_role() {
    let overrides = SubagentRuntimeOverrides {
        model: Some("explicit-model".into()),
        ..Default::default()
    };
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        default_capability_mode: Some("execute".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(resolved.model.as_deref(), Some("explicit-model"));
    assert_eq!(
        resolved.capability_mode, Some(xai_tool_types::SubagentCapabilityMode::Execute)
    );
}
#[test]
fn reasoning_effort_explicit_overrides_role() {
    let overrides = SubagentRuntimeOverrides {
        reasoning_effort: Some("high".into()),
        ..Default::default()
    };
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        reasoning_effort: Some("low".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(resolved.reasoning_effort.as_deref(), Some("high"));
}
#[test]
fn reasoning_effort_falls_back_to_role() {
    let overrides = SubagentRuntimeOverrides::default();
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        reasoning_effort: Some("medium".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(resolved.reasoning_effort.as_deref(), Some("medium"));
}
#[test]
fn invalid_role_capability_mode_ignored() {
    let overrides = SubagentRuntimeOverrides::default();
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        default_capability_mode: Some("invalid-mode".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert!(
        resolved.capability_mode.is_none(),
        "invalid role mode should not produce a capability_mode"
    );
}
#[test]
fn persona_resolved_from_config() {
    let overrides = SubagentRuntimeOverrides {
        persona: Some("researcher".into()),
        ..Default::default()
    };
    let mut personas = HashMap::new();
    personas
        .insert(
            "researcher".to_string(),
            xai_grok_subagent_resolution::config::SubagentPersona {
                instructions: Some("Be thorough.".into()),
                ..Default::default()
            },
        );
    let resolved = resolve_effective_overrides(&overrides, None, &personas, None, None);
    assert_eq!(resolved.persona.as_deref(), Some("researcher"));
    assert_eq!(resolved.persona_instructions.as_deref(), Some("Be thorough."));
}
#[test]
fn unknown_persona_produces_no_instructions() {
    let overrides = SubagentRuntimeOverrides {
        persona: Some("nonexistent".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(resolved.persona.as_deref(), Some("nonexistent"));
    assert!(resolved.persona_instructions.is_none());
}
#[test]
fn persona_inline_plus_file_merged_in_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("extra.md"), "File-based content.").unwrap();
    let overrides = SubagentRuntimeOverrides {
        persona: Some("combo".into()),
        ..Default::default()
    };
    let mut personas = HashMap::new();
    personas
        .insert(
            "combo".to_string(),
            xai_grok_subagent_resolution::config::SubagentPersona {
                instructions: Some("Inline first.".into()),
                instructions_file: Some("extra.md".into()),
                ..Default::default()
            },
        );
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &personas,
        Some(tmp.path()),
        None,
    );
    let pi = resolved.persona_instructions.as_deref().unwrap();
    assert!(pi.starts_with("Inline first."), "inline should come first: {pi}");
    assert!(pi.contains("File-based content."), "file content should be included: {pi}");
}
#[test]
fn model_precedence_explicit_over_role_over_persona() {
    let mut personas = HashMap::new();
    personas
        .insert(
            "dev".to_string(),
            xai_grok_subagent_resolution::config::SubagentPersona {
                model: Some("persona-model".into()),
                ..Default::default()
            },
        );
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        model: Some("role-model".into()),
        ..Default::default()
    };
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        model: Some("explicit-model".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.model.as_deref(), Some("explicit-model"));
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.model.as_deref(), Some("role-model"));
    let role_no_model = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        ..Default::default()
    };
    let r = resolve_effective_overrides(
        &overrides,
        Some(&role_no_model),
        &personas,
        None,
        None,
    );
    assert_eq!(r.model.as_deref(), Some("persona-model"));
    let overrides = SubagentRuntimeOverrides::default();
    let r = resolve_effective_overrides(&overrides, None, &HashMap::new(), None, None);
    assert!(r.model.is_none());
}
#[test]
fn reasoning_effort_precedence_explicit_over_role_over_persona() {
    let mut personas = HashMap::new();
    personas
        .insert(
            "dev".to_string(),
            xai_grok_subagent_resolution::config::SubagentPersona {
                reasoning_effort: Some("low".into()),
                ..Default::default()
            },
        );
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        reasoning_effort: Some("medium".into()),
        ..Default::default()
    };
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        reasoning_effort: Some("high".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.reasoning_effort.as_deref(), Some("high"));
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.reasoning_effort.as_deref(), Some("medium"));
    let role_no_re = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        ..Default::default()
    };
    let r = resolve_effective_overrides(
        &overrides,
        Some(&role_no_re),
        &personas,
        None,
        None,
    );
    assert_eq!(r.reasoning_effort.as_deref(), Some("low"));
    let overrides = SubagentRuntimeOverrides::default();
    let r = resolve_effective_overrides(&overrides, None, &HashMap::new(), None, None);
    assert!(r.reasoning_effort.is_none());
}
#[test]
fn persona_not_found_produces_error() {
    let overrides = SubagentRuntimeOverrides {
        persona: Some("missing".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &HashMap::new(),
        None,
        None,
    );
    assert!(resolved.persona_error.is_some());
    assert!(resolved.persona_error.as_deref().unwrap().contains("not found"),);
}
#[test]
fn prompt_assembly_ordering() {
    let role_prompt = Some(
        "<role-instructions>\nRole content\n</role-instructions>".to_string(),
    );
    let persona_instructions = Some(
        "<persona>\nPersona content\n</persona>".to_string(),
    );
    let task = "Do the task";
    let mut sections = Vec::new();
    sections.push("<fork-context>...</fork-context>".to_string());
    if let Some(ref rp) = role_prompt {
        sections.push(rp.clone());
    }
    if let Some(ref pi) = persona_instructions {
        sections.push(pi.clone());
    }
    sections.push(task.to_string());
    let assembled = sections.join("\n\n");
    let fork_pos = assembled.find("<fork-context>").unwrap();
    let role_pos = assembled.find("<role-instructions>").unwrap();
    let persona_pos = assembled.find("<persona>").unwrap();
    let task_pos = assembled.find("Do the task").unwrap();
    assert!(fork_pos < role_pos, "fork before role");
    assert!(role_pos < persona_pos, "role before persona");
    assert!(persona_pos < task_pos, "persona before task");
}
#[test]
fn no_persona_produces_none() {
    let overrides = SubagentRuntimeOverrides::default();
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &HashMap::new(),
        None,
        None,
    );
    assert!(resolved.persona.is_none());
    assert!(resolved.persona_instructions.is_none());
}
#[test]
fn initial_context_source_new_is_default() {
    let source = InitialContextSource::New;
    assert!(matches!(source, InitialContextSource::New));
}
#[test]
fn initial_context_source_forked_distinct_from_new_and_resumed() {
    let source = InitialContextSource::Forked;
    assert!(matches!(source, InitialContextSource::Forked));
    assert_ne!(source, InitialContextSource::New);
    assert_ne!(source, InitialContextSource::Resumed);
}
#[test]
fn forked_initial_context_normalizes_parent_history() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
        ConversationItem::system("parent system"),
        ConversationItem::user("UNIQUE_FORK_MARKER_abc123 implement multi-repo fix"),
        ConversationItem::assistant("noted"),
    ];
    let ctx = forked_initial_context(items);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(ctx.copy_error.is_none());
    assert_eq!(ctx.prefix_len, Some(2));
    assert_eq!(ctx.conversation.len(), 2);
    if let ConversationItem::User(ref u) = ctx.conversation[1] {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect();
        assert!(text.contains("<background_context>"));
        assert!(
            text.contains("UNIQUE_FORK_MARKER_abc123"),
            "distinctive parent token must appear in background: {text}"
        );
    } else {
        panic!("expected User background at [1]");
    }
}
#[test]
fn forked_initial_context_inherits_parent_across_reasoning() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
        ConversationItem::system("parent system"),
        ConversationItem::user("remember UNIQUE_FORK_MARKER_TEST"),
        ConversationItem::Reasoning(xai_grok_sampling_types::synthesized_reasoning_item("deliberating",)),
        ConversationItem::assistant("ack"),
    ];
    let ctx = forked_initial_context(items);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert_eq!(ctx.prefix_len, Some(2));
    assert_eq!(ctx.conversation.len(), 2);
    if let ConversationItem::User(ref u) = ctx.conversation[1] {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect();
        assert!(
            text.contains("<background_context>"),
            "background wrapper must be present: {text}"
        );
        assert!(
            text.contains("UNIQUE_FORK_MARKER_TEST"),
            "parent context must be inherited across the reasoning sibling: {text}"
        );
    } else {
        panic!("expected User background at [1]");
    }
}
#[test]
fn forked_initial_context_empty_fails_open_to_new() {
    let ctx = forked_initial_context(vec![]);
    assert_eq!(ctx.source, InitialContextSource::New);
    assert!(ctx.conversation.is_empty());
    assert!(ctx.copy_error.is_some());
}
#[test]
fn resume_vs_fork_helper_shapes_differ() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let resume_items = vec![
        ConversationItem::system("child system"),
        ConversationItem::user("prior subagent work"),
        ConversationItem::assistant("done"),
    ];
    let resumed = resume_initial_context(resume_items.clone());
    let forked = forked_initial_context(resume_items);
    assert_eq!(resumed.source, InitialContextSource::Resumed);
    assert_eq!(forked.source, InitialContextSource::Forked);
    assert!(resumed.conversation.len() > forked.conversation.len());
    assert!(
        ! matches!(resumed.conversation.get(1), Some(ConversationItem::User(u)) if u
        .content.iter().any(| p | matches!(p,
        xai_grok_sampling_types::conversation::ContentPart::Text { text }
if text
        .contains("<background_context>"))))
    );
}
#[test]
fn forked_initial_context_applies_fork_filter_before_normalize() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
        ConversationItem::system("sys"), ConversationItem::user("complete user"),
        ConversationItem::assistant("complete asst"),
        ConversationItem::user("INCOMPLETE_TRAILING"),
    ];
    let ctx = forked_initial_context(items);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    if let ConversationItem::User(ref u) = ctx.conversation[1] {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect();
        assert!(text.contains("complete user"));
        assert!(
            ! text.contains("INCOMPLETE_TRAILING"),
            "fork_filter must truncate incomplete trailing turn: {text}"
        );
    } else {
        panic!("expected background user");
    }
}
#[test]
fn verbatim_fork_keeps_items_byte_for_byte_when_small() {
    use xai_grok_sampling_types::conversation::{
        ContentPart, ConversationItem, SyntheticReason, UserItem,
    };
    let items = vec![
        ConversationItem::system("parent system"),
        ConversationItem::user("remember UNIQUE_FORK_MARKER_TEST"),
        ConversationItem::User(UserItem { content : vec![ContentPart::Text { text :
        "SYNTHETIC_KEEP_ME".into(), }], synthetic_reason :
        Some(SyntheticReason::SystemReminder), ..Default::default() }),
        ConversationItem::Reasoning(xai_grok_sampling_types::synthesized_reasoning_item("thinking",)),
        ConversationItem::assistant("ack"),
    ];
    let ctx = verbatim_or_normalize_fork(items, 256_000);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(ctx.verbatim_fork, "a small, complete-tail parent must mirror verbatim");
    assert_eq!(ctx.prefix_len, Some(5));
    assert_eq!(ctx.conversation.len(), 5);
    assert!(matches!(ctx.conversation[0], ConversationItem::System(_)));
    assert!(matches!(ctx.conversation.last(), Some(ConversationItem::Assistant(_))));
    let text_present = |needle: &str| {
        ctx
            .conversation
            .iter()
            .any(|i| {
                matches!(
                    i, ConversationItem::User(u) if u.content.iter().any(| p |
                    matches!(p, ContentPart::Text { text }
if text.contains(needle)))
                )
            })
    };
    assert!(text_present("UNIQUE_FORK_MARKER_TEST"), "marker must survive verbatim");
    assert!(
        text_present("SYNTHETIC_KEEP_ME"),
        "synthetic-reason item must be preserved verbatim, NOT stripped"
    );
    assert!(
        ctx.conversation.iter().any(| i | matches!(i, ConversationItem::User(u) if u
        .synthetic_reason.is_some())),
        "the synthetic_reason marker itself must remain in the verbatim mirror"
    );
    assert!(
        ! text_present("<background_context>"),
        "verbatim fork must NOT summarize into a background blob"
    );
}
#[test]
fn verbatim_fork_falls_back_to_summary_on_incomplete_tail() {
    use xai_grok_sampling_types::conversation::{
        AssistantItem, ContentPart, ConversationItem, ToolCall,
    };
    let items = vec![
        ConversationItem::system("parent system"),
        ConversationItem::user("q1 UNIQUE_FORK_MARKER_TEST"),
        ConversationItem::assistant("a1"), ConversationItem::user("q2"),
        ConversationItem::Assistant(AssistantItem { content : String::new().into(),
        tool_calls : vec![ToolCall { id : "tc1".into(), name : "bash".into(), arguments :
        "{}".into(), }], model_id : None, model_fingerprint : None, reasoning_effort :
        None, }),
    ];
    let ctx = verbatim_or_normalize_fork(items, 256_000);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(
        ! ctx.verbatim_fork,
        "an incomplete (dangling tool call) tail must fall back to summary"
    );
    assert_eq!(ctx.prefix_len, Some(2));
    assert!(
        ctx.conversation.iter().any(| i | { matches!(i, ConversationItem::User(u) if u
        .content.iter().any(| p | matches!(p, ContentPart::Text { text }
if text
        .contains("<background_context>")))) }),
        "summarized fallback must produce a background_context blob"
    );
}
#[test]
fn summarized_fork_is_not_a_verbatim_mirror() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
        ConversationItem::system("parent system prompt"),
        ConversationItem::user("turn one UNIQUE_FORK_MARKER_TEST"),
        ConversationItem::assistant("ack"),
    ];
    let ctx = verbatim_or_normalize_fork(items, 1);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(! ctx.verbatim_fork);
    let verbatim_mirror_fork = ctx.source == InitialContextSource::Forked
        && ctx.verbatim_fork;
    assert!(
        ! verbatim_mirror_fork,
        "a summarized fork must NOT be treated as a verbatim mirror"
    );
}
#[test]
fn verbatim_fork_falls_back_to_summary_when_oversize() {
    use xai_grok_sampling_types::conversation::{ContentPart, ConversationItem};
    let items = vec![
        ConversationItem::system("parent system"),
        ConversationItem::user("turn one UNIQUE_FORK_MARKER_TEST with some text"),
        ConversationItem::assistant("ack one"),
    ];
    let ctx = verbatim_or_normalize_fork(items, 1);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(! ctx.verbatim_fork, "oversize parent must fall back to summary");
    assert_eq!(ctx.prefix_len, Some(2));
    let has_blob = ctx
        .conversation
        .iter()
        .any(|i| {
            matches!(
                i, ConversationItem::User(u) if u.content.iter().any(| p | matches!(p,
                ContentPart::Text { text }
if text.contains("<background_context>")))
            )
        });
    assert!(has_blob, "oversize fallback must produce a background_context blob");
}
#[test]
fn verbatim_fork_empty_after_filter_fails_open_to_new() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![ConversationItem::user("/goal do the thing")];
    let ctx = verbatim_or_normalize_fork(items, 256_000);
    assert_eq!(ctx.source, InitialContextSource::New);
    assert!(! ctx.verbatim_fork);
    assert!(ctx.conversation.is_empty());
}
#[test]
fn verbatim_or_normalize_fork_system_only_fails_open_to_new() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    for items in [
        vec![ConversationItem::system("sys")],
        vec![ConversationItem::system("a"), ConversationItem::system("b")],
    ] {
        let ctx = verbatim_or_normalize_fork(items, 256_000);
        assert_eq!(
            ctx.source, InitialContextSource::New,
            "System-only fork must fail open to New"
        );
        assert!(! ctx.verbatim_fork);
        assert!(ctx.conversation.is_empty());
    }
}
#[test]
fn forked_initial_context_system_only_fails_open_to_new() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let ctx = forked_initial_context(vec![ConversationItem::system("sys")]);
    assert_eq!(ctx.source, InitialContextSource::New);
    assert!(! ctx.verbatim_fork);
    assert!(ctx.conversation.is_empty());
    assert!(ctx.copy_error.is_some());
}
#[test]
fn fork_context_normalized_only_for_summarized() {
    assert!(! fork_context_normalized(& InitialContextSource::Forked, true));
    assert!(fork_context_normalized(& InitialContextSource::Forked, false));
    assert!(! fork_context_normalized(& InitialContextSource::New, false));
    assert!(! fork_context_normalized(& InitialContextSource::Resumed, false));
    use xai_grok_sampling_types::conversation::ConversationItem;
    let verbatim = verbatim_or_normalize_fork(
        vec![
            ConversationItem::system("sys"), ConversationItem::user("q"),
            ConversationItem::assistant("a"),
        ],
        256_000,
    );
    assert!(verbatim.verbatim_fork);
    assert!(! fork_context_normalized(& verbatim.source, verbatim.verbatim_fork));
    let summarized = verbatim_or_normalize_fork(
        vec![
            ConversationItem::system("sys"), ConversationItem::user("q with text"),
            ConversationItem::assistant("a"),
        ],
        1,
    );
    assert!(! summarized.verbatim_fork);
    assert!(fork_context_normalized(& summarized.source, summarized.verbatim_fork));
}
fn bootstrap_test_request(fork_context: bool) -> SubagentRequest {
    let (result_tx, _) = oneshot::channel();
    SubagentRequest {
        id: "bootstrap-test".into(),
        prompt: "plan".into(),
        description: "d".into(),
        subagent_type: "general-purpose".into(),
        parent_session_id: "parent".into(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: false,
        surface_completion: false,
        fork_context,
        result_tx,
    }
}
#[tokio::test]
async fn bootstrap_no_fork_is_new() {
    let req = bootstrap_test_request(false);
    let ctx = ctx_with_toggle(HashMap::new());
    let child = SessionInfo {
        id: acp::SessionId::new("child-boot"),
        cwd: "/tmp".into(),
    };
    let out = bootstrap_initial_context(
            &req,
            None,
            &ctx,
            &child,
            Path::new("/tmp"),
            "m",
            128_000,
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(ic) => {
            assert_eq!(ic.source, InitialContextSource::New);
            assert!(ic.conversation.is_empty());
            assert!(ic.copy_error.is_none());
        }
        BootstrapInitialContext::ResumeAbort(m) => panic!("unexpected abort: {m}"),
    }
}
#[tokio::test]
async fn bootstrap_fork_without_parent_fails_open() {
    let req = bootstrap_test_request(true);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = None;
    ctx.parent_session_info = None;
    let child = SessionInfo {
        id: acp::SessionId::new("child-boot2"),
        cwd: "/tmp".into(),
    };
    let out = bootstrap_initial_context(
            &req,
            None,
            &ctx,
            &child,
            Path::new("/tmp"),
            "m",
            128_000,
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(ic) => {
            assert_eq!(ic.source, InitialContextSource::New);
            assert!(ic.copy_error.is_some());
        }
        BootstrapInitialContext::ResumeAbort(m) => {
            panic!("fork must fail open, not abort: {m}")
        }
    }
}
#[tokio::test]
async fn bootstrap_fork_live_parent_chat_state_is_forked_with_marker() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    const MARKER: &str = "UNIQUE_LIVE_FORK_MARKER_xyz789";
    let req = bootstrap_test_request(true);
    let mut ctx = ctx_with_toggle(HashMap::new());
    let chat = spawn_test_parent_chat_state("grok-4.5");
    chat.replace_conversation(
        vec![
            ConversationItem::system("parent system"),
            ConversationItem::user(format!("{MARKER} implement multi-repo fix")),
            ConversationItem::assistant("noted the multi-repo work"),
        ],
    );
    ctx.parent_chat_state = Some(chat);
    ctx.parent_session_info = None;
    let child = SessionInfo {
        id: acp::SessionId::new("child-boot-live"),
        cwd: "/tmp".into(),
    };
    let out = bootstrap_initial_context(
            &req,
            None,
            &ctx,
            &child,
            Path::new("/tmp"),
            "m",
            128_000,
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(ic) => {
            assert_eq!(ic.source, InitialContextSource::Forked);
            assert!(ic.copy_error.is_none());
            assert!(ic.verbatim_fork, "small complete-tail parent must mirror verbatim");
            assert_eq!(ic.conversation.len(), 3);
            assert_eq!(ic.prefix_len, Some(3));
            assert!(matches!(ic.conversation[0], ConversationItem::System(_)));
            assert!(matches!(ic.conversation[1], ConversationItem::User(_)));
            assert!(matches!(ic.conversation[2], ConversationItem::Assistant(_)));
            let text: String = ic
                .conversation
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::User(u) => {
                        Some(
                            u
                                .content
                                .iter()
                                .filter_map(|p| match p {
                                    xai_grok_sampling_types::conversation::ContentPart::Text {
                                        text,
                                    } => Some(text.as_ref()),
                                    _ => None,
                                })
                                .collect::<String>(),
                        )
                    }
                    _ => None,
                })
                .collect();
            assert!(
                text.contains(MARKER), "live parent marker must appear verbatim: {text}"
            );
            assert!(
                ! text.contains("<background_context>"),
                "verbatim mirror must NOT wrap items in a background_context blob: {text}"
            );
        }
        BootstrapInitialContext::ResumeAbort(m) => panic!("unexpected abort: {m}"),
    }
}
#[tokio::test]
async fn copy_session_data_preserves_parent_chat_history() {
    use crate::sampling::ConversationItem;
    use crate::session::storage::StorageAdapter;
    use crate::session::storage::jsonl::JsonlStorageAdapter;
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let adapter = JsonlStorageAdapter::with_root(root.to_path_buf());
    let parent_info = SessionInfo {
        id: acp::SessionId::new("parent-fork-test"),
        cwd: "/workspace".to_string(),
    };
    adapter.init_session(&parent_info, acp::ModelId::new("test-model")).await.unwrap();
    adapter
        .append_chat_message(&parent_info, &ConversationItem::user("What files?"))
        .await
        .unwrap();
    adapter
        .append_chat_message(&parent_info, &ConversationItem::assistant("listed"))
        .await
        .unwrap();
    let child_info = SessionInfo {
        id: acp::SessionId::new("child-fork-test"),
        cwd: "/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data_sync(
            &parent_info,
            &child_info,
            crate::session::storage::CopySessionOptions {
                parent_session_id: Some("parent-fork-test".to_string()),
                new_model_id: Some("test-model".to_string()),
                session_kind: Some("subagent_fork".to_string()),
                fork_context_source: Some("forked".to_string()),
                copy_plan_state: false,
                copy_plan_mode_state: false,
                copy_signals: false,
                copy_tool_state: false,
                fork_filter: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(result.chat_messages_copied > 0, "should copy chat history");
    let child_data = adapter.load_session(&child_info).await.unwrap();
    assert_eq!(child_data.summary.session_kind.as_deref(), Some("subagent_fork"));
    assert_eq!(child_data.summary.fork_context_source.as_deref(), Some("forked"));
    assert_eq!(
        child_data.summary.parent_session_id.as_deref(), Some("parent-fork-test")
    );
    assert!(
        ! child_data.chat_history.is_empty(),
        "child should have inherited parent chat history"
    );
}
#[tokio::test]
async fn handle_subagent_request_rejects_disabled_agent() {
    let toggle = HashMap::from([("explore".to_string(), false)]);
    let ctx = ctx_with_toggle(toggle);
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (request, result_rx) = make_request("explore");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, &coordinator, &gateway))
                .await;
        })
        .await;
    let result = result_rx.await.expect("should receive result");
    assert!(! result.success, "disabled subagent should fail");
    assert!(
        result.error.as_deref().unwrap_or("").contains("[subagents.toggle]"),
        "error should mention [subagents.toggle], got: {:?}", result.error
    );
}
#[tokio::test]
async fn handle_subagent_request_allows_when_absent_from_toggle() {
    let ctx = ctx_with_toggle(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (request, result_rx) = make_request("explore");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, &coordinator, &gateway))
                .await;
        })
        .await;
    let result = result_rx.await.expect("should receive result");
    if !result.success {
        assert!(
            ! result.error.as_deref().unwrap_or("").contains("[subagents.toggle]"),
            "should not be rejected by toggle gate when absent from toggle, \
                 but got: {:?}",
            result.error
        );
    }
}
#[tokio::test]
async fn handle_subagent_request_rejects_nonexistent_cwd() {
    let ctx = ctx_with_toggle(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (mut request, result_rx) = make_request("explore");
    request.cwd = Some("/nonexistent/path/that/does/not/exist".into());
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, &coordinator, &gateway))
                .await;
        })
        .await;
    let result = result_rx.await.expect("should receive result");
    assert!(! result.success, "nonexistent cwd should fail");
    assert!(
        result.error.as_deref().unwrap_or("").contains("does not exist"),
        "error should mention path does not exist, got: {:?}", result.error
    );
}
#[tokio::test]
async fn handle_subagent_request_rejects_file_as_cwd() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let tmp_file = tmp_dir.path().join("grok-test-cwd-file");
    std::fs::write(&tmp_file, b"not a directory").unwrap();
    let ctx = ctx_with_toggle(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (mut request, result_rx) = make_request("explore");
    request.cwd = Some(tmp_file.to_string_lossy().to_string());
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, &coordinator, &gateway))
                .await;
        })
        .await;
    let result = result_rx.await.expect("should receive result");
    assert!(! result.success, "file-as-cwd should fail");
    assert!(
        result.error.as_deref().unwrap_or("").contains("not a directory"),
        "error should mention not a directory, got: {:?}", result.error
    );
}
#[tokio::test]
async fn handle_subagent_request_valid_cwd_passes_validation() {
    let ctx = ctx_with_toggle(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (mut request, result_rx) = make_request("explore");
    request.cwd = Some("/tmp".into());
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, &coordinator, &gateway))
                .await;
        })
        .await;
    let result = result_rx.await.expect("should receive result");
    if !result.success {
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            ! err.contains("does not exist") && ! err.contains("not a directory"),
            "valid cwd should pass validation, but got cwd error: {err}"
        );
    }
}
#[tokio::test]
async fn handle_subagent_request_quoted_cwd_passes_validation() {
    let ctx = ctx_with_toggle(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (mut request, result_rx) = make_request("explore");
    request.cwd = Some("\"/tmp".into());
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, &coordinator, &gateway))
                .await;
        })
        .await;
    let result = result_rx.await.expect("should receive result");
    if !result.success {
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            ! err.contains("does not exist") && ! err.contains("not a directory"),
            "quoted cwd should be sanitized before validation, but got cwd error: {err}"
        );
    }
}
fn make_validation_ctx(toggle: HashMap<String, bool>) -> SubagentValidationContext {
    SubagentValidationContext {
        parent_cwd: PathBuf::from("/tmp"),
        subagent_toggle: toggle,
        ..Default::default()
    }
}
#[test]
fn validate_subagent_type_returns_ok_for_known_enabled_agent() {
    let ctx = make_validation_ctx(HashMap::new());
    let outcome = validate_subagent_type("explore", &ctx);
    assert!(
        matches!(outcome, SubagentValidateTypeOutcome::Ok),
        "expected Ok, got {outcome:?}",
    );
}
#[test]
fn validate_subagent_type_returns_unknown_for_invented_type() {
    let ctx = make_validation_ctx(HashMap::new());
    let outcome = validate_subagent_type("totally-invented-agent-name", &ctx);
    match outcome {
        SubagentValidateTypeOutcome::Unknown { available } => {
            for expected in ["general-purpose", "explore", "plan"] {
                assert!(
                    available.iter().any(| n | n == expected),
                    "available list must include built-in {expected:?}: {available:?}",
                );
            }
            let mut sorted = available.clone();
            sorted.sort();
            assert_eq!(available, sorted, "available must be sorted");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_returns_disabled_when_toggled_off() {
    let toggle = HashMap::from([("explore".to_string(), false)]);
    let ctx = make_validation_ctx(toggle);
    let outcome = validate_subagent_type("explore", &ctx);
    assert!(
        matches!(outcome, SubagentValidateTypeOutcome::Disabled),
        "expected Disabled, got {outcome:?}",
    );
}
#[test]
fn validate_subagent_type_returns_not_allowed_when_outside_allow_list() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.allowed_subagent_types = Some(vec!["plan".to_string()]);
    let outcome = validate_subagent_type("explore", &ctx);
    match outcome {
        SubagentValidateTypeOutcome::NotAllowed { allowed } => {
            assert_eq!(allowed, vec!["plan".to_string()]);
        }
        other => panic!("expected NotAllowed, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_allow_list_is_case_insensitive() {
    for (requested, allowed) in [
        ("explore", vec!["EXPLORE".to_string()]),
        ("EXPLORE", vec!["explore".to_string()]),
        ("Explore", vec!["eXpLoRe".to_string()]),
        ("explore", vec!["plan".to_string(), "EXPLORE".to_string()]),
    ] {
        let mut ctx = make_validation_ctx(HashMap::new());
        ctx.cli_agent_names = vec![requested.to_string()];
        ctx.allowed_subagent_types = Some(allowed.clone());
        assert!(
            matches!(validate_subagent_type(requested, & ctx),
            SubagentValidateTypeOutcome::Ok,),
            "{requested:?} should be permitted by allow-list {allowed:?}",
        );
    }
}
#[test]
fn validate_subagent_type_unknown_includes_cli_agents_in_available() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.cli_agent_names = vec!["user-defined-agent".to_string()];
    match validate_subagent_type("invented", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            assert!(
                available.iter().any(| n | n == "user-defined-agent"),
                "cli agent name missing from available list: {available:?}",
            );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_unknown_dedupes_cli_against_builtins() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.cli_agent_names = vec!["explore".to_string()];
    match validate_subagent_type("invented", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            let count = available.iter().filter(|n| n.as_str() == "explore").count();
            assert_eq!(count, 1, "explore must appear once: {available:?}");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_unknown_omits_disabled_types_from_available_list() {
    let toggle = HashMap::from([("explore".to_string(), false)]);
    let ctx = make_validation_ctx(toggle);
    match validate_subagent_type("explor", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            assert!(
                ! available.iter().any(| n | n == "explore"),
                "disabled type must not appear in available: {available:?}",
            );
            assert!(
                available.iter().any(| n | n == "general-purpose"),
                "non-disabled built-ins must still appear: {available:?}",
            );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_unknown_omits_disabled_cli_agents_from_available_list() {
    let toggle = HashMap::from([("custom".to_string(), false)]);
    let mut ctx = make_validation_ctx(toggle);
    ctx.cli_agent_names = vec!["custom".to_string(), "user-defined".to_string()];
    match validate_subagent_type("invented", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            assert!(
                ! available.iter().any(| n | n == "custom"),
                "disabled cli agent must not appear: {available:?}",
            );
            assert!(
                available.iter().any(| n | n == "user-defined"),
                "enabled cli agent must appear: {available:?}",
            );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_recognizes_cli_agent_by_name() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.cli_agent_names = vec!["user-defined".to_string()];
    assert!(
        matches!(validate_subagent_type("user-defined", & ctx),
        SubagentValidateTypeOutcome::Ok,)
    );
}
#[test]
#[serial_test::serial]
fn subagent_await_budget_default_and_override() {
    unsafe { std::env::remove_var("GROK_SUBAGENT_AWAIT_BUDGET_MS") };
    assert_eq!(SUBAGENT_AWAIT_BUDGET, std::time::Duration::from_secs(600));
    assert_eq!(subagent_await_budget(), SUBAGENT_AWAIT_BUDGET);
    unsafe { std::env::set_var("GROK_SUBAGENT_AWAIT_BUDGET_MS", "1500") };
    assert_eq!(subagent_await_budget(), std::time::Duration::from_millis(1500));
    unsafe { std::env::set_var("GROK_SUBAGENT_AWAIT_BUDGET_MS", "0") };
    assert_eq!(subagent_await_budget(), SUBAGENT_AWAIT_BUDGET);
    unsafe { std::env::set_var("GROK_SUBAGENT_AWAIT_BUDGET_MS", "not-a-number") };
    assert_eq!(subagent_await_budget(), SUBAGENT_AWAIT_BUDGET);
    unsafe { std::env::remove_var("GROK_SUBAGENT_AWAIT_BUDGET_MS") };
}
#[test]
fn summarize_tool_config_uses_name_override_and_strips_namespace() {
    use xai_grok_tools::registry::types::{ToolConfig, ToolServerConfig};
    use xai_grok_tools::types::tool::ToolKind;
    let mut read = ToolConfig::from_id("GrokBuild:read_file");
    read.kind = Some(ToolKind::Read);
    let mut read_dup = ToolConfig::from_id("Codex:read_file");
    read_dup.kind = Some(ToolKind::Read);
    read_dup.name_override = Some("codex_read".to_string());
    let mut grep = ToolConfig::from_id("OpenCode:grep");
    grep.kind = Some(ToolKind::Search);
    grep.name_override = Some("alt_grep".to_string());
    let mcp = ToolConfig::from_id("MCP:custom");
    let config = ToolServerConfig {
        tools: vec![read, read_dup, grep, mcp],
        behavior_preset: None,
    };
    let summary = summarize_tool_config(&config);
    assert_eq!(summary.tool_names.get(& ToolKind::Read).unwrap(), "read_file");
    assert_eq!(summary.tool_names.get(& ToolKind::Search).unwrap(), "alt_grep");
    assert!(summary.can_read && summary.can_search && ! summary.can_execute);
    assert_eq!(summary.tool_names.len(), 2);
}
#[test]
fn describe_subagent_type_unknown_returns_sorted_available() {
    let ctx = ctx_with_toggle(HashMap::new());
    match describe_subagent_type("totally-invented-type", None, &ctx) {
        SubagentDescribeOutcome::Unknown { available } => {
            let mut sorted = available.clone();
            sorted.sort();
            assert_eq!(available, sorted, "available must be sorted");
            assert!(available.iter().any(| n | n == "general-purpose"));
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn describe_subagent_type_disabled_when_toggled_off() {
    let ctx = ctx_with_toggle(HashMap::from([("explore".to_string(), false)]));
    assert!(
        matches!(describe_subagent_type("explore", None, & ctx),
        SubagentDescribeOutcome::Disabled)
    );
}
#[test]
fn describe_subagent_type_not_allowed_outside_allow_list() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.allowed_subagent_types = Some(vec!["plan".to_string()]);
    match describe_subagent_type("explore", None, &ctx) {
        SubagentDescribeOutcome::NotAllowed { allowed } => {
            assert_eq!(allowed, vec!["plan".to_string()]);
        }
        other => panic!("expected NotAllowed, got {other:?}"),
    }
}
/// Regression: on the DEFAULT grok-build host —
/// the primary `/goal` host — the `general-purpose` toolset's only
/// file-mutator is `search_replace` (`ToolKind::Edit`); the `write`
/// tool (`ToolKind::Write`) is injection-only and absent from the
/// pre-injection describe probe. The planner gate must therefore key on
/// the Edit-class capability, which this asserts is present.
#[test]
fn describe_default_host_general_purpose_has_edit_not_write() {
    use xai_grok_tools::types::tool::ToolKind;
    let ctx = ctx_with_toggle(HashMap::new());
    let SubagentDescribeOutcome::Ok(summary) = describe_subagent_type(
        "general-purpose",
        None,
        &ctx,
    ) else {
        panic!("expected Ok for default-host general-purpose");
    };
    assert!(summary.can_read, "default host reads (read_file)");
    assert!(
        summary.tool_names.contains_key(& ToolKind::Edit),
        "default host's file-mutator is search_replace (Edit): {:?}", summary.tool_names,
    );
    assert!(
        ! summary.tool_names.contains_key(& ToolKind::Write),
        "the injection-only `write` tool must NOT be in the pre-injection probe",
    );
}
/// Requirement 3 (fail-open trigger): an `agent_type` that does not resolve
/// to a harness `AgentDefinition` reports `Unknown`, which the `/goal`
/// resolver maps to a `ToolsetUnknown` fail-open to the session harness.
#[test]
fn goal_harness_override_unresolvable_returns_unknown() {
    let ctx = ctx_with_toggle(HashMap::new());
    match describe_subagent_type(
        "general-purpose",
        Some("totally-bogus-harness"),
        &ctx,
    ) {
        SubagentDescribeOutcome::Unknown { .. } => {}
        other => {
            panic!(
                "an unresolvable harness override must fail open as Unknown: {other:?}"
            )
        }
    }
}
/// The model fallback only fires for a strict harness: a custom profile
/// running a stock/vision model leaves subagents on the default harness, so
/// they keep native image input.
#[test]
fn subagent_keeps_default_flavor_when_parent_model_is_non_strict() {
    use xai_grok_agent::config::BuiltinAgentName;
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_agent_name = Some("ai-oncall-bot".to_string());
    ctx.parent_model_agent_type = Some(
        BuiltinAgentName::GrokBuildPlan.as_ref().to_string(),
    );
    let mut def = resolve_agent_definition("general-purpose", &ctx).expect("resolves");
    resolve_subagent_toolset("general-purpose", None, &ctx, &mut def);
    assert!(
        ! crate ::session::is_cursor_user_template(& def.user_message_template),
        "a non-strict parent model must leave subagents on the default harness",
    );
}
fn make_background_request(
    subagent_type: &str,
) -> (SubagentRequest, oneshot::Receiver<SubagentResult>) {
    let (mut req, rx) = make_request(subagent_type);
    req.run_in_background = true;
    (req, rx)
}
#[tokio::test]
async fn background_unknown_type_records_failure_completion() {
    let ctx = ctx_with_toggle(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (request, result_rx) = make_background_request("totally-invented-type");
    assert_background_pre_spawn_failure(
            ctx,
            &coordinator,
            &gateway,
            request,
            result_rx,
            "Unknown subagent type",
        )
        .await;
}
async fn assert_background_pre_spawn_failure(
    ctx: SubagentSpawnContext,
    coordinator: &std::cell::RefCell<SubagentCoordinator>,
    gateway: &GatewaySender,
    request: SubagentRequest,
    result_rx: oneshot::Receiver<SubagentResult>,
    expected_error_substring: &str,
) {
    let subagent_id = request.id.clone();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, coordinator, gateway)).await;
        })
        .await;
    let result = result_rx.await.expect("should receive result");
    assert!(! result.success);
    let err = result.error.as_deref().unwrap_or("");
    assert!(
        err.contains(expected_error_substring),
        "expected error substring {expected_error_substring:?} in {err:?}",
    );
    let lookup = coordinator.borrow().lookup(&subagent_id);
    match lookup {
        Some(SnapshotLookup::Ready(snap)) => {
            assert_eq!(snap.subagent_id, subagent_id);
            assert!(matches!(snap.status, SubagentSnapshotStatus::Failed { .. }));
        }
        _ => panic!("expected Ready(Failed) snapshot"),
    }
    let summaries = coordinator.borrow_mut().drain_pending_completions();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].subagent_id, subagent_id);
    assert!(! summaries[0].success);
}
#[tokio::test]
async fn background_disabled_type_records_failure_completion() {
    let toggle = HashMap::from([("explore".to_string(), false)]);
    let ctx = ctx_with_toggle(toggle);
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (request, result_rx) = make_background_request("explore");
    assert_background_pre_spawn_failure(
            ctx,
            &coordinator,
            &gateway,
            request,
            result_rx,
            "[subagents.toggle]",
        )
        .await;
}
#[tokio::test]
async fn background_not_allowed_type_records_failure_completion() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.allowed_subagent_types = Some(vec!["plan".to_string()]);
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (request, result_rx) = make_background_request("explore");
    assert_background_pre_spawn_failure(
            ctx,
            &coordinator,
            &gateway,
            request,
            result_rx,
            "not allowed",
        )
        .await;
}
async fn assert_blocking_pre_spawn_does_not_push_summary(
    ctx: SubagentSpawnContext,
    coordinator: &std::cell::RefCell<SubagentCoordinator>,
    gateway: &GatewaySender,
    request: SubagentRequest,
    result_rx: oneshot::Receiver<SubagentResult>,
) {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, coordinator, gateway)).await;
        })
        .await;
    let result = result_rx.await.expect("should receive result");
    assert!(! result.success);
    let summaries = coordinator.borrow_mut().drain_pending_completions();
    assert!(
        summaries.is_empty(),
        "blocking-mode pre-spawn failure must not push completion summaries: {summaries:?}",
    );
}
#[tokio::test]
async fn blocking_unknown_type_does_not_push_completion_summary() {
    let ctx = ctx_with_toggle(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (request, result_rx) = make_request("totally-invented-type");
    assert_blocking_pre_spawn_does_not_push_summary(
            ctx,
            &coordinator,
            &gateway,
            request,
            result_rx,
        )
        .await;
}
#[tokio::test]
async fn blocking_disabled_type_does_not_push_completion_summary() {
    let toggle = HashMap::from([("explore".to_string(), false)]);
    let ctx = ctx_with_toggle(toggle);
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (request, result_rx) = make_request("explore");
    assert_blocking_pre_spawn_does_not_push_summary(
            ctx,
            &coordinator,
            &gateway,
            request,
            result_rx,
        )
        .await;
}
#[tokio::test]
async fn blocking_not_allowed_type_does_not_push_completion_summary() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.allowed_subagent_types = Some(vec!["plan".to_string()]);
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (request, result_rx) = make_request("explore");
    assert_blocking_pre_spawn_does_not_push_summary(
            ctx,
            &coordinator,
            &gateway,
            request,
            result_rx,
        )
        .await;
}
#[tokio::test]
async fn background_failure_summary_includes_description() {
    let ctx = ctx_with_toggle(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let gateway = test_gateway();
    let (mut request, _result_rx) = make_background_request("invented");
    request.description = "find auth middleware".into();
    let id = request.id.clone();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, &coordinator, &gateway))
                .await;
        })
        .await;
    let summaries = coordinator.borrow_mut().drain_pending_completions();
    assert_eq!(summaries.len(), 1);
    let s = &summaries[0];
    assert_eq!(s.subagent_id, id);
    assert_eq!(s.subagent_type, "invented");
    assert_eq!(s.description, "find auth middleware");
}
#[tokio::test]
async fn background_unknown_type_emits_subagent_finished_notification() {
    use crate::test_support::lsp_runtime::{
        ctx_with_toggle_and_cmd_tx, test_gateway_with_receiver,
    };
    let (ctx, mut cmd_rx) = ctx_with_toggle_and_cmd_tx(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let (request, _result_rx) = make_background_request("invented-type");
    let subagent_id = request.id.clone();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Box::pin(handle_subagent_request(request, ctx, &coordinator, &gateway))
                .await;
        })
        .await;
    let mut found_persisted = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let SessionCommand::XaiSessionNotification { notification } = cmd
            && let SessionUpdate::SubagentFinished {
                subagent_id: id,
                status,
                error,
                ..
            } = &notification.update
        {
            assert_eq!(* id, subagent_id);
            assert_eq!(status, "failed");
            assert!(
                error.as_deref().is_some_and(| e | e.contains("Unknown subagent type")),
            );
            found_persisted = true;
        }
    }
    assert!(found_persisted, "must persist SubagentFinished via parent_cmd_tx");
    let mut found_live = false;
    while let Ok(msg) = gateway_rx.try_recv() {
        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg {
            let req: &acp::ExtNotification = &args.request;
            assert_eq!(req.method.as_ref(), "x.ai/session_notification");
            let body = req.params.get();
            assert!(body.contains("subagent_finished"));
            assert!(body.contains(& subagent_id));
            assert!(body.contains("\"status\":\"failed\""));
            assert!(body.contains("Unknown subagent type"));
            assert!(body.contains("\"will_wake\":false"));
            found_live = true;
            break;
        }
    }
    assert!(found_live, "must broadcast SubagentFinished via gateway");
}
/// The promote-guard teardown emits EXACTLY ONE cancelled `SubagentFinished`
/// (on both the persist + gateway channels), delivers a cancelled result, and
/// leaves the entry queryable as `Cancelled`.
#[tokio::test]
async fn cancel_pending_subagent_at_promote_emits_exactly_one_cancelled_finish() {
    use crate::test_support::lsp_runtime::{
        ctx_with_toggle_and_cmd_tx, test_gateway_with_receiver,
    };
    let (ctx, mut cmd_rx) = ctx_with_toggle_and_cmd_tx(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let (request, result_rx) = make_request("explore");
    let subagent_id = request.id.clone();
    let child_session_id = acp::SessionId::new(subagent_id.clone());
    coordinator
        .borrow_mut()
        .insert_pending(PendingSubagent {
            subagent_id: subagent_id.clone(),
            subagent_type: "explore".to_string(),
            description: "killed while pending".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: ctx.parent_session_id.clone(),
            started_at: std::time::Instant::now(),
            run_in_background: true,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    let child_handle = dummy_tracker(&subagent_id, "test-parent", "explore", "task")
        .child_handle;
    let meta_dir = std::env::temp_dir()
        .join(format!("subagent-promote-test-{subagent_id}"));
    let gcs_ctx = GcsUploadContext {
        bucket_url: None,
        upload_method: None,
        model_id: None,
        cwd: None,
        isolation_mode: None,
        capability_mode: None,
        reasoning_effort: None,
        role_name: None,
        parent_prompt_id: None,
        depth: 0,
        auth_manager: ctx.auth_manager.clone(),
    };
    cancel_pending_subagent_at_promote(
            request,
            &child_handle,
            &subagent_id,
            &child_session_id,
            &meta_dir,
            &coordinator,
            &gateway,
            &ctx.parent_session_id,
            ctx.parent_cmd_tx.as_ref(),
            None,
            false,
            42,
            &gcs_ctx,
        )
        .await;
    let mut persisted = 0;
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let SessionCommand::XaiSessionNotification { notification } = cmd
            && let SessionUpdate::SubagentFinished { subagent_id: id, status, .. } = &notification
                .update
        {
            assert_eq!(* id, subagent_id);
            assert_eq!(status, "cancelled");
            persisted += 1;
        }
    }
    assert_eq!(persisted, 1, "exactly one persisted SubagentFinished");
    let mut live = 0;
    while let Ok(msg) = gateway_rx.try_recv() {
        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg {
            let body = args.request.params.get();
            if body.contains("subagent_finished") {
                assert!(body.contains(& subagent_id));
                assert!(body.contains("\"status\":\"cancelled\""));
                live += 1;
            }
        }
    }
    assert_eq!(live, 1, "exactly one live SubagentFinished");
    let result = result_rx.await.expect("result delivered to oneshot");
    assert!(result.cancelled, "result must be cancelled");
    assert!(! result.success);
    match coordinator.borrow().lookup(&subagent_id) {
        Some(SnapshotLookup::Ready(snap)) => {
            assert!(
                matches!(snap.status, SubagentSnapshotStatus::Cancelled { .. }),
                "expected Cancelled, got {:?}", snap.status
            )
        }
        _ => panic!("expected Ready(Cancelled) snapshot after promote-abort"),
    }
}
/// Drive `cancel_pending_subagent_at_promote` against a real `worktree` and
/// assert it still emits EXACTLY ONE cancelled finish + leaves the entry
/// queryable as Cancelled. The caller asserts the worktree dir's fate.
async fn run_promote_cancel_with_worktree(
    worktree: &Path,
    worktree_freshly_created: bool,
) {
    use crate::test_support::lsp_runtime::{
        ctx_with_toggle_and_cmd_tx, test_gateway_with_receiver,
    };
    let (ctx, mut cmd_rx) = ctx_with_toggle_and_cmd_tx(HashMap::new());
    let coordinator = std::cell::RefCell::new(SubagentCoordinator::new());
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let (request, result_rx) = make_request("explore");
    let subagent_id = request.id.clone();
    let child_session_id = acp::SessionId::new(subagent_id.clone());
    coordinator
        .borrow_mut()
        .insert_pending(PendingSubagent {
            subagent_id: subagent_id.clone(),
            subagent_type: "explore".to_string(),
            description: "killed while pending".to_string(),
            persona: None,
            parent_prompt_id: None,
            parent_session_id: ctx.parent_session_id.clone(),
            started_at: std::time::Instant::now(),
            run_in_background: true,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    let child_handle = dummy_tracker(&subagent_id, "test-parent", "explore", "task")
        .child_handle;
    let meta_dir = std::env::temp_dir().join(format!("subagent-wt-test-{subagent_id}"));
    let gcs_ctx = GcsUploadContext {
        bucket_url: None,
        upload_method: None,
        model_id: None,
        cwd: None,
        isolation_mode: None,
        capability_mode: None,
        reasoning_effort: None,
        role_name: None,
        parent_prompt_id: None,
        depth: 0,
        auth_manager: ctx.auth_manager.clone(),
    };
    cancel_pending_subagent_at_promote(
            request,
            &child_handle,
            &subagent_id,
            &child_session_id,
            &meta_dir,
            &coordinator,
            &gateway,
            &ctx.parent_session_id,
            ctx.parent_cmd_tx.as_ref(),
            Some(worktree),
            worktree_freshly_created,
            42,
            &gcs_ctx,
        )
        .await;
    let mut persisted = 0;
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let SessionCommand::XaiSessionNotification { notification } = cmd
            && matches!(notification.update, SessionUpdate::SubagentFinished { .. })
        {
            persisted += 1;
        }
    }
    assert_eq!(persisted, 1, "exactly one persisted SubagentFinished");
    let mut live = 0;
    while let Ok(msg) = gateway_rx.try_recv() {
        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
            && args.request.params.get().contains("subagent_finished")
        {
            live += 1;
        }
    }
    assert_eq!(live, 1, "exactly one live SubagentFinished");
    let result = result_rx.await.expect("result delivered to oneshot");
    assert!(result.cancelled, "result must be cancelled");
    assert!(
        matches!(coordinator.borrow().lookup(& subagent_id),
        Some(SnapshotLookup::Ready(snap)) if matches!(snap.status,
        SubagentSnapshotStatus::Cancelled { .. }))
    );
}
/// The promote-abort teardown removes a FRESHLY-created worktree (this
/// subagent's own, pristine) but PRESERVES a resumed subagent's reused
/// worktree (it aliases the source's dir — deleting it would lose the
/// source's working state). Exactly one cancelled finish emits either way.
#[tokio::test]
async fn cancel_pending_at_promote_removes_fresh_worktree_preserves_resumed() {
    xai_test_utils::require_git!();
    use xai_test_utils::git::{git_commit_all, init_git_repo};
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_git_repo(&repo);
    std::fs::write(repo.join("tracked.txt"), "original").unwrap();
    git_commit_all(&repo, "initial");
    let fresh = temp.path().join("subagent-fresh");
    xai_fast_worktree::WorktreeBuilder::new(&repo, &fresh)
        .standalone(true)
        .create()
        .unwrap();
    assert!(fresh.exists());
    run_promote_cancel_with_worktree(&fresh, true).await;
    assert!(
        ! fresh.exists(), "freshly-created worktree must be removed on pending-kill"
    );
    let resumed = temp.path().join("subagent-resumed");
    xai_fast_worktree::WorktreeBuilder::new(&repo, &resumed)
        .standalone(true)
        .create()
        .unwrap();
    std::fs::write(resumed.join("tracked.txt"), "source edit").unwrap();
    assert!(resumed.exists());
    run_promote_cancel_with_worktree(&resumed, false).await;
    assert!(
        resumed.exists(),
        "resumed subagent's reused worktree must be preserved (source owns it)"
    );
    assert_eq!(
        std::fs::read_to_string(resumed.join("tracked.txt")).unwrap(), "source edit",
        "the source's working state must be left untouched"
    );
}
#[test]
fn record_pre_spawn_failure_populates_completed_and_summary() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .record_pre_spawn_failure(
            "sub-x".to_string(),
            "invented".to_string(),
            "bg job".to_string(),
            Some("prompt-1".to_string()),
            "parent-1".to_string(),
            "Unknown subagent type: invented",
            true,
        );
    let lookup = coordinator.lookup("sub-x");
    match lookup {
        Some(SnapshotLookup::Ready(snap)) => {
            assert_eq!(snap.subagent_id, "sub-x");
            match &snap.status {
                SubagentSnapshotStatus::Failed { error } => {
                    assert!(error.contains("Unknown subagent type"));
                }
                other => panic!("expected Failed, got {other:?}"),
            }
        }
        _ => panic!("expected Ready snapshot for recorded pre-spawn failure"),
    }
    let summaries = coordinator.drain_pending_completions();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].subagent_id, "sub-x");
    assert_eq!(summaries[0].subagent_type, "invented");
    assert_eq!(summaries[0].description, "bg job");
    assert!(! summaries[0].success);
}
#[test]
fn record_pre_spawn_failure_skips_buffer_when_flag_false() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .record_pre_spawn_failure(
            "sub-hidden-pre".to_string(),
            "invented".to_string(),
            "bg job".to_string(),
            None,
            "parent-1".to_string(),
            "Unknown subagent type: invented",
            false,
        );
    assert!(coordinator.drain_pending_completions().is_empty());
    assert!(coordinator.lookup("sub-hidden-pre").is_some());
}
#[tokio::test]
async fn record_pre_spawn_failure_notifies_waiters() {
    let mut coordinator = SubagentCoordinator::new();
    let notify = coordinator.completion_notify();
    let waiter = notify.notified();
    coordinator
        .record_pre_spawn_failure(
            "sub-y".to_string(),
            "invented".to_string(),
            "bg job".to_string(),
            None,
            "parent-1".to_string(),
            "error",
            true,
        );
    tokio::time::timeout(std::time::Duration::from_millis(50), waiter)
        .await
        .expect("notify_waiters must wake pre-armed waiter");
}
#[tokio::test]
async fn record_pre_spawn_failure_notifies_all_waiters() {
    let mut coordinator = SubagentCoordinator::new();
    let notify = coordinator.completion_notify();
    let waiter_a = notify.notified();
    let waiter_b = notify.notified();
    coordinator
        .record_pre_spawn_failure(
            "sub-multi".to_string(),
            "invented".to_string(),
            "bg job".to_string(),
            None,
            "parent-1".to_string(),
            "error",
            true,
        );
    let timeout = std::time::Duration::from_millis(50);
    tokio::time::timeout(timeout, waiter_a).await.expect("waiter_a must wake");
    tokio::time::timeout(timeout, waiter_b).await.expect("waiter_b must wake");
}
#[test]
fn record_pre_spawn_failure_clears_stale_pending_entry() {
    let mut coordinator = SubagentCoordinator::new();
    coordinator
        .insert_pending(PendingSubagent {
            subagent_id: "sub-z".to_string(),
            subagent_type: "invented".to_string(),
            description: "stale".to_string(),
            persona: None,
            parent_prompt_id: Some("prompt-X".to_string()),
            parent_session_id: "parent-1".to_string(),
            started_at: std::time::Instant::now(),
            run_in_background: true,
            surface_completion: true,
            color: None,
            cancel_token: CancellationToken::new(),
        });
    assert!(coordinator.pending.contains_key("sub-z"));
    coordinator
        .record_pre_spawn_failure(
            "sub-z".to_string(),
            "invented".to_string(),
            "bg job".to_string(),
            Some("prompt-X".to_string()),
            "parent-1".to_string(),
            "Unknown subagent type: invented",
            true,
        );
    assert!(! coordinator.pending.contains_key("sub-z"));
    match coordinator.lookup("sub-z") {
        Some(SnapshotLookup::Ready(snap)) => {
            assert!(matches!(snap.status, SubagentSnapshotStatus::Failed { .. }));
        }
        _ => panic!("expected Ready(Failed) post-collision"),
    }
    assert!(
        ! coordinator.outstanding_for_prompt("prompt-X").iter().any(| id | id ==
        "sub-z"), "outstanding_for_prompt must not still list a recorded-failed id",
    );
}
fn test_model_entry(model_id: &str) -> crate::agent::config::ModelEntry {
    crate::agent::config::ModelEntry {
        info: crate::agent::config::ModelInfo {
            user_selectable: true,
            id: None,
            model: model_id.to_string(),
            base_url: String::new(),
            name: None,
            description: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: Default::default(),
            auth_scheme: Default::default(),
            extra_headers: Default::default(),
            context_window: std::num::NonZeroU64::new(256_000).unwrap(),
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            use_concise: false,
            agent_type: crate::agent::config::default_agent_type(),
            inference_idle_timeout_secs: None,
            max_retries: None,
            hidden: false,
            supported_in_api: true,
            reasoning_effort: None,
            supports_reasoning_effort: false,
            reasoning_efforts: Vec::new(),
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: None,
            laziness_detector: crate::agent::config::LazinessDetectorPerModelConfig::default(),
        },
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}
fn byok_model_entry(model_id: &str) -> crate::agent::config::ModelEntry {
    crate::agent::config::ModelEntry {
        api_key: Some("byok-key".to_string()),
        ..test_model_entry(model_id)
    }
}
#[test]
fn subagent_auth_type_rule() {
    use crate::agent::auth_method::{CACHED_TOKEN_AUTH_METHOD_ID, XAI_API_KEY_METHOD_ID};
    use xai_chat_state::AuthType;
    let session = acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID);
    let api_key = acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID);
    let byok = byok_model_entry("grok-byok");
    let plain = test_model_entry("grok-plain");
    assert_eq!(super::subagent_auth_type(Some(& byok), & session), AuthType::ApiKey);
    assert_eq!(super::subagent_auth_type(Some(& byok), & api_key), AuthType::ApiKey);
    assert_eq!(
        super::subagent_auth_type(Some(& plain), & session), AuthType::SessionToken,
    );
    assert_eq!(super::subagent_auth_type(Some(& plain), & api_key), AuthType::ApiKey);
    assert_eq!(super::subagent_auth_type(None, & session), AuthType::SessionToken);
    assert_eq!(super::subagent_auth_type(None, & api_key), AuthType::ApiKey);
}
#[test]
fn fresh_tool_model_accepts_visible_key_and_internal_id() {
    let mut models = indexmap::IndexMap::new();
    models.insert("grok-3".to_string(), test_model_entry("grok-3-2025-02-15"));
    assert!(
        super::handle_request::task_model_override_error(Some("grok-3"),
        ModelOverrideProvenance::Tool, false, & models, false,).is_none(),
        "key lookup should succeed"
    );
    assert!(
        super::handle_request::task_model_override_error(Some("grok-3-2025-02-15"),
        ModelOverrideProvenance::Tool, false, & models, false,).is_none(),
        "info().model lookup should succeed"
    );
}
#[test]
fn fresh_tool_model_rejects_unavailable_exact_key_over_visible_slug_collision() {
    let mut models = indexmap::IndexMap::new();
    models.insert("visible-alias".to_string(), test_model_entry("collision"));
    let mut unavailable_exact = test_model_entry("hidden-internal");
    unavailable_exact.info.hidden = true;
    models.insert("collision".to_string(), unavailable_exact);
    assert_eq!(
        super::handle_request::task_model_override_error(Some("collision"),
        ModelOverrideProvenance::Tool, false, & models, false,).as_deref(),
        Some("Unknown Task.model slug 'collision'. Valid model slugs: visible-alias. \
                 Omit `model` to inherit the parent model."),
        "validation must inspect the unavailable exact-key entry selected by execution"
    );
}
#[test]
fn fresh_tool_model_rejects_unavailable_first_slug_collision() {
    let mut models = indexmap::IndexMap::new();
    let mut unavailable_first = test_model_entry("shared-routing-slug");
    unavailable_first.info.user_selectable = false;
    models.insert("blocked-first".to_string(), unavailable_first);
    models.insert("visible-second".to_string(), test_model_entry("shared-routing-slug"));
    assert_eq!(
        super::handle_request::task_model_override_error(Some("shared-routing-slug"),
        ModelOverrideProvenance::Tool, false, & models, false,).as_deref(),
        Some("Unknown Task.model slug 'shared-routing-slug'. Valid model slugs: \
                 visible-second. Omit `model` to inherit the parent model."),
        "validation must inspect the first routing-slug entry selected by execution"
    );
}
#[test]
fn fresh_tool_model_rejects_unknown_and_nonavailable_entries() {
    let mut models = indexmap::IndexMap::new();
    models.insert("zeta".to_string(), test_model_entry("zeta-internal"));
    let mut hidden = test_model_entry("hidden-internal");
    hidden.info.hidden = true;
    models.insert("hidden".to_string(), hidden);
    let mut not_selectable = test_model_entry("disabled-internal");
    not_selectable.info.user_selectable = false;
    models.insert("disabled".to_string(), not_selectable);
    let mut oauth_only = test_model_entry("oauth-only-internal");
    oauth_only.info.supported_in_api = false;
    models.insert("oauth-only".to_string(), oauth_only);
    models.insert("alpha".to_string(), test_model_entry("alpha-internal"));
    for requested in [
        "stale-model",
        "hidden",
        "hidden-internal",
        "disabled",
        "disabled-internal",
        "oauth-only",
        "oauth-only-internal",
    ] {
        let error = super::handle_request::task_model_override_error(
                Some(requested),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .unwrap();
        assert_eq!(
            error,
            format!("Unknown Task.model slug '{requested}'. Valid model slugs: alpha, zeta. \
                     Omit `model` to inherit the parent model.")
        );
        assert!(! error.contains("grok models"));
    }
    assert!(
        super::handle_request::task_model_override_error(Some("oauth-only"),
        ModelOverrideProvenance::Tool, false, & models, true,).is_none(),
        "OAuth-only model should resolve for session auth"
    );
}
#[test]
fn fresh_tool_model_reports_empty_valid_list() {
    let empty = indexmap::IndexMap::new();
    assert_eq!(
        super::handle_request::task_model_override_error(Some("anything"),
        ModelOverrideProvenance::Tool, false, & empty, false,).as_deref(),
        Some("Unknown Task.model slug 'anything'. No valid model slugs are currently \
                 available. Omit `model` to inherit the parent model.")
    );
}
#[test]
fn resumed_tool_model_override_is_ignored() {
    let empty = indexmap::IndexMap::new();
    assert!(
        super::handle_request::task_model_override_error(Some("stale-model"),
        ModelOverrideProvenance::Tool, true, & empty, false,).is_none(),
        "resume must preserve source-model pinning"
    );
}
#[test]
fn harness_model_override_keeps_internal_fallback_behavior() {
    let empty = indexmap::IndexMap::new();
    assert!(
        super::handle_request::task_model_override_error(Some("internal-model"),
        ModelOverrideProvenance::Harness, false, & empty, false,).is_none(),
        "internal role/config pins must retain downstream soft fallback"
    );
}
#[test]
fn normalize_forked_context_empty_parent() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![ConversationItem::system("sys prompt")];
    let (conv, prefix_len) = xai_grok_subagent_resolution::context::normalize_forked_context(
        items,
    );
    assert_eq!(conv.len(), 1);
    assert_eq!(prefix_len, 1);
    assert!(matches!(conv[0], ConversationItem::System(_)));
}
#[test]
fn normalize_forked_context_short_conversation() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
        ConversationItem::system("sys"), ConversationItem::user("hello"),
        ConversationItem::assistant("hi back"),
    ];
    let (conv, prefix_len) = xai_grok_subagent_resolution::context::normalize_forked_context(
        items,
    );
    assert_eq!(prefix_len, 2);
    assert_eq!(conv.len(), 2);
    assert!(matches!(conv[0], ConversationItem::System(_)));
    if let ConversationItem::User(u) = &conv[1] {
        let text = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("<background_context>"), "should have background tag");
        assert!(text.contains("[User]: hello"), "should include parent user message");
        assert!(
            text.contains("[Assistant]: hi back"),
            "should include parent assistant message"
        );
    } else {
        panic!("expected User message at position 1");
    }
}
fn test_sampling_config(model_slug: &str) -> xai_grok_sampling_types::SamplingConfig {
    use std::num::NonZeroU64;
    xai_grok_sampling_types::SamplingConfig {
        base_url: "https://api.test/v1".to_string(),
        model: model_slug.to_string(),
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        api_backend: Default::default(),
        extra_headers: Default::default(),
        context_window: NonZeroU64::new(256_000).expect("non-zero context window"),
        reasoning_effort: None,
        stream_tool_calls: None,
    }
}
fn spawn_test_parent_chat_state(model_slug: &str) -> xai_chat_state::ChatStateHandle {
    let (mock, _persistence_rx) = xai_chat_state::MockChatPersistence::new();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let token = tokio_util::sync::CancellationToken::new();
    xai_chat_state::ChatStateActor::spawn(
        vec![],
        test_sampling_config(model_slug),
        Box::new(mock),
        event_tx,
        token,
    )
}
mod rest;
