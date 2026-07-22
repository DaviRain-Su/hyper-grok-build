# Changelog

All notable changes to **Hyper** (`hyper` binary) are documented here.

## [0.2.109] — 2026-07-22

**Wire-compatible release.** Hyper stamps `x-grok-client-version` from the root
`VERSION` file via `GROK_VERSION` at build time. xAI's API gate rejects clients
below **0.1.202** (HTTP 426). The previous `0.1.0` marketing tag was therefore
unusable against production Grok models (e.g. grok-4.5).

This tag **matches the monorepo lockstep crate version** (`xai-grok-pager` /
`xai-grok-version` / shell at `0.2.109`), which is also above the official
stable line (`grok 0.2.106` at time of release).

### Fixes
- Align release `VERSION` / GitHub tag with monorepo client version so API
  version gates accept the binary.
- Document that Hyper release tags must track the pager lockstep version, not
  an independent `0.1.x` marketing line.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.109
```

The earlier `v0.1.0` assets remain on GitHub Releases for historical download
but must not be used against current xAI endpoints.

## [0.1.0] — 2026-07-22

First tagged Hyper community release of the multi-provider Grok Build fork.

> **Superseded for API use.** `x-grok-client-version: 0.1.0` is rejected by
> xAI (min **0.1.202**). Upgrade to **v0.2.109** or later.

### Highlights

- **Binary name `hyper`**, install root `~/.hyper/bin`, shared runtime state under `~/.grok` (compatible with official `grok` config/auth).
- **Multi-provider catalog**: Moonshot, Kimi Code OAuth, ChatGPT Codex OAuth, OpenAI/Anthropic BYOK, Z.AI, Ollama Cloud, and more.
- **Oracle** built-in read-only subagent for deep analysis (pin a strong model via `/agents` or `[subagents.models]`).
- **Community builds** disable the upstream self-updater so Hyper cannot overwrite `~/.grok/bin/grok`.

### Reliability (this release line)

- Keep xAI session bearer off third-party BYOK platforms (including live-only catalog models).
- Route-aware opaque reasoning replay (model + API backend + endpoint identity).
- Catalog-first OAuth identity for Kimi vs Codex (including shared reverse-proxy origins).
- Kimi/Codex sticky permanent-failure cache for revoked refresh tokens (process-local).
- Kimi lock-held refresh total budget capped below the cross-process flock wait.
- Multi-thread blocking resolvers bounded by a 20s operation timeout.
- MiniMax / Fireworks Messages bases normalized to `…/v1` before `/messages` join.
- Leader cleanup recognizes `hyper` and `grok` product processes (Linux argv0, Windows image path, macOS `proc_pidpath`).
- `hyper logout --all` clears xAI + Kimi + Codex OAuth; bare logout hints remaining scopes.
- `hyper --version` / completions brand as `hyper` when built with `community-build` (default).

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.109
```

See [README.md](./README.md) and [docs/KNOWN_ISSUES.md](./docs/KNOWN_ISSUES.md).

### Not in this release

- Amp-style **agent modes** (low/medium/high/ultra) — design only (`docs/design-modes.md`).
