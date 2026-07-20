//! Provider router presented to the pager as one ACP agent.
//!
//! Codex/ChatGPT subscription models are now native catalog entries
//! (`openai-codex/*`) served by the shell's own sampler against the ChatGPT
//! Codex Responses backend — the external `codex app-server` dependency is
//! gone. This wrapper remains only as a thin delegation layer, plus a
//! migration shim that rewrites legacy `codex:<model>` ids (saved sessions)
//! onto the native `openai-codex/<model>` catalog ids.

use std::rc::Rc;

use agent_client_protocol as acp;
use xai_acp_lib::AcpGatewaySender;
use xai_grok_shell::agent::MvpAgent;

/// Legacy app-server model prefix (pre-native sessions).
pub(crate) const CODEX_MODEL_PREFIX: &str = "codex:";
/// Native catalog prefix for ChatGPT subscription models.
pub(crate) const OPENAI_CODEX_MODEL_PREFIX: &str = "openai-codex/";

pub(crate) fn is_codex_model(model_id: &acp::ModelId) -> bool {
    model_id.0.as_ref().starts_with(CODEX_MODEL_PREFIX)
}

/// Map a legacy `codex:<model>` id onto the native `openai-codex/<model>`
/// catalog id; everything else passes through unchanged.
pub(crate) fn migrate_model_id(model_id: &str) -> String {
    match model_id.strip_prefix(CODEX_MODEL_PREFIX) {
        Some(rest) if !rest.is_empty() => format!("{OPENAI_CODEX_MODEL_PREFIX}{rest}"),
        _ => model_id.to_owned(),
    }
}

pub(crate) struct ProviderRouterAgent {
    grok: Rc<MvpAgent>,
}

impl ProviderRouterAgent {
    pub(crate) fn new(grok: Rc<MvpAgent>, client: AcpGatewaySender<acp::AgentSide>) -> Self {
        // The client sender was only used to fan out Codex app-server
        // notifications; native sessions stream through `grok` directly.
        let _ = client;
        Self { grok }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for ProviderRouterAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        self.grok.initialize(args).await
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        self.grok.authenticate(args).await
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        self.grok.new_session(args).await
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        self.grok.prompt(args).await
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        self.grok.cancel(args).await
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        self.grok.load_session(args).await
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        self.grok.set_session_mode(args).await
    }

    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        // Legacy sessions saved a `codex:<model>` id from the app-server
        // era; rewrite onto the native catalog id before delegating.
        let requested = args.model_id.0.as_ref();
        if is_codex_model(&args.model_id) {
            let migrated = migrate_model_id(requested);
            tracing::info!(
                requested,
                migrated = migrated.as_str(),
                "migrating legacy Codex model id to native catalog"
            );
            let args = acp::SetSessionModelRequest::new(
                args.session_id,
                acp::ModelId::new(std::sync::Arc::<str>::from(migrated)),
            )
            .meta(args.meta);
            return self.grok.set_session_model(args).await;
        }
        self.grok.set_session_model(args).await
    }

    async fn set_session_config_option(
        &self,
        args: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        self.grok.set_session_config_option(args).await
    }

    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        self.grok.list_sessions(args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        self.grok.ext_method(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> Result<(), acp::Error> {
        self.grok.ext_notification(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_codex_ids_migrate_to_native_catalog() {
        assert_eq!(migrate_model_id("codex:gpt-5.4"), "openai-codex/gpt-5.4");
        assert_eq!(migrate_model_id("codex:gpt-5.6-sol"), "openai-codex/gpt-5.6-sol");
        // Non-legacy ids pass through untouched.
        assert_eq!(migrate_model_id("openai-codex/gpt-5.4"), "openai-codex/gpt-5.4");
        assert_eq!(migrate_model_id("grok-4.5"), "grok-4.5");
        assert_eq!(migrate_model_id("codex:"), "codex:");
    }

    #[test]
    fn is_codex_model_matches_only_legacy_prefix() {
        assert!(is_codex_model(&acp::ModelId::new(std::sync::Arc::<str>::from(
            "codex:gpt-5.4"
        ))));
        assert!(!is_codex_model(&acp::ModelId::new(std::sync::Arc::<str>::from(
            "openai-codex/gpt-5.4"
        ))));
    }
}
