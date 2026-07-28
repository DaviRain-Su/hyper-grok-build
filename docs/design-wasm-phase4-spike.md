# Spike notes: WASM Extensions Phase 4+

| 项 | 内容 |
|----|------|
| 状态 | **register_tool MVP 已落地**；Component Model / before_model 仍 spike |
| 前提 | Phase 0–3.5 bootstrap 可用；Rust-first 作者路径 |
| 日期 | 2026-07-28 |

## 1. Component Model + WIT

**目标：** 用 `hyper:extension@0.1.0` WIT 替换 core-wasm `hyper_ext_*` 导出。

| 步骤 | 内容 |
|------|------|
| 1 | 锁定 `wit/extension.wit` 导出（init / on-*） |
| 2 | Host：`wasmtime::component` + `bindgen!` |
| 3 | Guest：`wit-bindgen` Rust cdylib |
| 4 | 双载：bootstrap 与 component 并存一个版本 |
| 5 | 弃用 bootstrap 导出名 |

**不做：** 第一期就砍掉 bootstrap。

## 2. `register_tool` — MVP **已实现**（bootstrap ABI）

```text
capability: register_tool
exports:
  hyper_ext_tool_count() -> i32
  hyper_ext_describe_tool()  # host sets tool_index; guest set_tool_name/desc/schema
  hyper_ext_invoke_tool()    # host sets tool_name + tool_input; guest set_tool_result
client name: wasm_{extension}_{name}
session: sync on plugin reload + session_start → ToolBridge.register_mcp_tools
```

重工具仍走 MCP。Component Model 后再统一 schema 类型。

## 3. `before_model` rewrite

- 事件：发 LLM 前的消息视图  
- 返回：allow 改写的摘要/注入（**禁止**无界删除历史默认开启）  
- 需新 capability `rewrite_context` + 严格审计  

## 4. Multi-language

- 官方只维护 **Rust** 模板与 CI  
- 其它语言：社区文档「如何产出同 ABI 的 wasm」  
- Component 后：`componentize-js` 等再评估  

## 5. UI Host API / store

- `notify` / status line → ACP 扩展或 pager channel  
- guest 持久化 → 仅经 host 写 `GROK_PLUGIN_DATA`  

## 6. 建议触发条件

| 能力 | 何时开 |
|------|--------|
| Component Model | bootstrap 稳定 + 外部作者 > 3 个真实插件 |
| register_tool | MCP 不够用的明确案例 |
| before_model | 有 compaction/trim 产品需求 |
| multi-lang | Rust 路径投诉量高或企业强制 |
