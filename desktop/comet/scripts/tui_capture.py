"""Shared pty driver and terminal emulator for the `comet-tui` scripts.

Used by `tui-smoke.py` (assert on what the frame says) and `tui-screenshots.py`
(render the frame to an image). Both need the same thing: run the real binary
under a pty and reconstruct the screen it painted.

Reconstruction is necessary rather than fussy. ratatui addresses the cursor per
changed run and repaints only diffs, so the byte stream is a sequence of
"jump here, write that" — stripping the escape codes yields concatenated
fragments in the wrong order. `Screen` interprets the subset ratatui emits (CUP,
ED, EL, and SGR) and keeps a character grid plus a per-cell style, which is
exactly what both callers want.
"""

from __future__ import annotations

import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import termios
import time

CSI = re.compile(rb"\x1b\[([0-9;?]*)([@-~])")
OSC = re.compile(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
# A CSI or OSC that has started but not finished — the tail of a read() that
# split a sequence in half. Feeding it as literal text would paint escape
# fragments like "34;30H" into the grid.
PARTIAL = re.compile(rb"\x1b(\[[0-9;?]*|\][^\x07\x1b]*|)$")

# The 16 ANSI colors, for the rare span that uses one (crossterm emits truecolor
# for our theme, but `Color::Reset` and terminal defaults still appear).
ANSI16 = [
    "#000000", "#cd3131", "#0dbc79", "#e5e510",
    "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5",
    "#666666", "#f14c4c", "#23d18b", "#f5f543",
    "#3b8eea", "#d670d6", "#29b8db", "#e5e5e5",
]


class Style:
    """Cell appearance. Immutable-ish; `Screen` copies on change."""

    __slots__ = ("fg", "bg", "bold", "italic", "reverse", "dim", "underline")

    def __init__(self) -> None:
        self.fg: str | None = None
        self.bg: str | None = None
        self.bold = False
        self.italic = False
        self.reverse = False
        self.dim = False
        self.underline = False

    def copy(self) -> "Style":
        other = Style()
        for name in self.__slots__:
            setattr(other, name, getattr(self, name))
        return other

    def key(self) -> tuple:
        return tuple(getattr(self, name) for name in self.__slots__)


class Screen:
    """Just enough VT to read a ratatui frame back, with colors."""

    def __init__(self, rows: int, cols: int):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.styles = [[Style() for _ in range(cols)] for _ in range(rows)]
        self.y = self.x = 0
        self.pen = Style()
        # Bytes carried over from the previous chunk: an unfinished escape
        # sequence or a UTF-8 character split across reads.
        self.pending = b""

    # -- input ------------------------------------------------------------

    def feed(self, data: bytes) -> None:
        data = self.pending + data
        self.pending = b""
        i = 0
        while i < len(data):
            byte = data[i : i + 1]
            if byte == b"\x1b":
                if PARTIAL.match(data, i):
                    self.pending = data[i:]
                    return
                match = CSI.match(data, i)
                if match:
                    self._csi(match.group(1).decode(), match.group(2).decode())
                    i = match.end()
                    continue
                match = OSC.match(data, i)
                if match:
                    i = match.end()
                    continue
                # Non-CSI escape (charset select, keypad mode): two bytes.
                i += 2
                continue
            if byte == b"\n":
                self.y = min(self.y + 1, self.rows - 1)
                i += 1
            elif byte == b"\r":
                self.x = 0
                i += 1
            elif byte < b" ":
                i += 1
            else:
                length = 1
                first = data[i]
                if first >= 0xF0:
                    length = 4
                elif first >= 0xE0:
                    length = 3
                elif first >= 0xC0:
                    length = 2
                if i + length > len(data):
                    self.pending = data[i:]
                    return
                char = data[i : i + length].decode("utf8", "replace")
                if self.y < self.rows and self.x < self.cols:
                    self.grid[self.y][self.x] = char
                    self.styles[self.y][self.x] = self.pen.copy()
                self.x += 1
                if self.x >= self.cols:
                    self.x = self.cols - 1
                i += length

    def _csi(self, params: str, final: str) -> None:
        raw = [p for p in params.split(";") if p != ""]
        numbers = [int(p) for p in raw if p.isdigit()]
        if final == "H":
            self.y = min(max((numbers[0] if numbers else 1) - 1, 0), self.rows - 1)
            self.x = min(max((numbers[1] if len(numbers) > 1 else 1) - 1, 0), self.cols - 1)
        elif final == "J":
            self.grid = [[" "] * self.cols for _ in range(self.rows)]
            self.styles = [[Style() for _ in range(self.cols)] for _ in range(self.rows)]
            self.y = self.x = 0
        elif final == "K":
            for x in range(self.x, self.cols):
                self.grid[self.y][x] = " "
                self.styles[self.y][x] = Style()
        elif final == "m":
            self._sgr(numbers if numbers else [0])

    def _sgr(self, numbers: list[int]) -> None:
        index = 0
        while index < len(numbers):
            code = numbers[index]
            if code == 0:
                self.pen = Style()
            elif code == 1:
                self.pen.bold = True
            elif code == 2:
                self.pen.dim = True
            elif code == 3:
                self.pen.italic = True
            elif code == 4:
                self.pen.underline = True
            elif code == 7:
                self.pen.reverse = True
            elif code in (22,):
                self.pen.bold = self.pen.dim = False
            elif code == 23:
                self.pen.italic = False
            elif code == 24:
                self.pen.underline = False
            elif code == 27:
                self.pen.reverse = False
            elif code == 39:
                self.pen.fg = None
            elif code == 49:
                self.pen.bg = None
            elif 30 <= code <= 37:
                self.pen.fg = ANSI16[code - 30]
            elif 40 <= code <= 47:
                self.pen.bg = ANSI16[code - 40]
            elif 90 <= code <= 97:
                self.pen.fg = ANSI16[code - 90 + 8]
            elif 100 <= code <= 107:
                self.pen.bg = ANSI16[code - 100 + 8]
            elif code in (38, 48) and index + 1 < len(numbers):
                mode = numbers[index + 1]
                if mode == 2 and index + 4 < len(numbers):
                    color = "#%02x%02x%02x" % tuple(numbers[index + 2 : index + 5])
                    index += 4
                elif mode == 5 and index + 2 < len(numbers):
                    color = xterm256(numbers[index + 2])
                    index += 2
                else:
                    index += 1
                    continue
                if code == 38:
                    self.pen.fg = color
                else:
                    self.pen.bg = color
            index += 1

    # -- output -----------------------------------------------------------

    def lines(self) -> list[str]:
        """Rows as the terminal shows them.

        A wide glyph occupies one cell and leaves a filler in the next, which
        ratatui's diff skips rather than emitting; skipping it here too is what
        makes a row's length equal its column count.
        """
        out = []
        for y in range(self.rows):
            row = []
            x = 0
            while x < self.cols:
                symbol = self.grid[y][x]
                row.append(symbol)
                x += max(char_width(symbol), 1)
            out.append("".join(row).rstrip())
        return out

    def text(self) -> str:
        return "\n".join(self.lines())

    def runs(self) -> list[list[tuple[str, Style]]]:
        """Rows as (text, style) runs — what a renderer wants."""
        out = []
        for y in range(self.rows):
            row: list[tuple[str, Style]] = []
            x = 0
            while x < self.cols:
                symbol = self.grid[y][x]
                style = self.styles[y][x]
                if row and row[-1][1].key() == style.key():
                    row[-1] = (row[-1][0] + symbol, style)
                else:
                    row.append((symbol, style))
                x += max(char_width(symbol), 1)
            out.append(row)
        return out

    def show(self, label: str) -> None:
        # Trim blank rows at the top and bottom, but never in the middle: the
        # renderer uses blank rows as its only separator, so dropping them makes
        # a correctly-spaced frame look like a wall of text. (This used to drop
        # them everywhere and got away with it only because a sidebar divider
        # made every row non-empty.)
        lines = self.lines()
        body = [i for i, line in enumerate(lines) if line.strip()]
        print(f"  ┌─ {label} " + "─" * max(0, 70 - len(label)))
        if body:
            for line in lines[body[0] : body[-1] + 1]:
                print("  │ " + line[: self.cols].rstrip())
        print("  └" + "─" * 72)


def xterm256(index: int) -> str:
    if index < 16:
        return ANSI16[index]
    if index < 232:
        index -= 16
        levels = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (
            levels[index // 36],
            levels[(index // 6) % 6],
            levels[index % 6],
        )
    value = 8 + (index - 232) * 10
    return "#%02x%02x%02x" % (value, value, value)


def char_width(char: str) -> int:
    """Columns a glyph occupies, so continuation cells can be skipped.

    Must match what ratatui assumed when it laid the cell out, or every row
    containing the glyph shifts. `east_asian_width` is the authority; the one
    thing it misses is astral-plane emoji, which are wide in every terminal.
    Deliberately *not* a blanket rule over U+2600–U+27BF — that block holds
    plenty of narrow symbols (`✓`, `✗`, `⌁`), and treating them as wide eats the
    space after them.
    """
    if not char or char == "\0":
        return 1
    import unicodedata

    if unicodedata.combining(char[0]):
        return 0
    if unicodedata.east_asian_width(char[0]) in ("W", "F"):
        return 2
    if 0x1F300 <= ord(char[0]) <= 0x1FAFF:
        return 2
    return 1


class Tui:
    """The `comet-tui` binary under a pty."""

    def __init__(self, binary: str, env: dict, rows: int, cols: int, args: list[str] | None = None):
        self.screen = Screen(rows, cols)
        self.primary, secondary = pty.openpty()
        fcntl.ioctl(secondary, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.proc = subprocess.Popen(
            [binary, *(args or [])],
            env=env,
            stdin=secondary,
            stdout=secondary,
            stderr=secondary,
            close_fds=True,
        )
        os.close(secondary)
        self.trailing = b""

    def settle(self, seconds: float = 1.5, quiet: float = 0.35) -> Screen:
        """Read until the frame stops changing (or the budget runs out)."""
        deadline = time.time() + seconds
        last = time.time()
        while time.time() < deadline:
            ready, _, _ = select.select([self.primary], [], [], 0.1)
            if ready:
                try:
                    chunk = os.read(self.primary, 1 << 20)
                except OSError:
                    break
                if not chunk:
                    break
                self.screen.feed(chunk)
                self.trailing += chunk
                last = time.time()
            elif time.time() - last > quiet:
                break
        return self.screen

    def send(self, data: bytes, settle: float = 1.0) -> Screen:
        os.write(self.primary, data)
        return self.settle(settle)

    def quit(self) -> tuple[int, str]:
        """Ctrl-C, then collect what was printed to the restored terminal.

        Draining has to happen *while* the process exits, not after: on BSD (so
        macOS) a read on the primary fd returns EIO as soon as the last secondary
        closes, discarding whatever was still buffered — which is exactly the
        farewell we want to read.
        """
        self.trailing = b""
        os.write(self.primary, b"\x03")
        deadline = time.time() + 15
        code = None
        while time.time() < deadline:
            ready, _, _ = select.select([self.primary], [], [], 0.1)
            if ready:
                try:
                    chunk = os.read(self.primary, 1 << 20)
                except OSError:
                    break
                if not chunk:
                    break
                self.trailing += chunk
                continue
            if code is None:
                code = self.proc.poll()
            else:
                break
        if code is None:
            try:
                code = self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                return (-1, "<hung>")
        farewell = OSC.sub(b"", CSI.sub(b"", self.trailing)).decode("utf8", "replace")
        return code, farewell
