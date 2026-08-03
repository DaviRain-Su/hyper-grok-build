//! Display-width-aware wrapping and the sanitizer every string passes through
//! before it reaches a cell.
//!
//! Sanitizing is not cosmetic. Agent transcripts carry whatever a tool printed,
//! including raw ANSI escapes and control bytes. ratatui writes a cell's symbol
//! through verbatim, so an un-stripped `ESC` would be emitted mid-frame and
//! desynchronize the terminal's parser — corrupting everything drawn after it.
//! Stripping happens once, at wrap time, and the result is cached; the render
//! loop never re-scans a string it has already laid out.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Tabs become this many spaces. Cells have no tab stops, so a literal `\t`
/// would render as a single blank and misalign every diff and code block.
const TAB_WIDTH: usize = 4;

/// Replacement for a character we refuse to emit.
const REPLACEMENT: char = '\u{fffd}';

/// Strip control characters, expand tabs, and drop zero-width joiners that
/// would let one grapheme claim more columns than [`UnicodeWidthStr`] predicts.
///
/// Newlines are *not* handled here — callers split on them first, because a
/// wrapped line can never contain one.
pub fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\t' => {
                for _ in 0..TAB_WIDTH {
                    out.push(' ');
                }
            }
            // Carriage returns show up in PTY-captured tool output; a bare CR
            // would move the cursor if passed through, and inside a single
            // wrapped line it carries no meaning.
            '\r' | '\n' => out.push(' '),
            // C0/C1 controls (including ESC, which is the dangerous one) and
            // the Unicode line/paragraph separators.
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => out.push(REPLACEMENT),
            c => out.push(c),
        }
    }
    out
}

/// Columns `text` occupies once sanitized.
pub fn width_of(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Wrap sanitized single-line `text` to `width` columns, breaking at word
/// boundaries and hard-splitting words longer than the line.
///
/// `subsequent_indent` is prepended to every line after the first — the hanging
/// indent that keeps bullets and numbered lists readable. It counts against
/// `width`.
///
/// Returns at least one (possibly empty) line, so a blank paragraph still
/// occupies a row and message spacing survives wrapping.
pub fn wrap(text: &str, width: usize, subsequent_indent: &str) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let indent_width = width_of(subsequent_indent);
    // A pathological indent (narrower terminal than the indent itself) would
    // leave zero usable columns and loop forever; fall back to no indent.
    let (subsequent_indent, indent_width) = if indent_width >= width {
        ("", 0)
    } else {
        (subsequent_indent, indent_width)
    };

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    // Start a new line carrying the hanging indent.
    macro_rules! newline {
        () => {{
            lines.push(std::mem::take(&mut current));
            current.push_str(subsequent_indent);
            current_width = indent_width;
        }};
    }
    // True when `current` holds real content, not just the hanging indent —
    // i.e. when breaking would produce a line worth emitting. Without this, a
    // word wider than the pane would emit an endless run of indent-only lines.
    macro_rules! has_content {
        () => {
            current_width > indent_width || (lines.is_empty() && current_width > 0)
        };
    }

    for word in split_keeping_spaces(text) {
        let word_width = width_of(&word);
        let is_space = word.starts_with(' ');

        if current_width + word_width <= width {
            current.push_str(&word);
            current_width += word_width;
            continue;
        }

        // A run of spaces at a wrap point is dropped rather than carried onto
        // the next line, where it would read as accidental indentation.
        if is_space {
            newline!();
            continue;
        }

        if has_content!() {
            newline!();
            if current_width + word_width <= width {
                current.push_str(&word);
                current_width += word_width;
                continue;
            }
        }

        // Wider than a whole line: split grapheme by grapheme so we never cut
        // inside one, or between the two cells of a wide character.
        for grapheme in word.graphemes(true) {
            let g_width = width_of(grapheme).max(1);
            if current_width + g_width > width && has_content!() {
                newline!();
            }
            current.push_str(grapheme);
            current_width += g_width;
        }
    }

    lines.push(current);
    // Strip the trailing spaces a wrap point can leave behind: they cost cells
    // to paint and can push a line one column past the pane.
    for line in &mut lines {
        while line.ends_with(' ') {
            line.pop();
        }
    }
    lines
}

/// Split into alternating runs of non-space and space, keeping the spaces so
/// intentional indentation inside a line survives wrapping.
fn split_keeping_spaces(text: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_space: Option<bool> = None;
    for grapheme in text.graphemes(true) {
        let is_space = grapheme == " ";
        if in_space != Some(is_space) && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        in_space = Some(is_space);
        current.push_str(grapheme);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Truncate to `width` columns, appending `…` when anything was cut. Used for
/// single-line rows (tool calls, sidebar labels) where wrapping would be noise.
pub fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if width_of(text) <= width {
        return text.to_string();
    }
    // Reserve one column for the ellipsis.
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let g_width = width_of(grapheme).max(1);
        if used + g_width > budget {
            break;
        }
        out.push_str(grapheme);
        used += g_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_controls_never_reach_a_cell() {
        // The important case: a raw ANSI sequence from tool output. The ESC is
        // replaced, so the bytes we hand the terminal cannot start a CSI.
        let sanitized = sanitize("\x1b[31mred\x1b[0m");
        assert!(!sanitized.contains('\x1b'));
        assert_eq!(sanitized, "\u{fffd}[31mred\u{fffd}[0m");
        // Tabs become real columns; CR/LF collapse to a space.
        assert_eq!(sanitize("a\tb"), "a    b");
        assert_eq!(sanitize("a\r\nb"), "a  b");
        // NUL and other C0 bytes are replaced, not dropped silently.
        assert_eq!(sanitize("a\0b"), "a\u{fffd}b");
        // Ordinary text (including wide glyphs) is untouched.
        assert_eq!(sanitize("héllo 日本"), "héllo 日本");
    }

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(
            wrap("the quick brown fox", 9, ""),
            ["the quick", "brown fox"]
        );
        // Exact fit doesn't spill.
        assert_eq!(wrap("abcd efgh", 9, ""), ["abcd efgh"]);
        // Empty input still occupies one row.
        assert_eq!(wrap("", 10, ""), [""]);
        // Every produced line respects the budget.
        let long = "alpha beta gamma delta epsilon zeta eta theta";
        for line in wrap(long, 12, "") {
            assert!(width_of(&line) <= 12, "{line:?} exceeds 12 columns");
        }
    }

    #[test]
    fn hard_splits_words_longer_than_the_line() {
        // A path or hash with no spaces must still be readable, not clipped.
        let lines = wrap("aaaaaaaaaaaaaaa", 5, "");
        assert_eq!(lines, ["aaaaa", "aaaaa", "aaaaa"]);
        // Reassembly is lossless.
        assert_eq!(lines.concat(), "aaaaaaaaaaaaaaa");
    }

    #[test]
    fn wide_graphemes_are_measured_in_columns_not_chars() {
        // Each CJK glyph is two columns: four of them fill an 8-column line.
        let lines = wrap("日本語話", 8, "");
        assert_eq!(lines, ["日本語話"]);
        let lines = wrap("日本語話", 6, "");
        assert_eq!(lines, ["日本語", "話"]);
        for line in wrap("日本語話 テスト", 5, "") {
            assert!(width_of(&line) <= 5, "{line:?} exceeds 5 columns");
        }
    }

    #[test]
    fn hanging_indent_applies_from_the_second_line() {
        let lines = wrap("one two three four", 8, "  ");
        assert_eq!(lines[0], "one two");
        for line in &lines[1..] {
            assert!(line.starts_with("  "), "{line:?} missing hanging indent");
            assert!(width_of(line) <= 8);
        }
        // An indent at least as wide as the pane is ignored rather than looping.
        let degenerate = wrap("one two three", 2, "    ");
        assert!(degenerate.iter().all(|l| width_of(l) <= 2));
    }

    #[test]
    fn truncate_marks_what_it_cut() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("", 4), "");
        assert_eq!(truncate("abc", 0), "");
        // Never splits a wide glyph across the ellipsis boundary.
        let cut = truncate("日本語話テスト", 7);
        assert!(width_of(&cut) <= 7, "{cut:?}");
    }

    #[test]
    fn zero_width_pane_does_not_panic() {
        assert_eq!(wrap("anything at all", 0, ""), [""]);
    }
}
