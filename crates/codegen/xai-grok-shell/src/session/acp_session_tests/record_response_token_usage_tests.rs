use super::support::*;
use super::*;
use xai_grok_sampling_types::{ConversationItem, ConversationResponse, TokenUsage};

fn response_with_usage(total_tokens: u32) -> ConversationResponse {
    ConversationResponse {
        items: vec![ConversationItem::assistant("ok")],
        stop_reason: None,
        usage: Some(TokenUsage {
            prompt_tokens: total_tokens.saturating_sub(50),
            completion_tokens: 50,
            total_tokens,

            reasoning_tokens: 0,
            cached_prompt_tokens: 0,
        }),
        cost_usd_ticks: None,
        message_chunks_emitted: 1,
        doom_loop_signals: Vec::new(),
        stop_message: None,
    }
}

fn response_without_usage() -> ConversationResponse {
    ConversationResponse {
        items: vec![ConversationItem::assistant("ok")],
        stop_reason: None,
        usage: None,
        cost_usd_ticks: None,
        message_chunks_emitted: 1,
        doom_loop_signals: Vec::new(),
        stop_message: None,
    }
}

/// `record_response_token_usage` must update `chat_state.total_tokens`
/// to the model-reported value. Without this call, `total_tokens`
/// stays frozen at the seed from `ChatState::new`, freezing
/// `/context` and corrupting resume restore (the original bug).
#[tokio::test(flavor = "current_thread")]
async fn updates_chat_state_total_tokens_from_response_usage() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let _sync = actor.chat_state_handle.get_total_tokens().await;
            assert_eq!(actor.chat_state_handle.get_total_tokens().await, 0);

            actor.record_response_token_usage(&response_with_usage(150_000), None);

            assert_eq!(actor.chat_state_handle.get_total_tokens().await, 150_000);
            let prompt = actor
                .chat_state_handle
                .try_get_prompt_usage()
                .await
                .expect("chat-state alive")
                .expect("prompt ledger opened");
            assert_eq!(prompt.totals.model_calls, 1);
            assert_eq!(prompt.totals.input_tokens, 149_950);
            assert!(prompt.totals.cost_usd_ticks.is_none());
            assert_eq!(
                actor
                    .chat_state_handle
                    .try_get_session_usage()
                    .await
                    .expect("chat-state actor alive")
                    .totals
                    .model_calls,
                1
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn preserves_total_tokens_when_response_has_no_usage() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(99_999, 256_000, 85, gateway_tx, persistence_tx).await;
            let _sync = actor.chat_state_handle.get_total_tokens().await;

            actor.record_response_token_usage(&response_without_usage(), None);

            assert_eq!(actor.chat_state_handle.get_total_tokens().await, 99_999);
        })
        .await;
}

/// Wire contract the pager + TUI renderers depend on:
/// `build_session_info().context.used` must reflect the model-reported
/// `total_tokens` after a turn, and `usage_pct` / `free_tokens` /
/// `message_tokens` must be server-computed (not derived by the renderer
/// via subtraction).
#[tokio::test(flavor = "current_thread")]
async fn build_session_info_used_reflects_recorded_response() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // Push a small non-system fixture (user + assistant + tool
            // result). Without non-system items `message_tokens` would
            // be 0 and the Bug F regression could slip through this
            // assertion. Bytes/4 of these strings is small but >0.
            // The trailing `_and_ack` flushes the actor mailbox so the
            // subsequent query sees the writes.
            actor
                .chat_state_handle
                .push_assistant_response(ConversationItem::assistant("hi there hi there hi there"));
            actor
                .chat_state_handle
                .push_tool_result(ConversationItem::tool_result(
                    "call-1",
                    "tool result body tool result body",
                ));
            actor
                .chat_state_handle
                .push_user_message_and_ack(ConversationItem::user("hello hello hello hello"))
                .await;

            actor.record_response_token_usage(&response_with_usage(120_000), None);

            let info = actor.build_session_info().await;
            assert_eq!(info.context.used, 120_000);
            assert_eq!(info.context.total, 256_000);
            // Server-computed: renderer no longer derives these.
            assert_eq!(info.context.free_tokens, 256_000 - 120_000);
            // 120_000 / 256_000 = 0.46875 -> 47 after rounding.
            assert_eq!(info.context.usage_pct, 47);
            // Bug F regression guard: bytes/4 of the non-system items
            // must be > 0. Subtraction-based formulas saturated this to
            // zero; the direct sum returns the real estimate.
            assert!(
                info.context.message_tokens > 0,
                "message_tokens should reflect non-system items, got {}",
                info.context.message_tokens,
            );
            assert!(
                info.context.usage_pct > 0,
                "usage_pct should be non-zero when used > 0",
            );
        })
        .await;
}

/// Shell sourcing seam for the fingerprint display gate:
/// `build_session_info` must populate `SessionInfoData.show_model_fingerprint`
/// from the catalog entry for the session's current model. Guards the slug-vs-key
/// bug: the catalog map is keyed by the config key (`"custom-catalog-id"`), which
/// differs from the routing slug (`"test"`, the harness sampling model) that
/// `build_session_info` reads from the sampling config. A direct `.get(slug)` would
/// miss the entry and wrongly yield false. Proven on a NON-coding slug so the flag
/// — not `is_coding_model_slug` — drives the value (the gate's coding-slug OR is
/// covered by the `acp_types` / pager `format_session_info` tests).
#[tokio::test(flavor = "current_thread")]
async fn build_session_info_sources_show_model_fingerprint_from_catalog() {
    use crate::agent::config::{ModelEntry, ModelInfo};

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // Catalog KEY ("custom-catalog-id") differs from the session's routing
            // SLUG ("test", the harness sampling model). Flag OFF → false.
            let mut entry = ModelEntry {
                info: ModelInfo::fallback("test"),
                api_key: None,
                env_key: None,
                auth_provider: None,
                api_base_url: None,
            };
            entry.info.show_model_fingerprint = false;
            actor
                .models_manager
                .insert_test_entry("custom-catalog-id", entry.clone());
            assert!(
                !actor.build_session_info().await.show_model_fingerprint,
                "non-coding slug without the catalog flag must yield false",
            );

            // Same entry with the flag ON → true. Exercises slug→catalog-key
            // resolution end-to-end; a direct slug `.get("test")` would miss the
            // entry keyed "custom-catalog-id" and regress to false.
            entry.info.show_model_fingerprint = true;
            actor
                .models_manager
                .insert_test_entry("custom-catalog-id", entry);
            assert!(
                actor.build_session_info().await.show_model_fingerprint,
                "catalog show_model_fingerprint=true must flow to SessionInfoData via slug→key resolution",
            );
        })
        .await;
}

/// `record_response_token_usage` must also stash the per-turn `TokenUsage`
/// in chat state so the next `PromptResponse._meta` can carry input/output
/// token counts to the bot's telemetry layer.
#[tokio::test(flavor = "current_thread")]
async fn stashes_per_turn_usage_in_chat_state() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // Baseline: no stashed usage.
            assert!(
                actor
                    .chat_state_handle
                    .get_last_turn_usage()
                    .await
                    .is_none()
            );

            // Use existing fixture: total=200_000 → prompt=199_950, completion=50.
            actor.record_response_token_usage(&response_with_usage(200_000), None);

            let stashed = actor
                .chat_state_handle
                .get_last_turn_usage()
                .await
                .expect("usage stashed by record_response_token_usage");
            assert_eq!(stashed.prompt_tokens, 199_950);
            assert_eq!(stashed.completion_tokens, 50);
            assert_eq!(stashed.total_tokens, 200_000);
        })
        .await;
}
