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
        LiveEvent::Phase(_) | LiveEvent::Levels(_) => {
            if app.live_runtime.visualizer.apply_event(&event) {
                needs_draw = true;
            }
        }
        LiveEvent::Transcript { kind, .. } => {
            let transcript_changed = app.live_runtime.visualizer.apply_event(&event);
            needs_draw |= transcript_changed;
            if *kind == super::TranscriptKind::Output && transcript_changed {
                let merged = app.live_runtime.visualizer.assistant_transcript.clone();
                needs_draw |= stream_assistant_transcript_to_scrollback(app, &merged);
            }
        }
        LiveEvent::Turn { role, .. } => {
            let finalized_changed = app.live_runtime.visualizer.apply_event(&event);
            needs_draw |= finalized_changed;

            // Finalize only a NEW assistant turn. A duplicate `turn.done`
            // leaves `finalized_changed == false` and the existing block intact.
            // Use the MERGED transcript rather than raw event text: OMP preserves
            // a longer streaming current when the final snapshot is a prefix.
            if *role == LiveRole::Assistant
                && finalized_changed
                && !app.live_runtime.visualizer.assistant_flushed
            {
                let merged = app.live_runtime.visualizer.assistant_transcript.clone();
                needs_draw |= finalize_assistant_transcript_to_scrollback(app, &merged);
            }
            // Do not reset role state here. Keeping the finalized current text
            // is required to reject late duplicate deltas and finals.
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
            // Preserve any partial spoken response as a finalized transcript;
            // the event loop performs channel/state teardown afterward.
            finish_live_assistant_transcript(app);
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

    // Reject a duplicate `delegation.created` id before it can enqueue the
    // same coding task twice. The broker registration happens before the
    // action is returned, so it also closes the small gap before dispatch
    // records the prompt correlation in `live_runtime.delegations`.
    if app.live_runtime.has_delegation(generation, delegation_id)
        || !app
            .live_runtime
            .broker
            .register_delegation(delegation_id.to_string())
    {
        tracing::debug!(
            delegation_id,
            generation,
            "duplicate Live delegation ignored"
        );
        return false;
    }

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

fn labeled_assistant_transcript(text: &str) -> String {
    let label = rust_i18n::t!("live.assistant_label");
    format!("{label}: {text}")
}

/// Replace the contents of a Live-owned streaming agent-message entry while
/// preserving its running identity in `ScrollbackState`.
fn set_assistant_entry_text(
    agent: &mut crate::app::agent_view::AgentView,
    entry_id: crate::scrollback::entry::EntryId,
    text: String,
) -> bool {
    let updated = if let Some(entry) = agent.scrollback.get_by_id_mut(entry_id)
        && matches!(
            entry.block,
            crate::scrollback::block::RenderBlock::AgentMessage(_)
        ) {
        entry.block = crate::scrollback::block::RenderBlock::agent_message(text);
        entry.invalidate_cache();
        true
    } else {
        false
    };
    if updated {
        agent.scrollback.mark_height_dirty(entry_id);
    }
    updated
}

/// Create or update the transient spoken-assistant block on every output
/// transcript delta. The full merged text replaces the block so cumulative
/// resends and corrected finals never duplicate words in scrollback.
fn stream_assistant_transcript_to_scrollback(app: &mut AppView, text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let (Some(agent_id), Some(generation)) = (
        app.live_runtime.state.agent_id(),
        app.live_runtime.state.generation(),
    ) else {
        return false;
    };
    let labeled = labeled_assistant_transcript(text);

    if let Some(entry) = app.live_runtime.assistant_transcript_entry
        && entry.agent_id == agent_id
        && entry.generation == generation
    {
        let updated = app
            .agents
            .get_mut(&agent_id)
            .is_some_and(|agent| set_assistant_entry_text(agent, entry.entry_id, labeled.clone()));
        if updated {
            return true;
        }
        // The entry was removed or changed externally; recreate it below.
        app.live_runtime.assistant_transcript_entry = None;
    }

    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return false;
    };
    let entry_id = agent.scrollback.start_streaming_agent();
    if !set_assistant_entry_text(agent, entry_id, labeled) {
        agent.scrollback.finish_running(entry_id);
        return false;
    }
    app.live_runtime.assistant_transcript_entry =
        Some(super::state::LiveAssistantTranscriptEntry {
            generation,
            agent_id,
            entry_id,
        });
    app.live_runtime.visualizer.assistant_flushed = false;
    true
}

/// Finalize the current spoken-assistant block, or create a complete block if
/// `turn.done` arrived without incremental transcript events.
fn finalize_assistant_transcript_to_scrollback(app: &mut AppView, text: &str) -> bool {
    if text.trim().is_empty() || app.live_runtime.visualizer.assistant_flushed {
        return false;
    }
    let Some(agent_id) = app.live_runtime.state.agent_id() else {
        return false;
    };
    let generation = app.live_runtime.state.generation();
    let labeled = labeled_assistant_transcript(text);

    if let Some(entry) = app.live_runtime.assistant_transcript_entry.take() {
        if entry.agent_id == agent_id && Some(entry.generation) == generation {
            let finalized = app.agents.get_mut(&agent_id).is_some_and(|agent| {
                let updated = set_assistant_entry_text(agent, entry.entry_id, labeled.clone());
                if updated {
                    agent.scrollback.finish_running(entry.entry_id);
                }
                updated
            });
            if finalized {
                app.live_runtime.visualizer.assistant_flushed = true;
                return true;
            }
        } else if let Some(agent) = app.agents.get_mut(&entry.agent_id) {
            // Defense in depth for a stale generation/view transition.
            agent.scrollback.finish_running(entry.entry_id);
        }
    }

    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return false;
    };
    agent
        .scrollback
        .push_block(crate::scrollback::block::RenderBlock::agent_message(
            labeled,
        ));
    app.live_runtime.visualizer.assistant_flushed = true;
    true
}

/// Finish a partial spoken response during stop/disconnect teardown. Idempotent
/// after an ordinary assistant `turn.done`.
pub fn finish_live_assistant_transcript(app: &mut AppView) -> bool {
    if app.live_runtime.assistant_transcript_entry.is_none() {
        return false;
    }
    let text = app.live_runtime.visualizer.assistant_transcript.clone();
    if text.trim().is_empty() {
        if let Some(entry) = app.live_runtime.assistant_transcript_entry.take()
            && let Some(agent) = app.agents.get_mut(&entry.agent_id)
        {
            agent.scrollback.finish_running(entry.entry_id);
            return true;
        }
        return false;
    }
    finalize_assistant_transcript_to_scrollback(app, &text)
}
