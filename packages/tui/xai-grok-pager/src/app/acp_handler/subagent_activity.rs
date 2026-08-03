use super::*;

/// Update the activity label on a subagent's collapsed scrollback block.
///
/// Skips the write (and cache invalidation) when the label hasn't changed,
/// so the per-delta common case ("Responding" stays "Responding") allocates
/// nothing.
pub(super) fn sync_activity_label(
    scrollback: &mut crate::scrollback::state::ScrollbackState,
    entry_id: Option<crate::scrollback::entry::EntryId>,
    activity_label: Option<&str>,
) {
    if let Some(eid) = entry_id
        && let Some(entry) = scrollback.get_by_id_mut(eid)
        && let RenderBlock::Subagent(ref mut sb) = entry.block
        && sb.activity_label.as_deref() != activity_label
    {
        sb.activity_label = activity_label.map(str::to_owned);
        entry.invalidate_cache();
    }
}

/// Fan a subagent's computed activity label out to both surfaces that show
/// it — the collapsed scrollback block and the [`SubagentInfo`] backing the
/// tasks pane / dashboard rows — so the two can't drift.
pub(super) fn sync_subagent_activity(
    parent: &mut AgentView,
    child_key: &str,
    activity_label: Option<String>,
) {
    let Some(info) = parent.subagent_sessions.get_mut(child_key) else {
        return;
    };
    sync_activity_label(
        &mut parent.scrollback,
        info.scrollback_entry_id,
        activity_label.as_deref(),
    );
    info.activity_label = activity_label;
}

/// Resolve a subagent child view's live activity into the display label the
/// fan-out stamps ("Waiting" while the child is busy between activities).
pub(super) fn subagent_activity_label(child_view: &AgentView) -> Option<String> {
    match child_view.resolve_turn_activity() {
        Some(a) => Some(crate::app::subagent::format_activity_label(&a)),
        None if child_view.session.state.is_busy() => Some("Waiting".to_string()),
        None => None,
    }
}

/// Synthesize a finish for a stuck row when a kill found nothing live to stop
/// (else `pending_kill` times out → "running"). `status` is the real terminal
/// status for an already-finished orphan, else `"cancelled"`.
pub(crate) fn finalize_killed_subagent(
    app: &mut AppView,
    session_id: &acp::SessionId,
    subagent_id: &str,
    status: &str,
) -> bool {
    let Some(SessionMatch::Root(agent_id)) = find_session_match(app, session_id) else {
        return false;
    };
    let Some(agent) = app.agents.get(&agent_id) else {
        return false;
    };
    // Idempotency: skip if already finished.
    let Some(child_session_id) = agent
        .subagent_sessions
        .values()
        .find(|i| i.subagent_id.as_ref() == subagent_id && !i.finished)
        .map(|i| i.child_session_id.to_string())
    else {
        return false;
    };

    let payload = SessionNotification {
        session_id: session_id.clone(),
        update: XaiSessionUpdate::SubagentFinished {
            subagent_id: subagent_id.to_string(),
            child_session_id,
            // An already-finished orphan may be "failed", but the cancel response
            // carries no failure reason (lost across the resume window), so
            // `error` stays None.
            status: status.to_string(),
            error: None,
            termination_reason: None,
            usage: None,
            tool_calls: 0,
            turns: 0,
            // Real run time is unknown for an already-gone orphan (the row's
            // started_at is stamped at resume, not the real spawn), so emit 0.
            duration_ms: 0,
            tokens_used: 0,
            output: None,
            will_wake: false,
        },
        meta: None,
    };
    let Ok(params) = serde_json::value::to_raw_value(&payload) else {
        return false;
    };
    let notif = acp::ExtNotification::new("x.ai/session/update", params.into());
    handle_ext_notification(&notif, app)
}
