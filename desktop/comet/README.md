# Comet — Hyper local desktop controller

Vendored from [hyper-comet](file:///home/davirain/hyper-comet) into this monorepo
as the **desktop / local control entry** for Hyper.

## Scope (this fork)

| Kept | Removed |
|------|---------|
| gpui desktop UI (`comet`) | Cloudflare edge / Durable Objects (`edge/`) |
| Local engine + localhost IPC | WorkOS multi-device cloud sign-in |
| TUI attach (`comet tui`) | Remote DeviceRoom peer relay (online path) |
| Hyper harness (`hyper agent stdio`) | Cloud release update (`comet update`) |
| Local daemon install | iOS app |
| Agent OAuth (`comet agent-login`) | Production `edge.comet.zeron.sh` |

**Local-link only:** UI ↔ local engine (IPC) ↔ Hyper agent. No cloud sync.

## Layout

```
desktop/comet/          # nested Cargo workspace (not in root workspace members)
  apps/comet/           # `comet` binary (headed default, headless, tui, daemon)
  apps/tui/             # `comet-tui` binary
  crates/
    engine/ harness/    # agent runs (Hyper default)
    rpc/                # localhost IPC
    ui/                 # gpui
    tui/                # ratatui viewport
    proto/ doc/ sync/   # local docs/store (cloud rooms off when offline)
```

## Build

Separate from the root Hyper workspace (gpui pulls a Zed fork).

```bash
cd desktop/comet
cargo build -p comet --release
# binary: target/release/comet
```

Needs system deps for gpui (X11/Wayland on Linux, etc.). See upstream Zed build notes.

## Run

```bash
# Headed desktop (embeds engine or attaches to local daemon)
./target/release/comet

# Engine only
./target/release/comet headless

# Terminal viewport (attaches to local IPC)
./target/release/comet tui

# Status / agent login / daemon
./target/release/comet status
./target/release/comet agent-login
./target/release/comet daemon install
```

### Environment

| Var | Default | Meaning |
|-----|---------|---------|
| `COMET_DATA_DIR` | `~/.comet-native` | Local store |
| `COMET_IPC_PORT` | `27654` | Localhost engine port |
| `COMET_HARNESS` | `hyper` | Agent harness (`hyper` / `mock` / …) |
| `HYPER_AGENT_BIN` | resolve `hyper`/`grok` on PATH | Agent CLI |

Cloud vars (`COMET_EDGE_*`, `COMET_WORKOS_*`) are **ignored** by this local-link entrypoint.

## Relation to Hyper CLI

| Surface | Role |
|---------|------|
| `hyper` (root monorepo) | Primary TUI coding agent |
| `comet` (this tree) | Optional desktop shell that **controls** Hyper via ACP |

Build Hyper first so `hyper` is on `PATH` (or set `HYPER_AGENT_BIN`).

## License

MIT (see `LICENSE`). Upstream Comet architecture notes remain in `ARCHITECTURE.md`
for historical context; cloud sections do not apply to this fork.
