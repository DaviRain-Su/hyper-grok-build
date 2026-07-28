# Oracle review: WASM extensions vs Pi (2026-07-28)

Full Oracle verdict: **ship-with-fixes**.

Source: oracle subagent session (read-only deep review).

## Verdict

Directionally correct Pi-like model (dynamic guest, lifecycle, tools, trust).  
Not production-complete for multi-session `register_tool` or stateful lifecycle.

## Critical / High (acted on this iteration where marked)

| # | Finding | Action |
|---|---------|--------|
| 1 | Global `unregister_tools_by_prefix("wasm_")` cross-session unsafe | **Fixed:** session-owned name list + `unregister_tool_by_name` only |
| 2 | Tools not synced at first session | **Already wired** at `DispatchSessionStartHook` + reload; comment clarified |
| 3 | Stateless instance per call | **Fixed:** session-retained Store/Instance (guest globals persist) |
| 4 | No memory limits | **Fixed:** module size cap + `StoreLimits` memory/tables |
| 5 | fail-closed coarse | **Fixed:** env default + per-extension `runtime.gate_fail` |
| 6 | Coarse trust | Unchanged; product policy |
| 7 | register_tool validation weak | **Fixed:** name/schema/uniqueness checks; session-scoped client names |
| 8 | WIT marketed as live | ABI strategy doc already says bootstrap is real API |
| 9 | Missing lifecycle inputs | Deferred |
| 10 | discovery misses wasm-only dirs | **Fixed:** convention `extension.wasm` counts as component |
| 11 | SDK not crates.io | Expected; monorepo path for now |

## Completeness vs Pi (Oracle-aligned)

See [extension-vs-pi.md](./extension-vs-pi.md). Headline: **extension base yes; full Pi parity no**.

## Second Oracle pass (post P2/P3) — ship-with-fixes acted on

| Finding | Action |
|---------|--------|
| Reload skipped lifecycle | **Fixed:** end → rebuild → start → tools |
| UTF-8 invoke truncate panic | **Fixed:** reject `PayloadTooLarge` |
| Cancel detach guest | **Fixed:** `EpochCancelGuard` Drop increments epoch |
| Free-form deny telemetry | **Fixed:** `category` only (`explicit_deny` / fail_closed) |
| plugin_data_dir overclaim | **Docs:** path metadata, no FS access |
| Invoke unadvertised tools | **Fixed:** advertised set after collect |

## Follow-up coverage (after ship-with-fixes)

| Item | Status |
|------|--------|
| Concurrent dual-session tools on one bridge | **done** (`concurrent_sessions_share_bridge_without_cross_unregister`) |
| `run_stop_gate` + stop-once fixture | **done** |
| before_model inject + system-reminder | **done** |
| fail-closed trap via `prepare_tool_call` | **done** |

## Remaining deferred

1. Publishable SDK on crates.io — monorepo path for now  
2. Component Model / history rewrite / multi-language / full UI Host API  



## Verification

```bash
./scripts/check-extensions.sh
cargo test -p xai-grok-extension-api --lib
cargo test -p xai-grok-extension-runtime --lib
cargo check -p xai-grok-shell -p xai-grok-agent
```
