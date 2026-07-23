# Hooks 与 Plugins 指南

Grok Build 支持 **hooks**（事件驱动的 shell 命令）和 **plugins**（技能、agent、hook 与 MCP 服务器的打包集合）。二者均通过统一的模态界面进行管理。

## 打开模态界面

| 方式 | 打开的标签页 |
|------|-------------|
| `Ctrl+L` | Plugins（任意窗格；**非 VS Code 系列** — 在 VS Code / Cursor / Windsurf / Zed 中请使用 `/plugins`） |
| `/plugins` | Plugins（任意终端） |
| `/hooks` | Hooks |

## 标签页

该模态界面包含三个标签页：**Hooks**、**Plugins** 与 **Marketplace**。使用 `Tab` / `→`（向前）或 `Shift+Tab` / `←`（向后）在标签页之间切换。

---

## Hooks 标签页

Hooks 是在诸如 `session_start`、`post_tool_use`、`notification` 等事件上自动运行的 shell 命令（或 HTTP 调用）。关于如何编写自定义 hook，请参阅 [创建自定义 Hooks](custom-hooks.md)。

Hooks 按来源分组：
- **全局 hooks** — 来自 `~/.grok/hooks/`
- **项目 hooks** — 来自仓库中的 `.grok/hooks/`
- **插件 hooks** — 随已安装插件一并提供
- **自定义 hooks** — 通过路径手动添加

每个 hook 会显示：
- 触发的 **Event**（例如 `session_start`、`post_tool_use`）
- 运行的 **Command** 或 **URL**
- **Timeout** 时长
- **Status** — 已启用或 `[disabled]`

### 快捷键（Hooks 标签页）

| 按键 | 操作 |
|------|------|
| `l` | 重新加载全部 hooks |
| `a` | 从路径添加 hook |
| `r` | 移除选中的 hook |
| `e` | 启用 / 禁用选中的 hook |
| `Space` | 展开 / 折叠分组 |

---

## Plugins 标签页

Plugins 是目录，可包含技能、agent、hook 与 MCP 服务器配置的任意组合。

每个插件在展开时会显示：
- **Name** 与 **version**
- **Scope** — `user`、`project`、`cli`，或 marketplace 来源名称
- **Skills** — 名称或数量
- **Agents** — 名称或数量
- **Hooks** — 数量
- **MCP servers** — 数量（若未信任则为 "blocked"）
- **Description**
- **Conflicts** — 若有冲突则显示 ⚠ 警告

插件 hooks 会自动获得 `GROK_PLUGIN_ROOT` 与 `GROK_PLUGIN_DATA` 环境变量（参见 [Plugins 指南](../user-guide/09-plugins.md#environment-variables-in-plugin-hooks)）。

### 快捷键（Plugins 标签页）

| 按键 | 操作 |
|------|------|
| `r` | 重新加载全部 plugins |
| `i` | 从路径安装 plugin |
| `e` | 启用 / 禁用选中的 plugin |
| `Space` | 展开 / 折叠插件详情 |
| `/` | 按名称搜索 plugins |

---

## Marketplace 标签页

从已配置的 marketplace 来源浏览并安装 plugins。

来源加载自：
1. **config.toml** — `[[marketplace.sources]]` 条目
2. **settings.json** — 来自 `~/.grok/settings.json` 或 `~/.claude/settings.json` 的 `extraKnownMarketplaces`

每个来源会展示其 plugins，并包含：
- **Name** 与 **version**
- **Description**
- **Install status** — `[installed]`、`[installed • update: v1 → v2]`，或未安装

### 快捷键（Marketplace 标签页）

| 按键 | 操作 |
|------|------|
| `i` | 安装选中的 plugin |
| `d` | 卸载选中的 plugin |
| `r` | 刷新 marketplace 来源（重新 clone/pull git 仓库） |
| `u` | 更新所有已安装的 marketplace plugins |
| `Space` | 展开 / 折叠来源或 plugin |
| `/` | 按名称搜索 plugins |

### 添加 Marketplace 来源

在 Marketplace 标签页按 `a`（或运行 `grok plugin marketplace add <source>`），
可使用 git URL、GitHub 简写（`owner/repo`），或本地目录路径
（`/absolute`、`~/dir` 或 `./relative`）。本地路径会以 `path`
来源形式存储 — 便于基于已有检出目录开发 marketplace。

来源会写入 `~/.grok/config.toml`：

```toml
[[marketplace.sources]]
name = "My Team Plugins"
git = "https://github.com/my-org/plugins.git"

[[marketplace.sources]]
name = "Local Dev"
path = "~/dev/my-plugins"
```

或写入 `~/.grok/settings.json` / `~/.claude/settings.json`：

```json
{
  "extraKnownMarketplaces": {
    "my-marketplace": {
      "source": { "source": "git", "url": "git@github.com:my-org/plugins.git" },
      "autoUpdate": true
    }
  }
}
```

---

## 通用键盘快捷键

这些快捷键在所有标签页中均可用：

| 按键 | 操作 |
|------|------|
| `Tab` / `→` | 下一个标签页 |
| `Shift+Tab` / `←` | 上一个标签页 |
| `j` / `↓` | 向下移动选择 |
| `k` / `↑` | 向上移动选择 |
| `Space` | 切换展开 / 折叠 |
| `/` | 开始搜索（Plugins 与 Marketplace） |
| `Backspace` | 删除搜索字符，或重新进入搜索 |
| `Esc` | 清除搜索，或关闭模态界面 |
| `q` | 关闭模态界面 |

## 确认与错误

某些操作（例如卸载 plugin）可能会要求确认：
- 按 `y` 确认
- 按 `Esc` 或任意其他键取消

错误会以消息浮层显示 — 按任意键关闭。

操作进行中时，模态界面会显示 "Processing..." 并屏蔽输入，直至操作完成。

## 另请参阅

- [创建自定义 Hooks](custom-hooks.md) — 编写自定义 hooks 与脚本的分步指南
- [Hooks 用户指南](user-guide/10-hooks.md) — 事件、匹配器、信任模型
- [Hook 示例](../../../xai-grok-hooks/examples/README.md) — 可直接使用的示例 hooks
- [Plugins 用户指南](user-guide/09-plugins.md) — 安装、信任与 marketplace
