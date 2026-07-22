# xai-grok-test-support

Shared test infrastructure for the grok-build crates: mock inference server,
SSE wire-format generators, ACP stdio clients, headless
runner, and sandboxed process env. Consumed by `xai-grok-shell` integration
tests, `xai-grok-pager-pty-harness` (`ContentController`), and `xai-grok-sampler`
tests.

> **Freshness rule:** update this README in the same PR that changes `src/` —
> reviewers should treat a `src/` diff without a README diff as incomplete.

How-to-test discovery lives with the pager PTY harness crate
(`xai-grok-pager-pty-harness`). This file is the API reference for the shared
test-support surface.

## Module map

| Module | What it provides |
|--------|------------------|
| `inference_override` | Typed request matching and response precedence shared by all inference routes: endpoint + foreground/auxiliary classification, named expectation state, overlapping-duplicate fingerprint replay, per-expectation barriers, compatibility FIFO dispatch, auth rejection, and compatibility completion-gate policy. The module is crate-private; only `InferenceEndpoint`, `InferenceRequestMatcher`, and `InferenceExpectation` are re-exported. |
| `mock_server` | `MockInferenceServer` — `/v1/chat/completions`, `/v1/responses`, `/v1/messages`, `/v1/models`, `/v1/settings`, `/v1/user` on `127.0.0.1:0`. `/v1/models` entries are `MockModelEntry` (re-exported as `MockModel` for PTY tests): `new(id)` / `with_agent_type(id, ty)` plus chainable `with_api_backend`, `with_supports_backend_search(bool)` → `supportsBackendSearch`, `with_supports_reasoning_effort(bool)` → `supportsReasoningEffort`, `with_reasoning_effort(&str)` → `reasoningEffort`, `with_reasoning_efforts(Vec<Value>)` → `reasoningEfforts` (raw option tables/bare strings), all emitted top-level as `parse_remote_model_value` reads them. Inference precedence is **matched expectation > compatibility FIFO > required-auth > echo/fixed mode**. Register a uniquely named response with `expect_response(name, InferenceRequestMatcher::{foreground,auxiliary}(InferenceEndpoint::{ChatCompletions,Responses,Messages}), ScriptedResponse)` or `expect_response_blocked`; duplicate names fail at registration and requests atomically claim one matching expectation. Overlapping duplicate requests replay by a deterministic fingerprint of endpoint, request kind, non-empty `x-grok-req-id`, and serialized request body; tool-result follow-ups reuse the turn id but change the body, so they claim the next expectation. Production exposes no explicit HTTP attempt/model-call identity, so completed sequential retries are intentionally not inferred from timing: after the active shared call settles, an identical request claims the next expectation. A foreground request normally carries a non-empty `x-grok-turn-idx`; a non-turn non-empty `x-grok-req-id` is auxiliary even if it uses tools, and empty headers fall through to the 2+-tool compatibility heuristic. The returned `InferenceExpectation` has watch-backed `wait_received`, `wait_blocked`, `release`, `wait_satisfied`, `is_satisfied`, and `assert_satisfied` lifecycle operations. `release` only opens the barrier; response-body/stream-owned RAII publishes `Satisfied` only when the primary crosses terminal and every active overlapping copy settles. Primary cancellation cleans up without satisfaction or replay retention, and dropping a handle safely releases blocked work. Echo (default) streams `Echo: <last user message>` and fixed mode via `set_response(text)` reconstructs bytes exactly. Constructors (`start`, `start_with_models`, `start_with_required_auth`) return `anyhow::Result`. Settings are 404-until-set (`set_settings(impl Serialize)`, `preset_allow_access()` for the `{"allow_access": true}` gate); scripted `/v1/settings` one-shots (`enqueue_response`) take precedence over the steady-state value (stale-snapshot tests). `/v1/user` serves a minimal `UserInfo` whose `subscriptionTier` is controlled by `set_user_subscription_tier(Option<&str>)` (`None` = free); its log entries keep the query string (e.g. `/v1/user?include=subscription`) so subscription-check cadence is countable. Request log: `requests()` (`LogEntry` with body, `authorization`, full POST headers + `header(name)` accessor), `request_bodies()`, `request_count()`, `has_chat_completion_request()` / `has_responses_request()` (exact, per endpoint), `messages_request_count()`, `last_system_prompt()`, `request_log_summary()`. **Storage:** `POST /v1/storage` with flippable 401 (`set_storage_unauthorized`); accepted uploads via `storage_uploads()` → `StorageUpload { path, size, body, authorization }` (`body` retained up to 256 KiB, empty above; `authorization` is the raw header). Runtime knobs: `set_models`, `set_messages_stop_reason`. Shuts down on drop. |
| `scripted` | Data-only response bodies (no axum types in the public surface): `SseEvent { event, data }` (`::data`, `::with_event`), `ScriptedBody::{Json, Sse, Raw}` (`Raw` = byte-controllable malformed SSE), `ScriptedResponse { status, headers, body }` (`::sse`, `::json`, `::text`). Prefer request-matched expectations for inference calls; `enqueue_response(path, response)` remains a compatibility FIFO per path and is still used for non-inference one-shots such as `/v1/settings`. Scripted SSE honors `set_chunk_delay`; matched JSON, raw, SSE, and even empty SSE bodies all honor per-expectation completion barriers. The compatibility `hold_agent_completions` gate also covers foreground scripted SSE on all three inference endpoints. Validation is eager — bad status/header panics at registration. |
| `sse` | The three wire formats as event-list builders: `chat_completion_events` / `responses_api_events` / `messages_api_events(text, model, stop_reason)` (echo-style, whitespace-collapsing) plus byte-exact axum variants `chat_completion_events_exact` / `responses_api_events_exact` and matching public scripted variants `chat_completion_script_exact` / `responses_api_script_exact` (messages is single-delta, byte-exact by construction). The exact/echo split is load-bearing — see the in-module byte-exactness tests. Also the scripted-scenario builders returning `SseEvent`s (for `ScriptedResponse::sse`): `responses_api_reasoning_only_events(reasoning, model)` — reasoning summary deltas completing with a `reasoning` item but no message/output-text, so the shell collector classifies the turn `EmptyReason::ReasoningOnly` (the model-doomloop trigger); `responses_api_reasoning_and_text_events(reasoning, text, model)` — reasoning deltas then a normal text answer (the ordinary reasoning-model turn); `responses_api_reasoning_then_tool_call_events(reasoning, call_id, name, arguments, model)` + its Chat Completions twin `chat_completions_reasoning_then_tool_call_events(...)` — reasoning deltas then one tool call (the think-then-call turn whose tool call finishes the thought and keeps the turn non-empty); the doom-loop check trio: `responses_api_doom_loop_check_events(triggers, reasoning, model)` — a doomed reasoning-only turn with NAMED `response.doom_loop_check` frames re-sent per cumulative prefix of `triggers` plus the terminal `doom_loop_check.triggers` copy on `response.completed`, `responses_api_doom_loop_terminal_only_events(triggers, reasoning, text, model)` — a normal answer whose terminal response alone carries the field, and `responses_api_with_doom_loop_frame(check_frame_data, reasoning, text, model)` — splices one named check frame with a caller-supplied payload (byte-exact `xai_grok_sampling_types::doom_loop::SAMPLE_CHECK_EVENT_DATA{,_CUMULATIVE}` fixtures or malformed variants) into an ordinary turn. |
| `acp_client` | `GrokStdioClient` — drives `grok agent stdio` over real pipes through `agent-client-protocol`: spawn variants (`spawn`, `spawn_with_home`, `spawn_with_home_and_env`, `spawn_with_home_env_and_args`), initialize/authenticate, session create/load, prompt, `*_with_timeout` wrappers, captured text + stderr. `RawStdioClient` — raw-wire sibling for bytes the typed `ClientSideConnection` can never produce (escaped-slash methods `"session\/prompt"`, string UUID ids — the Xcode/Foundation shape): `send_line` writes a line verbatim; `response_for_id` matches the response by exact string id (the match IS the id-echo assertion), skips notifications, auto-refuses agent→client requests with `-32601`, and panics on timeout with skipped-traffic diagnostics (count + last lines; `0 other messages` = true silence). Both spawn through one hermetic `spawn_agent_process` (sandbox env + debug-log kill-list exists once) atop `process::spawn_piped_with_stderr_capture` (crate-internal `process` module: pipes, `kill_on_drop`, stderr drain — also used by `leader::LeaderStdioClient`). |
| `headless` | `run_headless(server, args, cwd)` / `run_headless_with_env(server, args, cwd, env)` (extra env applied after the defaults, so it overrides them) / `run_headless_with_cmd(cmd)` → `HeadlessResult { status, stdout, stderr, timed_out }` (60s cap), `assert_headless_success`, `assert_no_crashes` (panic/SIGSEGV/linker patterns), `stderr_tail`. |
| `env` | `grok_binary()` (`GROK_BINARY` env → `CARGO_BIN_EXE` → local debug build of `xai-grok-pager`), `git_workdir()` (temp git repo, forces full libgit2 init), `test_env_cmd_tokio(cmd, mock_url, home)` (sandboxed HOME **and GROK_HOME** — Windows resolves `~` via USERPROFILE, so HOME alone doesn't sandbox — + mock endpoints + telemetry kill-switches). |
| `leader` | Unix-only `LeaderStdioClient` (`grok agent --leader stdio`, `env_clear`-hermetic, sandboxed `GROK_LEADER_SOCKET`; `spawn_with_binary` runs an explicit binary for version-skew lanes, per-role resolution via `leader_binary()` / `client_binary()` honoring `GROK_BINARY_LEADER` / `GROK_BINARY_CLIENT`) + lock-file helpers: `leader_lock_path`, `read_leader_pid`, `pid_alive`, `wait_for_live_leader`, `wait_for_new_leader`, `wait_for_replay_notifications`, `leader_log`. |
| `uds_proxy` | Unix-only `UdsProxy` — frame-aware (4-byte BE length prefix) man-in-the-middle for leader IPC sockets. `UdsProxy::spawn(proxy_path, upstream_path, FaultPlan)`; `FaultPlan { direction, drop_frame, sever_mid_frame, delay, duplicate_frame }` (1-based frame index, per connection per direction); runtime `FaultHandle::sever_now()` + `forwarded(direction)` counters; frame bodies capped at 64 MiB (leader-transport parity — corrupt lengths error instead of allocating). Zero production changes: point `LeaderClient::connect` / `GROK_LEADER_SOCKET` at the proxy path. |

## Consumer matrix

| Consumer | Uses | Notes |
|----------|------|-------|
| `xai-grok-shell` `tests/*.rs` | Everything | Direct imports (`use xai_grok_test_support::*` or module paths); no local shim. |
| `xai-grok-pager-pty-harness` `src/content.rs` | `MockInferenceServer`, `MockModelEntry` (re-exported as `MockModel`) | `ContentController` wraps the server and **keeps the HOME-sandbox `TempDir` + `env_for_pager()` harness-side**; presets `allow_access` + a fixed default response at construction. |
| `xai-grok-sampler` `tests/test_actor.rs` | `sse` generators | Happy-path payloads only; the actor keeps its own router for stall/conditional fixtures. |

## Adding a capability

**A response mode** (`mock_server.rs`): extend the private `ResponseMode` enum
+ add the setter; wire the new arm into all **three** inference handlers (the
match in each route); scripted responses must still win. Extend the in-crate
tests: an HTTP round-trip for the new mode plus a leg in
`scripted_responses_serve_fifo_per_path_then_fall_back` proving fallback
reaches it. The echo pinning test (`echo_mode_echoes_last_user_message`) must
pass unmodified — echo bytes are frozen.

**A wire format** (`sse.rs`): add the echo-style builder and, if clients
reconstruct text byte-for-byte, an `_exact` variant built on a delta fn;
then add the serving arm in `mock_server` (all modes) and a route if it is a
new endpoint. Extend the byte-exactness pins
(`deltas_reconstruct_multiline_response_byte_for_byte`,
`deltas_preserve_runs_of_whitespace`) — they are the contract that fenced
code blocks (mermaid) survive streaming. A **scripted-scenario builder** (one
that models a specific completion the echo/fixed modes can't express, e.g.
`responses_api_reasoning_only_events`) instead returns `SseEvent`s for
`ScriptedResponse::sse`, needs no `mock_server` mode wiring, and ships with an
in-module shape test asserting its event shape.

**An expectation matcher** (`inference_override.rs`): keep the public matcher
typed and narrow. Claim under the single expectation-state mutex before
serving, replay only overlapping active duplicates by model-call fingerprint,
and add focused tests for auxiliary non-consumption, concurrent one-claim
behavior, lifecycle barriers, and useful unsatisfied diagnostics. Expectations
and compatibility scripts must remain ahead of required auth and fallback modes.

**A scripted-body kind** (`scripted.rs`): new `ScriptedBody` variant + render
arm in `into_response_paced` + eager checks in `validate` if the data can be
invalid. Add an in-crate test asserting client-visible bytes (the `Raw`
byte-exactness test is the template), exercise terminal gating for the new
body, and keep `scripted_response_takes_precedence_over_required_auth` green —
precedence is part of the contract.
