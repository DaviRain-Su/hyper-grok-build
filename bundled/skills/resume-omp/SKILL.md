---
name: resume-omp
description: >
  Resume or continue work from a recent OMP session. Use when the user switched
  from OMP, says "continue from OMP" or "resume my OMP session", or names an
  OMP session by description, path, or native ID.
metadata:
  short-description: "Continue from a recent OMP session"
argument-hint: "[words describing the session | session id]"
---

Set `TOOL=omp`. Set
`SHARED_DIR="${SKILL_DIR}/../shared/resume-session"`. Read and follow
`${SHARED_DIR}/CORE.md`, using `$ARGUMENTS` unchanged as the optional session
reference.
