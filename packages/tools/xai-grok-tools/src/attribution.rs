//! 401 attribution: callback hook + shared helpers for tool HTTP clients.

use std::sync::Arc;

/// Bearer-fragment length shared across crate boundaries. The fragment
/// is the **last** N characters (the tail): JWT session bearers all share
/// the same base64 header, so only the tail distinguishes tokens. Mirrors
/// `token_suffix` in xai-grok-shell, which the shell's 401-attribution
/// event compares this fragment against.
pub const SENT_BEARER_PREFIX_LEN: usize = 12;

/// Which tool endpoint produced the 401.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConsumer {
    ImageGen,
    VideoGenStart,
    VideoGenPoll,
    WebSearch,
}

impl ToolConsumer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImageGen => "ImageGen",
            Self::VideoGenStart => "VideoGen.start",
            Self::VideoGenPoll => "VideoGen.poll",
            Self::WebSearch => "WebSearch",
        }
    }
}

/// 401 attribution callback. Shell wires this to emit telemetry.
pub trait Auth401AttributionCallback: Send + Sync + std::fmt::Debug {
    /// `sent_bearer_prefix` is truncated to [`SENT_BEARER_PREFIX_LEN`]
    /// before crossing this boundary. `None` = no bearer was sent.
    fn record_401(&self, consumer: ToolConsumer, sent_bearer_prefix: Option<&str>);
}

/// Shared, cheap-to-clone alias for the attribution callback.
pub type SharedAttributionCallback = Arc<dyn Auth401AttributionCallback>;

/// Record a 401 attribution event if a callback is wired. Truncates
/// the bearer to its [`SENT_BEARER_PREFIX_LEN`]-char tail before
/// crossing the trait boundary, so only the fragment is materialized
/// as an owned copy.
pub(crate) fn emit_401(
    callback: Option<&SharedAttributionCallback>,
    consumer: ToolConsumer,
    sent_bearer: Option<&str>,
) {
    if let Some(cb) = callback {
        let prefix = sent_bearer.map(|s| tail_fragment(s).to_string());
        cb.record_401(consumer, prefix.as_deref());
    }
}

/// Last [`SENT_BEARER_PREFIX_LEN`] characters of a bearer (see the
/// constant's doc for why the tail, not the head). Used by tool clients
/// before passing the bearer across the [`Auth401AttributionCallback`]
/// boundary. Counts chars from the end, so it cannot panic on a
/// non-char-boundary cut for non-ASCII input.
pub(crate) fn tail_fragment(s: &str) -> &str {
    match s.char_indices().rev().nth(SENT_BEARER_PREFIX_LEN - 1) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_fragment_semantics() {
        // The tail, not the head: heads are shared across xai keys/JWTs.
        assert_eq!(
            tail_fragment("xai-key-aaaaaaaaaaadistinct1"),
            "aaadistinct1"
        );
        assert_eq!(tail_fragment("abc"), "abc");
        assert_eq!(tail_fragment(""), "");
        assert_eq!(tail_fragment("123456789012"), "123456789012");
        // 13 multi-byte chars: a byte-index cut would land mid-char.
        assert_eq!(tail_fragment("ééééééééééééé"), "éééééééééééé");
    }

    #[test]
    fn tool_consumer_as_str_stable_identifiers() {
        assert_eq!(ToolConsumer::ImageGen.as_str(), "ImageGen");
        assert_eq!(ToolConsumer::VideoGenStart.as_str(), "VideoGen.start");
        assert_eq!(ToolConsumer::VideoGenPoll.as_str(), "VideoGen.poll");
        assert_eq!(ToolConsumer::WebSearch.as_str(), "WebSearch");
    }
}
