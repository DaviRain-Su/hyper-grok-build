# Comet — Hyper local desktop controller

Vendored desktop shell for Hyper: **local engine + gpui UI**, driving the monorepo
`hyper` binary over ACP (`hyper agent stdio`). Cloud edge / WorkOS multi-device
sync are disabled.

## Architecture

```text
comet UI / tui  ──IPC :27654──►  local engine  ──spawn──►  hyper agent stdio
     │                │                                         │
     │                └── data: ~/.hyper/desktop                │
     │                     (spaces / chats / UI)                │
     └── Settings → Hyper page                                  │
                                                                ▼
                                              GROK_HOME (~/.grok by default)
                                              auth, memory, skills, plugins,
                                              WASM extensions, Rhai workflows
```

| Surface | Role |
|---------|------|
| `hyper` (root monorepo) | Coding agent, auth, tools, TUI, workflows, extensions |
| `comet` (this tree) | Optional desktop controller (local-link only) |

**Sessions are not merged:** desktop chat lists live in the engine data dir;
Hyper TUI transcripts stay under `GROK_HOME`. Same agent identity and tools.

## Feature map (local-link fork)

| Feature | Status |
|---------|--------|
| Headed gpui + headless engine + TUI attach | Done |
| Default harness Hyper + monorepo binary discovery | Done |
| Offline (no cloud rooms / WorkOS) | Done |
| Settings: Hyper / Accounts / Shortcuts / Archived | Done |
| Devices multi-device UI | Hidden (deep-link only) |
| `/workflow` via Hyper agent in chat | Done (agent-side; see Settings → Hyper) |
| WASM extensions | Via Hyper agent config (not a second host) |
| `./scripts/run-desktop.sh` | Done |
| `./scripts/package-desktop.sh` | Done (local tarball; not in GitHub Release matrix yet) |
| CI `desktop.yml` check | Done |
| Ship `comet` on GitHub Release with `hyper` | Not yet (optional follow-up) |

## Build & run

From the **monorepo root**:

```bash
./scripts/run-desktop.sh              # build hyper (release) + comet, open UI
./scripts/run-desktop.sh --release    # both release
./scripts/run-desktop.sh --status
./scripts/run-desktop.sh -- headless
./scripts/package-desktop.sh          # dist/desktop/hyper-desktop-<ver>-<triple>.tar.gz
```

## Commands

```bash
comet                 # headed gpui
comet headless        # engine only
comet tui             # terminal viewport on local IPC
comet status          # data dir, IPC, resolved hyper path
comet agent-login     # hyper login --oauth
comet daemon install  # systemd/launchd (dev.hyper.desktop / hyper-desktop.service)
```

## Environment

| Var | Default | Meaning |
|-----|---------|---------|
| `HYPER_AGENT_BIN` | auto | Path to `hyper` / `grok` |
| `COMET_DATA_DIR` / `HYPER_DESKTOP_DATA_DIR` | `~/.hyper/desktop` | Engine store |
| `GROK_HOME` | `~/.grok` | Hyper agent home (shared with CLI) |
| `COMET_IPC_PORT` | `27654` | Localhost engine port |
| `COMET_HARNESS` | `hyper` | `hyper` / `mock` / `codex` / `cursor` |

Cloud vars (`COMET_EDGE_*`, `COMET_WORKOS_*`) are **ignored** / stripped from the agent child.

## Layout

```
desktop/comet/          # nested Cargo workspace (not in root members)
  apps/comet/           # `comet` binary
  apps/tui/             # `comet-tui`
  crates/
    engine/ harness/    # Hyper harness default
    ui/ tui/ rpc/       # gpui + ratatui + IPC
```

## License

MIT (see `LICENSE`). Historical cloud design notes in `ARCHITECTURE.md` no longer
apply to this local-link fork.
