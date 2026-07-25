# Changelog

All notable changes to **Hyper** (`hyper` binary) are documented here.

## [Unreleased]

## [0.2.112] — 2026-07-25

### Added
- **Scoped models (Pi-style shortlist)** — `[models].enabled_models` globs (also reads Pi camelCase `enabledModels`), `/scoped-models` (`add` / `remove` / `set` / `clear`), and **Alt+]** / **Alt+[** to cycle only the shortlist. Empty shortlist cycles all usable models; invalid globs are rejected and never silently expand to “all models”. Full picker remains `/model` · Ctrl+M.
- **OpenAI prompt-cache affinity** — Every turn stamps `prompt_cache_key` (session id, ≤64 chars) on **Responses** and **Chat Completions** so OpenAI-compatible prefix caches stick to the session. Optional `GROK_PROMPT_CACHE_RETENTION=24h` (or `long`) for Responses extended retention; Codex still strips retention (backend rejects it). **No** Anthropic Messages multi-breakpoint `cache_control` expansion.
- **`/usage` cache hit rate** — Session usage shows a hit-rate line when providers report cached input tokens.
- **Docs** — User guide §29 models/providers/scoped selection (EN/zh), §30 OpenAI prompt caching (EN/zh), slash-command notes for `/scoped-models`.

### Fixed
- **macOS `/live` speaker silence** — The `__speaker-play` helper no longer forces mono/`i16`/48 kHz on CoreAudio. It opens the device default config (often stereo + `f32` + 44.1 kHz), resamples and upmixes the WebRTC mono stream, waits for a `READY`/`ERR` handshake before feeding PCM, and stops the playback queue if the player dies so failures surface instead of silent no-sound.
- **macOS `/live` crackle / stutter (撕拉)** — Match Linux’s continuous-stream model inside the helper: PCM goes into a continuous sample ring (not discrete `mpsc` chunks), callbacks use the same fill path as Windows (pull only what the buffer needs, hold last sample on underrun), prefer a 48 kHz device config when available, enlarge the playback queue to ~1s, and stop flushing every pipe write (which fragmented audio and starved CoreAudio).
- **Live Opus PLC double-decode** — After packet-loss concealment/FEC recovery, the decoder no longer immediately re-decodes the same payload without FEC (which corrupted state and could double-play or mute frames).

### Changed
- **Hyper auth is multi-provider first** — On community builds, first launch no longer auto-starts Grok OAuth. The welcome screen waits for you to choose: press `l` for the default login, or after entry use `/login openai`, `/login kimi`, `/providers <platform> <key>`, or `/model`. Consumer SuperGrok access gates no longer lock the whole TUI. When Grok free usage is exhausted, the modal also offers **Switch model or use API key** and **Dismiss** instead of only upgrade links.

### Notes
- Provider stance: prefer OpenAI-compatible APIs (Chat Completions / Responses) plus existing Messages paths; no first-class native Google / Bedrock / Azure clients.
- Wire version remains lockstep with Hyper crate versions (`0.2.112`).

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.112
```

## [0.2.111] — 2026-07-25

### Added
- **Codex Live voice sessions (`/live`)** — Full-duplex voice powered by ChatGPT Codex OAuth and `gpt-live-1-codex`, with realtime transcripts, mute and barge-in controls, native WebRTC/Opus audio, and spoken model responses. The Live assistant delegates coding work to the bound Hyper agent, relays tool-boundary progress, and returns the agent's final result to the voice conversation.
- **Sideband protocol hardening** — Precise WebSocket close-frame diagnostics (code + reason preserved), binary frame treated as protocol failure, EOF-without-close detection, and once-only error reporting via atomic `failure_reported` guard.
- **Error toast propagation** — Terminal transport/media errors are now preserved through to the user-facing toast (e.g. `"Live stopped: Codex live sideband closed (1008): policy changed"`) instead of a generic `"stopped unexpectedly"` message.
- **Log security** — `redact_live_error_for_log()` strips Bearer tokens, access tokens, cookies, session IDs, and passwords before writing errors to persistent diagnostic logs, with bounded length truncation.
- **Data-channel event gating** — Sideband-open atomic gate prevents duplicate `delegation.created`/transcript/turn events when both the sideband WebSocket and the data-channel deliver the same server payload.
- **Command queue reliability** — Capacity-aware critical drain: `CompleteDelegation` and `Shutdown` are queued with stable sequence IDs when the channel is full; commentary events are shed under pressure without silent protocol loss.
- **PCM hot-loop fix** — Closed PCM source no longer starves session teardown; the session remains responsive to `Shutdown`/`CompleteDelegation` commands after the microphone source ends.
- **Config unification** — Codex base URL now resolved through `PlatformId::OpenAiCodex.base_url()`, sharing the same `GROK_OPENAI_CODEX_BASE_URL` override as normal Codex inference.
- **Build isolation** — Linux musl target flags for RELRO/non-executable stack hardening in `.cargo/config.toml`.
- **Documentation** — Complete Codex Live user guide in English and Simplified Chinese (`/live` slash command, audio requirements, environment variables).

### Changed
- Sync the community build with the upstream `0.2.111` monorepo line (`SOURCE_REV` `9b8d35b`), including auth fail-closed refresh, voice interim commit-on-submit, workflow scratch quotas / resumable failed runs, leader process hardening, plugin subagent MCP inheritance, and the refreshed permissions / plugins / marketplaces docs.

### Notes
- `/live` uses an undocumented internal Codex Live protocol and may stop working when the backend changes. It is independent of the active coding provider but requires `grok login --openai`.
- Existing `/voice` dictation is unchanged. `/voice` and `/live` are mutually exclusive so they never compete for the microphone.
- Wire version remains lockstep with upstream crate versions (`0.2.111`).

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.111
```

## [0.2.110] — 2026-07-23

### Added
- Add `hyper dashboard --web`, a loopback-only, read-only web observability UI built with Axum and Leptos SSR.
- Add session overview, filtering, detail, timeline, chat, charts, active-process memory, unified-log, JSON API, and live SSE views over existing `~/.grok` artifacts.
- Add runtime-selectable TUI localization with ten language bundles and complete Simplified Chinese user-guide and hooks documentation.
- Add the built-in `xdotcom` subagent for X.com content workflows.

### Changed
- Sync the community build with the upstream `0.2.110` monorepo line.
- Refresh the project storefront with Hyper branding, real TUI screenshots, badges, and updated Oracle/modes design guidance.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.110
```

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
