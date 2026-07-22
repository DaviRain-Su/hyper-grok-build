//! Map pipeline [`VoiceEvent`]s onto prompt-box dictation state.

use xai_grok_voice::VoiceEvent;

use crate::app::app_view::{AppView, VoiceTarget};

/// Append finalized text to whichever prompt started capture
/// (`voice_recording_target`) — the agent prompt or the dashboard dispatch input
/// — not necessarily the active view, so a late final after a view switch still
/// lands in the right place. Inserts a single separating space unless the prompt
/// is empty or already ends in whitespace (preserves trailing newlines).
fn append_voice_text_to_prompt(app: &mut AppView, text: &str) {
    let combine = |existing: &str| -> String {
        if existing.trim().is_empty() {
            text.to_string()
        } else if existing.ends_with(char::is_whitespace) {
            format!("{existing}{text}")
        } else {
            format!("{existing} {text}")
        }
    };
    match app.voice_recording_target() {
        Some(VoiceTarget::Agent(id)) => {
            let Some(agent) = app.agents.get_mut(&id) else {
                return;
            };
            let combined = combine(agent.prompt.text());
            agent.prompt.set_text(&combined);
            agent.prompt.set_cursor(combined.len());
        }
        Some(target @ (VoiceTarget::DashboardDispatch | VoiceTarget::DashboardPeekReply(_))) => {
            let Some(dashboard) = app.dashboard.as_mut() else {
                return;
            };
            // Route to the box bound at capture start. The dispatch box is stable,
            // but the peek reply widget is *shared* across rows and reassigned when
            // the peeked row changes. While listening `enforce_voice_session_bound`
            // stops capture on a row change, but after an explicit stop the target
            // is kept for the trailing final and that guard no longer runs — so
            // re-check the bound row here, or a final would land in (and send from)
            // another agent's reply.
            let prompt = match target {
                VoiceTarget::DashboardPeekReply(rec) => {
                    let peeked = match dashboard.peek.as_ref().map(|p| &p.row) {
                        Some(crate::views::dashboard::DashboardRowId::TopLevel(id)) => Some(*id),
                        _ => None,
                    };
                    if peeked != Some(rec) {
                        return;
                    }
                    &mut dashboard.peek_reply
                }
                _ => &mut dashboard.dispatch,
            };
            let combined = combine(prompt.text());
            prompt.set_text(&combined);
            prompt.set_cursor(combined.len());
        }
        None => {}
    }
}

/// Apply a voice event to app state. Returns whether the frame should redraw.
pub fn handle_voice_event(app: &mut AppView, event: VoiceEvent) -> bool {
    match event {
        VoiceEvent::InterimTranscript { text } => {
            // No-op unless recording, so a late interim after a stop can't
            // repopulate the overlay.
            app.voice_set_interim(text)
        }
        VoiceEvent::UtteranceFinal { text } => {
            app.voice_clear_interim();
            // Keep the mic open across pauses; user stops explicitly, then Enter to send.
            // The bound target survives a stop (`Stopping`), so a trailing final
            // after an explicit stop still lands.
            if !text.trim().is_empty() {
                append_voice_text_to_prompt(app, text.trim());
            }
            true
        }
        VoiceEvent::Error { message, hint } => {
            let target = app.voice_recording_target();
            app.voice_reset();
            app.show_toast(&format!("Voice: {message}"));
            // Long fix steps: agent/peek scrollback only (toast is one line;
            // dashboard dispatch has no scrollback).
            if let Some(hint) = hint
                && let Some(VoiceTarget::Agent(id) | VoiceTarget::DashboardPeekReply(id)) = target
                && let Some(agent) = app.agents.get_mut(&id)
            {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Voice: {message}. {hint}"
                    )));
            }
            true
        }
    }
}
