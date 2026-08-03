# 权限与安全控制

Grok 可以读取文件、搜索代码、编辑文件并运行 shell 命令。权限系统控制代理被允许执行哪些操作。你可以组合多层彼此独立的机制：权限规则、权限模式、钩子（hooks），以及操作系统级沙箱。

本指南说明工具调用如何被授权、如何通过 CLI、原生配置或 Claude 设置配置权限规则，以及如何使用 `PreToolUse` 钩子实现在所有模式下都生效的允许列表。

---

## 工具调用如何被授权

当模型请求使用某个工具时，会按以下顺序进行检查：

1. **`PreToolUse` 钩子**。钩子可以在其他任何检查之前拒绝一次工具调用。钩子若“允许”某次调用，并不会跳过后续检查；它只是选择不拒绝。参见 [10-hooks.md](10-hooks.md)。

2. **权限规则**（来自配置文件或 `--allow`/`--deny` 标志）
   - 匹配的 `deny` 规则会拒绝该调用。`deny` 优先于其他所有规则。
   - 匹配的 `ask` 规则会向你发起确认提示，包括对原本会自动批准的文件读取、搜索和 shell 命令。
   - 匹配的 `allow` 规则会批准该调用。

3. **已记住的授权**。你在此前提示中保存的按命令授权会在此生效，作用域为当前项目。已有授权可以满足 `ask` 规则，从而不必再次提示。[危险命令列表](#危险命令)上的命令会再次提示，而不会使用已记住的前缀。参见 [交互式审批](#交互式审批及其持久化位置)。

4. **内置自动批准**。只读工具以及一组固定的只读 shell 命令无需提示即可运行（见下文）。

5. **提示策略**（由[权限模式](#权限模式)设定）：向你发起提示、自动批准，或自动拒绝该调用。

始终批准模式（`bypassPermissions`）会在步骤 2 之后短路此流水线：`deny` 规则、钩子，以及匹配 shell 命令各段的 `ask` 规则仍然生效，但不会查阅已记住的授权（包括已记住的“永不允许”条目），且对非 shell 工具的 `ask` 规则不会发起提示。

---

## 默认从不提示的操作

下列操作被视为只读，并在所有模式（包括 `dontAsk`）下无需提示即可运行，除非有匹配的 `deny` 规则或钩子将其拦截。`ask` 规则会强制对文件读取、搜索和 shell 命令发起提示（参见[工具调用如何被授权](#工具调用如何被授权)）。

### 只读工具

- `read_file`
- `list_dir`
- `grep`（内容搜索）
- `web_search`
- `todo_write`
- `get_command_or_subagent_output` / `wait_commands_or_subagents` / `kill_command_or_subagent`（子代理控制）
- 调用 skills

### 只读 Shell 命令

在按链式分隔符拆分命令（`&&`、`||`、`;` 以及管道）之后，下列命令若作为主命令出现，会被识别为只读。该列表按词边界匹配，因此 `ls` 不会匹配 `lsof` 或 `less`。（你自己的 `Bash(...)` 规则匹配方式不同；参见[规则匹配参考](#规则匹配参考)。）

**文件系统（只读查看）：**
- `ls`、`cat`、`pwd`、`date`、`whoami`、`hostname`、`uptime`、`ps`
- `head`、`tail`、`wc`、`sort`、`uniq`、`tr`、`cut`

**Git（只读）：**
- `git status`、`git branch`、`git log`、`git diff`、`git ls-files`、`git show`、`git rev-parse`

**搜索与检查：**
- `grep`、`rg`（不包括 `rg --pre` / `rg --pre=…`，它们会为每个文件启动预处理器）

**Kubernetes（只读）：**
- `kubectl get`、`kubectl logs`、`kubectl describe`

> **说明：** `tee` 不在此列表中，因为它可将其输入写入任意文件。`cargo check` 不在此列表中，因为它会编译并运行仓库中的 `build.rs`、过程宏以及任何 `build.rustc-wrapper`（因此在 Ask 模式下会提示；Auto 模式仍可能启发式地允许将 `cargo` 作为项目代码运行器）。`sort --compress-program=…`（包括唯一的长选项缩写）、`git -c` / `--config-env` 覆盖，以及本地/工作树配置安装了可执行钩子的 git 命令（`core.fsmonitor`、`diff.*.command`/`textconv`/`external` 驱动，或 shell 形式的 `alias.<safe-subcommand> = !…`）会提高请求级别的底线并改为提示而非自动批准，除非用户已授权该完整脚本原文，或已开启 YOLO。

这些检查按段（segment）生效。在如 `ls && rm -rf /` 这样的命令中，`ls` 段会被识别为只读，但 `rm` 段不在列表中。在 `default` 模式下，`rm` 段会提示；在 `dontAsk` 下则会被拒绝。

---

## 权限模式

提示策略由下列模式之一命名：

| 模式                | 行为                                                                 | 典型用途                     |
|---------------------|--------------------------------------------------------------------------|---------------------------------|
| `default`           | 对未预先批准的操作发起提示                                     | 日常交互使用           |
| `dontAsk`           | 拒绝没有显式 allow 规则或内置自动批准的操作   | 无头模式、CI、高安全场景     |
| `bypassPermissions` | 自动批准工具调用（`deny` 规则、钩子以及 shell 的 `ask` 规则仍生效） | 受信任环境    |
| `acceptEdits`       | 自动批准文件编辑（`search_replace`、`write` 等）                | “接受编辑”工作流        |
| `plan`              | 为兼容性而接受；计划会话是独立功能（参见 [19-plan-mode.md](19-plan-mode.md)） | 结构化计划会话 |

### 设置模式

模式由 `.claude/settings.json` 中的 `defaultMode` 设定（参见 [Claude Code 兼容性](#3-claude-code-兼容性-claudesettingsjson)）。`dontAsk`、`acceptEdits` 和 `bypassPermissions` 会从该处改变提示策略；`default` 和 `plan` 则保持标准提示行为。

`--permission-mode` CLI 标志可应用 `bypassPermissions`（始终批准）和 `default`；显式标志值始终优先于配置中设定的模式。向该标志传入 `dontAsk`、`acceptEdits` 或 `plan` 会被接受，但不会启用对应策略；请改为通过 `defaultMode` 设置。

在无头运行（`-p`）中，本会提示的工具调用会被取消，并报告给模型，而不是等待输入。若要在自动化中默认拒绝，请设置 `defaultMode: "dontAsk"`。

### 禁用始终批准模式

管理员可以关闭始终批准（`bypassPermissions` / `--always-approve`），使其无法从 CLI、TUI 开关或 `/always-approve` 命令启用。在 `requirements.toml` 中设置专用键：

```toml
[ui]
disable_bypass_permissions_mode = true   # default: false. true = locked off.
```

不要用 `permission_mode` 做这件事；它是用户可切换的默认值，不是锁定。`requirements.toml` 中的旧键 `[ui] yolo = false` 也会禁用该模式，以保持向后兼容；在 `config.toml` 中，同一键仍是可切换的偏好设置。

用户级 `~/.grok/requirements.toml` 由用户控制，因此开发者可以通过编辑该文件解除锁定。若要实施用户无法覆盖的强制策略，请将该设置部署到 root 拥有的系统文件 `/etc/grok/requirements.toml`。

> **说明：** Grok 会遵循 Claude Code 的 `managed-settings.json` 中的权限规则，但不会遵循其 `disableBypassPermissionsMode` 锁定。要在 Grok 中禁用始终批准，请按上文使用 `requirements.toml`。

---

## 配置权限

Grok 从三个兼容来源读取权限规则。所有来源的规则合并为一套；规则的效果取决于其动作（`deny` > `ask` > `allow`），而非来自哪个文件。

### 权限规则存放位置（作用域）

权限规则可以是全局的（所有项目）、项目作用域的（单个仓库），或在项目内仅对你个人生效：

| 作用域 | 文件 | 是否与队友共享 |
|-------|------|-----------------------|
| 全局（所有项目） | `~/.grok/config.toml` | 否 |
| 项目（可提交） | `<project>/.grok/config.toml` | 是（提交它） |
| 项目（个人） | `<project>/.claude/settings.local.json` | 否（将其 gitignore） |
| 交互式授权 | 由 Grok 按项目内部存储 | 否 |

关于作用域的说明：

- Grok 会从仓库根目录一直到你的工作目录，在每一层目录发现 `.grok/config.toml`，因此子目录可以在仓库根规则之上追加规则。
- 所有作用域的规则合并为一套规则集；跨作用域仍适用 `deny` > `ask` > `allow`，因此全局 `deny` 不能被项目级 `allow` 覆盖。
- Grok 没有原生的 `config.local.toml`。若要在项目中使用个人的、不提交的规则，请使用 `.claude/settings.local.json`；Grok 会直接读取它（参见 [Claude Code 兼容性](#3-claude-code-兼容性-claudesettingsjson)）。
- 交互式“始终允许”决策存储在仓库之外，并按项目作用域隔离（参见 [交互式审批](#交互式审批及其持久化位置)）。

若要在某个项目中停止对特定命令的提示，请向该项目的 `.grok/config.toml`（或 `.claude/settings.json`）添加一条窄范围的 allow 规则：

```toml
[permission]
allow = ["Bash(cargo test *)", "Bash(npm run build)"]
```

这只会批准列出的命令。相比之下，始终批准模式会批准所有工具调用。

### 1. CLI 标志

```bash
grok -p "Review the API changes" \
  --allow 'Bash(git *)' \
  --allow 'Bash(gh *)' \
  --allow 'Read' \
  --allow 'Grep' \
  --deny 'Bash(rm -rf *)'
```

`--allow RULE` 和 `--deny RULE` 可以重复使用，并始终被强制执行。

规则语法示例：
- `Bash(git *)` — 任何以 `git ` 开头的命令
- `Bash(npm run build)` — 精确命令（或前缀）
- `Bash(git commit:*)` — `cmd:*` 后缀形式，等价于对 `git commit` 做前缀匹配
- `Read(src/**)` — `src/` 下的读取访问
- `Edit(**/*.rs)` — 编辑任意 Rust 文件
- `Grep` — 所有 grep 操作
- `MCPTool(my-server__*)` — 来自特定服务器的 MCP 工具

精确匹配语义（包括链式命令与通配符如何求值）见 [规则匹配参考](#规则匹配参考)。

### 2. 原生配置（`~/.grok/config.toml` 与 `.grok/config.toml`）

```toml
[permission]
rules = [
  { action = "allow", tool = "bash", pattern = "git *" },
  { action = "allow", tool = "bash", pattern = "gh *" },
  { action = "allow", tool = "read" },
  { action = "allow", tool = "grep" },
  { action = "deny",  tool = "bash", pattern = "rm -rf *" },  # block a dangerous pattern
  { action = "ask",   tool = "edit" },
]
```

结构化 `tool` 字段接受小写名称 `bash`、`read`、`edit`、`grep`、`mcp`、`webfetch` 和 `websearch`，对应[工具名称](#工具名称)中的工具类别。

由于 `deny` 始终优先，你不能将这些 `allow` 规则与针对 `bash` 的兜底 `deny` 组合来表达“只允许 git/gh”；`deny tool = "bash"` 规则也会拦截 `git` 和 `gh`。若要默认拒绝，请在 `.claude/settings.json` 中使用 `defaultMode: "dontAsk"`，或使用 `PreToolUse` 钩子（见下文）。

来自全局 `~/.grok/config.toml` 以及每个项目 `.grok/config.toml`（从仓库根到工作目录）的规则会合并为一套规则集，并与任何 `.claude/settings.json` 规则一并生效。

由组织部署的受管配置也会贡献 `[permission]` 规则：系统级 `/etc/grok/managed_config.toml`，以及 Grok 自动维护的用户级副本 `~/.grok/managed_config.toml`。受管规则与其他来源的规则一样合并，但对受管 `allow` 规则有两点特有性质：你自己的 `deny` 和 `ask` 规则优先于受管 `allow`（按严重性排序），且在始终批准被锁定关闭时，兜底的受管 `allow` 会被忽略。对于用户无法编辑掉的规则，请使用 root 拥有的系统文件 `/etc/grok/requirements.toml`。

所有来源的权限规则在会话启动时读取一次。更改在下一会话生效。

原生 `[permission]` 节也接受紧凑的 `allow` / `deny` / `ask` 字符串数组形式，使用与 `--allow` / `--deny` 标志和 `.claude/settings.json` 相同的规则字符串：

```toml
[permission]
deny = [
  "Read(/Users/you/private/**)",
  "Edit(/Users/you/private/**)",
  "Bash(rm -rf *)",
]
allow = [
  "Bash(git *)",
  "Bash(gh *)",
]
```

`deny` 始终优先于 `allow`（求值为 `deny` > `ask` > `allow`），与顺序或来源无关。若还要在操作系统层面阻止读取项目外路径，请将 deny 规则与 `strict` 沙箱配置文件结合使用（参见 [18-sandbox.md](18-sandbox.md)）。

### 3. Claude Code 兼容性（`.claude/settings.json`）

Grok 会读取 `~/.claude/settings.json` 和 `~/.claude/settings.local.json`，以及项目级的 `<project>/.claude/settings.json` 与 `settings.local.json`（向上遍历到仓库根）。权限规则的原生 `.grok` 来源是上文所述的 `config.toml`。

示例：

```json
{
  "permissions": {
    "defaultMode": "dontAsk",
    "allow": [
      "Read",
      "Grep",
      "Bash(git *)",
      "Bash(gh *)"
    ],
    "deny": [
      "Bash(rm -rf *)"
    ]
  }
}
```

支持的 `defaultMode` 值为 `default`、`acceptEdits`、`bypassPermissions`、`dontAsk` 和 `plan`。Grok 从其规范位置 `permissions` 下读取 `defaultMode`；当嵌套键不存在时，也接受顶层的 `defaultMode`。

`permissions.allow`、`permissions.deny` 和 `permissions.ask` 条目会被翻译为原生规则，再按[规则匹配参考](#规则匹配参考)中的语义进行匹配。翻译说明：

- MCP 工具的规则必须使用 `MCPTool(server__tool)` 形式；`mcp__server__tool` 形式永远不会匹配（参见 [MCP 规则](#mcp-规则)）。
- 命名了无法识别的工具的规则，以及如 `Agent(model:opus)` 这类参数规则，会以警告跳过，而不是导致加载失败。
- `permissions.additionalDirectories` 会被解析，但不支持。

你可以使用 **Ctrl+I**（“Import Claude settings”）以交互方式导入已有的 Claude 设置。

---

## 规则匹配参考

本节精确定义规则如何匹配。

### Bash 规则

`Bash(...)` 模式通过以下两种方式之一匹配命令：

- **前缀**：命令以模式文本开头，按字符逐一比较。没有词边界要求，因此 `Bash(git)` 会匹配 `gitleaks` 以及 `git status`。加上尾随空格与通配符（`Bash(git *)`）可要求前缀为完整单词。
- **Glob**：模式将整个命令当作 glob 匹配。`*` 可出现在任意位置，并匹配任意字符（包括空格与斜杠），因此 `Bash(git * main)` 会匹配 `git checkout main`。也支持 `?` 与 `[...]`。

匹配区分大小写。匹配前会去除命令的前导空白；除此之外不做规范化。

Bash 规则末尾的 `:*` 后缀会被剥离为普通前缀：`Bash(git commit:*)` 变为前缀 `git commit`。由于前缀没有词边界，写成 `Bash(sed:*)` 的 `deny` 也会拦截诸如 `sed-custom` 的命令。

**链式命令。** Grok 会像 shell 一样解析每条命令，并按 `&&`、`||`、`;`、`|` 以及换行拆分。规则动作对段的处理不同：

- `deny` 与 `ask` 规则会对每一段以及完整字符串进行检查。只要有一段被拒绝，整条命令就会被拒绝。
- `allow` 规则仅对完整命令字符串检查。因此 `Bash(git *)` 会自动批准 `git status && rm -rf /`，因为完整字符串以 `git ` 开头。请将窄范围 allow 规则与你想拦截的模式的 `deny` 规则配对使用。

无法拆分为简单段的命令（子 shell、命令替换 `$(...)`、反引号、后台 `&`、控制流）在配置了 Bash 限制时，会作为整体发起提示。

段级检查（`deny` 与 `ask` 规则、已记住的授权，以及只读命令列表）会剥离环境变量前缀（如 `RUST_LOG=debug`），并剥离一组固定的进程包装器（`timeout`、`nice`、`ionice`、`chrt`、`stdbuf`、`env`），从而使 `deny` 与 `ask` 规则既能匹配包装后的形式，也能匹配内部命令。对传给 `bash -c` 的内联脚本，`deny` 与 `ask` 规则也会在内部检查。其他包装器（包括 `sudo`、`xargs` 和 `nohup`）不会被剥离；请显式编写包含它们的规则。`allow` 规则不享受此处理：它们按原样匹配命令字符串，因此前导环境赋值或包装器会使 `allow` 规则无法匹配，命令改为发起提示。

### 危险命令

内置列表（`rm`、`chmod`、`chown`、`chgrp`、`chattr`、`pkill`、`kill`、`killall`、`git push`）即使某段已被已记住的命令前缀或只读命令列表覆盖，仍会发起提示。配置中的显式 `allow` 规则会批准它们，始终批准模式也会像对待其他命令一样自动批准它们；请使用 `deny` 规则无条件拦截它们。在将如 `Bash(rm *)` 的规则添加为 allow 规则之前，请仔细审查。

### Read、Edit 与 Grep 规则

路径模式是针对工具调用时传入的路径字符串进行的 glob 匹配：

- `*` 与 `?` 不会跨越 `/`；`**` 会。`Read(src/*)` 匹配 `src/main.rs` 但不匹配 `src/nested/mod.rs`；对整棵树使用 `Read(src/**)`。
- 裸文件名只匹配该精确字符串。使用 `**/.env` 可匹配任意深度的 `.env`。
- 没有锚定前缀：模式中的前导 `//` 或 `~/` 被当作字面 glob 文本。请改写为绝对路径模式或 `**/` 模式。
- 路径按给定形式匹配，不做规范化。路径是绝对还是相对取决于工具如何被调用，因此作为边界的模式应同时覆盖两种形式（例如同时覆盖 `/repo/secrets/**` 与 `secrets/**`）。
- `Read` 规则也管辖 `grep` 搜索；`Grep(...)` 规则仅匹配 grep。

`Read` 与 `Edit` 的 deny 规则还会应用于 shell 命令触及的文件路径（例如对已拒绝路径执行 `cat` 或 `sed`），包括传给 `bash`、`sh`、`dash`、`zsh` 或 `ksh` 且带 `-c` 的字面内联脚本；该 shell 级检查还会解析符号链接。直接的 `read_file`/`search_replace` 工具检查不解析符号链接。若要获得覆盖每个进程的操作系统级强制执行，请将 deny 规则与沙箱结合使用（[18-sandbox.md](18-sandbox.md)）。

### MCP 规则

`MCPTool(...)` 模式匹配完整的 Grok 工具名称（`server__tool` 形式），并支持 glob：`MCPTool(linear__*)` 匹配 `linear` 服务器上的每个工具。Grok 工具名称不带 `mcp__` 前缀，因此写成 `mcp__server__tool` 的规则永远不会匹配 MCP 调用；请改为写 `MCPTool(server__tool)`。

### WebFetch 规则

- `WebFetch(domain:example.com)` 匹配该主机及其所有子域（`api.example.com`），不区分大小写，并忽略前导 `www.`。`domain:` 模式内部不支持通配符。
- 不带 `domain:` 前缀的模式会对整个 URL 做 glob：`WebFetch(https://api.example.com/*)`。

### 工具名称

已识别的工具名称：`Bash`、`Read`（以及 `NotebookRead`）、`Edit`（以及 `Write`、`NotebookEdit`）、`Grep`（以及 `Glob`）、`MCPTool`、`WebFetch`、`WebSearch`。裸 `*` 规则匹配所有工具。工具名称位置不支持 glob。

命名了无法识别的工具的规则（例如 `Agent(model:opus)`）会以警告跳过，而不是导致加载失败。

### 求值顺序

所有来源的规则合并为一套，并按严重性而非顺序求值：任何匹配的 `deny` 都会拒绝；否则任何匹配的 `ask` 都会提示；否则任何匹配的 `allow` 都会批准。当没有任何规则匹配时，请求会落入内置自动批准，再落入提示策略，如[工具调用如何被授权](#工具调用如何被授权)所述。

---

## 交互式审批及其持久化位置

当工具调用需要审批时，权限提示提供下列选择：

- **Allow once**：仅批准这一次调用。
- **Reject once**：拒绝它，可选附带一条返回给模型的消息。
- **Enable always-approve mode**：批准之后所有工具调用，而不仅是当前被提示的那一次。
- **Allow all edits this session**：在文件编辑时显示。该授权仅保存在内存中，重启后不保留。

### 按命令的“始终允许”

一组更窄的选项只会记住正在被提示的特定命令、MCP 工具或 web-fetch 域，例如“Always allow `cargo test`”。这些选项默认关闭。通过以下方式启用：

```toml
# ~/.grok/config.toml
[ui]
remember_tool_approvals = true
```

启用该开关后，提示会增加：

- **`Always allow: <command>`**，会为命令前缀持久化一条 allow。
- 对应的 “never allow” 行，以同样方式持久化一条 deny。
- 针对 MCP 工具与 web-fetch 域的等效 “always allow” 行。

已记住的前缀仅限于命令的简短形式：只读命令仅持久化其列出的前缀（例如 `git status`，而非完整参数列表），其他命令则持久化一段简短的前导前缀。确认前，提示会精确显示将要记住的内容。[危险命令列表](#危险命令)上的命令会再次提示，而不会使用已记住的前缀。

### 持久化按项目隔离

交互式授权存储在你主目录下 Grok 自己的状态目录中，作用域为你启动 Grok 时所在的目录。在一个项目中做出的授权绝不会应用于另一个项目，授权不会写入仓库，也不打算手工编辑。

交互式授权是个人的、按机器的状态。若要一份可在代码审查中审阅并与队友共享的允许列表，请改为在项目的 `.grok/config.toml` 中使用声明式规则。

---

## 用钩子将 Bash 限制为特定命令

`PreToolUse` 钩子可对 `Bash` 工具强制执行允许列表，并在所有权限模式下生效。钩子在权限系统之前求值；钩子拒绝会停止调用，钩子允许则会落入正常权限检查（因此你的 `deny` 规则仍然生效）。

> **说明：** 钩子采用失败放行（fail open）。若钩子脚本崩溃、超时或缺失，工具调用会继续，如同钩子已允许它，失败会在 UI 中报告。用作安全边界的钩子必须自行处理错误，并必须考虑链式命令，如下例所示。参见 [10-hooks.md](10-hooks.md)。

### 示例：仅允许 `git` 与 `gh`

**`~/.grok/hooks/git-gh-only.json`**

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "git-gh-only.sh",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

**`~/.grok/hooks/git-gh-only.sh`**

```bash
#!/bin/sh
# Allow only git and gh commands, including within chained commands.

set -eu

deny() {
  echo '{"decision": "deny", "reason": "'"$1"'"}'
  exit 2
}

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.toolInput.command // empty')

[ -n "$CMD" ] || deny "Empty command is not allowed"

# Normalize '&&' and '||' to ';' so chains can be checked segment by
# segment, then reject constructs this script cannot inspect.
CMD=$(echo "$CMD" | sed 's/&&/;/g; s/||/;/g')
case "$CMD" in
  *'$('*|*'`'*|*'&'*|*'>'*|*'<'*) deny "Substitution, background, and redirection are not permitted" ;;
esac

# Split on the separators and require every segment to start with git or gh.
echo "$CMD" | tr ';|' '\n\n' | while IFS= read -r SEGMENT; do
  SEGMENT=$(echo "$SEGMENT" | sed 's/^[[:space:]]*//')
  [ -n "$SEGMENT" ] || continue
  case "$SEGMENT" in
    git\ *|git|gh\ *|gh) ;;
    *) deny "Only git and gh commands are permitted. Blocked segment: $SEGMENT" ;;
  esac
done
```

```bash
chmod +x ~/.grok/hooks/git-gh-only.sh
```

该钩子会拒绝每一条 `Bash` 命令，除非链式命令中的每一段都以 `git` 或 `gh` 开头，并直接拒绝命令替换、后台运行与重定向，因为它无法验证它们会执行什么。它在所有权限模式下都有效。

关于钩子安装、JSON 格式、项目钩子的信任模型以及其他事件，参见 [10-hooks.md](10-hooks.md)，其中还包含一个互补的“拦截危险模式”示例。

---

## 配置示例

### 无头模式仅允许 git 与 gh（CI 与自动化）

```bash
grok -p "Implement the feature using only git and GitHub CLI" \
  --allow 'Read' \
  --allow 'Grep' \
  --allow 'Bash(git *)' \
  --allow 'Bash(gh *)'
```

安装上文的 `git-gh-only` 钩子，以拒绝所有其他 `Bash` 命令。若要对所有工具默认拒绝，还请在 `.claude/settings.json` 中设置 `{"permissions": {"defaultMode": "dontAsk"}}`。

### 只读代码审查器

```toml
# .grok/config.toml
[permission]
rules = [
  { action = "allow", tool = "read" },
  { action = "allow", tool = "grep" },
  { action = "deny",  tool = "edit" },
  { action = "deny",  tool = "bash" },
]
```

### 交互式开发

使用 `default` 模式，并为你最常运行的命令（`git`、`cargo test`、`rg` 等）添加窄范围的 `Bash(...)` allow 规则。

---

## 与沙箱结合使用

权限控制模型被允许请求什么。操作系统级沙箱（参见 [18-sandbox.md](18-sandbox.md)）控制即使命令已被批准后进程仍能做什么。

对不受信任代码的推荐组合：

1. `dontAsk` 加上窄范围 allow 规则，或限制性钩子
2. `--sandbox strict` 或自定义配置文件
3. 项目信任，并审查任何 `SessionStart` 钩子

---

## 在 TUI 中管理权限

- 权限决策会出现在会话记录（transcript）中。
- `/always-approve` 命令切换始终批准模式；其他模式通过 `defaultMode` 设置（参见[设置模式](#设置模式)）。
- 当 `[ui] remember_tool_approvals = true` 时，权限提示会包含按命令的 “Always allow” 选项，且仅对当前项目持久化。参见 [交互式审批](#交互式审批及其持久化位置)。
- 要管理钩子与插件，请运行 `/hooks` 或 `/plugins`（在大多数终端上，**Ctrl+L** 也会打开扩展模态框；在 VS Code、Cursor、Windsurf 和 Zed 中，`Ctrl+L` 则是中途插入输入）。参见 [10-hooks.md](10-hooks.md)。

---

## 最佳实践

1. **优先使用窄范围模式。** `Bash(git *)` 授予的访问少于裸的 `Bash` allow 规则。
2. **组合多层机制。** `dontAsk`、窄范围 allow 规则、限制性钩子与沙箱各自独立地施加限制。
3. **审查来自不熟悉来源的项目配置。** `.grok/config.toml` 与 `.claude/settings.json` 中的项目权限规则（包括 `allow` 规则）会在没有单独信任提示的情况下生效。在不熟悉的检出中工作之前，请审查它们以及任何项目钩子（参见 [10-hooks.md](10-hooks.md) 中的安全说明）。
4. **测试你的策略。** 在设置了 `defaultMode: "dontAsk"`（或安装了你的 `PreToolUse` 钩子）后，运行代表性命令并确认哪些会被拦截。
5. **将只读命令列表视为便利功能，而非安全边界。**

---

## 另见

- [10-hooks.md](10-hooks.md) — 钩子编写指南
- [14-headless-mode.md](14-headless-mode.md) — 无头模式标志，包括与权限相关的标志
- [18-sandbox.md](18-sandbox.md) — 操作系统级隔离配置文件
- [05-configuration.md](05-configuration.md) — 原生 `config.toml` 结构
