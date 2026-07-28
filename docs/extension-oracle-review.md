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
| 3 | Stateless instance per call | **Documented** as intentional bootstrap; full Pi state deferred |
| 4 | No memory limits | **Fixed:** module size cap + `StoreLimits` memory/tables |
| 5 | fail-closed coarse | Env-level remains; per-extension later |
| 6 | Coarse trust | Unchanged; product policy |
| 7 | register_tool validation weak | Partial (no silent shared `wasm_tool` id) |
| 8 | WIT marketed as live | ABI strategy doc already says bootstrap is real API |
| 9 | Missing lifecycle inputs | Deferred |
| 10 | discovery misses wasm-only dirs | **Fixed:** convention `extension.wasm` counts as component |
| 11 | SDK not crates.io | Expected; monorepo path for now |

## Completeness vs Pi (Oracle-aligned)

See [extension-vs-pi.md](./extension-vs-pi.md). Headline: **extension base yes; full Pi parity no**.

## Remaining after this fix batch

1. Session-local ToolBridge or unique client names under multi-session contention  
2. Optional per-session Store retention for state  
3. Epoch interrupt / cancel spawn_blocking after timeout  
4. Per-extension fail-closed in manifest  
5. Publishable SDK + custom session_start without replacing boilerplate  

## Verification

```bash
./scripts/check-extensions.sh
cargo test -p xai-grok-extension-runtime --lib
cargo check -p xai-grok-shell
```
