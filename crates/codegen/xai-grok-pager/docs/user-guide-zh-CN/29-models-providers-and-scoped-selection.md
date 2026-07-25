# 模型、平台与 Scoped 选择

Hyper 如何列出模型、与 [Pi](https://github.com/earendil-works/pi) 的对比，以及 Pi 的 **scoped models（范围模型 / 短名单）** 是什么。

> **English:** [Models, Providers, and Scoped Selection](../user-guide/29-models-providers-and-scoped-selection.md)

---

## 速查

| 任务 | 命令 |
|------|------|
| 列出模型 | `hyper models` 或 `/model` |
| 切换模型 | `/model <id>` · `/m <id>` · 滚动区 **Ctrl+M** 打开选择器 |
| 默认模型 | `~/.grok/config.toml` → `[models] default = "…"` |
| 自定义 / 本地 | `[model.<name>]` — 见 [自定义模型](11-custom-models.md) |
| 平台 API Key | `/providers <platform> <key>` |
| ChatGPT Codex | `/login openai` 后 `/model openai-codex/…` — [Codex](28-openai-codex.md) |

模型 id 多为 `platform/model`（例如 `openrouter/anthropic/claude-sonnet-4.5`）。

---

## Hyper 现状

### 内置目录

Hyper 内嵌 **platform catalog**（`platform_catalog.json`），大量行从 Pi 的开源 provider 表同步而来，当前量级约 **460+** 第三方条目，覆盖 openrouter、openai、anthropic、ollama、together、fireworks、nvidia、groq、moonshot、kimi-code、minimax、zai、deepseek 等。

SpaceXAI 默认模型在 `default_models.json`（新会话默认多为 `grok-4.5`）。

### `platform_registry.json`

注册表定义 base URL、环境变量、是否 OAuth。**只有已注册且有凭据的平台，模型才真正可用。** 仅有目录行、没有 registry / 没有 key 时，选择器会显得“很少”。

### 为什么感觉模型“不全”

1. **未配 BYOK 时** 主要看到 Grok 默认列表。  
2. **按鉴权过滤** — 没有 Anthropic key 就看不到 / 用不了对应模型。  
3. **相对 Pi 的平台缺口** — Google / Vertex / Bedrock / Azure / Copilot / HuggingFace 等尚未一等公民接入。  
4. **openai-codex** 走独立 OAuth，与 `openai/*` API Key 不是同一条路径。

---

## Pi 的 SCOPE-MODEL（Scoped Models）是什么

Pi 里常说的 scoped models **不是**“模型能力 scope”，而是：

> **从全量目录里勾选一个短名单，用快捷键在短名单里快速切换。**

| Pi | 行为 |
|----|------|
| `/scoped-models` | 勾选、全选/清空、按提供商切换、排序、保存到 settings |
| `enabledModels` | 配置 glob，如 `["claude-*", "gpt-4o"]` |
| `--models` CLI | 同格式，限本次进程 |
| **Ctrl+P / Shift+Ctrl+P** | 在 **scoped 短名单** 里前后切换 |
| **Ctrl+L** | 打开**全量**模型选择器 |

注意：Hyper 的 **Ctrl+P** 是**命令面板**，不是模型循环。Hyper 目前用 **`/model` / Ctrl+M** 切模型，**还没有** `/scoped-models` 或 `enabledModels`。

这正是很多人觉得 Pi “好用、模型切换顺”的原因：日常只用 5～15 个模型，而不是在 400+ 里翻。

### 拟议的 Hyper 方向（文档先行）

```toml
# ~/.grok/config.toml（提案，尚未全部实现）
[models]
default = "openrouter/anthropic/claude-sonnet-4.5"
# enabled = ["grok-*", "openai-codex/*", "openrouter/anthropic/*"]
```

```text
# 提案
/scoped-models
# 为短名单单独绑定循环键（避免与 Ctrl+P 命令面板冲突）
```

---

## 其它可向 Pi 学习的模型配置

| 能力 | Pi | Hyper 现状 |
|------|-----|------------|
| `~/.pi/agent/models.json` 热重载 | 有 | `[model.*]` 在 config.toml，体验略不同 |
| `$ENV` / `!command` 解析密钥 | 有 | `env_key` / 环境变量为主 |
| `compat`（developer role 等） | 有 | 部分靠 `api_backend` / headers |
| 每模型 cost 与用量 | 有 | 视平台而定 |
| `pi update --models` 刷新目录 | 有 | 编译期嵌入 + 部分 OAuth 在线列表 |
| 平台广度 | 更全 | OpenRouter 大；Google/Bedrock/Azure/Copilot 等待补 |

---

## 想尽快“模型变全”的实操

1. **一条 OpenRouter**（最快铺开）：`/providers openrouter <key>`  
2. **官方 API Key**：`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` 等  
3. **订阅**：`/login openai`、`/login kimi`  
4. **本地**：Ollama 的 `[model.*]` 段（见自定义模型文档）  
5. **默认**：`[models] default = "…"`

---

## 相关文档

- [认证](02-authentication.md) — 多平台登录与额度用尽后的出路  
- [自定义模型](11-custom-models.md)  
- [OpenAI Codex](28-openai-codex.md) · [Kimi Code](26-kimi-code.md) · [Moonshot](25-moonshot-providers.md)  
- Pi：[models.md](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md) · [enabledModels](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/settings.md)  
