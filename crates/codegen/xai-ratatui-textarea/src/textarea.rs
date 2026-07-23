use crate::editor::{
    ApplyEditPlanError, EditBuffer, EditCommand, EditCommandCategory, EditOutcome, EditPlan,
    WordStyle, classify_key_event,
};
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::StatefulWidgetRef;
use ratatui::widgets::WidgetRef;
use ratatui_core::buffer::Buffer as CoreBuffer;
use ratatui_core::layout::Rect as CoreRect;
use ratatui_core::widgets::Widget as _;
use std::cell::Ref;
use std::cell::RefCell;
use std::ops::Range;
use std::time::Instant;
use textwrap::Options;
use tui_scrollbar::{ScrollBar, ScrollLengths};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Stable, unique identifier for a text element. Monotonically increasing, never reused.
///
/// The host app can use this as a key into its own metadata store
/// (e.g. `HashMap<ElementId, PasteMetadata>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElementId(u64);

impl ElementId {
    /// Construct an `ElementId` from a raw `u64` value.
    ///
    /// Primarily useful for tests and serialization; normal code should use
    /// the IDs returned by [`TextArea::insert_element`].
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Opaque element kind tag. The textarea does not interpret this value;
/// the host app defines constants like `ElementKind(1)` for pastes,
/// `ElementKind(2)` for file references, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElementKind(pub u16);

// ── Clipboard ──

/// Trait for clipboard access. The textarea calls this on copy/cut/paste.
///
/// The default implementation ([`InternalClipboard`]) stores text in memory.
/// Host apps can provide a system clipboard backend (e.g. `arboard`) via
/// [`TextArea::set_clipboard_provider`].
pub trait ClipboardProvider: std::fmt::Debug {
    /// Read the current clipboard contents (for paste).
    fn get(&mut self) -> Option<String>;
    /// Write text to the clipboard (on copy/cut).
    fn set(&mut self, text: &str);
}

/// In-memory clipboard — the default provider.
#[derive(Debug, Default)]
pub struct InternalClipboard {
    contents: Option<String>,
}

impl ClipboardProvider for InternalClipboard {
    fn get(&mut self) -> Option<String> {
        self.contents.clone()
    }

    fn set(&mut self, text: &str) {
        self.contents = Some(text.to_string());
    }
}

// ── Text element events ──

/// An interaction with a [`TextElement`], returned by [`TextArea::poll_element_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextElementEvent {
    /// The element that was interacted with.
    pub id: ElementId,
    /// What kind of interaction occurred.
    pub kind: TextElementEventKind,
}

/// The kind of element interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextElementEventKind {
    /// The element was clicked (single click).
    Click,
    /// The mouse entered the element (was outside or on a different element).
    HoverEnter,
    /// The mouse left the element (moved to plain text or a different element).
    HoverLeave,
}

/// An atomic text element embedded in the buffer.
///
/// Elements are indivisible units for navigation and editing. The cursor
/// cannot be placed inside an element; it jumps from the start boundary
/// to the end boundary atomically.
#[derive(Debug, Clone)]
pub struct TextElement {
    /// Stable identifier, unique across the lifetime of the `TextArea`.
    pub id: ElementId,
    /// Byte range in the underlying text buffer.
    pub range: Range<usize>,
    /// Host-defined kind tag.
    pub kind: ElementKind,
    /// Custom display text and styling. When `Some`, this `Line` is rendered
    /// instead of the raw buffer text. When `None`, the buffer text is rendered
    /// with a default element style (cyan).
    pub display: Option<Line<'static>>,
}

// ── Selection ──

/// A byte-range selection in the buffer, created by mouse drag.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    /// Buffer position where the selection started (fixed anchor).
    pub anchor: usize,
    /// Buffer position where the selection currently extends to (moves with drag).
    pub head: usize,
}

// ── Mouse ──

/// Result of processing a mouse event in the textarea.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseAction {
    /// Nothing interesting happened.
    Nothing,
    /// Cursor was placed at a position (single click on plain text).
    CursorPlaced,
    /// Selection was updated (drag in progress, or double/triple click).
    SelectionUpdated,
    /// Selection was finalized — text copied to clipboard.
    /// Host should call `take_clipboard()` to retrieve it.
    SelectionFinished,
    /// Content was scrolled (mouse wheel).
    Scrolled,
}

/// Tracks consecutive clicks at the same screen position to detect
/// double-click (word select) and triple-click (line select).
#[derive(Debug)]
struct ClickTracker {
    last_time: Instant,
    last_pos: (u16, u16),
    count: u8,
}

impl Default for ClickTracker {
    fn default() -> Self {
        Self {
            last_time: Instant::now(),
            last_pos: (u16::MAX, u16::MAX),
            count: 0,
        }
    }
}

impl ClickTracker {
    /// Maximum time between clicks to count as multi-click (ms).
    const MULTI_CLICK_MS: u128 = 500;

    /// Register a click at `(col, row)`. Returns the click count (1, 2, or 3).
    fn register(&mut self, col: u16, row: u16) -> u8 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_time).as_millis();
        if elapsed < Self::MULTI_CLICK_MS && self.last_pos == (col, row) && self.count < 3 {
            self.count += 1;
        } else {
            self.count = 1;
        }
        self.last_time = now;
        self.last_pos = (col, row);
        self.count
    }
}

#[derive(Debug)]
pub struct TextArea {
    text: EditBuffer,
    wrap_cache: RefCell<Option<WrapCache>>,
    preferred_col: Option<usize>,
    elements: Vec<TextElement>,
    next_element_id: u64,
    kill_buffer: String,
    undo: UndoState,
    /// Active selection (mouse drag). `None` when no selection.
    selection: Option<Selection>,
    /// Clipboard provider — defaults to [`InternalClipboard`].
    /// Swap with [`set_clipboard_provider`](Self::set_clipboard_provider)
    /// for system clipboard support.
    clipboard_provider: Box<dyn ClipboardProvider>,
    /// Last copied text — set on copy/cut, cleared by `take_clipboard()`.
    /// This is the "notification" channel: the host calls `take_clipboard()`
    /// to detect that something was just copied.
    clipboard: Option<String>,
    /// Whether to keep the selection visible after mouse-up.
    /// When `false`, selection clears immediately on mouse-up (fully transient).
    pub keep_selection_after_mouseup: bool,
    /// Style applied to selected text.  Defaults to a tokyonight-inspired
    /// blue background (`rgb(49, 62, 115)`) with an explicit light foreground
    /// (`rgb(192, 202, 245)`) so the selection is legible regardless of the
    /// host terminal's colour scheme.
    ///
    /// Override to match your own theme, e.g.:
    /// ```ignore
    /// textarea.selection_style = Style::default().bg(Color::Rgb(60, 60, 60));
    /// ```
    pub selection_style: Style,
    /// Screen position of the last mouse-down (for distinguishing click vs drag).
    mouse_down_pos: Option<(u16, u16)>,
    /// Buffer byte position of the mouse-down anchor (for drag selection).
    drag_anchor: Option<usize>,
    /// Whether a drag is currently in progress.
    drag_active: bool,
    /// Last time drag-scroll was applied (throttle).
    last_drag_scroll: Option<Instant>,
    /// Number of drag-scroll steps taken so far (for acceleration).
    drag_scroll_steps: u32,
    /// Stored drag event for continuous drag-scroll (re-triggered on timer).
    /// Set when a drag moves outside the textarea area; cleared on mouse-up.
    pending_drag_scroll: Option<MouseEvent>,
    /// Tracks multi-click (double/triple) at the same position.
    click_tracker: ClickTracker,
    /// Internal scroll offset set by mousewheel events.  When `Some`, this
    /// overrides the external `TextAreaState.scroll` so the viewport scrolls
    /// independently of the cursor.  Cleared whenever the cursor moves
    /// (typing, navigation, click) so the viewport snaps back to follow it.
    scroll_override: Option<u16>,
    /// Whether to show a scrollbar on the right edge when content overflows.
    /// When enabled, the rightmost column is reserved for the scrollbar track
    /// and the text area wraps at `width - 1`. Defaults to `true`.
    pub show_scrollbar: bool,
    /// Style for the scrollbar track (empty space).  Defaults to a dark
    /// tokyonight-inspired background.  Override to match your theme's
    /// background when embedding the textarea in a non-default-bg context.
    pub scrollbar_track_style: Style,
    /// Style for the scrollbar thumb (draggable indicator).  Defaults to a
    /// slightly lighter tokyonight shade.  Override to match your theme.
    pub scrollbar_thumb_style: Style,
    /// Padding (in columns) between the text content and the scrollbar track.
    /// Only applies when the scrollbar is visible.  Defaults to `0`.
    pub scrollbar_padding: u16,
    /// Whether the user is currently dragging the scrollbar thumb.
    scrollbar_dragging: bool,
    /// Currently hovered element (for enter/leave detection).
    hovered_element: Option<ElementId>,
    /// Pending element event — consumed by [`poll_element_event`](Self::poll_element_event).
    pending_element_event: Option<TextElementEvent>,
    /// Columns per tab character for display width and tab→space expansion on
    /// insert. `0` leaves tabs as-is (unicode-width treats them as 0-width).
    /// Defaults to `4`, matching scrollback `appearance::tab_width`.
    tab_width: u8,
}

#[derive(Debug, Clone)]
struct WrapCache {
    width: u16,
    lines: Vec<Range<usize>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TextAreaState {
    /// Index into wrapped lines of the first visible line.
    pub scroll: u16,
}

// ── Undo/Redo ──

/// A snapshot of the textarea state for undo/redo.
#[derive(Debug, Clone)]
struct UndoEntry {
    text: String,
    cursor: usize,
    elements: Vec<TextElement>,
}

/// What kind of mutation is being performed. Used for batching consecutive
/// same-kind operations into a single undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationKind {
    /// Character-by-character typing, `insert_str`, `yank`.
    Insert,
    /// Backspace, delete forward.
    Delete,
    /// Ctrl+K, Ctrl+U, word-delete — always a discrete undo step.
    Kill,
    /// `insert_element`, `replace_range_with_element` — always discrete.
    Element,
    /// `set_text`, `replace_range` (host-driven) — always discrete.
    Replace,
}

/// Manages the undo/redo stacks.
#[derive(Debug)]
struct UndoState {
    stack: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    max_depth: usize,
    /// The kind of the last mutation that was checkpointed.
    last_kind: Option<MutationKind>,
    /// Cursor position *after* the last mutation completed.
    /// Used to detect cursor jumps (arrows between inserts → new undo group).
    last_cursor: usize,
    /// Whether the last inserted character was whitespace.
    /// Used to break insert batches at word boundaries (ws↔non-ws transitions).
    last_insert_ws: bool,
    /// Nesting depth for undo groups. When > 0, `pre_mutate` is suppressed.
    group_depth: usize,
    /// Snapshot taken when the outermost `begin_undo_group()` was called.
    /// Used by `end_undo_group` to push the checkpoint, or by
    /// `cancel_undo_group` to restore the pre-group state.
    group_checkpoint: Option<UndoEntry>,
}

impl Default for UndoState {
    fn default() -> Self {
        Self {
            stack: Vec::new(),
            redo: Vec::new(),
            max_depth: 100,
            last_kind: None,
            last_cursor: 0,
            last_insert_ws: false,
            group_depth: 0,
            group_checkpoint: None,
        }
    }
}

/// Whether `key` is the undo chord [`TextArea::input`] binds: lowercase
/// 'z' with Ctrl or Cmd. Uppercase 'Z' (redo) is intentionally excluded,
/// which keeps this guard disjoint from the redo arm regardless of order.
///
/// Single source for the binding: `input()`'s undo arm consumes this
/// predicate, and hosts that react to undo (e.g. retiring an undo hint)
/// call it too, so the chord and its observers cannot drift.
pub fn is_undo_input(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('z'))
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER))
}

impl TextArea {
    /// Compute the number of lines to scroll per mouse wheel tick based on
    /// the viewport height.  Small viewports scroll slowly (1 line), large
    /// viewports scroll faster (up to 3 lines).
    fn scroll_lines_for_height(height: u16) -> u16 {
        match height {
            0..=5 => 1,
            6..=15 => 2,
            _ => 3,
        }
    }

    /// Drag-scroll throttle intervals (ms): ramps up from slow to fast.
    /// After the last entry, the final value repeats.
    const DRAG_SCROLL_RAMP_MS: &[u128] = &[80, 60, 40];

    /// Compute the drag-scroll interval for the given step count.
    fn drag_scroll_interval(step: u32) -> u128 {
        let ramp = Self::DRAG_SCROLL_RAMP_MS;
        ramp[ramp.len().min(step as usize + 1) - 1]
    }

    /// How many extra lines to scroll based on distance from area edge.
    /// Returns 1 for 1-2 rows outside, 2 for 3-4 rows, 3 for 5-8, etc.
    fn drag_scroll_lines_for_distance(distance: u16) -> usize {
        match distance {
            0..=2 => 1,
            3..=5 => 2,
            6..=10 => 3,
            _ => 5,
        }
    }

    /// Clamp a buffer position so it stays within a wrapped line's range
    /// `[line_start, line_end)`.  Without this, `display_col_to_buffer_pos`
    /// can return `line_end` when the column exceeds the line's display
    /// width — and `line_end` equals the *next* wrapped line's start,
    /// which confuses `effective_scroll` into thinking the cursor hasn't
    /// actually moved to the target line.
    ///
    /// Uses `self.text` to find the last valid char boundary inside the line
    /// so we never land in the middle of a multi-byte character.
    fn clamp_to_line(&self, pos: usize, line_start: usize, line_end: usize) -> usize {
        if line_end > line_start {
            // Find the start of the last character in the line.
            let last_char_start = self.text[line_start..line_end]
                .char_indices()
                .next_back()
                .map(|(i, _)| line_start + i)
                .unwrap_or(line_start);
            pos.min(last_char_start)
        } else {
            line_start
        }
    }

    pub fn new() -> Self {
        Self {
            text: EditBuffer::new(),
            wrap_cache: RefCell::new(None),
            preferred_col: None,
            elements: Vec::new(),
            next_element_id: 0,
            kill_buffer: String::new(),
            undo: UndoState::default(),
            selection: None,
            clipboard_provider: Box::new(InternalClipboard::default()),
            clipboard: None,
            keep_selection_after_mouseup: true,
            selection_style: Style::default()
                .bg(Color::Rgb(49, 62, 115))
                .fg(Color::Rgb(192, 202, 245)),
            mouse_down_pos: None,
            drag_anchor: None,
            drag_active: false,
            last_drag_scroll: None,
            drag_scroll_steps: 0,
            pending_drag_scroll: None,
            click_tracker: ClickTracker::default(),
            scroll_override: None,
            show_scrollbar: true,
            scrollbar_track_style: Style::default().bg(Color::Rgb(32, 35, 53)),
            scrollbar_thumb_style: Style::default()
                .fg(Color::Rgb(42, 46, 65))
                .bg(Color::Rgb(32, 35, 53)),
            scrollbar_padding: 0,
            scrollbar_dragging: false,
            hovered_element: None,
            pending_element_event: None,
            tab_width: 4,
        }
    }

    /// Columns per tab for display width and tab→space expansion (`0` = passthrough).
    pub fn tab_width(&self) -> u8 {
        self.tab_width
    }

    /// Set columns per tab. Also controls expansion on insert/`set_text`/`replace_range`.
    pub fn set_tab_width(&mut self, tab_width: u8) {
        if self.tab_width != tab_width {
            self.tab_width = tab_width;
            self.wrap_cache.replace(None);
        }
    }

    /// Expand `\t` to `tab_width` spaces (scrollback-compatible fixed width).
    /// `tab_width == 0` or no tabs → borrowed input.
    ///
    /// Public because it is the exact transform every insert path applies
    /// (see [`insert_str`](Self::insert_str) /
    /// [`insert_element`](Self::insert_element)), letting hosts canonicalize
    /// external text before comparing it against buffer content.
    pub fn expand_tabs<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        expand_tabs_with_width(text, self.tab_width)
    }

    /// Display width of plain buffer text, treating tabs as `tab_width` columns.
    fn plain_display_width(&self, text: &str) -> usize {
        plain_display_width_with_tab(text, self.tab_width)
    }

    /// Display width of a single grapheme cluster (tab uses `tab_width`).
    fn grapheme_display_width(&self, grapheme: &str) -> usize {
        grapheme_display_width_with_tab(grapheme, self.tab_width)
    }

    fn element_ranges(&self) -> Vec<Range<usize>> {
        self.elements
            .iter()
            .map(|element| element.range.clone())
            .collect()
    }

    fn adjust_position_after_edit(
        position: usize,
        replaced: &Range<usize>,
        inserted_len: usize,
    ) -> usize {
        if position < replaced.start {
            position
        } else if position <= replaced.end {
            replaced.start + inserted_len
        } else {
            position - replaced.len() + inserted_len
        }
    }

    fn is_semantic_edit(plan: &EditPlan) -> bool {
        plan.removed_text() != plan.replacement() || !plan.replaced_byte_range().is_empty()
    }

    fn assert_valid_edit_plan(&self, plan: &EditPlan) {
        if let Err(error) = self.text.validate_plan(plan) {
            panic!("textarea edit invariant failed: {error:?}");
        }
    }

    fn apply_validated_edit_plan(
        &mut self,
        plan: EditPlan,
        mutation_kind: Option<MutationKind>,
    ) -> EditOutcome {
        let semantic_edit = Self::is_semantic_edit(&plan);
        let replaced = plan.replaced_byte_range();
        let inserted_len = plan.replacement().len();
        let outcome = self.text.apply_validated_plan(&plan);
        if semantic_edit {
            self.update_elements_after_replace(replaced.start, replaced.end, inserted_len);
            if let Some(selection) = &mut self.selection {
                selection.anchor =
                    Self::adjust_position_after_edit(selection.anchor, &replaced, inserted_len);
                selection.head =
                    Self::adjust_position_after_edit(selection.head, &replaced, inserted_len);
            }
            if self
                .selection
                .is_some_and(|selection| selection.anchor == selection.head)
            {
                self.selection = None;
            }
            self.wrap_cache.replace(None);
            if mutation_kind == Some(MutationKind::Kill) {
                self.kill_buffer = plan.into_removed_text();
            }
        }
        if semantic_edit || !matches!(outcome, EditOutcome::Unchanged) {
            self.preferred_col = None;
            self.scroll_override = None;
        }
        outcome
    }

    fn try_apply_edit_plan(
        &mut self,
        plan: EditPlan,
        mutation_kind: Option<MutationKind>,
    ) -> Result<EditOutcome, ApplyEditPlanError> {
        self.text.validate_plan(&plan)?;
        let semantic_edit = Self::is_semantic_edit(&plan);
        if semantic_edit && let Some(kind) = mutation_kind {
            self.pre_mutate(kind);
        }
        let outcome = self.apply_validated_edit_plan(plan, mutation_kind);
        if semantic_edit && mutation_kind.is_some() {
            self.post_mutate();
        }
        Ok(outcome)
    }

    fn apply_edit_plan(
        &mut self,
        plan: EditPlan,
        mutation_kind: Option<MutationKind>,
    ) -> EditOutcome {
        match self.try_apply_edit_plan(plan, mutation_kind) {
            Ok(outcome) => outcome,
            Err(error) => panic!("textarea edit invariant failed: {error:?}"),
        }
    }

    fn apply_edit_command(
        &mut self,
        command: EditCommand,
        mutation_kind: Option<MutationKind>,
    ) -> EditOutcome {
        let category = command.category();
        let ranges = self.element_ranges();
        let plan = self.text.plan_command(command, &ranges);
        let outcome = self.apply_edit_plan(plan, mutation_kind);
        if category == EditCommandCategory::Navigation {
            self.preferred_col = None;
            self.scroll_override = None;
        }
        outcome
    }

    fn plan_edit_replacement(&self, range: Range<usize>, replacement: &str) -> EditPlan {
        let replacement = self.expand_tabs(replacement).into_owned();
        let ranges = self.element_ranges();
        self.text
            .plan_replace_byte_range(range, &replacement, &ranges)
    }

    fn apply_edit_replacement(
        &mut self,
        range: Range<usize>,
        replacement: &str,
        mutation_kind: Option<MutationKind>,
    ) {
        let plan = self.plan_edit_replacement(range, replacement);
        self.apply_edit_plan(plan, mutation_kind);
    }

    pub fn set_text(&mut self, text: &str) {
        let cursor = self.cursor();
        let plan = self.plan_edit_replacement(0..self.text.len(), text);
        self.assert_valid_edit_plan(&plan);
        self.pre_mutate(MutationKind::Replace);
        let _ = self.text.apply_validated_plan(&plan);
        self.elements.clear();
        let len = self.text.len();
        self.set_cursor_inner(cursor.min(len));
        self.wrap_cache.replace(None);
        self.preferred_col = None;
        // Kill buffer intentionally survives: yank is independent of buffer
        // content, so a cut can be pasted into a fresh prompt after send.
        self.selection = None;
        self.mouse_down_pos = None;
        self.drag_anchor = None;
        self.drag_active = false;
        self.last_drag_scroll = None;
        self.drag_scroll_steps = 0;
        self.pending_drag_scroll = None;
        self.click_tracker = ClickTracker::default();
        self.scroll_override = None;
        self.scrollbar_dragging = false;
        self.hovered_element = None;
        self.pending_element_event = None;
        self.post_mutate();
    }

    pub fn text(&self) -> &str {
        self.text.text()
    }

    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.scroll_override = None;
        // Word boundary: break the insert batch when char class changes (ws↔non-ws).
        if let Some(first) = text.chars().next() {
            let first_ws = first.is_whitespace();
            if self.undo.last_kind == Some(MutationKind::Insert)
                && self.undo.last_insert_ws != first_ws
            {
                // Force pre_mutate to see a "kind change" so it pushes a checkpoint.
                self.undo.last_kind = None;
            }
        }
        self.apply_edit_replacement(
            self.cursor()..self.cursor(),
            text,
            Some(MutationKind::Insert),
        );
        if let Some(last) = text.chars().last() {
            self.undo.last_insert_ws = last.is_whitespace();
        }
    }

    pub fn insert_str_at(&mut self, pos: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        self.apply_edit_replacement(pos..pos, text, Some(MutationKind::Insert));
        if let Some(last) = text.chars().last() {
            self.undo.last_insert_ws = last.is_whitespace();
        }
    }

    pub fn replace_range(&mut self, range: std::ops::Range<usize>, text: &str) {
        self.apply_edit_replacement(range, text, Some(MutationKind::Replace));
    }

    pub fn cursor(&self) -> usize {
        self.text.cursor_byte()
    }

    pub fn set_cursor(&mut self, pos: usize) {
        let pos = pos.clamp(0, self.text.len());
        let pos = self.clamp_pos_to_nearest_boundary(pos);
        self.set_cursor_inner(pos);
        self.preferred_col = None;
        self.scroll_override = None;
    }

    fn set_cursor_inner(&mut self, pos: usize) {
        let _ = self.text.set_cursor_byte(pos);
    }

    /// Override the scroll position, bypassing cursor-follow logic.
    ///
    /// When set to `Some(offset)`, `effective_scroll` will use this offset
    /// instead of ensuring the cursor is visible. Useful for forcing a
    /// specific viewport (e.g., scroll-to-top when the textarea is collapsed
    /// and unfocused). Set to `None` to restore normal cursor-following.
    ///
    /// Note: unlike the internal scroll_override set by mousewheel events,
    /// this is NOT cleared by cursor movement — it persists until explicitly
    /// cleared by the caller.
    pub fn set_scroll_override(&mut self, scroll: Option<u16>) {
        self.scroll_override = scroll;
    }

    /// Current scroll override value (if any).
    pub fn scroll_override(&self) -> Option<u16> {
        self.scroll_override
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        self.wrapped_lines(width).len() as u16
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.cursor_pos_with_state(area, TextAreaState::default())
    }

    /// Compute the on-screen cursor position taking scrolling into account.
    ///
    /// Returns `None` if the cursor is not visible in the current viewport
    /// (e.g. the user scrolled the viewport away from the cursor via mousewheel).
    ///
    /// Unlike [`screen_position_of`], this applies a wrap-boundary adjustment:
    /// when the cursor sits at the exact wrap boundary (col == content width),
    /// it is shown at the start of the next visual line instead of on the
    /// invisible right border.
    pub fn cursor_pos_with_state(&self, area: Rect, state: TextAreaState) -> Option<(u16, u16)> {
        let tw = self.text_width(area);
        let lines = self.wrapped_lines(tw);
        let effective_scroll = self.effective_scroll(area.height, &lines, state.scroll);
        let mut i = Self::wrapped_line_index_by_start(&lines, self.cursor())?;
        let ls = &lines[i];
        let mut col = self.display_width_of_range(ls.start, self.cursor()) as u16;

        // If the cursor sits at the exact wrap boundary (col == content width),
        // show it at the start of the next visual line instead of on the
        // invisible right border.  When the cursor is at text.len() and the
        // last line is exactly full, there is no next wrapped line — but we
        // still want the cursor on a new row at column 0.
        if col >= tw {
            i += 1;
            col = 0;
        }

        // If the cursor's visual line is outside the visible viewport, hide it.
        let scroll = effective_scroll as usize;
        if i < scroll || i >= scroll + area.height as usize {
            return None;
        }

        let screen_row = (i - scroll) as u16;
        Some((area.x + col, area.y + screen_row))
    }

    /// Compute the on-screen position of an arbitrary buffer byte offset.
    ///
    /// Returns `None` if the position is outside the visible viewport.
    /// Does not apply cursor-specific wrap-boundary adjustments — see
    /// [`cursor_pos_with_state`] for cursor positioning.
    pub fn screen_position_of(
        &self,
        pos: usize,
        area: Rect,
        state: TextAreaState,
    ) -> Option<(u16, u16)> {
        let tw = self.text_width(area);
        let lines = self.wrapped_lines(tw);
        let effective_scroll = self.effective_scroll(area.height, &lines, state.scroll);
        let i = Self::wrapped_line_index_by_start(&lines, pos)?;
        let ls = &lines[i];
        let col = self.display_width_of_range(ls.start, pos) as u16;

        let scroll = effective_scroll as usize;
        if i < scroll || i >= scroll + area.height as usize {
            return None;
        }

        let screen_row = (i - scroll) as u16;
        Some((area.x + col, area.y + screen_row))
    }

    /// Compute the on-screen cells covered by a buffer byte range.
    ///
    /// A soft-wrapped range can cross visual rows, so unlike
    /// [`screen_position_of`] this returns one height-1 [`Rect`] per visual
    /// row the range intersects, top to bottom, clamped to the content
    /// region (`text_width` columns — excludes any scrollbar column). Rows
    /// scrolled outside the viewport are skipped, so a partially visible
    /// range yields only its visible rows. Bytes belonging to no row (a
    /// `\n`, or whitespace dropped at a wrap boundary) are not covered;
    /// trailing spaces kept on a row are. Ranges that are empty, extend
    /// past the text, or have non-char-boundary endpoints yield no spans.
    pub fn screen_spans_of_range(
        &self,
        range: Range<usize>,
        area: Rect,
        state: TextAreaState,
    ) -> Vec<Rect> {
        let mut spans = Vec::new();
        if range.start >= range.end
            || range.end > self.text.len()
            || !self.text.is_char_boundary(range.start)
            || !self.text.is_char_boundary(range.end)
        {
            return spans;
        }
        let tw = self.text_width(area);
        let lines = self.wrapped_lines(tw);
        let scroll = self.effective_scroll(area.height, &lines, state.scroll) as usize;
        // Rows before the one containing `range.start` cannot intersect;
        // `None` (start ahead of the first row) falls back to scanning all.
        let first = Self::wrapped_line_index_by_start(&lines, range.start).unwrap_or(0);
        // Rendered content stops at `tw` columns; a row's trailing wrap
        // spaces can measure wider, so clamp to the content edge, not the
        // full area (whose last column may hold the scrollbar).
        let right_edge = area.x.saturating_add(tw);
        for (i, ls) in lines.iter().enumerate().skip(first) {
            if ls.start >= range.end {
                break;
            }
            if i < scroll {
                continue;
            }
            if i >= scroll + area.height as usize {
                break;
            }
            let seg_start = range.start.max(ls.start);
            let seg_end = range.end.min(ls.end);
            if seg_start >= seg_end {
                continue;
            }
            let start_x = area
                .x
                .saturating_add(self.display_width_of_range(ls.start, seg_start) as u16)
                .min(right_edge);
            let end_x = area
                .x
                .saturating_add(self.display_width_of_range(ls.start, seg_end) as u16)
                .min(right_edge);
            if start_x < end_x {
                spans.push(Rect {
                    x: start_x,
                    y: area.y + (i - scroll) as u16,
                    width: end_x - start_x,
                    height: 1,
                });
            }
        }
        spans
    }

    /// Map screen coordinates `(col, row)` to a buffer byte position.
    ///
    /// Returns `None` if `(col, row)` is outside the textarea `area`.
    ///
    /// Edge cases:
    /// - Click past end of a wrapped line → snaps to line end.
    /// - Click below all text → snaps to `text.len()`.
    /// - Click on an element → snaps to nearest element boundary (start or end).
    pub fn buffer_pos_at_screen(
        &self,
        col: u16,
        row: u16,
        area: Rect,
        state: TextAreaState,
    ) -> Option<usize> {
        // Outside the textarea area → None.
        if col < area.x || col >= area.x + area.width || row < area.y || row >= area.y + area.height
        {
            return None;
        }

        let tw = self.text_width(area);
        let lines = self.wrapped_lines(tw);
        let scroll = self.effective_scroll(area.height, &lines, state.scroll);

        let visual_row = (row - area.y) as usize + scroll as usize;

        // Below all text → end of text.
        if visual_row >= lines.len() {
            return Some(self.text.len());
        }

        let line = &lines[visual_row];
        let target_col = (col - area.x) as usize;
        // Clamp line.end to text length (safety measure for edge cases).
        let line_end = line.end.min(self.text.len());
        Some(
            self.display_col_to_buffer_pos(line.start, line_end, target_col)
                .0,
        )
    }

    /// Like `buffer_pos_at_screen` but also indicates whether the column
    /// fell on an element's display region.
    fn buffer_pos_at_screen_ex(
        &self,
        col: u16,
        row: u16,
        area: Rect,
        state: TextAreaState,
    ) -> Option<(usize, bool)> {
        if col < area.x || row < area.y {
            return None;
        }

        let tw = self.text_width(area);
        let lines = self.wrapped_lines(tw);
        let scroll = self.effective_scroll(area.height, &lines, state.scroll);

        let visual_row = (row - area.y) as usize + scroll as usize;

        if visual_row >= lines.len() {
            return Some((self.text.len(), false));
        }

        let line = &lines[visual_row];
        let target_col = (col - area.x) as usize;
        let line_end = line.end.min(self.text.len());
        Some(self.display_col_to_buffer_pos(line.start, line_end, target_col))
    }

    /// Return the element at screen coordinates, if any.
    ///
    /// Uses `buffer_pos_at_screen` to find the buffer position, then checks
    /// whether that position falls inside an element.
    pub fn element_at_screen(
        &self,
        col: u16,
        row: u16,
        area: Rect,
        state: TextAreaState,
    ) -> Option<&TextElement> {
        let (pos, hit_element) = self.buffer_pos_at_screen_ex(col, row, area, state)?;
        if hit_element {
            // hit_element means the column fell on an element's display.
            // pos may be elem start or elem end — match either.
            self.elements
                .iter()
                .find(|e| pos >= e.range.start && pos <= e.range.end && !e.range.is_empty())
        } else {
            self.elements
                .iter()
                .find(|e| pos >= e.range.start && pos < e.range.end)
        }
    }

    // ── Selection API ──

    /// Normalized selection range, expanded to element boundaries.
    ///
    /// Returns `None` if no selection is active or anchor == head (empty).
    pub fn selection_range(&self) -> Option<Range<usize>> {
        let sel = self.selection?;
        if sel.anchor == sel.head {
            return None;
        }
        let start = sel.anchor.min(sel.head);
        let end = sel.anchor.max(sel.head);
        let expanded = self.expand_range_to_element_boundaries(start..end);
        let clamped_start = expanded.start.min(self.text.len());
        let clamped_end = expanded.end.min(self.text.len());
        if clamped_start >= clamped_end {
            None
        } else {
            Some(clamped_start..clamped_end)
        }
    }

    /// Text within the current selection (buffer text, not display text).
    pub fn selected_text(&self) -> Option<String> {
        let range = self.selection_range()?;
        Some(self.text[range].to_string())
    }

    /// Clear the selection without affecting the clipboard.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Delete the selected range (if any). Returns `true` if text was deleted.
    ///
    /// This is a single undo step. After deletion, the cursor is placed at
    /// the start of the deleted range and the selection is cleared.
    pub fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection_range() else {
            return false;
        };
        let start = range.start;
        self.apply_edit_replacement(range, "", Some(MutationKind::Replace));
        self.set_cursor_inner(start.min(self.text.len()));
        self.post_mutate();
        self.selection = None;
        true
    }

    /// Set the selection programmatically.
    pub fn set_selection(&mut self, anchor: usize, head: usize) {
        self.selection = Some(Selection { anchor, head });
    }

    /// Take the clipboard contents (returns `None` if empty).
    ///
    /// This is the primary way for the host app to retrieve text
    /// that was selected by mouse drag / double-click / triple-click.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    /// Peek at the current clipboard content without consuming it.
    pub fn clipboard(&self) -> Option<&str> {
        self.clipboard.as_deref()
    }

    /// Replace the clipboard provider. The default is [`InternalClipboard`]
    /// (in-memory only). Pass an `arboard`-backed implementation to sync
    /// copy/cut/paste with the system clipboard.
    pub fn set_clipboard_provider(&mut self, provider: Box<dyn ClipboardProvider>) {
        self.clipboard_provider = provider;
    }

    // ── Element events ──

    /// Take the pending [`TextElementEvent`], if any.
    ///
    /// Call this after [`handle_mouse`](Self::handle_mouse) to check whether
    /// an element was clicked or hover-entered/left.
    pub fn poll_element_event(&mut self) -> Option<TextElementEvent> {
        self.pending_element_event.take()
    }

    /// Internal: set clipboard text via the provider AND the notification field.
    fn set_clipboard_text(&mut self, text: String) {
        if !text.is_empty() {
            self.clipboard_provider.set(&text);
            self.clipboard = Some(text);
        }
    }

    // ── Timers / tick ──

    /// Recommended poll timeout for the host event loop.
    ///
    /// When the textarea has pending timer-driven work (e.g. continuous
    /// drag-scrolling while the mouse is held outside the area), this
    /// returns `Some(ms)`.  The host should use this as the
    /// `event::poll` timeout.  When the poll times out without an event,
    /// call [`tick`](Self::tick).
    ///
    /// Returns `None` when no timer work is pending — the host can use
    /// its own default timeout.
    pub fn poll_timeout_ms(&self) -> Option<u64> {
        // Drag-scroll is the only timer-driven feature for now.
        self.pending_drag_scroll.as_ref()?;
        let interval = Self::drag_scroll_interval(self.drag_scroll_steps);
        Some(interval as u64)
    }

    /// Advance timer-driven work (called by the host when `poll` times
    /// out).  Returns a `MouseAction` describing what changed (typically
    /// `SelectionUpdated` for drag-scroll, or `Nothing`).
    pub fn tick(&mut self, area: Rect, state: TextAreaState) -> MouseAction {
        // Drag-scroll continuation.
        if let Some(event) = self.pending_drag_scroll {
            return self.handle_mouse(event, area, state);
        }
        MouseAction::Nothing
    }

    // ── Mouse ──

    /// Shared single/double-click treatment of a click that landed on an
    /// element display (`hit_element`): snap the cursor to the element
    /// start, anchor drags there, and emit [`TextElementEventKind::Click`].
    ///
    /// Returns `None` when the click was not on an element.
    fn element_click_snap(&mut self, pos: usize, hit_element: bool) -> Option<MouseAction> {
        if !hit_element {
            return None;
        }
        let elem = self
            .elements
            .iter()
            .find(|e| pos >= e.range.start && pos <= e.range.end && !e.range.is_empty())?;
        let id = elem.id;
        let start = elem.range.start;
        self.set_cursor_inner(start);
        self.preferred_col = None;
        self.drag_anchor = Some(start);
        self.pending_element_event = Some(TextElementEvent {
            id,
            kind: TextElementEventKind::Click,
        });
        Some(MouseAction::CursorPlaced)
    }

    /// Process a crossterm `MouseEvent` and return what happened.
    ///
    /// The host app is expected to call this from its event loop for
    /// every `Event::Mouse(mouse)` and pass the textarea's render `area`
    /// plus the current `TextAreaState` (for scroll info).
    pub fn handle_mouse(
        &mut self,
        event: MouseEvent,
        area: Rect,
        state: TextAreaState,
    ) -> MouseAction {
        // ── Scrollbar interaction ──
        // When scrollbar is shown, clicks/drags on the rightmost column
        // control the scroll position instead of placing the cursor.
        let tw = self.text_width(area);
        let has_scrollbar = self.show_scrollbar && tw < area.width;
        let on_scrollbar = has_scrollbar && event.column == area.x + area.width - 1;

        // Handle scrollbar drag continuation (even if pointer moved off the column).
        if self.scrollbar_dragging {
            match event.kind {
                MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Down(MouseButton::Left) => {
                    return self.handle_scrollbar_click(event.row, area, tw);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.scrollbar_dragging = false;
                    return MouseAction::Scrolled;
                }
                _ => {}
            }
        }

        if on_scrollbar && let MouseEventKind::Down(MouseButton::Left) = event.kind {
            self.scrollbar_dragging = true;
            // If the click is on the thumb, don't jump — just start the drag
            // from the current position.  Only jump when clicking the track.
            if self.is_scrollbar_thumb_at(event.row, area, tw) {
                return MouseAction::Scrolled;
            }
            return self.handle_scrollbar_click(event.row, area, tw);
        }

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Some terminals re-emit Down(Left) after a scroll event
                // even though the button was held the whole time.  When a
                // drag is already active, treat this as a drag continuation
                // so the selection anchor is preserved.
                if self.drag_active {
                    return self.handle_mouse(
                        MouseEvent {
                            kind: MouseEventKind::Drag(MouseButton::Left),
                            ..event
                        },
                        area,
                        state,
                    );
                }

                let col = event.column;
                let row = event.row;

                // Track multi-click (double/triple).
                let click_count = self.click_tracker.register(col, row);

                // Record the mouse-down position (for drag detection).
                self.mouse_down_pos = Some((col, row));
                self.drag_active = false;
                self.last_drag_scroll = None;
                self.drag_scroll_steps = 0;
                self.pending_drag_scroll = None;

                // Clear any existing selection.
                self.clear_selection();

                // Map screen coordinates to buffer position.
                // IMPORTANT: this must happen BEFORE clearing scroll_override
                // so that effective_scroll uses the current viewport, not the
                // cursor-following fallback.
                let Some((pos, hit_element)) = self.buffer_pos_at_screen_ex(col, row, area, state)
                else {
                    self.scroll_override = None;
                    self.drag_anchor = None;
                    return MouseAction::Nothing;
                };

                // Now that we have the correct buffer position, clear the
                // scroll override so the viewport follows the cursor again.
                self.scroll_override = None;

                match click_count {
                    2 => {
                        // Double-click on an element display: snap like a
                        // single click (cursor to element start + Click
                        // event). Word-selecting would select and copy the
                        // element's hidden buffer text to the clipboard;
                        // the host decides what a chip double-click means.
                        // Triple-click line-select below intentionally keeps
                        // buffer-text semantics, element content included —
                        // a copy gesture, like drag-select across a chip.
                        if let Some(action) = self.element_click_snap(pos, hit_element) {
                            return action;
                        }
                        // Double-click: select word under cursor.
                        // Whitespace clicks just place the cursor (no selection).
                        let is_ws = pos < self.text.len()
                            && self.text[pos..]
                                .chars()
                                .next()
                                .is_none_or(|ch| ch.is_whitespace());
                        let start = self.word_start_at(pos);
                        let end = self.word_end_at(pos);
                        if !is_ws && start < end {
                            self.selection = Some(Selection {
                                anchor: start,
                                head: end,
                            });
                            // Place cursor on the last character of the
                            // selection (neovim style), not one past the end.
                            let cursor = self.text[start..end]
                                .char_indices()
                                .next_back()
                                .map(|(i, _)| start + i)
                                .unwrap_or(start);
                            self.set_cursor_inner(cursor);
                            self.preferred_col = None;
                            if let Some(text) = self.selected_text() {
                                self.set_clipboard_text(text);
                            }
                            return MouseAction::SelectionFinished;
                        }
                        // Clicked on whitespace — just place cursor.
                        self.set_cursor_inner(pos);
                        self.preferred_col = None;
                        MouseAction::CursorPlaced
                    }
                    3 => {
                        // Triple-click: select entire source line (\n-delimited).
                        let line_start = self.beginning_of_line(pos);
                        // Include the trailing \n if present.
                        let line_end_excl = self.end_of_line(pos);
                        let line_end = if line_end_excl < self.text.len() {
                            line_end_excl + 1 // include \n
                        } else {
                            line_end_excl
                        };
                        self.selection = Some(Selection {
                            anchor: line_start,
                            head: line_end,
                        });
                        // Keep cursor at the click position (like neovim),
                        // not at the end of the selection.
                        self.set_cursor_inner(pos);
                        self.preferred_col = None;
                        if let Some(text) = self.selected_text() {
                            self.set_clipboard_text(text);
                        }
                        MouseAction::SelectionFinished
                    }
                    _ => {
                        // Single click: place cursor.
                        //
                        // If click landed on an element display, snap cursor
                        // to elem start. `hit_element` is reliable because
                        // display_col_to_buffer_pos sets it when the column
                        // falls within an element's visual width.
                        if let Some(action) = self.element_click_snap(pos, hit_element) {
                            return action;
                        }

                        self.drag_anchor = Some(pos);
                        self.set_cursor_inner(pos);
                        self.preferred_col = None;

                        MouseAction::CursorPlaced
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(anchor) = self.drag_anchor else {
                    return MouseAction::Nothing;
                };

                // Compute the buffer position for the drag endpoint.
                // We need to scope the `lines` borrow so it's dropped before
                // we mutate self.

                // Throttle drag-scroll (above/below area) to avoid
                // lightning-fast scrolling at mouse-report rate.
                // Acceleration: first step waits 80ms, then 60ms, then 40ms.
                let outside_area = event.row < area.y || event.row >= area.y + area.height;
                if outside_area {
                    // Store event for continuous drag-scroll re-triggering.
                    self.pending_drag_scroll = Some(event);

                    let now = Instant::now();
                    let interval = Self::drag_scroll_interval(self.drag_scroll_steps);
                    if let Some(last) = self.last_drag_scroll
                        && now.duration_since(last).as_millis() < interval
                    {
                        return MouseAction::Nothing;
                    }
                    self.last_drag_scroll = Some(now);
                    self.drag_scroll_steps = self.drag_scroll_steps.saturating_add(1);
                } else {
                    // Back inside area — cancel continuous drag-scroll.
                    self.pending_drag_scroll = None;
                }

                let (head, new_scroll) = {
                    let tw = self.text_width(area);
                    let lines = self.wrapped_lines(tw);
                    let scroll = self.effective_scroll(area.height, &lines, state.scroll) as usize;
                    let visible_end = scroll + area.height as usize;

                    if event.row < area.y {
                        // ── Dragging above the area → scroll up ──
                        let dist = area.y - event.row;
                        let n = Self::drag_scroll_lines_for_distance(dist);
                        let target_line = scroll.saturating_sub(n);
                        let pos = if target_line < lines.len() {
                            let col = event.column.saturating_sub(area.x) as usize;
                            let line = &lines[target_line];
                            let line_end = line.end.min(self.text.len());
                            let p = self.display_col_to_buffer_pos(line.start, line_end, col).0;
                            self.clamp_to_line(p, line.start, line_end)
                        } else {
                            0
                        };
                        (pos, Some(target_line as u16))
                    } else if event.row >= area.y + area.height {
                        // ── Dragging below the area → scroll down ──
                        let dist = event.row - (area.y + area.height) + 1;
                        let n = Self::drag_scroll_lines_for_distance(dist);
                        let target_line = (visible_end + n - 1).min(lines.len().saturating_sub(1));
                        let max_scroll = lines.len().saturating_sub(area.height as usize);
                        let new_scroll = (target_line + 1)
                            .saturating_sub(area.height as usize)
                            .min(max_scroll);
                        let pos = if target_line < lines.len() {
                            let col = event.column.saturating_sub(area.x) as usize;
                            let line = &lines[target_line];
                            let line_end = line.end.min(self.text.len());
                            let pos = self.display_col_to_buffer_pos(line.start, line_end, col).0;
                            self.clamp_to_line(pos, line.start, line_end)
                        } else {
                            self.text.len()
                        };
                        (pos, Some(new_scroll as u16))
                    } else {
                        // ── Within the area → normal drag ──
                        let col = event.column.clamp(area.x, area.x + tw.saturating_sub(1));
                        let row = event.row;
                        drop(lines); // release borrow for buffer_pos_at_screen
                        match self.buffer_pos_at_screen(col, row, area, state) {
                            Some(pos) => (pos, None),
                            None => return MouseAction::Nothing,
                        }
                    }
                };

                if let Some(s) = new_scroll {
                    self.scroll_override = Some(s);
                }
                if head == anchor {
                    self.drag_active = false;
                    self.selection = None;
                } else {
                    self.drag_active = true;
                    self.selection = Some(Selection { anchor, head });
                }
                self.set_cursor_inner(head);
                self.preferred_col = None;

                if self.selection.is_some() {
                    MouseAction::SelectionUpdated
                } else {
                    MouseAction::CursorPlaced
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.mouse_down_pos = None;
                let was_drag = self.drag_active;
                self.drag_active = false;
                self.scrollbar_dragging = false;
                self.pending_drag_scroll = None;
                self.drag_anchor = None;

                if was_drag {
                    // Discard zero-width selections (anchor == head) that arise
                    // from mouse jitter — they look like an active selection to
                    // the keyboard handler and silently swallow Backspace/Delete.
                    if self.selection_range().is_none() {
                        self.selection = None;
                        MouseAction::CursorPlaced
                    } else {
                        // Finalize selection: copy to clipboard.
                        if let Some(text) = self.selected_text()
                            && !text.is_empty()
                        {
                            self.set_clipboard_text(text);
                        }

                        if !self.keep_selection_after_mouseup {
                            self.selection = None;
                        }

                        MouseAction::SelectionFinished
                    }
                } else {
                    MouseAction::Nothing
                }
            }
            MouseEventKind::ScrollDown => {
                let tw = self.text_width(area);
                let lines = self.wrapped_lines(tw);
                let total = lines.len();
                if total <= area.height as usize {
                    return MouseAction::Nothing;
                }
                let max_scroll = total.saturating_sub(area.height as usize) as u16;
                let current = self
                    .scroll_override
                    .unwrap_or_else(|| self.effective_scroll(area.height, &lines, state.scroll));
                let scroll_lines = Self::scroll_lines_for_height(area.height);
                let new_scroll = (current + scroll_lines).min(max_scroll);
                if new_scroll == current {
                    return MouseAction::Nothing;
                }
                // If dragging, extend the selection head to follow the scroll.
                let drag_new_pos = if self.drag_active {
                    let target_line =
                        (new_scroll as usize + area.height as usize - 1).min(lines.len() - 1);
                    Some(lines[target_line].start)
                } else {
                    None
                };
                drop(lines);
                self.scroll_override = Some(new_scroll);
                if let Some(new_pos) = drag_new_pos {
                    if let Some(sel) = &mut self.selection {
                        sel.head = new_pos;
                    }
                    self.set_cursor_inner(new_pos);
                }
                MouseAction::Scrolled
            }
            MouseEventKind::ScrollUp => {
                let tw = self.text_width(area);
                let lines = self.wrapped_lines(tw);
                let total = lines.len();
                if total <= area.height as usize {
                    return MouseAction::Nothing;
                }
                let current = self
                    .scroll_override
                    .unwrap_or_else(|| self.effective_scroll(area.height, &lines, state.scroll));
                let scroll_lines = Self::scroll_lines_for_height(area.height);
                let new_scroll = current.saturating_sub(scroll_lines);
                if new_scroll == current {
                    return MouseAction::Nothing;
                }
                // If dragging, extend the selection head to follow the scroll.
                let drag_new_pos = if self.drag_active {
                    let target_line = new_scroll as usize;
                    Some(if target_line < lines.len() {
                        lines[target_line].start
                    } else {
                        0
                    })
                } else {
                    None
                };
                drop(lines);
                self.scroll_override = Some(new_scroll);
                if let Some(new_pos) = drag_new_pos {
                    if let Some(sel) = &mut self.selection {
                        sel.head = new_pos;
                    }
                    self.set_cursor_inner(new_pos);
                }
                MouseAction::Scrolled
            }
            MouseEventKind::Moved => {
                // Hover detection: hit-test elements under the cursor.
                let hovered_id = self
                    .element_at_screen(event.column, event.row, area, state)
                    .map(|e| e.id);

                let prev = self.hovered_element;
                if hovered_id != prev {
                    // Emit leave for the old element first, then enter for the new one.
                    // We only store the last event; if both happen, prefer enter
                    // (the caller already knows about the old element from a prior enter).
                    if let Some(old_id) = prev {
                        self.pending_element_event = Some(TextElementEvent {
                            id: old_id,
                            kind: TextElementEventKind::HoverLeave,
                        });
                    }
                    if let Some(new_id) = hovered_id {
                        self.pending_element_event = Some(TextElementEvent {
                            id: new_id,
                            kind: TextElementEventKind::HoverEnter,
                        });
                    }
                    self.hovered_element = hovered_id;
                }
                MouseAction::Nothing
            }
            _ => MouseAction::Nothing,
        }
    }

    /// Handle a click or drag on the scrollbar track.
    ///
    /// Maps the row position proportionally to a scroll offset:
    /// clicking at the top of the track scrolls to the start, at the
    /// bottom scrolls to the end.
    fn handle_scrollbar_click(&mut self, row: u16, area: Rect, tw: u16) -> MouseAction {
        if area.height == 0 {
            return MouseAction::Nothing;
        }
        let total = {
            let lines = self.wrapped_lines(tw);
            lines.len()
        };
        if total <= area.height as usize {
            return MouseAction::Nothing;
        }
        let max_scroll = total.saturating_sub(area.height as usize) as u16;
        let rel_row = row.saturating_sub(area.y);
        // Map relative row to a scroll offset proportionally.
        let scroll = if area.height <= 1 {
            0
        } else {
            ((rel_row as u32 * max_scroll as u32) / (area.height.saturating_sub(1)) as u32) as u16
        };
        self.scroll_override = Some(scroll.min(max_scroll));
        MouseAction::Scrolled
    }

    /// Check whether the given screen row falls on the scrollbar thumb.
    ///
    /// Renders the scrollbar into a scratch buffer and checks whether the
    /// cell at `row` is a non-space character (thumb glyph) or a space (track).
    fn is_scrollbar_thumb_at(&self, row: u16, area: Rect, tw: u16) -> bool {
        if area.height == 0 {
            return false;
        }
        let total = {
            let lines = self.wrapped_lines(tw);
            lines.len()
        };
        if total <= area.height as usize {
            return false;
        }
        let current_scroll = self.scroll_override.unwrap_or(0);

        let lengths = ScrollLengths {
            content_len: total,
            viewport_len: area.height as usize,
        };
        let scrollbar = ScrollBar::vertical(lengths).offset(current_scroll as usize);
        let sb_x = area.right().saturating_sub(1);
        let core_area = CoreRect {
            x: sb_x,
            y: area.y,
            width: 1,
            height: area.height,
        };
        let mut scratch = CoreBuffer::empty(core_area);
        (&scrollbar).render(core_area, &mut scratch);

        if row < area.y || row >= area.y + area.height {
            return false;
        }
        scratch[(sb_x, row)].symbol() != " "
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn is_word_char(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    /// Classify a character into a word-class for double-click selection.
    ///
    /// Three classes (matching vim/neovim `w` word definition):
    /// - `0`: whitespace
    /// - `1`: word chars (alphanumeric + underscore)
    /// - `2`: punctuation / everything else
    fn char_class(ch: char) -> u8 {
        if ch.is_whitespace() {
            0
        } else if Self::is_word_char(ch) {
            1
        } else {
            2
        }
    }

    /// Find the start of the word containing `pos` (for double-click selection).
    ///
    /// Uses vim-style word classes: word chars (alphanumeric + `_`), punctuation,
    /// and whitespace are three distinct groups.  Scans backward until the class
    /// changes.
    ///
    /// If `pos` is inside an element, returns the element start.
    fn word_start_at(&self, pos: usize) -> usize {
        // If inside an element, return element start.
        if let Some(elem) = self
            .elements
            .iter()
            .find(|e| pos >= e.range.start && pos < e.range.end)
        {
            return elem.range.start;
        }

        // Determine the class of the character at `pos` (or just before if at end).
        let target_class = if pos < self.text.len() {
            Self::char_class(self.text[pos..].chars().next().unwrap())
        } else if pos > 0 {
            let ch = self.text[..pos].chars().next_back().unwrap();
            Self::char_class(ch)
        } else {
            return 0;
        };

        let before = &self.text[..pos];
        let word_start = before
            .char_indices()
            .rev()
            .find(|&(_, ch)| Self::char_class(ch) != target_class)
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(0);
        self.adjust_pos_out_of_elements(word_start, true)
    }

    /// Find the end of the word containing `pos` (for double-click selection).
    ///
    /// Uses vim-style word classes (see [`Self::char_class`]).
    ///
    /// If `pos` is inside an element, returns the element end.
    fn word_end_at(&self, pos: usize) -> usize {
        // If inside an element, return element end.
        if let Some(elem) = self
            .elements
            .iter()
            .find(|e| pos >= e.range.start && pos < e.range.end)
        {
            return elem.range.end;
        }

        // Determine the class of the character at `pos`.
        let target_class = if pos < self.text.len() {
            Self::char_class(self.text[pos..].chars().next().unwrap())
        } else {
            return self.text.len();
        };

        let after = &self.text[pos..];
        let word_end = after
            .char_indices()
            .find(|&(_, ch)| Self::char_class(ch) != target_class)
            .map(|(rel_idx, _)| pos + rel_idx)
            .unwrap_or(self.text.len());
        self.adjust_pos_out_of_elements(word_end, false)
    }

    fn current_display_col(&self) -> usize {
        let bol = self.beginning_of_current_line();
        self.display_width_of_range(bol, self.cursor())
    }

    /// Compute the display width of the buffer range `[from..to)`.
    ///
    /// Plain runs use tab-aware width (`tab_width` columns per `\t`, or
    /// unicode-width when `tab_width == 0`). Element ranges with a custom
    /// `display` use the element's display width instead of the buffer text
    /// width. This is the core of the display projection system.
    fn display_width_of_range(&self, from: usize, to: usize) -> usize {
        if from >= to {
            return 0;
        }
        let mut width = 0usize;
        let mut pos = from;

        for elem in &self.elements {
            if elem.range.start >= to {
                break; // elements are sorted, no more overlap possible
            }
            if elem.range.end <= pos {
                continue; // element is entirely before our current position
            }

            // Plain text before this element
            if pos < elem.range.start {
                let plain_end = elem.range.start.min(to);
                width += self.plain_display_width(&self.text[pos..plain_end]);
                pos = plain_end;
            }
            if pos >= to {
                break;
            }

            // Element region
            let elem_start_in_range = elem.range.start.max(pos);
            let elem_end_in_range = elem.range.end.min(to);
            if elem_start_in_range < elem_end_in_range {
                if let Some(display) = &elem.display {
                    // If the range covers the entire element (or starts at element start),
                    // use the full display width. If it covers only a partial overlap
                    // (cursor inside element — shouldn't happen normally), fall back to
                    // buffer text width.
                    if elem_start_in_range == elem.range.start {
                        let display_w: usize = display
                            .spans
                            .iter()
                            .map(|s| s.content.as_ref().width())
                            .sum();
                        width += display_w;
                    } else {
                        width += self.plain_display_width(
                            &self.text[elem_start_in_range..elem_end_in_range],
                        );
                    }
                } else {
                    width += self
                        .plain_display_width(&self.text[elem_start_in_range..elem_end_in_range]);
                }
                pos = elem_end_in_range;
            }
        }

        // Remaining plain text after all elements
        if pos < to {
            width += self.plain_display_width(&self.text[pos..to]);
        }

        width
    }

    fn wrapped_line_index_by_start(lines: &[Range<usize>], pos: usize) -> Option<usize> {
        // partition_point returns the index of the first element for which
        // the predicate is false, i.e. the count of elements with start <= pos.
        let idx = lines.partition_point(|r| r.start <= pos);
        if idx == 0 { None } else { Some(idx - 1) }
    }

    /// Map a display column to a buffer byte position on a given wrapped line.
    ///
    /// Pure query — does not mutate any state. Handles elements (snapping to
    /// nearest element boundary) and wide unicode graphemes.
    /// If `target_col` is past the line's display width, returns `line_end`
    /// (clamped to the nearest element boundary).
    ///
    /// Returns `(byte_pos, hit_element)` where `hit_element` is `true` when
    /// the column fell on an element's display region.
    fn display_col_to_buffer_pos(
        &self,
        line_start: usize,
        line_end: usize,
        target_col: usize,
    ) -> (usize, bool) {
        let mut width_so_far = 0usize;
        let mut pos = line_start;

        while pos < line_end {
            // Check if pos is at or inside an element
            if let Some(elem_idx) = self
                .elements
                .iter()
                .position(|e| pos >= e.range.start && pos < e.range.end)
            {
                let elem = &self.elements[elem_idx];
                let elem_start = elem.range.start;
                let elem_buf_end = elem.range.end;
                // The visible portion of the element on this line
                let elem_line_end = elem_buf_end.min(line_end);

                if pos == elem_start {
                    // We're at the start of an element — treat it as a whole unit.
                    let elem_display_w = if let Some(display) = &elem.display {
                        display
                            .spans
                            .iter()
                            .map(|s| s.content.as_ref().width())
                            .sum()
                    } else {
                        self.plain_display_width(&self.text[elem_start..elem_line_end])
                    };

                    if width_so_far + elem_display_w > target_col {
                        // Click landed on this element display — snap to the
                        // nearer boundary (start vs end of the underlying
                        // buffer text) so that drag-selection works naturally.
                        let dist_start = target_col.saturating_sub(width_so_far);
                        let dist_end = elem_display_w.saturating_sub(dist_start);
                        if dist_start <= dist_end {
                            return (elem_start, true);
                        } else {
                            return (elem_buf_end, true);
                        }
                    }
                    width_so_far += elem_display_w;
                    pos = elem_buf_end.min(line_end); // move past element (or to line end)
                } else {
                    // We're in the middle of an element (e.g. a wrapped line starts
                    // mid-element). Skip past the rest of the element on this line.
                    let partial_w = self.plain_display_width(&self.text[pos..elem_line_end]);
                    if width_so_far + partial_w > target_col {
                        // Snap to element's actual end boundary
                        return (elem_buf_end, true);
                    }
                    width_so_far += partial_w;
                    pos = elem_buf_end.min(line_end); // move past element (or to line end)
                }
                continue;
            }

            // Plain text grapheme
            let slice = &self.text[pos..line_end];
            if let Some(grapheme) = slice.graphemes(true).next() {
                let grapheme_width = self.grapheme_display_width(grapheme);
                width_so_far += grapheme_width;
                if width_so_far > target_col {
                    return (self.clamp_pos_to_nearest_boundary(pos), false);
                }
                pos += grapheme.len();
            } else {
                break;
            }
        }

        (self.clamp_pos_to_nearest_boundary(line_end), false)
    }

    fn move_to_display_col_on_line(
        &mut self,
        line_start: usize,
        line_end: usize,
        target_col: usize,
    ) {
        let cursor = self
            .display_col_to_buffer_pos(line_start, line_end, target_col)
            .0;
        self.set_cursor_inner(cursor);
    }

    fn beginning_of_line(&self, pos: usize) -> usize {
        // Scan backward for '\n' that is NOT inside an element.
        // Newlines inside elements (e.g. multi-line paste) are not line boundaries.
        for i in (0..pos).rev() {
            if self.text.as_bytes()[i] == b'\n' && !self.is_inside_element(i) {
                return i + 1;
            }
        }
        0
    }
    fn beginning_of_current_line(&self) -> usize {
        self.beginning_of_line(self.cursor())
    }

    fn end_of_line(&self, pos: usize) -> usize {
        // Scan forward for '\n' that is NOT inside an element.
        for i in pos..self.text.len() {
            if self.text.as_bytes()[i] == b'\n' && !self.is_inside_element(i) {
                return i;
            }
        }
        self.text.len()
    }
    fn end_of_current_line(&self) -> usize {
        self.end_of_line(self.cursor())
    }

    /// Check if a byte position is inside (strictly within) an element.
    fn is_inside_element(&self, pos: usize) -> bool {
        self.elements
            .iter()
            .any(|e| pos >= e.range.start && pos < e.range.end)
    }

    fn apply_classified_command(&mut self, command: EditCommand) {
        if let EditCommand::Insert(character) = command {
            self.insert_str(&character.to_string());
            return;
        }
        let mutation_kind = match command.category() {
            EditCommandCategory::Insert => unreachable!("insert commands return above"),
            EditCommandCategory::Navigation => None,
            EditCommandCategory::Delete => Some(MutationKind::Delete),
            EditCommandCategory::Kill => Some(MutationKind::Kill),
        };
        self.apply_edit_command(command, mutation_kind);
    }

    pub fn input(&mut self, event: KeyEvent) {
        // ── Selection-aware interception ──
        // When a selection is active, certain keys interact with the selected
        // range rather than performing their normal single-char action.
        if self.selection.is_some() {
            if let Some(EditCommand::Insert(character)) = classify_key_event(&event) {
                self.begin_undo_group();
                if !self.delete_selection() {
                    self.clear_selection();
                }
                self.insert_str(&character.to_string());
                self.end_undo_group();
                return;
            }
            match event {
                // Enter / Ctrl-J/M → replace selection with newline.
                KeyEvent {
                    code: KeyCode::Char('j' | 'm'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Enter,
                    ..
                } => {
                    self.begin_undo_group();
                    if !self.delete_selection() {
                        self.clear_selection();
                    }
                    self.insert_str("\n");
                    self.end_undo_group();
                    return;
                }
                // Backspace / Delete → delete the selection only (no extra char).
                // If the selection is zero-width (anchor == head), delete_selection()
                // returns false — clear the stale selection and fall through to the
                // normal single-char delete so Backspace/Delete aren't silently swallowed.
                KeyEvent {
                    code: KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('\x08' | '\x7f'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('h'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('d'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    if self.delete_selection() {
                        return;
                    }
                    // Zero-width selection — clear and fall through.
                    self.clear_selection();
                }
                // Ctrl-X → cut selection (copy to clipboard + delete).
                KeyEvent {
                    code: KeyCode::Char('x'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    if let Some(text) = self.selected_text() {
                        self.set_clipboard_text(text);
                    }
                    if self.delete_selection() {
                        return;
                    }
                    // Zero-width selection — clear and fall through.
                    self.clear_selection();
                }
                // All other keys → clear selection, fall through to normal handling.
                _ => {
                    self.clear_selection();
                }
            }
        }

        if let Some(command) = classify_key_event(&event) {
            self.apply_classified_command(command);
            return;
        }

        match event {
            KeyEvent {
                code: KeyCode::Char('j' | 'm'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Enter,
                ..
            } => self.insert_str("\n"),
            KeyEvent {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.yank();
            }

            // Undo / Redo (Ctrl or Cmd)
            KeyEvent {
                code: KeyCode::Char('Z'),
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL)
                || modifiers.contains(KeyModifiers::SUPER) =>
            {
                // Ctrl/Cmd-Shift-Z → redo (terminals that report uppercase Z + Shift)
                self.redo();
            }
            k if is_undo_input(&k) => {
                self.undo();
            }
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.redo();
            }

            // Ctrl-V → paste from clipboard provider.
            KeyEvent {
                code: KeyCode::Char('v'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                if let Some(text) = self.clipboard_provider.get() {
                    self.insert_str(&text);
                }
            }

            // Cmd+Left / Cmd+Right (macOS): terminals using the Kitty keyboard
            // protocol (Ghostty, Kitty, WezTerm) send these as Super+Arrow.
            KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::SUPER,
                ..
            } => {
                self.move_cursor_to_beginning_of_line(false);
            }
            KeyEvent {
                code: KeyCode::Right,
                modifiers: KeyModifiers::SUPER,
                ..
            } => {
                self.move_cursor_to_end_of_line(false);
            }
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.move_cursor_up();
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.move_cursor_down();
            }
            // Home/End → visual row; Ctrl+A/E → logical line (emacs).
            KeyEvent {
                code: KeyCode::Home,
                ..
            } => {
                self.move_cursor_to_beginning_of_line(false);
            }

            KeyEvent {
                code: KeyCode::End, ..
            } => {
                self.move_cursor_to_end_of_line(false);
            }
            _o => {
                #[cfg(feature = "debug-logs")]
                tracing::debug!("Unhandled key event in TextArea: {:?}", _o);
            }
        }
    }

    // ── Undo/Redo ──

    /// Create a snapshot of the current textarea state.
    fn snapshot(&self) -> UndoEntry {
        UndoEntry {
            text: self.text().to_owned(),
            cursor: self.cursor(),
            elements: self.elements.clone(),
        }
    }

    /// Restore the textarea state from a snapshot.
    fn restore(&mut self, entry: UndoEntry) {
        self.text = EditBuffer::from_parts(entry.text, entry.cursor);
        self.elements = entry.elements;
        self.wrap_cache.replace(None);
        self.preferred_col = None;
        // Note: next_element_id is intentionally NOT restored — it only increases.
        // Note: kill_buffer is intentionally NOT restored — yank is separate from undo.
    }

    /// Called before a mutation to decide whether to push a new undo checkpoint.
    ///
    /// Batching rules:
    /// - Inside an undo group (`group_depth > 0`) → skip entirely.
    /// - First mutation ever → always checkpoint.
    /// - Kind changed from last → checkpoint.
    /// - Cursor moved since last mutation (arrows, clicks) → checkpoint.
    /// - Kill / Element / Replace → always checkpoint (discrete actions).
    /// - Same Insert or Delete with consecutive cursor → extend batch (no checkpoint).
    /// - Word boundary (ws↔non-ws transition) → checkpoint (handled by callers
    ///   resetting `last_kind` before calling this method).
    fn pre_mutate(&mut self, kind: MutationKind) {
        // Inside an undo group — the group handles its own checkpoint.
        if self.undo.group_depth > 0 {
            return;
        }

        let should_push = match self.undo.last_kind {
            None => true,
            Some(prev) => {
                prev != kind
                    || self.cursor() != self.undo.last_cursor
                    || matches!(
                        kind,
                        MutationKind::Kill | MutationKind::Element | MutationKind::Replace
                    )
            }
        };

        if should_push {
            let entry = self.snapshot();
            self.undo.stack.push(entry);
            if self.undo.stack.len() > self.undo.max_depth {
                self.undo.stack.remove(0);
            }
        }
        self.undo.redo.clear();
        self.undo.last_kind = Some(kind);
    }

    /// Update `last_cursor` after a mutation completes so the next `pre_mutate`
    /// can detect cursor jumps.
    fn post_mutate(&mut self) {
        self.undo.last_cursor = self.cursor();
    }

    /// Clear the undo/redo history, leaving the current text and cursor
    /// untouched.
    ///
    /// Use this when a buffer is reset to represent a *new logical
    /// context* — e.g. a shared input widget that is reused for a
    /// different target — so that a later `undo` can't resurrect text
    /// that belonged to the previous context. `set_text` deliberately
    /// records a checkpoint (so an accidental replace is undoable), so
    /// callers that want a hard reset must follow it with this.
    pub fn clear_history(&mut self) {
        self.undo.stack.clear();
        self.undo.redo.clear();
        self.undo.last_kind = None;
        self.undo.last_cursor = self.cursor();
    }

    /// Undo the last mutation. Returns `true` if there was something to undo.
    pub fn undo(&mut self) -> bool {
        if let Some(entry) = self.undo.stack.pop() {
            self.scroll_override = None;
            let current = self.snapshot();
            self.undo.redo.push(current);
            self.restore(entry);
            // Reset batching — next mutation starts a fresh group.
            self.undo.last_kind = None;
            self.undo.last_cursor = self.cursor();
            true
        } else {
            false
        }
    }

    /// Redo the last undone mutation. Returns `true` if there was something to redo.
    pub fn redo(&mut self) -> bool {
        if let Some(entry) = self.undo.redo.pop() {
            self.scroll_override = None;
            let current = self.snapshot();
            self.undo.stack.push(current);
            self.restore(entry);
            // Reset batching — next mutation starts a fresh group.
            self.undo.last_kind = None;
            self.undo.last_cursor = self.cursor();
            true
        } else {
            false
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undo.redo.is_empty()
    }

    /// Begin an undo group. All mutations between `begin_undo_group()` and
    /// `end_undo_group()` are collapsed into a single undo step.
    ///
    /// Groups can be nested: only the outermost `end_undo_group()` pushes
    /// the checkpoint. Inner begin/end pairs are reference-counted.
    ///
    /// Use cases:
    /// - Autocomplete: `replace_range_with_element` + `insert_str(" ")` = 1 undo step
    /// - Line-select: enter → N live-updates → confirm = 1 undo step
    pub fn begin_undo_group(&mut self) {
        if self.undo.group_depth == 0 {
            // Outermost group — take the snapshot.
            self.undo.group_checkpoint = Some(self.snapshot());
        }
        self.undo.group_depth += 1;
    }

    /// End an undo group. If this closes the outermost group and the state
    /// actually changed, a single undo entry is pushed.
    pub fn end_undo_group(&mut self) {
        if self.undo.group_depth == 0 {
            return; // Unbalanced call — ignore.
        }
        self.undo.group_depth -= 1;
        if self.undo.group_depth == 0 {
            if let Some(checkpoint) = self.undo.group_checkpoint.take() {
                // Only push if state actually changed.
                let changed = checkpoint.text.as_str() != self.text()
                    || checkpoint.cursor != self.cursor()
                    || checkpoint.elements.len() != self.elements.len();
                if changed {
                    self.undo.stack.push(checkpoint);
                    if self.undo.stack.len() > self.undo.max_depth {
                        self.undo.stack.remove(0);
                    }
                    self.undo.redo.clear();
                }
            }
            // Reset batching state so the next mutation starts fresh.
            self.undo.last_kind = None;
            self.undo.last_cursor = self.cursor();
        }
    }

    /// Cancel an undo group. Restores the textarea to the state it was in
    /// when `begin_undo_group()` was called — no undo entry is created.
    ///
    /// Use case: line-select cancel → revert all live-updates, leave no trace.
    pub fn cancel_undo_group(&mut self) {
        if self.undo.group_depth == 0 {
            return; // Unbalanced call — ignore.
        }
        // Always restore to the outermost checkpoint, regardless of nesting.
        self.undo.group_depth = 0;
        if let Some(checkpoint) = self.undo.group_checkpoint.take() {
            self.restore(checkpoint);
        }
        // Reset batching state.
        self.undo.last_kind = None;
        self.undo.last_cursor = self.cursor();
    }

    // ####### Input Functions #######
    pub fn delete_backward(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if n == 1 {
            self.apply_edit_command(
                EditCommand::DeleteGraphemeBackward,
                Some(MutationKind::Delete),
            );
            return;
        }
        self.begin_undo_group();
        for _ in 0..n {
            if matches!(
                self.apply_edit_command(
                    EditCommand::DeleteGraphemeBackward,
                    Some(MutationKind::Delete),
                ),
                EditOutcome::Unchanged
            ) {
                break;
            }
        }
        self.end_undo_group();
    }

    pub fn delete_forward(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if n == 1 {
            self.apply_edit_command(
                EditCommand::DeleteGraphemeForward,
                Some(MutationKind::Delete),
            );
            return;
        }
        self.begin_undo_group();
        for _ in 0..n {
            if matches!(
                self.apply_edit_command(
                    EditCommand::DeleteGraphemeForward,
                    Some(MutationKind::Delete),
                ),
                EditOutcome::Unchanged
            ) {
                break;
            }
        }
        self.end_undo_group();
    }

    pub fn delete_backward_word(&mut self) {
        self.apply_edit_command(
            EditCommand::DeleteWordBackward(WordStyle::Small),
            Some(MutationKind::Kill),
        );
    }

    /// readline `unix-word-rubout` (whitespace-delimited), vs
    /// [`Self::delete_backward_word`]'s punctuation-chunked M-DEL semantics.
    pub fn delete_backward_unix_word(&mut self) {
        self.apply_edit_command(
            EditCommand::DeleteWordBackward(WordStyle::WhitespaceDelimited),
            Some(MutationKind::Kill),
        );
    }

    /// Delete text to the right of the cursor using readline-style word semantics.
    ///
    /// Deletes from the current cursor position through the end of the next word as determined
    /// by `end_of_next_word()`. Any delimiters between the cursor and that word
    /// (whitespace, punctuation, newlines) are included in the deletion.
    pub fn delete_forward_word(&mut self) {
        self.apply_edit_command(
            EditCommand::DeleteWordForward(WordStyle::Small),
            Some(MutationKind::Kill),
        );
    }

    pub fn kill_to_end_of_line(&mut self) {
        self.apply_edit_command(EditCommand::DeleteToLineEnd, Some(MutationKind::Kill));
    }

    pub fn kill_to_beginning_of_line(&mut self) {
        self.apply_edit_command(EditCommand::DeleteToLineStart, Some(MutationKind::Kill));
    }

    /// Kill the entire current line (BOL to EOL), regardless of cursor position.
    /// If the line is already empty, consumes the preceding newline to join lines.
    pub fn kill_current_line(&mut self) {
        let bol = self.beginning_of_current_line();
        let eol = self.end_of_current_line();

        let range = if bol == eol {
            if bol > 0 { Some(bol - 1..bol) } else { None }
        } else {
            Some(bol..eol)
        };

        if let Some(range) = range {
            self.apply_edit_replacement(range, "", Some(MutationKind::Kill));
        }
    }

    pub fn yank(&mut self) {
        if self.kill_buffer.is_empty() {
            return;
        }
        let text = self.kill_buffer.clone();
        self.apply_edit_replacement(
            self.cursor()..self.cursor(),
            &text,
            Some(MutationKind::Insert),
        );
        if let Some(last) = text.chars().last() {
            self.undo.last_insert_ws = last.is_whitespace();
        }
    }

    /// Move the cursor left by a single grapheme cluster.
    pub fn move_cursor_left(&mut self) {
        self.apply_edit_command(EditCommand::MoveGraphemeLeft, None);
    }

    /// Move the cursor right by a single grapheme cluster.
    pub fn move_cursor_right(&mut self) {
        self.apply_edit_command(EditCommand::MoveGraphemeRight, None);
    }

    pub fn move_cursor_up(&mut self) {
        self.scroll_override = None;
        // If we have a wrapping cache, prefer navigating across wrapped (visual) lines.
        if let Some((target_col, maybe_line)) = {
            let cache_ref = self.wrap_cache.borrow();
            if let Some(cache) = cache_ref.as_ref() {
                let lines = &cache.lines;
                if let Some(idx) = Self::wrapped_line_index_by_start(lines, self.cursor()) {
                    let cur_range = &lines[idx];
                    let target_col = self.preferred_col.unwrap_or_else(|| {
                        self.display_width_of_range(cur_range.start, self.cursor())
                    });
                    if idx > 0 {
                        let prev = &lines[idx - 1];
                        let line_start = prev.start;
                        let line_end = prev.end;
                        Some((target_col, Some((line_start, line_end))))
                    } else {
                        Some((target_col, None))
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } {
            // We had wrapping info. Apply movement accordingly.
            match maybe_line {
                Some((line_start, line_end)) => {
                    if self.preferred_col.is_none() {
                        self.preferred_col = Some(target_col);
                    }
                    self.move_to_display_col_on_line(line_start, line_end, target_col);
                    return;
                }
                None => {
                    // Already at first visual line -> move to start
                    self.set_cursor_inner(0);
                    self.preferred_col = None;
                    return;
                }
            }
        }

        // Fallback to logical line navigation if we don't have wrapping info yet.
        if let Some(prev_nl) = self.text[..self.cursor()].rfind('\n') {
            let target_col = match self.preferred_col {
                Some(c) => c,
                None => {
                    let c = self.current_display_col();
                    self.preferred_col = Some(c);
                    c
                }
            };
            let prev_line_start = self.text[..prev_nl].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let prev_line_end = prev_nl;
            self.move_to_display_col_on_line(prev_line_start, prev_line_end, target_col);
        } else {
            self.set_cursor_inner(0);
            self.preferred_col = None;
        }
    }

    pub fn move_cursor_down(&mut self) {
        self.scroll_override = None;
        // If we have a wrapping cache, prefer navigating across wrapped (visual) lines.
        if let Some((target_col, move_to_last)) = {
            let cache_ref = self.wrap_cache.borrow();
            if let Some(cache) = cache_ref.as_ref() {
                let lines = &cache.lines;
                if let Some(idx) = Self::wrapped_line_index_by_start(lines, self.cursor()) {
                    let cur_range = &lines[idx];
                    let target_col = self.preferred_col.unwrap_or_else(|| {
                        self.display_width_of_range(cur_range.start, self.cursor())
                    });
                    if idx + 1 < lines.len() {
                        let next = &lines[idx + 1];
                        let line_start = next.start;
                        let line_end = next.end;
                        Some((target_col, Some((line_start, line_end))))
                    } else {
                        Some((target_col, None))
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } {
            match move_to_last {
                Some((line_start, line_end)) => {
                    if self.preferred_col.is_none() {
                        self.preferred_col = Some(target_col);
                    }
                    self.move_to_display_col_on_line(line_start, line_end, target_col);
                    return;
                }
                None => {
                    // Already on last visual line -> move to end
                    self.set_cursor_inner(self.text.len());
                    self.preferred_col = None;
                    return;
                }
            }
        }

        // Fallback to logical line navigation if we don't have wrapping info yet.
        let target_col = match self.preferred_col {
            Some(c) => c,
            None => {
                let c = self.current_display_col();
                self.preferred_col = Some(c);
                c
            }
        };
        if let Some(next_nl) = self.text[self.cursor()..]
            .find('\n')
            .map(|i| i + self.cursor())
        {
            let next_line_start = next_nl + 1;
            let next_line_end = self.text[next_line_start..]
                .find('\n')
                .map(|i| i + next_line_start)
                .unwrap_or(self.text.len());
            self.move_to_display_col_on_line(next_line_start, next_line_end, target_col);
        } else {
            self.set_cursor_inner(self.text.len());
            self.preferred_col = None;
        }
    }

    /// Home / Super+Left when `move_up_at_bol` is false (visual row if wrapped);
    /// Ctrl+A when true (logical line; already-at-BOL chains to previous line).
    pub fn move_cursor_to_beginning_of_line(&mut self, move_up_at_bol: bool) {
        if move_up_at_bol {
            self.apply_edit_command(EditCommand::MoveLogicalLineStart, None);
            return;
        }
        if let Some(bol) = self.beginning_of_current_visual_line() {
            self.set_cursor(bol);
            return;
        }

        let bol = self.beginning_of_current_line();
        self.set_cursor(bol);
    }

    /// End / Super+Right when `move_down_at_eol` is false (visual row if wrapped);
    /// Ctrl+E when true (logical line; already-at-EOL chains to next line).
    pub fn move_cursor_to_end_of_line(&mut self, move_down_at_eol: bool) {
        if move_down_at_eol {
            self.apply_edit_command(EditCommand::MoveLogicalLineEnd, None);
            return;
        }
        if let Some(eol) = self.end_of_current_visual_line() {
            self.set_cursor(eol);
            return;
        }

        let eol = self.end_of_current_line();
        self.set_cursor(eol);
    }

    fn beginning_of_current_visual_line(&self) -> Option<usize> {
        let cache = self.wrap_cache.borrow();
        let cache = cache.as_ref()?;
        let idx = Self::wrapped_line_index_by_start(&cache.lines, self.cursor())?;
        Some(cache.lines[idx].start)
    }

    /// Soft-continued visual rows land on the last char (exclusive end is the
    /// next row's start). Final segment of a logical line uses exclusive end.
    fn end_of_current_visual_line(&self) -> Option<usize> {
        let cache = self.wrap_cache.borrow();
        let cache = cache.as_ref()?;
        let idx = Self::wrapped_line_index_by_start(&cache.lines, self.cursor())?;
        let line = &cache.lines[idx];
        let end = line.end.min(self.text.len());
        let soft_continued = cache
            .lines
            .get(idx + 1)
            .is_some_and(|next| next.start == end);
        if soft_continued && end > line.start {
            Some(self.clamp_to_line(end, line.start, end))
        } else {
            Some(end)
        }
    }

    // ===== Text elements support =====

    /// Insert an atomic text element at the current cursor position.
    ///
    /// The `text` is inserted into the buffer and registered as an element.
    /// The `kind` tag is opaque to the textarea (host-defined).
    /// The `display` optionally overrides how the element is rendered.
    ///
    /// Returns the assigned [`ElementId`] so the host can store associated metadata.
    pub fn insert_element(
        &mut self,
        text: &str,
        kind: ElementKind,
        display: Option<Line<'static>>,
    ) -> ElementId {
        let plan = self.plan_edit_replacement(self.cursor()..self.cursor(), text);
        self.apply_element_transaction(plan, kind, display)
    }

    /// Replace a range of buffer text with an atomic element.
    ///
    /// This is the "confirm autocomplete" operation: the trigger text (e.g. `@foo`)
    /// is deleted and replaced with element text (e.g. `@src/foo.rs`) in a single
    /// atomic operation. The cursor is placed at the end of the new element.
    ///
    /// Returns the assigned [`ElementId`].
    pub fn replace_range_with_element(
        &mut self,
        range: Range<usize>,
        text: &str,
        kind: ElementKind,
        display: Option<Line<'static>>,
    ) -> ElementId {
        let plan = self.plan_edit_replacement(range, text);
        self.apply_element_transaction(plan, kind, display)
    }

    fn apply_element_transaction(
        &mut self,
        plan: EditPlan,
        kind: ElementKind,
        display: Option<Line<'static>>,
    ) -> ElementId {
        let start = plan.replaced_byte_range().start;
        let inserted_len = plan.replacement().len();
        self.assert_valid_edit_plan(&plan);
        self.pre_mutate(MutationKind::Element);
        self.apply_validated_edit_plan(plan, Some(MutationKind::Element));
        let end = start + inserted_len;
        let id = self.add_element(start..end, kind, display);
        self.set_cursor(end);
        self.post_mutate();
        id
    }

    fn add_element(
        &mut self,
        range: Range<usize>,
        kind: ElementKind,
        display: Option<Line<'static>>,
    ) -> ElementId {
        let id = ElementId(self.next_element_id);
        self.next_element_id += 1;
        let elem = TextElement {
            id,
            range,
            kind,
            display,
        };
        self.elements.push(elem);
        self.elements.sort_by_key(|e| e.range.start);
        self.wrap_cache.replace(None);
        id
    }

    /// Returns the element at the current cursor position, if any.
    ///
    /// If the cursor is at an element's start boundary, that element is returned.
    /// If the cursor is strictly inside an element (shouldn't happen in normal
    /// operation), the containing element is returned.
    pub fn element_at_cursor(&self) -> Option<&TextElement> {
        self.elements
            .iter()
            .find(|e| self.cursor() >= e.range.start && self.cursor() < e.range.end)
    }

    /// Returns the underlying buffer text for the element with the given id.
    pub fn element_text(&self, id: ElementId) -> Option<&str> {
        self.elements
            .iter()
            .find(|e| e.id == id)
            .map(|e| &self.text[e.range.clone()])
    }

    /// Update the display for an existing element. Invalidates the wrap cache.
    pub fn set_element_display(&mut self, id: ElementId, display: Option<Line<'static>>) {
        if let Some(e) = self.elements.iter_mut().find(|e| e.id == id) {
            e.display = display;
            self.wrap_cache.replace(None);
        }
    }

    /// Returns a slice of all elements, sorted by buffer position.
    pub fn elements(&self) -> &[TextElement] {
        &self.elements
    }

    /// Re-register elements after a [`set_text`] call that placed their
    /// buffer text back verbatim. Each `(range, kind, display)` tuple
    /// describes one element whose text already occupies `range` in the
    /// buffer. No text is inserted — this only recreates the element
    /// metadata so the textarea renders chips instead of raw text.
    pub fn restore_elements(
        &mut self,
        elems: impl IntoIterator<Item = (Range<usize>, ElementKind, Option<Line<'static>>)>,
    ) {
        for (range, kind, display) in elems {
            self.add_element(range, kind, display);
        }
        self.wrap_cache.replace(None);
    }

    /// Inline an element: remove it from the element list so its buffer text
    /// becomes plain editable characters. The text content is unchanged.
    ///
    /// The cursor is placed at the end of the inlined region.
    /// This operation is a single undoable step.
    ///
    /// Returns `true` if the element was found and inlined, `false` otherwise.
    pub fn inline_element(&mut self, id: ElementId) -> bool {
        let Some(idx) = self.elements.iter().position(|e| e.id == id) else {
            return false;
        };
        let end = self.elements[idx].range.end;

        // Snapshot for undo before removing the element.
        self.pre_mutate(MutationKind::Element);

        self.elements.remove(idx);
        self.set_cursor_inner(end);
        self.preferred_col = None;
        self.wrap_cache.replace(None);
        self.undo.last_kind = None; // always discrete

        true
    }

    /// Get the contiguous non-whitespace "word" that the cursor is inside or at the start of.
    ///
    /// Returns `(byte_range, text)` where `byte_range` is the range in the buffer.
    /// Returns `None` if the cursor is on whitespace or the buffer is empty.
    ///
    /// This is useful for trigger-character detection (e.g. finding `@foo` under the cursor
    /// for autocomplete). The host can then check `text.starts_with('@')` etc.
    pub fn word_at_cursor(&self) -> Option<(Range<usize>, &str)> {
        if self.text.is_empty() {
            return None;
        }
        let pos = self.cursor().min(self.text.len());

        // Find word start: scan backward from cursor to find whitespace boundary
        let start = self.text[..pos]
            .rfind(|c: char| c.is_whitespace())
            .map(|i| {
                i + self.text[i..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(1)
            })
            .unwrap_or(0);

        // Find word end: scan forward from cursor to find whitespace boundary
        let end = self.text[pos..]
            .find(|c: char| c.is_whitespace())
            .map(|i| i + pos)
            .unwrap_or(self.text.len());

        // Also extend backward from start in case cursor is at word boundary
        // Actually, we also need to handle cursor being between words.
        // If cursor is at whitespace, return None.
        if start >= end {
            return None;
        }

        // If cursor is beyond the word end (cursor at whitespace after word), return None
        // Unless cursor is exactly at start position of the word
        let word = &self.text[start..end];
        if word.chars().all(|c| c.is_whitespace()) {
            return None;
        }

        Some((start..end, word))
    }

    fn find_element_containing(&self, pos: usize) -> Option<usize> {
        self.elements
            .iter()
            .position(|e| pos > e.range.start && pos < e.range.end)
    }

    fn clamp_pos_to_nearest_boundary(&self, mut pos: usize) -> usize {
        if pos > self.text.len() {
            pos = self.text.len();
        }
        if let Some(idx) = self.find_element_containing(pos) {
            let e = &self.elements[idx];
            let dist_start = pos.saturating_sub(e.range.start);
            let dist_end = e.range.end.saturating_sub(pos);
            if dist_start <= dist_end {
                e.range.start
            } else {
                e.range.end
            }
        } else {
            pos
        }
    }

    fn expand_range_to_element_boundaries(&self, mut range: Range<usize>) -> Range<usize> {
        // Expand to include any intersecting elements fully
        loop {
            let mut changed = false;
            for e in &self.elements {
                if e.range.start < range.end && e.range.end > range.start {
                    let new_start = range.start.min(e.range.start);
                    let new_end = range.end.max(e.range.end);
                    if new_start != range.start || new_end != range.end {
                        range.start = new_start;
                        range.end = new_end;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        range
    }

    fn shift_elements(&mut self, at: usize, removed: usize, inserted: usize) {
        // Generic shift: for pure insert, removed = 0; for delete, inserted = 0.
        let end = at + removed;
        let diff = inserted as isize - removed as isize;
        // Remove elements fully deleted by the operation and shift the rest
        self.elements
            .retain(|e| !(e.range.start >= at && e.range.end <= end));
        for e in &mut self.elements {
            if e.range.end <= at {
                // before edit
            } else if e.range.start >= end {
                // after edit
                e.range.start = ((e.range.start as isize) + diff) as usize;
                e.range.end = ((e.range.end as isize) + diff) as usize;
            } else {
                // Overlap with element but not fully contained (shouldn't happen when using
                // element-aware replace, but degrade gracefully by snapping element to new bounds)
                let new_start = at.min(e.range.start);
                let new_end = at + inserted.max(e.range.end.saturating_sub(end));
                e.range.start = new_start;
                e.range.end = new_end;
            }
        }
    }

    fn update_elements_after_replace(&mut self, start: usize, end: usize, inserted_len: usize) {
        self.shift_elements(start, end.saturating_sub(start), inserted_len);
    }

    /// Move to the beginning of the previous navigable chunk.
    ///
    /// Word characters are alphanumeric plus `_`. Punctuation runs (such as
    /// `-`) are their own chunk, so moving left across `aa-bb` stops at the
    /// right side of `-`, then the left side of `-`, then the start of `aa`.
    /// Whitespace is skipped over. Elements remain atomic units.
    pub fn beginning_of_previous_word(&self) -> usize {
        let ranges = self.element_ranges();
        self.text
            .plan_command(EditCommand::MoveWordLeft(WordStyle::Small), &ranges)
            .cursor_byte()
    }

    /// Start of the previous whitespace-delimited WORD; elements count as
    /// non-whitespace.
    pub fn beginning_of_previous_unix_word(&self) -> usize {
        let ranges = self.element_ranges();
        self.text
            .plan_command(
                EditCommand::MoveWordLeft(WordStyle::WhitespaceDelimited),
                &ranges,
            )
            .cursor_byte()
    }

    /// Move to the end of the next navigable chunk.
    ///
    /// Word characters are alphanumeric plus `_`. Punctuation runs (such as
    /// `-`) are their own chunk, so moving right across `aa-bb` stops at the
    /// left side of `-`, then the right side of `-`, then the end of `bb`.
    /// Whitespace is skipped over. Elements remain atomic units.
    pub fn end_of_next_word(&self) -> usize {
        let ranges = self.element_ranges();
        self.text
            .plan_command(EditCommand::MoveWordRight(WordStyle::Small), &ranges)
            .cursor_byte()
    }

    fn adjust_pos_out_of_elements(&self, pos: usize, prefer_start: bool) -> usize {
        if let Some(idx) = self.find_element_containing(pos) {
            let e = &self.elements[idx];
            if prefer_start {
                e.range.start
            } else {
                e.range.end
            }
        } else {
            pos
        }
    }

    #[expect(clippy::unwrap_used)]
    fn wrapped_lines(&self, width: u16) -> Ref<'_, Vec<Range<usize>>> {
        // A zero-width terminal must not reach textwrap — it can produce
        // borrowed empty slices that don't point into the input buffer,
        // causing out-of-bounds panics in wrap_ranges pointer arithmetic.
        let width = width.max(1);
        // Ensure cache is ready (potentially mutably borrow, then drop)
        {
            let mut cache = self.wrap_cache.borrow_mut();
            let needs_recalc = match cache.as_ref() {
                Some(c) => c.width != width,
                None => true,
            };
            if needs_recalc {
                let lines = if self.elements.iter().any(|e| e.display.is_some()) {
                    self.element_aware_wrap_ranges(width as usize)
                } else {
                    crate::wrapping::wrap_ranges(
                        &self.text,
                        Options::new(width as usize)
                            .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit),
                    )
                };
                *cache = Some(WrapCache { width, lines });
            }
        }

        let cache = self.wrap_cache.borrow();
        Ref::map(cache, |c| &c.as_ref().unwrap().lines)
    }

    /// Element-display-aware greedy wrapping.
    ///
    /// Produces wrap ranges where each range is `start..end` with `end` being
    /// the exclusive byte position of the content (including any trailing
    /// spaces that belong to this visual line).
    ///
    /// Elements are treated as atomic units for wrapping: if an element's
    /// display width doesn't fit on the current line, the element is moved
    /// to a new line (like word-wrap). If it doesn't fit on *any* line
    /// (wider than terminal), it gets its own line and rendering truncates it.
    fn element_aware_wrap_ranges(&self, width: usize) -> Vec<Range<usize>> {
        let width = width.max(1);
        let mut result = Vec::new();

        // Process each logical line (split by \n), but skip \n inside elements.
        // Newlines inside elements are internal to the element and must not create
        // visual line breaks — the element's display is a single-line chip.
        let mut seg_start = 0;
        loop {
            let seg_end = self.next_logical_newline(seg_start);

            self.greedy_wrap_segment(seg_start, seg_end, width, &mut result);

            if seg_end >= self.text.len() {
                break;
            }
            seg_start = seg_end + 1; // skip \n
        }

        if result.is_empty() {
            result.push(0..0);
        }

        result
    }

    /// Find the next `\n` at or after `from` that is NOT inside an element.
    ///
    /// Returns `self.text.len()` if no such newline exists.
    fn next_logical_newline(&self, from: usize) -> usize {
        let mut pos = from;
        while pos < self.text.len() {
            // If pos is inside an element, skip past the entire element.
            if let Some(elem) = self
                .elements
                .iter()
                .find(|e| pos >= e.range.start && pos < e.range.end)
            {
                pos = elem.range.end;
                continue;
            }
            if self.text.as_bytes()[pos] == b'\n' {
                return pos;
            }
            pos += 1;
        }
        self.text.len()
    }

    /// Greedy-wrap a single logical line (no \n inside `start..end`).
    fn greedy_wrap_segment(
        &self,
        start: usize,
        end: usize,
        width: usize,
        result: &mut Vec<Range<usize>>,
    ) {
        if start >= end {
            // Empty logical line
            result.push(start..end);
            return;
        }

        let mut line_start = start;
        let mut pos = start;
        let mut display_w: usize = 0;
        // Position right after the last break opportunity (start of the next word/element).
        let mut last_break_pos: Option<usize> = None;

        while pos < end {
            // Check if pos is at the start of an element
            if let Some(elem) = self
                .elements
                .iter()
                .find(|e| pos == e.range.start && e.range.start < e.range.end)
            {
                let elem_end = elem.range.end.min(end);
                let elem_dw: usize = if let Some(display) = &elem.display {
                    display
                        .spans
                        .iter()
                        .map(|s| s.content.as_ref().width())
                        .sum()
                } else {
                    self.plain_display_width(&self.text[elem.range.start..elem_end])
                };

                if display_w > 0 && display_w + elem_dw > width {
                    // Element doesn't fit on current line — break before it.
                    let break_at = last_break_pos.unwrap_or(pos);
                    result.push(line_start..break_at);
                    line_start = break_at;
                    // Skip leading spaces/tabs on the new line
                    while line_start < end
                        && line_start < pos
                        && matches!(self.text.as_bytes().get(line_start), Some(b' ' | b'\t'))
                    {
                        line_start += 1;
                    }
                    pos = line_start;
                    display_w = 0;
                    last_break_pos = None;
                    continue;
                }

                display_w += elem_dw;
                pos = elem_end;
                // After element is a break opportunity
                last_break_pos = Some(pos);
                continue;
            }

            // Plain text grapheme cluster
            let slice = &self.text[pos..end];
            let Some(grapheme) = slice.graphemes(true).next() else {
                break;
            };
            let grapheme_width = self.grapheme_display_width(grapheme);

            if display_w + grapheme_width > width && display_w > 0 {
                // Need to wrap
                let break_at = last_break_pos.unwrap_or(pos);
                result.push(line_start..break_at);
                line_start = break_at;
                // Skip leading spaces/tabs on the new line
                while line_start < end
                    && line_start < pos
                    && matches!(self.text.as_bytes().get(line_start), Some(b' ' | b'\t'))
                {
                    line_start += 1;
                }
                display_w = self.display_width_of_range(line_start, pos);
                last_break_pos = None;
                if line_start == pos {
                    // No break opportunity found; break at current position (break_words).
                    display_w = grapheme_width;
                    pos += grapheme.len();
                }
                continue;
            }

            if grapheme == " " || grapheme == "\t" {
                // Space/tab is a break opportunity; break point is after it.
                last_break_pos = Some(pos + grapheme.len());
            }

            display_w += grapheme_width;
            pos += grapheme.len();
        }

        // Final visual line of this logical line
        result.push(line_start..end);
    }

    /// Calculate the scroll offset that should be used to satisfy the
    /// invariants given the current area size and wrapped lines.
    ///
    /// - Cursor is always on screen.
    /// - No scrolling if content fits in the area.
    fn effective_scroll(
        &self,
        area_height: u16,
        lines: &[Range<usize>],
        current_scroll: u16,
    ) -> u16 {
        let total_lines = lines.len() as u16;
        if area_height >= total_lines {
            return 0;
        }

        let max_scroll = total_lines.saturating_sub(area_height);

        // If we have an internal scroll override (from mousewheel), use it
        // — but still clamp to valid range.
        if let Some(ovr) = self.scroll_override {
            return ovr.min(max_scroll);
        }

        // Where is the cursor within wrapped lines? Prefer assigning boundary positions
        // (where pos equals the start of a wrapped line) to that later line.
        let cursor_line_idx =
            Self::wrapped_line_index_by_start(lines, self.cursor()).unwrap_or(0) as u16;

        let mut scroll = current_scroll.min(max_scroll);

        // Ensure cursor is visible within [scroll, scroll + area_height)
        if cursor_line_idx < scroll {
            scroll = cursor_line_idx;
        } else if cursor_line_idx >= scroll + area_height {
            scroll = cursor_line_idx + 1 - area_height;
        }
        scroll
    }

    /// Compute the effective content width for text wrapping, accounting for
    /// the scrollbar column.  Uses a 2-shot approach:
    ///
    /// 1. Wrap at full `area_width` to get line count.
    /// 2. If scrollbar needed (lines > height) and `show_scrollbar`, reduce
    ///    width by 1 for the scrollbar track.
    ///
    /// Returns `(content_width, needs_scrollbar)`.
    fn content_width(&self, area_width: u16, area_height: u16) -> (u16, bool) {
        if !self.show_scrollbar || area_width <= 1 {
            return (area_width, false);
        }
        // First shot — wrap at full width to check if content overflows.
        let lines = self.wrapped_lines(area_width);
        let needs = lines.len() as u16 > area_height;
        if needs {
            // 1 for scrollbar track + padding gap
            let reserved = 1 + self.scrollbar_padding;
            (area_width.saturating_sub(reserved), true)
        } else {
            (area_width, false)
        }
    }

    /// Convenience: content width for wrapping (area width minus scrollbar if needed).
    fn text_width(&self, area: Rect) -> u16 {
        self.content_width(area.width, area.height).0
    }
}

impl WidgetRef for &TextArea {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let (cw, needs_sb) = self.content_width(area.width, area.height);
        let content_area = Rect { width: cw, ..area };
        let lines = self.wrapped_lines(cw);
        self.render_lines(content_area, buf, &lines, 0..lines.len());
        if needs_sb {
            self.render_scrollbar(area, buf, lines.len() as u16, area.height, 0);
        }
    }
}

impl StatefulWidgetRef for &TextArea {
    type State = TextAreaState;

    fn render_ref(&self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let (cw, needs_sb) = self.content_width(area.width, area.height);
        let content_area = Rect { width: cw, ..area };
        let lines = self.wrapped_lines(cw);
        let scroll = self.effective_scroll(area.height, &lines, state.scroll);
        state.scroll = scroll;

        let start = scroll as usize;
        let end = (scroll + area.height).min(lines.len() as u16) as usize;
        self.render_lines(content_area, buf, &lines, start..end);
        if needs_sb {
            self.render_scrollbar(area, buf, lines.len() as u16, area.height, scroll);
        }
    }
}

impl TextArea {
    /// Render a scrollbar in the rightmost column of `area`.
    ///
    /// Uses `tui_scrollbar::ScrollBar` rendered into a scratch ratatui-core
    /// buffer, then copies cells into the main buffer with muted styling.
    fn render_scrollbar(
        &self,
        area: Rect,
        buf: &mut Buffer,
        total_lines: u16,
        viewport_lines: u16,
        offset: u16,
    ) {
        if total_lines <= viewport_lines || area.width == 0 || area.height == 0 {
            return;
        }

        let sb_area = Rect {
            x: area.right().saturating_sub(1),
            y: area.y,
            width: 1,
            height: area.height,
        };

        let lengths = ScrollLengths {
            content_len: total_lines as usize,
            viewport_len: viewport_lines as usize,
        };
        let scrollbar = ScrollBar::vertical(lengths).offset(offset as usize);

        // Render into ratatui-core scratch buffer then copy with styling.
        let core_area = CoreRect {
            x: sb_area.x,
            y: sb_area.y,
            width: sb_area.width,
            height: sb_area.height,
        };
        let mut scratch = CoreBuffer::empty(core_area);
        (&scrollbar).render(core_area, &mut scratch);

        let track_style = self.scrollbar_track_style;
        let thumb_style = self.scrollbar_thumb_style;

        for row in 0..sb_area.height {
            let x = sb_area.x;
            let y = sb_area.y + row;
            let src = &scratch[(x, y)];
            let dst = &mut buf[(x, y)];
            let symbol = src.symbol();
            dst.set_symbol(symbol);
            if symbol == " " {
                dst.set_style(track_style);
            } else {
                dst.set_style(thumb_style);
            }
        }
    }

    fn render_lines(
        &self,
        area: Rect,
        buf: &mut Buffer,
        lines: &[Range<usize>],
        range: std::ops::Range<usize>,
    ) {
        let area_right = area.x + area.width; // exclusive right boundary
        let sel_range = self.selection_range();

        for (row, idx) in range.enumerate() {
            let r = &lines[idx];
            let y = area.y + row as u16;
            let line_range = r.start..r.end;

            // Render the line segment-by-segment (plain text → element → plain text → …)
            // using display-aware x positioning. This ensures that when an element's
            // display text is wider (or narrower) than its buffer text, all subsequent
            // content is positioned correctly.
            let mut display_x: u16 = 0; // current display column
            let mut buf_pos = line_range.start; // current position in the buffer

            // Collect elements that overlap this visual line, in order.
            let overlapping: Vec<&TextElement> = self
                .elements
                .iter()
                .filter(|e| {
                    let os = e.range.start.max(line_range.start);
                    let oe = e.range.end.min(line_range.end);
                    os < oe
                })
                .collect();

            for elem in &overlapping {
                let overlap_start = elem.range.start.max(line_range.start);
                let overlap_end = elem.range.end.min(line_range.end);

                // 1. Render plain text before this element (buf_pos..overlap_start)
                if buf_pos < overlap_start && display_x < area.width {
                    let plain = &self.text[buf_pos..overlap_start];
                    let avail = (area.width - display_x) as usize;
                    let (paint, paint_w) = paint_plain_for_display(plain, avail, self.tab_width);
                    buf.set_string(area.x + display_x, y, paint.as_ref(), Style::default());
                    display_x += paint_w as u16;
                }

                // 2. Render the element
                if display_x >= area.width {
                    buf_pos = overlap_end;
                    continue;
                }

                let avail = (area.width - display_x) as usize;

                if let Some(display) = &elem.display {
                    if overlap_start == elem.range.start {
                        // First visual line of the element — render display text.
                        let display = truncate_line_display(display, avail);
                        for span in &display.spans {
                            let content = span.content.as_ref();
                            let w = content.width() as u16;
                            if display_x >= area.width {
                                break;
                            }
                            buf.set_string(area.x + display_x, y, content, span.style);
                            display_x += w;
                        }
                    }
                    // If element spans multiple visual lines but has a display,
                    // subsequent lines show nothing for this element region (blank).
                    // display_x doesn't advance (already blank in the buffer).
                } else {
                    // No custom display: render buffer text with default element style.
                    let styled = &self.text[overlap_start..overlap_end];
                    let style = Style::default().fg(Color::Cyan);
                    let (paint, paint_w) = paint_plain_for_display(styled, avail, self.tab_width);
                    buf.set_string(area.x + display_x, y, paint.as_ref(), style);
                    display_x += paint_w as u16;
                }

                buf_pos = overlap_end;
            }

            // 3. Render any remaining plain text after the last element
            if buf_pos < line_range.end && display_x < area.width {
                let plain = &self.text[buf_pos..line_range.end];
                let avail = (area.width - display_x) as usize;
                let (paint, paint_w) = paint_plain_for_display(plain, avail, self.tab_width);
                buf.set_string(area.x + display_x, y, paint.as_ref(), Style::default());
                // Keep display_x consistent with earlier segments (selection uses
                // display_width_of_range on a second pass).
                let _painted_end = display_x.saturating_add(paint_w as u16);
                let _ = _painted_end;
            }

            // 4. Apply selection highlight (second pass over cells)
            if let Some(sel_range) = &sel_range {
                // Intersect the selection with this visual line's buffer range.
                let line_sel_start = sel_range.start.max(line_range.start);
                let line_sel_end = sel_range.end.min(line_range.end);
                if line_sel_start < line_sel_end {
                    // Compute display column range for the selected portion.
                    let col_start =
                        self.display_width_of_range(line_range.start, line_sel_start) as u16;
                    let col_end =
                        self.display_width_of_range(line_range.start, line_sel_end) as u16;
                    let col_start = col_start.min(area.width);
                    let col_end = col_end.min(area.width);
                    for cx in col_start..col_end {
                        let cell = &mut buf[(area.x + cx, y)];
                        cell.set_style(self.selection_style);
                    }
                }
            }

            let _ = area_right; // suppress unused warning (used for documentation)
        }
    }
}

/// Expand `\t` to a fixed number of spaces (`tab_width`), matching scrollback.
fn expand_tabs_with_width(text: &str, tab_width: u8) -> std::borrow::Cow<'_, str> {
    if tab_width == 0 || !text.contains('\t') {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.replace('\t', &" ".repeat(tab_width as usize)))
}

fn grapheme_display_width_with_tab(grapheme: &str, tab_width: u8) -> usize {
    if grapheme == "\t" {
        if tab_width == 0 {
            0
        } else {
            tab_width as usize
        }
    } else {
        grapheme.width()
    }
}

fn plain_display_width_with_tab(text: &str, tab_width: u8) -> usize {
    if tab_width == 0 || !text.contains('\t') {
        return text.width();
    }
    text.graphemes(true)
        .map(|g| grapheme_display_width_with_tab(g, tab_width))
        .sum()
}

/// Clip a string to fit within `max_width` display columns (tabs = 0 width).
/// Returns a substring that is at most `max_width` columns wide.
fn clip_str_to_display_width(s: &str, max_width: usize) -> &str {
    clip_str_to_display_width_with_tab(s, max_width, 0)
}

/// Clip considering tabs as `tab_width` columns (byte index into original `s`).
fn clip_str_to_display_width_with_tab(s: &str, max_width: usize, tab_width: u8) -> &str {
    let mut width = 0;
    for (i, grapheme) in s.grapheme_indices(true) {
        let grapheme_width = grapheme_display_width_with_tab(grapheme, tab_width);
        if width + grapheme_width > max_width {
            return &s[..i];
        }
        width += grapheme_width;
    }
    s
}

/// Clip and expand tabs so paint width matches cursor/display-width math.
/// Returns (paint string, display columns used). Borrows when no expansion needed.
fn paint_plain_for_display(
    s: &str,
    max_width: usize,
    tab_width: u8,
) -> (std::borrow::Cow<'_, str>, usize) {
    let clipped = clip_str_to_display_width_with_tab(s, max_width, tab_width);
    let paint = expand_tabs_with_width(clipped, tab_width);
    let w = plain_display_width_with_tab(clipped, tab_width);
    (paint, w)
}

/// Truncate a display `Line` to fit within `max_width` columns.
///
/// If the line fits, it is returned as-is (cloned). If it overflows:
/// - Reserve 1 column for `…`.
/// - **Bracket-preservation heuristic:** if the display text ends with a closing
///   bracket (`]`, `)`, `}`, `>`), preserve it so e.g. `[Pasted ~10 lines]`
///   becomes `[Pasted ~1…]` rather than `[Pasted ~10…`.
/// - Otherwise, truncate and append `…`.
fn truncate_line_display(line: &Line<'static>, max_width: usize) -> Line<'static> {
    use ratatui::text::Span;

    let total_width: usize = line.spans.iter().map(|s| s.content.as_ref().width()).sum();
    if total_width <= max_width {
        return line.clone();
    }
    if max_width == 0 {
        return Line::default();
    }

    // Determine if we should preserve a closing bracket.
    let last_char = line
        .spans
        .iter()
        .rev()
        .find_map(|s| s.content.as_ref().chars().last());
    let (preserve_bracket, bracket_char, bracket_style) = match last_char {
        Some(ch @ (']' | ')' | '}' | '>')) => {
            // Find the style of the last span containing this char.
            let style = line.spans.last().map(|s| s.style).unwrap_or_default();
            (true, Some(ch), style)
        }
        _ => (false, None, Style::default()),
    };

    // Budget: max_width minus 1 for '…', minus 1 for bracket if preserving.
    // If max_width is too small for both ellipsis and bracket, skip bracket.
    let preserve_bracket = preserve_bracket && max_width >= 3;
    let content_budget = if preserve_bracket {
        max_width.saturating_sub(2) // 1 for …, 1 for bracket
    } else {
        max_width.saturating_sub(1) // 1 for …
    };

    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for span in &line.spans {
        let content = span.content.as_ref();
        let sw = content.width();
        if used + sw <= content_budget {
            new_spans.push(span.clone());
            used += sw;
        } else {
            // Partially include this span without splitting a grapheme cluster.
            let remaining = content_budget - used;
            if remaining > 0 {
                let partial = clip_str_to_display_width(content, remaining);
                if !partial.is_empty() {
                    new_spans.push(Span::styled(partial.to_string(), span.style));
                }
            }
            break;
        }
    }

    // Append ellipsis (inherits style of last content span, or default).
    let ellipsis_style = new_spans.last().map(|s| s.style).unwrap_or_default();
    new_spans.push(Span::styled("…", ellipsis_style));

    // Append preserved bracket if applicable.
    if preserve_bracket && let Some(ch) = bracket_char {
        new_spans.push(Span::styled(ch.to_string(), bracket_style));
    }

    Line::from(new_spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    // crossterm types are intentionally not imported here to avoid unused warnings
    use rand::prelude::*;

    fn rand_grapheme(rng: &mut rand::rngs::StdRng) -> String {
        let r: u8 = rng.random_range(0..100);
        match r {
            0..=4 => "\n".to_string(),
            5..=12 => " ".to_string(),
            13..=35 => (rng.random_range(b'a'..=b'z') as char).to_string(),
            36..=45 => (rng.random_range(b'A'..=b'Z') as char).to_string(),
            46..=52 => (rng.random_range(b'0'..=b'9') as char).to_string(),
            53..=65 => {
                // Some emoji (wide graphemes)
                let choices = ["👍", "😊", "🐍", "🚀", "🧪", "🌟"];
                choices[rng.random_range(0..choices.len())].to_string()
            }
            66..=75 => {
                // CJK wide characters
                let choices = ["漢", "字", "測", "試", "你", "好", "界", "编", "码"];
                choices[rng.random_range(0..choices.len())].to_string()
            }
            76..=85 => {
                // Combining mark sequences
                let base = ["e", "a", "o", "n", "u"][rng.random_range(0..5)];
                let marks = ["\u{0301}", "\u{0308}", "\u{0302}", "\u{0303}"];
                format!("{base}{}", marks[rng.random_range(0..marks.len())])
            }
            86..=92 => {
                // Some non-latin single codepoints (Greek, Cyrillic, Hebrew)
                let choices = ["Ω", "β", "Ж", "ю", "ש", "م", "ह"];
                choices[rng.random_range(0..choices.len())].to_string()
            }
            _ => {
                // ZWJ sequences (single graphemes but multi-codepoint)
                let choices = [
                    "👩\u{200D}💻", // woman technologist
                    "👨\u{200D}💻", // man technologist
                    "🏳️\u{200D}🌈", // rainbow flag
                ];
                choices[rng.random_range(0..choices.len())].to_string()
            }
        }
    }

    fn ta_with(text: &str) -> TextArea {
        let mut t = TextArea::new();
        t.insert_str(text);
        t
    }

    #[test]
    fn canonical_adapter_matches_standalone_edit_buffer() {
        let cases = [
            (
                "hello-world",
                "hello-world".len(),
                EditCommand::MoveWordLeft(WordStyle::Small),
            ),
            (
                "hello-world",
                0,
                EditCommand::MoveWordRight(WordStyle::Small),
            ),
            (
                "foo bar",
                "foo bar".len(),
                EditCommand::DeleteWordBackward(WordStyle::Small),
            ),
            (
                "foo bar",
                0,
                EditCommand::DeleteWordForward(WordStyle::Small),
            ),
            ("one\ntwo", 4, EditCommand::MoveLogicalLineStart),
            ("one\ntwo", 3, EditCommand::MoveLogicalLineEnd),
            ("abc", 2, EditCommand::DeleteGraphemeBackward),
            ("abc", 1, EditCommand::DeleteGraphemeForward),
        ];

        for (text, cursor, command) in cases {
            let mut textarea = TextArea::new();
            textarea.set_text(text);
            textarea.clear_history();
            textarea.set_cursor(cursor);
            let mut buffer = EditBuffer::from_parts(text, cursor);

            textarea.apply_classified_command(command);
            let _ = buffer.apply(command);

            assert_eq!(textarea.text(), buffer.text());
            assert_eq!(textarea.cursor(), buffer.cursor_byte());
        }
    }

    #[test]
    fn canonical_adapter_updates_selection_from_applied_delta() {
        let mut textarea = ta_with("abcdef");
        textarea.set_selection(4, 6);
        textarea.replace_range(0..2, "X");
        assert_eq!(textarea.text(), "Xcdef");
        assert_eq!(textarea.selection_range(), Some(3..5));
    }

    #[test]
    fn canonical_adapter_applies_same_byte_metadata_edits_with_history() {
        let mut textarea = TextArea::new();
        let id = textarea.insert_element("TOKEN", ElementKind(1), None);
        textarea.set_selection(0, 5);
        textarea.clear_history();

        textarea.replace_range(0..5, "TOKEN");
        assert_eq!(textarea.text(), "TOKEN");
        assert!(textarea.elements().is_empty());
        assert!(textarea.selection.is_none());
        assert!(textarea.can_undo());

        assert!(textarea.undo());
        assert_eq!(textarea.text(), "TOKEN");
        assert_eq!(textarea.elements().len(), 1);
        assert_eq!(textarea.elements()[0].id, id);
        assert!(textarea.redo());
        assert_eq!(textarea.text(), "TOKEN");
        assert!(textarea.elements().is_empty());
    }

    #[test]
    fn replace_element_forces_cursor_end_and_restores_metadata() {
        let mut before = ta_with("left TOKEN right");
        before.clear_history();
        before.set_cursor(0);
        let id = before.replace_range_with_element(5..10, "NODE", ElementKind(1), None);
        let end = 5 + "NODE".len();
        assert_eq!(before.cursor(), end);
        assert_eq!(before.elements()[0].id, id);

        assert!(before.undo());
        assert_eq!(before.text(), "left TOKEN right");
        assert!(before.elements().is_empty());
        assert_eq!(before.cursor(), 0);
        assert!(before.redo());
        assert_eq!(before.text(), "left NODE right");
        assert_eq!(before.elements()[0].id, id);
        assert_eq!(before.cursor(), end);

        let mut after = ta_with("left TOKEN right");
        after.clear_history();
        after.set_cursor(after.text().len());
        after.replace_range_with_element(5..10, "NODE", ElementKind(1), None);
        assert_eq!(after.cursor(), end);
    }

    #[test]
    fn empty_set_text_invalidates_redo_and_is_undoable() {
        let mut textarea = TextArea::new();
        textarea.insert_str("x");
        assert!(textarea.undo());
        assert!(textarea.can_redo());

        textarea.set_text("");
        assert!(!textarea.can_redo());
        assert!(textarea.can_undo());
        assert_eq!(textarea.cursor(), 0);
    }

    #[test]
    fn set_text_preserves_cursor_clamped_across_grow_and_shrink() {
        let mut grow = ta_with("abcd");
        grow.set_cursor(2);
        grow.set_text("abcdefgh");
        assert_eq!(grow.cursor(), 2);

        let mut shrink = ta_with("abcdefgh");
        shrink.set_cursor(6);
        shrink.set_text("abc");
        assert_eq!(shrink.cursor(), 3);
    }

    #[test]
    fn set_text_restores_zero_length_element_metadata_through_history() {
        let mut textarea = TextArea::new();
        let id = textarea.insert_element("", ElementKind(7), None);
        textarea.clear_history();

        textarea.set_text("");
        assert!(textarea.elements().is_empty());
        assert!(textarea.undo());
        assert_eq!(textarea.text(), "");
        assert_eq!(textarea.elements().len(), 1);
        assert_eq!(textarea.elements()[0].id, id);
        assert_eq!(textarea.elements()[0].range, 0..0);
        assert!(textarea.redo());
        assert!(textarea.elements().is_empty());
    }

    #[test]
    fn rejected_adapter_plan_has_no_side_effects() {
        let mut textarea = TextArea::new();
        let id = textarea.insert_element("TOKEN", ElementKind(1), None);
        textarea.set_selection(0, 5);
        textarea.kill_buffer = "sentinel".to_owned();
        textarea.preferred_col = Some(3);
        textarea.scroll_override = Some(2);
        let _ = textarea.desired_height(20);
        textarea.clear_history();
        let plan = textarea.plan_edit_replacement(0..5, "X");
        let _ = textarea.text.set_cursor_byte(0);

        let result = textarea.try_apply_edit_plan(plan, Some(MutationKind::Replace));
        assert_eq!(result, Err(ApplyEditPlanError::StalePlan));
        assert_eq!(textarea.text(), "TOKEN");
        assert_eq!(textarea.elements().len(), 1);
        assert_eq!(textarea.elements()[0].id, id);
        assert_eq!(textarea.selection_range(), Some(0..5));
        assert_eq!(textarea.kill_buffer, "sentinel");
        assert_eq!(textarea.preferred_col, Some(3));
        assert_eq!(textarea.scroll_override, Some(2));
        assert!(textarea.wrap_cache.borrow().is_some());
        assert!(!textarea.can_undo());
    }

    #[test]
    fn handled_boundary_navigation_clears_vertical_affinity() {
        let mut textarea = ta_with("ab\nwxyz");
        textarea.set_cursor(0);
        textarea.preferred_col = Some(3);
        textarea.scroll_override = Some(2);

        textarea.move_cursor_left();
        assert_eq!(textarea.cursor(), 0);
        assert_eq!(textarea.preferred_col, None);
        assert_eq!(textarea.scroll_override, None);

        textarea.move_cursor_down();
        assert_eq!(textarea.cursor(), 3);
    }

    #[test]
    fn insert_str_at_inside_element_clamps_to_an_atomic_boundary() {
        let mut textarea = TextArea::new();
        textarea.insert_str("a");
        textarea.insert_element("TOKEN", ElementKind(1), None);
        textarea.insert_str("b");
        textarea.clear_history();

        textarea.insert_str_at(3, "X");
        assert_eq!(textarea.text(), "aXTOKENb");
        assert_eq!(textarea.cursor(), 8);
        assert_eq!(textarea.elements()[0].range, 2..7);
        assert!(textarea.undo());
        assert_eq!(textarea.text(), "aTOKENb");
        assert_eq!(textarea.elements()[0].range, 1..6);
    }

    #[test]
    fn canonical_adapter_keeps_elements_atomic_for_motion_and_deletion() {
        let mut backward = TextArea::new();
        backward.insert_str("a");
        let id = backward.insert_element("TOKEN", ElementKind(1), None);
        backward.insert_str("b");
        let range = backward.elements()[0].range.clone();
        backward.set_cursor(range.end);
        backward.move_cursor_left();
        assert_eq!(backward.cursor(), range.start);
        backward.move_cursor_right();
        assert_eq!(backward.cursor(), range.end);
        backward.delete_backward(1);
        assert_eq!(backward.text(), "ab");
        assert!(backward.elements().iter().all(|element| element.id != id));

        let mut forward = TextArea::new();
        forward.insert_str("a");
        forward.insert_element("TOKEN", ElementKind(1), None);
        forward.insert_str("b");
        let range = forward.elements()[0].range.clone();
        forward.set_cursor(range.start);
        forward.delete_forward(1);
        assert_eq!(forward.text(), "ab");
        assert!(forward.elements().is_empty());
    }

    #[test]
    fn canonical_adapter_ignores_element_newlines_and_restores_kills() {
        let mut textarea = TextArea::new();
        textarea.insert_str("a");
        textarea.insert_element("X\nY", ElementKind(1), None);
        textarea.insert_str("b\nc");
        textarea.clear_history();

        textarea.set_cursor(0);
        textarea.move_cursor_to_end_of_line(true);
        assert_eq!(textarea.cursor(), 5);
        textarea.set_cursor(0);
        textarea.kill_to_end_of_line();
        assert_eq!(textarea.text(), "\nc");
        assert_eq!(textarea.kill_buffer, "aX\nYb");

        assert!(textarea.undo());
        assert_eq!(textarea.text(), "aX\nYb\nc");
        assert_eq!(textarea.elements().len(), 1);
        assert!(textarea.redo());
        assert_eq!(textarea.text(), "\nc");
        assert!(textarea.elements().is_empty());
    }

    #[test]
    fn canonical_adapter_preserves_right_affinity_through_undo_redo() {
        let woman = "👩";
        let tail = "👩🏽\u{200d}💻";
        let original = format!("{woman}{tail}");
        let mut textarea = ta_with(&original);
        textarea.clear_history();
        textarea.set_cursor(woman.len());

        textarea.insert_str("\u{200d}");
        assert_eq!(textarea.text().graphemes(true).count(), 1);
        assert_eq!(textarea.cursor(), textarea.text().len());

        assert!(textarea.undo());
        assert_eq!(textarea.text(), original);
        assert_eq!(textarea.cursor(), woman.len());
        assert!(textarea.redo());
        assert_eq!(textarea.text().graphemes(true).count(), 1);
        assert_eq!(textarea.cursor(), textarea.text().len());
    }

    #[test]
    fn is_undo_input_accepts_ctrl_and_cmd_z() {
        assert!(is_undo_input(&KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL
        )));
        assert!(is_undo_input(&KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::SUPER
        )));
    }

    #[test]
    fn is_undo_input_rejects_redo_and_plain_z() {
        // Uppercase 'Z' (redo) stays excluded so the guard is disjoint from
        // the redo arm regardless of match order.
        assert!(!is_undo_input(&KeyEvent::new(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        // A bare 'z' (no chord modifier) is plain typing, not undo.
        assert!(!is_undo_input(&KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::NONE
        )));
        assert!(!is_undo_input(&KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn insert_and_replace_update_cursor_and_text() {
        // insert helpers
        let mut t = ta_with("hello");
        t.set_cursor(5);
        t.insert_str("!");
        assert_eq!(t.text(), "hello!");
        assert_eq!(t.cursor(), 6);

        t.insert_str_at(0, "X");
        assert_eq!(t.text(), "Xhello!");
        assert_eq!(t.cursor(), 7);

        // Insert after the cursor should not move it
        t.set_cursor(1);
        let end = t.text().len();
        t.insert_str_at(end, "Y");
        assert_eq!(t.text(), "Xhello!Y");
        assert_eq!(t.cursor(), 1);

        // replace_range cases
        // 1) cursor before range
        let mut t = ta_with("abcd");
        t.set_cursor(1);
        t.replace_range(2..3, "Z");
        assert_eq!(t.text(), "abZd");
        assert_eq!(t.cursor(), 1);

        // 2) cursor inside range
        let mut t = ta_with("abcd");
        t.set_cursor(2);
        t.replace_range(1..3, "Q");
        assert_eq!(t.text(), "aQd");
        assert_eq!(t.cursor(), 2);

        // 3) cursor after range with shifted by diff
        let mut t = ta_with("abcd");
        t.set_cursor(4);
        t.replace_range(0..1, "AA");
        assert_eq!(t.text(), "AAbcd");
        assert_eq!(t.cursor(), 5);
    }

    #[test]
    fn delete_backward_and_forward_edges() {
        let mut t = ta_with("abc");
        t.set_cursor(1);
        t.delete_backward(1);
        assert_eq!(t.text(), "bc");
        assert_eq!(t.cursor(), 0);

        // deleting backward at start is a no-op
        t.set_cursor(0);
        t.delete_backward(1);
        assert_eq!(t.text(), "bc");
        assert_eq!(t.cursor(), 0);

        // forward delete removes next grapheme
        t.set_cursor(1);
        t.delete_forward(1);
        assert_eq!(t.text(), "b");
        assert_eq!(t.cursor(), 1);

        // forward delete at end is a no-op
        t.set_cursor(t.text().len());
        t.delete_forward(1);
        assert_eq!(t.text(), "b");
    }

    #[test]
    fn delete_backward_word_and_kill_line_variants() {
        // delete backward word at end removes the whole previous word
        let mut t = ta_with("hello   world  ");
        t.set_cursor(t.text().len());
        t.delete_backward_word();
        assert_eq!(t.text(), "hello   ");
        assert_eq!(t.cursor(), 8);

        // From inside a word, delete from word start to cursor
        let mut t = ta_with("foo bar");
        t.set_cursor(6); // inside "bar" (after 'a')
        t.delete_backward_word();
        assert_eq!(t.text(), "foo r");
        assert_eq!(t.cursor(), 4);

        // From end, delete the last word only
        let mut t = ta_with("foo bar");
        t.set_cursor(t.text().len());
        t.delete_backward_word();
        assert_eq!(t.text(), "foo ");
        assert_eq!(t.cursor(), 4);

        let mut t = ta_with("hello-world");
        t.set_cursor(t.text().len());
        t.delete_backward_word();
        assert_eq!(t.text(), "hello-");
        assert_eq!(t.cursor(), "hello-".len());

        // kill_to_end_of_line when not at EOL
        let mut t = ta_with("abc\ndef");
        t.set_cursor(1); // on first line, middle
        t.kill_to_end_of_line();
        assert_eq!(t.text(), "a\ndef");
        assert_eq!(t.cursor(), 1);

        // kill_to_end_of_line when at EOL deletes newline
        let mut t = ta_with("abc\ndef");
        t.set_cursor(3); // EOL of first line
        t.kill_to_end_of_line();
        assert_eq!(t.text(), "abcdef");
        assert_eq!(t.cursor(), 3);

        // kill_to_beginning_of_line from middle of line
        let mut t = ta_with("abc\ndef");
        t.set_cursor(5); // on second line, after 'e'
        t.kill_to_beginning_of_line();
        assert_eq!(t.text(), "abc\nef");

        // kill_to_beginning_of_line at beginning of non-first line removes the previous newline
        let mut t = ta_with("abc\ndef");
        t.set_cursor(4); // beginning of second line
        t.kill_to_beginning_of_line();
        assert_eq!(t.text(), "abcdef");
        assert_eq!(t.cursor(), 3);

        // kill_current_line from middle of single line
        let mut t = ta_with("hello world");
        t.set_cursor(5);
        t.kill_current_line();
        assert_eq!(t.text(), "");
        assert_eq!(t.cursor(), 0);

        // kill_current_line from middle of multiline
        let mut t = ta_with("abc\ndef\nghi");
        t.set_cursor(5);
        t.kill_current_line();
        assert_eq!(t.text(), "abc\n\nghi");
        assert_eq!(t.cursor(), 4);

        // kill_current_line on empty line joins with previous
        let mut t = ta_with("abc\n\nghi");
        t.set_cursor(4);
        t.kill_current_line();
        assert_eq!(t.text(), "abc\nghi");
        assert_eq!(t.cursor(), 3);

        // kill_current_line at beginning of only line
        let mut t = ta_with("hello");
        t.set_cursor(0);
        t.kill_current_line();
        assert_eq!(t.text(), "");
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn delete_forward_word_variants() {
        let mut t = ta_with("hello   world ");
        t.set_cursor(0);
        t.delete_forward_word();
        assert_eq!(t.text(), "   world ");
        assert_eq!(t.cursor(), 0);

        let mut t = ta_with("hello   world ");
        t.set_cursor(1);
        t.delete_forward_word();
        assert_eq!(t.text(), "h   world ");
        assert_eq!(t.cursor(), 1);

        let mut t = ta_with("hello   world");
        t.set_cursor(t.text().len());
        t.delete_forward_word();
        assert_eq!(t.text(), "hello   world");
        assert_eq!(t.cursor(), t.text().len());

        let mut t = ta_with("foo   \nbar");
        t.set_cursor(3);
        t.delete_forward_word();
        assert_eq!(t.text(), "foo");
        assert_eq!(t.cursor(), 3);

        let mut t = ta_with("foo\nbar");
        t.set_cursor(3);
        t.delete_forward_word();
        assert_eq!(t.text(), "foo");
        assert_eq!(t.cursor(), 3);

        let mut t = ta_with("hello-world");
        t.set_cursor(0);
        t.delete_forward_word();
        assert_eq!(t.text(), "-world");
        assert_eq!(t.cursor(), 0);

        let mut t = ta_with("hello   world ");
        t.set_cursor(t.text().len() + 10);
        t.delete_forward_word();
        assert_eq!(t.text(), "hello   world ");
        assert_eq!(t.cursor(), t.text().len());
    }

    #[test]
    fn super_right_moves_to_end_of_line() {
        let mut t = ta_with("hello world\nsecond line");
        t.set_cursor(3); // middle of "hello world"
        t.input(KeyEvent::new(KeyCode::Right, KeyModifiers::SUPER));
        assert_eq!(t.cursor(), 11); // end of "hello world" (before \n)

        // Already at end of line → stays there
        t.input(KeyEvent::new(KeyCode::Right, KeyModifiers::SUPER));
        assert_eq!(t.cursor(), 11);
    }

    #[test]
    fn super_left_moves_to_beginning_of_line() {
        let mut t = ta_with("hello world\nsecond line");
        let second_line_start = t.text().find("second").unwrap();
        t.set_cursor(second_line_start + 4); // middle of "second line"
        t.input(KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER));
        assert_eq!(t.cursor(), second_line_start);

        // Already at beginning of line → stays there
        t.input(KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER));
        assert_eq!(t.cursor(), second_line_start);
    }

    #[test]
    fn super_backspace_kills_to_beginning_of_line() {
        let mut t = ta_with("hello world\nsecond line");
        let second_line_start = t.text().find("second").unwrap();
        t.set_cursor(second_line_start + 7); // after "second "
        t.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER));
        assert_eq!(t.text(), "hello world\nline");
        assert_eq!(t.cursor(), second_line_start);
    }

    #[test]
    fn ctrl_u_kills_to_beginning_of_line_keeps_text_after_cursor() {
        let mut t = ta_with("hello world");
        t.set_cursor(5);
        t.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), " world");
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn delete_forward_word_handles_atomic_elements() {
        let kind = ElementKind(0);

        let mut t = TextArea::new();
        t.insert_element("<element>", kind, None);
        t.insert_str(" tail");

        t.set_cursor(0);
        t.delete_forward_word();
        assert_eq!(t.text(), " tail");
        assert_eq!(t.cursor(), 0);

        let mut t = TextArea::new();
        t.insert_str("   ");
        t.insert_element("<element>", kind, None);
        t.insert_str(" tail");

        t.set_cursor(0);
        t.delete_forward_word();
        assert_eq!(t.text(), " tail");
        assert_eq!(t.cursor(), 0);

        let mut t = TextArea::new();
        t.insert_str("prefix ");
        t.insert_element("<element>", kind, None);
        t.insert_str(" tail");

        // cursor in the middle of the element, delete_forward_word deletes the element
        let elem_range = t.elements()[0].range.clone();
        let _ = t
            .text
            .set_cursor_byte(elem_range.start + (elem_range.len() / 2));
        t.delete_forward_word();
        assert_eq!(t.text(), "prefix  tail");
        assert_eq!(t.cursor(), elem_range.start);
    }

    // ===== Phase 1: Typed element tests =====

    #[test]
    fn element_id_is_unique_and_stable() {
        let mut t = TextArea::new();
        let kind = ElementKind(1);

        let id1 = t.insert_element("aaa", kind, None);
        let id2 = t.insert_element("bbb", kind, None);
        assert_ne!(id1, id2);

        // ids survive after deletion of the first element
        t.set_cursor(0);
        t.delete_forward(1); // deletes "aaa" atomically
        assert_eq!(t.elements().len(), 1);
        assert_eq!(t.elements()[0].id, id2);
    }

    #[test]
    fn element_kind_preserved() {
        let mut t = TextArea::new();
        let kind_paste = ElementKind(1);
        let kind_file = ElementKind(2);

        t.insert_element("paste", kind_paste, None);
        t.insert_element("file", kind_file, None);

        assert_eq!(t.elements()[0].kind, kind_paste);
        assert_eq!(t.elements()[1].kind, kind_file);
    }

    #[test]
    fn element_at_cursor_returns_element() {
        let mut t = TextArea::new();
        let kind = ElementKind(0);

        t.insert_str("before ");
        let id = t.insert_element("[paste]", kind, None);
        t.insert_str(" after");

        // Cursor is at end of element after insert_element
        // Move to start of element
        t.set_cursor(7); // "before " is 7 bytes, element starts at 7
        let elem = t.element_at_cursor().expect("should find element");
        assert_eq!(elem.id, id);
        assert_eq!(elem.kind, kind);

        // Cursor before element
        t.set_cursor(0);
        assert!(t.element_at_cursor().is_none());

        // Cursor after element
        t.set_cursor(t.text().len());
        assert!(t.element_at_cursor().is_none());
    }

    #[test]
    fn element_text_returns_buffer_text() {
        let mut t = TextArea::new();
        let id = t.insert_element("raw buffer content", ElementKind(0), None);
        assert_eq!(t.element_text(id), Some("raw buffer content"));

        // Non-existent id returns None
        let fake_id = ElementId(9999);
        assert_eq!(t.element_text(fake_id), None);
    }

    #[test]
    fn element_display_can_be_set_and_updated() {
        let mut t = TextArea::new();
        let display = Line::from("[Pasted 5 lines]");
        let id = t.insert_element("lots of raw text here", ElementKind(1), Some(display));

        // Verify display is set
        let elem = &t.elements()[0];
        assert!(elem.display.is_some());
        assert_eq!(
            elem.display.as_ref().unwrap().to_string(),
            "[Pasted 5 lines]"
        );

        // Update display
        let new_display = Line::from("[Pasted 5 lines, 200 chars]");
        t.set_element_display(id, Some(new_display));
        let elem = &t.elements()[0];
        assert_eq!(
            elem.display.as_ref().unwrap().to_string(),
            "[Pasted 5 lines, 200 chars]"
        );

        // Clear display
        t.set_element_display(id, None);
        assert!(t.elements()[0].display.is_none());

        // Buffer text is unchanged
        assert_eq!(t.element_text(id), Some("lots of raw text here"));
    }

    #[test]
    fn insert_element_returns_id_for_metadata_tracking() {
        let mut t = TextArea::new();
        let mut metadata: std::collections::HashMap<ElementId, String> =
            std::collections::HashMap::new();

        let id1 = t.insert_element("paste1", ElementKind(1), None);
        metadata.insert(id1, "First paste".to_string());

        let id2 = t.insert_element("paste2", ElementKind(1), None);
        metadata.insert(id2, "Second paste".to_string());

        // Verify we can look up metadata by id
        assert_eq!(metadata.get(&id1), Some(&"First paste".to_string()));
        assert_eq!(metadata.get(&id2), Some(&"Second paste".to_string()));

        // Delete first element
        t.set_cursor(0);
        t.delete_forward(1);

        // id2 still valid in our metadata map
        let remaining = &t.elements()[0];
        assert_eq!(remaining.id, id2);
        assert_eq!(
            metadata.get(&remaining.id),
            Some(&"Second paste".to_string())
        );
    }

    #[test]
    fn elements_returns_sorted_slice() {
        let mut t = TextArea::new();
        let kind = ElementKind(0);

        t.insert_str("aaa ");
        t.insert_element("BBB", kind, None);
        t.insert_str(" ccc ");
        t.insert_element("DDD", kind, None);

        let elems = t.elements();
        assert_eq!(elems.len(), 2);
        assert!(elems[0].range.start < elems[1].range.start);
        assert_eq!(&t.text()[elems[0].range.clone()], "BBB");
        assert_eq!(&t.text()[elems[1].range.clone()], "DDD");
    }

    // ===== Phase 2: Display rendering & truncation tests =====

    #[test]
    fn render_element_with_display_shows_display_text() {
        use ratatui::style::Stylize;

        let mut t = TextArea::new();
        let display = Line::from("[Pasted]".cyan());
        t.insert_element("raw content here", ElementKind(0), Some(display));

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&t), area, &mut buf);

        // The rendered buffer should show "[Pasted]" not "raw content here"
        let rendered: String = (0..area.width)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
            .collect::<String>();
        let rendered = rendered.trim_end();
        assert_eq!(rendered, "[Pasted]");
    }

    #[test]
    fn render_element_without_display_shows_buffer_text_cyan() {
        let mut t = TextArea::new();
        t.insert_element("hello", ElementKind(0), None);

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&t), area, &mut buf);

        // Should show "hello" with cyan foreground
        let cell = buf.cell((0, 0)).unwrap();
        assert_eq!(cell.symbol(), "h");
        assert_eq!(cell.fg, Color::Cyan);
    }

    #[test]
    fn truncate_line_display_no_truncation_needed() {
        let line = Line::from("[Short]");
        let result = truncate_line_display(&line, 20);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "[Short]");
    }

    #[test]
    fn truncate_line_display_with_bracket_preservation() {
        let line: Line<'static> = Line::from("[Pasted ~100 lines]");
        // Width 12: budget = 12 - 2 (ellipsis + bracket) = 10 chars content
        let result = truncate_line_display(&line, 12);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with(']'), "should preserve ]: got {text:?}");
        assert!(text.contains('…'), "should contain ellipsis: got {text:?}");
        assert_eq!(text, "[Pasted ~1…]");
    }

    #[test]
    fn truncate_line_display_without_bracket() {
        let line: Line<'static> = Line::from("very long display text");
        let result = truncate_line_display(&line, 10);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('…'));
        assert!(!text.ends_with(']'));
        // 9 chars content + 1 ellipsis = 10
        assert_eq!(text, "very long…");
    }

    #[test]
    fn truncate_line_display_zero_width() {
        let line: Line<'static> = Line::from("[Pasted]");
        let result = truncate_line_display(&line, 0);
        assert!(result.spans.is_empty() || result.width() == 0);
    }

    #[test]
    fn truncate_line_display_width_1() {
        let line: Line<'static> = Line::from("[Pasted]");
        let result = truncate_line_display(&line, 1);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        // Only room for "…" (the bracket can't fit with content)
        assert_eq!(text, "…");
    }

    #[test]
    fn truncate_preserves_multi_span_styles() {
        use ratatui::text::Span;

        let line: Line<'static> = Line::from(vec![
            Span::styled("[", Style::default().fg(Color::Yellow)),
            Span::styled("Pasted ~100 lines", Style::default().fg(Color::Cyan)),
            Span::styled("]", Style::default().fg(Color::Yellow)),
        ]);
        let result = truncate_line_display(&line, 10);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with(']'));
        assert!(text.contains('…'));
        // "[" (1) + content budget (10-2=8) + "…" (1) + "]" (1) = 11? No...
        // Budget: 10 - 2 = 8 for content. "[" is 1, so 7 more chars of "Pasted ~"
        // Result: "[Pasted …]" which is 10 wide
        assert!(
            result.width() <= 10,
            "width should be <= 10, got {}",
            result.width()
        );
    }

    #[test]
    fn render_element_with_prefix_text() {
        let mut t = TextArea::new();
        t.insert_str("hi ");
        let display = Line::from("[P]");
        t.insert_element("raw", ElementKind(0), Some(display));

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&t), area, &mut buf);

        let rendered: String = (0..area.width)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
            .collect::<String>();
        let rendered = rendered.trim_end();
        assert_eq!(rendered, "hi [P]");
    }

    #[test]
    fn render_text_after_element_uses_display_width() {
        // User scenario: "foo " + element("Clean build", display="[📎 Pasted 1 line, 11 chars]") + " abcde"
        // Display: "[📎 Pasted 1 line, 11 chars]" = 1+2+1+23+1 = 28 display cols
        // Buffer: "Clean build" = 11 bytes
        // Without fix, text after element renders at buffer x, overlapping with element display.
        let mut t = TextArea::new();
        t.insert_str("foo ");
        let display = Line::from(vec![
            ratatui::text::Span::raw("["),
            ratatui::text::Span::raw("📎 "),
            ratatui::text::Span::raw("Pasted 1 line, 11 chars"),
            ratatui::text::Span::raw("]"),
        ]);
        t.insert_element("Clean build", ElementKind(0), Some(display));
        t.insert_str(" abcde");

        // Area wide enough to fit everything
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&t), area, &mut buf);

        // Verify key cells: text before element, element display, text after element.
        // "foo " occupies cols 0-3
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "f");
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), " ");

        // Element display starts at col 4: "[📎 Pasted 1 line, 11 chars]"
        assert_eq!(buf.cell((4, 0)).unwrap().symbol(), "[");
        assert_eq!(buf.cell((5, 0)).unwrap().symbol(), "📎");
        // col 6 is the wide-char continuation cell
        assert_eq!(buf.cell((7, 0)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((8, 0)).unwrap().symbol(), "P");

        // Element display ends at col 31 ("]")
        assert_eq!(buf.cell((31, 0)).unwrap().symbol(), "]");

        // Text after element: " abcde" starting at col 32
        assert_eq!(buf.cell((32, 0)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((33, 0)).unwrap().symbol(), "a");
        assert_eq!(buf.cell((34, 0)).unwrap().symbol(), "b");
        assert_eq!(buf.cell((35, 0)).unwrap().symbol(), "c");
        assert_eq!(buf.cell((36, 0)).unwrap().symbol(), "d");
        assert_eq!(buf.cell((37, 0)).unwrap().symbol(), "e");
    }

    #[test]
    fn render_text_after_wider_display_element_simple() {
        // Simpler case: element buffer text "x" (1 byte), display "[LONG]" (6 cols)
        // Suffix text "!" should render at column 6, not column 1.
        let mut t = TextArea::new();
        let display = Line::from("[LONG]");
        t.insert_element("x", ElementKind(0), Some(display));
        t.insert_str("!");

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&t), area, &mut buf);

        let rendered: String = (0..area.width)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
            .collect::<String>();
        let rendered = rendered.trim_end();
        assert_eq!(rendered, "[LONG]!");
    }

    // ===== Phase 3: Display projection tests =====

    #[test]
    fn display_width_of_range_plain_text() {
        let t = ta_with("hello world");
        assert_eq!(t.display_width_of_range(0, 5), 5); // "hello"
        assert_eq!(t.display_width_of_range(0, 11), 11); // "hello world"
        assert_eq!(t.display_width_of_range(6, 11), 5); // "world"
    }

    #[test]
    fn insert_expands_tabs_to_spaces() {
        let mut t = TextArea::new();
        assert_eq!(t.tab_width(), 4);
        t.insert_str("a\tb");
        assert_eq!(t.text(), "a    b");
        assert_eq!(t.cursor(), 6);
        assert_eq!(t.display_width_of_range(0, t.text().len()), 6);

        let mut t2 = TextArea::new();
        t2.set_tab_width(8);
        t2.insert_str("x\ty");
        assert_eq!(t2.text(), "x        y");
        assert_eq!(t2.cursor(), 10);
        assert_eq!(t2.display_width_of_range(0, t2.text().len()), 10);

        let mut t3 = TextArea::new();
        t3.insert_str("\ta");
        assert_eq!(t3.text(), "    a");
        t3.set_text("");
        t3.insert_str("a\t");
        assert_eq!(t3.text(), "a    ");
        assert_eq!(t3.cursor(), 5);
        t3.set_text("");
        t3.insert_str("\t\t");
        assert_eq!(t3.text(), "        ");
        assert_eq!(t3.display_width_of_range(0, 8), 8);
        t3.insert_str("");
        assert_eq!(t3.text(), "        ");
        t3.insert_str_at(0, "z\t");
        assert_eq!(t3.text(), "z            ");
    }

    #[test]
    fn set_text_and_replace_expand_tabs() {
        let mut t = TextArea::new();
        t.set_text("col1\tcol2");
        assert_eq!(t.text(), "col1    col2");

        t.replace_range(4..8, "\t");
        assert_eq!(t.text(), "col1    col2");

        t.replace_range(4..4, "\tx");
        assert_eq!(t.text(), "col1    x    col2");
        // Insert-only replace places cursor at end of inserted expansion (4 spaces + 'x').
        assert_eq!(&t.text()[4..9], "    x");

        let mut t0 = TextArea::new();
        t0.set_tab_width(0);
        t0.insert_str("a\tb");
        assert_eq!(t0.text(), "a\tb");
        t0.set_text("x\ty");
        assert_eq!(t0.text(), "x\ty");
        // Passthrough: display width matches unicode-width (no expansion).
        assert_eq!(
            t0.display_width_of_range(0, t0.text().len()),
            "x\ty".width()
        );
        t0.set_cursor(t0.text().len());
        let area = Rect::new(0, 0, 80, 1);
        let (x, _y) = t0.cursor_pos(area).unwrap();
        assert_eq!(x as usize, "x\ty".width());
    }

    #[test]
    fn remaining_tabs_count_in_display_width_and_cursor() {
        // Simulate leftover tabs without going through expand (tab_width set after set_text
        // would still expand on set_text; inject via tab_width=0 then enable display tabs).
        let mut t = TextArea::new();
        t.set_tab_width(0);
        t.set_text("a\tb\tc");
        assert!(t.text().contains('\t'));
        t.set_tab_width(4);
        // "a" + 4 + "b" + 4 + "c" = 11
        assert_eq!(t.display_width_of_range(0, t.text().len()), 11);
        t.set_cursor(t.text().len());
        let area = Rect::new(0, 0, 80, 1);
        let (x, _y) = t.cursor_pos(area).unwrap();
        assert_eq!(x, 11);
    }

    #[test]
    fn set_tab_width_does_not_rewrite_existing_spaces() {
        let mut t = TextArea::new();
        t.insert_str("a\tb");
        assert_eq!(t.text(), "a    b");
        t.set_tab_width(8);
        assert_eq!(t.text(), "a    b");
        t.insert_str("\tc");
        assert_eq!(t.text(), "a    b        c");
    }

    #[test]
    fn multi_column_paste_tabs_readable() {
        let mut t = TextArea::new();
        t.insert_str("Name\tAge\tCity\nAda\t36\tLondon");
        assert_eq!(t.text(), "Name    Age    City\nAda    36    London");
        assert_eq!(t.cursor(), t.text().len());
        let end = t.text().len();
        let area = Rect::new(0, 0, 80, 3);
        let (x, _y) = t.cursor_pos(area).unwrap();
        // Ada(3) + 4 + 36(2) + 4 + London(6) = 19
        assert_eq!(x, 19);
        let bol = t.text().rfind('\n').map(|i| i + 1).unwrap_or(0);
        assert_eq!(x as usize, t.display_width_of_range(bol, end));
        let last_line = &t.text()[bol..];
        let (paint, paint_w) = paint_plain_for_display(last_line, 80, 4);
        assert_eq!(paint.as_ref(), last_line);
        assert_eq!(paint_w, 19);
    }

    #[test]
    fn insert_element_expands_tabs_and_covers_full_range() {
        let mut t = TextArea::new();
        t.insert_element("a\tb", ElementKind(0), None);
        assert_eq!(t.text(), "a    b");
        assert_eq!(t.elements().len(), 1);
        assert_eq!(t.elements()[0].range, 0..6);
        assert_eq!(t.cursor(), 6);

        let mut t2 = TextArea::new();
        t2.insert_element("a\tb\nc\td", ElementKind(1), Some(Line::from("[P]")));
        assert_eq!(t2.text(), "a    b\nc    d");
        assert_eq!(t2.elements()[0].range, 0..t2.text().len());
        assert_eq!(t2.cursor(), t2.text().len());
        assert!(!t2.text().contains('\t'));
    }

    #[test]
    fn replace_range_with_element_expands_tabs() {
        let mut t = TextArea::new();
        t.insert_str("xx");
        t.replace_range_with_element(0..2, "a\tb", ElementKind(0), None);
        assert_eq!(t.text(), "a    b");
        assert_eq!(t.elements()[0].range, 0..6);
        assert_eq!(t.cursor(), 6);
    }

    #[test]
    fn unicode_plus_tabs_expansion_and_residual() {
        let mut t = TextArea::new();
        t.insert_str("名\tAge");
        // 名 is typically width 2; plus 4 spaces + Age
        assert_eq!(t.text(), "名    Age");
        assert_eq!(
            t.display_width_of_range(0, t.text().len()),
            "名".width() + 4 + 3
        );
        t.set_cursor(t.text().len());
        let area = Rect::new(0, 0, 80, 1);
        let (x, _) = t.cursor_pos(area).unwrap();
        assert_eq!(x as usize, "名".width() + 4 + 3);

        let mut t2 = TextArea::new();
        t2.set_tab_width(0);
        t2.set_text("😀\tb");
        t2.set_tab_width(4);
        let expected = "😀".width() + 4 + 1;
        assert_eq!(t2.display_width_of_range(0, t2.text().len()), expected);
        t2.set_cursor(t2.text().len());
        let (x, _) = t2.cursor_pos(area).unwrap();
        assert_eq!(x as usize, expected);
    }

    #[test]
    fn tab_helpers_clip_and_paint() {
        assert_eq!(expand_tabs_with_width("a\tb", 4).as_ref(), "a    b");
        assert!(matches!(
            expand_tabs_with_width("a\tb", 0),
            std::borrow::Cow::Borrowed("a\tb")
        ));
        assert!(matches!(
            expand_tabs_with_width("ab", 4),
            std::borrow::Cow::Borrowed("ab")
        ));
        assert_eq!(plain_display_width_with_tab("a\tb\tc", 4), 11);
        assert_eq!(
            plain_display_width_with_tab("a\tb\tc", 0),
            "a\tb\tc".width()
        );
        assert_eq!(clip_str_to_display_width_with_tab("a\tb", 3, 4), "a");
        let (paint, w) = paint_plain_for_display("a\tb", 80, 4);
        assert_eq!(paint.as_ref(), "a    b");
        assert_eq!(w, 6);
        let (paint2, w2) = paint_plain_for_display("a\tb", 3, 4);
        assert_eq!(paint2.as_ref(), "a");
        assert_eq!(w2, 1);
    }

    #[test]
    fn display_width_of_range_with_display_element() {
        let mut t = TextArea::new();
        t.insert_str("ab");
        // Element has 100 bytes of buffer text but displays as "[P]" (3 chars)
        let buffer_text = "x".repeat(100);
        let display = Line::from("[P]");
        t.insert_element(&buffer_text, ElementKind(0), Some(display));
        t.insert_str("cd");

        // Range covering just "ab" = 2
        assert_eq!(t.display_width_of_range(0, 2), 2);
        // Range covering "ab" + element = 2 + 3 = 5
        assert_eq!(t.display_width_of_range(0, 102), 5);
        // Range covering "ab" + element + "cd" = 2 + 3 + 2 = 7
        assert_eq!(t.display_width_of_range(0, 104), 7);
        // Range covering just the element = 3
        assert_eq!(t.display_width_of_range(2, 102), 3);
        // Range covering just "cd" = 2
        assert_eq!(t.display_width_of_range(102, 104), 2);
    }

    #[test]
    fn cursor_pos_uses_display_width() {
        let mut t = TextArea::new();
        t.insert_str("ab");
        let buffer_text = "x".repeat(50);
        let display = Line::from("[P]");
        t.insert_element(&buffer_text, ElementKind(0), Some(display));
        t.insert_str("cd");

        // Cursor at end of element (buffer pos 52)
        t.set_cursor(52);
        let area = Rect::new(0, 0, 80, 1);
        let (x, _y) = t.cursor_pos(area).unwrap();
        // Expected: "ab" (2) + "[P]" (3) = column 5
        assert_eq!(x, 5);

        // Cursor at end of text (buffer pos 54)
        t.set_cursor(54);
        let (x, _y) = t.cursor_pos(area).unwrap();
        // Expected: "ab" (2) + "[P]" (3) + "cd" (2) = column 7
        assert_eq!(x, 7);

        // Cursor at start
        t.set_cursor(0);
        let (x, _y) = t.cursor_pos(area).unwrap();
        assert_eq!(x, 0);
    }

    #[test]
    fn display_width_no_elements() {
        let t = ta_with("abc");
        assert_eq!(t.display_width_of_range(0, 3), 3);
        assert_eq!(t.display_width_of_range(1, 2), 1);
        assert_eq!(t.display_width_of_range(3, 3), 0);
    }

    #[test]
    fn display_width_element_without_display() {
        let mut t = TextArea::new();
        t.insert_element("elem", ElementKind(0), None);
        // No display override — width should equal buffer text width
        assert_eq!(t.display_width_of_range(0, 4), 4);
    }

    // ===== Wide unicode in display text =====

    #[test]
    fn display_width_with_wide_unicode_display() {
        let mut t = TextArea::new();
        t.insert_str("ab");
        // Display text has emoji (each width 2) and CJK
        let display = Line::from("📎漢字"); // 2 + 2 + 2 = 6 display columns
        t.insert_element("raw", ElementKind(0), Some(display));
        t.insert_str("cd");

        // "ab" = 2, element display = 6, "cd" = 2 → total 10
        assert_eq!(t.display_width_of_range(0, 2), 2);
        assert_eq!(t.display_width_of_range(2, 5), 6); // element "raw" = 3 bytes
        assert_eq!(t.display_width_of_range(0, 7), 10); // "ab" + elem + "cd"
    }

    #[test]
    fn cursor_pos_with_wide_unicode_display() {
        let mut t = TextArea::new();
        t.insert_str("a");
        // Element display is "🚀" (width 2), buffer text is "xyz" (3 bytes)
        let display = Line::from("🚀");
        t.insert_element("xyz", ElementKind(0), Some(display));
        t.insert_str("b");

        let area = Rect::new(0, 0, 40, 1);

        // Cursor after "a" (at element start → element_at_cursor returns it)
        t.set_cursor(1);
        let (x, _) = t.cursor_pos(area).unwrap();
        assert_eq!(x, 1); // "a" = 1 col

        // Cursor after element (buffer pos 4 = 1 + 3)
        t.set_cursor(4);
        let (x, _) = t.cursor_pos(area).unwrap();
        assert_eq!(x, 3); // "a" (1) + "🚀" (2) = 3

        // Cursor at end "b" (buffer pos 5)
        t.set_cursor(5);
        let (x, _) = t.cursor_pos(area).unwrap();
        assert_eq!(x, 4); // "a" (1) + "🚀" (2) + "b" (1) = 4
    }

    #[test]
    fn truncate_display_with_wide_unicode() {
        // Display: "📎paste" = 2+5 = 7 cols, truncate to 5
        let line: Line<'static> = Line::from("📎paste");
        let result = truncate_line_display(&line, 5);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        // Budget = 5 - 1 (ellipsis) = 4 content cols → "📎pa" (2+1+1=4)
        assert!(text.contains('…'));
        assert!(
            result.width() <= 5,
            "width should be <= 5, got {}",
            result.width()
        );
    }

    #[test]
    fn clip_str_to_display_width_preserves_zwj_graphemes() {
        let s = "👩\u{200D}💻a";

        assert_eq!(clip_str_to_display_width(s, 0), "");
        assert_eq!(clip_str_to_display_width(s, 1), "");
        assert_eq!(clip_str_to_display_width(s, 2), "👩\u{200D}💻");
        assert_eq!(clip_str_to_display_width(s, 3), s);
    }

    #[test]
    fn truncate_display_preserves_zwj_graphemes() {
        let line: Line<'static> = Line::from("👩\u{200D}💻abc");
        let result = truncate_line_display(&line, 3);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();

        assert_eq!(text, "👩\u{200D}💻…");
        assert_eq!(result.width(), 3);
    }

    #[test]
    fn truncate_display_wide_char_at_boundary() {
        // Display: "ab🚀cd" = 2+2+2 = 6 cols, truncate to 4
        // Budget = 4 - 1 = 3 content cols. "ab" = 2, "🚀" = 2 → doesn't fit → "ab…"
        let line: Line<'static> = Line::from("ab🚀cd");
        let result = truncate_line_display(&line, 4);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "ab…");
        assert!(result.width() <= 4);
    }

    #[test]
    fn truncate_display_bracket_with_wide_chars() {
        // "[📎 pasted]" = 1+2+1+6+1 = 11 cols, truncate to 7
        // Budget = 7 - 2 (ellipsis + bracket) = 5 content cols → "[📎 p…]"
        let line: Line<'static> = Line::from("[📎 pasted]");
        let result = truncate_line_display(&line, 7);
        let text: String = result.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with(']'), "should preserve ]: got {text:?}");
        assert!(text.contains('…'));
        assert!(result.width() <= 7, "got {}", result.width());
    }

    #[test]
    fn render_element_with_wide_unicode_display() {
        let mut t = TextArea::new();
        let display = Line::from("📎漢");
        t.insert_element("hidden text", ElementKind(0), Some(display));

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&t), area, &mut buf);

        // Should show "📎漢" (4 display cols: 2+2)
        let cell0 = buf.cell((0, 0)).unwrap();
        assert_eq!(cell0.symbol(), "📎");
        let cell2 = buf.cell((2, 0)).unwrap();
        assert_eq!(cell2.symbol(), "漢");
    }

    // ===== Element-aware editing behavior (explicit tests) =====

    #[test]
    fn backspace_at_element_end_deletes_entire_element() {
        let mut t = TextArea::new();
        t.insert_str("before ");
        t.insert_element("[paste]", ElementKind(0), None);
        // Cursor is now at end of element
        assert_eq!(t.cursor(), 14); // "before " (7) + "[paste]" (7)

        t.delete_backward(1);
        assert_eq!(t.text(), "before ");
        assert_eq!(t.cursor(), 7);
        assert!(t.elements().is_empty());
    }

    #[test]
    fn delete_at_element_start_deletes_entire_element() {
        let mut t = TextArea::new();
        t.insert_element("[paste]", ElementKind(0), None);
        t.insert_str(" after");
        t.set_cursor(0);

        t.delete_forward(1);
        assert_eq!(t.text(), " after");
        assert_eq!(t.cursor(), 0);
        assert!(t.elements().is_empty());
    }

    #[test]
    fn left_right_navigation_jumps_over_element() {
        let mut t = TextArea::new();
        t.insert_str("a");
        t.insert_element("[elem]", ElementKind(0), None);
        t.insert_str("b");
        // text = "a[elem]b", element at 1..7

        // Start at end, move left: should jump from 8 → 7 (before 'b'),
        // then 7 → 1 (before element, atomic jump), then 1 → 0
        t.set_cursor(8);
        t.move_cursor_left(); // 8 → 7
        assert_eq!(t.cursor(), 7);
        t.move_cursor_left(); // 7 → 1 (atomic jump over "[elem]")
        assert_eq!(t.cursor(), 1);
        t.move_cursor_left(); // 1 → 0
        assert_eq!(t.cursor(), 0);

        // Now right: 0 → 1, then 1 → 7 (atomic jump), then 7 → 8
        t.move_cursor_right(); // 0 → 1
        assert_eq!(t.cursor(), 1);
        t.move_cursor_right(); // 1 → 7 (atomic jump over "[elem]")
        assert_eq!(t.cursor(), 7);
        t.move_cursor_right(); // 7 → 8
        assert_eq!(t.cursor(), 8);
    }

    #[test]
    fn word_delete_backward_removes_element_atomically() {
        let mut t = TextArea::new();
        t.insert_str("prefix ");
        t.insert_element("[pasted content]", ElementKind(0), None);
        // Cursor at end of element
        assert_eq!(t.cursor(), 23); // 7 + 16

        t.delete_backward_word();
        // Should remove the entire element (it's one "word" unit)
        assert_eq!(t.text(), "prefix ");
        assert!(t.elements().is_empty());
    }

    #[test]
    fn word_delete_forward_removes_element_atomically() {
        let mut t = TextArea::new();
        t.insert_element("[element]", ElementKind(0), None);
        t.insert_str(" suffix");
        t.set_cursor(0);

        t.delete_forward_word();
        assert_eq!(t.text(), " suffix");
        assert!(t.elements().is_empty());
    }

    #[test]
    fn kill_to_eol_removes_element_in_range() {
        let mut t = TextArea::new();
        t.insert_str("start ");
        t.insert_element("[elem]", ElementKind(0), None);
        t.insert_str(" end");
        t.set_cursor(6); // right after "start "

        t.kill_to_end_of_line();
        assert_eq!(t.text(), "start ");
        assert!(t.elements().is_empty());
    }

    // ===== Element newline skipping in BOL/EOL =====
    //
    // Elements with multi-line buffer text (e.g. paste blocks) should be
    // treated as atomic for line navigation. Newlines inside elements are
    // NOT line boundaries.

    #[test]
    fn ctrl_e_skips_newline_inside_element() {
        // "foo <element:line1\nline2> bar"
        // Ctrl-E from start should go to end of the whole line, not stop
        // at the \n inside the element.
        let mut t = TextArea::new();
        t.insert_str("foo ");
        t.insert_element("line1\nline2", ElementKind(1), None);
        t.insert_str(" bar");
        // buffer = "foo line1\nline2 bar" (19 bytes), element at 4..15

        t.set_cursor(0);
        t.move_cursor_to_end_of_line(false);
        assert_eq!(t.cursor(), t.text().len()); // should reach end of "foo ... bar"
    }

    #[test]
    fn ctrl_a_skips_newline_inside_element() {
        let mut t = TextArea::new();
        t.insert_str("foo ");
        t.insert_element("line1\nline2", ElementKind(1), None);
        t.insert_str(" bar");
        // buffer = "foo line1\nline2 bar" (19 bytes)

        // Set cursor to end
        t.set_cursor(t.text().len());
        t.move_cursor_to_beginning_of_line(false);
        assert_eq!(t.cursor(), 0); // should reach beginning, not stop inside element
    }

    #[test]
    fn ctrl_e_from_element_boundary_skips_to_real_eol() {
        let mut t = TextArea::new();
        t.insert_str("foo ");
        t.insert_element("a\nb\nc", ElementKind(1), None);
        t.insert_str(" bar");
        // element at 4..9

        // Place cursor at element start boundary
        t.set_cursor(4);
        t.move_cursor_to_end_of_line(false);
        assert_eq!(t.cursor(), t.text().len());
    }

    #[test]
    fn ctrl_a_from_after_element_skips_to_real_bol() {
        let mut t = TextArea::new();
        t.insert_str("foo ");
        t.insert_element("a\nb\nc", ElementKind(1), None);
        t.insert_str(" bar");
        // element at 4..9, " bar" at 9..13

        // Place cursor on " bar"
        t.set_cursor(10);
        t.move_cursor_to_beginning_of_line(false);
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn kill_to_eol_with_multiline_element() {
        let mut t = TextArea::new();
        t.insert_str("foo ");
        t.insert_element("x\ny\nz", ElementKind(1), None);
        t.insert_str(" bar");
        // buffer = "foo x\ny\nz bar", element at 4..9

        t.set_cursor(0);
        t.kill_to_end_of_line();
        // Should kill everything on the line: "foo " + element + " bar"
        assert_eq!(t.text(), "");
    }

    #[test]
    fn kill_to_bol_with_multiline_element() {
        let mut t = TextArea::new();
        t.insert_str("foo ");
        t.insert_element("x\ny\nz", ElementKind(1), None);
        t.insert_str(" bar");

        t.set_cursor(t.text().len()); // end of " bar"
        t.kill_to_beginning_of_line();
        assert_eq!(t.text(), "");
    }

    #[test]
    fn bol_eol_with_real_newline_and_element() {
        // "hello\nfoo <element:a\nb> bar"
        // Two real lines. Element is on the second line.
        let mut t = TextArea::new();
        t.insert_str("hello\nfoo ");
        t.insert_element("a\nb", ElementKind(1), None);
        t.insert_str(" bar");
        // buffer = "hello\nfoo a\nb bar"
        // Real newline at 5. Element at 10..13. Element's \n at 11 should be skipped.

        // From start of second line (pos 6), Ctrl-E should reach end
        t.set_cursor(6);
        t.move_cursor_to_end_of_line(false);
        assert_eq!(t.cursor(), t.text().len());

        // From end, Ctrl-A should go back to pos 6
        t.move_cursor_to_beginning_of_line(false);
        assert_eq!(t.cursor(), 6);
    }

    #[test]
    fn bol_eol_no_element_unchanged() {
        // Verify the fix doesn't break normal (no-element) behavior.
        let mut t = TextArea::new();
        t.insert_str("line1\nline2\nline3");

        t.set_cursor(6); // start of "line2"
        t.move_cursor_to_end_of_line(false);
        assert_eq!(t.cursor(), 11); // end of "line2" (before \n)

        t.move_cursor_to_beginning_of_line(false);
        assert_eq!(t.cursor(), 6);
    }

    // ===== Element-display-aware wrapping =====

    #[test]
    fn wrapping_uses_element_display_width() {
        // Scenario: "foo bar " + element("Clean build", display 28 cols) at width 20.
        // Buffer text: "foo bar Clean build" = 19 buffer cols → textwrap says it fits on one line.
        // Display text: "foo bar [📎 Pasted 1 line, 11 chars]" = 8 + 28 = 36 display cols → should wrap.
        let mut t = TextArea::new();
        t.insert_str("foo bar ");
        let display = Line::from("[📎 Pasted 1 line, 11 chars]"); // 28 display cols
        t.insert_element("Clean build", ElementKind(0), Some(display));
        // buffer = "foo bar Clean build" (19 bytes)

        let lines = t.wrapped_lines(20);
        // The element display (28 cols) doesn't fit after "foo bar " (8 cols) on a 20-col line.
        // But it DOES fit on a fresh 20-col line (28 > 20, so it overflows but gets its own line).
        // Expected: line 1 = "foo bar ", line 2 = element.
        assert!(
            lines.len() >= 2,
            "Expected wrapping to produce at least 2 lines, got {} lines. \
             Line ranges: {:?}",
            lines.len(),
            &*lines,
        );
    }

    #[test]
    fn wrapping_element_fits_on_next_line() {
        // Element display (10 cols) doesn't fit after "hello " (6 cols) on 12-col line,
        // but fits on a fresh line.
        let mut t = TextArea::new();
        t.insert_str("hello ");
        let display = Line::from("[Pasted!]"); // 9 display cols
        t.insert_element("xy", ElementKind(0), Some(display));
        t.insert_str(" z");
        // buffer: "hello xy z" (10 bytes)
        // display: "hello [Pasted!] z" = 6 + 9 + 2 = 17 display cols

        let lines = t.wrapped_lines(12);
        // Line 1: "hello " (6 cols, fits)
        // Line 2: "[Pasted!] z" (9 + 2 = 11 cols, fits in 12)
        assert_eq!(
            lines.len(),
            2,
            "Expected 2 wrapped lines, got {}. Ranges: {:?}",
            lines.len(),
            &*lines,
        );
    }

    #[test]
    fn wrapping_element_without_display_uses_buffer_width() {
        // Element without display override: wrapping should use buffer text width (unchanged behavior).
        let mut t = TextArea::new();
        t.insert_str("hello ");
        t.insert_element("xy", ElementKind(0), None);
        t.insert_str(" z");
        // buffer: "hello xy z" (10 bytes), no display override
        // display = buffer = "hello xy z" = 10 cols

        let lines = t.wrapped_lines(12);
        // 10 cols fits on 12-col line → 1 line
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn wrapping_element_display_renders_on_correct_lines() {
        // End-to-end: wrapping + rendering with display element.
        // "abc " (4) + element("xy", display="[ELEM]" = 6 cols) + " d" (2)
        // At width 8: "abc " (4) + "[ELEM]" (6) = 10 > 8 → wrap before element
        // Line 1: "abc " (4 cols), Line 2: "[ELEM] d" (8 cols)
        let mut t = TextArea::new();
        t.insert_str("abc ");
        let display = Line::from("[ELEM]");
        t.insert_element("xy", ElementKind(0), Some(display));
        t.insert_str(" d");
        // buffer: "abc xy d" (8 bytes)

        // Check wrapping (drop the Ref before rendering)
        {
            let lines = t.wrapped_lines(8);
            assert_eq!(
                lines.len(),
                2,
                "Should wrap into 2 lines, got {:?}",
                &*lines
            );
        }

        // Render and verify
        let area = Rect::new(0, 0, 8, 2);
        let mut buf = Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&t), area, &mut buf);

        // Line 1 (y=0): "abc " padded to 8
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "a");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "b");
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "c");
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), " ");

        // Line 2 (y=1): "[ELEM] d"
        assert_eq!(buf.cell((0, 1)).unwrap().symbol(), "[");
        assert_eq!(buf.cell((1, 1)).unwrap().symbol(), "E");
        assert_eq!(buf.cell((5, 1)).unwrap().symbol(), "]");
        assert_eq!(buf.cell((6, 1)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((7, 1)).unwrap().symbol(), "d");
    }

    #[test]
    fn wrapping_element_with_newlines_stays_single_line() {
        // When an element's buffer text contains \n, wrapping must NOT split at those
        // newlines. The element's display is a single-line chip; the \n is internal.
        // Scenario: "hello " + element("line1\nline2\nline3", display="[paste]") + " world"
        // Buffer: "hello line1\nline2\nline3 world"  (contains \n inside element)
        // Display: "hello [paste] world" = 6 + 7 + 6 = 19 cols
        // At width 40: should be 1 visual line.
        let mut t = TextArea::new();
        t.insert_str("hello ");
        let display = Line::from("[paste]"); // 7 display cols
        t.insert_element("line1\nline2\nline3", ElementKind(0), Some(display));
        t.insert_str(" world");
        // buffer: "hello line1\nline2\nline3 world"

        let lines = t.wrapped_lines(40);
        assert_eq!(
            lines.len(),
            1,
            "Element with internal \\n should NOT create extra visual lines. \
             Got {} lines: {:?}",
            lines.len(),
            &*lines,
        );
    }

    #[test]
    fn cursor_pos_after_multiline_element() {
        // After inserting text after a multiline element, the cursor should be on the
        // same visual line as the element chip, not bumped down by internal newlines.
        let mut t = TextArea::new();
        t.insert_str("hello ");
        let display = Line::from("[paste]"); // 7 display cols
        t.insert_element("line1\nline2", ElementKind(0), Some(display));
        t.insert_str(" world");

        let area = Rect::new(0, 0, 80, 10);
        let pos = t.cursor_pos(area);
        assert_eq!(
            pos,
            Some((19, 0)), // 6 + 7 + 6 = 19, row 0
            "Cursor should be at col 19, row 0 after multiline element. \
             Got {:?}. Buffer: {:?}, cursor byte: {}",
            pos,
            t.text(),
            t.cursor(),
        );
    }

    #[test]
    fn yank_restores_last_kill() {
        let mut t = ta_with("hello");
        t.set_cursor(0);
        t.kill_to_end_of_line();
        assert_eq!(t.text(), "");
        assert_eq!(t.cursor(), 0);

        t.yank();
        assert_eq!(t.text(), "hello");
        assert_eq!(t.cursor(), 5);

        let mut t = ta_with("hello world");
        t.set_cursor(t.text().len());
        t.delete_backward_word();
        assert_eq!(t.text(), "hello ");
        assert_eq!(t.cursor(), 6);

        t.yank();
        assert_eq!(t.text(), "hello world");
        assert_eq!(t.cursor(), 11);

        let mut t = ta_with("hello");
        t.set_cursor(5);
        t.kill_to_beginning_of_line();
        assert_eq!(t.text(), "");
        assert_eq!(t.cursor(), 0);

        t.yank();
        assert_eq!(t.text(), "hello");
        assert_eq!(t.cursor(), 5);
    }

    #[test]
    fn no_op_kill_preserves_the_kill_buffer() {
        let mut textarea = ta_with("hello");
        textarea.set_cursor(0);
        textarea.kill_to_end_of_line();
        assert_eq!(textarea.kill_buffer, "hello");

        textarea.set_text("world");
        textarea.set_cursor(textarea.text().len());
        textarea.kill_to_end_of_line();
        textarea.yank();
        assert_eq!(textarea.text(), "worldhello");
    }

    #[test]
    fn kill_buffer_survives_set_text() {
        // A cut must outlive the buffer reset that send does via set_text("").
        let mut t = ta_with("hello");
        t.set_cursor(0);
        t.kill_to_end_of_line();
        assert_eq!(t.text(), "");

        t.set_text(""); // send resets the prompt
        assert_eq!(t.text(), "");

        t.yank();
        assert_eq!(t.text(), "hello");
        assert_eq!(t.cursor(), 5);
    }

    #[test]
    fn cursor_left_and_right_handle_graphemes() {
        let mut t = ta_with("a👍b");
        t.set_cursor(t.text().len());

        t.move_cursor_left(); // before 'b'
        let after_first_left = t.cursor();
        t.move_cursor_left(); // before '👍'
        let after_second_left = t.cursor();
        t.move_cursor_left(); // before 'a'
        let after_third_left = t.cursor();

        assert!(after_first_left < t.text().len());
        assert!(after_second_left < after_first_left);
        assert!(after_third_left < after_second_left);

        // Move right back to end safely
        t.move_cursor_right();
        t.move_cursor_right();
        t.move_cursor_right();
        assert_eq!(t.cursor(), t.text().len());
    }

    #[test]
    fn control_b_and_f_move_cursor() {
        let mut t = ta_with("abcd");
        t.set_cursor(1);

        t.input(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(t.cursor(), 2);

        t.input(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(t.cursor(), 1);
    }

    #[test]
    fn control_b_f_fallback_control_chars_move_cursor() {
        let mut t = ta_with("abcd");
        t.set_cursor(2);

        // Simulate terminals that send C0 control chars without CONTROL modifier.
        // ^B (U+0002) should move left
        t.input(KeyEvent::new(KeyCode::Char('\u{0002}'), KeyModifiers::NONE));
        assert_eq!(t.cursor(), 1);

        // ^F (U+0006) should move right
        t.input(KeyEvent::new(KeyCode::Char('\u{0006}'), KeyModifiers::NONE));
        assert_eq!(t.cursor(), 2);
    }

    /// Regression (user report): Ctrl+W must rubout to whitespace, not stop
    /// at punctuation.
    #[test]
    fn ctrl_w_unix_word_rubout_deletes_to_whitespace() {
        let mut t = ta_with("git commit -m hello-world");
        t.set_cursor(t.text().len());
        t.input(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "git commit -m ");
        t.input(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "git commit ");
    }

    #[test]
    fn unix_word_rubout_whitespace_runs_paths_and_edges() {
        let mut t = ta_with("cat path/to/file.rs   ");
        t.set_cursor(t.text().len());
        t.delete_backward_unix_word();
        assert_eq!(t.text(), "cat ");
        assert_eq!(t.cursor(), 4);

        let mut t = ta_with("foo bar-baz");
        t.set_cursor(7); // foo bar|-baz
        t.delete_backward_unix_word();
        assert_eq!(t.text(), "foo -baz");
        assert_eq!(t.cursor(), 4);

        // Newlines are whitespace: rubout crosses line boundaries.
        let mut t = ta_with("line1\nword  ");
        t.set_cursor(t.text().len());
        t.delete_backward_unix_word();
        assert_eq!(t.text(), "line1\n");

        let mut t = ta_with("");
        t.delete_backward_unix_word();
        assert_eq!(t.text(), "");
        let mut t = ta_with("   ");
        t.set_cursor(3);
        t.delete_backward_unix_word();
        assert_eq!(t.text(), "");
    }

    /// Readline parity: only C-w is whitespace-delimited; M-DEL/C-Backspace
    /// stay chunked.
    #[test]
    fn alt_backspace_keeps_word_chunk_semantics() {
        let mut t = ta_with("hello-world");
        t.set_cursor(t.text().len());
        t.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(t.text(), "hello-");

        let mut t = ta_with("hello-world");
        t.set_cursor(t.text().len());
        t.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(t.text(), "hello-");
    }

    #[test]
    fn delete_backward_word_alt_keys() {
        // Test the custom Alt+Ctrl+h binding
        let mut t = ta_with("hello world");
        t.set_cursor(t.text().len()); // cursor at the end
        t.input(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(t.text(), "hello ");
        assert_eq!(t.cursor(), 6);

        // Test the standard Alt+Backspace binding
        let mut t = ta_with("hello world");
        t.set_cursor(t.text().len()); // cursor at the end
        t.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(t.text(), "hello ");
        assert_eq!(t.cursor(), 6);
    }

    #[test]
    fn ctrl_backspace_deletes_backward_word() {
        let mut t = ta_with("hello world");
        t.set_cursor(t.text().len());
        t.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(t.text(), "hello ");
        assert_eq!(t.cursor(), 6);

        // From end of middle word: deletes "bar", leaves surrounding spaces
        let mut t = ta_with("foo bar baz");
        t.set_cursor(7); // after "bar"
        t.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(t.text(), "foo  baz");
        assert_eq!(t.cursor(), 4);
    }

    #[test]
    fn ctrl_delete_deletes_forward_word() {
        // Mirror of ctrl_backspace_deletes_backward_word.
        let mut t = ta_with("hello world");
        t.set_cursor(0);
        t.input(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL));
        assert_eq!(t.text(), " world");
        assert_eq!(t.cursor(), 0);

        // From start of middle word: deletes "bar", leaves surrounding spaces
        let mut t = ta_with("foo bar baz");
        t.set_cursor(4); // before "bar"
        t.input(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL));
        assert_eq!(t.text(), "foo  baz");
        assert_eq!(t.cursor(), 4);
    }

    #[test]
    fn delete_backward_word_handles_narrow_no_break_space() {
        let mut t = ta_with("32\u{202F}AM");
        t.set_cursor(t.text().len());
        t.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        pretty_assertions::assert_eq!(t.text(), "32\u{202F}");
        pretty_assertions::assert_eq!(t.cursor(), t.text().len());
    }

    #[test]
    fn delete_forward_word_with_without_alt_modifier() {
        let mut t = ta_with("hello world");
        t.set_cursor(0);
        t.input(KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT));
        assert_eq!(t.text(), " world");
        assert_eq!(t.cursor(), 0);

        let mut t = ta_with("hello");
        t.set_cursor(0);
        t.input(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(t.text(), "ello");
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn alt_d_deletes_forward_word() {
        // Alt+D (Meta-d, Emacs) → delete forward word
        let mut t = ta_with("hello world foo");
        t.set_cursor(0);
        t.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(t.text(), " world foo");
        assert_eq!(t.cursor(), 0);

        // Alt+D at a word boundary
        let mut t = ta_with("hello world");
        t.set_cursor(5); // cursor right after "hello"
        t.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(t.text(), "hello");
        assert_eq!(t.cursor(), 5);

        // Super+D (Cmd+D on macOS with Kitty protocol) also works
        let mut t = ta_with("hello world");
        t.set_cursor(0);
        t.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::SUPER));
        assert_eq!(t.text(), " world");
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn ctrl_p_moves_cursor_up() {
        let mut t = ta_with("first\nsecond\nthird");
        let second_line_start = 6; // after "first\n"
        t.set_cursor(second_line_start + 2); // middle of "second"
        t.input(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        // Should be on first line now
        assert!(t.cursor() < second_line_start);
    }

    #[test]
    fn ctrl_n_moves_cursor_down() {
        let mut t = ta_with("first\nsecond\nthird");
        t.set_cursor(2); // middle of "first"
        t.input(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        let second_line_start = 6;
        // Should be on second line now
        assert!(t.cursor() >= second_line_start);
    }

    #[test]
    fn control_h_backspace() {
        // Test Ctrl+H as backspace
        let mut t = ta_with("12345");
        t.set_cursor(3); // cursor after '3'
        t.input(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "1245");
        assert_eq!(t.cursor(), 2);

        // Test Ctrl+H at beginning (should be no-op)
        t.set_cursor(0);
        t.input(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "1245");
        assert_eq!(t.cursor(), 0);

        // Test Ctrl+H at end
        t.set_cursor(t.text().len());
        t.input(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "124");
        assert_eq!(t.cursor(), 3);
    }
    #[test]
    fn char_bs_backspace() {
        // Test Char('\x08') (BS) as backspace
        let mut t = ta_with("12345");
        t.set_cursor(3); // cursor after '3'
        t.input(KeyEvent::new(KeyCode::Char('\x08'), KeyModifiers::NONE));
        assert_eq!(t.text(), "1245");
        assert_eq!(t.cursor(), 2);
    }

    #[test]
    fn char_del_deletes_backward() {
        // Char('\x7f') (DEL) should delete backward — on Unix terminals,
        // Backspace sends 0x7F in legacy mode (no Kitty protocol).
        let mut t = ta_with("12345");
        t.set_cursor(2); // cursor after '2'
        t.input(KeyEvent::new(KeyCode::Char('\x7f'), KeyModifiers::NONE));
        assert_eq!(t.text(), "1345");
        assert_eq!(t.cursor(), 1);
    }

    #[test]
    fn raw_delete_chars_ignore_stray_modifiers() {
        for raw in ['\u{0008}', '\u{007f}'] {
            for modifiers in [
                KeyModifiers::ALT,
                KeyModifiers::CONTROL,
                KeyModifiers::SUPER,
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            ] {
                let mut t = ta_with("alpha beta");
                t.input(KeyEvent::new(KeyCode::Char(raw), modifiers));
                assert_eq!(
                    t.text(),
                    "alpha bet",
                    "raw {raw:?} with {modifiers:?} must delete one grapheme",
                );
            }
        }
    }

    #[test]
    fn del_char_treated_as_backspace() {
        // When Kitty keyboard protocol gets silently popped, Backspace can
        // arrive as raw DEL (0x7F) instead of KeyCode::Backspace. Ensure it
        // deletes backward instead of inserting an invisible character.
        let mut t = ta_with("hello");
        t.set_cursor(3);
        t.input(KeyEvent::new(KeyCode::Char('\u{007f}'), KeyModifiers::NONE));
        assert_eq!(t.text(), "helo");
        assert_eq!(t.cursor(), 2);

        // At beginning: no-op
        t.set_cursor(0);
        t.input(KeyEvent::new(KeyCode::Char('\u{007f}'), KeyModifiers::NONE));
        assert_eq!(t.text(), "helo");
        assert_eq!(t.cursor(), 0);

        t.set_cursor(t.text().len());
        t.input(KeyEvent::new(KeyCode::Char('\u{007f}'), KeyModifiers::ALT));
        assert_eq!(t.text(), "hel");
        assert_eq!(t.cursor(), 3);
    }

    #[test]
    fn bs_char_treated_as_backspace() {
        // BS (0x08) arriving as Char without CONTROL modifier should also
        // delete backward (Ctrl-H without the modifier flag).
        let mut t = ta_with("abcde");
        t.set_cursor(4);
        t.input(KeyEvent::new(KeyCode::Char('\u{0008}'), KeyModifiers::NONE));
        assert_eq!(t.text(), "abce");
        assert_eq!(t.cursor(), 3);
    }

    #[test]
    fn del_char_with_selection_deletes_selection() {
        // DEL (0x7F) arriving as Char with an active selection should delete
        // the selection cleanly, not insert an invisible control character.
        let mut t = ta_with("hello world");
        t.set_selection(0, 5);
        t.input(KeyEvent::new(KeyCode::Char('\u{007f}'), KeyModifiers::NONE));
        assert_eq!(t.text(), " world");
        assert_eq!(t.cursor(), 0);
        assert!(t.selection_range().is_none());
    }

    #[test]
    fn cursor_vertical_movement_across_lines_and_bounds() {
        let mut t = ta_with("short\nloooooooooong\nmid");
        // Place cursor on second line, column 5
        let second_line_start = 6; // after first '\n'
        t.set_cursor(second_line_start + 5);

        // Move up: target column preserved, clamped by line length
        t.move_cursor_up();
        assert_eq!(t.cursor(), 5); // first line has len 5

        // Move up again goes to start of text
        t.move_cursor_up();
        assert_eq!(t.cursor(), 0);

        // Move down: from start to target col tracked
        t.move_cursor_down();
        // On first move down, we should land on second line, at col 0 (target col remembered as 0)
        let pos_after_down = t.cursor();
        assert!(pos_after_down >= second_line_start);

        // Move down again to third line; clamp to its length
        t.move_cursor_down();
        let third_line_start = t.text().find("mid").unwrap();
        let third_line_end = third_line_start + 3;
        assert!(t.cursor() >= third_line_start && t.cursor() <= third_line_end);

        // Moving down at last line jumps to end
        t.move_cursor_down();
        assert_eq!(t.cursor(), t.text().len());
    }

    #[test]
    fn home_end_and_emacs_style_home_end() {
        let mut t = ta_with("one\ntwo\nthree");
        // Position at middle of second line
        let second_line_start = t.text().find("two").unwrap();
        t.set_cursor(second_line_start + 1);

        t.move_cursor_to_beginning_of_line(false);
        assert_eq!(t.cursor(), second_line_start);

        // Ctrl-A behavior: if at BOL, go to beginning of previous line
        t.move_cursor_to_beginning_of_line(true);
        assert_eq!(t.cursor(), 0); // beginning of first line

        // Move to EOL of first line
        t.move_cursor_to_end_of_line(false);
        assert_eq!(t.cursor(), 3);

        // Ctrl-E: if at EOL, go to end of next line
        t.move_cursor_to_end_of_line(true);
        // end of second line ("two") is right before its '\n'
        let end_second_nl = t.text().find("\nthree").unwrap();
        assert_eq!(t.cursor(), end_second_nl);
    }

    #[test]
    fn home_end_use_visual_row_when_soft_wrapped() {
        // width 4 → "abcd" | "efgh" | "ij"
        let mut t = ta_with("abcdefghij");
        let _ = t.desired_height(4);
        t.set_cursor(6); // mid second visual row

        t.move_cursor_to_beginning_of_line(false);
        assert_eq!(t.cursor(), 4);
        t.move_cursor_to_end_of_line(false);
        assert_eq!(t.cursor(), 7);

        t.input(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(t.cursor(), 4);
        t.input(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(t.cursor(), 7);

        // Last visual row: End → text end; Home → row start.
        t.set_cursor(8);
        t.move_cursor_to_end_of_line(false);
        assert_eq!(t.cursor(), t.text().len());
        t.move_cursor_to_beginning_of_line(false);
        assert_eq!(t.cursor(), 8);

        // Ctrl+A/E stay logical.
        t.set_cursor(6);
        t.move_cursor_to_beginning_of_line(true);
        assert_eq!(t.cursor(), 0);
        t.set_cursor(6);
        t.move_cursor_to_end_of_line(true);
        assert_eq!(t.cursor(), t.text().len());
    }

    #[test]
    fn end_of_line_or_down_at_end_of_text() {
        let mut t = ta_with("one\ntwo");
        // Place cursor at absolute end of the text
        t.set_cursor(t.text().len());
        // Should remain at end without panicking
        t.move_cursor_to_end_of_line(true);
        assert_eq!(t.cursor(), t.text().len());

        // Also verify behavior when at EOL of a non-final line:
        let eol_first_line = 3; // index of '\n' in "one\ntwo"
        t.set_cursor(eol_first_line);
        t.move_cursor_to_end_of_line(true);
        assert_eq!(t.cursor(), t.text().len()); // moves to end of next (last) line
    }

    #[test]
    fn word_navigation_helpers() {
        let t = ta_with("  alpha  beta   gamma");
        let mut t = t; // make mutable for set_cursor
        // Put cursor after "alpha"
        let after_alpha = t.text().find("alpha").unwrap() + "alpha".len();
        t.set_cursor(after_alpha);
        assert_eq!(t.beginning_of_previous_word(), 2); // skip initial spaces

        // Put cursor at start of beta
        let beta_start = t.text().find("beta").unwrap();
        t.set_cursor(beta_start);
        assert_eq!(t.end_of_next_word(), beta_start + "beta".len());

        // If at end, end_of_next_word returns len
        t.set_cursor(t.text().len());
        assert_eq!(t.end_of_next_word(), t.text().len());
    }

    #[test]
    fn word_navigation_splits_on_hyphen() {
        let mut t = ta_with("hello-world");
        let hyphen = t.text().find('-').unwrap();
        let after_hyphen = hyphen + 1;

        t.set_cursor(t.text().len());
        assert_eq!(t.beginning_of_previous_word(), after_hyphen);

        t.set_cursor(after_hyphen);
        assert_eq!(t.beginning_of_previous_word(), hyphen);

        t.set_cursor(hyphen);
        assert_eq!(t.beginning_of_previous_word(), 0);

        t.set_cursor(0);
        assert_eq!(t.end_of_next_word(), hyphen);

        t.set_cursor(hyphen);
        assert_eq!(t.end_of_next_word(), after_hyphen);

        t.set_cursor(after_hyphen);
        assert_eq!(t.end_of_next_word(), t.text().len());
    }

    #[test]
    fn alt_arrow_navigation_splits_on_hyphen() {
        let mut t = ta_with("hello-world");
        let hyphen = t.text().find('-').unwrap();
        let after_hyphen = hyphen + 1;
        let end = t.text().len();

        t.set_cursor(0);
        t.input(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(t.cursor(), hyphen);

        t.input(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(t.cursor(), after_hyphen);

        t.input(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(t.cursor(), end);

        t.input(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(t.cursor(), after_hyphen);

        t.input(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(t.cursor(), hyphen);

        t.input(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn cursor_at_wrap_boundary_shows_on_next_line() {
        // When typing fills an entire line, the cursor sits at the exact wrap
        // boundary.  It should be reported on the *next* visual line at col 0,
        // not at col == width (which is the invisible right border).

        // Case 1: text exactly fills one line — cursor at text.len()
        let mut t = ta_with("abcde");
        let area = Rect::new(0, 0, 5, 3); // width 5
        t.set_cursor(5); // cursor right after 'e'

        let (x, y) = t.cursor_pos(area).unwrap();
        assert_eq!(x, 0, "cursor x should be 0 (start of virtual next line)");
        assert_eq!(y, 1, "cursor y should be 1 (next line)");

        // Case 2: text wraps — cursor at the boundary between two wrapped lines
        let mut t = ta_with("abcdefgh");
        let area = Rect::new(0, 0, 5, 3); // width 5, wraps after 'e'
        // cursor at position 5 = start of "fgh" = should be col 0, row 1
        t.set_cursor(5);

        let (x, y) = t.cursor_pos(area).unwrap();
        assert_eq!(x, 0, "cursor at wrap point should be col 0 of next line");
        assert_eq!(y, 1, "cursor at wrap point should be on second visual line");
    }

    #[test]
    fn wrapping_and_cursor_positions() {
        let mut t = ta_with("hello world here");
        let area = Rect::new(0, 0, 6, 10); // width 6 -> wraps words
        // desired height counts wrapped lines
        assert!(t.desired_height(area.width) >= 3);

        // Place cursor in "world"
        let world_start = t.text().find("world").unwrap();
        t.set_cursor(world_start + 3);
        let (_x, y) = t.cursor_pos(area).unwrap();
        assert_eq!(y, 1); // world should be on second wrapped line

        // With state and small height, cursor is mapped onto visible row
        let mut state = TextAreaState::default();
        let small_area = Rect::new(0, 0, 6, 1);
        // First call: cursor not visible -> effective scroll ensures it is
        let (_x, y) = t.cursor_pos_with_state(small_area, state).unwrap();
        assert_eq!(y, 0);

        // Render with state to update actual scroll value
        let mut buf = Buffer::empty(small_area);
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&t), small_area, &mut buf, &mut state);
        // After render, state.scroll should be adjusted so cursor row fits
        let effective_lines = t.desired_height(small_area.width);
        assert!(state.scroll < effective_lines);
    }

    #[test]
    fn cursor_pos_with_state_basic_and_scroll_behaviors() {
        // Case 1: No wrapping needed, height fits — scroll ignored, y maps directly.
        let mut t = ta_with("hello world");
        t.set_cursor(3);
        let area = Rect::new(2, 5, 20, 3);
        // Even if an absurd scroll is provided, when content fits the area the
        // effective scroll is 0 and the cursor position matches cursor_pos.
        let bad_state = TextAreaState { scroll: 999 };
        let (x1, y1) = t.cursor_pos(area).unwrap();
        let (x2, y2) = t.cursor_pos_with_state(area, bad_state).unwrap();
        assert_eq!((x2, y2), (x1, y1));

        // Case 2: Cursor below the current window — y should be clamped to the
        // bottom row (area.height - 1) after adjusting effective scroll.
        let mut t = ta_with("one two three four five six");
        // Force wrapping to many visual lines.
        let wrap_width = 4;
        let _ = t.desired_height(wrap_width);
        // Put cursor somewhere near the end so it's definitely below the first window.
        t.set_cursor(t.text().len().saturating_sub(2));
        let small_area = Rect::new(0, 0, wrap_width, 2);
        let state = TextAreaState { scroll: 0 };
        let (_x, y) = t.cursor_pos_with_state(small_area, state).unwrap();
        assert_eq!(y, small_area.y + small_area.height - 1);

        // Case 3: Cursor above the current window — y should be top row (0)
        // when the provided scroll is too large.
        let mut t = ta_with("alpha beta gamma delta epsilon zeta");
        let wrap_width = 5;
        let lines = t.desired_height(wrap_width);
        // Place cursor near start so an excessive scroll moves it to top row.
        t.set_cursor(1);
        let area = Rect::new(0, 0, wrap_width, 3);
        let state = TextAreaState {
            scroll: lines.saturating_mul(2),
        };
        let (_x, y) = t.cursor_pos_with_state(area, state).unwrap();
        assert_eq!(y, area.y);
    }

    #[test]
    fn screen_spans_of_range_single_row() {
        let t = ta_with("xy /model tail");
        let area = Rect::new(2, 1, 40, 3);
        let state = TextAreaState::default();

        // "/model" = bytes 3..9, all on the first visual row.
        let spans = t.screen_spans_of_range(3..9, area, state);
        assert_eq!(spans, vec![Rect::new(5, 1, 6, 1)]);

        // Degenerate ranges yield no spans.
        assert!(t.screen_spans_of_range(4..4, area, state).is_empty());
        assert!(t.screen_spans_of_range(4..999, area, state).is_empty());
    }

    #[test]
    fn screen_spans_of_range_rejects_non_char_boundaries() {
        // 'é' spans bytes 1..3; an endpoint inside it must yield no spans
        // (tolerated like the other invalid-range shapes, never a panic).
        let t = ta_with("héllo");
        let area = Rect::new(0, 0, 10, 2);
        let state = TextAreaState::default();

        assert!(t.screen_spans_of_range(2..5, area, state).is_empty());
        assert!(t.screen_spans_of_range(0..2, area, state).is_empty());
    }

    #[test]
    fn screen_spans_of_range_covers_wrapped_rows() {
        // A token wider than the wrap width must split at the line end and
        // report one span per visual row it lands on.
        let mut t = ta_with("aa /pr-workflow");
        t.show_scrollbar = false;
        let area = Rect::new(0, 0, 8, 4);
        let state = TextAreaState::default();

        // "/pr-workflow" = bytes 3..15, display width 12 > wrap width 8.
        let spans = t.screen_spans_of_range(3..15, area, state);
        assert!(
            spans.len() >= 2,
            "token must cover multiple rows: {spans:?}"
        );
        assert!(spans.iter().all(|r| r.height == 1));
        for pair in spans.windows(2) {
            assert_eq!(pair[1].y, pair[0].y + 1, "rows must be consecutive");
        }
        for r in &spans[1..] {
            assert_eq!(r.x, area.x, "continuation rows start at the left edge");
            assert!(r.right() <= area.x + area.width);
        }
        // Tokens contain no whitespace, so no cell is lost at wrap boundaries:
        // the summed span widths equal the token's display width.
        let total: u16 = spans.iter().map(|r| r.width).sum();
        assert_eq!(total, 12);
    }

    #[test]
    fn screen_spans_of_range_skips_offscreen_rows() {
        // Cursor at the end scrolls the viewport to the tail: the token's
        // first row is above the viewport, but its visible tail must still
        // be reported (screen_position_of on the start would return None).
        let mut t = ta_with("/pr-workflow abc");
        t.show_scrollbar = false;
        let area = Rect::new(0, 0, 8, 2);
        let state = TextAreaState::default();

        let spans = t.screen_spans_of_range(0..12, area, state);
        assert!(!spans.is_empty(), "visible token tail must be reported");
        for r in &spans {
            assert!((area.y..area.y + area.height).contains(&r.y));
            assert!(r.width > 0 && r.right() <= area.x + area.width);
        }
        let total: u16 = spans.iter().map(|r| r.width).sum();
        assert!(
            total < 12,
            "off-screen head must not be reported: {spans:?}"
        );
    }

    #[test]
    fn screen_spans_of_range_uses_display_width() {
        // 2-cell CJK chars: 日本語 (9 bytes, display width 6) at wrap width 4
        // renders as 日本 / 語.
        let mut t = ta_with("日本語");
        t.show_scrollbar = false;
        let area = Rect::new(1, 0, 4, 3);
        let state = TextAreaState::default();

        let spans = t.screen_spans_of_range(0..9, area, state);
        assert_eq!(spans, vec![Rect::new(1, 0, 4, 1), Rect::new(1, 1, 2, 1)]);
    }

    #[test]
    fn screen_spans_of_range_clamps_to_content_edge() {
        // Overflowing content puts the scrollbar up, so content is only
        // `tw = width - 1` columns. Row 0's byte range keeps its trailing
        // wrap spaces ("ab   " measures 5), but the reported span must stop
        // at the content edge (4), never reaching the scrollbar column.
        let mut t = ta_with("ab   cd ef gh");
        t.set_cursor(0);
        let area = Rect::new(0, 0, 5, 2);
        let state = TextAreaState::default();

        let spans = t.screen_spans_of_range(0..5, area, state);
        assert_eq!(spans, vec![Rect::new(0, 0, 4, 1)]);
    }

    #[test]
    fn wrapped_navigation_across_visual_lines() {
        let mut t = ta_with("abcdefghij");
        t.show_scrollbar = false;
        // Force wrapping at width 4: lines -> ["abcd", "efgh", "ij"]
        let _ = t.desired_height(4);

        // From the very start, moving down should go to the start of the next wrapped line (index 4)
        t.set_cursor(0);
        t.move_cursor_down();
        assert_eq!(t.cursor(), 4);

        // Cursor at boundary index 4 should be displayed at start of second wrapped line
        t.set_cursor(4);
        let area = Rect::new(0, 0, 4, 10);
        let (x, y) = t.cursor_pos(area).unwrap();
        assert_eq!((x, y), (0, 1));

        // With state and small height, cursor should be visible at row 0, col 0
        let small_area = Rect::new(0, 0, 4, 1);
        let state = TextAreaState::default();
        let (x, y) = t.cursor_pos_with_state(small_area, state).unwrap();
        assert_eq!((x, y), (0, 0));

        // Place cursor in the middle of the second wrapped line ("efgh"), at 'g'
        t.set_cursor(6);
        // Move up should go to same column on previous wrapped line -> index 2 ('c')
        t.move_cursor_up();
        assert_eq!(t.cursor(), 2);

        // Move down should return to same position on the next wrapped line -> back to index 6 ('g')
        t.move_cursor_down();
        assert_eq!(t.cursor(), 6);

        // Move down again should go to third wrapped line. Target col is 2, but the line has len 2 -> clamp to end
        t.move_cursor_down();
        assert_eq!(t.cursor(), t.text().len());
    }

    #[test]
    fn cursor_pos_with_state_after_movements() {
        let mut t = ta_with("abcdefghij");
        // Wrap width 4 -> visual lines: abcd | efgh | ij
        let _ = t.desired_height(4);
        let area = Rect::new(0, 0, 4, 2);
        let mut state = TextAreaState::default();
        let mut buf = Buffer::empty(area);

        // Start at beginning
        t.set_cursor(0);
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&t), area, &mut buf, &mut state);
        let (x, y) = t.cursor_pos_with_state(area, state).unwrap();
        assert_eq!((x, y), (0, 0));

        // Move down to second visual line; should be at bottom row (row 1) within 2-line viewport
        t.move_cursor_down();
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&t), area, &mut buf, &mut state);
        let (x, y) = t.cursor_pos_with_state(area, state).unwrap();
        assert_eq!((x, y), (0, 1));

        // Move down to third visual line; viewport scrolls and keeps cursor on bottom row
        t.move_cursor_down();
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&t), area, &mut buf, &mut state);
        let (x, y) = t.cursor_pos_with_state(area, state).unwrap();
        assert_eq!((x, y), (0, 1));

        // Move up to second visual line; with current scroll, it appears on top row
        t.move_cursor_up();
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&t), area, &mut buf, &mut state);
        let (x, y) = t.cursor_pos_with_state(area, state).unwrap();
        assert_eq!((x, y), (0, 0));

        // Column preservation across moves: set to col 2 on first line, move down
        t.set_cursor(2);
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&t), area, &mut buf, &mut state);
        let (x0, y0) = t.cursor_pos_with_state(area, state).unwrap();
        assert_eq!((x0, y0), (2, 0));
        t.move_cursor_down();
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&t), area, &mut buf, &mut state);
        let (x1, y1) = t.cursor_pos_with_state(area, state).unwrap();
        assert_eq!((x1, y1), (2, 1));
    }

    #[test]
    fn wrapped_navigation_with_newlines_and_spaces() {
        // Include spaces and an explicit newline to exercise boundaries
        let mut t = ta_with("word1  word2\nword3");
        // Width 6 will wrap "word1  " and then "word2" before the newline
        let _ = t.desired_height(6);

        // Put cursor on the second wrapped line before the newline, at column 1 of "word2"
        let start_word2 = t.text().find("word2").unwrap();
        t.set_cursor(start_word2 + 1);

        // Up should go to first wrapped line, column 1 -> index 1
        t.move_cursor_up();
        assert_eq!(t.cursor(), 1);

        // Down should return to the same visual column on "word2"
        t.move_cursor_down();
        assert_eq!(t.cursor(), start_word2 + 1);

        // Down again should cross the logical newline to the next visual line ("word3"), clamped to its length if needed
        t.move_cursor_down();
        let start_word3 = t.text().find("word3").unwrap();
        assert!(t.cursor() >= start_word3 && t.cursor() <= start_word3 + "word3".len());
    }

    #[test]
    fn wrapped_navigation_with_wide_graphemes() {
        // Four thumbs up, each of display width 2, with width 3 to force wrapping inside grapheme boundaries
        let mut t = ta_with("👍👍👍👍");
        let _ = t.desired_height(3);

        // Put cursor after the second emoji (which should be on first wrapped line)
        t.set_cursor("👍👍".len());

        // Move down should go to the start of the next wrapped line (same column preserved but clamped)
        t.move_cursor_down();
        // We expect to land somewhere within the third emoji or at the start of it
        let pos_after_down = t.cursor();
        assert!(pos_after_down >= "👍👍".len());

        // Moving up should take us back to the original position
        t.move_cursor_up();
        assert_eq!(t.cursor(), "👍👍".len());
    }

    #[test]
    fn wrapped_navigation_with_zwj_graphemes() {
        let grapheme = "👩\u{200D}💻";
        let mut t = ta_with(&format!("{grapheme}{grapheme}{grapheme}"));
        let _ = t.desired_height(4);

        t.set_cursor(grapheme.len() * 2);

        t.move_cursor_down();
        let pos_after_down = t.cursor();
        assert!(pos_after_down >= grapheme.len() * 2);

        t.move_cursor_up();
        assert_eq!(t.cursor(), grapheme.len() * 2);
    }

    #[test]
    fn element_aware_wrap_ranges_preserve_zwj_graphemes() {
        let grapheme = "👩\u{200D}💻";
        let mut t = TextArea::new();
        t.insert_str(&format!("{grapheme}{grapheme}"));
        t.insert_element("raw", ElementKind(0), Some(Line::from("[P]")));

        let ranges = {
            let lines = t.wrapped_lines(2);
            lines.iter().cloned().collect::<Vec<_>>()
        };

        assert_eq!(ranges.len(), 3);
        assert_eq!(&t.text()[ranges[0].clone()], grapheme);
        assert_eq!(&t.text()[ranges[1].clone()], grapheme);
    }

    #[test]
    fn fuzz_textarea_randomized() {
        // Deterministic seed for reproducibility
        // Seed the RNG based on the current day in Pacific Time (PST/PDT). This
        // keeps the fuzz test deterministic within a day while still varying
        // day-to-day to improve coverage.
        let pst_today_seed: u64 = (chrono::Utc::now() - chrono::Duration::hours(8))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp() as u64;
        let mut rng = rand::rngs::StdRng::seed_from_u64(pst_today_seed);

        for _case in 0..500 {
            let mut ta = TextArea::new();
            let mut state = TextAreaState::default();
            // Track element payloads we insert. Payloads use characters '[' and ']' which
            // are not produced by rand_grapheme(), avoiding accidental collisions.
            let mut elem_texts: Vec<String> = Vec::new();
            let mut next_elem_id: usize = 0;
            // Start with a random base string
            let base_len = rng.random_range(0..30);
            let mut base = String::new();
            for _ in 0..base_len {
                base.push_str(&rand_grapheme(&mut rng));
            }
            ta.set_text(&base);
            // Choose a valid char boundary for initial cursor
            let mut boundaries: Vec<usize> = vec![0];
            boundaries.extend(ta.text().char_indices().map(|(i, _)| i).skip(1));
            boundaries.push(ta.text().len());
            let init = boundaries[rng.random_range(0..boundaries.len())];
            ta.set_cursor(init);

            let mut width: u16 = rng.random_range(1..=12);
            let mut height: u16 = rng.random_range(1..=4);

            for _step in 0..60 {
                // Mostly stable width/height, occasionally change
                if rng.random_bool(0.1) {
                    width = rng.random_range(1..=12);
                }
                if rng.random_bool(0.1) {
                    height = rng.random_range(1..=4);
                }

                // Pick an operation
                match rng.random_range(0..18) {
                    0 => {
                        // insert small random string at cursor
                        let len = rng.random_range(0..6);
                        let mut s = String::new();
                        for _ in 0..len {
                            s.push_str(&rand_grapheme(&mut rng));
                        }
                        ta.insert_str(&s);
                    }
                    1 => {
                        // Include mid-grapheme char boundaries so normalization stays exercised.
                        let mut b: Vec<usize> = vec![0];
                        b.extend(ta.text().char_indices().map(|(i, _)| i).skip(1));
                        b.push(ta.text().len());
                        let i1 = rng.random_range(0..b.len());
                        let i2 = rng.random_range(0..b.len());
                        let (start, end) = if b[i1] <= b[i2] {
                            (b[i1], b[i2])
                        } else {
                            (b[i2], b[i1])
                        };
                        let insert_len = rng.random_range(0..=4);
                        let mut s = String::new();
                        for _ in 0..insert_len {
                            s.push_str(&rand_grapheme(&mut rng));
                        }
                        let before = ta.text().len();
                        let atomic_ranges = ta.element_ranges();
                        let plan = ta
                            .text
                            .plan_replace_byte_range(start..end, &s, &atomic_ranges);
                        let normalized_len = plan.replaced_byte_range().len();
                        ta.replace_range(start..end, &s);
                        let after = ta.text().len();
                        assert_eq!(
                            after as isize,
                            before as isize + (s.len() as isize) - (normalized_len as isize)
                        );
                    }
                    2 => ta.delete_backward(rng.random_range(0..=3)),
                    3 => ta.delete_forward(rng.random_range(0..=3)),
                    4 => ta.delete_backward_word(),
                    5 => ta.kill_to_beginning_of_line(),
                    6 => ta.kill_to_end_of_line(),
                    7 => ta.move_cursor_left(),
                    8 => ta.move_cursor_right(),
                    9 => ta.move_cursor_up(),
                    10 => ta.move_cursor_down(),
                    11 => ta.move_cursor_to_beginning_of_line(true),
                    12 => ta.move_cursor_to_end_of_line(true),
                    13 => {
                        // Insert an element with a unique sentinel payload
                        let payload =
                            format!("[[EL#{}:{}]]", next_elem_id, rng.random_range(1000..9999));
                        next_elem_id += 1;
                        ta.insert_element(&payload, ElementKind(0), None);
                        elem_texts.push(payload);
                    }
                    14 => {
                        // Try inserting inside an existing element (should clamp to boundary)
                        if let Some(payload) = elem_texts.choose(&mut rng).cloned()
                            && let Some(start) = ta.text().find(&payload)
                        {
                            let end = start + payload.len();
                            if end - start > 2 {
                                let pos = rng.random_range(start + 1..end - 1);
                                let ins = rand_grapheme(&mut rng);
                                ta.insert_str_at(pos, &ins);
                            }
                        }
                    }
                    15 => {
                        // Replace a range that intersects an element -> whole element should be replaced
                        if let Some(payload) = elem_texts.choose(&mut rng).cloned()
                            && let Some(start) = ta.text().find(&payload)
                        {
                            let end = start + payload.len();
                            // Create an intersecting range [start-δ, end-δ2)
                            let mut s = start.saturating_sub(rng.random_range(0..=2));
                            let mut e = (end + rng.random_range(0..=2)).min(ta.text().len());
                            // Align to char boundaries to satisfy String::replace_range contract
                            let txt = ta.text();
                            while s > 0 && !txt.is_char_boundary(s) {
                                s -= 1;
                            }
                            while e < txt.len() && !txt.is_char_boundary(e) {
                                e += 1;
                            }
                            if s < e {
                                // Small replacement text
                                let mut srep = String::new();
                                for _ in 0..rng.random_range(0..=2) {
                                    srep.push_str(&rand_grapheme(&mut rng));
                                }
                                ta.replace_range(s..e, &srep);
                            }
                        }
                    }
                    16 => {
                        // Try setting the cursor to a position inside an element; it should clamp out
                        if let Some(payload) = elem_texts.choose(&mut rng).cloned()
                            && let Some(start) = ta.text().find(&payload)
                        {
                            let end = start + payload.len();
                            if end - start > 2 {
                                let pos = rng.random_range(start + 1..end - 1);
                                ta.set_cursor(pos);
                            }
                        }
                    }
                    _ => {
                        // Jump to word boundaries
                        if rng.random_bool(0.5) {
                            let p = ta.beginning_of_previous_word();
                            ta.set_cursor(p);
                        } else {
                            let p = ta.end_of_next_word();
                            ta.set_cursor(p);
                        }
                    }
                }

                // Sanity invariants
                assert!(ta.cursor() <= ta.text().len());

                // Element invariants
                for payload in &elem_texts {
                    if let Some(start) = ta.text().find(payload) {
                        let end = start + payload.len();
                        // 1) Text inside elements matches the initially set payload
                        assert_eq!(&ta.text()[start..end], payload);
                        // 2) Cursor is never strictly inside an element
                        let c = ta.cursor();
                        assert!(
                            c <= start || c >= end,
                            "cursor inside element: {start}..{end} at {c}"
                        );
                    }
                }

                // Render and compute cursor positions; ensure they are in-bounds and do not panic
                let area = Rect::new(0, 0, width, height);
                // Stateless render into an area tall enough for all wrapped lines
                let total_lines = ta.desired_height(width);
                let full_area = Rect::new(0, 0, width, total_lines.max(1));
                let mut buf = Buffer::empty(full_area);
                ratatui::widgets::WidgetRef::render_ref(&(&ta), full_area, &mut buf);

                // cursor_pos: x must be within width when present
                let _ = ta.cursor_pos(area);

                // cursor_pos_with_state: always within viewport rows
                let (_x, _y) = ta
                    .cursor_pos_with_state(area, state)
                    .unwrap_or((area.x, area.y));

                // Stateful render should not panic, and updates scroll
                let mut sbuf = Buffer::empty(area);
                ratatui::widgets::StatefulWidgetRef::render_ref(
                    &(&ta),
                    area,
                    &mut sbuf,
                    &mut state,
                );

                // After wrapping, desired height equals the number of lines we would render without scroll
                let total_lines = total_lines as usize;
                // state.scroll must not exceed total_lines when content fits within area height
                if (height as usize) >= total_lines {
                    assert_eq!(state.scroll, 0);
                }
            }
        }
    }

    // ── Mouse M1: Screen→Buffer mapping tests ──

    #[test]
    fn buffer_pos_at_screen_plain_text_start() {
        let t = ta_with("hello");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Click at column 0 → pos 0
        assert_eq!(t.buffer_pos_at_screen(0, 0, area, state), Some(0));
    }

    #[test]
    fn buffer_pos_at_screen_plain_text_middle() {
        let t = ta_with("hello");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Click at column 3 → pos 3
        assert_eq!(t.buffer_pos_at_screen(3, 0, area, state), Some(3));
    }

    #[test]
    fn buffer_pos_at_screen_past_end_of_line() {
        let t = ta_with("hello");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Click at column 10, line only has 5 chars → snap to end of text
        assert_eq!(t.buffer_pos_at_screen(10, 0, area, state), Some(5));
    }

    #[test]
    fn buffer_pos_at_screen_below_text() {
        let t = ta_with("hello");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Click on row 3, text only occupies row 0 → end of text
        assert_eq!(t.buffer_pos_at_screen(0, 3, area, state), Some(5));
    }

    #[test]
    fn buffer_pos_at_screen_outside_area() {
        let t = ta_with("hello");
        let area = Rect::new(5, 5, 20, 5);
        let state = TextAreaState::default();
        // Click at (0, 0) which is outside area starting at (5, 5)
        assert_eq!(t.buffer_pos_at_screen(0, 0, area, state), None);
        // Click at (4, 5) — just left of area
        assert_eq!(t.buffer_pos_at_screen(4, 5, area, state), None);
        // Click at (5, 4) — just above area
        assert_eq!(t.buffer_pos_at_screen(5, 4, area, state), None);
    }

    #[test]
    fn buffer_pos_at_screen_with_area_offset() {
        let t = ta_with("hello");
        let area = Rect::new(10, 5, 20, 5);
        let state = TextAreaState::default();
        // Click at screen (13, 5) = column 3 within the area → pos 3
        assert_eq!(t.buffer_pos_at_screen(13, 5, area, state), Some(3));
    }

    #[test]
    fn buffer_pos_at_screen_multiline() {
        let t = ta_with("hello\nworld");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Click on row 0, col 2 → "hello" pos 2
        assert_eq!(t.buffer_pos_at_screen(2, 0, area, state), Some(2));
        // Click on row 1, col 1 → "world" pos 6+1 = 7
        assert_eq!(t.buffer_pos_at_screen(1, 1, area, state), Some(7));
    }

    #[test]
    fn buffer_pos_at_screen_wrapped_text() {
        // "abcdefghij" at width 5 wraps into "abcde" (0..5) and "fghij" (5..10)
        let t = ta_with("abcdefghij");
        let area = Rect::new(0, 0, 5, 5);
        let state = TextAreaState::default();
        // Click on row 0, col 2 → pos 2
        assert_eq!(t.buffer_pos_at_screen(2, 0, area, state), Some(2));
        // Click on row 1, col 0 → pos 5 (start of second wrapped line)
        assert_eq!(t.buffer_pos_at_screen(0, 1, area, state), Some(5));
        // Click on row 1, col 3 → pos 8
        assert_eq!(t.buffer_pos_at_screen(3, 1, area, state), Some(8));
    }

    #[test]
    fn buffer_pos_at_screen_scrolled() {
        // 3 lines, area height 2 → first line scrolled off when cursor is at end
        let mut t = ta_with("aaa\nbbb\nccc");
        t.set_cursor(t.text().len()); // cursor at end → scroll to show last lines
        let area = Rect::new(0, 0, 20, 2);
        let mut state = TextAreaState::default();
        // Render to compute scroll
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&t), area, &mut buf, &mut state);
        // state.scroll should be 1 (skipping "aaa")
        assert_eq!(state.scroll, 1);
        // Click row 0 = visual row 0 = wrapped line 1 ("bbb"), col 1 → pos 5
        assert_eq!(t.buffer_pos_at_screen(1, 0, area, state), Some(5));
        // Click row 1 = visual row 1 = wrapped line 2 ("ccc"), col 2 → pos 10
        assert_eq!(t.buffer_pos_at_screen(2, 1, area, state), Some(10));
    }

    #[test]
    fn buffer_pos_at_screen_wide_unicode() {
        // "a🦀b" — 🦀 is 2 columns wide (4 bytes)
        let t = ta_with("a🦀b");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // col 0 → 'a' at pos 0
        assert_eq!(t.buffer_pos_at_screen(0, 0, area, state), Some(0));
        // col 1 → first column of 🦀 → pos 1
        assert_eq!(t.buffer_pos_at_screen(1, 0, area, state), Some(1));
        // col 2 → second column of 🦀 → still pos 1 (within the 2-wide grapheme;
        // display_col_to_buffer_pos snaps to start of grapheme since target_col < width_so_far)
        // Actually: width_so_far after 'a' is 1, then 🦀 adds 2 → width_so_far=3 > target_col=2
        // → returns pos 1 (start of 🦀)
        assert_eq!(t.buffer_pos_at_screen(2, 0, area, state), Some(1));
        // col 3 → 'b' at pos 5 (1 + 4 bytes for 🦀)
        assert_eq!(t.buffer_pos_at_screen(3, 0, area, state), Some(5));
    }

    #[test]
    fn buffer_pos_at_screen_element_with_display() {
        // "ab" + element(buffer="raw_text", display="[X]") + "cd"
        // Display: "ab[X]cd" — element is at display cols 2..5
        let mut t = TextArea::new();
        t.insert_str("ab");
        let display = Line::from("[X]");
        t.insert_element("raw_text", ElementKind(0), Some(display));
        t.insert_str("cd");
        // Buffer: "abraw_textcd", element range 2..10
        assert_eq!(t.text(), "abraw_textcd");

        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();

        // col 0 → 'a' at pos 0
        assert_eq!(t.buffer_pos_at_screen(0, 0, area, state), Some(0));
        // col 1 → 'b' at pos 1
        assert_eq!(t.buffer_pos_at_screen(1, 0, area, state), Some(1));
        // col 2 → start of element display "[X]" → snap to element start (pos 2)
        assert_eq!(t.buffer_pos_at_screen(2, 0, area, state), Some(2));
        // col 3 → middle of element display → snap to nearest boundary
        // display width = 3, dist_start = 1, dist_end = 2 → snap to start (pos 2)
        assert_eq!(t.buffer_pos_at_screen(3, 0, area, state), Some(2));
        // col 4 → near end of element display → snap to end (pos 10)
        // dist_start = 2, dist_end = 1 → snap to end
        assert_eq!(t.buffer_pos_at_screen(4, 0, area, state), Some(10));
        // col 5 → 'c' at pos 10
        assert_eq!(t.buffer_pos_at_screen(5, 0, area, state), Some(10));
        // col 6 → 'd' at pos 11
        assert_eq!(t.buffer_pos_at_screen(6, 0, area, state), Some(11));
    }

    #[test]
    fn element_at_screen_hit_and_miss() {
        let mut t = TextArea::new();
        t.insert_str("ab");
        let display = Line::from("[File]");
        let id = t.insert_element("file.rs", ElementKind(1), Some(display));
        t.insert_str("cd");
        // Display: "ab[File]cd"

        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();

        // Click on 'a' (col 0) → no element
        assert!(t.element_at_screen(0, 0, area, state).is_none());
        // Click on 'b' (col 1) → no element
        assert!(t.element_at_screen(1, 0, area, state).is_none());
        // Click on element display (col 2) → element (snaps to start, pos 2 = element start)
        let elem = t.element_at_screen(2, 0, area, state);
        assert!(elem.is_some());
        assert_eq!(elem.unwrap().id, id);
        // Click on element display (col 3) → still the element
        assert_eq!(t.element_at_screen(3, 0, area, state).unwrap().id, id);
        // Click past element (col 8) → 'c' or 'd', no element
        assert!(t.element_at_screen(8, 0, area, state).is_none());
    }

    #[test]
    fn buffer_pos_at_screen_empty_textarea() {
        let t = TextArea::new();
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Click on empty textarea → pos 0
        assert_eq!(t.buffer_pos_at_screen(0, 0, area, state), Some(0));
        // Click at col 5 → still pos 0 (end of empty text)
        assert_eq!(t.buffer_pos_at_screen(5, 0, area, state), Some(0));
    }

    // ── Mouse M2: Selection state + rendering tests ──

    #[test]
    fn selection_range_normalizes_anchor_head() {
        let mut t = ta_with("hello world");
        // anchor > head → range should be normalized to start..end
        t.set_selection(8, 3);
        let range = t.selection_range().unwrap();
        assert_eq!(range, 3..8);
    }

    #[test]
    fn selection_range_anchor_equals_head_is_none() {
        let mut t = ta_with("hello");
        t.set_selection(3, 3);
        assert!(t.selection_range().is_none());
    }

    #[test]
    fn selected_text_returns_buffer_substring() {
        let mut t = ta_with("hello world");
        t.set_selection(6, 11);
        assert_eq!(t.selected_text().unwrap(), "world");
    }

    #[test]
    fn selection_expands_to_element_boundaries() {
        let mut t = TextArea::new();
        t.insert_str("ab");
        t.insert_element("element_text", ElementKind(0), None);
        t.insert_str("cd");
        // Buffer: "abelement_textcd", element range 2..14
        assert_eq!(t.text(), "abelement_textcd");

        // Select only part of the element (bytes 5..10) → should expand to 2..14
        t.set_selection(5, 10);
        let range = t.selection_range().unwrap();
        assert_eq!(range.start, 2); // expanded to element start
        assert_eq!(range.end, 14); // expanded to element end
        assert_eq!(t.selected_text().unwrap(), "element_text");
    }

    #[test]
    fn clear_selection_clears() {
        let mut t = ta_with("hello");
        t.set_selection(0, 3);
        assert!(t.selection_range().is_some());
        t.clear_selection();
        assert!(t.selection_range().is_none());
    }

    #[test]
    fn take_clipboard_returns_and_clears() {
        let mut t = TextArea::new();
        t.set_clipboard_text("copied text".to_string());
        let text = t.take_clipboard();
        assert_eq!(text, Some("copied text".to_string()));
        // Second take returns None
        assert_eq!(t.take_clipboard(), None);
    }

    #[test]
    fn no_selection_returns_none() {
        let t = ta_with("hello");
        assert!(t.selection_range().is_none());
        assert!(t.selected_text().is_none());
    }

    #[test]
    fn selection_rendering_applies_default_selection_style() {
        let mut t = ta_with("hello");
        t.set_selection(1, 4); // select "ell"

        let area = Rect::new(0, 0, 10, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&t), area, &mut buf);

        let default_bg = Color::Rgb(49, 62, 115);
        let default_fg = Color::Rgb(192, 202, 245);
        // Cells 1, 2, 3 should have the default selection bg + fg
        for col in 1..4u16 {
            let cell = &buf[(col, 0)];
            assert_eq!(
                cell.bg, default_bg,
                "cell at col {col} should have default selection bg"
            );
            assert_eq!(
                cell.fg, default_fg,
                "cell at col {col} should have default selection fg"
            );
        }
        // Cell 0 ('h') and cell 4 ('o') should NOT have selection bg
        assert_ne!(buf[(0, 0)].bg, default_bg);
        assert_ne!(buf[(4, 0)].bg, default_bg);
    }

    // ── Phase 1: Undo/Redo plumbing tests ──

    #[test]
    fn undo_insert_chars_one_at_a_time() {
        let mut ta = TextArea::new();
        ta.insert_str("a");
        ta.insert_str("b");
        ta.insert_str("c");
        assert_eq!(ta.text(), "abc");
        assert_eq!(ta.cursor(), 3);

        // Phase 2: consecutive single-char inserts are batched into 1 undo step.
        assert!(ta.undo());
        assert_eq!(ta.text(), "");
        assert_eq!(ta.cursor(), 0);

        // Nothing left to undo.
        assert!(!ta.undo());
    }

    #[test]
    fn redo_after_undo_restores() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        ta.insert_str(" ");
        ta.insert_str("world");
        assert_eq!(ta.text(), "hello world");

        // With word boundary batching: "hello" / " " / "world" = 3 steps.
        ta.undo(); // undo "world"
        assert_eq!(ta.text(), "hello ");
        ta.undo(); // undo " "
        assert_eq!(ta.text(), "hello");
        ta.undo(); // undo "hello"
        assert_eq!(ta.text(), "");

        // Redo walks forward.
        ta.redo();
        assert_eq!(ta.text(), "hello");
        ta.redo();
        assert_eq!(ta.text(), "hello ");
        ta.redo();
        assert_eq!(ta.text(), "hello world");
        assert_eq!(ta.cursor(), 11);
    }

    #[test]
    fn undo_via_super_modifier() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        assert_eq!(ta.text(), "hello");

        // Cmd+Z (SUPER) triggers undo.
        ta.input(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::SUPER));
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn redo_via_super_modifier() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        ta.undo();
        assert_eq!(ta.text(), "");

        // Cmd+Shift+Z (SUPER, reported as uppercase Z) triggers redo.
        ta.input(KeyEvent::new(
            KeyCode::Char('Z'),
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        ));
        assert_eq!(ta.text(), "hello");
    }

    #[test]
    fn redo_cleared_by_new_mutation() {
        let mut ta = TextArea::new();
        ta.insert_str("abc");

        ta.undo(); // undo "abc" → ""
        assert_eq!(ta.text(), "");
        assert!(ta.can_redo());

        ta.insert_str("x"); // new mutation clears redo
        assert!(!ta.can_redo());
        assert_eq!(ta.text(), "x");
    }

    #[test]
    fn undo_delete_backward_restores_char() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        ta.delete_backward(1); // "hell"
        assert_eq!(ta.text(), "hell");

        ta.undo(); // undo delete → "hello"
        assert_eq!(ta.text(), "hello");
        assert_eq!(ta.cursor(), 5);
    }

    #[test]
    fn undo_redo_preserves_cursor() {
        let mut ta = TextArea::new();
        ta.insert_str("abc");
        // cursor is at 3
        ta.set_cursor(1);
        ta.insert_str("X"); // "aXbc", cursor at 2
        assert_eq!(ta.text(), "aXbc");
        assert_eq!(ta.cursor(), 2);

        ta.undo(); // undo insert "X" → "abc", cursor at 1
        assert_eq!(ta.text(), "abc");
        assert_eq!(ta.cursor(), 1);

        ta.redo(); // redo → "aXbc", cursor at 2
        assert_eq!(ta.text(), "aXbc");
        assert_eq!(ta.cursor(), 2);
    }

    #[test]
    fn can_undo_can_redo_reflect_state() {
        let mut ta = TextArea::new();
        assert!(!ta.can_undo());
        assert!(!ta.can_redo());

        ta.insert_str("a");
        assert!(ta.can_undo());
        assert!(!ta.can_redo());

        ta.undo();
        assert!(!ta.can_undo());
        assert!(ta.can_redo());

        ta.redo();
        assert!(ta.can_undo());
        assert!(!ta.can_redo());
    }

    #[test]
    fn undo_stack_depth_capped() {
        let mut ta = TextArea::new();
        // Override max_depth for testing.
        ta.undo.max_depth = 5;

        // Use set_text (Replace — always discrete) to force separate undo steps.
        for i in 0..10 {
            ta.set_text(&format!("v{i}"));
        }
        assert_eq!(ta.text(), "v9");
        // Stack should be capped at 5.
        assert_eq!(ta.undo.stack.len(), 5);

        // We can undo at most 5 times.
        let mut count = 0;
        while ta.undo() {
            count += 1;
        }
        assert_eq!(count, 5);
        // We've undone 5 set_text calls, landing on the 5th oldest state.
        assert_eq!(ta.text(), "v4");
    }

    #[test]
    fn undo_set_text_restores_previous() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        ta.set_text("new");
        assert_eq!(ta.text(), "new");

        ta.undo(); // undo set_text → "hello"
        assert_eq!(ta.text(), "hello");
    }

    #[test]
    fn undo_redo_multiple_round_trips() {
        let mut ta = TextArea::new();
        // Use separate insert kinds so they don't batch together.
        ta.insert_str("hello");
        ta.delete_backward(2); // "hel" — kind changes Insert→Delete, new undo step
        assert_eq!(ta.text(), "hel");

        ta.undo(); // undo delete → "hello"
        assert_eq!(ta.text(), "hello");

        ta.undo(); // undo insert → ""
        assert_eq!(ta.text(), "");

        ta.redo(); // redo insert → "hello"
        assert_eq!(ta.text(), "hello");

        ta.redo(); // redo delete → "hel"
        assert_eq!(ta.text(), "hel");

        // undo one, insert new → redo cleared
        ta.undo(); // undo delete → "hello"
        assert_eq!(ta.text(), "hello");
        ta.insert_str("z"); // "helloz" — new branch
        assert_eq!(ta.text(), "helloz");
        assert!(!ta.can_redo());

        // undo "z" — but wait, "z" extends the "hello" batch (same kind, consecutive cursor)?
        // No: undo reset last_kind=None, so "z" is a fresh group.
        ta.undo();
        assert_eq!(ta.text(), "hello");
    }

    // ── Phase 2: Batching tests ──

    #[test]
    fn batch_consecutive_inserts_into_one_undo_step() {
        // Typing "hello" char by char → batched into 1 undo step.
        let mut ta = TextArea::new();
        ta.insert_str("h");
        ta.insert_str("e");
        ta.insert_str("l");
        ta.insert_str("l");
        ta.insert_str("o");
        assert_eq!(ta.text(), "hello");
        assert_eq!(ta.undo.stack.len(), 1); // single checkpoint

        ta.undo();
        assert_eq!(ta.text(), "");
        assert!(!ta.undo());
    }

    #[test]
    fn multi_grapheme_delete_calls_are_single_undo_steps() {
        for forward in [false, true] {
            let mut ta = ta_with("hello");
            if forward {
                ta.set_cursor(0);
                ta.delete_forward(2);
                assert_eq!(ta.text(), "llo");
            } else {
                ta.delete_backward(2);
                assert_eq!(ta.text(), "hel");
            }
            assert!(ta.undo());
            assert_eq!(ta.text(), "hello");
        }
    }

    #[test]
    fn multi_count_deletes_cross_atomic_element_boundaries() {
        let mut backward = TextArea::new();
        backward.insert_str("a");
        backward.insert_element("TOKEN", ElementKind(1), None);
        backward.insert_str("b");
        backward.delete_backward(2);
        assert_eq!(backward.text(), "a");
        assert!(backward.elements().is_empty());
        assert!(backward.undo());
        assert_eq!(backward.text(), "aTOKENb");

        let mut forward = TextArea::new();
        forward.insert_str("a");
        forward.insert_element("TOKEN", ElementKind(1), None);
        forward.insert_str("b");
        forward.set_cursor(0);
        forward.delete_forward(2);
        assert_eq!(forward.text(), "b");
        assert!(forward.elements().is_empty());
        assert!(forward.undo());
        assert_eq!(forward.text(), "aTOKENb");
    }

    #[test]
    fn batch_consecutive_deletes_into_one_undo_step() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        // 5 backspaces — all Delete kind, consecutive cursor
        ta.delete_backward(1); // o
        ta.delete_backward(1); // l
        ta.delete_backward(1); // l
        ta.delete_backward(1); // e
        ta.delete_backward(1); // h
        assert_eq!(ta.text(), "");

        // 2 undo steps: 1 for insert batch, 1 for delete batch
        ta.undo(); // undo all deletes
        assert_eq!(ta.text(), "hello");

        ta.undo(); // undo insert
        assert_eq!(ta.text(), "");
        assert!(!ta.undo());
    }

    #[test]
    fn kind_change_breaks_batch() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        ta.delete_backward(1); // "hell" — kind changes → new step
        assert_eq!(ta.text(), "hell");

        // 2 undo steps
        ta.undo(); // undo delete
        assert_eq!(ta.text(), "hello");
        ta.undo(); // undo insert
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn cursor_jump_breaks_insert_batch() {
        let mut ta = TextArea::new();
        ta.insert_str("he"); // cursor at 2
        ta.set_cursor(0); // move cursor to 0 (no mutation, just movement)
        ta.insert_str("X"); // cursor was at 0, last_cursor was 2 → jump → new step
        assert_eq!(ta.text(), "Xhe");

        // 2 undo steps
        ta.undo(); // undo "X"
        assert_eq!(ta.text(), "he");
        ta.undo(); // undo "he"
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn kill_always_discrete() {
        let mut ta = TextArea::new();
        ta.insert_str("hello world");
        ta.set_cursor(5);
        ta.kill_to_end_of_line(); // kills " world"
        assert_eq!(ta.text(), "hello");
        ta.kill_to_end_of_line(); // kills nothing (already at EOL with no newline... wait)

        // Second kill at EOL does nothing (text.len() == cursor_pos).
        // So only 1 kill undo step.
        ta.undo(); // undo kill
        assert_eq!(ta.text(), "hello world");
    }

    #[test]
    fn kill_consecutive_each_own_step() {
        // Two kill operations back-to-back should be separate undo steps.
        let mut ta = TextArea::new();
        ta.insert_str("aaa bbb ccc");
        ta.set_cursor(7); // after "aaa bbb"
        ta.kill_to_end_of_line(); // kills " ccc" → "aaa bbb"
        assert_eq!(ta.text(), "aaa bbb");
        ta.set_cursor(3);
        ta.kill_to_end_of_line(); // kills " bbb" → "aaa"
        assert_eq!(ta.text(), "aaa");

        ta.undo(); // undo second kill
        assert_eq!(ta.text(), "aaa bbb");
        ta.undo(); // undo first kill
        assert_eq!(ta.text(), "aaa bbb ccc");
    }

    #[test]
    fn insert_str_multi_char_is_one_step() {
        // A single insert_str("hello world") call is 1 undo step.
        let mut ta = TextArea::new();
        ta.insert_str("hello world");
        assert_eq!(ta.undo.stack.len(), 1);

        ta.undo();
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn set_text_always_discrete() {
        let mut ta = TextArea::new();
        ta.set_text("first");
        ta.set_text("second");
        assert_eq!(ta.text(), "second");

        // Each set_text is its own undo step (Replace is always discrete)
        ta.undo();
        assert_eq!(ta.text(), "first");
        ta.undo();
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn insert_then_undo_then_insert_fresh_batch() {
        // After undo, last_kind is reset, so new inserts start a fresh batch.
        let mut ta = TextArea::new();
        ta.insert_str("ab");
        ta.undo(); // → ""
        ta.insert_str("cd");
        ta.insert_str("ef"); // should batch with "cd"
        assert_eq!(ta.text(), "cdef");

        ta.undo(); // undo "cdef" batch
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn delete_forward_batches() {
        let mut ta = TextArea::new();
        ta.insert_str("abcde");
        ta.set_cursor(0);
        ta.delete_forward(1); // "bcde"
        ta.delete_forward(1); // "cde"
        ta.delete_forward(1); // "de"
        assert_eq!(ta.text(), "de");

        // All delete_forward calls batch into 1 step
        ta.undo(); // undo all deletes
        assert_eq!(ta.text(), "abcde");
    }

    #[test]
    fn word_boundary_breaks_insert_batch() {
        // Typing "foo bar" char by char: ws↔non-ws transitions create checkpoints.
        let mut ta = TextArea::new();
        // "foo" — all non-ws, batches into 1 step
        ta.insert_str("f");
        ta.insert_str("o");
        ta.insert_str("o");
        // " " — whitespace, class change → new step
        ta.insert_str(" ");
        // "bar" — non-ws, class change → new step
        ta.insert_str("b");
        ta.insert_str("a");
        ta.insert_str("r");
        assert_eq!(ta.text(), "foo bar");

        ta.undo(); // undo "bar"
        assert_eq!(ta.text(), "foo ");
        ta.undo(); // undo " "
        assert_eq!(ta.text(), "foo");
        ta.undo(); // undo "foo"
        assert_eq!(ta.text(), "");
        assert!(!ta.undo());
    }

    #[test]
    fn word_boundary_whitespace_runs_batch_together() {
        // Multiple consecutive whitespace chars batch into one step.
        let mut ta = TextArea::new();
        ta.insert_str("a");
        ta.insert_str(" ");
        ta.insert_str(" ");
        ta.insert_str(" ");
        ta.insert_str("b");
        assert_eq!(ta.text(), "a   b");

        ta.undo(); // undo "b"
        assert_eq!(ta.text(), "a   ");
        ta.undo(); // undo "   "
        assert_eq!(ta.text(), "a");
        ta.undo(); // undo "a"
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn word_boundary_newlines_are_whitespace() {
        // Newlines are whitespace — they batch with spaces, break from words.
        let mut ta = TextArea::new();
        ta.insert_str("foo");
        ta.insert_str("\n");
        ta.insert_str("\n");
        ta.insert_str(" ");
        ta.insert_str(" ");
        ta.insert_str("bar");
        assert_eq!(ta.text(), "foo\n\n  bar");

        ta.undo(); // undo "bar"
        assert_eq!(ta.text(), "foo\n\n  ");
        ta.undo(); // undo "\n\n  " (all whitespace batched)
        assert_eq!(ta.text(), "foo");
        ta.undo(); // undo "foo"
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn word_boundary_multi_char_insert_str_is_one_step() {
        // A single insert_str("hello world") call is still 1 undo step,
        // even though it contains a space. Boundary check only applies
        // between separate insert_str calls.
        let mut ta = TextArea::new();
        ta.insert_str("hello world");
        assert_eq!(ta.undo.stack.len(), 1);

        ta.undo();
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn word_boundary_after_undo_starts_fresh() {
        // After undo, last_kind is reset — no stale boundary check.
        let mut ta = TextArea::new();
        ta.insert_str("abc");
        ta.insert_str(" ");
        ta.undo(); // undo " "
        assert_eq!(ta.text(), "abc");
        // Now insert non-ws — should start a fresh batch, no boundary check against stale state.
        ta.insert_str("d");
        ta.insert_str("e");
        assert_eq!(ta.text(), "abcde");

        ta.undo(); // undo "de"
        assert_eq!(ta.text(), "abc");
    }

    #[test]
    fn element_insert_always_discrete() {
        let mut ta = TextArea::new();
        ta.insert_str("hi ");
        ta.insert_element("@file.rs", ElementKind(0), None);
        // Element should be its own undo step, not batched with the insert.
        assert_eq!(ta.text(), "hi @file.rs");

        ta.undo(); // undo element
        assert_eq!(ta.text(), "hi ");
        assert!(ta.elements().is_empty());

        ta.undo(); // undo "hi "
        assert_eq!(ta.text(), "");
    }

    // ── Phase 3: Element undo/redo tests ──

    #[test]
    fn undo_insert_element_redo_preserves_element_id() {
        let mut ta = TextArea::new();
        let id = ta.insert_element("@foo", ElementKind(1), None);
        assert_eq!(ta.elements().len(), 1);
        assert_eq!(ta.elements()[0].id, id);
        assert_eq!(ta.cursor(), "@foo".len());

        ta.undo(); // remove element
        assert!(ta.elements().is_empty());
        assert_eq!(ta.text(), "");
        assert_eq!(ta.cursor(), 0);

        ta.redo(); // restore element — same ElementId
        assert_eq!(ta.elements().len(), 1);
        assert_eq!(ta.elements()[0].id, id);
        assert_eq!(ta.text(), "@foo");
        assert_eq!(ta.cursor(), "@foo".len());
    }

    #[test]
    fn undo_redo_zero_length_element_preserves_metadata_and_cursor() {
        let mut ta = TextArea::new();
        let id = ta.insert_element("", ElementKind(9), None);
        assert_eq!(ta.cursor(), 0);
        assert_eq!(ta.elements()[0].range, 0..0);

        assert!(ta.undo());
        assert!(ta.elements().is_empty());
        assert_eq!(ta.cursor(), 0);

        assert!(ta.redo());
        assert_eq!(ta.elements().len(), 1);
        assert_eq!(ta.elements()[0].id, id);
        assert_eq!(ta.elements()[0].range, 0..0);
        assert_eq!(ta.cursor(), 0);
    }

    #[test]
    fn undo_replace_range_with_element_restores_original() {
        let mut ta = TextArea::new();
        ta.insert_str("hello @foo world");
        // Replace "@foo" (6..10) with an element
        let id = ta.replace_range_with_element(6..10, "@bar.rs", ElementKind(2), None);
        assert_eq!(ta.text(), "hello @bar.rs world");
        assert_eq!(ta.elements().len(), 1);
        assert_eq!(ta.elements()[0].id, id);

        ta.undo(); // undo replace → original text, no elements
        assert_eq!(ta.text(), "hello @foo world");
        assert!(ta.elements().is_empty());

        ta.redo(); // redo → element back
        assert_eq!(ta.text(), "hello @bar.rs world");
        assert_eq!(ta.elements().len(), 1);
        assert_eq!(ta.elements()[0].id, id);
    }

    #[test]
    fn undo_element_display_preserved() {
        let mut ta = TextArea::new();
        let display = Line::from(vec![
            ratatui::text::Span::styled("[", Style::default().fg(Color::Green)),
            ratatui::text::Span::raw("file.rs"),
            ratatui::text::Span::styled("]", Style::default().fg(Color::Green)),
        ]);
        let id = ta.insert_element("@file.rs", ElementKind(0), Some(display));
        assert!(ta.elements()[0].display.is_some());

        ta.undo();
        assert!(ta.elements().is_empty());

        ta.redo();
        assert_eq!(ta.elements().len(), 1);
        assert_eq!(ta.elements()[0].id, id);
        // Display should be restored from the snapshot clone
        let restored = ta.elements()[0].display.as_ref().unwrap();
        assert_eq!(restored.spans.len(), 3);
        let text: String = restored.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "[file.rs]");
    }

    #[test]
    fn next_element_id_never_decreases_after_undo() {
        let mut ta = TextArea::new();
        let id1 = ta.insert_element("a", ElementKind(0), None);
        let id2 = ta.insert_element("b", ElementKind(0), None);

        ta.undo(); // undo element "b"
        ta.undo(); // undo element "a"
        assert!(ta.elements().is_empty());

        // New element after undo should get a fresh ID, never reuse id1 or id2.
        let id3 = ta.insert_element("c", ElementKind(0), None);
        assert_ne!(id3, id1);
        assert_ne!(id3, id2);
        // IDs are monotonically increasing
        assert!(id3.0 > id2.0);
    }

    #[test]
    fn backspace_on_element_undo_restores_element() {
        let mut ta = TextArea::new();
        ta.insert_str("before ");
        let id = ta.insert_element("[paste]", ElementKind(0), None);
        assert_eq!(ta.text(), "before [paste]");
        assert_eq!(ta.cursor(), 14);

        // Backspace at element end → deletes entire element atomically
        ta.delete_backward(1);
        assert_eq!(ta.text(), "before ");
        assert!(ta.elements().is_empty());

        // Undo → element restored with same ID
        ta.undo();
        assert_eq!(ta.text(), "before [paste]");
        assert_eq!(ta.elements().len(), 1);
        assert_eq!(ta.elements()[0].id, id);
        assert_eq!(ta.elements()[0].range, 7..14);
    }

    // ── Phase 4: Undo group tests ──

    #[test]
    fn undo_group_collapses_multiple_mutations() {
        // Autocomplete scenario: replace trigger + insert trailing space = 1 undo step.
        let mut ta = TextArea::new();
        ta.insert_str("hello @fo");
        assert_eq!(ta.text(), "hello @fo");

        ta.begin_undo_group();
        ta.replace_range_with_element(6..9, "@foo.rs", ElementKind(1), None);
        ta.insert_str(" "); // trailing space after element
        ta.end_undo_group();

        assert_eq!(ta.text(), "hello @foo.rs ");
        assert_eq!(ta.elements().len(), 1);

        // Single undo undoes the entire autocomplete operation.
        ta.undo();
        assert_eq!(ta.text(), "hello @fo");
        assert!(ta.elements().is_empty());
    }

    #[test]
    fn cancel_undo_group_restores_original() {
        // Line-select cancel: enter → N live-updates → cancel = 0 undo entries.
        let mut ta = TextArea::new();
        ta.insert_str("original");
        let stack_before = ta.undo.stack.len();

        ta.begin_undo_group();
        ta.set_text("modified once");
        ta.set_text("modified twice");
        ta.cancel_undo_group();

        // State restored to before the group.
        assert_eq!(ta.text(), "original");
        // No new undo entries created by the group.
        assert_eq!(ta.undo.stack.len(), stack_before);
    }

    #[test]
    fn nested_groups_only_outermost_pushes() {
        let mut ta = TextArea::new();
        ta.insert_str("start");

        ta.begin_undo_group(); // depth 1
        ta.insert_str(" A");
        ta.begin_undo_group(); // depth 2
        ta.insert_str(" B");
        ta.end_undo_group(); // depth 1 (inner end — no push)
        assert_eq!(ta.text(), "start A B");
        ta.insert_str(" C");
        ta.end_undo_group(); // depth 0 (outermost end — push)

        assert_eq!(ta.text(), "start A B C");

        // Single undo undoes everything in the group.
        ta.undo();
        assert_eq!(ta.text(), "start");
    }

    #[test]
    fn group_with_no_mutations_creates_no_entry() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        let stack_len = ta.undo.stack.len();

        ta.begin_undo_group();
        // No mutations inside the group.
        ta.end_undo_group();

        // Stack unchanged — no empty undo entry created.
        assert_eq!(ta.undo.stack.len(), stack_len);
    }

    #[test]
    fn redo_cleared_by_end_undo_group() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        ta.undo(); // undo → ""
        assert!(ta.can_redo());

        ta.begin_undo_group();
        ta.insert_str("world");
        ta.end_undo_group();

        // Redo from the previous undo should be cleared.
        assert!(!ta.can_redo());
        assert_eq!(ta.text(), "world");
    }

    #[test]
    fn cancel_nested_group_restores_outermost() {
        // Even if deeply nested, cancel restores to the outermost group snapshot.
        let mut ta = TextArea::new();
        ta.insert_str("original");

        ta.begin_undo_group();
        ta.insert_str(" X");
        ta.begin_undo_group();
        ta.insert_str(" Y");
        // Cancel from inner level — should still restore to outermost snapshot.
        ta.cancel_undo_group();

        assert_eq!(ta.text(), "original");
        assert_eq!(ta.undo.group_depth, 0);
    }

    #[test]
    fn mutations_after_group_work_normally() {
        // After a group ends, normal batching resumes.
        let mut ta = TextArea::new();

        ta.begin_undo_group();
        ta.insert_str("grouped");
        ta.end_undo_group();

        // Normal insert after group — should be its own batch.
        ta.insert_str("X");
        ta.insert_str("Y"); // batches with X

        ta.undo(); // undo "XY"
        assert_eq!(ta.text(), "grouped");

        ta.undo(); // undo group
        assert_eq!(ta.text(), "");
    }

    // ── M3: Click-to-place cursor tests ──

    /// Helper to create a MouseEvent for testing.
    fn mouse_down(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn mouse_up(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn click_places_cursor_at_correct_position() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click at column 3 → cursor at byte 3
        let action = ta.handle_mouse(mouse_down(3, 0), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        assert_eq!(ta.cursor(), 3);

        // Click at column 0 → cursor at byte 0
        let action = ta.handle_mouse(mouse_down(0, 0), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        assert_eq!(ta.cursor(), 0);
    }

    #[test]
    fn click_on_element_returns_clicked_element() {
        let mut ta = TextArea::new();
        ta.insert_str("hi ");
        let id = ta.insert_element("elem", ElementKind(0), None);
        ta.insert_str(" bye");

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Element occupies cols 3..7, click at col 4
        let action = ta.handle_mouse(mouse_down(4, 0), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        let ev = ta.poll_element_event().expect("should emit element click");
        assert_eq!(ev.id, id);
        assert_eq!(ev.kind, TextElementEventKind::Click);
    }

    #[test]
    fn click_past_end_of_line_snaps_to_line_end() {
        let mut ta = ta_with("hi");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click far past end of "hi" (col 20)
        let action = ta.handle_mouse(mouse_down(20, 0), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        assert_eq!(ta.cursor(), 2); // end of "hi"
    }

    #[test]
    fn click_below_text_snaps_to_text_end() {
        let mut ta = ta_with("hello");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click on row 3 (only 1 row of text)
        let action = ta.handle_mouse(mouse_down(0, 3), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        assert_eq!(ta.cursor(), 5); // text.len()
    }

    #[test]
    fn click_clears_existing_selection() {
        let mut ta = ta_with("hello world");
        ta.set_selection(0, 5);
        assert!(ta.selection_range().is_some());

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        ta.handle_mouse(mouse_down(8, 0), area, state);
        assert!(ta.selection_range().is_none());
    }

    #[test]
    fn click_outside_area_returns_nothing() {
        let mut ta = ta_with("hello");
        let area = Rect::new(5, 5, 20, 3);
        let state = TextAreaState::default();

        // Click outside the area
        let action = ta.handle_mouse(mouse_down(0, 0), area, state);
        assert_eq!(action, MouseAction::Nothing);
    }

    #[test]
    fn mouse_up_clears_down_pos() {
        let mut ta = ta_with("hello");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        ta.handle_mouse(mouse_down(2, 0), area, state);
        assert!(ta.mouse_down_pos.is_some());

        ta.handle_mouse(mouse_up(2, 0), area, state);
        assert!(ta.mouse_down_pos.is_none());
    }

    #[test]
    fn click_on_second_line_multiline_text() {
        let mut ta = ta_with("hello\nworld");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click on row 1, col 2 → "world" starts at byte 6, so byte 8 = 'r'
        let action = ta.handle_mouse(mouse_down(2, 1), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        assert_eq!(ta.cursor(), 8); // "hello\nwo" = 8 bytes → cursor at 'r'
    }

    // ── M4: Drag selection tests ──

    fn mouse_drag(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn drag_selects_text() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Mouse down at col 0, drag to col 5
        ta.handle_mouse(mouse_down(0, 0), area, state);
        let action = ta.handle_mouse(mouse_drag(5, 0), area, state);
        assert_eq!(action, MouseAction::SelectionUpdated);
        assert_eq!(ta.selection_range(), Some(0..5));
        assert_eq!(ta.selected_text(), Some("hello".to_string()));
    }

    #[test]
    fn drag_across_element_expands_to_element_boundaries() {
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        let mut ta = TextArea::new();
        ta.insert_str("ab");
        ta.insert_element("ELEM", ElementKind(0), None);
        ta.insert_str("cd");
        // buffer: "abELEMcd"
        // element range: 2..6
        // display cols: a(0) b(1) E(2) L(3) E(4) M(5) c(6) d(7)

        // Drag from col 1 ("b") to col 7 ("d") — fully crosses the element.
        // Raw selection: anchor=1, head=7. Element at 2..6 is fully inside.
        ta.handle_mouse(mouse_down(1, 0), area, state);
        ta.handle_mouse(mouse_drag(7, 0), area, state);
        let range = ta.selection_range().unwrap();
        assert_eq!(range, 1..7);

        let mut ta = TextArea::new();
        ta.insert_str("ab");
        ta.insert_element("ELEM", ElementKind(0), None);
        ta.insert_str("cd");

        // Now test partial overlap: drag from col 0 to col 3 (into the element).
        // display_col_to_buffer_pos snaps col 3 to element start (2) since dist
        // to start (1) < dist to end (3). Raw selection 0..2 → but element at
        // 2..6 is NOT overlapped, so no expansion.
        ta.handle_mouse(mouse_down(0, 0), area, state);
        ta.handle_mouse(mouse_drag(3, 0), area, state);
        let range = ta.selection_range().unwrap();
        assert_eq!(range, 0..2);

        let mut ta = TextArea::new();
        ta.insert_str("ab");
        ta.insert_element("ELEM", ElementKind(0), None);
        ta.insert_str("cd");

        // Drag from col 0 to col 5 — past element midpoint, so snaps to end (6).
        // Raw selection 0..6 → element fully covered.
        ta.handle_mouse(mouse_down(0, 0), area, state);
        ta.handle_mouse(mouse_drag(5, 0), area, state);
        let range = ta.selection_range().unwrap();
        assert_eq!(range, 0..6);
    }

    #[test]
    fn mouse_up_after_drag_copies_to_clipboard() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        ta.handle_mouse(mouse_down(6, 0), area, state);
        ta.handle_mouse(mouse_drag(11, 0), area, state);
        let action = ta.handle_mouse(mouse_up(11, 0), area, state);
        assert_eq!(action, MouseAction::SelectionFinished);

        // Clipboard should contain "world"
        assert_eq!(ta.take_clipboard(), Some("world".to_string()));
        // take_clipboard clears it
        assert_eq!(ta.take_clipboard(), None);
    }

    #[test]
    fn selection_persists_after_mouseup_by_default() {
        let mut ta = ta_with("hello");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        ta.handle_mouse(mouse_down(0, 0), area, state);
        ta.handle_mouse(mouse_drag(3, 0), area, state);
        ta.handle_mouse(mouse_up(3, 0), area, state);

        // Default: keep_selection_after_mouseup = true
        assert!(ta.selection_range().is_some());
        assert_eq!(ta.selected_text(), Some("hel".to_string()));
    }

    #[test]
    fn selection_clears_after_mouseup_when_configured() {
        let mut ta = ta_with("hello");
        ta.keep_selection_after_mouseup = false;
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        ta.handle_mouse(mouse_down(0, 0), area, state);
        ta.handle_mouse(mouse_drag(3, 0), area, state);
        ta.handle_mouse(mouse_up(3, 0), area, state);

        // Clipboard was still set
        assert_eq!(ta.take_clipboard(), Some("hel".to_string()));
        // But selection is cleared
        assert!(ta.selection_range().is_none());
    }

    #[test]
    fn backspace_deletes_selection_only() {
        let mut ta = ta_with("hello world");
        ta.set_selection(0, 5);
        assert_eq!(ta.selected_text(), Some("hello".to_string()));

        // Backspace should delete "hello", not an extra char.
        ta.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(ta.text(), " world");
        assert_eq!(ta.cursor(), 0);
        assert!(ta.selection_range().is_none());
    }

    #[test]
    fn typing_replaces_selection() {
        let mut ta = ta_with("hello world");
        ta.set_selection(0, 5);

        // Typing 'X' should replace "hello" with "X".
        ta.input(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert_eq!(ta.text(), "X world");
        assert_eq!(ta.cursor(), 1);
        assert!(ta.selection_range().is_none());
    }

    #[test]
    fn arrow_clears_selection() {
        let mut ta = ta_with("hello world");
        ta.set_selection(0, 5);
        assert!(ta.selection_range().is_some());

        ta.input(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(ta.selection_range().is_none());
    }

    #[test]
    fn undo_after_delete_selection_restores() {
        let mut ta = ta_with("hello world");
        ta.set_cursor(ta.text().len());
        ta.set_selection(0, 5);

        ta.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(ta.text(), " world");
        assert_eq!(ta.cursor(), 0);
        assert_eq!(ta.undo.last_cursor, 0);

        ta.undo();
        assert_eq!(ta.text(), "hello world");
    }

    #[test]
    fn undo_after_type_replace_selection_restores() {
        let mut ta = ta_with("hello world");
        ta.set_selection(0, 5);

        ta.input(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert_eq!(ta.text(), "X world");

        // Single undo should restore to pre-replacement state (undo group).
        ta.undo();
        assert_eq!(ta.text(), "hello world");
    }

    #[test]
    fn backspace_works_with_zero_width_selection() {
        // Regression: a zero-width selection (anchor == head, from mouse
        // jitter) caused Backspace/Delete to be silently swallowed because
        // delete_selection() returned false but input() still returned early.
        let mut ta = ta_with("hello");
        ta.set_cursor(5);
        // Simulate a zero-width selection (anchor == head at cursor).
        ta.set_selection(5, 5);
        assert!(ta.selection_range().is_none()); // zero-width → no range

        // Backspace must still delete the char before cursor.
        ta.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(ta.text(), "hell");
        assert_eq!(ta.cursor(), 4);
        // Selection should be cleared.
        assert!(ta.selection.is_none());
    }

    #[test]
    fn delete_forward_works_with_zero_width_selection() {
        let mut ta = ta_with("hello");
        ta.set_cursor(2);
        ta.set_selection(2, 2);

        ta.input(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(ta.text(), "helo");
        assert_eq!(ta.cursor(), 2);
        assert!(ta.selection.is_none());
    }

    #[test]
    fn ctrl_x_with_zero_width_selection_falls_through() {
        let mut ta = ta_with("hello");
        ta.set_cursor(5);
        ta.set_selection(5, 5);

        // Ctrl-X on zero-width selection shouldn't eat the key.
        // It should clear selection and fall through to normal handling
        // (which for Ctrl-X without selection is a no-op, but the selection
        // must be cleared).
        ta.input(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(ta.selection.is_none());
    }

    #[test]
    fn mouse_up_discards_zero_width_drag_selection() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click at col 3 then drag to same position (zero distance).
        ta.handle_mouse(mouse_down(3, 0), area, state);
        let action = ta.handle_mouse(mouse_drag(3, 0), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        let action = ta.handle_mouse(mouse_up(3, 0), area, state);
        assert_eq!(action, MouseAction::Nothing);

        // Zero-width drag should not leave a selection behind.
        assert!(ta.selection.is_none());
    }

    #[test]
    fn drag_backward_selects_correctly() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click at col 8, drag back to col 3 (backward selection)
        ta.handle_mouse(mouse_down(8, 0), area, state);
        ta.handle_mouse(mouse_drag(3, 0), area, state);
        // selection_range() normalizes anchor/head
        assert_eq!(ta.selection_range(), Some(3..8));
        assert_eq!(ta.selected_text(), Some("lo wo".to_string()));
    }

    #[test]
    fn click_after_drag_clears_selection() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Drag to select
        ta.handle_mouse(mouse_down(0, 0), area, state);
        ta.handle_mouse(mouse_drag(5, 0), area, state);
        ta.handle_mouse(mouse_up(5, 0), area, state);
        assert!(ta.selection_range().is_some());

        // New click clears the old selection
        ta.handle_mouse(mouse_down(8, 0), area, state);
        assert!(ta.selection_range().is_none());
    }

    #[test]
    fn set_text_clears_selection_and_drag_state() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        ta.handle_mouse(mouse_down(0, 0), area, state);
        ta.handle_mouse(mouse_drag(5, 0), area, state);
        assert!(ta.selection_range().is_some());
        assert!(ta.drag_anchor.is_some());
        assert!(ta.drag_active);
        assert!(ta.mouse_down_pos.is_some());

        ta.set_text("reset");

        assert!(ta.selection.is_none());
        assert!(ta.selection_range().is_none());
        assert!(ta.drag_anchor.is_none());
        assert!(!ta.drag_active);
        assert!(ta.mouse_down_pos.is_none());
        assert!(ta.pending_drag_scroll.is_none());
    }

    #[test]
    fn same_cell_drag_does_not_create_selection() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        ta.handle_mouse(mouse_down(3, 0), area, state);
        let action = ta.handle_mouse(mouse_drag(3, 0), area, state);

        assert_eq!(action, MouseAction::CursorPlaced);
        assert!(ta.selection.is_none());
        assert!(ta.selection_range().is_none());
        assert!(!ta.drag_active);
    }

    #[test]
    fn typing_with_zero_width_selection_inserts_character() {
        let mut ta = ta_with("hello");
        ta.set_cursor(5);
        ta.set_selection(5, 5);

        ta.input(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT));

        assert_eq!(ta.text(), "hello!");
        assert_eq!(ta.cursor(), 6);
        assert!(ta.selection.is_none());
    }

    #[test]
    fn mouse_up_after_drag_clears_drag_anchor() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        ta.handle_mouse(mouse_down(0, 0), area, state);
        ta.handle_mouse(mouse_drag(5, 0), area, state);
        assert!(ta.drag_anchor.is_some());

        ta.handle_mouse(mouse_up(5, 0), area, state);

        assert!(ta.drag_anchor.is_none());
        assert!(!ta.drag_active);
    }

    // ── M5: Double/triple click tests ──

    #[test]
    fn double_click_selects_word() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Double-click on "hello" (col 2)
        ta.handle_mouse(mouse_down(2, 0), area, state);
        let action = ta.handle_mouse(mouse_down(2, 0), area, state);
        assert_eq!(action, MouseAction::SelectionFinished);
        assert_eq!(ta.selection_range(), Some(0..5));
        assert_eq!(ta.selected_text(), Some("hello".to_string()));
        assert_eq!(ta.take_clipboard(), Some("hello".to_string()));
    }

    #[test]
    fn double_click_cursor_on_last_char() {
        // Neovim places cursor on the last character of the selection,
        // not one past the end.
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Double-click on "hello"
        ta.handle_mouse(mouse_down(2, 0), area, state);
        ta.handle_mouse(mouse_down(2, 0), area, state);

        assert_eq!(ta.selection_range(), Some(0..5));
        // Cursor should be on 'o' (byte 4), not on ' ' (byte 5)
        assert_eq!(
            ta.cursor(),
            4,
            "double-click cursor should be on last char 'o' (byte 4), got {}",
            ta.cursor()
        );
    }

    #[test]
    fn double_click_cursor_on_last_char_unicode() {
        // Test with multi-byte characters — cursor must land on a valid char boundary
        let mut ta = ta_with("café bar");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Double-click on "café" (col 1 = 'a')
        ta.handle_mouse(mouse_down(1, 0), area, state);
        ta.handle_mouse(mouse_down(1, 0), area, state);

        assert_eq!(ta.selected_text(), Some("café".to_string()));
        // 'é' is 2 bytes (0xC3 0xA9), so "café" = [c(0), a(1), f(2), é(3,4)]
        // Last char 'é' starts at byte 3
        assert_eq!(
            ta.cursor(),
            3,
            "cursor should be on 'é' (byte 3), got {}",
            ta.cursor()
        );
    }

    #[test]
    fn double_click_on_second_word() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Double-click on "world" (col 8)
        ta.handle_mouse(mouse_down(8, 0), area, state);
        let action = ta.handle_mouse(mouse_down(8, 0), area, state);
        assert_eq!(action, MouseAction::SelectionFinished);
        assert_eq!(ta.selection_range(), Some(6..11));
        assert_eq!(ta.selected_text(), Some("world".to_string()));
    }

    #[test]
    fn double_click_stops_at_punctuation() {
        // "hello, world," — double-click on 'h' should select "hello", not "hello,"
        let mut ta = ta_with("hello, world,");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Double-click on "hello" (col 2 = 'l')
        ta.handle_mouse(mouse_down(2, 0), area, state);
        let action = ta.handle_mouse(mouse_down(2, 0), area, state);
        assert_eq!(action, MouseAction::SelectionFinished);
        assert_eq!(
            ta.selected_text(),
            Some("hello".to_string()),
            "should select only 'hello', not include trailing comma"
        );
        assert_eq!(ta.selection_range(), Some(0..5));
    }

    #[test]
    fn double_click_on_punctuation_selects_punctuation_run() {
        // Double-click on punctuation selects the contiguous punctuation
        let mut ta = ta_with("hello... world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Double-click on "..." (col 6 = first '.')
        ta.handle_mouse(mouse_down(6, 0), area, state);
        let action = ta.handle_mouse(mouse_down(6, 0), area, state);
        assert_eq!(action, MouseAction::SelectionFinished);
        assert_eq!(
            ta.selected_text(),
            Some("...".to_string()),
            "double-click on punctuation should select the punctuation run"
        );
    }

    #[test]
    fn double_click_word_with_underscore() {
        // Underscores are part of a word (like vim's iskeyword)
        let mut ta = ta_with("hello_world foo");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Double-click on "hello_world" (col 3)
        ta.handle_mouse(mouse_down(3, 0), area, state);
        let action = ta.handle_mouse(mouse_down(3, 0), area, state);
        assert_eq!(action, MouseAction::SelectionFinished);
        assert_eq!(
            ta.selected_text(),
            Some("hello_world".to_string()),
            "underscore should be part of the word"
        );
    }

    #[test]
    fn double_click_on_element_snaps_like_single_click() {
        // Word-selecting an element would copy its hidden buffer text to
        // the clipboard; a double-click must instead snap to the element
        // start and re-emit Click so the host decides what it means.
        let mut ta = TextArea::new();
        ta.insert_str("hi ");
        let display = Line::from("[chip]");
        let id = ta.insert_element("hidden\ntext", ElementKind(0), Some(display));
        ta.insert_str(" bye");
        // Buffer: "hi hidden\ntext bye", element at 3..14, display "[chip]".

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // First click on the display (col 4) emits its own Click event.
        ta.handle_mouse(mouse_down(4, 0), area, state);
        assert!(ta.poll_element_event().is_some());

        let action = ta.handle_mouse(mouse_down(4, 0), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        assert_eq!(ta.cursor(), 3); // element start
        assert!(ta.selection_range().is_none());
        assert_eq!(ta.take_clipboard(), None);
        let ev = ta
            .poll_element_event()
            .expect("double-click re-emits Click");
        assert_eq!(ev.id, id);
        assert_eq!(ev.kind, TextElementEventKind::Click);
    }

    #[test]
    fn triple_click_selects_line() {
        let mut ta = ta_with("hello world\nsecond line\nthird");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Triple-click on first line (col 3)
        ta.handle_mouse(mouse_down(3, 0), area, state);
        ta.handle_mouse(mouse_down(3, 0), area, state);
        let action = ta.handle_mouse(mouse_down(3, 0), area, state);
        assert_eq!(action, MouseAction::SelectionFinished);
        // Should select "hello world\n" (including the newline)
        assert_eq!(ta.selection_range(), Some(0..12));
        assert_eq!(ta.selected_text(), Some("hello world\n".to_string()));
    }

    #[test]
    fn triple_click_on_last_line_selects_to_end() {
        let mut ta = ta_with("hello\nworld");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Triple-click on "world" (row 1, col 2)
        ta.handle_mouse(mouse_down(2, 1), area, state);
        ta.handle_mouse(mouse_down(2, 1), area, state);
        let action = ta.handle_mouse(mouse_down(2, 1), area, state);
        assert_eq!(action, MouseAction::SelectionFinished);
        // Last line has no trailing \n — selects to text.len()
        assert_eq!(ta.selection_range(), Some(6..11));
        assert_eq!(ta.selected_text(), Some("world".to_string()));
    }

    #[test]
    fn triple_click_cursor_stays_at_click_pos() {
        // Triple-click should select the whole line but keep the cursor
        // at the click position, not at the end of the selection.
        let mut ta = ta_with("hello world\nsecond line\nthird");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Triple-click on first line at col 3 (byte 3 = 'l')
        ta.handle_mouse(mouse_down(3, 0), area, state);
        ta.handle_mouse(mouse_down(3, 0), area, state);
        ta.handle_mouse(mouse_down(3, 0), area, state);

        // Selection covers the full line "hello world\n"
        assert_eq!(ta.selection_range(), Some(0..12));

        // Cursor should be at the click position (byte 3), not at line_end (12)
        assert_eq!(
            ta.cursor(),
            3,
            "triple-click cursor should stay at click pos (3), got {}",
            ta.cursor()
        );
    }

    #[test]
    fn selection_uses_custom_style_override() {
        let mut t = ta_with("hello");
        t.selection_style = Style::default().bg(Color::Blue);
        t.set_selection(1, 4); // select "ell"

        let area = Rect::new(0, 0, 10, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&t), area, &mut buf);

        // Cells 1, 2, 3 should have Blue background (custom selection style)
        for col in 1..4u16 {
            let cell = &buf[(col, 0)];
            assert_eq!(
                cell.bg,
                Color::Blue,
                "cell at col {col} should have Blue bg"
            );
        }
        // Cell 0 ('h') and cell 4 ('o') should NOT have Blue bg
        assert_ne!(buf[(0, 0)].bg, Color::Blue);
        assert_ne!(buf[(4, 0)].bg, Color::Blue);
    }

    #[test]
    fn double_click_on_whitespace_places_cursor() {
        let mut ta = ta_with("hello   world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Double-click on whitespace (col 6)
        ta.handle_mouse(mouse_down(6, 0), area, state);
        let action = ta.handle_mouse(mouse_down(6, 0), area, state);
        // Whitespace has no word → just places cursor
        assert_eq!(action, MouseAction::CursorPlaced);
        assert!(ta.selection_range().is_none());
    }

    #[test]
    fn click_tracker_resets_on_position_change() {
        let mut ta = ta_with("hello world");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click at col 2, then at col 8 → not a double-click
        ta.handle_mouse(mouse_down(2, 0), area, state);
        let action = ta.handle_mouse(mouse_down(8, 0), area, state);
        // Should be a single click, not a double-click
        assert_eq!(action, MouseAction::CursorPlaced);
        assert!(ta.selection_range().is_none());
    }

    // ── Drag-to-scroll tests ──

    #[test]
    fn drag_below_area_scrolls_down_and_extends_selection() {
        // Text with 5 lines, visible area is only 3 rows tall.
        let mut ta = ta_with("aaa\nbbb\nccc\nddd\neee");
        // Place cursor at start so scroll=0.
        ta.set_cursor(0);
        let area = Rect::new(0, 0, 40, 3);
        let state = TextAreaState::default();

        // Click on first line.
        ta.handle_mouse(mouse_down(0, 0), area, state);
        assert_eq!(ta.cursor(), 0);

        // Drag below the visible area (row 5, past area.height=3).
        let action = ta.handle_mouse(mouse_drag(0, 5), area, state);
        assert_eq!(action, MouseAction::SelectionUpdated);

        // Cursor should have moved past the visible area.
        // With scroll=0 and height=3, visible lines are 0,1,2 (aaa,bbb,ccc).
        // Dragging below → target_line = visible_end = 3 → "ddd" starts at byte 12.
        // At col 0, cursor should be at byte 12 (start of "ddd").
        assert!(ta.cursor() >= 12);

        // Selection should extend from anchor (0) to the new cursor position.
        let range = ta.selection_range().unwrap();
        assert_eq!(range.start, 0);
        assert!(range.end >= 12);
    }

    #[test]
    fn drag_above_area_scrolls_up_and_extends_selection() {
        // Text with 5 lines, start with cursor on the last line.
        let mut ta = ta_with("aaa\nbbb\nccc\nddd\neee");
        let area = Rect::new(0, 0, 40, 3);

        // Place cursor at the end first (to ensure scroll is at the bottom).
        ta.set_cursor(ta.text().len());
        let state = TextAreaState { scroll: 2 };

        // Click on bottom visible line (row 2).
        ta.handle_mouse(mouse_down(1, 2), area, state);

        // Drag above the visible area (row is before area.y).
        // Since area.y = 0, dragging to row=0 when scroll=2 means the row
        // is at the top edge. We need a row *above* the area. With area.y=0,
        // we can't go negative, but we can use an area with area.y > 0.
        let area2 = Rect::new(0, 5, 40, 3); // area starts at row 5
        ta.handle_mouse(mouse_down(1, 7), area2, state); // click at row 7 (visible)

        // Drag above: row 3 (above area2.y=5)
        let action = ta.handle_mouse(mouse_drag(0, 3), area2, state);
        assert_eq!(action, MouseAction::SelectionUpdated);

        // Cursor should have moved to a line above the visible region.
        // The exact position depends on how many lines we scroll per drag.
        let range = ta.selection_range().unwrap();
        assert!(range.start < range.end);
    }

    #[test]
    fn drag_below_area_moves_cursor_past_last_visible_line() {
        // 10 short lines, area shows only 2.
        let text = "L0\nL1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9";
        let mut ta = ta_with(text);
        // Place cursor at start so scroll=0.
        ta.set_cursor(0);
        let area = Rect::new(0, 0, 40, 2);
        let state = TextAreaState::default();

        // Click at start.
        ta.handle_mouse(mouse_down(0, 0), area, state);
        assert_eq!(ta.cursor(), 0);

        // Drag below area (row 10, way below the 2-row area).
        let action = ta.handle_mouse(mouse_drag(1, 10), area, state);
        assert_eq!(action, MouseAction::SelectionUpdated);

        // With scroll=0 and height=2, visible lines are 0,1 (L0,L1).
        // Dragging below → target_line = 2 → "L2" starts at byte 6.
        // Cursor should be at col 1 of L2 → byte 7.
        assert!(ta.cursor() >= 6, "cursor={} should be >= 6", ta.cursor());
    }

    #[test]
    fn drag_above_wide_column_still_scrolls_up() {
        // Bug: when dragging above the area with a column wider than the
        // target line, display_col_to_buffer_pos returns line_end which
        // equals the *next* line's start.  wrapped_line_index_by_start
        // then resolves to the next line, so effective_scroll sees the
        // cursor as still within the viewport and doesn't scroll.
        //
        // Scenario: 10 short lines ("ab"), area is 3 rows tall with
        // area.y = 2 (so we can drag above).  Scroll starts at line 5.
        // We drag to row 1 (above area.y = 2) at column 50 (way past
        // each 3-byte line).  The cursor must land ON the target line
        // (line 4), not spill over to line 5.
        let text = "ab\nab\nab\nab\nab\nab\nab\nab\nab\nab";
        let mut ta = ta_with(text);
        let area = Rect::new(0, 2, 40, 3); // area starts at row 2
        let state = TextAreaState { scroll: 5 };

        // Place cursor on wrapped line 6 (within viewport at scroll=5).
        // "ab\n" is 3 bytes per line, so line 6 starts at byte 18.
        ta.set_cursor(18);

        // Click inside the area (row 3 = area.y + 1).
        ta.handle_mouse(mouse_down(1, 3), area, state);

        // Drag above the area: row 1 (< area.y=2), column 50 (far right).
        let action = ta.handle_mouse(mouse_drag(50, 1), area, state);
        assert_eq!(action, MouseAction::SelectionUpdated);

        // target_line = scroll(5) - 1 = 4.  Line 4 spans bytes 12..15.
        // The cursor MUST be within line 4's range [12, 14], NOT at 15
        // (which is line 5's start).
        let cursor = ta.cursor();
        assert!(
            (12..15).contains(&cursor),
            "cursor={cursor} should be in [12, 15) (on line 4), \
             not at 15 (line 5 start)"
        );
    }

    #[test]
    fn drag_below_wide_column_still_scrolls_down() {
        // Same bug but for scrolling down: dragging below the area with
        // a wide column should place the cursor on the target line, not
        // spill over to the next line.
        let text = "ab\nab\nab\nab\nab\nab\nab\nab\nab\nab";
        let mut ta = ta_with(text);
        let area = Rect::new(0, 0, 40, 3);
        let state = TextAreaState::default(); // scroll=0

        // Place cursor on first line.
        ta.set_cursor(0);

        // Click inside the area.
        ta.handle_mouse(mouse_down(1, 0), area, state);

        // Drag below the area: row 5 (>= area.y + height=3), column 50.
        let action = ta.handle_mouse(mouse_drag(50, 5), area, state);
        assert_eq!(action, MouseAction::SelectionUpdated);

        // visible_end = 0 + 3 = 3.  dist = 5 - 3 + 1 = 3.
        // n = drag_scroll_lines_for_distance(3) = 2.
        // target_line = (3 + 2 - 1) = 4.  Line 4 spans bytes 12..15.
        // Cursor must be within [12, 14], not at 15.
        let cursor = ta.cursor();
        assert!(
            (12..15).contains(&cursor),
            "cursor={cursor} should be in [12, 15) (on line 4), \
             not at 15 (line 5 start)"
        );
    }

    #[test]
    fn drag_above_with_multibyte_line_end_does_not_panic() {
        // Regression: clamp_to_line used `line_end - 1` which can land inside
        // a multi-byte character (e.g. '│' = 3 bytes).
        let text = "aaa│\nbbb│\nccc│\nddd│\neee│\nfff│\nggg│";
        let mut ta = ta_with(text);
        let area = Rect::new(0, 0, 40, 3);
        // Start scrolled down so we can drag above.
        let state = TextAreaState { scroll: 3 };
        ta.set_cursor(20); // somewhere in the middle

        // Click inside the area.
        ta.handle_mouse(mouse_down(1, 1), area, state);

        // Drag above the area at a wide column (beyond line width).
        let action = ta.handle_mouse(mouse_drag(50, 0), area, state);
        // Should not panic — cursor should be on a valid char boundary.
        assert!(
            matches!(action, MouseAction::SelectionUpdated),
            "drag above should create selection, got {action:?}"
        );
        // Verify cursor is at a valid char boundary by reading from it.
        let cursor = ta.cursor();
        assert!(
            ta.text().is_char_boundary(cursor),
            "cursor at byte {cursor} is not a char boundary"
        );
    }

    #[test]
    fn selection_across_element_with_multibyte_chars_does_not_panic() {
        // Regression: display_col_to_buffer_pos used `line_end + 1` to skip
        // past elements, but `line_end + 1` can land inside a multi-byte
        // character (e.g. '│' = 3 bytes).
        let mut ta = TextArea::new();
        ta.insert_str("before ");
        // Create an element whose backing text contains multi-byte '│' chars
        // across multiple lines — this triggers wrapping mid-element.
        let backing = "│  Ctrl+Shift+Z/Y  redo  │\n│  Ctrl+C  clear  │";
        ta.insert_element(backing, ElementKind(0), None);
        ta.insert_str(" after");

        let area = Rect::new(0, 0, 30, 5); // narrow so wrapping is forced

        // Select across the entire text (from start to end).
        ta.set_selection(0, ta.text().len());

        // Render should not panic.
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&ta), area, &mut buf);
    }

    #[test]
    fn click_on_text_with_multibyte_chars_does_not_panic() {
        // Plain text with '│' — clicking anywhere should not panic.
        let text = "│  Ctrl+Shift+Z/Y  redo  │\n│  Ctrl+C  clear  │";
        let mut ta = ta_with(text);
        let area = Rect::new(0, 0, 30, 5);
        let state = TextAreaState::default();

        // Click at various columns — should not panic.
        for col in 0..25u16 {
            ta.handle_mouse(mouse_down(col, 0), area, state);
        }
        // Double-click should also be safe.
        ta.handle_mouse(mouse_down(5, 0), area, state);
        ta.handle_mouse(mouse_down(5, 0), area, state);
    }

    #[test]
    fn selecting_wrapped_line_ending_with_multibyte_char_does_not_panic() {
        // Regression: when a line wraps and '│' (3-byte char) ends up right
        // at the wrap boundary, the wrapping code (or rendering) can produce
        // a byte position inside the multi-byte character.
        //
        // Reproduce: enough spaces so '│' is pushed to the next wrapped line.
        let text = format!("{}│", " ".repeat(29)); // 29 spaces + '│' = 30 display cols
        let mut ta = ta_with(&text);
        let area = Rect::new(0, 0, 30, 5); // width 30 → '│' wraps to next line
        let _state = TextAreaState::default();

        // Select across the wrap boundary.
        ta.set_selection(0, ta.text().len());

        // Render should not panic.
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::WidgetRef::render_ref(&(&ta), area, &mut buf);
    }

    #[test]
    fn clicking_on_wrapped_multibyte_line_does_not_panic() {
        // Same as above but triggered via click/drag rather than render.
        for extra_spaces in 28..33 {
            let text = format!("{}│end", " ".repeat(extra_spaces));
            let mut ta = ta_with(&text);
            let area = Rect::new(0, 0, 30, 5);
            let state = TextAreaState::default();

            // Click on every column of both rows.
            for row in 0..2u16 {
                for col in 0..30u16 {
                    ta.handle_mouse(mouse_down(col, row), area, state);
                }
            }
        }
    }

    // ── Inline element tests ──

    #[test]
    fn inline_element_replaces_element_with_text() {
        let mut ta = TextArea::new();
        ta.insert_str("before ");
        let id = ta.insert_element("pasted\ncontent\nhere", ElementKind(1), None);
        ta.insert_str(" after");
        // Buffer: "before pasted\ncontent\nhere after"
        // Element at "pasted\ncontent\nhere" (bytes 7..26)

        let inlined = ta.inline_element(id);
        assert!(inlined);

        // Text should remain the same (the element's buffer text is kept).
        assert_eq!(ta.text(), "before pasted\ncontent\nhere after");
        // But the element should be gone.
        assert!(ta.elements().is_empty());
        // Cursor should be at the end of the inlined text.
        assert_eq!(ta.cursor(), 26); // end of "pasted\ncontent\nhere"
    }

    #[test]
    fn inline_element_is_undoable() {
        let mut ta = TextArea::new();
        ta.insert_str("A ");
        let id = ta.insert_element("multi\nline", ElementKind(1), None);
        ta.insert_str(" B");
        // Buffer: "A multi\nline B", element at 2..12

        assert_eq!(ta.elements().len(), 1);

        ta.inline_element(id);
        assert!(ta.elements().is_empty());
        assert_eq!(ta.text(), "A multi\nline B");

        // Undo should restore the element.
        assert!(ta.undo());
        assert_eq!(ta.elements().len(), 1);
        assert_eq!(ta.element_text(id), Some("multi\nline"));
    }

    #[test]
    fn inline_nonexistent_element_returns_false() {
        let mut ta = ta_with("hello");
        let fake_id = ElementId(9999);
        assert!(!ta.inline_element(fake_id));
    }

    #[test]
    fn inline_element_cursor_at_element_start() {
        let mut ta = TextArea::new();
        let id = ta.insert_element("elem", ElementKind(0), None);
        ta.insert_str(" tail");
        // Cursor is after " tail" → at end.
        // Move cursor to element start.
        ta.set_cursor(0);

        ta.inline_element(id);
        // Element removed, text unchanged.
        assert!(ta.elements().is_empty());
        // Cursor at end of inlined region.
        assert_eq!(ta.cursor(), 4);
    }

    // ── Click-on-element edge cases ──

    #[test]
    fn click_on_element_second_half_snaps_to_start() {
        // Element with a wide display: clicking on the right half should still
        // snap cursor to element start and emit a Click element event.
        let mut ta = TextArea::new();
        ta.insert_str("ab");
        // Element display is "ELEM" (4 chars). Buffer text is "xy".
        let display = Line::from("ELEM");
        let id = ta.insert_element("xy", ElementKind(0), Some(display));
        ta.insert_str("cd");
        // Buffer: "abxycd", element at 2..4, display "ELEM" (4 wide)
        // Visual: a b E L E M c d
        //         0 1 2 3 4 5 6 7

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();
        ta.set_cursor(0);

        // Click on col 5 → second half of "ELEM" display.
        // display_col_to_buffer_pos should return elem_end=4 (closer to end).
        // handle_mouse should detect this as on-element and snap to start.
        let action = ta.handle_mouse(mouse_down(5, 0), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        let ev = ta.poll_element_event().expect("should emit element click");
        assert_eq!(ev.id, id);
        assert_eq!(ev.kind, TextElementEventKind::Click);
        assert_eq!(ta.cursor(), 2); // element start
    }

    #[test]
    fn click_on_element_first_half_snaps_to_start() {
        let mut ta = TextArea::new();
        ta.insert_str("ab");
        let display = Line::from("ELEM");
        let id = ta.insert_element("xy", ElementKind(0), Some(display));
        ta.insert_str("cd");
        ta.set_cursor(0);

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click on col 2 → first half of "ELEM" display.
        let action = ta.handle_mouse(mouse_down(2, 0), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        let ev = ta.poll_element_event().expect("should emit element click");
        assert_eq!(ev.id, id);
        assert_eq!(ev.kind, TextElementEventKind::Click);
        assert_eq!(ta.cursor(), 2);
    }

    #[test]
    fn click_after_element_places_cursor_not_element() {
        let mut ta = TextArea::new();
        ta.insert_str("ab");
        let display = Line::from("EL");
        ta.insert_element("xy", ElementKind(0), Some(display));
        ta.insert_str("cd");
        ta.set_cursor(0);
        // Visual: a b E L c d
        //         0 1 2 3 4 5

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click on col 4 → 'c' (after element).
        let action = ta.handle_mouse(mouse_down(4, 0), area, state);
        assert_eq!(action, MouseAction::CursorPlaced);
        assert_eq!(ta.cursor(), 4); // byte 4 = 'c'
    }

    // ── Mouse wheel tests ──

    fn mouse_scroll_down(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn mouse_scroll_up(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn scroll_down_returns_scrolled() {
        let mut ta = ta_with("aaa\nbbb\nccc\nddd\neee");
        ta.set_cursor(0);
        let area = Rect::new(0, 0, 40, 3);
        let state = TextAreaState::default();

        let action = ta.handle_mouse(mouse_scroll_down(5, 1), area, state);
        assert_eq!(action, MouseAction::Scrolled);
    }

    #[test]
    fn scroll_up_returns_scrolled() {
        let mut ta = ta_with("aaa\nbbb\nccc\nddd\neee");
        ta.set_cursor(ta.text().len());
        let area = Rect::new(0, 0, 40, 3);
        let state = TextAreaState { scroll: 2 };

        let action = ta.handle_mouse(mouse_scroll_up(5, 1), area, state);
        assert_eq!(action, MouseAction::Scrolled);
    }

    #[test]
    fn scroll_down_when_content_fits_returns_nothing() {
        let mut ta = ta_with("short");
        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        let action = ta.handle_mouse(mouse_scroll_down(5, 1), area, state);
        assert_eq!(action, MouseAction::Nothing);
    }

    #[test]
    fn mousewheel_scrolls_viewport_not_cursor() {
        // Mousewheel should scroll the viewport without moving the cursor.
        // The cursor should stay at its current buffer position.
        let text = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ta = ta_with(&text);
        let area = Rect::new(0, 0, 40, 5); // only 5 lines visible
        let state = TextAreaState::default();

        // Place cursor on "line 2"
        let line2_start = text.find("line 2").unwrap();
        ta.set_cursor(line2_start);
        let cursor_before = ta.cursor();

        // Scroll down
        ta.handle_mouse(mouse_scroll_down(0, 0), area, state);

        // Cursor must NOT have moved
        assert_eq!(
            ta.cursor(),
            cursor_before,
            "mousewheel should not move cursor"
        );
    }

    #[test]
    fn click_after_scroll_places_cursor_at_clicked_line() {
        // After scrolling the viewport away from the cursor via mousewheel,
        // clicking on a visible line should place the cursor on THAT line —
        // not jump to some other position based on the old cursor location.
        let text = (0..40)
            .map(|i| format!("line {:02}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ta = ta_with(&text);
        // height=20 → scroll_lines_for_height returns 3 lines/tick
        let area = Rect::new(0, 0, 40, 20);
        let mut state = TextAreaState::default();

        // Cursor starts at line 0.
        ta.set_cursor(0);

        // Render to initialize state.scroll.
        let mut buf = Buffer::empty(area);
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&ta), area, &mut buf, &mut state);

        // Scroll down 3 ticks (3 lines × 3 = 9 lines).
        for _ in 0..3 {
            ta.handle_mouse(mouse_scroll_down(0, 0), area, state);
            ratatui::widgets::StatefulWidgetRef::render_ref(&(&ta), area, &mut buf, &mut state);
        }

        // Viewport should now start around line 9.
        assert!(
            state.scroll >= 9,
            "viewport should have scrolled; scroll={}",
            state.scroll
        );

        // Click on visual row 0 (which is now "line 09" or similar).
        ta.handle_mouse(mouse_down(0, 0), area, state);

        // The cursor should now be on a line that was VISIBLE.
        let cursor = ta.cursor();
        let lines = ta.wrapped_lines(area.width);
        let cursor_line = TextArea::wrapped_line_index_by_start(&lines, cursor).unwrap();
        assert!(
            cursor_line >= state.scroll as usize
                && cursor_line < (state.scroll + area.height) as usize,
            "click on visible row 0 should place cursor on a visible line; \
             cursor_line={cursor_line}, scroll={}, visible=[{}..{})",
            state.scroll,
            state.scroll,
            state.scroll as usize + area.height as usize,
        );
    }

    #[test]
    fn drag_select_after_scroll_selects_visible_text() {
        // After scrolling, drag-selecting should work on the visible text,
        // not jump the viewport back.
        let text = (0..20)
            .map(|i| format!("line {:02}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ta = ta_with(&text);
        let area = Rect::new(0, 0, 40, 5);
        let mut state = TextAreaState::default();

        ta.set_cursor(0);

        // Render + scroll down.
        let mut buf = Buffer::empty(area);
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&ta), area, &mut buf, &mut state);
        for _ in 0..3 {
            ta.handle_mouse(mouse_scroll_down(0, 0), area, state);
            ratatui::widgets::StatefulWidgetRef::render_ref(&(&ta), area, &mut buf, &mut state);
        }
        let scroll_after = state.scroll;

        // Click-down on row 1, then drag to row 3.
        ta.handle_mouse(mouse_down(0, 1), area, state);
        // Re-render so state.scroll updates after the click.
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&ta), area, &mut buf, &mut state);
        ta.handle_mouse(mouse_drag(5, 3), area, state);

        // Selection should exist.
        let sel = ta.selection_range().expect("drag should create selection");

        // The selected region should be within the visible range, not at line 0.
        let lines = ta.wrapped_lines(area.width);
        let sel_start_line = TextArea::wrapped_line_index_by_start(&lines, sel.start).unwrap();
        assert!(
            sel_start_line >= scroll_after as usize,
            "selection start should be in scrolled region; \
             sel_start_line={sel_start_line}, scroll={scroll_after}"
        );
    }

    #[test]
    fn drag_outside_after_mousewheel_still_scrolls() {
        // After mousewheel scrolling during a drag, dragging outside the
        // textarea area should continue to auto-scroll the viewport.
        let text = (0..30)
            .map(|i| format!("line {:02}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ta = ta_with(&text);
        let area = Rect::new(0, 0, 40, 5);
        let mut state = TextAreaState::default();

        ta.set_cursor(0);

        // Render to initialize state.
        let mut buf = Buffer::empty(area);
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&ta), area, &mut buf, &mut state);

        // Start drag at row 0.
        ta.handle_mouse(mouse_down(0, 0), area, state);
        ta.handle_mouse(mouse_drag(0, 1), area, state);
        assert!(ta.selection_range().is_some());

        // Mousewheel scroll down during drag.
        ta.handle_mouse(mouse_scroll_down(0, 0), area, state);
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&ta), area, &mut buf, &mut state);
        let scroll_after_wheel = state.scroll;

        // Now drag below the area (row = area.y + area.height = 5).
        // This should auto-scroll the viewport further down.
        // We need to bypass throttle, so reset the timer.
        ta.last_drag_scroll = None;
        ta.drag_scroll_steps = 0;
        ta.handle_mouse(mouse_drag(0, area.y + area.height), area, state);
        ratatui::widgets::StatefulWidgetRef::render_ref(&(&ta), area, &mut buf, &mut state);

        assert!(
            state.scroll > scroll_after_wheel,
            "drag-below after mousewheel should continue scrolling; \
             scroll={}, expected > {scroll_after_wheel}",
            state.scroll
        );
    }

    #[test]
    fn scroll_during_drag_preserves_selection_anchor() {
        // Start drag at "bbb", scroll down — selection should extend,
        // anchor stays at original position.
        let mut ta = ta_with("aaa\nbbb\nccc\nddd\neee\nfff\nggg");
        ta.set_cursor(0); // put cursor at start so scroll=0 is consistent
        let area = Rect::new(0, 0, 40, 3);
        let state = TextAreaState::default();

        // Click-down on "bbb" (row 1, col 1 → byte 5 = second 'b')
        ta.handle_mouse(mouse_down(1, 1), area, state);
        let anchor = ta.cursor();
        assert_eq!(
            anchor, 5,
            "click on bbb col 1 should place cursor at byte 5"
        );

        // Start drag → creates selection
        ta.handle_mouse(mouse_drag(2, 1), area, state);
        assert!(
            ta.selection_range().is_some(),
            "drag should create selection"
        );

        // Now scroll down while dragging
        let action = ta.handle_mouse(mouse_scroll_down(1, 1), area, state);
        assert_eq!(action, MouseAction::Scrolled);

        // Selection should still exist and anchor should not have moved
        let sel = ta
            .selection_range()
            .expect("selection should survive scroll");
        assert!(
            sel.contains(&anchor),
            "anchor byte {anchor} should still be inside selection {sel:?}"
        );
    }

    #[test]
    fn scroll_during_drag_extends_selection_head() {
        // Scroll down during drag should move the selection head forward.
        let mut ta = ta_with("aaa\nbbb\nccc\nddd\neee\nfff\nggg");
        ta.set_cursor(0);
        let area = Rect::new(0, 0, 40, 3);
        let state = TextAreaState::default();

        // Click-down on "aaa" (row 0, col 1)
        ta.handle_mouse(mouse_down(1, 0), area, state);
        // Start drag
        ta.handle_mouse(mouse_drag(2, 0), area, state);
        let sel_before = ta.selection_range().unwrap();

        // Scroll down
        ta.handle_mouse(mouse_scroll_down(1, 1), area, state);
        let sel_after = ta.selection_range().unwrap();

        // Selection should have grown (head moved forward)
        assert!(
            sel_after.end > sel_before.end,
            "scroll-down during drag should extend selection: before={sel_before:?} after={sel_after:?}"
        );
        // Anchor should not have moved
        assert_eq!(sel_after.start, sel_before.start);
    }

    #[test]
    fn down_during_active_drag_does_not_reset_anchor() {
        // Some terminals re-emit Down(Left) after a scroll event even though
        // the button was held the whole time.  When `drag_active` is true,
        // a Down should be treated as a drag continuation, not a new click.
        let mut ta = ta_with("aaa\nbbb\nccc\nddd\neee\nfff\nggg");
        ta.set_cursor(0);
        let area = Rect::new(0, 0, 40, 3);
        let state = TextAreaState::default();

        // Click on "aaa" (row 0, col 1), then drag to start selection.
        ta.handle_mouse(mouse_down(1, 0), area, state);
        ta.handle_mouse(mouse_drag(2, 0), area, state);
        let anchor_before = ta.selection_range().unwrap().start;

        // Scroll down while dragging.
        ta.handle_mouse(mouse_scroll_down(1, 1), area, state);
        assert!(
            ta.selection_range().is_some(),
            "selection must survive scroll"
        );

        // Simulate terminal re-emitting Down(Left) at a different row.
        ta.handle_mouse(mouse_down(1, 2), area, state);

        // Selection should still exist and anchor should NOT have moved.
        let sel = ta
            .selection_range()
            .expect("Down during drag must not kill selection");
        assert_eq!(
            sel.start, anchor_before,
            "anchor must not reset: expected {anchor_before}, got {}",
            sel.start
        );
    }

    // ── Drag-scroll acceleration / distance helpers ──

    #[test]
    fn drag_scroll_interval_ramps_up() {
        // Step 0 → 80ms, step 1 → 60ms, step 2+ → 40ms.
        assert_eq!(TextArea::drag_scroll_interval(0), 80);
        assert_eq!(TextArea::drag_scroll_interval(1), 60);
        assert_eq!(TextArea::drag_scroll_interval(2), 40);
        assert_eq!(TextArea::drag_scroll_interval(100), 40);
    }

    #[test]
    fn drag_scroll_lines_for_distance_tiers() {
        // Close: 1 line, farther: more lines.
        assert_eq!(TextArea::drag_scroll_lines_for_distance(1), 1);
        assert_eq!(TextArea::drag_scroll_lines_for_distance(2), 1);
        assert_eq!(TextArea::drag_scroll_lines_for_distance(3), 2);
        assert_eq!(TextArea::drag_scroll_lines_for_distance(5), 2);
        assert_eq!(TextArea::drag_scroll_lines_for_distance(6), 3);
        assert_eq!(TextArea::drag_scroll_lines_for_distance(10), 3);
        assert_eq!(TextArea::drag_scroll_lines_for_distance(20), 5);
    }

    // ── Scrollbar tests ──

    #[test]
    fn scrollbar_not_shown_when_content_fits() {
        // 3 lines of text in a 5-row viewport → no scrollbar needed.
        let mut ta = TextArea::new();
        ta.insert_str("aaa\nbbb\nccc");
        let area = Rect::new(0, 0, 20, 5);
        let (cw, needs) = ta.content_width(area.width, area.height);
        assert!(!needs, "should not need scrollbar when content fits");
        assert_eq!(cw, 20, "full width when no scrollbar");
    }

    #[test]
    fn scrollbar_shown_when_content_overflows() {
        // 10 lines of text in a 5-row viewport → scrollbar needed.
        let mut ta = TextArea::new();
        ta.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        let area = Rect::new(0, 0, 20, 5);
        let (cw, needs) = ta.content_width(area.width, area.height);
        assert!(needs, "should need scrollbar when content overflows");
        assert_eq!(cw, 19, "width reduced by 1 for scrollbar");
    }

    #[test]
    fn scrollbar_respects_show_scrollbar_false() {
        let mut ta = TextArea::new();
        ta.show_scrollbar = false;
        ta.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        let area = Rect::new(0, 0, 20, 5);
        let (cw, needs) = ta.content_width(area.width, area.height);
        assert!(!needs, "scrollbar disabled");
        assert_eq!(cw, 20, "full width when scrollbar disabled");
    }

    #[test]
    fn scrollbar_wrapping_uses_narrower_width() {
        // A line that fits in 20 cols but not in 19 should wrap differently
        // when scrollbar is present.
        let mut ta = TextArea::new();
        // 19 'a's → fits in 19 cols (no wrap).
        // Then enough other lines to overflow the viewport.
        ta.insert_str(&format!("{}\n2\n3\n4\n5\n6", "a".repeat(19)));
        let area = Rect::new(0, 0, 20, 5);
        let (cw, needs) = ta.content_width(area.width, area.height);
        assert!(needs, "overflows");
        assert_eq!(cw, 19);
        // The 19-char line should NOT wrap at width 19 — it fits exactly.
        let lines = ta.wrapped_lines(cw);
        // First wrapped line should contain all 19 chars.
        assert_eq!(&ta.text()[lines[0].clone()], &"a".repeat(19));
    }

    #[test]
    fn click_on_scrollbar_column_scrolls() {
        let mut ta = TextArea::new();
        ta.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        // Move cursor to start so we can verify it doesn't move.
        let _ = ta.text.set_cursor_byte(0);
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Click on the scrollbar column (rightmost column = 19),
        // at the bottom row of the viewport → should scroll to end.
        let action = ta.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 19,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
            state,
        );
        assert_eq!(action, MouseAction::Scrolled);
        // Cursor should not have moved — scrollbar click doesn't place cursor.
        assert_eq!(ta.cursor(), 0);
        // scroll_override should be set.
        assert!(ta.scroll_override.is_some());
    }

    #[test]
    fn click_on_scrollbar_top_scrolls_to_top() {
        let mut ta = TextArea::new();
        ta.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Scroll to the bottom first so the top of the track is NOT the thumb.
        ta.scroll_override = Some(5);
        // Click at top of scrollbar (row 0) — should be track, jump to top.
        ta.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 19,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            area,
            state,
        );
        assert_eq!(ta.scroll_override, Some(0));
    }

    #[test]
    fn click_on_text_area_does_not_trigger_scrollbar() {
        let mut ta = TextArea::new();
        ta.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Click on column 18 (text area, not scrollbar column 19).
        let action = ta.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 18,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            area,
            state,
        );
        // Should place cursor, not scroll.
        assert_eq!(action, MouseAction::CursorPlaced);
        assert!(!ta.scrollbar_dragging);
    }

    #[test]
    fn drag_on_scrollbar_scrolls_proportionally() {
        let mut ta = TextArea::new();
        ta.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        // Scroll to bottom so the thumb is at the bottom, then click
        // on the track at row 0 to start a track-based drag.
        ta.scroll_override = Some(5);
        ta.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 19,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            area,
            state,
        );
        assert!(ta.scrollbar_dragging);
        let scroll_at_top = ta.scroll_override.unwrap();
        assert_eq!(scroll_at_top, 0, "track click at top should jump to 0");
        // Drag to middle of scrollbar track.
        ta.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 19,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
            area,
            state,
        );
        let scroll_at_mid = ta.scroll_override.unwrap();
        assert!(
            scroll_at_mid > scroll_at_top,
            "dragging down should scroll further"
        );
        // Drag to bottom.
        ta.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 19,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
            state,
        );
        let scroll_at_bottom = ta.scroll_override.unwrap();
        assert!(
            scroll_at_bottom > scroll_at_mid,
            "dragging to bottom should scroll to max"
        );
        // Mouse up ends drag.
        ta.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 19,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
            state,
        );
        assert!(!ta.scrollbar_dragging);
    }

    #[test]
    fn scrollbar_render_produces_track_and_thumb() {
        // Render a textarea with overflow and verify the scrollbar column
        // has non-default styled cells.
        let mut ta = TextArea::new();
        ta.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        let mut state = TextAreaState::default();
        StatefulWidgetRef::render_ref(&&ta, area, &mut buf, &mut state);

        let sb_col = 19u16;
        // All cells in the scrollbar column should have the track bg color.
        let mut has_thumb = false;
        for row in 0..5u16 {
            let cell = &buf[(sb_col, row)];
            // Track bg is Rgb(45,45,55); check bg is set.
            assert!(cell.style().bg.is_some(), "scrollbar cell should have bg");
            if cell.symbol() != " " {
                has_thumb = true;
            }
        }
        assert!(has_thumb, "should have at least one thumb cell");
    }

    #[test]
    fn no_scrollbar_column_when_content_fits() {
        // When content fits, the rightmost column should not have
        // scrollbar styling.
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        let mut state = TextAreaState::default();
        StatefulWidgetRef::render_ref(&&ta, area, &mut buf, &mut state);

        let last_col = 19u16;
        let cell = &buf[(last_col, 0u16)];
        // Should be default (empty space), not scrollbar styled.
        assert!(
            cell.style().bg.is_none() || !matches!(cell.style().bg, Some(Color::Rgb(32, 35, 53))),
            "should not have scrollbar bg when content fits"
        );
    }

    #[test]
    fn cursor_pos_accounts_for_scrollbar_width() {
        // When scrollbar is shown, cursor position should use content width,
        // not full area width.
        let mut ta = TextArea::new();
        // Fill 18 chars + enough lines to overflow.
        ta.insert_str(&format!("{}\n2\n3\n4\n5\n6", "x".repeat(18)));
        let _ = ta.text.set_cursor_byte(0); // at start
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();
        let pos = ta.cursor_pos_with_state(area, state);
        // Cursor at pos 0 should be at (0, 0).
        assert_eq!(pos, Some((0, 0)));
    }

    #[test]
    fn click_on_scrollbar_thumb_does_not_jump() {
        // With 10 lines in a 5-row viewport, the thumb is near the top
        // when scroll is at 0.  Clicking on the thumb should NOT jump —
        // it should just start a drag from the current position.
        let mut ta = TextArea::new();
        ta.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();

        // Scroll is at 0 — thumb should be at the top of the track.
        // Click on row 0 (top of track = on the thumb).
        let action = ta.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 19,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            area,
            state,
        );
        assert_eq!(action, MouseAction::Scrolled);
        assert!(ta.scrollbar_dragging);
        // The scroll should NOT have changed — thumb click = no jump.
        assert!(
            ta.scroll_override.is_none() || ta.scroll_override == Some(0),
            "thumb click should not jump: {:?}",
            ta.scroll_override,
        );
    }

    #[test]
    fn click_on_scrollbar_track_jumps() {
        // Clicking on the track (outside the thumb) should jump.
        let mut ta = TextArea::new();
        ta.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        let area = Rect::new(0, 0, 20, 5);
        let state = TextAreaState::default();

        // Scroll at 0, thumb near top.  Click at bottom of track (row 4)
        // which should be on the track, not the thumb.
        let action = ta.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 19,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
            state,
        );
        assert_eq!(action, MouseAction::Scrolled);
        // Should have jumped to a non-zero scroll position.
        assert!(
            ta.scroll_override.unwrap_or(0) > 0,
            "track click should jump"
        );
    }

    // ── Clipboard provider tests ──

    #[test]
    fn default_clipboard_provider_round_trips() {
        let mut ta = TextArea::new();
        ta.insert_str("hello world");
        // Select all via set_selection and cut
        ta.set_selection(0, 5);
        ta.input(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        // take_clipboard returns the cut text
        assert_eq!(ta.take_clipboard(), Some("hello".to_string()));
        // Ctrl-V pastes it back
        ta.input(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert_eq!(ta.text(), "hello world");
    }

    #[test]
    fn custom_clipboard_provider() {
        #[derive(Debug)]
        struct TestClip {
            stored: Option<String>,
        }
        impl ClipboardProvider for TestClip {
            fn get(&mut self) -> Option<String> {
                self.stored.clone()
            }
            fn set(&mut self, text: &str) {
                self.stored = Some(format!("CUSTOM:{text}"));
            }
        }

        let mut ta = TextArea::new();
        ta.set_clipboard_provider(Box::new(TestClip { stored: None }));
        ta.insert_str("abc");
        ta.set_selection(0, 3);
        // Ctrl-X should call provider.set with "abc"
        ta.input(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        // Ctrl-V should paste from provider.get → "CUSTOM:abc"
        ta.input(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert_eq!(ta.text(), "CUSTOM:abc");
    }

    #[test]
    fn ctrl_v_pastes_from_provider() {
        #[derive(Debug)]
        struct PreloadedClip;
        impl ClipboardProvider for PreloadedClip {
            fn get(&mut self) -> Option<String> {
                Some("pasted!".to_string())
            }
            fn set(&mut self, _text: &str) {}
        }

        let mut ta = TextArea::new();
        ta.set_clipboard_provider(Box::new(PreloadedClip));
        ta.input(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert_eq!(ta.text(), "pasted!");
    }

    #[test]
    fn copy_on_selection_finalized_sets_provider() {
        // Drag-select → mouse up should call provider.set
        #[derive(Debug)]
        struct RecordingClip {
            last_set: Option<String>,
        }
        impl ClipboardProvider for RecordingClip {
            fn get(&mut self) -> Option<String> {
                self.last_set.clone()
            }
            fn set(&mut self, text: &str) {
                self.last_set = Some(text.to_string());
            }
        }

        let mut ta = TextArea::new();
        ta.set_clipboard_provider(Box::new(RecordingClip { last_set: None }));
        ta.insert_str("hello");
        ta.set_cursor(0);

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Click at 0, drag to 5, release
        ta.handle_mouse(mouse_down(0, 0), area, state);
        ta.handle_mouse(mouse_drag(5, 0), area, state);
        ta.handle_mouse(mouse_up(5, 0), area, state);

        // Now Ctrl-V should paste "hello" (from provider)
        ta.set_cursor(5);
        ta.input(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert_eq!(ta.text(), "hellohello");
    }

    // ── Hover / element event tests ──

    fn mouse_moved(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Moved,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn hover_enter_on_element() {
        let mut ta = TextArea::new();
        ta.insert_str("hi ");
        let id = ta.insert_element("elem", ElementKind(0), None);
        ta.insert_str(" bye");

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Move over plain text — no event
        ta.handle_mouse(mouse_moved(0, 0), area, state);
        assert!(ta.poll_element_event().is_none());

        // Move over element (col 3)
        ta.handle_mouse(mouse_moved(3, 0), area, state);
        let ev = ta.poll_element_event().expect("should emit HoverEnter");
        assert_eq!(ev.id, id);
        assert_eq!(ev.kind, TextElementEventKind::HoverEnter);
    }

    #[test]
    fn hover_leave_on_element() {
        let mut ta = TextArea::new();
        ta.insert_str("hi ");
        let id = ta.insert_element("elem", ElementKind(0), None);
        ta.insert_str(" bye");

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Enter the element
        ta.handle_mouse(mouse_moved(3, 0), area, state);
        ta.poll_element_event(); // consume

        // Leave the element
        ta.handle_mouse(mouse_moved(0, 0), area, state);
        let ev = ta.poll_element_event().expect("should emit HoverLeave");
        assert_eq!(ev.id, id);
        assert_eq!(ev.kind, TextElementEventKind::HoverLeave);
    }

    #[test]
    fn hover_stays_on_same_element_no_event() {
        let mut ta = TextArea::new();
        ta.insert_str("hi ");
        ta.insert_element("elem", ElementKind(0), None);
        ta.insert_str(" bye");

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Enter the element (col 3)
        ta.handle_mouse(mouse_moved(3, 0), area, state);
        ta.poll_element_event(); // consume enter

        // Move within the element (col 4) — no new event
        ta.handle_mouse(mouse_moved(4, 0), area, state);
        assert!(ta.poll_element_event().is_none());
    }

    #[test]
    fn hover_between_two_elements() {
        let mut ta = TextArea::new();
        let id1 = ta.insert_element("AA", ElementKind(0), None);
        ta.insert_str(" ");
        let id2 = ta.insert_element("BB", ElementKind(0), None);
        // Visual: A A   B B
        //         0 1 2 3 4

        let area = Rect::new(0, 0, 40, 5);
        let state = TextAreaState::default();

        // Hover element 1
        ta.handle_mouse(mouse_moved(0, 0), area, state);
        let ev = ta.poll_element_event().unwrap();
        assert_eq!(ev.id, id1);
        assert_eq!(ev.kind, TextElementEventKind::HoverEnter);

        // Move to element 2 — should emit enter for id2
        // (HoverLeave for id1 gets overwritten by HoverEnter for id2)
        ta.handle_mouse(mouse_moved(3, 0), area, state);
        let ev = ta.poll_element_event().unwrap();
        assert_eq!(ev.id, id2);
        assert_eq!(ev.kind, TextElementEventKind::HoverEnter);
    }

    // ── set_scroll_override / scroll_override tests ────────────────────

    #[test]
    fn scroll_override_getter_setter() {
        let mut ta = TextArea::new();
        assert_eq!(ta.scroll_override(), None);
        ta.set_scroll_override(Some(5));
        assert_eq!(ta.scroll_override(), Some(5));
        ta.set_scroll_override(None);
        assert_eq!(ta.scroll_override(), None);
    }

    /// Helper: stateful render (saves typing the full trait path).
    fn render_stateful(ta: &TextArea, area: Rect, buf: &mut Buffer, state: &mut TextAreaState) {
        ratatui::widgets::StatefulWidgetRef::render_ref(&ta, area, buf, state);
    }

    #[test]
    fn scroll_override_forces_viewport_ignoring_cursor() {
        // With 20 lines, cursor at end, and viewport of 5 rows,
        // effective_scroll normally follows the cursor to the bottom.
        // set_scroll_override(Some(0)) should force viewport to top.
        let text = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ta = ta_with(&text);
        ta.set_cursor(ta.text().len()); // cursor at end
        let area = Rect::new(0, 0, 40, 5);
        let mut state = TextAreaState::default();
        let mut buf = Buffer::empty(area);

        // Render without override: cursor-follow scrolls to bottom.
        render_stateful(&ta, area, &mut buf, &mut state);
        assert!(state.scroll > 0, "should scroll to show cursor at end");
        let normal_scroll = state.scroll;

        // Set override to 0 and render: viewport at top despite cursor at end.
        ta.set_scroll_override(Some(0));
        render_stateful(&ta, area, &mut buf, &mut state);
        assert_eq!(state.scroll, 0, "override should force scroll to 0");

        // Clear override: cursor-follow resumes.
        ta.set_scroll_override(None);
        render_stateful(&ta, area, &mut buf, &mut state);
        assert_eq!(
            state.scroll, normal_scroll,
            "clearing override should resume cursor-follow"
        );
    }

    #[test]
    fn scroll_override_clamped_to_max() {
        // Override value larger than max_scroll should be clamped.
        let text = "line 0\nline 1\nline 2"; // 3 lines
        let mut ta = ta_with(text);
        ta.set_cursor(0);
        let area = Rect::new(0, 0, 40, 2); // 2 rows visible, max_scroll = 1
        let mut state = TextAreaState::default();
        let mut buf = Buffer::empty(area);

        ta.set_scroll_override(Some(999));
        render_stateful(&ta, area, &mut buf, &mut state);
        assert_eq!(state.scroll, 1, "override should be clamped to max_scroll");
    }

    #[test]
    fn scroll_override_survives_render_cycles() {
        // The override should persist across multiple renders (unlike
        // mousewheel override which clears on cursor movement).
        let text = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ta = ta_with(&text);
        ta.set_cursor(ta.text().len());
        let area = Rect::new(0, 0, 40, 5);
        let mut state = TextAreaState::default();
        let mut buf = Buffer::empty(area);

        ta.set_scroll_override(Some(3));
        for _ in 0..5 {
            render_stateful(&ta, area, &mut buf, &mut state);
            assert_eq!(state.scroll, 3, "override should persist across renders");
        }
    }

    #[test]
    fn scroll_override_save_restore_round_trip() {
        // Simulates the collapsed-prompt pattern:
        // 1. Render normally (cursor-follow)
        // 2. Save state.scroll + scroll_override
        // 3. Override to 0, render collapsed
        // 4. Restore both → next render shows original viewport
        let text = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ta = ta_with(&text);
        ta.set_cursor(ta.text().len()); // cursor at end
        let area = Rect::new(0, 0, 40, 5);
        let mut state = TextAreaState::default();
        let mut buf = Buffer::empty(area);

        // 1. Initial render — cursor-follow scrolls to bottom.
        render_stateful(&ta, area, &mut buf, &mut state);
        let original_scroll = state.scroll;
        let original_override = ta.scroll_override();
        assert!(original_scroll > 0);
        assert_eq!(original_override, None);

        // 2. "Collapse": override to 0, render a few frames.
        ta.set_scroll_override(Some(0));
        for _ in 0..3 {
            render_stateful(&ta, area, &mut buf, &mut state);
            assert_eq!(state.scroll, 0);
        }

        // 3. Restore both.
        ta.set_scroll_override(original_override);
        state.scroll = original_scroll;

        // 4. Render "uncollapsed" — should show original position.
        render_stateful(&ta, area, &mut buf, &mut state);
        assert_eq!(
            state.scroll, original_scroll,
            "restored scroll should match original"
        );
    }

    #[test]
    fn scroll_override_save_restore_with_mousewheel() {
        // Same as above but the user had mousewheel-scrolled away from cursor
        // before collapse. Both state.scroll and scroll_override must be
        // saved/restored for the viewport to return to its pre-collapse position.
        let text = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ta = ta_with(&text);
        ta.set_cursor(0); // cursor at start
        let area = Rect::new(0, 0, 40, 5);
        let mut state = TextAreaState::default();
        let mut buf = Buffer::empty(area);

        // Render at start, then mousewheel to scroll viewport away from cursor.
        render_stateful(&ta, area, &mut buf, &mut state);
        assert_eq!(state.scroll, 0);
        for _ in 0..5 {
            ta.handle_mouse(mouse_scroll_down(0, 0), area, state);
            render_stateful(&ta, area, &mut buf, &mut state);
        }
        let mousewheel_scroll = state.scroll;
        let mousewheel_override = ta.scroll_override();
        assert!(mousewheel_scroll > 0, "should have scrolled away");
        assert!(mousewheel_override.is_some(), "mousewheel sets override");

        // "Collapse": save both, override to 0.
        let saved_scroll = state.scroll;
        let saved_override = ta.scroll_override();
        ta.set_scroll_override(Some(0));
        for _ in 0..3 {
            render_stateful(&ta, area, &mut buf, &mut state);
            assert_eq!(state.scroll, 0);
        }

        // Restore both.
        ta.set_scroll_override(saved_override);
        state.scroll = saved_scroll;

        // Render "uncollapsed" — viewport should be at the mousewheel position,
        // NOT snapped to cursor (which is at line 0).
        render_stateful(&ta, area, &mut buf, &mut state);
        assert_eq!(
            state.scroll, mousewheel_scroll,
            "viewport should restore to mousewheel position, not snap to cursor"
        );
    }

    #[test]
    fn shifted_character_classification_only_uppercases_letters() {
        for (input, expected) in [
            ('a', 'A'),
            ('z', 'Z'),
            ('A', 'A'),
            ('7', '7'),
            ('/', '/'),
            (';', ';'),
        ] {
            assert_eq!(
                classify_key_event(&KeyEvent::new(KeyCode::Char(input), KeyModifiers::SHIFT)),
                Some(EditCommand::Insert(expected))
            );
        }
    }

    #[test]
    fn modified_delete_and_arrow_keys_keep_word_semantics() {
        for modifiers in [
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT | KeyModifiers::CONTROL,
        ] {
            assert_eq!(
                classify_key_event(&KeyEvent::new(KeyCode::Delete, modifiers)),
                Some(EditCommand::DeleteWordForward(WordStyle::Small)),
            );
            assert_eq!(
                classify_key_event(&KeyEvent::new(KeyCode::Left, modifiers)),
                Some(EditCommand::MoveWordLeft(WordStyle::Small)),
            );
            assert_eq!(
                classify_key_event(&KeyEvent::new(KeyCode::Right, modifiers)),
                Some(EditCommand::MoveWordRight(WordStyle::Small)),
            );
        }
        for modifiers in [KeyModifiers::ALT, KeyModifiers::SUPER] {
            assert_eq!(
                classify_key_event(&KeyEvent::new(KeyCode::Char('d'), modifiers)),
                Some(EditCommand::DeleteWordForward(WordStyle::Small)),
            );
        }
    }

    #[test]
    fn modifier_keys_do_not_insert_text() {
        let mut t = ta_with("hello");
        let len = t.text().len();
        t.input(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(t.text().len(), len);
    }

    #[test]
    fn alt_word_nav_preserved() {
        let mut t = ta_with("hello world");
        t.set_cursor(t.text().len());
        let text = t.text().to_owned();
        t.input(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(t.text(), text);
        assert!(t.cursor() < text.len());

        t.set_cursor(0);
        t.input(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(t.text(), text);
        assert!(t.cursor() > 0);
    }

    #[test]
    fn ctrl_alt_h_deletes_word() {
        let mut t = ta_with("hello world");
        t.set_cursor(t.text().len());
        t.input(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(t.text(), "hello ");
    }

    #[test]
    fn plain_and_shifted_chars_insert() {
        let mut t = TextArea::new();
        for c in ['a', 'z', '1', '/', '@', '{', '!', '~'] {
            t.input(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(t.text(), "az1/@{!~");

        let mut t = TextArea::new();
        t.input(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT));
        t.input(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::SHIFT));
        assert_eq!(t.text(), "AZ");
    }

    #[test]
    fn altgr_char_insertion_platform_dependent() {
        let mut t = TextArea::new();
        t.input(KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        if cfg!(target_os = "windows") {
            assert_eq!(t.text(), "@");
        } else {
            assert_eq!(t.text(), "");
        }
    }

    #[test]
    fn shift_number_trusts_terminal_character() {
        // QWERTZ: terminal sends Char('/') + SHIFT for Shift+7.
        let mut t = TextArea::new();
        t.input(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::SHIFT));
        assert_eq!(t.text(), "/");
    }
}
