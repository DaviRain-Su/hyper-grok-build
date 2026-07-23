//! Permission request selection, follow-up, cancellation, and queue draining.

use super::modes::set_yolo_mode;
use crate::app::actions::Effect;
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView};
use agent_client_protocol as acp;

// ---------------------------------------------------------------------------
// Permission dispatch
// ---------------------------------------------------------------------------

/// Handle permission option selection (AllowOnce, AllowAlways, RejectAlways).
///
/// Pops the front request, sends the response, and handles queue transitions
/// (prompt restore on empty, prompt clear on next-front).
///
/// Special case for [`xai_grok_workspace::permission::ENABLE_ALWAYS_APPROVE_OPTION_ID`]:
/// when the user picks the prepended "Yes, and don't ask again for anything"
/// option, this dispatcher (a) sends the standard `Selected` response so the
/// in-flight request is allowed once (the shell's `map_selected_outcome`
/// resolves the id to `PromptOutcome::AllowOnce`), then (b) reuses the
/// existing `set_yolo_mode(true)` flow to flip the local YOLO state, drain
/// any remaining queued permissions, persist `[ui] permission_mode =
/// "always-approve"` to `~/.grok/config.toml`, and fire the
/// `x.ai/yolo_mode_changed` ACP notification. See the option-id constant
/// doc-comment for the full client/shell split. Under a managed-policy
/// pin step (b) is refused with a toast — the request is still allowed once.
pub(super) fn dispatch_permission_select(
    app: &mut AppView,
    option_id: acp::PermissionOptionId,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(perm) = agent.permission_queue.pop_front() else {
        return vec![];
    };

    // Detect the "enable always-approve mode" id BEFORE moving option_id
    // into the response. Cheap str compare on the `Arc<str>` interior.
    let enable_always_approve =
        option_id.0.as_ref() == xai_grok_workspace::permission::ENABLE_ALWAYS_APPROVE_OPTION_ID;

    // Remember the user's choice (by option kind) so the next prompt's cursor
    // sticks to it. Allow-flavored choices only — a rejection must not steer a
    // later prompt's cursor onto a reject row. Also skip the two options that
    // aren't per-prompt choices:
    //  - the global always-approve (YOLO) option flips global auto-approve, so
    //    there will be no subsequent prompt to land on;
    //  - "allow all edits during this session" is edit-scoped (kind
    //    `AllowAlways`) — letting it stick would steer an unrelated later
    //    prompt onto its "always allow this command" row, escalating scope.
    let steers_next_cursor = !enable_always_approve
        && option_id.0.as_ref() != xai_grok_workspace::permission::ALLOW_EDITS_SESSION_OPTION_ID;
    if steers_next_cursor
        && let Some(kind) = perm
            .options
            .iter()
            .find(|o| o.option_id == option_id)
            .map(|o| o.kind)
        && matches!(
            kind,
            acp::PermissionOptionKind::AllowOnce | acp::PermissionOptionKind::AllowAlways
        )
    {
        crate::appearance::permission_cursor::set_last_used_permission(
            crate::appearance::permission_cursor::DefaultSelectedPermission::from_kind(&kind),
        );
    }

    // Build response meta. MCP and bash flows are mutually exclusive at
    // the per-request level; check MCP first because it owns the
    // `allow-always-mcp` option id and the bash branch is the existing
    // fallback.
    let meta = if let Some(scope) = perm
        .mcp_scope
        .as_ref()
        .filter(|_| option_id.0.as_ref() == "allow-always-mcp")
    {
        let selection = match scope.selected {
            crate::views::permission_view::McpScope::Tool => {
                xai_grok_workspace::permission::McpScopeSelection::Tool {
                    tool_name: scope.tool_name.clone(),
                }
            }
            crate::views::permission_view::McpScope::Server => match &scope.server_prefix {
                Some(prefix) => xai_grok_workspace::permission::McpScopeSelection::Server {
                    server: prefix.clone(),
                },
                // Defensive: render path should disable Server when no prefix.
                None => xai_grok_workspace::permission::McpScopeSelection::Tool {
                    tool_name: scope.tool_name.clone(),
                },
            },
        };
        serde_json::to_value(selection)
            .ok()
            .and_then(|v| v.as_object().cloned())
    } else if let Some(ref h) = perm.bash_highlights
        && perm.bash_selection_count > 0
    {
        let parts: Vec<String> = h.highlighted_words[..perm.bash_selection_count].to_vec();
        serde_json::to_value(xai_grok_workspace::permission::BashCommandSelectedTerms {
            command_parts: parts,
        })
        .ok()
        .and_then(|v| v.as_object().cloned())
    } else {
        None
    };

    perm.request
        .response_tx
        .send(Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id)),
        )
        .meta(meta)))
        .ok();

    // Queue transition: restore prompt if queue is now empty, clear if next-front.
    resolve_permission_queue_transition(agent);

    // "Enable always-approve" side effect: flip YOLO + persist + notify.
    // Reuses the existing `set_yolo_mode` pipeline so telemetry, queue
    // drain, toast, modal refresh, config persistence, and ACP
    // notification all flow through one well-tested code path.
    //
    // Idempotency: if YOLO is already on, the pager auto-approves in
    // `handle_permission_request` before the panel is shown, so the
    // user couldn't have selected this option. The `is_yolo()` guard
    // is defensive — a redundant call would re-emit the toast and a
    // duplicate `PersistPermissionMode` effect, but is otherwise safe.
    if enable_always_approve {
        let already_on = app
            .agents
            .get(&id)
            .map(|a| a.session.is_yolo())
            .unwrap_or(false);
        if !already_on {
            return set_yolo_mode(app, true);
        }
    }

    vec![]
}

/// Handle permission followup message (RejectOnce with user-typed text).
pub(super) fn dispatch_permission_followup(app: &mut AppView, text: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(perm) = agent.permission_queue.pop_front() else {
        return vec![];
    };

    // Find the RejectOnce option.
    let option_id = perm
        .options
        .iter()
        .find(|o| o.kind == acp::PermissionOptionKind::RejectOnce)
        .map(|o| o.option_id.clone());

    let Some(option_id) = option_id else {
        // No RejectOnce option — cancel instead.
        perm.request
            .response_tx
            .send(Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            )))
            .ok();
        resolve_permission_queue_transition(agent);
        return vec![];
    };

    // Include followup message in meta.
    let meta = if !text.trim().is_empty() {
        serde_json::json!({
            "followup_message": text,
        })
        .as_object()
        .cloned()
    } else {
        None
    };

    perm.request
        .response_tx
        .send(Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id)),
        )
        .meta(meta)))
        .ok();

    resolve_permission_queue_transition(agent);
    vec![]
}

/// Handle permission cancel (Ctrl-C / Esc — cancels front request only).
pub(super) fn dispatch_permission_cancel(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(perm) = agent.permission_queue.pop_front() else {
        return vec![];
    };

    perm.request
        .response_tx
        .send(Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Cancelled,
        )))
        .ok();

    resolve_permission_queue_transition(agent);
    vec![]
}

/// Drain all queued permission requests, sending `Cancelled` to each.
///
/// Called on turn-end and turn-cancel. After draining, restores stashed
/// prompt/pane. Distinct from `dispatch_permission_cancel` (front only).
pub(super) fn drain_permission_queue(agent: &mut AgentView) {
    agent.last_permission_click = None;
    if agent.permission_queue.is_empty() {
        return;
    }
    for perm in agent.permission_queue.drain(..) {
        perm.request
            .response_tx
            .send(Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            )))
            .ok();
    }
    restore_permission_stashes(agent);
}

/// Handle queue transition after resolving (select/followup/cancel) the front
/// permission request.
///
/// - Queue now empty → restore stashed prompt/pane.
/// - Queue still has items → clear prompt text and reset next front to Options.
pub(crate) fn resolve_permission_queue_transition(agent: &mut AgentView) {
    agent.last_permission_click = None;
    if agent.permission_queue.is_empty() {
        restore_permission_stashes(agent);
    } else {
        // Clear any followup text from the just-resolved permission so it
        // doesn't leak into the next permission's UI.
        agent.prompt.set_text("");
        // Reset next front's focus to Options.
        if let Some(next) = agent.permission_queue.front_mut() {
            next.focus = crate::views::permission_view::PermissionFocus::Options;
        }
    }
}

/// Restore composer + pane stashes when the permission queue empties.
pub(super) fn restore_permission_stashes(agent: &mut AgentView) {
    if let Some(stashed) = agent.permission_stashed_prompt.take() {
        agent.prompt.restore(stashed);
    }
    if let Some(pane) = agent.permission_stashed_pane.take() {
        agent.set_active_pane(pane, true);
    }
}
