//! Session status, sharing, privacy, usage, and info dispatchers.

use agent_client_protocol as acp;

use super::ctx::get_active_agent;
use super::settings::ui::refresh_open_settings_modals;
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView};
use crate::notifications::{NotificationEvent, NotificationEventKind};
use crate::scrollback::block::RenderBlock;

/// Toggle YOLO mode (auto-approve all permissions).
///
/// When turning ON: auto-approve all currently queued permissions and
/// restore the stashed prompt. Future incoming permissions will be
/// auto-approved in `handle_permission_request`.
///
/// Share the current session via a public URL.
///
/// Produces Effect::ShareSession which spawns an async ACP ext request.
/// On completion, TaskResult::ShareSessionComplete shows the URL in scrollback.
pub(super) fn dispatch_share_session(app: &mut AppView) -> Vec<Effect> {
    if !app.sharing_enabled {
        app.show_toast("Sharing is disabled");
        return vec![];
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        // No active session — error should have been caught by slash command,
        // but guard here just in case.
        return vec![];
    };

    vec![Effect::ShareSession {
        agent_id: id,
        session_id,
    }]
}

/// Show session info: fetch via x.ai/session/info and display in scrollback.
///
/// Produces Effect::ShowSessionInfo which spawns an async ACP ext request.
/// On completion, TaskResult::SessionInfoComplete shows the formatted info.
pub(super) fn dispatch_show_session_info(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        // No active session — error should have been caught by slash command,
        // but guard here just in case.
        return vec![];
    };

    vec![Effect::ShowSessionInfo {
        agent_id: id,
        session_id,
        show_resolved_model: app.show_resolved_model,
    }]
}

/// State-only mutation for `coding_data_sharing`. SHELL-owned.
pub(super) fn set_coding_data_sharing_inner(app: &mut AppView, opted_in: bool) {
    app.coding_data_retention_opt_out = !opted_in;
}

/// Agent the coding-data ACP write is attributed to. Privacy is app-level,
/// so the id only routes the result back; `AgentId(0)` is the synthetic
/// stand-in for the welcome screen, where the banner is reachable before a
/// session exists.
fn coding_data_sharing_agent_id(app: &AppView) -> AgentId {
    match app.active_view {
        ActiveView::Agent(id) => id,
        _ => app.agents.keys().next().copied().unwrap_or(AgentId(0)),
    }
}

/// Claim the next write generation. Every `SetCodingDataSharing` must take
/// one so its reply can be matched against the newest write.
fn next_coding_data_write_seq(app: &mut AppView) -> u64 {
    app.coding_data_write_seq += 1;
    app.coding_data_write_seq
}

/// Is this reply from the newest write? Writes to this endpoint run
/// concurrently and can land out of order, so an older reply must not touch
/// state: its `rollback_to_opted_in` predates the newer write, and applying
/// it would silently undo whatever the user did since.
fn is_current_coding_data_write(app: &AppView, seq: u64, agent_id: AgentId) -> bool {
    if seq == app.coding_data_write_seq {
        return true;
    }
    tracing::debug!(
        target: "settings",
        key = "coding_data_sharing",
        ?agent_id,
        seq,
        current = app.coding_data_write_seq,
        "dropping superseded coding-data reply",
    );
    false
}

fn log_coding_data_consent_selected(
    source: xai_grok_telemetry::events::CodingDataConsentSource,
    opted_in: bool,
    previous_opted_in: bool,
) {
    use xai_grok_telemetry::events::{CodingDataConsentChoice, CodingDataConsentSelected};
    xai_grok_telemetry::session_ctx::log_event(CodingDataConsentSelected {
        source,
        choice: CodingDataConsentChoice::from_opted_in(opted_in),
        previous_choice: CodingDataConsentChoice::from_opted_in(previous_opted_in),
        changed: opted_in != previous_opted_in,
    });
}

/// Set coding-data-sharing preference. SHELL-owned, auth-metadata-backed
/// (persists via ACP ext-request, NOT `~/.grok/config.toml`).
pub(super) fn set_coding_data_sharing(
    app: &mut AppView,
    opted_in: bool,
    source: xai_grok_telemetry::events::CodingDataConsentSource,
) -> Vec<Effect> {
    // ── Guard 1: Enterprise ZDR ──────────────────────────────────────
    if app.is_zdr {
        app.show_toast("\u{2717} Cannot change: Zero Data Retention enabled");
        return vec![];
    }
    // ── Guard 2: Non-admin team member ───────────────────────────────
    if app.team_name.is_some() {
        let is_admin = app
            .team_role
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case("admin"));
        if !is_admin {
            app.show_toast("\u{2717} Data sharing is controlled by your team admin");
            return vec![];
        }
    }
    let agent_id = coding_data_sharing_agent_id(app);
    let prev = !app.coding_data_retention_opt_out;
    log_coding_data_consent_selected(source, opted_in, prev);

    // ── Idempotent path: skip the ACP round-trip. ────────────────────
    if prev == opted_in {
        return vec![];
    }

    // Optimistic mutation. Success is silent; only the refusals above and
    // the failure handler toast.
    set_coding_data_sharing_inner(app, opted_in);
    refresh_open_settings_modals(app);

    tracing::info!(
        target: "settings",
        key = "coding_data_sharing",
        opted_in,
        "setting changed",
    );

    vec![Effect::SetCodingDataSharing {
        agent_id,
        opted_in,
        rollback_to_opted_in: prev,
        seq: next_coding_data_write_seq(app),
    }]
}

/// Scrub an untrusted error string for toast display. Substitutes a
/// generic placeholder when the input exceeds 120 chars or contains
/// control / bidi-override characters (prevents escape-sequence
/// injection and visual spoofing). Full error stays in tracing logs.
pub(super) fn scrub_error_for_toast(error: &str) -> String {
    const MAX_TOAST_ERROR_LEN: usize = 120;
    if error.len() > MAX_TOAST_ERROR_LEN
        || error
            .chars()
            .any(crate::render::line_utils::is_unsafe_display_char)
    {
        "server error (see logs for details)".to_string()
    } else {
        error.to_string()
    }
}

/// Show context info: fetch via x.ai/session/info and display rich breakdown.
///
/// Produces Effect::ShowContextInfo which spawns an async ACP ext request.
/// On completion, TaskResult::ContextInfoComplete shows the formatted info.
pub(super) fn dispatch_show_context_info(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    vec![Effect::ShowContextInfo {
        agent_id: id,
        session_id,
    }]
}

/// `/usage` — session token/cost, then consumer credits when visible.
/// Credits are chained after the session block so layout stays ordered.
pub(super) fn dispatch_show_usage(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let session_id = {
        let Some(agent) = app.agents.get_mut(&id) else {
            return vec![];
        };
        agent.session.session_id.clone()
    };
    match session_id {
        Some(session_id) => vec![Effect::FetchSessionUsage {
            agent_id: id,
            session_id,
        }],
        None => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.scrollback.push_block(RenderBlock::system(
                    "Session usage is unavailable until the session starts.".to_string(),
                ));
            }
            append_consumer_billing_surface(app, id)
        }
    }
}

/// Commit a session-usage block if still on `session_id`, then consumer credits.
pub(super) fn commit_session_usage_block(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: &acp::SessionId,
    text: String,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    if agent.session.session_id.as_ref() != Some(session_id) {
        return vec![];
    }
    agent.scrollback.push_block(RenderBlock::system(text));
    append_consumer_billing_surface(app, agent_id)
}

/// Consumer credit follow-up for `/usage` (redirect or non-silent billing fetch).
pub(super) fn append_consumer_billing_surface(app: &mut AppView, agent_id: AgentId) -> Vec<Effect> {
    if !app.usage_visible {
        return vec![];
    }
    // Remote-settings kill switch (`grok_build_usage_redirect_url`): link out
    // instead of fetching billing from the backend.
    if let Some(url) = app.usage_billing_redirect_url.clone() {
        if let Some(agent) = app.agents.get_mut(&agent_id) {
            agent.scrollback.push_block(RenderBlock::System(
                crate::scrollback::blocks::SystemMessageBlock::new(format!(
                    "Please check your usage on {url}"
                )),
            ));
        }
        return vec![];
    }
    if !app.agents.contains_key(&agent_id) {
        return vec![];
    }
    // Non-silent: the effect also pulls the auto top-up rule so the summary
    // renders usage, prepaid credits, and auto top-up together.
    vec![Effect::FetchBilling {
        agent_id,
        silent: false,
    }]
}

/// `/usage manage` — open consumer billing. No-op when the surface is hidden.
pub(super) fn dispatch_manage_billing(app: &mut AppView) -> Vec<Effect> {
    if !app.usage_visible {
        return vec![];
    }
    super::router::dispatch(
        crate::app::actions::Action::OpenUrl("https://grok.com/?_s=usage".to_string()),
        app,
    )
}

/// Commit a one-line "update available" notice into the active agent's
/// scrollback. Minimal mode has no welcome screen (the full TUI's update
/// surface), so the background update check's result is shown here instead
/// No-op when there is no active agent.
pub(crate) fn commit_minimal_update_notice(app: &mut AppView, latest_version: &str) {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        agent.scrollback.push_block(RenderBlock::system(format!(
            "Update available: v{latest_version} — restart to apply."
        )));
    }
}

/// `/queue` — commit a read-only list of the queued prompts as a system block.
/// The text is built by [`crate::app::status_blocks::queue_block_text`]; this
/// just resolves the active agent and pushes it. Works in every render mode; the
/// primary inspection surface in minimal, which has no interactive `QueuePane`.
pub(super) fn dispatch_show_queue(app: &mut AppView) -> Vec<Effect> {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        let text = crate::app::status_blocks::queue_block_text(agent);
        agent.scrollback.push_block(RenderBlock::system(text));
    }
    vec![]
}

/// `/tasks` — commit a read-only list of background tasks, subagents, and
/// scheduled (`/loop`) tasks as a system block. The text is built by
/// [`crate::app::status_blocks::tasks_block_text`]; this just resolves the
/// active agent and pushes it. Works in every render mode; the primary snapshot
/// surface in minimal, which has no interactive `TasksPane`.
pub(super) fn dispatch_show_tasks(app: &mut AppView) -> Vec<Effect> {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        let text = crate::app::status_blocks::tasks_block_text(agent);
        agent.scrollback.push_block(RenderBlock::system(text));
    }
    vec![]
}

/// Open the hidden `/gboom` easter egg as a modal over the active agent
/// view. Requires a graphics-capable terminal (kitty protocol or iTerm2);
/// otherwise a toast explains why nothing happened. On session-less
/// surfaces (dashboard, welcome) this is a silent no-op.
///
/// Targets the top-level agent view (where the prompt lives), not a
/// focused subagent view: the modal's tick/draw plumbing runs on the
/// top-level view, mirroring the video viewer.
pub(super) fn dispatch_open_gboom(app: &mut AppView) -> Vec<Effect> {
    use crate::terminal::image::{GraphicsProtocol, detect_graphics_protocol};
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if detect_graphics_protocol() == GraphicsProtocol::None {
        agent.show_toast(
            "No demons here \u{2014} GBOOM needs a graphics-capable terminal \
             (kitty, Ghostty, WezTerm, iTerm2)",
        );
        return vec![];
    }
    // Close other media modals: they share the kitty placement id. Drop the
    // image viewer's in-flight loader too (its close path clears both —
    // a leaked rx would mis-feed the next image viewer's poll loop).
    agent.image_viewer = None;
    agent.image_load_rx = None;
    agent.video_viewer = None;
    agent.gboom = Some(crate::gboom::GboomState::new());
    vec![]
}

/// Emit a `SessionReady` notification for the given agent.
///
/// Takes `&NotificationService` separately from `&AgentView` to avoid
/// borrow-checker conflicts when `agent` is borrowed from `app.agents`.
pub(super) fn notify_session_ready(
    notification_service: &crate::notifications::NotificationService,
    agent: &AgentView,
) {
    notification_service.notify(NotificationEvent {
        kind: NotificationEventKind::SessionReady,
        title: "Grok".into(),
        body: NotificationEventKind::SessionReady.as_str().into(),
        session_id: agent.session.session_id.as_ref().map(|s| s.0.to_string()),
    });
}

// TaskResult handlers.

pub(super) fn handle_coding_data_sharing_updated(
    app: &mut AppView,
    agent_id: AgentId,
    opted_in: bool,
    seq: u64,
) -> Vec<Effect> {
    if !is_current_coding_data_write(app, seq, agent_id) {
        return vec![];
    }
    // Re-anchor mirror to server-confirmed value (defense-in-depth against
    // server reshaping the boolean). `agent_id` discarded — privacy is
    // app-level, not per-agent.
    set_coding_data_sharing_inner(app, opted_in);
    refresh_open_settings_modals(app);
    tracing::info!(
        target: "settings",
        key = "coding_data_sharing",
        ?agent_id,
        opted_in,
        "ACP update confirmed; mirror re-anchored",
    );
    let mut effects = vec![];
    // Ack only after a successful opt-in from the banner's [Opt in].
    if app.privacy_banner_opt_in_inflight {
        app.privacy_banner_opt_in_inflight = false;
        if opted_in {
            effects.extend(ack_privacy_banner(app));
        }
    }
    effects
}

pub(super) fn handle_coding_data_sharing_failed(
    app: &mut AppView,
    agent_id: AgentId,
    error: String,
    rollback_to_opted_in: bool,
    seq: u64,
) -> Vec<Effect> {
    // A superseded failure must not revert: `rollback_to_opted_in` predates
    // the newer write, so applying it would undo a change the user made
    // after this one was sent. It must not toast either — nothing the user
    // is looking at failed.
    if !is_current_coding_data_write(app, seq, agent_id) {
        return vec![];
    }
    // Revert optimistic mutation: inner → refresh → toast. `agent_id`
    // discarded — privacy is global.
    set_coding_data_sharing_inner(app, rollback_to_opted_in);
    refresh_open_settings_modals(app);
    // Scrub long/unsafe error strings before toasting.
    let scrubbed = scrub_error_for_toast(&error);
    app.show_toast(&format!(
        "\u{2717} Couldn't update coding data sharing: {scrubbed}"
    ));
    tracing::warn!(
        target: "settings",
        key = "coding_data_sharing",
        ?agent_id,
        rollback_to_opted_in,
        %error,
        "ACP update failed; reverted optimistic mutation",
    );
    // Opt-in failure: no ack; clear inflight so the banner stays.
    app.privacy_banner_opt_in_inflight = false;
    vec![]
}

/// Stamp `[privacy].privacy_banner_acked` (in-memory + disk).
pub(in crate::app::dispatch) fn ack_privacy_banner(app: &mut AppView) -> Vec<Effect> {
    let acked_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    app.privacy_banner_acked = Some(acked_at.clone());
    vec![Effect::PersistPrivacyBannerAcked { acked_at }]
}

/// `[Opt in]`: opt in via the settings path; ack only after ACP success, so
/// a failed round trip leaves the banner up instead of recording a change
/// that did not happen.
pub(in crate::app::dispatch) fn dispatch_privacy_banner_opt_in(app: &mut AppView) -> Vec<Effect> {
    if app.privacy_banner_opt_in_inflight || !app.privacy_banner_should_show() {
        return vec![];
    }
    let effects = set_coding_data_sharing(
        app,
        true,
        xai_grok_telemetry::events::CodingDataConsentSource::PrivacyBanner,
    );
    // should_show guarantees opted-out + unguarded, so effects is only empty
    // if a guard regresses; leaving inflight false keeps [Opt in] clickable.
    app.privacy_banner_opt_in_inflight = !effects.is_empty();
    effects
}

/// `[Opt out]`: ack locally, then record the decline.
///
/// The ack does NOT wait on the server, unlike `[Opt in]`'s: the user asked
/// for no change, so gating dismissal on a round trip would only re-ask a
/// question they answered.
///
/// The write is built here rather than through `set_coding_data_sharing`,
/// whose idempotent guard would skip it — the user is already opted out,
/// and recording that is the point. Its response re-anchors the mirror;
/// concurrent writes to this endpoint are still unordered.
pub(in crate::app::dispatch) fn dispatch_privacy_banner_opt_out(app: &mut AppView) -> Vec<Effect> {
    if app.privacy_banner_opt_in_inflight || !app.privacy_banner_should_show() {
        return vec![];
    }
    let previous_opted_in = !app.coding_data_retention_opt_out;
    log_coding_data_consent_selected(
        xai_grok_telemetry::events::CodingDataConsentSource::PrivacyBanner,
        false,
        previous_opted_in,
    );
    let mut effects = ack_privacy_banner(app);
    effects.push(Effect::SetCodingDataSharing {
        agent_id: coding_data_sharing_agent_id(app),
        opted_in: false,
        // Already opted out, so the revert is a no-op — and the generation
        // guard drops it entirely if the user has opted in since.
        rollback_to_opted_in: false,
        seq: next_coding_data_write_seq(app),
    });
    effects
}

pub(super) fn handle_context_info_complete(
    app: &mut AppView,
    agent_id: AgentId,
    info: Box<xai_grok_shell::session::SessionInfoResponse>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        let model = info.data.model.as_deref().unwrap_or("unknown").to_string();
        // Take ownership of the snapshot once, hand a clone to the
        // agent's running counters, then move the original into the
        // scrollback block (which keeps it for theme-reactive
        // re-rendering). This still costs one clone but reads as
        // "the agent needs a copy" rather than "the block needs a
        // copy", which matches the lifetime story.
        let snapshot = info.data.context;
        agent.apply_full_context_info(snapshot.clone());
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::context_info(
                snapshot, model,
            ));
    }
    vec![]
}

// Action handlers.

pub(super) fn dispatch_copy_session_id(app: &mut AppView, index: usize) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;
    // Try agent modal first, then fall back to app fields (welcome screen).
    let id = get_active_agent(app)
        .and_then(|agent| {
            if let Some(ActiveModal::SessionPicker {
                entries: Some(ref e),
                ..
            }) = agent.active_modal
            {
                e.get(index).map(|entry| entry.id.clone())
            } else {
                None
            }
        })
        .or_else(|| {
            app.session_picker_entries
                .as_ref()
                .and_then(|s| s.get(index))
                .map(|e| e.id.clone())
        });
    if let Some(id) = id {
        let delivery = crate::clipboard::copy_text_or_file(&id);
        app.show_toast(delivery.toast_message().as_ref());
    }
    vec![]
}

/// Open the onboarding tutorial overlay (top-level modal — works over both
/// the welcome screen and an agent session). Toggles: dispatching while
/// open closes instead of stacking.
pub(super) fn dispatch_open_tutorial(app: &mut AppView) -> Vec<Effect> {
    // Minimal mode has no modal host: the overlay would render nothing
    // while the app-level intercept swallowed all input.
    if app.screen_mode.is_minimal() {
        return vec![];
    }
    if app.tutorial.is_some() {
        app.tutorial = None;
        return vec![];
    }
    app.tutorial = Some(crate::views::tutorial::TutorialState::new());
    vec![]
}

pub(super) fn dispatch_show_release_notes(
    app: &mut AppView,
    title: String,
    content: String,
) -> Vec<Effect> {
    match app.active_view {
        ActiveView::Agent(id) => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.active_modal = Some(crate::views::modal::ActiveModal::DocViewer {
                    title,
                    content,
                    scroll: 0,
                    window: crate::views::modal_window::ModalWindowState::new(),
                    cached_lines: None,
                    previous_palette: None,
                    standalone: true,
                });
            }
        }
        ActiveView::Welcome => {
            app.welcome_doc_viewer = Some(crate::views::modal::ActiveModal::DocViewer {
                title,
                content,
                scroll: 0,
                window: crate::views::modal_window::ModalWindowState::new(),
                cached_lines: None,
                previous_palette: None,
                standalone: true,
            });
        }
        _ => {}
    }
    vec![]
}
