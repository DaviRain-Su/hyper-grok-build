# Kimi Code Subscription

Grok can use a **Kimi Code** subscription via the official device OAuth flow
(the same protocol as kimi-cli / the Kigi community build). This is Phase 2 of
built-in multi-provider support.

| | |
|--|--|
| Platform id | `kimi-code` |
| Inference | `https://api.kimi.com/coding/v1` |
| OAuth host | `https://auth.kimi.com` |
| Catalog model | `kimi-code/kimi-for-coding` |
| Protocol | OpenAI Chat Completions |

xAI login and Moonshot API keys remain independent. Kimi credentials live in
`~/.grok/auth.json` under the scope `oauth/kimi-code` and do **not** replace
your xAI session.

---

## Sign in

```bash
grok login --kimi
```

1. Grok requests a device code from Kimi.
2. Your browser opens the Kimi Code authorization page (or print the URL).
3. Confirm the user code, then return to the terminal.
4. Tokens are stored under `oauth/kimi-code`.

Device identity headers (`X-Msh-Device-*`) are sent on OAuth and inference
calls, matching kimi-cli.

---

## Use a Kimi Code model

```bash
grok models | grep kimi-code
grok -m kimi-code/kimi-for-coding -p "ping"
```

In the TUI:

```
/model kimi-code/kimi-for-coding
```

Or set a default:

```toml
# ~/.grok/config.toml
[models]
default = "kimi-code/kimi-for-coding"
```

Until you complete `grok login --kimi`, subscription models stay hidden from
API-key-only pickers (`supported_in_api = false`); after login they appear.

---

## Sign out of Kimi only

There is no dedicated CLI flag yet. Remove the scope manually or clear the
token with a small helper (coming soon). Removing `oauth/kimi-code` from
`~/.grok/auth.json` (while leaving other scopes) is safe. Do **not** delete
the whole file if you still need xAI login.

---

## Environment overrides (dev / test)

```bash
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

## Notes

- Unofficial integration of a third-party subscription API; not affiliated
  with Moonshot AI or xAI.
- Hosted xAI-only tools (server-side web/x search) are not available on Kimi.
- Token refresh runs when the catalog stamps credentials if the access token
  is past the early-expiry window and a refresh token is present.

---

## Troubleshooting

| Symptom | Check |
|---------|--------|
| Model not listed | Run `grok login --kimi`; restart TUI |
| 401 on inference | Re-login; clock skew; check `auth.json` still has `oauth/kimi-code` |
| Browser does not open | Copy the printed URL; complete login manually |
| Device authorization failed | Network access to `auth.kimi.com`; corporate proxy |
