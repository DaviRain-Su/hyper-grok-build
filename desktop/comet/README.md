# Comet — Hyper local desktop controller

Vendored desktop shell for Hyper: **local engine + gpui UI**, driving the monorepo
`hyper` binary over ACP (`hyper agent stdio`). Cloud edge / WorkOS multi-device
sync are disabled.

## Architecture

```text
comet UI / tui  ──IPC :27654──►  local engine  ──spawn──►  hyper agent stdio
                                     │
                                     └── data: ~/.hyper/desktop
                                              (spaces/chats; not ~/.grok)
```

| Surface | Role |
|---------|------|
| `hyper` (root monorepo) | Coding agent, auth (`~/.grok`), tools, TUI |
| `comet` (this tree) | Optional desktop controller |

## Build & run (recommended)

From the **monorepo root**:

```bash
./scripts/run-desktop.sh              # build hyper (release) + comet, then open UI
./scripts/run-desktop.sh --release    # both release
./scripts/run-desktop.sh --status     # resolve hyper + engine status
./scripts/run-desktop.sh -- headless  # engine only
```

Manual:

```bash
# 1) Hyper agent
cargo build -p xai-grok-pager-bin --features community-build --release
export HYPER_AGENT_BIN="$PWD/target/release/hyper"   # or CARGO_TARGET_DIR path

# 2) Desktop
cd desktop/comet
cargo build -p comet --release
./target/release/comet                # or $CARGO_TARGET_DIR/release/comet
```

`resolve_hyper_bin` auto-discovers monorepo `target/{release,debug}/hyper` when
you run from the repo (or set `HYPER_AGENT_BIN`).

## Commands

```bash
comet                 # headed gpui (default harness: hyper)
comet headless        # engine only
comet tui             # terminal viewport on local IPC
comet status          # data dir, IPC, resolved hyper path
comet agent-login     # hyper login --oauth
comet daemon install  # systemd/launchd user service
```

## Environment

| Var | Default | Meaning |
|-----|---------|---------|
| `HYPER_AGENT_BIN` | auto | Path to `hyper` / `grok` |
| `COMET_DATA_DIR` / `HYPER_DESKTOP_DATA_DIR` | `~/.hyper/desktop` (else legacy `~/.comet-native`) | Engine store |
| `COMET_IPC_PORT` | `27654` | Localhost engine port |
| `COMET_HARNESS` | `hyper` | `hyper` / `mock` / `codex` / `cursor` |

Cloud vars (`COMET_EDGE_*`, `COMET_WORKOS_*`) are **ignored**.

## Local-link product rules

- Settings nav: **Accounts / Shortcuts / Archived** (Devices cloud page hidden).
- Daemon: `dev.hyper.desktop` (launchd) / `hyper-desktop.service` (systemd).
- Agent credentials stay in Hyper (`hyper login` → `~/.grok`); desktop only stores UI sessions.

## Layout

```
desktop/comet/          # nested Cargo workspace (not in root members)
  apps/comet/           # `comet` binary
  apps/tui/             # `comet-tui`
  crates/
    engine/ harness/    # Hyper harness default
    ui/ tui/ rpc/       # gpui + ratatui + IPC
    proto/ doc/ sync/   # local docs (rooms off when offline)
```

## License

MIT (see `LICENSE`). Historical cloud design notes in `ARCHITECTURE.md` no longer
apply to this local-link fork.
