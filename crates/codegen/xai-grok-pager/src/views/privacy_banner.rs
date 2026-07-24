//! Coding-data sharing upsell banner (Figma "Data Sharing Upsell",
//! node 8698:3690). Shared by the welcome tip slot and the agent-view
//! banner slot; visibility is gated by `AppView::privacy_banner_should_show`.

use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// Legal line copy — used for both render spans and mouse hit width.
const PRIVACY_BANNER_LEGAL: &str = "Learn more and read Terms and Privacy Policy.";

/// Click target for the legal line links.
pub(crate) const PRIVACY_BANNER_LEGAL_URL: &str = "https://x.ai/legal";

/// Hit rects returned by [`render`] for mouse handling.
pub(crate) struct PrivacyBannerRects {
    pub accept: Rect,
    pub customize: Rect,
    pub legal: Rect,
}

/// Render the banner: copy left, `[Customize in settings]` / `[Accept]`
/// right, legal links on the second row. Needs `area.height >= 2`.
/// Hover styling mirrors the plugin CTA buttons.
pub(crate) fn render(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    mouse_pos: Option<(u16, u16)>,
) -> PrivacyBannerRects {
    let customize_label = "[Customize in settings]";
    let accept_label = "[Accept]";
    let right_w = (customize_label.len() + 1 + accept_label.len()) as u16;
    // Buttons render whole or not at all: a clipped/overflowing [Accept]
    // must never leave a click target in the blank margin (a stray click
    // there would silently opt the user in).
    let buttons_fit = area.width > right_w;
    let left_w = if buttons_fit {
        area.width - right_w - 1
    } else {
        area.width
    };

    let left = Rect {
        x: area.x,
        y: area.y,
        width: left_w,
        height: area.height.min(2),
    };
    let right = Rect {
        x: area.x + left_w + 1,
        y: area.y,
        width: right_w,
        height: 1,
    };

    let hovered = |r: Rect| {
        mouse_pos.is_some_and(|(mx, my)| r.contains(ratatui::layout::Position::new(mx, my)))
    };

    let legal_w = if left.width as usize >= PRIVACY_BANNER_LEGAL.len() {
        PRIVACY_BANNER_LEGAL.len()
    } else {
        "Learn more".len().min(left.width as usize)
    };
    // The legal line only exists when the slot really has a second row —
    // otherwise its rect would make the blank row below clickable.
    let legal_rect = if area.height >= 2 {
        Rect {
            x: left.x,
            y: left.y.saturating_add(1),
            width: legal_w as u16,
            height: 1,
        }
    } else {
        Rect::default()
    };

    // Figma node 8698:3806: title fg/primary, description fg/secondary,
    // legal line fg/tertiary with underlined links in the same color.
    // The whole legal line is one click target, so its links brighten together.
    let link_fg = if hovered(legal_rect) {
        theme.gray_bright
    } else {
        theme.gray
    };
    let link = Style::default()
        .fg(link_fg)
        .add_modifier(Modifier::UNDERLINED);
    let gray = Style::default().fg(theme.gray);
    let title = Span::styled("Help improve Grok", Style::default().fg(theme.text_primary));
    let desc = "Allow your sessions to improve SpaceXAI's models.";
    // Drop trailing spans whole rather than clipping mid-word when narrow.
    let line1 = if left.width as usize >= "Help improve Grok  ".len() + desc.len() {
        Line::from(vec![
            title,
            Span::raw("  "),
            Span::styled(desc, Style::default().fg(theme.gray_bright)),
        ])
    } else {
        Line::from(title)
    };
    // Span pieces must reassemble to PRIVACY_BANNER_LEGAL.
    let line2 = if left.width as usize >= PRIVACY_BANNER_LEGAL.len() {
        Line::from(vec![
            Span::styled("Learn more", link),
            Span::styled(" and read ", gray),
            Span::styled("Terms", link),
            Span::styled(" and ", gray),
            Span::styled("Privacy Policy", link),
            Span::styled(".", gray),
        ])
    } else {
        Line::from(Span::styled("Learn more", link))
    };
    Paragraph::new(vec![line1, line2]).render(left, buf);

    if !buttons_fit {
        return PrivacyBannerRects {
            accept: Rect::default(),
            customize: Rect::default(),
            legal: legal_rect,
        };
    }
    let customize_rect = Rect {
        x: right.x,
        y: right.y,
        width: customize_label.len() as u16,
        height: 1,
    };
    let accept_rect = Rect {
        x: right.x + customize_label.len() as u16 + 1,
        y: right.y,
        width: accept_label.len() as u16,
        height: 1,
    };
    let customize_style = if hovered(customize_rect) {
        Style::default().fg(theme.text_primary).bg(theme.bg_hover)
    } else {
        Style::default().fg(theme.gray_bright)
    };
    let accept_style = if hovered(accept_rect) {
        Style::default().fg(theme.link_fg).bg(theme.bg_hover)
    } else {
        Style::default().fg(theme.text_primary)
    };
    buf.set_stringn(
        customize_rect.x,
        customize_rect.y,
        customize_label,
        customize_rect.width as usize,
        customize_style,
    );
    buf.set_stringn(
        accept_rect.x,
        accept_rect.y,
        accept_label,
        accept_rect.width as usize,
        accept_style,
    );
    PrivacyBannerRects {
        accept: accept_rect,
        customize: customize_rect,
        legal: legal_rect,
    }
}
