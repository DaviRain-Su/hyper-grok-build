//! Map pipeline [`LiveEvent`]s onto pager state (visualizer + delegation
//! submission + scrollback transcript flush).

use super::state::LiveState;
use super::{LiveEvent, LiveRole};
use crate::app::actions::Action;
use crate::app::agent::AgentId;
use crate::app::app_view::AppView;

/// Apply a Live event to app state. Returns whether the frame should redraw
/// and any actions the event loop should dispatch (e.g. a delegation prompt
/// submission).
pub fn handle_live_event(app: &mut AppView, event: LiveEvent) -> (bool, Vec<Action>) {
    let mut needs_draw = false;
    let mut actions = Vec::new();

    match &event {
        LiveEvent::Phase(_) | LiveEvent::Levels(_) | LiveEvent::Transcript { .. } => {
            // Update visualizer state (streams all deltas). Do NOT flush
            // individual output deltas to scrollback — only the final Turn
            // event writes the complete transcript to scrollback exactly once.
            if app.live_runtime.visualizer.apply_event(&event) {
                needs_draw = true;
            }
        }
        LiveEvent::Turn { role, transcript } => {
            if app.live_runtime.visualizer.apply_event(&event) {
                needs_draw = true;
            }
            // Flush the FINALIZED assistant turn transcript to scrollback
            // exactly once per turn. The visualizer's `assistant_flushed`
            // flag is reset by `apply_event` on Turn { Assistant } so this
            // is the single flush site. Do not flush partial first-token
            // content from individual Transcript deltas.
            if *role == LiveRole::Assistant {
                flush_assistant_transcript_to_scrollback(app, transcript);
                app.live_runtime.visualizer.reset_turn();
                needs_draw = true;
            }
        }
        LiveEvent::Delegation { id, content } => {
            needs_draw = handle_delegation(app, id, content, &mut actions);
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
    delegation_id: &str,
    content: &[String],
    actions: &mut Vec<Action>,
) -> bool {
    // Only handle delegations when Live is active and bound.
    let (agent_id, session_id, generation) = match &app.live_runtime.state {
        LiveState::Active {
            agent_id,
            session_id,
            generation,
            draft: _,
        } => (*agent_id, session_id.clone(), *generation),
        _ => return false,
    };

    // The bound session must still be the active view and the session must
    // still match. This is the explicit bound-session path — never generic
    // whatever-is-active routing.
    if !app.is_live_bound_to_active(agent_id, &session_id) {
        return false;
    }

    // Join the delegation content into a single text block.
    let text = content.join("\n").trim().to_string();
    if text.is_empty() {
        return false;
    }

    // Preserve the draft: snapshot the current prompt text + cursor before
    // submitting. The draft was already captured at Live start, but we
    // re-snapshot here in case the user typed during the session.
    let current_draft = snapshot_draft(app, agent_id);

    // Register the delegation in the broker so ACP ingress can correlate it.
    app.live_runtime
        .broker
        .register_delegation(delegation_id.to_string());

    // Submit the delegation text as a prompt to the bound agent session.
    // This goes through the normal SendPrompt action → dispatch → effect
    // pipeline, so the prompt_id is assigned by the prompt dispatcher.
    actions.push(Action::LiveDelegationSubmit {
        agent_id,
        text,
        delegation_id: delegation_id.to_string(),
        generation,
        draft: current_draft,
    });

    true
}

/// Snapshot the current prompt draft (text + cursor) for the given agent.
fn snapshot_draft(app: &AppView, agent_id: AgentId) -> super::state::DraftSnapshot {
    let Some(agent) = app.agents.get(&agent_id) else {
        return super::state::DraftSnapshot::default();
    };
    super::state::DraftSnapshot {
        text: agent.prompt.text().to_string(),
        cursor: agent.prompt.cursor(),
    }
}

/// Restore the draft for the given agent (called after a delegation prompt
/// is submitted, since SendPrompt clears the textarea).
pub fn restore_draft(app: &mut AppView, agent_id: AgentId, draft: &super::state::DraftSnapshot) {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.prompt.set_text(&draft.text);
        agent.prompt.set_cursor(draft.cursor);
    }
}

/// Flush the assistant Live transcript to the bound agent's scrollback with
/// a distinct Live label. Streams exactly once per turn (guarded by
/// `assistant_flushed`).
fn flush_assistant_transcript_to_scrollback(app: &mut AppView, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if app.live_runtime.visualizer.assistant_flushed {
        return;
    }
    let Some(agent_id) = app.live_runtime.state.agent_id() else {
        return;
    };
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return;
    };
    // Push a labeled Live assistant transcript block into scrollback.
    let label = rust_i18n::t!("live.assistant_label");
    let block = crate::scrollback::block::RenderBlock::agent_message(format!("{label}: {text}"));
    agent.scrollback.push_block(block);
    app.live_runtime.visualizer.assistant_flushed = true;
}
