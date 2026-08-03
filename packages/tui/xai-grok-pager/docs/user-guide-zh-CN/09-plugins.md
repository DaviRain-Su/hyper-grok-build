# 插件

插件将技能（skills）、斜杠命令、代理（agents）、钩子（hooks）、MCP 服务器配置和 LSP 服务器配置打包为一个可安装单元。

---

## 插件包含什么

插件是一个目录，可包含以下任意组件组合：

- **技能（Skills）** -- `skills/` 目录下的 SKILL.md 文件
- **斜杠命令** -- `commands/` 目录下的命令文件
- **代理（Agents）** -- `agents/` 目录下的代理定义
- **钩子（Hooks）** -- `hooks/hooks.json` 生命周期钩子文件。插件钩子还会收到 `GROK_PLUGIN_ROOT` 和 `GROK_PLUGIN_DATA`（传给钩子的全部环境变量见 [Hooks 指南](10-hooks.md)）。
- **MCP 服务器** -- `.mcp.json` 服务器配置文件
- **LSP 服务器** -- `.lsp.json` 语言服务器配置文件

若插件包含 `plugin.json` 清单，清单可覆盖路径或添加元数据；否则组件从约定目录加载。清单是可选的：没有清单时，Grok 会从上述标准目录发现组件。

例如，`team-tools` 插件可能包含部署技能、代码审查代理、pre-commit 钩子以及 Linear MCP 服务器。可一步全部安装。

## 插件钩子中的环境变量

插件钩子除每个钩子都会设置的标准变量外，还会收到两个环境变量：

| 变量 | 说明 |
|----------------------|-------------|
| `GROK_PLUGIN_ROOT`   | 插件安装目录的绝对路径。 |
| `GROK_PLUGIN_DATA`   | 插件可写数据目录的绝对路径，用于插件状态、缓存和日志。 |

Grok 会设置这些值，并覆盖你在钩子 JSON 的 `env` 映射中为同一键声明的任何值。（为兼容性，Grok 也会设置 `CLAUDE_PLUGIN_ROOT` 和 `CLAUDE_PLUGIN_DATA` 别名。）传给钩子的全部环境变量见 [Hooks 指南](10-hooks.md)。

---

## 插件位置

Grok 按以下优先级顺序从这些位置发现插件：

| 位置 | 作用域 | 信任 |
|----------|-------|-------|
| `_meta.pluginDirs`（`session/new` / `session/load`） | 会话 -- 仅在该会话中加载 | 自动信任 |
| `--plugin-dir`（CLI 标志，`grok agent`） | 进程 -- 仅在该 agent 进程中加载 | 自动信任 |
| `.grok/plugins/` | 项目 -- 通过版本控制与团队共享 | 需要信任 |
| `~/.grok/plugins/` | 用户 -- 适用于每个项目的个人插件 | 自动信任 |
| `[plugins].paths`（配置） | 你在 `config.toml` 中添加的自定义目录 | 取决于位置 |

为兼容性，Grok 也会读取 `.claude/plugins/` 的对应路径。当两个插件同名时，优先级更高的位置胜出。

Agent SDK 通过 `GrokOptions.plugins` 加载按会话插件，该选项在 `session/new` 和 `session/load` 上作为 `_meta.pluginDirs` 到达；由于目录由调用方控制，这些插件始终受信任 -- 其钩子和 MCP 服务器无需提示即可激活，且不会超出会话持久化。`--plugin-dir` 标志是直接 CLI 使用时的进程级等价物（可重复：`grok agent --no-leader --plugin-dir A --plugin-dir B stdio`）；它仅适用于专用 agent 进程，在 leader 模式下会被忽略（共享 leader 自行发现自己的插件）。

---

## 在 TUI 中管理插件

### 打开模态框

| 操作 | 打开 |
|--------|-------|
| `Ctrl+L`（任意窗格；**非 VS Code 系列**） | Plugins 标签页 |
| `/plugins`（任意终端；**在 VS Code 系列上必需**） | Plugins 标签页 |

模态框有五个标签页：**Hooks**、**Plugins**、**Marketplace**、**Skills** 和 **MCP Servers**。用 `Tab`（向前）或 `Shift+Tab`（向后）切换标签页。`/hooks`、`/marketplace`、`/skills` 和 `/mcps` 命令会分别在对应标签页打开模态框。

### Plugins 标签页

按 `Enter` 展开插件行并显示详情：

- **名称**和**版本**
- **作用域** -- `cli`、`project`、`user`、`custom path`，或 marketplace 来源名称
- **Skills** -- 名称或数量
- **Agents** -- 名称或数量
- **Hooks** -- 数量
- **MCP 服务器** -- 数量（插件未受信任时为 `blocked`）
- **描述**和**路径**

在 Plugins 标签页使用这些按键：

| 按键 | 操作 |
|-----|--------|
| `r` | 重新加载所有插件 |
| `a` | 从 `owner/repo`、URL 或本地路径添加插件 |
| `Space` | 启用或禁用所选插件 |
| `x` | 卸载所选插件 |
| `f` | 按状态筛选（全部、已启用或已禁用） |
| `Enter` | 展开或折叠插件详情 |
| `/` | 按名称搜索插件 |

### Marketplace 标签页

浏览并从已配置的 marketplace 来源安装插件。

在 Marketplace 标签页使用这些按键：

| 按键 | 操作 |
|-----|--------|
| `i` | 安装所选插件 |
| `d` | 卸载所选插件 |
| `a` | 添加 marketplace 来源 |
| `x` | 移除所选来源及其插件 |
| `r` | 刷新 marketplace 来源 |
| `u` | 更新所选 marketplace 插件 |
| `Enter` | 展开或折叠来源或插件 |
| `/` | 按名称搜索插件 |

列表行上的组件摘要，以及展开视图中按类别的组件详情，仅在 marketplace 发布了 `plugin-index.json` 目录时才会显示。

---

## CLI 命令

无需启动交互式会话即可管理插件。

### 插件命令

```bash
grok plugin list [--json] [--available]   # List installed plugins (--available requires --json)
grok plugin install <source> --trust      # Git URL, GitHub shorthand (user/repo), or local path
grok plugin uninstall <name> [--confirm] [--keep-data]   # Aliases: rm, remove
grok plugin update [<name>]               # Omit the name to update all plugins
grok plugin enable <name>
grok plugin disable <name>
grok plugin details <name>                # Show the plugin's component inventory
grok plugin validate [<path>]             # Validate plugin.json (default: current directory)
grok plugin tag [<path>] [--push] [--force] [--dry-run]   # Tag a release from the manifest version
```

运行 `grok plugin install <source>` 且不带 `--trust` 时，Grok 会打印来源并警告安装将激活该插件的钩子、MCP 服务器和技能，然后停止而不安装。添加 `--trust` 以完成安装。

`<source>` 参数接受：

- `user/repo` -- GitHub 简写
- `user/repo@v1.0` -- 固定到某个 ref
- `user/repo@<commit-sha>` -- 固定到确切 commit（fetch 后校验）
- `user/repo#subdir` -- 仓库内的子目录
- `https://github.com/user/repo.git` -- 完整 URL
- `git@github.com:user/repo.git` -- SSH
- `./local-dir` 或 `/absolute/path` -- 本地目录

### 要求提交固定（`require_sha`）

远程插件没有密码学签名：跟踪分支或标签的安装会在明天运行该 ref 指向的任何内容。运维人员可要求每次远程安装和更新都固定完整的 commit sha（40 或 64 位十六进制，对照拉取后的检出校验）：

```toml
# config.toml
[marketplace]
require_sha = true
```

或 `GROK_MARKETPLACE_REQUIRE_SHA=1`。二者均为只收紧：任一启用该策略后，都无法再关闭。策略开启时，未固定的远程安装、没有发布 `sha` 的 marketplace 安装，以及对跟踪分支的安装的更新都会被拒绝。

范围：该策略覆盖在安装或更新时从远程 git URL 拉取的一切。marketplace 来源内部自带的插件会从该来源已同步的检出中复制，不在此范围内 — 通过在 `plugin-index.json` 中发布 `sha` 条目来固定 marketplace 来源的内容。

### Marketplace 命令

```bash
grok plugin marketplace list [--json]
grok plugin marketplace add <url>         # Git URL, GitHub shorthand (user/repo), or local path
grok plugin marketplace remove <url>      # Git URL or local path of a configured source
grok plugin marketplace update [<name>]   # Omit the name to refresh all sources
```

### 示例：搭建团队 marketplace

```bash
grok plugin marketplace add my-org/team-plugins
grok plugin marketplace list
grok plugin install my-org/team-plugins --trust
grok plugin list
grok plugin update
```

---

## 斜杠命令

在交互式会话中，这些命令会在特定标签页打开模态框。它们不接受参数 — 在模态框中或使用 `grok plugin` CLI 管理插件。

| 命令 | 打开 |
|---------|-------|
| `/plugins` | Plugins 标签页 |
| `/hooks` | Hooks 标签页 |
| `/marketplace` | Marketplace 标签页 |
| `/skills` | Skills 标签页 |
| `/mcps` | MCP Servers 标签页 |

---

## 配置

在 `~/.grok/config.toml` 中配置插件目录和各插件状态：

```toml
[plugins]
paths = ["~/my-plugins/custom-tools"]        # Additional plugin directories
disabled = ["user/a1b2c3d4/noisy-plugin"]    # Plugin IDs or names to skip
enabled = ["project/9f8e7d6c/team-tools"]    # Plugin IDs or names to force on
```

将插件列入 `disabled` 可发现它但跳过加载其组件。将插件列入 `enabled` 可激活它 — 插件默认禁用，除非有 CLI 覆盖或显式配置路径启用，因此在此添加以打开它们。每项要么是纯插件名称（与 `grok plugin list` 显示的一致），要么是完整插件 ID，形式为 `<scope>/<hash>/<name>`。

### 隐藏插件 UI

要隐藏钩子与插件 UI — `/hooks` 和 `/plugins` 命令以及回滚注释 — 在 `~/.grok/pager.toml` 中设置：

```toml
disable_plugins = true
```

---

## Marketplace 来源

添加 git 或本地 marketplace 来源以发现并安装插件。

### 在 config.toml 中

每个来源需要一个 `name`，以及 `git` URL（可带可选的 `branch`）或本地 `path` 之一：

```toml
[[marketplace.sources]]
name = "My Team Plugins"
git = "https://github.com/my-org/plugins.git"

[[marketplace.sources]]
name = "Local Dev"
path = "~/dev/my-plugins"
```

### 在 settings.json 中

在 `extraKnownMarketplaces` 下按名称添加来源。每项的 `source` 为 `git`（含 `url`）、`github`（含 `repo`）或 `local`（含 `path`）之一：

```json
{
  "extraKnownMarketplaces": {
    "my-marketplace": {
      "source": { "source": "git", "url": "git@github.com:my-org/plugins.git" }
    }
  }
}
```

将该文件放在 `~/.grok/settings.json` 或 `~/.claude/settings.json`。

---

## 信任模型

启用插件会加载其技能、斜杠命令和代理。信任是独立的，控制插件代码是否运行：即使对已启用的插件，其钩子、MCP 服务器和 LSP 服务器在你信任之前也会保持不活动。这可防止不受信任的仓库在你的机器上运行代码。

Grok 会自动信任 `~/.grok/plugins/` 中的插件。`.grok/plugins/` 中的项目插件需要显式信任。要信任插件，使用 `--trust` 安装：

```bash
grok plugin install <source> --trust
```

---

## 检查插件

运行 `grok inspect` 可查看所有已发现的插件及其提供的内容：

```bash
grok inspect          # Show plugins with their skills, agents, hooks, and MCP servers
grok inspect --json   # Emit machine-readable JSON
```

插件提供的组件会在各自章节（Skills、Agents、MCP Servers 等）中出现，并带有 `plugin: <name>` 标签，便于查看每个组件的来源。

---

## 通用键盘快捷键

这些按键在模态框的每个标签页中均可用：

| 按键 | 操作 |
|-----|--------|
| `Tab` | 下一标签页 |
| `Shift+Tab` | 上一标签页 |
| `j` / 向下箭头 | 选择下移 |
| `k` / 向上箭头 | 选择上移 |
| `Enter` | 展开或折叠所选项目 |
| `/` | 按名称搜索当前标签页 |
| `Esc` | 清除搜索，或关闭模态框 |

某些操作（例如卸载插件）会要求确认。按 `y` 确认，或按 `Esc` 取消。
