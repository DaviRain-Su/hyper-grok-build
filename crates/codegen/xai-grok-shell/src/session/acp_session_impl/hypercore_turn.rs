//! Feature-flagged plain-chat turn via Hypercore + [`ShellHyperHost`].
//!
//! Enable with `HYPERCORE_TURN=1` (or `GROK_HYPERCORE_TURN=1`). On failure the
//! caller falls back to the legacy `process_conversation_turn` path.
//!
//! Tools / MCP / subagents are **not** handled here — only streaming text.

use super::*;
use xai_grok_sampling_types::{ContentPart, ConversationItem};
use xai_hyper_core::{CoreConfig, CoreEvent, HyperCore, TranscriptItem, TurnRequest};

/// Env gate for the Hypercore plain-turn bypass.
pub(super) fn hypercore_plain_turn_enabled() -> bool {
    for key in ["HYPERCORE_TURN", "GROK_HYPERCORE_TURN"] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim();
            if t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes") {
                return true;
            }
            if t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("no") {
                return false;
            }
        }
    }
    false
}

impl SessionActor {
    /// Run one plain-text turn through Hypercore. User message must already be
    /// in `chat_state`. Streams `AgentMessageChunk` updates; pushes assistant
    /// into chat_state on success.
    pub(super) async fn run_hypercore_plain_turn(
        &self,
        prompt_id: &str,
        user_text: &str,
    ) -> Result<TurnOutcome, acp::Error> {
        let session_id = self.session_info.id.0.to_string();
        let host = self.shell_hypercore_host().await;
        let model = host.sampling_config().model.clone();

        let mut core = HyperCore::restore_or_new(
            host,
            session_id.clone(),
            CoreConfig {
                model,
                max_messages: 256,
            },
        )
        .await
        .map_err(|e| {
            acp::Error::internal_error().data(format!("hypercore restore: {e}"))
        })?;

        // Seed from chat_state, excluding the trailing user message for this turn
        // (submit_turn will append it again).
        let conversation = self.chat_state_handle.get_conversation().await;
        let seeded = conversation_to_seed_items(&conversation, user_text);
        let completed = seeded
            .iter()
            .filter(|i| i.role == "assistant")
            .count() as u64;
        core.seed_transcript(seeded, completed);

        let turn_id = prompt_id.to_string();
        tracing::info!(
            session_id = %session_id,
            turn_id = %turn_id,
            "hypercore plain turn: submit"
        );

        let outcome = core
            .submit_turn(TurnRequest {
                turn_id: turn_id.clone(),
                text: user_text.to_string(),
            })
            .await
            .map_err(|e| {
                acp::Error::internal_error().data(format!("hypercore submit_turn: {e}"))
            })?;

        for ev in &outcome.events {
            match ev {
                CoreEvent::AssistantDelta { text, .. } if !text.is_empty() => {
                    self.send_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new(text.clone())),
                        )),
                        None,
                    )
                    .await;
                }
                CoreEvent::TurnFailed { error, .. } => {
                    return Err(acp::Error::internal_error().data(error.clone()));
                }
                _ => {}
            }
        }

        // Authoritative chat_state already has the user message; add assistant.
        // On idempotent replay the assistant may already be in chat_state — still
        // safe to append a duplicate only if history does not end with the same text.
        if !trailing_assistant_matches(&conversation, &outcome.assistant_text) {
            self.chat_state_handle
                .push_assistant_response(ConversationItem::assistant(
                    outcome.assistant_text.clone(),
                ));
        }

        tracing::info!(
            session_id = %session_id,
            turn_id = %turn_id,
            replayed = outcome.replayed,
            bytes = outcome.assistant_text.len(),
            "hypercore plain turn: committed"
        );

        Ok(TurnOutcome::Completed {
            snapshot: Box::new(None),
            tools_called: Vec::new(),
            structured_output: None,
            refusal: None,
        })
    }
}

/// Convert chat_state conversation into hypercore seed items, dropping the
/// trailing user message when it matches `current_user_text`.
fn conversation_to_seed_items(
    conversation: &[ConversationItem],
    current_user_text: &str,
) -> Vec<TranscriptItem> {
    let mut items: Vec<TranscriptItem> = conversation
        .iter()
        .filter_map(conversation_item_to_transcript)
        .collect();

    if let Some(last) = items.last()
        && last.role == "user"
        && last.content.trim() == current_user_text.trim()
    {
        items.pop();
    }
    items
}

fn conversation_item_to_transcript(item: &ConversationItem) -> Option<TranscriptItem> {
    match item {
        ConversationItem::System(s) => Some(TranscriptItem {
            role: "system".into(),
            content: s.content.to_string(),
        }),
        ConversationItem::User(u) => {
            let text = user_parts_to_text(&u.content);
            if text.is_empty() {
                None
            } else {
                Some(TranscriptItem {
                    role: "user".into(),
                    content: text,
                })
            }
        }
        ConversationItem::Assistant(a) => {
            let text = a.content.to_string();
            if text.is_empty() && a.tool_calls.is_empty() {
                None
            } else if text.is_empty() {
                // Skip tool-only assistant rows on the plain-chat bypass.
                None
            } else {
                Some(TranscriptItem {
                    role: "assistant".into(),
                    content: text,
                })
            }
        }
        _ => None,
    }
}

fn user_parts_to_text(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for p in parts {
        if let ContentPart::Text { text } = p {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

fn trailing_assistant_matches(conversation: &[ConversationItem], assistant_text: &str) -> bool {
    match conversation.last() {
        Some(ConversationItem::Assistant(a)) => a.content.as_ref() == assistant_text,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_drops_matching_trailing_user() {
        let conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
        ];
        let seeded = conversation_to_seed_items(&conv, "hello");
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].role, "system");
    }

    #[test]
    fn seed_keeps_history_when_user_differs() {
        let conv = vec![
            ConversationItem::user("old"),
            ConversationItem::assistant("reply"),
            ConversationItem::user("new"),
        ];
        let seeded = conversation_to_seed_items(&conv, "new");
        assert_eq!(seeded.len(), 2);
        assert_eq!(seeded[0].content, "old");
        assert_eq!(seeded[1].content, "reply");
    }

    #[test]
    fn env_gate_parses() {
        // Don't assert global env; just ensure the function is callable.
        let _ = hypercore_plain_turn_enabled();
    }
}
