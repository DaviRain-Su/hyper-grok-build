# 跨会话记忆（Cross-Session Memory）

记忆功能让 Grok 能够回忆先前会话中的事实、决策与模式。Grok 会对你保存的信息进行索引并自动检索，使新会话可以复用相关上下文。

---

## 什么是记忆？

没有记忆时，每次 Grok 会话都从零开始：模型对先前会话一无所知。启用记忆后，Grok 可以：

- 回忆你此前说明过的项目约定。
- 复用已验证有效的调试步骤。
- 将会话间的架构决策延续下去。
- 避免重复询问它已有答案的问题。

记忆功能目前为实验性功能，默认关闭。

---

## 启用记忆

### 按会话标志

```bash
grok --experimental-memory
```

### 环境变量

```bash
export GROK_MEMORY=1
grok
```

### 配置文件（持久化）

```toml
# ~/.grok/config.toml
[memory]
enabled = true
```

### 强制禁用

即使其他设置已启用记忆，也可按如下方式禁用：

```bash
grok --no-memory
```

或：

```bash
export GROK_MEMORY=0
```

`--no-memory` 标志具有绝对最高优先级，始终会禁用记忆。

### 会话中途切换

无需重启即可在会话中打开或关闭记忆：

```
/memory on
/memory off
```

该切换仅作用于当前会话——不会写入 `config.toml`。关闭后会移除对记忆工具的访问，但磁盘上已有文件会保留。打开时会重新初始化记忆存储并注册记忆工具。

也可在 `/memory` 模态框内按 `t` 进行切换。

### 优先级顺序

1. `--no-memory` CLI 标志（始终禁用）
2. `--experimental-memory` CLI 标志（启用）
3. `GROK_MEMORY` 环境变量：`1`/`true` 启用，`0`/`false` 禁用
4. config.toml 中的 `[memory]` 配置段
5. 默认：禁用

---

## 记忆如何存储

记忆以 Markdown 文件形式存储在 `~/.grok/memory/` 下：

| 位置 | 范围 | 说明 |
|----------|-------|-------------|
| `~/.grok/memory/MEMORY.md` | 全局 | 适用于所有项目的事实 |
| `~/.grok/memory/<project-slug>-<hash8>/MEMORY.md` | 工作区 | 项目特定的约定与上下文 |
| `~/.grok/memory/<project-slug>-<hash8>/sessions/` | 会话 | 按会话的摘要与日志 |

Grok 会为每个工作区目录附加仓库身份的短哈希后缀。若该目录是带有 `origin` 远程的 Git 仓库，身份取 `origin` 远程的 `org/repo` 形式；否则使用目录路径。同一仓库的克隆与 worktree 共享同一个 `origin` 远程，因此也共享同一记忆目录。

SQLite 索引支持在所有记忆文件上做混合检索：
- **FTS5** 提供全文检索，用于关键词匹配。
- **vec0** 提供向量检索，用于语义相似度。向量检索为可选，需要 embedding。

---

## 自动保存

会话结束时，Grok 会将该会话的结构化元数据摘要写入当日会话日志。摘要包含：

- 消息计数（用户、助手与工具结果）。
- 主题：会话中前几条有实质内容的用户提示，最多五条。
- 会话日期与时间（UTC）。

Grok 从对话元数据构建摘要，不调用 LLM，也不增加延迟。对琐碎会话会跳过保存——实质提示少于三条，或用户文本少于 50 字节的会话。

摘要不会记录工具使用、文件路径或 shell 命令。会话 ID 构成日志文件名的一部分。若要关闭自动保存，设置 `session.save_on_end = false`。若需更完整地捕获决策、模式与推理过程，请使用 `/flush`。

---

## 用 /flush 保存更丰富的知识

若要更完整地捕获决策、模式、调试流程、API 发现等内容，请在 TUI 中使用 `/flush`：

```
/flush
```

这会触发由 LLM 生成的、关于当前会话最重要内容的摘要，并写入带日期的会话日志。该摘要会被索引，可在后续会话中检索。

在希望保留重要上下文时使用 `/flush`：
- 压缩（compaction）之前（压缩会丢弃旧的对话轮次）
- 一次高效调试会话结束时
- 发现重要模式或约定之后

---

## 使用记忆

### 记住（Remember）

请 Grok 记住某事，它会将该笔记追加到某个 `MEMORY.md` 文件——项目相关内容写入工作区文件，跨项目偏好写入全局的 `~/.grok/memory/MEMORY.md`：

```
> remember to always open PR links after pushing
```

Grok 会将条目以持久陈述形式记录在有组织的标题下，例如 `## Preferences`、`## Project Context` 或 `## Debugging`。文件监视器会在下次记忆检索时重新索引该变更，因此新条目在当前会话内即可被搜索到。

也可通过 `/remember` 命令直接保存笔记：

```
/remember always open PR links after pushing
```

无参数运行 `/remember` 会进入记住模式，你输入的下一行将成为笔记。无论哪种方式，Grok 都会打开审阅面板显示该笔记（可用 `Tab` 在可选改写版本间切换）；仅在你确认后才会写入。保存时 Grok 会显示 `Memory saved to ~/.grok/memory/MEMORY.md`。

### 遗忘（Forget）

请 Grok 忘记某事，它会查找并删除匹配条目：

```
> forget the snake_case convention
```

遗忘为尽力而为：模型会搜索记忆并删除匹配条目。若要保证删除，请直接编辑 `~/.grok/memory/` 下的文件并自行删除该条目。要定位文件，可打开 `/memory` 浏览器并按 `y` 复制其路径。

### 回忆（Recall）

询问 Grok 记得什么：

```
> what do you remember?
```

Grok 会在所有记忆文件中搜索，并按来源分组汇总：全局偏好、项目知识与会话历史。使用 `/memory` 可浏览原始文件。

### 直接编辑

你可以直接编辑 `~/.grok/memory/` 下的记忆文件。文件监视器会在下次记忆检索时重新索引你的更改。使用 `/flush` 可立即保存当前会话，使用 `/dream` 可将会话日志整理为有组织的主题。

---

## 用 /memory 浏览记忆

`/memory` 命令会打开一个模态框，显示所有记忆文件：

```
/memory
```

文件按范围分组：
- **全局（Global）** —— 跨项目记忆（`MEMORY.md`）。
- **工作区（Workspace）** —— 项目特定记忆（`MEMORY.md`）。
- **会话（Sessions）** —— 按会话的摘要，按时间倒序。

模态框采用分栏布局：左侧为文件列表，右侧为只读内容预览。在列表中移动时预览会更新。

### 键盘快捷键

| 按键 | 操作 |
|-----|--------|
| `↑`/`↓` 或 `j`/`k` | 在文件列表中移动 |
| `PgUp`/`PgDn` | 跳转 10 条 |
| `/` | 过滤文件列表 |
| `y` | 将所选文件路径复制到剪贴板 |
| `x` | 删除所选会话文件（再按一次 `x` 确认） |
| `t` | 打开或关闭记忆 |
| `Ctrl+F` | 切换全屏 |
| `Esc` | 关闭模态框，或退出过滤模式 |

预览窗格为只读。可用鼠标滚轮或拖动滚动条滚动。只能删除会话文件，不能删除全局或工作区的 `MEMORY.md`。

当记忆模态框内容区宽度不足 80 列时，会隐藏预览窗格，仅显示文件列表。

也可从命令面板打开 `/memory`。

---

## 记忆通知

使用 `/remember` 保存笔记时，Grok 会在回滚输出中确认：

```
Memory saved to ~/.grok/memory/MEMORY.md
```

后台保存——flush、dream 与会话结束——静默运行，不会在回滚输出中发消息。可随时用 `/memory` 浏览 Grok 已存储的内容。

---

## 用 /dream 进行 Dream 整合

`/dream` 命令将分散的记忆片段整合为有组织的主题：

```
/dream
```

Dream 会将会话日志与记忆条目重组为连贯、去重的知识库，从而降低噪声并随时间提升检索质量。`/dream` 需要启用记忆。

### 自动 Dream（Auto-Dream）

Dream 也会自动运行。默认情况下，Grok 在会话结束时检查整合门槛；当经过足够时间且累积足够多会话后，会运行一次 Dream：

```toml
[memory.dream]
enabled = true     # Run automatic consolidation (default: true)
min_hours = 4      # Minimum hours between consolidations
min_sessions = 3   # Minimum sessions since the last consolidation
# check_interval_secs is unset by default, so Dream runs only at session end.
# Set it to a positive number of seconds to also check on a periodic interval.
```

---

## 记忆如何影响提示

### 首轮注入（First-Turn Injection）

在每个会话的第一轮，Grok 会自动搜索与当前项目相关的记忆，并将其作为上下文注入。这意味着 Grok 无需提醒即可从先前会话的知识起步。

首轮注入可配置：

```toml
[memory.initial_injection]
enabled = true     # Enable or disable first-turn injection
min_score = 0.0    # Optional score threshold; unset by default, which applies no filtering
```

### 压缩之后

自动压缩（auto-compaction）之后也会搜索记忆，以恢复可能已被丢弃的相关上下文。

---

## 记忆检索

Grok 会自动搜索记忆，你也可以在对话中手动触发检索：

```
Search memory for "auth middleware patterns"
Read my workspace MEMORY.md
```

模型可使用两个记忆工具：
- `memory_search` —— 在全部记忆上做混合检索（向量 + 全文）
- `memory_get` —— 按路径读取指定记忆文件

### 混合评分（Hybrid Scoring）

记忆检索使用加权组合：
- **向量相似度（语义）** —— 权重：0.7
- **BM25 文本相似度（关键词）** —— 权重：0.3

结果会按最低分数阈值过滤（默认：0.35）。

### 来源权重（Source Weights）

每个记忆来源都有应用于其分数的权重乘数。所有来源默认均为 `1.0`，可在 `[memory.search.source_weights]` 下调整：

| 来源 | 权重 | 说明 |
|--------|--------|-------------|
| `workspace` | 1.0 | 项目特定记忆 |
| `session` | 1.0 | 会话日志 |
| `global` | 1.0 | 跨项目记忆 |

### 时间衰减（Temporal Decay）

会话记忆会随时间衰减，从而优先最近会话：

```toml
[memory.search.temporal_decay]
enabled = true           # Enable time-based decay
half_life_days = 7.0     # Score halves after this many days
```

仅会话分块会衰减。全局与工作区记忆不受影响，因为它们包含经过整理的长期知识。

### MMR（最大边际相关性，Maximal Marginal Relevance）

MMR 重排会惩罚冗余结果以提升多样性：

```toml
[memory.search.mmr]
enabled = false          # Opt-in diversity re-ranking
lambda = 0.7             # 0.0 = max diversity, 1.0 = pure relevance
```

---

## CLI 命令

`grok memory` 命令用于从 shell 管理记忆。它有一个子命令 `clear`：

```bash
# Clear workspace memory (MEMORY.md, sessions/, and index.sqlite). This is the default scope.
grok memory clear

# The same scope, stated explicitly
grok memory clear --workspace

# Clear the global MEMORY.md
grok memory clear --global

# Clear both workspace and global memory
grok memory clear --all

# Skip the confirmation prompt (-y is the short form)
grok memory clear --yes
```

若要从 shell 编辑记忆，请直接在编辑器中打开文件——例如 `$EDITOR ~/.grok/memory/MEMORY.md`。

---

## 配置参考

### 核心设置（`[memory]`）

| 键 | 默认值 | 说明 |
|-----|---------|-------------|
| `enabled` | `false` | 启用记忆 |
| `session.save_on_end` | `true` | 在会话结束时写入元数据摘要 |
| `watcher.enabled` | `true` | 监视 `~/.grok/memory/` 的外部编辑并重新索引 |

### 索引设置（`[memory.index]`）

| 键 | 默认值 | 说明 |
|-----|---------|-------------|
| `max_chunk_chars` | `1600` | 分块最大字符数 |
| `chunk_overlap_chars` | `320` | 分块之间的字符重叠 |

### Embedding 设置（`[memory.embedding]`）

| 键 | 默认值 | 说明 |
|-----|---------|-------------|
| `provider` | `"api"` | Embedding 提供方（当前为 `"api"`） |
| `model` | *（提供方默认）* | Embedding 模型名称 |
| `dimensions` | `1024` | Embedding 向量维度 |

### 检索设置（`[memory.search]`）

| 键 | 默认值 | 说明 |
|-----|---------|-------------|
| `max_results` | `6` | 最大检索结果数 |
| `min_score` | `0.35` | 最低相关性分数 |
| `vector_weight` | `0.7` | 向量相似度权重 |
| `text_weight` | `0.3` | BM25 文本相似度权重 |

### 首轮注入设置（`[memory.initial_injection]`）

| 键 | 默认值 | 说明 |
|-----|---------|-------------|
| `enabled` | `true` | 启用首轮记忆注入 |
| `min_score` | 未设置 | 首轮结果的分数阈值。未设置时 Grok 不应用阈值，等价于 `0.0`。 |

### Dream 设置（`[memory.dream]`）

| 键 | 默认值 | 说明 |
|-----|---------|-------------|
| `enabled` | `true` | 启用自动 Dream 整合 |
| `min_hours` | `4` | 两次整合之间的最短小时数 |
| `min_sessions` | `3` | 距上次整合后的最少会话数 |
| `stale_lock_secs` | `3600` | 陈旧整合锁被回收前的秒数 |
| `check_interval_secs` | 未设置 | 周期性检查间隔（秒）。未设置时 Dream 仅在会话结束时运行。 |

### Flush 设置（`[compaction.memory_flush]`）

Flush 配置在 `[compaction]` 下，而非 `[memory]`，因为它属于压缩行为。

| 键 | 默认值 | 说明 |
|-----|---------|-------------|
| `enabled` | `true` | 启用压缩前的记忆 flush |
| `soft_threshold_tokens` | `4000` | 触发 flush 时相对压缩阈值预留的 token 余量 |
| `max_flush_write_chars` | `8000` | flush 可写入记忆的最大字符数 |
| `flush_model` | 未设置 | flush 轮次使用的模型。未设置时 Grok 使用会话的主模型。 |
| `idle_timeout_secs` | 未设置 | 后台 flush 前的空闲秒数。未设置时仅在压缩前运行 flush。 |
| `semantic_dedup_threshold` | 未设置 | 对 flush 内容去重的余弦相似度阈值。未设置时默认为 `0.92`。 |

### 裁剪设置（`[compaction.pruning]`）

裁剪配置在 `[compaction]` 下，而非 `[memory]`，因为它属于压缩行为。

| 键 | 默认值 | 说明 |
|-----|---------|-------------|
| `enabled` | `true` | 启用工具结果裁剪 |
| `keep_last_n_turns` | `3` | 最近若干轮的工具结果永不裁剪 |
| `soft_trim_threshold` | `4000` | 超过该字符阈值时对旧工具结果做软裁剪 |
| `soft_trim_head` | `1500` | 软裁剪结果保留的起始字符数 |
| `soft_trim_tail` | `1500` | 软裁剪结果保留的末尾字符数 |
| `hard_clear_age_turns` | `10` | 超过该轮次年龄后，工具结果替换为占位符 |

---

## 记忆陈旧性

当会话记忆较旧时，Grok 会在检索结果中附加陈旧性说明。越旧的结果会有更强的提醒，请你在依赖前核实当前状态。这些说明有助于发现可能已不再准确的已存事实。全局与工作区记忆不会附加陈旧性说明，因为它们保存的是经过整理的长期知识。

---

## 文件监视器

默认情况下，Grok 监视 `~/.grok/memory/` 的外部文件变更。若你直接编辑记忆文件（例如在编辑器中），下次记忆检索时会自动拾取这些变更：

- 新建或修改的文件会被重新索引。
- 已删除文件的过时分块会从索引中移除。

```toml
[memory.watcher]
enabled = true    # default
```

---

## 故障排除

### 记忆未生效

1. 确认记忆已启用：检查 `grok inspect` 的输出。
2. 检查标志：`grok --experimental-memory` 或 `GROK_MEMORY=1`。
3. 检查是否被 `--no-memory` 或 `GROK_MEMORY=0` 覆盖了你的配置。

### 记忆未出现在会话中

记忆在第一轮注入。若你在启用记忆之前就已开始会话，请用 `/new` 开启新会话。

### 查看记忆文件

在 TUI 中使用 `/memory` 浏览全部记忆文件并预览。也可直接访问：

```bash
ls ~/.grok/memory/
cat ~/.grok/memory/MEMORY.md
$EDITOR ~/.grok/memory/MEMORY.md
```

### 调试日志

```bash
RUST_LOG=debug GROK_LOG_FILE=/tmp/grok.log grok
grep "memory" /tmp/grok.log
```
