//! The Grok Build backend: xAI OAuth2 / enterprise OIDC, the operator's auth binary, devbox.

use std::sync::Arc;

use super::{AuthBackend, LoginRequest};
use crate::auth::refresh::{
    AuthSnapshot, DiagnosticUploader, ExternalBinaryRefresher, ExternalCommandRunner,
    OidcRefresher, TokenRefresher,
};
use crate::auth::{AuthManager, GrokAuth, GrokComConfig};

#[derive(Default)]
pub(crate) struct GrokAuthBackend;

#[async_trait::async_trait(?Send)]
impl AuthBackend for GrokAuthBackend {
    fn scope_key(&self, config: &GrokComConfig) -> String {
        config.auth_scope()
    }

    /// Pre-OIDC devbox auth files wrote this key, and only this backend ever minted into it.
    fn inherited_scopes(&self) -> &'static [&'static str] {
        &[crate::auth::model::LEGACY_SCOPE]
    }

    fn is_xai_authority(&self) -> bool {
        true
    }

    async fn login(&self, req: LoginRequest<'_>) -> anyhow::Result<(GrokAuth, bool)> {
        crate::auth::flow::run_auth_flow_steps(
            req.auth_manager,
            req.grok_com_config,
            req.reauth,
            req.force_interactive,
            req.on_stderr,
            req.url_tx,
            req.code_rx,
            req.login_override,
        )
        .await
    }

    fn refresher(
        &self,
        manager: Arc<AuthManager>,
        auth_provider_command: Option<String>,
        diagnostic_uploader: Option<DiagnosticUploader>,
    ) -> Arc<dyn TokenRefresher> {
        match auth_provider_command {
            Some(cmd) => {
                let runner: Arc<dyn ExternalCommandRunner> = manager;
                Arc::new(ExternalBinaryRefresher::new(runner, cmd))
            }
            None => {
                let snapshot: Arc<dyn AuthSnapshot> = manager;
                let refresher = OidcRefresher::new(snapshot);
                match diagnostic_uploader {
                    Some(uploader) => Arc::new(refresher.with_diagnostic_upload(uploader)),
                    None => Arc::new(refresher),
                }
            }
        }
    }
}
