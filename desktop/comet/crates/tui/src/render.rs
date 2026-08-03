//! Drawing.
//!
//! The information architecture is comet-native's, read from the desktop shell
//! (`comet-ui/src/shell.rs::render_chat_sidebar`, `shell/spaces.rs`,
//! `shell/tabs.rs`) — **not** the original Electron app's single grouped list:
//!
//! - the sidebar has **two sections**, Spaces and a *flat global* Sessions list;
//! - the selected space's own sessions are the **tab strip** above the
//!   transcript, which is also the header (it replaced one).
//!
//! The visual language is opencode's: **nothing in the steady-state view is
//! drawn with a line**. No borders, no rules, no boxes, no tees, not even a
//! divider between the panes. Regions are told apart by which step of the
//! background ramp they are filled with — see [`crate::theme`], which carries
//! that ramp and the reasoning behind it.
//!
//! Three fills do all the work. The sidebar is a panel. The prompt is a block
//! that lifts from panel to element when it takes focus — the same move a menu
//! row makes when the cursor lands on it, so focus is a *level*, not a stroke.
//! The active tab is the one filled block in its row, which is what makes it the
//! active one. Everything else is the terminal's own background.
//!
//! Two numbers keep it aligned: [`MAIN_PAD`], the column of terminal background
//! down each side of the main pane that gives a fill somewhere to end, and
//! [`PAD`], the padding every region puts inside itself. Their sum is the one
//! column all content in the pane starts at.
//!
//! ```text
//!  Spaces                +     ● Ratatui terminal…    ● Diff sidebar…    +
//!  ▪ comet-native  this dev
//!  ▪ soccertcg     this dev                            the user's prompt
//!                                                                  14:32
//!  Sessions
//!  ● Ratatui terminal…  now    The assistant replies as plain text.
//!    comet-native · comet/…
//!  ● Rebalance player…   2h    ⌄ Ran 2 commands
//!    soccertcg · comet/re…       Run   cargo test --workspace
//!
//!                              ◜ Working · 11s · Ctrl-X to interrupt
//!  Wing Lee
//!  w@example.com               Do anything…
//!
//!                              Fable 5  High        ⎇ main  Worktree
//! ```
//!
//! Two habits keep the per-frame cost flat:
//!
//! - **Only visible rows are built.** The sidebar walks its row list until the
//!   pane is full; the transcript hands out cached lines by reference
//!   ([`Transcript::for_each_visible`]).
//! - **The transcript is never filled.** Every filled cell is an SGR sequence
//!   ratatui must emit, and a fill also takes away the user's own background and
//!   its transparency. opencode spends that on the sidebar, the prompt and the
//!   menus and leaves `background` transparent; the largest and most-scrolled
//!   region here stays free for the same reason.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};

use comet_proto::view::{ConnectionStatus, GatePhase};

use crate::app::{App, ChipKind, Hit, Overlay, Row};
use crate::keys::{Focus, HELP};
use crate::loaders;
use crate::theme::{self, Theme};
use crate::wrap;

/// Sidebar width. Wide enough for a title plus a time column and a readable
/// "project · branch" sub-line, capped so a narrow terminal keeps a usable
/// transcript.
const SIDEBAR_MAX: u16 = 32;
const SIDEBAR_MIN: u16 = 22;

/// Columns the sidebar insets its rows from its own fill, per side.
const SIDEBAR_PAD: u16 = 2;
/// The composer grows with its content up to this many text rows.
const COMPOSER_MAX_ROWS: u16 = 8;
/// The main pane's own margin — one column of terminal background down each
/// side, so a filled region inside it has somewhere to end.
const MAIN_PAD: u16 = 1;

/// Padding inside every region of the main pane, per side.
///
/// The tabs, the transcript, the working strip and the prompt all use this one
/// number, so a single content column runs down the whole pane instead of four
/// that nearly line up. Everything that consumes it is handed an
/// area already inset by [`MAIN_PAD`], so content lands at `MAIN_PAD + PAD` from
/// the pane's edge.
pub(crate) const PAD: usize = 2;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let area = frame.area();
    if area.width < 24 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("terminal too small").style(theme.subtle()),
            area,
        );
        return;
    }

    // A cell grid has no widget tree, so clicks resolve against a map the draw
    // rebuilds as it goes.
    app.clear_hits();

    // The app's own surface, under everything. Without it the blocks are judged
    // against whatever colour the user's terminal happens to be, and a block
    // only reads as raised against a surface it can be compared to.
    fill(frame, area, theme.base());

    // Both panes run the terminal's full height. A pane that stops short of the
    // top or bottom reads as a floating card rather than as one side of the
    // window, and the sidebar's fill is doing the work a divider used to.
    let body = area;

    let sidebar_width = if app.sidebar_visible {
        SIDEBAR_MAX
            .min(body.width / 3)
            .max(SIDEBAR_MIN.min(body.width / 2))
    } else {
        0
    };
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(12)]).areas(body);

    if sidebar_width > 0 {
        app.push_hit(sidebar, Hit::Pane(Focus::Sidebar));
        draw_sidebar(frame, sidebar, app, &theme);
    }
    draw_main(frame, main, app, &theme);

    if let Some(overlay) = &app.overlay {
        let panel = draw_overlay(frame, body, app, overlay, &theme);
        app.push_hit(body, Hit::Overlay);
        let _ = panel;
    }

    if app.help {
        // Everything below the tab strip, which stays readable (along with the
        // sidebar's Spaces header opposite it) so you keep your place while
        // reading the key map.
        let panel = Rect {
            y: body.y + TAB_ROWS,
            height: body.height.saturating_sub(TAB_ROWS),
            ..body
        };
        // Registered last, so it wins over everything beneath it.
        app.push_hit(panel, Hit::Overlay);
        draw_help(frame, panel, &theme);
    }
}

/// A status dot. A running session shows comet's 2×3 mini gradient spinner —
/// one braille cell, the same ring chase the desktop session rows animate —
/// while every other state is the static dot, coloured by meaning.
fn status_dot(
    status: comet_proto::ChatIndicator,
    app: &App,
    theme: &Theme,
    base: Style,
) -> (String, Style) {
    if status == comet_proto::ChatIndicator::Working {
        let (glyph, tint) = loaders::mini_spinner(app.elapsed());
        return (glyph, base.patch(Style::default().fg(tint)));
    }
    (theme::DOT.to_string(), base.patch(theme.dot(status)))
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

/// Paint a region's background.
///
/// This is the whole separation mechanic. ratatui's `Cell::set_style` patches
/// per-field, so a span carrying only a foreground leaves the fill underneath
/// intact — fill once here, then draw text with plain `fg` styles and the region
/// keeps its colour. It also means the *order* matters: fill before text.
pub(crate) fn fill(frame: &mut Frame, area: Rect, style: Style) {
    if area.width > 0 && area.height > 0 {
        frame.render_widget(Block::default().style(style), area);
    }
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    // No divider. The fill's own edge is the boundary — opencode's sidebar is a
    // `backgroundPanel` box with no border at all, and a stroke drawn along a
    // colour change is that boundary stated twice.
    fill(frame, area, theme.panel());
    // The fill reaches every edge; the *content* is inset from them. Two columns
    // each side and a row at the top, which puts "Spaces" on the same row as the
    // tab titles opposite it.
    let inner = Rect {
        x: area.x + SIDEBAR_PAD,
        y: area.y + 1,
        width: area.width.saturating_sub(SIDEBAR_PAD * 2),
        height: area.height.saturating_sub(2),
    };
    if inner.height == 0 || inner.width < 4 {
        return;
    }

    // The user row is pinned to the bottom, as in the original; everything else
    // scrolls above it.
    let user = app
        .rows
        .iter()
        .position(|row| matches!(row, Row::User { .. }));
    let (list_rows, footer_rows): (&[Row], &[Row]) = match user {
        Some(at) => (&app.rows[..at], &app.rows[at..]),
        None => (&app.rows, &[]),
    };
    let footer_height: u16 = footer_rows.iter().map(|row| row.height()).sum();
    let list_height = inner.height.saturating_sub(footer_height);

    // Scroll so the cursor's row is visible. Rows are 1–2 lines tall, so the
    // window is computed in lines, not indices.
    let first = first_visible(list_rows, app.cursor, list_height);
    // Collected before drawing, because drawing needs `&App` while registering
    // needs `&mut App`.
    let mut visible: Vec<(usize, Rect)> = Vec::new();
    let mut y = inner.y;
    for (index, row) in list_rows.iter().enumerate().skip(first) {
        let height = row.height();
        if y + height > inner.y + list_height {
            break;
        }
        visible.push((index, Rect { y, height, ..inner }));
        y += height;
    }
    let footer_start = inner.y + list_height;
    let mut footer: Vec<(usize, Rect)> = Vec::new();
    let mut y = footer_start;
    for (offset, row) in footer_rows.iter().enumerate() {
        footer.push((
            list_rows.len() + offset,
            Rect {
                y,
                height: row.height(),
                ..inner
            },
        ));
        y += row.height();
    }

    for (index, slot) in visible.iter().chain(footer.iter()) {
        if app.rows[*index].selectable() {
            app.push_hit(*slot, Hit::Row(*index));
        }
        // The Spaces header's affordance is its own target.
        if let Row::Section {
            action: Some(_),
            label,
        } = &app.rows[*index]
            && label == "Spaces"
        {
            let width = 3u16.min(slot.width);
            app.push_hit(
                Rect {
                    x: slot.x + slot.width - width,
                    width,
                    ..*slot
                },
                Hit::AddSpace,
            );
        }
    }
    for (index, slot) in visible.into_iter().chain(footer) {
        let selected = index == app.cursor && index < app.rows.len();
        let row = app.rows[index].clone();
        draw_sidebar_row(frame, slot, app, &row, selected, theme);
    }
}

/// First row index to draw so `cursor` fits in `height` lines.
fn first_visible(rows: &[Row], cursor: usize, height: u16) -> usize {
    let mut first = 0usize;
    loop {
        let mut used = 0u16;
        let mut visible_end = first;
        for row in &rows[first..] {
            if used + row.height() > height {
                break;
            }
            used += row.height();
            visible_end += 1;
        }
        if cursor < visible_end || first + 1 >= rows.len() {
            return first;
        }
        first += 1;
    }
}

fn draw_sidebar_row(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    row: &Row,
    selected: bool,
    theme: &Theme,
) {
    let width = area.width as usize;
    // The cursor's fill runs the pane's full width, not the content column's: a
    // fill that stopped at the text inset would read as a highlight on the words
    // rather than as the row being selected.
    let bleed = Rect {
        x: area.x.saturating_sub(SIDEBAR_PAD),
        width: area.width + SIDEBAR_PAD * 2,
        ..area
    };
    match row {
        // Label left, affordance right — herdr's header row.
        Row::Section { label, action } => {
            frame.render_widget(
                Paragraph::new(Span::styled(label.clone(), theme.label())),
                area,
            );
            if let Some(action) = action {
                let action_width = wrap::width_of(action) as u16;
                if action_width < area.width {
                    frame.render_widget(
                        Paragraph::new(Span::styled(action.clone(), theme.hint())),
                        Rect {
                            x: area.x + area.width - action_width,
                            width: action_width,
                            ..area
                        },
                    );
                }
            }
        }
        Row::Space {
            label,
            device,
            attention,
            offline,
            ..
        } => {
            let base = if selected {
                theme.selected()
            } else {
                Style::default()
            };
            if selected {
                fill(frame, bleed, theme.selected());
            }
            // The attention dot only appears when a member session is live, so a
            // quiet space stays quiet.
            let (dot, dot_style) = match attention {
                Some(status) => status_dot(*status, app, theme, base),
                None => (" ".to_string(), base),
            };
            let device = if *offline {
                format!("{device} · offline")
            } else {
                device.clone()
            };
            let device_width = wrap::width_of(&device);
            let label_width = width.saturating_sub(3 + device_width).max(1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(dot.to_string(), dot_style),
                    Span::styled(
                        format!(" {}", wrap::truncate(label, label_width)),
                        base.patch(if selected {
                            theme.body()
                        } else {
                            theme.subtle()
                        }),
                    ),
                ])),
                area,
            );
            let tail = device_width as u16;
            if tail < area.width {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        device.clone(),
                        base.patch(if *offline {
                            Style::default().fg(theme.warning)
                        } else {
                            theme.hint()
                        }),
                    )),
                    Rect {
                        x: area.x + area.width - tail,
                        width: tail,
                        ..area
                    },
                );
            }
        }
        Row::Blank => {}
        Row::Empty { label } => {
            frame.render_widget(
                Paragraph::new(Span::styled(label.clone(), theme.hint())),
                area,
            );
        }
        Row::Chat {
            id,
            title,
            location,
            indicator,
            archived,
            activity,
            ..
        } => {
            let base = if selected {
                theme.selected()
            } else {
                Style::default()
            };
            if selected {
                fill(frame, bleed, theme.selected());
            }
            let open = app.selected_chat.as_deref() == Some(id.as_str());
            let when = app.relative_time(*activity);
            // dot(1) + gap(1) + a column of air + the time column.
            let title_width = width.saturating_sub(3 + wrap::width_of(&when)).max(1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    {
                        let (glyph, style) = status_dot(*indicator, app, theme, base);
                        Span::styled(glyph, style)
                    },
                    Span::styled(
                        format!(" {}", wrap::truncate(title, title_width)),
                        base.patch(if open { theme.body() } else { theme.subtle() }),
                    ),
                ])),
                Rect { height: 1, ..area },
            );
            let time_width = wrap::width_of(&when) as u16;
            if time_width < area.width {
                frame.render_widget(
                    Paragraph::new(Span::styled(when.clone(), base.patch(theme.hint()))),
                    Rect {
                        x: area.x + area.width - time_width,
                        width: time_width,
                        height: 1,
                        ..area
                    },
                );
            }
            // The sub-line sits under the title, aligned past the dot.
            if area.height > 1 {
                let mut sub = location.clone().unwrap_or_default();
                if *archived {
                    sub = if sub.is_empty() {
                        "archived".into()
                    } else {
                        format!("{sub} · archived")
                    };
                }
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        // Aligned past the dot and its gap, so the sub-line hangs
                        // under the title rather than under the status column.
                        format!("  {}", wrap::truncate(&sub, width.saturating_sub(2))),
                        base.patch(theme.hint()),
                    )),
                    Rect {
                        y: area.y + 1,
                        height: 1,
                        ..area
                    },
                );
            }
        }
        Row::User { name, email } => {
            // With no display name the email takes the top line rather than
            // leaving a gap where a name would have been.
            let lead = if name.is_empty() { email } else { name };
            frame.render_widget(
                Paragraph::new(Span::styled(wrap::truncate(lead, width), theme.subtle())),
                Rect { height: 1, ..area },
            );
            if area.height > 1 && !email.is_empty() && email != name {
                frame.render_widget(
                    Paragraph::new(Span::styled(wrap::truncate(email, width), theme.hint())),
                    Rect {
                        y: area.y + 1,
                        height: 1,
                        ..area
                    },
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main panel
// ---------------------------------------------------------------------------

fn draw_main(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    // A column of terminal background down each side of the main pane.
    //
    // Not decoration: the sidebar is a panel fill and so is an unfocused prompt,
    // so without this gutter the two would meet at the same colour and read as
    // one continuous shape. A filled region needs somewhere to end, and on the
    // left that somewhere is this column.
    let area = Rect {
        x: area.x + MAIN_PAD,
        width: area.width.saturating_sub(MAIN_PAD * 2),
        ..area
    };
    // The tab strip IS the header — in the desktop shell it replaced one
    // (`shell/tabs.rs`), so there is no separate title row. A blank row, not a
    // rule, separates it from the transcript: the active tab already carries a
    // fill, so a stroke underneath would be the same boundary drawn twice.
    let [tabs, rest] =
        Layout::vertical([Constraint::Length(TAB_ROWS), Constraint::Min(4)]).areas(area);
    draw_tab_strip(frame, tabs, app, theme);

    match app.gate() {
        GatePhase::Ready => {}
        phase => {
            draw_gate(frame, rest, app, theme, phase);
            return;
        }
    }

    // Every row below the transcript is a fixed height, and deliberately so: the
    // strip, the prompt block and the footer all keep their row whether or not
    // they have anything to say. A pane that resizes as state arrives makes the
    // prompt jump under the cursor while you are typing into it.
    // A row of base under the prompt. The prompt is a block, not a pane: a block
    // that runs into the terminal's bottom edge reads as clipped, the same way
    // its meta row read as clipped when it sat on the fill's edge.
    let text_rows = composer_rows(app, rest.width);
    let [transcript, strip, composer, floor] = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(text_rows + COMPOSER_CHROME),
        Constraint::Length(1),
    ])
    .areas(rest);
    let _ = floor;

    app.push_hit(transcript, Hit::Pane(Focus::Transcript));
    app.push_hit(composer, Hit::Pane(Focus::Composer));
    draw_transcript(frame, transcript, app, theme);
    draw_status_strip(frame, strip, app, theme);
    draw_composer(frame, composer, text_rows, app, theme);
}

/// Width of one tab. The desktop strip uses a fixed 140px; a fixed column width
/// is the same idea, and keeps tabs from jittering as titles stream in. Wide
/// enough that a real session title gets a few words in before the ellipsis —
/// at 22 nearly every tab truncated to the same unhelpful prefix.
const TAB_WIDTH: usize = 26;

/// The narrowest a tab is allowed to get before the strip starts overflowing
/// instead.
///
/// Tabs shrink to fit first, but only so far: a strip of eight sessions all
/// truncated to "Rebuild C…" is not more navigable than four you can actually
/// read plus a marker saying there are more. This floor is what decides which
/// of those two you get.
const TAB_MIN: usize = 22;

/// Air between tabs. One column: the separation a tab needs comes from the
/// padding *inside* its own fill, not from pushing its neighbours away — a wide
/// gap just spreads the strip out without making any single tab easier to read.
const TAB_GAP: u16 = 1;

/// Shown in place of a tab when the strip has more either side than it can fit.
/// A strip that silently drops tabs is one you cannot navigate, because nothing
/// on screen says there is anywhere to navigate *to*.
const TAB_MORE_LEFT: &str = "‹";
const TAB_MORE_RIGHT: &str = "›";

/// Padding inside a tab, per side.
const TAB_PAD: u16 = 2;

/// Rows the tab strip occupies: the labels, with one row above and below.
///
/// The rows either side are only *half* filled for the active tab — `▄` above,
/// `▀` below — because a full row of fill on each side was too much and a bare
/// label was too little, and half a cell is the one size between them a
/// terminal can actually draw. (opencode plays the same trick on its prompt
/// border with `╹`.) The empty halves double as the strip's separation from the
/// terminal edge above and the transcript below, so no extra gap row is needed.
const TAB_ROWS: u16 = 3;

/// The session tab strip: every non-archived session of the selected space,
/// with `+` to start another. The active tab carries the selection wash.
fn draw_tab_strip(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    // The whole row is the strip; the engine label moved to the status line so
    // the tabs could have these columns.
    let strip_width = area.width;

    let tabs = app.tabs();
    if tabs.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                match app.selected_space {
                    Some(_) => "  No sessions here — n starts one",
                    None => "  No space selected",
                },
                theme.hint(),
            )),
            Rect {
                y: area.y + 1,
                width: strip_width.max(1),
                height: 1,
                ..area
            },
        );
    } else {
        // Scroll so the active tab is visible; `+` always trails the last tab.
        // Both ends reserve a column for an overflow marker whenever there is
        // something past them, so the window never sits flush against a tab it
        // is hiding.
        let active = tabs.iter().position(|tab| tab.active).unwrap_or(0);
        // Reserve the overflow markers either side and the trailing `+`, so a
        // full strip never pushes the one control that starts a session off the
        // end of the row.
        let room = strip_width.saturating_sub(8) as usize;
        // Fit them all if they will fit at a readable width; only then overflow.
        let tab_width = (room / tabs.len().max(1))
            .saturating_sub(TAB_GAP as usize)
            .clamp(TAB_MIN, TAB_WIDTH);
        let stride = tab_width + TAB_GAP as usize;
        let visible = (room / stride).max(1);
        let first = active.saturating_sub(visible.saturating_sub(1));
        let last = (first + visible).min(tabs.len());

        // Labels sit on the strip's middle row; the active tab's half-blocks
        // occupy the rows either side.
        let label_y = area.y + 1;
        let mut x = area.x;
        if first > 0 {
            frame.render_widget(
                Paragraph::new(Span::styled(TAB_MORE_LEFT, theme.hint())),
                Rect {
                    x,
                    y: label_y,
                    width: 1,
                    height: 1,
                },
            );
        }
        x += 2;

        for (offset, tab) in tabs.iter().enumerate().skip(first).take(visible) {
            let remaining = (area.x + strip_width).saturating_sub(x);
            let slot = Rect {
                x,
                y: area.y,
                width: (tab_width as u16).min(remaining),
                height: TAB_ROWS,
            };
            if slot.width < 6 {
                break;
            }
            app.push_hit(slot, Hit::Tab(offset));
            // The active tab is the only filled one — the desktop's
            // `white_alpha(0.08)` against a transparent inactive tab. Being the
            // one raised block in the row is what marks it, so nothing else has
            // to: no underline, no brackets, no brighter dot.
            let base = if tab.active {
                theme.element()
            } else {
                Style::default()
            };
            if tab.active {
                fill(
                    frame,
                    Rect {
                        y: label_y,
                        height: 1,
                        ..slot
                    },
                    base,
                );
                // Half a row of fill above and below the label. Under NO_COLOR
                // there is no element colour to draw them in, and the plain
                // theme's reversed label carries the tab alone.
                if !theme.plain {
                    for (glyph, y) in [("▄", slot.y), ("▀", slot.y + 2)] {
                        frame.render_widget(
                            Paragraph::new(Span::styled(
                                glyph.repeat(slot.width as usize),
                                Style::default().fg(theme.element),
                            )),
                            Rect {
                                y,
                                height: 1,
                                ..slot
                            },
                        );
                    }
                }
            }
            let inner = Rect {
                x: slot.x + TAB_PAD,
                y: label_y,
                width: slot.width.saturating_sub(TAB_PAD * 2),
                height: 1,
            };
            let title_width = (inner.width as usize).saturating_sub(2);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    {
                        let (glyph, style) = status_dot(tab.indicator, app, theme, base);
                        Span::styled(glyph, style)
                    },
                    Span::styled(
                        format!(" {}", wrap::truncate(&tab.title, title_width)),
                        base.patch(if tab.active {
                            theme.body()
                        } else {
                            theme.subtle()
                        }),
                    ),
                ])),
                inner,
            );
            x += slot.width + TAB_GAP;
        }
        if last < tabs.len() {
            frame.render_widget(
                Paragraph::new(Span::styled(TAB_MORE_RIGHT, theme.hint())),
                Rect {
                    x: x.saturating_sub(TAB_GAP),
                    y: label_y,
                    width: 1,
                    height: 1,
                },
            );
            x += 1;
        }
        if x + 3 <= area.x + strip_width {
            let plus = Rect {
                x,
                y: label_y,
                width: 3,
                height: 1,
            };
            app.push_hit(plus, Hit::NewSession);
            frame.render_widget(Paragraph::new(Span::styled(" + ", theme.hint())), plus);
        }
    }
}

fn draw_transcript(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    // The full pane. A capped reading measure left a band of dead space down the
    // right of every wide terminal, and the blocks inside stopped short of the
    // pane they were supposed to be filling.
    let column = area;
    // Lay out against the real column before reading anything from it; this is
    // the only place the transcript learns its width, so a resize lands here.
    app.lay_out_transcript(column.width, column.height as usize);

    let inset = " ".repeat(PAD);
    if app.selected_chat.is_none() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::default(),
                Line::from(Span::styled(
                    format!("{inset}No session open."),
                    theme.subtle(),
                )),
                Line::from(Span::styled(
                    format!("{inset}Press n to start one."),
                    theme.hint(),
                )),
            ]),
            column,
        );
        return;
    }
    if app.transcript.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{inset}Nothing here yet."),
                theme.hint(),
            ))),
            column,
        );
        return;
    }

    let buffer = frame.buffer_mut();
    app.transcript
        .for_each_visible(column.height as usize, |row, line| {
            // Rendered by reference: drawing a frame clones no line and
            // allocates no `Text`.
            line.render(
                Rect {
                    y: column.y + row,
                    height: 1,
                    ..column
                },
                buffer,
            );
        });
}

/// The reserved strip above the composer (the original's `h-6`): the working
/// indicator, or the scroll notice. Reserved even when empty so the composer
/// never shifts under the cursor.
fn draw_status_strip(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    // A notice outranks everything else here: it is the only channel the app has
    // for "that didn't work", and it used to live on a status bar that no longer
    // exists. This row is already reserved, so it costs nothing to land here.
    if let Some(notice) = &app.notice {
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    "{}{}",
                    " ".repeat(PAD),
                    wrap::truncate(
                        &wrap::sanitize(&notice.text),
                        (area.width as usize).saturating_sub(PAD)
                    )
                ),
                Style::default().fg(theme.warning),
            )),
            area,
        );
        return;
    }
    if let Some(elapsed) = app.working_elapsed() {
        let seconds = elapsed.as_secs();
        // The same indicator the session rows and tabs use. One running session
        // must not be drawn two different ways depending on where you look.
        let (glyph, tint) = loaders::mini_spinner(app.elapsed());
        let mut spans = vec![
            Span::raw(" ".repeat(PAD)),
            Span::styled(glyph, Style::default().fg(tint)),
        ];
        spans.push(Span::styled(" Working".to_string(), theme.subtle()));
        spans.push(Span::styled(
            format!(" · {seconds}s · Ctrl-X to interrupt"),
            theme.hint(),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    if !app.transcript.following() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("{}Scrolled back · G to follow", " ".repeat(PAD)),
                theme.hint(),
            )),
            area,
        );
    }
}

/// How many text rows the prompt wants, clamped.
fn composer_rows(app: &App, width: u16) -> u16 {
    let inner = composer_text_width(width);
    let rows = app.composer.lay_out(inner as usize).rows.len() as u16;
    rows.clamp(1, COMPOSER_MAX_ROWS)
}

/// Text width inside the composer: the block, less its padding on both sides.
fn composer_text_width(width: u16) -> u16 {
    width.saturating_sub(COMPOSER_PAD * 2).max(1)
}

/// Columns of padding inside the composer fill, per side (opencode's prompt uses
/// two, and at one the text reads as stuck to the edge of the block).
const COMPOSER_PAD: u16 = PAD as u16;

/// The prompt's accent bar. opencode marks its prompt with a `SplitBorder` — one
/// heavy stroke down the left — and the user's messages carry the same mark.
const BAR: &str = "┃";

/// Rows the composer block adds around its text: a row of air on top, a row of
/// air under the text, the meta row, and a row of air under *that*. The bottom
/// pad is not optional — without it the meta row sits flush on the block's edge
/// and the whole prompt reads as clipped. The meta row is always present too;
/// see [`App::composer_meta`] for why it can never be conditional.
const COMPOSER_CHROME: u16 = 4;

fn draw_composer(frame: &mut Frame, area: Rect, text_rows: u16, app: &mut App, theme: &Theme) {
    if area.height == 0 || area.width < 6 {
        return;
    }
    let focused = app.focus == Focus::Composer;

    // The prompt is a block like the user's messages above it, and carries the
    // same accent bar — the two things in the transcript that are *yours* look
    // alike. The bar is where focus lives: indigo while you are in it, and the
    // fill's own muted edge when you are not. Nothing changes shape, so nothing
    // reflows under the caret.
    let surface = theme.element();
    fill(frame, area, surface);
    let bar_style =
        surface.patch(Style::default().fg(if focused { theme.accent } else { theme.faint }));
    for offset in 0..area.height {
        frame.render_widget(
            Paragraph::new(Span::styled(BAR, bar_style)),
            Rect {
                x: area.x,
                y: area.y + offset,
                width: 1,
                height: 1,
            },
        );
    }

    let text_area = Rect {
        x: area.x + COMPOSER_PAD,
        y: area.y + 1,
        width: composer_text_width(area.width),
        height: text_rows,
    };

    // The meta row is the second-to-last row: the last one is the block's bottom
    // padding, which is what keeps the meta row off the edge.
    if area.height >= COMPOSER_CHROME {
        draw_composer_meta(
            frame,
            Rect {
                y: area.y + area.height - 2,
                height: 1,
                ..text_area
            },
            app,
            theme,
            surface,
        );
    }

    if app.composer.is_empty() {
        // The original's placeholder, verbatim.
        frame.render_widget(
            Paragraph::new(Span::styled("Do anything…", theme.hint())),
            text_area,
        );
        if focused {
            draw_caret(frame, text_area.x, text_area.y, theme);
            frame.set_cursor_position(Position {
                x: text_area.x,
                y: text_area.y,
            });
        }
        return;
    }

    let laid = app.composer.lay_out(text_area.width as usize);
    let visible = text_area.height as usize;
    let first = laid.cursor_row.saturating_sub(visible.saturating_sub(1));
    for (offset, row) in laid.rows.iter().skip(first).take(visible).enumerate() {
        frame.render_widget(
            Paragraph::new(Span::styled(row.clone(), theme.body())),
            Rect {
                y: text_area.y + offset as u16,
                height: 1,
                ..text_area
            },
        );
    }
    if focused {
        let row = laid.cursor_row.saturating_sub(first) as u16;
        if row < text_area.height {
            let x = text_area.x + (laid.cursor_col as u16).min(text_area.width.saturating_sub(1));
            let y = text_area.y + row;
            draw_caret(frame, x, y, theme);
            frame.set_cursor_position(Position { x, y });
        }
    }
}

/// The composer's meta row: `Opus 5  High` on the left, `⎇ main  Worktree` on
/// the right.
///
/// One row, two clusters, which is exactly how opencode builds its prompt's
/// meta row — `justifyContent="space-between"`, agent and model on the left and
/// `props.right` opposite. The desktop splits these across an actions row and a
/// footer because it has the pixels; a terminal does not, and putting all four
/// settings on one line groups them better than stacking them anyway.
///
/// Model and effort are one chip visually, as on the desktop where they were
/// merged into a single button — the model at full weight because it answers
/// "who is replying", the effort muted beside it because it qualifies that
/// answer. Each stays its own hit target, though: the pickers behind them are
/// genuinely different questions, and a terminal can afford the precision.
fn draw_composer_meta(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme, surface: Style) {
    if area.width == 0 {
        return;
    }
    let faint = surface.patch(Style::default().fg(theme.faint));
    let muted = surface.patch(Style::default().fg(theme.muted));

    // Right cluster first: it is the one that must not be pushed off, and its
    // width decides how much room the model label has to truncate into.
    let mut right = area.right();
    if let Some(footer) = app.composer_footer() {
        let checkout_width = wrap::width_of(&footer.checkout) as u16;
        if right.saturating_sub(checkout_width) > area.x {
            right -= checkout_width;
            let slot = Rect {
                x: right,
                width: checkout_width,
                height: 1,
                ..area
            };
            // Only a draft can still move: a session's checkout kind was fixed
            // when it was created, so a hit here would offer a choice that no
            // longer exists.
            if footer.checkout_live {
                app.push_hit(slot, Hit::Chip(ChipKind::Checkout));
            }
            frame.render_widget(
                Paragraph::new(Span::styled(footer.checkout.clone(), faint)),
                slot,
            );
        }
        let reference = format!("⎇ {}", footer.reference);
        let width = wrap::width_of(&reference) as u16 + 2;
        if right.saturating_sub(width) > area.x {
            right -= width;
            let slot = Rect {
                x: right,
                width,
                height: 1,
                ..area
            };
            app.push_hit(slot, Hit::Chip(ChipKind::Branch));
            frame.render_widget(
                Paragraph::new(Span::styled(format!("{reference}  "), muted)),
                slot,
            );
        }
    }

    // Left cluster, into whatever the right one left.
    let mut x = area.x;
    let room = right.saturating_sub(x) as usize;
    let model = wrap::truncate(&app.composer_meta().model, room);
    let model_width = wrap::width_of(&model) as u16;
    if model_width == 0 {
        return;
    }
    let slot = Rect {
        x,
        width: model_width,
        height: 1,
        ..area
    };
    app.push_hit(slot, Hit::Chip(ChipKind::Model));
    frame.render_widget(Paragraph::new(Span::styled(model, surface)), slot);
    x += model_width;

    if let Some(effort) = app.composer_meta().effort {
        let width = wrap::width_of(&effort) as u16 + 2;
        if x + width <= right {
            let slot = Rect {
                x,
                width,
                height: 1,
                ..area
            };
            app.push_hit(slot, Hit::Chip(ChipKind::Reasoning));
            frame.render_widget(
                Paragraph::new(Span::styled(format!("  {effort}"), faint)),
                slot,
            );
        }
    }
}

/// Paint the caret cell ourselves.
///
/// The terminal's own cursor is still placed (screen readers and cursor-shape
/// settings follow it), but relying on it alone makes a *trailing space*
/// invisible: the row is drawn without it, so there is nothing to tell you
/// whether you typed one. A block on the caret cell answers that at a glance.
fn draw_caret(frame: &mut Frame, x: u16, y: u16, theme: &Theme) {
    let area = frame.area();
    if x >= area.right() || y >= area.bottom() {
        return;
    }
    let cell = Rect {
        x,
        y,
        width: 1,
        height: 1,
    };
    let under = frame.buffer_mut()[(x, y)].symbol().to_string();
    let glyph = if under.trim().is_empty() {
        " ".to_string()
    } else {
        under
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            glyph,
            Style::default().fg(theme.selection).bg(theme.text),
        )),
        cell,
    );
}

// ---------------------------------------------------------------------------
// Hints, gates, overlays
// ---------------------------------------------------------------------------

fn draw_gate(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, phase: GatePhase) {
    let (title, body): (String, Vec<String>) = match phase {
        GatePhase::Loading => (
            "Starting the engine…".into(),
            vec![
                "Attaching to a running engine, or starting one if none is up.".into(),
                "It will keep running after you close this terminal.".into(),
            ],
        ),
        GatePhase::Failed(err) => (
            "Can't reach the engine".into(),
            err.lines().map(str::to_string).collect(),
        ),
        GatePhase::SignIn => (
            "Sign in".into(),
            vec![
                "The engine has no session.".into(),
                "Run `comet login` in another terminal, then `comet daemon restart`.".into(),
            ],
        ),
        GatePhase::OrgGate => (
            "Choose a workspace".into(),
            vec![
                "This account has no workspace selected.".into(),
                "Pick one in the desktop app; the TUI follows the engine's choice.".into(),
            ],
        ),
        GatePhase::Ready => return,
    };

    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            format!("  {title}"),
            theme.body().add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];
    let width = area.width.saturating_sub(4) as usize;
    for paragraph in body {
        for chunk in wrap::wrap(&wrap::sanitize(&paragraph), width, "") {
            lines.push(Line::from(Span::styled(
                format!("  {chunk}"),
                theme.subtle(),
            )));
        }
    }
    lines.push(Line::default());
    if matches!(app.connection, ConnectionStatus::Connecting) {
        let mut spans = vec![Span::raw("  ".to_string())];
        spans.extend(loaders::comet_wave(app.elapsed(), theme.text));
        lines.push(Line::from(spans));
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        "  r  retry now      q  quit",
        theme.hint(),
    )));
    frame.render_widget(Paragraph::new(lines), area);

    // While connecting, the animated comet mark sits above the message — the
    // desktop app's boot splash, at terminal resolution.
    if matches!(app.connection, ConnectionStatus::Connecting) {
        let mark = loaders::comet_mark(app.elapsed(), theme);
        let height = mark.len() as u16;
        let width = 14u16;
        if area.height > height + 8 && area.width > width + 4 {
            frame.render_widget(
                Paragraph::new(mark),
                Rect {
                    x: area.x + 2,
                    y: area.y + area.height - height - 1,
                    width,
                    height,
                },
            );
        }
    }
}

fn draw_help(frame: &mut Frame, area: Rect, theme: &Theme) {
    // Clear the whole body, not just the panel. A centered panel over live
    // content leaves the right-hand ends of long transcript lines floating
    // beside it, which reads as a rendering fault rather than as a modal.
    frame.render_widget(Clear, area);

    let widest = HELP
        .iter()
        .map(|(key, what)| wrap::width_of(key) + 2 + wrap::width_of(what))
        .max()
        .unwrap_or(40) as u16;
    let width = (widest + 6).min(area.width.saturating_sub(2));
    let height = (HELP.len() as u16 + 3).min(area.height);
    let panel = centred(area, width, height);
    let inner = draw_panel(frame, panel, theme, "Keys");

    let key_column = HELP
        .iter()
        .map(|(key, _)| wrap::width_of(key))
        .max()
        .unwrap_or(0);
    for (index, (key, what)) in HELP.iter().take(inner.height as usize).enumerate() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{key:>key_column$}  "), theme.panel()),
                Span::styled(
                    wrap::truncate(what, (inner.width as usize).saturating_sub(key_column + 2)),
                    theme.panel_hint(),
                ),
            ]))
            .style(theme.panel()),
            Rect {
                y: inner.y + index as u16,
                height: 1,
                ..inner
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Floating panels
// ---------------------------------------------------------------------------

/// A right-click menu, the model picker, or a rename prompt.
///
/// All three are the same panel: a raised wash on a cleared rect ([`draw_panel`]).
/// Only the menu is anchored at the pointer — the others are centred, because
/// they are about the session rather than about the row you happened to click.
fn draw_overlay(
    frame: &mut Frame,
    body: Rect,
    app: &App,
    overlay: &Overlay,
    theme: &Theme,
) -> Rect {
    match overlay {
        Overlay::Menu {
            title,
            items,
            active,
            column,
            row,
        } => {
            let width = items
                .iter()
                .map(|item| wrap::width_of(&item.label))
                .chain(std::iter::once(wrap::width_of(title)))
                .max()
                .unwrap_or(12) as u16
                + 4;
            // A separated item gets a blank row above it, not a rule: inside a
            // wash, air already groups.
            let separators = items.iter().filter(|item| item.separated).count() as u16;
            let height = items.len() as u16 + separators + 2;
            // Flip the anchor when the menu would run off an edge.
            let x = (*column)
                .min(body.right().saturating_sub(width + 1))
                .max(body.x);
            let y = (*row + 1)
                .min(body.bottom().saturating_sub(height))
                .max(body.y);
            let panel = Rect {
                x,
                y,
                width: width.min(body.width),
                height: height.min(body.height),
            };
            let inner = draw_panel(frame, panel, theme, title);

            let mut y = inner.y;
            for (index, item) in items.iter().enumerate() {
                if item.separated && y < inner.bottom() {
                    y += 1;
                }
                if y >= inner.bottom() {
                    break;
                }
                draw_panel_row(
                    frame,
                    Rect {
                        y,
                        height: 1,
                        ..inner
                    },
                    theme,
                    &item.label,
                    None,
                    0,
                    index == *active,
                );
                y += 1;
            }
            panel
        }

        Overlay::Refs { active } => {
            let refs = app_refs(app);
            let rows: Vec<(String, Option<String>)> = match &refs {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|candidate| {
                        // The same two tags the desktop ref picker shows.
                        let mut tags = Vec::new();
                        if candidate.current {
                            tags.push("current");
                        }
                        if candidate.worktree_path.is_some() {
                            tags.push("worktree");
                        }
                        (
                            candidate.name.clone(),
                            (!tags.is_empty()).then(|| tags.join(" · ")),
                        )
                    })
                    .collect(),
                Some(_) => vec![("Not a git checkout".into(), None)],
                None => vec![("Loading…".into(), None)],
            };
            list_card(frame, body, theme, "Branch", &rows, refs.is_some(), *active)
        }

        Overlay::Checkout { active } => {
            // Two modes, three outcomes: "current" reads differently depending
            // on whether the picked ref is already a worktree.
            let picked = app
                .draft
                .as_ref()
                .and_then(|draft| draft.selected_ref().cloned());
            let local = comet_proto::view::checkout_label(
                comet_proto::view::CheckoutKind::Local,
                picked.as_ref(),
            );
            let rows = vec![
                (
                    local.to_string(),
                    Some("run where the space already points".into()),
                ),
                (
                    "New worktree".to_string(),
                    Some("isolated checkout off the picked ref".into()),
                ),
            ];
            list_card(frame, body, theme, "Run in", &rows, true, *active)
        }

        Overlay::Reasoning { levels, active } => {
            let rows: Vec<(String, Option<String>)> = levels
                .iter()
                .map(|level| (crate::app::reasoning_label(*level).to_string(), None))
                .collect();
            list_card(frame, body, theme, "Effort", &rows, true, *active)
        }

        Overlay::Models { models, active } => {
            let rows: Vec<(String, Option<String>)> = match models {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|model| (model.label.clone(), model.description.clone()))
                    .collect(),
                Some(_) => vec![("No models for this harness".into(), None)],
                None => vec![("Loading…".into(), None)],
            };
            // Same panel as every other picker — a model list is a list.
            list_card(
                frame,
                body,
                theme,
                "Model",
                &rows,
                models.is_some(),
                *active,
            )
        }

        Overlay::Prompt { title, input, .. } => {
            let width = 52u16.min(body.width.saturating_sub(4));
            let panel = centred(body, width, 4);
            let inner = draw_panel(frame, panel, theme, title);
            let laid = input.lay_out(inner.width as usize);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    laid.rows.first().cloned().unwrap_or_default(),
                    theme.panel(),
                ))
                .style(theme.panel()),
                Rect { height: 1, ..inner },
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "Enter to confirm · Esc to cancel",
                    theme.panel_hint(),
                ))
                .style(theme.panel()),
                Rect {
                    y: inner.y + 1,
                    height: 1,
                    ..inner
                },
            );
            let caret_x = inner.x + (laid.cursor_col as u16).min(inner.width.saturating_sub(1));
            draw_caret(frame, caret_x, inner.y, theme);
            frame.set_cursor_position(Position {
                x: caret_x,
                y: inner.y,
            });
            panel
        }
    }
}

/// The draft's branches, if a draft is open.
fn app_refs(app: &App) -> Option<Vec<comet_proto::RepoRef>> {
    app.draft.as_ref().and_then(|draft| draft.refs.clone())
}

/// A centred list panel: label plus a muted trailing detail per row.
/// `selectable` is false while a list is still loading, so nothing highlights.
fn list_card(
    frame: &mut Frame,
    body: Rect,
    theme: &Theme,
    title: &str,
    rows: &[(String, Option<String>)],
    selectable: bool,
    active: usize,
) -> Rect {
    let width = 56u16.min(body.width.saturating_sub(4));
    let height = (rows.len() as u16 + 2).min(body.height.saturating_sub(2));
    let panel = centred(body, width, height);
    let inner = draw_panel(frame, panel, theme, title);

    // Details start at one column for the whole card, so they read as a second
    // column rather than as ragged tails chasing labels of different lengths.
    // Capped at half the card, or one long label would push every detail off.
    let detail_col = rows
        .iter()
        .filter(|(_, detail)| detail.is_some())
        .map(|(label, _)| wrap::width_of(label) + 2)
        .max()
        .unwrap_or(0)
        .min(width as usize / 2);

    for (index, (label, detail)) in rows.iter().enumerate() {
        let y = inner.y + index as u16;
        if y >= inner.bottom() {
            break;
        }
        draw_panel_row(
            frame,
            Rect {
                y,
                height: 1,
                ..inner
            },
            theme,
            label,
            detail.as_deref(),
            detail_col,
            selectable && index == active,
        );
    }
    panel
}

/// One row of a floating panel: a label, an optional muted detail in a column
/// beside it, and the selection fill when it is the active one.
fn draw_panel_row(
    frame: &mut Frame,
    slot: Rect,
    theme: &Theme,
    label: &str,
    detail: Option<&str>,
    detail_col: usize,
    selected: bool,
) {
    // The selection spans the panel's full width — a cursor that stopped at the
    // text's padding would read as a chip rather than as "you are here".
    let base = if selected {
        theme.selected()
    } else {
        theme.panel()
    };
    fill(frame, slot, base);
    let inner = Rect {
        x: slot.x + PANEL_PAD,
        width: slot.width.saturating_sub(PANEL_PAD * 2),
        ..slot
    };
    let mut spans = vec![Span::styled(
        wrap::truncate(label, inner.width as usize),
        if selected {
            base
        } else {
            base.patch(Style::default().fg(theme.muted))
        },
    )];
    if let Some(detail) = detail {
        // Pad out to the shared column; a label that overran it still gets its
        // two spaces so the two never collide.
        let gap = detail_col.saturating_sub(wrap::width_of(label)).max(2);
        let room = (inner.width as usize).saturating_sub(wrap::width_of(label) + gap);
        if room > 4 {
            spans.push(Span::styled(
                format!("{}{}", " ".repeat(gap), wrap::truncate(detail, room)),
                base.patch(Style::default().fg(theme.faint)),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

/// Padding inside a floating panel, per side. Matches the composer's, so every
/// filled region in the app indents its contents by the same amount.
const PANEL_PAD: u16 = 2;

/// The shared panel chrome: a panel-level fill on a cleared rect, its title on
/// the first row. Returns the rows the caller fills.
///
/// No border, because the fill already *is* one — a hairline box around a region
/// that is already a different colour is the same edge drawn twice, and on a
/// terminal the box also costs two columns and two rows of the content it
/// surrounds. This is both opencode's dialog (a `backgroundPanel` box, nothing
/// else) and comet's own desktop menu.
fn draw_panel(frame: &mut Frame, panel: Rect, theme: &Theme, title: &str) -> Rect {
    frame.render_widget(Clear, panel);
    fill(frame, panel, theme.panel());
    if panel.height == 0 || panel.width < PANEL_PAD * 2 + 2 {
        return panel;
    }
    let inner_width = panel.width - PANEL_PAD * 2;
    frame.render_widget(
        Paragraph::new(Span::styled(
            wrap::truncate(title, inner_width as usize),
            theme.panel_hint(),
        )),
        Rect {
            x: panel.x + PANEL_PAD,
            width: inner_width,
            height: 1,
            ..panel
        },
    );
    Rect {
        x: panel.x,
        y: panel.y + 1,
        width: panel.width,
        height: panel.height - 2,
    }
}

fn centred(body: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: body.x + (body.width.saturating_sub(width)) / 2,
        y: body.y + (body.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}
