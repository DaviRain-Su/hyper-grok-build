//! Admission of protected messages from an owning parent agent.

use super::*;
use xai_grok_tools::implementations::grok_build::task::coordinator::ActiveMessageAdmission;
use xai_grok_tools::implementations::grok_build::task::types::{
    ActiveAgentMessage, ActiveAgentMessageDelivery,
};

impl SessionActor {
    pub(super) async fn admit_parent_agent_message(
        self: &Arc<Self>,
        delivery: ActiveAgentMessageDelivery,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        parent_telemetry_ctx: xai_grok_telemetry::TelemetryCtx,
        respond_to: oneshot::Sender<ActiveMessageAdmission>,
        completion_tx: mpsc::UnboundedSender<super::tasks_cancel::TurnCompletionMsg>,
    ) {
        let message = delivery.message().clone();
        self.admit_parent_agent_message_with(
            message,
            receipt_sink,
            parent_telemetry_ctx,
            respond_to,
            completion_tx,
            |actor, state, item| {
                delivery
                    .commit_admission(|| actor.insert_parent_agent_message(state, item))
                    .is_some()
            },
        )
        .await;
    }

    async fn admit_parent_agent_message_with(
        self: &Arc<Self>,
        message: ActiveAgentMessage,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        parent_telemetry_ctx: xai_grok_telemetry::TelemetryCtx,
        respond_to: oneshot::Sender<ActiveMessageAdmission>,
        completion_tx: mpsc::UnboundedSender<super::tasks_cancel::TurnCompletionMsg>,
        commit: impl FnOnce(&Self, &mut State, InputItem) -> bool,
    ) {
        let receipt_permit = match receipt_sink.reserve_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                let _ = respond_to.send(ActiveMessageAdmission::ChannelClosed);
                return;
            }
        };

        self.ensure_prefix_ready().await;
        let prompt_id = format!("parent-message-{}", message.message_id);
        let input_origin = InputOrigin::new(super::PromptOrigin::ParentAgentMessage {
            message_id: message.message_id,
            sender_session_id: message.sender_session_id,
        });
        let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
            message.text.to_string(),
        ))];
        let queue_meta = crate::session::prompt_queue::QueueEntryMeta {
            id: prompt_id.clone(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: "parent_agent_message".to_string(),
            text: Self::queue_text_from_blocks(&prompt_blocks),
            combined_texts: None,
        };
        let (turn_result_tx, turn_result_rx) = oneshot::channel();
        let item = InputItem {
            prompt_id: prompt_id.clone(),
            prompt_blocks,
            prompt_mode: PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: false,
            json_schema: None,
            input_origin,
            task_wake_fallback: None,
            tool_overrides_update: None,
            respond_to: turn_result_tx,
            persist_ack: None,
            parsed_prompt_tx: None,
            initial_child_prompt_ready: None,
            queue_meta: Some(queue_meta),
            queue_mutation_policy: QueueMutationPolicy::new(true, false),
            send_now: false,
            traceparent: None,
        };
        let mut state = self.state.lock().await;
        let admitted_at = std::time::Instant::now();
        if !commit(self, &mut state, item) {
            let _ = respond_to.send(ActiveMessageAdmission::Rejected);
            return;
        }
        drop(state);

        receipt_permit.send(crate::agent::subagent::PromptTurnReceipt {
            prompt_id,
            result: turn_result_rx,
            telemetry: crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry {
                admitted_at,
                parent_ctx: parent_telemetry_ctx,
            },
        });
        let _ = respond_to.send(ActiveMessageAdmission::Admitted);
        Self::maybe_start_running_task(self.clone(), completion_tx).await;
    }

    fn insert_parent_agent_message(&self, state: &mut State, item: InputItem) {
        state.pending_inputs.push_back(item);
        self.broadcast_queue_changed(state);
    }

    #[cfg(test)]
    async fn admit_parent_agent_message_for_test(
        self: &Arc<Self>,
        message: ActiveAgentMessage,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        respond_to: oneshot::Sender<ActiveMessageAdmission>,
        completion_tx: mpsc::UnboundedSender<super::tasks_cancel::TurnCompletionMsg>,
    ) {
        self.admit_parent_agent_message_with(
            message,
            receipt_sink,
            xai_grok_telemetry::TelemetryCtx::new(
                "test-parent".to_owned(),
                std::sync::Arc::new(tokio::sync::Mutex::new(0)),
            ),
            respond_to,
            completion_tx,
            |actor, state, item| {
                actor.insert_parent_agent_message(state, item);
                true
            },
        )
        .await;
    }
}

#[cfg(test)]
#[path = "parent_message_tests.rs"]
mod tests;
