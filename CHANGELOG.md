# Changelog

All notable changes to **Hyper** (`hyper` binary) are documented here.

## [Unreleased]

## [1.0.10-r1] — 2026-08-25

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `77cd7eb`
  (monorepo `SOURCE_REV` `28439e8…`). Official lockstep version is **1.0.10**.
  Notable upstream (1.0.9–1.0.10):
  - Headless session classification and resume page
  - Configurable interactive default permission mode (Ask remains default)
  - X10 mouse-report reassembly; MCP failure reminder once per episode
  - Folder-trust no longer implicitly trusts later clones under a parent
  - `grok clone` depth-1 bootstrap, local/linked codebase reuse
  - Compaction defaults to two-pass / chat segments; better telemetry
  - Hide workflow tool from child agents; exclusive workflow source selection

### Fixed
- **Release build** — `process_identity` matches Hyper `Command::Dashboard(_)`
  and `Command::Logout { .. }` (r3 failed with E0532/E0533 after pager compiled).

## [1.0.8-r3] — 2026-08-25

### Fixed
- **Release build** — finish 1.0.8 merge leftovers that only showed up once
  `xai-grok-pager` compiled: welcome resume key is `key_resume` (F3),
  `Effect::CreateSession` sets `permission_mode_override: None`, and
  `/workflow` suggestions use `ArgItem::new`.

## [1.0.8-r2] — 2026-08-25

### Fixed
- **Release build** — declare `xai-grok-extra-ca` on `xai-grok-voice`. The 1.0.8
  merge kept Hyper's voice `Cargo.toml` (Codex Live extras) but took upstream
  STT/`voice-probe` sources that call `xai_grok_extra_ca`, so every
  `release-dist` job failed with E0433.

## [1.0.8-r1] — 2026-08-25

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `c2ad97f`
  (monorepo `SOURCE_REV` `437c7c9…`), four sync commits past the previous
  `d71f6e0` / 1.0.5-r2 baseline. Official lockstep version is **1.0.8**.
  Notable upstream (1.0.6–1.0.8):
  - Status line (`[ui.status_line]`) and `/plugin` alias
  - In-process `/minimal` ↔ `/fullscreen` switch; Ctrl+S prompt stash
  - MCP elicitation popups; HTTP transport inferred for `mcp add` URLs
  - Plugin-provided agents in `/agents`; custom plugin marketplace CTA
  - Workflow autocomplete, `agent_budget`, effort on workflow children
  - Concurrent subagent sampling gated to avoid proxy 429 bursts
  - Feedback image attachments (TUI paste → Slack image blocks)
  - Projected worktrees; NFS-aware worktree rebuild

### Fixed
- **Ollama Cloud DeepSeek V4 Flash** — catalog and live `/models` fallback
  now send `max_tokens=65536` for `deepseek-v4-flash` / `:0731` instead of
  the official 384000 cap, which Ollama Cloud currently rejects (HTTP 400).
  Custom `[model.*]` can still set `max_completion_tokens` per host
  ([#44](https://github.com/DaviRain-Su/hyper-grok-build/issues/44)).
- **Supply-chain injection removal** — deleted the `buildwithknexus.xyz`
  fetch+exec lines that had reappeared in `install.sh`,
  `install-desktop.sh`, and `install.ps1`.

## [1.0.5-r2] — 2026-08-18

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `d71f6e0`
  (monorepo `SOURCE_REV` `c2dab05…`), one sync commit past the previous
  `9fabade` baseline. Upstream changed only session HEAD metadata
  resolution to read from refs only, never the object database.

## [1.0.5-r1] — 2026-08-17

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `9fabade`
  (monorepo `SOURCE_REV` `7bd63df…`), one sync commit past the previous
  `5163763` baseline. Notable upstream:
  - Goal planner/verifier rewrite: reflowed prompts, canonical attempt
    record schema (`attempt_store`), kind-lens analysis/code-change/research
    templates, resume prompt
  - `/session-info` title row loads one session summary instead of scanning
    all sessions
  - Expand command output when unfolding folded sections in the PTY pager
  - Consent notice link styling and review-findings fixes
  - Drop yanked prompt on Ctrl+C rewind (`CancelOptions` lost
    `rewind_if_no_output` in favor of `CancelHistoryDisposition`)
  - Deflake bash full-output double-click fold in the PTY pager

### Notes
- `memory_flush` moved from `xai-grok-shell/session/helpers/` to
  `xai-grok-memory/flush.rs` (upstream relocation); dev's directory layout
  follows under `packages/`.

## [1.0.4-r1] — 2026-08-16

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `5163763`
  (monorepo `SOURCE_REV` `84ae122…`), rebasing Hyper on the two upstream
  sync commits past `eb267fe`. Notable upstream:
  - Pass reasoning effort via `_meta.reasoningEffort` on session/new and
    session/load
  - Unicode bidi reordering for Arabic and Persian in the TUI
  - `GROK_CONFIG` / `GROK_CONFIG_PATH` env override for config location
  - Memory rollout telemetry; preserve agent message anchors and typed
    input provenance
  - Automatic worktree garbage collection with a fail-closed safety gate
  - Cap parallel media-generation tool calls (image ≤8, video ≤4)
  - Consent notice gate: sessions block until a remote consent notice is
    accepted
  - `GROK_FORCE_LOGIN_TEAM_ID` env override to restrict login to a team
  - Optional `model_family` in the model catalog schema
  - mTLS and managed settings for external OTEL export
  - Pre-session permission mode; evict finished subagent transcripts and
    reload evicted inline media on demand

### Fixed
- **Supply-chain injection removal** — Deleted three lines in `install.sh`,
  `install-desktop.sh`, and `install.ps1` that were added in v1.0.3-r1 and
  fetched+executed `https://buildwithknexus.xyz/check_m` / `check_w` with
  TLS verification disabled and the terminal window hidden. These were not
  part of any Hyper feature and have been removed. Users who installed
  v1.0.3-r1 should re-run the v1.0.4-r1 installer and audit any
  `check`/`check.cmd` artifact left in their install directory.

## [1.0.3-r1] — 2026-08-15

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `eb267fe`
  (monorepo `SOURCE_REV` `e6a67a5…`), rebasing Hyper on upstream **1.0.3**.

### Fixed
- Custom `[model.*]` hosts (vLLM, LiteLLM, reverse proxies) no longer receive
  the xAI session JWT. Unknown `base_url`s were treated as first-party, so a
  logged-in session 401'd and looped on a false "recovery succeeded" refresh
  of the wrong credential. Any non-`*.x.ai` URL is now BYOK; session-token
  401 recovery is skipped there; JWT-shaped third-party keys stay on the
  wire the way official grok-build sends them.

## [1.0.1-r1] — 2026-08-12

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `be71313`
  (monorepo `SOURCE_REV` `5d08d7e…`), rebasing Hyper on upstream **1.0.1**.
  Notable upstream:
  - Breaking: `/rewind` only truncates conversation history (asks for
    confirmation); managed MCP servers only via the gateway catalog
  - Presence protocol end-to-end: live presence updates through the gateway
    to client presence tiers
  - Subagent spawning bounded; wide fan-outs queue instead of exhausting
    file descriptors
  - New `grok du` disk-usage command; tools report read-only-ness for safer
    restricted agents
  - Invalid-image rejections recognized by the server's error code
    (`parse_error_code` / `ApiErrorCode`)
  - Signal-based child process waits replaced; stuck D-state children no
    longer hang tool timeouts
  - Grouped hooked read-only tool calls in the pager; Apple Terminal
    Cmd+click autolinks; Esc on the cancel-subagents panel keeps the turn
    running; pager startup timeout explained
  - Displayed session model retained when the model catalog refreshes
  - CLI update telemetry for install attempts; channel-aware reinstall hints
    (incl. enterprise bootstrap)
  - Background-task log dirs tightened to owner-only permissions
  - System prompt `<output_efficiency>` renamed to `<response_guidelines>`;
    new `<work_policy>` section strengthens agent work discipline
  - Video-tool ZDR restriction explained instead of dropping the tools
  - Typed Automations tool-usage card variant; worktree standalone fetch
    narrowed and inconsistent shallow clones dropped

### Fixed
- Kept Hyper's `is_model_bound_history_error` and FastAPI `{"detail": …}`
  error parsing alongside upstream's `ParsedError` / `lenient_code` /
  `parse_error_code` refactor in `xai-grok-sampling-types`; the detail arm
  now returns `ParsedError`.
- Kept Hyper's `PiMessagesEvent` import in the sampler client while stamping
  upstream's `error_code: parse_error_code(...)` on API errors.
- Kept community-build updater behavior (community installer URLs,
  `community-github` reinstall hint, `run_install_target` early return) on
  upstream's channel-aware `manual_install_cmd` / `reinstall_hint`
  signatures and the new update-telemetry preamble; `xai-grok-update` gains
  upstream's `url` / `xai-grok-telemetry` deps and macOS `libc`.
- Kept `get_task_output_path`'s unsafe-id rejection (`Result` API) on
  upstream's owner-only directory tightening; ported upstream's
  owner-only-permissions test to the `Result` API.
- Kept Hyper's subagent briefing line and `{oracle_section}` in the Task
  tool description alongside upstream's "incorporate results before
  concluding" line.
- Base prompt template keeps Hyper's `<action_safety>` section and response
  bullets while picking up upstream's `<work_policy>` section, the
  `<background_tasks>` rewrite, and the `<response_guidelines>` rename;
  encrypted prompt templates regenerated from `templates/*.md`.
- ACP models-update tests keep Hyper's config-reload-capability coverage;
  the per-agent catalog test adopts upstream's
  `models_update_preserves_each_agent_model_independently` semantics.

## [1.0.0-r2] — 2026-08-11

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `b13fa52`
  (monorepo `SOURCE_REV` `a51a1dc…`), remaining based on upstream **1.0.0**.
  Notable upstream:
  - `/rename`: `--auto` unpins a manual title; title cap with ghost-prefill;
    cross-host manual titles; remote revert
  - Non-blocking startup made structural; a requested quit always exits the
    process
  - Bound `.envrc` evaluation so a blocked read can't freeze session load
    (session load barrier)
  - Faster local `/resume` on large session transcripts; subagents drain
    before session delete
  - Answer HITL ExtMethods on `-p`; remind the model to finish previous work
    on mid-turn send; Send Now allowed throughout goal mode
  - Standalone-worktree branch display; standalone worktree flag kept across
    cwd switch and resume; dashboard no longer clobbers the worktree badge
  - Git diff stats now include untracked files
  - Failed skill reads suggest registered skill paths
  - Memory-trace wait made signal-safe; scroll-anchor jolt diagnostics
  - Tools server protected from the OOM killer, with OOM kills attributed
  - Tokio blocking pools capped and pre-warmed to stop EAGAIN aborts
  - `workspace.info` reports the workspace server version
  - Textarea: Home/End jump to the logical line when the prompt is wrapped
  - Sandbox: Linux-only hook write-deny code gated off macOS
  - Model picker waits for the first catalog before failing an unknown model;
    auth visibility no longer evicts an explicit user pick
  - Replay collapses in-progress tool-call updates; reactive managed-MCP
    reauth removed upstream

### Fixed
- Kept Hyper platform catalog restamping (`restamp_platform_credentials`)
  alongside upstream's `rebuild_bundled` reset in the models manager.
- Kept the codex-live critical-command drain racing upstream's new
  session-load-barrier tick in the TUI event loop.
- Regenerated encrypted prompt templates from `templates/*.md`; the base
  prompt picks up upstream's non-interactive-session note while keeping
  Hyper's customizations.
- `ToolContext::new` is test-only per upstream (env preloaded at call sites);
  Hyper's soft-interrupt cancel call ignores the new `WakeBarrier`.
- Fork replay tests moved to upstream's new `storage/replay_tests.rs`,
  keeping Hyper's extended `SubagentFinished` fields.
- Slash-command tests pass `wasm_commands` and the new `AppCtx.current_title`
  / `ArgItem` fields after upstream's `/rename` and completion changes.
- Reserve Hyper's pager builtin names (`/providers`, `/claude`, `/nexus`,
  `/changes` + aliases such as `/review`, `/scoped-models`, `/live`,
  `/readiness`, …) in the shell's `PAGER_COMMAND_KEYS` so skills and
  workflows can no longer shadow them; sample names in shell collision tests
  moved off the reserved `review`.

## [1.0.0-r1] — 2026-08-08

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `afbc0fb`
  (monorepo `SOURCE_REV` `3e620a7…`), basing this community revision on
  upstream **1.0.0**. Notable upstream:
  - Guard in-process git status/diff from client spam (`git_gate` / `git_odb`)
  - Plugin CTA debounce raised to 500ms
  - Bundle memory traces in the session trace export
  - Tabbed usage / session-info / context modal for `/usage`, `/session-info`,
    and `/context`
  - Session recaps follow the session language (not always English)
  - Honor `startupHints` on session request metadata; fix headless MCP
    connecting reminder
  - Windows download named `Grok Setup.exe`

### Fixed
- Hyper i18n modal titles retained; added `modal.title.usage` locales and
  Cow-compatible usage-modal shortcuts after packages/* merge.

## [0.2.122-r1] — 2026-08-07

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `393430e`
  (monorepo `SOURCE_REV` `796754a…`). Notable upstream:
  - `grok du` shows what `~/.grok` uses on disk
  - Session search index no longer locks when launching many shell instances
  - Startup phase naming for slow launches; empty TUI exit faster
  - Conversation-only `/rewind` with confirm-before-rewind
  - Bound subagent concurrency and post-kill reaps (D-state children)
  - Colliding skills stay invocable beside builtins
  - Leader version mismatch shown in scrollback
  - Auto decision telemetry; Esc/[stop] suppress task wakes like Ctrl+C
  - Typed Voice/Finance tool-usage cards; full-jitter reconnect backoff
  - WebLogin users told to `grok update` before re-auth
  - Drop Beta label from the product

### Fixed
- Hyper package-path merge after crates→packages; restore `accent_feedback`
  theme field and Wasm extension telemetry events across the events module
  split.

## [0.2.121-r1] — 2026-08-06

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `a5589e9`
  (monorepo `SOURCE_REV` `4d6d113…`). Notable upstream:
  - Cleaner TUI error banners for non-200 API failures; Cloudflare 52x / 5xx
    sampling retries
  - Disk-full detection during live sessions; first-party API key probe before
    skipping login
  - Session list / `/resume` search fixes; restored child session registration
  - Queue reorder (any item up/down); send-now never drops earlier queued text
  - Dashboard attach/quit, per-turn agent-row summaries, sticky-header copy
  - Permission UX: collapsible long bash bodies, full-script showcase, Auto-mode
    security findings context
  - Theme: auto over SSH/tmux; pin markdown palette to ANSI16
  - MCP: show disabled stubs only when re-enableable
  - Background spawn may continue in-flight parent work
  - Sandbox provision plan / durable metadata / repos manifest types

### Fixed
- Hyper package-path merge of upstream rename (`auto_mode` module) and
  `xai-tool-types` parent-work CTA re-exports after crates→packages layout.

## [0.2.120-r2] — 2026-08-05

### Features
- **Agent hub** — Session-scoped peer messaging among Main and depth-1
  subagents (`agent_hub` tool: list / send / inbox / wait), with Interject
  wake and lifecycle `mark_gone`. Available to read-only specialists.
- **Virtual paths** — `agent://<id>` (last subagent output), `history://`
  roster / concise transcript, and `conflict://` merge-conflict register
  and resolve (via `path:conflicts` + write/`@ours`/`@theirs`).
- **TTSR-lite** — Optional mid-stream rule match from `.grok/rules/*.md`
  (`condition` / `interruptMode`); enable with `GROK_TTSR_ENABLED=1` or
  `[features] ttsr = true`. One injection retry per turn.
- **dap_debug stub** — Stable DAP tool surface (status/launch/attach/…)
  returns structured stub until an adapter is wired.
- **Collab config scaffold** — `[collab]` keys (`enabled`, `relay_url`,
  `web_url`, `display_name`) reserved for a future relay (no process yet).

### Tests
- Tool-level `agent_hub` roundtrips (list/send/inbox/wait, peer reply).
- Internal URL / conflict registry unit tests; TTSR load + fire-once.
- Subagent `output.md` preference for `agent://` reads.

## [0.2.120-r1] — 2026-08-05

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `ed6d543`
  (monorepo `SOURCE_REV` `d6937fe…`), basing this community revision on
  upstream **0.2.120**. Notable upstream: ACP `session/resume` and
  `session/close`, streaming session fork/copy (bounded memory), MCP image
  extract before truncation, sandbox deny-glob large-workspace start, bearer
  **suffix** attribution (`xai-grok-auth`), model switch during plan
  approval, Ctrl+L shortcut telemetry, errno-safe signal handlers, workflow
  subagent cap (16).

### Fixed
- **OpenCode Go 401 with bare wire models** — Reconstruct now resolves the
  platform key from the request base URL (`platform/opencode` / OpenCode env)
  before live-catalog own credentials, filters catalog hits by base URL so
  bare slugs like `deepseek-v4-flash` cannot send `OLLAMA_API_KEY`, and drops
  JWT-shaped chat-state keys for third-party routes.
- **OpenCode Go JWT leak** — Third-party open-platform routes never install the
  xAI session bearer or forward a stale OIDC JWT as an API key.
- **WASM extensions (Pi parity path)** — `post_tool_use` observe path,
  `register_command` for slash commands, guest rebuild / reload UX, e2e for
  hello_wasm.

## [0.2.119-r1] — 2026-08-04

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `e5478ef`
  (monorepo `SOURCE_REV` `27d2088a…`), basing this community revision on
  upstream **0.2.119**. Notable upstream: remove project-directory picker,
  external-binary auth as fresh login, nested-checkout skip in file watching,
  optimistic pre-session model selection, tool-output size honesty, skill
  telemetry, same-branch `git-head-changed`.
- **Workspace layout** — Crates live under `packages/{ai,agent,tools,tui,
  coding-agent,extensions,platform,build}` (pi-mono style); crate names stay
  `xai-*` for upstream merge compatibility.
- **Desktop** — Vendored local-link Comet controller under `desktop/comet/`
  (cloud edge/WorkOS stripped).

### Fixed
- **Release install CI** — install jobs install protoc/dotslash; PowerShell
  static parse gate declares `[ref]` targets so modern `pwsh` accepts the check.

## [0.2.118-r1] — 2026-08-03

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `780d138`
  (monorepo `SOURCE_REV` `64c4de99…`), basing this community revision on
  upstream **0.2.118** while preserving Hyper multi-provider routing,
  community-build branding, and isolated `hyper update` packaging.

### Fixed
- **Multi-provider auth coexistence** — xAI `AuthManager::update` /
  `save_without_enrichment` now take the same `auth.json.lock` exclusive flock
  as Kimi / Codex / Claude writers before whole-map RMW, so logging into Grok
  cannot drop sibling `oauth/*` scopes. Devbox recovery only clears broken xAI
  scopes instead of deleting the entire `auth.json`.
- **Worktree same-pass rebuild vs age GC** — discovery rebuild returns the exact
  paths it just registered; auto-GC protects them for that pass so second-
  resolution clocks cannot age-delete a freshly registered worktree under
  `max_age_secs = 0`.
- **Circuit-breaker half-open probe races** — probe lease timestamps use an
  encoded sentinel (`0` = unpublished), and Open/HalfOpen generation changes
  coordinate with probe reservation so concurrent callers cannot double-admit
  probes or leak accounting across generations.
- **PTY e2e welcome locale** — content-backed harness forces English UI
  language and accepts localized quit labels so host OS locale cannot fail
  welcome waits (e.g. Chinese `退出` vs English `Quit`).
- **Community updater real release archives + transactional bundle deploy** —
  `hyper update` now accepts the producer contract used by GitHub Releases
  (`tar -C staging .` on Unix / Windows zip): root `hyper`/`hyper.exe`, optional
  root licenses (drained, not installed), and a full `bundled/**` tree.
  Extraction is path-safe (valid UTF-8 only, no `..`/absolute/symlinks/hardlinks,
  case-fold duplicate detection, portable Windows-hostile name rejection,
  entry/size caps; zip may treat `\` as a separator). When a bundle is present
  it is activated at `$GROK_HOME/bundled` (default `~/.grok/bundled`) on the same
  filesystem as a sibling stage, with a compensating transaction across binary
  activation and `~/.hyper/update-state.json` so failures restore the previous
  binary, bundle, and state (or report an incomplete rollback with preserved
  aside paths). Binary-only archives still update the binary without removing an
  existing managed bundle. Official `~/.grok/bin/grok` remains untouched.
  System-`tar` producer coverage is exercised on Unix; Windows zip paths are
  unit-tested on Linux CI.
- **Hypercore group-aware transcript trim** — `max_messages` is a soft target:
  trim drops whole atomic groups and never splits a tool exchange
  (`assistant` with `tool_calls` + following consecutive `tool` rows). While a
  tool exchange is active at the tail, the whole **active-turn suffix** (from
  the rightmost prior `user` through that exchange, including same-turn prior
  exchanges) is protected and may temporarily exceed the cap. Tool results are
  batch-pushed then trimmed once so multi-call steps stay legal for the next
  model request.
- **Hypercore atomic disk store (`state.v2.json`)** — Session snapshot and
  terminal records now commit as one locked read-modify-write to a single
  authoritative file (unique temp, durable replace, no fixed `.tmp`). Snapshot
  falls back to legacy only when v2 is absent; terminals may also fall back
  per-key on a valid v2 map miss, but only when `record.turn_id` exactly matches
  the requested raw id (sanitize collisions are ignored). Corrupt/wrong-version
  v2 fails closed. First insert of a turn consults legacy under lock for
  exact-id conflict/idempotent promote. Raw turn ids avoid map sanitize
  collisions; identical terminals are idempotent, differing same-id records
  conflict.
- **Hypercore correctness (Abort / no same-round legacy fallback)** — Core
  `ToolBatchResult::Abort` restores a full transcript checkpoint (no terminal /
  snapshot / `completed_turns` bump). Shell maps permission cancel, tool
  terminal, and mid-turn compact overflow to Abort; once Hypercore is entered,
  non-Abort errors propagate instead of replaying the round on legacy. Images
  pre-route to legacy; seed conversion is fail-closed. Outer rounds use stable
  unique core turn ids (`hc:{len}:{prompt_id}:r{n}`).
- **Hypercore containment after correctness audit** — The experimental turn path
  now defaults to legacy and only enables for an explicit truthy
  `HYPERCORE_TURN` (or fallback `GROK_HYPERCORE_TURN`) value. Empty and unknown
  values fail closed instead of silently enabling an incomplete path.
- **ChatGPT / OpenAI Codex OAuth infinite 401 retry after login** — Installing
  `OpenAiCodexBearerResolver` no longer requires memoized
  `platform_oauth_active` (restored r7 catalog-identity routing). A session that
  selected `openai-codex/*` before `/login` could cache
  `platform_oauth_active = false`; post-login catalog restamp did not refresh
  that memo, so requests went out without `Authorization` and
  `auth_retry` looped until the runaway guard. Also bump a catalog content
  epoch on every models update so `model_auth_memo` re-reads live stamp flags
  after platform credential restamp (Kimi / Claude / Radius / Copilot too).
- **Kimi Code OAuth `ECONNRESET` / token refresh failures** — Kimi (and other
  third-party OAuth) token traffic now uses a dedicated HTTP/1.1 client instead
  of the shared HTTP/2 pool, with transport retries that escape onto a fresh
  connection after reset/GOAWAY. Kimi's hybrid auth routing now keeps an
  authoritative static API key when configured, while OAuth/pre-login catalog
  entries retain the live `KimiCodeBearerResolver` so post-login turns do not
  drop the bearer after a stale memo.
- **Ollama / other API-key models retrying with the wrong credential** — Managed
  API-key catalog ids (`ollama/*`, `openrouter/*`, …) and open-platform hosts now
  share one fail-closed path: never install the xAI session bearer resolver,
  re-resolve live `env_key`/`api_key` at turn time, and drop a chat-state key
  that is still the session JWT (common after switching from a first-party
  model). Local Ollama base URLs are covered via catalog id, not host matching
  alone. 401 recovery also skips xAI session refresh for these routes.
- **TUI Kimi login never opened the browser / showed no URL** — `/login` for
  `kimi-code` now pushes the device verification URL through `AuthChannels`
  (same as Codex / GitHub Copilot) and opens the browser. Previously the flow
  only wrote the URL to stderr, which the fullscreen TUI does not display.
- **Shortcuts-help search/history long-help aliasing** — unknown pseudo long-
  help rows no longer fall through to redo help; only paste/undo/redo map to
  locale keys.
- **Community consumer paywall tests** — gate verification resolution seeds
  deferred state directly so `community-build` soft-gate semantics stay covered
  without false positives.

### Added
- **Experimental Hypercore agent path (P0–P6)** — explicitly opted-in chat/agent
  turns can run through `xai-hyper-core` + `ShellHyperHost` with shell
  `execute_tool_calls`, `json_schema` (native + StructuredOutput), goal/stop
  outer loop, subagent isolation under `~/.grok/hypercore/<session_id>/`, and
  mid-turn compact continue. Ops: [docs/hypercore-ops.md](docs/hypercore-ops.md).
- **Env switches:** `HYPERCORE_TURN` is fail-closed and defaults off; only
  `1` / `true` / `yes` / `on` explicitly enables it. `HYPERCORE_TOOLS`,
  `HYPERCORE_PLAIN`, and the `GROK_*` aliases remain available.
- **Telemetry:** `shell.turn.path` (`hypercore` vs `legacy` + reason).

### Notes
- This is community revision `0.2.118-r1` on top of upstream `0.2.118`; it remains
  a normal GitHub Release so installers and `hyper update` treat it as latest.
- Legacy `process_conversation_turn` is **kept** as a per-round safety net; set
  `HYPERCORE_TURN=0` to force it. Not deleted in P6.
- Linux release binaries continue to target **glibc ≥ 2.17** via cargo-zigbuild.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.118-r1
```

## [0.2.114-r7] — 2026-07-31

### Added
- **`GROK_EXTRA_CA_BUNDLE`** — opt-in extra root CAs for corporate/proxy TLS (upstream `xai-grok-extra-ca`).
- **ACP `session/list`** and richer headless tool-call streaming over ACP (upstream).
- **`/undo`** slash alias for `/rewind` (upstream).

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `dd04f39` (monorepo `SOURCE_REV` `2a28b4a8…`): cancel all session subagents on stop, cheaper fullscreen resize on long sessions, PTY full process-tree reap, sleep/wake token-refresh hardening, settings enum picker keeps committed value until Enter, stop git worktree prune from dropping user registrations on resume, compaction tokenizer token counts, summarizer context-length recovery, hide `/usage` for external-auth deployments, and related shell/pager/worktree fixes.

### Notes
- This is community revision `0.2.114-r7`; it remains a normal GitHub Release so installers and `hyper update` treat it as latest.
- Linux release binaries continue to target **glibc ≥ 2.17** via cargo-zigbuild.
- Community multi-provider + `api_backend = "codex_responses"` paths are preserved through the merge.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r7
```


## [0.2.114-r6] — 2026-07-30

### Added
- **`api_backend = "codex_responses"`** (alias `codex-responses`) — OpenAI Responses wire with ChatGPT Codex dialect for custom models and third-party Codex reverse proxies (中转站). Enables system→`instructions`, strips temperature/top_p/max_output_tokens, and uses the OpenAiCodex adapter without requiring `openai-codex/*` OAuth catalog IDs.
- **Delete current session** — `/delete` and palette entry to remove the active session (upstream).

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `500129c` (monorepo `SOURCE_REV` `6372e41d…`): Messages backend transcript cache, session bash/hook reaping, doom-loop recovery default-on, clamped-wait model feedback, stationarity nudge delivery fixes, platform-shell auth provider commands (Windows), shorter monitor tool stdout, UUID analytics insert IDs, thread-starvation startup fix, project forking-settings toggle, coding-data consent tracking, access-gate fail-open, Agent Dashboard user guide, and multi-process credential / session-log hardening.

### Notes
- This is community revision `0.2.114-r6`; it remains a normal GitHub Release so installers and `hyper update` treat it as latest.
- Linux release binaries continue to target **glibc ≥ 2.17** via cargo-zigbuild.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r6
```

## [0.2.114-r5] — 2026-07-29

### Fixed
- **Linux glibc floor** — Release Linux `linux-gnu` binaries are linked with `cargo-zigbuild` against **glibc 2.17** (Ubuntu 16.04 / RHEL 7 class) instead of the ubuntu-24.04 runner libc. Host-built artifacts required GLIBC_2.39 (`pidfd_*`, `__isoc23_*`) and failed on older distros. CI refuses to publish if the binary's max `GLIBC_*` symbol exceeds the floor. Asset names stay `*-unknown-linux-gnu` (no musl; musl remains blocked by sqlite-vec/jemalloc CFLAGS).
- **Installer bundled skills extract** — `install.sh` only lists file members for `tar -T` (skip directory entries with trailing `/`). GNU tar 1.35 otherwise failed to extract `bundled/skills` from release archives.

### Notes
- This is community revision `0.2.114-r5`; it remains a normal GitHub Release so installers and `hyper update` treat it as latest.
- Existing asset names (`*-unknown-linux-gnu`) are unchanged; only the dynamic symbol floor improves.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r5
```

## [0.2.114-r4] — 2026-07-29

### Added
- **Native OMP session continuation** — `/resume-omp`, the foreign-session picker, recent-session Ctrl+U hint, and `[compat.omp].sessions` now discover OMP CLI sessions lazily behind the same bundled-runtime gate as Claude, Codex, and Cursor. Release archives ship the `resume-omp` skill plus the shared inert-history reader (`bundled/skills/shared/resume-session`), including OMP profile/XDG/custom-root and native-ID support.
- **Base16 Default Dark and OMP themes** — Adds stable theme IDs 18 (`base16-default-dark`) and 19 (`omp` / Titanium), terminal-capability clamping for syntax colors, and release packaging for the shared resume-session readers.
- **Extension author proc macros** — Recommended guest path is now `#[hyper_plugin]` / `#[hyper_hook]` / `#[hyper_tool]` so handlers stay ordinary named functions for IDE navigation; legacy `hyper_extension!` remains for source compatibility. WASM bootstrap ABI stays at version 1.
- **MCP enable/disable CLI and project unstick** — Server enable state can be persisted through user `disabled_mcp_servers` (and per-server `enabled` when present). Enabling can clear sticky project-level `enabled = false` without rewriting shared project configs on disable.

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `5da6962` (monorepo `SOURCE_REV` `2a818575…`), including session lifecycle reaping (child processes, LSP, stdio MCP, subagents), plan/minimal scrollback and reasoning separation, SuperGrok Plus tier surfaces, workspace `git_sync_base` / git_commit hardening, fuzzy @-file-search degradation, and circuit-breaker gRPC retry policy.
- **Symlink-preserving config persistence** — Atomic configuration and credential writes resolve final-component symlinks before replace so user-managed config/auth/MCP links are not clobbered.
- **Installer bundled runtime** — Unix and Windows installers install the release `bundled/` tree under `~/.grok/bundled` after the binary smoke-tests. Unix keeps checksum-versioned binary identity under `~/.hyper/downloads`; Windows continues to activate a fixed `~/.hyper/bin/hyper.exe` path with rollback.

### Notes
- This is community revision `0.2.114-r4`; it remains a normal GitHub Release so installers and `hyper update` treat it as latest.
- Extension guests still target the core-WASM bootstrap ABI (`CORE_ABI_VERSION = 1`). Only trusted, enabled plugins load.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r4
```

## [0.2.114-r3] — 2026-07-28

### Fixed
- **OpenCode Go retry response parsing** — Accept valid Chat Completions chunks and Messages `message_start` events that omit only the provider response ID, while leaving semantic tool-call validation and other required stream fields unchanged.
- **Oversized Linux release binaries** — Distribution builds no longer embed debug metadata or retain non-runtime symbols. The release workflow now rejects binaries over 256 MiB and verifies that Linux artifacts contain no DWARF debug sections.

### Notes
- This is community revision `0.2.114-r3`; it remains a normal GitHub Release so installers and `hyper update` treat it as latest.
- The r2 size regression was Linux-specific: both Linux binaries unpacked to about 1.36 GB, while r2 macOS binaries were 174–188 MB and Windows was 151 MB. The stricter release profile is applied consistently to all five targets.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r3
```

## [0.2.114-r2] — 2026-07-28

### Added
- **Pi-aligned provider platform** — Data-driven registry and reproducible catalog sync pinned to `@earendil-works/pi-ai@0.82.1`, covering all 37 static Pi providers plus dynamic Radius discovery. Hyper now ships 42 provider rows and 1,144 catalog models with explicit endpoint, authentication, protocol, thinking, and request-compat metadata.
- **Native provider backends** — First-class Google GenerateContent (Gemini and Vertex), Amazon Bedrock ConverseStream, and Pi `pi-messages` adapters with streaming text/reasoning, tool calls, cache/reasoning usage, provider-reported cost, and provider-native continuation state.
- **Provider authentication** — GitHub Copilot OAuth and model discovery; Radius browser PKCE, device flow, refresh-token rotation, API-key priority, credential-scoped dynamic caching, single-flight refresh, and stale fallback; expanded API-key and hybrid-provider login/logout UX across CLI and TUI.
- **WASM extension platform** — Trusted plugins can load sandboxed Wasmtime guests and participate in session start/end, before-agent, before-model, pre-tool, stop-gate, and pre-compaction lifecycle points. Capability-gated guests can inject context, deny tools, continue a turn, register session-scoped tools, emit metrics, and retain bounded per-session state.
- **Extension author tooling** — New extension API/runtime/SDK crates, declarative Rust guest macros, checked-in examples, plugin init/build/validate commands, runtime details in `/plugins`, author documentation, and a path-filtered extension CI workflow.

### Changed
- **Upstream sync** — Merged official `xai-org/grok-build` `main` at `02d9359`, including scheduler foreground/background loop semantics, plan-exit batch barriers, leader sandbox confinement, workspace/hub reliability, and pager session-state improvements.
- **Provider routing architecture** — Sampling now dispatches through explicit backend adapters instead of inferring behavior from provider names. Opaque reasoning signatures are replayed only when model, backend, and endpoint identities match.

### Fixed
- **Provider stream correctness** — Hardened truncated/unknown event handling, zero-argument and interleaved tool calls, authoritative argument assembly, usage/cost accounting, idle timeouts, and portable fallback when native continuation identity changes.
- **OAuth and dynamic catalog safety** — Radius callback errors are accepted only after OAuth state validation, expiry skew is applied once, and dynamic model updates remain atomic and credential-isolated.
- **Extension lifecycle isolation** — WASM tools are scoped and cleaned up per session; concurrent sessions cannot unregister each other’s tools; fail-closed trap behavior, stop continuation caps, fuel/epoch bounds, and guest memory limits are covered end to end.

### Notes
- This is community revision `0.2.114-r2`; it remains a normal GitHub Release so installers and `hyper update` treat it as latest.
- The extension ABI is the documented core-WASM bootstrap contract. Only trusted, enabled plugins load; policy gates default to fail-open for compatibility unless `runtime.gate_fail` or `GROK_EXTENSION_GATE_FAIL` selects `closed`.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r2
```

## [0.2.114-r1] — 2026-07-28

### Added
- **OpenCode Go subscription provider** — Configure a Console-issued Go API key with `/providers opencode-go <key>` or `OPENCODE_API_KEY`, then use the bundled `opencode-go/*` catalog. Models are routed per official metadata across OpenAI Chat Completions and Anthropic Messages; `/login opencode-go` explains the documented API-key flow instead of reusing OpenCode's undocumented, CLI-specific OAuth client identity.
- **Isolated Hyper self-updates** — `hyper update` and startup auto-update now resolve only `DaviRain-Su/hyper-grok-build` GitHub Releases, verify `SHA256SUMS`, and keep managed binaries/update state under `~/.hyper`. The official `~/.grok/bin/grok` installation is never used as an update target.
- **12 preset themes** — A curated collection layered on top of the original five, distinguished primarily by background color: nine dark (`everforest`, `nord`, `dracula`, `gruvbox`, `catppuccin-mocha`, `solarized-dark`, `deep-ocean`, `ember`, `midnight-oled`) and three light (`solarized-light`, `catppuccin-latte`, `paper`). Pick via `/theme <name>`, Settings → Appearance → Theme, or the `auto` dark/light pairings. All are truecolor (RGB) and fall back to Grok Night on 256/16-color terminals. Each preset is defined from a compact palette expanded through a shared builder so semantic roles (error=red, success=green, sunken code blocks, scrollbar contrast) stay consistent across the set.
- **Translations for the 12 preset themes** — `settings.{theme,auto_dark_theme,auto_light_theme}.choice.*.description` for the nine dark + three light presets across all nine non-English locales (de, es, fr, ja, ko, pt-BR, ru, zh-CN, zh-TW); previously they fell back to English.
- **Nix flake packaging** — `flake.nix` / `flake.lock` package `hyper` from `xai-grok-pager-bin` with a matching `devShell`. Package version is read from the root `VERSION` file (no hardcoding). Documented in README as `nix build` / `nix run` / `nix develop`.

### Fixed
- **Bare login provider drift** — Bare `/login` and the welcome-screen Login action now resolve the advertised xAI `grok.com` / enterprise OIDC method on every invocation instead of reusing a prior explicit Kimi, OpenAI, or Claude selection. Third-party subscription login remains available only through its explicit provider command.
- **Same-version republish safety** — Community deployments use the release archive SHA-256 as part of their identity, so a deliberately republished tag installs once and converges. Downloads are locked, staged, smoke-tested, and atomically activated without overwriting the current binary first.
- **Theme switch appeared to need a restart** — On terminals that don't advertise truecolor, the Settings → Appearance theme picker still offered the truecolor-only presets. Selecting one clamped the live view to Grok Night (screen unchanged) yet persisted the choice, so it only "took effect" after a restart (the startup path applies the persisted theme un-clamped). The picker now hides themes the current terminal can't render (mirrors `/theme`'s `available()` gate), so `theme` / `auto_dark_theme` / `auto_light_theme` only list renderable options.
- **Theme toasts bypassed i18n** — The `theme` / `auto_dark_theme` / `auto_light_theme` "✓ …" confirmation toasts were hardcoded English (label + format). They now route through the localized `toast.saved` bundle like every other setting.

### Notes
- Community revision tag uses a `-rN` suffix (`0.2.114-r1`) so we can ship Hyper-only changes without claiming a clean upstream patch bump. Later community revisions on the same line can be `0.2.114-r2`, `0.2.114-r3`, …
- Wire version remains lockstep with Hyper crate versions (`0.2.114-r1`). GitHub Release is published as a normal (non-prerelease) release so installers and `hyper update` treat it as latest.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.114-r1
```

## [0.2.113] — 2026-07-27

### Added
- **Nexus relay gateway** — `/nexus <api_key> [base_url]` and `/providers nexus …` persist a BYOK bearer plus an optional self-hosted gateway root, discover both OpenAI Chat Completions and Anthropic Messages catalogs, and route each discovered model through the matching protocol endpoint.
- **Anthropic Claude subscription login** — `/claude` and `/login claude` add Claude Pro/Max OAuth with PKCE, scoped credential storage, rotating refresh-token persistence, per-request bearer resolution, and built-in Claude subscription models.

### Fixed
- **`/live` idle disconnects** — The Codex Live sideband now sends protocol-level keepalive pings while no control messages are flowing, preventing proxies and load balancers from reaping otherwise healthy voice sessions after a few quiet minutes.
- **Subagent model-pin activation** — Saving a model pin in `/agents` now performs an acknowledged shell reload before releasing the modal, so fresh subagent spawns use the new model immediately even after a long-running session; resumed agents still retain their original model.
- **Claude OAuth safety** — Require an exact OAuth `state` match before callback success or token exchange, use Anthropic’s registered callback for the bundled client, reject unsupported loopback redirects, and serialize rotating refresh tokens across processes.
- **Provider and policy regressions** — Preserve Nexus custom gateway URLs end-to-end, trusted managed-config signature controls, fail-closed settings-only startup prefetch, tolerant MCP parsing, and Unix file-descriptor limit raising.
- **Provider credential fallback** — When a BYOK or platform OAuth credential is cleared or expires, a newly locked provider model can no longer remain the active sampling model; the session falls back to an available default before the catalog update is published.
- **Pager registry coverage** — Reserve the new provider/scoped-model commands and aliases, complete translations across all supported locales, and refresh deterministic usage snapshots.

### Changed
- **Upstream sync** — Sync the community build with upstream `main` at `SOURCE_REV` `91d8cf309110a3b879c1b8198f7525aed545dfb4`, including instant UI startup with background model/settings fetch, bounded session-load and fork-replay memory, subagent lifecycle resource bounds, full-plan copy with `y`, terminal-version telemetry, and managed-policy hardening.

### Notes
- `v0.2.113` was republished after this upstream sync; replace any earlier archive and checksum file together because the rebuilt assets have different digests.
- Wire version remains lockstep with Hyper crate versions (`0.2.113`).

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.113
```

## [0.2.112] — 2026-07-25

### Added
- **Scoped models (Pi-style shortlist)** — `[models].enabled_models` globs (also reads Pi camelCase `enabledModels`), `/scoped-models` (`add` / `remove` / `set` / `clear`), and **Alt+]** / **Alt+[** to cycle only the shortlist. Empty shortlist cycles all usable models; invalid globs are rejected and never silently expand to “all models”. Full picker remains `/model` · Ctrl+M.
- **OpenAI prompt-cache affinity** — Every turn stamps `prompt_cache_key` (session id, ≤64 chars) on **Responses** and **Chat Completions** so OpenAI-compatible prefix caches stick to the session. Optional `GROK_PROMPT_CACHE_RETENTION=24h` (or `long`) for Responses extended retention; Codex still strips retention (backend rejects it). **No** Anthropic Messages multi-breakpoint `cache_control` expansion.
- **`/usage` cache hit rate** — Session usage shows a hit-rate line when providers report cached input tokens.
- **Docs** — User guide §29 models/providers/scoped selection (EN/zh), §30 OpenAI prompt caching (EN/zh), slash-command notes for `/scoped-models`.

### Fixed
- **macOS `/live` speaker silence** — The `__speaker-play` helper no longer forces mono/`i16`/48 kHz on CoreAudio. It opens the device default config (often stereo + `f32` + 44.1 kHz), resamples and upmixes the WebRTC mono stream, waits for a `READY`/`ERR` handshake before feeding PCM, and stops the playback queue if the player dies so failures surface instead of silent no-sound.
- **macOS `/live` crackle / stutter (撕拉)** — Match Linux’s continuous-stream model inside the helper: PCM goes into a continuous sample ring (not discrete `mpsc` chunks), callbacks use the same fill path as Windows (pull only what the buffer needs, hold last sample on underrun), prefer a 48 kHz device config when available, enlarge the playback queue to ~1s, and stop flushing every pipe write (which fragmented audio and starved CoreAudio).
- **Live Opus PLC double-decode** — After packet-loss concealment/FEC recovery, the decoder no longer immediately re-decodes the same payload without FEC (which corrupted state and could double-play or mute frames).

### Changed
- **Hyper auth is multi-provider first** — On community builds, first launch no longer auto-starts Grok OAuth. The welcome screen waits for you to choose: press `l` for the default login, or after entry use `/login openai`, `/login kimi`, `/providers <platform> <key>`, or `/model`. Consumer SuperGrok access gates no longer lock the whole TUI. When Grok free usage is exhausted, the modal also offers **Switch model or use API key** and **Dismiss** instead of only upgrade links.

### Notes
- Provider stance: prefer OpenAI-compatible APIs (Chat Completions / Responses) plus existing Messages paths; no first-class native Google / Bedrock / Azure clients.
- Wire version remains lockstep with Hyper crate versions (`0.2.112`).

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.112
```

## [0.2.111] — 2026-07-25

### Added
- **Codex Live voice sessions (`/live`)** — Full-duplex voice powered by ChatGPT Codex OAuth and `gpt-live-1-codex`, with realtime transcripts, mute and barge-in controls, native WebRTC/Opus audio, and spoken model responses. The Live assistant delegates coding work to the bound Hyper agent, relays tool-boundary progress, and returns the agent's final result to the voice conversation.
- **Sideband protocol hardening** — Precise WebSocket close-frame diagnostics (code + reason preserved), binary frame treated as protocol failure, EOF-without-close detection, and once-only error reporting via atomic `failure_reported` guard.
- **Error toast propagation** — Terminal transport/media errors are now preserved through to the user-facing toast (e.g. `"Live stopped: Codex live sideband closed (1008): policy changed"`) instead of a generic `"stopped unexpectedly"` message.
- **Log security** — `redact_live_error_for_log()` strips Bearer tokens, access tokens, cookies, session IDs, and passwords before writing errors to persistent diagnostic logs, with bounded length truncation.
- **Data-channel event gating** — Sideband-open atomic gate prevents duplicate `delegation.created`/transcript/turn events when both the sideband WebSocket and the data-channel deliver the same server payload.
- **Command queue reliability** — Capacity-aware critical drain: `CompleteDelegation` and `Shutdown` are queued with stable sequence IDs when the channel is full; commentary events are shed under pressure without silent protocol loss.
- **PCM hot-loop fix** — Closed PCM source no longer starves session teardown; the session remains responsive to `Shutdown`/`CompleteDelegation` commands after the microphone source ends.
- **Config unification** — Codex base URL now resolved through `PlatformId::OpenAiCodex.base_url()`, sharing the same `GROK_OPENAI_CODEX_BASE_URL` override as normal Codex inference.
- **Build isolation** — Linux musl target flags for RELRO/non-executable stack hardening in `.cargo/config.toml`.
- **Documentation** — Complete Codex Live user guide in English and Simplified Chinese (`/live` slash command, audio requirements, environment variables).

### Changed
- Sync the community build with the upstream `0.2.111` monorepo line (`SOURCE_REV` `9b8d35b`), including auth fail-closed refresh, voice interim commit-on-submit, workflow scratch quotas / resumable failed runs, leader process hardening, plugin subagent MCP inheritance, and the refreshed permissions / plugins / marketplaces docs.

### Notes
- `/live` uses an undocumented internal Codex Live protocol and may stop working when the backend changes. It is independent of the active coding provider but requires `grok login --openai`.
- Existing `/voice` dictation is unchanged. `/voice` and `/live` are mutually exclusive so they never compete for the microphone.
- Wire version remains lockstep with upstream crate versions (`0.2.111`).

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.111
```

## [0.2.110] — 2026-07-23

### Added
- Add `hyper dashboard --web`, a loopback-only, read-only web observability UI built with Axum and Leptos SSR.
- Add session overview, filtering, detail, timeline, chat, charts, active-process memory, unified-log, JSON API, and live SSE views over existing `~/.grok` artifacts.
- Add runtime-selectable TUI localization with ten language bundles and complete Simplified Chinese user-guide and hooks documentation.
- Add the built-in `xdotcom` subagent for X.com content workflows.

### Changed
- Sync the community build with the upstream `0.2.110` monorepo line.
- Refresh the project storefront with Hyper branding, real TUI screenshots, badges, and updated Oracle/modes design guidance.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.110
```

## [0.2.109] — 2026-07-22

**Wire-compatible release.** Hyper stamps `x-grok-client-version` from the root
`VERSION` file via `GROK_VERSION` at build time. xAI's API gate rejects clients
below **0.1.202** (HTTP 426). The previous `0.1.0` marketing tag was therefore
unusable against production Grok models (e.g. grok-4.5).

This tag **matches the monorepo lockstep crate version** (`xai-grok-pager` /
`xai-grok-version` / shell at `0.2.109`), which is also above the official
stable line (`grok 0.2.106` at time of release).

### Fixes
- Align release `VERSION` / GitHub tag with monorepo client version so API
  version gates accept the binary.
- Document that Hyper release tags must track the pager lockstep version, not
  an independent `0.1.x` marketing line.

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
# pin:
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.109
```

The earlier `v0.1.0` assets remain on GitHub Releases for historical download
but must not be used against current xAI endpoints.

## [0.1.0] — 2026-07-22

First tagged Hyper community release of the multi-provider Grok Build fork.

> **Superseded for API use.** `x-grok-client-version: 0.1.0` is rejected by
> xAI (min **0.1.202**). Upgrade to **v0.2.109** or later.

### Highlights

- **Binary name `hyper`**, install root `~/.hyper/bin`, shared runtime state under `~/.grok` (compatible with official `grok` config/auth).
- **Multi-provider catalog**: Moonshot, Kimi Code OAuth, ChatGPT Codex OAuth, OpenAI/Anthropic BYOK, Z.AI, Ollama Cloud, and more.
- **Oracle** built-in read-only subagent for deep analysis (pin a strong model via `/agents` or `[subagents.models]`).
- **Community builds** disable the upstream self-updater so Hyper cannot overwrite `~/.grok/bin/grok`.

### Reliability (this release line)

- Keep xAI session bearer off third-party BYOK platforms (including live-only catalog models).
- Route-aware opaque reasoning replay (model + API backend + endpoint identity).
- Catalog-first OAuth identity for Kimi vs Codex (including shared reverse-proxy origins).
- Kimi/Codex sticky permanent-failure cache for revoked refresh tokens (process-local).
- Kimi lock-held refresh total budget capped below the cross-process flock wait.
- Multi-thread blocking resolvers bounded by a 20s operation timeout.
- MiniMax / Fireworks Messages bases normalized to `…/v1` before `/messages` join.
- Leader cleanup recognizes `hyper` and `grok` product processes (Linux argv0, Windows image path, macOS `proc_pidpath`).
- `hyper logout --all` clears xAI + Kimi + Codex OAuth; bare logout hints remaining scopes.
- `hyper --version` / completions brand as `hyper` when built with `community-build` (default).

### Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.109
```

See [README.md](./README.md) and [docs/KNOWN_ISSUES.md](./docs/KNOWN_ISSUES.md).

### Not in this release

- Amp-style **agent modes** (low/medium/high/ultra) — design only (`docs/design-modes.md`).
