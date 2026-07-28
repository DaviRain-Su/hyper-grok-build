---
name: resume-codex
description: >
  Resume or continue work from a recent Codex CLI or Codex VS Code session. Use
  when the user switched from Codex, says "continue from Codex" or "resume my
  Codex session", or names a Codex session by description, path, or native ID.
metadata:
  short-description: "Continue from a recent Codex session"
argument-hint: "[words describing the session | session id]"
---

Set `TOOL=codex`. Set
`SHARED_DIR="${SKILL_DIR}/../shared/resume-session"`. Read and follow
`${SHARED_DIR}/CORE.md`, using `$ARGUMENTS` unchanged as the optional session
reference.
