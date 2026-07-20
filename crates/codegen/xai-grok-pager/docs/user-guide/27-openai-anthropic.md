# OpenAI & Anthropic (built-in platforms)

Grok can call **OpenAI** and **Anthropic** directly (no separate gateway).
Model entries are curated from the Pi `models.generated` catalog (same source
as pi_agent_rust / pi-mono) and live under `{platform}/{model}` keys.

| Platform id | Auth | Default base | Protocol |
|-------------|------|--------------|----------|
| `openai` | API key | `https://api.openai.com/v1` | Chat Completions / **Responses** (per model) |
| `anthropic` | API key (`x-api-key`) | `https://api.anthropic.com/v1` | **Messages** |

Moonshot / Kimi Code remain separate; see
[25-moonshot-providers.md](25-moonshot-providers.md) and
[26-kimi-code.md](26-kimi-code.md).

---

## Quick start

```bash
# OpenAI
export GROK_OPENAI_API_KEY="sk-..."     # or OPENAI_API_KEY

# Anthropic
export GROK_ANTHROPIC_API_KEY="sk-ant-..."  # or ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN

./target/debug/xai-grok-pager models | grep -E 'openai/|anthropic/'
./target/debug/xai-grok-pager -m openai/gpt-4.1 -p "ping"
./target/debug/xai-grok-pager -m anthropic/claude-sonnet-4-5 -p "ping"
```

Config file (`~/.grok/config.toml`):

```toml
[platforms.openai]
api_key = "sk-..."

[platforms.anthropic]
api_key = "sk-ant-..."

[models]
default = "anthropic/claude-sonnet-4-5"
```

**Credential precedence:** env (`GROK_*` then common aliases) > `[platforms.*].api_key`
> per-model `[model.*]`.

---

## Offline catalog (Pi-curated)

Shipped in `xai-grok-models/platform_catalog.json` (generated from Pi
`models.generated.ts`). Typical entries:

**OpenAI** (mostly `api_backend = responses`):

- `openai/gpt-5.2`, `gpt-5.1`, `gpt-5.1-codex`, `gpt-5`, `gpt-5-mini`, `gpt-5-codex`
- `openai/gpt-4.1`, `gpt-4.1-mini`, `gpt-4o`, `gpt-4o-mini`
- `openai/o3`, `o3-mini`, `o4-mini`

**Anthropic** (`api_backend = messages`, `auth_scheme = x-api-key`,
`anthropic-version: 2023-06-01`):

- `anthropic/claude-opus-4-5`, `claude-sonnet-4-5`, `claude-haiku-4-5`
- `anthropic/claude-opus-4-1`, `claude-sonnet-4-0`, `claude-opus-4-0`
- `anthropic/claude-3-7-sonnet-latest`, `claude-3-5-haiku-latest`

We do **not** auto-expand the full org `/models` list for OpenAI/Anthropic
(too large / noisy). To add a model:

1. Append to `platform_catalog.json`, or
2. Declare `[model."openai/…"]` / `[model."anthropic/…"]` with the usual fields.

---

## Env overrides

```bash
export GROK_OPENAI_BASE_URL="https://api.openai.com/v1"
export GROK_ANTHROPIC_BASE_URL="https://api.anthropic.com/v1"
```

---

## Notes

- Anthropic uses **Messages** + `x-api-key` (not Bearer). Grok sets this via
  `auth_scheme` and injects `anthropic-version`.
- OpenAI flagship entries default to **Responses** (Pi mapping); override with
  `[model."openai/…"].api_backend = "chat_completions"` if needed.
- No LiteLLM / external gateway required for these platforms.
