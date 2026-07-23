# 项目规则（AGENTS.md）

项目规则让你按项目或目录配置 Grok。在仓库中放置 `AGENTS.md` 文件，即可设定编码约定、构建说明、风格指南，以及 Grok 在该代码库中工作时应遵循的其他指令。

---

## 什么是项目规则？

项目规则是 Grok 会读取并加入上下文的 Markdown 文件。在该目录树中的每一次交互，Grok 都会遵循其内容。

这是向 Grok 传授项目约定的主要机制，因此你不必在每个会话中重复说明。

---

## 支持的文件名

Grok 会在每个目录中按以下顺序查找这些文件名：

- `Agents.md`
- `Claude.md`
- `CLAUDE.md`
- `CLAUDE.local.md`
- `AGENT.md`
- `AGENTS.md`

Grok 会加载目录中所有匹配的文件，因此同时包含 `AGENTS.md` 和 `CLAUDE.md` 的文件夹会同时贡献两者。在大小写不敏感的文件系统上，解析为同一文件的名称（例如 `Agents.md` 与 `AGENTS.md`）会去重并只计一次。支持 `Claude.md`、`CLAUDE.md` 和 `CLAUDE.local.md`，以便与 Claude Code 工作流兼容。启用 Claude 兼容时（默认开启），Grok 还会扫描用户主目录下的 `~/.claude/` 中的这些文件名，并在每一级目录检查 `.claude/CLAUDE.md` 与 `.claude/CLAUDE.local.md`——这是 Claude Code 用于项目记忆的位置。启用 Cursor 兼容时，会以同样方式扫描主目录下的 `~/.cursor/`。

### 规则目录

除 AGENTS.md 文件外，Grok 还会从仓库根目录到当前工作目录的每一级（`<dir>`）扫描规则目录中的 `*.md` 文件：

| 位置 | 说明 |
|----------|-------|
| `<dir>/.grok/rules/` | 始终扫描 |
| `<dir>/.claude/rules/` | Claude 兼容（可配置） |
| `<dir>/.cursor/rules/` | Cursor 兼容（可配置） |

Grok 也会扫描主目录级规则，与起始位置无关。这些根路径本身已按厂商区分，因此规则直接位于 `rules/` 下：

| 位置 | 说明 |
|----------|-------|
| `$GROK_HOME/rules/`（默认 `~/.grok/rules/`） | 始终扫描；适用于所有项目 |
| `~/.claude/rules/` | 由 `compat.claude.rules` 控制 |
| `~/.cursor/rules/` | 由 `compat.cursor.rules` 控制 |

主目录规则最先加载，按表中顺序，随后是从仓库根目录到当前目录的项目文件。每个规则目录内的文件按字母序排列。厂商 `rules` 单元独立控制主目录与项目规则，与对应的 `agents` 单元无关。Claude 的 `agents` 单元控制 `~/.claude/` 下的命名文件以及项目中的 `<dir>/.claude/CLAUDE*.md`；通用顶层名称如 `Claude.md`、`CLAUDE.md` 和 `CLAUDE.local.md` 仍会被识别。参见[配置](05-configuration.md#harness-compatibility)。

---

## 发现机制如何工作

Grok 按以下顺序扫描项目规则：

1. **主目录规则**：`$GROK_HOME`，然后是已启用的 `~/.claude/` 与 `~/.cursor/` 来源
2. **仓库规则**：若位于 git 仓库内，则从仓库根目录到当前工作目录（含两端）的每一级目录
3. **仅当前目录**：若不在 git 仓库内，则只扫描当前工作目录

### 示例

给定如下项目结构：

```
~/projects/my-app/
  AGENTS.md              # "Use TypeScript. Follow ESLint rules."
  src/
    AGENTS.md            # "Prefer functional components."
    components/
      AGENTS.md          # "Use CSS modules for styling."
```

当 Grok 在 `~/projects/my-app/src/components/` 中运行时，会加载全部三个文件。指令会累积，因此 Grok 会看到它们全部。

### 更深层的文件优先

Grok 按从仓库根目录到当前工作目录的顺序排列文件，因此更深层目录中的文件在上下文中出现得更晚，在指令冲突时优先。在上例中，若根目录写“Use styled-components”，而 `components/AGENTS.md` 写“Use CSS modules”，则 CSS modules 指令胜出，因为它出现得更晚。

### 自动加载行为

- 会话开始时，Grok 会自动加载从仓库根目录到当前工作目录的文件。
- 当 Grok 在该初始集合之外的目录中读取、列出或编辑文件时，会检测其中的项目指令文件，记录其路径，并在与任务相关时读取它们。

---

## 项目规则中应写什么

### 编码约定

```markdown
# Coding Standards

- Use TypeScript for all new code
- Prefer functional components with hooks over class components
- Use `const` by default; only use `let` when reassignment is needed
- Maximum line length: 100 characters
```

### 构建与测试说明

```markdown
# Build & Test

- Run `npm test` before committing
- Use `npm run lint` to check code style
- Build with `npm run build` -- ensure no TypeScript errors
- Integration tests: `npm run test:e2e` (requires Docker)
```

### 风格指南

```markdown
# Style Guide

- Follow the Airbnb JavaScript Style Guide
- Use 2-space indentation
- Always use trailing commas in multi-line arrays/objects
- Prefer template literals over string concatenation
```

### PR 与提交要求

```markdown
# Version Control

- Write commit messages in conventional commits format
- Prefix branch names with `feature/`, `fix/`, or `chore/`
- All PRs require at least one approval before merge
- Squash-merge feature branches
```

### 架构说明

```markdown
# Architecture

- API routes go in `src/routes/` with one file per resource
- Business logic goes in `src/services/`
- Database queries go in `src/repositories/`
- Never import from `src/routes/` in `src/services/`
```

---

## 将规则限定到子目录

AGENTS.md 文件的作用域是其所在文件夹为根的整个目录树。可用此方式为代码库的不同部分提供不同指令：

```
my-monorepo/
  AGENTS.md                    # Monorepo-wide rules
  packages/
    frontend/
      AGENTS.md                # "Use React. Prefer CSS modules."
    backend/
      AGENTS.md                # "Use Express. Follow REST conventions."
    shared/
      AGENTS.md                # "No framework-specific code in this package."
```

---

## 会话规则标志

要在不编辑文件的情况下为单个会话添加规则，可传入 `--rules`（别名 `--append-system-prompt`）：

```bash
grok --rules "Always use TypeScript. Prefer functional components."
```

Grok 会将此文本追加到会话的 system prompt。用于会话级定制。

要完全替换 system prompt，传入 `--system-prompt-override`（别名 `--system-prompt`）。Grok 会原样使用该文本，并跳过默认 system prompt 与 `--rules`。（相比之下，通过 `--rules` 传入的文本会包在 `<human_rules>` 块中并追加到默认 prompt。）

---

## 文件大小

Grok 会完整加载每个项目指令文件；没有字符上限，也不会截断。即便如此，仍应保持指令简洁、聚焦。更短、更具体的规则比冗长规则更容易被 Grok 遵循，且每个加载的文件都会占用上下文。

---

## Gitignore 过滤

被 `.gitignore` 忽略的文件在发现过程中会被跳过。若要将个人覆盖排除在共享仓库之外，可 gitignore 受识别的文件名，例如 `CLAUDE.local.md`：

```gitignore
# .gitignore
CLAUDE.local.md
```

作为顶层指令文件，Grok 只发现[支持的文件名](#supported-file-names)下列出的受识别名称——不会发现自定义名称，如 `AGENTS.local.md` 或 `notes.md`。（在规则目录如 `.grok/rules/` 内，无论文件名如何，都会加载每个 `*.md` 文件。）

---

## `.grok/` 项目目录

除 AGENTS.md 文件外，项目根目录下的 `.grok/` 目录还可包含额外的项目级配置：

| 路径 | 用途 |
|------|---------|
| `.grok/config.toml` | 项目作用域的 MCP 服务器、插件与权限规则（其他设置仅从 `~/.grok/config.toml` 加载） |
| `.grok/skills/` | 项目作用域的 skill 定义 |
| `.grok/plugins/` | 项目作用域的插件 |
| `.grok/agents/` | 项目作用域的 agent 定义 |
| `.grok/hooks/` | 项目作用域的生命周期钩子 |
| `.grok/lsp.json` | LSP 服务器配置 |

这些均为可选。各组件的详细说明见对应指南。

---

## 检查已加载的规则

使用 `grok inspect` 查看所有已加载的项目指令：

```bash
grok inspect
```

这会显示找到的每个项目指令文件及其路径与大致 token 数量。可用它确认 Grok 是否拾取了你的规则。

---

## 最佳实践

1. **从根目录开始。** 将最重要、适用于全项目的规则放在仓库根目录的 AGENTS.md 中。

2. **尽量具体。** “Use TypeScript” 优于 “Use modern JavaScript”。“Run `cargo fmt` before committing” 优于 “Format your code”。

3. **保持简短。** 简洁指令比冗长指令更可能被遵循。

4. **大型仓库使用子目录作用域。** monorepo 的不同部分可能有不同约定。用按目录的 AGENTS.md 恰当地限定规则范围。

5. **将规则纳入版本控制。** 将 AGENTS.md 提交到仓库，让整个团队受益。用户特定覆盖应放在 `~/.grok/`（全局规则）。

6. **不要重复文档。** AGENTS.md 应包含可执行指令，而非项目 README 的副本。如有需要可链接到外部文档。

7. **定期审查。** 随着项目演进，更新规则以匹配当前约定。
