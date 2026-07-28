# WASM Extensions：已完成 vs 下一步

| 日期 | 2026-07-28 |
|------|------------|
| 原则 | **Rust-first + 厚 SDK**；bootstrap ABI 正式；Component Model **不急换** |

---

## 1. 已经做完的（不必再排）

### Host / 生命周期
- [x] wasmtime runtime、信任、capability  
- [x] session_start / end、pre_tool、before_agent、stop、pre_compact  
- [x] before_model **inject**（每轮 system-reminder）  
- [x] register_tool MVP → `wasm_*` ToolBridge  
- [x] fail-closed 可选、`set_gate_reason`、Linker 缓存  

### DX（偏 Host）
- [x] `plugin validate` / `--load`  
- [x] `plugin init`（拷模板）  
- [x] user-guide、设计/审查文档、ABI 策略说明  
- [x] 裸 `extern "C"` 的 rust-guest-template  

### 刻意不做 / 缓做
- before_model **rewrite** 整段 history  
- Component Model 硬切  
- 多语言一等公民  
- 完整 Instance 会话复用  

---

## 2. 成熟还差什么（真正挡第三方的）

| 缺口 | 说明 |
|------|------|
| **作者 SDK** | 现在仍要手写 `hyper_host` / `no_mangle` |
| **一键 build 体验** | 文档有，未封装成 `sdk` 默认路径 |
| **SDK 示例 = 默认模板** | init 应生成「用 SDK 写的扩展」 |
| 真 session e2e | 仅有 runtime 级测 |
| CI 编 wasm | 可选 |

**结论：** Host 功能面已经够「能用」；**成熟体验 = SDK**。下面工作以 SDK 为 P0。

---

## 3. 安排（按顺序做）

### P0 — 作者 SDK（当前冲刺）

| # | 事项 | 产出 | 状态 |
|---|------|------|------|
| 1 | crate `xai-grok-extension-sdk` | host 封装 + allow/deny/inject/tool helper | **done** |
| 2 | `extension_boilerplate!` 宏 | abi + session_start/end | **done** |
| 3 | 模板迁到 SDK | rust-guest-template 依赖 SDK | **done** |
| 4 | `plugin init` 指向 SDK | Cargo.toml path 到 monorepo SDK | **done** |
| 5 | user-guide 默认 SDK | 31-wasm-extensions 已改 | **done** |

### P1 — 生态打磨（SDK 之后）

| # | 事项 | 状态 |
|---|------|------|
| 6 | SDK 示例 path-guard / stop-once（+ 模板 echo tool） | **done** |
| 7 | runtime e2e（template + 两示例） | **done** |
| 8 | `scripts/check-extensions.sh` 编 wasm + test | **done** |
| 9 | Pi 对照矩阵 `docs/extension-vs-pi.md` | **done** |
| 10 | `/plugins` UI has_runtime | 已有 |
| 11 | GitHub CI 挂 check-extensions | 可选 |
| 12 | Oracle / 人审 vs Pi | 本迭代 |

### P2 — 可选大件（单独立项，不插队）

| # | 事项 | 触发条件 |
|---|------|----------|
| 10 | 过程宏 `#[extension]` / `#[tool]` | SDK 手写 helper 仍啰嗦 |
| 11 | `hyper-ext` CLI（build/pack） | init 不够用 |
| 12 | Component Model 双轨 | 见 abi-strategy |
| 13 | before_model rewrite | 产品明确要 |
| 14 | 多语言 | Rust 路径已跑通后 |

---

## 4. 给第三方的成熟态一句话

> 装 SDK → 写几个 `fn` → `cargo build --target wasm32-unknown-unknown` → `plugin validate --load` → 丢进 marketplace。  
> **不必**懂 wasmtime，**不必**手写 ptr/len（由 SDK 藏起来）。

---

## 5. 本文件状态

- 排期：生效  
- 执行：从 §3 P0 开始（SDK crate）  
