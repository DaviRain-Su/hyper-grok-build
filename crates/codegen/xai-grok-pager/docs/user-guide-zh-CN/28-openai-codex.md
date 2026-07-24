# OpenAI Codex (ChatGPT) 订阅

Grok 可通过官方第一方 OAuth 流程使用 **OpenAI Codex** 订阅（ChatGPT Plus/Pro）——协议与官方 Pi 的 `openai-codex` 提供方以及 Codex CLI 相同。无需安装外部 `codex` CLI：Grok 直接与 ChatGPT Codex 后端通信。

| | |
|--|--|
| 平台 ID | `openai-codex` |
| 推理 | `https://chatgpt.com/backend-api/codex`（Responses API，SSE） |
| OAuth 主机 | `https://auth.openai.com` |
| 目录模型 | `openai-codex/gpt-5.6-sol`、`openai-codex/gpt-5.6-terra`、`openai-codex/gpt-5.6-luna`、`openai-codex/gpt-5.5`、`openai-codex/gpt-5.4`、`openai-codex/gpt-5.4-mini`、`openai-codex/gpt-5.3-codex-spark` |
| 协议 | OpenAI Responses API，使用 `store: false`、加密推理、`instructions` 系统提示 |

xAI 登录及其他平台凭据保持独立。Codex 凭据保存在 `~/.grok/auth.json` 的 `oauth/openai-codex` 作用域下，**不会**替换你的 xAI 会话。

---

## 登录

### CLI

```bash
grok login --openai
```

默认启动浏览器登录（PKCE + 回环回调 `127.0.0.1:1455`）；也可手动粘贴授权码 / 重定向 URL。适用于无头或远程环境：

```bash
grok login --openai --device-code
```

会打印验证码与验证 URL（`https://auth.openai.com/codex/device`），可在另一台设备上批准。

### TUI

```
/login openai
```

（也接受 `/login codex` / `/login openai-codex` / `/login chatgpt`）

1. Grok 构建授权 URL 并打开浏览器。
2. 使用 ChatGPT 账户批准；浏览器重定向回 Grok。
3. 若重定向无法到达 CLI（远程虚拟机），改为将重定向 URL 粘贴到提示处。
4. 令牌保存在 `oauth/openai-codex` 下；访问令牌在使用时自动刷新。

### 退出登录

```bash
grok logout --openai
```

---

## 实验性 Live 语音

`/live` 会启动与 Codex Live 模型的长时间全双工语音对话。它不同于
`/voice`：后者仍然只是把语音听写成提示文本。

```text
/live
```

Live 模式可以：

- 同时录制麦克风并播放助手的语音回复；
- 显示实时的用户与助手转录；
- 支持插话，用户在助手说话时开口即可自然打断；
- 把需要编辑代码或调用工具的任务委派给**当前绑定的 Hyper Agent 会话**，
  再把执行进度和最终结果送回语音对话；
- 保留当前提示草稿和光标位置。

按 **Space** 静音或取消静音；按 **Esc** 或 **Ctrl+C** 结束 Live。也可以点击
Live 底栏中的静音/取消静音与停止控件。权限确认、提问等模态窗口打开时仍优先
处理键盘输入。启动 `/live` 会停止 `/voice`，反之亦然，确保两种模式不会争用麦克风。

> **实验性功能：** 此功能使用未公开的 Codex Live 内部协议和
> `gpt-live-1-codex` 模型，并不是公开的 OpenAI Realtime API。OpenAI 可能
> 随时更改或关闭该协议。

Live 始终需要上文所述的 ChatGPT/Codex OAuth 登录，但编码 Agent 可继续
使用任意已配置的供应商或模型。若缺少凭据，请先运行
`grok login --openai`。

普通 Hyper 构建默认开启此功能。管理员可通过分层配置或环境变量关闭：

```toml
# ~/.grok/config.toml 或托管的 requirements.toml
[features]
codex_live = false
```

```bash
GROK_CODEX_LIVE=0 hyper
```

开发/测试环境可用 `GROK_OPENAI_CODEX_BASE_URL` 覆盖信令地址，用
`GROK_CODEX_LIVE_SIDEBAND_BASE` 覆盖 sideband 地址；普通用户不应设置它们。

### 音频要求

- **Linux：** Hyper 依次尝试 PipeWire、PulseAudio 和 ALSA 工具。
- **macOS：** 麦克风和扬声器权限属于运行 Hyper 的终端应用；系统提示时请授权。
- **Windows：** Hyper 通过原生音频后端使用 WASAPI。

若 Live 无法打开音频设备，请按 Esc 结束，检查系统输入/输出设备和权限后重试。
无头环境无法完成依赖真实硬件的 Live 验收。

---

## 使用 Codex 模型

```bash
grok models | grep openai-codex
grok -m openai-codex/gpt-5.6-sol -p "ping"
```

TUI：`/model openai-codex/gpt-5.6-sol`

### 推理强度（Codex 目录）

菜单遵循官方 OpenAI Codex CLI 目录
（`codex-rs/models-manager/models.json` 的 `supported_reasoning_levels`）——
**每个模型有各自的档位阶梯**，并非统一的全局 low/medium/max。

| 模型 | 档位 | 默认 |
|-------|--------|---------|
| `gpt-5.6-sol` | low · medium · high · xhigh · **max** · **ultra** | low |
| `gpt-5.6-terra` | low · medium · high · xhigh · **max** · **ultra** | medium |
| `gpt-5.6-luna` | low · medium · high · xhigh · **max** | medium |
| `gpt-5.5` / `gpt-5.4` / mini | low · medium · high · xhigh | medium |

- **max** — 单代理最大推理深度（`reasoning.effort: "max"`）
- **ultra** — 与 Codex CLI 菜单对应的 UI 档位；请求体与 **max** 相同。官方 Codex 在线路上映射为 **Ultra → Max**
  （`codex-rs/core/src/client.rs` 中的 `reasoning_effort_for_request`）；
  ChatGPT 后端不接受 `effort: "ultra"`。自动任务委派属于未来的客户端侧策略，尚未实现；
  当前选择 Ultra 的行为与 Max 相同。

可通过 `/effort max`、`/effort ultra` 或 `grok --effort ultra` 覆盖
（接受 ultra；请求体使用 `max`）。

`grok codex` 便捷子命令会固定使用 Codex 模型并进入标准流程：

```bash
grok codex                    # interactive TUI on the default Codex model
grok codex -p "fix the bug"   # one-shot headless prompt
grok codex --status           # credential status + model list
```

在未登录状态下运行 `grok codex` 会自动启动浏览器登录
（交互式终端）；选择 `openai-codex/*` 模型但无凭据时，会显示 `/login openai` 提示。

---

## 迁移说明（app-server 移除）

早期构建会启动外部 `codex app-server` 二进制，并需要单独的 `codex login`。该依赖已移除：

* 会话中保存的旧版 `codex:<model>` ID 在选择时会改写为
  `openai-codex/<model>`。
* Codex app-server 线程 ID 无法恢复；请使用 `grok sessions` 管理
  原生会话。
* `--codex-binary` 会被忽略；`--resume <thread>` 会被拒绝并给出指引。

环境变量覆盖（仅用于开发/测试）：`GROK_OPENAI_CODEX_BASE_URL`、
`GROK_OPENAI_CODEX_OAUTH_HOST`。
