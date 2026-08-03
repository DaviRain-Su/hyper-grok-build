# Skills

Skills 是可复用的提示包，通过任务专用指令扩展 Grok 的能力。你只需把可重复的流程记录一次，而不必在每次会话中重新说明。

---

## 什么是 Skills？

Skill 是一个包含 `SKILL.md` 文件的目录。其 Markdown 正文告诉 Grok 如何处理某类特定任务：分步说明、约定以及工具使用方式。

适用于那些对 AGENTS.md 来说过于具体、又不值得每次重打的可重复流程。Grok 仅在 skill 与当前任务相关时才会激活它。

---

## Skill 位置

Grok 按优先级从这些目录发现 skills：

| 位置 | 作用域 | 优先级 | 说明 |
|----------|-------|----------|-------|
| `./.grok/skills/`、`./.grok/commands/` | 本地（CWD） | 最高 | 当前目录 skills / 旧版 command markdown |
| `<repo_root>/.grok/skills/`、`…/commands/` | 仓库 | 中 | 在整个仓库内共享 |
| `~/.grok/skills/`、`~/.grok/commands/` | 用户 | 最低 | 适用于所有项目的个人 skills |
| `~/.claude/skills/`、`~/.claude/commands/` | 用户 | 最低 | Claude Code 兼容（可配置） |
| `./.claude/skills/`、`./.claude/commands/` | 本地 / 仓库 | 高 | 项目 Claude skills 与旧版自定义 slash commands |
| `~/.cursor/skills/` | 用户 | 最低 | Cursor 兼容（可配置） |
| `./.cursor/skills/` | 本地 / 仓库 | 高 | 项目 Cursor skills（在启用 cursor 兼容 skills 时） |

Grok 按名称对 skills 去重——优先级更高的位置会覆盖较低位置。Grok 还会在每一层扫描 `.agents/skills/`（以及 `commands/`，与 `.grok/` 并列），并遍历从工作目录到仓库根目录之间的所有目录。

`commands/` 目录下的扁平 `*.md` 文件会成为用户可调用的 slash commands（文件名主干 = 命令名），与 Claude Code 的旧版自定义命令布局一致。

Skill 与 command 的发现**不会**使用 `.gitignore`。已知 skill 根目录（`.grok/`、`.agents/`、`.claude/`、`.cursor/`）下的路径，只要磁盘上存在就会加载——团队常把 `.claude/**` 当作仅本地配置忽略，但仍希望 `/frontend` 这类项目命令可用。若要隐藏某个 skill，请在配置中使用 `[skills] ignore`（而不是仓库的 ignore 规则）。

Grok 默认会扫描 Claude 与 Cursor 的 skill 目录。若要停止扫描某个厂商，请在 `~/.grok/config.toml` 的 `[compat.cursor]` 或 `[compat.claude]` 下将其 `skills` 设为 `false`，或将环境变量 `GROK_CURSOR_SKILLS_ENABLED` 或 `GROK_CLAUDE_SKILLS_ENABLED` 设为 `false`。详见 [配置](05-configuration.md#harness-compatibility)。无论这些设置如何，Grok 始终会过滤已知的厂商默认 skills（例如 Cursor 的 `shell`、`canvas` 和 `statusline`）。

### 额外 Skill 目录

通过 `~/.grok/config.toml` 中的 `[skills]` 可添加目录、排除路径，或禁用单个 skill：

```toml
[skills]
paths = ["~/my-team-skills"]          # Additional directories to scan
ignore = ["~/my-team-skills/wip"]     # Paths to exclude (hidden entirely)
disabled = ["wip-skill"]              # Skill names to keep listed but inactive
```

`paths` 中的每一项可以是一个 `SKILL.md` 文件，或 Grok 会递归遍历的目录。`ignore` 会完全隐藏某个 skill；`disabled` 会保留其在列表中，但排除在系统提示与调用之外。`paths` 与 `ignore` 接受文件系统路径并支持 `~` 展开；`disabled` 接受 skill 名称。

---

## 创建 Skill

### 目录结构

每个 skill 位于各自的目录中，并包含一个 `SKILL.md` 文件：

```
~/.grok/skills/
  commit/
    SKILL.md
  review-pr/
    SKILL.md
  deploy/
    SKILL.md
```

### SKILL.md 格式

Skill 文件由 YAML frontmatter 后接 Markdown 说明组成：

```markdown
---
name: commit
description: Create well-formatted git commits following conventional commit standards. Use when the user wants to commit changes or asks for /commit.
---

# Git Commit Skill

Review staged changes and create a commit with a clear, conventional message.

## Steps

1. Run `git diff --staged` to see changes
2. Summarize what changed and why
3. Create commit message following conventional commits format
4. Run `git commit -m "..."` with the message
```

### 核心 Frontmatter 字段

| 字段 | 说明 |
|-------|-------------|
| `name` | Skill 标识符。使用小写字母、数字和连字符，最长 64 个字符。Grok 会将空格和下划线规范为连字符。若省略 `name`，Grok 使用 skill 的目录名。 |
| `description` | Skill 做什么以及何时使用。Grok 据此决定是否调用该 skill。若省略，Grok 使用正文的第一段。 |

请写具体的 `description`。它决定 Grok 何时自动调用该 skill。写明触发短语与使用场景。

### 可选 Frontmatter 字段

多词 frontmatter 键使用 kebab-case（单词语键如 `model` 原样书写）。

| 字段 | 说明 |
|-------|-------------|
| `when-to-use` | 自动调用的触发短语，与 `description` 分开维护。 |
| `allowed-tools` | Skill 使用的工具，可为 YAML 列表，或逗号/空格分隔的字符串。 |
| `argument-hint` | 在 slash-command 自动补全中显示的提示文本（例如 `commit message`）。 |
| `user-invocable` | 是否可作为 slash command 运行。默认为 `true`；设为 `false` 可从 slash commands 中隐藏。（若要阻止模型调用 skill，请改设 `disable-model-invocation`。） |
| `disable-model-invocation` | 为 `true` 时，仅你的 slash command 可运行该 skill——模型不能自动调用。默认为 `false`。 |
| `model` | 运行该 skill 时的模型覆盖。 |
| `effort` | 推理强度覆盖。 |
| `license` | 许可证标识符（例如 `Apache-2.0`）。 |
| `compatibility` | 环境要求（例如 `Requires git, docker, jq`）。 |
| `metadata` | 任意字符串键值对。Grok 会提升 `metadata.author` 与 `metadata.short-description` 用于展示。 |

---

## 使用 /create-skill 创建 Skills

`/create-skill` 命令会交互式引导你构建新 skill。Grok 询问你的需求、起草文件并写入磁盘。

### 工作方式

运行 `/create-skill` 时，Grok 会：

1. **收集需求。** Grok 询问 skill 名称、保存作用域，以及你想捕获的工作流描述。名称应使用小写字母、数字和连字符（2–64 个字符，以字母或数字开头和结尾）。

2. **起草 description。** Grok 会写出说明 skill 做什么、触发短语以及 slash command 名称的 `description`。你需先批准或编辑草稿再继续。

3. **创建 skill 目录。** Grok 创建 `<scope>/.grok/skills/<name>/` 目录；若 skill 需要，还会创建 `scripts/` 或 `references/` 子目录。

4. **写入 SKILL.md。** Grok 写入 frontmatter（`name` 与 `description`）以及 Markdown 指令正文，以及任何配套文件。

5. **校验并确认。** Grok 回读文件、确认写入正确，并告诉你如何运行该 skill。

### 选择作用域

Grok 会询问将 skill 保存到何处：

- **项目**（`<repo_root>/.grok/skills/<name>/`）——仅在本仓库可用，可通过版本控制与队友共享。在 git 仓库内时，Grok 推荐此作用域。
- **用户**（`~/.grok/skills/<name>/`）——在你的所有项目中可用。

新 skill 会在几秒内出现在 slash 菜单中，因为磁盘文件变化时 Grok 会重新加载 skills。

---

## 使用 Skills

### 按名称运行 Skill

每个 skill 都是一个以其名称命名的 slash command。输入名称即可运行：

```
/commit              # Runs the "commit" skill
/review-pr           # Runs the "review-pr" skill
```

运行 skill 会将其指令加载到对话中，并引导模型遵循它们。若要传递参数，在名称后输入：

```
/commit fix the build
```

要浏览你的 skills，输入 `/` 打开 slash-command 菜单。Grok 会列出所有内置命令与 skills，并随你输入进行过滤。若要从命令行列出 skills，请运行 `grok inspect`（见 [查看 Skill 详情](#viewing-skill-details)）。

### 限定名

当 skill 名称与另一个 skill 或内置命令冲突时，Grok 会公布以 skill 作用域为前缀的限定名——`local:`、`repo:`、`user:`，或插件名。使用限定形式可选择特定 skill：

```
/local:commit        # The "commit" skill from ./.grok/skills/
/user:commit         # The "commit" skill from ~/.grok/skills/
```

### 自动调用

当 Grok 识别到相关任务时，可以自行调用 skill。Grok 会将你的提示与 skill 的 `description` 和 `when-to-use` 字段匹配，因此请写清触发情境。

例如，若 skill 的 description 写着 “Use when the user wants to commit changes”，那么说 “commit my changes” 就可能自动触发该 skill。若要求显式 slash command 并禁止自动调用，请在 frontmatter 中设置 `disable-model-invocation: true`。

---

## 查看 Skill 详情

运行 `grok inspect` 可查看 Grok 发现的所有 skills，以及其余配置：

```bash
grok inspect          # Human-readable summary
grok inspect --json   # Machine-readable report
```

在人类可读输出中，Skills 部分会列出每个 skill 的名称及其来源——`project`、`user`、`bundled`、`config`（来自 `[skills].paths` 条目）、`server`（从托管工作区 skill store 同步的 skills），或 `plugin: <name>`。Grok 会为通过 `[skills].disabled` 禁用的 skill，或来自已禁用厂商界面的 skill 标记 `[disabled]`。

报告会与实时会话一样遵循你的 `[skills]` 配置：来自 `paths` 的 skills 会列出，位于 `ignore` 前缀下的 skills 会被隐藏，在 `disabled` 中命名的 skills 仍会列出但标记为 `[disabled]`。

`--json` 报告包含每个 skill 的完整详情：其 `name`、`description`、`source`（含 `SKILL.md` 文件路径），以及 `userInvocable` 标志。

---

## 内置与插件 Skills

Grok 将平台 skills 与个人 skills 分开分发。内置 skills 缓存在 `~/.grok/bundled/skills/`；Grok 绝不会把它们写入 `~/.grok/skills/`。同名的本地、仓库或用户 skill 会覆盖内置副本。`grok inspect` 会按实际来源标注每个定义。（同名的插件 skill 不会覆盖原生 skill；它仍可通过其限定名 `plugin:name` 使用。）

Skills 也可来自插件。安装包含 skills 的插件后，它们会与你的用户和项目 skills 一并出现。`grok inspect` 会将每个插件提供的 skill 的来源标注为 `plugin: <name>`。

关于安装可提供 skills 的插件，详见 [插件指南](09-plugins.md)。

---

## 最佳实践

1. **写具体的 description。** description 驱动自动调用。“Create git commits” 过于模糊；“Create well-formatted git commits following conventional commit standards. Use when the user wants to commit changes or asks for /commit.” 效果更好。

2. **包含具体步骤。** 当 skill 给出清晰、有序的流程时效果最佳。

3. **按名称引用工具。** 当 skill 依赖特定工具（例如 `run_terminal_command` 或 `search_replace`）时，请写出名称，以便模型知道该用什么。

4. **保持 skill 聚焦。** 每个工作流写一个 skill。“deploy” 与 “rollback” 两个 skill 比单一的 “deploy-and-rollback” skill 更好。

5. **将项目 skills 纳入版本控制。** 把 `.grok/skills/` 提交到仓库，让整个团队受益。`~/.grok/skills/` 中的用户 skills 保持个人且不共享。

6. **通过运行来测试。** 调用 `/name` 并确认 skill 可用后，再依赖自动调用。
