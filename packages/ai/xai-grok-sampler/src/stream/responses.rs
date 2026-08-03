//! Layer-2 stream transform for the OpenAI Responses API.
//!
//! Consumes a raw [`ResponsesStreamItem`] stream and produces
//! [`SamplingEvent`]s. Pure: no I/O, no shell coupling.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};

use xai_grok_sampling_types::{
    ConversationItem, ConversationResponse, ResponseModelMetadata, SamplingError, StopReason,
    TokenUsage, rs,
};

use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::{RequestId, ResponsesStreamItem};

type BackendToolOutcome = Result<Option<serde_json::Value>, String>;
type OutputItemIdentity = (String, String);

fn output_item_identity(item: &rs::OutputItem) -> Option<OutputItemIdentity> {
    let value = serde_json::to_value(item).ok()?;
    let kind = value.get("type")?.as_str()?;
    let stable_id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| {
            value
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
        })?;
    Some((kind.to_string(), stable_id.to_string()))
}

fn merge_completed_output_items(
    terminal_output: &mut Vec<rs::OutputItem>,
    completed_output_items: Vec<rs::OutputItem>,
) -> usize {
    let mut added = 0;
    for item in completed_output_items {
        let identity = output_item_identity(&item);
        let already_present = match identity {
            Some(ref identity) => terminal_output
                .iter()
                .any(|existing| output_item_identity(existing).as_ref() == Some(identity)),
            None => terminal_output.iter().any(|existing| existing == &item),
        };
        if !already_present {
            terminal_output.push(item);
            added += 1;
        }
    }
    added
}

fn backend_tool_outcome(item: &rs::OutputItem) -> Option<(String, String, BackendToolOutcome)> {
    match item {
        rs::OutputItem::WebSearchCall(call) => {
            let outcome = if call.status == rs::WebSearchToolCallStatus::Failed {
                Err("web search failed".to_string())
            } else {
                Ok(serde_json::to_value(call).ok())
            };
            Some((call.id.clone(), "web_search".to_string(), outcome))
        }
        rs::OutputItem::CustomToolCall(call) => Some((
            call.id.clone(),
            "x_search".to_string(),
            Ok(serde_json::to_value(call).ok()),
        )),
        rs::OutputItem::CodeInterpreterCall(call) => Some((
            call.id.clone(),
            "code_interpreter".to_string(),
            Ok(serde_json::to_value(call).ok()),
        )),
        _ => None,
    }
}

fn start_backend_tool(
    active: &mut BTreeMap<String, String>,
    terminal: &BTreeSet<String>,
    request_id: &RequestId,
    call_id: String,
    name: &str,
) -> Option<SamplingEvent> {
    if terminal.contains(&call_id) || active.contains_key(&call_id) {
        return None;
    }
    active.insert(call_id.clone(), name.to_string());
    Some(SamplingEvent::BackendToolCallStarted {
        request_id: request_id.clone(),
        call_id,
        name: name.to_string(),
    })
}

fn finish_backend_tool(
    active: &mut BTreeMap<String, String>,
    terminal: &mut BTreeSet<String>,
    request_id: &RequestId,
    call_id: String,
    name: String,
    outcome: BackendToolOutcome,
) -> Vec<SamplingEvent> {
    if terminal.contains(&call_id) {
        return Vec::new();
    }
    let mut events = Vec::new();
    if let Some(started) = start_backend_tool(active, terminal, request_id, call_id.clone(), &name)
    {
        events.push(started);
    }
    active.remove(&call_id);
    terminal.insert(call_id.clone());
    events.push(match outcome {
        Ok(result) => SamplingEvent::BackendToolCallCompleted {
            request_id: request_id.clone(),
            call_id,
            name,
            result,
        },
        Err(error) => SamplingEvent::BackendToolCallFailed {
            request_id: request_id.clone(),
            call_id,
            name,
            error,
        },
    });
    events
}

fn fail_active_backend_tools(
    active: &mut BTreeMap<String, String>,
    terminal: &mut BTreeSet<String>,
    request_id: &RequestId,
    error: &str,
) -> Vec<SamplingEvent> {
    std::mem::take(active)
        .into_iter()
        .map(|(call_id, name)| {
            terminal.insert(call_id.clone());
            SamplingEvent::BackendToolCallFailed {
                request_id: request_id.clone(),
                call_id,
                name,
                error: error.to_string(),
            }
        })
        .collect()
}

/// Returns whether a Responses API event reflects real model progress
/// rather than a liveness-only heartbeat / status transition.
pub(crate) fn responses_event_has_meaningful_content(event: &rs::ResponseStreamEvent) -> bool {
    use rs::ResponseStreamEvent;

    match event {
        ResponseStreamEvent::ResponseCreated(_)
        | ResponseStreamEvent::ResponseInProgress(_)
        | ResponseStreamEvent::ResponseQueued(_) => false,
        ResponseStreamEvent::ResponseOutputTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseOutputTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseRefusalDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseRefusalDone(event) => !event.refusal.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDone(event) => {
            !event.arguments.is_empty() || event.name.as_ref().is_some_and(|name| !name.is_empty())
        }
        ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningSummaryTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDone(event) => !event.arguments.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDone(event) => !event.code.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDone(event) => !event.input.is_empty(),
        ResponseStreamEvent::ResponseFailed(event) => {
            !event.response.output.is_empty()
                || event
                    .response
                    .usage
                    .as_ref()
                    .is_some_and(|usage| usage.output_tokens > 0)
        }
        ResponseStreamEvent::ResponseCompleted(_)
        | ResponseStreamEvent::ResponseIncomplete(_)
        | ResponseStreamEvent::ResponseOutputItemAdded(_)
        | ResponseStreamEvent::ResponseOutputItemDone(_)
        | ResponseStreamEvent::ResponseContentPartAdded(_)
        | ResponseStreamEvent::ResponseContentPartDone(_)
        | ResponseStreamEvent::ResponseFileSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseFileSearchCallSearching(_)
        | ResponseStreamEvent::ResponseFileSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseWebSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseWebSearchCallSearching(_)
        | ResponseStreamEvent::ResponseWebSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartAdded(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartDone(_)
        | ResponseStreamEvent::ResponseImageGenerationCallCompleted(_)
        | ResponseStreamEvent::ResponseImageGenerationCallGenerating(_)
        | ResponseStreamEvent::ResponseImageGenerationCallInProgress(_)
        | ResponseStreamEvent::ResponseImageGenerationCallPartialImage(_)
        | ResponseStreamEvent::ResponseMCPCallCompleted(_)
        | ResponseStreamEvent::ResponseMCPCallFailed(_)
        | ResponseStreamEvent::ResponseMCPCallInProgress(_)
        | ResponseStreamEvent::ResponseMCPListToolsCompleted(_)
        | ResponseStreamEvent::ResponseMCPListToolsFailed(_)
        | ResponseStreamEvent::ResponseMCPListToolsInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInterpreting(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallCompleted(_)
        | ResponseStreamEvent::ResponseOutputTextAnnotationAdded(_)
        | ResponseStreamEvent::ResponseError(_) => true,
    }
}

pub(crate) fn responses_event_may_have_output(event: &rs::ResponseStreamEvent) -> bool {
    !matches!(event, rs::ResponseStreamEvent::ResponseError(_))
        && responses_event_has_meaningful_content(event)
}

/// Transform a raw Responses API event stream into a stream of
/// [`SamplingEvent`]s.
///
/// Yields exactly one terminal event ([`SamplingEvent::Completed`] or
/// [`SamplingEvent::Failed`]) per request. Server-side `ResponseFailed`
/// and `ResponseError` events are translated to
/// `SamplingError::Api { status: 500, .. }` so the actor's retry loop
/// treats them as retryable.
///
/// [`ResponsesStreamItem::Heartbeat`] frames (keepalive, unknown
/// forward-compat event types) carry no model output but prove the server is
/// alive: they reset the idle detector like codex-rs's frame-level timeout
/// does. Without this, a long heartbeat-only phase (server-side tool
/// execution on ChatGPT Codex) would be misclassified as an idle stall.
///
/// `doom_loop` is the collector returned alongside `raw_stream` by
/// `SamplingClient::conversation_stream_responses`; any signals the SSE
/// decoder recorded are drained onto the final `ConversationResponse`.
/// `None` (check disabled) leaves the response untouched.
pub fn stream_responses<'a>(
    raw_stream: BoxStream<'a, Result<ResponsesStreamItem, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    stream_responses_tracked(
        raw_stream,
        model_metadata,
        request_id,
        idle_timeout,
        doom_loop,
        Arc::new(AtomicBool::new(false)),
    )
}

pub(crate) fn stream_responses_tracked<'a>(
    raw_stream: BoxStream<'a, Result<ResponsesStreamItem, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
    output_observed: Arc<AtomicBool>,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    async_stream::stream! {
        use rs::{ResponseStreamEvent, Status};

        let stream_start = Instant::now();
        let mut chunk_timestamps: Vec<Instant> = Vec::new();

        yield SamplingEvent::StreamStarted {
            request_id: request_id.clone(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        if let Some(metadata) = model_metadata {
            yield SamplingEvent::ModelMetadata {
                request_id: request_id.clone(),
                metadata,
            };
        }

        let mut final_response: Option<rs::Response> = None;
        let mut chunk_index: u64 = 0;
        let mut message_chunk_count: u64 = 0;
        let mut first_token_emitted = false;
        let mut reasoning_acc = String::new();
        // Visible assistant text streamed via `output_text.delta`. Used when
        // ChatGPT Codex leaves `response.completed.output` empty (content only
        // appears on `response.output_item.done` / deltas).
        let mut text_acc = String::new();
        let mut last_content_chunk_at = Instant::now();

        // Maps Responses API `output_index` to our tool-only `tool_index`.
        // Populated when `ResponseOutputItemAdded` carries a `FunctionCall`;
        // later `ResponseFunctionCallArgumentsDelta` events
        // look up `output_index` here to find the matching `tool_index`.
        let mut output_to_tool_index: BTreeMap<u32, u32> = BTreeMap::new();
        let mut next_tool_index: u32 = 0;
        // Completed output items from `response.output_item.done`. ChatGPT
        // Codex's terminal `response.completed` often has `output: []` even
        // after full message items were streamed here — without this, we
        // mis-classify as empty_response and retry forever while the UI
        // already showed ChannelToken text.
        let mut completed_output_items: Vec<rs::OutputItem> = Vec::new();
        let mut active_backend_tools: BTreeMap<String, String> = BTreeMap::new();
        let mut terminal_backend_tools: BTreeSet<String> = BTreeSet::new();

        let mut stream = raw_stream;
        loop {
            let event_result = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(event_result)) => event_result,
                Ok(None) => break,
                Err(_elapsed) => {
                    let err = SamplingError::IdleTimeout {
                        elapsed_secs: idle_timeout.as_secs(),
                    };
                    for event in fail_active_backend_tools(
                        &mut active_backend_tools,
                        &mut terminal_backend_tools,
                        &request_id,
                        "backend request timed out",
                    ) {
                        yield event;
                    }
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            let item = match event_result {
                Ok(item) => item,
                Err(err) => {
                    for event in fail_active_backend_tools(
                        &mut active_backend_tools,
                        &mut terminal_backend_tools,
                        &request_id,
                        &err.to_string(),
                    ) {
                        yield event;
                    }
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            // Heartbeat / forward-compat skip frames carry no model output
            // but prove the server is alive: refresh the content-stall timer
            // and continue. codex-rs's idle timeout lives at the raw frame
            // level and behaves the same way.
            let event = match item {
                ResponsesStreamItem::Event(event) => event,
                ResponsesStreamItem::Heartbeat => {
                    last_content_chunk_at = Instant::now();
                    continue;
                }
            };

            if responses_event_may_have_output(&event) {
                output_observed.store(true, Ordering::Relaxed);
            }

            // A confident server-detected loop aborts the attempt (dropping
            // the SSE connection) so the retry loop can resample instead of
            // streaming the burning tail. Checked before the event is
            // processed so a terminal frame carrying the signal never
            // becomes the accepted response while the abort is armed.
            if let Some(triggers) = doom_loop.as_ref().and_then(|c| c.abort_triggers()) {
                let err = SamplingError::DoomLoopDetected {
                    triggers,
                    aborted_at_chunk: Some(chunk_index),
                };
                for event in fail_active_backend_tools(
                    &mut active_backend_tools,
                    &mut terminal_backend_tools,
                    &request_id,
                    "backend request aborted after doom-loop detection",
                ) {
                    yield event;
                }
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            let event_has_content = responses_event_has_meaningful_content(&event);

            // Track whether ResponseIncomplete should break the loop
            // after the content-aware idle check below.
            let mut should_break = false;

            match event {
                ResponseStreamEvent::ResponseOutputTextDelta(text_delta_event) => {
                    let delta = text_delta_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_timestamps.push(Instant::now());
                        chunk_index += 1;
                        message_chunk_count += 1;
                        text_acc.push_str(&delta);
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Text,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseReasoningSummaryTextDelta(summary_event) => {
                    let delta = summary_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseReasoningTextDelta(reasoning_event) => {
                    let delta = reasoning_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        reasoning_acc.push_str(&delta);
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                // Start of a Responses FunctionCall — emit initial id+name
                // and remember the output_index → tool_index mapping.
                ResponseStreamEvent::ResponseOutputItemAdded(added_event) => {
                    match added_event.item {
                        rs::OutputItem::FunctionCall(fc) => {
                            let tool_index = next_tool_index;
                            next_tool_index += 1;
                            output_to_tool_index.insert(added_event.output_index, tool_index);

                            yield SamplingEvent::ToolCallDelta {
                                request_id: request_id.clone(),
                                tool_index,
                                id: Some(fc.call_id),
                                name: Some(fc.name),
                                arguments_delta: None,
                            };
                        }
                        rs::OutputItem::WebSearchCall(call) => {
                            if let Some(event) = start_backend_tool(
                                &mut active_backend_tools,
                                &terminal_backend_tools,
                                &request_id,
                                call.id,
                                "web_search",
                            ) {
                                yield event;
                            }
                        }
                        rs::OutputItem::CustomToolCall(call) => {
                            if let Some(event) = start_backend_tool(
                                &mut active_backend_tools,
                                &terminal_backend_tools,
                                &request_id,
                                call.id,
                                "x_search",
                            ) {
                                yield event;
                            }
                        }
                        _ => {}
                    }
                }

                // Continuation chunk for a streaming FunctionCall's args.
                // Drop silently if no preceding OutputItemAdded mapped.
                ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(args_event) => {
                    let delta = args_event.delta;
                    if !delta.is_empty()
                        && let Some(&tool_index) =
                            output_to_tool_index.get(&args_event.output_index)
                    {
                        yield SamplingEvent::ToolCallDelta {
                            request_id: request_id.clone(),
                            tool_index,
                            id: None,
                            name: None,
                            arguments_delta: Some(delta),
                        };
                    }
                }

                ResponseStreamEvent::ResponseCompleted(completed_event) => {
                    final_response = Some(completed_event.response);
                }

                ResponseStreamEvent::ResponseIncomplete(incomplete_event) => {
                    final_response = Some(incomplete_event.response);
                    for event in fail_active_backend_tools(
                        &mut active_backend_tools,
                        &mut terminal_backend_tools,
                        &request_id,
                        "response incomplete",
                    ) {
                        yield event;
                    }
                    should_break = true;
                }

                ResponseStreamEvent::ResponseFailed(failed_event) => {
                    let response = failed_event.response;
                    let error_message = response
                        .error
                        .as_ref()
                        .map(|e| format!("{}: {}", e.code, e.message))
                        .unwrap_or_else(|| "Response failed with unknown error".to_string());
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message.clone(),
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                    };
                    for event in fail_active_backend_tools(
                        &mut active_backend_tools,
                        &mut terminal_backend_tools,
                        &request_id,
                        &error_message,
                    ) {
                        yield event;
                    }
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                ResponseStreamEvent::ResponseError(error_event) => {
                    let code = error_event.code.unwrap_or_else(|| "error".to_string());
                    let error_message = format!("{}: {}", code, error_event.message);
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message.clone(),
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                    };
                    for event in fail_active_backend_tools(
                        &mut active_backend_tools,
                        &mut terminal_backend_tools,
                        &request_id,
                        &error_message,
                    ) {
                        yield event;
                    }
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                // ── Backend-hosted tool lifecycle events ────────────
                // These tools are executed server-side by the agentic
                // sampler. We emit progress events so the shell/pager
                // can show status to the user.

                // Web search
                ResponseStreamEvent::ResponseWebSearchCallInProgress(ev) => {
                    if let Some(event) = start_backend_tool(
                        &mut active_backend_tools,
                        &terminal_backend_tools,
                        &request_id,
                        ev.item_id,
                        "web_search",
                    ) {
                        yield event;
                    }
                }
                ResponseStreamEvent::ResponseWebSearchCallSearching(ev) => {
                    if let Some(event) = start_backend_tool(
                        &mut active_backend_tools,
                        &terminal_backend_tools,
                        &request_id,
                        ev.item_id,
                        "web_search",
                    ) {
                        yield event;
                    }
                }
                // This progress event carries no result. Keep the call open
                // until output_item.done or terminal response reconciliation.
                ResponseStreamEvent::ResponseWebSearchCallCompleted(ev) => {
                    if let Some(event) = start_backend_tool(
                        &mut active_backend_tools,
                        &terminal_backend_tools,
                        &request_id,
                        ev.item_id,
                        "web_search",
                    ) {
                        yield event;
                    }
                }

                // Code interpreter (server-side, like web/x search). Surface it
                // the same way x_search is: a generic backend tool call that the
                // shell renders as a client `tool_use` + `user` `tool_result`
                // split (grok has no HostedTool::CodeInterpreter, so these events
                // are latent under the current hosted-tool set). The started
                // event fires on InProgress; the full payload (code + outputs)
                // rides ResponseOutputItemDone(CodeInterpreterCall) below.
                ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(ev) => {
                    if let Some(event) = start_backend_tool(
                        &mut active_backend_tools,
                        &terminal_backend_tools,
                        &request_id,
                        ev.item_id,
                        "code_interpreter",
                    ) {
                        yield event;
                    }
                }
                // Interpreting/Completed carry no payload — the result arrives
                // via ResponseOutputItemDone(CodeInterpreterCall) below.
                ResponseStreamEvent::ResponseCodeInterpreterCallInterpreting(_)
                | ResponseStreamEvent::ResponseCodeInterpreterCallCompleted(_) => {}

                // OutputItemDone carries the full result for backend tools
                // and (on ChatGPT Codex) the completed assistant message /
                // function_call items. Codex often leaves
                // `response.completed.output` empty, so we also stash items
                // for the final ConversationResponse.
                // For WebSearchCall this includes the query and source URLs.
                // For CustomToolCall this includes x_search results.
                ResponseStreamEvent::ResponseOutputItemDone(done_event) => {
                    if let Some((call_id, name, outcome)) = backend_tool_outcome(&done_event.item) {
                        for event in finish_backend_tool(
                            &mut active_backend_tools,
                            &mut terminal_backend_tools,
                            &request_id,
                            call_id,
                            name,
                            outcome,
                        ) {
                            yield event;
                        }
                    }
                    completed_output_items.push(done_event.item);
                }

                // CustomToolCallInputDelta is x_search in-progress streaming.
                // Emit a started event on first delta per item_id.
                ResponseStreamEvent::ResponseCustomToolCallInputDelta(ev) => {
                    if let Some(event) = start_backend_tool(
                        &mut active_backend_tools,
                        &terminal_backend_tools,
                        &request_id,
                        ev.item_id,
                        "x_search",
                    ) {
                        yield event;
                    }
                }
                ResponseStreamEvent::ResponseCustomToolCallInputDone(ev) => {
                    if let Some(event) = start_backend_tool(
                        &mut active_backend_tools,
                        &terminal_backend_tools,
                        &request_id,
                        ev.item_id,
                        "x_search",
                    ) {
                        yield event;
                    }
                }

                // All other events (intermediate progress, annotations,
                // image gen, file search, etc.) — no action needed.
                _ => {}
            }

            if event_has_content {
                last_content_chunk_at = Instant::now();
            } else if last_content_chunk_at.elapsed() > idle_timeout {
                let err = SamplingError::IdleTimeout {
                    elapsed_secs: idle_timeout.as_secs(),
                };
                for event in fail_active_backend_tools(
                    &mut active_backend_tools,
                    &mut terminal_backend_tools,
                    &request_id,
                    "backend request timed out",
                ) {
                    yield event;
                }
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            if should_break {
                break;
            }
        }

        // ── Build the final response ─────────────────────────────────
        let mut response = match final_response {
            Some(r) => r,
            None => {
                let err = SamplingError::Api {
                    status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    message: "No ResponseCompleted or ResponseIncomplete event received from \
                              Responses API"
                        .to_string(),
                    model_metadata: None,
                    retry_after_secs: None,
                    should_retry: None,
                };
                for event in fail_active_backend_tools(
                    &mut active_backend_tools,
                    &mut terminal_backend_tools,
                    &request_id,
                    "response stream ended before a terminal response",
                ) {
                    yield event;
                }
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }
        };

        // ChatGPT Codex (`chatgpt.com/backend-api/codex`) may return an empty
        // or only partially populated terminal `output`. Preserve its order,
        // then append items seen only on `output_item.done`. Prefer type+id,
        // fall back to type+call_id, and finally typed equality for identity.
        let merged_output_items =
            merge_completed_output_items(&mut response.output, completed_output_items);
        if merged_output_items > 0 {
            tracing::debug!(
                request_id = %request_id,
                n_items = merged_output_items,
                "merged output_item.done items missing from terminal response"
            );
        }
        if response.output.is_empty() && !text_acc.is_empty() {
            // Fallback: only deltas were usable (no done items).
            tracing::debug!(
                request_id = %request_id,
                text_len = text_acc.len(),
                "Codex completed with empty output[]; synthesizing message from text deltas"
            );
            response.output = vec![rs::OutputItem::Message(rs::OutputMessage {
                id: format!("msg_stream_{request_id}"),
                role: rs::AssistantRole::Assistant,
                status: rs::OutputStatus::Completed,
                content: vec![rs::OutputMessageContent::OutputText(rs::OutputTextContent {
                    text: text_acc.clone(),
                    annotations: vec![],
                    logprobs: None,
                })],
            })];
        }

        // Some compatible providers omit output_item.done and place the full
        // hosted-tool item only in the terminal response. Reconcile it here.
        for item in &response.output {
            if let Some((call_id, name, outcome)) = backend_tool_outcome(item) {
                for event in finish_backend_tool(
                    &mut active_backend_tools,
                    &mut terminal_backend_tools,
                    &request_id,
                    call_id,
                    name,
                    outcome,
                ) {
                    yield event;
                }
            }
        }
        // Last-resort compatibility for providers that send progress plus an
        // empty successful completion and no output item.
        for (call_id, name) in std::mem::take(&mut active_backend_tools) {
            terminal_backend_tools.insert(call_id.clone());
            yield SamplingEvent::BackendToolCallCompleted {
                request_id: request_id.clone(),
                call_id,
                name,
                result: None,
            };
        }

        // Billing fields (`prompt_tokens`, `completion_tokens`,
        // `cached_prompt_tokens`, `reasoning_tokens`) are the cumulative
        // wire values — they sum across every server-side turn of the
        // agent loop and are what we bill on / log to telemetry.
        //
        // `total_tokens` is the live context length used to drive the
        // CLI `/context` bar, the auto-compact threshold, and
        // `meta.totalTokens` on persisted sessions. The SSE decoder
        // (`deserialize_response_event`) has already rewritten
        // `u.total_tokens` to `context_details.input + output` when
        // the backend emits it; on older deployments the wire
        // value passes through unchanged.
        let usage = response.usage.as_ref().map(|u| TokenUsage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
            reasoning_tokens: u.output_tokens_details.reasoning_tokens,
            cached_prompt_tokens: u.input_tokens_details.cached_tokens,
            cache_creation_prompt_tokens: 0,
        });

        let cost_usd_ticks = response
            .metadata
            .as_mut()
            .and_then(|m| m.remove(crate::client::COST_USD_TICKS_METADATA_KEY))
            .and_then(|s| s.parse::<i64>().ok());

        let status = response.status.clone();

        // Convert to ConversationItem(s); patch in accumulated reasoning
        // text as a fallback when the final response lacks `content` /
        // `summary` (the streaming deltas may have arrived out of band).
        // Splice policy lives in `inject_streaming_reasoning_fallback`.
        let mut items = xai_grok_sampling_types::response_to_conversation_items(response);
        xai_grok_sampling_types::inject_streaming_reasoning_fallback(&mut items, reasoning_acc);

        let has_tool_calls = items.iter().any(|i| match i {
            ConversationItem::Assistant(a) => !a.tool_calls.is_empty(),
            _ => false,
        });

        let stop_reason = if has_tool_calls {
            Some(StopReason::ToolCalls)
        } else {
            match status {
                Status::Completed => Some(StopReason::Stop),
                Status::Incomplete => Some(StopReason::Length),
                _ => None,
            }
        };

        let stream_end = Instant::now();
        let metrics =
            InferenceLatencyStats::from_timestamps(stream_start, &chunk_timestamps, stream_end);

        // Warn-only for now: surface the server-reported triggers once per
        // request (raw labels only — ZDR-safe) and attach them for callers.
        let doom_loop_signals = doom_loop
            .as_ref()
            .map(|collector| collector.take())
            .unwrap_or_default();
        if !doom_loop_signals.is_empty() {
            tracing::warn!(
                request_id = %request_id,
                triggers = ?doom_loop_signals.iter().map(|s| s.raw.as_str()).collect::<Vec<_>>(),
                "server reported doom-loop triggers for this response"
            );
        }

        let conversation_response = ConversationResponse {
            items,
            stop_reason,
            usage,
            cost_usd_ticks,
            message_chunks_emitted: message_chunk_count,
            doom_loop_signals,
            stop_message: None, // not reported on the Responses API
            message_id: None,   // no provider message id on the Responses API
            raw_stop_reason: None,
            stop_sequence: None,
        };

        yield SamplingEvent::Completed {
            request_id: request_id.clone(),
            response: Box::new(conversation_response),
            metrics,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses as rs_types;
    use futures_util::stream;
    use std::pin::pin;

    fn rid() -> RequestId {
        RequestId::from("resp-test")
    }

    /// Wrap typed rs events into the raw-stream item type the SSE decoder
    /// emits in production.
    fn raw_events(
        events: Vec<Result<rs::ResponseStreamEvent, SamplingError>>,
    ) -> BoxStream<'static, Result<ResponsesStreamItem, SamplingError>> {
        stream::iter(
            events
                .into_iter()
                .map(|r| r.map(ResponsesStreamItem::Event)),
        )
        .boxed()
    }

    /// Build a minimal `rs_types::Response` for use in `ResponseCompleted`
    fn build_response(status: rs_types::Status) -> rs_types::Response {
        rs_types::Response {
            background: None,
            billing: None,
            conversation: None,
            created_at: 0,
            completed_at: None,
            error: None,
            id: "resp_1".into(),
            incomplete_details: None,
            instructions: None,
            max_output_tokens: None,
            metadata: None,
            model: "test-model".into(),
            object: "response".into(),
            output: vec![],
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None,
            service_tier: None,
            status,
            temperature: None,
            text: None,
            tool_choice: None,
            tools: None,
            top_logprobs: None,
            top_p: None,
            truncation: None,
            usage: None,
        }
    }

    fn empty_completed_response() -> rs_types::Response {
        build_response(rs_types::Status::Completed)
    }

    fn failed_response_with_error(message: &str) -> rs_types::Response {
        let mut r = build_response(rs_types::Status::Failed);
        r.error = Some(rs_types::ErrorObject {
            code: "server_error".into(),
            message: message.into(),
        });
        r
    }

    fn text_delta_event(delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputTextDelta(rs_types::ResponseTextDeltaEvent {
            sequence_number: 0,
            item_id: "item-1".into(),
            output_index: 0,
            content_index: 0,
            delta: delta.into(),
            logprobs: None,
        })
    }

    fn completed_event() -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
            response: empty_completed_response(),
            sequence_number: 0,
        })
    }

    fn message_item_done(text: &str) -> rs::ResponseStreamEvent {
        message_item_done_with_id("msg_1", text)
    }

    fn message_item(id: &str, text: &str) -> rs::OutputItem {
        rs::OutputItem::Message(rs::OutputMessage {
            id: id.into(),
            role: rs::AssistantRole::Assistant,
            status: rs::OutputStatus::Completed,
            content: vec![rs::OutputMessageContent::OutputText(
                rs::OutputTextContent {
                    text: text.into(),
                    annotations: vec![],
                    logprobs: None,
                },
            )],
        })
    }

    fn message_item_done_with_id(id: &str, text: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemDone(rs_types::ResponseOutputItemDoneEvent {
            sequence_number: 0,
            output_index: 0,
            item: message_item(id, text),
        })
    }

    fn web_search_item(id: &str, status: rs::WebSearchToolCallStatus) -> rs::OutputItem {
        rs::OutputItem::WebSearchCall(rs::WebSearchToolCall {
            id: id.into(),
            status,
            action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                query: "rust async traits".into(),
                sources: Some(vec![]),
            }),
        })
    }

    fn web_search_added(id: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemAdded(rs_types::ResponseOutputItemAddedEvent {
            sequence_number: 0,
            output_index: 0,
            item: web_search_item(id, rs::WebSearchToolCallStatus::InProgress),
        })
    }

    fn web_search_done(id: &str, status: rs::WebSearchToolCallStatus) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemDone(rs_types::ResponseOutputItemDoneEvent {
            sequence_number: 1,
            output_index: 0,
            item: web_search_item(id, status),
        })
    }

    fn web_search_progress(id: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseWebSearchCallInProgress(
            rs_types::ResponseWebSearchCallInProgressEvent {
                sequence_number: 0,
                output_index: 0,
                item_id: id.into(),
            },
        )
    }

    fn backend_lifecycle_counts(events: &[SamplingEvent]) -> (usize, usize, usize) {
        let started = events
            .iter()
            .filter(|event| matches!(event, SamplingEvent::BackendToolCallStarted { .. }))
            .count();
        let completed = events
            .iter()
            .filter(|event| matches!(event, SamplingEvent::BackendToolCallCompleted { .. }))
            .count();
        let failed = events
            .iter()
            .filter(|event| matches!(event, SamplingEvent::BackendToolCallFailed { .. }))
            .count();
        (started, completed, failed)
    }

    #[tokio::test]
    async fn web_search_done_only_synthesizes_started_once() {
        let raw = raw_events(vec![
            Ok(web_search_done(
                "ws_done_only",
                rs::WebSearchToolCallStatus::Completed,
            )),
            Ok(completed_event()),
        ]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert_eq!(backend_lifecycle_counts(&events), (1, 1, 0));
    }

    #[tokio::test]
    async fn web_search_progress_added_and_done_are_deduplicated() {
        let raw = raw_events(vec![
            Ok(web_search_progress("ws_dedup")),
            Ok(web_search_progress("ws_dedup")),
            Ok(web_search_added("ws_dedup")),
            Ok(web_search_done(
                "ws_dedup",
                rs::WebSearchToolCallStatus::Completed,
            )),
            Ok(completed_event()),
        ]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert_eq!(backend_lifecycle_counts(&events), (1, 1, 0));
    }

    #[tokio::test]
    async fn failed_web_search_done_maps_to_failed_lifecycle() {
        let raw = raw_events(vec![
            Ok(web_search_done(
                "ws_failed",
                rs::WebSearchToolCallStatus::Failed,
            )),
            Ok(completed_event()),
        ]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert_eq!(backend_lifecycle_counts(&events), (1, 0, 1));
    }

    #[tokio::test]
    async fn terminal_output_reconciles_missing_done_event() {
        let mut response = empty_completed_response();
        response.output.push(web_search_item(
            "ws_terminal",
            rs::WebSearchToolCallStatus::Completed,
        ));
        let terminal =
            rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
                response,
                sequence_number: 0,
            });
        let events = collect(stream_responses(
            raw_events(vec![Ok(terminal)]),
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert_eq!(backend_lifecycle_counts(&events), (1, 1, 0));
    }

    #[tokio::test]
    async fn empty_success_closes_progress_only_search() {
        let events = collect(stream_responses(
            raw_events(vec![
                Ok(web_search_progress("ws_progress_only")),
                Ok(completed_event()),
            ]),
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert_eq!(backend_lifecycle_counts(&events), (1, 1, 0));
    }

    #[tokio::test]
    async fn response_failure_closes_active_search_as_failed() {
        let failed = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("provider failed"),
            sequence_number: 1,
        });
        let events = collect(stream_responses(
            raw_events(vec![Ok(web_search_progress("ws_active")), Ok(failed)]),
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert_eq!(backend_lifecycle_counts(&events), (1, 0, 1));
        assert!(matches!(events.last(), Some(SamplingEvent::Failed { .. })));
    }

    /// ChatGPT Codex: deltas + output_item.done with text, but completed.output=[].
    /// Must not classify as empty (would spin retries while UI already showed text).
    #[tokio::test]
    async fn codex_empty_completed_output_adopts_output_item_done() {
        let raw = raw_events(vec![
            Ok(text_delta_event("Hi")),
            Ok(text_delta_event("!")),
            Ok(message_item_done("Hi!")),
            Ok(completed_event()),
        ]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let completed = events
            .iter()
            .find_map(|e| match e {
                SamplingEvent::Completed { response, .. } => Some(response.as_ref()),
                _ => None,
            })
            .expect("Completed event");
        assert!(
            completed.empty_reason().is_none(),
            "must not be empty: {:?}",
            completed.empty_reason()
        );
        assert_eq!(completed.assistant_text(), "Hi!");
    }

    #[tokio::test]
    async fn codex_partial_terminal_output_merges_only_missing_done_items() {
        let raw = raw_events(vec![
            Ok(message_item_done_with_id("msg_terminal", "first")),
            Ok(message_item_done_with_id("msg_done_only", " second")),
            Ok({
                let mut response = empty_completed_response();
                response.output.push(message_item("msg_terminal", "first"));
                rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
                    response,
                    sequence_number: 2,
                })
            }),
        ]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let completed = events
            .iter()
            .find_map(|event| match event {
                SamplingEvent::Completed { response, .. } => Some(response.as_ref()),
                _ => None,
            })
            .expect("Completed event");

        assert_eq!(completed.assistant_text(), "first\n second");
    }

    async fn collect(s: impl Stream<Item = SamplingEvent>) -> Vec<SamplingEvent> {
        let mut out = Vec::new();
        let mut s = pin!(s);
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn missing_completed_event_yields_failed() {
        let raw = raw_events(Vec::new());
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_delta_then_completed_yields_completed_with_stop() {
        let raw = raw_events(vec![Ok(text_delta_event("hello")), Ok(completed_event())]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let text_tokens: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                SamplingEvent::ChannelToken {
                    channel: SamplingChannel::Text,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_tokens, vec!["hello"]);

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn empty_failed_response_is_not_treated_as_output() {
        let event = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("boom"),
            sequence_number: 0,
        });
        assert!(!responses_event_may_have_output(&event));
    }

    #[tokio::test]
    async fn response_failed_yields_failed_500() {
        let failed = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("boom"),
            sequence_number: 0,
        });
        let raw = raw_events(vec![Ok(failed)]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
                assert!(error.message.contains("boom"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mid_stream_transport_error_yields_failed() {
        let raw = raw_events(vec![
            Ok(text_delta_event("hi")),
            Err(SamplingError::EventStreamError("conn reset".into())),
        ]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Failed { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_when_stream_stalls() {
        let raw = raw_events(vec![Ok(text_delta_event("hi"))])
            .chain(stream::pending())
            .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_millis(100),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::IdleTimeout);
            }
            other => panic!("expected Failed(IdleTimeout), got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_frames_keep_long_silent_phases_alive() {
        // 300s of heartbeat-only streaming (well past the 60s idle timeout),
        // then real content: the stream must complete, not fail with
        // IdleTimeout. Heartbeat gaps stay under the timeout — a gap longer
        // than idle_timeout still fails (dead connection).
        let raw = async_stream::stream! {
            for _ in 0..10 {
                yield Ok(ResponsesStreamItem::Heartbeat);
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            yield Ok(ResponsesStreamItem::Event(text_delta_event("hi")));
            yield Ok(ResponsesStreamItem::Event(completed_event()));
        }
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Failed { .. })),
            "heartbeat-kept-alive stream must not fail: {events:?}"
        );
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_gap_beyond_idle_timeout_still_fails() {
        // One heartbeat, then silence longer than idle_timeout: liveness is
        // disproven, so the transport timeout must fire.
        let raw = async_stream::stream! {
            yield Ok(ResponsesStreamItem::Heartbeat);
            tokio::time::sleep(Duration::from_secs(300)).await;
            yield Ok(ResponsesStreamItem::Event(completed_event()));
        }
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::IdleTimeout);
            }
            other => panic!("expected Failed(IdleTimeout), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_metadata_yielded_after_stream_started() {
        let raw = raw_events(vec![Ok(completed_event())]);
        let metadata = ResponseModelMetadata {
            context_window: Some(8192),
            ..Default::default()
        };
        let events = collect(stream_responses(
            raw,
            Some(metadata),
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(matches!(events[0], SamplingEvent::StreamStarted { .. }));
        assert!(matches!(events[1], SamplingEvent::ModelMetadata { .. }));
    }

    #[test]
    fn meaningful_content_classifier_basics() {
        // Text delta with content is meaningful.
        let event = text_delta_event("foo");
        assert!(responses_event_has_meaningful_content(&event));
        // Empty text delta is not.
        let empty = text_delta_event("");
        assert!(!responses_event_has_meaningful_content(&empty));
        // Completed is meaningful (terminal).
        assert!(responses_event_has_meaningful_content(&completed_event()));
    }

    #[test]
    fn output_classifier_covers_non_forwarded_backend_events() {
        let queued = rs::ResponseStreamEvent::ResponseQueued(rs_types::ResponseQueuedEvent {
            sequence_number: 0,
            response: empty_completed_response(),
        });
        assert!(!responses_event_may_have_output(&queued));

        let response_error = rs::ResponseStreamEvent::ResponseError(rs_types::ResponseErrorEvent {
            sequence_number: 1,
            code: Some("server_error".into()),
            message: "failed before output".into(),
            param: None,
        });
        assert!(!responses_event_may_have_output(&response_error));

        let refusal =
            rs::ResponseStreamEvent::ResponseRefusalDelta(rs_types::ResponseRefusalDeltaEvent {
                sequence_number: 1,
                item_id: "item-1".into(),
                output_index: 0,
                content_index: 0,
                delta: "no".into(),
            });
        assert!(responses_event_may_have_output(&refusal));

        let backend_progress = rs::ResponseStreamEvent::ResponseWebSearchCallSearching(
            rs_types::ResponseWebSearchCallSearchingEvent {
                sequence_number: 2,
                output_index: 0,
                item_id: "search-1".into(),
            },
        );
        assert!(responses_event_may_have_output(&backend_progress));
    }

    #[tokio::test]
    async fn tracked_stream_marks_non_forwarded_refusal_as_output() {
        let output_observed = Arc::new(AtomicBool::new(false));
        let refusal =
            rs::ResponseStreamEvent::ResponseRefusalDelta(rs_types::ResponseRefusalDeltaEvent {
                sequence_number: 0,
                item_id: "item-1".into(),
                output_index: 0,
                content_index: 0,
                delta: "no".into(),
            });
        let raw = stream::iter(vec![
            Ok(ResponsesStreamItem::Event(refusal)),
            Ok(ResponsesStreamItem::Event(completed_event())),
        ])
        .boxed();
        let _ = collect(stream_responses_tracked(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            Arc::clone(&output_observed),
        ))
        .await;

        assert!(output_observed.load(Ordering::Relaxed));
    }

    /// A server-side code-interpreter run surfaces as a generic backend tool
    /// call (started on InProgress, completed on OutputItemDone) named
    /// "code_interpreter" — the same shape as x_search — so it is no longer
    /// silently dropped from the event stream.
    #[tokio::test]
    async fn code_interpreter_forwards_backend_tool_call() {
        let in_progress = rs::ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(
            rs_types::ResponseCodeInterpreterCallInProgressEvent {
                sequence_number: 0,
                output_index: 0,
                item_id: "ci-1".into(),
            },
        );
        let done = rs::ResponseStreamEvent::ResponseOutputItemDone(
            rs_types::ResponseOutputItemDoneEvent {
                sequence_number: 1,
                output_index: 0,
                item: rs_types::OutputItem::CodeInterpreterCall(
                    rs_types::CodeInterpreterToolCall {
                        code: Some("print(1)".into()),
                        container_id: "cont-1".into(),
                        id: "ci-1".into(),
                        outputs: None,
                        status: rs_types::CodeInterpreterToolCallStatus::Completed,
                    },
                ),
            },
        );
        let raw = stream::iter(vec![Ok(in_progress), Ok(done), Ok(completed_event())])
            .map(|event| event.map(ResponsesStreamItem::Event))
            .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                SamplingEvent::BackendToolCallStarted { call_id, name, .. }
                    if call_id == "ci-1" && name == "code_interpreter"
            )),
            "expected a code_interpreter BackendToolCallStarted, got {events:?}"
        );
        let completed = events.iter().find_map(|e| match e {
            SamplingEvent::BackendToolCallCompleted {
                call_id,
                name,
                result,
                ..
            } if name == "code_interpreter" => Some((call_id.clone(), result.clone())),
            _ => None,
        });
        let (call_id, result) = completed.expect("a code_interpreter BackendToolCallCompleted");
        assert_eq!(call_id, "ci-1");
        let result = result.expect("serialized code-interpreter payload");
        assert_eq!(result["code"], "print(1)");
    }

    fn function_call_added_event(
        output_index: u32,
        call_id: &str,
        name: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemAdded(rs_types::ResponseOutputItemAddedEvent {
            sequence_number: 0,
            output_index,
            item: rs_types::OutputItem::FunctionCall(rs_types::FunctionToolCall {
                arguments: String::new(),
                call_id: call_id.into(),
                name: name.into(),
                id: None,
                status: None,
            }),
        })
    }

    fn function_call_args_delta_event(output_index: u32, delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(
            rs_types::ResponseFunctionCallArgumentsDeltaEvent {
                sequence_number: 0,
                item_id: format!("item-{output_index}"),
                output_index,
                delta: delta.into(),
            },
        )
    }

    type Delta = (u32, Option<String>, Option<String>, Option<String>);

    /// Extract all ToolCallDelta events as (tool_index, id, name, arguments_delta).
    fn tool_call_deltas(evs: &[SamplingEvent]) -> Vec<Delta> {
        evs.iter()
            .filter_map(|e| match e {
                SamplingEvent::ToolCallDelta {
                    tool_index,
                    id,
                    name,
                    arguments_delta,
                    ..
                } => Some((
                    *tool_index,
                    id.clone(),
                    name.clone(),
                    arguments_delta.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn function_call_emits_initial_id_name_then_arg_deltas() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_xyz", "do_thing")),
            Ok(function_call_args_delta_event(0, "{\"x\":")),
            Ok(function_call_args_delta_event(0, "1}")),
            Ok(completed_event()),
        ];
        let raw = raw_events(events);
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].0, 0);
        assert_eq!(deltas[0].1.as_deref(), Some("call_xyz"));
        assert_eq!(deltas[0].2.as_deref(), Some("do_thing"));
        assert_eq!(deltas[0].3, None);
        assert_eq!(deltas[1].0, 0);
        assert_eq!(deltas[1].1, None);
        assert_eq!(deltas[1].2, None);
        assert_eq!(deltas[1].3.as_deref(), Some("{\"x\":"));
        assert_eq!(deltas[2].3.as_deref(), Some("1}"));
    }

    #[tokio::test]
    async fn function_call_args_delta_without_added_event_is_dropped() {
        // ArgumentsDelta with no preceding OutputItemAdded has no
        // output_index → tool_index mapping; drop silently.
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_args_delta_event(7, "{\"oops\":1}")),
            Ok(completed_event()),
        ];
        let raw = raw_events(events);
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        assert_eq!(tool_call_deltas(&evs).len(), 0);
    }

    #[tokio::test]
    async fn multiple_function_calls_get_distinct_tool_indices() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_a", "tool_a")),
            Ok(function_call_added_event(1, "call_b", "tool_b")),
            Ok(function_call_args_delta_event(0, "a-args")),
            Ok(function_call_args_delta_event(1, "b-args")),
            Ok(completed_event()),
        ];
        let raw = raw_events(events);
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        assert_eq!(deltas.len(), 4);
        assert_eq!(deltas[0].0, 0);
        assert_eq!(deltas[0].1.as_deref(), Some("call_a"));
        assert_eq!(deltas[1].0, 1);
        assert_eq!(deltas[1].1.as_deref(), Some("call_b"));
        assert_eq!(deltas[2].0, 0);
        assert_eq!(deltas[2].3.as_deref(), Some("a-args"));
        assert_eq!(deltas[3].0, 1);
        assert_eq!(deltas[3].3.as_deref(), Some("b-args"));
    }

    #[tokio::test]
    async fn doom_loop_collector_signals_land_on_completed_response() {
        use xai_grok_sampling_types::doom_loop::{
            DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA,
        };
        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb(DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA));
        let raw = raw_events(vec![Ok(text_delta_event("hello")), Ok(completed_event())]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 1);
                assert_eq!(
                    response.doom_loop_signals[0].raw,
                    "tail_repetition:4@response"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// An armed collector holding a confident signal aborts the attempt with
    /// a retryable doom-loop failure; disarmed, the same stream completes and
    /// the signals ride the response instead.
    #[tokio::test]
    async fn confident_signal_aborts_stream_unless_disarmed() {
        let confident = r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking"]}}"#;

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        let raw = raw_events(vec![Ok(text_delta_event("hi")), Ok(completed_event())]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(
                    error.kind,
                    crate::events::SamplingErrorKind::DoomLoopDetected
                );
                assert!(error.is_retryable);
                assert_eq!(
                    error.doom_loop_triggers.as_deref(),
                    Some(&["tail_repetition:8@thinking".to_string()][..])
                );
            }
            other => panic!("expected Failed(DoomLoopDetected), got {other:?}"),
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        collector.disarm_abort();
        let raw = raw_events(vec![Ok(text_delta_event("hi")), Ok(completed_event())]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 1);
            }
            other => panic!("expected Completed after disarm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn doom_loop_signals_empty_without_collector_or_triggers() {
        let raw = raw_events(vec![Ok(completed_event())]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // A collector that never saw a trigger also leaves the field empty.
        let raw = raw_events(vec![Ok(completed_event())]);
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(crate::doom_loop::DoomLoopSignalCollector::default()),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
