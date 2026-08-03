I'll read the full offloaded document so the Chinese translation covers every section end-to-end.# Hooks

Hooks 允许你在 Grok 会话的关键时刻运行脚本或发送 HTTP 请求。可用它们自动化任务、执行安全检查、记录活动、发送通知，以及集成你自己的工具。

---

## 什么是 Hooks？

Hook 是 shell 命令或 HTTP 端点，在特定生命周期事件发生时由 Grok 调用。Hooks 可以：

- **阻止操作** —— `PreToolUse` hook 可在危险命令运行前拒绝它。
- **让 agent 继续工作** —— `Stop` hook 可阻止 agent 结束当前回合，直到某条件成立（例如测试套件通过），并将原因回传给模型。
- **响应事件** —— `PostToolUse` hook 可将每次工具执行记录到文件。
- **准备上下文** —— `SessionStart` hook 可导出环境变量或运行初始化脚本。

---

## 常见用例

- **安全防护**：在 `rm -rf /` 等命令运行前拦截。
- **审计日志**：将工具使用与会话记录到文件或外部服务。
- **通知**：任务完成时发送消息。
- **自动格式化**：编辑后运行 `cargo fmt` 或 `prettier`。
- **环境准备**：会话开始时导出变量。
- **自定义工作流**：在特定事件上触发构建、测试或部署。

---

## 快速开始

1. 创建 hooks 目录：

   ```sh
   mkdir -p ~/.grok/hooks
   ```

2. 创建 hook 文件，例如 `~/.grok/hooks/session-start.json`：

   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "hooks": [
             { "type": "command", "command": "echo 'Grok session started in '$(pwd)" }
           ]
         }
       ]
     }
   }
   ```

3. 启动（或重启）Grok 会话。该 hook 会在 `SessionStart` 时自动运行。

4. 在非 VS Code 系终端中按 `Ctrl+L`（或在任意位置运行 `/hooks` —— 在 VS Code 系终端中优先使用此方式），检查 Hooks 标签页以确认已加载。

---

## Hook 位置

Hooks 会从多处发现（全部合并）：

| 范围 | 路径 | 是否受信任？ | 说明 |
|-------|------|----------|-------|
| 全局 | `~/.grok/hooks/*.json` | 始终 | 个人 hooks |
| 全局 | `~/.claude/settings.json`（以及 `settings.local.json`） | 始终 | Claude Code 兼容（可配置） |
| 全局 | `~/.cursor/hooks.json` | 始终 | Cursor 兼容（可配置） |
| 项目 | `<project>/.grok/hooks/*.json` | 需要信任 | 按仓库的自动化 |
| 项目 | `<project>/.claude/settings.json`（以及 `settings.local.json`） | 需要信任 | Claude 兼容（可配置） |
| 项目 | `<project>/.cursor/hooks.json` | 需要信任 | Cursor 兼容（可配置） |
| 插件 | 打包在已安装插件内 | 按插件 | 团队共享 hooks |

默认会扫描 Claude 与 Cursor 的 hook 来源。若要禁用某个厂商的扫描，在 `~/.grok/config.toml` 中设置 `[compat.<vendor>] hooks = false`，或使用对应环境变量。详见[配置](05-configuration.md#harness-compatibility)。

**信任项目**：首次打开带 hooks 的项目时，必须先信任该项目，其项目级 hooks 才会运行 —— 在此之前会被静默跳过。通过运行 `/hooks-trust`（或使用 `--trust` 启动）授予信任；决定会写入统一的 folder-trust 存储（`~/.grok/trusted_folders.toml`），与控制仓库本地 MCP/LSP 服务器的同一道门闸。`~/.grok/hooks/` 中的全局 hooks 始终受信任，无需条目。这可防止不受信任的仓库执行任意代码。

由于 hooks 已统一到 folder-trust 下，`--trust` / `/hooks-trust` 会一并信任整个文件夹的 **MCP、LSP 和 hooks**，并级联到子目录。反过来，禁用 folder-trust（`GROK_FOLDER_TRUST=0` 或 `[folder_trust] enabled = false`）会同时放开项目 hooks 以及 MCP/LSP。

---

## Hook 事件

| 事件 | 触发时机 | 是否阻塞？ |
|-------|---------------|-----------|
| `SessionStart` | 会话开始。 | 否 |
| `UserPromptSubmit` | 你提交了一条提示。 | 否 |
| `PreToolUse` | 工具即将运行。 | 是 —— 可拒绝 |
| `PostToolUse` | 工具成功完成。 | 否 |
| `PostToolUseFailure` | 工具失败。 | 否 |
| `PermissionDenied` | 权限系统拒绝了一次工具调用。 | 否 |
| `Stop` | agent 回合以真正完成结束（非用户中断）。 | 是 —— 可阻止停止 |
| `StopFailure` | 回合因 API 错误结束。 | 否 |
| `Notification` | agent 发送通知。 | 否 |
| `SubagentStart` | 子 agent 启动。 | 否 |
| `SubagentStop` | 子 agent 的回合结束（在子 agent 内触发一次，带停止决策控制）。 | 是 —— 可阻止停止 |
| `PreCompact` | 即将进行对话压缩。 | 否 |
| `PostCompact` | 对话压缩完成。 | 否 |
| `SessionEnd` | 会话结束。 | 否 |

`SubagentEnd` 可作为 `SubagentStop` 的别名被接受。`PreToolUse` 可阻止工具调用，`Stop`/`SubagentStop` 可阻止 agent 停止（见[停止决策控制](#stop-decision-control)）；其余事件均为被动。

### Cursor Hook 兼容性

Grok 接受 Cursor 的 camelCase hook 事件名，因此 `~/.cursor/hooks.json` 可原样加载：

| Cursor 事件 | 映射到 |
|---|---|
| `sessionStart`、`sessionEnd` | `SessionStart`、`SessionEnd` |
| `preToolUse`、`postToolUse`、`postToolUseFailure` | `PreToolUse`、`PostToolUse`、`PostToolUseFailure` |
| `beforeShellExecution`、`beforeMCPExecution`、`beforeReadFile` | `PreToolUse` |
| `afterShellExecution`、`afterMCPExecution`、`afterFileEdit` | `PostToolUse` |
| `afterAgentResponse`、`afterAgentThought` | `PostToolUse` |
| `beforeSubmitPrompt` | `UserPromptSubmit` |
| `subagentStart`、`subagentStop` | `SubagentStart`、`SubagentStop` |
| `preCompact`、`stop` | `PreCompact`、`Stop` |

Cursor 的按操作 hooks（`beforeShellExecution`、`afterFileEdit` 等）映射到通用的 `PreToolUse`/`PostToolUse` 事件。Hook 脚本在 JSON 输入中会收到工具名，可据此过滤，或使用 `matcher` 字段。

---

## Hook JSON 格式

每个 `.json` 文件可为多个事件定义 hooks：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "bin/safety-check.sh", "timeout": 10 }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          { "type": "command", "command": "bin/log-activity.sh" }
        ]
      }
    ]
  }
}
```

### 关键字段

- **事件名**（顶层键）：[Hook 事件](#hook-events)中列出的任意事件。Grok 会跳过无法识别的事件名，因此共享的 Claude 或 Cursor 配置文件仍可加载。
- **matcher**（可选）：正则表达式，用于选择哪些调用会触发该 hook。匹配对象取决于事件：在工具事件（`PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`PermissionDenied`）上是工具名；在 `Notification` 上是通知类型；在 `SubagentStart`/`SubagentStop` 上是子 agent 类型（例如 `explore`）；在 `SessionStart` 上是启动来源（`startup`、`resume` 等）；在 `SessionEnd` 上是结束原因；在 `PreCompact`/`PostCompact` 上是压缩触发源（`manual` 或 `auto`）；在 `StopFailure` 上是错误类型（`rate_limit`、`authentication_failed`、`invalid_request`、`server_error`、`max_output_tokens` 或 `unknown`）。`Stop` 或 `UserPromptSubmit` 上的 matcher 会被忽略并给出警告（这些事件始终触发）。空或省略的 matcher 匹配一切。Matcher 测试的是真实工具名；经内部 `use_tool` 调度器路由的 MCP 调用表现为限定名 `server__tool`（例如 `linear__save_issue`），因此应匹配该名称，而非调度器名称。
- **type**：`"command"`（运行脚本或 shell 一行命令）或 `"http"`（将事件 POST 到 URL）。
- **command**：可执行文件路径（相对于 JSON 文件）或内联 shell 命令。
- **timeout**：在杀死 hook 前的秒数（默认：5；`Stop`/`SubagentStop` 门闸为 600，与 Claude Code 一致）。所有 hook 失败（超时、崩溃、输出格式错误、缺少必需环境变量）均为 fail-open：失败会记录到 UI 回滚历史，但不会阻止工具调用。只有 hook 返回的显式 `deny` 决策才会阻止工具调用。

### 工具名别名

在 `matcher` 中，Grok 会将 Claude 风格的工具名映射到自身工具名，使从 Claude 迁移的 hooks 能正确触发。常见别名包括：

- `Bash` → `run_terminal_command`
- `Read` → `read_file`
- `Edit`、`Write` 和 `MultiEdit` → `search_replace`
- `Grep` → `grep`
- `Glob` 和 `ListDir` → `list_dir`
- `WebSearch` → `web_search`
- `Task` → `spawn_subagent`

Matcher 也保留原始名称，因此 `Bash` 会同时匹配 `Bash` 和 `run_terminal_command`。

---

## 编写 Hook 脚本

### 输入

事件以 JSON 形式通过 **stdin** 发送（例如 `PreToolUse` 事件；载荷也始终包含 `toolUseId` 和 `toolInputTruncated`）：

```json
{
  "hookEventName": "pre_tool_use",
  "sessionId": "abc-123",
  "cwd": "/Users/you/project",
  "workspaceRoot": "/Users/you/project",
  "permissionMode": "default",
  "toolName": "run_terminal_command",
  "toolInput": { "command": "npm test" },
  "timestamp": "2026-04-14T12:00:00Z"
}
```

每个事件都携带相同的公共字段：`hookEventName`、`sessionId`、`cwd`、`workspaceRoot`、`timestamp` 和 `permissionMode`（`default`、`auto`、`plan` 或 `bypassPermissions`），以及如上所示的 `toolName` 等事件特定字段。

### 输出（阻塞型 Hooks）

对于 `PreToolUse` hooks，将 JSON 写到 **stdout**：

- **允许**：`{"decision": "allow"}`
- **拒绝**：`{"decision": "deny", "reason": "Unsafe command detected"}`

### 退出码

| 退出码 | 含义 |
|-----------|---------|
| `0` | 成功 / 允许（对阻塞型 hooks） |
| `2` | 显式拒绝（`PreToolUse`），或以 stderr 作为反馈的阻止停止（`Stop`/`SubagentStop`） |
| 其他 | Fail-open —— 失败会记录但不会阻止任何操作。对 `PreToolUse`，无论退出码如何，stdout JSON 中的 `deny` 决策都会被遵守。对 `Stop`/`SubagentStop`，stdout 上的有效决策 JSON 优先于退出码（与 Claude Code 一致）；仅当 stdout 没有可用 JSON 时才由退出码决定，此时退出码 2 会阻止停止并以 stderr 作为反馈。 |

### 停止决策控制

`Stop` 和 `SubagentStop` hooks 在 agent 即将结束其回合时运行，并可让其继续工作（与 Claude Code 兼容）。将 JSON 写到 **stdout**：

- **阻止停止**：`{"decision": "block", "reason": "The test suite hasn't been run yet"}`。原因会作为用户消息回传给模型，agent 在同一回合中再跑一轮。
- **非错误反馈**：`{"hookSpecificOutput": {"hookEventName": "Stop", "additionalContext": "Run the linter before finishing"}}`。同样会让 agent 继续工作，但以 hook 反馈而非 hook 错误的形式呈现。
- **强制停止**：`{"continue": false, "stopReason": "Budget exhausted"}`。结束回合，覆盖任何阻止。
- **允许停止**：以退出码 0 且无输出（或任何非 JSON 输出）退出。

以退出码 `2` 退出也会阻止停止，并以 **stderr** 作为反馈。

Hook 输入包含 `stopHookActive` 和 `lastAssistantMessage`。当 agent 因本回合此前的 stop-hook 阻止而已在继续时，`stopHookActive` 为 true；请检查它或 transcript，避免在永远无法满足的条件上反复阻止。`lastAssistantMessage` 携带本回合 agent 最终回复的文本，因此 hooks 无需解析 transcript 即可据此行动。同一回合内经过 **8 次继续**（阻止或非错误反馈）后，门闸会被覆盖并结束回合；那次最终强制停止不会再咨询 hooks。计数器按回合计算：下一条用户提示会重新开始，因此长时间目标可跨回合进行。Hook 失败为 fail-open：agent 正常停止。

`Stop` 和 `SubagentStop` hooks 默认超时 600 秒（与 Claude Code 一致），因为门闸常会跑构建或测试套件；超时的 hook 为 fail-open，因此 agent 仍会停止。其他事件保持 5 秒默认值。当门闸需要更长时间时，请显式设置 `timeout`：`{ "type": "command", "command": "bin/verify.sh", "timeout": 1200 }`。

门闸仅对真正完成的回合运行。被中断（Esc / Ctrl+C）、被拒绝以及达到 max-turns 的回合会完全跳过 Stop hooks；因 API 错误结束的回合则触发 `StopFailure`。会话结束时也会触发一次单独的 Stop（`reason: "channel_closed"` 或 `"shutdown"`）；其决策输出会被解析但忽略，因为已没有可继续的回合。对 Stop 触发做计数或门控的脚本应检查 `reason == "end_turn"`，以免会话结束触发影响结果。

`StopFailure` 仅为观察用（用于记录失败或发送告警；输出与退出码被忽略）。其输入携带 `error`（matcher 所测试的分类类型，采用 Claude Code 的词汇：`rate_limit`、`authentication_failed`、`invalid_request`、`server_error`、`max_output_tokens`，或运行时无法区分时的 `unknown`；容量错误并入 `rate_limit`，且没有 `billing_error` 信号）、`errorDetails`（原始错误详情，若可用），以及 `lastAssistantMessage`（对话中显示的渲染错误文本；对此事件它是错误字符串，而非 assistant 输出）。

`Stop` 输入还携带 `backgroundTasks` 和 `sessionCrons`，因此 hook 可区分“会话已完成”与“会话暂停，等待后台工作将其唤醒”。当没有进行中或已调度的任务时，两个数组皆为空。每个 `backgroundTasks` 条目描述一个进行中的任务：`id`、`type`（`shell`、`monitor` 或 `subagent`）、`status`，以及（取决于类型）`command`（仅 shell 任务）、`description`（monitor 所监视的命令行，或 subagent 的任务描述）和 `agentType`（subagents）。每个 `sessionCrons` 条目描述一次计划唤醒（`scheduler_create` 或 `/loop`）：`id`、`schedule`、`recurring` 和 `prompt`。`schedule` 值为人类可读的间隔，例如 `every 5 minutes`；grok 的调度是间隔，而非 cron 表达式。自由文本字段上限为 1000 个字符，超出会在字符串内以 `… [+N chars]` 标记截断。

在子 agent 内部，门闸以 `SubagentStop` 触发（agent frontmatter 中的 `Stop` hooks 会自动重映射）。`Stop` hook 仅门控主 agent。

`SubagentStop` 对每个子 agent 触发一次，发生在该子 agent 自身的回合结束时，与 Claude Code 一致。其输入携带 `phase` 字段（当前始终为 `"gate"`），预留给向前兼容。

**移植 Claude Code 的 stop hooks**：输出词汇（`decision`、`reason`、`continue`、`stopReason`、`additionalContext`）可原样使用。以下是与 Claude 不匹配之处：

- **camelCase 输入**：grok 的 stdin 信封全程使用 camelCase 键，而 Claude 使用 snake_case。读取 `.stop_hook_active`、`.hook_event_name` 或 `.background_tasks[].agent_type` 的脚本必须改为 `.stopHookActive`、`.hookEventName` 和 `.backgroundTasks[].agentType`（事件值为 `"stop"`）。通过 grok-agent-sdk 注册的 hooks 会将顶层键以及 `backgroundTasks`/`sessionCrons` 条目键转换为 snake_case，因此线路上的 `.backgroundTasks[].agentType` 在 SDK 中读作 `.background_tasks[].agent_type`。
- **`toolResult` 字段**：`PostToolUse` 的工具输出是 `toolResult`（SDK：`tool_result`），而非 Claude 的 `tool_response`；读取 `.tool_response` 的 hook 必须改为 `.toolResult`。
- **会话结束触发**：会话结束时会额外触发一次仅观察的 Stop；请用 `reason == "end_turn"` 过滤（见上文）。
- **间隔调度**：`sessionCrons[].schedule` 是人类可读的间隔，绝不是 cron 表达式。
- **任务类型**：`backgroundTasks[].type` 仅为 `shell`、`monitor` 或 `subagent`；Claude 的其他标签（`workflow`、`teammate` 等）不会发出。
- **StopFailure 分类**：发出的集合是 Claude Code 的词汇 —— `rate_limit`、`authentication_failed`、`invalid_request`、`server_error`、`max_output_tokens`、`unknown`。grok 发出子集：容量错误（503/529）像 Claude 一样并入 `rate_limit`，且永不发出 `billing_error`（无信号），因此 `billing_error` matcher 不会触发。
- **permission_mode 值**：grok 发出 `default`、`auto`、`plan` 或 `bypassPermissions`。Claude 的 `acceptEdits`/`dontAsk` 在 grok 中没有等价项（最接近的是 grok 的 `auto`），因此像 `permission_mode === "acceptEdits"` 这类检查永远不会匹配。
- **客户端（SDK）门闸超时**：SDK 的 `Stop`/`SubagentStop` 门闸默认 600 秒，与文件 hooks 相同；`PreToolUse` 客户端门闸默认 30 秒（交互热路径）。两者都可通过 `timeoutS` 按 matcher 组覆盖，上限为 600。
- **`/goal`**：grok 的 goal 循环是独立功能，在 stop 门闸之前运行；它不是提示类型的 Stop hook。

一个完整的“保持继续工作”策略脚本示例：

```bash
#!/bin/bash
input=$(cat)
# Gate only genuine turn ends, not the session-end observe fire.
if [ "$(echo "$input" | jq -r '.reason')" != "end_turn" ]; then exit 0; fi
if ! bin/verify.sh >/dev/null 2>&1; then
  echo '{"decision": "block", "reason": "verify.sh failed; fix the failures before finishing"}'
fi
```

注册为 `{ "type": "command", "command": "bin/stop-gate.sh", "timeout": 300 }`，其中 `timeout` 按验证步骤所需时间设定。每次继续后 hook 会再次触发，内置上限在 8 次后结束回合；检查 `stopHookActive` 可在 agent 显然无法处理的反馈上更早放弃。

### 被动 Hooks

对于 `SessionStart` 或 `PostToolUse` 这类事件，stdout 会被忽略。成功时只需以退出码 0 退出。

### 环境变量

Grok 会在每个 hook 进程上设置若干环境变量。编写感知上下文或感知插件的 hook 脚本时很有用。

#### 运行器注入的变量（始终可用）

这些变量由 hook 运行器为**每一个** hook 设置：

| 变量 | 说明 |
|-----------------------|-------------|
| `GROK_HOOK_EVENT`     | 触发该 hook 的事件名（例如 `pre_tool_use`、`session_start`、`post_tool_use`、`session_end`、`stop`、`notification`）。 |
| `GROK_HOOK_NAME`      | 该特定 hook 的配置名称（插件提供的 hooks 会包含插件前缀）。 |
| `GROK_SESSION_ID`     | 当前 Grok 会话的唯一标识符。 |
| `GROK_WORKSPACE_ROOT` | 当前工作区根目录的绝对路径。 |
| `CLAUDE_PROJECT_DIR`  | 工作区根目录的绝对路径。`GROK_WORKSPACE_ROOT` 的 Claude Code 兼容别名，对每个 hook 都会设置。 |

这些变量是**保留的**。你尝试通过 hook JSON 中的 `env` 字段为它们设置的任何值会在加载时被剥离（并记录警告），运行器在生成进程时始终注入真实值。

#### 插件 hook 变量

当 hook 来自插件时，Grok 还会额外注入以下变量：

| 变量 | 说明 |
|----------------------|-------------|
| `GROK_PLUGIN_ROOT`   | 插件安装目录的绝对路径。 |
| `GROK_PLUGIN_DATA`   | 插件可写数据目录的绝对路径（用于存储插件状态、缓存等）。 |

这些值由插件系统提供。对于四个与插件相关的键（`GROK_PLUGIN_ROOT`、`GROK_PLUGIN_DATA` 及其 Claude 别名），插件适配器确保官方插件值始终覆盖 hook 的 `env` 映射中任何用户声明的值。

#### 用户定义的环境变量

你可以使用 `env` 字段为单个 hook 处理程序提供额外环境变量：

```json
{
  "type": "command",
  "command": "bin/my-hook.sh",
  "env": {
    "MY_SECRET": "value",
    "LOG_LEVEL": "debug"
  }
}
```

这些变量会传递给 hook 进程，但不能覆盖上文列出的保留运行器或插件变量。

#### 在 `command` 和 `url` 字段中使用变量

`command` 和 `url` 均支持 `${VAR}` 与 `$VAR` 展开。关于加载时与运行时展开、`env` 映射查找顺序，以及参数展开修饰符（例如 `${VAR:-default}`）的处理方式，详见 custom-hooks 参考。

---

## HTTP Hooks

除了本地脚本，也可调用远程端点：

```json
{ "type": "http", "url": "https://hooks.example.com/grok-event", "timeout": 15 }
```

完整的事件信封会以 JSON 形式 POST。

---

## 在 TUI 中管理 Hooks

### Hooks 标签页

在非 VS Code 系终端中按 `Ctrl+L` 打开扩展模态框（Plugins 标签页），或运行 `/hooks`（任意终端；在 VS Code 系终端中必需，因为 `Ctrl+L` 用于 interject）以在 Hooks 标签页打开。在 **Hooks** 标签页中：

| 按键 | 操作 |
|-----|--------|
| `r` | 从磁盘重新加载所有 hooks |
| `a` | 按路径添加自定义 hook |
| `x` | 移除所选 hook |
| `Space` | 启用或禁用所选 hook |
| `f` | 循环状态筛选（全部 / 已启用 / 已禁用） |

Hooks 按来源分组：**Global**、**Project**、**Plugin** 和 **Custom**。

每个 hook 显示：
- 触发的 **Event**
- 运行的 **Command** 或 **URL**
- **Timeout** 时长
- **Status** —— 已启用或 `[disabled]`

### 斜杠命令

```
/hooks-list           # Show hooks loaded in this session
/hooks-trust          # Trust this project for hook execution
/hooks-add <path>     # Add a custom hook file or directory
/hooks-remove <path>  # Remove a custom hook
/hooks-untrust        # Revoke trust for this project
```

在 TUI pager 中，各个 `/hooks-*` 命令不会出现在斜杠命令列表里。`/hooks` 模态框覆盖列出、添加、移除以及启用或禁用 hooks；项目信任通过 `/hooks-trust`（或模态框的 Trust 操作）管理，会写入上文所述的统一 folder-trust 存储。

### 按 Hook 启用/禁用

在 Hooks 标签页中按 `Space` 可在运行时启用或禁用单个 hook。更改立即生效，无需重启会话。

### 会话中重新加载

在 Hooks 标签页中按 `r` 可从磁盘重新加载所有 hooks。Grok 会重新读取每个 hook 来源，因此可拾取你在会话期间对 hook 文件所做的更改。

---

## 回滚历史中的 Hook 标注

Hooks 执行时，其结果会作为标注出现在 TUI 回滚历史中。你可以看到哪些 hooks 已运行、它们是允许还是拒绝了操作，以及它们产生的任何输出。这些标注仅在启用插件 UI 时出现（默认启用）。

---

## 示例：安全 Shell 防护

拦截危险的 shell 命令：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "bin/safe-shell.sh", "timeout": 5 }
        ]
      }
    ]
  }
}
```

其中 `bin/safe-shell.sh`：

```bash
#!/bin/sh
INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.toolInput.command // empty')

# Block destructive patterns
if echo "$CMD" | grep -qE '(rm -rf /|mkfs|dd if=|:(){ :|& };:)'; then
  echo '{"decision": "deny", "reason": "Blocked potentially destructive command"}' 
  exit 2
fi

echo '{"decision": "allow"}'
```

---

## 安全说明

- 全局 hooks（`~/.grok/hooks/`）以你的用户权限运行 —— 应像对待 shell 脚本一样对待它们。
- 项目 hooks 需要 folder trust（`/hooks-trust` 或 `--trust`，与仓库本地 MCP/LSP 同一道门闸），以防恶意仓库的供应链攻击。
- HTTP hooks 会发送会话数据 —— 仅使用受信任的端点。

---

## 最佳实践

1. **保持 hooks 快速** —— 长时间运行的 hooks 会阻塞 UI。尽可能使用后台进程（`&`）或异步。
2. **用显式 `deny` 来阻止** —— hooks 在任何错误时都是 fail-open，因此崩溃的 hook 不会阻止工具。要执行策略，你的 hook 必须完整运行并在 stdout 上发出 `{"decision":"deny","reason":"..."}`。始终在脚本内部处理错误，以便返回显式决策。
3. **使用绝对路径或相对于 hook 文件的路径** —— 与 JSON 文件相邻的 `bin/` 中的脚本便于移植。
4. **用模态框测试** —— 按 `Ctrl+L`（非 VS Code 系）或运行 `/hooks`，在依赖 hooks 之前确认它们已加载并匹配。
5. **将项目 hooks 纳入版本控制** —— 提交 `.grok/hooks/`（但切勿提交密钥）。

---

## 故障排除

- **Hook 没有运行？** 在非 VS Code 系终端中按 `Ctrl+L`（或在任意位置运行 `/hooks`），查看它是否已加载并匹配。
- **项目 hooks 被忽略？** 文件夹可能不受信任。运行 `/hooks-trust`（或使用 `--trust` 重新启动）。
- **找不到脚本？** 检查路径是否相对于 `.json` 文件且可执行（`chmod +x`）。
- **需要查看错误？** 使用 `RUST_LOG=debug GROK_LOG_FILE=/tmp/grok.log grok` 启动以捕获日志，然后检查 `/tmp/grok.log`。
