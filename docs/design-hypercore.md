# Design: Hypercore（可插拔 Agent Core + Host 扩展）

| 项 | 内容 |
|----|------|
| 状态 | **Accepted · Phase 0+1 已落地；ShellHyperHost 已接入骨架** |
| 日期 | 2026-07-31 |
| 动机 | 从完整 Hyper CLI/TUI 中抽出可复用的会话核心，支撑本机、Cloudflare Durable Objects、以及未来其它 edge/server host，而不要求「整包 Hyper 上云」 |
| 对标 | [nanocodex PR #75](https://github.com/gakonst/nanocodex/pull/75)（host-managed transport）+ [PR #76](https://github.com/gakonst/nanocodex/pull/76)（Cloudflare DO） |
| 相关 | [design-wasm-extensions.md](design-wasm-extensions.md)（插件 WASM ≠ core WASM）；社区 fork `hyper` 二进制与 ACP |
| 产物落点 | 本文档 + 实现时新 crate：`xai-hyper-core`、`xai-hyper-host`（trait）、可选 `examples/cloudflare-workers` |

---

## 0. 一句话不变式

**Hypercore 只负责「会话状态 + turn 编排 + 模型协议」；一切 I/O（凭证、出站连接、持久化、真实文件系统/终端、鉴权入口）经版本化 `HyperHost` trait 由宿主实现。同一套 core 可挂本机进程、Durable Object、或远程 sidecar。**

---

## 1. 问题陈述

### 1.1 产品意图

- 希望 Hyper 能像 nanocodex 一样：**会话可托管在 Cloudflare（或其它边缘）**，浏览器/REPL 只持 capability + UI 状态。
- 完整 Hyper（TUI、PTY、git、MCP stdio、wasmtime 插件宿主、leader）**不能**原样塞进 Workers/DO。
- 需要一条 **窄而稳的核心路径**，让后续 host 可插拔，而不是为每个平台 fork 一套 agent。

### 1.2 现状（Hyper 单体）

| 层 | 主要落点 | 与 edge 的关系 |
|----|----------|----------------|
| TUI / 输入输出 | `xai-grok-pager` | 本机 only |
| ACP agent / session | `xai-grok-shell` | 本机进程假设强 |
| 采样 / 模型 wire | `xai-grok-sampler` + `sampling-types` | 可复用核心 |
| 对话状态 | `xai-chat-state` | 可复用核心 |
| 工具 | `xai-grok-tools` + terminal/PTY | 多数绑本机 |
| 插件 | wasmtime guest（扩展） | 与「core 自身 WASM」正交 |
| 凭证 | `auth` + 本机 `auth.json` | 必须留在 host |

### 1.3 对标 nanocodex 的结构差

```text
Nanocodex:  小 runtime → 先 WASM + host transport → 再挂 CF / Rivet
Hyper:      大 CLI/shell → 必须先「切 core」→ 再谈 edge host
```

我们难在 **拆**，不在「会不会写 Workers」。

---

## 2. 目标与非目标

### 2.1 目标

1. **可定义、可编译的 `hyper-core`**：单会话、多 turn、流式输出、可序列化快照。
2. **稳定 `HyperHost` ABI/trait**：凭证、模型传输、存储、可选 tool 执行全部经 host。
3. **三阶段交付**（见 §5），每阶段有可演示产物，不为「完整 Hyper on CF」阻塞首版。
4. **双栈共存**：现有 `hyper` TUI/shell **不砍**；core 是旁路或逐步内嵌，避免大爆炸 rewrite。
5. **为扩展预留**：同一 Host trait 可实现 `NativeHost`、`CloudflareDoHost`、`RemoteAcpHost` 等。
6. **安全默认**：secret 永不进入 core 快照/事件；client 只持 session capability。

### 2.2 非目标（明确不做或后置）

| 项 | 说明 |
|----|------|
| 把完整 shell/TUI 编进 WASM | 体积与 API 均不可行 |
| Phase 1 完整 MCP / PTY / git / 子 agent | 后置；先文本 turn |
| Phase 1 替换现有 `hyper` 默认入口 | 默认仍是本机 TUI |
| 在 DO 内再嵌 wasmtime 跑插件 | edge 上不做嵌套运行时 |
| 保证 CF 直连 ChatGPT WebSocket | 已知 403；需 host 侧 relay（与 nanocodex 一致） |
| 与 nanocodex 二进制兼容 | 协议可借鉴，不保证 wire 互通 |

---

## 3. 架构总览

### 3.1 分层

```text
┌─────────────────────────────────────────────────────────────┐
│  Clients（可替换）                                          │
│  浏览器 · REPL · 未来移动端 ·（可选）现有 TUI 旁路入口        │
└───────────────────────────┬─────────────────────────────────┘
                            │ capability URL / ACP-ish JSON
┌───────────────────────────▼─────────────────────────────────┐
│  Hosts（可替换，实现 HyperHost）                            │
│  NativeHost │ CloudflareDoHost │ RemoteSidecarHost │ …     │
│  · 鉴权 · 出站 WS/HTTP · SQLite/DO 存储 · 限流 · idle 卸载   │
└───────────────────────────┬─────────────────────────────────┘
                            │ trait / WIT / FFI（版本化）
┌───────────────────────────▼─────────────────────────────────┐
│  Hypercore（平台无关逻辑）                                  │
│  Session · Turn 状态机 · Prompt 编排 · 采样协议适配           │
│  Snapshot encode/decode · 幂等 turn 结果 · 事件流            │
│  （可选）Host-tool 调用描述，不直接执行 OS 能力               │
└─────────────────────────────────────────────────────────────┘
```

### 3.2 与现有 crate 的映射（实现时）

| 新模块 | 职责 | 优先抽取来源 |
|--------|------|----------------|
| `xai-hyper-core` | 会话 + turn + 快照 + 事件 | `session` turn 路径、`chat-state`、compaction 的只读策略 |
| `xai-hyper-host` | `HyperHost` trait + 类型 | 新建；不塞 shell |
| `xai-hyper-core` 依赖 | `sampling-types`、`sampler`（裁剪 feature）、`serde` | 现有 crate + feature gate |
| `examples/native-host` | 本机 tokio host 冒烟 | 新建 |
| `examples/cloudflare-workers` | DO + Worker 路由（Phase 2+） | 对标 nanocodex example，**不**抄进 monorepo 主二进制 |

**原则：** core **禁止**依赖 `xai-grok-pager`、`portable-pty`、`git2`、`process-wrap`、本机 `rusqlite`（持久化由 host 提供）、`wasmtime`。

### 3.3 数据流（单 turn）

```text
Client                Host                         Core
  │                     │                            │
  │── turn{id,text} ───►│                            │
  │                     │── begin_turn(id) ─────────►│
  │                     │◄─ NeedModelOpen / Ready ───│
  │                     │── open model stream ───────│ (host 持 token)
  │                     │── ModelChunk / Done ──────►│
  │◄── assistant_delta ─│◄── Event::Delta ───────────│
  │◄── turn_done ───────│◄── Event::TurnCommitted ───│
  │                     │── persist snapshot ────────│ (host 写 SQLite/DO)
```

**幂等：** 同一 `turn_id` 已提交 → host 直接返回终端结果，不二次调用模型（对齐 nanocodex terminal turns）。

---

## 4. 核心契约（草案）

### 4.1 `HyperHost`（Rust trait 草图）

实现语言：Phase 1 以 **Rust async trait** 为主；Phase 2 若 core 编译为 WASM，则 **host 在 TS/Worker，core 通过 host imports**（WIT 或手写 bindgen），trait 语义保持一致。

```rust
/// 版本化：HYPER_HOST_API = 1
#[async_trait]
pub trait HyperHost: Send + Sync {
    /// 打开/复用到模型后端的流（凭证仅存 host）。
    async fn open_model_stream(
        &self,
        req: ModelStreamRequest,
    ) -> Result<Box<dyn ModelStream>, HostError>;

    /// 原子写入会话快照 + 可选终端 turn 记录。
    async fn commit_snapshot(
        &self,
        session_id: &str,
        snapshot: &[u8],
        terminal: Option<&TerminalTurnRecord>,
    ) -> Result<(), HostError>;

    /// 读取最近快照（冷启动 restore）。
    async fn load_snapshot(&self, session_id: &str) -> Result<Option<Vec<u8>>, HostError>;

    /// 可选：执行 host 侧工具（读文件、HTTP 等）。Phase 1 可空实现。
    async fn invoke_tool(
        &self,
        call: HostToolCall,
    ) -> Result<HostToolResult, HostError> {
        Err(HostError::Unsupported("tools"))
    }

    /// 可观测性 / 时钟 / 随机（便于测试注入）。
    fn now_unix_ms(&self) -> u64;
}
```

### 4.2 Core 对外 API（草图）

```rust
pub struct HyperCore<H: HyperHost> { /* host, session, config */ }

impl<H: HyperHost> HyperCore<H> {
    pub async fn restore_or_new(host: H, session_id: SessionId) -> Result<Self, CoreError>;
    pub async fn submit_turn(&mut self, turn: TurnRequest) -> Result<TurnHandle, CoreError>;
    pub fn subscribe(&self) -> impl Stream<Item = CoreEvent>;
    pub fn export_snapshot(&self) -> Result<Vec<u8>, CoreError>;
}
```

### 4.3 快照与事件（可扩展）

**Snapshot（逻辑字段，序列化用 JSON 或 postcard + schema 版本）：**

| 字段 | 说明 |
|------|------|
| `schema_version` | 从 1 起 |
| `session_id` | 稳定 id |
| `items` | 对话 items（复用/裁剪 `sampling-types` ConversationItem） |
| `completed_turns` | 计数 |
| `model` / `api_backend` 摘要 | 不含 secret |
| `extensions` | `Map<String, Value>` 预留，未知字段向前兼容 |

**CoreEvent（client 可见子集）：**

- `Status` / `AssistantDelta` / `TurnStarted` / `TurnCommitted` / `TurnFailed` / `ToolCall*`（后期）
- **禁止** 出现 bearer、refresh token、完整 `Authorization` 头

### 4.4 配置（core 侧）

仅逻辑配置：

- 默认 model id、backend 类型（chat_completions / responses / …）
- 上下文窗口策略（简单截断先于完整 compaction）
- 最大并行 turn、最大 transcript 条目

密钥、base URL 覆盖、代理、extra CA → **host 配置**，不进 snapshot。

### 4.5 与现有 ACP 的关系

| 阶段 | 策略 |
|------|------|
| Phase 1 | Core 用 **自有 JSON 事件**（实现快）；不要求完整 ACP |
| Phase 2 | 可选 **ACP 适配层**（`session/new`、`session/prompt` 映射到 core） |
| 长期 | 本机 shell 可选择「内嵌 core」或继续现路径；避免双逻辑永久分裂 |

**不要**在 Phase 1 强行让 DO 实现完整 `hyper agent stdio`。

---

## 5. 三阶段实现计划

### Phase 0 — 边界冻结与骨架（设计落地） ✅

**目标：** 文档 + crate 骨架 + CI 编译空实现；无真实模型。

| 交付物 | 说明 |
|--------|------|
| 本文档合入 `docs/` | Accepted |
| `crates/codegen/xai-hyper-host` | `HyperHost` + `HYPER_HOST_API` + 错误/流类型 |
| `crates/codegen/xai-hyper-core` | `HyperCore`：restore / submit / snapshot |
| `MockHost`（`xai_hyper_core::mock`） | 内存 snapshot + terminal + echo stream |
| 单测 | restore → submit → commit → 幂等 `turn_id` |

**验收：** `cargo test -p xai-hyper-core` 全绿；不依赖 pager/shell。 **已通过。**

**工期量级：** 约 3–7 天（熟练贡献者）。

---

### Phase 1 — 可聊天的 native core（真实模型） ✅

**目标：** 本机 tokio host 上真实完成多 turn 流式对话 + 磁盘快照。

| 交付物 | 说明 |
|--------|------|
| 接入 `xai-grok-sampler`（feature `native`） | `chat_completions` / `responses` / `codex_responses` / `messages` |
| `NativeHost` | env / `auth.json` 读 key；`~/.grok/hypercore/<session>/` |
| CLI | `hypercore-demo`：stdin 多轮，流式打印 delta |
| 幂等 terminal turn 文件 | `terminals/<turn_id>.json` |
| 基础截断 | `max_messages` 丢最旧非 system |

**明确不做：** PTY、MCP、子 agent、插件、TUI。

**验收：**

1. 连续 3 轮真实模型对话 — 用 `hypercore-demo` + `XAI_API_KEY` 手测；  
2. 进程退出后 `restore` 仍保留历史 — demo 重启会打印 restored turns；  
3. 重复同一 `turn_id` 不二次开流 — 单测 `native_disk_snapshot_and_terminal_idempotent`。

**实现注记：** core 仍用精简 `TranscriptItem`；host 打开流时再转成 `ConversationRequest`。

---

### Phase 2 — 可插拔 Host + Cloudflare（或其它 edge）可演示

**目标：** 同一 core 语义挂上非本机 host；**至少一种** edge/demo 路径。

#### 2A. Host 抽象硬化

- `HyperHost` 稳定到 v1（changelog 约束）
- `RemoteSidecarHost`（可选）：Worker 只转发，core 跑在侧车容器（**最快上 CF 门面**）
- 指标：open_stream 延迟、commit 失败率、幂等命中率

#### 2B. Cloudflare Durable Objects（对齐 nanocodex 形态）

```text
Browser ──WSS──► Worker（admin token → session capability）
                    │
                    ▼
              Session DO
                ├─ 持 Hypercore 状态（WASM 或 侧车 RPC）
                ├─ SQLite：snapshot + terminal turns
                ├─ 上游模型：host 出站（必要时 egress relay）
                └─ idle alarm：卸载 / 下次 restore
```

**两条实现子路径（按难度）：**

| 路径 | 做法 | 难度 | 说明 |
|------|------|------|------|
| **2B-lite** | DO 只做路由+存储；core 在后端 HTTP/WSS sidecar | 中 | 先交付「上 Cloudflare」体验 |
| **2B-full** | core 编为 `wasm32` 进 DO（类 nanocodex） | 高 | 需 Phase 1 core 极瘦 + host imports |

**Cloudflare 必记约束（写入实现 checklist）：**

1. ChatGPT / 部分上游可能 **403 bot/egress** → 提供可选 **subscription egress relay**（仅转发约定头与帧，不落盘 refresh token）。  
2. DO 有出站 WS 时难 hibernate → **idle 超时** 关模型连接并 `commit_snapshot`。  
3. Client WS 用 hibernation API；capability URL 当 bearer，本地 state 文件 mode `0600`。  
4. 生产 bundle 体积与 CPU 时间闸门（CI 记录 KiB / 冒烟 turn 数）。

**验收（2B-lite 即可称 Phase 2 done）：**

1. 浏览器或 REPL 完成 ≥3 轮；  
2. detach 后重连同 session，历史仍在；  
3. 重复 turn_id 不重复计费/打模型；  
4. 文档说明 2B-full 的后续条件。

**工期量级：** 2B-lite 约 2–4 周；2B-full 在 Phase 1 质量够的前提下再 1–2 月。

---

### Phase 3 — 能力扩展（不阻塞 Phase 2 发布）

按优先级增量，**一律经 Host 或 core 纯逻辑**，禁止偷偷 `std::process`：

| 优先级 | 能力 | Host 扩展点 |
|--------|------|-------------|
| P1 | 只读文件 / 工作区列表 | `invoke_tool` |
| P1 | 更好的截断 / 轻量 compact | core |
| P2 | 有限 MCP（HTTP + 已有 OAuth token） | host 出站 |
| P2 | ACP 适配层 | host 或 adapter crate |
| P3 | 子 agent（需独立 session + 配额） | host 生成 child session |
| P3 | 与本机 shell 共享 transcript 格式 | 转换器，非强绑定 |

**扩展原则：** 新能力 = 新 `HostTool` 名或新 `CoreEvent` 变体 + `schema_version` 规则；未知事件 client 忽略。

---

## 6. 可扩展性设计（为「后面」留钩子）

### 6.1 版本与兼容

| 通道 | 规则 |
|------|------|
| `HYPER_HOST_API` | 破坏性变更 +1；host 与 core 协商 |
| Snapshot `schema_version` | 只增字段；读取时忽略未知 key |
| Client protocol | 带 `min_protocol`；旧 client 可只收 delta/done |

### 6.2 Feature flags（crate）

```toml
# xai-hyper-core
[features]
default = ["native-demo"]
sampler = ["dep:xai-grok-sampler"]   # Phase 1
json-schema-tools = []               # Phase 3
# 无 default 的 "full-shell"
```

### 6.3 多 Host 注册（逻辑）

```text
HostRegistry
  · native (default for demos)
  · cloudflare-do (example)
  · remote-sidecar
  · mock (tests)
```

CI：mock 必跑；native 集成测 optional（需 key）；CF 用 vitest pool workers + mock 上游。

### 6.4 与 WASM 扩展的边界（避免概念混淆）

| 概念 | 是什么 | 不是什么 |
|------|--------|----------|
| **Hypercore** | 会话/turn 引擎 | 不是第三方插件格式 |
| **WASM extensions**（现有设计） | shell 内插件 | 不是把 core 搬上 CF |
| **core-as-WASM**（Phase 2B-full） | core 编译目标之一 | 不等于启用全部插件 |

---

## 7. 安全与隐私

1. **Secret 隔离：** access/refresh token、API key **仅 host 内存或 host 机密存储**；不进 snapshot、不进 client 事件、不进 core 日志默认字段。  
2. **Capability：** session WebSocket URL = bearer；admin token 仅创建 session。  
3. **幂等与重放：** 防重复扣费；不承诺跨进程 exactly-once 推理（与 nanocodex 一致：commit 前崩溃则从快照重放请求）。  
4. **工具：** Phase 1 无工具；Phase 3 默认 deny，白名单 enable。  
5. **多租户：** 一 session 一 DO/目录；禁止跨 session 读 snapshot。

---

## 8. 测试策略

| 层 | 内容 |
|----|------|
| 单元 | 状态机、幂等、快照 round-trip、schema 向前兼容 |
| 契约 | `MockHost` 记录 `open_model_stream` 调用次数 |
| 集成 | native demo + mock OpenAI/Responses |
| Edge（Phase 2） | worker 单测 + 可选 live smoke（需凭证，不进默认 CI） |

---

## 9. 里程碑与目录草图

```text
crates/codegen/
  xai-hyper-host/          # trait + types
  xai-hyper-core/          # engine
examples/
  hypercore-native/        # Phase 1 CLI
  cloudflare-workers/      # Phase 2（可后建仓库子树）
docs/
  design-hypercore.md      # 本文
```

| 里程碑 | 完成定义 |
|--------|----------|
| M0 | 文档 Accepted + 空 crate CI |
| M1 | native 真模型 3 轮 + restore + 幂等 |
| M2 | 至少一种非本机 host 可演示（sidecar 或 DO） |
| M3 | 第一批 host tools + 协议稳定说明 |

---

## 10. 决策记录（拟）

| ID | 决策 | 理由 |
|----|------|------|
| D1 | Core 与 TUI 解耦，不替换默认 `hyper` 入口 | 降低回归面 |
| D2 | Phase 1 不追求完整 ACP | 加快闭环 |
| D3 | Phase 2 允许 2B-lite 先于 2B-full | 先交付「上云」价值 |
| D4 | Secret 永不进 snapshot | 安全底线 |
| D5 | 完整 PTY/MCP 不作为 edge MVP | 边界清晰 |

---

## 11. 开放问题（实现前可再收敛）

1. Snapshot 编码：JSON（易调试）vs postcard（更小）？建议 **v1 JSON**，v2 再加二进制。  
2. 是否与现有 `~/.grok/sessions` 目录格式对齐？建议 **初期独立** `hypercore/`，后期写转换器。  
3. Core WASM 目标：`wasm32-wasi` vs Cloudflare workers 自定义 imports？待 Phase 1 体积数据后再定。  
4. 社区版本号：core crate 是否跟 `0.2.114-rN` 锁步？建议 **独立 0.1.x**，host API 版本单独记。

---


## 11b. Existing shell 作为 HyperHost（本机集成，非 CF）

| 项 | 内容 |
|----|------|
| 模块 | `xai_grok_shell::hypercore_host::ShellHyperHost` |
| 凭证/路由 | 持有 session 的完整 `SamplerConfig`（`reconstruct_full_config`） |
| 存储 | `{grok_home}/hypercore/<session_id>/`（与 NativeHost 同布局） |
| 流 | 复用 `xai_hyper_core::native::open_model_stream_from_sampler_config` |
| 工厂 | `SessionActor::shell_hypercore_host()` |
| 旁路 turn | `HYPERCORE_TURN` 默认 on；**P3：默认走 Hypercore + shell `execute_tool_calls`** |
| Host API | **v2**：`ToolDefinition`、`ModelChunk::ToolCall`、`list_tools`、`ChatMessage` tool 字段 |
| Core tool loop | **done**：`submit_turn` / `submit_turn_with_tools`；snapshot **v2** |
| Native 桥 | **P2 partial**：`Completed` 提取 tool_calls；tools/json_schema → `ConversationRequest` |
| Shell 工具 | **P3 done**：`prepare_tool_definitions` → core；batch invoker → `execute_tool_calls` |
| json_schema / outer | **P4 done**：native schema + StructuredOutput tool；goal/stop outer loop |

**开关：**

```bash
# 默认：Hypercore 主路径 + shell 真工具环
hyper

# 强制全程 legacy
export HYPERCORE_TURN=0
hyper

# 关工具环（仅 plain；需 HYPERCORE_PLAIN=1 才走 Hypercore，否则 legacy）
export HYPERCORE_TOOLS=0
export HYPERCORE_PLAIN=1
hyper
```

**映射：** `CoreEvent::AssistantDelta` → ACP `AgentMessageChunk`；assistant 写回 `chat_state`。

### 迁移阶段（shell 路径）

| 阶段 | 状态 | 内容 |
|------|------|------|
| **P0** | **done** | Host/Core 类型：tools、ToolCall chunk、snapshot v2 |
| **P1** | **done** | Core 多步 tool loop + `MockHost::with_echo_tool` |
| **P2** | **partial** | Native 流提取 tool_calls + tools 进 sampler request |
| **P3** | **done** | `submit_turn_with_tools` + shell `execute_tool_calls`；`hypercore_tool_loop_ready` 默认 true |
| **P4** | **done** | json_schema（native + StructuredOutput）+ `run_turn_outer_loop` |
| **P5** | todo | subagent 独立 core；compact；旁路 memory/recap **有意留 shell** |
| **P6** | todo | 文档收口；可选删 legacy 主路径 |

### 尚未迁入 Hypercore 的路径（gap）

| 路径 | 现状 | 入口 |
|------|------|------|
| **Shell 真工具 / MCP** | **P3 done** via `submit_turn_with_tools` → `execute_tool_calls` | — |
| **json_schema 主路径** | **P4 done**（native / StructuredOutput） | — |
| **Subagent / spawn** | 独立 session turn | P5 |
| **Memory dream / recap / laziness** | 有意留 shell 旁路 sampler | 非目标（可薄封装） |
| **双 transcript** | chat_state 权威 + hypercore snapshot | 渐进统一 |
| **Cloudflare / 远端 Host** | Phase 2 后置 | — |

## 12. 下一步（执行顺序）

1. ~~**评审本文** → status 改为 Accepted~~ **done**  
2. ~~**开 M0：** `xai-hyper-host` + `xai-hyper-core` 骨架 + mock 测试~~ **done**  
3. ~~**M1 / Phase 1：** sampler + disk `NativeHost` + `hypercore-demo`~~ **done**  
4. ~~**Shell 作为 HyperHost + plain turn + 安全门控**~~ **done**  
5. ~~**P0/P1：** Host API v2 + Core tool loop（Mock）~~ **done**  
6. ~~**P3：** shell 真工具环~~ **done**  
7. ~~**P4：** json_schema + shell outer loop~~ **done**  
8. **P5/P6：** subagent、compact、文档；CF 后置  
9. 不把完整 TUI/PTY 逻辑塞进 core。

---

## 13. 参考

- nanocodex Cloudflare example 结构：Worker 路由 + 每会话 DO + SQLite snapshot + idle unload + 幂等 turn  
- nanocodex host-managed transport：凭证与 WS 升级在 host，guest/core 不持 secret  
- 本仓库：`xai-grok-sampler`、`xai-chat-state`、`xai-acp-lib`、现有 session turn 路径  

---

## 附录 A — 最小协议消息（Client ↔ Host，示意）

```json
// client → host
{"type":"turn","turn_id":"…","text":"hello"}
{"type":"ping"}

// host → client
{"type":"assistant_delta","turn_id":"…","text":"…"}
{"type":"turn_committed","turn_id":"…","stop_reason":"end_turn"}
{"type":"turn_failed","turn_id":"…","error":"…"}
{"type":"pong"}
```

Host 内部再调 core；client **永不**直连 core 类型。

---

## 附录 B — Phase 对照一览

| 阶段 | 用户可见结果 | 技术核心 |
|------|----------------|----------|
| **0** | 开发者可依赖空 core | trait + mock |
| **1** | 本机 CLI 真聊天 + 恢复会话 | sampler + 磁盘 snapshot |
| **2** | 浏览器/REPL 经 Cloudflare（或侧车）聊天 | 可插拔 Host + DO/路由 |
| **3** | 工具与协议加厚 | invoke_tool + ACP 适配 |

---

*本文为设计草案。实现以 PR 为准；若与代码冲突，先改文档或开 ADR。*
