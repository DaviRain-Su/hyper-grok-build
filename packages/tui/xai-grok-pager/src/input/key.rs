//! Key shortcut types and the `key!()` macro.
//!
//! A focused subset for
//! ergonomic key matching and test construction.
//!
//! ```
//! use xai_grok_pager::input::key::key;
//!
//! // Simple key
//! let q = key!('q');
//!
//! // Key with modifier
//! let ctrl_c = key!('c', CONTROL);
//! let ctrl_shift_z = key!('z', CONTROL | SHIFT);
//!
//! // Match against a crossterm KeyEvent
//! use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
//! let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
//! assert!(ctrl_c.matches(&event));
//! ```

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A key + modifiers pair for matching against crossterm events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyShortcut {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyShortcut {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }.normalize_case()
    }

    /// Simple key (no modifiers). Normalizes case.
    pub fn key(code: KeyCode) -> Self {
        Self::new(code, KeyModifiers::NONE)
    }

    /// Key with Ctrl modifier. Normalizes case.
    pub fn ctrl(code: KeyCode) -> Self {
        Self::new(code, KeyModifiers::CONTROL)
    }

    /// Normalize Shift+lowercase ↔ uppercase.
    fn normalize_case(mut self) -> Self {
        let c = match self.code {
            KeyCode::Char(c) => c,
            _ => return self,
        };
        if c.is_ascii_uppercase() {
            self.modifiers.insert(KeyModifiers::SHIFT);
        } else if self.modifiers.contains(KeyModifiers::SHIFT) {
            self.code = KeyCode::Char(c.to_ascii_uppercase());
        }
        self
    }

    /// Check if a crossterm KeyEvent matches this shortcut.
    /// Normalizes the event's case before comparing, so both
    /// `Char('G') + NONE` and `Char('g') + SHIFT` match `key!('G')`.
    pub fn matches(&self, event: &KeyEvent) -> bool {
        if event.kind == KeyEventKind::Release {
            return false;
        }
        let normalized = Self::new(event.code, event.modifiers);
        self.code == normalized.code && self.modifiers == normalized.modifiers
    }

    /// Build a KeyEvent (Press) for tests.
    pub fn to_key_event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }

    /// Display string for UI (shortcuts bar, etc.).
    /// Delegates to `fmt::Display`.
    pub fn display(&self) -> String {
        self.to_string()
    }

    /// Pretty display for the all-shortcuts cheatsheet modal.
    ///
    /// Uses `Ctrl+Q` style instead of the compact `ctrl-q` / `C-q` bar
    /// style. Shift is always shown explicitly (e.g. `Shift+G`,
    /// `Ctrl+Shift+P`, `Shift+Tab`).
    pub fn display_pretty(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        // SUPER first, spelled per-platform like Opt/Alt (Cmd on macOS).
        if self.modifiers.contains(KeyModifiers::SUPER) {
            parts.push(
                if cfg!(target_os = "macos") {
                    "Cmd"
                } else {
                    "Super"
                }
                .into(),
            );
        }
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl".into());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push(
                if cfg!(target_os = "macos") {
                    "Opt"
                } else {
                    "Alt"
                }
                .into(),
            );
        }
        let has_shift = self.modifiers.contains(KeyModifiers::SHIFT);
        if has_shift {
            parts.push("Shift".into());
        }
        // BackTab is Shift+Tab but doesn't carry SHIFT in modifiers —
        // inject "Shift" before the key name if not already present.
        if self.code == KeyCode::BackTab && !has_shift {
            parts.push("Shift".into());
        }
        parts.push(match self.code {
            KeyCode::Char(' ') => "Space".into(),
            KeyCode::Char(c) => c.to_ascii_lowercase().to_string(),
            KeyCode::Enter => "Enter".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Up => "\u{2191}".into(),
            KeyCode::Down => "\u{2193}".into(),
            KeyCode::Left => "\u{2190}".into(),
            KeyCode::Right => "\u{2192}".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "Page Up".into(),
            KeyCode::PageDown => "Page Down".into(),
            KeyCode::F(n) => format!("F{n}"),
            _ => format!("{:?}", self.code),
        });
        parts.join("+")
    }

    /// True iff this shortcut is a bare ASCII letter (no modifiers other
    /// than SHIFT). Used by the vim-mode gate in `ActionRegistry::lookup_with_mode`
    /// to decide whether a `When::ScrollbackFocused` binding should be
    /// suppressed when vim mode is off.
    pub fn is_letter_or_shift_letter(&self) -> bool {
        let KeyCode::Char(c) = self.code else {
            return false;
        };
        if !c.is_ascii_alphabetic() {
            return false;
        }
        let mods = self.modifiers;
        mods.is_empty() || mods == KeyModifiers::SHIFT
    }
}

pub fn is_paste_key(key: &KeyEvent) -> bool {
    if key!('v', CONTROL).matches(key) || key!('v', SUPER).matches(key) {
        return true;
    }
    // Windows-only escape hatch: Windows Terminal's default Ctrl+V is a
    // text-only `paste` action that silently drops image clipboards
    // (Win+Shift+S, browser "Copy Image"). Alt+V is unbound in default
    // WT profiles and reaches us as a normal keypress. macOS excluded
    // (`Opt+V` types `√`); Linux excluded (no interceptor to escape).
    // Doesn't collide with AltGr — AltGr arrives as `Ctrl+Alt`, not
    // bare `Alt`, and `KeyShortcut::matches` is exact-modifier.
    #[cfg(target_os = "windows")]
    if key!('v', ALT).matches(key) {
        return true;
    }
    false
}

pub fn is_inline_paste_key(key: &KeyEvent) -> bool {
    key!('v', CONTROL | SHIFT).matches(key) || key!('v', SUPER | SHIFT).matches(key)
}

/// Ctrl+Z / Cmd+Z — the textarea's undo binding. Delegates to the owning
/// crate's predicate so the chord can never desync from what the key does.
pub fn is_undo_key(key: &KeyEvent) -> bool {
    xai_ratatui_textarea::is_undo_input(key)
}

// On Windows, AltGr arrives as Ctrl+Alt; on other platforms it's composed before reaching us.
#[cfg(target_os = "windows")]
#[inline]
pub fn is_altgr(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

#[cfg(not(target_os = "windows"))]
#[inline]
pub fn is_altgr(_modifiers: KeyModifiers) -> bool {
    false
}

/// Canonical Shift+Tab encodings: `BackTab` (most xterm-likes),
/// `BackTab+SHIFT` (some terminals), `Tab+SHIFT` (kitty protocol, some
/// Windows terminals). Single source of truth for the `CycleMode` /
/// `DashboardCycleMode` ActionDefs and [`is_shift_tab`].
pub fn shift_tab_keys() -> [KeyShortcut; 3] {
    [
        KeyShortcut::key(KeyCode::BackTab),
        KeyShortcut::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        KeyShortcut::new(KeyCode::Tab, KeyModifiers::SHIFT),
    ]
}

/// True when the event is Shift+Tab in any encoding from
/// [`shift_tab_keys`]. Release events never match.
pub fn is_shift_tab(key: &KeyEvent) -> bool {
    shift_tab_keys().iter().any(|k| k.matches(key))
}

pub fn is_text_input_key(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(_))
        && (key.modifiers.is_empty()
            || key.modifiers == KeyModifiers::SHIFT
            || is_altgr(key.modifiers))
}

impl From<KeyEvent> for KeyShortcut {
    fn from(key: KeyEvent) -> Self {
        Self::new(key.code, key.modifiers)
    }
}

impl fmt::Display for KeyShortcut {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let has_shift = self.modifiers.contains(KeyModifiers::SHIFT);
        // SUPER first, spelled per-platform like Opt/Alt (Cmd on macOS).
        if self.modifiers.contains(KeyModifiers::SUPER) {
            let sup = if cfg!(target_os = "macos") {
                "Cmd+"
            } else {
                "Super+"
            };
            write!(f, "{sup}")?;
        }
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            write!(f, "Ctrl+")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            let alt = if cfg!(target_os = "macos") {
                "Opt+"
            } else {
                "Alt+"
            };
            write!(f, "{alt}")?;
        }
        if has_shift {
            write!(f, "Shift+")?;
        }
        match self.code {
            KeyCode::Char(' ') => write!(f, "Space"),
            KeyCode::Char(c) => write!(f, "{}", c.to_ascii_lowercase()),
            KeyCode::Enter => write!(f, "Enter"),
            KeyCode::Esc => write!(f, "Esc"),
            KeyCode::Tab => write!(f, "Tab"),
            KeyCode::BackTab => write!(f, "Shift+Tab"),
            KeyCode::Backspace => write!(f, "Bsp"),
            KeyCode::Delete => write!(f, "Del"),
            KeyCode::Up => write!(f, "↑"),
            KeyCode::Down => write!(f, "↓"),
            KeyCode::Left => write!(f, "←"),
            KeyCode::Right => write!(f, "→"),
            KeyCode::Home => write!(f, "Home"),
            KeyCode::End => write!(f, "End"),
            KeyCode::PageUp => write!(f, "PgUp"),
            KeyCode::PageDown => write!(f, "PgDn"),
            KeyCode::F(n) => write!(f, "F{n}"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Ergonomic macro for constructing [`KeyShortcut`] values.
///
/// ```ignore
/// key!(Enter)              // KeyCode::Enter, no modifiers
/// key!('q')                // KeyCode::Char('q')
/// key!('c', CONTROL)       // Ctrl-C
/// key!('z', CONTROL | SHIFT) // Ctrl+⇧Z
/// key!(F(5))               // F5
/// ```
#[macro_export]
macro_rules! key {
    // Char literal: key!('c') or key!('c', CONTROL)
    ($char:literal $(, $($mod:ident)|+)? $(,)?) => {
        $crate::input::key::KeyShortcut::new(
            ::crossterm::event::KeyCode::Char($char),
            ::crossterm::event::KeyModifiers::NONE
                $($(| ::crossterm::event::KeyModifiers::$mod)+)?,
        )
    };
    // Named key: key!(Enter) or key!(Enter, SHIFT)
    ($code:ident $(, $($mod:ident)|+)? $(,)?) => {
        $crate::input::key::KeyShortcut::new(
            ::crossterm::event::KeyCode::$code,
            ::crossterm::event::KeyModifiers::NONE
                $($(| ::crossterm::event::KeyModifiers::$mod)+)?,
        )
    };
    // Function key: key!(F(5))
    ($code:ident ($($arg:tt)*) $(, $($mod:ident)|+)? $(,)?) => {
        $crate::input::key::KeyShortcut::new(
            ::crossterm::event::KeyCode::$code($($arg)*),
            ::crossterm::event::KeyModifiers::NONE
                $($(| ::crossterm::event::KeyModifiers::$mod)+)?,
        )
    };
}

pub use key;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_char() {
        let k = key!('q');
        assert_eq!(k.code, KeyCode::Char('q'));
        assert_eq!(k.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn ctrl_modifier() {
        let k = key!('c', CONTROL);
        assert_eq!(k.code, KeyCode::Char('c'));
        assert_eq!(k.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn ctrl_shift_combined() {
        let k = key!('z', CONTROL | SHIFT);
        assert!(k.modifiers.contains(KeyModifiers::CONTROL));
        assert!(k.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn shift_tab_all_encodings() {
        use crossterm::event::KeyEvent;
        // The three encodings terminals use for Shift+Tab.
        assert!(is_shift_tab(&KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::NONE
        )));
        assert!(is_shift_tab(&KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT
        )));
        assert!(is_shift_tab(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT
        )));
        // Plain Tab is NOT Shift+Tab.
        assert!(!is_shift_tab(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE
        )));
        // Release events never match.
        let mut release = KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(!is_shift_tab(&release));
    }

    #[test]
    fn special_keys() {
        assert_eq!(key!(Enter).code, KeyCode::Enter);
        assert_eq!(key!(Esc).code, KeyCode::Esc);
        assert_eq!(key!(Tab).code, KeyCode::Tab);
        assert_eq!(key!(Backspace).code, KeyCode::Backspace);
    }

    #[test]
    fn function_key() {
        let k = key!(F(5));
        assert_eq!(k.code, KeyCode::F(5));
    }

    #[test]
    fn matches_key_event() {
        let ctrl_c = key!('c', CONTROL);
        let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(ctrl_c.matches(&event));

        let wrong_mod = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!ctrl_c.matches(&wrong_mod));
    }

    #[test]
    fn to_key_event_roundtrip() {
        let k = key!('x', ALT);
        let event = k.to_key_event();
        assert!(k.matches(&event));
    }

    #[test]
    fn is_paste_key_ctrl_v() {
        let ev = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert!(is_paste_key(&ev));
    }

    #[test]
    fn is_paste_key_super_v() {
        let ev = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER);
        assert!(is_paste_key(&ev));
    }

    #[test]
    fn is_paste_key_plain_v_is_not_paste() {
        let ev = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE);
        assert!(!is_paste_key(&ev));
    }

    /// Alt+V is the Windows-only escape hatch for WT's Ctrl+V interceptor.
    /// Must NOT match elsewhere (collides with macOS `Opt+V` → `√`).
    /// Must NOT match AltGr+V on Windows (AltGr = `Ctrl+Alt`, text-input).
    #[test]
    fn is_paste_key_alt_v_windows_only() {
        let alt_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT);
        assert_eq!(is_paste_key(&alt_v), cfg!(target_os = "windows"));

        let altgr_v = KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(!is_paste_key(&altgr_v), "AltGr+V must remain text input");
    }

    #[test]
    fn is_inline_paste_key_ctrl_shift_v() {
        let ev = KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(is_inline_paste_key(&ev));
    }

    #[test]
    fn is_inline_paste_key_super_shift_v() {
        let ev = KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        );
        assert!(is_inline_paste_key(&ev));
    }

    #[test]
    fn display_formatting() {
        assert_eq!(key!('q').to_string(), "q");
        assert_eq!(key!('c', CONTROL).to_string(), "Ctrl+c");
        assert_eq!(key!(Enter).to_string(), "Enter");
        if cfg!(target_os = "macos") {
            assert_eq!(key!('x', ALT).to_string(), "Opt+x");
        } else {
            assert_eq!(key!('x', ALT).to_string(), "Alt+x");
        }
        assert_eq!(key!('n', CONTROL | SHIFT).to_string(), "Ctrl+Shift+n");
        assert_eq!(key!('h', CONTROL | SHIFT).to_string(), "Ctrl+Shift+h");
        assert_eq!(key!('g', SHIFT).to_string(), "Shift+g");

        // SUPER is spelled per-platform like Opt/Alt: Cmd on macOS, Super
        // elsewhere — in both the compact and pretty forms.
        if cfg!(target_os = "macos") {
            assert_eq!(key!(',', SUPER).to_string(), "Cmd+,");
            assert_eq!(key!(',', SUPER).display_pretty(), "Cmd+,");
        } else {
            assert_eq!(key!(',', SUPER).to_string(), "Super+,");
            assert_eq!(key!(',', SUPER).display_pretty(), "Super+,");
        }

        // Pretty (cheatsheet) also spells "Shift" + lowercase letter.
        assert_eq!(key!('n', CONTROL | SHIFT).display_pretty(), "Ctrl+Shift+n");
        assert_eq!(key!('h', CONTROL | SHIFT).display_pretty(), "Ctrl+Shift+h");
    }

    #[test]
    fn is_undo_key_matches_ctrl_and_cmd_z() {
        assert!(is_undo_key(&KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL
        )));
        assert!(is_undo_key(&KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::SUPER
        )));
        // Redo (uppercase Z) is never undo.
        assert!(!is_undo_key(&KeyEvent::new(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn is_altgr_rejects_single_modifiers() {
        assert!(!is_altgr(KeyModifiers::NONE));
        assert!(!is_altgr(KeyModifiers::SHIFT));
        assert!(!is_altgr(KeyModifiers::CONTROL));
        assert!(!is_altgr(KeyModifiers::ALT));
    }

    #[test]
    fn is_altgr_ctrl_alt_platform_dependent() {
        let mods = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(is_altgr(mods), cfg!(target_os = "windows"));
    }

    #[test]
    fn text_input_accepts_plain_and_shifted() {
        assert!(is_text_input_key(&KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
        assert!(is_text_input_key(&KeyEvent::new(
            KeyCode::Char('A'),
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn text_input_rejects_shortcut_modifiers() {
        assert!(!is_text_input_key(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_text_input_key(&KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::ALT
        )));
        assert!(!is_text_input_key(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::SUPER
        )));
        assert!(!is_text_input_key(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn text_input_altgr_platform_dependent() {
        let at = KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(is_text_input_key(&at), cfg!(target_os = "windows"));
    }
}
