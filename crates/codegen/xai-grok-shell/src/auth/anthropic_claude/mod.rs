//! Anthropic Claude (Pro/Max) subscription auth (browser OAuth).
//!
//! Credentials are stored under [`crate::auth::model::ANTHROPIC_CLAUDE_OAUTH_SCOPE`]
//! in `~/.grok/auth.json` and stamped onto `anthropic-claude/*` catalog
//! entries. This path is independent of the primary xAI AuthManager session.

mod login;
pub(crate) mod oauth;

pub use crate::auth::model::ANTHROPIC_CLAUDE_OAUTH_SCOPE;
pub use login::{
    AnthropicClaudeBearerResolver, anthropic_claude_catalog_access_token_cached,
    ensure_anthropic_claude_access_token_blocking, ensure_anthropic_claude_auth,
    ensure_anthropic_claude_auth_blocking, force_refresh_anthropic_claude_auth,
    run_anthropic_claude_login,
};
pub use oauth::OAUTH_BETA_HEADER_VALUE;
