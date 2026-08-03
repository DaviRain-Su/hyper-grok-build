# Design: ACP Steer Contract (`x.ai/interject`)

| 项 | 内容 |
|----|------|
| 状态 | **Accepted — verified, no code change** |
| 日期 | 2026-08-01 |
| 背景 | comet fork 的 hyper harness 需要运行中插话(steer)能力 |
| 相关 | `hyper-comet/.rpiv/artifacts/plans/hyper-comet-migration.md` Phase 1/2 |

## 一句话

**grok 的 `hyper agent stdio` 已经实现了 mid-turn steer,并暴露为 ACP 扩展
`x.ai/interject`。comet 的 hyper harness 直接复用它,不在 grok 侧新增任何代码。**

## 契约(给 comet hyper harness 用)

ACP **ExtRequest**(请求/响应,非 notification):

- **method**: `"x.ai/interject"`
- **params**:
  - `sessionId: string` — ACP `SessionId.0`
  - `text: string` — steer 文本
  - `interjectionId?: string` — 客户端生成的 id,回包/广播里原样回传(用于去重)。comet 侧传 `SteerMessage.message_id`。
  - `content?: ContentBlock[]` — 可选的结构化块(text + images);省略 = legacy 纯文本形态,也能解析。
- **returns**: `{ "status": "queued" }`

## grok 侧实现(已存在,勿改)

- **入口注册**: `packages/coding-agent/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs:3848`
  `"x.ai/interject" => crate::extensions::interject::handle(self, &args).await`
- **handler**: `packages/coding-agent/xai-grok-shell/src/extensions/interject.rs`
  `handle()` 解析 `InterjectRequest`,发送 `SessionCommand::Interject { text, id, images }`。
- **run loop**: `packages/coding-agent/xai-grok-shell/src/session/acp_session_impl/run_loop.rs:1977`
  的 `Interject` 分支:turn 运行中 → push 进 `pending_interjections`;idle → 排一个
  fallback prompt turn。
- **drain**: `packages/coding-agent/xai-grok-shell/src/session/acp_session_impl/interjection.rs`
  `drain_pending_interjections` 在下一个安全点把缓冲的 steer 合进运行中的 turn
  (作为独立的 synthetic user message,不取消当前 turn)。
- **广播**: `broadcast_interjection` 向所有 attached client 扇出
  `x.ai/session/interjection`(发起方按 `interjectionId` 去重)。

## 约束(给未来 grok 改动者)

- **不要重命名/删除 `x.ai/interject`** —— comet 的 hyper harness 依赖它做 steer。
- **`interjectionId` 必须原样回传** —— comet 用来对账。
- 若要改 `InterjectRequest` 字段,保持 `sessionId`/`text`/`interjectionId?` 向后兼容
  (`content?` 已是可选)。

## 验证

- 代码读确认(本次 spike):注册 + handler + run-loop drain + broadcast 全链路存在。
- 全量测试待 grok 构建环境可用(target 软链接的外置盘挂载后):
  `cargo test -p xai-grok-shell --lib extensions::interject`
- 手动:`hyper agent stdio` 跑一个 turn,发 `x.ai/interject`,下一 turn 应带上 steer 文本。