# Grok Build 用户指南

了解如何安装、配置和扩展 Grok Build——由 SpaceXAI 提供的终端 AI 编程助手。

> **English:** [User Guide (EN)](../user-guide/README.md)

---

## 第 1 级：基础用户文档

从这里开始。这些指南涵盖你第一天需要了解的内容。

| # | 文档 | 说明 |
|---|----------|-------------|
| 1 | [快速入门](01-getting-started.md) | 安装、首次启动、身份验证、基本交互与核心概念 |
| 2 | [身份验证](02-authentication.md) | 浏览器登录、API 密钥、OIDC/SSO、外部认证提供方与设备码流程 |
| 3 | [键盘快捷键](03-keyboard-shortcuts.md) | TUI 中全部按键绑定与鼠标操作参考 |
| 4 | [斜杠命令](04-slash-commands.md) | 全部 `/` 命令，包括目标、深度研究与工作流运行管理 |
| 5 | [配置](05-configuration.md) | `config.toml`、`pager.toml`、环境变量与文件位置 |

---

## 第 2 级：核心功能文档

自定义并扩展 Grok Build。

| # | 文档 | 说明 |
|---|----------|-------------|
| 6 | [主题与外观](06-theming.md) | 主题、`/theme` 命令、`pager.toml` 与颜色支持检测 |
| 7 | [MCP 服务器](07-mcp-servers.md) | 通过 Model Context Protocol 集成外部工具 |
| 8 | [Skills](08-skills.md) | 采用 SKILL.md 格式的可复用提示词包 |
| 9 | [插件](09-plugins.md) | 打包并共享 skills、命令、agents、hooks 与 MCP 服务器；从市场源安装 |
| 10 | [Hooks](10-hooks.md) | 生命周期脚本与 HTTP 回调，用于工具调用前/后事件 |
| 11 | [自定义模型](11-custom-models.md) | 自带密钥、Ollama 与 OpenAI 兼容端点 |
| 25 | [Moonshot 提供方](25-moonshot-providers.md) | 内置 Moonshot / Kimi 开放平台 API 密钥（`moonshot-cn`、`moonshot-ai`） |
| 26 | [Kimi Code 订阅](26-kimi-code.md) | Kimi Code 的设备 OAuth 登录（`grok login --kimi`） |
| 27 | [OpenAI 与 Anthropic](27-openai-anthropic.md) | 内置 OpenAI Responses 与 Anthropic Messages 平台 |
| 12 | [项目规则（AGENTS.md）](12-project-rules.md) | 按目录生效的 AGENTS.md 指令及其优先级 |
| 13 | [记忆](13-memory.md) | 跨会话知识持久化，配合 `/flush`、`/dream` 与混合搜索 |

---

## 第 3 级：高级用法文档

自动化、脚本化，并将 Grok Build 与其他系统集成。

| # | 文档 | 说明 |
|---|----------|-------------|
| 14 | [无头模式与脚本](14-headless-mode.md) | `grok -p`、输出格式、CI/CD 集成与管道 |
| 15 | [Agent 模式与 IDE 集成](15-agent-mode.md) | ACP stdio 传输、WebSocket 中继与 SDK 集成 |
| 16 | [Subagents 与 Personas](16-subagents.md) | 并行子会话、agent 类型、personas 与能力模式 |
| 17 | [会话管理](17-sessions.md) | 保存、加载、恢复、回退、压缩，以及会话持久化格式 |
| 18 | [沙箱模式](18-sandbox.md) | 操作系统级文件系统与网络隔离配置 |
| 19 | [计划模式](19-plan-mode.md) | 结构化规划、计划文件编辑，以及编码前的审批 |
| 20 | [后台任务与监控](20-background-tasks.md) | `background: true`、`/loop`、`monitor`，以及用 `Ctrl+B` 降级 |
| 21 | [终端支持与故障排查](21-terminal-support.md) | tmux、SSH、truecolor、剪贴板与 OSC 52 |
| 22 | [权限与安全控制](22-permissions-and-safety.md) | `dontAsk` 模式、自动批准的工具、safe-bash 列表，以及限制性 PreToolUse hooks（例如仅允许 git/gh） |
| 23 | [Agent 仪表盘](23-dashboard.md) | 本地会话与 fork 的集中总览 |
| 24 | [用量监控（外部 OpenTelemetry）](24-monitoring-usage.md) | 客户侧 OTEL 导出 |
