//! Codex Live (`/live`) dispatch: toggle, mute, stop, and delegation submit.
//!
//! Mutually exclusive with `/voice` dictation — starting Live stops Voice and
//! vice versa. Live is bound to `(generation, AgentId, SessionId)` and stops
//! idempotently on session/view switch, close/exit/quit, ACP disconnect, and
//! teardown.

use crate::app::actions::{Action, Effect};
use crate::app::agent::AgentId;
use crate::app::app_view::{ActiveView, AppView};
use crate::live::state::{DraftSnapshot, LiveState};

/// Stop `/voice` dictation if it's active or pending (mutual exclusion —
/// starting Live stops Voice, including asynchronous/pending starts).
fn stop_voice_for_live(app: &mut AppView) {
    if app.voice_listening() || app.voice_state.pending_cold_start() {
        app.voice_reset();
    }
    app.voice_ui_active = false;
}

/// Start the Codex Live session. The start primitive reached by the toggle
/// (`/live`). Binds to the active agent + session, preserving the composer
/// draft+cursor. If the active AgentView has no bound ACP session, emits the
/// existing `CreateSession` effect and defers to `PendingUnbound`; the event
/// loop resumes the start exactly once when the matching AgentId/SessionId is
/// established. Failure/cancel restores the draft and idles.
pub(super) fn dispatch_live_toggle(app: &mut AppView) -> Vec<Effect> {
    // Gate: silent no-op when the Live gate is off.
    if !app.live_mode_enabled {
        return vec![];
    }

    // If Live is active or pending, stop it (toggle off).
    if app.live_in_flight() {
        return dispatch_live_stop(app);
    }

    // Mutual exclusion: stop `/voice` if active or pending.
    stop_voice_for_live(app);

    // Resolve the active agent.
    let agent_id = match app.active_view {
        ActiveView::Agent(id) => id,
        _ => return vec![], // Not on an agent screen — silent no-op.
    };

    // Snapshot the draft+cursor before starting (preserved across the session).
    let draft = snapshot_draft(app, agent_id);

    // Check if the agent has a bound session.
    let session_id = app
        .agents
        .get(&agent_id)
        .and_then(|a| a.session.session_id.as_ref())
        .map(|s| s.0.as_ref().to_string());

    if let Some(session_id) = session_id {
        // Session is bound — start Live now (ColdStart → event loop spawns).
        start_live_cold_start(app, agent_id, session_id, draft);
        vec![]
    } else {
        // No bound session — dispatch NewSession (which emits CreateSession
        // via the normal action path, respecting auth + folder-trust gates)
        // and defer to PendingUnbound. The event loop's `resume_pending_live`
        // detects the newly-established session and transitions to ColdStart
        // exactly once.
        app.live_runtime.state = LiveState::PendingUnbound { agent_id, draft };
        crate::app::dispatch::dispatch(Action::NewSession, app)
    }
}

/// Transition to `ColdStart` — the event loop picks this up and spawns the
/// pipeline.
fn start_live_cold_start(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: String,
    draft: DraftSnapshot,
) {
    let generation = app.live_runtime.next_generation();
    app.live_runtime.state = LiveState::ColdStart {
        agent_id,
        session_id,
        generation,
        draft,
    };
}

/// Resume a `PendingUnbound` start when the matching AgentId now has a bound
/// session. Called from the event loop after ACP session establishment.
/// Returns true if a start was resumed.
pub fn resume_pending_live(app: &mut AppView) -> bool {
    let (agent_id, draft) = match &app.live_runtime.state {
        LiveState::PendingUnbound { agent_id, draft } => (*agent_id, draft.clone()),
        _ => return false,
    };

    // Check if the agent now has a bound session.
    let session_id = app
        .agents
        .get(&agent_id)
        .and_then(|a| a.session.session_id.as_ref())
        .map(|s| s.0.as_ref().to_string());

    if let Some(session_id) = session_id {
        start_live_cold_start(app, agent_id, session_id, draft);
        true
    } else {
        false
    }
}

/// Cancel a `PendingUnbound` start (e.g. the user navigated away, the session
/// creation failed, or the user pressed Esc). Restores the draft and idles.
pub fn cancel_pending_live(app: &mut AppView) {
    if let LiveState::PendingUnbound { agent_id, draft } = &app.live_runtime.state {
        let agent_id = *agent_id;
        let draft = draft.clone();
        crate::live::handle::restore_draft(app, agent_id, &draft);
    }
    app.live_reset();
}

/// Stop the Codex Live session unconditionally. Idempotent. Restores the
/// draft+cursor, terminalizes the broker, and tears down the pipeline.
pub(super) fn dispatch_live_stop(app: &mut AppView) -> Vec<Effect> {
    if !app.live_in_flight() {
        return vec![];
    }

    // Terminalize all broker delegations (cancel_all) and send CompleteDelegation
    // for any that haven't completed.
    let cancel_decision = app
        .live_runtime
        .broker
        .observe_cancel_all(&app.live_runtime.delegations);
    let cmds = crate::live::broker::decision_to_commands(&cancel_decision);
    for cmd in cmds {
        app.live_send_cmd(cmd);
    }
    // Mark all delegations terminal in the registry.
    let delegation_ids: Vec<(u64, String)> = app.live_runtime.delegations.keys().cloned().collect();
    for (generation, delegation_id) in delegation_ids {
        app.live_runtime
            .mark_delegation_terminal(generation, &delegation_id);
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

/// Toggle the Live mute state (Space key in the visualizer).
pub(super) fn dispatch_live_toggle_mute(app: &mut AppView) -> Vec<Effect> {
    if !app.live_active() {
        return vec![];
    }
    app.live_toggle_mute();
    vec![]
}

/// Handle a `LiveEvent::Delegation` — submit literal plain text through the
/// prompt pipeline to the bound AgentSession, preserve the draft, capture the
/// actual prompt_id, and register `(generation, delegation_id) ->
/// (AgentId, SessionId, prompt_id, lifecycle)`.
///
/// **Bound-session path**: validates the bound agent/session/generation before
/// dispatch. Never uses generic whatever-is-active routing. Does not use
/// empty-string fallbacks for prompt_id.
pub(super) fn dispatch_live_delegation_submit(
    app: &mut AppView,
    agent_id: AgentId,
    text: String,
    delegation_id: String,
    generation: u64,
    draft: DraftSnapshot,
) -> Vec<Effect> {
    // Validate the bound agent still exists and has the expected session.
    let session_id = match app
        .agents
        .get(&agent_id)
        .and_then(|a| a.session.session_id.as_ref())
        .map(|s| s.0.as_ref().to_string())
    {
        Some(sid) => sid,
        None => {
            // Agent or session gone — cancel the delegation.
            app.live_runtime
                .mark_delegation_terminal(generation, &delegation_id);
            return vec![];
        }
    };

    // Restore the draft first (the SendPrompt path clears the textarea, but
    // we want the draft preserved throughout).
    crate::live::handle::restore_draft(app, agent_id, &draft);

    // Submit the text as a prompt to the bound agent session. This goes
    // through the normal SendPrompt action → dispatch → effect pipeline.
    let effects = crate::app::dispatch::dispatch(Action::SendPrompt(text), app);

    // Capture the actual prompt_id from the agent's current_prompt_id (set
    // by the dispatch). If the prompt wasn't accepted (no prompt_id), mark
    // the delegation terminal and bail — no empty-string fallback.
    let prompt_id = match app
        .agents
        .get(&agent_id)
        .and_then(|a| a.session.current_prompt_id.clone())
    {
        Some(pid) if !pid.is_empty() => pid,
        _ => {
            // Prompt was not accepted — mark terminal and bail.
            app.live_runtime
                .mark_delegation_terminal(generation, &delegation_id);
            crate::live::handle::restore_draft(app, agent_id, &draft);
            return effects;
        }
    };

    // Register the delegation with the exact correlation.
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
        // Also check PendingUnbound — if the user navigated away from the
        // pending agent, cancel the pending start.
        if let LiveState::PendingUnbound { agent_id, .. } = &app.live_runtime.state
            && !matches!(app.active_view, ActiveView::Agent(active) if active == *agent_id)
        {
            cancel_pending_live(app);
        }
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
/// Terminalizes broker state and restores the draft.
pub fn stop_live_on_teardown(app: &mut AppView) {
    if app.live_in_flight() {
        dispatch_live_stop(app);
    }
}
