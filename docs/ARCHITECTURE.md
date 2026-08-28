# Architecture — package layout (pi-mono style)

Hyper is a single Cargo workspace. Source crates live under `packages/*`,
grouped like [pi-mono](https://github.com/earendil-works/pi) product layers.
**Cargo package names** (`xai-*`) are unchanged for upstream merge compatibility.

## Layers

```text
packages/
  build/          # protoc / build helpers
  platform/       # paths, fs, git, crash, telemetry, test utils
  ai/             # models, auth, sampler, HTTP, voice  (~ pi-ai)
  tools/          # tools, sandbox, workspace, computer-hub
  agent/          # agent loop, chat state, compaction, hypercore  (~ pi-agent-core)
  tui/            # pager, render, markdown, ratatui, pty  (~ pi-tui)
  extensions/     # WASM extension host + SDK + marketplace
  coding-agent/   # shell session + hyper binary composition  (~ pi-coding-agent)

third_party/      # vendored deps
prod/             # product-specific shared types
bundled/          # runtime skills shipped with the binary
```

## Soft dependency direction

```text
platform  →  (leaf)
ai        →  platform
tools     →  platform, ai (types)
agent     →  ai, tools, platform
tui       →  platform (+ render-only)
extensions→  platform, tools (types)
coding-agent → agent, tools, tui, ai, extensions, platform
```

Phase-1 reorg only moved directories; layer cycles may still exist. Do not treat
this as an enforced lint yet.

## Key entry points

| Role | Crate path |
|------|------------|
| `hyper` binary | `packages/coding-agent/xai-grok-pager-bin` |
| Tailscale web control plane | `packages/coding-agent/xai-hyper-web` (`hyper web`) |
| TUI app | `packages/tui/xai-grok-pager` |
| Session / agent runtime | `packages/coding-agent/xai-grok-shell` |
| Sampler / LLM stream | `packages/ai/xai-grok-sampler` |
| Tools | `packages/tools/xai-grok-tools` |
| WASM extensions | `packages/extensions/xai-grok-extension-runtime` |

## Upstream merge

Upstream still uses `crates/codegen/*` and `crates/common/*`. See:

- [UPSTREAM_PATH_MAP.md](./UPSTREAM_PATH_MAP.md) — full old → new table
- `scripts/upstream-path-rewrite.py` — rewrite patches before apply
- `scripts/crate-package-map.toml` — machine-readable package membership

## User-facing docs

- English guide: `packages/tui/xai-grok-pager/docs/user-guide/`
- 中文指南: `packages/tui/xai-grok-pager/docs/user-guide-zh-CN/`
