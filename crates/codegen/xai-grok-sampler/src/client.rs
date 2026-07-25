//! HTTP client for the xAI sampling APIs.
//!
//! Owns the `reqwest::Client`, default request headers, and per-method
//! defaults. Talks to three backend shapes:
//!
//! * Chat Completions (`/chat/completions`)
//! * Responses API (`/responses`)
//! * Anthropic Messages API (`/messages`)
//!
//! All trace-upload and URL-based header injection is intentionally
//! *not* here. The session is responsible for putting any per-request
//! headers (proxy auth, OTel context, etc.)
//! into [`SamplerConfig::extra_headers`] before constructing the client.

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use indexmap::IndexMap;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde::Serialize;

use xai_grok_sampling_types::error::{try_parse_stream_error, user_facing_api_error_message};
use xai_grok_sampling_types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ConversationRequest,
    ConversationResponse, CreateResponseWrapper, DOOM_LOOP_CHECK_HEADER, MessagesRequestWrapper,
    ReasoningModelIdentity, ResponseModelMetadata, Result, SamplingError, build_messages_request,
    is_check_event, messages, rs,
};

use crate::config::{AuthScheme, OriginClientInfo, SamplerConfig};
use crate::types::ResponsesStreamItem;

// Re-export ApiBackend from the shared types crate for downstream callers.
pub use xai_grok_sampling_types::ApiBackend;

/// Process-level fallback for the `x-grok-client-identifier` header.
const DEFAULT_CLIENT_IDENTIFIER: &str = "grok-shell";

/// Product identifier baked into User-Agent strings.
const AGENT_PRODUCT: &str = "grok-shell";
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 128_000;
const RESPONSES_AUXILIARY_EVENT_TYPES: [&str; 2] = ["keepalive", "response.metadata"];

/// Return whether an SSE frame is an out-of-band Responses API event.
///
/// ChatGPT/Codex emits transport heartbeats (`keepalive`) and may emit
/// side-band `response.metadata` frames. Neither carries model output or maps
/// to async-openai's `ResponseStreamEvent`, so both must be filtered before
/// typed deserialization. They can be identified by the SSE `event:` field or
/// only by the JSON `type` discriminator in `data:`.
///
/// Matching stays exact: generated text containing one of these strings is not
/// swallowed, and every other unknown semantic event still fails loudly.
fn is_responses_auxiliary_event(event_name: &str, data: &str) -> bool {
    if RESPONSES_AUXILIARY_EVENT_TYPES.contains(&event_name) {
        return true;
    }

    // Avoid reparsing ordinary token deltas unless the payload could contain
    // one of the auxiliary discriminators. The parsed discriminator below is
    // still authoritative because generated text can contain either string.
    if !RESPONSES_AUXILIARY_EVENT_TYPES
        .iter()
        .any(|event_type| data.contains(event_type))
    {
        return false;
    }

    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(|event_type| RESPONSES_AUXILIARY_EVENT_TYPES.contains(&event_type))
        })
        .unwrap_or(false)
}

/// Per-request `x-grok-*` headers. Optional fields are skipped when empty/`None`.
struct GrokRequestHeaders<'a> {
    conv_id: &'a str,
    req_id: &'a str,
    model_id: &'a str,
    session_id: &'a str,
    turn_idx: Option<&'a str>,
    agent_id: &'a str,
    deployment_id: Option<&'a str>,
    user_id: Option<&'a str>,
}

impl GrokRequestHeaders<'_> {
    fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut b = builder
            .header("x-grok-conv-id", self.conv_id)
            .header("x-grok-req-id", self.req_id)
            .header("x-grok-model-override", self.model_id)
            .header("x-grok-session-id", self.session_id)
            .header("x-grok-agent-id", self.agent_id);
        if let Some(idx) = self.turn_idx {
            b = b.header("x-grok-turn-idx", idx);
        }
        if let Some(id) = self.deployment_id.filter(|s| !s.is_empty()) {
            b = b.header("x-grok-deployment-id", id);
        }
        if let Some(id) = self.user_id.filter(|s| !s.is_empty()) {
            b = b.header("x-grok-user-id", id);
        }
        b
    }
}

/// Parse the `Retry-After` response header as delta-seconds.
/// Our inference backends only emit integer seconds (never HTTP-date),
/// so we only handle that form. HTTP-dates silently return `None` and
/// the caller falls back to exponential backoff.
/// Capped at 120s to prevent absurdly long sleeps from a misbehaving upstream.
/// Every `type` discriminator async-openai's `rs::ResponseStreamEvent`
/// deserializes. Frames carrying any other `type` are skipped as
/// [`ResponsesStreamItem::Heartbeat`] instead of failing deserialization
/// fatally — the same posture as codex-rs, whose `ResponsesStreamEvent.kind`
/// is a plain `String` with a `_ => trace!` default arm. Without this, every
/// event type OpenAI adds server-side surfaces here as a non-retryable
/// `unknown variant` serialization error.
const RESPONSES_KNOWN_EVENT_TYPES: [&str; 49] = [
    "response.created",
    "response.in_progress",
    "response.completed",
    "response.failed",
    "response.incomplete",
    "response.output_item.added",
    "response.output_item.done",
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.delta",
    "response.output_text.done",
    "response.refusal.delta",
    "response.refusal.done",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "response.file_search_call.in_progress",
    "response.file_search_call.searching",
    "response.file_search_call.completed",
    "response.web_search_call.in_progress",
    "response.web_search_call.searching",
    "response.web_search_call.completed",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_part.done",
    "response.reasoning_summary_text.delta",
    "response.reasoning_summary_text.done",
    "response.reasoning_text.delta",
    "response.reasoning_text.done",
    "response.image_generation_call.completed",
    "response.image_generation_call.generating",
    "response.image_generation_call.in_progress",
    "response.image_generation_call.partial_image",
    "response.mcp_call_arguments.delta",
    "response.mcp_call_arguments.done",
    "response.mcp_call.completed",
    "response.mcp_call.failed",
    "response.mcp_call.in_progress",
    "response.mcp_list_tools.completed",
    "response.mcp_list_tools.failed",
    "response.mcp_list_tools.in_progress",
    "response.code_interpreter_call.in_progress",
    "response.code_interpreter_call.interpreting",
    "response.code_interpreter_call.completed",
    "response.code_interpreter_call_code.delta",
    "response.code_interpreter_call_code.done",
    "response.output_text.annotation.added",
    "response.queued",
    "response.custom_tool_call_input.delta",
    "response.custom_tool_call_input.done",
    "error",
];

/// Deserialize a Responses API SSE event, with a fallback for xAI-specific
/// tool types (e.g., `x_search`) that `async_openai` can't parse.
///
/// Returns `Ok(None)` when the frame is parseable JSON but should be skipped
/// rather than surfaced:
///
/// * the `type` discriminator is unknown to this build (forward-compat: new
///   OpenAI event types must never become fatal `unknown variant` errors), or
/// * it is `response.output_item.done` / `.added` whose nested `item` is an
///   `OutputItem` kind async-openai does not model — codex-rs likewise logs
///   and skips unparseable output items; the deltas and the terminal event
///   carry the usable content.
///
/// The API echoes the request's `tools` array in `ResponseCompleted` and
/// `ResponseCreated` events. If we sent `{"type": "x_search"}`, the response
/// includes it, and `rs::Tool` deserialization fails. On failure, we strip
/// unrecognized tools from the raw JSON and retry; terminal events get the
/// same treatment for unrecognized `output` items.
///
/// On `response.completed` / `response.incomplete`, this also rewrites
/// `response.usage.total_tokens` in place to the live context length
/// (`context_details.input_tokens + context_details.output_tokens`)
/// when the API emits the xAI-specific `context_details` field.
/// Async-openai's typed `ResponseUsage` doesn't model `context_details`,
/// so we peek the raw JSON for it. The cumulative `input_tokens` /
/// `output_tokens` / `cached_tokens` continue to flow from the typed
/// `ResponseUsage` unchanged so billing telemetry stays correct. When
/// the API doesn't emit `context_details` (older deployments) `total_tokens`
/// passes through unchanged.
fn deserialize_response_event(data: &str) -> Result<Option<rs::ResponseStreamEvent>> {
    let first_err = match serde_json::from_str::<rs::ResponseStreamEvent>(data) {
        Ok(mut event) => {
            apply_terminal_event_overrides(&mut event, data);
            return Ok(Some(event));
        }
        Err(first_err) => first_err,
    };

    // Try sanitizing: parse as Value, drop what async-openai can't model, retry.
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(data) else {
        tracing::error!(
            error = %first_err,
            raw_data = %data,
            "Failed to deserialize ResponseStreamEvent from stream"
        );
        return Err(SamplingError::Serialization(first_err));
    };

    if let Some(event_type) = value.get("type").and_then(|v| v.as_str()) {
        // Forward-compat: unknown top-level event type — skip, never fatal.
        if !RESPONSES_KNOWN_EVENT_TYPES.contains(&event_type) {
            tracing::debug!(
                event_type,
                "skipping Responses API event of unknown type (forward-compat)"
            );
            return Ok(None);
        }
        // Known frame carrying an output item kind async-openai cannot model
        // (e.g. a newly added OutputItem type): skip the frame rather than
        // fail the stream — streamed deltas and the terminal event still
        // carry the usable content.
        if matches!(
            event_type,
            "response.output_item.done" | "response.output_item.added"
        ) && value.get("item").is_some()
            && serde_json::from_value::<rs::OutputItem>(value["item"].clone()).is_err()
        {
            tracing::debug!(
                event_type,
                "skipping Responses API event with unknown output item kind"
            );
            return Ok(None);
        }
    }

    // Strip tools that async_openai's rs::Tool can't deserialize (e.g.,
    // xAI-specific "x_search"). Instead of maintaining a hardcoded allowlist,
    // try deserializing each tool entry — if it fails, drop it.
    if let Some(tools) = value
        .pointer_mut("/response/tools")
        .and_then(|v| v.as_array_mut())
    {
        tools.retain(|t| serde_json::from_value::<rs::Tool>(t.clone()).is_ok());
    }
    // Same for terminal-event `output` items of a kind this build predates.
    if let Some(output) = value
        .pointer_mut("/response/output")
        .and_then(|v| v.as_array_mut())
    {
        output.retain(|item| serde_json::from_value::<rs::OutputItem>(item.clone()).is_ok());
    }
    match serde_json::from_value::<rs::ResponseStreamEvent>(value) {
        Ok(mut event) => {
            apply_terminal_event_overrides(&mut event, data);
            Ok(Some(event))
        }
        Err(_) => {
            tracing::error!(
                error = %first_err,
                raw_data = %data,
                "Failed to deserialize ResponseStreamEvent from stream"
            );
            Err(SamplingError::Serialization(first_err))
        }
    }
}

/// Decode one Responses SSE frame.
///
/// Auxiliary frames (transport heartbeats) and forward-compat skips both
/// surface as [`ResponsesStreamItem::Heartbeat`] so the layer-2 idle detector
/// sees server liveness instead of a starved stream. API errors are surfaced
/// as `SamplingError`; malformed known events stay strict and fatal.
fn decode_responses_sse_frame(
    event_name: &str,
    data: &str,
) -> std::result::Result<ResponsesStreamItem, SamplingError> {
    if is_responses_auxiliary_event(event_name, data) {
        return Ok(ResponsesStreamItem::Heartbeat);
    }

    if let Some(stream_error) = try_parse_stream_error(data) {
        Err(stream_error)
    } else {
        match deserialize_response_event(data) {
            Ok(Some(event)) => Ok(ResponsesStreamItem::Event(event)),
            Ok(None) => Ok(ResponsesStreamItem::Heartbeat),
            Err(err) => Err(err),
        }
    }
}

/// Every `type` discriminator [`messages::MessageStreamEvent`] deserializes.
/// Frames carrying any other `type` are mapped to `Ping` (liveness-only)
/// instead of failing deserialization fatally — same forward-compat posture
/// as [`RESPONSES_KNOWN_EVENT_TYPES`].
const MESSAGES_KNOWN_EVENT_TYPES: [&str; 8] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "ping",
    "error",
];

/// Decode one Anthropic Messages SSE frame.
///
/// Forward-compat: frames whose `type` is unknown to this build (a newer
/// Anthropic / provider event kind), and `content_block_start` /
/// `content_block_delta` frames whose nested block / delta kind is unknown,
/// are surfaced as `Ping` — the layer-2 liveness-only event — rather than a
/// fatal `Serialization` error on a healthy stream. Malformed frames of a
/// known type stay strict and fatal.
fn decode_messages_sse_frame(data: &str) -> Result<messages::MessageStreamEvent> {
    let first_err = match serde_json::from_str::<messages::MessageStreamEvent>(data) {
        Ok(event) => return Ok(event),
        Err(first_err) => first_err,
    };

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(data)
        && let Some(event_type) = value.get("type").and_then(|v| v.as_str())
    {
        let skip = if !MESSAGES_KNOWN_EVENT_TYPES.contains(&event_type) {
            true
        } else {
            match event_type {
                "content_block_start" => value
                    .get("content_block")
                    .is_some_and(|b| serde_json::from_value::<messages::ContentBlock>(b.clone()).is_err()),
                "content_block_delta" => value
                    .get("delta")
                    .is_some_and(|d| serde_json::from_value::<messages::StreamDelta>(d.clone()).is_err()),
                _ => false,
            }
        };
        if skip {
            tracing::debug!(
                event_type,
                "skipping Messages API frame with unknown type/block kind (forward-compat)"
            );
            return Ok(messages::MessageStreamEvent::Ping);
        }
    }

    tracing::error!(
        error = %first_err,
        raw_data = %data,
        "Failed to deserialize MessageStreamEvent from stream"
    );
    Err(SamplingError::Serialization(first_err))
}

/// On terminal Responses API events (`response.completed` /
/// `response.incomplete`), rewrite `response.usage.total_tokens` to the
/// live context length when the wire includes
/// `response.usage.context_details.{input_tokens, output_tokens}`.
///
/// `total_tokens` drives the CLI's `/context` bar, the auto-compact
/// threshold, and `meta.totalTokens` on persisted sessions. Under
/// server-side multi-turn loops (e.g. `web_search`, `x_search`) the
/// wire's cumulative total inflates as the loop runs; `context_details`
/// reports the final turn's prompt + output tokens — the real live
/// context the model is sitting in. Billing fields
/// (`input_tokens`, `output_tokens`, `input_tokens_details.cached_tokens`,
/// `output_tokens_details.reasoning_tokens`) stay on the cumulative
/// wire values so telemetry is unaffected.
///
/// No-op when:
/// - the event is not terminal,
/// - `response.usage` is `None`,
/// - `context_details` is absent (older backends / non-loop responses),
/// - or either of `context_details.{input_tokens, output_tokens}` is
///   missing — we don't guess the missing half.
fn apply_terminal_event_overrides(event: &mut rs::ResponseStreamEvent, data: &str) {
    let response = match event {
        rs::ResponseStreamEvent::ResponseCompleted(e) => &mut e.response,
        rs::ResponseStreamEvent::ResponseIncomplete(e) => &mut e.response,
        _ => return,
    };
    // Re-parse for fields async_openai's types omit (context total, cost ticks).
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };
    // Stash cost ticks in metadata for stream_responses.
    if let Some(ticks) = xai_grok_sampling_types::reported_cost_ticks(
        value
            .pointer("/response/usage/cost_in_usd_ticks")
            .and_then(|v| v.as_i64()),
    ) {
        response
            .metadata
            .get_or_insert_with(Default::default)
            .insert(COST_USD_TICKS_METADATA_KEY.to_owned(), ticks.to_string());
    }
    let Some(usage) = response.usage.as_mut() else {
        return;
    };
    let Some(total) = extract_context_total(&value) else {
        return;
    };
    usage.total_tokens = total;
}

/// Stamp OpenAI Responses `prompt_cache_key` from session / conv id when unset.
///
/// Used for both public OpenAI-compatible Responses and Codex (Pi clamps to 64).
fn stamp_responses_prompt_cache_key(request: &mut CreateResponseWrapper) {
    if request.inner.prompt_cache_key.is_some() {
        return;
    }
    let key = request
        .x_grok_session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            request
                .x_grok_conv_id
                .as_deref()
                .filter(|s| !s.is_empty())
        });
    if let Some(key) = key {
        request.inner.prompt_cache_key =
            Some(xai_grok_sampling_types::clamp_prompt_cache_key(key));
    }
}

/// Apply the ChatGPT Codex backend dialect to a Responses API request.
///
/// Mirrors official Pi `openai-codex-responses.ts` request building:
/// - the system prompt travels in the top-level `instructions` field, not as
///   `input` items (Pi: `convertResponsesMessages` with
///   `includeSystemPrompt: false` + `instructions: context.systemPrompt`);
/// - `prompt_cache_key` is stamped via [`stamp_responses_prompt_cache_key`];
/// - `text.verbosity` defaults to `low` (Pi default) unless a text format
///   (e.g. structured output) is already set.
fn apply_codex_dialect(request: &mut CreateResponseWrapper) {
    if let rs::InputParam::Items(items) = &mut request.inner.input {
        let mut system_texts: Vec<String> = Vec::new();
        items.retain(|item| {
            if let rs::InputItem::EasyMessage(message) = item
                && message.role == rs::Role::System
            {
                if let rs::EasyInputContent::Text(text) = &message.content {
                    system_texts.push(text.clone());
                }
                return false;
            }
            true
        });
        if !system_texts.is_empty() && request.inner.instructions.is_none() {
            request.inner.instructions = Some(system_texts.join("\n\n"));
        }
    }
    if request.inner.instructions.is_none() {
        // Pi: `instructions: context.systemPrompt || "You are a helpful assistant."`
        request.inner.instructions = Some("You are a helpful assistant.".to_string());
    }
    stamp_responses_prompt_cache_key(request);
    if request.inner.text.is_none() {
        // Pi: `text: { verbosity: "low" }`. Format defaults to text when omitted;
        // typed Text serializes as `{ "type": "text" }` which the backend accepts.
        request.inner.text = Some(rs::ResponseTextParam {
            format: rs::TextResponseFormatConfiguration::Text,
            verbosity: Some(rs::Verbosity::Low),
        });
    }
    // Pi sends `reasoning.summary: "auto"` (the shared default is `concise`).
    if let Some(reasoning) = request.inner.reasoning.as_mut() {
        reasoning.summary = Some(rs::ReasoningSummary::Auto);
    }
    // ChatGPT Codex backend rejects parameters the public Responses API allows
    // (verified live against chatgpt.com/backend-api/codex/responses):
    //   Unsupported parameter: max_output_tokens | temperature | top_p | …
    // Official Pi never sends these on openai-codex-responses. Our catalog
    // stamps max_completion_tokens=128k on Codex models, which becomes
    // max_output_tokens here and 400s every turn.
    request.inner.max_output_tokens = None;
    request.inner.temperature = None;
    request.inner.top_p = None;
    request.inner.max_tool_calls = None;
}

/// Strip fields ChatGPT Codex rejects after JSON serialize (defense in depth
/// if typed defaults re-introduce them).
fn strip_codex_unsupported_body_fields(body: &mut serde_json::Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for key in [
        "max_output_tokens",
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "max_tool_calls",
        "background",
        "truncation",
        "top_logprobs",
        "prompt_cache_retention",
    ] {
        obj.remove(key);
    }
}

/// Codex / ChatGPT Responses wire form for `reasoning.effort`.
///
/// Mirrors official `openai/codex` `ModelClient::reasoning_effort_for_request`
/// (`codex-rs/core/src/client.rs`):
///
/// ```ignore
/// fn reasoning_effort_for_request(effort) {
///     match effort {
///         Ultra => Max,  // UI "ultra" never hits the Responses API as "ultra"
///         other => other,
///     }
/// }
/// ```
///
/// Ultra remains a **client** policy (Codex multi-agent v2 → `Proactive`
/// delegation in `multi_agents.rs`); the HTTP body always carries
/// `reasoning.effort: "max"`.
///
/// async-openai only types through `xhigh`, so Max would also collapse on
/// serialize without this post-serialize rewrite.
///
/// - `None` → drop `reasoning` (off)
/// - `Minimal` → `"low"` (Pi alias on Codex xhigh models)
/// - `Max` / `Ultra` → `"max"` (official Ultra→Max)
/// - others → their `as_str()` token
fn patch_codex_reasoning_effort_wire(
    body: &mut serde_json::Value,
    effort: Option<xai_grok_sampling_types::ReasoningEffort>,
) {
    use xai_grok_sampling_types::ReasoningEffort as E;
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match effort {
        None => {}
        Some(E::None) => {
            obj.remove("reasoning");
        }
        Some(e) => {
            // Keep in lockstep with openai/codex `reasoning_effort_for_request`.
            let wire = match e {
                E::None => unreachable!(),
                E::Minimal => "low",
                E::Low => "low",
                E::Medium => "medium",
                E::High => "high",
                E::Xhigh => "xhigh",
                // Ultra is multi-agent UX only; backend receives max.
                E::Max | E::Ultra => "max",
            };
            let reasoning = obj
                .entry("reasoning")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(r) = reasoning.as_object_mut() {
                r.insert("effort".into(), serde_json::Value::String(wire.into()));
                r.entry("summary")
                    .or_insert_with(|| serde_json::Value::String("auto".into()));
            }
        }
    }
}

/// Metadata key for cost ticks past typed Response events.
pub(crate) const COST_USD_TICKS_METADATA_KEY: &str = "xai.cost_usd_ticks";

/// Read `response.usage.context_details.{input_tokens, output_tokens}`
/// from the parsed terminal-event JSON and return their sum. Returns `None`
/// if either field is missing or out of `u32` range.
fn extract_context_total(value: &serde_json::Value) -> Option<u32> {
    let cd = value.pointer("/response/usage/context_details")?;
    let i = u32::try_from(cd.get("input_tokens")?.as_u64()?).ok()?;
    let o = u32::try_from(cd.get("output_tokens")?.as_u64()?).ok()?;
    Some(i.saturating_add(o))
}

/// Record `success=false` + `error` on the active inference span when a stream
/// request fails before any response (transport/connect/TLS errors). Without
/// this the `#[instrument]` span closes with both fields Empty, so an outage
/// shows zero `success=false` and error-rate alerts never fire.
fn record_stream_request_failure(err: &reqwest::Error) {
    let span = tracing::Span::current();
    span.record("success", false);
    span.record("error", err.to_string().as_str());
}

fn extract_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| s.min(120))
}

fn extract_should_retry(headers: &reqwest::header::HeaderMap) -> Option<bool> {
    headers
        .get("x-should-retry")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            if s.eq_ignore_ascii_case("true") {
                Some(true)
            } else if s.eq_ignore_ascii_case("false") {
                Some(false)
            } else {
                None // unknown value — treat as absent
            }
        })
}

fn extract_model_metadata(headers: &reqwest::header::HeaderMap) -> Option<ResponseModelMetadata> {
    let context_window = headers
        .get("x-grok-context-window")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let max_completion_tokens = headers
        .get("x-grok-max-completion-tokens")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok());

    let models_etag = headers
        .get("x-models-etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if context_window.is_some() || max_completion_tokens.is_some() || models_etag.is_some() {
        Some(ResponseModelMetadata {
            context_window,
            max_completion_tokens,
            models_etag,
        })
    } else {
        None
    }
}

/// Wrapper for streaming chat completion requests that adds `stream` and
/// `stream_options` fields without modifying the original `ChatCompletionRequest`.
///
/// Uses `#[serde(flatten)]` to inline all fields from the inner request,
/// allowing single-pass serialization instead of the previous two-pass
/// approach (serialize to `Value`, mutate, serialize to bytes).
#[derive(Serialize)]
struct StreamingChatRequest<'a> {
    #[serde(flatten)]
    inner: &'a ChatCompletionRequest,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Resolve `env_http_headers` (`header -> env var`) into `headers` via `getenv`, skipping unset/blank/invalid entries and trimming values.
fn apply_env_http_headers(
    env_http_headers: &IndexMap<String, String>,
    getenv: impl Fn(&str) -> Option<String>,
    headers: &mut HeaderMap,
) {
    for (key, env_var) in env_http_headers {
        let Some(value) = getenv(env_var) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let (Ok(name), Ok(header_value)) = (
            HeaderName::try_from(key.as_str()),
            HeaderValue::from_str(value),
        ) else {
            tracing::warn!(
                header = %key,
                env_var = %env_var,
                "skipping env_http_header with an invalid header name or value"
            );
            continue;
        };
        headers.insert(name, header_value);
    }
}

/// HTTP client for sampling. Cheap to clone; carries an `Arc`-backed
/// `reqwest::Client` and the default headers/request-defaults computed from a
/// [`SamplerConfig`] at construction time.
#[derive(Clone)]
pub struct SamplingClient {
    http: reqwest::Client,
    default_headers: HeaderMap,
    base_url: String,
    defaults: ClientDefaults,
    /// Optional 401-attribution hook. The shell wires this to emit a
    /// structured event at every UNAUTHORIZED arm so 401s can be
    /// bucketed by stale-snapshot vs. live-token-rejected. `None` for
    /// sampler-only callers and tests.
    attribution_callback: Option<crate::attribution::SharedAttributionCallback>,
    /// Per-request bearer override. See `SamplerConfig::bearer_resolver`.
    bearer_resolver: Option<crate::config::SharedBearerResolver>,
    /// Per-request header injection (OTel traceparent).
    header_injector: Option<crate::config::SharedHeaderInjector>,
    /// Endpoint URL builder, resolved once from `base_url` + `query_params`.
    endpoint: EndpointTemplate,
}

impl std::fmt::Debug for SamplingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplingClient")
            .field("base_url", &self.base_url)
            .field("defaults", &self.defaults)
            .field(
                "has_attribution_callback",
                &self.attribution_callback.is_some(),
            )
            .field("has_bearer_resolver", &self.bearer_resolver.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
struct ClientDefaults {
    model: String,
    max_completion_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    api_backend: ApiBackend,
    auth_scheme: AuthScheme,
    stream_tool_calls: bool,
    doom_loop_recovery: Option<xai_grok_sampling_types::DoomLoopRecoveryPolicy>,
    responses_codex_dialect: bool,
    kimi_dialect: bool,
    /// Session reasoning effort. Needed for Codex wire rewrite: async-openai
    /// only types through `xhigh`, so Max/Ultra must be restored post-serialize.
    reasoning_effort: Option<xai_grok_sampling_types::ReasoningEffort>,
}

/// Endpoint URL builder, resolved once at client construction so each request
/// only appends its path.
#[derive(Clone, Debug)]
enum EndpointTemplate {
    /// No query params and no query on the base URL (or an unparseable base):
    /// append the path to the base verbatim.
    Plain(String),
    /// Query params configured: `{prefix}/{path}{suffix}`. `suffix` starts with
    /// `?` and folds any base-URL params, with a configured key winning over the
    /// same key in `base_url` (percent-encoded, no duplicates).
    WithQuery { prefix: String, suffix: String },
}

impl EndpointTemplate {
    fn new(base_url: &str, query_params: &IndexMap<String, String>) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        // The fast path is safe only when there is nothing to fold: no configured
        // params and no query already on the base (which would otherwise land
        // before the appended path).
        if query_params.is_empty() && !base.contains('?') {
            return Self::Plain(base);
        }
        let mut url = match reqwest::Url::parse(&base) {
            Ok(url) => url,
            Err(error) => {
                tracing::warn!(
                    url = %base,
                    %error,
                    "failed to parse base URL for endpoint; sending without folded query"
                );
                return Self::Plain(base);
            }
        };
        let overridden: std::collections::HashSet<&str> =
            query_params.keys().map(String::as_str).collect();
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| !overridden.contains(k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let prefix = {
            let mut prefix_url = url.clone();
            prefix_url.set_query(None);
            prefix_url.as_str().trim_end_matches('/').to_string()
        };
        {
            let mut pairs = url.query_pairs_mut();
            pairs.clear();
            for (key, value) in &kept {
                pairs.append_pair(key, value);
            }
            for (key, value) in query_params {
                pairs.append_pair(key, value);
            }
        }
        let suffix = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        Self::WithQuery { prefix, suffix }
    }

    fn url_for_path(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        match self {
            Self::Plain(base) => format!("{base}/{path}"),
            Self::WithQuery { prefix, suffix } => format!("{prefix}/{path}{suffix}"),
        }
    }
}

// =============================================================================
// User-Agent helpers
// =============================================================================

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformInfo {
    os: String,
    arch: String,
}

impl PlatformInfo {
    fn current() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            other => other,
        }
        .to_string();

        let arch = match std::env::consts::ARCH {
            "arm64" => "aarch64",
            "x86_64" => "x86_64",
            other => other,
        }
        .to_string();

        Self { os, arch }
    }
}

fn agent_version() -> String {
    xai_grok_version::VERSION.to_string()
}

/// Render a User-Agent string for the given origin client.
///
/// Mirrors the shell's `user_agent_string_for` but uses sampler-local
/// constants. The session typically owns the canonical User-Agent
/// rendering for process-wide HTTP clients; this helper is for
/// per-session sampling clients that want to override it.
pub fn user_agent_string_for(origin: &OriginClientInfo) -> String {
    let agent_version = agent_version();
    let platform = PlatformInfo::current();

    if origin.product == AGENT_PRODUCT && origin.version.as_deref() == Some(agent_version.as_str())
    {
        return format!(
            "{}/{} ({}; {})",
            AGENT_PRODUCT, agent_version, platform.os, platform.arch
        );
    }

    match origin.version.as_deref() {
        Some(origin_version) => format!(
            "{}/{} {}/{} ({}; {})",
            origin.product,
            origin_version,
            AGENT_PRODUCT,
            agent_version,
            platform.os,
            platform.arch
        ),
        None => format!(
            "{} {}/{} ({}; {})",
            origin.product, AGENT_PRODUCT, agent_version, platform.os, platform.arch
        ),
    }
}

// =============================================================================
// SamplingClient
// =============================================================================

impl SamplingClient {
    /// Construct a sampling client from a [`SamplerConfig`].
    ///
    /// Grabs the process-wide shared `reqwest::Client` (HTTP/2 by
    /// default, HTTP/1.1 when `config.force_http1` is set) and
    /// pre-computes the default request headers. This does not perform
    /// any network I/O.
    pub fn new(config: SamplerConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(ref api_key) = config.api_key {
            match config.auth_scheme {
                AuthScheme::XApiKey => {
                    let header_value = HeaderValue::from_str(api_key).map_err(|_| {
                        tracing::debug!(
                            api_key = %api_key,
                            "Invalid api_key: cannot be converted to a valid HTTP header"
                        );
                        SamplingError::Auth(
                            "Invalid api_key: cannot be converted to a valid HTTP header"
                                .to_string(),
                        )
                    })?;
                    headers.insert(HeaderName::from_static("x-api-key"), header_value);
                }
                AuthScheme::Bearer => {
                    let bearer = format!("Bearer {}", api_key);
                    let header_value = HeaderValue::from_str(&bearer).map_err(|_| {
                        tracing::debug!(
                            api_key = %api_key,
                            "Invalid api_key: cannot be converted to a valid HTTP Authorization header"
                        );
                        SamplingError::Auth(
                            "Invalid api_key: cannot be converted to a valid HTTP Authorization header"
                                .to_string(),
                        )
                    })?;
                    headers.insert(AUTHORIZATION, header_value);
                }
            }
        }

        // Apply all extra headers verbatim. This is the single
        // injection point for proxy-auth headers and any other URL- or
        // environment-specific headers the session decides to set.
        for (key, value) in &config.extra_headers {
            let header_name = HeaderName::try_from(key.as_str())
                .map_err(|_| SamplingError::InvalidConfiguration("Invalid extra header name"))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|_| SamplingError::InvalidConfiguration("Invalid extra header value"))?;
            headers.insert(header_name, header_value);
        }

        // Resolve here, not into `extra_headers`, so an env-sourced secret stays
        // out of persisted state.
        apply_env_http_headers(
            &config.env_http_headers,
            |var| std::env::var(var).ok(),
            &mut headers,
        );

        // Add x-grok-client-version header for version gating at the proxy.
        if let Some(client_version) = config.client_version.as_ref()
            && let Ok(header_value) = HeaderValue::from_str(client_version)
        {
            headers.insert(
                HeaderName::from_static("x-grok-client-version"),
                header_value,
            );
        }

        if let Some(deployment_id) = config.deployment_id.as_ref()
            && let Ok(header_value) = HeaderValue::from_str(deployment_id)
        {
            headers.insert(
                HeaderName::from_static("x-grok-deployment-id"),
                header_value,
            );
        }

        if let Some(user_id) = config.user_id.as_ref()
            && let Ok(header_value) = HeaderValue::from_str(user_id)
        {
            headers.insert(HeaderName::from_static("x-grok-user-id"), header_value);
        }

        {
            let client_id = config
                .client_identifier
                .clone()
                .unwrap_or_else(|| DEFAULT_CLIENT_IDENTIFIER.to_string());
            if let Ok(header_value) = HeaderValue::from_str(&client_id) {
                headers.insert(
                    HeaderName::from_static("x-grok-client-identifier"),
                    header_value,
                );
            }
        }

        // Always set User-Agent: per-session origin if available, else fallback.
        {
            let ua_string = match config.origin_client.as_ref() {
                Some(origin) => user_agent_string_for(origin),
                None => user_agent_string_for(&OriginClientInfo {
                    product: AGENT_PRODUCT.to_string(),
                    version: Some(agent_version()),
                }),
            };
            if let Ok(v) = HeaderValue::from_str(&ua_string) {
                headers.insert(USER_AGENT, v);
            }
        }

        let http = if config.force_http1 {
            tracing::info!("Using HTTP/1.1 for sampling client (force_http1=true)");
            crate::shared_http::client_http1().map_err(SamplingError::Http)?
        } else {
            crate::shared_http::client().map_err(SamplingError::Http)?
        };

        tracing::info!(
            target: crate::sampling_log::TARGET,
            event = "client_new",
            base_url = %config.base_url,
            model = %config.model,
            api_backend = ?config.api_backend,
            auth_scheme = ?config.auth_scheme,
            // "unset" (not "none"): `ReasoningEffort::None` is a real wire value;
            // logging the absent Option as "none" looked like we were sending it.
            reasoning_effort = config.reasoning_effort.map_or("unset", |e| e.as_str()),
            has_api_key = config.api_key.is_some(),
            has_bearer_resolver = config.bearer_resolver.is_some(),
            has_authorization_header = headers.get(AUTHORIZATION).is_some(),
            has_x_api_key_header = headers.get(HeaderName::from_static("x-api-key")).is_some(),
        );

        let defaults = ClientDefaults {
            model: config.model,
            max_completion_tokens: config.max_completion_tokens,
            temperature: config.temperature,
            top_p: config.top_p,
            api_backend: config.api_backend,
            auth_scheme: config.auth_scheme,
            stream_tool_calls: config.stream_tool_calls,
            doom_loop_recovery: config.doom_loop_recovery,
            responses_codex_dialect: config.responses_codex_dialect,
            kimi_dialect: config.kimi_dialect,
            reasoning_effort: config.reasoning_effort,
        };

        let endpoint = EndpointTemplate::new(&config.base_url, &config.query_params);

        Ok(Self {
            http,
            default_headers: headers,
            base_url: config.base_url,
            defaults,
            attribution_callback: config.attribution_callback,
            bearer_resolver: config.bearer_resolver,
            header_injector: config.header_injector,
            endpoint,
        })
    }

    /// The configured API backend for this client.
    pub fn api_backend(&self) -> ApiBackend {
        self.defaults.api_backend.clone()
    }

    /// POST with default headers. A live resolver is evaluated exactly once;
    /// the returned prefix is carried to 401 attribution so logging never
    /// performs another potentially blocking credential lookup.
    /// When a bearer_resolver is wired it is the sole auth source: a missing
    /// live bearer strips default Authorization / x-api-key so a hard-expired
    /// seed key cannot ride on the wire.
    fn post(&self, url: impl reqwest::IntoUrl) -> (reqwest::RequestBuilder, Option<String>) {
        let mut headers = self.default_headers.clone();
        if let Some(resolver) = &self.bearer_resolver {
            // A resolver is authoritative. Remove construction-time auth even
            // when refresh returns `None`; otherwise an expired catalog stamp
            // is silently sent after a failed refresh.
            headers.remove(AUTHORIZATION);
            headers.remove(HeaderName::from_static("x-api-key"));
            let resolution = resolver.resolve_bearer();
            if let Some(fresh) = resolution.bearer {
                match self.defaults.auth_scheme {
                    AuthScheme::XApiKey => {
                        if let Ok(v) = HeaderValue::from_str(&fresh) {
                            headers.insert(HeaderName::from_static("x-api-key"), v);
                        }
                    }
                    AuthScheme::Bearer => {
                        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {fresh}")) {
                            headers.insert(AUTHORIZATION, v);
                        }
                    }
                }
            }
            for name in resolution.remove_headers {
                headers.remove(name);
            }
            headers.extend(resolution.headers);
        }
        {
            let auth_prefix = headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.chars().take(20).collect::<String>());
            let x_api_key_prefix = headers
                .get(HeaderName::from_static("x-api-key"))
                .and_then(|v| v.to_str().ok())
                .map(|s| s.chars().take(12).collect::<String>());
            tracing::info!(
                target: crate::sampling_log::TARGET,
                event = "client_post",
                base_url = %self.base_url,
                model = %self.defaults.model,
                api_backend = ?self.defaults.api_backend,
                auth_scheme = ?self.defaults.auth_scheme,
                has_bearer_resolver = self.bearer_resolver.is_some(),
                has_authorization_header = headers.get(AUTHORIZATION).is_some(),
                has_x_api_key_header = headers.get(HeaderName::from_static("x-api-key")).is_some(),
                auth_header_prefix = auth_prefix.as_deref().unwrap_or("none"),
                x_api_key_prefix = x_api_key_prefix.as_deref().unwrap_or("none"),
            );
        }
        if let Some(injector) = &self.header_injector {
            injector.inject(&mut headers);
        }
        let sent_bearer_prefix = self.extract_sent_bearer_from(&headers);
        (self.http.post(url).headers(headers), sent_bearer_prefix)
    }

    fn truncate_bearer_prefix(bearer: &str) -> String {
        // Authorization and x-api-key values are ASCII, so this byte boundary
        // is also a UTF-8 character boundary.
        bearer[..crate::attribution::SENT_BEARER_PREFIX_LEN.min(bearer.len())].to_string()
    }

    /// Bearer prefix for 401 attribution. When a resolver is wired it is
    /// authoritative (including `None` ⇒ nothing was sent). Without a resolver,
    /// fall back to construction-time default headers.
    ///
    /// Prefer the prefix returned by [`Self::post`] for request-path
    /// attribution so the live credential is resolved only once.
    #[cfg_attr(not(test), allow(dead_code))]
    fn current_sent_bearer_prefix(&self) -> Option<String> {
        if self.bearer_resolver.is_some() {
            return self
                .bearer_resolver
                .as_ref()
                .and_then(|r| r.current_bearer())
                .map(|s| Self::truncate_bearer_prefix(&s));
        }
        self.extract_sent_bearer()
    }

    /// Extract the construction-time bearer, truncated to prefix length.
    fn extract_sent_bearer(&self) -> Option<String> {
        self.extract_sent_bearer_from(&self.default_headers)
    }

    /// Extract the bearer that will actually be sent from a finalized header map.
    fn extract_sent_bearer_from(&self, headers: &HeaderMap) -> Option<String> {
        let raw = match self.defaults.auth_scheme {
            AuthScheme::XApiKey => headers
                .get(HeaderName::from_static("x-api-key"))
                .and_then(|v| v.to_str().ok()),
            AuthScheme::Bearer => headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer ")),
        };
        raw.map(Self::truncate_bearer_prefix)
    }

    /// Attribute a 401 to the exact bearer already resolved for this request.
    fn record_401_attribution(
        &self,
        consumer: crate::attribution::SamplingConsumer,
        sent_bearer_prefix: Option<&str>,
    ) {
        if let Some(cb) = self.attribution_callback.as_ref() {
            cb.record_401(consumer, sent_bearer_prefix);
        }
    }

    pub fn auth_info(&self) -> crate::sampling_log::AuthInfo {
        // Span construction must stay network-free. Live resolver prefixes are
        // captured later by `post()` and carried to 401 attribution.
        let auth_prefix = if self.bearer_resolver.is_some() {
            None
        } else {
            self.extract_sent_bearer()
        };
        let has_auth = self.bearer_resolver.is_some() || auth_prefix.is_some();
        let auth_type = if has_auth {
            match self.defaults.auth_scheme {
                AuthScheme::XApiKey => "x-api-key",
                AuthScheme::Bearer => "bearer",
            }
        } else {
            "none"
        };
        crate::sampling_log::AuthInfo {
            auth_type,
            auth_prefix,
        }
    }

    /// Check if a header name contains sensitive information that should be redacted.
    fn is_sensitive_header(name: &str) -> bool {
        let lower = name.to_lowercase();
        lower.contains("authorization")
            || lower.contains("api-key")
            || lower.contains("apikey")
            || lower.contains("token")
            || lower.contains("secret")
    }

    /// Short lossy body snippet for error logs (never user-facing).
    fn body_preview(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).chars().take(500).collect()
    }

    /// Log all headers from a request at debug level (redacting sensitive values).
    fn log_request_headers(request: &reqwest::Request, endpoint_name: &str) {
        for (name, value) in request.headers().iter() {
            let value_str = if Self::is_sensitive_header(name.as_str()) {
                "[REDACTED]"
            } else {
                value.to_str().unwrap_or("[non-utf8]")
            };
            tracing::debug!(
                header_name = %name,
                header_value = %value_str,
                "Request header ({})",
                endpoint_name
            );
        }
    }

    fn endpoint(&self, path: &str) -> String {
        self.endpoint.url_for_path(path)
    }

    fn apply_defaults(&self, mut request: ChatCompletionRequest) -> Result<ChatCompletionRequest> {
        if request.model.is_none() {
            request.model = Some(self.defaults.model.clone());
        }

        if request.max_tokens.is_none() {
            request.max_tokens = self.defaults.max_completion_tokens;
        }

        if request.temperature.is_none() {
            request.temperature = self.defaults.temperature;
        }

        if request.top_p.is_none() {
            request.top_p = self.defaults.top_p;
        }

        // OpenAI Chat Completions: sticky prompt_cache_key for automatic
        // prefix-cache affinity when the provider supports it.
        if request.prompt_cache_key.is_none() {
            let key = request
                .x_grok_session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    request
                        .x_grok_conv_id
                        .as_deref()
                        .filter(|s| !s.is_empty())
                });
            if let Some(key) = key {
                request.prompt_cache_key =
                    Some(xai_grok_sampling_types::clamp_prompt_cache_key(key));
            }
        }

        Ok(request)
    }

    async fn handle_response(
        &self,
        response: reqwest::Response,
        sent_bearer_prefix: Option<&str>,
    ) -> Result<ChatCompletionResponse> {
        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ChatCompletions,
                    sent_bearer_prefix,
                );
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401): {server_message}"
                )));
            }
            let message = user_facing_api_error_message(status, bytes.as_ref());
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let completion = serde_json::from_slice::<ChatCompletionResponse>(&bytes).map_err(|e| {
            let raw_body = String::from_utf8_lossy(&bytes);
            tracing::error!(
                error = %e,
                raw_body = %raw_body,
                "Failed to deserialize ChatCompletionResponse"
            );
            SamplingError::Serialization(e)
        })?;
        Ok(completion)
    }

    // =========================================================================
    // Chat Completions API
    // =========================================================================

    pub async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse> {
        let payload = self.apply_defaults(request)?;
        let x_grok_conv_id = &payload.x_grok_conv_id.clone().unwrap_or_default();
        let x_grok_req_id = &payload.x_grok_req_id.clone().unwrap_or_default();
        let model_id = payload.model.clone().unwrap_or_default();

        tracing::debug!(
            base_url = %self.base_url,
            model_id = %model_id,
            "Sending chat completion request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: payload.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: payload.x_grok_turn_idx.as_deref(),
            agent_id: payload.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: payload.x_grok_deployment_id.as_deref(),
            user_id: payload.x_grok_user_id.as_deref(),
        };
        let (http_request, sent_bearer_prefix) = self.post(self.endpoint("chat/completions"));
        let http_request = grok_headers.apply(http_request).json(&payload);

        let response = http_request.send().await.map_err(|e| {
            // Log at debug level; errors are surfaced to the caller.
            tracing::debug!("HTTP request failed: {}", e);
            e
        })?;

        self.handle_response(response, sent_bearer_prefix.as_deref())
            .await
    }

    /// Start a streaming chat completion request. Returns a stream of typed chunks.
    #[tracing::instrument(
        name = "http.chat_completion_stream",
        skip_all,
        fields(
            endpoint = %self.endpoint("chat/completions"),
            model_id = request.model.as_deref().unwrap_or(""),
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    pub async fn chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        let payload = self.apply_defaults(request)?;
        let x_grok_conv_id = &payload.x_grok_conv_id.clone().unwrap_or_default();
        let x_grok_req_id = &payload.x_grok_req_id.clone().unwrap_or_default();
        let model_id = payload.model.clone().unwrap_or_default();

        // Wrap the request with streaming fields and serialize once.
        // Previously this path serialized twice: first to serde_json::Value
        // (to inject `stream` and `stream_options`), then to HTTP body bytes.
        let streaming_request = StreamingChatRequest {
            inner: &payload,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: payload.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: payload.x_grok_turn_idx.as_deref(),
            agent_id: payload.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: payload.x_grok_deployment_id.as_deref(),
            user_id: payload.x_grok_user_id.as_deref(),
        };
        let (http_request, sent_bearer_prefix) = self.post(self.endpoint("chat/completions"));
        let http_request = grok_headers
            .apply(http_request)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .json(&streaming_request);

        let built_request = http_request.build().map_err(|e| {
            tracing::error!("Failed to build HTTP request: {}", e);
            SamplingError::Http(e)
        })?;

        tracing::debug!(
            url = %built_request.url(),
            method = %built_request.method(),
            "Sending chat/completions request"
        );
        Self::log_request_headers(&built_request, "chat/completions");

        let response = self.http.execute(built_request).await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            record_stream_request_failure(&e);
            e
        })?;

        let status = response.status();
        let span = tracing::Span::current();
        span.record("status_code", status.as_u16() as i64);
        span.record("success", status.is_success());
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("error", "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ChatCompletionsStream,
                    sent_bearer_prefix.as_deref(),
                );
                let endpoint = self.endpoint("chat/completions");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }

            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span.record("error", message.as_str());
            tracing::error!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "chat/completions API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        // Strip UTF-8 BOM if present: eventsource-stream 0.2.3 incorrectly slices BOM at byte 1 instead of 3.
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        // Turn raw bytes into SSE events
        let event_stream = byte_stream.eventsource();

        // Map SSE events into ChatCompletionChunk.
        // Uses `scan` so that `[DONE]` and transport errors both terminate the
        // stream (`None`). The first transport error is emitted to the consumer,
        // then subsequent polls return `None` -- preventing an infinite busy-loop
        // when the HTTP/2 connection drops and h2 keeps producing errors.
        let chunks = event_stream
            .scan(false, |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        tracing::info!(
                            target: crate::sampling_log::TARGET,
                            event = "sse_chunk",
                            backend = "chat_completions",
                            data = %data,
                        );

                        if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Err(stream_error))
                        } else {
                            Some(
                                serde_json::from_str::<ChatCompletionChunk>(data).map_err(|e| {
                                    tracing::error!(
                                        error = %e,
                                        raw_data = %data,
                                        "Failed to deserialize ChatCompletionChunk from stream"
                                    );
                                    SamplingError::Serialization(e)
                                }),
                            )
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Err(SamplingError::EventStreamError(e.to_string())))
                    }
                };
                std::future::ready(item)
            })
            .boxed();

        Ok((chunks, model_metadata))
    }

    // =========================================================================
    // Responses API
    // =========================================================================

    /// Apply default configuration to a Responses API request.
    fn apply_response_defaults(&self, request: &mut CreateResponseWrapper) -> Result<()> {
        // Apply model default if not specified
        if request.inner.model.is_none() {
            request.inner.model = Some(self.defaults.model.clone());
        }

        // Apply temperature default if not specified
        if request.inner.temperature.is_none() {
            request.inner.temperature = self.defaults.temperature;
        }

        // Apply top_p default if not specified
        if request.inner.top_p.is_none() {
            request.inner.top_p = self.defaults.top_p;
        }

        // Apply max_output_tokens default if not specified
        if request.inner.max_output_tokens.is_none() {
            request.inner.max_output_tokens = self.defaults.max_completion_tokens;
        }

        // Set store to false if not specified (default is true, but that breaks ZDR compliance)
        if request.inner.store.is_none() {
            request.inner.store = Some(false);
        }

        // Include encrypted reasoning content if not specified
        let includes = request.inner.include.get_or_insert_with(Vec::new);
        if !includes.contains(&rs::IncludeEnum::ReasoningEncryptedContent) {
            includes.push(rs::IncludeEnum::ReasoningEncryptedContent);
        }

        // OpenAI-compatible Responses: always pin prompt_cache_key for
        // session-affinity routing (automatic prefix cache). Codex dialect
        // does additional instruction reshaping below.
        stamp_responses_prompt_cache_key(request);

        if self.defaults.responses_codex_dialect {
            apply_codex_dialect(request);
        }

        Ok(())
    }

    /// Create a response using the Responses API (non-streaming).
    ///
    /// This uses the Responses API format which provides a simpler interface
    /// for multi-turn conversations and tool calling.
    pub async fn create_response(
        &self,
        mut request: CreateResponseWrapper,
    ) -> Result<rs::Response> {
        self.apply_response_defaults(&mut request)?;

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone().unwrap_or_default();

        // The trace field is process-local: it is consumed by upstream
        // session code (which may upload a payload artifact) and is not
        // forwarded by the sampler. Drop it before we send.
        request.trace.take();

        tracing::debug!("create_response: {:?}", &request);
        tracing::debug!("endpoint: {:?}", self.endpoint("responses"));

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let mut request_body = serde_json::to_value(&request.inner).map_err(|e| {
            tracing::error!("Failed to serialize responses request: {}", e);
            SamplingError::Serialization(e)
        })?;
        // async-openai's ReasoningTextContent struct omits the `type`
        // discriminator that the Responses API requires on input. Patch
        // it in post-serialize. This is the last surviving piece of the
        // old raw_output machinery.
        xai_grok_sampling_types::patch_reasoning_text_types(&mut request_body);
        if self.defaults.responses_codex_dialect {
            // ChatGPT Codex backend rejects non-stream requests:
            // `{"detail":"Stream must be set to true"}`.
            request_body["stream"] = serde_json::json!(true);
            strip_codex_unsupported_body_fields(&mut request_body);
            patch_codex_reasoning_effort_wire(
                &mut request_body,
                self.defaults.reasoning_effort,
            );
        }
        let (http_request, sent_bearer_prefix) = self.post(self.endpoint("responses"));
        let http_request = grok_headers.apply(http_request).json(&request_body);

        let response = http_request.send().await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            e
        })?;

        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::Responses,
                    sent_bearer_prefix.as_deref(),
                );
                let endpoint = self.endpoint("responses");
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }

            let message = user_facing_api_error_message(status, bytes.as_ref());
            tracing::warn!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "responses API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let response_obj = serde_json::from_slice::<rs::Response>(&bytes).map_err(|e| {
            let raw_body = String::from_utf8_lossy(&bytes);
            tracing::error!(
                error = %e,
                raw_body = %raw_body,
                "Failed to deserialize rs::Response"
            );
            SamplingError::Serialization(e)
        })?;
        Ok(response_obj)
    }

    /// Create a streaming response using the Responses API.
    ///
    /// Returns a stream of `rs::ResponseStreamEvent` which includes events like:
    /// - `response.created` - Initial response object
    /// - `response.output_text.delta` - Text content deltas
    /// - `response.function_call_arguments.delta` - Function call argument deltas
    /// - `response.completed` - Final response with all output
    ///
    /// The third tuple element is a per-request doom-loop signal collector,
    /// `Some` only when `SamplerConfig::doom_loop_recovery` is set — the same
    /// gate that adds the opt-in `x-grok-doom-loop-check` request header, so
    /// header and parse protection cannot drift apart. It is filled by the
    /// SSE decoder as the server reports triggers and is meant to be handed
    /// to `stream_responses` so the signals land on the final
    /// `ConversationResponse`.
    #[tracing::instrument(
        name = "http.create_response_stream",
        skip_all,
        fields(
            endpoint = %self.endpoint("responses"),
            model_id = request.inner.model.as_deref().unwrap_or(""),
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    #[allow(clippy::type_complexity)]
    pub async fn create_response_stream(
        &self,
        mut request: CreateResponseWrapper,
    ) -> Result<(
        BoxStream<'static, Result<ResponsesStreamItem>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        self.apply_response_defaults(&mut request)?;

        // Enable streaming
        request.inner.stream = Some(true);

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone().unwrap_or_default();

        // Drop process-local trace data (see note in `create_response`).
        request.trace.take();

        tracing::debug!(
            base_url = %self.base_url,
            model_id = model_id.as_str(),
            "Sending responses API stream request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let extra_tool_entries = std::mem::take(&mut request.extra_tool_entries);
        let mut request_body = serde_json::to_value(&request.inner).map_err(|e| {
            tracing::error!("Failed to serialize responses request: {}", e);
            SamplingError::Serialization(e)
        })?;
        // Inject xAI-specific fields not in async-openai's CreateResponse type.
        if self.defaults.stream_tool_calls {
            request_body["stream_tool_calls"] = serde_json::json!(true);
        }
        // Inject xAI-specific tools (e.g., x_search) that can't be expressed
        // via async_openai's rs::Tool enum.
        if !extra_tool_entries.is_empty() {
            if let Some(tools) = request_body.get_mut("tools").and_then(|v| v.as_array_mut()) {
                tools.extend(extra_tool_entries);
            } else {
                request_body["tools"] = serde_json::Value::Array(extra_tool_entries);
            }
        }
        xai_grok_sampling_types::patch_reasoning_text_types(&mut request_body);
        // Always force stream on the wire for Responses streaming calls.
        // ChatGPT Codex (`chatgpt.com/backend-api/codex/responses`) hard-requires
        // `stream: true` and returns FastAPI `{"detail":"Stream must be set to true"}`
        // otherwise — which our error parser used to collapse to a generic 400.
        request_body["stream"] = serde_json::json!(true);
        if self.defaults.responses_codex_dialect {
            strip_codex_unsupported_body_fields(&mut request_body);
            patch_codex_reasoning_effort_wire(
                &mut request_body,
                self.defaults.reasoning_effort,
            );
        }
        // Fresh per attempt so signals never leak across retries; `None`
        // (check disabled) sends no header and does no peek work per event.
        let doom_loop = self
            .defaults
            .doom_loop_recovery
            .map(crate::doom_loop::DoomLoopSignalCollector::new);
        let (http_request, sent_bearer_prefix) = self.post(self.endpoint("responses"));
        let mut http_request = grok_headers
            .apply(http_request)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if doom_loop.is_some() {
            // Presence opts in; the server ignores the value.
            http_request = http_request.header(DOOM_LOOP_CHECK_HEADER, "true");
        }
        let http_request = http_request.json(&request_body);

        let built_request = http_request.build().map_err(|e| {
            tracing::error!("Failed to build HTTP request: {}", e);
            SamplingError::Http(e)
        })?;

        tracing::debug!(
            url = %built_request.url(),
            method = %built_request.method(),
            "Sending responses API stream request"
        );
        Self::log_request_headers(&built_request, "responses");

        let response = self.http.execute(built_request).await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            record_stream_request_failure(&e);
            e
        })?;

        let status = response.status();
        let span = tracing::Span::current();
        span.record("status_code", status.as_u16() as i64);
        span.record("success", status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("error", "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ResponsesStream,
                    sent_bearer_prefix.as_deref(),
                );
                let endpoint = self.endpoint("responses");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span.record("error", message.as_str());
            tracing::error!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "responses API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let model_metadata = extract_model_metadata(response.headers());

        // Strip UTF-8 BOM if present
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        // Turn raw bytes into SSE events
        let event_stream = byte_stream.eventsource();

        let doom_loop_for_stream = doom_loop.clone();

        // Absorbed frames (doom-loop check events) and auxiliary frames both
        // surface as heartbeats: they carry no model output but prove server
        // liveness, which the layer-2 idle detector needs in order not to
        // mistake a long heartbeat-only phase for a dead connection.
        let events = event_stream
            .scan(false, move |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        tracing::info!(
                            target: crate::sampling_log::TARGET,
                            event = "sse_chunk",
                            backend = "responses",
                            data = %data,
                        );

                        // Intercept the non-standard doom-loop event before
                        // typed deserialization; async-openai's event enum
                        // does not know it and would fail to parse it. With
                        // the check disabled, the shared name-or-payload-type
                        // predicate guards against a server emitting it
                        // despite no opt-in (rollout skew), named or not.
                        let swallow = match &doom_loop_for_stream {
                            Some(collector) => collector.absorb(&event.event, data),
                            None => is_check_event(&event.event, data),
                        };
                        if swallow {
                            Ok(ResponsesStreamItem::Heartbeat)
                        } else {
                            decode_responses_sse_frame(&event.event, data)
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Err(SamplingError::EventStreamError(e.to_string()))
                    }
                };
                std::future::ready(Some(item))
            })
            .boxed();

        Ok((events, model_metadata, doom_loop))
    }

    // =========================================================================
    // Anthropic Messages API
    // =========================================================================

    /// Apply default configuration to a Messages API request.
    fn apply_message_defaults(&self, request: &mut MessagesRequestWrapper) -> Result<()> {
        // Apply model default if not specified
        if request.inner.model.is_empty() {
            request.inner.model = self.defaults.model.clone();
        }

        if request.inner.max_tokens == 0 {
            request.inner.max_tokens = self
                .defaults
                .max_completion_tokens
                .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);
        }

        // Apply temperature default if not specified
        if request.inner.temperature.is_none() {
            request.inner.temperature = self.defaults.temperature;
        }

        // Apply top_p default if not specified
        if request.inner.top_p.is_none() {
            request.inner.top_p = self.defaults.top_p;
        }

        Ok(())
    }

    /// Create a message using the Anthropic Messages API (non-streaming).
    pub async fn create_message(
        &self,
        mut request: MessagesRequestWrapper,
    ) -> Result<messages::MessagesResponse> {
        self.apply_message_defaults(&mut request)?;

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone();

        // Drop process-local trace data.
        request.trace.take();

        tracing::debug!("create_message: {:?}", &request.inner);
        tracing::debug!("endpoint: {:?}", self.endpoint("messages"));

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let (http_request, sent_bearer_prefix) = self.post(self.endpoint("messages"));
        let http_request = grok_headers.apply(http_request).json(&request.inner);

        let response = http_request.send().await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            e
        })?;

        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::Messages,
                    sent_bearer_prefix.as_deref(),
                );
                let endpoint = self.endpoint("messages");
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }

            let message = user_facing_api_error_message(status, bytes.as_ref());
            tracing::warn!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "messages API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let response_obj =
            serde_json::from_slice::<messages::MessagesResponse>(&bytes).map_err(|e| {
                let raw_body = String::from_utf8_lossy(&bytes);
                tracing::error!(
                    error = %e,
                    raw_body = %raw_body,
                    "Failed to deserialize MessagesResponse"
                );
                SamplingError::Serialization(e)
            })?;
        Ok(response_obj)
    }

    /// Create a streaming message using the Anthropic Messages API.
    ///
    /// Returns a stream of `MessageStreamEvent` which includes events like:
    /// - `message_start` - Initial message object
    /// - `content_block_start` / `content_block_delta` / `content_block_stop` - Content blocks
    /// - `message_delta` / `message_stop` - Final message with stop reason
    #[tracing::instrument(
        name = "http.create_message_stream",
        skip_all,
        fields(
            endpoint = %self.endpoint("messages"),
            model_id = request.inner.model.as_str(),
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    pub async fn create_message_stream(
        &self,
        mut request: MessagesRequestWrapper,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_message_defaults(&mut request)?;

        // Enable streaming
        request.inner.stream = Some(true);

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone();

        // Drop process-local trace data.
        request.trace.take();

        tracing::debug!(
            base_url = %self.base_url,
            model_id = model_id.as_str(),
            "Sending Messages API stream request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let (http_request, sent_bearer_prefix) = self.post(self.endpoint("messages"));
        let http_request = grok_headers
            .apply(http_request)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .json(&request.inner);

        let built_request = http_request.build().map_err(|e| {
            tracing::error!("Failed to build HTTP request: {}", e);
            SamplingError::Http(e)
        })?;

        tracing::debug!(
            url = %built_request.url(),
            method = %built_request.method(),
            "Sending messages API stream request"
        );
        Self::log_request_headers(&built_request, "messages");

        let response = self.http.execute(built_request).await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            record_stream_request_failure(&e);
            e
        })?;

        let status = response.status();
        let span = tracing::Span::current();
        span.record("status_code", status.as_u16() as i64);
        span.record("success", status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("error", "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::MessagesStream,
                    sent_bearer_prefix.as_deref(),
                );
                let endpoint = self.endpoint("messages");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span.record("error", message.as_str());
            tracing::error!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "messages API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let model_metadata = extract_model_metadata(response.headers());

        // Strip UTF-8 BOM if present
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        // Turn raw bytes into SSE events
        let event_stream = byte_stream.eventsource();

        // Map SSE events into MessageStreamEvent.
        // Uses `scan` so transport errors terminate the stream after the first
        // error (same pattern as `chat_completion_stream`).
        let events = event_stream
            .scan(false, |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        tracing::info!(
                            target: crate::sampling_log::TARGET,
                            event = "sse_chunk",
                            backend = "messages",
                            data = %data,
                        );

                        if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Err(stream_error))
                        } else {
                            Some(decode_messages_sse_frame(data))
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Err(SamplingError::EventStreamError(e.to_string())))
                    }
                };
                std::future::ready(item)
            })
            .boxed();

        Ok((events, model_metadata))
    }

    // =========================================================================
    // Unified Conversation API
    // =========================================================================

    /// Apply default configuration to a ConversationRequest.
    ///
    /// The actor calls this before constructing its Layer 2 stream so it can
    /// carry the exact route identity onto the accepted response.
    pub(crate) fn apply_conversation_defaults(
        &self,
        request: &mut ConversationRequest,
    ) -> Result<()> {
        if request.model.is_none() {
            request.model = Some(self.defaults.model.clone());
        }

        if request.temperature.is_none() {
            request.temperature = self.defaults.temperature;
        }

        if request.top_p.is_none() {
            request.top_p = self.defaults.top_p;
        }

        if request.max_output_tokens.is_none() {
            request.max_output_tokens = self.defaults.max_completion_tokens;
        }

        // The client is authoritative for the route that will actually carry
        // this request. Overwrite caller provenance so side requests (recap,
        // compaction, classifiers) cannot replay opaque reasoning across an
        // endpoint/backend switch merely because they reused the same slug.
        request.reasoning_model_identity = Some(ReasoningModelIdentity::new(
            request.model.clone().unwrap_or_default(),
            self.defaults.api_backend.clone(),
            &self.base_url,
        ));

        // Production always stamps dialect from SamplerConfig so bare
        // model-name matching cannot reshape Ollama/OpenRouter payloads.
        request.kimi_dialect = Some(self.defaults.kimi_dialect);

        Ok(())
    }

    /// Send a conversation request using the Chat Completions API (streaming).
    ///
    /// Converts the `ConversationRequest` to `ChatCompletionRequest` internally.
    /// Returns the stream and any model metadata extracted from response headers.
    pub async fn conversation_stream(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let mut chat_request: ChatCompletionRequest = request.into();
        if let Some(trace) = trace {
            chat_request.trace = Some(trace);
        }

        self.chat_completion_stream(chat_request).await
    }

    /// Send a conversation request using the Chat Completions API (non-streaming).
    ///
    /// Converts the `ConversationRequest` to `ChatCompletionRequest` internally.
    pub async fn conversation(
        &self,
        mut request: ConversationRequest,
    ) -> Result<ChatCompletionResponse> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let mut chat_request: ChatCompletionRequest = request.into();
        if let Some(trace) = trace {
            chat_request.trace = Some(trace);
        }

        self.chat_completion(chat_request).await
    }

    /// Send a conversation request using the Responses API (streaming).
    ///
    /// Converts the `ConversationRequest` to Responses API format internally.
    /// The third tuple element is the per-request doom-loop signal collector
    /// (see [`Self::create_response_stream`]); callers that don't consume the
    /// signals can ignore it.
    #[allow(clippy::type_complexity)]
    pub async fn conversation_stream_responses(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<ResponsesStreamItem>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        // Collect xAI-specific tools that can't be expressed via rs::Tool
        // (e.g., x_search). These are injected as raw JSON after serialization.
        let extra_tools = xai_grok_sampling_types::extra_tool_entries(&request.hosted_tools);

        let responses_request: rs::CreateResponse = (&request).into();

        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;
        wrapper.extra_tool_entries = extra_tools;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_response_stream(wrapper).await
    }

    /// Send a conversation request using the Responses API (non-streaming).
    ///
    /// Converts the `ConversationRequest` to Responses API format internally.
    pub async fn conversation_responses(
        &self,
        mut request: ConversationRequest,
    ) -> Result<rs::Response> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let responses_request: rs::CreateResponse = (&request).into();

        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_response(wrapper).await
    }

    /// Send a conversation request using the Anthropic Messages API (streaming).
    ///
    /// Converts the `ConversationRequest` to Messages API format internally.
    pub async fn conversation_stream_messages(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let messages_request = build_messages_request(&request);

        let mut wrapper = MessagesRequestWrapper::new(messages_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_message_stream(wrapper).await
    }

    /// Send a conversation request using the Anthropic Messages API (non-streaming).
    ///
    /// Converts the `ConversationRequest` to Messages API format internally.
    pub async fn conversation_messages(
        &self,
        mut request: ConversationRequest,
    ) -> Result<messages::MessagesResponse> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let messages_request = build_messages_request(&request);

        let mut wrapper = MessagesRequestWrapper::new(messages_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_message(wrapper).await
    }

    /// Backend-aware streaming call that collects the full response.
    pub async fn conversation_collect(
        &self,
        mut request: ConversationRequest,
    ) -> Result<ConversationResponse> {
        self.apply_conversation_defaults(&mut request)?;
        let reasoning_model_identity = request.reasoning_model_identity.clone();
        let request_id = crate::types::RequestId::random();
        let idle_timeout = std::time::Duration::from_secs(300);
        let result = match self.api_backend() {
            ApiBackend::ChatCompletions => {
                let (raw, meta) = self.conversation_stream(request).await?;
                let events =
                    crate::stream::stream_chat_completions(raw, meta, request_id, idle_timeout);
                crate::stream::collect_response(events).await
            }
            ApiBackend::Responses => {
                let (raw, meta, doom_loop) = self.conversation_stream_responses(request).await?;
                let events =
                    crate::stream::stream_responses(raw, meta, request_id, idle_timeout, doom_loop);
                crate::stream::collect_response(events).await
            }
            ApiBackend::Messages => {
                let (raw, meta) = self.conversation_stream_messages(request).await?;
                let events = crate::stream::stream_messages(raw, meta, request_id, idle_timeout);
                crate::stream::collect_response(events).await
            }
        };
        result
            .map(|(mut response, _metrics)| {
                for item in &mut response.items {
                    if let xai_grok_sampling_types::ConversationItem::Assistant(assistant) = item {
                        assistant.reasoning_model_identity = reasoning_model_identity.clone();
                    }
                }
                response
            })
            .map_err(|info| SamplingError::Api {
                status: info
                    .status_code
                    .and_then(|c| reqwest::StatusCode::from_u16(c).ok())
                    .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
                message: info.message,
                model_metadata: info.model_metadata,
                retry_after_secs: info.retry_after_secs,
                should_retry: None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use xai_grok_sampling_types::types::ChatRequestMessage;

    fn minimal_config() -> SamplerConfig {
        SamplerConfig {
            api_key: Some("test-key".to_string()),
            base_url: "https://example.test".to_string(),
            model: "test-model".to_string(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::ChatCompletions,
            auth_scheme: AuthScheme::Bearer,
            extra_headers: IndexMap::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window: 8192,
            force_http1: false,
            max_retries: None,
            stream_tool_calls: false,
            idle_timeout_secs: None,
            reasoning_effort: None,
            origin_client: None,
            client_identifier: None,
            deployment_id: None,
            user_id: None,
            client_version: None,
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

    #[test]
    fn conversation_defaults_use_the_actual_client_route_identity() {
        let cfg = SamplerConfig {
            base_url: "https://actual.example/v1/".to_string(),
            api_backend: ApiBackend::Responses,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let mut request = ConversationRequest {
            model: Some("side-request-model".to_string()),
            reasoning_model_identity: Some(ReasoningModelIdentity::new(
                "wrong-model",
                ApiBackend::Messages,
                "https://wrong.example/v1",
            )),
            ..Default::default()
        };

        client
            .apply_conversation_defaults(&mut request)
            .expect("defaults should apply");

        assert_eq!(
            request.reasoning_model_identity,
            Some(ReasoningModelIdentity::new(
                "side-request-model",
                ApiBackend::Responses,
                "https://actual.example/v1",
            ))
        );
    }

    /// Verify the serialized shape of StreamingChatRequest matches the
    /// expected wire format: all ChatCompletionRequest fields flattened at
    /// top level, plus `stream: true` and `stream_options.include_usage: true`.
    #[test]
    fn streaming_chat_request_serializes_correctly() {
        let request = ChatCompletionRequest {
            model: Some("test-model".into()),
            messages: vec![ChatRequestMessage::user("hello")],
            temperature: Some(0.7),
            max_tokens: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            user: None,
            prompt_cache_key: None,
            tools: None,
            tool_choice: None,
            search_parameters: None,
            response_format: None,
            reasoning_effort: None,
            thinking: None,
            x_grok_conv_id: None,
            x_grok_req_id: None,
            x_grok_session_id: None,
            x_grok_turn_idx: None,
            x_grok_agent_id: None,
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            trace: None,
        };

        let wrapper = StreamingChatRequest {
            inner: &request,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let json: serde_json::Value = serde_json::to_value(&wrapper).unwrap();
        let obj = json.as_object().unwrap();

        assert_eq!(obj.get("stream").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            obj.get("stream_options")
                .and_then(|v| v.get("include_usage"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );

        assert!(
            obj.get("inner").is_none(),
            "inner field should be flattened"
        );
        assert_eq!(
            obj.get("model").and_then(|v| v.as_str()),
            Some("test-model")
        );
        assert!(obj.get("messages").is_some());
        let temp = obj.get("temperature").and_then(|v| v.as_f64()).unwrap();
        assert!((temp - 0.7).abs() < 0.001, "temperature should be ~0.7");

        assert!(obj.get("max_tokens").is_none());
        assert!(obj.get("tools").is_none());
    }

    #[test]
    fn codex_dialect_moves_system_prompt_to_instructions() {
        use xai_grok_sampling_types::ConversationRequest;

        let mut config = minimal_config();
        config.api_backend = ApiBackend::Responses;
        config.responses_codex_dialect = true;
        let client = SamplingClient::new(config).unwrap();

        let request = ConversationRequest::from_items(vec![
            xai_grok_sampling_types::ConversationItem::system("you are codex"),
            xai_grok_sampling_types::ConversationItem::user("hello"),
        ]);
        let responses_request: rs::CreateResponse = (&request).into();
        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.x_grok_session_id = Some("session-123".to_string());

        client.apply_response_defaults(&mut wrapper).unwrap();
        let body = serde_json::to_value(&wrapper.inner).unwrap();

        assert_eq!(body["instructions"], "you are codex");
        assert_eq!(body["prompt_cache_key"], "session-123");
        assert_eq!(body["text"]["verbosity"], "low");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["store"], false);
        let input = body["input"].as_array().expect("input items");
        assert!(
            input.iter().all(|item| item["role"] != "system"),
            "system items must be lifted out of input: {input:?}"
        );
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn codex_dialect_off_keeps_system_in_input() {
        use xai_grok_sampling_types::ConversationRequest;

        let mut config = minimal_config();
        config.api_backend = ApiBackend::Responses;
        let client = SamplingClient::new(config).unwrap();

        let request = ConversationRequest::from_items(vec![
            xai_grok_sampling_types::ConversationItem::system("you are grok"),
            xai_grok_sampling_types::ConversationItem::user("hello"),
        ]);
        let responses_request: rs::CreateResponse = (&request).into();
        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.x_grok_session_id = Some("sess-openai".to_string());

        client.apply_response_defaults(&mut wrapper).unwrap();
        let body = serde_json::to_value(&wrapper.inner).unwrap();

        assert!(body.get("instructions").is_none());
        let input = body["input"].as_array().expect("input items");
        assert!(input.iter().any(|item| item["role"] == "system"));
        // Non-Codex OpenAI Responses still stamps prompt_cache_key for affinity.
        assert_eq!(body["prompt_cache_key"], "sess-openai");
    }

    #[test]
    fn responses_prompt_cache_key_from_conversation_request() {
        use xai_grok_sampling_types::ConversationRequest;

        let mut req = ConversationRequest::from_items(vec![
            xai_grok_sampling_types::ConversationItem::user("hi"),
        ]);
        req.prompt_cache_key = Some("explicit-key".into());
        req.prompt_cache_retention = Some("24h".into());
        let body: rs::CreateResponse = (&req).into();
        assert_eq!(body.prompt_cache_key.as_deref(), Some("explicit-key"));
        assert_eq!(
            body.prompt_cache_retention,
            Some(rs::PromptCacheRetention::Hours24)
        );
    }

    #[test]
    fn extract_retry_after_parses_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(30));
    }

    #[test]
    fn extract_retry_after_caps_at_120() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(120));
    }

    #[test]
    fn extract_retry_after_zero_is_valid() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(0));
    }

    #[test]
    fn extract_retry_after_ignores_http_date() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Fri, 31 Dec 2025 23:59:59 GMT".parse().unwrap(),
        );
        assert_eq!(extract_retry_after(&headers), None);
    }

    #[test]
    fn extract_retry_after_none_when_missing() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(extract_retry_after(&headers), None);
    }

    #[test]
    fn extract_should_retry_true() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "true".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(true));
    }

    #[test]
    fn extract_should_retry_true_case_insensitive() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "TRUE".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(true));
    }

    #[test]
    fn extract_should_retry_false() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "false".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(false));
    }

    #[test]
    fn extract_should_retry_unknown_value_is_none() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "banana".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), None);
    }

    #[test]
    fn extract_should_retry_absent_is_none() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(extract_should_retry(&headers), None);
    }

    #[test]
    fn new_with_minimal_config_succeeds() {
        let client = SamplingClient::new(minimal_config()).expect("client should construct");
        assert_eq!(client.api_backend(), ApiBackend::ChatCompletions);
    }

    #[test]
    fn new_applies_extra_headers() {
        let mut cfg = minimal_config();
        cfg.extra_headers
            .insert("x-test-header".to_string(), "test-value".to_string());
        cfg.extra_headers
            .insert("x-XAI-token-auth".to_string(), "xai-grok-cli".to_string());
        let _client = SamplingClient::new(cfg).expect("client with extra headers should construct");
    }

    #[test]
    fn apply_env_http_headers_resolves_trims_skips_and_overrides() {
        let mut map = IndexMap::new();
        map.insert("x-tenant-token".to_string(), "TENANT".to_string());
        map.insert("x-blank".to_string(), "BLANK".to_string());
        map.insert("x-missing".to_string(), "MISSING".to_string());
        map.insert("x-override".to_string(), "OVERRIDE".to_string());
        map.insert("x invalid".to_string(), "INVALID".to_string());

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-override"),
            HeaderValue::from_static("static"),
        );

        apply_env_http_headers(
            &map,
            |var| match var {
                // Leading space + trailing newline exercises trimming.
                "TENANT" => Some(" tenant-secret\n".to_string()),
                "BLANK" => Some("   ".to_string()),
                "OVERRIDE" => Some("from-env".to_string()),
                "INVALID" => Some("value".to_string()),
                _ => None,
            },
            &mut headers,
        );

        assert_eq!(headers.get("x-tenant-token").unwrap(), "tenant-secret");
        assert!(headers.get("x-blank").is_none());
        assert!(headers.get("x-missing").is_none());
        // A resolved env value overrides an existing header of the same name.
        assert_eq!(headers.get("x-override").unwrap(), "from-env");
        // An invalid header name is skipped rather than panicking.
        assert!(headers.get("x invalid").is_none());
    }

    #[test]
    fn endpoint_appends_path_before_a_base_url_query_without_configured_params() {
        let template =
            EndpointTemplate::new("https://gateway.example/v1?api-version=x", &IndexMap::new());
        let url = template.url_for_path("responses");
        assert!(
            url.starts_with("https://gateway.example/v1/responses?"),
            "url: {url}"
        );
        assert!(url.contains("api-version=x"), "url: {url}");
        assert!(!url.contains("x/responses"), "url: {url}");
    }

    #[test]
    fn messages_plus_anthropic_api_key_uses_x_api_key_and_not_authorization() {
        let cfg = SamplerConfig {
            api_key: Some("anthropic-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-api-key"))
                .is_some()
        );
        assert!(client.default_headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn messages_plus_bearer_uses_authorization_and_not_x_api_key() {
        let cfg = SamplerConfig {
            api_key: Some("bearer-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::Bearer,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(client.default_headers.get(AUTHORIZATION).is_some());
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-api-key"))
                .is_none()
        );
    }

    // Regression: a past change dropped User-Agent from sampling requests.
    #[test]
    fn sampling_client_always_has_user_agent() {
        let client = SamplingClient::new(minimal_config()).expect("build");
        assert!(client.default_headers.contains_key(USER_AGENT));
    }

    // Regression: a past change dropped HeaderInjector (traceparent) from sampling requests.
    #[test]
    fn header_injector_is_called_in_post() {
        #[derive(Debug)]
        struct TestInjector;
        impl crate::config::HeaderInjector for TestInjector {
            fn inject(&self, headers: &mut HeaderMap) {
                headers.insert(
                    HeaderName::from_static("traceparent"),
                    HeaderValue::from_static("00-test-trace-id-00"),
                );
            }
        }

        let mut config = minimal_config();
        config.header_injector = Some(std::sync::Arc::new(TestInjector));
        let client = SamplingClient::new(config).expect("build");
        let (req, _) = client.post("http://localhost/test");
        let req = req.build().expect("build request");
        assert!(
            req.headers().contains_key("traceparent"),
            "HeaderInjector should inject traceparent into post() requests"
        );
    }

    #[test]
    fn user_agent_includes_origin_and_agent_product() {
        let origin = OriginClientInfo {
            product: "my-client".to_string(),
            version: Some("1.2.3".to_string()),
        };
        let ua = user_agent_string_for(&origin);
        assert!(ua.contains("my-client/1.2.3"));
        assert!(ua.contains(AGENT_PRODUCT));
    }

    #[test]
    fn user_agent_omits_origin_version_when_absent() {
        let origin = OriginClientInfo {
            product: "my-client".to_string(),
            version: None,
        };
        let ua = user_agent_string_for(&origin);
        // No slash between product and the grok-shell agent product.
        assert!(ua.starts_with("my-client grok-shell/"));
    }

    #[test]
    fn user_agent_collapses_when_origin_matches_agent() {
        let agent_version = xai_grok_version::VERSION.to_string();
        let origin = OriginClientInfo {
            product: AGENT_PRODUCT.to_string(),
            version: Some(agent_version.clone()),
        };
        let ua = user_agent_string_for(&origin);
        // Single product/version slot when the origin and agent match.
        assert!(ua.starts_with(&format!("{}/{}", AGENT_PRODUCT, agent_version)));
    }

    /// Counts callbacks for assertions in the tests below.
    #[derive(Default, Debug)]
    struct CountingCallback {
        invocations: std::sync::Mutex<Vec<(crate::attribution::SamplingConsumer, Option<String>)>>,
    }

    #[derive(Debug)]
    struct StaticBearerResolver(&'static str);

    impl crate::config::BearerResolver for StaticBearerResolver {
        fn current_bearer(&self) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    #[derive(Debug)]
    struct MissingBearerResolver;

    impl crate::config::BearerResolver for MissingBearerResolver {
        fn current_bearer(&self) -> Option<String> {
            None
        }
    }

    #[derive(Debug)]
    struct CompanionHeaderResolver {
        account_id: Option<&'static str>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::config::BearerResolver for CompanionHeaderResolver {
        fn current_bearer(&self) -> Option<String> {
            panic!("post() must use the single resolve_bearer() result")
        }

        fn resolve_bearer(&self) -> crate::config::BearerResolution {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let name = HeaderName::from_static("chatgpt-account-id");
            let mut resolution =
                crate::config::BearerResolution::from_bearer(Some("fresh-bearer".to_string()));
            resolution.remove_headers.push(name.clone());
            if let Some(account_id) = self.account_id {
                resolution.headers.insert(
                    name,
                    HeaderValue::from_str(account_id).expect("valid account id"),
                );
            }
            resolution
        }
    }

    impl crate::attribution::Auth401AttributionCallback for CountingCallback {
        fn record_401(
            &self,
            consumer: crate::attribution::SamplingConsumer,
            sent_bearer: Option<&str>,
        ) {
            self.invocations
                .lock()
                .unwrap()
                .push((consumer, sent_bearer.map(|s| s.to_string())));
        }
    }

    /// `extract_sent_bearer` strips the `"Bearer "` prefix off
    /// `Authorization` for OpenAI-completions backends and truncates the
    /// remaining bearer to the cross-crate prefix length.
    #[test]
    fn extract_sent_bearer_strips_bearer_prefix_for_openai_compat() {
        let cfg = SamplerConfig {
            api_key: Some("test-bearer-1234567890".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let bearer = client.extract_sent_bearer();
        // Bearer is truncated at the crate boundary -- callers
        // downstream of this method only ever see the prefix.
        assert_eq!(bearer.as_deref(), Some("test-bearer-"));
        assert_eq!(
            bearer.as_deref().map(str::len),
            Some(crate::attribution::SENT_BEARER_PREFIX_LEN),
        );
    }

    /// `extract_sent_bearer` reads `x-api-key` for Anthropic Messages API
    /// and truncates the value to the cross-crate prefix length.
    #[test]
    fn extract_sent_bearer_reads_x_api_key_for_messages() {
        let cfg = SamplerConfig {
            api_key: Some("anthropic-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let bearer = client.extract_sent_bearer();
        assert_eq!(bearer.as_deref(), Some("anthropic-ke"));
        assert_eq!(
            bearer.as_deref().map(str::len),
            Some(crate::attribution::SENT_BEARER_PREFIX_LEN),
        );
    }

    /// `extract_sent_bearer` returns `None` when no auth header is set.
    #[test]
    fn extract_sent_bearer_returns_none_when_no_header() {
        let cfg = SamplerConfig {
            api_key: None,
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(client.extract_sent_bearer().is_none());
    }

    #[test]
    fn live_bearer_resolver_uses_authorization_for_messages_plus_bearer() {
        let cfg = SamplerConfig {
            api_key: Some("stale-bearer".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::Bearer,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-bearer"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (request, _) = client.post("https://example.test/v1/messages");
        let request = request.build().expect("request should build");
        let auth = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        assert_eq!(auth, Some("Bearer fresh-bearer"));
        assert!(request.headers().get("x-api-key").is_none());
    }

    #[test]
    fn invalid_live_bearer_is_not_attributed_as_sent() {
        let cfg = SamplerConfig {
            api_key: Some("stale-bearer".to_string()),
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("invalid\nbearer"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (request, sent_bearer_prefix) = client.post("https://example.test/v1/responses");
        let request = request.build().expect("request should build");

        assert!(request.headers().get(AUTHORIZATION).is_none());
        assert!(sent_bearer_prefix.is_none());
    }

    #[test]
    fn bearer_resolution_atomically_replaces_companion_headers() {
        for (account_id, expected) in [(Some("acct-new"), Some("acct-new")), (None, None)] {
            let mut cfg = minimal_config();
            cfg.api_key = Some("stale-bearer".to_string());
            cfg.extra_headers
                .insert("chatgpt-account-id".to_string(), "acct-stale".to_string());
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            cfg.bearer_resolver = Some(std::sync::Arc::new(CompanionHeaderResolver {
                account_id,
                calls: calls.clone(),
            }));
            let client = SamplingClient::new(cfg).expect("client should build");
            let info = client.auth_info();
            assert_eq!(info.auth_type, "bearer");
            assert!(info.auth_prefix.is_none());
            let (request, sent_bearer_prefix) = client.post("https://example.test/v1/responses");
            let request = request.build().expect("request should build");

            assert_eq!(
                request
                    .headers()
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer fresh-bearer")
            );
            assert_eq!(
                request
                    .headers()
                    .get("chatgpt-account-id")
                    .and_then(|value| value.to_str().ok()),
                expected
            );
            assert_eq!(sent_bearer_prefix.as_deref(), Some("fresh-bearer"));
            client.record_401_attribution(
                crate::attribution::SamplingConsumer::Responses,
                sent_bearer_prefix.as_deref(),
            );
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "span metadata, request auth, companion header, and 401 attribution must share one resolution"
            );
        }
    }

    #[test]
    fn missing_live_bearer_does_not_fall_back_to_stale_catalog_token() {
        let cfg = SamplerConfig {
            api_key: Some("expired-catalog-token".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::Bearer,
            bearer_resolver: Some(std::sync::Arc::new(MissingBearerResolver)),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (request, sent_bearer_prefix) = client.post("https://example.test/v1/messages");
        let request = request.build().expect("request should build");
        assert!(
            request.headers().get(AUTHORIZATION).is_none(),
            "failed refresh must not send the expired construction-time bearer"
        );
        assert!(request.headers().get("x-api-key").is_none());
        assert!(sent_bearer_prefix.is_none());
    }

    #[test]
    fn missing_live_x_api_key_does_not_fall_back_to_stale_catalog_key() {
        let cfg = SamplerConfig {
            api_key: Some("expired-catalog-key".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            bearer_resolver: Some(std::sync::Arc::new(MissingBearerResolver)),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (request, sent_bearer_prefix) = client.post("https://example.test/v1/messages");
        let request = request.build().expect("request should build");
        assert!(request.headers().get(AUTHORIZATION).is_none());
        assert!(
            request.headers().get("x-api-key").is_none(),
            "failed live resolution must not send a stale construction-time x-api-key"
        );
        assert!(sent_bearer_prefix.is_none());
    }

    /// Regression: when `api_key` (which seeds `default_headers` with an
    /// `Authorization: Bearer ...`) AND a `bearer_resolver` are both set,
    /// `post()` must produce **exactly one** `Authorization` header on the
    /// wire. The pre-fix code used `RequestBuilder::header(AUTHORIZATION, ...)`
    /// which appends rather than replaces, causing two identical
    /// `Authorization` headers and a 400 from cli-chat-proxy.
    #[test]
    fn post_emits_single_authorization_with_api_key_and_bearer_resolver() {
        let cfg = SamplerConfig {
            api_key: Some("stale-bearer".to_string()),
            api_backend: ApiBackend::Responses,
            auth_scheme: AuthScheme::Bearer,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-bearer"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (request, _) = client.post("https://example.test/v1/responses");
        let request = request.build().expect("request should build");
        let auth_count = request.headers().get_all(AUTHORIZATION).iter().count();
        assert_eq!(
            auth_count, 1,
            "expected exactly one Authorization header, got {auth_count}"
        );
        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer fresh-bearer"),
        );
    }

    #[test]
    fn live_bearer_resolver_uses_x_api_key_for_messages_plus_anthropic_api_key() {
        let cfg = SamplerConfig {
            api_key: Some("stale-anthropic".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-anthropic"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (request, _) = client.post("https://example.test/v1/messages");
        let request = request.build().expect("request should build");
        let api_key = request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok());
        assert_eq!(api_key, Some("fresh-anthropic"));
        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    /// Bearers shorter than the prefix length pass through unchanged.
    /// Defensive against the truncation logic inadvertently widening
    /// short bearers (no panics, no zero-padding).
    #[test]
    fn extract_sent_bearer_short_bearer_passes_through_unchanged() {
        let cfg = SamplerConfig {
            api_key: Some("abc".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert_eq!(client.extract_sent_bearer().as_deref(), Some("abc"));
    }

    /// `record_401_attribution` invokes the wired callback with the
    /// expected `consumer` and the truncated bearer prefix that the
    /// wire would carry. The key assertion is that the callback
    /// receives the prefix only -- the full bearer never crosses the
    /// crate boundary.
    #[test]
    fn record_401_attribution_invokes_callback_with_extracted_bearer() {
        let cb = std::sync::Arc::new(CountingCallback::default());
        let cb_dyn: crate::attribution::SharedAttributionCallback = cb.clone();
        let cfg = SamplerConfig {
            api_key: Some("the-bearer-1234567890-extra-tail".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            attribution_callback: Some(cb_dyn),
            bearer_resolver: None,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let sent_bearer_prefix = client.extract_sent_bearer();
        client.record_401_attribution(
            crate::attribution::SamplingConsumer::ChatCompletionsStream,
            sent_bearer_prefix.as_deref(),
        );
        let calls = cb.invocations.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            crate::attribution::SamplingConsumer::ChatCompletionsStream
        );
        // Prefix-only -- the `extra-tail` portion of the bearer is
        // dropped by `extract_sent_bearer` before the callback fires.
        assert_eq!(calls[0].1.as_deref(), Some("the-bearer-1"));
        assert_eq!(
            calls[0].1.as_deref().map(str::len),
            Some(crate::attribution::SENT_BEARER_PREFIX_LEN),
        );
    }

    /// When a bearer_resolver is wired but returns `None`, attribution must
    /// report no sent bearer (not the construction-time default header seed).
    #[test]
    fn bearer_resolver_none_attribution_ignores_default_headers() {
        #[derive(Debug)]
        struct EmptyResolver;
        impl crate::config::BearerResolver for EmptyResolver {
            fn current_bearer(&self) -> Option<String> {
                None
            }
        }

        let cfg = SamplerConfig {
            api_key: Some("stale-seed-token".to_string()),
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(std::sync::Arc::new(EmptyResolver)),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert_eq!(
            client.current_sent_bearer_prefix(),
            None,
            "resolver None must not attribute a stripped default seed"
        );
    }

    /// When a bearer_resolver is wired but returns `None` (hard-expired
    /// session with no live AT), default Authorization / x-api-key must be
    /// stripped so a stale seed key cannot ride the wire.
    #[test]
    fn bearer_resolver_none_strips_default_authorization() {
        #[derive(Debug)]
        struct EmptyResolver;
        impl crate::config::BearerResolver for EmptyResolver {
            fn current_bearer(&self) -> Option<String> {
                None
            }
        }

        let cfg = SamplerConfig {
            api_key: Some("stale-token".to_string()),
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(std::sync::Arc::new(EmptyResolver)),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (builder, sent_bearer_prefix) = client.post("https://example.test/v1/responses");
        let request = builder.body("").build().expect("request should build");
        assert!(
            request.headers().get(AUTHORIZATION).is_none(),
            "stale default Authorization must not be sent when resolver is empty"
        );
        assert!(
            sent_bearer_prefix.is_none(),
            "resolver None must not attribute a stripped default seed"
        );
    }

    /// Regression test: when a bearer_resolver is wired, `post()` must
    /// *replace* the Authorization header from `default_headers`, not
    /// append a second one. Duplicate Authorization headers cause
    /// Cloudflare to return 400 Bad Request.
    #[test]
    fn bearer_resolver_replaces_authorization_header() {
        #[derive(Debug)]
        struct StaticResolver(String);
        impl crate::config::BearerResolver for StaticResolver {
            fn current_bearer(&self) -> Option<String> {
                Some(self.0.clone())
            }
        }

        let resolver: crate::config::SharedBearerResolver =
            std::sync::Arc::new(StaticResolver("fresh-token".to_string()));
        let cfg = SamplerConfig {
            api_key: Some("stale-token".to_string()),
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(resolver),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");

        // Build a request to inspect the final headers.
        let (builder, _) = client.post("https://example.test/v1/responses");
        let request = builder.body("").build().expect("request should build");

        let auth_values: Vec<_> = request.headers().get_all(AUTHORIZATION).iter().collect();
        assert_eq!(
            auth_values.len(),
            1,
            "expected exactly one Authorization header, got {}: {:?}",
            auth_values.len(),
            auth_values
        );
        assert_eq!(
            auth_values[0].to_str().unwrap(),
            "Bearer fresh-token",
            "Authorization header should contain the resolver's fresh token"
        );
    }

    /// `record_401_attribution` is a no-op when `attribution_callback`
    /// is `None` (the BYOK / sampler-only path). The previous tests
    /// in this module construct clients without a callback and rely
    /// on this property holding.
    #[test]
    fn record_401_attribution_is_noop_without_callback() {
        let cfg = SamplerConfig {
            api_key: Some("bearer".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            attribution_callback: None,
            bearer_resolver: None,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        // Must not panic.
        client.record_401_attribution(
            crate::attribution::SamplingConsumer::ChatCompletions,
            Some("bearer"),
        );
    }

    #[test]
    fn decode_responses_sse_frame_maps_auxiliary_events_to_heartbeats() {
        for event_type in RESPONSES_AUXILIARY_EVENT_TYPES {
            let named = decode_responses_sse_frame(event_type, r#"{"side_band":true}"#);
            assert!(
                matches!(named, Ok(ResponsesStreamItem::Heartbeat)),
                "event: {event_type} should surface as a heartbeat"
            );

            let payload = serde_json::json!({
                "type": event_type,
                "sequence_number": 7,
                "metadata": { "request_id": "req_test" }
            })
            .to_string();
            let data_only = decode_responses_sse_frame("", &payload);
            assert!(
                matches!(data_only, Ok(ResponsesStreamItem::Heartbeat)),
                "type: {event_type} should surface as a heartbeat"
            );
        }
    }

    #[test]
    fn decode_responses_sse_frame_does_not_swallow_auxiliary_name_in_output_text() {
        let payload = serde_json::json!({
            "type": "response.output_text.delta",
            "sequence_number": 8,
            "item_id": "item_test",
            "output_index": 0,
            "content_index": 0,
            "delta": "literal response.metadata and keepalive text",
            "logprobs": []
        })
        .to_string();

        let Ok(ResponsesStreamItem::Event(rs::ResponseStreamEvent::ResponseOutputTextDelta(
            event,
        ))) = decode_responses_sse_frame("", &payload)
        else {
            panic!("a normal text delta containing auxiliary names must be preserved");
        };
        assert_eq!(event.delta, "literal response.metadata and keepalive text");
    }

    #[test]
    fn decode_responses_sse_frame_skips_unknown_event_types() {
        // Forward-compat: an event type this build does not model is a
        // heartbeat skip, not a fatal serialization error (codex-rs posture).
        let decoded = decode_responses_sse_frame(
            "",
            r#"{"type":"response.future_semantic_event","sequence_number":9}"#,
        );
        assert!(matches!(decoded, Ok(ResponsesStreamItem::Heartbeat)));
    }

    #[test]
    fn decode_responses_sse_frame_skips_unknown_output_item_kinds() {
        // A known frame whose nested item is an OutputItem kind async-openai
        // predates: skip the frame, keep the stream alive.
        let payload = serde_json::json!({
            "type": "response.output_item.done",
            "sequence_number": 10,
            "output_index": 0,
            "item": { "type": "brand_new_item_kind", "id": "item_1" }
        })
        .to_string();
        let decoded = decode_responses_sse_frame("", &payload);
        assert!(matches!(decoded, Ok(ResponsesStreamItem::Heartbeat)));
    }

    #[test]
    fn decode_responses_sse_frame_keeps_malformed_known_events_strict() {
        // A known event type with a malformed payload stays a fatal
        // serialization error — forward-compat leniency must not hide wire
        // corruption on events we do model.
        let decoded = decode_responses_sse_frame(
            "",
            r#"{"type":"response.output_text.delta","sequence_number":9}"#,
        );
        assert!(matches!(
            decoded,
            Err(SamplingError::Serialization(_))
        ));
    }

    #[test]
    fn decode_messages_sse_frame_skips_unknown_event_types() {
        // Forward-compat: a Messages event type this build does not model is
        // a liveness Ping, not a fatal serialization error.
        let decoded = decode_messages_sse_frame(r#"{"type":"citation","index":0}"#);
        assert!(matches!(
            decoded,
            Ok(messages::MessageStreamEvent::Ping)
        ));
    }

    #[test]
    fn decode_messages_sse_frame_skips_unknown_content_block_kinds() {
        let decoded = decode_messages_sse_frame(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"brand_new_block","id":"b1"}}"#,
        );
        assert!(matches!(
            decoded,
            Ok(messages::MessageStreamEvent::Ping)
        ));

        let decoded = decode_messages_sse_frame(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"brand_new_delta","x":"y"}}"#,
        );
        assert!(matches!(
            decoded,
            Ok(messages::MessageStreamEvent::Ping)
        ));
    }

    #[test]
    fn decode_messages_sse_frame_keeps_malformed_known_events_strict() {
        // Missing required fields on a known type: still a fatal
        // serialization error (wire corruption must not be hidden).
        let decoded = decode_messages_sse_frame(r#"{"type":"content_block_stop"}"#);
        assert!(matches!(
            decoded,
            Err(SamplingError::Serialization(_))
        ));
    }

    #[test]
    fn decode_messages_sse_frame_parses_ping_and_text_delta() {
        assert!(matches!(
            decode_messages_sse_frame(r#"{"type":"ping"}"#),
            Ok(messages::MessageStreamEvent::Ping)
        ));
        let decoded = decode_messages_sse_frame(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        );
        assert!(matches!(
            decoded,
            Ok(messages::MessageStreamEvent::ContentBlockDelta { .. })
        ));
    }

    /// `response.completed` carrying
    /// `usage.context_details.{input_tokens, output_tokens}` rewrites
    /// `usage.total_tokens` in place to the live context length
    /// (`ctx.input + ctx.output`). Billing fields stay on the wire's
    /// cumulative values.
    #[test]
    fn deserialize_response_event_overrides_total_tokens_from_context_details() {
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "grok-build",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 6003,
                    "input_tokens_details": { "cached_tokens": 1984 },
                    "output_tokens": 711,
                    "output_tokens_details": { "reasoning_tokens": 388 },
                    "total_tokens": 6714,
                    "context_details": {
                        "input_tokens": 5022,
                        "output_tokens": 571
                    }
                }
            }
        }"#;
        let event = deserialize_response_event(sse)
            .expect("parse")
            .expect("event present");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        // Billing fields stay cumulative — unchanged by context_details.
        assert_eq!(usage.input_tokens, 6003);
        assert_eq!(usage.output_tokens, 711);
        assert_eq!(usage.input_tokens_details.cached_tokens, 1984);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 388);
        // total_tokens rewritten to ctx.input + ctx.output (5022 + 571).
        // NOT the wire's cumulative total (6714).
        assert_eq!(usage.total_tokens, 5_593);
    }

    #[test]
    fn deserialize_response_event_stashes_cost_in_metadata() {
        let make = |ticks: i64| {
            format!(
                r#"{{
                "type": "response.completed",
                "sequence_number": 0,
                "response": {{
                    "id": "resp_1", "object": "response", "created_at": 0,
                    "model": "grok-build", "status": "completed", "output": [],
                    "usage": {{
                        "input_tokens": 10,
                        "input_tokens_details": {{ "cached_tokens": 0 }},
                        "output_tokens": 5,
                        "output_tokens_details": {{ "reasoning_tokens": 0 }},
                        "total_tokens": 15,
                        "cost_in_usd_ticks": {ticks}
                    }}
                }}
            }}"#
            )
        };

        let event = deserialize_response_event(&make(78))
            .expect("parse")
            .expect("event present");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        assert_eq!(
            e.response
                .metadata
                .as_ref()
                .and_then(|m| m.get(COST_USD_TICKS_METADATA_KEY))
                .map(String::as_str),
            Some("78")
        );

        // The REST mapper backfills 0 for unbilled requests: no stash.
        let event = deserialize_response_event(&make(0))
            .expect("parse")
            .expect("event present");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        assert!(e.response.metadata.is_none());
    }

    #[test]
    fn deserialize_response_event_total_tokens_unchanged_when_context_details_absent() {
        // Older / non-Responses backends omit `context_details`.
        // `total_tokens` passes through from the wire unchanged.
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "grok-build",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 10000,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens": 100,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 10100
                }
            }
        }"#;
        let event = deserialize_response_event(sse)
            .expect("parse")
            .expect("event present");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        assert_eq!(usage.total_tokens, 10_100);
    }

    #[test]
    fn deserialize_response_event_total_tokens_unchanged_when_context_details_partial() {
        // Defensive: if the backend ever ships only one of the two
        // context_details fields, we don't have a complete picture of
        // the live context size, so leave `total_tokens` on the wire's
        // cumulative value instead of guessing (treating the missing
        // half as 0 would silently under-report).
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "grok-build",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 6003,
                    "input_tokens_details": { "cached_tokens": 1984 },
                    "output_tokens": 711,
                    "output_tokens_details": { "reasoning_tokens": 388 },
                    "total_tokens": 6714,
                    "context_details": {
                        "input_tokens": 5022
                    }
                }
            }
        }"#;
        let event = deserialize_response_event(sse)
            .expect("parse")
            .expect("event present");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        assert_eq!(usage.total_tokens, 6_714);
    }

    #[test]
    fn deserialize_response_event_ignores_context_details_on_non_terminal_events() {
        // Non-terminal events don't carry final usage; even if the backend ever
        // echoed `context_details` on one, we don't touch it.
        let sse = r#"{
            "type": "response.output_text.delta",
            "sequence_number": 0,
            "item_id": "item-1",
            "output_index": 0,
            "content_index": 0,
            "delta": "hello",
            "logprobs": []
        }"#;
        let event = deserialize_response_event(sse)
            .expect("non-terminal event parses")
            .expect("event present");
        assert!(matches!(
            event,
            rs::ResponseStreamEvent::ResponseOutputTextDelta(_)
        ));
    }

    /// openai/codex maps Ultra → Max before the Responses body is built
    /// (`reasoning_effort_for_request`). Sending `effort: "ultra"` 400s.
    #[test]
    fn codex_wire_maps_ultra_to_max_like_official_cli() {
        use xai_grok_sampling_types::ReasoningEffort as E;
        let mut body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "reasoning": { "effort": "xhigh", "summary": "auto" }
        });
        patch_codex_reasoning_effort_wire(&mut body, Some(E::Ultra));
        assert_eq!(body["reasoning"]["effort"], "max");
        patch_codex_reasoning_effort_wire(&mut body, Some(E::Max));
        assert_eq!(body["reasoning"]["effort"], "max");
        patch_codex_reasoning_effort_wire(&mut body, Some(E::Xhigh));
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        patch_codex_reasoning_effort_wire(&mut body, Some(E::None));
        assert!(body.get("reasoning").is_none());
    }

    /// Codex backend 400s on max_output_tokens/temperature/top_p — strip them.
    #[test]
    fn strip_codex_unsupported_body_fields_removes_sampling_knobs() {
        let mut body = serde_json::json!({
            "model": "gpt-5.5",
            "stream": true,
            "max_output_tokens": 131072,
            "temperature": 1.0,
            "top_p": 1.0,
            "max_tool_calls": 8,
            "instructions": "hi",
        });
        strip_codex_unsupported_body_fields(&mut body);
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("max_tool_calls").is_none());
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "gpt-5.5");
    }
}
