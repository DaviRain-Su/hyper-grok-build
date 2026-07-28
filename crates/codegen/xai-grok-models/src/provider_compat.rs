//! Typed request-compatibility and route metadata shared by generated providers.
//!
//! Pi carries protocol-specific compatibility fields per model. Keeping those
//! fields typed here prevents provider quirks from turning into URL heuristics
//! or an ever-growing set of platform conditionals in the sampler.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    #[default]
    MaxCompletionTokens,
    MaxTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingFormat {
    #[default]
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openrouter")]
    OpenRouter,
    #[serde(rename = "deepseek")]
    DeepSeek,
    Together,
    Zai,
    Qwen,
    #[serde(rename = "chat-template")]
    ChatTemplate,
    #[serde(rename = "qwen-chat-template")]
    QwenChatTemplate,
    #[serde(rename = "string-thinking")]
    StringThinking,
    #[serde(rename = "ant-ling")]
    AntLing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlFormat {
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredToolsMode {
    Kimi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionAffinityFormat {
    #[default]
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openai-nosession")]
    OpenAiNoSession,
    #[serde(rename = "openrouter")]
    OpenRouter,
}

/// Fully resolved OpenAI Chat Completions behavior. Unlike Pi's source type,
/// fields are non-optional: the catalog sync resolves provider/URL defaults at
/// generation time so normal runtime code never guesses from a hostname.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiCompletionsCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub max_tokens_field: MaxTokensField,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub thinking_format: ThinkingFormat,
    pub chat_template_kwargs: BTreeMap<String, serde_json::Value>,
    pub openrouter_routing: BTreeMap<String, serde_json::Value>,
    pub vercel_gateway_routing: BTreeMap<String, serde_json::Value>,
    pub zai_tool_stream: bool,
    pub supports_openai_grammar_tools: bool,
    pub supports_strict_mode: bool,
    pub cache_control_format: Option<CacheControlFormat>,
    pub send_session_affinity_headers: bool,
    pub deferred_tools_mode: Option<DeferredToolsMode>,
    pub session_affinity_format: SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
}

impl Default for OpenAiCompletionsCompat {
    fn default() -> Self {
        Self {
            supports_store: true,
            supports_developer_role: true,
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            requires_tool_result_name: false,
            requires_assistant_after_tool_result: false,
            requires_thinking_as_text: false,
            requires_reasoning_content_on_assistant_messages: false,
            thinking_format: ThinkingFormat::OpenAi,
            chat_template_kwargs: BTreeMap::new(),
            openrouter_routing: BTreeMap::new(),
            vercel_gateway_routing: BTreeMap::new(),
            zai_tool_stream: false,
            supports_openai_grammar_tools: false,
            supports_strict_mode: true,
            cache_control_format: None,
            send_session_affinity_headers: false,
            deferred_tools_mode: None,
            session_affinity_format: SessionAffinityFormat::OpenAi,
            supports_long_cache_retention: true,
        }
    }
}

/// Fully resolved OpenAI Responses behavior (also used by Azure Responses and
/// the OpenAI Codex Responses dialect before adapter-specific overrides).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiResponsesCompat {
    pub supports_developer_role: bool,
    pub session_affinity_format: SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub supports_tool_search: bool,
    pub supports_explicit_prompt_cache_mode: bool,
}

impl Default for OpenAiResponsesCompat {
    fn default() -> Self {
        Self {
            supports_developer_role: true,
            session_affinity_format: SessionAffinityFormat::OpenAi,
            supports_long_cache_retention: true,
            supports_strict_mode: false,
            supports_openai_grammar_tools: false,
            supports_tool_search: false,
            supports_explicit_prompt_cache_mode: false,
        }
    }
}

/// Fully resolved Anthropic Messages behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnthropicMessagesCompat {
    pub supports_eager_tool_input_streaming: bool,
    pub supports_long_cache_retention: bool,
    pub send_session_affinity_headers: bool,
    pub supports_cache_control_on_tools: bool,
    pub supports_temperature: bool,
    pub force_adaptive_thinking: bool,
    pub allow_empty_signature: bool,
    pub supports_strict_tools: bool,
    pub supports_tool_references: bool,
}

impl Default for AnthropicMessagesCompat {
    fn default() -> Self {
        Self {
            supports_eager_tool_input_streaming: true,
            supports_long_cache_retention: true,
            send_session_affinity_headers: false,
            supports_cache_control_on_tools: true,
            supports_temperature: true,
            force_adaptive_thinking: false,
            allow_empty_signature: false,
            supports_strict_tools: false,
            supports_tool_references: false,
        }
    }
}

/// Fully resolved Google GenerateContent behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct GoogleGenerateContentCompat {
    pub supports_strict_tool_sampling: bool,
    /// Pi `thinkingLevelMap`; values are Google enum strings (`LOW`, `HIGH`, …) or null.
    pub thinking_level_map: BTreeMap<String, Option<String>>,
    /// Optional per-effort budget override, reserved for Pi-compatible custom budgets.
    pub thinking_budgets: BTreeMap<String, Value>,
}

/// Fully resolved Amazon Bedrock ConverseStream behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct BedrockConverseStreamCompat {
    /// Pi `compat.supportStrictMode`.
    pub supports_strict_mode: bool,
    /// Pi `thinkingLevelMap`; values are provider-specific strings or null.
    pub thinking_level_map: BTreeMap<String, Option<String>>,
}

/// Fully resolved Pi Messages behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PiMessagesCompat {}

/// Compatibility metadata for one concrete wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestCompat {
    #[serde(rename = "chat_completions")]
    ChatCompletions(OpenAiCompletionsCompat),
    #[serde(rename = "responses")]
    Responses(OpenAiResponsesCompat),
    #[serde(rename = "messages")]
    Messages(AnthropicMessagesCompat),
    #[serde(rename = "google_generate_content")]
    GoogleGenerateContent(GoogleGenerateContentCompat),
    #[serde(rename = "bedrock_converse_stream")]
    BedrockConverseStream(BedrockConverseStreamCompat),
    #[serde(rename = "pi_messages")]
    PiMessages(PiMessagesCompat),
}

impl RequestCompat {
    pub fn chat_completions(&self) -> Option<&OpenAiCompletionsCompat> {
        match self {
            Self::ChatCompletions(value) => Some(value),
            _ => None,
        }
    }

    pub fn responses(&self) -> Option<&OpenAiResponsesCompat> {
        match self {
            Self::Responses(value) => Some(value),
            _ => None,
        }
    }

    pub fn messages(&self) -> Option<&AnthropicMessagesCompat> {
        match self {
            Self::Messages(value) => Some(value),
            _ => None,
        }
    }

    pub fn google_generate_content(&self) -> Option<&GoogleGenerateContentCompat> {
        match self {
            Self::GoogleGenerateContent(value) => Some(value),
            _ => None,
        }
    }

    pub fn bedrock_converse_stream(&self) -> Option<&BedrockConverseStreamCompat> {
        match self {
            Self::BedrockConverseStream(value) => Some(value),
            _ => None,
        }
    }

    pub fn pi_messages(&self) -> Option<&PiMessagesCompat> {
        match self {
            Self::PiMessages(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RouteAuth {
    #[default]
    Bearer,
    XApiKey,
    /// Azure OpenAI's raw `api-key` request header.
    ApiKey,
    /// Cloudflare AI Gateway's `cf-aig-authorization: Bearer …` header.
    CfAigAuthorization,
    /// Google REST `x-goog-api-key` request header.
    XGoogApiKey,
}

/// Per-model HTTP route. `base_url` stays on `BuiltinPlatformModel` so user
/// environment overrides retain their existing precedence; this structure owns
/// everything appended or attached to that base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderRouteSpec {
    /// Relative endpoint path, without a leading slash.
    pub path: String,
    pub auth: RouteAuth,
    pub headers: BTreeMap<String, String>,
    pub query_params: BTreeMap<String, String>,
}

impl Default for ProviderRouteSpec {
    fn default() -> Self {
        Self {
            path: String::new(),
            auth: RouteAuth::Bearer,
            headers: BTreeMap::new(),
            query_params: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_compat_is_protocol_tagged_and_strict() {
        let value = RequestCompat::ChatCompletions(OpenAiCompletionsCompat {
            max_tokens_field: MaxTokensField::MaxTokens,
            thinking_format: ThinkingFormat::DeepSeek,
            ..Default::default()
        });
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["chat_completions"]["max_tokens_field"], "max_tokens");
        assert_eq!(json["chat_completions"]["thinking_format"], "deepseek");
        assert_eq!(
            serde_json::from_value::<RequestCompat>(json).unwrap(),
            value
        );

        assert!(
            serde_json::from_value::<OpenAiResponsesCompat>(serde_json::json!({
                "unknown_field": true
            }))
            .is_err()
        );
    }

    #[test]
    fn route_metadata_round_trips() {
        let route = ProviderRouteSpec {
            path: "messages".into(),
            auth: RouteAuth::XApiKey,
            headers: BTreeMap::from([("anthropic-version".into(), "2023-06-01".into())]),
            query_params: BTreeMap::new(),
        };
        assert_eq!(
            serde_json::from_value::<ProviderRouteSpec>(serde_json::to_value(&route).unwrap())
                .unwrap(),
            route
        );
    }
}
