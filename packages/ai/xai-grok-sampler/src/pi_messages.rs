use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_stream::stream;
use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use xai_grok_sampling_types::{
    AssistantItem, ContentPart, ConversationItem, ConversationRequest, ConversationResponse,
    ConversationToolChoice, PiMessagesNativeBlock, ProviderNativeAssistantState, ReasoningEffort,
    ReasoningModelIdentity, Result, SamplingError, StopReason, TokenUsage, ToolCall,
    reported_cost_ticks, synthesized_reasoning_item,
};

use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingErrorKind, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiMessagesRequest {
    pub model: String,
    pub context: PiContext,
    pub options: PiOptions,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<PiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<PiTool>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum PiMessage {
    User {
        content: Vec<PiUserContent>,
        /// Hyper does not persist source-message timestamps. A deterministic
        /// epoch value keeps replay/cache payloads stable while satisfying the
        /// canonical Pi Message schema.
        timestamp: i64,
    },
    Assistant {
        content: Vec<PiAssistantContent>,
        api: &'static str,
        provider: &'static str,
        model: String,
        usage: PiMessageUsage,
        #[serde(rename = "stopReason")]
        stop_reason: PiDoneReason,
        timestamp: i64,
    },
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        content: Vec<PiToolResultContent>,
        #[serde(rename = "isError")]
        is_error: bool,
        #[serde(
            rename = "addedToolNames",
            default,
            skip_serializing_if = "Vec::is_empty"
        )]
        added_tool_names: Vec<String>,
        timestamp: i64,
    },
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiMessageUsage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
    pub total_tokens: u32,
    pub cost: PiCost,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PiUserContent {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PiToolResultContent {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PiAssistantContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", rename = "textSignature")]
        text_signature: Option<String>,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none", rename = "thinkingSignature")]
        thinking_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        #[serde(skip_serializing_if = "Option::is_none", rename = "thoughtSignature")]
        thought_signature: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<PiToolChoice>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum PiToolChoice {
    String(String),
    Function {
        r#type: String,
        function: PiToolChoiceFunction,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PiToolChoiceFunction {
    pub name: String,
}

pub fn build_request(req: &ConversationRequest, model: &str) -> Result<PiMessagesRequest> {
    let target = req.reasoning_model_identity.as_ref();
    let mut context = PiContext::default();
    let mut tool_names_by_id: HashMap<String, String> = HashMap::new();
    for item in &req.items {
        match item {
            ConversationItem::System(s) => {
                let existing = context.system_prompt.get_or_insert_with(String::new);
                if !existing.is_empty() {
                    existing.push('\n');
                }
                existing.push_str(&s.content);
            }
            ConversationItem::User(u) => context.messages.push(PiMessage::User {
                content: u
                    .content
                    .iter()
                    .map(pi_user_content)
                    .collect::<Result<Vec<_>>>()?,
                timestamp: 0,
            }),
            ConversationItem::Assistant(a) => {
                for tc in &a.tool_calls {
                    tool_names_by_id.insert(tc.id.to_string(), tc.name.clone());
                }
                let content = assistant_content(a, target)?;
                for part in &content {
                    if let PiAssistantContent::ToolCall { id, name, .. } = part {
                        tool_names_by_id.insert(id.clone(), name.clone());
                    }
                }
                let has_tool_calls = content
                    .iter()
                    .any(|part| matches!(part, PiAssistantContent::ToolCall { .. }));
                context.messages.push(PiMessage::Assistant {
                    content,
                    api: "pi-messages",
                    // Radius is the canonical provider for this wire. Custom
                    // pi-messages backends treat this as source metadata; the
                    // request's model/base URL still controls routing.
                    provider: "radius",
                    model: a.model_id.clone().unwrap_or_else(|| model.to_string()),
                    usage: PiMessageUsage::default(),
                    stop_reason: if has_tool_calls {
                        PiDoneReason::ToolUse
                    } else {
                        PiDoneReason::Stop
                    },
                    timestamp: 0,
                });
            }
            ConversationItem::ToolResult(t) => context.messages.push(PiMessage::ToolResult {
                tool_call_id: t.tool_call_id.clone(),
                tool_name: tool_names_by_id
                    .get(&t.tool_call_id)
                    .cloned()
                    .unwrap_or_default(),
                content: tool_result_content(t)?,
                is_error: t.is_error,
                added_tool_names: Vec::new(),
                timestamp: 0,
            }),
            ConversationItem::Reasoning(_)
            | ConversationItem::BackendToolCall(_)
            | ConversationItem::Compaction(_) => {}
        }
    }
    context.tools = req
        .tools
        .iter()
        .map(|t| PiTool {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
        })
        .collect();

    Ok(PiMessagesRequest {
        model: model.to_string(),
        context,
        options: PiOptions {
            temperature: req.temperature,
            max_tokens: req.max_output_tokens,
            reasoning: req.reasoning_effort.map(pi_reasoning_effort),
            cache_retention: req.prompt_cache_retention.clone(),
            session_id: req.x_grok_session_id.clone(),
            tool_choice: req.tool_choice.clone().map(|choice| match choice {
                ConversationToolChoice::Auto => PiToolChoice::String("auto".into()),
                ConversationToolChoice::None => PiToolChoice::String("none".into()),
                ConversationToolChoice::Required => PiToolChoice::String("required".into()),
                ConversationToolChoice::Function(name) => PiToolChoice::Function {
                    r#type: "function".into(),
                    function: PiToolChoiceFunction { name },
                },
            }),
        },
    })
}

fn data_uri_image(url: &str) -> Result<(String, String)> {
    let Some(rest) = url.strip_prefix("data:") else {
        return Err(SamplingError::InvalidConfiguration(
            "Pi Messages requires image data URIs",
        ));
    };
    let Some((meta, data)) = rest.split_once(',') else {
        return Err(SamplingError::InvalidConfiguration(
            "Pi Messages received a malformed image data URI",
        ));
    };
    let mut metadata = meta.split(';');
    let mime_type = metadata.next().unwrap_or_default();
    let is_base64 = metadata.any(|part| part.eq_ignore_ascii_case("base64"));
    if !mime_type.starts_with("image/") || !is_base64 || data.is_empty() {
        return Err(SamplingError::InvalidConfiguration(
            "Pi Messages requires base64-encoded image data URIs",
        ));
    }
    Ok((mime_type.to_string(), data.to_string()))
}

fn pi_user_content(part: &ContentPart) -> Result<PiUserContent> {
    match part {
        ContentPart::Text { text } => Ok(PiUserContent::Text {
            text: text.to_string(),
        }),
        ContentPart::Image { url } => {
            let (mime_type, data) = data_uri_image(url)?;
            Ok(PiUserContent::Image { mime_type, data })
        }
    }
}

fn tool_result_content(
    result: &xai_grok_sampling_types::ToolResultItem,
) -> Result<Vec<PiToolResultContent>> {
    let mut content = Vec::new();
    if !result.content.is_empty() {
        content.push(PiToolResultContent::Text {
            text: result.content.to_string(),
        });
    }
    for part in &result.images {
        match part {
            ContentPart::Text { text } => content.push(PiToolResultContent::Text {
                text: text.to_string(),
            }),
            ContentPart::Image { url } => {
                let (mime_type, data) = data_uri_image(url)?;
                content.push(PiToolResultContent::Image { mime_type, data });
            }
        }
    }
    Ok(content)
}

fn json_object(arguments: &str) -> Result<Value> {
    let value = serde_json::from_str::<Value>(arguments).map_err(SamplingError::Serialization)?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(SamplingError::InvalidConfiguration(
            "Pi Messages tool arguments must be a JSON object",
        ))
    }
}

fn pi_reasoning_effort(effort: ReasoningEffort) -> String {
    match effort {
        ReasoningEffort::None => "off",
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max | ReasoningEffort::Ultra => "max",
    }
    .to_string()
}

fn assistant_content(
    a: &AssistantItem,
    target: Option<&ReasoningModelIdentity>,
) -> Result<Vec<PiAssistantContent>> {
    let replay_native = a
        .reasoning_model_identity
        .as_ref()
        .zip(target)
        .is_some_and(|(a, b)| a == b);
    if replay_native
        && let Some(state) = &a.provider_native_state
        && let Some(blocks) = state.pi_messages_blocks()
    {
        return Ok(blocks
            .iter()
            .map(|block| match block {
                PiMessagesNativeBlock::Text {
                    text,
                    text_signature,
                } => PiAssistantContent::Text {
                    text: text.to_string(),
                    text_signature: text_signature.clone(),
                },
                PiMessagesNativeBlock::Thinking {
                    text,
                    thinking_signature,
                    redacted,
                } => PiAssistantContent::Thinking {
                    thinking: text.to_string(),
                    thinking_signature: thinking_signature.clone(),
                    redacted: *redacted,
                },
                PiMessagesNativeBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    thought_signature,
                } => PiAssistantContent::ToolCall {
                    id: id.to_string(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    thought_signature: thought_signature.clone(),
                },
            })
            .collect());
    }

    let mut content = Vec::new();
    if !a.content.is_empty() {
        content.push(PiAssistantContent::Text {
            text: a.content.to_string(),
            text_signature: None,
        });
    }
    for tc in &a.tool_calls {
        content.push(PiAssistantContent::ToolCall {
            id: tc.id.to_string(),
            name: tc.name.clone(),
            arguments: json_object(&tc.arguments)?,
            thought_signature: None,
        });
    }
    Ok(content)
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PiMessagesEvent {
    Start,
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(default, rename = "contentSignature")]
        content_signature: Option<String>,
    },
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(default, rename = "contentSignature")]
        content_signature: Option<String>,
        #[serde(default)]
        redacted: Option<bool>,
    },
    ToolcallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    ToolcallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: PiWireToolCall,
    },
    Done {
        reason: PiDoneReason,
        #[serde(default)]
        usage: Option<PiUsage>,
        #[serde(default, rename = "responseId")]
        response_id: Option<String>,
        #[serde(default)]
        rewrite: Option<Value>,
    },
    Error {
        reason: PiErrorReason,
        #[serde(default)]
        usage: Option<PiUsage>,
        #[serde(default, rename = "errorMessage")]
        error_message: Option<String>,
        #[serde(default, rename = "responseId")]
        response_id: Option<String>,
        #[serde(default)]
        rewrite: Option<Value>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiWireToolCall {
    pub id: Arc<str>,
    pub name: String,
    pub arguments: Value,
    #[serde(default, rename = "thoughtSignature")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PiDoneReason {
    Stop,
    Length,
    ToolUse,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PiErrorReason {
    Aborted,
    Error,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiUsage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_read: u32,
    #[serde(default)]
    pub cache_write: u32,
    #[serde(default)]
    pub cache_write_1h: u32,
    #[serde(default)]
    pub reasoning: Option<u32>,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub cost: Option<PiCost>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiCost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub total: f64,
}

pub fn decode_event_data(data: &str) -> Result<Option<PiMessagesEvent>> {
    let trimmed = data.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(trimmed).map_err(SamplingError::Serialization)?;
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };
    match kind {
        "start" | "text_start" | "text_delta" | "text_end" | "thinking_start"
        | "thinking_delta" | "thinking_end" | "toolcall_start" | "toolcall_delta"
        | "toolcall_end" | "done" | "error" => serde_json::from_value(value)
            .map(Some)
            .map_err(SamplingError::Serialization),
        _ => {
            tracing::debug!(event_type = %kind, "skipping unknown pi-messages SSE event");
            Ok(None)
        }
    }
}

#[derive(Debug)]
enum BlockState {
    Text(String),
    Thinking(String),
    ToolCall {
        id: String,
        name: String,
        json: String,
    },
}

fn usage(u: Option<&PiUsage>) -> Option<TokenUsage> {
    u.map(|u| {
        let prompt_tokens = u
            .input
            .saturating_add(u.cache_read)
            .saturating_add(u.cache_write);
        let total_tokens = prompt_tokens.saturating_add(u.output);
        if u.total_tokens != 0 && u.total_tokens != total_tokens {
            tracing::debug!(
                wire_total_tokens = u.total_tokens,
                normalized_total_tokens = total_tokens,
                "normalizing pi-messages total token usage"
            );
        }
        TokenUsage {
            prompt_tokens,
            completion_tokens: u.output,
            total_tokens,
            reasoning_tokens: u.reasoning.unwrap_or(0),
            cached_prompt_tokens: u.cache_read,
            cache_creation_prompt_tokens: 0,
        }
    })
}

fn cost_usd_ticks(u: Option<&PiUsage>) -> Option<i64> {
    let total = u?.cost.as_ref()?.total;
    if !total.is_finite() || total <= 0.0 {
        return None;
    }
    let ticks = total * 10_000_000_000.0;
    if !ticks.is_finite() || ticks > i64::MAX as f64 {
        return None;
    }
    reported_cost_ticks(Some(ticks.round() as i64))
}

fn fail(
    request_id: RequestId,
    kind: SamplingErrorKind,
    message: impl Into<String>,
) -> SamplingEvent {
    SamplingEvent::Failed {
        request_id,
        error: SamplingErrorInfo {
            kind,
            status_code: None,
            message: message.into(),
            is_retryable: false,
            retry_after_secs: None,
            should_retry: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: xai_grok_sampling_types::SentCredential::Unknown,
            error_code: None,
        },
    }
}

pub fn stream_pi_messages<'a>(
    raw_stream: BoxStream<'a, Result<PiMessagesEvent>>,
    request_id: RequestId,
    identity: ReasoningModelIdentity,
    idle_timeout: Duration,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    stream! {
        let start = Instant::now();
        let mut timestamps = Vec::new();
        let mut seen_start = false;
        let mut first = false;
        let mut tool_calls_by_index: BTreeMap<usize, ToolCall> = BTreeMap::new();
        let mut text_by_index: BTreeMap<usize, String> = BTreeMap::new();
        let mut reasoning_by_index: BTreeMap<usize, String> = BTreeMap::new();
        let mut native_by_index: BTreeMap<usize, PiMessagesNativeBlock> = BTreeMap::new();
        let mut blocks: HashMap<usize, BlockState> = HashMap::new();
        let mut raw = raw_stream;
        let mut chunk_index = 0_u64;
        let mut text_chunks = 0_u64;
        yield SamplingEvent::StreamStarted { request_id: request_id.clone(), timestamp_ms: chrono::Utc::now().timestamp_millis() };
        loop {
            let next = tokio::time::timeout(idle_timeout, raw.next()).await;
            let item = match next {
                Ok(Some(item)) => item,
                Ok(None) => { yield fail(request_id.clone(), SamplingErrorKind::Api, "pi-messages stream ended without done/error"); break; }
                Err(_) => { yield fail(request_id.clone(), SamplingErrorKind::IdleTimeout, "pi-messages stream idle timeout"); break; }
            };
            let event = match item {
                Ok(e) => e,
                Err(e) => { yield fail(request_id.clone(), SamplingErrorKind::Api, e.to_string()); break; }
            };
            timestamps.push(Instant::now());
            match event {
                PiMessagesEvent::Start => {
                    if seen_start || !blocks.is_empty() || !native_by_index.is_empty() {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, "pi-messages duplicate or late start event");
                        break;
                    }
                    seen_start = true;
                }
                PiMessagesEvent::TextStart { content_index } => {
                    if blocks.contains_key(&content_index) || native_by_index.contains_key(&content_index) {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages duplicate content index {content_index}"));
                        break;
                    }
                    blocks.insert(content_index, BlockState::Text(String::new()));
                }
                PiMessagesEvent::TextDelta { content_index, delta } => {
                    let Some(BlockState::Text(text)) = blocks.get_mut(&content_index) else {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages text_delta for unknown or non-text content index {content_index}"));
                        break;
                    };
                    if !first { first = true; yield SamplingEvent::FirstToken { request_id: request_id.clone() }; }
                    text.push_str(&delta);
                    chunk_index += 1;
                    text_chunks += 1;
                    yield SamplingEvent::ChannelToken { request_id: request_id.clone(), channel: SamplingChannel::Text, text: delta, chunk_index };
                }
                PiMessagesEvent::TextEnd { content_index, content, content_signature } => {
                    let Some(BlockState::Text(streamed)) = blocks.remove(&content_index) else {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages text_end for unknown or non-text content index {content_index}"));
                        break;
                    };
                    let Some(suffix) = content.strip_prefix(&streamed) else {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages text_end content does not extend streamed content index {content_index}"));
                        break;
                    };
                    if !suffix.is_empty() {
                        if !first { first = true; yield SamplingEvent::FirstToken { request_id: request_id.clone() }; }
                        chunk_index += 1;
                        text_chunks += 1;
                        yield SamplingEvent::ChannelToken { request_id: request_id.clone(), channel: SamplingChannel::Text, text: suffix.to_string(), chunk_index };
                    }
                    text_by_index.insert(content_index, content.clone());
                    native_by_index.insert(content_index, PiMessagesNativeBlock::Text { text: Arc::from(content), text_signature: content_signature });
                }
                PiMessagesEvent::ThinkingStart { content_index } => {
                    if blocks.contains_key(&content_index) || native_by_index.contains_key(&content_index) {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages duplicate content index {content_index}"));
                        break;
                    }
                    blocks.insert(content_index, BlockState::Thinking(String::new()));
                }
                PiMessagesEvent::ThinkingDelta { content_index, delta } => {
                    let Some(BlockState::Thinking(text)) = blocks.get_mut(&content_index) else {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages thinking_delta for unknown or non-thinking content index {content_index}"));
                        break;
                    };
                    if !first { first = true; yield SamplingEvent::FirstToken { request_id: request_id.clone() }; }
                    text.push_str(&delta);
                    chunk_index += 1;
                    yield SamplingEvent::ChannelToken { request_id: request_id.clone(), channel: SamplingChannel::Reasoning, text: delta, chunk_index };
                }
                PiMessagesEvent::ThinkingEnd { content_index, content, content_signature, redacted } => {
                    let Some(BlockState::Thinking(streamed)) = blocks.remove(&content_index) else {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages thinking_end for unknown or non-thinking content index {content_index}"));
                        break;
                    };
                    let Some(suffix) = content.strip_prefix(&streamed) else {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages thinking_end content does not extend streamed content index {content_index}"));
                        break;
                    };
                    if !suffix.is_empty() {
                        if !first { first = true; yield SamplingEvent::FirstToken { request_id: request_id.clone() }; }
                        chunk_index += 1;
                        yield SamplingEvent::ChannelToken { request_id: request_id.clone(), channel: SamplingChannel::Reasoning, text: suffix.to_string(), chunk_index };
                    }
                    reasoning_by_index.insert(content_index, content.clone());
                    native_by_index.insert(content_index, PiMessagesNativeBlock::Thinking { text: Arc::from(content), thinking_signature: content_signature, redacted });
                }
                PiMessagesEvent::ToolcallStart { content_index, id, tool_name } => {
                    if blocks.contains_key(&content_index) || native_by_index.contains_key(&content_index) {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages duplicate content index {content_index}"));
                        break;
                    }
                    let Ok(tool_index) = u32::try_from(content_index) else {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages tool content index {content_index} exceeds u32"));
                        break;
                    };
                    if id.is_empty() || tool_name.is_empty() {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages toolcall_start for content index {content_index} has an empty id or name"));
                        break;
                    }
                    if !first { first = true; yield SamplingEvent::FirstToken { request_id: request_id.clone() }; }
                    yield SamplingEvent::ToolCallDelta { request_id: request_id.clone(), tool_index, id: Some(id.clone()), name: Some(tool_name.clone()), arguments_delta: None };
                    blocks.insert(content_index, BlockState::ToolCall { id, name: tool_name, json: String::new() });
                }
                PiMessagesEvent::ToolcallDelta { content_index, delta } => {
                    if let Some(BlockState::ToolCall { id, name, json }) = blocks.get_mut(&content_index) {
                        json.push_str(&delta);
                        yield SamplingEvent::ToolCallDelta { request_id: request_id.clone(), tool_index: content_index as u32, id: Some(id.clone()), name: Some(name.clone()), arguments_delta: Some(delta) };
                    } else {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages toolcall_delta for unknown or non-tool content index {content_index}"));
                        break;
                    }
                }
                PiMessagesEvent::ToolcallEnd { content_index, tool_call } => {
                    let Some(BlockState::ToolCall { id, name, json }) = blocks.remove(&content_index) else {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages toolcall_end for unknown or non-tool content index {content_index}"));
                        break;
                    };
                    if tool_call.id.as_ref() != id || tool_call.name != name {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages toolcall_end identity mismatch for content index {content_index}"));
                        break;
                    }
                    if !tool_call.arguments.is_object() {
                        yield fail(request_id.clone(), SamplingErrorKind::Api, format!("pi-messages toolcall_end arguments for content index {content_index} is not an object"));
                        break;
                    }
                    if !json.is_empty() {
                        match serde_json::from_str::<Value>(&json) {
                            Ok(streamed_arguments) if streamed_arguments != tool_call.arguments => {
                                tracing::debug!(content_index, "pi-messages toolcall_end replaced streamed arguments");
                            }
                            Err(error) => {
                                tracing::debug!(content_index, %error, "pi-messages toolcall_end repaired invalid streamed arguments");
                            }
                            _ => {}
                        }
                    }
                    let args = tool_call.arguments.to_string();
                    tool_calls_by_index.insert(content_index, ToolCall { id: tool_call.id.clone(), name: tool_call.name.clone(), arguments: Arc::from(args) });
                    native_by_index.insert(content_index, PiMessagesNativeBlock::ToolCall { id: tool_call.id, name: tool_call.name, arguments: tool_call.arguments, thought_signature: tool_call.thought_signature });
                }
                PiMessagesEvent::Done { reason, usage: u, response_id, rewrite } => {
                    if !blocks.is_empty() { yield fail(request_id.clone(), SamplingErrorKind::Api, "pi-messages done before all blocks ended"); break; }
                    if rewrite.is_some() { tracing::debug!(has_rewrite = true, "pi-messages rewrite metadata received"); }
                    let stop = match reason { PiDoneReason::Stop => StopReason::Stop, PiDoneReason::Length => StopReason::Length, PiDoneReason::ToolUse => StopReason::ToolCalls };
                    let _response_id = response_id;
                    let model_id = identity.model_id().to_string();
                    let final_text = text_by_index.into_values().collect::<Vec<_>>().join("");
                    let native_blocks = native_by_index.into_values().collect::<Vec<_>>();
                    let assistant = AssistantItem { content: Arc::from(final_text), provider_native_state: Some(ProviderNativeAssistantState::PiMessages { blocks: native_blocks }), tool_calls: tool_calls_by_index.into_values().collect(), model_id: Some(model_id), reasoning_model_identity: Some(identity), model_fingerprint: None, reasoning_effort: None };
                    let mut items = Vec::new();
                    let reasoning = reasoning_by_index.into_values().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                    if !reasoning.is_empty() {
                        items.push(ConversationItem::Reasoning(synthesized_reasoning_item(reasoning)));
                    }
                    items.push(ConversationItem::Assistant(assistant));
                    let cost_usd_ticks = cost_usd_ticks(u.as_ref());
                    let token_usage = usage(u.as_ref());
                    yield SamplingEvent::Completed { request_id, response: Box::new(ConversationResponse { items, stop_reason: Some(stop), usage: token_usage, cost_usd_ticks, message_chunks_emitted: text_chunks, doom_loop_signals: Vec::new(), stop_message: None, message_id: None, raw_stop_reason: None, stop_sequence: None }), metrics: InferenceLatencyStats::from_timestamps(start, &timestamps, Instant::now()) };
                    break;
                }
                PiMessagesEvent::Error { reason, error_message, rewrite, .. } => {
                    if rewrite.is_some() { tracing::debug!(has_rewrite = true, "pi-messages error rewrite metadata received"); }
                    let msg = error_message.unwrap_or_else(|| "pi-messages stream error".into());
                    let kind = match reason { PiErrorReason::Aborted => SamplingErrorKind::Api, PiErrorReason::Error => SamplingErrorKind::Api };
                    yield fail(request_id.clone(), kind, msg);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream as fut_stream;
    use std::sync::Arc;
    use xai_grok_sampling_types::{SystemItem, ToolSpec, UserItem};

    fn identity(base: &str) -> ReasoningModelIdentity {
        ReasoningModelIdentity::new("m", xai_grok_sampling_types::ApiBackend::PiMessages, base)
    }

    #[test]
    fn request_body_maps_context_signatures_tools_and_options() {
        let req = ConversationRequest {
            items: vec![
                ConversationItem::System(SystemItem {
                    content: Arc::from("sys"),
                }),
                ConversationItem::User(UserItem {
                    content: vec![
                        ContentPart::Text {
                            text: Arc::from("hi"),
                        },
                        ContentPart::Image {
                            url: Arc::from("data:image/png;base64,AA=="),
                        },
                    ],
                    ..Default::default()
                }),
                ConversationItem::Assistant(AssistantItem {
                    content: Arc::from("portable"),
                    provider_native_state: Some(ProviderNativeAssistantState::PiMessages {
                        blocks: vec![
                            PiMessagesNativeBlock::Thinking {
                                text: Arc::from("think"),
                                thinking_signature: Some("tsig".into()),
                                redacted: Some(true),
                            },
                            PiMessagesNativeBlock::Text {
                                text: Arc::from("answer"),
                                text_signature: Some("xsig".into()),
                            },
                            PiMessagesNativeBlock::ToolCall {
                                id: Arc::from("call_1"),
                                name: "tool".into(),
                                arguments: serde_json::json!({"x":1}),
                                thought_signature: Some("gsig".into()),
                            },
                        ],
                    }),
                    tool_calls: vec![],
                    model_id: None,
                    reasoning_model_identity: Some(identity("https://pi.test")),
                    model_fingerprint: None,
                    reasoning_effort: None,
                }),
                ConversationItem::ToolResult(xai_grok_sampling_types::ToolResultItem {
                    tool_call_id: "call_1".into(),
                    content: Arc::from("ok"),
                    is_error: true,
                    images: vec![ContentPart::Image {
                        url: Arc::from("data:image/jpeg;base64,/9g="),
                    }],
                }),
            ],
            tools: vec![ToolSpec {
                name: "tool".into(),
                description: Some("desc".into()),
                parameters: serde_json::json!({"type":"object"}),
            }],
            tool_choice: Some(ConversationToolChoice::Function("tool".into())),
            model: Some("m".into()),
            reasoning_model_identity: Some(identity("https://pi.test")),
            temperature: Some(0.2),
            max_output_tokens: Some(123),
            prompt_cache_retention: Some("long".into()),
            x_grok_session_id: Some("s".into()),
            reasoning_effort: Some(ReasoningEffort::None),
            ..Default::default()
        };
        let body = serde_json::to_value(build_request(&req, "m").unwrap()).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["context"]["systemPrompt"], "sys");
        assert_eq!(body["context"]["messages"][0]["timestamp"], 0);
        assert_eq!(body["context"]["messages"][1]["api"], "pi-messages");
        assert_eq!(body["context"]["messages"][1]["provider"], "radius");
        assert_eq!(body["context"]["messages"][1]["model"], "m");
        assert_eq!(body["context"]["messages"][1]["stopReason"], "toolUse");
        assert_eq!(body["context"]["messages"][1]["timestamp"], 0);
        assert_eq!(body["context"]["messages"][1]["usage"]["input"], 0);
        assert_eq!(
            body["context"]["messages"][1]["usage"]["cost"]["total"],
            0.0
        );
        assert_eq!(body["context"]["messages"][2]["timestamp"], 0);
        assert_eq!(
            body["context"]["messages"][0]["content"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            body["context"]["messages"][0]["content"][1]["type"],
            "image"
        );
        assert_eq!(
            body["context"]["messages"][0]["content"][1]["mimeType"],
            "image/png"
        );
        assert_eq!(body["context"]["messages"][0]["content"][1]["data"], "AA==");
        assert_eq!(
            body["context"]["messages"][1]["content"][0]["thinkingSignature"],
            "tsig"
        );
        assert!(
            body["context"]["messages"][1]["content"][0]
                .get("thoughtSignature")
                .is_none()
        );
        assert_eq!(
            body["context"]["messages"][1]["content"][2]["thoughtSignature"],
            "gsig"
        );
        assert_eq!(
            body["context"]["messages"][1]["content"][2]["arguments"]["x"],
            1
        );
        assert_eq!(body["context"]["messages"][2]["isError"], true);
        assert_eq!(body["context"]["messages"][2]["toolName"], "tool");
        assert_eq!(
            body["context"]["messages"][2]["content"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(body["context"]["messages"][2]["content"][0]["text"], "ok");
        assert_eq!(
            body["context"]["messages"][2]["content"][1]["mimeType"],
            "image/jpeg"
        );
        assert_eq!(body["context"]["messages"][2]["content"][1]["data"], "/9g=");
        assert!(body["context"]["messages"][2].get("compatNote").is_none());
        assert_eq!(body["context"]["tools"][0]["name"], "tool");
        assert!((body["options"]["temperature"].as_f64().unwrap() - 0.2).abs() < 0.00001);
        assert_eq!(body["options"]["maxTokens"], 123);
        assert_eq!(body["options"]["cacheRetention"], "long");
        assert_eq!(body["options"]["reasoning"], "off");
        assert_eq!(body["options"]["sessionId"], "s");
        assert_eq!(body["options"]["toolChoice"]["function"]["name"], "tool");
    }

    #[test]
    fn request_rejects_non_data_images_and_non_object_tool_arguments() {
        let image_request = ConversationRequest {
            items: vec![ConversationItem::User(UserItem {
                content: vec![ContentPart::Image {
                    url: Arc::from("https://example.com/image.png"),
                }],
                ..Default::default()
            })],
            ..Default::default()
        };
        assert!(matches!(
            build_request(&image_request, "m"),
            Err(SamplingError::InvalidConfiguration(_))
        ));

        let arguments_request = ConversationRequest {
            items: vec![ConversationItem::Assistant(AssistantItem {
                content: Arc::from(""),
                provider_native_state: None,
                tool_calls: vec![ToolCall {
                    id: Arc::from("call"),
                    name: "tool".into(),
                    arguments: Arc::from("[]"),
                }],
                model_id: None,
                reasoning_model_identity: None,
                model_fingerprint: None,
                reasoning_effort: None,
            })],
            ..Default::default()
        };
        assert!(matches!(
            build_request(&arguments_request, "m"),
            Err(SamplingError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn identity_mismatch_falls_back_to_portable_text_and_tool_calls() {
        let assistant = AssistantItem {
            content: Arc::from("portable"),
            provider_native_state: Some(ProviderNativeAssistantState::PiMessages {
                blocks: vec![PiMessagesNativeBlock::Text {
                    text: Arc::from("native"),
                    text_signature: Some("sig".into()),
                }],
            }),
            tool_calls: vec![ToolCall {
                id: Arc::from("t"),
                name: "tool".into(),
                arguments: Arc::from("{}"),
            }],
            model_id: None,
            reasoning_model_identity: Some(identity("https://one")),
            model_fingerprint: None,
            reasoning_effort: None,
        };
        let req = ConversationRequest {
            items: vec![ConversationItem::Assistant(assistant)],
            reasoning_model_identity: Some(identity("https://two")),
            ..Default::default()
        };
        let body = serde_json::to_value(build_request(&req, "m").unwrap()).unwrap();
        let content = &body["context"]["messages"][0]["content"];
        assert_eq!(content[0]["text"], "portable");
        assert!(content[0].get("textSignature").is_none());
        assert_eq!(content[1]["type"], "toolCall");
    }

    #[test]
    fn decode_done_unknown_and_crlf_multidata_payload() {
        assert!(decode_event_data("[DONE]").unwrap().is_none());
        assert!(
            decode_event_data(r#"{"type":"future_event","x":1}"#)
                .unwrap()
                .is_none()
        );
        let event = decode_event_data(r#"{"type":"text_delta","contentIndex":0,"delta":"hi"}"#)
            .unwrap()
            .unwrap();
        assert!(matches!(event, PiMessagesEvent::TextDelta { delta, .. } if delta == "hi"));
    }

    #[tokio::test]
    async fn stream_interleaves_text_thinking_tool_usage_and_response_id() {
        let events = vec![
            Ok(PiMessagesEvent::Start),
            Ok(PiMessagesEvent::TextStart { content_index: 0 }),
            Ok(PiMessagesEvent::TextDelta {
                content_index: 0,
                delta: "he".into(),
            }),
            Ok(PiMessagesEvent::ThinkingStart { content_index: 1 }),
            Ok(PiMessagesEvent::ThinkingDelta {
                content_index: 1,
                delta: "why".into(),
            }),
            Ok(PiMessagesEvent::ThinkingEnd {
                content_index: 1,
                content: "why because".into(),
                content_signature: Some("rs".into()),
                redacted: None,
            }),
            Ok(PiMessagesEvent::TextEnd {
                content_index: 0,
                content: "hello".into(),
                content_signature: Some("ts".into()),
            }),
            Ok(PiMessagesEvent::ToolcallStart {
                content_index: 2,
                id: "c".into(),
                tool_name: "tool".into(),
            }),
            Ok(PiMessagesEvent::ToolcallDelta {
                content_index: 2,
                delta: "{\"a\":".into(),
            }),
            Ok(PiMessagesEvent::ToolcallDelta {
                content_index: 2,
                delta: "1}".into(),
            }),
            Ok(PiMessagesEvent::ToolcallEnd {
                content_index: 2,
                tool_call: PiWireToolCall {
                    id: Arc::from("c"),
                    name: "tool".into(),
                    arguments: serde_json::json!({"a":1}),
                    thought_signature: Some("tool_sig".into()),
                },
            }),
            Ok(PiMessagesEvent::Done {
                reason: PiDoneReason::ToolUse,
                usage: Some(PiUsage {
                    input: 10,
                    output: 2,
                    cache_read: 3,
                    cache_write: 4,
                    cache_write_1h: 0,
                    reasoning: Some(1),
                    // Deliberately omit cache buckets from the wire total to
                    // verify Hyper normalizes to full prompt + output.
                    total_tokens: 12,
                    cost: Some(PiCost {
                        total: 0.3,
                        ..Default::default()
                    }),
                }),
                response_id: Some("resp_1".into()),
                rewrite: Some(serde_json::json!({"changed":false})),
            }),
        ];
        let raw = fut_stream::iter(events).boxed();
        let (response, _) = crate::stream::collect_response(stream_pi_messages(
            raw,
            RequestId::from("r"),
            identity("https://pi"),
            Duration::from_secs(30),
        ))
        .await
        .unwrap();
        assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
        assert_eq!(response.usage.as_ref().unwrap().prompt_tokens, 17);
        assert_eq!(response.usage.as_ref().unwrap().cached_prompt_tokens, 3);
        assert_eq!(response.usage.as_ref().unwrap().total_tokens, 19);
        assert_eq!(response.cost_usd_ticks, Some(3_000_000_000));
        assert_eq!(response.message_chunks_emitted, 2);
        let assistant = response.assistant().unwrap();
        assert_eq!(assistant.content.as_ref(), "hello");
        assert_eq!(assistant.model_id.as_deref(), Some("m"));
        assert_eq!(assistant.tool_calls[0].arguments.as_ref(), "{\"a\":1}");
        assert!(matches!(response.items[0], ConversationItem::Reasoning(_)));
        assert!(matches!(
            assistant
                .provider_native_state
                .as_ref()
                .unwrap()
                .pi_messages_blocks()
                .unwrap()[1],
            PiMessagesNativeBlock::Thinking { .. }
        ));
        assert!(
            matches!(&assistant.provider_native_state.as_ref().unwrap().pi_messages_blocks().unwrap()[2], PiMessagesNativeBlock::ToolCall { thought_signature: Some(sig), .. } if sig == "tool_sig")
        );
    }

    #[tokio::test]
    async fn stream_fails_truncated_and_unended_blocks() {
        let raw = fut_stream::iter(Vec::<Result<PiMessagesEvent>>::new()).boxed();
        let err = crate::stream::collect_response(stream_pi_messages(
            raw,
            RequestId::from("r"),
            identity("https://pi"),
            Duration::from_secs(30),
        ))
        .await
        .unwrap_err();
        assert!(err.message.contains("without done"));

        let raw = fut_stream::iter(vec![
            Ok(PiMessagesEvent::TextStart { content_index: 0 }),
            Ok(PiMessagesEvent::Done {
                reason: PiDoneReason::Stop,
                usage: None,
                response_id: None,
                rewrite: None,
            }),
        ])
        .boxed();
        let err = crate::stream::collect_response(stream_pi_messages(
            raw,
            RequestId::from("r"),
            identity("https://pi"),
            Duration::from_secs(30),
        ))
        .await
        .unwrap_err();
        assert!(err.message.contains("before all blocks ended"));
    }

    #[tokio::test]
    async fn stream_emits_toolcall_start_for_zero_args_and_sorts_tool_calls() {
        let events = vec![
            Ok(PiMessagesEvent::Start),
            Ok(PiMessagesEvent::ToolcallStart {
                content_index: 5,
                id: "b".into(),
                tool_name: "tool_b".into(),
            }),
            Ok(PiMessagesEvent::ToolcallEnd {
                content_index: 5,
                tool_call: PiWireToolCall {
                    id: Arc::from("b"),
                    name: "tool_b".into(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                },
            }),
            Ok(PiMessagesEvent::ToolcallStart {
                content_index: 2,
                id: "a".into(),
                tool_name: "tool_a".into(),
            }),
            Ok(PiMessagesEvent::ToolcallEnd {
                content_index: 2,
                tool_call: PiWireToolCall {
                    id: Arc::from("a"),
                    name: "tool_a".into(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                },
            }),
            Ok(PiMessagesEvent::Done {
                reason: PiDoneReason::ToolUse,
                usage: None,
                response_id: None,
                rewrite: None,
            }),
        ];
        let mut out: Vec<_> = stream_pi_messages(
            fut_stream::iter(events).boxed(),
            RequestId::from("r"),
            identity("https://pi"),
            Duration::from_secs(30),
        )
        .collect()
        .await;
        assert!(matches!(out.get(1), Some(SamplingEvent::FirstToken { .. })));
        assert!(
            matches!(out.get(2), Some(SamplingEvent::ToolCallDelta { id: Some(id), name: Some(name), arguments_delta: None, .. }) if id == "b" && name == "tool_b")
        );
        let completed = out.pop().unwrap();
        let SamplingEvent::Completed { response, .. } = completed else {
            panic!("expected completed");
        };
        let assistant = response.assistant().unwrap();
        assert_eq!(assistant.tool_calls[0].name, "tool_a");
        assert_eq!(assistant.tool_calls[1].name, "tool_b");
    }

    #[tokio::test]
    async fn stream_accepts_authoritative_tool_arguments_and_rejects_text_rewrite() {
        let events = vec![
            Ok(PiMessagesEvent::ToolcallStart {
                content_index: 0,
                id: "c".into(),
                tool_name: "tool".into(),
            }),
            Ok(PiMessagesEvent::ToolcallDelta {
                content_index: 0,
                delta: r#"{"x":1}"#.into(),
            }),
            Ok(PiMessagesEvent::ToolcallEnd {
                content_index: 0,
                tool_call: PiWireToolCall {
                    id: Arc::from("c"),
                    name: "tool".into(),
                    arguments: serde_json::json!({"x":2}),
                    thought_signature: None,
                },
            }),
            Ok(PiMessagesEvent::Done {
                reason: PiDoneReason::ToolUse,
                usage: None,
                response_id: None,
                rewrite: None,
            }),
        ];
        let (response, _) = crate::stream::collect_response(stream_pi_messages(
            fut_stream::iter(events).boxed(),
            RequestId::from("r"),
            identity("https://pi"),
            Duration::from_secs(30),
        ))
        .await
        .unwrap();
        assert_eq!(
            response.assistant().unwrap().tool_calls[0]
                .arguments
                .as_ref(),
            r#"{"x":2}"#
        );

        let rewrite = vec![
            Ok(PiMessagesEvent::TextStart { content_index: 0 }),
            Ok(PiMessagesEvent::TextDelta {
                content_index: 0,
                delta: "abc".into(),
            }),
            Ok(PiMessagesEvent::TextEnd {
                content_index: 0,
                content: "xyz".into(),
                content_signature: None,
            }),
        ];
        let error = crate::stream::collect_response(stream_pi_messages(
            fut_stream::iter(rewrite).boxed(),
            RequestId::from("r"),
            identity("https://pi"),
            Duration::from_secs(30),
        ))
        .await
        .unwrap_err();
        assert!(error.message.contains("does not extend streamed content"));
    }

    #[tokio::test]
    async fn stream_rejects_duplicate_wrong_type_unknown_and_non_object_tool_args() {
        let cases = vec![
            (
                vec![
                    Ok(PiMessagesEvent::TextStart { content_index: 0 }),
                    Ok(PiMessagesEvent::ThinkingStart { content_index: 0 }),
                ],
                "duplicate content index",
            ),
            (
                vec![
                    Ok(PiMessagesEvent::TextStart { content_index: 0 }),
                    Ok(PiMessagesEvent::ThinkingDelta {
                        content_index: 0,
                        delta: "x".into(),
                    }),
                ],
                "thinking_delta for unknown or non-thinking",
            ),
            (
                vec![Ok(PiMessagesEvent::ToolcallDelta {
                    content_index: 9,
                    delta: "{}".into(),
                })],
                "toolcall_delta for unknown or non-tool",
            ),
            (
                vec![
                    Ok(PiMessagesEvent::ToolcallStart {
                        content_index: 0,
                        id: "c".into(),
                        tool_name: "tool".into(),
                    }),
                    Ok(PiMessagesEvent::ToolcallEnd {
                        content_index: 0,
                        tool_call: PiWireToolCall {
                            id: Arc::from("other"),
                            name: "tool".into(),
                            arguments: serde_json::json!({}),
                            thought_signature: None,
                        },
                    }),
                ],
                "identity mismatch",
            ),
            (
                vec![
                    Ok(PiMessagesEvent::ToolcallStart {
                        content_index: 0,
                        id: "c".into(),
                        tool_name: "tool".into(),
                    }),
                    Ok(PiMessagesEvent::ToolcallEnd {
                        content_index: 0,
                        tool_call: PiWireToolCall {
                            id: Arc::from("c"),
                            name: "tool".into(),
                            arguments: serde_json::json!([]),
                            thought_signature: None,
                        },
                    }),
                ],
                "is not an object",
            ),
        ];
        for (events, expected) in cases {
            let err = crate::stream::collect_response(stream_pi_messages(
                fut_stream::iter(events).boxed(),
                RequestId::from("r"),
                identity("https://pi"),
                Duration::from_secs(30),
            ))
            .await
            .unwrap_err();
            assert!(
                err.message.contains(expected),
                "{} did not contain {}",
                err.message,
                expected
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stream_fails_idle_timeout() {
        let raw = fut_stream::pending::<Result<PiMessagesEvent>>().boxed();
        let fut = crate::stream::collect_response(stream_pi_messages(
            raw,
            RequestId::from("r"),
            identity("https://pi"),
            Duration::from_secs(5),
        ));
        tokio::pin!(fut);
        tokio::time::advance(Duration::from_secs(6)).await;
        let err = fut.await.unwrap_err();
        assert_eq!(err.kind, SamplingErrorKind::IdleTimeout);
    }
}
