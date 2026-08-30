//! Pure layout computation for the dashboard view.
//!
//! How the peek and the roster split the vertical space is specified in
//! [`docs/internal/33-dashboard-peek-responsive-layout.md`](../../../../docs/internal/33-dashboard-peek-responsive-layout.md).

use ratatui::layout::Rect;

/// Minimum width at which the dashboard can render meaningful rows.
/// Below this, the renderer falls back to a stripped, single-column view; row labels are middle-truncated.
pub const MIN_DASHBOARD_WIDTH: u16 = 40;

/// Minimum list-band height (terminal rows) while evaluating or opening a peek.
pub const LIST_FLOOR_ROWS: u16 = 12;

/// Minimum height of the whole peek box (borders, status, body, and reply) for a live-tail peek.
pub const PEEK_MIN_BOX_LIVE_TAIL: u16 = 8;

/// Minimum height of the whole peek box for question and permission peeks (options need room).
pub const PEEK_MIN_BOX_QUESTION: u16 = 10;

/// The peek max is ⌊H × PEEK_MAX_FRAC_NUM / PEEK_MAX_FRAC_DEN⌋ (whole box).
pub const PEEK_MAX_FRAC_NUM: u16 = 3;
pub const PEEK_MAX_FRAC_DEN: u16 = 8;

/// Secondary cap on live-tail body rows inside an allocated peek box.
pub const MAX_LIVE_TAIL_ROWS: u16 = 28;

/// Live-tail height budget for a peek with no question (status, optional blank, and reply rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeekLiveTailBudget {
    pub live_tail: u16,
    pub blank_row: bool,
    pub content_rows: u16,
}

/// Result of list-first peek allocation for height `H`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeekAllocation {
    pub show_peek: bool,
    /// Whole peek box height including borders; 0 if `!show_peek`.
    pub peek_box_h: u16,
    /// Max inner content rows for a peek at full allowed size (`peek_box_h - 2`).
    pub max_content_rows: u16,
}

/// The desired inner rows for a live-tail peek, shrunk to its content.
///
/// The row recipe matches the dense paint: `status + [pin?] + body + [blank?] + reply`.
/// - `body_measured`: the densified current-turn lines (everything after the last user prompt).
/// - `pin_user`: when a last user prompt exists, budget one pin row (the paint charges it too).
/// - A blank row is budgeted when the body is non-empty and room remains after the pin and body.
///   (The paint blanks only when the middle still has 2 or more rows after the blank, so the pin and body share.)
///
/// An empty body reserves 1 row for the empty/hint line.
/// The result never exceeds `max_content`; the body is also capped by [`MAX_LIVE_TAIL_ROWS`].
pub fn peek_live_tail_desired_content(
    max_content: u16,
    reply_rows: u16,
    body_measured: u16,
    pin_user: bool,
) -> PeekLiveTailBudget {
    let reply_rows = reply_rows.max(1);
    let pin = u16::from(pin_user);
    let fixed = 1u16 + reply_rows + pin; // status + reply + optional pin

    if max_content < fixed {
        return PeekLiveTailBudget {
            live_tail: 0,
            blank_row: false,
            content_rows: max_content,
        };
    }

    let room_no_blank = max_content.saturating_sub(fixed).min(MAX_LIVE_TAIL_ROWS);
    if room_no_blank == 0 {
        return PeekLiveTailBudget {
            live_tail: 0,
            blank_row: false,
            content_rows: fixed,
        };
    }

    // Prefer a breathing blank whenever body is non-empty and room remains.
    let room_with_blank = max_content
        .saturating_sub(fixed + 1)
        .min(MAX_LIVE_TAIL_ROWS);
    let blank = room_with_blank > 0;
    let body_cap = if blank {
        room_with_blank
    } else {
        room_no_blank
    };

    let body = if body_measured == 0 {
        1u16.min(body_cap)
    } else {
        body_measured.min(body_cap)
    };
    let blank = blank && body > 0;
    let content_rows = fixed + u16::from(blank) + body;
    PeekLiveTailBudget {
        live_tail: body,
        blank_row: blank,
        content_rows: content_rows.min(max_content),
    }
}

/// The whole-box peek cap: ⌊H × 3/8⌋ rows.
pub fn peek_max_box_rows(h: u16) -> u16 {
    ((u32::from(h) * u32::from(PEEK_MAX_FRAC_NUM)) / u32::from(PEEK_MAX_FRAC_DEN)) as u16
}

/// Chrome rows (header, gaps, footer, margins); the list and peek are not counted.
pub fn chrome_overhead(area: Rect) -> u16 {
    dashboard_fixed_overhead(area).0
}

/// List-first peek allocation.
///
/// 1. Reserve [`LIST_FLOOR_ROWS`] for the list band (clamped to space after chrome).
/// 2. The remainder is the candidate peek, capped by [`peek_max_box_rows`].
/// 3. A candidate below `peek_min_box` means no peek.
/// 4. Otherwise the peek height is `min(desired_content+2, max_candidate)`, at least `peek_min_box` when showing.
///
/// `desired_content_rows` is inner content (no borders).
/// A growing reply should increase it; the list may shrink only down to the floor (the max candidate enforces this).
pub fn allocate_peek(
    area_h: u16,
    fixed_overhead: u16,
    desired_content_rows: u16,
    peek_min_box: u16,
) -> PeekAllocation {
    let after = area_h.saturating_sub(fixed_overhead);
    if after == 0 {
        return PeekAllocation {
            show_peek: false,
            peek_box_h: 0,
            max_content_rows: 0,
        };
    }
    let list_floor = LIST_FLOOR_ROWS.min(after);
    let remainder = after.saturating_sub(list_floor);
    let peek_max = peek_max_box_rows(area_h);
    let max_peek = remainder.min(peek_max);
    let max_content_rows = max_peek.saturating_sub(2);

    if max_peek < peek_min_box {
        return PeekAllocation {
            show_peek: false,
            peek_box_h: 0,
            max_content_rows,
        };
    }

    let desired_box = desired_content_rows.saturating_add(2);
    let peek_box_h = desired_box.max(peek_min_box).min(max_peek);

    PeekAllocation {
        show_peek: true,
        peek_box_h,
        max_content_rows,
    }
}

/// Outer horizontal padding for the dispatch box (cols on each side).
///
/// Matches `LayoutConfig::outer_hpad_left/right = 2` from the agent view's default appearance config.
pub const DISPATCH_OUTER_HPAD: u16 = 2;

/// Outer horizontal padding for the top page header (cols on each side).
/// Matches list/dispatch so the title aligns with content below.
pub const HEADER_OUTER_HPAD: u16 = 2;

/// Outer horizontal padding for the row list (cols on each side).
///
/// Gives the list (rows, group headers, scrollbar) breathing room.
/// Selection markers, group header rules (`────`), and row text don't sit flush against the terminal edges.
pub const LIST_OUTER_HPAD: u16 = 2;

/// Output of [`compute_layout`].
#[derive(Debug, Clone, Copy)]
pub struct DashboardLayout {
    /// Top margin row (blank space above the header). Height: 0 or 1.
    /// Matches the welcome view's `v_margin` so the dashboard's title row doesn't sit flush against the alt-screen top edge.
    pub top_margin: Rect,
    /// Header row (title + summary). Height: 0 or 1.
    pub header: Rect,
    /// Vertical breathing room between the header and the row list. Height: 0 or 1.
    /// Drops to 0 on short terminals (`area.height <= 10`, the same threshold as the dispatch and shortcuts gaps).
    /// No sub-renderer ever touches this rect; the area-wide `bg_base` fill paints it.
    /// It is a named rect rather than an anonymous y-cursor bump only so tests can pin its position and threshold.
    pub header_gap: Rect,
    /// Scrollable list area (rows + group headers).
    pub list: Rect,
    /// Bottom dispatch input area.
    pub dispatch: Rect,
    /// Footer / shortcut hint row.
    pub footer: Rect,
    /// Bottom margin row (blank space below the shortcuts bar). Height: 0 or 1.
    /// Matches the agent view's `bottom_vpad` so the shortcuts bar doesn't sit flush against the alt-screen's bottom edge.
    /// Drops to 0 on short terminals (`area.height <= 16`, the same threshold as `views::agent::AgentViewLayout::compute`).
    pub bottom_margin: Rect,
}

/// Compute the dashboard layout for a given content area.
///
/// `peek_visible` requests the peek panel; the layout shows it only when the area has enough vertical room.
/// When the area is narrower than [`MIN_DASHBOARD_WIDTH`], the layout still returns a valid arrangement (the renderer truncates labels).
pub fn compute_layout(area: Rect, peek_visible: bool) -> DashboardLayout {
    // A single text row is the default
    // Callers that support a growing multiline dispatch box (Shift+Enter newlines) use [`compute_layout_with_dispatch`] to request more
    compute_layout_with_dispatch(area, peek_visible, 1)
}

fn dashboard_chrome_heights(area: Rect) -> (u16, u16, u16, u16, u16, u16, u16, bool) {
    // Match the welcome and agent top margins; drop on short terminals
    let top_margin_h: u16 = if area.height > 6 { 1 } else { 0 };
    let header_h: u16 = if area.height > 4 { 1 } else { 0 };
    // The gap between header and list; collapses with the dispatch and shortcuts gaps on short terminals
    let header_gap_h: u16 = if area.height > 10 { 1 } else { 0 };
    let footer_h: u16 = if area.height >= 2 { 1 } else { 0 };
    // Match the agent prompt and shortcuts gaps; drop on short terminals
    let dispatch_gap_h: u16 = if area.height > 10 { 1 } else { 0 };
    let shortcuts_gap_h: u16 = if area.height > 10 { 1 } else { 0 };
    // Match agent bottom_vpad; drop when height <= 16.
    let bottom_margin_h: u16 = if area.height > 16 { 1 } else { 0 };
    let short_terminal = area.height <= 8;
    (
        top_margin_h,
        header_h,
        header_gap_h,
        footer_h,
        dispatch_gap_h,
        shortcuts_gap_h,
        bottom_margin_h,
        short_terminal,
    )
}

fn dashboard_fixed_overhead(area: Rect) -> (u16, bool) {
    let (
        top_margin_h,
        header_h,
        header_gap_h,
        footer_h,
        dispatch_gap_h,
        shortcuts_gap_h,
        bottom_margin_h,
        short_terminal,
    ) = dashboard_chrome_heights(area);
    let fixed_overhead = top_margin_h
        + header_h
        + header_gap_h
        + footer_h
        + dispatch_gap_h
        + shortcuts_gap_h
        + bottom_margin_h;
    (fixed_overhead, short_terminal)
}

/// Max inner content rows available for a peek under list-first allocation (list floor and peek max fraction).
/// Returns 0 when a peek cannot open.
pub fn max_peek_content_rows(area: Rect) -> u16 {
    if area.height <= 8 {
        return 0;
    }
    let fixed = chrome_overhead(area);
    let probe = allocate_peek(
        area.height,
        fixed,
        // Probe with enough content that allocation uses full max candidate.
        255,
        PEEK_MIN_BOX_LIVE_TAIL,
    );
    probe.max_content_rows
}

/// Like [`compute_layout`] but with a fixed whole peek-box height (from [`allocate_peek`]).
/// The list band receives the rest after chrome.
pub fn compute_layout_with_peek_box(area: Rect, peek_box_h: u16) -> DashboardLayout {
    compute_layout_with_dispatch_inner(area, true, 0, Some(peek_box_h.max(3)))
}

/// Like [`compute_layout`] but lets the caller request a taller dispatch box.
/// `dispatch_text_rows` is the number of text rows the dispatch input wants (at least 1); the box adds 2 more for its top and bottom borders.
/// Used to grow the box as the user inserts newlines (Shift+Enter) so multiline dispatch prompts are fully visible.
///
/// When `peek_visible`, uses list-first [`allocate_peek`] with [`PEEK_MIN_BOX_LIVE_TAIL`].
/// Prefer [`compute_layout_with_peek_box`] when the caller already allocated.
pub fn compute_layout_with_dispatch(
    area: Rect,
    peek_visible: bool,
    dispatch_text_rows: u16,
) -> DashboardLayout {
    compute_layout_with_dispatch_inner(area, peek_visible, dispatch_text_rows, None)
}

fn compute_layout_with_dispatch_inner(
    area: Rect,
    peek_visible: bool,
    dispatch_text_rows: u16,
    forced_peek_box_h: Option<u16>,
) -> DashboardLayout {
    // When `area.height == 0`, every subrect collapses to zero
    // A default `footer_h` of 1 would otherwise produce a non-zero footer rect even on a 0-height area
    if area.height == 0 {
        let z = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 0,
        };
        return DashboardLayout {
            top_margin: z,
            header: z,
            header_gap: z,
            list: z,
            dispatch: z,
            footer: z,
            bottom_margin: z,
        };
    }
    let (
        top_margin_h,
        header_h,
        header_gap_h,
        footer_h,
        dispatch_gap_h,
        shortcuts_gap_h,
        bottom_margin_h,
        short_terminal,
    ) = dashboard_chrome_heights(area);
    let (fixed_overhead, _) = dashboard_fixed_overhead(area);

    // Peek: list-first allocation (see `allocate_peek`). With no peek, normal dispatch chrome applies.
    // `forced_peek_box_h` skips re-allocation when the caller already chose a height (and the peek min for question vs live-tail)
    let dispatch_h: u16 = if let Some(h) = forced_peek_box_h {
        let after = area.height.saturating_sub(fixed_overhead);
        let list_floor = LIST_FLOOR_ROWS.min(after);
        let max_peek = after
            .saturating_sub(list_floor)
            .min(peek_max_box_rows(area.height));
        h.min(max_peek).max(3)
    } else if peek_visible {
        if short_terminal {
            1
        } else {
            let alloc = allocate_peek(
                area.height,
                fixed_overhead,
                dispatch_text_rows,
                PEEK_MIN_BOX_LIVE_TAIL,
            );
            if alloc.show_peek { alloc.peek_box_h } else { 3 }
        }
    } else if !short_terminal {
        2 + dispatch_text_rows.max(1)
    } else {
        1
    };
    // The peek renders inside the dispatch rect (which grows when `peek_visible`, computed above); there is no standalone peek rect
    let remaining = area.height.saturating_sub(
        top_margin_h
            + header_h
            + header_gap_h
            + footer_h
            + dispatch_h
            + dispatch_gap_h
            + shortcuts_gap_h
            + bottom_margin_h,
    );

    let mut y = area.y;
    let top_margin = Rect {
        x: area.x,
        y,
        width: area.width,
        height: top_margin_h,
    };
    y += top_margin_h;
    // Inset the top page header to match the list and dispatch content columns
    let header_inner_pad = HEADER_OUTER_HPAD.saturating_mul(2);
    let header_width = area.width.saturating_sub(header_inner_pad);
    let header_x = if header_width > 0 {
        area.x.saturating_add(HEADER_OUTER_HPAD)
    } else {
        area.x
    };
    let header = Rect {
        x: header_x,
        y,
        width: if header_width > 0 {
            header_width
        } else {
            area.width
        },
        height: header_h,
    };
    y += header_h;

    // 1-row gap between header and list (collapsed on short terminals)
    // Painted by `render_dashboard`'s full-area fill; no sub-renderer touches it
    let header_gap = Rect {
        x: area.x,
        y,
        width: area.width,
        height: header_gap_h,
    };
    y += header_gap_h;

    // Inset the list by LIST_OUTER_HPAD on each side so the row content and group header rules have side breathing room
    // The outer columns stay painted bg_base by the area-wide fill in render_dashboard
    // Mirrors the dispatch inset pattern but with a smaller pad (1 vs 2) because row text is long and dense
    let list_inner_pad = LIST_OUTER_HPAD.saturating_mul(2);
    let list_width = area.width.saturating_sub(list_inner_pad);
    let list_x = if list_width > 0 {
        area.x.saturating_add(LIST_OUTER_HPAD)
    } else {
        area.x
    };
    let list = Rect {
        x: list_x,
        y,
        width: if list_width > 0 {
            list_width
        } else {
            area.width
        },
        height: remaining,
    };
    y += remaining;

    // 1-row gap between the list (or peek) and the dispatch box (mirrors `prompt_gap` in `views::agent::AgentViewLayout`)
    y += dispatch_gap_h;

    // The single-line dispatch input keeps its 2-col outer padding so the `❯` prefix lines up with the row content
    // Rows are indented past the marker column too
    let dispatch_inner_pad = DISPATCH_OUTER_HPAD.saturating_mul(2);
    let dispatch_width = area.width.saturating_sub(dispatch_inner_pad);
    let dispatch_x = if dispatch_width > 0 {
        area.x.saturating_add(DISPATCH_OUTER_HPAD)
    } else {
        area.x
    };
    let dispatch = Rect {
        x: dispatch_x,
        y,
        width: if dispatch_width > 0 {
            dispatch_width
        } else {
            area.width
        },
        height: dispatch_h,
    };
    y += dispatch_h;

    // 1-row gap between the dispatch box and the shortcuts footer (mirrors `shortcuts_gap`)
    y += shortcuts_gap_h;

    let footer = Rect {
        x: area.x,
        y,
        width: area.width,
        height: footer_h,
    };
    y += footer_h;

    // Bottom margin row below the shortcuts bar (matches the agent view's `bottom_vpad`)
    // Painted with `bg_base` by `render_dashboard`'s full-area fill; no sub-renderer ever touches this rect
    let bottom_margin = Rect {
        x: area.x,
        y,
        width: area.width,
        height: bottom_margin_h,
    };

    DashboardLayout {
        top_margin,
        header,
        header_gap,
        list,
        dispatch,
        footer,
        bottom_margin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_assigns_disjoint_areas() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, true);
        // Three blank gap rows sit between the content rects: header_gap, dispatch_gap, and shortcuts_gap
        // The bottom_margin is a real rect painted bg_base, so it counts inside the total
        // The gaps are intentional blank breathing-room rows that the area-wide bg fill paints without any dedicated sub-renderer
        let total = layout.top_margin.height
            + layout.header.height
            + layout.list.height
            + layout.dispatch.height
            + layout.footer.height
            + layout.bottom_margin.height;
        // The 3 rows absorbed by the gaps: header_gap, dispatch_gap, and shortcuts_gap
        assert_eq!(total + 3, area.height);
    }

    /// Multiline dispatch: the box grows by exactly one row per extra text row (2 border rows plus N text rows).
    /// The row list gives up the space so the totals still tile the area.
    #[test]
    fn dispatch_box_grows_for_multiline_input() {
        let area = Rect::new(0, 0, 80, 30);
        let single = compute_layout(area, false);
        // The single-line default is 3 rows: top border, 1 text row, bottom border
        assert_eq!(single.dispatch.height, 3);

        let three = compute_layout_with_dispatch(area, false, 3);
        assert_eq!(
            three.dispatch.height, 5,
            "3 text rows → 2 border + 3 text = 5 rows",
        );
        // The list absorbs the extra two rows the dispatch box took.
        assert_eq!(
            three.list.height + 2,
            single.list.height,
            "the row list must shrink by exactly the dispatch growth",
        );
        // Dispatch still sits above the footer with the same gap.
        assert_eq!(three.footer.y, three.dispatch.y + three.dispatch.height + 1);
    }

    /// A `dispatch_text_rows` of 0 is floored to a single text row so the box never collapses below its single-line chrome.
    #[test]
    fn dispatch_box_floors_at_single_text_row() {
        let area = Rect::new(0, 0, 80, 30);
        let zero = compute_layout_with_dispatch(area, false, 0);
        assert_eq!(zero.dispatch.height, 3, "0 text rows floors to 1 (3 total)");
    }

    /// The dashboard reserves 1 row of bottom margin below the shortcuts bar on tall enough terminals.
    /// Without it the shortcuts sit flush against the alt-screen's bottom edge.
    /// Mirrors the agent view's `bottom_vpad` (`outer_vpad = 1`, dropped to 0 at `area.height <= 16`).
    #[test]
    fn layout_reserves_bottom_margin_on_tall_terminals() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, false);
        assert_eq!(
            layout.bottom_margin.height, 1,
            "tall terminal must reserve a bottom margin row",
        );
        assert_eq!(
            layout.bottom_margin.y,
            layout.footer.y + layout.footer.height,
            "bottom_margin must sit directly below the footer",
        );
        assert_eq!(
            layout.bottom_margin.y + layout.bottom_margin.height,
            area.y + area.height,
            "bottom_margin must extend to the bottom of `area`",
        );
    }

    /// Bottom margin collapses to 0 on short terminals (`area.height <= 16`, matching the agent view's threshold) so the row list isn't starved.
    #[test]
    fn layout_drops_bottom_margin_on_short_terminals() {
        let area = Rect::new(0, 0, 80, 16);
        let layout = compute_layout(area, false);
        assert_eq!(layout.bottom_margin.height, 0);
    }

    /// The dispatch box gets `DISPATCH_OUTER_HPAD` cols of outer padding on each side so its rounded border doesn't reach the terminal edge.
    /// Matches the agent view's `outer_hpad_left/right`.
    #[test]
    fn layout_applies_outer_hpad_to_dispatch_box() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, false);
        assert_eq!(
            layout.dispatch.x,
            area.x + DISPATCH_OUTER_HPAD,
            "dispatch must be inset by DISPATCH_OUTER_HPAD on the left",
        );
        assert_eq!(
            layout.dispatch.width,
            area.width - DISPATCH_OUTER_HPAD * 2,
            "dispatch width must lose DISPATCH_OUTER_HPAD on each side",
        );
    }

    /// Header is inset by HEADER_OUTER_HPAD; list by LIST_OUTER_HPAD.
    /// Footer remains full-width.
    #[test]
    fn layout_insets_header_and_list() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, false);
        assert_eq!(
            layout.header.width,
            area.width - HEADER_OUTER_HPAD * 2,
            "header must be inset by HEADER_OUTER_HPAD on each side",
        );
        assert_eq!(layout.footer.width, area.width);
        assert_eq!(layout.list.width, area.width - LIST_OUTER_HPAD * 2);
    }

    /// The list rect is inset by LIST_OUTER_HPAD cols on each side so row content (markers, rules, text) doesn't touch the terminal edges.
    /// The outer columns remain bg_base (painted by the top-level area fill).
    #[test]
    fn layout_applies_outer_hpad_to_list() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, false);
        assert_eq!(
            layout.list.x,
            area.x + LIST_OUTER_HPAD,
            "list must be inset by LIST_OUTER_HPAD on the left",
        );
        assert_eq!(
            layout.list.width,
            area.width - LIST_OUTER_HPAD * 2,
            "list width must lose LIST_OUTER_HPAD on each side",
        );
    }

    /// The header's horizontal pad matches the list's so the title aligns with the content columns.
    #[test]
    fn layout_applies_outer_hpad_to_header() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, false);
        assert_eq!(HEADER_OUTER_HPAD, LIST_OUTER_HPAD);
        assert_eq!(
            layout.header.x,
            area.x + HEADER_OUTER_HPAD,
            "header must be inset by HEADER_OUTER_HPAD on the left",
        );
        assert_eq!(
            layout.header.width,
            area.width - HEADER_OUTER_HPAD * 2,
            "header width must lose HEADER_OUTER_HPAD on each side",
        );
        assert_eq!(layout.header.x, layout.list.x);
        assert_eq!(layout.header.width, layout.list.width);
    }

    /// A 1-row gap separates the list (or peek) from the dispatch box, and another 1-row gap separates the dispatch box from the footer.
    /// Mirrors `prompt_gap` and `shortcuts_gap` in the agent view's layout.
    #[test]
    fn layout_reserves_dispatch_and_shortcuts_gaps() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, false);
        // 1-row gap before dispatch
        let list_end = layout.list.y + layout.list.height;
        assert_eq!(
            layout.dispatch.y - list_end,
            1,
            "expected 1-row gap between list and dispatch, got {} (list_end={list_end}, dispatch.y={})",
            layout.dispatch.y - list_end,
            layout.dispatch.y,
        );
        // 1-row gap before footer
        let dispatch_end = layout.dispatch.y + layout.dispatch.height;
        assert_eq!(
            layout.footer.y - dispatch_end,
            1,
            "expected 1-row gap between dispatch and footer, got {} (dispatch_end={dispatch_end}, footer.y={})",
            layout.footer.y - dispatch_end,
            layout.footer.y,
        );
    }

    /// Gaps collapse to 0 on short terminals so the row list isn't starved.
    /// The threshold mirrors the top-margin threshold pattern (`> 10`).
    #[test]
    fn layout_drops_gaps_on_short_terminals() {
        let area = Rect::new(0, 0, 80, 10);
        let layout = compute_layout(area, false);
        let list_end = layout.list.y + layout.list.height;
        let dispatch_end = layout.dispatch.y + layout.dispatch.height;
        assert_eq!(
            layout.dispatch.y - list_end,
            0,
            "short terminal must collapse dispatch gap",
        );
        assert_eq!(
            layout.footer.y - dispatch_end,
            0,
            "short terminal must collapse shortcuts gap",
        );
    }

    /// The dashboard reserves a 1-row gap between the header and the row list on tall enough terminals.
    /// The status chips and `Dashboard` label then don't sit flush against the first group header or row.
    /// The gap sits immediately below the header and immediately above the list.
    #[test]
    fn layout_reserves_header_gap_on_tall_terminals() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, false);
        assert_eq!(
            layout.header_gap.height, 1,
            "tall terminal must reserve a header gap row",
        );
        assert_eq!(
            layout.header_gap.y,
            layout.header.y + layout.header.height,
            "header_gap must sit directly below the header",
        );
        assert_eq!(
            layout.list.y,
            layout.header_gap.y + layout.header_gap.height,
            "list must start directly below the header_gap",
        );
    }

    /// The header gap collapses to 0 on short terminals (`area.height <= 10`, the same threshold as the dispatch and shortcuts gaps).
    /// This keeps the row list from being starved.
    #[test]
    fn layout_drops_header_gap_on_short_terminals() {
        let area = Rect::new(0, 0, 80, 10);
        let layout = compute_layout(area, false);
        assert_eq!(
            layout.header_gap.height, 0,
            "short terminal must collapse header_gap",
        );
        assert_eq!(
            layout.list.y,
            layout.header.y + layout.header.height,
            "list must start directly below the header when the gap collapses",
        );
    }

    /// The dashboard reserves one row of top margin on terminals tall enough to spare it, mirroring the welcome view's `v_margin`.
    /// Below the height threshold the margin collapses to 0 so the row list isn't starved.
    #[test]
    fn layout_reserves_top_margin_on_tall_terminals() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, false);
        assert_eq!(
            layout.top_margin.height, 1,
            "tall terminal must reserve a top margin row",
        );
        assert_eq!(
            layout.header.y,
            area.y + 1,
            "header must sit below the top margin",
        );
    }

    /// Short terminals collapse the top margin to 0 so the row list still gets visible space.
    /// The threshold matches the dispatch chrome's threshold (`area.height > 6`).
    #[test]
    fn layout_drops_top_margin_on_short_terminals() {
        let area = Rect::new(0, 0, 80, 6);
        let layout = compute_layout(area, false);
        assert_eq!(layout.top_margin.height, 0);
    }

    #[test]
    fn layout_at_minimum_width_returns_valid_rect() {
        let area = Rect::new(0, 0, MIN_DASHBOARD_WIDTH, 30);
        let layout = compute_layout(area, false);
        // The list is inset by LIST_OUTER_HPAD on each side even at the minimum dashboard width (40 cols, 38 usable for content)
        assert_eq!(layout.list.width, MIN_DASHBOARD_WIDTH - LIST_OUTER_HPAD * 2);
    }

    // Boundary tests for zero-size and below-minimum areas

    /// A zero-height area produces zero-height sub-rects (and doesn't panic on saturating subtraction).
    #[test]
    fn layout_height_zero_produces_zero_subrects() {
        let area = Rect::new(0, 0, 80, 0);
        let layout = compute_layout(area, true);
        assert_eq!(layout.header.height, 0);
        assert_eq!(layout.list.height, 0);
        assert_eq!(layout.dispatch.height, 0);
        assert_eq!(layout.footer.height, 0);
    }

    /// List-first: short heights cannot open a peek (remainder < peek min).
    #[test]
    fn allocate_peek_refuses_when_remainder_below_min() {
        let area = Rect::new(0, 0, 80, 24);
        let fixed = chrome_overhead(area);
        let alloc = allocate_peek(area.height, fixed, 20, PEEK_MIN_BOX_LIVE_TAIL);
        // Chrome takes about 7 rows, leaving 17; the list floor of 12 leaves 5, below the peek min of 8, so no peek opens
        assert!(
            !alloc.show_peek,
            "h=24 should not fit list floor + peek min"
        );
    }

    /// Peek renders inside the dispatch rect, which grows when visible.
    #[test]
    fn layout_grows_dispatch_when_peek_visible() {
        let area = Rect::new(0, 0, 80, 40);
        let no_peek = compute_layout(area, false);
        let with_peek = compute_layout_with_dispatch(area, true, 12);
        assert!(
            with_peek.dispatch.height > no_peek.dispatch.height,
            "peek-visible must grow the dispatch rect, no_peek={} with_peek={}",
            no_peek.dispatch.height,
            with_peek.dispatch.height,
        );
        assert!(with_peek.list.height >= LIST_FLOOR_ROWS);
    }

    /// Larger desired content yields a taller peek box until the max fraction.
    #[test]
    fn peek_box_sizes_to_content_rows() {
        let area = Rect::new(0, 0, 80, 40);
        let small = compute_layout_with_dispatch(area, true, 6);
        let large = compute_layout_with_dispatch(area, true, 20);
        assert!(large.dispatch.height >= small.dispatch.height);
        assert!(large.list.height <= small.list.height);
        assert!(large.list.height >= LIST_FLOOR_ROWS);
        assert!(large.dispatch.height <= peek_max_box_rows(40));
    }

    /// Zero-width area returns valid zero-width rects.
    #[test]
    fn layout_width_zero_returns_zero_width_subrects() {
        let area = Rect::new(0, 0, 0, 30);
        let layout = compute_layout(area, false);
        assert_eq!(layout.list.width, 0);
        assert_eq!(layout.dispatch.width, 0);
    }

    /// A 39-wide area (below `MIN_DASHBOARD_WIDTH=40`) returns valid sub-rects; the renderer will fall back to narrow mode.
    /// The list still receives its outer hpad inset.
    #[test]
    fn layout_width_below_min_returns_valid_subrects() {
        let area = Rect::new(0, 0, MIN_DASHBOARD_WIDTH - 1, 30);
        let layout = compute_layout(area, false);
        assert_eq!(
            layout.list.width,
            MIN_DASHBOARD_WIDTH - 1 - LIST_OUTER_HPAD * 2
        );
        assert!(layout.list.height > 0);
    }

    #[test]
    fn max_peek_content_rows_zero_on_short_terminal() {
        assert_eq!(max_peek_content_rows(Rect::new(0, 0, 80, 8)), 0);
        assert_eq!(max_peek_content_rows(Rect::new(0, 0, 80, 1)), 0);
    }

    #[test]
    fn allocate_peek_list_floor_and_max_fraction() {
        for h in [28u16, 32, 40, 60, 80] {
            let area = Rect::new(0, 0, 80, h);
            let fixed = chrome_overhead(area);
            let alloc = allocate_peek(h, fixed, 255, PEEK_MIN_BOX_LIVE_TAIL);
            assert!(alloc.show_peek, "h={h} should open peek");
            assert!(
                alloc.peek_box_h <= peek_max_box_rows(h),
                "h={h} peek {} > max {}",
                alloc.peek_box_h,
                peek_max_box_rows(h)
            );
            assert!(alloc.peek_box_h >= PEEK_MIN_BOX_LIVE_TAIL);
            let layout = compute_layout_with_peek_box(area, alloc.peek_box_h);
            assert!(
                layout.list.height >= LIST_FLOOR_ROWS,
                "h={h} list {} < floor",
                layout.list.height
            );
            assert_eq!(layout.dispatch.height, alloc.peek_box_h);
        }
    }

    #[test]
    fn allocate_peek_respects_three_eighths_cap() {
        assert_eq!(peek_max_box_rows(40), 15); // floor(40*3/8)
        assert_eq!(peek_max_box_rows(60), 22);
        assert_eq!(peek_max_box_rows(8), 3);
    }

    #[test]
    fn reply_growth_steals_from_list_down_to_floor_then_body() {
        let area = Rect::new(0, 0, 80, 40);
        let fixed = chrome_overhead(area);
        let one = allocate_peek(40, fixed, 6, PEEK_MIN_BOX_LIVE_TAIL);
        let multi = allocate_peek(40, fixed, 14, PEEK_MIN_BOX_LIVE_TAIL);
        assert!(one.show_peek && multi.show_peek);
        assert!(multi.peek_box_h >= one.peek_box_h);
        let layout_multi = compute_layout_with_peek_box(area, multi.peek_box_h);
        assert!(layout_multi.list.height >= LIST_FLOOR_ROWS);
        assert!(multi.peek_box_h <= peek_max_box_rows(40));
    }

    #[test]
    fn layout_header_aligns_with_list_and_dispatch() {
        let area = Rect::new(0, 0, 80, 30);
        let layout = compute_layout(area, false);
        assert_eq!(layout.header.x, layout.list.x);
        assert_eq!(layout.header.x, layout.dispatch.x);
        assert_eq!(layout.header.width, layout.list.width);
        assert_eq!(layout.header.width, layout.dispatch.width);
    }

    #[test]
    fn peek_live_tail_desired_empty_uses_one_body_row() {
        let d = peek_live_tail_desired_content(20, 1, 0, false);
        assert_eq!(d.live_tail, 1);
        assert!(d.blank_row, "empty/hint body still budgets blank when room");
        assert_eq!(d.content_rows, 1 + 1 + 1 + 1); // status+reply+blank+body
    }

    #[test]
    fn peek_live_tail_desired_tight_pin_skips_blank() {
        // fixed is status + reply3 + pin = 5; max_content is fixed + 1, so body 1 and no blank
        let d = peek_live_tail_desired_content(6, 3, 1, true);
        assert!(!d.blank_row);
        assert_eq!(d.live_tail, 1);
        assert_eq!(d.content_rows, 1 + 3 + 1 + 1); // status+reply+pin+body
    }

    #[test]
    fn peek_live_tail_desired_short_body_budgets_blank_and_pin() {
        let d = peek_live_tail_desired_content(40, 1, 2, false);
        assert_eq!(d.live_tail, 2);
        assert!(d.blank_row);
        assert_eq!(d.content_rows, 1 + 1 + 1 + 2);

        let with_pin = peek_live_tail_desired_content(40, 1, 2, true);
        assert_eq!(with_pin.live_tail, 2);
        assert!(with_pin.blank_row);
        assert_eq!(with_pin.content_rows, 1 + 1 + 1 + 1 + 2); // +pin
        assert!(with_pin.content_rows > d.content_rows);
    }

    #[test]
    fn peek_live_tail_desired_long_body_hits_live_tail_cap() {
        let d = peek_live_tail_desired_content(80, 1, 200, false);
        assert_eq!(d.live_tail, MAX_LIVE_TAIL_ROWS);
        assert!(d.blank_row);
        assert_eq!(d.content_rows, 1 + 1 + 1 + MAX_LIVE_TAIL_ROWS);
    }

    #[test]
    fn peek_live_tail_desired_pin_fits_in_measured_body_budget() {
        // A body that fits without the pin must not force an ellipsis solely due to the pin
        // The desired size grows by the pin row, so the paint body_budget still covers the body
        let body = 4u16;
        let d = peek_live_tail_desired_content(40, 1, body, true);
        assert_eq!(d.live_tail, body);
        assert_eq!(
            d.content_rows,
            1 + 1 + 1 + 1 + body,
            "status+reply+pin+blank+body"
        );
    }

    #[test]
    fn peek_live_tail_desired_never_exceeds_max_content() {
        for max_content in 0..=40u16 {
            for reply in 1..=6u16 {
                for body in [0u16, 1, 3, 10, 50, 200] {
                    for pin in [false, true] {
                        let d = peek_live_tail_desired_content(max_content, reply, body, pin);
                        assert!(
                            d.content_rows <= max_content,
                            "content_rows={} > max={max_content} reply={reply} body={body} pin={pin}",
                            d.content_rows
                        );
                        assert!(d.live_tail <= MAX_LIVE_TAIL_ROWS);
                    }
                }
            }
        }
    }
}
