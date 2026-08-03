//! Pure text-classification helpers shared by the memory flush
//! (`session::helpers::memory_flush`) and dream (`session::memory::dream`)
//! response-processing paths.
//!
//! These live here, in the memory subsystem, so `dream` no longer reaches
//! *up* into `session::helpers::memory_flush` for them — which removes the
//! `dream` <-> `memory_flush` module dependency cycle and is a prerequisite
//! for extracting the memory subsystem into its own crate.

/// Check if text contains at least one markdown header (`#` or `##`).
///
/// Used by both flush and dream response processing to ensure the model
/// produced structured output.
pub fn has_markdown_headers(text: &str) -> bool {
    text.contains("## ") || text.contains("# ")
}

/// Check if the response matches the NO_REPLY convention.
///
/// Strips all non-alphanumeric characters, lowercases, and checks if the
/// remainder is exactly `"noreply"`. This handles common separator variants:
/// `"no reply"`, `"no_reply"`, `"no-reply"`, `"NO REPLY"`, etc.
pub fn is_no_reply(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    normalized == "noreply"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_no_reply() {
        assert!(is_no_reply("NO_REPLY"));
        assert!(is_no_reply("no reply"));
        assert!(is_no_reply("No-Reply"));
        assert!(is_no_reply("noreply"));
        assert!(!is_no_reply("no reply needed"));
        assert!(!is_no_reply("I have things to store"));
    }

    #[test]
    fn test_has_markdown_headers() {
        assert!(has_markdown_headers("## Topic"));
        assert!(has_markdown_headers("# Title\n\nBody"));
        assert!(has_markdown_headers("preamble\n\n## Topic"));
        assert!(!has_markdown_headers("plain text without headers"));
        assert!(!has_markdown_headers("#hashtag without space"));
    }
}
