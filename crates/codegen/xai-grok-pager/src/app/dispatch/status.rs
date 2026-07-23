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

/// Show privacy and data retention status as a system message in scrollback.
///
/// Three-state display: Enterprise ZDR, coding data sharing opted out,
/// or opted in. Labels align with `CODING_DATA_SHARING_CHOICES` in
/// `settings/defs.rs` and the `coding_data_sharing_toast` format.
///
/// Also lists config knobs that `/privacy` does not change (technical
/// pointers only; no policy claims).
pub(super) fn dispatch_show_privacy_info(app: &mut AppView) -> Vec<Effect> {
    let mut lines = Vec::new();

    if app.is_zdr {
        // Enterprise ZDR -- the team has disabled retention entirely.
        lines.push("  Zero Data Retention: enabled");
        lines.push("  Your data is not retained or used for training (ZDR enabled).");
    } else if app.coding_data_retention_opt_out {
        // Coding data sharing opted out -- matches desktop's "Privacy mode" state.
        lines.push("  Privacy: privacy mode");
        lines.push("  Your code data will not be trained on or used to improve the product.");
        lines.push("");
        lines.push("  Use /privacy opt-in to share data and help improve the product.");
    } else {
        // Coding data sharing opted in -- matches desktop's "Share data" state.
        lines.push("  Privacy: share data");
        lines.push("  Usage and code data may be used by SpaceXAI to improve the product.");
        lines.push("");
        lines.push("  Use /privacy opt-out to enable privacy mode.");
    }

    // Config keys only; do not describe retention/training/analytics policy here.
    lines.push("");
    lines.push("  Other settings (not changed by /privacy):");
    lines.push("  - [features] telemetry / GROK_TELEMETRY_ENABLED");
    lines.push("  - [telemetry] trace_upload / GROK_TELEMETRY_TRACE_UPLOAD");
    lines.push("  - GROK_EXTERNAL_OTEL / OTEL_*");
    lines.push("");
    lines.push("  Learn more: https://x.ai/legal");
    let text = lines.join("\n");
    push_system_to_any_agent(app, &text);
    vec![]
}

/// State-only mutation for `coding_data_sharing`. SHELL-owned.
pub(super) fn set_coding_data_sharing_inner(app: &mut AppView, opted_in: bool) {
    app.coding_data_retention_opt_out = !opted_in;
}

/// Set coding-data-sharing preference. SHELL-owned, auth-metadata-backed
/// (persists via ACP ext-request, NOT `~/.grok/config.toml`).
pub(super) fn set_coding_data_sharing(app: &mut AppView, opted_in: bool) -> Vec<Effect> {
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
    // Synthetic AgentId(0) when no agents (welcome banner Accept).
    let agent_id = match app.active_view {
        crate::app::app_view::ActiveView::Agent(id) => id,
        _ => app
            .agents
            .keys()
            .next()
            .copied()
            .unwrap_or(crate::app::agent::AgentId(0)),
    };

    let prev = !app.coding_data_retention_opt_out;

    // ── Idempotent path: toast but skip the ACP round-trip. ──────────
    if prev == opted_in {
        app.show_toast(&coding_data_sharing_toast(opted_in));
        return vec![];
    }

    // ── Optimistic mutation: state, then UI feedback, then effect. ───
    set_coding_data_sharing_inner(app, opted_in);
    refresh_open_settings_modals(app);
    app.show_toast(&coding_data_sharing_toast(opted_in));

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
    }]
}

/// Format the `Coding data sharing` toast. Asymmetric: opt-in
/// (privacy-degrading) uses ⚠ + consequence text; opt-out (safe
/// default) uses ✓. Uses display names from the registry catalog.
pub(super) fn coding_data_sharing_toast(opted_in: bool) -> String {
    let display = display_for_coding_data_sharing_canonical(opted_in);
    if opted_in {
        // Privacy-degrading: warn glyph + spelled-out consequence.
        format!(
            "\u{26A0} Coding data sharing: {display} \u{2014} code samples may be retained \
             for training"
        )
    } else {
        // Safe default — uniform ✓ glyph.
        format!("\u{2713} Coding data sharing: {display}")
    }
}

/// Display string for the canonical bool. Keep aligned with
/// `CODING_DATA_SHARING_CHOICES` in `settings/defs.rs`.
fn display_for_coding_data_sharing_canonical(opted_in: bool) -> &'static str {
    if opted_in { "Opt in" } else { "Opt out" }
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

/// Push a system message to the active agent's scrollback, or to any available
/// agent if on the welcome screen.
fn push_system_to_any_agent(app: &mut AppView, msg: &str) {
    let block = crate::scrollback::block::RenderBlock::system(msg.to_string());
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        agent.scrollback.push_block(block);
        return;
    }
    if let Some(agent) = app.agents.values_mut().next() {
        agent.scrollback.push_block(block);
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
) -> Vec<Effect> {
    // Re-anchor mirror to server-confirmed value (defense-in-depth against
    // server reshaping the boolean). `agent_id` discarded — privacy is
    // app-level, not per-agent.
    set_coding_data_sharing_inner(app, opted_in);
    refresh_open_settings_modals(app);
    // Re-toast on confirmation. Without this, a slow ACP round-trip would
    // leave the user with only the optimistic toast (already faded) and no
    // server-confirmed feedback.
    app.show_toast(&coding_data_sharing_toast(opted_in));
    tracing::info!(
        target: "settings",
        key = "coding_data_sharing",
        ?agent_id,
        opted_in,
        "ACP update confirmed; mirror re-anchored",
    );
    let mut effects = vec![];
    // Ack only after successful opt-in from the privacy banner Accept path.
    if app.privacy_banner_accept_inflight {
        app.privacy_banner_accept_inflight = false;
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
) -> Vec<Effect> {
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
    // Accept failure: no ack; clear inflight so the banner stays.
    app.privacy_banner_accept_inflight = false;
    vec![]
}

/// Stamp `[privacy].privacy_banner_acked` (in-memory + disk).
pub(in crate::app::dispatch) fn ack_privacy_banner(app: &mut AppView) -> Vec<Effect> {
    let acked_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    app.privacy_banner_acked = Some(acked_at.clone());
    vec![Effect::PersistPrivacyBannerAcked { acked_at }]
}

/// Accept: opt-in via settings path; ack only after ACP success.
pub(in crate::app::dispatch) fn dispatch_privacy_banner_accept(app: &mut AppView) -> Vec<Effect> {
    if app.privacy_banner_accept_inflight || !app.privacy_banner_should_show() {
        return vec![];
    }
    let effects = set_coding_data_sharing(app, true);
    // should_show guarantees opted-out + unguarded, so effects is only empty
    // if a guard regresses; leaving inflight false keeps Accept clickable.
    app.privacy_banner_accept_inflight = !effects.is_empty();
    effects
}

/// Customize: ack, then open settings on coding_data_sharing
/// (creates/switches agent when opened from welcome).
pub(in crate::app::dispatch) fn dispatch_privacy_banner_customize(
    app: &mut AppView,
) -> Vec<Effect> {
    if app.privacy_banner_accept_inflight || !app.privacy_banner_should_show() {
        return vec![];
    }
    let mut effects = ack_privacy_banner(app);
    effects.extend(super::settings::ui::dispatch_open_settings(
        app,
        Some("coding_data_sharing"),
    ));
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
