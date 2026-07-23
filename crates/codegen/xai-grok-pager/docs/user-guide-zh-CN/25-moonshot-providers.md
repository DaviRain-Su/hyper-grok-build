# Moonshot Providers（Kimi 开放平台）

Grok 可通过 API key 调用 **Moonshot AI** 开放平台模型——使用这些模型无需 xAI
登录。这是内置多提供商支持的 Phase 1
（Kimi Code 订阅的 OAuth 另见：[26-kimi-code.md](26-kimi-code.md)）。

目录键采用 `{platform}/{model}` 形式，因此同一 model id 可出现在
多个宿主上。

| Platform id | 区域 / 宿主 | 默认 base URL |
|-------------|---------------|------------------|
| `moonshot-cn` | 中国 | `https://api.moonshot.cn/v1` |
| `moonshot-ai` | 全球 | `https://api.moonshot.ai/v1` |

## 模型（官方产品线）

来源：[platform.kimi.ai Model List](https://platform.kimi.ai/docs/models)
（2026-07）。离线回退列表与此一致；在提供 API key 时，实时 `GET {base}/models`
会替换/扩展该列表。

| Model id | Catalog keys | 上下文 | 说明 |
|----------|--------------|---------|--------|
| `kimi-k3` | `moonshot-cn/kimi-k3`, `moonshot-ai/kimi-k3` | 1M | 旗舰；始终开启思考；`reasoning_effort`（文档：`max`） |
| `kimi-k2.7-code` | `…/kimi-k2.7-code` | 256k | 编程；思考**始终开启** |
| `kimi-k2.7-code-highspeed` | `…/kimi-k2.7-code-highspeed` | 256k | **HyperSpeed**（约 180–260 tok/s）；质量与 2.7 Code 相同 |
| `kimi-k2.6` | `…/kimi-k2.6` | 256k | 思考可开/关 + 保留思考（Preserved Thinking，`thinking.keep`） |
| `kimi-k2.5` | `…/kimi-k2.5` | 256k | 思考可开/关；不支持保留思考 |

仍会在离线环境注入的已弃用别名，供旧配置兼容（优先使用上表）：
`kimi-k2-turbo-preview`、`kimi-k2-thinking-turbo`。

协议：OpenAI **Chat Completions**（`api_backend = "chat_completions"`）。

---

## 请求参数（重要）

Moonshot 文档中有**模型特定**的请求字段。Grok 在构建 chat body 时会应用这些字段
（除非你确认该模型接受，否则不要设置冲突的 `[model.*]` temperature）。

| 字段 | `kimi-k3` | `kimi-k2.7-code`（+ highspeed） | `kimi-k2.6` | `kimi-k2.5` |
|-------|-----------|--------------------------------|-------------|-------------|
| `reasoning_effort` | `"max"`（默认） | 不使用 | 不使用 | 不使用 |
| `thinking.type` | — | 省略（始终开启） | `enabled` / `disabled` | `enabled` / `disabled` |
| `thinking.keep` | — | 服务端始终为 `all` | `null` / `"all"` | 不支持 |
| `temperature` / `top_p` / penalties | 正常 | **固定** — 省略（服务端 1.0 / 0.95 / 0） | 固定 — 省略 | 固定 — 省略 |
| `max_tokens` | 默认 **32768** | 默认 **32768** | 默认 **32768** | 默认 **32768** |
| `tool_choice` | 优先 auto/none | **仅** `auto` / `none` | 优先 auto/none | 优先 auto/none |

多步工具调用必须在 `messages` 中保留 assistant 的 `reasoning_content`
（Grok 的 chat 路径已将 reasoning 映射到该字段）。对于 K2.6 的工具
循环，Grok 会设置 `thinking.keep = "all"`。

详情：[Thinking Mode](https://platform.kimi.ai/docs/guide/use-kimi-k2-thinking-model)、
[K2.7 Code params](https://platform.kimi.ai/docs/guide/kimi-k2-7-code-quickstart)。

---

## 快速开始（环境变量）

```bash
# China open platform
export GROK_MOONSHOT_CN_API_KEY="sk-..."

# or global open platform
export GROK_MOONSHOT_AI_API_KEY="sk-..."

# optional: one key for both platforms
export GROK_MOONSHOT_API_KEY="sk-..."
# also accepted: MOONSHOT_API_KEY
```

```bash
grok models | grep moonshot
grok -m moonshot-cn/kimi-k2.7-code-highspeed -p "ping"
grok -m moonshot-cn/kimi-k3 -p "ping"
# TUI: /model moonshot-cn/kimi-k2.6
```

---

## 配置文件

`~/.grok/config.toml`：

```toml
[platforms.moonshot-cn]
api_key = "sk-..."

[platforms.moonshot-ai]
api_key = "sk-..."

[models]
# optional: make a Moonshot model the session default
default = "moonshot-cn/kimi-k2.7-code"
```

**凭据优先级**（先匹配先生效）：

1. 按模型的 `[model.<id>].api_key` / `env_key`
2. 平台作用域环境变量（`GROK_MOONSHOT_CN_API_KEY` / `GROK_MOONSHOT_AI_API_KEY`）
3. 通用环境变量（`GROK_MOONSHOT_API_KEY` 或 `MOONSHOT_API_KEY`）
4. 通过 `/providers moonshot-cn <api_key>` 在 UI 中粘贴的 key（存储在
   `~/.grok/auth.json` 的 `platform/moonshot-cn` 下）
5. config.toml 中的 `[platforms.<id>].api_key`

### TUI 配置

```
/providers
/providers moonshot-cn sk-...
/model moonshot-cn/kimi-k3
```

清除（登出）已存储的 key：

```
/providers clear moonshot-cn
/providers logout moonshot-cn
/providers moonshot-cn clear
```

注意：环境变量仍优先于已存储的 key。清除后，若模型仍保持可用，还需
`unset GROK_MOONSHOT_CN_API_KEY` / `MOONSHOT_API_KEY`。

API key 绝不会写回重新序列化的配置导出中，也不得提交到共享仓库。

---

## 实时目录同步

当平台 API key 存在且 `remote_fetch` 已启用时，Grok 会调用
`GET {base}/models`，并将编程系列模型（`kimi-k*`）合并进
目录。实时条目会覆盖离线回退项（上下文窗口、思考
力度、显示名称）。

---

## Moonshot 与 Kimi Code 订阅对比

| | Moonshot 开放 API | Kimi Code 订阅 |
|--|-------------------|-------------------------|
| 鉴权 | API key | Device OAuth（`grok login --kimi`） |
| 宿主 | `api.moonshot.cn` / `api.moonshot.ai` | `api.kimi.com/coding` |
| Model ids | `kimi-k3`、`kimi-k2.7-code`、… | 实时列表（常见如 `k3`、`kimi-for-coding`、…） |
| 文档 | 本页 | [26-kimi-code.md](26-kimi-code.md) |
