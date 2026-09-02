//! Legacy-console fallbacks for chrome glyphs that don't ship in the legacy Windows ConHost default font (Consolas / Lucida Console).
//!
//! Fallbacks are ASCII where possible (`x`, `o`, `c`, `*`), or a CP437 glyph when one reads better and still renders on the raster font.
//!
//! ConHost does no font fallback, so missing glyphs render as tofu.
//! Windows Terminal, VS Code, and modern emulators bundle fonts (or fall back to one) that cover the glyphs we use as chrome.
//! The substitution therefore only fires for legacy `cmd.exe` / `powershell.exe`.

use std::borrow::Cow;
use std::sync::OnceLock;

use crate::host::HostOs;
use crate::terminal::{TerminalName, terminal_context};

/// `"❯ "` normally, `"> "` on legacy ConHost. Always 2 columns wide.
pub fn prompt_arrow() -> &'static str {
    if is_legacy_windows_console() {
        "> "
    } else {
        "\u{276F} "
    }
}

/// Display width of [`prompt_arrow`] in columns.
pub const PROMPT_ARROW_WIDTH: u16 = 2;

/// Record indicator glyph shown above the prompt while voice capture is active: a dot inside a ring.
/// The bright half of the pulse shows the filled FISHEYE (`◉`), the dim half the open BULLSEYE (`◎`).
/// Together with a smooth color fade this reads like a studio recording light.
/// ASCII fallback (`*`/`o`) on legacy ConHost. Always 1 column wide.
pub fn record_dot(filled: bool) -> &'static str {
    if is_legacy_windows_console() {
        if filled { "*" } else { "o" }
    } else if filled {
        "\u{25C9}"
    } else {
        "\u{25CE}"
    }
}

/// `"❙"` normally, `"|"` on legacy ConHost. Always 1 column wide.
pub fn collapsed_accent() -> &'static str {
    if is_legacy_windows_console() {
        "|"
    } else {
        "\u{2759}"
    }
}

/// `"✗"` (U+2717 BALLOT X) normally, `"x"` on legacy ConHost. Always 1 column wide.
///
/// Used for close / cancel / kill buttons and failure status markers.
pub fn ballot_x() -> &'static str {
    if is_legacy_windows_console() {
        "x"
    } else {
        "\u{2717}"
    }
}

/// `"✓"` (U+2713 CHECK MARK) normally, `"√"` (U+221A SQUARE ROOT) on legacy ConHost. Always 1 column wide.
///
/// The success / done sibling of [`ballot_x`].
/// The fallback `√` is a CP437 glyph (code 0xFB), so it renders even on the stripped-down raster font and still reads as a checkmark.
pub fn check_mark() -> &'static str {
    if is_legacy_windows_console() {
        "\u{221A}"
    } else {
        "\u{2713}"
    }
}

/// `"↗"` (U+2197 NORTH EAST ARROW) normally, `"o"` on legacy ConHost. Always 1 column wide.
///
/// The enlarge / view / fullscreen button glyph.
/// The earlier `⛶` (U+26F6) is missing from many modern monospace fonts and rendered as tofu even on common macOS/Linux terminals.
/// U+2197 sits in the core Arrows block, which fonts cover well.
pub fn enlarge() -> &'static str {
    if is_legacy_windows_console() {
        "o"
    } else {
        "\u{2197}"
    }
}

/// `"⧉"` (U+29C9 TWO JOINED SQUARES) normally, `"c"` on legacy ConHost. Always 1 column wide.
///
/// The copy button glyph on the scrollback selection box (pairs with [`enlarge`]).
pub fn copy_icon() -> &'static str {
    if is_legacy_windows_console() {
        "c"
    } else {
        "\u{29C9}"
    }
}

/// `"⇣"` (U+21E3 DOWNWARDS DASHED ARROW) normally, `"↓"` (U+2193) on legacy ConHost. Always 1 column wide.
///
/// Used for the context-token count in the turn-status line.
pub fn token_arrow() -> &'static str {
    if is_legacy_windows_console() {
        "\u{2193}"
    } else {
        "\u{21E3}"
    }
}

/// Pulsing monitor-indicator frames (`○ ◎ ◉ ◎`) normally; a 1-column dot pulse (`·`, `○`, `•`, `○`) on legacy ConHost.
///
/// Animates the "N monitors still running" cue in the turn-status line: a concentric circle that breathes open and shut.
/// Of the fancy frames only `○` is in CP437, so the fallback uses CP437 dots and pulses by fill (faint, ring, solid, ring) rather than by size.
/// Every frame in both sets is exactly 1 column so the trailing label never shifts as the icon animates.
pub fn monitor_icon_frames() -> &'static [&'static str] {
    const FANCY: &[&str] = &["\u{25CB}", "\u{25CE}", "\u{25C9}", "\u{25CE}"];
    const FALLBACK: &[&str] = &["\u{00B7}", "\u{25CB}", "\u{2022}", "\u{25CB}"];
    if is_legacy_windows_console() {
        FALLBACK
    } else {
        FANCY
    }
}

/// `"◆"` (U+25C6 BLACK DIAMOND) normally, `"♦"` (U+2666 BLACK DIAMOND SUIT) on legacy ConHost. Always 1 column wide.
///
/// The filled diamond used across scrollback bullets, status cues, the `/context` bar, picker fold indicators, and dashboard row markers.
/// The fallback `♦` is a CP437 glyph (code `0x04`), so it renders and still reads as a filled diamond.
pub fn diamond_filled() -> &'static str {
    if is_legacy_windows_console() {
        "\u{2666}"
    } else {
        "\u{25C6}"
    }
}

/// `"◇"` (U+25C7 WHITE DIAMOND) normally, `"○"` (U+25CB WHITE CIRCLE) on legacy ConHost. Always 1 column wide.
///
/// The hollow sibling of [`diamond_filled`]: the `/context` bar's free (unused) cells and the dashboard's idle row marker.
/// The fallback `○` is a CP437 glyph (code `0x09`) and still reads as an empty marker.
pub fn diamond_hollow() -> &'static str {
    if is_legacy_windows_console() {
        "\u{25CB}"
    } else {
        "\u{25C7}"
    }
}

/// `"◈"` (U+25C8 WHITE DIAMOND CONTAINING BLACK SMALL DIAMOND) normally, `"♦"` (U+2666) on legacy ConHost. Always 1 column wide.
///
/// Used for the `/context` bar's tool-definitions category and the collapsed-group scrollback header.
/// Falls back to the same `♦` as [`diamond_filled`]; both call sites distinguish the category by color, so the collision is harmless.
pub fn diamond_dotted() -> &'static str {
    if is_legacy_windows_console() {
        "\u{2666}"
    } else {
        "\u{25C8}"
    }
}

/// Filled-diamond glyph as a [`char`] (see [`diamond_filled`]), for the tool-usage sequence bar which builds its row from single `char`s.
pub fn diamond_filled_char() -> char {
    diamond_filled().chars().next().unwrap_or('\u{25C6}')
}

/// Hollow-diamond glyph as a [`char`] (see [`diamond_hollow`]).
pub fn diamond_hollow_char() -> char {
    diamond_hollow().chars().next().unwrap_or('\u{25C7}')
}

/// Rotating braille progress-spinner frames (`⠋⠙⠹⠸⠼⠴⠦⠧`) normally; a 1-column ASCII spinner (`|`, `/`, `-`, `\`) on legacy ConHost.
///
/// The U+2800 Braille Patterns block is not in CP437, so every caller falls back to the classic ASCII spinner there.
/// Every frame in both sets is exactly 1 column so the surrounding layout never shifts.
pub fn braille_spinner_frames() -> &'static [&'static str] {
    const FANCY: &[&str] = &[
        "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
        "\u{2827}",
    ];
    const FALLBACK: &[&str] = &["|", "/", "-", "\\"];
    if is_legacy_windows_console() {
        FALLBACK
    } else {
        FANCY
    }
}

/// Pulsing dot progress-spinner frames (`⋅ : ⸬ ⁙`) normally; a quiet 1-column dot cycle (`.`, `:`, `·`) on legacy ConHost.
///
/// U+22C5 / U+2E2C / U+2059 are absent from CP437, so every caller falls back to a dot cycle the raster font does render.
/// Every frame in both sets is exactly 1 column.
pub fn dot_spinner_frames() -> &'static [&'static str] {
    const FANCY: &[&str] = &[
        "\u{22c5}", ":", "\u{2e2c}", "\u{2059}", "\u{22c5}", ":", "\u{2e2c}", "\u{2059}",
    ];
    const FALLBACK: &[&str] = &[".", ":", "\u{00b7}"];
    if is_legacy_windows_console() {
        FALLBACK
    } else {
        FANCY
    }
}

/// `"┃"` (U+2503 HEAVY VERTICAL) normally, `"│"` (U+2502 LIGHT VERTICAL, CP437 `0xB3`) on legacy ConHost. Always 1 column wide.
///
/// The left accent rail painted beside scrollback blocks and modal panels.
/// CP437 ships only the light `│` and double `║` verticals, so the heavy stroke falls back to the light one.
pub fn accent_bar() -> &'static str {
    if is_legacy_windows_console() {
        "\u{2502}"
    } else {
        "\u{2503}"
    }
}

/// `"▴"` (U+25B4 SMALL UP-POINTING TRIANGLE) normally, `"▲"` (U+25B2, CP437 `0x1E`) on legacy ConHost. Always 1 column wide.
///
/// The timeline sidebar's previous-turn chevron.
/// The small triangles are absent from CP437; the full-size ones are control-picture glyphs the raster font renders.
pub fn timeline_chevron_up() -> &'static str {
    if is_legacy_windows_console() {
        "\u{25B2}"
    } else {
        "\u{25B4}"
    }
}

/// `"▾"` (U+25BE SMALL DOWN-POINTING TRIANGLE) normally, `"▼"` (U+25BC, CP437 `0x1F`) on legacy ConHost. Always 1 column wide.
///
/// The timeline sidebar's next-turn chevron; see [`timeline_chevron_up`].
pub fn timeline_chevron_down() -> &'static str {
    if is_legacy_windows_console() {
        "\u{25BC}"
    } else {
        "\u{25BE}"
    }
}

/// `"━"` (U+2501 HEAVY HORIZONTAL) normally, `"─"` (U+2500 LIGHT HORIZONTAL, CP437 `0xC4`) on legacy ConHost. Always 1 column wide.
///
/// Prefer [`timeline_tick_active`] for the sidebar rail: on legacy ConHost this falls back to the same light stroke used for hover.
pub fn heavy_horizontal() -> &'static str {
    if is_legacy_windows_console() {
        "\u{2500}"
    } else {
        "\u{2501}"
    }
}

/// `"─"` (U+2500 LIGHT HORIZONTAL, CP437 `0xC4`). Always 1 column wide and present on every target.
/// Exposed so the timeline sidebar's inactive ticks share one glyph source with [`heavy_horizontal`] instead of hardcoding the codepoint.
pub fn light_horizontal() -> &'static str {
    "\u{2500}"
}

/// Precomposed 2-col active tick for the timeline rail: `"━━"` normally, `"══"` (U+2550, CP437 `0xCD`) on legacy ConHost.
/// The double stroke keeps the active tick distinct from the light hover/idle stroke there.
pub fn timeline_tick_active() -> &'static str {
    if is_legacy_windows_console() {
        "\u{2550}\u{2550}"
    } else {
        "\u{2501}\u{2501}"
    }
}

/// Precomposed 2-col hover tick for the timeline rail: `"──"` (light horizontal).
/// Idle ticks reuse a single light cell; this is the wide bright hover form.
pub fn timeline_tick_hover() -> &'static str {
    "\u{2500}\u{2500}"
}

/// `"●"` (U+25CF BLACK CIRCLE) normally, `"•"` (U+2022 BULLET, CP437 `0x07`) on legacy ConHost. Always 1 column wide.
///
/// The filled status / selection dot used in pickers, the settings and permission modals, the session list, and the file-search view.
/// Its hollow partner `○` (U+25CB) is already a CP437 glyph (`0x09`) and renders unchanged, so only the filled variant needs a stand-in.
pub fn filled_dot() -> &'static str {
    if is_legacy_windows_console() {
        "\u{2022}"
    } else {
        "\u{25CF}"
    }
}

/// `"▏"` (U+258F LEFT ONE EIGHTH BLOCK) normally, `"│"` (U+2502, CP437 `0xB3`) on legacy ConHost. Always 1 column wide.
///
/// The thin left bar marking the selected row in the dashboard and the settings panes.
pub fn selection_bar() -> &'static str {
    if is_legacy_windows_console() {
        "\u{2502}"
    } else {
        "\u{258F}"
    }
}

/// `"›"` (U+203A SINGLE RIGHT-POINTING ANGLE QUOTATION MARK) normally, `">"` (ASCII) on legacy ConHost. Always 1 column wide.
///
/// The chevron used for collapsed fold indicators, settings breadcrumbs, the integer-stepper increment affordance, and the dashboard "next" button.
pub fn chevron() -> &'static str {
    if is_legacy_windows_console() {
        ">"
    } else {
        "\u{203A}"
    }
}

/// `"‹"` (U+2039 SINGLE LEFT-POINTING ANGLE QUOTATION MARK) normally, `"<"` (ASCII) on legacy ConHost. Always 1 column wide.
///
/// The mirror of [`chevron`]: the integer-stepper decrement affordance and the dashboard "prev" button.
/// Kept in lockstep so a fixed `›`/`>` never sits next to a tofu `‹`.
pub fn chevron_left() -> &'static str {
    if is_legacy_windows_console() {
        "<"
    } else {
        "\u{2039}"
    }
}

/// `"⌄"` (U+2304 DOWN ARROWHEAD) normally, `"v"` (ASCII) on legacy ConHost. Always 1 column wide.
///
/// The downward member of the [`chevron`] family; matches `›`'s light visual weight (unlike the solid `▾` disclosure triangle).
/// Used by the scrollback expandable indicator when the selected row is an expanded verb-group header.
pub fn chevron_down() -> &'static str {
    if is_legacy_windows_console() {
        "v"
    } else {
        "\u{2304}"
    }
}

/// `"▾"` (U+25BE BLACK DOWN-POINTING SMALL TRIANGLE) normally, `"v"` (ASCII) on legacy ConHost. Always 1 column wide.
///
/// The "expanded" disclosure indicator for a collapsible dashboard section header (the section's rows are visible below it).
pub fn disclosure_open() -> &'static str {
    if is_legacy_windows_console() {
        "v"
    } else {
        "\u{25BE}"
    }
}

/// `"▸"` (U+25B8 BLACK RIGHT-POINTING SMALL TRIANGLE) normally, `">"` (ASCII) on legacy ConHost. Always 1 column wide.
///
/// The "collapsed" disclosure indicator for a collapsible dashboard section header (the section's rows are hidden).
/// Pairs with [`disclosure_open`].
pub fn disclosure_closed() -> &'static str {
    if is_legacy_windows_console() {
        ">"
    } else {
        "\u{25B8}"
    }
}

/// `"[✗]"` normally, `"[x]"` on legacy ConHost. Always 3 columns wide.
///
/// Pre-composed bracketed form of [`ballot_x`] so per-frame render paths reuse a `&'static str` instead of allocating a `format!` each frame.
pub fn ballot_x_button() -> &'static str {
    if is_legacy_windows_console() {
        "[x]"
    } else {
        "[\u{2717}]"
    }
}

/// `"[↗]"` normally, `"[o]"` on legacy ConHost. Always 3 columns wide.
///
/// Pre-composed bracketed sibling of [`ballot_x_button`] for the bg-task view / enlarge button.
pub fn enlarge_button() -> &'static str {
    if is_legacy_windows_console() {
        "[o]"
    } else {
        "[\u{2197}]"
    }
}

/// Substitute the chrome glyphs that legacy ConHost can't render (`✓` to `√`, `✗` to `x`, `⚠` to `!`) in flowing status text such as toasts.
///
/// Toasts are flowing text assembled in ~25 call sites, so one funnel where they enter view state beats threading a helper through every builder.
/// Returns a borrow unchanged on every non-legacy platform, so toast strings stay byte-identical there.
pub fn legacy_glyph_fallback(s: &str) -> Cow<'_, str> {
    if !is_legacy_windows_console() {
        return Cow::Borrowed(s);
    }
    if !s.contains(['\u{2713}', '\u{2717}', '\u{26A0}']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(to_legacy_glyphs(s))
}

/// Single-row toast sinks: glyph fallback, then map control chars to spaces.
/// Borrows when the input is already clean (common path).
pub fn sanitize_toast_message(msg: &str) -> Cow<'_, str> {
    let glyph = legacy_glyph_fallback(msg);
    if !glyph.chars().any(char::is_control) {
        return glyph;
    }
    Cow::Owned(
        glyph
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect(),
    )
}

/// Pure glyph-to-legacy mapping behind [`legacy_glyph_fallback`], split out so tests can exercise the substitution without faking the host probe.
/// `√` matches [`check_mark`]'s fallback; `x` matches [`ballot_x`]'s.
fn to_legacy_glyphs(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2713}' => '\u{221A}',
            '\u{2717}' => 'x',
            '\u{26A0}' => '!',
            other => other,
        })
        .collect()
}

/// True when running on native Windows in a console host whose default font is known not to ship the Dingbats glyphs we use as chrome.
/// Cached for process lifetime.
///
/// `GROK_FORCE_LEGACY_CONSOLE=1` (or `true`) forces this on regardless of host/terminal, and `=0` (or `false`) forces it off.
/// That override lets QA eyeball the ASCII fallbacks (or confirm the fancy glyphs) on any platform without a real ConHost.
pub fn is_legacy_windows_console() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        forced_legacy_console_override().unwrap_or_else(|| {
            // `env_brand`, not `brand`: bare ConHost detects as `Unknown`, but `brand` optimistically becomes `WindowsTerminal` on native Windows
            // Font capability needs the raw detection so legacy consoles still get the ASCII glyph fallback
            decide_legacy_windows_console(HostOs::current(), terminal_context().env_brand)
        })
    })
}

/// Read the `GROK_FORCE_LEGACY_CONSOLE` escape hatch from the environment.
fn forced_legacy_console_override() -> Option<bool> {
    parse_forced_legacy_console(std::env::var("GROK_FORCE_LEGACY_CONSOLE").ok().as_deref())
}

/// Pure parse of the override value so tests don't touch the environment.
/// `"1"` / `"true"` forces on, `"0"` / `"false"` forces off; anything else (including unset) is `None` so normal host/brand detection runs.
fn parse_forced_legacy_console(value: Option<&str>) -> Option<bool> {
    match value {
        Some("1" | "true") => Some(true),
        Some("0" | "false") => Some(false),
        _ => None,
    }
}

/// Pure decision function so tests can drive (host, brand) pairs without touching ambient state.
/// Default-deny on Windows: an unknown brand is treated as legacy.
/// Bare `cmd.exe` / `powershell.exe` in ConHost sets no terminal env vars, so the brand probe returns `Unknown` in exactly the case we need to catch.
fn decide_legacy_windows_console(host: HostOs, brand: TerminalName) -> bool {
    if host != HostOs::Windows {
        return false;
    }
    !matches!(
        brand,
        TerminalName::WindowsTerminal
            | TerminalName::VsCode
            | TerminalName::Cursor
            | TerminalName::Windsurf
            | TerminalName::Zed
            | TerminalName::WezTerm
            | TerminalName::Kitty
            | TerminalName::Alacritty
            | TerminalName::Ghostty
            | TerminalName::Rio
            | TerminalName::GrokDesktop
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    // Both variants must match `PROMPT_ARROW_WIDTH` so callers using the constant for layout math don't drift between platforms
    #[test]
    fn prompt_arrow_variants_are_two_columns() {
        assert_eq!("\u{276F} ".width(), PROMPT_ARROW_WIDTH as usize);
        assert_eq!("> ".width(), PROMPT_ARROW_WIDTH as usize);
    }

    // Both record-dot states must be exactly 1 column so the "Recording" label position is stable as the indicator pulses
    #[test]
    fn record_dot_states_are_one_column() {
        assert_eq!(record_dot(true).width(), 1);
        assert_eq!(record_dot(false).width(), 1);
        assert_eq!("\u{25C9}".width(), 1); // ◉ FISHEYE
        assert_eq!("\u{25CE}".width(), 1); // ◎ BULLSEYE
    }

    #[test]
    fn collapsed_accent_variants_are_one_column() {
        assert_eq!("\u{2759}".width(), 1);
        assert_eq!("|".width(), 1);
    }

    // Every icon and its fallback must be exactly one column so fixed-width button layouts don't shift between platforms
    #[test]
    fn icon_fallback_variants_are_one_column() {
        for (fancy, fallback) in [
            ("\u{2717}", "x"),        // ballot_x
            ("\u{2713}", "\u{221A}"), // check_mark
            ("\u{2197}", "o"),        // enlarge
            ("\u{29C9}", "c"),        // copy_icon
            ("\u{21E3}", "\u{2193}"), // token_arrow
        ] {
            assert_eq!(fancy.width(), 1, "icon {fancy:?} must be 1 column");
            assert_eq!(
                fallback.width(),
                1,
                "fallback {fallback:?} must be 1 column"
            );
        }
    }

    // Every diamond glyph and its legacy fallback must be exactly one column so the call sites keep their layout on every platform
    #[test]
    fn diamond_variants_are_one_column() {
        for (fancy, fallback) in [
            ("\u{25C6}", "\u{2666}"), // diamond_filled
            ("\u{25C7}", "\u{25CB}"), // diamond_hollow
            ("\u{25C8}", "\u{2666}"), // diamond_dotted
        ] {
            assert_eq!(fancy.width(), 1, "diamond {fancy:?} must be 1 column");
            assert_eq!(
                fallback.width(),
                1,
                "fallback {fallback:?} must be 1 column"
            );
        }
    }

    // Each chrome glyph and its legacy fallback must be exactly one column so the rails, dots, bars, and chevrons keep their layout everywhere
    #[test]
    fn chrome_glyph_variants_are_one_column() {
        for (fancy, fallback) in [
            ("\u{2503}", "\u{2502}"), // accent_bar
            ("\u{25CF}", "\u{2022}"), // filled_dot
            ("\u{258F}", "\u{2502}"), // selection_bar
            ("\u{203A}", ">"),        // chevron
            ("\u{2039}", "<"),        // chevron_left
            ("\u{2304}", "v"),        // chevron_down
        ] {
            assert_eq!(fancy.width(), 1, "glyph {fancy:?} must be 1 column");
            assert_eq!(
                fallback.width(),
                1,
                "fallback {fallback:?} must be 1 column"
            );
        }
    }

    // Every spinner and monitor-pulse frame, fancy or fallback, must be 1 column so animating them never shifts the label / timer that follows
    #[test]
    fn spinner_frames_are_one_column() {
        for frame in braille_spinner_frames()
            .iter()
            .chain(dot_spinner_frames().iter())
            .chain(monitor_icon_frames().iter())
            .chain(
                [
                    "|", "/", "-", "\\", ".", ":", "\u{00b7}", "\u{25cb}", "\u{2022}",
                ]
                .iter(),
            )
        {
            assert_eq!(frame.width(), 1, "spinner frame {frame:?} must be 1 column");
        }
    }

    // On the (non-Windows) test host the helpers must return the fancy glyphs, and the `char` helpers must agree with their `&str` siblings
    #[test]
    fn glyph_helpers_return_fancy_on_non_legacy() {
        assert!(!is_legacy_windows_console());
        assert_eq!(diamond_filled(), "\u{25C6}");
        assert_eq!(diamond_hollow(), "\u{25C7}");
        assert_eq!(diamond_dotted(), "\u{25C8}");
        assert_eq!(diamond_filled_char(), '\u{25C6}');
        assert_eq!(diamond_hollow_char(), '\u{25C7}');
        assert_eq!(braille_spinner_frames()[0], "\u{280b}");
        assert_eq!(dot_spinner_frames()[2], "\u{2e2c}");
        assert_eq!(
            monitor_icon_frames(),
            ["\u{25CB}", "\u{25CE}", "\u{25C9}", "\u{25CE}"]
        );
    }

    // Both variants of each pre-composed button must keep a fixed column width so the right-aligned chrome lands in the same cells everywhere
    #[test]
    fn button_variants_have_stable_width() {
        for (fancy, fallback, cols) in [
            ("[\u{2717}]", "[x]", 3), // ballot_x_button
            ("[\u{2197}]", "[o]", 3), // enlarge_button
        ] {
            assert_eq!(fancy.width(), cols, "button {fancy:?} must be {cols} cols");
            assert_eq!(
                fallback.width(),
                cols,
                "fallback {fallback:?} must be {cols} cols"
            );
        }
    }

    // The toast scrubber maps every chrome glyph that is tofu on legacy consoles to a 1-column stand-in and leaves all other text untouched
    #[test]
    fn to_legacy_glyphs_maps_known_glyphs() {
        assert_eq!(to_legacy_glyphs("\u{2713}\u{2717}\u{26A0}"), "\u{221A}x!");
        assert_eq!(
            to_legacy_glyphs("\u{2713} Saved: on"),
            "\u{221A} Saved: on",
            "only the glyph is replaced; surrounding text is preserved"
        );
        // Glyphs this module doesn't own (em dash, CJK) pass through verbatim.
        assert_eq!(
            to_legacy_glyphs("a \u{2014} \u{4e2d}"),
            "a \u{2014} \u{4e2d}"
        );
    }

    // On the (non-Windows) test host the funnel must be a zero-copy borrow so non-legacy toasts are byte-identical to the input
    #[test]
    fn legacy_glyph_fallback_is_borrow_on_non_legacy() {
        assert!(!is_legacy_windows_console());
        assert!(matches!(
            legacy_glyph_fallback("\u{2713} Saved"),
            Cow::Borrowed("\u{2713} Saved")
        ));
    }

    #[test]
    fn sanitize_toast_message_borrows_when_clean() {
        assert!(!is_legacy_windows_console());
        assert!(matches!(
            sanitize_toast_message("plain toast"),
            Cow::Borrowed("plain toast")
        ));
    }

    #[test]
    fn sanitize_toast_message_maps_controls_to_spaces() {
        let out = sanitize_toast_message("a\nb\tc");
        assert_eq!(out.as_ref(), "a b c");
        assert!(!out.chars().any(char::is_control));
    }

    #[test]
    fn forced_legacy_console_override_parses_known_values() {
        assert_eq!(parse_forced_legacy_console(Some("1")), Some(true));
        assert_eq!(parse_forced_legacy_console(Some("true")), Some(true));
        assert_eq!(parse_forced_legacy_console(Some("0")), Some(false));
        assert_eq!(parse_forced_legacy_console(Some("false")), Some(false));
        // Unset or unrecognized values defer to normal host/brand detection
        assert_eq!(parse_forced_legacy_console(None), None);
        assert_eq!(parse_forced_legacy_console(Some("")), None);
        assert_eq!(parse_forced_legacy_console(Some("yes")), None);
    }

    #[test]
    fn non_windows_is_never_legacy() {
        for brand in [
            TerminalName::Unknown,
            TerminalName::AppleTerminal,
            TerminalName::Vte,
            TerminalName::WindowsTerminal,
        ] {
            assert!(!decide_legacy_windows_console(HostOs::Macos, brand));
            assert!(!decide_legacy_windows_console(HostOs::Linux, brand));
            assert!(!decide_legacy_windows_console(HostOs::Other, brand));
        }
    }

    #[test]
    fn windows_unknown_is_legacy() {
        // Realistic ConHost case: no terminal env vars set.
        assert!(decide_legacy_windows_console(
            HostOs::Windows,
            TerminalName::Unknown
        ));
    }

    #[test]
    fn windows_terminal_is_not_legacy() {
        assert!(!decide_legacy_windows_console(
            HostOs::Windows,
            TerminalName::WindowsTerminal
        ));
    }

    #[test]
    fn vscode_family_on_windows_is_not_legacy() {
        for brand in [
            TerminalName::VsCode,
            TerminalName::Cursor,
            TerminalName::Windsurf,
            TerminalName::Zed,
        ] {
            assert!(!decide_legacy_windows_console(HostOs::Windows, brand));
        }
    }

    #[test]
    fn modern_emulators_on_windows_are_not_legacy() {
        for brand in [
            TerminalName::WezTerm,
            TerminalName::Kitty,
            TerminalName::Alacritty,
            TerminalName::Ghostty,
            TerminalName::Rio,
            TerminalName::GrokDesktop,
        ] {
            assert!(!decide_legacy_windows_console(HostOs::Windows, brand));
        }
    }

    // AppleTerminal/VTE can't actually be probed on Windows; the assertion is the default-deny safety net for unfamiliar brands
    #[test]
    fn unfamiliar_brands_on_windows_default_to_legacy() {
        for brand in [
            TerminalName::AppleTerminal,
            TerminalName::Vte,
            TerminalName::Iterm2,
            TerminalName::WarpTerminal,
        ] {
            assert!(decide_legacy_windows_console(HostOs::Windows, brand));
        }
    }
}
