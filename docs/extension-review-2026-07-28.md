# Review: WASM Extensions (Phase 0–3)

| 项 | 内容 |
|----|------|
| 范围 | `2dd18b7` … `01c8728`（4 commits） |
| 审查人 | 实现侧自审（人机后续可再开 Oracle） |
| 日期 | 2026-07-28 |
| 结论 | **可继续演进**；底座方向正确。作者语言策略锁定 **Rust-first**；多语言后置。 |

---

## 1. 变更摘要

- **Host**：`xai-grok-extension-api`（契约/capability）+ `xai-grok-extension-runtime`（wasmtime）
- **发现**：`plugin.json` `runtime` + 约定 `extension.wasm`
- **Session**：start / end / pre_tool / before_agent_start / stop / pre_compact
- **DX**：`grok plugin validate` + user-guide `31-wasm-extensions.md`
- **Guest 示例**：WAT `safe-shell-plugin`（非正式作者路径）

---

## 2. 做得好的地方

| 点 | 说明 |
|----|------|
| 边界清晰 | agent 只依赖 api；runtime 隔离 wasmtime；shell 只 `emit` |
| 信任模型 | untrusted 不 load；capability 管 gate/inject |
| 并发安全 | `RefCell` 不跨 await；clone runtime 再 dispatch |
| Fail-open | trap/超时不拖垮会话（与 hooks 一致） |
| 合并顺序 | hooks → wasm 明确 |
| 可验证 | unit 覆盖 load / deny / inject / stop / untrusted |

---

## 3. 问题与风险（按严重度）

### P1 — 应尽快处理（不阻塞人机试玩）

1. **安全策略 fail-open 可被恶意 guest 绕过**  
   故意 trap / 超时 → 等同于 allow。对「硬安全门」不够。  
   **建议：** 声明 `strict` capability 或 config：`[extensions] gate_fail = "closed"|"open"`（默认 open 保持现状）。

2. **每次调用都重新 instantiate**  
   正确隔离，但热路径（每 tool）有成本。  
   **建议：** 测 N=5 guest × 50 tools 延迟；若需，会话内复用 Instance + 限制可变全局。

3. **`plugin validate` 不校验 export 表**  
   只查魔数/WIT/cap 字符串，坏模块要到 load 才失败。  
   **建议：** validate 可选 `--load` 调 `WasmGuest::from_bytes`。

### P2 — 中期

4. **deny/stop 无 guest 自定义 reason 字符串**（除固定 host 文案）  
5. **pre_tool 传入完整 `raw_input` JSON**，大 payload 每调用复制（有 128KB 截断，仍偏粗）  
6. **wasmtime 钉在 31**，与生态最新版差距大；升级需单独 PR  
7. **WIT / Component Model 未接线**，bootstrap ABI 与设计目标双轨，文档已说明但易混  
8. **无 e2e session 测试**，仅 unit + `cargo check`

### P3 — 体验 / 文档

9. 官方示例是 **WAT**，与「Rust-first」作者策略不一致（见 §4）  
10. `extension init` 脚手架未做  
11. `/plugins` UI 未展示 runtime 组件  
12. inject 用 system-reminder，不改 durable system — 符合设计，但能力边界要在文档里写死（已部分写）

---

## 4. 作者语言策略（产品决议）

**优先用 Rust 写扩展；其它语言后置。**

| 阶段 | 作者语言 | 产物 |
|------|----------|------|
| **现在（官方）** | **Rust → `wasm32-unknown-unknown` cdylib** | `extension.wasm` |
| 示例/调试 | WAT 可保留（测试 fixture） | 不作为推荐 DX |
| 以后 | Go / C / AssemblyScript / TS→component | 同一 core 或 WIT ABI |

理由：

- Host 与 monorepo 同为 Rust，类型与 CI 最顺  
- 无 JS 运行时体积/依赖  
- 将来 Component Model + `wit-bindgen` 的一等公民就是 Rust  
- 多语言只是 **编译目标兼容**，不改变 Host API

落地要求：

1. 文档默认路径改为 Rust guest 模板  
2. 提供 `examples/rust-guest-template/`（最小 `no_std`/`cdylib` 导出）  
3. WAT 示例标注「ABI 调试用，非推荐作者路径」

---

## 5. 建议的后续 backlog（人机继续时）

1. Rust guest 官方模板 + 构建说明（`cargo build --target wasm32-unknown-unknown`）  
2. validate `--load`  
3. gate fail-closed 可选  
4. 热路径基准 + 可选 instance 复用  
5. Component Model 迁移 spike  
6. e2e：装插件 → 危险命令 deny  
7. Oracle / 第二人 review（本文件 + 四 commit）

---

## 6. 验收清单（当前状态）

- [x] 不重编 hyper 可装含 wasm 的 plugin（路径具备）  
- [x] session start / pre_tool / end 可挂  
- [x] before_agent inject  
- [x] stop gate  
- [x] validate 基础检查  
- [x] unit 测通过  
- [ ] Rust-first 官方模板（本 review 后补）  
- [ ] e2e  
- [ ] fail-closed 选项  

---

## 7. 审查结论

**合入演进主线：通过（带 P1 跟进）。**  
架构对齐 Pi「动态 guest + 稳定 Host API」；安全默认合理但偏「开发友好 fail-open」。  
人机下一步优先：**Rust 扩展模板 + 文档锁定 Rust-first**，再考虑 Oracle 深审与 fail-closed。
