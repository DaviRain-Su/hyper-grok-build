//! Backend adapter selection and validation.
//!
//! `ApiBackend` describes one of Hyper's existing wire protocols. `AdapterKind`
//! describes provider-specific behavior layered on top of (or eventually
//! replacing) those protocols. Keeping their resolution here gives native
//! providers one dispatch boundary without moving the stable Chat, Responses,
//! and Messages transports during the foundation refactor.

use xai_grok_sampling_types::{AdapterKind, ApiBackend, Result, SamplingError};

/// Resolved backend implementation for one sampling client.
///
/// Standard-compatible providers retain their existing wire backend. Native
/// variants deliberately have no wire backend until their adapter is
/// implemented, so a planned provider fails closed instead of accidentally
/// sending an OpenAI-shaped payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendAdapter {
    Standard(ApiBackend),
    KimiCoding(ApiBackend),
    OpenAiCodex,
    Nexus(ApiBackend),
    AnthropicClaude,
    MistralConversations,
    GitHubCopilot(ApiBackend),
    GoogleGenerateContent,
    BedrockConverseStream,
}

impl Default for BackendAdapter {
    fn default() -> Self {
        Self::Standard(ApiBackend::default())
    }
}

impl BackendAdapter {
    /// Resolve and validate provider adapter metadata against its catalog wire
    /// backend. This is the only factory used by `SamplingClient`.
    pub fn from_route(kind: AdapterKind, backend: ApiBackend) -> Result<Self> {
        match kind {
            AdapterKind::Standard if backend == ApiBackend::CodexResponses => Ok(Self::OpenAiCodex),
            AdapterKind::Standard => Ok(Self::Standard(backend)),
            AdapterKind::KimiCoding
                if matches!(backend, ApiBackend::ChatCompletions | ApiBackend::Messages) =>
            {
                Ok(Self::KimiCoding(backend))
            }
            AdapterKind::KimiCoding => Err(SamplingError::InvalidConfiguration(
                "Kimi Coding adapter requires Chat Completions or Messages",
            )),
            AdapterKind::OpenAiCodex
                if matches!(backend, ApiBackend::Responses | ApiBackend::CodexResponses) =>
            {
                Ok(Self::OpenAiCodex)
            }
            AdapterKind::OpenAiCodex => Err(SamplingError::InvalidConfiguration(
                "OpenAI Codex adapter requires the Responses or CodexResponses backend",
            )),
            AdapterKind::Nexus => Ok(Self::Nexus(backend)),
            AdapterKind::AnthropicClaude if backend == ApiBackend::Messages => {
                Ok(Self::AnthropicClaude)
            }
            AdapterKind::AnthropicClaude => Err(SamplingError::InvalidConfiguration(
                "Anthropic Claude adapter requires the Messages backend",
            )),
            AdapterKind::MistralConversations if backend == ApiBackend::ChatCompletions => {
                Ok(Self::MistralConversations)
            }
            AdapterKind::MistralConversations => Err(SamplingError::InvalidConfiguration(
                "Mistral Conversations adapter requires the Chat Completions backend",
            )),
            AdapterKind::GitHubCopilot => Ok(Self::GitHubCopilot(backend)),
            AdapterKind::GoogleGenerateContent if backend == ApiBackend::GoogleGenerateContent => {
                Ok(Self::GoogleGenerateContent)
            }
            AdapterKind::GoogleGenerateContent => Err(SamplingError::InvalidConfiguration(
                "Google GenerateContent adapter requires the Google GenerateContent backend",
            )),
            AdapterKind::BedrockConverseStream if backend == ApiBackend::BedrockConverseStream => {
                Ok(Self::BedrockConverseStream)
            }
            AdapterKind::BedrockConverseStream => Err(SamplingError::InvalidConfiguration(
                "Bedrock ConverseStream adapter requires the Bedrock ConverseStream backend",
            )),
        }
    }

    pub fn kind(&self) -> AdapterKind {
        match self {
            Self::Standard(_) => AdapterKind::Standard,
            Self::KimiCoding(_) => AdapterKind::KimiCoding,
            Self::OpenAiCodex => AdapterKind::OpenAiCodex,
            Self::Nexus(_) => AdapterKind::Nexus,
            Self::AnthropicClaude => AdapterKind::AnthropicClaude,
            Self::MistralConversations => AdapterKind::MistralConversations,
            Self::GitHubCopilot(_) => AdapterKind::GitHubCopilot,
            Self::GoogleGenerateContent => AdapterKind::GoogleGenerateContent,
            Self::BedrockConverseStream => AdapterKind::BedrockConverseStream,
        }
    }

    /// Existing wire protocol, if this adapter is implemented by one of the
    /// three stable transports.
    pub fn wire_backend(&self) -> Option<&ApiBackend> {
        match self {
            Self::Standard(backend)
            | Self::KimiCoding(backend)
            | Self::Nexus(backend)
            | Self::GitHubCopilot(backend) => Some(backend),
            Self::OpenAiCodex => Some(&ApiBackend::Responses),
            Self::AnthropicClaude => Some(&ApiBackend::Messages),
            Self::MistralConversations => Some(&ApiBackend::ChatCompletions),
            Self::GoogleGenerateContent => Some(&ApiBackend::GoogleGenerateContent),
            Self::BedrockConverseStream => Some(&ApiBackend::BedrockConverseStream),
        }
    }

    /// Default endpoint for adapters backed by an existing wire protocol.
    pub fn endpoint_path(&self) -> Option<&'static str> {
        self.wire_backend()
            .map(|backend| backend.default_endpoint_path())
    }

    pub fn uses_kimi_dialect(&self) -> bool {
        matches!(self, Self::KimiCoding(_))
    }

    pub fn uses_openai_codex_dialect(&self) -> bool {
        matches!(self, Self::OpenAiCodex)
    }

    pub fn uses_mistral_conversations_dialect(&self) -> bool {
        matches!(self, Self::MistralConversations)
    }

    pub fn uses_github_copilot_dialect(&self) -> bool {
        matches!(self, Self::GitHubCopilot(_))
    }

    pub fn ensure_implemented(&self) -> Result<()> {
        if self.wire_backend().is_some() {
            Ok(())
        } else {
            Err(SamplingError::InvalidConfiguration(
                "selected provider backend adapter is not implemented",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_factory_preserves_all_existing_wire_backends() {
        for backend in [
            ApiBackend::ChatCompletions,
            ApiBackend::Responses,
            ApiBackend::Messages,
            ApiBackend::PiMessages,
        ] {
            let adapter = BackendAdapter::from_route(AdapterKind::Standard, backend.clone())
                .expect("standard adapter");
            assert_eq!(adapter.wire_backend(), Some(&backend));
            assert!(adapter.ensure_implemented().is_ok());
        }
    }

    #[test]
    fn provider_dialects_validate_their_wire_protocols() {
        let kimi = BackendAdapter::from_route(AdapterKind::KimiCoding, ApiBackend::Messages)
            .expect("Kimi Messages adapter");
        assert!(kimi.uses_kimi_dialect());
        assert_eq!(kimi.endpoint_path(), Some("messages"));

        let codex = BackendAdapter::from_route(AdapterKind::OpenAiCodex, ApiBackend::Responses)
            .expect("Codex Responses adapter");
        assert!(codex.uses_openai_codex_dialect());
        assert_eq!(codex.endpoint_path(), Some("responses"));

        let codex_proxy =
            BackendAdapter::from_route(AdapterKind::OpenAiCodex, ApiBackend::CodexResponses)
                .expect("CodexResponses backend with OpenAiCodex adapter");
        assert!(codex_proxy.uses_openai_codex_dialect());
        assert_eq!(codex_proxy.endpoint_path(), Some("responses"));
        let configured_codex =
            BackendAdapter::from_route(AdapterKind::Standard, ApiBackend::CodexResponses)
                .expect("configured Codex Responses route");
        assert!(configured_codex.uses_openai_codex_dialect());
        assert_eq!(
            configured_codex.wire_backend(),
            Some(&ApiBackend::Responses)
        );

        assert!(
            BackendAdapter::from_route(AdapterKind::OpenAiCodex, ApiBackend::ChatCompletions)
                .is_err()
        );
        assert!(
            BackendAdapter::from_route(AdapterKind::AnthropicClaude, ApiBackend::Responses)
                .is_err()
        );
    }

    #[test]
    fn mistral_adapter_is_chat_completions_dialect() {
        let adapter = BackendAdapter::from_route(
            AdapterKind::MistralConversations,
            ApiBackend::ChatCompletions,
        )
        .expect("Mistral uses Chat Completions wire");
        assert_eq!(adapter.wire_backend(), Some(&ApiBackend::ChatCompletions));
        assert_eq!(adapter.endpoint_path(), Some("chat/completions"));
        assert!(adapter.uses_mistral_conversations_dialect());
        assert!(adapter.ensure_implemented().is_ok());
        assert!(
            BackendAdapter::from_route(AdapterKind::MistralConversations, ApiBackend::Responses)
                .is_err()
        );
    }
}
