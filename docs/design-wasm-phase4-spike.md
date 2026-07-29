# Spike notes: WASM Extensions Phase 4+

| 项 | 内容 |
|----|------|
| 状态 | **bootstrap Phase 4 MVP 关闭**；Component Model / rewrite / multi-lang / UI 仍 defer |
| 前提 | Phase 0–3.5 + SDK 过程宏（`#[hyper_plugin]`）+ `plugin build` / CI |
| 日期 | 2026-07-28 |

## 1. Component Model + WIT — **defer**

**目标：** 用 `hyper:extension@0.1.0` WIT 替换 core-wasm `hyper_ext_*` 导出。

| 步骤 | 内容 |
|------|------|
| 1 | 锁定 `wit/extension.wit` 导出（init / on-*） |
| 2 | Host：`wasmtime::component` + `bindgen!` |
| 3 | Guest：`wit-bindgen` Rust cdylib |
| 4 | 双载：bootstrap 与 component 并存一个版本 |
| 5 | 弃用 bootstrap 导出名 |

**不做：** 第一期就砍掉 bootstrap。  
**触发：** 见 [design-wasm-abi-strategy.md](./design-wasm-abi-strategy.md)。

## 2. `register_tool` — MVP **已实现**（bootstrap ABI）

```text
capability: register_tool
exports:
  hyper_ext_tool_count() -> i32
  hyper_ext_describe_tool()  # host sets tool_index; guest set_tool_name/desc/schema
  hyper_ext_invoke_tool()    # host sets tool_name + tool_input; guest set_tool_result
client name: wasm_{session12}_{extension}_{name}  (session-scoped on shared bridge)
session: sync on plugin reload + session_start → ToolBridge
validation: name / JSON Schema object / per-ext uniqueness
```

重工具仍走 MCP。Component Model 后再统一 schema 类型。

## 3. `before_model` inject — **已实现**；rewrite — **defer**

| 模式 | 状态 |
|------|------|
| inject（system-reminder） | **done**（capability `before_model_inject`） |
| rewrite 消息数组 | **defer** — 需 `rewrite_context` + 严格审计，禁止无界删历史默认开启 |

## 4. Multi-language — **defer**

- 官方只维护 **Rust** 模板与 CI  
- 其它语言：社区文档「如何产出同 ABI 的 wasm」  
- Component 后：`componentize-js` 等再评估  

## 5. UI Host API / store

| 切片 | 状态 |
|------|------|
| Guest → host **`log`**（tracing `wasm_extension`） | **done**（bootstrap 生产可观测） |
| `notify` / status line → ACP / pager | **defer** |
| guest 持久化 → host 写 `GROK_PLUGIN_DATA` | **defer** |

## 6. Load budget — **soft CI test done**

- `load_five_minimal_guests_under_budget`：N=5 加载 + session_start  
- Debug 软上限 10s / 5s；release 目标仍为 N=5 < ~100ms 量级（本机可 `cargo test … -- --nocapture` 看 eprintln）  

## 7. 建议触发条件（P2）

| 能力 | 何时开 |
|------|--------|
| Component Model | bootstrap 稳定 + 外部作者 > 3 个真实插件 |
| before_model rewrite | 有 compaction/trim 产品需求 |
| multi-lang | Rust 路径投诉量高或企业强制 |
| UI Host API | pager/ACP 通道设计就绪 |
