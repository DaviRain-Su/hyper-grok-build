# Kimi Code Subscription

Grok can use a **Kimi Code** subscription via the official device OAuth flow
(the same protocol as kimi-cli / the Kigi community build). This is Phase 2 of
built-in multi-provider support.

| | |
|--|--|
| Platform id | `kimi-code` |
| Inference | `https://api.kimi.com/coding/v1` |
| OAuth host | `https://auth.kimi.com` |
| Catalog models | `kimi-code/k3`, `kimi-code/k2p7`, `kimi-code/kimi-for-coding-highspeed` (K2.7 Hyper Speed) |
| Offline fallback | Same ids; synced live after login via `GET …/coding/v1/models` |
| Protocol | **OpenAI Chat Completions**; base `https://api.kimi.com/coding/v1` |

xAI login and Moonshot API keys remain independent. Kimi credentials live in
`~/.grok/auth.json` under the scope `oauth/kimi-code` and do **not** replace
your xAI session.

---

## Sign in

### CLI

```bash
grok login --kimi
```

### TUI

```
/login kimi
```

(also accepts `/login kimi-code`)

1. Grok requests a device code from Kimi.
2. Your browser opens the Kimi Code authorization page (or print the URL).
3. Confirm the user code, then return to the terminal.
4. Tokens are stored under `oauth/kimi-code`.

Device identity headers (`X-Msh-Device-*`) are sent on OAuth and inference
calls, matching kimi-cli. Expired access tokens are refreshed automatically
when the model catalog is rebuilt (and when a Tokio runtime is available).

---

## Use a Kimi Code model

After login (or on the next startup with a valid token), Grok calls
`GET https://api.kimi.com/coding/v1/models` and merges the listing into the
model catalog. That is how **K3** and other subscription models show up —
not only the offline fallbacks.

```bash
grokk models | grep kimi-code
# typically:
#   kimi-code/k3
#   kimi-code/k2p7                      # Kimi K2.7 Code
#   kimi-code/kimi-for-coding-highspeed # Kimi K2.7 Hyper Speed

grok -m kimi-code/k3 -p "ping"
grok -m kimi-code/k2p7 -p "ping"
```

In the TUI:

```
/model kimi-code/k3
```

Or set a default:

```toml
# ~/.grok/config.toml
[models]
default = "kimi-code/k3"
```

Until you complete `grok login --kimi`, subscription models stay hidden from
API-key-only pickers (`supported_in_api = false`); after login they appear
and credentials are stamped on every `kimi-code/*` entry.

K3 (and other models that advertise `think_efforts` on the wire) expose
selectable reasoning levels in the TUI (`low` / `high` / `max` → `Xhigh`).

---

## Sign out of Kimi only

```bash
grok logout --kimi
```

This clears the `oauth/kimi-code` scope only. Your xAI session (and
`XAI_API_KEY`) are left alone.

---

## Environment overrides (dev / test)

```bash
# Must include /v1 — Grok posts to {base}/messages → …/coding/v1/messages.
# Pi-style `…/coding` (no /v1) is auto-normalized to `…/coding/v1`.
export GROK_KIMI_CODE_BASE_URL="https://api.kimi.com/coding/v1"
export GROK_KIMI_CODE_OAUTH_HOST="https://auth.kimi.com"
```

---

## Moonshot open platform vs Kimi Code

| | Moonshot open API | Kimi Code subscription |
|--|-------------------|-------------------------|
| Auth | API key (`GROK_MOONSHOT_*`) | Device OAuth (`grok login --kimi`) |
| Hosts | `api.moonshot.cn` / `api.moonshot.ai` | `api.kimi.com/coding` |
| Docs | [25-moonshot-providers.md](25-moonshot-providers.md) | this page |

---

## Request parameters

### Subscription path (OpenAI Chat Completions)

Grok speaks the Kimi Code endpoint as OpenAI Chat Completions, using the
same reasoning/thinking mapping as the Moonshot open platform:

| Concern | Behavior in Grok |
|---------|------------------|
| Protocol | `POST {base}/chat/completions` with `User-Agent: KimiCLI/1.5` |
| Base URL | `https://api.kimi.com/coding/v1` (env: `GROK_KIMI_CODE_BASE_URL`) |
| Thinking | K3 / K2.7 Code / K2.7 Hyper Speed keep thinking enabled; reasoning effort is mapped to the model's thinking fields |
| Temperature | Omitted for fixed-sampling models (K2.7 Code / K2.7 Hyper Speed) |
| `max_tokens` / `max_completion_tokens` | Defaults to **32768** when unset |

| Model id | Notes |
|----------|-------|
| `k3` | 1M context; selectable reasoning effort |
| `k2p7` | Kimi K2.7 Code; 256k context |
| `kimi-for-coding-highspeed` | Kimi K2.7 Hyper Speed |

### Open-platform Moonshot (Chat Completions)

Separate from the subscription path — see
[25-moonshot-providers.md](25-moonshot-providers.md). In short: K3 uses
top-level `reasoning_effort`; K2.7 omits the K2 `thinking` object; K2.6 maps
effort → `thinking.type` (+ `keep: all` for tool loops).

Exact model ids on the **subscription** host come from live
`GET …/coding/v1/models` after login (not the open-platform id table alone).

## Notes

- Unofficial integration of a third-party subscription API; not affiliated
  with Moonshot AI or xAI.
- Hosted xAI-only tools (server-side web/x search) are not available on Kimi.
- Token refresh runs automatically when the catalog stamps credentials (and a
  Tokio runtime is available) if the access token is past the early-expiry
  window and a refresh token is present.

---

## Troubleshooting

| Symptom | Check |
|---------|--------|
| Model not listed | Run `grok login --kimi` / `/login kimi`; restart TUI |
| “Authentication required… Run /login” mid-session | **Kimi access tokens last ~15 minutes.** Grok refreshes them automatically on each request when a refresh token is stored. If refresh fails (revoked session, network), run `/login kimi` again — plain `/login` only re-auths xAI. |
| 401 on inference | Re-login with `/login kimi`; check `~/.grok/auth.json` still has `oauth/kimi-code` with `refresh_token`; clock skew |
| Only xAI fails, Kimi works | `grok login` (xAI); independent credentials |
| Browser does not open | Copy the printed URL; complete login manually |
| Device authorization failed | Network access to `auth.kimi.com`; corporate proxy |
