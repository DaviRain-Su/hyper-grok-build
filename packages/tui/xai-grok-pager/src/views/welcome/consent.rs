//! Consent screen: a notice the user must accept before a session starts.
//!
//! Mirrors the folder-trust question, the other blocking welcome screen. The body is laid out by
//! [`crate::app::consent::wrap`], which the validator also uses, so gate and screen agree on width.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;

use super::menu::render_menu;
use super::{
    VersionBadgeMode, WelcomeLayout, WelcomeLayoutInput, WelcomeRenderResult, inset_horizontal,
    prompt, render_logo, render_version_badge,
};
use crate::app::consent::{BodyCell, BodyRow, ConsentLegibility, ConsentNotice, row_cols, wrap};
use crate::theme::Theme;

/// Widest the body is allowed to run, so a long line stays readable on a wide terminal.
const MAX_BODY_COLS: u16 = 78;

/// Title row plus the blank beneath it, matching the trust question's spacing.
const BODY_TOP_OFFSET: u16 = 2;

/// Below this the fallback message is shortened, because the long one would itself be clipped.
const NARROW_COLS: u16 = 40;

const TOO_SMALL: &str = "Enlarge the window to read this notice";
const TOO_SMALL_NARROW: &str = "Window too small";

pub fn render_consent(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    notice: &ConsentNotice,
    selected: Option<usize>,
    pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    h_margin: u16,
    compact: bool,
) -> WelcomeRenderResult {
    let body_width = content_area
        .width
        .saturating_sub(h_margin * 2)
        .min(MAX_BODY_COLS);
    let rows = wrap(&notice.body, body_width);

    let (layout, legibility) = fit_body(content_area, compact, &rows);
    let message = inset_horizontal(layout.error, h_margin);

    render_logo(layout.logo, buf, theme, content_area.height);

    if legibility.can_accept() {
        paint_centered(
            message,
            buf,
            Style::default().fg(theme.gray_bright),
            &notice.title,
        );
        paint_body(message, buf, theme, &rows);
    } else {
        // Title dropped: on a screen this small, why the notice is unreadable matters more.
        let text = if message.width < NARROW_COLS {
            TOO_SMALL_NARROW
        } else {
            TOO_SMALL
        };
        paint_centered(message, buf, Style::default().fg(theme.gray), text);
    }

    // Accept is refused while the body is unread, so the row is withheld rather than offered and
    // ignored. Quit stays, or the screen would show no way out at all.
    let menu_items: &[(&str, &str)] = if legibility.can_accept() {
        &[("a", notice.accept_label.as_str()), ("q", "Quit")]
    } else {
        &[("q", "Quit")]
    };
    let menu_area = inset_horizontal(layout.menu, prompt::prompt_inset(compact));
    let menu_rects = render_menu(menu_area, buf, theme, menu_items, selected, None, 0);

    // The version row is the only free row, and a pending Ctrl+C matters more than the badge.
    if let Some(pending) = &pending_hint {
        super::render_pending_hint(layout.version, buf, theme, pending);
    } else {
        render_version_badge(
            layout.version,
            buf,
            theme,
            None,
            h_margin,
            false,
            VersionBadgeMode::Full {
                subscription_tier: None,
            },
        );
    }

    WelcomeRenderResult {
        menu_rects,
        consent_legibility: Some(legibility),
        ..Default::default()
    }
}

/// A body that does not fit is laid out around the fallback message instead, or it lands
/// on top of the menu.
fn fit_body(
    content_area: Rect,
    compact: bool,
    rows: &[BodyRow],
) -> (WelcomeLayout, ConsentLegibility) {
    // No prompt is painted here, so the rows compute_stacked reserves go to the body.
    let stacked = |message_height: u16| {
        WelcomeLayout::compute_stacked(WelcomeLayoutInput {
            content_area,
            error_height: message_height,
            menu_height: 2,
            prompt_height: Some(0),
            compact,
            prompt_compact: compact,
            ..Default::default()
        })
    };

    // Title, blank, body, then the spacer the menu sits below.
    let mut layout = stacked(rows.len() as u16 + BODY_TOP_OFFSET + 1);
    let legibility =
        if layout.error.height >= rows.len() as u16 + BODY_TOP_OFFSET && !rows.is_empty() {
            ConsentLegibility::Painted
        } else {
            layout = stacked(2);
            ConsentLegibility::Illegible
        };
    (layout, legibility)
}

/// Centres each row the way the trust question does.
fn paint_body(area: Rect, buf: &mut Buffer, theme: &Theme, rows: &[BodyRow]) {
    let top = area.y + BODY_TOP_OFFSET;
    for (index, row) in rows.iter().enumerate() {
        let y = top + index as u16;
        if y >= area.y + area.height {
            break;
        }
        let row = trim_end(row);
        let x = area.x + area.width.saturating_sub(row_cols(row)) / 2;
        let text: String = row.iter().map(|cell| cell.text.as_str()).collect();
        buf.set_span(
            x,
            y,
            &Span::styled(text, Style::default().fg(theme.gray)),
            row_cols(row),
        );
    }
}

/// A trailing space left by the wrap would underline past the label and shift the centring.
fn trim_end(row: &[BodyCell]) -> &[BodyCell] {
    let end = row
        .iter()
        .rposition(|cell| cell.text != " ")
        .map_or(0, |i| i + 1);
    &row[..end]
}

/// Centred within the message block rather than the full width, and ellipsized, so an oversized
/// title cannot run edge to edge.
fn paint_centered(area: Rect, buf: &mut Buffer, style: Style, text: &str) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let text = crate::render::line_utils::truncate_str(text, area.width as usize);
    let cols = unicode_width::UnicodeWidthStr::width(&*text) as u16;
    let x = area.x + area.width.saturating_sub(cols) / 2;
    buf.set_span(x, area.y, &Span::styled(text, style), cols);
}

#[cfg(test)]
#[path = "consent_tests.rs"]
mod tests;
