# WASM Extensions — 路线图（重排版 2026-07-28）

| 原则 | **Rust-first + 厚 SDK + 过程宏（属性）**；bootstrap ABI 正式；难项不插队 |
|------|------------------------------------------------------------------|
| 目标 | 把 **bootstrap MVP 推到可生产试点**；P2b/CM 等仍有触发条件再开 |

命名说明（避免和「设计 Phase 0–4」混）：

| 名称 | 含义 |
|------|------|
| **设计 Phase 0–4** | 功能面分期（lifecycle → DX → 可信 → 新能力）— **bootstrap MVP 已关** |
| **执行 P0–P4** | 工程排期：SDK → 生态 → **生产向** → **运营加深** → **远期难项** |

---

## 1. 已完成（不必再排）

### 设计 Phase 0–3.5 + Phase 4 bootstrap MVP
- Host 生命周期、gate/inject/stop、register_tool、before_model inject  
- 会话 Store 保留、epoch/fuel、fail-closed、session-scoped tools  
- SDK + 过程宏（`#[hyper_plugin]`）+ 声明宏兼容层、`plugin init/build/validate`、CI `extensions.yml`  

### 执行 P0–P2a（生产向易项）— **done**
- Session end 注销 `wasm_*`  
- Guest log + plugin_data_dir  
- Runtime metrics  
- ToolBridge 双会话 smoke  
- Production checklist  

---

## 2. 当前与后续排期

### P2 — 生产可试点 — **关闭**

| # | 事项 | 状态 |
|---|------|------|
| 16–21 | 清理 / log / metrics / data dir / bridge smoke / checklist | **done** |
| 23 | metrics **结构化上报**（session_start / reload / session_end） | **done this cut** |

**P2 出口标准（已满足）：** 第三方能 init→build→启用；多会话工具不泄漏；失败可观测。

### P3 — 运营加深（下一刀，易→中）

| # | 事项 | 状态 | 说明 |
|---|------|------|------|
| 30 | metrics 进 session 生命周期日志 | **done** | session_start / reload / session_end |
| 31 | deny 时同步打 metrics 摘要 | **done** | `tool_calls` warn + counters |
| 32 | `plugin details` 展示 runtime/cap/gate_fail | **done** | print_component_summary |
| 33 | init 默认 capabilities 与模板对齐 | **done** | +`register_tool` |
| 34 | SessionActor e2e（deny/allow/tools/stop/before_model/fail-closed/并发） | **done** | `wasm_extension_e2e_tests` |
| 35 | 指标 → product/dual telemetry 管道 | **done** | `wasm_extension_metrics` / `_blocked` |

### P3.5 — Pi 对照补齐（开发中，不打包）— **进行中**

| # | 事项 | 状态 | 说明 |
|---|------|------|------|
| 36 | WASM `post_tool_use` 全链路 | **done** | export + host `tool_success`/`tool_result_*` + shell 挂载 + SDK `#[hyper_hook(post_tool_use)]` |
| 37 | 对照文档刷新 | **done** | `extension-vs-pi.md` |
| 38 | 示例 / e2e 覆盖 post_tool | partial | runtime WAT 单测 + template 源码；checked-in wasm 可另编 |
| 39 | WASM `register_command` | pending | 声明式 commands/ 先顶；动态 slash 需 shell 挂点 |
| 39b | 热重载 UX 对齐 | pending | reload 已有，可再厚 |

### P4 — 远期难项（原 P2b / 设计 Phase 4 defer）— **有触发再开**

| # | 事项 | 触发条件 |
|---|------|----------|
| 40 | Component Model + wit-bindgen 双轨 | ≥3 真实插件 + 类型需求 |
| 41 | before_model **rewrite** | 产品明确要且有审计方案 |
| 42 | 多语言官方模板 | Rust 路径投诉高 / 企业强制 |
| 43 | 完整 UI Host API（notify/status/keybind） | ACP/pager 通道设计就绪 |
| 44 | 正式 release bench 与 SLA | 生产流量起来后 |
| 45 | compaction rewrite（非仅 observe） | 产品明确要且可回滚 |

---

## 3. 未在早期规划、但应继续推的（已并入 P2/P3）

| 项 | 来源 | 归入 |
|----|------|------|
| Session 工具 bridge 泄漏 | Oracle | P2 ✓ |
| Guest 可观测 log | WIT host.log 草案 | P2 ✓ |
| Ops counters | 生产 checklist | P2 ✓ |
| plugin data dir 只读 | store 草案小步 | P2 ✓ |
| Metrics 生命周期上报 | 运营 | P3 ✓ 本刀 |
| UI 列表 runtime 可见 | 运营 | P3 |
| 真 SessionActor e2e | 信心 | P3 optional |

---

## 4. 第三方一句话

> `grok plugin init` → `#[hyper_plugin]` 普通函数 → `grok plugin build --validate` → 启用 / `/plugins reload`。  
> 看日志：`RUST_LOG=wasm_extension=info`。

---

## 5. 本文件状态

- **现在：** **P2 + P3 关闭**；**P3.5 进行中**（`post_tool_use` 已落地）  
- **下一刀（不打包，持续开发）：** `register_command` 设计 / 示例 wasm 重编 / 试点真实插件  
- **P4：** 仍等触发条件  
- 清单： [extension-production-checklist.md](./extension-production-checklist.md)  
- 对照： [extension-vs-pi.md](./extension-vs-pi.md)
