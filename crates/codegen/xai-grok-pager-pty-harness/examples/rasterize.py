#!/usr/bin/env python3
"""Rasterize StyledLine JSON (from the tui_shot harness example) to PNG."""
import json, sys
from PIL import Image, ImageDraw, ImageFont

SCALE = 2
CELL_W, CELL_H = 9 * SCALE, 19 * SCALE
PAD_X, PAD_TOP = 16 * SCALE, 34 * SCALE
PAD_BOTTOM = 16 * SCALE

F_REG = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
F_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
F_SANS = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
F_CJK = "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"
F_CJKB = "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc"

def is_wide(ch):
    o = ord(ch)
    return (0x1100 <= o <= 0x115F or 0x2E80 <= o <= 0xA4CF or
            0xAC00 <= o <= 0xD7A3 or 0xF900 <= o <= 0xFAFF or
            0xFE30 <= o <= 0xFE4F or 0xFF00 <= o <= 0xFF60 or
            0xFFE0 <= o <= 0xFFE6 or 0x20000 <= o <= 0x2FFFD or 0x30000 <= o <= 0x3FFFD)

def hx(c, default):
    if not c: return default
    c = c.lstrip("#")
    try: return tuple(int(c[i:i+2], 16) for i in (0, 2, 4))
    except Exception: return default

def render(jpath, out_path, cols=100, rows=30):
    lines = {l["line"]: l["runs"] for l in json.load(open(jpath))}
    w = cols * CELL_W + PAD_X * 2
    h = rows * CELL_H + PAD_TOP + PAD_BOTTOM
    img = Image.new("RGB", (w, h), (10, 10, 16))
    d = ImageDraw.Draw(img)
    # window chrome: traffic lights + title
    d.rounded_rectangle([2, 2, w - 3, h - 3], radius=12, fill=(16, 16, 24), outline=(64, 54, 96))
    for i, c in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
        d.ellipse([14 + 18 * i, 13, 24 + 18 * i, 23], fill=c)
    d.text((w / 2 - 30, 11), "hyper", font=ImageFont.truetype(F_BOLD, 8 * SCALE), fill=(120, 120, 140))
    f_reg = ImageFont.truetype(F_REG, 8 * SCALE * 2)
    f_bold = ImageFont.truetype(F_BOLD, 8 * SCALE * 2)
    f_sans = ImageFont.truetype(F_SANS, 9 * SCALE * 2)
    f_cjk = ImageFont.truetype(F_CJK, 8 * SCALE * 2)
    f_cjkb = ImageFont.truetype(F_CJKB, 8 * SCALE * 2)
    DEF_FG, DEF_BG = (205, 205, 220), (16, 16, 24)
    def pick_font(ch, bold):
        o = ord(ch)
        if is_wide(ch):
            return f_cjkb if bold else f_cjk
        # Box-drawing (0x2500-0x259F) and Braille (0x2800-0x28FF, the
        # grok-build logo) live in DejaVu Sans, not the mono fonts.
        if 0x2500 <= o <= 0x28FF:
            return f_sans
        return f_bold if bold else f_reg
    for y in range(rows):
        runs = lines.get(y, [])
        x = 0
        for r in runs:
            text = r["text"]
            fg = hx(r.get("fg"), DEF_FG)
            bg = hx(r.get("bg"), DEF_BG)
            bold, italic, under, strike, dim, inv = (r.get("bold"), r.get("italic"),
                r.get("underline"), r.get("strikeout"), r.get("dim"), r.get("inverse"))
            if dim:
                fg = tuple(int(v * 0.65) for v in fg)
            if inv:
                fg, bg = (bg if bg != DEF_BG else (100, 100, 130)), fg
            for ch in text:
                if ch == "\x00": continue
                wpx = CELL_W * (2 if is_wide(ch) else 1)
                px, py = PAD_X + x * CELL_W, PAD_TOP + y * CELL_H
                if bg != DEF_BG or inv:
                    d.rectangle([px, py, px + wpx, py + CELL_H], fill=bg)
                if ch != " ":
                    d.text((px, py - 1), ch, font=pick_font(ch, bold), fill=fg)
                    if under:
                        d.line([px, py + CELL_H - 2, px + wpx, py + CELL_H - 2], fill=fg)
                    if strike:
                        d.line([px, py + CELL_H // 2, px + wpx, py + CELL_H // 2], fill=fg)
                x += 2 if is_wide(ch) else 1
    img.save(out_path)
    print(f"{jpath} -> {out_path} ({w}x{h})")

if __name__ == "__main__":
    render("/tmp/shot-en.json", "docs/assets/screenshot-welcome-en.png")
    render("/tmp/shot-zh.json", "docs/assets/screenshot-welcome-zh.png")
