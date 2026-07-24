//! Auth adapter for Codex Live: bridges the shell's OpenAI Codex OAuth onto
//! the real [`LiveAuthProvider`] trait (`bearer_account` / `force_refresh`),
//! returning a bearer + optional account id.
//!
//! Uses `ensure_openai_codex_auth` / `force_refresh_openai_codex_auth` so the
//! Live session follows the same credential lifecycle as the agent's Codex
//! sampler — no separate env var. Independent of the xAI `/voice` subscription
//! tier (which gates `api.x.ai` STT, not `chatgpt.com/backend-api/codex`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::{LiveAuth, LiveAuthProvider};

/// Adapts the shell's OpenAI Codex auth onto [`LiveAuthProvider`].
///
/// Resolves a fresh token per request (never a static snapshot) so a long Live
/// session follows the underlying `auth.json` refresh cycle instead of pinning
/// a token that 401s. On a 401 the voice core calls `force_refresh`, which
/// delegates to `force_refresh_openai_codex_auth`.
#[derive(Debug, Clone, Default)]
pub struct CodexLiveAuth;

impl LiveAuthProvider for CodexLiveAuth {
    fn bearer_account(&self) -> Pin<Box<dyn Future<Output = Option<LiveAuth>> + Send + '_>> {
        Box::pin(async move {
            let auth = xai_grok_shell::auth::openai_codex::ensure_openai_codex_auth().await;
            auth.map(|a| LiveAuth {
                bearer: a.key,
                account_id: a.account_id,
            })
        })
    }

    fn force_refresh(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let _ = xai_grok_shell::auth::openai_codex::force_refresh_openai_codex_auth().await;
        })
    }
}

/// Build the Live auth provider from the shell's Codex OAuth (cached-or-refresh).
pub fn build_live_auth() -> super::SharedLiveAuth {
    Arc::new(CodexLiveAuth)
}
