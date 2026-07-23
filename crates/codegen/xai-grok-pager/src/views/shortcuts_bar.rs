//! Shortcuts bar — renders keyboard hints.
//!
//! Accepts `&[HintItem]` from any source — action registry, prompt widget,
//! scrollback state, etc. Each view builds its own hints dynamically.
//!
//! When a `PendingAction` is active (double-press confirmation),
//! the bar replaces all hints with "press again to {label}".

use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::input::key::KeyShortcut;
use crate::theme::Theme;

/// A single hint for the shortcuts bar.
///
/// Carries semantic key data — the bar handles rendering.
/// Views build these dynamically from the registry, widget keymaps, or local state.
#[derive(Debug, Clone)]
pub struct HintItem {
    /// Keys to display. Multiple keys are shown joined with "/" (e.g., j/k).
    pub keys: Vec<KeyShortcut>,
    /// Short label for the bottom bar (e.g., "send", "nav", "cancel").
    pub label: Cow<'static, str>,
    /// Optional custom display string for keys (overrides keys.display()).
    pub custom_display: Option<&'static str>,
    /// Longer description for the all-shortcuts cheatsheet (e.g.,
    /// "Send prompt to agent"). When `None`, falls back to `label`.
    pub description: Option<Cow<'static, str>>,
    /// When true, the hint survives compact-mode truncation — it is always
    /// rendered regardless of `max_visible`. Use for hints that should be
    /// discoverable in every scrollback context (e.g. nav, turn, mode).
    pub pinned: bool,
}

impl HintItem {
    /// Single-key hint.
    pub fn new(key: KeyShortcut, label: impl Into<Cow<'static, str>>) -> Self {
        Self {
            keys: vec![key],
            label: label.into(),
            custom_display: None,
            description: None,
            pinned: false,
        }
    }

    /// Paired-key hint (e.g., j/k for nav, h/l for turn).
    pub fn paired(a: KeyShortcut, b: KeyShortcut, label: impl Into<Cow<'static, str>>) -> Self {
        Self {
            keys: vec![a, b],
            label: label.into(),
            custom_display: None,
            description: None,
            pinned: false,
        }
    }

    /// Mark this hint as pinned — it will always be shown in the compact
    /// shortcuts bar, even when the hint list exceeds `max_visible`.
    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }

    /// Render the keys portion as a display string (e.g., "j/k", "Enter", "Ctrl+c").
    fn key_display(&self) -> String {
        if let Some(display) = self.custom_display {
            display.to_string()
        } else {
            self.keys
                .iter()
                .map(|k| k.display())
                .collect::<Vec<_>>()
                .join("/")
        }
    }
}

/// Shortcuts bar widget. Renders a list of `HintItem`s.
pub struct ShortcutsBar<'a> {
    hints: &'a [HintItem],
    /// If set, replaces all hints with "press again to {label}".
    pending_confirmation: Option<PendingHint>,
    /// Right-aligned text (e.g. team name).
    right_text: Option<&'a str>,
    /// Compact mode config: render only the first `max_visible` hints from
    /// `hints`, then always append `help_hint` (e.g. the "all shortcuts"
    /// modal trigger). When None, all hints are rendered.
    compact: Option<CompactConfig>,
}

/// Compact-mode configuration for the shortcuts bar.
pub struct CompactConfig {
    /// Maximum number of items to render from the hint list before the
    /// trailing help hint.
    pub max_visible: usize,
    /// The trailing help hint (typically the binding for the all-shortcuts
    /// modal). Always rendered when set, even if the hint list is empty.
    pub help_hint: Option<HintItem>,
}

/// Info needed to render the "press again" hint.
#[derive(Clone)]
pub struct PendingHint {
    pub shortcut: KeyShortcut,
    pub label: Cow<'static, str>,
}

impl<'a> ShortcutsBar<'a> {
    /// Create from a pre-built list of hints.
    pub fn new(hints: &'a [HintItem]) -> Self {
        Self {
            hints,
            pending_confirmation: None,
            right_text: None,
            compact: None,
        }
    }

    /// Render only the first `max_visible` hints, then append `help_hint`
    /// (typically the binding that opens the all-shortcuts modal).
    pub fn compact(mut self, max_visible: usize, help_hint: Option<HintItem>) -> Self {
        self.compact = Some(CompactConfig {
            max_visible,
            help_hint,
        });
        self
    }

    /// Set the pending confirmation hint (replaces all normal hints).
    pub fn with_pending(mut self, pending: Option<PendingHint>) -> Self {
        self.pending_confirmation = pending;
        self
    }

    /// Set right-aligned text (e.g. team name).
    pub fn with_right_text(mut self, text: Option<&'a str>) -> Self {
        self.right_text = text;
        self
    }
}

impl Widget for ShortcutsBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        let theme = Theme::current();

        let bg_style = Style::default()
            .bg(theme.bg_base)
            .fg(theme.gray)
            .remove_modifier(Modifier::all());
        // Clear area content and style — set_style only patches style, leaving
        // old text from previous renders. Fill with spaces to clear.
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.reset();
                cell.set_style(bg_style);
            }
        }

        let key_style = Style::default()
            .fg(theme.text_secondary)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD);

        let action_style = Style::default()
            .fg(theme.gray)
            .bg(theme.bg_base)
            .remove_modifier(Modifier::BOLD | Modifier::DIM);

        // If pending confirmation, show only "press again to {label}"
        if let Some(pending) = &self.pending_confirmation {
            let key_text = pending.shortcut.display();
            let label = rust_i18n::t!("shortcuts.press_again", label = &pending.label);

            let mut x = area.x;

            let key_span = Span::styled(&key_text, key_style);
            let key_width = key_text.width() as u16;
            buf.set_span(x, area.y, &key_span, key_width);
            x += key_width;

            let colon = Span::styled(":", action_style);
            buf.set_span(x, area.y, &colon, 1);
            x += 1;

            let action_span = Span::styled(label.as_ref(), action_style);
            let action_width = label.width() as u16;
            buf.set_span(x, area.y, &action_span, action_width);
            let _ = x + action_width; // suppress unused
            return;
        }

        let sep_style = Style::default()
            .fg(theme.gray)
            .bg(theme.bg_base)
            .add_modifier(Modifier::DIM)
            .remove_modifier(Modifier::BOLD);

        let mut x = area.x;

        // Build the effective hint list (compact-aware).
        let effective = compute_effective_hints(self.hints, self.compact.as_ref());

        for (i, hint) in effective.iter().enumerate() {
            if i > 0 {
                let sep = Span::styled("  │  ", sep_style);
                let sep_width = 5u16;
                if x + sep_width > area.x + area.width {
                    break;
                }
                buf.set_span(x, area.y, &sep, sep_width);
                x += sep_width;
            }

            let key_text = hint.key_display();
            let key_span = Span::styled(&key_text, key_style);
            let key_width = key_text.width() as u16;
            if x + key_width > area.x + area.width {
                break;
            }
            buf.set_span(x, area.y, &key_span, key_width);
            x += key_width;

            let colon = Span::styled(":", action_style);
            if x + 1 > area.x + area.width {
                break;
            }
            buf.set_span(x, area.y, &colon, 1);
            x += 1;

            let action_span = Span::styled(hint.label.as_ref(), action_style);
            let action_width = hint.label.width() as u16;
            if x + action_width > area.x + area.width {
                break;
            }
            buf.set_span(x, area.y, &action_span, action_width);
            x += action_width;
        }

        // Right-aligned text (team name etc.)
        if let Some(text) = self.right_text {
            let right_style = Style::default().fg(theme.gray).bg(theme.bg_base);
            let display = format!("{text} ");
            let rw = display.width() as u16;
            if rw > 0 && rw < area.width {
                let rx = area.x + area.width.saturating_sub(rw);
                if rx > x + 1 {
                    let right_span = Span::styled(display, right_style);
                    buf.set_span(rx, area.y, &right_span, rw);
                }
            }
        }
    }
}

/// Compute the hint list the bar will actually render.
///
/// Without `compact`: returns every hint from the input slice.
/// With `compact`: pinned hints are always included; the remaining
/// `max_visible − pinned_count` slots are filled with unpinned hints in
/// their original order. The trailing `help_hint` is unconditionally
/// appended so users always see how to discover the rest.
pub fn compute_effective_hints<'a>(
    hints: &'a [HintItem],
    compact: Option<&'a CompactConfig>,
) -> Vec<&'a HintItem> {
    if let Some(cfg) = compact {
        let pinned_count = hints.iter().filter(|h| h.pinned).count();
        let unpinned_budget = cfg.max_visible.saturating_sub(pinned_count);
        let mut unpinned_used = 0;
        let mut v: Vec<&HintItem> = hints
            .iter()
            .filter(|h| {
                if h.pinned {
                    true
                } else if unpinned_used < unpinned_budget {
                    unpinned_used += 1;
                    true
                } else {
                    false
                }
            })
            .collect();
        if let Some(ref h) = cfg.help_hint {
            v.push(h);
        }
        v
    } else {
        hints.iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key;

    fn h(label: &'static str, k: crate::input::key::KeyShortcut) -> HintItem {
        HintItem::new(k, label)
    }

    #[test]
    fn full_mode_returns_all_hints() {
        let hints = vec![h("a", key!('a')), h("b", key!('b')), h("c", key!('c'))];
        let out = compute_effective_hints(&hints, None);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn compact_takes_first_n_then_appends_help() {
        let hints = vec![h("a", key!('a')), h("b", key!('b')), h("c", key!('c'))];
        let help = h("shortcuts", key!('/', CONTROL));
        let cfg = CompactConfig {
            max_visible: 2,
            help_hint: Some(help),
        };
        let out = compute_effective_hints(&hints, Some(&cfg));
        assert_eq!(out.len(), 3); // 2 + help
        assert_eq!(out[0].label, "a");
        assert_eq!(out[1].label, "b");
        assert_eq!(out[2].label, "shortcuts");
    }

    #[test]
    fn compact_help_hint_renders_even_with_empty_hint_list() {
        let hints: Vec<HintItem> = vec![];
        let help = h("shortcuts", key!('/', CONTROL));
        let cfg = CompactConfig {
            max_visible: 2,
            help_hint: Some(help),
        };
        let out = compute_effective_hints(&hints, Some(&cfg));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "shortcuts");
    }

    #[test]
    fn compact_without_help_just_truncates() {
        let hints = vec![h("a", key!('a')), h("b", key!('b')), h("c", key!('c'))];
        let cfg = CompactConfig {
            max_visible: 2,
            help_hint: None,
        };
        let out = compute_effective_hints(&hints, Some(&cfg));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn compact_max_visible_larger_than_input_is_safe() {
        let hints = vec![h("a", key!('a'))];
        let cfg = CompactConfig {
            max_visible: 10,
            help_hint: None,
        };
        let out = compute_effective_hints(&hints, Some(&cfg));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn compact_pinned_hints_always_included() {
        // 5 hints: a, b, c are unpinned; d, e are pinned.
        // max_visible=3 → budget for unpinned = 3-2 = 1.
        // Result: a (unpinned slot 1), d (pinned), e (pinned) = 3 items.
        let hints = vec![
            h("a", key!('a')),
            h("b", key!('b')),
            h("c", key!('c')),
            h("d", key!('d')).pinned(),
            h("e", key!('e')).pinned(),
        ];
        let cfg = CompactConfig {
            max_visible: 3,
            help_hint: None,
        };
        let out = compute_effective_hints(&hints, Some(&cfg));
        let labels: Vec<&str> = out.iter().map(|h| h.label.as_ref()).collect();
        assert_eq!(labels, vec!["a", "d", "e"]);
    }

    #[test]
    fn compact_pinned_preserves_original_order() {
        // Pinned hint appears between unpinned ones — order is preserved.
        let hints = vec![
            h("a", key!('a')),
            h("nav", key!('j')).pinned(),
            h("b", key!('b')),
            h("c", key!('c')),
        ];
        let cfg = CompactConfig {
            max_visible: 3,
            help_hint: None,
        };
        let out = compute_effective_hints(&hints, Some(&cfg));
        let labels: Vec<&str> = out.iter().map(|h| h.label.as_ref()).collect();
        // 1 pinned + budget 2 unpinned: a, nav, b
        assert_eq!(labels, vec!["a", "nav", "b"]);
    }

    #[test]
    fn compact_all_pinned_exceeding_max_visible() {
        // More pinned hints than max_visible — all pinned still shown.
        let hints = vec![
            h("a", key!('a')).pinned(),
            h("b", key!('b')).pinned(),
            h("c", key!('c')).pinned(),
            h("d", key!('d')),
        ];
        let cfg = CompactConfig {
            max_visible: 2,
            help_hint: None,
        };
        let out = compute_effective_hints(&hints, Some(&cfg));
        let labels: Vec<&str> = out.iter().map(|h| h.label.as_ref()).collect();
        // All 3 pinned, 0 budget for unpinned.
        assert_eq!(labels, vec!["a", "b", "c"]);
    }
}
