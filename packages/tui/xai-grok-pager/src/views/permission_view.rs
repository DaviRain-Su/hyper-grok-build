//! Permission view state and helpers.
//!
//! When the agent requests a permission (bash, edit, MCP tool, etc.), the pager
//! takes over the prompt area and shows a structured permission UI. This module
//! contains:
//!
//! - [`PermissionViewState`] — per-request state for the permission overlay
//! - [`PermissionFocus`] — options vs followup-input mode
//!
//! The pager maintains a `VecDeque<PermissionViewState>` on [`AgentView`].
//! Only the **front** request is rendered and interactive — subsequent requests
//! wait in the queue. This matches the TUI's `VecDeque<PermissionRequest>`
//! queueing semantics and prevents cancellation of older requests when newer
//! ones arrive.
//!
//! No rendering or input handling here — this is pure data and helpers.

use agent_client_protocol as acp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use xai_grok_workspace::permission::bash_command_splitting::{
    BashCommandHighlights, heredoc_payload_byte_ranges, range_fully_inside,
    soft_break_offsets_after_operators, split_physical_line_at_soft_breaks,
};
use xai_grok_workspace::permission::{
    BashCommandPermission, McpToolPermission, mcp_titleize_segment, mcp_tool_action,
    mcp_tool_display_name,
};

use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

// ── Enums ──────────────────────────────────────────────────────────────

/// Interaction mode for the permission overlay.
///
/// Mirrors [`QuestionFocus`](crate::views::question_view::QuestionFocus) from
/// `question_view.rs`. Even though `PromptWidget` owns the text editing state,
/// the permission overlay needs its own mode enum so that input routing,
/// rendering, and Esc behavior have a single source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionFocus {
    /// Cursor is on an option row. j/k navigate, Enter or 1-N select
    /// the option at that 1-based index.
    /// Left/Right (or `<`/`>`) expand/contract bash selection and jump the
    /// cursor to AllowAlways unless it already sits on a scoped
    /// (AllowAlways/RejectAlways) row. Ctrl-C cancels.
    Options,
    /// User is typing a followup message in the PromptWidget.
    /// Entered by pressing Enter on the RejectOnce option (or `x` shortcut).
    /// Esc exits back to Options (prompt text is preserved).
    /// Enter submits the followup message.
    FollowupInput,
    /// User is editing a free-form "Always allow" command pattern (a glob).
    /// Entered with `e` on a bash prompt; the buffer is [`PatternEditState`].
    /// Esc discards it and returns to Options; Enter persists the pattern.
    PatternEdit,
}

/// Single-line editor buffer for a free-form "Always allow" command pattern.
///
/// `cursor` is a byte offset into `buffer`, kept on a `char` boundary by every
/// mutation so slicing is always valid. Content mutations set `dirty`; cursor
/// moves do not. A confirmed grant is a glob only when dirty — unedited save
/// is a literal prefix of the pre-filled command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatternEditState {
    pub buffer: String,
    pub cursor: usize,
    /// True after any content mutation (insert/delete/clear). Routes the grant
    /// to `allowed_bash_globs` when the pattern is confirmed.
    dirty: bool,
}

impl PatternEditState {
    /// Start editing `initial` with the cursor at the end (clean).
    pub fn new(initial: impl Into<String>) -> Self {
        let buffer = initial.into();
        let cursor = buffer.len();
        Self {
            buffer,
            cursor,
            dirty: false,
        }
    }

    /// Whether the user has mutated the buffer since open.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The trimmed pattern to persist, or `None` when blank.
    pub fn trimmed(&self) -> Option<&str> {
        let t = self.buffer.trim();
        (!t.is_empty()).then_some(t)
    }

    pub fn insert_char(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.dirty = true;
    }

    pub fn backspace(&mut self) {
        if let Some(ch) = self.buffer[..self.cursor].chars().next_back() {
            self.cursor -= ch.len_utf8();
            self.buffer.remove(self.cursor);
            self.dirty = true;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
            self.dirty = true;
        }
    }

    pub fn move_left(&mut self) {
        if let Some(ch) = self.buffer[..self.cursor].chars().next_back() {
            self.cursor -= ch.len_utf8();
        }
    }

    pub fn move_right(&mut self) {
        if let Some(ch) = self.buffer[self.cursor..].chars().next() {
            self.cursor += ch.len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    pub fn clear(&mut self) {
        if !self.buffer.is_empty() {
            self.dirty = true;
        }
        self.buffer.clear();
        self.cursor = 0;
    }
}

/// Currently selected scope for an MCP "Always allow" prompt.
///
/// `Tool` whitelists exactly the named tool (smaller blast radius and the
/// default). `Server` whitelists every tool whose name starts with
/// `<server>__`, and is only reachable when the tool name actually has a
/// `__` separator (i.e. `McpScopeState::server_prefix.is_some()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpScope {
    Tool,
    Server,
}

/// Per-prompt MCP scope toggle state. Populated by `acp_handler` when the
/// request meta deserializes as [`McpToolPermission`]; `None` for non-MCP
/// prompts. The pager flips `selected` in response to ← / → arrow keys.
#[derive(Debug, Clone)]
pub struct McpScopeState {
    /// Full tool name (e.g. `"linear__list_issues"`).
    pub tool_name: String,
    /// Server segment (everything before the single `__` separator).
    /// `None` when the tool name has no `__`; the toggle is hidden in
    /// that case and only tool-scope is offered.
    pub server_prefix: Option<String>,
    /// Currently selected scope. Defaults to `Tool` on prompt entry.
    pub selected: McpScope,
}

impl McpScopeState {
    /// Action segment of the qualified tool name. See
    /// [`mcp_tool_action`].
    pub fn action(&self) -> &str {
        mcp_tool_action(&self.tool_name, self.server_prefix.as_deref())
    }

    /// User-facing tool label. See [`mcp_tool_display_name`].
    pub fn display_name(&self) -> String {
        mcp_tool_display_name(&self.tool_name, self.server_prefix.as_deref())
    }
}

// ── State ──────────────────────────────────────────────────────────────

/// A queued permission request awaiting user response.
///
/// The pager maintains a `VecDeque` of these on `AgentView`. Only the front
/// request is rendered and interactive — subsequent requests wait in the queue.
///
/// Not `Clone` because it owns the `response_tx` oneshot sender via
/// `xai_acp_lib::AcpArgs`.
pub struct PermissionViewState {
    /// The ACP permission request args (holds `response_tx` for sending
    /// the response back to the shell).
    pub request: xai_acp_lib::AcpArgs<acp::RequestPermissionRequest>,

    /// Unique ID for this request (monotonic counter, same as TUI's
    /// `perm_req_id`). Used to guard against stale resolution attempts.
    pub id: usize,

    // -- Interaction mode --
    /// Current focus mode. Determines input routing and rendering.
    pub focus: PermissionFocus,

    // -- Options --
    /// All permission options from the request (cloned from
    /// `request.options` so the request can be moved into the struct).
    pub options: Vec<acp::PermissionOption>,

    /// Currently focused option index (only meaningful for the front request).
    pub active_idx: usize,

    // -- Bash command selection --
    /// Parsed bash highlights from request meta (None for non-bash
    /// permissions). Imported from `xai-grok-shell`, NOT duplicated locally.
    pub bash_highlights: Option<BashCommandHighlights>,

    /// How many highlighted words are currently selected (1-indexed).
    /// Starts at `default_always_allow_scope(highlighted_words)`: the safe
    /// prefix for safe-listed commands, else first two words plus flags.
    /// Right-arrow (or `>`) expands, Left-arrow (or `<`) contracts, minimum 1.
    pub bash_selection_count: usize,

    /// Raw bash command string for display when `bash_highlights` is `None`
    /// (complex commands that tree-sitter cannot decompose).
    pub bash_command_raw: Option<String>,

    // -- MCP scope selection --
    /// MCP scope toggle state. `None` for non-MCP prompts. Populated when the
    /// request carries an `allow-always-mcp` option whose meta deserializes
    /// as `McpToolPermission`. Mutually exclusive with the bash flow at the
    /// per-request level.
    pub mcp_scope: Option<McpScopeState>,

    // -- Display content (precomputed on creation) --
    /// Title text (e.g. agent-provided bash description, or "Allow Edit?").
    pub title: String,

    /// Planned tool-input lines shown under the title — for MCP tools the
    /// pretty-printed JSON arguments the call would send (built by
    /// `acp_handler::build_permission_display`). Empty for bash/edit
    /// prompts, which have dedicated displays.
    pub description: Vec<String>,

    /// Whether the planned-args display is expanded (Ctrl-F toggle).
    /// Collapsed caps the args at [`MCP_ARGS_COLLAPSED_ROWS`] rows with a
    /// `... Ctrl-F to expand` indicator.
    pub args_expanded: bool,

    /// Scroll offset for description area.
    pub desc_scroll: u16,

    // -- Subagent provenance --
    /// If this permission was requested by a subagent, its descriptive label.
    /// Derived from matching `request.session_id` against known subagent
    /// sessions. Displayed as a provenance line above the title.
    pub subagent_label: Option<String>,

    // -- Prompt stash (queue-level, not per-request) --
    // NOTE: prompt stash is NOT on PermissionViewState.
    // It lives on AgentView as `permission_stashed_prompt`.
    // See the "Queue-level prompt stashing" section in the plan.

    // -- Layout cache --
    /// Cached options area height (for scroll calculations).
    pub options_area_height: usize,

    /// Scroll offset for options list (when there are more options than
    /// fit in the visible area).
    pub options_scroll_offset: usize,
}

impl PermissionViewState {
    /// Whether the scope selector (← → arrows) is meaningful for this prompt.
    ///
    /// True when:
    /// - bash: there are 2+ highlighted words to expand/contract between, OR
    /// - MCP: the tool name has a `__` separator, so server-scope is reachable.
    pub fn has_adjustable_scope(&self) -> bool {
        self.bash_highlights
            .as_ref()
            .is_some_and(|h| h.highlighted_words.len() > 1)
            || self
                .mcp_scope
                .as_ref()
                .is_some_and(|s| s.server_prefix.is_some())
    }

    /// Whether this prompt offers the free-form bash pattern editor (`e`): a
    /// bash command with an `AllowAlways` row to persist the pattern to. The
    /// height reservation, the render/hint gates, and the `e` key handler must
    /// all use this so they cannot drift (a stale copy would mis-size the
    /// overlay or advertise a key that does nothing).
    pub fn has_editable_bash_pattern(&self) -> bool {
        self.bash_highlights.is_some()
            && self
                .options
                .iter()
                .any(|o| o.kind == acp::PermissionOptionKind::AllowAlways)
    }
}

/// 1-based shortcut character for the given 0-based option index.
/// Returns `' '` for indices >= 9 (we never expect that many options).
fn shortcut_char(index: usize) -> char {
    if index < 9 {
        char::from(b'1' + index as u8)
    } else {
        ' '
    }
}

/// Pre-formatted shortcut labels to avoid per-frame `format!` allocation.
const SHORTCUT_LABELS: [&str; 10] = ["  ", "1 ", "2 ", "3 ", "4 ", "5 ", "6 ", "7 ", "8 ", "9 "];

fn shortcut_label(index: usize) -> &'static str {
    SHORTCUT_LABELS
        .get(index + 1)
        .copied()
        .unwrap_or(SHORTCUT_LABELS[0])
}

// ── Subagent tracking ──────────────────────────────────────────────────

// SubagentInfo lives in app::subagent — re-export for backward compat.
pub use crate::app::subagent::SubagentInfo;

// ── Height calculation ─────────────────────────────────────────────────

/// Chrome height for the permission view as actually rendered.
///
/// Public version for mouse hit-testing in agent_view. Takes `area_h`
/// so the returned value matches the rendering: when the area is too
/// small for all bash lines, they get clipped, and options start earlier.
pub fn permission_chrome_height_pub(
    state: &PermissionViewState,
    content_w: usize,
    area_h: u16,
) -> u16 {
    let uncapped = permission_chrome_height(state, content_w);
    // The rendering draws chrome then options sequentially, clipping at
    // area_h. So options start at min(uncapped_chrome, area_h - options - vpad_bottom).
    let options_and_pad = state.options.len() as u16 + 1;
    let max_chrome = area_h.saturating_sub(options_and_pad);
    uncapped.min(max_chrome)
}

/// Chrome height for the permission view (provenance + title + bash command
/// + planned MCP arguments + inline scope hint + gap).
///
/// Returns the uncapped chrome height. The caller is responsible for
/// applying a height cap to the overall permission view.
fn permission_chrome_height(state: &PermissionViewState, content_w: usize) -> u16 {
    let bash_line_count = bash_display_line_count(state, content_w) as u16;
    let mut h: u16 = 1; // vpad top
    if state.subagent_label.is_some() {
        h += 1; // provenance line
    }
    h += 1; // title line
    h += bash_line_count;
    // Planned MCP arguments: same `mcp_args_visible_rows` budget as the
    // render. Clamp before the cast (`as u16` wraps) and saturate the adds
    // so a pathological count can't overflow-panic in debug builds.
    let (args_rows, indicator) = mcp_args_visible_rows(state, content_w);
    let args_rows = args_rows
        .saturating_add(indicator as usize)
        .min(u16::MAX as usize) as u16;
    h = h.saturating_add(args_rows);
    // Rows reserved for the hint / edit controls; must match the render below:
    // two while editing (field + preview), else one when arrows or `e` show.
    if state.focus == PermissionFocus::PatternEdit {
        h = h.saturating_add(2);
    } else if state.has_adjustable_scope() || state.has_editable_bash_pattern() {
        h = h.saturating_add(1);
    }
    h.saturating_add(1) // gap before options
}

/// Compute the total height the permission view should occupy.
///
/// Caps at 50% of screen height (min 10, max 80%). The minimum ensures
/// at least a couple of bash command lines are visible alongside the
/// option rows. An expanded planned-args display (Ctrl-F) lifts the cap
/// to the full screen height.
pub fn permission_view_height(state: &PermissionViewState, screen_h: u16, content_w: usize) -> u16 {
    let chrome_h = permission_chrome_height(state, content_w);
    let options_h = state.options.len() as u16;
    let vpad_bottom: u16 = 1;
    let total = chrome_h
        .saturating_add(options_h)
        .saturating_add(vpad_bottom);

    if state.args_expanded {
        return total.min(screen_h);
    }
    let cap = (screen_h as u32 / 2)
        .max(10)
        .min(screen_h as u32 * 80 / 100) as u16;
    total.min(cap)
}

/// Collapsed row budget for the planned-args display, matching the
/// question tool's `DEFAULT_MAX_CHROME_DESC_LINES`. When truncated, the
/// last budgeted row is the `... Ctrl-F to expand` indicator.
pub const MCP_ARGS_COLLAPSED_ROWS: usize = 5;

/// Rows the planned-args display occupies: `(content_rows, show_indicator)`.
///
/// The one row-budget source shared by chrome height, render, and mouse
/// hit-testing. Counts plain text (no syntect on the hit-test path);
/// highlighting preserves text, so the styled render wraps identically.
/// The budget (>= 2) always fits content rows plus the indicator.
fn mcp_args_visible_rows(state: &PermissionViewState, content_w: usize) -> (usize, bool) {
    let total: usize = state
        .description
        .iter()
        .map(|raw| char_wrap_row_count(raw, content_w))
        .sum();
    if !state.args_expanded && total > MCP_ARGS_COLLAPSED_ROWS {
        (MCP_ARGS_COLLAPSED_ROWS - 1, true)
    } else {
        (total, false)
    }
}

/// Row count [`char_wrap`] would produce, without allocating the chunks.
/// Same break arithmetic; called per frame from the height/hit-test paths.
fn char_wrap_row_count(s: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 1usize;
    let mut cur_w = 0usize;
    let mut cur_empty = true;
    for ch in s.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + ch_w > width && !cur_empty {
            rows += 1;
            cur_w = 0;
        }
        cur_w += ch_w;
        // The breaking char starts the new row, as in `char_wrap`.
        cur_empty = false;
    }
    rows
}

/// Number of wrapped lines the bash/MCP-name display needs (uncapped).
fn bash_display_line_count(state: &PermissionViewState, content_w: usize) -> usize {
    if state.bash_highlights.is_some() || state.bash_command_raw.is_some() {
        build_permission_bash_lines(
            state.bash_highlights.as_ref(),
            state.bash_selection_count,
            state.bash_command_raw.as_deref(),
            content_w,
        )
        .len()
    } else if let Some(ref scope) = state.mcp_scope {
        // MCP scope renders as a single line. Themes may differ, but we
        // know it doesn't wrap because we elide width separately.
        let _ = content_w;
        let _ = scope;
        1
    } else {
        0
    }
}

// ── Rendering ──────────────────────────────────────────────────────────

fn hovered_bg(theme: &Theme) -> ratatui::style::Color {
    theme.bg_hover
}

/// Result from rendering the permission view, telling the caller where
/// to render the inline prompt widget (if in FollowupInput mode).
pub struct PermissionRenderResult {
    /// When in FollowupInput mode, the Y position and content_x/width
    /// where the inline prompt should be rendered (after the prefix).
    /// `None` when in Options mode (no inline prompt needed).
    pub inline_prompt: Option<InlinePromptArea>,
}

/// Layout info for the inline followup prompt.
pub struct InlinePromptArea {
    /// X position for the prompt widget text (after "x [x] ❯ " prefix).
    pub text_x: u16,
    /// Y position of the row.
    pub y: u16,
    /// Width available for the prompt widget text.
    pub text_w: u16,
    /// X position of the content area (for prefix rendering).
    pub content_x: u16,
    /// Full width of the content area.
    pub content_w: u16,
}

/// Width available for inline prompt text given the full area width.
///
/// Subtracts left padding (accent col + 2 = 3) and the followup prefix
/// (`"<n> (●) ❯ "` = 8 chars). Matches the `text_w` computed during
/// rendering so `desired_height` wraps at the same width as the draw area.
pub fn inline_text_width(area_width: u16) -> u16 {
    const LEFT_PAD: u16 = 3; // accent column + 2 padding
    const PREFIX_W: u16 = 8; // "x (●) ❯ " = 2 + 4 + 2
    area_width.saturating_sub(LEFT_PAD + PREFIX_W)
}

/// Render the complete permission view into the given area.
///
/// Mirrors `render_question_view`: bg_light background, accent `┃` line,
/// chrome header (provenance + title + bash command), option rows with
/// cursor/hover highlighting, shortcut labels.
///
/// In FollowupInput mode, the RejectOnce static row is skipped and the
/// returned `PermissionRenderResult` tells the caller where to render the
/// inline prompt widget (matching Q/A panel's InputMode pattern).
pub fn render_permission_view(
    buf: &mut Buffer,
    area: Rect,
    state: &PermissionViewState,
    followup_text: &str,
    pattern_edit: Option<&PatternEditState>,
    hovered_item: Option<usize>,
    theme: &Theme,
    focused: bool,
) -> PermissionRenderResult {
    if area.height == 0 || area.width == 0 {
        return PermissionRenderResult {
            inline_prompt: None,
        };
    }

    let is_followup = state.focus == PermissionFocus::FollowupInput;
    // Editor is only active while focus is PatternEdit *and* the buffer exists.
    let pattern_edit = pattern_edit.filter(|_| state.focus == PermissionFocus::PatternEdit);

    // Fill background — same as the focused prompt (bg_light).
    let bg = Style::default().bg(theme.bg_light);
    buf.set_style(area, bg);

    // Accent line ┃ on the left column — blue to match the shortcut key color.
    let accent_style = Style::default().fg(theme.accent_user);
    for row in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, row)) {
            cell.set_symbol(crate::glyphs::accent_bar()); // ┃
            cell.set_style(accent_style);
        }
    }

    // Content area (left: accent + 2-char pad, right: 2-char pad)
    let content_x = area.x + 3;
    let content_width = area.width.saturating_sub(5);
    let mut y = area.y;

    // Vertical padding at the top.
    y += 1;

    // ── Chrome header ──

    // Bottom of the drawable area. The chrome rows below are written at
    // increasing `y`; when the overlay is squeezed into a 1-2 row area at the
    // bottom of a short terminal they must not write past it (ratatui's
    // set_line panics on an out-of-bounds row).
    let area_bottom = area.y + area.height;

    // Subagent provenance line (if present).
    if let Some(ref label) = state.subagent_label {
        if y < area_bottom {
            let prov_style = Style::default().fg(theme.gray);
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(label.clone(), prov_style)),
                content_width,
            );
        }
        y += 1;
    }

    // Title (bold, accent color) — e.g. bash tool description or "Allow Edit?"
    if y < area_bottom {
        let title_style = Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD);
        buf.set_line(
            content_x,
            y,
            &Line::from(Span::styled(state.title.clone(), title_style)),
            content_width,
        );
    }
    y += 1;

    // Bash command / MCP tool name display: syntax-highlighted and
    // carefully soft-wrapped. Cap the number of lines so the option rows
    // always remain visible. Reserve: gap(1) + options + vpad_bottom(1).
    let mut bash_lines: Vec<Line<'_>> =
        if state.bash_highlights.is_some() || state.bash_command_raw.is_some() {
            build_permission_bash_lines(
                state.bash_highlights.as_ref(),
                state.bash_selection_count,
                state.bash_command_raw.as_deref(),
                content_width as usize,
            )
        } else if let Some(ref scope) = state.mcp_scope {
            build_mcp_scope_lines(scope, theme, content_width as usize)
        } else {
            Vec::new()
        };
    // Planned MCP arguments, appended to the same vec so the options
    // visibility cap and trailing ellipsis apply. The row budget is shared
    // with `permission_chrome_height` via `mcp_args_visible_rows`.
    {
        let (args_rows, indicator) = mcp_args_visible_rows(state, content_width as usize);
        bash_lines.extend(build_mcp_args_lines(
            &state.description,
            theme,
            content_width as usize,
            args_rows,
        ));
        if indicator {
            bash_lines.push(truncation_indicator_line(theme));
        }
    }

    let show_scope_hint = state.has_adjustable_scope();
    // Editing needs two rows (field + preview); otherwise one hint row when the
    // arrows or the `e` editor affordance is available.
    let show_edit_hint = state.has_editable_bash_pattern();
    let header_extra_h: u16 = if pattern_edit.is_some() {
        2
    } else if show_scope_hint || show_edit_hint {
        1
    } else {
        0
    };
    let options_reserve = header_extra_h + 1 + state.options.len() as u16 + 1;
    let max_bash_y = (area.y + area.height).saturating_sub(options_reserve);

    let mut last_drawn_bash: Option<usize> = None;
    for (li, bash_line) in bash_lines.iter().enumerate() {
        if y >= max_bash_y {
            break;
        }
        buf.set_line(content_x, y, bash_line, content_width);
        last_drawn_bash = Some(li);
        y += 1;
    }
    if let Some(last_idx) = last_drawn_bash
        && last_idx + 1 < bash_lines.len()
    {
        let text_w = bash_lines[last_idx].width() as u16;
        let ellipsis_x = content_x + text_w.min(content_width.saturating_sub(2));
        let ellipsis_style = Style::default().fg(theme.gray);
        buf.set_span(
            ellipsis_x,
            y - 1,
            &Span::styled(" \u{2026}", ellipsis_style),
            2,
        );
    }
    if let Some(edit) = pattern_edit {
        // ── Free-form pattern editor (two rows) ──
        if y < area.y + area.height {
            render_pattern_editor_line(buf, content_x, y, content_width, edit, theme);
            y += 1;
        }
        if y < area.y + area.height {
            let command = preview_command_text(state);
            render_pattern_preview_line(buf, content_x, y, content_width, edit, &command, theme);
            y += 1;
        }
    } else if (show_scope_hint || show_edit_hint) && y < area.y + area.height {
        // Readable secondary text (accent-highlighted keys). Advertise the
        // arrows only when there's a scope to move between, but always offer
        // `e edit` on a bash prompt so the free-form option is discoverable.
        let hint_style = Style::default()
            .fg(theme.text_secondary)
            .add_modifier(Modifier::DIM);
        let key_style = Style::default().fg(theme.accent_user);
        let mut spans: Vec<Span<'static>> = Vec::new();
        if show_scope_hint {
            spans.push(Span::styled(
                rust_i18n::t!("permission.scope_hint_a"),
                hint_style,
            ));
            spans.push(Span::styled("\u{2190} \u{2192}", key_style));
            spans.push(Span::styled(
                rust_i18n::t!("permission.scope_hint_b"),
                hint_style,
            ));
        }
        if show_edit_hint {
            if show_scope_hint {
                spans.push(Span::styled("  \u{00b7}  ", hint_style));
            }
            spans.push(Span::styled("e", key_style));
            spans.push(Span::styled(" edit pattern", hint_style));
        }
        buf.set_line(content_x, y, &Line::from(spans), content_width);
        y += 1;
    }

    // Gap before options.
    y += 1;

    // ── Option rows ──
    let visible_bottom = area.y + area.height;
    let hover_bg = hovered_bg(theme);

    // Precompute the selected words string for dynamic labels.
    let selected_words: Option<String> = state
        .bash_highlights
        .as_ref()
        .map(|h| h.highlighted_words[..state.bash_selection_count].join(" "));

    let mut inline_prompt_result: Option<InlinePromptArea> = None;

    for (i, option) in state.options.iter().enumerate() {
        if y >= visible_bottom {
            break;
        }

        // In FollowupInput mode, skip the RejectOnce static row —
        // the caller will render the inline prompt widget at this position.
        if is_followup && option.kind == acp::PermissionOptionKind::RejectOnce {
            let row_bg = theme.bg_visual; // always focused bg for the input row

            // Fill the FULL row width including padding between accent ┃ and content.
            let full_row = Rect {
                x: area.x + 1, // after the accent symbol
                y,
                width: area.width.saturating_sub(1),
                height: 1,
            };
            buf.set_style(full_row, Style::default().bg(row_bg));

            // Re-draw accent ┃ with the row bg so it blends.
            if let Some(cell) = buf.cell_mut((area.x, y)) {
                cell.set_symbol(crate::glyphs::accent_bar());
                cell.set_style(Style::default().fg(theme.accent_user).bg(row_bg));
            }

            // Render the "<n> (●) ❯ " prefix manually (same as Q/A panel).
            // Use the 1-based option index so the shortcut number shown
            // here matches what the user types to invoke RejectOnce.
            let num_style = Style::default().fg(theme.accent_user).bg(row_bg);
            let marker_style = Style::default()
                .fg(theme.text_primary)
                .bg(row_bg)
                .add_modifier(Modifier::BOLD);
            let prompt_ind = Style::default().fg(theme.accent_user).bg(row_bg);
            buf.set_span(content_x, y, &Span::styled(shortcut_label(i), num_style), 2);
            buf.set_span(
                content_x + 2,
                y,
                &Span::styled(format!("({}) ", crate::glyphs::filled_dot()), marker_style),
                4,
            );
            buf.set_span(
                content_x + 6,
                y,
                &Span::styled(crate::glyphs::prompt_arrow(), prompt_ind),
                2,
            );

            // Tell the caller where to render the prompt widget text.
            // Use full width to the right edge (not the 2-col-padded content_width)
            // so the scrollbar sits flush against the border — matching Q/A panel.
            let prefix_w: u16 = 8; // "x (●) ❯ " = 2 + 4 + 2 = 8
            let full_w = area.width.saturating_sub(3); // only left padding (accent + 2)
            inline_prompt_result = Some(InlinePromptArea {
                text_x: content_x + prefix_w,
                y,
                text_w: full_w.saturating_sub(prefix_w),
                content_x,
                content_w: full_w,
            });

            y += 1;
            continue;
        }

        let is_cursor = i == state.active_idx;
        let is_hovered = hovered_item == Some(i);
        // When the panel is unfocused, drop the cursor-row bg so it
        // reads as "no active selection" — same rule as question_view.
        let row_bg = if is_cursor && focused {
            theme.bg_visual
        } else if is_hovered {
            hover_bg
        } else {
            theme.bg_light
        };

        let line = build_permission_option_line(
            option,
            i,
            is_cursor,
            row_bg,
            selected_words.as_deref(),
            state.mcp_scope.as_ref(),
            followup_text,
            content_width,
            theme,
        );

        let row_rect = Rect {
            x: content_x,
            y,
            width: content_width,
            height: 1,
        };
        buf.set_style(row_rect, Style::default().bg(row_bg));
        buf.set_line(content_x, y, &line, content_width);
        y += 1;
    }

    // Unfocus dim: when the prompt area is unfocused (e.g. user moved
    // to scrollback), blend foregrounds toward `bg_light` so the panel
    // visually recedes. Mirrors the unfocused prompt widget pattern
    // (`prompt_widget.rs:1948`) and `render_question_view`.
    if !focused {
        crate::render::color::blend_area(buf, area, Some((theme.bg_light, 0.66)), None);
    }

    PermissionRenderResult {
        inline_prompt: inline_prompt_result,
    }
}

/// The primary command text the session enforcer matches a bash grant against:
/// the primary segment's words with wrappers (`timeout`/`nice`/`env`) peeled.
/// Shared by the pattern editor's pre-fill and its live match preview so both
/// agree with enforcement. Falls back to the raw command when untokenized.
pub(crate) fn preview_command_text(state: &PermissionViewState) -> String {
    match state.bash_highlights.as_ref() {
        Some(h) => xai_grok_workspace::permission::bash_command_splitting::unwrap_command_wrappers(
            &h.highlighted_words,
        )
        .join(" "),
        None => state.bash_command_raw.clone().unwrap_or_default(),
    }
}

/// Draw the single-line free-form pattern editor: an `❯ ` prompt followed by
/// the buffer text with a block caret. Horizontally scrolls to keep the cursor
/// visible so long patterns stay editable in a narrow overlay.
fn render_pattern_editor_line(
    buf: &mut Buffer,
    content_x: u16,
    y: u16,
    content_width: u16,
    edit: &PatternEditState,
    theme: &Theme,
) {
    let prompt_style = Style::default().fg(theme.accent_user);
    buf.set_span(content_x, y, &Span::styled("\u{276f} ", prompt_style), 2);

    let text_x = content_x + 2;
    let window = content_width.saturating_sub(2) as usize;
    if window == 0 {
        return;
    }

    let chars: Vec<char> = edit.buffer.chars().collect();
    let cursor_idx = edit.buffer[..edit.cursor].chars().count();
    // Reserve one column for the caret so an end-of-line cursor is visible.
    let start = (cursor_idx + 1).saturating_sub(window);

    let text_style = Style::default().fg(theme.text_primary);
    let caret_style = Style::default().fg(theme.bg_light).bg(theme.accent_user);

    let end = (start + window).min(chars.len());
    let mut col: u16 = 0;
    for (offset, ch) in chars[start..end].iter().enumerate() {
        let idx = start + offset;
        let style = if idx == cursor_idx {
            caret_style
        } else {
            text_style
        };
        buf.set_span(text_x + col, y, &Span::styled(ch.to_string(), style), 1);
        col += 1;
    }
    // Block caret past the final character (cursor at end of buffer).
    if cursor_idx >= chars.len() && (col as usize) < window {
        buf.set_span(text_x + col, y, &Span::styled(" ", caret_style), 1);
    }
}

/// Draw the live preview line under the pattern editor: whether the edited
/// pattern still matches the command being approved (reuses the real evaluator
/// so it can't drift), a non-blocking "very broad" warning, and the key hints.
fn render_pattern_preview_line(
    buf: &mut Buffer,
    content_x: u16,
    y: u16,
    content_width: u16,
    edit: &PatternEditState,
    command: &str,
    theme: &Theme,
) {
    let dim = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::DIM);
    let sep = Span::styled("  \u{00b7}  ", dim);

    let mut spans: Vec<Span<'static>> = Vec::new();
    match edit.trimmed() {
        None => {
            spans.push(Span::styled(
                "type a command pattern to allow (e.g. gh api repos/*)",
                dim,
            ));
        }
        Some(pattern) => {
            if xai_grok_workspace::permission::bash_pattern_matches_command(pattern, command) {
                spans.push(Span::styled(
                    "\u{2713} matches this command",
                    Style::default().fg(theme.accent_success),
                ));
            } else {
                spans.push(Span::styled(
                    "\u{2717} won't match this command",
                    Style::default().fg(theme.accent_error),
                ));
            }
            if xai_grok_workspace::permission::bash_pattern_is_broad(pattern) {
                spans.push(sep.clone());
                spans.push(Span::styled(
                    "\u{26a0} very broad",
                    Style::default().fg(theme.warning),
                ));
            }
            spans.push(sep);
            spans.push(Span::styled(
                "Enter",
                Style::default().fg(theme.accent_user),
            ));
            spans.push(Span::styled(" save  ", dim));
            spans.push(Span::styled("Esc", Style::default().fg(theme.accent_user)));
            spans.push(Span::styled(" cancel", dim));
        }
    }
    buf.set_line(content_x, y, &Line::from(spans), content_width);
}

/// Wrap + syntax-highlight a bash command the same way the permission
/// overlay body does: preserve source newlines / `\` continuations, keep
/// heredoc bodies intact, quote-aware width wrap only — **no** soft-breaks
/// at `&&` / `||` / `|` / `;` (those made one command look like multiple
/// prompts once the full command is shown in the overlay).
///
/// Used by the execute tool-call header so scrollback matches the overlay.
pub(crate) fn render_bash_command_display_lines(
    command: &str,
    content_width: usize,
) -> Vec<Line<'static>> {
    build_raw_bash_lines(command, content_width)
}

/// Build the permission-overlay bash command display.
///
/// Prefers the original `raw` command string so spacing, newlines, and
/// trailing-`\` line continuations survive. Falls back to re-joining
/// highlight tokens only when raw is missing. Soft-wraps only when a
/// physical line exceeds `content_width`, preferring breaks at shell
/// operators (`&&`, `||`, `|`, `;`) over every whitespace boundary.
fn build_permission_bash_lines(
    highlights: Option<&BashCommandHighlights>,
    selection_count: usize,
    raw: Option<&str>,
    content_width: usize,
) -> Vec<Line<'static>> {
    let display = display_command_text(highlights, raw);
    if display.is_empty() {
        return Vec::new();
    }

    let needs_dim = highlights.is_some_and(|h| {
        selection_count < h.highlighted_words.len() || !h.prefix.is_empty() || !h.suffix.is_empty()
    });

    if needs_dim && let Some(h) = highlights {
        if let Some(ranges) = map_selection_ranges(&display, h, selection_count) {
            return build_bash_lines_with_selection(&display, &ranges, content_width);
        }
        // Mapping onto the raw string failed (token order/shape mismatch).
        // Do **not** fall through to a fully-bright raw render — that would
        // drop the selection scope cue. Reconstruct from highlight tokens
        // and dim by token index instead.
        return build_bash_lines_from_highlight_tokens(h, selection_count, content_width);
    }

    build_raw_bash_lines(&display, content_width)
}

/// Fallback dim path when raw-string token mapping fails: join highlight
/// tokens with spaces and dim by token selection, then reuse the shared wrap.
fn build_bash_lines_from_highlight_tokens(
    h: &BashCommandHighlights,
    selection_count: usize,
    content_width: usize,
) -> Vec<Line<'static>> {
    let mut full = String::new();
    let mut ranges: Vec<(usize, usize, bool)> = Vec::new();

    let mut push = |text: &str, selected: bool| {
        if text.is_empty() {
            return;
        }
        if !full.is_empty() {
            full.push(' ');
        }
        let start = full.len();
        full.push_str(text);
        ranges.push((start, full.len(), selected));
    };

    for t in &h.prefix {
        push(t, false);
    }
    for (i, t) in h.highlighted_words.iter().enumerate() {
        push(t, i < selection_count);
    }
    for t in &h.suffix {
        push(t, false);
    }

    if full.is_empty() {
        return Vec::new();
    }
    build_bash_lines_with_selection(&full, &ranges, content_width)
}

/// Choose the best source text for the bash display.
///
/// Raw wins when present (preserves author formatting). Otherwise reconstruct
/// from highlight tokens with single spaces — last resort only.
fn display_command_text(highlights: Option<&BashCommandHighlights>, raw: Option<&str>) -> String {
    if let Some(raw) = raw {
        let prepared = prepare_bash_display_text(raw);
        if !prepared.is_empty() {
            return prepared;
        }
    }
    if let Some(h) = highlights {
        return reconstruct_from_highlights(h);
    }
    String::new()
}

/// Normalize command text for display without destroying structure.
///
/// - Unifies line endings to `\n`
/// - Trims trailing whitespace per physical line (keeps indent)
/// - **Preserves** intentional newlines, including lines that end in `\`
///   (shell line continuations like `cmd \\\n  --flag`)
fn prepare_bash_display_text(command: &str) -> String {
    let normalized = command.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    for (i, line) in normalized.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    // Drop trailing blank lines (common when scripts end with `\n`)
    // but keep interior blank lines.
    while out.ends_with('\n') {
        let without = &out[..out.len() - 1];
        if without.ends_with('\\') {
            // Dangling `\` continuation at EOF: keep the backslash visible on
            // the last line but drop the now-useless trailing newline, which
            // would otherwise render as a stray empty row in the overlay.
            out.pop();
            break;
        }
        if without.is_empty() || without.ends_with('\n') {
            out.pop();
            continue;
        }
        // Single trailing newline after content — drop it.
        out.pop();
        break;
    }
    out
}

/// Last-resort display when we only have tokenized highlights (no raw script).
fn reconstruct_from_highlights(h: &BashCommandHighlights) -> String {
    let mut parts: Vec<&str> =
        Vec::with_capacity(h.prefix.len() + h.highlighted_words.len() + h.suffix.len());
    parts.extend(h.prefix.iter().map(String::as_str));
    parts.extend(h.highlighted_words.iter().map(String::as_str));
    parts.extend(h.suffix.iter().map(String::as_str));
    parts.join(" ")
}

/// Soft-wrap one physical line using tree-sitter-derived break offsets from
/// the full script. Breaks only at real list/pipeline operators (never
/// `&&` inside heredocs, quotes, or comments).
///
/// Continuation rows after an operator soft-break **strip leading whitespace**
/// so `&&` / `|` do not leave a dangling space on the next display line.
/// Overlong segments that still exceed width use quote-aware wrapping (do not
/// break on spaces inside `'...'` / `"..."`), so e.g. `jq '.[] | ...'` stays
/// intact instead of wrapping at the `|`.
fn soft_wrap_physical_line(
    line: &str,
    line_start: usize,
    full_breaks: &[usize],
    heredoc_payload: &[(usize, usize)],
    content_width: usize,
) -> Vec<Line<'static>> {
    highlight_rows(soft_wrap_row_texts(
        line,
        line_start,
        full_breaks,
        heredoc_payload,
        content_width,
    ))
}

/// Compute the display row string slices for one physical `line`, applying the
/// same operator-aware + quote-aware wrapping as [`soft_wrap_physical_line`]
/// but **without** highlighting. Every returned slice is a sub-slice of `line`
/// (so a caller can recover its byte offset via pointer arithmetic), which lets
/// the selection-dimming path reuse the identical wrapping decisions.
fn soft_wrap_row_texts<'a>(
    line: &'a str,
    line_start: usize,
    full_breaks: &[usize],
    heredoc_payload: &[(usize, usize)],
    content_width: usize,
) -> Vec<&'a str> {
    if content_width == 0 {
        return vec![line];
    }

    if UnicodeWidthStr::width(line) <= content_width {
        return vec![line];
    }

    // Heredoc body/content is free-form payload, not shell syntax — do not
    // soft-wrap at spaces. Keep the physical line intact even if it overflows.
    let line_end = line_start + line.len();
    if range_fully_inside(line_start, line_end, heredoc_payload) {
        return vec![line];
    }

    let chunks = split_physical_line_at_soft_breaks(line, line_start, full_breaks);
    // No real operators on this line (or parse found none) — quote-aware wrap.
    if chunks.len() <= 1 {
        return bash_quote_aware_wrap(line, content_width);
    }

    // Pack contiguous partitions of `line` into rows that fit the width.
    // Chunks are contiguous slices, so a packed row is just line[start..end].
    let mut chunk_starts: Vec<usize> = Vec::with_capacity(chunks.len());
    {
        let mut cursor = 0usize;
        for chunk in &chunks {
            debug_assert_eq!(&line[cursor..cursor + chunk.len()], *chunk);
            chunk_starts.push(cursor);
            cursor += chunk.len();
        }
    }

    // Row specs as (start, end) into `line`, then trim for display.
    let mut row_ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < chunks.len() {
        // Skip leading whitespace when *starting* a continuation after a
        // previous row — that space belonged between operator and next cmd.
        let mut start = chunk_starts[i];
        if !row_ranges.is_empty() {
            while start < line.len() && line.as_bytes()[start].is_ascii_whitespace() {
                start += 1;
            }
            // Advance i if we skipped entire leading chunks of whitespace.
            while i < chunks.len() && chunk_starts[i] + chunks[i].len() <= start {
                i += 1;
            }
            if i >= chunks.len() {
                break;
            }
            // If we partially skipped into the current chunk, use `start`.
            if chunk_starts[i] < start {
                // start is inside chunks[i]
            } else {
                start = chunk_starts[i];
            }
        }

        let mut last_fit = i;
        let mut j = i;
        while j < chunks.len() {
            let end = chunk_starts[j] + chunks[j].len();
            // Display width from `start` (whitespace-trimmed for continuations).
            let slice = &line[start..end];
            if UnicodeWidthStr::width(slice) <= content_width {
                last_fit = j;
                j += 1;
            } else {
                break;
            }
        }
        if j == i {
            // Chunk alone exceeds width — emit rest of this chunk for quote-wrap.
            let end = chunk_starts[i] + chunks[i].len();
            row_ranges.push((start, end));
            i += 1;
        } else {
            let end = chunk_starts[last_fit] + chunks[last_fit].len();
            row_ranges.push((start, end));
            i = last_fit + 1;
        }
    }

    let mut out = Vec::new();
    for (start, end) in row_ranges {
        let row = line[start..end].trim_end();
        // Continuations already had leading ws skipped via `start`; first row
        // keeps any intentional indent.
        if UnicodeWidthStr::width(row) <= content_width {
            out.push(row);
        } else {
            out.extend(bash_quote_aware_wrap(row, content_width));
        }
    }
    out
}

fn highlight_rows<'a, I>(rows: I) -> Vec<Line<'static>>
where
    I: IntoIterator<Item = &'a str>,
{
    rows.into_iter()
        .map(|row| {
            let spans = crate::views::tasks_pane::highlight_bash_command(row);
            Line::from(spans)
        })
        .collect()
}

/// Word-wrap a bash fragment without breaking on whitespace that sits inside
/// single- or double-quoted strings.
///
/// Break candidates are byte offsets *after* a run of whitespace that is not
/// inside quotes. If a single unbreakable span (e.g. a long `'...'` literal)
/// still exceeds `width`, it is emitted as one row (may overflow the panel —
/// better than splitting `jq '.[] | ...'` mid-expression).
fn bash_quote_aware_wrap(line: &str, width: usize) -> Vec<&str> {
    if width == 0 || UnicodeWidthStr::width(line) <= width {
        return vec![line];
    }

    let break_after = quote_aware_break_points(line);
    if break_after.is_empty() {
        // Nowhere safe to break (entire line is one quoted span, or no spaces).
        return vec![line];
    }

    let mut rows: Vec<&str> = Vec::new();
    let mut row_start = 0usize;
    let mut last_break = 0usize; // exclusive end of content if we break here

    // Consider each break point as a candidate end for the current row.
    let mut candidates = break_after;
    candidates.push(line.len()); // allow ending at EOL

    for &b in &candidates {
        if b <= row_start {
            continue;
        }
        let candidate = line[row_start..b].trim_end();
        if UnicodeWidthStr::width(candidate) <= width {
            last_break = b;
            continue;
        }
        // Exceeded width: emit up to last_break if we made progress.
        if last_break > row_start {
            let row = line[row_start..last_break].trim_end();
            if !row.is_empty() {
                rows.push(row);
            }
            // Next row starts after whitespace at last_break.
            row_start = last_break;
            while row_start < line.len() && line.as_bytes()[row_start].is_ascii_whitespace() {
                row_start += 1;
            }
            last_break = row_start;
            // Re-evaluate this break point against the new row_start.
            if b > row_start {
                let candidate = line[row_start..b].trim_end();
                if UnicodeWidthStr::width(candidate) <= width {
                    last_break = b;
                } else {
                    // Still too wide with nothing smaller — force-emit unbreakable.
                    let force_end = b;
                    let row = line[row_start..force_end].trim_end();
                    if !row.is_empty() {
                        rows.push(row);
                    }
                    row_start = force_end;
                    while row_start < line.len() && line.as_bytes()[row_start].is_ascii_whitespace()
                    {
                        row_start += 1;
                    }
                    last_break = row_start;
                }
            }
        } else {
            // No prior break in this row — unbreakable span larger than width.
            let row = line[row_start..b].trim_end();
            if !row.is_empty() {
                rows.push(row);
            }
            row_start = b;
            while row_start < line.len() && line.as_bytes()[row_start].is_ascii_whitespace() {
                row_start += 1;
            }
            last_break = row_start;
        }
    }
    if row_start < line.len() {
        let row = line[row_start..].trim_end();
        if !row.is_empty() {
            rows.push(row);
        }
    }
    if rows.is_empty() { vec![line] } else { rows }
}

/// Byte offsets at the *start* of whitespace runs that are safe soft-wrap
/// points (outside single/double quotes). The caller ends the current row at
/// this offset (trimming the run) and skips the whitespace before the next
/// row. Does not include offsets inside quotes.
fn quote_aware_break_points(line: &str) -> Vec<usize> {
    let bytes = line.as_bytes();
    let mut breaks = Vec::new();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2; // skip escape
                continue;
            }
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b if b.is_ascii_whitespace() => {
                // Consume the whole whitespace run; break *after* it so the
                // next row starts at non-ws (caller also trims).
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                // Prefer breaking after the whitespace (start of next token).
                // Also allow break at end of prior token by recording `start`
                // so the previous row can end before the spaces.
                if start > 0 {
                    breaks.push(start);
                }
            }
            _ => i += 1,
        }
    }
    breaks.dedup();
    breaks
}

/// Build syntax-highlighted lines for a (possibly multi-line) bash command.
///
/// Preserves intentional newlines / `\` continuations. Soft-wraps overlong
/// physical lines at tree-sitter-validated shell operators (`&&` / `||` /
/// `|` / `;`), then quote-aware width wrap within each segment.
fn build_raw_bash_lines(command: &str, content_width: usize) -> Vec<Line<'static>> {
    let text = prepare_bash_display_text(command);
    if text.is_empty() {
        return Vec::new();
    }

    // Operator breaks + heredoc ranges need a full-script parse so body lines
    // are not space-wrapped and quoted `&&` is not treated as a list op.
    let full_breaks = soft_break_offsets_after_operators(&text);
    let heredoc_payload = heredoc_payload_byte_ranges(&text);

    let mut out = Vec::new();
    let mut offset = 0usize;
    for (idx, physical) in text.split('\n').enumerate() {
        if idx > 0 {
            offset += 1; // the '\n'
        }
        out.extend(soft_wrap_physical_line(
            physical,
            offset,
            &full_breaks,
            &heredoc_payload,
            content_width,
        ));
        offset += physical.len();
    }
    out
}

/// Map highlight selection onto byte ranges in the display string.
///
/// Walks tokens in order (prefix → highlighted_words → suffix), locating each
/// in the display text while skipping whitespace and `\`-newline continuations.
/// Returns `None` if any token cannot be found (caller falls back to undimmed).
fn map_selection_ranges(
    display: &str,
    h: &BashCommandHighlights,
    selection_count: usize,
) -> Option<Vec<(usize, usize, bool)>> {
    let mut ranges = Vec::new();
    let mut pos = 0usize;

    for token in &h.prefix {
        let (start, end) = find_next_token(display, pos, token)?;
        ranges.push((start, end, false));
        pos = end;
    }
    for (i, token) in h.highlighted_words.iter().enumerate() {
        let (start, end) = find_next_token(display, pos, token)?;
        ranges.push((start, end, i < selection_count));
        pos = end;
    }
    for token in &h.suffix {
        let (start, end) = find_next_token(display, pos, token)?;
        ranges.push((start, end, false));
        pos = end;
    }

    Some(ranges)
}

/// Skip whitespace and shell line-continuations (`\` + newline).
fn skip_ws_and_continuations(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    while i < bytes.len() {
        let c = s[i..].chars().next().expect("i on char boundary");
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if c == '\\' {
            let after = i + 1;
            // `\` followed by optional horizontal ws then a newline = continuation
            let mut j = after;
            while j < bytes.len() {
                let ch = s[j..].chars().next().expect("j on char boundary");
                if ch == ' ' || ch == '\t' {
                    j += ch.len_utf8();
                    continue;
                }
                if ch == '\n' {
                    i = j + 1;
                    break;
                }
                // Not a line continuation.
                return i;
            }
            if j >= bytes.len() {
                return i;
            }
            continue;
        }
        break;
    }
    i
}

/// Locate `token` at the next *in-order* shell position after `from`.
///
/// Skips whitespace, line-continuations, and bare list/pipeline operators
/// (`&&` `||` `|` `;`) that often sit between highlight tokens without being
/// part of the highlight list themselves. Does **not** free-scan the rest of
/// the string — a forward substring search would bind the wrong occurrence
/// (e.g. `test` inside `latest`).
fn find_next_token(display: &str, from: usize, token: &str) -> Option<(usize, usize)> {
    if token.is_empty() {
        return None;
    }
    // When the highlight token *is* an operator (e.g. prefix includes "&&"),
    // do not skip past operators — only skip whitespace/continuations.
    let i = if is_shell_op_token(token) {
        skip_ws_and_continuations(display, from)
    } else {
        skip_ws_ops_and_continuations(display, from)
    };
    if i >= display.len() {
        return None;
    }
    let end = match_token_at(display, i, token)?;
    Some((i, end))
}

fn is_shell_op_token(token: &str) -> bool {
    matches!(token, "&&" | "||" | "|" | ";")
}

/// Skip whitespace, `\`-newline continuations, and shell list/pipeline
/// operators that are not themselves mapped highlight tokens.
fn skip_ws_ops_and_continuations(s: &str, mut i: usize) -> usize {
    loop {
        i = skip_ws_and_continuations(s, i);
        if i >= s.len() {
            return i;
        }
        let rest = &s[i..];
        if rest.starts_with("&&") || rest.starts_with("||") {
            i += 2;
            continue;
        }
        if rest.starts_with('|') || rest.starts_with(';') {
            i += 1;
            continue;
        }
        return i;
    }
}

fn match_token_at(display: &str, i: usize, token: &str) -> Option<usize> {
    if !token_start_boundary(display, i) {
        return None;
    }
    let rest = &display[i..];
    // Bare token
    if rest.starts_with(token) {
        let end = i + token.len();
        if token_end_boundary(display, end) {
            return Some(end);
        }
    }
    // Double-quoted (parser stores unquoted content for simple strings)
    let dq = format!("\"{token}\"");
    if rest.starts_with(&dq) {
        let end = i + dq.len();
        if token_end_boundary(display, end) {
            return Some(end);
        }
    }
    // Single-quoted
    let sq = format!("'{token}'");
    if rest.starts_with(&sq) {
        let end = i + sq.len();
        if token_end_boundary(display, end) {
            return Some(end);
        }
    }
    None
}

fn token_start_boundary(display: &str, i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let c = display[..i]
        .chars()
        .next_back()
        .expect("i on char boundary");
    c.is_whitespace()
        || matches!(
            c,
            '&' | '|' | ';' | '<' | '>' | '(' | ')' | '`' | '\\' | '=' | '"' | '\''
        )
}

fn token_end_boundary(display: &str, end: usize) -> bool {
    if end >= display.len() {
        return true;
    }
    let c = display[end..].chars().next().expect("end on char boundary");
    c.is_whitespace() || matches!(c, '&' | '|' | ';' | '<' | '>' | '(' | ')' | '`' | '\\')
}

/// Highlight each display row and apply selection dimming by global offset.
///
/// Rows are produced by the **same** operator-aware + quote-aware wrapping as
/// the undimmed path ([`soft_wrap_row_texts`]), so a partially-selected command
/// (the common overlay state — `default_scope_count` selects only a couple of
/// tokens) wraps identically to a fully-selected one: `jq '.[] | ...'` is not
/// split inside its quotes and operators do not leave dangling continuation
/// rows. Each row is highlighted independently and dimmed per character using
/// its byte offset back into `display`.
fn build_bash_lines_with_selection(
    display: &str,
    ranges: &[(usize, usize, bool)],
    content_width: usize,
) -> Vec<Line<'static>> {
    let is_pos_selected = |pos: usize| -> bool {
        ranges
            .iter()
            .find(|(start, end, _)| pos >= *start && pos < *end)
            .map(|(_, _, sel)| *sel)
            .unwrap_or_else(|| {
                ranges
                    .iter()
                    .rfind(|(_, end, _)| *end <= pos)
                    .is_some_and(|(_, _, sel)| *sel)
            })
    };

    let full_breaks = soft_break_offsets_after_operators(display);
    let heredoc_payload = heredoc_payload_byte_ranges(display);

    let mut out = Vec::new();
    let mut offset = 0usize;
    for (line_idx, physical) in display.split('\n').enumerate() {
        if line_idx > 0 {
            offset += 1; // the '\n'
        }
        let line_start = offset;

        // Wrap this physical line into display rows using the identical logic
        // as the undimmed path. Every row is a sub-slice of `physical`, so its
        // start offset is recoverable via pointer arithmetic.
        for row in soft_wrap_row_texts(
            physical,
            line_start,
            &full_breaks,
            &heredoc_payload,
            content_width,
        ) {
            let row_start_in_physical = (row.as_ptr() as usize) - (physical.as_ptr() as usize);
            let row_global_start = line_start + row_start_in_physical;
            out.push(highlight_row_with_dim(
                row,
                row_global_start,
                &is_pos_selected,
            ));
        }
        offset = line_start + physical.len();
    }
    out
}

/// Syntax-highlight a single display `row` and overlay `Modifier::DIM` on
/// characters whose global byte offset (`global_start + local`) is not selected.
fn highlight_row_with_dim(
    row: &str,
    global_start: usize,
    is_pos_selected: &impl Fn(usize) -> bool,
) -> Line<'static> {
    let hl_spans = crate::views::tasks_pane::highlight_bash_command(row);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut pos = 0usize;
    for span in hl_spans {
        let text = span.content.into_owned();
        let base_style = span.style;
        let mut i = 0;
        while i < text.len() {
            let selected = is_pos_selected(global_start + pos + i);
            let mut j = i + 1;
            while j < text.len() && is_pos_selected(global_start + pos + j) == selected {
                j += 1;
            }
            let (i_aligned, j_aligned) = (snap_char(&text, i), snap_char(&text, j));
            if j_aligned > i_aligned {
                let slice = text[i_aligned..j_aligned].to_owned();
                let style = if selected {
                    base_style
                } else {
                    base_style.add_modifier(Modifier::DIM)
                };
                spans.push(Span::styled(slice, style));
            }
            i = j;
        }
        pos += text.len();
    }
    Line::from(spans)
}

/// Round a byte index down to the nearest UTF-8 char boundary.
fn snap_char(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Render the MCP tool name as a single line with the in-scope segment
/// highlighted (accent + bold) and the rest dimmed. The qualified name
/// is shown title-cased as `"(Server) Action"`; tool-scope highlights
/// both segments, server-scope highlights only `"(Server) "` and dims
/// the action.
fn build_mcp_scope_lines(
    scope: &McpScopeState,
    theme: &Theme,
    _content_w: usize,
) -> Vec<Line<'static>> {
    let active_style = Style::default()
        .fg(theme.accent_user)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(theme.gray).add_modifier(Modifier::DIM);

    let spans: Vec<Span<'static>> = match (scope.selected, scope.server_prefix.as_deref()) {
        // No server prefix: only tool-scope is reachable; whole name is "active".
        (_, None) => vec![Span::styled(scope.display_name(), active_style)],
        // Tool-scope highlights everything (the full qualified name is being whitelisted).
        (McpScope::Tool, Some(_)) => vec![Span::styled(scope.display_name(), active_style)],
        // Server-scope highlights "(Server) " and dims the action.
        (McpScope::Server, Some(prefix)) => vec![
            Span::styled(format!("({}) ", mcp_titleize_segment(prefix)), active_style),
            Span::styled(mcp_titleize_segment(scope.action()), inactive_style),
        ],
    };
    vec![Line::from(spans)]
}

/// Styled display lines for the planned MCP tool arguments
/// ([`PermissionViewState::description`]).
///
/// JSON-highlighted at render time with the theme-matched syntect instance
/// (a mid-prompt `/theme` switch recolors), falling back to a flat
/// secondary style. Highlighting preserves the text, so rows match
/// [`mcp_args_visible_rows`]; `max_rows` stops the syntect work once the
/// visible budget is filled.
fn build_mcp_args_lines(
    description: &[String],
    theme: &Theme,
    content_w: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if description.is_empty() || max_rows == 0 {
        return Vec::new();
    }
    let fallback = Style::default().fg(theme.text_secondary);
    let syntect = crate::syntax::get_syntect();
    // The highlighter is stateful across lines (pretty JSON nests).
    let mut hl = syntect.highlight_lines_for_token("json");
    let mut out: Vec<Line<'static>> = Vec::new();
    for raw in description {
        // Visible budget filled — skip highlighting the rest.
        if out.len() >= max_rows {
            break;
        }
        let spans = crate::syntax::highlight_line(raw, &mut hl, syntect, fallback);
        out.extend(char_wrap_spans(spans, content_w));
    }
    out.truncate(max_rows);
    out
}

/// The `... Ctrl-F to expand` indicator line for a collapsed args display.
/// Styling matches the question tool's truncation indicator.
fn truncation_indicator_line(theme: &Theme) -> Line<'static> {
    let style = Style::default().fg(theme.gray).bg(theme.bg_light);
    Line::from(vec![
        Span::styled("... ", style),
        Span::styled(
            "Ctrl-F",
            Style::default().fg(theme.accent_user).bg(theme.bg_light),
        ),
        Span::styled(" to expand", style),
    ])
}

/// Span-preserving variant of `char_wrap`: splits a styled span run into
/// lines at the same unicode-width column boundaries, merging adjacent
/// same-style runs.
///
/// Invariant: produces exactly `char_wrap(text, width).len()` lines for
/// the same flattened text (same break condition; empty input is one
/// blank row).
fn char_wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut line_spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    let mut col = 0usize;

    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            let line_has_content = !run.is_empty() || !line_spans.is_empty();
            if col + ch_w > width && line_has_content {
                if !run.is_empty() {
                    line_spans.push(Span::styled(std::mem::take(&mut run), run_style));
                }
                lines.push(Line::from(std::mem::take(&mut line_spans)));
                col = 0;
            }
            if style != run_style && !run.is_empty() {
                line_spans.push(Span::styled(std::mem::take(&mut run), run_style));
            }
            run_style = style;
            run.push(ch);
            col += ch_w;
        }
    }
    if !run.is_empty() {
        line_spans.push(Span::styled(run, run_style));
    }
    if !line_spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(line_spans));
    }
    lines
}

/// Character-wrap a plain string to `width` columns (unicode-width
/// aware); an empty input yields one blank row. Character (not word)
/// wrapping keeps every JSON column visible.
///
/// Test-only reference: production uses [`char_wrap_row_count`] and
/// [`char_wrap_spans`], pinned against this by the property tests.
#[cfg(test)]
fn char_wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in s.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + ch_w > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += ch_w;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Build a styled line for a single permission option.
///
/// Normal options (AllowOnce, AllowAlways, RejectAlways) render as radio rows
/// prefixed by the 1-based keyboard shortcut number:
/// ```text
///  1 (*) Always allow: cargo test
///  2 (o) Yes, proceed
///  4 (o) Never allow: cargo test
/// ```
///
/// RejectOnce renders as a freeform input row (matching question view style)
/// using the same 1-based shortcut number as its prefix:
/// ```text
///  3 [ ] Tell Grok what to do differently
///  3 [x] ❯ my followup message preview...
/// ```
///
/// When `selected_words` is `Some`, AllowAlways/RejectAlways options that
/// carry `BashCommandPermission` meta have their labels dynamically rebuilt
/// as `"{prompt_prefix} {selected_words}"`.
#[allow(clippy::too_many_arguments)]
fn build_permission_option_line<'a>(
    option: &acp::PermissionOption,
    index: usize,
    is_cursor: bool,
    row_bg: ratatui::style::Color,
    selected_words: Option<&str>,
    mcp_scope: Option<&McpScopeState>,
    followup_text: &str,
    row_width: u16,
    theme: &Theme,
) -> Line<'a> {
    let num_style = Style::default().fg(theme.accent_user).bg(row_bg);

    let sc = shortcut_char(index);

    if option.kind == acp::PermissionOptionKind::RejectOnce {
        return build_reject_once_line(sc, is_cursor, row_bg, followup_text, theme);
    }

    // Dynamic label: AllowAlways/RejectAlways with BashCommandPermission or
    // McpToolPermission meta gets its scope text rebuilt from current
    // selection state.
    let (label_prefix, scope_words) = dynamic_option_label(option, selected_words, mcp_scope);
    // MCP scope text is a plain identifier, not a bash script — skip
    // syntax highlighting in that case so we don't accidentally tokenize
    // tool names.
    let scope_is_mcp = mcp_scope.is_some();

    let marker = if is_cursor {
        format!("({})", crate::glyphs::filled_dot())
    } else {
        "(\u{25cb})".to_string()
    };
    let marker_style = if is_cursor {
        Style::default()
            .fg(theme.text_primary)
            .bg(row_bg)
            .add_modifier(Modifier::BOLD)
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

    let mut spans = vec![
        Span::styled(format!("{sc} "), num_style),
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(label_prefix, label_style),
    ];

    if let Some(scope) = scope_words {
        let prefix_w: usize = spans.iter().map(|s| s.width()).sum();
        let max_scope = (row_width as usize).saturating_sub(prefix_w + 1);
        let truncated = if scope.width() > max_scope {
            crate::render::line_utils::truncate_str(&scope, max_scope)
        } else {
            scope
        };
        if scope_is_mcp {
            spans.push(Span::styled(truncated, label_style));
        } else {
            for s in crate::views::tasks_pane::highlight_bash_command(&truncated) {
                spans.push(Span::styled(s.content.into_owned(), s.style.bg(row_bg)));
            }
        }
    }

    Line::from(spans).style(Style::default().bg(row_bg))
}

/// Build the RejectOnce row as a freeform input line (mirrors question view).
fn build_reject_once_line<'a>(
    shortcut_ch: char,
    is_cursor: bool,
    row_bg: ratatui::style::Color,
    followup_text: &str,
    theme: &Theme,
) -> Line<'a> {
    let num_style = Style::default().fg(theme.accent_user).bg(row_bg);
    let has_text = !followup_text.trim().is_empty();

    // Radio marker: (●) when cursor is on this row, (○) otherwise.
    // Same logic as other option rows — cursor position determines the marker.
    let (marker, marker_style) = if is_cursor {
        (
            format!("({})", crate::glyphs::filled_dot()),
            Style::default()
                .fg(theme.text_primary)
                .bg(row_bg)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "(\u{25cb})".to_string(),
            Style::default().fg(theme.gray).bg(row_bg),
        )
    };

    let prompt_indicator = Style::default().fg(theme.accent_user).bg(row_bg);

    let (label, label_style) = if has_text {
        // Show preview of typed text.
        let first_line = followup_text.lines().next().unwrap_or("");
        let preview = crate::render::line_utils::truncate_str(first_line, 50);
        (preview, Style::default().fg(theme.text_primary).bg(row_bg))
    } else {
        // Placeholder.
        (
            "No, reject (type to add feedback)".to_string(),
            Style::default().fg(theme.gray).bg(row_bg),
        )
    };

    let mut spans = vec![
        Span::styled(format!("{shortcut_ch} "), num_style),
        Span::styled(format!("{marker} "), marker_style),
    ];
    if has_text {
        spans.push(Span::styled(
            crate::glyphs::prompt_arrow(),
            prompt_indicator,
        ));
    }
    spans.push(Span::styled(label, label_style));

    Line::from(spans).style(Style::default().bg(row_bg))
}

/// Compute the display label for a permission option, with dynamic
/// scope-driven override for the AllowAlways / RejectAlways rows.
///
/// Returns `(prefix_label, Option<scope_text>)`. When `scope_text` is
/// `Some`, the caller renders it after the prefix; for bash that scope
/// is syntax-highlighted, for MCP it is rendered as a plain identifier.
///
/// Bash flow: `selected_words` carries the joined highlighted words and
/// `BashCommandPermission` meta provides the prefix.
///
/// MCP flow: `mcp_scope` carries the toggle selection and the option's
/// `McpToolPermission` meta provides the prefix and tool name. Tool-scope
/// renders the pretty tool name (`"(Server) Action"`); server-scope
/// renders `"all tools from <Server>"`.
fn dynamic_option_label(
    option: &acp::PermissionOption,
    selected_words: Option<&str>,
    mcp_scope: Option<&McpScopeState>,
) -> (String, Option<String>) {
    if matches!(
        option.kind,
        acp::PermissionOptionKind::AllowAlways | acp::PermissionOptionKind::RejectAlways
    ) && let Some(ref meta) = option.meta
    {
        if let Some(scope) = mcp_scope
            && let Ok(perm) =
                serde_json::from_value::<McpToolPermission>(serde_json::Value::Object(meta.clone()))
        {
            let scope_text = match scope.selected {
                McpScope::Tool => perm.display_name(),
                McpScope::Server => match scope.server_prefix.as_deref() {
                    Some(s) => format!("all tools from {}", mcp_titleize_segment(s)),
                    None => perm.display_name(),
                },
            };
            return (format!("{} ", perm.prompt_prefix), Some(scope_text));
        }

        if let Some(words) = selected_words
            && let Ok(bash_perm) = serde_json::from_value::<BashCommandPermission>(
                serde_json::Value::Object(meta.clone()),
            )
        {
            return (
                format!("{} ", bash_perm.prompt_prefix),
                Some(words.to_owned()),
            );
        }
    }
    (option.name.clone(), None)
}

/// Plain-string form of [`dynamic_option_label`] for surfaces without span
/// styling (dashboard peek). Keeps every render surface on the one label
/// source so what is shown always equals the scope the dispatch persists.
pub(crate) fn option_label_for_selection(
    option: &acp::PermissionOption,
    selected_words: Option<&str>,
    mcp_scope: Option<&McpScopeState>,
) -> String {
    let (prefix, scope_text) = dynamic_option_label(option, selected_words, mcp_scope);
    match scope_text {
        Some(scope) => format!("{prefix}{scope}"),
        None => prefix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn pattern_edit_edits_at_the_cursor() {
        let mut e = PatternEditState::new("ghapi");
        assert!(!e.is_dirty());
        assert_eq!(e.cursor, "ghapi".len()); // new() starts at the end
        e.move_home();
        e.move_right();
        e.move_right();
        assert!(!e.is_dirty(), "cursor moves are not content mutations");
        e.insert_char(' ');
        assert!(e.is_dirty());
        assert_eq!(e.buffer, "gh api");
        e.delete();
        assert_eq!(e.buffer, "gh pi");
        e.move_home();
        e.backspace(); // no-op at start
        assert_eq!((e.buffer.as_str(), e.cursor), ("gh pi", 0));
        e.clear();
        assert_eq!(e.trimmed(), None);
        assert!(e.is_dirty());
    }

    #[test]
    fn pattern_edit_respects_char_boundaries() {
        let mut e = PatternEditState::new("café");
        e.backspace();
        assert_eq!(e.buffer, "caf");
        e.insert_char('é');
        assert_eq!(e.buffer, "café");
        assert!(e.is_dirty());
    }

    fn mcp_state(tool: &str, server: Option<&str>, selected: McpScope) -> McpScopeState {
        McpScopeState {
            tool_name: tool.to_owned(),
            server_prefix: server.map(|s| s.to_owned()),
            selected,
        }
    }

    fn allow_always_mcp_option(tool: &str, server: Option<&str>) -> acp::PermissionOption {
        let perm = McpToolPermission {
            prompt_prefix: "Always allow:".to_owned(),
            tool_name: tool.to_owned(),
            server_prefix: server.map(|s| s.to_owned()),
        };
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-always-mcp")),
            format!("Always allow: {}", tool),
            acp::PermissionOptionKind::AllowAlways,
        )
        .meta(
            serde_json::to_value(perm)
                .ok()
                .and_then(|v| v.as_object().cloned()),
        )
    }

    fn permission_state_with_title(title: &str, n_options: usize) -> PermissionViewState {
        let (response_tx, _rx) = tokio::sync::oneshot::channel();
        let request = acp::RequestPermissionRequest::new(
            acp::SessionId::new(Arc::from("test")),
            acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from("call-1")),
                acp::ToolCallUpdateFields::default(),
            ),
            vec![],
        );
        let options: Vec<acp::PermissionOption> = (0..n_options)
            .map(|i| {
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new(Arc::from(format!("opt-{i}"))),
                    format!("Option {i}"),
                    acp::PermissionOptionKind::AllowOnce,
                )
            })
            .collect();
        PermissionViewState {
            request: xai_acp_lib::AcpArgs {
                request,
                response_tx,
            },
            id: 0,
            focus: PermissionFocus::Options,
            options,
            active_idx: 0,
            bash_highlights: None,
            bash_selection_count: 0,
            bash_command_raw: Some("cargo test --all".to_string()),
            mcp_scope: None,
            title: title.to_string(),
            description: vec![],
            args_expanded: false,
            desc_scroll: 0,
            subagent_label: Some("subagent: worker".to_string()),
            options_area_height: 0,
            options_scroll_offset: 0,
        }
    }

    #[test]
    fn render_short_area_at_buffer_bottom_does_not_panic() {
        // Regression: a squeezed permission overlay (0-2 rows) at the bottom of
        // a short terminal wrote the provenance/title rows one past the buffer
        // -> ratatui "index outside of buffer" panic (reported via /feedback as
        // index (5, 10) in a 147x10 terminal).
        let theme = Theme::current();
        for buf_h in [10u16, 12, 24] {
            for area_h in 0u16..=5 {
                for area_y in 0..buf_h {
                    if area_y + area_h > buf_h {
                        continue;
                    }
                    let state = permission_state_with_title("Allow command?", 3);
                    let area = Rect::new(2, area_y, 145, area_h);
                    let mut buf = Buffer::empty(Rect::new(0, 0, 147, buf_h));
                    let _ = render_permission_view(
                        &mut buf, area, &state, "", None, None, &theme, true,
                    );
                }
            }
        }
    }

    #[test]
    fn render_tiny_areas_with_args_do_not_panic() {
        // Panic sweep: widths where the content area underflows to 0,
        // 0-6 row heights, areas at the buffer bottom, both toggle states.
        let theme = Theme::current();
        for expanded in [false, true] {
            for buf_w in 0u16..=10 {
                for area_h in 0u16..=6 {
                    for area_y in [0u16, 4, 8] {
                        if area_y + area_h > 10 {
                            continue;
                        }
                        let mut state = long_args_state();
                        state.args_expanded = expanded;
                        state.subagent_label = Some("subagent: worker".into());
                        let area = Rect::new(0, area_y, buf_w, area_h);
                        let mut buf = Buffer::empty(Rect::new(0, 0, buf_w.max(1), 10));
                        let _ = render_permission_view(
                            &mut buf, area, &state, "follow", None, None, &theme, true,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn view_height_stays_sane_on_tiny_screens() {
        // Expanded height never exceeds the screen; collapsed has a
        // min-floor of 10 the renderer survives on shorter areas.
        let mut state = long_args_state();
        for screen_h in 0u16..=12 {
            let collapsed = permission_view_height(&state, screen_h, 20);
            assert!(
                collapsed <= screen_h.max(10),
                "collapsed {collapsed} exceeds screen {screen_h} (min-floor 10)"
            );
            state.args_expanded = true;
            let expanded = permission_view_height(&state, screen_h, 20);
            assert!(
                expanded <= screen_h,
                "expanded {expanded} > screen {screen_h}"
            );
            state.args_expanded = false;
        }
    }

    #[test]
    fn mcp_scope_state_initializes_to_tool() {
        // The pager constructs `mcp_scope` from request meta in
        // `acp_handler::enqueue_permission` with `selected: McpScope::Tool`
        // as the default. This sanity test pins that behavior at the
        // type level: a fresh state is always Tool.
        let s = mcp_state("linear__list", Some("linear"), McpScope::Tool);
        assert_eq!(s.selected, McpScope::Tool);
    }

    #[test]
    fn mcp_scope_toggle_left_then_right_round_trips() {
        // Mirror the agent_view arrow-key handler: <- contracts Tool -> Server
        // (visually "shrinks" the highlighted region to the server prefix);
        // -> expands Server -> Tool (visually "grows" back to the full tool
        // name). Matches the bash arrow convention.
        let mut s = mcp_state("linear__list", Some("linear"), McpScope::Tool);
        // Left contracts to server.
        if s.server_prefix.is_some() {
            s.selected = McpScope::Server;
        }
        assert_eq!(s.selected, McpScope::Server);
        // Right expands back to tool.
        s.selected = McpScope::Tool;
        assert_eq!(s.selected, McpScope::Tool);
    }

    #[test]
    fn dynamic_option_label_server_scope_renders_all_tools_from_wording() {
        // Pin both the UX wording AND the title-casing of the server
        // segment — if this regresses to `"all <server>__* tools"` or
        // drops title-casing, this test breaks.
        let opt = allow_always_mcp_option("linear__list", Some("linear"));
        let scope = mcp_state("linear__list", Some("linear"), McpScope::Server);
        let (prefix, scope_text) = dynamic_option_label(&opt, None, Some(&scope));
        assert_eq!(prefix, "Always allow: ");
        assert_eq!(scope_text.as_deref(), Some("all tools from Linear"));
    }

    #[test]
    fn dynamic_option_label_tool_scope_renders_pretty_name() {
        // Pins both the `"(Server) Action"` shape and the title-casing of
        // each side (underscores → spaces, each word capitalized).
        let opt = allow_always_mcp_option("linear__list_issues", Some("linear"));
        let scope = mcp_state("linear__list_issues", Some("linear"), McpScope::Tool);
        let (prefix, scope_text) = dynamic_option_label(&opt, None, Some(&scope));
        assert_eq!(prefix, "Always allow: ");
        assert_eq!(scope_text.as_deref(), Some("(Linear) List Issues"));
    }

    fn empty_view_state(mcp_scope: Option<McpScopeState>) -> PermissionViewState {
        let (response_tx, _rx) = tokio::sync::oneshot::channel();
        let request = acp::RequestPermissionRequest::new(
            acp::SessionId::new(Arc::from("test")),
            acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from("call-1")),
                acp::ToolCallUpdateFields::default(),
            ),
            vec![],
        );
        let perm = xai_acp_lib::AcpArgs {
            request,
            response_tx,
        };
        PermissionViewState {
            request: perm,
            id: 0,
            focus: PermissionFocus::Options,
            options: vec![],
            active_idx: 0,
            bash_highlights: None,
            bash_selection_count: 0,
            bash_command_raw: None,
            mcp_scope,
            title: String::new(),
            description: vec![],
            args_expanded: false,
            desc_scroll: 0,
            subagent_label: None,
            options_area_height: 0,
            options_scroll_offset: 0,
        }
    }

    #[test]
    fn mcp_scope_no_server_prefix_disables_toggle() {
        // When the tool name has no `__`, server_prefix is None and the
        // toggle is suppressed.
        let state = empty_view_state(Some(mcp_state("standalone", None, McpScope::Tool)));
        assert!(!state.has_adjustable_scope());
    }

    #[test]
    fn has_adjustable_scope_true_when_mcp_has_server() {
        let state = empty_view_state(Some(mcp_state(
            "linear__list",
            Some("linear"),
            McpScope::Tool,
        )));
        assert!(state.has_adjustable_scope());
    }

    #[test]
    fn has_adjustable_scope_false_for_plain_prompt() {
        let state = empty_view_state(None);
        assert!(!state.has_adjustable_scope());
    }

    #[test]
    fn char_wrap_row_count_matches_char_wrap() {
        // The alloc-free counter must agree with the reference wrapper.
        let cases = [
            "",
            "a",
            "abcdef",
            "  \"key\": \"value with spaces\",",
            "你好世界你好世界",
            "mixed 你 width 好 text",
            &"x".repeat(500),
        ];
        for s in cases {
            for width in [1usize, 2, 3, 7, 10, 80, 500] {
                assert_eq!(
                    char_wrap_row_count(s, width),
                    char_wrap(s, width).len(),
                    "{s:?} at width {width}"
                );
            }
        }
    }

    #[test]
    fn char_wrap_respects_width_and_yields_blank_row_for_empty() {
        assert_eq!(char_wrap("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(char_wrap("abcd", 3), vec!["abc", "d"]);
        // Empty input still occupies one row (blank JSON line).
        assert_eq!(char_wrap("", 10), vec![""]);
        // Width 0 is clamped to 1 (no infinite loop / panic).
        assert_eq!(char_wrap("ab", 0), vec!["a", "b"]);
        // Wide chars count as 2 columns.
        assert_eq!(char_wrap("你好", 2), vec!["你", "好"]);
    }

    #[test]
    fn char_wrap_spans_mirrors_char_wrap_boundaries() {
        // Chrome counts plain text, render wraps styled spans; they must
        // agree regardless of where style runs fall.
        let text = "  \"key\": \"a long value with spaces and 你好 wide chars\",";
        for width in [1usize, 2, 7, 10, 80] {
            // Split the text into arbitrarily-styled runs (every 5 chars).
            let chars: Vec<char> = text.chars().collect();
            let spans: Vec<Span<'static>> = chars
                .chunks(5)
                .enumerate()
                .map(|(i, chunk)| {
                    let style = if i % 2 == 0 {
                        Style::default().fg(ratatui::style::Color::Red)
                    } else {
                        Style::default().fg(ratatui::style::Color::Blue)
                    };
                    Span::styled(chunk.iter().collect::<String>(), style)
                })
                .collect();
            let lines = char_wrap_spans(spans, width);
            let plain = char_wrap(text, width);
            assert_eq!(lines.len(), plain.len(), "width {width}");
            for (line, expect) in lines.iter().zip(&plain) {
                let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert_eq!(&flat, expect, "width {width}");
            }
        }
    }

    #[test]
    fn char_wrap_spans_preserves_styles_across_wrap() {
        let red = Style::default().fg(ratatui::style::Color::Red);
        let blue = Style::default().fg(ratatui::style::Color::Blue);
        let spans = vec![Span::styled("aaaa", red), Span::styled("bbbb", blue)];
        let lines = char_wrap_spans(spans, 6);
        // Line 0: "aaaa" red + "bb" blue; line 1: "bb" blue.
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].content.as_ref(), "aaaa");
        assert_eq!(lines[0].spans[0].style, red);
        assert_eq!(lines[0].spans[1].content.as_ref(), "bb");
        assert_eq!(lines[0].spans[1].style, blue);
        assert_eq!(lines[1].spans[0].content.as_ref(), "bb");
        assert_eq!(lines[1].spans[0].style, blue);
    }

    #[test]
    fn build_mcp_args_lines_highlights_without_altering_text_or_count() {
        // Highlighting must be invisible to layout: text and row count
        // identical to the plain `char_wrap` mirror.
        let description: Vec<String> = vec![
            "{".into(),
            format!("  \"body\": \"{}\",", "x".repeat(120)),
            "  \"n\": 42".into(),
            "}".into(),
        ];
        let theme = Theme::current();
        for width in [10usize, 40, 80] {
            let lines = build_mcp_args_lines(&description, &theme, width, usize::MAX);
            let plain: Vec<String> = description
                .iter()
                .flat_map(|raw| char_wrap(raw, width))
                .collect();
            assert_eq!(lines.len(), plain.len(), "width {width}");
            for (line, expect) in lines.iter().zip(&plain) {
                let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert_eq!(&flat, expect, "width {width}");
            }
        }
        // The JSON grammar must resolve or the builder silently degrades
        // to the flat fallback. Asserted on the grammar, not span counts:
        // NO_COLOR quantizes styles equal and spans legitimately merge.
        assert!(
            crate::syntax::get_syntect()
                .highlight_lines_for_token("json")
                .is_some(),
            "JSON syntax missing from the two-face syntax set"
        );
    }

    #[test]
    fn chrome_height_counts_mcp_args_lines() {
        let mut state = empty_view_state(Some(mcp_state(
            "jira__AddjiraComment",
            Some("jira"),
            McpScope::Tool,
        )));
        let base = permission_chrome_height(&state, 80);
        state.description = vec!["{".into(), "  \"body\": \"hi\"".into(), "}".into()];
        assert_eq!(permission_chrome_height(&state, 80), base + 3);
        // A line wider than the content width wraps and is counted as such.
        state.description = vec!["x".repeat(100)];
        assert_eq!(permission_chrome_height(&state, 80), base + 2);
    }

    #[test]
    fn render_shows_planned_mcp_args() {
        // The overlay must render the payload, not just the tool name.
        let mut state = empty_view_state(Some(mcp_state(
            "jira__AddjiraComment",
            Some("jira"),
            McpScope::Tool,
        )));
        state.title = "Allow Jira: Addjira Comment?".to_string();
        state.description = vec![
            "{".to_string(),
            "  \"issue\": \"ABC-123\",".to_string(),
            "  \"body\": \"hello from grok\"".to_string(),
            "}".to_string(),
        ];
        state.options = vec![acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-once")),
            "Yes".to_owned(),
            acp::PermissionOptionKind::AllowOnce,
        )];
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        let _ = render_permission_view(&mut buf, area, &state, "", None, None, &theme, true);

        let text: String = (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|col| buf[(col, row)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(
            text.contains("\"issue\": \"ABC-123\","),
            "args JSON not rendered:\n{text}"
        );
        assert!(
            text.contains("\"body\": \"hello from grok\""),
            "args JSON not rendered:\n{text}"
        );
        // Option row still visible below the args.
        assert!(text.contains("Yes"), "options row missing:\n{text}");
    }

    fn long_args_state() -> PermissionViewState {
        let mut state = empty_view_state(Some(mcp_state(
            "jira__AddjiraComment",
            Some("jira"),
            McpScope::Tool,
        )));
        state.title = "Allow Jira: Addjira Comment?".to_string();
        state.description = (0..50).map(|i| format!("\"line{i}\": {i},")).collect();
        state.options = vec![acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-once")),
            "Yes".to_owned(),
            acp::PermissionOptionKind::AllowOnce,
        )];
        state
    }

    fn render_to_text(state: &PermissionViewState, area: Rect) -> String {
        let theme = Theme::current();
        let mut buf = Buffer::empty(area);
        let _ = render_permission_view(&mut buf, area, state, "", None, None, &theme, true);
        (0..area.height)
            .map(|row| {
                (area.x..area.x + area.width)
                    .map(|col| buf[(col, row)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn render_collapses_long_mcp_args_with_ctrl_f_indicator() {
        // Collapsed: 4 content rows + indicator, options visible.
        let state = long_args_state();
        let text = render_to_text(&state, Rect::new(0, 0, 80, 30));
        assert!(text.contains("\"line3\": 3,"), "4th content row:\n{text}");
        assert!(
            !text.contains("\"line4\": 4,"),
            "5th row must be the indicator:\n{text}"
        );
        assert!(
            text.contains("... Ctrl-F to expand"),
            "indicator missing:\n{text}"
        );
        assert!(text.contains("Yes"), "options row missing:\n{text}");
    }

    #[test]
    fn render_expanded_mcp_args_clips_at_area_keeping_options_visible() {
        // Expanded shows all the area allows; overflow clips with the
        // ellipsis and option rows always render.
        let mut state = long_args_state();
        state.args_expanded = true;
        let text = render_to_text(&state, Rect::new(0, 0, 80, 12));
        assert!(
            !text.contains("Ctrl-F to expand"),
            "no indicator when expanded:\n{text}"
        );
        assert!(text.contains("Yes"), "options row missing:\n{text}");
        assert!(
            text.contains('\u{2026}'),
            "area-clipped args missing ellipsis:\n{text}"
        );
        assert!(
            !text.contains("\"line49\": 49,"),
            "args should have been clipped:\n{text}"
        );
        // A tall area shows deep rows that the collapsed view never reaches.
        let text_tall = render_to_text(&state, Rect::new(0, 0, 80, 40));
        assert!(
            text_tall.contains("\"line20\": 20,"),
            "expanded view must show deep rows:\n{text_tall}"
        );
    }

    #[test]
    fn mcp_args_visible_rows_budget_and_boundary() {
        let mut state = long_args_state();
        // 50 one-row lines, collapsed: 4 content rows + indicator.
        assert_eq!(mcp_args_visible_rows(&state, 80), (4, true));
        // Expanded: everything, no indicator.
        state.args_expanded = true;
        assert_eq!(mcp_args_visible_rows(&state, 80), (50, false));
        // Exactly at the budget: no truncation, no indicator.
        state.args_expanded = false;
        state.description = (0..MCP_ARGS_COLLAPSED_ROWS)
            .map(|i| format!("l{i}"))
            .collect();
        assert_eq!(
            mcp_args_visible_rows(&state, 80),
            (MCP_ARGS_COLLAPSED_ROWS, false)
        );
    }

    #[test]
    fn expanded_args_lift_the_view_height_cap() {
        let mut state = long_args_state();
        let screen_h = 40;
        let collapsed = permission_view_height(&state, screen_h, 80);
        assert!(
            collapsed <= screen_h / 2,
            "collapsed view respects the 50% cap: {collapsed}"
        );
        state.args_expanded = true;
        let expanded = permission_view_height(&state, screen_h, 80);
        assert!(
            expanded > screen_h / 2 && expanded <= screen_h,
            "expanded view may grow past 50% up to the screen: {expanded}"
        );
    }

    #[test]
    fn dynamic_option_label_renders_tool_scope() {
        let opt = allow_always_mcp_option("linear__list", Some("linear"));
        let scope = mcp_state("linear__list", Some("linear"), McpScope::Tool);
        let (prefix, scope_text) = dynamic_option_label(&opt, None, Some(&scope));
        assert_eq!(prefix, "Always allow: ");
        assert_eq!(scope_text.as_deref(), Some("(Linear) List"));
    }

    #[test]
    fn dynamic_option_label_renders_server_scope() {
        let opt = allow_always_mcp_option("linear__list", Some("linear"));
        let scope = mcp_state("linear__list", Some("linear"), McpScope::Server);
        let (prefix, scope_text) = dynamic_option_label(&opt, None, Some(&scope));
        assert_eq!(prefix, "Always allow: ");
        assert_eq!(scope_text.as_deref(), Some("all tools from Linear"));
    }

    #[test]
    fn dynamic_option_label_server_scope_without_prefix_falls_back_to_tool() {
        // Defensive: render path should disable Server when no prefix,
        // but if state was constructed inconsistently the label still
        // renders the tool name rather than panicking.
        let opt = allow_always_mcp_option("standalone", None);
        let scope = mcp_state("standalone", None, McpScope::Server);
        let (_prefix, scope_text) = dynamic_option_label(&opt, None, Some(&scope));
        assert_eq!(scope_text.as_deref(), Some("Standalone"));
    }

    #[test]
    fn dynamic_option_label_falls_back_to_bash_when_no_mcp() {
        // When mcp_scope is None but selected_words is Some and the meta
        // is BashCommandPermission, the bash branch still works.
        let bash_perm = BashCommandPermission {
            prompt_prefix: "Always allow:".to_owned(),
        };
        let opt = acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-always-command")),
            "Always allow: cargo test".to_owned(),
            acp::PermissionOptionKind::AllowAlways,
        )
        .meta(
            serde_json::to_value(bash_perm)
                .ok()
                .and_then(|v| v.as_object().cloned()),
        );
        let (prefix, scope_text) = dynamic_option_label(&opt, Some("cargo test"), None);
        assert_eq!(prefix, "Always allow: ");
        assert_eq!(scope_text.as_deref(), Some("cargo test"));
    }

    #[test]
    fn dynamic_option_label_rebuilds_reject_always_bash_row() {
        // The "Never allow:" row shares the ←/→ word-scope selection with the
        // allow row, so its label must rebuild from selected_words too.
        let bash_perm = BashCommandPermission {
            prompt_prefix: "Never allow:".to_owned(),
        };
        let opt = acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("reject-always-command")),
            "Never allow: cargo test --workspace".to_owned(),
            acp::PermissionOptionKind::RejectAlways,
        )
        .meta(
            serde_json::to_value(bash_perm)
                .ok()
                .and_then(|v| v.as_object().cloned()),
        );
        let (prefix, scope_text) = dynamic_option_label(&opt, Some("cargo test"), None);
        assert_eq!(prefix, "Never allow: ");
        assert_eq!(scope_text.as_deref(), Some("cargo test"));
    }

    #[test]
    fn option_label_for_selection_matches_persisted_scope() {
        // Peek surface contract: the composed label must show exactly the
        // words the dispatch meta will persist, not the static full name.
        let opt = acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("reject-always-command")),
            "Never allow: cargo test --workspace".to_owned(),
            acp::PermissionOptionKind::RejectAlways,
        )
        .meta(
            serde_json::to_value(BashCommandPermission {
                prompt_prefix: "Never allow:".to_owned(),
            })
            .ok()
            .and_then(|v| v.as_object().cloned()),
        );
        assert_eq!(
            option_label_for_selection(&opt, Some("cargo"), None),
            "Never allow: cargo"
        );
        // Options without scope meta keep their static name.
        let plain = acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-once")),
            "Yes, proceed".to_owned(),
            acp::PermissionOptionKind::AllowOnce,
        );
        assert_eq!(
            option_label_for_selection(&plain, Some("cargo"), None),
            "Yes, proceed"
        );
    }

    #[test]
    fn prepare_bash_display_preserves_backslash_continuations() {
        let raw = "docker run \\\n  -v /tmp:/tmp \\\n  -e FOO=bar \\\n  alpine:latest\n";
        let prepared = prepare_bash_display_text(raw);
        assert!(
            prepared.contains("docker run \\\n  -v /tmp:/tmp \\\n  -e FOO=bar \\\n  alpine:latest"),
            "expected multi-line continuations, got: {prepared:?}"
        );
        // Must not flatten to a single space-joined line.
        assert!(!prepared.contains("docker run \\  -v"));
        assert_eq!(prepared.lines().count(), 4);
    }

    #[test]
    fn prepare_bash_display_drops_dangling_trailing_continuation_newline() {
        // A command ending in `\` + newline (with nothing after) must not
        // render a stray empty row in the height-capped overlay. The trailing
        // backslash stays visible; only the useless newline is dropped.
        let prepared = prepare_bash_display_text("echo a \\\n");
        assert_eq!(prepared, "echo a \\");
        let rows = build_raw_bash_lines("echo a \\\n", 80);
        assert_eq!(rows.len(), 1, "no trailing blank row");
        // Multiple trailing blank lines after a dangling `\` also collapse.
        assert_eq!(prepare_bash_display_text("echo a \\\n\n"), "echo a \\");
        // Interior continuations are untouched.
        assert_eq!(prepare_bash_display_text("a \\\nb\n"), "a \\\nb");
    }

    #[test]
    fn build_raw_bash_lines_keeps_continuation_rows() {
        let raw = "cargo test \\\n  --all \\\n  -- --nocapture";
        let lines = build_raw_bash_lines(raw, 80);
        assert!(
            lines.len() >= 3,
            "expected one row per physical line, got {}",
            lines.len()
        );
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("cargo test \\"));
        assert!(joined.contains("--all \\"));
        assert!(joined.contains("-- --nocapture"));
    }

    fn row_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn soft_wrap_prefers_shell_operators_over_every_space() {
        // Wide enough that each side of `&&` fits alone, but the full
        // line does not — should produce two rows at the operator, not
        // a word-wrap mid-flag.
        let line = "git status --short --branch && cargo test --workspace --all-features";
        let width = 40;
        assert!(UnicodeWidthStr::width(line) > width);
        let breaks = soft_break_offsets_after_operators(line);
        assert!(
            !breaks.is_empty(),
            "tree-sitter should find the real && operator"
        );
        let rows = soft_wrap_physical_line(line, 0, &breaks, &[], width);
        assert!(
            rows.len() >= 2,
            "expected operator split, got {} rows",
            rows.len()
        );
        let first = row_text(&rows[0]);
        assert!(
            first.contains("&&"),
            "first row should keep the operator: {first:?}"
        );
        assert!(
            !first.contains("cargo"),
            "cargo should be on a later row, not packed with git: {first:?}"
        );
        // Continuation must not start with a dangling space from after `&&`.
        let second = row_text(&rows[1]);
        assert!(
            !second.starts_with(' '),
            "no leading space on continuation row: {second:?}"
        );
        assert!(second.starts_with("cargo"), "second={second:?}");
    }

    #[test]
    fn soft_wrap_does_not_break_inside_jq_single_quoted_filter() {
        // Regression: long `gh ... --jq '.[] | ...'` must not wrap at the
        // space after `|` inside the single-quoted filter.
        let line = r#"gh search prs --author=@me --sort=updated --limit=15 --json number,title,url,state,updatedAt,repository,isDraft --jq '.[] | "\(.state)\t#\(.number)\t\(.updatedAt)\t\(.repository.nameWithOwner)\t\(.title)\t\(.url)"'"#;
        let width = 60;
        assert!(UnicodeWidthStr::width(line) > width);
        let breaks = soft_break_offsets_after_operators(line);
        assert!(
            breaks.is_empty(),
            "no shell list ops on this fragment: {breaks:?}"
        );
        let rows = soft_wrap_physical_line(line, 0, &breaks, &[], width);
        let rendered: Vec<String> = rows.iter().map(row_text).collect();
        // The jq filter must never be split at `.[] |`.
        for r in &rendered {
            assert!(
                !(r.ends_with(".[]") || r.ends_with(".[] |") || r.trim_end() == "'.[] |"),
                "must not break after .[] |; rows={rendered:?}"
            );
        }
        // The opening of the filter and the pipe should stay on the same row
        // as part of one single-quoted span (or the whole filter on one row).
        let joined = rendered.join("\n");
        assert!(
            !joined.contains(".[]\n") && !joined.contains(".[] |\n"),
            "jq filter split across rows: {rendered:?}"
        );
    }

    #[test]
    fn bash_quote_aware_wrap_keeps_single_quoted_span_together() {
        let line = "prefix_ok_here '.[] | not a pipe' trailing_words_here_too";
        // Width that forces a wrap, but only at spaces *outside* quotes.
        let width = 20;
        let rows = bash_quote_aware_wrap(line, width);
        let has_split_inside_quotes = rows.iter().any(|r| {
            // A row that opens a quote without closing it while ending at |
            r.contains(".[]") && !r.contains("not a pipe")
        });
        assert!(!has_split_inside_quotes, "split inside quotes: {rows:?}");
        // The full quoted token must appear wholly in some row.
        assert!(
            rows.iter().any(|r| r.contains("'.[] | not a pipe'")),
            "quoted span must be intact in some row: {rows:?}"
        );
    }

    #[test]
    fn soft_wrap_does_not_break_on_heredoc_body_and() {
        let script = "cat <<EOF && echo after\nfoo && bar inside body\nEOF";
        let lines = build_raw_bash_lines(script, 80);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let body_rows: Vec<&String> = rendered
            .iter()
            .filter(|r| r.contains("foo && bar"))
            .collect();
        assert_eq!(
            body_rows.len(),
            1,
            "heredoc body must stay one row, got {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|r| r.contains("cat <<EOF") && r.contains("&&")),
            "opener with real && should render: {rendered:?}"
        );
    }

    #[test]
    fn soft_wrap_does_not_break_on_quoted_and() {
        let line = r#"echo "keep && together" && echo next"#;
        let breaks = soft_break_offsets_after_operators(line);
        assert_eq!(breaks.len(), 1, "breaks={breaks:?}");
        let width = 28;
        assert!(UnicodeWidthStr::width(line) > width);
        let rows = soft_wrap_physical_line(line, 0, &breaks, &[], width);
        let first: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            first.contains(r#""keep && together""#),
            "quoted && must stay on the first row: {first:?}"
        );
        assert!(first.contains("&&"), "real operator stays with first row");
    }

    #[test]
    fn display_prefers_raw_over_rejoined_tokens() {
        let h = BashCommandHighlights {
            prefix: vec!["cd".into(), "/tmp".into(), "&&".into()],
            highlighted_words: vec!["git".into(), "status".into()],
            suffix: vec![],
        };
        // Raw keeps the original spacing / layout; rejoined tokens would
        // become "cd /tmp && git status".
        let raw = "cd /tmp && \\\n  git status";
        let display = display_command_text(Some(&h), Some(raw));
        assert_eq!(display, "cd /tmp && \\\n  git status");
    }

    #[test]
    fn map_selection_ranges_across_continuations() {
        let display = "cd /tmp && \\\n  git status --short";
        let h = BashCommandHighlights {
            prefix: vec!["cd".into(), "/tmp".into(), "&&".into()],
            highlighted_words: vec!["git".into(), "status".into(), "--short".into()],
            suffix: vec![],
        };
        let ranges = map_selection_ranges(display, &h, 2).expect("map tokens");
        // First two highlighted words selected: git, status
        let selected: Vec<&str> = ranges
            .iter()
            .filter(|(_, _, sel)| *sel)
            .map(|(s, e, _)| &display[*s..*e])
            .collect();
        assert_eq!(selected, vec!["git", "status"]);
    }

    #[test]
    fn short_single_line_stays_one_row() {
        let lines = build_raw_bash_lines("echo hello", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn heredoc_body_line_does_not_wrap_at_spaces() {
        // Physical heredoc-body lines must not soft-wrap on spaces.
        let script = "cat <<EOF && echo after\nthis is a very long heredoc body line with many spaces that would otherwise wrap\nEOF";
        let prepared = prepare_bash_display_text(script);
        let body_line = prepared
            .lines()
            .find(|l| l.contains("very long heredoc"))
            .expect("body line");
        let width = 20;
        assert!(UnicodeWidthStr::width(body_line) > width);
        // Find body line offset in prepared text.
        let body_start = prepared.find(body_line).unwrap();
        let breaks = soft_break_offsets_after_operators(&prepared);
        let heredoc = heredoc_payload_byte_ranges(&prepared);
        assert!(
            range_fully_inside(body_start, body_start + body_line.len(), &heredoc),
            "body must be classified as heredoc payload"
        );
        let rows = soft_wrap_row_texts(body_line, body_start, &breaks, &heredoc, width);
        assert_eq!(
            rows.len(),
            1,
            "heredoc body must stay one row even when narrow: {rows:?}"
        );
        assert_eq!(rows[0], body_line);
    }

    #[test]
    fn map_failure_still_dims_via_token_fallback() {
        // When map_selection_ranges fails, do not render fully bright.
        // Force a map miss with tokens that cannot appear in the raw string.
        let raw = "echo hello world";
        let h = BashCommandHighlights {
            prefix: vec![],
            highlighted_words: vec!["definitely_not_in_raw_zzzz".into(), "also_missing".into()],
            suffix: vec!["extra_suffix_missing".into()],
        };
        let lines = build_permission_bash_lines(Some(&h), 1, Some(raw), 200);
        assert!(!lines.is_empty());
        let mut has_dim = false;
        let mut bright = String::new();
        let mut dim = String::new();
        for line in &lines {
            for span in &line.spans {
                if span.style.add_modifier.contains(Modifier::DIM) {
                    has_dim = true;
                    dim.push_str(span.content.as_ref());
                } else {
                    bright.push_str(span.content.as_ref());
                }
            }
        }
        assert!(has_dim, "fallback must still dim unselected tokens");
        // First highlight token selected → bright; rest dimmed.
        assert!(
            bright.contains("definitely_not_in_raw_zzzz"),
            "selected token bright: {bright:?}"
        );
        assert!(
            dim.contains("also_missing") || dim.contains("extra_suffix_missing"),
            "unselected tokens dim: {dim:?}"
        );
    }

    #[test]
    fn find_next_token_does_not_bind_later_occurrence() {
        // Must not attach `test` to the later `test` inside a different word.
        // Sequence: token "cargo" then "test" — after cargo, only in-order match.
        let display = "cargo latest && test --all";
        // From start of "latest", looking for "test" must NOT match the "test"
        // suffix of "latest" via free scan — only the standalone `test` after &&.
        let from = display.find("latest").unwrap();
        // Old buggy free-scan could match inside "latest". New path only accepts
        // the token at the next shell position after skipping ops/ws.
        // From `latest`, skip doesn't skip alnum, so match at `latest` for "test"
        // fails (wrong token), and we return None — whole map fails closed.
        assert!(
            find_next_token(display, from, "test").is_none(),
            "must not match test as a suffix of latest"
        );
        // From after "cargo ", we get "latest" not "test".
        let after_cargo = display.find(' ').unwrap() + 1;
        assert!(find_next_token(display, after_cargo, "test").is_none());
        // Correct sequential map from 0 works for the real tokens.
        let (s, e) = find_next_token(display, 0, "cargo").unwrap();
        assert_eq!(&display[s..e], "cargo");
        let (s, e) = find_next_token(display, e, "latest").unwrap();
        assert_eq!(&display[s..e], "latest");
        let (s, e) = find_next_token(display, e, "test").unwrap();
        assert_eq!(&display[s..e], "test");
    }

    #[test]
    fn dim_path_wraps_quote_aware_like_undim_path() {
        // Regression: the REAL overlay path has highlights (Some) + raw (Some)
        // with a *partial* selection (default_scope_count selects ~2 tokens),
        // so `needs_dim` is true. Before the fix this path used plain
        // whitespace `word_wrap`, splitting `jq '.[] | ...'` inside its single
        // quotes. It must now wrap exactly like the undimmed path.
        let raw = r#"gh search prs --author=@me --json number,title,url --jq '.[] | "\(.state)\t#\(.number)\t\(.url)"'"#;
        let h = BashCommandHighlights {
            prefix: vec![],
            highlighted_words: vec![
                "gh".into(),
                "search".into(),
                "prs".into(),
                "--author=@me".into(),
                "--json".into(),
                "number,title,url".into(),
                "--jq".into(),
                r#".[] | "\(.state)\t#\(.number)\t\(.url)""#.into(),
            ],
            suffix: vec![],
        };
        let width = 60;
        // selection_count = 2 -> partial selection -> dim path.
        let dim_lines = build_permission_bash_lines(Some(&h), 2, Some(raw), width);
        let dim_rows: Vec<String> = dim_lines.iter().map(row_text).collect();
        for r in &dim_rows {
            assert!(
                !(r.trim_end().ends_with(".[]") || r.trim_end().ends_with(".[] |")),
                "jq filter split inside quotes under dim path; rows={dim_rows:?}"
            );
        }
        // The dimmed rows must match the undimmed wrapping row-for-row.
        let undim_rows: Vec<String> = build_raw_bash_lines(raw, width)
            .iter()
            .map(row_text)
            .collect();
        assert_eq!(
            dim_rows, undim_rows,
            "dim path must wrap identically to undim path"
        );
    }

    #[test]
    fn dim_path_preserves_selection_dimming() {
        // The first `selection_count` highlighted words render *without* DIM;
        // everything else is dimmed. Verify dimming survives the new per-row
        // highlight path.
        let raw = "git status --short && cargo test --workspace";
        let h = BashCommandHighlights {
            prefix: vec![],
            highlighted_words: vec!["git".into(), "status".into(), "--short".into()],
            suffix: vec![
                "&&".into(),
                "cargo".into(),
                "test".into(),
                "--workspace".into(),
            ],
        };
        // Select only `git status` (2 tokens); the rest must be dimmed.
        let lines = build_permission_bash_lines(Some(&h), 2, Some(raw), 200);
        let mut dim_text = String::new();
        let mut bright_text = String::new();
        for line in &lines {
            for span in &line.spans {
                if span.style.add_modifier.contains(Modifier::DIM) {
                    dim_text.push_str(span.content.as_ref());
                } else {
                    bright_text.push_str(span.content.as_ref());
                }
            }
        }
        assert!(
            bright_text.contains("git") && bright_text.contains("status"),
            "selected tokens must be bright: bright={bright_text:?}"
        );
        assert!(
            dim_text.contains("cargo") && dim_text.contains("workspace"),
            "unselected suffix must be dimmed: dim={dim_text:?}"
        );
        assert!(
            !bright_text.contains("cargo"),
            "cargo must not be bright: bright={bright_text:?}"
        );
    }

    #[test]
    fn prepare_bash_display_normalizes_crlf() {
        let raw = "echo a\r\necho b\r\n";
        let prepared = prepare_bash_display_text(raw);
        assert!(
            !prepared.contains('\r'),
            "CRLF not normalized: {prepared:?}"
        );
        assert_eq!(prepared, "echo a\necho b");
    }

    #[test]
    fn tiny_widths_do_not_panic_dim_and_undim() {
        // width 0 and width 1 must never panic (empty rows / mid-char indices).
        let raw = "git status --short && cargo test --workspace | grep ok";
        let h = BashCommandHighlights {
            prefix: vec![],
            highlighted_words: vec!["git".into(), "status".into(), "--short".into()],
            suffix: vec!["&&".into(), "cargo".into(), "test".into()],
        };
        for w in [0usize, 1, 2, 3] {
            let _ = build_raw_bash_lines(raw, w);
            let _ = build_permission_bash_lines(Some(&h), 2, Some(raw), w);
            // Multi-byte content must not panic on mid-char snapping either.
            let _ = build_raw_bash_lines("échø 'ünîcødé && stüff' && lß", w);
        }
    }

    #[test]
    fn dim_path_multiline_continuation_no_panic_and_dims() {
        let raw = "cd /tmp && \\\n  git status --short --branch --verbose --long";
        let h = BashCommandHighlights {
            prefix: vec!["cd".into(), "/tmp".into(), "&&".into()],
            highlighted_words: vec!["git".into(), "status".into(), "--short".into()],
            suffix: vec!["--branch".into(), "--verbose".into(), "--long".into()],
        };
        // Narrow width forces wrapping of the continuation line.
        let lines = build_permission_bash_lines(Some(&h), 2, Some(raw), 20);
        assert!(!lines.is_empty());
        let rows: Vec<String> = lines.iter().map(row_text).collect();
        // Delimiter soft-breaks are disabled — no row should end at `&&` solely
        // to start the next command on a new display line.
        for r in &rows {
            let t = r.trim_end();
            assert!(
                !(t.ends_with("&&") && rows.len() > 1),
                "must not soft-break at && for display: {rows:?}"
            );
        }
    }

    /// Human-review harness: render historic bash commands as the permission
    /// overlay would, to stdout at several widths.
    ///
    /// ```text
    /// PERMISSION_UI_RENDER_REVIEW=1 cargo test -p xai-grok-pager --lib \
    ///   render_historic_bash_commands_for_review -- --nocapture --ignored
    /// ```
    #[test]
    #[ignore = "manual review harness; run with PERMISSION_UI_RENDER_REVIEW=1 --ignored --nocapture"]
    fn render_historic_bash_commands_for_review() {
        let enabled = std::env::var("PERMISSION_UI_RENDER_REVIEW")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !enabled {
            eprintln!(
                "skip: set PERMISSION_UI_RENDER_REVIEW=1 and pass --ignored --nocapture to run"
            );
            return;
        }

        let fixture = historic_bash_fixture_path();
        let raw = std::fs::read_to_string(&fixture)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture.display()));
        let commands = parse_historic_bash_fixture(&raw);
        assert!(
            !commands.is_empty(),
            "no commands in fixture {}",
            fixture.display()
        );

        let widths: Vec<usize> = std::env::var("PERMISSION_UI_RENDER_WIDTHS")
            .ok()
            .map(|s| s.split(',').filter_map(|p| p.trim().parse().ok()).collect())
            .filter(|v: &Vec<usize>| !v.is_empty())
            .unwrap_or_else(|| vec![60, 80, 100]);

        let mut issues: Vec<String> = Vec::new();
        println!("# Permission UI bash render review");
        println!("# fixture: {}", fixture.display());
        println!("# commands: {}  widths: {widths:?}", commands.len());
        println!();

        for (idx, cmd) in commands.iter().enumerate() {
            let n = idx + 1;
            println!("{}", "=".repeat(88));
            println!(
                "CMD {n:02}  ({} bytes, {} physical lines)",
                cmd.len(),
                cmd.lines().count()
            );
            println!("{}", "-".repeat(88));
            println!("SOURCE:");
            for line in cmd.lines() {
                println!("  | {line}");
            }
            println!();

            for &w in &widths {
                let rows = build_raw_bash_lines(cmd, w);
                let texts: Vec<String> = rows.iter().map(line_plain_text).collect();
                println!("RENDER w={w}  ({} rows)", texts.len());
                for (ri, t) in texts.iter().enumerate() {
                    let vis = t.replace('\t', "\\t");
                    println!("  {ri:>2} │{vis}│");
                    // Delimiter soft-breaks are disabled — a wrap row should not
                    // end at &&/||/| solely to start the next command.
                    if ri + 1 < texts.len() {
                        let trimmed = t.trim_end();
                        if trimmed.ends_with("&&")
                            || trimmed.ends_with("||")
                            || (trimmed.ends_with('|') && !trimmed.ends_with("||"))
                        {
                            issues.push(format!(
                                "CMD {n:02} w={w} row {ri}: soft-break at delimiter {t:?}"
                            ));
                        }
                    }
                }
                // Flag split of jq-style `.[] |` across rows (quote-break regression).
                for window in texts.windows(2) {
                    let a = window[0].trim_end();
                    let b = window[1].trim_start();
                    if a.ends_with(".[]") && (b.starts_with('|') || b.starts_with(" |")) {
                        issues.push(format!("CMD {n:02} w={w}: split at .[] |  ({a:?} / {b:?})"));
                    }
                    if a.ends_with(".[] |") || a.ends_with(".[] | ") {
                        issues.push(format!("CMD {n:02} w={w}: row ends at .[] |  ({a:?})"));
                    }
                }
                println!();
            }
        }

        println!("{}", "=".repeat(88));
        if issues.is_empty() {
            println!("AUTO-CHECKS: OK (no delimiter soft-breaks, no .[] | splits)");
        } else {
            println!("AUTO-CHECKS: {} issue(s)", issues.len());
            for i in &issues {
                println!("  - {i}");
            }
        }
        // Soft-fail only on auto-check issues when running the harness.
        assert!(
            issues.is_empty(),
            "{} auto-check issue(s) — see stdout above",
            issues.len()
        );
    }

    fn line_plain_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn historic_bash_fixture_path() -> std::path::PathBuf {
        // CARGO_MANIFEST_DIR = packages/tui/xai-grok-pager
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/historic_bash_cmds.txt")
    }

    fn parse_historic_bash_fixture(raw: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur: Option<String> = None;
        for line in raw.lines() {
            if line.starts_with("### CMD ") {
                cur = Some(String::new());
                continue;
            }
            if line == "### END" {
                if let Some(mut s) = cur.take() {
                    while s.ends_with('\n') {
                        s.pop();
                    }
                    if !s.is_empty() {
                        out.push(s);
                    }
                }
                continue;
            }
            if let Some(ref mut s) = cur {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(line);
            }
        }
        out
    }
}
