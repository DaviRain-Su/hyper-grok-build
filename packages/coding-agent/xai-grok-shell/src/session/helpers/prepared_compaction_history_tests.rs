use super::*;
use xai_grok_sampling_types::ContentPart;

fn data_image(bytes: usize) -> ContentPart {
    let prefix = "data:image/png;base64,";
    ContentPart::Image {
        url: format!("{prefix}{}", "A".repeat(bytes - prefix.len())).into(),
    }
}

#[test]
fn reserved_tool_headroom_triggers_small_history_once() {
    let source = vec![ConversationItem::user_with_parts(vec![data_image(500)])];
    let unreserved = build_compaction_chat_history(source.clone(), None, true, 0);
    let effective_trigger = unreserved.image_budget.body_bytes.saturating_sub(1);
    let reserved_bytes = IMAGE_COMPACT_TRIGGER_BYTES.saturating_sub(effective_trigger);
    let reserved_tokens = u64::try_from(reserved_bytes.div_ceil(4)).unwrap();
    let prepared = build_compaction_chat_history(source, None, true, reserved_tokens);

    assert!(!unreserved.image_budget.needs_image_compaction);
    assert!(prepared.image_budget.needs_image_compaction);
    assert_eq!(prepared.image_budget.evicted, 1);
    assert_eq!(
        prepared.image_budget.body_bytes_after,
        serde_json::to_vec(&prepared.items).unwrap().len()
    );

    let expected_items = serde_json::to_value(&prepared.items).unwrap();
    let expected_budget = prepared.image_budget;
    let final_boundary = CompactionHistoryInput::from(prepared).prepare(u64::MAX);
    assert_eq!(final_boundary.image_budget, expected_budget);
    assert_eq!(
        serde_json::to_value(final_boundary.items).unwrap(),
        expected_items
    );
}

#[test]
fn no_image_history_is_unchanged_before_prompt() {
    let source = vec![
        ConversationItem::system("system text"),
        ConversationItem::user("user text"),
        ConversationItem::assistant("assistant text"),
        ConversationItem::tool_result("call-1", "tool text"),
    ];
    let source_serialized = serde_json::to_value(&source).unwrap();
    let request = build_compaction_chat_history(source.clone(), None, true, 0);

    assert_eq!(request.image_budget.inline_images, 0);
    assert_eq!(
        serde_json::to_value(&request.items[..source.len()]).unwrap(),
        source_serialized
    );
}
