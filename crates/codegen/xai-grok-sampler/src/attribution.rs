//! 401 attribution callback hook for the sampling client.
//!
//! Every 401 response site can optionally emit an attribution event so
//! a downstream observer can split production 401s into "client sent a
//! stale snapshot bearer that the server rejected" vs. "client sent
//! the live token from its auth source and the server still rejected
//! it" buckets.
//!
//! `xai-grok-sampler` is intentionally decoupled from `xai-grok-shell`
//! (no shell types, no logging crate, no auth-manager dependency). The
//! caller wires an implementation of [`Auth401AttributionCallback`]
//! into [`crate::SamplerConfig::attribution_callback`]; the sampler
//! invokes the callback at each UNAUTHORIZED arm with the bearer that
//! was actually sent on the wire. The implementation is free to join
//! the bearer with whatever live credential source it owns and emit
//! the attribution however it wants.
//!
//! When the callback is `None` (the default), the 401 sites are silent
//! and return the same `SamplingError::Auth` they would otherwise.

use std::sync::Arc;

/// A logical 401-emitting site inside the sampling client. The string
/// identifier ends up in the consumer field of the attribution event
/// so downstream queries can break down 401s by API path.
///
/// # Scope: sampler endpoints only
///
/// This enum enumerates the six HTTP endpoints owned by
/// `SamplingClient` (chat completions, responses, messages -- each in
/// streaming and non-streaming form). It does *not* cover image
/// generation, video generation, web search, or embedding -- those
/// tools live in `xai-grok-tools`
/// (`crates/codegen/xai-grok-tools/src/implementations/`), have their
/// own HTTP clients that do not flow through `SamplingClient`, and
/// hook into the `xai_grok_tools::ApiKeyProvider` trait rather than
/// this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingConsumer {
    /// `chat_completion_stream`: OpenAI-compatible streaming OpenAI Chat Completions API.
    ChatCompletionsStream,
    /// `chat_completion`: OpenAI-compatible non-streaming OpenAI Chat Completions API.
    ChatCompletions,
    /// `create_response_stream`: Responses API streaming.
    ResponsesStream,
    /// `create_response`: Responses API non-streaming.
    Responses,
    /// `messages_stream`: Anthropic Messages API streaming.
    MessagesStream,
    /// `messages`: Anthropic Messages API non-streaming.
    Messages,
}

impl SamplingConsumer {
    /// Stable string identifier for this emit site. Callbacks
    /// typically combine this with a fixed prefix (e.g. the client
    /// type) when building the consumer field of the attribution
    /// event.
    pub fn as_endpoint(self) -> &'static str {
        match self {
            Self::ChatCompletionsStream => "chat_completions_stream",
            Self::ChatCompletions => "chat_completions",
            Self::ResponsesStream => "responses_stream",
            Self::Responses => "responses",
            Self::MessagesStream => "messages_stream",
            Self::Messages => "messages",
        }
    }
}

/// Bearer-fragment length shared with attribution callbacks: the **last**
/// N chars (JWT heads are a shared constant; only the tail distinguishes
/// tokens). Must stay in lock-step with `token_suffix` in
/// `xai-grok-shell/src/auth/model.rs`, the comparison site.
pub const SENT_BEARER_PREFIX_LEN: usize = 12;
/// Last [`SENT_BEARER_PREFIX_LEN`] characters of a bearer, char-boundary
/// safe (bearer strings are visible-ASCII per the header grammars, but a
/// resolver-supplied `String` has no such guarantee -- counting chars from
/// the end avoids a byte-index panic on non-ASCII input).
pub(crate) fn bearer_tail_fragment(s: &str) -> &str {
    match s.char_indices().rev().nth(SENT_BEARER_PREFIX_LEN - 1) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

/// Hook invoked by [`crate::SamplingClient`] at every 401 response site.
///
/// Implementations are responsible for joining `sent_bearer_prefix`
/// with whatever live credential source they own (e.g. an auth
/// manager holding the most-recently-refreshed token) and emitting
/// whatever attribution event makes sense for their observability
/// stack.
///
/// Implementations must be cheap to invoke and must not block. They
/// run inside the request's response-handling path and any latency
/// they add is paid by the user-visible 401 error path.
//
// The `Debug` bound is a structural requirement: [`crate::SamplerConfig`]
// derives `Debug` and carries an `Option<Arc<dyn Auth401AttributionCallback>>`
// field, which only compiles when the trait is `Debug`. Do not remove
// the bound when factoring this trait out -- it will break
// `derive(Debug)` on `SamplerConfig`.
pub trait Auth401AttributionCallback: Send + Sync + std::fmt::Debug {
    /// Record a 401 attribution event for one logical 401 response.
    ///
    /// `sent_bearer_prefix` is the **last
    /// [`SENT_BEARER_PREFIX_LEN`] characters** (the tail -- see the
    /// constant's doc for why the tail, not the head) of the bearer
    /// that was actually sent on the wire. The sampler extracts the
    /// bearer from the `Authorization` header (or `x-api-key` for
    /// Anthropic Messages API backends) and truncates it to that
    /// fragment **before crossing this trait boundary** -- the full
    /// bearer never leaves [`crate::SamplingClient`]. This is the
    /// scrub-at-the-boundary invariant: even a misbehaving callback
    /// implementation that logs `sent_bearer_prefix` directly leaks
    /// only the prefix, never the full credential.
    ///
    /// `None` indicates the request had no bearer header at all
    /// (distinct from "had a bearer that turned out to be stale").
    fn record_401(&self, consumer: SamplingConsumer, sent_bearer_prefix: Option<&str>);
}

/// Shared, cheap-to-clone alias for the attribution callback.
pub type SharedAttributionCallback = Arc<dyn Auth401AttributionCallback>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tail_fragment_semantics() {
        // Tail, not head: JWT heads are a shared constant.
        assert_eq!(
            bearer_tail_fragment("eyJ0eXAiOiJh.shared-head.tail-distinct"),
            "ail-distinct"
        );
        assert_eq!(bearer_tail_fragment("abc"), "abc");
        assert_eq!(bearer_tail_fragment(""), "");
        assert_eq!(bearer_tail_fragment("123456789012"), "123456789012");
        // 13 multi-byte chars: a byte-index cut would land mid-char.
        assert_eq!(bearer_tail_fragment("ééééééééééééé"), "éééééééééééé");
    }
}
