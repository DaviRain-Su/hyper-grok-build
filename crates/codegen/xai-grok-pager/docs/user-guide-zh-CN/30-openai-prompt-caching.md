# OpenAI 提示缓存

Hyper 如何用 **OpenAI 兼容** 的前缀缓存降低延迟与输入费用。只覆盖 OpenAI / Responses / Chat Completions 路径，**不**扩展 Anthropic Messages 的 `cache_control`。

> **English:** [OpenAI Prompt Caching](../user-guide/30-openai-prompt-caching.md)

---

## 是什么

每一轮 agent 都会重发 system、工具定义和历史。支持 **自动前缀缓存** 的厂商可对匹配的 token 前缀复用 KV，只 prefill 新增后缀。

OpenAI 相关字段：

| 字段 | 作用 |
|------|------|
| `prompt_cache_key` | 粘性路由键，让同会话轮次落在同一缓存桶 |
| `prompt_cache_retention` | 仅 Responses：`in_memory`（默认 TTL）或 `24h`（延长） |
| usage `cached_tokens` | 命中缓存的输入 token 数 |

Hyper 将 **`prompt_cache_key` = session id**（最多 64 字符）打在：

- **Responses**（OpenAI BYOK 与尊重该字段的兼容网关）  
- **Chat Completions**（OpenAI 现行字段；多数网关忽略未知字段，但**严格**兼容代理可能直接拒请求 —— 优先 Responses 或确认接受 `prompt_cache_key` 的网关）  

- **Codex** 方言（同一路径）

Anthropic Messages 维持现有 system 块行为；本文档**不**做 Claude 多断点 `cache_control` 扩展。

---

## UI 里能看到什么

`/usage` 会话用量会显示：

- 输入 tokens 及 **cached** 数量  
- 有缓存命中时的 **命中率**  

部分网关不回报 `cached_tokens` 时显示为 0 / 无命中率行，属正常。

---

## 可选延长保留（Responses）

默认是厂商短 TTL（空闲常见约 5–10 分钟）。对 **OpenAI Responses**（非 Codex）可请求更长保留：

```bash
export GROK_PROMPT_CACHE_RETENTION=24h
# 别名: long | 24
# 短 TTL: in_memory | short
```

说明：

- **Codex** 会剥掉 `prompt_cache_retention`（ChatGPT 后端会拒）  
- 网关可能忽略  
- 延长 TTL 可能提高 **cache write** 费用；只在会长时间空闲时再开  

---

## 如何提高命中率

参考 [Earendil 文章](https://earendil.com/posts/prompt-caching/) 与 OpenAI 自动缓存：

1. 会话内尽量 **不换模型**  
2. 避免中途大改 **工具列表**（MCP 增减）  
3. **少中段改写历史**；中段 prune 会打断前缀  
4. **compact / rewind** 会故意开新前缀（预期 miss）  
5. 空闲超过 TTL → 下一轮全量重计费  

Scoped 循环（**Alt+]**）换模型时 miss 是预期行为。

---

## 相关

- [模型、平台与 Scoped 选择](29-models-providers-and-scoped-selection.md)  
- [OpenAI Codex](28-openai-codex.md) · [OpenAI 与 Anthropic](27-openai-anthropic.md)  
- [监控与用量](24-monitoring-usage.md)  
