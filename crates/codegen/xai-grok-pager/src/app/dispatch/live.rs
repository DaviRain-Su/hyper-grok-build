//! Codex Live (`/live`) dispatch: toggle, mute, stop, and delegation submit.
//!
//! Mutually exclusive with `/voice` dictation — starting Live stops Voice and
//! vice versa. Live is bound to `(generation, AgentId, SessionId)` and stops
//! idempotently on session/view switch, close/exit/quit, ACP disconnect, and
//! teardown.

#![cfg(feature = "codex-live")]

use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::app_view::{ActiveView, AppView};
use crate::live::state::{DraftSnapshot, LiveState};

/// Stop `/voice` dictation if it's active (mutual exclusion — starting Live
/// stops Voice).
fn stop_voice_for_live(app: &mut AppView) {
    if app.voice_listening() || app.voice_state.pending_cold_start() {
        app.voice_reset();
    }
    app.voice_ui_active = false;
}

/// Start the Codex Live session. The start primitive reached by the toggle
/// (`/live`). Binds to the active agent + session, preserving the composer
/// draft. If the active AgentView has no bound ACP session, uses the existing
/// `CreateSession` effect then continues pending Live start.
pub(super) fn dispatch_live_toggle(app: &mut AppView) -> Vec<Effect> {
    // Gate: silent no-op when the Live gate is off.
    if !app.live_mode_enabled {
        return vec![];
    }

    // If Live is active or pending, stop it (toggle off).
    if app.live_in_flight() {
        dispatch_live_stop(app);
        return vec![];
    }

    // Mutual exclusion: stop `/voice` if active.
    stop_voice_for_live(app);

    // Resolve the active agent + session.
    let agent_id = match app.active_view {
        ActiveView::Agent(id) => id,
        _ => return vec![], // Not on an agent screen — silent no-op.
    };

    // Snapshot the draft before starting (preserved across the session).
    let draft = snapshot_draft(app, agent_id);

    // Check if the agent has a bound session.
    let session_id = app
        .agents
        .get(&agent_id)
        .and_then(|a| a.session.session_id.as_ref())
        .map(|s| s.0.as_ref().to_string());

    if let Some(session_id) = session_id {
        // Session is bound — start Live now.
        start_live_session(app, agent_id, session_id, draft);
    } else {
        // No bound session — defer to PendingUnbound. The event loop will
        // create a session via CreateSession then continue the pending start.
        app.live_runtime.state = LiveState::PendingUnbound { agent_id };
    }

    vec![]
}

/// Start the Live session (pipeline spawn + state transition).
fn start_live_session(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: String,
    draft: DraftSnapshot,
) {
    let generation = app.live_runtime.next_generation();
    app.live_runtime.state = LiveState::ColdStart {
        agent_id,
        session_id: session_id.clone(),
        generation,
        draft: draft.clone(),
    };
    // The event loop picks up the ColdStart and spawns the pipeline.
}

/// Stop the Codex Live session unconditionally. Idempotent.
pub(super) fn dispatch_live_stop(app: &mut AppView) -> Vec<Effect> {
    if !app.live_in_flight() {
        return vec![];
    }

    // Restore the draft before teardown (so the editor/cursor comes back).
    let draft_to_restore = app.live_runtime.state.draft().cloned();
    let agent_to_restore = app.live_runtime.state.agent_id();
    if let (Some(draft), Some(agent_id)) = (draft_to_restore, agent_to_restore) {
        crate::live::handle::restore_draft(app, agent_id, &draft);
    }

    app.live_reset();
    vec![]
}

/// Set the Live mute state explicitly.
pub(super) fn dispatch_live_set_muted(app: &mut AppView, muted: bool) -> Vec<Effect> {
    if !app.live_active() {
        return vec![];
    }
    app.live_set_muted(muted);
    vec![]
}

/// Handle a `LiveEvent::Delegation` — submit literal plain text through the
/// prompt pipeline to the bound AgentSession, preserve the draft, obtain the
/// prompt_id, and register `(generation, delegation_id) ->
/// (AgentId, SessionId, prompt_id, lifecycle)`.
pub(super) fn dispatch_live_delegation_submit(
    app: &mut AppView,
    agent_id: AgentId,
    text: String,
    delegation_id: String,
    generation: u64,
    draft: DraftSnapshot,
) -> Vec<Effect> {
    // Restore the draft first (the SendPrompt path clears the textarea, but
    // we want the draft preserved throughout).
    crate::live::handle::restore_draft(app, agent_id, &draft);

    // Submit the text as a prompt to the bound agent session. This goes
    // through the normal SendPrompt action → dispatch → effect pipeline.
    // We dispatch SendPrompt directly so the prompt_id is assigned by the
    // prompt dispatcher. After the dispatch, we register the delegation.
    let effects =
        crate::app::dispatch::dispatch(crate::app::actions::Action::SendPrompt(text.clone()), app);

    // Obtain the prompt_id from the agent's current_prompt_id (set by the
    // dispatch). If unavailable, use a fallback.
    let prompt_id = app
        .agents
        .get(&agent_id)
        .and_then(|a| a.session.current_prompt_id.clone())
        .unwrap_or_default();

    let session_id = app
        .agents
        .get(&agent_id)
        .and_then(|a| a.session.session_id.as_ref())
        .map(|s| s.0.as_ref().to_string())
        .unwrap_or_default();

    app.live_runtime.register_delegation(
        generation,
        delegation_id,
        agent_id,
        session_id,
        prompt_id,
    );

    // Restore the draft again after the prompt dispatch cleared the textarea.
    crate::live::handle::restore_draft(app, agent_id, &draft);

    effects
}

/// Snapshot the current prompt draft (text + cursor) for the given agent.
fn snapshot_draft(app: &AppView, agent_id: AgentId) -> DraftSnapshot {
    let Some(agent) = app.agents.get(&agent_id) else {
        return DraftSnapshot::default();
    };
    DraftSnapshot {
        text: agent.prompt.text().to_string(),
        cursor: agent.prompt.cursor(),
    }
}

/// Enforce the Live session binding: stop Live if the user navigated away
/// from the bound agent/session. Called each event-loop tick (like
/// `enforce_voice_session_bound`).
pub fn enforce_live_session_bound(app: &mut AppView) {
    if !app.live_active() {
        return;
    }
    let (agent_id, session_id) = match &app.live_runtime.state {
        LiveState::Active {
            agent_id,
            session_id,
            ..
        } => (*agent_id, session_id.clone()),
        _ => return,
    };
    if !app.is_live_bound_to_active(agent_id, &session_id) {
        dispatch_live_stop(app);
    }
}

/// Stop Live on quit/exit/close/ACP-disconnect/teardown. Idempotent.
pub fn stop_live_on_teardown(app: &mut AppView) {
    if app.live_in_flight() {
        dispatch_live_stop(app);
    }
}
