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
./scripts/package-desktop.sh   # optional local bundle under dist/desktop/
```

CI: `.github/workflows/desktop.yml` runs `cargo check -p comet` on desktop changes.

See [`comet/README.md`](comet/README.md) for feature map, env vars, daemon install, and layout.
