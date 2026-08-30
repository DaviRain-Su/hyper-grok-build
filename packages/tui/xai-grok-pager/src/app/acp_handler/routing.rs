use super::*;

/// Result of looking up which view a notification's `session_id` targets.
///
/// The matched view's mutation must happen on the agent identified here, regardless of which view the user is currently looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionMatch {
    /// The session_id matches the root session of this agent.
    Root(AgentId),
    /// The session_id matches a subagent view child of this agent (i.e. an entry in `agent.subagent_views`).
    /// The child's key is the notification's `session_id.0.as_ref()`; the caller re-derives it to avoid an extra allocation.
    Child(AgentId),
}

impl SessionMatch {
    /// The owning agent's id: the matched agent for `Root`, the parent that owns the `subagent_views` entry for `Child`.
    /// Callers that do not care about root vs child should use this instead of duplicating the `match { Root(id) | Child(id) => id }` pattern.
    pub(super) fn agent_id(self) -> AgentId {
        match self {
            SessionMatch::Root(id) | SessionMatch::Child(id) => id,
        }
    }
}

/// Resolve the agent that owns a notification's `session_id` and whether the active view is affected.
///
/// Convenience wrapper around `find_session_match`, `is_matched_agent_active`, and `agents.get_mut()`, used by the bg-task notification handlers.
pub(super) fn resolve_notif_agent<'a>(
    app: &'a mut AppView,
    session_id: &acp::SessionId,
) -> Option<(SessionMatch, bool, &'a mut AgentView)> {
    let matched = find_session_match(app, session_id)?;
    let parent_id = matched.agent_id();
    let is_active = is_matched_agent_active(app, parent_id);
    let agent = app.agents.get_mut(&parent_id)?;
    Some((matched, is_active, agent))
}

/// Resolve the agent an MCP-lifecycle notification (`init_progress` / `mcp_initialized`) targets.
///
/// Routes by the payload's `sessionId`.
/// A background session's progress updates and completion signal land on *its* agent instead of whichever agent is foregrounded.
/// Otherwise a background agent's "Connecting MCPs (N/M)…" spinner is never cleared and sticks forever.
/// Falls back to the active agent when the payload omits a `sessionId`.
///
/// Returns the owning agent plus whether it is the currently displayed one (used to decide whether the notification warrants a redraw).
///
/// Only resolves to a `Root` agent: `mcp_init_progress` is a per-root-agent indicator with no per-subagent slot.
/// Notifications whose sessionId matches a subagent (`Child`) are dropped; otherwise a subagent's own MCP init would clobber its parent's spinner.
pub(super) fn mcp_target_agent<'a>(
    app: &'a mut AppView,
    session_id: Option<&str>,
) -> Option<(bool, &'a mut AgentView)> {
    match session_id {
        Some(sid) => {
            let sid = acp::SessionId::new(sid);
            let (matched, is_active, agent) = resolve_notif_agent(app, &sid)?;
            if matches!(matched, SessionMatch::Child(_)) {
                return None;
            }
            Some((is_active, agent))
        }
        None => {
            let ActiveView::Agent(id) = app.active_view else {
                return None;
            };
            let agent = app.agents.get_mut(&id)?;
            Some((true, agent))
        }
    }
}

/// Given a matched session and the owning agent, borrow the correct `(session, scrollback)` pair.
/// That is the child view's pair when the notification targets a subagent, the root agent's otherwise.
pub(super) fn resolve_target_view<'a>(
    agent: &'a mut AgentView,
    matched: SessionMatch,
    child_sid: &str,
) -> Option<(
    &'a mut AgentSession,
    &'a mut crate::scrollback::state::ScrollbackState,
)> {
    if matches!(matched, SessionMatch::Child(_)) {
        // A `TaskBackgrounded` / `TaskCompleted` block for a resumed child always follows the funneled tool_call that spawned the task
        // The child is therefore never still both prompt-only and NeedsReplay here, so it is precedence-exempt from the hydrate funnel
        let child_view = agent.subagent_views.get_mut(child_sid)?;
        Some((&mut child_view.session, &mut child_view.scrollback))
    } else {
        Some((&mut agent.session, &mut agent.scrollback))
    }
}

/// Locate the agent (or subagent view) a notification's `session_id` belongs to.
///
/// Search order:
/// 1. Exact root match: an agent whose `session.session_id` equals `session_id`.
/// 2. Subagent view: any agent whose `subagent_views` map contains `session_id` as a key.
/// 3. Race-window fallback: when no exact match exists AND the currently active agent has no `session_id` yet, route to it.
///    Notifications can race ahead of `TaskResult::SessionCreated`.
///    The only agent that could own such a pre-assignment notification is the one the user just created (necessarily active, `session_id == None`).
///
/// Returns `None` when the notification cannot be associated with any agent.
/// The caller should drop it (sending an empty Ok response if applicable).
///
/// All ACP-notification handlers must route through this function rather than gating on `app.active_view` directly.
/// See the `handle_scheduled_task_*` family for the legacy active-view pattern still pending migration.
pub(super) fn find_session_match(
    app: &AppView,
    session_id: &acp::SessionId,
) -> Option<SessionMatch> {
    // Single pass over `app.agents`, halving the two-pass iteration cost on the hot notification path
    // An exact root match returns immediately (root wins when both could match); the first child match seen is the fallback after the full scan
    //
    // Comparing `Option<&SessionId>` to `Some(&session_id)` borrows both sides, so no SessionId clone
    // The HashMap lookup uses the inner `&str` directly via the `Borrow<str>` impl on `String`, so no allocation either
    let child_key: &str = session_id.0.as_ref();
    let mut child_match: Option<AgentId> = None;
    for (id, agent) in &app.agents {
        if agent.session.session_id.as_ref() == Some(session_id) {
            return Some(SessionMatch::Root(*id));
        }
        if child_match.is_none() && agent.subagent_views.contains_key(child_key) {
            child_match = Some(*id);
        }
    }
    if let Some(id) = child_match {
        return Some(SessionMatch::Child(id));
    }
    // Pass 3: race-window fallback for notifications that arrive before the root session_id has been assigned
    // Only the active agent is eligible, and only when its `session_id` is still `None`
    // Otherwise we would misroute a stranger's notification to whichever agent happens to be foregrounded
    if let ActiveView::Agent(active_id) = app.active_view
        && let Some(agent) = app.agents.get(&active_id)
        && agent.session.session_id.is_none()
    {
        return Some(SessionMatch::Root(active_id));
    }
    None
}

/// Whether the matched agent is the one currently displayed.
pub(super) fn is_matched_agent_active(app: &AppView, matched_agent: AgentId) -> bool {
    matches!(app.active_view, ActiveView::Agent(id) if id == matched_agent)
}

/// Resolve the `AgentId` that should own an interactive modal (`ask_user_question` / `exit_plan_mode`) for `session_id`.
///
/// Routes by the request's session id via [`find_session_match`] (exactly like `session/update` notifications), not gated on `app.active_view`.
/// A modal raised by a **background** session thus lands on its own view even when the user is on the dashboard or a different session.
/// A child (subagent) match resolves to its parent agent, which owns the overlay.
///
/// Returns `None` when no local view exists for that session.
/// The caller must then leave the reverse-request unanswered (drop, do NOT error) and rely on the leader's replay-on-attach.
pub(super) fn interaction_target_agent(app: &AppView, session_id: &str) -> Option<AgentId> {
    let sid = acp::SessionId::new(session_id.to_owned());
    match find_session_match(app, &sid) {
        Some(SessionMatch::Root(id) | SessionMatch::Child(id)) => Some(id),
        None => None,
    }
}
