# 斜杠命令

在提示中输入 `/` 打开命令菜单。输入时会模糊匹配，选中命令后立即执行。

命令来自两处：**shell 内建**（由智能体后端 xai-grok-shell 处理）与 **pager 内建**（由 TUI 前端 xai-grok-pager 处理）。两者出现在同一菜单；任何启用了 `user-invocable: true` 的技能也会出现在此。

下列命令在有别名时会列出别名。部分命令仅在功能或会话状态启用时出现，文中会注明。

---

## 会话管理

### `/new`

开始全新会话并清空当前对话。别名：`/clear`。

### `/resume`

打开会话选择器，从磁盘重新加载先前会话。

### `/compact [context]`

压缩对话历史以腾出上下文窗口空间。可传入说明，告诉 Grok 要保留什么：

```
/compact
/compact keep the auth implementation details
```

上下文窗口达到 85% 时也会自动压缩（用 `[session] auto_compact_threshold_percent` 调整）。

### `/context`

显示上下文窗口使用情况：分类明细（系统提示、消息、推理与开销、剩余空间），以及工具定义、技能列表、MCP 服务器公告等估算 token 成本的信息行。

### `/session-info`

显示会话详情 —— 模型、回合数与上下文使用。别名：`/status`、`/info`。

### `/fork`

将当前会话分支为新智能体，保留至此的历史。

### `/rewind`

将对话回滚到更早的回合，并丢弃之后的一切。

### `/edit-prompt`

在 minimal 模式下，为空编辑器打开外部编辑器。Grok 依次解析 `$VISUAL`、`$EDITOR`，最后是 `vi`；命令值可含带引号参数。保存只替换草稿而不发送；保存空文件则清空。该命令在非 minimal 模式下隐藏。

```
/edit-prompt
```

若终端或多路复用器占用了 `Ctrl+G`，要编辑 **已有** 草稿时，打开命令调色板并选择 **Edit Prompt in External Editor**。该路径保留现有文本，并拒绝粘贴/文件引用/图片芯片（不会把它们压扁）。在编辑器里输入 `/edit-prompt` 会替换该输入，因此从空草稿开始。

### `/copy`

将最近一次回复复制到剪贴板。传数字可复制倒数第 N 次回复；传文件路径则写入文件而非剪贴板（SSH 时本地剪贴板常不可达，很有用）。

```
/copy
/copy 2
/copy out.txt
/copy 2 ~/exports/last-reply.md
```

每次复制也会写入备份文件 —— 默认 `~/.grok/last-copy.txt`，或设置 `GROK_COPY_FILE` —— toast 会说明文本落在何处，即便剪贴板不可达或 OSC 52 未被本终端确认也能找回。

### `/export`

将会话导出到文件或剪贴板。

### `/quit`

退出应用。别名：`/exit`。

### `/home`

离开当前会话并返回欢迎屏。别名：`/welcome`。

### `/rename`

重命名当前会话。别名：`/title`。

```
/rename new session title
```

---

## 模型与模式

### `/model <name>`

切换模型。接受模型 ID 或显示名（不区分大小写）；推理模型可把 effort 级别作为第二参数。别名：`/m`。

```
/model grok-build
/model Grok Build
/model Reasoning X high
```

普通切换会保留可移植 transcript（用户消息、可见助手回答、本地工具结果）。提供方特定的推理签名与原生后端工具续跑状态仅回放给产生它们的模型。若提供方仍拒绝陈旧续跑状态，Grok Build 会自动用可移植 transcript 重试一次，并在重试状态中展示该恢复。

不必每次切换前都 compact。下一次模型调用前，Grok Build 会对照目标模型上下文窗口检查 transcript，必要时自动 compact。当现有上下文已约占目标窗口 85% 时，TUI 会给出容量警告；仅当你想自己引导摘要时，才先运行 `/compact <要保留的内容>`。

### `/effort <level>`

在 **当前** 模型上设置推理 effort，无需重新选择。级别为 `low`、`medium`、`high`、`xhigh`；仅当活动模型支持 reasoning effort 时生效。

```
/effort high
```

### `/always-approve` 与 `/auto`

两者都是权限模式的真实开关：始终出现在菜单中，在已开启的模式下再运行会关闭。

| 命令 | 关闭时 | 已开启时 |
|---|---|---|
| `/always-approve` | 跳过所有权限提示 | 回到 ask |
| `/auto` | 分类器批准安全工具（危险工具仍可能提示） | 回到 ask |

一个已开时运行另一个会切换模式 —— 例如 always-approve 开启时运行 `/auto` 会切到 auto。`/auto` 仅在 auto 权限模式功能启用时出现。也可用 `Shift+Tab`（循环 Normal / Plan / Always-approve）、`Ctrl+O` 或 `/settings` 改模式。

### `/multiline`

切换多行输入。开启时 `Enter` 插入换行，`Shift+Enter`（或 `Alt+Enter`）发送。回合中途，空编辑器上的裸 `Enter` 仍会强制发送队首后续。别名：`/ml`。

### `/history`

打开提示历史搜索：按最新优先模糊搜索本会话提示，按 `Enter` 或 `Tab` 把匹配项放回提示。

快速召回：在空提示上按 `↑`。面板打开时已填入最近提示；`↑`/`↓` 在条目间移动（每项进入输入框），越过最新项时 `↓` 关闭面板，输入可就地编辑召回的提示。

### `/compact-mode`

切换紧凑显示 —— 更少内边距与更紧间距，输出更密。

### `/vim-mode`

切换 vim 风格回滚键（`j`/`k`、`h`/`l`、`g`/`G`、`y`/`Y` 等）。关闭时（默认），回滚区裸字母或 `Shift+字母` 只会聚焦提示并键入该字符。设置会持久化到 `[ui] vim_mode`。

### `/minimal` 与 `/fullscreen`

在另一种渲染模式下重新打开当前会话。`/minimal`（全屏时提供）切到实验性回滚区原生模式；`/fullscreen`（minimal 时提供；别名 `/full`）切回标准 alt-screen TUI。两者仅在本会话对同一对话重新启动 pager —— 不改 `config.toml`，重启动横幅会提醒如何切回。`--minimal` / `--fullscreen` CLI 标志同样仅会话范围。要让普通 `grok` 默认以某模式打开，用 `/settings` → **Default screen mode** 或设置 `[ui] screen_mode`。

### `/voice`

切换提示编辑器的麦克风听写。它仍然只做语音转文字，不会播放语音回复。

### `/live`

启动或停止**实验性** Codex 全双工语音对话。按 Space 静音，按 Esc 或 Ctrl+C 结束，也可以点击 Live 底栏中的静音/停止控件。Live 助手可以把编码工作委派给当前 Agent 并朗读结果。无论当前编码模型是什么，它都要求先运行 `grok login --openai`。`/live` 与 `/voice` 互斥。详见 [OpenAI Codex — 实验性 Live 语音](28-openai-codex.md#实验性-live-语音)。

### `/plan`

进入计划模式。

```
/plan [description]
```

### `/view-plan`

预览当前已保存计划。别名：`/show-plan`、`/plan-view`。

---

## 记忆

`/flush`、`/dream` 与 `/memory` 需要启用记忆（`--experimental-memory` 或 `GROK_MEMORY=1`）；`/memory` 还需要已配置的记忆后端。`/remember` 始终可用。

### `/memory`

浏览、查看与管理已保存记忆。传 `on` 或 `off` 启用或禁用记忆。别名：`/mem`。

```
/memory
/memory off
```

### `/flush`

立即将会话知识写入记忆，触发 LLM 对最重要内容的摘要。适合在 compact 之前，或任何想锁定上下文时使用。

### `/dream`

运行记忆整理 —— 将会话日志合并为有组织的主题。

### `/remember`

立即将笔记写入记忆，无需等待自动摘要。

```
/remember the staging deploy uses the eu-west cluster
```

---

## Hooks 与插件

`/hooks`、`/plugins`、`/marketplace` 与 `/skills` 打开同一扩展模态，各在自己的标签页。

### `/hooks`

在 Hooks 标签打开扩展模态，可查看已加载 hooks、增删自定义 hooks、单独开关。该模态不授予项目信任 —— 信任模型见 [10-hooks.md](10-hooks.md)。

shell 还提供 `/hooks-list`、`/hooks-trust`、`/hooks-add`、`/hooks-remove`、`/hooks-untrust`；在 TUI pager 中它们折叠进 `/hooks` 模态。

### `/plugins`

在 Plugins 标签打开扩展模态，查看已安装插件、从市场安装、管理信任。

shell 还支持子命令（`/plugins list`、`/plugins install <source>`、`/plugins uninstall <name>`、`/plugins update`、`/plugins reload`）。在 TUI 中模态以可视化方式完成同样工作。

### `/marketplace`

在 Marketplace 标签打开扩展模态，浏览并安装插件。

### `/skills`

在 Skills 标签打开扩展模态，查看已安装技能。

---

## 媒体生成

### `/imagine <description>`

根据文本描述生成图片。

```
/imagine a golden sunset over a calm ocean with silhouetted palm trees
```

### `/imagine-video <description>`

根据文本（或图片）描述生成视频。会规划镜头、生成源图，并用 `image_to_video` 动画化。

```
/imagine-video a cat playing piano in a jazz club
```

---

## 调度

### `/loop [interval] <prompt>`

按间隔重复运行提示。间隔形如 `30m`、`1 hour` 或 `every 2 days`；省略则 Grok 会询问。

```
/loop 30m check deploy status
/loop check deploy status every hour
```

间隔为 `Ns`（秒，最小 60）、`Nm`（分钟）、`Nh`（小时）或 `Nd`（天）；不足 60 秒会抬到最小值。循环任务 7 天后过期，可用创建时报告的 job ID 通过 `scheduler_delete` 取消。

---

## 工作流与目标

### `/goal`

设置、管理或查看自主目标。Grok 跨多轮工作，仅在独立证据审查确认声明后才将目标标为完成；若审查无法复现结果或没有可用证据，目标保持活动或因具体缺口而暂停。

```
/goal Migrate the auth module to the new API
/goal status
/goal pause
/goal resume
/goal clear
```

参数为 `<objective> [--budget <tokens>]`，或 `status`、`pause`、`resume`、`clear` 之一。这里的 `--budget` 是目标运行的 **token** 预算，与工作流使用的智能体数量预算分开。`/goal` 在会话启用目标模式时出现。驱动取决于后台工作流：开启时由宿主评估每轮并对抗式验证完成候选；关闭时走遗留的模型侧 `update_goal` 路径报告进度并触发验证。

### `/deep-research <query>`

启动后台研究工作流。规划有界问题集，收集带来源证据的结构化声明，在独立验证分片上交叉检查每条声明，并仅渲染通过验证的声明及其已验证来源定位。失败分片、丢弃的声明与研究者不确定性会作为覆盖范围限制报告；只要仍有剩余，报告会标为 **Partial**。

```
/deep-research Compare the migration risks of PostgreSQL 17 and MySQL 9
```

命令立即返回 —— 在 `/workflows` 中跟踪进度，最终报告会自行出现在对话中。

模型启动的工作流可在 `workflow` 工具上设置 `agent_budget`。这是逻辑子智能体调用的绝对累计上限：每次 `agent()` 调用与 `parallel()` 面板中的每一项各占一槽，schema 修正重试不计入。默认 128，显式值为 1–1,024；会越过剩余预算的面板在启动子项前被拒绝。`budget()` 将上限报告为 `total`，已接纳调用为 `spent`，`reserved`（恒为 0）与 `remaining`。具名斜杠启动使用默认预算。

### `/workflow`

启动已保存工作流，或用 `/workflows` 中显示的会话唯一显示名管理运行中的工作流。同一工作流启动两次时显示名会编号（`review-changes`、`review-changes-2`）；你无需内部 run ID。

```
/workflow review-changes {"target":"origin/main...HEAD"}
/workflow pause review-changes
/workflow resume review-changes
/workflow stop review-changes-2
/workflow save review-changes
```

项目工作流在 `.grok/workflows/*.rhai`；用户工作流在 `~/.grok/workflows/*.rhai`。同进程 pause/resume 从已提交的宿主调用结果继续原始不可变脚本、参数与 `agent_budget` 上限 —— 要迭代，请编辑返回的脚本副本并作为新运行启动。

预算受限的运行不同：只能通过提供高于已接纳智能体数量的 `agent_budget` 的模型/工具 resume 请求恢复。裸 `/workflow resume <name>` 无法提高上限，因此会拒绝预算受限运行。进程重启打断的运行完全不会恢复，因为外部效果没有稳定的跨进程身份。且 resume 不是恰好一次：同进程 pause 前未提交结果的外部效果可能再次运行。

### `/workflows`

打开实时工作流 **运行** 仪表盘 —— 活动与保留的运行，不是已保存定义的目录。每行显示运行的显示名、阶段、智能体名册、进度与结果。在运行详情中，`p` 暂停，`r` 恢复普通暂停，`x` 停止。预算受限运行不能裸 resume：`r` 返回 shell 拒绝（用传入更高 `agent_budget` 的模型/工具 resume 提高上限），`x` 仍可停止。`s` 保存运行脚本，但对已知内建与编号重复句柄隐藏 —— 对这些情况，选择新的唯一 `meta.name` 并显式保存编辑后的脚本。

---

## 其他

### `/theme`

切换 TUI 颜色主题。别名：`/t`。

### `/feedback [message]`

报告问题或发送反馈。

```
/feedback Something isn't working correctly
```

### `/btw`

向智能体发送旁注而不打断当前任务。在 minimal 模式（`--minimal`）下，回答显示在提示上方可关闭面板：`Esc` 关闭，完成的回答会保存到原生回滚区，对已关闭面板的迟到回复会被丢弃。旁问及其回答不属于主回合。

```
/btw also check the error handling
```

### `/mcps`

打开 MCP 服务器管理模态。

### `/doctor`

显示只读终端诊断报告 —— 颜色级别、可用主题、剪贴板路径、实时键盘与屏幕证据，以及常见问题修复。别名：`/terminal-setup`、`/terminal-check`、`/terminal-info`。

### `/release-notes`

查看当前版本发行说明。别名：`/changelog`。

### `/docs`

浏览 TUI 内 How-to Guides、打开在线 Build 文档，或按标题直达指南。别名：`/howto`、`/guides`。

```
/docs
/docs web
/docs Getting Started
```

- 裸 `/docs`（或 `/docs how-to`）打开 How-to Guides 选择器。
- `/docs web` 在浏览器打开 https://docs.x.ai/build/overview。
- `/docs <title>` 按不区分大小写的标题匹配打开特定指南。

### `/import-claude`

打开 Claude 导入模态，导入 `~/.claude` 设置：权限、环境变量、MCP 服务器、hooks 与路径。

---

## 智能体与人设

### `/config-agents`

打开智能体模态，查看与管理智能体定义、设置默认并切换活动智能体。别名：`/agents`。

### `/personas`

创建、编辑与删除人设。子智能体可应用人设以塑造行为。

---

## 账户与计费

### `/login`

登录或重新认证（可在会话内执行）：

```text
/login                 # 默认交互方式（多为 Grok / SpaceXAI）
/login openai          # ChatGPT Codex OAuth（也可用 codex / openai-codex / chatgpt）
/login kimi            # Kimi Code 设备 OAuth
```

Hyper **首次启动不会强制** Grok OAuth，需要时再 `/login`。详见 [认证](02-authentication.md) 与 [OpenAI Codex](28-openai-codex.md)。

### `/providers`

列出 / 设置 / 清除 BYOK 平台 API Key（OpenRouter、Anthropic、OpenAI API、Ollama 等）：

```text
/providers
/providers openrouter sk-or-...
/providers clear anthropic
```

### `/logout`

登出并返回登录屏。可用标志 / 子命令清除第三方 scope（`--openai`、`--kimi`、`--all`，或 `/logout provider <platform>`）。

### `/usage`

查看额度使用或管理计费。别名：`/cost`。

```
/usage
/usage manage
```

### `/privacy`

显示或切换隐私与数据保留状态。

```
/privacy
/privacy opt-in
/privacy opt-out
```

`/privacy` 不改动 `[features] telemetry`、`trace_upload` 或外部 OTEL 设置 —— 见 [用量监控](24-monitoring-usage.md#related-settings)。团队账号上只有团队管理员可如此切换隐私；管理员也可为团队启用或禁用 Zero Data Retention（[如何启用 ZDR](https://docs.x.ai/developers/faq/security#how-to-enable-zdr)）。

---

## 配置与 UI

### `/settings`

打开设置模态，交互查看与修改配置。别名：`/config`、`/preferences`、`/prefs`。

### `/timestamps`

开关消息时间戳。

---

## 作为斜杠命令的技能

任何在 SKILL.md frontmatter 中设 `user-invocable: true` 的已启用技能都会显示为斜杠命令。（通过 `/skills` 关闭技能后不再公布。）因此 `~/.grok/skills/commit/SKILL.md` 处的技能运行为：

```
/commit fix typo in README
```

来自插件的技能同理。跨作用域同名时加限定：

```
/local:commit      # 项目作用域技能
/user:commit       # 用户作用域技能
```

内建命令始终优先于同名技能。技能命名为 "compact" 时 `/compact` 仍运行内建 —— 但 `/local:compact` 会调用该技能。

---

## 自动补全

菜单支持模糊搜索：在 `/` 后开始输入以过滤。每项显示命令名、描述、参数提示（若有参数）以及来源（builtin、技能作用域或插件名）。按 `Tab` 或 `Enter` 接受高亮命令。
