use super::*;
use std::sync::Arc;
use xai_grok_tools::implementations::grok_build::task::types::ActiveAgentMessage;

#[expect(
    clippy::unwrap_used,
    reason = "test assertions require live response senders"
)]
fn admission_response(
    response: Result<ActiveMessageAdmission, oneshot::error::RecvError>,
) -> ActiveMessageAdmission {
    response.unwrap()
}

const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn message(id: &str) -> ActiveAgentMessage {
    ActiveAgentMessage {
        message_id: id.into(),
        sender_session_id: "root-session".into(),
        text: Arc::from("parent update"),
    }
}

async fn await_with_timeout<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("parent-message test timed out")
}

#[tokio::test(flavor = "current_thread")]
async fn admission_rejects_closed_receipt_sink_without_queueing() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let (receipt_sink, receipt_rx) = mpsc::channel(1);
        drop(receipt_rx);
        let (respond_to, response_rx) = oneshot::channel();
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();

        await_with_timeout(actor.admit_parent_agent_message_for_test(
            message("closed"),
            receipt_sink,
            respond_to,
            completion_tx,
        ))
        .await;

        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::ChannelClosed
        );
        assert!(
            await_with_timeout(actor.state.lock())
                .await
                .pending_inputs
                .is_empty()
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn receipt_backpressure_waits_before_queue_commit() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let (receipt_sink, mut receipt_rx) = mpsc::channel(1);
        let (_occupied_tx, occupied_rx) = oneshot::channel();
        receipt_sink
            .send(crate::agent::subagent::PromptTurnReceipt {
                prompt_id: "occupied".into(),
                result: occupied_rx,
                telemetry: crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry {
                    admitted_at: std::time::Instant::now(),
                    parent_ctx: xai_grok_telemetry::TelemetryCtx::new(
                        "parent".to_owned(),
                        Arc::new(tokio::sync::Mutex::new(0)),
                    ),
                },
            })
            .await
            .expect("occupy receipt capacity");
        let (respond_to, mut response_rx) = oneshot::channel();
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        let admission = tokio::task::spawn_local({
            let actor = Arc::clone(&actor);
            async move {
                actor
                    .admit_parent_agent_message_for_test(
                        message("backpressured"),
                        receipt_sink,
                        respond_to,
                        completion_tx,
                    )
                    .await;
            }
        });

        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut response_rx)
                .await
                .is_err(),
            "receipt backpressure must keep admission pending"
        );
        assert!(
            await_with_timeout(actor.state.lock())
                .await
                .pending_inputs
                .is_empty(),
            "no queue row exists before receipt capacity is reserved"
        );
        receipt_rx.recv().await.expect("occupied receipt");

        await_with_timeout(admission).await.expect("admission task");
        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::Admitted
        );
        assert_eq!(
            await_with_timeout(actor.state.lock())
                .await
                .pending_inputs
                .len(),
            1
        );
        assert_eq!(
            receipt_rx
                .recv()
                .await
                .expect("committed receipt")
                .prompt_id,
            "parent-message-backpressured"
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn busy_state_lock_waits_before_commit_and_receipt_handoff() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let state = await_with_timeout(actor.state.lock()).await;
        let (receipt_sink, mut receipt_rx) = mpsc::channel(1);
        let (respond_to, mut response_rx) = oneshot::channel();
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        let admission = tokio::task::spawn_local({
            let actor = Arc::clone(&actor);
            async move {
                actor
                    .admit_parent_agent_message_for_test(
                        message("busy"),
                        receipt_sink,
                        respond_to,
                        completion_tx,
                    )
                    .await;
            }
        });

        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut response_rx)
                .await
                .is_err(),
            "lock contention must keep admission pending"
        );
        assert!(state.pending_inputs.is_empty());
        assert!(receipt_rx.try_recv().is_err());
        drop(state);

        await_with_timeout(admission).await.expect("admission task");
        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::Admitted
        );
        assert!(receipt_rx.try_recv().is_ok());
        assert_eq!(
            await_with_timeout(actor.state.lock())
                .await
                .pending_inputs
                .len(),
            1
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn committed_delivery_queues_protected_fifo_row_with_typed_receipt_identity() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        actor.deferred_prefix.arm(tokio::task::spawn_local(async {
            "PARENT_PREFIX_READY".to_string()
        }));
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        await_with_timeout(actor.state.lock()).await.running_task =
            Some(super::super::support::running_task_stub("running"));
        let (receipt_sink, mut receipt_rx) = mpsc::channel(1);
        let (respond_to, response_rx) = oneshot::channel();

        await_with_timeout(actor.admit_parent_agent_message_for_test(
            message("queued"),
            receipt_sink,
            respond_to,
            completion_tx,
        ))
        .await;

        assert_eq!(
            admission_response(await_with_timeout(response_rx).await),
            ActiveMessageAdmission::Admitted
        );
        let receipt = receipt_rx.recv().await.expect("typed receipt handed off");
        assert_eq!(receipt.prompt_id, "parent-message-queued");
        assert_eq!(receipt.telemetry.parent_ctx.session_id, "test-parent");
        assert_eq!(
            *await_with_timeout(receipt.telemetry.parent_ctx.prompt_index.lock()).await,
            0,
        );
        let state = await_with_timeout(actor.state.lock()).await;
        let queued = state.pending_inputs.front().expect("queued input");
        assert_eq!(queued.prompt_id, receipt.prompt_id);
        assert!(queued.is_queue_protected());
        assert!(matches!(
            queued.input_origin.as_prompt_origin(),
            PromptOrigin::ParentAgentMessage {
                message_id,
                sender_session_id,
            } if message_id == "queued" && sender_session_id == "root-session"
        ));
        drop(state);
        let conversation = await_with_timeout(actor.chat_state_handle.get_conversation()).await;
        assert!(matches!(
            conversation.first(),
            Some(ConversationItem::User(user))
                if matches!(user.content.first(), Some(ContentPart::Text { text }) if text.as_ref() == "PARENT_PREFIX_READY")
        ));
    }))
    .await;
}
