//! Applies [`VoiceEvent`]s from the voice pipeline to the prompt text and the dictation overlay.

use xai_grok_voice::VoiceEvent;

use crate::app::app_view::{AppView, VoiceTarget};
use crate::views::prompt_widget::PromptWidget;

/// Joins the committed prompt text and a voice fragment with a space.
/// The space is skipped when the prompt is empty or already ends in whitespace, which keeps trailing newlines.
pub(crate) fn combine_prompt_with_voice_text(existing: &str, text: &str) -> String {
    if existing.trim().is_empty() {
        text.to_string()
    } else if existing.ends_with(char::is_whitespace) {
        format!("{existing}{text}")
    } else {
        format!("{existing} {text}")
    }
}

/// Appends `text` to the prompt bound at capture start (agent or dashboard).
/// The text lands at the end, or replaces a blank draft.
/// The caret follows only when it was already at the end; mid-text edits keep their place.
fn append_voice_text_to_prompt(app: &mut AppView, text: &str) {
    let append = |prompt: &mut PromptWidget| {
        let existing = prompt.text();
        let cursor = prompt.cursor();
        let blank = existing.trim().is_empty();
        // A blank draft is a full replace, so the caret parks at the new end
        // Otherwise the text appends at the end, and the caret follows only if it was already there
        let follow_end = blank || cursor >= existing.len();
        let combined = combine_prompt_with_voice_text(existing, text);
        prompt.set_text(&combined);
        prompt.set_cursor(if follow_end { combined.len() } else { cursor });
    };
    match app.voice_recording_target() {
        Some(VoiceTarget::Agent(id)) => {
            let Some(agent) = app.agents.get_mut(&id) else {
                return;
            };
            append(&mut agent.prompt);
        }
        Some(target @ (VoiceTarget::DashboardDispatch | VoiceTarget::DashboardPeekReply(_))) => {
            let Some(dashboard) = app.dashboard.as_mut() else {
                return;
            };
            // The peek reply box is shared across rows, so the text lands only if the bound row is still the peeked one
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
            append(prompt);
        }
        None => {}
    }
}

/// Move non-empty interim into the bound prompt and clear the overlay.
/// Does not stop the mic. Returns the promoted fragment.
pub(crate) fn commit_interim_into_prompt(app: &mut AppView) -> Option<String> {
    let interim = app
        .voice_interim()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)?;
    append_voice_text_to_prompt(app, &interim);
    app.voice_clear_interim();
    Some(interim)
}

/// Apply a voice event to app state. Returns whether the frame should redraw.
pub fn handle_voice_event(app: &mut AppView, event: VoiceEvent) -> bool {
    match event {
        VoiceEvent::InterimTranscript { text } => {
            // No-op unless recording, so a late interim after a stop can't repopulate the overlay
            app.voice_set_interim(text)
        }
        VoiceEvent::UtteranceFinal { text } => {
            app.voice_clear_interim();
            // The mic stays open across pauses; the user stops it explicitly, then presses Enter to send
            // The bound target survives a stop (`Stopping`), so a trailing final after an explicit stop still lands
            if !text.trim().is_empty() {
                append_voice_text_to_prompt(app, text.trim());
            }
            true
        }
        VoiceEvent::Error { message, hint } => {
            let target = app.voice_recording_target();
            app.voice_reset();
            app.show_toast(&format!("Voice: {message}"));
            // The hint holds long fix steps, so it goes to the agent or peek scrollback; a toast is one line, and dashboard dispatch has no scrollback
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
