//! OS-level rescue for the modified-Enter chord.
//!
//! Apple Terminal can't deliver Shift/Opt/Cmd + Enter modifier flags via crossterm.
//! We read modifier state through the same OS probe as [`super::keyboard_normalizer`].
//! [`crate::terminal::KeyboardCapabilities::enter_needs_rescue`] gates the rescue so which terminal brands need it is decided in one place.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::terminal::terminal_context;

thread_local! {
    static OS_MODIFIER_RESCUE_SUPPRESSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub struct OsModifierRescueGuard(bool);

impl Drop for OsModifierRescueGuard {
    fn drop(&mut self) {
        OS_MODIFIER_RESCUE_SUPPRESSED.set(self.0);
    }
}

/// Suppress current-modifier rescue while routing a previously queued key.
pub fn suppress_os_modifier_rescue() -> OsModifierRescueGuard {
    let previous = OS_MODIFIER_RESCUE_SUPPRESSED.replace(true);
    OsModifierRescueGuard(previous)
}

pub fn os_modifier_rescue_suppressed() -> bool {
    OS_MODIFIER_RESCUE_SUPPRESSED.get()
}

/// Returns `true` when the user holds a modifier that should turn bare `Enter` into a newline and the terminal is classified as dropping those flags.
pub fn is_apple_terminal_newline_modifier_held() -> bool {
    if os_modifier_rescue_suppressed() {
        return false;
    }
    let ctx = terminal_context();
    if !ctx.keyboard_capabilities().enter_needs_rescue() {
        return false;
    }
    os_any_newline_modifier_held()
}

/// Shift/Alt+Enter, or bare Enter while a newline modifier is held and the terminal drops those flags ([`is_apple_terminal_newline_modifier_held`]).
/// Always requires `KeyCode::Enter` so Shift+Tab / Shift+letters never match.
/// SUPER/Cmd is not included: on most terminals Cmd+Enter is fullscreen or split.
/// Apple Terminal Cmd+Enter is rescued via CoreGraphics on bare Enter ([`is_apple_terminal_newline_modifier_held`]), not the SUPER flag.
pub fn is_mod_enter(key: &KeyEvent) -> bool {
    key.code == KeyCode::Enter
        && (key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
            || is_apple_terminal_newline_modifier_held())
}

#[cfg(target_os = "macos")]
fn os_any_newline_modifier_held() -> bool {
    let s = super::macos_modifiers::snapshot();
    s.shift || s.option || s.command
}

#[cfg(not(target_os = "macos"))]
fn os_any_newline_modifier_held() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn rescue_suppression_is_scoped_and_nestable() {
        assert!(!os_modifier_rescue_suppressed());
        {
            let _outer = suppress_os_modifier_rescue();
            assert!(os_modifier_rescue_suppressed());
            {
                let _inner = suppress_os_modifier_rescue();
                assert!(os_modifier_rescue_suppressed());
            }
            assert!(os_modifier_rescue_suppressed());
        }
        assert!(!os_modifier_rescue_suppressed());
    }

    #[test]
    fn is_mod_enter_requires_enter_code() {
        assert!(is_mod_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));
        assert!(is_mod_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT
        )));
        // SUPER/Cmd is not a product-wide newline chord (fullscreen/split on many terminals)
        // Apple Terminal Cmd+Enter is rescued via CoreGraphics on bare Enter, not via the SUPER flag here
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SUPER
        )));
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL
        )));
        // Shift+Tab must never match (BackTab or Tab+SHIFT).
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::NONE
        )));
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT
        )));
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT
        )));
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::SHIFT
        )));
    }
}
