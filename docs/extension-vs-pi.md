# Hyper WASM Extensions vs Pi Extensions — 完整度对照

| 日期 | 2026-08-04（post_tool_use 补齐） |
|------|----------------------------------|
| Hyper 参考 | bootstrap ABI + `xai-grok-extension-sdk` + plugin/marketplace |
| Pi 参考 | TypeScript `ExtensionAPI`（`pi.on` / `registerTool` / packages） |

---

## 1. 哲学对照

| | Pi | Hyper（当前） |
|--|-----|----------------|
| 默认产品 | 最小 harness，功能多靠扩展 | 全功能 TUI，扩展是增强层 |
| Guest | 同进程 TS 模块 | WASM（Rust SDK 推荐） |
| 分发 | packages / npm/git | plugin 目录 + marketplace |
| 作者语言 | TS 一等 | **Rust-first**（多语言后置） |

**结论：** 不是 1:1 复刻 Pi，而是 **Pi 的扩展纪律 + Hyper 的产品面**。

---

## 2. 能力矩阵

| 能力 | Pi | Hyper | 完整度 | 备注 |
|------|----|-------|:------:|------|
| 动态加载扩展 | ● | ● | **齐** | 装包即 load |
| session_start / end | ● | ● | **齐** | |
| before_agent_start inject | ● | ● | **齐** | system-reminder / append |
| tool_call gate (deny) | ● | ● | **齐** | pre_tool + capability |
| tool 后观察 | ● | ● | **齐** | WASM `post_tool_use` + shell/HTTP hooks；success/input/result preview |
| stop / 续跑门 | ◐ | ● | **齐/更强** | stop_gate + cap |
| registerTool | ● | ● | **齐（MVP）** | `wasm_*` ToolBridge |
| 自定义 slash 命令 | ● | ● | **齐（MVP）** | 声明式 `commands/` + WASM `register_command` / `#[hyper_command]` |
| 键盘 / TUI 扩展 | ● | ○ | **缺** | 需 Host UI API（notify/status/keybind） |
| 改 compaction 管道 | ● | ◐ | **半** | pre_compact observe only；无 rewrite |
| before LLM rewrite messages | ● | ○ | **缺** | 仅 inject（before_agent / before_model）；full rewrite 有意后置 |
| 主题 / prompt templates | ● | ◐ | **半** | 产品内置 themes/skills，非 WASM |
| Skills | ● | ● | **齐** | 声明式 SKILL.md |
| 信任 / 沙箱 | 弱 | ● | **更强** | trusted + capability + fail-closed 可选 |
| 官方作者 SDK | TS 原生 | ● Rust SDK | **齐（过程宏）** | `#[hyper_plugin]` + `#[hyper_hook(post_tool_use)]` |
| 热重载 | ● `/reload` | ● | **齐（MVP）** | `/plugins reload` 重建 runtime + tools/commands + ACU；消息含 wasm 计数 |
| 示例生态 | 50+ | 3+ SDK 例 | **弱** | template 已含 post_tool + `hello_wasm` command；持续加 |
| 多 agent 编排 | packages/脚本 | ● | **另轨** | **Rhai workflows**（非 ExtensionAPI） |

图例：● 有 · ◐ 部分 · ○ 无

---

## 3. 实现正确性（相对设计合同）

| 合同 | 状态 |
|------|------|
| loop 只 emit，不散落 wasmtime | **对**（session 调 runtime） |
| untrusted 不 load | **对** |
| capability 管 gate/inject/tools | **对** |
| hooks 先于 wasm | **对**（含 post_tool） |
| post_tool_use 为 observe、fail-open | **对**（2026-08-04 落地） |
| fail-open 默认 | **对**；fail-closed 可选 |
| Rust-first 作者路径 | **对**（SDK + 模板） |
| Component Model 已上线 | **否**（文档定为可选） |

---

## 4. 「全不全」结论

| 问题 | 答案 |
|------|------|
| 能否说「有了像 Pi 的扩展底座」？ | **能** — 动态 guest + 生命周期 + 工具注册 + post_tool 观察 + SDK |
| 能否说「功能面 = Pi」？ | **不能** — 缺 TUI/快捷键扩展、message rewrite、丰富示例 |
| 能否给第三方用？ | **能起步** — SDK + init + validate；DX 还可厚 |
| Host 是否实现错方向？ | **否** — 对齐设计；弱项在作者体验深度与 UI 扩展 |

---

## 5. 剩余缺口与补齐优先级（开发中，不打包）

| 优先级 | 缺口 | 说明 | 状态 |
|--------|------|------|------|
| P0 | WASM `post_tool_use` | 设计 MVP 第 4 事件 | **done** |
| P0 | WASM `register_command` | 动态 slash + ACP autocomplete | **done**（MVP：host turn 输出文本） |
| P1 | 更多示例 / 文档 | post_tool、hello_wasm command | 进行中 |
| P2 | 热重载 UX 对齐 Pi `/reload` | 已有 reload，可再厚 | 半 |
| P3 | Host UI API | notify / status bar / keybind | 未做（需 pager/ACP 通道） |
| P3 | before_model **rewrite** | 仅 inject；rewrite 要审计 | 有意后置 |
| P4 | Component Model / 多语言 | 触发条件见 roadmap | 未开 |
| — | Rhai 当 ExtensionAPI | **不做**；Rhai = workflow 编排轨 | 明确分工 |

---

## 6. 作者一句话（当前）

```text
#[hyper_plugin]
mod plugin {
    #[hyper_hook(pre_tool_use)] fn gate() -> i32 { … }
    #[hyper_hook(post_tool_use)] fn observe() -> i32 {
        if !tool_success() { log_warn(&tool_result_preview()); }
        0
    }
    #[hyper_hook(before_agent_start)] fn inject() -> i32 { … }
    #[hyper_tool(description = "…")] fn my_tool(args: &str) -> i32 { … }
    #[hyper_command(name = "hello_wasm", description = "…", argument_hint = "<name>")]
    fn hello_wasm(args: &str) -> i32 { tool_result("…"); allow() }
}
```

`plugin.json` capabilities 需包含 `register_command`；装包 trust 后 `/hello_wasm` 进入 host turn 输出。

`grok plugin init` → build → trust → 启用 / reload。

---

## 7. 相关文档

- [design-wasm-extensions.md](./design-wasm-extensions.md) — 设计合同  
- [extension-next-roadmap.md](./extension-next-roadmap.md) — 执行排期  
- [extension-production-checklist.md](./extension-production-checklist.md) — 生产清单  
