//! Plan-mode edit gate through the real `prepare_tool_call` path: plan mode
//! is read-only except the plan file in EVERY permission mode. The fixture's
//! `PermissionHandle::allow_all()` is the always-approve worst case — before
//! the gate, it silently approved any edit in plan mode (the "yolo edits in
//! plan mode" bug); these tests pin that the gate rejects
//! BEFORE the permission layer can auto-approve.
use super::support::*;
use super::*;
/// Build an actor whose toolset parses grok `search_replace` plus the plan
/// tools (so `${{ tools.by_kind.exit_plan }}` resolves in the rejection
/// message), with a gateway drain answering session notifications.
async fn build_gate_actor() -> SessionActor {
    use xai_grok_tools::implementations::grok_build::enter_plan_mode::EnterPlanModeTool;
    use xai_grok_tools::implementations::grok_build::exit_plan_mode::ExitPlanModeTool;
    use xai_grok_tools::registry::types::ToolConfig;
    let (gateway_tx, mut gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    *actor.agent.borrow_mut() = test_agent_with_tools(vec![
        // search_replace's requirements demand a Read tool in the same toolset.
        ToolConfig::from_id("GrokBuild:read_file"),
        ToolConfig::from_id("GrokBuild:search_replace"),
        ToolConfig::for_tool::<EnterPlanModeTool>(),
        ToolConfig::for_tool::<ExitPlanModeTool>(),
    ])
    .await;
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    actor
}
/// Flip the fixture's tracker to Active (plan file: `/tmp/test-session/plan.md`).
fn activate_plan_mode(actor: &SessionActor) {
    let mut tracker = actor.plan_mode.lock();
    assert!(tracker.enter_pending());
    assert!(tracker.activate());
}
fn search_replace_call(id: &str, path: &str) -> ToolCallResponse {
    ToolCallResponse {
        id: id.to_string(),
        kind: "function".to_string(),
        function: crate::sampling::types::ToolCallFunction::new(
            "search_replace",
            format!(r#"{{"file_path":"{path}","old_string":"a","new_string":"b"}}"#),
        ),
    }
}
async fn prepare(
    actor: &SessionActor,
    call: ToolCallResponse,
) -> Result<PreparedToolCall, ToolLoop> {
    let mut deferred = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        actor.prepare_tool_call(call, &mut deferred),
    )
    .await
    .expect("prepare_tool_call must not hang (a hang means a permission prompt was issued)")
    .expect("prepare_tool_call must not error")
}
/// Last tool_result pushed for `call_id`, or panic.
async fn tool_result_text(actor: &SessionActor, call_id: &str) -> String {
    let conv = actor.chat_state_handle.get_conversation().await;
    conv.iter()
        .rev()
        .find_map(|item| match item {
            xai_grok_sampling_types::ConversationItem::ToolResult(tr)
                if tr.tool_call_id == call_id =>
            {
                Some(tr.content.to_string())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool_result for {call_id} in {conv:?}"))
}
/// The headline: plan mode Active + allow-all permissions (the always-approve
/// worst case) still rejects a grok edit outside the plan file, without ever
/// reaching the permission layer, and steers the model to `exit_plan_mode`.
#[tokio::test(flavor = "current_thread")]
async fn plan_mode_rejects_grok_edit_outside_plan_file_despite_allow_all_permissions() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_gate_actor().await;
            activate_plan_mode(&actor);
            let result =
                prepare(&actor, search_replace_call("call_gate", "/tmp/src/main.rs")).await;
            assert!(
                matches!(result, Err(ToolLoop::Continue)),
                "gate must reject with Continue (tool not executed); got {result:?}"
            );
            let text = tool_result_text(&actor, "call_gate").await;
            assert!(
                text.contains("Rejected: file edits are not allowed in plan mode"),
                "rejection text: {text}"
            );
            assert!(
                text.contains("/tmp/test-session/plan.md"),
                "must name the plan file so the model knows the one editable path: {text}"
            );
            assert!(
                !text.contains("exit_plan_mode"),
                "rejection should stay short (no exit-tool steering): {text}"
            );
        })
        .await;
}
/// The carve-out: the plan file itself prepares cleanly (the gate defers to
/// `should_auto_approve_edit`, the same predicate as the permission bypass).
#[tokio::test(flavor = "current_thread")]
async fn plan_mode_allows_plan_file_edit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_gate_actor().await;
            activate_plan_mode(&actor);
            let result = prepare(
                &actor,
                search_replace_call("call_plan_file", "/tmp/test-session/plan.md"),
            )
            .await;
            assert!(
                result.is_ok(),
                "plan-file edit must pass the gate and prepare; got {:?}",
                result.err()
            );
        })
        .await;
}
/// Control: with plan mode inactive the same edit prepares cleanly — the gate
/// is plan-scoped, not a general edit block.
#[tokio::test(flavor = "current_thread")]
async fn inactive_plan_mode_does_not_gate_edits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_gate_actor().await;
            let result = prepare(
                &actor,
                search_replace_call("call_no_plan", "/tmp/src/main.rs"),
            )
            .await;
            assert!(
                result.is_ok(),
                "edit outside plan mode must prepare; got {:?}",
                result.err()
            );
        })
        .await;
}
