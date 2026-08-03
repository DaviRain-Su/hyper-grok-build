//! The prompt editor: a multi-line buffer with grapheme-aware editing.
//!
//! Cursor position is a byte offset into `text`, but every movement steps by
//! *grapheme cluster*, so an emoji or a combining sequence is one press wide
//! rather than several — and the offset always lands on a char boundary, which
//! is what keeps slicing infallible.
//!
//! Wrapping is recomputed on demand rather than cached: a prompt is at most a
//! few hundred bytes, so laying it out costs less than tracking whether the
//! cache is stale. The transcript, which can be megabytes, is the one that
//! earns a cache.

use unicode_segmentation::UnicodeSegmentation;

use crate::wrap;

#[derive(Debug, Default, Clone)]
pub struct Composer {
    text: String,
    /// Byte offset of the caret. Invariant: always a grapheme boundary.
    cursor: usize,
}

/// One visual row of the composer plus where the caret falls on it.
pub struct Laid {
    pub rows: Vec<String>,
    /// Row index of the caret.
    pub cursor_row: usize,
    /// Column of the caret within that row.
    pub cursor_col: usize,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace the whole buffer, caret at the end. Used to hand a failed send's
    /// text back to the user.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }

    /// Take the buffer, leaving it empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Insert pasted text. Bracketed paste delivers a whole clipboard as one
    /// event, so this is one edit rather than N keystrokes — the difference
    /// between a paste being instant and being visibly slow.
    pub fn insert_str(&mut self, text: &str) {
        // Normalize line endings; a CRLF clipboard would otherwise leave stray
        // carriage returns that the sanitizer later turns into spaces.
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Delete the grapheme before the caret.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.prev_boundary(self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Delete the grapheme after the caret.
    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let end = self.next_boundary(self.cursor);
        self.text.replace_range(self.cursor..end, "");
    }

    /// Delete the word before the caret (Ctrl-W): trailing whitespace, then the
    /// run of non-whitespace.
    pub fn delete_word_back(&mut self) {
        let mut start = self.cursor;
        while start > 0 {
            let previous = self.prev_boundary(start);
            if self.text[previous..start].chars().all(char::is_whitespace) {
                start = previous;
            } else {
                break;
            }
        }
        while start > 0 {
            let previous = self.prev_boundary(start);
            if self.text[previous..start].chars().any(char::is_whitespace) {
                break;
            }
            start = previous;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Delete from the caret to the start of its line (Ctrl-U).
    pub fn delete_to_line_start(&mut self) {
        let start = self.line_start(self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Delete from the caret to the end of its line (Ctrl-K).
    pub fn delete_to_line_end(&mut self) {
        let end = self.line_end(self.cursor);
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    pub fn word_left(&mut self) {
        let mut at = self.cursor;
        while at > 0 {
            let previous = self.prev_boundary(at);
            if self.text[previous..at].chars().all(char::is_whitespace) {
                at = previous;
            } else {
                break;
            }
        }
        while at > 0 {
            let previous = self.prev_boundary(at);
            if self.text[previous..at].chars().any(char::is_whitespace) {
                break;
            }
            at = previous;
        }
        self.cursor = at;
    }

    pub fn word_right(&mut self) {
        let mut at = self.cursor;
        while at < self.text.len() {
            let next = self.next_boundary(at);
            if self.text[at..next].chars().all(char::is_whitespace) {
                at = next;
            } else {
                break;
            }
        }
        while at < self.text.len() {
            let next = self.next_boundary(at);
            if self.text[at..next].chars().any(char::is_whitespace) {
                break;
            }
            at = next;
        }
        self.cursor = at;
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start(self.cursor);
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end(self.cursor);
    }

    pub fn to_start(&mut self) {
        self.cursor = 0;
    }

    pub fn to_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Move the caret to the same column of the previous logical line. Returns
    /// false when there is no line above — the caller turns that into a
    /// different action (leaving the composer, recalling history).
    pub fn up(&mut self) -> bool {
        let start = self.line_start(self.cursor);
        if start == 0 {
            return false;
        }
        let column = wrap::width_of(&self.text[start..self.cursor]);
        let previous_start = self.line_start(start - 1);
        self.cursor = self.offset_at_column(previous_start, column);
        true
    }

    /// Move the caret to the same column of the next logical line.
    pub fn down(&mut self) -> bool {
        let end = self.line_end(self.cursor);
        if end >= self.text.len() {
            return false;
        }
        let column = wrap::width_of(&self.text[self.line_start(self.cursor)..self.cursor]);
        self.cursor = self.offset_at_column(end + 1, column);
        true
    }

    /// Lay the buffer out for a pane `width` columns wide, reporting where the
    /// caret lands so the renderer can place the terminal cursor there.
    pub fn lay_out(&self, width: usize) -> Laid {
        let width = width.max(1);
        let mut rows: Vec<String> = Vec::new();
        let mut cursor_row = 0usize;
        let mut cursor_col = 0usize;
        let mut offset = 0usize;

        for (index, logical) in self.text.split('\n').enumerate() {
            if index > 0 {
                // Account for the '\n' we split on.
                offset += 1;
            }
            let sanitized = wrap::sanitize(logical);
            let wrapped = wrap::wrap(&sanitized, width, "");
            // Where in this logical line does the caret sit?
            let caret_in_line = (self.cursor >= offset && self.cursor <= offset + logical.len())
                .then(|| self.cursor - offset);

            if let Some(caret) = caret_in_line {
                // Walk the wrapped rows accumulating consumed columns until the
                // caret's column is covered. `wrap` only removes spaces at wrap
                // points, so column arithmetic on the sanitized text is sound
                // for placing a caret.
                let caret_column = wrap::width_of(&wrap::sanitize(&logical[..caret]));
                let mut consumed = 0usize;
                let mut placed = false;
                for (row, chunk) in wrapped.iter().enumerate() {
                    let chunk_width = wrap::width_of(chunk);
                    if caret_column <= consumed + chunk_width {
                        cursor_row = rows.len() + row;
                        cursor_col = caret_column - consumed;
                        placed = true;
                        break;
                    }
                    // +1 for the space the wrap point swallowed.
                    consumed += chunk_width + 1;
                }
                if !placed {
                    cursor_row = rows.len() + wrapped.len().saturating_sub(1);
                    cursor_col = wrapped.last().map(|r| wrap::width_of(r)).unwrap_or(0);
                }
            }

            rows.extend(wrapped);
            offset += logical.len();
        }

        if rows.is_empty() {
            rows.push(String::new());
        }
        cursor_row = cursor_row.min(rows.len().saturating_sub(1));
        // A caret one column past a full row has nowhere legal to sit — the pane
        // ends there. Give it a row of its own, the way an editor does, rather
        // than clamping it back on top of the last glyph where it would look
        // like the next character will overwrite something.
        if cursor_col >= width {
            if cursor_row + 1 >= rows.len() {
                rows.push(String::new());
            }
            cursor_row += 1;
            cursor_col = 0;
        }
        Laid {
            rows,
            cursor_row,
            cursor_col,
        }
    }

    // ---- boundaries ----

    fn prev_boundary(&self, at: usize) -> usize {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn next_boundary(&self, at: usize) -> usize {
        self.text[at..]
            .graphemes(true)
            .next()
            .map(|g| at + g.len())
            .unwrap_or(self.text.len())
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(self.text.len())
    }

    /// Byte offset `column` display columns into the line starting at
    /// `line_start`, clamped to the line's end.
    fn offset_at_column(&self, line_start: usize, column: usize) -> usize {
        let end = self.line_end(line_start);
        let mut used = 0usize;
        for (index, grapheme) in self.text[line_start..end].grapheme_indices(true) {
            if used >= column {
                return line_start + index;
            }
            used += wrap::width_of(grapheme).max(1);
        }
        end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(text: &str) -> Composer {
        let mut c = Composer::default();
        c.set_text(text);
        c
    }

    #[test]
    fn editing_steps_by_grapheme_not_byte() {
        // A family emoji is one grapheme made of many bytes; backspace must
        // remove all of it, and the caret must never land mid-sequence.
        let mut c = composer("hi 👩‍👩‍👧");
        c.backspace();
        assert_eq!(c.text(), "hi ");
        // Combining accents likewise: "e" plus U+0301 is one cluster ("é"), so
        // one backspace takes the whole thing — not just the accent.
        let mut c = composer("cafe\u{301}");
        c.backspace();
        assert_eq!(c.text(), "caf");
        // Left/right are symmetric across a wide glyph.
        let mut c = composer("日本");
        c.to_end();
        c.left();
        assert_eq!(c.cursor(), "日".len());
        c.right();
        assert_eq!(c.cursor(), c.text().len());
        // Movement past the ends is a clamp, not a panic.
        c.right();
        assert_eq!(c.cursor(), c.text().len());
        c.to_start();
        c.left();
        assert_eq!(c.cursor(), 0);
        c.backspace();
        assert_eq!(c.text(), "日本");
    }

    #[test]
    fn word_deletion_eats_trailing_space_then_the_word() {
        let mut c = composer("cargo test --all   ");
        c.delete_word_back();
        assert_eq!(c.text(), "cargo test ");
        c.delete_word_back();
        assert_eq!(c.text(), "cargo ");
        c.delete_word_back();
        assert_eq!(c.text(), "");
        // Idempotent at the start.
        c.delete_word_back();
        assert_eq!(c.text(), "");
    }

    #[test]
    fn line_edits_respect_logical_lines() {
        let mut c = composer("first\nsecond\nthird");
        // Caret parked inside "second".
        c.to_start();
        c.down();
        c.end();
        assert_eq!(c.cursor(), "first\nsecond".len());
        c.delete_to_line_start();
        assert_eq!(c.text(), "first\n\nthird");

        let mut c = composer("alpha beta");
        c.home();
        c.delete_to_line_end();
        assert_eq!(c.text(), "");
    }

    #[test]
    fn vertical_movement_keeps_the_column_and_reports_the_edges() {
        let mut c = composer("aaaa\nbb\ncccc");
        c.to_start();
        c.right();
        c.right();
        c.right(); // column 3 of "aaaa"
        assert!(c.down());
        // "bb" is shorter — the caret clamps to its end, it doesn't overshoot.
        assert_eq!(c.cursor(), "aaaa\nbb".len());
        assert!(c.down());
        assert!(!c.down(), "no line below the last one");
        assert!(c.up());
        assert!(c.up());
        assert!(!c.up(), "no line above the first one");
    }

    #[test]
    fn paste_normalizes_line_endings_in_one_edit() {
        let mut c = composer("");
        c.insert_str("one\r\ntwo\rthree");
        assert_eq!(c.text(), "one\ntwo\nthree");
        assert_eq!(c.cursor(), c.text().len());
        // Pasting mid-buffer inserts at the caret.
        c.to_start();
        c.insert_str("zero\n");
        assert_eq!(c.text(), "zero\none\ntwo\nthree");
        assert_eq!(c.cursor(), "zero\n".len());
    }

    #[test]
    fn take_empties_and_resets() {
        let mut c = composer("send me");
        assert!(!c.is_empty());
        assert_eq!(c.take(), "send me");
        assert!(c.is_empty());
        assert_eq!(c.cursor(), 0);
        // Whitespace-only counts as empty: Enter must not queue a blank run.
        c.set_text("   \n  ");
        assert!(c.is_empty());
    }

    #[test]
    fn layout_places_the_caret_on_the_right_wrapped_row() {
        let c = composer("");
        let laid = c.lay_out(10);
        assert_eq!(laid.rows, [""]);
        assert_eq!((laid.cursor_row, laid.cursor_col), (0, 0));

        // Caret at the end of an exactly-full row gets a fresh row rather than
        // being clamped back onto the final glyph.
        let mut c = composer("the quick brown fox");
        c.to_end();
        let laid = c.lay_out(9);
        assert_eq!(laid.rows, ["the quick", "brown fox", ""]);
        assert_eq!((laid.cursor_row, laid.cursor_col), (2, 0));

        // A caret at the end of a row with room to spare stays on that row.
        let mut c = composer("the quick brown fox");
        c.to_end();
        let laid = c.lay_out(12);
        assert_eq!(laid.rows, ["the quick", "brown fox"]);
        assert_eq!(laid.cursor_row, 1);
        assert_eq!(laid.cursor_col, "brown fox".len());

        // Explicit newlines make rows regardless of width.
        let mut c = composer("a\nb\nc");
        c.to_start();
        c.down();
        let laid = c.lay_out(40);
        assert_eq!(laid.rows, ["a", "b", "c"]);
        assert_eq!(laid.cursor_row, 1);
        assert_eq!(laid.cursor_col, 0);

        // Caret at the start is row 0 even with content after it.
        let mut c = composer("hello world");
        c.to_start();
        let laid = c.lay_out(5);
        assert_eq!(laid.cursor_row, 0);
        assert_eq!(laid.cursor_col, 0);
    }

    #[test]
    fn layout_never_reports_a_row_or_column_out_of_bounds() {
        // Fuzz-ish: every caret position in a gnarly buffer must land inside the
        // laid-out grid, because the renderer uses these numbers as coordinates.
        let text = "short\n\na much longer line that wraps several times over\n日本語 wide\ttabbed";
        let mut c = composer(text);
        c.to_start();
        for width in [1usize, 3, 7, 12, 40] {
            c.to_start();
            loop {
                let laid = c.lay_out(width);
                assert!(
                    laid.cursor_row < laid.rows.len(),
                    "row {} out of {} rows at width {width}",
                    laid.cursor_row,
                    laid.rows.len()
                );
                assert!(
                    laid.cursor_col < width.max(1),
                    "col {} past width {width}",
                    laid.cursor_col
                );
                let before = c.cursor();
                c.right();
                if c.cursor() == before {
                    break;
                }
            }
        }
    }
}
