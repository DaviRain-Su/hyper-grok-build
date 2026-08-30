use super::*;
use crate::session::helpers::prepared_compaction_history::build_compaction_chat_history;

#[test]
fn sampler_state_keeps_exact_latest_prepared_items() {
    let mut state = SamplerState::default();
    let first = build_compaction_chat_history(vec![ConversationItem::user("first")], None, true, 0);
    let second =
        build_compaction_chat_history(vec![ConversationItem::user("second")], None, true, 0);
    state.record_attempt(&first);
    state.record_attempt(&second);

    assert_eq!(
        serde_json::to_value(state.last_attempted_items.unwrap()).unwrap(),
        serde_json::to_value(second.items).unwrap()
    );
}

#[test]
fn acp_error_message_reads_string_and_object_shaped_data() {
    let string_err = acp::Error::internal_error().data("compact failed: boom");
    assert_eq!(
        crate::sampling::error::acp_error_message(&string_err),
        "compact failed: boom"
    );
    let object_err = acp::Error::internal_error()
        .data(serde_json::json!({"kind": "compact_cancelled", "message": "compact cancelled"}));
    assert_eq!(
        crate::sampling::error::acp_error_message(&object_err),
        "compact cancelled"
    );
    let no_data = acp::Error::internal_error();
    assert_eq!(
        crate::sampling::error::acp_error_message(&no_data),
        "Internal error"
    );
}
