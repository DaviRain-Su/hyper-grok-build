//! Dashboard state types — `DashboardState`, `DashboardRowId`, filters,
//! grouping, persistence.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;

use super::peek::PeekPanelState;
use super::row::DashboardRow;
use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::agent::AgentId;
use crate::app::app_view::InputOutcome;
use crate::input::line_editor::{LineEditOutcome, LineEditor};
use crate::key;
use crate::views::prompt_widget::PromptWidget;

const PROMPT_MULTI_CLICK_MS: u128 = 300;

/// Stable identity for a dashboard row.
///
/// Top-level rows key by `AgentId`. Subagent rows key by their parent
/// agent + `child_session_id` (the parent's hash map key in
/// `AgentView::subagent_sessions`). When the parent closes, the subagent
/// rows naturally disappear because the row builder iterates
/// `app.agents.values()` — no separate cleanup needed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DashboardRowId {
    TopLevel(AgentId),
    Subagent {
        parent: AgentId,
        child_session_id: String,
    },
    /// A leader-roster-only row: a session hosted by the leader (or a
    /// remote host) that this client is NOT locally attached to. Keyed
    /// by the roster `session_id`. Not locally controllable.
    Roster {
        session_id: String,
    },
}

impl DashboardRowId {
    /// True for a subagent row. Used to gate the "rename" affordance —
    /// subagents are read-only in this version.
    pub fn is_subagent(&self) -> bool {
        matches!(self, Self::Subagent { .. })
    }

    pub(crate) fn matches_top_level_agent(&self, agent_id: AgentId) -> bool {
        matches!(self, Self::TopLevel(id) if *id == agent_id)
    }
}

pub(crate) struct PeekViewportLease {
    pub row: DashboardRowId,
    pub snapshot: crate::scrollback::state::ViewportSnapshot,
    pub page_flip_entry: Option<crate::scrollback::EntryId>,
}

pub(crate) fn scrollback_mut_for_row<'a>(
    row: &DashboardRowId,
    agents: &'a mut indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
) -> Option<&'a mut crate::scrollback::state::ScrollbackState> {
    match row {
        DashboardRowId::TopLevel(id) => agents.get_mut(id).map(|a| &mut a.scrollback),
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => agents
            .get_mut(parent)
            .and_then(|p| p.subagent_views.get_mut(child_session_id))
            .map(|c| &mut c.scrollback),
        DashboardRowId::Roster { .. } => None,
    }
}

pub(crate) fn scrollback_available_for_row(
    row: &DashboardRowId,
    agents: &indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
) -> bool {
    match row {
        DashboardRowId::TopLevel(id) => agents.contains_key(id),
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => agents
            .get(parent)
            .is_some_and(|p| p.subagent_views.contains_key(child_session_id)),
        DashboardRowId::Roster { .. } => false,
    }
}

/// A dispatch-input send (spawn a new session) stashed while a clipboard
/// attachment probe is off-thread.
///
/// Only the `attach` flag is captured; the text is re-read from the live
/// widget when the send is re-issued, so a freshly attached image chip (and
/// its aligned chip range) travels with it. Stashed per surface — see
/// [`DashboardState::take_deferred_sends_after_paste`].
#[derive(Debug)]
pub(crate) struct DeferredDispatchSend {
    pub(crate) attach: bool,
}

/// A peek-reply send (reply to an existing agent) stashed while a clipboard
/// attachment probe is off-thread. `row` pins the reply's target so a peek
/// that closed or moved to another row drops the stash instead of replying to
/// the wrong agent.
#[derive(Debug)]
pub(crate) struct DeferredPeekSend {
    pub(crate) row: DashboardRowId,
    pub(crate) attach: bool,
}

/// Persistent identity for a pinned/reordered row.
///
/// Replaces the previous `AgentId(usize)`-keyed
/// persistence which was meaningless across restarts (and worse,
/// could attach to the *wrong* agent if `next_agent_id` happened to
/// reissue the same `usize`).
///
/// Top-level rows persist by ACP `session_id`. Subagent rows persist
/// by (parent's `session_id`, `child_session_id`). When the dashboard
/// opens, we walk `app.agents` and resolve each `PersistedRowId` to
/// the current process's `DashboardRowId`; unresolved ids are dropped
/// (handled by [`DashboardState::gc_stale_refs`]).
///
/// Rows whose underlying session has not yet been created (and thus
/// have no `session_id`) cannot be persisted — they survive only
/// in-memory across the current process lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PersistedRowId {
    TopLevel {
        session_id: String,
    },
    Subagent {
        parent_session_id: String,
        child_session_id: String,
    },
}

impl PersistedRowId {
    /// On-disk serialisation. `top:<session_id>` or
    /// `sub:<parent_session_id>:<child_session_id>`. The session ids
    /// themselves are opaque to the dashboard — we never split them.
    /// The first colon after `sub:` is the only one we split on.
    pub fn to_key(&self) -> String {
        match self {
            Self::TopLevel { session_id } => format!("top:{session_id}"),
            Self::Subagent {
                parent_session_id,
                child_session_id,
            } => format!("sub:{parent_session_id}:{child_session_id}"),
        }
    }

    /// Parse a persisted key back. Returns `None` for malformed input.
    pub fn from_key(s: &str) -> Option<Self> {
        if let Some(sid) = s.strip_prefix("top:") {
            if sid.is_empty() {
                return None;
            }
            return Some(Self::TopLevel {
                session_id: sid.to_string(),
            });
        }
        if let Some(rest) = s.strip_prefix("sub:")
            && let Some((parent, child)) = rest.split_once(':')
            && !parent.is_empty()
            && !child.is_empty()
        {
            return Some(Self::Subagent {
                parent_session_id: parent.to_string(),
                child_session_id: child.to_string(),
            });
        }
        None
    }
}

/// Window within which a second `Ctrl+X` press confirms closing the
/// selected agent. Shared by the dispatcher (which gates the actual
/// close) and the footer (which only paints the "press again" hint
/// while the window is live).
pub const STOP_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// Coarse state used for the dashboard grouping.
///
/// See [`super::row::classify_top_level`] / [`super::row::classify_subagent`]
/// for the mapping rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RowState {
    /// Pending permission OR pending ask_user_question (top-level only;
    /// subagents never enter this state in this version).
    NeedsInput,
    /// Live turn or command running.
    Working,
    /// Alive, idle.
    Idle,
    /// A roster-only session: idle / dormant in another pager process
    /// and never loaded in this one. Local agents never classify as
    /// `Inactive` (see `classify_top_level`) — only
    /// `roster_activity_to_state` produces it, so the "Idle" section
    /// stays focused on sessions you're actively cycling between.
    Inactive,
    /// Finished + status == "completed".
    Completed,
    /// Finished + status in ("failed", "cancelled").
    Failed,
    /// Goal blocked / paused for human reasons. Reserved — unused in this version.
    Blocked,
}

impl RowState {
    /// Sort priority used inside a state group: higher = floats up.
    /// Pinned rows always float to the absolute top regardless of state.
    pub fn group_priority(self) -> u8 {
        match self {
            Self::NeedsInput => 6,
            Self::Working => 5,
            Self::Blocked => 4,
            Self::Idle => 3,
            // Below Idle (these aren't loaded here, so they're less
            // immediately actionable) but above Done/Failed (they're
            // still live, resumable sessions).
            Self::Inactive => 2,
            Self::Completed => 1,
            Self::Failed => 0,
        }
    }

    /// Human-readable group header.
    pub fn group_label(self) -> &'static str {
        match self {
            // Shorter, punchier labels. "Done" reads cleaner as a group
            // header than the past-tense "Completed" did.
            Self::NeedsInput => "Awaiting",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Inactive => "Inactive",
            Self::Completed => "Done",
            Self::Failed => "Failed",
            Self::Blocked => "Blocked",
        }
    }
}

/// Stable identity for a collapsible/selectable dashboard section
/// header. Sections only exist in [`Grouping::State`] mode: the
/// cross-cutting "Pinned" block plus one section per [`RowState`]
/// group. Keyed by the stable group identity (not a positional index)
/// so collapse / selection survive re-sorting and row churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKey {
    /// The "Pinned" block header.
    Pinned,
    /// A per-state group header (Working / Awaiting / Idle / …).
    State(RowState),
}

/// A keyboard-navigable cursor target in the dashboard list: a
/// collapsible section header or a row. Built in display order (see
/// `render::focusables`) so Up/Down navigation and the section/row
/// cursor stay in lockstep with what the renderer paints. A collapsed
/// section contributes only its header (its rows are absent), so nav
/// skips hidden rows.
#[derive(Debug, Clone, PartialEq)]
pub enum Focusable {
    Section(SectionKey),
    Row(DashboardRowId),
    /// The Idle group's "N more" toggle row. Present only
    /// when the Idle group is capped (more than `MAX_VISIBLE_IDLE`
    /// agents, the overflow older than the freshness window). Activating
    /// it (Enter / click) flips [`DashboardState::idle_show_all`].
    IdleOverflow,
}

/// Grouping mode for the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    /// Group by `RowState` (the default).
    #[default]
    State,
    /// Group by working directory (one section per cwd).
    Directory,
}

impl Grouping {
    pub fn toggled(self) -> Self {
        match self {
            Self::State => Self::Directory,
            Self::Directory => Self::State,
        }
    }
}

/// A filter for the visible rows.
///
/// Parsed from the dispatch input — see [`parse_filter`]. The dispatcher
/// applies it in [`super::row::apply_filter`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Filter {
    /// Show everything.
    #[default]
    None,
    /// Match by agent label (`a:<name>`). Case-insensitive substring.
    Agent(String),
    /// Match by row state (`s:<state>`).
    State(RowState),
    /// Free-text substring match against label + cwd display.
    Substring(String),
}

impl Filter {
    /// Convenient sum constructor.
    pub fn from_value(v: FilterValue) -> Self {
        match v {
            FilterValue::None => Self::None,
            FilterValue::Agent(s) => {
                if s.trim().is_empty() {
                    Self::None
                } else {
                    Self::Agent(s)
                }
            }
            FilterValue::State(rs) => Self::State(rs),
            FilterValue::Substring(s) => {
                if s.trim().is_empty() {
                    Self::None
                } else {
                    Self::Substring(s)
                }
            }
        }
    }

    /// Whether this filter would hide any rows. Used to decide whether
    /// `Esc` should clear the filter before exiting.
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Transport type for the [`Action::DashboardSetFilter`] action. Kept
/// separate from [`Filter`] so the action stays `Debug + Clone` without
/// requiring `Filter` to be `Hash`.
#[derive(Debug, Clone)]
pub enum FilterValue {
    None,
    Agent(String),
    State(RowState),
    Substring(String),
}

/// Persisted dashboard configuration stored under `[dashboard]` in
/// `~/.grok/config.toml`. Lenient — corrupted fields fall back to
/// defaults (edge case 12).
///
/// Pinned + reorder are keyed by stable session ids (see
/// [`PersistedRowId`]), not by per-process `AgentId`. The dashboard
/// resolves them to live `DashboardRowId` at open time via
/// [`PersistedDashboard::resolve`].
#[derive(Debug, Clone, Default)]
pub struct PersistedDashboard {
    pub enabled: bool,
    pub grouping: Grouping,
    pub pinned: BTreeSet<PersistedRowId>,
    pub reorder: Vec<PersistedRowId>,
}

impl PersistedDashboard {
    /// Construct a `PersistedDashboard` with feature-flag defaults.
    pub fn defaults() -> Self {
        Self {
            enabled: true,
            grouping: Grouping::default(),
            pinned: BTreeSet::new(),
            reorder: Vec::new(),
        }
    }
}

/// In-memory dashboard state.
///
/// Refreshed every render frame off `app.agents`. Selection is keyed by
/// `DashboardRowId` so a rename / reorder / completion does not invalidate
/// the cursor as long as the row's id is stable.
pub struct DashboardState {
    /// Currently selected row id. May be `None` when no rows are visible.
    pub selected: Option<DashboardRowId>,
    /// Hover target for visual feedback.
    pub hovered_row: Option<DashboardRowId>,
    /// Section-header cursor target. When `Some`, a collapsible section
    /// title (e.g. "Working") holds the cursor instead of a row or the
    /// `[+ New Agent]` button. Mutually exclusive with [`Self::selected`]
    /// and [`Self::new_agent_button_focused`].
    pub selected_section: Option<SectionKey>,
    /// Hovered section header (mouse-move driven); the renderer brightens
    /// its text. Independent of [`Self::hovered_row`].
    pub hovered_section: Option<SectionKey>,
    /// Sections the user has collapsed (their rows are hidden). In-memory
    /// for the dashboard's lifetime; keyed by stable [`SectionKey`].
    pub collapsed_sections: std::collections::HashSet<SectionKey>,
    /// Cursor sits on the Idle group's "N more" overflow
    /// toggle row. The fourth cursor target — mutually exclusive with
    /// [`Self::selected`], [`Self::selected_section`], and
    /// [`Self::new_agent_button_focused`] (enforced via the `focus_*`
    /// helpers).
    pub selected_idle_overflow: bool,
    /// Mouse is hovering the Idle overflow toggle row; the renderer
    /// brightens its text. Independent of [`Self::hovered_row`].
    pub hovered_idle_overflow: bool,
    /// When `true`, the Idle group shows every agent; when `false`
    /// (default) it caps to the `MAX_VISIBLE_IDLE` most-recent agents
    /// plus anything still inside the freshness window, folding the rest
    /// behind the overflow row. In-memory only (resets on pager restart),
    /// like section collapse.
    pub idle_show_all: bool,
    /// Pinned rows (persisted across pager restarts when their underlying
    /// agent still exists; stale ids are garbage-collected at open time).
    pub pinned: BTreeSet<DashboardRowId>,
    /// User reorderings — explicit position overrides applied AFTER the
    /// group + last_change sort.
    pub reorder: Vec<DashboardRowId>,
    /// Current grouping mode.
    pub grouping: Grouping,
    /// Current filter.
    pub filter: Filter,
    /// Search mode (toggled by `Ctrl+/`). While active the
    /// [`Self::dispatch`] buffer is interpreted as a live filter
    /// query rather than a prompt: every keystroke reparses into
    /// [`Self::filter`], the prompt prefix flips from `❯` to a
    /// yellow `Search:`, and Enter confirms (keeps the filter,
    /// leaves search) while Esc / `Ctrl+/` cancel (clear it).
    pub search_mode: bool,
    /// Dispatch / filter input widget.
    pub dispatch: PromptWidget,
    /// Effects queued by dashboard input (e.g. slash MRU persist after Tab accept).
    pub(crate) pending_effects: Vec<crate::app::actions::Effect>,
    /// In-flight deferred clipboard attachment probes for this dashboard. A send
    /// while `> 0` is stashed per surface so a paste-then-immediate-send never
    /// builds content blocks before the image attaches.
    pub(crate) paste_probe_in_flight: usize,
    /// A dispatch-input send deferred until the in-flight paste probe(s)
    /// complete; re-issued (with the now-updated widget text) on completion.
    pub(crate) deferred_dispatch_send: Option<DeferredDispatchSend>,
    /// A peek-reply send deferred the same way; per-surface slots so stashing
    /// one surface can never overwrite the other's pending send.
    pub(crate) deferred_peek_send: Option<DeferredPeekSend>,
    /// Peek panel state (Space toggles).
    pub peek: Option<PeekPanelState>,
    /// Session-scoped guest viewport for the live-tail peek (capture once
    /// on select; sticky while the same row is peeked; restore on leave).
    pub(crate) peek_viewport: Option<PeekViewportLease>,
    /// The peek panel's `❯ reply` input — a full [`PromptWidget`] so
    /// the reply gets paste chips (`[Pasted: N lines]`), word
    /// navigation, undo, and text selection exactly like [`Self::dispatch`].
    /// Owned here (NOT inside [`PeekPanelState`]) because a widget
    /// carries a fuzzy-file-matcher daemon thread and the panel struct
    /// is rebuilt whenever the selection cursor lands on a row. The
    /// draft is cleared when the peeked row changes or the panel
    /// closes (see [`Self::set_peek`]). `@` file completion is live
    /// (polled each tick, rooted at the peeked agent's cwd — see
    /// [`Self::peek_reply_cwd`]); slash completion stays inert because
    /// the dashboard never refreshes its snapshot for this widget.
    pub peek_reply: PromptWidget,
    /// Last frame's screen rect of the peek reply input (the `❯ reply`
    /// row, or the reject-feedback slot in question mode). Mirrors
    /// [`Self::dispatch_rect`]: consumed by mouse handling for
    /// click-to-focus and drag text selection. `None` while the peek
    /// is closed.
    pub peek_reply_rect: Option<Rect>,
    /// Directory the reply's `@` file-search daemon is currently rooted
    /// at. Tracked so [`Self::ensure_peek_reply_cwd`] can skip a
    /// `retarget` (which rebuilds the daemon thread) when the peeked
    /// agent's cwd hasn't actually changed. `None` = the construction
    /// default (`.`); set to the launch cwd at dashboard open.
    peek_reply_cwd: Option<PathBuf>,
    /// Cwd of the currently-peeked agent, recorded by the render pass
    /// (which has the agents map). Applied lazily to the reply's `@`
    /// picker on first compose — NOT on every cursor move — so merely
    /// navigating past agents in other directories spawns no matcher
    /// threads. `None` when no row is peeked or the row has no local
    /// agent (roster).
    peek_reply_target_cwd: Option<PathBuf>,
    /// Inline-rename state for `Ctrl+R` — `Some` when the cursor is on a
    /// row and the user is mid-edit.
    pub rename: Option<RenameDraft>,
    /// Pending dispatch feedback toast (e.g. "✗ Session no longer
    /// exists"). Rendered verbatim by `paint_dispatch_feedback_badge`;
    /// error messages are built via [`Self::set_error_toast`].
    pub error_toast: Option<String>,
    /// Pending stop confirmation. `Some((row, set_at))` after the first
    /// `Ctrl+X` press on a top-level row. The second press within
    /// [`STOP_CONFIRM_WINDOW`] closes the agent. Mirrors the session-close
    /// close-confirm pattern.
    pub stop_confirm: Option<(DashboardRowId, Instant)>,
    /// Tick counter for spinner animation. The
    /// counter is bumped by [`crate::app::app_view::AppView::tick`]
    /// (NOT the renderer, which is read-only).
    /// `SPINNER_DIVISOR` divides the index so the on-screen
    /// animation stays under 10 Hz at the ~30 Hz tick rate.
    pub spinner_tick: u64,
    /// Last frame's row layout — hit areas keyed by row id. Used by
    /// mouse handling to map (col, row) → row id without scanning the
    /// row list a second time.
    pub row_rects: Vec<(DashboardRowId, Rect)>,
    /// Last frame's section-header hit areas keyed by [`SectionKey`].
    /// Used by mouse handling to map (col, row) → section for
    /// click-to-toggle and hover. Rebuilt every render.
    pub section_rects: Vec<(SectionKey, Rect)>,
    /// Last frame's hit area for the Idle "N more" overflow
    /// toggle row, when painted. Consumed by mouse handling for
    /// click-to-toggle and hover. `None` when the Idle group isn't capped.
    pub idle_overflow_rect: Option<Rect>,
    /// Last frame's content-area rect for resize-aware peek toggling.
    pub last_area: Rect,
    /// When `Some(id)`, an agent conversation is
    /// attached as a popup overlay on top of the dashboard. The
    /// agent's full view (scrollback + prompt) renders inside the
    /// popup rect; input is routed to that agent (Esc closes the
    /// popup, returning to the dashboard rows). For subagent
    /// attaches, the parent agent's `active_subagent` field is also
    /// set so the subagent's child view renders within the parent's
    /// own subagent takeover.
    pub attached_agent: Option<AgentId>,
    /// Hit rect for the popup's `[✗]` close
    /// affordance. Populated by the renderer when the popup chrome
    /// is painted; consumed by `handle_mouse` to dispatch a popup
    /// close on click. `None` when no popup is open or the popup
    /// is too small for the affordance.
    pub popup_close_rect: Option<Rect>,
    /// Outer rect of the popup overlay (border
    /// included). Populated by the renderer; consumed by
    /// `handle_mouse` to:
    ///   - swallow clicks that land on the popup chrome (border /
    ///     title row) instead of dispatching dashboard actions, and
    ///   - dispatch `DashboardAttach(clicked_row)` when the user
    ///     clicks a dashboard row OUTSIDE this rect (switches the
    ///     popup target to the clicked row).
    pub popup_outer_rect: Option<Rect>,
    /// Hit area for the header's `[+ New Agent]`
    /// button. Painted by `render_header` and consumed by the mouse
    /// handler to create a new session. Carries both the rect (for
    /// click hit-testing) and a `hovered` flag (driven by mouse-move
    /// events; the renderer reads it to paint a hover highlight). The
    /// rect is `None` when the header is too narrow to fit the button
    /// or has zero area.
    pub new_agent_button_hit: crate::app::agent_view::HitArea,
    /// Items-area rect of the open slash-completion dropdown (set by
    /// `render_slash_dropdown`). Lets the mouse handler route a wheel /
    /// click inside the dropdown to the completion list rather than the
    /// row list. `None` when the dropdown is closed.
    pub slash_dropdown_items_area: Option<Rect>,
    /// Slash dropdown hit-test state from the renderer (see
    /// [`crate::views::slash_dropdown::RenderedDropdown`]).
    pub slash_dropdown_hit: crate::views::slash_dropdown::RenderedDropdown,
    /// Mirror of [`Self::slash_dropdown_items_area`] for the session-less
    /// `@` file-context picker dropdown (set by
    /// `render_file_search_dropdown`).
    pub file_search_dropdown_items_area: Option<Rect>,
    /// Two-focus model: `false` = the dispatch **input bar** is focused
    /// (typing); `true` = the **overview list** is focused (navigating).
    /// Tab toggles the two. When the list is focused, `j`/`k` (vim) and
    /// the arrows move between agent rows, the input dims + hides its
    /// caret, and printable keys hand focus back to the input. This is
    /// the explicit vim-style focus separation; the cursor itself
    /// (`selected` row / `new_agent_button_focused`) is shared.
    pub list_focused: bool,
    /// When an agent is attached from the dashboard
    /// (Enter on a row), the agent's view paints inside a bordered
    /// "session overlay" frame, similar to the subagent fullscreen
    /// takeover. These hit areas are populated by the overlay's
    /// renderer and consumed by the mouse handler — each carries
    /// both the rect (for click hit-testing) and a `hovered` flag
    /// (driven by mouse-move events; the renderer reads it to
    /// paint a hover highlight).
    ///
    /// - `overlay_close_hit` → `[✗]` close affordance
    /// - `overlay_prev_hit` / `overlay_next_hit` → `[Prev]` /
    ///   `[Next]` affordances that cycle through the dashboard's
    ///   visible row list
    ///
    /// All three are cleared by `close_popup` / `exit_overlay` so
    /// they can't outlive the overlay state.
    pub overlay_close_hit: crate::app::agent_view::HitArea,
    pub overlay_prev_hit: crate::app::agent_view::HitArea,
    pub overlay_next_hit: crate::app::agent_view::HitArea,
    /// Last mouse position (col, row). Used by hover and double-click
    /// detection.
    pub last_mouse_pos: Option<(u16, u16)>,
    /// Last mouse-down on a row + timestamp, used to detect a
    /// double-click as the "attach" gesture.
    pub last_click: Option<(DashboardRowId, Instant)>,
    /// Last mouse-down on dispatch / peek-reply (paste-chip double-click).
    pub last_prompt_click: Option<Instant>,
    /// Rect that closes the peek when clicked (`[×]`).
    pub peek_close_rect: Option<Rect>,
    /// Outer rect of the dispatch input box (set by `render_dashboard`
    /// when the box is painted). Consumed by the mouse handler so a
    /// click anywhere on the box focuses the input — i.e. clears
    /// `list_focused` — regardless of vim mode. `None` while peek mode
    /// or an attached-agent overlay replaces the input.
    pub dispatch_rect: Option<Rect>,
    /// Top of the visible row window. Clamped so the
    /// selected row stays visible. Wheel + PgUp/PgDn adjust this.
    pub viewport_offset: usize,
    /// True while the user is browsing the row list via the mouse
    /// wheel (or any pure-viewport scroll affordance). When set,
    /// [`Self::clamp_viewport`] skips its snap-to-selection
    /// pull-back so the viewport can travel past the selected row.
    /// Cleared on any selection-driven nav (arrow keys, click,
    /// filter rebuild, attach) so the viewport re-engages the
    /// selection follow.
    pub manual_scroll_active: bool,
    /// `Ctrl+.` opens a searchable cheatsheet of every action
    /// bound for the dashboard, mirroring the agent view's
    /// `ShortcutsHelp` modal. `Some` while the modal is open;
    /// input is routed to it before the dashboard's own
    /// handlers, and the renderer paints it on top of the row
    /// list. Cleared on close (Esc, `[✗]`, or the chrome's
    /// CloseRequested).
    pub shortcuts_modal: Option<Box<ShortcutsModalState>>,
    /// True when the header's `[+ New Agent]` button has focus.
    ///
    /// The button is the default selection target when no row is
    /// selected — Up-arrow from the first row, Esc deselect, and
    /// dashboard-open-without-prior-agent all land here. While
    /// focused, Enter with an empty prompt creates a session and
    /// opens detail view; click on the button does the same. The
    /// renderer paints the button with a brighter foreground so
    /// the cursor's location is obvious.
    ///
    /// Invariant: when `new_agent_button_focused == true`,
    /// `selected == None`. Any path that sets `selected` to
    /// `Some(_)` must also clear this flag — see
    /// [`Self::focus_row`] / [`Self::focus_new_agent_button`].
    pub new_agent_button_focused: bool,
    /// Model chosen for the *next* agent spawned from the dispatch input,
    /// set by `/model <name> [effort]` (intercepted in
    /// `dispatch_dashboard_dispatch_slash`). `None` → spawn on the default
    /// model. Sticky across dispatches; reset to `None` on every
    /// dashboard-open (alongside `pending_mode`).
    pub pending_model: Option<PendingDispatchModel>,
    /// Mode the next spawned agent starts in. Cycled with Shift+Tab and set
    /// by `/plan`. Sticky across dispatches; re-seeded from
    /// `app.default_yolo` on every dashboard-open (alongside `pending_model`).
    pub pending_mode: DashboardDispatchMode,
    /// Snapshot of the app-wide model catalog, seeded at dashboard-open so
    /// the session-less slash dropdown can suggest real model names for
    /// `/model`. Without this the dropdown would fall back to an empty
    /// `ModelState` and offer no completions.
    pub models: crate::acp::model_state::ModelState,
    /// Location picker modal — `Some` while open. Lets the user change the
    /// working directory new dashboard sessions spawn in. Opened via the
    /// header location label (click), `Ctrl+L`, or `/cd`; input is routed
    /// here before the dashboard's own handlers, and the renderer paints
    /// it on top of the row list. Reset on dashboard-open.
    pub location_picker: Option<LocationPickerState>,
    /// Hit area for the header's clickable location label. Painted by
    /// `render_header` and consumed by the mouse handler to open the
    /// location picker. `None` when the header is too narrow to paint it.
    pub location_hit: crate::app::agent_view::HitArea,
    /// Hit area for the header's promo upgrade CTA `[label]` button (click →
    /// `AnnouncementsOpenCta(Dashboard)`). `None` when no CTA is shown.
    pub upgrade_cta_hit: crate::app::agent_view::HitArea,
    /// A pinned (non-dismissible) promo CTA is live this frame (cached by
    /// `render_dashboard`); `Ctrl+O` opens it instead of falling through.
    pub pinned_upgrade_cta_live: bool,
    /// When `true`, agents dispatched from the dashboard are created in a
    /// fresh git worktree (rooted at the current cwd) instead of in the cwd
    /// directly: creating an agent first opens [`Self::worktree_dialog`] to
    /// collect a label. Toggled by the "worktree" button on the location
    /// picker's path row and persisted here so it survives the modal
    /// closing. Reset to `false` on every dashboard-open.
    pub dispatch_worktree: bool,
    /// Whether the current working directory is inside a git repo. Synced
    /// from `AppView::cwd_has_git_ancestor` on dashboard-open and on every
    /// location change. Worktrees require a git repo, so this gates the
    /// `Ctrl+W` worktree toggle and forces [`Self::dispatch_worktree`] off
    /// outside a repo (the dashboard is never in worktree mode in a non-git
    /// directory).
    pub cwd_has_git_ancestor: bool,
    /// Working directory new sessions dispatch from — a snapshot of
    /// `AppView::cwd`, synced on dashboard-open and on every location change.
    /// The header renders from this (not the process cwd) so it tracks a
    /// `/cd` even before `Effect::SetWorkingDir` lands, or if it fails.
    pub cwd: PathBuf,
    /// Worktree-label dialog — `Some` while the user is naming a worktree
    /// for a dashboard-dispatched agent. Reuses the welcome screen's
    /// [`NewWorktreeDialogState`](crate::app::app_view::NewWorktreeDialogState)
    /// widget; input is routed here and the renderer overlays it.
    pub worktree_dialog: Option<crate::app::app_view::NewWorktreeDialogState>,
    /// Prompt state stashed while [`Self::worktree_dialog`] is open.
    pub pending_worktree_prompt: Option<crate::views::prompt_widget::StashedPrompt>,
    /// Whether confirming the in-flight [`Self::worktree_dialog`] should open
    /// the new agent's detail view (`true` — the `[+ New Agent]` button and
    /// `Ctrl+S` "send + open") or stay on the dashboard (`false` — a plain
    /// `Enter` prompt-send). Stashed alongside [`Self::pending_worktree_prompt`]
    /// when the dialog opens; consumed on confirm. Mirrors the `attach` flag of
    /// the non-worktree dispatch path.
    pub pending_worktree_attach: bool,
    /// Mirror of `AppView::voice_listening`, synced each frame so the dispatch
    /// box can show a record badge + stream the interim transcript while voice
    /// dictation targets the dashboard's new-agent input.
    pub voice_listening: bool,
    /// Mirror of `AppView::voice_interim` — the live partial transcript.
    pub voice_interim: Option<String>,
    /// Surface-local compose mode for dispatch + peek (not persisted; not
    /// shared with agent sessions). `/multiline` or Ctrl+M.
    pub multiline_mode: bool,
}

/// Mode staged for the next agent the dashboard spawns. Mirrors the agent
/// view's Shift+Tab cycle (Normal → Plan → Always-Approve → Normal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardDispatchMode {
    #[default]
    Normal,
    Plan,
    AlwaysApprove,
}

impl DashboardDispatchMode {
    /// Advance to the next mode in the Shift+Tab rotation.
    pub fn cycle(self) -> Self {
        match self {
            Self::Normal => Self::Plan,
            Self::Plan => Self::AlwaysApprove,
            Self::AlwaysApprove => Self::Normal,
        }
    }
}

/// A model (and optional reasoning effort) staged for the next agent the
/// dashboard spawns. `display` is the human-readable model name, stored so
/// the renderer can show the indicator without a live `ModelState`.
#[derive(Debug, Clone)]
pub struct PendingDispatchModel {
    pub id: agent_client_protocol::ModelId,
    pub effort: Option<xai_grok_shell::sampling::types::ReasoningEffort>,
    pub display: String,
}

/// Snapshot of the shortcuts cheatsheet modal — entries, picker /
/// chrome state, and the user's filter / collapse preferences. The
/// fields mirror `crate::views::modal::ActiveModal::ShortcutsHelp`
/// so the existing `views::shortcuts_help::*` helpers (build,
/// filter, render-input) work without re-plumbing. No `Debug` /
/// `Clone` derive — `ShortcutsHelpEntry` lacks both, and this
/// state is owned in-place by `DashboardState` without cloning
/// anywhere.
pub struct ShortcutsModalState {
    pub entries: Vec<crate::views::shortcuts_help::ShortcutsHelpEntry>,
    pub state: crate::views::picker::PickerState,
    pub window: crate::views::modal_window::ModalWindowState,
    pub filter_active: bool,
    pub collapsed_sections: std::collections::HashSet<usize>,
    pub expanded_ids: std::collections::HashSet<crate::views::shortcuts_help::ExpandKey>,
    pub mode: crate::views::shortcuts_help::ShortcutsHelpMode,
}

/// In-progress rename for `Ctrl+R`.
#[derive(Debug, Clone)]
pub struct RenameDraft {
    pub row: DashboardRowId,
    editor: LineEditor,
}

const MAX_RENAME_SCALARS: usize = 100;

impl RenameDraft {
    pub fn new(row: DashboardRowId, text: impl Into<String>) -> Self {
        let mut draft = Self {
            row,
            editor: LineEditor::default(),
        };
        draft.set_text(text);
        draft
    }

    pub fn text(&self) -> &str {
        self.editor.text()
    }

    pub fn cursor_byte(&self) -> usize {
        self.editor.cursor_byte()
    }

    pub(crate) fn viewport(&self, width: usize) -> xai_ratatui_textarea::SingleLineViewport {
        self.editor.viewport(width)
    }

    pub(crate) fn set_text(&mut self, text: impl Into<String>) {
        let text = text
            .into()
            .chars()
            .filter(|character| rename_wire_character_allowed(*character))
            .take(MAX_RENAME_SCALARS)
            .collect::<String>();
        self.editor.set_text(text);
    }
}

fn rename_character_allowed(character: char) -> bool {
    !crate::render::line_utils::is_unsafe_display_char(character)
}

fn rename_wire_character_allowed(character: char) -> bool {
    // Preserve an existing emoji ZWJ sequence; interactive inserts still reject format chars.
    character == '\u{200d}' || rename_character_allowed(character)
}

/// One selectable directory in the location picker (see
/// [`LocationPickerState`]). `path` is the absolute directory; `label` /
/// `detail` are the pre-formatted display strings (basename, `~/display`
/// path, relative time, `(current)` marker). `worktree` is the managed
/// worktree's human label when this directory is a worktree root, used to
/// render a styled badge.
#[derive(Debug, Clone)]
pub struct LocationCandidate {
    pub path: PathBuf,
    pub label: String,
    pub detail: String,
    pub worktree: Option<String>,
}

/// Max directory entries listed per parent (bounds the cost of a
/// `readdir` on a huge directory) and max rows shown to the picker.
const LOCATION_DIR_LISTING_CAP: usize = 1000;
const LOCATION_VISIBLE_CAP: usize = 200;

/// State for the dashboard's location picker modal (change the directory new
/// sessions spawn in).
///
/// Hybrid autocomplete: a non-path query fuzzy-filters [`Self::recents`];
/// once the query looks like a path (`/`, `~`, or contains `/`) it switches
/// to shell-style directory completion over the `readdir` of its parent.
pub struct LocationPickerState {
    pub picker: crate::views::picker::PickerState,
    pub window: crate::views::modal_window::ModalWindowState,
    /// Static suggestions shown when the query isn't a path: current cwd
    /// first, then recent project dirs.
    pub recents: Vec<LocationCandidate>,
    /// Base directory for resolving relative typed paths (the pager cwd
    /// at open time).
    pub base_cwd: PathBuf,
    /// Worktree root path → label index, built once at open. Used to tag
    /// recents and directory suggestions that are managed worktrees.
    worktrees: std::collections::HashMap<PathBuf, String>,
    /// Cached subdirectories of the current path-mode parent.
    dir_listing: Vec<LocationCandidate>,
    /// The parent directory [`Self::dir_listing`] was read from. Used to
    /// skip the `readdir` when only the final (partial) segment changes.
    dir_listing_parent: Option<PathBuf>,
    /// Content-row hit areas from the last render, for mouse click /
    /// hover. `None` until the modal renders once.
    pub content_hits: Option<crate::views::picker::PickerContentHitAreas>,
    /// Inline error (e.g. "Not a directory") shown under the title when a
    /// chosen path fails to resolve. Cleared when the modal re-opens.
    pub error: Option<String>,
    /// When `true`, applying the chosen location also arms worktree mode on
    /// the dashboard (`DashboardState::dispatch_worktree`), so the next
    /// dispatched agent spawns in a fresh worktree. Toggled by the path-row
    /// "worktree" button; seeded from the dashboard flag on open.
    pub worktree_mode: bool,
    /// Hit area (rect + hover) of the path-row "worktree" toggle button.
    /// The rect is set during render (cleared when the button is hidden —
    /// modal too narrow, or the target isn't a repo); `hovered` is driven by
    /// the modal mouse handler so the button can brighten on hover.
    pub worktree_hit: crate::app::agent_view::HitArea,
    /// Memoized git-repo check for the worktree toggle: `(dir, is_repo)`.
    /// Avoids re-`stat`ing on every render frame while the selection /
    /// query (and thus the target directory) is unchanged.
    worktree_repo_cache: Option<(PathBuf, bool)>,
}

impl LocationPickerState {
    /// Build a fresh picker over the static `recents` list, resolving
    /// relative typed paths against `base_cwd`. `worktrees` is the
    /// root-path → label index used to tag worktree directories. The
    /// search field is active so the cursor shows immediately and a path
    /// can be typed.
    pub fn new(
        recents: Vec<LocationCandidate>,
        base_cwd: PathBuf,
        worktrees: std::collections::HashMap<PathBuf, String>,
    ) -> Self {
        let picker = crate::views::picker::PickerState::input_active();
        Self {
            picker,
            window: crate::views::modal_window::ModalWindowState::new(),
            recents,
            base_cwd,
            worktrees,
            dir_listing: Vec::new(),
            dir_listing_parent: None,
            content_hits: None,
            error: None,
            worktree_mode: false,
            worktree_hit: crate::app::agent_view::HitArea::default(),
            worktree_repo_cache: None,
        }
    }

    /// Whether `dir` is inside a git repo (worktrees require one), memoized
    /// so navigating the list doesn't re-`stat` on every render frame.
    pub fn target_is_repo(&mut self, dir: &Path) -> bool {
        if let Some((cached, is_repo)) = &self.worktree_repo_cache
            && cached == dir
        {
            return *is_repo;
        }
        let is_repo = dir.ancestors().any(|p| p.join(".git").exists());
        self.worktree_repo_cache = Some((dir.to_path_buf(), is_repo));
        is_repo
    }

    /// Whether the current query should be treated as a filesystem path
    /// (directory completion) rather than a fuzzy filter over recents.
    pub fn query_is_path(&self) -> bool {
        let q = self.picker.query();
        q.starts_with('/')
            || q.starts_with('~')
            || q.contains('/')
            // Windows: native absolute paths (`C:\…`) and backslash
            // separators. Gated so a literal backslash in a Unix filename
            // never forces path mode on non-Windows hosts.
            || (cfg!(windows) && (q.contains('\\') || has_windows_drive_prefix(q)))
    }

    /// Split a path-mode query into the `(parent_dir, partial_name)` to
    /// complete: everything up to the last separator resolves to a directory,
    /// and the trailing segment is the prefix to match. `~` expands to
    /// home; relative parents join [`Self::base_cwd`]. The separator is `/`
    /// on all hosts and additionally `\` on Windows.
    fn path_query_parts(&self) -> (PathBuf, String) {
        let q = self.picker.query();
        // Last path separator: `/` always; `\` additionally on Windows.
        let sep = match (q.rfind('/'), cfg!(windows).then(|| q.rfind('\\')).flatten()) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        match sep {
            Some(i) => {
                let parent = resolve_dir_prefix(&q[..=i], &self.base_cwd);
                (parent, q[i + 1..].to_string())
            }
            // No separator: a bare `~` (or `~name`) lists home; on Windows a
            // bare drive (`C:`) lists that drive's root.
            None => {
                if cfg!(windows) && has_windows_drive_prefix(q) && q.len() == 2 {
                    (PathBuf::from(format!("{q}\\")), String::new())
                } else {
                    let home = dirs::home_dir().unwrap_or_else(|| self.base_cwd.clone());
                    (home, q.trim_start_matches('~').to_string())
                }
            }
        }
    }

    /// Re-`readdir` the path-mode parent when it changes. Cheap no-op when
    /// the query isn't a path or the parent is unchanged (typing within a
    /// directory only re-filters the cached listing).
    pub fn refresh_suggestions(&mut self) {
        if !self.query_is_path() {
            if self.dir_listing_parent.is_some() {
                self.dir_listing.clear();
                self.dir_listing_parent = None;
            }
            return;
        }
        let (parent, _partial) = self.path_query_parts();
        if self.dir_listing_parent.as_deref() == Some(parent.as_path()) {
            return;
        }
        self.dir_listing = read_subdirs(&parent, &self.worktrees);
        self.dir_listing_parent = Some(parent);
        self.picker.selected = 0;
        self.picker.scroll_offset = None;
    }

    /// The effective list shown + selected from, given the current query.
    /// Path mode → cached subdirs prefix-matched against the partial
    /// (dot-directories hidden unless the partial starts with `.`); recents
    /// mode → the static list fuzzy-filtered by substring. Capped at
    /// [`LOCATION_VISIBLE_CAP`] rows.
    pub fn visible_candidates(&self) -> Vec<LocationCandidate> {
        if self.query_is_path() {
            let (_parent, partial) = self.path_query_parts();
            let pl = partial.to_lowercase();
            let want_hidden = partial.starts_with('.');
            self.dir_listing
                .iter()
                .filter(|c| {
                    if !want_hidden && c.label.starts_with('.') {
                        return false;
                    }
                    pl.is_empty() || c.label.to_lowercase().starts_with(&pl)
                })
                .take(LOCATION_VISIBLE_CAP)
                .cloned()
                .collect()
        } else {
            let q = self.picker.query().trim().to_lowercase();
            self.recents
                .iter()
                .filter(|c| {
                    q.is_empty()
                        || c.label.to_lowercase().contains(&q)
                        || c.detail.to_lowercase().contains(&q)
                        || c.path.to_string_lossy().to_lowercase().contains(&q)
                })
                .take(LOCATION_VISIBLE_CAP)
                .cloned()
                .collect()
        }
    }

    /// Resolve the path to apply on Enter: the selected visible row, else
    /// the raw typed query (so a path with no matching suggestion still
    /// navigates and validates downstream). `None` when nothing applies.
    pub fn chosen_input(&self) -> Option<String> {
        let visible = self.visible_candidates();
        if let Some(c) = visible.get(self.picker.selected) {
            return Some(c.path.to_string_lossy().into_owned());
        }
        let q = self.picker.query().trim();
        if !q.is_empty() {
            return Some(q.to_string());
        }
        None
    }
}

/// `true` when `s` starts with a Windows drive prefix like `C:`. Used only
/// under `cfg!(windows)` to route native absolute paths into path mode.
fn has_windows_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Resolve a path-prefix ending in a separator (e.g. `~/src/`, `/etc/`,
/// `../`, or on Windows `C:\Users\`) to an absolute directory. `~` expands
/// to home; relative prefixes join `base`.
fn resolve_dir_prefix(prefix: &str, base: &Path) -> PathBuf {
    // Strip trailing separators — `/` always, `\` additionally on Windows.
    let trimmed = if cfg!(windows) {
        prefix.trim_end_matches(['/', '\\'])
    } else {
        prefix.trim_end_matches('/')
    };
    if prefix.starts_with('/') {
        if trimmed.is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(trimmed)
        }
    } else if cfg!(windows) && Path::new(prefix).is_absolute() {
        // Windows drive-absolute (`C:\…`) or UNC. Keep the root separator for
        // a bare drive (`C:\` trims to `C:`, which is drive-RELATIVE without it).
        if has_windows_drive_prefix(trimmed) && trimmed.len() == 2 {
            PathBuf::from(format!("{trimmed}\\"))
        } else {
            PathBuf::from(trimmed)
        }
    } else if trimmed == "~" || prefix.starts_with("~/") {
        let rest = trimmed.trim_start_matches('~').trim_start_matches('/');
        let mut home = dirs::home_dir().unwrap_or_else(|| base.to_path_buf());
        if !rest.is_empty() {
            home.push(rest);
        }
        home
    } else if trimmed.is_empty() {
        base.to_path_buf()
    } else {
        base.join(trimmed)
    }
}

/// List the immediate subdirectories of `parent` as location candidates,
/// sorted case-insensitively by name and capped at
/// [`LOCATION_DIR_LISTING_CAP`]. Returns empty for an unreadable parent.
/// Subdirectories that are managed worktree roots are tagged from the
/// `worktrees` index. The lookup key is the entry's fully canonicalized
/// path (matching how the index keys + recent rows are built), so a
/// symlinked worktree directory still matches. The canonicalization is
/// skipped entirely when there are no worktrees to match against.
fn read_subdirs(
    parent: &Path,
    worktrees: &std::collections::HashMap<PathBuf, String>,
) -> Vec<LocationCandidate> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<LocationCandidate> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // `path.is_dir()` follows symlinks so symlinked dirs are offered.
        if path.is_dir() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Match the index's canonical keys; resolve symlinks on the
            // entry itself (a lexical `parent_canon.join(name)` would not).
            let worktree = if worktrees.is_empty() {
                None
            } else {
                let canon = dunce::canonicalize(&path).unwrap_or_else(|_| path.clone());
                worktrees.get(&canon).cloned()
            };
            out.push(LocationCandidate {
                label: name,
                detail: crate::project_picker::sources::display_path(&path),
                path,
                worktree,
            });
            if out.len() >= LOCATION_DIR_LISTING_CAP {
                break;
            }
        }
    }
    out.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    out
}

/// Per-call config for the location picker's shared-picker input handler.
/// Search is always active (no `/` to start), search is not disabled, and
/// rows are flat (no expand / tabs / filter / action keys).
fn location_picker_config<'a>() -> crate::views::picker::PickerConfig<'a> {
    crate::views::picker::PickerConfig {
        title: None,
        show_search_hint: false,
        expandable: false,
        esc_clears_query: false,
        shortcuts: None,
        pending_hint: None,
        shortcuts_area: None,
        non_selectable: &[],
        non_selectable_clickable: &[],
        tabs: None,
        active_tab: 0,
        filter_label: None,
        filter_key_hint: None,
        filter_active: false,
        action_keys: &[],
        disable_search: false,
        compact_bottom_bar: false,
        search_only_on_slash: false,
        vim_normal_first: crate::appearance::cache::load_vim_mode(),
    }
}

/// Resolve session ids → live `DashboardRowId`s and back.
///
/// Built once at dashboard-open time from `app.agents`. Embodies
/// the fix: pin/reorder persistence keys are stable session
/// ids (not per-process `AgentId`), so a restart resolves them
/// against the new process's agents instead of attaching to whatever
/// happens to have the same `AgentId(usize)`.
pub struct SessionIdResolver {
    /// session_id → AgentId (top-level).
    top: std::collections::HashMap<String, crate::app::agent::AgentId>,
    /// agent_session_id → set of (child_session_id, AgentId-of-parent).
    subs: std::collections::HashMap<
        String,
        std::collections::HashMap<String, crate::app::agent::AgentId>,
    >,
    /// Reverse: AgentId → session_id (for `to_persisted`).
    top_rev: std::collections::HashMap<crate::app::agent::AgentId, String>,
}

impl SessionIdResolver {
    /// Build from the live agents map.
    ///
    /// Collisions on `session_id` (two agents sharing
    /// the same ACP session id; rare but possible via resume / fork
    /// re-use) are detected at table-build time. The first mapping
    /// wins; the colliding entry is logged via `tracing::warn!` and
    /// dropped. Resolve queries for a collided id therefore return
    /// the first-seen `AgentId`, which is deterministic given the
    /// `IndexMap` iteration order.
    pub fn from_agents(
        agents: &indexmap::IndexMap<crate::app::agent::AgentId, crate::app::agent_view::AgentView>,
    ) -> Self {
        let mut top = std::collections::HashMap::new();
        let mut top_rev = std::collections::HashMap::new();
        let mut subs: std::collections::HashMap<
            String,
            std::collections::HashMap<String, crate::app::agent::AgentId>,
        > = std::collections::HashMap::new();
        for (id, agent) in agents {
            if let Some(sid) = agent.session.session_id.as_ref() {
                let sid_str = sid.0.to_string();
                if let Some(prev) = top.get(&sid_str) {
                    // Top-level collision: keep first mapping.
                    // And call out the subagents we're
                    // dropping by name so a debugger can correlate the
                    // missing children back to the losing parent.
                    let dropped: Vec<&str> =
                        agent.subagent_sessions.keys().map(String::as_str).collect();
                    tracing::warn!(
                        session_id = %sid_str,
                        first = ?prev,
                        duplicate = ?id,
                        dropped_subagents = ?dropped,
                        "SessionIdResolver: duplicate session_id; keeping first mapping (subagents of losing parent are dropped)"
                    );
                } else {
                    top.insert(sid_str.clone(), *id);
                    top_rev.insert(*id, sid_str.clone());
                    // Populate the subs map via the
                    // entry()/or_insert_with() API so the "first wins"
                    // semantics are explicit. Defensive: a colliding
                    // child_session_id within a single parent's map
                    // would also warn rather than silently overwrite.
                    let children = subs.entry(sid_str.clone()).or_default();
                    for child_sid in agent.subagent_sessions.keys() {
                        if let Some(prev_child) = children.get(child_sid) {
                            tracing::warn!(
                                parent_session_id = %sid_str,
                                child_session_id = %child_sid,
                                first = ?prev_child,
                                duplicate = ?id,
                                "SessionIdResolver: duplicate child session_id under same parent; keeping first mapping"
                            );
                        } else {
                            children.insert(child_sid.clone(), *id);
                        }
                    }
                    if children.is_empty() {
                        subs.remove(&sid_str);
                    }
                }
            }
        }
        Self { top, subs, top_rev }
    }

    /// Empty resolver — used when no agents have been loaded yet.
    /// Resolves nothing, persists nothing.
    pub fn empty() -> Self {
        Self {
            top: std::collections::HashMap::new(),
            subs: std::collections::HashMap::new(),
            top_rev: std::collections::HashMap::new(),
        }
    }

    pub fn resolve(&self, pid: &PersistedRowId) -> Option<DashboardRowId> {
        match pid {
            PersistedRowId::TopLevel { session_id } => self
                .top
                .get(session_id)
                .copied()
                .map(DashboardRowId::TopLevel),
            PersistedRowId::Subagent {
                parent_session_id,
                child_session_id,
            } => self
                .subs
                .get(parent_session_id)
                .and_then(|m| m.get(child_session_id).copied())
                .map(|parent| DashboardRowId::Subagent {
                    parent,
                    child_session_id: child_session_id.clone(),
                }),
        }
    }

    pub fn to_persisted(&self, id: &DashboardRowId) -> Option<PersistedRowId> {
        match id {
            DashboardRowId::TopLevel(agent_id) => {
                self.top_rev
                    .get(agent_id)
                    .map(|sid| PersistedRowId::TopLevel {
                        session_id: sid.clone(),
                    })
            }
            DashboardRowId::Subagent {
                parent,
                child_session_id,
            } => {
                let parent_sid = self.top_rev.get(parent)?.clone();
                Some(PersistedRowId::Subagent {
                    parent_session_id: parent_sid,
                    child_session_id: child_session_id.clone(),
                })
            }
            // Roster-only rows are ephemeral (not locally hosted) and are
            // never persisted across restarts.
            DashboardRowId::Roster { .. } => None,
        }
    }
}

impl Default for DashboardState {
    fn default() -> Self {
        Self::new()
    }
}

impl DashboardState {
    /// Construct an empty dashboard state. Persistence is applied via
    /// [`Self::apply_persisted`] right after `new()` at the open site.
    pub fn new() -> Self {
        // The dashboard is session-less: its dispatch input offers only
        // pager-global slash commands (hide session-scoped ones such as
        // /compact, /fork, /rewind — they make no sense with no session).
        let mut dispatch = PromptWidget::new();
        dispatch.hide_session_scoped_commands();
        // The peek reply is a quick single-row input — compact mode
        // lowers the `[Pasted: N lines]` chip threshold to 2 lines so
        // multi-line pastes fold instead of overflowing the row.
        let mut peek_reply = PromptWidget::new();
        peek_reply.set_compact(true);
        Self {
            selected: None,
            hovered_row: None,
            selected_section: None,
            hovered_section: None,
            // The "Inactive" section (roster-only idle/dormant sessions
            // owned by OTHER pager processes — see `RowState::Inactive`)
            // is background noise relative to the sessions you're actively
            // cycling between, so it starts collapsed: the actionable
            // groups (Awaiting / Working / Idle) own the viewport on open.
            // Collapse isn't persisted to disk, so expanding it survives
            // reopen for this process lifetime but resets to collapsed on
            // the next pager start — i.e. collapsed "by default".
            collapsed_sections: std::iter::once(SectionKey::State(RowState::Inactive)).collect(),
            selected_idle_overflow: false,
            hovered_idle_overflow: false,
            idle_show_all: false,
            pinned: BTreeSet::new(),
            reorder: Vec::new(),
            grouping: Grouping::default(),
            filter: Filter::None,
            search_mode: false,
            dispatch,
            pending_effects: Vec::new(),
            paste_probe_in_flight: 0,
            deferred_dispatch_send: None,
            deferred_peek_send: None,
            peek: None,
            peek_viewport: None,
            peek_reply,
            peek_reply_rect: None,
            peek_reply_cwd: None,
            peek_reply_target_cwd: None,
            rename: None,
            error_toast: None,
            stop_confirm: None,
            spinner_tick: 0,
            row_rects: Vec::new(),
            section_rects: Vec::new(),
            idle_overflow_rect: None,
            last_area: Rect::default(),
            attached_agent: None,
            popup_close_rect: None,
            popup_outer_rect: None,
            new_agent_button_hit: crate::app::agent_view::HitArea::default(),
            slash_dropdown_items_area: None,
            slash_dropdown_hit: Default::default(),
            file_search_dropdown_items_area: None,
            list_focused: false,
            overlay_close_hit: crate::app::agent_view::HitArea::default(),
            overlay_prev_hit: crate::app::agent_view::HitArea::default(),
            overlay_next_hit: crate::app::agent_view::HitArea::default(),
            last_mouse_pos: None,
            last_click: None,
            last_prompt_click: None,
            peek_close_rect: None,
            dispatch_rect: None,
            viewport_offset: 0,
            manual_scroll_active: false,
            shortcuts_modal: None,
            pending_model: None,
            pending_mode: DashboardDispatchMode::Normal,
            models: crate::acp::model_state::ModelState::default(),
            location_picker: None,
            location_hit: crate::app::agent_view::HitArea::default(),
            upgrade_cta_hit: crate::app::agent_view::HitArea::default(),
            pinned_upgrade_cta_live: false,
            dispatch_worktree: false,
            cwd_has_git_ancestor: false,
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            worktree_dialog: None,
            pending_worktree_prompt: None,
            pending_worktree_attach: false,
            voice_listening: false,
            voice_interim: None,
            multiline_mode: false,
            // Fresh dashboard with no rows seeded → the `[+ New
            // Agent]` button is the default cursor target. Open
            // sites that want a specific row seeded call
            // `focus_row` after construction, which clears this
            // flag atomically.
            new_agent_button_focused: true,
        }
    }

    /// Adopt the shared slash MRU store (owned by `AppView`) into both the
    /// dispatch input and the peek-reply input so dashboard slash completion
    /// shares command recency with agent prompts.
    pub(crate) fn adopt_slash_mru(
        &mut self,
        mru: std::rc::Rc<std::cell::RefCell<crate::slash::mru::SlashMru>>,
    ) {
        self.dispatch.adopt_slash_mru(mru.clone());
        self.peek_reply.adopt_slash_mru(mru);
    }

    pub(crate) fn set_screen_mode(&mut self, mode: crate::app::ScreenMode) {
        self.dispatch.set_screen_mode(mode);
        self.peek_reply.set_screen_mode(mode);
    }

    pub(crate) fn set_recap_visible(&mut self, visible: bool) {
        self.dispatch.set_recap_visible(visible);
        self.peek_reply.set_recap_visible(visible);
    }

    pub(crate) fn set_voice_visible(&mut self, visible: bool) {
        self.dispatch.set_voice_visible(visible);
        self.peek_reply.set_voice_visible(visible);
    }

    /// Gate `/auto` on both dashboard prompt registries (dispatch + peek
    /// reply). See [`crate::slash::SlashController::set_auto_mode_available`].
    pub(crate) fn set_auto_mode_available(&mut self, available: bool) {
        self.dispatch.set_auto_mode_available(available);
        self.peek_reply.set_auto_mode_available(available);
    }

    /// Replace the restricted slash-command deny list on both dashboard
    /// prompt registries (dispatch + peek reply).
    pub(crate) fn set_restricted_commands(&mut self, names: &[String]) {
        self.dispatch.set_restricted_commands(names);
        self.peek_reply.set_restricted_commands(names);
    }

    /// Set the dispatch feedback slot to an error `msg`, prefixed with
    /// the error glyph (`✗`, or `x` on legacy consoles). The badge
    /// (`paint_dispatch_feedback_badge`) paints the slot VERBATIM in a
    /// neutral colour, so this leading glyph is what marks the message
    /// as an error. Use for error messages without a glyph of their own
    /// (fixed literals, slash-command `CommandResult::Error` strings);
    /// pass-through messages that already carry their own glyph (e.g.
    /// `show_toast` builders, slash-command `CommandResult::Message`
    /// results) must be assigned to `error_toast` directly.
    pub(crate) fn set_error_toast(&mut self, msg: &str) {
        self.error_toast = Some(format!("{} {msg}", crate::glyphs::ballot_x()));
    }

    /// Focus the header's `[+ New Agent]` button. Clears any
    /// row selection so the "button focused → no row selected"
    /// invariant stays honoured. Idempotent — safe to call when
    /// the button is already focused.
    pub fn focus_new_agent_button(&mut self) {
        self.new_agent_button_focused = true;
        self.selected = None;
        self.selected_section = None;
        self.selected_idle_overflow = false;
    }

    /// Focus the row identified by `id`. Clears the
    /// `new_agent_button_focused` flag so the two cursor states
    /// stay mutually exclusive. Any caller that mutates
    /// `selected` directly bypasses this helper at its own
    /// risk — the invariant only holds when both fields are
    /// written through here.
    pub fn focus_row(&mut self, id: DashboardRowId) {
        self.selected = Some(id);
        self.new_agent_button_focused = false;
        self.selected_section = None;
        self.selected_idle_overflow = false;
    }

    /// Focus the section header identified by `key` — the third cursor
    /// target alongside rows and the `[+ New Agent]` button. Clears the
    /// row selection and button focus so exactly one cursor is active.
    pub fn focus_section(&mut self, key: SectionKey) {
        self.selected_section = Some(key);
        self.selected = None;
        self.new_agent_button_focused = false;
        self.selected_idle_overflow = false;
    }

    /// Focus the Idle group's "N more" overflow toggle —
    /// the fourth cursor target. Clears the other three so exactly one
    /// cursor is active.
    pub fn focus_idle_overflow(&mut self) {
        self.selected_idle_overflow = true;
        self.selected = None;
        self.selected_section = None;
        self.new_agent_button_focused = false;
    }

    /// Toggle whether the Idle group shows every agent (`true`) or caps
    /// to the recent ones with the rest folded behind the overflow row
    /// (`false`). In-memory only — resets to capped on the next pager
    /// start. Mirrors the in-memory lifetime of section collapse.
    pub fn toggle_idle_show_all(&mut self) {
        self.idle_show_all = !self.idle_show_all;
    }

    /// Whether `key`'s section is currently collapsed (its rows hidden).
    pub fn is_section_collapsed(&self, key: SectionKey) -> bool {
        self.collapsed_sections.contains(&key)
    }

    /// Collapse (`true`) or expand (`false`) the section identified by
    /// `key`. Idempotent. Shared by the keyboard (Left/Right on a
    /// selected header) and the mouse (click on a header).
    pub fn set_section_collapsed(&mut self, key: SectionKey, collapsed: bool) {
        if collapsed {
            self.collapsed_sections.insert(key);
        } else {
            self.collapsed_sections.remove(&key);
        }
    }

    /// Toggle the collapsed state of the section identified by `key`.
    pub fn toggle_section(&mut self, key: SectionKey) {
        if !self.collapsed_sections.remove(&key) {
            self.collapsed_sections.insert(key);
        }
    }

    /// Enter search mode (`Ctrl+/`). The dispatch buffer becomes a
    /// live filter query and the prompt prefix flips to a yellow
    /// `Search:`. Starts fresh — clears any half-typed dispatch text
    /// and the prior filter so the query builds from empty.
    pub fn enter_search_mode(&mut self) {
        self.search_mode = true;
        self.dispatch.set_text("");
        self.filter = Filter::None;
        self.error_toast = None;
        self.manual_scroll_active = false;
    }

    /// Leave search mode and CANCEL: clears the filter and the query
    /// buffer, restoring the normal dispatch prompt. (Enter instead
    /// CONFIRMS — it keeps the filter applied and only flips
    /// `search_mode` off; see [`Self::handle_key`].)
    pub fn exit_search_mode(&mut self) {
        self.search_mode = false;
        self.dispatch.set_text("");
        self.filter = Filter::None;
        self.manual_scroll_active = false;
    }

    /// Construct from persisted state, resolving session-id keys to
    /// live `DashboardRowId`s via the given resolver. Unresolvable ids
    /// (sessions that have closed, or never existed) are dropped — see
    /// edge case 20.
    pub fn from_persisted(p: &PersistedDashboard, resolver: &SessionIdResolver) -> Self {
        let mut s = Self::new();
        s.apply_persisted(p, resolver);
        s
    }

    /// Apply persisted fields, resolving via the given session-id
    /// resolver. Idempotent — safe to call twice. Kept
    /// `pub(super)` because the only caller is `from_persisted`; the
    /// previous `pub` advertised a public contract the function
    /// doesn't quite satisfy (it clobbers all persistence-backed
    /// fields rather than merging).
    pub(super) fn apply_persisted(&mut self, p: &PersistedDashboard, resolver: &SessionIdResolver) {
        self.grouping = p.grouping;
        self.pinned = p
            .pinned
            .iter()
            .filter_map(|pid| resolver.resolve(pid))
            .collect();
        self.reorder = p
            .reorder
            .iter()
            .filter_map(|pid| resolver.resolve(pid))
            .collect();
    }

    /// Snapshot back to a persistable form. Top-level rows whose
    /// underlying agent does not yet have a `session_id` are dropped
    /// (they can't be persisted yet — but they'll remain in the
    /// in-memory state for the rest of this process lifetime).
    pub fn to_persisted(&self, enabled: bool, resolver: &SessionIdResolver) -> PersistedDashboard {
        PersistedDashboard {
            enabled,
            grouping: self.grouping,
            pinned: self
                .pinned
                .iter()
                .filter_map(|id| resolver.to_persisted(id))
                .collect(),
            reorder: self
                .reorder
                .iter()
                .filter_map(|id| resolver.to_persisted(id))
                .collect(),
        }
    }

    /// Garbage-collect stale row ids from `pinned` / `reorder`.
    ///
    /// Called on dashboard open: any persisted id whose underlying
    /// agent/subagent no longer exists is silently dropped. Avoids edge
    /// case 20 (a pinned row whose agent was deleted).
    ///
    /// Also clears an in-flight `rename` whose row
    /// disappeared (parent closed, subagent finished, etc.), so a
    /// Commit-Enter doesn't silently drop the draft on a phantom row.
    ///
    /// Extends the gc to `peek`, `hovered_row`, and
    /// `last_click`. The previous version only cleared `pinned`,
    /// `reorder`, `rename`, and `selected`. A stale `peek` would
    /// render cached content for a dead row; a stale `last_click`
    /// could trigger a double-click "attach" against whatever new
    /// row took the cell.
    ///
    /// Drop the "Row no longer exists; rename
    /// cancelled" toast on stale rename. The toast fired on every
    /// dashboard open if a row was closed externally, surprising the
    /// user (they hadn't done anything since). The clear itself is
    /// preserved — silently clearing the rename matches the silent
    /// gc on `pinned` / `reorder` / `selected` / `peek` /
    /// `hovered_row` / `last_click`. the earlier invariant ("Commit
    /// Enter can't dispatch against a phantom row") is preserved
    /// because the renamed row can't render the overlay if it's
    /// gone, so Enter can't reach the commit path.
    pub fn gc_stale_refs(&mut self, alive: &dyn Fn(&DashboardRowId) -> bool) {
        self.pinned.retain(|id| alive(id));
        self.reorder.retain(|id| alive(id));
        if let Some(rn) = self.rename.as_ref()
            && !alive(&rn.row)
        {
            self.rename = None;
        }
        if let Some(sel) = self.selected.as_ref()
            && !alive(sel)
        {
            self.selected = None;
        }
        if let Some(p) = self.peek.as_ref()
            && !alive(&p.row)
        {
            self.set_peek(None);
        }
        if let Some(h) = self.hovered_row.as_ref()
            && !alive(h)
        {
            self.hovered_row = None;
        }
        if let Some((id, _)) = self.last_click.as_ref()
            && !alive(id)
        {
            self.last_click = None;
        }
        // `attached_agent` is keyed by `AgentId`, not
        // `DashboardRowId`, so we materialise the equivalent
        // `DashboardRowId::TopLevel` and probe `alive`. Clears the
        // popup gracefully when the underlying session was closed
        // outside the dashboard (e.g. via another surface).
        if let Some(agent_id) = self.attached_agent
            && !alive(&DashboardRowId::TopLevel(agent_id))
        {
            // close_popup() also clears
            // `popup_close_rect` / `popup_outer_rect` so the
            // invariant ("None attached_agent → None hit rects")
            // holds at every close site, not just here.
            self.close_popup();
        }
    }

    /// Switch grouping (`Ctrl+G`).
    pub fn toggle_grouping(&mut self) {
        self.grouping = self.grouping.toggled();
        // Section headers only exist in State grouping. If the cursor was
        // on a section header and grouping just left State mode, move it
        // somewhere valid so the cursor doesn't vanish.
        if self.selected_section.is_some() && !matches!(self.grouping, Grouping::State) {
            self.focus_new_agent_button();
        }
        // Re-engage selection follow — a grouping switch reshuffles
        // the visible row order, so the user's prior manual-scroll
        // position no longer points at the same content.
        self.clear_manual_scroll();
    }

    /// Toggle pin on the selected row. Returns the toggled id, or
    /// `None` if no row is selected.
    pub fn toggle_pin_selected(&mut self) -> Option<DashboardRowId> {
        let id = self.selected.clone()?;
        if !self.pinned.remove(&id) {
            self.pinned.insert(id.clone());
        }
        Some(id)
    }

    /// Forward a scroll event from the app-level mouse pipeline.
    /// Actually adjust `viewport_offset`. The
    /// renderer clamps against the visible row count.
    ///
    /// Mouse wheel is decoupled from the selected row: scrolling
    /// only moves the viewport, leaving `selected` alone. Setting
    /// `manual_scroll_active` tells [`Self::clamp_viewport`] to skip
    /// the snap-to-selection pull-back that the keyboard nav path
    /// relies on, so the viewport can travel past the selected row
    /// (e.g. a 50-row list with selection on row 2 and the user
    /// wheeling down to row 40). The flag is cleared by any
    /// selection-driven update — arrow keys, hover-click, filter
    /// rebuild — so the snap re-engages the moment the cursor
    /// becomes the source of truth again.
    pub fn handle_scroll(&mut self, lines: i32) {
        if lines == 0 {
            return;
        }
        self.manual_scroll_active = true;
        if lines > 0 {
            self.viewport_offset = self.viewport_offset.saturating_add(lines as usize);
        } else {
            self.viewport_offset = self.viewport_offset.saturating_sub((-lines) as usize);
        }
    }

    /// Re-engage selection-driven viewport tracking. Called by any
    /// path that owns selection (arrow keys, dispatcher-side
    /// `DashboardSelect*`, row click, filter / grouping change) so
    /// the next render snaps the viewport back to the selected row.
    /// No-op when the flag is already clear.
    pub fn clear_manual_scroll(&mut self) {
        self.manual_scroll_active = false;
    }

    /// Clamp `viewport_offset` so that `selected_line_idx` (if any)
    /// stays within the visible `viewport_h` window. Returns the
    /// clamped offset (same as `self.viewport_offset` after the call).
    ///
    /// Extracted from a private body inside `render_rows`
    /// so the clamp logic can be exercised in isolation. Both
    /// `render_rows` and `render_narrow_rows` still call this helper
    /// mid-render: viewport clamping needs the live row-count, which
    /// is itself computed at render time from the grouped row list.
    /// Moving the call out of the renderers would require re-deriving
    /// the same grouping in the dispatcher, so we keep the
    /// snap-to-selection side-effect intentional. The
    /// renderer is *factored for testability*; it is not strictly
    /// read-only.
    ///
    /// The snap-to-selection step is SKIPPED while
    /// `manual_scroll_active` is true — that flag means the user is
    /// driving the viewport directly via the mouse wheel and we
    /// must not pull them back to wherever the cursor happens to
    /// sit. The bounds clamp (`offset <= max_offset`) still runs so
    /// scrolling past the bottom edge is a soft stop, not a runaway.
    pub fn clamp_viewport(
        &mut self,
        selected_line: Option<usize>,
        viewport_h: usize,
        total_lines: usize,
    ) -> usize {
        let mut offset = self.viewport_offset;
        if !self.manual_scroll_active
            && let Some(sel_idx) = selected_line
            && viewport_h > 0
        {
            if sel_idx < offset {
                offset = sel_idx;
            } else if sel_idx >= offset + viewport_h {
                offset = sel_idx + 1 - viewport_h;
            }
        }
        let max_offset = total_lines.saturating_sub(viewport_h);
        if offset > max_offset {
            offset = max_offset;
        }
        self.viewport_offset = offset;
        offset
    }

    /// Set the peek panel, enforcing the
    /// "`peek_close_rect` is None whenever `peek` is None" invariant.
    /// Every site that toggles `peek` now goes
    /// through this helper so the close-rect can't be left stale.
    ///
    /// Reply-draft lifecycle: the dashboard-owned [`Self::peek_reply`]
    /// draft dies with the panel it was typed for — it is cleared
    /// whenever the peeked ROW changes through this helper (close,
    /// open, or retarget). Per-frame refreshes of an open panel go
    /// through `PeekPanelState::apply_fields` (not here), so an
    /// in-progress draft survives live updates.
    ///
    /// Does **not** restore the live-tail viewport lease — permission
    /// refresh may call `set_peek(None)` while the same row stays
    /// selected and reopens next paint.
    pub fn set_peek(&mut self, peek: Option<PeekPanelState>) {
        if peek.is_none() {
            self.peek_close_rect = None;
            self.peek_reply_rect = None;
        }
        let row_changed = peek.as_ref().map(|p| &p.row) != self.peek.as_ref().map(|p| &p.row);
        if row_changed {
            self.clear_peek_reply();
        }
        self.peek = peek;
    }

    pub fn restore_peek_viewport(
        &mut self,
        agents: &mut indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        let Some(lease) = self.peek_viewport.take() else {
            return;
        };
        let Some(sb) = scrollback_mut_for_row(&lease.row, agents) else {
            return;
        };
        let page_flip = lease.page_flip_entry;
        let w = lease.snapshot.last_width;
        let h = lease.snapshot.viewport_height;
        sb.restore_viewport_snapshot(lease.snapshot);
        if let Some(entry_id) = page_flip
            && let Some(idx) = sb.index_of_id(entry_id)
        {
            if w > 0 && h > 0 {
                sb.prepare_layout(w, h);
            }
            sb.set_selected(Some(idx));
            sb.scroll_to_entry_top(idx);
            sb.enable_follow_with_preserve();
        }
    }

    pub fn begin_peek_viewport(
        &mut self,
        row: DashboardRowId,
        agents: &mut indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        if self
            .peek_viewport
            .as_ref()
            .is_some_and(|lease| lease.row == row)
        {
            return;
        }
        self.restore_peek_viewport(agents);
        let Some(sb) = scrollback_mut_for_row(&row, agents) else {
            return;
        };
        let snapshot = sb.capture_viewport_snapshot();
        sb.set_view_mode(crate::scrollback::state::ViewMode::AllTurns);
        sb.enable_follow_mode();
        self.peek_viewport = Some(PeekViewportLease {
            row,
            snapshot,
            page_flip_entry: None,
        });
    }

    pub(crate) fn note_page_flip_for_lease(
        &mut self,
        agent_id: AgentId,
        entry_id: crate::scrollback::EntryId,
        agents: &indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        let Some(lease) = self.peek_viewport.as_mut() else {
            return;
        };
        if !lease.row.matches_top_level_agent(agent_id) {
            return;
        }
        let Some(sb) = agents.get(&agent_id).map(|agent| &agent.scrollback) else {
            return;
        };
        if sb.index_of_id(entry_id).is_none() {
            return;
        }
        if !sb.is_follow_preserve_scroll() {
            return;
        }
        lease.page_flip_entry = Some(entry_id);
    }

    /// Clear the peek reply draft AND its undo history.
    ///
    /// The history wipe is the load-bearing part: `set_text("")` alone
    /// records a `Replace` checkpoint, leaving the prior draft one
    /// `Ctrl+Z` away. On a row change that resurrected reply would
    /// target a DIFFERENT agent — exactly the mis-send the draft clear
    /// exists to prevent — so every lifecycle clear (row change, panel
    /// open/close, post-send, per-question reset) routes through here.
    pub(crate) fn clear_peek_reply(&mut self) {
        self.peek_reply.set_text("");
        self.peek_reply.clear_history();
        self.last_prompt_click = None;
    }

    /// Record the peeked agent's working directory so the reply's `@`
    /// picker can later resolve `@paths` against it. Called by the
    /// render pass (the only place with the agents map); the actual
    /// `retarget` is deferred to [`Self::ensure_peek_reply_cwd`].
    pub(crate) fn set_peek_reply_target_cwd(&mut self, cwd: Option<PathBuf>) {
        self.peek_reply_target_cwd = cwd;
    }

    /// Lazily root the reply's `@` file-search daemon at the peeked
    /// agent's cwd (recorded in [`Self::peek_reply_target_cwd`]).
    ///
    /// Applied only when it differs from the daemon's current root and
    /// only at the moment the user composes into the reply — never on a
    /// bare cursor move — because `retarget` rebuilds the matcher daemon
    /// thread. So navigating past a dozen agents in other directories
    /// costs nothing; the (single) retarget happens on the first
    /// keystroke/paste into the reply, deduped by cwd.
    fn ensure_peek_reply_cwd(&mut self) {
        if let Some(target) = self.peek_reply_target_cwd.clone()
            && self.peek_reply_cwd.as_deref() != Some(target.as_path())
        {
            self.peek_reply.file_search.retarget(&target);
            self.peek_reply_cwd = Some(target);
        }
    }

    /// The file-search state backing the `@` dropdown that is actually
    /// on screen: the peek reply's while the panel is open (the dropdown
    /// is drawn from `peek_reply` then — see `render_dashboard`),
    /// otherwise the dispatch box's. Used to route mouse-wheel scrolling
    /// to the SAME picker the user is looking at, so wheel navigation
    /// matches the rendered list.
    pub(crate) fn dropdown_file_search_mut(
        &mut self,
    ) -> &mut crate::views::file_search::FileSearchState {
        if self.peek.is_some() {
            &mut self.peek_reply.file_search
        } else {
            &mut self.dispatch.file_search
        }
    }

    /// Feed a key to the reply widget after ensuring its `@` picker is
    /// rooted at the peeked agent's cwd. All reply-composing paths route
    /// through here so the lazy retarget lands before a freshly typed
    /// `@` kicks off the directory walk on a stale root.
    fn peek_reply_handle_key(
        &mut self,
        key: &KeyEvent,
    ) -> crate::views::prompt_widget::PromptEvent {
        self.ensure_peek_reply_cwd();
        self.peek_reply.handle_key(key)
    }

    /// Convenience for closing the peek panel.
    pub fn close_peek(&mut self) {
        self.set_peek(None);
    }

    /// Atomically clear all popup state
    /// when the overlay closes. `attached_agent`, `popup_close_rect`,
    /// and `popup_outer_rect` are semantically linked: a `None`
    /// `attached_agent` should never leave behind "stale Some" hit
    /// rects, because a future contributor reading
    /// `popup_outer_rect` without the `attached_agent` guard would
    /// get a phantom hit area. Centralising the clear here keeps the
    /// invariant honest at every close site (Esc / Ctrl+\\ key
    /// handlers, `[✗]` mouse click, `dispatch_exit_dashboard`,
    /// `dispatch_open_dashboard`'s toggle branch).
    pub fn close_popup(&mut self) {
        self.attached_agent = None;
        self.popup_close_rect = None;
        self.popup_outer_rect = None;
        self.overlay_close_hit.clear();
        self.overlay_prev_hit.clear();
        self.overlay_next_hit.clear();
    }

    /// Top-level input handler. Mirrors `AgentView::handle_input` —
    /// returns an [`InputOutcome`] for the app to dispatch.
    pub fn handle_input(&mut self, ev: &Event, registry: &ActionRegistry) -> InputOutcome {
        self.handle_input_with_paste_provenance(
            ev,
            registry,
            crate::app::app_view::PasteProvenance::Terminal,
        )
    }

    pub(crate) fn handle_input_with_paste_provenance(
        &mut self,
        ev: &Event,
        registry: &ActionRegistry,
        paste_provenance: crate::app::app_view::PasteProvenance,
    ) -> InputOutcome {
        debug_assert!(
            matches!(ev, Event::Paste(_))
                || paste_provenance == crate::app::app_view::PasteProvenance::Terminal,
            "non-paste dashboard events cannot carry paste provenance"
        );
        // Cheatsheet modal owns the keyboard / mouse when open —
        // its own search input, picker scroll, and chrome buttons
        // would all be inconsistent if the dashboard's row nav got
        // a parallel say. Mirrors how `agent_view` short-circuits
        // any `active_modal` before the per-pane handlers run.
        if self.shortcuts_modal.is_some() {
            return self.handle_shortcuts_modal_input(ev);
        }

        // The location picker owns input while open — its query field,
        // row nav, and chrome buttons would all be inconsistent if the
        // dashboard's own handlers got a parallel say.
        if self.location_picker.is_some() {
            return self.handle_location_picker_input(ev);
        }

        // The worktree-label dialog (shown when a dashboard agent is
        // dispatched with worktree mode armed) owns input while open.
        if self.worktree_dialog.is_some() {
            return self.handle_worktree_dialog_input(ev);
        }

        // Rename mode owns input until committed or cancelled.
        if let Some(ref mut rn) = self.rename {
            match ev {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    return handle_rename_key(rn, key);
                }
                Event::Paste(text) => return handle_rename_paste(rn, text),
                _ => {}
            }
            return InputOutcome::Unchanged;
        }

        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key, registry),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            // Bracketed paste — wrap magic first (never as text); when the
            // peek panel is open it owns the paste (text + images into
            // `peek_reply`), mirroring the Ctrl/Cmd+V chord in
            // `handle_peek_key` (without this, terminals that deliver paste
            // as `Event::Paste` would leak into the HIDDEN new-session
            // dispatch input). Otherwise route through the dispatch paste
            // pipeline.
            Event::Paste(text) => {
                if let Some(wrap) =
                    crate::wrap_clipboard_image::try_decode_wrap_host_image_paste(text)
                {
                    return match wrap {
                        crate::wrap_clipboard_image::WrapImagePaste::Image(data) => {
                            let pasted = crate::prompt_images::from_clipboard_data(&data);
                            if self.peek.is_some() {
                                if let Some(p) = self.peek.as_mut() {
                                    p.focused = true;
                                }
                                self.ensure_peek_reply_cwd();
                                self.attach_peek_pasted_image(pasted).0
                            } else {
                                self.attach_pasted_image(pasted).0
                            }
                        }
                        crate::wrap_clipboard_image::WrapImagePaste::NoImage => {
                            InputOutcome::Unchanged
                        }
                    };
                }
                self.handle_bracketed_paste(
                    text,
                    self.peek.is_some(),
                    paste_provenance.may_probe_clipboard_attachments(),
                )
            }
            _ => InputOutcome::Unchanged,
        }
    }

    /// Bracketed-paste payload into the dispatch input (`peek = false`) or the
    /// peek reply (`peek = true`).
    ///
    /// A file-path / URL paste attaches as `[Image #N]` / path chips
    /// synchronously and wins; else the clipboard image / file-url probe defers
    /// off the event loop (the image wins over the caption, which is inserted on
    /// completion only if no image is found); else plain text inserts
    /// synchronously. Mirrors [`Self::handle_paste_key_deferred`] for the
    /// Ctrl/Cmd+V chord and the agent prompt's paste handling.
    fn handle_bracketed_paste(
        &mut self,
        text: &str,
        peek: bool,
        probe_clipboard_attachments: bool,
    ) -> InputOutcome {
        // The peek reply has extra preconditions the dispatch input does not.
        let in_question = if peek {
            let Some(p) = self.peek.as_ref() else {
                return InputOutcome::Unchanged;
            };
            // In question mode the `❯ reply` line is hidden — the reply widget
            // only backs the `RejectOnce` / "Other" free-text option. Accepting
            // a paste when that option isn't selected would silently fill an
            // invisible buffer that resurfaces if the user later highlights it.
            let in_question = p.question.is_some();
            if in_question {
                let on_reject = p.selected_option.is_some() && p.reject_option == p.selected_option;
                if !on_reject {
                    return InputOutcome::Unchanged;
                }
            }
            // Pasting implies an intent to reply — focus the input and root the
            // `@` picker at the peeked agent's cwd (a pasted `@path` walks it).
            if let Some(p) = self.peek.as_mut() {
                p.focused = true;
            }
            self.ensure_peek_reply_cwd();
            in_question
        } else {
            false
        };

        // Question mode is text-only on the wire — never attach/defer an image.
        if in_question {
            return self.insert_pasted_caption(Some(text), true).0;
        }

        // Pasted text may be image file path(s) / `file://` URL(s) (drag-drop
        // from Finder, or Copy on a file). Resolve them synchronously and win —
        // before deferring the clipboard probe, so a path paste is not ALSO
        // attached as the clipboard raster (no double insert).
        if !text.trim().is_empty() {
            let images = crate::prompt_images::try_read_images_from_paste(text);
            if !images.is_empty() {
                for img in images {
                    if peek {
                        self.attach_peek_pasted_image(img);
                    } else {
                        self.attach_pasted_image(img);
                    }
                }
                return InputOutcome::Changed;
            }
        }

        // Defer the clipboard image / file-url probe (osascript) off the event
        // loop; the image wins over the caption, so the caption is NOT inserted
        // now — it lands on completion only if the probe finds no image. The
        // native snapshot gate skips plain text with no raster so common text
        // pastes never enqueue. Skip large / multi-line pastes that are
        // obviously text; no empty-clipboard telemetry on the bracketed path.
        let should_probe = text.len() < 4096 && !text.contains('\n');
        if probe_clipboard_attachments
            && should_probe
            && let Some(change_count) = crate::clipboard::attachment_probe_gate(Some(text))
        {
            self.enqueue_clipboard_attachment_probe(
                crate::app::actions::ClipboardPasteSource::BracketedDeferred {
                    text: text.to_owned(),
                },
                peek,
                change_count,
            );
            return InputOutcome::Changed;
        }

        if text.trim().is_empty() {
            return InputOutcome::Unchanged;
        }

        // Plain text paste (no raster).
        self.insert_pasted_caption(Some(text), peek).0
    }

    /// Attach a pasted image to the dispatch input as an `[Image #N]` chip.
    fn attach_pasted_image(
        &mut self,
        mut pasted: crate::prompt_images::PastedImage,
    ) -> (InputOutcome, crate::app::actions::ClipboardPasteCompletion) {
        let preparation = pasted.preview_preparation();
        // Dashboard prompts keep display metadata while durable paths remain session-owned.
        pasted.session_image_path = None;
        pasted.staged_temp_path = None;
        let completion = match self.dispatch.insert_image(pasted) {
            Ok(()) => {
                if let Some(preparation) = preparation {
                    self.pending_effects.push(
                        crate::app::actions::Effect::PreparePromptImagePreview { preparation },
                    );
                }
                crate::app::actions::ClipboardPasteCompletion::Handled
            }
            Err(msg) => {
                self.set_error_toast(&msg);
                crate::app::actions::ClipboardPasteCompletion::Failed(
                    crate::app::actions::ClipboardPasteFailure::AlreadyReported,
                )
            }
        };
        (InputOutcome::Changed, completion)
    }

    /// Insert a plain-text (caption) paste into the dispatch input (`peek =
    /// false`) or the peek reply (`peek = true`); the image / file-url portion of
    /// a paste is handled by the deferred probe. Whitespace-only text is a no-op
    /// (matches the bracketed arm — no stray spaces inserted).
    fn insert_pasted_caption(
        &mut self,
        text: Option<&str>,
        peek: bool,
    ) -> (InputOutcome, crate::app::actions::ClipboardTextInsertion) {
        use crate::app::actions::ClipboardTextInsertion;
        let Some(text) = text else {
            // Clipboard entirely empty — consume the key.
            return (InputOutcome::Changed, ClipboardTextInsertion::Empty);
        };
        if text.trim().is_empty() {
            return (InputOutcome::Unchanged, ClipboardTextInsertion::Empty);
        }
        let event = if peek {
            self.peek_reply.handle_paste(text)
        } else {
            self.dispatch.handle_paste(text)
        };
        if !peek && matches!(event, crate::views::prompt_widget::PromptEvent::Edited) {
            self.dispatch.refresh_slash(&self.models);
        }
        let completion = match event {
            crate::views::prompt_widget::PromptEvent::Edited => ClipboardTextInsertion::Inserted,
            crate::views::prompt_widget::PromptEvent::Ignored => ClipboardTextInsertion::Failed,
        };
        (InputOutcome::Changed, completion)
    }

    /// Enqueue attachment probing off-thread so paste-then-send remains ordered.
    fn enqueue_clipboard_attachment_probe(
        &mut self,
        source: crate::app::actions::ClipboardPasteSource,
        peek: bool,
        change_count: Option<u64>,
    ) {
        let target = if peek {
            // Callers only pass `peek = true` with the panel open; stamping the
            // row lets the completion drop a paste whose panel closed/moved.
            let Some(p) = self.peek.as_ref() else {
                return;
            };
            crate::app::actions::ClipboardPasteTarget::DashboardPeek { row: p.row.clone() }
        } else {
            crate::app::actions::ClipboardPasteTarget::DashboardDispatch
        };
        self.paste_probe_in_flight += 1;
        self.pending_effects
            .push(crate::app::actions::Effect::ProbeClipboardAttachment {
                ctx: crate::app::actions::ClipboardPasteContext { target, source },
                change_count,
            });
    }

    /// Attach a pasted image to the peek reply as an `[Image #N]` chip.
    fn attach_peek_pasted_image(
        &mut self,
        mut pasted: crate::prompt_images::PastedImage,
    ) -> (InputOutcome, crate::app::actions::ClipboardPasteCompletion) {
        if self.peek.as_ref().is_some_and(|p| p.question.is_some()) {
            return (
                InputOutcome::Unchanged,
                crate::app::actions::ClipboardPasteCompletion::Dropped,
            );
        }
        let preparation = pasted.preview_preparation();
        pasted.session_image_path = None;
        pasted.staged_temp_path = None;
        let completion = match self.peek_reply.insert_image(pasted) {
            Ok(()) => {
                if let Some(preparation) = preparation {
                    self.pending_effects.push(
                        crate::app::actions::Effect::PreparePromptImagePreview { preparation },
                    );
                }
                crate::app::actions::ClipboardPasteCompletion::Handled
            }
            Err(msg) => {
                self.set_error_toast(&msg);
                crate::app::actions::ClipboardPasteCompletion::Failed(
                    crate::app::actions::ClipboardPasteFailure::AlreadyReported,
                )
            }
        };
        (InputOutcome::Changed, completion)
    }

    /// Ctrl/Cmd+V into the dispatch input (`peek = false`) or the peek reply
    /// (`peek = true`): a pasted file path resolves synchronously and wins; else
    /// the clipboard raster/file-url probe defers off the event loop (the image
    /// wins over the caption, inserted on completion only if no image); else
    /// plain text with no raster inserts synchronously.
    fn handle_paste_key_deferred(
        &mut self,
        clipboard_text: crate::app::actions::ClipboardTextRead,
        peek: bool,
    ) -> InputOutcome {
        // The peek reply has extra preconditions the dispatch input does not.
        let in_question = if peek {
            let Some(p) = self.peek.as_ref() else {
                return InputOutcome::Unchanged;
            };
            let in_question = p.question.is_some();
            if in_question {
                let on_reject = p.selected_option.is_some() && p.reject_option == p.selected_option;
                if !on_reject {
                    return InputOutcome::Unchanged;
                }
            }
            // Pasting implies an intent to reply — focus + root the `@` picker.
            if let Some(p) = self.peek.as_mut() {
                p.focused = true;
            }
            self.ensure_peek_reply_cwd();
            in_question
        } else {
            false
        };

        // Question mode is text-only on the wire — never attach/defer an image.
        if in_question {
            return self
                .insert_pasted_caption(clipboard_text.as_deref(), true)
                .0;
        }

        // A pasted file path resolves synchronously and wins (drag-drop / Finder
        // Cmd+C) — before deferring, so it is not double-attached.
        if let Some(text) = clipboard_text.as_deref()
            && !text.trim().is_empty()
        {
            let images = crate::prompt_images::try_read_images_from_paste(text);
            if !images.is_empty() {
                for img in images {
                    if peek {
                        self.attach_peek_pasted_image(img);
                    } else {
                        self.attach_pasted_image(img);
                    }
                }
                return InputOutcome::Changed;
            }
        }

        // Image likely on the pasteboard → defer; the image wins over the
        // caption (caption inserted on completion only if the probe finds none).
        if let Some(change_count) =
            crate::clipboard::attachment_probe_gate(clipboard_text.as_deref())
        {
            self.enqueue_clipboard_attachment_probe(
                crate::app::actions::ClipboardPasteSource::ClipboardKey {
                    text: clipboard_text,
                    tip_showing: false,
                },
                peek,
                change_count,
            );
            return InputOutcome::Changed;
        }

        // No raster / lone URL: insert the plain text synchronously.
        self.insert_pasted_caption(clipboard_text.as_deref(), peek)
            .0
    }

    /// Attach the result of a deferred clipboard attachment probe
    /// ([`Effect::ProbeClipboardAttachment`]) to the surface named by
    /// `ctx.target` (dispatch input or peek reply). The heavy read/decode
    /// already ran off-thread; this only mutates input state on the event loop.
    pub(crate) fn complete_clipboard_attachment_paste(
        &mut self,
        ctx: crate::app::actions::ClipboardPasteContext,
        image: crate::app::actions::ProbedAttachment,
        file_urls: Option<String>,
    ) -> crate::app::actions::ClipboardPasteCompletion {
        use crate::app::actions::{
            ClipboardPasteCompletion, ClipboardPasteFailure, ClipboardPasteTarget, ProbedAttachment,
        };
        let peek_row = match &ctx.target {
            ClipboardPasteTarget::DashboardPeek { row } => Some(row.clone()),
            _ => None,
        };
        let peek = peek_row.is_some();
        self.paste_probe_in_flight = self.paste_probe_in_flight.saturating_sub(1);
        // Drop a peek completion if the panel closed OR moved to another row
        // between enqueue and now, so the attachment can't land in the hidden
        // reply buffer or a different agent's reply (mirrors the agent's
        // agent-id-gone guard in dispatch).
        if let Some(row) = &peek_row
            && self.peek.as_ref().is_none_or(|p| p.row != *row)
        {
            return ClipboardPasteCompletion::Dropped;
        }
        // A question that arrived on the peeked row mid-probe makes the reply
        // text-only on the wire: attachments are discarded LOUDLY below (the
        // attach helper's silent question no-op would drop them with zero
        // feedback), and the caption/wrap paths stay suppressed.
        let peek_in_question = peek && self.peek.as_ref().is_some_and(|p| p.question.is_some());
        let insert_deferred_text = !peek_in_question
            && matches!(
                &image,
                ProbedAttachment::NoRaster
                    | ProbedAttachment::ProbeDropped
                    | ProbedAttachment::ProbeFailed
            );
        let mut attachment = match image {
            ProbedAttachment::Image(pasted) => {
                if peek_in_question {
                    self.set_error_toast("Pasted image discarded — reply switched to a question");
                    ClipboardPasteCompletion::Dropped
                } else {
                    let (_, completion) = if peek {
                        self.attach_peek_pasted_image(pasted)
                    } else {
                        self.attach_pasted_image(pasted)
                    };
                    completion
                }
            }
            ProbedAttachment::PersistFailed(msg) => {
                self.set_error_toast(&msg);
                ClipboardPasteCompletion::Failed(ClipboardPasteFailure::AlreadyReported)
            }
            ProbedAttachment::NoRaster => ClipboardPasteCompletion::FullMiss,
            ProbedAttachment::ProbeDropped => ClipboardPasteCompletion::Dropped,
            ProbedAttachment::ProbeFailed => {
                ClipboardPasteCompletion::Failed(ClipboardPasteFailure::AttachmentRead)
            }
        };
        if peek_in_question && attachment == ClipboardPasteCompletion::FullMiss {
            if file_urls.as_deref().is_some_and(|urls| {
                !crate::prompt_images::try_read_images_from_paste(urls).is_empty()
            }) {
                self.set_error_toast("Pasted image discarded — reply switched to a question");
            }
            attachment = ClipboardPasteCompletion::Dropped;
        }
        let file = if attachment == ClipboardPasteCompletion::FullMiss {
            file_urls.and_then(|urls| {
                let images = crate::prompt_images::try_read_images_from_paste(&urls);
                if images.is_empty() {
                    return None;
                }
                let mut completion =
                    ClipboardPasteCompletion::Failed(ClipboardPasteFailure::AlreadyReported);
                for img in images {
                    let (_, inserted) = if peek {
                        self.attach_peek_pasted_image(img)
                    } else {
                        self.attach_pasted_image(img)
                    };
                    if inserted == ClipboardPasteCompletion::Handled {
                        completion = ClipboardPasteCompletion::Handled;
                    }
                }
                Some(completion)
            })
        } else {
            None
        };
        let text = if insert_deferred_text {
            ctx.source
                .text_to_insert_on_miss()
                .filter(|text| !text.trim().is_empty())
                .map(|text| self.insert_pasted_caption(Some(text), peek).1)
        } else {
            None
        };
        let completion = crate::app::actions::reduce_clipboard_paste_completion(
            &ctx.source,
            attachment,
            file,
            text,
        );
        if completion == ClipboardPasteCompletion::FullMiss && ctx.source.is_clipboard_key() {
            crate::clipboard::log_paste_key_empty_host_clipboard(ctx.target.surface_str());
        }
        completion
    }

    /// After a deferred paste probe completes, take the sends stashed while the
    /// probe(s) were in flight — dispatch first, then peek — rebuilding each
    /// from its now-updated widget text so a freshly attached image chip (and
    /// its aligned range) travels with it. Returns empty while probes remain in
    /// flight. A peek stash whose panel closed, moved to another row, or now
    /// shows a question is dropped with a toast (the reply draft stays in the
    /// widget).
    pub(crate) fn take_deferred_sends_after_paste(&mut self) -> Vec<crate::app::actions::Action> {
        if self.paste_probe_in_flight != 0 {
            return Vec::new();
        }
        let mut actions = Vec::new();
        if let Some(DeferredDispatchSend { attach }) = self.deferred_dispatch_send.take() {
            actions.push(crate::app::actions::Action::DashboardDispatch {
                text: self.dispatch.text().to_string(),
                attach,
            });
        }
        if let Some(DeferredPeekSend { row, attach }) = self.deferred_peek_send.take() {
            let same_row = self.peek.as_ref().is_some_and(|p| p.row == row);
            let in_question = self.peek.as_ref().is_some_and(|p| p.question.is_some());
            if same_row && !in_question {
                actions.push(crate::app::actions::Action::DashboardPeekReply {
                    row,
                    text: self.peek_reply.text().to_string(),
                    attach,
                });
            } else if !same_row {
                // Never reply to a row the user is no longer peeking.
                self.set_error_toast("Reply canceled — peek panel changed");
            } else {
                // A question now owns the panel (Enter answers it there, and the
                // reply dispatch would silently queue a prompt + wipe the draft
                // behind the dialog) — drop the stash; the draft stays put.
                self.set_error_toast("Reply canceled — answer the question first");
            }
        }
        actions
    }

    /// Handle a key while the peek panel is open.
    ///
    /// The peek panel's `❯ reply` line is a live [`PromptWidget`]
    /// editor ([`Self::peek_reply`]), so when peek is open it owns the
    /// bare keys: printable characters, Backspace/Delete/arrows, and
    /// the widget's editing chords (word nav, `Ctrl+A`/`Ctrl+E`,
    /// `Alt+Backspace`, undo, Shift+arrow selection, inline paste)
    /// all edit the reply. Up/Down move the caret WITHIN the reply when
    /// it has content (multi-line drafts); when the reply is empty (or
    /// unfocused via Tab) they switch the peeked agent instead (the
    /// panel follows the selection cursor). Enter sends
    /// / queues the reply (Ctrl+S sends AND opens the agent; Shift+Enter
    /// / Alt+Enter insert a newline); Esc closes the panel; and 1–9
    /// select (toggle) a pending question's option, after which Enter
    /// answers the selection.
    ///
    /// `dashboard_owned` is whether the key resolved to a
    /// `When::DashboardFocused` registry binding (`Ctrl+X` stop,
    /// `Shift+↑/↓` reorder, `Shift+Tab` mode, …) — those return `None`
    /// so the dashboard handler (and the remaining app-global
    /// shortcuts) still fire with the panel open.
    ///
    /// Returns `Some` for keys the panel consumes and `None` for keys
    /// it leaves to the normal dashboard / global handling.
    fn handle_peek_key(
        &mut self,
        key: &KeyEvent,
        from_registry: Option<crate::actions::ActionId>,
    ) -> Option<InputOutcome> {
        let dashboard_owned = from_registry.is_some();
        // Paste → reply widget (text + images). Handled up-front because
        // the paste chord carries CONTROL / SUPER and must not be
        // mistaken for a dashboard chord.
        if crate::input::key::is_paste_key(key) {
            let clipboard_text = crate::app::actions::ClipboardTextRead::from_result(
                crate::clipboard::system_clipboard_read_text(),
            );
            return Some(self.handle_paste_key_deferred(clipboard_text, /* peek */ true));
        }

        // Ctrl+C / Ctrl+D must reach the app-global quit handler (the
        // double-press-to-quit fallback fires only when the view returns
        // `Unchanged`). Returning `Unchanged` here bubbles them up cleanly
        // instead of letting them leak into — and be swallowed by — the
        // reply widget (whose Ctrl+C would clear the draft instead).
        if key!('c', CONTROL).matches(key) || key!('d', CONTROL).matches(key) {
            return Some(InputOutcome::Unchanged);
        }

        // `@`-file-search intercept — while the reply's context picker
        // dropdown is open it owns Up/Down/PageUp-Down/Tab/Enter/Esc/
        // Ctrl+P-N (navigate / accept / dismiss), mirroring the dispatch
        // box's intercept. Routed BEFORE the peek's own nav / send /
        // close handlers so those keys steer the dropdown instead. Keys
        // the picker ignores fall through to the normal peek handling
        // below (so e.g. Ctrl+X stop still fires with the dropdown up).
        if self.peek_reply.file_search_visible() {
            match self.peek_reply_handle_key(key) {
                crate::views::prompt_widget::PromptEvent::Edited => {
                    return Some(InputOutcome::Changed);
                }
                crate::views::prompt_widget::PromptEvent::Ignored => {}
            }
        }

        // Empty peek reply: shortcuts help opens instead of typing.
        if matches!(
            from_registry,
            Some(crate::actions::ActionId::DashboardShortcutsHelp)
        ) && self.peek_reply.text().is_empty()
        {
            return Some(InputOutcome::Action(Action::DashboardOpenShortcutsHelp));
        }

        // Dashboard-owned chords fall through so the registry actions
        // (Ctrl+X stop, Ctrl+T pin, Shift+↑/↓ reorder, Shift+Tab mode,
        // …) and the hardcoded Ctrl+/ search toggle keep working with
        // the panel open. Keys the peek itself owns are exempt:
        //   - bare / Shift printable chars TYPE into the reply (vim's
        //     bare `j`/`k` nav lose to typing; `?` help only when the
        //     reply is non-empty — empty handled above),
        //   - bare Up/Down are the peek's agent switcher (handled
        //     below, independent of the dispatch box's focus gating),
        //   - Esc / Enter / Tab drive the peek's own affordances.
        // Everything else — non-owned Ctrl/Alt/Super editing chords
        // (word nav, kill-line, undo, …) — falls through to the reply
        // widget delegation at the bottom.
        let is_typing_char = matches!(key.code, KeyCode::Char(_))
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT);
        let peek_owned = is_typing_char
            || matches!(key.code, KeyCode::Esc | KeyCode::Enter)
            || (matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab)
                && key.modifiers.is_empty());
        if (dashboard_owned && !peek_owned) || key!('/', CONTROL).matches(key) {
            return None;
        }

        // Ctrl+S = "send + open": send / queue the reply AND walk into
        // the agent's detail view. This is the chord that replaced
        // Shift+Enter (now freed, with Alt+Enter, for newline). An empty
        // reply — or any pending question, which has no reply line —
        // simply opens the agent. Handled before the question / Esc / Tab
        // blocks so it works in every peek state.
        if key!('s', CONTROL).matches(key) {
            let Some(row) = self.peek.as_ref().map(|p| p.row.clone()) else {
                return Some(InputOutcome::Unchanged);
            };
            let reply_text = self.peek_reply.text().to_string();
            let has_question = self.peek.as_ref().is_some_and(|p| p.question.is_some());
            if has_question || reply_text.trim().is_empty() {
                return Some(InputOutcome::Action(Action::DashboardAttach(row)));
            }
            return Some(InputOutcome::Action(Action::DashboardPeekReply {
                row,
                text: reply_text,
                attach: true,
            }));
        }

        // Number keys 1–9 SELECT (toggle) the matching option when a question
        // is showing — they no longer answer directly. Selecting focuses the
        // panel so it becomes an answer surface (`Enter` then answers);
        // pressing the selected option's key again deselects it (→ navigation).
        if let KeyCode::Char(c) = key.code
            && let Some(d) = c.to_digit(10)
            && (1..=9).contains(&d)
            && self.peek.as_ref().is_some_and(|p| p.question.is_some())
        {
            let idx = (d - 1) as usize;
            let valid = self.peek.as_ref().is_some_and(|p| idx < p.options.len());
            if !valid {
                let n_opts = self.peek.as_ref().map(|p| p.options.len()).unwrap_or(0);
                self.set_error_toast(&format!("No such option (only {n_opts} available)"));
                return Some(InputOutcome::Changed);
            }
            if let Some(p) = self.peek.as_mut() {
                p.focused = true;
                p.selected_option = if p.selected_option == Some(idx) {
                    None
                } else {
                    Some(idx)
                };
            }
            return Some(InputOutcome::Changed);
        }

        // Esc: the peek is shown by default while a row is selected, so
        // Esc UNSELECTS — first clearing a typed reply, then deselecting
        // the row and focusing the `[+ New Agent]` button (which closes
        // the peek and brings back the new-session input).
        if matches!(key.code, KeyCode::Esc) {
            if !self.peek_reply.text().is_empty() {
                self.clear_peek_reply();
                return Some(InputOutcome::Changed);
            }
            self.set_peek(None);
            self.focus_new_agent_button();
            return Some(InputOutcome::Changed);
        }

        // Tab toggles focus between the reply input and the row list,
        // mirroring the dispatch box's two-focus model. Unfocusing dims
        // the panel border and hides the caret (see `render_peek_panel`).
        if matches!(key.code, KeyCode::Tab) && key.modifiers.is_empty() {
            if let Some(p) = self.peek.as_mut() {
                p.focused = !p.focused;
            }
            return Some(InputOutcome::Changed);
        }

        // When a permission / ask-tool question is showing and the panel is
        // focused it's an option picker. With NO option selected (the
        // default) it stays a navigation surface: `↑`/`↓` switch agents and
        // `Enter` opens the row in detail. Once an option is selected (number
        // key) it becomes a modal answer surface: `↑`/`↓` move within the
        // options (spilling to the prev/next agent at the first/last option),
        // `Enter` answers, and the `RejectOnce` ("No") option accepts inline
        // free-text feedback.
        let vim_mode = crate::appearance::cache::load_vim_mode();
        let question_mode = self
            .peek
            .as_ref()
            .is_some_and(|p| p.focused && p.question.is_some() && !p.options.is_empty());
        if question_mode {
            let selected = self.peek.as_ref().and_then(|p| p.selected_option);
            let opt_count = self.peek.as_ref().map(|p| p.options.len()).unwrap_or(0);
            let on_reject = selected.is_some()
                && self
                    .peek
                    .as_ref()
                    .is_some_and(|p| p.reject_option == selected);
            match (key.code, selected) {
                // ── No option selected → navigate agents / open ──
                (KeyCode::Up, None) => {
                    return Some(InputOutcome::Action(Action::DashboardSelectPrev));
                }
                (KeyCode::Down, None) => {
                    return Some(InputOutcome::Action(Action::DashboardSelectNext));
                }
                (KeyCode::Enter, None) => {
                    let row = self.peek.as_ref().map(|p| p.row.clone());
                    return Some(
                        row.map(|r| InputOutcome::Action(Action::DashboardAttach(r)))
                            .unwrap_or(InputOutcome::Unchanged),
                    );
                }
                // Right (and vim `l`) open detail on the nav surface;
                // without this the modal catch-all below swallows them.
                (KeyCode::Right | KeyCode::Char('l'), None)
                    if key.modifiers.is_empty()
                        && (matches!(key.code, KeyCode::Right) || vim_mode) =>
                {
                    let row = self.peek.as_ref().map(|p| p.row.clone());
                    return Some(
                        row.map(|r| InputOutcome::Action(Action::DashboardAttach(r)))
                            .unwrap_or(InputOutcome::Unchanged),
                    );
                }
                // ── Option selected → move within options (spill at edges) ──
                (KeyCode::Up, Some(0)) => {
                    return Some(InputOutcome::Action(Action::DashboardSelectPrev));
                }
                (KeyCode::Up, Some(i)) => {
                    if let Some(p) = self.peek.as_mut() {
                        p.selected_option = Some(i - 1);
                    }
                    return Some(InputOutcome::Changed);
                }
                (KeyCode::Down, Some(i)) if i + 1 >= opt_count => {
                    return Some(InputOutcome::Action(Action::DashboardSelectNext));
                }
                (KeyCode::Down, Some(i)) => {
                    if let Some(p) = self.peek.as_mut() {
                        p.selected_option = Some(i + 1);
                    }
                    return Some(InputOutcome::Changed);
                }
                (KeyCode::Enter, Some(i)) => {
                    let is_ask = self.peek.as_ref().is_some_and(|p| p.is_ask_question());
                    if on_reject
                        && matches!(
                            self.peek_reply.try_element_interaction(key),
                            Some(crate::views::prompt_widget::ElementInteraction::Inlined)
                        )
                    {
                        return Some(InputOutcome::Changed);
                    }
                    // On the reject/free-text option with typed text: submit
                    // the ask "Other" answer, or the permission rejection +
                    // feedback message.
                    let feedback = on_reject
                        .then(|| self.peek_reply.text_without_image_chips())
                        .filter(|t| !t.trim().is_empty());
                    if let Some(text) = feedback {
                        let row = self.peek.as_ref().map(|p| p.row.clone());
                        let request_id = self.peek.as_ref().and_then(|p| p.request_id);
                        if let Some(row) = row {
                            if is_ask {
                                return Some(InputOutcome::Action(
                                    Action::DashboardQuestionAnswer {
                                        row,
                                        option_idx: None,
                                        freeform: text,
                                    },
                                ));
                            } else if let Some(request_id) = request_id {
                                return Some(InputOutcome::Action(
                                    Action::DashboardPermissionFollowup {
                                        row,
                                        request_id,
                                        text,
                                    },
                                ));
                            }
                        }
                        return Some(InputOutcome::Unchanged);
                    }
                    // Otherwise answer the selected option — `peek_number_key`
                    // routes to the permission / ask answer action by source.
                    if let Some(action) = super::peek::peek_number_key(self, i + 1) {
                        return Some(InputOutcome::Action(action));
                    }
                    return Some(InputOutcome::Unchanged);
                }
                _ => {}
            }
            // Free-text feedback editing — only when the reject option is
            // the selected one. Delegated to the reply widget so the
            // feedback field gets the same editing surface (selection,
            // word ops, chips) as the main reply line.
            if on_reject
                && matches!(
                    self.peek_reply_handle_key(key),
                    crate::views::prompt_widget::PromptEvent::Edited
                )
            {
                return Some(InputOutcome::Changed);
            }
            // Modal while the question picker is up: consume any other key so
            // it can't leak into the (hidden) reply row or row navigation.
            return Some(InputOutcome::Unchanged);
        }

        let focused = self.peek.as_ref().map(|p| p.focused).unwrap_or(false);

        // BARE Up/Down switch the peeked agent ONLY while the reply is a
        // navigation surface — either unfocused (Tab → row nav) or empty
        // (a browse convenience, mirroring the dispatch input's
        // empty-prompt gate). With a non-empty FOCUSED reply the arrows
        // move the caret WITHIN the text instead (multi-line drafts), so
        // typing then arrowing edits the reply rather than jumping to
        // another agent — they fall through to the widget delegation
        // below.
        //
        // MODIFIED arrows (e.g. `Shift+↑/↓` row reorder) must fall through
        // to the registry actions regardless — matching on `key.code`
        // alone would otherwise swallow them as agent-switch and the peek
        // (shown by default on selection) would make reorder appear broken.
        if key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
            && (!focused || self.peek_reply.text().is_empty())
        {
            return Some(InputOutcome::Action(match key.code {
                KeyCode::Up => Action::DashboardSelectPrev,
                _ => Action::DashboardSelectNext,
            }));
        }

        // Open detail (mirror of overlay Left-to-back). Right: nav surface
        // (unfocused or empty). Vim `l`: unfocused only — focused reply
        // must type literal `l`, same as focused `j`/`k`.
        let open_detail = key.modifiers.is_empty()
            && match key.code {
                KeyCode::Right => !focused || self.peek_reply.text().is_empty(),
                KeyCode::Char('l') if vim_mode => !focused,
                _ => false,
            };
        if open_detail {
            let Some(row) = self.peek.as_ref().map(|p| p.row.clone()) else {
                return Some(InputOutcome::Unchanged);
            };
            return Some(InputOutcome::Action(Action::DashboardAttach(row)));
        }

        if matches!(key.code, KeyCode::Enter) {
            let mod_enter = crate::input::is_mod_enter(key);
            if focused
                && !mod_enter
                && matches!(
                    self.peek_reply.try_element_interaction(key),
                    Some(crate::views::prompt_widget::ElementInteraction::Inlined)
                )
            {
                return Some(InputOutcome::Changed);
            }
            let enter_is_newline =
                focused && compose_enter_is_newline(self.multiline_mode, mod_enter);
            if !enter_is_newline {
                let Some(row) = self.peek.as_ref().map(|p| p.row.clone()) else {
                    return Some(InputOutcome::Unchanged);
                };
                if !focused && vim_mode {
                    if let Some(p) = self.peek.as_mut() {
                        p.focused = true;
                    }
                    return Some(InputOutcome::Changed);
                }
                let reply_text = self.peek_reply.text().to_string();
                if !focused || reply_text.trim().is_empty() {
                    return Some(InputOutcome::Action(Action::DashboardAttach(row)));
                }
                return Some(InputOutcome::Action(Action::DashboardPeekReply {
                    row,
                    text: reply_text,
                    attach: false,
                }));
            }
        }

        // Ctrl+M: ToggleMultiline is PromptFocused only, so hardcode here.
        if key!('m', CONTROL).matches(key) {
            return Some(InputOutcome::Action(Action::SetMultilineMode(
                !self.multiline_mode,
            )));
        }

        // Space is just text now (the peek is tied to selection, so there
        // is no Space-to-close — use Esc to unselect). It falls through to
        // the reply editor below.
        if focused {
            // Focused reply input — everything else is an edit attempt,
            // delegated to the reply widget (chars, Backspace/Delete,
            // arrows, word nav, kill-line, undo, Shift+arrow selection,
            // inline paste, …). Keys the widget ignores are still
            // CONSUMED (`Unchanged`) so they can't leak into the hidden
            // dispatch input below; app-global shortcuts still fire off
            // the `Unchanged` bubble-up.
            return Some(match self.peek_reply_handle_key(key) {
                crate::views::prompt_widget::PromptEvent::Edited => InputOutcome::Changed,
                crate::views::prompt_widget::PromptEvent::Ignored => InputOutcome::Unchanged,
            });
        }

        // Vim unfocused: j/k select rows; i focuses reply without typing `i`.
        if vim_mode && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('j') => {
                    return Some(InputOutcome::Action(Action::DashboardSelectNext));
                }
                KeyCode::Char('k') => {
                    return Some(InputOutcome::Action(Action::DashboardSelectPrev));
                }
                KeyCode::Char('i') => {
                    if let Some(p) = self.peek.as_mut() {
                        p.focused = true;
                    }
                    return Some(InputOutcome::Changed);
                }
                _ => {}
            }
        }

        // Unfocused: non-vim printable focuses+types; vim swallows (focus via Enter/i/Tab).
        if is_typing_char {
            if vim_mode {
                return Some(InputOutcome::Unchanged);
            }
            if let Some(p) = self.peek.as_mut() {
                p.focused = true;
            }
            let _ = self.peek_reply_handle_key(key);
            return Some(InputOutcome::Changed);
        }

        // Every OTHER key is CONSUMED (`Unchanged`) rather than left to
        // fall through. The dispatch box is HIDDEN while the peek is open,
        // so a `None` here would let editing chords (Backspace, Delete,
        // `Ctrl+W`/`Ctrl+U`/`Ctrl+K`, Home/End, …) leak into and silently
        // mutate the invisible new-session draft behind the panel.
        // Dashboard-owned chords (`Ctrl+X` stop, `Shift+↑/↓` reorder, …),
        // `Ctrl+/`, and `Ctrl+C`/`Ctrl+D` already returned above, so
        // nothing reaching here has a dashboard or row-nav meaning;
        // returning `Unchanged` (not `None`) still lets app-global
        // shortcuts fire off the bubble-up.
        Some(InputOutcome::Unchanged)
    }

    /// Resolve the dispatch input's send action for the given `attach`
    /// flag. Shared by bare `Enter` (`attach == false`, stay on the
    /// dashboard) and the `Ctrl+S` "send + open" chord (`attach ==
    /// true`, walk into the detail view). Empty-prompt fallbacks mirror
    /// the old Enter handler: open the selected row, or create from the
    /// `[+ New Agent]` button. A `/command` always routes to the slash
    /// dispatcher (there's no session to "open"), so `attach` only
    /// affects the plain-dispatch path.
    fn dispatch_send_action(&self, attach: bool) -> InputOutcome {
        let text = self.dispatch.text().to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            if let Some(id) = self.selected.clone() {
                return InputOutcome::Action(Action::DashboardAttach(id));
            }
            if self.new_agent_button_focused {
                return InputOutcome::Action(Action::DashboardCreateNewAgentWithDetail);
            }
            return InputOutcome::Unchanged;
        }
        if trimmed.starts_with('/') {
            return InputOutcome::Action(Action::DashboardDispatchSlash { text });
        }
        InputOutcome::Action(Action::DashboardDispatch { text, attach })
    }

    fn handle_key(&mut self, key: &KeyEvent, registry: &ActionRegistry) -> InputOutcome {
        // Resolve the registry binding up-front — the toast / stop-confirm
        // clear below needs to know whether this key IS the stop key, and
        // it must run before the peek intercept (the lookup itself is a
        // pure read; the action is honoured further down).
        //
        // Vim-aware lookup: `lookup_with_mode` suppresses the bare-letter
        // (`j`/`k`) nav bindings when vim-mode is OFF, so they type into
        // the dispatch input instead of moving the row selection —
        // matching the agent view's scrollback. In vim-mode they navigate
        // (when the prompt is empty, per the `bare_letter_ok` gate below).
        // Arrows / Ctrl combos always resolve.
        let vim_mode = crate::appearance::cache::load_vim_mode();
        let from_registry =
            registry.lookup_with_mode(key, crate::actions::When::DashboardFocused, vim_mode);

        // Clear `error_toast` at the TOP of the
        // handler so any subsequent keypress dismisses the toast,
        // regardless of which branch handles the key (including keys
        // the peek panel consumes — peek is open by default for a
        // selected row, so nav keys route through it).
        //
        // When the toast is cleared, the linked
        // `stop_confirm` armed state is also cleared. The two state
        // bits are semantically linked: the user saw "Press Ctrl+X
        // again", that hint is now gone, so re-arm rather than let a
        // stale confirm window silently close the wrong session.
        //
        // The clear is SKIPPED when the resolved
        // action is `DashboardStop`. Without this skip, the second
        // Ctrl+X press would wipe the just-armed `stop_confirm`
        // before `dispatch_dashboard_stop` could observe it, and the
        // session would never close (the dispatcher kept re-arming a
        // fresh confirm on every press). The Ctrl+X path owns
        // `stop_confirm` and `error_toast` end-to-end: the first
        // press arms both, the second press observes them and closes.
        let preserve_stop_state =
            matches!(from_registry, Some(crate::actions::ActionId::DashboardStop));
        if !preserve_stop_state {
            self.error_toast = None;
            // The disarm is NOT gated on `error_toast` being set (the
            // Ctrl+X arm path deliberately plants no toast): a pending
            // stop confirmation is bound to the row that was selected
            // when Ctrl+X was pressed, so any other key — nav included —
            // must disarm it. Otherwise the footer's "press again to
            // close" hint lingers while the cursor moves to other agents.
            self.stop_confirm = None;
        }

        // Free-tier override: Ctrl+O opens the pinned upgrade CTA (when one is
        // live) instead of falling through to the dispatch input. Matched on the
        // chord directly — `ToggleYolo` is `When::AgentScreen`-scoped and never
        // resolves here. The dispatch re-resolves the slot gate, so a stale flag
        // stays a safe no-op. Stamped `Keyboard` (like the agent/welcome Ctrl+O)
        // so "which surface" stays orthogonal to "was it keyboard".
        if self.pinned_upgrade_cta_live && key!('o', CONTROL).matches(key) {
            return InputOutcome::Action(Action::AnnouncementsOpenCta(
                xai_grok_telemetry::events::AnnouncementCtaSurface::Keyboard,
            ));
        }

        // Shift+Tab while the peek is open cycles the PEEKED agent's live
        // mode, not the new-session staged mode. The registry resolves all
        // Shift+Tab encodings to `DashboardCycleMode`; re-route that to the
        // peek-scoped action here so the cycle acts on the agent under the
        // cursor (and the bottom-border badge updates to match). Outside the
        // peek it still cycles the dispatch box's staged mode.
        if self.peek.is_some()
            && matches!(
                from_registry,
                Some(crate::actions::ActionId::DashboardCycleMode)
            )
        {
            return InputOutcome::Action(Action::DashboardPeekCycleMode);
        }

        // When the peek panel is open it owns the bare keys: the `❯ reply`
        // line is a live editor, Up/Down switch the peeked agent, Enter
        // sends / queues the reply, and Esc closes. Routing here first
        // keeps peek typing from leaking into the (hidden) dispatch input
        // or the row-nav / new-session machinery below. Keys the panel
        // doesn't own (registry-bound dashboard chords like Ctrl+X stop
        // or Shift+↑/↓ reorder) return `None` and fall through so the
        // dashboard's registry actions and global shortcuts still fire.
        if self.peek.is_some()
            && let Some(outcome) = self.handle_peek_key(key, from_registry)
        {
            return outcome;
        }

        let prompt_empty = self.dispatch.text().is_empty();

        // Peek permission answering (digits 1–9) and
        // all other peek input is handled up-front by `handle_peek_key`
        // (the early return at the top of this function), so by the time
        // execution reaches here the peek panel is guaranteed closed.

        // ── Ctrl+V / Cmd+V paste ────────────────────────────────────────
        // Read the pbpaste text once and route through the shared deferred
        // paste pipeline: a file path wins synchronously, else the clipboard
        // image/file-url probe defers off the event loop. Mirrors `AgentView`
        // — without this, Ctrl+V on the dashboard did nothing useful.
        if crate::input::key::is_paste_key(key) {
            let clipboard_text = crate::app::actions::ClipboardTextRead::from_result(
                crate::clipboard::system_clipboard_read_text(),
            );
            return self.handle_paste_key_deferred(clipboard_text, /* peek */ false);
        }

        // ── @-file-search intercept ─────────────────────────────────────
        // The dispatch input offers a session-less `@` context picker
        // rooted at the pager's launch cwd. While its dropdown is
        // visible, the prompt widget owns Up/Down/Tab/Enter/Esc — route
        // the key there BEFORE the dashboard's row-nav / Enter / Esc
        // handlers, mirroring `agent_view::handle_prompt_key`.
        if self.dispatch.file_search_visible() {
            match self.dispatch.handle_key(key) {
                crate::views::prompt_widget::PromptEvent::Edited => {
                    self.dispatch.refresh_slash(&self.models);
                    return InputOutcome::Changed;
                }
                crate::views::prompt_widget::PromptEvent::Ignored => {
                    // Not a picker key — fall through to normal handling.
                }
            }
        }

        // `from_registry` (resolved at the top of this
        // handler) routes any `DashboardFocused` binding through the
        // registry. Special cases (Esc cascade, Enter dispatch) are
        // handled below because they require multi-tier behaviour
        // (clear filter, clear input, exit) that a single registry
        // action can't express. The bindings themselves ARE registered
        // (so the help text and key picker pick them up); we just don't
        // always honour the action's single-fire semantics for Esc.

        // Collapsible section headers: when the cursor is on a section
        // title, Right expands, Left collapses, Enter toggles (and, in
        // vim mode with the LIST focused, `l` / `h` mirror Right / Left,
        // matching the `j`/`k` nav parity). Up/Down nav reaches the
        // header via the registry nav actions below.
        //
        // Gated on `prompt_empty || list_focused`: while the input is
        // FOCUSED and holds text, Left/Right edit the draft and Enter
        // dispatches it (a section header is never a reply target). The
        // vim `l`/`h` arms additionally require `list_focused`, so they
        // fold the section ONLY while the LIST is focused; when the input
        // is focused they fall through to the widget and type literally
        // (vim on or off), consistent with the honour-gate principle
        // below (bare `Char(_)` types into the input unless the list is
        // focused).
        //
        // Sits BELOW the toast clear above so a pending
        // feedback toast is dismissed by collapse/expand keypresses
        // like every other key (the toast clear must stay the first
        // state mutation of the handler).
        //
        // Shared by the section and overflow blocks below.
        let vim_fold = vim_mode && self.list_focused;
        if let Some(section) = self.selected_section
            && (prompt_empty || self.list_focused)
            && key.modifiers.is_empty()
        {
            match key.code {
                // Right/`l` expand, Left/`h` collapse (vim letters only while list-focused).
                KeyCode::Right | KeyCode::Char('l')
                    if matches!(key.code, KeyCode::Right) || vim_fold =>
                {
                    self.set_section_collapsed(section, false);
                    return InputOutcome::Changed;
                }
                KeyCode::Left | KeyCode::Char('h')
                    if matches!(key.code, KeyCode::Left) || vim_fold =>
                {
                    self.set_section_collapsed(section, true);
                    return InputOutcome::Changed;
                }
                KeyCode::Enter => {
                    self.toggle_section(section);
                    return InputOutcome::Changed;
                }
                _ => {}
            }
        }

        // Idle "N more" overflow: Enter toggles; Right/`l` reveal, Left/`h` re-fold.
        if self.selected_idle_overflow
            && (prompt_empty || self.list_focused)
            && key.modifiers.is_empty()
        {
            match key.code {
                KeyCode::Enter => {
                    self.toggle_idle_show_all();
                    return InputOutcome::Changed;
                }
                KeyCode::Right | KeyCode::Char('l')
                    if matches!(key.code, KeyCode::Right) || vim_fold =>
                {
                    self.idle_show_all = true;
                    return InputOutcome::Changed;
                }
                KeyCode::Left | KeyCode::Char('h')
                    if matches!(key.code, KeyCode::Left) || vim_fold =>
                {
                    self.idle_show_all = false;
                    return InputOutcome::Changed;
                }
                _ => {}
            }
        }

        // Short-terminal open (peek suppressed). Right: empty prompt or
        // list focus. Vim `l`: list focus only (same as `j`/`k`).
        let open_row_detail = key.modifiers.is_empty()
            && match key.code {
                KeyCode::Right => prompt_empty || self.list_focused,
                KeyCode::Char('l') if vim_mode => self.list_focused && !self.search_mode,
                _ => false,
            };
        if open_row_detail && let Some(id) = self.selected.clone() {
            return InputOutcome::Action(Action::DashboardAttach(id));
        }

        // Slash-completion dropdown intercept — Up/Down/Ctrl+P/N/Tab/
        // Enter/Esc steer the `/command` dropdown when it's open. Never
        // in search mode (there the buffer is a filter query, not a
        // command). Mirrors `agent_view::handle_prompt_key`. The model
        // catalog snapshot seeded at dashboard-open backs the `/model`
        // arg suggestions.
        //
        // `slash_accepted_send`: Enter accepted a terminal (no-arg) row and
        // must fall through to dispatch — bypasses multiline Enter→newline.
        let mut slash_accepted_send = false;
        if self.dispatch.slash_open() && !self.search_mode {
            match key.code {
                KeyCode::Up => {
                    self.dispatch.slash_move_selection(-1);
                    return InputOutcome::Changed;
                }
                KeyCode::Down => {
                    self.dispatch.slash_move_selection(1);
                    return InputOutcome::Changed;
                }
                KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                    self.dispatch.slash_move_selection(-1);
                    return InputOutcome::Changed;
                }
                KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                    self.dispatch.slash_move_selection(1);
                    return InputOutcome::Changed;
                }
                KeyCode::Tab => {
                    // Accept records MRU + queues an off-thread persist internally.
                    self.dispatch.accept_slash_completion(&self.models);
                    return InputOutcome::Changed;
                }
                KeyCode::Esc => {
                    self.dispatch.slash_close();
                    return InputOutcome::Changed;
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    // Accept the selected completion; if its insert text
                    // ends with a space the row "chains" (more input
                    // expected, e.g. `/model `), otherwise close the
                    // dropdown and fall through to the Enter handler which
                    // dispatches the slash command.
                    let snap = self.dispatch.slash_snapshot();
                    let chains = snap
                        .selection()
                        .is_some_and(|row| row.insert_text.ends_with(' '));
                    // Accept records MRU + queues an off-thread persist internally.
                    self.dispatch.accept_slash_completion(&self.models);
                    if chains {
                        return InputOutcome::Changed;
                    }
                    self.dispatch.slash_close();
                    slash_accepted_send = true;
                }
                _ => {}
            }
        }

        // `Ctrl+/` toggles search mode. Repurposes the dispatch
        // buffer as a live filter query (the prompt prefix flips to
        // a yellow `Search:`). Entering starts a fresh query;
        // toggling off cancels (clears the filter and restores the
        // normal dispatch input). Handled before the registry
        // lookup / Esc cascade so it works regardless of rebindings.
        if key!('/', CONTROL).matches(key) {
            if self.search_mode {
                self.exit_search_mode();
            } else {
                self.enter_search_mode();
            }
            return InputOutcome::Changed;
        }

        // Esc precedence: search → peek → clear filter → unfocus the
        // input (→ overview list) → deselect → exit dashboard. This sits
        // between the registry lookup and the action emission because Esc
        // is registered as `DashboardExit` but its multi-tier behaviour
        // can't be expressed as a single action — we expand it here.
        if matches!(key.code, KeyCode::Esc) {
            // Search mode owns Esc first — cancel the search (clear
            // filter + query) and return to the dispatch prompt.
            if self.search_mode {
                self.exit_search_mode();
                return InputOutcome::Changed;
            }
            if self.peek.is_some() {
                self.set_peek(None);
                return InputOutcome::Changed;
            }
            // An active filter clears next — the empty-state message
            // promises "press Esc to clear the filter", so this stays
            // ahead of the focus/exit tiers regardless of which pane
            // holds focus.
            if self.filter.is_active() {
                self.filter = Filter::None;
                return InputOutcome::Changed;
            }
            // Esc on a focused dispatch input UNFOCUSES it: focus moves
            // to the overview list so the user can navigate rows (↑/↓,
            // j/k, Space) right away. Mirrors Tab's focus toggle. The
            // typed draft is deliberately left intact — Esc never clears
            // it (use Ctrl+U / Ctrl+C for that), and it is retained if
            // you close and reopen the dashboard within the same app
            // process (it is NOT persisted across an app restart —
            // `PersistedDashboard` does not store the dispatch draft). A
            // second Esc — now that the list holds focus — walks the
            // back-out → exit cascade below.
            if !self.list_focused {
                self.list_focused = true;
                // Re-engage selection-follow so the viewport tracks the
                // cursor once the list takes focus (mirrors Tab).
                self.clear_manual_scroll();
                return InputOutcome::Changed;
            }
            // List focused: graduated back-out → exit. The deselect tier
            // keeps the "reply vs new session" contract — a selected row
            // makes the dispatch input reply to it, and deselecting flips
            // it back to "new session" mode without leaving the dashboard.
            if self.selected.is_some() {
                // Deselect → focus the `[+ New Agent]` button so
                // the cursor lands on a stable target instead of
                // floating in `None`. The Enter-with-empty-prompt
                // path keys off this to create-and-open, and the
                // header paints the focused button in
                // `accent_user` so the user can see where the
                // cursor went.
                self.focus_new_agent_button();
                // Pure-viewport scroll state is bound to the
                // cursor — once the cursor goes away, the next
                // render frame should re-anchor without snapping
                // to a stale offset.
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }
            if self.selected_section.is_some() {
                // Deselect the section header → focus the `[+ New Agent]`
                // button, mirroring the row-deselect tier above.
                self.focus_new_agent_button();
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }
            if self.selected_idle_overflow {
                // Deselect the Idle overflow toggle → focus the
                // `[+ New Agent]` button, mirroring the section tier.
                self.focus_new_agent_button();
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }
            return InputOutcome::Action(Action::ExitDashboard);
        }

        // Focus-aware routing of registry actions (the two-focus model):
        //  - ↑/↓ navigate the overview when it is focused OR the input is
        //    empty (a convenience so you can browse without first
        //    pressing Tab); with non-empty input they move the caret.
        //  - Bare letters (`j`/`k` vim nav, Space, …) act only when the
        //    OVERVIEW is focused — in the input they type.
        //    (`j`/`k` additionally require vim-mode, gated upstream by
        //    `lookup_with_mode`.) In search mode every bare letter types
        //    into the query.
        //  - Shortcuts help (`?`) is the exception: same convenience as
        //    arrows (list focused or empty draft). Non-empty draft types.
        //  - Ctrl combos (pin/stop/group/…) and Shift+arrows (reorder)
        //    act regardless of focus — they can't be typed.
        if let Some(id) = from_registry {
            let honor = match key.code {
                KeyCode::Up | KeyCode::Down if key.modifiers.is_empty() => {
                    self.list_focused || (prompt_empty && !self.search_mode)
                }
                KeyCode::Char(_)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    if id == crate::actions::ActionId::DashboardShortcutsHelp {
                        self.list_focused || (prompt_empty && !self.search_mode)
                    } else {
                        self.list_focused && !self.search_mode
                    }
                }
                _ => true,
            };
            if honor && let Some(outcome) = dashboard_action_for_id(id, &mut self.error_toast) {
                return outcome;
            }
        }

        // `/` is no longer special — it types a literal `/` into the
        // prompt (handled by the widget fall-through below). Filtering
        // lives behind the explicit `Ctrl+/` search mode instead, so a
        // dispatched prompt can start with `/`, `s:`, `a:`, or `#`
        // without being silently swallowed as a filter.

        // Ctrl+S = "send + open": dispatch the prompt (or open the
        // selected row / create from the button) AND walk straight into
        // the detail view. This is the chord that replaced Shift+Enter
        // (now freed, with Alt+Enter, for newline insertion). No-op in
        // search mode, where the buffer is a filter query, not a prompt.
        if key!('s', CONTROL).matches(key) && !self.search_mode {
            return self.dispatch_send_action(true);
        }

        // Ctrl+M: ToggleMultiline is PromptFocused only, so hardcode here.
        if key!('m', CONTROL).matches(key) && !self.search_mode {
            return InputOutcome::Action(Action::SetMultilineMode(!self.multiline_mode));
        }

        if matches!(key.code, KeyCode::Enter) {
            // Overview focused: attach / create (or send if button + draft).
            if self.list_focused && key.modifiers.is_empty() {
                if let Some(id) = self.selected.clone() {
                    return InputOutcome::Action(Action::DashboardAttach(id));
                }
                if self.new_agent_button_focused {
                    if !self.dispatch.text().trim().is_empty() {
                        return self.dispatch_send_action(false);
                    }
                    return InputOutcome::Action(Action::DashboardCreateNewAgentWithDetail);
                }
                return InputOutcome::Unchanged;
            }
            let mod_enter = crate::input::is_mod_enter(key);
            if self.search_mode {
                // Confirm filter; clear query so dispatch returns to ❯.
                self.search_mode = false;
                self.dispatch.set_text("");
                return InputOutcome::Changed;
            }
            // slash_accepted_send: no-arg slash accept must submit, not newline.
            let enter_is_newline =
                !slash_accepted_send && compose_enter_is_newline(self.multiline_mode, mod_enter);
            // Expand paste/file chips only for real bare Enter. Apple Terminal
            // rescue yields bare Enter while is_mod_enter is true — that must
            // send/newline, not expand (peek already gates the same way).
            if !mod_enter
                && matches!(
                    self.dispatch.try_element_interaction(key),
                    Some(crate::views::prompt_widget::ElementInteraction::Inlined)
                )
            {
                self.dispatch.refresh_slash(&self.models);
                return InputOutcome::Changed;
            }
            if enter_is_newline {
                // fall through for newline
            } else {
                return self.dispatch_send_action(false);
            }
        }

        // Shift+Tab (DashboardCycleMode) is resolved through the registry
        // `from_registry` path above — its `ActionDef` carries the three
        // terminal encodings (`BackTab`, `BackTab`+SHIFT, `Tab`+SHIFT) as
        // default/alt keys, so no hardcoded intercept is needed here.

        // Tab toggles focus between the dispatch input and the overview
        // list (the vim-style way to reach j/k navigation). When the
        // slash / `@` dropdowns are open the intercepts above already
        // consumed Tab (accept completion), so this only fires otherwise.
        if matches!(key.code, KeyCode::Tab) && key.modifiers.is_empty() {
            self.list_focused = !self.list_focused;
            // Re-engage selection-follow so the viewport tracks the
            // cursor once the list takes focus.
            self.clear_manual_scroll();
            return InputOutcome::Changed;
        }

        // Overview focused: the input is inactive. A printable key hands
        // focus back to the input so the user can start composing; `i`
        // (vim) enters the input WITHOUT typing the `i`, other bare
        // letters in vim are swallowed (normal-mode), and in non-vim any
        // printable returns focus AND is typed by the widget below. Nav /
        // command keys were already consumed above.
        if self.list_focused {
            if matches!(key.code, KeyCode::Char(_))
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
            {
                if vim_mode {
                    if key.code == KeyCode::Char('i') && key.modifiers.is_empty() {
                        self.list_focused = false;
                        return InputOutcome::Changed;
                    }
                    return InputOutcome::Unchanged;
                }
                self.list_focused = false;
                // fall through to the widget so the char is typed.
            } else {
                // Non-printable (Backspace/Home/…) while the overview is
                // focused must NOT leak into the inactive input.
                return InputOutcome::Unchanged;
            }
        }

        // (error_toast already cleared at top of handle_key.)

        // Forward to the prompt widget (single-line).
        let old = self.dispatch.text().to_string();
        let event = self.dispatch.handle_key(key);
        let new = self.dispatch.text().to_string();
        if old != new {
            // Live-update the filter as the user types ONLY in search
            // mode — the dispatch buffer is then the search query.
            // Outside search mode the buffer is a dispatch prompt and
            // never touches the filter (so `s:`/`a:`/`#`/`/` prefixes
            // dispatch verbatim). `parse_filter` still honours the
            // `a:`/`s:`/`#` prefixes WITHIN search mode for power
            // users; plain text is a substring match.
            let mut filter_changed = false;
            if self.search_mode {
                let trimmed = new.trim();
                self.filter = if trimmed.is_empty() {
                    Filter::None
                } else {
                    Filter::from_value(parse_filter(trimmed))
                };
                filter_changed = true;
            } else {
                // Outside search mode the buffer is a dispatch prompt;
                // refresh the slash snapshot so the `/command` dropdown
                // opens / updates (and `@` context) as the user types.
                self.dispatch.refresh_slash(&self.models);
            }
            if filter_changed {
                // Live filter edits reshape the visible row set; the
                // user's prior wheel-scrolled position no longer
                // points at a meaningful row. Re-engage the snap so
                // the viewport tracks selection again.
                self.manual_scroll_active = false;
            }
            InputOutcome::Changed
        } else if event == crate::views::prompt_widget::PromptEvent::Edited {
            InputOutcome::Changed
        } else {
            InputOutcome::Unchanged
        }
    }

    fn handle_mouse(&mut self, mouse: &crossterm::event::MouseEvent) -> InputOutcome {
        use crossterm::event::{MouseButton, MouseEventKind};

        self.last_mouse_pos = Some((mouse.column, mouse.row));

        // Update hover state when the mouse moves over a row or the
        // header's `[+ New Agent]` button.
        if matches!(mouse.kind, MouseEventKind::Moved) {
            let mut changed = self
                .new_agent_button_hit
                .update_hover(mouse.column, mouse.row);
            changed |= self.location_hit.update_hover(mouse.column, mouse.row);
            changed |= self.upgrade_cta_hit.update_hover(mouse.column, mouse.row);

            // Slash / @-file dropdown hover wins over row hover so the
            // completion list tracks the pointer while open (mirrors
            // agent-view mouse handling in `app/mouse.rs`).
            if let Some(dd_area) = self.slash_dropdown_items_area {
                let has_scrollbar = self.slash_dropdown_hit.has_scrollbar;
                let on_scrollbar =
                    has_scrollbar && mouse.column >= dd_area.x + dd_area.width.saturating_sub(2);
                let new_hover =
                    if dd_area.contains((mouse.column, mouse.row).into()) && !on_scrollbar {
                        self.slash_dropdown_hit
                            .row_items
                            .get((mouse.row - dd_area.y) as usize)
                            .copied()
                    } else {
                        None
                    };
                changed |= self.dispatch.set_slash_hovered(new_hover);
            } else {
                changed |= self.dispatch.set_slash_hovered(None);
            }
            if let Some(dd_area) = self.file_search_dropdown_items_area {
                let result_count = if self.peek.is_some() {
                    self.peek_reply.file_search.result_count()
                } else {
                    self.dispatch.file_search.result_count()
                };
                let scroll_offset = if self.peek.is_some() {
                    self.peek_reply.file_search.scroll_offset()
                } else {
                    self.dispatch.file_search.scroll_offset()
                };
                let has_scrollbar = result_count > dd_area.height as usize;
                let on_scrollbar =
                    has_scrollbar && mouse.column >= dd_area.x + dd_area.width.saturating_sub(2);
                let new_dd_hover =
                    if dd_area.contains((mouse.column, mouse.row).into()) && !on_scrollbar {
                        Some((mouse.row - dd_area.y) as usize + scroll_offset)
                    } else {
                        None
                    };
                changed |= self.dropdown_file_search_mut().set_hovered(new_dd_hover);
            } else {
                changed |= self.dispatch.file_search.set_hovered(None);
                if self.peek.is_some() {
                    changed |= self.peek_reply.file_search.set_hovered(None);
                }
            }

            let new_hover = self
                .row_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(id, _)| id.clone());
            if new_hover != self.hovered_row {
                self.hovered_row = new_hover;
                changed = true;
            }
            // Section-header hover → the renderer brightens its text.
            let new_hover_section = self
                .section_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(key, _)| *key);
            if new_hover_section != self.hovered_section {
                self.hovered_section = new_hover_section;
                changed = true;
            }
            // Idle overflow row hover → the renderer brightens its text.
            let new_hover_overflow = self.idle_overflow_rect.is_some_and(|r| {
                mouse.column >= r.x
                    && mouse.column < r.x + r.width
                    && mouse.row >= r.y
                    && mouse.row < r.y + r.height
            });
            if new_hover_overflow != self.hovered_idle_overflow {
                self.hovered_idle_overflow = new_hover_overflow;
                changed = true;
            }
            if changed {
                return InputOutcome::Changed;
            }
            return InputOutcome::Unchanged;
        }

        // Click on the peek-panel close button.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some(rect) = self.peek_close_rect
            && mouse.column >= rect.x
            && mouse.column < rect.x + rect.width
            && mouse.row >= rect.y
            && mouse.row < rect.y + rect.height
        {
            // `set_peek(None)` enforces the
            // peek/close-rect invariant atomically.
            self.set_peek(None);
            return InputOutcome::Changed;
        }

        // Peek reply input — mouse interaction with the `❯ reply` row
        // (or the reject-feedback slot in question mode), mirroring the
        // dispatch box's click-to-focus plus the agent prompt's drag
        // text selection:
        //   - a left click inside the recorded rect focuses the reply
        //     and forwards to the widget so the caret lands under the
        //     pointer (double/triple-click word/line selection included);
        //   - Drag/Up anywhere continue a selection drag that STARTED in
        //     the input (the textarea tracks its drag state internally;
        //     stray Drag/Up events with no active drag are no-ops), so
        //     dragging past the box edge keeps selecting.
        if self.peek.is_some() {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(rect) = self.peek_reply_rect
                        && mouse.column >= rect.x
                        && mouse.column < rect.x + rect.width
                        && mouse.row >= rect.y
                        && mouse.row < rect.y + rect.height
                    {
                        if let Some(p) = self.peek.as_mut() {
                            p.focused = true;
                        }
                        let _ = self.peek_reply.handle_mouse(mouse);
                        let now = Instant::now();
                        if self.last_prompt_click.is_some_and(|last| {
                            now.duration_since(last).as_millis() < PROMPT_MULTI_CLICK_MS
                        }) {
                            let _ = self.peek_reply.expand_paste_element_at_cursor();
                        }
                        self.last_prompt_click = Some(now);
                        return InputOutcome::Changed;
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                    if !self.peek_reply.text().is_empty() =>
                {
                    let _ = self.peek_reply.handle_mouse(mouse);
                    return InputOutcome::Changed;
                }
                _ => {}
            }
        }

        // Same Drag/Up treatment for the dispatch box, gated off while a
        // peek is open (the dispatch input is hidden then) and in search
        // mode (the search line renders outside the textarea, matching
        // the Down forwarding below).
        if self.peek.is_none()
            && !self.search_mode
            && !self.dispatch.text().is_empty()
            && matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            )
        {
            let _ = self.dispatch.handle_mouse(mouse);
            return InputOutcome::Changed;
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            // Slash dropdown click must run BEFORE row attach: the model
            // list (and other arg suggestions) paints over agent rows,
            // so without this a mouse click falls through to
            // `DashboardAttach` and opens the underlying session.
            if let Some(dd_area) = self.slash_dropdown_items_area
                && dd_area.contains((mouse.column, mouse.row).into())
            {
                let snap = self.dispatch.slash_snapshot();
                let has_scrollbar = self.slash_dropdown_hit.has_scrollbar;
                let on_scrollbar =
                    has_scrollbar && mouse.column >= dd_area.x + dd_area.width.saturating_sub(2);

                if on_scrollbar {
                    let click_frac = (mouse.row - dd_area.y) as f64 / dd_area.height.max(1) as f64;
                    let target = (click_frac * snap.matches.len() as f64) as usize;
                    let max = snap.matches.len().saturating_sub(1);
                    let delta = target.min(max) as isize - snap.selected as isize;
                    self.dispatch.slash_move_selection(delta);
                    self.dispatch.slash_preview_current_selection();
                } else if let Some(&item_idx) = self
                    .slash_dropdown_hit
                    .row_items
                    .get((mouse.row - dd_area.y) as usize)
                {
                    self.dispatch.set_slash_hovered(Some(item_idx));
                    if self.dispatch.select_slash_hovered() {
                        self.dispatch.slash_commit_preview();
                        self.dispatch.accept_slash_completion(&self.models);
                    }
                }
                self.list_focused = false;
                return InputOutcome::Changed;
            }

            // File-search (`@`) dropdown click — same priority as slash:
            // absorb the hit so it never attaches a session row under the
            // list. Uses the on-screen picker (peek reply while peeking,
            // otherwise the dispatch box).
            if let Some(dd_area) = self.file_search_dropdown_items_area
                && dd_area.contains((mouse.column, mouse.row).into())
            {
                let result_count = if self.peek.is_some() {
                    self.peek_reply.file_search.result_count()
                } else {
                    self.dispatch.file_search.result_count()
                };
                let scroll_offset = if self.peek.is_some() {
                    self.peek_reply.file_search.scroll_offset()
                } else {
                    self.dispatch.file_search.scroll_offset()
                };
                let has_scrollbar = result_count > dd_area.height as usize;
                let on_scrollbar =
                    has_scrollbar && mouse.column >= dd_area.x + dd_area.width.saturating_sub(2);

                if on_scrollbar {
                    let click_frac = (mouse.row - dd_area.y) as f64 / dd_area.height.max(1) as f64;
                    let target = (click_frac * result_count as f64) as usize;
                    let max = result_count.saturating_sub(1);
                    let selected = if self.peek.is_some() {
                        self.peek_reply.file_search.selected()
                    } else {
                        self.dispatch.file_search.selected()
                    };
                    self.dropdown_file_search_mut()
                        .move_selection(target.min(max) as isize - selected as isize);
                } else {
                    let row_idx = (mouse.row - dd_area.y) as usize + scroll_offset;
                    let fs = self.dropdown_file_search_mut();
                    fs.set_hovered(Some(row_idx));
                    if fs.select_hovered() {
                        if self.peek.is_some() {
                            self.peek_reply.accept_file_search_result();
                        } else {
                            self.dispatch.accept_file_search_result();
                        }
                    }
                }
                self.list_focused = false;
                return InputOutcome::Changed;
            }

            // Click on the `[+ New Agent]` button — same outcome
            // as Enter-with-empty-prompt while the button is
            // focused. Routed through the dispatcher so the
            // create-session + view-switch sequence stays in one
            // place. The hit test runs BEFORE the row-click check
            // so the button can sit inside the header rect
            // without competing for clicks.
            if self.new_agent_button_hit.contains(mouse.column, mouse.row) {
                self.focus_new_agent_button();
                self.manual_scroll_active = false;
                return InputOutcome::Action(Action::DashboardCreateNewAgentWithDetail);
            }

            // Click on the header upgrade CTA `[label]` → open the promo url
            // (resolved through the slot gate at dispatch time).
            if self.upgrade_cta_hit.contains(mouse.column, mouse.row) {
                return InputOutcome::Action(Action::AnnouncementsOpenCta(
                    xai_grok_telemetry::events::AnnouncementCtaSurface::Dashboard,
                ));
            }

            // Click on the header location label → open the location
            // picker. Sits next to the `[+ New Agent]` check since both
            // are header affordances in separate columns.
            if self.location_hit.contains(mouse.column, mouse.row) {
                return InputOutcome::Action(Action::DashboardOpenLocationPicker);
            }

            // Click on a section header → select it and toggle collapse.
            // Checked before the row hit-test (separate layout region).
            if let Some(key) = self
                .section_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(k, _)| *k)
            {
                self.focus_section(key);
                self.toggle_section(key);
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }

            // Click on the Idle "N more" overflow row →
            // focus it and flip show-all (mirrors the section-header
            // click). Checked before the row hit-test; the overflow row
            // is never in `row_rects`.
            if let Some(rect) = self.idle_overflow_rect
                && mouse.column >= rect.x
                && mouse.column < rect.x + rect.width
                && mouse.row >= rect.y
                && mouse.row < rect.y + rect.height
            {
                self.focus_idle_overflow();
                self.toggle_idle_show_all();
                self.manual_scroll_active = false;
                return InputOutcome::Changed;
            }

            // Single-click attaches (opens the
            // conversation popup) immediately. A prior double-click-within-500ms
            // design felt unresponsive: users tap a row
            // and expect the conversation to open. Modern TUI tools
            // (gh-dash, k9s, lazygit) all use click-to-open on list
            // entries; keyboard navigation (↑/↓) is the way to
            // browse without attaching.
            if let Some(id) = self
                .row_rects
                .iter()
                .find(|(_, r)| {
                    mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                })
                .map(|(id, _)| id.clone())
            {
                // Clicking a row is selection-driven, so re-engage
                // the clamp's snap-to-selection by clearing the
                // manual-scroll flag. Without this, a click after a
                // wheel-scroll would jump the viewport on the next
                // frame (the bias-up pull-back kicks in).
                self.manual_scroll_active = false;
                // `focus_row` also clears `new_agent_button_focused`
                // so the two cursor states stay mutually exclusive
                // (clicking a row while the button was focused
                // hands the cursor over to the row).
                self.focus_row(id.clone());
                self.last_click = Some((id.clone(), Instant::now()));
                return InputOutcome::Action(Action::DashboardAttach(id));
            }

            // Click anywhere on the dispatch input box focuses it —
            // i.e. clears `list_focused`. This must work whether or not
            // vim mode is on: in vim mode the overview steals the
            // keyboard (j/k nav), so without this a mouse user clicking
            // the box would be stranded with no caret. Checked after the
            // button + row hit tests since those sit in separate layout
            // regions and shouldn't compete.
            if let Some(rect) = self.dispatch_rect
                && mouse.column >= rect.x
                && mouse.column < rect.x + rect.width
                && mouse.row >= rect.y
                && mouse.row < rect.y + rect.height
            {
                self.list_focused = false;
                // Forward the click so the caret lands where the user
                // clicked. Skipped in search mode, where the prompt
                // renders its own single-line cursor with a `Search:`
                // prefix rather than through the textarea.
                if !self.search_mode {
                    let _ = self.dispatch.handle_mouse(mouse);
                    let now = Instant::now();
                    if self.last_prompt_click.is_some_and(|last| {
                        now.duration_since(last).as_millis() < PROMPT_MULTI_CLICK_MS
                    }) && self.dispatch.expand_paste_element_at_cursor()
                    {
                        self.dispatch.refresh_slash(&self.models);
                    }
                    self.last_prompt_click = Some(now);
                }
                return InputOutcome::Changed;
            }
        }

        InputOutcome::Unchanged
    }

    /// Route input to the open location picker. Caller has confirmed
    /// `location_picker.is_some()` (the gate in [`Self::handle_input`]).
    /// Keys flow through the shared [`crate::views::picker`] handler (nav +
    /// query editing) except Enter, which is intercepted here to apply the
    /// chosen path. After any query edit the directory listing is
    /// refreshed; mouse flows through the modal chrome then the rows.
    fn handle_location_picker_input(&mut self, ev: &Event) -> InputOutcome {
        // Mouse is handled separately so the `location_picker` borrow
        // below doesn't overlap the `&mut self` mouse helper.
        if let Event::Mouse(mouse) = ev {
            return self.handle_location_picker_mouse(mouse);
        }

        let Some(lp) = self.location_picker.as_mut() else {
            return InputOutcome::Unchanged;
        };

        if let Event::Key(key) = ev
            && key.kind != KeyEventKind::Release
            && key.code == KeyCode::Enter
            && key.modifiers.is_empty()
        {
            lp.refresh_suggestions();
            return match lp.chosen_input() {
                Some(input) => InputOutcome::Action(Action::DashboardChangeLocation { input }),
                None => InputOutcome::Unchanged,
            };
        }

        // Tab — shell-style completion: fill the input with the selected
        // row's path (tilde-collapsed) plus a trailing `/` so the next
        // listing drills into it. No-op when nothing is selected.
        if let Event::Key(key) = ev
            && key.kind != KeyEventKind::Release
            && key.code == KeyCode::Tab
            && key.modifiers.is_empty()
        {
            let visible = lp.visible_candidates();
            let Some(c) = visible.get(lp.picker.selected) else {
                return InputOutcome::Unchanged;
            };
            let mut filled = crate::project_picker::sources::display_path(&c.path);
            if !filled.ends_with('/') {
                filled.push('/');
            }
            lp.picker.set_query(filled);
            lp.picker.selected = 0;
            lp.picker.scroll_offset = None;
            // The path changed — drop any stale "Not a directory" error.
            lp.error = None;
            lp.refresh_suggestions();
            return InputOutcome::Changed;
        }

        let entry_count = lp.visible_candidates().len();
        let config = location_picker_config();
        let outcome =
            crate::views::picker::handle_picker_input(ev, &mut lp.picker, entry_count, &config);
        // When the user edits the path, drop the stale validation error so a
        // corrected (possibly valid) path isn't shown next to a red
        // "Not a directory" left over from the previous failed attempt.
        if matches!(&outcome, crate::views::picker::PickerOutcome::QueryChanged) {
            lp.error = None;
            // Re-list only when the edited path changes; cursor motion is redraw-only.
            lp.refresh_suggestions();
        }
        match outcome {
            crate::views::picker::PickerOutcome::Closed => {
                InputOutcome::Action(Action::DashboardCloseLocationPicker)
            }
            crate::views::picker::PickerOutcome::Changed
            | crate::views::picker::PickerOutcome::QueryChanged => InputOutcome::Changed,
            _ => InputOutcome::Unchanged,
        }
    }

    /// Mouse handling for the open location picker: modal chrome (close
    /// button / click-outside) first, then content-row click (select +
    /// apply) and hover (move the cursor).
    fn handle_location_picker_mouse(
        &mut self,
        mouse: &crossterm::event::MouseEvent,
    ) -> InputOutcome {
        use crate::views::modal_window::{ModalWindowOutcome, handle_modal_mouse};
        use crossterm::event::{MouseButton, MouseEventKind};

        let Some(lp) = self.location_picker.as_mut() else {
            return InputOutcome::Unchanged;
        };

        match handle_modal_mouse(&mut lp.window, mouse.kind, mouse.column, mouse.row) {
            ModalWindowOutcome::CloseRequested => {
                return InputOutcome::Action(Action::DashboardCloseLocationPicker);
            }
            ModalWindowOutcome::Handled => return InputOutcome::Changed,
            _ => {}
        }

        // Worktree toggle button on the path row: a left-click flips
        // `worktree_mode`, which (when the location is applied) arms the
        // dashboard's worktree dispatch.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && lp.worktree_hit.contains(mouse.column, mouse.row)
        {
            lp.worktree_mode = !lp.worktree_mode;
            return InputOutcome::Changed;
        }

        // Map the cursor to a content row (visible-list index) without
        // holding the `content_hits` borrow past the lookup.
        let hit_idx: Option<usize> = lp.content_hits.as_ref().and_then(|hits| {
            hits.item_rects.iter().enumerate().find_map(|(i, r)| {
                let inside = mouse.column >= r.x
                    && mouse.column < r.x + r.width
                    && mouse.row >= r.y
                    && mouse.row < r.y + r.height;
                inside.then(|| hits.entry_indices.get(i).copied().unwrap_or(i))
            })
        });

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(entry_idx) = hit_idx {
                    let visible = lp.visible_candidates();
                    if let Some(c) = visible.get(entry_idx) {
                        let input = c.path.to_string_lossy().into_owned();
                        return InputOutcome::Action(Action::DashboardChangeLocation { input });
                    }
                }
                InputOutcome::Unchanged
            }
            MouseEventKind::Moved => {
                // Brighten the worktree button when hovered (its rect was
                // set on the prior render).
                let wt_changed = lp.worktree_hit.update_hover(mouse.column, mouse.row);
                let row_changed = match hit_idx {
                    Some(entry_idx) if lp.picker.selected != entry_idx => {
                        lp.picker.selected = entry_idx;
                        true
                    }
                    _ => false,
                };
                if wt_changed || row_changed {
                    InputOutcome::Changed
                } else {
                    InputOutcome::Unchanged
                }
            }
            _ => InputOutcome::Unchanged,
        }
    }

    /// Route input to the worktree-label dialog while it is open. Submit
    /// confirms via [`Action::DashboardConfirmWorktree`] (the dispatcher
    /// creates the worktree session and replays any stashed prompt); Esc /
    /// Ctrl+C cancels, clearing the dialog and the stashed prompt. Caller
    /// has confirmed `worktree_dialog.is_some()` (the gate in
    /// [`Self::handle_input`]).
    fn handle_worktree_dialog_input(&mut self, ev: &Event) -> InputOutcome {
        use crate::app::app_view::NewWorktreeDialogOutcome;
        let Some(dialog) = self.worktree_dialog.as_mut() else {
            return InputOutcome::Unchanged;
        };
        let outcome = match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => dialog.handle_key(key),
            Event::Paste(text) => dialog.insert_paste(text),
            _ => return InputOutcome::Unchanged,
        };
        match outcome {
            NewWorktreeDialogOutcome::Submitted(label) => {
                self.worktree_dialog = None;
                InputOutcome::Action(Action::DashboardConfirmWorktree { label })
            }
            NewWorktreeDialogOutcome::Cancelled => {
                self.worktree_dialog = None;
                // Don't silently discard the user's typed prompt — restore the
                // stashed prompt (from the prompt-send path) to the dispatch
                // input so they can resend it instead of losing it. Mirrors the
                // restore in `dispatch_dashboard_confirm_worktree`'s not-a-repo
                // error path. When the dialog was opened from the [+ New Agent]
                // button there's no stash, so `take()` yields `None` and the
                // input is left untouched.
                if let Some(prompt) = self.pending_worktree_prompt.take() {
                    self.dispatch.restore(prompt);
                }
                InputOutcome::Changed
            }
            NewWorktreeDialogOutcome::Changed => InputOutcome::Changed,
            NewWorktreeDialogOutcome::Unchanged => InputOutcome::Unchanged,
        }
    }

    /// Route a top-level event to the open shortcuts cheatsheet
    /// modal. Caller has already confirmed `shortcuts_modal.is_some()`
    /// — that gate sits in [`Self::handle_input`] so the no-modal
    /// fast path stays small. Returns an `InputOutcome::Action`
    /// when the modal asked to close (so the dispatcher can clear
    /// the field through the same channel that the close-button
    /// click uses), `Changed` for any visual mutation, and
    /// `Unchanged` otherwise. Mouse events route through
    /// `shortcuts_help::handle_mouse`; key events go through the
    /// chrome + picker pipeline via `handle_modal_key`.
    fn handle_shortcuts_modal_input(&mut self, ev: &Event) -> InputOutcome {
        use crate::views::shortcuts_help::{
            ModalKeyOutcome, ShortcutsHelpOutcome, handle_modal_key, handle_mouse, handle_paste,
            toggle_membership,
        };

        let Some(modal) = self.shortcuts_modal.as_mut() else {
            return InputOutcome::Unchanged;
        };
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                match handle_modal_key(
                    key,
                    &modal.entries,
                    &mut modal.state,
                    &mut modal.window,
                    modal.filter_active,
                    &modal.collapsed_sections,
                    &modal.expanded_ids,
                    &mut modal.mode,
                    /* compact */ false,
                ) {
                    ModalKeyOutcome::Close => {
                        InputOutcome::Action(Action::DashboardCloseShortcutsHelp)
                    }
                    ModalKeyOutcome::ToggleFilter => {
                        modal.filter_active = !modal.filter_active;
                        modal.state.selected = 0;
                        InputOutcome::Changed
                    }
                    ModalKeyOutcome::ToggleSection(idx) => {
                        toggle_membership(&mut modal.collapsed_sections, idx);
                        InputOutcome::Changed
                    }
                    ModalKeyOutcome::ToggleExpand(action_id) => {
                        toggle_membership(&mut modal.expanded_ids, action_id);
                        InputOutcome::Changed
                    }
                    ModalKeyOutcome::Changed => InputOutcome::Changed,
                    ModalKeyOutcome::Unchanged => InputOutcome::Unchanged,
                }
            }
            Event::Mouse(mouse) => {
                // Chrome first — the `[✗]` close button (click + hover
                // brightening) and click-outside-to-close live on the
                // modal-window chrome, not in the picker content. The
                // picker's own `hit.close_button` is a dead
                // `Rect::default()`, so skipping this step left the
                // button inert. Mirrors `agent_view::handle_modal_mouse`.
                match crate::views::modal_window::handle_modal_mouse(
                    &mut modal.window,
                    mouse.kind,
                    mouse.column,
                    mouse.row,
                ) {
                    crate::views::modal_window::ModalWindowOutcome::CloseRequested => {
                        return InputOutcome::Action(Action::DashboardCloseShortcutsHelp);
                    }
                    crate::views::modal_window::ModalWindowOutcome::Handled => {
                        return InputOutcome::Changed;
                    }
                    // Everything else (incl. wheel) belongs to the
                    // picker content below.
                    _ => {}
                }
                match handle_mouse(
                    mouse,
                    &modal.entries,
                    &mut modal.state,
                    modal.filter_active,
                    &modal.collapsed_sections,
                    &mut modal.mode,
                ) {
                    ShortcutsHelpOutcome::Close => {
                        InputOutcome::Action(Action::DashboardCloseShortcutsHelp)
                    }
                    ShortcutsHelpOutcome::ToggleFilter => {
                        modal.filter_active = !modal.filter_active;
                        modal.state.selected = 0;
                        InputOutcome::Changed
                    }
                    ShortcutsHelpOutcome::ToggleSection(idx) => {
                        toggle_membership(&mut modal.collapsed_sections, idx);
                        InputOutcome::Changed
                    }
                    // Unreachable today: handle_mouse never yields ToggleExpand (a row click opens detail); kept for exhaustiveness.
                    ShortcutsHelpOutcome::ToggleExpand(action_id) => {
                        toggle_membership(&mut modal.expanded_ids, action_id);
                        InputOutcome::Changed
                    }
                    ShortcutsHelpOutcome::Changed => InputOutcome::Changed,
                    ShortcutsHelpOutcome::Unchanged => InputOutcome::Unchanged,
                }
            }
            Event::Paste(text) => match handle_paste(text, &mut modal.state, &modal.mode) {
                ShortcutsHelpOutcome::Changed => InputOutcome::Changed,
                _ => InputOutcome::Unchanged,
            },
            _ => InputOutcome::Unchanged,
        }
    }

    /// Clamp the cursor to a still-visible row when the previous
    /// selection has disappeared. A deliberately-cleared selection
    /// (`None`) is PRESERVED — the dashboard's
    /// "no row selected → dispatch creates a new session,
    /// row selected → dispatch replies to that agent" contract
    /// hinges on `None` being a valid steady state. The previous
    /// auto-promotion-to-first-row behaviour would have hijacked
    /// the user's reply path the moment they pressed Esc to
    /// deselect.
    pub fn reanchor_selection(&mut self, rows: &[DashboardRow]) {
        // Section-cursor validation — a selected section header can go
        // stale without any grouping toggle: a `s:state` filter
        // suppresses all state headers, and row churn (the last
        // Working agent going idle) removes the header outright.
        // Re-derive the focusable set the renderer is about to paint
        // and, when the cursor's header is gone, move it to the
        // `[+ New Agent]` button (mirroring `toggle_grouping`) so the
        // footer hints and the Right/Left/Enter collapse keys never
        // act on an invisible section.
        let focusables = super::render::focusables(
            rows,
            self.grouping,
            &self.filter,
            &self.collapsed_sections,
            self.idle_show_all,
            self.search_mode,
        );
        if let Some(key) = self.selected_section {
            let alive = focusables
                .iter()
                .any(|f| matches!(f, Focusable::Section(k) if *k == key));
            if !alive {
                self.focus_new_agent_button();
            }
        }
        // Idle-overflow cursor validation — the "N more" toggle row
        // disappears when the Idle group shrinks below the cap (or the
        // user expands it, which removes the fold). A stranded cursor
        // there would leave Enter/footer hints acting on nothing, so
        // fall back to the `[+ New Agent]` button.
        if self.selected_idle_overflow
            && !focusables
                .iter()
                .any(|f| matches!(f, Focusable::IdleOverflow))
        {
            self.focus_new_agent_button();
        }
        // Row-cursor visibility — a selected row can vanish WITHOUT
        // leaving `rows`: state churn can migrate it into a collapsed
        // section (e.g. a selected Idle row starts Working while
        // "Working" is collapsed). The existence check below wouldn't
        // catch that (the row is still in `rows`), leaving the cursor
        // on an invisible row — footer hints and peek for a row you
        // can't see, and ↑/↓ resetting to the top. Move the cursor to
        // the section header that hid the row (the collapsed header is
        // always visible) so the user is one `→`/`Enter` away from
        // revealing it again.
        if let Some(sel) = self.selected.clone() {
            let visible = focusables
                .iter()
                .any(|f| matches!(f, Focusable::Row(id) if *id == sel));
            if !visible
                && let Some(key) =
                    super::render::section_of_row(rows, self.grouping, &self.filter, &sel)
            {
                self.focus_section(key);
            }
        }
        // Skip non-selectable placeholders ("… N more").
        let selectable: Vec<&DashboardRow> =
            rows.iter().filter(|r| !r.is_more_placeholder).collect();
        if selectable.is_empty() {
            self.selected = None;
            return;
        }
        if let Some(sel) = self.selected.as_ref()
            && !selectable.iter().any(|r| r.id == *sel)
        {
            // The previously selected row was filtered out / closed
            // / lost its parent. Drop the cursor — re-selecting is
            // the user's job.
            self.selected = None;
        }
    }
}

/// Strict Enter swap for compose surfaces (agent prompt parity).
/// Multiline off: Shift/Alt (or rescued) Enter → newline.
/// Multiline on: bare Enter → newline; Shift/Alt → send/create/open.
fn compose_enter_is_newline(multiline: bool, mod_enter: bool) -> bool {
    multiline != mod_enter
}

/// Exhaustive map from `ActionId` → `InputOutcome` for
/// dashboard-focused actions. Adding a new `Dashboard*` `ActionId`
/// without wiring it here is a compile error (the `_` arm is gone).
///
/// Returns `None` for non-dashboard `ActionId`s; the caller falls
/// through to widget input.
///
/// Convention for the contributor adding a NEW
/// `ActionId` variant in the future: if the new variant is
/// dashboard-specific, add a `Some(...)` arm; otherwise add it to
/// the `None` "Non-dashboard" arm. The match below is exhaustive on
/// purpose — there is no `_` arm — so the compiler will fail the
/// build if you forget. A future refactor could carve a separate
/// `DashboardActionId` enum to make this constraint type-system-enforced,
/// but the current convention + exhaustive match is sufficient.
fn dashboard_action_for_id(
    id: crate::actions::ActionId,
    error_toast: &mut Option<String>,
) -> Option<InputOutcome> {
    use crate::actions::ActionId;
    match id {
        ActionId::DashboardSelectNext => Some(InputOutcome::Action(Action::DashboardSelectNext)),
        ActionId::DashboardSelectPrev => Some(InputOutcome::Action(Action::DashboardSelectPrev)),
        ActionId::DashboardTogglePin => Some(InputOutcome::Action(Action::DashboardTogglePin)),
        ActionId::DashboardBeginRename => Some(InputOutcome::Action(Action::DashboardBeginRename)),
        ActionId::DashboardStop => Some(InputOutcome::Action(Action::DashboardStop)),
        ActionId::DashboardCycleMode => Some(InputOutcome::Action(Action::DashboardCycleMode)),
        ActionId::DashboardToggleAutoApprove => {
            Some(InputOutcome::Action(Action::DashboardToggleAutoApprove))
        }
        ActionId::DashboardToggleWorktree => {
            Some(InputOutcome::Action(Action::DashboardToggleWorktree))
        }
        ActionId::DashboardOpenLocationPicker => {
            Some(InputOutcome::Action(Action::DashboardOpenLocationPicker))
        }
        ActionId::DashboardToggleGrouping => {
            Some(InputOutcome::Action(Action::DashboardToggleGrouping))
        }
        ActionId::DashboardReorderUp => Some(InputOutcome::Action(Action::DashboardReorderUp)),
        ActionId::DashboardReorderDown => Some(InputOutcome::Action(Action::DashboardReorderDown)),
        ActionId::DashboardShortcutsHelp => {
            // Open a real cheatsheet modal (mirroring the
            // agent view's `ActiveModal::ShortcutsHelp`) instead
            // of stuffing a single-line hint into the dispatch
            // input via `error_toast`. The previous toast bled
            // through the prompt-placeholder slot, which the user
            // expected to be reserved for *their* typing target.
            // The modal opens via the action dispatcher so the
            // build-entries side-effect lives next to other
            // dashboard dispatchers in `app::dispatch`.
            let _ = error_toast;
            Some(InputOutcome::Action(Action::DashboardOpenShortcutsHelp))
        }
        ActionId::DashboardExit => {
            // Esc is handled in the cascade above. A user-rebind of
            // exit (e.g. F1) lands here.
            Some(InputOutcome::Action(Action::ExitDashboard))
        }
        // Non-dashboard ActionIds: fall through to the widget. This
        // is the ONLY arm that should ever match; the compiler will
        // flag a missing case when a new Dashboard* action is added.
        ActionId::SendPrompt
        | ActionId::InterjectPrompt
        | ActionId::ScrollUp
        | ActionId::ScrollDown
        | ActionId::PageUp
        | ActionId::PageDown
        | ActionId::HalfPageUp
        | ActionId::HalfPageDown
        | ActionId::GotoTop
        | ActionId::GotoBottom
        | ActionId::SelectNext
        | ActionId::SelectPrev
        | ActionId::NextTurn
        | ActionId::PrevTurn
        | ActionId::NextResponse
        | ActionId::PrevResponse
        | ActionId::Collapse
        | ActionId::Expand
        | ActionId::ToggleFold
        | ActionId::ToggleExpandAll
        | ActionId::ExpandAllThinking
        | ActionId::ToggleRaw
        | ActionId::ToggleMouseCapture
        | ActionId::NextModel
        | ActionId::CancelTurn
        | ActionId::ToggleYolo
        | ActionId::ToggleMultiline
        | ActionId::FocusPrompt
        | ActionId::FocusScrollback
        | ActionId::CopyBlockContent
        | ActionId::CopyBlockMeta
        | ActionId::OpenBlockViewer
        | ActionId::OpenNextLink
        | ActionId::OpenPrevLink
        | ActionId::ToggleTodos
        | ActionId::ToggleTasks
        | ActionId::EditPromptExternal
        | ActionId::ToggleQueue
        | ActionId::OpenSessions
        | ActionId::OpenExtensions
        | ActionId::SendToBackground
        | ActionId::CycleMode
        | ActionId::BashMode
        | ActionId::Rewind
        | ActionId::KillBgTask
        | ActionId::DumpInputLog
        | ActionId::Quit
        | ActionId::NewSession
        | ActionId::ExitSession
        | ActionId::NewSessionInWorktree
        | ActionId::CommandPalette
        | ActionId::ModelPicker
        | ActionId::ShortcutsHelp
        | ActionId::OpenSettings
        | ActionId::OpenDashboard
        | ActionId::EnableVoiceMode
        | ActionId::VoiceToggle
        // Overlay actions are intercepted at the AppView level
        // before they reach the dashboard's own input loop; they
        // can never arrive here.
        | ActionId::DashboardOverlayExit
        | ActionId::DashboardOverlayPrev
        | ActionId::DashboardOverlayNext
        | ActionId::DashboardOverlayStop => None,
    }
}

fn handle_rename_key(draft: &mut RenameDraft, key: &KeyEvent) -> InputOutcome {
    use crate::input::key::is_altgr;
    match key.code {
        KeyCode::Esc => return InputOutcome::Action(Action::DashboardCancelRename),
        KeyCode::Enter if key.modifiers.is_empty() => {
            return InputOutcome::Action(Action::DashboardCommitRename);
        }
        KeyCode::Char('c')
            if key.modifiers.contains(KeyModifiers::CONTROL) && !is_altgr(key.modifiers) =>
        {
            return InputOutcome::Action(Action::DashboardCancelRename);
        }
        _ => {}
    }

    let can_insert = draft.text().chars().count() < MAX_RENAME_SCALARS;
    let outcome = draft
        .editor
        .handle_key_with_insert_policy(key, |character| {
            can_insert && rename_character_allowed(character)
        });
    rename_edit_outcome(outcome)
}

fn handle_rename_paste(draft: &mut RenameDraft, text: &str) -> InputOutcome {
    let remaining = MAX_RENAME_SCALARS.saturating_sub(draft.text().chars().count());
    let outcome =
        draft
            .editor
            .insert_paste_with_policy(text, rename_wire_character_allowed, remaining);
    rename_edit_outcome(outcome)
}

fn rename_edit_outcome(outcome: LineEditOutcome) -> InputOutcome {
    match outcome {
        LineEditOutcome::TextChanged
        | LineEditOutcome::HandledNoChange
        | LineEditOutcome::CursorChanged => InputOutcome::Changed,
        LineEditOutcome::Unhandled => InputOutcome::Unchanged,
    }
}

// ---------------------------------------------------------------------------
// Filter parser (edge case 11)
// ---------------------------------------------------------------------------

/// Parse a filter expression from the dispatch input.
///
/// Rules per edge case 11:
/// - `a:` (empty) → no-op (clear filter).
/// - `a:<name>` → match by agent label (case-insensitive substring).
/// - `s:` (empty) → substring match.
/// - `s:<state>` → match by row state; accepts synonyms
///   `needs-input`/`needs_input`/`needsinput`/`blocked`/`completed`/
///   `failed`/`idle`/`working`. Unknown values fall back to substring.
/// - `#<n>` → Phase 2 stub: treat as substring filter (PR support is
///   out of scope; we still match against label + cwd so users see
///   nothing useless on screen).
/// - Anything else → substring on label + cwd.
pub fn parse_filter(text: &str) -> FilterValue {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("a:") {
        let rest = rest.trim();
        if rest.is_empty() {
            FilterValue::None
        } else {
            FilterValue::Agent(rest.to_string())
        }
    } else if let Some(rest) = trimmed.strip_prefix("s:") {
        let rest = rest.trim();
        if rest.is_empty() {
            // `s:` empty is consistent with `a:` empty:
            // both clear the filter.
            FilterValue::None
        } else if let Some(rs) = parse_row_state_token(rest) {
            FilterValue::State(rs)
        } else {
            // Unknown state token falls back to substring
            // on the full `s:foobar` so the user sees feedback (their
            // typed text is matched against labels) AND realises the
            // state path didn't take effect.
            FilterValue::Substring(rest.to_string())
        }
    } else if let Some(rest) = trimmed.strip_prefix('#') {
        // `#<n>` keeps the `#` in the substring needle so
        // it never matches arbitrary digits in labels. PR filtering
        // is reserved for a future revision.
        FilterValue::Substring(format!("#{rest}"))
    } else {
        FilterValue::Substring(trimmed.to_string())
    }
}

/// Parse a `RowState` from a user token. Accepts common synonyms.
pub fn parse_row_state_token(s: &str) -> Option<RowState> {
    let normalised: String = s
        .chars()
        .filter_map(|c| {
            if c == '-' || c == '_' || c == ' ' {
                None
            } else {
                Some(c.to_ascii_lowercase())
            }
        })
        .collect();
    match normalised.as_str() {
        "needsinput" | "needs" | "input" => Some(RowState::NeedsInput),
        "working" | "busy" | "running" => Some(RowState::Working),
        "idle" => Some(RowState::Idle),
        "inactive" | "dormant" => Some(RowState::Inactive),
        "completed" | "done" => Some(RowState::Completed),
        "failed" | "errored" | "cancelled" | "canceled" => Some(RowState::Failed),
        "blocked" | "paused" => Some(RowState::Blocked),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Persistence I/O
// ---------------------------------------------------------------------------

/// Read the persisted `[dashboard].enabled` flag (defaults to `true`).
///
/// Lenient: any error or unparseable value returns `None`, which the
/// caller interprets as "use the default".
pub fn load_persisted_enabled() -> Option<bool> {
    let path = config_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let doc: toml_edit::DocumentMut = content.parse().ok()?;
    doc.get("dashboard")
        .and_then(|d| d.get("enabled"))
        .and_then(|v| v.as_bool())
}

/// Load the full persisted dashboard from `~/.grok/config.toml`.
///
/// Returns `None` only when the file is missing or completely unreadable.
/// Malformed individual fields fall back to defaults silently (edge case
/// 12).
pub fn load_persisted() -> Option<PersistedDashboard> {
    let path = config_path()?;
    load_persisted_from_path(&path)
}

/// Path-taking variant of [`load_persisted`] so the
/// on-disk round-trip can be exercised in tests against a `tempfile::TempDir`.
/// Falls back to defaults when the table or individual fields are
/// malformed.
pub fn load_persisted_from_path(path: &std::path::Path) -> Option<PersistedDashboard> {
    let content = std::fs::read_to_string(path).ok()?;
    let doc: toml_edit::DocumentMut = content.parse().ok()?;
    let table = doc.get("dashboard")?;
    let enabled = table
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let grouping = table
        .get("grouping")
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "directory" | "dir" => Some(Grouping::Directory),
            "state" => Some(Grouping::State),
            _ => None,
        })
        .unwrap_or(Grouping::State);
    let pinned: BTreeSet<PersistedRowId> = match table.get("pinned") {
        Some(item) => parse_persist_keys(item),
        None => BTreeSet::new(),
    };
    let reorder: Vec<PersistedRowId> = match table.get("reorder") {
        Some(item) => parse_persist_key_list(item),
        None => Vec::new(),
    };
    Some(PersistedDashboard {
        enabled,
        grouping,
        pinned,
        reorder,
    })
}

/// Synchronous, blocking write of the persisted dashboard config.
/// Spawned from [`crate::app::actions::Effect`] handlers via `spawn_blocking`.
///
/// Short-circuits when `read_config_document_for_edit`
/// returns `None`. That helper returns `None` only when the file is
/// non-empty AND unparseable, meaning we MUST NOT overwrite it (the
/// file may contain user data we cannot interpret). Without this
/// guard, a single dashboard pin would clobber every other table in
/// `~/.grok/config.toml` (`[ui]`, `[hints]`, `[mcpServers]`, …).
///
/// Atomic write via `<path>.dashboard.tmp.<pid>`
/// + rename, so concurrent readers never observe a half-truncated file.
pub fn write_persisted(p: &PersistedDashboard) -> std::io::Result<()> {
    let path = config_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no grok home"))?;
    write_persisted_to_path(&path, p)
}

/// Path-taking variant of [`write_persisted`] so the
/// on-disk round-trip can be exercised in tests against a
/// `tempfile::TempDir`.
pub fn write_persisted_to_path(
    path: &std::path::Path,
    p: &PersistedDashboard,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut doc = match crate::config_toml_edit::read_config_document_for_edit(path) {
        Some(d) => d,
        None => {
            // File exists but is unparseable. Refuse to overwrite —
            // doing so would erase every other table.
            tracing::warn!(
                path = %path.display(),
                "refusing to persist dashboard: config.toml is non-empty and unparseable"
            );
            return Ok(());
        }
    };
    let dash = doc.entry("dashboard").or_insert(toml_edit::table());
    let Some(t) = dash.as_table_mut() else {
        return Ok(());
    };
    t["enabled"] = toml_edit::value(p.enabled);
    t["grouping"] = toml_edit::value(match p.grouping {
        Grouping::State => "state",
        Grouping::Directory => "directory",
    });
    let mut pin_arr = toml_edit::Array::new();
    for id in &p.pinned {
        pin_arr.push(id.to_key());
    }
    t["pinned"] = toml_edit::value(pin_arr);
    let mut reorder_arr = toml_edit::Array::new();
    for id in &p.reorder {
        reorder_arr.push(id.to_key());
    }
    t["reorder"] = toml_edit::value(reorder_arr);
    // The onboarding hint was removed — drop the stale table so old
    // configs don't carry a dead `[dashboard.onboarding]` key forever.
    t.remove("onboarding");
    atomic_write(path, doc.to_string().as_bytes())
}

/// Atomic write via `<path>.dashboard.tmp.<pid>` + `rename`. Concurrent
/// readers see either the old file or the new file, never a partial
/// truncated copy that would parse as `None` and trigger the
/// catastrophic clobber on the next writer.
///
/// The bytes are explicitly `sync_all()`'d
/// before the rename, so a power loss between the syscall return and
/// the OS flush cannot leave behind a renamed-but-zero-length file.
///
/// Also fsync the parent directory after the rename
/// so the metadata change (the rename itself) is durable across power
/// loss on filesystems where the directory entry isn't implicitly
/// synced by the file's `sync_all`. Best-effort: a failure to open or
/// fsync the parent is logged at `debug` but does not propagate (the
/// rename itself succeeded; durability of the directory entry is the
/// only loss).
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let pid = std::process::id();
    let tmp = path.with_extension(format!("toml.dashboard.tmp.{pid}"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // Ensure the bytes are physically on disk before the rename.
        // Without this, the rename can complete and the file system
        // can later observe the renamed inode pointing at zeroed data.
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Parent directory fsync (defense in depth on
    // unusual filesystems / network mounts).
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        // sync_all on a directory is allowed on Unix and is a no-op
        // on platforms that don't support it. Swallow errors: the
        // rename succeeded, and the durability question is moot if
        // the OS won't fsync the directory.
        let _ = dir.sync_all();
    }
    Ok(())
}

fn config_path() -> Option<PathBuf> {
    let home = xai_grok_shell::util::grok_home::grok_home();
    Some(home.join("config.toml"))
}

/// Cap parsed entry count to keep a corrupted
/// or malicious config.toml from ballooning allocations.
const MAX_PERSISTED_ENTRIES: usize = 256;
/// Cap persist-key string length so a malformed
/// entry doesn't carry around megabytes.
const MAX_PERSIST_KEY_LEN: usize = 1024;

fn parse_persist_keys(item: &toml_edit::Item) -> BTreeSet<PersistedRowId> {
    let Some(arr) = item.as_array() else {
        tracing::warn!("dashboard.pinned must be an array; ignoring");
        return BTreeSet::new();
    };
    let mut out = BTreeSet::new();
    for v in arr.iter().take(MAX_PERSISTED_ENTRIES) {
        if let Some(s) = v.as_str()
            && s.len() <= MAX_PERSIST_KEY_LEN
            && let Some(id) = PersistedRowId::from_key(s)
        {
            out.insert(id);
        }
    }
    out
}

fn parse_persist_key_list(item: &toml_edit::Item) -> Vec<PersistedRowId> {
    let Some(arr) = item.as_array() else {
        tracing::warn!("dashboard.reorder must be an array; ignoring");
        return Vec::new();
    };
    arr.iter()
        .take(MAX_PERSISTED_ENTRIES)
        .filter_map(|v| {
            v.as_str()
                .filter(|s| s.len() <= MAX_PERSIST_KEY_LEN)
                .and_then(PersistedRowId::from_key)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Helper: relative path display
// ---------------------------------------------------------------------------

/// Compact a `Path` for display against `$HOME`, returning a `String`.
///
/// Used by the row renderer + filter substring search to keep cwd
/// matching consistent.
///
/// When `cwd == home`, `strip_prefix` returns an
/// empty `Path` and the previous `format!("~/{}", ...)` produced
/// `"~/"` (trailing slash). The empty-rest branch now collapses to
/// the bare `"~"`.
pub fn compact_cwd(cwd: &Path, home: Option<&str>) -> String {
    if let Some(h) = home
        && let Ok(rest) = cwd.strip_prefix(h)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    cwd.display().to_string()
}

#[cfg(test)]
impl DashboardState {
    /// Test-only: clone the state for tests that can't move ownership.
    /// `PromptWidget` doesn't impl Clone so we cheat by constructing
    /// a fresh state with the same selection.
    pub(crate) fn clone_for_test(&self) -> Self {
        let mut s = Self::new();
        s.selected = self.selected.clone();
        s.pinned = self.pinned.clone();
        s.reorder = self.reorder.clone();
        s.grouping = self.grouping;
        s.filter = self.filter.clone();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_error_toast` prefixes the message with the error glyph
    /// (`✗`/`x`) so the verbatim-rendering badge marks it as an error.
    #[test]
    fn set_error_toast_prefixes_error_glyph() {
        let mut state = DashboardState::new();
        state.set_error_toast("boom");
        assert_eq!(
            state.error_toast.as_deref(),
            Some(format!("{} boom", crate::glyphs::ballot_x()).as_str()),
        );
    }

    #[test]
    fn filter_parser_agent_prefix() {
        match parse_filter("a:reviewer") {
            FilterValue::Agent(s) => assert_eq!(s, "reviewer"),
            other => panic!("expected agent filter, got {other:?}"),
        }
    }

    #[test]
    fn filter_parser_empty_agent_clears() {
        assert!(matches!(parse_filter("a:"), FilterValue::None));
        assert!(matches!(parse_filter("a:   "), FilterValue::None));
    }

    #[test]
    fn filter_parser_state_known() {
        assert!(matches!(
            parse_filter("s:needs-input"),
            FilterValue::State(RowState::NeedsInput)
        ));
        assert!(matches!(
            parse_filter("s:needs_input"),
            FilterValue::State(RowState::NeedsInput)
        ));
        assert!(matches!(
            parse_filter("s:needsinput"),
            FilterValue::State(RowState::NeedsInput)
        ));
        assert!(matches!(
            parse_filter("s:working"),
            FilterValue::State(RowState::Working)
        ));
        assert!(matches!(
            parse_filter("s:idle"),
            FilterValue::State(RowState::Idle)
        ));
        assert!(matches!(
            parse_filter("s:blocked"),
            FilterValue::State(RowState::Blocked)
        ));
        assert!(matches!(
            parse_filter("s:completed"),
            FilterValue::State(RowState::Completed)
        ));
        assert!(matches!(
            parse_filter("s:failed"),
            FilterValue::State(RowState::Failed)
        ));
    }

    #[test]
    fn filter_parser_state_unknown_falls_back_to_substring() {
        match parse_filter("s:foobar") {
            FilterValue::Substring(s) => assert_eq!(s, "foobar"),
            other => panic!("expected substring fallback, got {other:?}"),
        }
    }

    /// `s:` empty now mirrors `a:` empty: returns None,
    /// not Substring("").
    #[test]
    fn filter_parser_state_empty_is_none() {
        match parse_filter("s:") {
            FilterValue::None => {}
            other => panic!("expected None, got {other:?}"),
        }
    }

    /// `#<n>` keeps the `#` in the substring needle so it
    /// never matches arbitrary digits.
    #[test]
    fn filter_parser_pr_prefix_keeps_hash() {
        match parse_filter("#42") {
            FilterValue::Substring(s) => assert_eq!(s, "#42"),
            other => panic!("expected substring fallback, got {other:?}"),
        }
    }

    #[test]
    fn filter_parser_free_text() {
        match parse_filter("auth flow") {
            FilterValue::Substring(s) => assert_eq!(s, "auth flow"),
            other => panic!("expected substring filter, got {other:?}"),
        }
    }

    #[test]
    fn persisted_row_id_round_trip_top_level() {
        let id = PersistedRowId::TopLevel {
            session_id: "sess-abc".into(),
        };
        let key = id.to_key();
        assert_eq!(key, "top:sess-abc");
        assert_eq!(PersistedRowId::from_key(&key), Some(id));
    }

    #[test]
    fn persisted_row_id_round_trip_subagent() {
        let id = PersistedRowId::Subagent {
            parent_session_id: "p-1".into(),
            child_session_id: "c-1".into(),
        };
        let key = id.to_key();
        assert_eq!(key, "sub:p-1:c-1");
        assert_eq!(PersistedRowId::from_key(&key), Some(id));
    }

    #[test]
    fn persisted_row_id_subagent_with_colon_in_child_id() {
        // The child session id portion may itself contain colons; we
        // only split on the first colon after `sub:<parent>:`.
        let raw = "sub:parent-x:abc:def:ghi";
        let parsed = PersistedRowId::from_key(raw).unwrap();
        match parsed {
            PersistedRowId::Subagent {
                parent_session_id,
                child_session_id,
            } => {
                assert_eq!(parent_session_id, "parent-x");
                assert_eq!(child_session_id, "abc:def:ghi");
            }
            _ => panic!("expected subagent variant"),
        }
    }

    #[test]
    fn persisted_row_id_invalid() {
        assert!(PersistedRowId::from_key("garbage").is_none());
        // top:<empty> rejected.
        assert!(PersistedRowId::from_key("top:").is_none());
        // sub: with no parent rejected.
        assert!(PersistedRowId::from_key("sub::child").is_none());
        // sub:<parent> with no child rejected.
        assert!(PersistedRowId::from_key("sub:parent:").is_none());
        // sub: with no colon between parent/child rejected.
        assert!(PersistedRowId::from_key("sub:foo").is_none());
    }

    #[test]
    fn group_priority_ordering() {
        assert!(RowState::NeedsInput.group_priority() > RowState::Working.group_priority());
        assert!(RowState::Working.group_priority() > RowState::Idle.group_priority());
        assert!(RowState::Idle.group_priority() > RowState::Completed.group_priority());
        assert!(RowState::Completed.group_priority() > RowState::Failed.group_priority());
    }

    #[test]
    fn compact_cwd_strips_home() {
        let p = Path::new("/Users/alice/projects/grok");
        assert_eq!(compact_cwd(p, Some("/Users/alice")), "~/projects/grok");
    }

    #[test]
    fn compact_cwd_no_home_match() {
        let p = Path::new("/var/tmp/x");
        assert_eq!(compact_cwd(p, Some("/Users/alice")), "/var/tmp/x");
    }

    /// When `cwd == home`, return bare `"~"` (no
    /// trailing slash). This test pins the behaviour.
    #[test]
    fn compact_cwd_path_equals_home() {
        let p = Path::new("/Users/alice");
        assert_eq!(compact_cwd(p, Some("/Users/alice")), "~");
    }

    // Vacuous `rename_draft_caps_at_100_chars` deleted —
    // covered by the substantive `rename_at_cap_drops_extra_char` and
    // `rename_under_cap_appends` tests below, which assert exact
    // character at the cap boundary.

    /// Edge case 20: stale row ids in `pinned` / `reorder` are dropped
    /// on `gc_stale_refs`.
    #[test]
    fn gc_drops_stale_ids() {
        let mut state = DashboardState::new();
        state.pinned.insert(DashboardRowId::TopLevel(AgentId(7)));
        state.reorder.push(DashboardRowId::TopLevel(AgentId(7)));
        // Alive predicate says agent 7 no longer exists.
        state.gc_stale_refs(&|_| false);
        assert!(state.pinned.is_empty());
        assert!(state.reorder.is_empty());
    }

    /// Lenient parsing: malformed `pinned` (string instead of array)
    /// is silently dropped. Edge case 12.
    #[test]
    fn parse_persist_keys_rejects_non_array() {
        let s = r#"pinned = "not-an-array""#;
        let doc: toml_edit::DocumentMut = s.parse().unwrap();
        let item = doc.get("pinned").unwrap();
        let out = parse_persist_keys(item);
        assert!(out.is_empty());
    }

    /// Lenient parsing: malformed `reorder` (table instead of array)
    /// is silently dropped.
    #[test]
    fn parse_persist_key_list_rejects_non_array() {
        let s = "[reorder]\nx = 1";
        let doc: toml_edit::DocumentMut = s.parse().unwrap();
        let item = doc.get("reorder").unwrap();
        let out = parse_persist_key_list(item);
        assert!(out.is_empty());
    }

    /// Lenient parsing: array entries that aren't strings get dropped.
    #[test]
    fn parse_persist_keys_skips_non_string_entries() {
        let s = "pinned = [1, 2, \"top:sess-7\", false]";
        let doc: toml_edit::DocumentMut = s.parse().unwrap();
        let item = doc.get("pinned").unwrap();
        let out = parse_persist_keys(item);
        assert_eq!(out.len(), 1);
        assert!(out.contains(&PersistedRowId::TopLevel {
            session_id: "sess-7".into(),
        }));
    }

    /// Array entries past `MAX_PERSISTED_ENTRIES` are dropped.
    #[test]
    fn parse_persist_keys_caps_entry_count() {
        let many: Vec<String> = (0..(MAX_PERSISTED_ENTRIES * 2))
            .map(|i| format!("\"top:s-{i}\""))
            .collect();
        let s = format!("pinned = [{}]", many.join(","));
        let doc: toml_edit::DocumentMut = s.parse().unwrap();
        let item = doc.get("pinned").unwrap();
        let out = parse_persist_keys(item);
        assert!(out.len() <= MAX_PERSISTED_ENTRIES);
    }

    /// Each persisted key value is capped in length.
    #[test]
    fn parse_persist_keys_rejects_overlong_strings() {
        let huge = format!("\"top:{}\"", "a".repeat(MAX_PERSIST_KEY_LEN + 10));
        let s = format!("pinned = [{huge}]");
        let doc: toml_edit::DocumentMut = s.parse().unwrap();
        let item = doc.get("pinned").unwrap();
        let out = parse_persist_keys(item);
        assert!(out.is_empty());
    }

    /// Pin/unpin toggling works against the in-memory set.
    #[test]
    fn pin_unpin_toggle() {
        let mut state = DashboardState::new();
        let id = DashboardRowId::TopLevel(AgentId(1));
        state.selected = Some(id.clone());
        assert!(state.pinned.is_empty());
        state.toggle_pin_selected();
        assert!(state.pinned.contains(&id));
        state.toggle_pin_selected();
        assert!(state.pinned.is_empty());
    }

    /// Grouping toggle round-trips State → Directory → State.
    #[test]
    fn grouping_toggles() {
        let mut state = DashboardState::new();
        assert_eq!(state.grouping, Grouping::State);
        state.toggle_grouping();
        assert_eq!(state.grouping, Grouping::Directory);
        state.toggle_grouping();
        assert_eq!(state.grouping, Grouping::State);
    }

    /// `Ctrl+G` (the rebound grouping chord) emits `DashboardToggleGrouping`,
    /// and `Ctrl+S` no longer toggles grouping — it's now "send + open".
    #[test]
    fn ctrl_g_toggles_grouping_ctrl_s_does_not() {
        use crate::app::actions::Action;
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = DashboardState::new();
        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(
            matches!(
                state.handle_key(&ctrl_g, &reg),
                InputOutcome::Action(Action::DashboardToggleGrouping)
            ),
            "Ctrl+G must emit DashboardToggleGrouping",
        );

        // Ctrl+S on the empty `[+ New Agent]` button is "send + open"
        // (create + detail), NOT a grouping toggle.
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(
            matches!(
                state.handle_key(&ctrl_s, &reg),
                InputOutcome::Action(Action::DashboardCreateNewAgentWithDetail)
            ),
            "Ctrl+S must be send+open, not grouping",
        );
    }

    /// Edge case 4: selection survives a row refresh as long as the
    /// underlying `DashboardRowId` is still present.
    #[test]
    fn reanchor_selection_keeps_existing_id() {
        let mut state = DashboardState::new();
        let id1 = DashboardRowId::TopLevel(AgentId(1));
        state.selected = Some(id1.clone());
        let rows = vec![super::super::row::DashboardRow {
            id: id1.clone(),
            label: "r1".to_string(),
            subtitle: None,
            state: RowState::Idle,
            activity: None,
            secondary_line: None,
            cwd_display: String::new(),
            cwd: std::path::PathBuf::from("/"),
            last_change_at: std::time::SystemTime::now(),
            pinned: false,
            is_active: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        }];
        state.reanchor_selection(&rows);
        assert_eq!(state.selected, Some(id1));
    }

    /// When the previous selection has disappeared,
    /// `reanchor_selection` now drops the cursor to `None`
    /// instead of auto-promoting to the first row. The "no row
    /// selected → dispatch creates a new session" contract
    /// depends on `None` being a stable steady state — a stale
    /// agent vanishing must not silently re-arm the reply path
    /// against whatever happens to be at the top.
    #[test]
    fn reanchor_selection_drops_to_none_when_previous_disappeared() {
        let mut state = DashboardState::new();
        state.selected = Some(DashboardRowId::TopLevel(AgentId(99)));
        let id1 = DashboardRowId::TopLevel(AgentId(1));
        let rows = vec![super::super::row::DashboardRow {
            id: id1.clone(),
            label: "r1".to_string(),
            subtitle: None,
            state: RowState::Idle,
            activity: None,
            secondary_line: None,
            cwd_display: String::new(),
            cwd: std::path::PathBuf::from("/"),
            last_change_at: std::time::SystemTime::now(),
            pinned: false,
            is_active: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        }];
        state.reanchor_selection(&rows);
        assert_eq!(
            state.selected, None,
            "stale selection must drop to None so the new-session path stays reachable",
        );
    }

    /// Real on-disk round-trip via the new
    /// `write_persisted_to_path` / `load_persisted_from_path` helpers
    /// plus `tempfile::TempDir`.
    #[test]
    fn persisted_on_disk_round_trip() {
        use std::collections::BTreeSet;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut pinned: BTreeSet<PersistedRowId> = BTreeSet::new();
        pinned.insert(PersistedRowId::TopLevel {
            session_id: "sess-3".into(),
        });
        let reorder = vec![PersistedRowId::Subagent {
            parent_session_id: "sess-3".into(),
            child_session_id: "child-1".into(),
        }];
        let p = PersistedDashboard {
            enabled: false,
            grouping: Grouping::Directory,
            pinned: pinned.clone(),
            reorder: reorder.clone(),
        };
        write_persisted_to_path(&path, &p).unwrap();
        let loaded = load_persisted_from_path(&path).expect("must load back");
        assert!(!loaded.enabled);
        assert_eq!(loaded.grouping, Grouping::Directory);
        assert_eq!(loaded.pinned, pinned);
        assert_eq!(loaded.reorder, reorder);
    }

    /// The onboarding hint was removed — a stale `[dashboard.onboarding]`
    /// table left by an older version is dropped on the next write.
    #[test]
    fn persisted_write_drops_stale_onboarding_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[dashboard]\nenabled = true\n\n[dashboard.onboarding]\ndismissed = true\n",
        )
        .unwrap();
        write_persisted_to_path(&path, &PersistedDashboard::defaults()).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("onboarding"),
            "stale onboarding table must be removed, got: {after:?}"
        );
    }

    /// Pre-populated `[hints]` table survives the dashboard write
    /// (the guarantee — we never clobber unrelated tables).
    #[test]
    fn persisted_write_preserves_other_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[hints]\nproject_picker_disabled = true\n\n[ui]\ncompact_mode = false\n",
        )
        .unwrap();
        let p = PersistedDashboard {
            enabled: true,
            grouping: Grouping::State,
            pinned: BTreeSet::new(),
            reorder: Vec::new(),
        };
        write_persisted_to_path(&path, &p).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("[hints]"));
        assert!(after.contains("project_picker_disabled = true"));
        assert!(after.contains("[ui]"));
        assert!(after.contains("compact_mode = false"));
        assert!(after.contains("[dashboard]"));
    }

    /// Garbage `enabled` value falls back to defaults at load.
    #[test]
    fn persisted_load_garbage_enabled_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[dashboard]\nenabled = \"garbage\"\n").unwrap();
        let loaded = load_persisted_from_path(&path).expect("section present");
        // Garbage → default `true`.
        assert!(loaded.enabled);
        assert_eq!(loaded.grouping, Grouping::State);
        assert!(loaded.pinned.is_empty());
    }

    /// Write refuses to clobber a non-empty
    /// unparseable file (round-trip with file containing garbage).
    #[test]
    fn persisted_write_refuses_unparseable_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "this is :: not :: valid :: toml :: at all").unwrap();
        let p = PersistedDashboard {
            enabled: true,
            grouping: Grouping::Directory,
            pinned: BTreeSet::new(),
            reorder: Vec::new(),
        };
        // write_persisted_to_path returns Ok(()) but does NOT overwrite.
        write_persisted_to_path(&path, &p).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.starts_with("this is"),
            "file must be preserved verbatim; got: {after:?}"
        );
    }

    /// gc_stale_refs preserves the order of remaining
    /// reorder entries.
    #[test]
    fn gc_preserves_order_of_remaining_reorder_entries() {
        let mut state = DashboardState::new();
        let alive1 = DashboardRowId::TopLevel(AgentId(1));
        let alive2 = DashboardRowId::TopLevel(AgentId(2));
        let alive3 = DashboardRowId::TopLevel(AgentId(3));
        let stale99 = DashboardRowId::TopLevel(AgentId(99));
        let stale100 = DashboardRowId::TopLevel(AgentId(100));
        state.reorder = vec![
            alive1.clone(),
            stale99.clone(),
            alive2.clone(),
            stale100.clone(),
            alive3.clone(),
        ];
        let alive_set: std::collections::HashSet<_> =
            [alive1.clone(), alive2.clone(), alive3.clone()]
                .into_iter()
                .collect();
        state.gc_stale_refs(&|id| alive_set.contains(id));
        assert_eq!(state.reorder, vec![alive1, alive2, alive3]);
    }

    /// Two different pinned rows coexist.
    #[test]
    fn pin_two_different_rows_coexist() {
        let mut state = DashboardState::new();
        let id_a = DashboardRowId::TopLevel(AgentId(1));
        let id_b = DashboardRowId::TopLevel(AgentId(2));
        state.selected = Some(id_a.clone());
        state.toggle_pin_selected();
        state.selected = Some(id_b.clone());
        state.toggle_pin_selected();
        assert!(state.pinned.contains(&id_a));
        assert!(state.pinned.contains(&id_b));
        assert_eq!(state.pinned.len(), 2);
    }

    /// Grouping toggle idempotency over multiple cycles.
    #[test]
    fn grouping_toggles_three_times() {
        let mut state = DashboardState::new();
        let start = state.grouping;
        state.toggle_grouping();
        state.toggle_grouping();
        state.toggle_grouping();
        assert_ne!(state.grouping, start);
        state.toggle_grouping();
        assert_eq!(state.grouping, start);
    }

    /// parse_row_state_token covers all documented synonyms.
    #[test]
    fn parse_row_state_token_all_synonyms() {
        for (s, expected) in [
            ("needs-input", RowState::NeedsInput),
            ("needs_input", RowState::NeedsInput),
            ("needsinput", RowState::NeedsInput),
            ("needs", RowState::NeedsInput),
            ("input", RowState::NeedsInput),
            ("NEEDS-INPUT", RowState::NeedsInput),
            ("working", RowState::Working),
            ("busy", RowState::Working),
            ("running", RowState::Working),
            ("idle", RowState::Idle),
            ("IDLE", RowState::Idle),
            ("inactive", RowState::Inactive),
            ("dormant", RowState::Inactive),
            ("completed", RowState::Completed),
            ("done", RowState::Completed),
            ("failed", RowState::Failed),
            ("errored", RowState::Failed),
            ("cancelled", RowState::Failed),
            ("canceled", RowState::Failed),
            ("blocked", RowState::Blocked),
            ("paused", RowState::Blocked),
        ] {
            assert_eq!(parse_row_state_token(s), Some(expected), "input={s}");
        }
        // Empty and whitespace return None.
        assert_eq!(parse_row_state_token(""), None);
        assert_eq!(parse_row_state_token("   "), None);
        assert_eq!(parse_row_state_token("nonsense"), None);
    }

    /// Rename cap is honored exactly.
    #[test]
    fn rename_at_cap_drops_extra_char() {
        let mut draft = RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "a".repeat(100));
        let key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE);
        let outcome = handle_rename_key(&mut draft, &key);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(draft.text().chars().count(), 100);
        assert!(
            draft.text().ends_with('a'),
            "char at cap should NOT be replaced: got {:?}",
            draft.text()
        );
    }

    /// under-cap appends correctly.
    #[test]
    fn rename_under_cap_appends() {
        let mut draft = RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "a".repeat(99));
        let key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE);
        let outcome = handle_rename_key(&mut draft, &key);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(draft.text().chars().count(), 100);
        assert!(draft.text().ends_with('b'));
    }

    /// Ctrl+letter in rename mode rejected (does not type
    /// the bare letter into the draft); Ctrl+C cancels.
    #[test]
    fn rename_rejects_ctrl_chars() {
        let mut draft = RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "hello");
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let outcome = handle_rename_key(&mut draft, &ctrl_r);
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert_eq!(draft.text(), "hello", "draft must not gain 'r'");
        // Ctrl+C → cancel.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let outcome = handle_rename_key(&mut draft, &ctrl_c);
        assert!(matches!(
            outcome,
            InputOutcome::Action(crate::app::actions::Action::DashboardCancelRename)
        ));
    }

    #[test]
    fn rename_word_motion_is_canonical_and_cursor_only() {
        for key in [
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
        ] {
            let mut draft = RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "hello-world");
            let outcome = handle_rename_key(&mut draft, &key);
            assert!(matches!(outcome, InputOutcome::Changed));
            assert_eq!(draft.text(), "hello-world");
            assert_eq!(draft.cursor_byte(), "hello-".len());
        }

        for key in [
            KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
        ] {
            let mut draft = RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "hello-world");
            let _ = handle_rename_key(
                &mut draft,
                &KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
            );
            let outcome = handle_rename_key(&mut draft, &key);
            assert!(matches!(outcome, InputOutcome::Changed));
            assert_eq!(draft.cursor_byte(), "hello".len());
        }

        let mut draft = RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "hello-world");
        let outcome = handle_rename_key(
            &mut draft,
            &KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(draft.text(), "hello-");
    }

    #[test]
    fn rename_grapheme_delete_and_middle_insert() {
        let grapheme = "👩🏽\u{200d}💻";
        let mut draft = RenameDraft::new(
            DashboardRowId::TopLevel(AgentId(0)),
            format!("a{grapheme}b"),
        );
        let _ = handle_rename_key(
            &mut draft,
            &KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
        );
        let _ = handle_rename_key(
            &mut draft,
            &KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        let outcome = handle_rename_key(
            &mut draft,
            &KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(draft.text(), "ab");

        let outcome = handle_rename_key(
            &mut draft,
            &KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE),
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(draft.text(), "aXb");
    }

    #[test]
    fn rename_policy_and_paste_preserve_scalar_cap() {
        let mut draft = RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "a".repeat(99));
        let outcome = handle_rename_key(
            &mut draft,
            &KeyEvent::new(KeyCode::Char('\u{202e}'), KeyModifiers::NONE),
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(draft.text().chars().count(), 99);

        let outcome = handle_rename_paste(&mut draft, "中\r\n文");
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(draft.text().chars().count(), 100);
        assert!(draft.text().ends_with('中'));
    }

    #[test]
    fn modified_enter_does_not_commit_rename() {
        let mut draft = RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "name");
        for modifiers in [KeyModifiers::ALT, KeyModifiers::SHIFT] {
            let outcome = handle_rename_key(&mut draft, &KeyEvent::new(KeyCode::Enter, modifiers));
            assert!(!matches!(
                outcome,
                InputOutcome::Action(Action::DashboardCommitRename)
            ));
        }
    }

    #[test]
    fn rename_paste_preserves_emoji_zwj_sequences() {
        let mut draft = RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "");
        let outcome = handle_rename_paste(&mut draft, "👩‍💻");
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(draft.text(), "👩‍💻");
    }

    #[test]
    fn rename_mode_routes_bracketed_paste_only_to_rename_editor() {
        let mut state = DashboardState::new();
        state.dispatch.set_text("hidden dispatch");
        state.rename = Some(RenameDraft::new(DashboardRowId::TopLevel(AgentId(0)), "ab"));
        let registry = crate::actions::ActionRegistry::defaults();
        let _ = state.handle_input(
            &Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            &registry,
        );
        let outcome = state.handle_input(&Event::Paste("中\r\n".to_owned()), &registry);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.rename.as_ref().map(RenameDraft::text), Some("a中b"));
        assert_eq!(state.dispatch.text(), "hidden dispatch");
    }

    /// Esc-cancelling the worktree-label dialog must restore the stashed
    /// prompt (from the prompt-send path) to the dispatch input instead of
    /// silently discarding the user's typed text. Mirrors the restore in
    /// `dispatch_dashboard_confirm_worktree`'s not-a-repo error path.
    #[test]
    fn worktree_dialog_cancel_restores_stashed_prompt_state() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        // Simulate the prompt-send path: the prompt is stashed, the dialog
        // is opened, and the dispatch input is cleared.
        state.dispatch.set_text("fix the bug ");
        let draft_end = state.dispatch.text().len();
        state.dispatch.set_cursor(draft_end);
        state.dispatch.insert_image(peek_test_image()).unwrap();
        state.pending_worktree_prompt = Some(state.dispatch.stash());
        state.worktree_dialog = Some(crate::app::app_view::NewWorktreeDialogState::new());
        state.dispatch.set_text("");

        let outcome = state.handle_input(
            &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &reg,
        );

        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(state.worktree_dialog.is_none(), "Esc closes the dialog");
        assert!(
            state.pending_worktree_prompt.is_none(),
            "the stash must be consumed",
        );
        assert_eq!(
            state.dispatch.text(),
            "fix the bug [Image #1] ",
            "the stashed prompt must be restored to the dispatch input",
        );
        assert_eq!(state.dispatch.drain_images().len(), 1);
    }

    /// Cancelling the dialog when it was opened from the `[+ New Agent]`
    /// button (no stashed prompt) leaves the dispatch input untouched.
    #[test]
    fn worktree_dialog_cancel_without_stash_leaves_input_empty() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.worktree_dialog = Some(crate::app::app_view::NewWorktreeDialogState::new());
        state.dispatch.set_text("");

        let _ = state.handle_input(
            &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &reg,
        );

        assert!(state.worktree_dialog.is_none());
        assert!(state.pending_worktree_prompt.is_none());
        assert_eq!(
            state.dispatch.text(),
            "",
            "no stash → dispatch input stays empty",
        );
    }

    /// `Ctrl+W` resolves to the worktree-toggle action (which is what puts it
    /// in the dashboard cheatsheet and lets the dispatcher git-gate it). The
    /// actual flag flip + non-git guard live in
    /// `dispatch_dashboard_toggle_worktree` and are covered there.
    #[test]
    fn ctrl_w_emits_toggle_worktree_action() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();

        let outcome = state.handle_input(
            &Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            &reg,
        );
        assert!(
            matches!(
                outcome,
                InputOutcome::Action(Action::DashboardToggleWorktree)
            ),
            "Ctrl+W must resolve to the DashboardToggleWorktree action",
        );
    }

    // ---------------------------------------------------------------
    // handle_key tests (Esc cascade, Enter routing).
    // ---------------------------------------------------------------

    fn make_state_with_selection() -> DashboardState {
        let mut s = DashboardState::new();
        s.selected = Some(DashboardRowId::TopLevel(AgentId(0)));
        s
    }

    fn peek_fields_for_test(response_type: &str) -> super::super::peek::PeekFields {
        super::super::peek::PeekFields {
            label: "x".to_string(),
            time_ago: String::new(),
            response_type: response_type.to_string(),
            last_user_message: None,
            question: None,
            options: Vec::new(),
            request_id: None,
            reject_option: None,
        }
    }

    /// edge case 13: Esc closes peek first.
    #[test]
    fn esc_closes_peek_first() {
        let mut state = make_state_with_selection();
        state.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            peek_fields_for_test("Idle"),
        ));
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(state.peek.is_none());
    }

    fn state_with_open_peek() -> DashboardState {
        // PeekPanelState::new seeds focus from load_vim_mode(); pin off so
        // older tests that assume a focused reply don't depend on config /
        // process cache. Vim tests set true and restore themselves.
        crate::appearance::cache::set_vim_mode(false);
        let mut s = make_state_with_selection();
        s.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            peek_fields_for_test("Idle"),
        ));
        s
    }

    /// Regression: with the peek open but the reply UNFOCUSED (Tab → row
    /// nav), generic editing chords must NOT leak into the hidden new-session
    /// dispatch draft behind the panel. Backspace / Delete are consumed
    /// (`Unchanged`) instead of falling through to the hidden dispatch widget.
    /// (Ctrl+W is NOT tested here — it's a registry-bound dashboard chord, the
    /// worktree toggle, so like Ctrl+X it intentionally falls through to fire
    /// its action with the peek open.)
    #[test]
    fn peek_unfocused_editing_chords_do_not_leak_to_dispatch() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        // Hidden new-session draft, caret at END (where Backspace bites;
        // set_text alone parks it at 0).
        state.dispatch.set_text("hidden draft");
        state.dispatch.set_cursor(state.dispatch.text().len());
        // Tab → unfocus the reply (it becomes a row-nav surface).
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &reg);
        assert!(!state.peek.as_ref().unwrap().focused, "Tab must unfocus");

        for key in [
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
        ] {
            let outcome = state.handle_key(&key, &reg);
            assert!(
                matches!(outcome, InputOutcome::Unchanged),
                "{key:?} must be consumed (Unchanged) with the peek open, got {outcome:?}",
            );
        }
        assert_eq!(
            state.dispatch.text(),
            "hidden draft",
            "editing chords must NOT leak into the hidden dispatch draft",
        );
        assert!(
            !state.peek.as_ref().unwrap().focused,
            "consumed editing chords must not grab focus",
        );
    }

    /// While the peek panel is open, bare printable keys type into the
    /// `❯ reply` input (not the hidden dispatch box).
    #[test]
    fn peek_typing_edits_reply_buffer() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        for c in ['h', 'i'] {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            let _ = state.handle_key(&key, &reg);
        }
        assert_eq!(state.peek_reply.text(), "hi");
        // The dispatch (new-session) buffer is untouched.
        assert!(state.dispatch.text().is_empty());
    }

    /// Enter with a typed reply emits `DashboardPeekReply` (no attach);
    /// Ctrl+S ("send + open") sets `attach=true`.
    #[test]
    fn peek_enter_with_text_emits_reply() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        state.peek_reply.set_text("ship it");
        let reg = crate::actions::ActionRegistry::defaults();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_key(&enter, &reg) {
            InputOutcome::Action(Action::DashboardPeekReply { text, attach, row }) => {
                assert_eq!(text, "ship it");
                assert!(!attach);
                assert_eq!(row, DashboardRowId::TopLevel(AgentId(0)));
            }
            other => panic!("expected DashboardPeekReply, got {other:?}"),
        }

        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        match state.handle_key(&ctrl_s, &reg) {
            InputOutcome::Action(Action::DashboardPeekReply { attach, text, .. }) => {
                assert!(attach, "Ctrl+S must set attach=true (send + open)");
                assert_eq!(text, "ship it");
            }
            other => panic!("expected DashboardPeekReply, got {other:?}"),
        }
    }

    fn peek_test_image() -> crate::prompt_images::PastedImage {
        crate::prompt_images::PastedImage {
            element_id: xai_ratatui_textarea::ElementId::from_raw(0),
            display_number: 0,
            mime_type: "image/png".into(),
            dimensions: Some((10, 10)),
            byte_len: 16,
            encoded_bytes: Some(vec![0u8; 16].into()),
            source_path: Some(std::path::PathBuf::from(
                "/Users/somebody/very/long/path/screenshot.png",
            )),
            staged_temp_path: None,
            session_image_path: None,
            preview: crate::prompt_images::PromptImagePreview::default(),
        }
    }

    fn write_test_png(dir: &std::path::Path) -> std::path::PathBuf {
        let img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
            image::ImageBuffer::from_pixel(16, 16, image::Rgba([255, 0, 0, 255]));
        let path = dir.join("shot.png");
        img.save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        path
    }

    fn test_png_bytes() -> Vec<u8> {
        let img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
            image::ImageBuffer::from_pixel(16, 16, image::Rgba([0, 128, 255, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// The Ctrl/Cmd+V chord path attaches a clipboard image to the peek
    /// reply (image wins over the caption) without leaking into dispatch. Drives
    /// the real deferred entry point + completion.
    #[test]
    fn peek_paste_key_clipboard_image_wins_over_text() {
        let mut state = state_with_open_peek();
        cmd_v_image(&mut state, Some("ignored text"));
        assert_eq!(state.peek_reply.images.len(), 1);
        let text = state.peek_reply.text();
        assert!(text.contains("[Image #1]"), "got {text:?}");
        assert!(
            !text.contains("ignored text"),
            "text won over image: {text:?}"
        );
        assert!(state.dispatch.images.is_empty() && state.dispatch.text().is_empty());
    }

    /// In question mode the chord path must not defer/attach a clipboard
    /// image — the reply is text-only on the wire.
    #[test]
    fn peek_paste_key_question_mode_blocks_clipboard_image() {
        let mut state = state_with_open_peek();
        if let Some(p) = state.peek.as_mut() {
            p.focused = true;
            p.question = Some("Allow?".into());
            p.options = vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())];
            p.reject_option = Some(1);
            p.selected_option = Some(1);
        }
        let reg = crate::actions::ActionRegistry::defaults();
        // Even with a raster on the pasteboard, question mode must not probe.
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::with_raster(None),
        );
        let _ = state.handle_input(&ctrl_v_event(), &reg);
        let deferred = deferred_probe_target(&state).is_some();
        crate::clipboard::clear_clipboard_probe_hook();
        assert!(!deferred, "question mode must not defer an image probe");
        assert!(state.peek_reply.images.is_empty());
        assert!(!state.peek_reply.text().contains("[Image #"));
    }

    /// A whitespace-only chord paste inserts no text into the reply. Trimmed-empty
    /// routes to the FileUrlsThenImage probe (to catch an image-only pasteboard),
    /// so it defers rather than inserting spaces.
    #[test]
    fn peek_paste_key_whitespace_only_is_unchanged() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::no_raster(Some("   ")),
        );
        let _ = state.handle_input(&ctrl_v_event(), &reg);
        crate::clipboard::clear_clipboard_probe_hook();
        assert!(state.peek_reply.text().is_empty());
    }

    /// An empty-clipboard chord defers a probe (to catch a Finder file-url /
    /// image-only pasteboard) without inserting text.
    #[test]
    fn peek_paste_key_empty_clipboard_defers_probe() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::no_raster(None),
        );
        let _ = state.handle_input(&ctrl_v_event(), &reg);
        let target = deferred_probe_target(&state);
        crate::clipboard::clear_clipboard_probe_hook();
        assert!(
            matches!(
                target,
                Some(crate::app::actions::ClipboardPasteTarget::DashboardPeek { .. })
            ),
            "empty pbpaste still probes for a file-url / image via the peek target"
        );
        assert!(state.peek_reply.text().is_empty());
    }

    #[test]
    fn dashboard_failed_text_read_is_carried_into_deferred_context() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(crate::clipboard::ClipboardProbeHook {
            text_read_failed: true,
            ..crate::clipboard::ClipboardProbeHook::snapshot_unavailable()
        });

        let _ = state.handle_input(&ctrl_v_event(), &reg);
        let ctx = deferred_probe_ctx(&state).expect("failed text read must probe attachments");
        crate::clipboard::clear_clipboard_probe_hook();

        assert!(ctx.source.text_read_failed());
    }

    /// A real path-image paste routed through `handle_input`
    /// attaches an `[Image #N]` chip on the peek reply (not the hidden
    /// dispatch input), with the source path stripped for a clean chip.
    #[test]
    fn peek_path_paste_routes_image_to_reply_not_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let png = write_test_png(dir.path());
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        // Trailing newline skips the clipboard probe so the test is hermetic.
        let paste = format!("{}\n", png.display());
        let outcome = state.handle_input(&Event::Paste(paste), &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        let text = state.peek_reply.text();
        assert!(text.contains("[Image #1]"), "expected chip, got {text:?}");
        assert!(
            !text.contains("shot.png"),
            "chip must not embed the source path, got {text:?}"
        );
        assert_eq!(state.peek_reply.images.len(), 1);
        assert!(
            state.dispatch.text().is_empty() && state.dispatch.images.is_empty(),
            "image must not leak into the hidden dispatch input"
        );
    }

    /// In question mode the reply is text-only on the wire — a
    /// path-image paste must NOT become an image chip.
    #[test]
    fn peek_question_mode_path_paste_stays_text_only() {
        let dir = tempfile::tempdir().unwrap();
        let png = write_test_png(dir.path());
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        if let Some(p) = state.peek.as_mut() {
            p.focused = true;
            p.question = Some("Allow?".into());
            p.options = vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())];
            p.reject_option = Some(1);
            p.selected_option = Some(1);
        }
        let paste = format!("{}\n", png.display());
        let _ = state.handle_input(&Event::Paste(paste), &reg);
        assert!(state.peek_reply.images.is_empty());
        assert!(!state.peek_reply.text().contains("[Image #"));
    }

    /// `clear_peek_reply` drops image state with the draft text.
    #[test]
    fn clear_peek_reply_clears_images() {
        let mut state = state_with_open_peek();
        let _ = state.attach_peek_pasted_image(peek_test_image());
        assert!(!state.peek_reply.images.is_empty());
        state.clear_peek_reply();
        assert!(state.peek_reply.text().is_empty());
        assert!(state.peek_reply.images.is_empty());
    }

    /// Enter with an image chip on the reply emits `DashboardPeekReply`
    /// carrying the chip placeholder text (images drain at dispatch time).
    #[test]
    fn peek_enter_with_image_emits_reply() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        let _ = state.attach_peek_pasted_image(peek_test_image());
        let reg = crate::actions::ActionRegistry::defaults();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_key(&enter, &reg) {
            InputOutcome::Action(Action::DashboardPeekReply { text, attach, .. }) => {
                assert!(text.contains("[Image #1]"), "got {text:?}");
                assert!(!attach);
            }
            other => panic!("expected DashboardPeekReply, got {other:?}"),
        }
        // Action does not drain images — still on the widget until dispatch.
        assert_eq!(state.peek_reply.images.len(), 1);
    }

    /// Question/permission feedback is text-only — an image chip
    /// left on the reply must not leak its `[Image #N]` token into the
    /// submitted feedback text.
    #[test]
    fn peek_question_feedback_strips_image_placeholder() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        state.peek_reply.set_text("no thanks");
        state.peek_reply.set_cursor(state.peek_reply.text().len());
        let _ = state.attach_peek_pasted_image(peek_test_image());
        assert!(state.peek_reply.text().contains("[Image #1]"));
        if let Some(p) = state.peek.as_mut() {
            p.focused = true;
            p.question = Some("Allow?".into());
            p.options = vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())];
            p.reject_option = Some(1);
            p.selected_option = Some(1);
            p.request_id = Some(7);
        }
        let reg = crate::actions::ActionRegistry::defaults();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_key(&enter, &reg) {
            InputOutcome::Action(Action::DashboardPermissionFollowup { text, .. }) => {
                assert!(!text.contains("[Image #"), "image token leaked: {text:?}");
                assert!(
                    text.contains("no thanks"),
                    "feedback text dropped: {text:?}"
                );
            }
            other => panic!("expected DashboardPermissionFollowup, got {other:?}"),
        }
    }

    /// The Ask "Other" fallthrough (image-only draft, no typed text)
    /// must also strip the `[Image #N]` token from the freeform answer.
    #[test]
    fn peek_ask_other_image_only_strips_placeholder() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        let _ = state.attach_peek_pasted_image(peek_test_image());
        assert!(state.peek_reply.text().contains("[Image #1]"));
        if let Some(p) = state.peek.as_mut() {
            p.focused = true;
            p.question = Some("Which?".into());
            p.options = vec![("a".into(), "A".into()), ("other".into(), "Other".into())];
            p.reject_option = Some(1);
            p.selected_option = Some(1);
            p.request_id = None; // Ask tool (not a permission)
        }
        let reg = crate::actions::ActionRegistry::defaults();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_key(&enter, &reg) {
            InputOutcome::Action(Action::DashboardQuestionAnswer { freeform, .. }) => {
                assert!(
                    !freeform.contains("[Image #"),
                    "image token leaked: {freeform:?}"
                );
            }
            other => panic!("expected DashboardQuestionAnswer, got {other:?}"),
        }
    }

    /// Shift+Enter / Alt+Enter insert a newline into the focused peek
    /// reply (multiline compose) instead of sending — the reply text
    /// grows and no action is emitted.
    #[test]
    fn peek_shift_enter_inserts_newline() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        state.peek_reply.set_text("line one");
        // Caret at end so the newline appends.
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        let outcome = state.handle_key(&shift_enter, &reg);
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Shift+Enter must edit the reply, got {outcome:?}",
        );
        assert!(
            state.peek_reply.text().contains('\n'),
            "Shift+Enter must insert a newline, got {:?}",
            state.peek_reply.text(),
        );
        // Alt+Enter does the same.
        let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        let _ = state.handle_key(&alt_enter, &reg);
        assert_eq!(state.peek_reply.text().matches('\n').count(), 2);
    }

    /// With multiline_mode on, peek bare Enter inserts a newline; Shift+Enter sends.
    #[test]
    fn peek_multiline_mode_swaps_enter() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        state.multiline_mode = true;
        if let Some(p) = state.peek.as_mut() {
            p.focused = true;
        }
        state.peek_reply.set_text("line one");
        let reg = crate::actions::ActionRegistry::defaults();

        let bare = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            matches!(bare, InputOutcome::Changed),
            "bare Enter in multiline peek must insert newline, got {bare:?}"
        );
        assert!(
            state.peek_reply.text().contains('\n'),
            "got {:?}",
            state.peek_reply.text()
        );

        let shift = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &reg);
        match shift {
            InputOutcome::Action(Action::DashboardPeekReply {
                text,
                attach: false,
                ..
            }) => {
                assert!(text.contains("line one"), "peek reply text: {text:?}");
            }
            other => panic!("Shift+Enter in multiline peek must send, got {other:?}"),
        }
    }

    /// Enter with an empty reply opens the peeked agent rather than
    /// sending an empty prompt.
    #[test]
    fn peek_enter_empty_opens_agent() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_key(&enter, &reg) {
            InputOutcome::Action(Action::DashboardAttach(row)) => {
                assert_eq!(row, DashboardRowId::TopLevel(AgentId(0)));
            }
            other => panic!("expected DashboardAttach, got {other:?}"),
        }
    }

    /// Right arrow on a selected agent (peek open) opens its detail view
    /// — the mirror of the agent overlay's Left-arrow back-out.
    #[test]
    fn peek_right_arrow_opens_agent() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        match state.handle_key(&right, &reg) {
            InputOutcome::Action(Action::DashboardAttach(row)) => {
                assert_eq!(row, DashboardRowId::TopLevel(AgentId(0)));
            }
            other => panic!("expected DashboardAttach, got {other:?}"),
        }
    }

    /// Right arrow with a non-empty FOCUSED reply moves the caret within
    /// the draft instead of opening the agent — the reply text is
    /// preserved and no attach is emitted.
    #[test]
    fn peek_right_arrow_with_text_moves_caret_not_open() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        state.peek_reply.set_text("draft");
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let outcome = state.handle_key(&right, &reg);
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardAttach(_))),
            "Right with a focused non-empty reply must NOT open the agent, got {outcome:?}",
        );
        assert_eq!(
            state.peek_reply.text(),
            "draft",
            "Right must leave the reply draft intact",
        );
    }

    /// Up/Down switch the peeked agent (the panel follows the selection
    /// cursor).
    #[test]
    fn peek_arrows_switch_selected_agent() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        // Empty reply → arrows are a navigation surface.
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            state.handle_key(&down, &reg),
            InputOutcome::Action(Action::DashboardSelectNext)
        ));
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            state.handle_key(&up, &reg),
            InputOutcome::Action(Action::DashboardSelectPrev)
        ));
    }

    /// With a non-empty FOCUSED reply, bare Up/Down move the caret
    /// WITHIN the reply text (multi-line draft) instead of switching the
    /// peeked agent — they edit, never emit a `DashboardSelect*` action,
    /// and leave the draft text untouched.
    #[test]
    fn peek_arrows_move_caret_when_reply_has_content() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        // Two-line draft (caret at the start after set_text).
        state.peek_reply.set_text("line one\nline two");

        // Down must NOT switch agents — it moves the caret down a line.
        let down = state.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &reg);
        assert!(
            !matches!(down, InputOutcome::Action(_)),
            "Down with reply content must edit the caret, not switch agents, got {down:?}",
        );
        // Up likewise stays within the input.
        let up = state.handle_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &reg);
        assert!(
            !matches!(up, InputOutcome::Action(_)),
            "Up with reply content must edit the caret, not switch agents, got {up:?}",
        );
        // The draft is untouched (caret moves only).
        assert_eq!(state.peek_reply.text(), "line one\nline two");
    }

    /// An UNFOCUSED peek (Tab → row nav) keeps Up/Down as agent-switch
    /// even with reply content — the reply isn't the active surface.
    #[test]
    fn peek_arrows_switch_agent_when_unfocused_despite_content() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        state.peek_reply.set_text("a draft");
        state.peek.as_mut().unwrap().focused = false; // Tab → row nav
        assert!(matches!(
            state.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &reg),
            InputOutcome::Action(Action::DashboardSelectNext)
        ));
    }

    /// The (peek-less) dispatch input mirrors the peek: with content,
    /// bare Up/Down move the caret within the text (no `DashboardSelect*`
    /// emitted); with an EMPTY prompt they navigate the row list (browse
    /// convenience).
    #[test]
    fn dispatch_arrows_move_caret_with_content_navigate_when_empty() {
        use crate::app::actions::Action;
        let reg = crate::actions::ActionRegistry::defaults();

        // Empty prompt → Up/Down navigate the list.
        let mut empty = make_state_with_selection();
        assert!(empty.dispatch.text().is_empty());
        assert!(matches!(
            empty.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &reg),
            InputOutcome::Action(Action::DashboardSelectNext)
        ));

        // Non-empty multi-line prompt → Up/Down edit the caret, never
        // switching the selected row.
        let mut typed = make_state_with_selection();
        typed.dispatch.set_text("line one\nline two");
        let down = typed.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &reg);
        assert!(
            !matches!(
                down,
                InputOutcome::Action(Action::DashboardSelectNext | Action::DashboardSelectPrev)
            ),
            "Down with dispatch content must move the caret, not the list, got {down:?}",
        );
        let up = typed.handle_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &reg);
        assert!(
            !matches!(
                up,
                InputOutcome::Action(Action::DashboardSelectNext | Action::DashboardSelectPrev)
            ),
            "Up with dispatch content must move the caret, not the list, got {up:?}",
        );
    }

    /// Space closes the peek when the reply is empty (quick toggle) but
    /// types a space once the user has started composing.
    #[test]
    fn peek_space_types_into_reply() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        // The peek is tied to selection now, so Space is plain text (no
        // close) — Esc unselects instead.
        for c in ['h', 'i'] {
            let _ = state.handle_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &reg);
        }
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        let _ = state.handle_key(&space, &reg);
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &reg);
        assert_eq!(state.peek_reply.text(), "hi y");
        // Peek stays open while the row is selected.
        assert!(state.peek.is_some());
    }

    /// Esc unselects: with an empty draft it clears the selection and
    /// focuses the `[+ New Agent]` button (the new-session entry); a
    /// typed draft is cleared first.
    #[test]
    fn peek_esc_clears_draft_then_unselects() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        state.peek_reply.set_text("draft");
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        // First Esc clears the draft, keeps the peek + selection.
        let _ = state.handle_key(&esc, &reg);
        assert!(state.peek_reply.text().is_empty());
        assert!(state.selected.is_some());
        // Second Esc unselects → focuses the + New Agent button + closes peek.
        let _ = state.handle_key(&esc, &reg);
        assert!(state.peek.is_none());
        assert!(state.selected.is_none());
        assert!(state.new_agent_button_focused);
    }

    /// Ctrl-modified chords are never TYPED into the reply: non-bound
    /// editing chords (Ctrl+A → caret-to-start) are delegated to the
    /// widget as edits, while registry-bound dashboard chords (Ctrl+X
    /// stop, Ctrl+T pin, …) fall through so they keep firing with the
    /// peek open.
    #[test]
    fn peek_ctrl_keys_fall_through_not_typed() {
        use crate::app::actions::Action;
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let _ = state.handle_key(&ctrl_a, &reg);
        // 'a' must not have been typed into the reply (Ctrl+A is the
        // caret-to-start editing chord, not text input).
        assert!(state.peek_reply.text().is_empty());

        // A registry-bound dashboard chord still falls through to its
        // action with the peek open (Ctrl+T → pin toggle).
        let ctrl_t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(
            matches!(
                state.handle_key(&ctrl_t, &reg),
                InputOutcome::Action(Action::DashboardTogglePin)
            ),
            "registry-bound Ctrl+T must keep firing with the peek open",
        );
        assert!(state.peek_reply.text().is_empty());
    }

    /// Ctrl+C / Ctrl+D bubble up as `Unchanged` so the app-global
    /// quit handler fires — they are not typed into the reply or
    /// swallowed by the peek.
    #[test]
    fn peek_ctrl_c_d_bubble_to_global_quit() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            state.handle_key(&ctrl_c, &reg),
            InputOutcome::Unchanged
        ));
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(matches!(
            state.handle_key(&ctrl_d, &reg),
            InputOutcome::Unchanged
        ));
        // The peek stays open and nothing was typed.
        assert!(state.peek.is_some());
        assert!(state.peek_reply.text().is_empty());
    }

    /// Question picker flow: no option is selected by default (arrows switch
    /// agents, Enter opens). A number key selects (and toggles) an option;
    /// then `↑`/`↓` move within the options (spilling to the prev/next agent
    /// at the edges) and `Enter` answers the selected option.
    #[test]
    fn peek_arrows_navigate_options_and_enter_answers() {
        use crate::app::actions::Action;
        let mut state = make_state_with_selection();
        let mut f = peek_fields_for_test("Awaiting your input");
        f.question = Some("Allow Edit?".into());
        f.options = vec![
            ("allow".into(), "Allow".into()),
            ("deny".into(), "Deny".into()),
        ];
        f.request_id = Some(7);
        state.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            f,
        ));
        let reg = crate::actions::ActionRegistry::defaults();

        // Default: nothing selected → Down switches to the next agent.
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            state.handle_key(&down, &reg),
            InputOutcome::Action(Action::DashboardSelectNext)
        ));
        assert_eq!(state.peek.as_ref().unwrap().selected_option, None);

        // Pressing `1` selects the first option (and focuses the picker).
        let one = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE);
        assert!(matches!(
            state.handle_key(&one, &reg),
            InputOutcome::Changed
        ));
        assert_eq!(state.peek.as_ref().unwrap().selected_option, Some(0));

        // Now Down moves within the options to option 1.
        assert!(matches!(
            state.handle_key(&down, &reg),
            InputOutcome::Changed
        ));
        assert_eq!(state.peek.as_ref().unwrap().selected_option, Some(1));
        // Down again at the LAST option spills out to the next agent; the
        // selection is left unchanged.
        assert!(matches!(
            state.handle_key(&down, &reg),
            InputOutcome::Action(Action::DashboardSelectNext)
        ));
        assert_eq!(state.peek.as_ref().unwrap().selected_option, Some(1));

        // Enter answers the selected option (index 1 → "deny").
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_key(&enter, &reg) {
            InputOutcome::Action(Action::DashboardPermissionSelect {
                request_id,
                option_id,
                ..
            }) => {
                assert_eq!(request_id, 7);
                assert_eq!(option_id.0.as_ref(), "deny");
            }
            other => panic!("expected DashboardPermissionSelect, got {other:?}"),
        }

        // Up moves back toward the first option.
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let _ = state.handle_key(&up, &reg);
        assert_eq!(state.peek.as_ref().unwrap().selected_option, Some(0));
        // Up again at the FIRST option spills out to the previous row.
        assert!(matches!(
            state.handle_key(&up, &reg),
            InputOutcome::Action(Action::DashboardSelectPrev)
        ));
        assert_eq!(state.peek.as_ref().unwrap().selected_option, Some(0));

        // Pressing `1` again toggles the selection off → back to navigation,
        // where Enter opens the agent in detail.
        assert!(matches!(
            state.handle_key(&one, &reg),
            InputOutcome::Changed
        ));
        assert_eq!(state.peek.as_ref().unwrap().selected_option, None);
        assert!(matches!(
            state.handle_key(&enter, &reg),
            InputOutcome::Action(Action::DashboardAttach(_))
        ));
    }

    /// Right mirrors Enter on the question-picker navigation surface:
    /// with the panel focused and NO option selected, a bare Right opens
    /// the peeked row in detail — just like Enter. Regression guard: the
    /// `question_mode` block ends in a modal catch-all that returns
    /// `Unchanged`, so without an explicit Right arm Enter opened but
    /// Right did nothing (an inconsistent dead key).
    #[test]
    fn peek_right_arrow_opens_agent_in_focused_question_picker() {
        use crate::app::actions::Action;
        let mut state = make_state_with_selection();
        let mut f = peek_fields_for_test("Awaiting your input");
        f.question = Some("Allow Edit?".into());
        f.options = vec![
            ("allow".into(), "Allow".into()),
            ("deny".into(), "Deny".into()),
        ];
        f.request_id = Some(7);
        state.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            f,
        ));
        // Pin focused: PeekPanelState::new seeds from load_vim_mode().
        state.peek.as_mut().unwrap().focused = true;
        let reg = crate::actions::ActionRegistry::defaults();
        assert_eq!(state.peek.as_ref().unwrap().selected_option, None);

        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        match state.handle_key(&right, &reg) {
            InputOutcome::Action(Action::DashboardAttach(row)) => {
                assert_eq!(row, DashboardRowId::TopLevel(AgentId(0)));
            }
            other => panic!(
                "Right in the focused question picker must open the agent (DashboardAttach), got {other:?}",
            ),
        }
    }

    /// The reject ("No") option accepts inline free-text feedback:
    /// typing on it composes a message and `Enter` sends the rejection
    /// with that feedback. Typing on a non-reject option is consumed.
    #[test]
    fn peek_reject_option_accepts_typed_feedback() {
        use crate::app::actions::Action;
        let mut state = make_state_with_selection();
        let mut f = peek_fields_for_test("Awaiting your input");
        f.question = Some("Allow Edit?".into());
        f.options = vec![
            ("allow".into(), "Allow".into()),
            ("reject".into(), "No".into()),
        ];
        f.request_id = Some(9);
        f.reject_option = Some(1);
        state.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            f,
        ));
        let reg = crate::actions::ActionRegistry::defaults();

        // With no option selected, typing a letter is consumed — no feedback
        // composed and it doesn't leak into the reply buffer.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &reg);
        assert!(state.peek_reply.text().is_empty());

        // Select the reject option (index 1 → key `2`), then type feedback.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE), &reg);
        assert_eq!(state.peek.as_ref().unwrap().selected_option, Some(1));
        for c in ['n', 'o', 'p', 'e'] {
            let _ = state.handle_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &reg);
        }
        assert_eq!(state.peek_reply.text(), "nope");

        // Enter sends the rejection with the typed feedback.
        match state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg) {
            InputOutcome::Action(Action::DashboardPermissionFollowup {
                request_id, text, ..
            }) => {
                assert_eq!(request_id, 9);
                assert_eq!(text, "nope");
            }
            other => panic!("expected DashboardPermissionFollowup, got {other:?}"),
        }
    }

    /// `clear_peek_reply` (used on every lifecycle clear — row change,
    /// open/close, send) wipes the undo history too, so `Ctrl+Z` can't
    /// resurrect a draft typed for a DIFFERENT agent onto the newly
    /// peeked one. Regression for the cross-agent mis-send hole that a
    /// bare `set_text("")` left open (set_text records an undoable
    /// `Replace` checkpoint).
    #[test]
    fn peek_clear_wipes_undo_so_ctrl_z_cannot_resurrect_draft() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        for c in "secret for A".chars() {
            let _ = state.handle_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &reg);
        }
        assert_eq!(state.peek_reply.text(), "secret for A");
        // Simulate the row-change / lifecycle clear.
        state.clear_peek_reply();
        assert!(state.peek_reply.text().is_empty());
        // Ctrl+Z while the (now different-agent) reply is focused must
        // NOT bring the old draft back.
        let _ = state.handle_key(
            &KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
            &reg,
        );
        assert!(
            state.peek_reply.text().is_empty(),
            "undo must not resurrect a cleared cross-agent draft, got {:?}",
            state.peek_reply.text(),
        );
    }

    /// Typing `@` in the peek reply activates the session-less file
    /// context picker (rooted at the launch cwd, like the dispatch box),
    /// so the `@` dropdown can stream in and render above the panel.
    #[test]
    fn peek_typing_at_activates_file_search() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        assert!(
            state.peek_reply.file_search.context().is_none(),
            "no @-context before typing @",
        );
        for c in ['@', 's', 'r', 'c'] {
            let _ = state.handle_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &reg);
        }
        assert_eq!(state.peek_reply.text(), "@src");
        assert!(
            state.peek_reply.file_search.context().is_some(),
            "typing @ must activate the reply's file-search picker",
        );
    }

    /// Mouse-wheel scrolling over the `@` dropdown must drive the SAME
    /// picker that is rendered: the peek reply's while the panel is
    /// open, the dispatch box's otherwise. (Regression: the wheel
    /// intercept hardcoded `dispatch.file_search`, so scrolling the peek
    /// dropdown moved the hidden dispatch selection while the visible
    /// list stayed put.) Uses `context()` as a cheap observable for
    /// "which picker" — `@`-context is set synchronously, unlike the
    /// async results `is_visible()` needs.
    #[test]
    fn dropdown_file_search_follows_peek_state() {
        // Peek open → the picker behind the dropdown is the reply's.
        let mut open = state_with_open_peek();
        open.peek_reply.file_search.update_context("@a", 2);
        assert!(
            open.dropdown_file_search_mut().context().is_some(),
            "with the peek open, wheel scrolling must target the reply's picker",
        );

        // Peek closed → it's the dispatch box's.
        let mut closed = DashboardState::new();
        closed.dispatch.file_search.update_context("@b", 2);
        assert!(closed.peek.is_none());
        assert!(
            closed.dropdown_file_search_mut().context().is_some(),
            "with the peek closed, wheel scrolling must target the dispatch picker",
        );
    }

    /// The reply's `@` picker roots LAZILY at the peeked agent's cwd: a
    /// bare cursor move (navigation) never retargets the daemon (no
    /// thread churn), and the retarget lands on the first composing
    /// keystroke, deduped so a same-cwd agent switch is free.
    #[test]
    fn peek_reply_file_search_retargets_lazily_on_compose() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        // The render pass records the peeked agent's cwd; simulate it.
        state.set_peek_reply_target_cwd(Some(PathBuf::from("/work/repo-a")));
        assert_eq!(state.peek_reply_cwd, None, "daemon not retargeted yet");

        // Bare Down on an EMPTY reply switches agents — must NOT retarget.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &reg);
        assert_eq!(
            state.peek_reply_cwd, None,
            "navigation must not spawn a matcher daemon",
        );

        // First composing keystroke retargets to the recorded cwd.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &reg);
        assert_eq!(
            state.peek_reply_cwd.as_deref(),
            Some(Path::new("/work/repo-a")),
            "first compose must root the picker at the peeked agent's cwd",
        );

        // Switching to a different-cwd agent retargets once on next compose.
        state.set_peek_reply_target_cwd(Some(PathBuf::from("/work/repo-b")));
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &reg);
        assert_eq!(
            state.peek_reply_cwd.as_deref(),
            Some(Path::new("/work/repo-b")),
            "a cwd change retargets on the next compose",
        );
    }

    /// In question mode the `❯ reply` line is hidden, so a paste must NOT
    /// silently fill the (invisible) reply buffer unless the reject /
    /// "Other" free-text option is the selected one. (Regression: paste
    /// used to land in `peek_reply` regardless and resurface later.)
    #[test]
    fn peek_paste_in_question_mode_gated_on_reject_selection() {
        let mut state = make_state_with_selection();
        let reg = crate::actions::ActionRegistry::defaults();
        let mut f = peek_fields_for_test("Awaiting your input");
        f.question = Some("Allow Edit?".into());
        f.options = vec![
            ("allow".into(), "Allow".into()),
            ("reject".into(), "No".into()),
        ];
        f.request_id = Some(9);
        f.reject_option = Some(1);
        state.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            f,
        ));

        // No option selected → paste is dropped (would be invisible).
        let outcome = state.handle_input(&Event::Paste("ignored".to_string()), &reg);
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert!(
            state.peek_reply.text().is_empty(),
            "paste must not fill the hidden reply when no reject option is selected",
        );

        // Select the reject option → paste now lands in the feedback field.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE), &reg);
        assert_eq!(state.peek.as_ref().unwrap().selected_option, Some(1));
        let outcome = state.handle_input(&Event::Paste("real feedback".to_string()), &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.peek_reply.text(), "real feedback");
    }

    /// The Ask tool (`AskUserQuestion`) is answered from the peek too:
    /// selecting an option emits `DashboardQuestionAnswer { option_idx }`,
    /// and the "Other" free-text row emits it with `option_idx: None` +
    /// the typed text. (Ask questions carry no `request_id`.)
    #[test]
    fn peek_ask_question_answer_routing() {
        use crate::app::actions::Action;
        let mut state = make_state_with_selection();
        let mut f = peek_fields_for_test("Awaiting your input");
        f.question = Some("Which approach?".into());
        // Two real options + an appended "Other" free-text row.
        f.options = vec![
            ("Redis".into(), "Redis".into()),
            ("In-memory".into(), "In-memory".into()),
            ("__other__".into(), "Other".into()),
        ];
        f.reject_option = Some(2);
        f.request_id = None; // ← marks it as an ask question, not a permission
        state.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            f,
        ));
        let reg = crate::actions::ActionRegistry::defaults();

        // Select the first option (key `1`), then Enter answers by index.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE), &reg);
        assert_eq!(state.peek.as_ref().unwrap().selected_option, Some(0));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_key(&enter, &reg) {
            InputOutcome::Action(Action::DashboardQuestionAnswer {
                option_idx,
                freeform,
                ..
            }) => {
                assert_eq!(option_idx, Some(0));
                assert!(freeform.is_empty());
            }
            other => panic!("expected DashboardQuestionAnswer, got {other:?}"),
        }

        // Select the "Other" row (index 2 → key `3`), type free-text, Enter
        // → freeform answer.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE), &reg);
        assert_eq!(state.peek.as_ref().unwrap().selected_option, Some(2));
        for c in ['s', 'q', 'l'] {
            let _ = state.handle_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &reg);
        }
        match state.handle_key(&enter, &reg) {
            InputOutcome::Action(Action::DashboardQuestionAnswer {
                option_idx,
                freeform,
                ..
            }) => {
                assert_eq!(option_idx, None);
                assert_eq!(freeform, "sql");
            }
            other => panic!("expected DashboardQuestionAnswer(Other), got {other:?}"),
        }
    }

    /// Tab toggles peek reply focus; unfocused printable re-focuses and types (non-vim).
    #[test]
    fn peek_tab_toggles_focus_and_typing_refocuses() {
        crate::appearance::cache::set_vim_mode(false);
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        assert!(state.peek.as_ref().unwrap().focused);
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let _ = state.handle_key(&tab, &reg);
        assert!(!state.peek.as_ref().unwrap().focused, "Tab must unfocus");
        // Typing while unfocused re-focuses and composes.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &reg);
        assert!(
            state.peek.as_ref().unwrap().focused,
            "typing must re-focus the reply"
        );
        assert_eq!(state.peek_reply.text(), "y");
    }

    /// Vim: peek reply starts unfocused; j navigates; Enter focuses (no attach).
    #[test]
    fn vim_peek_opens_unfocused_jk_nav_enter_focuses() {
        let mut state = state_with_open_peek();
        // Fixture pins vim off; re-enable and rebuild so the panel is
        // born unfocused under vim.
        crate::appearance::cache::set_vim_mode(true);
        state.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            peek_fields_for_test("Idle"),
        ));
        let reg = crate::actions::ActionRegistry::defaults();
        assert!(
            !state.peek.as_ref().unwrap().focused,
            "vim peek must not auto-focus the reply"
        );
        let j = state.handle_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &reg);
        assert!(
            matches!(j, InputOutcome::Action(Action::DashboardSelectNext)),
            "vim j on unfocused peek must navigate, got {j:?}"
        );
        assert!(
            state.peek_reply.text().is_empty(),
            "j must not type into the reply, got {:?}",
            state.peek_reply.text()
        );
        let enter = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(matches!(enter, InputOutcome::Changed));
        assert!(
            state.peek.as_ref().unwrap().focused,
            "Enter must focus the peek reply in vim mode"
        );
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &reg);
        assert_eq!(state.peek_reply.text(), "j");
        crate::appearance::cache::set_vim_mode(false);
    }

    /// Vim unfocused: `i` focuses without inserting; other printables are swallowed.
    #[test]
    fn vim_peek_unfocused_i_focuses_printable_swallowed() {
        let mut state = state_with_open_peek();
        crate::appearance::cache::set_vim_mode(true);
        state.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            peek_fields_for_test("Idle"),
        ));
        let reg = crate::actions::ActionRegistry::defaults();
        assert!(!state.peek.as_ref().unwrap().focused);

        let x = state.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &reg);
        assert!(
            matches!(x, InputOutcome::Unchanged),
            "vim unfocused printable must be swallowed, got {x:?}"
        );
        assert!(
            !state.peek.as_ref().unwrap().focused,
            "swallowed key must not focus the reply"
        );
        assert!(
            state.peek_reply.text().is_empty(),
            "swallowed key must not type, got {:?}",
            state.peek_reply.text()
        );

        let i = state.handle_key(&KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &reg);
        assert!(matches!(i, InputOutcome::Changed));
        assert!(
            state.peek.as_ref().unwrap().focused,
            "i must focus the peek reply"
        );
        assert!(
            state.peek_reply.text().is_empty(),
            "i must not be inserted, got {:?}",
            state.peek_reply.text()
        );
        crate::appearance::cache::set_vim_mode(false);
    }

    /// Vim: apply_fields row change clears peek reply focus.
    #[test]
    fn vim_peek_row_change_unfocuses_reply() {
        let mut state = state_with_open_peek();
        crate::appearance::cache::set_vim_mode(true);
        state.peek.as_mut().unwrap().focused = true;
        let other = DashboardRowId::TopLevel(AgentId(99));
        let fields = super::super::peek::PeekFields {
            label: "other".into(),
            time_ago: String::new(),
            response_type: "Idle".into(),
            last_user_message: None,
            question: None,
            options: vec![],
            request_id: None,
            reject_option: None,
        };
        let changed = state.peek.as_mut().unwrap().apply_fields(other, fields);
        assert!(changed);
        assert!(
            !state.peek.as_ref().unwrap().focused,
            "vim row change must unfocus the reply"
        );
        crate::appearance::cache::set_vim_mode(false);
    }

    /// Non-registry editing chords reach the reply widget while the
    /// peek is focused: Ctrl+A moves the caret to the start and Ctrl+K
    /// kills to end-of-line — the full `PromptWidget` editing surface,
    /// not the old bare-char-only editor.
    #[test]
    fn peek_editing_chords_reach_reply_widget() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        for c in ['a', 'b', 'c'] {
            let _ = state.handle_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &reg);
        }
        assert_eq!(state.peek_reply.text(), "abc");
        // Ctrl+A → caret to line start (consumed by the widget, NOT a
        // dashboard action and NOT typed).
        let _ = state.handle_key(
            &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &reg,
        );
        assert_eq!(state.peek_reply.cursor(), 0, "Ctrl+A must move the caret");
        // Ctrl+K → kill to end of line.
        let _ = state.handle_key(
            &KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &reg,
        );
        assert!(
            state.peek_reply.text().is_empty(),
            "Ctrl+K must kill to end of line, got {:?}",
            state.peek_reply.text(),
        );
        // The hidden dispatch input was never touched.
        assert!(state.dispatch.text().is_empty());
    }

    /// Drag-selecting text in the dispatch box works like the peek
    /// reply: Down inside the rect anchors the drag, Drag extends the
    /// textarea selection, and Up finishes it — Drag/Up are forwarded
    /// even when the pointer leaves the box.
    #[test]
    fn dispatch_mouse_drag_selects_text() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::buffer::Buffer;

        let style = crate::views::prompt_widget::PromptStyle {
            focused: true,
            show_prefix: true,
            vpad_top: 0,
            chrome: false,
            ..crate::views::prompt_widget::PromptStyle::default()
        };
        let mut state = DashboardState::new();
        state.dispatch.set_text("hello world");
        let rect = Rect::new(2, 1, 60, 1);
        state.dispatch_rect = Some(rect);
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
        let _ = state
            .dispatch
            .draw(&mut buf, rect, None, &style, None, None);
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), 4),
            (MouseEventKind::Drag(MouseButton::Left), 12),
            // The drag continues past the right edge of the box and is
            // released there; the selection must keep extending.
            (MouseEventKind::Drag(MouseButton::Left), 70),
            (MouseEventKind::Up(MouseButton::Left), 70),
        ] {
            let _ = state.handle_mouse(&MouseEvent {
                kind,
                column,
                row: 1,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(
            state.dispatch.textarea.selection_range(),
            Some(0..11),
            "dispatch drag must extend the textarea selection like the peek reply",
        );
    }

    /// A left click inside the recorded reply rect focuses the reply
    /// input (mirrors the dispatch box's click-to-focus) and routes the
    /// event to the widget.
    #[test]
    fn peek_mouse_click_on_reply_rect_focuses() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut state = state_with_open_peek();
        state.peek.as_mut().unwrap().focused = false;
        state.peek_reply_rect = Some(Rect::new(2, 10, 40, 1));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        let outcome = state.handle_mouse(&click);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            state.peek.as_ref().unwrap().focused,
            "a click on the reply rect must focus the reply input",
        );
        // A click outside the rect (on no row) leaves focus alone.
        state.peek.as_mut().unwrap().focused = false;
        let miss = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 70,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };
        let _ = state.handle_mouse(&miss);
        assert!(
            !state.peek.as_ref().unwrap().focused,
            "a click outside the reply rect must not grab focus",
        );
    }

    /// Esc never wipes a typed dispatch draft. On a focused input the
    /// first Esc unfocuses (blurs) to the overview list so the user can
    /// navigate; the draft is left intact. A later Esc exits, still
    /// keeping the draft (retained across a same-process close/reopen of
    /// the dashboard; not persisted across an app restart).
    #[test]
    fn esc_preserves_dispatch_text() {
        let mut state = DashboardState::new();
        state.dispatch.set_text("fix the bug");
        assert!(!state.list_focused, "input focused by default");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        // First Esc blurs the input → list focus; draft preserved.
        let first = state.handle_key(&key, &reg);
        assert!(matches!(first, InputOutcome::Changed));
        assert!(state.list_focused, "Esc unfocuses the input");
        assert_eq!(state.dispatch.text(), "fix the bug");
        // Second Esc (list focused, nothing selected) exits — draft kept.
        let second = state.handle_key(&key, &reg);
        assert!(matches!(
            second,
            InputOutcome::Action(Action::ExitDashboard)
        ));
        assert_eq!(state.dispatch.text(), "fix the bug");
    }

    /// Esc on a focused input blurs to the list without touching the
    /// draft, even when a row is selected (the selection survives the
    /// blur and is only backed out of by a later Esc).
    #[test]
    fn esc_blurs_input_keeps_draft_and_selection() {
        let mut state = make_state_with_selection();
        state.dispatch.set_text("hello");
        assert!(!state.list_focused, "input focused by default");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(state.list_focused, "Esc unfocuses the input");
        assert!(state.selected.is_some(), "selection preserved on blur");
        assert_eq!(state.dispatch.text(), "hello", "draft is preserved");
    }

    /// edge case 13: Esc with empty input + active filter
    /// clears filter.
    #[test]
    fn esc_clears_active_filter() {
        let mut state = make_state_with_selection();
        state.filter = Filter::Agent("reviewer".into());
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(matches!(state.filter, Filter::None));
    }

    /// Esc with nothing to back out of: the first Esc unfocuses the
    /// input (→ overview list), the second exits the dashboard. The
    /// focus tier sits above exit so a focused input always blurs first.
    #[test]
    fn esc_with_nothing_to_clear_blurs_then_exits() {
        let mut state = DashboardState::new();
        assert!(!state.list_focused, "input focused by default");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let first = state.handle_key(&key, &reg);
        assert!(matches!(first, InputOutcome::Changed));
        assert!(state.list_focused, "first Esc unfocuses the input");
        let second = state.handle_key(&key, &reg);
        assert!(matches!(
            second,
            InputOutcome::Action(Action::ExitDashboard)
        ));
    }

    /// With the list focused and a row selected, Esc DESELECTS instead
    /// of exiting. The user's contract hinges on this: a selected row
    /// turns the dispatch input into "reply to this agent"; deselecting
    /// flips it back to "create a new session" without leaving the
    /// dashboard. (From a focused input Esc would blur first; here we
    /// start already on the list.)
    #[test]
    fn esc_with_selection_deselects() {
        let mut state = make_state_with_selection();
        state.list_focused = true;
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Esc on a selected dashboard must report `Changed`, got {outcome:?}",
        );
        assert!(
            state.selected.is_none(),
            "Esc must clear `selected` so the next dispatch reaches the new-session path",
        );
        assert!(
            state.new_agent_button_focused,
            "Esc-deselect must focus the `[+ New Agent]` button as the new cursor target",
        );
    }

    /// Enter with an empty prompt while the button is
    /// focused emits `DashboardCreateNewAgentWithDetail`. The
    /// state handler returns the action; the dispatcher then
    /// spawns the session + switches to its detail view.
    #[test]
    fn enter_on_focused_button_with_empty_prompt_emits_create_with_detail() {
        use crate::app::actions::Action;
        let mut state = DashboardState::new();
        // Fresh state defaults to button-focused; pin that
        // precondition so a future regression doesn't quietly
        // flip the default away from the button.
        assert!(state.new_agent_button_focused);
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        assert!(
            matches!(
                outcome,
                InputOutcome::Action(Action::DashboardCreateNewAgentWithDetail),
            ),
            "Enter on focused button with empty prompt must emit \
             DashboardCreateNewAgentWithDetail, got: {outcome:?}",
        );
    }

    /// Enter with a NON-empty prompt while the button is
    /// focused emits `DashboardDispatch` (the regular new-session
    /// path). Detail view does NOT open: the user wanted to fire
    /// off a session and keep working in the dashboard.
    #[test]
    fn enter_on_focused_button_with_non_empty_prompt_emits_dispatch() {
        use crate::app::actions::Action;
        let mut state = DashboardState::new();
        state.dispatch.set_text("queue a task");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        match outcome {
            InputOutcome::Action(Action::DashboardDispatch { text, attach }) => {
                assert_eq!(text, "queue a task");
                assert!(!attach, "plain Enter must keep `attach=false`");
            }
            other => panic!(
                "Enter on focused button with non-empty prompt must emit \
                 DashboardDispatch (NOT CreateNewAgentWithDetail), got: {other:?}",
            ),
        }
    }

    /// Ctrl+S ("send + open") on focused button + non-empty prompt
    /// emits `DashboardDispatch { attach: true }` so the
    /// dispatcher's new-session arm switches view AND sets
    /// `attached_agent`. The state handler doesn't know about attach
    /// semantics — it just forwards the chord through the payload.
    #[test]
    fn ctrl_s_on_focused_button_with_text_emits_dispatch_with_attach_true() {
        use crate::app::actions::Action;
        let mut state = DashboardState::new();
        state.dispatch.set_text("send and open");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let outcome = state.handle_key(&key, &reg);
        match outcome {
            InputOutcome::Action(Action::DashboardDispatch { text, attach }) => {
                assert_eq!(text, "send and open");
                assert!(attach, "Ctrl+S must set `attach=true`");
            }
            other => panic!(
                "Ctrl+S on focused button with text must emit \
                 DashboardDispatch {{ attach: true }}, got: {other:?}",
            ),
        }
    }

    /// Enter on a row-selected dashboard with an EMPTY
    /// prompt emits `DashboardAttach(row_id)` so the dispatcher
    /// opens the detail view without sending anything.
    #[test]
    fn enter_on_row_selected_empty_prompt_emits_attach() {
        use crate::app::actions::Action;
        let mut state = make_state_with_selection();
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        match outcome {
            InputOutcome::Action(Action::DashboardAttach(id)) => {
                assert_eq!(id, DashboardRowId::TopLevel(AgentId(0)));
            }
            other => panic!(
                "Enter on row + empty prompt must emit DashboardAttach, \
                 got: {other:?}",
            ),
        }
    }

    /// Enter on a row-selected dashboard with TYPED text
    /// emits `DashboardDispatch { attach: false }` so the
    /// dispatcher's reply arm sends without leaving the
    /// dashboard.
    #[test]
    fn enter_on_row_selected_with_text_emits_dispatch_no_attach() {
        use crate::app::actions::Action;
        let mut state = make_state_with_selection();
        state.dispatch.set_text("reply to selected");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        match outcome {
            InputOutcome::Action(Action::DashboardDispatch { text, attach }) => {
                assert_eq!(text, "reply to selected");
                assert!(!attach);
            }
            other => panic!(
                "Enter on row + text must emit DashboardDispatch {{ attach: false }}, \
                 got: {other:?}",
            ),
        }
    }

    /// Ctrl+S ("send + open") on a row-selected dashboard with TYPED
    /// text emits `DashboardDispatch { attach: true }`.
    #[test]
    fn ctrl_s_on_row_selected_with_text_emits_dispatch_with_attach() {
        use crate::app::actions::Action;
        let mut state = make_state_with_selection();
        state.dispatch.set_text("reply and open");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let outcome = state.handle_key(&key, &reg);
        match outcome {
            InputOutcome::Action(Action::DashboardDispatch { text, attach }) => {
                assert_eq!(text, "reply and open");
                assert!(attach, "Ctrl+S must set `attach=true`");
            }
            other => panic!(
                "Ctrl+S on row + text must emit DashboardDispatch {{ attach: true }}, \
                 got: {other:?}",
            ),
        }
    }

    /// Ctrl+S ("send + open") on focused button with EMPTY prompt
    /// behaves like plain Enter — emits `CreateNewAgentWithDetail`.
    /// There's nothing to "send" so the chord collapses to:
    /// the only sensible interpretation is "create + open
    /// detail", which the unmodified Enter already does.
    #[test]
    fn ctrl_s_on_focused_button_with_empty_prompt_emits_create_with_detail() {
        use crate::app::actions::Action;
        let mut state = DashboardState::new();
        assert!(state.new_agent_button_focused);
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let outcome = state.handle_key(&key, &reg);
        assert!(
            matches!(
                outcome,
                InputOutcome::Action(Action::DashboardCreateNewAgentWithDetail),
            ),
            "Ctrl+S on focused button with empty prompt must collapse to \
             CreateNewAgentWithDetail, got: {outcome:?}",
        );
    }

    /// Full Esc cascade from a focused input with a row selected:
    /// blur (→ list) → deselect (→ `[+ New Agent]`) → exit. Pins the
    /// tier ordering, catching a regression that would skip any tier.
    #[test]
    fn esc_cascade_blurs_then_deselects_then_exits() {
        let mut state = make_state_with_selection();
        assert!(!state.list_focused, "input focused by default");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        // 1: blur the input to the list (selection survives).
        let first = state.handle_key(&key, &reg);
        assert!(matches!(first, InputOutcome::Changed));
        assert!(state.list_focused, "first Esc unfocuses the input");
        assert!(state.selected.is_some(), "selection survives the blur");
        // 2: deselect the row.
        let second = state.handle_key(&key, &reg);
        assert!(matches!(second, InputOutcome::Changed));
        assert!(state.selected.is_none());
        assert!(state.new_agent_button_focused);
        // 3: exit.
        let third = state.handle_key(&key, &reg);
        assert!(
            matches!(third, InputOutcome::Action(Action::ExitDashboard)),
            "third Esc must exit the dashboard, got {third:?}",
        );
    }

    /// New contract: a `a:` / `s:` / `#` prefix is NO LONGER treated
    /// as a filter on Enter — filtering is the explicit `Ctrl+/`
    /// search mode now, so a prompt that merely starts with a prefix
    /// dispatches verbatim. This pins the bug fix: prefixed prompts
    /// must not be silently swallowed as filters.
    #[test]
    fn enter_with_prefix_text_dispatches_not_filters() {
        let mut state = make_state_with_selection();
        state.dispatch.set_text("a:reviewer please refactor");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        match outcome {
            InputOutcome::Action(Action::DashboardDispatch { text, attach }) => {
                assert_eq!(text, "a:reviewer please refactor");
                assert!(!attach);
            }
            other => panic!("prefix text must DISPATCH, not filter, got {other:?}"),
        }
        assert!(
            matches!(state.filter, Filter::None),
            "dispatch must not set a filter",
        );
    }

    /// edge case 21: Enter on free text dispatches.
    /// Assert the payload matches the typed text and that
    /// `attach` is false (no Shift modifier). A regression that
    /// dispatched a different string (or swallowed the input) would
    /// be invisible to a `matches!` assertion that ignores the
    /// payload fields.
    #[test]
    fn enter_with_free_text_dispatches() {
        let mut state = make_state_with_selection();
        let typed = "write some tests";
        state.dispatch.set_text(typed);
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let outcome = state.handle_key(&key, &reg);
        match outcome {
            InputOutcome::Action(Action::DashboardDispatch { text, attach }) => {
                assert_eq!(text, typed, "Dispatch payload must echo the typed text");
                assert!(!attach, "Plain Enter must not set attach=true");
            }
            other => panic!("expected DashboardDispatch, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Reconciled dispatch-input features: slash commands, Alt+Enter
    // multiline, vim-gated j/k, and paste — layered on top of the
    // reply-mode / search-mode base.
    // -----------------------------------------------------------------

    /// A `/command` Enter routes through the session-less slash
    /// dispatcher instead of becoming a new session's prompt.
    #[test]
    fn slash_command_on_enter_dispatches_slash() {
        let mut state = make_state_with_selection();
        state.dispatch.set_text("/help");
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_key(&key, &reg) {
            InputOutcome::Action(Action::DashboardDispatchSlash { text }) => {
                assert_eq!(text, "/help");
            }
            other => panic!("expected DashboardDispatchSlash, got {other:?}"),
        }
    }

    /// Alt+Enter AND Shift+Enter insert a newline (multiline compose)
    /// in the dispatch input rather than dispatching — "send + open"
    /// moved to Ctrl+S so both Enter-modifier chords are free for
    /// newlines (matching the agent prompt).
    #[test]
    fn alt_and_shift_enter_insert_newline_not_dispatch() {
        let reg = crate::actions::ActionRegistry::defaults();
        for modifier in [KeyModifiers::ALT, KeyModifiers::SHIFT] {
            let mut state = make_state_with_selection();
            for ch in "hi".chars() {
                let _ =
                    state.handle_key(&KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &reg);
            }
            let outcome = state.handle_key(&KeyEvent::new(KeyCode::Enter, modifier), &reg);
            assert!(
                !matches!(outcome, InputOutcome::Action(_)),
                "{modifier:?}+Enter must not dispatch, got {outcome:?}"
            );
            assert_eq!(
                state.dispatch.text(),
                "hi\n",
                "{modifier:?}+Enter must insert a newline"
            );
        }
    }

    #[test]
    fn compose_enter_is_newline_matrix() {
        // Strict swap: (multiline, mod_enter) → is_newline
        assert!(!compose_enter_is_newline(false, false));
        assert!(compose_enter_is_newline(false, true));
        assert!(compose_enter_is_newline(true, false));
        assert!(!compose_enter_is_newline(true, true));
    }

    /// With multiline_mode on, bare Enter inserts a newline (does not
    /// dispatch) and Shift/Alt+Enter send — the agent-prompt swap.
    #[test]
    fn multiline_mode_swaps_enter_and_shift_enter() {
        use crate::app::actions::Action;
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = make_state_with_selection();
        state.multiline_mode = true;
        state.dispatch.set_text("line one");

        let bare = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            matches!(bare, InputOutcome::Changed),
            "bare Enter in multiline must insert newline, got {bare:?}"
        );
        assert!(
            state.dispatch.text().contains('\n'),
            "bare Enter must insert a newline, got {:?}",
            state.dispatch.text()
        );

        // After newline, Shift+Enter should dispatch the draft.
        let shift = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &reg);
        match shift {
            InputOutcome::Action(Action::DashboardDispatch {
                text,
                attach: false,
            }) => {
                assert!(text.contains("line one"), "dispatch text: {text:?}");
            }
            other => panic!("Shift+Enter in multiline must dispatch, got {other:?}"),
        }
    }

    /// Multiline empty bare Enter inserts a newline (strict swap); Shift+Enter
    /// open/create/attach.
    #[test]
    fn multiline_mode_empty_bare_enter_is_newline() {
        use crate::app::actions::Action;
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = make_state_with_selection();
        state.multiline_mode = true;
        state.dispatch.set_text("");
        let bare = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            matches!(bare, InputOutcome::Changed),
            "empty bare Enter in multiline must insert newline, got {bare:?}"
        );
        assert!(
            state.dispatch.text().contains('\n'),
            "got {:?}",
            state.dispatch.text()
        );

        state.dispatch.set_text("");
        match state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &reg) {
            InputOutcome::Action(Action::DashboardAttach(id)) => {
                assert_eq!(id, DashboardRowId::TopLevel(AgentId(0)));
            }
            other => panic!("empty Shift+Enter in multiline must attach, got {other:?}"),
        }
    }

    /// Ctrl+M toggles multiline via SetMultilineMode (same chord as agent).
    #[test]
    fn ctrl_m_toggles_multiline_mode() {
        use crate::app::actions::Action;
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = DashboardState::new();
        assert!(!state.multiline_mode);
        let outcome = state.handle_key(
            &KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
            &reg,
        );
        match outcome {
            InputOutcome::Action(Action::SetMultilineMode(true)) => {}
            other => panic!("Ctrl+M must emit SetMultilineMode(true), got {other:?}"),
        }
        state.multiline_mode = true;
        let outcome = state.handle_key(
            &KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
            &reg,
        );
        match outcome {
            InputOutcome::Action(Action::SetMultilineMode(false)) => {}
            other => panic!("Ctrl+M when on must emit SetMultilineMode(false), got {other:?}"),
        }
    }

    /// Bare `?` opens shortcuts when the draft is empty (input-focused) or
    /// the list is focused; types into a non-empty draft.
    #[test]
    fn question_mark_honor_gate_matches_empty_or_list_focus() {
        let reg = crate::actions::ActionRegistry::defaults();
        let question = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);

        // Default: input-focused, empty → help.
        let mut state = DashboardState::new();
        assert!(!state.list_focused);
        assert!(matches!(
            state.handle_key(&question, &reg),
            InputOutcome::Action(Action::DashboardOpenShortcutsHelp)
        ));
        assert!(state.dispatch.text().is_empty());

        // Non-empty draft → type.
        state.dispatch.set_text("hello");
        let _ = state.handle_key(&question, &reg);
        assert!(
            state.dispatch.text().contains('?'),
            "non-empty draft: `?` must type, got {:?}",
            state.dispatch.text()
        );

        // List-focused with leftover draft → help, draft untouched.
        state.list_focused = true;
        state.dispatch.set_text("leftover");
        assert!(matches!(
            state.handle_key(&question, &reg),
            InputOutcome::Action(Action::DashboardOpenShortcutsHelp)
        ));
        assert_eq!(state.dispatch.text(), "leftover");

        // Empty peek reply → help; non-empty peek reply → type.
        let mut peek = state_with_open_peek();
        assert!(matches!(
            peek.handle_key(&question, &reg),
            InputOutcome::Action(Action::DashboardOpenShortcutsHelp)
        ));
        assert!(peek.peek_reply.text().is_empty());
        peek.peek_reply.set_text("draft");
        let _ = peek.handle_key(&question, &reg);
        assert!(
            peek.peek_reply.text().contains('?'),
            "non-empty peek reply: `?` must type, got {:?}",
            peek.peek_reply.text()
        );
    }

    /// vim-mode OFF — `j`/`k` type into the dispatch input (they are
    /// NOT hijacked as row navigation). Mirrors the agent scrollback.
    #[test]
    fn vim_off_jk_type_into_input() {
        crate::appearance::cache::set_vim_mode(false);
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = DashboardState::new();
        for ch in ['j', 'k'] {
            let _ = state.handle_key(&KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &reg);
        }
        assert_eq!(
            state.dispatch.text(),
            "jk",
            "vim-off j/k must type into the input, not navigate"
        );
    }

    /// vim-mode ON + the overview list focused (via Tab) — `j`/`k`
    /// navigate the row list. In the input focus they type (covered by
    /// `vim_on_jk_type_into_input_when_focused`).
    #[test]
    fn vim_on_jk_navigate_when_list_focused() {
        crate::appearance::cache::set_vim_mode(true);
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = DashboardState::new();
        state.list_focused = true;
        let j = state.handle_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &reg);
        assert!(
            matches!(j, InputOutcome::Action(Action::DashboardSelectNext)),
            "vim j must select the next row, got {j:?}"
        );
        let k = state.handle_key(&KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), &reg);
        assert!(
            matches!(k, InputOutcome::Action(Action::DashboardSelectPrev)),
            "vim k must select the previous row, got {k:?}"
        );
        crate::appearance::cache::set_vim_mode(false);
    }

    /// vim-mode ON but the INPUT focused (the default) — `j`/`k` type
    /// into the dispatch prompt; navigation requires Tab to the overview
    /// first. This is the "distinct focus areas" contract.
    #[test]
    fn vim_on_jk_type_into_input_when_focused() {
        crate::appearance::cache::set_vim_mode(true);
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = DashboardState::new();
        assert!(!state.list_focused, "input focused by default");
        for ch in ['j', 'k'] {
            let _ = state.handle_key(&KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &reg);
        }
        assert_eq!(
            state.dispatch.text(),
            "jk",
            "input-focused vim j/k must type, not navigate"
        );
        crate::appearance::cache::set_vim_mode(false);
    }

    /// Tab toggles the two-focus model: input bar ↔ overview list.
    #[test]
    fn tab_toggles_input_and_list_focus() {
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = make_state_with_selection();
        assert!(!state.list_focused, "input focused by default");
        let a = state.handle_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &reg);
        assert!(matches!(a, InputOutcome::Changed));
        assert!(state.list_focused, "Tab focuses the overview list");
        let b = state.handle_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &reg);
        assert!(matches!(b, InputOutcome::Changed));
        assert!(!state.list_focused, "Tab again returns focus to the input");
    }

    /// Shift+Tab emits `DashboardCycleMode` regardless of how the terminal
    /// encodes it — `BackTab` (with or without a SHIFT modifier) or
    /// `Tab`+SHIFT. Guards the regression where the registry's exact-modifier
    /// `key!(BackTab)` lookup silently failed on `BackTab`+SHIFT.
    #[test]
    fn shift_tab_emits_cycle_mode_for_all_encodings() {
        let reg = crate::actions::ActionRegistry::defaults();
        for key in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            let mut state = DashboardState::new();
            let outcome = state.handle_key(&key, &reg);
            assert!(
                matches!(outcome, InputOutcome::Action(Action::DashboardCycleMode)),
                "Shift+Tab ({key:?}) must emit DashboardCycleMode, got {outcome:?}",
            );
        }
    }

    /// Multiline must not treat Shift+Tab as the submit chord (is_mod_enter
    /// requires KeyCode::Enter).
    #[test]
    fn multiline_shift_tab_cycles_mode_with_non_empty_draft() {
        let reg = crate::actions::ActionRegistry::defaults();
        for key in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            let mut state = DashboardState::new();
            state.multiline_mode = true;
            state.dispatch.set_text("draft text");
            let outcome = state.handle_key(&key, &reg);
            assert!(
                matches!(outcome, InputOutcome::Action(Action::DashboardCycleMode)),
                "multiline + {key:?} must DashboardCycleMode, not send, got {outcome:?}",
            );
            assert_eq!(
                state.dispatch.text(),
                "draft text",
                "draft must not be consumed by Shift+Tab"
            );
        }
    }

    /// Shift+↑/↓ emits the reorder actions even with the peek open (the
    /// default state when a row is selected) and regardless of focus. Guards
    /// the reorder keybinding end-to-end through `handle_key`.
    #[test]
    fn shift_arrows_emit_reorder_with_peek_open() {
        let reg = crate::actions::ActionRegistry::defaults();
        for (code, expected) in [
            (KeyCode::Up, Action::DashboardReorderUp),
            (KeyCode::Down, Action::DashboardReorderDown),
        ] {
            let mut state = make_state_with_selection();
            // Peek is shown by default when a row is selected.
            state.peek = Some(super::super::peek::PeekPanelState::new(
                DashboardRowId::TopLevel(AgentId(0)),
                peek_fields_for_test("Idle"),
            ));
            let outcome = state.handle_key(&KeyEvent::new(code, KeyModifiers::SHIFT), &reg);
            assert!(
                matches!(&outcome, InputOutcome::Action(a) if std::mem::discriminant(a) == std::mem::discriminant(&expected)),
                "Shift+{code:?} must emit {expected:?}, got {outcome:?}",
            );
        }
    }

    /// Shift+Tab cycles the PEEKED agent's live mode while the peek is
    /// open (emitting `DashboardPeekCycleMode`), but the new-session
    /// staged mode (`DashboardCycleMode`) when no peek is shown.
    #[test]
    fn shift_tab_cycles_peeked_agent_mode_when_peek_open() {
        let reg = crate::actions::ActionRegistry::defaults();
        for key in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            let mut state = make_state_with_selection();
            state.peek = Some(super::super::peek::PeekPanelState::new(
                DashboardRowId::TopLevel(AgentId(0)),
                peek_fields_for_test("Idle"),
            ));
            let outcome = state.handle_key(&key, &reg);
            assert!(
                matches!(
                    outcome,
                    InputOutcome::Action(Action::DashboardPeekCycleMode)
                ),
                "Shift+Tab ({key:?}) with peek open must emit DashboardPeekCycleMode, got {outcome:?}",
            );
        }
        // No peek → still the new-session staged-mode cycle.
        let mut state = DashboardState::new();
        let outcome = state.handle_key(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE), &reg);
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardCycleMode)),
            "Shift+Tab without peek must emit DashboardCycleMode, got {outcome:?}",
        );
    }

    /// Overview focused: Enter opens the focused row; Esc backs out of
    /// the selection (focuses `[+ New Agent]`) and STAYS on the list —
    /// it no longer returns to the input (Tab / `i` do that now).
    #[test]
    fn list_focus_enter_opens_and_esc_backs_out() {
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = make_state_with_selection();
        state.list_focused = true;
        let enter = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            matches!(enter, InputOutcome::Action(Action::DashboardAttach(_))),
            "Enter on the focused overview must open the selected row, got {enter:?}"
        );
        let esc = state.handle_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &reg);
        assert!(matches!(esc, InputOutcome::Changed));
        assert!(
            state.list_focused,
            "Esc stays on the list; Tab / i return focus to the input"
        );
        assert!(state.selected.is_none(), "Esc backs out of the selection");
        assert!(state.new_agent_button_focused);
    }

    /// Regression for the Esc-blur draft-loss path: with the list focused
    /// (e.g. after Esc unfocuses the input) on the `[+ New Agent]`
    /// button, Enter must SEND a typed draft rather than create an empty
    /// session and silently drop it. An empty draft still
    /// creates-with-detail.
    #[test]
    fn list_focus_enter_on_button_sends_draft_else_creates() {
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        // Draft present → dispatch it (no loss), staying on the dashboard.
        let mut state = DashboardState::new();
        assert!(state.new_agent_button_focused);
        state.dispatch.set_text("fix the bug");
        state.list_focused = true; // e.g. after an Esc blur
        match state.handle_key(&key, &reg) {
            InputOutcome::Action(Action::DashboardDispatch { text, attach }) => {
                assert_eq!(text, "fix the bug");
                assert!(!attach, "Enter sends + stays on the dashboard");
            }
            other => panic!("Enter with a draft must dispatch it, got {other:?}"),
        }

        // Empty draft → create-with-detail (unchanged behavior).
        let mut empty = DashboardState::new();
        empty.list_focused = true;
        assert!(matches!(
            empty.handle_key(&key, &reg),
            InputOutcome::Action(Action::DashboardCreateNewAgentWithDetail)
        ));
    }

    /// Overview focused: a non-nav printable key hands focus back to the
    /// input. In vim mode `i` enters the input without typing the `i`.
    #[test]
    fn vim_i_returns_focus_to_input_without_typing() {
        crate::appearance::cache::set_vim_mode(true);
        let reg = crate::actions::ActionRegistry::defaults();
        let mut state = make_state_with_selection();
        state.list_focused = true;
        let outcome =
            state.handle_key(&KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(!state.list_focused, "i focuses the input");
        assert!(
            state.dispatch.text().is_empty(),
            "i must NOT be typed, got {:?}",
            state.dispatch.text()
        );
        crate::appearance::cache::set_vim_mode(false);
    }

    /// A multi-line bracketed paste keeps its full raw text (what gets
    /// dispatched) while collapsing to a single chip element in the
    /// textarea (rendered folded as `[Pasted: N lines]`).
    #[test]
    fn multiline_paste_folds_into_dispatch_input() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let pasted = "line one\nline two\nline three\nline four";
        let _ = state.handle_input(&Event::Paste(pasted.to_string()), &reg);
        assert_eq!(state.dispatch.text(), pasted, "raw paste text is preserved");
        assert_eq!(
            state.dispatch.textarea.elements().len(),
            1,
            "multi-line paste must collapse to one chip element"
        );
    }

    /// Enter with the caret on a paste chip expands it instead of
    /// dispatching (agent prompt parity).
    #[test]
    fn enter_on_dispatch_paste_chip_expands() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let pasted = "line one\nline two\nline three\nline four";
        let _ = state.handle_input(&Event::Paste(pasted.to_string()), &reg);
        state.dispatch.set_cursor(0);
        let outcome = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Enter on chip must expand, got {outcome:?}"
        );
        assert!(
            state.dispatch.textarea.elements().is_empty(),
            "chip must be inlined"
        );
        assert_eq!(state.dispatch.text(), pasted);
    }

    /// Enter with the caret right after a paste chip still dispatches
    /// (preview shows there; expand is on-chip only).
    #[test]
    fn enter_after_dispatch_paste_chip_dispatches() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let pasted = "line one\nline two\nline three\nline four";
        let _ = state.handle_input(&Event::Paste(pasted.to_string()), &reg);
        // handle_paste leaves the cursor after the chip.
        match state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg) {
            InputOutcome::Action(Action::DashboardDispatch { text, .. }) => {
                assert_eq!(text, pasted);
            }
            other => panic!("Enter after chip must dispatch, got {other:?}"),
        }
    }

    /// Peek reply: Enter on a paste chip expands rather than sending.
    #[test]
    fn enter_on_peek_reply_paste_chip_expands() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        state.peek.as_mut().unwrap().focused = true;
        let pasted = "a\nb\nc";
        // Peek reply is compact → 2-line threshold; 3 lines still chips.
        let _ = state.handle_input(&Event::Paste(pasted.to_string()), &reg);
        assert_eq!(state.peek_reply.textarea.elements().len(), 1);
        state.peek_reply.set_cursor(0);
        let outcome = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Enter on peek chip must expand, got {outcome:?}"
        );
        assert!(state.peek_reply.textarea.elements().is_empty());
        assert_eq!(state.peek_reply.text(), pasted);
    }

    /// Multiline peek still expands paste chips before treating bare Enter
    /// as a newline (dispatch + agent order).
    #[test]
    fn multiline_peek_enter_on_paste_chip_expands() {
        let mut state = state_with_open_peek();
        state.multiline_mode = true;
        let reg = crate::actions::ActionRegistry::defaults();
        state.peek.as_mut().unwrap().focused = true;
        let pasted = "a\nb\nc";
        let _ = state.handle_input(&Event::Paste(pasted.to_string()), &reg);
        assert_eq!(state.peek_reply.textarea.elements().len(), 1);
        state.peek_reply.set_cursor(0);
        let outcome = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "multiline Enter on peek chip must expand, got {outcome:?}"
        );
        assert!(
            state.peek_reply.textarea.elements().is_empty(),
            "chip must be inlined, not left as an element"
        );
        assert_eq!(state.peek_reply.text(), pasted);
        assert!(
            !state.peek_reply.text().contains("\n\n"),
            "must expand, not insert an extra newline: {:?}",
            state.peek_reply.text()
        );
    }

    /// Enter on an image chip still dispatches — dashboard has no image
    /// viewer, so ImagePreview must not swallow the key.
    #[test]
    fn enter_on_dispatch_image_chip_dispatches() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let _ = state.attach_pasted_image(peek_test_image());
        state.dispatch.set_cursor(0);
        match state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg) {
            InputOutcome::Action(Action::DashboardDispatch { text, .. }) => {
                assert!(text.contains("[Image #1]"), "got {text:?}");
            }
            other => panic!("Enter on image chip must dispatch, got {other:?}"),
        }
    }

    #[test]
    fn set_peek_clears_prompt_click_timer() {
        let mut state = DashboardState::new();
        state.last_prompt_click = Some(Instant::now());
        state.set_peek(Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            peek_fields_for_test("Idle"),
        )));
        assert!(
            state.last_prompt_click.is_none(),
            "opening peek must clear the shared double-click timer"
        );
        state.last_prompt_click = Some(Instant::now());
        state.set_peek(None);
        assert!(
            state.last_prompt_click.is_none(),
            "closing peek must clear the shared double-click timer"
        );
    }

    #[test]
    fn peek_row_change_clears_prompt_click_timer() {
        let mut state = state_with_open_peek();
        state.last_prompt_click = Some(Instant::now());
        state.clear_peek_reply();
        assert!(
            state.last_prompt_click.is_none(),
            "row change / clear_peek_reply must clear double-click timer"
        );
    }

    #[test]
    fn enter_on_reject_feedback_paste_chip_expands() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let mut f = peek_fields_for_test("Awaiting your input");
        f.question = Some("Allow Edit?".into());
        f.options = vec![
            ("allow".into(), "Allow".into()),
            ("no".into(), "No, reject".into()),
        ];
        f.reject_option = Some(1);
        f.request_id = Some(7);
        state.set_peek(Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            f,
        )));
        if let Some(p) = state.peek.as_mut() {
            p.focused = true;
            p.selected_option = Some(1);
        }
        let pasted = "reason line one\nreason line two\nreason line three";
        let _ = state.handle_input(&Event::Paste(pasted.to_string()), &reg);
        assert_eq!(state.peek_reply.textarea.elements().len(), 1);
        state.peek_reply.set_cursor(0);
        let outcome = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Enter on reject freeform paste chip must expand, got {outcome:?}"
        );
        assert!(state.peek_reply.textarea.elements().is_empty());
        assert_eq!(state.peek_reply.text(), pasted);
    }

    #[test]
    fn wrap_host_image_none_paste_not_inserted_as_text() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let outcome = state.handle_input(
            &Event::Paste(crate::wrap_clipboard_image::MAGIC_NONE.to_string()),
            &reg,
        );
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert!(state.dispatch.text().is_empty());
    }

    #[test]
    fn wrap_host_image_none_paste_not_inserted_into_peek_reply() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.focus_row(DashboardRowId::TopLevel(AgentId(0)));
        state.set_peek(Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            peek_fields_for_test("Idle"),
        )));
        let outcome = state.handle_input(
            &Event::Paste(crate::wrap_clipboard_image::MAGIC_NONE.to_string()),
            &reg,
        );
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert!(state.peek_reply.text().is_empty());
        assert!(state.dispatch.text().is_empty());
    }

    /// Wrap host-image bracketed paste with peek open must attach to
    /// `peek_reply`, not the hidden new-session dispatch input.
    #[test]
    fn wrap_host_image_paste_with_peek_open_goes_to_reply_not_dispatch() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        let png = test_png_bytes();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);
        let paste = format!(
            "{}\nimage/png\n{b64}",
            crate::wrap_clipboard_image::MAGIC_IMG
        );
        let outcome = state.handle_input(&Event::Paste(paste), &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        let text = state.peek_reply.text();
        assert!(text.contains("[Image #1]"), "got {text:?}");
        assert_eq!(state.peek_reply.images.len(), 1);
        assert!(
            state.dispatch.images.is_empty() && state.dispatch.text().is_empty(),
            "wrap image must not leak into hidden dispatch"
        );
        assert!(
            state.peek.as_ref().unwrap().focused,
            "wrap image paste must focus the reply"
        );
    }

    /// Question mode is text-only on the wire — wrap host-image must not
    /// attach to peek reply (or leak into dispatch).
    #[test]
    fn wrap_host_image_paste_question_mode_blocks_attach() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        if let Some(p) = state.peek.as_mut() {
            p.focused = true;
            p.question = Some("Allow?".into());
            p.options = vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())];
            p.reject_option = Some(1);
            p.selected_option = Some(1);
        }
        let png = test_png_bytes();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);
        let paste = format!(
            "{}\nimage/png\n{b64}",
            crate::wrap_clipboard_image::MAGIC_IMG
        );
        let outcome = state.handle_input(&Event::Paste(paste), &reg);
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert!(state.peek_reply.images.is_empty());
        assert!(!state.peek_reply.text().contains("[Image #"));
        assert!(state.dispatch.images.is_empty() && state.dispatch.text().is_empty());
    }

    /// A bracketed paste while the peek panel is open lands in the
    /// peek's `❯ reply` widget — NOT the hidden new-session dispatch
    /// input. (Regression: terminals with bracketed paste deliver
    /// Cmd/Ctrl+V as `Event::Paste`, which used to fall through to
    /// the dispatch arm and silently fill the box behind the
    /// panel.) A multi-line paste folds into a single `[Pasted: N
    /// lines]` chip (the reply widget is compact → 2-line threshold)
    /// while preserving the raw text for the eventual send, and pasting
    /// focuses the input like the Ctrl/Cmd+V chord does.
    #[test]
    fn bracketed_paste_with_peek_open_goes_to_reply() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.focus_row(DashboardRowId::TopLevel(AgentId(0)));
        state.set_peek(Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            peek_fields_for_test("Idle"),
        )));
        // Unfocused (Tab → row nav) — paste must still target the reply
        // and re-focus it ("pasting implies an intent to reply").
        state.peek.as_mut().unwrap().focused = false;
        let outcome = state.handle_input(&Event::Paste("hello\nworld".to_string()), &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(
            state.peek_reply.text(),
            "hello\nworld",
            "paste must land in the reply with its raw text preserved",
        );
        assert_eq!(
            state.peek_reply.textarea.elements().len(),
            1,
            "a 2-line paste must fold into one [Pasted: N lines] chip (compact threshold)",
        );
        assert!(
            state.peek.as_ref().unwrap().focused,
            "paste must focus the reply input"
        );
        assert!(
            state.dispatch.text().is_empty(),
            "paste must NOT leak into the hidden dispatch input, got {:?}",
            state.dispatch.text(),
        );
    }

    /// A pasted image attaches as a clean `[Image #N]` chip with no
    /// embedded source path (the path would blow out the single-line
    /// dispatch box and the dispatched session's scrollback).
    #[test]
    fn pasted_image_chip_omits_full_path() {
        let mut state = DashboardState::new();
        let pasted = crate::prompt_images::PastedImage {
            element_id: xai_ratatui_textarea::ElementId::from_raw(0),
            display_number: 0,
            mime_type: "image/png".into(),
            dimensions: Some((10, 10)),
            byte_len: 16,
            encoded_bytes: Some(vec![0u8; 16].into()),
            source_path: Some(std::path::PathBuf::from(
                "/Users/somebody/very/long/path/screenshot.png",
            )),
            staged_temp_path: None,
            session_image_path: None,
            preview: crate::prompt_images::PromptImagePreview::default(),
        };
        let _ = state.attach_pasted_image(pasted);
        let text = state.dispatch.text();
        assert!(
            text.contains("[Image #1]"),
            "expected a clean chip, got {text:?}"
        );
        assert!(
            !text.contains("screenshot.png"),
            "chip must not embed the source path, got {text:?}"
        );
        assert_eq!(
            state.dispatch.images[0].source_path.as_deref(),
            Some(std::path::Path::new(
                "/Users/somebody/very/long/path/screenshot.png"
            ))
        );
    }

    // -----------------------------------------------------------------
    // The clipboard raster/file-url probe (osascript) + image decode +
    // session persist run OFF the event loop. A paste that would probe
    // enqueues a `ProbeClipboardAttachment` effect and returns without an
    // inline probe (`clipboard_probe_call_count() == 0`); the chip
    // attaches later via `complete_clipboard_attachment_paste`. Snapshot /
    // support are faked via the test-only seam; plain text with no
    // raster stays fully synchronous (no defer).
    // -----------------------------------------------------------------

    fn probe_image_data() -> crate::clipboard::ImageData {
        crate::clipboard::ImageData {
            data: test_png_bytes(),
            mime_type: "image/png".into(),
        }
    }

    fn ctrl_v_event() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL))
    }

    /// Target of the enqueued deferred-probe effect, if any.
    fn deferred_probe_target(
        state: &DashboardState,
    ) -> Option<crate::app::actions::ClipboardPasteTarget> {
        deferred_probe_ctx(state).map(|ctx| ctx.target)
    }

    /// The `ClipboardPasteContext` of the enqueued deferred-probe effect, if any.
    fn deferred_probe_ctx(
        state: &DashboardState,
    ) -> Option<crate::app::actions::ClipboardPasteContext> {
        state.pending_effects.iter().find_map(|e| match e {
            crate::app::actions::Effect::ProbeClipboardAttachment { ctx, .. } => Some(ctx.clone()),
            _ => None,
        })
    }

    /// A ready-to-insert (already decoded) pasted image for completion tests.
    fn completion_pasted_image() -> crate::prompt_images::PastedImage {
        crate::prompt_images::from_clipboard_data(&probe_image_data())
    }

    /// A `ClipboardPasteContext` for driving `complete_clipboard_attachment_paste`
    /// directly (image-wins Cmd+V: carries the caption, inserts on a no-image
    /// miss). The peek target stamps the row `state_with_open_peek` peeks.
    fn completion_ctx(
        clipboard_text: Option<&str>,
        peek: bool,
    ) -> crate::app::actions::ClipboardPasteContext {
        crate::app::actions::ClipboardPasteContext {
            target: if peek {
                crate::app::actions::ClipboardPasteTarget::DashboardPeek {
                    row: DashboardRowId::TopLevel(AgentId(0)),
                }
            } else {
                crate::app::actions::ClipboardPasteTarget::DashboardDispatch
            },
            source: crate::app::actions::ClipboardPasteSource::ClipboardKey {
                text: crate::app::actions::ClipboardTextRead::Success(
                    clipboard_text.map(str::to_owned),
                ),
                tip_showing: false,
            },
        }
    }

    /// Drive a real Cmd+V that finds a raster (defers), then complete the probe
    /// with a decoded image — the full shipped deferred image-paste path. The
    /// caller sets `state` up so `handle_input` routes to the intended surface
    /// (peek open → reply; peek closed → dispatch).
    fn cmd_v_image(state: &mut DashboardState, clipboard_text: Option<&str>) {
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(crate::clipboard::ClipboardProbeHook {
            text: clipboard_text.map(str::to_owned),
            ..crate::clipboard::ClipboardProbeHook::with_raster(None)
        });
        let _ = state.handle_input(&ctrl_v_event(), &reg);
        let ctx = deferred_probe_ctx(state).expect("an image paste must defer a probe");
        crate::clipboard::clear_clipboard_probe_hook();
        state.complete_clipboard_attachment_paste(
            ctx,
            crate::app::actions::ProbedAttachment::Image(completion_pasted_image()),
            None,
        );
    }

    #[test]
    fn dispatch_bracketed_image_paste_defers_probe() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        // Empty bracketed paste + a raster on the pasteboard.
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::with_raster(None),
        );
        let _ = state.handle_input(&Event::Paste(String::new()), &reg);
        let calls = crate::clipboard::clipboard_probe_call_count();
        let target = deferred_probe_target(&state);
        crate::clipboard::clear_clipboard_probe_hook();

        assert_eq!(calls, 0, "probe must NOT run inline on the event loop");
        assert!(
            matches!(
                target,
                Some(crate::app::actions::ClipboardPasteTarget::DashboardDispatch)
            ),
            "a dispatch-targeted probe effect must be enqueued"
        );
        assert!(
            state.dispatch.images.is_empty(),
            "chip attaches on completion, not inline"
        );
    }

    /// Regression: an IME commit delivered as bracketed paste (Otty)
    /// must not attach the unrelated clipboard image.
    #[test]
    fn dispatch_bracketed_paste_stamps_ctx_bracketed() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::with_raster(None),
        );
        let _ = state.handle_input(&Event::Paste("中".to_owned()), &reg);
        let ctx = deferred_probe_ctx(&state);
        crate::clipboard::clear_clipboard_probe_hook();

        let ctx = ctx.expect("a bracketed paste with a raster must defer a probe");
        assert!(
            ctx.source.is_bracketed(),
            "bracketed source must let the probe verify payload origin"
        );
        assert_eq!(ctx.source.text(), Some("中"));
    }

    #[test]
    fn dispatch_cmd_v_probe_ctx_not_bracketed() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(crate::clipboard::ClipboardProbeHook {
            text: Some("caption".to_owned()),
            ..crate::clipboard::ClipboardProbeHook::with_raster(None)
        });
        let _ = state.handle_input(&ctrl_v_event(), &reg);
        let ctx = deferred_probe_ctx(&state);
        crate::clipboard::clear_clipboard_probe_hook();

        let ctx = ctx.expect("a Cmd+V with a raster must defer a probe");
        assert!(
            !ctx.source.is_bracketed(),
            "Cmd+V source must remain a CLIPBOARD-key read"
        );
    }

    /// Bracketed caption + raster: image wins across the deferral boundary — the
    /// caption is NOT inserted synchronously (it is carried into the effect and
    /// dropped when the probe returns an image), so the dashboard bracketed path
    /// attaches exactly one thing: the image, never image + caption.
    #[test]
    fn dispatch_bracketed_caption_image_wins_no_double_insert() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::with_raster(None),
        );
        let _ = state.handle_input(&Event::Paste("a caption".to_string()), &reg);
        let ctx = deferred_probe_ctx(&state);
        crate::clipboard::clear_clipboard_probe_hook();

        let ctx = ctx.expect("caption + raster must defer a probe");
        assert_eq!(
            state.dispatch.text(),
            "",
            "caption must not be inserted synchronously (image wins)"
        );
        assert_eq!(ctx.source.text(), Some("a caption"));
        assert!(matches!(
            ctx.source,
            crate::app::actions::ClipboardPasteSource::BracketedDeferred { .. }
        ));

        // Probe finds the image → image wins, caption dropped (no double insert).
        state.complete_clipboard_attachment_paste(
            ctx,
            crate::app::actions::ProbedAttachment::Image(completion_pasted_image()),
            None,
        );
        assert_eq!(state.dispatch.images.len(), 1);
        assert!(state.dispatch.text().contains("[Image #1]"));
        assert!(!state.dispatch.text().contains("a caption"));
    }

    #[test]
    fn dispatch_paste_key_image_defers_probe() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::with_raster(None),
        );
        let _ = state.handle_input(&ctrl_v_event(), &reg);
        let calls = crate::clipboard::clipboard_probe_call_count();
        let target = deferred_probe_target(&state);
        crate::clipboard::clear_clipboard_probe_hook();

        assert_eq!(calls, 0, "probe must NOT run inline on the event loop");
        assert!(matches!(
            target,
            Some(crate::app::actions::ClipboardPasteTarget::DashboardDispatch)
        ));
        assert!(state.dispatch.images.is_empty());
    }

    #[test]
    fn peek_bracketed_image_paste_defers_probe() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::with_raster(None),
        );
        let _ = state.handle_input(&Event::Paste(String::new()), &reg);
        let calls = crate::clipboard::clipboard_probe_call_count();
        let target = deferred_probe_target(&state);
        crate::clipboard::clear_clipboard_probe_hook();

        assert_eq!(calls, 0, "probe must NOT run inline on the event loop");
        assert!(
            matches!(
                target,
                Some(crate::app::actions::ClipboardPasteTarget::DashboardPeek { .. })
            ),
            "a peek-targeted probe effect must be enqueued"
        );
        assert!(state.peek_reply.images.is_empty());
    }

    #[test]
    fn peek_paste_key_image_defers_probe() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::with_raster(None),
        );
        let _ = state.handle_input(&ctrl_v_event(), &reg);
        let calls = crate::clipboard::clipboard_probe_call_count();
        let target = deferred_probe_target(&state);
        crate::clipboard::clear_clipboard_probe_hook();

        assert_eq!(calls, 0, "probe must NOT run inline on the event loop");
        assert!(matches!(
            target,
            Some(crate::app::actions::ClipboardPasteTarget::DashboardPeek { .. })
        ));
        assert!(state.peek_reply.images.is_empty());
    }

    #[test]
    fn dispatch_text_paste_no_raster_stays_synchronous() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        // Non-empty non-URL text with no raster: the snapshot gate skips the
        // probe entirely, so nothing is deferred and the text inserts inline.
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::no_raster(None),
        );
        let outcome = state.handle_input(&Event::Paste("hello world".to_string()), &reg);
        let calls = crate::clipboard::clipboard_probe_call_count();
        let deferred = deferred_probe_target(&state).is_some();
        crate::clipboard::clear_clipboard_probe_hook();

        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(calls, 0, "no inline probe");
        assert!(
            !deferred,
            "plain text with no raster must not defer a probe"
        );
        assert_eq!(
            state.dispatch.text(),
            "hello world",
            "text inserted synchronously"
        );
    }

    #[test]
    fn dispatch_path_paste_attaches_inline_without_deferring() {
        let dir = tempfile::tempdir().unwrap();
        let png = write_test_png(dir.path());
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        // Even with a raster snapshot, a pasted file path resolves inline and
        // must NOT also enqueue a probe (no double insert).
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::with_raster(None),
        );
        let outcome = state.handle_input(&Event::Paste(png.display().to_string()), &reg);
        let calls = crate::clipboard::clipboard_probe_call_count();
        let deferred = deferred_probe_target(&state).is_some();
        crate::clipboard::clear_clipboard_probe_hook();

        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(calls, 0, "no inline probe");
        assert!(
            !deferred,
            "a resolved path paste must not also defer a probe"
        );
        assert_eq!(
            state.dispatch.images.len(),
            1,
            "path attached as a chip inline"
        );
        assert!(state.dispatch.text().contains("[Image #1]"));
    }

    #[test]
    fn completion_attaches_image_to_dispatch() {
        let mut state = DashboardState::new();
        let completion = state.complete_clipboard_attachment_paste(
            completion_ctx(Some("caption"), false),
            crate::app::actions::ProbedAttachment::Image(completion_pasted_image()),
            None,
        );
        assert_eq!(
            completion,
            crate::app::actions::ClipboardPasteCompletion::Handled
        );
        assert_eq!(state.dispatch.images.len(), 1);
        assert!(state.dispatch.text().contains("[Image #1]"));
        // Image wins: the carried caption is NOT also inserted (no double).
        assert!(!state.dispatch.text().contains("caption"));
    }

    #[test]
    fn completion_attaches_image_to_peek() {
        let mut state = state_with_open_peek();
        state.complete_clipboard_attachment_paste(
            completion_ctx(None, true),
            crate::app::actions::ProbedAttachment::Image(completion_pasted_image()),
            None,
        );
        assert_eq!(state.peek_reply.images.len(), 1);
        assert!(state.peek_reply.text().contains("[Image #1]"));
        assert!(
            state.dispatch.images.is_empty(),
            "peek completion must not leak into dispatch"
        );
    }

    /// A no-image miss on an image-wins dispatch Cmd+V inserts the carried
    /// caption instead — deferring the probe must not lose a text-only paste.
    #[test]
    fn completion_inserts_caption_on_no_image_miss() {
        let mut state = DashboardState::new();
        let completion = state.complete_clipboard_attachment_paste(
            completion_ctx(Some("just some text"), false),
            crate::app::actions::ProbedAttachment::NoRaster,
            None,
        );
        assert_eq!(
            completion,
            crate::app::actions::ClipboardPasteCompletion::Handled
        );
        assert!(state.dispatch.images.is_empty());
        assert_eq!(state.dispatch.text(), "just some text");
    }

    #[test]
    fn dashboard_deferred_bracketed_text_survives_failed_or_dropped_probe() {
        use crate::app::actions::{
            ClipboardPasteCompletion, ClipboardPasteFailure, ProbedAttachment,
        };
        for (probe, expected) in [
            (
                ProbedAttachment::ProbeFailed,
                ClipboardPasteCompletion::Failed(ClipboardPasteFailure::AttachmentRead),
            ),
            (
                ProbedAttachment::ProbeDropped,
                ClipboardPasteCompletion::Dropped,
            ),
        ] {
            let mut state = DashboardState::new();
            let mut ctx = completion_ctx(None, false);
            ctx.source = crate::app::actions::ClipboardPasteSource::BracketedDeferred {
                text: "bracketed text".to_owned(),
            };

            let completion = state.complete_clipboard_attachment_paste(ctx, probe, None);

            assert_eq!(completion, expected);
            assert_eq!(state.dispatch.text(), "bracketed text");
        }
    }

    /// A no-image miss on a peek Cmd+V must NOT buffer the caption into the
    /// hidden reply if the peeked agent raised a question during the probe window
    /// (the reply is text-only on the wire in question mode).
    #[test]
    fn completion_peek_caption_dropped_in_question_mode() {
        let mut state = state_with_open_peek();
        state.paste_probe_in_flight = 1;
        if let Some(p) = state.peek.as_mut() {
            p.question = Some("Allow?".into());
        }
        let completion = state.complete_clipboard_attachment_paste(
            completion_ctx(Some("some caption"), true),
            crate::app::actions::ProbedAttachment::NoRaster,
            None,
        );
        assert_eq!(
            completion,
            crate::app::actions::ClipboardPasteCompletion::Dropped
        );
        assert!(
            state.peek_reply.text().is_empty(),
            "the caption must be dropped in question mode, not buffered into the hidden reply"
        );
        assert_eq!(state.paste_probe_in_flight, 0);
    }

    #[test]
    fn completion_attaches_file_url_to_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let png = write_test_png(dir.path());
        let mut state = DashboardState::new();
        let completion = state.complete_clipboard_attachment_paste(
            completion_ctx(None, false),
            crate::app::actions::ProbedAttachment::NoRaster,
            Some(png.display().to_string()),
        );
        assert_eq!(
            completion,
            crate::app::actions::ClipboardPasteCompletion::Handled
        );
        assert_eq!(
            state.dispatch.images.len(),
            1,
            "file URL attached as a chip"
        );
        assert!(state.dispatch.text().contains("[Image #1]"));
    }

    /// A peek completion arriving after the panel closed drops the attachment
    /// instead of inserting into the now-hidden reply buffer.
    #[test]
    fn completion_peek_dropped_when_panel_closed() {
        let mut state = DashboardState::new(); // no peek open
        state.paste_probe_in_flight = 1;
        let completion = state.complete_clipboard_attachment_paste(
            completion_ctx(None, true),
            crate::app::actions::ProbedAttachment::Image(completion_pasted_image()),
            None,
        );
        assert_eq!(
            completion,
            crate::app::actions::ClipboardPasteCompletion::Dropped
        );
        assert!(
            state.peek_reply.images.is_empty(),
            "a closed peek must not receive the deferred image"
        );
        assert_eq!(
            state.paste_probe_in_flight, 0,
            "the in-flight count is still decremented so a stashed send can drain"
        );
    }

    /// A peek completion arriving after the panel moved to ANOTHER row drops
    /// the attachment (never lands in a different agent's reply), and a peek
    /// send stashed for the old row is dropped at drain time — with a toast —
    /// instead of replying to the newly peeked agent.
    #[test]
    fn completion_peek_dropped_when_row_changed() {
        let mut state = state_with_open_peek(); // peeks TopLevel(AgentId(0))
        state.paste_probe_in_flight = 1;
        state.deferred_peek_send = Some(DeferredPeekSend {
            row: DashboardRowId::TopLevel(AgentId(0)),
            attach: false,
        });
        // The user moves the peek to a different row during the probe window.
        if let Some(p) = state.peek.as_mut() {
            p.row = DashboardRowId::TopLevel(AgentId(7));
        }
        state.complete_clipboard_attachment_paste(
            completion_ctx(None, true),
            crate::app::actions::ProbedAttachment::Image(completion_pasted_image()),
            None,
        );
        assert!(
            state.peek_reply.images.is_empty(),
            "a retargeted peek must not receive the deferred image"
        );
        assert_eq!(state.paste_probe_in_flight, 0);
        let actions = state.take_deferred_sends_after_paste();
        assert!(
            actions.is_empty(),
            "the stale peek stash must not reissue to the new row: {actions:?}"
        );
        assert!(state.deferred_peek_send.is_none(), "the stash is consumed");
        assert!(
            state.error_toast.is_some(),
            "dropping the stashed reply must be announced with a toast"
        );
    }

    /// A question arriving on the peeked row mid-probe makes the reply
    /// text-only: an Image completion for that row must be discarded WITH a
    /// toast (not silently no-opped by the attach helper's question guard).
    #[test]
    fn completion_peek_image_discarded_when_question_arrives() {
        let mut state = state_with_open_peek();
        let reg = crate::actions::ActionRegistry::defaults();
        // Cmd+V in NORMAL mode → the probe defers against the peeked row.
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::with_raster(None),
        );
        let _ = state.handle_input(&ctrl_v_event(), &reg);
        let ctx = deferred_probe_ctx(&state).expect("an image paste must defer a probe");
        crate::clipboard::clear_clipboard_probe_hook();
        assert_eq!(state.paste_probe_in_flight, 1);
        // A permission question arrives on the SAME row before completion.
        if let Some(p) = state.peek.as_mut() {
            p.question = Some("Allow?".into());
        }
        let completion = state.complete_clipboard_attachment_paste(
            ctx,
            crate::app::actions::ProbedAttachment::Image(completion_pasted_image()),
            None,
        );
        assert_eq!(
            completion,
            crate::app::actions::ClipboardPasteCompletion::Dropped
        );
        assert!(
            state.peek_reply.images.is_empty(),
            "the image must not attach to a question-mode reply"
        );
        assert!(!state.peek_reply.text().contains("[Image #"));
        assert!(
            state.error_toast.is_some(),
            "discarding the deferred image must be announced with a toast"
        );
        assert_eq!(state.paste_probe_in_flight, 0);
    }

    #[test]
    fn completion_reports_full_miss_for_unreadable_file_url() {
        let mut state = DashboardState::new();
        let completion = state.complete_clipboard_attachment_paste(
            completion_ctx(None, false),
            crate::app::actions::ProbedAttachment::NoRaster,
            Some("file:///definitely/missing/xai-primary-paste.png".to_owned()),
        );

        assert_eq!(
            completion,
            crate::app::actions::ClipboardPasteCompletion::FullMiss
        );
        assert!(state.dispatch.text().is_empty());
        assert!(state.dispatch.images.is_empty());
    }

    #[test]
    fn completion_reports_failed_probe() {
        let mut state = DashboardState::new();
        let completion = state.complete_clipboard_attachment_paste(
            completion_ctx(None, false),
            crate::app::actions::ProbedAttachment::ProbeFailed,
            None,
        );

        assert_eq!(
            completion,
            crate::app::actions::ClipboardPasteCompletion::Failed(
                crate::app::actions::ClipboardPasteFailure::AttachmentRead,
            )
        );
    }

    /// Same guard for the file-url completion arm: Finder file-URL chips are
    /// attachments too and must be discarded loudly in question mode.
    #[test]
    fn completion_peek_file_urls_discarded_when_question_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let png = write_test_png(dir.path());
        let mut state = state_with_open_peek();
        state.paste_probe_in_flight = 1;
        if let Some(p) = state.peek.as_mut() {
            p.question = Some("Allow?".into());
        }
        state.complete_clipboard_attachment_paste(
            completion_ctx(None, true),
            crate::app::actions::ProbedAttachment::NoRaster,
            Some(png.display().to_string()),
        );
        assert!(
            state.peek_reply.images.is_empty(),
            "file-url chips must not attach to a question-mode reply"
        );
        assert!(
            state.error_toast.is_some(),
            "discarding the deferred file-url chips must be announced with a toast"
        );
        assert_eq!(state.paste_probe_in_flight, 0);
    }

    /// A peek send stashed in normal mode must NOT reissue once a question
    /// owns the panel — the reply dispatch would silently queue a prompt and
    /// wipe the draft behind the dialog. It is dropped with a toast and the
    /// draft stays in the widget.
    #[test]
    fn stashed_peek_reply_dropped_when_question_active() {
        let mut state = state_with_open_peek(); // peeks TopLevel(AgentId(0))
        state.peek_reply.set_text("please look");
        state.deferred_peek_send = Some(DeferredPeekSend {
            row: DashboardRowId::TopLevel(AgentId(0)),
            attach: false,
        });
        if let Some(p) = state.peek.as_mut() {
            p.question = Some("Allow?".into());
        }
        let actions = state.take_deferred_sends_after_paste();
        assert!(
            actions.is_empty(),
            "the stashed reply must not reissue into question mode: {actions:?}"
        );
        assert!(state.deferred_peek_send.is_none(), "the stash is consumed");
        assert!(
            state.error_toast.is_some(),
            "dropping the stashed reply must be announced with a toast"
        );
        assert_eq!(
            state.peek_reply.text(),
            "please look",
            "the draft stays in the widget for after the question"
        );
    }

    // -----------------------------------------------------------------
    // `/` literal + `Ctrl+/` search mode (replaces the old `/`→filter
    // behaviour that silently swallowed prompts starting with a
    // filter prefix).
    // -----------------------------------------------------------------

    /// `/` types a literal slash into the prompt — it no longer
    /// enters a filter mode. (Filtering moved to `Ctrl+/`.)
    #[test]
    fn slash_types_literal_not_filter() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let ev = Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        let _ = state.handle_input(&ev, &reg);
        assert_eq!(state.dispatch.text(), "/", "/ must type a literal slash");
        assert!(!state.search_mode, "/ must NOT enter search mode");
        assert!(
            matches!(state.filter, Filter::None),
            "/ must NOT set a filter",
        );
    }

    /// `Ctrl+/` toggles search mode on, then off.
    #[test]
    fn ctrl_slash_toggles_search_mode() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let ctrl_slash = Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL));
        let o1 = state.handle_input(&ctrl_slash, &reg);
        assert!(matches!(o1, InputOutcome::Changed));
        assert!(state.search_mode, "Ctrl+/ must enter search mode");
        let o2 = state.handle_input(&ctrl_slash, &reg);
        assert!(matches!(o2, InputOutcome::Changed));
        assert!(!state.search_mode, "Ctrl+/ again must exit search mode");
    }

    /// In search mode the dispatch buffer is a live filter query;
    /// Enter CONFIRMS (keeps the filter, leaves search, clears query).
    #[test]
    fn search_mode_typing_filters_live_and_enter_confirms() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.enter_search_mode();
        for ch in "auth".chars() {
            let ev = Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            let _ = state.handle_input(&ev, &reg);
        }
        assert_eq!(state.dispatch.text(), "auth");
        assert!(
            matches!(&state.filter, Filter::Substring(s) if s == "auth"),
            "typing in search mode must update the filter live, got {:?}",
            state.filter,
        );
        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let outcome = state.handle_input(&enter, &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(!state.search_mode, "Enter must leave search mode");
        assert!(
            matches!(&state.filter, Filter::Substring(s) if s == "auth"),
            "Enter must KEEP the filter applied",
        );
        assert!(
            state.dispatch.text().is_empty(),
            "query buffer cleared after confirm",
        );
    }

    #[test]
    fn search_mode_cursor_only_edit_redraws_without_filter_change() {
        let mut state = DashboardState::new();
        let registry = crate::actions::ActionRegistry::defaults();
        state.enter_search_mode();
        state.dispatch.set_text("auth");
        state.dispatch.set_cursor(0);
        state.filter = Filter::Substring("auth".to_owned());

        let outcome = state.handle_input(
            &Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            &registry,
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(state.dispatch.text(), "auth");
        assert_eq!(state.dispatch.cursor(), 1);
        assert!(matches!(&state.filter, Filter::Substring(text) if text == "auth"));
    }

    /// Esc in search mode CANCELS: clears the filter and exits.
    #[test]
    fn search_mode_esc_cancels_and_clears_filter() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.enter_search_mode();
        state.dispatch.set_text("auth");
        state.filter = Filter::Substring("auth".into());
        let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let outcome = state.handle_input(&esc, &reg);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(!state.search_mode, "Esc must exit search mode");
        assert!(
            matches!(state.filter, Filter::None),
            "Esc must clear the filter",
        );
        assert!(state.dispatch.text().is_empty());
    }

    /// In search mode, bare letters that are normally nav shortcuts
    /// (j/k) type into the query instead of navigating.
    #[test]
    fn search_mode_bare_letter_types_into_query() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.enter_search_mode();
        for ch in "jk".chars() {
            let ev = Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            let _ = state.handle_input(&ev, &reg);
        }
        assert_eq!(
            state.dispatch.text(),
            "jk",
            "j/k must type in search mode, not navigate",
        );
    }

    /// edge case 21: Enter on empty input attaches.
    #[test]
    fn enter_with_empty_input_attaches() {
        let state = make_state_with_selection();
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let outcome = state.clone_for_test().handle_key(&key, &reg);
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::DashboardAttach(_))
        ));
    }

    /// Single left-click on a row attaches the
    /// conversation immediately. The previous double-click-required
    /// design felt unresponsive (user explicitly reported "click
    /// does not respond or do anything properly"). Mouse handling
    /// now mirrors click-to-open list semantics from gh-dash / k9s.
    #[test]
    fn single_click_on_row_attaches_immediately() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let mut state = make_state_with_selection();
        // Populate row_rects so the click lookup finds a row.
        let row_id = state
            .selected
            .as_ref()
            .cloned()
            .expect("seed state has a selection");
        let rect = Rect {
            x: 0,
            y: 5,
            width: 80,
            height: 1,
        };
        state.row_rects = vec![(row_id.clone(), rect)];
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let outcome = state.handle_mouse(&click);
        match outcome {
            InputOutcome::Action(Action::DashboardAttach(id)) => {
                assert_eq!(id, row_id, "click must attach the clicked row, got {id:?}");
            }
            other => panic!("expected DashboardAttach on single click, got {other:?}"),
        }
        // The clicked row also becomes selected so a follow-up Esc
        // returns the cursor to where the user clicked.
        assert_eq!(state.selected.as_ref(), Some(&row_id));
    }

    /// Clicking on an empty cell (no row at that
    /// position) is a no-op. Prevents accidental attaches when the
    /// user clicks between rows or in the header area.
    #[test]
    fn click_on_empty_area_is_no_op() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut state = make_state_with_selection();
        // Empty row_rects → no row to find at the click position.
        state.row_rects = Vec::new();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let outcome = state.handle_mouse(&click);
        assert!(
            matches!(outcome, InputOutcome::Unchanged),
            "click on empty area must be a no-op, got {outcome:?}",
        );
    }

    /// Clicking a model in the dashboard `/model` slash dropdown must
    /// accept the completion and must NOT attach the session row that
    /// sits under the same screen coordinates (click-through bug).
    #[test]
    fn slash_model_dropdown_click_selects_model_not_session_row() {
        use agent_client_protocol as acp;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use indexmap::IndexMap;
        use ratatui::layout::Rect;

        let mut state = make_state_with_selection();
        let row_id = state
            .selected
            .as_ref()
            .cloned()
            .expect("seed state has a selection");

        // Session row occupies the same Y as the model dropdown item.
        state.row_rects = vec![(
            row_id.clone(),
            Rect {
                x: 0,
                y: 5,
                width: 80,
                height: 1,
            },
        )];
        // Slash dropdown overlays that row (as `render_slash_dropdown` does).
        state.slash_dropdown_items_area = Some(Rect {
            x: 2,
            y: 5,
            width: 40,
            height: 4,
        });
        state.slash_dropdown_hit = crate::views::slash_dropdown::RenderedDropdown {
            row_items: vec![0, 1, 2, 3],
            has_scrollbar: false,
        };

        // Seed a model catalog and open `/model ` so arg suggestions exist.
        let model_id = acp::ModelId::new("beta-model");
        let mut available = IndexMap::new();
        available.insert(
            model_id.clone(),
            acp::ModelInfo::new(model_id.clone(), "Beta Model"),
        );
        available.insert(
            acp::ModelId::new("alpha-model"),
            acp::ModelInfo::new(acp::ModelId::new("alpha-model"), "Alpha Model"),
        );
        state.models.update_catalog(available, Some(model_id));
        // Mirror how the real dashboard types into the dispatch box:
        // caret at end so `/model ` is in the args phase.
        state.dispatch.set_text("/model ");
        let end = state.dispatch.text().len();
        state.dispatch.textarea.set_cursor(end);
        state.dispatch.refresh_slash(&state.models);
        let snap = state.dispatch.slash_snapshot();
        assert!(
            !snap.matches.is_empty(),
            "expected model arg suggestions for /model "
        );

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5, // inside BOTH dropdown and row_rects
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let outcome = state.handle_mouse(&click);
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardAttach(_))),
            "model-list click must not attach session, got {outcome:?}"
        );
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "dropdown click should be consumed as Changed, got {outcome:?}"
        );
        let text = state.dispatch.text();
        assert!(
            text.contains("alpha-model")
                || text.contains("beta-model")
                || text.contains("Alpha")
                || text.contains("Beta"),
            "dispatch should contain accepted model completion, got {text:?}"
        );
        assert!(
            !state.list_focused,
            "clicking the dropdown focuses the input"
        );
    }

    /// Hover over the open slash dropdown updates `slash_hovered` so the
    /// list tracks the pointer (agent-view parity).
    #[test]
    fn slash_dropdown_mouse_move_sets_hover() {
        use agent_client_protocol as acp;
        use crossterm::event::{MouseEvent, MouseEventKind};
        use indexmap::IndexMap;
        use ratatui::layout::Rect;

        let mut state = DashboardState::new();
        state.slash_dropdown_items_area = Some(Rect {
            x: 2,
            y: 5,
            width: 40,
            height: 4,
        });
        state.slash_dropdown_hit = crate::views::slash_dropdown::RenderedDropdown {
            row_items: vec![0, 1, 2, 3],
            has_scrollbar: false,
        };
        let model_id = acp::ModelId::new("hover-model");
        let mut available = IndexMap::new();
        available.insert(
            model_id.clone(),
            acp::ModelInfo::new(model_id.clone(), "Hover Model"),
        );
        state.models.update_catalog(available, Some(model_id));
        state.dispatch.set_text("/model ");
        let end = state.dispatch.text().len();
        state.dispatch.textarea.set_cursor(end);
        state.dispatch.refresh_slash(&state.models);
        assert!(
            !state.dispatch.slash_snapshot().matches.is_empty(),
            "expected model suggestions so hover can land on a row"
        );

        let move_ev = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let _ = state.handle_mouse(&move_ev);
        assert_eq!(
            state.dispatch.slash_hovered(),
            Some(0),
            "pointer over first dropdown row should set hover index 0"
        );
    }

    /// With a section header selected and the prompt empty, Right
    /// expands, Left collapses, and Enter toggles it.
    #[test]
    fn section_keys_collapse_expand_and_toggle() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let key_sec = SectionKey::State(RowState::Working);
        state.focus_section(key_sec);
        assert_eq!(state.selected_section, Some(key_sec));

        let _ = state.handle_key(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &reg);
        assert!(state.is_section_collapsed(key_sec), "Left must collapse");

        let _ = state.handle_key(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &reg);
        assert!(!state.is_section_collapsed(key_sec), "Right must expand");

        let _ = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            state.is_section_collapsed(key_sec),
            "Enter toggles → collapsed"
        );
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            !state.is_section_collapsed(key_sec),
            "Enter toggles → expanded"
        );
    }

    /// A freshly-constructed dashboard starts with the "Inactive"
    /// (roster-only) section collapsed by default — and no other
    /// section. Expanding it is one keypress away (and survives reopen
    /// within the process; see `collapsed_sections` docs).
    #[test]
    fn inactive_section_collapsed_by_default() {
        let state = DashboardState::new();
        assert!(
            state.is_section_collapsed(SectionKey::State(RowState::Inactive)),
            "Inactive must start collapsed",
        );
        for other in [
            RowState::NeedsInput,
            RowState::Working,
            RowState::Idle,
            RowState::Completed,
            RowState::Failed,
            RowState::Blocked,
        ] {
            assert!(
                !state.is_section_collapsed(SectionKey::State(other)),
                "{other:?} must start expanded",
            );
        }
        assert!(
            !state.is_section_collapsed(SectionKey::Pinned),
            "Pinned must start expanded",
        );
    }

    /// Enter on the Idle overflow toggle flips `idle_show_all`; Right
    /// reveals, Left re-folds (with an empty prompt).
    #[test]
    fn idle_overflow_enter_and_arrows_toggle_show_all() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.focus_idle_overflow();
        assert!(state.selected_idle_overflow);
        assert!(!state.idle_show_all, "starts capped");

        let _ = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(state.idle_show_all, "Enter reveals");
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(!state.idle_show_all, "Enter re-folds");

        let _ = state.handle_key(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &reg);
        assert!(state.idle_show_all, "Right reveals");
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &reg);
        assert!(!state.idle_show_all, "Left re-folds");
    }

    /// vim mode ON with the LIST focused — `l` / `h` on the Idle overflow
    /// toggle reveal / re-fold the folded agents (mirroring the section
    /// keys). vim mode ON with the INPUT focused — they type into the
    /// dispatch prompt instead of toggling.
    #[test]
    fn idle_overflow_vim_hl_focus_gated() {
        let reg = crate::actions::ActionRegistry::defaults();

        // vim ON + LIST focused — `l`/`h` toggle show-all.
        crate::appearance::cache::set_vim_mode(true);
        let mut state = DashboardState::new();
        state.focus_idle_overflow();
        state.list_focused = true;
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &reg);
        let show_all_after_l = state.idle_show_all;
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &reg);
        let show_all_after_h = state.idle_show_all;
        crate::appearance::cache::set_vim_mode(false);
        assert!(show_all_after_l, "list-focused vim `l` must reveal");
        assert!(!show_all_after_h, "list-focused vim `h` must re-fold");

        // vim ON + INPUT focused (list_focused == false) + empty draft —
        // `h`/`l` must type into the prompt, never toggle show-all.
        crate::appearance::cache::set_vim_mode(true);
        let mut state = DashboardState::new();
        state.focus_idle_overflow();
        // Input focused is the default; leave list_focused == false.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &reg);
        let show_all_after_input_h = state.idle_show_all;
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &reg);
        let show_all_after_input_l = state.idle_show_all;
        let typed = state.dispatch.text().to_string();
        crate::appearance::cache::set_vim_mode(false);
        assert!(
            !show_all_after_input_h && !show_all_after_input_l,
            "input-focused vim `h`/`l` must not toggle show-all",
        );
        assert_eq!(
            typed, "hl",
            "input-focused vim `h`/`l` must type into the dispatch input",
        );
    }

    /// With the list focused and the Idle overflow toggle selected, Esc
    /// focuses the `[+ New Agent]` button (mirroring the section / row
    /// deselect tiers), rather than exiting.
    #[test]
    fn idle_overflow_esc_focuses_new_agent_button() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.focus_idle_overflow();
        state.list_focused = true;
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &reg);
        assert!(state.new_agent_button_focused, "Esc focuses the button");
        assert!(
            !state.selected_idle_overflow,
            "Esc clears the overflow cursor"
        );
    }

    /// The overflow cursor is mutually exclusive with the other three
    /// cursor targets — focusing any of them clears it.
    #[test]
    fn focusing_other_targets_clears_idle_overflow() {
        let mut state = DashboardState::new();
        state.focus_idle_overflow();
        state.focus_new_agent_button();
        assert!(
            !state.selected_idle_overflow,
            "button focus clears overflow"
        );

        state.focus_idle_overflow();
        state.focus_section(SectionKey::State(RowState::Working));
        assert!(
            !state.selected_idle_overflow,
            "section focus clears overflow"
        );

        state.focus_idle_overflow();
        state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(0)));
        assert!(!state.selected_idle_overflow, "row focus clears overflow");
    }

    /// `reanchor_selection` drops a stale overflow cursor (the toggle row
    /// vanished because the Idle group is no longer capped) onto the
    /// `[+ New Agent]` button.
    #[test]
    fn reanchor_clears_stale_idle_overflow_cursor() {
        let mut state = DashboardState::new();
        state.focus_idle_overflow();
        // No rows at all → no overflow focusable exists.
        state.reanchor_selection(&[]);
        assert!(
            state.new_agent_button_focused,
            "a stranded overflow cursor must fall back to the button",
        );
        assert!(!state.selected_idle_overflow);
    }

    /// With the list focused and a section header selected, Esc focuses
    /// the `[+ New Agent]` button (mirroring the row-deselect tier),
    /// rather than exiting.
    #[test]
    fn section_esc_focuses_new_agent_button() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.focus_section(SectionKey::State(RowState::Working));
        state.list_focused = true;
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &reg);
        assert!(
            state.new_agent_button_focused,
            "Esc on a section must focus [+ New Agent]",
        );
        assert!(
            state.selected_section.is_none(),
            "Esc must clear the section cursor",
        );
    }

    /// Clicking a section header selects it and toggles its collapse.
    #[test]
    fn section_click_selects_and_toggles() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut state = DashboardState::new();
        let key_sec = SectionKey::State(RowState::Idle);
        // Simulate a rendered header hit rect (render rebuilds these).
        state.section_rects.push((key_sec, Rect::new(0, 0, 40, 1)));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let _ = state.handle_mouse(&click);
        assert_eq!(
            state.selected_section,
            Some(key_sec),
            "click selects the section",
        );
        assert!(state.is_section_collapsed(key_sec), "click collapses");
        // A second click expands it again (rect persists between renders).
        let _ = state.handle_mouse(&click);
        assert!(!state.is_section_collapsed(key_sec), "second click expands");
    }

    /// Moving the mouse over a section header sets `hovered_section`;
    /// moving off clears it.
    #[test]
    fn section_hover_sets_and_clears_hovered_section() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let mut state = DashboardState::new();
        let key_sec = SectionKey::State(RowState::Idle);
        state.section_rects.push((key_sec, Rect::new(0, 0, 40, 1)));
        let over = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 5,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let _ = state.handle_mouse(&over);
        assert_eq!(state.hovered_section, Some(key_sec));
        let off = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let _ = state.handle_mouse(&off);
        assert_eq!(state.hovered_section, None);
    }

    /// `focus_section` / `focus_row` / `focus_new_agent_button` keep the
    /// three cursor targets mutually exclusive.
    #[test]
    fn cursor_targets_are_mutually_exclusive() {
        let mut state = DashboardState::new();
        state.focus_section(SectionKey::Pinned);
        assert_eq!(state.selected_section, Some(SectionKey::Pinned));
        assert!(state.selected.is_none());
        assert!(!state.new_agent_button_focused);

        state.focus_row(DashboardRowId::TopLevel(crate::app::agent::AgentId(0)));
        assert!(
            state.selected_section.is_none(),
            "focus_row clears the section"
        );

        state.focus_section(SectionKey::Pinned);
        state.focus_new_agent_button();
        assert!(
            state.selected_section.is_none(),
            "focus_new_agent_button clears the section",
        );
    }

    /// The cheatsheet's `[✗]` chrome close button is clickable and
    /// hover-tracked — mouse events must route through
    /// `modal_window::handle_modal_mouse` before the picker content
    /// (whose own close rect is a dead `Rect::default()`).
    #[test]
    fn shortcuts_modal_close_button_clicks_and_hovers() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut state = DashboardState::new();
        let entries = Vec::new();
        let picker = crate::views::shortcuts_help::build_initial_picker_state(&entries);
        let mut modal = Box::new(ShortcutsModalState {
            entries,
            state: picker,
            window: Default::default(),
            filter_active: false,
            collapsed_sections: Default::default(),
            expanded_ids: std::collections::HashSet::new(),
            mode: crate::views::shortcuts_help::ShortcutsHelpMode::Browse,
        });
        // Simulate the rect render_close_button records each frame.
        modal.window.close_button_rect = Some(Rect::new(70, 2, 5, 1));
        state.shortcuts_modal = Some(modal);
        let reg = crate::actions::ActionRegistry::defaults();

        // Hover over the button → tracked + repaint requested.
        let over = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 72,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            state.handle_input(&over, &reg),
            InputOutcome::Changed
        ));
        assert!(
            state
                .shortcuts_modal
                .as_ref()
                .is_some_and(|m| m.window.close_hovered),
            "hovering [✗] must set close_hovered",
        );

        // Click on the button → close action.
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 72,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            matches!(
                state.handle_input(&click, &reg),
                InputOutcome::Action(Action::DashboardCloseShortcutsHelp)
            ),
            "clicking [✗] must request close",
        );
    }

    /// An inline-expand keypress toggles the selected row's id in and out of
    /// `expanded_ids` (on→off→on) through the shared `toggle_membership`.
    #[test]
    fn shortcuts_modal_key_toggles_inline_expand() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let entries = crate::views::shortcuts_help::build_entries(
            &[
                crate::actions::When::DashboardFocused,
                crate::actions::When::Always,
            ],
            &reg,
            true,
        );
        let picker = crate::views::shortcuts_help::build_initial_picker_state(&entries);
        let mut modal = Box::new(ShortcutsModalState {
            entries,
            state: picker,
            window: Default::default(),
            filter_active: false,
            collapsed_sections: Default::default(),
            expanded_ids: std::collections::HashSet::new(),
            mode: crate::views::shortcuts_help::ShortcutsHelpMode::Browse,
        });
        // Land on the first registry-backed hint (the section header is row 0).
        modal.state.selected = 1;
        state.shortcuts_modal = Some(modal);

        let right = || Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let expanded_len =
            |s: &DashboardState| s.shortcuts_modal.as_ref().unwrap().expanded_ids.len();

        assert!(matches!(
            state.handle_input(&right(), &reg),
            InputOutcome::Changed
        ));
        assert_eq!(expanded_len(&state), 1, "first press expands the row");
        state.handle_input(&right(), &reg);
        assert_eq!(expanded_len(&state), 0, "second press collapses it");
        state.handle_input(&right(), &reg);
        assert_eq!(expanded_len(&state), 1, "third press expands it again");
    }

    /// Opening the detail page and returning must leave every browse-list field
    /// (selection, query, filter, collapsed, expanded) exactly as it was.
    #[test]
    fn shortcuts_modal_detail_round_trip_preserves_browse_state() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let entries = crate::views::shortcuts_help::build_entries(
            &[
                crate::actions::When::DashboardFocused,
                crate::actions::When::Always,
            ],
            &reg,
            true,
        );
        let picker = crate::views::shortcuts_help::build_initial_picker_state(&entries);
        let mut modal = Box::new(ShortcutsModalState {
            entries,
            state: picker,
            window: Default::default(),
            filter_active: false,
            collapsed_sections: std::collections::HashSet::from([6usize]),
            expanded_ids: std::collections::HashSet::from([
                crate::views::shortcuts_help::ExpandKey::Action(crate::actions::ActionId::Quit),
            ]),
            mode: crate::views::shortcuts_help::ShortcutsHelpMode::Browse,
        });
        modal.state.selected = 1; // first registry-backed hint
        state.shortcuts_modal = Some(modal);

        let snapshot = |s: &DashboardState| {
            let m = s.shortcuts_modal.as_ref().unwrap();
            (
                m.state.selected,
                m.state.query().to_owned(),
                m.filter_active,
                m.collapsed_sections.clone(),
                m.expanded_ids.clone(),
            )
        };
        let before = snapshot(&state);

        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        state.handle_input(&enter, &reg);
        assert!(
            state.shortcuts_modal.as_ref().unwrap().mode.is_detail(),
            "Enter on a registry hint opens the detail page",
        );

        let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        state.handle_input(&esc, &reg);
        assert!(
            state.shortcuts_modal.as_ref().unwrap().mode.is_browse(),
            "Esc returns to the browse list",
        );
        assert_eq!(
            before,
            snapshot(&state),
            "detail round-trip must preserve all browse-list state",
        );
    }

    /// Collapse/expand keypresses on a section header dismiss a pending
    /// feedback toast like every other key (the invariant) —
    /// the intercept must sit BELOW the handler's toast-clear tier.
    #[test]
    fn section_keys_clear_pending_toast() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let key_sec = SectionKey::State(RowState::Working);
        state.focus_section(key_sec);
        state.set_error_toast("boom");
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &reg);
        assert!(state.is_section_collapsed(key_sec), "Left must collapse");
        assert!(
            state.error_toast.is_none(),
            "a collapse keypress must dismiss the pending toast",
        );
    }

    /// An armed stop confirmation is bound to the row that was selected
    /// when `Ctrl+X` was pressed — any other key (nav included) must
    /// disarm it, otherwise the footer's "press again to close" hint
    /// lingers while the cursor moves to other agents. The disarm must
    /// NOT depend on `error_toast` (the Ctrl+X arm path plants none).
    #[test]
    fn nav_key_disarms_pending_stop_confirm() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        state.focus_row(DashboardRowId::TopLevel(AgentId(0)));
        state.stop_confirm = Some((DashboardRowId::TopLevel(AgentId(0)), Instant::now()));
        assert!(state.error_toast.is_none(), "arm path plants no toast");
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &reg);
        assert!(
            state.stop_confirm.is_none(),
            "a nav keypress must disarm the pending stop confirm",
        );

        // Control — Ctrl+X itself preserves the armed confirm so the
        // dispatcher can observe it and close.
        state.stop_confirm = Some((DashboardRowId::TopLevel(AgentId(0)), Instant::now()));
        let _ = state.handle_key(
            &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &reg,
        );
        assert!(
            state.stop_confirm.is_some(),
            "Ctrl+X must preserve the armed confirm for the dispatcher",
        );

        // The actual repro path: peek is open by default for a selected
        // row, and `handle_peek_key` CONSUMES Up/Down (agent switch) —
        // the disarm must sit above that intercept or nav keys never
        // reach it and the footer hint lingers.
        state.stop_confirm = Some((DashboardRowId::TopLevel(AgentId(0)), Instant::now()));
        state.peek = Some(super::super::peek::PeekPanelState::new(
            DashboardRowId::TopLevel(AgentId(0)),
            peek_fields_for_test("Idle"),
        ));
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &reg);
        assert!(
            state.stop_confirm.is_none(),
            "a nav keypress consumed by the peek panel must still disarm the confirm",
        );
    }

    /// Section header selected while the LIST is focused — the input is
    /// inactive, so Enter / Left / Right operate on the section even
    /// when a draft is sitting in the (unfocused) dispatch input.
    /// (With the input focused, text flips those keys to draft editing
    /// / dispatch — covered by the gate's `prompt_empty` arm.)
    #[test]
    fn section_keys_work_with_draft_when_list_focused() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let key_sec = SectionKey::State(RowState::Working);
        state.focus_section(key_sec);
        state.list_focused = true;
        state.dispatch.set_text("draft for a new agent");

        let _ = state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &reg);
        assert!(
            state.is_section_collapsed(key_sec),
            "list-focused Enter must toggle the section despite the draft",
        );
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &reg);
        assert!(
            !state.is_section_collapsed(key_sec),
            "list-focused Right must expand despite the draft",
        );
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &reg);
        assert!(
            state.is_section_collapsed(key_sec),
            "list-focused Left must collapse despite the draft",
        );
        assert_eq!(
            state.dispatch.text(),
            "draft for a new agent",
            "the inactive input's draft must survive untouched",
        );
    }

    /// vim `l` opens detail on list-focused rows; focused dispatch types `l`.
    #[test]
    fn vim_l_row_attach_and_input_focus() {
        use crate::app::actions::Action;
        use crate::views::dashboard::DashboardRowId;

        let reg = crate::actions::ActionRegistry::defaults();
        let id = DashboardRowId::TopLevel(crate::app::agent::AgentId(42));
        let l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);

        crate::appearance::cache::set_vim_mode(true);
        let mut state = DashboardState::new();
        state.focus_row(id.clone());
        state.list_focused = true;
        match state.handle_key(&l, &reg) {
            InputOutcome::Action(Action::DashboardAttach(row)) => assert_eq!(row, id),
            other => panic!("list-focused vim `l` must attach, got {other:?}"),
        }

        let mut state = DashboardState::new();
        state.focus_row(id);
        state.list_focused = false;
        let outcome = state.handle_key(&l, &reg);
        crate::appearance::cache::set_vim_mode(false);
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardAttach(_))),
            "input-focused vim `l` must not attach, got {outcome:?}",
        );
        assert_eq!(state.dispatch.text(), "l");
    }

    /// vim `l` on peek: unfocused attaches; focused empty reply types `l`.
    #[test]
    fn vim_l_peek_attach_and_focused_type() {
        use crate::app::actions::Action;
        let reg = crate::actions::ActionRegistry::defaults();
        let l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);

        // state_with_open_peek pins vim off for seed focus; enable after.
        let mut state = state_with_open_peek();
        crate::appearance::cache::set_vim_mode(true);
        state.peek.as_mut().unwrap().focused = false;
        match state.handle_key(&l, &reg) {
            InputOutcome::Action(Action::DashboardAttach(row)) => {
                assert_eq!(row, DashboardRowId::TopLevel(AgentId(0)));
            }
            other => panic!("unfocused peek vim `l` must attach, got {other:?}"),
        }

        let mut state = state_with_open_peek();
        crate::appearance::cache::set_vim_mode(true);
        assert!(state.peek.as_ref().unwrap().focused);
        let outcome = state.handle_key(&l, &reg);
        let reply = state.peek_reply.text().to_string();
        crate::appearance::cache::set_vim_mode(false);
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardAttach(_))),
            "focused reply vim `l` must not attach, got {outcome:?}",
        );
        assert_eq!(reply, "l");
    }

    /// List-focused vim `h`/`l` fold sections; input-focused or vim-off type.
    #[test]
    fn section_vim_hl_collapse_expand() {
        let reg = crate::actions::ActionRegistry::defaults();
        let key_sec = SectionKey::State(RowState::Working);

        // vim ON + LIST focused — `h`/`l` fold the section.
        crate::appearance::cache::set_vim_mode(true);
        let mut state = DashboardState::new();
        state.focus_section(key_sec);
        state.list_focused = true;
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &reg);
        let collapsed_after_h = state.is_section_collapsed(key_sec);
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &reg);
        let collapsed_after_l = state.is_section_collapsed(key_sec);
        // Reset before asserting so a failure can't leak vim state
        // into another test sharing this thread's cache.
        crate::appearance::cache::set_vim_mode(false);
        assert!(collapsed_after_h, "list-focused vim `h` must collapse");
        assert!(!collapsed_after_l, "list-focused vim `l` must expand");

        // vim ON + INPUT focused (list_focused == false) + empty draft —
        // `h`/`l` must type into the prompt, never fold the section.
        crate::appearance::cache::set_vim_mode(true);
        let mut state = DashboardState::new();
        state.focus_section(key_sec);
        // Input focused is the default; leave list_focused == false.
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &reg);
        let collapsed_after_input_h = state.is_section_collapsed(key_sec);
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &reg);
        let collapsed_after_input_l = state.is_section_collapsed(key_sec);
        let typed = state.dispatch.text().to_string();
        crate::appearance::cache::set_vim_mode(false);
        assert!(
            !collapsed_after_input_h && !collapsed_after_input_l,
            "input-focused vim `h`/`l` must not fold the section",
        );
        assert_eq!(
            typed, "hl",
            "input-focused vim `h`/`l` must type into the dispatch input",
        );

        // vim OFF — bare letters are dispatch-input edits, never
        // collapse keys, even with a section header selected.
        let mut state = DashboardState::new();
        state.focus_section(key_sec);
        let _ = state.handle_key(&KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &reg);
        assert!(
            !state.is_section_collapsed(key_sec),
            "vim-off `l` must not collapse",
        );
        assert_eq!(
            state.dispatch.text(),
            "l",
            "vim-off `l` must type into the dispatch input",
        );
    }

    /// Build a minimal top-level row for the reanchor tests.
    fn reanchor_test_row(id: usize, state: RowState) -> super::super::row::DashboardRow {
        super::super::row::DashboardRow {
            id: DashboardRowId::TopLevel(AgentId(id)),
            label: format!("r{id}"),
            subtitle: None,
            state,
            activity: None,
            secondary_line: None,
            cwd_display: String::new(),
            cwd: std::path::PathBuf::from("/"),
            last_change_at: std::time::SystemTime::now(),
            pinned: false,
            is_active: false,
            badges: Vec::new(),
            context_pct: None,
            indent: 0,
            parent_label: None,
            is_more_placeholder: false,
            more_count: 0,
        }
    }

    /// A selected section header whose section no longer exists (row
    /// churn removed its last row) is moved to the `[+ New Agent]`
    /// button by `reanchor_selection`, so the footer hints and the
    /// collapse keys never act on an invisible header.
    #[test]
    fn reanchor_moves_stale_section_cursor_to_button() {
        let mut state = DashboardState::new();
        state.focus_section(SectionKey::State(RowState::Idle));
        // Only a Working row remains — the Idle header is gone.
        let rows = vec![reanchor_test_row(1, RowState::Working)];
        state.reanchor_selection(&rows);
        assert!(
            state.selected_section.is_none(),
            "stale section cursor must be cleared",
        );
        assert!(
            state.new_agent_button_focused,
            "cursor must move to the [+ New Agent] button",
        );
    }

    /// A `s:state` filter suppresses ALL state headers — a section
    /// cursor left over from before the filter must be re-anchored
    /// even though the section's rows still exist.
    #[test]
    fn reanchor_moves_section_cursor_when_state_filter_hides_headers() {
        let mut state = DashboardState::new();
        state.focus_section(SectionKey::State(RowState::Working));
        state.filter = Filter::State(RowState::Working);
        let rows = vec![reanchor_test_row(1, RowState::Working)];
        state.reanchor_selection(&rows);
        assert!(
            state.selected_section.is_none(),
            "headers are suppressed under a state filter — the section cursor must clear",
        );
        assert!(state.new_agent_button_focused);
    }

    /// A section cursor whose header is still on screen is untouched.
    #[test]
    fn reanchor_keeps_live_section_cursor() {
        let mut state = DashboardState::new();
        state.focus_section(SectionKey::State(RowState::Working));
        let rows = vec![reanchor_test_row(1, RowState::Working)];
        state.reanchor_selection(&rows);
        assert_eq!(
            state.selected_section,
            Some(SectionKey::State(RowState::Working)),
            "a live section cursor must survive reanchoring",
        );
        assert!(!state.new_agent_button_focused);
    }

    /// A selected row that state churn migrated INTO a collapsed
    /// section (still present in `rows`, but hidden by the collapse)
    /// moves the cursor onto the section header that hid it — never an
    /// invisible row with live footer hints / peek.
    #[test]
    fn reanchor_moves_collapse_hidden_row_cursor_to_its_header() {
        let mut state = DashboardState::new();
        state.set_section_collapsed(SectionKey::State(RowState::Working), true);
        // The row was selected while (say) Idle; it has since started
        // Working — and "Working" is collapsed.
        state.focus_row(DashboardRowId::TopLevel(AgentId(1)));
        let rows = vec![reanchor_test_row(1, RowState::Working)];
        state.reanchor_selection(&rows);
        assert!(
            state.selected.is_none(),
            "the hidden row must not keep the cursor",
        );
        assert_eq!(
            state.selected_section,
            Some(SectionKey::State(RowState::Working)),
            "the cursor must land on the header that hides the row",
        );
        assert!(!state.new_agent_button_focused);
    }

    /// Same churn scenario for the Pinned block: a selected pinned row
    /// hidden by a collapsed "Pinned" section re-anchors to that header.
    #[test]
    fn reanchor_moves_collapse_hidden_pinned_row_to_pinned_header() {
        let mut state = DashboardState::new();
        state.set_section_collapsed(SectionKey::Pinned, true);
        state.focus_row(DashboardRowId::TopLevel(AgentId(1)));
        let mut row = reanchor_test_row(1, RowState::Idle);
        row.pinned = true;
        state.reanchor_selection(&[row]);
        assert!(state.selected.is_none());
        assert_eq!(
            state.selected_section,
            Some(SectionKey::Pinned),
            "a collapse-hidden pinned row must re-anchor to the Pinned header",
        );
    }

    /// A subagent row hidden by its PARENT's collapsed section
    /// re-anchors to the parent's state header (subagents render under
    /// the parent's group).
    #[test]
    fn reanchor_moves_collapse_hidden_subagent_to_parent_header() {
        let mut state = DashboardState::new();
        state.set_section_collapsed(SectionKey::State(RowState::Working), true);
        let child_id = DashboardRowId::Subagent {
            parent: AgentId(1),
            child_session_id: "child-1".into(),
        };
        state.focus_row(child_id.clone());
        let parent = reanchor_test_row(1, RowState::Working);
        let mut child = reanchor_test_row(2, RowState::Working);
        child.id = child_id;
        child.indent = 1;
        state.reanchor_selection(&[parent, child]);
        assert!(state.selected.is_none());
        assert_eq!(
            state.selected_section,
            Some(SectionKey::State(RowState::Working)),
            "a hidden subagent must re-anchor to its parent's header",
        );
    }

    /// A selected row stays selected when a DIFFERENT section is
    /// collapsed (its own section is still expanded).
    #[test]
    fn reanchor_keeps_row_cursor_when_other_section_collapsed() {
        let mut state = DashboardState::new();
        state.set_section_collapsed(SectionKey::State(RowState::Idle), true);
        let sel = DashboardRowId::TopLevel(AgentId(1));
        state.focus_row(sel.clone());
        let rows = vec![
            reanchor_test_row(1, RowState::Working),
            reanchor_test_row(2, RowState::Idle),
        ];
        state.reanchor_selection(&rows);
        assert_eq!(
            state.selected,
            Some(sel),
            "a visible row's cursor must survive reanchoring",
        );
        assert!(state.selected_section.is_none());
    }

    /// Under a `s:state` filter headers are suppressed, so collapse
    /// never hides rows — a leftover collapsed flag must NOT steal the
    /// row cursor (the row is visible).
    #[test]
    fn reanchor_keeps_row_cursor_under_state_filter_despite_collapsed_flag() {
        let mut state = DashboardState::new();
        state.set_section_collapsed(SectionKey::State(RowState::Working), true);
        state.filter = Filter::State(RowState::Working);
        let sel = DashboardRowId::TopLevel(AgentId(1));
        state.focus_row(sel.clone());
        let rows = vec![reanchor_test_row(1, RowState::Working)];
        state.reanchor_selection(&rows);
        assert_eq!(
            state.selected,
            Some(sel),
            "headers (and thus collapse) are suppressed under a state filter — \
             the visible row must keep the cursor",
        );
        assert!(state.selected_section.is_none());
    }

    /// Clicking anywhere on the dispatch input box focuses the input
    /// (clears `list_focused`). This must hold in vim mode too — there
    /// the overview owns the keyboard (j/k nav), so a mouse user who
    /// Tabbed or vim-navigated into the list would otherwise be stuck
    /// with no way to click back into the prompt.
    #[test]
    fn click_on_dispatch_box_focuses_input_in_both_modes() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let box_rect = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 3,
        };
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        for vim in [false, true] {
            crate::appearance::cache::set_vim_mode(vim);
            let mut state = DashboardState::new();
            // Overview focused (as if via Tab / vim nav).
            state.list_focused = true;
            // Box rect as recorded by `render_dashboard`.
            state.dispatch_rect = Some(box_rect);
            let outcome = state.handle_mouse(&click);
            let focused_input = !state.list_focused;
            // Reset before asserting so a failure can't leak vim state
            // into another test sharing this thread's cache.
            crate::appearance::cache::set_vim_mode(false);
            assert!(
                matches!(outcome, InputOutcome::Changed),
                "vim={vim}: click on dispatch box must report Changed, got {outcome:?}",
            );
            assert!(
                focused_input,
                "vim={vim}: click on dispatch box must focus the input (clear list_focused)",
            );
        }
    }

    /// A click that lands outside the dispatch box (with no row or
    /// button underneath) does not steal focus into the input.
    #[test]
    fn click_outside_dispatch_box_leaves_focus() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let mut state = DashboardState::new();
        state.list_focused = true;
        // Box occupies the first 3 rows; click well below it.
        state.dispatch_rect = Some(Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 3,
        });
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 20,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let outcome = state.handle_mouse(&click);
        assert!(
            matches!(outcome, InputOutcome::Unchanged),
            "click below the dispatch box must be a no-op, got {outcome:?}",
        );
        assert!(
            state.list_focused,
            "click outside the dispatch box must not move focus to the input",
        );
    }

    /// Registry walk — `Ctrl+\\` is reserved for dashboard
    /// navigation. It's bound to `OpenDashboard` (global, `When::Always`) and
    /// to `DashboardOverlayExit` (`When::DashboardOverlay`) — disjoint
    /// contexts that both route to the dashboard (the overlay intercept maps
    /// both to `DashboardOverlayExit`). No OTHER, unrelated action may claim
    /// it. Notably it must NOT be the worktree toggle's key (that's Ctrl+W).
    #[test]
    fn ctrl_backslash_only_bound_to_dashboard_navigation() {
        use crate::actions::ActionId;
        let reg = crate::actions::ActionRegistry::defaults();
        let mut bound_to_ctrl_backslash: Vec<ActionId> = Vec::new();
        for def in reg.all() {
            let mut keys = vec![def.default_key];
            keys.extend_from_slice(&def.alt_keys);
            for k in keys {
                let s = k.display().to_string();
                if s.eq_ignore_ascii_case("Ctrl+\\") {
                    bound_to_ctrl_backslash.push(def.id);
                }
            }
        }
        bound_to_ctrl_backslash.sort_by_key(|id| format!("{id:?}"));
        assert_eq!(
            bound_to_ctrl_backslash,
            vec![ActionId::DashboardOverlayExit, ActionId::OpenDashboard],
            "Ctrl+\\ must be bound only to the two dashboard-navigation actions",
        );
    }

    /// Direct unit test for `clamp_viewport`: when the
    /// total row count shrinks below the current offset, the offset
    /// is pulled in to keep `max_offset = total - viewport_h`.
    #[test]
    fn clamp_viewport_pulls_offset_when_rows_shrink() {
        let mut s = DashboardState::new();
        s.viewport_offset = 10;
        // No selection, viewport=5, total=8 → max_offset = 3.
        s.clamp_viewport(None, 5, 8);
        assert_eq!(s.viewport_offset, 3);
    }

    /// Direct unit test for `clamp_viewport`: a selection
    /// below the visible window snaps the offset down so the row is
    /// visible at the bottom edge.
    #[test]
    fn clamp_viewport_snaps_to_selection() {
        let mut s = DashboardState::new();
        s.viewport_offset = 0;
        // Selection at line 12, viewport=5, total=20.
        // 12 >= offset + 5 (0 + 5) → offset = 12 + 1 - 5 = 8.
        s.clamp_viewport(Some(12), 5, 20);
        assert_eq!(s.viewport_offset, 8);
    }

    /// Selection above the window pulls the offset up to
    /// keep the selection visible at the top edge.
    #[test]
    fn clamp_viewport_snaps_offset_up_when_selection_scrolls_above() {
        let mut s = DashboardState::new();
        s.viewport_offset = 10;
        s.clamp_viewport(Some(2), 5, 20);
        // sel_idx < offset → offset = sel_idx (2).
        assert_eq!(s.viewport_offset, 2);
    }

    /// zero-height viewport: clamp returns 0 (no visible
    /// rows means nothing to scroll to).
    #[test]
    fn clamp_viewport_handles_zero_viewport_height() {
        let mut s = DashboardState::new();
        s.viewport_offset = 5;
        s.clamp_viewport(Some(3), 0, 10);
        // viewport_h = 0 → no snap-to-selection; max_offset = total -
        // 0 = 10, so offset stays at 5.
        assert_eq!(s.viewport_offset, 5);
    }

    // -----------------------------------------------------------------
    // Mouse wheel decoupled from selection
    // -----------------------------------------------------------------

    /// `handle_scroll` flags `manual_scroll_active` so the next
    /// `clamp_viewport` knows to skip the snap-to-selection
    /// pull-back.
    #[test]
    fn handle_scroll_sets_manual_scroll_active() {
        let mut s = DashboardState::new();
        assert!(!s.manual_scroll_active);
        s.handle_scroll(3);
        assert!(s.manual_scroll_active);
        assert_eq!(s.viewport_offset, 3);
    }

    /// `handle_scroll(0)` is a no-op — neither the offset nor the
    /// flag changes. Without this, a stray zero-line accumulator
    /// flush would clobber the snap state.
    #[test]
    fn handle_scroll_zero_lines_is_noop() {
        let mut s = DashboardState::new();
        s.viewport_offset = 4;
        s.handle_scroll(0);
        assert!(!s.manual_scroll_active);
        assert_eq!(s.viewport_offset, 4);
    }

    /// Negative scrolls (wheel up) move the offset toward the top
    /// AND still flag the viewport as user-driven.
    #[test]
    fn handle_scroll_negative_moves_offset_up() {
        let mut s = DashboardState::new();
        s.viewport_offset = 10;
        s.handle_scroll(-4);
        assert!(s.manual_scroll_active);
        assert_eq!(s.viewport_offset, 6);
    }

    /// Saturating arithmetic guards the upper-edge: scrolling up
    /// when already at 0 stays at 0 (no underflow panic) and still
    /// flips the manual-scroll flag.
    #[test]
    fn handle_scroll_saturates_at_zero() {
        let mut s = DashboardState::new();
        s.viewport_offset = 2;
        s.handle_scroll(-99);
        assert_eq!(s.viewport_offset, 0);
        assert!(s.manual_scroll_active);
    }

    /// THE FIX: when the user has manually scrolled, the
    /// snap-to-selection in `clamp_viewport` is skipped so the
    /// viewport doesn't get yanked back to the selected row. Without
    /// this skip, scrolling past the cursor was a no-op visually
    /// (the renderer snapped it back next frame).
    #[test]
    fn clamp_viewport_skips_snap_when_manual_scroll_active() {
        let mut s = DashboardState::new();
        s.manual_scroll_active = true;
        // viewport_h=5, total=50, selection at line 0 — without the
        // skip, the snap would pull offset back to 0.
        s.viewport_offset = 20;
        s.clamp_viewport(Some(0), 5, 50);
        assert_eq!(
            s.viewport_offset, 20,
            "manual_scroll_active must suppress the snap-to-selection pull-back",
        );
    }

    /// The manual-scroll flag does NOT disable the bounds clamp —
    /// scrolling past the bottom edge still stops at `max_offset`.
    /// Otherwise wheel acceleration would let the user park the
    /// viewport on an entirely empty band below the last row.
    #[test]
    fn clamp_viewport_still_clamps_max_offset_when_manual_scroll_active() {
        let mut s = DashboardState::new();
        s.manual_scroll_active = true;
        s.viewport_offset = 100;
        s.clamp_viewport(Some(0), 5, 20);
        // max_offset = 20 - 5 = 15.
        assert_eq!(s.viewport_offset, 15);
    }

    /// Clearing the manual-scroll flag re-engages the
    /// snap-to-selection on the next clamp, restoring the keyboard-
    /// nav contract.
    #[test]
    fn clear_manual_scroll_re_engages_snap_to_selection() {
        let mut s = DashboardState::new();
        s.manual_scroll_active = true;
        s.viewport_offset = 20;
        s.clear_manual_scroll();
        assert!(!s.manual_scroll_active);
        s.clamp_viewport(Some(0), 5, 50);
        // With the flag cleared, the snap pulls the offset back so
        // selection at line 0 is visible.
        assert_eq!(s.viewport_offset, 0);
    }

    /// Env var force-disables.
    ///
    /// Guard the env-var mutation with `serial_test`'s
    /// per-key serial lock. The `GROK_AGENT_DASHBOARD` key means this
    /// test runs serially with any other test that decorates itself
    /// with `#[serial_test::serial(GROK_AGENT_DASHBOARD)]` — see the
    /// `dispatch_open_dashboard`-calling tests in `app::dispatch`.
    /// A function-local `Mutex` would only serialize
    /// against itself; readers in other tests
    /// could still observe the transient `0` value.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn env_var_force_disables() {
        // SAFETY: the test temporarily mutates a process-wide env var.
        // `serial_test`'s lock ensures no other test marked with the
        // same `GROK_AGENT_DASHBOARD` key reads it concurrently.
        unsafe { std::env::set_var("GROK_AGENT_DASHBOARD", "0") };
        assert!(!super::super::dashboard_enabled());
        unsafe { std::env::remove_var("GROK_AGENT_DASHBOARD") };
    }

    // ── Location picker ─────────────────────────────────────────────

    fn location_candidate(path: &str, label: &str) -> LocationCandidate {
        LocationCandidate {
            path: PathBuf::from(path),
            label: label.to_string(),
            detail: path.to_string(),
            worktree: None,
        }
    }

    /// Build a picker over `recents` with a fixed base cwd and no worktrees.
    fn location_picker(recents: Vec<LocationCandidate>) -> LocationPickerState {
        LocationPickerState::new(
            recents,
            PathBuf::from("/base"),
            std::collections::HashMap::new(),
        )
    }

    /// Build a picker with a worktree index for tagging suggestions.
    fn location_picker_with_worktrees(
        recents: Vec<LocationCandidate>,
        worktrees: std::collections::HashMap<PathBuf, String>,
    ) -> LocationPickerState {
        LocationPickerState::new(recents, PathBuf::from("/base"), worktrees)
    }

    fn visible_labels(lp: &LocationPickerState) -> Vec<String> {
        lp.visible_candidates()
            .into_iter()
            .map(|c| c.label)
            .collect()
    }

    #[test]
    fn location_visible_empty_query_shows_all_recents() {
        let lp = location_picker(vec![
            location_candidate("/home/me/alpha", "alpha"),
            location_candidate("/home/me/beta", "beta"),
        ]);
        assert_eq!(visible_labels(&lp), vec!["alpha", "beta"]);
    }

    #[test]
    fn location_visible_filters_recents_by_substring() {
        let mut lp = location_picker(vec![
            location_candidate("/home/me/alpha", "alpha"),
            location_candidate("/home/me/beta", "beta"),
        ]);
        lp.picker.set_query("bet");
        assert_eq!(visible_labels(&lp), vec!["beta"]);
    }

    #[test]
    fn location_query_is_path_detection() {
        let mut lp = location_picker(vec![]);
        for q in ["/abs", "~/x", "rel/sub", "~"] {
            lp.picker.set_query(q);
            assert!(lp.query_is_path(), "`{q}` should be path mode");
        }
        for q in ["", "alpha", "bet"] {
            lp.picker.set_query(q);
            assert!(!lp.query_is_path(), "`{q}` should be recents mode");
        }
    }

    /// Windows drive-prefix detection is platform-independent (the
    /// `cfg!(windows)` gate on its use is exercised only on Windows, but the
    /// predicate itself must be correct everywhere).
    #[test]
    fn windows_drive_prefix_detection() {
        assert!(has_windows_drive_prefix("C:\\Users\\me"));
        assert!(has_windows_drive_prefix("d:/projects"));
        assert!(has_windows_drive_prefix("Z:"));
        assert!(!has_windows_drive_prefix("/usr/local"));
        assert!(!has_windows_drive_prefix("~/x"));
        assert!(!has_windows_drive_prefix("C"));
        assert!(!has_windows_drive_prefix("1:\\nope"));
    }

    #[test]
    fn location_chosen_input_falls_back_to_typed_path() {
        let mut lp = location_picker(vec![location_candidate("/home/me/alpha", "alpha")]);
        // A path with no matching suggestion → the raw typed path is used.
        lp.picker.set_query("/no/such/dir");
        assert_eq!(lp.chosen_input().as_deref(), Some("/no/such/dir"));
    }

    #[test]
    fn location_chosen_input_uses_selected_recent() {
        let mut lp = location_picker(vec![
            location_candidate("/home/me/alpha", "alpha"),
            location_candidate("/home/me/beta", "beta"),
        ]);
        lp.picker.selected = 1;
        assert_eq!(lp.chosen_input().as_deref(), Some("/home/me/beta"));
    }

    #[test]
    fn location_path_completion_lists_filters_and_hides_dotdirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir(tmp.path().join("beta")).unwrap();
        std::fs::create_dir(tmp.path().join(".hidden")).unwrap();
        let mut lp = location_picker(vec![]);

        // Trailing slash → list the (non-hidden) subdirs.
        lp.picker.set_query(format!("{}/", tmp.path().display()));
        lp.refresh_suggestions();
        let labels = visible_labels(&lp);
        assert!(labels.contains(&"alpha".to_string()), "got: {labels:?}");
        assert!(labels.contains(&"beta".to_string()), "got: {labels:?}");
        assert!(
            !labels.contains(&".hidden".to_string()),
            "dotdirs hidden unless the partial starts with `.`, got: {labels:?}",
        );

        // Prefix filter on the final segment.
        lp.picker.set_query(format!("{}/al", tmp.path().display()));
        lp.refresh_suggestions();
        assert_eq!(visible_labels(&lp), vec!["alpha"]);

        // A leading dot in the partial reveals dot-directories.
        lp.picker.set_query(format!("{}/.h", tmp.path().display()));
        lp.refresh_suggestions();
        assert_eq!(visible_labels(&lp), vec![".hidden"]);
    }

    #[test]
    fn location_path_completion_tags_worktree_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("wt")).unwrap();
        std::fs::create_dir(tmp.path().join("plain")).unwrap();
        // Index `wt` as a managed worktree (keys are canonical paths).
        let canon = dunce::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_path_buf());
        let mut worktrees = std::collections::HashMap::new();
        worktrees.insert(canon.join("wt"), "my-feature".to_string());

        let mut lp = location_picker_with_worktrees(vec![], worktrees);
        lp.picker.set_query(format!("{}/", tmp.path().display()));
        lp.refresh_suggestions();

        let visible = lp.visible_candidates();
        let wt = visible.iter().find(|c| c.label == "wt").expect("wt listed");
        assert_eq!(wt.worktree.as_deref(), Some("my-feature"));
        let plain = visible
            .iter()
            .find(|c| c.label == "plain")
            .expect("plain listed");
        assert_eq!(plain.worktree, None);
    }

    /// A worktree directory that is itself a symlink still gets tagged:
    /// the index key is the canonical (real) path, so `read_subdirs` must
    /// canonicalize the entry (resolving the symlink), not just join the
    /// name to the canonical parent.
    #[cfg(unix)]
    #[test]
    fn location_path_completion_tags_symlinked_worktree() {
        let real = tempfile::tempdir().unwrap(); // the real worktree target
        let parent = tempfile::tempdir().unwrap(); // the dir we list
        std::os::unix::fs::symlink(real.path(), parent.path().join("link")).unwrap();

        // Index keyed by the real (canonical) path, as the worktree DB is.
        let real_canon = dunce::canonicalize(real.path()).unwrap();
        let mut worktrees = std::collections::HashMap::new();
        worktrees.insert(real_canon, "linked-wt".to_string());

        let mut lp = location_picker_with_worktrees(vec![], worktrees);
        lp.picker.set_query(format!("{}/", parent.path().display()));
        lp.refresh_suggestions();

        let link = lp
            .visible_candidates()
            .into_iter()
            .find(|c| c.label == "link")
            .expect("symlinked dir listed");
        assert_eq!(link.worktree.as_deref(), Some("linked-wt"));
    }

    #[test]
    fn click_on_location_label_opens_picker() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut state = DashboardState::new();
        state.location_hit.set(Some(Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 1,
        }));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            state.handle_mouse(&click),
            InputOutcome::Action(Action::DashboardOpenLocationPicker)
        ));
    }

    #[test]
    fn ctrl_l_opens_location_picker() {
        let mut state = DashboardState::new();
        let reg = crate::actions::ActionRegistry::defaults();
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(matches!(
            state.handle_input(&Event::Key(key), &reg),
            InputOutcome::Action(Action::DashboardOpenLocationPicker)
        ));
    }

    #[test]
    fn location_picker_esc_closes() {
        // Pin vim-mode off; this test asserts the non-vim picker path.
        crate::appearance::cache::set_vim_mode(false);
        let mut state = DashboardState::new();
        state.location_picker = Some(location_picker(vec![location_candidate("/tmp", "tmp")]));
        let reg = crate::actions::ActionRegistry::defaults();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            state.handle_input(&Event::Key(esc), &reg),
            InputOutcome::Action(Action::DashboardCloseLocationPicker)
        ));
    }

    /// Editing the path clears a stale "Not a directory" error so it
    /// doesn't linger next to a corrected (possibly valid) input.
    #[test]
    fn location_picker_edit_clears_error() {
        let mut state = DashboardState::new();
        let mut lp = location_picker(vec![]);
        lp.error = Some("Not a directory: /bad".to_string());
        state.location_picker = Some(lp);
        let reg = crate::actions::ActionRegistry::defaults();

        // Typing a character edits the query → the error is dropped.
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let _ = state.handle_input(&Event::Key(key), &reg);
        assert!(
            state.location_picker.as_ref().unwrap().error.is_none(),
            "editing the path must clear the stale error",
        );
    }

    #[test]
    fn location_picker_enter_selects_recent() {
        let mut state = DashboardState::new();
        state.location_picker = Some(location_picker(vec![
            location_candidate("/home/me/alpha", "alpha"),
            location_candidate("/home/me/beta", "beta"),
        ]));
        state.location_picker.as_mut().unwrap().picker.selected = 1;
        let reg = crate::actions::ActionRegistry::defaults();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_input(&Event::Key(enter), &reg) {
            InputOutcome::Action(Action::DashboardChangeLocation { input }) => {
                assert_eq!(input, "/home/me/beta");
            }
            other => panic!("expected DashboardChangeLocation, got {other:?}"),
        }
    }

    #[test]
    fn location_picker_tab_fills_selected_path() {
        let mut state = DashboardState::new();
        state.location_picker = Some(location_picker(vec![
            // Paths outside $HOME so `display_path` leaves them absolute,
            // keeping the assertion independent of the test machine's home.
            location_candidate("/opt/projects/alpha", "alpha"),
            location_candidate("/opt/projects/beta", "beta"),
        ]));
        state.location_picker.as_mut().unwrap().picker.selected = 1;
        let reg = crate::actions::ActionRegistry::defaults();
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(
            state.handle_input(&Event::Key(tab), &reg),
            InputOutcome::Changed
        ));
        let lp = state.location_picker.as_ref().unwrap();
        assert_eq!(lp.picker.query(), "/opt/projects/beta/");
        assert_eq!(lp.picker.query_cursor(), lp.picker.query().len());
    }

    #[test]
    fn location_path_completion_enter_uses_selected_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        let mut lp = location_picker(vec![]);
        lp.picker.set_query(format!("{}/al", tmp.path().display()));
        lp.refresh_suggestions();

        let mut state = DashboardState::new();
        state.location_picker = Some(lp);
        let reg = crate::actions::ActionRegistry::defaults();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_input(&Event::Key(enter), &reg) {
            InputOutcome::Action(Action::DashboardChangeLocation { input }) => {
                let expected = tmp.path().join("alpha").to_string_lossy().into_owned();
                assert_eq!(input, expected);
            }
            other => panic!("expected DashboardChangeLocation, got {other:?}"),
        }
    }

    #[test]
    fn location_picker_typed_path_no_match_uses_raw_query() {
        let mut state = DashboardState::new();
        state.location_picker = Some(location_picker(vec![location_candidate(
            "/home/me/alpha",
            "alpha",
        )]));
        let reg = crate::actions::ActionRegistry::defaults();
        // A guaranteed-absent home path → no suggestion → the raw query is used.
        for c in "~/__nope_zzz__".chars() {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            let _ = state.handle_input(&Event::Key(key), &reg);
        }
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match state.handle_input(&Event::Key(enter), &reg) {
            InputOutcome::Action(Action::DashboardChangeLocation { input }) => {
                assert_eq!(input, "~/__nope_zzz__");
            }
            other => panic!("expected DashboardChangeLocation, got {other:?}"),
        }
    }

    fn lease_fixture_agent() -> (
        AgentId,
        indexmap::IndexMap<AgentId, crate::app::agent_view::AgentView>,
    ) {
        use crate::scrollback::block::RenderBlock;
        let id = AgentId(1);
        let mut agent = crate::test_util::make_agent_view(Some("s1"), "/tmp");
        agent.scrollback.push_block(RenderBlock::user_prompt("one"));
        agent
            .scrollback
            .push_block(RenderBlock::agent_message("long response body for wrap"));
        agent.scrollback.push_block(RenderBlock::user_prompt("two"));
        agent
            .scrollback
            .push_block(RenderBlock::agent_message("second reply"));
        agent.scrollback.prepare_layout(80, 24);
        agent.scrollback.set_selected(Some(0));
        agent.scrollback.set_scroll_offset(2);
        let mut agents = indexmap::IndexMap::new();
        agents.insert(id, agent);
        (id, agents)
    }

    #[test]
    fn peek_viewport_lease_restore_without_page_flip_keeps_pre_guest_nav() {
        let (id, mut agents) = lease_fixture_agent();
        let pre = agents[&id].scrollback.capture_viewport_snapshot();
        let mut dash = DashboardState::new();
        let row = DashboardRowId::TopLevel(id);
        dash.begin_peek_viewport(row, &mut agents);
        assert!(dash.peek_viewport.is_some());
        assert!(agents[&id].scrollback.is_follow_mode());
        assert!(
            agents
                .get_mut(&id)
                .unwrap()
                .scrollback
                .prepare_layout(40, 6),
            "guest width change is Case 1"
        );

        dash.restore_peek_viewport(&mut agents);
        assert!(dash.peek_viewport.is_none());
        let sb = &mut agents.get_mut(&id).unwrap().scrollback;
        assert_eq!(sb.scroll_offset(), pre.scroll_offset);
        assert_eq!(sb.is_follow_mode(), pre.follow_mode);
        assert_eq!(sb.selected(), pre.selected);
        assert!(
            sb.prepare_layout(80, 24),
            "restore must invalidate so full-width prepare is Case 1"
        );
        let snap = sb.capture_viewport_snapshot();
        assert_eq!(snap.last_width, 80);
    }

    #[test]
    fn peek_viewport_lease_page_flip_re_pins_entry_on_restore() {
        let (id, mut agents) = lease_fixture_agent();
        let mut dash = DashboardState::new();
        dash.begin_peek_viewport(DashboardRowId::TopLevel(id), &mut agents);
        let page_flip_entry = {
            let sb = &mut agents.get_mut(&id).unwrap().scrollback;
            sb.prepare_layout(40, 6);
            let last = sb.len().saturating_sub(1);
            let entry_id = sb.entry(last).unwrap().id;
            sb.set_selected(Some(last));
            sb.scroll_to_entry_top(last);
            sb.enable_follow_with_preserve();
            entry_id
        };
        assert!(agents[&id].scrollback.is_follow_preserve_scroll());
        dash.note_page_flip_for_lease(id, page_flip_entry, &agents);
        assert_eq!(
            dash.peek_viewport.as_ref().and_then(|l| l.page_flip_entry),
            Some(page_flip_entry)
        );

        dash.restore_peek_viewport(&mut agents);
        let sb = &agents[&id].scrollback;
        assert!(sb.is_follow_mode());
        assert!(sb.is_follow_preserve_scroll());
        assert_eq!(sb.selected(), Some(sb.len().saturating_sub(1)));
        let snap = sb.capture_viewport_snapshot();
        assert_eq!(snap.last_width, 80);
    }

    #[test]
    fn set_peek_none_does_not_clear_viewport_lease() {
        let (id, mut agents) = lease_fixture_agent();
        let mut dash = DashboardState::new();
        dash.begin_peek_viewport(DashboardRowId::TopLevel(id), &mut agents);
        assert!(dash.peek_viewport.is_some());
        dash.set_peek(None);
        assert!(dash.peek_viewport.is_some());
        dash.restore_peek_viewport(&mut agents);
        assert!(dash.peek_viewport.is_none());
    }

    #[test]
    fn sticky_begin_peek_does_not_recapture() {
        let (id, mut agents) = lease_fixture_agent();
        let mut dash = DashboardState::new();
        dash.begin_peek_viewport(DashboardRowId::TopLevel(id), &mut agents);
        let snap_offset = dash
            .peek_viewport
            .as_ref()
            .map(|l| l.snapshot.scroll_offset)
            .unwrap();
        agents.get_mut(&id).unwrap().scrollback.set_scroll_offset(0);
        dash.begin_peek_viewport(DashboardRowId::TopLevel(id), &mut agents);
        assert_eq!(
            dash.peek_viewport.as_ref().unwrap().snapshot.scroll_offset,
            snap_offset
        );
    }

    #[test]
    fn note_page_flip_only_when_row_and_entry_match() {
        let (id, mut agents) = lease_fixture_agent();
        let mut dash = DashboardState::new();
        dash.begin_peek_viewport(DashboardRowId::TopLevel(id), &mut agents);
        let entry_id = agents[&id].scrollback.entry(3).unwrap().id;
        agents
            .get_mut(&id)
            .unwrap()
            .scrollback
            .enable_follow_with_preserve();
        dash.note_page_flip_for_lease(AgentId(99), entry_id, &agents);
        assert!(
            dash.peek_viewport
                .as_ref()
                .unwrap()
                .page_flip_entry
                .is_none()
        );
        dash.note_page_flip_for_lease(id, crate::scrollback::EntryId::new(u64::MAX), &agents);
        assert!(
            dash.peek_viewport
                .as_ref()
                .unwrap()
                .page_flip_entry
                .is_none()
        );
        dash.note_page_flip_for_lease(id, entry_id, &agents);
        let lease = dash.peek_viewport.as_ref().unwrap();
        assert_eq!(lease.page_flip_entry, Some(entry_id));
        assert!(!lease.snapshot.follow_preserve_scroll);
        assert_eq!(lease.snapshot.selected, Some(0));
    }

    #[test]
    fn restore_ignores_page_flip_entry_removed_during_lease() {
        let (id, mut agents) = lease_fixture_agent();
        let pre = agents[&id].scrollback.capture_viewport_snapshot();
        let mut dash = DashboardState::new();
        dash.begin_peek_viewport(DashboardRowId::TopLevel(id), &mut agents);
        let entry_id = agents[&id].scrollback.entry(2).unwrap().id;
        agents
            .get_mut(&id)
            .unwrap()
            .scrollback
            .enable_follow_with_preserve();
        dash.note_page_flip_for_lease(id, entry_id, &agents);
        agents
            .get_mut(&id)
            .unwrap()
            .scrollback
            .remove_entry(entry_id);

        dash.restore_peek_viewport(&mut agents);

        assert!(dash.peek_viewport.is_none());
        assert_eq!(agents[&id].scrollback.selected(), pre.selected);
        assert_eq!(agents[&id].scrollback.is_follow_mode(), pre.follow_mode);
    }

    #[test]
    fn note_page_flip_ignores_subagent_lease_on_parent_agent() {
        let (id, mut agents) = lease_fixture_agent();
        let child = crate::test_util::make_agent_view(Some("child"), "/tmp");
        agents
            .get_mut(&id)
            .unwrap()
            .subagent_views
            .insert("child".into(), Box::new(child));
        let mut dash = DashboardState::new();
        dash.begin_peek_viewport(
            DashboardRowId::Subagent {
                parent: id,
                child_session_id: "child".into(),
            },
            &mut agents,
        );
        let entry_id = agents[&id].scrollback.entry(3).unwrap().id;
        dash.note_page_flip_for_lease(id, entry_id, &agents);
        assert!(
            dash.peek_viewport
                .as_ref()
                .unwrap()
                .page_flip_entry
                .is_none(),
            "parent drain must not write parent entries onto a subagent lease"
        );
        agents
            .get_mut(&id)
            .unwrap()
            .scrollback
            .enable_follow_with_preserve();
        dash.note_page_flip_for_lease(id, entry_id, &agents);
        assert!(
            dash.peek_viewport
                .as_ref()
                .unwrap()
                .page_flip_entry
                .is_none()
        );
    }
}
