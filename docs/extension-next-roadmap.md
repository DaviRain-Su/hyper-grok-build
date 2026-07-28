# WASM Extensions — 路线图（重排版 2026-07-28）

| 原则 | **Rust-first + 厚 SDK + 声明宏**；bootstrap ABI 正式；难项不插队 |
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
- SDK + 声明宏、`plugin init/build/validate`、CI `extensions.yml`  

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
| 34 | 整 SessionActor 级 e2e（可选） | optional | bridge smoke 已覆盖关键路径 |
| 35 | 指标 → mixpanel/外部 telemetry 管道 | optional | 有产品需求再接 |

### P4 — 远期难项（原 P2b / 设计 Phase 4 defer）— **有触发再开**

| # | 事项 | 触发条件 |
|---|------|----------|
| 40 | Component Model + wit-bindgen 双轨 | ≥3 真实插件 + 类型需求 |
| 41 | before_model **rewrite** | 产品明确要且有审计方案 |
| 42 | 多语言官方模板 | Rust 路径投诉高 / 企业强制 |
| 43 | 完整 UI Host API（notify/status） | ACP/pager 通道设计就绪 |
| 44 | 正式 release bench 与 SLA | 生产流量起来后 |

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

> `grok plugin init` → `hyper_extension!` → `grok plugin build --validate` → 启用。  
> 看日志：`RUST_LOG=wasm_extension=info`。

---

## 5. 本文件状态

- **现在：** **P2 关闭**；**P3 主线 30–33 关闭**（可选 34–35 仍开放）  
- **下一刀：** 仅可选加深或等触发开 P4  
- 清单： [extension-production-checklist.md](./extension-production-checklist.md)  
