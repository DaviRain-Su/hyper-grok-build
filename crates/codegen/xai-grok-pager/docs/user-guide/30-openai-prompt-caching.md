# OpenAI Prompt Caching

How Hyper reuses **OpenAI-compatible** prompt (prefix) caches to cut latency and input cost. This is the OpenAI / Responses / Chat Completions path only — **not** Anthropic Messages `cache_control`.

> **中文:** [OpenAI 提示缓存](../user-guide-zh-CN/30-openai-prompt-caching.md)

---

## What it is

On each turn the agent resends system prompt, tools, and history. Providers that support **automatic prefix caching** can reuse KV work for the matching token prefix and only prefill the new suffix.

OpenAI exposes:

| Field | Role |
|-------|------|
| `prompt_cache_key` | Sticky routing key so related turns land on the same cache bucket |
| `prompt_cache_retention` | Responses only: `in_memory` (default TTL) or `24h` (extended) |
| usage `cached_tokens` | How many input tokens were served from cache |

Hyper sets **`prompt_cache_key` = session id** (clamped to 64 chars) on:

- **Responses** API (including OpenAI BYOK and OpenAI-compatible gateways that honor the field)
- **Chat Completions** API (OpenAI’s modern field; most gateways ignore unknown fields, but a **strict** OpenAI-compat proxy might reject the request — prefer Responses or a gateway known to accept `prompt_cache_key`)
- **Codex** dialect (same key; ChatGPT backend already used this path)

Anthropic Messages still uses its existing system-block behavior; this doc does **not** expand Claude-style multi-breakpoint `cache_control`.

---

## What you get in the UI

`/usage` (session usage block) shows:

- Input tokens with **cached** count  
- **Cache hit rate** when any cached tokens were reported  

Providers that never report `cached_tokens` will show 0 cached / no hit-rate line — that is expected for some gateways.

---

## Optional extended retention (Responses)

Default is provider short TTL (often ~5–10 minutes of idle). For **OpenAI Responses** (not Codex), you can request longer retention:

```bash
export GROK_PROMPT_CACHE_RETENTION=24h
# aliases: long | 24
# short TTL explicit: in_memory | short
```

Notes:

- **Codex** (`openai-codex/*`) strips `prompt_cache_retention` — the ChatGPT backend rejects it.  
- Gateways may ignore retention.  
- Extended TTL can cost more on **cache writes**; only enable if you idle a lot mid-session.

---

## How to keep cache hits high

From [Earendil’s agent caching write-up](https://earendil.com/posts/prompt-caching/) and OpenAI’s automatic caching:

1. **Stay on one model** within a session when possible (model switch invalidates KV).  
2. **Avoid mid-session tool list thrash** (MCP add/remove reshuffles tools early in the prefix).  
3. **Prefer append over rewriting history** — aggressive prune mid-transcript busts the prefix.  
4. **Compaction / rewind** intentionally starts a new prefix (expected miss, not a bug).  
5. **Long idle** past the provider TTL → next turn re-bills the full prefix.

Scoped model cycling (**Alt+]** / `/scoped-models`) will miss cache when the model changes — that is expected.

---

## Related

- [Models, Providers, Scoped Selection](29-models-providers-and-scoped-selection.md)  
- [OpenAI Codex](28-openai-codex.md) · [OpenAI & Anthropic](27-openai-anthropic.md)  
- [Monitoring & usage](24-monitoring-usage.md)  
