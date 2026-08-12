//! Native Amazon Bedrock ConverseStream adapter.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_stream::stream;
use aws_credential_types::Credentials;
use aws_sdk_bedrockruntime::config::Token;
use aws_sdk_bedrockruntime::types as br;
use aws_smithy_runtime_api::client::auth::AuthSchemeId;
use aws_smithy_types::{Blob, Document};
use base64::Engine as _;
use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};
use serde_json::Value;

use xai_grok_sampling_types::{
    AssistantItem, BedrockNativeBlock, ContentPart, ConversationItem, ConversationRequest,
    ConversationResponse, ConversationToolChoice, ProviderNativeAssistantState,
    ReasoningModelIdentity, Result, SamplingError, StopReason, TokenUsage, ToolCall,
};

use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BedrockAuthMode {
    Bearer(String),
    Skip,
    AwsDefaultChain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BedrockEndpointConfig {
    pub region: Option<String>,
    pub endpoint_url: Option<String>,
    pub auth_mode: BedrockAuthMode,
    pub profile: Option<String>,
}

pub fn region_from_model_arn(model: &str) -> Option<String> {
    let mut parts = model.split(':');
    let is_arn = parts.next() == Some("arn");
    let partition = parts.next().unwrap_or_default();
    let service = parts.next().unwrap_or_default();
    (is_arn && partition.starts_with("aws") && service == "bedrock")
        .then(|| parts.next().map(str::to_string))
        .flatten()
        .filter(|s| !s.trim().is_empty())
}

pub fn infer_region_from_endpoint(endpoint: &str) -> Option<String> {
    let host = reqwest::Url::parse(endpoint)
        .ok()?
        .host_str()?
        .to_ascii_lowercase();
    let prefix = if host.starts_with("bedrock-runtime-fips.") {
        "bedrock-runtime-fips."
    } else if host.starts_with("bedrock-runtime.") {
        "bedrock-runtime."
    } else {
        return None;
    };
    let rest = &host[prefix.len()..];
    let region = rest
        .strip_suffix(".amazonaws.com.cn")
        .or_else(|| rest.strip_suffix(".amazonaws.com"))?;
    (!region.is_empty()).then(|| region.to_string())
}

pub fn should_use_explicit_bedrock_endpoint(
    base_url: &str,
    configured_region: Option<&str>,
    has_profile: bool,
) -> bool {
    infer_region_from_endpoint(base_url).is_none() || (configured_region.is_none() && !has_profile)
}

pub fn resolve_endpoint_config(
    model: &str,
    base_url: &str,
    explicit_bearer: Option<&str>,
    explicit_profile: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> BedrockEndpointConfig {
    let trim_env = |name: &str| {
        env(name)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let skip_auth = trim_env("AWS_BEDROCK_SKIP_AUTH").as_deref() == Some("1");
    let auth_mode = if let Some(token) = explicit_bearer.map(str::trim).filter(|s| !s.is_empty()) {
        if skip_auth {
            BedrockAuthMode::Skip
        } else {
            BedrockAuthMode::Bearer(token.to_string())
        }
    } else if let Some(token) = trim_env("AWS_BEARER_TOKEN_BEDROCK") {
        if skip_auth {
            BedrockAuthMode::Skip
        } else {
            BedrockAuthMode::Bearer(token)
        }
    } else if skip_auth {
        BedrockAuthMode::Skip
    } else {
        BedrockAuthMode::AwsDefaultChain
    };

    let arn_region = region_from_model_arn(model);
    let configured_region = trim_env("AWS_REGION").or_else(|| trim_env("AWS_DEFAULT_REGION"));
    let profile = explicit_profile
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(str::to_string);
    let has_profile = profile.is_some() || trim_env("AWS_PROFILE").is_some();
    // A region embedded in an inference-profile ARN is just as authoritative
    // as an explicit region. Do not pin a conflicting catalog endpoint in that
    // case; custom endpoints remain explicit because they have no standard
    // Bedrock hostname to infer from.
    let endpoint_url = (!base_url.trim().is_empty()
        && should_use_explicit_bedrock_endpoint(
            base_url,
            arn_region.as_deref().or(configured_region.as_deref()),
            has_profile,
        ))
    .then(|| base_url.trim_end_matches('/').to_string());
    let region = arn_region
        .or(configured_region)
        .or_else(|| endpoint_url.as_deref().and_then(infer_region_from_endpoint))
        .or_else(|| (!has_profile).then(|| "us-east-1".to_string()));
    BedrockEndpointConfig {
        region,
        endpoint_url,
        auth_mode,
        profile,
    }
}

pub async fn client_from_config(config: &BedrockEndpointConfig) -> aws_sdk_bedrockruntime::Client {
    let http_client = aws_smithy_http_client::Builder::new().build_http();
    let mut loader =
        aws_config::defaults(aws_config::BehaviorVersion::latest()).http_client(http_client);
    if let Some(profile) = &config.profile {
        loader = loader.profile_name(profile.clone());
    }
    if let Some(region) = &config.region {
        loader = loader.region(aws_config::Region::new(region.clone()));
    }
    if let Some(endpoint) = &config.endpoint_url {
        loader = loader.endpoint_url(endpoint.clone());
    }
    let shared = loader.load().await;
    let mut builder = aws_sdk_bedrockruntime::config::Builder::from(&shared);
    match &config.auth_mode {
        BedrockAuthMode::Bearer(token) => {
            builder = builder
                .bearer_token(Token::new(token.clone(), None))
                .auth_scheme_preference([AuthSchemeId::from("httpBearerAuth")]);
        }
        BedrockAuthMode::Skip => {
            builder = builder.credentials_provider(Credentials::new(
                "dummy-access-key",
                "dummy-secret-key",
                None,
                None,
                "bedrock-skip-auth",
            ));
        }
        BedrockAuthMode::AwsDefaultChain => {}
    }
    aws_sdk_bedrockruntime::Client::from_conf(builder.build())
}

#[derive(Debug)]
pub struct BedrockRequestParts {
    pub system: Vec<br::SystemContentBlock>,
    pub messages: Vec<br::Message>,
    pub tool_config: Option<br::ToolConfiguration>,
    pub inference_config: Option<br::InferenceConfiguration>,
    pub additional_model_request_fields: Option<Document>,
    pub request_metadata: HashMap<String, String>,
    pub custom_headers: HashMap<String, String>,
}

pub fn build_request(req: &ConversationRequest) -> Result<BedrockRequestParts> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    let mut pending_tool_results: Vec<br::ContentBlock> = Vec::new();
    let tool_id_map = build_tool_id_map(req);

    for item in &req.items {
        match item {
            ConversationItem::System(s) => {
                system.push(br::SystemContentBlock::Text(s.content.to_string()))
            }
            ConversationItem::User(u) => {
                flush_tool_results(&mut pending_tool_results, &mut messages)?;
                let mut content = Vec::new();
                for part in &u.content {
                    content.push(content_part_to_block(part)?);
                }
                if content.is_empty() {
                    content.push(br::ContentBlock::Text(EMPTY_TEXT_PLACEHOLDER.to_string()));
                }
                messages.push(message(br::ConversationRole::User, content)?);
            }
            ConversationItem::Assistant(a) => {
                flush_tool_results(&mut pending_tool_results, &mut messages)?;
                let mut content = Vec::new();
                let can_replay_native = a
                    .reasoning_model_identity
                    .as_ref()
                    .zip(req.reasoning_model_identity.as_ref())
                    .is_some_and(|(source, target)| source == target);
                if can_replay_native
                    && let Some(ProviderNativeAssistantState::BedrockConverseStream { blocks }) =
                        &a.provider_native_state
                {
                    for block in blocks {
                        match block {
                            BedrockNativeBlock::Text { text } => {
                                content.push(br::ContentBlock::Text(text.to_string()))
                            }
                            BedrockNativeBlock::Reasoning { text, signature } => {
                                push_reasoning_or_text(
                                    &mut content,
                                    req,
                                    text,
                                    signature.as_deref(),
                                )?;
                            }
                            BedrockNativeBlock::ToolUse { id, name, input } => {
                                content.push(br::ContentBlock::ToolUse(tool_use_block(
                                    &mapped_tool_id(&tool_id_map, id),
                                    name,
                                    input.clone(),
                                )?))
                            }
                        }
                    }
                } else {
                    if !a.content.is_empty() {
                        content.push(br::ContentBlock::Text(a.content.to_string()));
                    }
                    for tc in &a.tool_calls {
                        let input =
                            serde_json::from_str::<Value>(&tc.arguments).unwrap_or(Value::Null);
                        content.push(br::ContentBlock::ToolUse(tool_use_block(
                            &mapped_tool_id(&tool_id_map, &tc.id),
                            &tc.name,
                            input,
                        )?));
                    }
                }
                if !content.is_empty() {
                    messages.push(message(br::ConversationRole::Assistant, content)?);
                }
            }
            ConversationItem::ToolResult(t) => {
                let mut content = Vec::new();
                if let Some(text) = non_blank_text(&t.content) {
                    content.push(br::ToolResultContentBlock::Text(text));
                }
                for image in &t.images {
                    content.push(content_part_to_tool_result_block(image)?);
                }
                if content.is_empty() {
                    content.push(br::ToolResultContentBlock::Text(
                        EMPTY_TEXT_PLACEHOLDER.to_string(),
                    ));
                }
                let block = br::ToolResultBlock::builder()
                    .tool_use_id(mapped_tool_id(&tool_id_map, &t.tool_call_id))
                    .set_content(Some(content))
                    .build()
                    .map_err(|e| SamplingError::EventStreamError(e.to_string()))?;
                pending_tool_results.push(br::ContentBlock::ToolResult(block));
            }
            _ => {}
        }
    }
    flush_tool_results(&mut pending_tool_results, &mut messages)?;

    let tool_config =
        if req.tools.is_empty() || matches!(req.tool_choice, Some(ConversationToolChoice::None)) {
            None
        } else {
            let supports_strict = req
                .request_compat
                .as_ref()
                .and_then(|compat| compat.bedrock_converse_stream())
                .is_some_and(|compat| compat.supports_strict_mode);
            let mut specs = Vec::new();
            for tool in &req.tools {
                let schema = br::ToolInputSchema::Json(value_to_document(tool.parameters.clone()));
                let mut spec = br::ToolSpecification::builder()
                    .name(tool.name.clone())
                    .set_description(tool.description.clone())
                    .input_schema(schema);
                if supports_strict && schema_requests_strict(&tool.parameters) {
                    spec = spec.strict(true);
                }
                let spec = spec
                    .build()
                    .map_err(|e| SamplingError::EventStreamError(e.to_string()))?;
                specs.push(br::Tool::ToolSpec(spec));
            }
            let mut builder = br::ToolConfiguration::builder().set_tools(Some(specs));
            if let Some(choice) = &req.tool_choice {
                let choice = match choice {
                    ConversationToolChoice::Auto => {
                        br::ToolChoice::Auto(br::AutoToolChoice::builder().build())
                    }
                    ConversationToolChoice::Required => {
                        br::ToolChoice::Any(br::AnyToolChoice::builder().build())
                    }
                    ConversationToolChoice::Function(name) => br::ToolChoice::Tool(
                        br::SpecificToolChoice::builder()
                            .name(name.clone())
                            .build()
                            .map_err(|e| SamplingError::EventStreamError(e.to_string()))?,
                    ),
                    ConversationToolChoice::None => unreachable!("None omits toolConfig"),
                };
                builder = builder.tool_choice(choice);
            }
            Some(
                builder
                    .build()
                    .map_err(|e| SamplingError::EventStreamError(e.to_string()))?,
            )
        };

    // Pi's Bedrock adapter only forwards maxTokens and temperature in the
    // standard Converse inference config. Model-specific top-p support is not
    // uniform and must not leak in from an unrelated OpenAI-style default.
    let inference = if req.max_output_tokens.is_some() || req.temperature.is_some() {
        let mut builder = br::InferenceConfiguration::builder();
        if let Some(v) = req.max_output_tokens {
            builder = builder.max_tokens(v as i32);
        }
        if let Some(v) = req.temperature {
            builder = builder.temperature(v);
        }
        Some(builder.build())
    } else {
        None
    };

    if let Some(retention) = cache_retention(req) {
        if !system.is_empty() {
            system.push(br::SystemContentBlock::CachePoint(cache_point(retention)?));
        }
        // Bedrock/Pi only caches the trailing user turn. Do not walk backwards
        // across a final assistant turn and mutate an older message.
        if let Some(last_message) = messages.last_mut()
            && last_message.role == br::ConversationRole::User
        {
            last_message
                .content
                .push(br::ContentBlock::CachePoint(cache_point(retention)?));
        }
    }

    Ok(BedrockRequestParts {
        system,
        messages,
        tool_config,
        inference_config: inference,
        additional_model_request_fields: build_additional_model_request_fields(req),
        request_metadata: validate_metadata(&req.bedrock_request_metadata)?,
        custom_headers: validate_custom_headers(&req.bedrock_headers)?,
    })
}

fn message(role: br::ConversationRole, content: Vec<br::ContentBlock>) -> Result<br::Message> {
    br::Message::builder()
        .role(role)
        .set_content(Some(content))
        .build()
        .map_err(|e| SamplingError::EventStreamError(e.to_string()))
}

fn flush_tool_results(
    pending: &mut Vec<br::ContentBlock>,
    messages: &mut Vec<br::Message>,
) -> Result<()> {
    if !pending.is_empty() {
        messages.push(message(
            br::ConversationRole::User,
            std::mem::take(pending),
        )?);
    }
    Ok(())
}

fn content_part_to_block(part: &ContentPart) -> Result<br::ContentBlock> {
    match part {
        ContentPart::Text { text } => Ok(br::ContentBlock::Text(
            non_blank_text(text).unwrap_or_else(|| EMPTY_TEXT_PLACEHOLDER.to_string()),
        )),
        ContentPart::Image { url } => Ok(br::ContentBlock::Image(create_image_block(url)?)),
    }
}

fn content_part_to_tool_result_block(part: &ContentPart) -> Result<br::ToolResultContentBlock> {
    match part {
        ContentPart::Text { text } => Ok(br::ToolResultContentBlock::Text(
            non_blank_text(text).unwrap_or_else(|| EMPTY_TEXT_PLACEHOLDER.to_string()),
        )),
        ContentPart::Image { url } => {
            Ok(br::ToolResultContentBlock::Image(create_image_block(url)?))
        }
    }
}

fn tool_use_block(id: &str, name: &str, input: Value) -> Result<br::ToolUseBlock> {
    br::ToolUseBlock::builder()
        .tool_use_id(id.to_string())
        .name(name.to_string())
        .input(value_to_document(input))
        .build()
        .map_err(|e| SamplingError::EventStreamError(e.to_string()))
}

fn build_tool_id_map(req: &ConversationRequest) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut used: HashMap<String, usize> = HashMap::new();
    for item in &req.items {
        match item {
            ConversationItem::Assistant(a) => {
                for tc in &a.tool_calls {
                    insert_tool_id(&mut map, &mut used, &tc.id);
                }
                if let Some(ProviderNativeAssistantState::BedrockConverseStream { blocks }) =
                    &a.provider_native_state
                {
                    for block in blocks {
                        if let BedrockNativeBlock::ToolUse { id, .. } = block {
                            insert_tool_id(&mut map, &mut used, id);
                        }
                    }
                }
            }
            ConversationItem::ToolResult(t) => insert_tool_id(&mut map, &mut used, &t.tool_call_id),
            _ => {}
        }
    }
    map
}

fn insert_tool_id(map: &mut HashMap<String, String>, used: &mut HashMap<String, usize>, id: &str) {
    if map.contains_key(id) {
        return;
    }
    let mut base: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if base.is_empty() {
        base = "toolu_0".to_string();
    }
    let count = used.entry(base.clone()).or_insert(0);
    let mapped = if *count == 0 {
        base.clone()
    } else {
        let suffix = format!("_{}", count);
        let keep = 64usize.saturating_sub(suffix.len());
        format!("{}{}", base.chars().take(keep).collect::<String>(), suffix)
    };
    *count += 1;
    map.insert(id.to_string(), mapped);
}

fn mapped_tool_id(map: &HashMap<String, String>, id: &str) -> String {
    map.get(id).cloned().unwrap_or_else(|| {
        let mut used = HashMap::new();
        let mut single = HashMap::new();
        insert_tool_id(&mut single, &mut used, id);
        single.remove(id).unwrap()
    })
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

fn non_blank_text(text: &str) -> Option<String> {
    let sanitized = text.chars().filter(|c| *c != '\u{0}').collect::<String>();
    (!sanitized.trim().is_empty()).then_some(sanitized)
}

fn push_reasoning_or_text(
    content: &mut Vec<br::ContentBlock>,
    req: &ConversationRequest,
    text: &str,
    signature: Option<&str>,
) -> Result<()> {
    let Some(thinking) = non_blank_text(text) else {
        return Ok(());
    };
    let model = req.model.as_deref().unwrap_or_default();
    if is_claude_model(model) {
        if let Some(sig) = signature.filter(|s| !s.trim().is_empty()) {
            let reasoning = br::ReasoningTextBlock::builder()
                .text(thinking)
                .signature(sig.to_string())
                .build()
                .map_err(|e| SamplingError::EventStreamError(e.to_string()))?;
            content.push(br::ContentBlock::ReasoningContent(
                br::ReasoningContentBlock::ReasoningText(reasoning),
            ));
        } else {
            content.push(br::ContentBlock::Text(thinking));
        }
    } else {
        let reasoning = br::ReasoningTextBlock::builder()
            .text(thinking)
            .build()
            .map_err(|e| SamplingError::EventStreamError(e.to_string()))?;
        content.push(br::ContentBlock::ReasoningContent(
            br::ReasoningContentBlock::ReasoningText(reasoning),
        ));
    }
    Ok(())
}

fn create_image_block(url: &str) -> Result<br::ImageBlock> {
    let Some(rest) = url.strip_prefix("data:") else {
        return Err(SamplingError::InvalidConfiguration(
            "Bedrock requires image data URIs",
        ));
    };
    let (meta, data) = rest
        .split_once(',')
        .ok_or(SamplingError::InvalidConfiguration(
            "Invalid image data URI",
        ))?;
    let mut meta_parts = meta.split(';');
    let mime = meta_parts.next().unwrap_or_default().to_ascii_lowercase();
    if !meta_parts.any(|p| p.eq_ignore_ascii_case("base64")) {
        return Err(SamplingError::InvalidConfiguration(
            "Bedrock image data URI must be base64",
        ));
    }
    let format = match mime.as_str() {
        "image/jpeg" | "image/jpg" => br::ImageFormat::Jpeg,
        "image/png" => br::ImageFormat::Png,
        "image/gif" => br::ImageFormat::Gif,
        "image/webp" => br::ImageFormat::Webp,
        _ => {
            return Err(SamplingError::InvalidConfiguration(
                "Unsupported Bedrock image MIME type",
            ));
        }
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| SamplingError::EventStreamError(format!("invalid image base64: {e}")))?;
    br::ImageBlock::builder()
        .format(format)
        .source(br::ImageSource::Bytes(Blob::new(bytes)))
        .build()
        .map_err(|e| SamplingError::EventStreamError(e.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BedrockCacheRetention {
    Short,
    Long,
}

fn cache_point(retention: BedrockCacheRetention) -> Result<br::CachePointBlock> {
    let mut builder = br::CachePointBlock::builder().r#type(br::CachePointType::Default);
    if retention == BedrockCacheRetention::Long {
        builder = builder.ttl(br::CacheTtl::OneHour);
    }
    builder
        .build()
        .map_err(|e| SamplingError::EventStreamError(e.to_string()))
}

fn cache_retention(req: &ConversationRequest) -> Option<BedrockCacheRetention> {
    let configured = req
        .prompt_cache_retention
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| {
            if std::env::var("PI_CACHE_RETENTION")
                .ok()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("long"))
            {
                "long".to_string()
            } else {
                "short".to_string()
            }
        });
    let retention = match configured.as_str() {
        "none" | "off" | "disabled" => return None,
        "long" | "1h" | "one_hour" | "one-hour" | "24h" => BedrockCacheRetention::Long,
        _ => BedrockCacheRetention::Short,
    };
    (std::env::var("AWS_BEDROCK_FORCE_CACHE").ok().as_deref() == Some("1")
        || is_cache_capable(req.model.as_deref().unwrap_or_default()))
    .then_some(retention)
}

fn is_cache_capable(model: &str) -> bool {
    let s = model.to_ascii_lowercase().replace(['_', '.', ':'], "-");
    (s.contains("claude")
        && (s.contains("-4-")
            || s.contains("claude-3-7-sonnet")
            || s.contains("claude-3-5-haiku")
            || s.contains("fable-5")
            || s.contains("opus-5")
            || s.contains("sonnet-5")))
        || std::env::var("AWS_BEDROCK_FORCE_CACHE").ok().as_deref() == Some("1")
}

fn is_claude_model(model: &str) -> bool {
    let s = model.to_ascii_lowercase();
    s.contains("anthropic.claude") || s.contains("anthropic/claude") || s.contains("claude")
}

fn build_additional_model_request_fields(req: &ConversationRequest) -> Option<Document> {
    let effort = req.reasoning_effort?;
    if effort.as_str() == "none" {
        return None;
    }
    let model = req.model.as_deref().unwrap_or_default();
    if !is_claude_model(model) {
        return None;
    }
    let level = effort.as_str();
    let mapped = req
        .request_compat
        .as_ref()
        .and_then(|c| c.bedrock_converse_stream())
        .and_then(|c| c.thinking_level_map.get(level))
        .and_then(|v| v.clone())
        .unwrap_or_else(|| {
            match level {
                "minimal" | "low" => "low",
                "medium" => "medium",
                "xhigh" | "max" | "ultra" => "high",
                _ => "high",
            }
            .to_string()
        });
    let display = (!is_govcloud_target(model)).then_some("summarized");
    let mut root = serde_json::Map::new();
    if supports_adaptive_thinking(model) {
        root.insert(
            "thinking".into(),
            serde_json::json!({
                "type": "adaptive",
                "display": display,
            }),
        );
        if display.is_none()
            && let Some(thinking) = root.get_mut("thinking").and_then(Value::as_object_mut)
        {
            thinking.remove("display");
        }
        root.insert(
            "output_config".into(),
            serde_json::json!({"effort": mapped}),
        );
    } else {
        let budget = match level {
            "minimal" => 1024,
            "low" => 2048,
            "medium" => 8192,
            _ => 16384,
        };
        root.insert(
            "thinking".into(),
            serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget,
                "display": display,
            }),
        );
        if display.is_none()
            && let Some(thinking) = root.get_mut("thinking").and_then(Value::as_object_mut)
        {
            thinking.remove("display");
        }
        root.insert(
            "anthropic_beta".into(),
            serde_json::json!(["interleaved-thinking-2025-05-14"]),
        );
    }
    Some(value_to_document(Value::Object(root)))
}

fn supports_adaptive_thinking(model: &str) -> bool {
    let s = model.to_ascii_lowercase().replace(['_', '.', ':'], "-");
    [
        "opus-4-6",
        "opus-4-7",
        "opus-4-8",
        "opus-5",
        "sonnet-4-6",
        "sonnet-5",
        "fable-5",
    ]
    .iter()
    .any(|needle| s.contains(needle))
}

fn is_govcloud_target(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("us-gov.")
        || model.starts_with("arn:aws-us-gov:")
        || std::env::var("AWS_REGION")
            .ok()
            .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
            .is_some_and(|region| region.trim().to_ascii_lowercase().starts_with("us-gov-"))
}

fn validate_metadata(
    input: &indexmap::IndexMap<String, String>,
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    if input.len() > 50 {
        return Err(SamplingError::InvalidConfiguration(
            "Bedrock request metadata supports at most 50 pairs",
        ));
    }
    for (k, v) in input {
        let key = k.trim();
        let value = v.trim();
        if key.is_empty()
            || key.len() > 64
            || key.to_ascii_lowercase().starts_with("aws:")
            || value.len() > 256
        {
            return Err(SamplingError::InvalidConfiguration(
                "Invalid Bedrock request metadata",
            ));
        }
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

fn validate_custom_headers(
    input: &indexmap::IndexMap<String, String>,
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (k, v) in input {
        let lower = k.to_ascii_lowercase();
        if lower == "authorization" || lower == "host" || lower.starts_with("x-amz-") {
            return Err(SamplingError::InvalidConfiguration(
                "Reserved Bedrock header cannot be overridden",
            ));
        }
        out.insert(k.clone(), v.clone());
    }
    Ok(out)
}

fn value_to_document(value: Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(v) => Document::Bool(v),
        Value::Number(n) => n
            .as_i64()
            .map(Document::from)
            .or_else(|| n.as_u64().map(Document::from))
            .or_else(|| n.as_f64().map(Document::from))
            .unwrap_or(Document::Null),
        Value::String(s) => Document::String(s),
        Value::Array(values) => {
            Document::Array(values.into_iter().map(value_to_document).collect())
        }
        Value::Object(map) => Document::Object(
            map.into_iter()
                .map(|(k, v)| (k, value_to_document(v)))
                .collect::<HashMap<_, _>>(),
        ),
    }
}

pub async fn converse_stream(
    client: aws_sdk_bedrockruntime::Client,
    model: String,
    request: ConversationRequest,
) -> Result<BoxStream<'static, Result<br::ConverseStreamOutput>>> {
    let parts = build_request(&request)?;
    let mut op = client
        .converse_stream()
        .model_id(model)
        .set_messages(Some(parts.messages));
    if !parts.system.is_empty() {
        op = op.set_system(Some(parts.system));
    }
    if let Some(tool_config) = parts.tool_config {
        op = op.tool_config(tool_config);
    }
    if let Some(inference_config) = parts.inference_config {
        op = op.inference_config(inference_config);
    }
    if let Some(additional) = parts.additional_model_request_fields {
        op = op.additional_model_request_fields(additional);
    }
    for (k, v) in parts.request_metadata {
        op = op.request_metadata(k, v);
    }
    let custom_headers = parts.custom_headers;
    let output = op
        .customize()
        .mutate_request(move |req| {
            for (k, v) in &custom_headers {
                req.headers_mut().insert(k.clone(), v.clone());
            }
        })
        .send()
        .await
        .map_err(|e| SamplingError::Api {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            message: e.to_string(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
        })?;
    let mut receiver = output.stream;
    Ok(stream! {
        loop {
            match receiver.recv().await {
                Ok(Some(event)) => yield Ok(event),
                Ok(None) => break,
                Err(err) => { yield Err(SamplingError::EventStreamError(err.to_string())); break; }
            }
        }
    }
    .boxed())
}

#[derive(Debug, Default, Clone)]
struct StreamBlock {
    text: String,
    reasoning: String,
    signature: String,
    tool_id: Option<String>,
    tool_name: Option<String>,
    tool_args: String,
    kind: Option<StreamBlockKind>,
    stopped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamBlockKind {
    Text,
    Reasoning,
    Tool,
}

pub fn stream_bedrock_converse(
    raw: BoxStream<'static, Result<br::ConverseStreamOutput>>,
    request_id: RequestId,
    identity: ReasoningModelIdentity,
    idle_timeout: Duration,
) -> impl Stream<Item = SamplingEvent> {
    stream! {
        let start = Instant::now();
        let mut first_token_at: Option<Instant> = None;
        yield SamplingEvent::StreamStarted { request_id: request_id.clone(), timestamp_ms: chrono::Utc::now().timestamp_millis() };
        let mut raw = raw;
        let mut blocks: HashMap<i32, StreamBlock> = HashMap::new();
        let mut usage: Option<TokenUsage> = None;
        let mut stop: Option<StopReason> = None;
        let mut text_chunks = 0_u64;
        let mut content_chunks = 0_u64;

        loop {
            let next = tokio::time::timeout(idle_timeout, raw.next()).await;
            let Some(item) = (match next {
                Ok(v) => v,
                Err(_) => {
                    yield SamplingEvent::Failed { request_id: request_id.clone(), error: SamplingErrorInfo::from(&SamplingError::IdleTimeout { elapsed_secs: idle_timeout.as_secs() }) };
                    return;
                }
            }) else { break; };
            let event = match item {
                Ok(event) => event,
                Err(err) => {
                    yield SamplingEvent::Failed { request_id: request_id.clone(), error: SamplingErrorInfo::from(&err) };
                    return;
                }
            };
            match event {
                br::ConverseStreamOutput::MessageStart(e) => {
                    if e.role != br::ConversationRole::Assistant {
                        let err = SamplingError::EventStreamError(
                            "Bedrock stream started with a non-assistant role".to_string(),
                        );
                        yield SamplingEvent::Failed {
                            request_id: request_id.clone(),
                            error: SamplingErrorInfo::from(&err),
                        };
                        return;
                    }
                }
                br::ConverseStreamOutput::ContentBlockStart(e) => if let Some(br::ContentBlockStart::ToolUse(t)) = e.start {
                    let block = blocks.entry(e.content_block_index).or_default();
                    block.kind = Some(StreamBlockKind::Tool);
                    block.tool_id = Some(t.tool_use_id.clone());
                    block.tool_name = Some(t.name.clone());
                    if first_token_at.is_none() {
                        first_token_at = Some(Instant::now());
                        yield SamplingEvent::FirstToken { request_id: request_id.clone() };
                    }
                    yield SamplingEvent::ToolCallDelta { request_id: request_id.clone(), tool_index: e.content_block_index.max(0) as u32, id: Some(t.tool_use_id), name: Some(t.name), arguments_delta: Some(String::new()) };
                },
                br::ConverseStreamOutput::ContentBlockDelta(e) => if let Some(delta) = e.delta {
                    match delta {
                        br::ContentBlockDelta::Text(t) => {
                            let block = blocks.entry(e.content_block_index).or_default();
                            block.kind.get_or_insert(StreamBlockKind::Text);
                            block.text.push_str(&t);
                            if first_token_at.is_none() {
                                first_token_at = Some(Instant::now());
                                yield SamplingEvent::FirstToken { request_id: request_id.clone() };
                            }
                            text_chunks += 1;
                            content_chunks += 1;
                            yield SamplingEvent::ChannelToken { request_id: request_id.clone(), channel: SamplingChannel::Text, text: t, chunk_index: content_chunks };
                        }
                        br::ContentBlockDelta::ReasoningContent(r) => {
                            let block = blocks.entry(e.content_block_index).or_default();
                            block.kind.get_or_insert(StreamBlockKind::Reasoning);
                            match r {
                                br::ReasoningContentBlockDelta::Text(t) => {
                                    block.reasoning.push_str(&t);
                                    if first_token_at.is_none() {
                                        first_token_at = Some(Instant::now());
                                        yield SamplingEvent::FirstToken { request_id: request_id.clone() };
                                    }
                                    content_chunks += 1;
                                    yield SamplingEvent::ChannelToken { request_id: request_id.clone(), channel: SamplingChannel::Reasoning, text: t, chunk_index: content_chunks };
                                }
                                br::ReasoningContentBlockDelta::Signature(s) => block.signature.push_str(&s),
                                _ => {}
                            }
                        }
                        br::ContentBlockDelta::ToolUse(t) => {
                            let block = blocks.entry(e.content_block_index).or_default();
                            block.kind = Some(StreamBlockKind::Tool);
                            block.tool_args.push_str(&t.input);
                            if first_token_at.is_none() {
                                first_token_at = Some(Instant::now());
                                yield SamplingEvent::FirstToken { request_id: request_id.clone() };
                            }
                            content_chunks += 1;
                            yield SamplingEvent::ToolCallDelta { request_id: request_id.clone(), tool_index: e.content_block_index.max(0) as u32, id: block.tool_id.clone(), name: block.tool_name.clone(), arguments_delta: Some(t.input) };
                        }
                        _ => {}
                    }
                },
                br::ConverseStreamOutput::ContentBlockStop(e) => {
                    if let Some(block) = blocks.get_mut(&e.content_block_index) {
                        block.stopped = true;
                    }
                }
                br::ConverseStreamOutput::MessageStop(e) => {
                    match map_stop_reason(e.stop_reason) {
                        Ok(reason) => stop = Some(reason),
                        Err(err) => {
                            yield SamplingEvent::Failed {
                                request_id: request_id.clone(),
                                error: SamplingErrorInfo::from(&err),
                            };
                            return;
                        }
                    }
                },
                br::ConverseStreamOutput::Metadata(e) => if let Some(u) = e.usage {
                    let input = u.input_tokens.max(0) as u32;
                    let output = u.output_tokens.max(0) as u32;
                    let cache_read = u.cache_read_input_tokens.unwrap_or_default().max(0) as u32;
                    let cache_write = u.cache_write_input_tokens.unwrap_or_default().max(0) as u32;
                    let prompt_tokens = input
                        .saturating_add(cache_read)
                        .saturating_add(cache_write);
                    usage = Some(TokenUsage {
                        prompt_tokens,
                        completion_tokens: output,
                        total_tokens: prompt_tokens.saturating_add(output),
                        reasoning_tokens: 0,
                        cached_prompt_tokens: cache_read,
                        cache_creation_prompt_tokens: cache_write,
                    });
                },
                _ => {
                    let err = SamplingError::EventStreamError(
                        "Unsupported future Bedrock ConverseStream event".to_string(),
                    );
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            }
        }
        let Some(stop_reason) = stop else {
            let err = SamplingError::EventStreamError("Bedrock stream ended without a stop reason".to_string());
            yield SamplingEvent::Failed { request_id, error: SamplingErrorInfo::from(&err) };
            return;
        };
        let mut ordered: Vec<_> = blocks.into_iter().collect();
        ordered.sort_by_key(|(idx, _)| *idx);
        let mut native = Vec::new();
        let mut tool_calls = Vec::new();
        let mut text = String::new();
        let mut reasoning_text = String::new();
        for (idx, block) in ordered {
            if !block.stopped {
                let err = SamplingError::EventStreamError(format!("Bedrock content block {idx} ended without stop"));
                yield SamplingEvent::Failed { request_id, error: SamplingErrorInfo::from(&err) };
                return;
            }
            match block.kind {
                Some(StreamBlockKind::Text) => {
                    text.push_str(&block.text);
                    native.push(BedrockNativeBlock::Text { text: Arc::from(block.text) });
                }
                Some(StreamBlockKind::Reasoning) => {
                    reasoning_text.push_str(&block.reasoning);
                    native.push(BedrockNativeBlock::Reasoning {
                        text: Arc::from(block.reasoning),
                        signature: (!block.signature.is_empty()).then_some(block.signature),
                    });
                }
                Some(StreamBlockKind::Tool) => {
                    let id = block.tool_id.unwrap_or_else(|| format!("toolu_{idx}"));
                    let name = block.tool_name.unwrap_or_else(|| "tool".to_string());
                    let args = if block.tool_args.is_empty() { "{}".to_string() } else { block.tool_args };
                    native.push(BedrockNativeBlock::ToolUse { id: Arc::from(id.clone()), name: name.clone(), input: serde_json::from_str(&args).unwrap_or(Value::Object(Default::default())) });
                    tool_calls.push(ToolCall { id: Arc::from(id), name, arguments: Arc::from(args) });
                }
                None => {}
            }
        }
        let assistant = AssistantItem {
            content: Arc::from(text),
            provider_native_state: Some(ProviderNativeAssistantState::BedrockConverseStream {
                blocks: native,
            }),
            tool_calls,
            model_id: Some(identity.model_id().to_string()),
            reasoning_model_identity: Some(identity),
            model_fingerprint: None,
            reasoning_effort: None,
        };
        let mut items = Vec::new();
        if !reasoning_text.is_empty() {
            items.push(ConversationItem::Reasoning(
                xai_grok_sampling_types::synthesized_reasoning_item(reasoning_text),
            ));
        }
        items.push(ConversationItem::Assistant(assistant));
        let response = ConversationResponse { items, stop_reason: Some(stop_reason), usage, cost_usd_ticks: None, message_chunks_emitted: text_chunks, doom_loop_signals: Vec::new(), stop_message: None, message_id: None, raw_stop_reason: None, stop_sequence: None };
        yield SamplingEvent::Completed { request_id, response: Box::new(response), metrics: InferenceLatencyStats { time_to_first_token_ms: first_token_at.map(|t| t.duration_since(start).as_millis() as u64), time_to_last_byte_ms: start.elapsed().as_millis() as u64, chunk_count: content_chunks as u32, attempts: 1, ..Default::default() } };
    }
}

fn map_stop_reason(reason: br::StopReason) -> Result<StopReason> {
    match reason.as_str() {
        "end_turn" | "stop_sequence" => Ok(StopReason::Stop),
        "max_tokens" | "model_context_window_exceeded" => Ok(StopReason::Length),
        "tool_use" => Ok(StopReason::ToolCalls),
        "content_filtered" | "guardrail_intervened" => Ok(StopReason::ContentFilter),
        unknown => Err(SamplingError::EventStreamError(format!(
            "Unknown Bedrock stop reason: {unknown}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::{SystemItem, ToolResultItem, UserItem};

    #[test]
    fn resolves_auth_region_and_endpoint_precedence() {
        let cfg = resolve_endpoint_config(
            "arn:aws:bedrock:us-west-2:123:inference-profile/foo",
            "https://bedrock-runtime.eu-central-1.amazonaws.com",
            Some("explicit"),
            None,
            |_| None,
        );
        assert_eq!(cfg.region.as_deref(), Some("us-west-2"));
        assert_eq!(
            cfg.endpoint_url, None,
            "an ARN region must not be paired with a conflicting catalog endpoint"
        );
        assert_eq!(cfg.auth_mode, BedrockAuthMode::Bearer("explicit".into()));
    }

    #[test]
    fn request_replays_native_only_for_matching_identity_and_degrades_missing_signature() {
        let identity = ReasoningModelIdentity::new(
            "anthropic.claude-sonnet-4-5",
            xai_grok_sampling_types::ApiBackend::BedrockConverseStream,
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        );
        let assistant = AssistantItem {
            content: Arc::from("portable"),
            provider_native_state: Some(ProviderNativeAssistantState::BedrockConverseStream {
                blocks: vec![BedrockNativeBlock::Reasoning {
                    text: Arc::from("think"),
                    signature: None,
                }],
            }),
            tool_calls: vec![],
            model_id: None,
            reasoning_model_identity: Some(identity.clone()),
            model_fingerprint: None,
            reasoning_effort: None,
        };
        let req = ConversationRequest {
            items: vec![ConversationItem::Assistant(assistant.clone())],
            model: Some("anthropic.claude-sonnet-4-5".into()),
            reasoning_model_identity: Some(identity),
            ..Default::default()
        };
        let parts = build_request(&req).unwrap();
        assert!(
            matches!(parts.messages[0].content[0], br::ContentBlock::Text(_)),
            "missing Claude signature degrades to text in place"
        );

        let req = ConversationRequest {
            items: vec![ConversationItem::Assistant(assistant)],
            model: Some("anthropic.claude-sonnet-4-5".into()),
            reasoning_model_identity: Some(ReasoningModelIdentity::new(
                "other",
                xai_grok_sampling_types::ApiBackend::BedrockConverseStream,
                "https://bedrock-runtime.us-east-1.amazonaws.com",
            )),
            ..Default::default()
        };
        let parts = build_request(&req).unwrap();
        assert!(
            matches!(&parts.messages[0].content[0], br::ContentBlock::Text(t) if t == "portable")
        );
    }

    #[test]
    fn image_data_uri_is_required_and_preserved() {
        let good = "data:image/png;base64,iVBORw0KGgo=";
        let req = ConversationRequest {
            items: vec![ConversationItem::User(UserItem {
                content: vec![ContentPart::Image {
                    url: Arc::from(good),
                }],
                ..Default::default()
            })],
            ..Default::default()
        };
        let parts = build_request(&req).unwrap();
        assert!(matches!(
            parts.messages[0].content[0],
            br::ContentBlock::Image(_)
        ));
        let bad = ConversationRequest {
            items: vec![ConversationItem::User(UserItem {
                content: vec![ContentPart::Image {
                    url: Arc::from("https://example.com/a.png"),
                }],
                ..Default::default()
            })],
            ..Default::default()
        };
        assert!(build_request(&bad).is_err());
    }

    #[test]
    fn profile_does_not_pin_catalog_endpoint_or_region() {
        let cfg = resolve_endpoint_config(
            "anthropic.claude-3-5-haiku",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            None,
            None,
            |name| (name == "AWS_PROFILE").then(|| "dev".to_string()),
        );
        assert_eq!(cfg.region, None);
        assert_eq!(cfg.endpoint_url, None);
        assert_eq!(cfg.profile, None);

        let cfg = resolve_endpoint_config(
            "anthropic.claude-3-5-haiku",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            None,
            Some("stored"),
            |_| None,
        );
        assert_eq!(cfg.region, None);
        assert_eq!(cfg.endpoint_url, None);
        assert_eq!(cfg.profile.as_deref(), Some("stored"));
    }

    #[test]
    fn legacy_native_state_without_route_identity_is_not_replayed() {
        let assistant = AssistantItem {
            content: Arc::from("portable"),
            provider_native_state: Some(ProviderNativeAssistantState::BedrockConverseStream {
                blocks: vec![BedrockNativeBlock::Text {
                    text: Arc::from("opaque"),
                }],
            }),
            tool_calls: vec![],
            model_id: None,
            reasoning_model_identity: None,
            model_fingerprint: None,
            reasoning_effort: None,
        };
        let req = ConversationRequest {
            items: vec![ConversationItem::Assistant(assistant)],
            model: Some("anthropic.claude-sonnet-4-5".into()),
            reasoning_model_identity: None,
            ..Default::default()
        };
        let parts = build_request(&req).unwrap();
        assert!(
            matches!(&parts.messages[0].content[0], br::ContentBlock::Text(text) if text == "portable")
        );
    }

    #[test]
    fn strict_tools_and_long_cache_use_typed_bedrock_fields() {
        let req = ConversationRequest {
            items: vec![
                ConversationItem::System(SystemItem {
                    content: Arc::from("system"),
                }),
                ConversationItem::user("hello"),
            ],
            tools: vec![xai_grok_sampling_types::ToolSpec {
                name: "read_file".into(),
                description: Some("Read a file".into()),
                parameters: serde_json::json!({"type":"object", "strict":true}),
            }],
            model: Some("anthropic.claude-sonnet-4-5".into()),
            request_compat: Some(
                xai_grok_sampling_types::RequestCompat::BedrockConverseStream(
                    xai_grok_sampling_types::BedrockConverseStreamCompat {
                        supports_strict_mode: true,
                        thinking_level_map: Default::default(),
                    },
                ),
            ),
            prompt_cache_retention: Some("long".into()),
            ..Default::default()
        };
        let parts = build_request(&req).unwrap();
        let tool = parts.tool_config.unwrap().tools.remove(0);
        let br::Tool::ToolSpec(spec) = tool else {
            panic!("expected tool specification")
        };
        assert_eq!(spec.strict, Some(true));
        let br::SystemContentBlock::CachePoint(point) = &parts.system[1] else {
            panic!("expected system cache point")
        };
        assert_eq!(point.ttl.as_ref(), Some(&br::CacheTtl::OneHour));
        let br::ContentBlock::CachePoint(point) =
            parts.messages.last().unwrap().content.last().unwrap()
        else {
            panic!("expected trailing user cache point")
        };
        assert_eq!(point.ttl.as_ref(), Some(&br::CacheTtl::OneHour));
    }

    #[test]
    fn reasoning_none_does_not_enable_claude_thinking() {
        let req = ConversationRequest {
            model: Some("anthropic.claude-opus-4-8".into()),
            reasoning_effort: Some(xai_grok_sampling_types::ReasoningEffort::None),
            ..Default::default()
        };
        assert!(build_additional_model_request_fields(&req).is_none());
    }

    #[test]
    fn request_merges_consecutive_tool_results() {
        let req = ConversationRequest {
            items: vec![
                ConversationItem::System(SystemItem {
                    content: Arc::from("sys"),
                }),
                ConversationItem::ToolResult(ToolResultItem {
                    tool_call_id: "tool use 1".into(),
                    content: Arc::from("a"),
                    is_error: false,
                    images: vec![],
                }),
                ConversationItem::ToolResult(ToolResultItem {
                    tool_call_id: "tool_use_2".into(),
                    content: Arc::from("b"),
                    is_error: false,
                    images: vec![],
                }),
                ConversationItem::User(UserItem {
                    content: vec![ContentPart::Text {
                        text: Arc::from("hi"),
                    }],
                    ..Default::default()
                }),
            ],
            ..Default::default()
        };
        let parts = build_request(&req).unwrap();
        assert_eq!(parts.system.len(), 1);
        assert_eq!(parts.messages.len(), 2);
        assert_eq!(parts.messages[0].role, br::ConversationRole::User);
        assert_eq!(parts.messages[0].content.len(), 2);
    }

    #[tokio::test]
    async fn stream_preserves_block_order_signatures_tools_and_cache_usage() {
        let message_start = br::MessageStartEvent::builder()
            .role(br::ConversationRole::Assistant)
            .build()
            .unwrap();
        let text = br::ContentBlockDeltaEvent::builder()
            .content_block_index(0)
            .delta(br::ContentBlockDelta::Text("hello".into()))
            .build()
            .unwrap();
        let stop_text = br::ContentBlockStopEvent::builder()
            .content_block_index(0)
            .build()
            .unwrap();
        let tool_start = br::ContentBlockStartEvent::builder()
            .content_block_index(1)
            .start(br::ContentBlockStart::ToolUse(
                br::ToolUseBlockStart::builder()
                    .tool_use_id("call_1")
                    .name("read_file")
                    .build()
                    .unwrap(),
            ))
            .build()
            .unwrap();
        let tool_delta = br::ContentBlockDeltaEvent::builder()
            .content_block_index(1)
            .delta(br::ContentBlockDelta::ToolUse(
                br::ToolUseBlockDelta::builder()
                    .input("{}")
                    .build()
                    .unwrap(),
            ))
            .build()
            .unwrap();
        let stop_tool = br::ContentBlockStopEvent::builder()
            .content_block_index(1)
            .build()
            .unwrap();
        let reasoning_text = br::ContentBlockDeltaEvent::builder()
            .content_block_index(2)
            .delta(br::ContentBlockDelta::ReasoningContent(
                br::ReasoningContentBlockDelta::Text("think".into()),
            ))
            .build()
            .unwrap();
        let reasoning_signature = br::ContentBlockDeltaEvent::builder()
            .content_block_index(2)
            .delta(br::ContentBlockDelta::ReasoningContent(
                br::ReasoningContentBlockDelta::Signature("sig".into()),
            ))
            .build()
            .unwrap();
        let stop_reasoning = br::ContentBlockStopEvent::builder()
            .content_block_index(2)
            .build()
            .unwrap();
        let usage = br::TokenUsage::builder()
            .input_tokens(5)
            .output_tokens(4)
            .total_tokens(9)
            .cache_read_input_tokens(2)
            .cache_write_input_tokens(3)
            .build()
            .unwrap();
        let metadata = br::ConverseStreamMetadataEvent::builder()
            .usage(usage)
            .build();
        let message_stop = br::MessageStopEvent::builder()
            .stop_reason(br::StopReason::ToolUse)
            .build()
            .unwrap();
        let raw = futures_util::stream::iter(vec![
            Ok(br::ConverseStreamOutput::MessageStart(message_start)),
            Ok(br::ConverseStreamOutput::ContentBlockDelta(text)),
            Ok(br::ConverseStreamOutput::ContentBlockStop(stop_text)),
            Ok(br::ConverseStreamOutput::ContentBlockStart(tool_start)),
            Ok(br::ConverseStreamOutput::ContentBlockDelta(tool_delta)),
            Ok(br::ConverseStreamOutput::ContentBlockStop(stop_tool)),
            Ok(br::ConverseStreamOutput::ContentBlockDelta(reasoning_text)),
            Ok(br::ConverseStreamOutput::ContentBlockDelta(
                reasoning_signature,
            )),
            Ok(br::ConverseStreamOutput::ContentBlockStop(stop_reasoning)),
            Ok(br::ConverseStreamOutput::Metadata(metadata)),
            Ok(br::ConverseStreamOutput::MessageStop(message_stop)),
        ])
        .boxed();
        let identity = ReasoningModelIdentity::new(
            "anthropic.claude-sonnet-4-5",
            xai_grok_sampling_types::ApiBackend::BedrockConverseStream,
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        );
        let (response, metrics) = crate::stream::collect_response(stream_bedrock_converse(
            raw,
            RequestId::from("bedrock-test"),
            identity,
            Duration::from_secs(5),
        ))
        .await
        .unwrap();
        assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
        assert_eq!(response.usage.as_ref().unwrap().prompt_tokens, 10);
        assert_eq!(response.usage.as_ref().unwrap().total_tokens, 14);
        assert!(metrics.time_to_first_token_ms.is_some());
        let assistant = response
            .items
            .iter()
            .find_map(|item| match item {
                ConversationItem::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .unwrap();
        assert_eq!(assistant.tool_calls.len(), 1);
        let blocks = assistant
            .provider_native_state
            .as_ref()
            .and_then(ProviderNativeAssistantState::bedrock_blocks)
            .unwrap();
        assert!(matches!(blocks[0], BedrockNativeBlock::Text { .. }));
        assert!(matches!(blocks[1], BedrockNativeBlock::ToolUse { .. }));
        assert!(matches!(
            &blocks[2],
            BedrockNativeBlock::Reasoning { signature: Some(signature), .. } if signature == "sig"
        ));
    }
}
