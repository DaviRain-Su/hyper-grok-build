use std::sync::Arc;

use super::types::{
    BashExecutionBackgrounded, BashExecutionComplete, BashExecutionFailed, BashExecutionTimeout,
    BashOutputChunk, FileWritten, LspServerCrashed, LspServerFailed, LspServerReady,
    LspServerRetrying, LspServerStarting, MonitorEvent, PlanModeEntered, PlanModeExited,
    ScheduledTaskCreated, ScheduledTaskFired, ScheduledTaskRemoved, ToolNotification,
    UserQuestionAsked,
};
use crate::types::TaskSnapshot;

/// Envelope for consumers that can acknowledge durable notification handling.
pub struct AcknowledgedToolNotification {
    /// Notification delivered in the same FIFO as unacknowledged events.
    pub notification: ToolNotification,
    /// Completion sender present only when the producer requested acknowledgement.
    pub acknowledgement: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
}

/// Failure reported after all acknowledged notification targets have settled.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotificationAcknowledgementError {
    #[error("{0} acknowledging notification target(s) closed during dispatch")]
    DispatchClosed(usize),
    #[error("{0} notification acknowledgement(s) were dropped")]
    AcknowledgementDropped(usize),
    #[error("notification consumer rejected delivery: {0:?}")]
    ConsumerRejected(Vec<String>),
    #[error(
        "notification acknowledgement failed: {dispatch_closed} dispatch closed, {acknowledgements_dropped} acknowledgement(s) dropped, consumer rejections: {consumer_rejections:?}"
    )]
    Multiple {
        dispatch_closed: usize,
        acknowledgements_dropped: usize,
        consumer_rejections: Vec<String>,
    },
}

/// Receipts for one acknowledged fan-out operation.
#[must_use = "acknowledged notification receipts must be awaited"]
pub struct NotificationAcknowledgementBatch {
    receipts: Vec<tokio::sync::oneshot::Receiver<Result<(), String>>>,
    dispatch_closed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableNotificationTargets {
    None,
    Present,
}

impl NotificationAcknowledgementBatch {
    /// Wait for every live durable target and report all observed failure classes.
    pub async fn wait(self) -> Result<(), NotificationAcknowledgementError> {
        let mut acknowledgements_dropped = 0;
        let mut consumer_rejections = Vec::new();
        for receipt in self.receipts {
            match receipt.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => consumer_rejections.push(error),
                Err(_) => acknowledgements_dropped += 1,
            }
        }
        match (
            self.dispatch_closed,
            acknowledgements_dropped,
            consumer_rejections.is_empty(),
        ) {
            (0, 0, true) => Ok(()),
            (dispatch_closed, 0, true) => Err(NotificationAcknowledgementError::DispatchClosed(
                dispatch_closed,
            )),
            (0, acknowledgements_dropped, true) => Err(
                NotificationAcknowledgementError::AcknowledgementDropped(acknowledgements_dropped),
            ),
            (0, 0, false) => Err(NotificationAcknowledgementError::ConsumerRejected(
                consumer_rejections,
            )),
            (dispatch_closed, acknowledgements_dropped, _) => {
                Err(NotificationAcknowledgementError::Multiple {
                    dispatch_closed,
                    acknowledgements_dropped,
                    consumer_rejections,
                })
            }
        }
    }
}

#[derive(Clone)]
enum ToolNotificationTarget {
    Plain(tokio::sync::mpsc::UnboundedSender<ToolNotification>),
    Acknowledged(tokio::sync::mpsc::UnboundedSender<AcknowledgedToolNotification>),
}

/// Cloneable notification fan-out with per-target FIFO ordering.
#[derive(Clone)]
pub struct ToolNotificationHandle {
    targets: Arc<[ToolNotificationTarget]>,
}

impl Default for ToolNotificationHandle {
    fn default() -> Self {
        Self::noop()
    }
}

macro_rules! convenience_sends {
    ($($method:ident, $ty:ty, $variant:ident);+ $(;)?) => {
        $(pub fn $method(&self, value: $ty) { self.send(ToolNotification::$variant(value)); })+
    };
}

impl ToolNotificationHandle {
    pub fn new(sender: tokio::sync::mpsc::UnboundedSender<ToolNotification>) -> Self {
        Self {
            targets: Arc::from([ToolNotificationTarget::Plain(sender)]),
        }
    }

    pub fn from_sender(sender: tokio::sync::mpsc::UnboundedSender<ToolNotification>) -> Self {
        Self::new(sender)
    }

    pub fn channel() -> (Self, tokio::sync::mpsc::UnboundedReceiver<ToolNotification>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (Self::new(sender), receiver)
    }

    pub fn acknowledged_channel() -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<AcknowledgedToolNotification>,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                targets: Arc::from([ToolNotificationTarget::Acknowledged(sender)]),
            },
            receiver,
        )
    }

    pub fn noop() -> Self {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        Self::new(sender)
    }

    /// Combine handles while preserving each target's send order.
    pub fn tee(handles: Vec<ToolNotificationHandle>) -> ToolNotificationHandle {
        let targets = handles
            .iter()
            .flat_map(|handle| handle.targets.iter().cloned())
            .collect::<Vec<_>>();
        Self {
            targets: Arc::from(targets),
        }
    }

    pub(crate) fn durable_targets(&self) -> DurableNotificationTargets {
        if self.targets.iter().any(|target| {
            matches!(target, ToolNotificationTarget::Acknowledged(sender) if !sender.is_closed())
        }) {
            DurableNotificationTargets::Present
        } else {
            DurableNotificationTargets::None
        }
    }

    pub fn send(&self, notification: ToolNotification) {
        let last = self.targets.len().saturating_sub(1);
        let mut notification = Some(notification);
        for (index, target) in self.targets.iter().enumerate() {
            let notification = if index == last {
                let Some(notification) = notification.take() else {
                    break;
                };
                notification
            } else {
                let Some(notification) = notification.as_ref() else {
                    break;
                };
                notification.clone()
            };
            match target {
                ToolNotificationTarget::Plain(target) => {
                    let _ = target.send(notification);
                }
                ToolNotificationTarget::Acknowledged(target) => {
                    let _ = target.send(AcknowledgedToolNotification {
                        notification,
                        acknowledgement: None,
                    });
                }
            }
        }
    }

    /// Send a removal to every target and collect all durable acknowledgements.
    pub fn send_scheduled_task_removed_acknowledged(
        &self,
        removed: ScheduledTaskRemoved,
    ) -> NotificationAcknowledgementBatch {
        let notification = ToolNotification::ScheduledTaskRemoved(removed);
        let mut batch = NotificationAcknowledgementBatch {
            receipts: Vec::new(),
            dispatch_closed: 0,
        };
        for target in self.targets.iter() {
            match target {
                ToolNotificationTarget::Plain(target) => {
                    let _ = target.send(notification.clone());
                }
                ToolNotificationTarget::Acknowledged(target) => {
                    let (acknowledgement, receipt) = tokio::sync::oneshot::channel();
                    if target
                        .send(AcknowledgedToolNotification {
                            notification: notification.clone(),
                            acknowledgement: Some(acknowledgement),
                        })
                        .is_ok()
                    {
                        batch.receipts.push(receipt);
                    } else {
                        batch.dispatch_closed += 1;
                    }
                }
            }
        }
        batch
    }

    convenience_sends! {
        send_output_chunk, BashOutputChunk, BashOutputChunk;
        send_complete, BashExecutionComplete, BashExecutionComplete;
        send_timeout, BashExecutionTimeout, BashExecutionTimeout;
        send_backgrounded, BashExecutionBackgrounded, BashExecutionBackgrounded;
        send_failed, BashExecutionFailed, BashExecutionFailed;
        send_file_written, FileWritten, FileWritten;
        send_task_complete, TaskSnapshot, TaskCompleted;
        send_plan_mode_entered, PlanModeEntered, PlanModeEntered;
        send_plan_mode_exited, PlanModeExited, PlanModeExited;
        send_user_question_asked, UserQuestionAsked, UserQuestionAsked;
        send_lsp_starting, LspServerStarting, LspServerStarting;
        send_lsp_ready, LspServerReady, LspServerReady;
        send_lsp_crashed, LspServerCrashed, LspServerCrashed;
        send_lsp_retrying, LspServerRetrying, LspServerRetrying;
        send_lsp_failed, LspServerFailed, LspServerFailed;
        send_scheduled_task_fired, ScheduledTaskFired, ScheduledTaskFired;
        send_scheduled_task_removed, ScheduledTaskRemoved, ScheduledTaskRemoved;
        send_scheduled_task_created, ScheduledTaskCreated, ScheduledTaskCreated;
        send_monitor_event, MonitorEvent, MonitorEvent;
    }
}

/// Per-call notification override applied in addition to the session-wide sink.
#[derive(Clone)]
pub struct PerCallNotificationSink(pub ToolNotificationHandle);

#[cfg(test)]
#[path = "handle_tests.rs"]
mod tests;
