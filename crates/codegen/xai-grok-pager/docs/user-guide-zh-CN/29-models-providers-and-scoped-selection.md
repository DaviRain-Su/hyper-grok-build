# 模型、平台与 Scoped 选择

Hyper 如何列出模型、与 [Pi](https://github.com/earendil-works/pi) 的对比、**scoped models（短名单）**，以及我们支持哪些协议。

> **English:** [Models, Providers, and Scoped Selection](../user-guide/29-models-providers-and-scoped-selection.md)

---

## 速查

| 任务 | 命令 |
|------|------|
| 列出模型 | `hyper models` 或 `/model` |
| 切换模型 | `/model <id>` · `/m <id>` · 滚动区 **Ctrl+M** 打开选择器 |
| 短名单循环 | **Alt+]** 下一个 · **Alt+[** 上一个 |
| 管理短名单 | `/scoped-models` · `add` / `remove` / `set` / `clear` |
| 默认模型 | `~/.grok/config.toml` → `[models] default = "…"` |
| 循环短名单 | `[models] enabled_models = ["grok-*", …]` |
| 硬允许列表 | `[models] allowed_models = […]`（不匹配则不可选） |
| 自定义 / 本地 | `[model.<name>]` — 见 [自定义模型](11-custom-models.md) |
| 平台 API Key | `/providers <platform> <key>` |
| ChatGPT Codex | `/login openai` 后 `/model openai-codex/…` — [Codex](28-openai-codex.md) |

模型 id 多为 `platform/model`（例如 `openrouter/anthropic/claude-sonnet-4.5`）。

---

## 协议立场：优先 OpenAI 兼容

Hyper 的线协议只认真支持少数几种 HTTP API：

| 后端 | 常见厂商 |
|------|----------|
| **Chat Completions** | openrouter、groq、together、fireworks、deepseek、ollama、moonshot 等 |
| **Messages**（Anthropic 风格） | anthropic、kimi-code（默认）、部分 MiniMax / Fireworks Messages 行 |
| **Responses** | openai（BYOK）、openai-codex（ChatGPT OAuth） |

**社区版不打算做 Google AI Studio / Vertex、Amazon Bedrock、Azure 管理面、GitHub Copilot OAuth 的一等公民原生客户端**：它们需要不同的请求体、订阅产品接线，以及我们目前没有的 OAuth 体系。

**推荐替代路径：**

1. **OpenRouter**（或其它 OpenAI 兼容网关）— 一把 key 覆盖大量远程模型（含网关上的 Gemini 等）。  
2. **`[model.*]` 自定义 OpenAI 兼容 base URL** — 任意暴露 chat completions 的代理。  
3. **已接入的官方 OpenAI / Anthropic / Messages / Responses** 注册平台。

若厂商只有专有 API、没有 OpenAI 或 Messages 兼容端点，请走网关，或不要期望一等支持。

### 内置目录与注册表

内嵌 **platform catalog**（约 460 行第三方，多数为 chat_completions），大量从 Pi 开源表同步；SpaceXAI 默认在 `default_models.json`。

**注册平台**：openai、anthropic、kimi-code（OAuth）、moonshot-*、deepseek、groq、mistral、xai-direct、together、fireworks、openrouter、nvidia、ollama、cerebras、minimax-*、zai-*、openai-codex（OAuth）等。

只有**已注册且有凭据**的平台，模型才真正可用。

### 为什么感觉模型“不全”

1. 未配 BYOK 时主要是 Grok 默认列表。  
2. 按鉴权过滤 — 没 key 的平台会锁定。  
3. **故意不做** Google / Bedrock / Azure 原生行（请用 OpenRouter / 自定义 base）。  
4. openai-codex 走 OAuth，与 `openai/*` API Key 不同路径。

---

## Scoped models（已实现）

Pi 的 scoped models 是**快速切换用的短名单**，不是能力 scope。

| 概念 | Hyper |
|------|--------|
| 配置 | `[models].enabled_models`（glob；兼容别名 `enabledModels`） |
| 斜杠命令 | `/scoped-models` · 别名 `/scoped`、`/enabled-models` |
| 循环键 | **Alt+]** 下一个 · **Alt+[** 上一个（**不是** Ctrl+P；那是命令面板） |
| 全量选择器 | `/model` · **Ctrl+M** |
| 软 vs 硬 | `enabled_models` 只影响循环；**`allowed_models`** 仍是硬门禁 |

`enabled_models` 为空时，循环键遍历**当前全部可用**（未锁定）模型。

```toml
# ~/.grok/config.toml
[models]
default = "openrouter/anthropic/claude-sonnet-4.5"
enabled_models = ["grok-*", "openai-codex/*", "openrouter/anthropic/*"]
```

```text
/scoped-models
/scoped-models add grok-*
/scoped-models remove openai/gpt-5
/scoped-models set grok-4.5 openrouter/anthropic/claude-sonnet-4.5
/scoped-models clear
```

**不要和这些搞混：**

| 配置 | 作用 |
|------|------|
| `hidden_models` | 从选择器隐藏（All 视图 ^X）；`-m` 仍可用 |
| `disabled_models` | 从目录移除 |
| `allowed_models` | 硬允许列表 |

---

## 其它可借鉴 Pi 的点

| 能力 | Pi | Hyper |
|------|-----|--------|
| 用户 models 叠加 | `models.json` 热重载 | config.toml 的 `[model.*]` |
| 密钥 `$ENV` / `!command` | 有 | 主要是 env / `env_key` |
| 目录刷新 | `pi update --models` | 编译期嵌入 + 部分在线列表 |
| 原生厂商广度 | Google、Bedrock… | **优先 OpenAI 兼容网关**（见上文立场） |

---

## 想尽快“模型变全”

1. **一条 OpenRouter**：`/providers openrouter <key>`  
2. **官方 API Key**：`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`  
3. **订阅**：`/login openai`、`/login kimi`  
4. **短名单**：`/scoped-models set …` 后日常 **Alt+]** / **Alt+[**  
5. **本地 OpenAI 兼容**：`[model.*]` + `base_url`

---

## 相关文档

- [认证](02-authentication.md)  
- [自定义模型](11-custom-models.md)  
- [OpenAI Codex](28-openai-codex.md) · [Kimi Code](26-kimi-code.md) · [Moonshot](25-moonshot-providers.md) · [OpenAI & Anthropic](27-openai-anthropic.md)  
- Pi：[models.md](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md) · [enabledModels](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/settings.md)  
