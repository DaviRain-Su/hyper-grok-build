# 配置

Grok 从配置文件、环境变量与 CLI 标志读取设置。本页介绍常用选项。

---

## 优先级

设置按从高到低解析：

1. **CLI 标志**（如 `--yolo`、`--model`、`--sandbox`）
2. **环境变量**（如 `XAI_API_KEY`、`GROK_MEMORY`）
3. **config.toml**（`~/.grok/config.toml`）
4. **托管 / 要求配置**（组织可能部署的文件，如 `managed_config.toml` / `requirements.toml`）
5. **内建默认值**

---

## config.toml（主配置）

位置：`~/.grok/config.toml`。文件缺失时使用内建默认，只需设置要覆盖的值。

### 常规设置

```toml
[cli]
auto_update = true                     # 启动时检查更新

[models]
default = "grok-build"                 # 新会话使用的模型
web_search = "grok-4.20-multi-agent"   # web_search 工具使用的模型

# 应用于每个模型的默认值；按模型 [model.<id>] 始终优先。
# 按模型覆盖与完整细节见「自定义模型」。
extra_headers = { "X-Request-Tags" = "team=example,env=prod" }
temperature = 0.7
top_p = 0.95
max_completion_tokens = 8192
max_retries = 8
inference_idle_timeout_secs = 600
stream_tool_calls = true

[ui]
simple_mode = true                     # readline 风格提示编辑（默认）；false = 提示区 vim 编辑
vim_mode = false                       # vim 风格回滚导航键（默认：false）
max_thoughts_width = 120               # 推理显示最大列宽
default_selected_permission = "always_allow_all_sessions" # 会话首次批准提示的预选行
remember_tool_approvals = false        # 在权限提示上显示按命令「始终允许」选项；
                                       # 授权按项目记忆（默认：false）；见 22-permissions-and-safety.md
show_thinking_blocks = true            # 在 TUI 中显示智能体思考块（默认：true）
group_tool_verbs = true                # 将连续的 read/search/list 工具调用与子智能体行
                                       # —— 以及其间完成的思考 —— 折叠为一行（默认：true）
collapsed_edit_blocks = false          # 将编辑显示为单行 +N/-M diffstat 摘要，并合并
                                       # 同一文件的连续编辑为一行，展开查看
                                       # diffs（默认：false；pager.toml [scrollback.blocks.edit]
                                       # 的 expanded_by_default/line_summary 覆盖折叠形状）
page_flip_on_send = true               # 刚发送的提示钉在视口顶部，使
                                       # 回复从新页开始（默认：true）；设为 false
                                       # 则发送时从不移动滚动位置
screen_mode = "fullscreen"             # 默认渲染模式："fullscreen" | "minimal"
                                       # （未设置 → fullscreen）；通过 /settings → Default screen mode 设置

[features]
telemetry = false                      # 匿名用量遥测
feedback = true                        # 反馈系统（默认：true）
lsp_tools = false                      # 暴露 lsp 工具
codebase_indexing = true               # 代码图索引（默认：true）
two_pass_compaction = false            # 预触发双遍压缩（默认：false，可选）
remote_fetch = true                    # 允许可选的在线模型目录拉取（默认：true；
                                       # 防火墙/气隙部署设 false；后台
                                       # managed-config 同步有独立开关：managed_config）

[session]
auto_compact_threshold_percent = 85    # 达到上下文窗口此 % 时自动 compact（默认：85）
load_envrc = true                      # 加载 .envrc 环境变量

[tools]
respect_gitignore = false              # 默认：false；设 true 使每个工具跳过 gitignored 文件
```

#### 输入模式

`[ui] simple_mode` 控制 **提示区**（输入编辑器）如何编辑文本。与回滚区移动无关；那是 [`vim_mode`](#vim-mode)。

| 值 | 行为 |
|----|------|
| `true`（默认） | **Readline 编辑。** 普通 readline 风格文本输入。 |
| `false` | **Vim 编辑（实验性）。** Vim 风格模态编辑（normal 与 insert）。提示为空时以 normal 模式开始，焦点在回滚区。 |

将提示切换为 vim 风格编辑：

```toml
[ui]
simple_mode = false
```

也可从设置窗格（`/settings` → **Disable vim input mode**）切换；Grok 会把选择写入 `[ui] simple_mode`。`simple_mode` 与 `vim_mode` 相互独立 —— 一个管提示编辑器，一个管回滚导航。完整绑定见 [键盘快捷键](03-keyboard-shortcuts.md)。

#### 默认选中权限

智能体请求运行命令（或其他工具操作）时，批准菜单默认高亮一行。`[ui] default_selected_permission` 设置会话 **首次** 提示时是哪一行。

| 值 | 预选行 |
|----|--------|
| `always_allow_all_sessions`（默认） | 「Always allow on all sessions」行。 |
| `allow_command_always` | 「Always allow this command」行。 |
| `allow_once` | 「Yes」/ 仅此一次允许行。 |
| `reject` | 拒绝行。 |

```toml
[ui]
default_selected_permission = "allow_once"
```

回答首次提示后光标变为 **粘性**：之后每次提示预选你上次确认的选项（选一次「No」则后续从拒绝行开始），跨 edit / bash / MCP 提示保持，直到重启。因此该设置只决定起点。

值不区分大小写；未设置或无法识别则回退到 `always_allow_all_sessions`。`allow_command_always` 行始终限定于正在批准的具体操作（命令 / 工具 / 域名 / 编辑会话），从不是全局允许一切 —— 那是 `always_allow_all_sessions` 的职责。注意按命令「Always allow」行仅在 `[ui] remember_tool_approvals = true`（默认 false）时出现。见 [22-permissions-and-safety.md](22-permissions-and-safety.md)。

也可用 `GROK_DEFAULT_SELECTED_PERMISSION` 覆盖，便于无头或智能体测试运行不改 `config.toml`。优先级：环境变量 → `config.toml` → `always_allow_all_sessions`。

#### Vim 模式

`[ui] vim_mode` 控制 **回滚** 窗格是否启用 vim 风格绑定。不影响提示区。

| 值 | 行为 |
|----|------|
| `false`（默认） | 回滚区中裸字母与 `Shift+字母`（`j`/`k`、`h`/`l`、`g`/`G`、`y`/`Y`、`o`/`O`、`r`、`x`、`e`/`E`、`H`/`L`，以及 `i`）被抑制：按下会聚焦提示并键入该字符。方向键、`Tab`、`Space`、`PageUp`/`PageDown` 与所有 `Ctrl+字母` 仍可导航。`Esc` **不是** 回滚键 —— 遵循清空 / 回溯 / 回合中途吞掉策略（见 [键盘快捷键](03-keyboard-shortcuts.md#escape)）。 |
| `true` | 所有 vim 风格回滚绑定生效，与 [键盘快捷键](03-keyboard-shortcuts.md) 所列一致。 |

运行时用 `/vim-mode` 切换，或 `/settings` → **Vim scrollback navigation**。Grok 立即写入 `[ui] vim_mode` 并应用到之后每个 pager 会话，含同进程中的新智能体与子智能体。无按会话覆盖 —— 下次启动以 `config.toml` 为准。`vim_mode` 与 `simple_mode` 独立。

#### 屏幕模式

`[ui] screen_mode` 是普通 `grok` 启动的 **默认渲染模式**。通过 `/settings` → **Default screen mode**（需重启）或手改 `config.toml` —— 两者都写文件。CLI 标志（`--minimal` / `--fullscreen`）与斜杠命令（`/minimal` / `/fullscreen`）为会话范围，**不** 写此键；斜杠切换后，反向命令仅在本会话切回。

| 值 | 行为 |
|----|------|
| 未设置 | 设置显示 **Fullscreen**。启动时无粘性偏好：遗留 `pager.toml` 的 `[terminal] minimal` 仍可强制 minimal；泄漏鼠标报告的终端（JediTerm/Windows）可能自动打开 minimal，直到你设置显式值。否则由 alt-screen 策略选择全屏 vs 内联。 |
| `"fullscreen"` | 粘性非 minimal。全屏 vs 内联仍遵循 alt-screen 策略（`--no-alt-screen`、`[terminal] alt_screen`、终端自动检测）。 |
| `"minimal"` | 粘性 minimal（回滚区原生）模式。 |

CLI 标志对该次调用始终优先于配置值。

#### 发送时将提示钉到顶部

默认发送提示时会将其滚到视口顶部，使回复从新页开始。设 `[ui] page_flip_on_send = false`（或在 `/settings` → Appearance 切换 **Snap prompt to top on send**）可在发送时不改滚动位置。下次发送即生效 —— 无需重启。

#### 滚动

四个 `[ui]` 设置调节鼠标滚轮与触控板滚动。均立即生效，可在设置窗格编辑（`/settings` → **Scroll speed** / **Scroll input** / **Scroll lines** / **Invert scroll**）。

| 键 | 值（默认） | 行为 |
|----|------------|------|
| `scroll_speed` | `1`–`100`（`50`） | 滚轮与触控板速度倍率。`50` = 1.0x，`1` = 0.1x，`100` = 6.0x。 |
| `scroll_mode` | `auto` \| `wheel` \| `trackpad`（`auto`） | 滚轮 vs 触控板检测是启发式的（终端滚动事件无幅度）；自动误判时强制一种 —— 例如滚轮刻度过猛，或触控板感觉像步进。 |
| `scroll_lines` | `1`–`10`（未设置） | 每次滚动刻度的行数，**同时** 应用于滚轮与触控板。未设置时使用各终端自身配置（例如 tmux 下保守的 1 行/事件）。提交任意值 —— 即使是设置窗格显示的 `3` —— 也会永久切到该显式覆盖。 |
| `invert_scroll` | `false` \| `true`（`false`） | 反转垂直滚动方向（「自然」滚动）。 |

```toml
[ui]
scroll_speed = 50
scroll_mode = "auto"     # auto | wheel | trackpad
invert_scroll = false
# scroll_lines 默认未设置：由按终端配置主导。
# scroll_lines = 3
```

各设置也有环境变量覆盖，仅首次加载时应用（同样便于无头 / 测试）：`GROK_SCROLL_SPEED`、`GROK_SCROLL_MODE`、`GROK_INVERT_SCROLL`（`1`/`true`/`0`/`false`）、`GROK_SCROLL_LINES`。优先级：环境变量 → `config.toml` → 默认。无法识别的值回退默认，越界数字钳位。

### 工具配置

```toml
[toolset.bash]
timeout_secs = 120.0                   # 前台命令超时秒数（默认：120）
output_byte_limit = 20000              # 最大捕获输出字节（默认：20000）

[toolset.ask_user_question]
timeout_enabled = true                 # false = 永远等待回答（默认：true）
timeout_secs = 1800                    # 启用时等待秒数（默认：1800 / 30 分钟）

[toolset.web_fetch]
proxy_endpoint = "https://proxy.example.com"   # 出口代理 URL
allowed_domains = ["docs.rs", "x.ai"]          # 覆盖内建允许列表
allow_local = false                            # true = 仅允许 localhost / 127.0.0.0/8 / ::1
```

`allow_local` 默认关闭（SSRF 失败关闭）。开启（或设 `GROK_WEB_FETCH_ALLOW_LOCAL=1`）后，`web_fetch` 仅可访问 **显式** 环回主机 —— 私有、链路本地与云元数据范围仍被阻止。解析：TOML > 环境 > 默认关闭。

`[toolset.ask_user_question]` 在 **requirements.toml**、**托管配置** 与用户 **`config.toml`** 中均被遵循。优先级：requirements → 环境（`GROK_ASK_USER_QUESTION_TIMEOUT_ENABLED` / `GROK_ASK_USER_QUESTION_TIMEOUT_SECS`）→ 用户配置 → 托管 → 默认。在用户配置中设 `timeout_enabled = false` 可为自己禁用自动问卷超时；`timeout_secs` 须为正整数。也可从 `/settings` → **Ask-Question timeout**（Agent & Approval 下）切换 `timeout_enabled`；变更应用于新启动的会话。

### 认证

完整说明见 [认证](02-authentication.md)。

```toml
[auth]
auth_provider_command = "/usr/local/bin/my-auth-provider"
auth_provider_label = "Acme Corp"
auth_token_ttl = 3600

[grok_com_config.oidc]
issuer = "https://acme.okta.com"
client_id = "0oa1b2c3d4e5f6g7h8i9"
# scopes = ["openid", "profile", "email", "offline_access", "api:access"]
# audience = "https://api.acme.com"
```

### 自定义模型

添加自定义模型端点以使用替代提供方或自托管模型。

```toml
[model.my-model]
model = "model-id"                    # 发给 API 的模型标识
base_url = "https://api.example.com/v1"  # OpenAI 兼容端点
name = "Display Name"                 # 模型选择器中显示
description = "Model description"      # 可选
api_key = "sk-..."                    # 该提供方的 API key
env_key = "XAI_API_KEY"               # 保存 API key 的环境变量；字符串或数组（第一个已设置且非空者胜出）
temperature = 0.7                     # 采样温度（0.0-2.0）
top_p = 0.95                          # nucleus 采样参数
max_completion_tokens = 8192          # 每次响应最大 token
context_window = 128000               # 上下文窗口大小（用于自动 compact）
```

凭据解析：`api_key` > `env_key` > 已登录会话令牌 > `XAI_API_KEY`。

覆盖内建模型：用其名称作为节键，只设需要的字段：

```toml
[model.grok-build]
api_key = "my-api-key"
```

### MCP 服务器

通过 Model Context Protocol 配置外部工具集成。

```toml
[mcp_servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "ghp_xxx" }
enabled = true                        # 启用/禁用（默认：true）
startup_timeout_sec = 30              # 初始化超时秒数（默认：30）
tool_timeout_sec = 6000              # 工具调用超时秒数（默认：6000）
tool_timeouts = { create_issue = 120 }  # 按工具超时覆盖

[mcp_servers.postgres]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-postgres", "postgresql://user:pass@localhost/db"]

[mcp_servers.my-streamable-server]
url = "https://mcp.example.com/api/mcp"  # HTTP/SSE 传输
headers = { "x-mcp-session-id" = "{{session_id}}" }
```

MCP 服务器也可在 `.grok/config.toml` 中按项目设置。项目作用域配置贡献 `[mcp_servers]`、`[plugins]` 与 `[permission]` 规则；其他节仅从 `~/.grok/config.toml` 加载。

`[mcp_servers]` 与 `[plugins]` 优先级：`.grok/config.toml`（当前目录）> `<repo-root>/.grok/config.toml` > `~/.grok/config.toml`。`[permission]` 规则不按优先级覆盖 —— 跨所有文件合并，`deny` > `ask` > `allow`（见 [22-permissions-and-safety.md](22-permissions-and-safety.md)）。

### 记忆

跨会话持久化知识（需要 `--experimental-memory` 或 `GROK_MEMORY=1`）。

```toml
[memory]
enabled = false                       # 启用记忆

[memory.session]
save_on_end = true                    # 会话结束时写入元数据摘要

[memory.watcher]
enabled = true                        # 监视记忆文件的外部编辑

[memory.search]
max_results = 6                       # 默认结果数
min_score = 0.35                      # 最低相关性分数

[memory.initial_injection]
enabled = true                        # 第一回合自动注入记忆
min_score = 0.0                       # 第一回合注入的分数阈值

[memory.embedding]
model = "embedding-model"             # 嵌入模型名
dimensions = 1024                     # 向量维度
```

### 子智能体

```toml
[subagents]
enabled = true

[subagents.toggle]
explore = true                        # 启用/禁用特定类型
plan = false

[subagents.models]
explore = "grok-build"               # 路由到不同模型
```

要固定子智能体使用的模型，在 `[subagents.models]` 下设置对应项。

### 目标模式与后台工作流

`/goal` 有两种驱动，由后台工作流设置决定。工作流启用时，宿主拥有的工作流引擎评估轮次并驱动完成验证；禁用时 `/goal` 回退到遗留的模型侧 `update_goal` 工具。`/goal` 是否可用是另一独立开关（目标功能设置）。

后台工作流 —— `workflow` 工具、具名 `.grok/workflows/*.rhai` 脚本、`/deep-research` 与 `/workflow` 启动 —— **默认关闭**。

```toml
[workflows]
enabled = true                        # 启用后台工作流（或 GROK_WORKFLOWS=1）
```

项目工作流从 `<repo-root>/.grok/workflows/` 发现；用户工作流从 `~/.grok/workflows/`。发现与调用以脚本的 `meta.name` 为准，因此请保持文件名与 `meta.name` 一致。内建优先于项目名，项目名优先于用户名，请跨作用域保持名称唯一。

每次启动获得会话唯一的显示句柄，如 `deep-research-2`。该句柄出现在 `/workflows` 运行仪表盘，并传给 `/workflow pause`、`resume` 或 `stop` —— 内部 run ID 从不出现在命令中。编号句柄不是可复用的定义名，因此在你选择新的唯一 `meta.name` 并自行保存编辑后的脚本之前，仪表盘禁用 **save**。示例见 [斜杠命令](04-slash-commands.md)。

### 技能

```toml
[skills]
paths = ["~/my-team-skills"]          # 额外扫描目录
ignore = ["~/my-team-skills/wip"]     # 排除路径
disabled = ["wip-skill"]              # 保留列出但不激活的技能名
```

### 宿主兼容性

控制 Cursor、Claude、Codex 与 OMP 的厂商兼容。每个单元默认 `true`。会话发现按需执行：每个工具需要其 `sessions` 单元与匹配的 `resume-claude`、`resume-codex`、`resume-cursor` 或 `resume-omp` 技能；缺少技能时，Grok 不会读取该工具的会话文件系统。

```toml
[compat.cursor]
skills = true     # 扫描 ~/.cursor/skills/ 与 <cwd>/.cursor/skills/
rules = true      # 扫描 ~/.cursor/rules/ 与 <dir>/.cursor/rules/
agents = true     # 扫描 ~/.cursor/ 中的具名指令文件
mcps = true       # 扫描 ~/.cursor/mcp.json 与 <cwd>/.cursor/mcp.json
hooks = true      # 扫描 ~/.cursor/hooks.json 与 <cwd>/.cursor/hooks.json
sessions = true   # 列出近期 Cursor 会话以便恢复

[compat.claude]
skills = true     # 扫描 ~/.claude/skills/ 与 <cwd>/.claude/skills/
rules = true      # 扫描 ~/.claude/rules/ 与 <dir>/.claude/rules/
agents = true     # 扫描 ~/.claude/ 与 <dir>/.claude/CLAUDE*.md
mcps = true       # 从 ~/.claude.json 扫描 MCP 服务器
hooks = true      # 从 ~/.claude/settings.json 扫描 hooks
sessions = true   # 列出近期 Claude Code 会话以便恢复

[compat.codex]
sessions = true   # 列出近期 Codex 会话以便恢复

[compat.omp]
sessions = true   # 列出近期 OMP 会话以便恢复
```

Codex 的 `skills`、`rules`、`agents`、`mcps` 与 `hooks` 单元为预留且当前惰性 —— 不会启用 `.codex` 发现。OMP 当前仅提供 `sessions` 兼容单元；扫描器遵循 OMP 的默认/profile/XDG 会话目录以及 `PI_CODING_AGENT_DIR` 覆盖。

对 Claude 与 Cursor，`rules` 与 `agents` 相互独立：关闭具名指令文件不会禁用主目录或项目规则目录，关闭规则也不会禁用具名文件。Claude 的 `agents` 单元门控主目录级 `~/.claude/` 具名文件与项目 `<dir>/.claude/CLAUDE*.md`；通用顶层 `Claude.md`、`CLAUDE.md` 与 `CLAUDE.local.md` 仍被识别。项目规则路径从仓库根到当前目录的每一层都会扫描。

每个单元可通过环境变量或 `config.toml` 设置；名称见环境变量参考。解析：环境变量 > config.toml > 默认（开）。

`grok inspect` 将仍需会话启动时解析的单元报告为 `?` 直到有值；有显式环境或 TOML 值的单元使用该值。受影响的发现条目在 JSON 中报告 `compatibilityStatus: "unresolved"`，在人类输出中为 `[compat unresolved]`。

### 插件

```toml
[plugins]
paths = ["~/my-plugins/custom-tools"]
disabled = ["user/a1b2c3d4/noisy-plugin"]
```

### 提示（Hints）

`[hints]` 保存小型 UI 偏好 —— 主要是「别再问我」类退出。在 TUI 中选择「don't ask again」时 Grok 会写入；也可手改或删除；删除键恢复默认。

`[hints]` 从 **有效配置合并** 读取，优先级：系统托管 → 用户 `managed_config.toml` → 用户 `config.toml` → 用户 `requirements.toml` → 系统 `requirements.toml`，高层优先。TUI 仅将退出选项 **写入** 用户 `~/.grok/config.toml`。

```toml
[hints]
project_picker_disabled = false        # 跳过项目目录选择器
memory_modal_fullscreen = false        # 记住记忆模态全屏状态
new_session_worktree_mode = "never"    # /new worktree 提示："ask" | "always" | "never"
fork_worktree_mode = "ask"             # /fork worktree 提示："ask" | "always" | "never"
```

| 键 | 类型 | 默认 | 说明 |
|----|------|------|------|
| `project_picker_disabled` | bool | `false` | 为 `true` 时，在非项目目录（主目录、Desktop、Downloads、`/tmp`）启动后首次提示时跳过选择项目目录的选择器。在选择器中选 **"Don't ask me again"** 时自动设置。团队可在 `managed_config.toml` 或 `requirements.toml` 中固定。 |
| `memory_modal_fullscreen` | bool | `false` | 记住记忆模态上次是否全屏打开。 |
| `new_session_worktree_mode` | string | `"never"` | `/new` 的 worktree 提示：`ask` 显示弹窗，`always` 创建 worktree，`never` 跳过。 |
| `fork_worktree_mode` | string | `"ask"` | `/fork` 的 worktree 提示：`ask`、`always` 或 `never`。 |

### 通知

智能体完成回合或需要批准时触发终端通知。使用终端原生协议（OSC 9、OSC 99、OSC 777 或 BEL），默认按焦点门控，仅在你未看终端时触发。

```toml
[ui.notifications]
method = "auto"           # auto|osc9|osc99|osc777|bel|none
condition = "unfocused"   # unfocused|always|never
idle_threshold_secs = 3   # 失焦多少秒后触发通知
events = ["turn_complete", "approval_required"]
sleep_prevention = true   # 智能体回合期间防止显示休眠
progress_bar = true       # 显示标签进度条（OSC 9;4）

[ui.notifications.title]
enabled = true
items = ["action-required", "spinner", "activity", "session-name", "grok"]
```

| 选项 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `method` | string | `"auto"` | 通知协议。`auto` 为你的终端选择最佳。 |
| `condition` | string | `"unfocused"` | 何时通知：`unfocused`（仅终端失焦）、`always` 或 `never`。 |
| `idle_threshold_secs` | integer | `3` | 失焦最少多少秒后才触发。 |
| `events` | array | `["turn_complete", "approval_required"]` | 触发通知的事件。选项：`turn_complete`、`approval_required`、`session_ready`、`task_complete`、`agent_error`。 |
| `sleep_prevention` | bool | `true` | 智能体工作时保持显示器唤醒（macOS/Linux）。 |
| `progress_bar` | bool | `true` | 在终端标签中显示进度指示（OSC 9;4）。 |
| `title.enabled` | bool | `true` | 设置终端标题以反映智能体状态。 |
| `title.items` | array | （见上） | 标题栏显示项。选项：`action-required`、`spinner`、`activity`、`session-name`、`cwd`、`model`、`turn-timer`、`grok`。 |

#### 终端支持矩阵

| 终端 | 自动协议 | 焦点跟踪 | 进度条 |
|------|----------|----------|--------|
| iTerm2 | OSC 9 | 是 | 是 |
| Kitty | OSC 99 | 是 | 否 |
| Ghostty | OSC 777 | 是 | 是 |
| WezTerm | OSC 9 | 是 | 是 |
| Warp | OSC 9 | 是 | 否 |
| Alacritty | BEL | 是 | 否 |
| VS Code | BEL | 是 | 否 |
| Apple Terminal | BEL | 否 | 否 |
| VTE（GNOME Terminal） | OSC 777 | 是 | 否 |
| Grok Desktop | 无（原生） | N/A | N/A |
| 未知 | BEL | 否 | 否 |

`method = "auto"` 时，Grok 检测终端品牌并选择最佳协议。显式设置 `method` 可覆盖。

#### 通知 hooks

事件触发时运行你自己的命令。hooks 在环境中收到 `$GROK_EVENT`、`$GROK_MESSAGE` 与 `$GROK_SESSION_ID`。

```toml
# macOS 原生通知
[[ui.notifications.hooks]]
command = "terminal-notifier -title 'Grok' -message '$GROK_MESSAGE'"
events = ["turn_complete", "approval_required"]
only_unfocused = true
timeout_secs = 10

# 推送到 ntfy 服务器
[[ui.notifications.hooks]]
command = "curl -s -d '$GROK_MESSAGE' ntfy.sh/my-grok-alerts"
events = ["turn_complete"]
only_unfocused = true
timeout_secs = 10

# 播放声音
[[ui.notifications.hooks]]
command = "afplay /System/Library/Sounds/Glass.aiff"
events = ["turn_complete"]
only_unfocused = true
timeout_secs = 5
```

| Hook 选项 | 类型 | 默认 | 说明 |
|-----------|------|------|------|
| `command` | string | （必需） | 要运行的 shell 命令。 |
| `events` | array | `[]` | 触发此 hook 的事件（空 = 全部事件）。 |
| `only_unfocused` | bool | `true` | 仅在终端失焦时触发。 |
| `timeout_secs` | integer | `10` | 多少秒后杀死 hook 进程。 |

#### 故障排除

**tmux 中通知不工作：** tmux 默认阻止转义序列，请启用穿透：

```bash
# 在 ~/.tmux.conf
set -g allow-passthrough on
```

之后重启 tmux。若穿透不可用（tmux < 3.3），设 `method = "bel"`，无需穿透即可工作。

**焦点跟踪不工作：** 部分终端不报告焦点事件。若 `condition = "unfocused"` 从不触发，试 `condition = "always"`。除 Apple Terminal 与未识别终端外，Grok 在每个检测到的终端支持焦点跟踪。

**休眠防止不生效：** macOS 上通过 CoreFoundation 的 `IOPMAssertionCreateWithName`；Linux 上用 `systemd-inhibit`（须在 `$PATH`）。确保相关工具可用。防止仅在智能体回合中活动，回合结束自动释放。

### 键盘快捷键

键盘快捷键 **不可** 配置 —— 全部绑定内建。完整参考见 [键盘快捷键](03-keyboard-shortcuts.md)。

### 遥测

这些是独立旋钮（见 [用量监控](24-monitoring-usage.md#related-settings)）：

- **`[features] telemetry`** / `GROK_TELEMETRY_ENABLED` —— 产品分析总开关。`/privacy` 不改它。
- **`/privacy`** / 设置 —— 编码数据共享，与遥测分开。
- **`[telemetry] trace_upload`** / `GROK_TELEMETRY_TRACE_UPLOAD` —— 会话轨迹；未设置时跟随遥测。
- **`[telemetry] otel_*`** / `GROK_EXTERNAL_OTEL` —— 到你自己收集器的外部 OTEL（见下）。

遥测开启时，运行自有收集器的企业可在 `[telemetry]` 下重定向或关闭部分：

```toml
[telemetry]
events_url = "https://telemetry.your-company.com/events"  # 发送事件到你自己的收集器
events_api_key = "your-collector-token"                   # 收集器鉴权（若需要）
mixpanel_enabled = false                                  # 禁用 Mixpanel 产品分析
trace_upload = false                                      # 禁用会话/轨迹上传（未设置时继承遥测开关）
```

仅在需要把遥测指向自有基础设施或关闭部分时设置。内建端点与凭据由 Grok 管理 —— 保持未设置以使用默认。

同一 `[telemetry]` 表也配置 **外部 OpenTelemetry 流**，这是独立的 opt-in（不需要上面的遥测开关），将策划过的、无内容的用量 schema 发到你 *自己的* OTLP 收集器。收集器鉴权来自 `OTEL_EXPORTER_OTLP_HEADERS`，从不落盘。完整 schema、环境变量与隐私模型见 [监控与用量](24-monitoring-usage.md)。

```toml
[telemetry]
otel_enabled = true                                       # 外部 OTEL 总开关（= GROK_EXTERNAL_OTEL）
otel_metrics_exporter = "otlp"                            # otlp | console | none
otel_logs_exporter = "otlp"                               # otlp | console | none
otel_endpoint = "https://collector.corp.example:4318"     # OTLP 基端点
otel_protocol = "http/protobuf"                           # http/protobuf | grpc
otel_log_user_prompts = false                             # 内容门控（管理员可通过 requirements 固定）
otel_log_tool_details = false                             # 内容门控（管理员可通过 requirements 固定）
```

### 企业部署

企业用完整配置示例：

```toml
[cli]
auto_update = false

[auth]
auth_provider_command = "/usr/local/bin/my-company-auth-provider"
auth_provider_label = "Acme Corp"
auth_token_ttl = 3600

[models]
default = "company-grok"

[model.company-grok]
model = "grok-build"
base_url = "https://grok-proxy.acme.com/"
name = "Grok Build Latest (Proxy)"
context_window = 128000

[features]
telemetry = false
```

---

## pager.toml（外观配置）

位置：`~/.grok/pager.toml`。控制 TUI 外观。变更需重启生效。

### 终端

```toml
[terminal]
alt_screen = "auto"                   # 全屏模式："auto"、"always"、"never"
```

- `auto`（默认）：终端支持时使用备用屏幕。
- `always`：始终使用备用屏幕。
- `never`：在终端主回滚缓冲区中内联运行。

### 动画

```toml
[animation]
fps = 30                              # 动画帧率（每秒 tick）
wave_rows = 32                        # 强调动画每个波形周期的行数
```

### 提示

```toml
[prompt]
collapse_unfocused = true             # 回滚聚焦时折叠提示
mouse_hover = true                    # 在提示控件上显示悬停高亮
show_prefix = true                    # 显示提示前缀字符
```

紧凑模式不在此持久化 —— 运行时用 `[ui] compact_mode` 或 `/compact-mode` 控制。

### 回滚区

```toml
[scrollback.layout]
outer_vpad = 1                        # 垂直内边距
outer_hpad_left = 2                   # 左水平内边距
outer_hpad_right = 2                  # 右水平内边距
block_pad_left = 2                    # 块内、内容左侧内边距
block_pad_right = 2                   # 块内、内容右侧内边距

[scrollback.scrollbar]
enabled = true                        # 显示滚动条
gap_left = 0                          # 内容与滚动条间距
gap_right = 0                         # 滚动条与屏幕边缘间距

[scrollback.scroll]
margin = 0                            # 选择上下最少上下文行数
min_page_fraction = 0                 # 最小滚动占视口百分比（0-100）
follow_indicator = "center"           # 跟随指示："center" 或 "none"
follow_auto_select = true             # 跟随模式自动选中最新条目
follow_by_overscroll = true           # 滚过底部进入跟随模式
anchor_on_fold = true                 # 折叠时保持块位置
respect_manual_folds = true           # 可选（默认：false）：流式/结束期间保持手动折叠；跟随中展开会停止自动滚动

[scrollback.display]
sticky_headers = true                 # 将用户提示钉为粘性标题
tab_width = 4                         # 每个 tab 字符的空格数
expandable_indicator = true           # 在可折叠条目上显示展开指示
expandable_indicator_running = true   # 在运行中条目上显示指示
expandable_indicator_char = "›"       # 展开指示字符（默认："›"）
selection_buttons = false             # 选择时显示复制/查看按钮
line_under_last_entry = false         # 最后条目下方水平线
group_selection_split = true          # 展开块时分割选择框
highlight_overlays_border = false     # 高亮延伸到选择框边框
dim_accent = 0.5                      # 折叠强调的变暗因子（0.0-1.0）
```

`respect_manual_folds` 默认关闭。开启后，你手动折叠的块会被钉住：流式更新与结束事件（例如思考块结束）不改其折叠状态；在跟随模式尾随新内容时展开块会停止自动滚动以保持视图。跟随可通过 `Shift+G`、在最后一条上按 `j`、滚过底部或发送新提示恢复。`Shift+E` 清除全部钉住；`Ctrl+E` 清除思考块上的钉住。

### 块配置

```toml
[scrollback.blocks.edit]
indent = true                         # 缩进 diff 内容
vpad = false                          # 垂直内边距
# expanded_by_default = true          # 未设置：遵循 config.toml 的 [ui] collapsed_edit_blocks
                                      # （标志开 = 折叠单行）；取消注释以固定任一形状
dual_line_numbers = false             # 双列行号（旧 + 新）
# line_summary = false                # 在折叠标题中显示 +N/-M；未设置遵循同一标志
hunk_separator = "…"                  # diff hunk 之间的分隔符（默认："…"）

[scrollback.blocks.prompt]
vpad = true                           # 垂直内边距
show_prefix = true                    # 显示提示前缀字符
min_lines = 2                         # 粘性模式下最少内容行数

[scrollback.blocks.thinking]
animate = true                        # 思考时动画强调
truncated_lines = 3                   # 截断模式下的行数
```

### Todo

```toml
[todo]
badge_format = "default"              # "default"、"colon" 或 "comma"
```

徽章格式示例：

- `default`：`2/5` —— `done/total` 进度分数（done = 已完成，total = 除已取消外的全部任务）。
- `colon`：`[>:1 [ ]:4 ok:3 x:2]` —— 图标:计数。
- `comma`：`[1 >, 4 [ ], 3 ok, 2 x]` —— 计数 图标，逗号分隔。

### 插件

```toml
disable_plugins = false               # 完全隐藏 hooks/plugins UI
```

---

## 环境变量

关键项如下。完整列表见 README。

### 认证

| 变量 | 说明 |
|------|------|
| `XAI_API_KEY` | 来自 console.x.ai 的 API key |
| `GROK_AUTH_PROVIDER_COMMAND` | 外部认证二进制路径 |
| `GROK_AUTH_PROVIDER_LABEL` | TUI 登录屏显示名 |
| `GROK_AUTH_TOKEN_TTL` | 令牌生命周期（秒） |
| `GROK_AUTH_EARLY_INVALIDATION_SECS` | 过期前多少秒刷新（默认：300） |
| `GROK_OIDC_ISSUER` | OIDC issuer URL |
| `GROK_OIDC_CLIENT_ID` | OIDC client ID |

### 端点

| 变量 | 说明 |
|------|------|
| `GROK_CLI_CHAT_PROXY_BASE_URL` | 覆盖 API 代理基 URL |

### 功能

| 变量 | 说明 |
|------|------|
| `GROK_MEMORY` | 启用（`1`）或禁用（`0`）跨会话记忆 |
| `GROK_SUBAGENTS` | 启用（`1`）或禁用（`0`）子智能体 |
| `GROK_WORKFLOWS` | 启用（`1`）或禁用（`0`）后台工作流并选择 `/goal` 驱动（默认关：遗留 `update_goal`；开：宿主拥有的工作流驱动） |
| `GROK_WEB_FETCH` | 启用（`1`）或禁用（`0`）web_fetch 工具 |
| `GROK_WEB_FETCH_ALLOW_LOCAL` | 允许 `web_fetch` 仅访问显式环回主机（`localhost` / `127.0.0.0/8` / `::1`）。等同 `[toolset.web_fetch] allow_local`。默认关；私有/元数据仍阻止。 |
| `GROK_AGENT` | 自定义智能体定义路径或名称 |
| `GROK_SANDBOX` | 沙箱配置（off、workspace、devbox、read-only、strict；或自定义配置名） |

### 日志

| 变量 | 说明 |
|------|------|
| `GROK_LOG_FILE` | 将日志写入此文件路径（原样作为路径） |
| `RUST_LOG` | 日志级别过滤（如 `debug`）；控制 `GROK_LOG_FILE` 日志与无头 stderr 输出 |

### 路径

| 变量 | 说明 |
|------|------|
| `GROK_HOME` | 覆盖配置目录（默认：`~/.grok`） |
| `GROK_RESPECT_GITIGNORE` | 强制 gitignore 过滤开（`1`）或关（`0`）；覆盖 `[tools] respect_gitignore` |

### 遥测

| 变量 | 说明 |
|------|------|
| `GROK_TELEMETRY_ENABLED` | 启用/禁用遥测 |
| `GROK_TELEMETRY_TRACE_UPLOAD` | 启用/禁用会话轨迹上传 |
| `GROK_TELEMETRY_MIXPANEL_ENABLED` | 专门启用/禁用 Mixpanel |
| `GROK_EXTERNAL_OTEL` | 到你收集器的外部 OTEL（见 [24-monitoring-usage.md](24-monitoring-usage.md)） |
| `GROK_FEEDBACK_ENABLED` | 启用/禁用反馈系统 |
| `GROK_DEPLOYMENT_KEY` | 企业用管理 API key |

---

## 文件位置

| 路径 | 说明 |
|------|------|
| `~/.grok/config.toml` | 主配置文件 |
| `~/.grok/pager.toml` | TUI 外观配置 |
| `~/.grok/auth.json` | 认证凭据（自动管理） |
| `~/.grok/sessions/` | 持久化会话（按工作目录组织） |
| `~/.grok/memory/` | 跨会话记忆文件与索引 |
| `~/.grok/skills/` | 用户作用域技能定义 |
| `~/.grok/plugins/` | 用户作用域插件 |
| `~/.grok/agents/` | 用户作用域智能体定义 |
| `~/.grok/lsp.json` | LSP 服务器配置（用户作用域） |
| `~/.grok/logs/` | 内部日志文件（如 `unified.jsonl`、MCP 服务器日志） |
| `.grok/config.toml` | 项目作用域 MCP 服务器、插件与权限规则 |
| `.grok/skills/` | 项目作用域技能定义 |
| `.grok/plugins/` | 项目作用域插件 |
| `.grok/agents/` | 项目作用域智能体定义 |
| `.grok/hooks/` | 项目作用域 hooks |
| `.grok/lsp.json` | LSP 服务器配置 |

---

## 项目作用域配置

通过在仓库内 `.grok/` 放置文件，可按项目设置部分选项：

| 文件 | 配置内容 |
|------|----------|
| `.grok/config.toml` | MCP 服务器、插件、权限规则，以及 `[mcp] max_output_bytes` 工具结果上限（其他节仅从 `~/.grok/config.toml` 加载） |
| `.grok/skills/` | 项目特定技能 |
| `.grok/hooks/` | 项目特定生命周期 hooks |
| `.grok/agents/` | 项目特定智能体定义 |
| `.grok/lsp.json` | LSP 服务器配置 |
| `.grok/sandbox.toml` | 自定义沙箱配置 |
| `AGENTS.md` | 项目指令（系统提示） |

项目作用域 MCP 服务器覆盖同名全局服务器（完全替换，非合并）。

---

## LSP 服务器

语言服务器为被动诊断与可选 `lsp` 工具提供能力（见 [`lsp_tools`](#常规设置) 功能标志）。定义来自三个来源，按服务器名合并：

| 来源 | 位置 | 作用域 |
|------|------|--------|
| 用户 | `~/.grok/lsp.json` | 所有项目 |
| 项目 | `.grok/lsp.json` | 当前仓库 |
| 插件 | 受信任插件的 `.lsp.json` 文件，或其 `plugin.json` 中的内联 `lspServers` 块 | 插件启用处 |

同名服务器多源时，按优先级从高到低：

1. **项目** —— `.grok/lsp.json`
2. **用户** —— `~/.grok/lsp.json`
3. **插件** —— 基于文件的 `.lsp.json`，然后内联 `lspServers`，按插件加载顺序

项目与用户条目替换同名的较低优先级项。插件条目仅添加本地文件尚未定义名称的服务器，因此本地 `lsp.json` 始终优先于插件。插件 LSP 服务器仅在插件受信任后加载（见 [插件](09-plugins.md)）。
