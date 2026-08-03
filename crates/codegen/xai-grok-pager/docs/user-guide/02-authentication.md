# Authentication

Hyper (and Grok Build) supports **multiple independent auth methods**. You are not required to use only SpaceXAI / grok.com: you can mix subscriptions and API keys, and switch models without re-binding everything to one vendor.

| Method | How | Typical use |
|--------|-----|-------------|
| SpaceXAI / Grok OAuth | `hyper login` or `l` on welcome | Grok-hosted models |
| xAI API key | `export XAI_API_KEY=…` | CI / no browser |
| Kimi Code | `hyper login --kimi` / `/login kimi` | Kimi subscription |
| OpenAI Codex (ChatGPT) | `hyper login --openai` / `/login openai` | ChatGPT Plus/Pro coding |
| Anthropic Claude | `hyper login --claude` / `/login claude` | Claude Pro/Max coding |
| OpenCode Go | `/providers opencode-go <key>` / `OPENCODE_API_KEY` | Go subscription models |
| BYOK platforms | `/providers <platform> <key>` or env keys | OpenAI, Anthropic, OpenRouter, Ollama, … |
| Customer OIDC / SSO | config / env | Enterprise |

Credentials live under `~/.grok/auth.json` in **separate scopes** so logging out of xAI does not wipe Kimi, Codex, or Claude unless you ask.

---

## First launch (Hyper community builds)

**Hyper does not auto-open the Grok browser login.** You land on the welcome splash:

1. Press **`l`** when you want the default xAI interactive login (`grok.com` or configured enterprise OIDC).
2. Or enter a session after any credential is already configured, then use slash commands:

```text
/login kimi            # Kimi Code OAuth
/login openai          # ChatGPT Codex OAuth
/login claude          # Anthropic Claude Pro/Max OAuth
/providers opencode-go <key>  # OpenCode Go subscription
/providers openrouter <key>
/providers anthropic <key>
/model openrouter/...
```

If Grok **free usage** is exhausted, the upsell is not paywall-only: you can **switch model / use an API key** or **dismiss** and keep working with another provider. Consumer SuperGrok access gates do not lock the entire TUI on Hyper.

Official `grok` builds may still auto-start SpaceXAI login; Hyper (`community-build`) prefers multi-provider choice.

---

## Browser Login (SpaceXAI / Grok)

On first launch of an official Grok build, or when you press `l` / run login without flags, the CLI opens your browser to authenticate with SpaceXAI:

```bash
hyper
# or
hyper login
```

Credentials are stored in `~/.grok/auth.json` and reused across sessions. Access tokens refresh in the background. When a token can't be refreshed, you are prompted to sign in again. Credentials without a server-provided expiry fall back to a 30-day lifetime.

### Credential storage

Tokens in `~/.grok/auth.json` (and MCP OAuth tokens in `~/.grok/mcp_credentials.json`) are written with owner-only permissions (`0600` on Unix). Anyone with filesystem access to those paths can use the credentials, so:

- Prefer full-disk encryption (FileVault, BitLocker, LUKS, or equivalent).
- Do not copy `auth.json` or `mcp_credentials.json` into shared directories, tickets, or chat.
- On multi-user hosts, keep `$HOME` / `$GROK_HOME` private to your account.

### Re-authenticate

To switch accounts or resolve an authentication problem, run:

```bash
grok login
```

Running `grok login` starts the sign-in flow again, replacing your cached session. By default, it opens your browser and signs in through SpaceXAI OAuth at `auth.x.ai`. Pass a flag to select a different flow:

| Flag | Description |
|------|-------------|
| `--oauth` | Sign in through SpaceXAI OAuth at `auth.x.ai`. This is the default, so the flag is optional. |
| `--device-auth` (alias `--device-code`) | Sign in with the xAI device-code flow for headless or remote environments. |
| `--kimi` | Sign in with a Kimi Code subscription. |
| `--openai` | Sign in with a ChatGPT Plus/Pro subscription for OpenAI Codex. |
| `--claude` | Sign in with an Anthropic Claude Pro/Max subscription. |

To sign out of the **xAI** session, run `hyper logout` (or `grok logout` on
the official binary). That clears the primary xAI session scope only; Kimi,
Codex, and Claude sessions remain until explicitly cleared.

Third-party subscriptions and BYOK keys are separate:

| Command | Clears |
|---------|--------|
| `hyper logout` | xAI session (interactive OAuth / cached session); third-party subscriptions remain |
| `hyper logout --kimi` | Kimi Code OAuth only |
| `hyper logout --openai` | OpenAI Codex (ChatGPT) OAuth only |
| `hyper logout --claude` | Anthropic Claude OAuth only |
| `hyper logout --all` | xAI + Kimi + Codex + Claude OAuth (not BYOK platform keys) |
| `/logout provider <platform>` | A stored BYOK API key for that platform (same as `/providers clear`) |

`XAI_API_KEY` and other env-based keys are never removed by logout — unset the
environment variable yourself.

---

## API Key

For CI/CD, automation, or environments without browser access, use an API key from [console.x.ai](https://console.x.ai):

```bash
export XAI_API_KEY="xai-..."
grok
```

Grok uses the API key as a fallback when no session token is active. If you have already signed in interactively, the stored session token takes precedence. To fall back to the API key, run `hyper logout` (xAI session only) or delete the relevant scopes in `~/.grok/auth.json`.

---

## OpenCode Go subscription

OpenCode Go is a subscription product, but its documented client connection uses a **Console-issued API key**, not a portable provider OAuth flow:

1. Sign in and subscribe at <https://opencode.ai/go>.
2. Copy the API key from the OpenCode Console.
3. Save it in Hyper:

```text
/providers opencode-go <api_key>
/model opencode-go/kimi-k3
```

Or provide it through the environment:

```bash
export OPENCODE_API_KEY="..."
```

Hyper stores a pasted key under `platform/opencode-go` in `~/.grok/auth.json`; clear it with `/providers clear opencode-go`. The bundled catalog follows the current non-deprecated OpenCode Go roster and routes each model through its documented OpenAI Chat Completions or Anthropic Messages endpoint.

`/login opencode-go` intentionally shows these instructions instead of reusing OpenCode CLI's undocumented, CLI-specific device-client identity. This keeps the four interactive OAuth login families unchanged.

---

## OIDC (Customer SSO)

Authenticate developers through your own Identity Provider (IdP) -- such as Okta, Azure AD, or Auth0 -- instead of grok.com.

### 1. Register a public client in your IdP

- Grant type: Authorization Code with PKCE (Proof Key for Code Exchange)
- Redirect URI: `http://127.0.0.1/callback` -- a loopback address. Grok binds a random port at sign-in time, and most IdPs treat the loopback redirect as port-agnostic per [RFC 8252](https://tools.ietf.org/html/rfc8252).
- No client secret. PKCE replaces it.

### 2. Configure the CLI

Via config file:

```toml
# ~/.grok/config.toml
[grok_com_config.oidc]
issuer = "https://acme.okta.com"
client_id = "0oa1b2c3d4e5f6g7h8i9"
```

Or via environment variables:

```bash
export GROK_OIDC_ISSUER="https://acme.okta.com"
export GROK_OIDC_CLIENT_ID="0oa1b2c3d4e5f6g7h8i9"
```

You can also override the API endpoint to point at your own proxy:

```bash
export GROK_CLI_CHAT_PROXY_BASE_URL="https://grok-proxy.acme.com/v1"
```

### 3. Run `grok`

The CLI discovers endpoints via `{issuer}/.well-known/openid-configuration`, opens the IdP login page, and stores tokens in `~/.grok/auth.json`. Tokens auto-refresh silently via the stored `refresh_token`.

### Optional fields

| Field | Default | Notes |
|-------|---------|-------|
| `scopes` | `["openid", "profile", "email", "offline_access", "api:access"]` | `offline_access` enables silent token refresh |
| `audience` | None | Required by some IdPs (e.g., Auth0) |

---

## External Auth Provider

When browser-based login isn't possible -- for example, on sandboxed VMs, CI runners, or air-gapped networks -- delegate authentication to an external binary or script.

### How It Works

```
+--------------+     sh -c     +------------------------+
|     Grok     |-------------->|  your auth binary      |
|              |               |                        |
|  reads       |<-- stdout ----|  prints token          |
|  auth.json   |               |                        |
|              |   (stderr)    |  prints status/URLs    |--> surfaced to user
+--------------+               +------------------------+
```

1. Grok runs your command via `sh -c "<command>"`
2. Your binary runs whatever auth flow it needs (SSO, device code, certificate exchange)
3. **stderr** carries human-readable output, such as login URLs and status messages. Grok reads stderr and surfaces it to the user; in the TUI, it turns the first `https://` URL into a clickable sign-in link.
4. **stdout** is captured by Grok and saved as the access token
5. Exit 0 = success; exit non-zero = Grok falls back to interactive login

### The stdout / stderr Contract

| Stream | What to print | Who sees it |
|--------|---------------|-------------|
| **stdout** | The token -- nothing else | Grok (parsed and stored in auth.json) |
| **stderr** | Login URLs, status messages, errors | The user (Grok reads stderr and shows the sign-in URL as a clickable link in the TUI) |

**Do not print anything to stdout except the token.** No progress messages, no debug output. Grok reads stdout, trims surrounding whitespace, and parses the result as a token.

### stdout Token Format

**Bare string** -- just the raw token:

```
eyJhbGciOiJSUzI1NiIs...
```

**JSON** -- with optional refresh token, expiry, and issuer:

```json
{"access_token": "eyJhbGciOi...", "refresh_token": "ref-tok", "expires_in": 3600, "issuer": "https://idp.example.com"}
```

Use JSON if your tokens expire and you want Grok to automatically re-run the binary before expiry.

JSON fields:

| Field | Required | Meaning |
|-------|----------|---------|
| `access_token` | yes | Bearer token Grok sends to the xAI API |
| `refresh_token` | no | Stored for reference. Grok refreshes by re-running your binary, not with an OAuth refresh grant |
| `expires_in` | no | Token lifetime in seconds; enables proactive refresh before expiry |
| `issuer` | no | Identifies the token's issuer |

### Configuration

Via config file:

```toml
# ~/.grok/config.toml
[auth]
auth_provider_command = "/usr/local/bin/my-auth-provider"
auth_provider_label = "Acme Corp"   # optional -- customizes the TUI login button
auth_token_ttl = 3600               # optional -- token lifetime in seconds
```

Or via environment variables:

```bash
export GROK_AUTH_PROVIDER_COMMAND="/usr/local/bin/my-auth-provider"
export GROK_AUTH_PROVIDER_LABEL="Acme Corp"
export GROK_AUTH_TOKEN_TTL=3600
```

### Token Refresh

When Grok needs to refresh an expired token, it re-runs your binary with `GROK_AUTH_EXPIRED=1` set in the environment. Each run fully replaces the stored credential, so emit the same JSON fields (such as `issuer`) on every invocation, including refreshes. Your binary can use this to take a faster silent-refresh path:

```bash
#!/bin/sh
if [ "$GROK_AUTH_EXPIRED" = "1" ]; then
    echo "Refreshing token..." >&2
    TOKEN=$(my-company-auth --refresh --silent)
else
    echo "Authenticating via Acme Corp SSO..." >&2
    TOKEN=$(my-company-auth --login --interactive)
fi

if [ -z "$TOKEN" ]; then
    echo "Authentication failed" >&2
    exit 1
fi

echo "{\"access_token\": \"$TOKEN\", \"expires_in\": 3600}"
```

The refresh run is headless: stdin is closed and the binary is given a few
seconds, so it cannot prompt you. If it can't produce a token there — because
your SSO session lapsed and it needs you to sign in again — Grok stops treating
the stored credential as usable and starts the interactive flow instead, the
same one you get on a machine that has never signed in. That run has a long
timeout and shows your binary's stderr, so a device-code URL or a browser prompt
reaches you. Mid-session, the turn fails with a re-auth prompt and `/login`
re-runs the binary interactively.

### Environment Variables

| Variable | Description |
|----------|-------------|
| `GROK_AUTH_PROVIDER_COMMAND` | Path to your auth binary |
| `GROK_AUTH_PROVIDER_LABEL` | Display name on the TUI login screen (e.g., "Acme Corp") |
| `GROK_AUTH_TOKEN_TTL` | Token lifetime in seconds (for bare-string tokens without `expires_in`) |
| `GROK_AUTH_EXPIRED` | Set to `1` by Grok when re-running the binary for token refresh |
| `GROK_AUTH_EARLY_INVALIDATION_SECS` | Seconds before expiry to proactively refresh (default: 300) |

---

## Device Code Flow

For headless environments (SSH sessions, Docker containers, remote VMs) where no browser is available locally:

```bash
grok login --device-auth    # or: grok login --device-code
```

This prints a URL and code to the terminal. Open the URL on any device, enter the code, and complete authentication. Grok polls until the login is confirmed.

You can also implement the device-code flow through an [External Auth Provider](#external-auth-provider) for full control.

---

## Automatic Credential Refresh

Grok automatically refreshes expired credentials:

- **Before expiry:** If your auth provider returned `expires_in` (JSON output) or you set `auth_token_ttl`, Grok re-runs the auth binary ~5 minutes before expiry.
- **On auth error:** If the server returns 401 Unauthorized, Grok refreshes the credentials and retries the request.
- **OIDC:** If a `refresh_token` is available, Grok silently refreshes via your IdP without re-opening the browser.

Tune the refresh buffer:

```bash
# Refresh 5 minutes before expiry (default)
export GROK_AUTH_EARLY_INVALIDATION_SECS=300

# Disable the proactive buffer: refresh at expiry or on a 401 (set to 0)
export GROK_AUTH_EARLY_INVALIDATION_SECS=0
```

---

## Hot Reload

Grok picks up changes to `~/.grok/auth.json` automatically. If you update credentials externally (for example, with a script that writes new tokens), Grok uses the new credentials on the next API call without a restart.

---

## Auth Precedence

Grok resolves credentials for each request in this order, highest to lowest:

1. **Per-model `api_key` or `env_key`** -- set under `[model.<name>]` in `config.toml`. Wins whenever present.
2. **Active session token** -- obtained through browser, OIDC/OAuth2, or external-provider login and stored in `~/.grok/auth.json`.
3. **`XAI_API_KEY`** -- fallback when no session token is active.

When more than one login flow is configured, Grok populates the session token from the first available source, highest to lowest:

1. **External auth provider** (`auth_provider_command`)
2. **Enterprise OIDC** -- when OIDC is configured, through `[grok_com_config.oidc]` in `config.toml` or the `GROK_OIDC_ISSUER` and `GROK_OIDC_CLIENT_ID` environment variables
3. **SpaceXAI OAuth2 browser login** -- the default

During a session, the active method handles all mid-session refreshes.

---

## Related settings

Coding-data sharing — **Coding data, retention, and training** in Settings,
which `/privacy` opens — does not change these config knobs:

| Setting | How to set it |
|---------|---------------|
| `[features] telemetry` | `config.toml` or `GROK_TELEMETRY_ENABLED` |
| `[telemetry] trace_upload` | `config.toml` or `GROK_TELEMETRY_TRACE_UPLOAD` |
| External OpenTelemetry | `GROK_EXTERNAL_OTEL` / `[telemetry] otel_*`. See [Monitoring Usage](24-monitoring-usage.md). |

On team accounts, only a team admin can change coding-data sharing.
Team admins can also enable or disable Zero Data Retention (ZDR) for their team.
See [How to enable ZDR](https://docs.x.ai/developers/faq/security#how-to-enable-zdr).
When ZDR is on, coding-data sharing cannot be changed at all — the settings
row shows `ZDR` in place of the value.

See [Monitoring Usage](24-monitoring-usage.md#related-settings) and [Configuration](05-configuration.md#telemetry).

---

## Troubleshooting

### Debug logging

Set `RUST_LOG` to control the verbosity of the file log and headless stderr output. (The TUI's on-screen tracing pane uses a fixed filter and ignores `RUST_LOG`.) In the TUI, file logging defaults to `DEBUG`; in headless mode (`-p`), `RUST_LOG` defaults to `off` so only the answer is printed — set `RUST_LOG=error` (or broader) to see logs on stderr.

In the TUI, set `GROK_LOG_FILE` to an absolute path to write logs to that file:

```bash
GROK_LOG_FILE=/tmp/grok.log RUST_LOG=debug grok
tail -f /tmp/grok.log
```

`GROK_LOG_FILE` is treated as a literal file path. A relative value such as `1` writes a file named `1` in the current directory.

In headless mode, logs go to stderr. Redirect them to a file:

```bash
RUST_LOG=debug grok -p "hello" 2> /tmp/grok.log
```

### Common log messages

| Log message | What it means |
|-------------|---------------|
| `auth: running external auth provider` | Grok is running your binary |
| `auth: external auth provider returned fresh token` | Grok parsed and stored the token |
| `auth: external auth provider failed` | Binary exited non-zero or stdout was empty |
| `auth: external auth provider timed out (likely needs interactive auth), killing` | Binary did not exit before the timeout and was killed |
| `auth: failed to start external auth provider` | Command could not be spawned (binary not found) |

### Common fixes

- **"Authentication failed"** -- Run `grok logout` to clear cached credentials, then `grok login` to sign in again.
- **Token expires too quickly** -- Set `auth_token_ttl` or return `expires_in` in your auth provider's JSON output.
- **OIDC redirect fails** -- Ensure your IdP allows loopback redirect URIs (`http://127.0.0.1/callback`).
- **External auth provider not found** -- Check that the `auth_provider_command` path is correct and the binary is executable.
