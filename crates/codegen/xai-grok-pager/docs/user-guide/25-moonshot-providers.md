# Moonshot Providers (Kimi open platform)

Grok can call **Moonshot AI** open-platform models with an API key — no xAI
login required for those models. This is Phase 1 of built-in multi-provider
support (Kimi Code subscription OAuth is separate: [26-kimi-code.md](26-kimi-code.md)).

Catalog keys use the form `{platform}/{model}` so the same model id can exist
on more than one host.

| Platform id | Region / host | Default base URL |
|-------------|---------------|------------------|
| `moonshot-cn` | China | `https://api.moonshot.cn/v1` |
| `moonshot-ai` | Global | `https://api.moonshot.ai/v1` |

## Models (official lineup)

Source: [platform.kimi.ai Model List](https://platform.kimi.ai/docs/models)
(2026-07). Offline fallbacks match this list; live `GET {base}/models` with an
API key replaces/extends them.

| Model id | Catalog keys | Context | Notes |
|----------|--------------|---------|--------|
| `kimi-k3` | `moonshot-cn/kimi-k3`, `moonshot-ai/kimi-k3` | 1M | Flagship; always thinking; `reasoning_effort` (docs: `max`) |
| `kimi-k2.7-code` | `…/kimi-k2.7-code` | 256k | Coding; thinking **always on** |
| `kimi-k2.7-code-highspeed` | `…/kimi-k2.7-code-highspeed` | 256k | **HyperSpeed** (~180–260 tok/s); same quality as 2.7 Code |
| `kimi-k2.6` | `…/kimi-k2.6` | 256k | Thinking on/off + Preserved Thinking (`thinking.keep`) |
| `kimi-k2.5` | `…/kimi-k2.5` | 256k | Thinking on/off; no preserved thinking |

Deprecated aliases still injected offline for older configs (prefer the table
above): `kimi-k2-turbo-preview`, `kimi-k2-thinking-turbo`.

Protocol: OpenAI **Chat Completions** (`api_backend = "chat_completions"`).

---

## Request parameters (important)

Moonshot documents **model-specific** request fields. Grok applies these when
building the chat body (do not set conflicting `[model.*]` temperature unless
you know the model accepts it).

| Field | `kimi-k3` | `kimi-k2.7-code` (+ highspeed) | `kimi-k2.6` | `kimi-k2.5` |
|-------|-----------|--------------------------------|-------------|-------------|
| `reasoning_effort` | `"max"` (default) | not used | not used | not used |
| `thinking.type` | — | omit (always on) | `enabled` / `disabled` | `enabled` / `disabled` |
| `thinking.keep` | — | always `all` server-side | `null` / `"all"` | not supported |
| `temperature` / `top_p` / penalties | normal | **fixed** — omit (server 1.0 / 0.95 / 0) | fixed — omit | fixed — omit |
| `max_tokens` | default **32768** | default **32768** | default **32768** | default **32768** |
| `tool_choice` | auto/none preferred | **only** `auto` / `none` | auto/none preferred | auto/none preferred |

Multi-step tool calls must keep assistant `reasoning_content` in `messages`
(Grok’s chat path already maps reasoning into that field). For K2.6 tool
loops Grok sets `thinking.keep = "all"`.

Details: [Thinking Mode](https://platform.kimi.ai/docs/guide/use-kimi-k2-thinking-model),
[K2.7 Code params](https://platform.kimi.ai/docs/guide/kimi-k2-7-code-quickstart).

---

## Quick start (environment)

```bash
# China open platform
export GROK_MOONSHOT_CN_API_KEY="sk-..."

# or global open platform
export GROK_MOONSHOT_AI_API_KEY="sk-..."

# optional: one key for both platforms
export GROK_MOONSHOT_API_KEY="sk-..."
# also accepted: MOONSHOT_API_KEY
```

```bash
grok models | grep moonshot
grok -m moonshot-cn/kimi-k2.7-code-highspeed -p "ping"
grok -m moonshot-cn/kimi-k3 -p "ping"
# TUI: /model moonshot-cn/kimi-k2.6
```

---

## Config file

`~/.grok/config.toml`:

```toml
[platforms.moonshot-cn]
api_key = "sk-..."

[platforms.moonshot-ai]
api_key = "sk-..."

[models]
# optional: make a Moonshot model the session default
default = "moonshot-cn/kimi-k2.7-code"
```

**Credential precedence** (first match wins):

1. Per-model `[model.<id>].api_key` / `env_key`
2. Platform-scoped env (`GROK_MOONSHOT_CN_API_KEY` / `GROK_MOONSHOT_AI_API_KEY`)
3. Generic env (`GROK_MOONSHOT_API_KEY` or `MOONSHOT_API_KEY`)
4. UI-pasted key from `/providers moonshot-cn <api_key>` (stored in
   `~/.grok/auth.json` under `platform/moonshot-cn`)
5. `[platforms.<id>].api_key` in config.toml

### TUI setup

```
/providers
/providers moonshot-cn sk-...
/model moonshot-cn/kimi-k3
```

Clear a stored key with `/providers moonshot-cn clear`.

API keys are never written back into re-serialized config dumps and must not
be committed to shared repos.

---

## Live catalog sync

When a platform API key is present and `remote_fetch` is enabled, Grok calls
`GET {base}/models` and merges coding-family models (`kimi-k*`) into the
catalog. Live entries override offline fallbacks (context window, think
efforts, display names).

---

## Moonshot vs Kimi Code subscription

| | Moonshot open API | Kimi Code subscription |
|--|-------------------|-------------------------|
| Auth | API key | Device OAuth (`grok login --kimi`) |
| Hosts | `api.moonshot.cn` / `api.moonshot.ai` | `api.kimi.com/coding` |
| Model ids | `kimi-k3`, `kimi-k2.7-code`, … | live list (often `k3`, `kimi-for-coding`, …) |
| Docs | this page | [26-kimi-code.md](26-kimi-code.md) |
