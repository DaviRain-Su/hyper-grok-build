# Hyper known issues

Living list of fork-specific gaps, fixed items, and intentional limits.
Update this file when closing an issue or shipping a release.

Last reviewed: 2026-07-22 (post S1: sticky OAuth refresh + child oracle text).

## Open

| ID | Severity | Topic | Notes |
|----|----------|--------|--------|
| F-8 | low (UX) | Bare logout is xAI-only | `hyper logout` clears the xAI session; Kimi/Codex need `hyper logout --kimi` / `--openai` or `/logout provider …`. Documented; optional `--all` not implemented. |
| Modes | design-only | Amp-style low–ultra agent modes | See [design-modes.md](./design-modes.md) — **not implemented**. |

## Fixed in tree

### S0 — coexistence / branding / Messages URLs

| ID | Topic | Fix |
|----|--------|-----|
| F-1 | `is_grok_process` ignored `hyper` | Recognizes basenames `hyper` / `grok`, `xai-grok-*` / `xai_grok_*` test bins, and `~/.hyper/bin` / `~/.grok/bin` paths. |
| F-2 | MiniMax / Fireworks Messages 404 | Messages `base_url_override` values are normalized to end in `/v1` before the sampler joins `/messages`. |
| F-3 | Branding | `community-build` (default on the Hyper binary) makes `--version` and `completions` emit `hyper`. |
| F-9 | Local builds without community-build | `xai-grok-pager-bin` defaults include `community-build`. |

### S1 — OAuth refresh storms + oracle discoverability

| ID | Topic | Fix |
|----|--------|-----|
| F-4 | Kimi lock-held refresh vs 45s follower | Entire Kimi refresh retry loop is capped at **40s** (`REFRESH_TOTAL_BUDGET_SECS`), below the 45s flock wait. |
| F-5 | Kimi/Codex sticky permanent-failure | Process-local sticky cache keyed by RT fingerprint; 401/`invalid_grant` short-circuits force-refresh for 5 minutes; cleared on login/logout/successful refresh. |
| F-7 | Child Task tool text omitted `oracle` | Nested `CHILD_TASK_DESCRIPTION` and `TaskToolInput` schema list `oracle`. |

## Intentional / accepted

| Topic | Behavior |
|--------|----------|
| Shared `~/.grok` | Config, auth, sessions, and leader IPC live under the upstream home. Binary install root is `~/.hyper`. |
| Shared Kimi + Codex proxy | Catalog id (`kimi-code/*` vs `openai-codex/*`) selects credentials; ambiguous URL alone does not guess a family. |
| Hyper Modes | Design doc only until implemented. |
| Sticky refresh cache | In-process only (not shared across processes); multi-process still uses flock + compare/adopt. |

## Coexistence with official `grok`

- Different binaries: `hyper` vs `grok`.
- Shared runtime state under `~/.grok` (including `leader*.sock` / `leader*.lock`).
- Prefer `hyper leader kill` / `grok leader kill` only against leaders you own; both binaries recognize the other product process by name when cleaning locks.
- Community builds never run the upstream self-updater that targets `~/.grok/bin/grok`.
