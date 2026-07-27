# WASM Extensions

WASM extensions let you change agent behavior **without rebuilding Hyper**:
dynamic load of a guest module that hooks into session lifecycle events
(similar in spirit to [Pi](https://pi.dev/) TypeScript extensions; guest
format is WebAssembly because Hyper is a Rust host).

Design reference: [design-wasm-extensions.md](../../../../docs/design-wasm-extensions.md).

---

## When to use what

| Approach | Best for | Runs as |
|----------|----------|---------|
| **Skills / commands** | Teach the model a procedure | Prompt text |
| **Hooks** (shell/HTTP) | Audit, simple gates, integrations | Subprocess / HTTP |
| **MCP** | Heavy tools, external systems | Separate server |
| **WASM extension** | Fast in-process gates, inject context, stop gates | Wasmtime guest |

Prefer hooks for one-off scripts. Prefer WASM when you need low latency, shared
logic packaged with a plugin, or deterministic policy without shelling out.

---

## Plugin layout

```text
my-ext/
  plugin.json          # name + optional runtime block
  extension.wasm       # compiled guest (or path from runtime.wasm)
  skills/ …            # optional declarative components
  hooks/ …             # optional
```

Minimal `plugin.json`:

```json
{
  "name": "safe-shell-wasm",
  "version": "0.1.0",
  "description": "Deny tool inputs containing rm -rf",
  "runtime": {
    "wasm": "extension.wasm",
    "wit": "hyper:extension@0.1.0",
    "capabilities": ["pre_tool_gate"]
  }
}
```

### Capabilities

| Capability | Effect |
|------------|--------|
| *(none)* | Observe-only if handlers exist; gate/inject results ignored |
| `pre_tool_gate` | `pre_tool_use` may **deny** |
| `before_agent_inject` | May inject context / append system notes before the agent loop |
| `stop_gate` | May **block** turn completion (force another round) |

Without a `runtime` block, a root-level `extension.wasm` is still discovered,
but with **no capabilities**.

---

## Trust and load rules

- Only **enabled + trusted** plugins load WASM (same trust model as hooks/MCP).
- User plugins under `~/.grok/plugins/` are auto-trusted.
- Project plugins need folder trust / install `--trust`.
- Guest traps and timeouts are **fail-open** (do not crash the session).

---

## Lifecycle hooks (guest exports)

Core-wasm bootstrap ABI (Phase 0–3). Optional exports may be missing.

| Export | Return | Role |
|--------|--------|------|
| `hyper_ext_abi_version` | `i32` = `1` | Required |
| `hyper_ext_on_session_start` | `0` ok | Required at load |
| `hyper_ext_on_session_end` | `0` | Optional |
| `hyper_ext_on_pre_tool_use` | `0` allow / `1` deny | Needs `pre_tool_gate` |
| `hyper_ext_on_before_agent_start` | `0` | Needs `before_agent_inject` |
| `hyper_ext_on_stop` | `0` allow stop / `1` block | Needs `stop_gate` |
| `hyper_ext_on_pre_compact` | `0` | Observe only |

### Host imports (`hyper_host`)

| Import | Use |
|--------|-----|
| `tool_name_len` / `tool_name_byte` | Current tool name |
| `input_len` / `input_byte` | Tool input JSON |
| `prompt_len` / `prompt_byte` | User prompt (`before_agent_start`) |
| `set_inject_context(ptr,len)` | Write inject string from guest `memory` |
| `set_append_system(ptr,len)` | Write append-system string |
| `stop_hook_active` | Whether stop already continued this turn |
| `compact_reason_len` / `compact_reason_byte` | Compaction trigger |

Injected text is applied as a **system-reminder** (inject) or
`<system-extension>` (append)—not a full rewrite of the durable system prompt.

Order relative to classic hooks: **shell/HTTP hooks first, then WASM**.

---

## Build a guest

Example (WebAssembly Text):

See
[`xai-grok-extension-runtime/examples/safe-shell-plugin/`](../../../xai-grok-extension-runtime/examples/safe-shell-plugin/).

```bash
wat2wasm extension.wat -o extension.wasm
```

Future: Component Model + WIT (`hyper:extension@0.1.0`) will replace the
bootstrap ABI without changing the plugin packaging model.

---

## Install and enable

```bash
mkdir -p ~/.grok/plugins/safe-shell-wasm
cp plugin.json extension.wasm ~/.grok/plugins/safe-shell-wasm/

# ~/.grok/config.toml
[plugins]
enabled = ["safe-shell-wasm"]
```

Reload plugins in the TUI (`/plugins` → `r`) or start a new session.

Validate before sharing:

```bash
grok plugin validate ./safe-shell-wasm
```

Checks name, runtime path, `\0asm` magic, WIT compatibility, and known
capabilities.

---

## Security notes

- Guests cannot open files, network, or shells unless the host later adds
  capability-gated imports.
- Treat third-party WASM like any other trusted plugin code.
- Prefer pinned marketplace commits when distributing (`require_sha`).

---

## See also

- [Plugins](09-plugins.md)
- [Hooks](10-hooks.md)
- [MCP servers](07-mcp-servers.md)
- [Design: WASM Extensions](../../../../docs/design-wasm-extensions.md)
