# 会话管理

Grok 会自动将每次对话保存到磁盘。无论你在 TUI、无头模式（headless）还是通过 agent stdio 工作，Grok 都会把交流记录为一个会话。你可以恢复、回退或压缩它。本文档说明如何管理会话。

---

## 什么是会话

会话是一次带有完整历史的持久化对话。它包含：

- 所有用户提示与 agent 回复
- 工具调用及其结果
- TODO/任务列表状态
- 用于回退的文件快照
- Token 用量与轮次计数
- 子 agent 会话（启用时）

会话由唯一的会话 ID 标识（Grok 生成时为 UUIDv7；客户端可用 `-s` 自行提供 ID），并存储在磁盘上的 `~/.grok/sessions/` 下。设置 `GROK_HOME` 可覆盖基础目录；未设置时，Grok 使用 `~/.grok`。

---

## 存储布局

Grok 将每个会话存放在各自的目录中，并按工作目录分组。分组名由工作目录经 URL 编码得到。当编码后的名称超过 255 字节时，改用 slug 加哈希，并在该分组内的 `.cwd` 文件中记录原始路径。

```
~/.grok/sessions/<encoded-cwd>/<session-id>/
  summary.json            # metadata: summary/title, timestamps, model ID, message counts
  updates.jsonl           # ACP session update stream (conversation + tool calls)
  chat_history.jsonl      # raw chat messages sent to the model
  plan.json               # TODO/task list state
  rewind_points.jsonl     # file snapshots for /rewind undo
  signals.json            # session signals (token usage, tool/turn counters)
  feedback.jsonl          # user feedback and ratings
  compaction_checkpoints/ # saved state from compaction (manual or auto)
  subagents/              # per-subagent metadata (meta.json); the child sessions live in the normal sessions tree
```

`summary.json` 是索引条目。它记录会话摘要与生成标题、模型 ID、创建与更新时间戳、消息计数，以及分叉或恢复会话的父会话引用。`updates.jsonl` 是权威的对话日志，驱动 `/resume` 与会话恢复。

---

## 开始与结束会话

### 新会话

每次启动时，TUI 都会创建新会话。若要在会话中途显式重新开始：

```
/new
```

这会清空当前上下文并开始新对话。别名：`/clear`。

### 退出

结束会话并退出 Grok：

```
/quit
```

别名：`/exit`。若要离开当前会话但留在 Grok 中，使用 `/home` 返回欢迎界面。

---

## 恢复会话

### 从 TUI

使用 `/resume` 命令浏览并恢复以往会话：

```
/resume
```

这会打开会话选择器，列出当前工作区的近期会话。选择一个会话即可恢复。该命令不接受参数。

在选择器中输入可按标题过滤列表，并会随输入搜索对话内容；内容匹配结果会出现在「Extended search results」标题下。按 `Ctrl+/` 可立即搜索，无需短暂等待。

要在当前活动的会话（父会话及任意分叉）之间切换、重命名或关闭它们，请改用 `/dashboard`（或其别名 `/sessions`）。

### 从命令行

按 ID 恢复指定会话：

```bash
grok --resume <session-id>
```

运行不带 ID 的 `grok --resume` 可恢复当前目录下最近的一次会话。

### 从欢迎界面

启动 `grok` 时，欢迎界面会列出当前目录的近期会话。选择一个即可恢复。

---

## 分叉与重命名会话

### 分叉

将当前会话分支为一个对等 agent，从其对话副本开始：

```
/fork [--worktree|--no-worktree] [directive]
```

可传入可选的 `directive` 作为新会话的首条提示。使用 `--worktree` 或 `--no-worktree` 选择分叉是否在新的 git worktree 中运行；两者都省略时每次都会询问。本版本不支持 `--at <turn>` 标志。

### 重命名

重命名当前会话的标题：

```
/rename <title>
```

别名：`/title`。

---

## /rewind 命令

`/rewind` 通过将文件恢复到对话中较早时刻的状态来撤销近期更改。用它从失误中恢复。

```
/rewind
```

当你运行 `/rewind`（或在空闲、提示为空且已有对话消息时，在 800ms 内按 **Esc Esc**），Grok 会：

1. 显示回退点列表（每个用户提示对应一个）
2. 让你选择要回退到的点
3. 将所有文件恢复到该点的状态
4. 将对话历史截断到该点

文件快照会在每次提示时记录，因此你可以回到任意先前状态。

**重要：** `/rewind` 会修改磁盘上的文件。除非你已将其纳入 git，否则被回退的更改会丢失。

---

## /compact 命令

`/compact` 压缩对话历史以节省上下文窗口空间。适用于早期消息已不再相关的长会话。

```
/compact
/compact [context]
```

可选的 `context` 参数可让你额外说明压缩时应保留的内容。

### 自动压缩

当上下文窗口接近上限时，Grok 会自动压缩对话。自动压缩触发时你会看到通知。模型配置上的 `context_window` 设置控制何时达到该阈值。

---

## /session-info 命令

查看当前会话的详细信息：

```
/session-info
```

会显示：

- 会话标题（若已设置）
- Shell 版本
- 会话 ID
- 工作目录
- 模型（编码模型会附带 model hash）
- API 后端与沙箱配置（若已设置）
- 上下文窗口用量（已用与总 token，以及已用百分比）

---

## 无头模式的会话管理

在无头模式下，通过命令行标志管理会话：

```bash
# New session each time (default)
grok -p "Hello"

# Resume an existing session by ID (errors if it does not exist)
grok -p "Continue where we left off" -r <session-id>

# Continue the most recent session in the current directory
grok -p "What were we doing?" -c
```

在无头模式下，使用 `-r`/`--resume` 恢复已有会话（若会话不存在会报错），或使用 `-c`/`--continue` 继续当前目录下最近的会话。将 JSON 输出中的会话 ID（见下文）传给 `-r`。

仅在**创建**带有 **UUID** 的新会话时使用 `-s`/`--session-id`（若值不是 UUID，或目标会话目录下该 ID 已有会话，则会报错）。它**不会**恢复已有会话——那是旧的隐藏 upsert 行为；请改用 `-r`/`-c`。仅在同时传入 `--fork-session` 时才将 `-s` 与 `-r`/`-c` 组合使用（将历史分叉到新 ID；可选的 `-s` 为子会话指定 UUID）。这与 Claude Code 的防覆盖模型一致（在写入 cwd 下做客户端预检；顺序使用可靠，相同 ID 的并发为尽力而为）。

要回读会话 ID，请请求 JSON 输出：

```bash
grok -p "Hello" --output-format json | jq -r '.sessionId'
```

---

## Agent stdio 会话管理

在基于 ACP 构建时，通过协议方法管理会话：

```typescript
// Create new session
const { sessionId } = await connection.request("session/new", {
  cwd: "/path/to/project",
  mcpServers: [],
});

// Load existing session
await connection.request("session/load", {
  sessionId: "existing-session-id",
  cwd: "/path/to/project",
  mcpServers: [],
});
```

Agent 会自动持久化所有会话更新。客户端可按 ID 重新连接并加载以往会话。

---

## grok sessions 子命令

从命令行列出或搜索会话。`grok sessions` 需要子命令：

```bash
# List recent sessions for the current directory
grok sessions list

# Limit the number of results (default 20)
grok sessions list --limit 50

# Search sessions by keyword (matches titles and prompts)
grok sessions search "rate limit"
```

`grok sessions list` 显示当前工作目录下的会话，并按 worktree 标签分组。每行列出会话 ID、创建与更新日期、来源状态以及摘要。`grok sessions search` 会结合本地 SQLite 索引与远程结果。

---

## Worktree 会话

在使用子 agent 或会话分叉时，Grok 可为每个会话创建隔离的 git worktree。每个 worktree 都有工作目录的独立副本，因此一个会话中的文件更改不会影响另一个。

Worktree 会话通过 `x.ai/git/worktree/*` 扩展方法在内部管理。主要操作：

- **创建**：为隔离会话创建新 worktree
- **应用**：将 worktree 更改合并回主工作目录
- **移除**：会话结束后清理 worktree

使用 `grok -w -r <session-id>` 可在全新 worktree 中恢复会话。

---

## 会话存储细节

### 持久化格式

Grok 以换行分隔的 JSON（JSONL）存储对话。`updates.jsonl` 中的每一行都是一个自包含的 ACP 会话更新事件。该格式支持：

- 增量写入（会话期间仅追加）
- 高效流式读取（用于会话恢复）
- 便于调试（每行都是合法 JSON）

较小的状态文件——`summary.json`、`plan.json` 和 `signals.json`——是普通 JSON 而非 JSONL。JSONL 是会话内容的权威来源；`grok sessions search` 还会在本地维护一个覆盖会话标题与提示的 SQLite FTS5 索引，以实现快速关键词搜索。

### 会话元数据

`summary.json` 记录的字段包括但不限于：

- `info` -- 会话 ID 与工作目录
- `session_summary` 与 `generated_title` -- 会话摘要及其模型生成的标题
- `created_at` 与 `updated_at` -- 创建与最后更新时间戳
- `num_messages` 与 `num_chat_messages` -- 更新与聊天消息计数
- `current_model_id` -- 正在使用的模型
- `parent_session_id` -- 分叉或恢复的源会话
- `agent_name` -- 会话上次保存时激活的 agent 定义

### 磁盘占用

回退点快照（已修改文件的副本）是修改大量文件的会话中磁盘占用的主要来源。使用 `/compact` 可减小历史大小。

---

## 提示

- 当当前上下文不再相关时，使用 `/new` 重新开始。
- 在长会话中主动使用 `/compact`，以保持上下文窗口的有效性。
- 使用 `/rewind` 撤销失误；它会恢复真实的文件快照，而不是依赖 agent 重建先前状态。
- 在无头模式下，从 JSON 输出中捕获 `sessionId` 并传给 `-r`，以构建能保持上下文的多步自动化。
- 用 `/session-info` 查看上下文窗口已使用多少。
