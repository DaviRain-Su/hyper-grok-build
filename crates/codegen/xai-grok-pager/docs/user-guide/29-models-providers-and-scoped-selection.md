# Models, Providers, and Scoped Selection

How Hyper lists models, how that compares to [Pi](https://github.com/earendil-works/pi), **scoped models**, and which provider protocols we support.

> **中文:** [模型、平台与 Scoped 选择](../user-guide-zh-CN/29-models-providers-and-scoped-selection.md)

---

## Quick reference

| Task | Command |
|------|---------|
| List models | `hyper models` or `/model` |
| Switch model | `/model <id>` or `/m <id>` · picker: **Ctrl+M** (scrollback) |
| Cycle scoped shortlist | **Alt+]** next · **Alt+[** prev |
| Manage shortlist | `/scoped-models` · `/scoped-models add\|remove\|set\|clear` |
| Set default | `~/.grok/config.toml` → `[models] default = "…"` |
| Soft cycle list | `[models] enabled_models = ["grok-*", …]` |
| Hard allowlist | `[models] allowed_models = […]` (blocks non-matches) |
| Custom / local | `[model.<name>]` sections — see [Custom Models](11-custom-models.md) |
| Platform API key | `/providers <platform> <key>` |
| OpenCode Go | `/providers opencode-go <key>` then `/model opencode-go/…` |
| ChatGPT Codex models | `/login openai` then `/model openai-codex/…` — [Codex](28-openai-codex.md) |

Model ids are usually `platform/model` (for example `openrouter/anthropic/claude-sonnet-4.5`, `openai/gpt-5`).

---

## Protocol stance: OpenAI-compatible first

Hyper’s wire layer speaks a small set of HTTP APIs:

| Backend | Typical providers |
|---------|-------------------|
| **Chat Completions** (`/v1/chat/completions`) | openrouter, groq, together, fireworks, deepseek, ollama, moonshot, OpenCode Go chat rows, … |
| **Messages** (Anthropic-style `/v1/messages`) | anthropic, kimi-code (default), OpenCode Go MiniMax/Qwen rows, some Fireworks rows |
| **Responses** | openai (BYOK), openai-codex (ChatGPT OAuth) |

**We do not plan first-class native clients for Google AI Studio / Vertex, Amazon Bedrock, Azure OpenAI admin APIs, or GitHub Copilot OAuth** in the community build: those need different request shapes, subscription product wiring, and/or OAuth stacks Hyper does not own today.

**Practical alternatives (preferred):**

1. **OpenRouter** (or another OpenAI-compatible gateway) — one key, many remote models including Gemini-class and Bedrock-proxied models when the gateway offers them.  
2. **`[model.*]` custom OpenAI-compatible base URL** — any proxy that exposes chat completions.  
3. **Already-wired first-party OpenAI / Anthropic / Messages / Responses** platforms in the registry.

If a vendor only speaks a proprietary API with no OpenAI- or Messages-compatible endpoint, use a gateway or do not expect first-class support.

### Built-in catalog & registry

Hyper embeds a **platform catalog** (`platform_catalog.json`, ~460 third-party rows, mostly chat_completions) largely synced from Pi’s open model data, plus SpaceXAI defaults in `default_models.json`.

**Registry platforms** (API key or OAuth as noted): openai, anthropic, kimi-code (OAuth), openai-codex (OAuth), opencode-go (subscription API key), moonshot-*, deepseek, groq, mistral, xai-direct, together, fireworks, openrouter, nvidia, ollama, cerebras, minimax-*, zai-*.

A model only shows as **usable** when the platform is registered and credentials exist (env, `/providers`, or `/login`).

### Why the picker can feel “thin”

1. **Default xAI list is small** until you add BYOK / OpenRouter / login.  
2. **Auth filters the list** — no key for `anthropic` → those models stay locked.  
3. **No Google/Bedrock/Azure first-class rows** by design (use OpenRouter / custom base).  
4. **openai-codex** is OAuth-backed; not the same as BYOK `openai/*`.

---

## Scoped models (implemented)

Pi’s **scoped models** are a **shortlist for fast cycling**, not a capability scope.

| Concept | Hyper |
|---------|--------|
| Config | `[models].enabled_models` (globs; alias `enabledModels` accepted) |
| Slash | `/scoped-models` · aliases `/scoped`, `/enabled-models` |
| Cycle keys | **Alt+]** next · **Alt+[** prev (not Ctrl+P — that stays the command palette) |
| Full picker | `/model` · **Ctrl+M** |
| Soft vs hard | `enabled_models` = cycle shortlist only; **`allowed_models`** still hard-gates selectable models |

When `enabled_models` is empty, cycle keys walk **all currently usable** (unlocked) models.

```toml
# ~/.grok/config.toml
[models]
default = "openrouter/anthropic/claude-sonnet-4.5"
enabled_models = ["grok-*", "openai-codex/*", "openrouter/anthropic/*"]
```

```text
/scoped-models                  # status + matching usable models
/scoped-models add grok-*
/scoped-models remove openai/gpt-5
/scoped-models set grok-4.5 openrouter/anthropic/claude-sonnet-4.5
/scoped-models clear            # cycle all usable again
```

**Not the same as:**

| Setting | Effect |
|---------|--------|
| `hidden_models` | Hide from picker (^X in All view); still usable via `-m` |
| `disabled_models` | Remove from catalog |
| `allowed_models` | Hard allowlist for chat selection |

---

## Other Pi ideas still useful

| Idea | Pi | Hyper |
|------|-----|--------|
| User models overlay | `models.json` hot reload | `[model.*]` in config.toml |
| `$ENV` / `!shell` secrets | Yes | `env_key`, env vars |
| Catalog refresh | `pi update --models` | Compile-time embed + live lists (Kimi/Moonshot/Ollama) |
| Extra native providers | Google, Bedrock, … | Prefer OpenAI-compat gateways (see stance above) |

---

## Practical setup for a full shelf

1. **One aggregator key**  
   ```text
   /providers openrouter $OPENROUTER_API_KEY
   /model openrouter/...
   ```
2. **First-party keys**  
   ```text
   export ANTHROPIC_API_KEY=...
   export OPENAI_API_KEY=...
   ```
3. **Subscriptions**  
   ```text
   /login openai    # Codex OAuth
   /login kimi
   /providers opencode-go <key>  # OpenCode Go uses a subscription API key
   ```
4. **Scoped shortlist**  
   ```text
   /scoped-models set grok-* openrouter/anthropic/*
   ```
   Then **Alt+]** / **Alt+[** day-to-day.
5. **Local OpenAI-compat**  
   ```toml
   [model.ollama-local]
   model = "llama3.1:8b"
   base_url = "http://127.0.0.1:11434/v1"
   api_key = "ollama"
   ```

---

## Related docs

- [Authentication](02-authentication.md) — multi-provider login, free-usage escape  
- [Custom Models](11-custom-models.md) — `[model.*]` BYOK  
- [OpenAI Codex](28-openai-codex.md) · [Kimi Code](26-kimi-code.md) · [Moonshot](25-moonshot-providers.md) · [OpenAI & Anthropic](27-openai-anthropic.md)  
- Pi reference: [models.md](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md), [settings `enabledModels`](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/settings.md)  
