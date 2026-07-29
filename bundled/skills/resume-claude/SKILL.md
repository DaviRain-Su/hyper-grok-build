---
name: resume-claude
description: >
  Resume or continue work from a recent Claude Code session. Use when the user
  switched from Claude Code, says "continue from Claude" or "resume my Claude
  session", or names a Claude session by description, path, or native ID.
metadata:
  short-description: "Continue from a recent Claude Code session"
argument-hint: "[words describing the session | session id]"
---

Set `TOOL=claude`. Set
`SHARED_DIR="${SKILL_DIR}/../shared/resume-session"`. Read and follow
`${SHARED_DIR}/CORE.md`, using `$ARGUMENTS` unchanged as the optional session
reference.
