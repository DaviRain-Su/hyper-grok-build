# Changelog

All notable changes to **Hyper** (`hyper` binary) are documented here.

## [Unreleased]

### Added
- **Native OMP session continuation** — `/resume-omp`, the foreign-session picker, recent-session Ctrl+U hint, and `[compat.omp].sessions` now discover OMP CLI sessions lazily behind the same bundled-runtime gate as Claude, Codex, and Cursor. Release archives install the native `resume-omp` wrapper plus the shared inert-history reader, including OMP profile/XDG/custom-root and native-ID support.
- **Base16 Default Dark and OMP themes** — Adds stable theme IDs 18 (`base16-default-dark`) and 19 (`omp` / Titanium), terminal-capability clamping for syntax colors, and release packaging for the shared resume-session readers.
- **Extension author proc macros** — Recommended guest path is now `#[hyper_plugin]` / `#[hyper_hook]` / `#[hyper_tool]` so handlers stay ordinary named functions for IDE navigation; legacy `hyper_extension!` remains for source compatibility.

## [0.2.114-r3] — 2026-07-28

### Fixed
- **OpenCode Go retry response parsing** — Accept valid Chat Completions chunks and Messages `message_start` events that omit only the provider response ID, while leaving semantic tool-call validation and other required stream fields unchanged.
- **Oversized Linux release binaries** — Distribution builds no longer embed debug metadata or retain non-runtime symbols. The release workflow now rejects binaries over 256 MiB and verifies that Linux artifacts contain no DWARF debug sections.

### Notes
- This is community revision `0.2.114-r3`; it remains a normal GitHub Release so installers and `hyper update` treat it as latest.
- The r2 size regression was Linux-specific: both Linux binaries unpacked to about 1.36 GB, while r2 macOS binaries were 174–188 MB and Windows was 151 MB. The stricter release profile is applied consistently to all five targets.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r3
```

## [0.2.114-r2] — 2026-07-28

### Added
- **Pi-aligned provider platform** — Data-driven registry and reproducible catalog sync pinned to `@earendil-works/pi-ai@0.82.1`, covering all 37 static Pi providers plus dynamic Radius discovery. Hyper now ships 42 provider rows and 1,144 catalog models with explicit endpoint, authentication, protocol, thinking, and request-compat metadata.
- **Native provider backends** — First-class Google GenerateContent (Gemini and Vertex), Amazon Bedrock ConverseStream, and Pi `pi-messages` adapters with streaming text/reasoning, tool calls, cache/reasoning usage, provider-reported cost, and provider-native continuation state.
- **Provider authentication** — GitHub Copilot OAuth and model discovery; Radius browser PKCE, device flow, refresh-token rotation, API-key priority, credential-scoped dynamic caching, single-flight refresh, and stale fallback; expanded API-key and hybrid-provider login/logout UX across CLI and TUI.
- **WASM extension platform** — Trusted plugins can load sandboxed Wasmtime guests and participate in session start/end, before-agent, before-model, pre-tool, stop-gate, and pre-compaction lifecycle points. Capability-gated guests can inject context, deny tools, continue a turn, register session-scoped tools, emit metrics, and retain bounded per-session state.
- **Extension author tooling** — New extension API/runtime/SDK crates, declarative Rust guest macros, checked-in examples, plugin init/build/validate commands, runtime details in `/plugins`, author documentation, and a path-filtered extension CI workflow.

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `02d9359`, including scheduler foreground/background loop semantics, plan-exit batch barriers, leader sandbox confinement, workspace/hub reliability, and pager session-state improvements.
- **Provider routing architecture** — Sampling now dispatches through explicit backend adapters instead of inferring behavior from provider names. Opaque reasoning signatures are replayed only when model, backend, and endpoint identities match.

### Fixed
- **Provider stream correctness** — Hardened truncated/unknown event handling, zero-argument and interleaved tool calls, authoritative argument assembly, usage/cost accounting, idle timeouts, and portable fallback when native continuation identity changes.
- **OAuth and dynamic catalog safety** — Radius callback errors are accepted only after OAuth state validation, expiry skew is applied once, and dynamic model updates remain atomic and credential-isolated.
- **Extension lifecycle isolation** — WASM tools are scoped and cleaned up per session; concurrent sessions cannot unregister each other’s tools; fail-closed trap behavior, stop continuation caps, fuel/epoch bounds, and guest memory limits are covered end to end.

### Notes
- This is community revision `0.2.114-r2`; it remains a normal GitHub Release so installers and `hyper update` treat it as latest.
- The extension ABI is the documented core-WASM bootstrap contract. Only trusted, enabled plugins load; policy gates default to fail-open for compatibility unless `runtime.gate_fail` or `GROK_EXTENSION_GATE_FAIL` selects `closed`.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r2
```

## [0.2.114-r1] — 2026-07-28

### Added
- **OpenCode Go subscription provider** — Configure a Console-issued Go API key with `/providers opencode-go <key>` or `OPENCODE_API_KEY`, then use the bundled `opencode-go/*` catalog. Models are routed per official metadata across OpenAI Chat Completions and Anthropic Messages; `/login opencode-go` explains the documented API-key flow instead of reusing OpenCode's undocumented, CLI-specific OAuth client identity.
- **Isolated Hyper self-updates** — `hyper update` and startup auto-update now resolve only `DaviRain-Su/hyper-grok-build` GitHub Releases, verify `SHA256SUMS`, and keep managed binaries/update state under `~/.hyper`. The official `~/.grok/bin/grok` installation is never used as an update target.
- **12 preset themes** — A curated collection layered on top of the original five, distinguished primarily by background color: nine dark (`everforest`, `nord`, `dracula`, `gruvbox`, `catppuccin-mocha`, `solarized-dark`, `deep-ocean`, `ember`, `midnight-oled`) and three light (`solarized-light`, `catppuccin-latte`, `paper`). Pick via `/theme <name>`, Settings → Appearance → Theme, or the `auto` dark/light pairings. All are truecolor (RGB) and fall back to Grok Night on 256/16-color terminals. Each preset is defined from a compact palette expanded through a shared builder so semantic roles (error=red, success=green, sunken code blocks, scrollbar contrast) stay consistent across the set.
- **Translations for the 12 preset themes** — `settings.{theme,auto_dark_theme,auto_light_theme}.choice.*.description` for the nine dark + three light presets across all nine non-English locales (de, es, fr, ja, ko, pt-BR, ru, zh-CN, zh-TW); previously they fell back to English.
- **Nix flake packaging** — `flake.nix` / `flake.lock` package `hyper` from `xai-grok-pager-bin` with a matching `devShell`. Package version is read from the root `VERSION` file (no hardcoding). Documented in README as `nix build` / `nix run` / `nix develop`.

### Fixed
- **Bare login provider drift** — Bare `/login` and the welcome-screen Login action now resolve the advertised xAI `grok.com` / enterprise OIDC method on every invocation instead of reusing a prior explicit Kimi, OpenAI, or Claude selection. Third-party subscription login remains available only through its explicit provider command.
- **Same-version republish safety** — Community deployments use the release archive SHA-256 as part of their identity, so a deliberately republished tag installs once and converges. Downloads are locked, staged, smoke-tested, and atomically activated without overwriting the current binary first.
- **Theme switch appeared to need a restart** — On terminals that don't advertise truecolor, the Settings → Appearance theme picker still offered the truecolor-only presets. Selecting one clamped the live view to Grok Night (screen unchanged) yet persisted the choice, so it only "took effect" after a restart (the startup path applies the persisted theme un-clamped). The picker now hides themes the current terminal can't render (mirrors `/theme`'s `available()` gate), so `theme` / `auto_dark_theme` / `auto_light_theme` only list renderable options.
- **Theme toasts bypassed i18n** — The `theme` / `auto_dark_theme` / `auto_light_theme` "✓ …" confirmation toasts were hardcoded English (label + format). They now route through the localized `toast.saved` bundle like every other setting.

### Notes
- Community revision tag uses a `-rN` suffix (`0.2.114-r1`) so we can ship Hyper-only changes without claiming a clean upstream patch bump. Later community revisions on the same line can be `0.2.114-r2`, `0.2.114-r3`, …
- Wire version remains lockstep with Hyper crate versions (`0.2.114-r1`). GitHub Release is published as a normal (non-prerelease) release so installers and `hyper update` treat it as latest.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r1
```

## [0.2.113] — 2026-07-27

### Added
- **Nexus relay gateway** — `/nexus <api_key> [base_url]` and `/providers nexus …` persist a BYOK bearer plus an optional self-hosted gateway root, discover both OpenAI Chat Completions and Anthropic Messages catalogs, and route each discovered model through the matching protocol endpoint.
- **Anthropic Claude subscription login** — `/claude` and `/login claude` add Claude Pro/Max OAuth with PKCE, scoped credential storage, rotating refresh-token persistence, per-request bearer resolution, and built-in Claude subscription models.

### Fixed
- **`/live` idle disconnects** — The Codex Live sideband now sends protocol-level keepalive pings while no control messages are flowing, preventing proxies and load balancers from reaping otherwise healthy voice sessions after a few quiet minutes.
- **Subagent model-pin activation** — Saving a model pin in `/agents` now performs an acknowledged shell reload before releasing the modal, so fresh subagent spawns use the new model immediately even after a long-running session; resumed agents still retain their original model.
- **Claude OAuth safety** — Require an exact OAuth `state` match before callback success or token exchange, use Anthropic’s registered callback for the bundled client, reject unsupported loopback redirects, and serialize rotating refresh tokens across processes.
- **Provider and policy regressions** — Preserve Nexus custom gateway URLs end-to-end, trusted managed-config signature controls, fail-closed settings-only startup prefetch, tolerant MCP parsing, and Unix file-descriptor limit raising.
- **Provider credential fallback** — When a BYOK or platform OAuth credential is cleared or expires, a newly locked provider model can no longer remain the active sampling model; the session falls back to an available default before the catalog update is published.
- **Pager registry coverage** — Reserve the new provider/scoped-model commands and aliases, complete translations across all supported locales, and refresh deterministic usage snapshots.

### Changed
- **Upstream sync** — Sync the community build with upstream `main` at `SOURCE_REV` `91d8cf309110a3b879c1b8198f7525aed545dfb4`, including instant UI startup with background model/settings fetch, bounded session-load and fork-replay memory, subagent lifecycle resource bounds, full-plan copy with `y`, terminal-version telemetry, and managed-policy hardening.

### Notes
- `v0.2.113` was republished after this upstream sync; replace any earlier archive and checksum file together because the rebuilt assets have different digests.
- Wire version remains lockstep with Hyper crate versions (`0.2.113`).

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.113
```

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
