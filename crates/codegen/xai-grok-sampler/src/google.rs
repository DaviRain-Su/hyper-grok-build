//! Native Google Gemini GenerateContent REST/SSE adapter.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_stream::stream;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use token_source::TokenSourceProvider;

use xai_grok_sampling_types::{
    AssistantItem, ContentPart, ConversationItem, ConversationRequest, ConversationResponse,
    ConversationToolChoice, GoogleNativePart, ProviderNativeAssistantState, ReasoningModelIdentity,
    RequestCompat, Result, SamplingError, StopReason, TokenUsage, ToolCall,
};

use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

const GOOGLE_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const VERTEX_EXPRESS_BASE_URL: &str = "https://aiplatform.googleapis.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoogleEndpointKind {
    GenerativeLanguage,
    Vertex,
}

#[derive(Debug, Clone)]
pub struct GoogleEndpoint {
    pub kind: GoogleEndpointKind,
    pub base_url: String,
    pub model: String,
    pub project: Option<String>,
    pub location: Option<String>,
}

impl GoogleEndpoint {
    pub fn from_config(base_url: &str, model: &str) -> Self {
        let kind = if base_url.contains("aiplatform.googleapis.com") {
            GoogleEndpointKind::Vertex
        } else {
            GoogleEndpointKind::GenerativeLanguage
        };
        Self {
            kind,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            project: std::env::var("GOOGLE_CLOUD_PROJECT")
                .ok()
                .or_else(|| std::env::var("GCLOUD_PROJECT").ok()),
            location: std::env::var("GOOGLE_CLOUD_LOCATION").ok(),
        }
    }

    pub fn url(&self, stream: bool, vertex_express: bool) -> Result<String> {
        let suffix = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let base = self.resolved_base_url(vertex_express);
        let url = match self.kind {
            GoogleEndpointKind::GenerativeLanguage => {
                format!("{base}/models/{}:{suffix}", self.model)
            }
            GoogleEndpointKind::Vertex if vertex_express => {
                format!("{base}/v1/publishers/google/models/{}:{suffix}", self.model)
            }
            GoogleEndpointKind::Vertex => {
                let project = self.project.as_deref().ok_or(SamplingError::InvalidConfiguration(
                    "Google Vertex requires GOOGLE_CLOUD_PROJECT or GCLOUD_PROJECT when no Vertex API key is configured",
                ))?;
                let location = self.location.as_deref().ok_or(SamplingError::InvalidConfiguration(
                    "Google Vertex requires GOOGLE_CLOUD_LOCATION when no Vertex API key is configured",
                ))?;
                format!(
                    "{base}/v1/projects/{project}/locations/{location}/publishers/google/models/{}:{suffix}",
                    self.model
                )
            }
        };
        if stream {
            Ok(format!("{url}?alt=sse"))
        } else {
            Ok(url)
        }
    }

    fn resolved_base_url(&self, vertex_express: bool) -> String {
        let base = self.base_url.trim_end_matches('/');
        if !vertex_express || !matches!(self.kind, GoogleEndpointKind::Vertex) {
            return base.to_string();
        }
        if base.contains('{') || base.contains("{GOOGLE_CLOUD_LOCATION}") {
            return VERTEX_EXPRESS_BASE_URL.to_string();
        }
        base.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    pub contents: Vec<GoogleContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GoogleContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GoogleTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GoogleContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub parts: Vec<GooglePart>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GooglePart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<InlineData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GoogleFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GoogleFunctionResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineData {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GoogleFunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GoogleFunctionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub response: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<GooglePart>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleTool {
    pub function_declarations: Vec<GoogleFunctionDeclaration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleFunctionDeclaration {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters_json_schema: Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    pub response_id: Option<String>,
    pub candidates: Option<Vec<GoogleCandidate>>,
    pub usage_metadata: Option<GoogleUsageMetadata>,
    pub prompt_feedback: Option<GooglePromptFeedback>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoogleCandidate {
    pub content: Option<GoogleContent>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GooglePromptFeedback {
    pub block_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoogleUsageMetadata {
    pub prompt_token_count: Option<u32>,
    pub candidates_token_count: Option<u32>,
    pub thoughts_token_count: Option<u32>,
    pub cached_content_token_count: Option<u32>,
    pub total_token_count: Option<u32>,
}

pub fn build_request(
    req: &ConversationRequest,
    model: &str,
    compat: Option<&RequestCompat>,
) -> GenerateContentRequest {
    let mut system = Vec::new();
    let mut contents = Vec::new();
    let target = req.reasoning_model_identity.as_ref();
    let mut pending_function_responses: Option<GoogleContent> = None;

    for item in &req.items {
        match item {
            ConversationItem::System(s) => system.push(s.content.to_string()),
            ConversationItem::User(u) => {
                flush_pending(&mut pending_function_responses, &mut contents);
                let parts = u
                    .content
                    .iter()
                    .filter_map(content_part_to_google)
                    .collect::<Vec<_>>();
                if !parts.is_empty() {
                    contents.push(GoogleContent {
                        role: Some("user".into()),
                        parts,
                    });
                }
            }
            ConversationItem::Assistant(a) => {
                flush_pending(&mut pending_function_responses, &mut contents);
                let parts = assistant_parts(a, target);
                if !parts.is_empty() {
                    contents.push(GoogleContent {
                        role: Some("model".into()),
                        parts,
                    });
                }
            }
            ConversationItem::ToolResult(t) => {
                let image_parts = t
                    .images
                    .iter()
                    .filter_map(content_part_to_google)
                    .filter(|p| p.inline_data.is_some())
                    .collect::<Vec<_>>();
                let response_text = if t.content.is_empty() && !image_parts.is_empty() {
                    "(see attached image)".to_string()
                } else {
                    t.content.to_string()
                };
                let function_response = GooglePart {
                    function_response: Some(GoogleFunctionResponse {
                        id: Some(t.tool_call_id.clone()),
                        name: tool_name_for_result(&req.items, &t.tool_call_id)
                            .unwrap_or_else(|| t.tool_call_id.clone()),
                        response: json!({ "output": response_text }),
                        parts: (!image_parts.is_empty()).then_some(image_parts),
                    }),
                    ..Default::default()
                };
                let entry = pending_function_responses.get_or_insert_with(|| GoogleContent {
                    role: Some("user".into()),
                    parts: Vec::new(),
                });
                entry.parts.push(function_response);
            }
            ConversationItem::Reasoning(_)
            | ConversationItem::BackendToolCall(_)
            | ConversationItem::Compaction(_) => {}
        }
    }
    flush_pending(&mut pending_function_responses, &mut contents);

    let tools = if req.tools.is_empty() {
        None
    } else {
        Some(vec![GoogleTool {
            function_declarations: req
                .tools
                .iter()
                .map(|t| GoogleFunctionDeclaration {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters_json_schema: t.parameters.clone(),
                })
                .collect(),
        }])
    };

    let mut generation_config = Map::new();
    if let Some(t) = req.temperature {
        generation_config.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        generation_config.insert("topP".into(), json!(p));
    }
    if let Some(m) = req.max_output_tokens {
        generation_config.insert("maxOutputTokens".into(), json!(m));
    }
    if let Some(schema) = &req.json_schema {
        generation_config.insert("responseMimeType".into(), json!("application/json"));
        generation_config.insert("responseJsonSchema".into(), schema.clone());
    }
    if let Some(thinking) = thinking_config(req, model, compat) {
        generation_config.insert("thinkingConfig".into(), thinking);
    }

    GenerateContentRequest {
        contents,
        system_instruction: (!system.is_empty()).then(|| GoogleContent {
            role: Some("system".into()),
            parts: vec![GooglePart {
                text: Some(system.join("\n\n")),
                ..Default::default()
            }],
        }),
        tools,
        tool_config: tool_config(req, compat),
        generation_config: (!generation_config.is_empty())
            .then(|| Value::Object(generation_config)),
    }
}

fn flush_pending(pending: &mut Option<GoogleContent>, contents: &mut Vec<GoogleContent>) {
    if let Some(content) = pending.take()
        && !content.parts.is_empty()
    {
        contents.push(content);
    }
}

fn content_part_to_google(part: &ContentPart) -> Option<GooglePart> {
    match part {
        ContentPart::Text { text } => Some(GooglePart {
            text: Some(text.to_string()),
            ..Default::default()
        }),
        ContentPart::Image { url } => parse_data_uri(url).map(|(mime_type, data)| GooglePart {
            inline_data: Some(InlineData { mime_type, data }),
            ..Default::default()
        }),
    }
}

fn parse_data_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.split(';').next().unwrap_or("application/octet-stream");
    Some((mime.to_string(), data.to_string()))
}

fn assistant_parts(a: &AssistantItem, target: Option<&ReasoningModelIdentity>) -> Vec<GooglePart> {
    let replay_native = a
        .reasoning_model_identity
        .as_ref()
        .zip(target)
        .is_some_and(|(a, b)| a == b);
    if replay_native
        && let Some(state) = &a.provider_native_state
        && let Some(parts) = state.google_parts()
    {
        return parts.iter().filter_map(native_part_to_google).collect();
    }
    let mut parts = Vec::new();
    if !a.content.is_empty() {
        parts.push(GooglePart {
            text: Some(a.content.to_string()),
            ..Default::default()
        });
    }
    for call in &a.tool_calls {
        let args = serde_json::from_str(call.arguments.as_ref()).unwrap_or_else(|_| json!({}));
        parts.push(GooglePart {
            function_call: Some(GoogleFunctionCall {
                id: Some(call.id.to_string()),
                name: call.name.clone(),
                args,
            }),
            ..Default::default()
        });
    }
    parts
}

fn native_part_to_google(part: &GoogleNativePart) -> Option<GooglePart> {
    match part {
        GoogleNativePart::Text {
            text,
            thought_signature,
        } => Some(GooglePart {
            text: Some(text.to_string()),
            thought_signature: valid_sig(thought_signature.as_deref()),
            ..Default::default()
        }),
        GoogleNativePart::Thinking {
            text,
            thought_signature,
        } => Some(GooglePart {
            text: Some(text.to_string()),
            thought: Some(true),
            thought_signature: valid_sig(thought_signature.as_deref()),
            ..Default::default()
        }),
        GoogleNativePart::ToolCall {
            id,
            name,
            arguments,
            thought_signature,
        } => Some(GooglePart {
            function_call: Some(GoogleFunctionCall {
                id: Some(id.to_string()),
                name: name.clone(),
                args: arguments.clone(),
            }),
            thought_signature: valid_sig(thought_signature.as_deref()),
            ..Default::default()
        }),
    }
}

fn valid_sig(sig: Option<&str>) -> Option<String> {
    let sig = sig?;
    if sig.len() % 4 == 0
        && sig
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        Some(sig.to_string())
    } else {
        None
    }
}

fn tool_name_for_result(items: &[ConversationItem], id: &str) -> Option<String> {
    items.iter().rev().find_map(|item| match item {
        ConversationItem::Assistant(a) => a
            .tool_calls
            .iter()
            .find(|c| c.id.as_ref() == id)
            .map(|c| c.name.clone()),
        _ => None,
    })
}

fn tool_config(req: &ConversationRequest, compat: Option<&RequestCompat>) -> Option<Value> {
    if req.tools.is_empty() {
        return None;
    }
    let compat = compat.and_then(RequestCompat::google_generate_content);
    let strict = compat.is_some_and(|c| c.supports_strict_tool_sampling)
        && req
            .tools
            .iter()
            .any(|tool| schema_requests_strict(&tool.parameters));
    let mut function_calling = Map::new();
    let mode = match &req.tool_choice {
        Some(ConversationToolChoice::None) => Some("NONE"),
        Some(ConversationToolChoice::Required) => Some("ANY"),
        Some(ConversationToolChoice::Function(name)) => {
            function_calling.insert("allowedFunctionNames".into(), json!([name]));
            Some("ANY")
        }
        Some(ConversationToolChoice::Auto) | None if strict => Some("VALIDATED"),
        Some(ConversationToolChoice::Auto) | None => None,
    }?;
    function_calling.insert("mode".into(), json!(mode));
    Some(json!({ "functionCallingConfig": Value::Object(function_calling) }))
}

fn schema_requests_strict(schema: &Value) -> bool {
    schema
        .get("strict")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || schema
            .get("x-strict")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn thinking_config(
    req: &ConversationRequest,
    model: &str,
    compat: Option<&RequestCompat>,
) -> Option<Value> {
    let effort = req.reasoning_effort?;
    let compat = compat.and_then(RequestCompat::google_generate_content);
    let token = effort.as_str();
    if let Some(level) = compat
        .and_then(|c| c.thinking_level_map.get(token))
        .cloned()
        .flatten()
    {
        return Some(json!({ "includeThoughts": true, "thinkingLevel": level }));
    }
    if model.contains("2.5-pro") {
        return Some(
            json!({ "includeThoughts": true, "thinkingBudget": match token { "minimal" => 128, "low" => 2048, "medium" => 8192, "high" => 32768, _ => -1 } }),
        );
    }
    if model.contains("2.5-flash-lite") {
        return Some(
            json!({ "includeThoughts": true, "thinkingBudget": match token { "minimal" => 512, "low" => 2048, "medium" => 8192, "high" => 24576, _ => -1 } }),
        );
    }
    if model.contains("2.5-flash") {
        return Some(
            json!({ "includeThoughts": true, "thinkingBudget": match token { "minimal" => 128, "low" => 2048, "medium" => 8192, "high" => 24576, _ => -1 } }),
        );
    }
    Some(json!({ "includeThoughts": true, "thinkingBudget": -1 }))
}

pub fn stream_google_generate_content<'a>(
    raw_stream: BoxStream<'a, Result<GenerateContentResponse>>,
    request_id: RequestId,
    identity: ReasoningModelIdentity,
    idle_timeout: Duration,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    stream! {
        let start = Instant::now();
        let mut chunk_timestamps = Vec::new();
        yield SamplingEvent::StreamStarted { request_id: request_id.clone(), timestamp_ms: chrono::Utc::now().timestamp_millis() };
        let mut text_acc = String::new();
        let mut reasoning_acc = String::new();
        let mut native_parts = Vec::new();
        let mut calls = Vec::new();
        let mut usage = None;
        let mut finish = None;
        let mut response_id: Option<String> = None;
        let mut chunk_index = 0u64;
        let mut msg_chunks = 0u64;
        let mut first = false;
        let mut stream = raw_stream;
        loop {
            let next = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(next)) => next,
                Ok(None) => break,
                Err(_) => {
                    let err = SamplingError::IdleTimeout { elapsed_secs: idle_timeout.as_secs() };
                    yield SamplingEvent::Failed { request_id: request_id.clone(), error: SamplingErrorInfo::from(&err) };
                    return;
                }
            };
            let chunk = match next {
                Ok(c) => c,
                Err(e) => {
                    yield SamplingEvent::Failed { request_id: request_id.clone(), error: SamplingErrorInfo::from(&e) };
                    return;
                }
            };
            response_id = response_id.or(chunk.response_id);
            if let Some(prompt_feedback) = chunk.prompt_feedback
                && prompt_feedback.block_reason.is_some()
            {
                finish = Some(StopReason::ContentFilter);
            }
            if let Some(u) = chunk.usage_metadata {
                usage = Some(TokenUsage {
                    prompt_tokens: u.prompt_token_count.unwrap_or(0).saturating_sub(u.cached_content_token_count.unwrap_or(0)),
                    completion_tokens: u.candidates_token_count.unwrap_or(0) + u.thoughts_token_count.unwrap_or(0),
                    total_tokens: u.total_token_count.unwrap_or(0),
                    reasoning_tokens: u.thoughts_token_count.unwrap_or(0),
                    cached_prompt_tokens: u.cached_content_token_count.unwrap_or(0),
            cache_creation_prompt_tokens: 0,
                });
            }
            if let Some(c) = chunk.candidates.and_then(|mut v| v.drain(..).next()) {
                if let Some(fr) = c.finish_reason { finish = Some(map_finish(&fr)); }
                if let Some(content) = c.content {
                    for part in content.parts {
                        if let Some(t) = part.text {
                            if !first {
                                first = true;
                                yield SamplingEvent::FirstToken { request_id: request_id.clone() };
                            }
                            chunk_index += 1;
                            chunk_timestamps.push(Instant::now());
                            if part.thought == Some(true) {
                                reasoning_acc.push_str(&t);
                                native_parts.push(GoogleNativePart::Thinking { text: Arc::from(t.clone()), thought_signature: part.thought_signature.clone() });
                                yield SamplingEvent::ChannelToken { request_id: request_id.clone(), channel: SamplingChannel::Reasoning, text: t, chunk_index };
                            } else {
                                msg_chunks += 1;
                                text_acc.push_str(&t);
                                native_parts.push(GoogleNativePart::Text { text: Arc::from(t.clone()), thought_signature: part.thought_signature.clone() });
                                yield SamplingEvent::ChannelToken { request_id: request_id.clone(), channel: SamplingChannel::Text, text: t, chunk_index };
                            }
                        }
                        if let Some(fc) = part.function_call {
                            let id = fc.id.unwrap_or_else(|| format!("{}_{}", fc.name, calls.len() + 1));
                            let args = fc.args;
                            let arg_s = args.to_string();
                            native_parts.push(GoogleNativePart::ToolCall { id: Arc::from(id.clone()), name: fc.name.clone(), arguments: args, thought_signature: part.thought_signature.clone() });
                            calls.push(ToolCall { id: Arc::from(id.clone()), name: fc.name.clone(), arguments: Arc::from(arg_s.clone()) });
                            yield SamplingEvent::ToolCallDelta { request_id: request_id.clone(), tool_index: (calls.len() - 1) as u32, id: Some(id), name: Some(fc.name), arguments_delta: Some(arg_s) };
                        }
                    }
                }
            }
        }
        if !calls.is_empty() { finish = Some(StopReason::ToolCalls); }
        let mut items = Vec::new();
        if !reasoning_acc.is_empty() {
            items.push(ConversationItem::Reasoning(xai_grok_sampling_types::synthesized_reasoning_item(reasoning_acc)));
        }
        let _response_id = response_id;
        items.push(ConversationItem::Assistant(AssistantItem {
            content: Arc::from(text_acc),
            provider_native_state: Some(ProviderNativeAssistantState::GoogleGenerateContent { parts: native_parts }),
            tool_calls: calls,
            model_id: Some(identity.model_id().to_string()),
            reasoning_model_identity: Some(identity),
            model_fingerprint: None,
            reasoning_effort: None,
        }));
        yield SamplingEvent::Completed { request_id, response: Box::new(ConversationResponse { items, stop_reason: finish, usage, cost_usd_ticks: None, message_chunks_emitted: msg_chunks, doom_loop_signals: Vec::new(), stop_message: None, message_id: None, raw_stop_reason: None, stop_sequence: None }), metrics: InferenceLatencyStats::from_timestamps(start, &chunk_timestamps, Instant::now()) };
    }
}

fn map_finish(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::ContentFilter,
    }
}

pub fn decode_sse_response(data: &str) -> Result<GenerateContentResponse> {
    serde_json::from_str(data).map_err(SamplingError::Serialization)
}

pub fn sse_stream(
    response: reqwest::Response,
) -> BoxStream<'static, Result<GenerateContentResponse>> {
    response
        .bytes_stream()
        .eventsource()
        .filter_map(|event| async move {
            match event {
                Ok(event) if event.data == "[DONE]" => None,
                Ok(event) => Some(decode_sse_response(&event.data)),
                Err(e) => Some(Err(SamplingError::EventStreamError(e.to_string()))),
            }
        })
        .boxed()
}

#[derive(Debug)]
pub struct VertexAdcTokenProvider {
    inner: tokio::sync::OnceCell<Arc<dyn token_source::TokenSource>>,
}

impl VertexAdcTokenProvider {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::OnceCell::new(),
        }
    }

    pub async fn token(&self) -> Result<String> {
        let source = self
            .inner
            .get_or_try_init(|| async {
                let scopes = [GOOGLE_SCOPE];
                let config = gcloud_auth::project::Config::default().with_scopes(&scopes);
                let provider = gcloud_auth::token::DefaultTokenSourceProvider::new(config)
                    .await
                    .map_err(|e| {
                        SamplingError::Auth(format!("failed to initialize Google ADC: {e}"))
                    })?;
                Ok::<Arc<dyn token_source::TokenSource>, SamplingError>(provider.token_source())
            })
            .await?;
        source
            .token()
            .await
            .map_err(|e| SamplingError::Auth(format!("failed to mint Google ADC token: {e}")))
    }
}

impl Default for VertexAdcTokenProvider {
    fn default() -> Self {
        Self::new()
    }
}

pub fn apply_google_auth_headers(
    headers: &mut HeaderMap,
    api_key: Option<&str>,
    bearer: Option<&str>,
) {
    headers.remove(AUTHORIZATION);
    headers.remove(HeaderName::from_static("x-goog-api-key"));
    if let Some(key) = api_key.filter(|s| !s.trim().is_empty()) {
        if let Ok(v) = HeaderValue::from_str(key) {
            headers.insert(HeaderName::from_static("x-goog-api-key"), v);
        }
    } else if let Some(bearer) = bearer
        && let Ok(v) = HeaderValue::from_str(&format!("Bearer {bearer}"))
    {
        headers.insert(AUTHORIZATION, v);
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::{ConversationItem, ReasoningEffort};

    #[test]
    fn builds_google_vertex_adc_and_vertex_express_urls_without_query_keys() {
        let google = GoogleEndpoint {
            kind: GoogleEndpointKind::GenerativeLanguage,
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            model: "gemini-2.5-flash".into(),
            project: None,
            location: None,
        };
        assert_eq!(
            google.url(true, false).unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        let vertex = GoogleEndpoint {
            kind: GoogleEndpointKind::Vertex,
            base_url: "https://us-central1-aiplatform.googleapis.com".into(),
            model: "gemini-2.5-pro".into(),
            project: Some("p".into()),
            location: Some("us-central1".into()),
        };
        assert_eq!(
            vertex.url(false, false).unwrap(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent"
        );
        let express = GoogleEndpoint {
            kind: GoogleEndpointKind::Vertex,
            base_url: "https://{GOOGLE_CLOUD_LOCATION}-aiplatform.googleapis.com".into(),
            model: "gemini-3-flash-preview".into(),
            project: None,
            location: None,
        };
        assert_eq!(
            express.url(true, true).unwrap(),
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn converts_system_text_image_tools_tool_results_and_schema() {
        let req = ConversationRequest::from_items(vec![
            ConversationItem::system("sys"),
            ConversationItem::User(xai_grok_sampling_types::UserItem {
                content: vec![
                    ContentPart::Text {
                        text: Arc::from("hi"),
                    },
                    ContentPart::Image {
                        url: Arc::from("data:image/png;base64,AAAA"),
                    },
                ],
                synthetic_reason: None,
                cwd_generation: None,
                prior_turn_interrupt: None,
                prompt_index: None,
            }),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: Arc::from("call_1"),
                name: "read_file".into(),
                arguments: Arc::from("{\"path\":\"a\"}"),
            }]),
            ConversationItem::tool_result("call_1", "ok"),
        ])
        .with_tools(vec![xai_grok_sampling_types::ToolSpec {
            name: "read_file".into(),
            description: Some("read".into()),
            parameters: json!({"type":"object"}),
        }]);
        let mut req = req;
        req.json_schema = Some(json!({ "type": "object" }));
        let body = build_request(&req, "gemini-2.5-flash", None);
        assert_eq!(
            body.system_instruction.unwrap().parts[0].text.as_deref(),
            Some("sys")
        );
        assert!(
            body.contents[0]
                .parts
                .iter()
                .any(|p| p.inline_data.is_some())
        );
        assert!(
            body.contents
                .iter()
                .any(|c| c.parts.iter().any(|p| p.function_response.is_some()))
        );
        assert_eq!(
            body.tools.unwrap()[0].function_declarations[0].name,
            "read_file"
        );
        let generation_config = body.generation_config.unwrap();
        assert_eq!(generation_config["responseMimeType"], "application/json");
        assert_eq!(generation_config["responseJsonSchema"]["type"], "object");
    }

    #[test]
    fn tool_choice_function_sets_allowed_function_names_and_strict_validated() {
        let mut req = ConversationRequest::from_items(vec![ConversationItem::user("hi")])
            .with_tools(vec![
                xai_grok_sampling_types::ToolSpec {
                    name: "read_file".into(),
                    description: None,
                    parameters: json!({"type":"object", "strict": true}),
                },
                xai_grok_sampling_types::ToolSpec {
                    name: "write".into(),
                    description: None,
                    parameters: json!({"type":"object"}),
                },
            ]);
        req.tool_choice = Some(ConversationToolChoice::Function("read_file".into()));
        let compat = RequestCompat::GoogleGenerateContent(
            xai_grok_sampling_types::GoogleGenerateContentCompat {
                supports_strict_tool_sampling: true,
                thinking_level_map: Default::default(),
                thinking_budgets: Default::default(),
            },
        );
        let body = build_request(&req, "gemini-3-pro-preview", Some(&compat));
        assert_eq!(
            body.tool_config.as_ref().unwrap()["functionCallingConfig"]["mode"],
            "ANY"
        );
        assert_eq!(
            body.tool_config.unwrap()["functionCallingConfig"]["allowedFunctionNames"][0],
            "read_file"
        );

        let mut auto = req;
        auto.tool_choice = None;
        let body = build_request(&auto, "gemini-3-pro-preview", Some(&compat));
        assert_eq!(
            body.tool_config.unwrap()["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
    }

    #[test]
    fn replays_valid_signature_only_for_same_identity() {
        let identity = ReasoningModelIdentity::new(
            "gemini-3-pro-preview",
            xai_grok_sampling_types::ApiBackend::GoogleGenerateContent,
            "https://generativelanguage.googleapis.com/v1beta",
        );
        let assistant = AssistantItem {
            content: Arc::from("portable"),
            provider_native_state: Some(ProviderNativeAssistantState::GoogleGenerateContent {
                parts: vec![GoogleNativePart::Thinking {
                    text: Arc::from("think"),
                    thought_signature: Some("QUJDRA==".into()),
                }],
            }),
            tool_calls: vec![],
            model_id: Some("gemini-3-pro-preview".into()),
            reasoning_model_identity: Some(identity.clone()),
            model_fingerprint: None,
            reasoning_effort: None,
        };
        let req = ConversationRequest::from_items(vec![ConversationItem::Assistant(assistant)])
            .with_model("gemini-3-pro-preview")
            .with_reasoning_model_identity(identity.clone());
        let body = build_request(&req, "gemini-3-pro-preview", None);
        assert_eq!(
            body.contents[0].parts[0].thought_signature.as_deref(),
            Some("QUJDRA==")
        );
        let other = ReasoningModelIdentity::new(
            "gemini-3-pro-preview",
            xai_grok_sampling_types::ApiBackend::GoogleGenerateContent,
            "https://other.example",
        );
        let body = build_request(
            &req.with_reasoning_model_identity(other),
            "gemini-3-pro-preview",
            None,
        );
        assert!(body.contents[0].parts[0].thought_signature.is_none());
    }

    #[test]
    fn usage_maps_cache_reasoning_tokens_and_keeps_model_id_not_response_id() {
        let raw = futures_util::stream::iter(vec![Ok(GenerateContentResponse {
            response_id: Some("response-123".into()),
            candidates: Some(vec![GoogleCandidate {
                content: Some(GoogleContent {
                    role: Some("model".into()),
                    parts: vec![GooglePart {
                        text: Some("hello".into()),
                        ..Default::default()
                    }],
                }),
                finish_reason: Some("STOP".into()),
            }]),
            usage_metadata: Some(GoogleUsageMetadata {
                prompt_token_count: Some(10),
                candidates_token_count: Some(3),
                thoughts_token_count: Some(2),
                cached_content_token_count: Some(4),
                total_token_count: Some(15),
            }),
            prompt_feedback: None,
        })])
        .boxed();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let events = rt.block_on(async {
            crate::stream::collect_response(stream_google_generate_content(
                raw,
                RequestId::from("r"),
                ReasoningModelIdentity::new(
                    "gemini-2.5-flash",
                    xai_grok_sampling_types::ApiBackend::GoogleGenerateContent,
                    "https://g",
                ),
                Duration::from_secs(30),
            ))
            .await
            .unwrap()
            .0
        });
        let usage = events.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 6);
        assert_eq!(usage.cached_prompt_tokens, 4);
        assert_eq!(usage.reasoning_tokens, 2);
        assert_eq!(usage.completion_tokens, 5);
        let ConversationItem::Assistant(assistant) = events.items.last().unwrap() else {
            panic!("expected assistant")
        };
        assert_eq!(assistant.model_id.as_deref(), Some("gemini-2.5-flash"));
    }

    #[test]
    fn thinking_level_from_pi_compat_wins() {
        let mut req = ConversationRequest::from_items(vec![ConversationItem::user("hi")]);
        req.reasoning_effort = Some(ReasoningEffort::High);
        let compat = RequestCompat::GoogleGenerateContent(
            xai_grok_sampling_types::GoogleGenerateContentCompat {
                supports_strict_tool_sampling: true,
                thinking_level_map: [("high".to_string(), Some("HIGH".to_string()))]
                    .into_iter()
                    .collect(),
                thinking_budgets: Default::default(),
            },
        );
        let body = build_request(&req, "gemini-3-pro-preview", Some(&compat));
        assert_eq!(
            body.generation_config.unwrap()["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
    }
}
