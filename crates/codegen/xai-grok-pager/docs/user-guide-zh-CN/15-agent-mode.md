# Agent 模式（ACP）与 IDE 集成

Agent 模式将 Grok 作为 ACP（Agent Client Protocol）服务器运行，用于与 IDE、编辑器及自定义工具集成。与单次提示模式（`grok -p`，打印一次响应后退出）不同，Agent 模式会保持进程持续运行，并通过结构化 JSON-RPC 消息进行通信。

---

## 什么是 ACP？

[Agent Client Protocol（ACP）](https://agentclientprotocol.com) 是一套 AI Agent 通信标准。它定义了客户端（IDE、编辑器、自定义应用）如何通过结构化 JSON-RPC 协议与 AI Agent 交互。ACP 提供：

- **会话管理** -- 创建、加载与恢复对话
- **提示提交** -- 发送用户消息并接收流式响应
- **工具可见性** -- 实时查看 Agent 正在使用的工具
- **思考流** -- 观察 Agent 的推理过程
- **权限处理** -- 交互式批准或拒绝工具执行

---

## stdio 传输

stdio 是主要的集成模式。Agent 通过 stdin 和 stdout 交换 JSON-RPC 消息：

```bash
grok agent stdio
```

使用此模式的客户端包括：

- IDE 扩展（例如 Zed、Neovim 和 Emacs）
- 自定义自动化工具
- ACP 客户端库

### 选项

这些选项属于 `grok agent` 命令，适用于所有模式。请在模式名称之前传入，例如 `grok agent --model grok-build stdio`。`stdio` 子命令本身不接受选项。

| 标志                       | 说明                                                              |
| -------------------------- | ---------------------------------------------------------------- |
| `-m, --model <MODEL>`      | 设置模型 ID（例如 `grok-build`）。                               |
| `--always-approve`         | 自动批准每次工具执行。（别名：`--yolo`。）                       |
| `--reauth`                 | 在启动 Agent 前先运行身份认证。                                  |
| `--agent-profile <PATH>`   | 从文件加载 Agent 配置文件（profile）。                           |

---

## 服务器模式

将 Agent 作为 WebSocket 服务器运行，供远程客户端连接：

```bash
grok agent serve --bind 127.0.0.1:2419 --secret <token>
```

客户端通过 WebSocket 连接，并使用密钥令牌进行身份验证。若省略 `--secret`，Agent 会在启动时生成令牌并打印；也可以通过 `GROK_AGENT_SECRET` 环境变量提供。Agent 在重连之间保持状态，因此客户端可以断开连接，之后再恢复进行中的工作。

---

## WebSocket 中继

若要在互联网而非本地网络上访问 Agent，可运行 WebSocket 中继服务器，并让 Agent 连接到它：

```bash
grok agent headless --grok-ws-url wss://your-relay.example.com/ws
```

Agent 主动连接到你的中继，Web 客户端也连接到同一中继。这适用于构建 Web UI 的场景——浏览器无法生成本地进程。

---

## ACP 协议基础

通信遵循 JSON-RPC 2.0 格式。典型会话生命周期：

1. **初始化** -- 客户端发送带能力声明的 `initialize`
2. **创建会话** -- 客户端发送带工作目录的 `session/new`
3. **发送提示** -- 客户端发送带用户消息的 `session/prompt`
4. **接收更新** -- Agent 发送带流式内容的 `session/update` 通知
5. **处理权限** -- Agent 可能请求工具执行批准

### 架构

```
+------------------------------------------+
|           ACP Client                     |
|  (IDE, Editor, Custom Application)       |
+-------------------+----------------------+
                    | JSON-RPC over stdio
+-------------------v----------------------+
|           grok agent stdio               |
|                                          |
|  +---------+  +---------+  +---------+   |
|  | Session |  |  Tools  |  |   MCP   |   |
|  | Manager |  | Registry|  | Servers |   |
|  +---------+  +---------+  +---------+   |
+------------------------------------------+
```

---

## 流式更新

ACP 以结构化事件的形式进行流式传输。每条 `session/update` 通知都带有 `sessionUpdate` 字段，用于标识更新类型：

| `sessionUpdate` 值        | 说明                                                   |
| --------------------- | ----------------------------------------------------- |
| `agent_message_chunk` | Agent 响应文本的一个分块。                             |
| `agent_thought_chunk` | Agent 内部推理的一个分块。                             |
| `tool_call`           | 一次新的工具调用（标题、类型、状态、输入）。           |
| `tool_call_update`    | 进行中工具调用的状态或结果更新。                       |
| `plan`                | Agent 的执行计划。                                     |

每次更新都会标明其类型，因此客户端可以为推理、工具调用和响应文本渲染不同的面板。

---

## 扩展方法

在基础 ACP 协议之外，Grok 在 `x.ai/` 前缀下定义了扩展方法，用于 SpaceXAI 特有功能。涵盖：

| 类别                       | 前缀                 | 示例                                             |
| -------------------------- | -------------------- | ------------------------------------------------ |
| **文件系统**               | `x.ai/fs/*`          | `list`、`exists`、`read_file`、`write_file`      |
| **Git**                    | `x.ai/git/*`         | `status`、`stage`、`commit`、`diffs`、`discard`  |
| **Git Worktree**           | `x.ai/git/worktree/*`| `create`、`remove`、`apply`、`list`、`gc`        |
| **搜索**                   | `x.ai/search/*`      | `fuzzy/open`、`fuzzy/change`、`content`          |
| **终端**                   | `x.ai/terminal/*`    | `create`、`kill`、`output`、`wait_for_exit`      |
| **会话管理**               | `x.ai/session/*`     | `fork`、`resolve_local_for_worktree_resume`      |
| **对话与历史**             | `x.ai/*`             | `prompt_history`、`rewind/*`、`compact_conversation` |
| **身份认证**               | `x.ai/auth/*`        | `get_url`、`submit_code`                         |
| **反馈与遥测**             | `x.ai/*`             | `feedback`、`telemetry/*`                        |

此处表格展示的是各类别的代表性方法。`x.ai/*` 集合为 SpaceXAI 特有，可能随版本扩展，因此应将其视为非穷尽列表，并从 Agent 的 `initialize` 响应中发现可用方法。

### 通知（Agent 到客户端）

Agent 会向客户端发送推送通知，以提供实时更新：

| 通知                       | 说明                                 |
| -------------------------- | ------------------------------------ |
| `x.ai/search/fuzzy/status` | 模糊搜索结果更新                     |
| `x.ai/git/worktree/status` | Worktree 创建进度                     |
| `x.ai/fs_notify`           | 文件系统变更通知                     |
| `x.ai/fs/index`            | 完整文件索引更新                     |
| `x.ai/fs/index/delta`      | 增量文件索引更新                     |
| `x.ai/session_notification`| 会话相关更新（diff 审阅、重试状态、自动压缩） |
| `x.ai/session/update`      | 会话更新（工具调用、内容）           |

---

## 会话 `_meta` 选项

`session/new` 请求接受以下可选 `_meta` 字段：

| 字段                   | 说明                                           |
| ---------------------- | ---------------------------------------------- |
| `rules`                | 追加到系统提示的额外规则。                     |
| `systemPromptOverride` | 用于替换的系统提示。                           |
| `agentProfile`         | Agent 配置文件，可以是名称或 JSON 对象。       |

---

## ACP SDK

官方 SDK 库支持多种语言：

| 语言       | 包                                                                                       |
| ---------- | ---------------------------------------------------------------------------------------- |
| TypeScript | [`@agentclientprotocol/sdk`](https://www.npmjs.com/package/@agentclientprotocol/sdk)     |
| Rust       | [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol)                |
| Python     | [`agent-client-protocol-python`](https://github.com/PsiACE/agent-client-protocol-python) |
| Go         | [`acp-go-sdk`](https://github.com/coder/acp-go-sdk)                                     |
| Kotlin     | [`acp`](https://github.com/agentclientprotocol/kotlin-sdk)                               |

---

## 兼容客户端

| 客户端                                                   | 状态        |
| -------------------------------------------------------- | ----------- |
| [Zed](https://zed.dev/docs/ai/external-agents)           | 已支持      |
| [Neovim](https://neovim.io)（CodeCompanion、avante.nvim） | 已支持      |
| [Emacs](https://github.com/xenodium/agent-shell)         | 已支持      |
| [marimo notebook](https://github.com/marimo-team/marimo) | 已支持      |
| JetBrains                                                | 即将推出    |

---

## 集成示例：TypeScript ACP 客户端

```typescript
import { spawn, ChildProcess } from "child_process";
import * as readline from "readline";

class GrokACPChat {
  private proc!: ChildProcess;
  private sessionId!: string;
  private rl!: readline.Interface;

  constructor(private cwd = ".") {}

  async init() {
    this.proc = spawn("grok", ["agent", "stdio"]);
    this.rl = readline.createInterface({ input: this.proc.stdout! });

    // Initialize
    await this.request("initialize", {
      protocolVersion: 1,
      clientCapabilities: {
        fs: { readTextFile: true, writeTextFile: true },
        terminal: true,
      },
    });

    // Create session
    const { sessionId } = await this.request("session/new", {
      cwd: this.cwd,
      mcpServers: [],
    });
    this.sessionId = sessionId;
    return this;
  }

  private async request(method: string, params: any): Promise<any> {
    return new Promise((resolve) => {
      const msg = JSON.stringify({ jsonrpc: "2.0", id: 1, method, params });
      this.proc.stdin!.write(msg + "\n");

      this.rl.once("line", (line) => {
        resolve(JSON.parse(line).result || {});
      });
    });
  }

  async *streamPrompt(text: string) {
    const msg = JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "session/prompt",
      params: {
        sessionId: this.sessionId,
        prompt: [{ type: "text", text }],
      },
    });
    this.proc.stdin!.write(msg + "\n");

    for await (const line of this.rl) {
      const data = JSON.parse(line);

      if (data.method === "session/update") {
        const update = data.params.update;
        yield update; // { sessionUpdate, content, title, ... }
      } else if (data.result) {
        break; // Final response
      }
    }
  }
}

// Usage
const client = await new GrokACPChat(".").init();

for await (const update of client.streamPrompt("List the files in this project")) {
  switch (update.sessionUpdate) {
    case "agent_message_chunk":
      process.stdout.write(update.content?.text || "");
      break;
    case "agent_thought_chunk":
      console.log(`\n[Thinking: ${update.content?.text}]`);
      break;
    case "tool_call":
      console.log(`\n[Tool: ${update.title}]`);
      break;
  }
}
```

---

## 相关资源

- [ACP 规范](https://agentclientprotocol.com/protocol/prompt-turn)
- [协议简介](https://agentclientprotocol.com/overview/introduction)
