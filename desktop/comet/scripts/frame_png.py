#!/usr/bin/env python3
"""Render a frame_dump JSON cell grid to a PNG.

Reading a text dump tells you where the glyphs are and nothing about how the
frame looks, which is the thing being designed. This draws every cell with its
real foreground, background and modifiers, at a terminal-ish metric.
"""
import json
import sys

from PIL import Image, ImageDraw, ImageFont

FONT = "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf"
FONT_BOLD = "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Bold.ttf"
FALLBACK = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
SIZE = 22
CELL_W, CELL_H = 13, 30
# What the terminal itself shows for `reset` — a plain dark terminal.
TERM_BG = "#0d0b14"
TERM_FG = "#d4d4d4"
BOLD = 1 << 0
DIM = 1 << 2
REVERSED = 1 << 6 if False else 1 << 4


def load(path):
    return json.load(open(path)) if path else json.load(sys.stdin)


def main():
    src = sys.argv[1] if len(sys.argv) > 1 else None
    out = sys.argv[2] if len(sys.argv) > 2 else "/tmp/frame.png"
    data = load(src)
    w, h = data["w"], data["h"]
    img = Image.new("RGB", (w * CELL_W, h * CELL_H), TERM_BG)
    draw = ImageDraw.Draw(img)
    regular = ImageFont.truetype(FONT, SIZE)
    bold = ImageFont.truetype(FONT_BOLD, SIZE)
    fallback = ImageFont.truetype(FALLBACK, SIZE)

    def has_glyph(font, ch):
        probe = Image.new("L", (SIZE * 2, SIZE * 2), 0)
        ImageDraw.Draw(probe).text((2, 2), ch, font=font, fill=255)
        return probe.getbbox() is not None

    # Backgrounds first, so a fill never clips the glyph drawn before it.
    for c in data["cells"]:
        bg = c["b"]
        if bg == "reset":
            continue
        x, y = c["x"] * CELL_W, c["y"] * CELL_H
        draw.rectangle([x, y, x + CELL_W - 1, y + CELL_H - 1], fill=bg)

    for c in data["cells"]:
        ch = c["s"]
        if not ch.strip():
            continue
        fg = TERM_FG if c["f"] == "reset" else c["f"]
        mods = c["m"]
        font = bold if mods & BOLD else regular
        if not has_glyph(font, ch):
            font = fallback
        x, y = c["x"] * CELL_W, c["y"] * CELL_H
        draw.text((x, y + 3), ch, font=font, fill=fg)

    img.save(out)
    print(out, img.size)


if __name__ == "__main__":
    main()
