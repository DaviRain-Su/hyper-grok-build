# safe-shell-wasm (Phase 1 example)

Denies tool calls whose JSON input contains the substring `rm -rf`, using the
**core-wasm bootstrap ABI** described in
[`docs/design-wasm-extensions.md`](../../../../../../docs/design-wasm-extensions.md).

## Build

```bash
# with wabt
wat2wasm extension.wat -o extension.wasm

# or with the `wat` crate (from this workspace)
cargo test -p xai-grok-extension-runtime safe_shell_denies_rm_rf
```

## Install

```bash
mkdir -p ~/.grok/plugins/safe-shell-wasm
cp plugin.json extension.wasm ~/.grok/plugins/safe-shell-wasm/
```

Enable in `~/.grok/config.toml`:

```toml
[plugins]
enabled = ["safe-shell-wasm"]
```

User-scope plugins under `~/.grok/plugins/` are auto-trusted. Restart the
session (or reload plugins) so the runtime loads `extension.wasm`.

## Expected behavior

- `run_terminal_command` with `rm -rf …` in the arguments → blocked  
- Other shell commands → allowed  
- Guest trap / timeout → fail-open (tool proceeds)
