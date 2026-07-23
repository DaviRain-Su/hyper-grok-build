# 后台任务与监控

Grok 可以在不阻塞对话的情况下运行长生命周期进程。本文档介绍后台命令、`/loop` 命令、`monitor` 工具以及调度器。

---

## 后台命令

在 `run_terminal_command` 工具上设置 `background: true`，即可在后台运行命令。它会立即返回任务 ID；可用 `get_command_or_subagent_output` 获取输出。

### 工作原理

1. Agent 调用 `run_terminal_command` 并设置 `background: true`。
2. 命令在后台启动。
3. Agent 收到一个供后续引用的 `task_id`。
4. 命令完成后，对话中会出现通知。

### 获取输出

使用 `get_command_or_subagent_output` 工具查看后台命令或子 Agent 的状态：

- `get_command_or_subagent_output(task_id)` — 立即返回当前输出与状态，不进行等待
- `get_command_or_subagent_output(task_id, timeout_ms=30000)` — 最多等待指定毫秒数直至完成

### 等待多个任务

使用 `wait_commands_or_subagents` 同时阻塞等待多个任务：

- `task_ids` — 要等待的任务 ID 列表（最多 20 个）
- `mode` — `wait_any` 在第一个任务完成时返回；`wait_all` 等待所有任务完成
- `timeout_ms` — 最长等待时间，单位为毫秒（默认：30 秒）

该工具会返回你列出的每个任务的状态与输出。

### 终止后台任务

使用 `kill_command_or_subagent(task_id)` 终止正在运行的后台任务或子 Agent。该工具会向 shell 进程发送 SIGTERM，随后发送 SIGKILL；对子 Agent 则发送 Cancel 和 Shutdown。若任务已被终止或已经退出，则报告成功。

### 常见用例

- **开发服务器**：启动开发服务器后继续编码
- **测试套件**：在修复问题的同时于后台运行测试
- **构建流程**：启动构建，稍后再查看结果
- **长时间编译**：启动编译后继续处理其他任务

---

## 将正在运行的任务转入后台

在交互式 TUI 中，按 `Ctrl+B` 可将正在运行的前台命令转入后台。这是唯一的后台化快捷键。适合在以下情况使用：

- 某条命令耗时超出预期。
- 你希望在命令运行期间向 Agent 询问其他问题。
- 命令启动后才发现这是一个长时间运行的进程。

任务会继续运行，完成后你会收到通知。

---

## /loop 命令

`/loop` 会按固定间隔反复执行一条提示词。适用于轮询任务、定期检查与持续监控。

### 语法

```
/loop [interval] <prompt>
```

间隔格式支持：

| 格式 | 示例 | 说明 |
| ------ | ------- | ------------------ |
| `Ns`   | `60s`   | 每 N 秒（最少 60 秒） |
| `Nm`   | `5m`    | 每 N 分钟 |
| `Nh`   | `2h`    | 每 N 小时 |
| `Nd`   | `1d`    | 每 N 天 |

### 示例

```
/loop 5m Check if the test suite passes and report any failures
/loop 2h Summarize new commits since the last check
/loop 60s Check if the dev server at localhost:3000 is responding
```

### 行为

- 创建后立即触发一次提示词，之后按指定间隔重复
- 每次触发都会创建新的 Agent 轮次
- 周期性任务会在 7 天后自动过期
- 同时最多可有 50 个活跃的计划任务

---

## monitor 工具

`monitor` 工具会从长时间运行的脚本中流式推送事件。每一行输出都会成为对话中的一条通知。`monitor` 是 `/loop` 的流式对应物：用 `/loop` 做周期性检查，用 `monitor` 处理实时事件流。

### 工作原理

1. 你提供一条 shell 命令（`command`）以及会在每条通知中显示的简短 `description`。
2. Grok 将命令的 stdout 与 stderr 合并到同一个输出文件中。
3. 该文件中的每一行新内容都会作为通知投递到对话。
4. 监控会一直运行，直到命令退出或你将其停止。

### 脚本编写指南

- **管道中务必使用 `grep --line-buffered`。** 否则管道缓冲可能导致事件延迟数分钟。
- **在轮询循环中处理瞬时失败**（`curl ... || true`）。单次请求失败不应让监控停止。
- **使用有选择性的过滤器。** 每一行都会变成消息，因此切勿直接管道输出原始日志。
- **轮询间隔应与数据源匹配。** 对远程 API 使用 30 秒或更长间隔以尊重速率限制；本地检查可用 0.5 到 1 秒。
- **stdout 与 stderr 都会生成事件。** 对不希望成为事件的输出进行重定向——例如追加 `2>/dev/null`——或将其过滤掉。

### 示例

```bash
# Watch for errors in a log file
tail -f /var/log/app.log | grep --line-buffered "ERROR"

# Monitor file changes in a directory
inotifywait -m --format '%e %f' /watched/dir

# Poll GitHub for new PR comments
last=$(date -u +%Y-%m-%dT%H:%M:%SZ)
while true; do
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  gh api "repos/owner/repo/issues/123/comments?since=$last" \
    --jq '.[] | "\(.user.login): \(.body)"'
  last=$now; sleep 30
done
```

### 持久监控

对需要在整个会话生命周期内运行的监控，设置 `persistent: true`：

- PR 监控
- 日志跟踪
- CI 状态监视

使用 `kill_command_or_subagent(task_id)` 停止持久监控。

### 流量控制

如果某个监控产生的事件过多，Grok 会自动将其停止。发生这种情况时，请用更严格的过滤器重新启动监控。优先使用 `grep --line-buffered`、`awk`，或只输出你关心的事件的包装脚本。

---

## 调度器

调度器提供用于创建周期性任务的较低层 API。`/loop` 是调度器的便捷封装。

### scheduler_create

创建计划任务：

| 参数 | 说明 |
| ---------------- | -------------------------------------------------------- |
| `interval`       | 运行频率：`"5m"`、`"2h"`、`"1d"`、`"60s"` |
| `prompt`         | 每次触发时执行的提示词文本 |
| `fire_immediately`| 除按间隔触发外，创建时是否立即触发（默认：`false`） |
| `recurring`      | 是否重复（默认：`true`），或只触发一次（`false`） |
| `durable`        | 是否跨会话持久化（默认：`false`） |

### scheduler_list

列出所有活跃的计划任务及其 ID、提示词、间隔与下次触发时间。

### scheduler_delete

按 ID 取消计划任务。若找到并移除了该任务，则返回成功。

---

## 任务面板

在交互式 TUI 中，按 `Ctrl+G` 可切换任务面板。该面板在同一视图中列出：

- 正在运行的子 Agent 及其进度
- 活跃的后台任务及其状态
- 监控任务与 `/loop` 任务，各自带有实时行数徽章
- 每个条目的任务 ID

若要切换提示词队列，请按 `Ctrl+;`。

---

## “仍在运行”状态行

只要在 Agent 看起来空闲时仍有后台工作在运行——例如在轮次之间，或当前轮次因可被用户中断的等待而阻塞——提示词上方就会显示一条持久状态行：

```
◎ 1 command · 2 monitors · 1 loop · 1 subagent still running
```

它会统计正在运行的后台命令、监控、计划中的 `/loop` 任务以及后台子 Agent，并在各自结束时实时更新。其中任一项都可以唤醒 Agent 开启新轮次（命令与子 Agent 在完成时，监控在有事件时，loop 在定时器触发时），因此在全部结束前该提示会一直保留。运行中计数只出现在这条状态行上：完成情况会以单个 “Task completed” 芯片写入对话记录，“Worked for” 标记保持简洁——对话记录不会重复或重述运行中计数。

---

## 用例与模式

### 开发服务器 + 编码

在后台启动开发服务器并继续编码：

```
Start the dev server with `npm run dev` in the background, then implement the login form.
```

Agent 会以 `background: true` 运行开发服务器并继续编写代码。服务器启动后，你会看到通知。

### 持续测试监控

```
/loop 5m Run the test suite and report any new failures since the last run
```

每 5 分钟，Agent 会运行测试并仅报告新增失败。

### 日志监控

使用 `monitor` 监视特定事件：

```
Monitor the application log for ERROR and WARN entries. Use:
tail -f /var/log/app.log | grep --line-buffered -E "ERROR|WARN"
```

每条错误或警告都会作为对话中的通知出现。

### 监视 CI 流水线

```
/loop 2m Check the status of the GitHub Actions run for this PR. Report when it completes.
```

---

## 最佳实践

- **对一次性长命令使用 `background`**（构建、测试套件、服务器启动）
- **对周期性检查使用 `/loop`**（CI 状态、测试运行、健康检查）
- **对实时事件流使用 `monitor`**（日志跟踪、文件监视）
- **对延迟的一次性任务使用 `scheduler_create` 并设置 `recurring: false`**
- **保持监控过滤器严格** — 优先使用 `grep --line-buffered`，而不是原始日志流
- **不要在普通命令中用 sleep 循环轮询** — 改用带 `timeout_ms` 的 `get_command_or_subagent_output`
- **设置合理的轮询间隔** — 远程 API 使用 30 秒及以上以避免触发速率限制，本地检查可用更短间隔
