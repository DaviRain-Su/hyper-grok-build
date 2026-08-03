//! **Experimental** agent turn path via Hypercore + [`ShellHyperHost`] (P6).
//!
//! **Default off.** Session turns stay on the legacy path until Hypercore is
//! explicitly enabled. This is a containment gate: production traffic should
//! not enter Hypercore without an intentional opt-in.
//!
//! Enable with an explicit truthy value only:
//! - `HYPERCORE_TURN=1` / `true` / `yes` / `on` (preferred)
//! - `GROK_HYPERCORE_TURN=…` — secondary alias, consulted **only** when
//!   `HYPERCORE_TURN` is unset
//!
//! Empty, unknown, `0` / `false` / `no` / `off`, and missing values all
//! **fail closed** to legacy (`false`).
//!
//! Other related flags (unchanged by this gate):
//! - `HYPERCORE_TOOLS=0` — disable tool loop (plain only with `HYPERCORE_PLAIN=1`)
//! - `HYPERCORE_PLAIN=1` — force plain Hypercore (no tools in the request)
//!
//! Once path decision selects Hypercore, non-[`xai_hyper_core::CoreError::Aborted`]
//! errors **propagate** — there is no same-round broad fallback to legacy.
//! Legacy is chosen only by capability / path decision **before** entering Core.
//! Multimodal (image) user content is pre-routed to legacy; conversion is fail-closed.

use super::*;
use std::sync::Arc;
use xai_grok_sampling_types::conversation::ToolCall as SamplingToolCall;
use xai_grok_sampling_types::{ContentPart, ConversationItem, SentCredential};
use xai_hyper_core::{
    CoreConfig, CoreError, CoreEvent, HyperCore, ToolBatchResult, TranscriptItem, TurnRequest,
};
use xai_hyper_host::{
    HostError, HostToolCall, HostToolResult, ToolDefinition as HostToolDefinition,
};

/// Explicit truthy values that enable the Hypercore turn path.
///
/// Pure / fail-closed: only `1` / `true` / `yes` / `on` (case-insensitive,
/// surrounding whitespace ignored) return `true`. Everything else — including
/// empty, unknown tokens, and explicit falsy forms — returns `false`.
fn parse_hypercore_turn_flag(raw: Option<&str>) -> bool {
    let Some(v) = raw else {
        return false;
    };
    let t = v.trim();
    if t.is_empty() {
        return false;
    }
    t == "1"
        || t.eq_ignore_ascii_case("true")
        || t.eq_ignore_ascii_case("yes")
        || t.eq_ignore_ascii_case("on")
}

/// Resolve Hypercore turn enablement from both env candidates (pure).
///
/// Priority: when `HYPERCORE_TURN` is **set** (including empty / unknown), it
/// alone decides and `GROK_HYPERCORE_TURN` is ignored. The grok-prefixed alias
/// is only read when `HYPERCORE_TURN` is unset. Both fail closed (default
/// `false`).
fn hypercore_turn_enabled_from_env_values(
    hypercore_turn: Option<&str>,
    grok_hypercore_turn: Option<&str>,
) -> bool {
    match hypercore_turn {
        Some(v) => parse_hypercore_turn_flag(Some(v)),
        None => parse_hypercore_turn_flag(grok_hypercore_turn),
    }
}

/// Env gate: Hypercore turn path is **opt-in** (default off / fail-closed).
pub(super) fn hypercore_plain_turn_enabled() -> bool {
    hypercore_turn_enabled_from_env_values(
        std::env::var("HYPERCORE_TURN").ok().as_deref(),
        std::env::var("GROK_HYPERCORE_TURN").ok().as_deref(),
    )
}

/// Force plain-text Hypercore (no tools in the request).
pub(super) fn hypercore_plain_forced() -> bool {
    for key in ["HYPERCORE_PLAIN", "GROK_HYPERCORE_PLAIN"] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim();
            if t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes") {
                return true;
            }
        }
    }
    false
}

/// Whether the shell tool loop is ready on Hypercore (P3+).
///
/// Default **true**. Set `HYPERCORE_TOOLS=0` / `false` / `no` to disable.
pub(super) fn hypercore_tool_loop_ready() -> bool {
    for key in ["HYPERCORE_TOOLS", "GROK_HYPERCORE_TOOLS"] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim();
            if t.is_empty() {
                continue;
            }
            if t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("no") {
                return false;
            }
            if t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes") {
                return true;
            }
            return true;
        }
    }
    true
}

/// Why a round uses Hypercore or stays on legacy (for logs / tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HypercorePathDecision {
    /// Enter Hypercore for this round.
    Use,
    /// Hypercore turn gate not explicitly enabled (default / fail-closed).
    DisabledByEnv,
    /// Empty user text — cannot open a core turn.
    EmptyPrompt,
    /// Tools disabled and plain not forced.
    ToolsDisabledNeedsPlain,
    /// Current or historical user message has non-text content (e.g. image).
    UnsupportedMultimodal,
}

impl HypercorePathDecision {
    /// Stable reason string for telemetry / logs.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Use => "hypercore",
            Self::DisabledByEnv => "legacy_env_disabled",
            Self::EmptyPrompt => "legacy_empty_prompt",
            Self::ToolsDisabledNeedsPlain => "legacy_tools_off",
            Self::UnsupportedMultimodal => "legacy_multimodal",
        }
    }

    pub(super) fn uses_hypercore(self) -> bool {
        matches!(self, Self::Use)
    }
}

/// Stable unique Hypercore turn id for an outer-loop round / compact segment.
///
/// Format: `hc:{prompt_id_len}:{prompt_id}:r{round}` with optional `:c{n}` for
/// mid-turn compact continuations. Length-prefix avoids collisions when
/// `prompt_id` itself contains `:r…` / `:c…` suffixes. ACP `prompt_id` /
/// telemetry keep the original value; only the core turn id uses this form.
///
/// Outer rounds use `round` 0, 1, 2, … without reuse. Compact segments append
/// `c1`, `c2`, … Same segment retries reuse the same id.
pub(super) fn hypercore_round_turn_id(
    prompt_id: &str,
    outer_round: u32,
    compact_segment: Option<u32>,
) -> String {
    let base = format!("hc:{}:{prompt_id}:r{outer_round}", prompt_id.len());
    match compact_segment {
        Some(n) if n > 0 => format!("{base}:c{n}"),
        _ => base,
    }
}

/// Error policy after path decision has selected Hypercore.
///
/// Locked for tests: must remain `"propagate"` (never `"legacy_fallback"`).
pub(super) fn hypercore_post_entry_error_policy() -> &'static str {
    "propagate"
}

/// Decide Hypercore vs legacy for one prompt round (text-only gates).
///
/// Empty prompt → legacy. Tool loop ready → Hypercore. Else only
/// `HYPERCORE_PLAIN=1` takes plain Hypercore. `json_schema` is allowed (P4).
///
/// Prefer [`hypercore_path_decision_for_conversation`] when history is available
/// so image / multimodal user rows pre-route to legacy.
pub(super) fn hypercore_path_decision(prompt: &str) -> HypercorePathDecision {
    if !hypercore_plain_turn_enabled() {
        return HypercorePathDecision::DisabledByEnv;
    }
    if prompt.trim().is_empty() {
        return HypercorePathDecision::EmptyPrompt;
    }
    if hypercore_tool_loop_ready() || hypercore_plain_forced() {
        return HypercorePathDecision::Use;
    }
    HypercorePathDecision::ToolsDisabledNeedsPlain
}

/// Path decision including multimodal pre-route from conversation history.
pub(super) fn hypercore_path_decision_for_conversation(
    prompt: &str,
    conversation: &[ConversationItem],
) -> HypercorePathDecision {
    let base = hypercore_path_decision(prompt);
    if base.uses_hypercore() && conversation_has_non_text_user_content(conversation) {
        return HypercorePathDecision::UnsupportedMultimodal;
    }
    base
}

/// Whether this prompt should enter the Hypercore path for a round.
pub(super) fn should_use_hypercore_turn(prompt: &str) -> bool {
    hypercore_path_decision(prompt).uses_hypercore()
}

/// True if any user message has a non-[`ContentPart::Text`] part (e.g. image).
pub(super) fn conversation_has_non_text_user_content(conversation: &[ConversationItem]) -> bool {
    conversation.iter().any(|item| {
        if let ConversationItem::User(u) = item {
            u.content
                .iter()
                .any(|p| !matches!(p, ContentPart::Text { .. }))
        } else {
            false
        }
    })
}

/// Max mid-turn compact→continue restarts inside one Hypercore prompt.
const HYPERCORE_MAX_COMPACT_ROUNDS: u32 = 3;

impl SessionActor {
    /// Run one turn through Hypercore (tools + optional json_schema).
    ///
    /// User message must already be in `chat_state`. Child/subagent sessions
    /// share this path (own `session_id` → independent
    /// `~/.grok/hypercore/<id>/`).
    ///
    /// P5: pre-seed auto-compact, model-switch compact, and mid-turn
    /// preflight overflow → compact → [`HyperCore::continue_turn_with_tools`].
    ///
    /// `core_turn_id_base` is the stable Hypercore turn id for this outer-loop
    /// round (see [`hypercore_round_turn_id`]); compact segments append `:cN`.
    /// ACP `prompt_id` stays separate for telemetry.
    pub(super) async fn run_hypercore_plain_turn(
        self: &std::sync::Arc<Self>,
        prompt_id: &str,
        user_text: &str,
        json_schema: Option<serde_json::Value>,
        core_turn_id_base: &str,
    ) -> Result<TurnOutcome, acp::Error> {
        let session_id = self.session_info.id.0.to_string();
        tracing::info!(
            session_id = %session_id,
            prompt_id = %prompt_id,
            core_turn_id = %core_turn_id_base,
            is_subagent = self.startup_hints.is_subagent,
            parent = ?self.startup_hints.parent_session_id.as_deref(),
            "hypercore turn: begin"
        );

        // P5: model-switch + pre-seed auto-compact (parity with legacy turn start).
        self.maybe_compact_on_model_switch().await?;
        if self.tool_context.task_output_token_budget.is_none()
            && let Some(trigger) = self.check_auto_compact_needed().await
        {
            tracing::info!(
                session_id = %session_id,
                percentage = trigger.percentage,
                "hypercore turn: pre-seed auto-compact"
            );
            if let Err(e) = self.run_compact_only(trigger).await {
                tracing::error!(error = %e, "hypercore pre-seed compact failed");
                if Self::is_auth_compact_error(&e) {
                    return Err(self.surface_compact_auth_failure(e).await);
                }
            }
        }

        // Structured-output strategy (parity with legacy process_conversation_turn).
        let structured_output_validator = json_schema.as_ref().map(|schema| {
            jsonschema::validator_for(schema).map_err(|e| format!("invalid output schema: {e}"))
        });
        let schema_ok = matches!(structured_output_validator, Some(Ok(_)));
        let native_backend = if json_schema.is_some() {
            match self.chat_state_handle.get_sampling_config().await {
                Some(c) => c.api_backend.supports_native_schema(),
                None => {
                    tracing::warn!(
                        "hypercore structured output: no sampling config; using StructuredOutput tool"
                    );
                    false
                }
            }
        } else {
            false
        };
        let structured_output_native = schema_ok && native_backend;
        let structured_output_tool = schema_ok && !native_backend;

        if structured_output_tool {
            self.push_system_reminder(
                "A response schema is required. After any tool use, call the \
                 `StructuredOutput` tool exactly once with your final answer as its \
                 arguments; do not return the answer as text.",
            );
        }

        let tools = self
            .hypercore_prepare_host_tools(structured_output_tool, json_schema.as_ref(), &session_id)
            .await;

        let req_json_schema = if structured_output_native {
            json_schema.clone()
        } else {
            None
        };

        let structured_retries: std::cell::Cell<u32> = std::cell::Cell::new(0);
        let structured_from_tool: std::cell::RefCell<Option<Result<serde_json::Value, String>>> =
            std::cell::RefCell::new(None);

        // Actually-executed tool names across compact segments. Source of truth
        // for final `TurnOutcome::Completed.tools_called` (not Core Ok outcomes,
        // which drop names when a segment Aborts after tools already ran).
        let executed_tools: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut compact_round: u32 = 0;
        let mut is_continuation = false;
        let mut final_outcome: Option<xai_hyper_core::TurnOutcome> = None;
        // Side-channels for Abort handling (not applied into core transcript).
        let mut terminal_abort: Option<TurnOutcome> = None;
        // Same per-incident 401 budget as the legacy turn loop. Hypercore must
        // not unbounded-retry auth failures or skip recovery entirely.
        let mut auth_retry_schedule = AuthRetrySchedule::new();

        loop {
            let host = self.shell_hypercore_host().await;
            let model = host.sampling_config().model.clone();
            let mut core = HyperCore::restore_or_new(
                host,
                session_id.clone(),
                CoreConfig {
                    model,
                    max_messages: 256,
                    max_tool_steps: 64,
                },
            )
            .await
            .map_err(|e| acp::Error::internal_error().data(format!("hypercore restore: {e}")))?;

            let conversation = self.chat_state_handle.get_conversation().await;
            let seeded = if is_continuation {
                // Full history after tools + compact (no user re-append).
                // Tool side-effects already live in chat_state.
                conversation_to_full_seed_items(&conversation).map_err(|e| {
                    acp::Error::internal_error().data(format!("hypercore seed: {e}"))
                })?
            } else {
                conversation_to_seed_items(&conversation, user_text).map_err(|e| {
                    acp::Error::internal_error().data(format!("hypercore seed: {e}"))
                })?
            };
            let completed = seeded.iter().filter(|i| i.role == "assistant").count() as u64;
            core.seed_transcript(seeded, completed);

            // Compact segment 0 = base id; c1/c2 after each compact restart.
            let turn_id = if is_continuation {
                format!("{core_turn_id_base}:c{compact_round}")
            } else {
                core_turn_id_base.to_string()
            };

            tracing::info!(
                session_id = %session_id,
                prompt_id = %prompt_id,
                turn_id = %turn_id,
                continuation = is_continuation,
                compact_round,
                with_tools = tools.as_ref().map(|t| !t.is_empty()).unwrap_or(false),
                structured_native = structured_output_native,
                structured_tool = structured_output_tool,
                "hypercore turn: submit"
            );

            let abort: std::cell::RefCell<Option<TurnOutcome>> = std::cell::RefCell::new(None);
            let compact_restart: std::cell::Cell<bool> = std::cell::Cell::new(false);

            let invoker = |assistant_text: String, calls: Vec<HostToolCall>| {
                let abort = &abort;
                let compact_restart = &compact_restart;
                let structured_retries = &structured_retries;
                let structured_from_tool = &structured_from_tool;
                let structured_output_validator = &structured_output_validator;
                let executed_tools = &executed_tools;
                async move {
                    if abort.borrow().is_some() {
                        return ToolBatchResult::Abort {
                            reason: "turn already aborted".into(),
                        };
                    }

                    if structured_output_tool
                        && let Some(validator) = structured_output_validator.as_ref()
                        && let Some(batch) = self
                            .hypercore_try_structured_output_batch(
                                &calls,
                                &assistant_text,
                                validator,
                                structured_retries,
                                structured_from_tool,
                                abort,
                                compact_restart,
                                executed_tools,
                            )
                            .await
                    {
                        return batch;
                    }

                    let batch_names: Vec<String> = calls.iter().map(|c| c.name.clone()).collect();
                    match self
                        .hypercore_execute_tool_batch(calls, &assistant_text)
                        .await
                    {
                        Ok(results) => {
                            // Record real executions even if we Abort for compact.
                            record_executed_tool_names(
                                &mut executed_tools.borrow_mut(),
                                batch_names,
                            );
                            // P5: post-tool preflight overflow → Abort core
                            // segment (no terminal/snapshot); shell compact
                            // restarts from full chat_state on a new segment id.
                            if self.tool_context.task_output_token_budget.is_none()
                                && self.check_preflight_overflow().await.is_some()
                            {
                                tracing::warn!(
                                    session_id = %self.session_info.id.0,
                                    "hypercore turn: preflight overflow after tools; will compact and continue"
                                );
                                compact_restart.set(true);
                                return ToolBatchResult::Abort {
                                    reason: "preflight overflow; compact restart".into(),
                                };
                            }
                            ToolBatchResult::Continue(results)
                        }
                        Err(terminal) => {
                            *abort.borrow_mut() = Some(terminal);
                            ToolBatchResult::Abort {
                                reason: "tool terminal / cancel".into(),
                            }
                        }
                    }
                }
            };

            let core_result = if is_continuation {
                core.continue_turn_with_tools(
                    turn_id.clone(),
                    user_text.to_string(),
                    tools.clone(),
                    req_json_schema.clone(),
                    invoker,
                )
                .await
            } else {
                core.submit_turn_with_tools(
                    TurnRequest {
                        turn_id: turn_id.clone(),
                        text: user_text.to_string(),
                        json_schema: req_json_schema.clone(),
                        tools: tools.clone(),
                    },
                    invoker,
                )
                .await
            };

            let outcome = match core_result {
                Ok(o) => o,
                Err(CoreError::Aborted(reason)) => {
                    let terminal = abort.into_inner();
                    let wants_compact = compact_restart.get();

                    match (terminal, wants_compact) {
                        (Some(t), true) => {
                            // Fail closed: terminal side-channel wins.
                            tracing::error!(
                                session_id = %session_id,
                                reason = %reason,
                                "hypercore invariant: Abort with both terminal and compact flags; preferring terminal"
                            );
                            terminal_abort = Some(t);
                            break;
                        }
                        (Some(t), false) => {
                            tracing::info!(
                                session_id = %session_id,
                                reason = %reason,
                                "hypercore turn: Abort with terminal side-channel"
                            );
                            terminal_abort = Some(t);
                            break;
                        }
                        (None, true) if compact_round < HYPERCORE_MAX_COMPACT_ROUNDS => {
                            // Aborted segment must not emit buffered assistant
                            // events as committed; tools already dual-wrote to
                            // chat_state. Compact then continue with cN id.
                            compact_round += 1;
                            if let Some(trigger) = self.check_preflight_overflow().await {
                                tracing::info!(
                                    session_id = %session_id,
                                    round = compact_round,
                                    "hypercore turn: mid-turn compact before continue"
                                );
                                if let Err(e) = self.run_compact_only(trigger).await {
                                    tracing::error!(
                                        error = %e,
                                        "hypercore mid-turn compact failed"
                                    );
                                    if Self::is_auth_compact_error(&e) {
                                        return Err(self.surface_compact_auth_failure(e).await);
                                    }
                                    return Err(acp::Error::internal_error()
                                        .data(format!("hypercore mid-turn compact failed: {e}")));
                                }
                            } else if let Some(trigger) = self.check_auto_compact_needed().await
                                && let Err(e) = self.run_compact_only(trigger).await
                            {
                                tracing::error!(
                                    error = %e,
                                    "hypercore mid-turn auto-compact failed"
                                );
                                if Self::is_auth_compact_error(&e) {
                                    return Err(self.surface_compact_auth_failure(e).await);
                                }
                                return Err(acp::Error::internal_error()
                                    .data(format!("hypercore mid-turn auto-compact failed: {e}")));
                            }
                            is_continuation = true;
                            continue;
                        }
                        (None, true) => {
                            return Err(acp::Error::internal_error().data(format!(
                                "hypercore compact restart limit exceeded ({HYPERCORE_MAX_COMPACT_ROUNDS}): {reason}"
                            )));
                        }
                        (None, false) => {
                            return Err(acp::Error::internal_error().data(format!(
                                "hypercore aborted without terminal or compact: {reason}"
                            )));
                        }
                    }
                }
                Err(e) => {
                    // Auth/401: force-refresh + charge the same per-incident
                    // budget as legacy. Never broad-fallback to legacy; never
                    // unbounded-retry outside AuthRetrySchedule.
                    if let Some(acp_err) = self
                        .hypercore_try_auth_recovery(&e, &mut auth_retry_schedule)
                        .await?
                    {
                        return Err(acp_err);
                    }
                    // Recovery succeeded under budget → rebuild host credentials
                    // and resubmit the same segment (same turn_id semantics as
                    // a retry; host opens a fresh stream with the new bearer).
                    continue;
                }
            };

            // Auth recovery is **only** via typed `CoreError::Host(HostError::Auth)`
            // on the Err path above. Core always returns `Err(Host(...))` for
            // stream open/chunk failures (and may also push a display-only
            // `TurnFailed` string event). Never rebuild auth from that string —
            // it loses credential provenance and would double-charge the budget.
            //
            // ACP stream events for this *committed* segment only.
            for ev in &outcome.events {
                match ev {
                    CoreEvent::AssistantDelta { text, .. } if !text.is_empty() => {
                        self.send_update(
                            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                acp::ContentBlock::Text(acp::TextContent::new(text.clone())),
                            )),
                            None,
                        )
                        .await;
                    }
                    CoreEvent::TurnFailed { error, .. } => {
                        // Display-only: stream path already returned Err(Host).
                        // If we somehow see TurnFailed without Err, surface it
                        // as a hard failure — do not string-match for 401.
                        return Err(acp::Error::internal_error().data(error.clone()));
                    }
                    _ => {}
                }
            }

            // Do not extend tools_called from Core outcome — Abort segments
            // after real tools would drop names, and Finish would double-count
            // if we also recorded in the invoker. `executed_tools` is authoritative.

            // Structured-output acceptance uses Finish (commits) and stashes a
            // shell TurnOutcome::Completed on the abort side-channel (already
            // merged with prior executed tool names).
            if let Some(terminal) = abort.into_inner() {
                terminal_abort = Some(terminal);
                break;
            }

            // Dual-write final assistant when not already present.
            let conversation_after = self.chat_state_handle.get_conversation().await;
            if !outcome.assistant_text.is_empty()
                && !trailing_assistant_matches(&conversation_after, &outcome.assistant_text)
            {
                self.chat_state_handle
                    .push_assistant_response(ConversationItem::assistant(
                        outcome.assistant_text.clone(),
                    ));
            }

            // A committed model response ends the open 401 incident.
            auth_retry_schedule.reset_on_success();
            final_outcome = Some(outcome);
            break;
        }

        let tools_called = executed_tools.into_inner();

        if let Some(terminal) = terminal_abort {
            tracing::info!(
                session_id = %session_id,
                tools = tools_called.len(),
                "hypercore turn: finished via tool-loop terminal outcome"
            );
            self.record_turn_model().await;
            // Prefer side-channel tools_called when present (SO path merges
            // prior + StructuredOutput into the Completed variant).
            return Ok(match terminal {
                TurnOutcome::Completed {
                    snapshot,
                    tools_called: side,
                    structured_output,
                    refusal,
                } => TurnOutcome::Completed {
                    snapshot,
                    // Side-channel should already include the full ordered list;
                    // fall back to accumulated if empty but we have names.
                    tools_called: if side.is_empty() && !tools_called.is_empty() {
                        tools_called
                    } else {
                        side
                    },
                    structured_output,
                    refusal,
                },
                other => other,
            });
        }

        let outcome = final_outcome.ok_or_else(|| {
            acp::Error::internal_error().data("hypercore turn produced no outcome")
        })?;

        let structured_output = if structured_output_native {
            structured_output_validator
                .as_ref()
                .map(|v| super::turn::validate_structured_output(v, &outcome.assistant_text))
        } else {
            structured_from_tool.into_inner()
        };

        self.record_turn_model().await;

        tracing::info!(
            session_id = %session_id,
            prompt_id = %prompt_id,
            turn_id = %outcome.turn_id,
            replayed = outcome.replayed,
            bytes = outcome.assistant_text.len(),
            tools = tools_called.len(),
            compact_rounds = compact_round,
            has_structured = structured_output.is_some(),
            is_subagent = self.startup_hints.is_subagent,
            "hypercore turn: committed"
        );

        Ok(TurnOutcome::Completed {
            snapshot: Box::new(None),
            tools_called,
            structured_output,
            refusal: None,
        })
    }

    /// Hypercore auth-error recovery with the **same** per-incident budget as
    /// the legacy turn loop (`AuthRetrySchedule`).
    ///
    /// Returns:
    /// - `Ok(None)` — recovery succeeded under budget; caller should resubmit.
    /// - `Ok(Some(err))` — terminal failure (budget exhausted, non-auth, or
    ///   refresh failed); caller should return the error.
    /// - `Err(err)` — ACP error from budget exhaustion (already formatted).
    ///
    /// Non-auth host errors return `Ok(Some(...))` so the caller propagates
    /// without retrying. Auth failures that recover return `Ok(None)`.
    async fn hypercore_try_auth_recovery(
        self: &Arc<Self>,
        err: &CoreError,
        schedule: &mut AuthRetrySchedule,
    ) -> Result<Option<acp::Error>, acp::Error> {
        // Typed Auth only — no Transport/Message string heuristics.
        let (credential, message) = match err {
            CoreError::Host(HostError::Auth {
                status_code: _,
                message,
                credential,
            }) => {
                let cred = match credential.as_str() {
                    "sent" => SentCredential::Sent,
                    "missing" => SentCredential::Missing,
                    _ => SentCredential::Unknown,
                };
                (cred, message.clone())
            }
            other => {
                return Ok(Some(
                    acp::Error::internal_error().data(format!("hypercore submit_turn: {other}")),
                ));
            }
        };
        if schedule.reset_if_incident_spans_suspend() {
            tracing::info!("hypercore auth 401 retry: incident spanned a suspend; budget reset");
        }

        // Force-refresh the active credential family (platform OAuth or xAI
        // session) via the same paths as legacy `handle_sampling_failure`.
        // Build a synthetic SamplingErrorInfo so we reuse that logic without
        // reopening a model stream outside the budget.
        let synthetic = xai_grok_sampler::SamplingErrorInfo {
            kind: xai_grok_sampler::SamplingErrorKind::Auth,
            status_code: Some(401),
            message: message.clone(),
            is_retryable: true,
            retry_after_secs: None,
            should_retry: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential,
        };
        match self.handle_sampling_failure(synthetic).await {
            Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                credential: recovered_cred,
                store,
            }) => match schedule.on_recovered_401(recovered_cred) {
                AuthRetryDecision::UnchargedResubmit { resubmit } => {
                    tracing::warn!(
                        resubmit,
                        "hypercore auth 401 retry: no credential was sent; resubmitting uncharged"
                    );
                    pace_uncharged_resubmit(store, self.auth_manager.as_ref()).await;
                    Ok(None)
                }
                AuthRetryDecision::Backoff { attempt, delay } => {
                    tracing::warn!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "hypercore auth 401 retry: backing off before resubmit"
                    );
                    tokio::time::sleep(delay).await;
                    Ok(None)
                }
                AuthRetryDecision::Exhausted | AuthRetryDecision::RunawayGuard { .. } => {
                    let (rejections, authenticated) = schedule.incident_counts();
                    let msg = format!(
                        "Hypercore auth retry budget exhausted after {rejections} \
                         post-recovery 401s ({authenticated} provably carried a credential)."
                    );
                    tracing::error!(%msg);
                    Err(acp::Error::internal_error().data(
                        crate::sampling::error::error_data_with_status(msg, Some(401)),
                    ))
                }
            },
            Ok(SamplerFailureRecovery::CompactAndResubmit) => {
                // Unexpected for pure auth; treat as terminal to avoid loops.
                Ok(Some(acp::Error::internal_error().data(format!(
                    "hypercore auth recovery returned compact: {message}"
                ))))
            }
            Err(e) => Ok(Some(e)),
        }
    }

    async fn hypercore_prepare_host_tools(
        &self,
        structured_output_tool: bool,
        json_schema: Option<&serde_json::Value>,
        session_id: &str,
    ) -> Option<Vec<HostToolDefinition>> {
        if hypercore_tool_loop_ready() && !hypercore_plain_forced() {
            let defs = self.prepare_tool_definitions().await;
            let mut host_tools = sampling_tools_to_host(&defs);
            tracing::info!(
                session_id = %session_id,
                tool_count = host_tools.len(),
                "hypercore turn: tools prepared"
            );
            if structured_output_tool && let Some(schema) = json_schema {
                host_tools.push(HostToolDefinition {
                    name: super::turn::STRUCTURED_OUTPUT_TOOL.to_string(),
                    description: "Return your final answer as JSON matching the required schema. \
                         Call this exactly once, at the end."
                        .to_string(),
                    input_schema: schema.clone(),
                });
            }
            Some(host_tools)
        } else if structured_output_tool && let Some(schema) = json_schema {
            Some(vec![HostToolDefinition {
                name: super::turn::STRUCTURED_OUTPUT_TOOL.to_string(),
                description: "Return your final answer as JSON matching the required schema. \
                     Call this exactly once, at the end."
                    .to_string(),
                input_schema: schema.clone(),
            }])
        } else {
            Some(Vec::new())
        }
    }

    /// Handle a batch that includes `StructuredOutput` (tool-based schema path).
    ///
    /// Returns `Some` when the batch was fully handled (continue, finish, or abort).
    /// Mixed SO + real tools: execute real tools only, soft-correct SO (not a
    /// successful StructuredOutput call), check preflight overflow like a normal
    /// batch, and keep Core results order-aligned.
    #[allow(clippy::too_many_arguments)]
    async fn hypercore_try_structured_output_batch(
        &self,
        calls: &[HostToolCall],
        assistant_text: &str,
        validator: &Result<jsonschema::Validator, String>,
        structured_retries: &std::cell::Cell<u32>,
        structured_from_tool: &std::cell::RefCell<Option<Result<serde_json::Value, String>>>,
        abort: &std::cell::RefCell<Option<TurnOutcome>>,
        compact_restart: &std::cell::Cell<bool>,
        executed_tools: &std::cell::RefCell<Vec<String>>,
    ) -> Option<ToolBatchResult> {
        let so_name = super::turn::STRUCTURED_OUTPUT_TOOL;
        let has_so = calls.iter().any(|c| c.name == so_name);
        if !has_so {
            return None;
        }

        // Mixed with real tools: push corrective results for SO calls only;
        // execute real tools; do not count unaccepted SO as a successful call.
        let real: Vec<HostToolCall> = calls
            .iter()
            .filter(|c| c.name != so_name)
            .cloned()
            .collect();
        let so_calls: Vec<&HostToolCall> = calls.iter().filter(|c| c.name == so_name).collect();

        if !real.is_empty() {
            // Dual-write assistant with all calls, then only execute real tools;
            // SO gets a corrective tool_result (not a successful StructuredOutput).
            self.hypercore_push_assistant_tool_calls(calls, assistant_text)
                .await;
            for so in &so_calls {
                self.chat_state_handle
                    .push_tool_result(ConversationItem::tool_result(
                        so.id.clone(),
                        "Call StructuredOutput alone, exactly once, after all other tools finish.",
                    ));
            }
            let real_names: Vec<String> = real.iter().map(|c| c.name.clone()).collect();
            match self.hypercore_execute_tool_batch_prepared(real).await {
                Ok(results) => {
                    // Record real tool names (not the mixed SO correction).
                    record_executed_tool_names(&mut executed_tools.borrow_mut(), real_names);

                    // Rebuild full-order Core results (SO soft rows + real).
                    let mut full = Vec::with_capacity(calls.len());
                    let mut real_i = 0;
                    for c in calls {
                        if c.name == so_name {
                            full.push(HostToolResult {
                                call_id: c.id.clone(),
                                ok: true,
                                content: "Call StructuredOutput alone, exactly once, after all other tools finish.".into(),
                            });
                        } else {
                            full.push(results.get(real_i).cloned().unwrap_or(HostToolResult {
                                call_id: c.id.clone(),
                                ok: false,
                                content: "missing real tool result".into(),
                            }));
                            real_i += 1;
                        }
                    }

                    // Same post-tool preflight as a normal batch.
                    if self.tool_context.task_output_token_budget.is_none()
                        && self.check_preflight_overflow().await.is_some()
                    {
                        tracing::warn!(
                            session_id = %self.session_info.id.0,
                            "hypercore turn: preflight overflow after mixed SO+tools; will compact and continue"
                        );
                        compact_restart.set(true);
                        return Some(ToolBatchResult::Abort {
                            reason: "preflight overflow after mixed tools; compact restart".into(),
                        });
                    }
                    return Some(ToolBatchResult::Continue(full));
                }
                Err(terminal) => {
                    *abort.borrow_mut() = Some(terminal);
                    return Some(ToolBatchResult::Abort {
                        reason: "tool terminal / cancel during structured-output batch".into(),
                    });
                }
            }
        }

        // Sole StructuredOutput call(s) — take the first.
        let so = so_calls[0];
        self.hypercore_push_assistant_tool_calls(calls, assistant_text)
            .await;

        let args_raw = match &so.arguments {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let validated = super::turn::validate_structured_output(validator, &args_raw);
        let retries = structured_retries.get();

        // Validation retry: do not record StructuredOutput as a successful call.
        if let Err(err) = &validated
            && retries < super::turn::STRUCTURED_OUTPUT_MAX_RETRIES
        {
            structured_retries.set(retries + 1);
            let msg = format!("{err}\nFix the arguments and call StructuredOutput again.");
            self.chat_state_handle
                .push_tool_result(ConversationItem::tool_result(so.id.clone(), msg.clone()));
            return Some(ToolBatchResult::Continue(vec![HostToolResult {
                call_id: so.id.clone(),
                ok: false,
                content: msg,
            }]));
        }

        let content = match &validated {
            Ok(_) => "Structured output accepted.".to_string(),
            Err(err) => err.clone(),
        };
        self.chat_state_handle
            .push_tool_result(ConversationItem::tool_result(
                so.id.clone(),
                content.clone(),
            ));
        *structured_from_tool.borrow_mut() = Some(validated.clone());

        // Final acceptance only: prior real tools + StructuredOutput.
        // Validation retries never reach here without accepting/ending.
        let tools_called = merge_tools_with_structured_output(&executed_tools.borrow(), so_name);
        *executed_tools.borrow_mut() = tools_called.clone();
        *abort.borrow_mut() = Some(TurnOutcome::Completed {
            snapshot: Box::new(None),
            tools_called,
            structured_output: Some(validated),
            refusal: None,
        });

        Some(ToolBatchResult::Finish(vec![HostToolResult {
            call_id: so.id.clone(),
            ok: true,
            content,
        }]))
    }

    async fn hypercore_push_assistant_tool_calls(
        &self,
        calls: &[HostToolCall],
        assistant_text: &str,
    ) {
        let sampling_calls: Vec<SamplingToolCall> = calls
            .iter()
            .map(|c| {
                let args = match &c.arguments {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                SamplingToolCall {
                    id: Arc::<str>::from(c.id.as_str()),
                    name: c.name.clone(),
                    arguments: Arc::<str>::from(args.as_str()),
                }
            })
            .collect();
        let mut assistant_item = ConversationItem::assistant_tool_calls(sampling_calls);
        if let ConversationItem::Assistant(ref mut a) = assistant_item
            && !assistant_text.is_empty()
        {
            a.content = Arc::<str>::from(assistant_text);
        }
        self.record_assistant_response(assistant_item).await;
    }

    /// Execute one model-step's tool batch via the legacy pipeline.
    async fn hypercore_execute_tool_batch(
        &self,
        calls: Vec<HostToolCall>,
        assistant_text: &str,
    ) -> Result<Vec<HostToolResult>, TurnOutcome> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        self.hypercore_push_assistant_tool_calls(&calls, assistant_text)
            .await;
        self.hypercore_execute_tool_batch_prepared(calls).await
    }

    async fn hypercore_execute_tool_batch_prepared(
        &self,
        calls: Vec<HostToolCall>,
    ) -> Result<Vec<HostToolResult>, TurnOutcome> {
        let tool_call_responses: Vec<crate::sampling::types::ToolCallResponse> = calls
            .iter()
            .map(|c| {
                let args = match &c.arguments {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                crate::sampling::types::ToolCallResponse {
                    id: c.id.clone(),
                    kind: "function".to_string(),
                    function: crate::sampling::types::ToolCallFunction {
                        name: c.name.clone(),
                        arguments: args,
                    },
                }
            })
            .collect();

        self.emit_event(crate::session::events::Event::PhaseChanged {
            phase: crate::session::events::Phase::ToolExecution,
        });
        self.observability_bridge
            .emit(
                xai_tool_protocol::session_event::SessionEvent::PhaseChanged {
                    phase: xai_tool_protocol::session_event::SessionPhase::ToolExecution,
                },
            )
            .await;

        let loop_result = self
            .execute_tool_calls(tool_call_responses)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "hypercore tool batch execute failed");
                TurnOutcome::Cancelled {
                    category: Some(
                        crate::session::events::CancellationCategory::PermissionCancelled,
                    ),
                    context: Some(serde_json::json!({ "error": e.to_string() })),
                }
            })?;

        match loop_result {
            ToolLoop::PermissionReject { tool_name, reason } => {
                return Err(TurnOutcome::Cancelled {
                    category: Some(
                        crate::session::events::CancellationCategory::PermissionRejected,
                    ),
                    context: Some(serde_json::json!({
                        "tool_name": tool_name,
                        "reason": reason,
                    })),
                });
            }
            ToolLoop::Cancelled => {
                return Err(TurnOutcome::Cancelled {
                    category: Some(
                        crate::session::events::CancellationCategory::PermissionCancelled,
                    ),
                    context: None,
                });
            }
            ToolLoop::FollowupMessage(msg) => {
                self.add_followup_message_as_user_turn(&msg).await;
            }
            ToolLoop::Continue
            | ToolLoop::HookDenied { .. }
            | ToolLoop::NonExistingTool
            | ToolLoop::ToolParsingError => {}
        }

        Ok(self.collect_host_tool_results(&calls).await)
    }

    /// Pull tool results for `calls` from the tail of `chat_state`.
    async fn collect_host_tool_results(&self, calls: &[HostToolCall]) -> Vec<HostToolResult> {
        let conv = self.chat_state_handle.get_conversation().await;
        let mut out = Vec::with_capacity(calls.len());
        for call in calls {
            let found = conv.iter().rev().find_map(|item| {
                if let ConversationItem::ToolResult(t) = item
                    && t.tool_call_id == call.id
                {
                    return Some(HostToolResult {
                        call_id: call.id.clone(),
                        ok: !t.is_error,
                        content: t.content.to_string(),
                    });
                }
                None
            });
            out.push(found.unwrap_or_else(|| HostToolResult {
                call_id: call.id.clone(),
                ok: false,
                content: format!(
                    "tool `{}` completed without a chat_state result row",
                    call.name
                ),
            }));
        }
        out
    }
}

fn sampling_tools_to_host(
    defs: &[crate::sampling::types::ToolDefinition],
) -> Vec<HostToolDefinition> {
    defs.iter()
        .map(|d| HostToolDefinition {
            name: d.function.name.clone(),
            description: d.function.description.clone().unwrap_or_default(),
            input_schema: d.function.parameters.clone(),
        })
        .collect()
}

/// Append actually-executed tool names in call order (preserves duplicates).
///
/// Used across compact segments and StructuredOutput acceptance so
/// `TurnOutcome::Completed.tools_called` does not depend on Core Ok outcomes
/// (which drop names when a segment Aborts after tools already ran).
fn record_executed_tool_names(acc: &mut Vec<String>, names: impl IntoIterator<Item = String>) {
    acc.extend(names);
}

/// Merge prior real-tool names with a final accepted StructuredOutput call.
///
/// Pure helper for tests / SO terminal path. Does not record SO validation retries
/// or mixed-batch SO corrections.
fn merge_tools_with_structured_output(
    prior: &[String],
    structured_output_tool: &str,
) -> Vec<String> {
    let mut out = prior.to_vec();
    out.push(structured_output_tool.to_string());
    out
}

/// Contract for mixed SO + real tools after successful real execution:
/// record real names only, then either Continue or Abort(compact) — never treat
/// the unaccepted SO correction as a successful StructuredOutput call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MixedSoPostToolsAction {
    /// Apply soft SO results + real results and sample again.
    Continue,
    /// Overflow: keep real tool names, set compact, Abort without Core commit.
    AbortCompact,
}

fn mixed_so_post_tools_action(preflight_overflow: bool) -> MixedSoPostToolsAction {
    if preflight_overflow {
        MixedSoPostToolsAction::AbortCompact
    } else {
        MixedSoPostToolsAction::Continue
    }
}

/// Fail-closed conversion of chat_state → hypercore seed (drops matching trailing user).
fn conversation_to_seed_items(
    conversation: &[ConversationItem],
    current_user_text: &str,
) -> Result<Vec<TranscriptItem>, String> {
    let mut items = conversation_to_full_seed_items(conversation)?;

    if let Some(last) = items.last()
        && last.role == "user"
        && last.content.trim() == current_user_text.trim()
    {
        items.pop();
    }
    Ok(items)
}

/// Full chat_state → hypercore seed (used after mid-turn compact).
fn conversation_to_full_seed_items(
    conversation: &[ConversationItem],
) -> Result<Vec<TranscriptItem>, String> {
    let mut items = Vec::with_capacity(conversation.len());
    for item in conversation {
        if let Some(t) = conversation_item_to_transcript(item)? {
            items.push(t);
        }
    }
    Ok(items)
}

/// Convert one conversation row. Returns `Ok(None)` for intentionally skipped
/// empty rows; `Err` for unsupported multimodal (never silent strip).
fn conversation_item_to_transcript(
    item: &ConversationItem,
) -> Result<Option<TranscriptItem>, String> {
    use xai_hyper_core::TranscriptToolCall;

    match item {
        ConversationItem::System(s) => {
            Ok(Some(TranscriptItem::text("system", s.content.to_string())))
        }
        ConversationItem::User(u) => {
            let text = user_parts_to_text(&u.content)?;
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(TranscriptItem::text("user", text)))
            }
        }
        ConversationItem::Assistant(a) => {
            let text = a.content.to_string();
            let tool_calls: Vec<TranscriptToolCall> = a
                .tool_calls
                .iter()
                .map(|tc| TranscriptToolCall {
                    id: tc.id.to_string(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.to_string(),
                })
                .collect();
            if text.is_empty() && tool_calls.is_empty() {
                Ok(None)
            } else {
                Ok(Some(TranscriptItem {
                    role: "assistant".into(),
                    content: text,
                    tool_calls,
                    tool_call_id: None,
                }))
            }
        }
        ConversationItem::ToolResult(t) => Ok(Some(TranscriptItem {
            role: "tool".into(),
            content: t.content.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: Some(t.tool_call_id.clone()),
        })),
        // Other item kinds are intentionally omitted from core transcript.
        _ => Ok(None),
    }
}

/// Join user text parts. Fail closed on any non-text part (images, etc.).
fn user_parts_to_text(parts: &[ContentPart]) -> Result<String, String> {
    let mut out = String::new();
    for p in parts {
        match p {
            ContentPart::Text { text } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            ContentPart::Image { .. } => {
                return Err(
                    "unsupported multimodal content: image parts cannot be converted for Hypercore"
                        .into(),
                );
            }
        }
    }
    Ok(out)
}

fn trailing_assistant_matches(conversation: &[ConversationItem], assistant_text: &str) -> bool {
    match conversation.last() {
        Some(ConversationItem::Assistant(a)) => a.content.as_ref() == assistant_text,
        _ => false,
    }
}

/// Pure outer-loop decision for Hypercore auth recovery (mirrors the shell
/// turn's `match core_result / AuthRetrySchedule / continue` without SessionActor).
///
/// Returns whether the loop should resubmit (`true`) or terminate (`false`),
/// and counts each simulated submit attempt.
#[cfg(test)]
fn hypercore_auth_submit_loop(
    max_attempts: u32,
    credential: xai_grok_sampling_types::SentCredential,
    recover_ok: bool,
) -> (u32, bool) {
    use xai_grok_sampling_types::SentCredential;
    let mut schedule = AuthRetrySchedule::new();
    let mut submits = 0u32;
    for _ in 0..max_attempts {
        submits += 1;
        // Simulate typed HostError::Auth from open_model_stream / next_chunk.
        let err = CoreError::Host(HostError::Auth {
            status_code: Some(401),
            message: "token rejected".into(),
            credential: match credential {
                SentCredential::Sent => "sent".into(),
                SentCredential::Missing => "missing".into(),
                _ => "unknown".into(),
            },
        });
        // Only typed Auth is recoverable (same match as hypercore_try_auth_recovery).
        let is_typed_auth = matches!(&err, CoreError::Host(HostError::Auth { .. }));
        if !is_typed_auth {
            return (submits, false);
        }
        if !recover_ok {
            // Refresh failed → terminate without further resubmit.
            return (submits, false);
        }
        match schedule.on_recovered_401(credential) {
            AuthRetryDecision::UnchargedResubmit { .. } | AuthRetryDecision::Backoff { .. } => {
                // One resubmit per decision — no same-round extra submit.
                continue;
            }
            AuthRetryDecision::Exhausted | AuthRetryDecision::RunawayGuard { .. } => {
                return (submits, false);
            }
        }
    }
    (submits, true)
}

/// Display-only TurnFailed must never enter the auth schedule (no resubmit).
#[cfg(test)]
fn hypercore_turn_failed_does_not_resubmit(error: &str) -> bool {
    // Production path: TurnFailed in Ok(outcome) → hard Err, no AuthRetrySchedule.
    let _ = error;
    false
}

mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn seed_drops_matching_trailing_user() {
        let conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
        ];
        let seeded = conversation_to_seed_items(&conv, "hello").expect("text only");
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].role, "system");
    }

    #[test]
    fn seed_keeps_history_when_user_differs() {
        let conv = vec![
            ConversationItem::user("old"),
            ConversationItem::assistant("reply"),
            ConversationItem::user("new"),
        ];
        let seeded = conversation_to_seed_items(&conv, "new").expect("text only");
        assert_eq!(seeded.len(), 2);
        assert_eq!(seeded[0].content, "old");
        assert_eq!(seeded[1].content, "reply");
    }

    #[test]
    fn seed_rejects_image_user_parts_fail_closed() {
        let conv = vec![ConversationItem::user_with_parts(vec![
            ContentPart::Text {
                text: Arc::<str>::from("caption"),
            },
            ContentPart::Image {
                url: Arc::<str>::from("https://example.com/x.png"),
            },
        ])];
        let err = conversation_to_seed_items(&conv, "caption").unwrap_err();
        assert!(
            err.contains("unsupported multimodal") || err.contains("image"),
            "unexpected err: {err}"
        );
        // Must not silently drop the image and keep text-only.
        assert!(conversation_has_non_text_user_content(&conv));
    }

    #[test]
    fn image_only_user_is_detected_and_conversion_fails() {
        let conv = vec![ConversationItem::user_with_parts(vec![
            ContentPart::Image {
                url: Arc::<str>::from("data:image/png;base64,xx"),
            },
        ])];
        assert!(conversation_has_non_text_user_content(&conv));
        let err = conversation_to_full_seed_items(&conv).unwrap_err();
        assert!(err.contains("image") || err.contains("multimodal"));
    }

    #[test]
    fn hypercore_round_turn_ids_are_unique_and_stable() {
        let a = hypercore_round_turn_id("prompt", 0, None);
        let b = hypercore_round_turn_id("prompt", 1, None);
        let c = hypercore_round_turn_id("prompt", 0, Some(1));
        let d = hypercore_round_turn_id("prompt", 0, Some(2));
        // Length-prefix: prompt_id "p:r0" must not collide with "p" round 0.
        let e = hypercore_round_turn_id("p:r0", 0, None);
        let f = hypercore_round_turn_id("p", 0, None);
        assert_eq!(a, "hc:6:prompt:r0");
        assert_eq!(b, "hc:6:prompt:r1");
        assert_eq!(c, "hc:6:prompt:r0:c1");
        assert_eq!(d, "hc:6:prompt:r0:c2");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(e, f);
        // Retry same segment reuses id.
        assert_eq!(a, hypercore_round_turn_id("prompt", 0, None));
        assert_eq!(c, hypercore_round_turn_id("prompt", 0, Some(1)));
    }

    #[test]
    fn post_entry_error_policy_is_propagate_not_legacy_fallback() {
        assert_eq!(hypercore_post_entry_error_policy(), "propagate");
        assert_ne!(hypercore_post_entry_error_policy(), "legacy_fallback");
    }

    #[test]
    fn path_decision_multimodal_reason_stable() {
        assert_eq!(
            HypercorePathDecision::UnsupportedMultimodal.as_str(),
            "legacy_multimodal"
        );
        assert!(!HypercorePathDecision::UnsupportedMultimodal.uses_hypercore());
    }

    #[test]
    fn parse_hypercore_turn_flag_explicit_enable_only() {
        for on in ["1", "true", "TRUE", "Yes", "on", "ON", "  true  "] {
            assert!(
                parse_hypercore_turn_flag(Some(on)),
                "expected enable for {on:?}"
            );
        }
        for off in [
            "0", "false", "FALSE", "no", "off", "OFF", "", "   ", "maybe", "2", "enable",
            "enabled", "y", "t",
        ] {
            assert!(
                !parse_hypercore_turn_flag(Some(off)),
                "expected fail-closed for {off:?}"
            );
        }
        assert!(!parse_hypercore_turn_flag(None));
    }

    #[test]
    fn hypercore_turn_default_off_when_both_unset() {
        assert!(!hypercore_turn_enabled_from_env_values(None, None));
    }

    #[test]
    fn hypercore_turn_prefers_primary_over_alias() {
        // Primary set (even to false) wins; alias is ignored.
        assert!(!hypercore_turn_enabled_from_env_values(
            Some("0"),
            Some("1")
        ));
        assert!(!hypercore_turn_enabled_from_env_values(
            Some("false"),
            Some("true")
        ));
        assert!(!hypercore_turn_enabled_from_env_values(Some(""), Some("1")));
        assert!(!hypercore_turn_enabled_from_env_values(
            Some("maybe"),
            Some("1")
        ));
        // Primary explicit enable wins even if alias is off.
        assert!(hypercore_turn_enabled_from_env_values(Some("1"), Some("0")));
        assert!(hypercore_turn_enabled_from_env_values(
            Some("true"),
            Some("false")
        ));
    }

    #[test]
    fn hypercore_turn_alias_used_only_when_primary_unset() {
        assert!(hypercore_turn_enabled_from_env_values(None, Some("1")));
        assert!(hypercore_turn_enabled_from_env_values(None, Some("yes")));
        assert!(!hypercore_turn_enabled_from_env_values(None, Some("0")));
        assert!(!hypercore_turn_enabled_from_env_values(None, Some("")));
        assert!(!hypercore_turn_enabled_from_env_values(
            None,
            Some("unknown")
        ));
    }

    #[test]
    fn env_gate_smoke_and_empty_prompt() {
        // Live env is process-global; only assert pure empty-prompt behavior here.
        let _ = hypercore_plain_turn_enabled();
        let _ = hypercore_tool_loop_ready();
        assert!(!should_use_hypercore_turn(""));
        assert!(!should_use_hypercore_turn("   "));
    }

    #[test]
    fn path_decision_empty_prompt_never_uses_hypercore() {
        // Gate is checked before empty-prompt classification. With the
        // fail-closed default (off), empty prompts surface as DisabledByEnv;
        // when the process has an explicit opt-in they surface as EmptyPrompt.
        // Either way Hypercore must not run.
        let d_empty = hypercore_path_decision("");
        let d_ws = hypercore_path_decision("  \n");
        assert!(!d_empty.uses_hypercore());
        assert!(!d_ws.uses_hypercore());
        if hypercore_plain_turn_enabled() {
            assert_eq!(d_empty, HypercorePathDecision::EmptyPrompt);
            assert_eq!(d_ws, HypercorePathDecision::EmptyPrompt);
        } else {
            assert_eq!(d_empty, HypercorePathDecision::DisabledByEnv);
            assert_eq!(d_ws, HypercorePathDecision::DisabledByEnv);
        }
    }

    #[test]
    fn path_decision_reasons_are_stable() {
        assert_eq!(HypercorePathDecision::Use.as_str(), "hypercore");
        assert_eq!(
            HypercorePathDecision::DisabledByEnv.as_str(),
            "legacy_env_disabled"
        );
        assert_eq!(
            HypercorePathDecision::EmptyPrompt.as_str(),
            "legacy_empty_prompt"
        );
        assert_eq!(
            HypercorePathDecision::ToolsDisabledNeedsPlain.as_str(),
            "legacy_tools_off"
        );
        assert_eq!(
            HypercorePathDecision::UnsupportedMultimodal.as_str(),
            "legacy_multimodal"
        );
    }

    #[test]
    fn full_seed_keeps_trailing_user() {
        let conv = vec![
            ConversationItem::user("u"),
            ConversationItem::assistant("a"),
            ConversationItem::user("tail"),
        ];
        let full = conversation_to_full_seed_items(&conv).expect("text only");
        assert_eq!(full.len(), 3);
        assert_eq!(full[2].role, "user");
        assert_eq!(full[2].content, "tail");
    }

    #[test]
    fn path_decision_for_conversation_routes_images_to_legacy() {
        // Only assert multimodal override when base decision would be Use.
        // Without live env opt-in, base is DisabledByEnv and multimodal is
        // not reached — still must detect content for the gate.
        let with_image = vec![ConversationItem::user_with_parts(vec![
            ContentPart::Text {
                text: Arc::<str>::from("look"),
            },
            ContentPart::Image {
                url: Arc::<str>::from("https://example.com/i.png"),
            },
        ])];
        let d = hypercore_path_decision_for_conversation("look", &with_image);
        assert!(!d.uses_hypercore());
        if hypercore_plain_turn_enabled()
            && (hypercore_tool_loop_ready() || hypercore_plain_forced())
        {
            assert_eq!(d, HypercorePathDecision::UnsupportedMultimodal);
        }
        let plain = vec![ConversationItem::user("look")];
        let d_plain = hypercore_path_decision_for_conversation("look", &plain);
        // Multimodal gate must not fire on pure text.
        assert_ne!(d_plain, HypercorePathDecision::UnsupportedMultimodal);
    }

    #[test]
    fn sampling_tools_convert() {
        let defs = vec![crate::sampling::types::ToolDefinition::function(
            "read_file",
            Some("Read a file"),
            serde_json::json!({"type": "object"}),
        )];
        let host = sampling_tools_to_host(&defs);
        assert_eq!(host.len(), 1);
        assert_eq!(host[0].name, "read_file");
    }

    #[test]
    fn record_executed_tool_names_preserves_order_and_duplicates() {
        let mut acc = Vec::new();
        record_executed_tool_names(&mut acc, ["read_file".into(), "bash".into()]);
        // Compact abort then more tools — append, do not replace.
        record_executed_tool_names(&mut acc, ["bash".into(), "read_file".into()]);
        assert_eq!(
            acc,
            vec![
                "read_file".to_string(),
                "bash".to_string(),
                "bash".to_string(),
                "read_file".to_string(),
            ]
        );
    }

    #[test]
    fn merge_tools_with_structured_output_appends_so_only_on_accept() {
        let prior = vec!["read_file".to_string(), "bash".to_string()];
        let merged = merge_tools_with_structured_output(&prior, "StructuredOutput");
        assert_eq!(
            merged,
            vec![
                "read_file".to_string(),
                "bash".to_string(),
                "StructuredOutput".to_string(),
            ]
        );
        // Retries / empty prior: only SO when nothing ran before.
        assert_eq!(
            merge_tools_with_structured_output(&[], "StructuredOutput"),
            vec!["StructuredOutput".to_string()]
        );
    }

    /// Outer submit loop + AuthRetrySchedule: Missing does not charge until
    /// runaway (50 uncharged resubmits, 51st attempt terminates). Each
    /// decision yields exactly one resubmit — no same-round double submit.
    #[test]
    fn hypercore_submit_loop_missing_budget() {
        use xai_grok_sampling_types::SentCredential;
        // 50 uncharged + 1 that hits RunawayGuard = 51 submits, terminated.
        let (submits, still_running) =
            hypercore_auth_submit_loop(100, SentCredential::Missing, true);
        assert!(!still_running);
        assert_eq!(
            submits,
            AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS + 1,
            "50 uncharged resubmits then terminate on 51st"
        );
    }

    /// Sent credentials: initial + MAX_RETRIES backoffs then Exhausted.
    /// MAX_RETRIES delays ⇒ MAX_RETRIES successful resubmit decisions, then
    /// the next 401 exhausts (total submits = MAX_RETRIES + 1).
    #[test]
    fn hypercore_submit_loop_sent_budget() {
        use xai_grok_sampling_types::SentCredential;
        let (submits, still_running) = hypercore_auth_submit_loop(100, SentCredential::Sent, true);
        assert!(!still_running);
        assert_eq!(
            submits,
            AuthRetrySchedule::MAX_RETRIES + 1,
            "3 charged backoffs then exhaust on 4th submit"
        );
    }

    /// Transport 401 strings and TurnFailed display must not recover.
    #[test]
    fn hypercore_auth_recovery_typed_only_no_string_bridge() {
        let transport = CoreError::Host(HostError::Transport("HTTP 401 Unauthorized".into()));
        assert!(!matches!(
            &transport,
            CoreError::Host(HostError::Auth { .. })
        ));
        assert!(!matches!(
            &transport,
            CoreError::Host(h) if h.is_auth_error()
        ));
        let typed = CoreError::Host(HostError::Auth {
            status_code: Some(401),
            message: "token rejected".into(),
            credential: "missing".into(),
        });
        assert!(matches!(
            &typed,
            CoreError::Host(h) if h.is_auth_error()
        ));
        assert!(!hypercore_turn_failed_does_not_resubmit("auth: 401"));
    }

    /// Refresh failure: single submit, no resubmit.
    #[test]
    fn hypercore_submit_loop_refresh_fail_stops_immediately() {
        use xai_grok_sampling_types::SentCredential;
        let (submits, still_running) = hypercore_auth_submit_loop(10, SentCredential::Sent, false);
        assert!(!still_running);
        assert_eq!(submits, 1);
    }

    #[test]
    fn mixed_so_overflow_contract_records_real_only_then_abort_compact() {
        // Contract: after real tools succeed in a mixed batch, overflow →
        // AbortCompact (not Finish/Continue), and only real names are recorded.
        let mut acc = Vec::new();
        let real_names = vec!["read_file".to_string(), "bash".to_string()];
        record_executed_tool_names(&mut acc, real_names.clone());
        assert_eq!(
            mixed_so_post_tools_action(true),
            MixedSoPostToolsAction::AbortCompact
        );
        assert_eq!(
            mixed_so_post_tools_action(false),
            MixedSoPostToolsAction::Continue
        );
        // Unaccepted SO must not appear.
        assert!(!acc.iter().any(|n| n == "StructuredOutput"));
        assert_eq!(acc, real_names);
    }
}
