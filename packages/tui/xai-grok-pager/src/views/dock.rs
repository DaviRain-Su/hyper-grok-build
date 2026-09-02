//! Consolidated panel dock — the Figma "Exploration" panels layout: one
//! header row per non-empty section (Subagents / Tasks / Watchers / Queued)
//! directly above the prompt, each with a live count and a rule filling the
//! rest of the line. Sections with a zero count are hidden; an all-zero dock
//! renders nothing. Subagents / Tasks / Watchers expand to inline rows with a
//! right-aligned meta column; the Queued section embeds the queue pane as its
//! body.
//!
//! Experimental, gated by remote `dock_enabled` (`enabled`). Keyboard model: the
//! dock cursor walks [`visible_items`] (headers + rows); Enter toggles a
//! section header or opens a subagent row.

use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// Rows shown inline per expanded section before its "N more" line.
pub const MAX_SECTION_ROWS: usize = 2;

/// Left pad for a section body row, past the header chevron so the diamond is the first glyph.
const ROW_INDENT: &str = "    ";
/// One step past [`ROW_INDENT`] so the overflow line nests under the visible rows.
const MORE_INDENT: &str = "      ";

/// Seeded at startup and on `x.ai/settings/update` from Feature::Dock.
static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Release);
}

pub struct DockRow {
    pub kind: String,
    pub description: String,
    pub activity: Option<String>,
    /// Right-aligned meta column, e.g. `grok-4.5 2m14s` or `every 5m`.
    pub meta: String,
    pub killable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Subagents,
    Tasks,
    Watchers,
    Queued,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Subagents => "Subagents",
            Section::Tasks => "Tasks",
            Section::Watchers => "Watchers",
            Section::Queued => "Queued",
        }
    }
}

/// One selectable dock line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DockItem {
    Header(Section),
    /// Row index within its section's rows.
    Row(Section, usize),
}

/// Section lengths + expanded flags — the single source the renderer, the
/// item list, and row hit-testing all derive from, so the cursor and mouse
/// always address what is on screen.
#[derive(Default, Clone, Copy)]
pub struct DockCounts {
    pub subagents: usize,
    pub tasks: usize,
    pub watchers: usize,
    pub queued: usize,
    pub subagents_expanded: bool,
    pub tasks_expanded: bool,
    pub watchers_expanded: bool,
}

impl DockCounts {
    /// `(section, len, expanded)` for the three row sections, render order.
    fn row_sections(self) -> [(Section, usize, bool); 3] {
        [
            (Section::Subagents, self.subagents, self.subagents_expanded),
            (Section::Tasks, self.tasks, self.tasks_expanded),
            (Section::Watchers, self.watchers, self.watchers_expanded),
        ]
    }
}

#[derive(Default)]
pub struct DockData {
    pub subagents: Vec<DockRow>,
    pub tasks: Vec<DockRow>,
    pub watchers: Vec<DockRow>,
    pub queued: usize,
    pub subagents_expanded: bool,
    pub tasks_expanded: bool,
    pub watchers_expanded: bool,
    pub focused: bool,
    /// Cursor over [`visible_items`] while focused.
    pub cursor: usize,
    /// Rows reserved below the Queued header for the embedded queue pane
    /// (rendered by the caller into the tail of the dock rect). Non-zero only
    /// when the Queued section is visible.
    pub queue_body_rows: u16,
}

impl DockData {
    pub fn counts(&self) -> DockCounts {
        DockCounts {
            subagents: self.subagents.len(),
            tasks: self.tasks.len(),
            watchers: self.watchers.len(),
            queued: self.queued,
            subagents_expanded: self.subagents_expanded,
            tasks_expanded: self.tasks_expanded,
            watchers_expanded: self.watchers_expanded,
        }
    }

    fn rows(&self, section: Section) -> &[DockRow] {
        match section {
            Section::Subagents => &self.subagents,
            Section::Tasks => &self.tasks,
            Section::Watchers => &self.watchers,
            Section::Queued => &[],
        }
    }
}

fn shown(len: usize) -> usize {
    len.min(MAX_SECTION_ROWS)
}

/// One painted line in the section stack above the embedded queue body.
enum Visual {
    Header(Section),
    Row(Section, usize),
    More(usize),
}

/// The dock's painted line sequence, in order. `items`, `item_at`,
/// `desired_height`, and `render` all derive from this one walk so the cursor,
/// mouse hit-testing, height, and paint can't drift. The queue body, when
/// present, follows these lines (see [`queue_body_rect`]).
fn visual_rows(c: &DockCounts) -> Vec<Visual> {
    let mut rows = Vec::new();
    for (section, len, expanded) in c.row_sections() {
        if len == 0 {
            continue;
        }
        rows.push(Visual::Header(section));
        if expanded {
            rows.extend((0..shown(len)).map(|i| Visual::Row(section, i)));
            if len > shown(len) {
                rows.push(Visual::More(len - shown(len)));
            }
        }
    }
    if c.queued > 0 {
        rows.push(Visual::Header(Section::Queued));
    }
    rows
}

/// The selectable item at a visual line, if any (`More` lines aren't selectable).
fn as_item(v: &Visual) -> Option<DockItem> {
    match *v {
        Visual::Header(section) => Some(DockItem::Header(section)),
        Visual::Row(section, i) => Some(DockItem::Row(section, i)),
        Visual::More(_) => None,
    }
}

/// Selectable items in render order; the dock cursor indexes this.
pub fn items(c: &DockCounts) -> Vec<DockItem> {
    visual_rows(c).iter().filter_map(as_item).collect()
}

pub fn visible_items(data: &DockData) -> Vec<DockItem> {
    items(&data.counts())
}

/// The selectable item at row offset `row` inside the dock rect, or `None` for
/// a "N more" line or the embedded queue body below the section stack.
pub fn item_at(c: &DockCounts, row: u16) -> Option<DockItem> {
    visual_rows(c).get(row as usize).and_then(as_item)
}

pub fn desired_height(data: &DockData) -> u16 {
    let rows = visual_rows(&data.counts()).len() as u16;
    let queue_body = if data.queued > 0 {
        data.queue_body_rows
    } else {
        0
    };
    rows + queue_body
}

/// Rect for the embedded queue pane body, placed directly below the section
/// content rather than pinned to the dock's bottom. When the dock is clipped
/// shorter than [`desired_height`] the body shrinks and then disappears
/// instead of overwriting the headers above it — so those rows never
/// hit-test as `Queue`.
pub fn queue_body_rect(area: Rect, data: &DockData) -> Rect {
    if data.queued == 0 || data.queue_body_rows == 0 {
        return Rect::default();
    }
    let above = desired_height(data).saturating_sub(data.queue_body_rows);
    let top = area.y.saturating_add(above);
    if top >= area.bottom() {
        return Rect::default();
    }
    Rect {
        y: top,
        height: data.queue_body_rows.min(area.bottom() - top),
        ..area
    }
}

pub fn render(buf: &mut Buffer, area: Rect, theme: &Theme, data: &DockData) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let counts = data.counts();
    let bottom = area.bottom();
    let mut y = area.y;
    let mut item_index = 0usize;
    let selected_at = |idx: usize| data.focused && idx == data.cursor;

    for visual in visual_rows(&counts) {
        if y >= bottom {
            return;
        }
        match visual {
            Visual::Header(section) => {
                let (count, expanded) = match section {
                    Section::Subagents => (counts.subagents, counts.subagents_expanded),
                    Section::Tasks => (counts.tasks, counts.tasks_expanded),
                    Section::Watchers => (counts.watchers, counts.watchers_expanded),
                    Section::Queued => (counts.queued, data.queue_body_rows > 0),
                };
                let line = section_header(theme, area.width, expanded, section.label(), count);
                buf.set_line(area.x, y, &line, area.width);
                if selected_at(item_index) {
                    highlight_row(buf, area, y, theme);
                }
                item_index += 1;
            }
            Visual::Row(section, i) => {
                let selected = selected_at(item_index);
                paint_row(
                    buf,
                    area,
                    y,
                    theme,
                    &data.rows(section)[i],
                    selected,
                    section == Section::Subagents,
                );
                if selected {
                    highlight_row(buf, area, y, theme);
                }
                item_index += 1;
            }
            Visual::More(n) => {
                let line = Line::from(Span::styled(
                    format!("{MORE_INDENT}▾ {n} more"),
                    Style::default().fg(theme.gray),
                ));
                buf.set_line(area.x, y, &line, area.width);
            }
        }
        y += 1;
    }
    // The caller renders the embedded queue pane into the remaining
    // `queue_body_rows` rows below (see `queue_body_rect`).
}

fn highlight_row(buf: &mut Buffer, area: Rect, y: u16, theme: &Theme) {
    for x in area.x..area.x + area.width {
        buf[(x, y)].set_bg(theme.bg_highlight);
    }
}

/// `▾ Subagents 5 ────…`
fn section_header(
    theme: &Theme,
    width: u16,
    expanded: bool,
    label: &str,
    count: usize,
) -> Line<'static> {
    let chevron = if expanded { "▾ " } else { "▸ " };
    let count_text = format!(" {count} ");
    let used = chevron.width() + label.width() + count_text.width();
    let fill = (width as usize).saturating_sub(used);
    Line::from(vec![
        Span::styled(chevron.to_string(), Style::default().fg(theme.gray)),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(theme.gray_bright)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(count_text, Style::default().fg(theme.gray)),
        Span::styled("─".repeat(fill), Style::default().fg(theme.gray_dim)),
    ])
}

/// `    ◆ Kind description — activity` with the right-aligned meta column; the
/// selected row gains `[↗]` (subagents) and `[stop]` (killable) actions.
fn paint_row(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    theme: &Theme,
    row: &DockRow,
    selected: bool,
    openable: bool,
) {
    let accent = Style::default().fg(theme.accent_running);
    let mut spans = vec![
        Span::raw(ROW_INDENT),
        Span::styled(format!("{} ", crate::glyphs::diamond_filled()), accent),
        Span::styled(row.kind.clone(), accent),
        Span::raw(" "),
        Span::styled(
            row.description.clone(),
            Style::default().fg(theme.text_primary),
        ),
    ];
    if let Some(activity) = row.activity.as_deref().filter(|s| !s.is_empty()) {
        spans.push(Span::styled(
            format!(" — {activity}"),
            Style::default().fg(theme.gray),
        ));
    }
    let left = Line::from(spans);
    let left_width = left.width() as u16;
    buf.set_line(area.x, y, &left, area.width);

    let mut meta_spans = vec![Span::styled(
        row.meta.clone(),
        Style::default().fg(theme.gray),
    )];
    if selected {
        if openable {
            meta_spans.push(Span::styled(" [↗]", Style::default().fg(theme.gray_bright)));
        }
        if row.killable {
            meta_spans.push(Span::styled(
                " [stop]",
                Style::default().fg(theme.accent_error),
            ));
        }
    }
    let meta_line = Line::from(meta_spans);
    let meta_width = meta_line.width() as u16;
    if left_width + 1 + meta_width <= area.width {
        let x = area.x + area.width - meta_width;
        buf.set_line(x, y, &meta_line, meta_width);
    }
}

pub fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn row(kind: &str, description: &str, meta: &str, killable: bool) -> DockRow {
        DockRow {
            kind: kind.into(),
            description: description.into(),
            activity: None,
            meta: meta.into(),
            killable,
        }
    }

    fn sample() -> DockData {
        DockData {
            subagents: vec![
                DockRow {
                    kind: "Explore".into(),
                    description: "find dashboard render path".into(),
                    activity: Some("reading render.rs".into()),
                    meta: "grok-4.5 2m14s".into(),
                    killable: true,
                },
                row("General", "fix flaky pty scroll test", "12s", true),
                row("General", "third", "1s", true),
            ],
            tasks: vec![row("Run", "cargo test -p theme (bg)", "12s", true)],
            watchers: vec![
                row("Monitor", "watch build log", "3m01s", true),
                row("Loop", "check CI status", "every 5m", false),
            ],
            queued: 2,
            subagents_expanded: true,
            tasks_expanded: false,
            watchers_expanded: false,
            focused: false,
            cursor: 0,
            queue_body_rows: 0,
        }
    }

    #[test]
    fn all_zero_dock_renders_nothing() {
        let data = DockData::default();
        assert!(visible_items(&data).is_empty());
        assert_eq!(desired_height(&data), 0);
    }

    #[test]
    fn zero_count_sections_are_hidden() {
        let data = DockData {
            queued: 2,
            queue_body_rows: 3,
            ..DockData::default()
        };
        assert_eq!(
            visible_items(&data),
            vec![DockItem::Header(Section::Queued)]
        );
        assert_eq!(desired_height(&data), 4);

        let theme = Theme::tokyonight();
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert!(row_text(&buf, 0).starts_with("▾ Queued 2 ─"), "expanded");
        assert_eq!(queue_body_rect(area, &data), Rect::new(0, 1, 40, 3));
    }

    #[test]
    fn all_sections_render_with_counts_and_collapse_state() {
        let theme = Theme::tokyonight();
        let data = sample();
        assert_eq!(desired_height(&data), 7);

        let area = Rect::new(0, 0, 100, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);

        assert!(row_text(&buf, 0).starts_with("▾ Subagents 3 ─"));
        let first = row_text(&buf, 1);
        let diamond = crate::glyphs::diamond_filled();
        assert!(
            first.starts_with(&format!("{ROW_INDENT}{diamond} Explore")),
            "row must start with indent + diamond, got {first:?}"
        );
        assert!(
            !first.trim_start().starts_with('.'),
            "row must not start with '.', got {first:?}"
        );
        assert!(
            first.contains("Explore find dashboard render path — reading render.rs"),
            "{first}"
        );
        assert!(first.trim_end().ends_with("grok-4.5 2m14s"), "{first}");
        assert!(row_text(&buf, 3).contains("▾ 1 more"));
        assert!(row_text(&buf, 4).starts_with("▸ Tasks 1 ─"));
        assert!(row_text(&buf, 5).starts_with("▸ Watchers 2 ─"));
        assert!(row_text(&buf, 6).starts_with("▸ Queued 2 ─"));
    }

    #[test]
    fn expanded_tasks_and_watchers_show_rows() {
        let theme = Theme::tokyonight();
        let mut data = sample();
        data.subagents_expanded = false;
        data.tasks_expanded = true;
        data.watchers_expanded = true;
        // headers(4) + task row + 2 watcher rows
        assert_eq!(desired_height(&data), 7);

        let area = Rect::new(0, 0, 80, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert!(row_text(&buf, 0).starts_with("▸ Subagents 3 ─"));
        assert!(row_text(&buf, 1).starts_with("▾ Tasks 1 ─"));
        assert!(row_text(&buf, 2).contains("Run cargo test -p theme (bg)"));
        assert!(row_text(&buf, 3).starts_with("▾ Watchers 2 ─"));
        assert!(row_text(&buf, 4).contains("Monitor watch build log"));
        let loop_row = row_text(&buf, 5);
        assert!(loop_row.contains("Loop check CI status"), "{loop_row}");
        assert!(loop_row.trim_end().ends_with("every 5m"), "{loop_row}");
    }

    #[test]
    fn focused_cursor_highlights_and_shows_row_actions() {
        let theme = Theme::tokyonight();
        let mut data = sample();
        data.focused = true;
        data.cursor = 1;
        let area = Rect::new(0, 0, 100, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);

        let first = row_text(&buf, 1);
        assert!(first.trim_end().ends_with("[↗] [stop]"), "{first}");
        assert_eq!(buf[(0, 1)].bg, theme.bg_highlight);

        // A non-killable watcher row shows neither action.
        let mut data = sample();
        data.subagents_expanded = false;
        data.tasks_expanded = false;
        data.watchers_expanded = true;
        data.focused = true;
        data.cursor = 4; // loop row: [subs hdr, tasks hdr, watchers hdr, monitor, loop]
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        let loop_row = row_text(&buf, 4);
        assert!(!loop_row.contains("[stop]"), "{loop_row}");
        assert!(!loop_row.contains("[↗]"), "{loop_row}");
    }

    #[test]
    fn item_at_maps_rows_and_skips_more_lines() {
        let data = sample();
        let c = data.counts();
        assert_eq!(item_at(&c, 0), Some(DockItem::Header(Section::Subagents)));
        assert_eq!(item_at(&c, 1), Some(DockItem::Row(Section::Subagents, 0)));
        assert_eq!(item_at(&c, 2), Some(DockItem::Row(Section::Subagents, 1)));
        assert_eq!(item_at(&c, 3), None, "the N-more line");
        assert_eq!(item_at(&c, 4), Some(DockItem::Header(Section::Tasks)));
        assert_eq!(item_at(&c, 5), Some(DockItem::Header(Section::Watchers)));
        assert_eq!(item_at(&c, 6), Some(DockItem::Header(Section::Queued)));
        assert_eq!(item_at(&c, 7), None, "past the end / queue body");
    }

    #[test]
    fn meta_is_dropped_when_the_row_is_too_narrow() {
        let theme = Theme::tokyonight();
        let data = sample();
        let area = Rect::new(0, 0, 30, 7);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme, &data);
        assert!(!row_text(&buf, 1).contains("grok-4.5"));
    }
}
