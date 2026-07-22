//! OpenAI Codex (ChatGPT) subscription auth.
//!
//! First-party OAuth against `https://auth.openai.com` (browser PKCE +
//! device code), ported from official Pi `auth/oauth/openai-codex.ts`.
//! Credentials are stored under [`super::model::OPENAI_CODEX_OAUTH_SCOPE`] in
//! `~/.grok/auth.json` and stamped onto `openai-codex/*` catalog entries.
//! This path is independent of the primary xAI AuthManager session.

mod login;
mod oauth;

pub use crate::auth::model::OPENAI_CODEX_OAUTH_SCOPE;
pub use login::{
    CodexLoginMethod, OpenAiCodexBearerResolver, ensure_openai_codex_access_token,
    ensure_openai_codex_access_token_blocking, ensure_openai_codex_auth,
    ensure_openai_codex_auth_blocking, force_refresh_openai_codex_auth,
    openai_codex_catalog_access_token_cached, run_openai_codex_login,
};
