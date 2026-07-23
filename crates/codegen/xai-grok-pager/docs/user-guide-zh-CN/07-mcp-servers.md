# MCP 服务器

MCP（Model Context Protocol，模型上下文协议）服务器通过外部工具集成扩展 Grok。它们让 Grok 能够与任何实现 MCP 标准的服务交互。

---

## 什么是 MCP 服务器？

MCP 服务器是一个通过标准化协议向 Grok 暴露工具的进程。配置 MCP 服务器后，其工具会与 Grok 的内置工具一起提供给模型使用。模型可在会话中发现并调用这些工具。

例如，GitHub MCP 服务器可能暴露 `create_issue`、`list_pull_requests` 和 `search_code` 等工具。数据库服务器可能暴露 `query`、`list_tables` 和 `describe_schema`。

协议细节见 [MCP 规范](https://modelcontextprotocol.io)。

---

## 配置

MCP 服务器在 `~/.grok/config.toml` 中通过 `[mcp_servers.<name>]` 节进行配置。

### stdio 传输（本地进程）

Grok 会启动本地进程，并通过 stdin/stdout 通信：

```toml
[mcp_servers.my-server]
command = "/path/to/server"           # Server executable
args = ["--flag", "value"]            # Command arguments
env = { API_KEY = "sk-..." }          # Environment variables
enabled = true                        # Enable or disable the server (default: true)
startup_timeout_sec = 30              # Server startup timeout, seconds (default: 30)
tool_timeout_sec = 6000               # Per-tool-call timeout fallback, seconds (default: 6000)
tool_timeouts = { slow_op = 120 }     # Per-tool timeout overrides, seconds
```

> **全局启动超时覆盖：** 除了为每个服务器单独设置 `startup_timeout_sec`，
> 你也可以通过环境变量 `MCP_TIMEOUT`（毫秒，与 Claude Code 兼容）或
> `GROK_MCP_STARTUP_TIMEOUT_SECS`（秒）更改所有服务器的默认值。服务器级别的
> `startup_timeout_sec` 仍优先于二者。首次启动时需要下载包的冷启动
> `npx`/`uvx` 服务器通常需要调大该值；默认值为 30 秒。
>
> **MCP 工具结果大小上限：** 较大的 MCP / `use_tool` 结果会在行内截断
> （完整载荷会写入会话的 `mcp/` 目录）。默认值为
> **20_000 字节**。可通过以下方式覆盖：
>
> - 环境变量 `GROK_MAX_MCP_OUTPUT_BYTES` 或 `MAX_MCP_OUTPUT_BYTES`（单位为字节；两者都设置时
>   Grok 原生变量优先；名称兼容 Claude 风格，但我们按 **字节** 而非 token 限制）
> - `config.toml` — 用户级（`~/.grok/config.toml`）**或仓库级**
>   （从 cwd → git 根路径链上任意位置的 `.grok/config.toml`；最深的
>   文件优先，且仓库级配置仅在该目录被信任后生效）：
>
> ```toml
> [mcp]
> max_output_bytes = 40000
> ```
>
> 优先级：requirements.toml > 环境变量 > 仓库 `.grok/config.toml` >
> 用户/托管配置 > 默认值。对该目录中运行中的会话，仓库级修改会通过配置热重载生效。

### HTTP/SSE 传输（远程服务器）

对于可通过 HTTP 访问的远程 MCP 服务器：

```toml
[mcp_servers.remote-api]
url = "https://mcp.example.com/api"
headers = { "Authorization" = "Bearer token" }
```

### 带会话 ID 的可流式 HTTP

```toml
[mcp_servers.my-streamable-server]
url = "https://mcp.example.com/api/mcp"
headers = { "x-mcp-session-id" = "{{session_id}}" }
```

---

## CLI 管理

无需编辑配置文件，即可从命令行管理 MCP 服务器：

```bash
# List configured MCP servers
grok mcp list
grok mcp list --json          # Machine-readable output

# Add a stdio server. Everything after -- is the server command, so flags
# like -y reach the server instead of being parsed by grok.
grok mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /path/to/dir

# Add a stdio server with environment variables (-e is repeatable)
grok mcp add postgres -e DATABASE_URL=postgres://localhost/mydb -- npx -y @modelcontextprotocol/server-postgres

# Add a remote HTTP server
grok mcp add --transport http sentry https://mcp.sentry.dev/mcp

# Add a remote server with an authentication header (--header is repeatable)
grok mcp add --transport http api https://mcp.example.com/mcp --header "Authorization: Bearer YOUR_TOKEN"

# Add a remote SSE server
grok mcp add --transport sse linear https://mcp.linear.app/sse

# Remove a server
grok mcp remove github

# Diagnose a server's configuration and connectivity
grok mcp doctor               # Check every configured server
grok mcp doctor github        # Check one server
grok mcp doctor --json        # Machine-readable output
```

传输方式默认为 `stdio`；远程服务器请传入 `--transport http` 或 `--transport sse`。

默认情况下，`grok mcp add` 会写入 `~/.grok/config.toml`（`--scope user`）。使用 `--scope project` 可改为写入当前目录下的 `.grok/config.toml`，便于提交并与团队共享（参见 [项目级 MCP 服务器](#project-scoped-mcp-servers)）。Header 与环境变量的值会原样存储，因此在会提交的项目配置中请用 `${VAR}` 引用密钥，而不是直接粘贴（参见 [示例配置](#example-configurations)）。`grok mcp list` 会显示两个作用域中的服务器，并将项目级服务器标记为 `(project)`。

`grok mcp remove` 会搜索两个作用域，成功移除服务器后以退出码 0 结束。名称未找到，或名称同时定义在用户级与项目级作用域时，以退出码 1 结束——此时需传入 `--scope` 指明要移除的是哪一个。

与早期版本的破坏性变更：`--env` 现在每个标志只接受一个 `KEY=value`（请使用 `-e A=1 -e B=2`，而不是 `--env A=1 B=2`），且服务器名称只能包含字母、数字、连字符和下划线。

---

## 项目级 MCP 服务器

通过在仓库中放置 `.grok/config.toml`，可为每个项目单独配置 MCP 服务器：

```
my-project/
  .grok/
    config.toml
  src/
  ...
```

```toml
# .grok/config.toml
[mcp_servers.linear]
url = "https://mcp.linear.app/mcp"
enabled = true
```

当服务器提供原生 HTTP/SSE 端点时，优先使用 `url` 形式，而不是用 `npx mcp-remote <url>` 这类 stdio 代理包装。Grok 可直接处理 HTTP/SSE 与 OAuth，因此原生形式可避免每个会话多起一个子进程，同时也会向提供方注册 Grok 自己的 OAuth 客户端。

Grok 会从当前目录向上遍历到 git 仓库根目录，并在每一层加载 `.grok/config.toml`：

| 位置 | 作用域 | 优先级 |
|----------|-------|----------|
| `~/.grok/config.toml` | 所有项目 | 最低 |
| `<repo-root>/.grok/config.toml` | 本仓库 | 中等 |
| `<cwd>/.grok/config.toml` | 当前目录 | 最高 |

若项目定义了与全局同名的服务器，将完整替换全局版本（字段不会合并）。

项目级文件会贡献 `[mcp_servers]`、`[plugins]` 和 `[permission]` 条目。Grok 对大多数其他配置节仅从 `~/.grok/config.toml` 读取。

---

## 工具命名

MCP 工具会用服务器名称做命名空间，以避免冲突：

- 服务器 `filesystem` 的工具 `read_file` 变为 `filesystem__read_file`
- 服务器 `github` 的工具 `create_issue` 变为 `github__create_issue`

---

## 在运行时切换服务器

你可以在会话中启用或禁用 MCP 服务器，无需重启 Grok。

### /mcps 模态框

在 TUI 中打开 MCP 服务器模态框：

- 以斜杠命令运行 `/mcps`
- 或按 `Ctrl+L`（非 VS Code 系列）并导航到 MCP Servers 选项卡；在 VS Code 系列中使用 `/plugins` 或 `/mcp` 并打开 MCP Servers 选项卡

在模态框中你可以：

- 查看每个服务器的来源、启用状态与工具数量
- 用 `Space` 启用或禁用服务器
- 展开服务器以查看其提供的工具
- 编辑 `config.toml` 后按 `r` 刷新列表
- 用 `i` 对 OAuth 服务器进行认证
- 用 `a` 添加服务器，或用 `x` 移除服务器

### 工具发现

模型可使用两个内置工具来处理 MCP 服务器：

- `search_tool` — 在所有已启用的 MCP 服务器中发现可用的集成工具。可用它按名称或描述查找工具。
- `use_tool` — 调用通过 `search_tool` 发现的集成工具。需指定完全限定的工具名（例如 `github__create_issue`）。

---

## 兼容性

为兼容性考虑，Grok 会从多个来源加载 MCP 服务器配置：

| 来源 | 格式 | 位置 | 是否可配置 |
|--------|--------|----------|-------------|
| `config.toml` | 原生 Grok 配置 | `~/.grok/config.toml`、`.grok/config.toml` | 始终启用 |
| `.claude.json` | Claude Code 格式 | `~/.claude.json` | `[compat.claude] mcps` |
| `.cursor/mcp.json` | Cursor 格式 | `~/.cursor/mcp.json`、`<project>/.cursor/mcp.json` | `[compat.cursor] mcps` |
| `.mcp.json` | MCP 标准格式 | 项目根目录（从 cwd 到 git 根） | 除非你已导入或关闭 Claude 导入提示（导入标记已设置），否则会加载 |

所有来源按优先级合并：config.toml > Claude > Cursor > `.mcp.json`。名称冲突时，优先级更高的来源中的服务器优先生效。

默认会扫描 Claude 与 Cursor 的 MCP 来源。若要禁用某个厂商的扫描，请在 `~/.grok/config.toml` 中设置 `[compat.<vendor>] mcps = false`，或使用对应的环境变量（`GROK_CURSOR_MCPS_ENABLED`、`GROK_CLAUDE_MCPS_ENABLED`）。详情见 [配置](05-configuration.md#harness-compatibility)。使用 `grok inspect` 可查看已加载的 MCP 服务器及其厂商来源（`[cursor]`、`[claude]`）。

---

## MCP OAuth

对于需要 OAuth 认证的 MCP 服务器，Grok 会自动处理凭证流程。当 MCP 服务器请求 OAuth 凭证时，Grok 会打开基于浏览器的授权流程，并将得到的 token 存储供后续使用。

---

## 示例配置

对托管的 MCP 服务器使用 `url` 形式，对本地 stdio 工具使用 `command` / `args` 形式。

### 原生 HTTP（托管服务）

基于 OAuth 的 MCP 服务器必须先完成认证才能使用。Grok 会将得到的 token 以本地明文形式存储在 `~/.grok/mcp_credentials.json` 中，并设置仅所有者可读写的文件权限（在 Unix 上为 `0600`）。建议在主机上启用全盘加密。编辑 `config.toml` 后，在 `/mcps` 模态框中按 `r` 刷新服务器列表。

```toml
[mcp_servers.linear]
url = "https://mcp.linear.app/mcp"
enabled = true

[mcp_servers.sentry]
url = "https://mcp.sentry.dev/mcp"
enabled = true

[mcp_servers.mixpanel]
url = "https://mcp.mixpanel.com/mcp"
enabled = true
```

对于使用静态 bearer token 而非 OAuth 认证的内部或自托管服务器，请显式设置 `Authorization` header：

```toml
[mcp_servers.internal-tools]
url = "https://mcp.internal.example.com/mcp"
enabled = true

[mcp_servers.internal-tools.headers]
Authorization = "Bearer <token>"
```

为避免在配置文件中写入密钥，可用 `${VAR}`（或 `${VAR:-default}`）引用环境变量。Grok 会在加载时展开 `[mcp_servers.*]` 中的字符串字段——`url`、`command`、`args`，以及 `env` 与 `headers` 中的值：

```toml
[mcp_servers.internal-tools]
url = "https://mcp.internal.example.com/mcp"
enabled = true
headers = { "Authorization" = "Bearer ${INTERNAL_MCP_TOKEN}" }
```

### 本地 stdio

对必须在本地运行的工具（文件系统访问、本地数据库、内部服务器）使用 stdio。

```toml
# Filesystem access scoped to a directory
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/directory"]

# Local Postgres
[mcp_servers.postgres]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-postgres", "postgresql://user:pass@localhost/db"]

# Custom server with a longer startup timeout and tuned per-tool timeouts
[mcp_servers.my-tools]
command = "/usr/local/bin/my-mcp-server"
args = ["--config", "/etc/my-mcp.json"]
startup_timeout_sec = 30
tool_timeout_sec = 120
tool_timeouts = { slow_analysis = 300, quick_lookup = 10 }
```

在 Windows 上，npm 会将 `npx`、`npm`、`pnpm`、`yarn` 等启动器安装为 `.cmd` 批处理垫片（没有 `npx.exe`）。Grok 在启动前会把裸 `command`（如 `npx`）解析为 `PATH` 上的真实启动器路径（并遵循 `PATHEXT`），因此无需手动用 `cmd /c` 包装即可工作。若 `command` 为绝对路径或包含路径分隔符，则按原样使用。

---

## 可用的 MCP 服务器

以下是部分可用 `url` 或 `command` 形式配置的 MCP 服务器列表（形式见上文）。使用前请向各提供方确认当前端点或包名：

| 服务器 | 传输方式 | 端点 / 包 |
|--------|-----------|--------------------|
| Linear | HTTP (OAuth) | `https://mcp.linear.app/mcp` |
| Sentry | HTTP (OAuth) | `https://mcp.sentry.dev/mcp` |
| Mixpanel | HTTP (OAuth) | `https://mcp.mixpanel.com/mcp` |
| Filesystem | stdio | `@modelcontextprotocol/server-filesystem` |
| Git | stdio | `@modelcontextprotocol/server-git` |
| GitHub | stdio | `@modelcontextprotocol/server-github` |
| GitLab | stdio | `@modelcontextprotocol/server-gitlab` |
| PostgreSQL | stdio | `@modelcontextprotocol/server-postgres` |
| SQLite | stdio | `@modelcontextprotocol/server-sqlite` |
| Puppeteer | stdio | `@modelcontextprotocol/server-puppeteer` |

完整社区服务器列表见 [MCP 服务器注册表](https://github.com/modelcontextprotocol/servers)，协议细节见 [MCP 规范](https://modelcontextprotocol.io)。

---

## 故障排除

### 服务器无法启动

```bash
# Test the server command manually
npx -y @modelcontextprotocol/server-filesystem /path

# Increase startup timeout
# In config.toml:
[mcp_servers.filesystem]
startup_timeout_sec = 30
```

对于 stdio 服务器，Grok 会将进程的标准错误捕获到 `~/.grok/logs/mcp/<server>.stderr.log`，并在每次启动时截断。当服务器已启动但握手失败时，请检查该文件：

```bash
tail -f ~/.grok/logs/mcp/filesystem.stderr.log
```

### 查看服务器状态

使用 `grok inspect` 查看所有已加载的 MCP 服务器及其来源：

```bash
grok inspect          # Human-readable
grok inspect --json   # Machine-readable
```

### 调试日志

```bash
RUST_LOG=debug GROK_LOG_FILE=/tmp/grok.log grok
tail -f /tmp/grok.log
```

查找包含 `mcp` 的日志条目，以跟踪服务器启动、工具发现与工具调用执行。
