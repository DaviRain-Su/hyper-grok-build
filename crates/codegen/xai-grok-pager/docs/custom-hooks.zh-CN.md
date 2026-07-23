# 自定义 Hooks 指南

Hooks 让你可以在 Grok 会话的关键时刻运行自定义脚本或发起 HTTP 请求——例如工具运行前后、会话开始或结束时，或 agent 发送通知时。

它们非常适合自动化、安全检查、日志记录、通知，以及与你自己的工具集成。

## 为什么使用 Hooks？

常见用途：

- **安全防护**：在危险命令（如 `rm -rf /`）执行前拦截。
- **审计日志**：将每次工具调用或会话记录到文件或外部服务。
- **通知**：长时间任务完成时发送 Slack/Discord 消息。
- **自动格式化**：编辑后自动运行 `cargo fmt` 或 `prettier`。
- **环境准备**：在会话开始时导出密钥或设置变量。
- **自定义工作流**：在特定事件上触发构建、测试或部署。

## 快速开始

1. 创建 hooks 目录：
   ```sh
   mkdir -p ~/.grok/hooks
   ```

2. 创建一个简单的 hook 文件，例如 `~/.grok/hooks/session-start.json`：
   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "hooks": [
            { "type": "command", "command": "echo \"🚀 Grok session started in $(pwd)\"" }
           ]
         }
       ]
     }
   }
   ```

3. 启动（或重启）Grok 会话。Hook 会在 `SessionStart` 时自动运行。

   试用：在非 VS Code 系环境按 `Ctrl+L`（或在任意位置运行 `/hooks`——在 VS Code / Cursor / Windsurf / Zed 上更推荐），并查看 Hooks 标签页确认已加载。

## Hook 位置

Hooks 会从多个位置发现（全部合并）：

| 作用域     | 路径                              | 是否受信任？     | 说明 |
|-----------|-----------------------------------|--------------|-------|
| 全局    | `~/.grok/hooks/*.json`            | 始终信任       | 最适合个人 hooks |
| 全局    | `~/.claude/settings.json`         | 始终信任       | Claude Code 兼容 |
| 项目   | `<project>/.grok/hooks/*.json`    | 需要信任 | 按仓库自动化 |
| 项目   | `<project>/.claude/settings.json` | 需要信任 | Claude 兼容 |
| 插件    | 打包在已安装插件内  | 按插件   | 团队共享 hooks |

**信任项目**：首次打开带 hooks 的项目时，打开 hooks 模态框（非 VS Code 系按 `Ctrl+L`，或在包括 VS Code 系在内的任意终端运行 `/hooks`），或运行 `/hooks-trust`（与 `--trust` 相同的文件夹信任门控，记录在 `~/.grok/trusted_folders.toml`）。这可防止不受信任的仓库运行任意代码。

## Hook JSON 格式

每个 `.json` 文件可以定义多个 hooks：

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

关键字段：

- **事件名**（顶层键）：`SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`Stop`、`Notification`、`SessionEnd` 等。
- **matcher**（可选）：对事件的匹配值做正则测试——工具事件上是工具名，其他事件见用户指南的 Hooks 章节。为空 = 匹配全部。
- **type**：`"command"`（运行脚本或 shell 单行命令）或 `"http"`（将事件 POST 到 URL）。
- **command**：可执行文件路径（相对该 JSON 文件）或内联 shell 命令。
- **timeout**：超时前杀死 hook 的秒数（默认：5；`Stop`/`SubagentStop` 门控为 600）。超时时 hooks 采用失败放行（fail open）。

**工具名别名**：Claude 风格名称如 `Bash`、`Edit`、`Read` 会自动匹配 Grok 内部名称（`run_terminal_cmd`、`search_replace`、`read_file`）。

## 编写 Hook 脚本

### 输入
完整事件以 JSON 形式通过 **stdin** 传入。`PreToolUse` hook 示例：

```json
{
  "hookEventName": "pre_tool_use",
  "sessionId": "abc-123",
  "cwd": "/Users/you/project",
  "workspaceRoot": "/Users/you/project",
  "toolName": "run_terminal_cmd",
  "toolInput": { "command": "npm test" },
  "timestamp": "2026-04-14T12:00:00Z"
}
```

### 输出（用于 PreToolUse 等可拦截 hooks）
向 **stdout** 写入 JSON：

- 允许：`{"decision": "allow"}`
- 拒绝：`{"decision": "deny", "reason": "Unsafe command detected"}`

**退出码**（行为因 hook 类型而异）：
- `0` — 成功 / 允许（对可拦截 hooks）
- `2` — 显式拒绝（`PreToolUse`），或以 stderr 作为反馈阻止停止（`Stop`/`SubagentStop`；见用户指南中的 Stop Decision Control）
- 其他任意值（包括超时/崩溃/缺少环境变量）— **失败放行（fail-open）**：失败会被记录并显示在 hook 回滚日志中，但不会拦截工具调用。要拦截工具调用，请在 stdout 返回 JSON `{"decision":"deny","reason":"..."}`。

### 被动 hooks
对于 `SessionStart` 或 `PostToolUse` 等事件，stdout 会被忽略。成功时退出码为 0 即可。

### 有用的环境变量

Grok 会向每个 hook 进程注入以下变量：

- `GROK_HOOK_EVENT` — 事件名（例如 `pre_tool_use`、`session_start`、`post_tool_use`）
- `GROK_HOOK_NAME` — 此 hook 的完整配置名称
- `GROK_SESSION_ID` — 当前会话标识符
- `GROK_WORKSPACE_ROOT` — 工作区根目录的绝对路径

对于插件提供的 hooks，还会设置：

- `GROK_PLUGIN_ROOT` — 插件安装目录的绝对路径
- `GROK_PLUGIN_DATA` — 插件可写数据目录的绝对路径

这些由运行器与插件注入的变量始终优先。尝试通过 `env` 字段覆盖保留的运行器键会在加载时被剥离（并记录警告）。对于插件 hooks，`GROK_PLUGIN_ROOT` 与 `GROK_PLUGIN_DATA` 同样会覆盖用户为这些键提供的任何值。

### 自定义环境变量（`env` 字段）

每个 handler 可以声明额外的环境变量注入到子进程：

```json
{
  "type": "command",
  "command": "bin/check.sh",
  "env": {
    "MY_API_TOKEN": "secret-here",
    "LOG_LEVEL": "debug"
  }
}
```

值必须是 **字符串** — JSON 数字和布尔值目前无法解析
（如需要请用引号包裹）。

对于插件 hooks，插件适配器还会额外注入
`GROK_PLUGIN_ROOT` 与 `GROK_PLUGIN_DATA`。这些键会覆盖用户为相同名称声明的
任何值（插件契约不可协商）。

### 变量替换

`command` 与 `url` 字符串在配置加载时支持 `$VAR` 与 `${VAR}` 替换：

```json
{
  "type": "command",
  "command": "${HOME}/.config/grok-hooks/check.sh"
}
```

每个引用的查找顺序：
1. handler 自身的 `env` 映射。
2. 当前进程环境（Grok 自身所见的环境）。

若两者都未设置，引用会 **原样保留**（例如 `${UNSET}`
保持为字面字符串）。运行时的 `sh -c` 分支可能在变量稍后被设置时再解析；否则运行器会拒绝启动并给出明确的
「required env var(s) not set」错误。

特别地，对于 HTTP hooks，`url` 还会在 **请求时** 再次展开
（紧接在 SSRF 校验之前），因此像
`${GROK_PLUGIN_ROOT}/check` 这类插件注入变量会按插件的实际路径解析。

#### 参数展开修饰符

POSIX 参数展开形式 — `${VAR:-default}`、`${VAR-default}`、
`${VAR:=x}`、`${VAR:?msg}`、`${VAR:+x}`、`${VAR%pat}`、`${VAR#pat}`、
`${VAR/pat/repl}`、`${VAR:N:M}` — **绝不会** 在加载时展开，而是
原样留给运行时的 `sh -c` 分支处理。这可避免加载时展开器与 POSIX shell 语义之间
出现细微分歧（尤其是 `:-` 对空字符串的行为）。

若 hook 命令包含 shell 元字符（空格、管道、`&&`、
重定向、`$` 等），运行器会通过 `sh -c` 执行，你将获得完整的
shell 展开语义。若命令是无元字符的裸路径，
运行器会直接启动它——但路径中的 `$VAR` / `${VAR}` 引用
仍会在加载时解析，因此像
`${HOME}/bin/check.sh` 这样的直接执行路径无需包在 `sh -c` 中也能工作。

#### 哪些不会被展开

- **`matcher`** 是正则（`$` 是行尾锚点）。它
  绝不会做环境变量展开——替换 `$VAR` 会静默改变正则的
  语义，并很可能产生无效模式。若需要动态
  matcher，请在写入时生成 JSON 文件。
- **`timeout`** 是数值，无需展开。
- **`env` 映射自身的值** — 这些会原样存储并
  传给子进程，因此 `"BAR": "${HOME}/x"` 会把字面
  字符串 `${HOME}/x` 注入子进程环境。

## 在 TUI 中管理 Hooks

在非 VS Code 系按 `Ctrl+L`（或在任意位置运行 `/hooks`）打开 Hooks & Plugins 模态框。

在 **Hooks** 标签页中你可以：
- `l` — 重新加载全部 hooks
- `a` — 按路径添加自定义 hook（便于测试）
- `e` — 启用/禁用
- `r` — 移除
- `Space` — 展开分组

来自 `~/.grok/hooks/` 的 hooks 显示在 **Global** 下，项目 hooks 在 **Project** 下，等等。

## HTTP Hooks

不必使用本地脚本，也可以调用远程端点：

```json
{ "type": "http", "url": "https://hooks.example.com/grok-event", "timeout": 15 }
```

完整事件信封会以 JSON POST 发送。适用于 webhooks、分析或 serverless 函数。

## 最佳实践

1. **保持 hooks 快速** — 长时间运行的 hooks 会阻塞 UI（尽可能使用后台 `&` 或异步）。
2. **用显式 `deny` 拦截** — hooks 在任何错误（超时、崩溃、缺少环境变量等）时都失败放行，因此崩溃的 hook 不会拦截工具调用。要强制执行策略，hook 必须运行完成并在 stdout 输出 `{"decision":"deny","reason":"..."}`。
3. **使用绝对路径或相对 hook 文件的路径** — 与 JSON 同目录的 `bin/` 中的脚本便于移植。
4. **用 `Ctrl+L`（非 VS Code 系）/ `/hooks` 测试** — 在依赖之前先验证加载与匹配。
5. **将项目 hooks 纳入版本控制** — 提交 `.grok/hooks/`（但切勿提交密钥）。

## 安全说明

- 全局 hooks（`~/.grok/...`）以你的用户权限运行——请像对待 shell 脚本一样对待它们。
- 项目 hooks 需要显式信任（运行 `/hooks-trust` 或使用模态框），以防止恶意仓库的供应链攻击。
- HTTP hooks 会发送会话数据——仅使用受信任的端点。

## 故障排除

- **Hook 未运行？** → 在非 VS Code 系按 `Ctrl+L`（或在任意位置运行 `/hooks`），查看是否已加载并匹配。
- **项目 hooks 被忽略？** → 请先信任该项目。
- **找不到脚本？** → 检查路径是否相对 `.json` 文件且可执行（`chmod +x`）。
- **看到错误？** → 查看 pager 日志（通常在 tracing 面板或 `~/.grok/logs`）。

## 更多示例

参见 `xai-grok-hooks` crate 中的内置示例：

- [安全 Shell 防护](../../../xai-grok-hooks/examples/hooks/safe-shell.json)
- [禁止递归 Grep](../../../xai-grok-hooks/examples/hooks/no-recursive-grep.json) — 硬性拦截 `grep -r`/`grep -R`/`rgrep`（OOM 防护）
- [会话审计日志](../../../xai-grok-hooks/examples/hooks/session-log.json)
- [工具活动记录器](../../../xai-grok-hooks/examples/hooks/tool-logger.json)

将它们复制到 `~/.grok/hooks/` 并按需定制。

## 完整参考

完整事件列表、matcher 语义、信任模型与高级细节，请参阅 [Hooks 用户指南](user-guide/10-hooks.md)。

---

*玩得开心！* 若你做出了很酷的东西，可以考虑以插件形式分享。
