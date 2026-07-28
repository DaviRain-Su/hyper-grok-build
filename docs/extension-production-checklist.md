# WASM Extensions — production checklist

Push the **bootstrap MVP** toward production without waiting on hard Phase 4
(defer: Component Model, history rewrite, multi-lang, full UI Host API).

| 日期 | 2026-07-28 |
|------|------------|

## Author path (must work)

- [x] `grok plugin init` → SDK template + `hyper_extension!`
- [x] `grok plugin build . --validate` → `extension.wasm` + ABI load
- [x] Path-filtered CI: `.github/workflows/extensions.yml`
- [x] user-guide `31-wasm-extensions.md`

## Runtime safety (must hold)

- [x] Only **enabled + trusted** plugins load WASM
- [x] Capability-gated effects (deny/inject/stop/tools)
- [x] Fail-open default; env + per-ext `runtime.gate_fail: "closed"`
- [x] Memory limits + module size cap + fuel + epoch timeout
- [x] Session-scoped tool names + session-owned unregister
- [x] **Session end unregisters `wasm_*` tools** (no bridge leak)
- [x] Tool name / schema / uniqueness validation

## Observability (shipped light)

- [x] Host `tracing` on load skip / trap / deny
- [x] Guest → host **`hyper_host.log`** / SDK `log_info!` path (`log_info`, …)
- [ ] Full UI Host API (status line / ACP notify) — **defer**

## Ops / rollout

1. Enable a pilot plugin under `~/.grok/plugins/` with explicit `enabled`.
2. Prefer `gate_fail: "closed"` only for security-critical gates after soak.
3. Watch logs: `RUST_LOG=wasm_extension=info,xai_grok_extension_runtime=info`.
4. On session close, confirm no leftover `wasm_*` tools in multi-session hosts.
5. Rebuild guests after SDK bumps: `grok plugin build --validate`.

## Still deferred (hard P4)

| Item | Why wait |
|------|----------|
| Component Model / WIT bindgen | ABI freeze cost; bootstrap is formal |
| before_model rewrite | Unbounded history mutation risk |
| Multi-language templates | Rust path must stay green first |
| Full UI Host API | Needs ACP/pager channel design |

## Smoke commands

```bash
./scripts/check-extensions.sh
cargo test -p xai-grok-extension-runtime --lib
cargo check -p xai-grok-shell -p xai-grok-pager
```
