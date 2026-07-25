# Models, Providers, and Scoped Selection

How Hyper lists models, how that compares to [Pi](https://github.com/earendil-works/pi), and what “scoped models” means.

> **中文:** [模型、平台与 Scoped 选择](../user-guide-zh-CN/29-models-providers-and-scoped-selection.md)

---

## Quick reference

| Task | Command |
|------|---------|
| List models | `hyper models` or `/model` |
| Switch model | `/model <id>` or `/m <id>` · picker: **Ctrl+M** (scrollback) |
| Set default | `~/.grok/config.toml` → `[models] default = "…"` |
| Custom / local | `[model.<name>]` sections — see [Custom Models](11-custom-models.md) |
| Platform API key | `/providers <platform> <key>` |
| ChatGPT Codex models | `/login openai` then `/model openai-codex/…` — [Codex](28-openai-codex.md) |

Model ids are usually `platform/model` (for example `openrouter/anthropic/claude-sonnet-4.5`, `openai/gpt-5`).

---

## What Hyper ships today

### Built-in catalog

Hyper embeds a **platform catalog** (`platform_catalog.json`) largely synced from Pi’s open model data (`earendil-works/pi` provider tables). On a current tree that is on the order of **~460** third-party rows across platforms such as:

- openrouter, openai, anthropic  
- ollama, together, fireworks, nvidia, groq, cerebras  
- moonshot-ai / moonshot-cn, kimi-code  
- minimax / minimax-cn, zai / zai-coding, deepseek, xai-direct  

Plus SpaceXAI defaults in `default_models.json` (session default is typically `grok-4.5`).

### Platforms wired in `platform_registry.json`

Registry entries define base URLs, env keys, and OAuth flags. **A model only shows as usable when the platform is registered and credentials exist** (session, env key, or `/providers`). If a platform is missing from the registry, catalog rows for it never become first-class targets.

Current registry platforms (illustrative): openai, anthropic, kimi-code, moonshot-*, deepseek, groq, mistral, xai-direct, together, fireworks, openrouter, nvidia, ollama, cerebras, minimax-*, zai-*.

### Why the picker can feel “thin”

1. **Default xAI list is small** — until you add BYOK / OpenRouter / login, `/model` is dominated by Grok defaults.  
2. **Auth filters the list** — no key for `anthropic` → those models stay unavailable or hidden.  
3. **Registry gaps vs Pi** — Pi’s `packages/ai` still has providers Hyper has not fully wired yet (see below).  
4. **openai-codex** models are OAuth-backed and documented separately; they are not the same as BYOK `openai/*` API keys.

---

## What Pi does that Hyper can learn from

[Pi](https://github.com/earendil-works/pi) is a useful reference for multi-provider coding CLIs.

### 1. Scoped models (`/scoped-models`, `enabledModels`, Ctrl+P)

Pi lets you **pin a shortlist** of models for fast cycling:

| Pi concept | Behavior |
|------------|----------|
| `/scoped-models` | UI to enable/disable models for the cycle list; reorder; save to settings |
| `enabledModels` in settings | Glob patterns, e.g. `["claude-*", "gpt-4o", "gemini-2*"]` |
| CLI `--models` | Same patterns for one process |
| **Ctrl+P / Shift+Ctrl+P** | Cycle forward/backward **only among scoped models** |
| **Ctrl+L** | Full model picker (all available) |

That is **not** the same as Hyper’s **Ctrl+P** (command palette). Hyper today:

| Hyper | Behavior |
|-------|----------|
| `/model` · **Ctrl+M** | Full picker / switch |
| Ctrl+P | Command palette (not model cycle) |
| No `enabledModels` / `/scoped-models` yet | — |

**Why it matters:** a 400+ model catalog is hard to navigate; a 5–15 model “scope” matches day-to-day work and is what many Pi users mean by “SCOPE-MODEL”.

**Planned direction for Hyper** (docs only until implemented):

```toml
# ~/.grok/config.toml (proposed)
[models]
default = "openrouter/anthropic/claude-sonnet-4.5"
# enabled = ["grok-*", "openai-codex/*", "openrouter/anthropic/*"]
```

```text
# proposed
/scoped-models
# cycle shortlist with a dedicated binding (not Ctrl+P if that stays the palette)
```

### 2. User `models.json` overlay

Pi: `~/.pi/agent/models.json` — add providers (Ollama, vLLM, Google AI Studio, proxies) with `baseUrl`, `api`, `apiKey` (`$ENV` / `!command`), per-model `contextWindow`, `cost`, `reasoning`, `compat`, reload on `/model` without restart.

Hyper already has rich **`[model.<name>]` in config.toml** (see [Custom Models](11-custom-models.md)). Worth learning from Pi:

| Idea | Pi | Hyper today |
|------|-----|-------------|
| JSON overlay + hot reload | Yes | Edit `config.toml` / restart or re-open picker depending on path |
| `$ENV` / `!shell` for secrets | Yes | `env_key`, env vars; limited shell-out |
| `compat` flags (developer role, reasoning_effort) | Yes | Partial via `api_backend` / headers |
| Per-model cost for usage UI | Yes | Partial / platform-dependent |
| `modelOverrides` on built-ins | Yes | Custom sections + platform keys |

### 3. Provider breadth

Pi’s generated catalog includes extra families Hyper registry still lacks as first-class platforms, for example:

- Google AI Studio / Vertex  
- Amazon Bedrock  
- Azure OpenAI Responses  
- GitHub Copilot  
- Hugging Face  
- Vercel / Cloudflare AI gateways  
- More regional token-plan providers (Qwen, Xiaomi, …)

Hyper already covers a large OpenRouter slice (many remote models via one key). Closing the gap means **registering platforms + refreshing `platform_catalog.json`**, not only documenting names.

### 4. Dynamic refresh

Pi: `pi update --models` and automatic provider catalog refresh.  
Hyper: catalog is compile-time embedded + live lists for some OAuth platforms (e.g. Kimi). A community refresh script against Pi / models.dev data would keep lists fresher between releases.

---

## Practical setup for a “full” Hyper model shelf

1. **One aggregator key (fastest)**  
   ```text
   /providers openrouter $OPENROUTER_API_KEY
   /model openrouter/...
   ```
2. **First-party keys**  
   ```text
   export ANTHROPIC_API_KEY=...
   export OPENAI_API_KEY=...
   /model anthropic/...
   /model openai/...
   ```
3. **Subscriptions**  
   ```text
   /login openai    # Codex
   /login kimi
   /model openai-codex/gpt-5.6-sol
   ```
4. **Local**  
   ```toml
   [model.ollama-local]
   model = "llama3.1:8b"
   base_url = "http://127.0.0.1:11434/v1"
   api_key = "ollama"
   ```
5. **Default**  
   ```toml
   [models]
   default = "openrouter/anthropic/claude-sonnet-4.5"
   ```

---

## Related docs

- [Authentication](02-authentication.md) — multi-provider login, free-usage escape  
- [Custom Models](11-custom-models.md) — `[model.*]` BYOK  
- [OpenAI Codex](28-openai-codex.md) · [Kimi Code](26-kimi-code.md) · [Moonshot](25-moonshot-providers.md) · [OpenAI & Anthropic](27-openai-anthropic.md)  
- Pi reference: [models.md](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md), [settings `enabledModels`](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/settings.md)  
