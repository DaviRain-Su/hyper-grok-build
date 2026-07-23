# 快速开始

Grok Build 是 SpaceXAI 出品的终端 AI 编程助手。它以 TUI（终端用户界面）运行，能理解你的代码库、执行 shell 命令、编辑文件、搜索网页并管理任务。

你可以用全屏 TUI 交互使用，也能以无头模式做脚本与 CI/CD，或通过 Agent Client Protocol（ACP）集成到编辑器中。

---

## 安装

安装最新稳定版（macOS、Linux，或通过 Git Bash 的 Windows）：

```bash
curl -fsSL https://x.ai/cli/install.sh | bash
```

安装指定版本：

```bash
curl -fsSL https://x.ai/cli/install.sh | bash -s 0.1.42
```

在 **Windows（PowerShell）** 上，使用原生 PowerShell 安装脚本：

```powershell
irm https://x.ai/cli/install.ps1 | iex
```

安装指定版本：

```powershell
$env:GROK_VERSION="0.1.42"; irm https://x.ai/cli/install.ps1 | iex
```

PowerShell 安装脚本会自动把 `%USERPROFILE%\.grok\bin` 加入用户 PATH。也可以通过 [Git for Windows](https://gitforwindows.org/)（Git Bash）或 MSYS2 使用上面的 bash 脚本。WSL 用户会自动获得 Linux 二进制。

验证安装：

```bash
grok --version
```

随时更新到最新版本：

```bash
grok update
```

---

## 首次启动

运行：

```bash
grok
```

首次启动时，Grok 会打开浏览器，让你通过 grok.com 登录。登录后，凭据会保存在 `~/.grok/auth.json`，跨会话复用。Grok 会自动刷新凭据；无法续期时会再次提示登录。

若更倾向 API Key（例如 CI/CD 或没有浏览器的环境），可设置 `XAI_API_KEY`：

```bash
export XAI_API_KEY="xai-..."
grok
```

完整认证选项（含 OIDC、外部认证提供方、设备码流程）见 [认证](02-authentication.md)。

---

## 基本交互

认证完成后，Grok 呈现全屏 TUI，主要两块区域：

- **回滚区（Scrollback）** —— 对话历史，包含你的提示、Grok 回复、工具调用、文件编辑等。
- **提示区（Prompt）** —— 底部输入区，用来输入消息。

输入消息后按 `Enter` 发送。Grok 会按需读文件、跑命令、改代码。每次工具执行都会实时流式显示在回滚区。

按 `Tab` 在提示区与回滚区之间切换焦点。回合进行中时，`Ctrl+C` 取消（或先清空非空草稿）；`Esc` 在回合中为无操作。空闲时，800ms 内连按两次 `Esc` 可清空非空提示；若提示为空且已有对话消息，则打开回溯 —— 详见 [键盘快捷键](03-keyboard-shortcuts.md#escape)。焦点在回滚区时，用方向键选择条目并折叠/展开。若要用 `j`/`k` 导航、`h`/`l` 折叠，请启用 Vim 模式。

### 文件引用

在提示中用 `@` 附加文件：

```
@src/main.rs              # 附加文件
@src/main.rs:10-50        # 附加第 10–50 行
@src/                     # 浏览目录
```

`@` 会打开模糊文件选择器。默认遵循 `.gitignore` 并隐藏点文件。加前缀 `!` 可搜索隐藏文件：

```
@!.github                 # 搜索隐藏文件
@!.env                    # 附加 .env 文件
```

### 权限

默认情况下，执行 shell 命令或编辑文件前 Grok 会请求许可。可逐个批准，或切换始终批准模式：

- 按 `Ctrl+O` 切换始终批准模式
- 启动时加 `--yolo`：`grok --yolo`
- 在提示中输入 `/always-approve` 切换

---

## 核心概念

### 会话（Sessions）

每次对话都是一个 **会话**。会话自动保存到 `~/.grok/sessions/`，之后可恢复。每个会话记录完整对话历史、工具调用、文件编辑与任务状态。

- 新建会话：`Ctrl+N` 或 `/new`
- 恢复会话：TUI 中用 `/resume`，或 CLI 用 `--resume <ID>`
- 继续最近一次会话：`grok -c`

### 回滚区（Scrollback）

回滚区是主显示区域，包含：

- **用户提示** —— 你的消息，以粘性标题渲染
- **智能体消息** —— Grok 回复，含完整 Markdown 与语法高亮
- **思考块** —— Grok 的推理过程（可折叠）
- **工具调用** —— 文件编辑（内联 diff）、命令执行、搜索结果等
- **任务列表** —— 跟踪进度的 TODO

用 `Left`/`Right`（或 Vim 模式下的 `h`/`l` 与 `e`）折叠/展开当前条目。Vim 模式下按 `y` 复制内容、`Y` 复制元数据（例如执行的命令）。任意模式下按 `Enter` 在全屏查看器中打开。

### 工具（Tools）

Grok 内置工具包括：

| 工具 | 说明 |
|------|------|
| `read_file` / `search_replace` | 按行精确读写文件 |
| `grep` | 全库正则搜索（基于 ripgrep） |
| `list_dir` | 列出目录内容 |
| `run_terminal_command` | 执行 shell 命令 |
| `web_search` / `web_fetch` | 网页搜索与抓取 |
| `todo_write` | 创建与管理任务列表 |
| `spawn_subagent` | 派生并行子智能体会话 |
| `memory_search` | 跨会话记忆搜索 |

可通过 [MCP 服务器](05-configuration.md#mcp-servers) 扩展工具（GitHub、数据库等）。

### 斜杠命令（Slash Commands）

在提示中输入 `/` 使用命令，无需写完整提示即可执行快捷操作：

```
/model grok-build                 # 切换模型
/compact                          # 压缩对话历史
/always-approve                   # 切换始终批准
/new                              # 新建会话
```

完整列表见 [斜杠命令](04-slash-commands.md)。

---

## 常用启动选项

```bash
# 启动交互 TUI，并把初始提示作为第一轮发送
grok "fix the failing auth test and run it"

# 在新的 git worktree 中带初始提示。请用 --worktree=<name>（带 `=`），
# 否则提示会被当成 worktree 名称 —— `grok -w "refactor module X"`
# 会把 "refactor module X" 当作 worktree 标签，而不是提示。
grok --worktree=feat "refactor module X"

# 以指定分支（如 main）为基准创建 worktree，而不是当前 HEAD：
grok -w --ref main "implement feature from main"


# 在指定项目目录启动
grok --cwd ~/projects/my-app

# 添加项目规则
grok --rules "Always use TypeScript. Prefer functional components."

# 自动批准所有工具执行
grok --yolo

# 使用指定模型
grok -m grok-build

# 恢复先前会话
grok --resume <session-id>

# 继续最近一次会话
grok -c

# 实验性「回滚区原生」渲染模式。会记住：普通 `grok` 会以
# 上次通过 --minimal/--fullscreen（或 /minimal//fullscreen）选择的模式打开。
grok --minimal

# 回到标准全屏 TUI（并再次记住）
grok --fullscreen

# 无头模式（脚本）
grok -p "Explain this codebase"
```

---

## 无头模式

以非交互方式运行，用于脚本、CI/CD 与自动化：

```bash
grok -p "Your prompt here"
```

输出格式：

| 格式 | 标志 | 说明 |
|------|------|------|
| `plain` | （默认） | 人类可读文本 |
| `json` | `--output-format json` | 单个 JSON 对象，含 `text`、`stopReason`、`sessionId`、`requestId` |
| `streaming-json` | `--output-format streaming-json` | NDJSON 事件流，便于实时处理 |

CI/CD 示例：

```bash
grok -p "Review changes for bugs" --output-format json --yolo | jq -r '.text'
```

---

## 项目规则（AGENTS.md）

在仓库中创建 `AGENTS.md` 可添加项目级说明。Grok 会读取这些文件，并在对话开始时作为项目指令注入：

```
~/.grok/AGENTS.md           # 全局规则（所有项目）
<repo-root>/AGENTS.md       # 仓库级规则
<cwd>/AGENTS.md             # 目录级规则（优先级最高）
```

更深层的文件优先。为兼容性也会读取 `CLAUDE.md`。

---

## 下一步

| 文档 | 内容 |
|------|------|
| [认证](02-authentication.md) | 浏览器登录、API Key、OIDC、外部认证、设备码流程 |
| [键盘快捷键](03-keyboard-shortcuts.md) | 全部按键绑定参考 |
| [斜杠命令](04-slash-commands.md) | 全部 `/` 命令 |
| [配置](05-configuration.md) | config.toml、pager.toml、环境变量 |
