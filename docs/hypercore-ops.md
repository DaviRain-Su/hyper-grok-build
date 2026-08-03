# Hypercore operations guide

Hypercore is an **experimental opt-in agent turn path**. The legacy
`process_conversation_turn` loop remains the default production path while
Hypercore correctness and parity work continues.

Design background: [design-hypercore.md](design-hypercore.md).

## Environment variables

| Variable | Default | Meaning |
|----------|---------|---------|
| `HYPERCORE_TURN` / `GROK_HYPERCORE_TURN` | **off** | Only `1` / `true` / `yes` / `on` enables Hypercore; missing, empty, unknown, or falsy values stay on legacy |
| `HYPERCORE_TOOLS` / `GROK_HYPERCORE_TOOLS` | **on once opted in** | `0` disables the tool loop; with tools off, Hypercore is only used if plain is forced |
| `HYPERCORE_PLAIN` / `GROK_HYPERCORE_PLAIN` | off | `1` forces plain-text Hypercore (no tools in the model request) after the turn gate is enabled |

`HYPERCORE_TURN` has priority whenever it is set. The `GROK_` alias is consulted
only when the primary variable is unset; all parsing is case-insensitive and
fail-closed.

### Examples

```bash
# Normal agent (safe default: legacy)
hyper

# Explicit Hypercore canary (tools enabled)
export HYPERCORE_TURN=1
hyper

# Plain Hypercore canary (no tool schemas)
export HYPERCORE_TURN=1
export HYPERCORE_TOOLS=0
export HYPERCORE_PLAIN=1
hyper
```

## Disk layout

```text
~/.grok/hypercore/<session_id>/
  state.v2.json              # authoritative atomic snapshot + terminals
  state.v2.lock              # cross-process exclusive lock (RMW)
  snapshot.json              # legacy transcript (never auto-deleted/modified)
  terminals/<sanitized>.json # legacy terminal records (never auto-deleted/modified)
```

- **Authoritative on-disk Hypercore state** is `state.v2.json` (`format_version: 2`):
  opaque snapshot bytes plus a `BTreeMap` of terminal records keyed by **raw**
  `turn_id` (map key must equal `record.turn_id`). New commits publish this
  single file under lock (unique temp → fsync → atomic replace → directory
  sync on Unix).
- **Snapshot legacy fallback**: read `snapshot.json` only when `state.v2.json`
  is **absent**. If v2 exists but is corrupt or has a wrong `format_version`,
  loads and commits **fail closed** (no snapshot fallback, no overwrite).
- **Terminal legacy fallback**: when v2 is absent, **or** when a valid v2 is
  present but the raw turn_id is missing from the map, read
  `terminals/<sanitize(turn_id)>.json`. The record is returned only if
  `record.turn_id` **exactly** equals the requested raw id (sanitize collisions
  yield “not found”; files are never deleted or modified). Corrupt /
  wrong-version v2 never falls back for terminals either.
- **Commit vs legacy**: inserting a new raw turn_id into the v2 map consults the
  matching legacy file under the session lock. Exact-id same record →
  idempotent promote; exact-id different content → conflict (snapshot does not
  advance); missing file or sanitize collision → free to insert; parse/I/O
  errors → fail closed.
- **Turn-id collisions**: v2 keys use the raw turn id, so ids that share a
  sanitize form (e.g. `a/b` vs `a?b`) no longer collide in the map. Session
  directory names and legacy terminal filenames still use path sanitization;
  those collisions are documented only and are **not** migrated in this layout.
- There is **no** automatic migration scan, dual-write to legacy, terminal GC,
  or deletion of orphan/legacy files (only the current commit’s temp file is
  cleaned up on failure).
- **Authoritative chat** for the TUI / ACP remains `chat_state` / session store.
- Hypercore snapshot is a parallel restore + idempotency surface.
- Each **subagent** session has its own `session_id` directory.

## Telemetry / logs

Look for:

| Event / field | Meaning |
|---------------|---------|
| `shell.turn.path` unified log | `path=hypercore` or `path=legacy` (+ `reason`) |
| `hypercore turn: begin` | Round entered Hypercore (`is_subagent`, `parent`, `core_turn_id`) |
| `hypercore turn: tools prepared` | Tool count advertised to the model |
| `hypercore turn: committed` | Core committed; `compact_rounds`, `tools`, `has_structured` |
| `agent round using legacy path` | Decision skipped Hypercore (`legacy_env_disabled`, `legacy_multimodal`, …) |

Path `reason` values include: `legacy_env_disabled`, `legacy_empty_prompt`,
`legacy_tools_off`, `legacy_multimodal`. There is **no**
`hypercore_error` same-round legacy fallback.

## Core turn ids

Outer goal/stop rounds use a stable unique Hypercore turn id:

```text
hc:{prompt_id_len}:{prompt_id}:r{n}           # outer round n (0, 1, …)
hc:{prompt_id_len}:{prompt_id}:r{n}:c{m}      # compact segment m (1, 2, …)
```

ACP `prompt_id` / client telemetry keep the original value; only the core
terminal / stream id uses the `hc:…` form. Same segment retries reuse the id.

## Failure behavior

1. **Hypercore not explicitly enabled** → legacy for the whole session (default).
2. **Path decision pre-routes** empty prompt, tools-off-without-plain, and any
   current/historical non-text user content (images) → legacy **before** Core.
3. **Once Hypercore is entered**, non-`Aborted` errors **propagate** to the
   caller. There is **no** same-round broad fallback to legacy.
4. **`ToolBatchResult::Abort`** (permission reject/cancel, overflow compact
   restart, abort short-circuit) → core restores full transcript checkpoint,
   does **not** write terminal/snapshot or bump `completed_turns`. Shell maps:
   - terminal side-channel → return that `TurnOutcome`
   - compact flag only → compact chat_state, continue on `:cN` segment
   - neither → explicit error (not a silent legacy replay)
5. **Context overflow mid-tools** → Abort aborted segment (no terminal) → compact
   → `continue_turn_with_tools` (up to 3 restarts); only the final continuation
   commits terminal.
6. **Auth compact failure** → same reauth surface as legacy compact.
7. **Same turn_id, different request text** → `TurnIdConflict` (no stream).

## What stays on shell (not Core)

- Memory dream, recap (`/btw`), laziness classifier  
- Full TUI / PTY / permission UI (executed via host `execute_tool_calls`)  
- Goal / stop-gate outer loop (shell wraps each Hypercore round)

## Rollback

Unset the opt-in or force it off:

```bash
unset HYPERCORE_TURN GROK_HYPERCORE_TURN
# or: export HYPERCORE_TURN=0
```

No binary rebuild is required. Legacy remains compiled in and is the default.
