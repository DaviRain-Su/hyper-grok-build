#!/usr/bin/env python3
"""End-to-end smoke test for `comet-tui`, driven through a real pty.

The property under test is the one no unit test can reach: that the viewport and
the engine have independent lifetimes. We start the TUI with nothing running,
let it spawn a detached daemon, do real work through it, quit, and then check the
engine is *still there* — and that reattaching finds the work.

The pty driver and terminal emulator live in `tui_capture`, shared with
`tui-screenshots.py`: ratatui addresses the cursor per changed run, so the frame
has to be reconstructed rather than grepped out of the byte stream.

Usage:  scripts/tui-smoke.py [--keep]
        --keep  leave the daemon running afterwards (for poking at by hand)
"""

from __future__ import annotations

import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from tui_capture import CSI, OSC, Screen, Tui  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "comet-tui")
COMET = os.path.join(ROOT, "target", "debug", "comet")
DATA_DIR = "/tmp/comet-tui-smoke/daemon"
IPC = "27933"
ROWS, COLS = 34, 110

ENV = dict(
    os.environ,
    COMET_DATA_DIR=DATA_DIR,
    COMET_IPC_PORT=IPC,
    # The mock harness streams a canned reply, so no agent CLI is needed.
    COMET_HARNESS="mock",
    # Empty client id forces dev-mode auth (apps/comet's documented escape
    # hatch), so a detached daemon with no TTY doesn't need `comet login`.
    COMET_WORKOS_CLIENT_ID="",
    RUST_LOG="warn",
    TERM="xterm-256color",
)


def tui(*args):
    return subprocess.run([BIN, *args], env=ENV, capture_output=True, text=True)


def launch() -> Tui:
    return Tui(BIN, ENV, ROWS, COLS)


# --------------------------------------------------------------------------
# Checks
# --------------------------------------------------------------------------

FAILURES: list[str] = []


def check(condition: bool, what: str, detail: str = "") -> None:
    if condition:
        print(f"  ✓ {what}")
    else:
        print(f"  ✗ {what}")
        if detail:
            print("      " + detail.replace("\n", "\n      ")[:2000])
        FAILURES.append(what)


def probe() -> bool:
    result = subprocess.run([BIN, "--probe"], env=ENV, capture_output=True, text=True)
    return result.returncode == 0


def main() -> int:
    keep = "--keep" in sys.argv
    for binary in (BIN, COMET):
        if not os.path.exists(binary):
            print(f"missing {binary} — run: cargo build -p comet -p comet-tui-bin")
            return 2
    subprocess.run(["rm", "-rf", "/tmp/comet-tui-smoke"], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)

    try:
        print("\n1. nothing is running yet")
        check(not probe(), "--probe reports no engine")

        print("\n2. launch with no daemon: it must start one and connect")
        tui = launch()
        screen = tui.settle(25, quiet=1.2)
        screen.show("first frame")
        # Both sidebar sections are present before any data arrives.
        check(
            "Spaces" in screen.text() and "Sessions" in screen.text(),
            "the shell rendered",
            screen.text(),
        )
        check(
            "Starting the engine" not in screen.text(),
            "the boot gate cleared (engine reachable)",
            screen.text(),
        )
        # The engine label left the viewport with the status bar; `--probe` is
        # the check that it is really attached, and it runs immediately below.
        check(probe(), "a daemon is listening")

        print("\n3. create a space and a session, then send a prompt")
        # A fresh store has no spaces, so make one through the same RPC the
        # desktop picker uses; the TUI has no folder browser.
        device = rpc("LocalDevice", "{}")
        device_id = device.strip().split('"deviceId":"')[1].split('"')[0]
        space_id = new_uuid()
        rpc(
            "Mutate",
            f'{{"op":"createSpace","spaceId":"{space_id}","deviceId":"{device_id}",'
            f'"path":"{ROOT}"}}',
        )
        screen = tui.settle(3, quiet=0.5)
        check(
            "swift-delta" in screen.text() or "comet-native" in screen.text(),
            "the new space appeared without a restart",
            screen.text(),
        )

        screen = tui.send(b"\t", 1.0)  # focus the sidebar
        screen = tui.send(b"n", 3.0)  # new session
        screen.show("after n")
        check("New session" in screen.text(), "a session row was created", screen.text())

        screen = tui.send(b"explain the streaming pipeline", 1.0)
        check(
            "explain the streaming pipeline" in screen.text(),
            "typing reaches the composer",
            screen.text(),
        )
        screen = tui.send(b"\r", 8.0)
        screen.show("after send")
        # The reply is long and the transcript is bottom-anchored, so the prompt
        # that started it has scrolled off; escape to the transcript and page up
        # to it. (It used to fit only because the blocks had no padding.)
        tui.send(b"\x1b", 0.4)
        scrolled = tui.send(b"k" * 30, 1.0)
        check(
            "explain the streaming" in scrolled.text(),
            "the prompt echoed into the transcript",
            screen.text(),
        )
        check(
            "mock" in screen.text().lower() or len(screen.text()) > 400,
            "the mock harness replied",
            screen.text(),
        )

        print("\n4. detach with Ctrl-C")
        code, farewell = tui.quit()
        check(code == 0, f"clean exit (rc={code})")
        check("Detached" in farewell, "it says it detached", farewell)
        check("reattach" in farewell, "it says how to come back", farewell)
        check("still running" in farewell, "it says the engine survived", farewell)

        print("\n5. THE POINT: the engine outlived the terminal")
        check(probe(), "the daemon is still listening after the TUI exited")

        print("\n6. reattach and find the work")
        again = launch()
        screen = again.settle(15, quiet=1.0)
        screen.show("reattached")
        check(
            "Starting the engine" not in screen.text(),
            "attached without starting anything",
            screen.text(),
        )
        again.send(b"\x1b", 0.4)
        scrolled = again.send(b"k" * 30, 1.0)
        check(
            "explain the streaming" in scrolled.text(),
            "the earlier conversation is still there",
            screen.text(),
        )
        print("\n7. an idle viewport must cost nothing")
        # The design claim: the loop has no tick, so a viewport with nothing
        # happening does zero wakeups and writes zero bytes. Measured, not
        # asserted in a comment — a polling redraw loop would show up here
        # immediately.
        idle_seconds = 5.0
        before = cpu_seconds(again.proc.pid)
        time.sleep(idle_seconds)
        after = cpu_seconds(again.proc.pid)
        if before is None or after is None:
            print("  ~ could not read CPU time; skipping")
        else:
            used = after - before
            percent = 100 * used / idle_seconds
            check(
                used <= 0.10,
                f"idle CPU is negligible ({used:.2f}s over {idle_seconds:.0f}s ≈ {percent:.1f}%)",
                f"{used:.3f}s of CPU while idle suggests the loop is polling",
            )

        print("\n8. attaching with --no-spawn works when a daemon is up")
        result = subprocess.run(
            [BIN, "--probe", "--no-spawn"], env=ENV, capture_output=True, text=True
        )
        check(result.returncode == 0, "--probe --no-spawn sees the live daemon", result.stdout)

        code, farewell = again.quit()
        check(code == 0, f"clean exit on the second run (rc={code})")

    finally:
        if not keep:
            stop_daemon()

    print()
    if FAILURES:
        print(f"FAILED: {len(FAILURES)} check(s)")
        for failure in FAILURES:
            print(f"  - {failure}")
        return 1
    print("all checks passed")
    return 0


def daemon_pid() -> int | None:
    """The engine holding this data dir, from the lock file it stamps.

    Deliberately not `pkill -f 'comet headless'`: that would kill the
    developer's own daemon on the same machine. The lock file scopes the kill to
    the throwaway data dir this test created.
    """
    try:
        with open(os.path.join(DATA_DIR, "engine.lock")) as handle:
            return int(handle.read().strip())
    except (OSError, ValueError):
        return None


def stop_daemon() -> None:
    pid = daemon_pid()
    if pid is None:
        print("\nno daemon to clean up")
        return
    print(f"\ncleaning up the daemon we started (pid {pid})")
    try:
        os.kill(pid, 15)
    except ProcessLookupError:
        pass


def cpu_seconds(pid: int) -> float | None:
    """Cumulative CPU time of a process, in seconds."""
    result = subprocess.run(
        ["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True
    )
    raw = result.stdout.strip()
    if not raw:
        return None
    # macOS prints [DD-]HH:MM:SS.cc or MM:SS.cc; Linux prints HH:MM:SS.
    parts = raw.replace("-", ":").split(":")
    try:
        numbers = [float(p) for p in parts]
    except ValueError:
        return None
    total = 0.0
    for number in numbers:
        total = total * 60 + number
    return total


def rpc(method: str, params: str) -> str:
    """Call the daemon directly, the way scripts/dev-demo.sh seeds data."""
    result = subprocess.run(
        [
            "cargo",
            "run",
            "-q",
            "-p",
            "comet-rpc",
            "--example",
            "rpc_probe",
            "--",
            f"ws://127.0.0.1:{IPC}",
            method,
            params,
        ],
        cwd=ROOT,
        env=ENV,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SystemExit(f"rpc {method} failed: {result.stderr}")
    return result.stdout


def new_uuid() -> str:
    import uuid

    return str(uuid.uuid4())


if __name__ == "__main__":
    sys.exit(main())
