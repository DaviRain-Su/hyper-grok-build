# Design: Hyper Modes（Amp 式档位 + 可配置槽位）

| 项 | 内容 |
|----|------|
| 状态 | **缓停 / 非当前主线**（设计保留作参考，不排期实现） |
| 初稿 | 2026-07-21 |
| 决策修订 | 2026-07-23 |
| 动机（原） | 子代理默认继承主模型；缺 Amp 式 low–ultra 搭配与可配置降级 |
| 对标 | [Amp Modes & Models](https://ampcode.com/modes)、omp modelRoles |
| 竞品背景 | [competitive-analysis.md](./competitive-analysis.md) |
| 受众 | 个人 power user；多 provider / BYOK |
| 相关主线 | Oracle 触发与强模型钉定 → [design-oracle.md](./design-oracle.md) |

---

## 0. 决策修订（2026-07-23）

**结论：不做 Amp 四档 Modes 作为当前产品线。** 现有 `/model`、`/effort`、`[subagents.models]`、persona/role、spawn `model` 已覆盖「可配置」；Amp low–ultra 本质是 **意见鲜明的默认搭配表**，不是更强的自定义能力。对追求 BYOK / 多 provider / 细粒度钉模型的 Hyper 用户，整包抄四档 ROI 低，还会和 permission mode / plan mode 撞名。

| 项 | 决定 |
|----|------|
| Amp 式 `low \| medium \| high \| ultra` | **不做**（或永久缓；本文以下章节仅作归档） |
| 现有配置表面 | **保持**；用文档 / 示例 preset 教人钉 explore、oracle 等 |
| 真正缺口 | **Oracle 何时被叫、用什么更强模型** — 见 [design-oracle.md](./design-oracle.md) |
| 若将来再做「一键搭配」 | 优先 **薄 preset 包**（写入现有 `[subagents.models]`），不要新解析层 |

下文 §1 起仍是原设计全文，**不再作为实现 checklist**。

---

## 1. 目标与非目标

### 1.1 目标

1. **用户心智对齐 Amp**：会话有 `low | medium | high | ultra`，一档 = 多角色搭配表，不是单 model 开关。
2. **多 provider 安全**：不写死 Fable / GPT 等；槽位填 slug、策略或 `inherit`。
3. **缺模型可运行**：primary → fallback 列表 → 策略 → inherit → 全局 default，fail-soft + 可观察。
4. **出厂即有默认**：代码内置 balanced 语义；用户不写 config 也能分档。
5. **可自定义**：用户可改每档每个槽位的 model / effort / fallback；可做整包 preset。
6. **兼容现有管道**：不破坏 `[subagents.models]`、agent 定义、persona/role、spawn `model`。

### 1.2 非目标（本设计范围外）

- 云端保证模型可用性（Amp 云托管能力）。
- 完整 Oracle 旁路双会话实现的产品化细节（P3 只预留槽位与解析；实现可另开设计）。
- 替换主会话 `/model` 选择器（mode 与主模型共存：`inherit` 以主会话为基准）。
- **ACP 暴露**：`session/set_mode` 在 ACP 语义中是权限模式（Normal/Plan/Always-approve），本功能不复用该通道；agent mode 进 ACP 另开设计。
- 计费、订阅、mode plugin 市场。

---

## 2. 产品概念

### 2.1 从 Amp 学到什么

Amp 页面结构：

| 层 | Amp | Hyper 对应 |
|----|-----|------------|
| Agent Mode | System Prompt + Tools + **Model 搭配** | Mode 表：多 **slot** × model × effort |
| Low–Ultra | 四档主模式 | 同名四档（固定产品名） |
| 每档 Agent + Oracle | 干活 + 旁路深想 | 槽位 `agent` + `oracle`（oracle 可后实现） |
| Subagents 表 | Review / Search / Librarian… 各钉模型 | 槽位 `explore` / `plan` / … + 以后扩展 |
| Mode plugins | 整包换模型搭配 | `mode_presets.*` 用户/内置包 |

**抄结构，不抄死模型 ID。**

### 2.2 核心对象

```text
Session.mode: low | medium | high | ultra   (默认 medium)

Mode
  └── slots: agent | oracle | explore | plan | … 
        each SlotSpec:
          model:    ModelRef
          effort:   EffortRef
          fallback: [ModelRef, ...]   # 可选

ModelRef =
  | "inherit"                 # 父会话 / 主 agent 当前模型
  | "auto:cheaper"            # P1 可 stub 为 inherit；P4 实现
  | "auto:stronger"
  | "auto:same"
  | "<provider>/<model>" 等 catalog slug

EffortRef =
  | "inherit"
  | 现有 ReasoningEffort 枚举值（low|medium|high|xhigh|max|ultra）
  | 语义档位 "min"|"max"（取模型支持菜单的底/顶档）
```

> **命名约定**：本功能在产品文案中统一叫 **agent mode（档位）**，以区别已有的
> permission/session mode（Normal/Plan/Always-approve，Shift+Tab 循环）、
> plan mode（实现前规划）与 ACP `session/set_mode`（权限模式，**不复用**）。

### 2.3 用户可见行为

| 操作 | 行为 |
|------|------|
| 启动 / 默认 | `mode = medium`（可配置 `modes.default`） |
| `/mode` 或快捷键 | 切换 low → medium → high → ultra |
| CLI / headless | `hyper --mode high`、`GROK_MODE=high`（CI / `-p` 可用；CLI > env > config > 默认） |
| 主会话采样 | 应用当前 mode 的 **`agent` 槽**（若 agent.model = inherit，则仍用用户选的主模型，只调 effort，且受 §4.3b 约束） |
| spawn explore | 应用当前 mode 的 **`explore` 槽**（再走全局优先级，见 §4.1） |
| spawn plan | 应用 **`plan` 槽** |
| general-purpose 子代理 | 默认 **`agent` 槽**（见 §3.2） |
| 缺模型 | 降级链；TUI/日志提示实际解析结果 |

---

## 3. 内置默认（出厂策略）

默认放在 **代码内置**，不依赖仓库提交 `.grok/config.toml`。用户 TOML 只做覆盖。

### 3.1 四档语义（P1：以 inherit + effort 为主）

P1 刻意保守：不依赖 pricing 元数据也能工作。后续可把 low 的 model 升级为 `auto:cheaper`。
表中 effort 为**语义档位**（解析规则见 §4.3：映射到目标模型菜单的对应档）。

| Mode | 说明 | agent | oracle | explore | plan |
|------|------|-------|--------|---------|------|
| **low** | 小而明确的任务，省钱 | inherit / low | inherit / high* | inherit / low | inherit / medium |
| **medium** | 默认平衡 | inherit / medium | inherit / high | inherit / low | inherit / high |
| **high** | 难任务 | inherit / high | inherit / high | inherit / medium | inherit / high |
| **ultra** | 最强 | inherit / 顶档 | inherit / 顶档 | inherit / medium | inherit / 顶档 |

\* oracle 在 P1–P2 可与 agent 同模型，仅 effort 更高；真正旁路会话见 P3。

> **P1 预期管理**：内置默认全 `inherit` model，只调 effort。对 effort 菜单粗/无的模型，
> 切档在主会话几乎无感——**真正的省钱来自换模型**。P1 已支持槽位写显式 slug，
> 想立刻省钱请直接配置（示例 §3.3），不必等 P4 的 `auto:cheaper`。

### 3.2 内置 agent type → 默认槽

| Subagent type | 使用槽 |
|---------------|--------|
| `explore` | `explore` |
| `plan` | `plan` |
| `general-purpose` | `agent`（与主会话同档搭配） |
| 其它 / 自定义 | `agent`，除非 agent 定义或 config 指定 `effort` / slot |

### 3.3 概念 TOML（用户覆盖示例）

```toml
# ~/.grok/config.toml

[modes]
default = "medium"

# 覆盖某一档的某一槽
[modes.low.agent]
model = "kimi-code/kimi-for-coding"
effort = "low"
fallback = ["inherit"]

[modes.low.explore]
model = "kimi-code/kimi-for-coding"
effort = "low"
fallback = ["inherit"]

[modes.ultra.agent]
model = "anthropic/claude-opus-4-6"
effort = "high"
fallback = ["openai/gpt-5.4", "inherit"]

[modes.ultra.oracle]
model = "openai/gpt-5.4"
effort = "high"
fallback = ["inherit"]
```

整包 preset（P2+）：

```toml
[modes]
preset = "kimi_economy"   # 可选：应用内置或用户包，再叠加上面的细粒度覆盖

# 用户自定义包
[mode_presets.my_stack]
default = "high"

[mode_presets.my_stack.modes.low.agent]
model = "..."
effort = "low"
# ... 可只覆盖部分 mode/slot
```

---

## 4. 解析优先级

### 4.1 子代理模型 / effort

先明确**现有实现的真实顺序**（`handle_request.rs` → `resolve_effective_model_config` → `resolve_subagent_sampling_config`）：

```text
effective_runtime.model = spawn override > role > persona   （第一层整体）
  > [subagents.models] pin                                   （第二层）
  > AgentDefinition.model                                    （第三层）
  > inherit 父 session                                        （第四层）
```

已有测试锁定该行为：*"goal runtime override must win over the `[subagents.models]` pin"*（`rest.rs:2658`）；*"`[subagents.models]` wins over agent def"*（`rest.rs:2814`）。**文档与实现以此为准。**

Mode 槽位的插入点（**新增第 6 层**），全链从高到低：

```text
1. spawn 显式 model（Task.model；Tool provenance 先经 catalog 校验）
2. role model
3. persona model
4. [subagents.models] pin      # 逃生舱，保留，优先级不变
5. AgentDefinition.model
6. ★ Session.mode 对应槽位（explore/plan/agent…）
   → 解析 SlotSpec：model 链（§4.2）+ effort（§4.3）
7. inherit 父 session 当前采样配置
```

说明：

- **4 仍是逃生舱**：power user 钉死 explore 用某 slug 时，不依赖 mode 表。
- **6 是本设计新增的默认分档层**：无配置时 explore 不再与主模型完全同一套 effort。
- **resume_from**：继续钉死源会话模型（现有行为），忽略 mode 切换对已 resume 子代的影响。

**Effort 级联**（与 model 平行，现有实现为 `override > role > persona > AgentDefinition`，见 `handle_request.rs:170-171`），mode 槽 effort 插在 AgentDefinition 之后、inherit 之前：

```text
spawn override > role > persona > AgentDefinition.reasoning_effort
  > ★ mode 槽 effort > inherit 父 session
```

### 4.2 槽位 ModelRef 解析（fail-soft）

对单个 `ModelRef` + `fallback[]`：

```text
for ref in [primary] + fallback:
  if ref == "inherit":
    return parent_model
  if ref starts with "auto:":
    if policy implemented and resolves:
      return resolved
    else:
      continue  # 或 degrade to inherit
  if ref in catalog AND usable (auth/available):
    return ref
  else:
    warn(unknown or unavailable)
return parent_model  # 最终保底
  若父也不可用 → 全局 default_model + error 可观测
```

### 4.3 Effort 解析

**复用现有 `ReasoningEffort` 枚举**（`xai-grok-sampling-types`：`low / medium / high / xhigh / max / ultra`，含各 provider 序列化差异，如 Kimi 把 xhigh 写成 `"max"`）。配置里的 effort 字符串直接走现有 serde 解析，**不发明新写法**（写 `xhigh`，不写 `x-high`）。

档位表（§3.1）中 effort 的语义是**相对档位**，落地时映射为「目标模型在该档语义下支持的最高匹配值」：

```text
1. effort == "inherit" 或省略 → 用父 session effort（或模型默认）
2. 否则按模型 reasoning_efforts 菜单（models.rs: model_reasoning_efforts）
   取 ≤ 目标档的最高支持值；模型无 effort 菜单 → ignore（不导致 spawn 失败）
3. 解析失败（非法字符串）→ warn + 按 inherit 处理
```

注意命名：档位名 `ultra` 与枚举变体 `ReasoningEffort::Ultra` 撞名纯属巧合——档位表的 "max effort" 语义是**取该模型菜单顶档**，不绑定某个具体枚举值。

### 4.3b 与已有 effort 旋钮的优先级

Hyper 已有三个用户级 effort 来源，mode 槽 effort **不得**压过用户的显式选择：

| 优先级 | 来源 | 说明 |
|--------|------|------|
| 1（最高） | CLI `--reasoning`（`reasoning_effort_override`） | 用户当次显式指定 |
| 2 | `/effort` 当次会话设置 / `models.default_reasoning_effort` | 用户持久偏好 |
| 3 | mode 槽 effort | 本设计新增，仅作默认 |
| 4 | 模型自身默认 | 兜底 |

即：**mode 槽 effort 只在用户没有显式压过时生效**（对主会话 agent 槽）；子代理槽位的 effort 按 §4.1 级联走，子代理自己的 spawn/role/persona effort 仍高于 mode 槽。

### 4.4 主会话与 mode.agent

| agent.model | 主会话行为 |
|-------------|------------|
| `inherit`（默认） | 用户 `/model` 选的模型不变；仅按档应用 effort（若支持） |
| 显式 slug | 切换 mode 时是否改写主模型：**P1 建议不自动改主模型**，只影响子代理；主模型仍用户显式选。P2 可加 `modes.apply_agent_model_to_session = true` 选择加入 Amp 式「切 mode 换主模型」。 |

**P1 推荐**：mode 主要驱动 **子代理槽** + 可选主会话 **effort**；避免切 mode 抢走用户刚选的主模型。文档写清，避免和 Amp「切 mode 换 Agent 模型」的预期差；可用 `/mode` 帮助文案说明。

---

## 5. 与现有代码的映射

| 现有组件 | 关系 |
|----------|------|
| `SubagentsConfig`（`[subagents]`） | 保留 `models` / `toggle` / `roles` / `personas`；新增旁路读 `ModesConfig` 或同文件 `[modes]` |
| `resolve_subagent_sampling_config` | 在 agent-def 层与 inherit 之间插入 **mode slot**（§4.1 第 6 层） |
| `resolve_effective_model_config` | runtime override（spawn/role/persona）层不动，仍最先判定 |
| `resolve_effective_overrides` | 不变；其 model/effort 仍优先于 mode slot |
| `spawn_subagent` / Task `model` | 保留；schema 文案：仅用户点名时传 model；**新增可选 `effort` 或遵循 session mode** |
| Agent 定义 `ModelOverride` | 保留 |
| `default_models.json` / catalog | slug 校验与 auto 策略的数据源 |
| `log_subagent_model_resolution` | **扩展字段**（mode/slot/via），不新增日志路径 |
| Session / pager UI | 显示当前 agent mode；`/mode`；子代理卡片显示实际 model + 是否 fallback |

建议新模块（实现时）：

- `ModesConfig` 解析（serde）
- `resolve_mode_slot(mode, slot) -> SlotSpec`
- `resolve_model_ref(spec, parent, catalog) -> ResolvedSampling`
- 内置 `BUILTIN_MODES: LazyLock<...>`

---

## 6. 配置 Schema（草案）

```toml
[modes]
default = "medium"                    # low | medium | high | ultra
# preset = "kimi_economy"             # P2+
# apply_agent_model_to_session = false

[modes.low]
description = "Fast, low-cost for small tasks"  # 可选，UI 用

[modes.low.agent]
model = "inherit"
effort = "low"
fallback = []

[modes.low.oracle]
model = "inherit"
effort = "high"

[modes.low.explore]
model = "inherit"
effort = "low"

[modes.low.plan]
model = "inherit"
effort = "medium"

# medium / high / ultra 同结构
```

序列化注意：

- 省略的 mode 或 slot → 合并内置默认（deep merge，不是整表替换）。
- 未知 mode 名 → warn，回退 `default`。
- 未知 slot 键 → ignore + warn。
- `[modes]` 参与现有 **project > user** 配置分层（与 `config_override.rs` 同一套合并机制），项目可在 `.grok/config.toml` 覆盖本仓库的档位搭配。
- fallback 中未知 slug 的检测复用现有 `available_models` 校验（`handle_request.rs` 的 warn + 回退路径），行为与 spawn model 校验一致。

---

## 7. 工具与 Prompt 策略

### 7.1 spawn_subagent

- **默认**：不传 model；子代理走 §4.1（含当前 mode 槽）。
- **可选**（P1 或 P2）：`effort` 参数覆盖本子代档位语义（映射到临时 slot 或一次性 SlotSpec），**不必**暴露四档以外的复杂结构。
- **model**：仅当用户明确要求某模型时由主 agent 填写（保持现有约束文案，可微调）。

### 7.2 系统提示 / 用户指南

- 说明当前 session mode 与四档含义（对齐 Amp 文案风格，简短）。
- 子代理类型与槽位对应关系。
- 如何用 TOML 覆盖与 fallback。

---

## 8. 分阶段实现

### Phase 1 — 开箱分档（优先实现）

**范围：**

1. 内置四档 + 默认 SlotSpec（inherit + 语义档位 effort）。
2. Session 持有 `mode`；`/mode` slash + 配置 `modes.default` + **`--mode` CLI flag + `GROK_MODE` env**（优先级：CLI > env > config > 默认）。
3. spawn `explore` / `plan` / `general-purpose` 走 mode 槽（§4.1 第 6 层）。
4. ModelRef：仅 `inherit` + 显式 slug + fallback[] + 最终 inherit。
5. Effort：按 §4.3 语义档位映射 + §4.3b 用户旋钮优先。
6. 日志：**扩展现有 `log_subagent_model_resolution`**，加 `mode` / `slot` / `via` 字段，不新造日志路径。
7. User-guide 短文 + 本设计链接。

**验收：**

- 无用户 config 时，explore 的 reasoning_effort（或等价）与主会话 high 档可区分（在支持 effort 的模型上）。
- `[subagents.models] explore = "x"` 仍覆盖 mode。
- role / persona model 仍高于 mode 槽（与 §4.1 一致）。
- CLI `--reasoning` 覆盖 mode 槽 effort（§4.3b）。
- 不可用 slug 降级到 inherit，spawn 成功。

**不做：** oracle 真旁路、auto:cheaper、preset 包、切 mode 改主模型、ACP 暴露。

### Phase 2 — 用户自定义档位

1. 完整 TOML deep merge 覆盖。
2. fallback 链完整行为 + UI 提示降级。
3. 可选 `modes.apply_agent_model_to_session`。
4. 示例 config 片段（`hyper config example modes` 或文档）。

### Phase 3 — Oracle 槽落地

1. 可选旁路 reviewer（对齐 omp advisor / Amp Oracle）。
2. mode 表中 oracle 槽真正独立采样。
3. 默认关闭或 high/ultra 才启用（成本控制）。

### Phase 4 — 智能策略与 preset

1. `auto:cheaper` / `auto:stronger`（依赖 catalog 元数据）。
2. 内置/社区 `mode_presets`（xAI / Kimi / Codex 推荐包）。
3. Agents modal 可视化编辑搭配表。

---

## 9. UX 草案

| 表面 | 行为 |
|------|------|
| 状态栏 / footer | `agent:medium`（前缀消歧，区别于 permission mode 指示） |
| `/mode` | 无参：展示四档与当前槽解析结果；`/mode high`：切换。帮助文案写明「agent mode（档位），非 plan mode / 权限模式」 |
| CLI | `--mode <low|medium|high|ultra>`；`GROK_MODE` env；非法值 warn + 回退默认 |
| Shift+Tab | **不动**——继续循环 Normal/Plan/Always-approve（permission mode），与 agent mode 无关 |
| 子代理完成卡片 | `explore · low · model=… (fallback)` |
| 配置错误 | 非致命 warn；不阻止启动 |

---

## 10. 测试计划（实现时）

| 用例 | 期望 |
|------|------|
| 默认 medium，无 config，spawn explore | 走 medium.explore 槽（low effort） |
| mode=ultra，spawn plan | plan 槽取模型菜单顶档 |
| `[subagents.models] explore=good-slug` | 用 good-slug，忽略 mode 槽 |
| role / persona 带 model | 高于 mode 槽（§4.1 第 2/3 层 > 第 6 层） |
| spawn model 显式 | 最高优先 |
| CLI `--reasoning high` + mode low | 主会话 agent 槽 effort 被 CLI 覆盖（§4.3b） |
| resume_from | 源模型，不因 mode 改变 |
| 不支持 effort 的模型 | spawn 成功，effort 忽略 |
| TOML 只覆盖 `modes.low.explore.model` | 其它槽仍内置默认 |
| `--mode bogus` / `GROK_MODE=bogus` | warn + 回退 `modes.default`，不崩 |
| 项目 `.grok/config.toml` 覆盖 `[modes]` | 项目层 > 用户层 |

---

## 11. 风险与决策记录

| 风险 | 缓解 |
|------|------|
| 用户以为切 mode 会像 Amp 一样换主模型 | P1 明确只驱动子代理 + 可选 effort；文案写清 |
| 多 provider 无统一「更便宜」 | P1 不用 auto；P4 再做 |
| "mode" 一词多义（permission mode / plan mode / ACP set_mode） | 产品文案统一 **agent mode（档位）**；footer 显示 `agent:medium`；ACP 不复用 `session/set_mode`；Shift+Tab 不动 |
| 档位 `ultra` 与 `ReasoningEffort::Ultra` 撞名 | 槽位 effort 用语义档位（min/max/枚举），不写死变体名 |
| mode 槽 effort 覆盖用户显式旋钮 | §4.3b：CLI / `/effort` / persisted default 优先 |
| 配置表面膨胀 | deep merge + 内置默认，用户最少只设 `modes.default` |
| 双模型成本 | oracle 默认不跑真旁路直到 P3 |

### 已拍板（历史；已被 §0 修订覆盖）

| 项 | 原决定 | 2026-07-23 |
|----|--------|------------|
| 档位名 | Amp 式 low / medium / high / ultra | **缓停，不实现** |
| 产品名词 | **agent mode（档位）** | 不再引入 |
| 实现顺序 | 先文档后 coding（P1 起） | **取消排期**；Oracle 见 [design-oracle.md](./design-oracle.md) |
| 缺模型 / 自定义 / 优先级链等 | 见上文章节 | 仅作归档参考 |

---

## 12. 文档与代码落点（实现 checklist）

- [ ] `docs/design-modes.md`（本文）
- [ ] user-guide：`NN-modes.md` + README 索引
- [ ] `SubagentsConfig` / 新 `ModesConfig` 解析
- [ ] Session `mode` 状态 + `/mode`
- [ ] `resolve_subagent_sampling_config` 插入 mode 层
- [ ] 内置 `BUILTIN_MODES`
- [ ] 测试 §10
- [ ] `competitive-analysis.md` 回写：Effort/roles 从 ◐ 推进说明

---

## 13. 参考

- Amp Modes: https://ampcode.com/modes  
- Amp Manual: https://ampcode.com/manual  
- Hyper 竞品分析: [competitive-analysis.md](./competitive-analysis.md)  
- 现有 subagent 文档: `crates/codegen/xai-grok-pager/docs/user-guide/16-subagents.md`  
- 现有解析: `xai-grok-shell/.../subagent/mod.rs`（`resolve_subagent_sampling_config`）  
- 现有 overrides: `xai-grok-subagent-resolution`  
