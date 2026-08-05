# 子智能体与角色（Personas）

子智能体是可并行处理任务的独立子会话。每个子智能体拥有自己的上下文窗口，因此主智能体可以将工作（研究、实现、测试与代码审查）委派出去，而无需占用自身上下文。子智能体完成后会向父会话回报摘要。

子智能体默认启用。

---

## 智能体（Agents）与角色（Personas）

智能体与角色都会定制行为，但作用层级不同：

| | **智能体（Agents）** | **角色（Personas）** |
|---|---|---|
| **配置内容** | 整个会话：模型、工具、提示模式、系统提示 | 叠加到子智能体提示上的行为层 |
| **作用范围** | 主会话或子智能体 | 仅子智能体 |
| **设置方式** | 启动时，或通过智能体定义（`.grok/agents/` 或 `~/.grok/agents/` 中的 `.md` 文件） | 在 `config.toml`（`[subagents.personas]`）或 `.grok/personas/` 下的 `.toml` 文件中；在子智能体解析时应用 |
| **控制内容** | 模型、工具可用性、提示正文、技能 | 语气、输出格式、任务焦点，以及输入/输出契约 |
| **谁可编辑** | 你 —— 在智能体模态框中创建、删除或切换，或直接编辑文件 | 你 —— 在配置或文件中定义自定义角色；内置角色只读 |
| **示例** | `grok-build`、`explore`、`plan` | `researcher`、`concise` |

智能体定义会话本身。角色塑造子智能体在会话中的行为方式。子智能体始终以某种智能体类型运行（例如 `general-purpose`），解析时可以在其上叠加一个角色。

在智能体模态框中管理二者。使用 `/config-agents`（别名 `/agents`）打开，或使用 `/personas` 直接打开「角色」标签页。该模态框有两个标签页：**智能体** 与 **角色**。

---

## 禁用子智能体

可通过环境变量或配置文件禁用子智能体：

```bash
export GROK_SUBAGENTS=0              # Environment variable
```

```toml
# ~/.grok/config.toml
[subagents]
enabled = false
```

---

## 子智能体如何工作

当主智能体识别到可委派的工作时，会调用 `spawn_subagent` 工具启动子会话。子会话以如下方式运行：

- 拥有独立于父会话的上下文窗口
- 工具集由其智能体类型与可选能力模式决定
- 可选的角色说明在解析时应用

子会话完成后，父会话会收到其子会话的输出——通常是一份摘要。

---

## 内置智能体类型

`spawn_subagent` 工具接受 `subagent_type` 参数，用于选择子会话的角色：

| 类型              | 说明                                          |
| ----------------- | ---------------------------------------------------- |
| `general-purpose` | 默认类型。适用于任意任务的全能力智能体。    |
| `explore`         | 快速只读研究智能体。可读取文件、列出目录并搜索代码；没有 shell 或编辑工具。 |
| `plan`            | 只读规划智能体。可检视代码库并维护规划待办列表；没有 shell 或编辑工具。 |
| `oracle`          | 深度分析顾问。只读；工作智能体在卡住、调试复杂问题或权衡方案时咨询它，然后按其建议行动。建议固定到更强的模型以获得最佳效果。 |

### 打包附带的专业智能体

发行包还会在 `bundled/agents/` 提供 Markdown 智能体（安装到
`~/.grok/bundled/agents/`）。它们以 **bundled** 来源出现（优先级低于项目
`.grok/agents/` 与用户 `~/.grok/agents/`）：

| 类型 | 说明 |
| ---- | ---- |
| `scout` | 快速只读侦察，压缩 handoff（Summary / Files / Architecture）。适合「X 在哪」类摸底；与 `explore` 部分重叠。 |
| `reviewer` | 只读合并前审查；仅报告补丁引入问题，P0–P3。 |
| `security-reviewer` | 只读安全审查；source→sink 证据链，拒绝无路径臆测。 |
| `librarian` | 基于**已安装或克隆的源码**做外部库/API 求证，带引用。 |
| `designer` | UI/UX 实现与审查；设计系统优先，可写。 |

通过 `spawn_subagent` / Task 的 `subagent_type` 填入上表名称即可。同名项目/用户
智能体可覆盖；也可用 `[subagents.toggle]` 关闭。

项目级或用户定义的智能体可以新增类型，或按名称覆盖这些内置类型。

### 咨询 Oracle

Oracle 模式将快速工作模型与更强的分析模型配对：主智能体继续用自己的模型工作，当遇到需要更深推理的问题时，会生成 `oracle` 子智能体，阅读其建议后继续执行。Oracle 在工具集层面是只读的，因此可以检视仓库但绝不会编辑它。

通过 `/agents` → 选择 `oracle` → `m`（或在 `config.toml` 中设置 `[subagents.models] oracle = "..."`）将 oracle 固定到你配置中最强的模型。若不固定，它会继承会话模型，从而失去该模式的意义。

**主智能体应在何时咨询 Oracle**（由它自己通过 `spawn_subagent` 决定；Hyper 不会像某些云产品那样每轮自动跑 Oracle）：

- 同一个测试或错误在多次修改后仍然失败
- 根因不明、架构取舍、或高风险变更
- 你要求它重新思考、审查方案或听取第二意见

如果工作模型一直硬撑而不咨询 Oracle，**直接在对话里点名** —— 没有 `/oracle` 斜杠命令。例如："用 oracle 看为什么测试还红"、"ask oracle why this still fails"、"spawn oracle on the root cause"。请先钉好更强的模型，让这次咨询物有所值。

**可观测性。** 两道护栏保证钉定生效：

- 如果 `oracle` 子智能体启动时解析出的模型与会话**相同**，会弹出提示："Oracle 正使用与会话相同的模型 — 请通过 `/agents` → `oracle` → `m` 钉一个更强的模型"。
- 当 Oracle 完全没有 `[subagents.models]` 钉定时，`/doctor` 会给出建议；在 TUI 中，钉定模型与会话模型相同也会提示。

内置 Oracle 默认有边界：12 轮模型/工具使用、40 次工具调用，以及 180 秒总墙钟时间。其中 30 秒预留给收尾。接近限制时，Grok Build 会要求 Oracle 停止调查并返回其最佳结构化建议；若它忽略警告，则由硬性的工具/时间限制取消它。Oracle 响应契约包括 findings、evidence、alternatives、risks、verification handoff、confidence 与最终建议。

自定义或覆盖的智能体定义可使用相同的 YAML frontmatter 字段：

```yaml
maxTurns: 12
maxToolCalls: 40
timeoutSecs: 180
finalizeGraceSecs: 30
```

所有值必须大于零。`maxTurns` 是既有的模型/工具循环限制；其余字段可选。没有这些字段的智能体保持无界，除非继承了父级的 `maxTurns` 限制。

---

## 角色（Personas）

角色是具名的行为叠加层。其说明会作为 `<system-reminder>` 注入子智能体对话，从而塑造语气、输出格式与任务焦点，而不改变子智能体的智能体类型、模型或工具。

在 `config.toml` 或 `.toml` 文件中定义角色：

```toml
[subagents.personas.researcher]
instructions = "You are a thorough researcher. Always cite specific file paths."
description = "Deep investigator."
```

Grok Build 按以下优先级从这些位置发现基于文件的角色：

- `.grok/personas/*.toml`（项目）
- `~/.grok/personas/*.toml`（用户）
- 内置角色目录（最低优先级）

每个文件定义一个角色，文件名（不含扩展名）即为角色名。内联的 `config.toml` 角色优先于文件。仅发现 `.toml` 文件。

在智能体模态框的「角色」标签页（`/personas`）中管理角色。你定义的角色（用户或项目文件）可编辑；内置角色只读，但若为内置角色指定模型，会向 `~/.grok/personas/` 写入自定义副本以覆盖内置定义。

要从 TUI 为角色设置模型，在「角色」标签页中选中它并按 `m`——会打开与智能体固定相同的模型选择器（输入以筛选，`↑`/`↓` 选择，Enter 应用，**inherit** 清除）。仅提供已配置凭据的模型。选择会写入角色 `.toml` 文件中的 `model` 键。项目角色（`.grok/personas/`）在每次生成时重新发现，因此其模型立即生效；用户角色（`~/.grok/personas/`）在会话启动时加载，因此其模型在新会话中生效。

> **注意：** Grok Build 通过子智能体解析与角色应用 personas，而不是通过 `spawn_subagent` 参数。主智能体在生成子会话时不会传入角色名称。

### 角色字段

| 字段               | 说明                                                          |
| ------------------- | ------------------------------------------------------------------- |
| `instructions`      | 作为角色层应用的内联说明文本。               |
| `instructions_file` | 说明文件路径，在生成时加载，并在 `instructions` 之后合并。 |
| `description`       | 在角色目录中显示的简短摘要。若未提供，则回退为 `instructions` 的第一段。 |
| `inputs` / `outputs`| 声明的输入与输出契约（见下文）。                     |
| `model`             | 使用该角色时应用的模型覆盖。                    |
| `reasoning_effort`  | 经验证的推理强度（`none`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max` 或 `ultra`）。 |
| `default_isolation` | 默认隔离模式（`none` 或 `worktree`）。                      |

### 输入/输出契约

角色可以声明其期望的输入与产生的输出。父智能体读取这些信息，以了解应提供什么上下文、期望什么产物。这使你可以将角色串联起来，使一个角色的输出文件成为下一个角色的输入：

```toml
[[subagents.personas.reviewer.inputs]]
name = "review_file"
io_type = "file"
required = true
description = "Path to the code under review"

[[subagents.personas.reviewer.outputs]]
name = "summary_file"
io_type = "file"
required = false
description = "Path to write review notes"
```

每个字段都有 `name`、`io_type`（默认为 `file`）、`required` 标志与 `description`。

### 角色解析

当应用角色时，Grok Build 在生成边界处一次性解析有效模型与推理强度，顺序如下（优先级从高到低）：

1. 显式的生成时覆盖
2. 角色（role）默认值
3. 角色（persona）默认值
4. 智能体定义默认值
5. 父会话

隔离遵循相同的前四层，但默认是 `none`（无 worktree），而不是从父会话继承。能力模式刻意更严格：显式请求、角色与智能体定义的模式作为安全上限取交集。调用方可收窄访问权限，但不能将 `oracle`、`explore` 或 `plan` 智能体扩大到只读之外。该限制在默认工具组装之后运行；内置只读智能体不会继承未分类的 MCP 工具；`explore` 与 `oracle` 保持其精确的策展工具集。

若请求了角色但无法解析——未找到、没有说明，或其 `instructions_file` 不可读——则生成失败。推理强度值在配置加载期间解析为固定枚举，因此拼写错误会尽早失败，而不是静默传到提供方。

---

## 生成子智能体

主智能体调用 `spawn_subagent` 工具。其参数：

| 参数         | 说明                                                       |
| ----------------- | ---------------------------------------------------------------- |
| `prompt`          | 子智能体的完整任务提示。                           |
| `description`     | 任务的短标签（3–5 个词）。                          |
| `subagent_type`   | 要启动的智能体类型。默认为 `general-purpose`。         |
| `background`       | 在后台运行子智能体，并立即返回子智能体 ID。默认为 `true`。 |
| `capability_mode` | 限制子智能体的工具：`read-only`、`read-write`、`execute` 或 `all`。 |
| `isolation`       | `none`（共享工作区，默认）或 `worktree`（隔离的 git worktree）。 |
| `resume_from`     | 继续已完成子智能体的对话。传入其 subagent ID。 |
| `cwd`             | 子智能体的工作目录。与 `isolation: worktree` 互斥；在设置了 `resume_from` 时忽略（恢复的子会话继承其来源的目录）。 |

在后台运行子智能体时，稍后使用 `get_command_or_subagent_output` 获取其结果。

---

## 能力模式

能力模式是对子智能体工具的可选粗粒度过滤：

| 模式         | 读 | 写 | 执行 | 说明                                  |
| ------------ | ---- | ----- | ------- | -------------------------------------------- |
| `read-only`  | 是  | 否    | 否      | 读取、搜索与检视（也包括网络搜索与 LSP）；无文件编辑或 shell。 |
| `read-write` | 是  | 是   | 否      | 读取，以及创建、编辑、删除与移动文件。无 shell。 |
| `execute`    | 是  | 否    | 是     | 读取，以及运行 shell 命令与后台任务。无文件编辑。 |
| `all`        | 是  | 是   | 是     | 无限制的工具访问。                    |

若省略 `capability_mode`，子智能体使用其智能体类型的工具集。内置的 `explore`、`plan` 与 `oracle` 类型强制只读，且没有 shell 或编辑工具；`general-purpose` 提供完整工具集。

---

## 上下文继承

### resume_from

`resume_from` 参数使新的子智能体可以从已完成子智能体停下的地方继续，这对多阶段工作流很有用：

1. 生成一个研究子智能体来调查问题。
2. 再生成第二个子智能体，将 `resume_from` 设为第一个子智能体的 ID，使其带着完整研究上下文继续。

新子智能体继承来源的对话记录、工具状态与模型；其系统提示与工具会根据当前智能体定义重新渲染。来源必须已完成（非运行中）、属于当前会话，并使用相同的智能体类型。

---

## 隔离：Worktree 模式

对于会修改文件的任务，使用 `isolation: worktree` 在隔离的 git worktree 中运行子智能体。这样可避免子会话的编辑与父会话冲突：

- 子智能体在自己的工作树副本中工作。
- 其变更与父会话隔离，直到你合并它们。
- 子智能体的结果包含 worktree 路径。

Grok Build 通过 `x.ai/git/worktree/*` 扩展方法管理工作树，包括将变更合并回主工作目录的 apply 操作。

---

## 配置

### 按类型开关与模型覆盖

禁用特定智能体类型，或将其路由到不同模型：

```toml
[subagents.toggle]
explore = true                       # default -- omit to keep enabled
plan = false                         # disable the plan subagent

[subagents.models]
explore = "grok-build"               # route explore to a specific model
```

按类型的模型覆盖对任意父会话生效。没有覆盖时，子智能体继承父会话的模型。

### 按类型钉定推理强度（effort）

可以为每种智能体类型钉定 effort 层级 —— 比如 `explore` 用便宜档位、`oracle` 用深想档位：

```toml
[subagents.effort]
oracle = "high"      # 可选: none, minimal, low, medium, high, xhigh, max, ultra
explore = "low"
```

优先级（从高到低）：单次 spawn 的显式覆盖 > 角色（role）的 `reasoning_effort` > 角色（persona）的 > 本 `[subagents.effort]` 钉定 > 智能体定义里的 `effort:` 字段 > 继承父会话的 effort。填写了未知档位的钉定会被忽略并记录日志警告。`/agents` 会在模型钉定旁边显示 effort 钉定（行内 `effort: <档位>`，展开详情中有 `pinned — [subagents.effort]` 说明）；编辑目前通过 `config.toml` 进行。

你也可以从 TUI 管理模型固定：打开 `/agents`，选择一个智能体，然后按 `m`。会打开模型选择器，列出你有凭据的模型——输入以筛选，用 `↑`/`↓` 选择，按 Enter 固定。选择 **inherit**（第一行）可清除固定并重新跟随会话模型。已固定的智能体在列表中显示 `→ <model>`，并在展开详情中显示 `pinned — [subagents.models]` 说明。固定写入 `~/.grok/config.toml`，并在下一次子智能体生成时生效——无需重启。

### 自定义角色（Roles）与 Personas

用各自的能力与模型默认值定义自定义角色：

```toml
[subagents.roles.researcher]
description = "Deep research agent"
default_capability_mode = "read-only"
model = "grok-build"
prompt_file = ".grok/prompts/researcher.md"
```

用行为说明定义自定义 personas：

```toml
[subagents.personas.concise]
instructions = "Be concise. No filler words."
# instructions_file = ".grok/personas/concise.md"  # or load from a file
```

Grok Build 还会从 `.grok/roles/*.toml` 发现角色，从 `.grok/personas/*.toml` 发现 personas。内联的 `config.toml` 定义优先于文件。

---

## 任务面板（TUI）

Grok Build 在智能体屏幕的侧边面板中显示运行中与已完成的工作：

- 按 `Ctrl+G` 切换任务面板，其中列出活动与已完成的子智能体及后台命令及其状态。
- 按 `Ctrl+T` 切换独立的待办面板。

要查看可用的智能体类型与角色，用 `Ctrl+P` 打开命令面板，并选择 **Manage Agents**（`/config-agents`）。

子智能体出现在任务面板顶部，位于可折叠的「Subagents」分组中。运行中的行显示已用时间（以及配置的墙钟限制）、相对限制的实时工具调用次数，以及当前上下文 token。完成的行切换为累计模型用量，并在可用时显示可信的报告成本。由运行时驱动的结束会以 `tool limit`、`time limit` 或 `budget finalized` 等原因标注。

---

## 在 TUI 中查看子智能体

子智能体在交互式 TUI 的多个位置出现：

### 回滚记录（父对话历史）

当生成子智能体时，会在*父会话*的回滚记录中添加一个紧凑的生命周期块：

- `Subagent running: "do the thing" (Implementer · grok-3) — Thinking`
- 或对于后台子智能体：`Subagent started: "..."`

运行期间，该块显示实时活动后缀（例如 "Running: cargo test"、"Compacting"、"Retrying (2/3)"），来自子会话的 turn tracker。项目符号会根据状态动画（或着色）。

在该块上按 **Enter**（或 Ctrl-F）可打开子智能体的完整对话记录。

对于阻塞式子智能体，单个条目会在子会话完成时更新其项目符号颜色。对于后台子智能体，会追加后续的 `Subagent completed/failed/cancelled in Xs: "..."` 块。

### 任务面板（Ctrl+G）

如上所述——分组在「Subagents」下，带有旋转指示器、已用时间，以及快速终止或检视的入口。

### 全屏边框视图（子对话记录）

当你打开子智能体（从回滚块或任务面板）时，父视图会被替换为包含子会话完整对话记录的边框框架：

- 框架内标题栏：状态图标（旋转指示器 / ✓ / ✗）、标签 + 粗体描述 + 模型、可选的 "resumed"/"forked" 徽章、实时活动 · 已用时间，以及 [✗] 关闭按钮。
- 子会话自己的回滚、思考、工具调用与（有限的）提示区域在框架内渲染。
- 子智能体视图大体是观察性的——你通常不能像对父会话那样直接向它们发送新的顶层提示。

使用 `q`、`Esc` 或点击关闭按钮返回父视图。父会话的回滚记录会继续显示子智能体的状态。

---

## 虚拟路径：`agent://` / `history://` / `conflict://`

`read_file`（以及带虚拟 path 的 `grep`）支持会话内方案：

| URL | 含义 |
|-----|------|
| `agent://<subagent_id>` | 子智能体最近一次成功输出 |
| `history://` | 本父会话下的子智能体 roster |
| `history://<id>` | 精简 transcript / 最终输出 |
| `conflict://` | 已注册合并冲突列表 |
| `conflict://N` | 冲突 N 的完整 marker |
| `conflict://N/ours` 等 | 单侧内容 |

对普通路径加后缀 `:conflicts`（如 `src/foo.rs:conflicts`）可注册 git 冲突标记。用 `search_replace` 写 `conflict://N`，内容为 `@ours` / `@theirs` / `@base` 或自由文本以解决。

## TTSR-lite（流式规则）

可选的中途纠偏（默认 **关**）。开启方式：`GROK_TTSR_ENABLED=1`，或在 `~/.grok/config.toml` 写：

```toml
[features]
ttsr = true
```

在 `.grok/rules/*.md` 写带 frontmatter 的规则（`condition` 正则 + `interruptMode`）。匹配后会取消当前流、注入规则正文一次，并允许一次自动重试。

## DAP debug stub（`dap_debug`）

`dap_debug` 为 **占位工具**（status / launch / attach / control / breakpoints / evaluate）。当前一律返回 stub 说明；真正调试请用 shell 调本地 debugger。预留配置：`[features] dap_debug = true`。

## Collab 脚手架

多人协作目前仅配置、无 relay 进程：

```toml
[collab]
enabled = false
relay_url = "wss://collab.example/ws"
web_url = "https://collab.example"
display_name = "alice"
```

## Agent hub（对等消息）

在同一父会话树内，**Main** 与存活的 depth-1 子智能体共享进程内 **agent hub** 总线。对等体可直接互发消息，无需每条都经父会话中转。

面向模型的工具名为 `agent_hub`（与 workspace 远程 hub 不同）。操作：

| Op | 行为 |
|----|------|
| `list` | 对等体 roster：id、标签、`running` / `gone` |
| `send` | 向 `to`（来自 `list`）发纯文本；最大 8 KiB |
| `inbox` | 排空本端邮箱（非阻塞） |
| `wait` | 阻塞直到有邮件或 `timeout_ms`（上限 30s） |

使用约定：

- 发送前先 `list`，不要臆造 peer id。
- 用自然语言；不要用 hub 问工具能回答的问题（`grep`、`read_file`、构建等）。
- 邮箱深度 32（满则丢最旧）；软限流约每发送方 20 条/分钟。
- 投递可能通过 interject 唤醒存活对等体；已结束对等体为 `gone`，拒收新消息。
- 嵌套仍为 depth 1：子智能体不能再 spawn，但可用 `agent_hub` 与 Main / 兄弟通信。

只读 toolset（`explore`、`oracle`、bundled scout/reviewer 等）也包含 `agent_hub`，以便在无 shell/编辑的前提下协调。

---

## 深度限制

只有顶层会话可以生成子智能体。子智能体不能再生成自己的子智能体：最大嵌套深度为一层。若子智能体调用 `spawn_subagent`，该调用会因深度限制错误而失败。这使智能体树保持扁平，并防止失控生成。

---

## 何时使用子智能体

**适合的用例：**

- 在父会话继续其他工作的同时研究代码库
- 在父会话实现变更的同时并行运行测试
- 在提交之前审查生成的变更
- 委派彼此不依赖的独立任务

**不宜使用的情况：**

- 父会话可直接处理的简单任务
- 需要与用户紧密来回交互的任务，因为子智能体自主运行，不适合交互式交流
- 上下文准备成本超过并行收益的任务
