//! Compile-time selection of the authority this build talks to.
//!
//! One implementation compiles, so the trait is a checklist: a backend that forgets a decision
//! fails to build.
use crate::auth::flow::StderrCallback;
use crate::auth::refresh::{DiagnosticUploader, TokenRefresher};
use crate::auth::{AuthManager, AuthUrlInfo, GrokAuth, GrokComConfig, LoginTransportOverride};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
mod grok;
/// The inputs of one login attempt.
pub(crate) struct LoginRequest<'a> {
    pub(crate) auth_manager: &'a Arc<AuthManager>,
    pub(crate) grok_com_config: &'a GrokComConfig,
    pub(crate) reauth: bool,
    pub(crate) force_interactive: bool,
    pub(crate) on_stderr: Option<StderrCallback>,
    pub(crate) url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    pub(crate) code_rx: Option<mpsc::Receiver<String>>,
    pub(crate) login_override: LoginTransportOverride,
}
/// `?Send`: `url_tx` is an `Rc`, so a login future can never cross threads.
#[async_trait::async_trait(?Send)]
pub(crate) trait AuthBackend {
    /// Key under which this backend owns its entry in auth.json.
    fn scope_key(&self, config: &GrokComConfig) -> String;
    /// Older scope keys this backend minted, and so may adopt from and tidy, most recent first.
    fn inherited_scopes(&self) -> &'static [&'static str];
    /// Whether xAI issued this backend's credentials and may therefore receive them.
    /// Gates every request that carries the bearer to an xAI host, and every xAI-only policy.
    fn is_xai_authority(&self) -> bool;
    /// Obtain a credential; the flag reports whether a login actually ran.
    async fn login(&self, req: LoginRequest<'_>) -> anyhow::Result<(GrokAuth, bool)>;
    /// The renewal authority for the credentials this backend mints.
    fn refresher(
        &self,
        manager: Arc<AuthManager>,
        auth_provider_command: Option<String>,
        diagnostic_uploader: Option<DiagnosticUploader>,
    ) -> Arc<dyn TokenRefresher>;
}
pub(crate) type ActiveAuthBackend = grok::GrokAuthBackend;
