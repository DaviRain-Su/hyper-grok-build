//! Admission and safe-point delivery of messages from an owning parent agent.

use super::*;
use std::sync::Arc;
use xai_grok_tools::implementations::grok_build::task::coordinator::ActiveMessageAdmission;
use xai_grok_tools::implementations::grok_build::task::types::{
    ActiveAgentMessage, ActiveAgentMessageDelivery, ActiveAgentMessageOperation,
};
use xai_message_delivery_core::{
    DeliveryMessage, MessageDeliveryLifecycle, OwnedDelivery, TerminalCause, TerminalTarget,
    TurnBinding,
};

#[derive(Clone)]
pub(super) struct ParentAgentSource {
    sender_session_id: String,
}

#[derive(Clone)]
pub(super) struct PendingParentAgentMessage {
    prompt_id: String,
    message_id: String,
    text: Arc<str>,
    telemetry: crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry,
}

type ParentMessageCompletion = oneshot::Sender<crate::session::commands::PromptTurnResult>;
type ParentMessageLifecycle = MessageDeliveryLifecycle<
    String,
    ParentAgentSource,
    PendingParentAgentMessage,
    ParentMessageCompletion,
    String,
    TurnEpoch,
>;
pub(super) type ParentOwnedDelivery = OwnedDelivery<
    String,
    ParentAgentSource,
    PendingParentAgentMessage,
    ParentMessageCompletion,
    String,
    TurnEpoch,
>;

/// Named bound on live Steer slots waiting for the next safe point.
/// Parent text becomes model-visible in one batch; this keeps that batch finite.
const MAX_PARENT_STEER_SLOTS: usize = 32;

/// A safe-point delivery commit could not reach a downstream actor;
/// the slots stay projecting for terminal settlement.
enum DrainCommitError {
    PersistenceUnavailable,
    ChatStateUnavailable,
}

#[derive(Default)]
pub(crate) struct MessageDeliveryState {
    lifecycle: ParentMessageLifecycle,
}

impl MessageDeliveryState {
    fn transition(
        &mut self,
        target: TerminalTarget<'_, String, TurnEpoch>,
        cause: TerminalCause,
    ) -> xai_message_delivery_core::TerminalTransition<
        String,
        ParentAgentSource,
        PendingParentAgentMessage,
        ParentMessageCompletion,
        String,
        TurnEpoch,
    > {
        self.lifecycle.transition(target, cause)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.lifecycle.is_empty()
    }
}

impl Drop for MessageDeliveryState {
    fn drop(&mut self) {
        let transition = self.transition(TerminalTarget::All, TerminalCause::ActorDrop);
        let result = Err(acp::Error::internal_error()
            .data("active-message owner dropped before terminal settlement"));
        transition
            .completions
            .into_iter()
            .chain(transition.fallbacks)
            .for_each(|owned| {
                let (_, _, completion) = owned.into_parts();
                let _ = completion.send(result.clone());
            });
    }
}

impl PendingParentAgentMessage {
    fn into_input(
        self,
        source: ParentAgentSource,
        respond_to: ParentMessageCompletion,
    ) -> InputItem {
        let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
            self.text.to_string(),
        ))];
        let queue_meta = crate::session::prompt_queue::QueueEntryMeta {
            id: self.prompt_id.clone(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: "parent_agent_message".to_owned(),
            text: SessionActor::queue_text_from_blocks(&prompt_blocks),
            combined_texts: None,
        };
        InputItem {
            prompt_id: self.prompt_id,
            prompt_blocks,
            prompt_mode: PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: false,
            json_schema: None,
            input_origin: InputOrigin::new(super::PromptOrigin::ParentAgentMessage {
                message_id: self.message_id,
                sender_session_id: source.sender_session_id,
            }),
            task_wake_fallback: None,
            tool_overrides_update: None,
            respond_to,
            persist_ack: None,
            parsed_prompt_tx: None,
            initial_child_prompt_ready: None,
            queue_meta: Some(queue_meta),
            queue_mutation_policy: QueueMutationPolicy::new(true, false),
            send_now: false,
            traceparent: None,
        }
    }
}

fn turn_binding(task: &AgentTask) -> TurnBinding<String, TurnEpoch> {
    TurnBinding::new(task.prompt_id.clone(), task.epoch)
}

fn contains_queued_identity(state: &State, identity: &str) -> bool {
    state.pending_inputs.iter().any(|item| {
        matches!(
            item.input_origin.as_prompt_origin(),
            PromptOrigin::ParentAgentMessage { message_id, .. } if message_id == identity
        )
    })
}

impl SessionActor {
    pub(super) async fn admit_parent_agent_message(
        self: &Arc<Self>,
        delivery: ActiveAgentMessageDelivery,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        parent_telemetry_ctx: xai_grok_telemetry::TelemetryCtx,
        respond_to: oneshot::Sender<ActiveMessageAdmission>,
        completion_tx: mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
    ) {
        let message = delivery.message().clone();
        let requested = delivery.operation();
        self.admit_parent_agent_message_inner(
            Some(delivery),
            message,
            requested,
            receipt_sink,
            parent_telemetry_ctx,
            respond_to,
            completion_tx,
        )
        .await;
    }

    async fn admit_parent_agent_message_inner(
        self: &Arc<Self>,
        delivery: Option<ActiveAgentMessageDelivery>,
        message: ActiveAgentMessage,
        requested: ActiveAgentMessageOperation,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        parent_telemetry_ctx: xai_grok_telemetry::TelemetryCtx,
        respond_to: oneshot::Sender<ActiveMessageAdmission>,
        completion_tx: mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
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
        let (turn_result_tx, turn_result_rx) = oneshot::channel();
        let admitted_at = std::time::Instant::now();
        let mut state = self.state.lock().await;
        if state
            .message_delivery
            .lifecycle
            .contains_identity(&message.message_id)
            || contains_queued_identity(&state, &message.message_id)
        {
            let _ = respond_to.send(ActiveMessageAdmission::Rejected);
            return;
        }
        let effective = match (requested, state.running_task.as_ref()) {
            (ActiveAgentMessageOperation::Steer, Some(_)) => ActiveAgentMessageOperation::Steer,
            (ActiveAgentMessageOperation::Queue | ActiveAgentMessageOperation::Steer, None)
            | (ActiveAgentMessageOperation::Queue, Some(_)) => ActiveAgentMessageOperation::Queue,
        };
        if effective == ActiveAgentMessageOperation::Steer
            && state.message_delivery.lifecycle.len() >= MAX_PARENT_STEER_SLOTS
        {
            let _ = respond_to.send(ActiveMessageAdmission::Rejected);
            return;
        }
        let telemetry = crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry::new(
            admitted_at,
            parent_telemetry_ctx,
            requested,
            effective,
            (requested == ActiveAgentMessageOperation::Steer
                && effective == ActiveAgentMessageOperation::Queue)
                .then_some(crate::session::telemetry::ActiveAgentMessageFallbackReason::Idle),
        );
        let content = PendingParentAgentMessage {
            prompt_id: prompt_id.clone(),
            message_id: message.message_id.clone(),
            text: message.text,
            telemetry: telemetry.clone(),
        };
        let source = ParentAgentSource {
            sender_session_id: message.sender_session_id,
        };
        let commit = || match effective {
            ActiveAgentMessageOperation::Queue => {
                let item = content.into_input(source, turn_result_tx);
                self.commit_queued_delivery(
                    &mut state,
                    super::prompt_queue::PreparedDelivery(item),
                );
            }
            ActiveAgentMessageOperation::Steer => {
                let binding = turn_binding(
                    state
                        .running_task
                        .as_ref()
                        .unwrap_or_else(|| unreachable!("Steer effective only while running")),
                );
                state
                    .message_delivery
                    .lifecycle
                    .admit_pending(
                        binding,
                        DeliveryMessage::new(message.message_id, source, content),
                        turn_result_tx,
                    )
                    .unwrap_or_else(|_| unreachable!("live identity checked under state lock"));
            }
        };
        let committed = if let Some(delivery) = delivery {
            delivery.commit_admission(commit).is_some()
        } else {
            commit();
            true
        };
        if !committed {
            let _ = respond_to.send(ActiveMessageAdmission::Rejected);
            return;
        }
        drop(state);

        receipt_permit.send(crate::agent::subagent::PromptTurnReceipt {
            prompt_id,
            result: turn_result_rx,
            telemetry,
        });
        let _ = respond_to.send(ActiveMessageAdmission::Admitted);
        Self::maybe_start_running_task(self.clone(), completion_tx).await;
    }

    pub(super) async fn drain_parent_messages_at_safe_point(&self) -> bool {
        let model_id = self.current_model_id().await;
        let user_chunk_meta = serde_json::json!({ "modelId": model_id })
            .as_object()
            .cloned();
        let notification_meta = self.build_notification_meta();
        let binding = {
            let mut state = self.state.lock().await;
            let Some(binding) = state.running_task.as_ref().map(turn_binding) else {
                return false;
            };
            if state
                .message_delivery
                .lifecycle
                .begin_delivery(&binding)
                .is_empty()
            {
                return false;
            }
            binding
        };
        // The barrier precedes every visible side effect: a dead or cancelled
        // persistence actor skips delivery, and a teardown settlement landing
        // while this await is suspended leaves nothing to roll back.
        let (persisted_tx, persisted_rx) = oneshot::channel();
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::FlushAndAck {
                respond_to: persisted_tx,
            })
            .is_err()
            || !matches!(persisted_rx.await, Ok(Ok(())))
        {
            tracing::error!(
                session_id = %self.session_info.id.0,
                "parent-message drain skipped: persistence barrier failed"
            );
            return false;
        }
        // Persist and push only inside the commit under the state lock: teardown
        // settlement transitions slots under the same lock, so a slot settled
        // during the barrier yields no projecting messages here and its text never
        // reaches updates.jsonl or chat state after the parent was told failure.
        let mut state = self.state.lock().await;
        let committed = state
            .message_delivery
            .lifecycle
            .finish_delivery(&binding, |messages| {
                for message in messages {
                    let update = acp::SessionUpdate::UserMessageChunk(
                        acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                            message.content().text.to_string(),
                        )))
                        .meta(user_chunk_meta.clone()),
                    );
                    self.notifications
                        .persistence_tx
                        .send(PersistenceMsg::Update(SessionUpdate::Acp(Box::new(
                            acp::SessionNotification::new(self.session_info.id.clone(), update)
                                .meta(notification_meta.clone().as_object().cloned()),
                        ))))
                        .map_err(|_| DrainCommitError::PersistenceUnavailable)?;
                }
                self.chat_state_handle
                    .try_push_user_messages_batch(
                        messages
                            .iter()
                            .map(|message| {
                                ConversationItem::agent_message(message.content().text.to_string())
                            })
                            .collect(),
                    )
                    .map_err(|_| DrainCommitError::ChatStateUnavailable)?;
                let delivered_at = std::time::Instant::now();
                for message in messages {
                    message
                        .content()
                        .telemetry
                        .record_safe_point_delivery(delivered_at);
                }
                Ok(())
            });
        match committed {
            Ok(delivered) => !delivered.is_empty(),
            Err(DrainCommitError::PersistenceUnavailable) => {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    "parent-message drain skipped: persistence actor unavailable"
                );
                false
            }
            Err(DrainCommitError::ChatStateUnavailable) => {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    "parent-message drain skipped: chat-state actor unavailable"
                );
                false
            }
        }
    }

    pub(super) fn transition_parent_messages(
        &self,
        state: &mut State,
        target: TerminalTarget<'_, String, TurnEpoch>,
        cause: TerminalCause,
    ) -> (Vec<ParentOwnedDelivery>, bool) {
        let transition = state.message_delivery.transition(target, cause);
        let has_fallbacks = !transition.fallbacks.is_empty();
        for owned in transition.fallbacks {
            let fallback_reason = match cause {
                TerminalCause::Completion => {
                    crate::session::telemetry::ActiveAgentMessageFallbackReason::Completion
                }
                TerminalCause::SoftCancel => {
                    crate::session::telemetry::ActiveAgentMessageFallbackReason::SoftCancel
                }
                TerminalCause::Rewind => {
                    crate::session::telemetry::ActiveAgentMessageFallbackReason::Rewind
                }
                TerminalCause::HardTeardown | TerminalCause::ActorDrop => {
                    unreachable!("terminal cause cannot produce a fallback")
                }
            };
            let (_, message, completion) = owned.into_parts();
            let (_, source, content) = message.into_parts();
            content.telemetry.record_fallback(fallback_reason);
            state
                .pending_inputs
                .push_back(content.into_input(source, completion));
        }
        (transition.completions, has_fallbacks)
    }

    pub(super) async fn settle_all_parent_messages(&self, cause: TerminalCause) {
        debug_assert!(matches!(
            cause,
            TerminalCause::HardTeardown | TerminalCause::ActorDrop
        ));
        let completions = {
            let mut state = self.state.lock().await;
            let (completions, _) =
                self.transition_parent_messages(&mut state, TerminalTarget::All, cause);
            completions
        };
        let result = Err(acp::Error::internal_error()
            .data("active-message owner ended before delivery completed"));
        Self::settle_parent_message_completions(completions, &result);
    }

    pub(super) fn settle_parent_message_completions(
        completions: Vec<ParentOwnedDelivery>,
        result: &crate::session::commands::PromptTurnResult,
    ) {
        completions.into_iter().for_each(|owned| {
            let (_, _, completion) = owned.into_parts();
            let _ = completion.send(result.clone());
        });
    }

    #[cfg(test)]
    async fn admit_parent_agent_message_for_test(
        self: &Arc<Self>,
        message: ActiveAgentMessage,
        operation: ActiveAgentMessageOperation,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        respond_to: oneshot::Sender<ActiveMessageAdmission>,
        completion_tx: mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
    ) {
        self.admit_parent_agent_message_inner(
            None,
            message,
            operation,
            receipt_sink,
            xai_grok_telemetry::TelemetryCtx::new(
                "test-parent".to_owned(),
                std::sync::Arc::new(tokio::sync::Mutex::new(0)),
            ),
            respond_to,
            completion_tx,
        )
        .await;
    }
}

#[cfg(test)]
#[path = "parent_message_tests.rs"]
mod tests;
