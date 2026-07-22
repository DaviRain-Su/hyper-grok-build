//! Tool-call execution concern for `SessionActor`: the model-output →
//! tool-execution pipeline (`execute_tool_calls`, `prepare_tool_call`,
//! tool-call start/success/error notifications, and sampling-event handling).
//!
//! `#[path]` child of `acp_session` (see the module comments there) so this
//! `impl SessionActor` block retains access to the actor's private fields and
//! the parent module's private helpers.
use super::*;
use futures::StreamExt;
/// Whether a tool name is an MCP `create_pull_request` (qualified
/// `server__create_pull_request` or bare).
fn is_mcp_create_pull_request(tool_name: &str) -> bool {
    match crate::session::mcp_servers::parse_mcp_tool_name(tool_name) {
        Some((_, tool)) => tool == "create_pull_request",
        None => tool_name == "create_pull_request",
    }
}
/// Blocking wait tools that should abort when a mid-turn interjection is pending.
fn is_interruptible_wait_tool(tool_name: &str, args: &serde_json::Value) -> bool {
    match tool_name {
        "get_task_output"
        | "get_command_or_subagent_output"
        | "get_task_or_subagent_output"
        | "get_terminal_command_output" => xai_tool_types::task_output_waits_from_json(args),
        "wait_tasks" | "wait_commands_or_subagents" | "wait_tasks_or_subagents" => true,
        "Await" | "AwaitShell" => true,
        _ => false,
    }
}
async fn wait_for_pending_interjection(buf: &InterjectionBuffer<acp::ImageContent>) {
    loop {
        if !buf.is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
use crate::tools::tool_context::BlockingWaitGuard;
/// Model-facing result when a wait is aborted for a pending interjection.
fn interrupted_wait_tool_result(args: &serde_json::Value) -> ToolRunResult {
    interrupted_wait_tool_result_with_msg(args, "Wait interrupted: the user sent a message.")
}
/// [`interrupted_wait_tool_result`] with a caller-chosen model-facing message.
fn interrupted_wait_tool_result_with_msg(args: &serde_json::Value, msg: &str) -> ToolRunResult {
    use xai_tool_types::{TaskOutputOutput, TaskOutputResult};
    let task_id = args
        .get("task_ids")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .or_else(|| args.get("task_id").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let result = TaskOutputResult {
        task_id,
        command: String::new(),
        status: "cancelled".to_string(),
        exit_code: None,
        started: String::new(),
        ended: None,
        duration_secs: 0.0,
        output: msg.to_string(),
        output_file: String::new(),
        truncated: false,
        truncation_hint: String::new(),
        raw_output_bytes: msg.len(),
    };
    ToolRunResult {
        output: ToolsToolOutput::TaskOutput(TaskOutputOutput::Result(result)),
        prompt_text: msg.to_string(),
        effective_tool_name: None,
    }
}
/// Clears `awaiting_plan_approval` (and re-persists) when the
/// [`SessionActor::request_plan_approval`] await **resolves** (a decision came
/// back) or is **dropped** (the model turn was cancelled) — so a cancelled
/// in-session approval can never strand the bit `true`.
///
/// It is deliberately [`disarm`](Self::disarm)ed on the client-disconnect
/// (quit) path: there the approval is genuinely still pending, so the bit must
/// stay `true` on disk for the next resume to re-park it.
/// `PlanModeState` writes are immediate (no debounce), so writing `false` here
/// would race the quit and lose the gate.
struct AwaitingApprovalGuard<'a>(&'a SessionActor);
impl AwaitingApprovalGuard<'_> {
    /// Keep `awaiting_plan_approval` set (skip the clear-on-drop). Used when the
    /// client disconnected without answering, so resume re-parks the approval.
    fn disarm(self) {
        std::mem::forget(self);
    }
}
impl Drop for AwaitingApprovalGuard<'_> {
    fn drop(&mut self) {
        self.0.plan_mode.lock().set_awaiting_plan_approval(false);
        self.0.persist_plan_mode_state();
    }
}
/// Outcome of reading the on-disk plan file for the exit-plan intercept path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlanFileRead {
    Present(String),
    Absent,
    Unreadable,
}
/// Classify a plan-file read result into present / absent / unreadable.
pub(super) fn classify_plan_file_read(result: Result<String, std::io::Error>) -> PlanFileRead {
    match result {
        Ok(text) if !text.trim().is_empty() => PlanFileRead::Present(text),
        Ok(_) => PlanFileRead::Absent,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PlanFileRead::Absent,
        Err(_) => PlanFileRead::Unreadable,
    }
}
/// Whether to intercept exit-plan tools for client-side plan approval.
///
/// A mode-switch back to agent with `PlanFileRead::Absent` skips intercept
/// (leaving without approving is allowed). `Present` / `Unreadable` still intercept
/// (unreadable = fail-closed, empty approval UI rather than silent exit).
pub(super) fn should_intercept_exit_plan_approval(
    is_exit_plan_mode: bool,
    is_cursor_switch_to_agent: bool,
    is_cursor_create_plan: bool,
    plan_read: &PlanFileRead,
) -> bool {
    if !is_exit_plan_mode && !is_cursor_switch_to_agent && !is_cursor_create_plan {
        return false;
    }
    if is_cursor_switch_to_agent && matches!(plan_read, PlanFileRead::Absent) {
        return false;
    }
    true
}
/// Verdict for a tool call evaluated against the plan-mode edit gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanEditGate {
    /// Execute normally (plan mode inactive, not an edit, or allowed target).
    Allow,
    /// Grok-toolset edit outside the plan file (plan-file-only rule).
    RejectNonPlanFile,
}
/// Gate edit-class tool calls while plan mode is active.
///
/// Plan mode is read-only **in every permission mode, including
/// always-approve**: the permission manager's YOLO fast path deliberately
/// knows nothing about plan mode, so this gate — not the permission system —
/// is what enforces it. Two rules, matching the two toolsets' contracts:
///
/// - **Compat-toolset `Write`/`StrReplace`**: any markdown
///   file is editable in plan mode (plan docs are written with these
///   same tools); everything else is rejected. Pre-existing behavior.
/// - **Compat-toolset `Delete`** is **not** on the markdown carve-out: it maps to
///   `AccessKind::Edit` and is plan-file-only (same as grok edits). Deleting
///   an arbitrary `.md` in plan mode must not pass.
/// - **Every other edit tool** (`AccessKind::Edit`) is restricted to the plan
///   file itself, via the same predicate that auto-approves plan-file edits
///   ([`PlanModeTracker::should_auto_approve_edit`]) so the gate and the
///   permission bypass can never disagree.
///
/// `apply_patch` maps to a placeholder `AccessKind::Edit("apply_patch")` and
/// therefore never matches the plan file: it is always rejected in plan mode
/// (conservative — per-file targets are only known after patch parsing).
/// Non-edit tools (bash, read, grep, MCP, web) are never gated here; they
/// flow to the normal permission path, where yolo may still auto-approve
/// them. `enter_plan_mode` / `exit_plan_mode` map to `AccessKind::Read` and
/// are likewise never gated.
pub(super) fn plan_mode_edit_gate(
    tracker: &crate::session::plan_mode::PlanModeTracker,
    tool_input: &ToolInput,
    access_kind: &AccessKind,
) -> PlanEditGate {
    if !tracker.is_active() {
        return PlanEditGate::Allow;
    }
    let _ = tool_input;
    match access_kind {
        AccessKind::Edit(path) if !tracker.should_auto_approve_edit(Path::new(path)) => {
            PlanEditGate::RejectNonPlanFile
        }
        _ => PlanEditGate::Allow,
    }
}
/// Typed view of an `exit_plan_mode` approval decision. The wire type
/// (`ExitPlanModeExtResponse`) carries `outcome` as a string; both the mid-turn
/// intercept and the resume re-park match on this enum instead. Unknown /
/// unrecognized outcomes map to [`Cancelled`](Self::Cancelled) so the session
/// fails CLOSED (stays in plan mode) rather than auto-approving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanApprovalOutcome {
    Approved,
    Cancelled,
    Abandoned,
}
impl PlanApprovalOutcome {
    fn from_response(
        resp: &xai_grok_tools::implementations::grok_build::exit_plan_mode::ExitPlanModeExtResponse,
    ) -> Self {
        match resp.outcome.as_str() {
            "approved" => Self::Approved,
            "abandoned" => Self::Abandoned,
            _ => Self::Cancelled,
        }
    }
}
/// Classify an `ext_method` failure: `true` when the reverse-request could not
/// be DELIVERED to any client (no interactive client wired — headless / SDK),
/// `false` when it was delivered but the client went away before answering
/// (quit / disconnect / leader restart).
///
/// Uses `xai_acp_lib`'s TYPED [`AcpChannelFailure`](xai_acp_lib::AcpChannelFailure)
/// discriminant (carried in the error's `data`) rather than substring-matching
/// another crate's message text: `SendFailed` (enqueue failed → no connection) →
/// `true`; `RecvFailed` (delivered then dropped) → `false`. Any other error
/// (including a non-`acp_send` error) defaults to `false` so the approval is
/// kept pending and never auto-approved.
fn ext_method_no_client(err: &acp::Error) -> bool {
    matches!(
        xai_acp_lib::acp_channel_failure(err),
        Some(xai_acp_lib::AcpChannelFailure::SendFailed)
    )
}
/// Model-facing turn injected after a resumed plan is approved.
const PLAN_APPROVED_IMPLEMENT_MESSAGE: &str =
    "The user approved the plan. Implement the plan in plan.md.";
/// Shared "revise the plan" message for the request-changes outcome, used by
/// both the mid-turn intercept and the resume re-park.
fn revise_plan_message(feedback: &str) -> String {
    let feedback = feedback.trim();
    if feedback.is_empty() {
        "The user wants to revise the plan. \
         Ask the user what changes they would like to make."
            .to_string()
    } else {
        format!("The user wants to revise the plan. The user said:\n{feedback}")
    }
}
/// What the resume re-park does with the user's decision. Extracted
/// from `resume_plan_approval` so the branch logic is unit-testable without
/// driving a real turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResumeAction {
    /// Approved: leave plan mode and start an implement turn (Agent mode).
    LeaveAndImplement,
    /// Request changes: stay in plan mode and start a revise turn (Plan mode).
    StayAndRevise(String),
    /// Abandoned: leave plan mode and wait for the user (no turn).
    LeaveOnly,
}
fn resume_action_for(outcome: PlanApprovalOutcome, feedback: Option<String>) -> ResumeAction {
    match outcome {
        PlanApprovalOutcome::Approved => ResumeAction::LeaveAndImplement,
        PlanApprovalOutcome::Cancelled => {
            ResumeAction::StayAndRevise(revise_plan_message(feedback.as_deref().unwrap_or("")))
        }
        PlanApprovalOutcome::Abandoned => ResumeAction::LeaveOnly,
    }
}
impl SessionActor {
    /// Merge the canonical `x.ai/tool` identity envelope into a tool-call
    /// event's `_meta`, resolving the tool from the live toolset by wire name.
    pub(super) fn stamp_tool_meta(
        &self,
        existing: Option<acp::Meta>,
        wire_name: &str,
        parsed: Option<&ToolInput>,
    ) -> Option<acp::Meta> {
        let toolset = self.agent.borrow().tool_bridge().toolset();
        xai_grok_tools::normalization::merge_tool_meta(
            &toolset,
            existing.map(serde_json::Value::Object),
            wire_name,
            parsed,
        )
        .and_then(|v| v.as_object().cloned())
    }
    #[tracing::instrument(
        name = "tools.execute",
        skip_all,
        fields(
            tool_count = tool_calls.len(),
            model_id,
            session_id = %self.session_info.id.0
        )
    )]
    pub(super) async fn execute_tool_calls(
        &self,
        tool_calls: Vec<crate::sampling::types::ToolCallResponse>,
    ) -> Result<ToolLoop, acp::Error> {
        if let Some(cfg) = self.chat_state_handle.get_sampling_config().await {
            tracing::Span::current().record("model_id", cfg.model.as_str());
        }
        let mut final_result: Option<ToolLoop> = None;
        let mut deferred_followups: Vec<ConversationItem> = Vec::new();
        let mut approved: Vec<PreparedToolCall> = Vec::new();
        for call in tool_calls.into_iter() {
            if final_result.is_some() {
                let message = match &final_result {
                    Some(ToolLoop::PermissionReject { .. }) => {
                        format!(
                            "Tool execution cancelled due to earlier permission rejection for tool `{}`",
                            call.function.name
                        )
                    }
                    Some(ToolLoop::Cancelled) => {
                        format!(
                            "Tool execution cancelled due to earlier user cancellation for tool `{}`",
                            call.function.name
                        )
                    }
                    Some(ToolLoop::FollowupMessage(_)) => {
                        format!(
                            "Tool execution cancelled due to earlier user followup message for tool `{}`",
                            call.function.name
                        )
                    }
                    _ => {
                        format!("Tool execution cancelled for tool `{}`", call.function.name)
                    }
                };
                self.chat_state_handle
                    .push_tool_result(ConversationItem::tool_result(call.id.clone(), message));
                continue;
            }
            self.emit_event(crate::session::events::Event::ToolStarted {
                tool_name: call.function.name.clone(),
            });
            self.observability_bridge
                .emit(
                    xai_tool_protocol::session_event::SessionEvent::ToolCallStarted {
                        tool_call_id: call.id.clone(),
                        tool_name: call.function.name.clone(),
                        turn_number: self.current_turn_number.get(),
                    },
                )
                .await;
            let call_name = call.function.name.clone();
            match self
                .prepare_tool_call(call, &mut deferred_followups)
                .await?
            {
                Ok(prepared) => approved.push(prepared),
                Err(tool_loop) => {
                    self.events.tool_finished();
                    if let Some((server, tool)) =
                        crate::session::mcp_servers::parse_mcp_tool_name(&call_name)
                    {
                        let error_reason = match &tool_loop {
                            ToolLoop::PermissionReject { reason, .. } => reason.clone(),
                            ToolLoop::Cancelled => "cancelled".to_string(),
                            ToolLoop::FollowupMessage(_) => "followup".to_string(),
                            ToolLoop::HookDenied { hook_name, .. } => {
                                format!("hook_denied:{hook_name}")
                            }
                            other => format!("{other:?}"),
                        };
                        self.emit_event(xai_file_utils::events::Event::McpToolCallCompleted {
                            server_name: server.to_string(),
                            tool_name: tool.to_string(),
                            call_id: format!(
                                "{}{}{}",
                                server,
                                crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER,
                                tool
                            ),
                            duration_ms: 0,
                            success: false,
                            is_timeout: false,
                            error: Some(error_reason),
                            reconnect_attempted: false,
                            auth_retry_attempted: false,
                        });
                    }
                    if matches!(
                        tool_loop,
                        ToolLoop::PermissionReject { .. }
                            | ToolLoop::Cancelled
                            | ToolLoop::FollowupMessage(_)
                    ) && final_result.is_none()
                    {
                        final_result = Some(tool_loop);
                    }
                }
            }
        }
        if approved.iter().any(|p| p.tool_name == "search_tool") {
            self.retry_auth_required_servers().await;
        }
        let write_paths: std::collections::HashSet<String> = approved
            .iter()
            .filter(|prepared| !prepared.is_read_only)
            .filter_map(|prepared| lock_path_for_args(&prepared.parsed_args).map(str::to_owned))
            .collect();
        let file_locks = {
            let mut map: std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>> =
                std::collections::HashMap::new();
            for prepared in &approved {
                if let Some(fp) = lock_path_for_args(&prepared.parsed_args)
                    && write_paths.contains(fp)
                {
                    map.entry(fp.to_owned())
                        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())));
                }
            }
            map
        };
        let shared_recovery = Arc::new(tokio::sync::OnceCell::<bool>::const_new());
        let workspace_ops = self.workspace_ops.clone();
        let pending_interjections = self.pending_interjections.clone();
        let session_id: Arc<str> = Arc::from(&*self.session_info.id.0);
        let dispatch_futures: Vec<_> = approved
            .iter()
            .enumerate()
            .map(|(idx, prepared)| {
                let prepared = Arc::new(prepared.clone());
                let am = self.auth_manager.clone();
                let shared_recovery = Arc::clone(&shared_recovery);
                let workspace_ops = workspace_ops.clone();
                let session_id = session_id.clone();
                let pending_interjections = pending_interjections.clone();
                let blocking_wait_depth = self.tool_context.blocking_wait_depth.clone();
                let interruptible =
                    is_interruptible_wait_tool(&prepared.tool_name, &prepared.parsed_args);
                let lock = lock_path_for_args(&prepared.parsed_args)
                    .and_then(|fp| file_locks.get(fp).cloned());
                async move {
                    let exec_start = std::time::Instant::now();
                    let run_tool = || {
                        let prepared = Arc::clone(&prepared);
                        let workspace_ops = workspace_ops.clone();
                        let session_id = session_id.clone();
                        let lock = lock.clone();
                        async move {
                            let _guard = if let Some(ref l) = lock {
                                Some(l.lock().await)
                            } else {
                                None
                            };
                            dispatch_tool(&workspace_ops, &prepared, &session_id).await
                        }
                    };
                    let result = if interruptible {
                        let _wait_guard = BlockingWaitGuard::enter(blocking_wait_depth.clone());
                        tokio::select! {
                            biased; result = call_with_auth_retry(am.as_ref(), Some(&
                            shared_recovery), & prepared.tool_name, run_tool,) => result,
                            _ = wait_for_pending_interjection(& pending_interjections) =>
                            { tracing::info!(tool = % prepared.tool_name,
                            "abort wait tool: interjection pending");
                            Ok(interrupted_wait_tool_result(& prepared.parsed_args)) }
                        }
                    } else {
                        call_with_auth_retry(
                            am.as_ref(),
                            Some(&shared_recovery),
                            &prepared.tool_name,
                            run_tool,
                        )
                        .await
                    };
                    let success = match &result {
                        Ok(tool_result) => !tool_result.output.is_error(),
                        Err(_) => false,
                    };
                    xai_grok_telemetry::unified_log::info(
                        "shell.tool.exec_done",
                        Some(session_id.as_ref()),
                        Some(serde_json::json!(
                            { "tool_name" : prepared.tool_name.as_str(), "elapsed_ms" :
                            exec_start.elapsed().as_millis() as u64, "success" :
                            success, }
                        )),
                    );
                    (idx, result)
                }
            })
            .collect();
        tokio::task::yield_now().await;
        let mut dispatch_stream = futures::stream::FuturesUnordered::new();
        for fut in dispatch_futures {
            dispatch_stream.push(fut);
        }
        let mut approved_slots: Vec<Option<PreparedToolCall>> =
            approved.into_iter().map(Some).collect();
        let (dispatch_tx, mut dispatch_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, _)>();
        let drainer = tokio::spawn(async move {
            while let Some(item) = dispatch_stream.next().await {
                if dispatch_tx.send(item).is_err() {
                    break;
                }
            }
        });
        let _drainer_guard = crate::util::AbortOnDrop(drainer);
        while let Some((idx, mut result)) = dispatch_rx.recv().await {
            let prepared = approved_slots[idx]
                .take()
                .expect("dispatch index should match an approved slot exactly once");
            self.signals_handle().record_tool_call(&prepared.tool_name);
            let tool_start = self.events.tool_started(prepared.tool_name.clone());
            let mut post_tool_use_result: Option<serde_json::Value> = None;
            if let Some((server, _)) =
                crate::session::mcp_servers::parse_mcp_tool_name(&prepared.tool_name)
                && server.starts_with(crate::session::managed_mcp::MANAGED_MCP_PREFIX)
            {
                let auth_rejected = match &result {
                    Err(err) => xai_grok_mcp::servers::is_auth_rejection_message(&err.to_string()),
                    Ok(tool_result) => {
                        tool_result.output.is_error()
                            && xai_grok_mcp::servers::is_auth_rejection_message(
                                &tool_result.prompt_text,
                            )
                    }
                };
                if auth_rejected && self.reactive_managed_reauth(&server).await.is_ok() {
                    result = dispatch_tool(&self.workspace_ops, &prepared, &self.session_info.id.0)
                        .await;
                }
            }
            let tool_result_size_bytes = match &result {
                Ok(tool_result) => tool_result.prompt_text.len() as i64,
                Err(_) => 0,
            };
            let tool_failed = match &result {
                Ok(tool_result) => tool_result.output.is_error(),
                Err(_) => true,
            };
            let tool_loop = match result {
                Ok(tool_result) => {
                    let effective_tool_name = tool_result
                        .effective_tool_name
                        .clone()
                        .or_else(|| prepared.dispatch_target_name.clone())
                        .unwrap_or_else(|| prepared.tool_name.clone());
                    post_tool_use_result = self
                        .hook_event_active(xai_grok_hooks::event::HookEventName::PostToolUse)
                        .then(|| {
                            serde_json::to_value(&tool_result.output)
                                .unwrap_or(serde_json::Value::Null)
                        });
                    let followups = self
                        .handle_bridge_tool_success(
                            &prepared.tool_call_id,
                            &prepared.call_id,
                            &prepared.tool_name,
                            &effective_tool_name,
                            tool_result,
                            prepared.concatenated_json_count,
                            &prepared.model_id,
                            &prepared.parsed_args,
                        )
                        .await?;
                    deferred_followups.extend(followups);
                    if prepared.tool_name == "search_tool" {
                        let pi = self.chat_state_handle.get_prompt_index().await as i64;
                        self.last_search_prompt_index
                            .store(pi, std::sync::atomic::Ordering::Relaxed);
                    }
                    ToolLoop::Continue
                }
                Err(err) => {
                    let err: anyhow::Error = err.into();
                    let err_followups = self
                        .handle_tool_error(
                            &prepared.tool_call_id,
                            &prepared.call_id,
                            &prepared.tool_name,
                            prepared.dispatch_target_name.as_deref(),
                            &err,
                            &prepared.model_id,
                        )
                        .await;
                    deferred_followups.extend(err_followups);
                    if self
                        .hook_event_active(xai_grok_hooks::event::HookEventName::PostToolUseFailure)
                    {
                        let raw_input: serde_json::Value =
                            serde_json::from_str(&prepared.raw_arguments)
                                .unwrap_or(serde_json::Value::Null);
                        let (tool_input_value, tool_input_truncated) =
                            xai_grok_hooks::event::truncate_payload(raw_input);
                        let hook_tool_name = prepared.hook_tool_name();
                        self.dispatch_hook(
                            xai_grok_hooks::event::HookEventName::PostToolUseFailure,
                            xai_grok_hooks::event::HookPayload::PostToolUseFailure {
                                tool_name: hook_tool_name.to_owned(),
                                tool_use_id: prepared.call_id.clone(),
                                tool_input: tool_input_value,
                                tool_input_truncated,
                                error: format!("{err:#}"),
                                subagent_type: self.subagent_type_label(),
                            },
                            None,
                            Some(hook_tool_name),
                        )
                        .await;
                    }
                    ToolLoop::Continue
                }
            };
            {
                let bridge = self.agent.borrow().tool_bridge().clone();
                if let Some(effects) = bridge.apply_pending_skill_update().await {
                    if let Some(item) = self.wrap_skill_reminder(&effects) {
                        deferred_followups.push(item);
                    }
                    if effects.send_available_commands {
                        self.send_available_commands_update().await;
                    }
                }
            }
            if let Some(tool_result_value) = post_tool_use_result {
                let raw_input: serde_json::Value = serde_json::from_str(&prepared.raw_arguments)
                    .unwrap_or(serde_json::Value::Null);
                let (tool_input_value, tool_input_truncated) =
                    xai_grok_hooks::event::truncate_payload(raw_input);
                let (tool_result_val, tool_result_truncated) =
                    xai_grok_hooks::event::truncate_payload(tool_result_value);
                let hook_tool_name = prepared.hook_tool_name();
                self.dispatch_hook(
                    xai_grok_hooks::event::HookEventName::PostToolUse,
                    xai_grok_hooks::event::HookPayload::PostToolUse {
                        tool_name: hook_tool_name.to_owned(),
                        tool_use_id: prepared.call_id.clone(),
                        tool_input: tool_input_value,
                        tool_result: tool_result_val,
                        tool_input_truncated,
                        tool_result_truncated,
                        duration_ms: None,
                        is_backgrounded: false,
                        subagent_type: self.subagent_type_label(),
                    },
                    None,
                    Some(hook_tool_name),
                )
                .await;
            }
            self.events.tool_finished();
            let tool_outcome = match &tool_loop {
                _ if tool_failed => crate::session::events::ToolOutcome::Error,
                ToolLoop::Continue => crate::session::events::ToolOutcome::Success,
                ToolLoop::PermissionReject { .. } => {
                    crate::session::events::ToolOutcome::PermissionRejected
                }
                ToolLoop::Cancelled => crate::session::events::ToolOutcome::PermissionCancelled,
                ToolLoop::FollowupMessage(_) => crate::session::events::ToolOutcome::Followup,
                ToolLoop::HookDenied { .. } => crate::session::events::ToolOutcome::HookDenied,
                ToolLoop::NonExistingTool | ToolLoop::ToolParsingError => {
                    crate::session::events::ToolOutcome::InvalidTool
                }
            };
            let duration_ms = tool_start.elapsed().as_millis() as u64;
            self.signals_handle()
                .record_tool_duration(&prepared.tool_name, duration_ms);
            self.emit_event(crate::session::events::Event::ToolCompleted {
                tool_name: prepared.tool_name.clone(),
                duration_ms,
                outcome: tool_outcome,
            });
            self.observability_bridge
                .emit(
                    xai_tool_protocol::session_event::SessionEvent::ToolCallCompleted {
                        tool_call_id: prepared.call_id.clone(),
                        tool_name: prepared.tool_name.clone(),
                        duration_ms,
                        outcome: map_tool_outcome(tool_outcome),
                    },
                )
                .await;
            let (ext_file_path, ext_parameters) = if xai_grok_telemetry::external::is_active() {
                let parsed: Option<serde_json::Value> =
                    serde_json::from_str(&prepared.raw_arguments).ok();
                let file_path = parsed.as_ref().and_then(|v| {
                    ["file_path", "target_file", "filePath", "path"]
                        .iter()
                        .find_map(|k| v.get(*k).and_then(|p| p.as_str()))
                        .map(str::to_owned)
                });
                (file_path, parsed)
            } else {
                (None, None)
            };
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::events::ToolCallCompleted {
                    tool_name: prepared.tool_name.clone(),
                    outcome: tool_outcome,
                    duration_ms,
                    file_path: ext_file_path,
                    parameters: ext_parameters,
                },
            );
            tracing::info_span!(
                "tool.execution", tool_name = % prepared.tool_name, tool_use_id = %
                prepared.call_id, tool_input_size_bytes = prepared.raw_arguments.len() as
                i64, tool_result_size_bytes = tool_result_size_bytes, success =
                matches!(tool_outcome, crate ::session::events::ToolOutcome::Success),
                outcome = <&'static str >::from(tool_outcome),
            )
            .in_scope(|| {});
            if let Some(artifact) = compaction_artifact_read(&prepared.parsed_args) {
                tracing::info_span!(
                    "compaction.segment_read", session_id = % self.session_info.id.0,
                    tool_name = % prepared.tool_name, artifact = % artifact,
                    segment_index = artifact.segment_index().map(| i | i as i64), success
                    = matches!(tool_outcome, crate
                    ::session::events::ToolOutcome::Success),
                )
                .in_scope(|| {});
            }
            match &tool_loop {
                ToolLoop::PermissionReject { .. }
                | ToolLoop::Cancelled
                | ToolLoop::FollowupMessage(_) => {
                    if final_result.is_none() {
                        final_result = Some(tool_loop);
                    }
                }
                _ => {}
            }
        }
        {
            let _span = if !deferred_followups.is_empty() {
                Some(
                    tracing::info_span!(
                        "tools.deferred_followups",
                        count = deferred_followups.len()
                    )
                    .entered(),
                )
            } else {
                None
            };
            for chat in deferred_followups {
                self.chat_state_handle.push_user_message(chat);
            }
        }
        self.drain_pending_interjections().await;
        self.flush_pending_skill_reminders().await;
        if let Some(final_result) = final_result {
            return Ok(final_result);
        }
        Ok(ToolLoop::Continue)
    }
    /// Phase 1: pre-flight (MCP, args, hooks, permission, ExitPlanMode).
    pub(crate) async fn prepare_tool_call(
        &self,
        call: crate::sampling::types::ToolCallResponse,
        deferred_followups: &mut Vec<ConversationItem>,
    ) -> Result<Result<PreparedToolCall, ToolLoop>, acp::Error> {
        let tool_call_id = acp::ToolCallId::new(Arc::from(call.id.clone()));
        let model_id_str = self.current_model_id().await;
        tracing::info!(
            "Model requesting tool: name='{}', call_id='{}'",
            call.function.name,
            call.id,
        );
        {
            let _span = tracing::info_span!("tool.register").entered();
            let early_raw_input =
                serde_json::from_str::<serde_json::Value>(&call.function.arguments).ok();
            let subagent_background = matches!(
                call.function.name.as_str(),
                "task" | "Task" | "spawn_subagent"
            )
            .then(|| {
                early_raw_input
                    .as_ref()
                    .and_then(|v| v.get("run_in_background").or_else(|| v.get("background")))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true)
            });
            let mut meta = self.stamp_tool_meta(None, &call.function.name, None);
            if let Some(bg) = subagent_background {
                meta.get_or_insert_with(serde_json::Map::new).insert(
                    "subagentBackground".to_string(),
                    serde_json::Value::Bool(bg),
                );
            }
            self.send_update(
                acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(tool_call_id.clone(), call.function.name.clone())
                        .kind(acp::ToolKind::Other)
                        .status(acp::ToolCallStatus::Pending)
                        .raw_input(early_raw_input)
                        .meta(meta),
                ),
                None,
            )
            .await;
        }
        let mcp_parts = parse_mcp_tool_name(&call.function.name);
        let is_mcp_tool = mcp_parts.is_some();
        if let Some((ref server, _)) = mcp_parts
            && server.starts_with(crate::session::managed_mcp::MANAGED_MCP_PREFIX)
        {
            let _span = tracing::info_span!("tool.refresh_managed_mcp").entered();
            self.refresh_managed_mcp_if_stale().await;
        }
        if is_mcp_tool && !self.mcp_state.lock().await.is_initialized() {
            match self.mcp_strategy {
                McpInitStrategy::Blocking => {
                    let _span = tracing::info_span!("tool.wait_mcp_init").entered();
                    self.wait_for_mcp_initialized().await;
                }
                McpInitStrategy::Progressive => {
                    let err = anyhow::anyhow!(
                        "Tool not available. Use search_tool to find available tools."
                    );
                    let followups = self
                        .handle_tool_error(
                            &tool_call_id,
                            &call.id,
                            &call.function.name,
                            None,
                            &err,
                            &model_id_str,
                        )
                        .await;
                    deferred_followups.extend(followups);
                    return Ok(Err(ToolLoop::NonExistingTool));
                }
            }
        }
        let args_str = crate::session::helpers::tool_input_parsing::normalize_empty_arguments(
            &call.function.arguments,
        );
        let parse_result = serde_json::from_str::<serde_json::Value>(args_str);
        let mut concatenated_json_count: usize = 0;
        let raw_input = match &parse_result {
            Ok(value) => value.clone(),
            Err(e) => {
                if let Some(objects) = crate::session::helpers::tool_input_parsing::try_extract_concatenated_json_objects(
                    &call.function.arguments,
                ) {
                    let total_count = objects.len();
                    if objects.is_empty() {
                        json!({ "raw" : call.function.arguments.clone() })
                    } else {
                        let best_match = objects[0].clone();
                        let mut selected_index = 0;
                        let mut matched_tool = false;
                        let bridge = self.agent.borrow().tool_bridge().clone();
                        for (idx, obj) in objects.iter().enumerate() {
                            if bridge
                                .try_parse(&call.function.name, obj.clone())
                                .await
                                .is_ok()
                            {
                                selected_index = idx;
                                matched_tool = true;
                                break;
                            }
                        }
                        tracing::warn!(
                            tool_name = % call.function.name, call_id = % call.id,
                            total_objects = total_count, selected_index,
                            matched_named_tool = matched_tool,
                            "Detected concatenated JSON in tool arguments — \
                            extracting best matching object (index {selected_index}/{total_count}). \
                            The model should use separate tool calls instead of \
                            concatenating JSON objects."
                        );
                        concatenated_json_count = total_count;
                        best_match
                    }
                } else {
                    tracing::warn!(
                        "Failed to parse arguments as JSON ({}), wrapping in 'raw' field",
                        e
                    );
                    json!({ "raw" : call.function.arguments.clone() })
                }
            }
        };
        let tool_input = match self
            .agent
            .borrow()
            .tool_bridge()
            .try_parse(&call.function.name, raw_input.clone())
            .await
        {
            Ok(input) => input,
            Err(err) => {
                self.handle_tool_parse_error(
                    &tool_call_id,
                    &call.id,
                    &call.function.name,
                    err,
                    &call.function.arguments,
                    &model_id_str,
                )
                .await?;
                return Ok(Err(ToolLoop::ToolParsingError));
            }
        };
        let access_kind = AccessKind::from(&tool_input);
        let plan_gate = plan_mode_edit_gate(&self.plan_mode.lock(), &tool_input, &access_kind);
        if plan_gate != PlanEditGate::Allow {
            tracing::info_span!(
                "tool.decision", tool_name = % call.function.name, tool_use_id = % call
                .id, decision = "deny", source = "plan_mode", wait_ms = 0_i64,
            )
            .in_scope(|| {});
            let msg = self.plan_mode_edit_rejected_message().await;
            self.handle_tool_not_executed(&call.id, &tool_call_id, msg)
                .await?;
            return Ok(Err(ToolLoop::Continue));
        }
        let tool_call_display = self
            .send_tool_call_start(&tool_call_id, &call.function.name, tool_input.clone())
            .await;
        let _recovered_raw_input = if concatenated_json_count > 0 {
            Some(raw_input.clone())
        } else {
            None
        };
        let dispatch_target_name = tool_input.dispatch_target_name();
        let resolved_tool_name = dispatch_target_name
            .clone()
            .unwrap_or_else(|| call.function.name.clone());
        if self.hook_event_active(xai_grok_hooks::event::HookEventName::PreToolUse) {
            let (hook_tool_input, hook_tool_input_truncated) =
                xai_grok_hooks::event::truncate_payload(raw_input.clone());
            let envelope = self.make_hook_envelope(
                xai_grok_hooks::event::HookEventName::PreToolUse,
                None,
                xai_grok_hooks::event::HookPayload::PreToolUse {
                    tool_name: resolved_tool_name.clone(),
                    tool_use_id: call.id.clone(),
                    tool_input: hook_tool_input,
                    tool_input_truncated: hook_tool_input_truncated,
                    subagent_type: self.subagent_type_label(),
                },
            );
            let hook_registry_snapshot = self.hook_registry.borrow().clone();
            if let Some(registry) = hook_registry_snapshot {
                let ctx = self.hook_run_ctx();
                let pre_result =
                    xai_grok_hooks::dispatcher::dispatch_pre_tool_use(&registry, &envelope, &ctx)
                        .await;
                self.send_hook_execution(
                    "pre_tool_use",
                    Some(&resolved_tool_name),
                    None,
                    &pre_result.results,
                )
                .await;
                self.emit_hook_executed_telemetry(
                    "pre_tool_use",
                    Some(&resolved_tool_name),
                    &pre_result.results,
                )
                .await;
                if let xai_grok_hooks::result::HookDecision::Deny { reason, hook_name } =
                    pre_result.decision
                {
                    return Ok(Err(self
                        .deny_tool(
                            &call.id,
                            &tool_call_id,
                            resolved_tool_name.clone(),
                            hook_name,
                            reason,
                        )
                        .await?));
                }
            }
            if let Some(denied) = self
                .run_pre_tool_use_client_hook(&call, &tool_call_id, &envelope)
                .await?
            {
                return Ok(Err(denied));
            }
        }
        let plan_file_auto_approve = if let AccessKind::Edit(ref path) = access_kind {
            self.plan_mode
                .lock()
                .should_auto_approve_edit(std::path::Path::new(path))
        } else {
            false
        };
        if plan_file_auto_approve {
            tracing::info_span!(
                "tool.decision", tool_name = % call.function.name, tool_use_id = % call
                .id, decision = "allow", source = "config", wait_ms = 0_i64,
            )
            .in_scope(|| {});
        }
        if !plan_file_auto_approve {
            let (perm_title, perm_kind, perm_raw_input) = tool_call_display
                .as_ref()
                .map(|(t, k, r)| (Some(t.clone()), Some(*k), Some(r.clone())))
                .unwrap_or((None, None, None));
            let tool_call_update = acp::ToolCallUpdate::new(
                tool_call_id.clone(),
                acp::ToolCallUpdateFields::new()
                    .title(perm_title)
                    .kind(perm_kind)
                    .raw_input(perm_raw_input),
            )
            .meta(self.stamp_tool_meta(None, &call.function.name, Some(&tool_input)));
            let (telemetry_access_kind, _access_detail) = match &access_kind {
                xai_grok_workspace::permission::AccessKind::Read(p) => (
                    xai_grok_telemetry::events::AccessKind::Read,
                    p.clone().unwrap_or_default(),
                ),
                xai_grok_workspace::permission::AccessKind::Edit(p) => {
                    (xai_grok_telemetry::events::AccessKind::Edit, p.clone())
                }
                xai_grok_workspace::permission::AccessKind::Bash(cmd) => {
                    (xai_grok_telemetry::events::AccessKind::Bash, cmd.clone())
                }
                xai_grok_workspace::permission::AccessKind::Grep { path, glob } => (
                    xai_grok_telemetry::events::AccessKind::Grep,
                    path.clone().or_else(|| glob.clone()).unwrap_or_default(),
                ),
                xai_grok_workspace::permission::AccessKind::MCPTool { name, .. } => {
                    (xai_grok_telemetry::events::AccessKind::Mcp, name.clone())
                }
                xai_grok_workspace::permission::AccessKind::WebFetch(u) => {
                    (xai_grok_telemetry::events::AccessKind::Web, u.clone())
                }
                xai_grok_workspace::permission::AccessKind::WebSearch(q) => {
                    (xai_grok_telemetry::events::AccessKind::Web, q.clone())
                }
            };
            let subagent_session_id = if self.startup_hints.is_subagent {
                Some(self.session_id_string())
            } else {
                None
            };
            let perm_mode = if self.permissions.is_yolo_mode() {
                xai_grok_telemetry::enums::PermissionMode::AlwaysApprove
            } else if self.permissions.is_auto_mode() {
                xai_grok_telemetry::enums::PermissionMode::Auto
            } else {
                xai_grok_telemetry::enums::PermissionMode::Ask
            };
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::events::PermissionPrompted {
                    tool_name: call.function.name.clone(),
                    access_kind: telemetry_access_kind,
                    permission_mode: perm_mode,
                    subagent_session_id: subagent_session_id.clone(),
                    subagent_type: None,
                },
            );
            let perm_start = self.events.permission_requested(&call.function.name);
            debug_assert!(
                !self.session_info.id.0.is_empty(),
                "permission reverse-request must carry a non-empty sessionId (design §5.4)"
            );
            if !self.permissions.is_yolo_mode() {
                self.dispatch_notification_hook(
                    "permission_prompt",
                    Some("Tool permission requested".into()),
                    None,
                    Some("info".into()),
                )
                .await;
            }
            if self.permissions.is_auto_mode() {
                let conv = self.chat_state_handle.get_conversation().await;
                let turns = super::build_classifier_turns(&conv, super::CLASSIFIER_REFRESH_TURNS);
                if !turns.is_empty() {
                    self.permissions.set_classifier_transcript(turns);
                }
            }
            let edit_path_context = matches!(&access_kind, AccessKind::Edit(_)).then(|| {
                xai_grok_workspace::permission::types::EditPathContext {
                    real_cwd: std::path::PathBuf::from(self.session_info.cwd.as_str()),
                    display_cwd: self
                        .display_cwd
                        .get()
                        .map(|cwd| std::path::PathBuf::from(cwd.as_str())),
                }
            });
            let decision = {
                let _pending_guard =
                    crate::session::pending_interaction::PendingInteractionGuard::new(
                        self.pending_interactions.clone(),
                        self.notifications.gateway.clone(),
                        self.session_info.id.clone(),
                        tool_call_id.to_string(),
                        crate::session::pending_interaction::PendingKind::Permission,
                    );
                self.permissions
                    .request_with_edit_path_context(
                        access_kind.clone(),
                        tool_call_update,
                        edit_path_context,
                        Some(self.session_info.id.0.to_string()),
                        None,
                        None,
                    )
                    .await
            };
            self.events.permission_resolved(
                &call.function.name,
                match &decision {
                    Decision::Allow | Decision::Ask => {
                        xai_file_utils::events::types::PermissionDecision::Allow
                    }
                    Decision::Reject(_) | Decision::PolicyDeny(_) => {
                        xai_file_utils::events::types::PermissionDecision::Deny
                    }
                    Decision::Cancelled => {
                        xai_file_utils::events::types::PermissionDecision::Cancelled
                    }
                    Decision::FollowupMessage(_) => {
                        xai_file_utils::events::types::PermissionDecision::Followup
                    }
                },
                perm_start,
            );
            let wait_ms = perm_start.elapsed().as_millis() as u64;
            let (decision_outcome, _reject_reason) = match &decision {
                Decision::Allow | Decision::Ask => {
                    (xai_grok_telemetry::events::PermissionOutcome::Allow, None)
                }
                Decision::Reject(reason) | Decision::PolicyDeny(reason) => (
                    xai_grok_telemetry::events::PermissionOutcome::Deny,
                    Some(reason.to_string()),
                ),
                Decision::Cancelled => (
                    xai_grok_telemetry::events::PermissionOutcome::Cancelled,
                    None,
                ),
                Decision::FollowupMessage(_) => (
                    xai_grok_telemetry::events::PermissionOutcome::Followup,
                    None,
                ),
            };
            tracing::info_span!(
                "tool.decision", tool_name = % call.function.name, tool_use_id = % call
                .id, decision = decision_outcome.as_str(), source = crate
                ::session::telemetry::permission_decision_source(& decision, self
                .permissions.is_yolo_mode(),), wait_ms = wait_ms as i64,
            )
            .in_scope(|| {});
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::events::PermissionDecisionPayload {
                    tool_name: call.function.name.clone(),
                    access_kind: telemetry_access_kind,
                    decision: decision_outcome,
                    wait_ms,
                    permission_mode: perm_mode,
                    source: Some(
                        crate::session::telemetry::permission_decision_source(
                            &decision,
                            self.permissions.is_yolo_mode(),
                        )
                        .to_owned(),
                    ),
                    subagent_session_id: subagent_session_id.clone(),
                    subagent_type: None,
                },
            );
            match decision {
                Decision::PolicyDeny(ref reason) | Decision::Reject(ref reason) => {
                    let is_policy_deny = matches!(&decision, Decision::PolicyDeny(_));
                    let message = if is_policy_deny {
                        format!("Tool `{}` was not executed: {reason}", call.function.name)
                    } else {
                        format!("{reason} for tool `{}`", call.function.name)
                    };
                    self.handle_tool_not_executed(&call.id, &tool_call_id, message)
                        .await?;
                    let (tool_input_value, tool_input_truncated) =
                        xai_grok_hooks::event::truncate_payload(raw_input.clone());
                    self.dispatch_hook(
                        xai_grok_hooks::event::HookEventName::PermissionDenied,
                        xai_grok_hooks::event::HookPayload::PermissionDenied {
                            tool_name: resolved_tool_name.clone(),
                            tool_use_id: tool_call_id.to_string(),
                            tool_input: tool_input_value,
                            tool_input_truncated,
                        },
                        None,
                        Some(&resolved_tool_name),
                    )
                    .await;
                    let loop_action = if is_policy_deny {
                        ToolLoop::Continue
                    } else {
                        ToolLoop::PermissionReject {
                            tool_name: call.function.name.clone(),
                            reason: reason.clone(),
                        }
                    };
                    return Ok(Err(loop_action));
                }
                Decision::Cancelled => {
                    let message = format!(
                        "User cancelled the execution for tool `{}`",
                        call.function.name
                    );
                    self.handle_tool_not_executed(&call.id, &tool_call_id, message)
                        .await?;
                    return Ok(Err(ToolLoop::Cancelled));
                }
                Decision::FollowupMessage(followup_message) => {
                    let message = format!(
                        "The user elected to avoid running the {} tool. The tool was not executed. \
                         Please refer to the user's message for next steps.",
                        call.function.name
                    );
                    self.handle_tool_not_executed(&call.id, &tool_call_id, message)
                        .await?;
                    return Ok(Err(ToolLoop::FollowupMessage(followup_message)));
                }
                Decision::Allow | Decision::Ask => {}
            }
        }
        let is_exit_plan_mode = matches!(&tool_input, ToolInput::ExitPlanMode(_));
        let is_cursor_switch_to_agent = false;
        let is_cursor_create_plan = false;
        let plan_file_path = self.plan_mode.lock().plan_file_path().to_path_buf();
        let plan_read = if is_exit_plan_mode || is_cursor_switch_to_agent || is_cursor_create_plan {
            let inline_cursor_plan: Option<PlanFileRead> = None;
            if let Some(plan) = inline_cursor_plan {
                plan
            } else {
                let io_result = tokio::fs::read_to_string(&plan_file_path).await;
                if let Err(ref e) = io_result
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(
                        path = % plan_file_path.display(), error = % e,
                        "[exit_plan_mode] plan file unreadable; intercepting anyway"
                    );
                }
                classify_plan_file_read(io_result)
            }
        } else {
            PlanFileRead::Absent
        };
        let plan_content = match &plan_read {
            PlanFileRead::Present(s) => Some(s.clone()),
            PlanFileRead::Absent | PlanFileRead::Unreadable => None,
        };
        if should_intercept_exit_plan_approval(
            is_exit_plan_mode,
            is_cursor_switch_to_agent,
            is_cursor_create_plan,
            &plan_read,
        ) {
            tracing::info!(
                tool_call_id = % tool_call_id, cursor_create_plan =
                is_cursor_create_plan, cursor_switch_to_agent =
                is_cursor_switch_to_agent, has_plan_content = plan_content.is_some(),
                "[exit_plan_mode] intercepted, sending ext_method to client"
            );
            let resp = self
                .request_plan_approval(&tool_call_id, plan_content.clone())
                .await;
            match resp {
                Ok(parsed) => match PlanApprovalOutcome::from_response(&parsed) {
                    PlanApprovalOutcome::Abandoned => {
                        tracing::info!("[exit_plan_mode] user abandoned plan — deactivating");
                        self.leave_plan_mode_to_default();
                        let message = format!(
                            "The user chose to abandon the plan entirely (via the Abandon option in the plan approval dialog). Plan mode has been disabled. Do not call {} again unless the user explicitly asks to re-enter plan mode.",
                            call.function.name
                        );
                        let tool_update = acp::ToolCallUpdate::new(
                            tool_call_id.clone(),
                            acp::ToolCallUpdateFields::new()
                                .status(Some(acp::ToolCallStatus::Completed))
                                .content(Some(vec![acp::ToolCallContent::from(
                                    acp::ContentBlock::Text(acp::TextContent::new(message.clone())),
                                )])),
                        );
                        self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_update), None)
                            .await;
                        let tool_chat = ConversationItem::tool_result(call.id.clone(), message);
                        self.chat_state_handle.push_tool_result(tool_chat);
                        return Ok(Err(ToolLoop::Continue));
                    }
                    PlanApprovalOutcome::Cancelled => {
                        let message = if plan_content.is_some() {
                            revise_plan_message(parsed.feedback.as_deref().unwrap_or(""))
                        } else {
                            "The user does not want to exit plan mode. \
                             Continue planning and ask the user what they would like to do."
                                .to_string()
                        };
                        let tool_update = acp::ToolCallUpdate::new(
                            tool_call_id.clone(),
                            acp::ToolCallUpdateFields::new()
                                .status(Some(acp::ToolCallStatus::Completed))
                                .content(Some(vec![acp::ToolCallContent::from(
                                    acp::ContentBlock::Text(acp::TextContent::new(message.clone())),
                                )])),
                        );
                        self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_update), None)
                            .await;
                        let tool_chat = ConversationItem::tool_result(call.id.clone(), message);
                        self.chat_state_handle.push_tool_result(tool_chat);
                        return Ok(Err(ToolLoop::Continue));
                    }
                    PlanApprovalOutcome::Approved => {
                        tracing::info!("[exit_plan_mode] user approved — executing tool");
                    }
                },
                Err(err) => {
                    if ext_method_no_client(&err) {
                        tracing::debug!(
                            % err, "exit_plan_mode: no client wired; executing tool"
                        );
                    } else {
                        tracing::info!(
                            % err,
                            "exit_plan_mode: client disconnected mid-approval; plan mode stays active"
                        );
                        let message = "Plan approval could not be completed because the \
                             client disconnected. Plan mode remains active; the approval \
                             will reappear on reconnect."
                            .to_string();
                        self.handle_tool_not_executed(&call.id, &tool_call_id, message)
                            .await?;
                        return Ok(Err(ToolLoop::Cancelled));
                    }
                }
            }
        } else if is_cursor_switch_to_agent {
            tracing::info!(
                tool_call_id = % tool_call_id,
                "[exit_plan_mode] cursor SwitchMode(agent) with empty plan — skipping intercept"
            );
        }
        let is_read_only = self
            .agent
            .borrow()
            .tool_bridge()
            .tool_kind(&call.function.name)
            .map(|k| {
                use xai_grok_tools::types::tool::ToolKind;
                matches!(
                    k,
                    ToolKind::Read
                        | ToolKind::Search
                        | ToolKind::Lsp
                        | ToolKind::ListDir
                        | ToolKind::List
                        | ToolKind::MemorySearch
                        | ToolKind::MemoryGet
                        | ToolKind::WebSearch
                        | ToolKind::WebFetch
                        | ToolKind::EnterPlan
                        | ToolKind::ExitPlan
                        | ToolKind::AskUser
                )
            })
            .unwrap_or(false);
        let prepared = PreparedToolCall {
            call_id: call.id.clone(),
            tool_call_id,
            tool_name: call.function.name.clone(),
            raw_arguments: call.function.arguments.clone(),
            parsed_args: raw_input.clone(),
            model_id: model_id_str,
            concatenated_json_count,
            dispatch_target_name,
            is_read_only,
        };
        Ok(Ok(prepared))
    }
    /// Issue the `x.ai/exit_plan_mode` reverse-request and await the user's
    /// decision. Shared by the mid-turn intercept and the resume
    /// re-park. Marks `awaiting_plan_approval` while the request is
    /// outstanding and clears it on every exit path via [`AwaitingApprovalGuard`].
    pub(super) async fn request_plan_approval(
        &self,
        tool_call_id: &acp::ToolCallId,
        plan_content: Option<String>,
    ) -> Result<
        xai_grok_tools::implementations::grok_build::exit_plan_mode::ExitPlanModeExtResponse,
        acp::Error,
    > {
        use agent_client_protocol::Client as _;
        use xai_grok_tools::implementations::grok_build::exit_plan_mode::{
            ExitPlanModeExtRequest, ExitPlanModeExtResponse,
        };
        let ext_req = ExitPlanModeExtRequest {
            session_id: self.session_id_string(),
            tool_call_id: tool_call_id.to_string(),
            plan_content,
        };
        debug_assert!(
            !ext_req.session_id.is_empty(),
            "exit_plan_mode reverse-request must carry a non-empty sessionId (design §5.4)"
        );
        let ext_request = acp::ExtRequest::new(
            "x.ai/exit_plan_mode",
            serde_json::value::to_raw_value(&ext_req)
                .expect("ExitPlanModeExtRequest serialization should not fail")
                .into(),
        );
        self.dispatch_notification_hook(
            "permission_prompt",
            Some("Plan approval requested".into()),
            None,
            Some("info".into()),
        )
        .await;
        self.plan_mode.lock().set_awaiting_plan_approval(true);
        self.persist_plan_mode_state();
        let clear_awaiting = AwaitingApprovalGuard(self);
        let resp = {
            let _pending_guard = crate::session::pending_interaction::PendingInteractionGuard::new(
                self.pending_interactions.clone(),
                self.notifications.gateway.clone(),
                self.session_info.id.clone(),
                tool_call_id.to_string(),
                crate::session::pending_interaction::PendingKind::PlanApproval,
            );
            self.notifications.gateway.ext_method(ext_request).await
        };
        let raw = match resp {
            Ok(raw) => raw,
            Err(err) => {
                clear_awaiting.disarm();
                return Err(err);
            }
        };
        Ok(
            serde_json::from_str::<ExitPlanModeExtResponse>(raw.0.get()).unwrap_or_else(|_| {
                ExitPlanModeExtResponse {
                    outcome: "cancelled".into(),
                    feedback: None,
                }
            }),
        )
    }
    /// Leave plan mode (approved/abandoned) and tell the client to show the
    /// Default mode. Mirrors the mid-turn exit so the resume re-park
    /// drives the mode change through the same path.
    fn leave_plan_mode_to_default(&self) {
        let deactivated = self.plan_mode.lock().deactivate_approved();
        if deactivated {
            *self.current_prompt_mode.lock() = PromptMode::Agent;
            *self.turn_prompt_mode.lock() = PromptMode::Agent;
            self.persist_plan_mode_state();
            self.enqueue_current_mode_update(acp::SessionModeId::new(
                xai_grok_tools::types::SessionMode::Default.as_id(),
            ));
        }
    }
    /// Resume hook: re-issue the parked `exit_plan_mode` approval
    /// after a session restored with `awaiting_plan_approval == true`, so the
    /// client re-shows approval chrome over a real live waiter. Handles the
    /// decision with no in-flight turn — approve: leave plan mode + start an
    /// implement turn; request-changes: stay in plan mode + feed the comments
    /// back as a turn; abandon: leave plan mode and wait for the user.
    pub(super) async fn resume_plan_approval(
        self: Arc<Self>,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) {
        if !self.plan_mode.lock().is_awaiting_plan_approval() {
            return;
        }
        if crate::session::pending_interaction::has_parked_plan_approval(&self.pending_interactions)
        {
            tracing::debug!("[exit_plan_mode] resume: approval already pending; skip re-park");
            return;
        }
        let plan_path = self.plan_mode.lock().plan_file_path().to_path_buf();
        let plan_content = match tokio::fs::read_to_string(&plan_path).await {
            Ok(s) if !s.trim().is_empty() => s,
            _ => {
                tracing::info!("[exit_plan_mode] resume: no plan.md; clearing awaiting flag");
                self.plan_mode.lock().set_awaiting_plan_approval(false);
                self.persist_plan_mode_state();
                return;
            }
        };
        let tool_call_id = acp::ToolCallId::new(Arc::from(
            format!("exit-plan-mode-resume-{}", self.session_info.id.0).as_str(),
        ));
        tracing::info!(
            tool_call_id = % tool_call_id,
            "[exit_plan_mode] re-parking approval after resume"
        );
        let parsed = match self
            .request_plan_approval(&tool_call_id, Some(plan_content))
            .await
        {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::debug!(% err, "resume exit_plan_mode reverse-request failed");
                return;
            }
        };
        match resume_action_for(PlanApprovalOutcome::from_response(&parsed), parsed.feedback) {
            ResumeAction::LeaveOnly => {
                tracing::info!("[exit_plan_mode] resume: user abandoned plan");
                self.leave_plan_mode_to_default();
            }
            ResumeAction::StayAndRevise(text) => {
                tracing::info!("[exit_plan_mode] resume: user requested changes");
                self.start_resume_turn(text, PromptMode::Plan, completion_tx)
                    .await;
            }
            ResumeAction::LeaveAndImplement => {
                tracing::info!("[exit_plan_mode] resume: user approved plan");
                self.leave_plan_mode_to_default();
                self.start_resume_turn(
                    PLAN_APPROVED_IMPLEMENT_MESSAGE.to_string(),
                    PromptMode::Agent,
                    completion_tx,
                )
                .await;
            }
        }
    }
    /// Inject a synthetic user turn after a resumed plan decision and kick the
    /// scheduler (no in-flight turn exists on resume to continue).
    async fn start_resume_turn(
        self: Arc<Self>,
        text: String,
        mode: PromptMode,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) {
        let prompt_id = format!("plan-resume-{}", chrono::Utc::now().timestamp_millis());
        let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(text))];
        let (respond_to, _rx) = oneshot::channel();
        self.queue_input(
            prompt_blocks,
            prompt_id,
            mode,
            None,
            None,
            None,
            None,
            false,
            None,
            false,
            None,
            respond_to,
            None,
            None,
        )
        .await;
        SessionActor::maybe_start_running_task(self.clone(), completion_tx).await;
    }
    /// Refine the initial (minimal) ToolCall that was registered during
    /// tool preparation.  Now that we have a fully parsed `ToolInput`
    /// we can send a `ToolCallUpdate` with a human-readable title, the correct
    /// kind, file locations, and the serialised raw input.
    ///
    /// Returns `(title, kind, raw_input)` so callers can reuse them (e.g. in
    /// the permission-request update for subagent sessions whose prior
    /// `SessionUpdate` events the client may have suppressed).
    async fn send_tool_call_start(
        &self,
        tool_call_id: &acp::ToolCallId,
        wire_name: &str,
        tool_call_input: ToolInput,
    ) -> Result<(String, acp::ToolKind, serde_json::Value), acp::Error> {
        #[allow(unused_mut)]
        let mut raw_input = serde_json::to_value(&tool_call_input)?;
        let canonical_meta = self.stamp_tool_meta(None, wire_name, Some(&tool_call_input));
        let (title, kind, locations, content) = match tool_call_input {
            ToolInput::ListDir(list_dir) => (
                format!("List `{}`", list_dir.target_directory),
                acp::ToolKind::Other,
                vec![acp::ToolCallLocation::new(
                    list_dir.target_directory.clone(),
                )],
                vec![],
            ),
            ToolInput::SearchReplace(sr) => {
                let display_path = self.tool_context.cwd.join(&sr.file_path).to_path_buf();
                let meta = if !sr.old_string.is_empty() {
                    let _span = tracing::info_span!("tool.sr_line_lookup").entered();
                    self.tool_context
                        .fs
                        .read_to_string(&display_path)
                        .await
                        .ok()
                        .and_then(|file_content| {
                            let pos = file_content.find(&sr.old_string)?;
                            let line = file_content[..pos].matches('\n').count() + 1;
                            serde_json::json!({ "old_line" : line, "new_line" : line, })
                                .as_object()
                                .cloned()
                        })
                } else {
                    None
                };
                (
                    format!("Edit `{}`", sr.file_path.as_str()),
                    acp::ToolKind::Edit,
                    vec![acp::ToolCallLocation::new(sr.file_path.clone())],
                    vec![acp::ToolCallContent::from(
                        acp::Diff::new(display_path, sr.new_string)
                            .old_text(Some(sr.old_string))
                            .meta(meta),
                    )],
                )
            }
            ToolInput::Bash(bash_tool) => execute_tool_call_parts(
                &bash_tool.command,
                Some(bash_tool.description.as_str()),
                self.tool_context.cwd.as_path(),
            ),
            ToolInput::ReadFile(read_file) => (
                format!("Read `{}`", read_file.path.clone()),
                acp::ToolKind::Read,
                vec![
                    acp::ToolCallLocation::new(read_file.path).line(
                        xai_grok_tools::normalization::norm_offset_i64(read_file.offset)
                            .map(|l| l as u32),
                    ),
                ],
                Vec::new(),
            ),
            ToolInput::TodoWrite(_) => (
                "Updating plan".to_string(),
                acp::ToolKind::Think,
                Vec::new(),
                Vec::new(),
            ),
            ToolInput::Grep(gs) => (gs.pattern.clone(), acp::ToolKind::Search, vec![], vec![]),
            ToolInput::WebSearch(ws) => (
                format!("Web search: \"{}\"", ws.query),
                acp::ToolKind::Search,
                vec![],
                vec![],
            ),
            ToolInput::ImageGen(ig) => (
                format!("imagine: {}", ig.prompt),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::ImageEdit(ie) => (
                format!("imagine-edit: {}", ie.prompt),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::ImageToVideo(i2v) => (
                format!(
                    "image-to-video: {}",
                    i2v.prompt.as_deref().unwrap_or(&i2v.image)
                ),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::ReferenceToVideo(r2v) => (
                format!("reference-to-video: {}", r2v.prompt),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::MCPTool(mcp_tool) => (
                mcp_tool.tool_name.to_owned(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::TaskOutput(task_output) => {
                let ids = task_output.resolved_task_ids();
                let label = match ids.as_slice() {
                    [] => "Get task output".to_string(),
                    [one] => format!("Get task output: {one}"),
                    many => format!("Get task output: {} tasks", many.len()),
                };
                (label, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::WaitTasks(wait) => (
                format!(
                    "Wait tasks: {} ids, mode={}",
                    wait.task_ids.len(),
                    match wait.mode {
                        xai_tool_types::WaitMode::WaitAny => "wait_any",
                        xai_tool_types::WaitMode::WaitAll => "wait_all",
                    }
                ),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::KillTask(kill_task) => (
                format!("Kill task: {}", kill_task.task_id),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::Skill(skill) => {
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::events::SkillDispatched {
                        skill_name: skill.skill.clone(),
                        plugin_source: None,
                    },
                );
                tracing::info_span!(
                    "skill.activated", skill_name = % skill.skill, invocation_trigger =
                    "skill_tool",
                )
                .in_scope(|| {});
                (
                    format!("Skill: {}", skill.skill),
                    acp::ToolKind::Other,
                    vec![],
                    vec![],
                )
            }
            ToolInput::ApplyPatch(_) => (
                "Apply patch".to_string(),
                acp::ToolKind::Edit,
                vec![],
                vec![],
            ),
            ToolInput::Dynamic(_) => (
                "Dynamic tool call".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::MemorySearch(ms) => {
                let end = ms
                    .query
                    .char_indices()
                    .nth(60)
                    .map_or(ms.query.len(), |(i, _)| i);
                let display = &ms.query[..end];
                (
                    format!("Memory search: \"{display}\""),
                    acp::ToolKind::Other,
                    vec![],
                    vec![],
                )
            }
            ToolInput::MemoryGet(mg) => (
                format!("Memory read: {}", mg.path),
                acp::ToolKind::Read,
                vec![],
                vec![],
            ),
            ToolInput::HashlineEdit(he) => (
                format!("Edit `{}`", he.file_path),
                acp::ToolKind::Edit,
                vec![acp::ToolCallLocation::new(he.file_path.clone())],
                vec![],
            ),
            ToolInput::Task(task) => (
                task.description.clone(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::EnterPlanMode(_) => (
                "Plan: Enter".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::ExitPlanMode(_) => (
                "Plan: Exit".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::AskUserQuestion(ref ask) => {
                let title = if ask.questions.len() == 1 {
                    format!("Ask: {}", ask.questions[0].question)
                } else {
                    format!("Ask {} questions", ask.questions.len())
                };
                (title, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::WebFetch(wf) => (
                format!("Fetch: {}", wf.url),
                acp::ToolKind::Fetch,
                vec![],
                vec![],
            ),
            ToolInput::SearchTool(st) => (
                format!("Search tools: \"{}\"", st.query),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::UseTool(ut) => (ut.tool_name.clone(), acp::ToolKind::Other, vec![], vec![]),
            ToolInput::Write(ref w) => (
                format!("Write `{}`", w.file_path),
                acp::ToolKind::Edit,
                vec![acp::ToolCallLocation::new(w.file_path.clone())],
                vec![acp::ToolCallContent::from(
                    acp::Diff::new(
                        self.tool_context.cwd.join(&w.file_path).to_path_buf(),
                        w.content.clone(),
                    )
                    .old_text(Some(String::new())),
                )],
            ),
            ToolInput::Workflow(ref w) => {
                let script_name = |script: &str| -> Option<String> {
                    let head = script.get(..600).unwrap_or(script);
                    let rest = &head[head.find("name:")? + 5..];
                    let rest = &rest[rest.find('"')? + 1..];
                    Some(rest[..rest.find('"')?].to_string())
                };
                let inline_name = w.script.as_deref().and_then(script_name);
                let title = if w.validate_only {
                    match inline_name.or_else(|| w.name.clone()) {
                        Some(n) => format!("Validating workflow '{n}'"),
                        None => "Validating workflow script".to_string(),
                    }
                } else if w.script.is_some() {
                    match inline_name {
                        Some(n) => format!("Creating workflow '{n}'"),
                        None => "Creating workflow".to_string(),
                    }
                } else if let Some(ref name) = w.name {
                    format!("Workflow: {name}")
                } else if w.resume_from_run_id.is_some() {
                    "Workflow: resume run".to_string()
                } else {
                    "Workflow: launch script".to_string()
                };
                (title, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::UpdateGoal(ref ug) => {
                let title = if ug.completed == Some(true) {
                    "Goal: marking complete".to_string()
                } else if let Some(ref reason) = ug.blocked_reason {
                    format!("Goal: blocked — {reason}")
                } else if let Some(ref msg) = ug.message {
                    format!("Goal: {msg}")
                } else {
                    "Goal: update".to_string()
                };
                (title, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::Monitor(ref m) => (
                format!("Start monitor: {}", m.description),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::SchedulerCreate(ref sc) => {
                let title = match (&sc.task_id, &sc.interval) {
                    (Some(id), Some(interval)) => {
                        format!("Update scheduled task {id} (every {interval})")
                    }
                    (Some(id), None) => format!("Update scheduled task {id}"),
                    (None, Some(interval)) => {
                        format!("Create scheduled task (every {interval})")
                    }
                    (None, None) => "Create scheduled task".to_string(),
                };
                (title, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::SchedulerDelete(ref sd) => (
                format!("Delete scheduled task: {}", sd.id),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::SchedulerList(_) => (
                "List scheduled tasks".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            #[allow(unreachable_patterns)]
            _ => (
                "Tool call".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
        };
        let tool_call_update = acp::ToolCallUpdate::new(
            tool_call_id.clone(),
            acp::ToolCallUpdateFields::new()
                .title(Some(title.clone()))
                .kind(Some(kind))
                .locations(Some(locations))
                .content(if content.is_empty() {
                    None
                } else {
                    Some(content)
                })
                .raw_input(Some(raw_input.clone())),
        )
        .meta(canonical_meta);
        self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_call_update), None)
            .await;
        Ok((title, kind, raw_input))
    }
    async fn handle_tool_parse_error(
        &self,
        tool_call_id: &acp::ToolCallId,
        call_id: &str,
        function_name: &str,
        err: xai_tool_runtime::ToolError,
        raw_arguments: &str,
        model_id: &str,
    ) -> Result<(), acp::Error> {
        tracing::error!(
            session_id = % self.session_info.id.0, tool_name = function_name, model_id =
            model_id, error_kind = "parse_failure", error_message = % err,
            "tool_error: parse_failure"
        );
        self.signals_handle().record_tool_failure(function_name);
        let message = build_tool_parse_error_message(function_name, &err, raw_arguments);
        self.send_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                tool_call_id.clone(),
                acp::ToolCallUpdateFields::new()
                    .status(Some(acp::ToolCallStatus::Failed))
                    .content(Some(vec![acp::ToolCallContent::from(
                        acp::ContentBlock::Text(acp::TextContent::new(message.clone())),
                    )])),
            )),
            None,
        )
        .await;
        let tool_chat = ConversationItem::tool_result(call_id.to_string(), message);
        self.chat_state_handle.push_tool_result(tool_chat);
        Ok(())
    }
    /// Sweep `pending_inputs` and `pending_notifications` for entries
    /// matching `consumed_ids`. Called after every successful tool result
    /// so that queued auto-wake synthetic prompts for a task/subagent the
    /// model already learned about are dropped before they get flushed to
    /// chat history (which would surface as a trailing
    /// `<system-reminder>` with no assistant reply).
    ///
    /// The ID list comes from
    /// `xai_grok_tools::reminders::task_completion::consumed_completion_ids`,
    /// which is the same predicate used by `TaskCompletionReminder` —
    /// they cannot drift because they share the function.
    ///
    /// Reservations are deliberately not released here because the tool result
    /// that triggered this sweep is the canonical consumption surface, and
    /// `TaskCompletionReminder` already suppresses the per-tool-call
    /// reminder for these IDs via its own suppress list (also derived
    /// from `consumed_completion_ids`). Un-marking here would risk a
    /// duplicate reminder for an ID that was just consumed.
    ///
    /// Note on `MonitorEvent` interaction: any pending `MonitorEvent`
    /// notification whose `task_id` matches a consumed completion is
    /// also dropped. This is intentional — the model just learned via
    /// the `get_task_output` / `kill_task` result that the task is
    /// done, so any pending monitor stdout for it is stale.
    pub(super) async fn drop_pending_items_for_consumed_completions(&self, consumed_ids: &[&str]) {
        if consumed_ids.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        let dropped = state.sweep_pending_inputs(|i| {
            i.origin
                .completion_id()
                .is_some_and(|id| consumed_ids.contains(&id))
        });
        let dropped_inputs = dropped.len();
        let before_notifications = state.pending_notifications.len();
        state
            .pending_notifications
            .retain(|n| !consumed_ids.contains(&n.source.task_id()));
        let dropped_notifications = before_notifications - state.pending_notifications.len();
        drop(state);
        if let Some(reservations) = &self.tool_context.task_completion_reservations {
            for task_id in dropped
                .iter()
                .filter_map(|input| input.origin.completion_id())
            {
                reservations.release(task_id);
            }
        }
        if dropped_inputs > 0 || dropped_notifications > 0 {
            tracing::info!(
                dropped_inputs, dropped_notifications, consumed_ids = ? consumed_ids,
                "auto-wake: dropped queued synthetic items for consumed completions"
            );
        }
    }
    /// Drain all queued synthetic prompts (auto-wake task/subagent
    /// completions, notification-drain batches, and goal-summary turns —
    /// every `PromptOrigin` variant where `is_synthetic()` returns
    /// `true`) from `pending_inputs`, and clear ALL
    /// `pending_notifications` unconditionally (every current
    /// `NotificationSource` variant is sourced from a synthetic event).
    ///
    /// Called from `SessionCommand::Shutdown` as a defensive backstop
    /// so a synthetic prompt that slipped past the per-tool-result
    /// sweep cannot be flushed to `chat_history.jsonl` after the actor
    /// returns. Real user inputs are preserved.
    pub(super) async fn drop_pending_synthetic_items(&self) {
        let mut state = self.state.lock().await;
        let mut kept = VecDeque::with_capacity(state.pending_inputs.len());
        let mut dropped = Vec::new();
        for input in std::mem::take(&mut state.pending_inputs) {
            if input.origin.is_synthetic() {
                dropped.push(input);
            } else {
                kept.push_back(input);
            }
        }
        state.pending_inputs = kept;
        state.pending_notifications.clear();
        drop(state);
        if let Some(reservations) = &self.tool_context.task_completion_reservations {
            for task_id in dropped
                .iter()
                .filter_map(|input| input.origin.completion_id())
            {
                reservations.release(task_id);
            }
        }
    }
    /// Record git/PR ops from a successful tool result into session signals
    /// (`turn_result.json`) and telemetry. Detection runs here at the shell's
    /// tool-result chokepoint over the command + prompt output (nothing is
    /// wired through the tool's output schema): successful foreground bash
    /// commands, plus MCP `create_pull_request` results (url/number parsed
    /// from the result text). Backgrounded commands are not scanned.
    fn record_git_pr_signals(&self, effective_tool_name: &str, result: &ToolRunResult) {
        use xai_grok_telemetry::enums::PrCreationSource;
        use xai_grok_tools::util::git_detect;
        match &result.output {
            xai_grok_tools::types::output::ToolOutput::Bash(b) if b.exit_code == 0 => {
                let Some(ops) = git_detect::detect_git_ops(&b.command, &b.output_for_prompt) else {
                    return;
                };
                if ops.committed {
                    self.signals_handle().record_git_commit();
                }
                if let Some(pr) = ops.pr_created {
                    self.record_pr_created(pr, PrCreationSource::Bash);
                }
                if ops.pr_merged {
                    self.signals_handle().record_pr_merged();
                    xai_grok_telemetry::session_ctx::log_event(
                        xai_grok_telemetry::events::PrMerged {},
                    );
                }
            }
            xai_grok_tools::types::output::ToolOutput::MCP(m)
                if !m.is_error && is_mcp_create_pull_request(effective_tool_name) =>
            {
                let pr = git_detect::PrRef::find_in(&result.prompt_text).unwrap_or_default();
                self.record_pr_created(pr, PrCreationSource::Mcp);
            }
            _ => {}
        }
    }
    /// Record a PR creation into session signals.
    ///
    /// `had_commit_in_session` is provisional here: the signals actor
    /// reconciles it at `TakeTurnEndSnapshot`, after every event of the turn
    /// has been processed, so out-of-order parallel tool results (a create
    /// landing before a sibling commit) cannot mis-attribute. The Mixpanel
    /// `pr_created` event is emitted from the reconciled turn-end delta in
    /// `finalize_turn_bookkeeping`.
    fn record_pr_created(
        &self,
        pr: xai_grok_tools::util::git_detect::PrRef,
        source: xai_grok_telemetry::enums::PrCreationSource,
    ) {
        self.signals_handle()
            .record_pr_created(crate::session::signals::PrCreatedSignal {
                url: pr.url,
                number: pr.number,
                source,
                had_commit_in_session: false,
            });
    }
    pub(super) async fn handle_bridge_tool_success(
        &self,
        tool_call_id: &acp::ToolCallId,
        call_id: &str,
        requested_tool_name: &str,
        effective_tool_name: &str,
        result: ToolRunResult,
        concatenated_json_count: usize,
        model_id: &str,
        tool_parsed_args: &serde_json::Value,
    ) -> Result<Vec<ConversationItem>, acp::Error> {
        use crate::session::acp_conversion::{acp_plan_update, acp_tool_update, maybe_rewrite};
        let consumed_ids =
            xai_grok_tools::reminders::task_completion::consumed_completion_ids(&result.output);
        if !consumed_ids.is_empty() {
            self.drop_pending_items_for_consumed_completions(&consumed_ids)
                .await;
        }
        if let ToolsToolOutput::BackgroundTaskStarted(ref bg) = result.output {
            self.record_goal_turn_task_ids([bg.task_id.clone()]);
        }
        if matches!(
            &result.output,
            ToolsToolOutput::SearchReplace(
                xai_grok_tools::types::output::SearchReplaceOutput::EditsApplied(_)
            ) | ToolsToolOutput::Bash(_)
        ) {
            self.maybe_notify_git_branch().await;
        }
        if let xai_grok_tools::types::output::ToolOutput::Bash(ref b) = result.output
            && b.was_bare_echo
        {
            self.signals_handle().record_bare_echo();
        }
        self.record_git_pr_signals(effective_tool_name, &result);
        let path_rewriter = self.path_rewriter();
        let tool_meta = {
            let state = self.mcp_state.lock().await;
            state.mcp_tool_meta.get(effective_tool_name).cloned()
        };
        if let Some(mut tool_update) =
            acp_tool_update(&result.output, call_id, path_rewriter.as_ref(), tool_meta)
        {
            if tool_update.fields.status == Some(acp::ToolCallStatus::Failed) {
                tracing::error!(
                    session_id = % self.session_info.id.0, tool_name =
                    requested_tool_name, effective_tool_name = effective_tool_name,
                    model_id = model_id, error_kind = "tool_output_error",
                    "tool_error: tool_output_error"
                );
                self.signals_handle()
                    .record_tool_failure(requested_tool_name);
            } else {
                self.signals_handle()
                    .record_tool_success(requested_tool_name);
            }
            if matches!(
                &result.output,
                xai_grok_tools::types::output::ToolOutput::EnterPlanMode(_)
                    | xai_grok_tools::types::output::ToolOutput::ExitPlanMode(_)
            ) {
                let plan_path = self.plan_mode.lock().plan_file_path().display().to_string();
                if let Some(ref mut content) = tool_update.fields.content {
                    for item in content.iter_mut() {
                        if let acp::ToolCallContent::Content(acp::Content {
                            content: acp::ContentBlock::Text(t),
                            ..
                        }) = item
                        {
                            t.text = format!("Plan file: {}", plan_path);
                        }
                    }
                }
            }
            tool_update.tool_call_id = tool_call_id.clone();
            self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_update), None)
                .await;
        } else {
            self.signals_handle()
                .record_tool_success(requested_tool_name);
        }
        if let Some(acp_plan) = acp_plan_update(&result.output) {
            self.send_update(acp::SessionUpdate::Plan(acp_plan), None)
                .await;
        }
        #[allow(unused_mut)]
        let mut prompt_text = if concatenated_json_count > 0 && !self.is_cursor_harness() {
            let remaining = concatenated_json_count - 1;
            format!(
                "{}\n\n<system-reminder>\nIMPORTANT: Your tool call contained {} concatenated JSON \
                 objects, but only the best-matching one was executed. The remaining {} \
                 were ignored. You MUST use separate tool calls (one per operation) \
                 instead of concatenating multiple JSON objects in a single call's \
                 arguments. Make {} individual tool call{} for the remaining \
                 operations.\n</system-reminder>",
                result.prompt_text,
                concatenated_json_count,
                remaining,
                remaining,
                if remaining == 1 { "" } else { "s" },
            )
        } else {
            result.prompt_text
        };
        let mut inline_images: Vec<ContentPart> = Vec::new();
        let extraction = if !self.is_cursor_harness()
            && !matches!(
                result.output,
                ToolsToolOutput::ReadFile(ReadFileOutput::ImageContent(_))
                    | ToolsToolOutput::ReadFile(ReadFileOutput::PdfPageImages(_))
            ) {
            xai_grok_tools::util::base64_images::extract_base64_images(prompt_text)
        } else {
            xai_grok_tools::util::base64_images::ExtractionResult {
                text: prompt_text,
                images: Vec::new(),
            }
        };
        let mut extracted_images = extraction.images;
        let prompt_text = extraction.text;
        if !self.is_cursor_harness()
            && let ToolsToolOutput::ReadFile(ReadFileOutput::FileContent(ref fc)) = result.output
        {
            extracted_images.extend(fc.extracted_images.iter().cloned());
        }
        let mut prompt_text = maybe_rewrite(path_rewriter.as_ref(), prompt_text);
        if !self.is_cursor_harness()
            && let ToolsToolOutput::ReadFile(ReadFileOutput::ImageContent(ref image_content)) =
                result.output
        {
            let path = tool_parsed_args
                .get("target_file")
                .or_else(|| tool_parsed_args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            use crate::session::image_normalize::{InlineAttachVerdict, inline_attach_verdict};
            match inline_attach_verdict(&image_content.data) {
                InlineAttachVerdict::TooSmall => {
                    prompt_text = format!(
                        "[Image from {path} was not attached: too small for vision models]"
                    );
                }
                InlineAttachVerdict::Unreadable => {
                    prompt_text = format!(
                        "[Image from {path} was not attached: invalid or unreadable image data]"
                    );
                }
                InlineAttachVerdict::Attach => {
                    let url = format!(
                        "data:{};base64,{}",
                        image_content.mime_type, image_content.data
                    );
                    inline_images.push(ContentPart::Image {
                        url: std::sync::Arc::<str>::from(url),
                    });
                    prompt_text = format!("Read image file: {path}");
                }
            }
        }
        if !self.is_cursor_harness()
            && let ToolsToolOutput::ReadFile(ReadFileOutput::PdfPageImages(ref pdf)) = result.output
        {
            for page in &pdf.pages {
                let url = format!("data:{};base64,{}", page.mime_type, page.data);
                inline_images.push(ContentPart::Image {
                    url: std::sync::Arc::<str>::from(url),
                });
            }
            let path = tool_parsed_args
                .get("target_file")
                .or_else(|| tool_parsed_args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            prompt_text = format!(
                "Read PDF file: {path} ({} pages rendered, {} total)",
                pdf.pages.len(),
                pdf.total_pages,
            );
        }
        let tool_chat = if inline_images.is_empty() {
            ConversationItem::tool_result(call_id.to_string(), prompt_text)
        } else {
            ConversationItem::tool_result_with_images(
                call_id.to_string(),
                prompt_text,
                inline_images,
            )
        };
        self.chat_state_handle.push_tool_result(tool_chat);
        let mut deferred_followups = Vec::new();
        if !extracted_images.is_empty() {
            let count = extracted_images.len();
            tracing::info!(
                session_id = % self.session_info.id, tool = requested_tool_name, count,
                "base64 images extracted from tool result",
            );
            let acp_images: Vec<agent_client_protocol::ImageContent> = extracted_images
                .into_iter()
                .map(|img| agent_client_protocol::ImageContent::new(img.data, img.mime_type))
                .collect();
            let is_cursor_for_tool_result = self.is_cursor_harness();
            let mut norm_result = crate::session::image_normalize::normalize_images(
                acp_images,
                is_cursor_for_tool_result,
            )
            .await;
            if !norm_result.re_encode_fallbacks.is_empty() {
                tracing::warn!(
                    session_id = % self.session_info.id, notes = % norm_result
                    .re_encode_fallbacks.join(" "),
                    "Extracted tool image kept original after re-encode failure",
                );
            }
            if let Some((notice, notes)) = crate::session::image_normalize::dropped_to_envelope(
                std::mem::take(&mut norm_result.dropped),
                is_cursor_for_tool_result,
            ) {
                deferred_followups.push(ConversationItem::user(notice));
                self.send_xai_notification(XaiSessionUpdate::ImageDropped { notes })
                    .await;
            }
            for norm in norm_result.images {
                let url = format!("data:{};base64,{}", norm.mime_type, norm.data);
                let mut image_msg =
                    ConversationItem::user("[Image extracted from tool result above]");
                image_msg.add_image(url);
                deferred_followups.push(image_msg);
            }
        }
        Ok(deferred_followups)
    }
    /// Handle a hard tool execution error (dispatch/validation failure).
    ///
    /// Emits the failed tool_result to the client and records failure signals.
    /// Tool failures are not fed to the doom-loop detector (error-count streaks
    /// were removed), so this never warns/terminates and returns no deferred
    /// follow-ups today.
    pub(super) async fn handle_tool_error(
        &self,
        tool_call_id: &acp::ToolCallId,
        call_id: &str,
        requested_tool_name: &str,
        effective_tool_name: Option<&str>,
        err: &anyhow::Error,
        model_id: &str,
    ) -> Vec<ConversationItem> {
        tracing::error!(
            session_id = % self.session_info.id.0, tool_name = requested_tool_name,
            effective_tool_name = effective_tool_name, model_id = model_id, error_kind =
            "execution_failure", error_message = % err, "tool_error: execution_failure"
        );
        self.signals_handle()
            .record_tool_failure(requested_tool_name);
        let rewriter = self.path_rewriter();
        let err_str = match rewriter.as_ref() {
            Some(rw) => rw.rewrite(&err.to_string()),
            None => err.to_string(),
        };
        let message = match effective_tool_name {
            Some(effective) if effective != requested_tool_name => {
                format!("Tool `{effective}` failed via `{requested_tool_name}`: {err_str}")
            }
            _ => format!("Tool `{requested_tool_name}` failed: {err_str}"),
        };
        self.send_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                tool_call_id.clone(),
                acp::ToolCallUpdateFields::new()
                    .status(Some(acp::ToolCallStatus::Failed))
                    .content(Some(vec![acp::ToolCallContent::from(
                        acp::ContentBlock::Text(acp::TextContent::new(message.clone())),
                    )]))
                    .raw_output(Some(json!(
                        { "error" : "tool_execution_failed", "message" : err_str, }
                    ))),
            )),
            None,
        )
        .await;
        let tool_chat = ConversationItem::tool_result(call_id.to_string(), message);
        self.chat_state_handle.push_tool_result(tool_chat);
        vec![]
    }
    async fn send_thought_chunk(&self, text: String, chunk_index: u64) {
        self.send_update(
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))),
            Some(chunk_index),
        )
        .await;
    }
    /// Translate one [`xai_grok_sampler::SamplingEvent`] from the
    /// per-session sampler actor into the corresponding ACP / shell
    /// side-effects (notifications, signal recording, model-metadata
    /// refresh, etc.).
    ///
    /// Called from the drainer task spawned in `spawn_session_actor`,
    /// which loops `while let Some(event) = sampler_event_rx.recv().await`.
    /// Pure event mapping. Semantic recovery (compaction, friendly
    /// errors) lives in [`Self::handle_sampling_failure`] and runs in
    /// the turn loop, not here, because it depends on per-turn state
    /// and may need to call back into `sampler_handle.update_config`
    /// or resubmit.
    pub(crate) async fn handle_sampling_event(
        self: &Arc<Self>,
        event: xai_grok_sampler::SamplingEvent,
    ) {
        use xai_grok_sampler::{SamplingChannel, SamplingEvent};
        match event {
            SamplingEvent::StreamStarted { timestamp_ms, .. } => {
                {
                    let prompt_id = self
                        .current_prompt_id
                        .lock()
                        .expect("current_prompt_id mutex poisoned")
                        .clone();
                    let mut cap = self.streaming_turn_capture.lock();
                    if cap.prompt_id.as_deref() != prompt_id.as_deref() {
                        cap.begin_turn(prompt_id, self.current_turn_number.get());
                    }
                    cap.start_stream(timestamp_ms);
                }
                self.chat_state_handle.record_stream_start(timestamp_ms);
            }
            SamplingEvent::FirstToken { .. } => {
                self.emit_event(crate::session::events::Event::FirstToken);
            }
            SamplingEvent::ChannelToken {
                channel,
                text,
                chunk_index,
                ..
            } => match channel {
                SamplingChannel::Text => {
                    {
                        let mut cap = self.streaming_turn_capture.lock();
                        if cap.prompt_id.is_none() {
                            let prompt_id = self
                                .current_prompt_id
                                .lock()
                                .expect("current_prompt_id mutex poisoned")
                                .clone();
                            cap.begin_turn(prompt_id, self.current_turn_number.get());
                            cap.attempt_count += 1;
                        }
                        cap.append(false, &text);
                    }
                    self.emit_event(crate::session::events::Event::PhaseChanged {
                        phase: crate::session::events::Phase::StreamingText,
                    });
                    self.send_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new(text)),
                        )),
                        Some(chunk_index),
                    )
                    .await;
                }
                SamplingChannel::Reasoning => {
                    {
                        let mut cap = self.streaming_turn_capture.lock();
                        if cap.prompt_id.is_none() {
                            let prompt_id = self
                                .current_prompt_id
                                .lock()
                                .expect("current_prompt_id mutex poisoned")
                                .clone();
                            cap.begin_turn(prompt_id, self.current_turn_number.get());
                            cap.attempt_count += 1;
                        }
                        cap.append(true, &text);
                    }
                    self.emit_event(crate::session::events::Event::PhaseChanged {
                        phase: crate::session::events::Phase::StreamingReasoning,
                    });
                    self.send_thought_chunk(text, chunk_index).await;
                }
            },
            SamplingEvent::ToolCallDelta {
                tool_index,
                id,
                name,
                arguments_delta,
                ..
            } => {
                {
                    let mut cap = self.streaming_turn_capture.lock();
                    if cap.prompt_id.is_some() {
                        cap.phase = CapturePhase::ToolCall;
                    }
                }
                self.send_buffered_xai_update(XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: id,
                    tool_index,
                    name,
                    arguments_delta,
                })
                .await;
            }
            SamplingEvent::Completed {
                response, metrics, ..
            } => {
                if let Some(tx) = self.turn_stream_drained.lock().take() {
                    let _ = tx.send(());
                }
                if let Some(policy) = self.doom_loop_recovery {
                    let triggers = policy.confident_triggers(&response.doom_loop_signals);
                    if !triggers.is_empty() {
                        let attempts = {
                            let mut tally = self.doom_loop_turn_tally.lock();
                            if tally.attempts == 0 {
                                None
                            } else {
                                tally.accepted_after_budget = true;
                                tally.merge_triggers(&triggers);
                                Some(tally.attempts)
                            }
                        };
                        if let Some(attempts) = attempts {
                            self.streaming_turn_capture.lock().stamp_doom_loop(
                                crate::session::streaming_capture::DoomLoopSegmentStamp {
                                    doom_loop_triggers: triggers.clone(),
                                    attempt: attempts + 1,
                                    aborted_at_chunk: None,
                                    action: "accepted_after_budget".to_string(),
                                },
                            );
                            self.signals_handle()
                                .record_doom_loop_accepted_after_budget(triggers);
                        }
                    }
                }
                self.streaming_turn_capture.lock().clear_current_segment();
                self.record_api_request_time();
                self.signals_handle().record_inference_metrics(metrics);
            }
            SamplingEvent::ModelMetadata { metadata, .. } => {
                self.handle_model_metadata_update(metadata).await;
            }
            SamplingEvent::Retrying {
                request_id,
                attempt,
                max_retries,
                kind,
                reason,
                doom_loop_triggers,
                doom_loop_aborted_at_chunk,
            } => {
                if kind == xai_grok_sampler::SamplingErrorKind::DoomLoopDetected {
                    let triggers = doom_loop_triggers.unwrap_or_default();
                    let attempt_number = {
                        let mut tally = self.doom_loop_turn_tally.lock();
                        tally.attempts += 1;
                        tally.merge_triggers(&triggers);
                        tally.attempts
                    };
                    self.streaming_turn_capture.lock().stamp_doom_loop(
                        crate::session::streaming_capture::DoomLoopSegmentStamp {
                            doom_loop_triggers: triggers.clone(),
                            attempt: attempt_number,
                            aborted_at_chunk: doom_loop_aborted_at_chunk,
                            action: "resampled".to_string(),
                        },
                    );
                    self.signals_handle()
                        .record_doom_loop_recovery_attempt(triggers, doom_loop_aborted_at_chunk);
                }
                xai_grok_telemetry::unified_log::warn(
                    "shell.turn.inference_retry",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!(
                        { "sampler_request_id" : request_id.as_str(), "attempt" :
                        attempt, "max_retries" : max_retries, "kind" : kind.as_str(),
                        "reason" : crate ::util::truncate(& reason, 300), }
                    )),
                );
                self.send_xai_notification(XaiSessionUpdate::RetryState(
                    crate::extensions::notification::RetryState::Retrying {
                        attempt,
                        max_retries,
                        reason,
                    },
                ))
                .await;
            }
            SamplingEvent::Failed { request_id, error } => {
                xai_grok_telemetry::unified_log::error(
                    "shell.turn.inference_failed",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!(
                        { "sampler_request_id" : request_id.as_str(), "kind" : error
                        .kind.as_str(), "status_code" : error.status_code,
                        "is_retryable" : error.is_retryable, "message" : crate
                        ::util::truncate(& error.message, 300), }
                    )),
                );
                self.signals_handle()
                    .record_error_typed(error.kind.as_str());
                if let Some(ref ctx) = error.empty_response_context {
                    tracing::info!(
                        empty_response = true, empty_reason = ctx.reason.as_str(),
                        had_reasoning = ctx.had_reasoning, finish_reason = ctx
                        .finish_reason_str(), model = % ctx.model,
                        "sampler reported empty response (will retry if retryable)",
                    );
                }
            }
            SamplingEvent::BackendToolCallStarted { call_id, name, .. } => {
                self.signals_handle().record_tool_call(&name);
                let (title, kind, raw_input) = backend_tool_display(&name);
                self.send_update(
                    acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(
                            acp::ToolCallId::new(Arc::from(call_id.as_str())),
                            title,
                        )
                        .kind(kind)
                        .status(acp::ToolCallStatus::InProgress)
                        .content(vec![])
                        .locations(vec![])
                        .raw_input(Some(raw_input))
                        .meta(serde_json::json!({ "backend" : true }).as_object().cloned()),
                    ),
                    None,
                )
                .await;
            }
            SamplingEvent::BackendToolCallCompleted {
                call_id,
                name,
                result,
                ..
            } => {
                self.signals_handle().record_tool_success(&name);
                let (title, _kind, _raw_input) = backend_tool_display(&name);
                self.send_update(
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        acp::ToolCallId::new(Arc::from(call_id.as_str())),
                        acp::ToolCallUpdateFields::new()
                            .status(Some(acp::ToolCallStatus::Completed))
                            .title(Some(title))
                            .raw_output(result),
                    )),
                    None,
                )
                .await;
            }
        }
    }
    /// Model-facing rejection for a non-plan-file edit while plan mode is
    /// active. Rendered via the session's `TemplateRenderer` so
    /// `${{ plan_path }}` resolves; falls back if rendering fails.
    pub(super) async fn plan_mode_edit_rejected_message(&self) -> String {
        let plan_path = self.plan_mode.lock().plan_file_path().to_path_buf();
        self.render_plan_template(
            crate::session::plan_mode::plan_mode_edit_rejected_template(),
            &plan_path,
            false,
        )
        .await
        .unwrap_or_else(|| {
            format!(
                "Rejected: file edits are not allowed in plan mode - the only editable \
                 file is the plan file ({}).",
                plan_path.display()
            )
        })
    }
    pub(super) async fn handle_tool_not_executed(
        &self,
        model_call_id: &str,
        tool_call_id: &acp::ToolCallId,
        reason: String,
    ) -> Result<(), acp::Error> {
        let tool_update = acp::ToolCallUpdate::new(
            tool_call_id.clone(),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::Failed))
                .content(Some(vec![acp::ToolCallContent::from(
                    acp::ContentBlock::Text(acp::TextContent::new(reason.clone())),
                )])),
        );
        self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_update), None)
            .await;
        let tool_chat = ConversationItem::tool_result(model_call_id.to_owned(), reason);
        self.chat_state_handle.push_tool_result(tool_chat);
        Ok(())
    }
}
/// Execute tool-call display parts. The title peels a redundant leading
/// `cd <cwd>` for chrome only; `raw_input` is serialized separately and stays full.
fn execute_tool_call_parts(
    command: &str,
    description: Option<&str>,
    cwd: &std::path::Path,
) -> (
    String,
    acp::ToolKind,
    Vec<acp::ToolCallLocation>,
    Vec<acp::ToolCallContent>,
) {
    let display = xai_grok_tools::util::strip_redundant_session_cd(command, cwd);
    (
        format!("Execute `{display}`"),
        acp::ToolKind::Execute,
        Vec::new(),
        vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
            acp::TextContent::new(description.unwrap_or_default().to_string()),
        ))],
    )
}
#[cfg(test)]
mod execute_tool_call_parts_tests {
    use super::execute_tool_call_parts;
    use std::path::Path;
    #[test]
    fn peels_redundant_session_cd_from_title() {
        let (title, ..) =
            execute_tool_call_parts("cd /proj && echo hi", Some("desc"), Path::new("/proj"));
        assert_eq!(title, "Execute `echo hi`");
    }
    #[test]
    fn keeps_command_when_cd_not_redundant() {
        let (title, ..) = execute_tool_call_parts("cd /other && ls", None, Path::new("/proj"));
        assert_eq!(title, "Execute `cd /other && ls`");
    }
}
#[cfg(test)]
mod exit_plan_intercept_tests {
    use super::{PlanFileRead, classify_plan_file_read, should_intercept_exit_plan_approval};
    #[test]
    fn exit_plan_mode_empty_plan_still_intercepts() {
        assert!(should_intercept_exit_plan_approval(
            true,
            false,
            false,
            &PlanFileRead::Absent,
        ));
    }
    #[test]
    fn exit_plan_mode_nonempty_plan_intercepts() {
        assert!(should_intercept_exit_plan_approval(
            true,
            false,
            false,
            &PlanFileRead::Present("plan body".into()),
        ));
    }
    #[test]
    fn create_plan_empty_still_intercepts() {
        assert!(should_intercept_exit_plan_approval(
            false,
            false,
            true,
            &PlanFileRead::Absent,
        ));
    }
    #[test]
    fn create_plan_nonempty_intercepts() {
        assert!(should_intercept_exit_plan_approval(
            false,
            false,
            true,
            &PlanFileRead::Present("inline plan".into()),
        ));
    }
    #[test]
    fn unrelated_tool_does_not_intercept() {
        assert!(!should_intercept_exit_plan_approval(
            false,
            false,
            false,
            &PlanFileRead::Absent,
        ));
        assert!(!should_intercept_exit_plan_approval(
            false,
            false,
            false,
            &PlanFileRead::Present("ignored".into()),
        ));
    }
    #[test]
    fn classify_plan_file_read_present() {
        assert_eq!(
            classify_plan_file_read(Ok("# plan".into())),
            PlanFileRead::Present("# plan".into())
        );
    }
    #[test]
    fn classify_plan_file_read_absent_empty() {
        assert_eq!(
            classify_plan_file_read(Ok("  \n".into())),
            PlanFileRead::Absent
        );
    }
    #[test]
    fn classify_plan_file_read_absent_not_found() {
        assert_eq!(
            classify_plan_file_read(Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "missing",
            ))),
            PlanFileRead::Absent
        );
    }
    #[test]
    fn classify_plan_file_read_unreadable_permission_denied() {
        assert_eq!(
            classify_plan_file_read(Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            ))),
            PlanFileRead::Unreadable
        );
    }
}
#[cfg(test)]
mod plan_mode_edit_gate_tests {
    use super::{PlanEditGate, plan_mode_edit_gate};
    use crate::session::plan_mode::PlanModeTracker;
    use xai_grok_tools::types::ToolInput;
    use xai_grok_workspace::permission::AccessKind;
    /// Tracker with plan mode Active and plan file at
    /// `/tmp/gate-session/plan.md`.
    fn active_tracker() -> PlanModeTracker {
        let mut t = PlanModeTracker::new(std::path::PathBuf::from("/tmp/gate-session"));
        assert!(t.enter_pending());
        assert!(t.activate());
        t
    }
    fn gate(tracker: &PlanModeTracker, input: &ToolInput) -> PlanEditGate {
        plan_mode_edit_gate(tracker, input, &AccessKind::from(input))
    }
    fn search_replace(path: &str) -> ToolInput {
        use xai_grok_tools::implementations::grok_build::search_replace::SearchReplaceInput;
        ToolInput::SearchReplace(SearchReplaceInput {
            file_path: path.into(),
            old_string: "a".into(),
            new_string: "b".into(),
            replace_all: false,
        })
    }
    fn write(path: &str) -> ToolInput {
        use xai_grok_tools::implementations::opencode::write::WriteInput;
        ToolInput::Write(WriteInput {
            file_path: path.into(),
            content: "x".into(),
        })
    }
    /// Grok edit tools are plan-file-only while plan mode is active — the
    /// enforcement that makes plan mode read-only even under always-approve.
    #[test]
    fn grok_edits_outside_plan_file_rejected() {
        let t = active_tracker();
        assert_eq!(
            gate(&t, &search_replace("/tmp/src/main.rs")),
            PlanEditGate::RejectNonPlanFile
        );
        assert_eq!(
            gate(&t, &write("/tmp/README.md")),
            PlanEditGate::RejectNonPlanFile,
            "grok tools get no markdown exception — plan file only"
        );
    }
    /// The carve-out and the permission bypass share `should_auto_approve_edit`,
    /// so the plan file itself stays editable.
    #[test]
    fn plan_file_edit_allowed() {
        let t = active_tracker();
        assert_eq!(
            gate(&t, &search_replace("/tmp/gate-session/plan.md")),
            PlanEditGate::Allow
        );
        assert_eq!(
            gate(&t, &write("/tmp/gate-session/plan.md")),
            PlanEditGate::Allow
        );
    }
    /// `apply_patch` carries a placeholder access path, never the plan file:
    /// always rejected in plan mode (conservative).
    #[test]
    fn apply_patch_rejected_in_plan_mode() {
        use xai_grok_tools::implementations::codex::apply_patch::ApplyPatchInput;
        let t = active_tracker();
        assert_eq!(
            gate(
                &t,
                &ToolInput::ApplyPatch(ApplyPatchInput {
                    patch: String::new()
                })
            ),
            PlanEditGate::RejectNonPlanFile
        );
    }
    /// Non-edit tools are never gated — they flow to the normal permission
    /// path (where yolo may auto-approve them). Plan mode blocks
    /// edits, not bash/reads.
    #[test]
    fn non_edit_tools_not_gated() {
        use xai_grok_tools::implementations::BashToolInput;
        let t = active_tracker();
        assert_eq!(
            gate(
                &t,
                &ToolInput::Bash(BashToolInput {
                    command: "echo hi > /tmp/f".into(),
                    timeout: None,
                    description: "write via bash".into(),
                    is_background: false,
                })
            ),
            PlanEditGate::Allow,
            "bash is deliberately not gated — plan mode blocks edits only"
        );
    }
    /// Inactive (or merely Pending) plan mode gates nothing.
    #[test]
    fn inactive_or_pending_plan_mode_allows_everything() {
        let inactive = PlanModeTracker::new(std::path::PathBuf::from("/tmp/gate-session"));
        assert_eq!(
            gate(&inactive, &search_replace("/tmp/src/main.rs")),
            PlanEditGate::Allow
        );
        let mut pending = PlanModeTracker::new(std::path::PathBuf::from("/tmp/gate-session"));
        assert!(pending.enter_pending());
        assert_eq!(
            gate(&pending, &search_replace("/tmp/src/main.rs")),
            PlanEditGate::Allow,
            "Pending means the model has no plan-mode instructions yet — don't gate"
        );
    }
}
#[cfg(test)]
mod plan_approval_helper_tests {
    use super::{
        PlanApprovalOutcome, ResumeAction, ext_method_no_client, resume_action_for,
        revise_plan_message,
    };
    use xai_grok_tools::implementations::grok_build::exit_plan_mode::ExitPlanModeExtResponse;
    fn resp(outcome: &str) -> ExitPlanModeExtResponse {
        ExitPlanModeExtResponse {
            outcome: outcome.into(),
            feedback: None,
        }
    }
    #[test]
    fn outcome_from_response_maps_known_and_fails_closed() {
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("approved")),
            PlanApprovalOutcome::Approved
        );
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("abandoned")),
            PlanApprovalOutcome::Abandoned
        );
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("cancelled")),
            PlanApprovalOutcome::Cancelled
        );
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("approve")),
            PlanApprovalOutcome::Cancelled
        );
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("")),
            PlanApprovalOutcome::Cancelled
        );
    }
    #[test]
    fn ext_method_no_client_defaults_false_for_untagged_error() {
        assert!(!ext_method_no_client(&xai_acp_lib::acp_internal_error(
            "unrelated internal error"
        )));
    }
    #[test]
    fn revise_plan_message_includes_feedback_when_present() {
        assert!(revise_plan_message("").contains("Ask the user what changes"));
        assert!(revise_plan_message("   ").contains("Ask the user what changes"));
        let with = revise_plan_message("use async");
        assert!(with.contains("The user said:"));
        assert!(with.contains("use async"));
    }
    #[test]
    fn resume_action_maps_each_outcome() {
        assert_eq!(
            resume_action_for(PlanApprovalOutcome::Approved, None),
            ResumeAction::LeaveAndImplement
        );
        assert_eq!(
            resume_action_for(PlanApprovalOutcome::Abandoned, Some("ignored".into())),
            ResumeAction::LeaveOnly
        );
        match resume_action_for(PlanApprovalOutcome::Cancelled, Some("tweak it".into())) {
            ResumeAction::StayAndRevise(text) => assert!(text.contains("tweak it")),
            other => panic!("expected StayAndRevise, got {other:?}"),
        }
    }
}
#[cfg(test)]
mod wait_interrupt_tests {
    use super::{
        BlockingWaitGuard, interrupted_wait_tool_result, is_interruptible_wait_tool,
        wait_for_pending_interjection,
    };
    use xai_grok_tools::types::output::ToolOutput;
    use xai_tool_types::TaskOutputOutput;
    /// The interruptible-wait select arms: a pending interjection aborts an
    /// in-flight wait, and `biased` prefers an already-completed wait result
    /// over the abort. (Unit-level: the full dispatch loop has no test seam.)
    #[tokio::test(start_paused = true)]
    async fn pending_interjection_aborts_in_flight_wait() {
        use super::InterjectionBuffer;
        use xai_interjection_core::PendingInterjection;
        let buf: InterjectionBuffer<agent_client_protocol::ImageContent> =
            InterjectionBuffer::default();
        let out = tokio::select! {
            biased; r = async { "wait-result" } => r, _ = wait_for_pending_interjection(&
            buf) => "aborted",
        };
        assert_eq!(out, "wait-result");
        buf.push(PendingInterjection {
            text: "user message".into(),
            attachments: Vec::new(),
        });
        let out = tokio::select! {
            biased; r = async { tokio::time::sleep(std::time::Duration::from_secs(3600)).
            await; "wait-result" } => r, _ = wait_for_pending_interjection(& buf) =>
            "aborted",
        };
        assert_eq!(out, "aborted");
        let out = tokio::select! {
            biased; r = async { "wait-result" } => r, _ = wait_for_pending_interjection(&
            buf) => "aborted",
        };
        assert_eq!(out, "wait-result");
    }
    #[test]
    fn interruptible_wait_tool_only_when_timeout_positive() {
        assert!(is_interruptible_wait_tool(
            "get_command_or_subagent_output",
            &serde_json::json!({ "task_ids" : ["t"], "timeout_ms" : 120_000 })
        ));
        assert!(!is_interruptible_wait_tool(
            "get_task_output",
            &serde_json::json!({
            "task_ids" : ["t"], "timeout_ms" : 0 })
        ));
        assert!(!is_interruptible_wait_tool(
            "get_task_output",
            &serde_json::json!({
            "task_ids" : ["t"] })
        ));
        assert!(is_interruptible_wait_tool(
            "wait_commands_or_subagents",
            &serde_json::json!({ "task_ids" : ["t"] })
        ));
        assert!(!is_interruptible_wait_tool(
            "read_file",
            &serde_json::json!({ "target_file"
            : "/tmp/x" })
        ));
    }
    #[test]
    fn interrupted_wait_result_is_cancelled_not_error() {
        let r = interrupted_wait_tool_result(
            &serde_json::json!({ "task_ids" : ["bg-9"], "timeout_ms" : 60_000 }),
        );
        assert!(
            r.prompt_text
                .contains("Wait interrupted: the user sent a message.")
        );
        match &r.output {
            ToolOutput::TaskOutput(TaskOutputOutput::Result(res)) => {
                assert_eq!(res.task_id, "bg-9");
                assert_eq!(res.status, "cancelled");
            }
            other => panic!("expected TaskOutput Result, got {other:?}"),
        }
        assert!(!r.output.is_error());
    }
    /// `BlockingWaitGuard` counts nested waits; drop always decrements.
    #[test]
    fn blocking_wait_guard_counts_and_restores_on_drop() {
        use std::sync::Arc;
        let depth = Arc::new(crate::tools::tool_context::BlockingWaitState::new());
        {
            let _g1 = BlockingWaitGuard::enter(depth.clone());
            assert_eq!(depth.depth(), 1);
            {
                let _g2 = BlockingWaitGuard::enter(depth.clone());
                assert_eq!(depth.depth(), 2);
            }
            assert_eq!(depth.depth(), 1);
        }
        assert_eq!(depth.depth(), 0, "drop must restore");
    }
    /// An aborted wait future must not leak the depth count.
    #[tokio::test(start_paused = true)]
    async fn blocking_wait_guard_decrements_when_future_aborted() {
        use std::sync::Arc;
        let depth = Arc::new(crate::tools::tool_context::BlockingWaitState::new());
        let inner = depth.clone();
        let task = tokio::spawn(async move {
            let _g = BlockingWaitGuard::enter(inner);
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        tokio::task::yield_now().await;
        assert_eq!(depth.depth(), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(depth.depth(), 0, "abort must not leak");
    }
    #[test]
    fn blocking_wait_guard_reset_is_generation_scoped() {
        use std::sync::Arc;
        let depth = Arc::new(crate::tools::tool_context::BlockingWaitState::new());
        let old = BlockingWaitGuard::enter(depth.clone());
        assert_eq!(depth.depth(), 1);
        depth.reset();
        let new = BlockingWaitGuard::enter(depth.clone());
        assert_eq!(depth.depth(), 1);
        drop(old);
        assert_eq!(
            depth.depth(),
            1,
            "old-generation drop must not consume the new wait"
        );
        drop(new);
        assert_eq!(depth.depth(), 0);
    }
}
