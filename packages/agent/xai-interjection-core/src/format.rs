/// Truncation threshold, matching the shell's large-prompt limit.
pub const LARGE_PROMPT_THRESHOLD: usize = 25_000;

pub const INTERJECTION_NOTE: &str = "The user sent a message while you were working:";

/// Trailing reminder so a mid-turn steer does not drop in-flight work.
const UNFINISHED_TASKS_REMINDER: &str =
    "Make sure to complete any unfinished tasks from previous turns.";

/// Wrap a user message in the canonical `<user_query>` envelope.
pub fn user_query(user_message: &str) -> String {
    format!(
        r#"<user_query>
{user_message}
</user_query>"#
    )
}

/// Wrap interjection text as a synthetic user message with a mid-turn note
/// and a reminder to finish in-flight work from earlier turns.
pub fn format_interjection(text: String) -> String {
    let truncated = if text.len() > LARGE_PROMPT_THRESHOLD {
        let end = text
            .char_indices()
            .take_while(|(i, _)| *i < LARGE_PROMPT_THRESHOLD)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(text.len());
        format!("{}... [truncated]", &text[..end])
    } else {
        text
    };

    format!(
        "{INTERJECTION_NOTE}\n{}\n{UNFINISHED_TASKS_REMINDER}",
        user_query(&truncated)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_in_user_query_with_midturn_note() {
        let out = format_interjection("stop and fix the test first".into());
        assert!(out.starts_with("The user sent a message while you were working:\n<user_query>\n"));
        assert!(out.contains("stop and fix the test first"));
        assert!(
            out.ends_with(&format!("</user_query>\n{UNFINISHED_TASKS_REMINDER}")),
            "unfinished-task reminder must follow the wrapped query, got: {out}"
        );
    }

    #[test]
    fn truncates_at_utf8_boundary() {
        let s = "é".repeat(LARGE_PROMPT_THRESHOLD);
        let out = format_interjection(s);
        assert!(out.contains("... [truncated]"));
        assert!(out.len() < LARGE_PROMPT_THRESHOLD + 200);
    }

    #[test]
    fn short_text_untouched() {
        let out = format_interjection("hi".into());
        assert!(!out.contains("[truncated]"));
        assert_eq!(
            out,
            "The user sent a message while you were working:\n<user_query>\nhi\n</user_query>\nMake sure to complete any unfinished tasks from previous turns."
        );
    }
}
