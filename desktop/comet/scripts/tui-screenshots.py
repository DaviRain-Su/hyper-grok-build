#!/usr/bin/env python3
"""Capture `comet-tui` frames as PNGs.

Drives the real binary against a seeded throwaway daemon, reconstructs each frame
with the emulator in `tui_capture` (colors and attributes included), renders it to
HTML styled like a terminal, and screenshots that with headless Chrome. The
result is a pixel-accurate picture of what the app actually painted — not a
mockup, and not a lossy text dump.

    scripts/tui-screenshots.py [--out DIR] [--keep]

Requires: cargo build -p comet -p comet-tui-bin, and Google Chrome.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from tui_capture import Screen, Style, Tui  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "comet-tui")
COMET = os.path.join(ROOT, "target", "debug", "comet")
DATA_DIR = "/tmp/comet-tui-shots/daemon"
IPC = "27934"
ROWS, COLS = 36, 116
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

# The theme's own background, so the image matches a terminal running it rather
# than whatever the default page color is.
BACKGROUND = "#131316"
FOREGROUND = "#e4e4e7"
# Cell metrics in px. 8.4 x 18 is close to SF Mono 14px/1.28, and picking them
# explicitly keeps every frame the same size regardless of font fallback.
CELL_W, CELL_H = 8.4, 18.0
FONT_PX = 14

ENV = dict(
    os.environ,
    COMET_DATA_DIR=DATA_DIR,
    COMET_IPC_PORT=IPC,
    COMET_HARNESS="mock",
    COMET_WORKOS_CLIENT_ID="",
    # Pace the mock stream so a mid-run frame is catchable.
    COMET_MOCK_DELAY_MS="420",
    RUST_LOG="warn",
    TERM="xterm-256color",
)


# --------------------------------------------------------------------------
# Rendering
# --------------------------------------------------------------------------


def escape(text: str) -> str:
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace(" ", " ")
    )


def css(style: Style) -> str:
    fg, bg = style.fg, style.bg
    if style.reverse:
        fg, bg = bg or BACKGROUND, fg or FOREGROUND
    rules = []
    if fg:
        rules.append(f"color:{fg}")
    if bg:
        rules.append(f"background:{bg}")
    if style.bold:
        rules.append("font-weight:600")
    if style.italic:
        rules.append("font-style:italic")
    if style.underline:
        rules.append("text-decoration:underline")
    if style.dim:
        rules.append("opacity:.65")
    return ";".join(rules)


def to_html(screen: Screen, title: str) -> str:
    """Render a frame as a terminal-looking HTML page.

    Sized from the screen's own dimensions rather than the module constants, so a
    short frame (the shell transcript in `text_frame`) produces a short image
    instead of one padded out to the full app height.
    """
    rows = []
    for run_row in screen.runs():
        spans = []
        for text, style in run_row:
            rule = css(style)
            body = escape(text)
            spans.append(f'<span style="{rule}">{body}</span>' if rule else body)
        rows.append("".join(spans))
    body = "\n".join(rows)
    width = screen.cols * CELL_W
    height = screen.rows * CELL_H
    return f"""<!doctype html>
<meta charset="utf-8">
<title>{escape(title)}</title>
<style>
  html,body {{ margin:0; padding:0; background:transparent; }}
  .frame {{
    width:{width + 44:.0f}px;
    background:{BACKGROUND};
    padding:14px 22px 18px;
    box-sizing:border-box;
    border-radius:10px;
    box-shadow:0 18px 48px rgba(0,0,0,.55);
    font-family:"SF Mono","Menlo","DejaVu Sans Mono",monospace;
    font-size:{FONT_PX}px;
    line-height:{CELL_H}px;
    color:{FOREGROUND};
    -webkit-font-smoothing:antialiased;
  }}
  .bar {{ display:flex; gap:7px; padding-bottom:11px; }}
  .bar i {{ width:11px; height:11px; border-radius:50%; display:block; }}
  pre {{ margin:0; white-space:pre; height:{height:.0f}px; letter-spacing:0; }}
</style>
<div class="frame">
  <div class="bar">
    <i style="background:#ff5f57"></i><i style="background:#febc2e"></i><i style="background:#28c840"></i>
  </div>
  <pre>{body}</pre>
</div>
"""


def render_png(html: str, out_png: str, scratch: str, screen: Screen) -> None:
    """HTML -> PNG via headless Chrome, the window sized to the frame."""
    html_path = os.path.join(scratch, os.path.basename(out_png) + ".html")
    with open(html_path, "w") as handle:
        handle.write(html)
    width = int(screen.cols * CELL_W + 44) + 2
    height = int(screen.rows * CELL_H + 14 + 18 + 22) + 2
    result = subprocess.run(
        [
            CHROME,
            "--headless",
            "--disable-gpu",
            "--hide-scrollbars",
            "--force-device-scale-factor=2",
            "--default-background-color=00000000",
            f"--window-size={width},{height}",
            f"--screenshot={out_png}",
            f"file://{html_path}",
        ],
        capture_output=True,
        text=True,
    )
    if not os.path.exists(out_png):
        raise SystemExit(f"chrome failed:\n{result.stderr[-2000:]}")


def text_frame(lines: list[str], title: str) -> tuple[str, Screen]:
    """A plain shell transcript in the same chrome — for the detach shot, where
    the point is what the *shell* says after the viewport is gone."""
    cols = max(len(line) for line in lines) + 4
    screen = Screen(len(lines), cols)
    for y, line in enumerate(lines):
        for x, char in enumerate(line[:cols]):
            screen.grid[y][x] = char
    return to_html(screen, title), screen


# --------------------------------------------------------------------------
# Seeding
# --------------------------------------------------------------------------


def rpc(method: str, params: str) -> str:
    result = subprocess.run(
        [
            "cargo", "run", "-q", "-p", "comet-rpc", "--example", "rpc_probe", "--",
            f"ws://127.0.0.1:{IPC}", method, params,
        ],
        cwd=ROOT,
        env=ENV,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SystemExit(f"rpc {method} failed: {result.stderr[-1500:]}")
    return result.stdout


def seed(device_id: str) -> str:
    """A workspace that looks like real use: three spaces, several sessions."""
    home = os.environ.get("HOME", "/home/w")
    spaces = {}
    for project in ("comet-native", "soccertcg", "aether"):
        space_id = str(uuid.uuid4())
        rpc(
            "Mutate",
            json.dumps(
                {
                    "op": "createSpace",
                    "spaceId": space_id,
                    "deviceId": device_id,
                    "path": f"{home}/github/{project}",
                }
            ),
        )
        spaces[project] = space_id

    focus = None
    rows = [
        ("comet-native", "Ratatui terminal frontend", "comet/high-performance-tui", 0, True),
        ("comet-native", "Diff sidebar reconciler", "comet-native/main", 3, False),
        ("soccertcg", "Rebalance player stat caps", "comet/rebalance-stat-caps", 2, True),
        ("soccertcg", "Craft premium TCG experience", "comet/craft-premium-exp", 26, False),
        ("aether", "Initial context exploration", "aether/main", 48, False),
    ]
    for project, title, branch, age_hours, run in rows:
        chat_id = str(uuid.uuid4())
        rpc(
            "Mutate",
            json.dumps(
                {
                    "op": "createChat",
                    "chatId": chat_id,
                    "spaceId": spaces[project],
                    "config": {
                        "harness": "mock",
                        "model": "fable-5",
                        "reasoning": None,
                        "sandbox": "workspace-write",
                    },
                }
            ),
        )
        rpc("Mutate", json.dumps({"op": "renameChat", "chatId": chat_id, "title": title}))
        rpc("Mutate", json.dumps({"op": "setChatBranch", "chatId": chat_id, "branch": branch}))
        if run:
            rpc(
                "QueueCommand",
                json.dumps(
                    {
                        "chatId": chat_id,
                        "command": {
                            "kind": "run",
                            "messageId": str(uuid.uuid4()),
                            "request": {
                                "prompt": "Walk me through the streaming pipeline",
                                "model": None,
                                "reasoning": None,
                                "modelOptions": {},
                                "cwd": "/tmp",
                                "sandbox": "workspace-write",
                                "autoApprove": True,
                                "resume": None,
                            },
                        },
                    }
                ),
            )
            time.sleep(6)
            if focus is None:
                focus = chat_id
        rpc(
            "Mutate",
            json.dumps(
                {
                    "op": "setChatActivity",
                    "chatId": chat_id,
                    "lastMessageAt": int((time.time() - age_hours * 3600) * 1000),
                }
            ),
        )
    return focus


# --------------------------------------------------------------------------


def daemon_pid() -> int | None:
    try:
        with open(os.path.join(DATA_DIR, "engine.lock")) as handle:
            return int(handle.read().strip())
    except (OSError, ValueError):
        return None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", default="/tmp/comet-tui-shots/png")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()

    for binary in (BIN, COMET, CHROME):
        if not os.path.exists(binary):
            print(f"missing {binary}")
            return 2

    subprocess.run(["rm", "-rf", "/tmp/comet-tui-shots"], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    os.makedirs(args.out, exist_ok=True)
    scratch = "/tmp/comet-tui-shots/html"
    os.makedirs(scratch, exist_ok=True)

    shots: list[tuple[str, str]] = []

    def shoot(name: str, screen: Screen, title: str) -> None:
        path = os.path.join(args.out, f"{name}.png")
        render_png(to_html(screen, title), path, scratch, screen)
        size = os.path.getsize(path)
        print(f"  ✓ {name}.png  ({size // 1024} KB)")
        shots.append((name, path))

    try:
        print("starting the engine + seeding a workspace")
        tui = Tui(BIN, ENV, ROWS, COLS)
        tui.settle(25, quiet=1.2)
        device = rpc("LocalDevice", "{}")
        device_id = device.strip().split('"deviceId":"')[1].split('"')[0]
        seed(device_id)
        tui.settle(4, quiet=0.6)

        # 1. Overview: sidebar + a finished transcript + the prompt.
        #    `g` then `j` lands on the first session of the first space, which is
        #    the one `seed` gave a completed run — an empty transcript would
        #    make a poor picture of a chat app.
        tui.send(b"\t", 0.8)               # sidebar
        tui.send(b"g", 0.4)                # top: the first space
        tui.send(b"\r", 1.2)               # activate it (swaps the tab strip)
        tui.send(b"j", 0.4)                # walk down to the Sessions list
        tui.send(b"j", 0.4)
        tui.send(b"j", 0.4)
        screen = tui.send(b"\r", 3.0)      # open the session
        shoot("01-overview", screen, "comet-tui — overview")

        # 2. Help overlay.
        tui.send(b"\x1b", 0.5)             # out of the composer
        screen = tui.send(b"?", 1.0)
        shoot("02-keys", screen, "comet-tui — key map")
        tui.send(b"\x1b", 0.5)

        # 3. Streaming: send a prompt and catch it mid-run (the mock harness is
        #    paced by COMET_MOCK_DELAY_MS so this is reliable).
        tui.send(b"i", 0.4)
        tui.send(b"how does the transcript cache stay O(changed)?", 0.6)
        tui.send(b"\r", 0.2)
        screen = tui.settle(2.2, quiet=0.15)
        shoot("03-streaming", screen, "comet-tui — streaming")

        # 4. Scrolled back: the follow banner, and history above the live tail.
        tui.settle(14, quiet=1.0)
        tui.send(b"\x1b", 0.4)
        tui.send(b"k" * 8, 1.0)
        screen = tui.settle(1.0, quiet=0.3)
        shoot("04-scrolled", screen, "comet-tui — scrolled back")

        # 5. Detaching: the shell after Ctrl-C, plus proof the engine is up.
        code, farewell = tui.quit()
        probe = subprocess.run([BIN, "--probe"], env=ENV, capture_output=True, text=True)
        lines = ["$ comet-tui"]
        lines += [line for line in farewell.splitlines() if line.strip()]
        lines += ["", f"$ comet-tui --probe    # exit {probe.returncode}"]
        lines += [line for line in probe.stdout.splitlines() if line.strip()]
        lines += ["", "$ # the agents are still running; run comet-tui again to reattach"]
        path = os.path.join(args.out, "05-detach.png")
        html, frame = text_frame(lines, "comet-tui — detaching")
        render_png(html, path, scratch, frame)
        print(f"  ✓ 05-detach.png  ({os.path.getsize(path) // 1024} KB)  [exit {code}]")
        shots.append(("05-detach", path))

    finally:
        if not args.keep:
            pid = daemon_pid()
            if pid:
                try:
                    os.kill(pid, 15)
                except ProcessLookupError:
                    pass

    print(f"\n{len(shots)} shots in {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
