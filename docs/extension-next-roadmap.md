# WASM Extensions：已完成 vs 下一步

| 日期 | 2026-07-28 |
|------|------------|
| 原则 | **Rust-first + 厚 SDK + 声明宏**；bootstrap ABI 正式；Component Model **不急换** |

---

## 1. 已经做完的（不必再排）

### Host / 生命周期
- [x] wasmtime runtime、信任、capability  
- [x] session_start / end、pre_tool、before_agent、stop、pre_compact  
- [x] before_model **inject**（每轮 system-reminder）  
- [x] register_tool MVP → session-scoped `wasm_*` ToolBridge  
- [x] fail-closed（env + 每扩展 `runtime.gate_fail`）  
- [x] 会话保留 Store/Instance、epoch/fuel、tool 校验  

### DX（作者路径）
- [x] `xai-grok-extension-sdk` + **声明宏**（不做过程宏）  
- [x] `plugin init` / `plugin build` / `plugin validate --load`  
- [x] 模板 + path-guard / stop-once 示例  
- [x] `scripts/check-extensions.sh` + **GitHub CI** `extensions.yml`  
- [x] user-guide / 设计 / Oracle / vs-Pi 文档  

### 刻意不做 / 缓做
- before_model **rewrite** 整段 history  
- Component Model 硬切  
- 多语言一等公民  
- 完整 UI Host API（notify / status line）  

---

## 2. 成熟态（第三方可写扩展）

| 能力 | 状态 |
|------|------|
| 作者 SDK + 声明宏 | **done** |
| 一键 build | **`grok plugin build`** |
| init = SDK 模板 | **done** |
| runtime e2e + fixture wasm | **done** |
| CI 编 wasm | **done**（path-filtered） |
| 真 session e2e（整 shell） | 可选加深 |

**一句话：** 装 SDK → `hyper_extension!` → `grok plugin build --validate` → 启用 plugin。

---

## 3. 安排（按顺序）

### P0 — 作者 SDK — **done**
### P1 — 生态打磨 — **done**
### P1.5 — 声明宏 DX — **done**
### P1 收尾（第四阶段前） — **done this iteration**

| # | 事项 | 状态 |
|---|------|------|
| 11 | GitHub CI `check-extensions` | **done** |
| 11b | `grok plugin build`（替代独立 hyper-ext CLI） | **done** |

### Phase 4 — bootstrap 新能力 — **MVP closed**

| # | 事项 | 状态 |
|---|------|------|
| register_tool | MVP + session 命名 + 校验 | **done** |
| before_model inject | system-reminder | **done** |
| load-N budget test | N=5 软预算 | **done** |
| before_model rewrite | 安全面大 | **defer** |
| Component Model | 见 abi-strategy | **defer** |
| multi-lang / UI Host API | 触发条件未到 | **defer** |

### P2 — 可选大件（单独立项）

| # | 事项 | 触发条件 |
|---|------|----------|
| 12 | Component Model 双轨 | 多真实插件 + 类型需求 |
| 13 | before_model rewrite | 产品明确要 |
| 14 | 多语言 | Rust 路径已跑通后投诉 |
| 15 | UI Host API | ACP/pager 通道设计就绪 |

---

## 4. 给第三方的成熟态一句话

> `grok plugin init` → 写 `hyper_extension!` → `grok plugin build --validate` → 启用。  
> **不必**懂 wasmtime，**不必**过程宏，**不必**手写 ptr/len。

---

## 5. 本文件状态

- 排期：P0–P1.5 + Phase 4 bootstrap MVP **已完成**  
- 下一刀：仅在触发条件满足时开 P2（CM / rewrite / multi-lang / UI）  
