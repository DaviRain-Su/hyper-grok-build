# Agent Dashboard

Agent Dashboard 是一个集中式、以 agent 为中心的总览界面，展示你所有进行中的
顶层会话——包括本地会话与 fork——按状态分组，并可在同一屏幕上
peek、attach 与 dispatch。
子 agent 不会出现在此列表：它们运行在父会话之下，
父会话已能反映是否有工作正在进行。

---

## 打开 Dashboard

三个入口，打开的是同一视图：

- **`grok dashboard`** — 直接以 TUI 启动并进入 dashboard。
- **`/dashboard`**（别名 **`/agents-dashboard`**、**`/sessions`**）— 在
  活跃会话内打开。
- **Ctrl+\\** — 与斜杠命令相同，两键即可。可在
  `~/.grok/config.toml` 的 `[keybindings]` 下配置，与其他快捷键一样。

---

## 你会看到什么

```
 Grok Build · Dashboard — 4 agents · 2 awaiting
▌● reviewer · audit token flow    Awaiting your input            2m
 ● implementer · fix login bug    Running: cargo test           12m
 ⋅ refactor · feat/login          Responding…                   24m
 ○ housekeeping                   idle                           1h
 ● implementer · add login tests  8 tools · 1.2k tok            14m
╭─────────────────────────────────────────────────────────────────╮
│ ❯ Dispatch a new agent                                          │
╰─ dispatch ──────────────────────────────────────────────────────╯
 ↑/↓ select (peek) · Enter open · Ctrl+R rename · Ctrl+T pin · Ctrl+X stop · ? help · Esc new
```

每一行是一个顶层 agent（子 agent 不显示——它们运行在
父会话下）。行按状态排序（Needs input → Working → Idle →
Inactive → Completed → Failed），使同状态的行相邻；也可按
工作目录排序（用 `Ctrl+G` 切换）。**Inactive** 存放仅在 roster 中的
会话——由其他 pager 进程拥有、空闲/休眠且尚未在本进程中加载的会话——
从而让 **Idle** 专注于你正在轮换使用的会话。由于它属于背景噪音，
**Inactive 默认折叠**（用 `→` / 点击展开——见下文）。

为保持 **Idle** 分组可扫视，只显示最近的空闲 agent——
最新的 8 个，以及过去一小时内活跃过的。其余折叠进组底的
**"N more"** 行（带 `+` / `-` 切换标记）；选中后按 `Enter` / `→`
（或点击）展开全部，按 `←` 重新折叠。Idle 标题始终显示真实总数。
启用筛选或搜索时，折叠会暂停（以便显示所有匹配项）。

状态图标与 Grok Build 的兄弟视图（
`tasks_pane`）一致：

- `⋅`/`:`/`⸬`/`⁙` — **Working** 行的动画 spinner。
- `●` — **Needs input**、**Completed**、**Failed**、
  **Blocked** 的实心圆。颜色传达状态（黄 / 绿 / 红 /
  琥珀）。
- `○` — **Idle** 与 **Inactive** 行的空心圆。

只要仍有实时后台工作，行就会保持在 **Working**——即使
本轮已结束——例如正在运行的后台任务、`monitor`，或活跃的
定时 `/loop`。活动行会说明正在运行什么（例如
`1 monitor · 2 loops still running`），因为每一项都可能唤醒 agent
进入新一轮。

没有行内分组标题——排序使同状态
行相邻，每行的圆点+颜色表明该行所属分组
（与其他会话列表一致）。

dispatch 输入与 agent 视图的 prompt 共用同一套 `PromptWidget` chrome
（圆角框、`❯` 前缀、强调色边框、信息行）。按 `Ctrl+/`
切换到 **搜索模式**：`❯`
前缀变为黄色的 `Search:`，你输入的内容会实时过滤
行列表，而不是被 dispatch。

---

## 快捷键

| 按键 | 操作 |
| --- | --- |
| `↑` / `↓`，`j` / `k` | 在行与分组标题间导航（选中某行会打开其 peek 面板） |
| `→` / `←`（在分组标题上） | 展开 / 折叠该分组（显示 / 隐藏其行）；vim 模式下为 `l` / `h` |
| `Enter`（在分组标题上） | 切换该分组折叠 / 展开 |
| `Enter`（空回复） | 全屏打开所选 agent 的对话（详情视图） |
| `Ctrl+S` | 发送 peek 回复并打开该 agent（或 dispatch + attach 到新会话） |
| `Shift+Enter` / `Alt+Enter` | 在回复 / dispatch 输入中插入换行（多行编辑） |
| `1`–`9` | 回答待处理的 permission / ask question（当 peek 显示选项时） |
| `Enter`（已输入回复） | 向所选 agent 发送 / 排队回复 |
| `/` | 向 prompt 输入字面量 `/` |
| `Ctrl+/` | 切换搜索模式（实时过滤行） |
| `Ctrl+R` | 重命名所选行 |
| `Ctrl+T` | 固定 / 取消固定 |
| `Ctrl+G` | 切换分组方式（状态 ↔ 目录） |
| `Ctrl+X` | 停止 / 终止（2 秒内按两次以关闭会话） |
| `Shift+↑` / `Shift+↓` | 重排已固定行 |
| `Esc` | 回退一级：取消搜索 → 关闭 peek（先清空回复草稿，再取消选中）→ 清除筛选 → **取消 dispatch 输入焦点**（使 `↑`/`↓`、`j`/`k` 导航列表）→ 取消选中行（→ `[+ New Agent]`）→ 退出 dashboard。Esc 绝不会清空你已输入的 dispatch 草稿——请用 `Ctrl+U` / `Ctrl+C` |
| `Ctrl+\` | 从详情视图返回 dashboard，或退出 dashboard |
| `Ctrl+.`（备选：`?`） | 打开键盘快捷键速查。当 `Ctrl+.` 无法送达时，页脚会提示 `?`。单独按 `?` 在列表获焦或草稿为空时打开帮助（否则会输入字符）；`Ctrl+X` 仍为停止 |

按状态分组时，每个分组有一个 **分组标题**（例如 `Working`、
`Idle`），带 `▸`/`▾` 展开标记。分组标题参与上下导航：
选中后按 `→` 展开（显示其行），
或按 `←` 折叠——vim 模式开启时 `l` / `h` 作用相同。
**点击**分组标题会切换展开/折叠，**悬停**
会加亮其文字。在 dashboard 保持打开期间会记住折叠状态。
**Inactive** 分组在每次 pager
启动时默认折叠；展开后会保持，直到你退出。

打开某行会在 **详情视图** 中显示该 agent 的对话：
顶部单行标题（左侧为 agent 名称，右侧为 `{i}/{n} [‹][›]
[Dashboard]` 循环/关闭控件）位于对话上方，
对话 **全宽** 渲染——没有带边框的模态框——因此 prompt
位置与整体边距与 dashboard 列表视图一致。所有按键
路由到已 attach 的 agent；`Esc` / `Ctrl+\\`（或 `[Dashboard]`
控件）返回 dashboard，`[‹]` / `[›]` 芯片切换到
上一个 / 下一个 agent，agent 的快捷键栏显示
`Ctrl+\\: back to dashboard` 提示。注意——`Esc` 仅返回
dashboard；在 agent 内输入 `/exit` 会真正关闭
底层会话（返回 dashboard 并显示 “Session closed”
toast）。

详情视图中的 `Ctrl+X` 随状态而变。在 **轮次正在
运行** 时，它取消该轮次——与 `Ctrl+C` 行为相同，
包括 keep-subagents 提示——且绝不触及会话本身，
因此连按停止轮次不会关闭任何东西。在其他
状态——**idle**、斜杠命令执行中（命令尚不能
取消），或取消仍在等待中——`Ctrl+X` 进入
确认：快捷键栏变为 “press Ctrl+x again to
close this session”，2 秒内再按一次会关闭
会话并返回 dashboard。按任何其他键
取消确认；若在窗口内开始了新轮次，
已确认的按压会降级为取消，而不是关闭。
（在 `Ctrl+X` 同时绑定为快捷键速查的
终端上，详情视图内仍可通过 `Ctrl+.`
打开速查。）

完整行为规范（包括 registry 查找
规则与鼠标事件拦截矩阵）见计划
[§3.10](../../plan/agent-dashboard.md)「Keybindings (v1)」——本用户
指南刻意精简，并以该计划为
事实来源进行交叉引用。

所有快捷键注册在 `When::DashboardFocused` 下，可
通过 `~/.grok/config.toml` 重新绑定。

---

## Dispatch 输入

底部文本区 **始终创建新会话**——它永远不是
回复目标。选中的行是总览的导航光标，不是
回复目的地；要与已有 agent 对话，请打开它（导航 +
`Enter`，或点击）并在其自身视图中回复。

Enter 处理逻辑：

- 自由文本 → 创建新的顶层会话，以该 prompt 作为种子。
  文本 **绝不会** 被重新解释为筛选——prompt 可以以
  `/`、`s:`、`a:` 或 `#` 开头，仍会原样 dispatch（筛选是
  显式的 `Ctrl+/` 搜索模式）。前导 `/` 会运行 pager 全局
  斜杠命令。
- 空输入 → 打开所选行（`Attach`），或在 `[+ New Agent]`
  按钮获焦时创建新 agent。

输入 prompt 后按 `Ctrl+S` 可 dispatch 并 attach
（跳入新会话）；普通 `Enter` 留在 dashboard，
以便连续 dispatch 多个会话。`Shift+Enter` / `Alt+Enter`
插入换行以编写多行 prompt——输入框会随行数 **增高**
（有上限，超出后滚动），使整份草稿保持可见。

dispatch 输入接受任何非空 prompt；空 /
仅空白的 prompt 会被忽略。超过 64 KiB 的 prompt 会
以 toast 拒绝。

### 焦点：输入栏 ↔ 总览列表（`Tab`）

Dashboard 有两个焦点区域——**dispatch 输入栏**（输入）
与 **总览列表**（导航）。`Tab` 在二者间切换；非活动
输入会变暗边框并隐藏光标。

打开时，若至少存在一个 agent，焦点默认在 **总览列表**
（以便 `↑`/`↓` / vim `j`/`k` 立即可导航）。若 **没有**
agent，焦点留在 **dispatch 输入**，便于立刻输入第一个
prompt。无论哪种情况，光标目标都是 `[+ New Agent]` 按钮
（不会预选任何 agent 行）。

- **输入获焦**：键入以撰写新会话 prompt。prompt 为空时
  `↑`/`↓` 导航行列表（便利行为），
  否则移动光标。`Esc` 取消输入焦点 → 总览列表
  （保留已输入草稿），以便立刻导航。
- **总览获焦**：`↑`/`↓`——以及在 **vim 模式** 下 `j`/`k`——在
  agent 行间移动。`Enter` 打开高亮的 agent（在
  `[+ New Agent]` 上时，若有已输入草稿则发送，否则创建新会话）。
  `Esc` **留在列表上** 并回退——先清除活跃筛选，
  再取消选中行（→ `[+ New Agent]`），然后退出
  dashboard。`Tab` 或 `i`（vim）——或任何其他可打印键——返回
  输入。

---

## Peek 面板

只要选中了 agent 行，peek 面板 **默认显示**——它 **替换**
新会话 dispatch 框。未选中行时（`[+ New Agent]` 按钮获焦，
或按 `Esc` 后），dispatch 框恢复，用于启动新会话。因此选中行是
与已有 agent 对话的方式；取消选中是启动新
会话的方式。

面板自上而下显示：标题（**最后响应类型**——
`Thinking` / `Thought` / `Response` / `Edit` / `Read` / `Bash` / … ——在
左侧，**时间** 在最右侧）、最近响应
（**自动换行** 适配宽度，最多约 3 行），以及实时的 `❯ reply` 输入。仅当内容超出可显示范围时，最后一行会出现
`…` 标记。

所选 agent 的 **model**，以及在 always-approve（yolo）
模式下的 **`always-approve`** 标志，显示在面板 **底边框**
（右下）——与新会话
dispatch 框使用的同一 config-badge 槽位。在 question / approval 模式下同样如此，因此
你在回答时始终能看到 model 与 approval 模式。（Dashboard 列表行
不再重复 model 或 always-approve 徽章，
以保持列表紧凑。）

**`Shift+Tab` 循环切换被 peek 的 agent 的模式**（Normal → Plan →
Always-approve → Normal）——与该 agent
聊天视图内的 Shift+Tab 循环相同，作用于 **实时** agent（徽章会同步更新）。
这与新会话 dispatch 框不同，后者的 Shift+Tab 仅
为 *下一个* agent 暂存模式。

与仅创建新会话的 dispatch 框不同，
peek 的回复 **与所选 agent 对话**：

- **在 `❯ reply` 中输入，然后按 `Enter`** 发送。**idle** 的 agent
  立即开始该轮；**busy** 的 agent 会 **排队** 该消息，
  在当前轮结束后发送（与 agent 视图自身 prompt 的
  队列/排空行为相同）。`Ctrl+S` 回复并
  打开 agent 详情视图；`Shift+Enter` / `Alt+Enter` 插入
  换行（多行编辑），回复区会 **增高** 以容纳
  草稿（有上限，超出后滚动）。
- 回复为 **空** 时，`Enter` 打开该 agent。
- 一旦回复有内容，**`↑`/`↓` 在回复内移动光标**（以便
  编辑多行草稿）。回复 **为空**（或
  通过 `Tab` 失焦）时，`↑`/`↓` 改为 **切换所选 agent**——
  面板跟随选择光标并实时刷新，且切换会
  清除半完成的草稿，避免回复发到错误的
  agent。（草稿在回复中时，用 `Tab` 切到行列表以导航 agent。）
- **`Esc` 取消选中**：先清空已输入的回复，再取消选中
  行并聚焦 `[+ New Agent]` 按钮（恢复
  新会话输入）。
- **`Tab`** 在回复输入与行列表间切换焦点：
  失焦的回复会变暗边框并隐藏光标；可打印键
  会重新聚焦并开始编辑。
- 回复是 **完整的 prompt 编辑器**（与
  dispatch 框和 agent prompt 同一组件）：粘贴多行文本会折叠
  为 `[Pasted: N lines]` chip，预览浮层与
  展开交互与 agent prompt 相同（`Enter` / 双击 /
  再次粘贴），鼠标点击 / 拖拽放置光标并选择文本，
  常用编辑快捷键均可用（按词导航、`Ctrl+A`/`Ctrl+E`、
  `Alt+Backspace`、`Ctrl+W`/`Ctrl+U`/`Ctrl+K`、撤销、Shift+方向键
  选择、`Ctrl+Shift+V` 内联粘贴）。
  输入 **`@`** 会打开文件上下文选择器，根目录为 **被 peek
  的 agent** 的工作目录（因此 `@path` 相对于你正在
  回复的 agent 解析）；其下拉浮在面板 **上方**，
  打开时由 `↑`/`↓`/`Tab`/`Enter`/`Esc` 驱动。
  Dashboard 快捷键（`Ctrl+X` 停止、`Ctrl+T` 固定、`Shift+↑/↓` 重排
  等）在面板打开时仍优先于编辑器。
- 当有 **permission / ask-tool 问题** 待处理时，`❯ reply`
  行会隐藏，改为列出选项： **`↑`/`↓` 移动
  高亮选项**（以 `▸` 标记），**`Enter` 作答**。
  **`1`–`9`** 仍可直接选择选项。（回答期间，方向键
  选择选项而非切换 agent。）
- **自由文本行** 接受内联输入答案（与
  聊天面板相同）：permission 的 **"No" / reject**
  选项（"No, reject
  (type to add feedback)"）与 ask-tool 的 **"Other"** 行（"Other
  (type your own answer)"）。在其上输入并按 `Enter` 发送拒绝 +
  消息 / 自由文本答案。
- 这也覆盖 agent 的 **Ask tool**（`AskUserQuestion`）：其
  选项 + "Other" 行显示在 peek 中，以相同方式回答。
  **多问题** 表单一次走一个问题——`(i/N)`
  标记显示进度，每个答案前进到下一个，在最后一题
  提交。（含 **multi-select** 问题的表单留给
  agent 自身视图——打开 agent 来回答那些。）

仅当终端足够高时才渲染面板；在很矮的
终端上，即使选中了行也仍显示 dispatch 框。

---

## 搜索 / 筛选（`Ctrl+/`）

筛选放在显式的 **搜索模式** 背后，使正常输入
始终执行 dispatch。按 `Ctrl+/` 切换：prompt 前缀
从 `❯` 变为黄色的 `Search:`，每次按键都会实时过滤
行列表。

在搜索模式下：

- `Enter` — **确认**：保持筛选生效并返回
  dispatch prompt（行保持筛选状态；之后 `Esc` 会清除）。
- `Esc` 或 `Ctrl+/` — **取消**：清除筛选并退出搜索。
- `↑` / `↓` — 在筛选后的行中导航。

查询支持与此前相同的前缀（现在仅在
*搜索模式内* 生效）：

- `a:<name>` — 按 agent 标签筛选（不区分大小写的子串，
  匹配 persona / role）。
- `s:<state>` — 按行状态筛选。接受 `working`、`idle`、
  `completed`、`failed`、`needs-input`、`blocked` 及同义词
  （`busy`/`running`/`done`/等）。
- `#<text>` — 对 `#<text>` 的子串匹配（匹配标签中的字面量
  `#`；预留给未来的 PR 筛选）。
- 其他内容 — 对标签 + 工作目录的普通子串匹配。

---

## 持久化

每用户的 dashboard 偏好位于 `~/.grok/config.toml` 的
`[dashboard]` 下：

```toml
[dashboard]
enabled = true
grouping = "state"   # or "directory"
pinned   = ["top:<session_id>", "sub:<parent_session_id>:<child_session_id>"]
reorder  = ["top:<session_id>"]
```

固定/重排条目以 **session id** 为键，而非
按进程的 `AgentId(usize)`，因此能在重启后保留，且不会
附着到碰巧占用旧槽位号的任意 agent。

设置 `GROK_AGENT_DASHBOARD=0` 可在单次
pager 调用中强制禁用该功能；斜杠命令与 CLI 子命令会打印
友好的 toast。

---

## Phase 4（v1 范围外）

当前 dashboard 仅列出 **本** pager
进程拥有的 agent。计划中的 Phase 4（「supervisor / `grok --bg`」）会列出
在 pager 退出后仍存活的会话——那是独立路线图，
尚未交付。
