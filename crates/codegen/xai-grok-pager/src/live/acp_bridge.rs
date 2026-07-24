//! ACP bridge hooks — called from the ACP handler to feed the
//! `LiveDelegationBroker` and convert its decisions into `LiveCommand`s.
//!
//! These functions are no-ops when Live is not active or the notification
//! doesn't match a bound delegation. They never block (all sends are
//! `try_send`).

#![cfg(feature = "codex-live")]

use crate::app::app_view::AppView;
use crate::live::broker::decision_to_commands;
use crate::live::state::LiveState;

/// Hook: an `AgentMessageChunk` arrived for `session_id`+`prompt_id` with
/// `text`. If Live is active and the delegation is registered, accumulate the
/// text and optionally flush commentary at tool boundaries. No blocking sends.
pub fn on_agent_message_chunk(
    app: &mut AppView,
    session_id: &str,
    prompt_id: &str,
    text: &str,
    is_tool_boundary: bool,
) {
    if !app.live_active() {
        return;
    }
    let decision = app.live_runtime.broker.observe_chunk(
        session_id,
        prompt_id,
        text,
        is_tool_boundary,
        &app.live_runtime.delegations,
    );
    let cmds = decision_to_commands(&decision);
    for cmd in cmds {
        app.live_send_cmd(cmd);
    }
}

/// Hook: a turn completed (or `PromptResponse` success) for
/// `session_id`+`prompt_id`. Emits the terminal `CompleteDelegation` exactly
/// once (idempotent).
pub fn on_turn_completed(app: &mut AppView, session_id: &str, prompt_id: &str) {
    if !app.live_active() {
        return;
    }
    let decision = app.live_runtime.broker.observe_turn_completed(
        session_id,
        prompt_id,
        &app.live_runtime.delegations,
    );
    let cmds = decision_to_commands(&decision);
    for cmd in cmds {
        app.live_send_cmd(cmd);
    }
    // Mark delegations terminal in the registry.
    for del_id in &decision.mark_terminal {
        if let Some(generation) = app.live_runtime.state.generation() {
            app.live_runtime
                .mark_delegation_terminal(generation, del_id);
        }
    }
}

/// Hook: a prompt error / cancel / failure for `session_id`+`prompt_id`.
/// Marks the delegation terminal without a final message (idempotent).
pub fn on_prompt_error(app: &mut AppView, session_id: &str, prompt_id: &str) {
    if !app.live_active() {
        return;
    }
    let decision = app.live_runtime.broker.observe_failure(
        session_id,
        prompt_id,
        &app.live_runtime.delegations,
    );
    let cmds = decision_to_commands(&decision);
    for cmd in cmds {
        app.live_send_cmd(cmd);
    }
    for del_id in &decision.mark_terminal {
        if let Some(generation) = app.live_runtime.state.generation() {
            app.live_runtime
                .mark_delegation_terminal(generation, del_id);
        }
    }
}

/// Hook: session disconnect / ACP failure / navigation / teardown.
/// Terminalizes all active delegations and stops Live.
pub fn on_session_disconnect(app: &mut AppView) {
    if !app.live_in_flight() {
        return;
    }
    crate::app::stop_live_on_teardown(app);
}

/// Whether a notification for `session_id`+`prompt_id` should be fed to the
/// broker (i.e. Live is active, the session matches, and the prompt_id
/// belongs to a registered non-terminal delegation).
pub fn should_observe(app: &AppView, session_id: &str, prompt_id: &str) -> bool {
    if !app.live_active() {
        return false;
    }
    let bound_session = match &app.live_runtime.state {
        LiveState::Active { session_id, .. } => session_id.as_str(),
        _ => return false,
    };
    if bound_session != session_id {
        return false;
    }
    // Check if the prompt_id belongs to a registered delegation.
    app.live_runtime
        .find_delegation_by_prompt_id(prompt_id)
        .is_some()
}
