"""Focused OMP compatibility tests for the bundled foreign-session reader."""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path
from unittest import mock

MODULE_PATH = (
    Path(__file__).resolve().parent.parent
    / "bundled"
    / "skills"
    / "shared"
    / "resume-session"
    / "session_reader.py"
)
SPEC = importlib.util.spec_from_file_location("resume_session_reader", MODULE_PATH)
assert SPEC and SPEC.loader
reader = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = reader
SPEC.loader.exec_module(reader)


@contextmanager
def environment(**values: str | None):
    patch = {key: value for key, value in values.items() if value is not None}
    removals = [key for key, value in values.items() if value is None]
    with mock.patch.dict(os.environ, patch, clear=False):
        saved = {key: os.environ.pop(key, None) for key in removals}
        try:
            yield
        finally:
            for key, value in saved.items():
                if value is not None:
                    os.environ[key] = value


class OmpReaderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="omp-reader-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.session = self.root / "session.jsonl"

    def _write(self, *records: dict) -> None:
        self.session.write_text(
            "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
        )

    @staticmethod
    def _header() -> dict:
        return {
            "type": "session",
            "version": 3,
            "id": "session-id",
            "timestamp": "2026-07-27T00:00:00Z",
            "cwd": "/work/project",
        }

    @staticmethod
    def _message(entry_id: str, parent: str | None, role: str, content, **extra) -> dict:
        return {
            "type": "message",
            "id": entry_id,
            "parentId": parent,
            "timestamp": "2026-07-27T00:00:01Z",
            "message": {"role": role, "content": content, **extra},
        }

    def test_linear_v1_session_without_entry_ids_uses_file_order(self) -> None:
        header = self._header()
        header["version"] = 1
        self._write(
            header,
            {"type": "message", "message": {"role": "user", "content": "legacy request"}},
            {"type": "message", "message": {"role": "assistant", "content": "legacy answer"}},
        )
        result = reader.read_omp_session(self.session)
        self.assertEqual([turn["text"] for turn in result["turns"]], ["legacy request", "legacy answer"])

    def test_only_active_leaf_parent_chain_is_restored(self) -> None:
        self._write(
            self._header(),
            self._message("u1", None, "user", "start"),
            self._message("old", "u1", "assistant", "abandoned answer"),
            self._message("new", "u1", "user", "new branch request"),
            self._message("leaf", "new", "assistant", "active answer"),
        )
        result = reader.read_omp_session(self.session)
        self.assertEqual([turn["text"] for turn in result["turns"]], ["start", "new branch request", "active answer"])
        self.assertNotIn("abandoned answer", json.dumps(result))

    def test_tool_calls_and_results_are_preserved_and_truncated(self) -> None:
        self._write(
            self._header(),
            self._message("u1", None, "user", "run tests"),
            self._message(
                "a1",
                "u1",
                "assistant",
                [{"type": "toolCall", "id": "call-1", "name": "bash", "arguments": {"command": "pytest --verbose"}}],
            ),
            self._message(
                "t1",
                "a1",
                "toolResult",
                [{"type": "text", "text": "abcdefghijklmnopqrstuvwxyz"}],
                toolCallId="call-1",
                toolName="bash",
                isError=False,
            ),
        )
        result = reader.read_omp_session(self.session, max_tool_chars=12)
        self.assertEqual(result["turns"][1]["tool_calls"][0]["name"], "bash")
        tool_result = result["turns"][2]["tool_results"][0]
        self.assertEqual(tool_result["tool_use_id"], "call-1")
        self.assertEqual(tool_result["content"], "abcdefghijkl...")
        self.assertEqual(result["last_assistant_action"], "called inert foreign tool(s): bash")

    def test_latest_compaction_summary_and_kept_tail_form_context(self) -> None:
        self._write(
            self._header(),
            self._message("old", None, "user", "old request"),
            self._message("kept", "old", "user", "kept request"),
            {
                "type": "compaction",
                "id": "compact",
                "parentId": "kept",
                "timestamp": "2026-07-27T00:00:02Z",
                "summary": "Earlier work summary",
                "firstKeptEntryId": "kept",
            },
            self._message("leaf", "compact", "assistant", "after compact"),
        )
        result = reader.read_omp_session(self.session)
        self.assertEqual(
            [(turn["role"], turn["text"]) for turn in result["turns"]],
            [
                ("context", "[OMP compaction summary]\nEarlier work summary"),
                ("user", "kept request"),
                ("assistant", "after compact"),
            ],
        )
        self.assertEqual(result["last_user_request"], "kept request")

    def test_profile_normalization_matches_native_scanner(self) -> None:
        with mock.patch.object(reader.Path, "home", return_value=self.root / "home"):
            with environment(OMP_PROFILE=" default ", PI_PROFILE="work", PI_CODING_AGENT_DIR=None, PI_CONFIG_DIR=None, XDG_DATA_HOME=None):
                self.assertEqual(reader._omp_sessions_root(), self.root / "home" / ".omp" / "agent" / "sessions")
            with environment(OMP_PROFILE="", PI_PROFILE="work", PI_CODING_AGENT_DIR=None, PI_CONFIG_DIR=None, XDG_DATA_HOME=None):
                self.assertEqual(reader._omp_sessions_root(), self.root / "home" / ".omp" / "agent" / "sessions")
            with environment(OMP_PROFILE=None, PI_PROFILE="work", PI_CODING_AGENT_DIR=None, PI_CONFIG_DIR=None, XDG_DATA_HOME=None):
                self.assertEqual(reader._omp_sessions_root(), self.root / "home" / ".omp" / "profiles" / "work" / "agent" / "sessions")
            with environment(OMP_PROFILE="../escape", PI_PROFILE=None, PI_CODING_AGENT_DIR=None, PI_CONFIG_DIR=None, XDG_DATA_HOME=None):
                self.assertEqual(reader._omp_sessions_root(), self.root / "home" / ".omp" / "agent" / "sessions")

    def test_explicit_default_suppresses_inherited_profile_agent_dir(self) -> None:
        home = self.root / "home"
        inherited = home / ".omp" / "profiles" / "work" / "agent"
        with mock.patch.object(reader.Path, "home", return_value=home):
            with environment(
                OMP_PROFILE="default",
                PI_PROFILE="work",
                PI_CODING_AGENT_DIR=str(inherited),
                PI_CONFIG_DIR=None,
                XDG_DATA_HOME=None,
            ):
                self.assertEqual(reader._omp_sessions_root(), home / ".omp" / "agent" / "sessions")

    def test_relative_agent_dir_resolves_from_current_directory(self) -> None:
        with mock.patch.object(reader.Path, "home", return_value=self.root / "home"):
            with mock.patch.object(reader.Path, "cwd", return_value=self.root / "cwd"):
                with environment(
                    OMP_PROFILE=None,
                    PI_PROFILE=None,
                    PI_CODING_AGENT_DIR="relative-agent",
                    PI_CONFIG_DIR=None,
                    XDG_DATA_HOME=None,
                ):
                    self.assertEqual(
                        reader._omp_sessions_root(),
                        self.root / "cwd" / "relative-agent" / "sessions",
                    )

    def test_root_environment_paths_keep_literal_tilde(self) -> None:
        with mock.patch.object(reader.Path, "home", return_value=self.root / "home"):
            with mock.patch.object(reader.Path, "cwd", return_value=self.root / "cwd"):
                with environment(
                    OMP_PROFILE=None,
                    PI_PROFILE=None,
                    PI_CODING_AGENT_DIR="~/literal-agent",
                    PI_CONFIG_DIR=None,
                    XDG_DATA_HOME=None,
                ):
                    self.assertEqual(
                        reader._omp_sessions_root(),
                        self.root / "cwd" / "~" / "literal-agent" / "sessions",
                    )

    def test_xdg_root_keeps_literal_tilde_on_unix(self) -> None:
        home = self.root / "home"
        with mock.patch.object(reader.Path, "home", return_value=home):
            with mock.patch.object(reader.sys, "platform", "linux"):
                with mock.patch.object(reader.Path, "is_dir", return_value=True):
                    with environment(
                        OMP_PROFILE=None,
                        PI_PROFILE=None,
                        PI_CODING_AGENT_DIR=None,
                        PI_CONFIG_DIR=None,
                        XDG_DATA_HOME="~/literal-xdg",
                    ):
                        self.assertEqual(reader._omp_sessions_root(), Path("~/literal-xdg/omp/sessions"))

    def test_xdg_root_is_ignored_on_windows(self) -> None:
        home = self.root / "home"
        xdg = self.root / "xdg"
        (xdg / "omp").mkdir(parents=True)
        with mock.patch.object(reader.Path, "home", return_value=home):
            with mock.patch.object(reader.sys, "platform", "win32"):
                with environment(
                    OMP_PROFILE=None,
                    PI_PROFILE=None,
                    PI_CODING_AGENT_DIR=None,
                    PI_CONFIG_DIR=None,
                    XDG_DATA_HOME=str(xdg),
                ):
                    self.assertEqual(reader._omp_sessions_root(), home / ".omp" / "agent" / "sessions")


if __name__ == "__main__":
    unittest.main()
