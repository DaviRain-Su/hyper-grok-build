# Hyper WASM Extensions vs Pi Extensions — 完整度对照

| 日期 | 2026-07-28 |
|------|------------|
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
| tool 后观察 | ● | ◐ | **半** | PostToolUse hooks 有；WASM 无独立 export |
| stop / 续跑门 | ◐ | ● | **齐/更强** | stop_gate + cap |
| registerTool | ● | ● | **齐（MVP）** | `wasm_*` ToolBridge |
| 自定义 slash 命令 | ● | ◐ | **半** | 声明式 commands/ 有；WASM 无 register_command |
| 键盘 / TUI 扩展 | ● | ○ | **缺** | 需 Host UI API |
| 改 compaction 管道 | ● | ◐ | **半** | pre_compact observe only |
| before LLM rewrite messages | ● | ○ | **缺** | 仅 inject；rewrite 未做 |
| 主题 / prompt templates | ● | ◐ | **半** | 产品内置 themes/skills，非 WASM |
| Skills | ● | ● | **齐** | 声明式 SKILL.md |
| 信任 / 沙箱 | 弱 | ● | **更强** | trusted + capability + fail-closed 可选 |
| 官方作者 SDK | TS 原生 | ● Rust SDK | **齐（初版）** | 无过程宏 CLI |
| 热重载 | ● `/reload` | ◐ | **半** | plugin reload 重建 runtime |
| 示例生态 | 50+ | 3+ SDK 例 | **弱** | 持续加 |

图例：● 有 · ◐ 部分 · ○ 无

---

## 3. 实现正确性（相对设计合同）

| 合同 | 状态 |
|------|------|
| loop 只 emit，不散落 wasmtime | **对**（session 调 runtime） |
| untrusted 不 load | **对** |
| capability 管 gate/inject/tools | **对** |
| hooks 先于 wasm | **对** |
| fail-open 默认 | **对**；fail-closed 可选 |
| Rust-first 作者路径 | **对**（SDK + 模板） |
| Component Model 已上线 | **否**（文档定为可选） |

---

## 4. 「全不全」结论

| 问题 | 答案 |
|------|------|
| 能否说「有了像 Pi 的扩展底座」？ | **能** — 动态 guest + 生命周期 + 工具注册 + SDK |
| 能否说「功能面 = Pi」？ | **不能** — 缺 TUI/快捷键扩展、message rewrite、丰富示例 |
| 能否给第三方用？ | **能起步** — SDK + init + validate；DX 还可厚 |
| Host 是否实现错方向？ | **否** — 对齐设计；弱项在作者体验深度与 UI 扩展 |

---

## 5. 建议 Oracle / 人审重点

1. 安全：fail-open 默认是否可接受；strict 路径是否够  
2. register_tool 命名/权限是否与 MCP 冲突  
3. SDK 抽象是否泄漏过多 ABI  
4. 与 Pi 的差异是否应在 README 写清「非功能对等」  
5. 下一步：过程宏 vs 更多示例 vs Component Model  

---

## 6. 下一步（执行中 / 待做）

见 [extension-next-roadmap.md](./extension-next-roadmap.md)。  
P0 SDK **已做**；P1 示例 + CI 脚本在本迭代补齐。  
