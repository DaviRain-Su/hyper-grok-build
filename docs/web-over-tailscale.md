# Hyper Web over Tailscale

Drive a Hyper agent that stays on **your machine** from a browser on your
phone or another laptop. The network is Tailscale. Code, credentials, and
tools never leave the host.

This is not Amp Orbs (cloud VMs) and not `hyper dashboard --web` (read-only
metrics on loopback).

## Run the control plane

On the machine that has the repo and `~/.grok` auth:

```sh
hyper web
```

Default listen address is `127.0.0.1:9100`. Startup prints a local URL that
includes a token stored at `~/.grok/web-token` (mode `0600`).

`GET /healthz` is unauthenticated (liveness only). Every other route needs
the token as `Authorization: Bearer …`, `?token=…`, or cookie
`hyper_web_token`.

Chat sessions are not wired yet. The server is the authenticated listener
the later session API sits on.

## Put Tailscale in front

Do **not** bind `0.0.0.0` and do **not** enable Tailscale Funnel.

```sh
# still on the Hyper host
hyper web
tailscale serve --bg http://127.0.0.1:9100
```

Open the HTTPS MagicDNS URL Tailscale prints, and append the token:

```
https://<machine>.<tailnet>.ts.net/?token=<from hyper web stdout>
```

Phone and laptop must be on the same tailnet.

### Optional: bind a Tailscale address directly

```sh
hyper web --bind 100.x.y.z:9100 --allow-remote
```

`--allow-remote` is refused for unspecified addresses (`0.0.0.0` / `::`).

## Flags

| Flag | Meaning |
|------|---------|
| `--bind ADDR` | Listen address (default `127.0.0.1:9100`) |
| `--allow-remote` | Permit a non-loopback bind (Tailscale 100.x) |
| `--open` | Open the local URL in a browser (includes the token) |
