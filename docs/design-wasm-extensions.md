# Design: WASM Extensions（Pi 式生命周期注入）

| 项 | 内容 |
|----|------|
| 状态 | **Phase 3 落地**（stop/pre_compact/session_end + validate + user-guide；Component Model 仍后续） |
| 日期 | 2026-07-28 |
| 动机 | 让第三方/团队在**不重编 Hyper 核心**的前提下扩展 agent 行为；对齐 Pi 的 Extension 哲学，guest 格式用 WASM |
| 对标 | [Pi Extensions](https://pi.dev/)（TS ExtensionAPI + lifecycle events）；本仓库 [competitive-analysis.md](docs/competitive-analysis.md) §3.4 / A13 |
| 已有底座 | hooks 事件表与 dispatcher；plugins + marketplace；MCP/skills/agents 声明式包 |
| 产物落点（实现时） | `docs/design-wasm-extensions.md` + 新 crate（见 §7） |

---

## 0. 一句话不变式

**Agent loop 只 `emit` 稳定生命周期事件；扩展以 WASM guest 动态加载，经版本化 WIT 返回 gate/inject 决策；声明式 plugin 包与 shell hooks 仍可用，并与 WASM 共用同一条 Extension Bus。**

---

## 1. 问题陈述

### 1.1 用户与产品意图

- 希望 Hyper 能像 **pi-mono** 一样：开发者写扩展 → 动态加载 → 挂在 before/start/end 等点上拦截或注入。
- 宿主是 **Rust**，不能像 Pi 那样 `import()` TS 模块；动态第三方 guest 的合理默认形态是 **WASM**（Component Model + WIT）。
- 现状「有 hooks / plugins」但偏 **声明式内容包 + 外进程脚本**，缺少 **进程内可编程 ExtensionAPI**，开发者感觉「没法进一步开发平台」。

### 1.2 现状能力矩阵

| 层 | 现状 | 缺口 |
|----|------|------|
| 生命周期事件 | `xai-grok-hooks`：`SessionStart` / `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `Stop` / `PreCompact` / `SessionEnd` 等 | 事件面够；**可改写契约**不足 |
| Handler | shell command + HTTP | **无同进程 guest** |
| 打包分发 | plugins + marketplace + trust | 无 `extension.wasm` 约定 |
| 自定义 tool | 主要靠 MCP | 无 `register_tool` 一等 API |
| 注入上下文 | 弱 / 分散 | 缺 Pi 的 `before_agent_start` 级 inject |
| 动态加载 | 配置热重载级 | 无 wasmtime runtime |

### 1.3 已有挂载点（实现必须复用，不另开洞）

| 事件 | 典型调用位置（现状） | 决策类型 |
|------|----------------------|----------|
| SessionStart | `SessionCommand::DispatchSessionStartHook` → run_loop | observe |
| UserPromptSubmit | `acp_session_impl/turn.rs` | observe |
| PreToolUse | `acp_session_impl/tool_calls.rs` → `dispatch_pre_tool_use` | **gate (deny)** |
| Stop / SubagentStop | `acp_session_impl/stop_gate.rs` → `dispatch_stop` | **stop gate** |
| PreCompact / PostCompact | `session/compaction.rs` 等 | observe |
| 其它 | `dispatch_non_blocking` | observe |

**设计原则：** loop 侧继续只调 **Bus**；WASM 作为新 handler backend 进入现有 dispatcher 链，而不是在 shell 里散落 `wasmtime` 调用。

---

## 2. 目标与非目标

### 2.1 目标

1. **Pi 同构的扩展模型**：固定 lifecycle 注入点 + 动态加载 guest + 稳定 Host/Guest API。
2. **Guest = WASM**（`wasmtime` + **WebAssembly Component Model** + WIT），跨平台、可沙箱、ABI 可版本化。
3. **与现有生态合并**：plugin 目录 / marketplace / trust / enable；hooks 脚本不废；MCP/skills 仍走声明式。
4. **MVP 可验收**：至少一个官方示例扩展（如 path/safe-shell 策略）证明 load → gate → 不崩主进程。
5. **安全默认**：未信任不跑 gate/inject；capability 白名单；超时 + trap fail-open（与 hooks 一致，除非声明 strict）。
6. **上游友好**：feature flag / 可选依赖；不强迫所有用户背 wasmtime 成本（可 feature 裁剪）。

### 2.2 非目标（明确不做或后置）

| 项 | 说明 |
|----|------|
| 原生 `.so`/dylib 插件 | ABI/崩溃/提权；仅可能内部实验，不进社区模型 |
| 第一期 TUI 扩展 | panel / 快捷键 / status bar → 后期 Host API |
| 第一期 `before_model` 全文 rewrite | 最强也最危险；P2+ |
| 第一期 `register_tool` 完整版 | P2；MVP 用 MCP 顶 |
| 把 Hyper 砍成 Pi 四工具默认 | 学扩展纪律，不阉割产品 |
| 替换 hooks/MCP | 并存，不是迁移强迫 |
| 任意语言无文档支持 | 作者语言先 **Rust → component**；TS→wasm 后置 |

---

## 3. 产品概念（对齐 Pi）

```text
Pi:     TS module ── import() ── ExtensionAPI.on / registerTool
Hyper:  .wasm     ── wasmtime ── WIT hyper:extension@0.1 + Host imports
```

```text
Plugin bundle
├── plugin.json          # + runtime.wasm 字段
├── extension.wasm       # 可选：进程内扩展
├── skills/ agents/ …
├── hooks/hooks.json     # 可选：旧路径
└── .mcp.json            # 可选：重工具
```

**用户感知：**

- 安装：`hyper plugin install … --trust`（已有路径）
- 生效：session start 加载 wasm；`/plugins` 显示 runtime 组件
- 开发：写 guest → `cargo component build` → 放进 plugin 目录

**与 Pi 事件心智映射：**

| Pi（概念） | Hyper MVP 事件 | 能力 |
|------------|----------------|------|
| `session_start` | `session_start` | init / log |
| `before_agent_start` | **`before_agent_start`（新增语义）** | inject context / append system |
| `tool_call` | `pre_tool_use` | allow / deny |
| tool 后 | `post_tool_use` | observe |
| turn 结束 | `stop` | block 续跑 |
| compact 前 | `pre_compact` | observe（MVP） |
| `session_shutdown` | `session_end` | cleanup |

`UserPromptSubmit` 保留为 observe；**可注入**统一到 `before_agent_start`，避免两个点都能改 prompt 造成顺序混乱。

---

## 4. 架构

### 4.1 总图

```text
┌──────────────────────────────────────────────────────────────┐
│  Agent / Session Loop (shell)                                │
│    仅: extension_bus.emit(Event, Envelope) -> Decision         │
└────────────────────────────┬─────────────────────────────────┘
                             │
                             ▼
┌──────────────────────────────────────────────────────────────┐
│  Extension Bus（建议落在 hooks 扩展，而非新建第二总线）        │
│    顺序: Command/HTTP hooks  ──然后──►  Wasm guests            │
│    合并: deny 短路; inject 拼接; fail-open                     │
└─────────────┬──────────────────────────────┬─────────────────┘
              │                              │
              ▼                              ▼
     xai-grok-hooks runner          xai-grok-extension-runtime
     (已有 shell/HTTP)              (wasmtime + 实例 + 超时)
                                             │
                                             ▼
                                    extension.wasm (guest)
                                    import: host.log / …
                                    export: on_* handlers
```

### 4.2 设计纪律

1. **Bus 在宿主，guest 不持有 loop 控制权**  
2. **WIT 是唯一跨版本 ABI**；Rust 内部类型可改  
3. **短调用**：guest 函数同步、有超时；长 I/O 只能经 host 异步 capability（后期）  
4. **默认无能力**：仅 `log` + 订阅的事件回调；`pre_tool_gate` / `before_agent_inject` 需声明 + trust  
5. **一份 plugin 可同时有 wasm + skills + mcp**

### 4.3 Crate 边界（实现时）

| Crate | 职责 | 依赖方向 |
|-------|------|----------|
| `xai-grok-extension-api` | WIT 源（`wit/extension.wit`）、事件/决策/capability 类型、`CORE_ABI_VERSION` | serde only |
| `xai-grok-extension-runtime` | 加载 `.wasm`、调用、超时、trap→错误；feature `wasm` | api + wasmtime |
| `xai-grok-hooks` | Bus 合并；`HandlerKind::Wasm`；对外仍 `dispatch_*` | runtime（feature） |
| `xai-grok-agent` plugins | 发现 `extension.wasm` / manifest `runtime`；trust 传递 | 类型 only |
| shell session | **不直接**依赖 wasmtime；继续调 dispatcher | hooks |

Feature 建议：`extension-wasm`（默认 on 或 off 由包体决定，实现时量体积再定）。

---

## 5. 生命周期与返回契约（核心 API）

### 5.1 MVP 事件集合（锁定 7 个）

| # | Event | 可改写 | MVP 必须 | 返回 |
|---|-------|--------|----------|------|
| 1 | `session_start` | 否 | 是 | ok / err |
| 2 | `before_agent_start` | **是** | 是（P1） | inject_context?, append_system? |
| 3 | `pre_tool_use` | gate | 是（P0–P1） | allow \| deny(reason) |
| 4 | `post_tool_use` | 否 | 是 | ok |
| 5 | `stop` | gate | P1 | continue \| block(reason) |
| 6 | `pre_compact` | 否 | P1 observe | ok |
| 7 | `session_end` | 否 | 是 | ok |

后置：`before_model` / message rewrite、`register_tool`、`subagent_*` 细控、UI。

### 5.2 合并规则（与现有 hooks 对齐）

| 类型 | 规则 |
|------|------|
| `pre_tool_use` | 顺序执行；**任一 deny 短路**；wasm trap/超时 **fail-open**（记 Failed） |
| `stop` | block 可续跑；`force_stop` 覆盖；遵守现有 continuation cap |
| `before_agent_start` | 多 guest 的 `inject_context` / `append_system` **按顺序拼接**（带扩展名分隔注释可选） |
| observe 事件 | 全跑；错误只记日志 |
| 与 shell hooks 顺序 | **先 command/http，后 wasm**（实现可配置；默认此序，便于「脚本粗拦 + wasm 细拦」） |

### 5.3 `before_agent_start` 语义（相对现状的新增）

- **触发时机：** 用户 prompt 已接受、即将进入 agent loop（在 `UserPromptSubmit` observe 之后）。  
- **可做：**  
  - `inject_context`：作为额外上下文进入当轮（host 决定是 system 旁路消息还是 user 前缀；**实现时只选一种并写死**，建议：独立 system-ish 片段，不伪造 user）。  
  - `append_system`：追加到 system prompt 片段。  
- **不可做（MVP）：** 删除历史、改模型、改权限模式。

### 5.4 Payload 限制

- 对齐 hooks：`tool_input` / 大字段 **≤ 128KB** 截断或拒绝进 guest。  
- 字符串用 UTF-8；二进制不进 MVP 事件。

---

## 6. WIT 草案（`hyper:extension@0.1.0`）

> 实现前可微调字段名；**版本与 world 名锁定后**插件才能稳定。

```wit
package hyper:extension@0.1.0;

interface host {
  /// 始终可用（或仅 trusted）
  log: func(level: string, msg: string);
}

interface types {
  record session-start-in {
    session-id: string,
    cwd: string,
    reason: string,  // "startup" | "resume" | "new" | …
  }

  record before-agent-start-in {
    prompt: string,
  }

  record before-agent-start-out {
    inject-context: option<string>,
    append-system: option<string>,
  }

  record pre-tool-in {
    tool-name: string,
    tool-input-json: string,
  }

  variant pre-tool-out {
    allow,
    deny(string),  // reason
  }

  record post-tool-in {
    tool-name: string,
    success: bool,
  }

  record stop-in {
    stop-hook-active: bool,
  }

  variant stop-out {
    continue,
    block(string),
  }

  record pre-compact-in {
    reason: string,
  }
}

world extension {
  import host;
  // Guest exports (概念；实际用 wit-bindgen 生成):
  // export init: func() -> result<_, string>
  // export on-session-start: func(session-start-in) -> result<_, string>
  // export on-before-agent-start: func(before-agent-start-in) -> result<before-agent-start-out, string>
  // export on-pre-tool-use: func(pre-tool-in) -> result<pre-tool-out, string>
  // export on-post-tool-use: func(post-tool-in) -> result<_, string>
  // export on-stop: func(stop-in) -> result<stop-out, string>
  // export on-pre-compact: func(pre-compact-in) -> result<_, string>
  // export on-session-end: func() -> result<_, string>
}
```

**Capability 声明（manifest，非 WIT）：**

```json
{
  "name": "path-guard",
  "version": "0.1.0",
  "runtime": {
    "wasm": "extension.wasm",
    "wit": "hyper:extension@0.1.0",
    "capabilities": [
      "pre_tool_gate",
      "before_agent_inject",
      "stop_gate"
    ]
  }
}
```

| Capability | 允许的 export 生效 |
|------------|-------------------|
| （无） | 仅 observe 类；gate/inject 返回被 host 忽略 |
| `pre_tool_gate` | deny 生效 |
| `before_agent_inject` | inject/append 生效 |
| `stop_gate` | block 生效 |

未 `trusted` 的 plugin：**不实例化 wasm** 或仅 observe（二选一，实现定：**不实例化** 更安全）。

---

## 7. Plugin 发现与信任

### 7.1 发现

在现有 plugin 扫描（`xai-grok-agent` plugins discovery）中增加：

1. `plugin.json` → `runtime.wasm` 路径（默认 `extension.wasm`）  
2. 文件存在且 trusted + enabled → 注册到 runtime registry  
3. `grok plugin details` / inspect 展示 `runtime: wasm@0.1`

### 7.2 加载时机

- Session start（与 hooks 加载同相位）  
- Plugins reload（`r` / 新 session）  
- **不**在每个 tool call 重新 instantiate（实例可 per-session 复用；start 时 `init`）

### 7.3 超时

| 调用 | 默认超时 |
|------|----------|
| `init` | 2s |
| observe handlers | 1s |
| gate (`pre_tool` / `stop`) | 2s（对齐 hooks 默认量级） |
| `before_agent_start` | 1s |

超时 = fail-open + `HookRunResult::Failed` 风格遥测。

---

## 8. 安全模型

| 威胁 | 缓解 |
|------|------|
| 恶意插件 RCE | WASM 沙箱；默认无 fs/net/shell import |
| 恶意 deny 导致 DoS | 用户可 disable plugin；trust 门槛 |
| 注入超长 system | host 截断 inject/append（如各 8–32KB） |
| 依赖供应链 | marketplace pin sha（已有 `require_sha`） |
| trap 拖垮进程 | wasmtime trap 捕获，不 panic 进 UI 线程 |
| 与 shell hook 叠加 | 同一 trust/folder-trust 模型 |

**明确不在 MVP 给 guest：** 任意 host 函数调用 shell、读密钥、改 config 文件。

---

## 9. 实现分期与验收

### Phase 0 — Spike + Bus 骨架（可扔代码边界清晰）

- [x] 评估 `wasmtime`：采用 **v31**（Apache-2.0），crate feature `wasm` 可关  
- [x] 最小 **core-wasm bootstrap ABI**（`hyper_ext_abi_version` / `on_session_start`；Component Model 仍为 Phase 1 目标）  
- [x] `ExtensionRuntime::load` + 墙钟 timeout（`tokio::time::timeout` + fuel）  
- [x] **验收：** `xai-grok-extension-api` / `xai-grok-extension-runtime` 单测通过；未信任拒绝加载；trap fail-open；pre_tool deny  
- [ ] 主程序（pager-bin / shell）尚未依赖 runtime（Phase 1 再挂）

### Phase 1 — P0 产品最小可用

- [x] manifest `runtime` + discovery（`PluginManifest.runtime` / `runtime_wasm_path` / `LoadedPlugin.extension_spec`）  
- [x] session：spawn + plugin reload 重建 `ExtensionRuntime`；`SessionStart` 调 wasm  
- [x] **`pre_tool_use` deny**：shell hooks 之后跑 wasm（`tool_calls.rs`）；host imports 传 tool input  
- [x] 示例：`xai-grok-extension-runtime/examples/safe-shell-plugin/`（`.wat` + `plugin.json`）  
- [x] **验收：** unit 测 deny `rm -rf`；trap fail-open；未 trust 不加载  
- [x] session_end 已挂（channel close + shutdown）  
- [ ] Component Model / wit-bindgen 仍为后续

### Phase 2 — 像 Pi 的注入

- [x] host：`UserPromptSubmit` 之后 `dispatch_before_agent_start`（`turn.rs`）  
- [x] inject_context → `push_system_reminder`；append_system → `<system-extension>` tag  
- [x] guest ABI：`hyper_ext_on_before_agent_start` + `set_inject_context` / `set_append_system`（guest memory）  
- [x] capability `before_agent_inject`；多 guest 顺序拼接 + `[wasm:name]` 前缀  
- [x] `stop` gate（`stop_gate` + `run_stop_gate`）  
- [x] `pre_compact` observe  
- [x] **验收：** unit 测 inject/append/stop；shell check 通过  

### Phase 3 — 开发者体验

- [ ] `hyper extension init` 模板（Rust guest）— 暂缓；WAT 示例足够起步  
- [x] `grok plugin validate` 检查 wasm 魔数、WIT、capabilities  
- [x] 文档：user-guide `31-wasm-extensions.md` + plugins 交叉引用  
- [x] 回写 competitive-analysis A13 → building（Phase 3）  


### Phase 4+（不在本设计强制范围）

- `register_tool`  
- `before_model` rewrite  
- TS→component 工具链  
- UI notify Host API  
- guest 持久 store（`GROK_PLUGIN_DATA` 经 host 读写）

---

## 10. 测试策略

| 层 | 内容 |
|----|------|
| 单元 | WIT 编解码、capability 拒绝、超时、截断 |
| 集成 | 临时 plugin dir + 假 session dispatch |
| 回归 | 现有 hooks 集成测不因 wasm feature 破坏 |
| 示例 | CI 构建示例 wasm（或 checked-in fixture wasm） |
| 性能 | session start 加载 N 个扩展的预算（目标：N=5 时 < 100ms 量级，spike 校准） |

---

## 11. 文档与仓库落点（实现阶段写入）

| 文件 | 用途 |
|------|------|
| `docs/design-wasm-extensions.md` | 本设计正文（从 plan 晋升） |
| `docs/competitive-analysis.md` | A13 状态 → design/building |
| `crates/codegen/xai-grok-pager/docs/user-guide/…` | 开发者指南（Phase 3） |
| `crates/.../examples/extensions/` | 官方示例 |

---

## 12. 风险与未决（实现前建议拍板）

| # | 问题 | 建议默认 |
|---|------|----------|
| R1 | wasmtime 是否默认编进 release | spike 后定；可 `extension-wasm` feature |
| R2 | Component Model vs 经典 wasm abi | **Component Model + WIT** |
| R3 | before_agent inject 呈现方式 | system 片段，不伪造 user |
| R4 | hooks 与 wasm 顺序 | 先 hooks 后 wasm |
| R5 | 未 trust 行为 | **不加载** runtime |
| R6 | guest 语言 | 先 Rust；TS 后置 |
| R7 | 是否新事件名进 HookEventName | `BeforeAgentStart` 加入事件表，或 bus 内部事件 + 文档映射；**倾向加入正式事件表** 以便 UI/ACP 一致 |

---

## 13. 建议的实现顺序（拍板后）

```text
1. 将本 plan 晋升 docs/design-wasm-extensions.md
2. Phase 0 spike（独立 branch / feature）
3. Phase 1：discovery + pre_tool gate + 示例
4. Phase 2：before_agent_start inject
5. Phase 3：DX 与文档；A13 回写
```

**本 PLAN 模式产出到此为止：设计与分期已锁定意图。**  
用户确认后退出 plan → 再动代码（先文档晋升与 Phase 0）。

---

## 14. 验收总表（Definition of Done for “像 Pi 的扩展底座”）

- [ ] 不重编 hyper 二进制即可安装含 `extension.wasm` 的 plugin  
- [ ] 至少 3 个生命周期点可被 guest 订阅（start / pre_tool / end）  
- [ ] pre_tool deny 与 shell hooks 行为一致且可叠加  
- [ ] before_agent_start 可注入上下文（Phase 2）  
- [ ] 恶意/错误 wasm 不崩溃会话  
- [ ] 官方示例 + 验证命令 + 设计文档可被新贡献者按文档复现  

---

## 附录 A — 与「只加强 hooks」对比

| | 只加强 shell hooks | 本设计 WASM bus |
|--|-------------------|-----------------|
| 动态加载 | 有 | 有 |
| 同进程状态 | 难 | 易（guest 内存 / 后续 host store） |
| 延迟 | 进程启动高 | 调用低 |
| 沙箱 | 无（全权限 shell） | 默认强 |
| 改写 agent 上下文 | 弱 | `before_agent_start` 一等 |
| 像 Pi | 否 | 是（ABI 不同，模型同） |

## 附录 B — 选型记录

- **Guest 格式：WASM** — 用户与架构共识；非 dylib。  
- **哲学：Pi** — 最小稳定 API + 动态扩展；非「功能全焊死核心」。  
- **集成：扩展现有 hooks bus** — 避免双总线。  
- **MVP 事件：7 个** — 覆盖 80% 扩展场景。  
