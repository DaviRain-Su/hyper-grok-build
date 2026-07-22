<div align="center">

<h1>Hyper (<code>hyper</code>)</h1>

**Hyper** is an unofficial multi-provider community build of
[Grok Build](https://github.com/xai-org/grok-build) — a terminal-based AI
coding agent with first-class support for xAI Grok, Kimi Code / Moonshot,
ChatGPT Codex, OpenAI, Anthropic, Z.AI, Ollama Cloud, and more.

It runs as a full-screen TUI that understands your codebase, edits files,
executes shell commands, searches the web, and manages long-running tasks —
interactively, headlessly for scripting/CI, or embedded in editors via the
Agent Client Protocol (ACP).

[Installation](#installation) ·
[Providers](#providers) ·
[Building from source](#building-from-source) ·
[Releasing](#releasing) ·
[Coexistence with official <code>grok</code>](#coexistence-with-official-grok) ·
[License](#license)

</div>

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
glibc / `linux-gnu` — correct for Omarchy, Ubuntu, Fedora, etc.), and Windows
(x86_64) are published on
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
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.1.0
```

The installer verifies every download against the release’s `SHA256SUMS`,
installs into `~/.hyper/bin/hyper` (`%USERPROFILE%\.hyper\bin\hyper.exe` on
Windows), and prints the PATH line to add when needed.

> **No release yet?** Build from source below, or wait for the first
> `v0.1.0` tag (see [Releasing](#releasing)).

---

## Providers

Hyper keeps the multi-provider registry from this tree (see the pager
[user guide](crates/codegen/xai-grok-pager/docs/user-guide/)):

| Platform | Auth | Notes |
| -------- | ---- | ----- |
| xAI / Grok | `hyper login` (OIDC) or `XAI_API_KEY` | First-party models |
| Kimi Code | device OAuth / subscription | `kimi-code/*` catalog |
| Moonshot CN / AI | API key | open platform |
| ChatGPT Codex | ChatGPT OAuth | GPT-5.x reasoning efforts incl. max/ultra |
| OpenAI / Anthropic / DeepSeek-style | API keys | BYOK catalog |
| Z.AI Coding Plan | platform key | international plan |
| Ollama Cloud | API key | live roster sync |

Model ids in the picker look like `{platform}/{model}` (e.g.
`kimi-code/k3`, `codex:gpt-5.5`). Platform docs live under
`crates/codegen/xai-grok-pager/docs/user-guide/` (Moonshot, Kimi Code,
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

1. Bump the root [`VERSION`](VERSION) file (e.g. `0.1.0`).
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
| macOS arm64 | `hyper-0.1.0-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `hyper-0.1.0-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (musl static) | `hyper-0.1.0-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 (musl static) | `hyper-0.1.0-aarch64-unknown-linux-musl.tar.gz` |
| Windows x86_64 | `hyper-0.1.0-x86_64-pc-windows-msvc.zip` |
| Checksums | `SHA256SUMS` |

The tag must match `VERSION` exactly (`v0.1.0` ↔ `0.1.0`) or the build fails.

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
- Community builds disable the upstream self-updater so `hyper update` cannot overwrite `~/.grok/bin/grok`. Upgrade Hyper by re-running `install.sh` / `install.ps1`.

Nothing in the official installer is rewritten by Hyper’s install script.

---

## Building notes (this fork)

```sh
# Defaults enable community-build (Hyper branding + no upstream updater).
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

[`crates/codegen/xai-grok-pager/docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)

Upstream product docs: [docs.x.ai/build](https://docs.x.ai/build/overview)

`SOURCE_REV` records the monorepo commit this tree was last synced from.

---

## Repository layout

| Path | Contents |
|------|----------|
| `crates/codegen/xai-grok-pager-bin` | Composition root; builds the `hyper` binary |
| `crates/codegen/xai-grok-pager` | TUI |
| `crates/codegen/xai-grok-shell` | Agent runtime |
| `install.sh` / `install.ps1` | Release installers |
| `.github/workflows/release.yml` | Multi-target release CI |

> [!IMPORTANT]
> The root `Cargo.toml` (workspace members / dependency versions) is
> **generated** from the monorepo — treat it as read-only. Prefer editing
> per-crate `Cargo.toml` files for local changes that should survive syncs.

---

## License

Apache-2.0. See [`LICENSE`](LICENSE), [`NOTICE`](NOTICE), and
[`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES).

Based on Grok Build open source
([xai-org/grok-build](https://github.com/xai-org/grok-build)).
