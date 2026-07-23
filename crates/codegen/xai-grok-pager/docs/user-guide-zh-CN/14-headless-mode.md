# 无界面模式与脚本化

无界面模式（headless mode）从命令行以非交互方式运行 Grok。它接受单条提示词，在完整工具权限下执行，并返回结果。可用于自动化任务、编排工作流、构建集成，以及以编程方式解析输出。

---

## 基本用法

以非交互方式传入提示词会触发无界面模式。最常见的方式是 `-p` 标志（`--single` 的简写）；`--prompt-json` 与 `--prompt-file` 也会触发该模式：

```bash
grok -p "Your prompt here"
```

Grok 会处理提示词、运行所需工具，并将结果打印到 stdout。响应完成后进程退出。

---

## 命令行选项

| Flag                    | Description                                           |
| ----------------------- | ----------------------------------------------------- |
| `-p, --single <PROMPT>` | 要发送的提示词（也可使用 `--prompt-json` / `--prompt-file`） |
| `-m, --model <MODEL>`   | 使用的模型（例如 `grok-build`）              |
| `-s, --session-id <ID>` | 使用该 **UUID** 创建**新**会话（若 UUID 无效，或在目标会话目录下已被占用，则报错；不会恢复会话——请使用 `-r`/`-c`） |
| `--fork-session`        | 与 `-r`/`-c` 联用时，分叉到新的会话 ID，而不是追加到原会话 |
| `-r, --resume <ID>`     | 恢复已有会话（未找到时会报错）      |
| `-c, --continue`        | 继续当前目录下最近一次会话  |
| `--cwd <PATH>`          | 设置工作目录                                 |
| `--output-format <FMT>` | 输出格式：`plain`、`json`、`streaming-json`      |
| `--yolo`                | 自动批准所有工具执行                      |
| `--rules <TEXT>`        | 系统提示词的自定义规则                    |
| `--tools <TOOLS>`       | 内置工具白名单（逗号分隔）。MCP 元工具除非被拒绝，否则仍可用。仅无界面模式。 |
| `--disallowed-tools <TOOLS>` | 要从默认集合中移除的内置工具黑名单（逗号分隔）。支持 `Agent` 条目。仅无界面模式。 |
| `--max-turns <N>`       | 停止前允许的最大智能体轮次。仅无界面模式。 |
| `--reasoning-effort` / `--effort <LEVEL>` | 推理模型的推理强度。规范级别：`none`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max`（各级彼此独立；模型只接受其菜单中声明的级别）。也接受各模型菜单选项 id（例如 `deep` → 映射后的 wire 值），与 `/effort` 相同。在 TUI 与无界面模式中均可用。 |
| `--permission-mode <MODE>` | 权限模式。`bypassPermissions` 通过此标志启用始终批准（见 [22-permissions-and-safety.md](22-permissions-and-safety.md)）；若要默认拒绝，请在 `.claude/settings.json` 中使用 `defaultMode`。 |
| `--allow <RULE>`        | 带 glob 模式的权限允许规则（可重复）。在 TUI 与无界面模式中均可用。 |
| `--deny <RULE>`         | 带 glob 模式的权限拒绝规则（可重复）。在 TUI 与无界面模式中均可用。 |
| `--prompt-json <JSON>`  | 以 JSON content blocks 形式提供提示词                         |
| `--prompt-file <PATH>`  | 从文件读取提示词                                    |
| `--verbatim`            | 按原样发送提示词                          |
| `--no-auto-update`      | 禁用本次会话的更新检查                |
| `--sandbox <PROFILE>`   | 文件系统/网络访问的沙箱配置文件         |

> **注意：** `--tools`、`--disallowed-tools`、`--max-turns` 与 `--agents` 是仅无界面模式的标志。若在交互式 TUI 中使用，会打印警告并忽略该标志。`--reasoning-effort`/`--effort`、`--permission-mode`、`--allow` 与 `--deny` 在两种模式下均可用。更多标志（agents 与 worktrees）见 [更多无界面标志](#additional-headless-flags)。

### 工具过滤

使用 `--tools` 将智能体限制为明确的工具集合（白名单），或使用 `--disallowed-tools` 从默认集合中移除特定工具（黑名单）。两者均接受逗号分隔的工具名。

工具名是内部工具 ID（例如 shell 工具是 `run_terminal_cmd`，不是 `bash`）。

```bash
# Only allow read-only tools
grok -p "Explain this codebase" --tools "read_file,grep,list_dir"

# Remove web access and file editing
grok -p "Review this code" --disallowed-tools "web_search,web_fetch,search_replace"

# Remove shell access
grok -p "Review this code" --disallowed-tools "run_terminal_cmd"
```

`--disallowed-tools` 还支持特殊的 `Agent` 条目，用于控制子智能体（subagent）的派生：

| Entry                  | Effect                                  |
| ---------------------- | --------------------------------------- |
| `Agent`                | 阻止所有子智能体派生             |
| `Agent(explore)`       | 仅阻止 `explore` 类型的子智能体  |
| `Agent(explore, plan)` | 阻止多个指定类型           |

```bash
# Prevent the agent from spawning any subagents
grok -p "Fix this bug" --disallowed-tools "Agent"

# Block only the explore subagent
grok -p "Refactor this module" --disallowed-tools "Agent(explore)"
```

`--tools` 会保留所选智能体配置文件的注入策略：stock 配置文件在应用白名单之前会注入已启用的可选工具，而 curated 配置文件则保持严格。最终工具集保留所请求的工具，以及始终开启的 MCP 元工具。当两个标志同时存在时，以 `--disallowed-tools` 为准。

### 权限规则（`--allow` / `--deny`）

权限规则控制特定工具调用是自动批准、拒绝，还是需要用户确认。与会完全移除工具的 `--disallowed-tools` 不同，权限规则保留工具可用性，但对其执行进行门控。

规则使用 `ToolPrefix(glob_pattern)` 语法：

| Prefix        | What it controls                   |
| ------------- | ---------------------------------- |
| `Bash(...)`   | Shell 命令执行            |
| `Edit(...)`   | 文件编辑（路径 glob）           |
| `Write(...)`  | 文件写入（路径 glob）           |
| `Read(...)`   | 文件读取（路径 glob）           |
| `Grep(...)`   | 搜索操作（路径 glob）      |
| `WebFetch(...)` | URL 抓取（glob 或 `domain:host`） |
| `MCPTool(...)` | MCP 工具调用              |

对于路径规则（`Read`、`Edit`、`Write`、`Grep`），`*` 是单层通配符，`**` 是递归匹配。对于 `Bash` 规则，`*` 匹配任意字符（含空格）。不带括号的裸前缀匹配该类型的所有调用，而 `Bash(cmd:*)` 等价于对 `cmd` 做前缀匹配。完整匹配语义见 [22-permissions-and-safety.md](22-permissions-and-safety.md#rule-matching-reference)。

```bash
# Deny shell commands matching "rm*"
grok -p "Clean up this project" --deny "Bash(rm*)"

# Allow npm commands, deny sudo
grok -p "Set up the project" --allow "Bash(npm*)" --deny "Bash(sudo*)"

# Allow all bash commands (auto-approve without prompting)
grok -p "Build the project" --allow "Bash"
```

`--allow` 与 `--deny` 可重复使用。拒绝规则优先于允许规则。

---

## 输出格式

无界面模式支持三种输出格式，通过 `--output-format` 选择。

### plain（默认）

人类可读文本，适合直接显示或管道传递：

```
Here's a summary of the codebase...
```

### json

响应完成后输出的单个 JSON 对象：响应文本、停止原因、会话 ID、请求 ID（存在推理时还包含 `thought`）。当提示词已到达模型时，同一对象还会携带消耗相关字段（`usage`、`num_turns`、`modelUsage`、成本）。

```json
{
  "text": "Here's a summary of the codebase...",
  "stopReason": "EndTurn",
  "sessionId": "abc123",
  "requestId": "xyz789",
  "num_turns": 7,
  "usage": {
    "input_tokens": 7210,
    "cache_read_input_tokens": 41000,
    "output_tokens": 1893,
    "reasoning_tokens": 412,
    "total_tokens": 50103
  },
  "modelUsage": {
    "grok-build": {
      "inputTokens": 7210,
      "outputTokens": 1893,
      "cacheReadInputTokens": 41000,
      "modelCalls": 7,
      "costUSD": 0.01268905
    }
  },
  "total_cost_usd": 0.01268905,
  "total_cost_usd_ticks": 126890500
}
```

用法说明：

- `usage` 汇总该提示词的 token，包括在轮次结束前已完成的子智能体（也出现在各自的 `modelUsage` 键下）。压缩（compaction）及其他旁路模型调用不计入。
- **Token 字段策略（无界面结果 / `end` / 错误消耗）：**
  - `usage.input_tokens` 与 `modelUsage.*.inputTokens` 仅为**未缓存**部分。
  - `cache_read_input_tokens` / `cacheReadInputTokens` 为缓存命中。
  - `total_tokens` 为完整输入 + 输出（含缓存）：
    `total_tokens = input_tokens + cache_read_input_tokens + output_tokens`。
  - ACP `_meta.usage.inputTokens`（PromptUsage）仍是提示词的**完整**合计；只有无界面投影器会减去缓存。做消耗自动化时优先使用无界面字段。
- `num_turns` 统计提示词账本上记录的主智能体模型轮次（已报告 usage 的工具循环轮次）。子智能体的采样器调用不增加该值。各模型调用次数（含子智能体）保留在 `modelUsage.*.modelCalls`。它与 `--max-turns` 属于同一计数体系，但在某些轮次缺少 usage 或触发门控时，不保证完全相等。
- `total_cost_usd` 仅在服务器报告了**完整**成本时出现。缺失表示未上报或不完整，绝不表示免费。目前成本会为 API key 流量打标；pool/OAuth 路径在服务器打标成本前通常会省略。当部分调用缺少成本时，`cost_is_partial` 为 true，并省略**所有**成本浮点数（`total_cost_usd` 以及每个 `modelUsage.*.costUSD`），以免消费者把模型行相加得到虚假完整账单。
- `total_cost_usd_ticks` 是同一数值的精确整数 tick（1 USD = 10^10 ticks），出现条件相同。用于账单对账：对各次调用的 tick 求和可与服务器 usage 导出完全一致，浮点美元无法保证这一点。
- 当子智能体 usage 无法应用、嵌套子智能体 usage 不完整，或成功路径的 drain 超时（轮次任务最长 120 秒）时，`usage_is_incomplete` 为 true，并以同样方式省略成本浮点数（token 合计可能少计子智能体）。取消快照会跳过该长时间 drain，并在子智能体仍存活时标记为不完整。不完整且无已记录 token 时，仅输出 `usage_is_incomplete`（不会出现全零的 `usage` 对象）。
- 从未到达模型的提示词会省略消耗相关字段。

`sessionId` 字段便于之后恢复对话。

失败时，Grok 会输出错误对象（进程以非零退出码退出）。提示词级失败在已记录 usage 时也可能包含冻结的消耗字段：

```json
{"type":"error","message":"Couldn't start session: ..."}
```

### streaming-json

实时发出的换行分隔 JSON 事件。每行是带有 `type` 字段的独立 JSON 对象：

```json
{"type":"text","data":"Here's"}
{"type":"text","data":" a summary"}
{"type":"thought","data":"Analyzing the directory structure..."}
{"type":"end","stopReason":"EndTurn","sessionId":"abc123","requestId":"xyz789","usage":{...},"num_turns":7,"modelUsage":{...}}
```

事件类型：

| Type       | Description                                                    |
| ---------- | -------------------------------------------------------------- |
| `text`     | 智能体响应文本的一个片段                            |
| `thought`  | 内部推理（thinking tokens）                            |
| `end`      | 最终事件，在可用时包含元数据与消耗字段       |
| `error`    | 发生错误（携带 `message`，以及若有则包含消耗字段）  |

`end` 始终是最后一个事件。`end` 上的消耗字段与 json 对象形状一致（snake_case 的未缓存 `input_tokens`、安全的成本浮点数）。

Grok 还可能发出 `max_turns_reached` 与 `auto_compact_*` 事件；请将该列表视为非穷尽，并按 `type` 分支处理。

---

## 无界面模式下的会话管理

默认情况下，每次 `grok -p` 调用都会创建新会话。要在多次调用间保持上下文，请使用会话相关标志。

### 命名会话（`-s`）

要在多次无界面调用间携带上下文，请使用 `-r/--resume` 或 `-c/--continue`。仅在创建带 **UUID** 的**新**会话时使用 `-s/--session-id`（若不是 UUID，或在目标目录下已被占用，则报错）。旧的隐藏 `-s` upsert/恢复行为已移除——请用 `-r`/`-c` 继续。与 `-r`/`-c` 联用时，`-s` 需要 `--fork-session`：

```bash
# Start a headless session and capture its ID
grok -p "Review the changes in this PR" --output-format json | jq -r '.sessionId'

# Continue in the same session
grok -p "Now check for security issues" --resume "<id>"

# Optional: create with a client-chosen UUID (must not already exist)
grok -p "hello" --session-id "$(uuidgen | tr '[:upper:]' '[:lower:]')" --output-format json
```

> **注意：** `-s/--session-id` 仅用于创建新会话（有效 UUID；若已占用则报错）。恢复会话请使用 `-r`。

### 恢复（`-r`）

`-r/--resume` 标志按 ID 恢复指定会话。会话不存在时会报错：

```bash
# Get the session ID from a previous JSON response
grok -p "Remember: the secret number is 42" --output-format json
# Output includes "sessionId": "abc123"

# Resume that exact session
grok -p "What's the secret number?" --resume abc123
```

### 继续（`-c`）

`-c/--continue` 标志继续当前工作目录下最近一次会话：

```bash
grok -p "Continue where we left off" -c
```

### 提取会话 ID

使用 `--output-format json` 并解析 `sessionId` 字段：

```bash
grok -p "Hello" --output-format json | jq -r '.sessionId'
```

---

## 管道输入与输出

无界面模式可自然配合 Unix 管道与重定向。

### 标准输出

```bash
# Pipe output to a file
grok -p "Generate a README" > README.md

# Parse JSON output with jq
grok -p "List files" --output-format json | jq -r '.text'
```

### 标准输入

无界面模式不会把管道传入的 stdin 读入提示词。请通过命令替换或 `--prompt-file` 传入外部内容：

```bash
# Include git diff as context via command substitution
grok -p "Write a concise commit message for these changes:

$(git diff --staged)"

# Or read the prompt from a file
grok --prompt-file ./prompt.txt
```

---

## CI/CD 集成示例

### 自动化代码审查

```bash
grok -p "Review changes for bugs and security issues." \
  --output-format json --yolo | jq -r '.text' > review.md
```

### Pre-Commit 钩子

```bash
grok -p "Review staged changes for obvious bugs. Reply OK if fine, or list issues." \
  --yolo --output-format json | jq -r '.text' | grep -q "^OK" || exit 1
```

### 批处理

```bash
for file in src/*.js; do
  grok -p "Migrate $file from CommonJS to ES modules." --yolo
done
```

---

## 脚本化模式

### Python 封装

Grok 的无界面模式可以封装为兼容 OpenAI 的 chat completion API：

```python
import asyncio
import json
import os

class GrokChat:
    """Simple OpenAI-compatible wrapper using headless mode."""

    def __init__(self, cwd="."):
        self.cwd = cwd
        self.env = {**os.environ}

    def _build_cmd(self, prompt, model, stream):
        return ["grok", "-p", prompt, "-m", model, "--cwd", self.cwd,
                "--output-format", "streaming-json" if stream else "json",
                "--yolo"]

    async def create(self, messages, model="grok-build", stream=False):
        prompt = messages[-1]["content"] if len(messages) == 1 else "\n".join(
            f"{m['role']}: {m['content']}" for m in messages
        )
        cmd = self._build_cmd(prompt, model, stream)

        if stream:
            return self._stream(cmd)

        proc = await asyncio.create_subprocess_exec(
            *cmd, env=self.env, stdout=asyncio.subprocess.PIPE
        )
        stdout, _ = await proc.communicate()
        data = json.loads(stdout.decode()) if stdout else {"text": ""}
        return {
            "choices": [{
                "message": {"role": "assistant", "content": data.get("text", "")},
                "finish_reason": "stop"
            }]
        }

    async def _stream(self, cmd):
        proc = await asyncio.create_subprocess_exec(
            *cmd, env=self.env, stdout=asyncio.subprocess.PIPE
        )
        async for line in proc.stdout:
            if not line.strip():
                continue
            event = json.loads(line)
            if event.get("type") == "text":
                yield {"choices": [{"delta": {"content": event["data"]}}]}
            elif event.get("type") == "end":
                yield {"choices": [{"delta": {}, "finish_reason": "stop"}]}


async def main():
    client = GrokChat(cwd=".")
    response = await client.create(
        [{"role": "user", "content": "What files are here?"}]
    )
    print(response["choices"][0]["message"]["content"])

asyncio.run(main())
```

### Shell 脚本

```bash
#!/bin/bash
# Run a code review and exit with failure if issues are found

RESULT=$(grok -p "Review this PR for bugs. Output JSON with 'issues' array." \
  --output-format json --yolo | jq -r '.text')

ISSUE_COUNT=$(echo "$RESULT" | jq '.issues | length' 2>/dev/null || echo "0")

if [ "$ISSUE_COUNT" -gt 0 ]; then
  echo "Found $ISSUE_COUNT issues"
  echo "$RESULT" | jq '.issues[]'
  exit 1
fi

echo "No issues found"
```

---

## 使用 --yolo 进行全自动运行

`--yolo` 标志启用始终批准模式（与 `--permission-mode bypassPermissions` 和 `--always-approve` 相同），会自动批准工具执行（文件写入、命令执行等），无需确认提示。显式的 `deny` 规则与 `PreToolUse` 钩子仍然生效，管理员也可通过 `requirements.toml` 禁用该模式（见 [22-permissions-and-safety.md](22-permissions-and-safety.md)）。无人值守自动化需要此模式：

```bash
# Format all files without asking
grok -p "Format all files" --yolo

# Run tests and fix failures
grok -p "Run the tests and fix any failures" --cwd ~/projects/my-app --yolo
```

**请谨慎使用 `--yolo`。** 它赋予智能体修改文件与运行命令的完全自主权。仅在可信环境中使用，或配合范围明确的提示词。

---

## 无界面模式相关环境变量

影响无界面模式的关键环境变量：

| Variable                        | Description                                                   |
| ------------------------------- | ------------------------------------------------------------- |
| `XAI_API_KEY`        | 用于身份验证的 API 密钥（无浏览器登录时必需）   |
| `GROK_HOME`                    | 覆盖配置目录（默认：`~/.grok`）                |
| `GROK_LOG_FILE`                | 日志文件路径（按原样用作路径；在无界面与 TUI 中均可用，遵循 `RUST_LOG`） |
| `RUST_LOG`                     | 日志级别过滤器（例如 `debug`）。无界面模式将日志写到 stderr。     |

在无浏览器访问的 CI 环境中，请使用来自 [console.x.ai](https://console.x.ai) 的 API 密钥设置 `XAI_API_KEY`：

```bash
export XAI_API_KEY="xai-..."
grok -p "Run the test suite" --yolo
```

---

## 退出码

| Code | Meaning                              |
| ---- | ------------------------------------ |
| `0`  | 成功——提示词正常完成 |
| `1`  | 错误——身份验证失败、网络错误或运行时错误 |
| `130` | 被 SIGINT 中断（Ctrl+C）                                   |
| `143` | 被 SIGTERM 终止                                            |

---

## 无界面环境的身份验证

无界面使用时，可通过以下方式之一完成身份验证：

- **`XAI_API_KEY`** — 对 CI 最简单。见上文的 [环境变量](#environment-variables-for-headless)。
- **`grok login --device-auth`**（或 `--device-code`）— 目标机器无需浏览器。
  见 [Authentication > Device Code Flow](02-authentication.md#device-code-flow)。
- **`grok login`** — 在有 GUI 的机器上进行基于浏览器的 OAuth2。

若此前已登录，会自动使用缓存的凭据。

---

## 提示

- 无界面模式默认启动**全新会话**。要在多次调用间保持上下文，请使用 `-r/--resume` 或 `-c/--continue`。
- `--output-format json` 的响应始终包含 `sessionId`，可用于 `--resume` 进行后续调用。
- 将 `--yolo` 与 `--rules` 结合以设置护栏：`grok -p "..." --yolo --rules "Never delete files"`。
- 调试时可提高日志级别并捕获 stderr：`RUST_LOG=debug grok -p "..." 2> debug.log`。

---

## 项目根目录发现

Grok 启动时，会从 `--cwd`（或当前目录）向上遍历，直到找到 `.git` 目录，以此发现项目根目录。

注意：若 `--cwd` 嵌套在大型仓库（例如 monorepo）内部，Grok 会将该仓库识别为项目根，并将其发现范围（AGENTS.md、skills、git 历史）限定在该仓库，这可能使启动变慢。将 `--cwd` 指向你要处理的具体子项目，以缩小范围。

---

## 文件位置

Grok 将数据存储在 `~/.grok`（可用 `GROK_HOME` 覆盖；见 [无界面模式相关环境变量](#environment-variables-for-headless)）：

| Path                     | Contents                              |
| ------------------------ | ------------------------------------- |
| `config.toml`            | 用户配置                    |
| `auth.json`              | 缓存的 OAuth2/API 凭据         |
| `version.json`           | 用于更新检查的版本缓存       |
| `sessions/`              | 会话记录（SQLite）          |
| `memory/`                | 跨会话记忆存储            |
| `logs/`                  | 内部日志文件（例如 `unified.jsonl`） |
| `logs/mcp/`              | MCP 服务器日志                       |
| `skills/`                | 用户技能定义                |
| `personas/`              | 用户作用域的智能体 persona            |
| `crash/`                 | 崩溃报告                         |
| `trace-exports/`         | 会话 trace 导出                 |
| `worktrees/`             | Git worktree 元数据                 |

### 只读 `~/.grok`

在容器或 CI 中，可将 `~/.grok` 以只读方式挂载：

- 预先填充 `auth.json`，或使用 `XAI_API_KEY`
- 会话持久化会静默失败（临时）
- 更新检查会记录警告并跳过

```bash
export XAI_API_KEY="xai-..."
export GROK_DISABLE_AUTOUPDATER=1
grok -p "..." --no-auto-update
```

---

## 抑制更新检查

| Method                          | Scope     |
| ------------------------------- | --------- |
| `--no-auto-update`              | 会话级   |
| `GROK_DISABLE_AUTOUPDATER=1`    | 进程级   |
| 非 TTY 的 stderr（自动检测）  | 自动 |
| `[cli] auto_update = false`     | 持久|

将 `GROK_DISABLE_AUTOUPDATER` 设为假值（`0`、`false`、`off`、`no` 或空字符串，大小写不敏感）视为未设置。智能体 SDK 会为其派生的非 leader 智能体注入 `GROK_DISABLE_AUTOUPDATER=1`（SDK 隔离环境中的假值会保持更新开启），而 stdio 智能体除非运行自托管安装（`$GROK_HOME/bin/grok`），否则会跳过其后台更新。

更新消息输出到 **stderr**。Stdout 在 `--output-format json` 下保持干净。另见 [无界面模式相关环境变量](#environment-variables-for-headless)。

---

## 更多无界面标志

这些标志补充上文 [命令行选项](#command-line-options) 表。表中已列出的标志（`--prompt-json`、`--prompt-file`、`--verbatim`、`--sandbox`、`--no-auto-update`）此处不再重复。

| Flag                          | Description                                       |
| ----------------------------- | ------------------------------------------------- |
| `--agent <NAME>`              | 智能体名称或定义文件路径                |
| `--agents <JSON>`             | 以内联 JSON 定义子智能体               |
| `--system-prompt-override`    | 覆盖智能体的系统提示词                |
| `--no-plan`                   | 禁用 plan 模式                                 |
| `--no-subagents`              | 禁用子智能体派生                         |
| `--no-memory`                 | 禁用跨会话记忆                      |
| `--disable-web-search`        | 禁用网页搜索与抓取工具                |
| `--no-alt-screen`             | 以内联方式运行（不使用备用屏幕）                  |
| `--worktree [NAME]`           | 在新的 git worktree 中启动会话               |
| `--ref <REF>` / `--worktree-ref <REF>` | 作为 worktree 基线的分支/标签/提交（与 `--worktree` 联用） |

---

## 被中断的无界面运行

收到 SIGINT/SIGTERM 时：

- 会话状态会保存到最后一次已完成的工具调用
- 工具对文件的修改**不会回滚**
- SIGINT 的退出码为 **130**（`128 + 2`），SIGTERM 为 **143**（`128 + 15`）；CI 流水线可据此与普通错误（退出码 `1`）区分
- 恢复：`grok -p "continue" --resume "<id>"` 或 `grok -p "continue" --continue`

关于命名会话以及 `-s`/`-r`/`-c` 标志的详情，见 [无界面模式下的会话管理](#session-management-in-headless-mode)。
