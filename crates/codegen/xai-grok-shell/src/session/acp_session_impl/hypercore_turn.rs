//! Feature-flagged turn path via Hypercore + [`ShellHyperHost`].
//!
//! **Default on** (`HYPERCORE_TURN`). P3: shell tool loop. P4: `json_schema`
//! structured output (native + StructuredOutput tool) and shell outer loop
//! (goal / stop_gate) around each Hypercore round.
//!
//! - `HYPERCORE_TURN=0` — force legacy always
//! - `HYPERCORE_TOOLS=0` — disable tool loop (plain only with `HYPERCORE_PLAIN=1`)
//! - `HYPERCORE_PLAIN=1` — force plain Hypercore (no tools in the request)
//!
//! On Hypercore failure the outer loop falls back to legacy for that round.

use super::*;
use std::sync::Arc;
use xai_grok_sampling_types::conversation::ToolCall as SamplingToolCall;
use xai_grok_sampling_types::{ContentPart, ConversationItem};
use xai_hyper_core::{
    CoreConfig, CoreEvent, HyperCore, ToolBatchResult, TranscriptItem, TurnRequest,
};
use xai_hyper_host::{HostToolCall, HostToolResult, ToolDefinition as HostToolDefinition};

/// Env gate: prefer Hypercore when the path is capable.
pub(super) fn hypercore_plain_turn_enabled() -> bool {
    for key in ["HYPERCORE_TURN", "GROK_HYPERCORE_TURN"] {
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

/// Whether this prompt should enter the Hypercore path for a round.
///
/// Empty prompt → no. Tool loop ready → yes. Else only `HYPERCORE_PLAIN=1`.
/// `json_schema` is allowed (P4).
pub(super) fn should_use_hypercore_turn(prompt: &str) -> bool {
    if !hypercore_plain_turn_enabled() {
        return false;
    }
    if prompt.trim().is_empty() {
        return false;
    }
    if hypercore_tool_loop_ready() {
        return true;
    }
    hypercore_plain_forced()
}

impl SessionActor {
    /// Run one turn through Hypercore (tools + optional json_schema).
    ///
    /// User message must already be in `chat_state`.
    pub(super) async fn run_hypercore_plain_turn(
        &self,
        prompt_id: &str,
        user_text: &str,
        json_schema: Option<serde_json::Value>,
    ) -> Result<TurnOutcome, acp::Error> {
        let session_id = self.session_info.id.0.to_string();
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
        .map_err(|e| {
            acp::Error::internal_error().data(format!("hypercore restore: {e}"))
        })?;

        let conversation = self.chat_state_handle.get_conversation().await;
        let seeded = conversation_to_seed_items(&conversation, user_text);
        let completed = seeded
            .iter()
            .filter(|i| i.role == "assistant")
            .count() as u64;
        core.seed_transcript(seeded, completed);

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

        let mut tools = if hypercore_tool_loop_ready() && !hypercore_plain_forced() {
            let defs = self.prepare_tool_definitions().await;
            let mut host_tools = sampling_tools_to_host(&defs);
            tracing::info!(
                session_id = %session_id,
                tool_count = host_tools.len(),
                "hypercore turn: tools prepared"
            );
            if structured_output_tool && let Some(schema) = json_schema.clone() {
                host_tools.push(HostToolDefinition {
                    name: super::turn::STRUCTURED_OUTPUT_TOOL.to_string(),
                    description: "Return your final answer as JSON matching the required schema. \
                         Call this exactly once, at the end."
                        .to_string(),
                    input_schema: schema,
                });
            }
            Some(host_tools)
        } else if structured_output_tool && let Some(schema) = json_schema.clone() {
            // Plain path but still need StructuredOutput for non-native backends.
            Some(vec![HostToolDefinition {
                name: super::turn::STRUCTURED_OUTPUT_TOOL.to_string(),
                description: "Return your final answer as JSON matching the required schema. \
                     Call this exactly once, at the end."
                    .to_string(),
                input_schema: schema,
            }])
        } else {
            Some(Vec::new())
        };

        // When tools are empty and we only need native schema, keep tools empty.
        let _ = &mut tools;

        let turn_id = prompt_id.to_string();
        tracing::info!(
            session_id = %session_id,
            turn_id = %turn_id,
            with_tools = tools.as_ref().map(|t| !t.is_empty()).unwrap_or(false),
            structured_native = structured_output_native,
            structured_tool = structured_output_tool,
            "hypercore turn: submit"
        );

        // Terminal outcomes (cancel / structured complete override).
        let abort: std::cell::RefCell<Option<TurnOutcome>> = std::cell::RefCell::new(None);
        let structured_retries: std::cell::Cell<u32> = std::cell::Cell::new(0);
        // Captured structured output from StructuredOutput tool path.
        let structured_from_tool: std::cell::RefCell<Option<Result<serde_json::Value, String>>> =
            std::cell::RefCell::new(None);

        let req_json_schema = if structured_output_native {
            json_schema.clone()
        } else {
            None
        };

        let outcome = core
            .submit_turn_with_tools(
                TurnRequest {
                    turn_id: turn_id.clone(),
                    text: user_text.to_string(),
                    json_schema: req_json_schema,
                    tools,
                },
                |assistant_text, calls| {
                    let abort = &abort;
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

                        // StructuredOutput tool intercept (P4 non-native backends).
                        if structured_output_tool
                            && let Some(validator) = structured_output_validator.as_ref()
                        {
                            if let Some(batch) = self
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
                        }

                        match self
                            .hypercore_execute_tool_batch(calls, &assistant_text)
                            .await
                        {
                            Ok(results) => ToolBatchResult::Continue(results),
                            Err(terminal) => {
                                *abort.borrow_mut() = Some(terminal);
                                ToolBatchResult::Finish(vec![])
                            }
                        }
                    }
                },
            )
            .await
            .map_err(|e| {
                acp::Error::internal_error().data(format!("hypercore submit_turn: {e}"))
            })?;

        if let Some(terminal) = abort.into_inner() {
            tracing::info!(
                session_id = %session_id,
                turn_id = %turn_id,
                "hypercore turn: finished via tool-loop terminal outcome"
            );
            return Ok(terminal);
        }

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

        let conversation_after = self.chat_state_handle.get_conversation().await;
        if !outcome.assistant_text.is_empty()
            && !trailing_assistant_matches(&conversation_after, &outcome.assistant_text)
        {
            self.chat_state_handle
                .push_assistant_response(ConversationItem::assistant(
                    outcome.assistant_text.clone(),
                ));
        }

        let tools_called: Vec<String> = outcome
            .tools_called
            .iter()
            .map(|t| t.name.clone())
            .collect();

        // Native schema: validate final assistant text.
        let structured_output = if structured_output_native {
            structured_output_validator
                .as_ref()
                .map(|v| super::turn::validate_structured_output(v, &outcome.assistant_text))
        } else {
            structured_from_tool.into_inner()
        };

        tracing::info!(
            session_id = %session_id,
            turn_id = %turn_id,
            replayed = outcome.replayed,
            bytes = outcome.assistant_text.len(),
            tools = tools_called.len(),
            has_structured = structured_output.is_some(),
            "hypercore turn: committed"
        );

        Ok(TurnOutcome::Completed {
            snapshot: Box::new(None),
            tools_called,
            structured_output,
            refusal: None,
        })
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
            match self
                .hypercore_execute_tool_batch_prepared(real)
                .await
            {
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
            .push_tool_result(ConversationItem::tool_result(so.id.clone(), content.clone()));
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
    let mut items: Vec<TranscriptItem> = conversation
        .iter()
        .filter_map(conversation_item_to_transcript)
        .collect();

    if let Some(last) = items.last()
        && last.role == "user"
        && last.content.trim() == current_user_text.trim()
    {
        items.pop();
    }
    items
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
    fn env_gate_parses() {
        let _ = hypercore_plain_turn_enabled();
        let _ = hypercore_tool_loop_ready();
        let _ = should_use_hypercore_turn("hi");
        assert!(!should_use_hypercore_turn(""));
        assert!(!should_use_hypercore_turn("   "));
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
