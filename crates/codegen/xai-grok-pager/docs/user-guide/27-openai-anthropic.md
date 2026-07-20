# Multi-provider catalog (official Pi)

Grok’s third-party model catalog is generated from the official
[earendil-works/pi](https://github.com/earendil-works/pi) package
`@earendil-works/pi-ai` (`packages/ai/src/providers/data/*.json` after
`npm run generate-models`).

**~400+ tool-capable models**, keys `{platform}/{model_id}`. No separate
gateway process — each platform talks to the vendor API with Grok’s existing
`chat_completions` / `responses` / `messages` backends.

## Platforms (API key unless noted)

| Platform id | Env keys (examples) | Default base | Notes |
|-------------|---------------------|--------------|--------|
| `openai` | `OPENAI_API_KEY` | `api.openai.com/v1` | Responses (most models) |
| `anthropic` | `ANTHROPIC_API_KEY` | `api.anthropic.com/v1` | Messages + `x-api-key` |
| `kimi-code` | OAuth `login --kimi` | `api.kimi.com/coding/v1` | **Messages** + adaptive thinking (Pi official) |
| `moonshot-cn` / `moonshot-ai` | `GROK_MOONSHOT_*` | moonshot.cn / .ai | Chat Completions |
| `deepseek` | `DEEPSEEK_API_KEY` | `api.deepseek.com` | |
| `groq` | `GROQ_API_KEY` | `api.groq.com/openai/v1` | |
| `openrouter` | `OPENROUTER_API_KEY` | `openrouter.ai/api/v1` | Large catalog |
| `together` / `fireworks` / `cerebras` / `nvidia` | matching `*_API_KEY` | vendor URLs | |
| `minimax` / `minimax-cn` | `MINIMAX_API_KEY` | | |
| `zai` / `zai-coding-cn` | `ZAI_API_KEY` | | |
| `ollama` | `OLLAMA_API_KEY` | `ollama.com/v1` | Cloud models; override `GROK_OLLAMA_BASE_URL` for local |
| `xai-direct` | `XAI_API_KEY` | `api.x.ai/v1` | BYOK xAI (vs Grok login session) |

`mistral` is reserved; Pi Mistral uses a proprietary conversations API we do
not implement yet.

Also see [25-moonshot-providers.md](25-moonshot-providers.md) and
[26-kimi-code.md](26-kimi-code.md).

---

## Quick start

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export OPENROUTER_API_KEY=sk-or-...

./target/debug/xai-grok-pager models | head
./target/debug/xai-grok-pager -m openai/gpt-5 -p "ping"
./target/debug/xai-grok-pager -m anthropic/claude-sonnet-4-5 -p "ping"
./target/debug/xai-grok-pager -m openrouter/openai/gpt-4o -p "ping"
```

```toml
# ~/.grok/config.toml
[platforms.openai]
api_key = "sk-..."

[platforms.anthropic]
api_key = "sk-ant-..."

[platforms.openrouter]
api_key = "sk-or-..."

[models]
default = "anthropic/claude-sonnet-4-5"
```

**Credential precedence:** env (`GROK_*` then common aliases) >
`[platforms.*].api_key` > per-model `[model.*]`.

---

## Discovery: locked models and `/providers`

You do not need a key configured to see what a platform offers. The `/model`
picker lists **every** catalog model: usable ones first, then credential-less
platform models dimmed with a 🔒 and a one-line setup hint (exact env var
names + the `[platforms.<id>]` config table). Picking a locked model prints
its setup instructions instead of switching.

`/providers` shows one row per platform — configured ✓ vs locked 🔒, model
count, and its unlock method (`/login kimi` for the OAuth subscription).

As soon as the key resolves (env or config reload), the platform's models
become selectable — no restart, no other toggle. Selecting a locked model is
also rejected agent-side, and the credential seam never falls through to your
xAI session token for a third-party base URL.

---

## Refreshing the catalog from Pi

On a checkout of [earendil-works/pi](https://github.com/earendil-works/pi):

```bash
cd packages/ai && npm run generate-models -- --pretty
# then re-run the import script used in hyper-grok-build development
# (copies providers/data/*.json → platform_catalog.json)
```

Shipped files:

- `xai-grok-models/platform_catalog.json` — models
- `xai-grok-models/platform_registry.json` — platform metadata

We do **not** auto-fetch OpenAI/Anthropic/OpenRouter org `/models` at runtime
(too large). Kimi Code + Moonshot still live-sync when credentials exist.

---

## Notes

- Anthropic: Messages + `x-api-key` + `anthropic-version: 2023-06-01`.
- Kimi For Coding (official Pi): **Messages** at `https://api.kimi.com/coding/v1`,
  models `k3` / `k2p7` / `kimi-for-coding-highspeed`, `User-Agent: KimiCLI/1.5`,
  `anthropic-version`, adaptive thinking (`thinking.type=adaptive` +
  `output_config.effort`).
- OpenAI flagships: **Responses** per Pi mapping.
- No LiteLLM sidecar required.
