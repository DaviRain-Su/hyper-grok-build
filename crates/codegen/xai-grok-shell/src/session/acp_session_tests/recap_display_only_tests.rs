//! Regression tests for the session-recap display-only invariant.
//!
//! A recap must NEVER mutate the model conversation — it is generated from a
//! read-only snapshot and surfaced as a notification only. These tests lock
//! that contract: after `handle_recap` returns, `get_conversation()` must be
//! byte-identical to what it was before.

use super::support::*;
use super::*;
use xai_grok_sampling_types::ConversationItem;

#[tokio::test(flavor = "current_thread")]
async fn new_prompt_cancels_in_flight_recap_epoch() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            let epoch0 = actor.recap_epoch.get();
            assert!(!actor.recap_was_cancelled(epoch0));

            actor.cancel_pending_recap_for_new_prompt();
            assert!(
                actor.recap_was_cancelled(epoch0),
                "bumping epoch cancels a recap that captured the prior value"
            );
            let epoch1 = actor.recap_epoch.get();
            assert_eq!(epoch1, epoch0.wrapping_add(1));
            assert!(
                !actor.recap_was_cancelled(epoch1),
                "a recap that captures after the bump is still live"
            );
        })
        .await;
}

/// `queue_input` for a real user prompt bumps epoch before any await so a
/// LocalSet recap cannot commit after Prompt accept but before handle_prompt.
#[tokio::test(flavor = "current_thread")]
async fn queue_input_user_prompt_bumps_recap_epoch() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            let epoch0 = actor.recap_epoch.get();
            let (respond_to, _rx) = tokio::sync::oneshot::channel();
            actor
                .queue_input(
                    vec![],
                    "user-next".to_string(),
                    crate::session::plan_mode::PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    None,
                    /*tool_overrides_update*/ None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(
                actor.recap_was_cancelled(epoch0),
                "user queue_input must invalidate in-flight recap epoch"
            );
        })
        .await;
}

/// Synthetic auto-wake must not cancel an in-flight recap.
#[tokio::test(flavor = "current_thread")]
async fn queue_input_synthetic_does_not_bump_recap_epoch() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            let epoch0 = actor.recap_epoch.get();
            let (respond_to, _rx) = tokio::sync::oneshot::channel();
            actor
                .queue_input(
                    vec![],
                    "task-completed-bg-1".to_string(),
                    crate::session::plan_mode::PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    None,
                    /*tool_overrides_update*/ None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert_eq!(
                actor.recap_epoch.get(),
                epoch0,
                "synthetic queue_input must leave recap epoch alone"
            );
            assert!(!actor.recap_was_cancelled(epoch0));
        })
        .await;
}

/// Production commit branch: epoch bump mid-flight → no watermark, in-flight cleared.
#[tokio::test(flavor = "current_thread")]
async fn try_commit_recap_cancelled_clears_in_flight_without_watermark() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            actor.last_recap_main_turn.set(2);
            actor.recap_in_flight.set(true);
            let epoch = actor.recap_epoch.get();
            actor.cancel_pending_recap_for_new_prompt();

            assert!(
                !actor.try_commit_recap(epoch, 7),
                "stale epoch must not commit"
            );
            assert_eq!(
                actor.last_recap_main_turn.get(),
                2,
                "cancelled recap must not advance watermark"
            );
            assert!(
                !actor.recap_in_flight.get(),
                "cancelled recap must clear recap_in_flight"
            );
        })
        .await;
}

/// Live epoch commits watermark and clears in-flight (emit path may proceed).
#[tokio::test(flavor = "current_thread")]
async fn try_commit_recap_live_advances_watermark() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            actor.last_recap_main_turn.set(2);
            actor.recap_in_flight.set(true);
            let epoch = actor.recap_epoch.get();

            assert!(actor.try_commit_recap(epoch, 7));
            assert_eq!(actor.last_recap_main_turn.get(), 7);
            assert!(!actor.recap_in_flight.get());
        })
        .await;
}

/// Auto cancel is silent; manual cancel emits SessionRecapUnavailable.
#[tokio::test(flavor = "current_thread")]
async fn drop_recap_after_cancel_auto_silent_manual_unavailable() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            actor.recap_in_flight.set(true);
            actor.drop_recap_after_cancel(true).await;
            assert!(!actor.recap_in_flight.get());
            assert!(
                !drained_recap_unavailable(&mut persistence_rx),
                "auto cancel must not emit SessionRecapUnavailable"
            );
            assert!(
                !drained_session_recap(&mut persistence_rx),
                "auto cancel must not emit SessionRecap"
            );

            actor.recap_in_flight.set(true);
            actor.drop_recap_after_cancel(false).await;
            assert!(!actor.recap_in_flight.get());
            assert!(
                drained_recap_unavailable(&mut persistence_rx),
                "manual cancel must emit SessionRecapUnavailable"
            );
            assert!(
                !drained_session_recap(&mut persistence_rx),
                "manual cancel must not emit SessionRecap"
            );
        })
        .await;
}

/// Drain whether a `SessionRecap` update was emitted.
fn drained_session_recap(rx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>) -> bool {
    let mut saw = false;
    while let Ok(msg) = rx.try_recv() {
        if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n)) = msg
            && matches!(
                n.update,
                crate::extensions::notification::SessionUpdate::SessionRecap { .. }
            )
        {
            saw = true;
        }
    }
    saw
}

/// Auto recap below `MIN_TURNS_FOR_AUTO_RECAP` is a no-op and display-only.
#[tokio::test(flavor = "current_thread")]
async fn auto_recap_below_min_turns_is_noop_and_display_only() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::user("explain the borrow checker"),
                ConversationItem::assistant("it enforces shared-xor-mutable"),
            ]);
            let before = actor.chat_state_handle.get_conversation().await;
            assert_eq!(
                before.len(),
                2,
                "seed must be applied before the recap call"
            );

            actor.handle_recap(true).await;

            let after = actor.chat_state_handle.get_conversation().await;
            assert_eq!(
                serde_json::to_string(&before).unwrap(),
                serde_json::to_string(&after).unwrap(),
                "a gated auto recap must not mutate the conversation"
            );
            assert!(
                persistence_rx.try_recv().is_err(),
                "a gated auto recap must emit no notification"
            );
        })
        .await;
}

/// A manual `/recap` passes the gate (when a new main turn exists) and attempts
/// generation; the test's base_url is unreachable so the model call fails.
/// Either way the conversation must be byte-identical afterwards — display-only.
#[tokio::test(flavor = "current_thread")]
async fn manual_recap_never_mutates_conversation() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("you are a coding agent"),
                ConversationItem::user("explain the borrow checker"),
                ConversationItem::assistant("it enforces shared-xor-mutable"),
            ]);
            let before = actor.chat_state_handle.get_conversation().await;
            assert_eq!(
                before.len(),
                3,
                "seed must be applied before the recap call"
            );

            actor.handle_recap(false).await;

            let after = actor.chat_state_handle.get_conversation().await;
            assert_eq!(
                serde_json::to_string(&before).unwrap(),
                serde_json::to_string(&after).unwrap(),
                "manual recap must be display-only"
            );
        })
        .await;
}

/// Drain the persistence channel and report whether a `SessionRecapUnavailable`
/// xAI update was emitted.
fn drained_recap_unavailable(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) -> bool {
    let mut saw = false;
    while let Ok(msg) = rx.try_recv() {
        if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n)) = msg
            && matches!(
                n.update,
                crate::extensions::notification::SessionUpdate::SessionRecapUnavailable
            )
        {
            saw = true;
        }
    }
    saw
}

/// A manual `/recap` on a brand-new session (no main turns yet) must NOT strand
/// the client's loading spinner: the recap gate skips before any model call, but
/// instead of silently dropping, the shell emits `SessionRecapUnavailable` so
/// the client can clear it. Deterministic (no-network) repro of the
/// forever-spinner bug.
#[tokio::test(flavor = "current_thread")]
async fn manual_recap_with_no_turns_emits_unavailable() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // No main (user) turns — the gate skips before any model call.
            actor.chat_state_handle.replace_conversation(vec![]);

            actor.handle_recap(false).await;

            assert!(
                drained_recap_unavailable(&mut persistence_rx),
                "manual recap with no turns must emit SessionRecapUnavailable"
            );
        })
        .await;
}

/// A manual `/recap` whose generation fails (the test's base_url is
/// unreachable, so the prepare/model call errors) must also emit
/// `SessionRecapUnavailable` rather than leaving the spinner running.
#[tokio::test(flavor = "current_thread")]
async fn manual_recap_generation_failure_emits_unavailable() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // One main (user) turn clears the recap gate, so the failure comes
            // from the (unreachable) prepare/model call rather than the gate.
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("you are a coding agent"),
                ConversationItem::user("explain the borrow checker"),
                ConversationItem::assistant("it enforces shared-xor-mutable"),
            ]);

            actor.handle_recap(false).await;

            assert!(
                drained_recap_unavailable(&mut persistence_rx),
                "a failed manual recap must emit SessionRecapUnavailable"
            );
        })
        .await;
}

/// When the recap model call is attempted (gate passes) but fails, we still
/// persist a `RecapRequest` artifact (with `error` set) for offline replay —
/// same idea as compaction request artifacts on failure.
#[tokio::test(flavor = "current_thread")]
async fn manual_recap_generation_failure_persists_request_artifact() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("you are a coding agent"),
                ConversationItem::user("explain the borrow checker"),
                ConversationItem::assistant("it enforces shared-xor-mutable"),
            ]);

            actor.handle_recap(false).await;

            let mut saw_recap_request = false;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::RecapRequest(artifact) = msg {
                    assert_eq!(artifact.trigger, "manual");
                    assert!(
                        artifact.error.is_some(),
                        "failed recap must record error on the artifact"
                    );
                    assert!(
                        artifact.summary.is_none(),
                        "failed recap must not invent a summary"
                    );
                    assert!(
                        !artifact.chat_history.is_empty(),
                        "artifact must include the recap request items"
                    );
                    assert!(
                        artifact.x_grok_req_id.starts_with("xai-recap-"),
                        "req id: {}",
                        artifact.x_grok_req_id
                    );
                    saw_recap_request = true;
                }
            }
            assert!(
                saw_recap_request,
                "failed recap must enqueue PersistenceMsg::RecapRequest"
            );
        })
        .await;
}

/// An automatic recap below the turn gate stays silent — it shows no spinner,
/// so it must NOT emit `SessionRecapUnavailable` (which would be wasted wire
/// traffic and could clear an unrelated manual spinner on another client).
#[tokio::test(flavor = "current_thread")]
async fn auto_recap_gated_does_not_emit_unavailable() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // One main turn (below the auto min-turns gate): the auto path is
            // gated and shows no spinner, so it must stay silent.
            actor
                .chat_state_handle
                .replace_conversation(vec![ConversationItem::user("hi, nothing yet")]);

            actor.handle_recap(true).await;

            assert!(
                !drained_recap_unavailable(&mut persistence_rx),
                "a gated auto recap must not emit SessionRecapUnavailable"
            );
        })
        .await;
}

/// Over-budget recap: the persisted `RecapRequest` is trimmed within budget and
/// the conversation is left unmutated (display-only). Seeds an oversized item so
/// the over-budget branch runs deterministically with no network.
#[tokio::test(flavor = "current_thread")]
async fn manual_recap_over_budget_trims_persisted_request_and_is_display_only() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            // window 8_000 => prompt_budget = 8_000 * 85 / 100 - 4_000 = 2_800.
            const PROMPT_BUDGET: u64 = 8_000 * 85 / 100 - 4_000;
            let actor = create_test_actor(0, 8_000, 85, gateway_tx, persistence_tx).await;

            // An oversized real user turn (~40 KB => ~10k est tokens) forces the
            // over-budget branch regardless of the harness `total_tokens` arg.
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("you are a coding agent"),
                ConversationItem::user("x".repeat(40_000)),
            ]);
            let before = actor.chat_state_handle.get_conversation().await;

            actor.handle_recap(false).await;

            // Display-only: the conversation is byte-identical afterwards.
            let after = actor.chat_state_handle.get_conversation().await;
            assert_eq!(
                serde_json::to_string(&before).unwrap(),
                serde_json::to_string(&after).unwrap(),
                "an over-budget recap must not mutate the conversation"
            );

            // The model call fails (unreachable base_url) → the error arm persists
            // the (trimmed) request artifact.
            let mut saw_recap_request = false;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::RecapRequest(artifact) = msg {
                    let est = xai_chat_state::estimate_conversation_tokens(&artifact.chat_history);
                    assert!(
                        est <= PROMPT_BUDGET,
                        "persisted recap request must be within budget: {est} > {PROMPT_BUDGET}"
                    );
                    assert!(
                        !artifact.chat_history.is_empty(),
                        "the trimmed recap request must be non-empty"
                    );
                    assert!(
                        matches!(
                            artifact.chat_history.last(),
                            Some(ConversationItem::User(_))
                        ),
                        "the recap request must end with the appended User instruction"
                    );
                    saw_recap_request = true;
                }
            }
            assert!(
                saw_recap_request,
                "an over-budget recap must still enqueue a trimmed RecapRequest artifact"
            );
        })
        .await;
}

/// Over-budget recap serializes to a well-formed Anthropic Messages payload:
/// system preserved, reasoning stripped, no dangling `tool_use`/`tool_result`, no
/// `tool_result` before the appended instruction. (Messages is the strictest
/// shape, so it also covers the laxer grok ChatCompletions/Responses shapes.)
#[test]
fn over_budget_recap_serializes_to_well_formed_messages_request() {
    use crate::session::helpers::session_recap;
    use xai_grok_sampling_types::messages::{ContentBlock, MessageContent, MessageRole};
    use xai_grok_sampling_types::{ConversationRequest, ToolCall, rs};

    let mk_reasoning = |id: &str| {
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: id.to_string(),
            summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text: format!("secret thinking {id}"),
            })],
            content: None,
            encrypted_content: None,
            status: None,
        })
    };
    let mk_call = |id: &str| ToolCall {
        id: std::sync::Arc::from(id),
        name: "read_file".into(),
        arguments: std::sync::Arc::from("{}"),
    };

    // Over-budget (window 8_000) conversation that ENDS in a tool run and carries
    // reasoning; a valid interior tool pair sits behind a non-tool barrier so it
    // survives the trim.
    let conv = vec![
        ConversationItem::system("you are a coding agent"),
        ConversationItem::user("o".repeat(60_000)), // oldest, dropped by trim
        mk_reasoning("r1"),
        ConversationItem::assistant_tool_calls(vec![mk_call("c1")]),
        ConversationItem::tool_result("c1", "fn main() {}"),
        ConversationItem::assistant("done reading the parser"), // non-tool barrier
        ConversationItem::user("what did you change?"),
        ConversationItem::assistant_tool_calls(vec![mk_call("c2")]), // trailing run
        ConversationItem::tool_result("c2", "z".repeat(40_000)),     // trailing run
    ];

    // grok backend => strip_reasoning=false; the over-budget branch strips anyway.
    let items = session_recap::budget_recap_items(conv, "system-reminder", false, 8_000);
    let req = ConversationRequest::from_items(items);
    let msg = xai_grok_sampling_types::build_messages_request(&req);

    assert!(msg.system.is_some(), "system prompt must be preserved");

    // Flatten every content block across all messages (each message's content is
    // a `Blocks` vec here).
    let all_blocks: Vec<ContentBlock> = msg
        .messages
        .iter()
        .flat_map(|m| match &m.content {
            MessageContent::Blocks(b) => b.clone(),
            MessageContent::Text(_) => Vec::new(),
        })
        .collect();

    // Reasoning stripped: no thinking block anywhere.
    assert!(
        !all_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking { .. })),
        "over-budget branch must strip reasoning (no thinking blocks)"
    );

    // Last message is the appended user instruction: role user, text-only.
    let last = msg.messages.last().expect("messages must be non-empty");
    assert!(matches!(last.role, MessageRole::User));
    assert!(
        matches!(&last.content, MessageContent::Blocks(b)
            if b.iter().all(|blk| matches!(blk, ContentBlock::Text { .. }))),
        "the appended instruction message must be text-only (no tool_result/tool_use)"
    );

    // No dangling tool_use: every tool_use id has a matching tool_result id.
    let mut tool_use_ids = std::collections::HashSet::new();
    let mut tool_result_ids = std::collections::HashSet::new();
    for b in &all_blocks {
        match b {
            ContentBlock::ToolUse { id, .. } => {
                tool_use_ids.insert(id.clone());
            }
            ContentBlock::ToolResult { tool_use_id, .. } => {
                tool_result_ids.insert(tool_use_id.clone());
            }
            _ => {}
        }
    }
    assert!(
        tool_use_ids.is_subset(&tool_result_ids),
        "no dangling tool_use: uses={tool_use_ids:?} results={tool_result_ids:?}"
    );
}

/// Recap wire shape: main-turn tools + `prompt_cache_key` = session id, so the
/// request rides the parent turn's prefix cache instead of cold-prefilling.
#[tokio::test(flavor = "current_thread")]
async fn recap_request_rides_parent_prompt_cache() {
    use xai_grok_test_support::MockInferenceServer;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            // Register a real tool so the "recap sends the main turn's tools"
            // assertion is non-vacuous.
            *actor.agent.borrow_mut() = test_agent_with_goal_tool().await;

            let server = MockInferenceServer::start().await.unwrap();
            server.set_response("You asked about the borrow checker.");
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = server.url();
            cfg.api_backend = xai_grok_sampling_types::ApiBackend::Responses;
            actor.chat_state_handle.update_sampling_config(cfg);

            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("you are a coding agent"),
                ConversationItem::user("explain the borrow checker"),
                ConversationItem::assistant("it enforces shared-xor-mutable"),
            ]);

            actor.handle_recap(false).await;

            assert!(
                server.has_responses_request(),
                "recap must hit /v1/responses"
            );
            let requests = server.requests();
            let recap_req = requests
                .iter()
                .rev()
                .find(|r| r.path.contains("responses"))
                .expect("a responses request must be recorded");

            let conv_id = recap_req
                .header("x-grok-conv-id")
                .expect("recap must send x-grok-conv-id");
            assert!(
                conv_id.starts_with("recap-"),
                "conv id keeps the recap-* label: {conv_id}"
            );

            let body = recap_req.body.as_ref().expect("recap body must be JSON");
            assert_eq!(
                body["prompt_cache_key"].as_str(),
                Some(actor.session_info.id.to_string().as_str()),
                "prompt_cache_key must be the parent session id for sticky routing"
            );
            let main_turn_specs =
                actor.turn_base_tool_specs(&actor.prepare_tool_definitions().await);
            assert!(!main_turn_specs.is_empty(), "test env must expose tools");
            let tools = body["tools"].as_array().expect("tools must be present");
            assert_eq!(
                tools.len(),
                main_turn_specs.len(),
                "recap must send exactly the main turn's tool specs"
            );
        })
        .await;
}

/// Hosted tools serialize into the token prefix on the Responses path, so a recap in a backend-search session must send the main turn's hosted
/// tools or its prefix diverges and cold-misses the cache.
#[tokio::test(flavor = "current_thread")]
async fn recap_request_sends_hosted_tools_under_backend_search() {
    use xai_grok_sampling_types::HostedTool;
    use xai_grok_test_support::MockInferenceServer;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor.agent.borrow_mut() = test_agent_with_goal_tool().await;

            // Backend-search fixture: agent carries hosted tools and both gates are on.
            {
                let mut agent_slot = actor.agent.borrow_mut();
                let agent = &*agent_slot;
                *agent_slot = xai_grok_agent::Agent::new(
                    agent.definition().clone(),
                    agent.prompt_context().clone(),
                    agent.system_prompt().to_string(),
                    std::sync::Arc::clone(agent.tool_bridge()),
                    agent.reminder_policy().clone(),
                    agent.compaction_policy().clone(),
                    vec![HostedTool::WebSearch { options: None }],
                    true,
                );
            }
            actor.supports_backend_search.set(true);

            let server = MockInferenceServer::start().await.unwrap();
            server.set_response("You asked about the borrow checker.");
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = server.url();
            cfg.api_backend = xai_grok_sampling_types::ApiBackend::Responses;
            actor.chat_state_handle.update_sampling_config(cfg);

            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("you are a coding agent"),
                ConversationItem::user("explain the borrow checker"),
                ConversationItem::assistant("it enforces shared-xor-mutable"),
            ]);

            actor.handle_recap(false).await;

            let requests = server.requests();
            let recap_req = requests
                .iter()
                .rev()
                .find(|r| r.path.contains("responses"))
                .expect("a responses request must be recorded");
            let body = recap_req.body.as_ref().expect("recap body must be JSON");
            let tools = body["tools"].as_array().expect("tools must be present");

            assert!(
                tools
                    .iter()
                    .any(|t| t["type"].as_str() == Some("web_search")),
                "recap must send the main turn's hosted tools: {tools:?}"
            );
            // Function tools must still match the main turn's specs exactly.
            let main_turn_specs =
                actor.turn_base_tool_specs(&actor.prepare_tool_definitions().await);
            assert!(!main_turn_specs.is_empty(), "test env must expose tools");
            let function_tools = tools
                .iter()
                .filter(|t| t["type"].as_str() == Some("function"))
                .count();
            assert_eq!(
                function_tools,
                main_turn_specs.len(),
                "hosted tools augment, not replace, the main turn's function tools"
            );
        })
        .await;
}

/// A recap must serialize the main turn's *effective* hosted tools, so an active per-turn cutoff
/// reaches the recap's `x_search` entry rather than an unbounded tool.
#[tokio::test(flavor = "current_thread")]
async fn recap_hosted_tools_reflect_the_active_per_turn_override() {
    use xai_grok_sampling_types::{HostedTool, SearchDateBound, ToolOverrides, XSearchOptions};
    use xai_grok_test_support::MockInferenceServer;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _grx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _prx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor.agent.borrow_mut() = test_agent_with_goal_tool().await;

            // Backend-search fixture seeded with an *unbounded* x_search (options: None), so any
            // bound the recap sends can only have come from the per-turn override below.
            {
                let mut agent_slot = actor.agent.borrow_mut();
                let agent = &*agent_slot;
                *agent_slot = xai_grok_agent::Agent::new(
                    agent.definition().clone(),
                    agent.prompt_context().clone(),
                    agent.system_prompt().to_string(),
                    std::sync::Arc::clone(agent.tool_bridge()),
                    agent.reminder_policy().clone(),
                    agent.compaction_policy().clone(),
                    vec![HostedTool::XSearch { options: None }],
                    true,
                );
            }
            actor.supports_backend_search.set(true);

            // A per-turn cutoff (toDate only), with no definition seed: the recap must reflect it.
            *actor.tool_overrides.borrow_mut() = Some(ToolOverrides {
                x_search: Some(XSearchOptions {
                    date_bound: Some(
                        SearchDateBound::new(None, Some("2024-03-15".to_string())).unwrap(),
                    ),
                }),
                web_search: None,
            });

            let server = MockInferenceServer::start().await.unwrap();
            server.set_response("recap summary");
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = server.url();
            cfg.api_backend = xai_grok_sampling_types::ApiBackend::Responses;
            actor.chat_state_handle.update_sampling_config(cfg);

            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("you are a coding agent"),
                ConversationItem::user("explain the borrow checker"),
                ConversationItem::assistant("it enforces shared-xor-mutable"),
            ]);

            actor.handle_recap(false).await;

            let requests = server.requests();
            let recap_req = requests
                .iter()
                .rev()
                .find(|r| r.path.contains("responses"))
                .expect("a responses request must be recorded");
            let body = recap_req.body.as_ref().expect("recap body must be JSON");
            let tools = body["tools"].as_array().expect("tools must be present");
            let x_search = tools
                .iter()
                .find(|t| t["type"].as_str() == Some("x_search"))
                .expect("recap must send the x_search hosted tool");
            assert_eq!(
                x_search["to_date"].as_str(),
                Some("2024-03-15"),
                "recap must serialize the per-turn override's cutoff, not the unbounded seed: {x_search:?}"
            );
        })
        .await;
}
