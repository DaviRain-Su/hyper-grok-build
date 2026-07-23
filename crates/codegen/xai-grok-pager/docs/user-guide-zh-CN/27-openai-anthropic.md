# 多提供方目录（官方 Pi）

Grok 的第三方模型目录由官方
[earendil-works/pi](https://github.com/earendil-works/pi) 包
`@earendil-works/pi-ai` 生成（在 `npm run generate-models` 之后位于
`packages/ai/src/providers/data/*.json`）。

**约 400+ 个支持工具调用的模型**，键为 `{platform}/{model_id}`。无需单独的
网关进程——各平台通过 Grok 现有的
`chat_completions` / `responses` / `messages` 后端直接对接厂商 API。

## 平台（除非另有说明，均使用 API key）

| 平台 id | 环境变量键（示例） | 默认 base | 说明 |
|-------------|---------------------|--------------|--------|
| `openai` | `OPENAI_API_KEY` | `api.openai.com/v1` | Responses（大多数模型） |
| `anthropic` | `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` | `api.anthropic.com/v1` | Messages；支持 Claude Code `ANTHROPIC_BASE_URL` 网关 |
| `kimi-code` | OAuth `login --kimi` | `api.kimi.com/coding/v1` | **Messages** + 自适应 thinking（Pi 官方） |
| `moonshot-cn` / `moonshot-ai` | `GROK_MOONSHOT_*` | moonshot.cn / .ai | Chat Completions |
| `deepseek` | `DEEPSEEK_API_KEY` | `api.deepseek.com` | |
| `groq` | `GROQ_API_KEY` | `api.groq.com/openai/v1` | |
| `openrouter` | `OPENROUTER_API_KEY` | `openrouter.ai/api/v1` | 大型目录 |
| `together` / `fireworks` / `cerebras` / `nvidia` | 对应的 `*_API_KEY` | 厂商 URL | Fireworks 的 **Messages** 条目使用 `…/inference/v1`（SDK 根路径会规范化，以便与 Grok 的 `/messages` 拼接） |
| `minimax` / `minimax-cn` | `MINIMAX_API_KEY` | Messages 模型使用 `…/anthropic/v1` | 目录覆盖为 `…/anthropic`；运行时解析为 `…/anthropic/v1`，使请求落到 `…/v1/messages` |
| `zai` | `ZAI_API_KEY` / `GROK_ZAI_API_KEY` | `api.z.ai/api/paas/v4` | 通用 PaaS |
| `zai-coding` | `ZAI_API_KEY` / `GROK_ZAI_CODING_API_KEY` | `api.z.ai/api/coding/paas/v4` | 国际 Coding Plan |
| `zai-coding-cn` | `ZAI_API_KEY` / `GROK_ZAI_CODING_CN_API_KEY` | `open.bigmodel.cn/api/coding/paas/v4` | 国内 Coding Plan |
| `ollama` | `OLLAMA_API_KEY` | `ollama.com/v1` | 云端模型；本地请覆盖 `GROK_OLLAMA_BASE_URL` |
| `xai-direct` | `XAI_API_KEY` | `api.x.ai/v1` | BYOK xAI（相对 Grok 登录会话） |

`mistral` 已预留；Pi 的 Mistral 使用我们尚未实现的专有 conversations API。

另见 [25-moonshot-providers.md](25-moonshot-providers.md) 与
[26-kimi-code.md](26-kimi-code.md)。

---

## 快速开始

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export OPENROUTER_API_KEY=sk-or-...

./target/debug/hyper models | head
./target/debug/hyper -m openai/gpt-5 -p "ping"
./target/debug/hyper -m anthropic/claude-sonnet-4-5 -p "ping"
./target/debug/hyper -m openrouter/openai/gpt-4o -p "ping"
```

```toml
# ~/.grok/config.toml
[platforms.openai]
api_key = "sk-..."

[platforms.anthropic]
api_key = "sk-ant-..."

[platforms.openrouter]
api_key = "sk-or-..."

[models]
default = "anthropic/claude-sonnet-4-5"
```

**凭证优先级：** 环境变量（`GROK_*`，然后是常见别名）>
`[platforms.*].api_key` > 按模型的 `[model.*]`。对于 Anthropic，Claude
Code 的 Bearer 凭证 `ANTHROPIC_AUTH_TOKEN` 优先于
`ANTHROPIC_API_KEY`。

### 复用 Claude Code 网关

Grok 识别 Claude Code 的标准网关变量：

```bash
export ANTHROPIC_BASE_URL="https://gateway.example.com"
export ANTHROPIC_AUTH_TOKEN="..." # Bearer；或使用 ANTHROPIC_API_KEY 作为 x-api-key
```

Claude Code 将 `ANTHROPIC_BASE_URL` 视为网关根路径并追加
`/v1/messages`；Grok 会将该根路径规范化到同一端点。
Grok 专用的 `GROK_ANTHROPIC_BASE_URL` 覆盖优先级更高，且
预期已包含 `/v1`。

若在 shell 启动文件中注释掉这些 export 后 Anthropic 模型仍处于未锁定状态，
请重启启动 Grok 的进程，或先在其 shell 中执行：
`unset ANTHROPIC_AUTH_TOKEN ANTHROPIC_API_KEY ANTHROPIC_BASE_URL`。
仍在运行的父进程会保留旧环境，并传给每一个新的
Grok 子进程。锁定模型仍可在选择器的
**全部**视图中有意查找或搜索到，但无法选中。

---

## 发现：锁定模型、作用域与 `/providers`

无需配置密钥即可查看某平台提供的内容——但完整
目录约有 450 个模型，因此 `/model` 选择器默认带有**作用域**：

- **作用域视图（默认）** 仅列出可用模型（xAI + 密钥已解析的平台）。
  锁定的 BYOK 模型不会挡路。
- **Tab** 切换到全部视图：目录中的每个模型，锁定行变暗
  并标记 🔒，另附一行设置提示（确切的环境变量名 +
  `[platforms.<id>]` 配置表）。选择锁定模型会打印其设置
  说明，而不会切换模型。
- 在任意界面（选择器、内联下拉）**输入查询**也会搜索
  锁定模型——输入 `deepseek` 即可找到它们。
- **^X** 隐藏所选模型（将精确条目持久化到
  `~/.grok/config.toml` 的 `[models].hidden_models`；目录会热重载）。
  在全部视图中，隐藏模型以 🚫 变暗显示，^X 可取消隐藏。

`/providers` 为每个平台显示一行——已配置 ✓ 与锁定 🔒、模型
数量，以及解锁方式（OAuth 订阅使用 `/login kimi`）。

密钥一旦解析（环境变量或配置重载），该平台的模型
即可选中——无需重启，也无需其他开关。选择锁定模型
也会在 agent 侧被拒绝，且凭证缝隙绝不会回落到你的
xAI 会话 token 去访问第三方 base URL。

Kimi Code + Moonshot + Ollama Cloud 在凭证解析后还会额外**实时同步**
其 `/models` 列表，因此新模型（例如完整的
Ollama Cloud 名单）无需等待目录更新即可出现。

---

## 从 Pi 刷新目录

在 [earendil-works/pi](https://github.com/earendil-works/pi) 的检出中：

```bash
cd packages/ai && npm run generate-models -- --pretty
# then re-run the import script used in hyper-grok-build development
# (copies providers/data/*.json → platform_catalog.json)
```

随附文件：

- `xai-grok-models/platform_catalog.json` — 模型
- `xai-grok-models/platform_registry.json` — 平台元数据

我们**不会**在运行时自动拉取 OpenAI/Anthropic/OpenRouter 组织的 `/models`
（体量过大）。Kimi Code + Moonshot 在凭证存在时仍会实时同步。

---

## 说明

- Anthropic：Messages + `x-api-key` + `anthropic-version: 2023-06-01`。
- Kimi For Coding（官方 Pi）：**Messages** 位于 `https://api.kimi.com/coding/v1`，
  模型 `k3` / `k2p7` / `kimi-for-coding-highspeed`，`User-Agent: KimiCLI/1.5`，
  `anthropic-version`，自适应 thinking（`thinking.type=adaptive` +
  `output_config.effort`）。
- OpenAI 旗舰：**Responses**，按 Pi 映射。
- 无需 LiteLLM sidecar。
