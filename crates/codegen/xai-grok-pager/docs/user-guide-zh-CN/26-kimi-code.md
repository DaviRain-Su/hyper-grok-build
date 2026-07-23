# Kimi Code 订阅

Grok 可通过官方设备 OAuth 流程使用 **Kimi Code** 订阅
（与 kimi-cli / Kigi 社区构建版相同的协议）。这是内置多提供商支持的第二阶段。

| | |
|--|--|
| 平台 ID | `kimi-code` |
| 推理 | `https://api.kimi.com/coding/v1` |
| OAuth 主机 | `https://auth.kimi.com` |
| 目录模型 | `kimi-code/k3`、`kimi-code/k2p7`、`kimi-code/kimi-for-coding-highspeed`（K2.7 Hyper Speed） |
| 离线回退 | 相同 ID；登录后通过 `GET …/coding/v1/models` 实时同步 |
| 协议 | **OpenAI Chat Completions**；基址 `https://api.kimi.com/coding/v1` |

xAI 登录与 Moonshot API 密钥彼此独立。Kimi 凭据保存在
`~/.grok/auth.json` 的 `oauth/kimi-code` 作用域下，**不会**替换
你的 xAI 会话。

---

## 登录

### CLI

```bash
grok login --kimi
```

### TUI

```
/login kimi
```

（也接受 `/login kimi-code`）

1. Grok 向 Kimi 请求设备码。
2. 浏览器打开 Kimi Code 授权页（或打印 URL）。
3. 确认用户码，然后返回终端。
4. 令牌存储在 `oauth/kimi-code` 下。

设备身份头（`X-Msh-Device-*`）会随 OAuth 与推理请求一并发送，
与 kimi-cli 一致。访问令牌过期后会在重建模型目录时自动刷新
（且在存在 Tokio 运行时的情况下）。

---

## 使用 Kimi Code 模型

登录后（或下次启动且令牌仍有效时），Grok 会调用
`GET https://api.kimi.com/coding/v1/models`，并将列表合并进
模型目录。**K3** 及其他订阅模型就是这样出现的——
并非只有离线回退项。

```bash
grokk models | grep kimi-code
# typically:
#   kimi-code/k3
#   kimi-code/k2p7                      # Kimi K2.7 Code
#   kimi-code/kimi-for-coding-highspeed # Kimi K2.7 Hyper Speed

grok -m kimi-code/k3 -p "ping"
grok -m kimi-code/k2p7 -p "ping"
```

在 TUI 中：

```
/model kimi-code/k3
```

或设置默认值：

```toml
# ~/.grok/config.toml
[models]
default = "kimi-code/k3"
```

在完成 `grok login --kimi` 之前，订阅模型会在
仅 API 密钥的选择器中保持隐藏（`supported_in_api = false`）；登录后它们会出现，
且凭据会盖印到每个 `kimi-code/*` 条目上。

K3（以及在线路上声明 `think_efforts` 的其他模型）会在 TUI 中暴露
可选的推理级别（`low` / `high` / `max` → `Xhigh`）。

---

## 仅退出 Kimi

```bash
grok logout --kimi
```

这只会清除 `oauth/kimi-code` 作用域。你的 xAI 会话（以及
`XAI_API_KEY`）不受影响。

---

## 环境变量覆盖（开发 / 测试）

```bash
# Must include /v1 — Grok posts to {base}/messages → …/coding/v1/messages.
# Pi-style `…/coding` (no /v1) is auto-normalized to `…/coding/v1`.
export GROK_KIMI_CODE_BASE_URL="https://api.kimi.com/coding/v1"
export GROK_KIMI_CODE_OAUTH_HOST="https://auth.kimi.com"

# Wire backend: `messages` (Anthropic Messages, default) or
# `chat_completions` (OpenAI-compatible, gray-release opt-in while parity
# is validated). Unset / unrecognized values keep the default.
export GROK_KIMI_CODE_API_BACKEND="chat_completions"
```

---

## Moonshot 开放平台 vs Kimi Code

| | Moonshot 开放 API | Kimi Code 订阅 |
|--|-------------------|-------------------------|
| 鉴权 | API 密钥（`GROK_MOONSHOT_*`） | 设备 OAuth（`grok login --kimi`） |
| 主机 | `api.moonshot.cn` / `api.moonshot.ai` | `api.kimi.com/coding` |
| 文档 | [25-moonshot-providers.md](25-moonshot-providers.md) | 本页 |

---

## 请求参数

### 订阅路径（线路后端）

Kimi For Coding 提供两种可选线路后端，由
`GROK_KIMI_CODE_API_BACKEND` 选择：

- **`messages`（默认）** — Anthropic Messages：`POST {base}/messages`，
  带 `anthropic-version: 2023-06-01` 与 `User-Agent: KimiCLI/1.5`。
- **`chat_completions`（可选启用）** — OpenAI Chat Completions：
  `POST {base}/chat/completions`，带 `User-Agent: KimiCLI/1.5`
  （无 `anthropic-version` 头），推理/思考字段映射与
  Moonshot 开放平台相同。

| 关注点 | Grok 中的行为 |
|---------|------------------|
| 基址 URL | `https://api.kimi.com/coding/v1`（环境变量：`GROK_KIMI_CODE_BASE_URL`） |
| 思考 | K3 / K2.7 Code / K2.7 Hyper Speed 保持思考开启；推理力度映射到模型的思考字段 |
| Temperature | 对固定采样模型（K2.7 Code / K2.7 Hyper Speed）省略 |
| `max_tokens` / `max_completion_tokens` | 未设置时默认为 **32768** |

| 模型 ID | 说明 |
|----------|-------|
| `k3` | 1M 上下文；可选推理力度 |
| `k2p7` | Kimi K2.7 Code；256k 上下文 |
| `kimi-for-coding-highspeed` | Kimi K2.7 Hyper Speed |

### 开放平台 Moonshot（Chat Completions）

与订阅路径分离——见
[25-moonshot-providers.md](25-moonshot-providers.md)。简要说明：K3 使用
顶层 `reasoning_effort`；K2.7 省略 K2 的 `thinking` 对象；K2.6 将
力度映射为 `thinking.type`（工具循环另加 `keep: all`）。

**订阅** 主机上的精确模型 ID 来自登录后的实时
`GET …/coding/v1/models`（并非仅依赖开放平台的 ID 表）。

## 说明

- 对第三方订阅 API 的非官方集成；与 Moonshot AI 或 xAI 无关联。
- 仅限托管 xAI 的工具（服务端 web/x 搜索）在 Kimi 上不可用。
- 当目录盖印凭据时（且存在 Tokio 运行时），若访问令牌已超过
  提前过期窗口且存在刷新令牌，会自动执行令牌刷新。

---

## 故障排除

| 现象 | 检查 |
|---------|--------|
| 模型未列出 | 运行 `grok login --kimi` / `/login kimi`；重启 TUI |
| 会话中途出现 “Authentication required… Run /login” | **Kimi 访问令牌约 15 分钟有效。** 在存有刷新令牌时，Grok 会在每次请求时自动刷新。若刷新失败（会话被吊销、网络问题），请再次运行 `/login kimi`——普通 `/login` 只会重新鉴权 xAI。 |
| 推理返回 401 | 用 `/login kimi` 重新登录；检查 `~/.grok/auth.json` 是否仍有带 `refresh_token` 的 `oauth/kimi-code`；时钟偏差 |
| 仅 xAI 失败，Kimi 正常 | `grok login`（xAI）；凭据相互独立 |
| 浏览器未打开 | 复制打印出的 URL；手动完成登录 |
| 设备授权失败 | 能访问 `auth.kimi.com` 的网络；公司代理 |
