//! Auth adapter for Codex Live: bridges the shell's OpenAI Codex OAuth onto
//! the [`LiveAuthProvider`] trait, returning a bearer + account id.
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
/// a token that 401s. On a 401 the pager calls `force_refresh_openai_codex_auth`
/// via [`force_refresh`].
#[derive(Debug, Clone)]
pub struct CodexLiveAuth {
    /// Set to `true` to force a network refresh (401 recovery).
    force_refresh: bool,
}

impl CodexLiveAuth {
    /// Create a provider that resolves the cached-or-refreshed credential.
    pub fn new() -> Self {
        Self {
            force_refresh: false,
        }
    }

    /// Create a provider that forces a network refresh (401 recovery).
    pub fn force_refresh() -> Self {
        Self {
            force_refresh: true,
        }
    }
}

impl Default for CodexLiveAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveAuthProvider for CodexLiveAuth {
    fn live_auth(&self) -> Pin<Box<dyn Future<Output = Option<LiveAuth>> + Send + '_>> {
        let force = self.force_refresh;
        Box::pin(async move {
            let auth = if force {
                xai_grok_shell::auth::openai_codex::force_refresh_openai_codex_auth().await
            } else {
                xai_grok_shell::auth::openai_codex::ensure_openai_codex_auth().await
            };
            auth.map(|a| LiveAuth {
                bearer: a.key,
                account_id: a.account_id.unwrap_or_default(),
            })
        })
    }
}

/// Build the Live auth provider from the shell's Codex OAuth (cached-or-refresh).
pub fn build_live_auth() -> super::SharedLiveAuth {
    Arc::new(CodexLiveAuth::new())
}

/// Build the Live auth provider in force-refresh mode (401 recovery).
pub fn build_live_auth_force_refresh() -> super::SharedLiveAuth {
    Arc::new(CodexLiveAuth::force_refresh())
}
