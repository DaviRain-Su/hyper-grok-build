# Hyper known issues

Living list of fork-specific gaps, fixed items, and intentional limits.
Update this file when closing an issue or shipping a release.

Last reviewed: 2026-07-22 (post S0: leader recognition, Messages `/v1`, branding).

## Open (post-v0.1.0 hardening)

| ID | Severity | Topic | Notes |
|----|----------|--------|--------|
| F-4 | medium | Kimi refresh lock vs follower wait | Pathological 5xx retry sequences can outlive the 45s follower lock wait. |
| F-5 | medium | Kimi/Codex sticky permanent-failure | Revoked refresh tokens can re-hit the IdP every turn; no credential-scoped terminal verdict. |
| F-7 | low | Child Task tool text omits `oracle` | Parent roster includes oracle; nested `CHILD_TASK_DESCRIPTION` still lists only general-purpose / explore / plan. |
| F-8 | low (UX) | Bare logout is xAI-only | `hyper logout` clears the xAI session; Kimi/Codex need `hyper logout --kimi` / `--openai` or `/logout provider …`. |
| Modes | design-only | Amp-style low–ultra agent modes | See [design-modes.md](./design-modes.md) — **not implemented**. |

## Fixed in tree (S0)

| ID | Topic | Fix |
|----|--------|-----|
| F-1 | `is_grok_process` ignored `hyper` | Recognizes basenames `hyper` / `grok`, `xai-grok-*` test bins, and `~/.hyper/bin` / `~/.grok/bin` paths. |
| F-2 | MiniMax / Fireworks Messages 404 | Messages `base_url_override` values are normalized to end in `/v1` before the sampler joins `/messages`. |
| F-3 | Branding | `community-build` (default on the Hyper binary) makes `--version` and `completions` emit `hyper`. |
| F-9 | Local builds without community-build | `xai-grok-pager-bin` defaults include `community-build`. |

## Intentional / accepted

| Topic | Behavior |
|--------|----------|
| Shared `~/.grok` | Config, auth, sessions, and leader IPC live under the upstream home. Binary install root is `~/.hyper`. |
| Shared Kimi + Codex proxy | Catalog id (`kimi-code/*` vs `openai-codex/*`) selects credentials; ambiguous URL alone does not guess a family. |
| Hyper Modes | Design doc only until implemented. |

## Coexistence with official `grok`

- Different binaries: `hyper` vs `grok`.
- Shared runtime state under `~/.grok` (including `leader*.sock` / `leader*.lock`).
- Prefer `hyper leader kill` / `grok leader kill` only against leaders you own; both binaries now recognize the other product process by name when cleaning locks.
- Community builds never run the upstream self-updater that targets `~/.grok/bin/grok`.
