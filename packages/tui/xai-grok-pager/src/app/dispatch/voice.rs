//! Voice mode enable, toggle, and stop dispatchers.

use super::session::lifecycle::dispatch_new_session;
use crate::app::actions::Effect;
use crate::app::app_view::{ActiveView, AppView, VoiceState, VoiceTarget};

/// Promote live interim into the bound prompt, then hard-reset (no trailing final).
/// Returns the fragment for callers that captured text earlier.
pub(super) fn voice_stop_on_submit(app: &mut AppView) -> Option<String> {
    let interim = crate::voice::commit_interim_into_prompt(app);
    app.voice_reset();
    interim
}

/// Merge interim into a payload captured before [`voice_stop_on_submit`].
pub(super) fn merge_prompt_with_voice_interim(existing: String, interim: Option<String>) -> String {
    match interim {
        Some(interim) => crate::voice::combine_prompt_with_voice_text(&existing, &interim),
        None => existing,
    }
}

/// The prompt box dictation should target for the current view.
/// That is a top-level row's peek reply when one is open, the new-agent dispatch input otherwise, or the active agent's prompt.
/// `None` off those views.
/// A non-top-level peek (subagent / roster, which can't accept a reply) maps to the dispatch box.
/// `enforce_voice_session_bound` then stops that session since a peek is open.
fn voice_target_for_view(app: &AppView) -> Option<VoiceTarget> {
    use crate::views::dashboard::DashboardRowId;
    match app.active_view {
        ActiveView::Agent(id) => Some(VoiceTarget::Agent(id)),
        ActiveView::AgentDashboard => {
            let dashboard = app.dashboard.as_ref();
            // The attached-agent popup hides the dispatch/peek inputs; don't bind dictation to a box the user can't see (no overlay, finals lost)
            if dashboard.is_some_and(|d| d.attached_agent.is_some()) {
                return None;
            }
            Some(
                match dashboard.and_then(|d| d.peek.as_ref()).map(|p| &p.row) {
                    Some(DashboardRowId::TopLevel(id)) => VoiceTarget::DashboardPeekReply(*id),
                    _ => VoiceTarget::DashboardDispatch,
                },
            )
        }
        _ => None,
    }
}

/// Show the SuperGrok upsell when a tier-restricted (free / X Basic) user tries to start voice via the Ctrl+Space / F8 keybinding.
/// That keybinding bypasses the slash registry (`/voice` is instead hidden and upsold via the deny list).
/// Mirrors the slash-command upsell: a Q&A modal on an agent screen ([`super::billing::open_restricted_command_upsell`]).
/// On the dashboard it is the feedback toast (no modal there).
/// Elsewhere (e.g. the welcome screen, which has no agent to host a modal) it is a silent no-op.
/// Never starts voice; always returns no effects.
fn open_voice_tier_upsell(app: &mut AppView) -> Vec<Effect> {
    let login_method = app.login_method_id.as_ref().map(|id| id.0.to_string());
    match app.active_view {
        ActiveView::Agent(id) => {
            if let Some(agent) = app.agents.get_mut(&id) {
                super::billing::open_restricted_command_upsell(agent, login_method);
            }
        }
        ActiveView::AgentDashboard => {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_error_toast(&format!(
                    "/voice requires SuperGrok: upgrade at {}",
                    super::billing::UPSELL_URL_UPGRADE
                ));
            }
        }
        _ => {}
    }
    vec![]
}

/// Enable session voice mode and start recording.
/// The start primitive reached by the toggle ([`dispatch_voice_toggle`], i.e. `/voice` / Ctrl+Space when idle) and the Ctrl+Space hold-to-talk press.
///
/// **Gated on the remote settings flag and the subscription tier.**
/// When voice isn't available (flag off, or a build without audio capture) this is a **silent no-op** with no toast.
/// Users who don't have the feature see nothing.
/// When voice IS available but the user is on a restricted tier (free / X Basic), it shows the SuperGrok upsell instead of starting a session.
/// This is the enforcement point for the keybinding, which bypasses the slash registry (see [`open_voice_tier_upsell`]).
/// Otherwise dictation routes into a prompt box: the active agent's prompt, or the dashboard's dispatch (new-agent) input.
/// On the session-less welcome screen (first launch) a session is created first, so voice works from a cold start in one press.
/// That goes via the gated [`dispatch_new_session`], so auth and folder-trust are respected.
/// Any other view with no visible box (off-screen, or the dashboard behind a popup) is a silent no-op.
///
/// `from_hold` marks a Ctrl+Space hold-press start (`VoiceState::*::hold`).
/// The matching Ctrl+Space release (see [`dispatch_voice_stop`]) then ends *this* session and only this one.
/// `/voice` and the toggle pass `false` so a Ctrl+Space release can't stop them.
pub(super) fn dispatch_enable_voice_mode(app: &mut AppView, from_hold: bool) -> Vec<Effect> {
    // Remote-flag gate only. Silent when unavailable.
    if !app.voice_mode_enabled || !xai_grok_voice::AUDIO_SUPPORTED {
        return vec![];
    }
    // Tier gate: free / X Basic personal users can't use voice (the server zero-limits these tiers)
    // The Ctrl+Space / F8 keybinding bypasses the slash registry, so this is the enforcement point for it
    // Show the SuperGrok upsell instead of starting a doomed session (`/voice` itself is separately hidden and upsold via the deny list)
    if app.is_voice_tier_restricted() {
        return open_voice_tier_upsell(app);
    }
    // The session-less welcome screen (first launch) has no prompt box, so create a session there, letting voice dictate into it from a cold start
    // That goes via the gated `dispatch_new_session`, so auth and folder-trust hold
    // `switch_to_agent` makes the new agent the active view, which the target lookup below then binds
    let mut effects = Vec::new();
    if matches!(app.active_view, ActiveView::Welcome) && app.session_startup_allowed() {
        effects = dispatch_new_session(app);
    }

    // Bind the dictation target at press time (the cold-start path defers capture to the event loop, where the view could have changed)
    // `None` means a view with no box (occluded dashboard, or a welcome the gate kept closed), so stay a silent no-op
    let Some(target) = voice_target_for_view(app) else {
        return effects;
    };

    app.voice_ui_active = true;
    if app.voice_cmd_tx.is_some() {
        // Pipeline already up. Start a new recording now; if one is already live leave it (and its hold-ownership) untouched.
        // A press over an existing session doesn't take it over
        if !app.voice_listening() {
            app.voice_begin_recording(target, from_hold);
        }
    } else if !app.voice_state.pending_cold_start() {
        // Pipeline still spawning. Queue a cold-start, but only if one isn't already pending.
        // A second toggle/press must re-affirm the first start, not clobber its hold-ownership or its bound target
        // Hold-ownership decides whether a Ctrl+Space release cancels the start
        // The event loop opens the mic once the pipeline is live
        app.voice_state = VoiceState::ColdStart {
            hold: from_hold,
            target,
        };
    }
    effects
}

/// Toggle mic capture: `Ctrl+Space`, Esc (while listening), the recording-row `[stop]`, and `Ctrl+Space` on terminals without key-release events.
/// While recording this stops; otherwise it starts, enabling voice mode and spawning the pipeline if needed, exactly like `/voice`.
/// Running `/voice` first is not required.
pub(super) fn dispatch_voice_toggle(app: &mut AppView) -> Vec<Effect> {
    if app.voice_listening() {
        // Stop always succeeds, even if the remote flag or `/voice` mode flipped mid-recording
        app.voice_stop_keeping_final();
        return vec![];
    }
    // Not recording: start. Mirrors `/voice` so the banner appears with one keypress (enables voice mode and spawns the pipeline if it isn't up yet).
    // Not a hold, so a stray Ctrl+Space release won't cancel a queued cold-start.
    dispatch_enable_voice_mode(app, /* from_hold */ false)
}

/// Ctrl+Space hold-to-talk key release: end the session a Ctrl+Space hold started.
/// That cancels a cold-start the hold queued (so a quick tap captures nothing) or stops a live hold recording.
/// A `/voice` / toggle session is left untouched, so a Ctrl+Space release can neither cancel its queued start nor stop its recording.
pub(super) fn dispatch_voice_stop(app: &mut AppView) -> Vec<Effect> {
    app.voice_hold_release();
    vec![]
}
