//! Bounds-checked buffer helpers.
//!
//! Ratatui's `Buffer::set_line`, `set_span`, and `set_string` panic when
//! given out-of-bounds coordinates (via `index_of`).  During terminal resize
//! races, computed widget areas can momentarily exceed the buffer, causing
//! a crash.
//!
//! This extension trait provides `set_line_safe` / `set_span_safe` /
//! `set_string_safe` that silently skip the write when `y` is outside the
//! buffer — trading a single missed frame for a panic-free resize.

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Extension trait for bounds-checked buffer writes.
pub trait SafeBuf {
    /// Like `Buffer::set_line` but returns immediately when `y` is outside
    /// the buffer area.
    fn set_line_safe(&mut self, x: u16, y: u16, line: &Line<'_>, width: u16);

    /// Like `Buffer::set_span` but returns immediately when `y` is outside
    /// the buffer area.
    fn set_span_safe(&mut self, x: u16, y: u16, span: &Span<'_>, width: u16);

    /// Like `Buffer::set_string` but returns immediately when `y` is outside
    /// the buffer area.
    fn set_string_safe<S: AsRef<str>>(&mut self, x: u16, y: u16, string: S, style: Style);
}

impl SafeBuf for Buffer {
    #[inline]
    fn set_line_safe(&mut self, x: u16, y: u16, line: &Line<'_>, width: u16) {
        if y >= self.area.y && y < self.area.bottom() && x < self.area.right() {
            self.set_line(x, y, line, width);
        }
    }

    #[inline]
    fn set_span_safe(&mut self, x: u16, y: u16, span: &Span<'_>, width: u16) {
        if y >= self.area.y && y < self.area.bottom() && x < self.area.right() {
            self.set_span(x, y, span, width);
        }
    }

    #[inline]
    fn set_string_safe<S: AsRef<str>>(&mut self, x: u16, y: u16, string: S, style: Style) {
        if y >= self.area.y && y < self.area.bottom() && x < self.area.right() {
            self.set_string(x, y, string, style);
        }
    }
}
