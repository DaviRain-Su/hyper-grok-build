# Hypercore operations guide

Hyper is the **default agent turn path** as of P6. The legacy
`process_conversation_turn` loop remains only as a safety net.

Design background: [design-hypercore.md](design-hypercore.md).

## Environment variables

| Variable | Default | Meaning |
|----------|---------|---------|
| `HYPERCORE_TURN` / `GROK_HYPERCORE_TURN` | **on** | `0` / `false` / `no` forces legacy for every round |
| `HYPERCORE_TOOLS` / `GROK_HYPERCORE_TOOLS` | **on** | `0` disables the tool loop; with tools off, Hypercore is only used if plain is forced |
| `HYPERCORE_PLAIN` / `GROK_HYPERCORE_PLAIN` | off | `1` forces plain-text Hypercore (no tools in the model request) |

Aliases accept `1` / `true` / `yes` and `0` / `false` / `no` (case-insensitive).

### Examples

```bash
# Normal agent (Hypercore + tools)
hyper

# Full legacy (canary / bisect)
export HYPERCORE_TURN=0
hyper

# Plain Hypercore only (no tool schemas)
export HYPERCORE_TOOLS=0
export HYPERCORE_PLAIN=1
hyper
```

## Disk layout

```text
~/.grok/hypercore/<session_id>/
  snapshot.json          # schema_version 2 transcript
  terminals/<turn_id>.json   # idempotent terminal records
```

- **Authoritative chat** for the TUI / ACP remains `chat_state` / session store.
- Hypercore snapshot is a parallel restore + idempotency surface.
- Each **subagent** session has its own `session_id` directory.

## Telemetry / logs

Look for:

| Event / field | Meaning |
|---------------|---------|
| `shell.turn.path` unified log | `path=hypercore` or `path=legacy` (+ `reason`) |
| `hypercore turn: begin` | Round entered Hypercore (`is_subagent`, `parent`) |
| `hypercore turn: tools prepared` | Tool count advertised to the model |
| `hypercore turn: committed` | Core committed; `compact_rounds`, `tools`, `has_structured` |
| `agent round using legacy path` | Decision skipped Hypercore (`legacy_env_disabled`, …) |
| `falling back to legacy for this round` | Hypercore errored; same outer-loop round continues on legacy |

## Failure behavior

1. **Hypercore disabled** → legacy for the whole session (env).
2. **Hypercore error mid-round** → log warn + **legacy for that round only**.
3. **Context overflow mid-tools** → compact chat_state → `continue_turn_with_tools` (up to 3 restarts).
4. **Auth compact failure** → same reauth surface as legacy compact.

## What stays on shell (not Core)

- Memory dream, recap (`/btw`), laziness classifier  
- Full TUI / PTY / permission UI (executed via host `execute_tool_calls`)  
- Goal / stop-gate outer loop (shell wraps each Hypercore round)

## Rollback

```bash
export HYPERCORE_TURN=0
```

No binary rebuild required. Legacy code path is still compiled in (P6 does **not** feature-gate it away).
