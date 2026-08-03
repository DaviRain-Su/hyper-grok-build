# OpenAI Codex (ChatGPT) Subscription

Grok can use an **OpenAI Codex** subscription (ChatGPT Plus/Pro) via the
official first-party OAuth flow — the same protocol as the official Pi
`openai-codex` provider and the Codex CLI. No external `codex` CLI
installation is required: Grok talks to the ChatGPT Codex backend directly.

| | |
|--|--|
| Platform id | `openai-codex` |
| Inference | `https://chatgpt.com/backend-api/codex` (Responses API, SSE) |
| OAuth host | `https://auth.openai.com` |
| Catalog models | `openai-codex/gpt-5.6-sol`, `openai-codex/gpt-5.6-terra`, `openai-codex/gpt-5.6-luna`, `openai-codex/gpt-5.5`, `openai-codex/gpt-5.4`, `openai-codex/gpt-5.4-mini`, `openai-codex/gpt-5.3-codex-spark` |
| Protocol | OpenAI Responses API with `store: false`, encrypted reasoning, `instructions` system prompt |

xAI login and other platform credentials remain independent. Codex
credentials live in `~/.grok/auth.json` under the scope `oauth/openai-codex`
and do **not** replace your xAI session.

---

## Sign in

### CLI

```bash
grok login --openai
```

Browser login (PKCE + loopback callback on `127.0.0.1:1455`) starts by
default; you can also paste the authorization code / redirect URL manually.
For headless or remote environments:

```bash
grok login --openai --device-code
```

prints a code and a verification URL (`https://auth.openai.com/codex/device`)
to approve on another machine.

### TUI

```
/login openai
```

(also accepts `/login codex` / `/login openai-codex` / `/login chatgpt`)

1. Grok builds the authorize URL and opens your browser.
2. Approve with your ChatGPT account; the browser redirects back to Grok.
3. If the redirect cannot reach the CLI (remote VM), paste the redirect URL
   into the prompt instead.
4. Tokens are stored under `oauth/openai-codex`; access tokens are refreshed
   automatically on use.

### Sign out

```bash
grok logout --openai
```

---

## Experimental Live voice

`/live` starts a long-running, full-duplex voice conversation with the Codex
Live model. It is separate from `/voice`, which remains push-to-dictate speech
to text.

```text
/live
```

Live mode:

- records the microphone and plays the assistant's spoken response at the same
  time;
- shows realtime user and assistant transcripts;
- supports barge-in, so speaking over the assistant interrupts naturally;
- delegates coding or tool work to the **currently bound Hyper agent session**,
  then feeds progress and the final result back into the voice conversation;
- leaves the current prompt draft and cursor intact.

Press **Space** to mute or unmute the microphone. Press **Esc** or **Ctrl+C** to
stop Live mode. You can also click the mute/unmute and stop controls in the Live
footer. Permission prompts, questions, and other modals keep keyboard
priority while they are open. Starting `/live` stops `/voice` dictation, and
starting `/voice` stops `/live`, so only one mode owns the microphone.

> **Experimental:** this feature uses the undocumented internal Codex Live
> protocol and the `gpt-live-1-codex` model. OpenAI can change or disable it
> without notice. It is not the public OpenAI Realtime API.

Live always requires the ChatGPT/Codex OAuth login described above, but the
coding agent may use any configured provider or model. If credentials are
missing, run `grok login --openai` and retry. If Live reports `You have reached
your usage limit.`, that message comes from the OpenAI account's Live/Codex
quota or entitlement; xAI/SuperGrok usage does not apply to `/live`.

Availability defaults to on in normal Hyper builds. Administrators can disable
it with either of these layered settings:

```toml
# ~/.grok/config.toml or managed requirements.toml
[features]
codex_live = false
```

```bash
GROK_CODEX_LIVE=0 hyper
```

For development and test endpoints, `GROK_OPENAI_CODEX_BASE_URL` overrides the
Codex platform base (the production value is
`https://chatgpt.com/backend-api/codex`); Live appends `/realtime/calls` to it.
`GROK_CODEX_LIVE_SIDEBAND_BASE` overrides the sideband base. Normal users
should not set either variable.

### Audio requirements

- **Linux:** Hyper tries PipeWire first, then PulseAudio and ALSA tools.
- **macOS:** microphone and speaker access belong to the terminal application
  running Hyper; grant access in System Settings if prompted.
- **Windows:** Hyper uses WASAPI through the native audio backend.

If Live cannot open an audio device, end the session with Esc, check the system
input/output device and permissions, then retry. Hardware-backed Live sessions
cannot be exercised in headless environments.

---

## Use a Codex model

```bash
grok models | grep openai-codex
grok -m openai-codex/gpt-5.6-sol -p "ping"
```

TUI: `/model openai-codex/gpt-5.6-sol`

### Reasoning effort (Codex catalog)

Menus follow the official OpenAI Codex CLI catalog
(`codex-rs/models-manager/models.json` `supported_reasoning_levels`) —
**each model has its own ladder**, not a single global low/medium/max.

| Model | Levels | Default |
|-------|--------|---------|
| `gpt-5.6-sol` | low · medium · high · xhigh · **max** · **ultra** | low |
| `gpt-5.6-terra` | low · medium · high · xhigh · **max** · **ultra** | medium |
| `gpt-5.6-luna` | low · medium · high · xhigh · **max** | medium |
| `gpt-5.5` / `gpt-5.4` / mini | low · medium · high · xhigh | medium |

- **max** — maximum single-agent reasoning depth (`reasoning.effort: "max"`)
- **ultra** — UI tier matching the Codex CLI menu; the request body is
  identical to **max**. Official Codex maps **Ultra → Max on the wire**
  (`reasoning_effort_for_request` in `codex-rs/core/src/client.rs`); the
  ChatGPT backend does not accept `effort: "ultra"`. Automatic task
  delegation is a future client-side policy and is not yet implemented;
  selecting Ultra today behaves like Max.

Override with `/effort max`, `/effort ultra`, or `grok --effort ultra`
(ultra is accepted; request body uses `max`).

The `grok codex` convenience subcommand pins a Codex model and drops into
the standard flows:

```bash
grok codex                    # interactive TUI on the default Codex model
grok codex -p "fix the bug"   # one-shot headless prompt
grok codex --status           # credential status + model list
```

Running `grok codex` signed out starts the browser login automatically
(interactive terminals); selecting an `openai-codex/*` model without a
credential shows the `/login openai` hint.

---

## Migration notes (app-server removal)

Earlier builds spawned the external `codex app-server` binary and required a
separate `codex login`. That dependency is gone:

* Legacy `codex:<model>` ids saved in sessions are rewritten to
  `openai-codex/<model>` on selection.
* Codex app-server thread ids cannot be resumed; use `grok sessions` for
  native sessions.
* `--codex-binary` is ignored; `--resume <thread>` is rejected with guidance.

Environment overrides (dev/test only): `GROK_OPENAI_CODEX_BASE_URL`,
`GROK_OPENAI_CODEX_OAUTH_HOST`.
