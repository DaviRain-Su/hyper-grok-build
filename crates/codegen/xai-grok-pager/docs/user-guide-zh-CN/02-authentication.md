# 认证

Hyper（以及 Grok Build）支持**多种互不绑定的认证方式**。不必只用 SpaceXAI / grok.com：可以混合订阅与 API Key，并随时换模型。

| 方式 | 怎么做 | 常见用途 |
|------|--------|----------|
| SpaceXAI / Grok OAuth | `hyper login` 或欢迎页按 `l` | Grok 托管模型 |
| xAI API Key | `export XAI_API_KEY=…` | CI / 无浏览器 |
| OpenAI Codex（ChatGPT） | `hyper login --openai` / `/login openai` | ChatGPT Plus/Pro 编程 |
| Kimi Code | `hyper login --kimi` / `/login kimi` | Kimi 订阅 |
| BYOK 平台 | `/providers <platform> <key>` 或环境变量 | OpenAI、Anthropic、OpenRouter、Ollama 等 |
| 企业 OIDC / SSO | 配置 / 环境变量 | 企业部署 |

凭据按**不同 scope** 写在 `~/.grok/auth.json`，退出 xAI 不会自动清掉 Codex / Kimi（除非你显式清除）。

---

## 首次启动（Hyper 社区构建）

**Hyper 不会一打开就强制跳 Grok 浏览器登录。** 你会先到欢迎页：

1. 需要默认交互登录时按 **`l`**（通常是 grok.com）。
2. 也可在已有凭据时直接进会话，再用斜杠命令：

```text
/login openai          # ChatGPT Codex OAuth
/login kimi            # Kimi Code OAuth
/providers openrouter <key>
/providers anthropic <key>
/model openrouter/...
```

若 **Grok 免费额度用尽**，弹窗不只逼升级：可选 **切换模型 / 使用 API Key** 或 **Dismiss**，用其他厂商继续工作。Hyper 不会用个人 SuperGrok 订阅 gate 锁死整个 TUI。

官方 `grok` 构建仍可能自动走 SpaceXAI 登录；Hyper（`community-build`）优先多平台自选。

---

## 浏览器登录（SpaceXAI / Grok）

官方 Grok 构建首次启动，或按 `l` / 无参数 `login` 时，会打开浏览器完成 SpaceXAI 认证：

```bash
hyper
# 或
hyper login
```

凭据保存在 `~/.grok/auth.json`，跨会话复用。访问令牌会在后台自动刷新。无法刷新时会提示重新登录。若服务器未提供过期时间，凭据默认按 30 天生命周期处理。

### 凭据存储

`~/.grok/auth.json` 中的令牌（以及 `~/.grok/mcp_credentials.json` 中的 MCP OAuth 令牌）以仅所有者可读写的权限写入（Unix 上为 `0600`）。能访问这些路径的人即可使用凭据，因此：

- 优先启用全盘加密（FileVault、BitLocker、LUKS 等）。
- 不要把 `auth.json` 或 `mcp_credentials.json` 复制到共享目录、工单或聊天中。
- 在多用户主机上，保持 `$HOME` / `$GROK_HOME` 仅对本账户私有。

### 重新认证

要切换账号或排查认证问题，运行：

```bash
grok login
```

`grok login` 会重新走登录流程并替换缓存会话。默认打开浏览器，通过 `auth.x.ai` 的 SpaceXAI OAuth 登录。可用标志选择其他流程：

| 标志 | 说明 |
|------|------|
| `--oauth` | 通过 `auth.x.ai` 的 SpaceXAI OAuth 登录。此为默认，标志可选。 |
| `--device-auth`（别名 `--device-code`） | 设备码流程，适合无头或远程环境。 |

要退出 **xAI** 会话，运行 `hyper logout`（官方二进制则为 `grok logout`）。这只清除主 xAI 会话范围；若仍有 Kimi 或 Codex 会话，CLI 会提示如何清除。

第三方订阅与 BYOK 密钥是分开的：

| 命令 | 清除内容 |
|------|----------|
| `hyper logout` | xAI 会话（交互 OAuth / 缓存会话）；若仍有 Kimi/Codex 会给出提示 |
| `hyper logout --kimi` | 仅 Kimi Code OAuth |
| `hyper logout --openai` | 仅 OpenAI Codex（ChatGPT）OAuth |
| `hyper logout --all` | xAI + Kimi + Codex OAuth（不含 BYOK 平台密钥） |
| `/logout provider <platform>` | 该平台已存储的 BYOK API Key（等同 `/providers clear`） |

`XAI_API_KEY` 等基于环境变量的密钥不会被 logout 移除 —— 请自行取消设置。

---

## API Key

用于 CI/CD、自动化或无浏览器环境时，从 [console.x.ai](https://console.x.ai) 获取 API Key：

```bash
export XAI_API_KEY="xai-..."
grok
```

无活动会话令牌时，Grok 会回退到 API Key。若已交互登录，存储的会话令牌优先。要回退到 API Key，运行 `hyper logout`（仅 xAI 会话）或删除 `~/.grok/auth.json` 中的相关范围。

---

## OIDC（客户 SSO）

通过你自己的身份提供方（IdP，如 Okta、Azure AD、Auth0）认证开发者，而不是 grok.com。

### 1. 在 IdP 中注册公共客户端

- 授权类型：Authorization Code + PKCE（Proof Key for Code Exchange）
- 重定向 URI：`http://127.0.0.1/callback` —— 环回地址。Grok 在登录时绑定随机端口；多数 IdP 按 [RFC 8252](https://tools.ietf.org/html/rfc8252) 将环回重定向视为与端口无关。
- 无 client secret。PKCE 替代 secret。

### 2. 配置 CLI

通过配置文件：

```toml
# ~/.grok/config.toml
[grok_com_config.oidc]
issuer = "https://acme.okta.com"
client_id = "0oa1b2c3d4e5f6g7h8i9"
```

或通过环境变量：

```bash
export GROK_OIDC_ISSUER="https://acme.okta.com"
export GROK_OIDC_CLIENT_ID="0oa1b2c3d4e5f6g7h8i9"
```

也可覆盖 API 端点，指向自建代理：

```bash
export GROK_CLI_CHAT_PROXY_BASE_URL="https://grok-proxy.acme.com/v1"
```

### 3. 运行 `grok`

CLI 通过 `{issuer}/.well-known/openid-configuration` 发现端点，打开 IdP 登录页，并将令牌存入 `~/.grok/auth.json`。令牌会借助存储的 `refresh_token` 静默自动刷新。

### 可选字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `scopes` | `["openid", "profile", "email", "offline_access", "api:access"]` | `offline_access` 启用静默刷新 |
| `audience` | 无 | 部分 IdP（如 Auth0）需要 |

---

## 外部认证提供方

无法使用浏览器登录时 —— 例如沙箱 VM、CI runner 或气隙网络 —— 可将认证委托给外部二进制或脚本。

### 工作原理

```
+--------------+     sh -c     +------------------------+
|     Grok     |-------------->|  your auth binary      |
|              |               |                        |
|  reads       |<-- stdout ----|  prints token          |
|  auth.json   |               |                        |
|              |   (stderr)    |  prints status/URLs    |--> surfaced to user
+--------------+               +------------------------+
```

1. Grok 通过 `sh -c "<command>"` 运行你的命令
2. 你的二进制执行所需认证流程（SSO、设备码、证书交换等）
3. **stderr** 输出人类可读信息，如登录 URL 与状态。Grok 读取 stderr 并展示给用户；在 TUI 中，会把第一个 `https://` URL 变成可点击登录链接。
4. **stdout** 由 Grok 捕获并保存为访问令牌
5. 退出码 0 = 成功；非零 = Grok 回退到交互登录

### stdout / stderr 约定

| 流 | 应打印的内容 | 谁看到 |
|----|--------------|--------|
| **stdout** | 仅令牌，无其他内容 | Grok（解析并存入 auth.json） |
| **stderr** | 登录 URL、状态、错误 | 用户（Grok 读取 stderr，并在 TUI 中把登录 URL 显示为可点击链接） |

**除令牌外不要向 stdout 打印任何内容。** 不要输出进度或调试信息。Grok 读取 stdout，去掉首尾空白，并解析为令牌。

### stdout 令牌格式

**裸字符串** —— 原始令牌：

```
eyJhbGciOiJSUzI1NiIs...
```

**JSON** —— 可含 refresh token、过期与 issuer：

```json
{"access_token": "eyJhbGciOi...", "refresh_token": "ref-tok", "expires_in": 3600, "issuer": "https://idp.example.com"}
```

若令牌会过期且希望 Grok 在过期前自动重新运行二进制，请使用 JSON。

JSON 字段：

| 字段 | 必需 | 含义 |
|------|------|------|
| `access_token` | 是 | Grok 发给 xAI API 的 Bearer 令牌 |
| `refresh_token` | 否 | 仅作参考存储。Grok 通过重新运行你的二进制刷新，而不是 OAuth refresh grant |
| `expires_in` | 否 | 令牌生命周期（秒）；用于过期前主动刷新 |
| `issuer` | 否 | 标识令牌签发方 |

### 配置

通过配置文件：

```toml
# ~/.grok/config.toml
[auth]
auth_provider_command = "/usr/local/bin/my-auth-provider"
auth_provider_label = "Acme Corp"   # 可选 —— 自定义 TUI 登录按钮文案
auth_token_ttl = 3600               # 可选 —— 令牌生命周期（秒）
```

或通过环境变量：

```bash
export GROK_AUTH_PROVIDER_COMMAND="/usr/local/bin/my-auth-provider"
export GROK_AUTH_PROVIDER_LABEL="Acme Corp"
export GROK_AUTH_TOKEN_TTL=3600
```

### 令牌刷新

需要刷新过期令牌时，Grok 会在环境中设置 `GROK_AUTH_EXPIRED=1` 后重新运行你的二进制。每次运行都会完全替换已存凭据，因此每次调用（含刷新）都应输出相同 JSON 字段（如 `issuer`）。二进制可用该变量走更快的静默刷新路径：

```bash
#!/bin/sh
if [ "$GROK_AUTH_EXPIRED" = "1" ]; then
    echo "Refreshing token..." >&2
    TOKEN=$(my-company-auth --refresh --silent)
else
    echo "Authenticating via Acme Corp SSO..." >&2
    TOKEN=$(my-company-auth --login --interactive)
fi

if [ -z "$TOKEN" ]; then
    echo "Authentication failed" >&2
    exit 1
fi

echo "{\"access_token\": \"$TOKEN\", \"expires_in\": 3600}"
```

### 环境变量

| 变量 | 说明 |
|------|------|
| `GROK_AUTH_PROVIDER_COMMAND` | 认证二进制路径 |
| `GROK_AUTH_PROVIDER_LABEL` | TUI 登录屏显示名（如 "Acme Corp"） |
| `GROK_AUTH_TOKEN_TTL` | 令牌生命周期（秒）（用于无 `expires_in` 的裸字符串令牌） |
| `GROK_AUTH_EXPIRED` | 刷新时由 Grok 设为 `1` |
| `GROK_AUTH_EARLY_INVALIDATION_SECS` | 过期前多少秒主动刷新（默认：300） |

---

## 设备码流程

用于本地无浏览器的无头环境（SSH、Docker、远程 VM）：

```bash
grok login --device-auth    # 或: grok login --device-code
```

终端会打印 URL 与代码。在任意设备打开该 URL，输入代码并完成认证。Grok 会轮询直至登录确认。

也可通过 [外部认证提供方](#外部认证提供方) 自行实现设备码流程以获得完全控制。

---

## 自动凭据刷新

Grok 会自动刷新过期凭据：

- **过期前：** 若认证提供方返回了 `expires_in`（JSON 输出）或你设置了 `auth_token_ttl`，Grok 会在过期约 5 分钟前重新运行认证二进制。
- **认证错误时：** 服务器返回 401 Unauthorized 时，Grok 刷新凭据并重试请求。
- **OIDC：** 若有 `refresh_token`，Grok 通过 IdP 静默刷新，无需重新打开浏览器。

调整刷新缓冲：

```bash
# 过期前 5 分钟刷新（默认）
export GROK_AUTH_EARLY_INVALIDATION_SECS=300

# 禁用主动缓冲：到期时或 401 时刷新（设为 0）
export GROK_AUTH_EARLY_INVALIDATION_SECS=0
```

---

## 热重载

Grok 会自动拾取 `~/.grok/auth.json` 的变更。若你在外部更新凭据（例如脚本写入新令牌），下一次 API 调用即可使用新凭据，无需重启。

---

## 认证优先级

每次请求按以下从高到低解析凭据：

1. **按模型的 `api_key` 或 `env_key`** —— 在 `config.toml` 的 `[model.<name>]` 下设置。存在即优先。
2. **活动会话令牌** —— 通过浏览器、OIDC/OAuth2 或外部提供方登录获得，存于 `~/.grok/auth.json`。
3. **`XAI_API_KEY`** —— 无活动会话令牌时的回退。

当配置了多种登录流程时，会话令牌按以下从高到低取第一个可用来源：

1. **外部认证提供方**（`auth_provider_command`）
2. **企业 OIDC** —— 在 `config.toml` 的 `[grok_com_config.oidc]` 或环境变量 `GROK_OIDC_ISSUER` 与 `GROK_OIDC_CLIENT_ID` 中配置时
3. **SpaceXAI OAuth2 浏览器登录** —— 默认

会话进行中，活动方法负责所有中途刷新。

---

## 相关设置

`/privacy` **不会** 改动下列配置项：

| 设置 | 如何设置 |
|------|----------|
| `[features] telemetry` | `config.toml` 或 `GROK_TELEMETRY_ENABLED` |
| `[telemetry] trace_upload` | `config.toml` 或 `GROK_TELEMETRY_TRACE_UPLOAD` |
| 外部 OpenTelemetry | `GROK_EXTERNAL_OTEL` / `[telemetry] otel_*`。见 [用量监控](24-monitoring-usage.md)。 |

团队账号上，只有团队管理员可用 `/privacy` 切换隐私。
团队管理员也可为团队启用或禁用 Zero Data Retention（ZDR）。
见 [如何启用 ZDR](https://docs.x.ai/developers/faq/security#how-to-enable-zdr)。
ZDR 开启时，`/privacy` 无法更改编码数据共享。

见 [用量监控](24-monitoring-usage.md#related-settings) 与 [配置](05-configuration.md#telemetry)。

---

## 故障排除

### 调试日志

用 `RUST_LOG` 控制文件日志与无头 stderr 的详细程度。（TUI 屏幕上的追踪面板使用固定过滤器，忽略 `RUST_LOG`。）TUI 中文件日志默认 `DEBUG`；无头模式（`-p`）下 `RUST_LOG` 默认为 `off`，只打印答案 —— 设置 `RUST_LOG=error`（或更宽）可在 stderr 上看到日志。

在 TUI 中，将 `GROK_LOG_FILE` 设为绝对路径以写入该文件：

```bash
GROK_LOG_FILE=/tmp/grok.log RUST_LOG=debug grok
tail -f /tmp/grok.log
```

`GROK_LOG_FILE` 被视为字面文件路径。相对值如 `1` 会在当前目录写入名为 `1` 的文件。

无头模式下日志走 stderr。可重定向到文件：

```bash
RUST_LOG=debug grok -p "hello" 2> /tmp/grok.log
```

### 常见日志消息

| 日志消息 | 含义 |
|----------|------|
| `auth: running external auth provider` | 正在运行你的二进制 |
| `auth: external auth provider returned fresh token` | 已解析并存储令牌 |
| `auth: external auth provider failed` | 二进制非零退出或 stdout 为空 |
| `auth: external auth provider timed out (likely needs interactive auth), killing` | 超时未退出，已被杀死 |
| `auth: failed to start external auth provider` | 无法启动命令（找不到二进制） |

### 常见修复

- **"Authentication failed"** —— 运行 `grok logout` 清除缓存凭据，再 `grok login` 重新登录。
- **令牌过期过快** —— 设置 `auth_token_ttl`，或在认证提供方 JSON 输出中返回 `expires_in`。
- **OIDC 重定向失败** —— 确保 IdP 允许环回重定向 URI（`http://127.0.0.1/callback`）。
- **找不到外部认证提供方** —— 检查 `auth_provider_command` 路径正确且二进制可执行。
