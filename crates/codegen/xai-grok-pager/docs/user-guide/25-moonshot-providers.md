# Moonshot Providers (Kimi open platform)

Grok can call **Moonshot AI** open-platform models (Kimi K2 family) with an
API key — no xAI login required for those models. This is Phase 1 of
built-in multi-provider support (subscription-style Kimi Code login comes
later).

Catalog keys use the form `{platform}/{model}` so the same model id can exist
on more than one host.

| Platform id | Region / host | Default base URL |
|-------------|---------------|------------------|
| `moonshot-cn` | China | `https://api.moonshot.cn/v1` |
| `moonshot-ai` | Global | `https://api.moonshot.ai/v1` |

Built-in offline models (also overridable via `[model.*]`):

- `moonshot-cn/kimi-k2-turbo-preview`
- `moonshot-cn/kimi-k2-thinking-turbo`
- `moonshot-ai/kimi-k2-turbo-preview`
- `moonshot-ai/kimi-k2-thinking-turbo`

Protocol: OpenAI **Chat Completions** (`api_backend = "chat_completions"`).

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
grok models
grok -m moonshot-cn/kimi-k2-turbo-preview -p "ping"
# or in the TUI:
# /model moonshot-cn/kimi-k2-turbo-preview
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
default = "moonshot-cn/kimi-k2-turbo-preview"
```

**Credential precedence** (first match wins):

1. Per-model `[model.<id>].api_key` / `env_key`
2. Platform-scoped env (`GROK_MOONSHOT_CN_API_KEY` / `GROK_MOONSHOT_AI_API_KEY`)
3. Generic env (`GROK_MOONSHOT_API_KEY` or `MOONSHOT_API_KEY`)
4. `[platforms.<id>].api_key` in config.toml

API keys are never written back into re-serialized config dumps and must not
be committed to shared repos.

---

## Per-model overrides

You can override any field of a built-in entry the same way as other custom
models:

```toml
[model."moonshot-cn/kimi-k2-turbo-preview"]
temperature = 0.3
context_window = 262144
# base_url inherits the platform default unless set:
# base_url = "https://api.moonshot.cn/v1"
```

To add another Moonshot model id that is not in the built-in list:

```toml
[model."moonshot-cn/my-custom-kimi"]
model = "kimi-k2-0905-preview"
base_url = "https://api.moonshot.cn/v1"
name = "Kimi K2 0905"
context_window = 262144
api_backend = "chat_completions"
env_key = ["GROK_MOONSHOT_CN_API_KEY", "GROK_MOONSHOT_API_KEY", "MOONSHOT_API_KEY"]
```

If the catalog key starts with `moonshot-cn/` or `moonshot-ai/`, Grok also
applies the platform credential stamp from `[platforms.*]` / env.

---

## Dev / test base URL overrides

```bash
export GROK_MOONSHOT_CN_BASE_URL="http://127.0.0.1:8080/v1"
export GROK_MOONSHOT_AI_BASE_URL="http://127.0.0.1:8081/v1"
```

---

## Notes

- xAI models and auth continue to work as before; Moonshot entries are additive.
- Hosted xAI tools (server-side web/x search) are not available on Moonshot.
- For **Kimi Code subscription** login (device OAuth), see
  [Kimi Code Subscription](26-kimi-code.md).
- For a generic OpenAI-compatible gateway (LiteLLM, etc.), continue to use
  [Custom Models](11-custom-models.md) / `models_base_url`.

---

## Troubleshooting

```bash
# Is the model in the catalog?
grok models | grep moonshot

# Does the key resolve? (do not paste secrets into chat logs)
test -n "$GROK_MOONSHOT_CN_API_KEY" && echo "cn env set"

# Request logging
RUST_LOG=debug GROK_LOG_FILE=/tmp/grok.log grok -m moonshot-cn/kimi-k2-turbo-preview -p "hi"
```

Common failures:

| Symptom | Check |
|---------|--------|
| Model not listed | Rebuild from a tree that includes platform support; restart the TUI |
| 401 Unauthorized | Key env/config for the correct platform; CN vs AI hosts differ |
| Connection error | Network / region; try the matching host (`moonshot.cn` vs `moonshot.ai`) |
