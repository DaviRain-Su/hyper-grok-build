#!/usr/bin/env python3
"""Rewrite upstream (xai-org/grok-build) paths to Hyper packages/* layout.

Reads a unified diff / patch from stdin (or a file path argument) and writes
the rewritten patch to stdout.

Mapping source: docs/UPSTREAM_PATH_MAP.md (table rows) or the hardcoded
fallback embedded from scripts/crate-package-map.toml at reorg time.

Usage:
  git show upstream/main -- crates/codegen/foo | scripts/upstream-path-rewrite.py
  scripts/upstream-path-rewrite.py path/to/upstream.patch > local.patch
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAP_MD = ROOT / "docs" / "UPSTREAM_PATH_MAP.md"

ROW = re.compile(
    r"^\|\s*`(?P<old>[^`]+)`\s*\|\s*`(?P<new>[^`]+)`\s*\|\s*`(?P<pkg>[^`]+)`\s*\|"
)


def load_map() -> list[tuple[str, str]]:
    pairs: list[tuple[str, str]] = []
    if MAP_MD.is_file():
        for line in MAP_MD.read_text(encoding="utf-8").splitlines():
            m = ROW.match(line.strip())
            if m:
                pairs.append((m.group("old"), m.group("new")))
    if not pairs:
        raise SystemExit(f"no path map rows found in {MAP_MD}")
    # Longest first so crates/codegen/xai-foo matches before crates/codegen
    pairs.sort(key=lambda p: -len(p[0]))
    return pairs


def rewrite(text: str, pairs: list[tuple[str, str]]) -> str:
    for old, new in pairs:
        text = text.replace(old, new)
    return text


def main() -> None:
    pairs = load_map()
    if len(sys.argv) > 1:
        raw = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
    else:
        raw = sys.stdin.read()
    sys.stdout.write(rewrite(raw, pairs))


if __name__ == "__main__":
    main()
