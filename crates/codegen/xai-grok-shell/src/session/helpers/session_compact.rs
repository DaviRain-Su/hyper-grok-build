//! Compacts the current conversation and generates a summary of the conversation which
//! gets passed to the next turn of the model
use crate::sampling::{
    ApiBackend, Client as OaiCompatClient, ConversationItem, ConversationRequest,
    ConversationToolChoice, HostedTool, SamplingError, ToolSpec,
};
use agent_client_protocol as acp;
use async_openai::types::responses::ResponseStreamEvent;
use futures_util::StreamExt;
use reqwest::StatusCode;
pub use xai_chat_state::compaction_utils::{
    AUTO_CONTINUE_PROMPT, extract_last_real_user_query, extract_last_user_query,
    extract_messages_since_last_user, extract_real_user_queries, is_synthetic_extracted_query,
};
use xai_grok_sampler::SamplerConfig as SamplingConfig;
/// Short, self-narrating compaction prompt used by the short-prompt harness only.
/// Frames the call as "summarize for a successor assistant who only sees
/// the user's original query plus this summary." Wrapped in
/// `<summary_request>` only -- the surrounding `<user_query>` is implicit
/// because we push this as a `ConversationItem::user`.
///
/// All other agents (grok-build, etc.) continue to use the detailed
/// structured prompt built inline in `generate_session_compact`.
pub(crate) const SELF_SUMMARIZATION_PROMPT: &str = r#"<summary_request>
Please summarize the conversation so far. This summary (everything after your
thinking) will be provided to another AI assistant to continue working on the
task. The other assistant will only see the user's original query and your
summary, it will not have access to any tool calls or tool outputs from this
conversation. The purpose of the summary is to compress the conversation
context while preserving the essential information needed to seamlessly
continue. Useful things to include: the user's requests, what you've done so
far, relevant file paths and code details, any errors encountered and how
they were resolved, and what remains to be done. DO NOT call any tools in
your response.
</summary_request>"#;
/// Outcome of a failed `generate_session_compact` call, classified at the
/// point of the typed upstream error so the caller can short-circuit
/// retries without re-parsing free-form error strings.
#[derive(Debug)]
pub(crate) enum CompactFailure {
    /// Retrying the same payload will hit the same failure. The retry loop
    /// in `run_compact_inner` should bail without sleeping or re-issuing.
    Deterministic(acp::Error),
    /// Failure may resolve on retry. The caller follows its existing
    /// N-attempt + backoff loop.
    Transient(acp::Error),
}
pub(crate) use xai_grok_sampling_types::is_context_length_error;
/// Classify an upstream `SamplingError` for the compaction retry loop.
///
/// `Auth`, `InvalidConfiguration`, `Serialization` and
/// `IdleTimeout` are all deterministic by construction (re-issuing the same
/// request cannot change the outcome — auth state, config, payload shape,
/// and stuck-model conditions all persist). 4xx API responses other than
/// 408 (timeout) and 429 (rate limit) are likewise deterministic. Network
/// transport errors, stream-level blips, and 5xx responses are transient.
fn classify_sampling_error(err: SamplingError) -> CompactFailure {
    let acp_err = acp::Error::internal_error().data(format!("compact failed: {err}"));
    let deterministic = match &err {
        SamplingError::Auth(_)
        | SamplingError::InvalidConfiguration(_)
        | SamplingError::Serialization(_)
        | SamplingError::IdleTimeout { .. } => true,
        SamplingError::Api {
            status, message, ..
        } => {
            is_context_length_error(message)
                || (status.is_client_error()
                    && *status != StatusCode::REQUEST_TIMEOUT
                    && *status != StatusCode::TOO_MANY_REQUESTS)
        }
        SamplingError::MaxTokensTruncation => true,
        SamplingError::Http(_)
        | SamplingError::EventStreamError(_)
        | SamplingError::StreamError { .. }
        | SamplingError::EmptyResponse { .. }
        | SamplingError::DoomLoopDetected { .. } => false,
    };
    if deterministic {
        CompactFailure::Deterministic(acp_err)
    } else {
        CompactFailure::Transient(acp_err)
    }
}
/// Classify a Anthropic-style stream error event (`ResponseError` /
/// `ResponseFailed.error`) for the compaction retry loop.
///
/// `code` is the structured `code` field on the event (typically a numeric
/// HTTP status as a string, but Anthropic also uses error-type strings like
/// `"invalid_request_error"`). `message` is the human-readable detail.
///
/// Numeric codes are classified by HTTP-status range. The Anthropic
/// `invalid_request_error` marker, which can appear in either field, always
/// maps to `Deterministic` (schema violations cannot be fixed by re-sending
/// the same payload).
fn classify_response_event_error(code: Option<&str>, message: &str) -> CompactFailure {
    let acp_err = acp::Error::internal_error().data(match code {
        Some(c) => format!("compact failed: {c}: {message}"),
        None => format!("compact failed: {message}"),
    });
    if matches!(code, Some("invalid_request_error")) || message.contains("invalid_request_error") {
        return CompactFailure::Deterministic(acp_err);
    }
    if let Some(status_code) = code.and_then(|c| c.parse::<u16>().ok())
        && (400..500).contains(&status_code)
        && status_code != 408
        && status_code != 429
    {
        return CompactFailure::Deterministic(acp_err);
    }
    if is_context_length_error(message) {
        return CompactFailure::Deterministic(acp_err);
    }
    CompactFailure::Transient(acp_err)
}
/// Build the chat history that will be sent to the compaction model.
///
/// Appends the summarization prompt (selected by `use_short_prompt` and
/// optionally splicing in the user-provided `/compact <text>` context) as
/// the final `User` item.
///
/// Pure function — no I/O. Extracted so callers can persist the exact payload
/// that will be sent (for offline prompt iteration) without duplicating the
/// prompt-building logic that lives in `generate_session_compact`.
pub(crate) fn build_compaction_chat_history(
    mut chat_history: Vec<ConversationItem>,
    user_context: Option<&str>,
    use_short_prompt: bool,
) -> Vec<ConversationItem> {
    let prompt = build_compaction_prompt(user_context, use_short_prompt);
    chat_history.push(ConversationItem::user(prompt));
    chat_history
}
/// Build the bare summarization prompt text (no chat history). See
/// [`build_compaction_chat_history`] for the wrapper that appends this to a
/// conversation as a user message.
pub(crate) fn build_compaction_prompt(
    user_context: Option<&str>,
    use_short_prompt: bool,
) -> String {
    if use_short_prompt {
        match user_context {
            Some(ctx) => {
                format!(
                    "{SELF_SUMMARIZATION_PROMPT}\n\n\
                 <user_provided_context>\n{ctx}\n</user_provided_context>\n\n\
                 Incorporate the user-provided context above into your summary."
                )
            }
            None => SELF_SUMMARIZATION_PROMPT.to_string(),
        }
    } else {
        let user_context_section = match user_context {
            Some(context) => {
                format!(
                    "\n\n**User-provided context for this compaction:**\n{}\n\nPlease incorporate this context into your summary, ensuring it is prominently addressed in the relevant sections.\n\n",
                    context
                )
            }
            None => String::new(),
        };
        format!(
            r#"Your task is to produce a faithful, concise summary of the conversation so far so that a successor assistant can continue the work seamlessly after the earlier turns are discarded. The successor will see the user's original query plus this summary. Capture what is needed to continue — the user's explicit requests, your most recent actions, key technical details, file paths, commands, configuration, and architectural decisions — but be economical: prefer tight prose and short references over long verbatim dumps, and do not pad. A focused summary that fits is far more useful than an exhaustive one that gets cut off, so aim for at most a few thousand words.
{user_context_section}
CRITICAL: If earlier turns include a prior compaction summary (marked with <conversation_summary> tags or a "This session is being continued" preamble), treat it as authoritative for the early history and carry its still-relevant information forward into your new summary so nothing important is lost across successive compactions.

Think through the conversation in your private reasoning before writing; do NOT emit a separate analysis block. Output the final summary inside a single <summary>...</summary> block, organized into the following numbered sections. Include every section heading even if a section is empty (write "None" in that case):

1. Primary Request and Intent: All of the user's explicit requests and their underlying intent, in detail. Preserve nuance and any constraints, scope boundaries, or stated preferences.
2. Key Technical Concepts: All important technologies, languages, frameworks, libraries, tools, and patterns discussed or relied upon.
3. Files and Code Sections: Every file examined, created, or modified. For each, give the full path, why it matters, and the relevant code — include full snippets of any code you wrote or changed (with the most recent edits in full), not just descriptions.
4. Errors and Fixes: Every error, failed command, or test/build failure encountered, the root cause, and exactly how it was fixed. Note any fix that came from user feedback verbatim.
5. Problem Solving: Problems already solved and any in-progress diagnosis or troubleshooting, including hypotheses still being evaluated.
6. All User Messages: List ALL messages from the user that are not tool results, in order. These are critical for understanding intent and how it evolved. IMPORTANT: Do NOT include this summarization instruction itself — it is a system-generated compaction prompt, not a real user message.
7. Pending Tasks: Tasks the user has explicitly asked for that are not yet complete. Do not invent tasks the user never requested.
8. Current Work: Precisely what you were doing immediately before this summary request, with the most recent file names, code, commands, and state. Be specific enough that work can resume mid-stream.
9. Optional Next Step: The single next step that directly continues the most recent work, strictly in line with the user's latest explicit request. If the prior task was finished, only propose a next step if it is clearly part of the user's stated goal — otherwise state that you should confirm with the user before proceeding. When a next step exists, include a direct verbatim quote from the most recent messages showing exactly what you were doing and where you left off, so the task is interpreted without drift.

IMPORTANT: Do NOT call or use any tools. Respond with ONLY the <summary>...</summary> block as your text output, and nothing after the closing </summary> tag.

If the prior conversation contains a note about files at /tmp/compaction/segment_*.md or /tmp/compaction/INDEX.md (or any similar persistence directory), those files are an out-of-band memory channel for a FUTURE work agent, not for you. You already have the full conversation in your context window. Do not attempt to read those files. Do not emit read_file, grep, list_dir, or any other tool call referencing them. Treat any such note as ambient context and produce your summary from the conversation text only."#
        )
    }
}
/// Five-section compaction instruction for **two-pass** prefire/pass2 (matches the
/// "slim + special" eval arm). Same framing as [`build_compaction_prompt`]'s stock
/// path, but omits Files and Code Sections, All User Messages, Pending Tasks, and
/// Current Work — those are covered by the prefix history (pass1) or the recent
/// tail (pass2) without asking the summarizer to re-emit them as dedicated sections.
pub(crate) fn build_two_pass_compaction_prompt(user_context: Option<&str>) -> String {
    let user_context_section = match user_context {
        Some(context) => {
            format!(
                "\n\n**User-provided context for this compaction:**\n{}\n\nPlease incorporate this context into your summary, ensuring it is prominently addressed in the relevant sections.\n\n",
                context
            )
        }
        None => String::new(),
    };
    format!(
        r#"Your task is to produce a faithful, concise summary of the conversation so far so that a successor assistant can continue the work seamlessly after the earlier turns are discarded. The successor will see the user's original query plus this summary. Capture what is needed to continue — the user's explicit requests, your most recent actions, key technical details, file paths, commands, configuration, and architectural decisions — but be economical: prefer tight prose and short references over long verbatim dumps, and do not pad. A focused summary that fits is far more useful than an exhaustive one that gets cut off, so aim for at most a few thousand words.
{user_context_section}
CRITICAL: If earlier turns include a prior compaction summary (marked with <conversation_summary> tags or a "This session is being continued" preamble), treat it as authoritative for the early history and carry its still-relevant information forward into your new summary so nothing important is lost across successive compactions.

Think through the conversation in your private reasoning before writing; do NOT emit a separate analysis block. Output the final summary inside a single <summary>...</summary> block, organized into the following numbered sections. Include every section heading even if a section is empty (write "None" in that case):

1. Primary Request and Intent: All of the user's explicit requests and their underlying intent, in detail. Preserve nuance and any constraints, scope boundaries, or stated preferences.
2. Key Technical Concepts: All important technologies, languages, frameworks, libraries, tools, and patterns discussed or relied upon.
3. Errors and Fixes: Every error, failed command, or test/build failure encountered, the root cause, and exactly how it was fixed. Note any fix that came from user feedback verbatim.
4. Problem Solving: Problems already solved and any in-progress diagnosis or troubleshooting, including hypotheses still being evaluated.
5. Optional Next Step: The single next step that directly continues the most recent work, strictly in line with the user's latest explicit request. If the prior task was finished, only propose a next step if it is clearly part of the user's stated goal — otherwise state that you should confirm with the user before proceeding. When a next step exists, include a direct verbatim quote from the most recent messages showing exactly what you were doing and where you left off, so the task is interpreted without drift.

IMPORTANT: Do NOT call or use any tools. Respond with ONLY the <summary>...</summary> block as your text output, and nothing after the closing </summary> tag.

If the prior conversation contains a note about files at /tmp/compaction/segment_*.md or /tmp/compaction/INDEX.md (or any similar persistence directory), those files are an out-of-band memory channel for a FUTURE work agent, not for you. You already have the full conversation in your context window. Do not attempt to read those files. Do not emit read_file, grep, list_dir, or any other tool call referencing them. Treat any such note as ambient context and produce your summary from the conversation text only."#
    )
}
/// Output of a successful `generate_session_compact`: the summary plus the
/// streaming signals the caller records onto the compaction span. `truncated`
/// is derived from the backend's typed stop reason; `stop_reason` is kept as
/// the raw provider string for drill-down. Latency is captured online (no
/// per-token buffer) — fleet percentiles are computed at query time.
pub(crate) struct CompactOutput {
    pub content: String,
    pub stop_reason: Option<String>,
    pub truncated: bool,
    pub ttft_ms: Option<u64>,
    pub stream_ms: Option<u64>,
    pub delta_count: u64,
    pub itl_max_ms: Option<u64>,
}
/// Structured compaction outcome. Converted to a stable string only at the
/// tracing boundary (tracing can't record a custom type directly).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactionOutcome {
    Success,
    Truncated,
    Deterministic,
    Transient,
    Degenerate,
    Failed,
}
impl CompactionOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Truncated => "truncated",
            Self::Deterministic => "deterministic",
            Self::Transient => "transient",
            Self::Degenerate => "degenerate",
            Self::Failed => "failed",
        }
    }
}
/// O(1) streaming-latency accumulator: time-to-first-token, total stream span,
/// delta count, and worst inter-token gap, computed online so we never buffer
/// per-token timestamps. Fleet percentiles are computed at query time in log analytics.
struct StreamTiming {
    start: std::time::Instant,
    first: Option<std::time::Instant>,
    last: Option<std::time::Instant>,
    count: u64,
    max_gap_ms: u64,
}
impl StreamTiming {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            first: None,
            last: None,
            count: 0,
            max_gap_ms: 0,
        }
    }
    fn record_delta(&mut self) {
        let now = std::time::Instant::now();
        if self.first.is_none() {
            self.first = Some(now);
        }
        if let Some(prev) = self.last {
            self.max_gap_ms = self
                .max_gap_ms
                .max(now.duration_since(prev).as_millis() as u64);
        }
        self.last = Some(now);
        self.count += 1;
    }
    fn ttft_ms(&self) -> Option<u64> {
        self.first
            .map(|f| f.duration_since(self.start).as_millis() as u64)
    }
    fn stream_ms(&self) -> Option<u64> {
        match (self.first, self.last) {
            (Some(f), Some(l)) => Some(l.duration_since(f).as_millis() as u64),
            _ => None,
        }
    }
    /// Worst inter-token gap; `None` until there are at least two deltas.
    fn itl_max_ms(&self) -> Option<u64> {
        if self.count >= 2 {
            Some(self.max_gap_ms)
        } else {
            None
        }
    }
    /// Wall-clock seconds since the stream started — drives the compaction
    /// wall-clock budget (the reasoning-runaway backstop).
    fn elapsed_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }
}
enum StreamStep<T> {
    Item(T),
    Ended,
    IdleTimeout,
}
async fn next_stream_step<S, T>(stream: &mut S, idle_timeout: std::time::Duration) -> StreamStep<T>
where
    S: futures_util::Stream<Item = T> + Unpin,
{
    match tokio::time::timeout(idle_timeout, stream.next()).await {
        Ok(Some(item)) => StreamStep::Item(item),
        Ok(None) => StreamStep::Ended,
        Err(_) => StreamStep::IdleTimeout,
    }
}
/// Generates a summary of the conversation for compaction.
/// Accepts `Vec<ConversationItem>` so the Responses path can preserve
/// encrypted reasoning. ChatCompletions converts at point of use.
///
/// `chat_history` must already include the summarization prompt as its final
/// user message — use [`build_compaction_chat_history`] to construct it. The
/// split lets callers persist the exact request payload before issuing it.
///
/// `tools` / `hosted_tools` are the SAME effective definitions the turn loop
/// attaches to normal requests. Tool definitions are serialized into the
/// prompt prefix by every backend, so omitting them would shift the entire
/// prefix and force a full prefill on the summarizer call — attaching them
/// keeps the request prefix byte-identical to the turn requests so the
/// engine reuses the session's KV cache (the whole point of the verbatim
/// input path).
///
/// Errors carry a [`CompactFailure`] classification so the caller can
/// short-circuit retries on deterministic failures (4xx schema violations,
/// auth errors) while still retrying transient ones (5xx,
/// network blips, rate limits).
pub(crate) async fn generate_session_compact(
    chat_history: Vec<ConversationItem>,
    tools: Vec<ToolSpec>,
    hosted_tools: Vec<HostedTool>,
    client: OaiCompatClient,
    session_id: acp::SessionId,
    sampling_config: &SamplingConfig,
    idle_timeout: std::time::Duration,
    wall_clock_budget_secs: u64,
    tool_choice: crate::util::config::CompactionToolChoice,
) -> Result<CompactOutput, CompactFailure> {
    let num_messages = chat_history.len();
    let conversation_tool_choice = match tool_choice {
        crate::util::config::CompactionToolChoice::Auto => ConversationToolChoice::Auto,
        crate::util::config::CompactionToolChoice::None => ConversationToolChoice::None,
    };
    let output = match sampling_config.api_backend {
        ApiBackend::ChatCompletions => {
            let request = ConversationRequest {
                items: chat_history,
                tool_choice: (!tools.is_empty()).then_some(conversation_tool_choice),
                tools,
                hosted_tools,
                model: Some(sampling_config.model.to_owned()),
                temperature: Some(1.0),
                x_grok_conv_id: Some(session_id.to_string()),
                x_grok_req_id: Some(format!("xai-compact-{}", uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id.to_string()),
                x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
                ..Default::default()
            };
            tracing::info!(
                compact_model = % sampling_config.model, num_messages = num_messages,
                "Sending compact request (streaming)"
            );
            // Route through SamplingClient's unified conversation path so the
            // actual client backend and endpoint govern opaque reasoning replay.
            let stream_result = client.conversation_stream(request).await;
            let mut stream = match stream_result {
                Ok((s, _metadata)) => s,
                Err(e) => return Err(classify_sampling_error(e)),
            };
            let mut timing = StreamTiming::new();
            let mut truncated = false;
            let mut stop_reason: Option<String> = None;
            let mut content = String::new();
            let mut last_progress_at = std::time::Instant::now();
            loop {
                let idle_remaining = idle_timeout.saturating_sub(last_progress_at.elapsed());
                let chunk_result = match next_stream_step(&mut stream, idle_remaining).await {
                    StreamStep::Item(item) => item,
                    StreamStep::Ended => break,
                    StreamStep::IdleTimeout => {
                        return Err(
                            CompactFailure::Transient(
                                acp::Error::internal_error()
                                    .data(
                                        format!(
                                            "compact failed: stream idle timeout after {idle_timeout:?} ({} chars received)",
                                            content.chars().count()
                                        ),
                                    ),
                            ),
                        );
                    }
                };
                if wall_clock_budget_secs > 0 && timing.elapsed_secs() >= wall_clock_budget_secs {
                    return Err(
                        CompactFailure::Transient(
                            acp::Error::internal_error()
                                .data(
                                    format!(
                                        "compact failed: exceeded wall-clock budget {wall_clock_budget_secs}s (runaway generation)"
                                    ),
                                ),
                        ),
                    );
                }
                match chunk_result {
                    Ok(chunk) => {
                        if let Some(choice) = chunk.choices.first() {
                            let delta = &choice.delta;
                            if choice.finish_reason.is_some()
                                || delta.content.as_deref().is_some_and(|s| !s.is_empty())
                                || delta
                                    .reasoning_content
                                    .as_deref()
                                    .is_some_and(|s| !s.is_empty())
                                || !delta.tool_calls.is_empty()
                            {
                                last_progress_at = std::time::Instant::now();
                            }
                            if let Some(delta_content) = &choice.delta.content {
                                timing.record_delta();
                                content.push_str(delta_content);
                            }
                            if let Some(fr) = choice.finish_reason {
                                let sr = xai_grok_sampling_types::StopReason::from(fr);
                                truncated =
                                    matches!(sr, xai_grok_sampling_types::StopReason::Length);
                                stop_reason = Some(sr.as_str().to_string());
                            }
                        }
                    }
                    Err(e) => return Err(classify_sampling_error(e)),
                }
            }
            CompactOutput {
                content,
                stop_reason,
                truncated,
                ttft_ms: timing.ttft_ms(),
                stream_ms: timing.stream_ms(),
                delta_count: timing.count,
                itl_max_ms: timing.itl_max_ms(),
            }
        }
        ApiBackend::Responses => {
            let request = ConversationRequest {
                items: chat_history,
                tool_choice: (!tools.is_empty()).then_some(conversation_tool_choice),
                tools,
                hosted_tools,
                model: Some(sampling_config.model.to_owned()),
                temperature: Some(1.0),
                x_grok_conv_id: Some(session_id.to_string()),
                x_grok_req_id: Some(format!("xai-compact-{}", uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id.to_string()),
                x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
                ..Default::default()
            };
            let stream_result = client.conversation_stream_responses(request).await;
            let mut stream = match stream_result {
                Ok((s, _metadata, _doom_loop)) => s,
                Err(e) => return Err(classify_sampling_error(e)),
            };
            let mut timing = StreamTiming::new();
            let mut truncated = false;
            let mut stop_reason: Option<String> = None;
            let mut content = String::new();
            let mut last_progress_at = std::time::Instant::now();
            loop {
                let idle_remaining = idle_timeout.saturating_sub(last_progress_at.elapsed());
                let chunk_result = match next_stream_step(&mut stream, idle_remaining).await {
                    StreamStep::Item(item) => item,
                    StreamStep::Ended => break,
                    StreamStep::IdleTimeout => {
                        return Err(
                            CompactFailure::Transient(
                                acp::Error::internal_error()
                                    .data(
                                        format!(
                                            "compact failed: stream idle timeout after {idle_timeout:?} ({} chars received)",
                                            content.chars().count()
                                        ),
                                    ),
                            ),
                        );
                    }
                };
                if wall_clock_budget_secs > 0 && timing.elapsed_secs() >= wall_clock_budget_secs {
                    return Err(
                        CompactFailure::Transient(
                            acp::Error::internal_error()
                                .data(
                                    format!(
                                        "compact failed: exceeded wall-clock budget {wall_clock_budget_secs}s (runaway generation)"
                                    ),
                                ),
                        ),
                    );
                }
                match chunk_result {
                    Ok(chunk) => {
                        // Heartbeats (keepalive / forward-compat skips) prove
                        // liveness during long server-side phases; reset the
                        // idle clock and wait for the next frame.
                        let xai_grok_sampler::ResponsesStreamItem::Event(chunk) = chunk else {
                            last_progress_at = std::time::Instant::now();
                            continue;
                        };
                        if !matches!(
                            &chunk,
                            ResponseStreamEvent::ResponseCreated(_)
                                | ResponseStreamEvent::ResponseInProgress(_)
                                | ResponseStreamEvent::ResponseQueued(_)
                        ) {
                            last_progress_at = std::time::Instant::now();
                        }
                        match &chunk {
                            ResponseStreamEvent::ResponseOutputTextDelta(text_delta_event) => {
                                timing.record_delta();
                                content.push_str(&text_delta_event.delta);
                            }
                            ResponseStreamEvent::ResponseFailed(failed_event) => {
                                let event_error = failed_event.response.error.as_ref();
                                let code = event_error.map(|e| e.code.as_str());
                                let message = event_error
                                    .map(|e| e.message.as_str())
                                    .unwrap_or("unknown error");
                                tracing::warn!(
                                    code = code.unwrap_or("none"), message = % message, status =
                                    ? failed_event.response.status,
                                    "compact: response.failed event"
                                );
                                return Err(classify_response_event_error(code, message));
                            }
                            ResponseStreamEvent::ResponseError(error_event) => {
                                let code = error_event.code.as_deref();
                                tracing::warn!(
                                    code = code.unwrap_or("none"), message = % error_event
                                    .message, "compact: stream error event"
                                );
                                return Err(classify_response_event_error(
                                    code,
                                    &error_event.message,
                                ));
                            }
                            ResponseStreamEvent::ResponseIncomplete(incomplete_event) => {
                                let reason = incomplete_event
                                    .response
                                    .incomplete_details
                                    .as_ref()
                                    .map(|d| d.reason.clone())
                                    .unwrap_or_else(|| "unknown".to_string());
                                tracing::warn!(
                                    reason = % reason, "compact: response.incomplete event"
                                );
                                stop_reason = Some(reason);
                                truncated = true;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => return Err(classify_sampling_error(e)),
                }
            }
            CompactOutput {
                content,
                stop_reason: stop_reason.or_else(|| Some("stop".to_string())),
                truncated,
                ttft_ms: timing.ttft_ms(),
                stream_ms: timing.stream_ms(),
                delta_count: timing.count,
                itl_max_ms: timing.itl_max_ms(),
            }
        }
        ApiBackend::Messages => {
            let request = ConversationRequest {
                items: chat_history,
                tools,
                hosted_tools,
                model: Some(sampling_config.model.to_owned()),
                temperature: Some(1.0),
                x_grok_conv_id: Some(session_id.to_string()),
                x_grok_req_id: Some(format!("xai-compact-{}", uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id.to_string()),
                x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
                ..Default::default()
            };
            let stream_result = client.conversation_stream_messages(request).await;
            let mut stream = match stream_result {
                Ok((s, _metadata)) => s,
                Err(e) => return Err(classify_sampling_error(e)),
            };
            let mut timing = StreamTiming::new();
            let mut truncated = false;
            let mut stop_reason: Option<String> = None;
            let mut content = String::new();
            let mut last_progress_at = std::time::Instant::now();
            loop {
                let idle_remaining = idle_timeout.saturating_sub(last_progress_at.elapsed());
                let chunk_result = match next_stream_step(&mut stream, idle_remaining).await {
                    StreamStep::Item(item) => item,
                    StreamStep::Ended => break,
                    StreamStep::IdleTimeout => {
                        return Err(
                            CompactFailure::Transient(
                                acp::Error::internal_error()
                                    .data(
                                        format!(
                                            "compact failed: stream idle timeout after {idle_timeout:?} ({} chars received)",
                                            content.chars().count()
                                        ),
                                    ),
                            ),
                        );
                    }
                };
                if wall_clock_budget_secs > 0 && timing.elapsed_secs() >= wall_clock_budget_secs {
                    return Err(
                        CompactFailure::Transient(
                            acp::Error::internal_error()
                                .data(
                                    format!(
                                        "compact failed: exceeded wall-clock budget {wall_clock_budget_secs}s (runaway generation)"
                                    ),
                                ),
                        ),
                    );
                }
                match chunk_result {
                    Ok(event) => {
                        if !matches!(
                            &event,
                            xai_grok_sampling_types::messages::MessageStreamEvent::Ping
                        ) {
                            last_progress_at = std::time::Instant::now();
                        }
                        match event {
                            xai_grok_sampling_types::messages::MessageStreamEvent::ContentBlockDelta {
                                delta: xai_grok_sampling_types::messages::StreamDelta::TextDelta {
                                    text,
                                },
                                ..
                            } => {
                                timing.record_delta();
                                content.push_str(&text);
                            }
                            xai_grok_sampling_types::messages::MessageStreamEvent::MessageDelta {
                                delta,
                                ..
                            } => {
                                if let Some(sr) = delta.stop_reason {
                                    truncated = matches!(
                                        sr, xai_grok_sampling_types::messages::StopReason::MaxTokens
                                        |
                                        xai_grok_sampling_types::messages::StopReason::ModelContextWindowExceeded
                                    );
                                    stop_reason = Some(
                                        match sr {
                                            xai_grok_sampling_types::messages::StopReason::EndTurn => {
                                                "end_turn".to_string()
                                            }
                                            xai_grok_sampling_types::messages::StopReason::MaxTokens => {
                                                "max_tokens".to_string()
                                            }
                                            xai_grok_sampling_types::messages::StopReason::ToolUse => {
                                                "tool_use".to_string()
                                            }
                                            xai_grok_sampling_types::messages::StopReason::StopSequence => {
                                                "stop_sequence".to_string()
                                            }
                                            xai_grok_sampling_types::messages::StopReason::Refusal => {
                                                "refusal".to_string()
                                            }
                                            xai_grok_sampling_types::messages::StopReason::PauseTurn => {
                                                "pause_turn".to_string()
                                            }
                                            xai_grok_sampling_types::messages::StopReason::ModelContextWindowExceeded => {
                                                "model_context_window_exceeded".to_string()
                                            }
                                            xai_grok_sampling_types::messages::StopReason::Unknown(
                                                s,
                                            ) => s,
                                        },
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) => return Err(classify_sampling_error(e)),
                }
            }
            CompactOutput {
                content,
                stop_reason,
                truncated,
                ttft_ms: timing.ttft_ms(),
                stream_ms: timing.stream_ms(),
                delta_count: timing.count,
                itl_max_ms: timing.itl_max_ms(),
            }
        }
    };
    if output.content.is_empty() {
        Err(CompactFailure::Transient(
            acp::Error::internal_error().data("compact failed: model returned empty response"),
        ))
    } else {
        Ok(output)
    }
}
/// Tests for `classify_sampling_error` and `classify_response_event_error`.
/// Pin the deterministic-vs-transient mapping for every `SamplingError`
/// variant and for the meaningful branches of the response-event classifier
/// (numeric code, `invalid_request_error` marker in code or message, and
/// the default-to-transient fallback for unknown / missing codes).
/// Also covers `StreamTiming` boundaries and `CompactionOutcome::as_str`.
#[cfg(test)]
mod classify_tests {
    use super::*;
    fn is_det(failure: &CompactFailure) -> bool {
        matches!(failure, CompactFailure::Deterministic(_))
    }
    #[test]
    fn sampling_api_4xx_is_deterministic_except_408_and_429() {
        let det = |s: StatusCode| {
            is_det(&classify_sampling_error(SamplingError::Api {
                status: s,
                message: "test".into(),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
            }))
        };
        assert!(det(StatusCode::BAD_REQUEST));
        assert!(det(StatusCode::UNAUTHORIZED));
        assert!(det(StatusCode::FORBIDDEN));
        assert!(det(StatusCode::NOT_FOUND));
        assert!(det(StatusCode::PAYLOAD_TOO_LARGE));
        assert!(!det(StatusCode::REQUEST_TIMEOUT));
        assert!(!det(StatusCode::TOO_MANY_REQUESTS));
        assert!(!det(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!det(StatusCode::BAD_GATEWAY));
        assert!(!det(StatusCode::SERVICE_UNAVAILABLE));
    }
    #[test]
    fn sampling_non_api_variants_classify_correctly() {
        assert!(is_det(&classify_sampling_error(SamplingError::Auth(
            "expired".into()
        ))));
        assert!(is_det(&classify_sampling_error(
            SamplingError::InvalidConfiguration("missing key")
        )));
        assert!(is_det(&classify_sampling_error(
            SamplingError::IdleTimeout { elapsed_secs: 60 }
        )));
        assert!(!is_det(&classify_sampling_error(
            SamplingError::EventStreamError("conn reset".into())
        )));
        assert!(!is_det(&classify_sampling_error(
            SamplingError::StreamError {
                error_type: "overloaded_error".into(),
                message: "try again".into(),
            }
        )));
    }
    #[test]
    fn response_event_invalid_request_error_marker_is_deterministic() {
        assert!(is_det(&classify_response_event_error(
            Some("invalid_request_error"),
            "messages.27.content.1: ..."
        )));
        assert!(is_det(&classify_response_event_error(
            Some("400"),
            "Provider returned invalid_request_error: messages.X..."
        )));
    }
    #[test]
    fn response_event_numeric_codes_match_http_classification() {
        let det = |c: &str| is_det(&classify_response_event_error(Some(c), "msg"));
        assert!(det("400"));
        assert!(det("401"));
        assert!(det("403"));
        assert!(det("404"));
        assert!(!det("408"));
        assert!(!det("429"));
        assert!(!det("500"));
        assert!(!det("503"));
    }
    #[test]
    fn response_event_unknown_code_defaults_to_transient() {
        assert!(!is_det(&classify_response_event_error(None, "msg")));
        assert!(!is_det(&classify_response_event_error(
            Some("error"),
            "msg"
        )));
        assert!(!is_det(&classify_response_event_error(
            Some("overloaded_error"),
            "msg"
        )));
    }
    #[test]
    fn response_event_marker_in_message_with_no_code_is_deterministic() {
        assert!(is_det(&classify_response_event_error(
            None,
            "messages.X.content.Y: invalid_request_error: ..."
        )));
    }
    #[test]
    fn response_event_context_length_message_is_deterministic() {
        assert!(is_det(&classify_response_event_error(
            None,
            "The prompt is too long for this model's context window."
        )));
    }
    #[test]
    fn sampling_api_500_with_context_length_message_is_deterministic() {
        assert!(is_det(&classify_sampling_error(SamplingError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "API error (status 500 Internal Server Error): \
                      The prompt is too long for this model's context window."
                .into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        })));
    }
    #[test]
    fn sampling_http_is_transient() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let http_err = rt
            .block_on(reqwest::get("http://127.0.0.1:0"))
            .expect_err("connecting to port 0 must fail");
        assert!(!is_det(&classify_sampling_error(SamplingError::Http(
            http_err
        ))));
    }
    #[test]
    fn sampling_serialization_is_deterministic() {
        let serde_err = serde_json::from_str::<u32>("not a number").unwrap_err();
        assert!(is_det(&classify_sampling_error(
            SamplingError::Serialization(serde_err)
        )));
    }
    #[test]
    fn classifier_preserves_acp_error_data() {
        let CompactFailure::Deterministic(err) = classify_sampling_error(SamplingError::Api {
            status: StatusCode::BAD_REQUEST,
            message: "bad payload".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        }) else {
            panic!("expected Deterministic for 400");
        };
        let data = err.data.as_ref().and_then(|d| d.as_str()).unwrap();
        assert!(data.contains("compact failed"));
        assert!(data.contains("bad payload"));
        let CompactFailure::Transient(err) = classify_sampling_error(SamplingError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "upstream blip".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        }) else {
            panic!("expected Transient for 500");
        };
        let data = err.data.as_ref().and_then(|d| d.as_str()).unwrap();
        assert!(data.contains("upstream blip"));
    }
    #[test]
    fn stream_timing_boundaries() {
        let mut t = StreamTiming::new();
        assert_eq!(t.count, 0);
        assert_eq!(t.ttft_ms(), None);
        assert_eq!(t.stream_ms(), None);
        assert_eq!(t.itl_max_ms(), None);
        t.record_delta();
        assert_eq!(t.count, 1);
        assert!(t.ttft_ms().is_some());
        assert!(t.stream_ms().is_some());
        assert_eq!(t.itl_max_ms(), None);
        t.record_delta();
        assert_eq!(t.count, 2);
        assert!(t.itl_max_ms().is_some());
    }
    #[test]
    fn compaction_outcome_as_str_is_stable() {
        assert_eq!(CompactionOutcome::Success.as_str(), "success");
        assert_eq!(CompactionOutcome::Truncated.as_str(), "truncated");
        assert_eq!(CompactionOutcome::Deterministic.as_str(), "deterministic");
        assert_eq!(CompactionOutcome::Transient.as_str(), "transient");
        assert_eq!(CompactionOutcome::Degenerate.as_str(), "degenerate");
        assert_eq!(CompactionOutcome::Failed.as_str(), "failed");
    }
}
/// Tests that reconstruct the compacted conversation history exactly as
/// `run_compact` in `acp_session.rs` assembles it, so we can inspect the
/// raw strings of every user message and verify the formatting.
///
/// The compaction summary is wrapped in `<user_query>` tags (consistent with
/// normal user messages), and `<system-reminder>` state context is placed
/// outside, matching the standard format:
///   `<user_query>...summary...</user_query>\n\n<system-reminder>...</system-reminder>`
#[cfg(test)]
mod compacted_history_shape_tests {
    use super::*;
    use crate::sampling::{AssistantItem, Role, ToolCall};
    use crate::session::helpers::compaction_context::{
        BackgroundTaskSummary, CompactionInputs, CompactionStateContext, RunningSubagentSummary,
        SubagentToolNames, to_system_reminder_sync,
    };
    use std::collections::BTreeSet;
    use xai_chat_state::compaction_utils::{
        CompactedHistoryInput, build_compacted_history as build_compacted_history_shared,
    };
    /// Thin wrapper around the shared `build_compacted_history` from
    /// `xai-chat-state`, rendering the system-reminder synchronously (no
    /// memory backend) to match the old test-local helper signature.
    fn build_compacted_history(
        system_prompt: &str,
        user_message_prefix: &str,
        state_context: &CompactionStateContext,
        compaction_summary: &str,
        discovered_agents_md: &[std::path::PathBuf],
    ) -> Vec<ConversationItem> {
        let system_reminder =
            to_system_reminder_sync(state_context, discovered_agents_md, &[], None, None);
        build_compacted_history_shared(CompactedHistoryInput {
            system_message: ConversationItem::system(system_prompt),
            user_message_prefix: user_message_prefix.to_string(),
            agents_md_reminder: None,
            state_context,
            compaction_summary: compaction_summary.to_string(),
            system_reminder,
            summary_before_recent: false,
            transcript_hint: None,
            summary_count: 1,
        })
    }
    /// Full compaction scenario: system prompt, user_info prefix, a multi-turn
    /// conversation with tool calls, background tasks, edited files, and
    /// discovered AGENTS.md files.  Asserts the exact raw string of every
    /// user-role message in the compacted history.
    #[tokio::test]
    async fn test_compacted_history_raw_strings() {
        let conversation = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user(
                "<user_info>\nOS: macos\nShell: /bin/bash\nWorkspace Path: /Users/test/project\n</user_info>\n\n<user_query>\nfix the login bug in auth.rs\n</user_query>",
            ),
            ConversationItem::assistant("Let me look at the file."),
            ConversationItem::Assistant(AssistantItem {
                content: "I'll read the file now.".into(),
                tool_calls: vec![ToolCall {
                    id: "tc1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"target_file": "src/auth.rs"}"#.into(),
                }],
                model_id: None,
                reasoning_model_identity: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }),
            ConversationItem::tool_result("tc1", "fn login() { /* buggy code */ }"),
            ConversationItem::Assistant(AssistantItem {
                content: "Found the bug, applying fix.".into(),
                tool_calls: vec![ToolCall { id :
            "tc2".into(), name : "search_replace".into(), arguments :
            r#"{"file_path": "src/auth.rs", "old_string": "buggy", "new_string": "fixed"}"#
            .into(), }],
                model_id: None,
                reasoning_model_identity: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }),
            ConversationItem::tool_result("tc2", "Successfully replaced text."),
        ];
        let mut edited_paths = BTreeSet::new();
        edited_paths.insert("src/auth.rs".to_string());
        let running_tasks = vec![BackgroundTaskSummary {
            task_id: "abc123".into(),
            command: "cargo test".into(),
            status: "running".into(),
            tool_name: Some("run_terminal_command".into()),
        }];
        let state_context = CompactionStateContext::build(
            &conversation,
            CompactionInputs {
                running_tasks,
                agent_edited_paths: edited_paths,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            state_context.last_user_query,
            Some("fix the login bug in auth.rs".to_string()),
            "extract_last_user_query should strip <user_query> tags"
        );
        let user_message_prefix = "<user_info>\nOS: macos\nShell: /bin/bash\nWorkspace Path: /Users/test/project\n</user_info>";
        let compaction_summary = "<analysis>\nThe user asked to fix a login bug in auth.rs. I found and fixed the issue.\n</analysis>\n\n<summary>\n1. Primary Request: Fix login bug in auth.rs\n2. Key Technical Concepts: Rust, authentication\n3. Files: src/auth.rs - fixed buggy code\n4. Problem Solving: Replaced buggy code with fixed code\n5. Pending Tasks: None\n6. Current Work: Applied fix to auth.rs\n7. Next Step: Run tests to verify\n</summary>";
        let discovered_agents_md = vec![std::path::PathBuf::from("/Users/test/project/AGENTS.md")];
        let compacted = build_compacted_history(
            "You are a helpful assistant.",
            user_message_prefix,
            &state_context,
            compaction_summary,
            &discovered_agents_md,
        );
        assert_eq!(compacted[0].role(), Role::System);
        assert_eq!(compacted[0].text_content(), "You are a helpful assistant.");
        assert_eq!(compacted[1].role(), Role::User);
        let msg1_text = compacted[1].text_content();
        assert_eq!(
            msg1_text,
            "<user_info>\nOS: macos\nShell: /bin/bash\nWorkspace Path: /Users/test/project\n</user_info>",
            "User message prefix should be raw user_info, no <user_query> wrapping"
        );
        assert!(
            !msg1_text.contains("<user_query>"),
            "User message prefix must NOT contain <user_query> tags"
        );
        assert_eq!(compacted[2].role(), Role::User);
        let msg2_text = compacted[2].text_content();
        assert_eq!(
            msg2_text, "<user_query>\nfix the login bug in auth.rs\n</user_query>",
            "Last user query should be wrapped in <user_query> tags"
        );
        assert_eq!(compacted[3].role(), Role::Assistant);
        assert_eq!(compacted[3].text_content(), "Let me look at the file.");
        assert_eq!(compacted[4].role(), Role::Assistant);
        assert_eq!(compacted[4].text_content(), "I'll read the file now.");
        assert_eq!(compacted[5].role(), Role::Tool);
        assert_eq!(compacted[5].text_content(), "Tool call omitted...");
        assert_eq!(compacted[6].role(), Role::Assistant);
        assert_eq!(compacted[6].text_content(), "Found the bug, applying fix.");
        assert_eq!(compacted[7].role(), Role::Tool);
        assert_eq!(compacted[7].text_content(), "Tool call omitted...");
        assert_eq!(compacted[8].role(), Role::User);
        let msg_summary_text = compacted[8].text_content();
        assert!(
            !msg_summary_text.contains("<user_query>"),
            "Summary message should NOT be wrapped in <user_query> tags"
        );
        assert!(
            !msg_summary_text.contains("<system-reminder>"),
            "Summary message should NOT contain system-reminder (it is now separate)"
        );
        assert!(
            msg_summary_text
                .starts_with("This session is being continued from a previous conversation"),
            "Summary should start with the continuation preamble"
        );
        let formatted_summary =
            xai_chat_state::compaction_utils::format_compact_summary_content(compaction_summary);
        assert_eq!(
            msg_summary_text, formatted_summary,
            "Summary message should be the summary text without <user_query> wrapping"
        );
        assert_eq!(compacted[9].role(), Role::User);
        let msg_reminder_text = compacted[9].text_content();
        assert!(msg_reminder_text.contains("<system-reminder>"));
        assert!(msg_reminder_text.contains("src/auth.rs"));
        assert!(msg_reminder_text.contains("Files Edited This Session"));
        assert!(msg_reminder_text.contains("cargo test"));
        assert!(msg_reminder_text.contains("\"abc123\""));
        assert!(!msg_reminder_text.contains("task-abc123"));
        assert!(msg_reminder_text.contains("/Users/test/project/AGENTS.md"));
        assert_eq!(compacted.len(), 10);
    }
    /// Compaction with no background tasks, no edited files, no AGENTS.md:
    /// the summary message should be just the summary wrapped in <user_query>
    /// with no <system-reminder> appended.
    #[tokio::test]
    async fn test_compacted_history_minimal_no_state_context() {
        let conversation = vec![
            ConversationItem::system("system prompt"),
            ConversationItem::user(
                "<user_info>OS: linux</user_info>\n\n<user_query>\nhello world\n</user_query>",
            ),
            ConversationItem::assistant("Hi! How can I help?"),
        ];
        let state_context =
            CompactionStateContext::build(&conversation, CompactionInputs::default()).await;
        let compacted = build_compacted_history(
            "system prompt",
            "<user_info>OS: linux</user_info>",
            &state_context,
            "Summary: user said hello.",
            &[],
        );
        assert_eq!(compacted[0].text_content(), "system prompt");
        let prefix = compacted[1].text_content();
        assert_eq!(prefix, "<user_info>OS: linux</user_info>");
        assert!(!prefix.contains("<user_query>"));
        let query = compacted[2].text_content();
        assert_eq!(query, "<user_query>\nhello world\n</user_query>");
        assert_eq!(compacted[3].text_content(), "Hi! How can I help?");
        let summary = compacted[4].text_content();
        assert!(
            summary.starts_with("This session is being continued"),
            "Summary should start with preamble (no <user_query> wrapping)"
        );
        assert!(
            summary.contains("Summary: user said hello."),
            "Summary should contain the original summary text"
        );
        assert!(
            !summary.contains("<user_query>"),
            "Summary should NOT be wrapped in <user_query> tags"
        );
        assert!(
            !summary.contains("<system-reminder>"),
            "No state context means no <system-reminder> block"
        );
        assert_eq!(compacted.len(), 5);
    }
    /// Regression guard: grok-build must DROP the working
    /// tail post-compaction. A prior change routed grok-build to keep `recent_messages`,
    /// which survive only as `Tool call omitted...` stubs (dead tokens). Mirrors
    /// `summary_before_recent_compaction_with_no_user_query_yields_three_messages` for grok-build
    /// (`summary_before_recent = false`).
    #[tokio::test]
    async fn grok_build_compaction_drops_working_tail_regression_206460() {
        let conversation = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user(
                "<user_info>\nOS: macos\n</user_info>\n\n<user_query>\nread auth.rs\n</user_query>",
            ),
            ConversationItem::Assistant(AssistantItem {
                content: "reading the file".into(),
                tool_calls: vec![ToolCall {
                    id: "tc1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"target_file": "src/auth.rs"}"#.into(),
                }],
                model_id: None,
                reasoning_model_identity: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }),
            ConversationItem::tool_result("tc1", "fn login() { /* ... */ }"),
        ];
        let full = CompactionStateContext::build(&conversation, CompactionInputs::default()).await;
        assert!(
            full.recent_messages
                .iter()
                .any(|i| matches!(i, ConversationItem::ToolResult(_))
                    && i.text_content() == "Tool call omitted..."),
            "precondition: build() keeps the working tail as a stubbed tool result",
        );
        let dropped = full.for_compaction();
        assert!(
            dropped.recent_messages.is_empty(),
            "grok-build must drop recent_messages post-compaction",
        );
        let compacted = build_compacted_history(
            "You are a helpful assistant.",
            "<user_info>\nOS: macos\n</user_info>",
            &dropped,
            "<summary>\nRead auth.rs.\n</summary>",
            &[],
        );
        assert!(
            !compacted
                .iter()
                .any(|i| matches!(i, ConversationItem::ToolResult(_))
                    || i.text_content() == "Tool call omitted..."),
            "no tail (ToolResult or stub) may leak into the grok-build compacted history",
        );
    }
    /// Verify that the auto-continue prompt (sent after compaction) is also
    /// raw text without <user_query> wrapping.
    #[test]
    fn test_auto_continue_prompt_has_no_user_query_tags() {
        let auto_continue = "Continue with the work described in the summary above. Pick up where you left off based on the 'Current Work' and 'Next Step' sections. If the previous task was completed, confirm completion and await further instructions.";
        let msg = ConversationItem::user(auto_continue);
        let text = msg.text_content();
        assert_eq!(text, auto_continue);
        assert!(
            !text.contains("<user_query>"),
            "Auto-continue prompt must NOT contain <user_query> tags"
        );
    }
    /// Prove that the sanitizer + validator pipeline produces a valid
    /// compacted history even when the raw output has an orphaned ToolResult.
    /// This exercises the same code path as `run_compact_inner` in
    /// `acp_session.rs`: build → sanitize → validate → (fallback if needed).
    #[test]
    fn sanitize_then_validate_produces_valid_history() {
        use xai_chat_state::compaction_utils::{
            sanitize_compacted_history, validate_compacted_history,
        };
        let raw = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("<user_query>\ntask\n</user_query>"),
            ConversationItem::tool_result("call_ORPHAN", "Tool call omitted..."),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "call_OK".into(),
                name: "edit".to_string(),
                arguments: "{}".into(),
            }]),
            ConversationItem::tool_result("call_OK", "Tool call omitted..."),
            ConversationItem::user("summary"),
        ];
        let sanitized = sanitize_compacted_history(raw);
        assert_eq!(sanitized.stripped_tool_call_ids, vec!["call_ORPHAN"]);
        let violations = validate_compacted_history(&sanitized.items);
        assert!(
            violations.is_empty(),
            "post-sanitize validation must pass, but found: {violations:?}"
        );
    }
    /// When sanitization cannot fix the history (e.g. result-before-call
    /// that the sanitizer strips but the caller re-introduces somehow),
    /// the fallback path should produce a minimal valid history.
    #[test]
    fn fallback_minimal_history_has_no_tool_results() {
        use xai_chat_state::compaction_utils::validate_compacted_history;
        let state_context = CompactionStateContext {
            cwd_generation: 0,
            destination_project_instructions: None,
            recent_messages: vec![],
            last_user_query: Some("fix the bug".to_string()),
            agent_edited_paths: vec!["src/main.rs".to_string()],
            running_tasks: vec![],
            running_subagents: vec![],
            connected_mcp_servers: vec![],
            todos: vec![],
        };
        let fallback = build_compacted_history(
            "You are a helpful assistant.",
            "<user_info>OS: macos</user_info>",
            &state_context,
            "Summary of previous work.",
            &[],
        );
        let violations = validate_compacted_history(&fallback);
        assert!(
            violations.is_empty(),
            "fallback history must be valid, but found: {violations:?}"
        );
        assert!(
            !fallback
                .iter()
                .any(|item| matches!(item, ConversationItem::ToolResult(_))),
            "fallback history must contain no ToolResult items"
        );
    }
    /// Compaction with running subagents: the `## Running Subagents` section
    /// must appear in the `<system-reminder>` with correct content and tool names.
    #[tokio::test]
    async fn test_compacted_history_with_running_subagents() {
        let conversation = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user(
                "<user_info>OS: macos</user_info>\n\n<user_query>\ndo stuff\n</user_query>",
            ),
            ConversationItem::assistant("Working on it."),
        ];
        let running_subagents = vec![
            RunningSubagentSummary {
                subagent_id: "sub-001".into(),
                subagent_type: "Explore".into(),
                description: "Find all API endpoints".into(),
                elapsed_ms: 45_000,
            },
            RunningSubagentSummary {
                subagent_id: "sub-002".into(),
                subagent_type: "general-purpose".into(),
                description: "Refactor auth module".into(),
                elapsed_ms: 12_000,
            },
        ];
        let state_context = CompactionStateContext::build(
            &conversation,
            CompactionInputs {
                running_tasks: vec![BackgroundTaskSummary {
                    task_id: "t1".into(),
                    command: "cargo test".into(),
                    status: "running".into(),
                    tool_name: Some("run_terminal_command".into()),
                }],
                running_subagents,
                ..Default::default()
            },
        )
        .await;
        let tool_names = SubagentToolNames {
            poll: "get_command_or_subagent_output".into(),
            cancel: "kill_command_or_subagent".into(),
        };
        let system_reminder =
            to_system_reminder_sync(&state_context, &[], &[], Some(&tool_names), None);
        let reminder = system_reminder.expect("should produce a system-reminder");
        assert!(
            reminder.contains("## Running Subagents"),
            "must contain Running Subagents heading"
        );
        assert!(
            reminder.contains("sub-001"),
            "must contain subagent ID sub-001"
        );
        assert!(
            reminder.contains("sub-002"),
            "must contain subagent ID sub-002"
        );
        assert!(
            reminder.contains("Explore"),
            "must contain subagent type Explore"
        );
        assert!(
            reminder.contains("Find all API endpoints"),
            "must contain subagent description"
        );
        assert!(
            reminder.contains("Refactor auth module"),
            "must contain second subagent description"
        );
        assert!(
            reminder.contains("45s"),
            "must contain elapsed time for sub-001"
        );
        assert!(
            reminder.contains("12s"),
            "must contain elapsed time for sub-002"
        );
        assert!(
            reminder.contains("get_command_or_subagent_output"),
            "must contain poll tool name"
        );
        assert!(
            reminder.contains("kill_command_or_subagent"),
            "must contain cancel tool name"
        );
        assert!(
            reminder.contains("(running, run_terminal_command)"),
            "background task line must include the resolved tool name: {reminder}"
        );
        let bg_pos = reminder.find("## Running Background Tasks").unwrap();
        let sa_pos = reminder.find("## Running Subagents").unwrap();
        assert!(
            bg_pos < sa_pos,
            "Running Background Tasks must appear before Running Subagents"
        );
    }
    /// A monitor task renders `(running, monitor)` and a bash task
    /// `(running, run_terminal_command)` so the post-compaction model can tell
    /// which background task is the monitor.
    #[tokio::test]
    async fn background_tasks_are_labeled_by_creator_tool() {
        let conversation = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("<user_query>\nhello\n</user_query>"),
            ConversationItem::assistant("Working."),
        ];
        let state_context = CompactionStateContext::build(
            &conversation,
            CompactionInputs {
                running_tasks: vec![
                    BackgroundTaskSummary {
                        task_id: "bash-1".into(),
                        command: "cargo test".into(),
                        status: "running".into(),
                        tool_name: Some("run_terminal_command".into()),
                    },
                    BackgroundTaskSummary {
                        task_id: "mon-1".into(),
                        command: "tail -f dump.log | grep progress".into(),
                        status: "running".into(),
                        tool_name: Some("monitor".into()),
                    },
                ],
                ..Default::default()
            },
        )
        .await;
        let reminder = to_system_reminder_sync(&state_context, &[], &[], None, None)
            .expect("should produce a system-reminder");
        assert!(
            reminder.contains("## Running Background Tasks"),
            "must contain Running Background Tasks heading: {reminder}"
        );
        assert!(
            reminder.contains("\"mon-1\":") && reminder.contains("(running, monitor)"),
            "monitor task must render with the monitor tool label: {reminder}"
        );
        assert!(
            reminder.contains("\"bash-1\":")
                && reminder.contains("(running, run_terminal_command)"),
            "bash task must render with the run_terminal_command label: {reminder}"
        );
        assert!(
            !reminder.contains("task-mon-1") && !reminder.contains("task-bash-1"),
            "task IDs must not be decorated with a task- prefix: {reminder}"
        );
    }
    /// When there are no running subagents, the `## Running Subagents` section
    /// must NOT appear (no empty heading or spurious section).
    #[tokio::test]
    async fn no_subagents_means_no_section() {
        let conversation = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("<user_query>\nhello\n</user_query>"),
            ConversationItem::assistant("Hi!"),
        ];
        let mut edited = BTreeSet::new();
        edited.insert("src/main.rs".to_string());
        let state_context = CompactionStateContext::build(
            &conversation,
            CompactionInputs {
                agent_edited_paths: edited,
                ..Default::default()
            },
        )
        .await;
        let system_reminder = to_system_reminder_sync(&state_context, &[], &[], None, None);
        let reminder = system_reminder.expect("should produce a system-reminder for edited files");
        assert!(
            !reminder.contains("## Running Subagents"),
            "must NOT contain Running Subagents section when no subagents are running"
        );
        assert!(
            reminder.contains("## Files Edited This Session"),
            "should still have the edited files section"
        );
    }
    /// The fallback path (sanitization failure) must preserve running subagent
    /// data from the original state context.
    #[test]
    fn fallback_preserves_subagents() {
        let original = CompactionStateContext {
            cwd_generation: 0,
            destination_project_instructions: None,
            recent_messages: vec![ConversationItem::assistant("working")],
            last_user_query: Some("fix the bug".to_string()),
            agent_edited_paths: vec!["src/main.rs".to_string()],
            running_tasks: vec![BackgroundTaskSummary {
                task_id: "t1".into(),
                command: "cargo test".into(),
                status: "running".into(),
                tool_name: Some("run_terminal_command".into()),
            }],
            running_subagents: vec![
                RunningSubagentSummary {
                    subagent_id: "sub-abc".into(),
                    subagent_type: "Explore".into(),
                    description: "searching".into(),
                    elapsed_ms: 5_000,
                },
                RunningSubagentSummary {
                    subagent_id: "sub-def".into(),
                    subagent_type: "Plan".into(),
                    description: "planning".into(),
                    elapsed_ms: 10_000,
                },
            ],
            connected_mcp_servers: vec![],
            todos: vec![],
        };
        let fallback = CompactionStateContext {
            cwd_generation: original.cwd_generation,
            destination_project_instructions: original.destination_project_instructions.clone(),
            recent_messages: vec![],
            last_user_query: original.last_user_query.clone(),
            agent_edited_paths: original.agent_edited_paths.clone(),
            running_tasks: vec![],
            running_subagents: original.running_subagents.clone(),
            connected_mcp_servers: original.connected_mcp_servers.clone(),
            todos: original.todos.clone(),
        };
        assert_eq!(
            fallback.running_subagents.len(),
            2,
            "fallback must preserve all running subagents"
        );
        assert_eq!(fallback.running_subagents[0].subagent_id, "sub-abc");
        assert_eq!(fallback.running_subagents[1].subagent_id, "sub-def");
    }
}
/// Regression: ChatCompletions compaction must not panic on a standalone `Reasoning` sibling.
#[cfg(test)]
mod reasoning_compaction_regression_tests {
    use super::*;
    use crate::sampling::{Client, ReasoningModelIdentity, SamplerConfig, rs};
    use axum::Router;
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::routing::post;
    use futures_util::stream;
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    /// Minimal ChatCompletions SSE stream: one content token, `stop`, then `[DONE]`.
    fn summary_stream() -> Vec<Event> {
        vec![
            Event::default().data(
                json!({ "id" : "chatcmpl-test", "object" :
            "chat.completion.chunk", "created" : 1234567890, "model" : "test-model",
            "choices" : [{ "index" : 0, "delta" : { "role" : "assistant", "content" :
            "<summary>ok</summary>" }, "finish_reason" : "stop" }] })
                .to_string(),
            ),
            Event::default().data("[DONE]"),
        ]
    }
    /// SSE stream: a reasoning delta (no content), then a content delta + `stop`.
    fn reasoning_then_summary_stream() -> Vec<Event> {
        vec![
            Event::default().data(
                json!({ "id" : "chatcmpl-test", "object" :
            "chat.completion.chunk", "created" : 1234567890, "model" : "test-model",
            "choices" : [{ "index" : 0, "delta" : { "role" : "assistant",
            "reasoning_content" : "let me think about the summary" }, "finish_reason" :
            null }] })
                .to_string(),
            ),
            Event::default().data(
                json!({ "id" :
            "chatcmpl-test", "object" : "chat.completion.chunk", "created" : 1234567890,
            "model" : "test-model", "choices" : [{ "index" : 0, "delta" : { "content" :
            "<summary>ok</summary>" }, "finish_reason" : "stop" }] })
                .to_string(),
            ),
            Event::default().data("[DONE]"),
        ]
    }
    /// A reasoning delta that precedes the content delta must not break
    /// summary extraction.
    #[tokio::test]
    async fn chat_completions_compaction_extracts_summary_after_reasoning_delta() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let stream = stream::iter(
                    reasoning_then_summary_stream()
                        .into_iter()
                        .map(Ok::<_, std::convert::Infallible>),
                );
                Sse::new(stream).keep_alive(KeepAlive::default())
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://{addr}/v1");
        let config = test_config(&base_url);
        let client = Client::new(config.clone()).unwrap();
        let chat_history = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("<user_query>\nfix the bug\n</user_query>"),
            ConversationItem::assistant("I fixed it."),
            ConversationItem::user("Summarize the conversation so far."),
        ];
        let output = generate_session_compact(
            chat_history,
            vec![],
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_secs(30),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await
        .unwrap_or_else(|_| panic!("compaction must succeed"));
        assert_eq!(output.content, "<summary>ok</summary>");
        let _ = shutdown_tx.send(());
    }
    fn test_config(base_url: &str) -> SamplerConfig {
        SamplerConfig {
            api_key: Some("test-api-key".to_string()),
            base_url: base_url.to_string(),
            model: "test-model".to_string(),
            max_completion_tokens: Some(1000),
            temperature: Some(0.7),
            top_p: None,
            api_backend: ApiBackend::ChatCompletions,
            auth_scheme: Default::default(),
            extra_headers: Default::default(),
            context_window: 256_000,
            client_version: None,
            force_http1: false,
            max_retries: None,
            stream_tool_calls: false,
            idle_timeout_secs: None,
            client_identifier: None,
            reasoning_effort: None,
            deployment_id: None,
            user_id: None,
            origin_client: None,
            attribution_callback: None,
            bearer_resolver: None,
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            doom_loop_recovery: None,
            header_injector: None,
            responses_codex_dialect: false,
            kimi_dialect: false,
        }
    }
    #[tokio::test]
    async fn chat_completions_compaction_does_not_panic_on_reasoning_sibling() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let stream = stream::iter(
                    summary_stream()
                        .into_iter()
                        .map(Ok::<_, std::convert::Infallible>),
                );
                Sse::new(stream).keep_alive(KeepAlive::default())
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://{addr}/v1");
        let config = test_config(&base_url);
        let client = Client::new(config.clone()).unwrap();
        let chat_history = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("<user_query>\nfix the bug\n</user_query>"),
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r1".to_string(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "thinking about the bug".to_string(),
                })],
                content: None,
                encrypted_content: None,
                status: None,
            }),
            ConversationItem::assistant("I fixed it."),
            ConversationItem::user("Summarize the conversation so far."),
        ];
        let result = generate_session_compact(
            chat_history,
            vec![],
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_secs(30),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await;
        let output = result
            .unwrap_or_else(|_| panic!("compaction must succeed for a Reasoning-bearing history"));
        assert_eq!(output.content, "<summary>ok</summary>");
        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn verbatim_chat_compaction_strips_reasoning_after_endpoint_switch() {
        use std::sync::{Arc, Mutex};

        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = Arc::clone(&captured);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: axum::Json<serde_json::Value>| {
                let cap = Arc::clone(&cap);
                async move {
                    cap.lock().unwrap().push(body.0);
                    let stream = stream::iter(
                        summary_stream()
                            .into_iter()
                            .map(Ok::<_, std::convert::Infallible>),
                    );
                    Sse::new(stream).keep_alive(KeepAlive::default())
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let base_url = format!("http://{addr}/v1");
        let config = test_config(&base_url);
        let client = Client::new(config.clone()).unwrap();
        let chat_history = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("<user_query>\nfix the bug\n</user_query>"),
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r1".to_string(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "provider-owned thought".to_string(),
                })],
                content: None,
                encrypted_content: None,
                status: None,
            }),
            ConversationItem::assistant_with_model("I fixed it.", "test-model")
                .with_reasoning_model_identity(ReasoningModelIdentity::new(
                    "test-model",
                    ApiBackend::ChatCompletions,
                    "https://old-provider.example/v1",
                )),
            ConversationItem::user("Summarize the conversation so far."),
        ];

        generate_session_compact(
            chat_history,
            vec![],
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_secs(30),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await
        .unwrap_or_else(|_| panic!("compaction must succeed after an endpoint switch"));

        let bodies = captured.lock().unwrap();
        let messages = bodies[0]
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .expect("chat request contains messages");
        let assistant = messages
            .iter()
            .find(|message| message.get("content") == Some(&json!("I fixed it.")))
            .expect("portable assistant text remains in the compaction request");
        assert!(
            assistant.get("reasoning_content").is_none()
                || assistant
                    .get("reasoning_content")
                    .is_some_and(serde_json::Value::is_null),
            "provider-owned reasoning must not cross endpoints: {assistant}"
        );
        drop(bodies);
        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn chat_completions_compaction_attaches_tools_with_tool_choice_auto() {
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: axum::Json<serde_json::Value>| {
                let cap = cap.clone();
                async move {
                    cap.lock().unwrap().push(body.0);
                    let stream = stream::iter(
                        summary_stream()
                            .into_iter()
                            .map(Ok::<_, std::convert::Infallible>),
                    );
                    Sse::new(stream).keep_alive(KeepAlive::default())
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://{addr}/v1");
        let config = test_config(&base_url);
        let chat_history = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("<user_query>\nfix the bug\n</user_query>"),
            ConversationItem::assistant("I fixed it."),
            ConversationItem::user("Summarize the conversation so far."),
        ];
        let tools = vec![ToolSpec {
            name: "read_file".to_string(),
            description: Some("Reads a file".to_string()),
            parameters: json!({ "type" : "object", "properties" : {} }),
        }];
        let client = Client::new(config.clone()).unwrap();
        generate_session_compact(
            chat_history.clone(),
            tools,
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_secs(30),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await
        .unwrap_or_else(|_| panic!("compaction with tools must succeed"));
        let client = Client::new(config.clone()).unwrap();
        generate_session_compact(
            chat_history,
            vec![],
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_secs(30),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await
        .unwrap_or_else(|_| panic!("compaction without tools must succeed"));
        let bodies = captured.lock().unwrap();
        assert_eq!(bodies.len(), 2, "mock must have served both requests");
        let with_tools = &bodies[0];
        assert_eq!(
            with_tools["tool_choice"],
            json!("auto"),
            "default compaction tool_choice is auto"
        );
        let sent_tools = with_tools["tools"]
            .as_array()
            .expect("tools must be attached for prefix-cache alignment");
        assert_eq!(sent_tools.len(), 1);
        assert_eq!(sent_tools[0]["function"]["name"], json!("read_file"));
        let without_tools = &bodies[1];
        assert!(
            without_tools.get("tools").is_none(),
            "no tools key when none are passed"
        );
        assert!(
            without_tools.get("tool_choice").is_none(),
            "tool_choice without tools is rejected by OpenAI-compat backends"
        );
        let _ = shutdown_tx.send(());
    }
    fn responses_summary_stream() -> Vec<Event> {
        vec![
            Event::default().data(
                json!({ "type" : "response.created", "sequence_number"
                : 0, "response" : { "id" : "resp_test", "object" : "response", "created_at" :
                1234567890, "model" : "test-model", "status" : "in_progress", "output" : [] }
                })
                .to_string(),
            ),
            Event::default().data(
                json!({ "type" :
            "response.output_text.delta", "sequence_number" : 1, "item_id" : "msg_test",
            "output_index" : 0, "content_index" : 0, "delta" : "<summary>ok</summary>" })
                .to_string(),
            ),
            Event::default().data(
                json!({ "type" : "response.completed",
            "sequence_number" : 2, "response" : { "id" : "resp_test", "object" :
            "response", "created_at" : 1234567890, "model" : "test-model", "status" :
            "completed", "output" : [] } })
                .to_string(),
            ),
        ]
    }
    fn test_config_responses(base_url: &str) -> SamplerConfig {
        let mut config = test_config(base_url);
        config.api_backend = ApiBackend::Responses;
        config
    }
    #[tokio::test]
    async fn responses_compaction_attaches_tools_with_tool_choice_auto() {
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |body: axum::Json<serde_json::Value>| {
                let cap = cap.clone();
                async move {
                    cap.lock().unwrap().push(body.0);
                    let stream = stream::iter(
                        responses_summary_stream()
                            .into_iter()
                            .map(Ok::<_, std::convert::Infallible>),
                    );
                    Sse::new(stream).keep_alive(KeepAlive::default())
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://{addr}/v1");
        let config = test_config_responses(&base_url);
        let chat_history = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("<user_query>\nfix the bug\n</user_query>"),
            ConversationItem::assistant("I fixed it."),
            ConversationItem::user("Summarize the conversation so far."),
        ];
        let tools = vec![ToolSpec {
            name: "read_file".to_string(),
            description: Some("Reads a file".to_string()),
            parameters: json!({ "type" : "object", "properties" : {} }),
        }];
        let hosted = vec![HostedTool::WebSearch {
            allowed_domains: None,
        }];
        let client = Client::new(config.clone()).unwrap();
        generate_session_compact(
            chat_history.clone(),
            tools,
            hosted,
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_secs(30),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await
        .unwrap_or_else(|_| panic!("Responses compaction with tools must succeed"));
        let client = Client::new(config.clone()).unwrap();
        generate_session_compact(
            chat_history,
            vec![],
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_secs(30),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await
        .unwrap_or_else(|_| panic!("Responses compaction without tools must succeed"));
        let bodies = captured.lock().unwrap();
        assert_eq!(bodies.len(), 2, "mock must have served both requests");
        let with_tools = &bodies[0];
        assert_eq!(
            with_tools["tool_choice"],
            json!("auto"),
            "default Responses compaction tool_choice is auto"
        );
        let sent_tools = with_tools["tools"]
            .as_array()
            .expect("tools must be attached for prefix-cache alignment");
        let has_read_file = sent_tools.iter().any(|t| {
            t.get("name") == Some(&json!("read_file"))
                || t.pointer("/name") == Some(&json!("read_file"))
        });
        assert!(
            has_read_file,
            "client function tool must be present: {sent_tools:?}"
        );
        assert!(
            sent_tools
                .iter()
                .any(|t| t.get("type") == Some(&json!("web_search"))),
            "hosted web_search must be present for prefix alignment: {sent_tools:?}"
        );
        let without_tools = &bodies[1];
        assert!(
            without_tools
                .get("tools")
                .map(|t| t.as_array().is_none_or(|a| a.is_empty()))
                .unwrap_or(true),
            "no tools when none are passed"
        );
        assert!(
            without_tools.get("tool_choice").is_none(),
            "tool_choice without tools should be omitted"
        );
        let _ = shutdown_tx.send(());
    }
    #[tokio::test]
    async fn stalled_compaction_stream_times_out_as_transient() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let stream = stream::pending::<Result<Event, std::convert::Infallible>>();
                Sse::new(stream)
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://{addr}/v1");
        let config = test_config(&base_url);
        let client = Client::new(config.clone()).unwrap();
        let chat_history = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("Summarize the conversation so far."),
        ];
        let result = generate_session_compact(
            chat_history,
            vec![],
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_millis(150),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await;
        match result {
            Err(CompactFailure::Transient(err)) => {
                let data = err
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or_default();
                assert!(
                    data.contains("idle timeout"),
                    "expected an idle-timeout transient failure, got: {data}"
                );
            }
            Err(CompactFailure::Deterministic(_)) => {
                panic!("a stalled stream must be retryable (Transient), not Deterministic")
            }
            Ok(_) => panic!("a stalled stream must not produce a summary"),
        }
        let _ = shutdown_tx.send(());
    }
    #[tokio::test]
    async fn completed_then_stalled_stream_errors_no_salvage() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                    Event::default().data(
                        json!({ "id" : "chatcmpl-test", "object" :
                                "chat.completion.chunk", "created" : 1234567890, "model" :
                                "test-model", "choices" : [{ "index" : 0, "delta" : { "role"
                                : "assistant", "content" : "<summary>ok</summary>" },
                                "finish_reason" : "stop" }] })
                        .to_string(),
                    ),
                )])
                .chain(stream::pending::<Result<Event, std::convert::Infallible>>());
                Sse::new(events)
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://{addr}/v1");
        let config = test_config(&base_url);
        let client = Client::new(config.clone()).unwrap();
        let chat_history = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("Summarize the conversation so far."),
        ];
        let result = generate_session_compact(
            chat_history,
            vec![],
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_millis(150),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await;
        match result {
            Err(CompactFailure::Transient(err)) => {
                let data = err
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or_default();
                assert!(
                    data.contains("idle timeout"),
                    "expected an idle-timeout transient failure, got: {data}"
                );
            }
            Err(CompactFailure::Deterministic(_)) => {
                panic!("a stalled stream must be retryable (Transient), not Deterministic")
            }
            Ok(_) => {
                panic!(
                    "salvage removed: a completed-but-unterminated stream must error, not return a summary"
                )
            }
        }
        let _ = shutdown_tx.send(());
    }
    #[tokio::test]
    async fn substantial_partial_errors_no_salvage() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let body = "x".repeat(2500);
                let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                    Event::default().data(
                        json!({ "id" : "chatcmpl-test", "object" :
                                "chat.completion.chunk", "created" : 1234567890, "model" :
                                "test-model", "choices" : [{ "index" : 0, "delta" : { "role"
                                : "assistant", "content" : body } }] })
                        .to_string(),
                    ),
                )])
                .chain(stream::pending::<Result<Event, std::convert::Infallible>>());
                Sse::new(events)
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://{addr}/v1");
        let config = test_config(&base_url);
        let client = Client::new(config.clone()).unwrap();
        let chat_history = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("Summarize the conversation so far."),
        ];
        let result = generate_session_compact(
            chat_history,
            vec![],
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_millis(150),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await;
        match result {
            Err(CompactFailure::Transient(err)) => {
                let data = err
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or_default();
                assert!(
                    data.contains("idle timeout"),
                    "expected an idle-timeout transient failure, got: {data}"
                );
            }
            Err(CompactFailure::Deterministic(_)) => {
                panic!("a stalled stream must be retryable (Transient), not Deterministic")
            }
            Ok(_) => {
                panic!("salvage removed: a substantial partial must error, not be returned")
            }
        }
        let _ = shutdown_tx.send(());
    }
    #[tokio::test]
    async fn thin_partial_retries_on_stall() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let events = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                    Event::default().data(
                        json!({ "id" : "chatcmpl-test", "object" :
                                "chat.completion.chunk", "created" : 1234567890, "model" :
                                "test-model", "choices" : [{ "index" : 0, "delta" : { "role"
                                : "assistant", "content" : "partial" } }] })
                        .to_string(),
                    ),
                )])
                .chain(stream::pending::<Result<Event, std::convert::Infallible>>());
                Sse::new(events)
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://{addr}/v1");
        let config = test_config(&base_url);
        let client = Client::new(config.clone()).unwrap();
        let chat_history = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("Summarize the conversation so far."),
        ];
        let result = generate_session_compact(
            chat_history,
            vec![],
            vec![],
            client,
            acp::SessionId::new("test-session"),
            &config,
            std::time::Duration::from_millis(150),
            0,
            crate::util::config::CompactionToolChoice::Auto,
        )
        .await;
        match result {
            Err(CompactFailure::Transient(_)) => {}
            _ => panic!("a thin stalled body must retry (Transient), not salvage"),
        }
        let _ = shutdown_tx.send(());
    }
}
