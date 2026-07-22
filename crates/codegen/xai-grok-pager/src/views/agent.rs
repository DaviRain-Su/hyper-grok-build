//! Agent view layout and rendering helpers.
//!
//! This module provides:
//! - [`AgentViewLayout`] — pure layout computation (screen area → pane rects)
//! - [`ActivePane`] / [`PaneAreas`] — pane identity and hit-testing
//! - Overlay helpers — small focused functions for selection/hover chrome
//! - [`build_hints`] — shortcuts bar hint generation
//!
//! The actual rendering orchestration lives in [`AgentView::draw()`](crate::app::agent_view::AgentView::draw),
//! which calls shared widgets (StatusBar, ScrollbackPane, PromptWidget,
//! ShortcutsBar) and uses these helpers for the agent-specific glue.
use crate::actions::{ActionId, ActionRegistry, When};
use crate::appearance::{LayoutConfig, ScrollbarConfig};
use crate::render::SafeBuf;
use crate::render::scrollbar::render_scrollbar_styled;
use crate::scrollback::layout::HorizontalLayout;
use crate::scrollback::search::ScrollbackSearchState;
use crate::scrollback::selection::SelectionBox;
use crate::scrollback::state::ScrollbackState;
use crate::theme::Theme;
use crate::views::prompt_widget::PromptWidget;
use crate::views::shortcuts_bar::HintItem;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, Padding, Widget};
/// Which pane is currently active in the agent view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivePane {
    #[default]
    Scrollback,
    Todo,
    Queue,
    Prompt,
    Tasks,
    Catalog,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum InputMode {
    #[default]
    Vim,
    Simple,
}
/// Cached pane areas from the last render, used for mouse hit-testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneAreas {
    pub scrollback: Rect,
    pub todo: Rect,
    pub queue: Rect,
    pub prompt: Rect,
    pub tasks: Rect,
    pub catalog: Rect,
}
impl PaneAreas {
    /// Determine which pane a screen position falls in, if any.
    pub fn hit_test(&self, col: u16, row: u16) -> Option<ActivePane> {
        let pos = (col, row).into();
        if self.tasks.area() > 0 && self.tasks.contains(pos) {
            return Some(ActivePane::Tasks);
        }
        if self.catalog.area() > 0 && self.catalog.contains(pos) {
            return Some(ActivePane::Catalog);
        }
        if self.todo.area() > 0 && self.todo.contains(pos) {
            return Some(ActivePane::Todo);
        }
        if self.queue.area() > 0 && self.queue.contains(pos) {
            return Some(ActivePane::Queue);
        }
        if self.scrollback.contains(pos) {
            return Some(ActivePane::Scrollback);
        }
        if self.prompt.contains(pos) {
            return Some(ActivePane::Prompt);
        }
        None
    }
}
/// Terminals at or below this height suppress the optional rows above the
/// prompt (plugin CTA, follow-ups, banner/tip) so the prompt and scrollback
/// are never starved.
pub const SHORT_TERMINAL_ROWS: u16 = 16;
/// Auto-compact threshold: at or below this height the render-value compact
/// flag is forced on. Deliberately above [`SHORT_TERMINAL_ROWS`], which stays
/// the hard-degradation gate (tip-row renderability, CTA/follow-up trims).
pub const AUTO_COMPACT_MAX_ROWS: u16 = 20;
const _: () = assert!(SHORT_TERMINAL_ROWS < AUTO_COMPACT_MAX_ROWS);
/// Render-value derivation for compact mode: the user setting, force-enabled
/// while the terminal is [`AUTO_COMPACT_MAX_ROWS`] or shorter (auto-compact).
///
/// Derived only — never persisted and never written back to the user setting
/// (`current_ui.compact_mode` / the render cache / disk), so growing the
/// window restores the user's choice. `terminal_rows == 0` means "not yet
/// measured" and never forces compact.
pub fn effective_compact(user_compact: bool, terminal_rows: u16) -> bool {
    user_compact || (terminal_rows > 0 && terminal_rows <= AUTO_COMPACT_MAX_ROWS)
}
/// Computed screen layout for the agent view.
///
/// Pure data — no rendering. Computed from screen area + appearance config +
/// prompt height + todo height. Shared widgets use these rects to render into.
pub struct AgentViewLayout {
    pub status_bar: Rect,
    /// Startup terminal-warning banner (between status bar and bg tasks/scrollback).
    pub startup_warnings: Rect,
    pub tasks: Rect,
    pub catalog: Rect,
    pub scrollback: Rect,
    pub todo: Rect,
    pub queue: Rect,
    /// Inline /btw side question panel (above queue / turn status / prompt).
    pub btw: Rect,
    pub turn_status: Rect,
    /// Banner rect above the prompt (mode-switch banner, ephemeral tips).
    pub banner: Rect,
    /// Inline plugin-CTA row (below banner, above the prompt gap).
    pub plugin_cta: Rect,
    /// Follow-up suggestion chips row (below the plugin CTA, above the prompt).
    pub follow_ups: Rect,
    /// Single-row record indicator ("◉ Recording") directly above the prompt,
    /// shown only while voice capture is active.
    pub voice_recording: Rect,
    pub prompt: Rect,
    pub shortcuts: Rect,
    /// Scrollback area narrowed for scrollbar (content rendering uses this).
    pub scrollback_content: Rect,
    /// Scrollbar track position (x coordinate).
    pub scrollbar_x: u16,
    /// Timeline rail left edge (only meaningful when `timeline_width > 0`;
    /// the rail's right edge lands on the scrollbar column, which the rail
    /// replaces).
    pub timeline_x: u16,
    /// Columns reserved for the timeline rail (0 = rail hidden). Non-zero
    /// also means the scrollbar does not render this frame.
    pub timeline_width: u16,
}
impl AgentViewLayout {
    /// Compute layout from screen area, appearance config, prompt height,
    /// todo pane height, turn status height, and prompt gap.
    ///
    /// When `todo_height` is 0, the todo pane and its gap are omitted.
    /// When `turn_status_height` is 0, the turn status line is omitted.
    /// When `banner_height` is 0, the reserved banner row is omitted.
    /// When `cta_height` is 0, the plugin-CTA row is omitted. It is also
    /// forced to 0 on short terminals (`area.height <= SHORT_TERMINAL_ROWS`)
    /// so the prompt and scrollback are never starved.
    /// When `follow_ups_height` is 0, the follow-up chips row is omitted; it
    /// is force-suppressed on short terminals on the same `SHORT_TERMINAL_ROWS`
    /// rule as the CTA row.
    /// When `startup_warning_height` is 0, the startup warning area is omitted.
    /// `prompt_gap` is 0 or 1 — controls the gap row between turn status
    /// (or scrollback) and the prompt widget.
    /// `timeline_width` reserves rail columns for the timeline sidebar in
    /// place of the scrollbar (0 = hidden); it requires the scrollbar's
    /// gutter geometry, so a disabled scrollbar forces it to 0.
    #[allow(clippy::too_many_arguments)]
    pub fn compute(
        area: Rect,
        layout_cfg: &LayoutConfig,
        scrollbar_cfg: &ScrollbarConfig,
        timeline_width: u16,
        prompt_height: u16,
        tasks_height: u16,
        catalog_height: u16,
        todo_height: u16,
        queue_height: u16,
        btw_height: u16,
        turn_status_height: u16,
        banner_height: u16,
        cta_height: u16,
        follow_ups_height: u16,
        startup_warning_height: u16,
        prompt_gap: u16,
        voice_recording_height: u16,
        shortcuts_height: u16,
        compact: bool,
    ) -> Self {
        let outer_vpad = layout_cfg.eff_outer_vpad(compact);
        let bottom_vpad = if area.height <= SHORT_TERMINAL_ROWS {
            0
        } else {
            outer_vpad
        };
        let cta_height = if area.height <= SHORT_TERMINAL_ROWS {
            0
        } else {
            cta_height
        };
        let follow_ups_height = if area.height <= SHORT_TERMINAL_ROWS {
            0
        } else {
            follow_ups_height
        };
        let top_vpad = outer_vpad;
        let outer_block = Block::default().padding(Padding::new(
            layout_cfg.eff_hpad_left(compact),
            layout_cfg.eff_hpad_right(compact),
            top_vpad,
            bottom_vpad,
        ));
        let inner_area = outer_block.inner(area);
        let mut constraints = vec![Constraint::Length(1)];
        if startup_warning_height > 0 {
            constraints.push(Constraint::Length(startup_warning_height));
        }
        let pane_gap = if top_vpad == 0 { 0u16 } else { 1 };
        if tasks_height > 0 {
            constraints.push(Constraint::Length(pane_gap));
            constraints.push(Constraint::Length(tasks_height));
        }
        if catalog_height > 0 {
            constraints.push(Constraint::Length(pane_gap));
            constraints.push(Constraint::Length(catalog_height));
        }
        if todo_height > 0 {
            constraints.push(Constraint::Length(pane_gap));
            constraints.push(Constraint::Length(todo_height));
        }
        let status_gap = if top_vpad == 0 { 0u16 } else { 1 };
        constraints.push(Constraint::Length(status_gap));
        constraints.push(Constraint::Min(5));
        if btw_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(btw_height));
        }
        if queue_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(queue_height));
        }
        if turn_status_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(turn_status_height));
        }
        if banner_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(banner_height));
        }
        if cta_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(cta_height));
        }
        if follow_ups_height > 0 {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(follow_ups_height));
        }
        if prompt_gap > 0 {
            constraints.push(Constraint::Length(prompt_gap));
        }
        if voice_recording_height > 0 {
            constraints.push(Constraint::Length(voice_recording_height));
        }
        constraints.push(Constraint::Length(prompt_height));
        let shortcuts_gap = if bottom_vpad == 0 { 0u16 } else { 1 };
        if shortcuts_gap > 0 {
            constraints.push(Constraint::Length(shortcuts_gap));
        }
        constraints.push(Constraint::Length(shortcuts_height));
        let chunks = Layout::vertical(constraints).split(inner_area);
        let mut i = 0;
        let status_bar = chunks[i];
        i += 1;
        let startup_warnings = if startup_warning_height > 0 {
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        let tasks = if tasks_height > 0 {
            i += 1;
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        let catalog = if catalog_height > 0 {
            i += 1;
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        let todo = if todo_height > 0 {
            i += 1;
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        i += 1;
        let scrollback = chunks[i];
        i += 1;
        let btw = if btw_height > 0 {
            i += 1;
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        let queue = if queue_height > 0 {
            i += 1;
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        let turn_status = if turn_status_height > 0 {
            i += 1;
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        let banner = if banner_height > 0 {
            i += 1;
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        let plugin_cta = if cta_height > 0 {
            i += 1;
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        let follow_ups = if follow_ups_height > 0 {
            i += 1;
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        if prompt_gap > 0 {
            i += 1;
        }
        let voice_recording = if voice_recording_height > 0 {
            let r = chunks[i];
            i += 1;
            r
        } else {
            Rect::default()
        };
        let prompt = chunks[i];
        i += 1;
        if shortcuts_gap > 0 {
            i += 1;
        }
        let shortcuts = chunks[i];
        let scrollbar_x = area.right().saturating_sub(scrollbar_cfg.gap_right + 1);
        let timeline_width = if scrollbar_cfg.enabled {
            timeline_width
        } else {
            0
        };
        let timeline_x = (scrollbar_x + 1).saturating_sub(timeline_width);
        let content_end_x = if timeline_width > 0 {
            timeline_x.saturating_sub(scrollbar_cfg.gap_left)
        } else {
            scrollbar_x.saturating_sub(scrollbar_cfg.gap_left)
        };
        let scrollback_right = scrollback.x + scrollback.width;
        let scrollback_content = if !scrollbar_cfg.enabled || content_end_x >= scrollback_right {
            scrollback
        } else {
            Rect {
                width: content_end_x.saturating_sub(scrollback.x),
                ..scrollback
            }
        };
        Self {
            status_bar,
            startup_warnings,
            tasks,
            catalog,
            scrollback,
            todo,
            queue,
            btw,
            turn_status,
            banner,
            plugin_cta,
            follow_ups,
            voice_recording,
            prompt,
            shortcuts,
            scrollback_content,
            scrollbar_x,
            timeline_x,
            timeline_width,
        }
    }
    /// Inner area width (for prompt height computation before full layout).
    ///
    /// This computes just the inner width without the full layout split,
    /// since prompt height is needed as input to `compute()`.
    pub fn inner_width(area: Rect, layout_cfg: &LayoutConfig, compact: bool) -> u16 {
        let vpad = layout_cfg.eff_outer_vpad(compact);
        let outer_block = Block::default().padding(Padding::new(
            layout_cfg.eff_hpad_left(compact),
            layout_cfg.eff_hpad_right(compact),
            vpad,
            vpad,
        ));
        outer_block.inner(area).width
    }
    /// Convert to PaneAreas for mouse hit-testing.
    pub fn pane_areas(&self) -> PaneAreas {
        PaneAreas {
            scrollback: self.scrollback,
            todo: self.todo,
            queue: self.queue,
            prompt: self.prompt,
            tasks: self.tasks,
            catalog: self.catalog,
        }
    }
}
/// Fill the screen area with base background and outer padding.
pub fn fill_background(
    buf: &mut Buffer,
    area: Rect,
    layout_cfg: &LayoutConfig,
    compact: bool,
    theme: &Theme,
) {
    buf.set_style(area, Style::default().bg(theme.bg_base));
    let vpad = layout_cfg.eff_outer_vpad(compact);
    let outer_styled = Block::default()
        .padding(Padding::new(
            layout_cfg.eff_hpad_left(compact),
            layout_cfg.eff_hpad_right(compact),
            vpad,
            vpad,
        ))
        .style(Style::default().bg(theme.bg_base));
    outer_styled.render(area, buf);
}
/// Render follow-up suggestion chips into a single row, returning the
/// clickable rect of each rendered chip.
///
/// Chips render left-to-right and rendering STOPS at the first chip that does
/// not fit the row width — the result is a rendered prefix of `suggestions`
/// (index-aligned), not a filtered subset. A transient, mouse-clickable
/// affordance above the prompt — the same row slot family as the plugin CTA.
/// Suggestion text is server-controlled and already sanitized at ingestion;
/// here it is additionally length-clamped per chip and written through
/// `set_span_safe`, so a label can neither overflow the row nor inject
/// terminal escape sequences.
///
/// `hovered` highlights the chip under the mouse (`theme.bg_hover` + primary text).
pub(crate) fn render_follow_ups(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    suggestions: &[String],
    hovered: Option<usize>,
) -> Vec<Rect> {
    use crate::render::line_utils::truncate_str;
    use unicode_width::UnicodeWidthStr;
    let mut rects = Vec::new();
    if area.height == 0 || area.width == 0 {
        return rects;
    }
    for col in 0..area.width {
        if let Some(cell) = buf.cell_mut((area.x + col, area.y)) {
            cell.set_char(' ');
            cell.fg = theme.text_secondary;
            cell.bg = theme.bg_base;
        }
    }
    const MAX_CHIP_LABEL: usize = 48;
    let chip_style = Style::default().fg(theme.link_fg);
    let hover_style = Style::default().fg(theme.text_primary).bg(theme.bg_hover);
    let row_end = area.x + area.width;
    let mut x = area.x;
    for (i, label) in suggestions.iter().enumerate() {
        let label = truncate_str(label, MAX_CHIP_LABEL);
        let chip = format!("[ {label} ]");
        let chip_w = chip.width() as u16;
        if x >= row_end || chip_w > row_end - x {
            break;
        }
        let style = if hovered == Some(i) {
            hover_style
        } else {
            chip_style
        };
        buf.set_span_safe(x, area.y, &Span::styled(chip, style), chip_w);
        rects.push(Rect::new(x, area.y, chip_w, 1));
        x = x.saturating_add(chip_w).saturating_add(1);
    }
    rects
}
/// Render hover box around a scrollback entry (dimmed selection border).
///
/// Skipped if the hovered entry is also the selected entry (selection box wins).
pub fn render_entry_hover(
    buf: &mut Buffer,
    scrollback_area: Rect,
    scrollback: &ScrollbackState,
    hovered_entry: Option<usize>,
    theme: &Theme,
) {
    let Some(hover_idx) = hovered_entry else {
        return;
    };
    if scrollback.selected() == Some(hover_idx) {
        return;
    }
    let split_mode = scrollback
        .appearance()
        .scrollback
        .display
        .group_selection_split;
    let group = scrollback.group_range_of(hover_idx, split_mode);
    if group.len() <= 1 {
        if let Some((area, top_clipped, bottom_clipped)) =
            scrollback.entry_screen_area(hover_idx, scrollback_area)
        {
            SelectionBox::new(area, Style::default().fg(theme.hover_border))
                .with_top_clipped(top_clipped)
                .with_bottom_clipped(bottom_clipped)
                .render(buf);
        }
    } else {
        let first_area = scrollback.entry_screen_area(group.start, scrollback_area);
        let last_area = scrollback.entry_screen_area(group.end - 1, scrollback_area);
        if let (
            Some((first_rect, first_top_clipped, _)),
            Some((last_rect, _, last_bottom_clipped)),
        ) = (first_area, last_area)
        {
            let y_start = first_rect.y;
            let y_end = last_rect.y + last_rect.height;
            let combined = Rect::new(first_rect.x, y_start, first_rect.width, y_end - y_start);
            SelectionBox::new(combined, Style::default().fg(theme.hover_border))
                .with_top_clipped(first_top_clipped)
                .with_bottom_clipped(last_bottom_clipped)
                .render(buf);
        }
    }
}
/// Render a floating popup showing hook details when hovering over
/// a collapsed tool call entry that has hooks.
pub fn render_hook_hover_popup(
    buf: &mut Buffer,
    scrollback_area: Rect,
    scrollback: &ScrollbackState,
    hovered_entry: Option<usize>,
    mouse_pos: (u16, u16),
    theme: &Theme,
) {
    let Some(hover_idx) = hovered_entry else {
        return;
    };
    let Some(entry) = scrollback.get(hover_idx) else {
        return;
    };
    if entry.display_mode != crate::scrollback::types::DisplayMode::Collapsed {
        return;
    }
    let Some(ref hd) = entry.hook_data else {
        return;
    };
    if !hd.has_content() {
        return;
    }
    use crate::scrollback::blocks::tool::hook::render_hooks_for_mode;
    let mode = crate::scrollback::types::DisplayMode::Expanded;
    let mut lines = Vec::new();
    let pre = render_hooks_for_mode("pre_tool_use", &hd.pre_hooks, mode);
    let post = render_hooks_for_mode("post_tool_use", &hd.post_hooks, mode);
    lines.extend(pre);
    lines.extend(post);
    for (event_name, runs) in &hd.lifecycle {
        lines.extend(render_hooks_for_mode(event_name, runs, mode));
    }
    if lines.is_empty() {
        return;
    }
    let Some((entry_area, _, _)) = scrollback.entry_screen_area(hover_idx, scrollback_area) else {
        return;
    };
    let (mouse_col, mouse_row) = mouse_pos;
    if mouse_row < entry_area.y || mouse_row >= entry_area.y + entry_area.height {
        return;
    }
    let badge_row = entry_area.y;
    let row_start = scrollback_area.x;
    let row_end = scrollback_area.x + scrollback_area.width;
    let row_text: String = (row_start..row_end)
        .filter_map(|col| {
            buf.cell(ratatui::layout::Position::new(col, badge_row))
                .map(|c| c.symbol().to_string())
        })
        .collect();
    let Some(badge_start_in_text) = row_text.find("[hooks:") else {
        return;
    };
    let badge_end_in_text = row_text[badge_start_in_text..]
        .find(']')
        .map(|i| badge_start_in_text + i + 1)
        .unwrap_or(row_text.len());
    let badge_start_col = row_start + badge_start_in_text as u16;
    let badge_end_col = row_start + badge_end_in_text as u16;
    if mouse_row != badge_row || mouse_col < badge_start_col || mouse_col >= badge_end_col {
        return;
    }
    let buf_height = buf.area.height;
    let buf_width = buf.area.width;
    let popup_height = (lines.len() as u16 + 2).min(15).min(buf_height);
    let popup_width = scrollback_area
        .width
        .saturating_sub(8)
        .max(40)
        .min(buf_width);
    let popup_y = if entry_area.y + entry_area.height + popup_height
        <= scrollback_area.y + scrollback_area.height
    {
        entry_area.y + entry_area.height
    } else {
        entry_area.y.saturating_sub(popup_height)
    };
    let popup_x = entry_area.x + 4;
    let popup_area = Rect::new(
        popup_x.min(scrollback_area.x + scrollback_area.width.saturating_sub(popup_width)),
        popup_y,
        popup_width,
        popup_height.min(buf_height.saturating_sub(popup_y)),
    );
    let bg = theme.bg_base;
    let clear_style = ratatui::style::Style::default().bg(bg);
    for y in popup_area.y..popup_area.y + popup_area.height {
        for x in popup_area.x..popup_area.x + popup_area.width {
            if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(x, y)) {
                cell.reset();
                cell.set_style(clear_style);
            }
        }
    }
    let border_style = ratatui::style::Style::default().fg(theme.gray);
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(border_style)
        .style(ratatui::style::Style::default().bg(bg));
    let inner = block.inner(popup_area);
    ratatui::widgets::Widget::render(block, popup_area, buf);
    for (i, line) in lines.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        buf.set_line_safe(inner.x, y, &line.content, inner.width);
    }
}
/// Selection/hover chrome for a side pane (todo / queue / tasks). Focused panes get a dismiss control.
pub fn render_todo_chrome(
    buf: &mut Buffer,
    todo_area: Rect,
    layout_cfg: &LayoutConfig,
    focused: bool,
    hovered: bool,
    close_hovered: bool,
    theme: &Theme,
) -> Option<SelectionBox> {
    render_todo_chrome_with_close_label(
        buf,
        todo_area,
        layout_cfg,
        focused,
        hovered,
        close_hovered,
        theme,
        None,
    )
}
/// Like [`render_todo_chrome`], with optional close label (queue uses `[close]`).
#[allow(clippy::too_many_arguments)]
pub fn render_todo_chrome_with_close_label(
    buf: &mut Buffer,
    todo_area: Rect,
    layout_cfg: &LayoutConfig,
    focused: bool,
    hovered: bool,
    close_hovered: bool,
    theme: &Theme,
    close_label: Option<&'static str>,
) -> Option<SelectionBox> {
    if todo_area.area() == 0 {
        return None;
    }
    let color = if focused {
        theme.selection_border
    } else if hovered {
        theme.hover_border
    } else {
        return None;
    };
    let layout = HorizontalLayout::new(todo_area, layout_cfg);
    let mut sel = SelectionBox::new(layout.selection_area(), Style::default().fg(color))
        .with_closable(focused, close_hovered);
    if focused && let Some(label) = close_label {
        sel = sel.with_close_label(Some(label));
    }
    sel.render(buf);
    Some(sel)
}
/// Render the scrollbar track and thumb.
///
/// When `is_following` is true, the scrollbar thumb is dimmed to indicate
/// the viewport is locked to the bottom. This makes G / follow state
/// immediately visible in the scrollbar.
pub fn render_scrollbar(
    buf: &mut Buffer,
    scrollback_area: Rect,
    scrollbar_x: u16,
    scrollbar_cfg: &ScrollbarConfig,
    scroll_info: Option<crate::scrollback::selection::ScrollInfo>,
    is_following: bool,
    theme: &Theme,
) {
    if scrollbar_cfg.enabled
        && let Some(scroll_info) = scroll_info
    {
        let scrollbar_area = Rect {
            x: scrollbar_x,
            y: scrollback_area.y,
            width: 1,
            height: scrollback_area.height,
        };
        let scrollbar_bg = scrollbar_cfg.scrollbar_bg.unwrap_or(theme.scrollbar_bg);
        let scrollbar_fg = scrollbar_cfg.scrollbar_fg.unwrap_or(theme.scrollbar_fg);
        let thumb_fg = if is_following {
            crate::render::color::blend_color(scrollbar_bg, scrollbar_fg, 0.4)
                .unwrap_or(scrollbar_fg)
        } else {
            scrollbar_fg
        };
        let track_style = Style::default().bg(scrollbar_bg);
        let thumb_style = Style::default().fg(thumb_fg).bg(scrollbar_bg);
        let scale = if scroll_info.total_height > u16::MAX as usize {
            (scroll_info.total_height / u16::MAX as usize) + 1
        } else {
            1
        };
        let scaled_total = (scroll_info.total_height / scale) as u16;
        let scaled_offset = (scroll_info.scroll_offset / scale) as u16;
        render_scrollbar_styled(
            buf,
            Some(scrollbar_area),
            scaled_total,
            scroll_info.viewport_height,
            scaled_offset,
            track_style,
            thumb_style,
        );
    }
}
use crate::appearance::TodoBadgeFormat;
/// Build the todo badge as styled spans (without rendering).
///
/// Returns `None` if there are no items to display.
pub fn render_todo_badge_spans(
    counts: &super::todo_pane::TodoCounts,
    hovered: bool,
    flash: bool,
    format: TodoBadgeFormat,
    theme: &Theme,
) -> Option<Vec<Span<'static>>> {
    use ratatui::style::Modifier;
    if counts.total() == 0 {
        return None;
    }
    let highlighted = hovered || flash;
    let count_style = if highlighted {
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_secondary).bg(theme.bg_base)
    };
    let dim_style = if highlighted {
        Style::default().fg(theme.text_secondary).bg(theme.bg_base)
    } else {
        Style::default().fg(theme.gray_dim).bg(theme.bg_base)
    };
    if matches!(format, TodoBadgeFormat::Default) {
        let total = counts.total_excluding_cancelled();
        if total == 0 {
            return None;
        }
        return Some(vec![
            Span::styled(counts.completed.to_string(), count_style),
            Span::styled("/", dim_style),
            Span::styled(total.to_string(), count_style),
            Span::styled(
                format!(" {}", crate::glyphs::check_mark()),
                Style::default().fg(theme.text_secondary).bg(theme.bg_base),
            ),
        ]);
    }
    let icon_mod = if highlighted {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    let statuses: [(usize, &str, ratatui::style::Color); 4] = [
        (counts.in_progress, "▶", theme.warning),
        (counts.pending, "□", theme.text_secondary),
        (
            counts.completed,
            crate::glyphs::check_mark(),
            theme.accent_success,
        ),
        (
            counts.cancelled,
            crate::glyphs::ballot_x(),
            theme.accent_error,
        ),
    ];
    let parts: Vec<(usize, &str, ratatui::style::Color)> =
        statuses.into_iter().filter(|(n, _, _)| *n > 0).collect();
    if parts.is_empty() {
        return None;
    }
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(parts.len() * 4);
    for (i, (count, icon, color)) in parts.iter().enumerate() {
        let icon_style = Style::default()
            .fg(*color)
            .bg(theme.bg_base)
            .add_modifier(icon_mod);
        let num_style = match format {
            TodoBadgeFormat::Default => {
                let mut s = Style::default().fg(*color).bg(theme.bg_base);
                if highlighted {
                    s = s.add_modifier(Modifier::BOLD);
                }
                s
            }
            _ => count_style,
        };
        match format {
            TodoBadgeFormat::Default => {
                if i > 0 {
                    spans.push(Span::styled(" ", dim_style));
                }
                spans.push(Span::styled(format!("{count}"), num_style));
            }
            TodoBadgeFormat::Colon => {
                if i > 0 {
                    spans.push(Span::styled(" ", dim_style));
                }
                spans.push(Span::styled(*icon, icon_style));
                spans.push(Span::styled(":", dim_style));
                spans.push(Span::styled(format!("{count}"), num_style));
            }
            TodoBadgeFormat::Comma => {
                if i > 0 {
                    spans.push(Span::styled(", ", dim_style));
                }
                spans.push(Span::styled(format!("{count}"), num_style));
                spans.push(Span::styled(" ", dim_style));
                spans.push(Span::styled(*icon, icon_style));
            }
        }
    }
    spans.push(Span::styled(
        format!(" {}", crate::glyphs::check_mark()),
        Style::default().fg(theme.text_secondary).bg(theme.bg_base),
    ));
    Some(spans)
}
/// Space:prompt hint — shared across multiple scrollback hint branches.
fn space_prompt_hint() -> HintItem {
    use crate::input::key::KeyShortcut;
    use crossterm::event::{KeyCode, KeyModifiers};
    HintItem {
        keys: vec![KeyShortcut::new(KeyCode::Char(' '), KeyModifiers::NONE)],
        label: "prompt".into(),
        custom_display: Some("Space"),
        description: None,
        pinned: false,
    }
}
/// Build the hints list for the shortcuts bar based on current state.
///
/// Each pane contributes its own hints dynamically. The registry provides
/// the key bindings; the view decides which ones are visible.
///
/// `fold_label` is the dynamic label for the fold action based on selected
/// entry state: "expand", "collapse", or "fold" (no foldable entry selected).
///
/// `group_header_label` ("expand"/"collapse") marks a selected group header;
/// it replaces the fold and Enter:open hints with a single Enter toggle hint.
#[allow(clippy::too_many_arguments)]
pub fn build_hints(
    active_pane: ActivePane,
    prompt: &PromptWidget,
    registry: &ActionRegistry,
    is_editing_queued: bool,
    fold_label: Option<&'static str>,
    group_header_label: Option<&'static str>,
    thinking_label: &'static str,
    show_done: bool,
    selected_supports_copy: bool,
    selected_meta_label: Option<&'static str>,
    selected_supports_fullscreen: bool,
    can_demote: bool,
    selected_can_kill: bool,
    multiline_mode: bool,
    vim_mode: bool,
    is_subagent_view: bool,
    is_turn_running: bool,
    has_queued_follow_up: bool,
    selected_is_user_prompt: bool,
    selected_is_agent_message: bool,
    selected_is_credit_limit: bool,
    shift_enter_unavailable: bool,
    scrollback_search: Option<&ScrollbackSearchState>,
) -> Vec<HintItem> {
    let mut hints = match active_pane {
        ActivePane::Todo => {
            let mut hints = Vec::new();
            hints.push(HintItem::new(
                crate::key!('h'),
                if show_done { "hide done" } else { "show done" },
            ));
            hints
        }
        ActivePane::Queue => {
            let mut hints = vec![
                HintItem::new(crate::key!('x'), "delete row"),
                HintItem::new(crate::key!('e'), "edit"),
                HintItem::paired(crate::key!('J'), crate::key!('K'), "reorder"),
                HintItem::new(crate::key!('y'), "copy"),
            ];
            if is_turn_running && let Some(def) = registry.find(ActionId::InterjectPrompt) {
                hints.push(def.hint());
            }
            hints
        }
        ActivePane::Prompt if is_editing_queued => {
            let mut hints = Vec::new();
            if prompt.can_send() {
                hints.push(HintItem::new(crate::key!(Enter), "save"));
            }
            hints.push(HintItem::new(crate::key!(Esc), "cancel"));
            hints
        }
        ActivePane::Prompt if prompt.history_search.is_active() => {
            use crate::input::key::KeyShortcut;
            use crossterm::event::KeyCode;
            vec![
                HintItem::paired(
                    KeyShortcut::key(KeyCode::Up),
                    KeyShortcut::key(KeyCode::Down),
                    "nav",
                ),
                HintItem::paired(
                    KeyShortcut::key(KeyCode::PageUp),
                    KeyShortcut::key(KeyCode::PageDown),
                    "page",
                ),
                HintItem::new(KeyShortcut::key(KeyCode::Enter), "select"),
                HintItem::new(KeyShortcut::key(KeyCode::Esc), "cancel"),
            ]
        }
        ActivePane::Prompt => {
            let mut hints = Vec::new();
            let newline_key = if shift_enter_unavailable {
                crate::key!(Enter, ALT)
            } else {
                crate::key!(Enter, SHIFT)
            };
            let submit_label = if is_turn_running { "queue" } else { "send" };
            if let Some(key) = registry.key_for(ActionId::SendPrompt) {
                if prompt.paste_element_at_cursor().is_some() {
                    hints.push(HintItem::new(key, "expand"));
                } else if multiline_mode && prompt.can_send() {
                    hints.push(HintItem::new(newline_key, submit_label));
                } else if prompt.can_send() {
                    hints.push(HintItem::new(key, submit_label));
                } else if is_turn_running && has_queued_follow_up {
                    hints.push(HintItem::new(key, "send now"));
                }
            }
            if shift_enter_unavailable && !multiline_mode && prompt.can_send() {
                hints.push(HintItem::new(crate::key!(Enter, ALT), "newline"));
            }
            if prompt.file_ref_near_cursor() {
                hints.push(HintItem::new(crate::key!(':'), "lines"));
            }
            if prompt.prompt_suggestion_visible() {
                hints.push(
                    HintItem::paired(crate::key!(Tab), crate::key!(Right), "accept suggestion")
                        .pinned(),
                );
            }
            hints.push(HintItem::new(crate::key!(BackTab), "mode"));
            for def in registry.hints(&[When::PromptFocused, When::AgentScreen, When::Always]) {
                if def.id == ActionId::SendPrompt
                    || def.id == ActionId::CommandPalette
                    || def.id == ActionId::Quit
                {
                    continue;
                }
                if def.id == ActionId::EnableVoiceMode || def.id == ActionId::VoiceToggle {
                    continue;
                }
                hints.push(def.hint());
            }
            hints
        }
        ActivePane::Tasks => {
            let mut hints = Vec::new();
            if selected_supports_fullscreen {
                hints.push(HintItem::new(crate::key!(Enter), "view"));
            }
            if selected_supports_copy {
                hints.push(HintItem::new(crate::key!('y'), "copy output"));
            }
            if selected_can_kill {
                hints.push(HintItem::new(crate::key!('x'), "kill"));
            }
            hints.push(HintItem::new(
                crate::key!('h'),
                if show_done { "hide done" } else { "show done" },
            ));
            hints
        }
        ActivePane::Catalog => vec![],
        ActivePane::Scrollback if scrollback_search.is_some() => {
            let mut hints = Vec::new();
            if vim_mode {
                if scrollback_search.is_some_and(|s| s.is_composing()) {
                    hints.push(HintItem::new(crate::key!(Enter), "go"));
                } else {
                    hints.push(HintItem::paired(
                        crate::key!('n'),
                        crate::key!('N'),
                        "next/prev",
                    ));
                }
            } else {
                use crate::input::key::KeyShortcut;
                use crossterm::event::KeyCode;
                hints.push(HintItem::paired(
                    KeyShortcut::key(KeyCode::Down),
                    KeyShortcut::key(KeyCode::Up),
                    "next/prev",
                ));
            }
            hints.push(HintItem::new(crate::key!(Esc), "cancel"));
            hints
        }
        ActivePane::Scrollback => {
            let mut hints = Vec::new();
            let nothing_special = !selected_is_agent_message
                && !selected_is_user_prompt
                && !selected_is_credit_limit
                && fold_label.is_none()
                && group_header_label.is_none()
                && !selected_supports_fullscreen;
            if nothing_special {
                hints.push(space_prompt_hint());
            }
            if selected_is_credit_limit {
                if let Some(key) = registry.key_for(ActionId::OpenBlockViewer) {
                    hints.push(HintItem::new(key, "open"));
                }
                hints.push(space_prompt_hint());
            }
            if selected_is_agent_message {
                if vim_mode
                    && selected_supports_copy
                    && let Some(key) = registry.key_for(ActionId::CopyBlockContent)
                {
                    hints.push(HintItem::new(key, "copy"));
                }
                hints.push(space_prompt_hint());
            }
            if selected_is_user_prompt {
                let user_collapsed = fold_label == Some("expand");
                if user_collapsed {
                    let key = registry
                        .key_for_mode(ActionId::ToggleFold, vim_mode)
                        .or_else(|| registry.key_for_mode(ActionId::Expand, vim_mode));
                    if let Some(key) = key {
                        hints.push(HintItem::new(key, "expand"));
                    }
                }
                if let Some(key) = registry.key_for(ActionId::ExpandAllThinking) {
                    hints.push(HintItem::new(key, thinking_label));
                }
                if !user_collapsed {
                    hints.push(space_prompt_hint());
                }
            }
            let user_collapsed_already_pushed =
                selected_is_user_prompt && fold_label == Some("expand");
            if let Some(label) = group_header_label {
                if let Some(key) = registry.key_for(ActionId::OpenBlockViewer) {
                    hints.push(HintItem::new(key, label));
                }
            } else if !user_collapsed_already_pushed && let Some(label) = fold_label {
                let directional = if label == "expand" {
                    ActionId::Expand
                } else {
                    ActionId::Collapse
                };
                let key = registry
                    .key_for_mode(ActionId::ToggleFold, vim_mode)
                    .or_else(|| registry.key_for_mode(directional, vim_mode));
                if let Some(key) = key {
                    hints.push(HintItem::new(key, label));
                }
            }
            if group_header_label.is_none()
                && selected_supports_fullscreen
                && let Some(key) = registry.key_for(ActionId::OpenBlockViewer)
            {
                hints.push(HintItem::new(key, "open"));
            }
            if vim_mode
                && let (Some(j), Some(k)) = (
                    registry.key_for(ActionId::SelectNext),
                    registry.key_for(ActionId::SelectPrev),
                )
            {
                hints.push(HintItem::paired(j, k, "nav").pinned());
            }
            if vim_mode
                && let (Some(h), Some(l)) = (
                    registry.key_for(ActionId::PrevTurn),
                    registry.key_for(ActionId::NextTurn),
                )
            {
                let mut hint = HintItem::paired(l, h, "turn").pinned();
                hint.custom_display = Some("Shift+l/h");
                hints.push(hint);
            }
            if !selected_is_user_prompt
                && let Some(key) = registry.key_for(ActionId::ExpandAllThinking)
            {
                hints.push(HintItem::new(key, thinking_label));
            }
            if vim_mode
                && let (Some(g), Some(bg)) = (
                    registry.key_for(ActionId::GotoTop),
                    registry.key_for(ActionId::GotoBottom),
                )
            {
                hints.push(HintItem::paired(g, bg, "top/btm"));
            }
            if vim_mode
                && !selected_is_agent_message
                && selected_supports_copy
                && let Some(key) = registry.key_for(ActionId::CopyBlockContent)
            {
                hints.push(HintItem::new(key, "copy"));
            }
            if vim_mode
                && let Some(label) = selected_meta_label
                && let Some(key) = registry.key_for(ActionId::CopyBlockMeta)
            {
                hints.push(HintItem::new(key, label));
            }
            if selected_can_kill {
                hints.push(HintItem::new(crate::key!('x'), "kill"));
            }
            if is_subagent_view {
                hints.push(HintItem::paired(crate::key!('q'), crate::key!(Esc), "back"));
            }
            hints
        }
    };
    if is_turn_running && let Some(def) = registry.find(ActionId::CancelTurn) {
        hints.push(def.hint());
    }
    let has_composer_payload = !prompt.text().trim().is_empty() || is_editing_queued;
    if matches!(active_pane, ActivePane::Prompt)
        && ActionRegistry::interjection_possible(is_turn_running, has_composer_payload)
        && let Some(def) = registry.find(ActionId::InterjectPrompt)
    {
        hints.push(def.hint());
    }
    if can_demote
        && !is_subagent_view
        && let Some(key) = registry.key_for(ActionId::SendToBackground)
    {
        hints.push(HintItem::new(key, "send to bg"));
    }
    hints
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ActionRegistry;
    /// Convenience: build hints for the Scrollback pane with sensible defaults.
    /// Override only what each test cares about.
    #[allow(clippy::too_many_arguments)]
    fn scrollback_hints(
        registry: &ActionRegistry,
        fold_label: Option<&'static str>,
        selected_supports_copy: bool,
        selected_supports_fullscreen: bool,
        selected_is_user_prompt: bool,
        selected_is_agent_message: bool,
    ) -> Vec<HintItem> {
        scrollback_hints_with_vim_mode(
            registry,
            fold_label,
            selected_supports_copy,
            selected_supports_fullscreen,
            selected_is_user_prompt,
            selected_is_agent_message,
            true,
        )
    }
    fn scrollback_hints_with_vim_mode(
        registry: &ActionRegistry,
        fold_label: Option<&'static str>,
        selected_supports_copy: bool,
        selected_supports_fullscreen: bool,
        selected_is_user_prompt: bool,
        selected_is_agent_message: bool,
        vim_mode: bool,
    ) -> Vec<HintItem> {
        build_hints(
            ActivePane::Scrollback,
            &PromptWidget::default(),
            registry,
            false,
            fold_label,
            None,
            "expand thinking",
            false,
            selected_supports_copy,
            None,
            selected_supports_fullscreen,
            false,
            false,
            false,
            vim_mode,
            false,
            false,
            false,
            selected_is_user_prompt,
            selected_is_agent_message,
            false,
            false,
            None,
        )
    }
    fn first_two_labels(hints: &[HintItem]) -> Vec<&str> {
        hints.iter().take(2).map(|h| h.label.as_ref()).collect()
    }
    #[test]
    fn demotion_hint_uses_registered_ctrl_b_binding() {
        let registry = ActionRegistry::defaults();
        let hints = build_hints(
            ActivePane::Scrollback,
            &PromptWidget::default(),
            &registry,
            false,
            None,
            None,
            "expand thinking",
            false,
            false,
            None,
            false,
            true,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        );
        let hint = hints
            .iter()
            .find(|hint| hint.label == "send to bg")
            .expect("running Execute should advertise demotion");
        assert_eq!(hint.keys, vec![crate::key!('b', CONTROL)]);
    }
    #[test]
    fn group_header_shows_enter_toggle_hint_instead_of_open_and_fold() {
        let registry = ActionRegistry::defaults();
        let hints = build_hints(
            ActivePane::Scrollback,
            &PromptWidget::default(),
            &registry,
            false,
            Some("expand"),
            Some("expand"),
            "expand thinking",
            false,
            false,
            None,
            true,
            false,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        );
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            !labels.contains(&"open"),
            "no Enter:open hint on a group header, got {labels:?}"
        );
        let expand_hints: Vec<&HintItem> = hints.iter().filter(|h| h.label == "expand").collect();
        assert_eq!(
            expand_hints.len(),
            1,
            "exactly one expand hint (Enter toggle, no separate fold hint), got {labels:?}"
        );
        let enter_key = registry
            .key_for(crate::actions::ActionId::OpenBlockViewer)
            .expect("OpenBlockViewer has a default key");
        assert_eq!(
            expand_hints[0].keys,
            vec![enter_key],
            "the header expand hint must be bound to Enter (OpenBlockViewer)"
        );
        assert_eq!(
            labels.first(),
            Some(&"expand"),
            "the Enter toggle takes the first compact slot, got {labels:?}"
        );
    }
    #[test]
    fn scrollback_user_prompt_collapsed_hoists_expand_then_thinking() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, Some("expand"), false, false, true, false);
        assert_eq!(first_two_labels(&hints), vec!["expand", "expand thinking"]);
    }
    #[test]
    fn scrollback_user_prompt_expanded_hoists_thinking_then_space() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, Some("collapse"), false, false, true, false);
        assert_eq!(first_two_labels(&hints), vec!["expand thinking", "prompt"]);
    }
    #[test]
    fn scrollback_agent_message_hoists_copy_then_space() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, None, true, false, false, true);
        assert_eq!(first_two_labels(&hints), vec!["copy", "prompt"]);
    }
    #[test]
    fn scrollback_agent_message_no_duplicate_y_copy_in_full_list() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, None, true, false, false, true);
        let copy_count = hints.iter().filter(|h| h.label == "copy").count();
        assert_eq!(copy_count, 1, "copy should appear exactly once");
    }
    #[test]
    fn scrollback_default_block_shows_prompt_first() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, None, false, false, false, false);
        assert_eq!(first_two_labels(&hints), vec!["prompt", "nav"]);
    }
    #[test]
    fn scrollback_foldable_non_user_block_hoists_fold_then_open() {
        let registry = ActionRegistry::defaults();
        let hints = scrollback_hints(&registry, Some("fold"), false, true, false, false);
        assert_eq!(first_two_labels(&hints), vec!["fold", "open"]);
    }
    #[test]
    fn scrollback_vim_mode_on_shows_nav_and_turn_hints() {
        let registry = ActionRegistry::defaults();
        let hints =
            scrollback_hints_with_vim_mode(&registry, None, false, false, false, false, true);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            labels.contains(&"nav"),
            "vim mode should show j/k nav hint; got {labels:?}"
        );
        assert!(
            labels.contains(&"turn"),
            "vim mode should show Shift+l/h turn hint; got {labels:?}"
        );
        assert!(
            labels.contains(&"top/btm"),
            "vim mode should show g/G top/btm hint; got {labels:?}"
        );
    }
    #[test]
    fn scrollback_vim_mode_off_hides_nav_turn_and_topbtm() {
        let registry = ActionRegistry::defaults();
        let hints =
            scrollback_hints_with_vim_mode(&registry, None, false, false, false, false, false);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            !labels.contains(&"nav"),
            "vim-off must hide j/k nav hint; got {labels:?}"
        );
        assert!(
            !labels.contains(&"turn"),
            "vim-off must hide Shift+l/h turn hint; got {labels:?}"
        );
        assert!(
            !labels.contains(&"top/btm"),
            "vim-off must hide g/G top/btm hint; got {labels:?}"
        );
    }
    #[test]
    fn scrollback_vim_mode_off_hides_y_copy_on_agent_message() {
        let registry = ActionRegistry::defaults();
        let hints =
            scrollback_hints_with_vim_mode(&registry, None, true, false, false, true, false);
        assert!(
            !hints.iter().any(|h| h.label == "copy"),
            "vim-off must hide y:copy hint on agent message"
        );
    }
    #[test]
    fn scrollback_never_shows_rewind_hint_on_user_prompt() {
        let registry = ActionRegistry::defaults();
        for vim_mode in [true, false] {
            let hints = scrollback_hints_with_vim_mode(
                &registry,
                Some("expand"),
                false,
                false,
                true,
                false,
                vim_mode,
            );
            assert!(
                !hints.iter().any(|h| h.label == "rewind"),
                "rewind is slash-command only (vim_mode={vim_mode})"
            );
        }
    }
    /// Build scrollback hints with an open search session in the given phase.
    fn scrollback_search_hints(
        registry: &ActionRegistry,
        vim_mode: bool,
        composing: bool,
    ) -> Vec<HintItem> {
        let mut search = ScrollbackSearchState::open();
        if !composing {
            search.accept();
        }
        build_hints(
            ActivePane::Scrollback,
            &PromptWidget::default(),
            registry,
            false,
            None,
            None,
            "expand thinking",
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            vim_mode,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            Some(&search),
        )
    }
    #[test]
    fn scrollback_search_hint_not_in_bottom_bar() {
        let registry = ActionRegistry::defaults();
        for vim_mode in [true, false] {
            let hints = scrollback_hints_with_vim_mode(
                &registry, None, false, false, false, false, vim_mode,
            );
            assert!(
                !hints.iter().any(|h| h.label == "search"),
                "bottom bar must not advertise / search (vim_mode={vim_mode})"
            );
        }
    }
    #[test]
    fn scrollback_search_composing_shows_go_and_cancel_only() {
        let registry = ActionRegistry::defaults();
        let labels: Vec<String> = scrollback_search_hints(&registry, true, true)
            .iter()
            .map(|h| h.label.to_string())
            .collect();
        assert!(labels.contains(&"go".to_string()), "got {labels:?}");
        assert!(labels.contains(&"cancel".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"next/prev".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"nav".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"search".to_string()), "got {labels:?}");
    }
    #[test]
    fn scrollback_search_browsing_shows_next_prev_and_cancel() {
        let registry = ActionRegistry::defaults();
        let labels: Vec<String> = scrollback_search_hints(&registry, true, false)
            .iter()
            .map(|h| h.label.to_string())
            .collect();
        assert!(labels.contains(&"next/prev".to_string()), "got {labels:?}");
        assert!(labels.contains(&"cancel".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"go".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"nav".to_string()), "got {labels:?}");
    }
    /// The keys behind the `next/prev` hint for a search in the given phase.
    fn next_prev_keys(
        registry: &ActionRegistry,
        vim_mode: bool,
        composing: bool,
    ) -> Vec<crate::input::key::KeyShortcut> {
        scrollback_search_hints(registry, vim_mode, composing)
            .into_iter()
            .find(|h| h.label == "next/prev")
            .map(|h| h.keys)
            .unwrap_or_default()
    }
    #[test]
    fn scrollback_search_vim_browsing_uses_n_keys() {
        use crossterm::event::KeyCode;
        let registry = ActionRegistry::defaults();
        let keys = next_prev_keys(&registry, true, false);
        let codes: Vec<KeyCode> = keys.iter().map(|k| k.code).collect();
        assert_eq!(codes, vec![KeyCode::Char('n'), KeyCode::Char('N')]);
    }
    #[test]
    fn scrollback_search_simple_mode_uses_arrow_keys_in_both_phases() {
        use crossterm::event::KeyCode;
        let registry = ActionRegistry::defaults();
        for composing in [true, false] {
            let codes: Vec<KeyCode> = next_prev_keys(&registry, false, composing)
                .iter()
                .map(|k| k.code)
                .collect();
            assert_eq!(
                codes,
                vec![KeyCode::Down, KeyCode::Up],
                "simple mode next/prev should be arrows (composing={composing})"
            );
        }
    }
    #[test]
    fn prompt_branch_excludes_exit_session_home_hint() {
        let registry = ActionRegistry::defaults();
        let hints = build_hints(
            ActivePane::Prompt,
            &PromptWidget::default(),
            &registry,
            false,
            None,
            None,
            "expand thinking",
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        );
        assert!(
            !hints.iter().any(|h| h.label == "home"),
            "ExitSession (home) must not appear in prompt-focused bar"
        );
    }
    fn prompt_hints_with_text(
        multiline_mode: bool,
        shift_enter_unavailable: bool,
    ) -> Vec<HintItem> {
        prompt_hints_with_text_and_turn(multiline_mode, shift_enter_unavailable, false)
    }
    fn prompt_hints_with_text_and_turn(
        multiline_mode: bool,
        shift_enter_unavailable: bool,
        is_turn_running: bool,
    ) -> Vec<HintItem> {
        let mut prompt = PromptWidget::default();
        prompt.textarea.insert_str("hello");
        let registry = ActionRegistry::defaults();
        build_hints(
            ActivePane::Prompt,
            &prompt,
            &registry,
            false,
            None,
            None,
            "expand thinking",
            false,
            false,
            None,
            false,
            false,
            false,
            multiline_mode,
            true,
            false,
            is_turn_running,
            false,
            false,
            false,
            false,
            shift_enter_unavailable,
            None,
        )
    }
    #[test]
    fn prompt_idle_submit_hint_is_send() {
        let hints = prompt_hints_with_text_and_turn(false, false, false);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            labels.contains(&"send") && !labels.contains(&"queue"),
            "idle prompt must advertise Enter:send; got {labels:?}"
        );
    }
    #[test]
    fn prompt_running_submit_hint_is_queue_and_send_now() {
        let hints = prompt_hints_with_text_and_turn(false, false, true);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            labels.contains(&"queue"),
            "mid-turn follow-up must advertise Enter:queue (not send); got {labels:?}"
        );
        assert!(
            !labels.contains(&"send"),
            "mid-turn must not mislabel Enter as send; got {labels:?}"
        );
        assert!(
            labels.contains(&"send now"),
            "mid-turn with composer text must advertise the send-now (interject) chord; got {labels:?}"
        );
    }
    /// Empty composer + mid-turn queue: bare Enter is send-now in both normal
    /// and multiline modes (multiline only inserts newline when there is text).
    #[test]
    fn prompt_empty_mid_turn_queue_advertises_send_now_including_multiline() {
        for multiline in [false, true] {
            let prompt = PromptWidget::default();
            let registry = ActionRegistry::defaults();
            let hints = build_hints(
                ActivePane::Prompt,
                &prompt,
                &registry,
                false,
                None,
                None,
                "expand thinking",
                false,
                false,
                None,
                false,
                false,
                false,
                multiline,
                true,
                false,
                true,
                true,
                false,
                false,
                false,
                false,
                None,
            );
            let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
            assert!(
                labels.contains(&"send now"),
                "empty composer mid-turn with queue must advertise Enter:send now \
                 (multiline={multiline}); got {labels:?}"
            );
        }
    }
    #[test]
    fn prompt_legacy_vte_adds_alt_enter_newline_hint() {
        let hints = prompt_hints_with_text(false, true);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            labels.contains(&"newline"),
            "legacy VTE must surface an explicit Alt+Enter newline hint; \
             got {labels:?}"
        );
    }
    #[test]
    fn prompt_modern_terminal_no_newline_hint() {
        let hints = prompt_hints_with_text(false, false);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            !labels.contains(&"newline"),
            "modern terminals must not show the legacy-VTE newline hint \
             (Shift+Enter works natively); got {labels:?}"
        );
    }
    #[test]
    fn prompt_multiline_mode_no_extra_newline_hint() {
        let hints = prompt_hints_with_text(true, true);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_ref()).collect();
        assert!(
            !labels.contains(&"newline"),
            "multiline mode must not show the legacy-VTE newline hint \
             (Enter already inserts a newline); got {labels:?}"
        );
    }
    #[test]
    fn prompt_multiline_send_hint_uses_alt_on_legacy_vte() {
        use crossterm::event::KeyModifiers;
        let hints = prompt_hints_with_text(true, true);
        let send_hint = hints
            .iter()
            .find(|h| h.label == "send")
            .expect("multiline mode with text must show a send hint");
        let key = send_hint
            .keys
            .first()
            .expect("send hint must have at least one key");
        assert!(
            key.modifiers.contains(KeyModifiers::ALT),
            "legacy VTE multiline send hint must advertise Alt+Enter, \
             got modifiers {:?}",
            key.modifiers
        );
        assert!(
            !key.modifiers.contains(KeyModifiers::SHIFT),
            "legacy VTE multiline send hint must NOT advertise Shift+Enter, \
             got modifiers {:?}",
            key.modifiers
        );
    }
    #[test]
    fn prompt_multiline_send_hint_uses_shift_on_modern_terminal() {
        use crossterm::event::KeyModifiers;
        let hints = prompt_hints_with_text(true, false);
        let send_hint = hints
            .iter()
            .find(|h| h.label == "send")
            .expect("multiline mode with text must show a send hint");
        let key = send_hint
            .keys
            .first()
            .expect("send hint must have at least one key");
        assert!(
            key.modifiers.contains(KeyModifiers::SHIFT),
            "modern terminal multiline send hint must advertise Shift+Enter, \
             got modifiers {:?}",
            key.modifiers
        );
    }
    fn layout_with_rows(
        area: Rect,
        banner_height: u16,
        cta_height: u16,
        follow_ups_height: u16,
    ) -> AgentViewLayout {
        let layout_cfg = LayoutConfig::default();
        let scrollbar_cfg = ScrollbarConfig::default();
        AgentViewLayout::compute(
            area,
            &layout_cfg,
            &scrollbar_cfg,
            0,
            2,
            0,
            0,
            0,
            0,
            0,
            0,
            banner_height,
            cta_height,
            follow_ups_height,
            0,
            0,
            0,
            1,
            false,
        )
    }
    fn layout_with_cta(area: Rect, cta_height: u16) -> AgentViewLayout {
        layout_with_rows(area, 0, cta_height, 0)
    }
    /// Minimal layout with a timeline rail request — hides the cfg-dependent
    /// arity of `compute` like `layout_with_rows` does.
    fn layout_with_rail(
        area: Rect,
        timeline_width: u16,
        scrollbar_cfg: &ScrollbarConfig,
    ) -> AgentViewLayout {
        let layout_cfg = LayoutConfig::default();
        AgentViewLayout::compute(
            area,
            &layout_cfg,
            scrollbar_cfg,
            timeline_width,
            2,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            1,
            false,
        )
    }
    #[test]
    fn timeline_rail_replaces_the_scrollbar_column() {
        let area = Rect::new(0, 0, 80, 40);
        let scrollbar_cfg = ScrollbarConfig::default();
        let without = layout_with_rail(area, 0, &scrollbar_cfg);
        let with_rail = layout_with_rail(area, 3, &scrollbar_cfg);
        assert_eq!(with_rail.timeline_width, 3);
        assert_eq!(with_rail.scrollbar_x, without.scrollbar_x);
        assert_eq!(with_rail.timeline_x, with_rail.scrollbar_x + 1 - 3);
        assert!(
            with_rail.scrollback_content.x + with_rail.scrollback_content.width
                <= with_rail.timeline_x,
            "content (ends {}) must not overlap the rail (starts {})",
            with_rail.scrollback_content.x + with_rail.scrollback_content.width,
            with_rail.timeline_x,
        );
        assert!(
            with_rail.scrollback_content.width <= without.scrollback_content.width,
            "the rail never widens the content"
        );
        let no_scrollbar = ScrollbarConfig {
            enabled: false,
            ..ScrollbarConfig::default()
        };
        let forced_off = layout_with_rail(area, 3, &no_scrollbar);
        assert_eq!(forced_off.timeline_width, 0);
        assert_eq!(
            forced_off.scrollback_content.width, forced_off.scrollback.width,
            "no carve-out without the scrollbar gutter"
        );
    }
    #[test]
    fn plugin_cta_row_present_above_prompt() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_cta(area, 1);
        assert_eq!(layout.plugin_cta.height, 1);
        assert!(layout.plugin_cta.y < layout.prompt.y);
        assert!(layout.plugin_cta.y >= layout.scrollback.y + layout.scrollback.height);
    }
    #[test]
    fn plugin_cta_row_absent_when_height_zero() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_cta(area, 0);
        assert_eq!(layout.plugin_cta, Rect::default());
    }
    #[test]
    fn plugin_cta_row_suppressed_on_short_terminal() {
        let area = Rect::new(0, 0, 80, 16);
        let layout = layout_with_cta(area, 1);
        assert_eq!(layout.plugin_cta, Rect::default());
        assert!(layout.scrollback.height >= 5);
    }
    /// Banner row (mode banner / ephemeral tip slot): height 1 reserves a
    /// one-row rect directly above the prompt (gap row in between).
    #[test]
    fn banner_row_present_above_prompt() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 1, 0, 0);
        assert_eq!(layout.banner.height, 1);
        assert_eq!(layout.prompt.y, layout.banner.y + 1);
        assert!(layout.banner.y >= layout.scrollback.y + layout.scrollback.height);
    }
    #[test]
    fn banner_row_absent_when_height_zero() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 0, 0, 0);
        assert_eq!(layout.banner, Rect::default());
    }
    fn row_text(buf: &Buffer, area: Rect) -> String {
        (0..area.width)
            .filter_map(|x| buf.cell((x, 0)).map(|c| c.symbol().to_string()))
            .collect()
    }
    #[test]
    fn render_follow_ups_emits_clickable_chips() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let suggestions = vec!["Yes".to_string(), "No".to_string()];
        let rects = render_follow_ups(area, &mut buf, &theme, &suggestions, None);
        assert_eq!(rects.len(), 2, "both chips fit on a wide row");
        let row = row_text(&buf, area);
        assert!(row.contains("Yes"), "row = {row:?}");
        assert!(row.contains("No"), "row = {row:?}");
        assert!(rects[0].x + rects[0].width <= rects[1].x);
        assert!(rects[1].x + rects[1].width <= area.width);
    }
    #[test]
    fn render_follow_ups_drops_chips_that_do_not_fit() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let suggestions = vec!["abc".to_string(), "way too long to fit here".to_string()];
        let rects = render_follow_ups(area, &mut buf, &theme, &suggestions, None);
        assert_eq!(rects.len(), 1, "only the first chip fits");
    }
    #[test]
    fn render_follow_ups_zero_area_is_noop() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let theme = Theme::current();
        let rects = render_follow_ups(
            Rect::new(0, 0, 0, 0),
            &mut buf,
            &theme,
            &["x".to_string()],
            None,
        );
        assert!(rects.is_empty());
    }
    #[test]
    fn render_follow_ups_clamps_long_label() {
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let long = "a".repeat(200);
        let rects = render_follow_ups(area, &mut buf, &theme, &[long], None);
        assert_eq!(rects.len(), 1);
        assert!(
            rects[0].width <= 52,
            "chip width {} not clamped",
            rects[0].width
        );
    }
    #[test]
    fn render_follow_ups_applies_hover_style() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let suggestions = vec!["Yes".to_string(), "No".to_string()];
        let rects = render_follow_ups(area, &mut buf, &theme, &suggestions, Some(0));
        assert_eq!(rects.len(), 2);
        let r0 = rects[0];
        let cell = buf.cell((r0.x + 1, r0.y)).expect("hovered chip cell");
        assert_eq!(
            cell.fg, theme.text_primary,
            "hovered chip should use primary text"
        );
        assert_eq!(
            cell.bg, theme.bg_hover,
            "hovered chip should use theme.bg_hover"
        );
        let r1 = rects[1];
        let cell1 = buf.cell((r1.x + 1, r1.y)).expect("non-hovered chip cell");
        assert_eq!(cell1.fg, theme.link_fg, "non-hovered chip keeps link color");
    }
    #[test]
    fn follow_ups_row_present_above_prompt() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 0, 0, 1);
        assert_eq!(layout.follow_ups.height, 1);
        assert!(layout.follow_ups.y < layout.prompt.y);
        assert!(layout.follow_ups.y >= layout.scrollback.y + layout.scrollback.height);
    }
    #[test]
    fn follow_ups_row_below_plugin_cta() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 0, 1, 1);
        assert!(layout.plugin_cta.height == 1 && layout.follow_ups.height == 1);
        assert!(layout.plugin_cta.y < layout.follow_ups.y);
        assert!(layout.follow_ups.y < layout.prompt.y);
    }
    #[test]
    fn follow_ups_row_absent_when_height_zero() {
        let area = Rect::new(0, 0, 80, 40);
        let layout = layout_with_rows(area, 0, 0, 0);
        assert_eq!(layout.follow_ups, Rect::default());
    }
    #[test]
    fn follow_ups_row_suppressed_on_short_terminal() {
        let area = Rect::new(0, 0, 80, 16);
        let layout = layout_with_rows(area, 0, 0, 1);
        assert_eq!(layout.follow_ups, Rect::default());
        assert!(layout.scrollback.height >= 5);
    }
    /// Default todo badge is a `done/total` fraction: numerator = completed,
    /// denominator = all tasks except cancelled.
    #[test]
    fn todo_badge_default_renders_done_over_total_fraction() {
        let theme = Theme::current();
        let counts = super::super::todo_pane::TodoCounts {
            in_progress: 1,
            pending: 2,
            completed: 2,
            cancelled: 0,
        };
        let spans =
            render_todo_badge_spans(&counts, false, false, TodoBadgeFormat::Default, &theme)
                .expect("badge renders when there are todos");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.starts_with("2/5"),
            "expected a 2/5 done/total fraction, got {text:?}"
        );
        let with_cancelled = super::super::todo_pane::TodoCounts {
            in_progress: 0,
            pending: 1,
            completed: 2,
            cancelled: 1,
        };
        let spans = render_todo_badge_spans(
            &with_cancelled,
            false,
            false,
            TodoBadgeFormat::Default,
            &theme,
        )
        .expect("badge renders");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.starts_with("2/3"),
            "cancelled tasks are excluded from the total, got {text:?}"
        );
    }
    /// No todos → no badge.
    #[test]
    fn todo_badge_absent_when_no_todos() {
        let theme = Theme::current();
        let empty = super::super::todo_pane::TodoCounts::default();
        assert!(
            render_todo_badge_spans(&empty, false, false, TodoBadgeFormat::Default, &theme)
                .is_none()
        );
    }
    /// All-cancelled list → no badge (denominator would be 0; nothing to track).
    #[test]
    fn todo_badge_absent_when_all_cancelled() {
        let theme = Theme::current();
        let all_cancelled = super::super::todo_pane::TodoCounts {
            in_progress: 0,
            pending: 0,
            completed: 0,
            cancelled: 3,
        };
        assert!(
            render_todo_badge_spans(
                &all_cancelled,
                false,
                false,
                TodoBadgeFormat::Default,
                &theme
            )
            .is_none()
        );
    }
}
