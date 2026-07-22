//! Server-authoritative prompt queue.
use super::support::*;
use super::*;

/// The shared-queue text must use a block's compact `displayText` (e.g. a
/// locally-expanded `/loop` invocation) rather than the raw expanded wire text,
/// so other clients' turn-start shim renders the compact user block — not the
/// full skill instruction.
#[test]
fn queue_text_prefers_display_text_over_raw_wire_text() {
    let block = acp::ContentBlock::Text(
        acp::TextContent::new(
            "# /loop -- schedule a recurring prompt\n\nParse the input ...".to_string(),
        )
        .meta(
            serde_json::json!({ "displayText": "/loop 5s echo \"hello\"" })
                .as_object()
                .cloned(),
        ),
    );
    assert_eq!(
        SessionActor::queue_text_from_blocks(&[block]),
        "/loop 5s echo \"hello\""
    );
}

#[test]
fn queue_text_falls_back_to_raw_text_without_display_text() {
    let block = acp::ContentBlock::Text(acp::TextContent::new("just a normal prompt".to_string()));
    assert_eq!(
        SessionActor::queue_text_from_blocks(&[block]),
        "just a normal prompt"
    );

    // An empty displayText is ignored — fall back to the raw text.
    let block_empty = acp::ContentBlock::Text(
        acp::TextContent::new("raw text".to_string()).meta(
            serde_json::json!({ "displayText": "   " })
                .as_object()
                .cloned(),
        ),
    );
    assert_eq!(
        SessionActor::queue_text_from_blocks(&[block_empty]),
        "raw text"
    );
}

fn ids(wire: &[crate::session::prompt_queue::QueueEntryWire]) -> Vec<String> {
    wire.iter().map(|e| e.id.clone()).collect()
}

/// A queued user bash item (`!cmd`), mirroring `queue_input`'s derivation.
fn bash_item(id: &str, owner: &str, command: &str) -> InputItem {
    let mut item = user_item(id, owner);
    let meta = serde_json::to_value(crate::extensions::prompt_meta::PromptBlockMeta::bash(
        command,
    ))
    .unwrap()
    .as_object()
    .cloned();
    item.prompt_blocks = vec![acp::ContentBlock::Text(
        acp::TextContent::new(command.to_string()).meta(meta),
    )];
    let meta = item.queue_meta.as_mut().unwrap();
    meta.kind = "bash".to_string();
    meta.text = command.to_string();
    item
}

#[test]
fn combine_front_merges_consecutive_plain_prompts() {
    use crate::session::commands::{PromptCompletionKind, PromptTurnOk};

    let (p1, _) = user_item_with_rx("p1", "A");
    let (p2, rx2) = user_item_with_rx("p2", "A");
    let (p3, rx3) = user_item_with_rx("p3", "A");
    let mut pending = std::collections::VecDeque::from([p1, p2, p3]);

    SessionActor::combine_front_pending_inputs(&mut pending, &[]);

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].prompt_id, "p1");
    let combined = "text for p1\n\ntext for p2\n\ntext for p3";
    assert_eq!(
        SessionActor::queue_text_from_blocks(&pending[0].prompt_blocks),
        combined
    );
    assert_eq!(
        pending[0].queue_meta.as_ref().map(|m| m.text.as_str()),
        Some(combined)
    );
    assert_eq!(
        pending[0]
            .queue_meta
            .as_ref()
            .and_then(|m| m.combined_texts.as_ref())
            .map(|v| v.as_slice()),
        Some(
            [
                "text for p1".to_string(),
                "text for p2".to_string(),
                "text for p3".to_string()
            ]
            .as_slice()
        )
    );
    // Content-block meta for echo/replay multi-bubble paint.
    let segs = pending[0]
        .prompt_blocks
        .first()
        .and_then(|b| match b {
            acp::ContentBlock::Text(t) => t.meta.as_ref(),
            _ => None,
        })
        .and_then(|m| m.get(crate::session::prompt_queue::COMBINED_DISPLAY_TEXTS_META))
        .and_then(|v| v.as_array())
        .expect("combinedDisplayTexts stamped");
    assert_eq!(
        segs.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>(),
        ["text for p1", "text for p2", "text for p3"]
    );
    for mut rx in [rx2, rx3] {
        assert!(matches!(
            rx.try_recv(),
            Ok(Ok(PromptTurnOk {
                completion_kind: PromptCompletionKind::RemovedFromQueue,
                ..
            }))
        ));
    }
}

#[test]
fn combine_front_stops_at_bash() {
    let mut pending = std::collections::VecDeque::from([
        user_item("p1", "A"),
        user_item("p2", "A"),
        bash_item("bash1", "A", "ls"),
        user_item("p3", "A"),
    ]);

    SessionActor::combine_front_pending_inputs(&mut pending, &[]);

    assert_eq!(pending.len(), 3);
    assert_eq!(
        SessionActor::queue_text_from_blocks(&pending[0].prompt_blocks),
        "text for p1\n\ntext for p2"
    );
    assert_eq!(pending[1].prompt_id, "bash1");
    assert_eq!(pending[2].prompt_id, "p3");
}

#[test]
fn combine_front_noop_when_ineligible() {
    let mut single = std::collections::VecDeque::from([user_item("only", "A")]);
    SessionActor::combine_front_pending_inputs(&mut single, &[]);
    assert_eq!(single.len(), 1);

    let mut bash_front =
        std::collections::VecDeque::from([bash_item("b", "A", "pwd"), user_item("p", "A")]);
    SessionActor::combine_front_pending_inputs(&mut bash_front, &[]);
    assert_eq!(bash_front.len(), 2);
    assert_eq!(bash_front[0].prompt_id, "b");
}

#[test]
fn combine_front_skips_client_expanded_skill() {
    let mut skill = user_item("skill", "A");
    skill.prompt_blocks = vec![acp::ContentBlock::Text(
        acp::TextContent::new("# expanded skill body".to_string()).meta(
            serde_json::json!({ "displayText": "/commit fix" })
                .as_object()
                .cloned(),
        ),
    )];
    let mut pending = std::collections::VecDeque::from([skill, user_item("follow", "A")]);

    SessionActor::combine_front_pending_inputs(&mut pending, &[]);

    assert_eq!(
        pending.len(),
        2,
        "displayText front must not absorb followers"
    );
}

#[test]
fn combine_front_skips_edit_hold() {
    let (p1, _) = user_item_with_rx("p1", "A");
    let (p2, mut rx2) = user_item_with_rx("p2", "A");
    let (p3, _) = user_item_with_rx("p3", "A");
    let mut pending = std::collections::VecDeque::from([p1, p2, p3]);
    SessionActor::combine_front_pending_inputs(&mut pending, &["p2"]);
    assert_eq!(pending.len(), 3, "edit-hold follower must not be absorbed");
    assert!(rx2.try_recv().is_err(), "held row must stay queued");
}

/// Two prompts arrive (serialized by the actor mailbox → FIFO); the agent
/// drains the front; an edit against the already-drained item is a benign
/// no-op that re-broadcasts the current queue; a stale-version edit is also
/// a no-op; a correct-version remove empties the queue.
#[tokio::test]
async fn two_enqueues_drain_fifo_and_stale_edit_is_noop() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx) = build_actor().await;

            // p1 then p2 (arrival order).
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "A"));
                actor.broadcast_queue_changed(&state);
                state.pending_inputs.push_back(user_item("p2", "B"));
                actor.broadcast_queue_changed(&state);
                assert_eq!(ids(&actor.build_queue_wire(&state)), vec!["p1", "p2"]);
            }

            // Agent drains the front (FIFO) — simulate turn-completion pop.
            {
                let mut state = actor.state.lock().await;
                let drained = state.pending_inputs.pop_front().unwrap();
                assert_eq!(drained.prompt_id, "p1");
                actor.broadcast_queue_changed(&state);
                assert_eq!(ids(&actor.build_queue_wire(&state)), vec!["p2"]);
            }

            // Edit against drained p1 → no-op + rebroadcast of [p2].
            actor.handle_remove_queued_prompt("p1", 0, None).await;
            {
                let state = actor.state.lock().await;
                assert_eq!(ids(&actor.build_queue_wire(&state)), vec!["p2"]);
            }

            // Stale-version edit against live p2 → no-op.
            actor.handle_remove_queued_prompt("p2", 99, None).await;
            {
                let state = actor.state.lock().await;
                assert_eq!(ids(&actor.build_queue_wire(&state)), vec!["p2"]);
            }

            // Correct-version remove → empties the queue.
            actor.handle_remove_queued_prompt("p2", 0, None).await;
            {
                let state = actor.state.lock().await;
                assert!(actor.build_queue_wire(&state).is_empty());
            }

            // The final broadcast must reflect the empty queue.
            let mut last: Option<crate::session::prompt_queue::QueueChanged> = None;
            while let Ok(msg) = gateway_rx.try_recv() {
                if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                    && args.request.method.as_ref()
                        == crate::session::prompt_queue::QUEUE_CHANGED_METHOD
                {
                    last = serde_json::from_str(args.request.params.get()).ok();
                }
            }
            let last = last.expect("at least one queue/changed broadcast");
            assert!(
                last.entries.is_empty(),
                "final broadcast must show empty queue"
            );
        })
        .await;
}

/// Owner-scoped clear removes only the requesting client's queued prompts.
#[tokio::test]
async fn clear_queue_is_owner_scoped() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("a1", "A"));
                state.pending_inputs.push_back(user_item("b1", "B"));
                state.pending_inputs.push_back(user_item("a2", "A"));
            }
            actor.handle_clear_queue(Some("A")).await;
            let state = actor.state.lock().await;
            assert_eq!(ids(&actor.build_queue_wire(&state)), vec!["b1"]);
        })
        .await;
}

/// Removing a queued prompt must resolve its in-flight `session/prompt` RPC
/// with `Cancelled` rather than dropping the `respond_to` sender. A bare drop
/// surfaces to the client as `RecvError` → "session failed to respond", which
/// — because the client's PromptResponse prompt-id gate only runs on the `Ok`
/// path — is misattributed to the running turn and rendered as a spurious
/// "Turn failed". Regression guard.
#[tokio::test]
async fn remove_queued_prompt_resolves_rpc_cancelled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            let (item, prompt_rx) = user_item_with_rx("p1", "A");
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(item);
            }

            actor.handle_remove_queued_prompt("p1", 0, None).await;

            let turn_result = prompt_rx
                .await
                .expect("respond_to must be fulfilled, not dropped");
            assert!(
                matches!(
                    turn_result,
                    Ok(crate::session::commands::PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        completion_kind: PromptCompletionKind::RemovedFromQueue,
                        ..
                    })
                ),
                "removed queued prompt must report RemovedFromQueue"
            );
        })
        .await;
}

/// In-place LWW edit: replacing the text of a queued prompt
/// bumps `version`, records `last_editor`, preserves the original `owner`, and
/// re-broadcasts. The underlying `prompt_blocks` is also rebuilt so the agent
/// runs the new text when the prompt is eventually drained.
#[tokio::test]
async fn edit_queued_prompt_replaces_text_and_bumps_version() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "alice"));
            }

            actor
                .handle_edit_queued_prompt("p1", "edited".into(), Some("bob"))
                .await;

            let state = actor.state.lock().await;
            let item = state
                .pending_inputs
                .iter()
                .find(|i| i.queue_meta.as_ref().is_some_and(|m| m.id == "p1"))
                .expect("p1 still in queue");
            let meta = item.queue_meta.as_ref().unwrap();
            assert_eq!(meta.text, "edited");
            assert_eq!(meta.version, 1, "edit bumps version");
            assert_eq!(meta.owner.as_deref(), Some("alice"), "owner preserved");
            assert_eq!(
                meta.last_editor.as_deref(),
                Some("bob"),
                "last_editor recorded"
            );

            // Underlying prompt_blocks was rebuilt with the new text.
            assert_eq!(item.prompt_blocks.len(), 1);
            match &item.prompt_blocks[0] {
                acp::ContentBlock::Text(t) => assert_eq!(t.text, "edited"),
                other => panic!("expected text block, got {other:?}"),
            }

            // The wire projection also reflects the new state.
            let wire = actor.build_queue_wire(&state);
            assert_eq!(wire.len(), 1);
            assert_eq!(wire[0].text, "edited");
            assert_eq!(wire[0].version, 1);
            assert_eq!(wire[0].owner.as_deref(), Some("alice"));
            assert_eq!(wire[0].last_editor.as_deref(), Some("bob"));
        })
        .await;
}

/// Two sequential edits — last write wins (the actor mailbox serializes them).
#[tokio::test]
async fn edit_queued_prompt_is_last_writer_wins() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "alice"));
            }

            actor
                .handle_edit_queued_prompt("p1", "first-write".into(), Some("bob"))
                .await;
            actor
                .handle_edit_queued_prompt("p1", "second-write".into(), Some("carol"))
                .await;

            let state = actor.state.lock().await;
            let meta = state
                .pending_inputs
                .iter()
                .find_map(|i| i.queue_meta.as_ref().filter(|m| m.id == "p1"))
                .expect("p1 still in queue");
            assert_eq!(meta.text, "second-write", "LWW: last edit wins");
            assert_eq!(meta.version, 2, "each edit bumps version");
            assert_eq!(meta.last_editor.as_deref(), Some("carol"));
            assert_eq!(meta.owner.as_deref(), Some("alice"), "owner unchanged");
        })
        .await;
}

/// Editing a missing id is a benign no-op (the entry was already drained or
/// removed by another client); no rebroadcast is required because nothing
/// changed.
#[tokio::test]
async fn edit_queued_prompt_missing_id_is_noop() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "alice"));
            }

            actor
                .handle_edit_queued_prompt("ghost", "ignored".into(), Some("bob"))
                .await;

            let state = actor.state.lock().await;
            let meta = state
                .pending_inputs
                .iter()
                .find_map(|i| i.queue_meta.as_ref().filter(|m| m.id == "p1"))
                .expect("p1 still in queue");
            // p1 untouched.
            assert_eq!(meta.text, "text for p1");
            assert_eq!(meta.version, 0);
            assert!(meta.last_editor.is_none());
        })
        .await;
}

/// Editing the currently-running turn is a no-op — the in-flight prompt is
/// out of scope for queue edits.
#[tokio::test]
async fn edit_queued_prompt_running_turn_is_noop() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "alice"));
                // Mark p1 as the running turn (race-free identity: the task
                // slot, not the `current_prompt_id` pin).
                state.running_task = Some(running_task_stub("p1"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p1".into());

            actor
                .handle_edit_queued_prompt("p1", "no-op".into(), Some("bob"))
                .await;

            let state = actor.state.lock().await;
            let meta = state
                .pending_inputs
                .iter()
                .find_map(|i| i.queue_meta.as_ref().filter(|m| m.id == "p1"))
                .expect("p1 still in queue");
            assert_eq!(meta.text, "text for p1", "running turn untouched");
            assert_eq!(meta.version, 0);
            assert!(meta.last_editor.is_none());
        })
        .await;
}

/// An edit with no editor (None) clears `last_editor` rather than preserving
/// the previous editor — the most recent edit's identity is what we want to
/// surface.
#[tokio::test]
async fn edit_queued_prompt_clears_last_editor_when_none() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "alice"));
            }

            actor
                .handle_edit_queued_prompt("p1", "v1".into(), Some("bob"))
                .await;
            actor
                .handle_edit_queued_prompt("p1", "v2".into(), None)
                .await;

            let state = actor.state.lock().await;
            let meta = state
                .pending_inputs
                .iter()
                .find_map(|i| i.queue_meta.as_ref().filter(|m| m.id == "p1"))
                .expect("p1 still in queue");
            assert_eq!(meta.text, "v2");
            assert_eq!(meta.version, 2);
            assert!(
                meta.last_editor.is_none(),
                "last_editor reflects the most recent edit"
            );
        })
        .await;
}

/// Owner-scoped clear must resolve every cleared prompt's RPC with `Cancelled`
/// (not drop it) — same failure mode as remove. Prompts owned by other clients
/// stay queued and keep their RPC pending.
#[tokio::test]
async fn clear_queue_resolves_cleared_rpcs_cancelled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            let (a1, a1_rx) = user_item_with_rx("a1", "A");
            let (b1, b1_rx) = user_item_with_rx("b1", "B");
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(a1);
                state.pending_inputs.push_back(b1);
            }

            actor.handle_clear_queue(Some("A")).await;

            // a1 (owned by A) was cleared → its RPC resolves Cancelled.
            let a1_result = a1_rx.await.expect("cleared prompt RPC must be resolved");
            assert!(
                matches!(
                    a1_result,
                    Ok(crate::session::commands::PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        completion_kind: PromptCompletionKind::RemovedFromQueue,
                        ..
                    })
                ),
                "cleared queued prompt must report RemovedFromQueue"
            );

            // b1 (owned by B) stays queued → its RPC is still pending.
            let state = actor.state.lock().await;
            assert_eq!(ids(&actor.build_queue_wire(&state)), vec!["b1"]);
            drop(b1_rx);
        })
        .await;
}

/// With NO turn running (e.g. the turn ended in the `Send now` race window) the
/// interject is a benign no-op: the prompt stays queued so it runs normally as
/// its own turn, and nothing is stranded in the interjection buffer.
#[tokio::test]
async fn interject_queued_prompt_noop_without_running_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "A"));
            }
            // current_prompt_id is None → no turn running.

            actor
                .handle_interject_queued_prompt("p1", 0, None, None)
                .await;

            let state = actor.state.lock().await;
            assert_eq!(
                ids(&actor.build_queue_wire(&state)),
                vec!["p1"],
                "prompt stays queued when no turn is running"
            );
            assert!(
                actor.pending_interjections.is_empty(),
                "nothing buffered without a running turn"
            );
        })
        .await;
}

/// A stale `expected_version` interject is a benign no-op (mirrors remove): the
/// prompt stays queued and no interjection is buffered.
#[tokio::test]
async fn interject_queued_prompt_stale_version_noop() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "A"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());

            // Version 99 != live version 0 → no-op.
            actor
                .handle_interject_queued_prompt("p1", 99, None, None)
                .await;

            let state = actor.state.lock().await;
            assert_eq!(ids(&actor.build_queue_wire(&state)), vec!["p1"]);
            assert!(actor.pending_interjections.is_empty());
        })
        .await;
}

/// Interject in the cancel gap (turn cleared, next prompt not started) must do nothing: no buffer
/// into `pending_interjections` (no drain), prompt stays queued to run alone; queue rebroadcasts.
#[tokio::test]
async fn interject_after_cancel_does_nothing_and_keeps_prompt_queued() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("running", "A"));
                state.running_task = Some(running_task_stub("running"));
                state.pending_inputs.push_back(user_item("p1", "A"));
                state.running_task = Some(AgentTask {
                    prompt_id: "running".into(),
                    handle: tokio::task::spawn_local(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                });
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());

            actor.cancel_running_task(true, false, false, None).await;

            // p1 would start next; the interject lands in the gap where no turn runs yet.
            actor
                .handle_interject_queued_prompt("p1", 0, None, None)
                .await;

            let state = actor.state.lock().await;
            assert_eq!(
                ids(&actor.build_queue_wire(&state)),
                vec!["p1"],
                "prompt must stay queued (it runs as its own turn)"
            );
            assert!(
                actor.pending_interjections.is_empty(),
                "no interjection may buffer without a running turn (nothing would drain it)"
            );
            drop(state);

            // The interject no-op still rebroadcasts so clients reconcile.
            let mut saw_broadcast = false;
            while let Ok(msg) = gateway_rx.try_recv() {
                if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                    && args.request.method.as_ref()
                        == crate::session::prompt_queue::QUEUE_CHANGED_METHOD
                {
                    saw_broadcast = true;
                }
            }
            assert!(saw_broadcast, "interject no-op must rebroadcast the queue");
        })
        .await;
}

/// Turn ended before the edited interject landed (the `Send now` race): the
/// edit is saved to the queued row as an LWW write — nothing is buffered, but
/// the row drains later with the EDITED text instead of silently reverting.
#[tokio::test]
async fn interject_queued_prompt_with_new_text_no_running_turn_saves_edit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                let mut item = user_item("p1", "A");
                item.prompt_blocks
                    .push(acp::ContentBlock::Image(test_image_content()));
                state.pending_inputs.push_back(item);
            }
            // current_prompt_id is None → no turn running.

            actor
                .handle_interject_queued_prompt("p1", 0, None, Some("EDITED text"))
                .await;

            {
                let state = actor.state.lock().await;
                let wire = actor.build_queue_wire(&state);
                assert_eq!(ids(&wire), vec!["p1"], "row stays queued");
                assert_eq!(wire[0].text, "EDITED text", "edit saved to the row");
                assert_eq!(wire[0].version, 1, "LWW edit bumps the version");
                assert!(
                    actor.pending_interjections.is_empty(),
                    "nothing buffered without a running turn"
                );
            }

            // The row's Image blocks survive the text-only LWW edit — the
            // edit must not silently detach the queued prompt's images.
            {
                let state = actor.state.lock().await;
                let images: usize = state.pending_inputs[0]
                    .prompt_blocks
                    .iter()
                    .filter(|b| matches!(b, acp::ContentBlock::Image(_)))
                    .count();
                assert_eq!(images, 1, "image block must survive the LWW edit");
            }

            // Stale version gets NO fallback even without a running turn —
            // a concurrent edit won and losing ours is correct LWW.
            actor
                .handle_interject_queued_prompt("p1", 99, None, Some("LOSER edit"))
                .await;
            let state = actor.state.lock().await;
            let wire = actor.build_queue_wire(&state);
            assert_eq!(wire[0].text, "EDITED text", "stale edit must not win");
            assert_eq!(wire[0].version, 1);
        })
        .await;
}

/// Editing a queued bash row rebuilds the bash `PromptBlockMeta` with the edited text.
#[tokio::test]
async fn edit_queued_bash_row_rebuilds_bash_meta_with_new_text() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(bash_item("p1", "alice", "ls"));
            }

            actor
                .handle_edit_queued_prompt("p1", "ls -la".into(), Some("bob"))
                .await;

            let state = actor.state.lock().await;
            let item = state
                .pending_inputs
                .iter()
                .find(|i| i.queue_meta.as_ref().is_some_and(|m| m.id == "p1"))
                .expect("p1 still in queue");
            assert_eq!(
                SessionActor::extract_bash_command(&item.prompt_blocks).as_deref(),
                Some("ls -la"),
                "edited row must still execute as a bash command with the NEW text"
            );
            let meta = item.queue_meta.as_ref().unwrap();
            assert_eq!(meta.kind, "bash", "wire kind unchanged");
            assert_eq!(meta.text, "ls -la");
            assert_eq!(meta.version, 1);
        })
        .await;
}

/// A blank `newText` never blanks a queued row.
#[tokio::test]
async fn edit_queued_prompt_empty_text_is_noop() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(bash_item("p1", "alice", "ls"));
            }

            actor
                .handle_edit_queued_prompt("p1", "  ".into(), None)
                .await;

            let state = actor.state.lock().await;
            let wire = actor.build_queue_wire(&state);
            assert_eq!(wire[0].text, "ls", "row text untouched");
            assert_eq!(wire[0].version, 0, "no LWW bump for a blank edit");
        })
        .await;
}

/// A plain-prompt edit must not gain bash meta.
#[tokio::test]
async fn edit_queued_plain_row_stays_plain() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "alice"));
            }

            actor
                .handle_edit_queued_prompt("p1", "! looks like bash".into(), Some("bob"))
                .await;

            let state = actor.state.lock().await;
            let item = &state.pending_inputs[0];
            assert!(
                SessionActor::extract_bash_command(&item.prompt_blocks).is_none(),
                "plain rows must not acquire bash meta"
            );
        })
        .await;
}

/// Interjecting a queued bash row is a benign no-op that still rebroadcasts.
#[tokio::test]
async fn interject_queued_bash_row_noop_keeps_row_queued() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(bash_item("p1", "A", "ls"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());

            actor
                .handle_interject_queued_prompt("p1", 0, None, None)
                .await;

            let state = actor.state.lock().await;
            assert_eq!(
                ids(&actor.build_queue_wire(&state)),
                vec!["p1"],
                "bash row must stay queued"
            );
            assert!(
                actor.pending_interjections.is_empty(),
                "a bash command must never buffer as an interjection"
            );
            drop(state);

            let mut saw_broadcast = false;
            while let Ok(msg) = gateway_rx.try_recv() {
                if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                    && args.request.method.as_ref()
                        == crate::session::prompt_queue::QUEUE_CHANGED_METHOD
                {
                    saw_broadcast = true;
                }
            }
            assert!(saw_broadcast, "interject no-op must rebroadcast the queue");
        })
        .await;
}

/// An edited interject of a bash row refuses the interject but keeps the edit.
#[tokio::test]
async fn interject_queued_bash_row_with_new_text_saves_edit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(bash_item("p1", "A", "ls"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());

            actor
                .handle_interject_queued_prompt("p1", 0, None, Some("ls -la"))
                .await;

            let state = actor.state.lock().await;
            let wire = actor.build_queue_wire(&state);
            assert_eq!(ids(&wire), vec!["p1"], "bash row must stay queued");
            assert_eq!(wire[0].text, "ls -la", "the edit must be kept (LWW)");
            assert_eq!(wire[0].version, 1, "LWW edit bumps the version");
            assert_eq!(wire[0].kind, "bash", "kind survives the refused interject");
            assert!(actor.pending_interjections.is_empty());
        })
        .await;
}

/// A stale version no-ops the WHOLE edited interject: no interjection (edited
/// text included) and the row untouched — edit + interject is one atomic op.
#[tokio::test]
async fn interject_queued_prompt_with_new_text_stale_version_full_noop() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("p1", "A"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());

            actor
                .handle_interject_queued_prompt("p1", 99, None, Some("EDITED text"))
                .await;

            let state = actor.state.lock().await;
            assert_eq!(
                ids(&actor.build_queue_wire(&state)),
                vec!["p1"],
                "row untouched on stale version"
            );
            assert!(
                actor.pending_interjections.is_empty(),
                "edited text must not interject on stale version"
            );
        })
        .await;
}

/// Send-now `queue_input`: prompt lands behind the running front and cancels the turn.
#[tokio::test]
async fn queue_input_send_now_inserts_behind_running_front_and_requests_cancel() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("running", "A"));
                state.running_task = Some(running_task_stub("running"));
                state.pending_inputs.push_back(user_item("held", "A"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());

            let (respond_to, _prx) = oneshot::channel();
            let cancel = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("now"))],
                    "d-now".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    /* send_now */ true,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(cancel, "send-now behind a running turn must cancel it");

            let state = actor.state.lock().await;
            let order: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                order,
                vec!["running", "d-now", "held"],
                "send-now runs next; held rows keep their place behind it"
            );
        })
        .await;
}

/// Stacked send-now prompts insert FIFO (first sent runs first), not LIFO.
/// Exercised via an active goal turn, which promotes send-now rows but never
/// cancels — so multiple sends accumulate behind the running front.
#[tokio::test]
async fn queue_input_stacked_send_now_prompts_insert_fifo_during_goal_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("running", "A"));
                state.running_task = Some(running_task_stub("running"));
                state.pending_inputs.push_back(user_item("held", "A"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());
            actor
                .tool_context
                .goal_loop_active_gate
                .store(true, std::sync::atomic::Ordering::Relaxed);

            for id in ["sn-1", "sn-2"] {
                let (respond_to, _prx) = oneshot::channel();
                let cancel = actor
                    .queue_input(
                        vec![acp::ContentBlock::Text(acp::TextContent::new(id))],
                        id.to_string(),
                        PromptMode::Agent,
                        None,
                        None,
                        None,
                        None,
                        false,
                        None,
                        /* send_now */ true,
                        None,
                        respond_to,
                        None,
                        None,
                    )
                    .await;
                assert!(!cancel, "goal turns promote send-now but never cancel");
            }

            let state = actor.state.lock().await;
            let order: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                order,
                vec!["running", "sn-1", "sn-2", "held"],
                "send-now prompts must run in the order they were sent (FIFO)"
            );
        })
        .await;
}

#[tokio::test]
async fn queue_input_auto_send_now_only_inside_wait_window() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("running", "A"));
                state.running_task = Some(running_task_stub("running"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());

            let (respond_to, _p1) = oneshot::channel();
            let cancel = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("early"))],
                    "pre-wait".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(!cancel, "pre-wait rows must not cancel the turn");

            actor.tool_context.blocking_wait_depth.set_depth_for_test(1);
            let (respond_to, _p2) = oneshot::channel();
            let cancel = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("mid-wait"))],
                    "d-mid".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(
                !cancel,
                "with a held queue present, mid-wait prompts must not cancel-and-send"
            );

            let state = actor.state.lock().await;
            let order: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                order,
                vec!["running", "pre-wait", "d-mid"],
                "mid-wait prompt appends behind existing held rows"
            );
        })
        .await;
}

#[tokio::test]
async fn queue_input_auto_send_now_when_wait_and_held_queue_empty() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("running", "A"));
                state.running_task = Some(running_task_stub("running"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());
            actor.tool_context.blocking_wait_depth.set_depth_for_test(1);

            let (respond_to, _p) = oneshot::channel();
            let cancel = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("first"))],
                    "first".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(cancel, "first prompt during empty-held wait must cancel");

            let state = actor.state.lock().await;
            let order: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(order, vec!["running", "first"]);
            assert!(
                state
                    .pending_inputs
                    .iter()
                    .find(|i| i.prompt_id == "first")
                    .is_some_and(|i| i.send_now),
                "first empty-held wait prompt is send-now"
            );

            drop(state);
            let (respond_to, _p2) = oneshot::channel();
            let cancel2 = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("second"))],
                    "second".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(!cancel2, "second prompt with held row must not cancel");
            let state = actor.state.lock().await;
            let order: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(order, vec!["running", "first", "second"]);
            assert!(
                !state
                    .pending_inputs
                    .iter()
                    .find(|i| i.prompt_id == "second")
                    .is_some_and(|i| i.send_now),
                "second prompt is a plain held append"
            );
        })
        .await;
}

/// A foreground subagent await (its `BlockingWaitGuard`) opens the same send-now window.
#[tokio::test]
async fn queue_input_auto_send_now_during_foreground_subagent_await_window() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("running", "A"));
                state.running_task = Some(running_task_stub("running"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());

            let depth = actor.tool_context.blocking_wait_depth.clone();
            assert_eq!(depth.depth(), 0, "no wait window yet");

            let wait_guard = crate::tools::tool_context::BlockingWaitGuard::enter(depth.clone());
            assert_eq!(
                depth.depth(),
                1,
                "a foreground subagent await must raise blocking_wait_depth"
            );

            let (respond_to, _p1) = oneshot::channel();
            let cancel = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("preempt"))],
                    "during-await".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(
                cancel,
                "a prompt sent during a foreground subagent await must take the send-now path"
            );

            drop(wait_guard);
            assert_eq!(
                depth.depth(),
                0,
                "guard drop must restore blocking_wait_depth"
            );

            let (respond_to, _p2) = oneshot::channel();
            let cancel = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("later"))],
                    "after-await".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    false,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(
                !cancel,
                "outside the await window prompts must queue normally"
            );

            let state = actor.state.lock().await;
            let order: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                order,
                vec!["running", "during-await", "after-await"],
                "the mid-await prompt runs next; the later one queues behind it"
            );
        })
        .await;
}

/// Synthetic inputs and active goal loops never take the send-now path.
#[tokio::test]
async fn queue_input_send_now_exempts_synthetic_and_goal_turns() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("running", "A"));
                state.running_task = Some(running_task_stub("running"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());
            actor.tool_context.blocking_wait_depth.set_depth_for_test(1);

            let (respond_to, _p1) = oneshot::channel();
            let cancel = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("done"))],
                    "task-completed-bg1".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    true,
                    None,
                    false,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(!cancel, "synthetic prompts must never cancel the turn");

            actor
                .tool_context
                .goal_loop_active_gate
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let (respond_to, _p2) = oneshot::channel();
            let cancel = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("nudge"))],
                    "d-goal".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    true,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(!cancel, "goal turns must not be cancelled by send-now");
            let state = actor.state.lock().await;
            assert_eq!(
                state.pending_inputs.back().map(|i| i.prompt_id.as_str()),
                Some("d-goal"),
                "goal-suppressed send-now falls back to a plain append"
            );
        })
        .await;
}

/// Queue-row send-now: the row (any kind) promotes to run behind the running
/// front with its RPC live and an LWW edit applied, and cancels the turn.
#[tokio::test]
async fn queue_send_now_promotes_row_and_requests_cancel() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("running", "A"));
                state.running_task = Some(running_task_stub("running"));
                state.pending_inputs.push_back(user_item("held", "A"));
                state.pending_inputs.push_back(bash_item("b1", "A", "ls"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".into());

            let cancel = actor
                .handle_interject_queued_prompt("b1", 0, Some("A"), Some("ls -la"))
                .await;
            assert!(cancel, "promoting a row behind a running turn cancels it");

            let state = actor.state.lock().await;
            let order: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                order,
                vec!["running", "b1", "held"],
                "promoted row runs next; the held row stays behind it"
            );
            let promoted = &state.pending_inputs[1];
            assert_eq!(
                promoted.queue_meta.as_ref().map(|m| m.text.as_str()),
                Some("ls -la"),
                "edit applies LWW before promotion"
            );
            assert!(
                actor.pending_interjections.is_empty(),
                "send-now never merges into the running turn"
            );
        })
        .await;
}

/// Queue-row send-now with no running turn: the row fronts but nothing cancels.
#[tokio::test]
async fn queue_send_now_idle_fronts_row_without_cancel() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("q1", "A"));
                state.pending_inputs.push_back(user_item("q2", "A"));
            }

            let cancel = actor
                .handle_interject_queued_prompt("q2", 0, Some("A"), None)
                .await;
            assert!(!cancel, "no running turn — nothing to cancel");

            let state = actor.state.lock().await;
            let order: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(order, vec!["q2", "q1"], "send-now row runs first");
        })
        .await;
}

/// Send racing turn completion: the front-pin keys on `running_prompt_id()`,
/// not the already-cleared `current_prompt_id`, so the unpopped front survives.
#[tokio::test]
async fn queue_input_send_now_pins_front_on_running_task_identity() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("finished", "A"));
                state.running_task = Some(running_task_stub("finished"));
            }
            // Completion-race window: current_prompt_id cleared, front finished but unpopped.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = None;

            let (respond_to, _prx) = oneshot::channel();
            let cancel = actor
                .queue_input(
                    vec![acp::ContentBlock::Text(acp::TextContent::new("now"))],
                    "d-now".to_string(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    /* send_now */ true,
                    None,
                    respond_to,
                    None,
                    None,
                )
                .await;
            assert!(
                cancel,
                "running_task present = a turn to cancel, even mid-completion-race"
            );

            let state = actor.state.lock().await;
            let order: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                order,
                vec!["finished", "d-now"],
                "the finished-but-unpopped front must NOT be displaced"
            );
        })
        .await;
}

/// A stale completion must not clear the promoted turn's `running_task` (would double-spawn).
#[tokio::test]
async fn stale_completion_does_not_clear_promoted_turns_running_task() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_item("promoted", "A"));
                state.running_task = Some(running_task_stub("promoted"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("promoted".into());

            actor
                .handle_completion(
                    "cancelled-old".to_string(),
                    Ok(crate::session::commands::PromptTurnOk {
                        stop_reason: acp::StopReason::EndTurn,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: crate::session::commands::PromptCompletionKind::Completed,
                        structured_output: None,
                        usage: None,
                    }),
                )
                .await;

            let state = actor.state.lock().await;
            assert!(
                state.running_task.is_some(),
                "stale completion must not clear the promoted turn's task"
            );
            assert_eq!(
                state.running_prompt_id(),
                Some("promoted"),
                "promoted turn still owns the front"
            );
            assert_eq!(
                actor
                    .current_prompt_id
                    .lock()
                    .expect("current_prompt_id mutex poisoned")
                    .as_deref(),
                Some("promoted"),
                "stale completion must not clear the promoted turn's prompt id"
            );
        })
        .await;
}
