//! Dashboard dispatchers: attach, overlays, rows, renames, and permissions.

use super::ctx::{show_welcome, surface_yolo_launch_block_notice};
use super::dashboard_telemetry::{
    log_dashboard_attached, log_dashboard_closed, log_dashboard_launched, log_dashboard_opened,
};
use super::modes::{dispatch_cycle_mode_and_sync, set_yolo_mode, yolo_enable_blocked};
use super::permissions::resolve_permission_queue_transition;
use super::queue::{maybe_drain_queue, note_peek_page_flip};
use super::router::dispatch;
use super::session::lifecycle::{
    dispatch_new_session_inner_with_id, dispatch_new_worktree_session,
};
use super::session::load::dispatch_load_session;
use super::session::load::focus_if_session_already_open;
use super::session::modal::dispatch_sessions_confirm_close;
use super::turn::dispatch_cancel_turn;
use super::voice::voice_stop_on_submit;
use crate::app::actions::{Action, Effect};
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView, DashboardReturn, TrustState};
use agent_client_protocol as acp;

// ---------------------------------------------------------------------------
// Agent Dashboard dispatchers
// ---------------------------------------------------------------------------

/// Build a `DashboardState` from the persisted layout (pins / reorder /
/// grouping), loading + caching `app.dashboard_persisted` on first use. Used
/// both to materialize the real dashboard and to compute a correct cycle order
/// before the dashboard has been opened.
fn dashboard_state_from_persisted(app: &mut AppView) -> crate::views::dashboard::DashboardState {
    use crate::views::dashboard::{DashboardState, load_persisted};
    if app.dashboard_persisted.is_none() {
        app.dashboard_persisted = load_persisted();
    }
    let persisted = app
        .dashboard_persisted
        .clone()
        .unwrap_or_else(crate::views::dashboard::PersistedDashboard::defaults);
    let resolver = crate::views::dashboard::SessionIdResolver::from_agents(&app.agents);
    DashboardState::from_persisted(&persisted, &resolver)
}

pub(super) fn ensure_dashboard_state(app: &mut AppView) {
    if app.dashboard.is_some() {
        return;
    }
    let mut state = dashboard_state_from_persisted(app);
    state.gc_stale_refs(&dashboard_alive_fn(&app.agents));
    state.adopt_slash_mru(app.slash_mru.clone());
    state.adopt_command_tags(app.command_tags.clone());
    state.set_screen_mode(app.screen_mode);
    state.set_recap_visible(app.session_recap_available);
    state.set_voice_visible(app.voice_mode_enabled);
    state.set_restricted_commands(&app.tier_restricted_commands);
    let billing = app.usage_visible;
    state
        .dispatch
        .slash_controller
        .set_billing_surface_visible(billing);
    state
        .peek_reply
        .slash_controller
        .set_billing_surface_visible(billing);
    app.dashboard = Some(state);
}

/// Configure the dashboard for display: snapshot app-wide state (cwd, models,
/// plugins, `default_yolo`) and reset per-session staging. Shared by
/// `dispatch_open_dashboard` and the overlay-cycle path; no-op if unallocated.
fn configure_dashboard_state(app: &mut AppView) {
    let bootstrap_commands = app.bootstrap_acp_commands.clone();
    let models = app.models.clone();
    let disable_plugins = app.appearance.disable_plugins;
    let default_yolo = app.default_yolo;
    let cwd = app.cwd.clone();
    let cwd_has_git_ancestor = app.cwd_has_git_ancestor;
    let has_agents = !app.agents.is_empty();
    if let Some(d) = app.dashboard.as_mut() {
        d.close_popup();
        d.location_picker = None;
        d.cwd = cwd.clone();
        d.cwd_has_git_ancestor = cwd_has_git_ancestor;
        d.dispatch_worktree = false;
        d.worktree_dialog = None;
        d.pending_worktree_prompt = None;
        d.pending_worktree_attach = false;
        d.focus_new_agent_button();
        d.list_focused = has_agents;
        d.dispatch.file_search.retarget(&cwd);
        // Tool gating disabled (None): the dashboard has no agent toolset.
        d.dispatch
            .slash_controller
            .registry_mut()
            .set_plugins_visible(!disable_plugins);
        d.dispatch
            .sync_acp_commands(&bootstrap_commands, None, &models);
        d.models = models;
        d.pending_model = None;
        d.pending_mode = if default_yolo {
            crate::views::dashboard::DashboardDispatchMode::AlwaysApprove
        } else {
            crate::views::dashboard::DashboardDispatchMode::Normal
        };
    }
}

/// Open the dashboard view. Respects the
/// [`crate::views::dashboard::dashboard_enabled`] feature flag (env var
/// override + persisted setting). The dashboard is independent of leader
/// mode: it renders local sessions from `app.agents` and, when connected
/// via a leader, additionally polls the leader roster (see the roster-poll
/// gate in the event loop).
pub(super) fn dispatch_open_dashboard(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::dashboard_enabled;

    if !dashboard_enabled() {
        app.show_toast("Agent dashboard is disabled in this configuration");
        return vec![];
    }
    // Gate behind auth. Until login completes, the
    // backend rejects new sessions; activating the dashboard view
    // visually dismisses the auth UI. Toast and stay put.
    if !matches!(app.auth_state, crate::app::app_view::AuthState::Done) {
        app.show_toast("Sign in to open the dashboard");
        return vec![];
    }
    // Same rationale for folder trust: opening the dashboard would visually
    // dismiss the trust question with the folder still unanswered. Toast and
    // stay put (mirrors the auth gate above) so the question is resolved first.
    if matches!(app.trust_state, TrustState::Pending { .. }) {
        app.show_toast("Answer the folder-trust question to open the dashboard");
        return vec![];
    }
    // Edge case 24: idempotent toggle — opening from the dashboard view
    // itself just closes.
    //
    // Ctrl+\ is now a single-shot toggle between the
    // agent view and the dashboard view. The previous design used
    // Ctrl+\ as a 3-state cascade (open dashboard → close popup →
    // exit dashboard) which fought the user's mental model of
    // "Ctrl+\ flips views". With auto-attach landing a popup on every
    // open, the close-popup step would have eaten the user's expected
    // exit press. Now Ctrl+\ always exits when already in the
    // dashboard; closing the popup-only stays bound to Esc inside the
    // popup mouse/key cascade.
    if matches!(app.active_view, ActiveView::AgentDashboard) {
        return dispatch_exit_dashboard(app);
    }
    // Stamp return target for this visit (clears any prior leftover).
    app.dashboard_return = match app.active_view {
        ActiveView::Agent(id) => Some(DashboardReturn::Agent(id)),
        _ => None,
    };
    // Preserve in-memory state across reopen.
    // `app.dashboard.is_some()` means we've previously initialised
    // it; preserve the user's filter / dispatch text / hover /
    // selection. Otherwise seed from persisted state.
    if app.dashboard.is_none() {
        ensure_dashboard_state(app);
    } else if let Some(d) = app.dashboard.as_mut() {
        // Subsequent reopen — just gc dead ids; in-memory state stays.
        d.gc_stale_refs(&dashboard_alive_fn(&app.agents));
        d.set_recap_visible(app.session_recap_available);
        d.set_voice_visible(app.voice_mode_enabled);
        d.set_restricted_commands(&app.tier_restricted_commands);
    }
    // Refresh each local agent's git context (branch / worktree / label)
    // from disk so the row subtitles show the LATEST branch and worktree
    // name, not a value that may have gone stale since the last
    // `git_head_changed` notification. Runs on open (a deliberate user
    // action), never per frame; `compute_cwd_git_info` returns `None` for
    // non-repo cwds so existing values are left intact rather than cleared.
    let agent_cwds: Vec<(AgentId, std::path::PathBuf)> = app
        .agents
        .iter()
        .map(|(id, a)| (*id, a.session.cwd.clone()))
        .collect();
    for (id, cwd) in agent_cwds {
        if let Some(info) = crate::git_info::compute_cwd_git_info(&cwd)
            && let Some(agent) = app.agents.get_mut(&id)
        {
            agent.current_branch = info.branch;
            agent.is_worktree = info.is_worktree;
            agent.main_repo = info.main_repo;
            agent.worktree_label = info.worktree_label;
        }
    }
    // The previous "auto-attach popup overlay" path
    // showed BOTH the dashboard (as a top banner) AND the focused
    // agent (as a bottom popup) on every `/dashboard` open. The
    // stacked layout was confusing — the user couldn't tell which
    // view owned the prompt, and the popup's keybindings (Enter
    // to send, etc.) had subtle input-routing bugs. Dashboard
    // open now shows ONLY the dashboard; pressing Enter on a row
    // switches the whole view to the agent's fullscreen view
    // (handled in `dispatch_dashboard_attach`).
    //
    // Always open in NEW-SESSION mode: focus the `[+ New Agent]` button
    // (no row selected) so typing a prompt + Enter dispatches a brand
    // new agent. Reply mode is opt-in — the user navigates (↑/↓ or j/k)
    // or clicks a row to select it, which arms "reply to that agent".
    //
    // Previously the dashboard pre-seeded `selected` to the agent the
    // user came from, which silently armed reply mode: a prompt typed
    // right after opening went to the old agent instead of spawning a
    // new one, AND the reply path never clears `selected`, so EVERY
    // subsequent dispatch kept replying to the same agent — the user
    // got "stuck to the same agent" and couldn't quickly dispatch new
    // sessions. New-session is the dashboard's primary gesture, so it is
    // the default; reply stays one explicit selection away.
    //
    configure_dashboard_state(app);
    app.active_view = ActiveView::AgentDashboard;
    log_dashboard_opened(app);
    app.dashboard_sessions_loading = true;
    if app.leader_mode {
        return vec![Effect::FetchRoster];
    }
    vec![Effect::FetchDashboardSessions]
}

/// Helper: produce a closure that answers "does this DashboardRowId
/// still exist in `agents`?". Static lifetime not possible (closures
/// borrow), so callers pass `&app.agents`.
fn dashboard_alive_fn(
    agents: &indexmap::IndexMap<AgentId, AgentView>,
) -> impl Fn(&crate::views::dashboard::DashboardRowId) -> bool + '_ {
    move |id| match id {
        crate::views::dashboard::DashboardRowId::TopLevel(a) => agents.contains_key(a),
        crate::views::dashboard::DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => agents
            .get(parent)
            .is_some_and(|a| a.subagent_sessions.contains_key(child_session_id)),
        // Roster-only rows are not locally hosted; they aren't tracked by
        // `agents` and are never persisted, so treat them as not alive for
        // pinned/reorder GC purposes.
        crate::views::dashboard::DashboardRowId::Roster { .. } => false,
    }
}

pub(super) fn dispatch_exit_dashboard(app: &mut AppView) -> Vec<Effect> {
    // Also clear any popup attachment so a fresh
    // reopen lands on the row list, not on a stale popup
    // (`close_popup()` atomically clears the hit
    // rects too.)
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        d.close_popup();
    }
    log_dashboard_closed(app);
    let preferred = app
        .dashboard_return
        .take()
        .filter(|t| app.agents.contains_key(&t.agent_id()));
    // Overlay chrome only when the preferred target is still alive — never
    // on the insertion-order fallback after the return agent was closed.
    let (return_id, rearm_overlay) = match preferred {
        Some(t) => (Some(t.agent_id()), t.is_overlay()),
        None => (app.agents.keys().next().copied(), false),
    };
    if let Some(id) = return_id {
        app.active_view = ActiveView::Agent(id);
        if rearm_overlay {
            rearm_session_overlay(app, id);
        }
        surface_yolo_launch_block_notice(app, id);
    } else {
        show_welcome(app);
    }
    vec![]
}

/// Restore session-overlay chrome (`attached_agent` + row cursor).
/// Keeps a live subagent takeover; otherwise clears it and selects TopLevel.
fn rearm_session_overlay(app: &mut AppView, id: AgentId) {
    use crate::views::dashboard::DashboardRowId;
    let live_child = app.agents.get(&id).and_then(|a| {
        a.active_subagent
            .as_ref()
            .filter(|c| a.subagent_sessions.contains_key(*c))
            .cloned()
    });
    let row = match live_child {
        Some(child_session_id) => DashboardRowId::Subagent {
            parent: id,
            child_session_id,
        },
        None => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.active_subagent = None;
            }
            DashboardRowId::TopLevel(id)
        }
    };
    if let Some(d) = app.dashboard.as_mut() {
        d.focus_row(row);
        d.attached_agent = Some(id);
    }
}

pub(super) fn dispatch_dashboard_attach(
    app: &mut AppView,
    id: crate::views::dashboard::DashboardRowId,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;
    // Attach is a fullscreen view switch AND signals
    // "session-overlay mode" via `attached_agent`. The agent view
    // takes the full screen and the renderer wraps it in a bordered
    // frame with `[Prev] [Next] [✗]` affordances at the top right
    // (mirrors the subagent fullscreen takeover). Input goes
    // straight to the agent because `active_view = Agent(id)`, so
    // Enter/Shift+Tab/etc. all work as in any regular agent view.
    //
    // Attaching re-targets the overlay — an overlay stop-confirm armed
    // on a previously attached agent (legacy popup row-click path
    // reaches here without a key press) must not follow the user in.
    clear_pending_overlay_stop(app);
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
    }
    match id {
        DashboardRowId::TopLevel(agent_id) => {
            if !app.agents.contains_key(&agent_id) {
                if let Some(d) = app.dashboard.as_mut() {
                    d.set_error_toast("Session no longer exists");
                }
                return vec![];
            }
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                // Drop any prior subagent takeover so the agent
                // view paints the parent, not whatever subagent
                // the user last opened fullscreen.
                agent.active_subagent = None;
            }
            if let Some(d) = app.dashboard.as_mut() {
                // `focus_row` (not a bare `selected` assignment) so the
                // section cursor / button focus are cleared — exactly one
                // cursor target stays active.
                d.focus_row(DashboardRowId::TopLevel(agent_id));
                // Signal session-overlay mode: render the agent
                // wrapped in the bordered frame with cycle/close
                // affordances at the top right.
                d.attached_agent = Some(agent_id);
            }
            app.active_view = ActiveView::Agent(agent_id);
            log_dashboard_attached(&DashboardRowId::TopLevel(agent_id));
            surface_yolo_launch_block_notice(app, agent_id);
        }
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => {
            // If the subagent has finished/disappeared
            // between row build and attach, toast and stay put rather
            // than silently switching to the parent.
            let alive = app
                .agents
                .get(&parent)
                .is_some_and(|a| a.subagent_sessions.contains_key(&child_session_id));
            if !alive {
                if let Some(d) = app.dashboard.as_mut() {
                    d.set_error_toast("Subagent no longer running");
                }
                return vec![];
            }
            if let Some(agent) = app.agents.get_mut(&parent) {
                agent.open_subagent_fullscreen(child_session_id.clone());
            }
            let row_id = DashboardRowId::Subagent {
                parent,
                child_session_id,
            };
            if let Some(d) = app.dashboard.as_mut() {
                d.focus_row(row_id.clone());
                d.attached_agent = Some(parent);
            }
            app.active_view = ActiveView::Agent(parent);
            log_dashboard_attached(&row_id);
            surface_yolo_launch_block_notice(app, parent);
        }
        DashboardRowId::Roster { session_id } => {
            // A roster row is a leader-hosted session this client is not
            // locally attached to. Attaching issues an ACP `session/load`,
            // which the leader join-not-steal-subscribes: it adds this
            // client to the session's subscriber set (replay + live
            // broadcast) instead of stealing ownership. Mirror the session
            // picker's strict resume path — inserting a
            // leader-hosted, possibly cross-cwd session into THIS cwd's
            // recent list would be wrong.

            // Resolve cwd + origin first (also drives kind for focus-if-open).
            let (session_cwd, conversation_entry) = app
                .leader_roster
                .iter()
                .chain(app.dashboard_local_sessions.iter())
                .find(|e| e.session_id == session_id)
                .map(|e| {
                    let is_conversation = e.origin.kind == "conversation";
                    (
                        // Conversation rows have no cwd to re-home into.
                        (!is_conversation).then(|| std::path::PathBuf::from(&e.cwd)),
                        is_conversation,
                    )
                })
                .unwrap_or((None, false));

            // Already local (e.g. double-click after the row converted): focus only.
            if let Some(existing_id) =
                focus_if_session_already_open(app, session_id.as_str(), conversation_entry)
            {
                log_dashboard_attached(&DashboardRowId::TopLevel(existing_id));
                return vec![];
            }

            // Mirror the picker resume path: allocate a new local agent and
            // emit `Effect::LoadSession` (strict load). `dispatch_load_session`
            // already sets `app.active_view` to the new agent. Conversation-
            // origin roster rows carry the conversation-entry bit so they take
            // the direct chat load, never local/GCS resolution.
            let effects = dispatch_load_session(app, session_id, session_cwd, conversation_entry);
            if let Some(new_id) = effects.iter().find_map(|e| match e {
                Effect::LoadSession { agent_id, .. } => Some(*agent_id),
                _ => None,
            }) {
                if let Some(d) = app.dashboard.as_mut() {
                    d.focus_row(DashboardRowId::TopLevel(new_id));
                    d.attached_agent = Some(new_id);
                }
                log_dashboard_attached(&DashboardRowId::TopLevel(new_id));
            }
            return effects;
        }
    }
    vec![]
}

/// Exit the dashboard's session-overlay: dismiss the bordered
/// chrome and return to the dashboard view. Mirrors the popup
/// `[✗]` close from the older design but applied to the new
/// fullscreen-with-frame layout.
pub(super) fn dispatch_dashboard_overlay_exit(app: &mut AppView) -> Vec<Effect> {
    // Capture before close_popup() clears attached_agent.
    if let ActiveView::Agent(id) = app.active_view {
        app.dashboard_return = Some(DashboardReturn::Overlay(id));
    }
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        d.close_popup();
    }
    // Leaving the overlay by mouse (`[Dashboard]` click) doesn't pass
    // through the key-press disarm in `handle_input`, so an armed
    // overlay stop-confirm would otherwise survive the exit and let a
    // later Ctrl+X on the dashboard close a session without the
    // two-press ritual. The confirm is bound to "this overlay, this
    // agent" — clear it on the way out.
    clear_pending_overlay_stop(app);
    app.active_view = ActiveView::AgentDashboard;
    vec![]
}

/// Disarm a pending overlay stop-confirm (see
/// [`dispatch_dashboard_overlay_stop`]). Called from every overlay
/// navigation that can happen WITHOUT a key press (mouse clicks on
/// `[Dashboard]` / `[‹]` / `[›]`); key presses already disarm via the
/// pending-action fast path in `AppView::handle_input`.
fn clear_pending_overlay_stop(app: &mut AppView) {
    if app
        .pending_action
        .as_ref()
        .is_some_and(|p| matches!(p.action, Action::DashboardOverlayStop))
    {
        app.pending_action = None;
    }
}

/// Confirmed stop from inside the dashboard's session-overlay — the
/// second Ctrl+X press within the confirm window. Canonical state
/// machine for overlay Ctrl+X (the intercept in
/// `app_view::handle_input` and the `DashboardOverlayStop` def both
/// point here):
///
/// - First press, turn RUNNING → `Action::CancelTurn` (the agent
///   view's Ctrl+C behaviour: keep-subagents prompt, prompt rewind).
///   Never arms, so mashing Ctrl+X to stop a turn can't close the
///   session.
/// - First press, any other state (idle, command in flight, cancel
///   pending) → arms `AppView::pending_action` with the dashboard's
///   2s `STOP_CONFIRM_WINDOW`; the shortcuts bar paints "press Ctrl+x
///   again to close this session". Cancel can't help in the non-idle
///   variants of this arm — `dispatch_cancel_turn` no-ops unless a
///   turn is running, and command cancellation isn't implemented (see
///   `handle_agent_action`'s `CancelTurn` TODO) — so the two-press
///   close is the only termination the user can reach, matching the
///   dashboard list's Ctrl+X which arms even while busy.
/// - Second press inside the window lands here via the pending-action
///   fast path; any other key disarms via that same path. A turn that
///   STARTED inside the window (queued prompt drained, user sent one)
///   downgrades the confirmed press to a cancel instead of closing
///   work in flight.
///
/// Mirrors `dispatch_dashboard_stop`'s second press, except the user
/// is INSIDE the session being closed: the view returns to the
/// dashboard instead of falling back to another agent.
pub(super) fn dispatch_dashboard_overlay_stop(app: &mut AppView) -> Vec<Effect> {
    let Some(id) = app.dashboard.as_ref().and_then(|d| d.attached_agent) else {
        return vec![];
    };
    if app
        .agents
        .get(&id)
        .is_some_and(|a| a.session.state.is_turn_running())
    {
        return dispatch_cancel_turn(app);
    }
    // Land on the dashboard BEFORE closing: `dispatch_sessions_confirm_close`
    // only re-targets the active view when the closed agent IS the
    // active view, so switching first keeps it from picking a fallback
    // agent. This also means a refused close (e.g. "Cannot close the
    // only session") still returns to the dashboard — with the refusal
    // toast surfaced there via `AppView::show_toast`.
    // Pick the cursor's next home on the dashboard BEFORE the row
    // vanishes, so returning to the list lands the selection on the
    // neighbouring agent (down 1) instead of a stale cursor that
    // bounces the next ↑/↓ back to the top.
    let neighbor =
        dashboard_neighbor_row(app, &crate::views::dashboard::DashboardRowId::TopLevel(id));
    if let Some(d) = app.dashboard.as_mut() {
        d.close_popup();
    }
    app.active_view = ActiveView::AgentDashboard;
    let effects = dispatch_sessions_confirm_close(app, id);
    if !app.agents.contains_key(&id)
        && let Some(d) = app.dashboard.as_mut()
    {
        // Move the cursor onto the neighbouring row, or the always-present
        // `[+ New Agent]` button when none is left — both go through the
        // focus helpers so the "exactly one cursor active" invariant holds
        // (a bare `selected = None` would leave no cursor and drop the footer
        // into its defensive fallback).
        match neighbor {
            Some(n) => d.focus_row(n),
            None => d.focus_new_agent_button(),
        }
        // Success feedback — the close path itself is silent. Mirrors the
        // "Session closed" toast the render path shows when an attached
        // agent disappears externally (`app_view.rs`, AgentDashboard arm).
        // Don't clobber a refusal toast planted by the close path above.
        if d.error_toast.is_none() {
            d.error_toast = Some(format!("{} Session closed", crate::glyphs::check_mark()));
        }
    }
    effects
}

/// Toggle worktree-dispatch mode for the dashboard (bound to Ctrl+W).
///
/// When armed, the next agent dispatched from the dashboard spawns in a fresh
/// git worktree and the `[+ New Agent]` button reads `[+ New Worktree]`.
/// Worktrees require a git repo, so outside one the toggle is a no-op and we
/// surface an explanatory toast instead of silently doing nothing (and the
/// dashboard is never left in worktree mode in a non-git directory).
pub(super) fn dispatch_dashboard_toggle_worktree(app: &mut AppView) -> Vec<Effect> {
    let has_git = app.cwd_has_git_ancestor;
    if let Some(d) = app.dashboard.as_mut() {
        if has_git {
            d.dispatch_worktree = !d.dispatch_worktree;
        } else {
            d.dispatch_worktree = false;
            d.set_error_toast("Not a git repository — worktrees need one");
        }
    }
    vec![]
}

/// Toggle auto-approve (YOLO mode) on the selected dashboard
/// row's owning agent. Subagents inherit their parent's mode, so
/// a subagent selection routes to the parent.
///
/// Reuses `set_yolo_mode` (which reads `active_view` to target
/// the agent) by temporarily switching the active view to the
/// selected agent for the duration of the call — keeps all the
/// existing drain / persist / toast logic in a single code path
/// instead of duplicating it.
pub(super) fn dispatch_dashboard_toggle_auto_approve(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let Some(d) = app.dashboard.as_ref() else {
        return vec![];
    };
    let Some(selected) = d.selected.as_ref() else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Select a session first");
        }
        return vec![];
    };
    let agent_id = match selected {
        DashboardRowId::TopLevel(id) => *id,
        DashboardRowId::Subagent { parent, .. } => *parent,
        DashboardRowId::Roster { .. } => return vec![],
    };
    if !app.agents.contains_key(&agent_id) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Session no longer exists");
        }
        return vec![];
    }
    let agent = app.agents.get(&agent_id).expect("checked above");
    let new = !agent.session.yolo_mode;

    // Managed policy pins always-approve off — toast on the dashboard's own
    // error slot (the inner gate's agent toast is invisible from here).
    if let Some(warning) = yolo_enable_blocked(app, new) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast(warning);
        }
        return vec![];
    }

    // Temporarily borrow active_view so `set_yolo_mode` targets
    // the dashboard's selected agent rather than whichever view
    // is currently active. Restored before returning.
    let saved_view = app.active_view;
    app.active_view = ActiveView::Agent(agent_id);
    let effects = set_yolo_mode(app, new);
    app.active_view = saved_view;
    effects
}

fn snapshot_prompt_widget(
    prompt: &mut crate::views::prompt_widget::PromptWidget,
    text: String,
) -> crate::views::prompt_widget::StashedPrompt {
    if prompt.text() == text
        || !prompt.images.is_empty()
        || !prompt.textarea().elements().is_empty()
    {
        prompt.stash().with_transformed_text(text)
    } else {
        crate::views::prompt_widget::StashedPrompt::from_submission(text, Vec::new(), Vec::new())
    }
}

/// Open the worktree-label dialog and stash the dispatch prompt until confirm.
fn open_dashboard_worktree_dialog(
    app: &mut AppView,
    prompt: Option<String>,
    attach: bool,
) -> Vec<Effect> {
    if let Some(d) = app.dashboard.as_mut() {
        d.pending_worktree_prompt =
            prompt.map(|text| snapshot_prompt_widget(&mut d.dispatch, text));
        d.pending_worktree_attach = attach;
        d.worktree_dialog = Some(crate::app::app_view::NewWorktreeDialogState::new());
        d.dispatch.set_text("");
        d.error_toast = None;
    }
    vec![]
}

/// Create a new session AND switch into its detail view. Routed from the
/// `[+ New Agent]` button and Enter-on-empty-prompt while it's focused.
/// Mirrors `dispatch_dashboard_dispatch`'s new-session arm with `attach=true`,
/// minus the prompt enqueue.
pub(super) fn dispatch_dashboard_create_new_agent_with_detail(app: &mut AppView) -> Vec<Effect> {
    // Creating/switching consumes the dispatch surface — stop voice and drop the
    // target so a late final can't refill the box after the view switch.
    voice_stop_on_submit(app);
    // Worktree mode armed + git repo: open the label dialog (which spawns the
    // agent in a fresh worktree on confirm) instead of a plain session. The
    // button opens the detail view, so confirm attaches (`attach = true`).
    if app.cwd_has_git_ancestor && app.dashboard.as_ref().is_some_and(|d| d.dispatch_worktree) {
        return open_dashboard_worktree_dialog(app, None, /* attach */ true);
    }
    let pending_model = app.dashboard.as_ref().and_then(|d| d.pending_model.clone());
    let pending_mode = app
        .dashboard
        .as_ref()
        .map(|d| d.pending_mode)
        .unwrap_or_default();
    let model_id = pending_model.as_ref().map(|m| m.id.clone());
    log_dashboard_launched("new_agent_button");
    let (new_id, effects) = dispatch_new_session_inner_with_id(app, model_id);
    let policy_block = app.yolo_policy_block;
    if let Some(agent) = app.agents.get_mut(&new_id) {
        apply_pending_dispatch_config(agent, pending_model.as_ref(), pending_mode, policy_block);
    }
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        // Clear the dispatch input even though we don't enqueue
        // anything — a stray paste while the button is focused
        // (no typed Enter) shouldn't survive the view switch.
        d.dispatch.set_text("");
        d.error_toast = None;
        d.filter = crate::views::dashboard::Filter::None;
        // Snap the cursor onto the new row so the overlay's `i/n`
        // indicator matches the active view, AND so the chrome's
        // `[‹]` / `[›]` cycle anchors on the right starting point.
        d.focus_row(crate::views::dashboard::DashboardRowId::TopLevel(new_id));
        d.attached_agent = Some(new_id);
    }
    app.active_view = ActiveView::Agent(new_id);
    surface_yolo_launch_block_notice(app, new_id);
    effects
}

/// Open the dashboard's shortcuts cheatsheet modal.
///
/// Builds the entry list from the registry, scoped to the `DashboardFocused`
/// + `Always` contexts. Mirrors `ActionId::ShortcutsHelp`'s agent-view handler.
pub(super) fn dispatch_dashboard_open_shortcuts_help(app: &mut AppView) {
    let Some(d) = app.dashboard.as_mut() else {
        return;
    };
    if d.shortcuts_modal.is_some() {
        // Idempotent — re-pressing Ctrl+. while the modal is
        // already open is a no-op (rather than blowing away the
        // user's search query / scroll position with a fresh
        // build).
        return;
    }
    use crate::actions::When;
    let contexts = [When::DashboardFocused, When::Always];
    let entries = crate::views::shortcuts_help::build_entries(
        &contexts,
        &app.registry,
        /* vim_mode */ false,
    );
    let state = crate::views::shortcuts_help::build_initial_picker_state(&entries);
    d.shortcuts_modal = Some(Box::new(crate::views::dashboard::ShortcutsModalState {
        entries,
        state,
        window: Default::default(),
        filter_active: false,
        collapsed_sections: crate::views::shortcuts_help::default_collapsed(),
        expanded_ids: std::collections::HashSet::new(),
        mode: crate::views::shortcuts_help::ShortcutsHelpMode::Browse,
    }));
}

/// Short display label for a directory in the location picker — the
/// basename (truncated), or `~` for the home directory itself.
fn location_picker_label(path: &std::path::Path) -> String {
    if dirs::home_dir().is_some_and(|h| h == path) {
        return "~".to_string();
    }
    let raw = path.file_name().and_then(|n| n.to_str()).unwrap_or("/");
    crate::render::line_utils::truncate_str(raw, 30)
}

/// Resolve a raw location-picker / `/cd` path string to an absolute path,
/// expanding a leading `~` and joining relative paths against `cwd`.
/// Returns `None` for empty input or when `~` can't be expanded. The
/// caller is responsible for validating the result is a directory.
pub(super) fn resolve_location_input(
    input: &str,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded: std::path::PathBuf = if trimmed == "~" {
        dirs::home_dir()?
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        std::path::PathBuf::from(trimmed)
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        Some(cwd.join(expanded))
    }
}

/// Open the dashboard's location picker. Seeds the candidate list with
/// the current cwd (marked `(current)`) followed by recent project
/// directories from session history. Idempotent — a no-op if the picker
/// is already open or the dashboard isn't active.
pub(super) fn dispatch_dashboard_open_location_picker(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::{LocationCandidate, LocationPickerState};

    if !matches!(app.active_view, ActiveView::AgentDashboard) {
        // `/cd` reached from a non-dashboard surface — the location
        // picker is a dashboard affordance, so guide the user there.
        // Gate on the dashboard being the *foreground* view (not merely
        // `app.dashboard.is_some()`, which stays true for the rest of the
        // session once the dashboard has been opened even once).
        app.show_toast("Open the dashboard (/dashboard) to change location");
        return vec![];
    }
    // Idempotent — re-triggering while open keeps the current query.
    if app
        .dashboard
        .as_ref()
        .is_some_and(|d| d.location_picker.is_some())
    {
        return vec![];
    }

    let cwd = app.cwd.clone();
    // Same pattern as `open_project_question` — the recent-dirs source is
    // async; block the current runtime thread briefly to collect it.
    let recent = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(crate::project_picker::sources::collect_recent_dirs(10))
    });

    // Worktree label index (root path → label), built once and reused to
    // tag both recents and live directory suggestions.
    let worktrees = crate::git_info::worktree_label_index();
    let worktree_label = |path: &std::path::Path| -> Option<String> {
        let key = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        worktrees.get(&key).cloned()
    };

    let mut candidates: Vec<LocationCandidate> = Vec::new();
    candidates.push(LocationCandidate {
        label: location_picker_label(&cwd),
        detail: format!(
            "{}  (current)",
            crate::project_picker::sources::display_path(&cwd)
        ),
        worktree: worktree_label(&cwd),
        path: cwd.clone(),
    });
    for (path, ts) in recent.into_iter().filter(|(p, _)| p != &cwd) {
        let detail = format!(
            "{}  ({})",
            crate::project_picker::sources::display_path(&path),
            crate::views::session_title::format_relative_time(
                (chrono::Utc::now() - ts).to_std().unwrap_or_default()
            ),
        );
        candidates.push(LocationCandidate {
            label: location_picker_label(&path),
            detail,
            worktree: worktree_label(&path),
            path,
        });
    }

    if let Some(d) = app.dashboard.as_mut() {
        let mut lp = LocationPickerState::new(candidates, cwd, worktrees);
        // Reflect the dashboard's current worktree arming so reopening the
        // picker shows the toggle in its existing state.
        lp.worktree_mode = d.dispatch_worktree;
        d.location_picker = Some(lp);
    }
    crate::unified_log::info("dashboard.location_picker.opened", None, None);
    vec![]
}

/// Apply a location-picker / `/cd` selection. Resolves + validates the
/// path, and on success updates `app.cwd` + the process cwd (so newly
/// dispatched dashboard sessions spawn there) and closes the modal. On
/// failure the modal stays open with an inline error and the cwd is
/// unchanged.
pub(super) fn dispatch_dashboard_change_location(app: &mut AppView, input: String) -> Vec<Effect> {
    // Gate on the dashboard being the *foreground* view. `/cd <path>` typed
    // from another surface (welcome / an agent session / the dashboard
    // overlay) would otherwise silently change the process cwd, since
    // `app.dashboard` stays `Some` for the rest of the session once opened.
    if !matches!(app.active_view, ActiveView::AgentDashboard) {
        app.show_toast("Open the dashboard (/dashboard) to change location");
        return vec![];
    }
    let path = match resolve_location_input(&input, &app.cwd).filter(|p| p.is_dir()) {
        Some(p) => p,
        None => {
            if let Some(lp) = app
                .dashboard
                .as_mut()
                .and_then(|d| d.location_picker.as_mut())
            {
                lp.error = Some(format!("Not a directory: {}", input.trim()));
            } else if let Some(d) = app.dashboard.as_mut() {
                // `/cd <bad path>` typed into the dispatch box (no picker
                // open) — surface the error as a dashboard toast.
                d.set_error_toast(&format!("Not a directory: {}", input.trim()));
            }
            return vec![];
        }
    };

    crate::unified_log::info(
        "dashboard.location_picker.changed",
        None,
        Some(serde_json::json!({ "path": path.display().to_string() })),
    );

    let changed = app.cwd != path;
    let display = crate::project_picker::sources::display_path(&path);
    app.cwd = path.clone();
    // Keep the git-repo flag in sync with the new cwd (it's otherwise only
    // computed at startup). Worktree dispatch reads it.
    app.cwd_has_git_ancestor = path.ancestors().any(|p| p.join(".git").exists());
    // Warm the per-cwd git cache the header / top bar read (keyed on the
    // new cwd) so the new branch + worktree label show on the next frame
    // instead of waiting for the first lazy refresh (no git spawn on the
    // render path).
    crate::git_info::populate_from_cwd_async(path.clone());

    let has_git = app.cwd_has_git_ancestor;
    if let Some(d) = app.dashboard.as_mut() {
        // Keep the dashboard's cwd + git-repo snapshot in sync with the new
        // cwd so the header tracks it immediately (the process cwd changes
        // later, via `Effect::SetWorkingDir`).
        d.cwd = path.clone();
        d.cwd_has_git_ancestor = has_git;
        // Carry the picker's worktree toggle onto the dashboard so the next
        // dispatched agent honors it after the modal closes — but only when
        // the destination is a git repo. Worktrees require one, so navigating
        // to a non-git directory forces worktree mode off (the dashboard is
        // never in worktree mode outside a repo).
        if let Some(wt) = d.location_picker.as_ref().map(|lp| lp.worktree_mode) {
            d.dispatch_worktree = wt && has_git;
        } else if !has_git {
            d.dispatch_worktree = false;
        }
        d.location_picker = None;
        if changed {
            // Re-root the dispatch box's `@` file-context picker at the new
            // cwd so completions walk the same tree that newly dispatched
            // sessions run in (both keyed off `app.cwd`). Cheap: the walk
            // only spawns once the user actually types `@`.
            d.dispatch.file_search.retarget(&path);
            d.error_toast = Some(format!("\u{2192} {display}"));
        }
    }

    // Change the process cwd via an effect, not inline, so the reducer
    // stays side-effect free and parallel tests don't leak the process
    // cwd into each other.
    vec![Effect::SetWorkingDir { path }]
}

/// Confirm the dashboard worktree-label dialog: create the next dashboard
/// agent in a fresh worktree at `app.cwd`, replaying any prompt stashed when
/// the dialog opened. The dialog itself was already cleared by the input
/// handler. Surfaces a dashboard toast (instead of creating) when the cwd
/// isn't a git repository.
pub(super) fn dispatch_dashboard_confirm_worktree(
    app: &mut AppView,
    label: Option<String>,
) -> Vec<Effect> {
    // Apply the prompt, attach choice, and staged model/mode together.
    // The worktree path must honor all of these exactly like the normal
    // dispatch path (`dispatch_dashboard_dispatch`).
    let (mut prompt, attach) = match app.dashboard.as_mut() {
        Some(d) => (
            d.pending_worktree_prompt.take(),
            std::mem::replace(&mut d.pending_worktree_attach, false),
        ),
        None => (None, false),
    };
    let pending_model = app.dashboard.as_ref().and_then(|d| d.pending_model.clone());
    let pending_mode = app
        .dashboard
        .as_ref()
        .map(|d| d.pending_mode)
        .unwrap_or_default();
    if !app.cwd_has_git_ancestor {
        if let Some(d) = app.dashboard.as_mut() {
            // Restore the typed prompt if the cwd stopped being a repo.
            if let Some(p) = prompt {
                d.dispatch.restore(p);
            }
            d.set_error_toast("Not a git repository — can't create a worktree here");
        }
        return vec![];
    }
    let (prompt_text, mut images, chip_elements) = if let Some(stashed) = prompt.take() {
        let (text, images, chip_elements) = stashed.into_submission();
        (Some(text), images, chip_elements)
    } else {
        (None, Vec::new(), Vec::new())
    };
    let model_id = pending_model.as_ref().map(|m| m.id.clone());
    let effects =
        dispatch_new_worktree_session(app, None, label, prompt_text, model_id, None, None);
    if let Some(new_id) = effects.iter().find_map(|e| match e {
        Effect::CreateWorktreeSession { agent_id, .. } => Some(*agent_id),
        _ => None,
    }) {
        // Apply the staged effort / plan / always-approve mode to the new
        // worktree agent (base model is seeded via the effect's `model_id`),
        // then carry any pasted images onto the replayed prompt — both mirror
        // `dispatch_dashboard_dispatch`.
        let policy_block = app.yolo_policy_block;
        if let Some(agent) = app.agents.get_mut(&new_id) {
            apply_pending_dispatch_config(
                agent,
                pending_model.as_ref(),
                pending_mode,
                policy_block,
            );
            if let Some(entry) = agent.session.pending_prompts.back_mut() {
                entry.images = std::mem::take(&mut images);
                entry.chip_elements = chip_elements;
            }
        }
        if attach {
            // Send+open: attach the dashboard's detail-view overlay onto the
            // agent (`dispatch_new_worktree_session` already set `active_view`).
            if let Some(d) = app.dashboard.as_mut() {
                d.restore_peek_viewport(&mut app.agents);
                d.focus_row(crate::views::dashboard::DashboardRowId::TopLevel(new_id));
                d.attached_agent = Some(new_id);
            }
        } else {
            // Plain Enter: undo the view switch `dispatch_new_worktree_session`
            // made and stay on the dashboard.
            app.active_view = ActiveView::AgentDashboard;
        }
    }
    crate::prompt_images::drain_and_cleanup(&mut images);
    effects
}

/// Cycle the dashboard overlay to the prev (-1) / next (+1) agent in the
/// visible row order, wrapping at the ends. Attaches overlay chrome on the
/// first cycle from a session not opened via the dashboard.
pub(super) fn dispatch_dashboard_overlay_cycle(app: &mut AppView, delta: i32) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    // Anchor on the agent on screen, not a possibly-stale `attached_agent`:
    // keeps cycling correct after a session switch the view without
    // re-attaching. (Cycling is only ever invoked from an agent view.)
    let ActiveView::Agent(current) = app.active_view else {
        return vec![];
    };
    if app.agents.len() <= 1 {
        return vec![];
    }
    // Compute the order without permanently materializing the dashboard, so a
    // no-op cycle (filter hides all / current row hidden) leaves no state. When
    // the dashboard was never opened, build a throwaway state from the persisted
    // layout (pins / reorder / grouping) so prev/next match what the user sees
    // after opening — `load_persisted` is cached on `app.dashboard_persisted`.
    let order = match app.dashboard.as_ref() {
        Some(d) => crate::views::dashboard::overlay_cycle_order(d, &app.agents),
        None => {
            // Materializing here must honor the same gates as
            // `dispatch_open_dashboard` (feature flag AND auth), so cycling
            // can't surface an ungated dashboard on back-out.
            if !crate::views::dashboard::dashboard_enabled()
                || !matches!(app.auth_state, crate::app::app_view::AuthState::Done)
            {
                return vec![];
            }
            let transient = dashboard_state_from_persisted(app);
            crate::views::dashboard::overlay_cycle_order(&transient, &app.agents)
        }
    };
    if order.len() <= 1 {
        return vec![];
    }
    let Some(idx) = order.iter().position(|id| *id == current) else {
        return vec![];
    };
    let n = order.len() as i32;
    let next_idx = (((idx as i32) + delta).rem_euclid(n)) as usize;
    let next_id = order[next_idx];
    if next_id == current {
        return vec![];
    }
    // Materialize + configure only on a real switch — otherwise a
    // cycle-created dashboard renders bare on back-out (default cwd, empty
    // `/model`, wrong auto-approve).
    if app.dashboard.is_none() {
        ensure_dashboard_state(app);
        configure_dashboard_state(app);
    }
    // Drop any prior subagent takeover on the next agent so the
    // overlay paints the parent view, mirroring the attach path.
    if let Some(agent) = app.agents.get_mut(&next_id) {
        agent.active_subagent = None;
    }
    // A stop-confirm armed on the CURRENT agent must not carry over to
    // the next one — a mouse click on `[‹]` / `[›]` lands here without
    // the key-press disarm ever running.
    clear_pending_overlay_stop(app);
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        d.attached_agent = Some(next_id);
        d.focus_row(DashboardRowId::TopLevel(next_id));
    }
    app.active_view = ActiveView::Agent(next_id);
    surface_yolo_launch_block_notice(app, next_id);
    vec![]
}

pub(super) fn dispatch_dashboard_dispatch(
    app: &mut AppView,
    text: String,
    attach: bool,
) -> Vec<Effect> {
    // Enter is a submit attempt — stop voice and drop the target up front (as the
    // agent path does), so even a rejected send (empty / over-cap) can't leave a
    // hot mic or let a late final refill the box.
    voice_stop_on_submit(app);
    // Paste-then-immediate-send: a Cmd+V image probe is still off-thread. Stash
    // this send and re-issue it once the probe completes so the image is never
    // dropped from the dispatched prompt's content blocks.
    if let Some(d) = app.dashboard.as_mut()
        && d.paste_probe_in_flight > 0
    {
        d.deferred_dispatch_send =
            Some(crate::views::dashboard::state::DeferredDispatchSend { attach });
        return vec![];
    }
    let trimmed = text.trim().to_string();
    // Reject only an empty / whitespace-only prompt; any non-empty
    // input (even a single character) dispatches a new session. The
    // keyboard path already filters empty input upstream (see
    // `DashboardState::handle_key`), so this guard mainly protects the
    // slash-fallback callers.
    if trimmed.is_empty() {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Type a prompt to dispatch a session");
        }
        return vec![];
    }
    // Cap dispatch text at 64 KiB so a runaway paste
    // doesn't queue a multi-megabyte prompt.
    //
    // Toast shows BOTH the character count (what the
    // user thinks in) AND the byte budget (the actual limit) so the
    // char/byte unit mismatch isn't surprising.
    const MAX_DISPATCH_BYTES: usize = 64 * 1024;
    if text.len() > MAX_DISPATCH_BYTES {
        let chars = text.chars().count();
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast(&format!(
                "Prompt too long ({chars} chars / {} bytes; max ~64 KiB)",
                text.len()
            ));
        }
        return vec![];
    }

    // Worktree mode armed (via the location picker) AND the cwd is a git
    // repo: stash the prompt and open the worktree-label dialog. Confirming
    // it spawns the agent in a fresh worktree and replays this prompt (see
    // `Action::DashboardConfirmWorktree`). The `attach` flag rides along so
    // confirm honors the same detail-vs-dashboard choice as a normal dispatch
    // (bare Enter stays on the dashboard; Ctrl+S opens the detail view).
    // Outside a repo we fall through to a normal session dispatch (the toggle
    // is hidden there anyway).
    if app.cwd_has_git_ancestor && app.dashboard.as_ref().is_some_and(|d| d.dispatch_worktree) {
        return open_dashboard_worktree_dialog(app, Some(text), attach);
    }

    // The dashboard's dispatch input ALWAYS spawns a new session — it is
    // never a reply target. A row being selected is purely the overview
    // navigation cursor (Enter on it OPENS the agent); it must not turn
    // the input into "reply to that agent". Conflating the two trapped
    // the user: navigating to a row (vim j/k) flipped the input to
    // "Reply to <agent>" and there was no obvious way back to spawning a
    // new session. To converse with an existing agent, open it (navigate
    // + Enter, or click) and reply inside its own view.
    //
    // New-session path.
    //
    // Return the new AgentId from the inner constructor
    // so we don't have to rely on `app.agents.last()`.
    //
    // Carry the dashboard's staged model / plan-mode (set via `/model` and
    // `/plan`) onto the new session: the model id seeds `CreateSession`, and
    // effort / plan are applied post-creation by `apply_pending_dispatch_config`.
    let pending_model = app.dashboard.as_ref().and_then(|d| d.pending_model.clone());
    let pending_mode = app
        .dashboard
        .as_ref()
        .map(|d| d.pending_mode)
        .unwrap_or_default();
    let model_id = pending_model.as_ref().map(|m| m.id.clone());
    let prompt_state = app
        .dashboard
        .as_mut()
        .map(|dashboard| snapshot_prompt_widget(&mut dashboard.dispatch, text.clone()))
        .unwrap_or_else(|| {
            crate::views::prompt_widget::StashedPrompt::from_submission(
                text,
                Vec::new(),
                Vec::new(),
            )
        });
    let (prompt_text, mut pasted_images, chip_elements) = prompt_state.into_submission();
    log_dashboard_launched("prompt");
    let saved_shown = app.project_picker_shown;
    app.project_picker_shown = true;
    let (new_id, effects) = dispatch_new_session_inner_with_id(app, model_id);
    app.project_picker_shown = saved_shown;
    let policy_block = app.yolo_policy_block;
    if let Some(agent) = app.agents.get_mut(&new_id) {
        agent.session.enqueue_prompt(prompt_text);
        if let Some(entry) = agent.session.pending_prompts.back_mut() {
            entry.images = std::mem::take(&mut pasted_images);
            entry.chip_elements = chip_elements;
        }
        apply_pending_dispatch_config(agent, pending_model.as_ref(), pending_mode, policy_block);
    }
    crate::prompt_images::drain_and_cleanup(&mut pasted_images);
    // Only clear the input AFTER successful session
    // creation. If the agent failed to register the prompt above we
    // would have already returned without effects.
    if let Some(d) = app.dashboard.as_mut() {
        d.dispatch.set_text("");
        d.error_toast = None;
        d.filter = crate::views::dashboard::Filter::None;
    }
    if attach {
        // Ctrl+S (Send+Open) — walk into the new agent's
        // detail view AND paint the session-overlay chrome.
        // Mirrors `dispatch_dashboard_attach` and
        // `dispatch_dashboard_create_new_agent_with_detail`:
        // both `attached_agent` and `selected` follow the new
        // row so the overlay's `i/n [‹][›] [✗]` chips have an
        // anchor and Esc walks back to the dashboard.
        if let Some(d) = app.dashboard.as_mut() {
            d.restore_peek_viewport(&mut app.agents);
            d.focus_row(crate::views::dashboard::DashboardRowId::TopLevel(new_id));
            d.attached_agent = Some(new_id);
        }
        app.active_view = ActiveView::Agent(new_id);
        surface_yolo_launch_block_notice(app, new_id);
    } else {
        // Plain Enter (Send) stays on the dashboard, no auto-select.
        // The freshly-created row is left unselected so the overview
        // navigation cursor stays where the user had it — the input
        // always dispatches a NEW session regardless of selection, so
        // there's no reply state to worry about here.
        app.active_view = ActiveView::AgentDashboard;
        // `apply_pending_dispatch_config` clamps a pinned always-approve to
        // Normal and toasts the new agent, but that toast is invisible while the
        // view stays on the dashboard — mirror it on the dashboard's error slot.
        let enabling =
            pending_mode == crate::views::dashboard::DashboardDispatchMode::AlwaysApprove;
        if let Some(warning) = yolo_enable_blocked(app, enabling)
            && let Some(d) = app.dashboard.as_mut()
        {
            d.set_error_toast(warning);
        }
    }
    effects
}

/// Resolve a slash command typed into the dashboard's dispatch input.
///
/// The dashboard has no session context, so the execution path is more
/// limited than the agent view's:
///
///   - Builtin commands that return `CommandResult::Action(...)` (e.g.
///     `/dashboard`, `/exit`, `/theme`, `/settings`, `/help`, `/model`,
///     `/mcps`, `/plugin`, …) are dispatched identically to the agent path.
///   - `CommandResult::Message` / `Error` surface as an `error_toast`
///     on the dashboard (no scrollback to push into). `Error` strings
///     get the `✗` prefix via `set_error_toast`; `Message` strings are
///     stored verbatim (they carry their own glyph).
///   - `CommandResult::Handled` / `HandledNoOp` clear the input.
///   - `CommandResult::PassThrough` (unknown commands, ACP-advertised
///     pass-throughs, `CommandResult::QueueCommand`, and
///     `InjectSkill`) fall back to dispatching a new session with the
///     text as its first prompt — the dashboard treats them like a
///     bare free-text dispatch. This keeps the user's typing from
///     being silently dropped on the floor when they invoke a
///     plugin/skill from the dashboard surface.
///
/// Offer / execute tri-state (matches completion's [`command_offered`]):
///   - **Unknown** token → [`dispatch_dashboard_dispatch`] (new session prompt).
///   - **Registered, not offered** (session-scoped hidden on this surface,
///     or `dashboard_only` off-dashboard) → clear dispatch + error toast;
///     do **not** spawn with the slash text as the prompt.
///   - **Registered, offered** → MRU + `command.run` (e.g. `/model` /
///     `/plan` stage the next spawn).
pub(super) fn dispatch_dashboard_dispatch_slash(app: &mut AppView, text: String) -> Vec<Effect> {
    use crate::slash::command::{CommandExecCtx, CommandResult};
    use crate::slash::parse_invocation;

    // Enter is a submit attempt — stop voice and drop the target up front.
    voice_stop_on_submit(app);
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return vec![];
    }

    let coding_data_sharing_opt_out_from_app = app.coding_data_retention_opt_out;
    let show_tips_from_app = app.show_tips;
    let auto_update_from_app = app.auto_update;
    let respect_manual_folds_from_app = app.appearance.scrollback.scroll.respect_manual_folds;
    let auto_mode_gate_from_app = app.auto_mode_gate;
    let ask_user_question_timeout_enabled_from_app = app.ask_user_question_timeout_enabled;
    let voice_stt_language_from_app = app.voice_config.language.clone();

    // Build the execution context from app-wide state. The dashboard
    // is session-less, so `session_id` is `None`. Offered session-less
    // opt-ins (`/model`, `/plan`) and pager-global commands still run
    // and may toast if a dispatcher needs an agent.
    let result = {
        let Some(invocation) = parse_invocation(trimmed.as_str()) else {
            return vec![];
        };

        // Get the slash registry from the dashboard's prompt widget.
        // The dashboard owns its own registry (populated at open time
        // with builtins + ACP commands from `bootstrap_acp_commands`).
        let Some(dashboard) = app.dashboard.as_ref() else {
            return vec![];
        };
        let reg = dashboard.dispatch.slash_controller.registry();

        {
            use xai_grok_telemetry::events::{PagerCommandSource, PagerSlashCommand};
            use xai_grok_telemetry::session_ctx::log_event;
            let source = if reg.is_builtin(invocation.token) {
                PagerCommandSource::Builtin
            } else {
                PagerCommandSource::NonBuiltin
            };
            log_event(PagerSlashCommand {
                command_name: invocation.token.to_string(),
                source,
            });
        }

        // Tier-restricted commands stay visible for discoverability but must
        // not execute — and must not fall through to the unknown-command
        // path below (which would spawn a session with the raw slash text as
        // its first prompt). The dashboard has no question-modal surface, so
        // upsell via the feedback toast.
        if reg.is_restricted(invocation.token) {
            let token = invocation.token.to_string();
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast(&format!(
                    "/{token} requires SuperGrok — upgrade at {}",
                    super::billing::UPSELL_URL_UPGRADE
                ));
            }
            return vec![];
        }

        let Some(command) = reg.get(invocation.token).cloned() else {
            // Unknown command. Fall back to the regular dispatch
            // path so the text becomes a new session's prompt.
            return dispatch_dashboard_dispatch(app, text, /* attach */ false);
        };
        // Registered but not offered on this surface (session-scoped
        // hidden from the dropdown, or non-dashboard `dashboard_only`):
        // error toast — never spawn a session whose first prompt is the
        // slash text (that was worse than the old loud Action toasts).
        if !dashboard
            .dispatch
            .slash_controller
            .is_command_offered(command.as_ref(), &app.models)
        {
            let name = command.name();
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast(&format!("/{name} only works in a session"));
            }
            return vec![];
        }
        if let Some(dashboard) = app.dashboard.as_mut() {
            // Records MRU and queues an off-thread persist internally.
            dashboard
                .dispatch
                .slash_controller
                .record_command_use(invocation.token, invocation.token);
        }

        let dashboard_multiline = app.dashboard.as_ref().is_some_and(|d| d.multiline_mode);
        let mut ctx = CommandExecCtx {
            models: &app.models,
            session_id: None,
            bundle_state: &app.bundle_state,
            screen_mode: app.screen_mode,
            billing_surface_visible: app.usage_visible,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: dashboard_multiline,
                yolo_mode: app.default_yolo,
                auto_mode: app.current_ui.permission_mode.as_deref() == Some("auto")
                    && !app.default_yolo,
                current_model_name: app.models.current_model_name(),
                available_models: app
                    .models
                    .available
                    .iter()
                    .map(|(id, info)| (info.name.clone(), id.clone()))
                    .collect(),
                coding_data_sharing_opt_out: coding_data_sharing_opt_out_from_app,
                plan_mode_active: false,
                show_tips: show_tips_from_app,
                auto_update: auto_update_from_app,
                vim_mode: crate::appearance::cache::load_vim_mode(),
                scroll_speed: crate::appearance::cache::load_scroll_speed(),
                respect_manual_folds: respect_manual_folds_from_app,
                auto_mode_gate: auto_mode_gate_from_app,
                ask_user_question_timeout_enabled: ask_user_question_timeout_enabled_from_app,
                voice_stt_language: voice_stt_language_from_app,
            },
        };
        command.run(&mut ctx, invocation.args)
    };

    match result {
        CommandResult::Handled | CommandResult::HandledNoOp => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = None;
            }
            vec![]
        }
        CommandResult::Error(msg) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                // Command errors are plain strings ("Unknown model: …",
                // "Usage: /model <name> [effort]") with no glyph of their
                // own — route through `set_error_toast` so the verbatim
                // badge shows the `✗` error marker. `Message` results
                // below stay verbatim: they carry their own glyph
                // (e.g. `✓ Theme: …`).
                d.set_error_toast(&msg);
            }
            vec![]
        }
        CommandResult::Message(msg) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = Some(msg);
            }
            vec![]
        }
        CommandResult::Action(Action::ExitSession) => {
            // ExitSession from a session-less surface is meaningless
            // — collapse it to `/dashboard`'s exit semantics.
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
            }
            dispatch(Action::ExitDashboard, app)
        }
        // `/model` on the session-less dashboard stages the model for the
        // NEXT spawned agent instead of switching a (nonexistent) session.
        // Both the effort-bearing (`SwitchModel`) and bare
        // (`SetDefaultModel`) forms map to the same per-spawn staging — we
        // deliberately do NOT persist a global default here.
        CommandResult::Action(Action::SwitchModel { model_id, effort }) => {
            stage_dashboard_model(app, model_id, effort);
            vec![]
        }
        CommandResult::Action(Action::SetDefaultModel(model_id)) => {
            stage_dashboard_model(app, model_id, None);
            vec![]
        }
        // `/plan` toggles whether the next spawned agent starts in plan
        // mode. The command always reports `On` here (the dashboard's
        // `plan_mode_active` snapshot is always false), so we flip between
        // Plan and Normal to give a session-less way to turn it back off.
        CommandResult::Action(Action::SetPlanMode(_)) => {
            use crate::views::dashboard::DashboardDispatchMode;
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = None;
                d.pending_mode = if d.pending_mode == DashboardDispatchMode::Plan {
                    DashboardDispatchMode::Normal
                } else {
                    DashboardDispatchMode::Plan
                };
            }
            vec![]
        }
        // `/plan <description>` — stage plan mode AND spawn immediately with
        // the description as the first prompt.
        CommandResult::Action(Action::EnterPlanMode { description }) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.pending_mode = crate::views::dashboard::DashboardDispatchMode::Plan;
            }
            match description {
                Some(desc) => {
                    dispatch_dashboard_dispatch(app, desc, /* attach */ false)
                }
                None => {
                    if let Some(d) = app.dashboard.as_mut() {
                        d.dispatch.set_text("");
                        d.error_toast = None;
                    }
                    vec![]
                }
            }
        }
        // `/plan` with an existing plan has nothing to show on the
        // session-less dashboard.
        CommandResult::Action(Action::ShowPlan) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast("No plan to show on the dashboard");
            }
            vec![]
        }
        CommandResult::Action(action) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = None;
            }
            dispatch(action, app)
        }
        CommandResult::Doctor(_) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast("Open a session to run /doctor.");
            }
            vec![]
        }
        CommandResult::QueueCommand(_)
        | CommandResult::InjectSkill { .. }
        | CommandResult::PassThrough(_) => {
            // These results all expect an agent session to consume
            // them. The dashboard has none, so route the original
            // text through the normal "new session with this text as
            // the first prompt" path so the user's intent isn't lost.
            dispatch_dashboard_dispatch(app, text, /* attach */ false)
        }
    }
}

/// Stage a model (+ optional reasoning effort) for the next agent the
/// dashboard spawns. Resolves the human-readable display name from the
/// app's model catalog (falling back to the raw id) so the renderer can
/// show the indicator without a live `ModelState`. Clears the dispatch
/// input + any error toast.
fn stage_dashboard_model(
    app: &mut AppView,
    model_id: acp::ModelId,
    effort: Option<xai_grok_shell::sampling::types::ReasoningEffort>,
) {
    let display = app
        .models
        .available
        .get(&model_id)
        .map(|info| info.name.clone())
        .unwrap_or_else(|| model_id.0.to_string());
    if let Some(d) = app.dashboard.as_mut() {
        d.dispatch.set_text("");
        d.error_toast = None;
        // Mirror the staged choice into the dashboard's catalog snapshot so a
        // subsequent `/model` dropdown marks THIS model as `(current)` (and
        // its effort as `(active)`) rather than the app default the snapshot
        // was seeded with at open. Without this the dropdown lags one step
        // behind `pending_model`.
        d.models.set_current(model_id.clone(), effort);
        d.pending_model = Some(crate::views::dashboard::PendingDispatchModel {
            id: model_id,
            effort,
            display,
        });
    }
}

/// Apply the dashboard's staged model effort + plan mode to a freshly
/// spawned agent. The base model is already seeded via `CreateSession`'s
/// `model_id`; here we stash the reasoning effort (pushed to the shell once
/// the session exists, mirroring the agent-view flow) and the deferred plan
/// `SessionMode` (consumed in the `SessionCreated` handlers).
pub(super) fn apply_pending_dispatch_config(
    agent: &mut AgentView,
    pending_model: Option<&crate::views::dashboard::PendingDispatchModel>,
    pending_mode: crate::views::dashboard::DashboardDispatchMode,
    policy_block: Option<&'static str>,
) {
    use crate::views::dashboard::DashboardDispatchMode;

    if let Some(m) = pending_model {
        // The base model is seeded via `CreateSession.model_id`; only stash a
        // deferred switch when an explicit effort must be pushed. Setting it
        // (or clearing to `None`) also overrides any CLI `-m` default so the
        // dashboard's `/model` choice wins.
        agent.session.deferred_model_switch = m.effort.map(|e| (m.id.clone(), Some(e)));
    }
    match pending_mode {
        DashboardDispatchMode::Normal => {
            // Explicit normal overrides any `app.default_yolo` seed.
            agent.session.yolo_mode = false;
        }
        DashboardDispatchMode::Plan => {
            agent.session.yolo_mode = false;
            agent.deferred_session_mode = Some(xai_grok_tools::types::SessionMode::Plan);
            // Optimistic so the agent view reflects plan mode immediately when
            // opened via Ctrl+S, before the ACP round-trip confirms it.
            agent.plan_mode_pending = Some(true);
        }
        // Backstop: staging is already gated, but this write sits outside
        // `set_yolo_mode_inner`, so re-check the pin here.
        DashboardDispatchMode::AlwaysApprove => {
            if let Some(warning) = policy_block {
                agent.session.yolo_mode = false;
                agent.show_toast(warning);
            } else {
                // Client-side auto-approve for the spawned session (per-spawn;
                // not persisted as a global default).
                agent.session.yolo_mode = true;
            }
        }
    }
}

/// Send or queue a reply typed into the peek panel's `❯ reply` input.
///
/// The reply is enqueued on the row's owning top-level agent and then
/// [`maybe_drain_queue`] decides the rest: an **idle** agent sends it
/// immediately (a turn starts), a **mid-turn** agent keeps it queued so
/// it drains after the current turn finishes. This is the same queue /
/// drain pipeline the agent view's own prompt input uses, so the two
/// surfaces behave identically.
///
/// Subagent rows can't be replied to (they're driven by their parent),
/// so they surface a toast and leave the peek open.
///
/// `attach` (Ctrl+S) additionally walks into the agent's detail
/// view, mirroring the dispatch input's send+open affordance.
/// Cycle the PEEKED agent's live mode (Normal → Plan → Always-Approve →
/// Normal), the peek-panel counterpart to `DashboardCycleMode`. Reuses
/// the shared cycle body `dispatch_cycle_mode_and_sync` by temporarily
/// targeting the peeked agent (the same `active_view` swap as
/// `dispatch_dashboard_toggle_auto_approve`), so the peek behaves exactly
/// like Shift+Tab inside that agent's chat view. The bottom-border badge
/// reflects the new mode on the next frame. Only top-level agents have a
/// mode to cycle — subagents are parent-driven.
pub(super) fn dispatch_dashboard_peek_cycle_mode(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let Some(row) = app
        .dashboard
        .as_ref()
        .and_then(|d| d.peek.as_ref().map(|p| p.row.clone()))
    else {
        return vec![];
    };
    let agent_id = match row {
        DashboardRowId::TopLevel(id) => id,
        DashboardRowId::Subagent { .. } => {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_error_toast("Can't change a subagent's mode");
            }
            return vec![];
        }
        DashboardRowId::Roster { .. } => return vec![],
    };
    if !app.agents.contains_key(&agent_id) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Session no longer exists");
        }
        return vec![];
    }

    // Temporarily target the peeked agent so `dispatch_cycle_mode_and_sync`
    // (which reads `active_view`) acts on it; restored before returning so the
    // dashboard stays foregrounded. Uses the telemetry-free cycle body: the
    // user is viewing the dashboard, not this agent's prompt, so a plan nudge
    // still within TTL must not attribute a (spurious) acceptance.
    let saved_view = app.active_view;
    app.active_view = ActiveView::Agent(agent_id);
    let effects = dispatch_cycle_mode_and_sync(app);
    app.active_view = saved_view;
    effects
}

pub(super) fn dispatch_dashboard_peek_reply(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    text: String,
    attach: bool,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    // Enter is a submit attempt — stop voice and drop the target up front so a
    // rejected reply can't leave a hot mic or let a late final refill the box.
    voice_stop_on_submit(app);

    // Paste-then-immediate-send: a Cmd+V image probe is still off-thread. Stash
    // this reply and re-issue it once the probe completes so the image is never
    // dropped from the reply's content blocks.
    if let Some(d) = app.dashboard.as_mut()
        && d.paste_probe_in_flight > 0
    {
        d.deferred_peek_send =
            Some(crate::views::dashboard::state::DeferredPeekSend { row, attach });
        return vec![];
    }

    // Only top-level agents accept replies. Subagents are read-only
    // peeks driven by their parent turn.
    let DashboardRowId::TopLevel(agent_id) = row else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Can't reply to a subagent");
        }
        return vec![];
    };

    if !app.agents.contains_key(&agent_id) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Session no longer exists");
        }
        return vec![];
    }

    let prompt_state = app
        .dashboard
        .as_mut()
        .map(|dashboard| snapshot_prompt_widget(&mut dashboard.peek_reply, text.clone()))
        .unwrap_or_else(|| {
            crate::views::prompt_widget::StashedPrompt::from_submission(
                text,
                Vec::new(),
                Vec::new(),
            )
        });
    let (text, images, chip_elements) = prompt_state.into_submission();

    if text.trim().is_empty() && images.is_empty() {
        return vec![];
    }

    let drain = {
        let Some(agent) = app.agents.get_mut(&agent_id) else {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_peek(None);
                d.set_error_toast("Session no longer exists");
            }
            return vec![];
        };

        // Enqueue + drain: idle → sends now, running → stays queued.
        // Untrimmed so `chip_elements` byte ranges stay aligned with the stored text.
        agent.session.enqueue_prompt(text);
        if let Some(entry) = agent.session.pending_prompts.back_mut() {
            entry.chip_elements = chip_elements;
            if !images.is_empty() {
                entry.images = images;
            }
        }
        maybe_drain_queue(agent)
    };
    note_peek_page_flip(app, agent_id, drain.page_flip_entry);
    let effects = drain.effects;

    // Clear the reply draft now that it's been accepted, and drop any
    // stale error toast.
    if let Some(d) = app.dashboard.as_mut() {
        d.clear_peek_reply();
        d.error_toast = None;
    }

    if attach {
        // Ctrl+S (Send+Open) — walk into the agent's detail view
        // with the session-overlay chrome, mirroring
        // `dispatch_dashboard_dispatch`'s attach branch.
        if let Some(d) = app.dashboard.as_mut() {
            d.restore_peek_viewport(&mut app.agents);
            d.focus_row(DashboardRowId::TopLevel(agent_id));
            d.attached_agent = Some(agent_id);
        }
        app.active_view = ActiveView::Agent(agent_id);
        surface_yolo_launch_block_notice(app, agent_id);
    }

    effects
}

pub(super) fn dispatch_dashboard_toggle_pin(app: &mut AppView) -> Vec<Effect> {
    if let Some(d) = app.dashboard.as_mut() {
        let _ = d.toggle_pin_selected();
    }
    dispatch_dashboard_persist(app)
}

pub(super) fn dispatch_dashboard_begin_rename(app: &mut AppView) {
    let Some(d) = app.dashboard.as_mut() else {
        return;
    };
    let Some(sel) = d.selected.clone() else {
        return;
    };
    // Only top-level rows are renameable (subagents are tool-spawned
    // and have no user-visible name to rename).
    if sel.is_subagent() {
        d.set_error_toast("Subagent rows can't be renamed");
        return;
    }
    // The draft starts EMPTY (not prefilled with the current title):
    // renames are almost always full rewrites, so prefilling only costs
    // the user a hold-Backspace. Esc / empty-draft Enter cancel without
    // touching the existing name.
    if let Some(d) = app.dashboard.as_mut() {
        d.rename = Some(crate::views::dashboard::state::RenameDraft::new(sel, ""));
    }
}

pub(super) fn dispatch_dashboard_commit_rename(app: &mut AppView) -> Vec<Effect> {
    let Some(d) = app.dashboard.as_mut() else {
        return vec![];
    };
    let Some(rn) = d.rename.take() else {
        return vec![];
    };
    // Edge case 5: empty/whitespace draft cancels without committing.
    let trimmed = rn.text().trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let title: String = crate::views::session_title::sanitize_display_text(trimmed).into_owned();
    if title.is_empty() {
        return vec![];
    }
    let crate::views::dashboard::DashboardRowId::TopLevel(agent_id) = rn.row else {
        return vec![];
    };
    let mut effects = Vec::new();
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        if let Some(session_id) = agent.session.session_id.clone() {
            let cwd = agent.session.cwd.clone();
            agent.display_name = Some(title.clone());
            effects.push(Effect::RenameSession {
                agent_id,
                session_id,
                title,
                cwd,
            });
        } else {
            agent.display_name = Some(title);
        }
    }
    effects
}

/// Pick the dashboard row the cursor should land on after `closed` is
/// removed. Computed against the CURRENT (pre-removal) display order so
/// it mirrors what the user sees: the next visible row below `closed`
/// (so the cursor stays put while the rows below shift up — i.e. it
/// "moves down 1" relative to the list) or, when `closed` is the last
/// row, the previous visible row. Section headers are skipped so the
/// cursor always lands on an agent row. Returns `None` when `closed`
/// is the only row — the caller then clears the selection and lets
/// `reanchor_selection` fall back to the `[+ New Agent]` button.
///
/// Without this, closing the selected agent leaves a stale cursor that
/// `reanchor_selection` drops to `None`, and the next ↑/↓ restarts from
/// the top of the list — a jarring jump.
fn dashboard_neighbor_row(
    app: &AppView,
    closed: &crate::views::dashboard::DashboardRowId,
) -> Option<crate::views::dashboard::DashboardRowId> {
    use crate::views::dashboard::Focusable;
    let d = app.dashboard.as_ref()?;
    let home = crate::views::dashboard::render::cached_home();
    let roster: &[crate::app::roster::RosterEntry] = if app.leader_mode {
        &app.leader_roster
    } else {
        &app.dashboard_local_sessions
    };
    let rows = crate::views::dashboard::build_rows_with_roster(
        &app.agents,
        &d.pinned,
        &d.reorder,
        None,
        d.grouping,
        &d.filter,
        home,
        roster,
    );
    let focusables = crate::views::dashboard::render::focusables(
        &rows,
        d.grouping,
        &d.filter,
        &d.collapsed_sections,
        d.idle_show_all,
        d.search_mode,
    );
    let cur = focusables
        .iter()
        .position(|f| matches!(f, Focusable::Row(id) if id == closed))?;
    // Next row below (down 1); else the closed row was last → previous row.
    let next = focusables[cur + 1..].iter().find_map(|f| match f {
        Focusable::Row(id) => Some(id.clone()),
        Focusable::Section(_) | Focusable::IdleOverflow => None,
    });
    next.or_else(|| {
        focusables[..cur].iter().rev().find_map(|f| match f {
            Focusable::Row(id) => Some(id.clone()),
            Focusable::Section(_) | Focusable::IdleOverflow => None,
        })
    })
}

pub(super) fn dispatch_dashboard_stop(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;
    use std::time::Instant;

    let Some(sel) = app.dashboard.as_ref().and_then(|d| d.selected.clone()) else {
        return vec![];
    };
    match &sel {
        DashboardRowId::TopLevel(id) => {
            let id = *id;
            let Some(agent) = app.agents.get(&id) else {
                return vec![];
            };
            let now = Instant::now();
            // `t.elapsed()` is the idiomatic Instant API
            // for "how long since this Instant". Behaviour identical
            // to `now.duration_since(*t)` when `t <= now`, which is
            // the only case the dispatcher constructs.
            let already_confirming = app
                .dashboard
                .as_ref()
                .and_then(|d| d.stop_confirm.as_ref())
                .is_some_and(|(prev, t)| {
                    *prev == sel
                        && t.elapsed() < crate::views::dashboard::state::STOP_CONFIRM_WINDOW
                });
            if already_confirming {
                // Pick the cursor's next home BEFORE the row vanishes, so
                // closing moves the selection down 1 instead of letting it
                // go stale (which `reanchor_selection` drops to `None`,
                // bouncing the next ↑/↓ back to the top of the list).
                let neighbor = dashboard_neighbor_row(app, &sel);
                if let Some(d) = app.dashboard.as_mut() {
                    d.stop_confirm = None;
                }
                // Second press: close the agent.
                let effects = dispatch_sessions_confirm_close(app, id);
                // Only move the cursor if the close actually happened
                // (it's refused for the last remaining session).
                if !app.agents.contains_key(&id)
                    && let Some(d) = app.dashboard.as_mut()
                {
                    match neighbor {
                        Some(n) => d.focus_row(n),
                        // No neighbour left — land on the always-present
                        // `[+ New Agent]` button via the focus helper so the
                        // "exactly one cursor active" invariant holds (a bare
                        // `selected = None` would leave no cursor and drop the
                        // footer into its defensive fallback).
                        None => d.focus_new_agent_button(),
                    }
                }
                return effects;
            }
            // First press: cancel turn if running, plant confirmation.
            let mut effects = Vec::new();
            if !agent.session.state.is_idle()
                && let Some(sid) = agent.session.session_id.clone()
            {
                effects.push(Effect::CancelTurn {
                    session_id: sid,
                    cancel_subagents: true,
                    trigger: None,
                    // Dashboard first-press cancel — no local prompt rewind.
                    rewind_if_pristine: false,
                });
            }
            if let Some(d) = app.dashboard.as_mut() {
                // The footer's `ShortcutsBar::with_pending` already
                // paints the "press Ctrl+X again to close this
                // session" prompt in the bottom bar. Surfacing the
                // same line via `error_toast` would also bleed it
                // into the dispatch input placeholder — two copies
                // of the same hint, in two different places, with
                // the dispatch one stealing visual weight from the
                // user's typing area. The footer hint is the
                // canonical surface.
                d.stop_confirm = Some((sel, now));
            }
            effects
        }
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => {
            let Some(agent) = app.agents.get_mut(parent) else {
                return vec![];
            };
            let Some(info) = agent.subagent_sessions.get_mut(child_session_id) else {
                return vec![];
            };
            let subagent_id = info.subagent_id.to_string();
            info.pending_kill = true;
            info.kill_requested_at = Some(Instant::now());
            let session_id = agent.session.session_id.clone();
            session_id
                .map(|sid| Effect::KillSubagent {
                    session_id: sid,
                    subagent_id,
                })
                .into_iter()
                .collect()
        }
        // Roster-only rows are hosted elsewhere — this client can't stop
        // them.
        DashboardRowId::Roster { .. } => vec![],
    }
}

pub(super) fn dispatch_dashboard_toggle_grouping(app: &mut AppView) -> Vec<Effect> {
    if let Some(d) = app.dashboard.as_mut() {
        d.toggle_grouping();
    }
    dispatch_dashboard_persist(app)
}

pub(super) fn dispatch_dashboard_select(app: &mut AppView, next: bool) {
    let Some(d) = app.dashboard.as_mut() else {
        return;
    };
    // We don't have rows cached here; reconstruct from agents.
    // Use the shared `cached_home()` instead of
    // re-reading the env var on every keystroke.
    let home = crate::views::dashboard::render::cached_home();
    // Same roster source the renderer uses, so navigation matches the visible
    // rows: leader roster in leader mode, local idle sessions otherwise.
    // Disjoint field borrows (`app.dashboard` is held mutably via `d`).
    let roster: &[crate::app::roster::RosterEntry] = if app.leader_mode {
        &app.leader_roster
    } else {
        &app.dashboard_local_sessions
    };
    let rows = crate::views::dashboard::build_rows_with_roster(
        &app.agents,
        &d.pinned,
        &d.reorder,
        None,
        d.grouping,
        &d.filter,
        home,
        roster,
    );
    // Unified, display-order cursor targets: section headers AND visible
    // rows (collapsed sections contribute only their header, so their
    // hidden rows are skipped). Placeholders are excluded.
    let focusables = crate::views::dashboard::render::focusables(
        &rows,
        d.grouping,
        &d.filter,
        &d.collapsed_sections,
        d.idle_show_all,
        d.search_mode,
    );
    let set_cursor = |d: &mut crate::views::dashboard::DashboardState,
                      f: &crate::views::dashboard::Focusable| {
        match f {
            crate::views::dashboard::Focusable::Section(key) => d.focus_section(*key),
            crate::views::dashboard::Focusable::Row(id) => d.focus_row(id.clone()),
            crate::views::dashboard::Focusable::IdleOverflow => d.focus_idle_overflow(),
        }
    };
    // Button-focused navigation contract:
    //   - Down on the button → first focusable (header or row).
    //   - Up on the button   → stay on the button (no wrap).
    if d.new_agent_button_focused {
        if next && !focusables.is_empty() {
            set_cursor(d, &focusables[0]);
            d.clear_manual_scroll();
        }
        return;
    }
    if focusables.is_empty() {
        // Nothing to land on AND button isn't focused → fall back to the
        // button so the cursor lives somewhere.
        d.focus_new_agent_button();
        return;
    }
    // Current index from the active cursor (section header or row).
    let cur = focusables
        .iter()
        .position(|f| match f {
            crate::views::dashboard::Focusable::Section(key) => d.selected_section == Some(*key),
            crate::views::dashboard::Focusable::Row(id) => d.selected.as_ref() == Some(id),
            crate::views::dashboard::Focusable::IdleOverflow => d.selected_idle_overflow,
        })
        .unwrap_or(0);
    // Up on the first focusable → focus the `[+ New Agent]` button.
    // The button acts as a sentinel above index 0, exactly like the
    // agent's tabs in the agents modal.
    if !next && cur == 0 {
        d.focus_new_agent_button();
        d.clear_manual_scroll();
        return;
    }
    let new = if next {
        (cur + 1).min(focusables.len() - 1)
    } else {
        cur.saturating_sub(1)
    };
    set_cursor(d, &focusables[new]);
    // Arrow-key nav is selection-driven: re-engage the clamp's
    // snap-to-selection so the viewport tracks the cursor. Without
    // this, ↑/↓ after a wheel scroll would leave the cursor visibly
    // selected on a row outside the viewport.
    d.clear_manual_scroll();
}

pub(super) fn dispatch_dashboard_reorder(app: &mut AppView, up: bool) -> Vec<Effect> {
    let Some(d) = app.dashboard.as_mut() else {
        return vec![];
    };
    let Some(sel) = d.selected.clone() else {
        return vec![];
    };
    // Maintain a stable explicit ordering list. The renderer applies
    // this list as a "float to position" rule inside the row's group.
    let pos = d.reorder.iter().position(|r| *r == sel);
    if up {
        match pos {
            Some(0) => {
                d.reorder.remove(0);
            }
            Some(i) => {
                d.reorder.swap(i, i - 1);
            }
            None => {
                d.reorder.insert(0, sel);
            }
        }
    } else {
        match pos {
            Some(i) if i + 1 < d.reorder.len() => {
                d.reorder.swap(i, i + 1);
            }
            Some(_) => {
                // Already at the bottom — append to end.
            }
            None => {
                d.reorder.push(sel);
            }
        }
    }
    dispatch_dashboard_persist(app)
}

fn dispatch_dashboard_persist(app: &mut AppView) -> Vec<Effect> {
    let Some(d) = app.dashboard.as_ref() else {
        return vec![];
    };
    // Don't hardcode `enabled = true`. Thread through
    // the on-disk value so a user who deliberately set `enabled =
    // false` doesn't get it silently overwritten on the next pin or
    // grouping toggle.
    let enabled = app
        .dashboard_persisted
        .as_ref()
        .map(|p| p.enabled)
        .unwrap_or(true);
    let resolver = crate::views::dashboard::SessionIdResolver::from_agents(&app.agents);
    let persisted = d.to_persisted(enabled, &resolver);
    app.dashboard_persisted = Some(persisted.clone());
    vec![Effect::PersistDashboard(persisted)]
}

/// Answer a permission request from the
/// dashboard peek panel without going through `PermissionSelect`,
/// which only works when `active_view == Agent(_)`. Routes directly
/// to the row's owning agent and verifies the request_id has not
/// rotated since the peek snapshot was taken.
pub(super) fn dispatch_dashboard_permission_select(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    request_id: usize,
    option_id: acp::PermissionOptionId,
) -> Vec<Effect> {
    // Determine the owning AgentId.
    let target_id = match &row {
        crate::views::dashboard::DashboardRowId::TopLevel(id) => *id,
        crate::views::dashboard::DashboardRowId::Subagent { parent, .. } => *parent,
        crate::views::dashboard::DashboardRowId::Roster { .. } => return vec![],
    };
    let Some(agent) = app.agents.get_mut(&target_id) else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Row no longer exists");
        }
        return vec![];
    };
    // Stale-snapshot guard.
    let front_matches = agent
        .permission_queue
        .front()
        .is_some_and(|p| p.id == request_id);
    if !front_matches {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Permission has changed — re-open peek");
        }
        return vec![];
    }
    let Some(perm) = agent.permission_queue.pop_front() else {
        return vec![];
    };

    let meta = if let Some(scope) = perm
        .mcp_scope
        .as_ref()
        .filter(|_| option_id.0.as_ref() == "allow-always-mcp")
    {
        let selection = match scope.selected {
            crate::views::permission_view::McpScope::Tool => {
                xai_grok_workspace::permission::McpScopeSelection::Tool {
                    tool_name: scope.tool_name.clone(),
                }
            }
            crate::views::permission_view::McpScope::Server => match &scope.server_prefix {
                Some(prefix) => xai_grok_workspace::permission::McpScopeSelection::Server {
                    server: prefix.clone(),
                },
                None => xai_grok_workspace::permission::McpScopeSelection::Tool {
                    tool_name: scope.tool_name.clone(),
                },
            },
        };
        serde_json::to_value(selection)
            .ok()
            .and_then(|v| v.as_object().cloned())
    } else if let Some(ref h) = perm.bash_highlights
        && perm.bash_selection_count > 0
    {
        let parts: Vec<String> = h.highlighted_words[..perm.bash_selection_count].to_vec();
        serde_json::to_value(xai_grok_workspace::permission::BashCommandSelectedTerms {
            command_parts: parts,
        })
        .ok()
        .and_then(|v| v.as_object().cloned())
    } else {
        None
    };

    perm.request
        .response_tx
        .send(Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id)),
        )
        .meta(meta)))
        .ok();

    resolve_permission_queue_transition(agent);

    // Refresh the peek (it likely no longer has a question).
    if let Some(d) = app.dashboard.as_mut() {
        d.set_peek(None);
    }
    vec![]
}

/// Reject the peeked agent's pending permission with a typed feedback
/// message — the peek panel's "No, type to add feedback" path.
///
/// Mirrors [`dispatch_permission_followup`](super::permissions::dispatch_permission_followup) (resolve the front request
/// with the `RejectOnce` option + `followup_message` meta) but targets
/// the dashboard row's agent instead of the active view, with the same
/// stale-request guard as [`dispatch_dashboard_permission_select`].
pub(super) fn dispatch_dashboard_permission_followup(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    request_id: usize,
    text: String,
) -> Vec<Effect> {
    let target_id = match &row {
        crate::views::dashboard::DashboardRowId::TopLevel(id) => *id,
        crate::views::dashboard::DashboardRowId::Subagent { parent, .. } => *parent,
        crate::views::dashboard::DashboardRowId::Roster { .. } => return vec![],
    };
    let Some(agent) = app.agents.get_mut(&target_id) else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Row no longer exists");
        }
        return vec![];
    };
    // Stale-snapshot guard — the front request must still match.
    let front_matches = agent
        .permission_queue
        .front()
        .is_some_and(|p| p.id == request_id);
    if !front_matches {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Permission has changed — re-open peek");
        }
        return vec![];
    }
    let Some(perm) = agent.permission_queue.pop_front() else {
        return vec![];
    };
    // Resolve with the RejectOnce option + feedback; cancel if the
    // request somehow has no reject option.
    let option_id = perm
        .options
        .iter()
        .find(|o| o.kind == acp::PermissionOptionKind::RejectOnce)
        .map(|o| o.option_id.clone());
    let outcome = match option_id {
        Some(option_id) => {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id))
        }
        None => acp::RequestPermissionOutcome::Cancelled,
    };
    let meta = if !text.trim().is_empty() {
        serde_json::json!({ "followup_message": text })
            .as_object()
            .cloned()
    } else {
        None
    };
    perm.request
        .response_tx
        .send(Ok(acp::RequestPermissionResponse::new(outcome).meta(meta)))
        .ok();
    resolve_permission_queue_transition(agent);
    if let Some(d) = app.dashboard.as_mut() {
        d.set_peek(None);
    }
    vec![]
}

/// Answer the peeked agent's pending `AskUserQuestion` (the Ask tool)
/// from the dashboard peek panel — `option_idx` selects an option,
/// `None` + non-empty `freeform` submits the "Other" free-text answer.
/// Delegates to [`AgentView::dashboard_answer_question`], which sends the
/// ext-response; the peek closes once an answer is actually submitted.
pub(super) fn dispatch_dashboard_question_answer(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    option_idx: Option<usize>,
    freeform: String,
) -> Vec<Effect> {
    let target_id = match &row {
        crate::views::dashboard::DashboardRowId::TopLevel(id) => *id,
        crate::views::dashboard::DashboardRowId::Subagent { parent, .. } => *parent,
        crate::views::dashboard::DashboardRowId::Roster { .. } => return vec![],
    };
    let Some(agent) = app.agents.get_mut(&target_id) else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Row no longer exists");
        }
        return vec![];
    };
    match agent.dashboard_answer_question(option_idx, freeform) {
        // Whole form answered → close the peek.
        crate::app::agent_view::PeekAnswerOutcome::Submitted => {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_peek(None);
            }
        }
        // Advanced to the next question → keep the peek open but reset the
        // per-question draft (option cursor + free-text) so the next
        // question starts fresh; the next render refreshes its content.
        crate::app::agent_view::PeekAnswerOutcome::Advanced => {
            if let Some(d) = app.dashboard.as_mut() {
                if let Some(p) = d.peek.as_mut() {
                    p.selected_option = None;
                }
                d.clear_peek_reply();
            }
        }
        // Nothing happened (e.g. empty "Other") → leave the peek as-is.
        crate::app::agent_view::PeekAnswerOutcome::NoOp => {}
    }
    vec![]
}
