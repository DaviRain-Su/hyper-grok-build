#!/usr/bin/env python3
"""Regression tests for the OMP reader in session_reader.py.

These tests exercise the OMP-specific behavior that must match
`crates/codegen/xai-grok-workspace/src/foreign_sessions/omp.rs` and the
official OMP append-only-tree session format:

* `_omp_profile` / `_omp_sessions_root` profile precedence and suppression.
* `_omp_head` streaming bounded reads.
* `read_omp_session` active-leaf ancestor chain selection (no abandoned
  branches), compaction folding, inert toolCall / toolResult preservation
  with `max_tool_chars`.
* discovery bounds.

They are hermetic: every test pins HOME / env and writes fixtures into a
tempdir, then restores the original environment on teardown.
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Any

SHARED_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SHARED_DIR))

import session_reader as sr  # noqa: E402


def _omp_record(
    *,
    type_: str,
    id_: str | None,
    parent_id: str | None,
    timestamp: str = "2026-07-27T00:00:00Z",
    **fields: Any,
) -> dict[str, Any]:
    record: dict[str, Any] = {"type": type_, "timestamp": timestamp}
    if id_ is not None:
        record["id"] = id_
    if parent_id is not None:
        record["parentId"] = parent_id
    record.update(fields)
    return record


def _write_jsonl(path: Path, records: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, ensure_ascii=False) + "\n")


class _EnvGuard:
    """Snapshot and restore os.environ + a temp HOME."""

    def __init__(self) -> None:
        self._snapshot: dict[str, str | None] = {}
        self._home: str | None = None
        self._cwd: str | None = None

    def __enter__(self) -> "_EnvGuard":
        for name in (
            "OMP_PROFILE",
            "PI_PROFILE",
            "PI_CODING_AGENT_DIR",
            "PI_CONFIG_DIR",
            "XDG_DATA_HOME",
            "HOME",
            "TMPDIR",
        ):
            self._snapshot[name] = os.environ.get(name)
        self._home = os.environ.get("HOME")
        self._cwd = os.getcwd()
        return self

    def __exit__(self, *exc: object) -> None:
        for name, value in self._snapshot.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
        if self._home is not None:
            os.environ["HOME"] = self._home
        os.chdir(self._cwd)

    def set(self, name: str, value: str) -> None:
        os.environ[name] = value

    def unset(self, name: str) -> None:
        os.environ.pop(name, None)


class OmpProfileRootTests(unittest.TestCase):
    """Requirement 1: profile/root precedence matches omp.rs."""

    def setUp(self) -> None:
        self._guard = _EnvGuard()
        self._guard.__enter__()
        self._tmp = tempfile.TemporaryDirectory()
        self.home = Path(self._tmp.name) / "home"
        self.home.mkdir()
        os.environ["HOME"] = str(self.home)
        os.chdir(self._tmp.name)
        # Pin config-sensitive vars to a clean baseline so real-env values
        # cannot leak into the assertions below.
        self._guard.unset("PI_CONFIG_DIR")
        self._guard.unset("XDG_DATA_HOME")

    def tearDown(self) -> None:
        self._tmp.cleanup()
        self._guard.__exit__(None, None, None)

    def test_omp_profile_wins_over_pi_profile(self) -> None:
        os.environ["OMP_PROFILE"] = "work"
        os.environ["PI_PROFILE"] = "other"
        self.assertEqual(sr._omp_profile(), "work")

    def test_omp_profile_empty_is_explicit_default_no_pi_fallback(self) -> None:
        os.environ["OMP_PROFILE"] = ""
        os.environ["PI_PROFILE"] = "work"
        # OMP_PROFILE present => explicit default; never fall back to PI_PROFILE.
        self.assertIsNone(sr._omp_profile())

    def test_omp_profile_default_value_is_explicit_default(self) -> None:
        os.environ["OMP_PROFILE"] = "default"
        os.environ["PI_PROFILE"] = "work"
        self.assertIsNone(sr._omp_profile())

    def test_omp_profile_unset_falls_back_to_pi_profile(self) -> None:
        self._guard.unset("OMP_PROFILE")
        os.environ["PI_PROFILE"] = "work"
        self.assertEqual(sr._omp_profile(), "work")

    def test_omp_profile_rejects_invalid_names(self) -> None:
        for value in ("", ".", "..", "Work", "-work", "work.", "x" * 65):
            self.assertFalse(sr._valid_omp_profile(value), value)
        for value in ("work", "work-2", "a.b_c-d", "1abc"):
            self.assertTrue(sr._valid_omp_profile(value), value)

    def test_pi_coding_agent_dir_used_without_profile(self) -> None:
        self._guard.unset("OMP_PROFILE")
        self._guard.unset("PI_PROFILE")
        self._guard.unset("XDG_DATA_HOME")
        agent = self.home / "agent-override"
        os.environ["PI_CODING_AGENT_DIR"] = str(agent)
        self.assertEqual(sr._omp_sessions_root(), agent / "sessions")

    def test_empty_pi_coding_agent_dir_does_not_short_circuit(self) -> None:
        self._guard.unset("OMP_PROFILE")
        self._guard.unset("PI_PROFILE")
        self._guard.unset("XDG_DATA_HOME")
        os.environ["PI_CODING_AGENT_DIR"] = ""
        root = sr._omp_sessions_root()
        self.assertNotEqual(root, Path("") / "sessions")
        self.assertTrue(root.is_absolute())

    def test_profile_suppresses_pi_coding_agent_dir(self) -> None:
        agent = self.home / "agent-override"
        os.environ["PI_CODING_AGENT_DIR"] = str(agent)
        os.environ["OMP_PROFILE"] = "work"
        self._guard.unset("PI_PROFILE")
        self._guard.unset("XDG_DATA_HOME")
        root = sr._omp_sessions_root()
        self.assertNotEqual(root, agent / "sessions")
        self.assertEqual(root, self.home / ".omp" / "profiles" / "work" / "agent" / "sessions")

    def test_inherited_profile_agent_dir_suppressed_for_explicit_default(self) -> None:
        config_name = ".omp-test"
        os.environ["PI_CONFIG_DIR"] = config_name
        os.environ["OMP_PROFILE"] = ""
        os.environ["PI_PROFILE"] = "work"
        inherited = self.home / config_name / "profiles" / "work" / "agent"
        os.environ["PI_CODING_AGENT_DIR"] = str(inherited)
        self._guard.unset("XDG_DATA_HOME")
        root = sr._omp_sessions_root()
        self.assertEqual(root, self.home / config_name / "agent" / "sessions")

    def test_non_default_omp_profile_keeps_agent_dir_suppressed(self) -> None:
        # A real (non-default) profile must also bypass PI_CODING_AGENT_DIR.
        agent = self.home / "agent-override"
        os.environ["PI_CODING_AGENT_DIR"] = str(agent)
        os.environ["OMP_PROFILE"] = "work"
        os.environ["PI_PROFILE"] = "other"
        self._guard.unset("XDG_DATA_HOME")
        root = sr._omp_sessions_root()
        self.assertNotEqual(root, agent / "sessions")

    def test_relative_pi_coding_agent_dir_resolves_from_cwd(self) -> None:
        self._guard.unset("OMP_PROFILE")
        self._guard.unset("PI_PROFILE")
        self._guard.unset("XDG_DATA_HOME")
        os.environ["PI_CODING_AGENT_DIR"] = "relative-agent"
        root = sr._omp_sessions_root()
        self.assertEqual(root, Path(self._tmp.name) / "relative-agent" / "sessions")

    def test_empty_xdg_data_home_rejected(self) -> None:
        self._guard.unset("OMP_PROFILE")
        self._guard.unset("PI_PROFILE")
        self._guard.unset("PI_CODING_AGENT_DIR")
        os.environ["XDG_DATA_HOME"] = ""
        root = sr._omp_sessions_root()
        self.assertEqual(root, self.home / ".omp" / "agent" / "sessions")

    def test_whitespace_xdg_data_home_rejected(self) -> None:
        self._guard.unset("OMP_PROFILE")
        self._guard.unset("PI_PROFILE")
        self._guard.unset("PI_CODING_AGENT_DIR")
        os.environ["XDG_DATA_HOME"] = "   "
        root = sr._omp_sessions_root()
        self.assertEqual(root, self.home / ".omp" / "agent" / "sessions")

    def test_xdg_used_when_omp_dir_exists(self) -> None:
        self._guard.unset("OMP_PROFILE")
        self._guard.unset("PI_PROFILE")
        self._guard.unset("PI_CODING_AGENT_DIR")
        xdg_root = self.home / "xdg" / "omp"
        xdg_root.mkdir(parents=True)
        os.environ["XDG_DATA_HOME"] = str(self.home / "xdg")
        if os.name == "nt":
            self.skipTest("XDG branch is posix-only")
        self.assertEqual(sr._omp_sessions_root(), xdg_root / "sessions")

    def test_xdg_missing_falls_back_to_config_root(self) -> None:
        self._guard.unset("OMP_PROFILE")
        self._guard.unset("PI_PROFILE")
        self._guard.unset("PI_CODING_AGENT_DIR")
        os.environ["XDG_DATA_HOME"] = str(self.home / "xdg-empty")
        os.environ["PI_CONFIG_DIR"] = ".omp-test"
        root = sr._omp_sessions_root()
        self.assertEqual(root, self.home / ".omp-test" / "agent" / "sessions")
        self.assertFalse(root.is_relative_to(self.home / "xdg-empty"))

    def test_xdg_embeds_profile(self) -> None:
        if os.name == "nt":
            self.skipTest("XDG branch is posix-only")
        self._guard.unset("PI_CODING_AGENT_DIR")
        self._guard.unset("PI_PROFILE")
        os.environ["OMP_PROFILE"] = "work"
        profile_root = self.home / "xdg" / "omp" / "profiles" / "work"
        profile_root.mkdir(parents=True)
        os.environ["XDG_DATA_HOME"] = str(self.home / "xdg")
        self.assertEqual(sr._omp_sessions_root(), profile_root / "sessions")


class OmpHeadTests(unittest.TestCase):
    """Requirement 4: _omp_head is a streaming bounded read."""

    def setUp(self) -> None:
        self._guard = _EnvGuard()
        self._guard.__enter__()
        self._tmp = tempfile.TemporaryDirectory()
        os.environ["HOME"] = self._tmp.name
        os.chdir(self._tmp.name)
        for name in ("OMP_PROFILE", "PI_PROFILE", "PI_CODING_AGENT_DIR",
                     "PI_CONFIG_DIR", "XDG_DATA_HOME"):
            self._guard.unset(name)
        self.cwd = str(Path(self._tmp.name) / "repo")
        Path(self.cwd).mkdir()

    def tearDown(self) -> None:
        self._tmp.cleanup()
        self._guard.__exit__(None, None, None)

    def _session_path(self, name: str = "abc12345.jsonl") -> Path:
        directories = sr._omp_session_directories(self.cwd)
        return directories[0] / name

    def test_head_reads_session_title_cwd_and_first_user(self) -> None:
        path = self._session_path()
        _write_jsonl(
            path,
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z", "title": "My title"},
                _omp_record(type_="message", id_="m1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "hi"}]}),
            ],
        )
        meta = sr._omp_head(path)
        self.assertIsNotNone(meta)
        assert meta is not None
        self.assertEqual(meta["session_id"], "native-id")
        self.assertEqual(meta["cwd"], self.cwd)
        self.assertEqual(meta["title"], "My title")
        self.assertEqual(meta["created_at"], "2026-07-27T00:00:00Z")

    def test_head_missing_session_header_returns_none(self) -> None:
        path = self._session_path()
        _write_jsonl(path, [_omp_record(type_="message", id_="m1", parent_id=None,
                                        message={"role": "user", "content": "hi"})])
        self.assertIsNone(sr._omp_head(path))

    def test_head_missing_cwd_returns_none(self) -> None:
        path = self._session_path()
        _write_jsonl(path, [{"type": "session", "version": 3, "id": "x"}])
        self.assertIsNone(sr._omp_head(path))

    def test_head_uses_short_summary_when_no_title(self) -> None:
        path = self._session_path()
        _write_jsonl(
            path,
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                {"type": "compaction", "id": "c1", "parentId": None,
                 "shortSummary": "short recap"},
            ],
        )
        meta = sr._omp_head(path)
        assert meta is not None
        self.assertEqual(meta["title"], "short recap")

    def test_head_uses_first_user_when_no_title_or_summary(self) -> None:
        path = self._session_path()
        _write_jsonl(
            path,
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="m1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "first request"}]}),
            ],
        )
        meta = sr._omp_head(path)
        assert meta is not None
        self.assertEqual(meta["title"], "first request")

    def test_head_is_bounded_by_max_records(self) -> None:
        path = self._session_path()
        # The session header must be within the first MAX_RECORDS lines; place
        # it past the bound to confirm the head stops reading and returns None.
        padding = [{"type": "label", "id": f"l{i}", "parentId": None} for i in range(sr._OMP_HEAD_MAX_RECORDS)]
        _write_jsonl(
            path,
            padding
            + [{"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                "timestamp": "2026-07-27T00:00:00Z"}],
        )
        self.assertIsNone(sr._omp_head(path))

    def test_head_is_bounded_by_max_bytes(self) -> None:
        path = self._session_path()
        # One very long line that exceeds the byte budget before the header.
        long_line = {"type": "label", "id": "big", "parentId": None,
                     "blob": "x" * (sr._OMP_HEAD_MAX_BYTES + 1024)}
        _write_jsonl(
            path,
            [long_line]
            + [{"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                "timestamp": "2026-07-27T00:00:00Z"}],
        )
        self.assertIsNone(sr._omp_head(path))

    def test_head_counts_malformed_lines(self) -> None:
        path = self._session_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("w", encoding="utf-8") as handle:
            handle.write("{not json\n")
            handle.write(json.dumps({"type": "session", "version": 3, "id": "native-id",
                                     "cwd": self.cwd, "timestamp": "2026-07-27T00:00:00Z"}) + "\n")
        meta = sr._omp_head(path)
        assert meta is not None
        self.assertEqual(meta["malformed"], 1)

    def test_head_does_not_load_full_file(self) -> None:
        path = self._session_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        # A header in the bounded window plus a huge tail that must NOT be read.
        header = {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                  "timestamp": "2026-07-27T00:00:00Z", "title": "head-only"}
        with path.open("w", encoding="utf-8") as handle:
            handle.write(json.dumps(header) + "\n")
            handle.write("x" * (sr._OMP_HEAD_MAX_BYTES * 4) + "\n")
        meta = sr._omp_head(path)
        assert meta is not None
        self.assertEqual(meta["title"], "head-only")


class OmpReadSessionTests(unittest.TestCase):
    """Requirements 2 & 3: active-leaf chain + inert tool preservation."""

    def setUp(self) -> None:
        self._guard = _EnvGuard()
        self._guard.__enter__()
        self._tmp = tempfile.TemporaryDirectory()
        os.environ["HOME"] = self._tmp.name
        os.chdir(self._tmp.name)
        for name in ("OMP_PROFILE", "PI_PROFILE", "PI_CODING_AGENT_DIR",
                     "PI_CONFIG_DIR", "XDG_DATA_HOME"):
            self._guard.unset(name)
        self.cwd = str(Path(self._tmp.name) / "repo")
        Path(self.cwd).mkdir()

    def tearDown(self) -> None:
        self._tmp.cleanup()
        self._guard.__exit__(None, None, None)

    def _session_path(self, name: str = "abc12345.jsonl") -> Path:
        return sr._omp_session_directories(self.cwd)[0] / name

    def _read(self, records: list[dict[str, Any]], max_tool_chars: int = 300) -> dict[str, Any]:
        path = self._session_path()
        _write_jsonl(path, records)
        return sr.read_omp_session(path, max_tool_chars=max_tool_chars)

    def test_linear_chain_preserved_in_order(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "hello"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [{"type": "text", "text": "hi there"}]}),
            ]
        )
        roles = [turn["role"] for turn in result["turns"]]
        texts = [turn["text"] for turn in result["turns"]]
        self.assertEqual(roles, ["user", "assistant"])
        self.assertEqual(texts, ["hello", "hi there"])
        self.assertTrue(all(turn["inert"] for turn in result["turns"]))

    def test_abandoned_branch_excluded_from_active_chain(self) -> None:
        # Build a tree: root user -> assistant A -> tool result -> user edit
        # AND a sibling abandoned assistant B (different branch) that is NOT
        # an ancestor of the final leaf.
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "do work"}]}),
                # Abandoned branch: assistant B under u1 (not the leaf path).
                _omp_record(type_="message", id_="a-abandoned", parent_id="u1",
                            message={"role": "assistant",
                                     "content": [{"type": "text", "text": "abandoned reply"}]}),
                # Active branch: assistant A under u1.
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant",
                                     "content": [{"type": "text", "text": "active reply"}]}),
                # Final leaf under a1.
                _omp_record(type_="message", id_="u2", parent_id="a1",
                            message={"role": "user", "content": [{"type": "text", "text": "edit"}]}),
            ]
        )
        texts = [turn["text"] for turn in result["turns"]]
        self.assertIn("do work", texts)
        self.assertIn("active reply", texts)
        self.assertIn("edit", texts)
        self.assertNotIn("abandoned reply", texts)

    def test_last_appended_entry_is_the_leaf(self) -> None:
        # Even with an abandoned branch appended AFTER the active branch's
        # continuation, the last-appended entry defines the active leaf only
        # when it lies on the active path. Here the last entry is on the
        # active path so it becomes the leaf.
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "root"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [{"type": "text", "text": "a1"}]}),
                # An abandoned sibling under u1 appended AFTER a1.
                _omp_record(type_="message", id_="a2-abandoned", parent_id="u1",
                            message={"role": "assistant", "content": [{"type": "text", "text": "a2-abandoned"}]}),
                # The active leaf is the last-appended entry, on a1's branch.
                _omp_record(type_="message", id_="u2", parent_id="a1",
                            message={"role": "user", "content": [{"type": "text", "text": "u2"}]}),
            ]
        )
        texts = [turn["text"] for turn in result["turns"]]
        self.assertEqual(texts, ["root", "a1", "u2"])
        self.assertNotIn("a2-abandoned", texts)

    def test_assistant_tool_call_preserved_inert(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "read file"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [
                                {"type": "text", "text": "Reading."},
                                {"type": "toolCall", "id": "toolu_01", "name": "read",
                                 "arguments": {"path": "src/a.ts"}},
                            ]}),
            ]
        )
        assistant = next(t for t in result["turns"] if t["role"] == "assistant")
        self.assertEqual(assistant["tool_calls"], [
            {"id": "toolu_01", "name": "read",
             "input": sr._json_preview({"path": "src/a.ts"}, 300), "inert": True},
        ])
        self.assertTrue(assistant["inert"])

    def test_tool_result_message_preserved_inert(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "read file"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [
                                {"type": "toolCall", "id": "toolu_01", "name": "read",
                                 "arguments": {"path": "src/a.ts"}}]}),
                _omp_record(type_="message", id_="tr1", parent_id="a1",
                            message={"role": "toolResult", "toolCallId": "toolu_01",
                                     "toolName": "read",
                                     "content": [{"type": "text", "text": "file contents here"}],
                                     "isError": False}),
            ]
        )
        tool_turn = next(t for t in result["turns"] if t["role"] == "tool")
        self.assertEqual(len(tool_turn["tool_results"]), 1)
        result_entry = tool_turn["tool_results"][0]
        self.assertEqual(result_entry["tool_use_id"], "toolu_01")
        self.assertEqual(result_entry["is_error"], False)
        self.assertEqual(result_entry["inert"], True)
        self.assertIn("file contents here", result_entry["content"])

    def test_tool_result_error_flag_preserved(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "go"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [
                                {"type": "toolCall", "id": "toolu_01", "name": "run", "arguments": {}}]}),
                _omp_record(type_="message", id_="tr1", parent_id="a1",
                            message={"role": "toolResult", "toolCallId": "toolu_01",
                                     "toolName": "run",
                                     "content": [{"type": "text", "text": "boom"}],
                                     "isError": True}),
            ]
        )
        tool_turn = next(t for t in result["turns"] if t["role"] == "tool")
        self.assertTrue(tool_turn["tool_results"][0]["is_error"])

    def test_max_tool_chars_truncates_tool_input_and_result(self) -> None:
        big = "x" * 500
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "go"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [
                                {"type": "toolCall", "id": "t1", "name": "write",
                                 "arguments": {"data": big}}]}),
                _omp_record(type_="message", id_="tr1", parent_id="a1",
                            message={"role": "toolResult", "toolCallId": "t1", "toolName": "write",
                                     "content": [{"type": "text", "text": big}], "isError": False}),
            ],
            max_tool_chars=20,
        )
        assistant = next(t for t in result["turns"] if t["role"] == "assistant")
        tool_turn = next(t for t in result["turns"] if t["role"] == "tool")
        self.assertLessEqual(len(assistant["tool_calls"][0]["input"]), 23)  # 20 + "..."
        self.assertLessEqual(len(tool_turn["tool_results"][0]["content"]), 23)

    def test_thinking_blocks_dropped(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "go"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [
                                {"type": "thinking", "thinking": "secret reasoning"},
                                {"type": "text", "text": "answer"}]}),
            ]
        )
        assistant = next(t for t in result["turns"] if t["role"] == "assistant")
        self.assertEqual(assistant["text"], "answer")
        self.assertNotIn("secret reasoning", assistant["text"])

    def test_compaction_folds_history_into_summary(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "old question"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [{"type": "text", "text": "old answer"}]}),
                # Compaction: replaces u1..a1 with a summary; keeps from a1 onward.
                {"type": "compaction", "id": "c1", "parentId": "a1",
                 "timestamp": "2026-07-27T00:00:05Z",
                 "summary": "Prior work summarized", "shortSummary": "recap",
                 "firstKeptEntryId": "a1"},
                _omp_record(type_="message", id_="u2", parent_id="c1",
                            message={"role": "user", "content": [{"type": "text", "text": "new question"}]}),
            ]
        )
        texts = [turn["text"] for turn in result["turns"]]
        # The pre-compaction "old question" must be dropped (replaced by summary).
        self.assertNotIn("old question", texts)
        # The compaction summary appears as a context turn.
        self.assertIn("Prior work summarized", " ".join(texts))
        # The kept tail (a1) and the post-compaction entry survive.
        self.assertIn("old answer", texts)
        self.assertIn("new question", texts)

    def test_compaction_missing_first_kept_warns(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "old"}]}),
                {"type": "compaction", "id": "c1", "parentId": "u1",
                 "timestamp": "2026-07-27T00:00:05Z",
                 "summary": "summary", "firstKeptEntryId": "missing-id"},
                _omp_record(type_="message", id_="u2", parent_id="c1",
                            message={"role": "user", "content": [{"type": "text", "text": "new"}]}),
            ]
        )
        codes = {w["code"] for w in result["warnings"]}
        self.assertIn("missing_compaction_tail", codes)

    def test_parent_cycle_detected_and_warned(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="a", parent_id="b",
                            message={"role": "user", "content": [{"type": "text", "text": "a"}]}),
                _omp_record(type_="message", id_="b", parent_id="a",
                            message={"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
            ]
        )
        codes = {w["code"] for w in result["warnings"]}
        self.assertIn("cyclic_parent_chain", codes)

    def test_missing_parent_warns(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="a", parent_id="ghost",
                            message={"role": "user", "content": [{"type": "text", "text": "a"}]}),
            ]
        )
        codes = {w["code"] for w in result["warnings"]}
        self.assertIn("missing_parent", codes)

    def test_unknown_record_types_counted(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "go"}]}),
                {"type": "totally_unknown_type", "id": "x", "parentId": "u1"},
            ]
        )
        codes = {w["code"] for w in result["warnings"]}
        self.assertIn("unknown_records", codes)

    def test_no_session_header_raises(self) -> None:
        path = self._session_path()
        _write_jsonl(path, [_omp_record(type_="message", id_="u1", parent_id=None,
                                        message={"role": "user", "content": "hi"})])
        with self.assertRaises(sr.ReaderError):
            sr.read_omp_session(path)

    def test_malformed_records_warned(self) -> None:
        path = self._session_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("w", encoding="utf-8") as handle:
            handle.write("{bad json\n")
            handle.write(json.dumps({"type": "session", "version": 3, "id": "native-id",
                                     "cwd": self.cwd, "timestamp": "2026-07-27T00:00:00Z"}) + "\n")
            handle.write(json.dumps(_omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "hi"}]})) + "\n")
        result = sr.read_omp_session(path)
        codes = {w["code"] for w in result["warnings"]}
        self.assertIn("malformed_records", codes)

    def test_tool_calls_never_execute(self) -> None:
        # The reader must never run the recorded tool; it only captures args.
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "rm"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [
                                {"type": "toolCall", "id": "t1", "name": "shell",
                                 "arguments": {"cmd": "rm -rf /"}}]}),
            ]
        )
        assistant = next(t for t in result["turns"] if t["role"] == "assistant")
        self.assertTrue(assistant["tool_calls"][0]["inert"])
        self.assertEqual(assistant["tool_calls"][0]["name"], "shell")
        # No tool_results were fabricated.
        self.assertEqual(assistant["tool_results"], [])

    def test_last_user_request_and_assistant_action_derived(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "first"}]}),
                _omp_record(type_="message", id_="a1", parent_id="u1",
                            message={"role": "assistant", "content": [{"type": "text", "text": "ok"}]}),
                _omp_record(type_="message", id_="u2", parent_id="a1",
                            message={"role": "user", "content": [{"type": "text", "text": "second request"}]}),
            ]
        )
        self.assertEqual(result["last_user_request"], "second request")
        self.assertEqual(result["last_assistant_action"], "ok")

    def test_result_shape(self) -> None:
        result = self._read(
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z", "title": "the title"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "hi"}]}),
            ]
        )
        self.assertEqual(result["tool"], "omp")
        self.assertEqual(result["source"], "omp-cli")
        self.assertEqual(result["session_id"], "native-id")
        self.assertEqual(result["cwd"], self.cwd)
        self.assertEqual(result["title"], "the title")
        self.assertEqual(result["created_at"], "2026-07-27T00:00:00Z")
        for field in ("branch", "source_repo_root_path"):
            self.assertIsNone(result[field])
        self.assertIsInstance(result["turns"], list)
        self.assertIsInstance(result["warnings"], list)


class OmpDirectoryNamesTests(unittest.TestCase):
    """Directory-name encoding matches omp.rs current/legacy layouts."""

    def setUp(self) -> None:
        self._guard = _EnvGuard()
        self._guard.__enter__()
        self._tmp = tempfile.TemporaryDirectory()
        home = Path(self._tmp.name) / "home"
        home.mkdir()
        os.environ["HOME"] = str(home)
        os.chdir(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()
        self._guard.__exit__(None, None, None)

    def test_home_relative_encoding(self) -> None:
        home = Path(os.environ["HOME"])
        cwd = str(home / "Projects" / "repo")
        names = [p.name for p in sr._omp_session_directories(cwd)]
        self.assertIn("-Projects-repo", names)

    def test_legacy_absolute_encoding_included(self) -> None:
        cwd = "/workspace/repo"
        names = [p.name for p in sr._omp_session_directories(cwd)]
        self.assertTrue(any(name.startswith("--") and name.endswith("--") for name in names),
                        names)


class OmpDiscoveryTests(unittest.TestCase):
    """Requirement 4: discovery has reasonable upper bounds."""

    def setUp(self) -> None:
        self._guard = _EnvGuard()
        self._guard.__enter__()
        self._tmp = tempfile.TemporaryDirectory()
        home = Path(self._tmp.name) / "home"
        home.mkdir()
        os.environ["HOME"] = str(home)
        os.chdir(self._tmp.name)
        self.cwd = str(Path(self._tmp.name) / "repo")
        Path(self.cwd).mkdir()
        # Pin sessions root at an isolated location under the tempdir.
        os.environ["PI_CODING_AGENT_DIR"] = str(Path(self._tmp.name) / "agent")
        self._guard.unset("OMP_PROFILE")
        self._guard.unset("PI_PROFILE")
        self._guard.unset("XDG_DATA_HOME")

    def tearDown(self) -> None:
        self._tmp.cleanup()
        self._guard.__exit__(None, None, None)

    def _bucket(self) -> Path:
        return sr._omp_session_directories(self.cwd)[0]

    def test_discovery_returns_matching_sessions(self) -> None:
        path = self._bucket() / "sess-one.jsonl"
        _write_jsonl(
            path,
            [
                {"type": "session", "version": 3, "id": "sess-one", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z", "title": "One"},
                _omp_record(type_="message", id_="m1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "hi"}]}),
            ],
        )
        sessions = sr._discover_omp(self.cwd, within_min=0)
        self.assertEqual(len(sessions), 1)
        self.assertEqual(sessions[0]["session_id"], "sess-one")
        self.assertEqual(sessions[0]["tool"], "omp")
        self.assertEqual(sessions[0]["source"], "omp-cli")
        self.assertEqual(sessions[0]["cwd"], self.cwd)

    def test_discovery_filters_other_cwd(self) -> None:
        other_cwd = str(Path(self._tmp.name) / "other")
        Path(other_cwd).mkdir()
        path = self._bucket() / "sess-other.jsonl"
        _write_jsonl(
            path,
            [
                {"type": "session", "version": 3, "id": "sess-other", "cwd": other_cwd,
                 "timestamp": "2026-07-27T00:00:00Z", "title": "Other"},
            ],
        )
        sessions = sr._discover_omp(self.cwd, within_min=0)
        self.assertEqual(sessions, [])

    def test_discovery_respects_within_min(self) -> None:
        path = self._bucket() / "old.jsonl"
        _write_jsonl(
            path,
            [{"type": "session", "version": 3, "id": "old", "cwd": self.cwd,
              "timestamp": "2026-07-27T00:00:00Z"}],
        )
        # Backdate the file by ~30 minutes; within_min=1 must exclude it.
        old_mtime = time.time() - 30 * 60
        os.utime(path, (old_mtime, old_mtime))
        self.assertEqual(sr._discover_omp(self.cwd, within_min=1), [])
        self.assertEqual(len(sr._discover_omp(self.cwd, within_min=0)), 1)

    def test_discovery_bounded_by_max_metadata_reads(self) -> None:
        bucket = self._bucket()
        bucket.mkdir(parents=True, exist_ok=True)
        for i in range(sr._OMP_MAX_METADATA_READS + 5):
            _write_jsonl(
                bucket / f"sess-{i:04d}.jsonl",
                [{"type": "session", "version": 3, "id": f"sess-{i:04d}", "cwd": self.cwd,
                  "timestamp": "2026-07-27T00:00:00Z"}],
            )
        sessions = sr._discover_omp(self.cwd, within_min=0)
        self.assertLessEqual(len(sessions), sr._OMP_MAX_METADATA_READS)

    def test_discovery_skips_non_jsonl_and_symlinks(self) -> None:
        bucket = self._bucket()
        bucket.mkdir(parents=True, exist_ok=True)
        (bucket / "not-a-session.txt").write_text("nope")
        _write_jsonl(
            bucket / "good.jsonl",
            [{"type": "session", "version": 3, "id": "good", "cwd": self.cwd,
              "timestamp": "2026-07-27T00:00:00Z"}],
        )
        sessions = sr._discover_omp(self.cwd, within_min=0)
        self.assertEqual([s["session_id"] for s in sessions], ["good"])


class OmpResolveTests(unittest.TestCase):
    """End-to-end resolve + read via the public API."""

    def setUp(self) -> None:
        self._guard = _EnvGuard()
        self._guard.__enter__()
        self._tmp = tempfile.TemporaryDirectory()
        home = Path(self._tmp.name) / "home"
        home.mkdir()
        os.environ["HOME"] = str(home)
        os.chdir(self._tmp.name)
        self.cwd = str(Path(self._tmp.name) / "repo")
        Path(self.cwd).mkdir()
        os.environ["PI_CODING_AGENT_DIR"] = str(Path(self._tmp.name) / "agent")
        self._guard.unset("OMP_PROFILE")
        self._guard.unset("PI_PROFILE")
        self._guard.unset("XDG_DATA_HOME")

    def tearDown(self) -> None:
        self._tmp.cleanup()
        self._guard.__exit__(None, None, None)

    def test_resolve_latest_reads_session(self) -> None:
        bucket = sr._omp_session_directories(self.cwd)[0]
        _write_jsonl(
            bucket / "native-id.jsonl",
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
                _omp_record(type_="message", id_="u1", parent_id=None,
                            message={"role": "user", "content": [{"type": "text", "text": "hi"}]}),
            ],
        )
        candidate = sr.resolve_session("omp", "latest", self.cwd)
        result = sr.read_resolved_session(candidate)
        self.assertEqual(result["session_id"], "native-id")
        self.assertEqual([t["text"] for t in result["turns"]], ["hi"])

    def test_resolve_by_native_id(self) -> None:
        bucket = sr._omp_session_directories(self.cwd)[0]
        _write_jsonl(
            bucket / "native-id.jsonl",
            [
                {"type": "session", "version": 3, "id": "native-id", "cwd": self.cwd,
                 "timestamp": "2026-07-27T00:00:00Z"},
            ],
        )
        candidate = sr.resolve_session("omp", "native-id", self.cwd)
        self.assertEqual(candidate["session_id"], "native-id")


if __name__ == "__main__":
    unittest.main()