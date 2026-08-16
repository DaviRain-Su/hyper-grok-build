use super::*;

fn notice() -> ConsentNotice {
    ConsentNotice {
        id: "tos-2026".to_string(),
        version: 2,
        title: "Updated Terms".to_string(),
        body: "Review the Acceptable Use Policy. Now's the time.".to_string(),
        accept_label: "I accept".to_string(),
    }
}

fn render(width: u16, height: u16) -> (Buffer, WelcomeRenderResult) {
    render_with(width, height, &notice(), None)
}

fn render_with(
    width: u16,
    height: u16,
    notice: &ConsentNotice,
    pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
) -> (Buffer, WelcomeRenderResult) {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    let result = render_consent(
        area,
        &mut buf,
        &Theme::current(),
        notice,
        Some(0),
        pending_hint,
        2,
        false,
    );
    (buf, result)
}

/// Wide graphemes occupy two cells and ratatui blanks the second, so the buffer reads back padded.
fn unpadded(row: &str) -> String {
    row.chars().filter(|c| *c != ' ').collect()
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn screen(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn row_containing(buf: &Buffer, needle: &str) -> Option<String> {
    (0..buf.area.height)
        .map(|y| row_text(buf, y))
        .find(|row| row.contains(needle))
}

/// Measured in characters, a wide-character notice is clipped to half its columns while still
/// reporting itself as painted.
#[test]
fn a_wide_character_notice_is_measured_in_columns() {
    let body = "利用規約を確認してください。";
    let wide = ConsentNotice {
        body: body.to_string(),
        ..notice()
    };

    let (buf, result) = render_with(100, 40, &wide, None);

    assert_eq!(result.consent_legibility, Some(ConsentLegibility::Painted));

    let painted = (0..buf.area.height)
        .map(|y| unpadded(&row_text(&buf, y)))
        .find(|row| row.starts_with('利'))
        .expect("the body paints");
    assert_eq!(painted, body, "the tail must not be clipped");
}

#[test]
fn a_pending_double_press_replaces_the_version_badge() {
    use crossterm::event::{KeyCode, KeyModifiers};

    let hint = crate::views::shortcuts_bar::PendingHint {
        shortcut: crate::input::key::KeyShortcut::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        label: "quit",
    };

    let (buf, _) = render_with(100, 40, &notice(), Some(hint));

    let screen = screen(&buf);
    assert!(screen.contains("press again to quit"), "{screen}");
    assert!(!screen.contains("Grok Build"), "{screen}");
}

/// `Buffer::set_line` ignores a line's alignment, so the centring is done by hand;
/// an oversized title is ellipsized inside the margin rather than running to the screen edge.
#[test]
fn the_title_is_centred_and_stays_inside_the_margin() {
    let title = "Updated Terms";

    let (buf, _) = render(100, 40);

    let row = row_containing(&buf, title).expect("the title paints");
    assert_eq!(
        row.find(title).expect("title column") as u16,
        (buf.area.width - title.len() as u16) / 2,
    );

    let width = 60u16;
    let margin = 2u16;
    let long = ConsentNotice {
        title: "T".repeat(200),
        ..notice()
    };

    let (buf, _) = render_with(width, 40, &long, None);

    let painted = row_containing(&buf, "TT").expect("the title paints");
    assert!(painted.ends_with('…'), "{painted:?}");
    assert!(
        painted.trim_start().chars().count() <= (width - margin * 2) as usize,
        "the title must stay inside the margin: {painted:?}",
    );
}

/// Quit must stay even when the body is illegible: it is the only way out.
#[test]
fn an_unreadable_body_withholds_accept_but_still_offers_quit() {
    let (small, small_result) = render(40, 10);
    let (large, large_result) = render(100, 40);

    assert_eq!(
        small_result.consent_legibility,
        Some(ConsentLegibility::Illegible)
    );
    assert!(
        screen(&small).contains("Window too small"),
        "{}",
        screen(&small)
    );

    assert!(!screen(&small).contains("I accept"), "{}", screen(&small));
    assert_eq!(small_result.menu_rects.len(), 1);

    assert!(screen(&large).contains("I accept"));
    assert_eq!(large_result.menu_rects.len(), 2);

    for painted in [screen(&small), screen(&large)] {
        assert!(painted.contains("Quit"), "{painted}");
    }
}

/// The label is remote input, and the key hint paints at the right edge over whatever reaches it.
#[test]
fn a_long_accept_label_cannot_overwrite_its_key_hint() {
    let long = ConsentNotice {
        accept_label: "I have read and accept the updated enterprise terms of service".to_string(),
        ..notice()
    };

    let (buf, result) = render_with(46, 40, &long, None);

    let row = result.menu_rects[0];
    let painted = row_text(&buf, row.y);
    assert!(
        painted.ends_with('a'),
        "the key hint must survive: {painted:?}"
    );
    assert!(painted.contains('…'), "the label must be cut: {painted:?}");
}

#[test]
fn the_largest_allowed_body_paints_on_a_standard_terminal() {
    let rows = crate::app::consent::MAX_CONSENT_BODY_ROWS;
    let body = (0..rows)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let tallest = ConsentNotice { body, ..notice() };

    let (_, result) = render_with(80, 21, &tallest, None);

    assert_eq!(result.consent_legibility, Some(ConsentLegibility::Painted));
}
