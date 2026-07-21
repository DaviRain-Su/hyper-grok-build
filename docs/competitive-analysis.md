# Code Agent 竞品分析（Hyper 整合视角）

| 项 | 内容 |
|----|------|
| 状态 | 分析文档（非实现规格） |
| 受众 | 个人 power user（终端重度、多 provider、重视 harness 锐度） |
| 产品 | **Hyper** — Grok Build 多 provider 社区版 |
| 初稿日期 | 2026-07-21 |
| 维护 | 能力矩阵与竞品特性会过时；落地功能后请回写「Hyper 基线 / 矩阵」列 |

---

## 目录

1. [执行摘要](#1-执行摘要)
2. [Hyper 能力基线](#2-hyper-能力基线)
3. [竞品深度画像](#3-竞品深度画像)
4. [其他 agent 速览](#4-其他-agent-速览)
5. [总能力矩阵](#5-总能力矩阵)
6. [按失败模式映射功能](#6-按失败模式映射功能)
7. [个人 power user 优先级建议](#7-个人-power-user-优先级建议)
8. [战略定位建议](#8-战略定位建议)
9. [参考链接](#9-参考链接)

---

## 1. 执行摘要

Hyper 已经站在「一流终端 coding agent」梯队：hashline 编辑、LSP、subagents/personas、plan mode、MCP/skills/hooks、多 provider、sandbox 都有。竞品的价值不在「有没有 agent」，而在三类差异：

| 类型 | 代表 | 对 power user 的意义 |
|------|------|----------------------|
| **Harness 锐度** | omp | 编辑 / 调试 / 纠偏 / 模型路由更「打得准、花得少」 |
| **角色与线程产品化** | Amp | Oracle / Librarian、effort 模式、可分享 thread 的心智 |
| **大规模自治与 SDLC** | Factory Droid | Missions、Readiness、平台化——个人用户可后置 |

**建议学习顺序（分析结论，非排期）：**

1. **omp** 的 harness 细节（DAP、stream rules、advisor、role 路由、typed subagent、ast）
2. **Amp** 的 effort modes + 特化角色 UX + change review
3. **Droid** 的 readiness / missions 思路（按需、轻量复刻）
4. **Claude Code / Codex / Aider** 的默认体验与 git 工作流细节

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
- **Advisor** 旁路双模型评审
- **Effort / Role** 产品化路由（smol / slow / plan，或 low–ultra）
- **特化角色**一等公民 UX（Oracle / Librarian 级）
- **Change accept / reject** 统一审阅面
- **Missions / Readiness** 级仓库自治产品
- **Collab / 会话分享**
- **AST 结构编辑**（ast-grep 级）
- **Eval 内核**（持久 Python / JS + tool re-entry）

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
| 血统 | [Pi](https://github.com/badlogic/pi-mono) 的 batteries-included fork |

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

**对 power user 的启示：** 优先学「让弱模型也打得准」的 harness（edit format、LSP、DAP、stream rules），以及「贵模型只花在刀刃上」的 role 路由。协作 / collab 可后置。

---

### 3.2 Amp Code

**一句话：** 意见鲜明的 frontier agent；多模型编排 + 线程分享 + 特化子代理。

| 项 | 内容 |
|----|------|
| 站点 | [ampcode.com](https://ampcode.com/) |
| 手册 | [Owner’s Manual](https://ampcode.com/manual) |
| 形态 | CLI + IDE 连接 + Web；threads 云端 |
| 原则 | 不限 token 心态、永远好模型、raw power、随模型演进少包袱 |
| 模式 | **low / medium / high / ultra** |

**产品子系统：**

- **Oracle** — 深度设计 / 评审
- **Librarian** — 代码库 / 文档检索顾问
- **Painter** — 视觉 / UI 向特化
- **Code Review** — 可组合的评审 agent
- **Orbs** — 并行轻量工作单元（产品包装）
- **Runners / Changes Workflow** — 执行与改动审阅
- **Thread Sharing / Remote Control / Slack**
- **Plugins** — 自定义 mode / subagent / 权限

**对 power user 的启示：**

- 用 **effort 四档** 替代「在几十个 model id 里点选」
- 把 **Librarian / Oracle / Reviewer** 做成内置一等公民，而不是只靠用户自写 persona
- **Thread = 任务边界** 的习惯与 UI（一任务一线程）
- **看 agent 改了什么** 的 Changes 面

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

## 4. 其他 agent 速览

| Agent | 形态 | Power user 可借 |
|-------|------|-----------------|
| **Claude Code** | 官方 TUI | 默认权限 UX、hooks 生态、工具面克制 |
| **Codex CLI** | 官方 | apply_patch、沙箱、订阅 OAuth；Hyper 已有对接 |
| **Cursor / Windsurf** | IDE | @ 引用、多文件 diff 接受 UI、Tab 混 agent |
| **Aider** | git-centric | repo map、以 commit 为边界、便宜模型友好 |
| **OpenCode / Pi** | 开源 harness | 极简扩展点；omp 从此分出 |
| **Cline / Roo** | VS Code | 浏览器、检查点 |
| **Devin 类** | 云端异步 | 长任务异步交付、人审 PR |

---

## 5. 总能力矩阵

图例：● 强 · ◐ 部分 · ○ 弱 / 无 · ★ 该维度标杆（分析时点快照）

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
| Typed subagent I/O | ◐ | ●★ | ◐ | ◐ |
| 跨 session 记忆 | ◐ | ● | ◐ | ● |
| 协作 / 分享 | ○ | ● | ●★ | ● |
| Browser | ○ | ● | ● | ● |
| AST 编辑 | ○ | ● | ○ | ○ |
| Effort / roles | ◐ | ● | ●★ | ● router |
| Fallback / multi-key | ◐ | ●★ | ◐ | ◐ |
| Headless CI | ● | ● | ● | ●★ |
| Readiness | ○ | ○ | ○ | ●★ |
| Sandbox / perm | ● | ● | ● | ● |
| MCP / Skills / Hooks | ● | ● | ● | ● |
| SDLC 自动化 | ◐ | ◐ | ● | ●★ |
| Codebase graph | ● | ◐ | ◐ | ◐ |

---

## 6. 按失败模式映射功能

比「抄功能清单」更有用的是按实际失败模式选型：

| 痛点 | 竞品解法 | Hyper 现状 | 分析建议 |
|------|----------|------------|----------|
| 模型 edit 偏一行 / 空白打架 | hashline、专用 edit format | 已有 hashline | 打磨默认 toolset 与 prompt，对标 omp 宣传级稳定性 |
| Rename 漏引用 | 深 LSP | 有 LSP | 查 willRename / diagnostics-after-edit 闭环完整度 |
| 不会用调试器、只会加 log | DAP | 无 | 中长期高价值差异化 |
| 长任务跑偏 / 违规 API | stream rules、advisor | hooks 偏工具边界 | stream rules + advisor 值得设计 |
| 上下文贵、子任务应用小模型 | roles / low–ultra | 手选 model | 高性价比产品化 |
| 父 agent 读不懂子 agent 散文 | typed yield | persona I/O 弱约束 | 强化 schema 交接 |
| 不敢 auto-apply | Changes review | hunk tracker 有、UX 弱 | 接受 / 拒绝 / 部分接受 |
| 仓库不适合自治却硬跑 | Readiness | 无 | 轻量 `/readiness` 很适合 power user |
| 超大 feature 单 session 崩 | Missions | plan + Orca | 在现有 plan 上叠里程碑即可，不必上云 |

---

## 7. 个人 power user 优先级建议

### 7.1 第一梯队（性价比最高）

1. **Effort / Role 路由**（Amp modes × omp roles）  
   → **已落设计**：[design-modes.md](./design-modes.md)（Amp 式 low–ultra + 可配置槽位 + fallback；先文档后实现）
2. **内置 Librarian / Oracle / Reviewer**（personas / agents 产品化）
3. **Typed subagent 输出**
4. **Diff review UX**（hunk tracker 产品化）
5. **Provider fallback + multi-key**

### 7.2 第二梯队（护城河）

6. DAP
7. Time-traveling stream rules
8. Advisor
9. ast_grep / ast_edit
10. 轻量 Readiness

### 7.3 第三梯队（可缓）

11. Missions 完整编排
12. Collab / 云 thread
13. 原生 Browser（MCP 可先顶）
14. Eval 内核
15. SDLC 平台自动化

### 7.4 应优先「讲清楚并打磨」的已有能力

- Hashline 默认路径与 stale-anchor 恢复
- LSP 诊断闭环
- Subagent 状态可视化（时长 / 费用）
- Plan mode 验收清单字段
- Memory 默认策略与 UI
- Skills / 插件示例

### 7.5 刻意缓做

- 闭源专有云能力（Amp Orbs 计费、Factory 企业部署）— 复刻体验即可
- 为对比而堆工具数量 — 优先可靠性与工作流
- 与上游 Grok Build 冲突的破坏性改动 — 扩展以 config / plugin 为先

---

## 8. 战略定位建议

```text
Hyper 可占据的位置（power user）：

  「本地优先 · 多 provider · 可审计 · harness 与 Grok/Codex/Kimi 同级锐度」

  相对 omp：更强的多 provider 社区生态 + 已有 graph/compaction/ACP；
            补齐 DAP/stream rules/advisor 后 harness 不落下风。

  相对 Amp：本地、可选模型、可审 diff；用 effort + 角色 UX 追上产品顺滑。

  相对 Droid：不做完整 SDLC 云平台；借 Readiness + 轻量里程碑即可。
```

**整合原则：**

```text
不要「抄功能列表」，要按失败模式选功能：
  编辑失败     → hashline / LSP / ast
  长任务跑偏   → stream rules + advisor + plan / missions
  上下文爆/贵  → role 路由 + 子代理 + 记忆 / compaction
  不可信改动   → change review + autonomy
  仓库不适配   → readiness
```

---

## 9. 参考链接

| 产品 | 链接 |
|------|------|
| omp | https://omp.sh/ · https://github.com/can1357/oh-my-pi |
| Amp | https://ampcode.com/ · https://ampcode.com/manual |
| Factory | https://factory.ai/ · https://docs.factory.ai/llms.txt |
| Hyper | [README.md](../README.md) · [user-guide](../crates/codegen/xai-grok-pager/docs/user-guide/) |
| Pi（omp 上游） | https://github.com/badlogic/pi-mono |

---

## 附录：决策快照

| 项 | 选择 |
|----|------|
| 受众 | 个人 power user |
| 竞品分析 | `docs/competitive-analysis.md` |
| Modes 设计 | `docs/design-modes.md`（Amp low–ultra + 槽位可配 + fallback；实现按 P1→P4） |
| 实现顺序 | 先文档后 coding |
