---
name: resume-cursor
description: >
  Resume or continue work from a recent Cursor CLI or Cursor Desktop session.
  Use when the user switched from Cursor, says "continue from Cursor" or
  "resume my Cursor session", or names a Cursor session by description, path,
  or native ID.
metadata:
  short-description: "Continue from a recent Cursor session"
argument-hint: "[words describing the session | session id]"
---

Set `TOOL=cursor`. Set
`SHARED_DIR="${SKILL_DIR}/../shared/resume-session"`. Read and follow
`${SHARED_DIR}/CORE.md`, using `$ARGUMENTS` unchanged as the optional session
reference.
