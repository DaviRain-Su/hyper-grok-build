//! Modal dialogs and the [`ActiveModal`] enum.
//!
//! [`ActiveModal`] wraps concrete modal instances for storage on
//! `AgentView`. Picker-based variants (`CommandPalette`, `ArgPicker`,
//! `SessionPicker`, `DocPicker`, `DocViewer`) use the shared
//! [`ModalWindow`](super::modal_window) component for chrome and
//! [`render_picker_content`](super::picker::render_picker_content) for
//! entry rendering. `EditConfirm` is a bar-style overlay (not a popup).
//!
//! `ModalConfirmation<R>` is a small dialog that blocks all input until
//! the user presses one of the listed keys.
use crate::docs::{DocEntry, default_howto_entries};
use crate::theme::Theme;
use crate::views::modal_window::ModalWindowState;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;
/// A blocking confirmation dialog with typed results.
///
/// `R` is the result type — each dialog use-case defines its own enum.
/// Key-matching is generic; labels are computed per-variant at render time.
pub struct ModalConfirmation<R> {
    /// Available options (key → result). Labels are derived from `R` at render time.
    pub options: Vec<ModalOption<R>>,
}
/// One option in a modal dialog.
pub struct ModalOption<R> {
    /// The key that triggers this option (e.g., 'y', 'n', 'x').
    pub key: char,
    /// The result produced when this option is chosen.
    pub result: R,
}
impl<R> ModalConfirmation<R> {
    /// Check if a character matches any option. Returns the result if matched.
    pub fn resolve(&self, ch: char) -> Option<&R> {
        self.options.iter().find(|o| o.key == ch).map(|o| &o.result)
    }
}
/// Result of the edit-confirmation modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditConfirmResult {
    /// Save changes (back to queue, or save & send if drain-blocked).
    Save,
    /// Discard changes (revert to original; sends original if drain-blocked).
    Discard,
    /// Delete the prompt entirely from the queue.
    Delete,
    /// Cancel — dismiss the dialog, stay in editing mode.
    Cancel,
}
impl EditConfirmResult {
    /// Dynamic label based on whether the agent is waiting to drain.
    pub fn label(&self, drain_blocked: bool) -> &'static str {
        match (self, drain_blocked) {
            (Self::Save, false) => "save",
            (Self::Save, true) => "save & send",
            (Self::Discard, false) => "discard changes",
            (Self::Discard, true) => "discard & send",
            (Self::Delete, _) => "delete prompt",
            (Self::Cancel, _) => "cancel",
        }
    }
}
impl ModalConfirmation<EditConfirmResult> {
    /// Create the edit confirmation modal.
    ///
    /// Always shows three options: save (y), discard (n), delete (x).
    /// Labels are computed dynamically at render time based on `drain_blocked`.
    pub fn edit_confirm() -> Self {
        Self {
            options: vec![
                ModalOption {
                    key: 'y',
                    result: EditConfirmResult::Save,
                },
                ModalOption {
                    key: 'n',
                    result: EditConfirmResult::Discard,
                },
                ModalOption {
                    key: 'x',
                    result: EditConfirmResult::Delete,
                },
            ],
        }
    }
}
/// Result of the reset-settings confirmation modal.
/// `y` → Reset, `n`/`Esc`/`F2`/`Ctrl+,` → Cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetSettingsResult {
    /// Restore the setting to its registered default.
    Reset,
    /// Cancel — return to the Settings modal unchanged.
    Cancel,
}
impl ResetSettingsResult {
    /// Label for the y/n buttons rendered in the modal footer.
    pub fn label(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::Cancel => "cancel",
        }
    }
}
/// Shortcut IDs for the reset-confirm footer buttons (1-2, avoiding
/// the extensions modal's 100+ range).
pub const RESET_CONFIRM_YES_ID: usize = 1;
pub const RESET_CONFIRM_NO_ID: usize = 2;
impl ModalConfirmation<ResetSettingsResult> {
    /// Create the reset-settings confirmation modal (`y`/`n`).
    pub fn reset_settings() -> Self {
        Self {
            options: vec![
                ModalOption {
                    key: 'y',
                    result: ResetSettingsResult::Reset,
                },
                ModalOption {
                    key: 'n',
                    result: ResetSettingsResult::Cancel,
                },
            ],
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelTurnChoice {
    StopRunning,
    ContinueToRun,
    AlwaysStop,
    AlwaysContinue,
}
impl CancelTurnChoice {
    pub const ALL: [CancelTurnChoice; 4] = [
        CancelTurnChoice::StopRunning,
        CancelTurnChoice::ContinueToRun,
        CancelTurnChoice::AlwaysStop,
        CancelTurnChoice::AlwaysContinue,
    ];
    pub fn label(&self) -> &'static str {
        match self {
            Self::StopRunning => "Stop running",
            Self::ContinueToRun => "Continue to run",
            Self::AlwaysStop => "Always stop",
            Self::AlwaysContinue => "Always continue",
        }
    }
}
pub struct CancelTurnViewState {
    pub active_idx: usize,
    pub running_count: usize,
}
/// Returns a ready-to-open DocPicker modal for the how-to guides list.
///
/// `previous_palette` is the saved command-palette state; when provided,
/// pressing Esc in the doc picker restores that palette instead of
/// closing the modal outright.
pub fn howto_list_modal(previous_palette: Option<PaletteSnapshot>) -> ActiveModal {
    ActiveModal::DocPicker {
        entries: default_howto_entries(),
        state: crate::views::picker::PickerState::default(),
        previous_palette,
        window: ModalWindowState::new(),
    }
}
/// The currently active modal dialog, if any.
///
/// Each variant wraps a `ModalConfirmation<R>` with its concrete result
/// type plus any context needed for resolution (e.g., pending focus target).
pub enum ActiveModal {
    /// Confirmation for leaving a dirty queued-prompt edit.
    EditConfirm {
        modal: ModalConfirmation<EditConfirmResult>,
        /// Where to switch focus after confirmation (if not Cancel).
        pending_target: super::agent::ActivePane,
    },
    /// Command palette (Ctrl+P).
    CommandPalette {
        entries: Vec<PaletteEntry>,
        state: crate::views::picker::PickerState,
        /// Shared modal window chrome state.
        window: ModalWindowState,
    },
    /// Argument picker for commands with pre-defined choices (model, theme).
    /// Opens when selecting such a command from the command palette.
    ArgPicker {
        /// Command name (e.g., "model", "theme").
        command: String,
        /// Args query passed to `suggest_args` (empty = first phase; for `/model`,
        /// a trailing-space query enters the reasoning-effort sub-menu).
        args_query: String,
        /// Filtered items (re-filtered from original_items on query change).
        items: Vec<crate::slash::command::ArgItem>,
        /// Original items from suggest_args() (source for filtering).
        original_items: Vec<crate::slash::command::ArgItem>,
        /// `/model` only: show the full catalog (locked BYOK rows + hidden
        /// models) instead of the scoped default. Toggled with Tab.
        show_all: bool,
        /// `/model` only: ids currently in `[models].hidden_models` (for
        /// hide/unhide via ^X and All-view synthesis).
        hidden_ids: Vec<String>,
        /// Unified picker state.
        state: crate::views::picker::PickerState,
        /// Previous command palette state (if opened from palette). Restored on Esc.
        previous_palette: Option<PaletteSnapshot>,
        /// Shared modal window chrome state.
        window: ModalWindowState,
    },
    /// Session picker (opened from /resume command or command palette).
    SessionPicker {
        /// Unified picker state.
        state: crate::views::picker::PickerState,
        /// Fetched session entries (None = not yet loaded).
        entries: Option<Vec<crate::app::app_view::SessionPickerEntry>>,
        /// Whether the session list is being fetched.
        loading: bool,
        /// Foreign lane completion and deferred native-lane notice.
        lanes: crate::views::session_picker::SessionPickerLanes,
        /// Previous command palette state (if opened from palette). Restored on Esc.
        previous_palette: Option<PaletteSnapshot>,
        /// Shared modal window chrome state.
        window: ModalWindowState,
        /// Content-based (deep search) results from ACP session search.
        content_results: Option<Vec<xai_grok_shell::extensions::session_search::SearchSessionHit>>,
        /// Whether a deep search is currently in flight.
        content_loading: bool,
        /// Monotonically increasing sequence number for deep search requests.
        deep_search_seq: u64,
        /// The search query `entries` were server-fetched with (`None` =
        /// unfiltered fetch). See
        /// [`crate::views::session_picker::effective_filter_query`].
        entries_query: Option<String>,
        /// Source filter for the modal session picker.
        source_filter: crate::views::session_picker::SourceFilter,
        /// Session armed for delete, captured as `(source, session_id, cwd)` when
        /// `d` is pressed so the `y` confirm always has a valid cwd even if
        /// the picker lists change underneath it. `Some` only while the
        /// focused row is armed; cleared on cancel / completion.
        pending_delete: Option<(String, String, String)>,
    },
    /// How-to documentation list modal (wider picker style).
    DocPicker {
        entries: Vec<DocEntry>,
        state: crate::views::picker::PickerState,
        /// Previous command palette state (if opened from palette). Restored on Esc.
        previous_palette: Option<PaletteSnapshot>,
        /// Shared modal window chrome state.
        window: ModalWindowState,
    },
    /// Documentation panel showing full content of a selected how-to guide.
    DocViewer {
        title: String,
        content: String,
        /// Vertical scroll offset in lines.
        scroll: u16,
        /// Shared modal window chrome state.
        window: ModalWindowState,
        /// Cached pre-rendered markdown lines + the width they were
        /// rendered at. Invalidated when the content area width changes
        /// (e.g. terminal resize) so lines are re-parsed at the new width.
        cached_lines: Option<(u16, Vec<ratatui::text::Line<'static>>)>,
        /// Palette snapshot shuttled from DocPicker, passed back when returning
        /// to the doc list on Esc so the DocPicker can still restore the palette.
        previous_palette: Option<PaletteSnapshot>,
        /// When true, Esc closes the modal directly instead of returning
        /// to the DocPicker list (used for /release-notes).
        standalone: bool,
    },
    /// All-shortcuts cheatsheet for the current view/state.
    /// Rendered via the unified picker (same look as CommandPalette).
    /// See `crate::views::shortcuts_help` for build/render/input logic.
    ShortcutsHelp {
        /// Snapshot of the entries (section headers + hints) at open time.
        entries: Vec<crate::views::shortcuts_help::ShortcutsHelpEntry>,
        /// Unified picker state (search query, selection, scroll, hit areas).
        state: crate::views::picker::PickerState,
        /// Modal window chrome state (close button, scroll region).
        window: crate::views::modal_window::ModalWindowState,
        /// When true, dimmed (out-of-context) shortcuts are hidden.
        filter_active: bool,
        /// Indices into CATEGORY_ORDER that are collapsed. Default: all except 0.
        collapsed_sections: std::collections::HashSet<usize>,
        /// Rows whose inline help is expanded under the list (pattern A).
        expanded_ids: std::collections::HashSet<crate::views::shortcuts_help::ExpandKey>,
        /// Browse list vs in-modal detail page (pattern B).
        mode: crate::views::shortcuts_help::ShortcutsHelpMode,
    },
    /// Memory browser modal (/memory).
    MemoryBrowser {
        state: Box<crate::views::memory_modal::MemoryModalState>,
    },
    /// Settings modal (F2, /settings, palette). Boxed — large state.
    Settings {
        state: Box<crate::views::settings_modal::SettingsModalState>,
    },
    /// Reset-settings confirmation, stacked above Settings.
    ///
    /// The underlying `SettingsModalState` is moved in/out so cancel
    /// preserves the user's filter/scroll position. The setting key
    /// lives only here (single source of truth for dispatch).
    ResetSettingsConfirm {
        modal: ModalConfirmation<ResetSettingsResult>,
        /// Setting key being reset.
        key: crate::settings::SettingKey,
        /// Preserved settings state, restored by both choice branches.
        settings_state: Box<crate::views::settings_modal::SettingsModalState>,
    },
    /// Modal preview for a `#` remember note. Shows the raw text immediately;
    /// the LLM-enhanced version arrives asynchronously and can be toggled with Tab.
    RememberNoteReview {
        raw_content: String,
        enhanced_content: Option<String>,
        showing_enhanced: bool,
        scroll: u16,
        window: ModalWindowState,
        cached_lines: Option<(u16, Vec<ratatui::text::Line<'static>>)>,
        cwd: std::path::PathBuf,
        agent_id: crate::app::agent::AgentId,
        /// Monotonic nonce to correlate async rewrite results with the modal
        /// that requested them, preventing stale results from populating a
        /// different note's review modal.
        rewrite_nonce: u64,
    },
}
/// Snapshot of the command palette state, saved when opening an arg picker
/// and restored on Esc.
#[derive(Debug, Clone)]
pub struct PaletteSnapshot {
    pub entries: Vec<PaletteEntry>,
    pub state: crate::views::picker::PickerState,
}
/// A single entry in the command palette.
#[derive(Debug, Clone)]
pub struct PaletteEntry {
    /// Display label (e.g., "New Session").
    pub label: String,
    /// Keyboard shortcut hint (e.g., "Ctrl+N").
    pub shortcut: String,
    /// Which palette command this executes.
    pub command: PaletteCommand,
}
/// Commands available in the command palette.
#[derive(Debug, Clone)]
pub enum PaletteCommand {
    NewSession,
    NewSessionInWorktree,
    Home,
    Quit,
    /// Execute a slash command through the palette's draft-preserving route.
    SlashCommand(String),
    /// Edit the minimal-mode composer draft without routing through slash text.
    EditPromptExternal,
    /// Non-selectable section header for visual grouping.
    SectionHeader(String),
    /// Open the how-to documentation picker.
    HowTo,
    /// Open the keyboard shortcuts cheatsheet (Ctrl+.).
    KeyboardShortcuts,
    /// Open the memory browser modal.
    Memory,
    /// Open the Extensions modal on a specific tab. Used by palette
    /// entries that don't have a corresponding slash command (e.g.
    /// "Marketplace", "Skills") and to keep direct entries consistent.
    OpenExtensionsTab(crate::views::extensions_modal::ExtensionsTab),
    /// Open the settings modal.
    OpenSettings,
    /// Open the Agents modal (listing all agent definitions).
    OpenAgentsModal,
}
/// Build the default set of palette entries with section grouping.
///
/// `sharing_enabled` controls whether `/share` is included. `screen_mode`
/// exposes the draft-preserving external-editor row only in minimal mode.
pub(crate) fn default_palette_entries(
    sharing_enabled: bool,
    screen_mode: crate::app::ScreenMode,
) -> Vec<PaletteEntry> {
    let mut entries = vec![
        // ── Session ──
        PaletteEntry {
            label: "Session".into(),
            shortcut: String::new(),
            command: PaletteCommand::SectionHeader("Session".into()),
        },
        PaletteEntry {
            label: "New Session".into(),
            shortcut: "Ctrl+N".into(),
            command: PaletteCommand::NewSession,
        },
        PaletteEntry {
            label: "New Session in Worktree".into(),
            shortcut: "Ctrl+P → worktree".into(),
            command: PaletteCommand::NewSessionInWorktree,
        },
        PaletteEntry {
            label: "Agent Dashboard".into(),
            shortcut: "/dashboard".into(),
            command: PaletteCommand::SlashCommand("/dashboard".into()),
        },
        PaletteEntry {
            label: "Back to Home".into(),
            shortcut: "/home".into(),
            command: PaletteCommand::Home,
        },
        PaletteEntry {
            label: "Resume Session".into(),
            shortcut: "/resume".into(),
            command: PaletteCommand::SlashCommand("/resume".into()),
        },
        PaletteEntry {
            label: "Share Session".into(),
            shortcut: "/share".into(),
            command: PaletteCommand::SlashCommand("/share".into()),
        },
        PaletteEntry {
            label: "Rename Session".into(),
            shortcut: "/rename ".into(),
            command: PaletteCommand::SlashCommand("/rename ".into()),
        },
        PaletteEntry {
            label: "Session Info".into(),
            shortcut: "/session-info".into(),
            command: PaletteCommand::SlashCommand("/session-info".into()),
        },
        PaletteEntry {
            label: "Send Feedback".into(),
            shortcut: "/feedback".into(),
            command: PaletteCommand::SlashCommand("/feedback ".into()),
        },
        // ── Context ──
        PaletteEntry {
            label: "Context".into(),
            shortcut: String::new(),
            command: PaletteCommand::SectionHeader("Context".into()),
        },
        PaletteEntry {
            label: "Compact History".into(),
            shortcut: "/compact".into(),
            command: PaletteCommand::SlashCommand("/compact".into()),
        },
        PaletteEntry {
            label: "Context Usage".into(),
            shortcut: "/context".into(),
            command: PaletteCommand::SlashCommand("/context".into()),
        },
        PaletteEntry {
            label: "View Plan".into(),
            shortcut: "/view-plan".into(),
            command: PaletteCommand::SlashCommand("/view-plan".into()),
        },
        PaletteEntry {
            label: "Memory".into(),
            shortcut: "/memory".into(),
            command: PaletteCommand::Memory,
        },
        // ── Model & Input ──
        PaletteEntry {
            label: "Model & Input".into(),
            shortcut: String::new(),
            command: PaletteCommand::SectionHeader("Model & Input".into()),
        },
        PaletteEntry {
            label: "Switch Model".into(),
            shortcut: "/model".into(),
            command: PaletteCommand::SlashCommand("/model ".into()),
        },
        PaletteEntry {
            label: "Always Approve Mode".into(),
            shortcut: "/always-approve".into(),
            command: PaletteCommand::SlashCommand("/always-approve".into()),
        },
        PaletteEntry {
            label: "Multiline Input".into(),
            shortcut: "/multiline".into(),
            command: PaletteCommand::SlashCommand("/multiline".into()),
        },
        PaletteEntry {
            label: "Edit Prompt in External Editor".into(),
            shortcut: "Ctrl+G".into(),
            command: PaletteCommand::EditPromptExternal,
        },
        // ── Tools ──
        PaletteEntry {
            label: "Tools".into(),
            shortcut: String::new(),
            command: PaletteCommand::SectionHeader("Tools".into()),
        },
        PaletteEntry {
            label: "Hooks".into(),
            shortcut: "/hooks".into(),
            command: PaletteCommand::OpenExtensionsTab(
                crate::views::extensions_modal::ExtensionsTab::Hooks,
            ),
        },
        PaletteEntry {
            label: "Plugins".into(),
            shortcut: "/plugins".into(),
            command: PaletteCommand::OpenExtensionsTab(
                crate::views::extensions_modal::ExtensionsTab::Plugins,
            ),
        },
        PaletteEntry {
            label: "Marketplace".into(),
            shortcut: "/marketplace".into(),
            command: PaletteCommand::OpenExtensionsTab(
                crate::views::extensions_modal::ExtensionsTab::Marketplace,
            ),
        },
        PaletteEntry {
            label: "Skills".into(),
            shortcut: "/skills".into(),
            command: PaletteCommand::OpenExtensionsTab(
                crate::views::extensions_modal::ExtensionsTab::Skills,
            ),
        },
        PaletteEntry {
            label: "MCP Servers".into(),
            shortcut: "/mcps".into(),
            command: PaletteCommand::OpenExtensionsTab(
                crate::views::extensions_modal::ExtensionsTab::McpServers,
            ),
        },
        PaletteEntry {
            label: "Manage Agents".into(),
            shortcut: "/config-agents".into(),
            command: PaletteCommand::OpenAgentsModal,
        },
        // ── Other ──
        PaletteEntry {
            label: "Other".into(),
            shortcut: String::new(),
            command: PaletteCommand::SectionHeader("Other".into()),
        },
        PaletteEntry {
            label: "Switch Theme".into(),
            shortcut: "/theme".into(),
            command: PaletteCommand::SlashCommand("/theme ".into()),
        },
        PaletteEntry {
            label: "Settings".into(),
            shortcut: "F2".into(),
            command: PaletteCommand::OpenSettings,
        },
        PaletteEntry {
            label: "Keyboard Shortcuts".into(),
            shortcut: if crate::actions::ctrl_dot_unreliable() {
                "Ctrl+X".into()
            } else {
                "Ctrl+.".into()
            },
            command: PaletteCommand::KeyboardShortcuts,
        },
        PaletteEntry {
            label: "How-to Guides".into(),
            shortcut: "/docs".into(),
            command: PaletteCommand::HowTo,
        },
        PaletteEntry {
            label: "Quit".into(),
            shortcut: "Ctrl+Q".into(),
            command: PaletteCommand::Quit,
        },
    ];
    entries.retain(|entry| {
        if !sharing_enabled
            && matches!(&entry.command, PaletteCommand::SlashCommand(s) if s.trim() == "/share")
        {
            return false;
        }
        screen_mode.is_minimal() || !matches!(entry.command, PaletteCommand::EditPromptExternal)
    });
    entries
}
#[allow(clippy::collapsible_if)]
/// Filter palette entries for search, preserving section headers when any item in the section matches.
pub(crate) fn filter_palette_entries(
    query: &str,
    sharing_enabled: bool,
    screen_mode: crate::app::ScreenMode,
) -> Vec<PaletteEntry> {
    let all = default_palette_entries(sharing_enabled, screen_mode);
    let query_lower = query.to_lowercase();
    if query_lower.is_empty() {
        return all;
    }
    let mut result = Vec::new();
    let mut pending_header: Option<PaletteEntry> = None;
    let mut section_has_match = false;
    for entry in all {
        if matches!(entry.command, PaletteCommand::SectionHeader(_)) {
            if let Some(h) = pending_header.take() {
                if section_has_match {
                    result.push(h);
                }
            }
            pending_header = Some(entry);
            section_has_match = false;
        } else {
            let matches = entry.label.to_lowercase().contains(&query_lower)
                || entry.shortcut.to_lowercase().contains(&query_lower);
            if matches {
                if let Some(h) = pending_header.take() {
                    result.push(h);
                    section_has_match = true;
                }
                result.push(entry);
            }
        }
    }
    if let Some(h) = pending_header {
        if section_has_match {
            result.push(h);
        }
    }
    result
}
impl ActiveModal {
    pub fn hint_pairs(&self, drain_blocked: bool) -> Vec<(char, &'static str)> {
        match self {
            ActiveModal::EditConfirm { modal, .. } => modal
                .options
                .iter()
                .map(|o| (o.key, o.result.label(drain_blocked)))
                .collect(),
            ActiveModal::ResetSettingsConfirm { modal, .. } => modal
                .options
                .iter()
                .map(|o| (o.key, o.result.label()))
                .collect(),
            ActiveModal::CommandPalette { .. }
            | ActiveModal::ArgPicker { .. }
            | ActiveModal::SessionPicker { .. }
            | ActiveModal::DocPicker { .. }
            | ActiveModal::DocViewer { .. }
            | ActiveModal::ShortcutsHelp { .. }
            | ActiveModal::MemoryBrowser { .. }
            | ActiveModal::Settings { .. }
            | ActiveModal::RememberNoteReview { .. } => vec![],
        }
    }
    pub fn message(&self, drain_blocked: bool) -> std::borrow::Cow<'static, str> {
        match self {
            ActiveModal::EditConfirm { .. } => {
                if drain_blocked {
                    rust_i18n::t!("modal.title.save_and_send")
                } else {
                    rust_i18n::t!("modal.title.save_changes")
                }
            }
            ActiveModal::CommandPalette { .. } => rust_i18n::t!("modal.title.commands"),
            ActiveModal::SessionPicker { .. } => rust_i18n::t!("modal.title.resume_session"),
            ActiveModal::ArgPicker {
                command,
                args_query,
                ..
            } => match command.as_str() {
                "model" | "m" if !args_query.is_empty() => {
                    rust_i18n::t!("modal.title.pick_effort")
                }
                "model" | "m" => rust_i18n::t!("modal.title.pick_model"),
                "theme" | "t" => rust_i18n::t!("modal.title.pick_theme"),
                _ => rust_i18n::t!("modal.title.pick_option"),
            },
            ActiveModal::DocPicker { .. } => rust_i18n::t!("modal.title.howto_guides"),
            ActiveModal::DocViewer { title, .. } => title.clone().into(),
            ActiveModal::ShortcutsHelp { .. } => rust_i18n::t!("modal.title.keyboard_shortcuts"),
            ActiveModal::MemoryBrowser { .. } => rust_i18n::t!("modal.title.memory"),
            ActiveModal::Settings { .. } => rust_i18n::t!("modal.title.settings"),
            ActiveModal::ResetSettingsConfirm { .. } => {
                rust_i18n::t!("modal.title.reset_setting")
            }
            ActiveModal::RememberNoteReview { .. } => rust_i18n::t!("modal.title.memory_note"),
        }
    }
}
/// Build the reset-confirmation prompt, e.g. "Reset 'Compact mode' to default (off)?".
/// Returns `None` if the modal isn't `ResetSettingsConfirm` or the key is unknown.
pub fn reset_confirm_prompt(modal: &ActiveModal) -> Option<String> {
    let ActiveModal::ResetSettingsConfirm {
        key,
        settings_state,
        ..
    } = modal
    else {
        return None;
    };
    let meta = settings_state.registry.find(key)?;
    let default = crate::settings::default_value_for(meta);
    Some(
        rust_i18n::t!(
            "settings_modal.reset_prompt",
            label = meta.label_l10n(),
            default = format_default_for_prompt(meta, &default)
        )
        .into_owned(),
    )
}
/// Abbreviated title breadcrumb for the reset-confirm dialog, e.g. "Reset 'Compact mode'".
pub fn reset_confirm_breadcrumb(modal: &ActiveModal) -> Option<String> {
    let ActiveModal::ResetSettingsConfirm {
        key,
        settings_state,
        ..
    } = modal
    else {
        return None;
    };
    let meta = settings_state.registry.find(key)?;
    Some(rust_i18n::t!("settings_modal.reset_breadcrumb", label = meta.label_l10n()).into_owned())
}
/// Format a `SettingValue` for the prompt's `(<default>)` display.
fn format_default_for_prompt(
    meta: &crate::settings::SettingMeta,
    value: &crate::settings::SettingValue,
) -> String {
    use crate::settings::{SettingKind, SettingValue};
    match value {
        SettingValue::Bool(true) => rust_i18n::t!("settings_modal.value_on").into_owned(),
        SettingValue::Bool(false) => rust_i18n::t!("settings_modal.value_off").into_owned(),
        SettingValue::Enum(canonical) => {
            if let SettingKind::Enum { choices, .. } = &meta.kind {
                for c in *choices {
                    if c.canonical == *canonical {
                        return c.display_l10n(meta.key).into_owned();
                    }
                }
            }
            (*canonical).to_owned()
        }
        SettingValue::String(s) => format!("\"{s}\""),
        SettingValue::Int(i) => i.to_string(),
    }
}
/// A clickable button region from the rendered modal.
#[derive(Debug, Clone, Copy)]
pub struct ModalButtonHit {
    pub rect: Rect,
    pub key: char,
}
/// Result of rendering a modal overlay.
pub struct ModalRenderResult {
    /// Hit areas for each button (for mouse click/hover).
    pub buttons: Vec<ModalButtonHit>,
}
/// Render the modal overlay: dim screen + styled bar at bottom.
///
/// Layout of the bar:
/// ```text
/// Save changes?  [ y:save ] [ n:discard ]
/// ```
///
/// - Message in `text_primary` bold
/// - Each button: dark bg pill, lighter on hover
/// - Screen above the bar is dimmed to `gray_dim` fg + `bg_base` bg
///
/// `bar_area` is the 1-line rect where the bar renders (shortcuts bar slot).
/// `dim_area` is everything above the bar (to be dimmed).
/// `hovered_key` is the button key currently under the mouse (if any).
pub fn render_modal_overlay(
    buf: &mut Buffer,
    modal: &ActiveModal,
    bar_area: Rect,
    dim_area: Rect,
    hovered_key: Option<char>,
    drain_blocked: bool,
) -> ModalRenderResult {
    let theme = Theme::current();
    let dim_style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
    for y in dim_area.y..dim_area.y + dim_area.height {
        for x in dim_area.x..dim_area.x + dim_area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(dim_style);
            }
        }
    }
    if bar_area.height == 0 || bar_area.width < 10 {
        return ModalRenderResult {
            buttons: Vec::new(),
        };
    }
    let bar_bg = theme.bg_base;
    let bar_style = Style::default().fg(theme.text_primary).bg(bar_bg);
    for x in bar_area.x..bar_area.x + bar_area.width {
        if let Some(cell) = buf.cell_mut((x, bar_area.y)) {
            cell.reset();
            cell.set_style(bar_style);
        }
    }
    let msg = modal.message(drain_blocked);
    let msg_style = Style::default()
        .fg(theme.text_primary)
        .bg(bar_bg)
        .add_modifier(Modifier::BOLD);
    let msg_span = Span::styled(msg.clone(), msg_style);
    buf.set_span(bar_area.x, bar_area.y, &msg_span, bar_area.width);
    let mut x = bar_area.x + msg.width() as u16 + 2;
    let btn_bg = theme.bg_dark;
    let btn_hover_bg = theme.gray_dim;
    let pairs = modal.hint_pairs(drain_blocked);
    let mut buttons = Vec::with_capacity(pairs.len());
    for (key, label) in &pairs {
        let btn_text_w = 1 + 1 + 1 + label.width() + 1;
        let btn_w = btn_text_w as u16;
        if x + btn_w > bar_area.x + bar_area.width {
            break;
        }
        let is_hovered = hovered_key == Some(*key);
        let bg = if is_hovered { btn_hover_bg } else { btn_bg };
        let key_style = Style::default()
            .fg(theme.accent_user)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        let label_style = Style::default().fg(theme.gray).bg(bg);
        let pad_style = Style::default().bg(bg);
        let btn_rect = Rect {
            x,
            y: bar_area.y,
            width: btn_w,
            height: 1,
        };
        buf.set_span(x, bar_area.y, &Span::styled(" ", pad_style), 1);
        buf.set_span(
            x + 1,
            bar_area.y,
            &Span::styled(key.to_string(), key_style),
            1,
        );
        buf.set_span(x + 2, bar_area.y, &Span::styled(":", label_style), 1);
        buf.set_span(
            x + 3,
            bar_area.y,
            &Span::styled(label.to_string(), label_style),
            label.width() as u16,
        );
        buf.set_span(
            x + 3 + label.width() as u16,
            bar_area.y,
            &Span::styled(" ", pad_style),
            1,
        );
        buttons.push(ModalButtonHit {
            rect: btn_rect,
            key: *key,
        });
        x += btn_w + 1;
    }
    ModalRenderResult { buttons }
}
/// vpad(1) + title(1) + count(1) + gap(1) + 4 options + vpad(1) = 9
const CANCEL_TURN_PANEL_HEIGHT: u16 = 9;
pub fn cancel_turn_panel_height(screen_h: u16) -> u16 {
    let cap = (screen_h as u32 * 33 / 100)
        .max(8)
        .min(screen_h as u32 * 80 / 100) as u16;
    CANCEL_TURN_PANEL_HEIGHT.min(cap)
}
pub fn render_cancel_turn_panel(
    buf: &mut Buffer,
    area: Rect,
    state: &CancelTurnViewState,
    focused: bool,
    button_rects: &mut Vec<Rect>,
) {
    button_rects.clear();
    let theme = Theme::current();
    buf.set_style(area, Style::default().bg(theme.bg_light));
    let accent_style = Style::default().fg(theme.warning);
    for row in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, row)) {
            cell.set_symbol(crate::glyphs::accent_bar());
            cell.set_style(accent_style);
        }
    }
    let content_x = area.x + 3;
    let content_w = area.width.saturating_sub(5) as usize;
    let mut y = area.y + 1;
    let title_style = Style::default()
        .fg(theme.accent_user)
        .add_modifier(Modifier::BOLD);
    buf.set_line(
        content_x,
        y,
        &Line::from(Span::styled(
            "Subagents are still running. Stop them?",
            title_style,
        )),
        content_w as u16,
    );
    y += 1;
    let count_text = if state.running_count == 1 {
        "1 subagent running".to_string()
    } else {
        format!("{} subagents running", state.running_count)
    };
    buf.set_line(
        content_x,
        y,
        &Line::from(Span::styled(count_text, Style::default().fg(theme.gray))),
        content_w as u16,
    );
    y += 2;
    for (i, choice) in CancelTurnChoice::ALL.iter().enumerate() {
        if y >= area.y + area.height {
            break;
        }
        let is_cursor = i == state.active_idx;
        let row_bg = if is_cursor && focused {
            theme.bg_visual
        } else {
            theme.bg_light
        };
        let row_rect = Rect {
            x: content_x.saturating_sub(1),
            y,
            width: content_w as u16 + 2,
            height: 1,
        };
        buf.set_style(row_rect, Style::default().bg(row_bg));
        button_rects.push(row_rect);
        let marker = if is_cursor {
            crate::glyphs::filled_dot()
        } else {
            "\u{25CB}"
        };
        let num = (i + 1).to_string();
        let num_style = Style::default().fg(theme.accent_user).bg(row_bg);
        let marker_style = if is_cursor {
            Style::default().fg(theme.accent_user).bg(row_bg)
        } else {
            Style::default().fg(theme.gray).bg(row_bg)
        };
        let label_style = Style::default()
            .fg(theme.text_primary)
            .bg(row_bg)
            .add_modifier(if is_cursor {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let line = Line::from(vec![
            Span::styled(format!("{num} "), num_style),
            Span::styled(format!("({marker}) "), marker_style),
            Span::styled(choice.label(), label_style),
        ]);
        buf.set_line(content_x, y, &line, content_w as u16);
        y += 1;
    }
    if !focused {
        crate::render::color::blend_area(buf, area, Some((theme.bg_light, 0.66)), None);
    }
}
/// Apply scroll-key dispatch for a DocViewer modal. Returns `true` if the key
/// was handled (caller should return `InputOutcome::Changed`).
pub fn apply_doc_scroll(code: crossterm::event::KeyCode, scroll: &mut u16) -> bool {
    use crossterm::event::KeyCode;
    match code {
        KeyCode::Down | KeyCode::Char('j') => {
            *scroll = scroll.saturating_add(3);
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *scroll = scroll.saturating_sub(3);
            true
        }
        KeyCode::PageDown => {
            *scroll = scroll.saturating_add(20);
            true
        }
        KeyCode::PageUp => {
            *scroll = scroll.saturating_sub(20);
            true
        }
        KeyCode::Home => {
            *scroll = 0;
            true
        }
        KeyCode::End => {
            *scroll = u16::MAX;
            true
        }
        _ => false,
    }
}
/// Apply a signed line delta to a DocViewer scroll offset (positive = down).
pub fn apply_doc_scroll_delta(scroll: &mut u16, lines: i32) {
    if lines == 0 {
        return;
    }
    if lines > 0 {
        *scroll = scroll.saturating_add(lines as u16);
    } else {
        *scroll = scroll.saturating_sub(lines.unsigned_abs() as u16);
    }
}
/// Apply mouse-wheel events to a DocViewer scroll offset. Returns `true` if
/// the event was a scroll and the offset was updated.
pub fn apply_doc_mouse_scroll(kind: crossterm::event::MouseEventKind, scroll: &mut u16) -> bool {
    use crossterm::event::MouseEventKind;
    match kind {
        MouseEventKind::ScrollDown => {
            apply_doc_scroll_delta(scroll, 3);
            true
        }
        MouseEventKind::ScrollUp => {
            apply_doc_scroll_delta(scroll, -3);
            true
        }
        _ => false,
    }
}
const DOCS_USER_GUIDE_REL: &str = "docs/user-guide";
/// Prefer keeping the on-disk path when width is tight.
fn fit_docs_ask_grok_tip(docs_path: &str, width: usize) -> String {
    use crate::render::line_utils::truncate_str;
    if width == 0 {
        return String::new();
    }
    let long =
        format!("Tip · Ask Grok about the docs ({docs_path}) — e.g. \"how do I set up MCP?\"");
    if long.width() <= width {
        return long;
    }
    let short = format!("Tip · Ask Grok about the docs · {docs_path}");
    if short.width() <= width {
        return short;
    }
    let path_only = format!("Tip · {docs_path}");
    if path_only.width() <= width {
        return path_only;
    }
    const PREFIX: &str = "Tip · ";
    let budget = width.saturating_sub(PREFIX.width());
    if budget == 0 {
        return truncate_str("Tip", width);
    }
    format!("{PREFIX}{}", truncate_str(docs_path, budget))
}
pub fn render_doc_picker_overlay(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    window: &mut super::modal_window::ModalWindowState,
    entries: &[DocEntry],
    state: &mut super::picker::PickerState,
    compact: bool,
    theme: &Theme,
) {
    use super::modal_window::{
        self as mw, ModalSizing, ModalWindowConfig, Shortcut, footer_lines_with_tip_gap,
        render_centered_tip_footer, split_content_for_tip_footer,
    };
    use super::picker::{self, PickerEntry, PickerRow};
    let filtered: Vec<_> = if state.query().is_empty() {
        entries.iter().enumerate().collect()
    } else {
        let q = state.query().to_lowercase();
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.title.to_lowercase().contains(&q) || e.description.to_lowercase().contains(&q)
            })
            .collect()
    };
    let non_sel = vec![false; filtered.len()];
    let mut picker_shortcuts: Vec<Shortcut<'_>> = vec![
        Shortcut {
            label: "\u{2191}/\u{2193} nav",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Enter select",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Esc close",
            clickable: false,
            id: 0,
        },
    ];
    mw::push_vim_nav_search_hint(&mut picker_shortcuts, state.search_active);
    let base_sizing = ModalSizing {
        width_pct: 0.70,
        max_width: 120,
        min_width: 44,
        v_margin: 4,
        h_pad: 2,
        v_pad: 1,
        footer_lines: 2,
    }
    .with_compact(compact);
    let sizing = ModalSizing {
        footer_lines: footer_lines_with_tip_gap(area, &base_sizing, &picker_shortcuts),
        ..base_sizing
    };
    let modal_config = ModalWindowConfig {
        title: "How-to Guides",
        tabs: None,
        shortcuts: &picker_shortcuts,
        sizing,
        fold_info: None,
    };
    let Some(mca) = mw::render_modal_window(buf, area, window, &modal_config, theme) else {
        return;
    };
    let (picker_area, tip_area) = split_content_for_tip_footer(mca.content);
    if let Some(tip_rect) = tip_area {
        let docs_path = crate::util::display_user_grok_path(DOCS_USER_GUIDE_REL);
        let tip_line = fit_docs_ask_grok_tip(&docs_path, tip_rect.width as usize);
        render_centered_tip_footer(buf, tip_rect, theme, &tip_line);
    }
    const NARROW_THRESHOLD: u16 = 70;
    let narrow = picker_area.width < NARROW_THRESHOLD;
    let desc_slices: Vec<Vec<&str>> = if narrow {
        filtered
            .iter()
            .map(|(_, e)| vec![e.description.as_str()])
            .collect()
    } else {
        Vec::new()
    };
    let selected_orig = filtered
        .get(state.selected)
        .map(|(o, _)| *o)
        .unwrap_or(usize::MAX);
    let picker_entries: Vec<PickerEntry<'_>> = filtered
        .iter()
        .enumerate()
        .map(|(i, (orig_idx, e))| {
            PickerEntry::Row(PickerRow {
                label: &e.title,
                right_label: if narrow { "" } else { &e.description },
                selected: *orig_idx == selected_orig,
                expanded: narrow,
                fields: &[],
                description_lines: if narrow { &desc_slices[i] } else { &[] },
                summary_lines: &[],
                dimmed: false,
                indent: 0,
                badge: "",
                badge_color: None,
                collapsible: false,
                underline_last_desc: false,
            })
        })
        .collect();
    picker::render_picker_in_modal(
        buf,
        picker_area,
        mca.inner_x,
        mca.inner_width,
        theme,
        state,
        &picker_entries,
        &non_sel,
        false,
    );
}
/// Render a DocViewer overlay: modal window chrome + cached markdown content.
#[allow(clippy::too_many_arguments)]
pub fn render_doc_viewer_overlay(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    window: &mut super::modal_window::ModalWindowState,
    title: &str,
    content: &str,
    scroll: &mut u16,
    cached_lines: &mut Option<(u16, Vec<ratatui::text::Line<'static>>)>,
    compact: bool,
    theme: &Theme,
) {
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    let doc_shortcuts = vec![
        super::modal_window::Shortcut {
            label: "\u{2191}/\u{2193} scroll",
            clickable: false,
            id: 0,
        },
        super::modal_window::Shortcut {
            label: "Esc back",
            clickable: false,
            id: 0,
        },
    ];
    let modal_config = super::modal_window::ModalWindowConfig {
        title,
        tabs: None,
        shortcuts: &doc_shortcuts,
        sizing: super::modal_window::ModalSizing {
            width_pct: 0.80,
            max_width: 120,
            min_width: 44,
            v_margin: 4,
            h_pad: 2,
            v_pad: 1,
            footer_lines: 2,
        }
        .with_compact(compact),
        fold_info: None,
    };
    if let Some(super::modal_window::ModalContentArea {
        content: content_area,
        ..
    }) = super::modal_window::render_modal_window(buf, area, window, &modal_config, theme)
    {
        let w = content_area.width;
        let needs_reparse = cached_lines
            .as_ref()
            .is_none_or(|(cached_w, _)| *cached_w != w);
        if needs_reparse {
            let mc = crate::scrollback::blocks::markdown_content::MarkdownContent::new(content);
            let output = mc.output(w as usize);
            let lines: Vec<ratatui::text::Line<'static>> =
                output.lines.into_iter().map(|b| b.content).collect();
            *cached_lines = Some((w, lines));
        }
        let all_lines = &cached_lines.as_ref().unwrap().1;
        let max_scroll = all_lines.len().saturating_sub(content_area.height as usize);
        *scroll = (*scroll as usize).min(max_scroll) as u16;
        let start = *scroll as usize;
        let visible: Vec<ratatui::text::Line> = all_lines
            .iter()
            .skip(start)
            .take(content_area.height as usize)
            .cloned()
            .collect();
        let para = Paragraph::new(visible).wrap(Wrap { trim: false });
        para.render(content_area, buf);
    }
}
#[cfg(test)]
mod doc_viewer_scroll_tests {
    use super::{apply_doc_mouse_scroll, apply_doc_scroll, apply_doc_scroll_delta};
    use crossterm::event::{KeyCode, MouseEventKind};
    #[test]
    fn apply_doc_scroll_moves_by_key() {
        let mut scroll = 10u16;
        assert!(apply_doc_scroll(KeyCode::Down, &mut scroll));
        assert_eq!(scroll, 13);
        assert!(apply_doc_scroll(KeyCode::Up, &mut scroll));
        assert_eq!(scroll, 10);
        assert!(apply_doc_scroll(KeyCode::Home, &mut scroll));
        assert_eq!(scroll, 0);
        assert!(apply_doc_scroll(KeyCode::End, &mut scroll));
        assert_eq!(scroll, u16::MAX);
    }
    #[test]
    fn apply_doc_scroll_delta_saturates_at_zero() {
        let mut scroll = 2u16;
        apply_doc_scroll_delta(&mut scroll, -10);
        assert_eq!(scroll, 0);
        apply_doc_scroll_delta(&mut scroll, 4);
        assert_eq!(scroll, 4);
    }
    #[test]
    fn apply_doc_mouse_scroll_handles_wheel() {
        let mut scroll = 0u16;
        assert!(apply_doc_mouse_scroll(
            MouseEventKind::ScrollDown,
            &mut scroll
        ));
        assert_eq!(scroll, 3);
        assert!(apply_doc_mouse_scroll(
            MouseEventKind::ScrollUp,
            &mut scroll
        ));
        assert_eq!(scroll, 0);
        assert!(!apply_doc_mouse_scroll(MouseEventKind::Moved, &mut scroll));
    }
}
#[cfg(test)]
mod palette_sharing_tests {
    use super::*;
    fn has_share(entries: &[PaletteEntry]) -> bool {
        entries
            .iter()
            .any(|e| matches!(&e.command, PaletteCommand::SlashCommand(s) if s.trim() == "/share"))
    }
    #[test]
    fn default_palette_includes_share_when_enabled() {
        let entries = default_palette_entries(true, crate::app::ScreenMode::Fullscreen);
        assert!(
            has_share(&entries),
            "/share should be present when sharing_enabled=true"
        );
    }
    #[test]
    fn default_palette_includes_dashboard() {
        let entries = default_palette_entries(true, crate::app::ScreenMode::Fullscreen);
        let has_dashboard = entries.iter().any(
            |e| matches!(&e.command, PaletteCommand::SlashCommand(s) if s.trim() == "/dashboard"),
        );
        assert!(
            has_dashboard,
            "/dashboard entry must be present in the palette so users can switch between agents"
        );
        let labelled = entries.iter().any(|e| e.label == "Agent Dashboard");
        assert!(
            labelled,
            "palette entry must use the 'Agent Dashboard' label"
        );
    }
    #[test]
    fn edit_prompt_palette_entry_is_minimal_only() {
        let minimal = default_palette_entries(true, crate::app::ScreenMode::Minimal);
        assert!(
            minimal
                .iter()
                .any(|entry| matches!(entry.command, PaletteCommand::EditPromptExternal))
        );
        let fullscreen = default_palette_entries(true, crate::app::ScreenMode::Fullscreen);
        assert!(
            !fullscreen
                .iter()
                .any(|entry| matches!(entry.command, PaletteCommand::EditPromptExternal))
        );
    }
    #[test]
    fn default_palette_omits_share_when_disabled() {
        let entries = default_palette_entries(false, crate::app::ScreenMode::Fullscreen);
        assert!(
            !has_share(&entries),
            "/share must not appear in palette when sharing_enabled=false"
        );
    }
    #[test]
    fn filter_palette_omits_share_when_disabled() {
        let entries = filter_palette_entries("", false, crate::app::ScreenMode::Fullscreen);
        assert!(
            !has_share(&entries),
            "/share must not appear in unfiltered palette when sharing_enabled=false"
        );
        let entries = filter_palette_entries("share", false, crate::app::ScreenMode::Fullscreen);
        assert!(
            !has_share(&entries),
            "/share must not appear when filtering for 'share' with sharing_enabled=false"
        );
    }
    #[test]
    fn filter_palette_includes_share_when_enabled_and_matched() {
        let entries = filter_palette_entries("share", true, crate::app::ScreenMode::Fullscreen);
        assert!(
            has_share(&entries),
            "/share should match a 'share' query when sharing_enabled=true"
        );
    }
    #[test]
    fn palette_tools_section_routes_each_tab_to_itself() {
        use crate::views::extensions_modal::ExtensionsTab;
        let entries = default_palette_entries(true, crate::app::ScreenMode::Fullscreen);
        for (label, expected) in [
            ("Hooks", ExtensionsTab::Hooks),
            ("Plugins", ExtensionsTab::Plugins),
            ("Marketplace", ExtensionsTab::Marketplace),
            ("Skills", ExtensionsTab::Skills),
            ("MCP Servers", ExtensionsTab::McpServers),
        ] {
            let entry = entries
                .iter()
                .find(|e| e.label == label)
                .unwrap_or_else(|| panic!("Tools entry {label:?} missing from palette"));
            assert!(
                matches!(
                    &entry.command,
                    PaletteCommand::OpenExtensionsTab(t) if *t == expected,
                ),
                "Tools entry {label:?} dispatches to the wrong tab",
            );
        }
    }
    #[test]
    fn howto_list_modal_opens_on_first_guide() {
        let modal = howto_list_modal(None);
        let ActiveModal::DocPicker { state, entries, .. } = modal else {
            panic!("howto_list_modal should build a DocPicker");
        };
        assert!(!state.search_active, "how-to picker must open list-focused");
        assert_eq!(state.selected, 0);
        assert_eq!(
            entries.first().map(|e| e.title.as_str()),
            Some("Getting Started")
        );
    }
}
#[cfg(test)]
mod doc_picker_tip_tests {
    use super::{
        ActiveModal, DOCS_USER_GUIDE_REL, fit_docs_ask_grok_tip, howto_list_modal,
        render_doc_picker_overlay,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use unicode_width::UnicodeWidthStr;
    #[test]
    fn fit_docs_tip_prefers_path_and_never_overflows() {
        let path = crate::util::display_user_grok_path(DOCS_USER_GUIDE_REL);
        let long =
            format!("Tip · Ask Grok about the docs ({path}) — e.g. \"how do I set up MCP?\"");
        let short = format!("Tip · Ask Grok about the docs · {path}");
        let path_only = format!("Tip · {path}");
        assert_eq!(fit_docs_ask_grok_tip(&path, long.width()), long);
        assert_eq!(fit_docs_ask_grok_tip(&path, short.width()), short);
        assert_eq!(fit_docs_ask_grok_tip(&path, path_only.width()), path_only);
        for w in [5usize, 12, 20, 30] {
            let line = fit_docs_ask_grok_tip(&path, w);
            assert!(line.width() <= w, "overflow at {w}: {line:?}");
        }
    }
    #[test]
    fn doc_picker_renders_tip_with_path() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let mut modal = howto_list_modal(None);
        let ActiveModal::DocPicker {
            entries,
            state,
            window,
            ..
        } = &mut modal
        else {
            panic!("expected DocPicker");
        };
        let theme = crate::theme::Theme::current();
        let mut buf = Buffer::empty(area);
        render_doc_picker_overlay(&mut buf, area, window, entries, state, false, &theme);
        let mut all = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    all.push_str(cell.symbol());
                }
            }
            all.push('\n');
        }
        assert!(
            all.contains("Tip") && all.contains("Ask Grok"),
            "missing tip footer:\n{all}"
        );
        assert!(
            all.contains("docs/user-guide") || all.contains("docs\\user-guide"),
            "missing docs path:\n{all}"
        );
    }
}
