//! Root view component.
//!
//! [`AppView`] owns all application state and provides the top-level
//! `handle_input()` and `draw()` methods. The event loop calls these
//! and knows nothing about input routing, overlays, or view internals.
use super::ScreenMode;
use crate::acp::model_state::ModelState;
use crate::actions::{ActionId, ActionRegistry, When};
use crate::appearance::AppearanceConfig;
use crate::input::KeyboardNormalizer;
use crate::input::key::KeyShortcut;
use crate::input::line_editor::{LineEditOutcome, LineEditor};
use crate::input::mouse::{MouseScrollState, ScrollConfig, ScrollDirection};
use crate::key;
use crate::notifications::NotificationService;
use crate::render::draw::CursorState;
use crate::scrollback::render::ScratchBuffer;
use crate::views::prompt_widget::PromptWidget;
use crate::views::welcome::WelcomePromptFocus;
use agent_client_protocol as acp;
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use indexmap::IndexMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use xai_acp_lib::AcpAgentTx;
/// State for the "New Worktree" popup dialog on the welcome screen.
#[derive(Debug, Default)]
pub struct NewWorktreeDialogState {
    /// Text input for the worktree label (empty = auto-generated name).
    label: LineEditor,
}
const MAX_WORKTREE_LABEL_BYTES: usize = 100;
impl NewWorktreeDialogState {
    pub fn new() -> Self {
        Self {
            label: LineEditor::default(),
        }
    }
    pub fn label(&self) -> &str {
        self.label.text()
    }
    pub(crate) fn viewport(&self, width: usize) -> xai_ratatui_textarea::SingleLineViewport {
        self.label.viewport(width)
    }
    #[cfg(test)]
    pub(crate) fn set_label(&mut self, label: impl Into<String>) {
        self.label.set_text(label);
    }
    #[cfg(test)]
    pub(crate) fn set_cursor_byte(&mut self, cursor_byte: usize) -> LineEditOutcome {
        self.label.set_cursor_byte(cursor_byte)
    }
    pub fn insert_paste(&mut self, text: &str) -> NewWorktreeDialogOutcome {
        Self::from_line_edit(
            self.label
                .insert_paste_with_byte_limit(text, MAX_WORKTREE_LABEL_BYTES),
        )
    }
    /// Handle a key event. Returns the dialog outcome.
    pub fn handle_key(&mut self, key: &crossterm::event::KeyEvent) -> NewWorktreeDialogOutcome {
        use crossterm::event::{KeyCode, KeyModifiers};
        if crate::input::key::is_paste_key(key) {
            return crate::clipboard::system_clipboard_get()
                .map_or(NewWorktreeDialogOutcome::Unchanged, |text| {
                    self.insert_paste(&text)
                });
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !crate::input::key::is_altgr(key.modifiers)
            && matches!(key.code, KeyCode::Char('c' | 'd' | 'q'))
        {
            return NewWorktreeDialogOutcome::Cancelled;
        }
        if key.code == KeyCode::Enter && !key.modifiers.is_empty() {
            return NewWorktreeDialogOutcome::Unchanged;
        }
        match key.code {
            KeyCode::Enter if key.modifiers.is_empty() => {
                let label = self.label().trim().to_string();
                NewWorktreeDialogOutcome::Submitted(if label.is_empty() {
                    None
                } else {
                    Some(label)
                })
            }
            KeyCode::Esc => NewWorktreeDialogOutcome::Cancelled,
            _ => {
                let remaining = MAX_WORKTREE_LABEL_BYTES.saturating_sub(self.label().len());
                let outcome = self.label.handle_key_with_insert_policy(key, |character| {
                    character.len_utf8() <= remaining
                });
                Self::from_line_edit(outcome)
            }
        }
    }
    fn from_line_edit(outcome: LineEditOutcome) -> NewWorktreeDialogOutcome {
        match outcome {
            LineEditOutcome::TextChanged
            | LineEditOutcome::CursorChanged
            | LineEditOutcome::HandledNoChange => NewWorktreeDialogOutcome::Changed,
            LineEditOutcome::Unhandled => NewWorktreeDialogOutcome::Unchanged,
        }
    }
}
/// Per-visit announcement UI state on the welcome screen. Reset on every
/// return-to-welcome transition (see `show_welcome`) so a previously expanded
/// announcement can't leak into a freshly shown screen; the non-`expanded`
/// fields are recomputed each frame, so resetting them is harmless.
#[derive(Debug, Default)]
pub struct WelcomeAnnouncementState {
    /// Whether a long announcement is expanded inline (default: 2 lines + `…`).
    pub expanded: bool,
    /// Mouse last over the announcement block (drives hover color + redraws).
    pub on_cta: bool,
    /// Whether the announcement overflowed (the "expandable" signal).
    pub truncated: bool,
    /// Hit-test rect for the full announcement block (click anywhere to toggle).
    pub rect: Option<ratatui::layout::Rect>,
}
/// Outcome of handling input in the new-worktree dialog.
#[derive(Debug)]
pub enum NewWorktreeDialogOutcome {
    /// User pressed Enter — create the worktree.
    /// `None` means auto-generate the name.
    Submitted(Option<String>),
    /// User pressed Esc — close without creating.
    Cancelled,
    /// Input changed (redraw needed).
    Changed,
    /// Nothing happened.
    Unchanged,
}
/// Persisted worktree preference for `/new` and `/fork`.
///
/// Controls whether the worktree question popup is shown when starting a
/// new session or forking. Each command has its own config key:
/// - `[hints] new_session_worktree_mode` (default: `ask`)
/// - `[hints] fork_worktree_mode` (default: `ask`)
///
/// The legacy `[hints] worktree_mode` key is read as a fallback when
/// neither per-command key is set.
///
/// Startup resolution lives in
/// [`xai_grok_shell::util::config::resolve_hints`]; this type is the pager's
/// in-memory mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeMode {
    /// Always show the popup.
    Ask,
    /// Always create a worktree, skip the popup.
    Always,
    /// Never create a worktree, skip the popup.
    Never,
}
impl From<xai_grok_shell::util::config::WorktreeHintMode> for WorktreeMode {
    fn from(mode: xai_grok_shell::util::config::WorktreeHintMode) -> Self {
        use xai_grok_shell::util::config::WorktreeHintMode;
        match mode {
            WorktreeHintMode::Ask => Self::Ask,
            WorktreeHintMode::Always => Self::Always,
            WorktreeHintMode::Never => Self::Never,
        }
    }
}
impl WorktreeMode {
    /// Parse from a TOML string value. Unrecognised values fall back to
    /// [`WorktreeMode::Never`] with a debug-level log.
    pub fn from_config_str(s: &str) -> Self {
        xai_grok_shell::util::config::WorktreeHintMode::from_config_str(s).into()
    }
    /// Serialise to the TOML string representation.
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
    /// Resolve per-command worktree modes from a parsed TOML document.
    ///
    /// Returns `(new_session_worktree_mode, fork_worktree_mode)`.
    ///
    /// Resolution order:
    /// - `/new`: `new_session_worktree_mode` key, else legacy `worktree_mode`, else `Never` (no popup).
    /// - `/fork`: `fork_worktree_mode` key, else legacy `worktree_mode`, else `Ask`.
    pub fn resolve_from_hints(hints: Option<&toml_edit::Item>) -> (Self, Self) {
        let get_str = |key: &str| -> Option<Self> {
            hints
                .and_then(|h| h.get(key))
                .and_then(|v| v.as_str())
                .map(Self::from_config_str)
        };
        Self::resolve_from_hint_strings(get_str)
    }
    /// Same as [`Self::resolve_from_hints`], for merged effective config (`toml::Value`).
    pub fn resolve_from_hints_value(hints: Option<&toml::Value>) -> (Self, Self) {
        let (new_session, fork) =
            xai_grok_shell::util::config::WorktreeHintMode::resolve_pair(hints);
        (new_session.into(), fork.into())
    }
    fn resolve_from_hint_strings(get_str: impl Fn(&str) -> Option<Self>) -> (Self, Self) {
        let legacy = get_str("worktree_mode");
        let new_session = get_str("new_session_worktree_mode")
            .or(legacy)
            .unwrap_or(Self::Never);
        let fork = get_str("fork_worktree_mode")
            .or(legacy)
            .unwrap_or(Self::Ask);
        (new_session, fork)
    }
}
use super::PagerTerminal;
use super::actions::Action;
use super::agent::AgentId;
use super::agent_view::{AgentView, McpInitProgress};
use super::bundle::BundleState;
/// Which view is currently displayed.
///
/// Note: `AgentDashboard` does not carry state directly because
/// `DashboardState` is not `Copy`. The dashboard view-state lives on
/// `AppView::dashboard` and is only "active" when `active_view == AgentDashboard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveView {
    Welcome,
    Agent(AgentId),
    /// The top-level Agent Dashboard. State lives in `AppView::dashboard`.
    AgentDashboard,
}
/// Target restored when leaving the dashboard (Ctrl+\ / Esc).
/// Consumed by `dispatch_exit_dashboard`; dead agents fall back to
/// insertion-order first / Welcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardReturn {
    /// Plain agent view (no session-overlay chrome).
    Agent(AgentId),
    /// Session overlay: re-set `attached_agent` on the way back.
    Overlay(AgentId),
}
impl DashboardReturn {
    pub fn agent_id(self) -> AgentId {
        match self {
            Self::Agent(id) | Self::Overlay(id) => id,
        }
    }
    pub fn is_overlay(self) -> bool {
        matches!(self, Self::Overlay(_))
    }
}
/// Tick cadence demanded by the current view state — see
/// [`AppView::tick_demand`]. Ordered: `None < Slow < Fast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TickDemand {
    /// Nothing animates or polls: the event loop parks (zero wakeups).
    None,
    /// Only low-frequency work is pending (welcome logo shimmer at ~12fps,
    /// the macOS Cmd link-hover poll): tick at [`SLOW_TICK_INTERVAL`].
    Slow,
    /// Real animation is on screen: tick at the configured animation fps.
    Fast,
}
/// Tick cadence for [`TickDemand::Slow`] (~12fps). Matches the welcome logo's
/// `SHIMMER_FPS` so slow ticks sample every shimmer frame, and bounds the
/// latency of the macOS Cmd link-hover underline.
pub const SLOW_TICK_INTERVAL: Duration = Duration::from_millis(83);
/// Which prompt box in-flight voice dictation appends its finalized text to.
/// Captured when recording **starts** so a trailing STT final still lands where
/// the user was dictating, even if they navigate away — or toggle a dashboard
/// row's peek panel — mid-utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceTarget {
    /// A live agent session's prompt box.
    Agent(AgentId),
    /// The dashboard's new-agent dispatch input (no row peek was open at start).
    DashboardDispatch,
    /// The dashboard's peek reply input, bound to the agent whose peek was open
    /// at start. The id pins the row: selecting a different row mid-utterance
    /// stops capture (the reply widget is shared and clears on row change), so a
    /// final can't land on the wrong agent's reply.
    DashboardPeekReply(AgentId),
}
/// The voice-dictation lifecycle. Exactly one state holds at a time, so the
/// "is the mic live / is a start queued / does a Ctrl+Space hold own it / which
/// box receives finals" facts can never disagree (they once lived as separate
/// booleans and repeatedly drifted apart). All transitions go through the
/// `AppView::voice_*` methods.
///
/// `hold` marks a session begun by a Ctrl+Space hold-press: its matching
/// Ctrl+Space release ends it (and only it), while `/voice` / toggle sessions
/// leave `hold` false so a Ctrl+Space release can't touch them. `target` is the
/// prompt box bound at
/// **start**, so a trailing final lands where the user was dictating. `interim`
/// (the live partial transcript) lives inside the recording states so it can't
/// linger as a stale overlay once dictation ends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum VoiceState {
    /// No dictation in flight.
    #[default]
    Idle,
    /// A start was requested before the lazy pipeline existed; the event loop
    /// spawns it once and then opens the mic.
    ColdStart { hold: bool, target: VoiceTarget },
    /// Mic is open and streaming audio to STT.
    Recording {
        hold: bool,
        target: VoiceTarget,
        interim: Option<String>,
    },
    /// Capture was explicitly stopped (Esc / Ctrl+Space / [stop] / Ctrl+Space
    /// release), but the target — and the last interim — are kept so a trailing
    /// STT final still lands without the overlay flickering in the meantime.
    Stopping {
        target: VoiceTarget,
        interim: Option<String>,
    },
}
impl VoiceState {
    /// Mic is live (the `Recording` state).
    pub fn listening(&self) -> bool {
        matches!(self, Self::Recording { .. })
    }
    /// A start is queued for the lazy pipeline (the `ColdStart` state).
    pub fn pending_cold_start(&self) -> bool {
        matches!(self, Self::ColdStart { .. })
    }
    /// The prompt box that owns this session's dictation, if any.
    pub fn target(&self) -> Option<VoiceTarget> {
        match self {
            Self::ColdStart { target, .. }
            | Self::Recording { target, .. }
            | Self::Stopping { target, .. } => Some(*target),
            Self::Idle => None,
        }
    }
    /// The live partial transcript shown in the prompt overlay, if any.
    pub fn interim(&self) -> Option<&str> {
        match self {
            Self::Recording { interim, .. } | Self::Stopping { interim, .. } => interim.as_deref(),
            _ => None,
        }
    }
    /// Whether a hold-press owns the current session (so its key release ends
    /// it). `/voice` and toggle-style starts leave this false.
    pub(crate) fn hold(&self) -> bool {
        matches!(
                    self, Self::ColdStart { hold, .. } | Self::Recording { hold, .. }
        if * hold
                )
    }
}
/// Entry in the session picker list on the welcome screen.
#[derive(Debug, Clone)]
pub struct SessionPickerEntry {
    pub id: String,
    pub summary: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub cwd: String,
    pub hostname: Option<String>,
    pub source: String,
    pub model_id: Option<String>,
    pub num_messages: usize,
    /// When the session last had content added (most recent of local and remote).
    pub last_active_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Git branch associated with the session (if available from the server response).
    pub branch: Option<String>,
    /// Repo display name derived from the CWD path (last 2 path components joined by `-`).
    pub repo_name: String,
    /// Human-readable worktree label (if the session was created in a named worktree).
    pub worktree_label: Option<String>,
    /// Lazy-loaded detail for the expanded card view.
    pub card_detail: Option<CardDetail>,
}
/// Detail loaded on-demand when a session card is expanded.
#[derive(Debug, Clone)]
pub struct CardDetail {
    pub turn_count: usize,
    pub tool_call_count: usize,
    pub first_prompt_preview: String,
}
/// Authentication state for the welcome screen.
///
/// Drives the login flow UI and input routing on the welcome screen.
#[derive(Debug)]
pub enum AuthState {
    /// No login required (API key, cached token, or already authenticated).
    Done,
    /// Login required -- show login menu on welcome screen.
    /// `error` is set after a failed auth attempt so the user sees what went wrong.
    Pending { error: Option<String> },
    /// Auth flow is in progress.
    Authenticating {
        /// Sequence number for this auth attempt (stale results are ignored).
        request_seq: u64,
        /// Abort handle for the in-flight Authenticate task.
        handle: Option<tokio::task::AbortHandle>,
        /// Auth URL from the provider (populated by AuthUrlReady).
        auth_url: Option<String>,
        /// How the auth flow presents itself to the user.
        mode: AuthMode,
    },
}
/// How the auth flow presents itself to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Mode not yet determined (waiting for auth URL response).
    Pending,
    /// Browser opened automatically by external provider.
    Command,
    /// Manual: user must visit URL and paste token.
    Loopback,
    /// RFC 8628 device flow: device code + copyable URL, no paste box.
    Device,
}
/// Folder-trust state for the welcome screen.
///
/// Mirrors [`AuthState`]: a welcome sub-state that drives the "Do you trust the
/// contents of this directory?" question and gates session creation until it is
/// answered. Seeded once before the first render from the pure
/// [`xai_grok_workspace::folder_trust::decide`] verdict; when the feature flag
/// is off `decide` returns trusted, so this is always [`TrustState::Done`].
#[derive(Debug)]
pub enum TrustState {
    /// No question needed (feature off, already trusted, nothing to gate) or
    /// the question has been answered. Session creation may proceed.
    Done,
    /// An untrusted folder with repo-local code-exec config: show the trust
    /// question and defer session creation until the user answers.
    Pending {
        /// The resolved workspace root (git root) that is trusted on accept and
        /// is shown in the question.
        workspace: std::path::PathBuf,
    },
}
/// Result of `handle_input`. Tells the event loop what to do next.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum InputOutcome {
    /// Dispatch this action, then redraw.
    Action(Action),
    /// Dispatch this action, then re-process the same event through the
    /// (now-changed) active view. Used when the welcome screen creates a
    /// new session on the first keystroke so the character lands in the
    /// session's prompt instead of being consumed.
    ActionThenForward(Action),
    /// Dispatch both actions in order, then redraw (e.g. revert preview
    /// + open reset confirm).
    ActionPair(Action, Action),
    /// Arm a double-press pending action (e.g. idle Esc clear/rewind).
    /// AppView installs [`PendingAction`]; second press within `ttl` fires
    /// `action`. `label: None` arms silently (no shortcuts-bar hint).
    ArmPending {
        action: Action,
        shortcut: KeyShortcut,
        label: Option<&'static str>,
        ttl: Duration,
    },
    /// Something changed visually (prompt text, scroll). Redraw needed.
    Changed,
    /// Nothing happened. Skip redraw to preserve cursor blink.
    Unchanged,
}
/// Immutable origin carried beside a paste event until its target consumes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PasteProvenance {
    /// Terminal bracketed paste or pager key-coalesced paste.
    Terminal,
    /// Linux X11 PRIMARY read triggered by one unmodified middle-button down.
    #[allow(dead_code)]
    X11Primary,
}
impl PasteProvenance {
    pub(crate) fn may_probe_clipboard_attachments(self) -> bool {
        matches!(self, Self::Terminal)
    }
}
/// A pending action awaiting double-press confirmation.
///
/// Set when a `requires_confirmation` action is triggered. The shortcuts bar
/// shows "press again to {label}" when [`Self::label`] is `Some`. If the same
/// key is pressed within the TTL, the action fires. Any other key or expiry
/// clears it. `label: None` = silent arm (idle-empty Esc→rewind).
pub struct PendingAction {
    /// The action to fire on second press.
    pub action: Action,
    /// The specific key that was pressed (narrowed from the binding).
    pub shortcut: KeyShortcut,
    /// When `Some`, shortcuts bar shows "press again to {label}".
    pub label: Option<&'static str>,
    /// When this pending action expires.
    pub expires_at: Instant,
}
impl PendingAction {
    pub const TTL: Duration = Duration::from_millis(1000);
    /// Double-press timeout for idle Esc clear / rewind arms.
    pub const ESC_DOUBLE_PRESS_TTL: Duration = Duration::from_millis(800);
    pub fn new(action: Action, shortcut: KeyShortcut, label: &'static str) -> Self {
        Self::with_ttl(action, shortcut, Some(label), Self::TTL)
    }
    /// Like [`Self::new`] but with an explicit confirm window. Used by
    /// the dashboard-overlay stop (Ctrl+X), which mirrors the
    /// dashboard's [`crate::views::dashboard::state::STOP_CONFIRM_WINDOW`]
    /// rather than the default double-press TTL.
    pub fn with_ttl(
        action: Action,
        shortcut: KeyShortcut,
        label: Option<&'static str>,
        ttl: Duration,
    ) -> Self {
        Self {
            action,
            shortcut,
            label,
            expires_at: Instant::now() + ttl,
        }
    }
    pub fn expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}
/// Cap for the `GROK_ESC_DOUBLE_PRESS_MS` override; the pty_e2e suite sets
/// exactly this value.
pub const ESC_DOUBLE_PRESS_TEST_MS: u64 = 60_000;
/// Idle-Esc double-press confirm window, `GROK_ESC_DOUBLE_PRESS_MS`-overridable
/// (read once, bounded). Test seam: a loaded pty_e2e shard's render round-trip
/// between the two presses can outlast the 800ms default and expire the arm.
pub(crate) fn esc_double_press_ttl() -> Duration {
    use std::sync::OnceLock;
    static TTL: OnceLock<Duration> = OnceLock::new();
    *TTL.get_or_init(|| parse_esc_ttl(std::env::var("GROK_ESC_DOUBLE_PRESS_MS").ok()))
}
/// Extracted pure (no `OnceLock`) so the bounds — zero/garbage → default,
/// oversized → clamp — are unit-testable.
fn parse_esc_ttl(raw: Option<String>) -> Duration {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(|ms| Duration::from_millis(ms.min(ESC_DOUBLE_PRESS_TEST_MS)))
        .unwrap_or(PendingAction::ESC_DOUBLE_PRESS_TTL)
}
/// Slash commands unavailable on the free and X Basic subscription tiers.
///
/// To restrict another command for these tiers, add its canonical name
/// (no leading `/`) here — matching covers aliases automatically via
/// [`crate::slash::registry::CommandRegistry::set_restricted_commands`].
///
/// Current set:
/// - `usage` — coding credit / billing UI (alias: `/cost`)
/// - `imagine` — image generation entry point
/// - `imagine-video` — video generation entry point
/// - `voice` — voice dictation entry point (the Ctrl+Space / F8 keybinding is
///   gated separately in [`crate::app::dispatch::voice`], since it bypasses the
///   slash registry)
pub(crate) const TIER_RESTRICTED_COMMANDS: &[&str] =
    &["usage", "imagine", "imagine-video", "voice"];
/// Whether a subscription-tier display name is a tier with restricted
/// commands: the free tier (no subscription ⇒ `None`, or an explicit
/// "Free") and X Basic (CCP display name "X Basic"; JWT claim fallback
/// "x_basic"). Everything else — paid tiers and unknown future names —
/// is unrestricted (fail-open).
///
/// The string classification is shared with the shell's capability
/// (toolset) gate via [`xai_grok_shell::tier::is_restricted_tier_name`] so
/// the two can't drift. The pager's *cosmetic* slash-command gate treats an
/// absent tier (`None`) as restricted (it recovers live on the next settings
/// update); the shell's capability gate treats absence as unrestricted.
fn is_restricted_tier(tier: Option<&str>) -> bool {
    match tier {
        None => true,
        Some(t) => xai_grok_shell::tier::is_restricted_tier_name(t),
    }
}
/// True for API-key labels from shell/CCP: `"ApiKey"`, `"API Key"`, `"api_key"`.
pub(crate) fn is_api_key_label(s: &str) -> bool {
    s.trim().to_ascii_lowercase().replace([' ', '_', '-'], "") == "apikey"
}
/// Pending re-exec into another screen mode (see `/minimal` / `/fullscreen`).
#[derive(Debug, Clone)]
pub struct ScreenModeRelaunch {
    /// `true` → `--minimal`; `false` → fullscreen (non-minimal).
    pub minimal: bool,
    /// Active session to reopen via `--resume`.
    pub session_id: String,
}
/// Root view component — owns all application state.
pub struct AppView {
    /// Which view is currently active.
    pub active_view: ActiveView,
    /// View to return to after a mid-session login flow completes or is
    /// cancelled. `Some` only while a `/login` (or 401-triggered re-auth)
    /// initiated from an active session is in progress — it lets the auth
    /// UI take over the `Welcome` screen and then restore the caller's view
    /// (e.g. `Agent`) afterwards. `None` at startup so the normal
    /// login-then-load flow is preserved.
    pub auth_return_view: Option<ActiveView>,
    /// Per-agent views (keyed by AgentId).
    pub agents: IndexMap<AgentId, AgentView>,
    /// Monotonically increasing counter for agent ID allocation.
    /// Never reuse IDs after `shift_remove` to avoid collisions.
    pub next_agent_id: usize,
    /// Available/selected models (shared across agents).
    pub models: ModelState,
    /// Keybinding definitions.
    pub registry: ActionRegistry,
    /// Settings registry — canonical metadata for user-tunable preferences.
    pub settings_registry: Arc<crate::settings::SettingsRegistry>,
    /// In-memory snapshot of the effective `UiConfig`. Seeded once at
    /// startup; updated synchronously by `set_X_inner` so dispatch
    /// stays sans-IO.
    pub current_ui: xai_grok_shell::agent::config::UiConfig,
    /// Working directory.
    pub cwd: PathBuf,
    /// Whether the project picker question has already been shown this session.
    pub project_picker_shown: bool,
    /// "Don't ask me again" opt-out from [`xai_grok_shell::util::config::resolve_hints`];
    /// TUI writes to user `config.toml` only.
    pub project_picker_disabled: bool,
    /// Whether the cwd is inside a git repository (any ancestor has `.git`).
    /// Pre-computed at startup so dispatch stays free of filesystem I/O.
    pub cwd_has_git_ancestor: bool,
    /// ACP channel for sending requests (shared resource, cloned into agents).
    pub acp_tx: AcpAgentTx,
    /// Local cache of bundle sync/status state from the shell.
    pub(crate) bundle_state: BundleState,
    /// Reusable scratch buffer for rendering.
    pub scratch: ScratchBuffer,
    /// Cursor state for blink-preserving cursor management.
    /// See [`crate::render::draw`] for the full rationale.
    pub cursor: CursorState,
    /// Pending double-press confirmation (quit, etc.).
    pub pending_action: Option<PendingAction>,
    /// Pending exit-session confirmation for slash command path.
    /// Set when `/home` is first typed; confirmed on second invocation within TTL.
    pub exit_session_pending: Option<Instant>,
    /// Mouse scroll normalization state (wheel/trackpad detection, acceleration).
    /// App-level because scroll is a physical input property, not per-agent.
    pub scroll_state: MouseScrollState,
    /// Scroll config derived from terminal detection.
    pub scroll_config: ScrollConfig,
    /// Current appearance config (hot-reloadable from ~/.grok/pager.toml).
    /// Stored here so new agents inherit the current config.
    pub appearance: AppearanceConfig,
    /// Notification service (terminal bell, OSC sequences, title updates).
    pub notification_service: NotificationService,
    /// Escape sequences (title, progress bar) accumulated by the last
    /// `update_notifications()` tick. Consumed by `draw()` and appended
    /// to the frame's `post_flush_escapes` so they are written inside the
    /// synchronized output block.
    pub(crate) pending_notification_escapes: Option<String>,
    /// Notification deferred by several ticks so the terminal has time to
    /// process the idle title escape before the notification fires.
    ///
    /// The idle title goes through the frame pipeline (writer thread
    /// channel), then Ghostty must read it from the PTY and apply it.
    /// Ghostty debounces `setTitle()` by 75 ms, so we need >75 ms
    /// before the notification reads `self.title` for the subtitle.
    /// 3 ticks × 33 ms ≈ 99 ms covers the debounce comfortably.
    ///
    /// The `u8` counts remaining ticks; the notification fires when it
    /// reaches 0.
    pub(crate) deferred_notification: Option<(crate::notifications::NotificationEvent, u8)>,
    /// Tracing log channel receiver. Set by the event loop after
    /// `init_tracing()`. Drained into `tracing_pane` each tick in debug/dev
    /// builds; otherwise drained-and-discarded.
    pub tracing_rx: Option<crate::tracing::LogRx>,
    /// Scroll-diagnostics HUD (`GROK_SCROLL_DEBUG` env / `/scroll-debug`).
    /// Release-compiled behind its runtime gate — see the module doc.
    pub scroll_debug_hud: crate::views::scroll_debug_hud::ScrollDebugHud,
    /// Release-safe FPS HUD (`/debug fps`; `GROK_FPS` env on release
    /// builds, where the dev overlay is compiled out) — see the module doc.
    pub fps_hud: crate::views::fps_hud::FpsHud,
    pub active_announcements: Vec<xai_grok_announcements::RemoteAnnouncement>,
    /// Persisted hide keys, filtered at the banner selection gate — hiding one
    /// critical reveals the next unhidden one, and a NEW id re-arms the banner.
    pub hidden_announcement_ids: std::collections::BTreeSet<String>,
    pub announcements_last_gen: u64,
    /// Selected welcome announcement for this pager launch.
    pub announcement: Option<xai_grok_announcements::RemoteAnnouncement>,
    /// Cached changelog markdown (for `/release-notes`). Populated by
    /// `FetchChangelog` at startup; `None` until the fetch completes.
    pub changelog_markdown: Option<String>,
    /// Cached changelog bullets (for welcome screen). Populated by
    /// `FetchChangelog` at startup; empty until the fetch completes.
    pub changelog_bullets: Vec<String>,
    /// Resolved tip list from config layers.
    pub tips: Vec<String>,
    /// Selected tip for the current launch/session.
    pub tip: Option<String>,
    /// Whether to show the resolved model ID in /session-info output.
    pub show_resolved_model: bool,
    /// Whether the `/share` slash command is available. Gated by
    /// `RemoteSettings.sharing_enabled`; defaults to `false` when remote
    /// settings are unavailable or the field is absent.
    pub sharing_enabled: bool,
    /// Whether the plugin marketplace CTA is enabled. Env `GROK_PLUGIN_CTA`
    /// overrides `RemoteSettings.plugin_cta` (remote settings); defaults to `false`.
    pub plugin_cta_enabled: bool,
    /// Whether the `/usage` slash command is available. Hidden for team
    /// (`team_name.is_some()`) and API-key auth.
    pub usage_visible: bool,
    /// Slash commands denied for the current subscription tier
    /// ([`TIER_RESTRICTED_COMMANDS`] when the user is on the free / X Basic
    /// tier, empty otherwise). Recomputed by [`Self::apply_tier_restrictions`]
    /// and fanned out to every slash registry (welcome prompt, agents,
    /// dashboard); deny wins over all other visibility gates.
    pub tier_restricted_commands: Vec<String>,
    /// Whether the pager is connected via a leader (leader mode). The Agent
    /// Dashboard entry points (`/dashboard`, `Ctrl+\`, `grok dashboard`, the
    /// startup hook) are only meaningful when a leader is coordinating a
    /// fleet of sessions, so they are gated on this flag. Set in
    /// `event_loop::run` from `connection.leader_status_rx.is_some()`;
    /// defaults to `false` (non-leader, dashboard hidden).
    pub leader_mode: bool,
    /// App-level credit balance used to show the usage warning on the
    /// welcome screen before any agent session exists.
    pub credit_balance: Option<crate::views::credit_bar::CreditBalance>,
    /// App-level auto top-up rule paired with `credit_balance` for the warning.
    pub auto_topup: Option<crate::views::credit_bar::AutoTopupInfo>,
    /// Periodic billing poll requested (credits >= 99%).
    pub billing_poll_wanted: bool,
    /// Leader-mode session roster (FleetView dashboard). Populated from
    /// `x.ai/sessions/list` polls and `x.ai/sessions/changed` broadcasts.
    /// Empty in non-leader mode, which naturally gates roster rendering.
    pub leader_roster: Vec<crate::app::roster::RosterEntry>,
    /// Local on-disk session list (dormant/idle sessions) surfaced on the
    /// dashboard when NOT in leader mode. There is no live leader roster to
    /// poll outside leader mode, so we fetch the same `x.ai/session/list` the
    /// resume picker uses and render those as idle rows. Entries are stored as
    /// [`crate::app::roster::RosterEntry`] (activity `Dormant`) so they reuse
    /// the existing roster-row rendering / attach path. Empty in leader mode.
    pub dashboard_local_sessions: Vec<crate::app::roster::RosterEntry>,
    /// Whether the dashboard is currently loading local sessions (non-leader mode).
    pub dashboard_sessions_loading: bool,
    /// Server-authoritative shared prompt queues, keyed by `sessionId`
    /// Reconciled from `x.ai/queue/changed` broadcasts so
    /// every client renders the same ordered queue (including prompts queued
    /// by other clients). Empty in non-leader mode.
    pub shared_prompt_queues:
        std::collections::HashMap<String, Vec<crate::app::prompt_queue::QueueEntryWire>>,
    /// Optimistic echo rows for prompts the pager sent server-authoritatively
    /// (plain prompt typed while a turn is running) but for which the
    /// confirming `x.ai/queue/changed` broadcast has not yet arrived. Keyed by
    /// `sessionId`. Pinned into `shared_prompt_queues` on reconcile so the row
    /// doesn't flicker, and dropped once the authoritative broadcast reflects
    /// the id (or it starts running). Never persisted.
    pub optimistic_prompt_echoes:
        std::collections::HashMap<String, Vec<crate::app::prompt_queue::QueueEntryWire>>,
    /// Server-authoritative running prompts that drained into the running slot
    /// while the previous turn was still finishing locally (handoff race).
    /// Keyed by `AgentId`. Consumed by the `PromptResponse` handler after
    /// `finish_turn` clears `current_prompt_id`, which then adopts the prompt
    /// and runs the turn-start shim. Never persisted.
    pub(crate) pending_running_adoptions:
        std::collections::HashMap<AgentId, crate::app::acp_handler::PendingRunningAdoption>,
    /// Whether the session picker groups entries by repo name with
    /// non-selectable headers. Gated by `GROK_SESSION_PICKER_GROUPED` env var
    /// or remote settings `session_picker_grouped`; defaults to `false`.
    pub session_picker_grouped: bool,
    /// Whether Ctrl+C before first server activity rewinds the prompt
    /// back into the input box. Gated by `GROK_CANCEL_REWIND` env /
    /// `[features] cancel_rewind` config / remote settings flag.
    pub cancel_rewind_enabled: bool,
    /// Whether session recap (`/recap` + automatic away recap) is rolled out,
    /// resolved by the shell and advertised on ACP initialize (`sessionRecap`).
    /// When false, the pager must not request recaps (zero `x.ai/recap` traffic).
    pub session_recap_available: bool,
    /// Stateful prompt widget rendered on the welcome screen (persists input across frames).
    pub welcome_prompt: PromptWidget,
    /// The single slash-command MRU/recency store. Owned here and injected
    /// into every agent prompt and the dashboard dispatch via
    /// [`PromptWidget::adopt_slash_mru`] so command recency is shared across
    /// surfaces (single-threaded UI; no process-global singleton).
    pub(crate) slash_mru: std::rc::Rc<std::cell::RefCell<crate::slash::mru::SlashMru>>,
    /// Whether the welcome screen prompt is currently capturing focus (user typed in it).
    /// When true, menu shortcuts like n/w/q are disabled and Escape unfocuses the prompt.
    pub welcome_prompt_focused: bool,
    /// Sticky flag: set once the user types in the welcome prompt, hides the
    /// tip for the rest of the session (even if the input is cleared).
    pub welcome_tip_typing_dismissed: bool,
    /// Effects queued by notification handlers (drained by the event loop).
    pub pending_effects: Vec<crate::app::actions::Effect>,
    /// Path to open in `$EDITOR` after the current event cycle completes.
    /// Set by `Action::SuspendForEditor`; consumed by the event loop which
    /// leaves the alternate screen, disables raw mode, spawns the editor,
    /// waits for it to exit, then restores the TUI.
    pub pending_editor_path: Option<std::path::PathBuf>,
    /// After `$EDITOR` exits, refresh the agents modal tab list if still open.
    pub pending_agents_modal_refresh: Option<crate::views::agents_modal::AgentsTab>,
    /// Path to open in `$PAGER` (default `less`) after the current event cycle.
    /// Set by `Action::OpenTranscriptPager` (`/transcript`); consumed by the
    /// event loop which suspends the inline TUI, spawns the pager, then restores
    /// and deletes the temp file. Primarily for minimal mode (no interactive
    /// scrollback pane), but works in every mode.
    pub pending_pager_path: Option<std::path::PathBuf>,
    /// Whether [`pending_pager_path`](Self::pending_pager_path) holds an
    /// ANSI-colored file (the minimal "full view" transcript). When true the
    /// event loop ensures the pager renders raw control codes (`less -R`) so the
    /// colors show instead of literal escapes. Plain-text transcripts (`/export`
    /// markdown) leave this false.
    pub pending_pager_ansi: bool,
    /// Minimal mode only: the Ctrl+T **force-show** pin for the todo panel.
    /// Minimal-mode-only per-session state, consolidated into a single field so
    /// the central `AppView` isn't peppered with loose minimal flags. Default-
    /// empty and inert outside `--minimal`; the `xai-grok-pager-minimal` crate
    /// reads/mutates it through the `crate::minimal_api` accessors. See
    /// [`crate::minimal_api::MinimalState`].
    pub(crate) minimal_state: crate::minimal_api::MinimalState,
    /// Currently highlighted menu item on the welcome screen (arrow keys / hover).
    pub welcome_menu_index: Option<usize>,
    /// Hit-test rects for welcome menu items (populated during render).
    pub welcome_menu_rects: Vec<ratatui::layout::Rect>,
    /// Whether the welcome menu currently includes a "Changelog" row (above
    /// Quit). Set during render; the input handler uses it to size the menu and
    /// map the extra row to the release-notes action.
    pub welcome_show_changelog_action: bool,
    /// Hit-test rect for the import-claude banner on the welcome screen.
    pub welcome_import_banner_rect: Option<ratatui::layout::Rect>,
    /// Last known mouse position (column, row), updated on every Mouse event.
    /// Used by the welcome screen to render fine-grained hover effects (e.g.
    /// brighter red on the import row's `[x]` when the mouse is exactly on
    /// those cells).
    pub last_mouse_pos: Option<(u16, u16)>,
    /// Origin (column, row) of the in-progress scroll gesture. Reused by
    /// `update_tick`'s residual flush so sub-line carry / stream-gap flushes
    /// route via `hit_test` to the originating pane instead of leaking into
    /// scrollback. Cleared on the finalize-transition tick.
    pub last_scroll_pos: Option<(u16, u16)>,
    /// Last off-screen render-cache eviction sweep (see
    /// [`Self::maybe_evict_offscreen_caches`]).
    pub(super) last_cache_evict_at: Option<Instant>,
    /// Hit-test rect for welcome prompt input (populated during render).
    pub welcome_prompt_rect: Option<ratatui::layout::Rect>,
    /// Hit-test rect for the auth URL (click-to-open during Authenticating).
    pub welcome_auth_url_rect: Option<ratatui::layout::Rect>,
    /// Whether the mouse pointer was last over the auth URL (for OSC 22 cursor shape).
    pub welcome_on_auth_url: bool,
    /// Mouse last over the changelog block (drives hover color + redraws).
    pub welcome_on_changelog_cta: bool,
    /// Per-visit announcement UI state on the welcome screen (expansion, hover,
    /// overflow flag, hit-rect).
    pub welcome_announcement: WelcomeAnnouncementState,
    /// Hit-test rect for the "show full URL" fallback link.
    pub welcome_auth_fallback_rect: Option<ratatui::layout::Rect>,
    /// Hit-test rect for the "[Refresh]" button on the paywall tier line.
    pub welcome_refresh_rect: Option<ratatui::layout::Rect>,
    /// Hit-test rect for the gate URL link on the paywall CTA.
    pub welcome_gate_url_rect: Option<ratatui::layout::Rect>,
    /// Hit-test rect for the welcome hero upgrade CTA `[label]` button
    /// (click → `AnnouncementsOpenCta(Welcome)`).
    pub welcome_upgrade_cta_rect: Option<ratatui::layout::Rect>,
    /// Sticky hover flag for the welcome upgrade CTA (redraw on enter/leave).
    pub welcome_on_upgrade_cta: bool,
    /// Hit-test rect for the clickable changelog info block (opens release notes).
    pub welcome_changelog_cta_rect: Option<ratatui::layout::Rect>,
    /// Show the raw auth URL with mouse capture disabled for manual copy.
    pub auth_show_raw_url: bool,
    /// Whether mouse capture is currently disabled for raw URL mode.
    pub auth_mouse_disabled: bool,
    /// Fetched session list for the session picker (None = not yet fetched).
    pub session_picker_entries: Option<Vec<SessionPickerEntry>>,
    /// Whether the session list is currently being fetched.
    pub session_picker_loading: bool,
    /// Unified picker state for the session picker.
    pub session_picker_state: crate::views::picker::PickerState,
    /// Source filter for the welcome-screen session picker.
    pub session_picker_source_filter: crate::views::session_picker::SourceFilter,
    /// Content-based (deep search) results from ACP session search.
    pub session_picker_content_results:
        Option<Vec<xai_grok_shell::extensions::session_search::SearchSessionHit>>,
    /// Whether a deep search is currently in flight.
    pub session_picker_content_loading: bool,
    /// Monotonically increasing sequence number for deep search requests.
    pub session_picker_deep_search_seq: u64,
    /// Monotonically increasing sequence number for session list fetches
    /// (`Effect::FetchSessionList`): only the seq-current response is
    /// applied, so a stale completion can't clobber newer results. Bumped
    /// only under chat mode (server-search supersede); in Build mode it
    /// stays 0 so plain list responses keep their pre-existing
    /// last-write-wins behavior.
    pub session_picker_list_seq: u64,
    /// Resolved compat-session cells used before checking resume-skill paths.
    pub(crate) foreign_session_compat:
        xai_grok_workspace::foreign_sessions::EnabledForeignSessionSources,
    /// Monotonic picker scan sequence, bumped on every open and close.
    pub(crate) foreign_session_scan_seq: u64,
    /// Coalesces obsolete foreign scans across welcome and modal pickers.
    pub(crate) foreign_scan_coordinator: crate::app::ForeignScanCoordinator,
    /// Foreign lane completion and deferred native-lane notice.
    pub(crate) session_picker_lanes: crate::views::session_picker::SessionPickerLanes,
    /// Invalidates detail reads when picker rows or filters change.
    pub(crate) session_picker_detail_generation: u64,
    /// The search query `session_picker_entries` were server-fetched with
    /// (`None` = unfiltered fetch). Via
    /// [`crate::views::session_picker::effective_filter_query`], skips the
    /// local fuzzy re-filter for server search results.
    pub session_picker_entries_query: Option<String>,
    /// Tick counter for welcome screen spinner animation.
    pub welcome_tick: u64,
    /// Last shimmer frame drawn on the welcome screen. Lets `tick` throttle the
    /// wall-clock logo animation to a few fps instead of the full tick rate.
    pub welcome_shimmer_frame: u64,
    /// CLI model override (`-m` / `--model`). Seeded into every new
    /// `AgentSession.deferred_model_switch` so the model is applied once
    /// the session is created.
    pub cli_model_override: Option<acp::ModelId>,
    /// CLI effort token (`--reasoning-effort` / `--effort`). Applied on session create.
    pub cli_effort_token: Option<String>,
    /// Default YOLO for new sessions, seeded at startup from `effective_yolo_for_launch`.
    pub default_yolo: bool,
    /// Soft-default still owns the mode: settings/update may rewrite UI +
    /// `default_yolo`. Cleared on user Shift+Tab / settings / CLI claim.
    /// Not inferred from the rendered permission string.
    pub permission_mode_from_soft_default: bool,
    /// Whether the **auto** permission-mode feature gate is enabled (resolved at
    /// startup from env / `[auto_mode] enabled` / remote settings, default OFF). When
    /// `false`, the Shift+Tab cycle skips Auto. See
    /// `xai_grok_shell::util::config::resolve_auto_permission_mode_enabled`.
    pub auto_mode_gate: bool,
    /// Managed-policy pin (set at startup); gates every runtime always-approve enable.
    pub yolo_policy_block: Option<&'static str>,
    /// One-shot notice that a launch `--yolo` was pinned off; shown on the first agent view.
    pub yolo_launch_block_notice: Option<&'static str>,
    /// One-shot switch-back toast after a screen-mode re-exec.
    pub screen_mode_switch_hint: Option<&'static str>,
    /// Require explicit plan approval via the plan viewer UI even in
    /// always-approve (YOLO) mode. Loaded from `[ui] require_plan_approval`
    /// in config.toml at startup.
    pub require_plan_approval: bool,
    /// Enable plan mode for new sessions (`--plan`).
    /// Adds `enter_plan_mode`, `exit_plan_mode` tools; implies `ask_user`.
    pub plan_mode: bool,
    /// Enable subagent spawning for new sessions (`--subagents`).
    /// Adds the `TaskTool` for spawning subagents.
    pub subagents: bool,
    /// Enable the ask-user-question tool for new sessions (`--ask-user`).
    /// Automatically enabled by `plan_mode`.
    pub ask_user: bool,
    /// Process-wide gateway light-frontend from CLI `--chat` only.
    /// Stamps `_meta["x.ai/session"].kind = "chat"` and omits Build agent
    /// profiles on create/load while set. `/chat` does **not** set this
    /// (uses [`Self::deferred_startup`] one-shot state instead).
    pub chat_mode: bool,
    /// Whether mouse capture is currently enabled. Disabled during the
    /// Authenticating state so the terminal handles native text selection.
    pub mouse_captured: bool,
    /// Active "New Worktree" dialog on the welcome screen.
    pub new_worktree_dialog: Option<NewWorktreeDialogState>,
    /// Resolved per-tip gates for the contextual ephemeral hints (undo tip,
    /// plan nudge, clipboard-image tip, send-now tip). Default all ON; resolved
    /// at startup and on settings toggles from `GROK_CONTEXTUAL_HINTS` (master)
    /// > `[ui.contextual_hints]` user config > remote tier > default.
    pub contextual_hints: xai_grok_shell::util::config::ResolvedContextualHints,
    /// Remote tier for the contextual hints, kept so a settings toggle can
    /// re-resolve the untouched tips against the same remote defaults.
    pub remote_contextual_hints: Option<xai_grok_shell::util::config::ContextualHintsRemote>,
    /// Per-key seen counts that gate seen-capped ephemeral tips; the single
    /// copy of this state. Passed to `show_ephemeral_tip`, which increments the
    /// matching key in place. In-memory only and per-session — never persisted
    /// to disk, so each pager run starts fresh (count 0).
    pub tip_seen_counts: std::collections::HashMap<&'static str, u32>,
    /// Terminal height (rows) from startup / the last `Event::Resize`. Feeds
    /// the auto-compact derivation (`views::agent::effective_compact`): the
    /// render-value compact flag is forced on while the terminal is
    /// `AUTO_COMPACT_MAX_ROWS` or shorter. 0 = unknown (never forces compact).
    pub last_known_terminal_rows: u16,
    /// One-shot gate for the small-screen `/compact-mode` tip: set after the
    /// first evaluation at a stable agent-view draw (regardless of outcome),
    /// so later resizes can never re-trigger the tip within this run.
    pub small_screen_tip_evaluated: bool,
    /// One-shot gate for the SSH `grok wrap` tip: set after the first
    /// evaluation at a stable agent-view draw (the environment gates are
    /// process-constant, so one evaluation decides the run).
    pub ssh_wrap_tip_evaluated: bool,
    /// Focus-scoped, opportunistically-polled clipboard-image tip state: poll
    /// throttle, changeCount delta-detection, fire cooldown, and changeCount
    /// dedup (macOS-only at the probe layer).
    pub clipboard_focus_tip: crate::tips::clipboard_focus::ClipboardFocusTipState,
    /// Persisted worktree preference for `/new`.
    /// Defaults to [`WorktreeMode::Never`] (no popup).
    pub new_session_worktree_mode: WorktreeMode,
    /// Persisted worktree preference for `/fork`.
    /// Defaults to [`WorktreeMode::Ask`] (show popup).
    pub fork_worktree_mode: WorktreeMode,
    /// Restore code state on resume (`--restore-code`).
    pub restore_code: Option<bool>,
    pub agent_override: Option<serde_json::Value>,
    /// ACP-advertised commands seeded into every new `AgentSession` so
    /// autocomplete has shell builtins and skills before any runtime
    /// `AvailableCommandsUpdate` arrives.
    ///
    /// Initially populated from `InitializeResponse.meta.availableCommands`
    /// (AlwaysOn builtins only). Updated whenever the active agent receives
    /// an `AvailableCommandsUpdate` that includes skills, so subsequent
    /// sessions start with the full command catalog immediately.
    pub bootstrap_acp_commands: Vec<agent_client_protocol::AvailableCommand>,
    /// Auth methods from the ACP connection (preserved for re-login after logout).
    pub auth_methods: Vec<acp::AuthMethod>,
    /// Authentication state for the welcome screen login flow.
    pub auth_state: AuthState,
    /// Folder-trust state for the welcome screen. Mirrors [`AppView::auth_state`]:
    /// when `Pending`, the welcome screen shows the trust question and session
    /// creation is deferred (gated after auth) until it is answered.
    pub trust_state: TrustState,
    /// Login button label from `AuthMethod.name` (e.g., "grok.com", "Acme Corp").
    pub login_label: Option<String>,
    /// The auth method ID to use for login.
    pub login_method_id: Option<acp::AuthMethodId>,
    /// Initial auth mode hint from method metadata.
    pub auth_start_mode: AuthMode,
    /// Text buffer for manual auth token paste (loopback mode).
    pub(crate) auth_code_input: LineEditor,
    /// Monotonically increasing sequence number for auth requests.
    pub next_auth_request_seq: u64,
    /// Abort handle for the in-flight `PollAuthUrl` task (with its request_seq).
    /// Aborted alongside the Authenticate task in single-flight re-login.
    pub auth_url_poll_handle: Option<(u64, tokio::task::AbortHandle)>,
    /// Every session/chat/worktree/prompt action deferred behind startup gates.
    pub deferred_startup: crate::app::session_startup::DeferredStartupActions,
    /// Whether deferred welcome-screen login should force OAuth.
    pub auth_use_oauth: bool,
    /// Delivery state from the last clipboard copy during auth.
    pub auth_clipboard_delivery: Option<crate::clipboard::ClipboardDelivery>,
    /// Generation of the current auth copy feedback and its clear timer.
    pub auth_clipboard_feedback_generation: u64,
    /// Team principal UUID from auth (`None` for personal sessions).
    pub team_id: Option<String>,
    /// Team name from auth (displayed in the shortcuts bar).
    pub team_name: Option<String>,
    /// Whether the user's team has enterprise Zero Data Retention enabled.
    pub is_zdr: bool,
    /// Team role (e.g. "Admin", "Member", "Read Only") for access-control checks.
    pub team_role: Option<String>,
    /// Whether the user has opted out of coding data retention.
    pub coding_data_retention_opt_out: bool,
    /// Persisted `[cli].show_tips` mirror. `None` = no override (default `true`).
    pub show_tips: Option<bool>,
    /// Persisted `[cli].auto_update` mirror. `None` = no override (default `true`).
    pub auto_update: Option<bool>,
    /// Persisted `[toolset.ask_user_question].timeout_enabled` mirror, seeded
    /// from the effective TOML merge like `show_tips`. `None` = unset in TOML
    /// (default `true`); toggles write the user layer.
    pub ask_user_question_timeout_enabled: Option<bool>,
    /// Whether ZDR users are allowed to use the product.
    /// Server-controlled via RemoteSettings (remote settings). Default `false` (blocked) during beta.
    pub zdr_access_enabled: bool,
    /// When set, `/usage` shows a link to this URL instead of fetching billing
    /// data from the backend. Server-controlled via RemoteSettings (remote settings
    /// `grok_build_usage_redirect_url`, targeted at personal-team users).
    /// `None` (default) fetches usage from the backend.
    pub usage_billing_redirect_url: Option<String>,
    pub access_gate_shown_logged: bool,
    /// (hide-key, surface) pairs whose `AnnouncementCtaShown` impression was
    /// already logged — once per pager process, cleared on logout. Keyed by
    /// `announcement_hide_key` (stable even for id-less items, unlike the
    /// event's `id`).
    pub announcement_cta_impressions_logged:
        std::collections::BTreeSet<(String, xai_grok_telemetry::events::AnnouncementCtaSurface)>,
    /// Access gate from `grok_build_access_gate`. `Some` = blocked.
    pub gate: Option<xai_grok_shell::auth::GateInfo>,
    /// User-friendly subscription tier name (e.g. "SuperGrok", "Free").
    pub subscription_tier: Option<String>,
    /// When the pager started auto-checking subscriptions (for 10-min timeout).
    pub paywall_check_started: Option<std::time::Instant>,
    /// Debounce stamp for watch/focus subscription checks (see
    /// [`super::subscription`]).
    pub last_subscription_check_at: Option<std::time::Instant>,
    /// Server override (seconds) for the subscription-watch cadence.
    pub subscription_watch_interval_secs: Option<u64>,
    /// A stale-source gate held out of `gate` while a live check verifies
    /// it (see [`super::subscription`]).
    pub pending_gate_verification: Option<xai_grok_shell::auth::GateInfo>,
    /// Generation stamp of the current gate verification.
    pub gate_verify_gen: u64,
    /// Whether a leader reconnect is in progress (blocks prompt submission).
    pub reconnect_pending: bool,
    /// Structured startup warnings collected from the terminal diagnostics
    /// engine at launch. Empty when the environment is healthy.
    pub startup_warnings: Vec<crate::startup::StartupWarning>,
    /// Whether the user authenticated with an API key (shown in the version badge).
    pub is_api_key_auth: bool,
    /// Latest version string from a background update check. Set when
    /// a newer version is detected; rendered as a notification on the
    /// welcome screen.
    pub pending_update_version: Option<String>,
    /// When true, the event loop should exit so the user can relaunch
    /// to pick up the downloaded update.
    pub quit_for_update: bool,
    /// Generation and state for the one launch-scoped foreign resume detection.
    pub(crate) foreign_resume_launch_generation: u64,
    pub(crate) foreign_resume_launch: Option<crate::app::foreign_sessions::ForeignResumeLaunch>,
    /// When set, the event loop should exit and the process re-exec into the
    /// other screen mode. Driven by `/minimal` and `/fullscreen`. Captures the
    /// session id at action time so a later teardown cannot drop `--resume`.
    pub relaunch: Option<ScreenModeRelaunch>,
    /// Whether importable `.claude/` settings were detected at startup.
    pub has_claude_import: bool,
    /// When set, the welcome screen renders an interactive import modal instead of normal content.
    pub import_claude_modal: Option<crate::views::import_claude_modal::ImportClaudeModalState>,
    /// Doc viewer overlay for the welcome screen (release notes via Ctrl+L).
    pub welcome_doc_viewer: Option<crate::views::modal::ActiveModal>,
    /// Whether the pager uses fullscreen (alt-screen) or inline mode.
    /// Set from the resolved terminal state at startup.
    pub(crate) screen_mode: super::ScreenMode,
    /// Agent Dashboard state. `Some(_)` only when the dashboard view
    /// is active (`active_view == AgentDashboard`) or recently closed.
    /// Held outside the `ActiveView` discriminant because `DashboardState`
    /// is not `Copy` (owns its prompt widget, peek panel, etc.).
    pub dashboard: Option<crate::views::dashboard::DashboardState>,
    /// Where to return when leaving the dashboard. See [`DashboardReturn`].
    pub dashboard_return: Option<DashboardReturn>,
    /// Persisted dashboard configuration (pinned rows, reorderings,
    /// grouping). Loaded once on startup from
    /// `~/.grok/config.toml`. `None` when the file/section is absent
    /// or contained malformed data — falls back to in-memory defaults.
    pub dashboard_persisted: Option<crate::views::dashboard::PersistedDashboard>,
    /// Per-platform key event normalizer.
    ///
    /// NOTE: new event consumers that bypass `AppView::handle_input`
    /// will not get rescued modifiers unless also normalizing.
    pub(crate) keyboard_normalizer: KeyboardNormalizer,
    /// Voice gate (GA default on at startup resolution). When false — remote
    /// kill switch or `GROK_VOICE_MODE=0` — the STT pipeline is not started and
    /// session voice mode cannot turn on. Unit tests leave this false until
    /// they call [`Self::apply_voice_mode_enabled`].
    pub voice_mode_enabled: bool,
    /// Session UI mode from `/voice` (this CLI process only — not in config.toml).
    /// When true and the pipeline is up, the in-prompt dictation overlay can show
    /// and capture may start. Cleared on exit or when the remote flag turns off.
    pub voice_ui_active: bool,
    /// Optional `[voice]` overrides from config (`api_base`, `language`, …).
    pub voice_config: xai_grok_voice::VoiceConfig,
    /// Auth for STT (OAuth session via shell `AuthManager`, or `XAI_API_KEY`).
    /// `None` until the pipeline is first started (lazy on `/voice`).
    pub voice_auth: Option<xai_grok_voice::SharedVoiceAuth>,
    /// Commands into the voice pipeline (start/stop capture — toggle, not hold).
    pub voice_cmd_tx: Option<tokio::sync::mpsc::Sender<xai_grok_voice::VoiceCommand>>,
    /// The dictation lifecycle (idle / queued / recording / stopping), including
    /// the live interim transcript. One state at a time, so inconsistent
    /// combinations are unrepresentable; production mutates it only through the
    /// `AppView::voice_*` transition methods.
    pub voice_state: VoiceState,
}
impl AppView {
    pub fn is_zdr_blocked(&self) -> bool {
        self.is_zdr && !self.zdr_access_enabled
    }
    /// User is not gated (no gate from remote settings or subscription fallback).
    pub fn has_access(&self) -> bool {
        self.gate.is_none()
    }
    /// True when the user should not see the prompt (gate, subscription, or ZDR).
    pub fn is_access_blocked(&self) -> bool {
        !self.has_access() || self.is_zdr_blocked()
    }
    /// Whether deferred session-startup actions may run: both auth AND folder
    /// trust must be resolved. Mirrors the auth gate at the session-creating
    /// startup sites; trust is gated AFTER auth so a pending trust question
    /// defers session creation until answered.
    pub fn session_startup_allowed(&self) -> bool {
        matches!(self.auth_state, AuthState::Done) && matches!(self.trust_state, TrustState::Done)
    }
    /// Extract `GateInfo` from `RemoteSettings`.
    pub fn gate_from_settings(
        rs: &xai_grok_shell::util::config::RemoteSettings,
    ) -> Option<xai_grok_shell::auth::GateInfo> {
        let msg = rs.gate_message.as_ref()?;
        if msg.is_empty() {
            return None;
        }
        Some(xai_grok_shell::auth::GateInfo {
            message: msg.clone(),
            url: rs.gate_url.clone(),
            label: rs.gate_label.clone(),
        })
    }
    /// Apply typed auth metadata from the shell.
    pub fn apply_auth_meta(&mut self, meta: &xai_grok_shell::auth::AuthMeta) {
        self.pending_gate_verification = None;
        let was_gated = self.gate.is_some();
        self.team_id = meta.team_id.clone();
        self.team_name = meta.team_name.clone();
        self.is_zdr = meta.is_zdr;
        self.team_role = meta.team_role.clone();
        self.coding_data_retention_opt_out = meta.coding_data_retention_opt_out;
        self.gate = meta.gate.clone();
        if was_gated && self.gate.is_none() {
            self.paywall_check_started = None;
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::events::SubscriptionActivated {
                    auth_method: self.login_method_id.as_ref().map(|id| id.0.to_string()),
                    upsell_shown_this_session: self.access_gate_shown_logged,
                },
            );
        }
        self.subscription_tier = meta.subscription_tier.clone();
        let was_api_key = self.is_api_key_auth;
        self.is_api_key_auth = meta.auth_mode.as_deref().is_some_and(is_api_key_label)
            || meta
                .subscription_tier
                .as_deref()
                .is_some_and(is_api_key_label);
        self.usage_visible = meta.team_name.is_none() && !self.is_api_key_auth;
        self.apply_tier_restrictions();
        if self.is_api_key_auth {
            self.ensure_voice_for_api_key();
        } else if was_api_key && is_restricted_tier(self.subscription_tier.as_deref()) {
            self.voice_reset();
            self.voice_ui_active = false;
            self.apply_voice_mode_enabled(false);
        }
        if let Some(show) = meta.show_resolved_model {
            self.show_resolved_model = show;
        }
    }
    /// Force voice on for API-key sessions when only a remote rule left it off.
    /// Requirement / env / config pins still win.
    pub(crate) fn ensure_voice_for_api_key(&mut self) {
        if !self.is_api_key_auth || self.voice_mode_enabled {
            return;
        }
        if crate::app::resolve_voice_mode_live(None, false) {
            self.apply_voice_mode_enabled(true);
        }
    }
    /// Create a new AppView with the given ACP connection details.
    pub fn new(
        acp_tx: AcpAgentTx,
        models: ModelState,
        bootstrap_acp_commands: Vec<agent_client_protocol::AvailableCommand>,
    ) -> Self {
        let slash_mru =
            std::rc::Rc::new(std::cell::RefCell::new(crate::slash::mru::SlashMru::new()));
        let mut welcome_prompt = PromptWidget::new();
        welcome_prompt.adopt_slash_mru(slash_mru.clone());
        Self {
            active_view: ActiveView::Welcome,
            auth_return_view: None,
            agents: IndexMap::new(),
            next_agent_id: 0,
            models,
            registry: ActionRegistry::defaults(),
            settings_registry: Arc::new(crate::settings::SettingsRegistry::defaults()),
            current_ui: xai_grok_shell::agent::config::UiConfig::default(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            project_picker_shown: false,
            project_picker_disabled: false,
            cwd_has_git_ancestor: std::env::current_dir()
                .ok()
                .is_some_and(|c| c.ancestors().any(|p| p.join(".git").exists())),
            acp_tx,
            bundle_state: BundleState::default(),
            scratch: ScratchBuffer::new(),
            cursor: CursorState::new(),
            pending_action: None,
            exit_session_pending: None,
            scroll_state: MouseScrollState::default(),
            scroll_config: ScrollConfig::from_settings(),
            appearance: AppearanceConfig::default(),
            notification_service: NotificationService::new(Default::default()),
            pending_notification_escapes: None,
            deferred_notification: None,
            tracing_rx: None,
            scroll_debug_hud: crate::views::scroll_debug_hud::ScrollDebugHud::new(),
            fps_hud: crate::views::fps_hud::FpsHud::new(),
            active_announcements: Vec::new(),
            hidden_announcement_ids: Default::default(),
            announcements_last_gen: 0,
            announcement: None,
            changelog_markdown: None,
            changelog_bullets: Vec::new(),
            tips: Vec::new(),
            tip: None,
            welcome_prompt,
            slash_mru,
            welcome_prompt_focused: true,
            welcome_tip_typing_dismissed: false,
            pending_effects: Vec::new(),
            pending_editor_path: None,
            pending_agents_modal_refresh: None,
            pending_pager_path: None,
            pending_pager_ansi: false,
            minimal_state: crate::minimal_api::MinimalState::default(),
            welcome_menu_index: None,
            welcome_menu_rects: Vec::new(),
            welcome_show_changelog_action: false,
            welcome_import_banner_rect: None,
            last_mouse_pos: None,
            last_scroll_pos: None,
            last_cache_evict_at: None,
            welcome_prompt_rect: None,
            welcome_auth_url_rect: None,
            welcome_on_auth_url: false,
            welcome_on_changelog_cta: false,
            welcome_announcement: WelcomeAnnouncementState::default(),
            welcome_auth_fallback_rect: None,
            welcome_refresh_rect: None,
            welcome_gate_url_rect: None,
            welcome_upgrade_cta_rect: None,
            welcome_on_upgrade_cta: false,
            welcome_changelog_cta_rect: None,
            auth_show_raw_url: false,
            auth_mouse_disabled: false,
            session_picker_entries: None,
            session_picker_loading: false,
            session_picker_state: crate::views::picker::PickerState::with_mode(
                crate::views::picker::PickerMode::FullScreen,
            ),
            session_picker_source_filter: crate::views::session_picker::SourceFilter::default(),
            session_picker_content_results: None,
            session_picker_content_loading: false,
            session_picker_deep_search_seq: 0,
            session_picker_list_seq: 0,
            foreign_session_compat: Default::default(),
            foreign_session_scan_seq: 0,
            foreign_scan_coordinator: Default::default(),
            session_picker_lanes: Default::default(),
            session_picker_detail_generation: 0,
            session_picker_entries_query: None,
            welcome_tick: 0,
            welcome_shimmer_frame: 0,
            cli_model_override: None,
            cli_effort_token: None,
            default_yolo: false,
            permission_mode_from_soft_default: true,
            auto_mode_gate: xai_grok_shell::util::config::auto_permission_mode_enabled_from_disk(),
            yolo_policy_block: None,
            yolo_launch_block_notice: None,
            screen_mode_switch_hint: None,
            require_plan_approval: false,
            plan_mode: false,
            subagents: false,
            ask_user: false,
            chat_mode: false,
            mouse_captured: true,
            new_worktree_dialog: None,
            contextual_hints: Default::default(),
            remote_contextual_hints: None,
            tip_seen_counts: Default::default(),
            last_known_terminal_rows: 0,
            small_screen_tip_evaluated: false,
            ssh_wrap_tip_evaluated: false,
            clipboard_focus_tip: Default::default(),
            new_session_worktree_mode: WorktreeMode::Never,
            fork_worktree_mode: WorktreeMode::Ask,
            restore_code: None,
            agent_override: None,
            bootstrap_acp_commands,
            auth_methods: Vec::new(),
            auth_state: AuthState::Done,
            trust_state: TrustState::Done,
            login_label: None,
            login_method_id: None,
            auth_start_mode: AuthMode::Pending,
            auth_code_input: LineEditor::default(),
            next_auth_request_seq: 1,
            auth_url_poll_handle: None,
            deferred_startup: Default::default(),
            auth_use_oauth: false,
            auth_clipboard_delivery: None,
            auth_clipboard_feedback_generation: 0,
            team_id: None,
            team_name: None,
            is_zdr: false,
            team_role: None,
            coding_data_retention_opt_out: true,
            show_tips: None,
            auto_update: None,
            ask_user_question_timeout_enabled: None,
            zdr_access_enabled: false,
            usage_billing_redirect_url: None,
            access_gate_shown_logged: false,
            announcement_cta_impressions_logged: Default::default(),
            gate: None,
            subscription_tier: None,
            paywall_check_started: None,
            last_subscription_check_at: None,
            subscription_watch_interval_secs: None,
            pending_gate_verification: None,
            gate_verify_gen: 0,
            reconnect_pending: false,
            startup_warnings: Vec::new(),
            is_api_key_auth: false,
            pending_update_version: None,
            foreign_resume_launch_generation: 0,
            foreign_resume_launch: None,
            quit_for_update: false,
            relaunch: None,
            has_claude_import: false,
            import_claude_modal: None,
            welcome_doc_viewer: None,
            screen_mode: ScreenMode::Inline,
            show_resolved_model: true,
            sharing_enabled: false,
            plugin_cta_enabled: false,
            usage_visible: true,
            tier_restricted_commands: Vec::new(),
            leader_mode: false,
            credit_balance: None,
            auto_topup: None,
            billing_poll_wanted: false,
            leader_roster: Vec::new(),
            dashboard_local_sessions: Vec::new(),
            dashboard_sessions_loading: false,
            shared_prompt_queues: std::collections::HashMap::new(),
            optimistic_prompt_echoes: std::collections::HashMap::new(),
            pending_running_adoptions: std::collections::HashMap::new(),
            session_picker_grouped: false,
            cancel_rewind_enabled: true,
            session_recap_available: false,
            dashboard: None,
            dashboard_return: None,
            dashboard_persisted: None,
            keyboard_normalizer: KeyboardNormalizer::from_terminal_context(),
            voice_mode_enabled: false,
            voice_ui_active: false,
            voice_config: xai_grok_voice::VoiceConfig::default(),
            voice_auth: None,
            voice_cmd_tx: None,
            voice_state: VoiceState::Idle,
        }
    }
    /// Seed `deferred_model_switch` from CLI `-m`. The CLI effort token is
    /// resolved later against the authoritative session catalog in
    /// [`take_deferred_model_switch`](crate::app::dispatch::session::lifecycle::take_deferred_model_switch);
    /// resolving it here would use the pre-session dashboard catalog and a
    /// remapped menu id could resolve differently.
    pub fn deferred_model_switch_from_cli(
        &self,
    ) -> Option<(
        acp::ModelId,
        Option<xai_grok_shell::sampling::types::ReasoningEffort>,
    )> {
        Some((self.cli_model_override.clone()?, None))
    }
    /// Voice capture is armed: the in-prompt dictation overlay can show and
    /// Ctrl+Space can start capture.
    ///
    /// Requires the voice gate, session `/voice` mode, and a live pipeline.
    /// Stopping capture remains allowed when the kill switch flips mid-record
    /// (see `dispatch_voice_toggle`).
    pub fn voice_available(&self) -> bool {
        self.voice_mode_enabled && self.voice_ui_active && self.voice_cmd_tx.is_some()
    }
    /// Whether launch may spawn the background STT pipeline (independent of
    /// `/voice`). Gated on the voice gate + a build that compiled in audio
    /// capture. Free-tier upsell is separate ([`Self::is_voice_tier_restricted`]).
    pub fn voice_can_start_pipeline(&self) -> bool {
        self.voice_mode_enabled && xai_grok_voice::AUDIO_SUPPORTED
    }
    /// Sync voice availability into slash surfaces, cheatsheet, and settings.
    /// Mirrors `apply_session_recap_available` for `/recap`.
    pub fn apply_voice_mode_enabled(&mut self, enabled: bool) {
        self.voice_mode_enabled = enabled;
        crate::app::VOICE_MODE_ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
        for agent in self.agents.values_mut() {
            agent.set_voice_mode_available(enabled);
            match agent.active_modal.as_mut() {
                Some(crate::views::modal::ActiveModal::Settings { state }) => {
                    state.rebuild_rows();
                }
                Some(crate::views::modal::ActiveModal::ResetSettingsConfirm {
                    settings_state,
                    ..
                }) => {
                    settings_state.rebuild_rows();
                }
                _ => {}
            }
        }
        self.welcome_prompt.set_voice_visible(enabled);
        if let Some(dashboard) = self.dashboard.as_mut() {
            dashboard.set_voice_visible(enabled);
        }
    }
    /// Sync the auto permission-mode feature gate into every slash surface.
    /// `/auto` is hard-hidden when `self.auto_mode_gate` is off; otherwise both
    /// `/always-approve` and `/auto` stay offered as true toggles. Mirrors
    /// [`Self::apply_voice_mode_enabled`]. Call after gate flips, startup,
    /// reconnect, and session create/switch (so new agents inherit the gate).
    pub fn sync_permission_mode_slash_gate(&mut self) {
        let available = self.auto_mode_gate;
        for agent in self.agents.values_mut() {
            agent.prompt.set_auto_mode_available(available);
        }
        self.welcome_prompt.set_auto_mode_available(available);
        if let Some(dashboard) = self.dashboard.as_mut() {
            dashboard.set_auto_mode_available(available);
        }
    }
    /// Recompute the tier-restricted slash commands from the current auth
    /// state and sync the deny list into every slash surface (welcome
    /// prompt, all agents, dashboard) so restricted commands hide/show in
    /// lockstep. Mirrors [`Self::apply_voice_mode_enabled`].
    ///
    /// Called from [`Self::apply_auth_meta`] (startup / login) and from the
    /// `x.ai/settings/update` handler when the subscription tier changes, so
    /// a mid-session upgrade lifts the restrictions without a restart.
    pub fn apply_tier_restrictions(&mut self) {
        let restricted = self.team_name.is_none()
            && !self.is_api_key_auth
            && is_restricted_tier(self.subscription_tier.as_deref());
        let names: Vec<String> = if restricted {
            TIER_RESTRICTED_COMMANDS
                .iter()
                .map(|n| (*n).to_string())
                .collect()
        } else {
            Vec::new()
        };
        for agent in self.agents.values_mut() {
            agent.set_restricted_commands(&names);
        }
        self.welcome_prompt.set_restricted_commands(&names);
        if let Some(dashboard) = self.dashboard.as_mut() {
            dashboard.set_restricted_commands(&names);
        }
        self.tier_restricted_commands = names;
    }
    /// Whether voice mode is withheld for the current subscription tier
    /// (free / X Basic personal accounts). Derived from the computed
    /// [`Self::tier_restricted_commands`] deny list so it stays in lockstep
    /// with the slash-command gate. Used to gate the Ctrl+Space / F8 voice
    /// keybinding, which bypasses the slash registry entirely (see
    /// [`crate::app::dispatch::voice`]).
    pub fn is_voice_tier_restricted(&self) -> bool {
        self.tier_restricted_commands.iter().any(|c| c == "voice")
    }
    /// Draw-time expiry can flip the live-announcement predicate between
    /// pushes; resync the slash gate only when it diverges from the stored
    /// flags (checked per frame, fan-out runs only on change).
    pub fn resync_announcement_slash_gate_on_divergence(&mut self) {
        let has =
            crate::views::announcements::has_session_announcements(&self.active_announcements);
        if self
            .agents
            .values()
            .any(|a| a.prompt.slash_controller.has_session_announcements() != has)
        {
            self.sync_session_announcement_slash_gate();
        }
    }
    /// Offer `/announcements` only when session items (critical or promo)
    /// exist (even if currently hidden — user may still run `/announcements
    /// show`).
    pub fn sync_session_announcement_slash_gate(&mut self) {
        let has =
            crate::views::announcements::has_session_announcements(&self.active_announcements);
        for agent in self.agents.values_mut() {
            agent
                .prompt
                .slash_controller
                .set_has_session_announcements(has);
            for child in agent.subagent_views.values_mut() {
                child.set_has_session_announcements(has);
            }
        }
    }
    /// Mic is live (the [`VoiceState::Recording`] state).
    pub fn voice_listening(&self) -> bool {
        self.voice_state.listening()
    }
    /// Whether the in-flight session is owned by a hold-press (so its key
    /// release ends it). `/voice` and toggle-style starts leave this false.
    pub fn voice_hold_owned(&self) -> bool {
        self.voice_state.hold()
    }
    /// The prompt box that owns in-flight dictation, if any.
    pub fn voice_recording_target(&self) -> Option<VoiceTarget> {
        self.voice_state.target()
    }
    /// The live partial transcript shown in the prompt overlay, if any.
    pub fn voice_interim(&self) -> Option<&str> {
        self.voice_state.interim()
    }
    /// Best-effort one-shot command into the voice pipeline (no-op if it isn't up).
    fn voice_send(&self, cmd: xai_grok_voice::VoiceCommand) {
        if let Some(tx) = &self.voice_cmd_tx
            && tx.try_send(cmd).is_err()
        {
            tracing::trace!("voice command dropped: pipeline channel full or closed");
        }
    }
    /// Open the mic now (pipeline already up) and enter [`VoiceState::Recording`]
    /// bound to `target`. `hold` marks a Ctrl+Space hold-press start.
    pub(crate) fn voice_begin_recording(&mut self, target: VoiceTarget, hold: bool) {
        self.voice_send(xai_grok_voice::VoiceCommand::PttPress);
        self.voice_state = VoiceState::Recording {
            hold,
            target,
            interim: None,
        };
    }
    /// Set the live interim transcript. No-op unless recording, so a late event
    /// after a stop can't repopulate the overlay.
    pub(crate) fn voice_set_interim(&mut self, text: String) -> bool {
        if let VoiceState::Recording { interim, .. } = &mut self.voice_state {
            *interim = Some(text);
            true
        } else {
            false
        }
    }
    /// Clear the interim in place, keeping the current state. Called when a final
    /// commits (or yields empty) so the overlay drops the partial without a
    /// teardown.
    pub(crate) fn voice_clear_interim(&mut self) {
        match &mut self.voice_state {
            VoiceState::Recording { interim, .. } | VoiceState::Stopping { interim, .. } => {
                *interim = None;
            }
            VoiceState::Idle | VoiceState::ColdStart { .. } => {}
        }
    }
    /// Explicit stop (Esc / Ctrl+Space / `[stop]`): release the mic but keep
    /// the target and last interim so a trailing STT final still lands. Always
    /// allowed — never leaves a hot mic. No-op unless recording.
    pub(crate) fn voice_stop_keeping_final(&mut self) {
        let VoiceState::Recording {
            target, interim, ..
        } = &mut self.voice_state
        else {
            return;
        };
        let target = *target;
        let interim = interim.take();
        self.voice_send(xai_grok_voice::VoiceCommand::PttRelease);
        self.voice_state = VoiceState::Stopping { target, interim };
    }
    /// Hard teardown (submit / error / kill-switch / navigate-away): release the
    /// mic and forget the session — no trailing final, no queued start.
    pub(crate) fn voice_reset(&mut self) {
        if self.voice_state.listening() {
            self.voice_send(xai_grok_voice::VoiceCommand::PttRelease);
        }
        self.voice_state = VoiceState::Idle;
    }
    /// Ctrl+Space hold release: end only a session a Ctrl+Space hold started —
    /// cancel a queued hold cold-start, or stop a live hold recording (keeping
    /// its trailing final). A `/voice` / toggle session (`hold` false) is left
    /// untouched, so a Ctrl+Space release can neither cancel nor stop it.
    pub(crate) fn voice_hold_release(&mut self) {
        match self.voice_state {
            VoiceState::ColdStart { hold: true, .. } => self.voice_reset(),
            VoiceState::Recording { hold: true, .. } => self.voice_stop_keeping_final(),
            _ => {}
        }
    }
    /// Whether the active view still owns the bound dictation `target` — i.e. the
    /// box dictation started in is the one currently on screen and selected. The
    /// target is bound at capture start; on the dashboard that means dispatch
    /// requires no peek open, a peek reply requires the *same* top-level row still
    /// peeked (the shared reply widget clears on row change), and any open
    /// attached-agent popup (which occludes the dashboard inputs) disqualifies it.
    /// `false` when no dictation is bound.
    fn voice_target_on_active_surface(&self) -> bool {
        let Some(target) = self.voice_recording_target() else {
            return false;
        };
        if matches!(self.active_view, ActiveView::AgentDashboard)
            && self
                .dashboard
                .as_ref()
                .is_some_and(|d| d.attached_agent.is_some())
        {
            return false;
        }
        let peeked_top_level = self
            .dashboard
            .as_ref()
            .and_then(|d| match d.peek.as_ref()?.row {
                crate::views::dashboard::DashboardRowId::TopLevel(id) => Some(id),
                _ => None,
            });
        match (self.active_view, target) {
            (ActiveView::Agent(active), VoiceTarget::Agent(rec)) => active == rec,
            (ActiveView::AgentDashboard, VoiceTarget::DashboardDispatch) => {
                self.dashboard.as_ref().is_none_or(|d| d.peek.is_none())
            }
            (ActiveView::AgentDashboard, VoiceTarget::DashboardPeekReply(rec)) => {
                peeked_top_level == Some(rec)
            }
            _ => false,
        }
    }
    /// Auto-release the mic if the user navigates away from the box that started
    /// recording (another agent / dashboard popup / a changed peek row). Keeps
    /// stop controls and the recording session aligned. Event-loop each tick;
    /// no-op unless recording.
    pub fn enforce_voice_session_bound(&mut self) {
        if !self.voice_state.listening() || self.voice_target_on_active_surface() {
            return;
        }
        self.voice_reset();
    }
    /// Esc handling shared by the agent and dashboard surfaces: while voice is
    /// active, Esc aborts it (and consumes the key) rather than falling into the
    /// surface's own Esc behaviour. Gated on voice state only (not the remote
    /// flag) so Esc can always abort. `None` means Esc isn't ours — the caller
    /// continues its normal routing.
    fn voice_esc_outcome(
        &mut self,
        key_event: Option<&crossterm::event::KeyEvent>,
    ) -> Option<InputOutcome> {
        let key = key_event?;
        if key.code != KeyCode::Esc || !key.modifiers.is_empty() {
            return None;
        }
        if self.voice_listening() {
            Some(InputOutcome::Action(Action::VoiceToggle))
        } else if self.voice_state.pending_cold_start() {
            self.voice_reset();
            Some(InputOutcome::Changed)
        } else {
            None
        }
    }
    /// The active agent's view, when an agent tab is focused.
    ///
    /// Always the root agent, even when a subagent view is focused within the
    /// tab; for subagent-aware resolution use `dispatch::ctx::get_active_agent`.
    pub fn active_agent(&self) -> Option<&AgentView> {
        match self.active_view {
            ActiveView::Agent(id) => self.agents.get(&id),
            _ => None,
        }
    }
    /// Session ID of the active agent, if one exists and has an established session.
    pub fn active_session_id(&self) -> Option<&str> {
        match self.active_view {
            ActiveView::Agent(id) => self
                .agents
                .get(&id)
                .and_then(|a| a.session.session_id.as_ref())
                .map(|sid| sid.0.as_ref()),
            _ => None,
        }
    }
    /// Whether the project picker should intercept the next prompt.
    pub fn needs_project_picker(&self) -> bool {
        !self.project_picker_shown
            && !self.project_picker_disabled
            && !crate::project_picker::detection::is_project_dir(&self.cwd)
    }
    /// Mark the project picker as resolved so it won't fire again.
    pub fn mark_project_picker_done(&mut self) {
        self.project_picker_shown = true;
    }
    /// Show a toast on the currently active agent.
    ///
    /// No-op on the welcome screen. From the dashboard, toasts route
    /// into the dispatch input's inline error slot so the user sees
    /// the message at the bottom of the dashboard. From inside an
    /// agent view the existing per-agent toast machinery fires.
    pub fn show_toast(&mut self, msg: &str) {
        match self.active_view {
            ActiveView::Agent(id) => {
                if let Some(agent) = self.agents.get_mut(&id) {
                    if let Some(child_sid) = agent.active_subagent.clone()
                        && let Some(child) = agent.subagent_views.get_mut(&child_sid)
                    {
                        child.show_toast(msg);
                    } else {
                        agent.show_toast(msg);
                    }
                }
            }
            ActiveView::AgentDashboard => {
                if let Some(d) = self.dashboard.as_mut() {
                    d.error_toast = Some(crate::glyphs::legacy_glyph_fallback(msg).into_owned());
                }
            }
            ActiveView::Welcome => {}
        }
    }
    /// Insert or replace a leader roster entry, keyed by `session_id`.
    pub fn upsert_roster_entry(&mut self, entry: crate::app::roster::RosterEntry) {
        if let Some(existing) = self
            .leader_roster
            .iter_mut()
            .find(|e| e.session_id == entry.session_id)
        {
            *existing = entry;
        } else {
            self.leader_roster.push(entry);
        }
    }
    /// Remove a leader roster entry by `session_id`.
    pub fn remove_roster_entry(&mut self, sid: &str) {
        self.leader_roster.retain(|e| e.session_id != sid);
    }
    /// The roster source the dashboard renders alongside locally-hosted
    /// agents. In leader mode this is the live leader roster (FleetView). With
    /// no leader there is nothing to poll, so we fall back to the local
    /// on-disk session list ([`Self::dashboard_local_sessions`]) so the
    /// dashboard still shows idle/dormant sessions instead of being empty.
    pub fn dashboard_roster(&self) -> &[crate::app::roster::RosterEntry] {
        if self.leader_mode {
            &self.leader_roster
        } else {
            &self.dashboard_local_sessions
        }
    }
    /// Reconcile the shared prompt queue for a session from a
    /// `x.ai/queue/changed` broadcast. The broadcast is
    /// authoritative: it fully replaces the previously-known queue for that
    /// session. An empty list clears the entry.
    ///
    /// Returns `(old_id, new_id)` for echoes retired via the kind+text
    /// fallback (re-keyed: the old id never appears in any broadcast). The
    /// caller routes these through `AgentView::note_queue_echo_rekeyed` so
    /// per-agent state moves with the message instead of leaking.
    pub fn apply_queue_changed(
        &mut self,
        changed: crate::app::prompt_queue::QueueChanged,
    ) -> Vec<(String, String)> {
        let crate::app::prompt_queue::QueueChanged {
            session_id,
            mut entries,
            running_prompt_id,
        } = changed;
        let mut rekeyed_echo_ids: Vec<(String, String)> = Vec::new();
        let running_row: Option<(String, String)> = running_prompt_id.as_ref().and_then(|pid| {
            self.shared_prompt_queues
                .get(&session_id)
                .and_then(|q| q.iter().find(|e| &e.id == pid))
                .map(|e| (e.kind.clone(), e.text.clone()))
        });
        if let Some(opt) = self.optimistic_prompt_echoes.get_mut(&session_id) {
            opt.retain(|e| {
                let id_matches_running = running_prompt_id.as_deref() == Some(e.id.as_str());
                let id_matches_entry = entries.iter().any(|x| x.id == e.id);
                let content_match_id = running_row
                    .as_ref()
                    .filter(|(kind, text)| *kind == e.kind && *text == e.text)
                    .and_then(|_| running_prompt_id.clone())
                    .or_else(|| {
                        entries
                            .iter()
                            .find(|x| x.kind == e.kind && x.text == e.text)
                            .map(|x| x.id.clone())
                    });
                let retired = id_matches_running || id_matches_entry || content_match_id.is_some();
                if retired
                    && !id_matches_running
                    && !id_matches_entry
                    && let Some(new_id) = content_match_id
                {
                    rekeyed_echo_ids.push((e.id.clone(), new_id));
                }
                !retired
            });
            for e in opt.iter() {
                if !entries.iter().any(|x| x.id == e.id) {
                    let mut pinned = e.clone();
                    pinned.position = entries.len();
                    entries.push(pinned);
                }
            }
            if opt.is_empty() {
                self.optimistic_prompt_echoes.remove(&session_id);
            }
        }
        if entries.is_empty() {
            self.shared_prompt_queues.remove(&session_id);
        } else {
            self.shared_prompt_queues.insert(session_id, entries);
        }
        rekeyed_echo_ids
    }
    /// Push an optimistic echo row for a server-authoritative prompt the pager
    /// just sent (a plain prompt or agent-bound kind typed while a turn is
    /// running). The row is keyed by `prompt_id` so the authoritative
    /// `x.ai/queue/changed` broadcast replaces it (matched by `id`) rather than
    /// duplicating it. `kind` (`"prompt"`/`"bash"`/…) drives the row's display
    /// and, on adoption, the turn-start shim's block + focus flag.
    pub fn push_optimistic_prompt_echo(
        &mut self,
        session_id: &str,
        prompt_id: &str,
        text: &str,
        kind: &str,
    ) {
        let entry = crate::app::prompt_queue::QueueEntryWire {
            id: prompt_id.to_string(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: kind.to_string(),
            text: text.to_string(),
            position: 0,
        };
        let opt = self
            .optimistic_prompt_echoes
            .entry(session_id.to_string())
            .or_default();
        if !opt.iter().any(|e| e.id == entry.id) {
            opt.push(entry.clone());
        }
        let shared = self
            .shared_prompt_queues
            .entry(session_id.to_string())
            .or_default();
        if !shared.iter().any(|e| e.id == entry.id) {
            let mut e = entry;
            e.position = shared.len();
            shared.push(e);
        }
    }
    /// The shared (server-authoritative) prompt queue for a session, if any.
    pub fn shared_prompt_queue(
        &self,
        session_id: &str,
    ) -> Option<&Vec<crate::app::prompt_queue::QueueEntryWire>> {
        self.shared_prompt_queues.get(session_id)
    }
    /// Apply a (possibly hot-reloaded) appearance config to all agents.
    pub fn set_appearance(&mut self, config: AppearanceConfig) {
        for agent in self.agents.values_mut() {
            agent.scrollback.set_appearance(config.clone());
            for child in agent.subagent_views.values_mut() {
                child.scrollback.set_appearance(config.clone());
                child.prompt.sync_tab_width_from_appearance();
            }
            agent
                .prompt
                .slash_controller
                .registry_mut()
                .set_plugins_visible(!config.disable_plugins);
            agent.prompt.sync_tab_width_from_appearance();
        }
        self.welcome_prompt.sync_tab_width_from_appearance();
        self.appearance = config;
    }
    /// Recompute the render-value compact flag from the user setting +
    /// terminal height (`views::agent::effective_compact`) and propagate it
    /// to the appearance fan-out and every agent's prompt widget when it
    /// changed. In-memory only: never touches the user setting
    /// (`current_ui.compact_mode`), the render cache, or disk — auto-compact
    /// is derived, so growing the window restores the user's choice.
    pub(crate) fn apply_effective_compact(&mut self) {
        let derived = crate::views::agent::effective_compact(
            self.current_ui.compact_mode,
            self.last_known_terminal_rows,
        );
        if self.appearance.prompt.compact == derived {
            return;
        }
        let mut config = self.appearance.clone();
        config.prompt.compact = derived;
        self.set_appearance(config);
        for agent in self.agents.values_mut() {
            agent.prompt.set_compact(derived);
        }
    }
    /// Viewport height (rows) of the surface a scroll would move — the
    /// active agent's (or its fullscreen subagent's) scrollback pane, as
    /// measured at the last draw. 0 = unknown (welcome/dashboard views),
    /// which keeps the trackpad per-flush cap at its floor.
    fn scroll_viewport_height(&self) -> u16 {
        match self.active_view {
            ActiveView::Agent(id) => self.agents.get(&id).map_or(0, |agent| {
                let scrollback = agent
                    .active_subagent
                    .as_ref()
                    .and_then(|sid| agent.subagent_views.get(sid))
                    .map_or(&agent.scrollback, |child| &child.scrollback);
                scrollback.scroll_info().1
            }),
            _ => 0,
        }
    }
    /// Assemble the scroll-debug HUD's per-frame params (`None` unless
    /// enabled). Called by `draw()` BEFORE the frame closure: all scroll
    /// state updates for this frame already happened (input/ticks run before
    /// draw), and the snapshot is read-only, so the HUD observes exactly the
    /// state the frame renders without perturbing it.
    fn scroll_debug_panel(&self) -> Option<crate::views::scroll_debug_hud::ScrollDebugPanel> {
        if !self.scroll_debug_hud.enabled() {
            return None;
        }
        let config = self
            .scroll_config
            .with_viewport_height(self.scroll_viewport_height());
        let snapshot = self
            .scroll_state
            .debug_snapshot(&config, std::time::Instant::now());
        let view = match self.active_view {
            ActiveView::Agent(id) => self.agents.get(&id).map(|agent| {
                let scrollback = agent
                    .active_subagent
                    .as_ref()
                    .and_then(|sid| agent.subagent_views.get(sid))
                    .map_or(&agent.scrollback, |child| &child.scrollback);
                let (scroll_offset, viewport, total_height) = scrollback.scroll_info();
                let max_offset = total_height.saturating_sub(viewport as usize);
                crate::views::scroll_debug_hud::ViewportDebug {
                    scroll_offset,
                    max_offset,
                    total_height,
                    follow_mode: scrollback.is_follow_mode(),
                    at_bottom: scroll_offset >= max_offset,
                }
            }),
            _ => None,
        };
        let top_offset = self.dev_fps_rows() + self.fps_hud.overlay_height();
        Some(crate::views::scroll_debug_hud::ScrollDebugPanel {
            snapshot,
            view,
            top_offset,
        })
    }
    /// Rows the dev `GROK_FPS` overlay occupies (0 in non-dev builds), so
    /// runtime debug overlays stack below instead of overpainting it.
    fn dev_fps_rows(&self) -> u16 {
        0
    }
    /// Route a scroll delta to the active view.
    fn dispatch_scroll(&mut self, lines: i32, column: u16, row: u16) {
        match self.active_view {
            ActiveView::Agent(id) => {
                if let Some(agent) = self.agents.get_mut(&id) {
                    if let Some(child_sid) = agent.active_subagent.clone()
                        && let Some(child) = agent.subagent_views.get_mut(&child_sid)
                    {
                        child.handle_scroll(lines, column, row);
                        return;
                    }
                    agent.handle_scroll(lines, column, row);
                }
            }
            ActiveView::Welcome => {
                if let Some(crate::views::modal::ActiveModal::DocViewer { scroll, .. }) =
                    self.welcome_doc_viewer.as_mut()
                {
                    crate::views::modal::apply_doc_scroll_delta(scroll, lines);
                }
            }
            ActiveView::AgentDashboard => {
                let popup_target = self.dashboard.as_ref().and_then(|d| {
                    d.attached_agent
                        .zip(d.popup_outer_rect)
                        .filter(|(_, outer)| {
                            column >= outer.x
                                && column < outer.x + outer.width
                                && row >= outer.y
                                && row < outer.y + outer.height
                        })
                });
                if let Some((agent_id, _outer)) = popup_target {
                    if let Some(agent) = self.agents.get_mut(&agent_id) {
                        if let Some(child_sid) = agent.active_subagent.clone()
                            && let Some(child) = agent.subagent_views.get_mut(&child_sid)
                        {
                            child.handle_scroll(lines, column, row);
                            return;
                        }
                        agent.handle_scroll(lines, column, row);
                    }
                    return;
                }
                let in_file_search_dropdown = self
                    .dashboard
                    .as_ref()
                    .and_then(|d| d.file_search_dropdown_items_area)
                    .is_some_and(|dd| {
                        column >= dd.x
                            && column < dd.x + dd.width
                            && row >= dd.y
                            && row < dd.y + dd.height
                    });
                if in_file_search_dropdown {
                    if let Some(ref mut dashboard) = self.dashboard {
                        dashboard
                            .dropdown_file_search_mut()
                            .move_selection(lines.signum() as isize);
                    }
                    return;
                }
                let in_slash_dropdown = self
                    .dashboard
                    .as_ref()
                    .and_then(|d| d.slash_dropdown_items_area)
                    .is_some_and(|dd| {
                        column >= dd.x
                            && column < dd.x + dd.width
                            && row >= dd.y
                            && row < dd.y + dd.height
                    });
                if in_slash_dropdown {
                    if let Some(ref mut dashboard) = self.dashboard {
                        dashboard
                            .dispatch
                            .slash_scroll_selection(lines.signum() as isize);
                    }
                    return;
                }
                if let Some(ref mut dashboard) = self.dashboard {
                    dashboard.handle_scroll(lines);
                }
            }
        }
    }
}
impl AppView {
    /// Handle a terminal event. Routes through the input layer stack:
    ///
    /// 1. Pending action check (double-press confirmation)
    /// 2. Active view (Welcome or Agent — agent does pane + agent-level)
    /// 3. Global actions (quit with confirmation)
    ///
    /// Quit always goes through double-press confirmation, even when
    /// escalated from agent-level (e.g., Ctrl-C while cancelling).
    pub fn handle_input(&mut self, ev: &Event) -> InputOutcome {
        self.handle_input_at_with_paste_provenance(ev, Instant::now(), PasteProvenance::Terminal)
    }
    pub(crate) fn handle_input_at_with_paste_provenance(
        &mut self,
        ev: &Event,
        arrived_at: Instant,
        paste_provenance: PasteProvenance,
    ) -> InputOutcome {
        debug_assert!(
            matches!(ev, Event::Paste(_)) || paste_provenance == PasteProvenance::Terminal,
            "non-paste events cannot carry paste provenance"
        );
        let normalized = self.keyboard_normalizer.rescue(ev);
        let ev: &Event = &normalized;
        let key_event = match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => Some(k),
            _ => None,
        };
        if let Event::Resize(_, rows) = ev {
            for agent in self.agents.values_mut() {
                agent.note_terminal_resize();
                for child in agent.subagent_views.values_mut() {
                    child.note_terminal_resize();
                }
            }
            self.last_known_terminal_rows = *rows;
            self.apply_effective_compact();
        }
        if let Some(key) = key_event
            && let Some(pending) = &self.pending_action
        {
            let stale_idle_arm_while_busy = matches!(
                pending.action,
                Action::ClearPrompt | Action::RewindShowPicker
            ) && matches!(
                self.active_view, ActiveView::Agent(id) if self.agents.get(& id)
                .is_some_and(| a | { a.session.state.is_turn_running() || a.session
                .state.is_cancelling() })
            );
            if !stale_idle_arm_while_busy && !pending.expired() && pending.shortcut.matches(key) {
                let action = self.pending_action.take().unwrap().action;
                return InputOutcome::Action(action);
            }
            self.pending_action = None;
        }
        let modal_open = self.is_scroll_blocking_modal_open();
        if let Event::Mouse(mouse) = ev
            && let Some(direction) = ScrollDirection::from_mouse_event(mouse)
            && !modal_open
        {
            let config = self
                .scroll_config
                .with_viewport_height(self.scroll_viewport_height());
            let update = self
                .scroll_state
                .on_scroll_event_at(arrived_at, direction, config);
            let pos = (mouse.column, mouse.row);
            self.last_scroll_pos = Some(pos);
            if update.lines != 0 {
                self.dispatch_scroll(update.lines, pos.0, pos.1);
                return InputOutcome::Changed;
            }
            return InputOutcome::Unchanged;
        }
        if let Event::Mouse(mouse) = ev {
            self.last_mouse_pos = Some((mouse.column, mouse.row));
            let is_mouse_action = matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left)
                    | MouseEventKind::Up(MouseButton::Left)
                    | MouseEventKind::Moved
            );
            if is_mouse_action {}
        }
        let zdr_blocked = self.is_zdr_blocked();
        let has_access = self.has_access();
        let welcome_pinned_upgrade_cta = crate::views::announcements::promo_cta(
            &self.active_announcements,
            &self.hidden_announcement_ids,
        )
        .is_some_and(|(owner, _, _)| !crate::views::announcements::is_dismissible(owner));
        let has_foreign_resume = self.foreign_resume_hint().is_some();
        let outcome = match self.active_view {
            ActiveView::Welcome => handle_welcome_input(
                ev,
                &mut WelcomeInputCtx {
                    auth_state: &self.auth_state,
                    trust_state: &self.trust_state,
                    cwd: &self.cwd,
                    mid_session_login: self.auth_return_view.is_some(),
                    auth_code_input: &mut self.auth_code_input,
                    prompt: &mut self.welcome_prompt,
                    prompt_focused: &mut self.welcome_prompt_focused,
                    new_worktree_dialog: &mut self.new_worktree_dialog,
                    menu_index: &mut self.welcome_menu_index,
                    menu_rects: &self.welcome_menu_rects,
                    menu_count: if zdr_blocked {
                        2
                    } else {
                        3 + if self.has_claude_import { 1 } else { 0 }
                            + if self.welcome_show_changelog_action {
                                1
                            } else {
                                0
                            }
                    },
                    prompt_rect: self.welcome_prompt_rect.as_ref(),
                    import_banner_rect: self.welcome_import_banner_rect.as_ref(),
                    auth_url_rect: self.welcome_auth_url_rect.as_ref(),
                    auth_fallback_rect: self.welcome_auth_fallback_rect.as_ref(),
                    refresh_rect: self.welcome_refresh_rect.as_ref(),
                    gate_url_rect: self.welcome_gate_url_rect.as_ref(),
                    upgrade_cta_rect: self.welcome_upgrade_cta_rect.as_ref(),
                    on_upgrade_cta: &mut self.welcome_on_upgrade_cta,
                    upgrade_cta_keyboard: welcome_pinned_upgrade_cta,
                    changelog_cta_rect: self.welcome_changelog_cta_rect.as_ref(),
                    on_changelog_cta: &mut self.welcome_on_changelog_cta,
                    announcement_truncated: self.welcome_announcement.truncated,
                    announcement_rect: self.welcome_announcement.rect.as_ref(),
                    on_announcement_cta: &mut self.welcome_announcement.on_cta,
                    announcement_expanded: &mut self.welcome_announcement.expanded,
                    show_raw_url: &mut self.auth_show_raw_url,
                    has_access,
                    is_zdr_blocked: zdr_blocked,
                    sp_entries: &mut self.session_picker_entries,
                    sp_state: &mut self.session_picker_state,
                    sp_content_results: &self.session_picker_content_results,
                    sp_content_loading: self.session_picker_content_loading,
                    sp_entries_query: &self.session_picker_entries_query,
                    has_claude_import: self.has_claude_import,
                    import_claude_modal: &mut self.import_claude_modal,
                    welcome_doc_viewer: &mut self.welcome_doc_viewer,
                    changelog_markdown: &self.changelog_markdown,
                    show_changelog_action: self.welcome_show_changelog_action,
                    has_pending_update: self.pending_update_version.is_some(),
                    has_foreign_resume,
                    cwd_has_git_ancestor: self.cwd_has_git_ancestor,
                    session_picker_grouped: self.session_picker_grouped,
                    sp_source_filter: &mut self.session_picker_source_filter,
                    chat_mode: self.chat_mode,
                },
            ),
            ActiveView::Agent(id) => {
                let overlay_active = self
                    .dashboard
                    .as_ref()
                    .is_some_and(|d| d.attached_agent == Some(id));
                if !overlay_active
                    && let Event::Key(key) = ev
                    && key.kind != KeyEventKind::Release
                {
                    match self
                        .registry
                        .lookup(key, crate::actions::When::DashboardOverlay)
                    {
                        Some(crate::actions::ActionId::DashboardOverlayPrev) => {
                            return InputOutcome::Action(Action::DashboardOverlayPrev);
                        }
                        Some(crate::actions::ActionId::DashboardOverlayNext) => {
                            return InputOutcome::Action(Action::DashboardOverlayNext);
                        }
                        _ => {}
                    }
                }
                if overlay_active {
                    if let Event::Key(key) = ev
                        && key.kind != KeyEventKind::Release
                    {
                        let lookup = self
                            .registry
                            .lookup(key, crate::actions::When::DashboardOverlay)
                            .or_else(|| self.registry.lookup(key, crate::actions::When::Always));
                        match lookup {
                            Some(crate::actions::ActionId::OpenDashboard)
                            | Some(crate::actions::ActionId::DashboardOverlayExit) => {
                                return InputOutcome::Action(Action::DashboardOverlayExit);
                            }
                            Some(crate::actions::ActionId::DashboardOverlayPrev) => {
                                return InputOutcome::Action(Action::DashboardOverlayPrev);
                            }
                            Some(crate::actions::ActionId::DashboardOverlayNext) => {
                                return InputOutcome::Action(Action::DashboardOverlayNext);
                            }
                            Some(crate::actions::ActionId::DashboardOverlayStop) => {
                                if self
                                    .agents
                                    .get(&id)
                                    .is_some_and(|a| a.session.state.is_turn_running())
                                {
                                    return InputOutcome::Action(Action::CancelTurn);
                                }
                                self.pending_action = Some(PendingAction::with_ttl(
                                    Action::DashboardOverlayStop,
                                    KeyShortcut::from(*key),
                                    Some("close this session"),
                                    crate::views::dashboard::state::STOP_CONFIRM_WINDOW,
                                ));
                                return InputOutcome::Changed;
                            }
                            _ => {}
                        }
                        if key.code == KeyCode::Left
                            && key.modifiers.is_empty()
                            && self
                                .agents
                                .get(&id)
                                .is_some_and(|a| a.is_empty_focused_prompt())
                        {
                            return InputOutcome::Action(Action::DashboardOverlayExit);
                        }
                        if key.code == KeyCode::Esc
                            && key.modifiers.is_empty()
                            && self
                                .agents
                                .get(&id)
                                .is_some_and(|a| a.overlay_esc_backs_out_from_prompt())
                        {
                            return InputOutcome::Action(Action::DashboardOverlayExit);
                        }
                        if key.modifiers.is_empty()
                            && self.agents.get(&id).is_some_and(|a| match key.code {
                                KeyCode::Esc => a.overlay_esc_backs_out(),
                                KeyCode::Left => a.overlay_left_backs_out(),
                                _ => false,
                            })
                        {
                            return InputOutcome::Action(Action::DashboardOverlayExit);
                        }
                        let neutral = self.agents.get(&id).is_some_and(|a| {
                            a.is_bare_scrollback() && a.no_input_overlay_pending()
                        });
                        if key.code == KeyCode::Char('q') && key.modifiers.is_empty() && neutral {
                            return InputOutcome::Action(Action::DashboardOverlayExit);
                        }
                        if key.code == KeyCode::Esc
                            && key.modifiers.is_empty()
                            && neutral
                            && self.agents.get(&id).is_some_and(|a| {
                                a.no_esc_consumer_pending()
                                    && !a.session.state.is_turn_running()
                                    && !a.session.state.is_cancelling()
                            })
                        {
                            return InputOutcome::Action(Action::DashboardOverlayExit);
                        }
                    }
                    if let Event::Mouse(mouse) = ev {
                        use crossterm::event::{MouseButton, MouseEventKind};
                        match mouse.kind {
                            MouseEventKind::Moved => {
                                let mut changed = false;
                                if let Some(d) = self.dashboard.as_mut() {
                                    changed |=
                                        d.overlay_close_hit.update_hover(mouse.column, mouse.row);
                                    changed |=
                                        d.overlay_prev_hit.update_hover(mouse.column, mouse.row);
                                    changed |=
                                        d.overlay_next_hit.update_hover(mouse.column, mouse.row);
                                }
                                if changed {
                                    return InputOutcome::Changed;
                                }
                            }
                            MouseEventKind::Down(MouseButton::Left) => {
                                if let Some(d) = self.dashboard.as_ref() {
                                    if d.overlay_close_hit.contains(mouse.column, mouse.row) {
                                        return InputOutcome::Action(Action::DashboardOverlayExit);
                                    }
                                    if d.overlay_prev_hit.contains(mouse.column, mouse.row) {
                                        return InputOutcome::Action(Action::DashboardOverlayPrev);
                                    }
                                    if d.overlay_next_hit.contains(mouse.column, mouse.row) {
                                        return InputOutcome::Action(Action::DashboardOverlayNext);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(modal) = self.import_claude_modal.as_mut() {
                    use crate::views::import_claude_modal::ImportClaudeModalOutcome;
                    let outcome_to_input = |o: ImportClaudeModalOutcome| match o {
                        ImportClaudeModalOutcome::Confirmed => {
                            InputOutcome::Action(Action::ImportClaudeConfirm)
                        }
                        ImportClaudeModalOutcome::Cancelled => {
                            InputOutcome::Action(Action::ImportClaudeCancel)
                        }
                        ImportClaudeModalOutcome::Changed => InputOutcome::Changed,
                        ImportClaudeModalOutcome::Unchanged => InputOutcome::Unchanged,
                    };
                    if let Event::Key(key) = ev {
                        if key.kind == KeyEventKind::Release {
                            return InputOutcome::Unchanged;
                        }
                        return outcome_to_input(modal.handle_key(key));
                    }
                    if let Event::Mouse(mouse) = ev {
                        return outcome_to_input(modal.handle_mouse(
                            mouse.kind,
                            mouse.column,
                            mouse.row,
                        ));
                    }
                    return InputOutcome::Unchanged;
                }
                if let Some(outcome) = self.voice_esc_outcome(key_event) {
                    return outcome;
                }
                if self.screen_mode.is_minimal()
                    && let Event::Key(key) = ev
                    && key.kind != KeyEventKind::Release
                    && let Some(outcome) = self.minimal_key_intercept(key)
                {
                    return outcome;
                }
                let prompt_paging = !overlay_active && !self.screen_mode.is_minimal();
                if self.screen_mode.is_minimal() {
                    match self.agents.get_mut(&id) {
                        Some(agent) => {
                            let outcome = agent.handle_minimal_input(ev, &self.registry);
                            if let Event::Key(key) = ev {
                                agent.record_input(key, &outcome);
                            }
                            self.pending_effects.append(&mut agent.pending_effects);
                            outcome
                        }
                        None => InputOutcome::Unchanged,
                    }
                } else {
                    match self.agents.get_mut(&id) {
                        Some(agent) => {
                            let outcome = if prompt_paging {
                                agent.handle_input_with_prompt_paging(ev, &self.registry)
                            } else {
                                agent.handle_input(ev, &self.registry)
                            };
                            if let Event::Key(key) = ev {
                                agent.record_input(key, &outcome);
                            }
                            self.pending_effects.append(&mut agent.pending_effects);
                            outcome
                        }
                        None => InputOutcome::Unchanged,
                    }
                }
            }
            ActiveView::AgentDashboard => {
                if let Some(outcome) = self.voice_esc_outcome(key_event) {
                    return outcome;
                }
                let attached_raw = self.dashboard.as_ref().and_then(|d| d.attached_agent);
                let attached = attached_raw.filter(|id| self.agents.contains_key(id));
                if attached_raw.is_some()
                    && attached.is_none()
                    && let Some(d) = self.dashboard.as_mut()
                {
                    d.close_popup();
                }
                if let Some(agent_id) = attached {
                    if let Event::Key(key) = ev
                        && key.kind != KeyEventKind::Release
                    {
                        let close_via_esc = key.code == KeyCode::Esc;
                        let close_via_action = self
                            .registry
                            .lookup(key, crate::actions::When::Always)
                            .is_some_and(|id| {
                                matches!(
                                    id,
                                    crate::actions::ActionId::OpenDashboard
                                        | crate::actions::ActionId::DashboardExit
                                )
                            });
                        if close_via_action || close_via_esc {
                            if let Some(d) = self.dashboard.as_mut() {
                                d.close_popup();
                            }
                            if let Some(agent) = self.agents.get_mut(&agent_id) {
                                agent.active_subagent = None;
                            }
                            return InputOutcome::Changed;
                        }
                    }
                    if let Event::Mouse(mouse) = ev
                        && matches!(
                            mouse.kind,
                            crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Left
                            )
                        )
                    {
                        let (close_rect, outer_rect, row_target) = {
                            let dash = self.dashboard.as_ref();
                            let close_rect = dash.and_then(|d| d.popup_close_rect);
                            let outer_rect = dash.and_then(|d| d.popup_outer_rect);
                            let row_target = dash.and_then(|d| {
                                d.row_rects
                                    .iter()
                                    .find(|(_, r)| {
                                        mouse.column >= r.x
                                            && mouse.column < r.x + r.width
                                            && mouse.row >= r.y
                                            && mouse.row < r.y + r.height
                                    })
                                    .map(|(id, _)| id.clone())
                            });
                            (close_rect, outer_rect, row_target)
                        };
                        let in_close = close_rect.is_some_and(|r| {
                            mouse.column >= r.x
                                && mouse.column < r.x + r.width
                                && mouse.row >= r.y
                                && mouse.row < r.y + r.height
                        });
                        let in_outer = outer_rect.is_some_and(|r| {
                            mouse.column >= r.x
                                && mouse.column < r.x + r.width
                                && mouse.row >= r.y
                                && mouse.row < r.y + r.height
                        });
                        if in_close {
                            if let Some(d) = self.dashboard.as_mut() {
                                d.close_popup();
                            }
                            if let Some(agent) = self.agents.get_mut(&agent_id) {
                                agent.active_subagent = None;
                            }
                            return InputOutcome::Changed;
                        }
                        if !in_outer && let Some(target) = row_target {
                            return InputOutcome::Action(Action::DashboardAttach(target));
                        }
                        if !in_outer {
                            return InputOutcome::Unchanged;
                        }
                    }
                    match self.agents.get_mut(&agent_id) {
                        Some(agent) => {
                            let outcome = agent.handle_input(ev, &self.registry);
                            if let Event::Key(key) = ev {
                                agent.record_input(key, &outcome);
                            }
                            self.pending_effects.append(&mut agent.pending_effects);
                            if matches!(outcome, InputOutcome::Action(Action::ExitSession)) {
                                if let Some(d) = self.dashboard.as_mut() {
                                    d.close_popup();
                                }
                                if let Some(agent) = self.agents.get_mut(&agent_id) {
                                    agent.active_subagent = None;
                                }
                                return InputOutcome::Changed;
                            }
                            outcome
                        }
                        None => InputOutcome::Unchanged,
                    }
                } else if let Some(ref mut dashboard) = self.dashboard {
                    let outcome = dashboard.handle_input_with_paste_provenance(
                        ev,
                        &self.registry,
                        paste_provenance,
                    );
                    self.pending_effects.append(&mut dashboard.pending_effects);
                    outcome
                } else {
                    InputOutcome::Unchanged
                }
            }
        };
        if let InputOutcome::Action(Action::Quit) = &outcome {
            return self.apply_quit_confirmation(key_event);
        }
        if let InputOutcome::Action(Action::QuitConfirmed) = &outcome {
            return InputOutcome::Action(Action::Quit);
        }
        if let InputOutcome::Action(Action::ExitSessionConfirmed) = &outcome {
            return InputOutcome::Action(Action::ExitSession);
        }
        if let InputOutcome::ArmPending {
            action,
            shortcut,
            label,
            ttl,
        } = outcome
        {
            self.pending_action = Some(PendingAction::with_ttl(action, shortcut, label, ttl));
            return InputOutcome::Changed;
        }
        if let InputOutcome::Action(Action::ExitSession) = &outcome
            && matches!(self.active_view, ActiveView::Agent(_))
        {
            return self.apply_exit_session_confirmation(key_event);
        }
        if matches!(
            outcome,
            InputOutcome::Action(Action::NewSession | Action::NewWorktreeSession { .. })
        ) && matches!(self.active_view, ActiveView::Agent(_))
            && let Some(key) = key_event
        {
            let (action, action_id) = match &outcome {
                InputOutcome::Action(Action::NewSession) => {
                    (Action::NewSession, ActionId::NewSession)
                }
                InputOutcome::Action(Action::NewWorktreeSession {
                    load_session_id,
                    label,
                    git_ref,
                }) => {
                    let action = Action::NewWorktreeSession {
                        load_session_id: load_session_id.clone(),
                        label: label.clone(),
                        git_ref: git_ref.clone(),
                    };
                    let shortcut = KeyShortcut::from(*key);
                    self.pending_action =
                        Some(PendingAction::new(action, shortcut, "new in worktree"));
                    return InputOutcome::Changed;
                }
                _ => unreachable!(),
            };
            if let Some(def) = self.registry.find(action_id)
                && def.requires_confirmation
            {
                let shortcut = if def.default_key.matches(key)
                    || def.alt_keys.iter().any(|alt| alt.matches(key))
                {
                    KeyShortcut::from(*key)
                } else {
                    def.default_key
                };
                self.pending_action = Some(PendingAction::new(action, shortcut, def.label));
                return InputOutcome::Changed;
            }
        }
        if !matches!(outcome, InputOutcome::Unchanged) {
            return outcome;
        }
        if matches!(ev, Event::Resize(_, _)) {
            return InputOutcome::Changed;
        }
        if let Some(key) = key_event
            && let Some(action_id) = self.registry.lookup(key, When::Always)
        {
            return self.handle_global_action(action_id, key);
        }
        if let Some(key) = key_event
            && (key!('c', CONTROL).matches(key) || key!('d', CONTROL).matches(key))
            && matches!(
                self.active_view,
                ActiveView::Agent(_) | ActiveView::AgentDashboard
            )
        {
            self.pending_action = Some(PendingAction::new(
                Action::Quit,
                KeyShortcut::from(*key),
                "quit",
            ));
            return InputOutcome::Changed;
        }
        InputOutcome::Unchanged
    }
    /// Handle a global-level action. Applies confirmation if required.
    fn handle_global_action(
        &mut self,
        action_id: ActionId,
        key: &crossterm::event::KeyEvent,
    ) -> InputOutcome {
        let Some(def) = self.registry.find(action_id) else {
            return InputOutcome::Unchanged;
        };
        let action = match action_id {
            ActionId::Quit => Action::Quit,
            ActionId::NewSession => Action::NewSession,
            ActionId::NewSessionInWorktree => Action::NewWorktreeSession {
                load_session_id: None,
                label: None,
                git_ref: None,
            },
            ActionId::OpenDashboard => Action::OpenDashboard,
            ActionId::VoiceToggle => Action::VoiceToggle,
            _ => return InputOutcome::Unchanged,
        };
        if def.requires_confirmation {
            let shortcut = KeyShortcut::from(*key);
            let action = if action_id == ActionId::NewSession
                && matches!(self.active_view, ActiveView::Agent(_))
                && self.new_session_worktree_mode == WorktreeMode::Ask
            {
                Action::ChooseNewSessionMode
            } else {
                action
            };
            self.pending_action = Some(PendingAction::new(action, shortcut, def.label));
            InputOutcome::Changed
        } else {
            InputOutcome::Action(action)
        }
    }
    /// Apply quit confirmation (double-press). Used both for direct global
    /// quit and for escalated quit from agent-level cancel.
    fn apply_quit_confirmation(
        &mut self,
        key_event: Option<&crossterm::event::KeyEvent>,
    ) -> InputOutcome {
        let Some(key) = key_event else {
            return InputOutcome::Action(Action::Quit);
        };
        let Some(def) = self.registry.find(ActionId::Quit) else {
            return InputOutcome::Action(Action::Quit);
        };
        if def.requires_confirmation {
            let shortcut = KeyShortcut::from(*key);
            self.pending_action = Some(PendingAction::new(Action::Quit, shortcut, def.label));
            InputOutcome::Changed
        } else {
            InputOutcome::Action(Action::Quit)
        }
    }
    /// Apply exit-session confirmation (double-press). Works like quit confirmation
    /// but transitions to the welcome screen instead of quitting.
    fn apply_exit_session_confirmation(
        &mut self,
        key_event: Option<&crossterm::event::KeyEvent>,
    ) -> InputOutcome {
        let Some(key) = key_event else {
            return InputOutcome::Action(Action::ExitSession);
        };
        let Some(def) = self.registry.find(ActionId::ExitSession) else {
            return InputOutcome::Action(Action::ExitSession);
        };
        if def.requires_confirmation {
            let shortcut = KeyShortcut::from(*key);
            self.pending_action =
                Some(PendingAction::new(Action::ExitSession, shortcut, def.label));
            InputOutcome::Changed
        } else {
            InputOutcome::Action(Action::ExitSession)
        }
    }
}
pub(crate) use crate::views::session_picker::filter_session_entries;
use crate::views::session_picker::{
    CONTENT_EXPAND_OFFSET, PickerItem, SessionPickerWorktreeSelection, build_entry_map,
    session_picker_worktree_selection, sync_session_picker_query_expansion,
};
/// Context for welcome-view input handling.
struct WelcomeInputCtx<'a> {
    auth_state: &'a AuthState,
    /// Folder-trust state. When `Pending` (and auth is `Done`), the trust
    /// question intercepts keys and swallows the rest so no session starts.
    trust_state: &'a TrustState,
    /// Live working directory (tracks `Effect::SetWorkingDir`), used to pin
    /// the current repo's group to the top of the session picker.
    cwd: &'a std::path::Path,
    /// `true` when the welcome screen is showing only to host a login flow
    /// that was started from inside a session. Esc / `q` then cancel the
    /// login and return to the session rather than quitting the app.
    mid_session_login: bool,
    auth_code_input: &'a mut LineEditor,
    prompt: &'a mut PromptWidget,
    prompt_focused: &'a mut bool,
    new_worktree_dialog: &'a mut Option<NewWorktreeDialogState>,
    menu_index: &'a mut Option<usize>,
    menu_rects: &'a [ratatui::layout::Rect],
    menu_count: usize,
    prompt_rect: Option<&'a ratatui::layout::Rect>,
    import_banner_rect: Option<&'a ratatui::layout::Rect>,
    auth_url_rect: Option<&'a ratatui::layout::Rect>,
    auth_fallback_rect: Option<&'a ratatui::layout::Rect>,
    refresh_rect: Option<&'a ratatui::layout::Rect>,
    gate_url_rect: Option<&'a ratatui::layout::Rect>,
    /// Hit-test rect for the welcome hero upgrade CTA `[label]` button
    /// (click → open the promo url).
    upgrade_cta_rect: Option<&'a ratatui::layout::Rect>,
    /// Sticky hover flag for the upgrade CTA (redraw on enter/leave so the
    /// button brightens/dims).
    on_upgrade_cta: &'a mut bool,
    /// A pinned (non-dismissible) promo CTA is live, so `Ctrl+O` opens it
    /// (the welcome screen has no YOLO toggle to preserve).
    upgrade_cta_keyboard: bool,
    /// Hit-test rect for the clickable changelog info block (opens release notes).
    changelog_cta_rect: Option<&'a ratatui::layout::Rect>,
    /// Sticky hover flag for the changelog block (redraw on enter/leave).
    on_changelog_cta: &'a mut bool,
    /// Whether the announcement overflowed — the "expandable" signal for click-to-toggle.
    announcement_truncated: bool,
    /// Hit-test rect for the full announcement block (click anywhere to toggle).
    announcement_rect: Option<&'a ratatui::layout::Rect>,
    /// Sticky hover flag for the announcement block (redraw on enter/leave).
    on_announcement_cta: &'a mut bool,
    /// Whether the long announcement is currently expanded inline.
    announcement_expanded: &'a mut bool,
    show_raw_url: &'a mut bool,
    has_access: bool,
    is_zdr_blocked: bool,
    sp_entries: &'a mut Option<Vec<SessionPickerEntry>>,
    sp_state: &'a mut crate::views::picker::PickerState,
    sp_content_results:
        &'a Option<Vec<xai_grok_shell::extensions::session_search::SearchSessionHit>>,
    sp_content_loading: bool,
    /// The query `sp_entries` were server-fetched with (see
    /// [`crate::views::session_picker::effective_filter_query`]).
    sp_entries_query: &'a Option<String>,
    has_claude_import: bool,
    import_claude_modal: &'a mut Option<crate::views::import_claude_modal::ImportClaudeModalState>,
    welcome_doc_viewer: &'a mut Option<crate::views::modal::ActiveModal>,
    changelog_markdown: &'a Option<String>,
    /// Whether the welcome menu currently includes a "Changelog" row (above
    /// Quit), so index→action mapping accounts for it.
    show_changelog_action: bool,
    has_pending_update: bool,
    /// A recent foreign session is available to resume when no update is pending.
    has_foreign_resume: bool,
    cwd_has_git_ancestor: bool,
    session_picker_grouped: bool,
    sp_source_filter: &'a mut crate::views::session_picker::SourceFilter,
    /// Process-wide `--chat`: the session picker hides its Local/Remote
    /// source filter (conversations-only list), so `f` must not cycle it.
    chat_mode: bool,
}
/// Welcome view input -- auth-state-aware routing.
fn handle_welcome_input(ev: &Event, ctx: &mut WelcomeInputCtx<'_>) -> InputOutcome {
    if let Some(modal) = ctx.import_claude_modal.as_mut() {
        use crate::views::import_claude_modal::ImportClaudeModalOutcome;
        let outcome_to_input = |o: ImportClaudeModalOutcome| match o {
            ImportClaudeModalOutcome::Confirmed => {
                InputOutcome::Action(Action::ImportClaudeConfirm)
            }
            ImportClaudeModalOutcome::Cancelled => InputOutcome::Action(Action::ImportClaudeCancel),
            ImportClaudeModalOutcome::Changed => InputOutcome::Changed,
            ImportClaudeModalOutcome::Unchanged => InputOutcome::Unchanged,
        };
        if let Event::Key(key) = ev {
            if key.kind == crossterm::event::KeyEventKind::Release {
                return InputOutcome::Unchanged;
            }
            return outcome_to_input(modal.handle_key(key));
        }
        if let Event::Mouse(mouse) = ev {
            return outcome_to_input(modal.handle_mouse(mouse.kind, mouse.column, mouse.row));
        }
        return InputOutcome::Unchanged;
    }
    if let Some(modal) = ctx.welcome_doc_viewer {
        if let Event::Key(key) = ev {
            if key.kind == crossterm::event::KeyEventKind::Release {
                return InputOutcome::Unchanged;
            }
            use crate::views::modal_window as mw;
            if let crate::views::modal::ActiveModal::DocViewer { window, scroll, .. } = modal {
                let chrome_cfg = mw::ModalWindowConfig {
                    title: "",
                    tabs: None,
                    shortcuts: &[],
                    sizing: mw::ModalSizing::default(),
                    fold_info: None,
                };
                match mw::handle_modal_key(window, key, &chrome_cfg) {
                    mw::ModalWindowOutcome::CloseRequested => {
                        *ctx.welcome_doc_viewer = None;
                        return InputOutcome::Changed;
                    }
                    mw::ModalWindowOutcome::Unhandled => {
                        if crate::views::modal::apply_doc_scroll(key.code, scroll) {
                            return InputOutcome::Changed;
                        }
                        return InputOutcome::Unchanged;
                    }
                    _ => return InputOutcome::Changed,
                }
            }
        }
        if let Event::Mouse(mouse) = ev {
            use crate::views::modal_window as mw;
            if let crate::views::modal::ActiveModal::DocViewer { window, scroll, .. } = modal {
                match mw::handle_modal_mouse(window, mouse.kind, mouse.column, mouse.row) {
                    mw::ModalWindowOutcome::CloseRequested => {
                        *ctx.welcome_doc_viewer = None;
                        return InputOutcome::Changed;
                    }
                    mw::ModalWindowOutcome::Unhandled => {
                        if crate::views::modal::apply_doc_mouse_scroll(mouse.kind, scroll) {
                            return InputOutcome::Changed;
                        }
                    }
                    _ => return InputOutcome::Changed,
                }
            }
        }
        return InputOutcome::Unchanged;
    }
    if let Some(dialog) = ctx.new_worktree_dialog.as_mut() {
        let outcome = match ev {
            Event::Key(key) if key.kind != crossterm::event::KeyEventKind::Release => {
                dialog.handle_key(key)
            }
            Event::Paste(text) => dialog.insert_paste(text),
            Event::Resize(_, _) => return InputOutcome::Changed,
            _ => NewWorktreeDialogOutcome::Unchanged,
        };
        match outcome {
            NewWorktreeDialogOutcome::Submitted(label) => {
                *ctx.new_worktree_dialog = None;
                return InputOutcome::Action(Action::NewWorktreeSession {
                    load_session_id: None,
                    label,
                    git_ref: None,
                });
            }
            NewWorktreeDialogOutcome::Cancelled => {
                *ctx.new_worktree_dialog = None;
                return InputOutcome::Changed;
            }
            NewWorktreeDialogOutcome::Changed => return InputOutcome::Changed,
            NewWorktreeDialogOutcome::Unchanged => return InputOutcome::Unchanged,
        }
    }
    if matches!(ctx.auth_state, AuthState::Done)
        && ctx.has_access
        && !ctx.is_zdr_blocked
        && matches!(ctx.trust_state, TrustState::Pending { .. })
    {
        if let Event::Key(key) = ev {
            if key.kind == KeyEventKind::Release {
                return InputOutcome::Unchanged;
            }
            if key!('y').matches(key) || key!('Y').matches(key) || key!(Enter).matches(key) {
                return InputOutcome::Action(Action::TrustFolder);
            }
            if key!('n').matches(key) || key!('N').matches(key) || key!(Esc).matches(key) {
                return InputOutcome::Action(Action::QuitConfirmed);
            }
            if key!('c', CONTROL).matches(key) || key!('d', CONTROL).matches(key) {
                return InputOutcome::Action(Action::Quit);
            }
            return InputOutcome::Unchanged;
        }
        if let Event::Mouse(mouse) = ev
            && matches!(
                mouse.kind,
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
            )
        {
            for (i, rect) in ctx.menu_rects.iter().enumerate() {
                if rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row)) {
                    return InputOutcome::Action(if i == 0 {
                        Action::TrustFolder
                    } else {
                        Action::QuitConfirmed
                    });
                }
            }
            return InputOutcome::Unchanged;
        }
        if matches!(ev, Event::Resize(_, _)) {
            return InputOutcome::Changed;
        }
        return InputOutcome::Unchanged;
    }
    if ctx.sp_entries.is_some() && matches!(ctx.auth_state, AuthState::Done) {
        use crate::views::picker::{PickerConfig, PickerOutcome, handle_picker_input};
        let source_filter = *ctx.sp_source_filter;
        let current_repo =
            crate::views::session_picker::repo_name_from_cwd(&ctx.cwd.to_string_lossy());
        let entry_map = build_entry_map(
            ctx.sp_entries.as_deref(),
            ctx.sp_content_results.as_deref(),
            crate::views::session_picker::effective_filter_query(
                ctx.sp_state.query(),
                ctx.sp_entries_query.as_deref(),
            ),
            ctx.session_picker_grouped,
            ctx.sp_content_loading,
            source_filter,
            Some(current_repo.as_str()),
        );
        let entry_count = entry_map.len();
        let non_selectable_flags: Vec<bool> = entry_map.iter().map(|e| e.is_none()).collect();
        let config = PickerConfig {
            title: Some("Resume session"),
            show_search_hint: true,
            expandable: true,
            esc_clears_query: true,
            shortcuts: Some(crate::views::picker::picker_shortcuts()),
            pending_hint: None,
            non_selectable: &non_selectable_flags,
            non_selectable_clickable: &[],
            shortcuts_area: None,
            tabs: None,
            active_tab: 0,
            filter_label: (!ctx.chat_mode).then(|| source_filter.label()),
            filter_key_hint: (!ctx.chat_mode).then_some("f"),
            filter_active: !ctx.chat_mode && source_filter.is_active(),
            action_keys: &[],
            disable_search: false,
            compact_bottom_bar: false,
            search_only_on_slash: false,
            vim_normal_first: crate::appearance::cache::load_vim_mode(),
        };
        if let Event::Key(key) = ev {
            if key.kind == KeyEventKind::Press
                && (key!('c', CONTROL).matches(key) || key!('d', CONTROL).matches(key))
            {
                return InputOutcome::Action(Action::Quit);
            }
            if let Some(selection) = session_picker_worktree_selection(
                key,
                ctx.sp_state,
                &entry_map,
                &non_selectable_flags,
                ctx.sp_entries.as_deref(),
                ctx.sp_content_results.as_deref(),
            ) {
                return InputOutcome::Action(match selection {
                    SessionPickerWorktreeSelection::Fuzzy(original_index) => {
                        Action::PickSessionInWorktree(original_index)
                    }
                    SessionPickerWorktreeSelection::Content { session_id, cwd } => {
                        Action::PickContentSessionInWorktree { session_id, cwd }
                    }
                    SessionPickerWorktreeSelection::Unavailable => {
                        return InputOutcome::Changed;
                    }
                });
            }
        }
        let outcome = handle_picker_input(ev, ctx.sp_state, entry_count, &config);
        match outcome {
            PickerOutcome::Selected(i) => match entry_map.get(i).and_then(|e| e.as_ref()) {
                Some(PickerItem::Fuzzy { original_index }) => {
                    return InputOutcome::Action(Action::PickSession(*original_index));
                }
                Some(PickerItem::Content { hit_index }) => {
                    if let Some(hits) = ctx.sp_content_results.as_ref()
                        && let Some(hit) = hits.get(*hit_index)
                    {
                        return InputOutcome::Action(Action::PickContentSession {
                            session_id: hit.session_id.clone(),
                            cwd: hit.cwd.clone(),
                        });
                    }
                    return InputOutcome::Changed;
                }
                None => return InputOutcome::Changed,
            },
            PickerOutcome::SubmitQuery => {
                let query = ctx.sp_state.query().trim().to_string();
                if !query.is_empty() {
                    return InputOutcome::Action(Action::LoadSession(query, None, false));
                }
                return InputOutcome::Unchanged;
            }
            PickerOutcome::Closed => {
                *ctx.sp_entries = None;
                ctx.sp_state.reset();
                *ctx.sp_source_filter = crate::views::session_picker::SourceFilter::default();
                return InputOutcome::Action(Action::SessionPickerClosed);
            }
            PickerOutcome::Expand(i) => {
                match entry_map.get(i).and_then(|e| e.as_ref()) {
                    Some(PickerItem::Fuzzy { original_index }) => {
                        if let Some(ents) = ctx.sp_entries.as_ref()
                            && let Some(entry) = ents.get(*original_index)
                            && !crate::app::foreign_sessions::is_foreign_picker_source(
                                &entry.source,
                            )
                        {
                            return InputOutcome::Action(Action::ExpandSessionCard {
                                source: entry.source.clone(),
                                session_id: entry.id.clone(),
                            });
                        }
                    }
                    Some(PickerItem::Content { hit_index }) => {
                        if let Some(hits) = ctx.sp_content_results.as_ref()
                            && let Some(hit) = hits.get(*hit_index)
                        {
                            return InputOutcome::Action(Action::ExpandSessionCard {
                                source: "local".into(),
                                session_id: hit.session_id.clone(),
                            });
                        }
                    }
                    None => {}
                }
                return InputOutcome::Changed;
            }
            PickerOutcome::Collapse(i) => {
                match entry_map.get(i).and_then(|e| e.as_ref()) {
                    Some(PickerItem::Fuzzy { original_index }) => {
                        if ctx.sp_state.expanded.contains(original_index)
                            && let Some(ents) = ctx.sp_entries.as_ref()
                            && let Some(entry) = ents.get(*original_index)
                        {
                            return InputOutcome::Action(Action::ExpandSessionCard {
                                source: entry.source.clone(),
                                session_id: entry.id.clone(),
                            });
                        }
                    }
                    Some(PickerItem::Content { hit_index }) => {
                        let key = CONTENT_EXPAND_OFFSET + hit_index;
                        if ctx.sp_state.expanded.contains(&key)
                            && let Some(hits) = ctx.sp_content_results.as_ref()
                            && let Some(hit) = hits.get(*hit_index)
                        {
                            return InputOutcome::Action(Action::ExpandSessionCard {
                                source: "local".into(),
                                session_id: hit.session_id.clone(),
                            });
                        }
                    }
                    None => {}
                }
                return InputOutcome::Changed;
            }
            PickerOutcome::Copy(i) => {
                if let Some(Some(PickerItem::Fuzzy { original_index })) = entry_map.get(i) {
                    return InputOutcome::Action(Action::CopySessionId(*original_index));
                }
                return InputOutcome::Changed;
            }
            PickerOutcome::QueryChanged => {
                sync_session_picker_query_expansion(
                    ctx.sp_entries.as_deref(),
                    ctx.sp_content_results.as_deref(),
                    ctx.sp_entries_query.as_deref(),
                    ctx.sp_state,
                    ctx.session_picker_grouped,
                    ctx.sp_content_loading,
                    source_filter,
                    Some(current_repo.as_str()),
                );
                return InputOutcome::Action(Action::TriggerDeepSearch);
            }
            PickerOutcome::Changed => return InputOutcome::Changed,
            PickerOutcome::Unchanged => {
                if let Event::Key(key) = ev
                    && key.kind == KeyEventKind::Press
                {
                    if key!('/', CONTROL).matches(key) && !ctx.sp_state.query().trim().is_empty() {
                        return InputOutcome::Action(Action::ForceDeepSearch);
                    }
                }
                return InputOutcome::Unchanged;
            }
            PickerOutcome::FilterCycled => {
                return InputOutcome::Action(Action::CycleSessionSourceFilter);
            }
            PickerOutcome::NonSelectableClick(_)
            | PickerOutcome::TabChanged(_)
            | PickerOutcome::Action(_) => {
                return InputOutcome::Changed;
            }
        }
    }
    if let Event::Key(key) = ev {
        if key.kind == KeyEventKind::Release {
            return InputOutcome::Unchanged;
        }
        if ctx.is_zdr_blocked && matches!(ctx.auth_state, AuthState::Done) {
            return handle_menu_shortcuts(
                key,
                ctx.menu_index,
                &['l', 'q'],
                dispatch_zdr_menu_action,
            );
        }
        if !ctx.has_access && matches!(ctx.auth_state, AuthState::Done) {
            return handle_menu_shortcuts(
                key,
                ctx.menu_index,
                &['g', 'l', 'q'],
                dispatch_access_gate_menu_action,
            );
        }
        if matches!(ctx.auth_state, AuthState::Done)
            && key!(Enter).matches(key)
            && key.modifiers.is_empty()
        {
            return InputOutcome::Action(Action::NewSession);
        }
        if matches!(ctx.auth_state, AuthState::Done) {
            if ctx.upgrade_cta_keyboard && key!('o', CONTROL).matches(key) {
                return InputOutcome::Action(Action::AnnouncementsOpenCta(
                    xai_grok_telemetry::events::AnnouncementCtaSurface::Keyboard,
                ));
            }
            if key!('w', CONTROL).matches(key) && ctx.cwd_has_git_ancestor {
                return InputOutcome::Action(Action::OpenNewWorktreeDialog);
            }
            if key!('s', CONTROL).matches(key) {
                return InputOutcome::Action(Action::FetchSessionList);
            }
            if ctx.has_pending_update && key!('u', CONTROL).matches(key) {
                return InputOutcome::Action(Action::QuitForUpdate);
            }
            if ctx.has_foreign_resume && key!('u', CONTROL).matches(key) {
                return InputOutcome::Action(Action::ResumeForeignSession);
            }
            if ctx.has_claude_import && key!('i', CONTROL).matches(key) {
                return InputOutcome::Action(Action::ImportClaudeSettings);
            }
            if ctx.has_claude_import && key!('I', CONTROL | SHIFT).matches(key) {
                return InputOutcome::Action(Action::DismissClaudeImport);
            }
        }
        if matches!(ctx.auth_state, AuthState::Done) && crate::input::key::is_shift_tab(key) {
            return InputOutcome::ActionThenForward(Action::NewSession);
        }
        if *ctx.prompt_focused
            && matches!(ctx.auth_state, AuthState::Done)
            && let KeyCode::Char(ch) = key.code
            && (crate::input::key::is_text_input_key(key)
                || (ch == 'v' && crate::input::key::is_paste_key(key)))
        {
            return InputOutcome::ActionThenForward(Action::NewSession);
        }
        if *ctx.prompt_focused {
            match ctx.prompt.handle_key(key) {
                crate::views::prompt_widget::PromptEvent::Edited => {
                    return InputOutcome::Changed;
                }
                crate::views::prompt_widget::PromptEvent::Ignored => {
                    if key!(Esc).matches(key) {
                        *ctx.prompt_focused = false;
                        return InputOutcome::Changed;
                    }
                }
            }
        }
        if !*ctx.prompt_focused && matches!(ctx.auth_state, AuthState::Done) {
            if let Some(outcome) = handle_menu_nav(key, ctx.menu_index, ctx.menu_count) {
                return outcome;
            }
            if key!(Enter).matches(key)
                && let Some(idx) = *ctx.menu_index
            {
                return dispatch_menu_action(
                    idx,
                    ctx.has_claude_import,
                    ctx.show_changelog_action,
                    ctx.changelog_markdown.as_deref(),
                );
            }
            if crate::input::key::is_text_input_key(key) {
                *ctx.prompt_focused = true;
                *ctx.menu_index = None;
                return InputOutcome::ActionThenForward(Action::NewSession);
            }
        }
        match ctx.auth_state {
            AuthState::Done => {
                if key!('c', CONTROL).matches(key) || key!('d', CONTROL).matches(key) {
                    return InputOutcome::Action(Action::Quit);
                }
            }
            AuthState::Pending { .. } => {
                if key!('q').matches(key)
                    || key!('c', CONTROL).matches(key)
                    || key!('d', CONTROL).matches(key)
                {
                    if ctx.mid_session_login {
                        return InputOutcome::Action(Action::CancelLogin);
                    }
                    return InputOutcome::Action(Action::QuitConfirmed);
                }
                if key!('l').matches(key) || key!(Enter).matches(key) {
                    return InputOutcome::Action(Action::Login);
                }
            }
            AuthState::Authenticating { .. } if *ctx.show_raw_url => {
                if key!('q', CONTROL).matches(key) || key!('c', CONTROL).matches(key) {
                    return InputOutcome::Action(Action::HideRawAuthUrl);
                }
                return InputOutcome::Unchanged;
            }
            AuthState::Authenticating {
                mode: AuthMode::Loopback,
                ..
            } => {
                if key!(Esc).matches(key)
                    || key!('q', CONTROL).matches(key)
                    || key!('c', CONTROL).matches(key)
                {
                    if ctx.mid_session_login {
                        return InputOutcome::Action(Action::CancelLogin);
                    }
                    return InputOutcome::Action(Action::QuitConfirmed);
                }
                if key!(Enter).matches(key) {
                    let trimmed = ctx.auth_code_input.text().trim().to_string();
                    if !trimmed.is_empty() {
                        return InputOutcome::Action(Action::SubmitAuthCode(trimmed));
                    }
                    return InputOutcome::Unchanged;
                }
                let outcome = if crate::input::key::is_paste_key(key) {
                    let Some(text) = crate::clipboard::system_clipboard_get() else {
                        return InputOutcome::Unchanged;
                    };
                    ctx.auth_code_input.insert_paste(&text)
                } else if key.modifiers.intersects(
                    crossterm::event::KeyModifiers::CONTROL
                        | crossterm::event::KeyModifiers::ALT
                        | crossterm::event::KeyModifiers::SUPER,
                ) && !crate::input::key::is_altgr(key.modifiers)
                {
                    return InputOutcome::Changed;
                } else {
                    ctx.auth_code_input
                        .handle_key_with_insert_policy(key, |character| !character.is_control())
                };
                return match outcome {
                    LineEditOutcome::TextChanged
                    | LineEditOutcome::CursorChanged
                    | LineEditOutcome::HandledNoChange => InputOutcome::Changed,
                    LineEditOutcome::Unhandled => InputOutcome::Unchanged,
                };
            }
            AuthState::Authenticating { .. } => {
                if key!(Esc).matches(key)
                    || key!('q', CONTROL).matches(key)
                    || key!('c', CONTROL).matches(key)
                {
                    if ctx.mid_session_login {
                        return InputOutcome::Action(Action::CancelLogin);
                    }
                    return InputOutcome::Action(Action::QuitConfirmed);
                }
            }
        }
    }
    if let Event::Paste(text) = ev {
        match ctx.auth_state {
            AuthState::Done => {
                if !ctx.has_access || ctx.is_zdr_blocked {
                    return InputOutcome::Unchanged;
                }
                return InputOutcome::ActionThenForward(Action::NewSession);
            }
            AuthState::Authenticating {
                mode: AuthMode::Loopback,
                ..
            } => {
                let _ = ctx.auth_code_input.insert_paste(text);
                return InputOutcome::Changed;
            }
            _ => {}
        }
    }
    if matches!(ev, Event::Resize(_, _)) {
        return InputOutcome::Changed;
    }
    if let Event::Mouse(mouse) = ev {
        use crossterm::event::{MouseButton, MouseEventKind};
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                for (i, rect) in ctx.menu_rects.iter().enumerate() {
                    if mouse.column >= rect.x
                        && mouse.column < rect.x + rect.width
                        && mouse.row >= rect.y
                        && mouse.row < rect.y + rect.height
                    {
                        if matches!(ctx.auth_state, AuthState::Pending { .. }) {
                            return dispatch_pending_menu_action(i);
                        }
                        if ctx.is_zdr_blocked {
                            return dispatch_zdr_menu_action(i);
                        }
                        if !ctx.has_access {
                            return dispatch_access_gate_menu_action(i);
                        }
                        if ctx.has_claude_import
                            && i == 0
                            && mouse.column >= rect.x + rect.width.saturating_sub(4)
                            && mouse.column < rect.x + rect.width.saturating_sub(1)
                        {
                            return InputOutcome::Action(Action::DismissClaudeImport);
                        }
                        return dispatch_menu_action(
                            i,
                            ctx.has_claude_import,
                            ctx.show_changelog_action,
                            ctx.changelog_markdown.as_deref(),
                        );
                    }
                }
                if let Some(rect) = ctx.refresh_rect
                    && rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                {
                    return InputOutcome::Action(Action::CheckSubscription);
                }
                if let Some(rect) = ctx.gate_url_rect
                    && rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                {
                    return InputOutcome::Action(Action::OpenSupergrokUrl);
                }
                if let Some(rect) = ctx.upgrade_cta_rect
                    && rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                {
                    return InputOutcome::Action(Action::AnnouncementsOpenCta(
                        xai_grok_telemetry::events::AnnouncementCtaSurface::Welcome,
                    ));
                }
                if let Some(rect) = ctx.changelog_cta_rect
                    && rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                    && let Some(md) = ctx.changelog_markdown.as_deref()
                {
                    return InputOutcome::Action(Action::ShowReleaseNotes {
                        title: "Release Notes".to_string(),
                        content: md.trim().to_string(),
                    });
                }
                if let Some(rect) = ctx.announcement_rect
                    && (ctx.announcement_truncated || *ctx.announcement_expanded)
                    && rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                {
                    *ctx.announcement_expanded = !*ctx.announcement_expanded;
                    return InputOutcome::Changed;
                }
                if let Some(rect) = ctx.auth_url_rect
                    && matches!(ctx.auth_state, AuthState::Authenticating { .. })
                    && rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                {
                    return InputOutcome::Action(Action::CopyAuthUrl);
                }
                if let Some(rect) = ctx.auth_fallback_rect
                    && matches!(ctx.auth_state, AuthState::Authenticating { .. })
                    && rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                {
                    return InputOutcome::Action(Action::ShowRawAuthUrl);
                }
                if let Some(rect) = ctx.import_banner_rect
                    && matches!(ctx.auth_state, AuthState::Done)
                    && mouse.column >= rect.x
                    && mouse.column < rect.x + rect.width
                    && mouse.row >= rect.y
                    && mouse.row < rect.y + rect.height
                {
                    return InputOutcome::Action(Action::ImportClaudeSettings);
                }
                if let Some(rect) = ctx.prompt_rect
                    && matches!(ctx.auth_state, AuthState::Done)
                    && mouse.column >= rect.x
                    && mouse.column < rect.x + rect.width
                    && mouse.row >= rect.y
                    && mouse.row < rect.y + rect.height
                {
                    *ctx.prompt_focused = true;
                    return InputOutcome::Changed;
                }
            }
            MouseEventKind::Moved => {
                let mut new_index = None;
                for (i, rect) in ctx.menu_rects.iter().enumerate() {
                    if mouse.column >= rect.x
                        && mouse.column < rect.x + rect.width
                        && mouse.row >= rect.y
                        && mouse.row < rect.y + rect.height
                    {
                        new_index = Some(i);
                        break;
                    }
                }
                if new_index != *ctx.menu_index {
                    *ctx.menu_index = new_index;
                    return InputOutcome::Changed;
                }
                if ctx.has_claude_import && new_index == Some(0) {
                    return InputOutcome::Changed;
                }
                let pos = ratatui::layout::Position::new(mouse.column, mouse.row);
                let over_cta = ctx.changelog_cta_rect.is_some_and(|r| r.contains(pos));
                if over_cta != *ctx.on_changelog_cta {
                    *ctx.on_changelog_cta = over_cta;
                    return InputOutcome::Changed;
                }
                let over_upgrade = ctx.upgrade_cta_rect.is_some_and(|r| r.contains(pos));
                if over_upgrade != *ctx.on_upgrade_cta {
                    *ctx.on_upgrade_cta = over_upgrade;
                    return InputOutcome::Changed;
                }
                let over_ann = (ctx.announcement_truncated || *ctx.announcement_expanded)
                    && ctx.announcement_rect.is_some_and(|r| r.contains(pos));
                if over_ann != *ctx.on_announcement_cta {
                    *ctx.on_announcement_cta = over_ann;
                    return InputOutcome::Changed;
                }
                if matches!(ctx.auth_state, AuthState::Authenticating { .. })
                    && ctx.auth_url_rect.is_some()
                {
                    return InputOutcome::Changed;
                }
            }
            _ => {}
        }
    }
    InputOutcome::Unchanged
}
/// Handle Up/Down arrow key cycling through a menu of `count` items.
fn is_quit_signal(key: &crossterm::event::KeyEvent) -> bool {
    key!('c', CONTROL).matches(key) || key!('d', CONTROL).matches(key)
}
/// `shortcuts[i]` triggers `menu_dispatch(i)`.
fn handle_menu_shortcuts(
    key: &crossterm::event::KeyEvent,
    menu_index: &mut Option<usize>,
    shortcuts: &[char],
    menu_dispatch: fn(usize) -> InputOutcome,
) -> InputOutcome {
    if is_quit_signal(key) {
        return InputOutcome::Action(Action::Quit);
    }
    for (i, &ch) in shortcuts.iter().enumerate() {
        if key.code == KeyCode::Char(ch) {
            return menu_dispatch(i);
        }
    }
    if key!(Enter).matches(key) {
        return menu_dispatch(menu_index.unwrap_or(0));
    }
    if let Some(outcome) = handle_menu_nav(key, menu_index, shortcuts.len()) {
        return outcome;
    }
    InputOutcome::Unchanged
}
fn handle_menu_nav(
    key: &crossterm::event::KeyEvent,
    index: &mut Option<usize>,
    count: usize,
) -> Option<InputOutcome> {
    match key.code {
        KeyCode::Down => {
            *index = Some(match *index {
                Some(i) if i + 1 < count => i + 1,
                Some(_) | None => 0,
            });
            Some(InputOutcome::Changed)
        }
        KeyCode::Up => {
            *index = Some(match *index {
                Some(0) | None => count.saturating_sub(1),
                Some(i) => i - 1,
            });
            Some(InputOutcome::Changed)
        }
        _ => None,
    }
}
/// Dispatch an action for a welcome menu item when not yet authenticated.
/// Menu layout: 0 = Login, 1 = Quit.
fn dispatch_pending_menu_action(index: usize) -> InputOutcome {
    match index {
        0 => InputOutcome::Action(Action::Login),
        1 => InputOutcome::Action(Action::Quit),
        _ => InputOutcome::Unchanged,
    }
}
/// Dispatch an action for a welcome menu item when ZDR-blocked.
/// Menu layout: 0 = Switch account, 1 = Quit.
fn dispatch_zdr_menu_action(index: usize) -> InputOutcome {
    match index {
        0 => InputOutcome::Action(Action::SwitchAccount),
        1 => InputOutcome::Action(Action::Quit),
        _ => InputOutcome::Unchanged,
    }
}
/// Menu actions when user is access-gated: 0 = Subscribe CTA, 1 = Logout, 2 = Quit.
/// "Refresh" (ctrl-r) is handled as a direct key shortcut, not a menu item.
fn dispatch_access_gate_menu_action(index: usize) -> InputOutcome {
    match index {
        0 => InputOutcome::Action(Action::OpenSupergrokUrl),
        1 => InputOutcome::Action(Action::Logout),
        2 => InputOutcome::Action(Action::Quit),
        _ => InputOutcome::Unchanged,
    }
}
/// Dispatch an action for a welcome menu item by index.
///
/// Menu order: `[Import]`, New worktree, Resume session, `[Changelog]`, Quit.
/// `show_changelog_action` is true when the Changelog row is rendered; release
/// notes open only once `changelog_md` is available.
fn dispatch_menu_action(
    index: usize,
    has_claude_import: bool,
    show_changelog_action: bool,
    changelog_md: Option<&str>,
) -> InputOutcome {
    let base = if has_claude_import { 1 } else { 0 };
    let worktree_idx = base;
    let resume_idx = base + 1;
    let (changelog_idx, quit_idx) = if show_changelog_action {
        (Some(base + 2), base + 3)
    } else {
        (None, base + 2)
    };
    if has_claude_import && index == 0 {
        return InputOutcome::Action(Action::ImportClaudeSettings);
    }
    if index == worktree_idx {
        return InputOutcome::Action(Action::OpenNewWorktreeDialog);
    }
    if index == resume_idx {
        return InputOutcome::Action(Action::FetchSessionList);
    }
    if Some(index) == changelog_idx {
        if let Some(md) = changelog_md {
            return InputOutcome::Action(Action::ShowReleaseNotes {
                title: "Release Notes".to_string(),
                content: md.trim().to_string(),
            });
        }
        return InputOutcome::Unchanged;
    }
    if index == quit_idx {
        return InputOutcome::Action(Action::Quit);
    }
    InputOutcome::Unchanged
}
impl AppView {
    /// Merge notification escape sequences with render-produced post-flush
    /// escapes. Both inputs are optional; returns `None` only when both are
    /// `None`.
    fn merge_escapes(
        notif: Option<String>,
        render: Option<crate::terminal::overlay::PostFlush>,
    ) -> Option<crate::terminal::overlay::PostFlush> {
        Self::merge_post_flush(
            notif.map(crate::terminal::overlay::PostFlush::plain),
            render,
        )
    }
    fn merge_post_flush(
        first: Option<crate::terminal::overlay::PostFlush>,
        second: Option<crate::terminal::overlay::PostFlush>,
    ) -> Option<crate::terminal::overlay::PostFlush> {
        match (first, second) {
            (Some(mut first), Some(second)) => {
                first.append(second);
                Some(first)
            }
            (Some(first), None) => Some(first),
            (None, second) => second,
        }
    }
    /// Build the Kitty delete escapes that remove image placements left
    /// behind by agent views that are not drawn this frame.
    ///
    /// Kitty graphics survive cell redraws until explicitly deleted, and
    /// every regular clear lives inside `AgentView::draw` / the prompt
    /// widget's per-frame self-heal. Once the dashboard takes over the
    /// frame those paths stop running, so an image overlay (or inline
    /// scrollback media) the user left open in the agent view would float
    /// above the dashboard forever. Called from the
    /// `ActiveView::AgentDashboard` draw branch every frame:
    ///
    /// - Placement id 1 is cleared only when no popup agent is drawn; a popup
    ///   owns and reuses that slot across consecutive dashboard frames.
    /// - Inline scrollback media ids (2+) are drained per agent via
    ///   `AgentView::take_inline_media_clear_escapes`, which resets the
    ///   agent's placement tracking — a one-shot sweep per transition,
    ///   not a per-frame cost. The popup-attached agent is skipped: it
    ///   just drew and manages its own placements. The clears-before-popup
    ///   ordering also means a drained id that collides with one the popup
    ///   re-places this frame ends up displayed, not deleted.
    fn dashboard_stale_image_clears(
        agents: &mut IndexMap<AgentId, AgentView>,
        drawn_agent: Option<AgentId>,
    ) -> Option<crate::terminal::overlay::PostFlush> {
        if crate::terminal::image::detect_graphics_protocol()
            == crate::terminal::image::GraphicsProtocol::None
        {
            return None;
        }
        let mut clears = crate::terminal::overlay::PostFlush::default();
        let mut has_escapes = false;
        for (id, agent) in agents.iter_mut() {
            if Some(*id) == drawn_agent {
                continue;
            }
            if let Some(esc) = agent.take_inline_media_clear_escapes() {
                clears.append_plain(&esc);
                has_escapes = true;
            }
        }
        if drawn_agent.is_none() {
            clears.append(crate::terminal::overlay::clear_kitty().into());
            has_escapes = true;
        }
        has_escapes.then_some(clears)
    }
    /// Minimal mode: queue the most-recently committed folded block (collapsed
    /// reasoning / truncated tool output) to be re-printed fully expanded below
    /// the conversation on the next draw (design decision K10). Returns whether
    /// something was queued. No-op when nothing folded remains to expand.
    pub(crate) fn minimal_expand_last(&mut self) -> bool {
        let ActiveView::Agent(id) = &self.active_view else {
            return false;
        };
        let id = *id;
        let found = match self.agents.get_mut(&id) {
            Some(agent) => agent.scrollback.take_expandable_committed(),
            None => None,
        };
        if let Some(eid) = found {
            self.minimal_state.pending_expand.push(eid);
            true
        } else {
            false
        }
    }
    /// Minimal-mode key overrides, handled inline instead of by
    /// `agent.handle_input`. Returns `Some` when the key was consumed here.
    /// Callers gate on `is_minimal()` + non-release before dispatching.
    ///
    /// These keys carry full-TUI meanings that don't apply to the
    /// scrollback-native mode, so minimal remaps them:
    /// - `Ctrl+T` pins/unpins the todo panel (force-show). It otherwise
    ///   auto-hides once all todos are done (`minimal::live::todo_panel_visible`);
    ///   the pin keeps a finished list visible for review. The full-TUI
    ///   Ctrl+T toggles the todo overlay pane, which minimal never renders.
    /// - `Ctrl+E` re-prints the most-recently committed folded block fully
    ///   expanded below the conversation (K10) — committed terminal text can't be
    ///   mutated, so expansion is an honest re-print. The full-TUI Ctrl+E toggles
    ///   the scrollback-pane fold.
    /// - `Ctrl+O` opens the whole conversation fully expanded in `$PAGER` (the
    ///   "expand everything" view, the honest equivalent of a full
    ///   transcript mode for a static native scrollback). The full-TUI Ctrl+O is
    ///   interject, which keeps its Ctrl+Enter / Ctrl+I alt bindings —
    ///   **except on Apple Terminal**, where Ctrl+O *is* the interject chord
    ///   (kitty keyboard protocol unavailable → Ctrl+Enter doesn't arrive and
    ///   Ctrl+I aliases to Tab, see `default_actions`'s terminal-aware
    ///   `InterjectPrompt` binding). There the remap yields to interject only
    ///   while an interject would actually consume the press (turn running with
    ///   a non-empty composer, turn running with a queued follow-up on an empty
    ///   composer, or editing a queued row) — otherwise minimal on Apple
    ///   Terminal would have no working interject key at all. At idle / with an
    ///   empty composer and no queue the interject path is a silent no-op, so
    ///   the remap keeps the key and the transcript opens (it looked simply
    ///   dead before); see `minimal_api::minimal_ctrl_o_opens_transcript`, which
    ///   the info-row hint shares so it always advertises what a press would do.
    /// - The `ToggleQueue` chord (Ctrl+; by default; registry-resolved because
    ///   it is remappable and terminal-dependent) commits the read-only
    ///   `/queue` snapshot instead of toggling the full-TUI queue pane: the
    ///   pane never renders in minimal, so the toggle focused an *invisible*
    ///   pane that ate every keystroke (the same class of trap as the
    ///   never-rendered `/mcps` modal). Queue edits stay full-TUI-only; K13's
    ///   panes-become-committed-blocks rule applies.
    fn minimal_key_intercept(&mut self, key: &crossterm::event::KeyEvent) -> Option<InputOutcome> {
        if key!('t', CONTROL).matches(key) {
            self.minimal_state.show_todos = !self.minimal_state.show_todos;
        } else if self
            .registry
            .matches_id(crate::actions::ActionId::ToggleQueue, key)
        {
            return Some(InputOutcome::Action(crate::app::actions::Action::ShowQueue));
        } else if key!('e', CONTROL).matches(key) {
            self.minimal_expand_last();
        } else if key!('o', CONTROL).matches(key) {
            if crate::minimal_api::minimal_ctrl_o_opens_transcript(self) {
                return Some(InputOutcome::Action(
                    crate::app::actions::Action::OpenTranscriptPager,
                ));
            }
            if let ActiveView::Agent(id) = &self.active_view {
                let id = *id;
                if let Some(agent) = self.agents.get_mut(&id) {
                    return Some(agent.handle_prompt_key(key, &self.registry, false));
                }
            }
            return None;
        } else {
            return None;
        }
        Some(InputOutcome::Changed)
    }
    /// Render the current view to the terminal.
    ///
    /// Delegates to [`crate::render::draw::draw_frame`] which handles the
    /// low-level terminal interaction (bypassing ratatui's `try_draw`,
    /// synchronized output, cursor blink preservation). See that module's
    /// docs for the full rationale.
    pub fn draw(&mut self, terminal: &mut PagerTerminal) {
        self.draw_inner(terminal);
        crate::memory_release::run_deferred_release();
    }
    fn draw_inner(&mut self, terminal: &mut PagerTerminal) {
        self.resync_announcement_slash_gate_on_divergence();
        if self.screen_mode.is_minimal() {
            if let Some(hooks) = crate::minimal_hook::hooks() {
                (hooks.draw)(self, terminal);
            }
            return;
        }
        if self.welcome_on_auth_url
            && !matches!(
                (&self.active_view, &self.auth_state),
                (ActiveView::Welcome, AuthState::Authenticating { .. })
            )
        {
            self.welcome_on_auth_url = false;
            if crate::terminal::terminal_context()
                .hyperlink_capabilities()
                .osc22_cursor
            {
                xai_grok_shell::util::with_locked_stderr(|stderr| {
                    let _ = crossterm::execute!(stderr, crate::terminal::SetDefaultCursor);
                });
            }
        }
        let want_mouse_off = self.auth_show_raw_url
            && !self.screen_mode.is_minimal()
            && matches!(self.active_view, ActiveView::Welcome)
            && matches!(self.auth_state, AuthState::Authenticating { .. });
        if want_mouse_off && !self.auth_mouse_disabled {
            self.auth_mouse_disabled = true;
            xai_grok_shell::util::with_locked_stderr(|stderr| {
                let _ = crossterm::execute!(stderr, crossterm::event::DisableMouseCapture);
            });
            #[cfg(windows)]
            super::win_native_selection::enable_native_selection();
            super::MOUSE_CAPTURE_ENABLED.store(false, std::sync::atomic::Ordering::Release);
        } else if !want_mouse_off && self.auth_mouse_disabled {
            self.auth_mouse_disabled = false;
            xai_grok_shell::util::with_locked_stderr(|stderr| {
                let _ = crossterm::execute!(stderr, crossterm::event::EnableMouseCapture);
            });
            super::MOUSE_CAPTURE_ENABLED.store(true, std::sync::atomic::Ordering::Release);
            for agent in self.agents.values_mut() {
                agent.set_sticky_toast_recursive(None);
            }
        }
        self.maybe_trigger_small_screen_tip();
        self.maybe_trigger_ssh_wrap_tip();
        let compact = self.appearance.prompt.compact;
        let (header_pad_left, header_pad_right, header_pad_top) = {
            let layout_cfg = &self.appearance.scrollback.layout;
            (
                layout_cfg.eff_hpad_left(compact),
                layout_cfg.eff_hpad_right(compact),
                layout_cfg.eff_outer_vpad(compact),
            )
        };
        let zdr_blocked_for_draw = self.is_zdr_blocked();
        let has_access = self.has_access();
        let voice_available = self.voice_available();
        let voice_on_surface = self.voice_target_on_active_surface();
        let voice_listening = voice_on_surface && self.voice_listening();
        let voice_interim = voice_on_surface
            .then(|| self.voice_interim().map(str::to_owned))
            .flatten();
        let scroll_debug_panel = self.scroll_debug_panel();
        let dev_fps_rows = self.dev_fps_rows();
        let fps_overlay = self.fps_hud.overlay(dev_fps_rows);
        let foreign_resume_hint = self.foreign_resume_hint().cloned();
        let Self {
            active_view,
            agents,
            registry,
            scratch,
            cursor,
            pending_action,
            pending_notification_escapes,
            ..
        } = self;
        let notif_escapes = pending_notification_escapes.take();
        let pending_hint = pending_action
            .as_ref()
            .filter(|p| !p.expired())
            .and_then(|p| {
                p.label
                    .map(|label| crate::views::shortcuts_bar::PendingHint {
                        shortcut: p.shortcut,
                        label,
                    })
            });
        let fps_frame_started = fps_overlay.as_ref().map(|_| std::time::Instant::now());
        crate::render::draw::draw_frame(terminal, cursor, |f, link_spans| {
            let full_area = f.area();
            let tracing_height = 0u16;
            #[allow(unused_variables)]
            let (tracing_area, view_area) =
                if tracing_height > 0 && tracing_height < full_area.height {
                    let tracing = ratatui::layout::Rect {
                        x: full_area.x,
                        y: full_area.y,
                        width: full_area.width,
                        height: tracing_height,
                    };
                    let view = ratatui::layout::Rect {
                        x: full_area.x,
                        y: full_area.y + tracing_height,
                        width: full_area.width,
                        height: full_area.height.saturating_sub(tracing_height),
                    };
                    (Some(tracing), view)
                } else if tracing_height >= full_area.height {
                    let tracing = full_area;
                    (Some(tracing), ratatui::layout::Rect::default())
                } else {
                    (None, full_area)
                };
            if view_area.height > 0 {
                match *active_view {
                    ActiveView::Welcome => {
                        let mut flags_vec: Vec<crate::views::prompt_widget::PromptFlag<'_>> =
                            Vec::new();
                        if self.default_yolo {
                            flags_vec.push(crate::views::prompt_widget::PromptFlag {
                                text: "always-approve",
                                color: None,
                                bold: false,
                            });
                        }
                        if !self.welcome_prompt.text().is_empty() {
                            self.welcome_tip_typing_dismissed = true;
                        }
                        let tip = if self.welcome_tip_typing_dismissed {
                            None
                        } else {
                            self.tip.as_deref()
                        };
                        let model_name_base = self.models.current_model_name().unwrap_or_default();
                        let model_name = match self.models.reasoning_effort {
                            Some(eff) => format!("{model_name_base} ({eff})"),
                            None => model_name_base,
                        };
                        let hero_cta = crate::views::announcements::promo_cta(
                            &self.active_announcements,
                            &self.hidden_announcement_ids,
                        );
                        let hero_announcement = hero_cta
                            .map(|(owner, _, _)| owner)
                            .or_else(|| {
                                crate::views::announcements::first_session_announcement(
                                    &self.active_announcements,
                                    &self.hidden_announcement_ids,
                                )
                            })
                            .or(self.announcement.as_ref());
                        let welcome_params = crate::views::welcome::WelcomeRenderParams {
                            prompt_focus: if self.welcome_prompt_focused {
                                WelcomePromptFocus::Focused
                            } else {
                                WelcomePromptFocus::Unfocused
                            },
                            cwd: &self.cwd,
                            auth_state: &self.auth_state,
                            trust_state: &self.trust_state,
                            login_label: self.login_label.as_deref(),
                            auth_code_input: self.auth_code_input.text(),
                            auth_code_cursor_byte: self.auth_code_input.cursor_byte(),
                            clipboard_delivery: self.auth_clipboard_delivery,
                            show_raw_url: self.auth_show_raw_url,
                            announcement: hero_announcement,
                            tip,
                            model_name: &model_name,
                            flags: &flags_vec,
                            selected: self.welcome_menu_index,
                            team_name: self.team_name.as_deref(),
                            has_access,
                            has_claude_import: self.has_claude_import,
                            mouse_pos: self.last_mouse_pos,
                            is_zdr_blocked: zdr_blocked_for_draw,
                            session_picker: self.session_picker_entries.as_deref(),
                            session_picker_loading: self.session_picker_entries.is_none()
                                && (self.session_picker_loading
                                    || self.session_picker_lanes.foreign_loading),
                            compact,
                            pending_hint,
                            startup_warnings: &self.startup_warnings,
                            pending_update_version: self.pending_update_version.as_deref(),
                            foreign_resume_hint: foreign_resume_hint.as_ref(),
                            session_picker_content_results: self
                                .session_picker_content_results
                                .as_deref(),
                            session_picker_content_loading: self.session_picker_content_loading,
                            session_picker_entries_query: self
                                .session_picker_entries_query
                                .as_deref(),
                            welcome_tick: self.welcome_tick,
                            gate: self.gate.as_ref(),
                            subscription_tier: self.subscription_tier.as_deref(),
                            session_picker_grouped: self.session_picker_grouped,
                            session_picker_source_filter: self.session_picker_source_filter,
                            chat_mode: self.chat_mode,
                            credit_balance: self.credit_balance.as_ref(),
                            auto_topup: self.auto_topup.as_ref(),
                            usage_visible: self.usage_visible,
                            is_api_key_auth: self.is_api_key_auth,
                            changelog_bullets: &self.changelog_bullets,
                            changelog_has_full_notes: self.changelog_markdown.is_some(),
                            welcome_announcement_expanded: self.welcome_announcement.expanded,
                            upgrade_cta: hero_cta.map(|(_owner, label, _)| label),
                        };
                        let result = crate::views::welcome::render_welcome(
                            view_area,
                            f.buffer_mut(),
                            &welcome_params,
                            &mut self.welcome_prompt,
                            &mut self.session_picker_state,
                        );
                        self.welcome_menu_rects = result.menu_rects;
                        self.welcome_show_changelog_action = result.changelog_action_present;
                        self.welcome_prompt_rect = result.prompt_rect;
                        self.welcome_import_banner_rect = result.import_banner_rect;
                        self.welcome_auth_url_rect = result.auth_url_rect;
                        self.welcome_auth_fallback_rect = result.auth_fallback_rect;
                        self.welcome_refresh_rect = result.refresh_rect;
                        self.welcome_gate_url_rect = result.gate_url_rect;
                        self.welcome_upgrade_cta_rect = result.upgrade_cta_rect;
                        self.welcome_changelog_cta_rect = result.changelog_cta_rect;
                        self.welcome_announcement.truncated = result.announcement_truncated;
                        self.welcome_announcement.rect = result.announcement_rect;
                        self.session_picker_state.hit_areas = result.session_picker_hit_areas;
                        if let Some(modal) = self.import_claude_modal.as_mut() {
                            let theme = crate::theme::Theme::current();
                            crate::views::import_claude_modal::render_import_claude_modal(
                                f.buffer_mut(),
                                view_area,
                                modal,
                                &theme,
                                compact,
                            );
                        }
                        if let Some(dialog) = self.new_worktree_dialog.as_ref() {
                            crate::views::new_worktree_dialog::render_new_worktree_dialog(
                                view_area,
                                f.buffer_mut(),
                                dialog,
                            );
                        }
                        if let Some(crate::views::modal::ActiveModal::DocViewer {
                            ref title,
                            ref content,
                            ref mut scroll,
                            ref mut window,
                            ref mut cached_lines,
                            ..
                        }) = self.welcome_doc_viewer
                        {
                            let theme = crate::theme::Theme::current();
                            crate::views::modal::render_doc_viewer_overlay(
                                f.buffer_mut(),
                                view_area,
                                window,
                                title,
                                content,
                                scroll,
                                cached_lines,
                                compact,
                                &theme,
                            );
                        }
                        if !has_access && !self.access_gate_shown_logged {
                            self.access_gate_shown_logged = true;
                            xai_grok_telemetry::session_ctx::log_event(
                                xai_grok_telemetry::events::SuperGrokUpsellShown {
                                    source:
                                        xai_grok_telemetry::events::SuperGrokUpsell::WelcomeScreen,
                                    auth_method: self
                                        .login_method_id
                                        .as_ref()
                                        .map(|id| id.0.to_string()),
                                },
                            );
                        }
                        if let Some(fps) = &fps_overlay {
                            fps.render(full_area, f.buffer_mut());
                        }
                        if let Some(panel) = &scroll_debug_panel {
                            panel.render(full_area, f.buffer_mut());
                        }
                        let has_cloud_modal = false;
                        let cursor = if has_cloud_modal {
                            None
                        } else {
                            result.cursor_pos
                        };
                        let on_url = self.welcome_auth_url_rect.as_ref().is_some_and(|r| {
                            matches!(self.auth_state, AuthState::Authenticating { .. })
                                && self.last_mouse_pos.is_some_and(|(mx, my)| {
                                    mx >= r.x
                                        && mx < r.x + r.width
                                        && my >= r.y
                                        && my < r.y + r.height
                                })
                        });
                        let mut post_flush = result.post_flush_escapes;
                        if crate::terminal::terminal_context()
                            .hyperlink_capabilities()
                            .osc22_cursor
                            && on_url != self.welcome_on_auth_url
                        {
                            use crossterm::Command;
                            let mut buf = String::new();
                            if on_url {
                                let _ = crate::terminal::SetPointerCursor.write_ansi(&mut buf);
                            } else {
                                let _ = crate::terminal::SetDefaultCursor.write_ansi(&mut buf);
                            }
                            match post_flush.as_mut() {
                                Some(existing) => existing.append_plain(&buf),
                                None => {
                                    post_flush =
                                        Some(crate::terminal::overlay::PostFlush::plain(buf));
                                }
                            }
                        }
                        self.welcome_on_auth_url = on_url;
                        return (cursor, post_flush);
                    }
                    ActiveView::Agent(id) => {
                        let overlay_focused = false;
                        let overlay_active = self
                            .dashboard
                            .as_ref()
                            .is_some_and(|d| d.attached_agent == Some(id));
                        let position: Option<(usize, usize)> =
                            if overlay_active && let Some(d) = self.dashboard.as_ref() {
                                let order = crate::views::dashboard::overlay_cycle_order(d, agents);
                                order
                                    .iter()
                                    .position(|i| *i == id)
                                    .map(|idx| (idx + 1, order.len()))
                            } else {
                                None
                            };
                        let (agent_area, header) = if overlay_active {
                            let theme = crate::theme::Theme::current();
                            let title = agents
                                .get(&id)
                                .map(crate::views::session_title::entry_title)
                                .unwrap_or_else(|| "(session)".to_string());
                            let (hover_prev, hover_next, hover_close) = self
                                .dashboard
                                .as_ref()
                                .map(|d| {
                                    (
                                        d.overlay_prev_hit.hovered,
                                        d.overlay_next_hit.hovered,
                                        d.overlay_close_hit.hovered,
                                    )
                                })
                                .unwrap_or((false, false, false));
                            let header = crate::views::dashboard::render_dashboard_session_header(
                                f.buffer_mut(),
                                view_area,
                                &theme,
                                &title,
                                position,
                                hover_prev,
                                hover_next,
                                hover_close,
                                header_pad_left,
                                header_pad_right,
                                header_pad_top,
                            );
                            match header {
                                Some(chrome) => (chrome.content, Some(chrome)),
                                None => (view_area, None),
                            }
                        } else {
                            (view_area, None)
                        };
                        if let Some(d) = self.dashboard.as_mut() {
                            d.overlay_close_hit.set(header.and_then(|c| c.close_rect));
                            d.overlay_prev_hit.set(header.and_then(|c| c.prev_rect));
                            d.overlay_next_hit.set(header.and_then(|c| c.next_rect));
                        }
                        if let Some(d) = self.dashboard.as_mut()
                            && d.peek_viewport.is_some()
                        {
                            d.restore_peek_viewport(agents);
                        }
                        if let Some(agent) = agents.get_mut(&id) {
                            let announcement_banner_h =
                                crate::views::announcements::session_banner_height(
                                    &self.active_announcements,
                                    &self.hidden_announcement_ids,
                                );
                            let show_session_tip = self.tip.is_some() && agent.should_show_tip();
                            let has_mode_banner = agent.mode_switch_banner.is_some();
                            let banner_height = if has_mode_banner {
                                1
                            } else if announcement_banner_h > 0 {
                                announcement_banner_h
                            } else if show_session_tip {
                                1
                            } else {
                                0
                            };
                            let result = agent.draw(
                                agent_area,
                                f.buffer_mut(),
                                registry,
                                scratch,
                                pending_hint,
                                overlay_focused,
                                banner_height,
                                &self.active_announcements,
                                &self.hidden_announcement_ids,
                                if show_session_tip {
                                    self.tip.as_deref()
                                } else {
                                    None
                                },
                                &self.bundle_state,
                                overlay_active,
                                link_spans,
                                voice_available,
                                voice_listening,
                                voice_interim.as_deref(),
                            );
                            if let Some(modal) = self.import_claude_modal.as_mut() {
                                let theme = crate::theme::Theme::current();
                                crate::views::import_claude_modal::render_import_claude_modal(
                                    f.buffer_mut(),
                                    view_area,
                                    modal,
                                    &theme,
                                    compact,
                                );
                            }
                            if let Some(fps) = &fps_overlay {
                                fps.render(full_area, f.buffer_mut());
                            }
                            if let Some(panel) = &scroll_debug_panel {
                                panel.render(full_area, f.buffer_mut());
                            }
                            let (cursor_pos, post_flush) = result;
                            let has_cloud = false;
                            if has_cloud || self.import_claude_modal.is_some() {
                                link_spans.clear();
                            }
                            let cursor = if has_cloud { None } else { cursor_pos };
                            return (cursor, Self::merge_escapes(notif_escapes, post_flush));
                        }
                    }
                    ActiveView::AgentDashboard => {
                        if let Some(dashboard) = self.dashboard.as_mut() {
                            dashboard.voice_listening = voice_listening;
                            dashboard.voice_interim = voice_interim.clone();
                            if let Some(id) = dashboard.attached_agent
                                && !agents.contains_key(&id)
                            {
                                dashboard.close_popup();
                                if dashboard.error_toast.is_none() {
                                    dashboard.error_toast = Some(format!(
                                        "{} Session closed",
                                        crate::glyphs::check_mark()
                                    ));
                                }
                            }
                            let dashboard_roster: &[crate::app::roster::RosterEntry] =
                                if self.leader_mode {
                                    &self.leader_roster
                                } else {
                                    &self.dashboard_local_sessions
                                };
                            let dash_upgrade_cta = crate::views::announcements::promo_cta(
                                &self.active_announcements,
                                &self.hidden_announcement_ids,
                            )
                            .map(
                                |(owner, label, _)| crate::views::dashboard::HeaderUpgradeCta {
                                    label,
                                    pinned: !crate::views::announcements::is_dismissible(owner),
                                    caption: crate::views::announcements::usable_cta_caption(owner),
                                },
                            );
                            let dash_cursor = crate::views::dashboard::render_dashboard(
                                f.buffer_mut(),
                                view_area,
                                dashboard,
                                agents,
                                registry,
                                pending_hint,
                                dashboard_roster,
                                self.dashboard_sessions_loading,
                                dash_upgrade_cta,
                            );
                            let (popup_cursor, popup_post_flush, drawn_popup_agent) =
                                if let Some(agent_id) = dashboard.attached_agent {
                                    let theme = crate::theme::Theme::current();
                                    let popup_area = crate::views::dashboard::popup_rect(view_area);
                                    let title = agents
                                        .get(&agent_id)
                                        .map(crate::views::session_title::entry_title)
                                        .unwrap_or_else(|| "(session)".to_string());
                                    let bundle_state = &self.bundle_state;
                                    let (cursor, post_flush, drawn) =
                                        crate::views::dashboard::render_popup_overlay(
                                            f.buffer_mut(),
                                            popup_area,
                                            &theme,
                                            &title,
                                            dashboard,
                                            |inner, buf| {
                                                if let Some(agent) = agents.get_mut(&agent_id) {
                                                    agent.draw(
                                                        inner,
                                                        buf,
                                                        registry,
                                                        scratch,
                                                        None,
                                                        false,
                                                        0,
                                                        &[],
                                                        &std::collections::BTreeSet::new(),
                                                        None,
                                                        bundle_state,
                                                        false,
                                                        link_spans,
                                                        false,
                                                        false,
                                                        None,
                                                    )
                                                } else {
                                                    (None, None)
                                                }
                                            },
                                        );
                                    (cursor, post_flush, drawn.then_some(agent_id))
                                } else {
                                    (None, None, None)
                                };
                            let stale_clears =
                                Self::dashboard_stale_image_clears(agents, drawn_popup_agent);
                            let popup_post_flush =
                                Self::merge_post_flush(stale_clears, popup_post_flush);
                            if let Some(fps) = &fps_overlay {
                                fps.render(full_area, f.buffer_mut());
                            }
                            if let Some(panel) = &scroll_debug_panel {
                                panel.render(full_area, f.buffer_mut());
                            }
                            let cursor = if dashboard.attached_agent.is_some() {
                                popup_cursor
                            } else {
                                dash_cursor
                            };
                            return (cursor, Self::merge_escapes(notif_escapes, popup_post_flush));
                        }
                    }
                }
            }
            if let Some(fps) = &fps_overlay {
                fps.render(full_area, f.buffer_mut());
            }
            if let Some(panel) = &scroll_debug_panel {
                panel.render(full_area, f.buffer_mut());
            }
            (None, Self::merge_escapes(notif_escapes, None))
        });
        if let Some(started) = fps_frame_started {
            self.fps_hud.record(started.elapsed());
        }
        self.log_announcement_cta_impressions();
        self.maybe_evict_offscreen_caches();
    }
    /// Log [`xai_grok_telemetry::events::AnnouncementCtaShown`] for each
    /// surface whose CTA button is painted this frame (armed hit rect, not
    /// covered by a frame occluder — the click/OSC 8 truth the impression
    /// pairs with), once per (announcement, surface) per pager process
    /// (cleared on logout). The owner resolves through the same slot gate as
    /// the click dispatch, so a critical preempting the slot or a hidden
    /// promo emits nothing.
    pub(crate) fn log_announcement_cta_impressions(&mut self) {
        use xai_grok_telemetry::events::AnnouncementCtaSurface;
        let (banner, welcome, header, dashboard) = match self.active_view {
            ActiveView::Welcome => (false, self.welcome_upgrade_cta_rect.is_some(), false, false),
            ActiveView::Agent(agent_id) => match self.agents.get(&agent_id) {
                Some(a) => {
                    let cta_rect = a.hit_announcement_cta.rect;
                    let header_rect = a.hit_upgrade_cta.rect;
                    (
                        cta_rect.is_some_and(|r| !a.rect_occluded(r)),
                        false,
                        header_rect.is_some_and(|r| !a.rect_occluded(r)),
                        false,
                    )
                }
                None => return,
            },
            ActiveView::AgentDashboard => (
                false,
                false,
                false,
                self.dashboard
                    .as_ref()
                    .is_some_and(|d| d.upgrade_cta_hit.rect.is_some()),
            ),
        };
        if !(banner || welcome || header || dashboard) {
            return;
        }
        let Some((owner, _label, _url)) = crate::views::announcements::promo_cta(
            &self.active_announcements,
            &self.hidden_announcement_ids,
        ) else {
            return;
        };
        let key = xai_grok_announcements::announcement_hide_key(owner);
        let id = owner.id.clone();
        let surfaces = [
            (AnnouncementCtaSurface::Banner, banner),
            (AnnouncementCtaSurface::Welcome, welcome),
            (AnnouncementCtaSurface::Header, header),
            (AnnouncementCtaSurface::Dashboard, dashboard),
        ];
        for (surface, _) in surfaces.into_iter().filter(|(_, painted)| *painted) {
            if self
                .announcement_cta_impressions_logged
                .insert((key.clone(), surface))
            {
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::events::AnnouncementCtaShown {
                        id: id.clone(),
                        source: surface,
                    },
                );
            }
        }
    }
    /// Interval between off-screen render-cache eviction sweeps.
    const CACHE_EVICT_INTERVAL: Duration = Duration::from_secs(5);
    /// Throttled sweep of off-screen render caches for the active view's
    /// scrollback (parent agent, or the open fullscreen subagent child).
    /// A sweep is an O(entries) walk of pointer-sized cache slots — trivial
    /// next to a frame render — but there's no reason to run it per frame.
    fn maybe_evict_offscreen_caches(&mut self) {
        let ActiveView::Agent(id) = self.active_view else {
            return;
        };
        let now = Instant::now();
        if self
            .last_cache_evict_at
            .is_some_and(|t| now.duration_since(t) < Self::CACHE_EVICT_INTERVAL)
        {
            return;
        }
        self.last_cache_evict_at = Some(now);
        if let Some(agent) = self.agents.get_mut(&id) {
            let evicted = if let Some(child_sid) = agent.active_subagent.clone() {
                agent
                    .subagent_views
                    .get(&child_sid)
                    .map(|child| child.scrollback.evict_offscreen_render_caches())
                    .unwrap_or(0)
            } else {
                agent.scrollback.evict_offscreen_render_caches()
            };
            if evicted > 0 {
                tracing::debug!(evicted, "scrollback.evicted_offscreen_render_caches");
            }
        }
    }
}
impl AppView {
    /// True when any modal that should swallow scroll input is open.
    fn is_scroll_blocking_modal_open(&self) -> bool {
        let cloud_modal_open = false;
        matches!(
            self.active_view, ActiveView::Agent(id) if self.agents.get(& id)
            .is_some_and(| a | a.extensions_modal.is_some() || a.active_modal.is_some())
        ) || self.import_claude_modal.is_some()
            || self.new_worktree_dialog.is_some()
            || self.welcome_doc_viewer.is_some()
            || matches!(
                self.active_view, ActiveView::AgentDashboard if self.dashboard.as_ref()
                .is_some_and(| d | d.shortcuts_modal.is_some())
            )
            || cloud_modal_open
    }
    /// Store the resolved per-tip gates and propagate the prompt-relevant tips
    /// (undo + plan nudge) to every agent's prompt. Reused by startup and the
    /// settings live-apply path so a runtime toggle reaches existing agents.
    pub fn apply_contextual_hints(
        &mut self,
        resolved: xai_grok_shell::util::config::ResolvedContextualHints,
    ) {
        self.contextual_hints = resolved;
        for agent in self.agents.values_mut() {
            agent
                .prompt
                .set_contextual_hints(resolved.undo, resolved.plan_mode);
        }
    }
    /// One-shot small-screen `/compact-mode` tip trigger, run at the top of
    /// every `draw`. Waits (without consuming the one-shot) until the active
    /// AGENT view has a stable, draw-measured size — so a welcome screen, an
    /// undrawn agent, or a pending post-resize re-measure defer it. An
    /// out-of-band (or user-compact-on) first measure consumes the one-shot,
    /// so later resizes can never re-trigger. An in-band measure whose banner
    /// row is occluded (permission ask, modal, open dropdown, session banner)
    /// defers instead of consuming: the show gate would refuse it anyway, and
    /// spending the run's only evaluation on an invisible frame would kill
    /// the hint for the run.
    pub(crate) fn maybe_trigger_small_screen_tip(&mut self) {
        if self.small_screen_tip_evaluated {
            return;
        }
        let ActiveView::Agent(id) = self.active_view else {
            return;
        };
        let Some(agent) = self.agents.get(&id) else {
            return;
        };
        if agent.terminal_size_stale || agent.last_terminal_size == (0, 0) {
            return;
        }
        if !crate::tips::small_screen::small_screen_band_contains(agent.last_terminal_size.1)
            || self.current_ui.compact_mode
        {
            self.small_screen_tip_evaluated = true;
            return;
        }
        if !agent.ephemeral_tip_can_render() {
            return;
        }
        self.small_screen_tip_evaluated = true;
        super::dispatch::show_small_screen_tip(self);
    }
    /// One-shot SSH `grok wrap` tip trigger, run at the top of every `draw`
    /// right after [`Self::maybe_trigger_small_screen_tip`]. The welcome
    /// screen has no ephemeral-tip row, so the first stable agent-view draw
    /// is the earliest surface that can paint a session-load tip. Reads the
    /// live environment (cached statics) and delegates to the injectable
    /// inner so tests never depend on the host's SSH shape.
    pub(crate) fn maybe_trigger_ssh_wrap_tip(&mut self) {
        if self.ssh_wrap_tip_evaluated {
            return;
        }
        static ENV_RECOMMENDS_WRAP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let env_recommends_wrap = *ENV_RECOMMENDS_WRAP.get_or_init(|| {
            let ctx = crate::terminal::terminal_context();
            crate::diagnostics::ssh_wrap_hint(
                ctx.is_ssh,
                crate::clipboard::osc52_sink_active(),
                ctx.is_official_vscode_remote,
            )
            .is_some()
        });
        self.maybe_trigger_ssh_wrap_tip_inner(env_recommends_wrap);
    }
    /// Inner trigger with the environment verdict injected
    /// (`diagnostics::ssh_wrap_hint` on the live path). Same
    /// defer-vs-consume rules as the small-screen trigger above, with two
    /// deltas: the environment gates are process-constant, so a failing
    /// verdict consumes the one-shot; and a busy tip slot defers instead of
    /// replacing — both session-load tips can qualify on the same first
    /// draw, and replacing would burn the other tip's once-per-session show,
    /// while this one loses nothing by waiting for a later draw.
    pub(crate) fn maybe_trigger_ssh_wrap_tip_inner(&mut self, env_recommends_wrap: bool) {
        if self.ssh_wrap_tip_evaluated {
            return;
        }
        let ActiveView::Agent(id) = self.active_view else {
            return;
        };
        let Some(agent) = self.agents.get(&id) else {
            return;
        };
        if agent.terminal_size_stale || agent.last_terminal_size == (0, 0) {
            return;
        }
        if !env_recommends_wrap {
            self.ssh_wrap_tip_evaluated = true;
            return;
        }
        if !agent.ephemeral_tip_can_render() || agent.ephemeral_tip.is_active() {
            return;
        }
        self.ssh_wrap_tip_evaluated = true;
        super::dispatch::show_ssh_wrap_tip(self);
    }
    /// Whether the clipboard-image tip may poll right now — the single in-window
    /// gate. Outside it the poll touches the pasteboard ZERO times: contextual
    /// hints on, the probe supported (macOS), past the fire cooldown, the
    /// terminal focused, and the active agent eligible (the tip row can paint,
    /// no image chips attached, an image-capable model). Cooldown is part of the
    /// gate so a recently-fired tip suppresses even the cheap changeCount read.
    fn clipboard_tip_in_poll_window(&self, now: std::time::Instant) -> bool {
        self.contextual_hints.image_input
            && crate::clipboard::clipboard_image_probe_supported()
            && !self.clipboard_focus_tip.in_cooldown(now)
            && self.notification_service.focus_tracker.is_focused()
            && match self.active_view {
                ActiveView::Agent(id) => self
                    .agents
                    .get(&id)
                    .is_some_and(AgentView::clipboard_image_tip_eligible),
                _ => false,
            }
    }
    /// Opportunistic, throttled clipboard-image poll. Driven from event-loop
    /// iterations that already run for another reason (input, FocusGained,
    /// resize, an animation tick) — never from a timer and never by forcing
    /// `needs_animation`, so an idle/hibernating/unfocused app polls zero times.
    /// In-window it does at most one cheap `changeCount` read per `POLL_INTERVAL`
    /// and pays for the heavier type classification ONLY on a changeCount delta.
    /// Returns true when the tip was shown (needs redraw).
    pub(crate) fn poll_clipboard_focus_tip(&mut self) -> bool {
        let now = std::time::Instant::now();
        if !self.clipboard_tip_in_poll_window(now) {
            return false;
        }
        let outcome = self.clipboard_focus_tip.poll(
            now,
            crate::clipboard::clipboard_change_count,
            crate::tips::clipboard_focus::run_clipboard_check,
        );
        match outcome {
            Some(outcome) => self.apply_clipboard_probe(outcome, now),
            None => false,
        }
    }
    /// Decide + show for a probe outcome, committing the cooldown/dedup only
    /// when the show actually lands. Split out so the show/commit logic is
    /// unit-testable with a synthetic [`CheckOutcome`] (the native probe reads
    /// real hardware).
    fn apply_clipboard_probe(
        &mut self,
        outcome: crate::tips::clipboard_focus::CheckOutcome,
        now: std::time::Instant,
    ) -> bool {
        if !self.clipboard_focus_tip.should_fire(&outcome, now) {
            return false;
        }
        let ActiveView::Agent(id) = self.active_view else {
            return false;
        };
        let Some(agent) = self.agents.get_mut(&id) else {
            return false;
        };
        if agent.show_ephemeral_tip(
            crate::tips::clipboard_focus::clipboard_image_tip(),
            &mut self.tip_seen_counts,
        ) {
            self.clipboard_focus_tip.note_fired(&outcome, now);
            xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::ContextualTip {
                tip: xai_grok_telemetry::events::ContextualTipKind::ImageInput,
                action: xai_grok_telemetry::events::ContextualTipAction::Shown,
            });
            return true;
        }
        false
    }
    /// Advance animation timers and drain tracing channel.
    ///
    /// Called at a fixed rate (~30fps) from the event loop. Produces
    /// redraws when there are running entries with animated accents,
    /// when a pending action expires (to clear the "press again" hint),
    /// or when new tracing entries arrive via the channel.
    pub fn tick(&mut self) -> bool {
        let mut needs_redraw = false;
        needs_redraw |= self.minimal_state.transcript.is_some();
        needs_redraw |= self.poll_clipboard_focus_tip();
        if matches!(self.active_view, ActiveView::Welcome) {
            self.welcome_tick = self.welcome_tick.wrapping_add(1);
            if self.session_picker_content_loading {
                needs_redraw = true;
            } else {
                let frame = crate::views::welcome::shimmer_frame();
                if frame != self.welcome_shimmer_frame {
                    self.welcome_shimmer_frame = frame;
                    needs_redraw = true;
                }
            }
        }
        if matches!(self.active_view, ActiveView::AgentDashboard)
            && let Some(d) = self.dashboard.as_mut()
        {
            d.spinner_tick = d.spinner_tick.wrapping_add(1);
            needs_redraw = true;
            d.dispatch.poll_file_search();
            d.peek_reply.poll_file_search();
        }
        if let Some(pending) = &self.pending_action
            && pending.expired()
        {
            self.pending_action = None;
            needs_redraw = true;
        }
        if let Some(rx) = &mut self.tracing_rx {
            while rx.try_recv().is_ok() {}
        }
        let mut bootstrap_commands_update: Option<Vec<agent_client_protocol::AvailableCommand>> =
            None;
        for agent in self.agents.values_mut() {
            needs_redraw |= agent.edit_hl_tick();
            for child in agent.subagent_views.values_mut() {
                needs_redraw |= child.edit_hl_tick();
            }
        }
        if let ActiveView::Agent(id) = self.active_view
            && let Some(agent) = self.agents.get_mut(&id)
        {
            needs_redraw |= agent.scrollback.tick();
            needs_redraw |= agent.todo.list_state.tick();
            needs_redraw |= agent.todo.badge_tick();
            needs_redraw |= agent.tasks.tick();
            for child_view in agent.subagent_views.values_mut() {
                needs_redraw |= child_view.scrollback.tick();
                needs_redraw |= child_view.tick_toast();
                needs_redraw |= child_view.tick_ephemeral_tip();
                needs_redraw |= child_view.tick_mode_banner();
                needs_redraw |= child_view.tick_selection_highlight();
                needs_redraw |= child_view.tick_drag_autoscroll();
                needs_redraw |= child_view.poll_link_modifier();
                needs_redraw |= child_view.poll_scrollback_search();
                needs_redraw |= child_view.mermaid_tick();
                needs_redraw |= Self::tick_agent_image_load(child_view);
                needs_redraw |= Self::tick_agent_block_viewer(child_view);
            }
            let spinner_frame_tick =
                agent.scrollback.animation_tick() % crate::views::turn_status::SPINNER_DIVISOR == 0;
            needs_redraw |= !agent.session.state.is_idle() && spinner_frame_tick;
            needs_redraw |= agent
                .mcp_init_progress
                .as_ref()
                .is_some_and(McpInitProgress::is_visible)
                && spinner_frame_tick;
            needs_redraw |= matches!(
                agent.btw_state,
                Some(crate::views::btw_overlay::BtwOverlayState::Loading { .. })
            ) && spinner_frame_tick;
            needs_redraw |= agent.drain_blocked();
            if agent.acp_synced_generation != agent.session.available_commands_generation {
                agent.prompt.sync_acp_commands(
                    &agent.session.available_commands,
                    agent.session.available_tools.as_ref(),
                    &agent.session.models,
                );
                agent.acp_synced_generation = agent.session.available_commands_generation;
                bootstrap_commands_update = Some(agent.session.available_commands.clone());
                needs_redraw = true;
            }
            needs_redraw |= agent.prompt.poll_file_search();
            needs_redraw |= agent.prompt.history_search.poll();
            needs_redraw |= agent.poll_scrollback_search();
            needs_redraw |= agent.tick_toast();
            needs_redraw |= agent.tick_extensions_result_notice();
            needs_redraw |= agent.tick_ephemeral_tip();
            needs_redraw |= agent.tick_mode_banner();
            needs_redraw |= agent.tick_selection_highlight();
            needs_redraw |= agent.tick_drag_autoscroll();
            needs_redraw |= agent.poll_link_modifier();
            needs_redraw |= Self::tick_agent_image_load(agent);
            needs_redraw |= Self::tick_agent_block_viewer(agent);
            if let Some(ref mut viewer) = agent.video_viewer {
                needs_redraw |= viewer.tick();
            }
            if let Some(ref mut gboom) = agent.gboom {
                gboom.tick();
                needs_redraw = true;
            }
            if let Some(ref rx) = agent.video_load_rx
                && let Ok(result) = rx.try_recv()
            {
                agent.video_load_rx = None;
                match result {
                    Some(video) => {
                        agent.replace_inline_video(video);
                        agent.toast = None;
                    }
                    None => {
                        agent.show_toast("Video playback requires ffmpeg");
                    }
                }
                needs_redraw = true;
            }
            needs_redraw |= agent.mermaid_tick();
            if let Some(ref mut video) = agent.inline_video
                && !video.finished
                && !video.frames.is_empty()
            {
                let elapsed = video.last_frame_time.elapsed();
                let frame_dur = std::time::Duration::from_secs_f64(1.0 / video.fps);
                if elapsed >= frame_dur {
                    if video.current_frame + 1 >= video.frames.len() {
                        video.finished = true;
                    } else {
                        video.current_frame += 1;
                        video.last_frame_time = std::time::Instant::now();
                    }
                    needs_redraw = true;
                }
            }
        }
        if let Some(commands) = bootstrap_commands_update {
            self.welcome_prompt
                .sync_acp_commands(&commands, None, &self.models);
            if let Some(d) = self.dashboard.as_mut() {
                d.dispatch.sync_acp_commands(&commands, None, &self.models);
            }
            self.bootstrap_acp_commands = commands;
        }
        self.update_notifications();
        if let Some((_, remaining)) = self.deferred_notification.as_mut() {
            if *remaining == 0 {
                let event = self.deferred_notification.take().unwrap().0;
                self.notification_service.notify(event);
            } else {
                *remaining -= 1;
            }
        }
        needs_redraw |= self.tick_scroll();
        needs_redraw
    }
    /// Flush pending scroll lines (stream gap detection, redraw cadence).
    /// Without this, stale streams are never finalized after the user stops
    /// scrolling, and sub-line fractional remainders may not be flushed.
    ///
    /// Primarily driven by the event loop's scroll clock, armed from
    /// [`MouseScrollState::scroll_clock_deadline`] while a stream is active so
    /// residuals land on the 16ms redraw cadence (not the animation fps) and
    /// the 80ms stream-gap finalize fires on time. Returns true only when
    /// lines were dispatched — i.e. a draw would show real movement.
    pub(crate) fn tick_scroll(&mut self) -> bool {
        let mut needs_redraw = false;
        let had_scroll_stream = self.scroll_state.has_active_stream();
        let scroll_update = self.scroll_state.on_tick();
        if scroll_update.lines != 0
            && let Some((col, row)) = self.last_scroll_pos
            && !self.is_scroll_blocking_modal_open()
        {
            self.dispatch_scroll(scroll_update.lines, col, row);
            needs_redraw = true;
        }
        if had_scroll_stream && !self.scroll_state.has_active_stream() {
            self.last_scroll_pos = None;
        }
        needs_redraw
    }
    /// Whether the `/gboom` easter egg is open on the active agent view.
    /// While active it owns input, so the event loop preserves key-release
    /// events for it and bypasses paste coalescing.
    pub(crate) fn gboom_active(&self) -> bool {
        matches!(
            self.active_view, ActiveView::Agent(id) if self.agents.get(& id)
            .is_some_and(| a | a.gboom.is_some())
        )
    }
    /// Un-latch held movement on every open `/gboom` game.
    ///
    /// In release-aware (Kitty) mode a key stays latched until its release
    /// event arrives. On window focus loss the active game's release may be
    /// dropped, so clear all games' holds to stop runaway motion.
    pub(crate) fn gboom_release_all_games(&mut self) {
        for agent in self.agents.values_mut() {
            if let Some(gboom) = agent.gboom.as_mut() {
                gboom.release_all();
            }
        }
    }
    /// Un-latch held movement on every `/gboom` game that is *not* the active
    /// input target. Only the active game receives release events; a key
    /// still held when the user switches agent tabs (or to any other view)
    /// would otherwise leave that backgrounded game walking or turning with
    /// no key down when it is next reopened. Reconciled every event-loop
    /// iteration while a game is open, so it holds regardless of which view
    /// becomes active or whether the shared keyboard layer stays pushed.
    pub(crate) fn gboom_release_backgrounded_games(&mut self) {
        let active = match self.active_view {
            ActiveView::Agent(id) => Some(id),
            _ => None,
        };
        for (id, agent) in self.agents.iter_mut() {
            if Some(*id) != active
                && let Some(gboom) = agent.gboom.as_mut()
            {
                gboom.release_all();
            }
        }
    }
    /// Tick-interval ceiling requested by the current view state, if any.
    ///
    /// The `/gboom` easter egg targets ~30 fps even when the user configured
    /// a lower `animation.fps`; the simulation steps with wall-clock `dt`,
    /// so this only affects smoothness, never game speed.
    pub fn tick_interval_ceiling(&self) -> Option<std::time::Duration> {
        if self.gboom_active() {
            return Some(std::time::Duration::from_millis(33));
        }
        if self.minimal_state.transcript.is_some() {
            return Some(std::time::Duration::from_millis(16));
        }
        None
    }
    /// Deferred image viewer load (background thread). Shared by parent agent
    /// and fullscreen subagent children so gate/tick stay symmetric.
    fn tick_agent_image_load(agent: &mut AgentView) -> bool {
        if let Some(ref mut viewer) = agent.image_viewer
            && viewer.loading
        {
            if agent.image_load_rx.is_none()
                && let Some(path) = viewer.take_source_path()
            {
                let (tx, rx) = std::sync::mpsc::channel();
                agent.image_load_rx = Some(rx);
                std::thread::spawn(move || {
                    let _ = tx.send(crate::prompt_images::load_image_data(&path));
                });
            }
            if let Some(ref rx) = agent.image_load_rx {
                use crate::prompt_images::ImageLoadResult;
                match rx.try_recv() {
                    Ok(ImageLoadResult::Loaded(data)) => {
                        viewer.apply_loaded(data);
                        agent.image_load_rx = None;
                    }
                    Ok(ImageLoadResult::Failed)
                    | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        agent.image_viewer = None;
                        agent.image_load_rx = None;
                        agent.toast = Some(("Couldn't load image preview".into(), 6));
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
            return true;
        }
        false
    }
    /// Block viewer streaming / follow-mode ticks (parent or subagent child).
    fn tick_agent_block_viewer(agent: &mut AgentView) -> bool {
        let mut needs_redraw = false;
        if let Some(ref mut viewer) = agent.block_viewer {
            if viewer.kind == crate::views::block_viewer::ViewerKind::BgTask
                && let Some(ref task_id) = viewer.bg_task_id.clone()
                && let Some(task) = agent.session.bg_tasks.get(task_id)
            {
                let is_running = task.status == crate::app::agent::BgTaskStatus::Running;
                needs_redraw |= viewer.tick_bg_task(&task.stdout, is_running);
            } else if let Some(entry) = agent.scrollback.get_by_id(viewer.entry_id) {
                needs_redraw |= viewer.tick(entry);
            } else {
                agent.block_viewer = None;
                needs_redraw = true;
            }
        }
        needs_redraw
    }
    /// Check if animation ticks should be scheduled.
    pub fn needs_animation(&self) -> bool {
        self.tick_demand() != TickDemand::None
    }
    /// What tick cadence the current view state demands.
    ///
    /// [`TickDemand::Fast`] runs at the configured animation fps (default
    /// 30). [`TickDemand::Slow`] runs at [`SLOW_TICK_INTERVAL`] and is used
    /// when the only reasons to tick are low-frequency by construction —
    /// the ~12fps welcome logo shimmer and the macOS Cmd link-hover poll —
    /// so an app that *looks* idle doesn't spin a 30fps loop for them.
    pub fn tick_demand(&self) -> TickDemand {
        if self.pending_action.is_some() {
            return TickDemand::Fast;
        }
        if self.minimal_state.transcript.is_some() {
            return TickDemand::Fast;
        }
        if self
            .agents
            .values()
            .any(|a| a.pending_turn_end_reconcile.is_some())
        {
            return TickDemand::Fast;
        }
        if self.deferred_notification.is_some() {
            return TickDemand::Fast;
        }
        if self.voice_listening() {
            return TickDemand::Fast;
        }
        if self.session_picker_content_loading {
            return TickDemand::Fast;
        }
        if self.agents.values().any(|agent| {
            agent.edit_hl_needs_tick()
                || agent
                    .subagent_views
                    .values()
                    .any(|c| c.edit_hl_needs_tick())
        }) {
            return TickDemand::Fast;
        }
        match self.active_view {
            ActiveView::Agent(id) => {
                let Some(agent) = self.agents.get(&id) else {
                    return TickDemand::None;
                };
                let fast = agent.scrollback.needs_animation()
                    || agent.todo.list_state.needs_tick()
                    || agent.todo.badge_needs_tick()
                    || agent.tasks.needs_tick()
                    || agent.acp_synced_generation != agent.session.available_commands_generation
                    || !agent.session.state.is_idle()
                    || agent.session.loading_replay
                    || agent
                        .mcp_init_progress
                        .as_ref()
                        .is_some_and(McpInitProgress::is_visible)
                    || agent.plugin_cta.phase.is_spinner()
                    || matches!(
                        agent.btw_state,
                        Some(crate::views::btw_overlay::BtwOverlayState::Loading { .. })
                    )
                    || agent.drain_blocked()
                    || agent.prompt.file_search.context().is_some()
                    || agent.prompt.history_search.is_active()
                    || agent.scrollback_search.is_some()
                    || agent.line_viewer.is_some()
                    || agent.toast.is_some()
                    || agent
                        .extensions_modal
                        .as_ref()
                        .is_some_and(|m| m.result_notice.is_some())
                    || agent.ephemeral_tip_needs_tick()
                    || agent.mode_switch_banner.is_some()
                    || agent.has_drag_autoscroll()
                    || agent.selection_created_at.is_some()
                    || agent.block_viewer.is_some()
                    || agent.image_viewer.as_ref().is_some_and(|v| v.loading)
                    || agent.image_load_rx.is_some()
                    || agent.video_viewer.as_ref().is_some_and(|v| v.playing)
                    || agent.gboom.is_some()
                    || agent.inline_video.as_ref().is_some_and(|v| !v.finished)
                    || agent.video_load_rx.is_some()
                    || agent.mermaid_needs_tick()
                    || !agent.permission_queue.is_empty()
                    || agent.subagent_views.iter().any(|(sid, child)| {
                        child.toast.is_some()
                            || child.ephemeral_tip_needs_tick()
                            || child.mode_switch_banner.is_some()
                            || child.has_drag_autoscroll()
                            || child.selection_created_at.is_some()
                            || (agent.active_subagent.as_deref() == Some(sid.as_str())
                                && child.scrollback.needs_animation())
                            || child.scrollback_search.is_some()
                            || child.block_viewer.is_some()
                            || child.image_viewer.as_ref().is_some_and(|v| v.loading)
                            || child.image_load_rx.is_some()
                            || child.mermaid_needs_tick()
                    });
                if fast {
                    return TickDemand::Fast;
                }
                if cfg!(target_os = "macos")
                    && (agent.needs_link_modifier_poll()
                        || agent
                            .subagent_views
                            .values()
                            .any(|child| child.needs_link_modifier_poll()))
                {
                    return TickDemand::Slow;
                }
                TickDemand::None
            }
            ActiveView::AgentDashboard => {
                let agents_need = self.agents.values().any(|agent| {
                    !agent.session.state.is_idle()
                        || !agent.permission_queue.is_empty()
                        || agent.session.loading_replay
                        || agent.subagent_sessions.values().any(|info| !info.finished)
                });
                let dash_search = self.dashboard.as_ref().is_some_and(|d| {
                    d.dispatch.file_search.context().is_some()
                        || d.peek_reply.file_search.context().is_some()
                });
                if agents_need || dash_search {
                    TickDemand::Fast
                } else {
                    TickDemand::None
                }
            }
            ActiveView::Welcome => TickDemand::Slow,
        }
    }
    /// Update the terminal tab title and OSC 9;4 progress bar.
    ///
    /// Stores any resulting escape sequences in `pending_notification_escapes`
    /// so that the next `draw()` can pipe them through the frame's
    /// `post_flush_escapes` (inside the synchronized output block).
    ///
    /// Also clears the permission notification flag when no permissions
    /// remain queued, so the next batch fires a fresh bell/popup.
    pub fn update_notifications(&mut self) {
        let (session_name, model, activity, has_perms, turn_elapsed, is_busy) =
            if let ActiveView::Agent(id) = self.active_view
                && let Some(agent) = self.agents.get(&id)
            {
                let name = agent
                    .display_name
                    .as_deref()
                    .or(agent.generated_session_title.as_deref());
                let model = agent.session.models.current_model_name();
                let parked = agent.renders_parked();
                let activity = if parked {
                    None
                } else {
                    agent.resolve_turn_activity()
                };
                let has_perms = !agent.permission_queue.is_empty();
                let elapsed = if parked { None } else { agent.turn_elapsed() };
                let is_busy = agent.session.state.is_busy() && !parked;
                (name, model, activity, has_perms, elapsed, is_busy)
            } else {
                (None, None, None, false, None, false)
            };
        let any_agent_has_perms = self.agents.values().any(|a| !a.permission_queue.is_empty());
        if !any_agent_has_perms {
            self.notification_service.clear_permission_notification();
        }
        let cwd_str = self.cwd.to_string_lossy();
        let title_state = crate::notifications::TitleState {
            session_name,
            model: model.as_deref(),
            activity: activity.as_ref(),
            has_pending_permissions: has_perms,
            cwd: Some(&cwd_str),
            turn_elapsed,
            is_busy,
            focused: self.notification_service.focus_tracker.is_focused(),
        };
        if let Some(esc) = self.notification_service.on_tick(&title_state) {
            self.pending_notification_escapes
                .get_or_insert_with(String::new)
                .push_str(&esc);
        }
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::acp::tracker::AcpUpdateTracker;
    use crate::app::agent::{AgentSession, AgentState};
    use crate::app::agent_view::{AgentView, PromptMode};
    use crate::app::bundle::BundleState;
    use crate::scrollback::state::ScrollbackState;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    #[test]
    fn parse_esc_ttl_bounds() {
        let default = PendingAction::ESC_DOUBLE_PRESS_TTL;
        assert_eq!(parse_esc_ttl(None), default);
        assert_eq!(parse_esc_ttl(Some("garbage".into())), default);
        assert_eq!(parse_esc_ttl(Some("".into())), default);
        assert_eq!(parse_esc_ttl(Some("0".into())), default);
        assert_eq!(parse_esc_ttl(Some("-5".into())), default);
        assert_eq!(
            parse_esc_ttl(Some(" 1200 ".into())),
            Duration::from_millis(1200)
        );
        assert_eq!(
            parse_esc_ttl(Some(ESC_DOUBLE_PRESS_TEST_MS.to_string())),
            Duration::from_millis(ESC_DOUBLE_PRESS_TEST_MS)
        );
        assert_eq!(
            parse_esc_ttl(Some(u64::MAX.to_string())),
            Duration::from_millis(ESC_DOUBLE_PRESS_TEST_MS)
        );
    }
    /// `AppView::draw` is the ONLY drain point for the process-wide deferred
    /// release flag; if the wrapper loses its `run_deferred_release()` call,
    /// every draw/tick-path cliff (video scroll-off, takeover drain,
    /// frame-set replacement) silently stops purging. Drives the real
    /// `draw()` against a channel-backed terminal (no tty; same recipe as
    /// pager-render's `draw_frame` tests). Serialized: process-wide flag.
    #[test]
    #[serial_test::serial(MEMORY_RELEASE_DEFER)]
    fn app_draw_drains_deferred_release_after_flush() {
        use crate::memory_release::test_support;
        use ratatui::{TerminalOptions, Viewport};
        test_support::install_counting_hook();
        crate::memory_release::run_deferred_release();
        let (frame_tx, _frame_rx) =
            std::sync::mpsc::channel::<crate::render::draw::WriterPayload>();
        let writer =
            crate::render::draw::TermWriter::new(frame_tx, crate::render::draw::WriterSync::new())
                .expect("single test writer");
        let backend = ratatui::backend::CrosstermBackend::new(writer);
        let mut terminal = xai_ratatui_inline::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("channel-backed terminal requires no tty");
        let mut app = test_app();
        crate::memory_release::request_release_after_draw_with("unit-test-defer");
        let before = test_support::calls();
        app.draw(&mut terminal);
        assert_eq!(
            test_support::calls(),
            before + 1,
            "AppView::draw must drain the deferred release post-flush"
        );
        let before = test_support::calls();
        app.draw(&mut terminal);
        assert_eq!(
            test_support::calls(),
            before,
            "a draw without a pending request must not purge"
        );
    }
    pub(crate) fn test_app() -> AppView {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        AppView {
            active_view: ActiveView::Welcome,
            auth_return_view: None,
            agents: indexmap::IndexMap::new(),
            next_agent_id: 0,
            models: ModelState::default(),
            registry: ActionRegistry::defaults(),
            settings_registry: std::sync::Arc::new(crate::settings::SettingsRegistry::defaults()),
            current_ui: xai_grok_shell::agent::config::UiConfig::default(),
            cwd: std::path::PathBuf::from("/tmp"),
            project_picker_shown: true,
            project_picker_disabled: false,
            cwd_has_git_ancestor: false,
            acp_tx: tx,
            scratch: crate::scrollback::render::ScratchBuffer::new(),
            cursor: CursorState::new(),
            pending_action: None,
            exit_session_pending: None,
            scroll_state: MouseScrollState::default(),
            scroll_config: ScrollConfig::default(),
            appearance: AppearanceConfig::default(),
            notification_service: NotificationService::new(Default::default()),
            pending_notification_escapes: None,
            deferred_notification: None,
            tracing_rx: None,
            active_announcements: vec![],
            hidden_announcement_ids: Default::default(),
            announcements_last_gen: 0,
            announcement: None,
            changelog_markdown: None,
            changelog_bullets: Vec::new(),
            tips: Vec::new(),
            tip: None,
            cli_model_override: None,
            cli_effort_token: None,
            default_yolo: false,
            permission_mode_from_soft_default: true,
            auto_mode_gate: true,
            yolo_policy_block: None,
            yolo_launch_block_notice: None,
            screen_mode_switch_hint: None,
            require_plan_approval: false,
            plan_mode: false,
            subagents: false,
            ask_user: false,
            chat_mode: false,
            mouse_captured: true,
            new_worktree_dialog: None,
            contextual_hints: Default::default(),
            remote_contextual_hints: None,
            tip_seen_counts: Default::default(),
            last_known_terminal_rows: 0,
            small_screen_tip_evaluated: false,
            ssh_wrap_tip_evaluated: false,
            clipboard_focus_tip: Default::default(),
            new_session_worktree_mode: WorktreeMode::Never,
            fork_worktree_mode: WorktreeMode::Ask,
            restore_code: None,
            agent_override: None,
            bootstrap_acp_commands: Vec::new(),
            auth_methods: Vec::new(),
            auth_state: AuthState::Done,
            trust_state: TrustState::Done,
            login_label: None,
            login_method_id: None,
            auth_start_mode: AuthMode::Pending,
            auth_code_input: LineEditor::default(),
            next_auth_request_seq: 1,
            auth_url_poll_handle: None,
            deferred_startup: Default::default(),
            auth_use_oauth: false,
            auth_clipboard_delivery: None,
            auth_clipboard_feedback_generation: 0,
            team_id: None,
            team_name: None,
            is_zdr: false,
            team_role: None,
            coding_data_retention_opt_out: true,
            show_tips: None,
            auto_update: None,
            ask_user_question_timeout_enabled: None,
            zdr_access_enabled: false,
            usage_billing_redirect_url: None,
            access_gate_shown_logged: false,
            announcement_cta_impressions_logged: Default::default(),
            gate: None,
            subscription_tier: None,
            paywall_check_started: None,
            last_subscription_check_at: None,
            subscription_watch_interval_secs: None,
            pending_gate_verification: None,
            gate_verify_gen: 0,
            bundle_state: BundleState::default(),
            scroll_debug_hud: crate::views::scroll_debug_hud::ScrollDebugHud::new(),
            fps_hud: crate::views::fps_hud::FpsHud::new(),
            welcome_prompt: crate::views::prompt_widget::PromptWidget::new(),
            slash_mru: std::rc::Rc::new(std::cell::RefCell::new(
                crate::slash::mru::SlashMru::new_in_memory(),
            )),
            welcome_prompt_focused: false,
            welcome_tip_typing_dismissed: false,
            welcome_menu_index: None,
            welcome_menu_rects: Vec::new(),
            welcome_show_changelog_action: false,
            welcome_import_banner_rect: None,
            last_mouse_pos: None,
            last_scroll_pos: None,
            last_cache_evict_at: None,
            welcome_prompt_rect: None,
            welcome_auth_url_rect: None,
            welcome_on_auth_url: false,
            welcome_on_changelog_cta: false,
            welcome_announcement: WelcomeAnnouncementState::default(),
            welcome_auth_fallback_rect: None,
            welcome_refresh_rect: None,
            welcome_gate_url_rect: None,
            welcome_upgrade_cta_rect: None,
            welcome_on_upgrade_cta: false,
            welcome_changelog_cta_rect: None,
            auth_show_raw_url: false,
            auth_mouse_disabled: false,
            session_picker_entries: None,
            session_picker_loading: false,
            session_picker_state: crate::views::picker::PickerState::with_mode(
                crate::views::picker::PickerMode::FullScreen,
            ),
            session_picker_source_filter: crate::views::session_picker::SourceFilter::default(),
            session_picker_content_results: None,
            session_picker_content_loading: false,
            session_picker_deep_search_seq: 0,
            session_picker_list_seq: 0,
            foreign_session_compat: Default::default(),
            foreign_session_scan_seq: 0,
            foreign_scan_coordinator: Default::default(),
            session_picker_lanes: Default::default(),
            session_picker_detail_generation: 0,
            session_picker_entries_query: None,
            welcome_tick: 0,
            welcome_shimmer_frame: 0,
            startup_warnings: Vec::new(),
            is_api_key_auth: false,
            pending_update_version: None,
            foreign_resume_launch_generation: 0,
            foreign_resume_launch: None,
            quit_for_update: false,
            relaunch: None,
            has_claude_import: false,
            import_claude_modal: None,
            welcome_doc_viewer: None,
            screen_mode: ScreenMode::Inline,
            pending_effects: Vec::new(),
            pending_editor_path: None,
            pending_agents_modal_refresh: None,
            pending_pager_path: None,
            pending_pager_ansi: false,
            minimal_state: crate::minimal_api::MinimalState::default(),
            reconnect_pending: false,
            show_resolved_model: true,
            sharing_enabled: false,
            plugin_cta_enabled: false,
            usage_visible: true,
            tier_restricted_commands: Vec::new(),
            leader_mode: true,
            credit_balance: None,
            auto_topup: None,
            billing_poll_wanted: false,
            leader_roster: Vec::new(),
            dashboard_local_sessions: Vec::new(),
            dashboard_sessions_loading: false,
            shared_prompt_queues: std::collections::HashMap::new(),
            optimistic_prompt_echoes: std::collections::HashMap::new(),
            pending_running_adoptions: std::collections::HashMap::new(),
            session_picker_grouped: false,
            cancel_rewind_enabled: true,
            session_recap_available: false,
            dashboard: None,
            dashboard_return: None,
            dashboard_persisted: None,
            keyboard_normalizer: KeyboardNormalizer::from_terminal_context(),
            voice_mode_enabled: false,
            voice_ui_active: false,
            voice_config: xai_grok_voice::VoiceConfig::default(),
            voice_auth: None,
            voice_cmd_tx: None,
            voice_state: VoiceState::Idle,
        }
    }
    pub(crate) fn test_app_with_agent() -> AppView {
        let mut app = test_app();
        let id = super::super::agent::AgentId(0);
        let mut agent = AgentView::new(
            AgentSession {
                id,
                acp_tx: app.acp_tx.clone(),
                session_id: Some("test-session".into()),
                models: ModelState::default(),
                state: AgentState::Idle,
                tracker: AcpUpdateTracker::new(),
                cwd: std::path::PathBuf::from("/tmp"),
                is_worktree: false,
                forked_from: None,
                pending_prompts: std::collections::VecDeque::new(),
                next_queue_id: 0,
                yolo_mode: false,
                auto_mode: false,
                prompt_history: Vec::new(),
                prompt_history_loading: false,
                loading_replay: false,
                restore_degree: None,
                rate_limited: false,
                model_incompatible: false,
                credit_limit_blocked: false,
                free_usage_blocked: false,
                available_commands: Vec::new(),
                available_commands_generation: 0,
                available_tools: None,
                model_switch_pending: false,
                user_model_preference: None,
                deferred_model_switch: None,
                bg_tasks: std::collections::BTreeMap::new(),
                bg_tool_call_to_task: std::collections::HashMap::new(),
                scheduled_tasks: std::collections::HashMap::new(),
                in_flight_prompt: None,
                current_prompt_id: None,
                created_via_new: false,
            },
            ScrollbackState::new(),
        );
        agent.active_pane = crate::views::agent::ActivePane::Scrollback;
        app.agents.insert(id, agent);
        super::super::dispatch::switch_to_agent(
            &mut app,
            id,
            super::super::dispatch::SwitchCause::Load,
        );
        app
    }
    #[test]
    fn dashboard_x11_primary_provenance_bypasses_unrelated_clipboard_image() {
        const PRIMARY: &str = "PRIMARY selection text";
        let clipboard_hook = || crate::clipboard::ClipboardProbeHook {
            text: Some("CLIPBOARD text".to_owned()),
            primary_text: Some(PRIMARY.to_owned()),
            x11_primary_available: true,
            ..crate::clipboard::ClipboardProbeHook::with_raster(Some(crate::clipboard::ImageData {
                data: vec![1, 2, 3],
                mime_type: "image/png".to_owned(),
            }))
        };
        let mut bracketed = test_app();
        bracketed.active_view = ActiveView::AgentDashboard;
        bracketed.dashboard = Some(crate::views::dashboard::DashboardState::new());
        crate::clipboard::set_clipboard_probe_hook(clipboard_hook());
        let _ = bracketed.handle_input(&Event::Paste(PRIMARY.to_owned()));
        crate::clipboard::clear_clipboard_probe_hook();
        assert!(
            bracketed.pending_effects.iter().any(|effect| matches!(
                effect,
                crate::app::actions::Effect::ProbeClipboardAttachment { .. }
            )),
            "the distinct CLIPBOARD image must make ordinary bracketed paste probe"
        );
        let mut primary = test_app();
        primary.active_view = ActiveView::AgentDashboard;
        primary.dashboard = Some(crate::views::dashboard::DashboardState::new());
        crate::clipboard::set_clipboard_probe_hook(clipboard_hook());
        let outcome = primary.handle_input_at_with_paste_provenance(
            &Event::Paste(PRIMARY.to_owned()),
            Instant::now(),
            PasteProvenance::X11Primary,
        );
        let probe_calls = crate::clipboard::clipboard_probe_call_count();
        crate::clipboard::clear_clipboard_probe_hook();
        assert!(matches!(outcome, InputOutcome::Changed));
        let dashboard = primary.dashboard.as_ref().expect("dashboard state");
        assert_eq!(dashboard.dispatch.text(), PRIMARY);
        assert!(!dashboard.dispatch.text().contains("CLIPBOARD"));
        assert!(dashboard.dispatch.images.is_empty());
        assert_eq!(dashboard.paste_probe_in_flight, 0);
        assert!(primary.pending_effects.iter().all(|effect| !matches!(
            effect,
            crate::app::actions::Effect::ProbeClipboardAttachment { .. }
        )));
        assert_eq!(probe_calls, 0);
    }
    /// With the image-input tip OFF, the poll short-circuits at the window gate
    /// before touching the pasteboard — the per-tip gate fails closed.
    #[test]
    fn clipboard_poll_no_op_when_flag_off() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);
        app.notification_service.focus_tracker.on_focus_gained();
        app.contextual_hints.image_input = false;
        assert!(!app.poll_clipboard_focus_tip(), "tip-off poll is a no-op");
        assert!(!app.agents[&id].ephemeral_tip.is_active());
    }
    /// The in-window gate decides whether an already-running iteration may touch
    /// the pasteboard at all. It opens only when contextual hints are on, the
    /// probe is supported (macOS), the fire cooldown is clear, the terminal is
    /// focused, and the active agent is eligible; flipping any one closes it so
    /// the poll reads the clipboard zero times. (Probe support is macOS-only, so
    /// the in-window result tracks the platform.)
    #[test]
    fn clipboard_poll_window_gate() {
        let mut app = test_app_with_agent();
        app.contextual_hints.image_input = true;
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);
        app.notification_service.focus_tracker.on_focus_gained();
        let now = std::time::Instant::now();
        let supported = crate::clipboard::clipboard_image_probe_supported();
        assert_eq!(
            app.clipboard_tip_in_poll_window(now),
            supported,
            "in window"
        );
        app.contextual_hints.image_input = false;
        assert!(!app.clipboard_tip_in_poll_window(now), "tip off");
        app.contextual_hints.image_input = true;
        app.notification_service.focus_tracker.on_focus_lost();
        assert!(!app.clipboard_tip_in_poll_window(now), "unfocused");
        app.notification_service.focus_tracker.on_focus_gained();
        let img = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
            data: vec![1, 2, 3],
            mime_type: "image/png".into(),
        });
        app.agents.get_mut(&id).unwrap().prompt.images.push(img);
        assert!(!app.clipboard_tip_in_poll_window(now), "image attached");
        app.agents.get_mut(&id).unwrap().prompt.images.clear();
        let fired = crate::tips::clipboard_focus::CheckOutcome {
            change_count: Some(1),
            has_image: true,
        };
        app.clipboard_focus_tip.note_fired(&fired, now);
        assert!(!app.clipboard_tip_in_poll_window(now), "in cooldown");
    }
    /// A positive, deduped, un-cooled-down outcome on a drawable agent shows the
    /// tip and commits the cooldown + changeCount dedup (same content won't
    /// re-fire). Drives `apply_clipboard_probe` with a synthetic outcome so it
    /// is independent of the real pasteboard.
    #[test]
    fn clipboard_probe_shows_and_commits_on_positive_outcome() {
        use crate::tips::clipboard_focus::CheckOutcome;
        let mut app = test_app_with_agent();
        app.contextual_hints.image_input = true;
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);
        let now = std::time::Instant::now();
        let outcome = CheckOutcome {
            change_count: Some(7),
            has_image: true,
        };
        assert!(app.apply_clipboard_probe(outcome, now));
        assert!(app.agents[&id].ephemeral_tip.is_active());
        assert!(
            !app.clipboard_focus_tip.should_fire(&outcome, now),
            "fired content must commit the changeCount dedup"
        );
    }
    /// A refused show (here: the renderability gate on a short terminal) must
    /// burn nothing — the same outcome stays fireable.
    #[test]
    fn clipboard_probe_refused_show_burns_nothing() {
        use crate::tips::clipboard_focus::CheckOutcome;
        let mut app = test_app_with_agent();
        app.contextual_hints.image_input = true;
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 10);
        let now = std::time::Instant::now();
        let outcome = CheckOutcome {
            change_count: Some(7),
            has_image: true,
        };
        assert!(!app.apply_clipboard_probe(outcome, now));
        assert!(!app.agents[&id].ephemeral_tip.is_active());
        assert!(
            app.clipboard_focus_tip.should_fire(&outcome, now),
            "refused show must leave cooldown and dedup uncommitted"
        );
    }
    /// Build an idle subagent child `AgentView` for child gate↔tick symmetry tests.
    fn idle_child_view(app: &AppView, id_n: usize, sid: &str) -> Box<AgentView> {
        let session = AgentSession {
            id: super::super::agent::AgentId(id_n),
            acp_tx: app.acp_tx.clone(),
            session_id: Some(sid.to_string().into()),
            models: ModelState::default(),
            state: AgentState::Idle,
            tracker: AcpUpdateTracker::new(),
            cwd: std::path::PathBuf::from("/tmp"),
            is_worktree: false,
            forked_from: None,
            pending_prompts: std::collections::VecDeque::new(),
            next_queue_id: 0,
            yolo_mode: false,
            auto_mode: false,
            prompt_history: Vec::new(),
            prompt_history_loading: false,
            loading_replay: false,
            restore_degree: None,
            rate_limited: false,
            model_incompatible: false,
            credit_limit_blocked: false,
            free_usage_blocked: false,
            available_commands: Vec::new(),
            available_commands_generation: 0,
            available_tools: None,
            model_switch_pending: false,
            user_model_preference: None,
            deferred_model_switch: None,
            bg_tasks: std::collections::BTreeMap::new(),
            bg_tool_call_to_task: std::collections::HashMap::new(),
            scheduled_tasks: std::collections::HashMap::new(),
            in_flight_prompt: None,
            current_prompt_id: None,
            created_via_new: false,
        };
        Box::new(AgentView::new(session, ScrollbackState::new()))
    }
    fn key_event(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }
    /// Build a registry pinned to the non-VSCode bindings so tests are
    /// deterministic regardless of the host terminal.
    fn pin_non_vscode_registry(app: &mut AppView) {
        let mut actions = crate::actions::default_actions(false);
        for def in actions.iter_mut() {
            if def.id == ActionId::Quit {
                def.default_key = key!('q', CONTROL);
                def.alt_keys = vec![key!('d', CONTROL)];
            }
            if def.id == ActionId::HalfPageDown {
                def.default_key = key!('d', CONTROL);
            }
        }
        app.registry = ActionRegistry::new(actions);
    }
    fn ctrl_d() -> Event {
        key_event(KeyCode::Char('d'), KeyModifiers::CONTROL)
    }
    fn ctrl_q() -> Event {
        key_event(KeyCode::Char('q'), KeyModifiers::CONTROL)
    }
    fn ctrl_c() -> Event {
        key_event(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }
    fn left_mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }
    #[test]
    fn needs_animation_ignores_tracing_rx_outside_dev_builds() {
        let mut app = test_app_with_agent();
        let (_tx, rx) = tokio::sync::mpsc::channel::<String>(4);
        app.tracing_rx = Some(rx);
        assert!(
            !app.needs_animation(),
            "release builds must not request animation ticks just because \
             tracing_rx exists (always true after startup)"
        );
    }
    #[test]
    fn needs_animation_gates_prompt_history_tick_delivery() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        assert!(
            !app.needs_animation(),
            "an idle agent with no history overlay must not request animation ticks"
        );
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.prompt_history = vec!["first prompt".into(), "second prompt".into()];
            let history = agent.combined_prompt_history();
            agent.prompt.history_search.activate(&history, "");
        }
        assert!(
            app.needs_animation(),
            "an open prompt history overlay must request animation ticks"
        );
        let mut delivered = false;
        for _ in 0..1000 {
            if app.tick() && app.agents[&id].prompt.history_search.result_count() == 2 {
                delivered = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            delivered,
            "tick() must poll the history daemon and deliver results"
        );
        app.agents
            .get_mut(&id)
            .unwrap()
            .prompt
            .history_search
            .deactivate();
        assert!(
            !app.needs_animation(),
            "closing the history overlay stops the animation ticks"
        );
    }
    #[test]
    fn needs_animation_gates_scrollback_search_tick_delivery() {
        use crate::scrollback::ScrollbackSearchState;
        use crate::scrollback::block::RenderBlock;
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent
                .scrollback
                .push_block(RenderBlock::user_prompt("foo bar"));
            agent
                .scrollback
                .push_block(RenderBlock::user_prompt("baz foo"));
            agent.scrollback.prepare_layout(80, 24);
        }
        assert!(
            !app.needs_animation(),
            "an idle agent with no search open must not request animation ticks"
        );
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.scrollback_search = Some(ScrollbackSearchState::open());
            let search = agent.scrollback_search.as_mut().unwrap();
            search.update_query("foo", &agent.scrollback);
            assert_eq!(
                search.current_index(),
                None,
                "matches are not computed synchronously on the input thread"
            );
        }
        assert!(
            app.needs_animation(),
            "an open scrollback search must request animation ticks"
        );
        let mut delivered = false;
        for _ in 0..1000 {
            app.tick();
            if app.agents[&id]
                .scrollback_search
                .as_ref()
                .unwrap()
                .current_index()
                == Some(0)
            {
                delivered = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(delivered, "tick() must poll the daemon and deliver results");
        assert_eq!(
            app.agents[&id]
                .scrollback_search
                .as_ref()
                .unwrap()
                .match_count(),
            2
        );
        app.agents.get_mut(&id).unwrap().scrollback_search = None;
        assert!(
            !app.needs_animation(),
            "closing the search stops the animation ticks"
        );
    }
    /// The welcome screen shimmer only advances ~12fps, so a resting welcome
    /// screen must demand Slow ticks — not a 30fps loop; the deep-search
    /// spinner upgrades it to Fast while loading.
    #[test]
    fn tick_demand_welcome_is_slow_unless_loading() {
        let mut app = test_app();
        assert_eq!(app.active_view, ActiveView::Welcome);
        assert_eq!(app.tick_demand(), TickDemand::Slow);
        assert!(app.needs_animation(), "slow still counts as animating");
        app.session_picker_content_loading = true;
        assert_eq!(app.tick_demand(), TickDemand::Fast);
    }
    /// An idle agent view demands no ticks at all; the macOS Cmd link-hover
    /// poll (when it is the only pending work) demands Slow, never Fast.
    #[test]
    #[cfg(target_os = "macos")]
    fn tick_demand_link_poll_is_slow_only() {
        use crate::render::osc8::{LinkOverlay, OverlayLink};
        use std::sync::Arc;
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        assert_eq!(app.tick_demand(), TickDemand::None, "idle agent parks");
        {
            let agent = app.agents.get_mut(&id).unwrap();
            let mut overlay = LinkOverlay::new();
            overlay.push(OverlayLink {
                screen_row: 2,
                col_start: 0,
                col_end: 10,
                target: crate::render::osc8::LinkTarget::Url(Arc::from("https://example.com")),
                presentation: crate::render::osc8::LinkPresentation::Opaque,
                id: Some(1),
            });
            agent.visible_link_map.rebuild(1, &overlay, vec![]);
            agent.hovered_entry = Some(0);
            agent.last_mouse_moved_at = Some(std::time::Instant::now());
        }
        if !crate::app::agent_view::has_native_link_hover() {
            assert_eq!(
                app.tick_demand(),
                TickDemand::Slow,
                "link poll alone must not spin the fast loop"
            );
        }
    }
    #[test]
    fn needs_animation_gates_mode_switch_banner_countdown() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        assert!(!app.needs_animation(), "idle agent must not request ticks");
        app.agents
            .get_mut(&id)
            .unwrap()
            .show_mode_switch_banner("Plan");
        assert!(
            app.needs_animation(),
            "mode_switch_banner must request ticks (tick_mode_banner countdown)"
        );
        let mut cleared = false;
        for _ in 0..512 {
            app.tick();
            if app.agents[&id].mode_switch_banner.is_none() {
                cleared = true;
                break;
            }
        }
        assert!(
            cleared,
            "tick() must decrement mode_switch_banner until it expires"
        );
        assert!(
            !app.needs_animation(),
            "expired mode banner must stop requesting ticks"
        );
    }
    /// Draw-entry resync: an `expires_at` crossing between pushes must close
    /// the `/announcements` gate on the next frame; a later live list re-opens
    /// it through the same divergence check.
    #[test]
    fn slash_gate_resyncs_when_critical_expires_between_pushes() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.agents
            .get_mut(&id)
            .unwrap()
            .set_has_session_announcements(true);
        app.active_announcements = vec![xai_grok_announcements::RemoteAnnouncement {
            id: Some("expired".into()),
            message: Some("gone".into()),
            severity: Some("critical".into()),
            expires_at: Some("2000-01-01T00:00:00Z".into()),
            ..Default::default()
        }];
        app.resync_announcement_slash_gate_on_divergence();
        assert!(
            !app.agents[&id]
                .prompt
                .slash_controller
                .has_session_announcements(),
            "expired-only list must close the gate on the next frame"
        );
        app.active_announcements = vec![xai_grok_announcements::RemoteAnnouncement {
            id: Some("live".into()),
            message: Some("new outage".into()),
            severity: Some("critical".into()),
            ..Default::default()
        }];
        app.resync_announcement_slash_gate_on_divergence();
        assert!(
            app.agents[&id]
                .prompt
                .slash_controller
                .has_session_announcements(),
            "a live critical must re-open the gate"
        );
    }
    /// Critical freezes tip TTL and must not arm needs_animation for a tip
    /// that is not counting down (session-long metronome heat).
    #[test]
    fn ephemeral_tip_frozen_under_critical_does_not_request_animation_or_burn_ttl() {
        use std::collections::HashMap;
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            let _ = agent.ephemeral_tip.show(
                crate::tips::EphemeralTip::new("t", ratatui::text::Line::from("TIP")),
                &mut HashMap::new(),
            );
            agent.session_banner_active = true;
        }
        let before = app.agents[&id]
            .ephemeral_tip
            .ticks_remaining()
            .expect("tip active");
        assert!(
            !app.agents[&id].ephemeral_tip_needs_tick(),
            "critical must freeze tip tick policy"
        );
        assert!(
            !app.needs_animation(),
            "frozen tip under critical must not arm the metronome on an idle agent"
        );
        for _ in 0..10 {
            app.tick();
        }
        assert_eq!(
            app.agents[&id].ephemeral_tip.ticks_remaining(),
            Some(before),
            "TTL must not burn while critical occludes"
        );
        app.agents.get_mut(&id).unwrap().session_banner_active = false;
        assert!(
            app.needs_animation(),
            "unfreezing must re-arm tip countdown ticks"
        );
        app.tick();
        let after = app.agents[&id]
            .ephemeral_tip
            .ticks_remaining()
            .expect("tip still active");
        assert!(after < before, "TTL must resume when critical clears");
    }
    /// The word-select tip's long TTL is bounded by prompt divergence: ANY
    /// prompt change since the tip was shown (typed here; the snapshot guard
    /// covers paste/drop identically) refuses the chord immediately and
    /// retires the tip on the next tick, so Ctrl+Y goes back to yank.
    #[test]
    fn word_select_tip_retires_on_prompt_divergence_and_accepts_before() {
        use std::collections::HashMap;
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.last_terminal_size = (80, 30);
            agent.active_pane = crate::views::agent::ActivePane::Prompt;
            let _ = agent.ephemeral_tip.show(
                crate::tips::word_select::word_select_tip(),
                &mut HashMap::new(),
            );
            agent.word_select_tip_prompt_snapshot = Some(agent.prompt.text().to_string());
        }
        let out = app.handle_input(&key_event(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(
            matches!(out, InputOutcome::Action(Action::AcceptWordSelectTip)),
            "Ctrl+Y with the tip up must route to accept, got {out:?}"
        );
        let _ = app.handle_input(&key_event(KeyCode::Char('a'), KeyModifiers::NONE));
        let out = app.handle_input(&key_event(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(
            !matches!(out, InputOutcome::Action(Action::AcceptWordSelectTip)),
            "Ctrl+Y after a prompt edit must not accept, got {out:?}"
        );
        app.tick();
        assert!(
            !app.agents[&id].ephemeral_tip.is_active(),
            "prompt divergence must retire the word-select tip on tick"
        );
        assert!(
            app.agents[&id].word_select_tip_prompt_snapshot.is_none(),
            "snapshot must drop with the tip"
        );
    }
    #[test]
    fn needs_animation_gates_image_viewer_loading() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        assert!(!app.needs_animation());
        let viewer = crate::prompt_images::ImageViewerState::open_from_path_deferred(
            std::path::Path::new("/nonexistent/image_gate_test.png"),
        );
        assert!(viewer.loading, "deferred open must be in loading state");
        app.agents.get_mut(&id).unwrap().image_viewer = Some(viewer);
        assert!(
            app.needs_animation(),
            "image_viewer.loading must request ticks (poll/spawn load path)"
        );
        let mut terminal = false;
        for _ in 0..200 {
            app.tick();
            let agent = &app.agents[&id];
            if agent.image_viewer.is_none()
                || agent.toast.is_some()
                || agent.image_load_rx.is_some()
                || agent.image_viewer.as_ref().is_some_and(|v| !v.loading)
            {
                terminal = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            terminal,
            "tick() must progress image load (spawn rx, fail toast, or clear loading)"
        );
        app.agents.get_mut(&id).unwrap().image_viewer = None;
        app.agents.get_mut(&id).unwrap().image_load_rx = None;
        app.agents.get_mut(&id).unwrap().toast = None;
        assert!(!app.needs_animation());
    }
    #[test]
    fn needs_animation_gates_loading_replay() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        assert!(!app.needs_animation());
        app.agents.get_mut(&id).unwrap().session.loading_replay = true;
        assert!(
            app.needs_animation(),
            "loading_replay (attach/resume) must keep ticks alive"
        );
        let _ = app.tick();
        app.agents.get_mut(&id).unwrap().session.loading_replay = false;
        assert!(!app.needs_animation());
    }
    #[test]
    fn active_scroll_stream_arms_scroll_clock_not_animation_ticks() {
        use crate::input::mouse::{ScrollConfig, ScrollDirection};
        let mut app = test_app_with_agent();
        assert!(!app.needs_animation());
        let _ = app
            .scroll_state
            .on_scroll_event(ScrollDirection::Up, ScrollConfig::default());
        assert!(
            app.scroll_state.has_active_stream(),
            "fixture: scroll event must arm an active stream"
        );
        assert!(
            !app.needs_animation(),
            "scroll streams must not demand animation ticks (scroll clock owns pacing)"
        );
        assert!(
            app.scroll_state
                .scroll_clock_deadline(std::time::Instant::now())
                .is_some(),
            "active stream must expose a scroll-clock deadline to the event loop"
        );
        let mut finalized = false;
        for _ in 0..200 {
            let _ = app.tick_scroll();
            if !app.scroll_state.has_active_stream() {
                finalized = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            finalized,
            "tick_scroll() must finalize the scroll stream without a metronome"
        );
        assert!(
            app.scroll_state
                .scroll_clock_deadline(std::time::Instant::now())
                .is_none(),
            "finalized stream must disarm the scroll clock (no idle wakeups)"
        );
        assert!(!app.needs_animation());
    }
    #[test]
    fn handle_input_scroll_suppressed_events_do_not_report_changed() {
        let mut app = test_app_with_agent();
        let wheel = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        const EVENTS: u32 = 30;
        let start = std::time::Instant::now();
        let mut changed = 0u32;
        for _ in 0..EVENTS {
            if matches!(app.handle_input(&wheel), InputOutcome::Changed) {
                changed += 1;
            }
            assert!(
                app.scroll_state.has_active_stream(),
                "wheel burst must keep the stream active"
            );
        }
        let elapsed_ms = start.elapsed().as_millis() as u32;
        assert!(
            changed >= 1,
            "a flushing wheel event must still report Changed"
        );
        let max_changed = elapsed_ms / 16 + 2;
        assert!(
            changed <= max_changed,
            "cadence-suppressed wheel events must not report Changed: got \
             {changed} Changed outcomes from {EVENTS} events in {elapsed_ms}ms \
             (bound {max_changed})"
        );
        assert!(
            app.scroll_state
                .scroll_clock_deadline(std::time::Instant::now())
                .is_some(),
            "armed stream must schedule a scroll-clock deadline"
        );
    }
    #[test]
    fn needs_animation_gates_dashboard_file_search() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        assert!(app.agents[&id].session.state.is_idle());
        app.active_view = ActiveView::AgentDashboard;
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        assert!(
            !app.needs_animation(),
            "idle AgentDashboard with no agents alive and no @-search must not request ticks"
        );
        app.dashboard
            .as_mut()
            .unwrap()
            .dispatch
            .file_search
            .update_context("@a", 2);
        assert!(
            app.dashboard
                .as_ref()
                .unwrap()
                .dispatch
                .file_search
                .context()
                .is_some(),
            "fixture: dispatch @-context must be armed"
        );
        assert!(
            app.needs_animation(),
            "dispatch file_search.context() on AgentDashboard must request ticks"
        );
        let _ = app.tick();
        assert!(
            app.dashboard
                .as_ref()
                .unwrap()
                .dispatch
                .file_search
                .context()
                .is_some(),
            "tick() must not clear dispatch @-context"
        );
        assert!(app.needs_animation());
        app.dashboard
            .as_mut()
            .unwrap()
            .dispatch
            .file_search
            .update_context("", 0);
        assert!(
            !app.needs_animation(),
            "clearing dispatch @-context stops ticks when agents stay idle"
        );
        app.dashboard
            .as_mut()
            .unwrap()
            .peek_reply
            .file_search
            .update_context("@b", 2);
        assert!(
            app.needs_animation(),
            "peek_reply file_search.context() on AgentDashboard must request ticks"
        );
        let _ = app.tick();
        app.dashboard
            .as_mut()
            .unwrap()
            .peek_reply
            .file_search
            .update_context("", 0);
        assert!(!app.needs_animation());
    }
    #[test]
    fn tick_drains_tracing_rx_and_does_not_metronome_on_channel() {
        let mut app = test_app_with_agent();
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(8);
        for i in 0..5 {
            tx.try_send(format!("trace line {i}"))
                .expect("queue tracer line");
        }
        app.tracing_rx = Some(rx);
        assert!(
            !app.needs_animation(),
            "non-dev: queued tracer lines must not request animation ticks"
        );
        let _ = app.tick();
        assert!(
            matches!(
                app.tracing_rx.as_mut().unwrap().try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "tick() must drain the tracer channel (bounded; cannot grow unbounded)"
        );
        assert!(
            !app.needs_animation(),
            "non-dev: a present-but-drained tracer channel must not request ticks"
        );
        drop(tx);
    }
    #[test]
    fn needs_animation_gates_btw_loading_spinner() {
        use crate::views::btw_overlay::BtwOverlayState;
        use crate::views::turn_status::SPINNER_DIVISOR;
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        assert!(!app.needs_animation());
        app.agents.get_mut(&id).unwrap().btw_state = Some(BtwOverlayState::Loading {
            question: "what is X?".into(),
        });
        assert!(app.needs_animation());
        let saw_redraw = (0..SPINNER_DIVISOR).any(|_| app.tick());
        assert!(
            saw_redraw,
            "Loading must redraw at spinner cadence while idle"
        );
        app.agents.get_mut(&id).unwrap().btw_state =
            Some(BtwOverlayState::done("what is X?".into(), "X is …".into()));
        assert!(!app.needs_animation());
        app.agents.get_mut(&id).unwrap().btw_state = Some(BtwOverlayState::Error {
            question: "what is X?".into(),
            error: "boom".into(),
        });
        assert!(!app.needs_animation());
    }
    #[test]
    fn needs_animation_gates_todo_badge_flash() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        assert!(!app.needs_animation(), "idle agent must not request ticks");
        app.agents
            .get_mut(&id)
            .unwrap()
            .todo
            .update_todos(vec![xai_grok_shell::tools::TodoItem {
                content: "do the thing".into(),
                priority: Default::default(),
                status: xai_grok_shell::tools::TodoStatus::InProgress,
                meta: None,
            }]);
        assert!(
            app.agents[&id].todo.badge_needs_tick(),
            "fixture: a counts change must arm the badge flash"
        );
        assert!(
            app.needs_animation(),
            "an active todo badge flash must request animation ticks"
        );
        app.agents
            .get_mut(&id)
            .unwrap()
            .todo
            .expire_badge_flash_for_test();
        let _ = app.tick();
        assert!(
            !app.agents[&id].todo.badge_needs_tick(),
            "tick() must clear the expired badge flash (badge_tick)"
        );
        assert!(
            !app.needs_animation(),
            "a cleared badge flash must stop requesting ticks"
        );
    }
    #[test]
    fn needs_animation_gates_pending_acp_command_sync() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        assert!(
            !app.needs_animation(),
            "an idle, fully-synced agent must not request ticks"
        );
        app.agents
            .get_mut(&id)
            .unwrap()
            .session
            .available_commands_generation += 1;
        assert!(
            app.agents[&id].acp_synced_generation
                != app.agents[&id].session.available_commands_generation,
            "fixture: a commands update must leave the catalog sync pending"
        );
        assert!(
            app.needs_animation(),
            "a pending ACP command-catalog sync must request animation ticks"
        );
        let _ = app.tick();
        assert_eq!(
            app.agents[&id].acp_synced_generation,
            app.agents[&id].session.available_commands_generation,
            "tick() must reconcile the slash-command catalog generation"
        );
        assert!(
            !app.needs_animation(),
            "a reconciled command catalog must stop requesting ticks"
        );
    }
    #[test]
    fn needs_animation_gates_pending_turn_end_reconcile() {
        use super::super::dispatch::{TURN_END_RECONCILE_GRACE, reconcile_overdue_turn_ends};
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::AgentDashboard;
        assert!(app.agents[&id].session.state.is_idle());
        assert!(
            !app.needs_animation(),
            "idle background agent on the dashboard must not request ticks"
        );
        app.agents.get_mut(&id).unwrap().pending_turn_end_reconcile =
            Some(super::super::agent_view::PendingTurnEnd {
                prompt_id: "pid-stuck".into(),
                stop_reason: Some("end_turn".into()),
                agent_result: None,
                cancel_trigger: None,
                received_at: std::time::Instant::now()
                    - (TURN_END_RECONCILE_GRACE + std::time::Duration::from_secs(1)),
            });
        assert!(
            app.needs_animation(),
            "an armed turn-end reconcile must request ticks even for a background agent"
        );
        let _ = reconcile_overdue_turn_ends(&mut app);
        assert!(
            app.agents[&id].pending_turn_end_reconcile.is_none(),
            "reconcile must clear the overdue marker"
        );
        assert!(
            !app.needs_animation(),
            "a cleared reconcile marker must stop requesting ticks"
        );
    }
    #[test]
    fn needs_animation_gates_subagent_image_viewer_loading() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let child_sid = "child-img-gate";
        let child = idle_child_view(&app, 1, child_sid);
        app.agents
            .get_mut(&id)
            .unwrap()
            .subagent_views
            .insert(child_sid.to_string(), child);
        assert!(
            !app.needs_animation(),
            "an idle agent with an idle subagent child must not request ticks"
        );
        let viewer = crate::prompt_images::ImageViewerState::open_from_path_deferred(
            std::path::Path::new("/nonexistent/child_img_gate.png"),
        );
        assert!(viewer.loading, "deferred open must be in loading state");
        app.agents
            .get_mut(&id)
            .unwrap()
            .subagent_views
            .get_mut(child_sid)
            .unwrap()
            .image_viewer = Some(viewer);
        assert!(
            app.needs_animation(),
            "a loading image viewer on a subagent CHILD must request ticks (child arm)"
        );
        let mut terminal = false;
        for _ in 0..200 {
            app.tick();
            let child = &app.agents[&id].subagent_views[child_sid];
            if child.image_viewer.is_none()
                || child.toast.is_some()
                || child.image_load_rx.is_some()
                || child.image_viewer.as_ref().is_some_and(|v| !v.loading)
            {
                terminal = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            terminal,
            "tick() must progress the CHILD image load (shared tick_agent_image_load)"
        );
        {
            let child = app
                .agents
                .get_mut(&id)
                .unwrap()
                .subagent_views
                .get_mut(child_sid)
                .unwrap();
            child.image_viewer = None;
            child.image_load_rx = None;
            child.toast = None;
        }
        assert!(
            !app.needs_animation(),
            "a cleared child image viewer must stop requesting ticks"
        );
    }
    #[test]
    fn gboom_backgrounded_game_drops_held_movement() {
        use crate::gboom::GboomState;
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let mut game = GboomState::new();
        game.handle_key(&KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        game.handle_key(&KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert!(
            game.any_movement_held(),
            "press should latch a movement hold"
        );
        app.agents.get_mut(&id).unwrap().gboom = Some(game);
        app.active_view = ActiveView::Agent(id);
        app.gboom_release_backgrounded_games();
        assert!(
            app.agents[&id].gboom.as_ref().unwrap().any_movement_held(),
            "the active game must keep its holds"
        );
        app.active_view = ActiveView::Welcome;
        app.gboom_release_backgrounded_games();
        assert!(
            !app.agents[&id].gboom.as_ref().unwrap().any_movement_held(),
            "a backgrounded game must drop its holds"
        );
    }
    /// `Event::Resize` must close the tip show gate of every agent view —
    /// parent AND fullscreen-capable subagent children — until the next draw
    /// re-measures: a trigger firing between the event and the (debounced)
    /// resize draw would otherwise act on the pre-resize measurement and burn
    /// a seen count on a tip the new layout can never paint. The event must
    /// NOT write the full terminal size into `last_terminal_size` — views can
    /// paint into chrome-shrunk rects, so the event height proves nothing
    /// about the banner row.
    #[test]
    fn resize_event_closes_tip_show_gate_until_redraw() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let child_sid = "child-session";
        {
            let mut child = idle_child_view(&app, 1, child_sid);
            child.note_terminal_size((80, 28));
            let agent = app.agents.get_mut(&id).unwrap();
            agent.note_terminal_size((80, 30));
            agent.subagent_views.insert(child_sid.to_string(), child);
        }
        let _ = app.handle_input(&Event::Resize(120, 50));
        let agent = app.agents.get_mut(&id).unwrap();
        assert_eq!(
            agent.last_terminal_size,
            (80, 30),
            "event must not overwrite the draw-measured rect size"
        );
        let mut counts = std::collections::HashMap::new();
        let tip = || {
            crate::tips::EphemeralTip::new("t", ratatui::text::Line::from("hint"))
                .with_session_seen_cap("t_seen", 2)
        };
        assert!(!agent.show_ephemeral_tip(tip(), &mut counts));
        assert!(counts.is_empty(), "stale-size show must not burn a count");
        let child = agent.subagent_views.get_mut(child_sid).unwrap();
        assert!(!child.show_ephemeral_tip(tip(), &mut counts));
        assert!(counts.is_empty(), "child stale-size show must not burn");
        child.note_terminal_size((118, 46));
        assert!(child.show_ephemeral_tip(tip(), &mut counts));
        let agent = app.agents.get_mut(&id).unwrap();
        agent.note_terminal_size((120, 50));
        assert!(agent.show_ephemeral_tip(tip(), &mut counts));
        assert_eq!(counts.get("t_seen"), Some(&2));
    }
    #[test]
    fn apply_auth_meta_hides_usage_for_team_users() {
        let mut app = test_app();
        assert!(app.usage_visible);
        let meta = xai_grok_shell::auth::AuthMeta {
            team_id: Some("team-uuid".into()),
            team_name: Some("Acme Corp".into()),
            ..Default::default()
        };
        app.apply_auth_meta(&meta);
        assert!(!app.usage_visible);
        assert_eq!(app.team_id.as_deref(), Some("team-uuid"));
    }
    #[test]
    fn apply_auth_meta_shows_usage_for_personal_users() {
        let mut app = test_app();
        app.usage_visible = false;
        let meta = xai_grok_shell::auth::AuthMeta::default();
        app.apply_auth_meta(&meta);
        assert!(app.usage_visible);
    }
    #[test]
    fn apply_auth_meta_clears_api_key_flag_and_shows_usage_on_personal_login() {
        let mut app = test_app();
        app.is_api_key_auth = true;
        app.usage_visible = false;
        app.apply_auth_meta(&xai_grok_shell::auth::AuthMeta::default());
        assert!(!app.is_api_key_auth);
        assert!(app.usage_visible);
    }
    #[test]
    fn apply_auth_meta_api_key_enables_voice_and_skips_tier_gate() {
        let mut app = test_app();
        advertise_media_tools(&mut app);
        assert!(!app.voice_mode_enabled);
        app.apply_auth_meta(&xai_grok_shell::auth::AuthMeta {
            auth_mode: Some("ApiKey".into()),
            subscription_tier: Some("API Key".into()),
            ..Default::default()
        });
        assert!(app.is_api_key_auth);
        assert!(!app.usage_visible);
        assert!(app.tier_restricted_commands.is_empty());
        assert_tier_restricted_commands_present(&app);
        assert!(!app.is_voice_tier_restricted());
        assert!(app.voice_mode_enabled);
        let mut app = test_app();
        app.apply_auth_meta(&xai_grok_shell::auth::AuthMeta {
            subscription_tier: Some("api_key".into()),
            ..Default::default()
        });
        assert!(app.is_api_key_auth);
        assert!(app.voice_mode_enabled);
        assert!(app.tier_restricted_commands.is_empty());
        app.apply_auth_meta(&xai_grok_shell::auth::AuthMeta {
            auth_mode: Some("Oidc".into()),
            subscription_tier: Some("Free".into()),
            ..Default::default()
        });
        assert!(!app.is_api_key_auth);
        assert!(!app.voice_mode_enabled);
        assert!(app.usage_visible);
        assert!(!app.tier_restricted_commands.is_empty());
    }
    fn expected_tier_restricted_commands() -> Vec<String> {
        TIER_RESTRICTED_COMMANDS
            .iter()
            .map(|n| (*n).to_string())
            .collect()
    }
    /// Make every tier-restricted command visible on the welcome prompt so the
    /// present/absent assertions exercise the deny list, not incidental
    /// fail-closed hiding:
    /// - `/imagine`, `/imagine-video` are `required_tools()`-gated, so advertise
    ///   their tools (otherwise the registry fail-closes them).
    /// - `/voice` is fail-closed hidden until the remote flag turns it on, so
    ///   reveal it via the registry directly. (We drive the prompt's registry
    ///   rather than `apply_voice_mode_enabled`, which also flips a process-global
    ///   atomic and would leak across parallel tests.)
    fn advertise_media_tools(app: &mut AppView) {
        app.welcome_prompt
            .slash_controller
            .registry_mut()
            .set_available_tools(
                ["image_gen", "image_to_video"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            );
        app.welcome_prompt.set_voice_visible(true);
    }
    fn assert_tier_restricted_commands_absent(app: &AppView) {
        let reg = app.welcome_prompt.slash_controller.registry();
        for name in TIER_RESTRICTED_COMMANDS {
            assert!(
                reg.get(name).is_none(),
                "/{name} must be denied on a restricted tier"
            );
        }
        assert!(reg.get("cost").is_none(), "/cost alias must be denied");
    }
    fn assert_tier_restricted_commands_present(app: &AppView) {
        let reg = app.welcome_prompt.slash_controller.registry();
        for name in TIER_RESTRICTED_COMMANDS {
            assert!(
                reg.get(name).is_some(),
                "/{name} must be available when not tier-restricted (tools advertised)"
            );
        }
    }
    #[test]
    fn apply_auth_meta_restricts_usage_for_free_tier() {
        let mut app = test_app();
        advertise_media_tools(&mut app);
        app.apply_auth_meta(&xai_grok_shell::auth::AuthMeta::default());
        assert_eq!(
            app.tier_restricted_commands,
            expected_tier_restricted_commands()
        );
        assert_tier_restricted_commands_absent(&app);
        assert!(app.usage_visible);
    }
    #[test]
    fn apply_auth_meta_restricts_usage_for_x_basic_tier() {
        let mut app = test_app();
        advertise_media_tools(&mut app);
        let meta = xai_grok_shell::auth::AuthMeta {
            subscription_tier: Some("X Basic".into()),
            ..Default::default()
        };
        app.apply_auth_meta(&meta);
        assert_eq!(
            app.tier_restricted_commands,
            expected_tier_restricted_commands()
        );
        assert_tier_restricted_commands_absent(&app);
    }
    #[test]
    fn apply_auth_meta_lifts_restrictions_for_paid_tiers_and_teams() {
        let mut app = test_app();
        advertise_media_tools(&mut app);
        let meta = xai_grok_shell::auth::AuthMeta {
            subscription_tier: Some("SuperGrok".into()),
            ..Default::default()
        };
        app.apply_auth_meta(&meta);
        assert!(app.tier_restricted_commands.is_empty());
        assert_tier_restricted_commands_present(&app);
        let mut app = test_app();
        advertise_media_tools(&mut app);
        app.apply_auth_meta(&xai_grok_shell::auth::AuthMeta::default());
        assert!(!app.tier_restricted_commands.is_empty());
        app.subscription_tier = Some("SuperGrok".into());
        app.apply_tier_restrictions();
        assert!(app.tier_restricted_commands.is_empty());
        assert_tier_restricted_commands_present(&app);
        let mut app = test_app();
        let meta = xai_grok_shell::auth::AuthMeta {
            team_id: Some("team-uuid".into()),
            team_name: Some("Acme Corp".into()),
            ..Default::default()
        };
        app.apply_auth_meta(&meta);
        assert!(app.tier_restricted_commands.is_empty());
    }
    #[test]
    fn is_restricted_tier_classification() {
        assert!(is_restricted_tier(None));
        assert!(is_restricted_tier(Some("")));
        assert!(is_restricted_tier(Some("Free")));
        assert!(is_restricted_tier(Some("X Basic")));
        assert!(is_restricted_tier(Some("x_basic")));
        assert!(!is_restricted_tier(Some("SuperGrok")));
        assert!(!is_restricted_tier(Some("SuperGrok Heavy")));
        assert!(!is_restricted_tier(Some("X Premium")));
        assert!(!is_restricted_tier(Some("X Premium+")));
        assert!(!is_restricted_tier(Some("SomeFutureTier")));
    }
    #[test]
    fn voice_included_in_tier_restricted_commands() {
        assert!(TIER_RESTRICTED_COMMANDS.contains(&"voice"));
    }
    #[test]
    fn is_voice_tier_restricted_tracks_tier() {
        let mut app = test_app();
        app.apply_auth_meta(&xai_grok_shell::auth::AuthMeta::default());
        assert!(app.is_voice_tier_restricted());
        let mut app = test_app();
        let meta = xai_grok_shell::auth::AuthMeta {
            subscription_tier: Some("SuperGrok".into()),
            ..Default::default()
        };
        app.apply_auth_meta(&meta);
        assert!(!app.is_voice_tier_restricted());
    }
    #[test]
    fn apply_auth_meta_clears_gate_on_subscription() {
        let mut app = test_app();
        app.gate = Some(xai_grok_shell::auth::GateInfo {
            message: "Subscribe to use Grok Build".into(),
            url: Some("https://grok.com/supergrok?referrer=grok-build".into()),
            label: None,
        });
        assert!(app.is_access_blocked());
        let meta = xai_grok_shell::auth::AuthMeta::default();
        app.apply_auth_meta(&meta);
        assert!(app.gate.is_none());
        assert!(app.has_access());
    }
    #[test]
    fn apply_auth_meta_gate_unchanged_when_still_gated() {
        let mut app = test_app();
        let gate = xai_grok_shell::auth::GateInfo {
            message: "Subscribe".into(),
            url: None,
            label: None,
        };
        app.gate = Some(gate.clone());
        let meta = xai_grok_shell::auth::AuthMeta {
            gate: Some(gate),
            ..Default::default()
        };
        app.apply_auth_meta(&meta);
        assert!(app.gate.is_some());
        assert!(app.is_access_blocked());
    }
    #[test]
    fn welcome_ctrl_q_requires_confirmation() {
        let mut app = test_app();
        let outcome = app.handle_input(&key_event(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(matches!(outcome, InputOutcome::Changed));
        let pending = app
            .pending_action
            .as_ref()
            .expect("expected pending action");
        assert!(matches!(pending.action, Action::Quit));
        assert_eq!(
            pending.shortcut,
            KeyShortcut::from(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL))
        );
    }
    #[test]
    fn welcome_ctrl_u_update_keeps_priority_over_foreign_resume() {
        let mut app = test_app();
        app.foreign_session_compat =
            xai_grok_workspace::foreign_sessions::EnabledForeignSessionSources {
                cursor: true,
                ..Default::default()
            };
        let crate::app::actions::Effect::CanonicalizeForeignResumeCwd {
            requested_cwd,
            launch_token,
        } = app.begin_foreign_resume_detection().unwrap()
        else {
            panic!("expected canonicalization effect");
        };
        let canonical_cwd = dunce::canonicalize(&requested_cwd).unwrap();
        assert!(app.accept_foreign_resume_canonical_cwd(
            launch_token,
            &requested_cwd,
            Some(canonical_cwd.clone()),
        ));
        app.apply_foreign_resume_detection(
            launch_token,
            &canonical_cwd,
            Some(xai_grok_workspace::foreign_sessions::RecentForeignSession {
                tool: xai_grok_workspace::foreign_sessions::ForeignSessionTool::Cursor,
                native_id: "cursor-session".into(),
                age: std::time::Duration::from_secs(30),
            }),
        );
        let key = key_event(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(matches!(
            app.handle_input(&key),
            InputOutcome::Action(Action::ResumeForeignSession)
        ));
        app.pending_update_version = Some("9.9.9".into());
        assert!(matches!(
            app.handle_input(&key),
            InputOutcome::Action(Action::QuitForUpdate)
        ));
    }
    #[test]
    fn minimal_ctrl_t_toggles_todo_panel() {
        let mut app = test_app_with_agent();
        app.screen_mode = ScreenMode::Minimal;
        assert!(!app.minimal_state.show_todos);
        let out = app.handle_input(&key_event(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(matches!(out, InputOutcome::Changed));
        assert!(
            app.minimal_state.show_todos,
            "Ctrl+T pins the panel visible"
        );
        let _ = app.handle_input(&key_event(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(
            !app.minimal_state.show_todos,
            "Ctrl+T again unpins the panel"
        );
    }
    #[test]
    fn non_minimal_ctrl_t_leaves_todo_panel_flag_untouched() {
        let mut app = test_app_with_agent();
        app.screen_mode = ScreenMode::Inline;
        assert!(!app.minimal_state.show_todos);
        let _ = app.handle_input(&key_event(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(
            !app.minimal_state.show_todos,
            "the minimal todo-panel flag must never flip outside minimal mode"
        );
    }
    /// The minimal info-row transcript hint and the Ctrl+O key remap are gated
    /// on the same predicate. Ctrl+O opens the transcript pager unless it is
    /// the interject chord (Apple Terminal) AND an interject would actually
    /// consume the press (turn running + non-empty composer, turn running +
    /// queued follow-up with empty composer, or editing a queued row) — at
    /// idle / empty composer with no queue the interject path is a silent
    /// no-op, so the remap keeps the key (it looked simply dead before).
    #[test]
    fn minimal_ctrl_o_transcript_predicate_tracks_interject_binding() {
        let mut app = test_app_with_agent();
        app.registry = ActionRegistry::non_vscode_for_test();
        assert!(
            crate::minimal_api::minimal_ctrl_o_opens_transcript(&app),
            "Ctrl+O opens the transcript when interject doesn't own the chord"
        );
        app.registry = ActionRegistry::apple_terminal_for_test();
        assert!(
            crate::minimal_api::minimal_ctrl_o_opens_transcript(&app),
            "idle + empty composer: Ctrl+O must open the transcript, not no-op"
        );
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        assert!(
            crate::minimal_api::minimal_ctrl_o_opens_transcript(&app),
            "running turn + empty composer + empty queue: still no interjection"
        );
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.prompt.set_text("");
            agent.session.enqueue_prompt("queued follow-up".into());
        }
        assert!(
            !crate::minimal_api::minimal_ctrl_o_opens_transcript(&app),
            "running + empty composer + queue: Ctrl+O must yield to send-now"
        );
        app.agents
            .get_mut(&id)
            .unwrap()
            .session
            .pending_prompts
            .clear();
        app.agents.get_mut(&id).unwrap().prompt.set_text("steer it");
        assert!(
            !crate::minimal_api::minimal_ctrl_o_opens_transcript(&app),
            "running turn + payload: Ctrl+O must yield to interject"
        );
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.state = AgentState::Idle;
            agent.prompt_mode = PromptMode::EditingQueued {
                id: 1,
                original: String::new(),
                server_id: None,
                kind: crate::app::agent::QueueEntryKind::Prompt,
            };
        }
        assert!(
            !crate::minimal_api::minimal_ctrl_o_opens_transcript(&app),
            "editing a queued row: Ctrl+O must stay the interject/save key"
        );
    }
    /// In minimal mode Ctrl+O routes to `Action::OpenTranscriptPager` (unless
    /// interject owns the chord AND would consume the press — see the
    /// predicate test above).
    #[test]
    fn minimal_ctrl_o_opens_transcript_pager() {
        let mut app = test_app_with_agent();
        app.screen_mode = ScreenMode::Minimal;
        app.registry = ActionRegistry::non_vscode_for_test();
        let out = app.handle_input(&key_event(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(
            matches!(out, InputOutcome::Action(Action::OpenTranscriptPager)),
            "expected OpenTranscriptPager, got {out:?}"
        );
    }
    /// Apple Terminal (interject = Ctrl+O), minimal mode: at idle the interject
    /// path would silently no-op, so Ctrl+O must open the transcript — this was
    /// the "Ctrl+O appears dead on Mac" report. With a running turn and text in
    /// the composer the same key must send-now (cancel-and-send). With a running
    /// turn, empty composer, and a queued follow-up it must force-send that row
    /// (send-now).
    #[test]
    fn minimal_ctrl_o_on_apple_terminal_transcript_at_idle_interject_with_payload() {
        let mut app = test_app_with_agent();
        app.screen_mode = ScreenMode::Minimal;
        app.registry = ActionRegistry::apple_terminal_for_test();
        let out = app.handle_input(&key_event(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(
            matches!(out, InputOutcome::Action(Action::OpenTranscriptPager)),
            "idle Apple-Terminal Ctrl+O must open the transcript, got {out:?}"
        );
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.prompt.set_text("steer it");
        }
        let out = app.handle_input(&key_event(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(
            matches!(out, InputOutcome::Action(Action::SendPromptNow { ref text, .. }) if
            text == "steer it"),
            "running Apple-Terminal Ctrl+O with payload must send-now, got {out:?}"
        );
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.prompt.set_text("");
            agent.session.enqueue_prompt("queued follow-up".into());
        }
        let out = app.handle_input(&key_event(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(
            matches!(out, InputOutcome::Action(Action::SendPromptNow { ref text, .. }) if
            text == "queued follow-up"),
            "running + empty + queue: Apple-Terminal Ctrl+O must send-now, got {out:?}"
        );
        assert!(
            app.agents[&id].session.pending_prompts.is_empty(),
            "queued row must be consumed by prompt-path send-now"
        );
    }
    /// In minimal mode the `ToggleQueue` chord (Ctrl+; here) must commit the
    /// read-only `/queue` snapshot instead of toggling the full-TUI queue
    /// pane: that pane never renders in minimal, so the old routing focused an
    /// invisible pane that swallowed every subsequent keystroke.
    #[test]
    fn minimal_toggle_queue_chord_shows_queue_block() {
        let mut app = test_app_with_agent();
        app.screen_mode = ScreenMode::Minimal;
        app.registry = ActionRegistry::non_vscode_for_test();
        let out = app.handle_input(&key_event(KeyCode::Char(';'), KeyModifiers::CONTROL));
        assert!(
            matches!(out, InputOutcome::Action(Action::ShowQueue)),
            "expected ShowQueue, got {out:?}"
        );
        app.screen_mode = ScreenMode::Fullscreen;
        let out = app.handle_input(&key_event(KeyCode::Char(';'), KeyModifiers::CONTROL));
        assert!(
            !matches!(out, InputOutcome::Action(Action::ShowQueue)),
            "full TUI must keep the queue-pane toggle, got {out:?}"
        );
    }
    fn welcome_session_entry(id: &str) -> SessionPickerEntry {
        SessionPickerEntry {
            id: id.into(),
            summary: id.into(),
            updated_at: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            cwd: "/tmp/repo".into(),
            hostname: None,
            source: "local".into(),
            model_id: None,
            num_messages: 0,
            last_active_at: None,
            branch: None,
            repo_name: "tmp-repo".into(),
            worktree_label: None,
            card_detail: None,
        }
    }
    fn open_welcome_session_picker(app: &mut AppView) {
        crate::appearance::cache::set_vim_mode(false);
        app.session_picker_entries = Some(vec![welcome_session_entry("session-0")]);
        app.session_picker_state.search_active = true;
    }
    #[test]
    fn welcome_session_picker_ctrl_w_resumes_in_worktree_while_search_is_focused() {
        let mut app = test_app();
        open_welcome_session_picker(&mut app);
        app.session_picker_state.set_query("session");
        let outcome = app.handle_input(&key_event(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::PickSessionInWorktree(0))
        ));
        assert_eq!(app.session_picker_state.query(), "session");
    }
    #[test]
    fn welcome_session_picker_ctrl_d_keeps_global_quit_precedence() {
        let mut app = test_app();
        open_welcome_session_picker(&mut app);
        app.session_picker_state.set_query("session");
        let outcome = app.handle_input(&ctrl_d());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(matches!(
            app.pending_action.as_ref().map(|pending| &pending.action),
            Some(Action::Quit)
        ));
        assert_eq!(app.session_picker_state.query(), "session");
    }
    #[test]
    fn welcome_session_picker_cursor_motion_does_not_trigger_deep_search() {
        let mut app = test_app();
        open_welcome_session_picker(&mut app);
        app.session_picker_state.set_query("session");
        let outcome = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.session_picker_state.query(), "session");
    }
    #[test]
    fn welcome_session_picker_ctrl_u_kills_to_cursor_and_triggers_deep_search() {
        let mut app = test_app();
        open_welcome_session_picker(&mut app);
        app.session_picker_state.set_query("session");
        let _ = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        let outcome = app.handle_input(&key_event(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::TriggerDeepSearch)
        ));
        assert_eq!(app.session_picker_state.query(), "n");
        assert_eq!(app.session_picker_state.query_cursor(), 0);
    }
    #[test]
    fn welcome_ctrl_w_opens_new_worktree_dialog() {
        let mut app = test_app();
        app.cwd_has_git_ancestor = true;
        let outcome = app.handle_input(&key_event(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::OpenNewWorktreeDialog)
        ));
    }
    #[test]
    fn welcome_ctrl_w_noop_outside_git_repo() {
        let mut app = test_app();
        app.cwd_has_git_ancestor = false;
        let outcome = app.handle_input(&key_event(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(matches!(outcome, InputOutcome::Unchanged));
    }
    #[test]
    fn welcome_trust_decline_keys_quit() {
        for code in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
            let mut app = test_app();
            app.trust_state = TrustState::Pending {
                workspace: std::path::PathBuf::from("/tmp/x"),
            };
            let outcome = app.handle_input(&key_event(code, KeyModifiers::NONE));
            assert!(
                matches!(outcome, InputOutcome::Action(Action::Quit)),
                "{code:?} on the trust prompt must quit, got {outcome:?}"
            );
        }
        let mut app = test_app();
        app.trust_state = TrustState::Pending {
            workspace: std::path::PathBuf::from("/tmp/x"),
        };
        let outcome = app.handle_input(&key_event(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Action(Action::TrustFolder)));
    }
    #[test]
    fn welcome_ctrl_c_requires_confirmation() {
        let mut app = test_app();
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Changed));
        let pending = app
            .pending_action
            .as_ref()
            .expect("expected pending action");
        assert!(matches!(pending.action, Action::Quit));
        assert_eq!(
            pending.shortcut,
            KeyShortcut::from(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        );
    }
    #[test]
    fn welcome_ctrl_c_double_press_quits() {
        let mut app = test_app();
        let _ = app.handle_input(&ctrl_c());
        assert!(app.pending_action.is_some());
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Action(Action::Quit)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn welcome_ctrl_d_requires_confirmation() {
        let mut app = test_app();
        let outcome = app.handle_input(&ctrl_d());
        assert!(matches!(outcome, InputOutcome::Changed));
        let pending = app
            .pending_action
            .as_ref()
            .expect("expected pending action");
        assert!(matches!(pending.action, Action::Quit));
        assert_eq!(
            pending.shortcut,
            KeyShortcut::from(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        );
    }
    #[test]
    fn menu_action_indices_without_changelog() {
        assert!(matches!(
            dispatch_menu_action(0, false, false, None),
            InputOutcome::Action(Action::OpenNewWorktreeDialog)
        ));
        assert!(matches!(
            dispatch_menu_action(1, false, false, None),
            InputOutcome::Action(Action::FetchSessionList)
        ));
        assert!(matches!(
            dispatch_menu_action(2, false, false, None),
            InputOutcome::Action(Action::Quit)
        ));
    }
    #[test]
    fn menu_action_changelog_sits_above_quit() {
        let md = Some("# notes");
        assert!(matches!(
            dispatch_menu_action(1, false, true, md),
            InputOutcome::Action(Action::FetchSessionList)
        ));
        assert!(matches!(
            dispatch_menu_action(2, false, true, md),
            InputOutcome::Action(Action::ShowReleaseNotes { .. })
        ));
        assert!(matches!(
            dispatch_menu_action(3, false, true, md),
            InputOutcome::Action(Action::Quit)
        ));
    }
    #[test]
    fn menu_action_changelog_before_fetch_is_noop() {
        assert!(matches!(
            dispatch_menu_action(2, false, true, None),
            InputOutcome::Unchanged
        ));
    }
    #[test]
    fn menu_action_indices_with_import_and_changelog() {
        let md = Some("# notes");
        assert!(matches!(
            dispatch_menu_action(0, true, true, md),
            InputOutcome::Action(Action::ImportClaudeSettings)
        ));
        assert!(matches!(
            dispatch_menu_action(1, true, true, md),
            InputOutcome::Action(Action::OpenNewWorktreeDialog)
        ));
        assert!(matches!(
            dispatch_menu_action(2, true, true, md),
            InputOutcome::Action(Action::FetchSessionList)
        ));
        assert!(matches!(
            dispatch_menu_action(3, true, true, md),
            InputOutcome::Action(Action::ShowReleaseNotes { .. })
        ));
        assert!(matches!(
            dispatch_menu_action(4, true, true, md),
            InputOutcome::Action(Action::Quit)
        ));
    }
    #[test]
    fn welcome_pending_ctrl_c_quits_instantly() {
        let mut app = test_app();
        app.auth_state = AuthState::Pending { error: None };
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Action(Action::Quit)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn welcome_authenticating_ctrl_c_quits_instantly() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Command,
        };
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Action(Action::Quit)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn page_keys_from_prompt_page_conversation_without_mutating_prompt() {
        let mut app = test_app_with_agent();
        let ActiveView::Agent(id) = app.active_view else {
            panic!("test app must start on an agent");
        };
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.set_active_pane(crate::app::agent_view::AgentPane::Prompt, true);
            agent.prompt.set_text("draft text");
            agent.prompt.textarea.set_selection(1, 5);
        }
        let prompt_before = {
            let agent = &app.agents[&id];
            (
                agent.prompt.text().to_owned(),
                agent.prompt.cursor(),
                agent.prompt.textarea.selection_range(),
            )
        };
        assert!(
            prompt_before.2.is_some(),
            "precondition: prompt selection is active"
        );
        for (code, page_up) in [(KeyCode::PageUp, true), (KeyCode::PageDown, false)] {
            let outcome = app.handle_input(&key_event(code, KeyModifiers::NONE));
            assert!(
                matches!(
                    (&outcome, page_up),
                    (InputOutcome::Action(Action::PageUp), true)
                        | (InputOutcome::Action(Action::PageDown), false)
                ),
                "{code:?} must page the conversation, got {outcome:?}",
            );
            let agent = &app.agents[&id];
            assert_eq!(agent.active_pane, crate::app::agent_view::AgentPane::Prompt);
            assert_eq!(agent.prompt.text(), prompt_before.0);
            assert_eq!(agent.prompt.cursor(), prompt_before.1);
            assert_eq!(agent.prompt.textarea.selection_range(), prompt_before.2);
        }
    }
    #[test]
    fn prompt_paging_scope_matches_agent_surface() {
        fn focused_app(screen_mode: ScreenMode) -> (AppView, super::super::agent::AgentId) {
            let mut app = test_app_with_agent();
            app.screen_mode = screen_mode;
            let ActiveView::Agent(id) = app.active_view else {
                panic!("test app must start on an agent");
            };
            app.agents
                .get_mut(&id)
                .unwrap()
                .set_active_pane(crate::app::agent_view::AgentPane::Prompt, true);
            (app, id)
        }
        #[derive(Clone, Copy)]
        enum Surface {
            Agent(ScreenMode),
            DashboardOverlay,
            DashboardPopup,
        }
        for (label, surface, paging_enabled) in [
            ("inline agent", Surface::Agent(ScreenMode::Inline), true),
            (
                "fullscreen agent",
                Surface::Agent(ScreenMode::Fullscreen),
                true,
            ),
            ("minimal agent", Surface::Agent(ScreenMode::Minimal), false),
            (
                "dashboard session overlay",
                Surface::DashboardOverlay,
                false,
            ),
            ("dashboard attached popup", Surface::DashboardPopup, false),
        ] {
            let screen_mode = match surface {
                Surface::Agent(mode) => mode,
                Surface::DashboardOverlay | Surface::DashboardPopup => ScreenMode::Inline,
            };
            let (mut app, id) = focused_app(screen_mode);
            match surface {
                Surface::DashboardOverlay => {
                    app.dashboard = Some(crate::views::dashboard::DashboardState::new());
                    app.dashboard.as_mut().unwrap().attached_agent = Some(id);
                }
                Surface::DashboardPopup => assert_eq!(attach_popup(&mut app), id),
                Surface::Agent(_) => {}
            }
            let outcome = app.handle_input(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
            assert_eq!(
                matches!(
                    &outcome,
                    InputOutcome::Action(Action::PageUp | Action::PageDown)
                ),
                paging_enabled,
                "{label} prompt paging scope mismatch: {outcome:?}",
            );
        }
    }
    #[test]
    fn prompt_page_actions_target_visible_fullscreen_child_scrollback() {
        fn make_pageable(agent: &mut AgentView) {
            for i in 0..16 {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::agent_message(
                        format!("message {i}\ncontinued"),
                    ));
            }
            agent.scrollback.prepare_layout(40, 6);
            agent.scrollback.goto_bottom();
            assert!(
                agent.scrollback.scroll_info().0 > 0,
                "precondition: scrollback must have a page above"
            );
        }
        let mut app = test_app_with_agent();
        app.screen_mode = ScreenMode::Fullscreen;
        let ActiveView::Agent(id) = app.active_view else {
            panic!("test app must start on an agent");
        };
        let child_sid = "page-target-child";
        let mut child = idle_child_view(&app, 1, child_sid);
        child.set_active_pane(crate::app::agent_view::AgentPane::Prompt, true);
        make_pageable(&mut child);
        {
            let parent = app.agents.get_mut(&id).unwrap();
            make_pageable(parent);
            parent.subagent_views.insert(child_sid.to_owned(), child);
            parent.active_subagent = Some(child_sid.to_owned());
        }
        let offsets = |app: &AppView| {
            let parent = &app.agents[&id];
            (
                parent.scrollback.scroll_info().0,
                parent.subagent_views[child_sid].scrollback.scroll_info().0,
            )
        };
        let before = offsets(&app);
        let outcome = app.handle_input(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
        let InputOutcome::Action(action @ Action::PageUp) = outcome else {
            panic!("child prompt PageUp must emit PageUp, got {outcome:?}");
        };
        let _ = super::super::dispatch::dispatch(action, &mut app);
        let after_up = offsets(&app);
        assert_eq!(after_up.0, before.0, "parent scrollback must not move");
        assert!(
            after_up.1 < before.1,
            "PageUp must move the visible child scrollback"
        );
        let outcome = app.handle_input(&key_event(KeyCode::PageDown, KeyModifiers::NONE));
        let InputOutcome::Action(action @ Action::PageDown) = outcome else {
            panic!("child prompt PageDown must emit PageDown, got {outcome:?}");
        };
        let _ = super::super::dispatch::dispatch(action, &mut app);
        let after_down = offsets(&app);
        assert_eq!(after_down.0, before.0, "parent scrollback must stay put");
        assert!(
            after_down.1 > after_up.1,
            "PageDown must move the visible child scrollback"
        );
    }
    #[test]
    fn ctrl_d_from_scrollback_is_half_page_down_not_quit() {
        let mut app = test_app_with_agent();
        pin_non_vscode_registry(&mut app);
        let outcome = app.handle_input(&ctrl_d());
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::HalfPageDown)
        ));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn ctrl_d_double_press_quits_from_prompt() {
        let mut app = test_app_with_agent();
        pin_non_vscode_registry(&mut app);
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().active_pane = crate::views::agent::ActivePane::Prompt;
        let outcome = app.handle_input(&ctrl_d());
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "first Ctrl+D should set pending quit, got: {outcome:?}",
        );
        assert!(app.pending_action.is_some());
        assert_eq!(app.pending_action.as_ref().unwrap().label, Some("quit"));
        let outcome = app.handle_input(&ctrl_d());
        assert!(matches!(outcome, InputOutcome::Action(Action::Quit)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn ctrl_d_in_vscode_quits_from_scrollback() {
        let mut app = test_app_with_agent();
        let mut actions = crate::actions::default_actions(false);
        for def in actions.iter_mut() {
            if def.id == ActionId::Quit {
                def.default_key = key!('d', CONTROL);
                def.alt_keys = vec![];
            }
            if def.id == ActionId::HalfPageDown {
                def.default_key = key!('D');
            }
        }
        app.registry = ActionRegistry::new(actions);
        let outcome = app.handle_input(&ctrl_d());
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "first Ctrl+D should set pending quit, got: {outcome:?}",
        );
        assert!(app.pending_action.is_some());
        assert_eq!(app.pending_action.as_ref().unwrap().label, Some("quit"));
        let outcome = app.handle_input(&ctrl_d());
        assert!(matches!(outcome, InputOutcome::Action(Action::Quit)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn ctrl_q_sets_pending_action() {
        let mut app = test_app_with_agent();
        let outcome = app.handle_input(&ctrl_q());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(app.pending_action.is_some());
        assert_eq!(app.pending_action.as_ref().unwrap().label, Some("quit"));
    }
    #[test]
    fn ctrl_q_double_press_quits() {
        let mut app = test_app_with_agent();
        let _ = app.handle_input(&ctrl_q());
        assert!(app.pending_action.is_some());
        let outcome = app.handle_input(&ctrl_q());
        assert!(matches!(outcome, InputOutcome::Action(Action::Quit)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn different_key_clears_pending() {
        crate::appearance::cache::set_simple_mode(false);
        let mut app = test_app_with_agent();
        if let ActiveView::Agent(id) = app.active_view
            && let Some(agent) = app.agents.get_mut(&id)
        {
            agent.vim_mode = true;
        }
        let _ = app.handle_input(&ctrl_q());
        assert!(app.pending_action.is_some());
        let outcome = app.handle_input(&key_event(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(app.pending_action.is_none());
        assert!(matches!(outcome, InputOutcome::Action(Action::SelectNext)));
    }
    #[test]
    fn ctrl_q_then_ctrl_d_does_not_confirm() {
        let mut app = test_app_with_agent();
        pin_non_vscode_registry(&mut app);
        let _ = app.handle_input(&ctrl_q());
        assert!(app.pending_action.is_some());
        let outcome = app.handle_input(&ctrl_d());
        assert!(app.pending_action.is_none());
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::HalfPageDown)
        ));
    }
    fn ctrl_n() -> Event {
        key_event(KeyCode::Char('n'), KeyModifiers::CONTROL)
    }
    #[test]
    fn ctrl_n_sets_pending_new_session() {
        let mut app = test_app_with_agent();
        let outcome = app.handle_input(&ctrl_n());
        assert!(matches!(outcome, InputOutcome::Changed));
        let pending = app.pending_action.as_ref().expect("pending action");
        assert_eq!(pending.label, Some("new"));
    }
    #[test]
    fn second_ctrl_n_opens_new_session_mode_question_when_mode_is_ask() {
        let mut app = test_app_with_agent();
        app.new_session_worktree_mode = WorktreeMode::Ask;
        let _ = app.handle_input(&ctrl_n());
        let outcome = app.handle_input(&ctrl_n());
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::ChooseNewSessionMode)
        ));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn second_ctrl_n_respects_never_worktree_mode() {
        let mut app = test_app_with_agent();
        app.new_session_worktree_mode = WorktreeMode::Never;
        let _ = app.handle_input(&ctrl_n());
        let outcome = app.handle_input(&ctrl_n());
        assert!(matches!(outcome, InputOutcome::Action(Action::NewSession)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn second_ctrl_n_respects_always_worktree_mode() {
        let mut app = test_app_with_agent();
        app.new_session_worktree_mode = WorktreeMode::Always;
        let _ = app.handle_input(&ctrl_n());
        let outcome = app.handle_input(&ctrl_n());
        assert!(matches!(outcome, InputOutcome::Action(Action::NewSession)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn ctrl_c_running_cancels_turn() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Action(Action::CancelTurn)));
    }
    #[test]
    fn ctrl_c_cancelling_escalates_to_quit_pending() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnCancelling;
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(app.pending_action.is_some());
        assert_eq!(app.pending_action.as_ref().unwrap().label, Some("quit"));
    }
    fn assert_pending_quit(app: &AppView) {
        let pending = app
            .pending_action
            .as_ref()
            .expect("expected pending action");
        assert_eq!(pending.label, Some("quit"));
        assert!(matches!(pending.action, Action::Quit));
    }
    #[test]
    fn ctrl_c_idle_empty_prompt_sets_pending_quit() {
        crate::appearance::cache::set_simple_mode(true);
        let mut app = test_app_with_agent();
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_pending_quit(&app);
    }
    #[test]
    fn ctrl_c_idle_empty_prompt_focused_sets_pending_quit() {
        crate::appearance::cache::set_simple_mode(true);
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().active_pane = crate::views::agent::ActivePane::Prompt;
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_pending_quit(&app);
    }
    #[test]
    fn ctrl_c_double_press_idle_quits() {
        crate::appearance::cache::set_simple_mode(true);
        let mut app = test_app_with_agent();
        let _ = app.handle_input(&ctrl_c());
        assert!(app.pending_action.is_some());
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Action(Action::Quit)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn ctrl_c_consumed_by_cancel_does_not_set_pending() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Action(Action::CancelTurn)));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn ctrl_c_consumed_by_text_clear_does_not_set_pending() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent.prompt.textarea.set_text("some text");
        let outcome = app.handle_input(&ctrl_c());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn ctrl_c_then_other_key_resets_pending() {
        crate::appearance::cache::set_simple_mode(true);
        let mut app = test_app_with_agent();
        let _ = app.handle_input(&ctrl_c());
        assert!(app.pending_action.is_some());
        let _ = app.handle_input(&key_event(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn ctrl_c_idle_prompt_with_text_clears_text() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent.prompt.textarea.set_text("draft prompt");
        assert!(agent.session.state.is_idle());
        let outcome = app.handle_input(&ctrl_c());
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Ctrl+C with text in idle prompt must Change (clear text), got: {outcome:?}",
        );
        assert!(
            app.agents[&id].prompt.textarea.text().is_empty(),
            "Ctrl+C must clear prompt text when agent is idle; got: {:?}",
            app.agents[&id].prompt.textarea.text(),
        );
    }
    #[test]
    fn ctrl_c_running_prompt_with_text_clears_text_and_preserves_turn() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent.prompt.textarea.set_text("draft prompt");
        let outcome = app.handle_input(&ctrl_c());
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Ctrl+C with text in a running prompt must clear the text, got: {outcome:?}",
        );
        assert!(
            app.agents[&id].prompt.textarea.text().is_empty(),
            "Ctrl+C must clear prompt text first; got: {:?}",
            app.agents[&id].prompt.textarea.text(),
        );
        assert!(
            app.agents[&id].session.state.is_turn_running(),
            "First Ctrl+C must NOT cancel the turn while a draft was present",
        );
        let outcome = app.handle_input(&ctrl_c());
        assert!(
            matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "Second Ctrl+C on empty running prompt must CancelTurn, got: {outcome:?}",
        );
    }
    #[test]
    fn esc_from_prompt_pane_running_turn_is_swallowed() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "1× Esc while running must swallow (not cancel), got {outcome:?}"
        );
        assert!(app.pending_action.is_none());
        assert!(app.agents[&id].cancel_trigger_hint.is_none());
        assert!(app.agents[&id].session.state.is_turn_running());
    }
    #[test]
    fn esc_from_prompt_pane_running_turn_with_draft_is_swallowed_not_clear() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent.prompt.textarea.set_text("draft while streaming");
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "mid-turn Esc with draft must swallow, got {outcome:?}"
        );
        assert!(app.pending_action.is_none());
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "Esc must not cancel mid-turn"
        );
        assert_eq!(
            app.agents[&id].prompt.textarea.text(),
            "draft while streaming",
            "mid-turn Esc must not clear the draft or arm idle clear"
        );
        assert!(app.agents[&id].session.state.is_turn_running());
    }
    #[test]
    fn esc_from_scrollback_pane_running_turn_is_swallowed() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.active_pane = crate::views::agent::ActivePane::Scrollback;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "1× Esc from scrollback while running must swallow, got {outcome:?}"
        );
        assert!(app.pending_action.is_none());
        assert!(app.agents[&id].cancel_trigger_hint.is_none());
        assert!(app.agents[&id].session.state.is_turn_running());
    }
    #[test]
    fn esc_while_cancelling_retries_cancel() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnCancelling;
        agent.active_pane = crate::views::agent::ActivePane::Scrollback;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "Esc while cancelling must retry CancelTurn, got {outcome:?}"
        );
        assert!(app.pending_action.is_none());
        assert_eq!(
            app.agents[&id].cancel_trigger_hint,
            Some(crate::app::actions::CancelTrigger::Esc)
        );
    }
    #[test]
    fn idle_non_empty_double_esc_clears_prompt() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent.prompt.textarea.set_text("draft to clear");
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        let pending = app.pending_action.as_ref().expect("arm clear");
        assert_eq!(pending.label, Some("clear"));
        assert!(matches!(pending.action, Action::ClearPrompt));
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Action(Action::ClearPrompt)));
        assert!(app.pending_action.is_none());
        let effects = crate::app::dispatch::dispatch(Action::ClearPrompt, &mut app);
        assert!(effects.is_empty());
        assert!(app.agents[&id].prompt.textarea.text().is_empty());
        assert_eq!(
            app.agents[&id]
                .session
                .prompt_history
                .first()
                .map(String::as_str),
            Some("draft to clear")
        );
    }
    #[test]
    fn idle_empty_with_messages_double_esc_opens_rewind_silent() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::user_prompt(
                "earlier",
            ));
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        let pending = app.pending_action.as_ref().expect("arm rewind");
        assert!(
            pending.label.is_none(),
            "first Esc for rewind must be silent"
        );
        assert!(matches!(pending.action, Action::RewindShowPicker));
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::RewindShowPicker)
        ));
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn idle_empty_no_messages_esc_is_swallowed() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        assert!(agent.scrollback.is_empty());
        assert!(agent.prompt.textarea.text().is_empty());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "idle empty with no messages must swallow Esc (not FocusScrollback), got {outcome:?}"
        );
        assert!(app.pending_action.is_none());
        assert_eq!(
            app.agents[&id].active_pane,
            crate::views::agent::ActivePane::Prompt
        );
    }
    #[test]
    fn mouse_send_retires_armed_clear_so_next_esc_swallows() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::views::agent::ActivePane::Prompt;
            agent.prompt.textarea.set_text("draft to clear");
        }
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        let pending = app.pending_action.as_ref().expect("arm clear");
        assert!(matches!(pending.action, Action::ClearPrompt));
        let _ =
            crate::app::dispatch::dispatch(Action::SendPrompt("draft to clear".into()), &mut app);
        assert!(
            app.pending_action.is_none(),
            "submit must retire the stale ClearPrompt arm",
        );
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Esc after a mouse-send must swallow mid-turn, got {outcome:?}",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::ClearPrompt)),
            "the retired ClearPrompt arm must not fire",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "Esc must not cancel mid-turn",
        );
        assert!(app.agents[&id].cancel_trigger_hint.is_none());
        assert!(app.pending_action.is_none());
    }
    /// Arm an idle-Esc `ClearPrompt`, submit via `text`-carrying `action` (a
    /// turn-start path with no intervening key), assert the arm was retired, then
    /// with the turn running assert the next Esc swallows (never the stale clear).
    fn assert_submit_path_retires_clear_arm(action: Action) {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::views::agent::ActivePane::Prompt;
            agent.prompt.textarea.set_text("draft to clear");
        }
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(matches!(
            app.pending_action.as_ref().expect("arm clear").action,
            Action::ClearPrompt
        ));
        let _ = crate::app::dispatch::dispatch(action, &mut app);
        assert!(
            app.pending_action.is_none(),
            "every submit path (inner funnel) must retire the stale ClearPrompt arm",
        );
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Esc after a non-keyed submit must swallow mid-turn, got {outcome:?}",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "Esc must not cancel mid-turn",
        );
        assert!(app.agents[&id].cancel_trigger_hint.is_none());
        assert!(app.pending_action.is_none());
    }
    #[test]
    fn submit_follow_up_retires_armed_clear_so_next_esc_swallows() {
        assert_submit_path_retires_clear_arm(Action::SubmitFollowUp("follow up".into()));
    }
    #[test]
    fn slash_preserving_send_retires_armed_clear_so_next_esc_swallows() {
        assert_submit_path_retires_clear_arm(Action::SendSlashCommandPreservingDraft(
            "/compact".into(),
        ));
    }
    #[test]
    fn stale_idle_clear_arm_never_fires_on_busy_agent() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::views::agent::ActivePane::Prompt;
            agent.prompt.textarea.set_text("draft to clear");
        }
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(matches!(
            app.pending_action.as_ref().expect("arm clear").action,
            Action::ClearPrompt
        ));
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Esc on a busy agent must swallow, not fire the stale clear arm, got {outcome:?}",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::ClearPrompt)),
            "the stale ClearPrompt arm must not fire on a running turn",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "Esc must not cancel mid-turn",
        );
        assert!(app.agents[&id].cancel_trigger_hint.is_none());
        assert!(
            app.pending_action.is_none(),
            "the stale arm must be dropped"
        );
    }
    #[test]
    fn stale_idle_rewind_arm_never_fires_on_busy_agent() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::views::agent::ActivePane::Prompt;
            agent
                .scrollback
                .push_block(crate::scrollback::block::RenderBlock::user_prompt(
                    "earlier",
                ));
        }
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(matches!(
            app.pending_action.as_ref().expect("arm rewind").action,
            Action::RewindShowPicker
        ));
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Esc on a busy agent must swallow, not fire the stale rewind arm, got {outcome:?}",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "Esc must not cancel mid-turn",
        );
        assert!(
            app.pending_action.is_none(),
            "the stale arm must be dropped"
        );
    }
    #[test]
    fn esc_consumed_by_policy_disarms_esc_d_combo() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::views::agent::ActivePane::Prompt;
            assert!(agent.scrollback.is_empty());
            assert!(agent.prompt.textarea.text().is_empty());
        }
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            app.agents[&id].esc_pressed_at.is_none(),
            "idle-empty swallow Esc must disarm the Esc→d combo",
        );
        let mut app = test_app_with_agent();
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.active_pane = crate::views::agent::ActivePane::Prompt;
        }
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            app.agents[&id].esc_pressed_at.is_none(),
            "mid-turn swallow Esc must disarm the Esc→d combo",
        );
    }
    #[test]
    fn idle_non_empty_esc_ttl_expiry_re_arms_without_clearing() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent.prompt.textarea.set_text("still here");
        let _ = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        if let Some(p) = app.pending_action.as_mut() {
            p.expires_at = std::time::Instant::now() - std::time::Duration::from_millis(1);
        }
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(
            app.agents[&id].prompt.textarea.text(),
            "still here",
            "expired first Esc must not clear"
        );
        let pending = app.pending_action.as_ref().expect("re-arm clear");
        assert_eq!(pending.label, Some("clear"));
    }
    #[test]
    fn idle_images_only_double_esc_arms_clear() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent
            .prompt
            .images
            .push(crate::prompt_images::from_clipboard_data(
                &crate::clipboard::ImageData {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                },
            ));
        assert!(agent.prompt.textarea.text().is_empty());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        let pending = app.pending_action.as_ref().expect("arm clear for images");
        assert!(matches!(pending.action, Action::ClearPrompt));
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Action(Action::ClearPrompt)));
        assert!(app.pending_action.is_none());
        let effects = crate::app::dispatch::dispatch(Action::ClearPrompt, &mut app);
        assert!(effects.is_empty());
        assert!(
            app.agents[&id].prompt.images.is_empty(),
            "second Esc must clear the image chips"
        );
        assert!(
            app.agents[&id].session.prompt_history.is_empty(),
            "an images-only (empty-text) clear records nothing in prompt history"
        );
    }
    /// Scrollback-pane double-Esc, idle + empty prompt + messages: first Esc
    /// arms `RewindShowPicker` silently, second within the TTL opens the
    /// picker. Driven per scrollback nav mode because the routing differs —
    /// vim resolves through `lookup_with_mode(vim=true)`, non-vim adds the
    /// bare-letter forward-to-prompt fallback — and neither may consume Esc.
    fn assert_scrollback_double_esc_opens_rewind(vim: bool) {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.vim_mode = vim;
        agent.set_input_mode(if vim {
            crate::views::agent::InputMode::Vim
        } else {
            crate::views::agent::InputMode::Simple
        });
        agent.active_pane = crate::views::agent::ActivePane::Scrollback;
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::user_prompt(
                "earlier",
            ));
        assert!(agent.prompt.textarea.text().is_empty());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "vim={vim}: first scrollback Esc must arm silently, got {outcome:?}"
        );
        let pending = app
            .pending_action
            .as_ref()
            .expect("scrollback-pane idle Esc must arm rewind");
        assert!(
            pending.label.is_none(),
            "vim={vim}: first Esc for rewind must be silent"
        );
        assert!(matches!(pending.action, Action::RewindShowPicker));
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::RewindShowPicker)),
            "vim={vim}: second Esc from scrollback must open the rewind picker, got {outcome:?}"
        );
        assert!(app.pending_action.is_none());
    }
    /// Non-vim (simple) scrollback nav: double-Esc from scrollback opens rewind.
    #[test]
    fn idle_scrollback_pane_double_esc_opens_rewind() {
        assert_scrollback_double_esc_opens_rewind(false);
    }
    /// Vim scrollback nav consumes no plain Esc, so the same flow must work.
    #[test]
    fn idle_scrollback_pane_double_esc_opens_rewind_vim_mode() {
        assert_scrollback_double_esc_opens_rewind(true);
    }
    /// From the SCROLLBACK pane an idle Esc with a draft in the (unfocused)
    /// composer arms NOTHING and leaves the draft intact: clear is skipped by
    /// the prompt-pane gate, and rewind is skipped by the global
    /// empty-composer gate even with turns present — never clear or
    /// rewind-stash a draft the reader has scrolled past. The Esc is
    /// swallowed (no pending, no global quit/back-out).
    #[test]
    fn idle_scrollback_pane_esc_with_draft_and_messages_swallows() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Scrollback;
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::user_prompt(
                "earlier",
            ));
        agent
            .prompt
            .textarea
            .set_text("draft while reading scrollback");
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            app.pending_action.is_none(),
            "scrollback-pane Esc with a draft must arm neither clear nor rewind"
        );
        assert_eq!(
            app.agents[&id].prompt.textarea.text(),
            "draft while reading scrollback",
            "scrollback-pane Esc must leave the composer draft intact"
        );
    }
    /// A pending needs-input overlay blocks the scrollback rewind arm: the
    /// overlay intercepts exempt the scrollback pane, so its Esc reaches the
    /// policy — which must swallow rather than arm a picker that would
    /// key-starve the pending overlay. The overlay must survive the Esc.
    #[test]
    fn idle_scrollback_pane_esc_with_pending_input_overlay_does_not_arm_rewind() {
        type OverlayInstaller = (&'static str, fn(&mut AgentView));
        let installers: [OverlayInstaller; 2] = [
            ("cancel_turn_view", |a| {
                a.cancel_turn_view = Some(crate::views::modal::CancelTurnViewState {
                    active_idx: 0,
                    running_count: 1,
                });
            }),
            ("question_view", |a| {
                let stashed = a.prompt.stash();
                a.question_view = Some(crate::views::question_view::QuestionViewState::new(
                    "call-q".into(),
                    vec![],
                    stashed,
                ));
            }),
        ];
        for (name, install) in installers {
            let mut app = test_app_with_agent();
            let id = super::super::agent::AgentId(0);
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::views::agent::ActivePane::Scrollback;
            agent
                .scrollback
                .push_block(crate::scrollback::block::RenderBlock::user_prompt(
                    "earlier",
                ));
            assert!(agent.prompt.textarea.text().is_empty());
            install(agent);
            assert!(
                !agent.no_input_overlay_pending(),
                "{name}: fixture must have a pending needs-input overlay"
            );
            let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
            assert!(
                matches!(outcome, InputOutcome::Changed),
                "{name}: scrollback Esc under a pending overlay must swallow, got {outcome:?}"
            );
            assert!(
                app.pending_action.is_none(),
                "{name}: must not arm rewind under a pending needs-input overlay"
            );
            assert!(
                !app.agents[&id].no_input_overlay_pending(),
                "{name}: the pending overlay must survive the swallowed Esc"
            );
        }
    }
    /// A latent Bash/Remember/Feedback composer mode blocks the scrollback
    /// rewind arm — a rewind restore must not drop conversation text into a
    /// still-armed `!` composer. The Esc must swallow WITHOUT exiting the
    /// mode: mode exit stays a prompt-pane (step 0e) affordance.
    #[test]
    fn idle_scrollback_pane_esc_in_bash_mode_does_not_arm_rewind() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Scrollback;
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::user_prompt(
                "earlier",
            ));
        assert!(agent.prompt.textarea.text().is_empty());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "scrollback Esc with a latent bash composer must swallow, got {outcome:?}"
        );
        assert!(
            app.pending_action.is_none(),
            "must not arm rewind while the composer is in bash mode"
        );
        assert_eq!(
            app.agents[&id].prompt_input_mode,
            crate::app::agent_view::PromptInputMode::Bash,
            "scrollback Esc must not exit the composer mode either"
        );
    }
    /// An active prompt history search blocks the scrollback rewind arm — the
    /// step 0b intercept is prompt-pane-only, so a scrollback Esc reaches the
    /// policy while the search overlay is open and must swallow rather than
    /// stack a rewind arm under it. The search must survive the Esc.
    #[test]
    fn idle_scrollback_pane_esc_with_history_search_does_not_arm_rewind() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::user_prompt(
                "earlier",
            ));
        assert!(agent.prompt.textarea.text().is_empty());
        let history = agent.combined_prompt_history();
        let current_text = agent.prompt.text().to_string();
        agent
            .prompt
            .history_search
            .activate(&history, &current_text);
        agent.active_pane = crate::views::agent::ActivePane::Scrollback;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "scrollback Esc with an open history search must swallow, got {outcome:?}"
        );
        assert!(
            app.pending_action.is_none(),
            "must not arm rewind while history search is open"
        );
        assert!(
            app.agents[&id].prompt.history_search.is_active(),
            "scrollback Esc must not dismiss the search either"
        );
    }
    #[test]
    fn running_slash_dropdown_esc_dismisses_not_cancel() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent.prompt.set_text("/he");
        agent.prompt.refresh_slash(&agent.session.models);
        assert!(
            agent.prompt.slash_open(),
            "precondition: slash dropdown open"
        );
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(app.pending_action.is_none());
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "slash Esc must steal, not cancel"
        );
        assert!(app.agents[&id].session.state.is_turn_running());
        assert!(!app.agents[&id].prompt.slash_open());
    }
    #[test]
    fn running_bash_mode_empty_esc_exits_mode_not_cancel() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        assert!(agent.prompt.textarea.text().is_empty());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "empty bash Esc exits mode, does not cancel while running"
        );
        assert_eq!(
            app.agents[&id].prompt_input_mode,
            crate::app::agent_view::PromptInputMode::Normal
        );
        assert!(app.agents[&id].session.state.is_turn_running());
    }
    #[test]
    fn tab_from_prompt_still_focuses_scrollback() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        let outcome = app.handle_input(&key_event(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::FocusScrollback)
        ));
    }
    #[test]
    fn prompt_focused_printable_chars_still_go_to_textarea() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        let _ = app.handle_input(&key_event(KeyCode::Char('a'), KeyModifiers::NONE));
        let agent = app.agents.get(&id).unwrap();
        assert_eq!(agent.prompt.textarea.text(), "a");
    }
    #[test]
    fn prompt_focused_question_mark_with_shift_still_goes_to_textarea() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::views::agent::ActivePane::Prompt;
        let outcome = app.handle_input(&key_event(KeyCode::Char('?'), KeyModifiers::SHIFT));
        assert!(
            !matches!(outcome, InputOutcome::Changed if app.agents.get(& id).unwrap()
            .active_modal.is_some()),
            "?+SHIFT must not open the command palette when typing in the prompt; got {outcome:?}",
        );
        let agent = app.agents.get(&id).unwrap();
        assert!(
            agent.active_modal.is_none(),
            "?+SHIFT must not open any modal in the prompt",
        );
        assert!(
            agent.prompt.textarea.text().contains('?'),
            "?+SHIFT must reach the textarea as `?`; got {:?}",
            agent.prompt.textarea.text(),
        );
    }
    #[test]
    fn prompt_focused_bare_text_chars_promote_no_action() {
        for ch in ['p', 'b', '/', '?', '1', '5', 'm', 'o', 'c', 'h'] {
            let mut app = test_app_with_agent();
            let id = super::super::agent::AgentId(0);
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::views::agent::ActivePane::Prompt;
            let _ = app.handle_input(&key_event(KeyCode::Char(ch), KeyModifiers::NONE));
            let agent = app.agents.get(&id).unwrap();
            assert!(
                agent.active_modal.is_none(),
                "bare `{ch}` must not open any modal",
            );
            assert!(
                agent.prompt.textarea.text().contains(ch),
                "bare `{ch}` must reach the textarea; got {:?}",
                agent.prompt.textarea.text(),
            );
            let mut app = test_app_with_agent();
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::views::agent::ActivePane::Prompt;
            let _ = app.handle_input(&key_event(KeyCode::Char(ch), KeyModifiers::SHIFT));
            let agent = app.agents.get(&id).unwrap();
            assert!(
                agent.active_modal.is_none(),
                "shift+`{ch}` must not open any modal",
            );
        }
    }
    #[test]
    fn welcome_pending_l_triggers_login() {
        let mut app = test_app();
        app.auth_state = AuthState::Pending { error: None };
        app.welcome_prompt_focused = false;
        let outcome = app.handle_input(&key_event(KeyCode::Char('l'), KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Action(Action::Login)));
    }
    #[test]
    fn welcome_pending_enter_triggers_login() {
        let mut app = test_app();
        app.auth_state = AuthState::Pending { error: None };
        app.welcome_prompt_focused = false;
        let outcome = app.handle_input(&key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Action(Action::Login)));
    }
    #[test]
    fn welcome_pending_n_is_unchanged() {
        let mut app = test_app();
        app.auth_state = AuthState::Pending { error: None };
        app.welcome_prompt_focused = false;
        let outcome = app.handle_input(&key_event(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Unchanged));
    }
    #[test]
    fn welcome_done_n_starts_session() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        let outcome = app.handle_input(&key_event(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(
            outcome,
            InputOutcome::ActionThenForward(Action::NewSession)
        ));
    }
    #[test]
    fn welcome_done_ctrl_w_opens_new_worktree_dialog() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        app.cwd_has_git_ancestor = true;
        let outcome = app.handle_input(&key_event(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::OpenNewWorktreeDialog)
        ));
    }
    #[test]
    fn welcome_ctrl_v_creates_normal_session() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        app.welcome_prompt_focused = true;
        let outcome = app.handle_input(&key_event(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(matches!(
            outcome,
            InputOutcome::ActionThenForward(Action::NewSession)
        ));
    }
    #[test]
    fn welcome_cmd_v_creates_normal_session() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        app.welcome_prompt_focused = true;
        let outcome = app.handle_input(&key_event(KeyCode::Char('v'), KeyModifiers::SUPER));
        assert!(matches!(
            outcome,
            InputOutcome::ActionThenForward(Action::NewSession)
        ));
    }
    #[test]
    fn worktree_dialog_enter_creates_worktree_session() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        app.new_worktree_dialog = Some(NewWorktreeDialogState::new());
        let outcome = app.handle_input(&key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::NewWorktreeSession {
                load_session_id: None,
                label: None,
                git_ref: None,
            })
        ));
        assert!(app.new_worktree_dialog.is_none());
    }
    #[test]
    fn worktree_dialog_modified_enter_is_ignored() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        app.new_worktree_dialog = Some(NewWorktreeDialogState::new());
        let outcome = app.handle_input(&key_event(KeyCode::Enter, KeyModifiers::CONTROL));
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert!(app.new_worktree_dialog.is_some());
        let outcome = app.handle_input(&key_event(KeyCode::Char('w'), KeyModifiers::SHIFT));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.new_worktree_dialog.as_ref().unwrap().label(), "W");
    }
    #[test]
    fn worktree_dialog_enter_threads_label() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        app.new_worktree_dialog = Some(NewWorktreeDialogState::new());
        for c in "wolves".chars() {
            app.handle_input(&key_event(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let outcome = app.handle_input(&key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::NewWorktreeSession {
            load_session_id : None, label : Some(ref l), git_ref : None, }) if l ==
            "wolves")
        );
        assert!(app.new_worktree_dialog.is_none());
    }
    #[test]
    fn worktree_dialog_esc_closes() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        app.new_worktree_dialog = Some(NewWorktreeDialogState::new());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(app.new_worktree_dialog.is_none());
    }
    #[test]
    fn worktree_dialog_typing_updates_label() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        app.new_worktree_dialog = Some(NewWorktreeDialogState::new());
        let outcome = app.handle_input(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.new_worktree_dialog.as_ref().unwrap().label(), "h");
        let outcome = app.handle_input(&key_event(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.new_worktree_dialog.as_ref().unwrap().label(), "hi");
    }
    #[test]
    fn worktree_dialog_backspace_removes_char() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        let mut dialog = NewWorktreeDialogState::new();
        dialog.set_label("test");
        app.new_worktree_dialog = Some(dialog);
        let outcome = app.handle_input(&key_event(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.new_worktree_dialog.as_ref().unwrap().label(), "tes");
    }
    #[test]
    fn worktree_dialog_enforces_byte_cap_for_typing_and_middle_paste() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        let mut dialog = NewWorktreeDialogState::new();
        dialog.set_label("a".repeat(98));
        let _ = dialog.set_cursor_byte(1);
        app.new_worktree_dialog = Some(dialog);
        let outcome = app.handle_input(&Event::Paste("éx".to_owned()));
        assert!(matches!(outcome, InputOutcome::Changed));
        let dialog = app.new_worktree_dialog.as_ref().unwrap();
        assert_eq!(dialog.label().len(), 100);
        assert_eq!(&dialog.label()[1.."aé".len()], "é");
        let outcome = app.handle_input(&key_event(KeyCode::Char('中'), KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.new_worktree_dialog.as_ref().unwrap().label().len(), 100);
        let mut dialog = NewWorktreeDialogState::new();
        dialog.set_label("a".repeat(99));
        app.new_worktree_dialog = Some(dialog);
        let _ = app.handle_input(&key_event(KeyCode::Char('é'), KeyModifiers::NONE));
        assert_eq!(app.new_worktree_dialog.as_ref().unwrap().label().len(), 99);
    }
    #[test]
    fn worktree_dialog_paste_is_scoped_away_from_welcome_prompt() {
        let mut app = test_app();
        app.auth_state = AuthState::Done;
        let mut dialog = NewWorktreeDialogState::new();
        dialog.set_label("ab");
        let _ = dialog.set_cursor_byte(1);
        app.new_worktree_dialog = Some(dialog);
        let outcome = app.handle_input(&Event::Paste("中".to_owned()));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.new_worktree_dialog.as_ref().unwrap().label(), "a中b");
        assert!(app.welcome_prompt.text().is_empty());
    }
    #[test]
    fn authenticating_loopback_esc_quits() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Action(Action::Quit)));
    }
    #[test]
    fn authenticating_command_esc_quits() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Command,
        };
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Action(Action::Quit)));
    }
    /// Regression (user report): 'q' must type into the auth-code input,
    /// not quit.
    #[test]
    fn authenticating_loopback_q_types_into_code_input() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        let outcome = app.handle_input(&key_event(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "typing 'q' must edit the auth code input, got {outcome:?}"
        );
        assert_eq!(app.auth_code_input.text(), "q");
    }
    /// Users reflex-type the displayed device code; bare 'q' must not abort.
    #[test]
    fn authenticating_device_and_command_q_does_not_quit() {
        for mode in [AuthMode::Device, AuthMode::Command] {
            let mut app = test_app();
            app.auth_state = AuthState::Authenticating {
                request_seq: 1,
                handle: None,
                auth_url: None,
                mode,
            };
            let outcome = app.handle_input(&key_event(KeyCode::Char('q'), KeyModifiers::NONE));
            assert!(
                matches!(outcome, InputOutcome::Unchanged),
                "bare 'q' must not quit during {mode:?} auth, got {outcome:?}"
            );
        }
    }
    /// Advertised cancel keys must survive the bare-'q' removal.
    #[test]
    fn authenticating_advertised_cancel_keys_still_quit() {
        for mode in [AuthMode::Loopback, AuthMode::Device, AuthMode::Command] {
            for (code, mods) in [
                (KeyCode::Char('q'), KeyModifiers::CONTROL),
                (KeyCode::Char('c'), KeyModifiers::CONTROL),
                (KeyCode::Esc, KeyModifiers::NONE),
            ] {
                let mut app = test_app();
                app.auth_state = AuthState::Authenticating {
                    request_seq: 1,
                    handle: None,
                    auth_url: None,
                    mode,
                };
                let outcome = app.handle_input(&key_event(code, mods));
                assert!(
                    matches!(outcome, InputOutcome::Action(Action::Quit)),
                    "{code:?}+{mods:?} must still quit during {mode:?} auth, got {outcome:?}"
                );
            }
        }
    }
    #[test]
    fn authenticating_loopback_char_mutates_input() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        let outcome = app.handle_input(&key_event(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.auth_code_input.text(), "a");
    }
    #[test]
    fn authenticating_loopback_readline_control_chords_are_ignored() {
        for code in [KeyCode::Char('u'), KeyCode::Char('d')] {
            let mut app = test_app();
            app.auth_state = AuthState::Authenticating {
                request_seq: 1,
                handle: None,
                auth_url: None,
                mode: AuthMode::Loopback,
            };
            app.auth_code_input.set_text("token");
            let outcome = app.handle_input(&key_event(code, KeyModifiers::CONTROL));
            assert!(matches!(outcome, InputOutcome::Changed));
            assert_eq!(app.auth_code_input.text(), "token");
        }
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn authenticating_loopback_altgr_char_mutates_input() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        let outcome = app.handle_input(&key_event(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.auth_code_input.text(), "@");
    }
    #[test]
    fn authenticating_loopback_backspace_removes_char() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        app.auth_code_input.set_text("ab");
        let outcome = app.handle_input(&key_event(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.auth_code_input.text(), "a");
    }
    #[test]
    fn authenticating_loopback_paste_appends_text() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        app.auth_code_input.set_text("tok");
        let outcome = app.handle_input(&Event::Paste("en_value".to_string()));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.auth_code_input.text(), "token_value");
    }
    #[test]
    fn authenticating_loopback_cursor_edit_and_paste_stay_scoped() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        app.auth_code_input.set_text("ab");
        let _ = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        let _ = app.handle_input(&Event::Paste("中\r\n".to_owned()));
        assert_eq!(app.auth_code_input.text(), "a中b");
        assert!(app.welcome_prompt.text().is_empty());
        let _ = app.handle_input(&key_event(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(app.auth_code_input.text(), "a中");
    }
    #[test]
    fn authenticating_loopback_uses_canonical_super_v_paste() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        crate::clipboard::set_clipboard_probe_hook(
            crate::clipboard::ClipboardProbeHook::no_raster(Some("secret\r\n")),
        );
        let outcome = app.handle_input(&key_event(KeyCode::Char('v'), KeyModifiers::SUPER));
        crate::clipboard::clear_clipboard_probe_hook();
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.auth_code_input.text(), "secret");
        assert!(app.welcome_prompt.text().is_empty());
    }
    #[test]
    fn authenticating_loopback_enter_empty_is_noop() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        app.auth_code_input.set_text("   ");
        let outcome = app.handle_input(&key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Unchanged));
    }
    #[test]
    fn authenticating_loopback_enter_with_content_submits() {
        let mut app = test_app();
        app.auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Loopback,
        };
        app.auth_code_input.set_text(" token123 ");
        let outcome = app.handle_input(&key_event(KeyCode::Enter, KeyModifiers::NONE));
        match outcome {
            InputOutcome::Action(Action::SubmitAuthCode(code)) => {
                assert_eq!(code, "token123");
            }
            other => panic!("expected SubmitAuthCode, got {:?}", other),
        }
    }
    #[test]
    fn moved_with_button_held_promotes_pending_scrollback_drag() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(crate::scrollback::RenderBlock::agent_message(
                "hello world this should wrap across lines",
            ));
        agent.scrollback.prepare_layout(40, 10);
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 40, 20));
        let _ = agent.draw(
            ratatui::layout::Rect::new(0, 0, 40, 20),
            &mut buf,
            &ActionRegistry::defaults(),
            &mut crate::scrollback::render::ScratchBuffer::new(),
            None,
            false,
            0,
            &[],
            &std::collections::BTreeSet::new(),
            None,
            &BundleState::default(),
            false,
            &mut Vec::new(),
            false,
            false,
            None,
        );
        let hit = agent
            .last_scrollback_selection_model
            .ranges
            .first()
            .and_then(|range| range.lines.first())
            .cloned()
            .expect("expected selectable markdown line");
        let down_col = hit.screen_x + hit.selectable_cols.start;
        let row = hit.screen_y.min(9);
        let move_col = down_col + 1;
        let down = left_mouse(MouseEventKind::Down(MouseButton::Left), down_col, row);
        let moved = left_mouse(MouseEventKind::Moved, move_col, row);
        assert!(matches!(app.handle_input(&down), InputOutcome::Changed));
        let agent = app.agents.get(&id).unwrap();
        assert!(agent.pending_text_drag.is_some());
        assert!(agent.drag_selection.is_none());
        assert!(matches!(app.handle_input(&moved), InputOutcome::Changed));
        let agent = app.agents.get(&id).unwrap();
        assert!(agent.pending_text_drag.is_some());
        assert!(agent.drag_selection.is_some());
    }
    #[test]
    fn moved_without_button_does_not_promote_pending_scrollback_drag() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(crate::scrollback::RenderBlock::agent_message(
                "hello world this should wrap across lines",
            ));
        agent.scrollback.prepare_layout(40, 10);
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 40, 20));
        let _ = agent.draw(
            ratatui::layout::Rect::new(0, 0, 40, 20),
            &mut buf,
            &ActionRegistry::defaults(),
            &mut crate::scrollback::render::ScratchBuffer::new(),
            None,
            false,
            0,
            &[],
            &std::collections::BTreeSet::new(),
            None,
            &BundleState::default(),
            false,
            &mut Vec::new(),
            false,
            false,
            None,
        );
        let hit = agent
            .last_scrollback_selection_model
            .ranges
            .first()
            .and_then(|range| range.lines.first())
            .cloned()
            .expect("expected selectable markdown line");
        let down_col = hit.screen_x + hit.selectable_cols.start;
        let row = hit.screen_y.min(9);
        let move_col = down_col + 1;
        let down = left_mouse(MouseEventKind::Down(MouseButton::Left), down_col, row);
        let up = left_mouse(MouseEventKind::Up(MouseButton::Left), down_col, row);
        let moved = left_mouse(MouseEventKind::Moved, move_col, row);
        assert!(matches!(app.handle_input(&down), InputOutcome::Changed));
        assert!(matches!(app.handle_input(&up), InputOutcome::Changed));
        let agent = app.agents.get(&id).unwrap();
        assert!(!agent.left_mouse_down);
        assert!(agent.pending_text_drag.is_none());
        assert!(agent.drag_selection.is_none());
        let outcome = app.handle_input(&moved);
        assert!(matches!(
            outcome,
            InputOutcome::Unchanged | InputOutcome::Changed
        ));
        let agent = app.agents.get(&id).unwrap();
        assert!(agent.drag_selection.is_none());
    }
    #[test]
    fn scrollback_click_still_selects_entry_on_mouse_up() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(crate::scrollback::RenderBlock::agent_message("hello world"));
        agent.scrollback.prepare_layout(40, 10);
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 40, 20));
        let _ = agent.draw(
            ratatui::layout::Rect::new(0, 0, 40, 20),
            &mut buf,
            &ActionRegistry::defaults(),
            &mut crate::scrollback::render::ScratchBuffer::new(),
            None,
            false,
            0,
            &[],
            &std::collections::BTreeSet::new(),
            None,
            &BundleState::default(),
            false,
            &mut Vec::new(),
            false,
            false,
            None,
        );
        let hit = agent
            .last_scrollback_selection_model
            .ranges
            .first()
            .and_then(|range| range.lines.first())
            .cloned()
            .expect("expected selectable markdown line");
        let click_col = hit.screen_x + hit.selectable_cols.start;
        let click_row = hit.screen_y;
        let down = left_mouse(
            MouseEventKind::Down(MouseButton::Left),
            click_col,
            click_row,
        );
        let up = left_mouse(MouseEventKind::Up(MouseButton::Left), click_col, click_row);
        assert!(matches!(app.handle_input(&down), InputOutcome::Changed));
        assert!(matches!(app.handle_input(&up), InputOutcome::Changed));
        let selected_after = app.agents.get(&id).unwrap().scrollback.selected();
        assert_eq!(selected_after, Some(0));
    }
    fn make_test_warning() -> crate::startup::StartupWarning {
        crate::startup::StartupWarning {
            severity: crate::startup::WarningSeverity::Warning,
            message: "test warning".to_string(),
            action: Some("run /terminal-setup".to_string()),
        }
    }
    #[test]
    fn welcome_d_starts_session_when_no_warnings() {
        let mut app = test_app();
        app.welcome_prompt_focused = true;
        app.startup_warnings = vec![];
        let outcome = app.handle_input(&key_event(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::ActionThenForward(Action::NewSession)),
            "Expected NewSession when no warnings, got {outcome:?}"
        );
    }
    #[test]
    fn welcome_other_char_starts_session_even_with_warnings() {
        let mut app = test_app();
        app.welcome_prompt_focused = true;
        app.startup_warnings = vec![make_test_warning()];
        let outcome = app.handle_input(&key_event(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::ActionThenForward(Action::NewSession)),
            "Expected NewSession for 'a' even with warnings, got {outcome:?}"
        );
    }
    #[test]
    fn merge_escapes_both_some_concatenates() {
        let result = AppView::merge_escapes(
            Some("notif".into()),
            Some(crate::terminal::overlay::PostFlush::plain("render".into())),
        );
        assert_eq!(
            result.as_ref().map(|post| post.as_str()),
            Some("notifrender")
        );
    }
    #[test]
    fn merge_escapes_only_notif() {
        let result = AppView::merge_escapes(Some("notif".into()), None);
        assert_eq!(result.as_ref().map(|post| post.as_str()), Some("notif"));
    }
    #[test]
    fn merge_escapes_only_render() {
        let result = AppView::merge_escapes(
            None,
            Some(crate::terminal::overlay::PostFlush::plain("render".into())),
        );
        assert_eq!(result.as_ref().map(|post| post.as_str()), Some("render"));
    }
    #[test]
    fn merge_escapes_both_none() {
        let result = AppView::merge_escapes(None, None);
        assert!(result.is_none());
    }
    #[test]
    fn dashboard_stale_clears_modal_placement_under_kitty() {
        use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};
        let _g = set_protocol_for_test(GraphicsProtocol::Kitty);
        let mut app = test_app_with_agent();
        let clears = AppView::dashboard_stale_image_clears(&mut app.agents, None);
        let expected = crate::terminal::overlay::clear_kitty().into_string();
        assert_eq!(
            clears.as_ref().map(|post| post.as_str()),
            Some(expected.as_str()),
            "the modal/preview placement (id 1) is deleted every dashboard frame"
        );
    }
    #[test]
    fn dashboard_stale_clears_none_without_graphics_protocol() {
        use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};
        let _g = set_protocol_for_test(GraphicsProtocol::None);
        let mut app = test_app_with_agent();
        let clears = AppView::dashboard_stale_image_clears(&mut app.agents, None);
        assert!(clears.is_none(), "text-only terminals never get escapes");
    }
    #[test]
    fn dashboard_stale_clears_drain_undrawn_agent_inline_media() {
        use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};
        let _g = set_protocol_for_test(GraphicsProtocol::Kitty);
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent
                .inline_media_ids
                .insert(std::path::PathBuf::from("/tmp/media.png"), 5);
            agent.inline_media_active = true;
        }
        let clears = AppView::dashboard_stale_image_clears(&mut app.agents, None)
            .expect("kitty sweep always emits");
        assert!(
            clears
                .as_str()
                .contains(&crate::terminal::image::clear_kitty_image(5)),
            "deletes the undrawn agent's inline placement: {clears:?}"
        );
        let again = AppView::dashboard_stale_image_clears(&mut app.agents, None);
        let expected = crate::terminal::overlay::clear_kitty().into_string();
        assert_eq!(
            again.as_ref().map(|post| post.as_str()),
            Some(expected.as_str()),
        );
    }
    #[test]
    fn dashboard_stale_clears_skip_attached_popup_agent() {
        use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};
        let _g = set_protocol_for_test(GraphicsProtocol::Kitty);
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent
                .inline_media_ids
                .insert(std::path::PathBuf::from("/tmp/media.png"), 5);
            agent.inline_media_active = true;
        }
        crate::terminal::overlay::reset_owner();
        let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        let _ = crate::terminal::overlay::static_image(&png, 20, 10, 0, 0, 7)
            .unwrap()
            .commit();
        for _ in 0..2 {
            assert!(AppView::dashboard_stale_image_clears(&mut app.agents, Some(id)).is_none());
            let popup = crate::terminal::overlay::static_image(&png, 20, 10, 0, 0, 7).unwrap();
            assert!(!popup.as_str().contains("a=t"));
            let _ = popup.commit();
        }
        let agent = app.agents.get(&id).unwrap();
        assert!(agent.inline_media_active, "drawn agent state is untouched");
        assert_eq!(agent.inline_media_ids.len(), 1);
    }
    #[test]
    fn dashboard_too_small_popup_clears_shared_overlay_slot() {
        use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};
        let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);
        crate::terminal::overlay::reset_owner();
        let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        let _ = crate::terminal::overlay::static_image(&png, 20, 10, 0, 0, 8)
            .unwrap()
            .commit();
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        let mut dashboard = crate::views::dashboard::DashboardState::new();
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 40, 4));
        let (_, _, drawn) = crate::views::dashboard::render_popup_overlay(
            &mut buf,
            ratatui::layout::Rect::new(0, 0, 40, 4),
            &crate::theme::Theme::current(),
            "Tiny",
            &mut dashboard,
            |_inner, _buf| panic!("tiny popup must not draw the agent"),
        );
        assert!(!drawn);
        let clear =
            AppView::dashboard_stale_image_clears(&mut app.agents, drawn.then_some(id)).unwrap();
        assert!(clear.as_str().contains("a=d"));
        assert!(
            !crate::terminal::overlay::static_image(&png, 20, 10, 0, 0, 8)
                .unwrap()
                .as_str()
                .contains("a=t")
        );
        clear.write_to(&mut Vec::new()).unwrap();
        assert!(
            crate::terminal::overlay::static_image(&png, 20, 10, 0, 0, 8)
                .unwrap()
                .as_str()
                .contains("a=t")
        );
    }
    #[test]
    fn worktree_mode_round_trip_ask() {
        let mode = WorktreeMode::from_config_str("ask");
        assert_eq!(mode, WorktreeMode::Ask);
        assert_eq!(mode.as_config_str(), "ask");
    }
    #[test]
    fn worktree_mode_round_trip_always() {
        let mode = WorktreeMode::from_config_str("always");
        assert_eq!(mode, WorktreeMode::Always);
        assert_eq!(mode.as_config_str(), "always");
    }
    #[test]
    fn worktree_mode_round_trip_never() {
        let mode = WorktreeMode::from_config_str("never");
        assert_eq!(mode, WorktreeMode::Never);
        assert_eq!(mode.as_config_str(), "never");
    }
    #[test]
    fn worktree_mode_unrecognised_falls_back_to_never() {
        assert_eq!(WorktreeMode::from_config_str("alway"), WorktreeMode::Never);
        assert_eq!(WorktreeMode::from_config_str(""), WorktreeMode::Never);
        assert_eq!(WorktreeMode::from_config_str("ALWAYS"), WorktreeMode::Never);
    }
    /// Helper: parse a TOML string and return the document.
    fn parse_toml(s: &str) -> toml_edit::DocumentMut {
        s.parse::<toml_edit::DocumentMut>().expect("valid TOML")
    }
    #[test]
    fn resolve_from_hints_no_keys_returns_defaults() {
        let doc = parse_toml("");
        let (new_s, fork) = WorktreeMode::resolve_from_hints(doc.get("hints"));
        assert_eq!(new_s, WorktreeMode::Never);
        assert_eq!(fork, WorktreeMode::Ask);
    }
    #[test]
    fn resolve_from_hints_legacy_key_sets_both() {
        let doc = parse_toml("[hints]\nworktree_mode = \"always\"\n");
        let (new_s, fork) = WorktreeMode::resolve_from_hints(doc.get("hints"));
        assert_eq!(new_s, WorktreeMode::Always);
        assert_eq!(fork, WorktreeMode::Always);
    }
    #[test]
    fn resolve_from_hints_per_command_keys_override_legacy() {
        let doc = parse_toml(
            "[hints]\n\
             worktree_mode = \"always\"\n\
             new_session_worktree_mode = \"never\"\n\
             fork_worktree_mode = \"ask\"\n",
        );
        let (new_s, fork) = WorktreeMode::resolve_from_hints(doc.get("hints"));
        assert_eq!(new_s, WorktreeMode::Never);
        assert_eq!(fork, WorktreeMode::Ask);
    }
    #[test]
    fn resolve_from_hints_only_per_command_keys() {
        let doc = parse_toml(
            "[hints]\n\
             new_session_worktree_mode = \"ask\"\n\
             fork_worktree_mode = \"never\"\n",
        );
        let (new_s, fork) = WorktreeMode::resolve_from_hints(doc.get("hints"));
        assert_eq!(new_s, WorktreeMode::Ask);
        assert_eq!(fork, WorktreeMode::Never);
    }
    #[test]
    fn resolve_from_hints_one_per_command_key_other_falls_back_to_legacy() {
        let doc = parse_toml(
            "[hints]\n\
             worktree_mode = \"always\"\n\
             fork_worktree_mode = \"never\"\n",
        );
        let (new_s, fork) = WorktreeMode::resolve_from_hints(doc.get("hints"));
        assert_eq!(new_s, WorktreeMode::Always);
        assert_eq!(fork, WorktreeMode::Never);
    }
    #[test]
    fn resolve_from_hints_one_per_command_key_other_falls_back_to_default() {
        let doc = parse_toml("[hints]\nnew_session_worktree_mode = \"always\"\n");
        let (new_s, fork) = WorktreeMode::resolve_from_hints(doc.get("hints"));
        assert_eq!(new_s, WorktreeMode::Always);
        assert_eq!(fork, WorktreeMode::Ask);
    }
    fn scroll_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }
    #[test]
    fn scroll_event_stashes_origin_for_residual_flush() {
        let mut app = test_app();
        assert!(app.last_scroll_pos.is_none());
        let _ = app.handle_input(&scroll_event(MouseEventKind::ScrollDown, 42, 17));
        assert_eq!(app.last_scroll_pos, Some((42, 17)));
        let _ = app.handle_input(&scroll_event(MouseEventKind::ScrollUp, 7, 3));
        assert_eq!(app.last_scroll_pos, Some((7, 3)));
    }
    #[test]
    fn scroll_event_does_not_stash_when_blocking_modal_open() {
        let mut app = test_app();
        app.new_worktree_dialog = Some(NewWorktreeDialogState::new());
        assert!(app.is_scroll_blocking_modal_open());
        let _ = app.handle_input(&scroll_event(MouseEventKind::ScrollDown, 42, 17));
        assert!(
            app.last_scroll_pos.is_none(),
            "scroll events must be ignored while a scroll-blocking modal is open",
        );
    }
    #[test]
    fn welcome_doc_viewer_is_scroll_blocking_and_wheel_scrolls_content() {
        let mut app = test_app();
        app.active_view = ActiveView::Welcome;
        app.welcome_doc_viewer = Some(crate::views::modal::ActiveModal::DocViewer {
            title: "Release Notes".into(),
            content: "line\n".repeat(80),
            scroll: 0,
            window: crate::views::modal_window::ModalWindowState::new(),
            cached_lines: None,
            previous_palette: None,
            standalone: true,
        });
        assert!(
            app.is_scroll_blocking_modal_open(),
            "welcome release-notes overlay must block background scroll",
        );
        let outcome = app.handle_input(&scroll_event(MouseEventKind::ScrollDown, 40, 12));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "wheel must be handled by the doc viewer",
        );
        assert!(
            app.last_scroll_pos.is_none(),
            "wheel must not reach the background scroll path while release notes are open",
        );
        let scroll = match app.welcome_doc_viewer.as_ref() {
            Some(crate::views::modal::ActiveModal::DocViewer { scroll, .. }) => *scroll,
            _ => panic!("expected DocViewer"),
        };
        assert!(scroll > 0, "wheel must advance doc scroll, got {scroll}");
    }
    #[test]
    fn dashboard_shortcuts_modal_is_scroll_blocking() {
        let mut app = test_app();
        app.active_view = ActiveView::AgentDashboard;
        let mut d = crate::views::dashboard::DashboardState::new();
        let entries = Vec::new();
        let state = crate::views::shortcuts_help::build_initial_picker_state(&entries);
        d.shortcuts_modal = Some(Box::new(crate::views::dashboard::ShortcutsModalState {
            entries,
            state,
            window: Default::default(),
            filter_active: false,
            collapsed_sections: Default::default(),
            expanded_ids: std::collections::HashSet::new(),
            mode: crate::views::shortcuts_help::ShortcutsHelpMode::Browse,
        }));
        app.dashboard = Some(d);
        assert!(
            app.is_scroll_blocking_modal_open(),
            "an open dashboard cheatsheet must block background scroll",
        );
        let _ = app.handle_input(&scroll_event(MouseEventKind::ScrollDown, 42, 17));
        assert!(
            app.last_scroll_pos.is_none(),
            "wheel must not reach the background scroll path while the cheatsheet is open",
        );
    }
    /// Ctrl+C on the session-less dashboard arms the quit confirmation
    /// (like the agent view) and a second press confirms. Regression for
    /// "Ctrl+C/D/Q do nothing on the dashboard prompt".
    #[test]
    fn ctrl_c_on_dashboard_arms_then_confirms_quit() {
        let mut app = test_app();
        app.active_view = ActiveView::AgentDashboard;
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        let outcome = app.handle_input(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            app.pending_action.is_some(),
            "Ctrl+C on the dashboard must arm a pending quit confirmation"
        );
        let outcome = app.handle_input(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::Quit)),
            "second Ctrl+C must quit, got {outcome:?}"
        );
    }
    /// Ctrl+Q on the dashboard arms quit via the global `When::Always`
    /// lookup (it's not bound to `When::DashboardFocused`).
    #[test]
    fn ctrl_q_on_dashboard_arms_quit() {
        let mut app = test_app();
        app.active_view = ActiveView::AgentDashboard;
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        let outcome = app.handle_input(&key_event(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            app.pending_action.is_some(),
            "Ctrl+Q on the dashboard must arm a pending quit confirmation"
        );
    }
    /// Ctrl+Space on the dashboard resolves to `VoiceToggle` via the global
    /// `When::Always` fallthrough — the dispatch input ignores the chord, so it
    /// falls through to `handle_global_action`. (The event loop intercepts
    /// Ctrl+Space before this for hold-to-talk/toggle when voice is enabled;
    /// this registry route is the cheatsheet/command-palette fallback.)
    #[test]
    fn ctrl_space_on_dashboard_routes_to_voice_toggle() {
        let mut app = test_app();
        pin_non_vscode_registry(&mut app);
        app.active_view = ActiveView::AgentDashboard;
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        let outcome = app.handle_input(&key_event(KeyCode::Char(' '), KeyModifiers::CONTROL));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::VoiceToggle)),
            "Ctrl+Space on the dashboard must route to VoiceToggle, got {outcome:?}"
        );
    }
    /// Esc while voice is recording on the dashboard must STOP voice (route to
    /// `VoiceToggle`) rather than fall into the dashboard's Esc cascade
    /// (clear filter / unfocus / deselect / exit).
    #[test]
    fn esc_on_dashboard_while_listening_stops_voice() {
        let mut app = test_app();
        pin_non_vscode_registry(&mut app);
        app.active_view = ActiveView::AgentDashboard;
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        app.voice_state = VoiceState::Recording {
            hold: false,
            target: VoiceTarget::DashboardDispatch,
            interim: None,
        };
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::VoiceToggle)),
            "Esc while recording on the dashboard must stop voice, got {outcome:?}"
        );
    }
    /// Esc on the dashboard while NOT recording must keep its normal cascade
    /// behaviour (here: not a `VoiceToggle`).
    #[test]
    fn esc_on_dashboard_not_listening_does_not_toggle_voice() {
        let mut app = test_app();
        pin_non_vscode_registry(&mut app);
        app.active_view = ActiveView::AgentDashboard;
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        app.voice_state = VoiceState::Idle;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::VoiceToggle)),
            "Esc must not toggle voice when not recording, got {outcome:?}"
        );
    }
    /// Esc with a voice cold-start still queued (pipeline spawning, mic not yet
    /// open) must cancel it so the event loop doesn't open the mic after the user
    /// backed out — even though `voice_listening` is still false.
    #[test]
    fn esc_cancels_pending_voice_cold_start() {
        let mut app = test_app();
        pin_non_vscode_registry(&mut app);
        app.active_view = ActiveView::AgentDashboard;
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        app.voice_state = VoiceState::ColdStart {
            hold: false,
            target: VoiceTarget::DashboardDispatch,
        };
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            !app.voice_state.pending_cold_start(),
            "Esc must cancel the queued cold-start"
        );
        assert!(
            app.voice_recording_target().is_none(),
            "target dropped on cancel"
        );
    }
    /// The dictation overlay must only render on the surface that owns the bound
    /// target. After an explicit stop the interim is kept (`Stopping`) for a
    /// trailing final, so navigating away must not flash it on the wrong box.
    #[test]
    fn voice_overlay_bound_to_target_surface() {
        let id = super::super::agent::AgentId(0);
        let mut app = test_app();
        app.voice_state = VoiceState::Stopping {
            target: VoiceTarget::Agent(id),
            interim: Some("partial".into()),
        };
        app.active_view = ActiveView::Agent(id);
        assert!(
            app.voice_target_on_active_surface(),
            "overlay shows on the agent that owns the dictation"
        );
        app.active_view = ActiveView::AgentDashboard;
        assert!(
            !app.voice_target_on_active_surface(),
            "overlay hidden once the user navigates off the target surface"
        );
    }
    /// Entering a session from the dashboard sets `active_view = Agent(id)` but
    /// leaves `attached_agent = Some(id)` as a return breadcrumb. The agent is
    /// fullscreen, so dictation into its prompt must stay on-surface and the
    /// bind-enforcer must not auto-stop it. Regression: recording bar missing
    /// after clicking into a session. (Popup-over-dashboard suppression is
    /// covered by `dispatch::tests::voice_suppressed_while_dashboard_popup_open`.)
    #[test]
    fn voice_target_on_agent_entered_from_dashboard() {
        let id = super::super::agent::AgentId(0);
        let mut app = test_app();
        app.voice_state = VoiceState::Recording {
            hold: false,
            target: VoiceTarget::Agent(id),
            interim: None,
        };
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        app.dashboard.as_mut().unwrap().attached_agent = Some(id);
        assert!(app.voice_target_on_active_surface());
        app.enforce_voice_session_bound();
        assert!(
            app.voice_listening(),
            "entering a session from the dashboard must not auto-stop the mic"
        );
    }
    /// Attach a popup overlay onto a freshly-built `test_app_with_agent`
    /// and return the attached agent id. Convenience for the
    /// popup-handle-input tests.
    /// NOTE: this helper bypasses
    /// `dispatch_dashboard_attach`. The action-dispatcher path
    /// (which sets `attached_agent` via `Action::DashboardAttach(...)`)
    /// is pinned by tests in `dispatch.rs`
    /// (`dashboard_attach_top_level_opens_popup_overlay`,
    /// `dashboard_attach_subagent_opens_popup_with_subagent`).
    /// `attach_popup` exists so the `handle_input`/`dispatch_scroll`
    /// tests in this file can stand up a popup'd state in two lines
    /// without re-exercising the dispatcher each time.
    fn attach_popup(app: &mut AppView) -> super::super::agent::AgentId {
        app.active_view = ActiveView::AgentDashboard;
        let id = super::super::agent::AgentId(0);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
            d.selected = Some(crate::views::dashboard::DashboardRowId::TopLevel(id));
        }
        id
    }
    /// Esc keystroke closes the popup at the
    /// `AppView::handle_input` layer (not the dispatch layer the
    /// other tests exercise).
    #[test]
    fn handle_input_esc_closes_popup_overlay() {
        let mut app = test_app_with_agent();
        let id = attach_popup(&mut app);
        assert_eq!(app.dashboard.as_ref().unwrap().attached_agent, Some(id));
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.dashboard.as_ref().unwrap().attached_agent, None);
    }
    /// Esc on a neutral overlay (scrollback focused, no modals
    /// or viewers, no text selection or link highlight, no
    /// question / goal / rewind / permission overlays) closes the
    /// dashboard session overlay — mirrors the `q` shortcut and
    /// gives users a single-key back-out from agent detail to
    /// the dashboard. The Esc cascade is preserved for non-
    /// neutral states: see `overlay_esc_passes_through_when_*`.
    #[test]
    fn overlay_esc_exits_when_agent_is_neutral() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc on a neutral overlay must request DashboardOverlayExit, got {outcome:?}",
        );
    }
    /// In a dashboard overlay an empty, Normal-mode prompt-focused Esc backs
    /// out to the dashboard (attach lands on Prompt
    /// focus, so without this Esc would silently arm the agent's rewind policy
    /// instead of returning to the list).
    #[test]
    fn overlay_esc_backs_out_when_empty_normal_prompt() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
        assert!(agent.prompt.text().is_empty());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "empty prompt Esc in an overlay must back out to the dashboard, got {outcome:?}",
        );
        assert!(app.pending_action.is_none());
    }
    /// Overlay + open `/btw` + empty Normal prompt: Esc dismisses `/btw`, not
    /// dashboard back-out; a follow-up Esc still exits when the guard holds.
    #[test]
    fn overlay_esc_dismisses_btw_before_dashboard_backout() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
        agent.btw_state = Some(crate::views::btw_overlay::BtwOverlayState::done(
            "side question".into(),
            "side answer".into(),
        ));
        let first = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(first, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc with open /btw must not exit the overlay, got {first:?}",
        );
        assert!(app.agents.get(&id).unwrap().btw_state.is_none());
        let second = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(second, InputOutcome::Action(Action::DashboardOverlayExit)),
            "second Esc with no /btw must back out to the dashboard, got {second:?}",
        );
    }
    /// Regression: in an overlay, a bare Esc while a turn is
    /// RUNNING must swallow (matching full-screen), NOT detach to the dashboard
    /// and NOT cancel. The empty-prompt back-out is idle-gated, so Esc falls
    /// through to `try_handle_esc_policy` → mid-turn swallow.
    #[test]
    fn overlay_esc_running_turn_empty_prompt_swallows_not_backout() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
        agent.session.state = AgentState::TurnRunning;
        assert!(agent.prompt.text().is_empty());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "running-turn overlay Esc (empty prompt) must swallow, not detach/cancel, got {outcome:?}",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "Esc must not cancel mid-turn",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc must not detach mid-turn",
        );
        assert!(app.agents[&id].cancel_trigger_hint.is_none());
        assert!(app.pending_action.is_none());
    }
    /// Regression: in an overlay, a bare Esc from the
    /// (neutral) bare-scrollback pane while a turn is RUNNING must swallow, NOT
    /// detach — the neutral back-out is idle-gated. The fixture is otherwise
    /// neutral (so the gate, not a missing-neutral, is what suppresses detach).
    #[test]
    fn overlay_esc_running_turn_scrollback_swallows_not_backout() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Scrollback;
        agent.session.state = AgentState::TurnRunning;
        assert!(agent.is_bare_scrollback() && agent.no_input_overlay_pending());
        assert!(agent.no_esc_consumer_pending());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "running-turn overlay Esc (scrollback) must swallow, not detach/cancel, got {outcome:?}",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "Esc must not cancel mid-turn",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc must not detach mid-turn",
        );
        assert!(app.agents[&id].cancel_trigger_hint.is_none());
    }
    /// Overlay + TurnCancelling: Esc retries cancel (does not detach).
    #[test]
    fn overlay_esc_cancelling_scrollback_retries_cancel_not_backout() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Scrollback;
        agent.session.state = AgentState::TurnCancelling;
        assert!(agent.is_bare_scrollback() && agent.no_input_overlay_pending());
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
            "cancelling overlay Esc must retry CancelTurn, got {outcome:?}",
        );
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc must not detach while cancelling",
        );
    }
    /// Counterpart to the back-out: a NON-EMPTY draft Esc in an overlay must
    /// pass through to the agent's policy (arms "press again to clear"), never
    /// back out — so the user doesn't lose a draft by reaching for the dashboard.
    #[test]
    fn overlay_esc_with_draft_arms_clear_not_backout() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
        agent.prompt.textarea.set_text("draft in overlay");
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "a drafted overlay prompt Esc must NOT back out, got {outcome:?}",
        );
        let pending = app.pending_action.as_ref().expect("clear arm");
        assert_eq!(pending.label, Some("clear"));
    }
    /// A Bash/Remember/Feedback empty prompt keeps Esc as its mode-exit even in
    /// an overlay — the back-out is gated to `PromptInputMode::Normal`, so the
    /// special-mode Esc is not stolen as a dashboard back-out.
    #[test]
    fn overlay_esc_in_bash_mode_exits_mode_not_backout() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "empty bash-mode Esc in an overlay must exit the mode, not back out, got {outcome:?}",
        );
        assert_eq!(
            app.agents[&id].prompt_input_mode,
            crate::app::agent_view::PromptInputMode::Normal,
            "Esc must have exited bash mode",
        );
    }
    /// A live highlighted link consumes Esc (the agent's scrollback
    /// handler clears it). We mustn't pre-empt that — the overlay
    /// closes only after the per-pane Esc work is drained.
    #[test]
    fn overlay_esc_passes_through_when_link_highlight_present() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        app.agents.get_mut(&id).unwrap().highlighted_link_idx = Some(0);
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc with a highlighted link must clear the highlight first, got {outcome:?}",
        );
    }
    /// Build an app with a neutral agent attached as the dashboard overlay.
    fn neutral_overlay_app() -> (AppView, super::super::agent::AgentId) {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        app.dashboard = Some(crate::views::dashboard::DashboardState::new());
        if let Some(d) = app.dashboard.as_mut() {
            d.attached_agent = Some(id);
        }
        (app, id)
    }
    /// With a pending input overlay, neither `q` nor `Esc` is consumed as a
    /// dashboard-overlay exit — both fall through to the agent (the scrollback
    /// handler, not the overlay handler).
    #[test]
    fn overlay_q_esc_do_not_exit_while_input_overlay_pending() {
        let installers: [fn(&mut AgentView); 2] = [
            |a| {
                a.cancel_turn_view = Some(crate::views::modal::CancelTurnViewState {
                    active_idx: 0,
                    running_count: 1,
                });
            },
            |a| {
                let request = crate::views::plan_approval_view::ExitPlanModeExtRequest {
                    session_id: "s".into(),
                    tool_call_id: "c".into(),
                    plan_content: Some("p".into()),
                };
                let stashed = crate::views::prompt_widget::StashedPrompt {
                    text: String::new(),
                    cursor: 0,
                    images: Vec::new(),
                    chip_elements: Vec::new(),
                    image_counter: 0,
                    image_undo_stash: Vec::new(),
                };
                let view = crate::views::plan_approval_view::PlanApprovalViewState::new(
                    request,
                    stashed,
                    tokio::sync::oneshot::channel().0,
                );
                a.plan_approval_view = Some(view);
            },
        ];
        for key in [KeyCode::Char('q'), KeyCode::Esc] {
            let (mut app, id) = neutral_overlay_app();
            assert!(
                app.agents.get(&id).unwrap().is_bare_scrollback()
                    && app.agents.get(&id).unwrap().no_input_overlay_pending(),
                "fixture must start neutral",
            );
            let bare = app.handle_input(&key_event(key, KeyModifiers::NONE));
            assert!(
                matches!(bare, InputOutcome::Action(Action::DashboardOverlayExit)),
                "neutral {key:?} must exit, got {bare:?}",
            );
            for &install in &installers {
                let (mut app, id) = neutral_overlay_app();
                install(app.agents.get_mut(&id).unwrap());
                let outcome = app.handle_input(&key_event(key, KeyModifiers::NONE));
                assert!(
                    !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
                    "{key:?} with an input overlay pending must fall through, got {outcome:?}",
                );
            }
        }
    }
    /// Left arrow on an empty, prompt-focused overlay backs out to the
    /// dashboard — the mirror of the dashboard's Right-arrow "open
    /// detail". Requires the prompt to be focused with an empty buffer.
    #[test]
    fn overlay_left_arrow_empty_prompt_exits_to_dashboard() {
        let (mut app, id) = neutral_overlay_app();
        app.agents.get_mut(&id).unwrap().active_pane = crate::app::agent_view::AgentPane::Prompt;
        let outcome = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left on an empty focused prompt must exit the overlay, got {outcome:?}",
        );
    }
    /// `/gboom` is opened from an empty prompt — the exact state where the
    /// dashboard overlay steals Left/Esc as back-out. Both must reach the game.
    #[test]
    fn overlay_gboom_owns_left_and_esc() {
        let (mut app, id) = neutral_overlay_app();
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
            agent.gboom = Some(crate::gboom::GboomState::new());
        }
        let left = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !matches!(left, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left with /gboom open must reach the game, got {left:?}",
        );
        assert!(
            app.agents.get(&id).unwrap().gboom.is_some(),
            "Left must not close /gboom",
        );
        let esc = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(esc, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc with /gboom open must close the game, not the overlay, got {esc:?}",
        );
        assert!(
            app.agents.get(&id).unwrap().gboom.is_none(),
            "Esc should close /gboom",
        );
    }
    /// Left arrow with an active prompt history search (empty draft) is NOT
    /// an overlay exit — the search owns the key (Left moves its query caret),
    /// so it must reach the agent rather than backing out to the dashboard.
    #[test]
    fn overlay_left_arrow_history_search_active_does_not_exit() {
        let (mut app, id) = neutral_overlay_app();
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
            agent.prompt.history_search.activate(&[], "");
            assert!(
                agent.prompt.text().is_empty(),
                "fixture draft must be empty"
            );
        }
        let outcome = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left with an active history search must reach the agent, got {outcome:?}",
        );
    }
    /// Left arrow with the `@` file-search dropdown open is NOT an overlay exit
    /// — the prompt widget owns picker nav (Right drills in, Up/Down move the
    /// selection), so the key must reach the agent rather than backing out. In
    /// production an open dropdown implies a non-empty draft (the `@` token);
    /// we force the decoupled state to isolate the explicit file-search guard.
    #[test]
    fn overlay_left_arrow_file_search_open_does_not_exit() {
        let (mut app, id) = neutral_overlay_app();
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
            let ctx =
                crate::views::file_search::context::detect("@", 1).expect("@-context must parse");
            agent.prompt.file_search.set_test_state(
                ctx,
                vec![xai_grok_workspace::file_system::FuzzyMatchResult {
                    path: nucleo::Utf32String::from("src"),
                    score: 100,
                    indices: Vec::new(),
                    is_dir: true,
                }],
                0,
            );
            assert!(
                agent.prompt.file_search_visible(),
                "fixture must open the @ dropdown",
            );
            assert!(
                agent.prompt.text().is_empty(),
                "fixture keeps the draft empty to isolate the file-search guard",
            );
            assert!(
                !agent.is_empty_focused_prompt(),
                "an open @ dropdown must fail the empty-focused-prompt guard",
            );
        }
        let outcome = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left with the @ dropdown open must reach the agent, got {outcome:?}",
        );
    }
    /// Left arrow with a non-empty prompt draft is NOT an overlay exit —
    /// it falls through to the prompt so it moves the caret within the
    /// text rather than closing the agent detail.
    #[test]
    fn overlay_left_arrow_with_draft_does_not_exit() {
        let (mut app, id) = neutral_overlay_app();
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
            agent.prompt.set_text("draft");
        }
        let outcome = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left with a non-empty prompt must NOT exit the overlay, got {outcome:?}",
        );
    }
    /// Left arrow while the scrollback pane is focused is NOT an overlay
    /// exit — it must reach the agent so the scrollback's `Left=collapse`
    /// binding keeps working (the back-out is prompt-only).
    #[test]
    fn overlay_left_arrow_in_scrollback_does_not_exit() {
        let (mut app, _id) = neutral_overlay_app();
        let outcome = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left in scrollback must reach the agent, got {outcome:?}",
        );
    }
    /// An open modal (extensions modal or `active_modal`) makes
    /// `is_empty_focused_prompt` false even on an empty, prompt-focused
    /// composer, so the modal — not the overlay back-out — owns Esc/Left.
    #[test]
    fn overlay_open_modal_fails_empty_focused_prompt_guard() {
        let (mut app, id) = neutral_overlay_app();
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
        assert!(
            agent.is_empty_focused_prompt(),
            "bare empty prompt must satisfy the guard",
        );
        agent.extensions_modal = Some(crate::views::extensions_modal::ExtensionsModalState::new(
            crate::views::extensions_modal::ExtensionsTab::Plugins,
        ));
        assert!(
            !agent.is_empty_focused_prompt(),
            "an open extensions modal must fail the guard",
        );
        agent.extensions_modal = None;
        agent.active_modal = Some(crate::views::modal::ActiveModal::CommandPalette {
            entries: Vec::new(),
            state: crate::views::picker::PickerState::default(),
            window: crate::views::modal_window::ModalWindowState::new(),
        });
        assert!(
            !agent.is_empty_focused_prompt(),
            "an open active_modal must fail the guard",
        );
        agent.active_modal = None;
        assert!(
            agent.is_empty_focused_prompt(),
            "clearing the modals restores the guard",
        );
    }
    /// With an agent attached (dashboard overlay) and the extensions modal
    /// open on the Prompt pane, Esc/Left must reach the modal rather than
    /// backing out to the dashboard. Esc closes the modal; Left folds /
    /// is consumed by the modal — neither yields `DashboardOverlayExit`.
    #[test]
    fn overlay_modal_open_esc_left_do_not_exit() {
        let (mut app, id) = neutral_overlay_app();
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
            agent.extensions_modal =
                Some(crate::views::extensions_modal::ExtensionsModalState::new(
                    crate::views::extensions_modal::ExtensionsTab::Plugins,
                ));
        }
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc with the extensions modal open must not back out, got {outcome:?}",
        );
        assert!(
            app.agents[&id].extensions_modal.is_none(),
            "Esc must reach the modal handler and close it",
        );
        let (mut app, id) = neutral_overlay_app();
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
            agent.extensions_modal =
                Some(crate::views::extensions_modal::ExtensionsModalState::new(
                    crate::views::extensions_modal::ExtensionsTab::Plugins,
                ));
        }
        let outcome = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left with the extensions modal open must not back out, got {outcome:?}",
        );
        assert!(
            app.agents[&id].extensions_modal.is_some(),
            "Left must reach the modal (fold), keeping it open",
        );
        for code in [KeyCode::Esc, KeyCode::Left] {
            let (mut app, id) = neutral_overlay_app();
            {
                let agent = app.agents.get_mut(&id).unwrap();
                agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
                agent.active_modal = Some(crate::views::modal::ActiveModal::CommandPalette {
                    entries: Vec::new(),
                    state: crate::views::picker::PickerState::default(),
                    window: crate::views::modal_window::ModalWindowState::new(),
                });
            }
            let outcome = app.handle_input(&key_event(code, KeyModifiers::NONE));
            assert!(
                !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
                "{code:?} with active_modal open must not back out, got {outcome:?}",
            );
        }
    }
    /// The graduated plan/Q&A back-out also defers to an open modal: with a
    /// single-question Q&A overlay at its back-out top AND a modal open, both
    /// `overlay_esc_backs_out` and `overlay_left_backs_out` return false (a modal
    /// and a question view can coexist when the ACP handler installs the overlay
    /// without closing the modal).
    #[test]
    fn graduated_back_out_defers_to_open_modal() {
        let (mut app, id) = neutral_overlay_app();
        install_question_overlay(&mut app, id, 1);
        {
            let a = app.agents.get(&id).unwrap();
            assert!(
                a.overlay_esc_backs_out() && a.overlay_left_backs_out(),
                "fixture must be a back-out-top state without a modal",
            );
        }
        app.agents.get_mut(&id).unwrap().extensions_modal =
            Some(crate::views::extensions_modal::ExtensionsModalState::new(
                crate::views::extensions_modal::ExtensionsTab::Plugins,
            ));
        {
            let a = app.agents.get(&id).unwrap();
            assert!(
                !a.overlay_esc_backs_out() && !a.overlay_left_backs_out(),
                "an open extensions modal must suppress the graduated back-out",
            );
        }
        app.agents.get_mut(&id).unwrap().extensions_modal = None;
        app.agents.get_mut(&id).unwrap().active_modal =
            Some(crate::views::modal::ActiveModal::CommandPalette {
                entries: Vec::new(),
                state: crate::views::picker::PickerState::default(),
                window: crate::views::modal_window::ModalWindowState::new(),
            });
        {
            let a = app.agents.get(&id).unwrap();
            assert!(
                !a.overlay_esc_backs_out() && !a.overlay_left_backs_out(),
                "an open active_modal must suppress the graduated back-out",
            );
        }
    }
    /// Install a plan-approval overlay on the agent and put it in the
    /// "focused dashboard overlay, prompt pane" state the graduated
    /// back-out cares about.
    fn install_plan_overlay(app: &mut AppView, id: super::super::agent::AgentId) {
        let a = app.agents.get_mut(&id).unwrap();
        a.in_dashboard_overlay = true;
        a.active_pane = crate::app::agent_view::AgentPane::Prompt;
        let request = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            plan_content: Some("p".into()),
        };
        let stashed = crate::views::prompt_widget::StashedPrompt {
            text: String::new(),
            cursor: 0,
            images: Vec::new(),
            chip_elements: Vec::new(),
            image_counter: 0,
            image_undo_stash: Vec::new(),
        };
        let mut view = crate::views::plan_approval_view::PlanApprovalViewState::new(
            request,
            stashed,
            tokio::sync::oneshot::channel().0,
        );
        view.focus = crate::views::plan_approval_view::PlanApprovalFocus::Prompt;
        a.plan_approval_view = Some(view);
    }
    /// Install a Q&A overlay with `n_questions` single-select questions,
    /// focused in the dashboard overlay's Navigation surface.
    fn install_question_overlay(
        app: &mut AppView,
        id: super::super::agent::AgentId,
        n_questions: usize,
    ) {
        use crate::views::question_view::QuestionViewState;
        use xai_grok_tools::implementations::grok_build::ask_user_question::{
            Question, QuestionOption,
        };
        let questions: Vec<Question> = (0..n_questions)
            .map(|i| Question {
                question: format!("Q{i}?"),
                options: vec![QuestionOption {
                    label: "opt".into(),
                    description: String::new(),
                    preview: None,
                    id: None,
                }],
                multi_select: None,
                id: None,
            })
            .collect();
        let a = app.agents.get_mut(&id).unwrap();
        a.in_dashboard_overlay = true;
        a.active_pane = crate::app::agent_view::AgentPane::Prompt;
        a.question_view = Some(QuestionViewState::new(
            "c".into(),
            questions,
            crate::views::prompt_widget::StashedPrompt::default(),
        ));
    }
    /// Graduated back-out: at the plan feedback top state (empty prompt,
    /// no pending comment) a bare Esc returns to the dashboard, leaving
    /// the plan overlay pending (no approve / reject is sent).
    #[test]
    fn overlay_esc_exits_at_plan_top_state() {
        let (mut app, id) = neutral_overlay_app();
        install_plan_overlay(&mut app, id);
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc at the plan top state must back out, got {outcome:?}",
        );
        assert!(
            app.agents.get(&id).unwrap().plan_approval_view.is_some(),
            "backing out must leave the plan overlay pending (unanswered)",
        );
    }
    /// A typed feedback draft is NOT a top state — Esc keeps its
    /// in-overlay meaning so the draft isn't lost to an accidental exit.
    #[test]
    fn overlay_esc_does_not_exit_with_plan_draft() {
        let (mut app, id) = neutral_overlay_app();
        install_plan_overlay(&mut app, id);
        app.agents.get_mut(&id).unwrap().prompt.set_text("feedback");
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc with a plan feedback draft must NOT back out, got {outcome:?}",
        );
    }
    /// Graduated back-out: in the Q&A Navigation surface with nothing
    /// selected, a bare Esc (whose only job there is to unselect) backs
    /// out to the dashboard instead of dead-ending.
    #[test]
    fn overlay_esc_exits_when_question_nav_unselected() {
        let (mut app, id) = neutral_overlay_app();
        install_question_overlay(&mut app, id, 1);
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc with nothing selected must back out, got {outcome:?}",
        );
        assert!(
            app.agents.get(&id).unwrap().question_view.is_some(),
            "backing out must leave the question overlay pending",
        );
    }
    /// Multi-question Q&A: on question 2+ a bare `Esc` must NOT back out — the
    /// flow isn't at its top, so `Esc` stays in-flow (the question view handles
    /// it) and `Left` can still walk back. Only `active_tab == 0` is the
    /// back-out top.
    #[test]
    fn overlay_esc_does_not_exit_on_later_multi_question() {
        let (mut app, id) = neutral_overlay_app();
        install_question_overlay(&mut app, id, 2);
        app.agents
            .get_mut(&id)
            .unwrap()
            .question_view
            .as_mut()
            .unwrap()
            .next_question();
        assert_eq!(
            app.agents
                .get(&id)
                .unwrap()
                .question_view
                .as_ref()
                .unwrap()
                .active_tab,
            1,
            "fixture must be on the second question",
        );
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc on question 2+ of a multi-question Q&A must stay in-flow, got {outcome:?}",
        );
    }
    /// ...but from question 1 (the top of a multi-question flow) with nothing
    /// selected, a bare `Esc` still backs out, leaving the Q&A pending.
    #[test]
    fn overlay_esc_exits_at_first_multi_question() {
        let (mut app, id) = neutral_overlay_app();
        install_question_overlay(&mut app, id, 2);
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc at question 1 of a multi-question Q&A must back out, got {outcome:?}",
        );
        assert!(
            app.agents.get(&id).unwrap().question_view.is_some(),
            "backing out must leave the question overlay pending",
        );
    }
    /// With an option selected, Esc has something to clear — it must NOT
    /// back out (the first Esc unselects; a second, now-unselected Esc
    /// would exit).
    #[test]
    fn overlay_esc_does_not_exit_when_question_option_selected() {
        let (mut app, id) = neutral_overlay_app();
        install_question_overlay(&mut app, id, 1);
        app.agents
            .get_mut(&id)
            .unwrap()
            .question_view
            .as_mut()
            .unwrap()
            .select_option(0, 0);
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc with a selection must unselect first, not back out, got {outcome:?}",
        );
    }
    /// Left backs out of a single-question Q&A (Left has no prev question
    /// to step to), but with multiple questions Left switches question
    /// and must NOT exit.
    #[test]
    fn overlay_left_exits_single_question_only() {
        let (mut app, id) = neutral_overlay_app();
        install_question_overlay(&mut app, id, 1);
        let single = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            matches!(single, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left in a single-question Q&A must back out, got {single:?}",
        );
        let (mut app, id) = neutral_overlay_app();
        install_question_overlay(&mut app, id, 2);
        let multi = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !matches!(multi, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left in a multi-question Q&A must switch question, not back out, got {multi:?}",
        );
    }
    /// Single-question Q&A back-out is key-specific: `Left` has no in-overlay
    /// behaviour there (only multi-question `Left` switches questions; `Esc`
    /// owns unselect), so a bare `Left` backs out even with an option selected.
    /// The exit is non-destructive — the Q&A and its selection stay pending — so
    /// nothing is lost. (Esc stays graduated, clearing the selection first: see
    /// `overlay_esc_does_not_exit_when_question_option_selected`.)
    #[test]
    fn overlay_left_exits_single_question_with_selection() {
        let (mut app, id) = neutral_overlay_app();
        install_question_overlay(&mut app, id, 1);
        app.agents
            .get_mut(&id)
            .unwrap()
            .question_view
            .as_mut()
            .unwrap()
            .select_option(0, 0);
        assert!(
            app.agents
                .get(&id)
                .unwrap()
                .question_view
                .as_ref()
                .unwrap()
                .active_tab_has_selection(),
            "fixture must start with a live selection",
        );
        let outcome = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        let InputOutcome::Action(action @ Action::DashboardOverlayExit) = outcome else {
            panic!(
                "Left in a single-question Q&A must back out even with a selection, got {outcome:?}",
            );
        };
        let _ = super::super::dispatch::dispatch(action, &mut app);
        let agent = app.agents.get(&id).unwrap();
        assert!(
            agent
                .question_view
                .as_ref()
                .is_some_and(|qv| qv.active_tab_has_selection()),
            "the selection must survive the back-out (Q&A still pending)",
        );
    }
    /// Install a plan-approval overlay showing the plan in the line
    /// viewer (`Preview` focus) — the default shape when the plan has
    /// content (`acp_handler` opens the preview). This is the state the
    /// user reported as stuck: `Esc` / `Left` are dead no-ops in the
    /// plan line viewer.
    fn install_plan_preview_overlay(app: &mut AppView, id: super::super::agent::AgentId) {
        let request = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            plan_content: Some("# Plan\n- step one\n- step two".into()),
        };
        let view = crate::views::plan_approval_view::PlanApprovalViewState::new(
            request,
            crate::views::prompt_widget::StashedPrompt::default(),
            tokio::sync::oneshot::channel().0,
        );
        let a = app.agents.get_mut(&id).unwrap();
        a.in_dashboard_overlay = true;
        a.plan_approval_view = Some(view);
        a.show_plan_preview();
        assert!(
            a.line_viewer.is_some(),
            "fixture must open the plan line viewer",
        );
    }
    /// Regression for the reported bug: in plan approval shown via the
    /// line viewer (the common case), Esc was a dead no-op. It must now
    /// back out to the dashboard, leaving the plan pending (unanswered).
    #[test]
    fn overlay_esc_exits_at_plan_preview() {
        let (mut app, id) = neutral_overlay_app();
        install_plan_preview_overlay(&mut app, id);
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc in the plan line-viewer preview must back out, got {outcome:?}",
        );
        assert!(
            app.agents.get(&id).unwrap().plan_approval_view.is_some(),
            "backing out must leave the plan overlay pending (unanswered)",
        );
    }
    /// Left is likewise a no-op in the plan line viewer (the list pane
    /// ignores it), so it backs out too.
    #[test]
    fn overlay_left_exits_at_plan_preview() {
        let (mut app, id) = neutral_overlay_app();
        install_plan_preview_overlay(&mut app, id);
        let outcome = app.handle_input(&key_event(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Left in the plan line-viewer preview must back out, got {outcome:?}",
        );
    }
    /// Backing out of the plan preview is non-destructive: dispatching the
    /// resulting `DashboardOverlayExit` switches to the dashboard but
    /// leaves BOTH the plan-approval view and its line-viewer preview
    /// intact, so re-opening the agent shows the plan exactly as before.
    #[test]
    fn overlay_exit_from_plan_preview_keeps_preview_intact() {
        let (mut app, id) = neutral_overlay_app();
        install_plan_preview_overlay(&mut app, id);
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        let InputOutcome::Action(action @ Action::DashboardOverlayExit) = outcome else {
            panic!("Esc must yield DashboardOverlayExit, got {outcome:?}");
        };
        let _ = super::super::dispatch::dispatch(action, &mut app);
        assert!(
            matches!(app.active_view, ActiveView::AgentDashboard),
            "exit must land on the dashboard",
        );
        let agent = app.agents.get(&id).unwrap();
        assert!(
            agent.plan_approval_view.is_some(),
            "plan approval must survive the back-out (still pending)",
        );
        assert!(
            agent.line_viewer.is_some(),
            "the plan line-viewer preview must survive the back-out",
        );
    }
    /// Graduated: while a visual selection is active in the plan viewer,
    /// Esc must clear it first (reach the viewer) rather than backing out.
    #[test]
    fn overlay_esc_does_not_exit_plan_preview_in_visual_mode() {
        let (mut app, id) = neutral_overlay_app();
        install_plan_preview_overlay(&mut app, id);
        app.agents
            .get_mut(&id)
            .unwrap()
            .line_viewer
            .as_mut()
            .unwrap()
            .list_state
            .visual_mode = true;
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc with an active visual selection must reach the viewer, got {outcome:?}",
        );
    }
    /// Graduated: while an accepted search matcher is active in the plan
    /// viewer (input bar closed, filter still applied), the first Esc must
    /// clear it (reach the viewer) rather than backing out; only once it's
    /// cleared does Esc exit to the dashboard.
    #[test]
    fn overlay_esc_clears_matcher_before_exiting_plan_preview() {
        use crate::views::list_pane::{ListMatcher, MatchMode, QueryKind};
        let (mut app, id) = neutral_overlay_app();
        install_plan_preview_overlay(&mut app, id);
        app.agents
            .get_mut(&id)
            .unwrap()
            .line_viewer
            .as_mut()
            .unwrap()
            .list_state
            .set_matcher(Some(ListMatcher::new(
                "step",
                QueryKind::Substring,
                MatchMode::Search,
            )));
        let outcome = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !matches!(outcome, InputOutcome::Action(Action::DashboardOverlayExit)),
            "Esc with an accepted matcher must reach the viewer, got {outcome:?}",
        );
        assert!(
            app.agents
                .get(&id)
                .unwrap()
                .line_viewer
                .as_ref()
                .unwrap()
                .list_state
                .matcher()
                .is_none(),
            "the first Esc must clear the accepted search matcher",
        );
        let outcome2 = app.handle_input(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome2, InputOutcome::Action(Action::DashboardOverlayExit)),
            "once the matcher is cleared, Esc must back out, got {outcome2:?}",
        );
    }
    /// Overlay Ctrl+X on an agent with a RUNNING turn — routes to the
    /// agent view's existing cancel behaviour (`Action::CancelTurn`,
    /// same as Ctrl+C) and never arms the close confirm: mashing
    /// Ctrl+X to stop a turn must not be able to close the session.
    #[test]
    fn overlay_ctrl_x_busy_agent_cancels_turn_without_arming() {
        let (mut app, id) = neutral_overlay_app();
        app.agents.get_mut(&id).unwrap().session.state = crate::app::agent::AgentState::TurnRunning;
        for _ in 0..2 {
            let outcome = app.handle_input(&key_event(KeyCode::Char('x'), KeyModifiers::CONTROL));
            assert!(
                matches!(outcome, InputOutcome::Action(Action::CancelTurn)),
                "Ctrl+X on a busy agent must cancel the turn, got {outcome:?}",
            );
            assert!(
                app.pending_action.is_none(),
                "Ctrl+X on a busy agent must not arm the close confirm",
            );
        }
        assert!(
            app.agents.get(&id).unwrap().active_modal.is_none(),
            "Ctrl+X must be intercepted before the agent sees it",
        );
    }
    /// Overlay Ctrl+X on a non-turn busy agent (command in flight,
    /// cancel pending) — `Action::CancelTurn` would no-op for these
    /// states, so the press arms the two-press close instead of
    /// being a dead key.
    #[test]
    fn overlay_ctrl_x_command_or_cancelling_agent_arms_close_confirm() {
        use crate::app::agent::{AgentCommand, AgentState};
        let states = [
            AgentState::CommandRunning {
                command: AgentCommand::Compact,
                started_at: std::time::Instant::now(),
            },
            AgentState::TurnCancelling,
            AgentState::CommandCancelling {
                command: AgentCommand::Compact,
            },
        ];
        for state in states {
            let (mut app, id) = neutral_overlay_app();
            app.agents.get_mut(&id).unwrap().session.state = state.clone();
            let outcome = app.handle_input(&key_event(KeyCode::Char('x'), KeyModifiers::CONTROL));
            assert!(
                matches!(outcome, InputOutcome::Changed),
                "Ctrl+X on a {state:?} agent must arm (not cancel / fire), got {outcome:?}",
            );
            assert!(
                app.pending_action
                    .as_ref()
                    .is_some_and(|p| matches!(p.action, Action::DashboardOverlayStop)),
                "Ctrl+X on a {state:?} agent must arm the close confirm",
            );
        }
    }
    /// Overlay Ctrl+X on an IDLE agent — arms the two-press close
    /// confirm (`pending_action` = `DashboardOverlayStop` so the
    /// shortcuts bar paints "press again to close this session");
    /// there is no turn to cancel.
    #[test]
    fn overlay_ctrl_x_idle_agent_arms_close_confirm() {
        let (mut app, _id) = neutral_overlay_app();
        let outcome = app.handle_input(&key_event(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "first Ctrl+X on an idle agent must arm (not fire), got {outcome:?}",
        );
        let pending = app.pending_action.as_ref().expect("confirm must be armed");
        assert!(
            matches!(pending.action, Action::DashboardOverlayStop),
            "pending action must be the overlay stop",
        );
        assert_eq!(pending.label, Some("close this session"));
        assert!(
            !pending.expired(),
            "the confirm window must still be live right after arming",
        );
        assert!(
            app.pending_effects.is_empty(),
            "no CancelTurn for an idle agent",
        );
    }
    /// Overlay Ctrl+X, second press inside the confirm window — the
    /// pending-action fast path consumes the key and fires
    /// `Action::DashboardOverlayStop` (close + back to dashboard).
    #[test]
    fn overlay_ctrl_x_second_press_fires_overlay_stop() {
        let (mut app, _id) = neutral_overlay_app();
        let _ = app.handle_input(&key_event(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(app.pending_action.is_some(), "first press must arm");
        let outcome = app.handle_input(&key_event(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::DashboardOverlayStop)),
            "second Ctrl+X must fire the confirmed stop, got {outcome:?}",
        );
        assert!(
            app.pending_action.is_none(),
            "firing must consume the pending confirm",
        );
    }
    /// Overlay Ctrl+X then ANY other key — the pending-action fast
    /// path disarms the confirm (the dashboard's stop-confirm
    /// semantics: any other press cancels), and the other key is
    /// still processed normally.
    #[test]
    fn overlay_ctrl_x_other_key_disarms_confirm() {
        let (mut app, _id) = neutral_overlay_app();
        let _ = app.handle_input(&key_event(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(app.pending_action.is_some(), "first press must arm");
        let _ = app.handle_input(&key_event(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(
            app.pending_action.is_none(),
            "any other key must disarm the pending stop confirm",
        );
        let outcome = app.handle_input(&key_event(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "Ctrl+X after a disarm must re-arm, not fire, got {outcome:?}",
        );
    }
    /// OUTSIDE the overlay (a plain agent view, no dashboard attach),
    /// Ctrl+X must keep its existing agent-screen behaviour — the
    /// overlay stop binding lives in `When::DashboardOverlay` only.
    #[test]
    fn plain_agent_ctrl_x_does_not_arm_overlay_stop() {
        let mut app = test_app_with_agent();
        let id = super::super::agent::AgentId(0);
        app.active_view = ActiveView::Agent(id);
        let _ = app.handle_input(&key_event(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(
            !app.pending_action
                .as_ref()
                .is_some_and(|p| matches!(p.action, Action::DashboardOverlayStop)),
            "overlay stop must not arm outside the dashboard overlay",
        );
    }
    /// When the attached agent disappears externally,
    /// the `handle_input` filter must clear `attached_agent`
    /// immediately rather than waiting for the next draw frame.
    #[test]
    fn minimal_double_ctrl_c_arms_then_quits() {
        let prev = crate::app::minimal_mode_active();
        crate::app::set_minimal_mode_active_for_test(true);
        let mut app = test_app_with_agent();
        if let ActiveView::Agent(id) = app.active_view {
            app.agents.get_mut(&id).unwrap().active_pane = crate::views::agent::ActivePane::Prompt;
        }
        let o1 = app.handle_input(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let armed = app.pending_action.is_some();
        let o2 = app.handle_input(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        crate::app::set_minimal_mode_active_for_test(prev);
        assert!(armed, "first Ctrl+C should arm quit (o1={o1:?})");
        assert!(
            matches!(o2, InputOutcome::Action(crate::app::actions::Action::Quit)),
            "second Ctrl+C should quit (o2={o2:?})"
        );
    }
    #[test]
    fn handle_input_clears_stale_attached_agent_on_input() {
        let mut app = test_app_with_agent();
        let id = attach_popup(&mut app);
        app.agents.shift_remove(&id);
        let _ = app.handle_input(&key_event(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(
            app.dashboard.as_ref().unwrap().attached_agent,
            None,
            "stale attached_agent must be cleared on input",
        );
    }
    /// Click on the popup's `[✗]` close affordance
    /// closes the popup. The close-rect is registered into
    /// `state.popup_close_rect` by the renderer; we set it
    /// directly here since this test doesn't run a render pass.
    #[test]
    fn handle_input_mouse_click_on_close_affordance_closes_popup() {
        let mut app = test_app_with_agent();
        let _ = attach_popup(&mut app);
        if let Some(d) = app.dashboard.as_mut() {
            d.popup_close_rect = Some(ratatui::layout::Rect::new(50, 1, 3, 1));
            d.popup_outer_rect = Some(ratatui::layout::Rect::new(0, 0, 60, 20));
        }
        let click = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 51,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        let outcome = app.handle_input(&click);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(app.dashboard.as_ref().unwrap().attached_agent, None);
    }
    /// A click on a dashboard row outside the popup's
    /// outer rect dispatches `DashboardAttach(clicked_row)` so the
    /// popup target switches.
    #[test]
    fn handle_input_mouse_click_outside_popup_on_row_switches_target() {
        let mut app = test_app_with_agent();
        let _ = attach_popup(&mut app);
        let row_id =
            crate::views::dashboard::DashboardRowId::TopLevel(super::super::agent::AgentId(42));
        if let Some(d) = app.dashboard.as_mut() {
            d.popup_outer_rect = Some(ratatui::layout::Rect::new(20, 5, 40, 10));
            d.row_rects
                .push((row_id.clone(), ratatui::layout::Rect::new(0, 1, 10, 1)));
        }
        let click = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        let outcome = app.handle_input(&click);
        match outcome {
            InputOutcome::Action(Action::DashboardAttach(target)) => {
                assert_eq!(target, row_id);
            }
            other => panic!("expected DashboardAttach, got {other:?}"),
        }
    }
    /// Scroll routing through the popup
    /// overlay. A scroll inside `popup_outer_rect` must NOT advance
    /// the dashboard's `viewport_offset` (it forwards to the
    /// attached agent). A scroll outside the popup falls through to
    /// the dashboard list pane and DOES advance `viewport_offset`.
    #[test]
    fn handle_input_scroll_inside_popup_forwards_to_agent() {
        let mut app = test_app_with_agent();
        let _ = attach_popup(&mut app);
        let popup_outer = ratatui::layout::Rect::new(20, 5, 40, 10);
        if let Some(d) = app.dashboard.as_mut() {
            d.popup_outer_rect = Some(popup_outer);
            d.viewport_offset = 0;
        }
        let inside_x = popup_outer.x + 5;
        let inside_y = popup_outer.y + 3;
        app.dispatch_scroll(3, inside_x, inside_y);
        assert_eq!(
            app.dashboard.as_ref().unwrap().viewport_offset,
            0,
            "scroll inside popup must not advance the dashboard viewport",
        );
        let outside_x = 0;
        let outside_y = 0;
        app.dispatch_scroll(3, outside_x, outside_y);
        assert_eq!(
            app.dashboard.as_ref().unwrap().viewport_offset,
            3,
            "scroll outside popup must advance the dashboard viewport",
        );
    }
    /// When the attached agent emits
    /// `Action::ExitSession` (via the synchronous outcome path,
    /// e.g. user presses the keybind for ExitSession inside the
    /// popup), the popup is closed but the agent stays in
    /// `app.agents`. The `/exit` slash command takes a different
    /// path (emits an effect) — see the user-guide for the
    /// asymmetry; this test pins only the synchronous-outcome
    /// branch.
    ///
    /// We can't easily synthesize an `ExitSession` from
    /// `agent.handle_input` without a real prompt event sequence,
    /// so the test exercises the popup-close intercept by feeding a
    /// key that lands in the agent's prompt and observing the popup
    /// state after the intercept runs. Concretely: we drive an Esc
    /// key (which the popup-close fast-path catches BEFORE the
    /// agent intercept). To prove the `ExitSession` branch
    /// independently, we directly invoke the intercepted-outcome
    /// path with a stub: set `attached_agent`, then call the same
    /// close routine the intercept would call. This is the smallest
    /// behavioural pin available without a full prompt-mode setup.
    #[test]
    fn handle_input_exit_session_action_closes_popup() {
        let mut app = test_app_with_agent();
        let id = attach_popup(&mut app);
        assert!(app.agents.contains_key(&id));
        assert_eq!(app.dashboard.as_ref().unwrap().attached_agent, Some(id));
        if let Some(d) = app.dashboard.as_mut() {
            d.close_popup();
        }
        if let Some(agent) = app.agents.get_mut(&id) {
            agent.active_subagent = None;
        }
        assert_eq!(app.dashboard.as_ref().unwrap().attached_agent, None);
        assert!(
            app.agents.contains_key(&id),
            "ExitSession intercept must NOT remove the agent (it only closes the popup)",
        );
    }
    #[test]
    fn needs_project_picker_false_when_disabled() {
        let mut app = test_app();
        app.project_picker_shown = false;
        app.cwd = std::path::PathBuf::from("/tmp");
        app.project_picker_disabled = true;
        assert!(!app.needs_project_picker());
    }
    #[test]
    fn needs_project_picker_false_when_already_shown() {
        let mut app = test_app();
        app.project_picker_shown = true;
        app.cwd = std::path::PathBuf::from("/tmp");
        app.project_picker_disabled = false;
        assert!(!app.needs_project_picker());
    }
    #[test]
    fn needs_project_picker_true_for_non_project_dir() {
        let mut app = test_app();
        app.project_picker_shown = false;
        app.project_picker_disabled = false;
        app.cwd = std::path::PathBuf::from("/tmp");
        assert!(app.needs_project_picker());
    }
    /// Chat mode hides the welcome picker's source filter, so `f` must not
    /// cycle it; Build mode keeps the cycle.
    #[test]
    fn welcome_picker_f_cycle_disabled_under_chat_mode() {
        let conversation_entry = SessionPickerEntry {
            id: "conv-welcome-f".into(),
            summary: "chat".into(),
            updated_at: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            cwd: String::new(),
            hostname: None,
            source: "conversation".into(),
            model_id: None,
            num_messages: 0,
            last_active_at: None,
            branch: None,
            repo_name: "r".into(),
            worktree_label: None,
            card_detail: None,
        };
        let f_key = Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        crate::appearance::cache::set_vim_mode(false);
        let mut app = test_app();
        app.session_picker_entries = Some(vec![conversation_entry]);
        app.chat_mode = true;
        let _ = app.handle_input(&f_key);
        assert_eq!(
            app.session_picker_source_filter,
            crate::views::session_picker::SourceFilter::All,
            "f must not cycle the hidden source filter under chat mode"
        );
        assert_eq!(
            app.session_picker_state.query(),
            "f",
            "under chat mode `f` keeps its normal typing/search meaning"
        );
        app.session_picker_state.reset();
        app.chat_mode = false;
        let outcome = app.handle_input(&f_key);
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::CycleSessionSourceFilter)
        ));
    }
}
