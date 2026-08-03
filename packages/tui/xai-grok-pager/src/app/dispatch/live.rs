//! Codex Live (`/live`) dispatch: toggle, mute, stop, and delegation submit.
//!
//! Mutually exclusive with `/voice` dictation — starting Live stops Voice and
//! vice versa. Live is bound to `(generation, AgentId, SessionId)` and stops
//! idempotently on session/view switch, close/exit/quit, ACP disconnect, and
//! teardown.

use crate::app::actions::Effect;
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
        // No bound session — emit `Effect::CreateSession` for the SAME agent
        // (not `Action::NewSession`, which creates/switches to a new AgentId).
        // The event loop's `resume_pending_live` detects the newly-established
        // session for this same agent_id and transitions to ColdStart exactly
        // once.
        app.live_runtime.state = LiveState::PendingUnbound { agent_id, draft };
        super::session::lifecycle::skip_picker_and_create_session(app, agent_id)
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
    // `observe_cancel_all` deliberately returns terminal identities without a
    // success payload. Best-effort an explicit wrapped cancellation while the
    // command channel has room; session close remains the terminal guarantee
    // if a saturated channel cannot accept these before teardown.
    let cancelled = crate::live::prompts::wrap_agent_final_message(
        "The delegated coding task was cancelled because Live stopped.",
    );
    for delegation_id in &cancel_decision.mark_terminal {
        app.live_send_cmd(crate::live::LiveCommand::CompleteDelegation {
            delegation_id: delegation_id.clone(),
            text: cancelled.clone(),
        });
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

/// Complete a delegation that could not enter the prompt pipeline. This keeps
/// the Live model from waiting forever while deliberately leaving every
/// pre-existing user queue entry untouched.
fn reject_delegation_submit(app: &mut AppView, generation: u64, delegation_id: &str, reason: &str) {
    app.live_runtime
        .mark_delegation_terminal(generation, delegation_id);
    app.live_send_cmd(crate::live::LiveCommand::CompleteDelegation {
        delegation_id: delegation_id.to_string(),
        text: crate::live::prompts::wrap_agent_final_message(reason),
    });
}

/// Handle a `LiveEvent::Delegation` — submit literal plain text through the
/// prompt pipeline to the bound AgentSession, preserve the draft, capture the
/// actual prompt_id from the returned `Effect::SendPrompt`, and register
/// `(generation, delegation_id) -> (AgentId, SessionId, prompt_id, lifecycle)`.
///
/// **Bound-session path**: validates the bound agent/session/generation AND
/// the active view before dispatch. Never uses generic whatever-is-active
/// routing. Does not use empty-string fallbacks for prompt_id. Does not
/// consume or mutate the user's composer text, images, or history.
pub(super) fn dispatch_live_delegation_submit(
    app: &mut AppView,
    agent_id: AgentId,
    text: String,
    delegation_id: String,
    generation: u64,
    draft: DraftSnapshot,
) -> Vec<Effect> {
    // ── Revalidate the Live binding immediately at dispatch time ──────────
    // The state must still be Active with the same generation, agent, and
    // session, and the active view must be the bound agent.
    let (bound_agent, bound_session, bound_generation) = match &app.live_runtime.state {
        LiveState::Active {
            agent_id: aid,
            session_id: sid,
            generation: g,
            ..
        } => (*aid, sid.clone(), *g),
        _ => {
            // Live is no longer active — fail the delegation explicitly.
            app.live_runtime
                .mark_delegation_terminal(generation, &delegation_id);
            return vec![];
        }
    };

    // Generation mismatch — stale delegation from a prior session.
    if bound_generation != generation {
        app.live_runtime
            .mark_delegation_terminal(generation, &delegation_id);
        return vec![];
    }

    // Defense in depth: `handle_live_event` rejects duplicate created frames
    // before creating this action, but a duplicated/delayed action must also
    // never submit the same coding task twice.
    if app.live_runtime.has_delegation(generation, &delegation_id) {
        return vec![];
    }

    // Agent mismatch — the delegation was for a different agent.
    if bound_agent != agent_id {
        app.live_runtime
            .mark_delegation_terminal(generation, &delegation_id);
        return vec![];
    }

    // The active view must be the bound agent (never generic routing).
    if !matches!(app.active_view, ActiveView::Agent(active) if active == agent_id) {
        app.live_runtime
            .mark_delegation_terminal(generation, &delegation_id);
        return vec![];
    }

    // Validate the bound agent still exists and has the expected session.
    let session_id = match app
        .agents
        .get(&agent_id)
        .and_then(|a| a.session.session_id.as_ref())
        .map(|s| s.0.as_ref().to_string())
    {
        Some(sid) if sid == bound_session => sid,
        _ => {
            // Agent/session gone or mismatched — fail the delegation.
            app.live_runtime
                .mark_delegation_terminal(generation, &delegation_id);
            return vec![];
        }
    };

    // ── Submit literal text without consuming the composer ────────────────
    // Use `dispatch_send_prompt_inner` with `consume_input=false` (does not
    // clear the textarea, does not drain prompt images) and `literal=true`
    // (bypasses slash-command parsing and project-picker). This submits the
    // delegation text as a plain prompt without touching the user's draft,
    // images, or history.
    //
    // ── Composer image isolation ──────────────────────────────────────────
    // `dispatch_send_prompt_inner`'s immediate-server-send eligibility gates
    // on `agent.prompt.images.is_empty()`: with draft images attached AND a
    // turn running, the literal delegation would otherwise be enqueued
    // locally (no `Effect::SendPrompt` returned), the delegation would be
    // reported as failed, and untracked work would execute later from the
    // local queue. The delegation text is literal plain text with NO images of
    // its own, so we temporarily detach the composer's images for the duration
    // of the dispatch and reattach them AFTER any draft restore below. This
    // lets the delegation take the same immediate-send path as any plain
    // literal prompt while preserving the user's draft images byte-for-byte.
    // (Restoring after `restore_draft` is load-bearing: `set_text("")` drains
    // prompt images, so an earlier restore would be wiped by the draft
    // restore.)
    // Snapshot the exact local queue identity before calling the generic
    // prompt dispatcher. A Live delegation must never merge with, drain, or
    // hide a pre-existing user prompt. `immediate_server_send_eligible`
    // intentionally refuses immediate sends while local work is queued, so
    // there is no safe acceptance path in that case.
    let (delegation_queue_id, had_pending_user_prompt) = app
        .agents
        .get(&agent_id)
        .map(|agent| {
            (
                agent.session.next_queue_id,
                !agent.session.pending_prompts.is_empty(),
            )
        })
        .unwrap_or((0, true));
    if had_pending_user_prompt {
        reject_delegation_submit(
            app,
            generation,
            &delegation_id,
            "Delegation failed: the agent already has queued user work",
        );
        return vec![];
    }

    let stashed_images = app
        .agents
        .get_mut(&agent_id)
        .map(|agent| agent.prompt.drain_images())
        .unwrap_or_default();
    let delegation_text = text.clone();

    let effects = super::prompt::dispatch_send_prompt_inner(
        app, text, false, // consume_input = false — preserve the composer
        true,  // literal = true — bypass slash/picker
        false, // is_follow_up = false
    );

    // ── Extract the actual prompt_id from the returned Effect::SendPrompt ─
    // We read the prompt_id from the effect, NOT from `current_prompt_id`
    // (which can be an older running prompt). The effect must match the exact
    // bound agent_id and session_id — a mismatched effect (e.g. routed to a
    // different agent/session by a generic path) is treated as not-accepted so
    // no untracked delegation work executes. If no matching
    // `Effect::SendPrompt` was returned, the prompt was not accepted (queued,
    // reconnecting, etc.) — fail the delegation explicitly and leave no queued
    // delegation behind.
    // If generic dispatch enqueued the delegation but could not drain it
    // (model switch, replay, command-running state, queue edit, etc.), remove
    // exactly the row minted from our `next_queue_id` snapshot. Never clear or
    // reorder unrelated user rows.
    let removed_unsent_queue_row = app.agents.get_mut(&agent_id).is_some_and(|agent| {
        let before = agent.session.pending_prompts.len();
        agent
            .session
            .pending_prompts
            .retain(|queued| queued.id != delegation_queue_id);
        agent.session.pending_prompts.len() != before
    });

    let prompt_id = (!removed_unsent_queue_row)
        .then(|| {
            effects.iter().find_map(|e| match e {
                Effect::SendPrompt {
                    prompt_id,
                    text: effect_text,
                    agent_id: eff_agent,
                    session_id: eff_session,
                    ..
                } if *eff_agent == agent_id
                    && eff_session.0.as_ref() == bound_session.as_str()
                    && effect_text == &delegation_text =>
                {
                    Some(prompt_id.clone())
                }
                _ => None,
            })
        })
        .flatten()
        .filter(|pid| !pid.is_empty());

    let prompt_id = match prompt_id {
        Some(pid) => pid,
        None => {
            // Prompt was not accepted. The exact queue-id rollback above has
            // already removed any local delegation row while preserving all
            // user work, so it is safe to report terminal failure.
            reject_delegation_submit(
                app,
                generation,
                &delegation_id,
                "Delegation failed: prompt was not accepted",
            );
            // Restore the composer images before returning (no draft restore
            // runs on this path, so reattach directly).
            if !stashed_images.is_empty()
                && let Some(agent) = app.agents.get_mut(&agent_id)
            {
                agent.prompt.set_images(stashed_images);
            }
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

    // Restore the draft (the literal submit with consume_input=false should
    // not have cleared it, but restore anyway for safety). This may clear
    // prompt images via `set_text("")`, so the composer image reattach MUST
    // run after it.
    crate::live::handle::restore_draft(app, agent_id, &draft);

    // Reattach the composer images AFTER the draft restore. The delegation
    // path never owns the user's images; restoring here guarantees the user's
    // draft images survive the delegation regardless of the draft snapshot.
    if !stashed_images.is_empty()
        && let Some(agent) = app.agents.get_mut(&agent_id)
    {
        agent.prompt.set_images(stashed_images);
    }

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
