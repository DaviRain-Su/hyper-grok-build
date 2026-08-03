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
//! On Hypercore failure the outer loop falls back to
//! `process_conversation_turn` for that round only. Legacy is **retained** as
//! a safety net (not deleted in P6).

use super::*;
use std::sync::Arc;
use xai_grok_sampling_types::conversation::ToolCall as SamplingToolCall;
use xai_grok_sampling_types::{ContentPart, ConversationItem};
use xai_hyper_core::{
    CoreConfig, CoreEvent, HyperCore, ToolBatchResult, TranscriptItem, TurnRequest,
};
use xai_hyper_host::{HostToolCall, HostToolResult, ToolDefinition as HostToolDefinition};

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
}

impl HypercorePathDecision {
    /// Stable reason string for telemetry / logs.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Use => "hypercore",
            Self::DisabledByEnv => "legacy_env_disabled",
            Self::EmptyPrompt => "legacy_empty_prompt",
            Self::ToolsDisabledNeedsPlain => "legacy_tools_off",
        }
    }

    pub(super) fn uses_hypercore(self) -> bool {
        matches!(self, Self::Use)
    }
}

/// Decide Hypercore vs legacy for one prompt round.
///
/// Empty prompt → legacy. Tool loop ready → Hypercore. Else only
/// `HYPERCORE_PLAIN=1` takes plain Hypercore. `json_schema` is allowed (P4).
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

/// Whether this prompt should enter the Hypercore path for a round.
pub(super) fn should_use_hypercore_turn(prompt: &str) -> bool {
    hypercore_path_decision(prompt).uses_hypercore()
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
    pub(super) async fn run_hypercore_plain_turn(
        self: &std::sync::Arc<Self>,
        prompt_id: &str,
        user_text: &str,
        json_schema: Option<serde_json::Value>,
    ) -> Result<TurnOutcome, acp::Error> {
        let session_id = self.session_info.id.0.to_string();
        tracing::info!(
            session_id = %session_id,
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

        let mut accumulated_tools: Vec<String> = Vec::new();
        let mut compact_round: u32 = 0;
        let mut is_continuation = false;
        let mut final_outcome: Option<xai_hyper_core::TurnOutcome> = None;
        let mut terminal_abort: Option<TurnOutcome> = None;

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
                conversation_to_full_seed_items(&conversation)
            } else {
                conversation_to_seed_items(&conversation, user_text)
            };
            let completed = seeded.iter().filter(|i| i.role == "assistant").count() as u64;
            core.seed_transcript(seeded, completed);

            let turn_id = if is_continuation {
                format!("{prompt_id}-c{compact_round}")
            } else {
                prompt_id.to_string()
            };

            tracing::info!(
                session_id = %session_id,
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
                async move {
                    if abort.borrow().is_some() {
                        return ToolBatchResult::Finish(
                            calls
                                .into_iter()
                                .map(|c| HostToolResult {
                                    call_id: c.id,
                                    ok: false,
                                    content: "turn aborted".into(),
                                })
                                .collect(),
                        );
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
                            )
                            .await
                    {
                        return batch;
                    }

                    match self
                        .hypercore_execute_tool_batch(calls, &assistant_text)
                        .await
                    {
                        Ok(results) => {
                            // P5: post-tool preflight overflow → compact + continue.
                            if self.tool_context.task_output_token_budget.is_none()
                                && self.check_preflight_overflow().await.is_some()
                            {
                                tracing::warn!(
                                    session_id = %self.session_info.id.0,
                                    "hypercore turn: preflight overflow after tools; will compact and continue"
                                );
                                compact_restart.set(true);
                                return ToolBatchResult::Finish(results);
                            }
                            ToolBatchResult::Continue(results)
                        }
                        Err(terminal) => {
                            *abort.borrow_mut() = Some(terminal);
                            ToolBatchResult::Finish(vec![])
                        }
                    }
                }
            };

            let outcome = if is_continuation {
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
            }
            .map_err(|e| {
                acp::Error::internal_error().data(format!("hypercore submit_turn: {e}"))
            })?;

            // ACP stream events for this segment.
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
                        return Err(acp::Error::internal_error().data(error.clone()));
                    }
                    _ => {}
                }
            }

            accumulated_tools.extend(outcome.tools_called.iter().map(|t| t.name.clone()));

            if let Some(terminal) = abort.into_inner() {
                terminal_abort = Some(terminal);
                break;
            }

            if compact_restart.get() && compact_round < HYPERCORE_MAX_COMPACT_ROUNDS {
                compact_round += 1;
                if let Some(trigger) = self.check_preflight_overflow().await {
                    tracing::info!(
                        session_id = %session_id,
                        round = compact_round,
                        "hypercore turn: mid-turn compact before continue"
                    );
                    if let Err(e) = self.run_compact_only(trigger).await {
                        tracing::error!(error = %e, "hypercore mid-turn compact failed");
                        if Self::is_auth_compact_error(&e) {
                            return Err(self.surface_compact_auth_failure(e).await);
                        }
                        // Fall through to return partial outcome if compact fails non-auth.
                        final_outcome = Some(outcome);
                        break;
                    }
                } else if let Some(trigger) = self.check_auto_compact_needed().await {
                    let _ = self.run_compact_only(trigger).await;
                }
                is_continuation = true;
                continue;
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

            final_outcome = Some(outcome);
            break;
        }

        if let Some(terminal) = terminal_abort {
            tracing::info!(
                session_id = %session_id,
                "hypercore turn: finished via tool-loop terminal outcome"
            );
            self.record_turn_model().await;
            return Ok(terminal);
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
            turn_id = %outcome.turn_id,
            replayed = outcome.replayed,
            bytes = outcome.assistant_text.len(),
            tools = accumulated_tools.len(),
            compact_rounds = compact_round,
            has_structured = structured_output.is_some(),
            is_subagent = self.startup_hints.is_subagent,
            "hypercore turn: committed"
        );

        Ok(TurnOutcome::Completed {
            snapshot: Box::new(None),
            tools_called: accumulated_tools,
            structured_output,
            refusal: None,
        })
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
    /// Returns `Some` when the batch was fully handled (continue or finish).
    /// Returns `None` when StructuredOutput is mixed with real tools → strip and
    /// let the real tools run via normal execute.
    async fn hypercore_try_structured_output_batch(
        &self,
        calls: &[HostToolCall],
        assistant_text: &str,
        validator: &Result<jsonschema::Validator, String>,
        structured_retries: &std::cell::Cell<u32>,
        structured_from_tool: &std::cell::RefCell<Option<Result<serde_json::Value, String>>>,
        abort: &std::cell::RefCell<Option<TurnOutcome>>,
    ) -> Option<ToolBatchResult> {
        let so_name = super::turn::STRUCTURED_OUTPUT_TOOL;
        let has_so = calls.iter().any(|c| c.name == so_name);
        if !has_so {
            return None;
        }

        // Mixed with real tools: push corrective results for SO calls only,
        // drop them from the batch by returning None so caller runs real tools
        // — but we must not execute SO. Handle: if any non-SO tools, return
        // Continue after feeding SO co-emit errors and executing real tools only.
        let real: Vec<HostToolCall> = calls
            .iter()
            .filter(|c| c.name != so_name)
            .cloned()
            .collect();
        let so_calls: Vec<&HostToolCall> = calls.iter().filter(|c| c.name == so_name).collect();

        if !real.is_empty() {
            // Dual-write assistant with all calls, then only execute real tools;
            // SO gets a corrective tool_result.
            self.hypercore_push_assistant_tool_calls(calls, assistant_text)
                .await;
            for so in &so_calls {
                self.chat_state_handle
                    .push_tool_result(ConversationItem::tool_result(
                        so.id.clone(),
                        "Call StructuredOutput alone, exactly once, after all other tools finish.",
                    ));
            }
            match self.hypercore_execute_tool_batch_prepared(real).await {
                Ok(mut results) => {
                    // Prepend SO soft results so Core alignment still works if
                    // we returned full order — better rebuild full order:
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
                    let _ = &mut results;
                    return Some(ToolBatchResult::Continue(full));
                }
                Err(terminal) => {
                    *abort.borrow_mut() = Some(terminal);
                    return Some(ToolBatchResult::Finish(vec![]));
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

        let tools_called = vec![so_name.to_string()];
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

fn conversation_to_seed_items(
    conversation: &[ConversationItem],
    current_user_text: &str,
) -> Vec<TranscriptItem> {
    let mut items = conversation_to_full_seed_items(conversation);

    if let Some(last) = items.last()
        && last.role == "user"
        && last.content.trim() == current_user_text.trim()
    {
        items.pop();
    }
    items
}

/// Full chat_state → hypercore seed (used after mid-turn compact).
fn conversation_to_full_seed_items(conversation: &[ConversationItem]) -> Vec<TranscriptItem> {
    conversation
        .iter()
        .filter_map(conversation_item_to_transcript)
        .collect()
}

fn conversation_item_to_transcript(item: &ConversationItem) -> Option<TranscriptItem> {
    use xai_hyper_core::TranscriptToolCall;

    match item {
        ConversationItem::System(s) => Some(TranscriptItem::text("system", s.content.to_string())),
        ConversationItem::User(u) => {
            let text = user_parts_to_text(&u.content);
            if text.is_empty() {
                None
            } else {
                Some(TranscriptItem::text("user", text))
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
                None
            } else {
                Some(TranscriptItem {
                    role: "assistant".into(),
                    content: text,
                    tool_calls,
                    tool_call_id: None,
                })
            }
        }
        ConversationItem::ToolResult(t) => Some(TranscriptItem {
            role: "tool".into(),
            content: t.content.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: Some(t.tool_call_id.clone()),
        }),
        _ => None,
    }
}

fn user_parts_to_text(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for p in parts {
        if let ContentPart::Text { text } = p {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

fn trailing_assistant_matches(conversation: &[ConversationItem], assistant_text: &str) -> bool {
    match conversation.last() {
        Some(ConversationItem::Assistant(a)) => a.content.as_ref() == assistant_text,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_drops_matching_trailing_user() {
        let conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
        ];
        let seeded = conversation_to_seed_items(&conv, "hello");
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
        let seeded = conversation_to_seed_items(&conv, "new");
        assert_eq!(seeded.len(), 2);
        assert_eq!(seeded[0].content, "old");
        assert_eq!(seeded[1].content, "reply");
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
    }

    #[test]
    fn full_seed_keeps_trailing_user() {
        let conv = vec![
            ConversationItem::user("u"),
            ConversationItem::assistant("a"),
            ConversationItem::user("tail"),
        ];
        let full = conversation_to_full_seed_items(&conv);
        assert_eq!(full.len(), 3);
        assert_eq!(full[2].role, "user");
        assert_eq!(full[2].content, "tail");
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
}
