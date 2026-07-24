//! Map pipeline [`LiveEvent`]s onto pager state (visualizer + delegation
//! submission).

use super::state::{DraftSnapshot, LiveState};
use super::{LiveDelegation, LiveEvent};
use crate::app::actions::Action;
use crate::app::app_view::AppView;

/// Apply a Live event to app state. Returns whether the frame should redraw
/// and any actions the event loop should dispatch (e.g. a delegation prompt
/// submission).
pub fn handle_live_event(app: &mut AppView, event: LiveEvent) -> (bool, Vec<Action>) {
    let mut needs_draw = false;
    let mut actions = Vec::new();

    match event {
        LiveEvent::Phase(_) | LiveEvent::Levels(_) | LiveEvent::Transcript { .. } => {
            if app.live_runtime.visualizer.apply_event(&event) {
                needs_draw = true;
            }
        }
        LiveEvent::Delegation(delegation) => {
            needs_draw = handle_delegation(app, &delegation, &mut actions);
        }
        LiveEvent::Error { .. } => {
            if app.live_runtime.visualizer.apply_event(&event) {
                needs_draw = true;
            }
        }
        LiveEvent::Closed => {
            // The event loop handles channel cleanup; just mark the state.
            needs_draw = true;
        }
    }

    (needs_draw, actions)
}

/// Handle a `LiveEvent::Delegation`: submit literal plain text through the
/// existing prompt/effect pipeline to the bound current AgentSession,
/// preserve the draft, and register `(generation, delegation_id) ->
/// (AgentId, SessionId, prompt_id, lifecycle)`.
fn handle_delegation(
    app: &mut AppView,
    delegation: &LiveDelegation,
    actions: &mut Vec<Action>,
) -> bool {
    // Only handle delegations when Live is active and bound.
    let (agent_id, session_id, generation) = match &app.live_runtime.state {
        LiveState::Active {
            agent_id,
            session_id,
            generation,
            ..
        } => (*agent_id, session_id.clone(), *generation),
        _ => return false,
    };

    // The bound session must still be the active view.
    if !app.is_live_bound_to_active(agent_id, &session_id) {
        return false;
    }

    let text = delegation.text.trim();
    if text.is_empty() {
        return false;
    }

    // Preserve the draft: snapshot the current prompt text + cursor before
    // submitting, then restore after (the SendPrompt path clears the textarea,
    // so we restore the draft immediately).
    let draft = snapshot_draft(app, agent_id);

    // Submit the delegation text as a prompt to the bound agent session.
    // This goes through the normal SendPrompt action → dispatch → effect
    // pipeline, so the prompt_id is assigned by the prompt dispatcher.
    actions.push(Action::LiveDelegationSubmit {
        agent_id,
        text: text.to_string(),
        delegation_id: delegation.id.clone(),
        generation,
        draft,
    });

    true
}

/// Snapshot the current prompt draft (text + cursor) for the given agent.
fn snapshot_draft(app: &AppView, agent_id: crate::app::agent::AgentId) -> DraftSnapshot {
    let Some(agent) = app.agents.get(&agent_id) else {
        return DraftSnapshot::default();
    };
    DraftSnapshot {
        text: agent.prompt.text().to_string(),
        cursor: agent.prompt.cursor(),
    }
}

/// Restore the draft for the given agent (called after a delegation prompt
/// is submitted, since SendPrompt clears the textarea).
pub fn restore_draft(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    draft: &DraftSnapshot,
) {
    if draft.text.is_empty() {
        return;
    }
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.prompt.set_text(&draft.text);
        agent.prompt.set_cursor(draft.cursor);
    }
}
