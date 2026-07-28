//! GitHub Copilot subscription auth core.
//!
//! Credentials are stored under [`crate::auth::model::GITHUB_COPILOT_OAUTH_SCOPE`]
//! in `~/.grok/auth.json`. `GrokAuth.key` is the short Copilot inference token;
//! `GrokAuth.refresh_token` is the durable GitHub OAuth token used to mint it.

mod login;
pub(crate) mod oauth;

pub use crate::auth::model::GITHUB_COPILOT_OAUTH_SCOPE;
pub use login::{
    COPILOT_GITHUB_TOKEN_ENV, GitHubCopilotBearerResolver, copilot_github_token_env,
    force_refresh_github_copilot_auth, github_copilot_available_models_cached,
    github_copilot_catalog_access_token_cached, github_copilot_catalog_base_url_cached,
    run_github_copilot_login, run_github_copilot_login_with_channels,
};
pub(crate) use login::{ensure_github_copilot_access_token_blocking, ensure_github_copilot_auth};
