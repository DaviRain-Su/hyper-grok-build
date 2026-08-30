//! Tip after queuing a follow-up while a turn is running: advertise that bare Enter on an empty prompt force-sends the top queued item ("send now").

use ratatui::style::{Modifier, Style};

use super::EphemeralTip;
use crate::theme::Theme;

/// Dedup key for this tip.
pub(crate) const SEND_NOW_TIP_KEY: &str = "send_now_tip";

/// Key for this tip in the in-memory map counting how many times each tip was shown this session.
pub(crate) const SEND_NOW_TIP_SEEN_KEY: &str = "send_now_tip_shown_count";

/// Stop showing after this many shows within a single session.
const SEND_NOW_TIP_SEEN_CAP: u32 = 3;

/// Build "Queued · Enter to send now", shown at most [`SEND_NOW_TIP_SEEN_CAP`] times per session (in-memory).
///
/// After the user queues a prompt mid-turn the composer is empty, so a second Enter force-sends the top queued follow-up without a special chord.
pub fn send_now_tip() -> EphemeralTip {
    let theme = Theme::current();
    let dim = Style::default().fg(theme.gray);
    let key_style = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::BOLD);
    // `key` is the chord name (a literal key token, not prose) — it stays
    // untranslated and is styled as the key chord span.
    let tpl = rust_i18n::t!("tips.send_now", key = "Enter");
    EphemeralTip::new(
        SEND_NOW_TIP_KEY,
        super::styled_placeholder_line(&tpl, &["Enter"], dim, key_style),
    )
    .with_session_seen_cap(SEND_NOW_TIP_SEEN_KEY, SEND_NOW_TIP_SEEN_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_now_tip_builder_applies_seen_gating() {
        assert_eq!(
            send_now_tip().session_seen.map(|(key, _cap)| key),
            Some(SEND_NOW_TIP_SEEN_KEY)
        );
        assert_eq!(
            send_now_tip().session_seen.map(|(_, cap)| cap),
            Some(SEND_NOW_TIP_SEEN_CAP)
        );
    }

    #[test]
    fn send_now_tip_advertises_enter() {
        let tip = send_now_tip();
        let text: String = tip.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("Enter") && text.contains("send now") && text.contains("Queued"),
            "expected queued/send-now copy with Enter, got {text:?}"
        );
    }
}
