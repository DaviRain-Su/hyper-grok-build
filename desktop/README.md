# Desktop

Optional desktop control surfaces for Hyper. Nested workspaces live here so
heavy UI stacks (gpui / Zed fork) do not pollute the root Cargo workspace.

| Path | Purpose |
|------|---------|
| [`comet/`](comet/) | Local-link Comet controller (gpui + engine + Hyper harness). Cloud stripped. |

## Quick start

```bash
# monorepo root
./scripts/run-desktop.sh
```

See [`comet/README.md`](comet/README.md) for env vars, daemon install, and layout.
