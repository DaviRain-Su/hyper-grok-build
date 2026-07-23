# Design: Oracle 升级（强模型钉定 + 可感知触发）

| 项 | 内容 |
|----|------|
| 状态 | **设计文档**（优先于 Amp Modes；Modes 已缓停） |
| 日期 | 2026-07-23 |
| 修订 | 2026-07-23：用户强制入口 = **对话里说 oracle**；**不做** `/oracle` slash |
| 动机 | 主会话常用较弱/便宜模型（如 Grok）；Oracle 存在但常「不会被叫」或「叫了也同模型」 |
| 对标 | Amp Oracle（模式内自动深想）；omp Advisor（可选旁路） |
| 背景 | [competitive-analysis.md](./competitive-analysis.md)；Modes 缓停见 [design-modes.md](./design-modes.md) §0 |
| 已有底座 | 内置 `oracle` agent（只读、有预算与结构化输出）；`[subagents.models]`；`/agents` 钉模型 |

---

## 1. 问题陈述

Amp 的 Oracle **不是**「多一个子代理类型」这么简单，而是：

1. **编排层会主动用**：effort / mode 表里 Oracle 是一等公民，工作 agent 自然会去咨询。
2. **模型默认更强**：干活模型和深想模型是两套；用户很少落到「Oracle = 主会话同款弱模型」。

Hyper 现状：

| 层 | 现状 | 缺口 |
|----|------|------|
| 类型与工具 | `oracle` 只读、有 rounds/tool/time 预算、有 Recommendation 合同 | 够用 |
| 模型 | 未钉时 **继承主会话模型** | 主会话 = Grok → Oracle 也是 Grok → **模式失效** |
| 触发 | 完全靠主模型读 tool 文案后 **自行决定** spawn | 弱模型往往「硬撑」；用户感觉「不像 Amp 会去调 Oracle」 |
| 用户入口 | 对话里可口头要求（已有能力路径） | 缺「同模型无意义」的可观测警告；主模型对用户说「用 oracle」的服从度不够稳 |

**用户痛点（原话意图）：** 主线程用 Grok，强模型（如 GPT 5.6）只想给 Oracle；怎样让系统 **感知实现不行**，并 **用更强模型纠偏**，而不是 Grok 自我安慰式继续改？

---

## 2. 目标与非目标

### 2.1 目标

1. **强模型钉定可发现、可强制建议**：Oracle 未钉或与父模型相同 → 明确提示；文档与 `/doctor`（或 spawn 时 warn）说清「不钉就没用」。
2. **触发可感知**：主 agent 有清晰、可测的「何时必须 / 应该咨询 Oracle」规则（prompt + 可选 harness 信号）。
3. **用户强制走自然语言**：在对话框直接说「用 oracle / ask oracle / spawn oracle …」即可；主模型必须服从并 `spawn_subagent(oracle)`。
4. **保持本地 / 多 provider**：不绑死某家 slug；钉的是 catalog 里用户已有凭证的模型。
5. **默认可关、成本可控**：不默认每步旁听（那是 Advisor）；Oracle 仍是 **按需、有界**。

### 2.2 非目标

- 复活 Amp 四档 Modes 作为触发前提（已缓停）。
- **新增 `/oracle` slash**（用户明确不要；对话入口足够）。
- 完整 omp Advisor「每步第二模型挑刺」（可作后续可选；成本高）。
- 让 Oracle 直接改代码（保持只读；纠偏由主 agent 执行 Recommendation）。
- 云端保证强模型永远在线。

---

## 3. 产品概念

```text
主会话（干活，可便宜）
    │  卡住 / 反复失败 / 架构抉择 / 用户在对话里点名 oracle
    ▼
spawn oracle（只读深想）── 必须尽量用「更强钉定模型」
    │
    ▼
结构化 Recommendation → 主会话执行与验证
```

**不变式：** Oracle 的价值 = **（比主会话更强的模型）×（被真正调用）**。缺一则用户感知为「没有 Amp 那种 Oracle」。

---

## 4. 模型钉定（先做，小改动高收益）

### 4.1 配置（已有，要产品化）

```toml
# ~/.grok/config.toml
[subagents.models]
oracle = "openai/gpt-5.6"   # 示例 slug；以用户 catalog 为准
# explore = "…"             # 可选：探路用更便宜的
```

TUI：`/agents` → 选 `oracle` → `m` 钉模型（已有）。

### 4.2 行为建议（实现切片）

| 优先级 | 行为 |
|--------|------|
| P0 文档 | user-guide：开箱必读「钉 Oracle，否则等于没用」；给多 provider 示例；**对话里怎么点名** |
| P1 可观测 | spawn `oracle` 且解析后 model == 父 session model → **非致命 warn**（TUI/日志）：`oracle model equals parent; pin [subagents.models] oracle` |
| P1 doctor | `/doctor` 或 agents 面板：Oracle 未 pin / 与主模型相同 → 标黄建议 |
| P2 可选严格 | `oracle.require_stronger = true`：未 pin 或同模型则 **拒绝 spawn** 并提示如何配置（默认关，避免打断无第二模型的用户） |

### 4.3 「更强」如何定义（多 provider）

不做神秘 auto pricing 也可先落地：

1. **用户显式 pin** = 权威（够用）。
2. 可选后续：`auto:stronger` 依赖 catalog 元数据（原 Modes P4 思路）— **不阻塞 P0/P1**。

---

## 5. 触发：怎样「感知实现不行」

分三层。Amp 的「自动感」≈ 编排强制 + 强模型；Hyper 对齐 **层 A+B**，层 C 可选。

### 5.1 层 A — Prompt / 工具文案（主模型自觉）

加强主会话对 `spawn_subagent(oracle)` 的说明（父 Task 描述 + system 策略），给出 **可操作触发条件**，例如：

| 信号 | 应咨询 Oracle |
|------|----------------|
| 同一失败 | 同一测试 / 同一错误在 ≥2 次 edit 循环后仍在 |
| 不确定 | 架构二选一、安全/数据迁移、根因不清 |
| 自我怀疑 | 准备大范围重构或删除前 |
| 用户语言 | 「再想想」「你确定吗」「review 一下方案」 |

并写明：**不要用同模型硬撑**；Oracle 只读，由你执行其 Recommendation 并跑 Verification Handoff。

验收：在固定 fixture 会话里，注入「连续两次测试失败」类 transcript，主模型应发起 `oracle` spawn（评测可后补）。

### 5.2 层 B — 用户在对话框点名（不新增 slash）

**决定：不加 `/oracle`。** 用户强制入口 = 普通聊天消息。

| 用户说法（示例） | 主模型必须做的事 |
|------------------|------------------|
| 「用 oracle 看下为什么还红」 | `spawn_subagent`，`subagent_type=oracle`，brief 含失败上下文 |
| 「ask oracle / consult the oracle」 | 同上 |
| 「spawn oracle on …」 | 同上 |

实现要点（仍主要是 prompt / tool 文案，不是新命令）：

1. 父 agent 文案写死：**用户点名 oracle / Oracle 时，不得自己硬撑，必须 spawn `oracle`。**
2. user-guide 教这一句即可，与 `/btw`、`/doctor` 等 slash 并列说明「Oracle 走对话，不走 slash」。
3. （可选，P2）子代理卡片「Ask Oracle」——仍非 slash；需要时再加。

这解决：「Grok 不知道自己不行」→ **用户在对话框一句话升级深想**，不增加命令面。

### 5.3 层 C — Harness 信号（可选，更像产品）

在 shell/agent 循环里检测廉价信号，**建议或自动**咨询（默认建议，可开 auto）：

| 信号（示例） | 动作 |
|--------------|------|
| 连续 N 次 edit 后同一文件/同一测试仍红 | system-reminder：建议对话里 ask oracle，或自动 spawn（flag） |
| 主模型输出含反复道歉 / 「try another approach」循环 | 同上 |
| plan 验收清单多项失败 | 建议 Oracle 审计划而非继续盲改 |

**默认不要**每步 Advisor（贵）。与 omp Advisor、原 Modes P3 旁路区分：Oracle 仍是 **按需会话**，不是双模型常驻。

---

## 6. 与 Amp / 现有能力对照

| | Amp | Hyper 现状 | 本文方向 |
|--|-----|------------|----------|
| 四档 mode 驱动 Oracle | 有 | 不做四档 | 用 pin + 触发代替 |
| Oracle 更强模型 | 云内置 | 需用户 pin，常忘 | P0/P1 钉定 + warn |
| 自动咨询 | 产品编排强 | 靠模型自觉 | prompt 规则 + 对话点名 + 可选 harness |
| 用户强制入口 | 产品内编排 | 口头可说 | **对话点名**；不做 `/oracle` |
| 只读深想 | 有 | 已有 | 保持 |
| 结构化回复 | 有 | 已有合同 | 保持 |

---

## 7. 分阶段实现

### Phase 0 — 文档与默认心智（立刻）

- user-guide `16-subagents.md`：强调 **必须 pin**；给 Grok 干活 + GPT/Claude Oracle 示例；**对话里怎么点名**。
- competitive-analysis：Modes 降级；Oracle 升为第一梯队缺口；入口写对话而非 slash。

### Phase 1 — 钉定可观测 + 对话服从

1. spawn 同模型 warn；agents/`/doctor` 提示未 pin。
2. 加强父 agent 文案：用户点名 oracle → **必须** spawn；失败循环等自觉触发（层 A）。
3. **不做** `/oracle` slash。

### Phase 2 — 轻量 harness 建议

- 失败循环 reminder（默认仅建议用户在对话里 ask oracle，或可选 auto-spawn）。
- 可选 `oracle.auto_on_repeated_failure = true`。

### Phase 3 — 可选 Advisor 旁路

- 与「按需 Oracle」并列；默认关；高成本用户再开。

---

## 8. 验收场景

| # | 场景 | 期望 |
|---|------|------|
| 1 | 主模型 Grok，未 pin oracle | spawn 时 warn；doctor 提示 |
| 2 | `[subagents.models] oracle = 强模型` | oracle 子会话实际用强模型（日志可见） |
| 3 | 用户说「用 oracle 看为什么测试还红」 | 主模型 spawn oracle（钉定强模型）+ Recommendation 回主会话 |
| 4 | 主模型连续两次修同一失败测试仍红 + 层 A 文案 | 倾向 spawn oracle（人工/评测抽查） |
| 5 | 无第二模型凭证 | 不崩；warn；用户可关严格模式 |

---

## 9. 风险

| 风险 | 缓解 |
|------|------|
| 弱模型从不叫 Oracle | 对话点名服从文案 + harness 建议，不单靠自觉 |
| 用户说了 oracle 仍硬撑 | Phase 1 把「点名必须 spawn」写进父 agent / Task 说明并抽查 |
| 钉错模型 / slug 不可用 | 现有 catalog 校验 + fallback 行为保持可观测 |
| 费用暴涨 | 保持预算上限；不默认每步 Advisor |
| 与 explore 混淆 | 文案：explore = 广搜；oracle = 深决策/根因 |
| 命令面膨胀 | **明确不做** `/oracle` |

---

## 10. 决策记录

| 项 | 决定 |
|----|------|
| Amp 四档 Modes | **不做**（见 design-modes §0） |
| Oracle 模型 | **用户 pin 为第一真相**；同模型 warn |
| 用户强制入口 | **对话框自然语言点名**（用 / ask / spawn oracle）；**不做 `/oracle` slash** |
| 触发 | Prompt 规则 + 对话服从 + 可选 harness；自动旁路后置 |
| 自动每步 Advisor | 后置、默认关 |
| 实现顺序 | Phase 0 文档 → Phase 1 pin warn + 对话服从文案 → Phase 2 harness 建议 |

---

## 11. 参考

- 现有 Oracle：`xai-tool-types` `ORACLE_PROMPT` / `ORACLE_SUBAGENT`；`AgentDefinition::oracle()`
- 用户文档：`crates/codegen/xai-grok-pager/docs/user-guide/16-subagents.md`（Consulting the Oracle）
- Amp Oracle / Modes：https://ampcode.com/manual 、https://ampcode.com/modes
