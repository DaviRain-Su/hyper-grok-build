use crate::sampling::Client as OaiCompatClient;
use crate::sampling::types::ChatRequestMessage;
use crate::sampling::{ConversationItem, ConversationRequest, Role};
use anyhow::Result;

pub fn build_transcript(messages: &[ConversationItem]) -> String {
    const MAX_CONTENT_BYTES: usize = 2000;

    if messages.is_empty() {
        return String::new();
    }

    // Pre-allocate with estimated capacity to avoid reallocations
    let estimated_capacity: usize = messages
        .iter()
        .map(|m| m.text_content().len().min(MAX_CONTENT_BYTES) + 16)
        .sum();
    let mut result = String::with_capacity(estimated_capacity);

    for m in messages {
        let role = match m.role() {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };

        let text_content = m.text_content();
        let content = text_content.trim();

        result.push('[');
        result.push_str(role);
        result.push_str("] ");

        if content.len() > MAX_CONTENT_BYTES {
            let end = floor_char_boundary(content, MAX_CONTENT_BYTES);
            result.push_str(&content[..end]);
            result.push_str("...");
        } else {
            result.push_str(content);
        }

        result.push_str("\n\n");
    }

    // Remove trailing newline to match original join behavior
    result.pop();
    result
}

/// Returns the largest valid UTF-8 character boundary index at or before `index`.
#[inline]
pub(super) fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        s.len()
    } else if s.is_char_boundary(index) {
        index
    } else {
        // UTF-8 characters are at most 4 bytes, back up at most 3 bytes
        let mut i = index;
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}

pub fn truncate_middle_words(s: &str, max_words: usize) -> (String, Option<usize>) {
    let words: Vec<&str> = s.split_whitespace().collect();
    let total = words.len();
    if total <= max_words || max_words == 0 {
        return (s.to_string(), None);
    }
    let left = max_words / 2;
    let right = max_words - left;
    let prefix = words[..left].join(" ");
    let suffix = words[total.saturating_sub(right)..].join(" ");
    let truncated_count = total.saturating_sub(left + right);
    let marker = format!("…{} words truncated…", truncated_count);
    (format!("{}\n{}\n{}", prefix, marker, suffix), Some(total))
}

pub async fn text_completion(
    sampling_client: &OaiCompatClient,
    system: ChatRequestMessage,
    user: ChatRequestMessage,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
) -> Result<String> {
    let mut request = ConversationRequest::from_items(vec![
        ConversationItem::from(system),
        ConversationItem::from(user),
    ]);
    request.temperature = temperature;
    request.max_output_tokens = max_tokens;
    let response = sampling_client.conversation_collect(request).await?;
    let text = response
        .assistant()
        .map(|a| a.content.as_ref().to_owned())
        .unwrap_or_default();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty response");
    }
    Ok(trimmed.to_string())
}

/// Build a prompt string from a template by injecting a truncated transcript and additional variables.
/// - Replaces `{{transcript}}` with a truncated transcript derived from `conversation`
/// - Applies each `(needle, value)` replacement from `extras` sequentially
pub fn build_prompt_from_template(
    conversation: &[ConversationItem],
    template: &str,
    word_budget: usize,
    extras: &[(&str, &str)],
) -> String {
    let transcript = build_transcript(conversation);
    let (transcript_text, _words) = truncate_middle_words(&transcript, word_budget);
    let mut prompt = template.replace("{{transcript}}", &transcript_text);
    for (needle, value) in extras {
        prompt = prompt.replace(needle, value);
    }
    prompt
}
