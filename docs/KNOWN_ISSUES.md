# Hyper known issues

Living list of fork-specific gaps, fixed items, and intentional limits.
Update this file when closing an issue or shipping a release.

Last reviewed: 2026-07-22 (v0.2.109 wire-compat release).

## Fixed in v0.2.109

- **xAI HTTP 426 / `x-grok-client-version`.** Release CI stamps
  `GROK_VERSION` from the root `VERSION` file into the binary. The `v0.1.0`
  marketing tag set that header to `0.1.0`, which production rejects
  (minimum **0.1.202**). Releases must use the monorepo lockstep version
  (currently `0.2.109`). Upgrade with a fresh `install.sh` run.

## Open (accepted for v0.2.109)

| ID | Severity | Topic | Notes |
|----|----------|--------|--------|
| Modes | design-only | Amp-style low–ultra agent modes | See [design-modes.md](./design-modes.md) — **not shipped**. Deferred; not a release blocker. |
| Non-Darwin Unix process ID | low | BSD without libproc | `is_grok_process` falls back to liveness-only on non-Linux non-macOS Unix. Rare for Hyper targets (we ship Linux/macOS/Windows). |

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
| F-4 | Kimi lock-held refresh vs 45s follower | Entire Kimi refresh retry loop is capped at **40s** (`REFRESH_TOTAL_BUDGET_SECS`), below the 45s flock wait. Blocking multi-thread resolvers also use the **20s** op timeout. |
| F-5 | Kimi/Codex sticky permanent-failure | Process-local sticky cache keyed by RT fingerprint (char-safe); 401/`invalid_grant` short-circuits force-refresh for 5 minutes; 5xx bodies are not sticky; cleared on login/logout/successful refresh. |
| F-7 | Child Task tool text omitted `oracle` | Nested `CHILD_TASK_DESCRIPTION` and `TaskToolInput` schema list `oracle`. |
| F-1-linux | Leader argv false positives | Linux classification uses **argv0 only** (not later args like `sleep hyper`). |

### S2 — macOS process identity + logout UX

| ID | Topic | Fix |
|----|--------|-----|
| F-1-mac | macOS/BSD liveness-only process check | macOS/iOS uses `proc_pidpath` + the same basename/path rules as Linux/Windows. |
| F-8 | Bare logout only cleared xAI | Bare logout prints remaining Kimi/Codex scopes; `hyper logout --all` clears xAI + Kimi + Codex (not BYOK keys). |

## Intentional / accepted

| Topic | Behavior |
|--------|----------|
| Shared `~/.grok` | Config, auth, sessions, and leader IPC live under the upstream home. Binary install root is `~/.hyper`. |
| Shared Kimi + Codex proxy | Catalog id (`kimi-code/*` vs `openai-codex/*`) selects credentials; ambiguous URL alone does not guess a family. |
| Hyper Modes | Design doc only until implemented. |
| Sticky refresh cache | In-process only (not shared across processes); multi-process still uses flock + compare/adopt. |
| Logout `--all` vs BYOK | Platform API keys under `platform/*` scopes stay until `/logout provider` / `/providers clear`. |

## Coexistence with official `grok`

- Different binaries: `hyper` vs `grok`.
- Shared runtime state under `~/.grok` (including `leader*.sock` / `leader*.lock`).
- Prefer `hyper leader kill` / `grok leader kill` only against leaders you own; both binaries recognize the other product process by name when cleaning locks (Linux argv0, Windows image path, macOS `proc_pidpath`).
- Community builds never run the upstream self-updater that targets `~/.grok/bin/grok`.
