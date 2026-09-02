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
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tracing::Instrument;

use xai_grok_sampling_types::error::{
    parse_error_code, try_parse_stream_error, user_facing_api_error_message,
};
use xai_grok_sampling_types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ConversationRequest,
    ConversationResponse, CreateResponseWrapper, DOOM_LOOP_CHECK_HEADER, MaxTokensField,
    MessagesRequestWrapper, ReasoningModelIdentity, RequestCompat, ResponseModelMetadata, Result,
    SamplingError, SentCredential, SessionAffinityFormat, ThinkingFormat, build_messages_request,
    is_check_event, messages, rs,
};

use crate::pi_messages::PiMessagesEvent;
use crate::adapter::BackendAdapter;
use crate::config::{AuthScheme, OriginClientInfo, SamplerConfig};
use crate::events::SamplingErrorInfo;
use crate::types::ResponsesStreamItem;
use crate::span_timing::{ERROR, STATUS_CODE, SUCCESS, ItemClass, StreamSpanTiming};
use crate::stream_classify::{chat_chunk_class, message_event_class, responses_event_class};

// Re-export ApiBackend from the shared types crate for downstream callers.
pub use xai_grok_sampling_types::ApiBackend;

/// Process-level fallback for the `x-grok-client-identifier` header.
const DEFAULT_CLIENT_IDENTIFIER: &str = "grok-shell";

/// Product identifier baked into User-Agent strings.
const AGENT_PRODUCT: &str = "grok-shell";
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 128_000;
const RESPONSES_AUXILIARY_EVENT_TYPES: [&str; 2] = ["keepalive", "response.metadata"];
const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
const CODEX_TURN_STATE_CACHE_CAPACITY: usize = 256;

/// True for xAI / SpaceXAI first-party sampling endpoints that may receive
/// product/session `x-grok-*` headers.
///
/// Requirements:
/// - Scheme must be **`https`** (cleartext never gets product metadata).
/// - Host is an allowlisted first-party suffix (`*.x.ai`, `*.spacexai.com`,
///   `cli-chat-proxy.grok.com`). Matching is suffix-safe
///   (`evil-x.ai.example` is rejected).
///
/// Third-party base URLs (OpenAI-compatible proxies, Azure, localhost mocks,
/// attacker-controlled hosts) must **not** receive session ids, deployment
/// ids, client identifiers, or other product metadata. Authorization and
/// other non-`x-grok-*` headers are unaffected.
///
/// Cross-origin redirect safety is enforced separately: the shared sampling
/// HTTP clients follow only **HTTPS same-origin** redirects (custom policy),
/// so `x-grok-*` cannot be forwarded to a third-party Location target.
pub fn is_first_party_grok_endpoint(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    // x.ai apex + subdomains (api.x.ai, …).
    if host == "x.ai" || host.ends_with(".x.ai") {
        return true;
    }
    // SpaceXAI branding / possible future product hosts.
    if host == "spacexai.com" || host.ends_with(".spacexai.com") {
        return true;
    }
    // Production cli-chat-proxy (and nested subdomains if any).
    if host == "cli-chat-proxy.grok.com" || host.ends_with(".cli-chat-proxy.grok.com") {
        return true;
    }
    false
}

/// Remove every `x-grok-*` header. Used when the configured base URL is not a
/// first-party endpoint so product/session identity cannot leak to third parties
/// via `extra_headers`, env headers, or late injectors.
fn strip_x_grok_headers(headers: &mut HeaderMap) {
    let keys: Vec<HeaderName> = headers
        .keys()
        .filter(|name| {
            let s = name.as_str();
            s.len() >= 7 && s.as_bytes()[..7].eq_ignore_ascii_case(b"x-grok-")
        })
        .cloned()
        .collect();
    for key in keys {
        headers.remove(key);
    }
}

/// `(provider route, session id, turn id)`. The provider route is deliberately
/// query-free so credentials embedded in query parameters never enter the
/// process-wide cache.
type CodexTurnKey = (String, String, String);
type CodexTurnStateCache = Arc<Mutex<HashMap<CodexTurnKey, String>>>;
type CodexCompactUnsupportedCache = Arc<Mutex<HashSet<String>>>;

fn shared_codex_turn_state_cache() -> CodexTurnStateCache {
    static CACHE: std::sync::OnceLock<CodexTurnStateCache> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

fn shared_codex_compact_unsupported_cache() -> CodexCompactUnsupportedCache {
    static CACHE: std::sync::OnceLock<CodexCompactUnsupportedCache> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(Mutex::new(HashSet::new())))
        .clone()
}

fn compact_endpoint_is_unsupported(status: reqwest::StatusCode, message: &str) -> bool {
    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::METHOD_NOT_ALLOWED
            | reqwest::StatusCode::NOT_IMPLEMENTED
    ) {
        return true;
    }
    let message = message.to_ascii_lowercase();
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        // Some Codex-compatible relays report a missing compact-model route as
        // a 503 even though their ordinary Responses route remains healthy.
        // Keep this narrow so transient and generic server failures still
        // surface instead of being hidden by local compaction.
        return message.contains("no available channel") && message.contains("-openai-compact");
    }
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    let names_endpoint = message.contains("endpoint")
        || message.contains("route")
        || message.contains("responses/compact");
    let says_missing = message.contains("not found")
        || message.contains("unknown")
        || message.contains("not support")
        || message.contains("unsupported endpoint")
        || message.contains("unsupported route");
    names_endpoint && says_missing
}

fn json_header(value: &serde_json::Value, name: &str) -> Option<String> {
    value.as_object()?.iter().find_map(|(key, value)| {
        if !key.eq_ignore_ascii_case(name) {
            return None;
        }
        match value {
            serde_json::Value::String(value) => Some(value.clone()),
            serde_json::Value::Array(values) => values
                .first()
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            _ => None,
        }
    })
}

fn codex_event_turn_state(data: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    (value.get("type").and_then(serde_json::Value::as_str) == Some("response.metadata")).then(
        || {
            value
                .get("headers")
                .and_then(|headers| json_header(headers, X_CODEX_TURN_STATE_HEADER))
        },
    )?
}

fn codex_event_response_model(data: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    value
        .pointer("/response/headers")
        .and_then(|headers| {
            json_header(headers, "openai-model").or_else(|| json_header(headers, "x-openai-model"))
        })
        .or_else(|| {
            value.get("headers").and_then(|headers| {
                json_header(headers, "openai-model")
                    .or_else(|| json_header(headers, "x-openai-model"))
            })
        })
        .filter(|model| !model.is_empty())
}

fn take_codex_turn_state(cache: &CodexTurnStateCache, key: &CodexTurnKey) -> Option<String> {
    let mut cache = cache.lock().ok()?;
    cache.retain(|(route, session_id, turn_id), _| {
        route != &key.0 || session_id != &key.1 || turn_id == &key.2
    });
    cache.get(key).cloned()
}

fn cache_codex_turn_state(cache: &CodexTurnStateCache, key: &CodexTurnKey, value: String) {
    if value.is_empty() {
        return;
    }
    if let Ok(mut cache) = cache.lock() {
        // A turn-state token is scoped to exactly one user turn. Discard an
        // older token for the same provider/session before recording the new
        // one. Identical Hyper session ids can safely target two providers.
        cache.retain(|(route, session_id, turn_id), _| {
            route != &key.0 || session_id != &key.1 || turn_id == &key.2
        });
        if cache.len() >= CODEX_TURN_STATE_CACHE_CAPACITY
            && !cache.contains_key(key)
            && let Some(evicted) = cache.keys().next().cloned()
        {
            cache.remove(&evicted);
        }
        cache.insert(key.clone(), value);
    }
}

#[derive(Debug, serde::Deserialize)]
struct CodexTerminalEnvelope {
    #[serde(rename = "type")]
    kind: String,
    response: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct CodexTerminalError {
    code: Option<String>,
    message: Option<String>,
}

fn codex_retry_after_seconds(code: Option<&str>, message: Option<&str>) -> Option<u64> {
    if code != Some("rate_limit_exceeded") {
        return None;
    }
    let words: Vec<&str> = message?.split_whitespace().collect();
    let marker = words.windows(3).position(|window| {
        window[0].eq_ignore_ascii_case("try")
            && window[1].eq_ignore_ascii_case("again")
            && window[2].eq_ignore_ascii_case("in")
    })?;
    let raw_value = words
        .get(marker + 3)?
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.');
    let split = raw_value
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(raw_value.len());
    let (number, suffix) = raw_value.split_at(split);
    let unit = if suffix.is_empty() {
        words.get(marker + 4).copied().unwrap_or_default()
    } else {
        suffix
    };
    let numeric = number.parse::<f64>().ok()?;
    let unit = unit
        .trim_matches(|ch: char| !ch.is_ascii_alphabetic())
        .to_ascii_lowercase();
    let seconds = if unit == "ms" {
        numeric / 1000.0
    } else {
        numeric
    };
    // Hyper's cross-process error contract stores whole seconds. Preserve a
    // sub-second retry as zero (immediate retry) rather than stretching e.g.
    // Codex's 28ms hint into a full second; round longer fractional delays up.
    Some(if seconds < 1.0 {
        0
    } else {
        seconds.ceil().clamp(0.0, 120.0) as u64
    })
}

/// Match the current codex-rs terminal-event policy while translating it to
/// Hyper's existing status/retry contract. This is Codex-only; the strict
/// Responses route continues through async-openai unchanged.
fn codex_terminal_error(data: &str) -> Option<SamplingError> {
    let event: CodexTerminalEnvelope = serde_json::from_str(data).ok()?;
    if event.kind == "response.incomplete" {
        let reason = event
            .response
            .as_ref()
            .and_then(|response| response.pointer("/incomplete_details/reason"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        if reason == "max_output_tokens" {
            return Some(SamplingError::MaxTokensTruncation);
        }
        return Some(SamplingError::Api {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("Incomplete response returned, reason: {reason}"),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: Some(true),
            error_code: None,
        });
    }
    if event.kind != "response.failed" {
        return None;
    }

    let error = event
        .response
        .as_ref()
        .and_then(|response| response.get("error"))
        .and_then(|error| serde_json::from_value::<CodexTerminalError>(error.clone()).ok());
    let code = error.as_ref().and_then(|error| error.code.as_deref());
    let supplied_message = error.as_ref().and_then(|error| error.message.as_deref());
    let default_message = code.unwrap_or("response.failed event received");
    let message = supplied_message
        .filter(|message| !message.trim().is_empty())
        .unwrap_or(default_message)
        .to_string();

    let (status, should_retry, message) = match code {
        Some("context_length_exceeded") => (
            reqwest::StatusCode::BAD_REQUEST,
            Some(false),
            format!("context_length_exceeded: {message}"),
        ),
        Some("insufficient_quota" | "usage_not_included") => {
            (reqwest::StatusCode::PAYMENT_REQUIRED, Some(false), message)
        }
        Some("invalid_prompt" | "bio_policy") => {
            (reqwest::StatusCode::BAD_REQUEST, Some(false), message)
        }
        Some("cyber_policy") => (
            reqwest::StatusCode::FORBIDDEN,
            Some(false),
            if supplied_message.is_some_and(|message| !message.trim().is_empty()) {
                message
            } else {
                "This request has been flagged for possible cybersecurity risk.".to_string()
            },
        ),
        Some("rate_limit_exceeded") => {
            (reqwest::StatusCode::TOO_MANY_REQUESTS, Some(true), message)
        }
        Some("server_is_overloaded" | "slow_down") => (
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            Some(true),
            message,
        ),
        _ => (
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            Some(true),
            message,
        ),
    };
    Some(SamplingError::Api {
        status,
        retry_after_secs: codex_retry_after_seconds(code, supplied_message),
        message,
        model_metadata: None,
        should_retry,
        error_code: None,
    })
}

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
///
/// Only applied for first-party xAI/SpaceXAI endpoints (see
/// [`is_first_party_grok_endpoint`]); third-party base URLs get an unchanged
/// builder so session/product identity never leaves first-party hosts.
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
    fn apply(
        &self,
        builder: reqwest::RequestBuilder,
        first_party: bool,
    ) -> reqwest::RequestBuilder {
        if !first_party {
            return builder;
        }
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

fn infer_copilot_initiator(body: &serde_json::Value) -> &'static str {
    if let Some(messages) = body.get("messages").and_then(serde_json::Value::as_array)
        && let Some(last) = messages.last()
    {
        let role = last.get("role").and_then(serde_json::Value::as_str);
        if role != Some("user") {
            return "agent";
        }

        // Anthropic Messages represents an internal `toolResult` as a user
        // message containing only `tool_result` blocks. Pi decides from the
        // pre-conversion role, where that item is not a user turn, so retain
        // `agent` here instead of mistaking the wire role for a human prompt.
        let only_tool_results = last
            .get("content")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|content| {
                !content.is_empty()
                    && content.iter().all(|block| {
                        block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
                    })
            });
        return if only_tool_results { "agent" } else { "user" };
    }
    if let Some(input) = body.get("input").and_then(serde_json::Value::as_array)
        && let Some(last) = input.last()
    {
        // Responses serializes local tool results as output items without a
        // role. In Pi the source message role is `toolResult`, hence agent-
        // initiated. Handle it before walking back to the preceding message.
        if last
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind.ends_with("_call_output"))
        {
            return "agent";
        }
        if let Some(last_message) = input.iter().rev().find(|item| {
            item.get("role")
                .and_then(serde_json::Value::as_str)
                .is_some()
                || item.get("type").and_then(serde_json::Value::as_str) == Some("message")
        }) {
            return if last_message.get("role").and_then(serde_json::Value::as_str) == Some("user") {
                "user"
            } else {
                "agent"
            };
        }
    }
    "user"
}

fn value_has_copilot_image(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            matches!(
                map.get("type").and_then(serde_json::Value::as_str),
                Some("image" | "image_url" | "input_image")
            ) || map.values().any(value_has_copilot_image)
        }
        serde_json::Value::Array(items) => items.iter().any(value_has_copilot_image),
        _ => false,
    }
}

fn apply_copilot_dynamic_headers(
    builder: reqwest::RequestBuilder,
    body: &serde_json::Value,
) -> reqwest::RequestBuilder {
    let mut builder = builder
        .header("X-Initiator", infer_copilot_initiator(body))
        .header("Openai-Intent", "conversation-edits");
    if value_has_copilot_image(body) {
        builder = builder.header("Copilot-Vision-Request", "true");
    }
    builder
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

/// Response SSE shapes differ between the public/xAI Responses API and Codex
/// providers. The latter intentionally permits sparse lifecycle envelopes,
/// matching the loose parser used by the official Codex CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsesWireDialect {
    Strict,
    Codex,
}

/// Fill fields required by async-openai's full `Response` type when a Codex
/// provider emits the sparse lifecycle envelopes accepted by codex-rs.
///
/// Only missing fields are synthesized. In particular, `response.id` remains
/// required so a malformed terminal event cannot silently complete a turn.
fn normalize_codex_response_event(value: &mut serde_json::Value, requested_model: &str) {
    let Some(event_type) = value.get("type").and_then(serde_json::Value::as_str) else {
        return;
    };
    let lifecycle_status = match event_type {
        "response.created" | "response.in_progress" => Some("in_progress"),
        "response.completed" => Some("completed"),
        "response.failed" => Some("failed"),
        "response.incomplete" => Some("incomplete"),
        "response.queued" => Some("queued"),
        _ => None,
    };

    let Some(event) = value.as_object_mut() else {
        return;
    };
    event
        .entry("sequence_number")
        .or_insert(serde_json::Value::from(0));
    let Some(status) = lifecycle_status else {
        return;
    };
    let top_level_model = event.get("headers").and_then(|headers| {
        json_header(headers, "openai-model").or_else(|| json_header(headers, "x-openai-model"))
    });
    let Some(response) = event
        .get_mut("response")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };

    response
        .entry("created_at")
        .or_insert(serde_json::Value::from(0));
    let event_model = response
        .get("headers")
        .and_then(|headers| {
            json_header(headers, "openai-model").or_else(|| json_header(headers, "x-openai-model"))
        })
        .or(top_level_model)
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| requested_model.to_string());
    if response
        .get("model")
        .and_then(serde_json::Value::as_str)
        .is_none_or(str::is_empty)
    {
        response.insert("model".to_string(), serde_json::Value::String(event_model));
    }
    response
        .entry("object")
        .or_insert(serde_json::Value::String("response".to_string()));
    response
        .entry("output")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    response
        .entry("status")
        .or_insert(serde_json::Value::String(status.to_string()));

    // Codex-compatible relays sometimes include aggregate usage but omit the
    // detail objects required by async-openai. Preserve supplied counters and
    // default only absent fields, as the official Codex parser does.
    let Some(usage) = response
        .get_mut("usage")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    usage
        .entry("input_tokens")
        .or_insert(serde_json::Value::from(0));
    usage
        .entry("output_tokens")
        .or_insert(serde_json::Value::from(0));
    usage
        .entry("total_tokens")
        .or_insert(serde_json::Value::from(0));

    let input_details = usage
        .entry("input_tokens_details")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(input_details) = input_details.as_object_mut() {
        input_details
            .entry("cached_tokens")
            .or_insert(serde_json::Value::from(0));
    }
    let output_details = usage
        .entry("output_tokens_details")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(output_details) = output_details.as_object_mut() {
        output_details
            .entry("reasoning_tokens")
            .or_insert(serde_json::Value::from(0));
    }
}

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
pub(crate) fn deserialize_response_event(
    data: &str,
    dialect: ResponsesWireDialect,
) -> Result<Option<rs::ResponseStreamEvent>> {
    deserialize_response_event_for_model(data, dialect, "")
}

fn deserialize_response_event_for_model(
    data: &str,
    dialect: ResponsesWireDialect,
    requested_model: &str,
) -> Result<Option<rs::ResponseStreamEvent>> {
    let mut normalized_value =
        match dialect {
            ResponsesWireDialect::Strict => None,
            ResponsesWireDialect::Codex => serde_json::from_str::<serde_json::Value>(data)
                .ok()
                .map(|mut value| {
                    normalize_codex_response_event(&mut value, requested_model);
                    value
                }),
        };
    let first_result = match normalized_value.as_ref() {
        Some(value) => serde_json::from_value::<rs::ResponseStreamEvent>(value.clone()),
        None => serde_json::from_str::<rs::ResponseStreamEvent>(data),
    };
    let first_err = match first_result {
        Ok(mut event) => {
            apply_terminal_event_overrides(&mut event, data);
            return Ok(Some(event));
        }
        Err(first_err) => first_err,
    };

    // Try sanitizing: parse as Value, drop what async-openai can't model, retry.
    let Some(mut value) = normalized_value
        .take()
        .or_else(|| serde_json::from_str::<serde_json::Value>(data).ok())
    else {
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
#[cfg(test)]
fn decode_responses_sse_frame(
    event_name: &str,
    data: &str,
    dialect: ResponsesWireDialect,
) -> std::result::Result<ResponsesStreamItem, SamplingError> {
    decode_responses_sse_frame_for_model(event_name, data, dialect, "")
}

fn decode_responses_sse_frame_for_model(
    event_name: &str,
    data: &str,
    dialect: ResponsesWireDialect,
    requested_model: &str,
) -> std::result::Result<ResponsesStreamItem, SamplingError> {
    if is_responses_auxiliary_event(event_name, data) {
        return Ok(ResponsesStreamItem::Heartbeat);
    }

    if dialect == ResponsesWireDialect::Codex
        && let Some(terminal_error) = codex_terminal_error(data)
    {
        Err(terminal_error)
    } else if let Some(stream_error) = try_parse_stream_error(data) {
        Err(stream_error)
    } else {
        match deserialize_response_event_for_model(data, dialect, requested_model) {
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
                "content_block_start" => value.get("content_block").is_some_and(|b| {
                    serde_json::from_value::<messages::ContentBlock>(b.clone()).is_err()
                }),
                "content_block_delta" => value.get("delta").is_some_and(|d| {
                    serde_json::from_value::<messages::StreamDelta>(d.clone()).is_err()
                }),
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
        .or_else(|| request.x_grok_conv_id.as_deref().filter(|s| !s.is_empty()));
    if let Some(key) = key {
        request.inner.prompt_cache_key = Some(xai_grok_sampling_types::clamp_prompt_cache_key(key));
    }
}

/// Match `openai/codex` provider detection for Azure Responses endpoints.
/// Azure requires `store: true`; ordinary OpenAI and compatible relays use
/// stateless `store: false` together with encrypted reasoning replay.
fn is_azure_responses_endpoint(base_url: &str) -> bool {
    let base_url = base_url.to_ascii_lowercase();
    const AZURE_MARKERS: [&str; 6] = [
        "openai.azure.",
        "cognitiveservices.azure.",
        "aoai.azure.",
        "azure-api.",
        "azurefd.",
        "windows.net/openai",
    ];
    AZURE_MARKERS.iter().any(|marker| base_url.contains(marker))
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
    // Raw tool entries are reserved for xAI extensions such as `x_search`.
    // Codex providers accept the native Responses `web_search` tool only.
    request.extra_tool_entries.clear();
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
    // Codex serializes both fields on every request, even when tools is empty.
    if request.inner.tool_choice.is_none() {
        request.inner.tool_choice = Some(rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Auto));
    }
    if request.inner.parallel_tool_calls.is_none() {
        request.inner.parallel_tool_calls = Some(true);
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

fn patch_codex_request_body(
    body: &mut serde_json::Value,
    request: &CreateResponseWrapper,
    reasoning_effort: Option<xai_grok_sampling_types::ReasoningEffort>,
) {
    strip_codex_unsupported_body_fields(body);
    patch_codex_reasoning_effort_wire(body, reasoning_effort);
    // Codex's canonical request struct always serializes `tools`, including
    // an empty array. Some relays validate that fixed request shape.
    if body.get("tools").is_none() {
        body["tools"] = serde_json::Value::Array(Vec::new());
    }

    // `async-openai::CreateResponse` predates Codex's client_metadata. Send
    // only stable identities Hyper owns; do not fabricate installation,
    // window, attestation, or first-party product metadata.
    let mut metadata = serde_json::Map::new();
    if let Some(session_id) = request
        .x_grok_session_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        metadata.insert(
            "session_id".to_string(),
            serde_json::Value::String(session_id.to_string()),
        );
    }
    if let Some(thread_id) = request
        .x_grok_conv_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            request
                .x_grok_session_id
                .as_deref()
                .filter(|value| !value.is_empty())
        })
    {
        metadata.insert(
            "thread_id".to_string(),
            serde_json::Value::String(thread_id.to_string()),
        );
    }
    if let Some(turn_id) = request
        .x_grok_turn_idx
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        metadata.insert(
            "turn_id".to_string(),
            serde_json::Value::String(turn_id.to_string()),
        );
    }
    if !metadata.is_empty() {
        body["client_metadata"] = serde_json::Value::Object(metadata);
    }
}

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
const MISTRAL_REASONING_EFFORT_MODELS: [&str; 3] = [
    "mistral-small-2603",
    "mistral-small-latest",
    "mistral-medium-3.5",
];

fn mistral_uses_reasoning_effort(model: &str) -> bool {
    MISTRAL_REASONING_EFFORT_MODELS.contains(&model)
}

fn derive_mistral_tool_call_id(id: &str, attempt: u32) -> String {
    let normalized: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if attempt == 0 && normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed = if attempt == 0 {
        if normalized.is_empty() {
            id.to_string()
        } else {
            normalized
        }
    } else {
        format!(
            "{}:{attempt}",
            if normalized.is_empty() {
                id
            } else {
                &normalized
            }
        )
    };
    let digest = Sha256::digest(seed.as_bytes());
    let hex = format!("{digest:x}");
    hex.chars().take(MISTRAL_TOOL_CALL_ID_LENGTH).collect()
}

fn normalize_mistral_tool_call_id(
    id: &str,
    id_map: &mut std::collections::BTreeMap<String, String>,
    reverse_map: &mut std::collections::BTreeMap<String, String>,
) -> String {
    if let Some(existing) = id_map.get(id) {
        return existing.clone();
    }
    let mut attempt = 0;
    loop {
        let candidate = derive_mistral_tool_call_id(id, attempt);
        match reverse_map.get(&candidate) {
            None => {
                id_map.insert(id.to_string(), candidate.clone());
                reverse_map.insert(candidate.clone(), id.to_string());
                return candidate;
            }
            Some(owner) if owner == id => return candidate,
            Some(_) => attempt += 1,
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

fn responses_stream_item_class(item: &ResponsesStreamItem) -> ItemClass {
    match item {
        ResponsesStreamItem::Event(event) => responses_event_class(event),
        ResponsesStreamItem::Heartbeat => ItemClass::Other,
    }
}

/// Splice the raw-JSON hosted-tool entries for `web_search` and `x_search` into a serialized
/// Responses request body's `tools` array. `x_search` has no `rs::Tool` variant at all, and
/// `web_search` has one whose typed filters cannot carry `excluded_domains`, so both travel as raw
/// JSON and neither may also be emitted as a typed `rs::Tool` (the API rejects the duplicate).
/// Shared by the streaming (`create_response_stream`) and non-streaming (`create_response`) paths
/// so neither can silently drop these tools.
fn splice_extra_tool_entries(
    request_body: &mut serde_json::Value,
    entries: Vec<serde_json::Value>,
) {
    if entries.is_empty() {
        return;
    }
    if let Some(tools) = request_body.get_mut("tools").and_then(|v| v.as_array_mut()) {
        tools.extend(entries);
    } else {
        request_body["tools"] = serde_json::Value::Array(entries);
    }
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

fn append_response_includes(body: &mut serde_json::Value, extra_includes: &[String]) {
    if extra_includes.is_empty() {
        return;
    }
    let Some(body) = body.as_object_mut() else {
        return;
    };
    let include = body.entry("include").or_insert(serde_json::Value::Null);
    if include.is_null() {
        *include = serde_json::Value::Array(Vec::new());
    }
    let Some(include) = include.as_array_mut() else {
        return;
    };
    for value in extra_includes {
        if !include
            .iter()
            .any(|existing| existing.as_str() == Some(value.as_str()))
        {
            include.push(serde_json::Value::String(value.clone()));
        }
    }
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

fn auth_header_name(auth_scheme: AuthScheme) -> HeaderName {
    match auth_scheme {
        AuthScheme::Bearer => AUTHORIZATION,
        AuthScheme::XApiKey => HeaderName::from_static("x-api-key"),
        AuthScheme::ApiKey => HeaderName::from_static("api-key"),
        AuthScheme::CfAigAuthorization => HeaderName::from_static("cf-aig-authorization"),
        AuthScheme::XGoogApiKey => HeaderName::from_static("x-goog-api-key"),
    }
}

fn auth_header_value(auth_scheme: AuthScheme, api_key: &str) -> Option<HeaderValue> {
    let value = match auth_scheme {
        AuthScheme::Bearer | AuthScheme::CfAigAuthorization => format!("Bearer {api_key}"),
        AuthScheme::XApiKey | AuthScheme::ApiKey | AuthScheme::XGoogApiKey => api_key.to_string(),
    };
    HeaderValue::from_str(&value).ok()
}

fn remove_known_auth_headers(headers: &mut HeaderMap) {
    headers.remove(AUTHORIZATION);
    headers.remove(HeaderName::from_static("x-api-key"));
    headers.remove(HeaderName::from_static("api-key"));
    headers.remove(HeaderName::from_static("cf-aig-authorization"));
    headers.remove(HeaderName::from_static("x-goog-api-key"));
}

/// Keep exactly the typed auth header. This runs after every other header
/// source so Cloudflare Gateway can never inherit an upstream provider key.
fn normalize_auth_headers(
    headers: &mut HeaderMap,
    auth_scheme: AuthScheme,
    desired: Option<HeaderValue>,
) {
    remove_known_auth_headers(headers);
    if let Some(value) = desired {
        headers.insert(auth_header_name(auth_scheme), value);
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
    google_adc: Arc<crate::google::VertexAdcTokenProvider>,
    /// Sticky-routing tokens returned by Codex providers. The shared cache is
    /// keyed by provider route + session + user turn so HTTP client rebuilds
    /// retain the token, while a new turn or different provider can never
    /// replay an older route's state.
    codex_turn_state: CodexTurnStateCache,
    /// Provider routes that explicitly rejected `/responses/compact`. Shared
    /// across rebuilt clients so automatic compaction does not probe the same
    /// unsupported relay on every turn.
    codex_compact_unsupported: CodexCompactUnsupportedCache,
    first_use_noted: Arc<AtomicBool>,
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
    adapter: BackendAdapter,
    configured_api_backend: ApiBackend,
    request_compat: Option<RequestCompat>,
    endpoint_path: Option<String>,
    auth_scheme: AuthScheme,
    stream_tool_calls: bool,
    extra_response_includes: Vec<String>,
    doom_loop_recovery: Option<xai_grok_sampling_types::DoomLoopRecoveryPolicy>,
    /// Session reasoning effort. Needed for Codex wire rewrite: async-openai
    /// only types through `xhigh`, so Max/Ultra must be restored post-serialize.
    reasoning_effort: Option<xai_grok_sampling_types::ReasoningEffort>,
    bedrock_request_metadata: IndexMap<String, String>,
    bedrock_headers: IndexMap<String, String>,
    bedrock_profile: Option<String>,
    compact_timeout: std::time::Duration,
}

impl ClientDefaults {
    fn chat_compat(&self) -> Option<&xai_grok_sampling_types::OpenAiCompletionsCompat> {
        self.request_compat
            .as_ref()
            .and_then(RequestCompat::chat_completions)
    }

    fn responses_compat(&self) -> Option<&xai_grok_sampling_types::OpenAiResponsesCompat> {
        self.request_compat
            .as_ref()
            .and_then(RequestCompat::responses)
    }

    fn messages_compat(&self) -> Option<&xai_grok_sampling_types::AnthropicMessagesCompat> {
        self.request_compat
            .as_ref()
            .and_then(RequestCompat::messages)
    }
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

/// A request builder coupled to the credential state it was built with, so
/// a 401 arm cannot classify from anything but the build-time capture. The
/// wire default (`SentCredential::Unknown`, which charges the retry budget)
/// stays the fail-closed one; only an explicit `sent_bearer: None` — a send
/// the builder provably stamped no credential onto — reaches the uncharged
/// lane via [`auth_rejected`].
struct SentRequest {
    builder: reqwest::RequestBuilder,
    /// Tail fragment of the credential in the built headers (`None` = no
    /// credential header at all).
    sent_bearer: Option<String>,
}

/// The one way a 401 becomes a `SamplingError::Auth` with a wire-derived
/// credential classification: from the fragment its [`SentRequest`] captured.
fn auth_rejected(message: String, sent_bearer: Option<&str>) -> SamplingError {
    SamplingError::Auth {
        message,
        credential: SentCredential::from_sent_fragment(sent_bearer),
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

        // Typed credentials are authoritative over static/env headers. When no
        // `api_key` is present, preserve an explicitly injected target header
        // but still remove conflicting authentication placements.
        let desired_auth = if let Some(api_key) = config.api_key.as_deref() {
            Some(
                auth_header_value(config.auth_scheme, api_key).ok_or_else(|| {
                    tracing::debug!("api_key could not be converted to a valid HTTP auth header");
                    SamplingError::auth_unknown(
                        "Invalid api_key: cannot be converted to a valid HTTP auth header",
                    )
                })?,
            )
        } else {
            headers.get(auth_header_name(config.auth_scheme)).cloned()
        };
        normalize_auth_headers(&mut headers, config.auth_scheme, desired_auth);

        // Product/session `x-grok-*` headers are first-party only. Third-party
        // base URLs (proxies, Azure, BYOK OpenAI, …) must not receive them —
        // including via extra_headers / env injection (strip below).
        let first_party = is_first_party_grok_endpoint(&config.base_url);
        if first_party {
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
        } else {
            // Drop any x-grok-* that arrived via extra_headers / env_http_headers.
            strip_x_grok_headers(&mut headers);
        }

        // Set User-Agent only when the concrete catalog route did not provide
        // one. Provider catalog identity (e.g. GitHub Copilot's Pi-pinned UA)
        // wins over Grok's generic client UA.
        if !headers.contains_key(USER_AGENT) {
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

        // Preserve the two legacy dialect booleans while model/session plumbing
        // migrates to the registry's typed adapter metadata. An explicit
        // adapter_kind always wins. `ApiBackend::CodexResponses` also forces
        // the Codex dialect for third-party reverse proxies.
        let adapter_kind = if config.adapter_kind != xai_grok_sampling_types::AdapterKind::Standard
        {
            config.adapter_kind
        } else if config.responses_codex_dialect || config.api_backend.uses_codex_dialect() {
            xai_grok_sampling_types::AdapterKind::OpenAiCodex
        } else if config.kimi_dialect {
            xai_grok_sampling_types::AdapterKind::KimiCoding
        } else {
            xai_grok_sampling_types::AdapterKind::Standard
        };
        let adapter = BackendAdapter::from_route(adapter_kind, config.api_backend.clone())?;
        adapter.ensure_implemented()?;

        let defaults = ClientDefaults {
            model: config.model,
            max_completion_tokens: config.max_completion_tokens,
            temperature: config.temperature,
            top_p: config.top_p,
            adapter,
            configured_api_backend: config.api_backend,
            request_compat: config.request_compat,
            endpoint_path: config.endpoint_path,
            auth_scheme: config.auth_scheme,
            stream_tool_calls: config.stream_tool_calls,
            extra_response_includes: config.extra_response_includes,
            doom_loop_recovery: config.doom_loop_recovery,
            reasoning_effort: config.reasoning_effort,
            bedrock_request_metadata: config.bedrock_request_metadata,
            bedrock_headers: config.bedrock_headers,
            bedrock_profile: config.bedrock_profile,
            compact_timeout: std::time::Duration::from_secs(
                config.idle_timeout_secs.unwrap_or(300).saturating_mul(4),
            ),
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
            google_adc: Arc::new(crate::google::VertexAdcTokenProvider::new()),
            codex_turn_state: shared_codex_turn_state_cache(),
            codex_compact_unsupported: shared_codex_compact_unsupported_cache(),
            first_use_noted: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Resolved provider adapter for this client.
    pub fn backend_adapter(&self) -> &BackendAdapter {
        &self.defaults.adapter
    }

    /// Existing wire backend used by the resolved adapter.
    pub fn api_backend(&self) -> ApiBackend {
        self.defaults
            .adapter
            .wire_backend()
            .expect("SamplingClient rejects unimplemented native adapters")
            .clone()
    }

    /// Configured route identity before provider adapters normalize it to a
    /// shared wire protocol.
    pub fn configured_api_backend(&self) -> ApiBackend {
        self.defaults.configured_api_backend.clone()
    }

    /// POST with default headers, returning the builder coupled to the tail
    /// fragment of the credential actually placed in its headers (`None` =
    /// no credential) — captured at build time because a record-time
    /// re-read races with the recovery a 401 triggers.
    ///
    /// A live resolver is evaluated exactly once; the returned fragment is
    /// carried to 401 attribution so logging never performs another potentially
    /// blocking credential lookup. A wired bearer_resolver is the sole auth
    /// source: a missing live bearer strips default Authorization / x-api-key
    /// so a hard-expired
    /// seed key cannot ride on the wire.
    fn post(&self, url: impl reqwest::IntoUrl) -> SentRequest {
        if !self.first_use_noted.load(Ordering::Relaxed)
            && !self.first_use_noted.swap(true, Ordering::Relaxed)
        {
            crate::prewarm::note_first_sampling_use(&self.base_url);
        }
        let mut headers = self.default_headers.clone();
        if let Some(resolver) = &self.bearer_resolver {
            // A resolver is authoritative. Remove construction-time auth even
            // when refresh returns `None`; otherwise an expired catalog stamp
            // is silently sent after a failed refresh.
            remove_known_auth_headers(&mut headers);
            let resolution = resolver.resolve_bearer();
            if let Some(fresh) = resolution.bearer
                && let Some(value) = auth_header_value(self.defaults.auth_scheme, &fresh)
            {
                headers.insert(auth_header_name(self.defaults.auth_scheme), value);
            }
            for name in resolution.remove_headers {
                headers.remove(name);
            }
            headers.extend(resolution.headers);
        }
        tracing::info!(
            target: crate::sampling_log::TARGET,
            event = "client_post",
            base_url = %self.base_url,
            model = %self.defaults.model,
            api_backend = ?self.api_backend(),
            auth_scheme = ?self.defaults.auth_scheme,
            has_bearer_resolver = self.bearer_resolver.is_some(),
            has_authorization_header = headers.get(AUTHORIZATION).is_some(),
            has_x_api_key_header = headers.get(HeaderName::from_static("x-api-key")).is_some(),
            has_api_key_header = headers.get(HeaderName::from_static("api-key")).is_some(),
            has_cf_aig_authorization_header = headers
                .get(HeaderName::from_static("cf-aig-authorization"))
                .is_some(),
        );
        // Preserve the resolved typed credential across late header injection.
        // Header injectors may add tracing metadata but may not change provider
        // authentication or reintroduce a conflicting standard auth header.
        let authoritative_auth = headers
            .get(auth_header_name(self.defaults.auth_scheme))
            .cloned();
        if let Some(injector) = &self.header_injector {
            injector.inject(&mut headers);
        }
        normalize_auth_headers(&mut headers, self.defaults.auth_scheme, authoritative_auth);
        // Privacy: never ship product/session `x-grok-*` to third-party bases,
        // even if a late injector or resolver reintroduced them. Auth headers
        // are intentionally left alone.
        if !is_first_party_grok_endpoint(&self.base_url) {
            strip_x_grok_headers(&mut headers);
        }
        let sent_bearer = self.extract_sent_bearer_from(&headers);
        SentRequest {
            builder: self.http.post(url).headers(headers),
            sent_bearer,
        }
    }

    /// Whether this client's base URL may receive product/session `x-grok-*`.
    fn allows_x_grok_headers(&self) -> bool {
        is_first_party_grok_endpoint(&self.base_url)
    }

    fn bearer_suffix(bearer: &str) -> String {
        xai_grok_auth::bearer_suffix(bearer).to_string()
    }

    /// Bearer tail fragment for 401 attribution. When a resolver is wired it is
    /// authoritative (including `None` ⇒ nothing was sent). Without a resolver,
    /// fall back to construction-time default headers.
    ///
    /// Prefer the tail fragment returned by [`Self::post`] for request-path
    /// attribution so the live credential is resolved only once.
    #[cfg_attr(not(test), allow(dead_code))]
    fn current_sent_bearer_suffix(&self) -> Option<String> {
        if self.bearer_resolver.is_some() {
            return self
                .bearer_resolver
                .as_ref()
                .and_then(|r| r.current_bearer())
                .map(|s| Self::bearer_suffix(&s));
        }
        self.extract_sent_bearer()
    }

    /// Extract the construction-time bearer as a bounded tail fragment.
    fn extract_sent_bearer(&self) -> Option<String> {
        self.extract_sent_bearer_from(&self.default_headers)
    }

    /// Extract the bearer that will actually be sent from a finalized header map.
    fn extract_sent_bearer_from(&self, headers: &HeaderMap) -> Option<String> {
        let raw = match self.defaults.auth_scheme {
            AuthScheme::XApiKey => headers
                .get(HeaderName::from_static("x-api-key"))
                .and_then(|v| v.to_str().ok()),
            AuthScheme::ApiKey => headers
                .get(HeaderName::from_static("api-key"))
                .and_then(|v| v.to_str().ok()),
            AuthScheme::Bearer => headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer ")),
            AuthScheme::XGoogApiKey => headers
                .get(HeaderName::from_static("x-goog-api-key"))
                .and_then(|v| v.to_str().ok()),
            AuthScheme::CfAigAuthorization => headers
                .get(HeaderName::from_static("cf-aig-authorization"))
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer ")),
        };
        raw.map(Self::bearer_suffix)
    }

    /// Attribute a 401 to the exact bearer already resolved for this request.
    fn record_401_attribution(
        &self,
        consumer: crate::attribution::SamplingConsumer,
        sent_bearer_suffix: Option<&str>,
    ) {
        if let Some(cb) = self.attribution_callback.as_ref() {
            cb.record_401(consumer, sent_bearer_suffix);
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
                AuthScheme::ApiKey => "api-key",
                AuthScheme::Bearer => "bearer",
                AuthScheme::CfAigAuthorization => "cf-aig-authorization",
                AuthScheme::XGoogApiKey => "x-goog-api-key",
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

    fn endpoint(&self, backend_default_path: &str) -> String {
        let path = self
            .defaults
            .endpoint_path
            .as_deref()
            .or_else(|| self.defaults.adapter.endpoint_path())
            .unwrap_or(backend_default_path);
        self.endpoint.url_for_path(path)
    }

    fn strip_strict_tool_fields(body: &mut serde_json::Value) {
        let Some(tools) = body
            .get_mut("tools")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return;
        };
        for tool in tools {
            if let Some(function) = tool
                .get_mut("function")
                .and_then(serde_json::Value::as_object_mut)
            {
                function.remove("strict");
            }
        }
    }

    fn normalize_mistral_tool_call_ids(body: &mut serde_json::Value) {
        let Some(messages) = body
            .get_mut("messages")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return;
        };
        let mut id_map = std::collections::BTreeMap::new();
        let mut reverse_map = std::collections::BTreeMap::new();
        for message in messages {
            if let Some(tool_calls) = message
                .get_mut("tool_calls")
                .and_then(serde_json::Value::as_array_mut)
            {
                for tool_call in tool_calls {
                    if let Some(id_value) = tool_call.get_mut("id")
                        && let Some(id) = id_value.as_str()
                    {
                        *id_value = serde_json::Value::String(normalize_mistral_tool_call_id(
                            id,
                            &mut id_map,
                            &mut reverse_map,
                        ));
                    }
                }
            }
            if let Some(tool_call_id) = message.get_mut("tool_call_id")
                && let Some(id) = tool_call_id.as_str()
            {
                *tool_call_id = serde_json::Value::String(normalize_mistral_tool_call_id(
                    id,
                    &mut id_map,
                    &mut reverse_map,
                ));
            }
        }
    }

    fn patch_mistral_request_body(&self, body: &mut serde_json::Value) {
        Self::normalize_mistral_tool_call_ids(body);
        let Some(object) = body.as_object_mut() else {
            return;
        };
        let effort = object.remove("reasoning_effort");
        let Some(effort) = effort else {
            return;
        };
        if effort.as_str() == Some("none") || effort.as_str() == Some("minimal") {
            return;
        }
        if mistral_uses_reasoning_effort(&self.defaults.model) {
            object.insert("reasoning_effort".into(), effort);
        } else {
            object.insert(
                "prompt_mode".into(),
                serde_json::Value::String("reasoning".into()),
            );
        }
    }

    /// Apply fully resolved Pi Chat Completions compatibility after typed
    /// serialization. Several gateway fields are not represented by the
    /// shared request type, so one post-serialize boundary is both safer and
    /// easier to audit than provider-specific request structs.
    fn patch_chat_request_body(&self, body: &mut serde_json::Value, streaming: bool) {
        if self.defaults.adapter.uses_mistral_conversations_dialect() {
            self.patch_mistral_request_body(body);
        }
        let Some(compat) = self.defaults.chat_compat() else {
            return;
        };
        let Some(object) = body.as_object_mut() else {
            return;
        };

        if compat.max_tokens_field == MaxTokensField::MaxCompletionTokens
            && let Some(value) = object.remove("max_tokens")
        {
            object.insert("max_completion_tokens".into(), value);
        }
        if compat.supports_store {
            object
                .entry("store")
                .or_insert(serde_json::Value::Bool(false));
        } else {
            object.remove("store");
        }
        if streaming && !compat.supports_usage_in_streaming {
            object.remove("stream_options");
        }
        if !compat.supports_strict_mode {
            Self::strip_strict_tool_fields(body);
        }
        if compat.zai_tool_stream && body.get("tools").is_some() {
            body["tool_stream"] = serde_json::Value::Bool(true);
        }
        if !compat.openrouter_routing.is_empty() {
            body["provider"] =
                serde_json::to_value(&compat.openrouter_routing).unwrap_or(serde_json::Value::Null);
        }
        if !compat.vercel_gateway_routing.is_empty() {
            body["providerOptions"] = serde_json::json!({
                "gateway": compat.vercel_gateway_routing,
            });
        }

        let effort = body
            .as_object_mut()
            .and_then(|object| object.remove("reasoning_effort"));
        let effort_string = effort.as_ref().and_then(serde_json::Value::as_str);
        match compat.thinking_format {
            ThinkingFormat::OpenAi => {
                if compat.supports_reasoning_effort
                    && let Some(effort) = effort
                {
                    body["reasoning_effort"] = effort;
                }
            }
            ThinkingFormat::OpenRouter => {
                if let Some(effort) = effort_string {
                    body["reasoning"] = serde_json::json!({ "effort": effort });
                }
            }
            ThinkingFormat::DeepSeek => {
                if effort_string.is_some() {
                    body["thinking"] = serde_json::json!({ "type": "enabled" });
                }
                if compat.supports_reasoning_effort
                    && let Some(effort) = effort
                {
                    body["reasoning_effort"] = effort;
                }
            }
            ThinkingFormat::Together => {
                body["reasoning"] = serde_json::json!({ "enabled": effort_string.is_some() });
                if compat.supports_reasoning_effort
                    && let Some(effort) = effort
                {
                    body["reasoning_effort"] = effort;
                }
            }
            ThinkingFormat::Zai => {
                body["thinking"] = if effort_string.is_some() {
                    serde_json::json!({ "type": "enabled", "clear_thinking": false })
                } else {
                    serde_json::json!({ "type": "disabled" })
                };
                if compat.supports_reasoning_effort
                    && let Some(effort) = effort
                {
                    body["reasoning_effort"] = effort;
                }
            }
            ThinkingFormat::Qwen => {
                body["enable_thinking"] = serde_json::Value::Bool(effort_string.is_some());
            }
            ThinkingFormat::QwenChatTemplate => {
                body["chat_template_kwargs"] = serde_json::json!({
                    "enable_thinking": effort_string.is_some(),
                    "preserve_thinking": true,
                });
            }
            ThinkingFormat::StringThinking => {
                if let Some(effort) = effort_string {
                    body["thinking"] = serde_json::Value::String(effort.to_string());
                }
            }
            ThinkingFormat::AntLing => {
                if let Some(effort) = effort_string {
                    body["reasoning"] = serde_json::json!({ "effort": effort });
                }
            }
            ThinkingFormat::ChatTemplate => {
                let mut kwargs = serde_json::Map::new();
                for (key, value) in &compat.chat_template_kwargs {
                    let resolved = match value.pointer("/$var").and_then(serde_json::Value::as_str)
                    {
                        Some("thinking.enabled") => {
                            serde_json::Value::Bool(effort_string.is_some())
                        }
                        Some("thinking.effort") => {
                            effort.clone().unwrap_or(serde_json::Value::Null)
                        }
                        _ => value.clone(),
                    };
                    kwargs.insert(key.clone(), resolved);
                }
                if !kwargs.is_empty() {
                    body["chat_template_kwargs"] = serde_json::Value::Object(kwargs);
                }
            }
        }

        if compat.requires_reasoning_content_on_assistant_messages
            && let Some(messages) = body
                .get_mut("messages")
                .and_then(serde_json::Value::as_array_mut)
        {
            for message in messages {
                if message.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                    && let Some(object) = message.as_object_mut()
                {
                    object
                        .entry("reasoning_content")
                        .or_insert_with(|| serde_json::Value::String(String::new()));
                }
            }
        }
    }

    fn apply_chat_session_affinity(
        &self,
        mut builder: reqwest::RequestBuilder,
        session_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
            return builder;
        };
        if self.defaults.adapter.uses_mistral_conversations_dialect() {
            builder = builder.header("x-affinity", session_id);
        }
        let Some(compat) = self.defaults.chat_compat() else {
            return builder;
        };
        if !compat.send_session_affinity_headers {
            return builder;
        }
        match compat.session_affinity_format {
            SessionAffinityFormat::OpenRouter => builder.header("x-session-id", session_id),
            SessionAffinityFormat::OpenAi => {
                builder = builder.header("session_id", session_id);
                builder = builder.header("x-client-request-id", session_id);
                builder.header("x-session-affinity", session_id)
            }
            SessionAffinityFormat::OpenAiNoSession => {
                builder = builder.header("x-client-request-id", session_id);
                builder.header("x-session-affinity", session_id)
            }
        }
    }

    fn apply_responses_session_affinity(
        &self,
        mut builder: reqwest::RequestBuilder,
        session_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let Some(compat) = self.defaults.responses_compat() else {
            return builder;
        };
        let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
            return builder;
        };
        match compat.session_affinity_format {
            SessionAffinityFormat::OpenRouter => builder.header("x-session-id", session_id),
            SessionAffinityFormat::OpenAi => {
                builder = builder.header("session_id", session_id);
                builder.header("x-client-request-id", session_id)
            }
            SessionAffinityFormat::OpenAiNoSession => {
                builder.header("x-client-request-id", session_id)
            }
        }
    }

    fn apply_messages_session_affinity(
        &self,
        builder: reqwest::RequestBuilder,
        session_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let Some(compat) = self.defaults.messages_compat() else {
            return builder;
        };
        let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
            return builder;
        };
        if compat.send_session_affinity_headers {
            builder.header("x-session-affinity", session_id)
        } else {
            builder
        }
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
                .or_else(|| request.x_grok_conv_id.as_deref().filter(|s| !s.is_empty()));
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
        sent_bearer: Option<&str>,
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
                    sent_bearer,
                );
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401): {server_message}"),
                    sent_bearer,
                ));
            }
            let message = user_facing_api_error_message(status, bytes.as_ref());
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
                error_code: parse_error_code(bytes.as_ref()),
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
        let mut request_body = serde_json::to_value(&payload).map_err(|error| {
            tracing::error!(%error, "failed to serialize chat/completions request");
            SamplingError::Serialization(error)
        })?;
        self.patch_chat_request_body(&mut request_body, false);
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("chat/completions"));
        let mut http_request = self.apply_chat_session_affinity(
            grok_headers.apply(builder, self.allows_x_grok_headers()),
            payload.x_grok_session_id.as_deref(),
        );
        if self.defaults.adapter.uses_github_copilot_dialect() {
            http_request = apply_copilot_dynamic_headers(http_request, &request_body);
        }
        let http_request = http_request.json(&request_body);

        let response = http_request.send().await.map_err(|e| {
            // Log at debug level; errors are surfaced to the caller.
            tracing::debug!("HTTP request failed: {}", e);
            e
        })?;

        self.handle_response(response, sent_bearer.as_deref()).await
    }

    async fn execute_stream_request(
        &self,
        built_request: reqwest::Request,
        span_timing: &mut StreamSpanTiming,
    ) -> Result<reqwest::Response> {
        span_timing.record_request_build();
        let response = self.http.execute(built_request).await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            span_timing.record_transport_failure(&e.to_string());
            e
        })?;
        span_timing.record_response_headers();
        Ok(response)
    }

    /// Start a streaming chat completion request. Returns a stream of typed chunks.
    pub async fn chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        let region = crate::span_timing::stream_span!(
            "http.chat_completion_stream",
            endpoint = %self.endpoint("chat/completions"),
            model_id = request.model.as_deref().unwrap_or(""),
        );
        if region.span().is_disabled() {
            self.chat_completion_stream_inner(request, region).await
        } else {
            let span = region.span().clone();
            self.chat_completion_stream_inner(request, region)
                .instrument(span)
                .await
        }
    }

    async fn chat_completion_stream_inner(
        &self,
        request: ChatCompletionRequest,
        region: crate::span_timing::Region,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        let mut span_timing = StreamSpanTiming::start(region);
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
        let mut request_body = serde_json::to_value(&streaming_request).map_err(|error| {
            tracing::error!(%error, "failed to serialize streaming chat/completions request");
            SamplingError::Serialization(error)
        })?;
        self.patch_chat_request_body(&mut request_body, true);

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
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("chat/completions"));
        let mut http_request = self
            .apply_chat_session_affinity(
                grok_headers.apply(builder, self.allows_x_grok_headers()),
                payload.x_grok_session_id.as_deref(),
            )
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if self.defaults.adapter.uses_github_copilot_dialect() {
            http_request = apply_copilot_dynamic_headers(http_request, &request_body);
        }
        let http_request = http_request.json(&request_body);

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

        let response = self
            .execute_stream_request(built_request, &mut span_timing)
            .await?;

        let status = response.status();
        span_timing
            .span()
            .record(STATUS_CODE, status.as_u16() as i64);
        span_timing.span().record(SUCCESS, status.is_success());
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span_timing.span().record(ERROR, "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ChatCompletionsStream,
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("chat/completions");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
            }

            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span_timing.span().record(ERROR, message.as_str());
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
                error_code: parse_error_code(bytes.as_ref()),
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

        Ok((
            span_timing.hold_until_first_content(chunks, chat_chunk_class),
            model_metadata,
        ))
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
        if self
            .defaults
            .responses_compat()
            .is_some_and(|compat| !compat.supports_long_cache_retention)
        {
            request.inner.prompt_cache_retention = None;
        }

        if self.defaults.adapter.uses_openai_codex_dialect() {
            apply_codex_dialect(request);
            if is_azure_responses_endpoint(&self.base_url) {
                request.inner.store = Some(true);
            }
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
        if self.defaults.adapter.uses_openai_codex_dialect() {
            return Err(SamplingError::InvalidConfiguration(
                "Codex Responses requires the streaming Responses API path",
            ));
        }
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
        let extra_tool_entries = std::mem::take(&mut request.extra_tool_entries);
        let mut request_body = serde_json::to_value(&request.inner).map_err(|e| {
            tracing::error!("Failed to serialize responses request: {}", e);
            SamplingError::Serialization(e)
        })?;
        splice_extra_tool_entries(&mut request_body, extra_tool_entries);
        append_response_includes(&mut request_body, &self.defaults.extra_response_includes);
        // async-openai's ReasoningTextContent struct omits the `type`
        // discriminator that the Responses API requires on input. Patch
        // it in post-serialize. This is the last surviving piece of the
        // old raw_output machinery.
        xai_grok_sampling_types::patch_reasoning_text_types(&mut request_body);
        if self
            .defaults
            .responses_compat()
            .is_some_and(|compat| !compat.supports_strict_mode)
        {
            Self::strip_strict_tool_fields(&mut request_body);
        }
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("responses"));
        let mut http_request = self.apply_responses_session_affinity(
            grok_headers.apply(builder, self.allows_x_grok_headers()),
            request.x_grok_session_id.as_deref(),
        );
        if self.defaults.adapter.uses_github_copilot_dialect() {
            http_request = apply_copilot_dynamic_headers(http_request, &request_body);
        }
        let http_request = http_request.json(&request_body);

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
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("responses");
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
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
                error_code: parse_error_code(bytes.as_ref()),
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
    #[allow(clippy::type_complexity)]
    pub async fn create_response_stream(
        &self,
        request: CreateResponseWrapper,
    ) -> Result<(
        BoxStream<'static, Result<ResponsesStreamItem>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        let region = crate::span_timing::stream_span!(
            "http.create_response_stream",
            endpoint = %self.endpoint("responses"),
            model_id = request.inner.model.as_deref().unwrap_or(""),
        );
        if region.span().is_disabled() {
            self.create_response_stream_inner(request, region).await
        } else {
            let span = region.span().clone();
            self.create_response_stream_inner(request, region)
                .instrument(span)
                .await
        }
    }

    #[allow(clippy::type_complexity)]
    async fn create_response_stream_inner(
        &self,
        mut request: CreateResponseWrapper,
        region: crate::span_timing::Region,
    ) -> Result<(
        BoxStream<'static, Result<ResponsesStreamItem>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        let mut span_timing = StreamSpanTiming::start(region);
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
        splice_extra_tool_entries(&mut request_body, extra_tool_entries);
        append_response_includes(&mut request_body, &self.defaults.extra_response_includes);
        xai_grok_sampling_types::patch_reasoning_text_types(&mut request_body);
        if self
            .defaults
            .responses_compat()
            .is_some_and(|compat| !compat.supports_strict_mode)
        {
            Self::strip_strict_tool_fields(&mut request_body);
        }
        // Always force stream on the wire for Responses streaming calls.
        // ChatGPT Codex (`chatgpt.com/backend-api/codex/responses`) hard-requires
        // `stream: true` and returns FastAPI `{"detail":"Stream must be set to true"}`
        // otherwise — which our error parser used to collapse to a generic 400.
        request_body["stream"] = serde_json::json!(true);
        if self.defaults.adapter.uses_openai_codex_dialect() {
            patch_codex_request_body(&mut request_body, &request, self.defaults.reasoning_effort);
        }
        // Fresh per attempt so signals never leak across retries; `None`
        // (check disabled) sends no header and does no peek work per event.
        let doom_loop = self
            .defaults
            .doom_loop_recovery
            .map(crate::doom_loop::DoomLoopSignalCollector::new);
        let responses_endpoint = self.endpoint("responses");
        let codex_route = responses_endpoint
            .split(['?', '#'])
            .next()
            .unwrap_or(responses_endpoint.as_str())
            .trim_end_matches('/')
            .to_string();
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(responses_endpoint);
        let codex_turn_key = self
            .defaults
            .adapter
            .uses_openai_codex_dialect()
            .then_some(codex_route)
            .and_then(|route| {
                request
                    .x_grok_session_id
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .zip(
                        request
                            .x_grok_turn_idx
                            .as_deref()
                            .filter(|value| !value.is_empty()),
                    )
                    .map(|(session_id, turn_id)| {
                        (route, session_id.to_string(), turn_id.to_string())
                    })
            });
        let mut http_request = self
            .apply_responses_session_affinity(
                grok_headers.apply(builder, self.allows_x_grok_headers()),
                request.x_grok_session_id.as_deref(),
            )
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if self.defaults.adapter.uses_openai_codex_dialect()
            && let Some(turn_state) = codex_turn_key
                .as_ref()
                .and_then(|key| take_codex_turn_state(&self.codex_turn_state, key))
        {
            http_request = http_request.header(X_CODEX_TURN_STATE_HEADER, turn_state);
        }
        if let Some(policy) = self.defaults.doom_loop_recovery {
            if self.allows_x_grok_headers() {
                http_request =
                    http_request.header(DOOM_LOOP_CHECK_HEADER, policy.window_tokens.to_string());
            }
        }
        if self.defaults.adapter.uses_github_copilot_dialect() {
            http_request = apply_copilot_dynamic_headers(http_request, &request_body);
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

        let response = self
            .execute_stream_request(built_request, &mut span_timing)
            .await?;

        let status = response.status();
        span_timing
            .span()
            .record(STATUS_CODE, status.as_u16() as i64);
        span_timing.span().record(SUCCESS, status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span_timing.span().record(ERROR, "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ResponsesStream,
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("responses");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span_timing.span().record(ERROR, message.as_str());
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
                error_code: parse_error_code(bytes.as_ref()),
            });
        }

        let model_metadata = extract_model_metadata(response.headers());
        let response_model = response
            .headers()
            .get("openai-model")
            .or_else(|| response.headers().get("x-openai-model"))
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let response_turn_state = response
            .headers()
            .get(X_CODEX_TURN_STATE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if let (Some(key), Some(turn_state)) = (codex_turn_key.as_ref(), response_turn_state) {
            cache_codex_turn_state(&self.codex_turn_state, key, turn_state);
        }
        let responses_wire_dialect =
            if self.defaults.configured_api_backend == ApiBackend::CodexResponses {
                ResponsesWireDialect::Codex
            } else {
                ResponsesWireDialect::Strict
            };

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
        let codex_turn_state_for_stream = self.codex_turn_state.clone();
        let mut requested_model_for_stream = response_model.unwrap_or_else(|| model_id.clone());

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

                        if responses_wire_dialect == ResponsesWireDialect::Codex
                            && let Some(response_model) = codex_event_response_model(data)
                        {
                            requested_model_for_stream = response_model;
                        }
                        if responses_wire_dialect == ResponsesWireDialect::Codex
                            && let (Some(key), Some(turn_state)) =
                                (codex_turn_key.as_ref(), codex_event_turn_state(data))
                        {
                            cache_codex_turn_state(&codex_turn_state_for_stream, key, turn_state);
                        }

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
                            decode_responses_sse_frame_for_model(
                                &event.event,
                                data,
                                responses_wire_dialect,
                                &requested_model_for_stream,
                            )
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

        Ok((
            span_timing.hold_until_first_content(events, responses_stream_item_class),
            model_metadata,
            doom_loop,
        ))
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

        // Apply temperature only when the concrete Messages route accepts it.
        if self
            .defaults
            .messages_compat()
            .is_some_and(|compat| !compat.supports_temperature)
        {
            request.inner.temperature = None;
        } else if request.inner.temperature.is_none() {
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
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("messages"));
        let mut http_request = self.apply_messages_session_affinity(
            grok_headers.apply(builder, self.allows_x_grok_headers()),
            request.x_grok_session_id.as_deref(),
        );
        if self.defaults.adapter.uses_github_copilot_dialect() {
            let request_body =
                serde_json::to_value(&request.inner).map_err(SamplingError::Serialization)?;
            http_request = apply_copilot_dynamic_headers(http_request, &request_body);
        }
        let http_request = http_request.json(&request.inner);

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
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("messages");
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
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
                error_code: parse_error_code(bytes.as_ref()),
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
    pub async fn create_message_stream(
        &self,
        request: MessagesRequestWrapper,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        let region = crate::span_timing::stream_span!(
            "http.create_message_stream",
            endpoint = %self.endpoint("messages"),
            model_id = request.inner.model.as_str(),
        );
        if region.span().is_disabled() {
            self.create_message_stream_inner(request, region).await
        } else {
            let span = region.span().clone();
            self.create_message_stream_inner(request, region)
                .instrument(span)
                .await
        }
    }

    async fn create_message_stream_inner(
        &self,
        mut request: MessagesRequestWrapper,
        region: crate::span_timing::Region,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        let mut span_timing = StreamSpanTiming::start(region);
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
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("messages"));
        let mut http_request = self
            .apply_messages_session_affinity(
                grok_headers.apply(builder, self.allows_x_grok_headers()),
                request.x_grok_session_id.as_deref(),
            )
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if self.defaults.adapter.uses_github_copilot_dialect() {
            let request_body =
                serde_json::to_value(&request.inner).map_err(SamplingError::Serialization)?;
            http_request = apply_copilot_dynamic_headers(http_request, &request_body);
        }
        let http_request = http_request.json(&request.inner);

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

        let response = self
            .execute_stream_request(built_request, &mut span_timing)
            .await?;

        let status = response.status();
        span_timing
            .span()
            .record(STATUS_CODE, status.as_u16() as i64);
        span_timing.span().record(SUCCESS, status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span_timing.span().record(ERROR, "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::MessagesStream,
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("messages");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span_timing.span().record(ERROR, message.as_str());
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
                error_code: parse_error_code(bytes.as_ref()),
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

        Ok((
            span_timing.hold_until_first_content(events, message_event_class),
            model_metadata,
        ))
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
            self.configured_api_backend(),
            &self.base_url,
        ));

        // Production always stamps route metadata from SamplerConfig so bare
        // model-name/URL matching cannot reshape third-party payloads.
        request.kimi_dialect = Some(self.defaults.adapter.uses_kimi_dialect());
        if self.defaults.adapter.uses_openai_codex_dialect() {
            request.hosted_tools.retain(|tool| {
                matches!(tool, xai_grok_sampling_types::HostedTool::WebSearch { .. })
            });
        }
        request.request_compat = self.defaults.request_compat.clone();
        if request.bedrock_request_metadata.is_empty() {
            request.bedrock_request_metadata = self.defaults.bedrock_request_metadata.clone();
        }
        if request.bedrock_headers.is_empty() {
            request.bedrock_headers = self.defaults.bedrock_headers.clone();
        }

        Ok(())
    }

    /// Send a conversation request using Google GenerateContent (streaming).
    pub async fn conversation_stream_google(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<crate::google::GenerateContentResponse>>,
        ReasoningModelIdentity,
    )> {
        self.apply_conversation_defaults(&mut request)?;
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.defaults.model.clone());
        let endpoint = crate::google::GoogleEndpoint::from_config(&self.base_url, &model);
        let body =
            crate::google::build_request(&request, &model, self.defaults.request_compat.as_ref());
        let mut headers = self.default_headers.clone();
        remove_known_auth_headers(&mut headers);
        // Google endpoints are never first-party xAI; strip any product headers
        // that may have arrived via shared default_headers.
        strip_x_grok_headers(&mut headers);
        let api_key = self
            .default_headers
            .get(HeaderName::from_static("x-goog-api-key"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let vertex_express =
            api_key.is_some() && matches!(endpoint.kind, crate::google::GoogleEndpointKind::Vertex);
        let url = endpoint.url(true, vertex_express)?;
        let bearer = if api_key.is_none()
            && matches!(endpoint.kind, crate::google::GoogleEndpointKind::Vertex)
        {
            Some(self.google_adc.token().await?)
        } else {
            None
        };
        crate::google::apply_google_auth_headers(
            &mut headers,
            api_key.as_deref(),
            bearer.as_deref(),
        );
        let response = self
            .http
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await.unwrap_or_default();
            return Err(SamplingError::Api {
                status,
                message: user_facing_api_error_message(status, bytes.as_ref()),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
                error_code: parse_error_code(bytes.as_ref()),
            });
        }
        let identity =
            ReasoningModelIdentity::new(model, ApiBackend::GoogleGenerateContent, &self.base_url);
        Ok((crate::google::sse_stream(response), identity))
    }

    /// Send a conversation request using Amazon Bedrock ConverseStream (streaming).
    pub async fn conversation_stream_bedrock(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<aws_sdk_bedrockruntime::types::ConverseStreamOutput>>,
        ReasoningModelIdentity,
    )> {
        self.apply_conversation_defaults(&mut request)?;
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.defaults.model.clone());
        let bearer = self
            .default_headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ").or(Some(v)))
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        let cfg = crate::bedrock::resolve_endpoint_config(
            &model,
            &self.base_url,
            bearer.as_deref(),
            self.defaults.bedrock_profile.as_deref(),
            |name| std::env::var(name).ok(),
        );
        let client = crate::bedrock::client_from_config(&cfg).await;
        let identity = ReasoningModelIdentity::new(
            model.clone(),
            ApiBackend::BedrockConverseStream,
            cfg.endpoint_url.as_deref().unwrap_or(&self.base_url),
        );
        let stream = crate::bedrock::converse_stream(client, model, request).await?;
        Ok((stream, identity))
    }

    /// Send a conversation request using Pi Messages (streaming).
    pub async fn conversation_stream_pi_messages(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<PiMessagesEvent>>,
        ReasoningModelIdentity,
    )> {
        self.apply_conversation_defaults(&mut request)?;
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.defaults.model.clone());
        let body = crate::pi_messages::build_request(&request, &model)?;
        tracing::debug!(base_url = %self.base_url, model_id = %model, "Sending Pi Messages stream request");
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(self.endpoint("messages"));
        let http_request = builder
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .json(&body);
        let response = http_request.send().await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            e
        })?;
        let status = response.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::MessagesStream,
                    sent_bearer.as_deref(),
                );
                let endpoint = self.endpoint("messages");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {server_message}"),
                    sent_bearer.as_deref(),
                ));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            tracing::error!(status = %status, error_message = %message, body_preview = %Self::body_preview(bytes.as_ref()), model_id = %model, "pi-messages API error");
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
                error_code: parse_error_code(bytes.as_ref()),
            });
        }
        let events = response
            .bytes_stream()
            .eventsource()
            .filter_map(|event_res| async move {
                match event_res {
                    Ok(event) => match crate::pi_messages::decode_event_data(&event.data) {
                        Ok(Some(event)) => Some(Ok(event)),
                        Ok(None) => None,
                        Err(e) => Some(Err(e)),
                    },
                    Err(e) => Some(Err(SamplingError::EventStreamError(e.to_string()))),
                }
            })
            .boxed();
        let identity = ReasoningModelIdentity::new(model, ApiBackend::PiMessages, &self.base_url);
        Ok((events, identity))
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

        // The hosted tools travel as raw JSON, spliced in after serialization by
        // `splice_extra_tool_entries`, whose doc explains why each one does.
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

    /// Compact Codex Responses history through the provider's unary
    /// `/responses/compact` endpoint.
    ///
    /// `Ok(None)` means the provider explicitly does not implement the
    /// endpoint and the caller may fall back to local summary compaction.
    /// Authentication, quota, rate-limit, transport, and server failures stay
    /// as errors so fallback cannot hide a real provider problem or duplicate
    /// a paid request.
    pub async fn compact_conversation(
        &self,
        mut request: ConversationRequest,
        instructions: String,
    ) -> Result<Option<Vec<xai_grok_sampling_types::ConversationItem>>> {
        if self.defaults.configured_api_backend != ApiBackend::CodexResponses {
            return Err(SamplingError::InvalidConfiguration(
                "remote compact requires api_backend = codex-responses",
            ));
        }
        self.apply_conversation_defaults(&mut request)?;
        let route_identity = request.reasoning_model_identity.clone();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();
        let x_grok_deployment_id = request.x_grok_deployment_id.clone();
        let x_grok_user_id = request.x_grok_user_id.clone();

        let mut wrapper = CreateResponseWrapper::new((&request).into());
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;
        wrapper.x_grok_deployment_id = x_grok_deployment_id;
        wrapper.x_grok_user_id = x_grok_user_id;
        self.apply_response_defaults(&mut wrapper)?;

        let mut normal_body =
            serde_json::to_value(&wrapper.inner).map_err(SamplingError::Serialization)?;
        xai_grok_sampling_types::patch_reasoning_text_types(&mut normal_body);
        patch_codex_reasoning_effort_wire(&mut normal_body, self.defaults.reasoning_effort);
        if self
            .defaults
            .responses_compat()
            .is_some_and(|compat| !compat.supports_strict_mode)
        {
            Self::strip_strict_tool_fields(&mut normal_body);
        }

        let mut compact_body = serde_json::Map::new();
        let model = normal_body
            .get("model")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::String(self.defaults.model.clone()));
        compact_body.insert("model".into(), model);
        compact_body.insert(
            "input".into(),
            normal_body
                .get("input")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
        );
        let resolved_instructions = if instructions.trim().is_empty() {
            normal_body
                .get("instructions")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("You are a helpful assistant.")
                .to_string()
        } else {
            instructions
        };
        compact_body.insert(
            "instructions".into(),
            serde_json::Value::String(resolved_instructions),
        );
        compact_body.insert(
            "tools".into(),
            normal_body
                .get("tools")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
        );
        compact_body.insert("parallel_tool_calls".into(), serde_json::Value::Bool(true));
        for field in ["reasoning", "service_tier", "prompt_cache_key", "text"] {
            if let Some(value) = normal_body.get(field).filter(|value| !value.is_null()) {
                compact_body.insert(field.to_string(), value.clone());
            }
        }
        let compact_body = serde_json::Value::Object(compact_body);

        let endpoint = self.endpoint.url_for_path("responses/compact");
        let route = endpoint
            .split(['?', '#'])
            .next()
            .unwrap_or(endpoint.as_str())
            .trim_end_matches('/')
            .to_string();
        if self
            .codex_compact_unsupported
            .lock()
            .is_ok_and(|cache| cache.contains(&route))
        {
            return Ok(None);
        }
        let model_id = compact_body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let grok_headers = GrokRequestHeaders {
            conv_id: wrapper.x_grok_conv_id.as_deref().unwrap_or_default(),
            req_id: wrapper.x_grok_req_id.as_deref().unwrap_or_default(),
            model_id,
            session_id: wrapper.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: wrapper.x_grok_turn_idx.as_deref(),
            agent_id: wrapper.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: wrapper.x_grok_deployment_id.as_deref(),
            user_id: wrapper.x_grok_user_id.as_deref(),
        };
        let codex_turn_key = wrapper
            .x_grok_session_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .zip(
                wrapper
                    .x_grok_turn_idx
                    .as_deref()
                    .filter(|value| !value.is_empty()),
            )
            .map(|(session_id, turn_id)| {
                // Share state with the ordinary Responses endpoint: compact is
                // a continuation inside the same Codex turn.
                let responses_route = self
                    .endpoint
                    .url_for_path("responses")
                    .split(['?', '#'])
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .to_string();
                (responses_route, session_id.to_string(), turn_id.to_string())
            });
        let SentRequest {
            builder,
            sent_bearer,
        } = self.post(endpoint.clone());
        let mut http_request = self
            .apply_responses_session_affinity(
                grok_headers.apply(builder, self.allows_x_grok_headers()),
                wrapper.x_grok_session_id.as_deref(),
            )
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .timeout(self.defaults.compact_timeout);
        if let Some(turn_state) = codex_turn_key
            .as_ref()
            .and_then(|key| take_codex_turn_state(&self.codex_turn_state, key))
        {
            http_request = http_request.header(X_CODEX_TURN_STATE_HEADER, turn_state);
        }
        let built_request = http_request
            .json(&compact_body)
            .build()
            .map_err(SamplingError::Http)?;
        Self::log_request_headers(&built_request, "responses/compact");
        let response = self.http.execute(built_request).await?;
        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let response_turn_state = response
            .headers()
            .get(X_CODEX_TURN_STATE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await?;
        if !status.is_success() {
            let message = user_facing_api_error_message(status, bytes.as_ref());
            if compact_endpoint_is_unsupported(status, &message) {
                if let Ok(mut cache) = self.codex_compact_unsupported.lock() {
                    cache.insert(route);
                }
                tracing::info!(status = %status, "Codex remote compact unsupported; using local fallback");
                return Ok(None);
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::Responses,
                    sent_bearer.as_deref(),
                );
                return Err(auth_rejected(
                    format!("Unauthorized (401) from {endpoint}: {message}"),
                    sent_bearer.as_deref(),
                ));
            }
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
                error_code: parse_error_code(bytes.as_ref()),
            });
        }
        if let (Some(key), Some(turn_state)) = (codex_turn_key.as_ref(), response_turn_state) {
            cache_codex_turn_state(&self.codex_turn_state, key, turn_state);
        }
        let response: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(SamplingError::Serialization)?;
        let output = response.get("output").cloned().ok_or_else(|| {
            SamplingError::Serialization(<serde_json::Error as serde::de::Error>::custom(
                "Codex compact response is missing output",
            ))
        })?;
        let items = xai_grok_sampling_types::codex_compact_output_to_conversation_items(
            output,
            route_identity,
        );
        if !items.iter().any(|item| {
            matches!(
                item,
                xai_grok_sampling_types::ConversationItem::Compaction(_)
            )
        }) {
            return Err(SamplingError::Serialization(
                <serde_json::Error as serde::de::Error>::custom(
                    "Codex compact response contains no compaction item",
                ),
            ));
        }
        Ok(Some(items))
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

        // The hosted tools travel as raw JSON, spliced in by `create_response` through
        // `splice_extra_tool_entries`, whose doc explains why each one does.
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
    ///
    /// Honors the request's [`LengthPolicy`](xai_grok_sampling_types::LengthPolicy)
    /// like the actor path: under the default `Fail`, a `Length` stop is an
    /// error, so side callers (autocomplete, memory notes, summaries) never
    /// persist a silently truncated result. Under `CompletePartial` the
    /// same salvage gate as `drive_l2` applies: empty Length and Length
    /// carrying tool calls still fail.
    pub async fn conversation_collect(
        &self,
        mut request: ConversationRequest,
    ) -> Result<ConversationResponse> {
        self.apply_conversation_defaults(&mut request)?;
        let reasoning_model_identity = request.reasoning_model_identity.clone();
        self.conversation_collect_with_idle_timeout(request, std::time::Duration::from_secs(300))
            .await
            .map(|mut response| {
                for item in &mut response.items {
                    if let xai_grok_sampling_types::ConversationItem::Assistant(assistant) = item {
                        assistant.reasoning_model_identity = reasoning_model_identity.clone();
                    }
                }
                response
            })
    }

    /// [`Self::conversation_collect`] with a caller-chosen idle timeout, for
    /// short side calls (autocomplete, memory notes) that must give up fast.
    pub async fn conversation_collect_with_idle_timeout(
        &self,
        request: ConversationRequest,
        idle_timeout: std::time::Duration,
    ) -> Result<ConversationResponse> {
        let request_id = crate::types::RequestId::random();
        let length_policy = request.length_policy;
        let result = match self.api_backend() {
            ApiBackend::ChatCompletions => {
                let (raw, meta) = self.conversation_stream(request).await?;
                let events =
                    crate::stream::stream_chat_completions(raw, meta, request_id, idle_timeout);
                crate::stream::collect_response(events).await
            }
            ApiBackend::Responses | ApiBackend::CodexResponses => {
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
            ApiBackend::GoogleGenerateContent => {
                let (raw, identity) = self.conversation_stream_google(request).await?;
                let events = crate::google::stream_google_generate_content(
                    raw,
                    request_id,
                    identity,
                    idle_timeout,
                );
                crate::stream::collect_response(events).await
            }
            ApiBackend::BedrockConverseStream => {
                let (raw, identity) = self.conversation_stream_bedrock(request).await?;
                let events = crate::bedrock::stream_bedrock_converse(
                    raw,
                    request_id,
                    identity,
                    idle_timeout,
                );
                crate::stream::collect_response(events).await
            }
            ApiBackend::PiMessages => {
                let (raw, identity) = self.conversation_stream_pi_messages(request).await?;
                let events =
                    crate::pi_messages::stream_pi_messages(raw, request_id, identity, idle_timeout);
                crate::stream::collect_response(events).await
            }
        };
        let response = result
            .map(|(response, _metrics)| response)
            .map_err(stream_collect_error)?;
        apply_length_policy(length_policy, response)
    }
}

/// Applies the request's [`xai_grok_sampling_types::LengthPolicy`] to a
/// collected response: fails a `Length` stop the policy rejects, logs the
/// salvage breadcrumb otherwise. The single gate shared by `drive_l2` and
/// the direct-collect path so the two cannot drift.
pub(crate) fn apply_length_policy(
    policy: xai_grok_sampling_types::LengthPolicy,
    response: xai_grok_sampling_types::ConversationResponse,
) -> Result<xai_grok_sampling_types::ConversationResponse> {
    use xai_grok_sampling_types::LengthVerdict;
    match policy.verdict(&response) {
        LengthVerdict::Pass => Ok(response),
        LengthVerdict::Fail => Err(SamplingError::MaxTokensTruncation),
        LengthVerdict::Salvage => {
            // Breadcrumb for "why did the user get half an answer".
            tracing::info!(
                content_len = response.assistant().map_or(0, |a| a.content.len()),
                completion_tokens = response.usage.as_ref().map(|u| u.completion_tokens),
                "salvaging Length-truncated response per LengthPolicy::CompletePartial"
            );
            Ok(response)
        }
        LengthVerdict::SalvageToolCalls => {
            // Breadcrumb for counting turns rescued from max_tokens_truncation.
            tracing::info!(
                tool_calls = response.tool_calls().len(),
                content_len = response.assistant().map_or(0, |a| a.content.len()),
                completion_tokens = response.usage.as_ref().map(|u| u.completion_tokens),
                "completing Length-truncated response with completed tool calls"
            );
            Ok(response)
        }
    }
}

/// Rebuild `Api` from stream-collected info, preserving status,
/// `Retry-After`, and `x-should-retry` (kind is lost on this path).
fn stream_collect_error(info: SamplingErrorInfo) -> SamplingError {
    SamplingError::Api {
        status: info
            .status_code
            .and_then(|c| reqwest::StatusCode::from_u16(c).ok())
            .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
        message: info.message,
        model_metadata: info.model_metadata,
        retry_after_secs: info.retry_after_secs,
        should_retry: info.should_retry,
        error_code: info.error_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Bytes, routing::post};
    use indexmap::IndexMap;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use xai_grok_sampling_types::ApiErrorCode;
    use xai_grok_sampling_types::types::ChatRequestMessage;

    #[test]
    fn splice_extra_tool_entries_extends_existing_tools_array() {
        let mut body = serde_json::json!({ "tools": [{ "type": "function" }] });
        splice_extra_tool_entries(&mut body, vec![serde_json::json!({ "type": "web_search" })]);
        assert_eq!(
            body["tools"],
            serde_json::json!([{ "type": "function" }, { "type": "web_search" }])
        );
    }

    #[test]
    fn splice_extra_tool_entries_creates_tools_array_when_absent() {
        let mut body = serde_json::json!({});
        splice_extra_tool_entries(&mut body, vec![serde_json::json!({ "type": "web_search" })]);
        assert_eq!(body["tools"], serde_json::json!([{ "type": "web_search" }]));
    }

    #[test]
    fn splice_extra_tool_entries_noop_when_empty() {
        let mut body = serde_json::json!({ "tools": [{ "type": "function" }] });
        splice_extra_tool_entries(&mut body, vec![]);
        assert_eq!(body["tools"], serde_json::json!([{ "type": "function" }]));
    }

    #[test]
    fn stream_collect_error_preserves_should_retry() {
        let info = SamplingErrorInfo {
            kind: crate::events::SamplingErrorKind::Api,
            status_code: Some(529),
            message: "Overloaded".into(),
            is_retryable: true,
            retry_after_secs: Some(3),
            should_retry: Some(false),
            error_code: Some(ApiErrorCode::InvalidImage),
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: xai_grok_sampling_types::SentCredential::Unknown,
        };
        // SamplingError is not PartialEq (it carries reqwest/serde errors),
        // so destructure once and compare all fields in a single assert.
        let SamplingError::Api {
            status,
            message,
            model_metadata,
            retry_after_secs,
            should_retry,
            error_code,
        } = stream_collect_error(info)
        else {
            panic!("expected Api");
        };
        assert_eq!(
            (
                status.as_u16(),
                message.as_str(),
                model_metadata.is_none(),
                retry_after_secs,
                should_retry,
                error_code,
            ),
            (
                529,
                "Overloaded",
                true,
                Some(3),
                Some(false),
                Some(ApiErrorCode::InvalidImage)
            ),
        );
    }

    fn minimal_config() -> SamplerConfig {
        SamplerConfig {
            api_key: Some("test-key".to_string()),
            base_url: "https://example.test".to_string(),
            model: "test-model".to_string(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::ChatCompletions,
            adapter_kind: xai_grok_sampling_types::AdapterKind::Standard,
            request_compat: None,
            endpoint_path: None,
            auth_scheme: AuthScheme::Bearer,
            extra_headers: IndexMap::new(),
            extra_response_includes: Vec::new(),
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
            bedrock_request_metadata: IndexMap::new(),
            bedrock_headers: IndexMap::new(),
            bedrock_profile: None,
            kimi_dialect: false,
        }
    }

    fn mistral_client(model: &str) -> SamplingClient {
        SamplingClient::new(SamplerConfig {
            model: model.to_string(),
            adapter_kind: xai_grok_sampling_types::AdapterKind::MistralConversations,
            endpoint_path: Some("chat/completions".into()),
            reasoning_effort: Some(xai_grok_sampling_types::ReasoningEffort::High),
            ..minimal_config()
        })
        .expect("mistral client")
    }

    #[test]
    fn mistral_request_uses_chat_endpoint_and_affinity_header() {
        let client = mistral_client("magistral-small");
        assert_eq!(client.api_backend(), ApiBackend::ChatCompletions);
        assert_eq!(
            client.endpoint("responses"),
            "https://example.test/chat/completions"
        );

        let builder = client.http.post("https://example.test/chat/completions");
        let request = client
            .apply_chat_session_affinity(builder, Some("session-123"))
            .build()
            .expect("request builds");
        assert_eq!(
            request
                .headers()
                .get("x-affinity")
                .and_then(|v| v.to_str().ok()),
            Some("session-123")
        );
    }

    #[test]
    fn mistral_prompt_mode_reasoning_and_tool_ids_are_patched() {
        let client = mistral_client("magistral-small");
        let mut body = serde_json::json!({
            "model": "magistral-small",
            "messages": [
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1234567890abcdef",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{}"}
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1234567890abcdef",
                    "content": "ok"
                }
            ],
            "reasoning_effort": "high"
        });

        client.patch_chat_request_body(&mut body, true);

        assert_eq!(body["prompt_mode"], "reasoning");
        assert!(body.get("reasoning_effort").is_none());
        let assistant_id = body["messages"][0]["tool_calls"][0]["id"].as_str().unwrap();
        let tool_result_id = body["messages"][1]["tool_call_id"].as_str().unwrap();
        assert_eq!(assistant_id, tool_result_id);
        assert_eq!(assistant_id.len(), MISTRAL_TOOL_CALL_ID_LENGTH);
        assert!(assistant_id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn mistral_reasoning_effort_models_keep_reasoning_effort() {
        let client = mistral_client("mistral-small-2603");
        let mut body = serde_json::json!({
            "model": "mistral-small-2603",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high"
        });

        client.patch_chat_request_body(&mut body, true);

        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("prompt_mode").is_none());
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

    #[test]
    fn legacy_builtin_codex_keeps_responses_identity() {
        let client = SamplingClient::new(SamplerConfig {
            api_backend: ApiBackend::Responses,
            adapter_kind: xai_grok_sampling_types::AdapterKind::OpenAiCodex,
            ..minimal_config()
        })
        .unwrap();
        assert_eq!(client.api_backend(), ApiBackend::Responses);
        assert_eq!(client.configured_api_backend(), ApiBackend::Responses);

        let mut request = ConversationRequest {
            model: Some("builtin-codex".into()),
            ..Default::default()
        };
        client.apply_conversation_defaults(&mut request).unwrap();
        assert_eq!(
            request.reasoning_model_identity,
            Some(ReasoningModelIdentity::new(
                "builtin-codex",
                ApiBackend::Responses,
                "https://example.test",
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
            x_grok_transient_retry: None,
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
    fn codex_responses_keeps_native_web_search_and_sets_tool_defaults() {
        use xai_grok_sampling_types::{ConversationRequest, HostedTool, ToolSpec};

        let client = SamplingClient::new(SamplerConfig {
            api_backend: ApiBackend::CodexResponses,
            ..minimal_config()
        })
        .unwrap();
        let mut request = ConversationRequest::from_items(vec![
            xai_grok_sampling_types::ConversationItem::system("you are codex"),
            xai_grok_sampling_types::ConversationItem::user("search the web"),
        ])
        .with_tools(vec![ToolSpec {
            name: "read_file".into(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
        }]);
        request.hosted_tools = vec![
            HostedTool::WebSearch { options: None },
            HostedTool::XSearch { options: None },
        ];

        client.apply_conversation_defaults(&mut request).unwrap();
        assert_eq!(
            request.hosted_tools,
            vec![HostedTool::WebSearch { options: None }]
        );
        let extra_tools = xai_grok_sampling_types::extra_tool_entries(&request.hosted_tools);
        let mut wrapper = CreateResponseWrapper::new((&request).into());
        wrapper.extra_tool_entries = extra_tools;
        client.apply_response_defaults(&mut wrapper).unwrap();
        let body = serde_json::to_value(&wrapper.inner).unwrap();

        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
        let tools = body["tools"].as_array().expect("Codex tools array");
        assert!(tools.iter().any(|tool| tool["type"] == "web_search"));
        assert!(tools.iter().any(|tool| tool["type"] == "function"));
        assert!(tools.iter().all(|tool| tool["type"] != "x_search"));
        assert!(wrapper.extra_tool_entries.is_empty());
    }

    #[test]
    fn codex_request_without_tools_keeps_fixed_defaults_and_client_metadata() {
        let client = SamplingClient::new(SamplerConfig {
            api_backend: ApiBackend::CodexResponses,
            ..minimal_config()
        })
        .unwrap();
        let request =
            ConversationRequest::from_items(vec![xai_grok_sampling_types::ConversationItem::user(
                "hello",
            )]);
        let mut wrapper = CreateResponseWrapper::new((&request).into());
        wrapper.x_grok_session_id = Some("session-123".into());
        wrapper.x_grok_conv_id = Some("thread-456".into());
        wrapper.x_grok_turn_idx = Some("turn-7".into());
        client.apply_response_defaults(&mut wrapper).unwrap();

        let mut body = serde_json::to_value(&wrapper.inner).unwrap();
        patch_codex_request_body(&mut body, &wrapper, client.defaults.reasoning_effort);

        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["tools"], serde_json::json!([]));
        assert_eq!(body["client_metadata"]["session_id"], "session-123");
        assert_eq!(body["client_metadata"]["thread_id"], "thread-456");
        assert_eq!(body["client_metadata"]["turn_id"], "turn-7");
    }

    #[test]
    fn codex_azure_store_matches_official_cli_while_relays_remain_stateless() {
        for base_url in [
            "https://foo.openai.azure.com/openai",
            "https://foo.cognitiveservices.azure.cn/openai",
            "https://foo.aoai.azure.com/openai",
            "https://foo.openai.azure-api.net/openai",
            "https://foo.z01.azurefd.net/openai",
            "https://foo.windows.net/openai",
        ] {
            let client = SamplingClient::new(SamplerConfig {
                base_url: base_url.into(),
                api_backend: ApiBackend::CodexResponses,
                ..minimal_config()
            })
            .unwrap();
            let mut wrapper = CreateResponseWrapper::default();
            client.apply_response_defaults(&mut wrapper).unwrap();
            assert_eq!(wrapper.inner.store, Some(true), "{base_url}");
        }

        let relay = SamplingClient::new(SamplerConfig {
            base_url: "https://relay.example/v1".into(),
            api_backend: ApiBackend::CodexResponses,
            ..minimal_config()
        })
        .unwrap();
        let mut wrapper = CreateResponseWrapper::default();
        relay.apply_response_defaults(&mut wrapper).unwrap();
        assert_eq!(wrapper.inner.store, Some(false));
    }

    #[test]
    fn strict_responses_does_not_inject_codex_client_metadata() {
        let client = SamplingClient::new(SamplerConfig {
            api_backend: ApiBackend::Responses,
            ..minimal_config()
        })
        .unwrap();
        let request =
            ConversationRequest::from_items(vec![xai_grok_sampling_types::ConversationItem::user(
                "hello",
            )]);
        let mut wrapper = CreateResponseWrapper::new((&request).into());
        wrapper.x_grok_session_id = Some("session-123".into());
        wrapper.x_grok_turn_idx = Some("turn-7".into());
        client.apply_response_defaults(&mut wrapper).unwrap();

        let body = serde_json::to_value(&wrapper.inner).unwrap();
        assert!(body.get("client_metadata").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[tokio::test]
    async fn codex_responses_non_streaming_fails_before_http() {
        let client = SamplingClient::new(SamplerConfig {
            api_backend: ApiBackend::CodexResponses,
            ..minimal_config()
        })
        .unwrap();
        let error = client
            .create_response(CreateResponseWrapper::default())
            .await
            .expect_err("Codex non-streaming must fail fast");
        assert!(matches!(error, SamplingError::InvalidConfiguration(_)));
        assert!(error.to_string().contains("requires the streaming"));
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

        let mut req =
            ConversationRequest::from_items(vec![xai_grok_sampling_types::ConversationItem::user(
                "hi",
            )]);
        req.prompt_cache_key = Some("explicit-key".into());
        req.prompt_cache_retention = Some("24h".into());
        let body: rs::CreateResponse = (&req).into();
        assert_eq!(body.prompt_cache_key.as_deref(), Some("explicit-key"));
        assert_eq!(
            body.prompt_cache_retention,
            Some(rs::PromptCacheRetention::Hours24)
        );
    }

    async fn capture_response_body(streaming: bool) -> serde_json::Value {
        let (body_tx, body_rx) = oneshot::channel();
        let body_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(body_tx)));
        let app = Router::new().route(
            "/v1/responses",
            post(move |body: Bytes| {
                let body_tx = body_tx.clone();
                async move {
                    let _ = body_tx.lock().unwrap().take().unwrap().send(body);
                    if streaming {
                        axum::response::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from("data: [DONE]\n\n"))
                            .unwrap()
                    } else {
                        axum::response::Response::builder()
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(r#"{"id":"resp","object":"response","created_at":0,"model":"test-model","status":"completed","output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0}}"#))
                            .unwrap()
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = SamplingClient::new(SamplerConfig {
            base_url: format!("http://{addr}/v1"),
            api_backend: ApiBackend::Responses,
            extra_response_includes: vec!["no_inline_citations".to_owned()],
            ..minimal_config()
        })
        .unwrap();
        let mut request = rs::CreateResponse {
            input: rs::InputParam::Text("hi".to_owned()),
            include: Some(vec![rs::IncludeEnum::ReasoningEncryptedContent]),
            tools: Some(vec![rs::Tool::WebSearch(rs::WebSearchTool::default())]),
            ..Default::default()
        };
        let mut wrapper = CreateResponseWrapper::new(request.clone());
        wrapper.extra_tool_entries = vec![serde_json::json!({"type": "x_search"})];
        if streaming {
            let (_stream, _model_metadata, _doom_loop_collector) = client
                .create_response_stream(wrapper)
                .await
                .expect("streaming request should succeed");
        } else {
            request.tools = None;
            client
                .create_response(CreateResponseWrapper::new(request))
                .await
                .expect("unary request should succeed");
        }
        let body = body_rx.await.unwrap();
        server.abort();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn response_call_sites_emit_final_includes_and_stream_fields() {
        let unary = capture_response_body(false).await;
        assert_eq!(
            serde_json::json!(["reasoning.encrypted_content", "no_inline_citations"]),
            unary["include"],
        );

        let stream = capture_response_body(true).await;
        assert_eq!(
            serde_json::json!(["reasoning.encrypted_content", "no_inline_citations"]),
            stream["include"],
        );
        assert_eq!(Some(true), stream["stream"].as_bool());
        assert!(
            stream["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["type"] == "x_search")
        );
    }

    #[test]
    fn append_response_includes_preserves_typed_values_and_deduplicates() {
        let typed = [
            "reasoning.encrypted_content",
            "web_search_call.action.sources",
        ];
        let mut body = serde_json::json!({ "include": typed });
        append_response_includes(
            &mut body,
            &[
                "no_inline_citations".to_owned(),
                "no_inline_citations".to_owned(),
            ],
        );
        assert_eq!(
            serde_json::json!([
                "reasoning.encrypted_content",
                "web_search_call.action.sources",
                "no_inline_citations",
            ]),
            body["include"],
        );

        let mut unchanged = serde_json::json!({ "include": typed });
        let expected = unchanged.clone();
        append_response_includes(&mut unchanged, &[]);
        assert_eq!(expected, unchanged);

        for mut body in [
            serde_json::json!({}),
            serde_json::json!({ "include": null }),
        ] {
            append_response_includes(&mut body, &["no_inline_citations".to_owned()]);
            assert_eq!(serde_json::json!(["no_inline_citations"]), body["include"]);
        }
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
    fn catalog_user_agent_is_not_overwritten_by_generic_user_agent() {
        let mut cfg = minimal_config();
        cfg.extra_headers.insert(
            "User-Agent".to_string(),
            "GitHubCopilotChat/0.35.0".to_string(),
        );
        let client = SamplingClient::new(cfg).expect("client with catalog UA");
        assert_eq!(
            client
                .default_headers
                .get(USER_AGENT)
                .and_then(|v| v.to_str().ok()),
            Some("GitHubCopilotChat/0.35.0")
        );
    }

    #[test]
    fn github_copilot_dynamic_headers_follow_pi_rules() {
        let assistant = serde_json::json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"}
            ]
        });
        assert_eq!(infer_copilot_initiator(&assistant), "agent");
        assert!(!value_has_copilot_image(&assistant));

        let chat_tool_result = serde_json::json!({
            "messages": [
                {"role": "assistant", "tool_calls": [{"id": "call_1"}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
            ]
        });
        assert_eq!(infer_copilot_initiator(&chat_tool_result), "agent");

        let responses_tool_result = serde_json::json!({
            "input": [
                {"type": "message", "role": "assistant", "content": []},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ]
        });
        assert_eq!(infer_copilot_initiator(&responses_tool_result), "agent");

        let messages_tool_result = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "ok"}]
            }]
        });
        assert_eq!(infer_copilot_initiator(&messages_tool_result), "agent");

        let user_image = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}]
            }]
        });
        assert_eq!(infer_copilot_initiator(&user_image), "user");
        assert!(value_has_copilot_image(&user_image));
    }

    #[test]
    fn github_copilot_adapter_preserves_wire_backend() {
        let client = SamplingClient::new(SamplerConfig {
            adapter_kind: xai_grok_sampling_types::AdapterKind::GitHubCopilot,
            ..minimal_config()
        })
        .expect("github copilot client");
        assert_eq!(client.api_backend(), ApiBackend::ChatCompletions);
        assert!(client.backend_adapter().uses_github_copilot_dialect());
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
    fn chat_compat_rewrites_max_tokens_field() {
        let mut cfg = minimal_config();
        cfg.request_compat = Some(RequestCompat::ChatCompletions(
            xai_grok_sampling_types::OpenAiCompletionsCompat {
                supports_store: false,
                max_tokens_field: MaxTokensField::MaxCompletionTokens,
                ..Default::default()
            },
        ));
        let client = SamplingClient::new(cfg).expect("client should build");
        let mut body = serde_json::json!({ "max_tokens": 321 });

        client.patch_chat_request_body(&mut body, false);

        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["max_completion_tokens"], 321);
    }

    #[test]
    fn chat_compat_removes_unsupported_stream_usage() {
        let mut cfg = minimal_config();
        cfg.request_compat = Some(RequestCompat::ChatCompletions(
            xai_grok_sampling_types::OpenAiCompletionsCompat {
                supports_store: false,
                supports_usage_in_streaming: false,
                ..Default::default()
            },
        ));
        let client = SamplingClient::new(cfg).expect("client should build");
        let mut body = serde_json::json!({
            "stream": true,
            "stream_options": { "include_usage": true }
        });

        client.patch_chat_request_body(&mut body, true);

        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn chat_compat_rewrites_deepseek_and_openrouter_thinking() {
        for (format, expected_field) in [
            (ThinkingFormat::DeepSeek, "thinking"),
            (ThinkingFormat::OpenRouter, "reasoning"),
        ] {
            let mut cfg = minimal_config();
            cfg.request_compat = Some(RequestCompat::ChatCompletions(
                xai_grok_sampling_types::OpenAiCompletionsCompat {
                    supports_store: false,
                    thinking_format: format,
                    ..Default::default()
                },
            ));
            let client = SamplingClient::new(cfg).expect("client should build");
            let mut body = serde_json::json!({ "reasoning_effort": "high" });

            client.patch_chat_request_body(&mut body, false);

            assert!(body.get(expected_field).is_some(), "body: {body}");
            if format == ThinkingFormat::OpenRouter {
                assert!(body.get("reasoning_effort").is_none());
                assert_eq!(body["reasoning"]["effort"], "high");
            } else {
                assert_eq!(body["thinking"]["type"], "enabled");
                assert_eq!(body["reasoning_effort"], "high");
            }
        }
    }

    #[test]
    fn explicit_endpoint_path_overrides_backend_default() {
        let mut cfg = minimal_config();
        cfg.base_url = "https://azure.example/openai".into();
        cfg.endpoint_path = Some("deployments/demo/responses".into());
        cfg.query_params
            .insert("api-version".into(), "2025-04-01-preview".into());
        let client = SamplingClient::new(cfg).expect("client should build");

        assert_eq!(
            client.endpoint("chat/completions"),
            "https://azure.example/openai/deployments/demo/responses?api-version=2025-04-01-preview"
        );
    }

    #[test]
    fn messages_compat_suppresses_unsupported_temperature() {
        let mut cfg = minimal_config();
        cfg.api_backend = ApiBackend::Messages;
        cfg.temperature = Some(0.7);
        cfg.request_compat = Some(RequestCompat::Messages(
            xai_grok_sampling_types::AnthropicMessagesCompat {
                supports_temperature: false,
                ..Default::default()
            },
        ));
        let client = SamplingClient::new(cfg).expect("client should build");
        let mut request = MessagesRequestWrapper::new(messages::MessagesRequest {
            temperature: Some(0.3),
            ..Default::default()
        });

        client
            .apply_message_defaults(&mut request)
            .expect("defaults should apply");

        assert!(request.inner.temperature.is_none());
    }

    #[test]
    fn chat_session_affinity_respects_no_session_format() {
        let mut cfg = minimal_config();
        cfg.request_compat = Some(RequestCompat::ChatCompletions(
            xai_grok_sampling_types::OpenAiCompletionsCompat {
                send_session_affinity_headers: true,
                session_affinity_format: SessionAffinityFormat::OpenAiNoSession,
                ..Default::default()
            },
        ));
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest { builder, .. } = client.post("https://example.test/v1/chat/completions");
        let request = client
            .apply_chat_session_affinity(builder, Some("session-123"))
            .build()
            .expect("request should build");

        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "session-123"
        );
        assert_eq!(
            request.headers().get("x-session-affinity").unwrap(),
            "session-123"
        );
        assert!(request.headers().get("session_id").is_none());
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

    #[test]
    fn azure_api_key_uses_raw_api_key_header_exclusively() {
        let mut cfg = minimal_config();
        cfg.api_key = Some("azure-test-key".into());
        cfg.auth_scheme = AuthScheme::ApiKey;
        cfg.extra_headers
            .insert("Authorization".into(), "Bearer must-not-survive".into());
        cfg.extra_headers
            .insert("x-api-key".into(), "must-not-survive".into());
        let client = SamplingClient::new(cfg).expect("client should build");
        assert_eq!(
            client
                .default_headers
                .get(HeaderName::from_static("api-key"))
                .and_then(|value| value.to_str().ok()),
            Some("azure-test-key")
        );
        assert!(client.default_headers.get(AUTHORIZATION).is_none());
        assert!(client.default_headers.get("x-api-key").is_none());
        assert!(client.default_headers.get("cf-aig-authorization").is_none());
    }

    #[test]
    fn cloudflare_gateway_auth_survives_resolver_and_rejects_late_conflicts() {
        #[derive(Debug)]
        struct Resolver;
        impl crate::config::BearerResolver for Resolver {
            fn current_bearer(&self) -> Option<String> {
                Some("fresh-cloudflare-key".into())
            }
        }
        #[derive(Debug)]
        struct ConflictingInjector;
        impl crate::config::HeaderInjector for ConflictingInjector {
            fn inject(&self, headers: &mut HeaderMap) {
                headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer wrong"));
                headers.insert(
                    HeaderName::from_static("x-api-key"),
                    HeaderValue::from_static("wrong"),
                );
                headers.insert(
                    HeaderName::from_static("api-key"),
                    HeaderValue::from_static("wrong"),
                );
                headers.insert(
                    HeaderName::from_static("cf-aig-authorization"),
                    HeaderValue::from_static("Bearer overwritten"),
                );
            }
        }

        let mut cfg = minimal_config();
        cfg.api_key = Some("stale-cloudflare-key".into());
        cfg.auth_scheme = AuthScheme::CfAigAuthorization;
        cfg.extra_headers
            .insert("Authorization".into(), "Bearer extra-conflict".into());
        cfg.bearer_resolver = Some(std::sync::Arc::new(Resolver));
        cfg.header_injector = Some(std::sync::Arc::new(ConflictingInjector));
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest {
            builder,
            sent_bearer: sent_fragment,
        } = client.post("https://example.test/v1/responses");
        let request = builder.build().expect("request should build");
        assert_eq!(
            request
                .headers()
                .get("cf-aig-authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer fresh-cloudflare-key")
        );
        assert!(request.headers().get(AUTHORIZATION).is_none());
        assert!(request.headers().get("x-api-key").is_none());
        assert!(request.headers().get("api-key").is_none());
        assert_eq!(sent_fragment.as_deref(), Some("oudflare-key"));
    }

    // Regression: a past change dropped User-Agent from sampling requests.
    #[test]
    fn sampling_client_always_has_user_agent() {
        let client = SamplingClient::new(minimal_config()).expect("build");
        assert!(client.default_headers.contains_key(USER_AGENT));
    }

    #[test]
    fn first_party_endpoint_detection_is_suffix_safe() {
        assert!(is_first_party_grok_endpoint("https://api.x.ai/v1"));
        assert!(is_first_party_grok_endpoint("https://x.ai/v1"));
        assert!(is_first_party_grok_endpoint(
            "https://cli-chat-proxy.grok.com/v1"
        ));
        assert!(is_first_party_grok_endpoint(
            "https://api.spacexai.com/v1/chat"
        ));
        // HTTPS required: cleartext never gets product headers.
        assert!(!is_first_party_grok_endpoint("http://api.x.ai/v1"));
        assert!(!is_first_party_grok_endpoint(
            "http://cli-chat-proxy.grok.com/v1"
        ));
        // Third-party / attacker hosts.
        assert!(!is_first_party_grok_endpoint("https://example.test/v1"));
        assert!(!is_first_party_grok_endpoint(
            "https://api.x.ai.evil.example/v1"
        ));
        assert!(!is_first_party_grok_endpoint(
            "https://evil-x.ai.attacker.com/v1"
        ));
        assert!(!is_first_party_grok_endpoint("https://openai.com/v1"));
        assert!(!is_first_party_grok_endpoint("https://localhost:8080/v1"));
        assert!(!is_first_party_grok_endpoint("not-a-url"));
    }

    #[test]
    fn third_party_base_url_does_not_carry_x_grok_product_headers() {
        let mut cfg = minimal_config();
        // minimal_config uses example.test (third-party).
        cfg.client_version = Some("1.2.3".into());
        cfg.deployment_id = Some("dep-secret".into());
        cfg.user_id = Some("user-secret".into());
        cfg.client_identifier = Some("client-secret".into());
        // Even if extra_headers try to force product metadata…
        cfg.extra_headers
            .insert("x-grok-session-id".into(), "sess-leak".into());
        cfg.extra_headers
            .insert("x-grok-client-identifier".into(), "forced".into());
        let client = SamplingClient::new(cfg).expect("client");
        assert!(
            client
                .default_headers
                .get("x-grok-client-version")
                .is_none(),
            "third-party must not get client-version"
        );
        assert!(client.default_headers.get("x-grok-deployment-id").is_none());
        assert!(client.default_headers.get("x-grok-user-id").is_none());
        assert!(
            client
                .default_headers
                .get("x-grok-client-identifier")
                .is_none()
        );
        assert!(client.default_headers.get("x-grok-session-id").is_none());
        // Authorization (or equivalent) must still be present for auth.
        assert!(
            client.default_headers.get(AUTHORIZATION).is_some(),
            "auth must be unchanged for third-party"
        );
        // post() path also strips late injectors.
        #[derive(Debug)]
        struct LeakInjector;
        impl crate::config::HeaderInjector for LeakInjector {
            fn inject(&self, headers: &mut HeaderMap) {
                headers.insert(
                    HeaderName::from_static("x-grok-session-id"),
                    HeaderValue::from_static("injected-leak"),
                );
            }
        }
        let mut cfg = minimal_config();
        cfg.header_injector = Some(std::sync::Arc::new(LeakInjector));
        let client = SamplingClient::new(cfg).expect("client");
        let SentRequest { builder, .. } = client.post("https://example.test/v1/chat/completions");
        let req = builder.build().expect("build");
        assert!(
            req.headers().get("x-grok-session-id").is_none(),
            "post must strip injector x-grok-* on third-party"
        );
        assert!(req.headers().get(AUTHORIZATION).is_some());
    }

    #[test]
    fn first_party_base_url_includes_x_grok_product_headers() {
        let mut cfg = minimal_config();
        cfg.base_url = "https://api.x.ai/v1".into();
        cfg.client_version = Some("9.9.9".into());
        cfg.deployment_id = Some("dep-ok".into());
        cfg.user_id = Some("user-ok".into());
        cfg.client_identifier = Some("cli-ok".into());
        let client = SamplingClient::new(cfg).expect("client");
        assert_eq!(
            client
                .default_headers
                .get("x-grok-client-version")
                .and_then(|v| v.to_str().ok()),
            Some("9.9.9")
        );
        assert_eq!(
            client
                .default_headers
                .get("x-grok-deployment-id")
                .and_then(|v| v.to_str().ok()),
            Some("dep-ok")
        );
        assert_eq!(
            client
                .default_headers
                .get("x-grok-user-id")
                .and_then(|v| v.to_str().ok()),
            Some("user-ok")
        );
        assert_eq!(
            client
                .default_headers
                .get("x-grok-client-identifier")
                .and_then(|v| v.to_str().ok()),
            Some("cli-ok")
        );
        // Auth still present.
        assert!(client.default_headers.get(AUTHORIZATION).is_some());

        // Per-request headers applied only on first-party.
        let grok = GrokRequestHeaders {
            conv_id: "c1",
            req_id: "r1",
            model_id: "m1",
            session_id: "s1",
            turn_idx: Some("3"),
            agent_id: "a1",
            deployment_id: None,
            user_id: None,
        };
        let SentRequest { builder, .. } = client.post("https://api.x.ai/v1/chat/completions");
        let req = grok
            .apply(builder, client.allows_x_grok_headers())
            .build()
            .expect("build");
        assert_eq!(
            req.headers()
                .get("x-grok-session-id")
                .and_then(|v| v.to_str().ok()),
            Some("s1")
        );
        assert_eq!(
            req.headers()
                .get("x-grok-conv-id")
                .and_then(|v| v.to_str().ok()),
            Some("c1")
        );
    }

    #[test]
    fn third_party_skips_per_request_x_grok_headers() {
        let client = SamplingClient::new(minimal_config()).expect("client");
        assert!(!client.allows_x_grok_headers());
        let grok = GrokRequestHeaders {
            conv_id: "c1",
            req_id: "r1",
            model_id: "m1",
            session_id: "s1",
            turn_idx: Some("3"),
            agent_id: "a1",
            deployment_id: Some("d1"),
            user_id: Some("u1"),
        };
        let SentRequest { builder, .. } = client.post("https://example.test/v1/chat/completions");
        let req = grok
            .apply(builder, client.allows_x_grok_headers())
            .build()
            .expect("build");
        for name in [
            "x-grok-session-id",
            "x-grok-conv-id",
            "x-grok-req-id",
            "x-grok-agent-id",
            "x-grok-deployment-id",
            "x-grok-user-id",
            "x-grok-turn-idx",
            "x-grok-model-override",
        ] {
            assert!(
                req.headers().get(name).is_none(),
                "third-party must not send {name}"
            );
        }
        assert!(req.headers().get(AUTHORIZATION).is_some());
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
        let SentRequest { builder, .. } = client.post("http://localhost/test");
        let req = builder.build().expect("build request");
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

    /// `post()` strips the `"Bearer "` scheme prefix off `Authorization`
    /// and captures the tail fragment (see `BEARER_SUFFIX_LEN`).
    #[test]
    fn post_captures_bearer_tail_for_openai_compat() {
        let cfg = SamplerConfig {
            api_key: Some("test-bearer-1234567890".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest {
            sent_bearer: bearer,
            ..
        } = client.post("https://example.test/v1/chat/completions");
        assert_eq!(bearer.as_deref(), Some("r-1234567890"));
        assert_eq!(
            bearer.as_deref().map(str::len),
            Some(crate::attribution::BEARER_SUFFIX_LEN),
        );
    }

    /// `post()` captures `x-api-key` for Messages-API backends and keeps
    /// the value's tail fragment.
    #[test]
    fn post_captures_x_api_key_tail_for_messages() {
        let cfg = SamplerConfig {
            api_key: Some("anthropic-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest {
            sent_bearer: bearer,
            ..
        } = client.post("https://example.test/v1/messages");
        assert_eq!(bearer.as_deref(), Some("c-key-abc123"));
        assert_eq!(
            bearer.as_deref().map(str::len),
            Some(crate::attribution::BEARER_SUFFIX_LEN),
        );
    }

    /// `post()` captures `None` when the request carries no auth header.
    #[test]
    fn post_captures_none_when_no_header() {
        let cfg = SamplerConfig {
            api_key: None,
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let SentRequest {
            sent_bearer: bearer,
            ..
        } = client.post("https://example.test/v1/chat/completions");
        assert!(bearer.is_none());
    }

    /// The race this design closes: a 401 triggers a recovery that rotates
    /// the resolver, so a record-time re-read attributes a bearer the
    /// rejected request never carried. The attributed fragment must be the
    /// one captured when the request was built.
    #[test]
    fn post_capture_is_immune_to_resolver_rotation_after_build() {
        #[derive(Debug)]
        struct RotatingResolver(std::sync::Mutex<String>);
        impl crate::config::BearerResolver for RotatingResolver {
            fn current_bearer(&self) -> Option<String> {
                Some(self.0.lock().unwrap().clone())
            }
        }

        let resolver = std::sync::Arc::new(RotatingResolver(std::sync::Mutex::new(
            "rejected-token-oldtail1".to_string(),
        )));
        let cfg = SamplerConfig {
            api_key: None,
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(resolver.clone()),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");

        let SentRequest {
            sent_bearer: sent_at_build,
            ..
        } = client.post("https://example.test/v1/responses");
        // The 401 kicks recovery; the resolver rotates before the callback runs.
        *resolver.0.lock().unwrap() = "fresh-token-newtail99".to_string();

        assert_eq!(
            sent_at_build.as_deref(),
            Some("ken-oldtail1"),
            "attribution must describe the bearer the rejected request carried"
        );
        // A record-time re-read (the pre-fix behavior) would report the
        // rotated token instead:
        assert_eq!(
            client.current_sent_bearer_suffix().as_deref(),
            Some("en-newtail99"),
            "sanity: the build-time capture and a live re-read now differ"
        );
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
        let SentRequest { builder, .. } = client.post("https://example.test/v1/messages");
        let request = builder.build().expect("request should build");
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
        let SentRequest {
            builder,
            sent_bearer,
        } = client.post("https://example.test/v1/responses");
        let request = builder.build().expect("request should build");

        assert!(request.headers().get(AUTHORIZATION).is_none());
        assert!(sent_bearer.is_none());
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
            let SentRequest {
                builder,
                sent_bearer,
            } = client.post("https://example.test/v1/responses");
            let request = builder.build().expect("request should build");

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
            assert_eq!(sent_bearer.as_deref(), Some("fresh-bearer"));
            client.record_401_attribution(
                crate::attribution::SamplingConsumer::Responses,
                sent_bearer.as_deref(),
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
        let SentRequest {
            builder,
            sent_bearer,
        } = client.post("https://example.test/v1/messages");
        let request = builder.build().expect("request should build");
        assert!(
            request.headers().get(AUTHORIZATION).is_none(),
            "failed refresh must not send the expired construction-time bearer"
        );
        assert!(request.headers().get("x-api-key").is_none());
        assert!(sent_bearer.is_none());
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
        let SentRequest {
            builder,
            sent_bearer,
        } = client.post("https://example.test/v1/messages");
        let request = builder.build().expect("request should build");
        assert!(request.headers().get(AUTHORIZATION).is_none());
        assert!(
            request.headers().get("x-api-key").is_none(),
            "failed live resolution must not send a stale construction-time x-api-key"
        );
        assert!(sent_bearer.is_none());
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
        let SentRequest { builder, .. } = client.post("https://example.test/v1/responses");
        let request = builder.build().expect("request should build");
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
        let SentRequest { builder, .. } = client.post("https://example.test/v1/messages");
        let request = builder.build().expect("request should build");
        let api_key = request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok());
        assert_eq!(api_key, Some("fresh-anthropic"));
        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    /// The callback receives the `post()`-captured fragment only — the
    /// full bearer never crosses the crate boundary.
    #[test]
    fn record_401_attribution_invokes_callback_with_captured_bearer() {
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
        let SentRequest { sent_bearer, .. } =
            client.post("https://example.test/v1/chat/completions");
        client.record_401_attribution(
            crate::attribution::SamplingConsumer::ChatCompletionsStream,
            sent_bearer.as_deref(),
        );
        let calls = cb.invocations.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            crate::attribution::SamplingConsumer::ChatCompletionsStream
        );
        assert_eq!(calls[0].1.as_deref(), Some("0-extra-tail"));
        assert_eq!(
            calls[0].1.as_deref().map(str::len),
            Some(crate::attribution::BEARER_SUFFIX_LEN),
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
            client.current_sent_bearer_suffix(),
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
        let SentRequest {
            builder,
            sent_bearer,
        } = client.post("https://example.test/v1/responses");
        let request = builder.body("").build().expect("request should build");
        assert!(
            request.headers().get(AUTHORIZATION).is_none(),
            "stale default Authorization must not be sent when resolver is empty"
        );
        assert!(
            sent_bearer.is_none(),
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
        let SentRequest { builder, .. } = client.post("https://example.test/v1/responses");
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
            let named = decode_responses_sse_frame(
                event_type,
                r#"{"side_band":true}"#,
                ResponsesWireDialect::Strict,
            );
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
            let data_only = decode_responses_sse_frame("", &payload, ResponsesWireDialect::Strict);
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

        let Ok(ResponsesStreamItem::Event(rs::ResponseStreamEvent::ResponseOutputTextDelta(event))) =
            decode_responses_sse_frame("", &payload, ResponsesWireDialect::Strict)
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
            ResponsesWireDialect::Strict,
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
        let decoded = decode_responses_sse_frame("", &payload, ResponsesWireDialect::Strict);
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
            ResponsesWireDialect::Strict,
        );
        assert!(matches!(decoded, Err(SamplingError::Serialization(_))));
    }

    fn codex_failed_event(code: &str, message: Option<&str>) -> String {
        serde_json::json!({
            "type": "response.failed",
            "response": {
                "error": {
                    "code": code,
                    "message": message,
                }
            }
        })
        .to_string()
    }

    fn assert_codex_terminal_api_error(
        payload: &str,
        expected_status: reqwest::StatusCode,
        expected_retry: bool,
        expected_delay: Option<u64>,
    ) {
        let Some(SamplingError::Api {
            status,
            retry_after_secs,
            should_retry,
            ..
        }) = codex_terminal_error(payload)
        else {
            panic!("expected a Codex terminal API error");
        };
        assert_eq!(status, expected_status);
        assert_eq!(should_retry, Some(expected_retry));
        assert_eq!(retry_after_secs, expected_delay);
    }

    #[test]
    fn codex_terminal_failures_match_official_retry_policy() {
        for code in ["context_length_exceeded", "invalid_prompt", "bio_policy"] {
            assert_codex_terminal_api_error(
                &codex_failed_event(code, Some("fatal request")),
                reqwest::StatusCode::BAD_REQUEST,
                false,
                None,
            );
        }
        for code in ["insufficient_quota", "usage_not_included"] {
            assert_codex_terminal_api_error(
                &codex_failed_event(code, Some("quota failure")),
                reqwest::StatusCode::PAYMENT_REQUIRED,
                false,
                None,
            );
        }
        assert_codex_terminal_api_error(
            &codex_failed_event("cyber_policy", None),
            reqwest::StatusCode::FORBIDDEN,
            false,
            None,
        );
        for code in ["server_is_overloaded", "slow_down"] {
            assert_codex_terminal_api_error(
                &codex_failed_event(code, Some("try later")),
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                true,
                None,
            );
        }
        assert_codex_terminal_api_error(
            &codex_failed_event("future_server_failure", Some("unknown")),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            true,
            None,
        );
    }

    #[test]
    fn codex_rate_limit_retry_delay_parses_official_message_forms() {
        for (message, seconds) in [
            ("Please try again in 28ms.", 0),
            ("Please try again in 1.898s.", 2),
            ("Rate limit exceeded. Try again in 35 seconds.", 35),
        ] {
            assert_codex_terminal_api_error(
                &codex_failed_event("rate_limit_exceeded", Some(message)),
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                true,
                Some(seconds),
            );
        }
    }

    #[test]
    fn codex_incomplete_max_tokens_uses_existing_truncation_signal() {
        let max_tokens = r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#;
        assert!(matches!(
            codex_terminal_error(max_tokens),
            Some(SamplingError::MaxTokensTruncation)
        ));

        let other = r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#;
        assert_codex_terminal_api_error(
            other,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            true,
            None,
        );
    }

    #[test]
    fn strict_responses_does_not_apply_codex_terminal_error_mapping() {
        let payload = codex_failed_event("context_length_exceeded", Some("too long"));
        let decoded = decode_responses_sse_frame("", &payload, ResponsesWireDialect::Strict);
        assert!(!matches!(
            decoded,
            Err(SamplingError::Api {
                status: reqwest::StatusCode::BAD_REQUEST,
                ..
            })
        ));
    }

    #[test]
    fn codex_turn_state_cache_is_route_and_turn_scoped() {
        let cache: CodexTurnStateCache = Arc::new(Mutex::new(HashMap::new()));
        let route_a = "https://provider-a.test/v1/responses".to_string();
        let route_b = "https://provider-b.test/v1/responses".to_string();
        let a_turn_1 = (route_a.clone(), "session".into(), "turn-1".into());
        let a_turn_2 = (route_a, "session".into(), "turn-2".into());
        let b_turn_1 = (route_b, "session".into(), "turn-1".into());

        cache_codex_turn_state(&cache, &a_turn_1, "state-a1".into());
        cache_codex_turn_state(&cache, &b_turn_1, "state-b1".into());
        assert_eq!(
            take_codex_turn_state(&cache, &a_turn_1).as_deref(),
            Some("state-a1")
        );
        assert_eq!(
            take_codex_turn_state(&cache, &b_turn_1).as_deref(),
            Some("state-b1")
        );

        cache_codex_turn_state(&cache, &a_turn_2, "state-a2".into());
        assert_eq!(
            take_codex_turn_state(&cache, &a_turn_2).as_deref(),
            Some("state-a2")
        );
        assert!(
            !cache
                .lock()
                .expect("turn state cache")
                .contains_key(&a_turn_1)
        );
        assert_eq!(
            take_codex_turn_state(&cache, &b_turn_1).as_deref(),
            Some("state-b1")
        );
    }

    #[test]
    fn codex_responses_accepts_sparse_completed_event() {
        let decoded = decode_responses_sse_frame(
            "",
            r#"{"type":"response.completed","response":{"id":"resp1"}}"#,
            ResponsesWireDialect::Codex,
        )
        .expect("Codex sparse completion should parse");
        let ResponsesStreamItem::Event(rs::ResponseStreamEvent::ResponseCompleted(event)) = decoded
        else {
            panic!("expected ResponseCompleted");
        };
        assert_eq!(event.sequence_number, 0);
        assert_eq!(event.response.id, "resp1");
        assert_eq!(event.response.created_at, 0);
        assert_eq!(event.response.object, "response");
        assert_eq!(event.response.model, "");
        assert_eq!(event.response.status, rs::Status::Completed);
        assert!(event.response.output.is_empty());
    }

    #[test]
    fn strict_responses_rejects_sparse_completed_event() {
        let decoded = decode_responses_sse_frame(
            "",
            r#"{"type":"response.completed","response":{"id":"resp1"}}"#,
            ResponsesWireDialect::Strict,
        );
        assert!(matches!(decoded, Err(SamplingError::Serialization(_))));
    }

    #[test]
    fn codex_responses_still_requires_response_id() {
        let decoded = decode_responses_sse_frame(
            "",
            r#"{"type":"response.completed","response":{}}"#,
            ResponsesWireDialect::Codex,
        );
        assert!(matches!(decoded, Err(SamplingError::Serialization(_))));
    }

    #[test]
    fn codex_responses_accepts_sparse_created_event() {
        let decoded = decode_responses_sse_frame(
            "",
            r#"{"type":"response.created","response":{"id":"resp1"}}"#,
            ResponsesWireDialect::Codex,
        )
        .expect("Codex sparse created event should parse");
        let ResponsesStreamItem::Event(rs::ResponseStreamEvent::ResponseCreated(event)) = decoded
        else {
            panic!("expected ResponseCreated");
        };
        assert_eq!(event.sequence_number, 0);
        assert_eq!(event.response.status, rs::Status::InProgress);
    }

    #[test]
    fn codex_responses_defaults_sparse_usage_details() {
        let decoded = decode_responses_sse_frame(
            "",
            r#"{"type":"response.completed","response":{"id":"resp1","usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#,
            ResponsesWireDialect::Codex,
        )
        .expect("Codex sparse usage should parse");
        let ResponsesStreamItem::Event(rs::ResponseStreamEvent::ResponseCompleted(event)) = decoded
        else {
            panic!("expected ResponseCompleted");
        };
        let usage = event.response.usage.expect("usage should be retained");
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.total_tokens, 5);
        assert_eq!(usage.input_tokens_details.cached_tokens, 0);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 0);
    }

    #[test]
    fn codex_responses_defaults_sequence_number_on_output_events() {
        let decoded = decode_responses_sse_frame(
            "",
            r#"{"type":"response.output_text.delta","item_id":"item1","output_index":0,"content_index":0,"delta":"OK"}"#,
            ResponsesWireDialect::Codex,
        )
        .expect("Codex output event without sequence number should parse");
        let ResponsesStreamItem::Event(rs::ResponseStreamEvent::ResponseOutputTextDelta(event)) =
            decoded
        else {
            panic!("expected ResponseOutputTextDelta");
        };
        assert_eq!(event.sequence_number, 0);
        assert_eq!(event.delta, "OK");
    }

    #[tokio::test]
    async fn codex_text_delta_then_sparse_completed_finishes_turn() {
        let delta = decode_responses_sse_frame(
            "",
            r#"{"type":"response.output_text.delta","sequence_number":1,"item_id":"item1","output_index":0,"content_index":0,"delta":"OK"}"#,
            ResponsesWireDialect::Codex,
        )
        .expect("text delta should parse");
        let completed = decode_responses_sse_frame(
            "",
            r#"{"type":"response.completed","response":{"id":"resp1"}}"#,
            ResponsesWireDialect::Codex,
        )
        .expect("sparse completion should parse");
        let raw = futures_util::StreamExt::boxed(futures_util::stream::iter(vec![
            Ok(delta),
            Ok(completed),
        ]));
        let events = futures_util::StreamExt::collect::<Vec<_>>(crate::stream::stream_responses(
            raw,
            None,
            crate::types::RequestId::from("codex-sparse-test"),
            std::time::Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last() {
            Some(crate::events::SamplingEvent::Completed { response, .. }) => {
                assert_eq!(response.assistant_text(), "OK");
            }
            other => panic!("expected successful completion, got {other:?}"),
        }
    }

    #[test]
    fn decode_messages_sse_frame_skips_unknown_event_types() {
        // Forward-compat: a Messages event type this build does not model is
        // a liveness Ping, not a fatal serialization error.
        let decoded = decode_messages_sse_frame(r#"{"type":"citation","index":0}"#);
        assert!(matches!(decoded, Ok(messages::MessageStreamEvent::Ping)));
    }

    #[test]
    fn decode_messages_sse_frame_skips_unknown_content_block_kinds() {
        let decoded = decode_messages_sse_frame(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"brand_new_block","id":"b1"}}"#,
        );
        assert!(matches!(decoded, Ok(messages::MessageStreamEvent::Ping)));

        let decoded = decode_messages_sse_frame(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"brand_new_delta","x":"y"}}"#,
        );
        assert!(matches!(decoded, Ok(messages::MessageStreamEvent::Ping)));
    }

    #[test]
    fn decode_messages_sse_frame_keeps_malformed_known_events_strict() {
        // Missing required fields on a known type: still a fatal
        // serialization error (wire corruption must not be hidden).
        let decoded = decode_messages_sse_frame(r#"{"type":"content_block_stop"}"#);
        assert!(matches!(decoded, Err(SamplingError::Serialization(_))));
    }

    #[test]
    fn decode_messages_sse_frame_accepts_message_start_without_response_id() {
        let decoded = decode_messages_sse_frame(
            r#"{"type":"message_start","message":{"type":"message","role":"assistant","content":[],"model":"minimax-m3","stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}"#,
        );
        match decoded {
            Ok(messages::MessageStreamEvent::MessageStart { message }) => {
                assert!(message.id.is_empty());
                assert_eq!(message.model, "minimax-m3");
            }
            other => panic!("expected MessageStart, got {other:?}"),
        }
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
        let event = deserialize_response_event(sse, ResponsesWireDialect::Strict)
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

        let event = deserialize_response_event(&make(78), ResponsesWireDialect::Strict)
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
        let event = deserialize_response_event(&make(0), ResponsesWireDialect::Strict)
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
        let event = deserialize_response_event(sse, ResponsesWireDialect::Strict)
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
        let event = deserialize_response_event(sse, ResponsesWireDialect::Strict)
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
        let event = deserialize_response_event(sse, ResponsesWireDialect::Strict)
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
