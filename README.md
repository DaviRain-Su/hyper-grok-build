<div align="center">

<h1>Hyper (<code>hyper</code>)</h1>

<img src="docs/assets/hyper-banner.jpg" alt="Hyper — terminal AI coding agent" width="720">

<p>
  <a href="https://github.com/DaviRain-Su/hyper-grok-build/releases"><img src="https://img.shields.io/github/v/release/DaviRain-Su/hyper-grok-build?display_name=tag" alt="Release"></a>
  <a href="https://github.com/DaviRain-Su/hyper-grok-build/actions/workflows/release.yml"><img src="https://github.com/DaviRain-Su/hyper-grok-build/actions/workflows/release.yml/badge.svg" alt="Release CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License"></a>
  <img src="https://img.shields.io/badge/rust-1.92.0-orange?logo=rust" alt="Rust 1.92">
  <img src="https://img.shields.io/badge/platform-macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-lightgrey" alt="Platforms: macOS, Linux, Windows">
  <a href="https://github.com/DaviRain-Su/hyper-grok-build/releases"><img src="https://img.shields.io/github/downloads/DaviRain-Su/hyper-grok-build/total?label=downloads" alt="Downloads"></a>
  <img src="https://img.shields.io/badge/i18n-10%20locales-brightgreen" alt="i18n: 10 locales">
</p>

**Hyper** is an unofficial multi-provider community build of
[Grok Build](https://github.com/xai-org/grok-build) — a terminal-based AI
coding agent written in Rust, with first-class multi-provider LLM support:
xAI Grok, Kimi Code / Moonshot, ChatGPT Codex, OpenCode Go, OpenAI,
Anthropic, Z.AI, Ollama Cloud, and more.

It runs as a full-screen TUI that understands your codebase, edits files,
executes shell commands, searches the web, and manages long-running tasks —
interactively, headlessly for scripting/CI, or embedded in editors via the
Agent Client Protocol (ACP). The UI is localized in 10 languages
(English, 中文, 日本語, 한국어, Español, Português, Français, Deutsch,
Русский) and switchable live from Settings. A local, read-only Rust web
dashboard is available with `hyper dashboard --web` for session metrics,
timelines, charts, logs, and live event streaming.

[Installation](#installation) ·
[Providers](#providers) ·
[Building from source](#building-from-source) ·
[Releasing](#releasing) ·
[Coexistence with official <code>grok</code>](#coexistence-with-official-grok) ·
[License](#license)

**中文文档: [README.zh-CN.md](README.zh-CN.md)** ·
**中文用户指南: [docs/user-guide-zh-CN/](packages/tui/xai-grok-pager/docs/user-guide-zh-CN/)**

</div>

---

## Screenshots

The real TUI (captured in a PTY with the in-repo
[`tui_shot`](packages/tui/xai-grok-pager-pty-harness/examples/tui_shot.rs)
harness), in two of the ten UI locales:

| English | 简体中文 |
| ------- | -------- |
| ![Hyper TUI in English](docs/assets/screenshot-welcome-en.png) | ![中文界面的 Hyper TUI](docs/assets/screenshot-welcome-zh.png) |

---

## Why “Hyper”?

The fork repo is already named `hyper-grok-build`. **Hyper** keeps that brand:

| | Official | This fork |
|---|---|---|
| Product | Grok Build | **Hyper** |
| Binary | `grok` | **`hyper`** |
| Install root | `~/.grok` | **`~/.hyper`** (binary only) |
| Config / auth | `~/.grok` | **`~/.grok`** (shared; same runtime) |
| Upstream | [xai-org/grok-build](https://github.com/xai-org/grok-build) | multi-provider community patches |

Short CLI, no clash with `grok`, and room to grow beyond a single provider
(unlike Kimi-only forks such as [Kigi](https://github.com/ZacharyZhang-NY/Kigi-CLI)).

---

## Installation

Prebuilt single-file binaries for macOS (arm64/x86_64), Linux (arm64/x86_64,
glibc / `linux-gnu` — linked against **glibc 2.17+** so they run on Ubuntu
16.04 / RHEL 7 and newer, not only Ubuntu 24.04), and Windows (x86_64) are
published on
[GitHub Releases](https://github.com/DaviRain-Su/hyper-grok-build/releases):

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.ps1 | iex
```

```sh
hyper --version
hyper login          # xAI / Grok session (browser OAuth)
hyper                # start the TUI
```

Pin a release:

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.118-r1
```

The installer verifies every download against the release’s `SHA256SUMS`,
installs into `~/.hyper/bin/hyper` (`%USERPROFILE%\.hyper\bin\hyper.exe` on
Windows), and prints the PATH line to add when needed.

> Need unreleased changes? Build from source below; otherwise install the latest release above.

### Install with Nix

A [Nix](https://nixos.org) flake is provided (`flake.nix`), so on any
Nix-enabled machine you can skip the installer and build/run directly.
The flake builds the same `hyper` binary as the release artifacts
(statically links Opus and jemalloc; `ldd` shows only glibc).

```sh
# Run directly from the repo (no clone, no install):
nix run github:DaviRain-Su/hyper-grok-build#hyper-grok-build -- --version

# Or install into your Nix profile (puts `hyper` on PATH):
nix profile install github:DaviRain-Su/hyper-grok-build#hyper-grok-build
```

From a clone (e.g. for unreleased changes or to hack on it):

```sh
git clone https://github.com/DaviRain-Su/hyper-grok-build
cd hyper-grok-build
nix run .#hyper-grok-build -- --version      # run
nix build .#hyper-grok-build                 # build to ./result
nix develop                                   # shell with rust + protoc + cmake + git
```

> First run compiles from source (~14 min on a modern machine). There is
> no binary cache yet, so every Nix user builds locally for now. Linux
> (`x86_64`/`aarch64`) is supported; macOS/Windows are not wired up in
> the flake (use the prebuilt binaries above for those).

---

## Providers

Hyper keeps the multi-provider registry from this tree (see the pager
[user guide](packages/tui/xai-grok-pager/docs/user-guide/)):

| Platform | Auth | Notes |
| -------- | ---- | ----- |
| xAI / Grok | `hyper login` (OIDC) or `XAI_API_KEY` | First-party models |
| Kimi Code | device OAuth / subscription | `kimi-code/*` catalog |
| Moonshot CN / AI | API key | open platform |
| ChatGPT Codex | ChatGPT OAuth | GPT-5.x reasoning plus experimental full-duplex `/live` voice |
| OpenCode Go | subscription API key | `opencode-go/*` models over Chat Completions + Messages |
| OpenAI / Anthropic / DeepSeek-style | API keys | BYOK catalog |
| Z.AI Coding Plan | platform key | international plan |
| Ollama Cloud | API key | live roster sync |

Model ids in the picker look like `{platform}/{model}` (e.g.
`kimi-code/k3`, `opencode-go/kimi-k3`, `openai-codex/gpt-5.6-sol`). Platform docs live under
`packages/tui/xai-grok-pager/docs/user-guide/` (Moonshot, Kimi Code,
OpenAI Codex, …).

Config and credentials still live under **`~/.grok`** (same paths as
upstream Grok Build), so existing sessions, API keys, and `auth.json`
keep working.

---

## Building from source

Requirements:

- **Rust** — pinned by [`rust-toolchain.toml`](rust-toolchain.toml)
  (`rustup` installs it on first build)
- **[DotSlash](https://dotslash-cli.com)** — hermetic `bin/protoc`
  ```sh
  cargo install dotslash
  # or: brew install dotslash
  ```
- **CMake 3.5+** — builds the bundled static Opus library used by experimental
  `/live` voice (the workspace pins `CMAKE_POLICY_VERSION_MINIMUM=3.5`)

```sh
cargo run -p xai-grok-pager-bin              # build + launch TUI (binary: hyper)
cargo build -p xai-grok-pager-bin --profile release-dist
./target/release-dist/hyper --version
```

The composition-root package is still `xai-grok-pager-bin` (monorepo
layout); the **shipped binary name** is `hyper`.

---

## Changelog

See [`CHANGELOG.md`](./CHANGELOG.md) for release notes. Known limitations:
[`docs/KNOWN_ISSUES.md`](./docs/KNOWN_ISSUES.md).

---

## Releasing

1. Set the root [`VERSION`](VERSION) file to the **monorepo lockstep client
   version** (same as `packages/tui/xai-grok-pager/Cargo.toml` /
   `xai-grok-version`, currently `0.2.118-r1`). CI compiles this into
   `x-grok-client-version`; xAI rejects clients below **0.1.202** (HTTP 426).
   Do **not** invent a separate low marketing version (e.g. `0.1.0`).
2. Commit on `dev` (or your release branch); update `CHANGELOG.md`.
3. Tag and push — CI builds five targets and publishes a GitHub Release:

```sh
VERSION=$(tr -d '[:space:]' < VERSION)
git tag "v${VERSION}"
git push origin "v${VERSION}"
```

Workflow: [`.github/workflows/release.yml`](.github/workflows/release.yml)

Artifacts:

| Asset | Example |
| ----- | ------- |
| macOS arm64 | `hyper-0.2.118-r1-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `hyper-0.2.118-r1-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (glibc ≥2.17) | `hyper-0.2.118-r1-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 (glibc ≥2.17) | `hyper-0.2.118-r1-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `hyper-0.2.118-r1-x86_64-pc-windows-msvc.zip` |
| Checksums | `SHA256SUMS` |

The tag must match `VERSION` exactly (`v0.2.118-r1` ↔ `0.2.118-r1`) or the build fails.

---

## Coexistence with official `grok`

Hyper is **not** affiliated with xAI / SpaceXAI. On the same machine:

| Surface | Official `grok` | Hyper |
|---------|-----------------|-------|
| Binary | `grok` | `hyper` |
| Managed install root | `~/.grok/bin` | `~/.hyper/bin` |
| Config / auth / sessions | `~/.grok` | **same** `~/.grok` |
| Leader IPC (`leader*.sock` / `.lock`) | under `~/.grok` | **same** namespace |

Implications:

- Sessions, API keys, and OAuth scopes are shared — log in once, both CLIs can see them.
- Leader list/kill can see both products’ leaders. Prefer killing only leaders you started.
- Community builds use an isolated updater: `hyper update` and startup auto-update read only this repository's GitHub Releases, while Hyper binaries and update state stay under `~/.hyper` (the managed executable is `~/.hyper/bin/hyper`). Release archives may also ship a managed `bundled/**` tree, which the updater installs transactionally at `~/.grok/bundled` (or `$GROK_HOME/bundled`). They never overwrite `~/.grok/bin/grok`. The auto-update preference remains part of Hyper's shared `~/.grok` configuration. Re-running `install.sh` / `install.ps1` remains a supported recovery path.

Nothing in the official installer is rewritten by Hyper’s install script.

---

## Building notes (this fork)

```sh
# Defaults enable community-build (Hyper branding + isolated community updater).
cargo run -p xai-grok-pager-bin

# Explicit release-style local binary
cargo build -p xai-grok-pager-bin --profile release-dist --features community-build
```

Amp-style **agent modes** (low / medium / high / ultra slots) are **design-only** —
see [`docs/design-modes.md`](docs/design-modes.md). They are not shipped yet.

Known issues and remaining work: [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md).

---

## Documentation

In-tree user guide (examples may still say `grok`; the Hyper binary name is
`hyper`, paths remain under `~/.grok`):

- English: [`packages/tui/xai-grok-pager/docs/user-guide/`](packages/tui/xai-grok-pager/docs/user-guide/)
- 中文: [`packages/tui/xai-grok-pager/docs/user-guide-zh-CN/`](packages/tui/xai-grok-pager/docs/user-guide-zh-CN/)

Related extension docs also have Chinese translations (`*.zh-CN.md`) under
`packages/tui/xai-grok-pager/docs/`.

Upstream product docs: [docs.x.ai/build](https://docs.x.ai/build/overview)

`SOURCE_REV` records the monorepo commit this tree was last synced from.

---

## Repository layout

Layout follows [pi-mono](https://github.com/earendil-works/pi)-style package
layers under `packages/*` (crate names stay `xai-*` for upstream merges).
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and
[docs/UPSTREAM_PATH_MAP.md](docs/UPSTREAM_PATH_MAP.md).

| Path | Contents |
|------|----------|
| `packages/ai/` | Models, auth, sampler, HTTP, voice |
| `packages/agent/` | Agent loop, chat state, compaction, HyperCore |
| `packages/tools/` | Tools, sandbox, workspace, computer-hub |
| `packages/tui/` | Pager TUI, render, markdown, PTY |
| `packages/coding-agent/` | Shell session + `hyper` binary composition |
| `packages/extensions/` | WASM extension host / SDK / marketplace |
| `packages/platform/` | Paths, FS/git, crash, telemetry, tests |
| `packages/build/` | Build helpers (protoc) |
| `desktop/comet/` | Optional **local** desktop controller (gpui; nested workspace; cloud stripped) |
| `install.sh` / `install.ps1` | Release installers |
| `.github/workflows/release.yml` | Multi-target release CI |

> [!IMPORTANT]
> Prefer editing per-crate `Cargo.toml` files. Workspace member paths live in
> the root `Cargo.toml`; keep them in sync when adding crates.

---

## License

Apache-2.0. See [`LICENSE`](LICENSE), [`NOTICE`](NOTICE), and
[`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES).

Based on Grok Build open source
([xai-org/grok-build](https://github.com/xai-org/grok-build)).
