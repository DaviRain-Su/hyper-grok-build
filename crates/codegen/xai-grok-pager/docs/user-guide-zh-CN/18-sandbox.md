# 沙箱模式

沙箱模式通过操作系统内核原语（Linux 上的 Landlock、macOS 上的 Seatbelt）限制 agent 进程及其派生命令对文件系统和网络的访问。内核在整个进程生命周期内强制执行这些限制。

沙箱模式默认关闭。

---

## 快速开始

```bash
# Run with workspace sandbox (read everywhere, write to CWD + temp dirs + ~/.grok/)
grok --sandbox workspace

# Read-only mode (read everywhere, write only to ~/.grok/ + temp dirs)
grok --sandbox read-only

# Most restrictive profile (read CWD + system paths, write CWD + temp dirs + ~/.grok/, no child network)
grok --sandbox strict
```

---

## 内置配置档

| 配置档                | 文件系统读          | 文件系统写                                      | 子进程网络     | 适用场景                          |
| --------------------- | ------------------ | ---------------------------------------------- | ------------- | --------------------------------- |
| `off`（默认）         | 无限制             | 无限制                                         | 无限制        | 不使用沙箱                        |
| `workspace`           | 任意位置           | CWD + `~/.grok/` + `/tmp` + `/var/tmp`         | 允许          | 日常开发                          |
| `devbox`              | 任意位置           | 除 `/data` 外的所有顶层目录                    | 允许          | 一次性开发虚拟机                  |
| `read-only`           | 任意位置           | `~/.grok/` + `/tmp` + `/var/tmp`               | 阻止¹         | 探索、代码审查                    |
| `strict`              | CWD + 系统路径     | CWD + `~/.grok/` + `/tmp` + `/var/tmp`         | 阻止¹         | 不受信任的代码                    |

¹ 子进程网络阻止**仅在 Linux 上**强制执行（通过 seccomp）。在 macOS 上为 no-op——这些配置档在该平台上不会限制子进程网络。

若要在配置档之上再屏蔽特定文件（例如 `.env` 或凭据路径），请定义带 `deny` 列表的[自定义配置档](#custom-profiles)——由内核强制执行（读 + 写/重命名），并支持如 `**/*.pem` 的 glob 模式。

### 配置档详情

**workspace** —— 日常开发的推荐配置档。agent 可以读取系统上的任意文件（用于理解依赖、系统库等），但只能写入当前工作目录、`~/.grok/` 以及临时目录（`/tmp`、`/var/tmp`，外加 macOS 临时目录）。允许网络访问，以便使用 `web_search` 和 MCP 服务器等工具。

**devbox** —— 为一次性开发虚拟机预留的内置配置档。agent 可在任意位置读取，并可写入除 `/data` 和虚拟文件系统（`/proc`、`/sys`、`/dev`）以外的每个顶层目录，包括 home 目录。允许网络访问。`--sandbox devbox` 始终运行内置配置档，会遮蔽你在 `sandbox.toml` 中定义的任何 `[profiles.devbox]`。

**read-only** —— 希望 agent 分析代码但不修改项目文件时使用。agent 可读取一切，但只能写入 `~/.grok/`（会话持久化所需）和临时目录。子进程网络访问在 Linux 上被阻止（在 macOS 上为 no-op）。

**strict** —— 最严格的配置档，用于审查不受信任的代码。agent 只能读取当前工作目录内的文件以及必要的系统路径。写入仅限于 CWD、`~/.grok/` 和临时目录。子进程网络访问在 Linux 上被阻止（在 macOS 上为 no-op）。

---

## 自定义配置档

在 `~/.grok/sandbox.toml`（全局）或 `.grok/sandbox.toml`（按项目）中创建自定义沙箱配置档：

```toml
[profiles.project]
# Start from a built-in profile, then add overrides
extends = "workspace"
restrict_network = true

# Paths the agent can read but NOT write/delete
read_only = ["/data"]

# Additional writable paths
read_write = ["/tmp/scratch"]

# Paths or globs to kernel-deny (read + write/rename, enforced; see notes below)
deny = ["/data/shared-secrets", "**/.env", "**/*.pem"]
```

使用自定义配置档：

```bash
grok --sandbox project
```

自定义配置档不能复用内置名称。`--sandbox devbox` 始终运行内置 `devbox` 配置档，会遮蔽你定义的任何 `[profiles.devbox]`。

当全局文件与按项目文件定义了同名自定义配置档时，用户级定义优先，项目级定义被忽略。若两者定义不同，Grok 会在启动时就冲突发出警告——在 TUI 的欢迎界面上显示，无头模式下输出到 stderr。内容完全相同的重复定义不会产生警告。

### 自定义配置档字段

| 字段               | 类型     | 说明                                              |
| ------------------ | -------- | ---------------------------------------------------- |
| `extends`          | String   | 继承的基础内置配置档（`workspace`、`devbox`、`read-only`、`strict`）。省略时默认为 `workspace` |
| `restrict_network` | Boolean  | 阻止子进程的网络访问                             |
| `read_only`        | String[] | 额外的只读路径                                   |
| `read_write`       | String[] | 额外的可读写路径                                 |
| `deny`             | String[] | 由内核拒绝的路径或 glob（读 + 写/重命名；见说明）。含 `*`、`?` 或 `[` 的条目视为 glob |

> **关于 `deny` 的说明：** 非空的 `deny` 列表由**内核强制执行**。被拒绝的路径会通过 macOS 上的 Seatbelt 和 Linux 上的 bwrap bind-over 实现**读拒绝与写/重命名拒绝**，因此被拒绝的路径既不能被读取（经 `bash`、`grep` 或子 agent），也不能通过移出拒绝集合后在别处读取（`mv secret x && cat x` 这类绕过已被封堵）。在 **Linux** 上，读拒绝需要 `bubblewrap`：若其缺失（或任一 deny 路径无法绑定），Grok 会拒绝启动，而不是在被拒绝路径暴露的情况下运行（仅对 `/data` 做写拒绝的 `devbox` 仍会回退到 Landlock）。对**不在** `deny` 中的路径的写入，由你在 `read_write` 中授予的权限控制。

> **`deny` 中的 glob：** 若条目包含 `*`、`?` 或 `[`，则视为 **glob**。
> 这些字符**始终**表示 glob——若要拒绝名称中含有这些字符的字面文件，请改为指定其父目录。支持的、gitignore 风格的子集为：
>
> - `*` —— 单个路径段内的任意字符序列（在 `/` 处停止）
> - `?` —— 单个路径段内恰好一个字符
> - `**` —— 跨目录（作为完整路径段，例如 `**/`、`a/**`）；`**/`
>   也匹配零个目录，因此 `**/.env` 可匹配 `.env` 和 `sub/.env`
> - `[abc]` / `[a-z]` —— 字符类；前导 `!` **或** `^` 表示取反
>   （`[!a]` 与 `[^a]` 均表示“非 `a`”）
>
> 花括号交替（`{a,b}`）、反斜杠转义，以及不常见的类形式
> `[]…]`（字面 `]` 在前）和 POSIX `[[:…:]]` **不受支持**，因此两个
> 平台绝不会对同一 glob 产生不同解释。使用了不支持元字符或格式错误的
> glob 会让 Grok 在**两个**平台上都**拒绝启动**（失败即关闭）——请将 `*.pem` 和 `*.key` 写成独立条目，
> 而不是 `*.{pem,key}`。
>
> 相对 glob 锚定在 workspace；绝对 glob（例如
> `/home/**/.ssh`）锚定在其字面前缀。非 glob 条目保持精确路径
> 匹配。强制执行方式因平台而异：
>
> - **macOS 严密：** 每个 glob 会变成运行时应用的 Seatbelt 正则，
>   因此匹配文件即使在 Grok 启动**之后**创建也会被拒绝。
> - **Linux 尽力而为：** 挂载命名空间无法在运行时做 glob，因此每个
>   glob 会展开为**启动时已存在**的文件并对其做 bind-over。
>   之后创建且匹配 glob 的文件**不会**被覆盖——在 Linux 上若某路径必须严密保护，请写精确路径。匹配文件过多、
>   或目录树过深/过广而无法遍历的 glob，会让 Grok **拒绝启动**，而不是降级执行。

---

## 工作原理

沙箱在启动时通过内核原语应用于**整个 grok 进程**——而非逐命令包装。这意味着所有工具操作都受覆盖：

- `read_file`、`search_replace`、`list_dir` —— 由进程内的 Landlock/Seatbelt 限制
- `bash` 命令、`grep`（rg）—— 子进程自动继承文件系统限制
- 网络 —— 在 Linux 上可通过 seccomp 阻止子进程；在 macOS 上为 no-op

沙箱一旦应用即**不可逆**。agent 无法在运行时放宽限制。

---

## 恢复会话

会话启动时使用的配置档会随会话保存，并在**会话整个生命周期内固定**。当你恢复会话时（`grok --resume <id>`、`grok --continue` 或 `grok -r`），Grok 会自动还原同一配置档——因此以 `--sandbox workspace` 启动的会话不会在恢复时静默变成更严格的默认配置，从而破坏此前可正常工作的命令。

恢复**不会**更改会话的沙箱：

- 恢复时省略 `--sandbox` 会使用会话已保存的配置档。
- 传入与已保存配置档**相同**的 `--sandbox <profile>` 是允许的。
- 传入与已保存配置档**不同**的 `--sandbox <profile>` 会**以错误拒绝**——更改已恢复会话的沙箱是安全隐患（可能扩大本应受限的访问范围，或破坏依赖更广访问权限的会话）。若要使用不同配置档，请开启新会话。

**新**会话的配置档解析顺序：

1. 显式的 `--sandbox <profile>` 标志或 `GROK_SANDBOX` 环境变量
2. 配置中的 `[sandbox] profile`
3. `off`（无沙箱）

---

## 平台支持

| 平台     | 机制      | 最低版本               |
| -------- | --------- | ---------------------- |
| Linux    | Landlock  | 内核 5.13 或更高       |
| macOS    | Seatbelt  | macOS（所有版本）      |

若无法应用沙箱（例如内核不受支持、缺少 entitlement），Grok 会记录警告并在不强制执行的情况下继续。例外是显式请求的**自定义配置档**：在 **macOS 和 Linux 上**，若无法应用（未知配置档、格式错误的 `sandbox.toml`，或——在 Linux 上——非空 `deny` 时 `bubblewrap` 不可用），Grok 会拒绝启动，而不是在被拒绝路径暴露的情况下运行。

---

## 网络限制

在 Linux 上，带有 `restrict_network` 的配置档会通过 seccomp 阻止**子进程**（bash 命令、脚本）的网络访问。在 macOS 上，网络阻止为 no-op。在进程内发起 HTTP 请求的内置工具（网络搜索、LLM API 调用）永远不受影响——agent 需要网络访问才能正常工作。

在 Linux 上，实际效果是：

- `web_search`、`web_fetch` 和 LLM API 始终拥有网络访问
- 当启用 `restrict_network` 时，`curl`、`wget`、`npm install` 等 `bash` 命令会被阻止

---

## 事件日志

沙箱事件会记录到 `~/.grok/sandbox-events.jsonl`，便于调试。事件包括：

- 已应用的配置档（哪个配置档、时间戳）
- 违规（试图访问被拒绝的路径）

---

## 何时使用沙箱模式

**在以下情况使用 `workspace`：**

- 在自己的项目上工作，并希望获得基本的写保护
- 在共享环境中运行，希望限制变更范围

**在以下情况定义带 `deny` 列表的自定义配置档：**

- 需要在基础配置档之上屏蔽特定文件（例如 `.env` 或凭据路径）
- 需要覆盖 `bash`、`grep` 和子 agent 的内核强制执行——而不仅仅是 `read_file` 工具

**在以下情况使用 `read-only`：**

- 审查你不信任的代码
- 探索代码库且不希望意外修改
- 运行代码分析或审计

**在以下情况使用 `strict`：**

- 分析不受信任或第三方代码
- 在安全敏感环境中运行
- 需要最大程度的隔离

**在以下情况跳过沙箱：**

- agent 需要安装依赖（`npm install`、`pip install`）
- agent 需要修改工作目录外的文件
- 你在受信任的环境中工作，并希望最大灵活性

---

## 权衡

| 方面       | 无沙箱                     | 有沙箱                          |
| ---------- | -------------------------- | ------------------------------- |
| 安全性     | agent 拥有完整系统访问     | agent 受配置档规则限制          |
| 能力       | 可做任何事                 | 受配置档限制                    |
| 性能       | 无开销                     | 开销可忽略                      |
| 恢复能力   | 必须信任 agent             | 内核强制边界                    |

沙箱在操作系统层面强制限制——在 Linux 上通过 Landlock 或挂载命名空间，在 macOS 上通过 Seatbelt——而不是独立虚拟机。
