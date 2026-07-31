# 自定义模型

Grok 可连接自定义模型端点，用于替代提供商、自托管模型，以及覆盖内置设置。本指南说明如何选择模型、配置端点，以及集成第三方提供商。

---

## 默认模型

默认情况下，Grok 使用 SpaceXAI 托管的模型，新会话以 `grok-build` 启动。默认模型无需额外配置。使用 `grok login` 或 API 密钥完成认证后即可开始会话。

列出所有可用模型：

```bash
grok models
```

---

## 选择模型

### CLI 标志

```bash
grok -p "Hello" -m grok-build
```

### 斜杠命令

在 TUI 中，可在会话期间切换模型：

```
/model grok-build
```

或使用别名：

```
/m grok-build
```

### 模型选择器（Ctrl+M）

在滚动回看窗格中按 `Ctrl+M` 可打开模型选择器。它会列出所有可用模型（内置与自定义），并支持单键切换。当焦点在提示输入框时，`Ctrl+M` 会改为切换多行输入 —— 此时请使用 `/model` 切换模型，而无需离开提示框。

### 配置默认值

在 `~/.grok/config.toml` 中设置持久默认模型：

```toml
[models]
default = "grok-build"
```

---

## 支持的 API 后端

Grok 支持四种 API 后端。在 `[model.*]` 配置中设置 `api_backend`，以选择模型使用的协议：

| 值 | API | 默认 |
|-------|-----|---------|
| `"chat_completions"` | OpenAI Chat Completions（`/v1/chat/completions`） | 是 |
| `"responses"` | OpenAI Responses（`/v1/responses`） | |
| `"codex_responses"` | OpenAI Codex 兼容 Responses（`/v1/responses`） | |
| `"messages"` | Anthropic Messages（`/v1/messages`） | |

省略 `api_backend` 时，Grok 使用 `chat_completions`。

`codex_responses` 复用 Responses 传输，并启用 Codex 请求形态（instructions、reasoning、缓存、流式响应和工具默认值）。它面向实现 Codex Responses 方言的 BYOK 提供商，仍使用该提供商自己的 API key 与请求头，不会切换到 ChatGPT OAuth。输入时也接受 `codex-responses` 别名，但保存和序列化时使用规范值 `codex_responses`。压缩会优先调用 Codex 的 unary `POST /v1/responses/compact` 端点并持久化其不透明 replacement history；提供商明确返回不支持该端点（例如 404 或 405）时会自动降级为本地摘要压缩。部分中转站会以 `503` 明确报告没有可用的 `-openai-compact` 模型通道，这种情况同样会降级；认证、额度、限流和其他服务端故障仍会作为错误显示。

若需发送提供商专用的认证或版本请求头 —— 例如 Anthropic 的 `x-api-key` —— 请使用下文所述的 `extra_headers` 字段。Grok 会将这些请求头原样附加到发往该端点的每次请求。

---

## 配置自定义模型

在 `~/.grok/config.toml` 的 `[model.<name>]` 小节中添加自定义模型端点：

```toml
[model.my-model]
model = "model-id"                        # Model identifier sent to the API
base_url = "https://api.example.com/v1"   # OpenAI-compatible endpoint
name = "Display Name"                     # Shown in the model picker
description = "Model description"          # Optional description
api_key = "sk-..."                        # API key for this provider (optional)
env_key = "XAI_API_KEY"                   # Env var holding the API key (optional; string or array)
api_backend = "chat_completions"          # chat_completions、responses、codex_responses 或 messages
temperature = 0.7                         # Sampling temperature
top_p = 0.95                              # Nucleus sampling parameter
max_completion_tokens = 8192              # Maximum tokens per response
context_window = 128000                   # Total context window in tokens
extra_headers = { "x-api-key" = "sk-..." } # Extra request headers, sent verbatim (optional)
```

### 凭证解析

Grok 按以下顺序解析 API 密钥：

1. 模型配置中的 `api_key` 字段
2. `env_key` 指定的环境变量 —— 可为单个字符串或名称数组。取第一个已设置且非空的值（例如 `env_key = ["ANTHROPIC_AUTH_TOKEN", "LC_ANTHROPIC_AUTH_TOKEN"]`，便于 SSH 转发 `LC_*`）
3. 你的已登录会话令牌（来自 `grok login`），适用于未配置自身 `api_key`/`env_key` 的模型
4. `XAI_API_KEY` 环境变量（全局回退；Grok 也为向后兼容接受 `GROK_CODE_XAI_API_KEY`）

### 上下文窗口

`context_window` 值用于告知 Grok 何时触发自动压缩（auto-compaction）。覆盖已知模型时，Grok 会继承该模型的上下文窗口。定义新模型且省略 `context_window` 时，Grok 默认使用 200,000 tokens，因此请显式设置以匹配你的提供商。

### 全局默认请求头

若要对目录中的*所有*模型 —— 内置、从 `/v1/models` 预取，或自定义 —— 应用相同请求头，请在全局 `[models]` 小节中一次性设置，无需在每个模型中重复：

```toml
[models]
extra_headers = { "X-Request-Tags" = "team=example,env=prod" }
```

这些请求头会作为每个模型推理请求的基础。按模型设置的 `[model.<id>].extra_headers` 会**按键**覆盖全局默认值（键名大小写不敏感）：模型上设置的键优先生效，仅存在于全局的键仍会被该模型继承。与按模型字段相同，它们只随该模型的推理调用发送 —— 不会用于图像生成或视频生成等独立服务 —— 因此很适合用于归因标签（例如成本追踪），而无需在每次新增模型时重新声明。

### 全局默认值

一些常见的按模型设置也可以在 `[models]` 下一次性设为*每个*模型的默认值。按模型的 `[model.<id>]` 值始终优先；全局值仅在模型（或服务端模型列表）未设置该字段时填补：

```toml
[models]
temperature                 = 0.7
top_p                       = 0.95
max_completion_tokens       = 8192
max_retries                 = 8
inference_idle_timeout_secs = 600
stream_tool_calls           = true
```

这是一组固定的、环境级调节项。用于标识特定模型的设置（`model`、`base_url`、`api_key`、`context_window` 等）不能以这种方式设默认值；另有一些设置拥有独立配置位置 —— 自动压缩（`[session]`）、系统提示标签（`[agent]`），以及推理强度（`[models].default_reasoning_effort`）—— 仍保留在原有位置。

> **关于 `stream_tool_calls` 的说明：** 该选项会影响请求*形态*，而不仅仅是采样参数。少数端点（部分 BYOK 提供商）期望将其保持未设置；若全局 `stream_tool_calls = true` 导致某个模型出问题，可在其 `[model.<id>]` 块中用 `stream_tool_calls = false` 为该模型关闭。

---

## 覆盖内置模型

你可以覆盖内置模型的特定字段，而无需重新定义全部内容。只需指定要更改的字段：

```toml
# Override only the API key for a default model
[model.grok-build]
api_key = "my-api-key"

# Override temperature and add a custom API key
[model.grok-build]
temperature = 0.5
api_key = "sk-custom"
```

覆盖内置模型时，Grok 会从默认配置（含正确的 `base_url`）出发，仅应用你指定的字段。未指定字段继承默认值。

### 优先级顺序

1. 你的配置（`[model.*]`）—— 最高优先级
2. 从远程 `/v1/models` 预取的模型
3. 硬编码默认值 —— 最低优先级

---

## 提供商示例

### Anthropic（Claude）

通过 Anthropic Messages API 直接使用 Claude 模型：

```toml
[model.claude-opus]
model = "claude-opus-4-6"
base_url = "https://api.anthropic.com/v1"
name = "Claude Opus 4.6"
api_backend = "messages"
context_window = 200000
extra_headers = { "x-api-key" = "sk-ant-...", "anthropic-version" = "2023-06-01" }
```

`messages` 后端使用 Anthropic Messages 协议。Anthropic 使用 `x-api-key` 请求头认证，而不是 `Authorization: Bearer`，因此请通过 `extra_headers` 传入密钥，Grok 会原样发送。

### OpenAI（Chat Completions）

```toml
[model.gpt-4o]
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
name = "GPT-4o"
env_key = "OPENAI_API_KEY"
```

`api_backend` 默认为 `"chat_completions"`，因此使用 OpenAI 时无需显式设置。

### OpenAI（Responses API）

若提供商支持较新的 Responses API：

```toml
[model.gpt-4o-responses]
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
name = "GPT-4o (Responses)"
api_backend = "responses"
env_key = "OPENAI_API_KEY"
```

### Codex 兼容 Responses（BYOK）

当提供商实现 Codex Responses 请求方言时，使用 `codex_responses`：

```toml
[model_providers.codex-gateway]
base_url = "https://codex-provider.example/v1"
env_key = "CODEX_PROVIDER_API_KEY"
api_backend = "codex_responses"

[model.codex-byok]
model = "gpt-5-codex"
name = "Codex (BYOK)"
model_provider = "codex-gateway"
```

该后端默认令 `supports_backend_search = true`，并发送 Responses 原生 `web_search` 工具，绝不会发送 xAI 专用的 `x_search`。若需关闭，可在提供商或模型上显式设置 `supports_backend_search = false`。

### Ollama（本地 / 云端）

通过 [Ollama](https://ollama.ai) 运行模型 —— 可用云服务，也可用本地实例。

**内置平台（云端）：** Ollama 作为内置平台可用（id：`ollama`），
默认指向 Ollama Cloud API：`https://ollama.com/v1`。
在环境中设置 `OLLAMA_API_KEY`（或 `GROK_OLLAMA_API_KEY`）：

```bash
export OLLAMA_API_KEY=your-key
```

然后通过 `/model ollama/<model-name>` 选择模型。密钥解析成功后，
**完整的 Ollama Cloud 模型列表会从 `GET /v1/models` 实时同步** ——
离线目录条目仅作密钥就绪前的回退（在此之前，/model 选择器的 All 视图中会以变暗方式显示）。

**本地覆盖：** 设置 `GROK_OLLAMA_BASE_URL` 指向本地实例：

```bash
export GROK_OLLAMA_BASE_URL=http://localhost:11434/v1
```

**自定义模型（备选）：** 也可以手动配置 Ollama：

```toml
[model.ollama-codellama]
model = "codellama"
base_url = "http://localhost:11434/v1"
name = "CodeLlama (Ollama)"
```

请确保 Ollama 正在运行（`ollama serve`），且已拉取模型（`ollama pull codellama`）。

### Together AI

```toml
[model.together-mixtral]
model = "mistralai/Mixtral-8x7B-Instruct-v0.1"
base_url = "https://api.together.xyz/v1"
name = "Mixtral 8x7B"
env_key = "TOGETHER_API_KEY"
```

### Moonshot / Kimi（内置平台）

Moonshot 开放平台为一等公民：目录键如
`moonshot-cn/kimi-k2-turbo-preview`、环境变量 `GROK_MOONSHOT_*_API_KEY`，以及
config.toml 中的 `[platforms.moonshot-cn]` / `[platforms.moonshot-ai]`。完整指南见
[Moonshot 提供商](25-moonshot-providers.md)。

### 本地 OpenAI 兼容服务器

任何实现 OpenAI Chat Completions 或 Responses API 的服务器：

```toml
[model.local-llama]
model = "llama-3.1-70b"
base_url = "http://localhost:8080/v1"
name = "Local Llama"
temperature = 0.8
```

---

## 自定义模型端点

将 Grok 指向自定义的 OpenAI 兼容 `/v1/models` 端点，而不是默认端点。适用于模型位于企业网关或自托管推理服务之后的场景。

### 环境变量

| 变量 | 必需 | 说明 |
|----------|----------|-------------|
| `GROK_MODELS_BASE_URL` | 是 | 推理用基础 URL。Grok 从 `{base_url}/models` 获取模型列表。 |
| `XAI_API_KEY` | 是 | 作为 `Authorization: Bearer` 发送的 API 密钥。Grok 也接受 `GROK_CODE_XAI_API_KEY`。 |
| `GROK_MODELS_LIST_URL` | 否 | 当模型列表 URL 与 `{base_url}/models` 不同时，用于覆盖该 URL。 |

### 设置

```bash
export GROK_MODELS_BASE_URL="https://api.acme.com/v1"
export XAI_API_KEY="xai-..."
grok
```

### 配置文件备选方案

```toml
[endpoints]
models_base_url = "https://api.acme.com/v1"

# Override only the API key for a specific model
[model.grok-build]
api_key = "my-api-key"
```

使用 `[endpoints]` 并配合部分模型覆盖时，Grok 会从 endpoints 配置继承 `base_url`，因此无需在每个 `[model.*]` 小节中重复指定。

### 认证行为

设置 `models_base_url` 后，Grok 使用 API 密钥认证（`Authorization: Bearer`），而不是会话认证。无需 `grok login` —— API 密钥即可。

---

## Web 搜索模型

`web_search` 工具使用独立模型。可通过以下方式配置：

```toml
[models]
web_search = "grok-4.20-multi-agent"
```

或通过环境变量：

```bash
export GROK_WEB_SEARCH_MODEL="grok-4.20-multi-agent"
```

若将本地 `web_search` function 指向自定义模型，还需要对应的 `[model.*]` 条目，以便 Grok 能够访问。提供商托管的（“backend”）搜索与之独立：它只会在 Responses 兼容后端、模型支持该能力且 backend tools 已启用时运行：

```toml
[models]
web_search = "my-custom-model"

[model.my-custom-model]
model = "my-custom-model"
supports_backend_search = true
```

对于 `api_backend = "codex_responses"`，该能力默认开启；显式的 `supports_backend_search = false` 优先。`disable_web_search` 是本地搜索和提供商托管搜索共同的最终关闭开关。

---

## 使用自定义模型

```bash
# List available models (including custom)
grok models

# Use in the TUI via slash command
/model my-model

# Use in headless mode
grok -p "Hello" -m my-model

# Set as default in config.toml:
[models]
default = "my-model"
```

---

## 企业部署

面向企业部署、含自定义模型的完整配置示例：

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

## 故障排查

### 找不到模型

```bash
# List available models
grok models

# Check config.toml for typos in [model.*] sections
```

### 连接错误

验证端点是否可达：

```bash
curl -s https://api.example.com/v1/models \
  -H "Authorization: Bearer $XAI_API_KEY"
```

### 调试日志

```bash
RUST_LOG=debug GROK_LOG_FILE=/tmp/grok.log grok
tail -f /tmp/grok.log
```

查找包含 `model` 或 `sampling` 的日志条目，以追踪模型选择与 API 调用。
