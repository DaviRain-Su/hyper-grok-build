# Code Agent 竞品分析（Hyper 整合视角）

| 项 | 内容 |
|----|------|
| 状态 | 分析文档（非实现规格） |
| 受众 | 个人 power user（终端重度、多 provider、重视 harness 锐度） |
| 产品 | **Hyper** — Grok Build 多 provider 社区版 |
| 初稿日期 | 2026-07-21 |
| 最近更新 | 2026-07-23 — 补 Pi / Nanocodex 画像、吸收原则与看板；对齐 Modes 缓停 / Oracle 主线 |
| 维护 | 能力矩阵与竞品特性会过时；落地功能后请回写「Hyper 基线 / 矩阵 / 吸收看板」列 |

---

## 目录

1. [执行摘要](#1-执行摘要)
2. [Hyper 能力基线](#2-hyper-能力基线)
3. [竞品深度画像](#3-竞品深度画像)
4. [其他 agent 速览](#4-其他-agent-速览)
5. [总能力矩阵](#5-总能力矩阵)
6. [按失败模式映射功能](#6-按失败模式映射功能)
7. [个人 power user 优先级建议](#7-个人-power-user-优先级建议)
8. [战略定位与吸收原则](#8-战略定位与吸收原则)
9. [吸收看板](#9-吸收看板)
10. [参考链接](#10-参考链接)

---

## 1. 执行摘要

Hyper 已经站在「一流终端 coding agent」梯队：hashline 编辑、LSP、subagents/personas、plan mode、MCP/skills/hooks、多 provider、sandbox 都有。竞品的价值不在「有没有 agent」，而在几类**可学习的差异**：

| 类型 | 代表 | 对 power user 的意义 |
|------|------|----------------------|
| **Harness 锐度** | omp | 编辑 / 调试 / 纠偏 / 模型路由更「打得准、花得少」 |
| **角色与线程产品化** | Amp | Oracle / Librarian、effort 模式、可分享 thread 的心智 |
| **大规模自治与 SDLC** | Factory Droid | Missions、Readiness、平台化——个人用户可后置 |
| **最小可扩展 harness** | Pi | 小核心 + Extensions/Skills/packages；不 fork 也能长能力 |
| **嵌入式 + Code Mode** | Nanocodex | 模型–harness 共设计、Turn/steer/fork、一次 cell 内编排多 tool |

**目标不是「功能博物馆」。** 吸收的是失败模式的解法，落到 Hyper 已有表面（config / sampler / tools / plugin），见 [§8](#8-战略定位与吸收原则) 与 [§9](#9-吸收看板)。

**建议学习 / 对照顺序（分析结论，非排期）：**

1. **omp** 的 harness 细节（DAP、stream rules、advisor、role 路由、typed subagent、ast）
2. **Amp** 的特化角色 UX + change review（**四档 Modes 对 Hyper 缓停**，见 design-modes §0）
3. **Droid** 的 readiness / missions 思路（按需、轻量复刻）
4. **Pi** 的扩展边界与 session/compaction 教科书结构
5. **Nanocodex** 的 Turn 生命周期、Code Mode、历史 fork（研究项）
6. **Claude Code / Codex / Aider** 的默认体验与 git 工作流细节

**当前 Hyper 主线（相对本文排期）：** Oracle 升级 → Diff review / Dashboard；Modes 不排期。详见 [design-oracle.md](./design-oracle.md)。

---

## 2. Hyper 能力基线

### 2.1 产品形态

- 全屏 TUI agent（`hyper`）
- Headless / CI（`hyper -p`）
- ACP 嵌入 IDE
- 配置与凭证：`~/.grok`；二进制：`~/.hyper`

详见根目录 [README.md](../README.md) 与
[user-guide](../crates/codegen/xai-grok-pager/docs/user-guide/)。

### 2.2 已具备的关键能力（与竞品重叠大）

| 域 | 能力 | 仓库线索 |
|----|------|----------|
| 模型 | 多平台 registry + BYOK | `xai-grok-models`；user-guide 11、25–28 |
| 文件 | read / search_replace / list / grep | `xai-grok-tools` |
| 可靠编辑 | **hashline** 锚点 read / edit / grep | `grok_build_hashline/` |
| 智能 | **LSP** tool | `implementations/lsp`、`grok_build/lsp` |
| 执行 | bash、background、monitor、scheduler | user-guide 20 |
| 网络 | web_search / web_fetch | tools |
| 多代理 | spawn_subagent、explore / plan、personas、worktree | user-guide 16 |
| 规划 | plan mode（只写 plan.md） | user-guide 19 |
| 记忆 | 跨 session memory（实验，默认关） | user-guide 13 |
| 扩展 | MCP、Skills、Plugins、Hooks、AGENTS.md | user-guide 7–12 |
| 安全 | sandbox、permissions、safe-bash | user-guide 18、22 |
| 工程 | codebase-graph、hunk-tracker、compaction | 对应 crates |

### 2.3 相对空白（分析结论）

- 真 **DAP 调试** 工具
- **流式中途规则注入**（time-traveling stream rules）
- **Advisor** 旁路双模型评审（与按需 Oracle 不同）
- ~~Effort / Role 产品化四档（Amp low–ultra）~~ → **缓停**：现有 config 已够自定义；见 [design-modes.md](./design-modes.md) §0
- **Oracle 触发 + 强模型钉定**（类型已有，但常同模型继承 / 主 agent 不自觉调用）→ [design-oracle.md](./design-oracle.md)
- **Change accept / reject** 统一审阅面
- **Missions / Readiness** 级仓库自治产品
- **Collab / 会话分享**
- **AST 结构编辑**（ast-grep 级）
- **Eval 内核**（持久 Python / JS + tool re-entry）
- **Code Mode**（模型写脚本编排 tools，Nanocodex 标杆）— 研究项
- **Turn steer / 便宜历史 fork** 产品化（Nanocodex；Hyper 有 interjection / subagent，体验未对齐）
- ~~Typed subagent I/O~~ → workflow/`output_schema` **已有**，矩阵回写为 ●（验证后可再标 ★）

---

## 3. 竞品深度画像

### 3.1 omp — Oh My Pi

**一句话：** 开源「把 IDE 焊进终端」的 harness；和 Hyper 技术气质最接近。

| 项 | 内容 |
|----|------|
| 站点 | [omp.sh](https://omp.sh/) |
| 源码 | [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi)（MIT） |
| 栈 | Rust 核心 + TypeScript 扩展 / Bun |
| 规模宣称 | ~55k LOC Rust；32 tools；14 LSP ops；28 DAP ops；40+ providers |
| 血统 | [Pi](https://github.com/earendil-works/pi) 的 batteries-included 路线（原 pi-mono 生态） |

**标志性能力（对 Hyper 最有启发）：**

1. **Hashline edits** — 内容哈希锚点，消灭空白 / 错位 diff（Hyper 已有同类）
2. **LSP 深度接入** — rename 走 `workspace/willRenameFiles`，barrel / re-export 一起更新
3. **DAP** — lldb / dlv / debugpy 真调试，不是 print
4. **Time-traveling stream rules** — 规则平时不进 context；regex 命中则 abort 流、注入 reminder、同点重试；compaction 后仍存活
5. **Typed subagents** — `task` fan-out + worktree + **schema-validated** 返回
6. **Advisor** — 第二模型旁观每步，concern / blocker
7. **Eval** — 持久 Python + Bun；内核内可回调 agent tools
8. **ast_edit / ast_grep** — 结构改动 + proposed → resolve 两阶段
9. **Model roles** — default / smol / slow / plan / commit + fallback chains + path-scoped models + multi-key 轮询
10. **`/collab`** — 实时协作 / 只读观战 + QR
11. **web_search** — 25+ 后端 + 站点感知抽取（arxiv / npm / github…）
12. **Magic keywords** — `ultrathink` / `orchestrate` / `workflowz`
13. **内部 scheme** — `conflict://`、`xd://` 发现冷门工具

**对 power user 的启示：** 优先学「让弱模型也打得准」的 harness（edit format、LSP、DAP、stream rules），以及「贵模型只花在刀刃上」的 role / pin 路由。协作 / collab 可后置。Hyper 侧用 **Oracle pin + 触发** 部分覆盖 role 路由，不必整包 Modes。

---

### 3.2 Amp Code

**一句话：** 意见鲜明的 frontier agent；多模型编排 + 线程分享 + 特化子代理。

| 项 | 内容 |
|----|------|
| 站点 | [ampcode.com](https://ampcode.com/) |
| 手册 | [Owner’s Manual](https://ampcode.com/manual) |
| 形态 | CLI + IDE 连接 + Web；threads 云端 |
| 原则 | 不限 token 心态、永远好模型、raw power、随模型演进少包袱 |
| 模式 | **low / medium / high / ultra**（Hyper：**缓停**抄整表，见 design-modes §0） |

**产品子系统：**

- **Oracle** — 深度设计 / 评审（Hyper 当前主线对标点）
- **Librarian** — 代码库 / 文档检索顾问
- **Painter** — 视觉 / UI 向特化
- **Code Review** — 可组合的评审 agent
- **Orbs** — 并行轻量工作单元（产品包装）
- **Runners / Changes Workflow** — 执行与改动审阅
- **Thread Sharing / Remote Control / Slack**
- **Plugins** — 自定义 mode / subagent / 权限

**对 power user 的启示：**

- 把 **Librarian / Oracle / Reviewer** 做成内置一等公民，而不是只靠用户自写 persona
- **Thread = 任务边界** 的习惯与 UI（一任务一线程）
- **看 agent 改了什么** 的 Changes 面
- effort 四档是「默认搭配 UX」；Hyper 已有细配能力时，**优先钉死 Oracle 强模型 + 可感知触发**，而不是先建 mode 解析层

**约束：** 闭源；可借体验，不借实现。Hyper 可在「本地优先 + 可审 diff」上更符合 power user。

---

### 3.3 Factory Droid

**一句话：** Agent-native SDLC 平台；harness 调优 + 企业工作流。

| 项 | 内容 |
|----|------|
| 站点 | [factory.ai](https://factory.ai/) |
| 文档索引 | [docs.factory.ai/llms.txt](https://docs.factory.ai/llms.txt) |
| 形态 | Droid CLI、Factory App、**droid exec** headless、Cloud、Slack / Linear / IDE (ACP) |
| 特化 | Code / Knowledge / Reliability 等 Droid |
| 大招 | **Missions**、**Agent Readiness**、Automations、Automated Review / QA / Security / IR |

**Missions 工作流（概要）：**

1. `/missions` 进入
2. 协作澄清目标 → 功能 + 里程碑计划
3. 绑定 / 生成 skills
4. Mission Control 编排执行
5. 用户可介入；强调仓库需 scriptable QA（Readiness 达标）

**对 power user 的启示：**

- **Readiness**：一键诊断「这仓库 agent 能不能自治」（build / test / AGENTS.md / 可脚本验收）
- **Missions 轻量版**：超大任务 = 计划 + 里程碑 + 验收，而不是无限长单 session
- Autonomy levels 与 Hyper permissions 可概念映射
- 完整企业集成对个人可缓

---

### 3.4 Pi — minimal agent harness

**一句话：** 刻意做小的终端 coding harness；扩展优先于内置功能堆叠。与 omp「焊满 IDE」形成对照。

| 项 | 内容 |
|----|------|
| 站点 | [pi.dev](https://pi.dev/) |
| 源码 | [earendil-works/pi](https://github.com/earendil-works/pi)（MIT；原 badlogic/pi-mono 迁至 Earendil） |
| 栈 | TypeScript monorepo（`pi-ai` / `pi-agent-core` / `pi-coding-agent` / `pi-tui`） |
| 默认工具 | 极简：`read` / `write` / `edit` / `bash`（能力靠扩展补） |
| 扩展 | TypeScript Extensions、Skills、prompt templates、themes、**Pi packages**（npm/git 分享） |
| 形态 | interactive、print/JSON、RPC、SDK 嵌入 |
| 刻意不做 | 内置 subagents、plan mode 等——留给扩展或用户自建 |

**标志性能力 / 哲学：**

1. **最小模型面** — 默认四工具，降低 prompt 与权限表面积
2. **扩展不改核心** — 用 package 分享工作流，而不是 fork harness
3. **分层清晰** — unified LLM API → agent loop/session/compaction → coding CLI/TUI
4. **Session tree / branching / compaction** — harness 文档对 turn snapshot vs config 边界讲得很清
5. **权限默认跟用户进程** — 无内置细粒度 sandbox；文档推荐 Docker / micro-VM / OpenShell

**对 power user / Hyper 的启示：**

- 很多「竞品独有功能」应先做成 **skill/plugin 包**，验证后再内置（Pi 式扩展哲学）
- 学 **agent loop / session / compaction 教科书结构**，不必砍掉 Hyper 已有丰富工具面
- omp = Pi 血统上的 batteries-included；对标 harness 细节读 omp，对标「边界怎么画」读 Pi

**约束：** 与 Hyper 产品定位不同（极简 vs 全功能）。吸收的是**扩展纪律与核心边界**，不是把 Hyper 砍成四工具。

---

### 3.5 Nanocodex — Codex 的 library-first reimplementation

**一句话：** 绑定 OpenAI Responses / Codex 行为合同的极简 Rust agents SDK；Code Mode 优先；可嵌入 CLI/TUI/评测，而非完整产品平台。

| 项 | 内容 |
|----|------|
| 源码 | [gakonst/nanocodex](https://github.com/gakonst/nanocodex)（MIT OR Apache-2.0） |
| 栈 | Rust workspace：`nanocodex-core` / `service` / `tools` / `mcp` / `macros` + `bin/nanocodex`（Ratatui） |
| Thesis | **模型与 harness 是一个系统**；指令、tool 契约、history 顺序、cache identity、WS 生命周期皆为行为合同 |
| 默认模型面 | 窄：OpenAI Responses WebSocket、gpt-5.6 一族；非多 provider 产品 |
| 工具模型 | **Code Mode**：模型写 JS，组合 Rust/MCP tools（循环、`Promise.all`、子 agent spawn/fork） |
| 生命周期 | `prompt` → `Turn`（steer / cancel / result）→ checkpoint；`fork` / `fork_from`；可选 `AgentEvents` |
| 状态 | 进程内权威；CLI 默认可写 Codex-compatible rollout（`~/.codex` 可交接 `codex resume`） |

**标志性能力（对 Hyper 最有启发）：**

1. **Code Mode** — 减少「每 tool 一 round-trip」；编排拓扑由模型生成，核心不做通用 multi-agent 调度器
2. **Turn 语义** — accepted ≠ completed；steer 中途纠偏；cancel 与结果独立 await
3. **历史 fork** — 从 checkpoint 便宜分叉，避免重放整段 history（自有 bench 强调路径延迟）
4. **crate 切分** — core 类型 / service 传输 / tools 执行 / 上层 lifecycle，适合当 SDK 教材
5. **Tower 边界** — 一次 streamed attempt；重试与中间件分层清晰
6. **子 agent 作 tool** — `spawn` 干净子进程、`fork` 带上下文，由 Code Mode 扇出

**对 power user / Hyper 的启示：**

- **研究项**：Code Mode 或等价「可编程 tool 编排」是否值得在 Hyper tools runtime 旁路实验
- **产品化可借**：steer（中途纠偏）、`/btw` 类旁支 fork、checkpoint 语义
- **工程原则**：按 provider 锁「行为合同」做回归（尤其 Codex / 订阅通道），而不是只测抽象 tool loop
- 适用边界：嵌进应用 / 评测 / 单供应商极致路径；**不**替代 Hyper 的多 provider 产品面

**约束：** 很新；沙箱/权限/持久多客户端交给应用层；Node 为 Code Mode 运行时依赖。

**与 Codex / Hyper 关系：**

```text
Codex CLI     = 官方完整产品
Nanocodex     = 同一行为证据下的薄库 + Code Mode
Hyper         = 多 provider 终端产品（可对接 Codex 通道，核心不是 Codex 复刻）
```

---

## 4. 其他 agent 速览

| Agent | 形态 | Power user 可借 |
|-------|------|-----------------|
| **Claude Code** | 官方 TUI（偏闭源） | 默认权限 UX、hooks 生态、工具面克制 |
| **Codex CLI** | 官方 Rust | apply_patch、沙箱、订阅 OAuth；Hyper 已有对接；与 Nanocodex 对照「产品 vs 库」 |
| **OpenCode** | 开源 TUI，多 provider | 大社区默认路径、subagent/MCP 生态广度 |
| **Goose** | Rust，MCP-native | 通用 agent + MCP 作为一等公民 |
| **Cursor / Windsurf** | IDE | @ 引用、多文件 diff 接受 UI、Tab 混 agent |
| **Aider** | git-centric CLI | repo map、以 commit 为边界、便宜模型友好 |
| **Cline / Roo** | VS Code | 浏览器、检查点 |
| **Charm Crush** | Go TUI | 终端 UX / 渲染品味 |
| **Devin 类** | 云端异步 | 长任务异步交付、人审 PR |

Pi / Nanocodex 已升格为 [§3.4](#34-pi--minimal-agent-harness) / [§3.5](#35-nanocodex--codex-的-library-first-reimplementation)，不再只放速览表。

---

## 5. 总能力矩阵

图例：● 强 · ◐ 部分 · ○ 弱 / 无 · ★ 该维度标杆（分析时点快照）

### 5.1 产品级终端 agent

| 能力 | Hyper | omp | Amp | Droid |
|------|:-----:|:---:|:---:|:-----:|
| 终端 agent | ● | ● | ● | ● |
| 多 provider / BYOK | ●★ | ● | ◐ | ● |
| Hashline / 可靠 edit | ● | ●★ | ◐ | ◐ |
| LSP | ● | ●★ | ◐ | ◐ |
| DAP | ○ | ●★ | ○ | ○ |
| Plan | ● | ● | ● | ● |
| 大任务编排 | ◐ | ◐ | ◐ | ●★ Missions |
| 特化角色 UX | ◐ | ● advisor | ●★ | ● |
| Advisor 旁路 | ○ | ●★ | ◐ | ● review |
| Stream 规则纠偏 | ○ | ●★ | ○ | ○ |
| Typed subagent I/O | ● | ●★ | ◐ | ◐ |
| 跨 session 记忆 | ◐ | ● | ◐ | ● |
| 协作 / 分享 | ○ | ● | ●★ | ● |
| Browser | ○ | ● | ● | ● |
| AST 编辑 | ○ | ● | ○ | ○ |
| Effort / roles 产品化 | ◐ pin | ● | ●★ | ● router |
| Fallback / multi-key | ◐ | ●★ | ◐ | ◐ |
| Headless CI | ● | ● | ● | ●★ |
| Readiness | ○ | ○ | ○ | ●★ |
| Sandbox / perm | ● | ● | ● | ● |
| MCP / Skills / Hooks | ● | ● | ● | ● |
| SDLC 自动化 | ◐ | ◐ | ● | ●★ |
| Codebase graph | ● | ◐ | ◐ | ◐ |

### 5.2 Harness / 库向对照（Pi · Nanocodex）

与上表维度不完全同构；强调「可学习差异」而非全面对标产品。

| 能力 | Hyper | Pi | Nanocodex | 备注 |
|------|:-----:|:--:|:---------:|------|
| 定位 | 全功能 TUI 产品 | 最小 harness | 嵌入式 Codex 风格 SDK | 三层不同 |
| 多 provider | ●★ | ● | ○（OpenAI 钉死） | Hyper 护城河 |
| 默认工具面 | 丰富 | 极简四件套 | Code Mode + 调用方 tools | |
| 扩展哲学 | plugins/hooks/skills | Extensions/packages ★ | 调用方自建 | Pi 最「不改核心」 |
| Agent loop 可读性 | ◐（monorepo 大） | ●★ | ● | 入门读 Pi / Nanocodex |
| Session / compaction | ● | ● | ◐ 进程内 | |
| Subagents | ● 内置 | ○ 留给扩展 | ● 作 tool + Code Mode 编排 | |
| Code Mode 编排 | ○ | ○ | ●★ | Hyper 研究项 |
| Turn steer / cancel | ◐ interject | ◐ | ●★ | |
| 历史 fork / checkpoint | ◐ subagent/worktree | ● branch | ●★ 便宜 fork | |
| 内置 sandbox | ● | ○（文档建议容器） | ○（应用负责） | |
| 嵌入 / SDK | ACP · headless | SDK · RPC ★ | library-first ★ | |
| TUI 成熟度 | ● | ● | ◐ 日用 CLI | |

---

## 6. 按失败模式映射功能

比「抄功能清单」更有用的是按实际失败模式选型：

| 痛点 | 竞品解法 | Hyper 现状 | 分析建议 |
|------|----------|------------|----------|
| 模型 edit 偏一行 / 空白打架 | hashline、专用 edit format | 已有 hashline | 打磨默认 toolset 与 prompt，对标 omp 宣传级稳定性 |
| Rename 漏引用 | 深 LSP | 有 LSP | 查 willRename / diagnostics-after-edit 闭环完整度 |
| 不会用调试器、只会加 log | DAP | 无 | 中长期高价值差异化 |
| 长任务跑偏 / 违规 API | stream rules、advisor | hooks 偏工具边界 | stream rules + advisor 值得设计 |
| 弱模型硬撑、不请教强模型 | Amp Oracle 编排 + 强模型 | oracle 类型有、触发/pin 弱 | **当前主线** design-oracle |
| 上下文贵、子任务应用小模型 | roles / low–ultra / pin | 手选 + subagents.models | 钉子代理模型即可；四档 Modes 缓停 |
| 父 agent 读不懂子 agent 散文 | typed yield | `output_schema` 已有 | 默认工作流打磨 + 文档 |
| 不敢 auto-apply | Changes review | hunk tracker 有、UX 弱 | 接受 / 拒绝 / 部分接受 |
| 仓库不适合自治却硬跑 | Readiness | 无 | 轻量 `/readiness` 很适合 power user |
| 超大 feature 单 session 崩 | Missions | plan + Orca | 在现有 plan 上叠里程碑即可，不必上云 |
| 每步 tool 都回模型、编排僵硬 | Code Mode（Nanocodex） | 经典 tool loop | P3 研究；勿阻塞产品主线 |
| 中途想改方向 / 旁支提问 | steer、`/btw` fork | interjection / 多 session | 产品化 steer + 旁支 UX |
| 扩展必须改核心 | Pi packages | 已有 plugins | 强化「一包分享」路径与示例 |

---

## 7. 个人 power user 优先级建议

### 7.1 第一梯队（性价比最高）

1. **Oracle 升级**（强模型钉定 + 可感知触发；对话里点名，**不做** `/oracle`）  
   → 设计：[design-oracle.md](./design-oracle.md)。类型与只读合同已有；缺口是「不叫 / 同模型白叫」。
2. **Diff review UX**（hunk tracker 产品化）
3. **本地 Dashboard**（会话/费用透视；竞品空白）
4. **Provider fallback + multi-key**（钉模型失败时的可观测降级）
5. 打磨已有：hashline 默认路径、subagent 费用/时长、plan 验收、`output_schema` 工作流

~~原 Amp 四档 Modes 作 P0~~ → **缓停**（[design-modes.md](./design-modes.md) §0）：Hyper 已可细配模型；四档是默认搭配 UX，不是自定义刚需。

### 7.2 第二梯队（护城河）

6. Time-traveling stream rules  
7. 轻量 Readiness（`/doctor` 仓库向）  
8. Advisor 旁路（默认关；贵；与 Oracle **按需**不同）  
9. ast_grep / ast_edit（先 skill / plugin）  
10. DAP  
11. Turn steer + 旁支 fork UX（借 Nanocodex 语义，挂现有 session）  

### 7.3 第三梯队（可缓 / 研究）

12. Missions 完整编排 / plan 里程碑轻量版  
13. Collab / 云 thread  
14. 原生 Browser（MCP 可先顶）  
15. Eval 内核  
16. **Code Mode** spike（Nanocodex；与主 tool loop 并行实验）  
17. Amp 式 low–ultra Modes（仅当要用「一键搭配」时再考虑薄 preset，勿整包解析层）  
18. SDLC 平台自动化  

### 7.4 应优先「讲清楚并打磨」的已有能力

- **Oracle pin**：`[subagents.models] oracle = "…"`（不钉则继承主模型，模式失效）
- Hashline 默认路径与 stale-anchor 恢复
- LSP 诊断闭环
- Subagent 状态可视化（时长 / 费用）
- Plan mode 验收清单字段
- Memory 默认策略与 UI
- Typed/`output_schema` workflow（已实现，矩阵已回写 ●）
- Skills / 插件示例（**Pi 式：先包后核**）

### 7.5 刻意缓做

- **Amp 四档 agent modes** — 见 design-modes §0；用 pin + Oracle 触发代替「模式表驱动」
- 闭源专有云能力（Amp Orbs 计费、Factory 企业部署）— 复刻体验即可
- 为对比而堆工具数量 — 优先可靠性与工作流
- 与上游 Grok Build 冲突的破坏性改动 — 扩展以 config / plugin 为先
- 把 Hyper 砍成 Pi 四工具默认 — 学边界，不学阉割产品

---

## 8. 战略定位与吸收原则

```text
Hyper 可占据的位置（power user）：

  「本地优先 · 多 provider · 可审计 · harness 与 Grok/Codex/Kimi 同级锐度」

  相对 omp：更强的多 provider 社区生态 + 已有 graph/compaction/ACP；
            补齐 DAP/stream rules/advisor 后 harness 不落下风。

  相对 Amp：本地、可选模型、可审 diff；**不抄四档 Modes**。
            用「干活模型 + 钉死的更强 Oracle + 对话点名 / 可感知触发」追上深想体验。

  相对 Droid：不做完整 SDLC 云平台；借 Readiness + 轻量里程碑即可。

  相对 Pi：保留全功能默认；学扩展纪律与「核心保持可推理」。

  相对 Nanocodex：不绑单供应商；借 Code Mode / Turn / fork 作研究与局部 UX，
                  不把核心缩成「只服务 OpenAI Responses」。
```

**整合原则（强制）：**

```text
不要「抄功能列表」，要按失败模式选功能：
  编辑失败     → hashline / LSP / ast
  长任务跑偏   → stream rules + oracle/advisor + plan / missions
  弱模型硬撑   → oracle 强模型 pin + 对话点名 + 失败循环提醒
  上下文爆/贵  → 子代理钉便宜模型 + 记忆 / compaction（不必四档 mode）
  不可信改动   → change review + autonomy
  仓库不适配   → readiness
  编排 round-trip 过重 → Code Mode 研究（非 P0）
  扩展要改核   → 先 plugin/skill 包（Pi）；进程内可编程 → WASM guest（design-wasm-extensions）
```

**吸收纪律：**

| 原则 | 含义 |
|------|------|
| 抄行为，不抄皮肤 | Oracle 是「旁路深想角色」，不是必须云线程或同名 |
| 优先落在已有管道 | oracle → subagents；review → hunk-tracker；readiness → doctor/slash |
| 一能力一验收 | 必须对应可复现失败场景 |
| 可关、可配、默认可解释 | 锐度默认开得克制；贵路径默认关 |
| 上游友好 | config / plugin / feature flag 优先 |
| 先插件后内核 | Pi 哲学：skill 验证 → 再考虑内置 |

**五层挂载点（避免到处开洞）：**

```text
1. UX 产品层     slash / review pane / readiness；oracle 走对话点名 → pager
2. 会话策略层    角色 pin · 权限 · plan 里程碑                 → shell/config
3. 采样/流式层   stream rules · steer · 行为合同               → sampler
4. 工具层        DAP · ast · code-mode · typed I/O             → tools
5. 扩展层        skills / hooks / plugins / MCP                → 先做包
```

**吸收流水线（每个特性同一套路）：**

```text
观察竞品失败场景 → 提炼一句话不变式 → 映射 Hyper 落点
  → MVP（可关）→ 同场景验收 → 回写本文件矩阵与 §9 看板
```

---

## 9. 吸收看板

状态：`idea` · `design` · `spike` · `building` · `done` · `wont`（缓做/不做）  
批次与 [§7](#7-个人-power-user-优先级建议) 对齐；落地后改状态并回写矩阵。

| ID | 不变式（不是功能名） | 主要来源 | Hyper 落点 | 批次 | 状态 | 设计/备注 |
|----|----------------------|----------|------------|------|------|-----------|
| A1 | 弱模型硬撑时能请到**更强只读顾问**且模型真的更强 | Amp Oracle | subagents oracle + pin + 触发；对话点名（无 `/oracle`） | P1 | **done**（Phase 0/1） | [design-oracle.md](./design-oracle.md)；spawn 同模型 toast + `/doctor` pin 检查 + 触发文案已落地；Phase 2 harness 信号可选未做 |
| A2 | 用户能**审** agent 改动：接受 / 拒绝 / 部分接受 | Amp Changes | hunk-tracker + TUI | P1 | **done**（A2a） | `/changes` 审阅面板：hunk 按文件分组 + 补丁预览 + a/x 单块、A/X 整文件、^A/^X 全部（外部改动与文件/全部拒绝需 y/n 确认且标明不可撤销）；动作经 ACP hunk-action/file-action/all-action；部分接受（行级）与 undo 未做 |
| A3 | 会话/费用/子代理成本**可透视** | 竞品空白 / 自研 | pager dashboard | P1 | idea | |
| A4 | 钉定模型失败时有**可观测 fallback** | omp multi-key | models / auth / config | P1 | idea | |
| A5 | 流中违规可 **abort→注入→同点重试**，规则不常驻 context | omp stream rules | sampler 流式层 | P2 | idea | 非 PreToolUse 替代 |
| A6 | 一键知道仓库**适不适合自治** | Droid Readiness | `/readiness` 或 doctor 扩展 | P2 | **done** | `/readiness` 已落地：8 探针 + 结论分级 + 10 语言。**v1 为静态信号近似**（不跑 build/test；Droid 原版是执行式验证，可作后续 `--verify` 增强） |
| A7 | 可选第二模型逐步挑刺（默认关） | omp Advisor | 旁路 subagent | P2 | idea | 与 A1 区分：逐步 vs 按需 |
| A8 | 结构改动走 AST，不只文本 diff | omp ast | skill → 可选 tool | P2 | idea | 先插件 |
| A9 | 真调试器，不是只 print | omp DAP | tools + adapter | P2 | idea | 工作量大 |
| A10 | 中途纠偏 / 旁支提问不打断主线语义 | Nanocodex steer/fork | interjection + session UX | P2 | idea | |
| A11 | 超大任务有里程碑与验收，不靠单 session 硬撑 | Droid Missions | plan.md 字段 | P3 | idea | 轻量即可 |
| A12 | 多 tool 编排不必每步回模型 | Nanocodex Code Mode | tools runtime 实验 | P3 | idea | spike only |
| A13 | 扩展工作流**不改核心**即可分享 | Pi packages | plugins/skills + **WASM Extension Bus + SDK** | 横向 | **building**（P1 示例/SDK） | [design-wasm-extensions.md](./design-wasm-extensions.md)；[extension-vs-pi.md](./extension-vs-pi.md) |
| A14 | 一键 effort 搭配表 | Amp Modes | config preset | — | **wont**（缓停） | [design-modes.md](./design-modes.md) §0 |
| A15 | 云 thread / collab | Amp / omp | — | — | wont | 个人后置 |
| A16 | 企业 SDLC 云平台 | Droid | — | — | wont | 体验可借 |

**打磨类（非新能力，持续）：** hashline 默认路径、LSP 闭环、subagent 费用 UI、plan 验收、`output_schema` 默认工作流、Oracle pin 文档。

---

## 10. 参考链接

| 产品 | 链接 |
|------|------|
| omp | https://omp.sh/ · https://github.com/can1357/oh-my-pi |
| Amp | https://ampcode.com/ · https://ampcode.com/manual |
| Factory | https://factory.ai/ · https://docs.factory.ai/llms.txt |
| Pi | https://pi.dev/ · https://github.com/earendil-works/pi |
| Nanocodex | https://github.com/gakonst/nanocodex |
| Codex CLI | https://github.com/openai/codex |
| OpenCode | https://github.com/anomalyco/opencode |
| Goose | https://github.com/block/goose （组织迁移时以官方为准） |
| Hyper | [README.md](../README.md) · [user-guide](../crates/codegen/xai-grok-pager/docs/user-guide/) |
| Hyper 设计 | [design-oracle.md](./design-oracle.md) · [design-modes.md](./design-modes.md) · [KNOWN_ISSUES.md](./KNOWN_ISSUES.md) |

---

## 附录：决策快照

| 项 | 选择 |
|----|------|
| 受众 | 个人 power user |
| 竞品分析 | `docs/competitive-analysis.md`（本文） |
| Amp 四档 Modes | **缓停** — [design-modes.md](./design-modes.md) §0（配置已够；四档非自定义刚需） |
| Oracle | **当前主线设计** — [design-oracle.md](./design-oracle.md)（pin + 触发；对话点名，不做 `/oracle`） |
| Pi | 学扩展纪律与 harness 分层；不砍默认工具面 |
| Nanocodex | Code Mode / Turn/fork 为 P3 研究与局部 UX 灵感；不绑单供应商 |
| 吸收方式 | 失败模式 → 不变式 → 五层挂载 → §9 看板状态机 |
| 实现顺序 | Oracle 文档/可观测 → Diff review / Dashboard；Modes 不排期；Code Mode 不插队 |
