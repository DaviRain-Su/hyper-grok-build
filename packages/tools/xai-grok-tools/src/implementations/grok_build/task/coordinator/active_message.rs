//! Active-child message admission and finalization linearization.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio_util::sync::WaitForCancellationFutureOwned;

use super::SubagentCoordinator;
use crate::implementations::grok_build::task::active_message::{
    ActiveMessageAdmissionLease, ActiveMessageIngress,
};
use crate::implementations::grok_build::task::coordinator_state::{
    ACTIVE_MESSAGE_ADMISSION_TIMEOUT, ActiveMessageAdmission, ChildControl, ChildRunner,
    MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD,
};
use crate::implementations::grok_build::task::types::{
    ActiveAgentMessage, ActiveAgentMessageDelivery, ActiveAgentMessageOutcome,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActiveChildGeneration(uuid::Uuid);

impl ActiveChildGeneration {
    pub(super) fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BeginAdmission {
    Started,
    Finalizing,
    Saturated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::implementations::grok_build::task) enum TerminalDrainDisposition {
    Clean,
    Uncertain,
}

impl TerminalDrainDisposition {
    fn record(&mut self, is_settled: bool) {
        if !is_settled {
            *self = Self::Uncertain;
        }
    }

    fn is_clean(self) -> bool {
        self == Self::Clean
    }
}

pub(in crate::implementations::grok_build::task) enum ActiveMessageLifecycle {
    Open {
        in_flight: usize,
        disposition: TerminalDrainDisposition,
    },
    Finalizing {
        in_flight: usize,
        disposition: TerminalDrainDisposition,
        waiters: Vec<oneshot::Sender<bool>>,
    },
}

impl Default for ActiveMessageLifecycle {
    fn default() -> Self {
        Self::Open {
            in_flight: 0,
            disposition: TerminalDrainDisposition::Clean,
        }
    }
}

impl ActiveMessageLifecycle {
    fn begin_admission(&mut self) -> BeginAdmission {
        let Self::Open { in_flight, .. } = self else {
            return BeginAdmission::Finalizing;
        };
        if *in_flight >= MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD {
            return BeginAdmission::Saturated;
        }
        *in_flight += 1;
        BeginAdmission::Started
    }

    pub(super) fn begin_finalizing(&mut self, respond_to: oneshot::Sender<bool>) {
        if let Some(is_clean) = self.start_terminalizing() {
            let _ = respond_to.send(is_clean);
        } else if let Self::Finalizing { waiters, .. } = self {
            waiters.push(respond_to);
        }
    }

    pub(super) fn start_terminalizing(&mut self) -> Option<bool> {
        match self {
            Self::Open {
                in_flight,
                disposition,
            } => {
                let in_flight = *in_flight;
                let disposition = *disposition;
                *self = Self::Finalizing {
                    in_flight,
                    disposition,
                    waiters: Vec::new(),
                };
                (in_flight == 0).then(|| disposition.is_clean())
            }
            Self::Finalizing {
                in_flight,
                disposition,
                ..
            } if *in_flight == 0 => Some(disposition.is_clean()),
            Self::Finalizing { .. } => None,
        }
    }

    fn finish_admission(&mut self, is_settled: bool) -> Option<bool> {
        let (in_flight, disposition, waiters) = match self {
            Self::Open {
                in_flight,
                disposition,
            } => (in_flight, disposition, None),
            Self::Finalizing {
                in_flight,
                disposition,
                waiters,
            } => (in_flight, disposition, Some(waiters)),
        };
        disposition.record(is_settled);
        *in_flight = in_flight
            .checked_sub(1)
            .unwrap_or_else(|| unreachable!("active-message completion without admission"));
        if *in_flight == 0
            && let Some(waiters) = waiters
        {
            let is_clean = disposition.is_clean();
            resolve_waiters(waiters, is_clean);
            return Some(is_clean);
        }
        None
    }
}

impl Drop for ActiveMessageLifecycle {
    fn drop(&mut self) {
        if let Self::Finalizing { waiters, .. } = self {
            resolve_waiters(waiters, false);
        }
    }
}
fn resolve_waiters(waiters: &mut Vec<oneshot::Sender<bool>>, outcome: bool) {
    waiters.drain(..).for_each(|waiter| {
        let _ = waiter.send(outcome);
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveMessageCompletionOutcome {
    Admission(ActiveMessageAdmission),
    Cancelled,
    DeadlineElapsed,
}

pub(super) struct ActiveMessageCompletion {
    subagent_id: String,
    generation: ActiveChildGeneration,
    parent_session_id: String,
    message_id: String,
    respond_to: Option<oneshot::Sender<ActiveAgentMessageOutcome>>,
    outcome: ActiveMessageCompletionOutcome,
    is_settled: bool,
    _ingress_permit: OwnedSemaphorePermit,
}

fn protocol_outcome_from_completion(
    outcome: ActiveMessageCompletionOutcome,
    is_settled: bool,
    message_id: &str,
) -> ActiveAgentMessageOutcome {
    if !is_settled {
        return ActiveAgentMessageOutcome::AdmissionUncertain;
    }
    match outcome {
        ActiveMessageCompletionOutcome::Admission(ActiveMessageAdmission::Admitted) => {
            ActiveAgentMessageOutcome::Accepted {
                message_id: message_id.to_owned(),
            }
        }
        ActiveMessageCompletionOutcome::Admission(ActiveMessageAdmission::Unsupported) => {
            ActiveAgentMessageOutcome::Unsupported
        }
        ActiveMessageCompletionOutcome::Admission(ActiveMessageAdmission::ChannelClosed) => {
            ActiveAgentMessageOutcome::ChannelClosed
        }
        ActiveMessageCompletionOutcome::Admission(ActiveMessageAdmission::Rejected) => {
            ActiveAgentMessageOutcome::NotActiveOrFinalizing
        }
        ActiveMessageCompletionOutcome::Cancelled
        | ActiveMessageCompletionOutcome::DeadlineElapsed => {
            ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline
        }
    }
}

/// Reply when the completion is dropped after poll: a committed/claimed
/// admission cannot become a definite rejection.
fn lost_completion_outcome(
    outcome: ActiveMessageCompletionOutcome,
    is_settled: bool,
) -> ActiveAgentMessageOutcome {
    match (outcome, is_settled) {
        (ActiveMessageCompletionOutcome::Admission(ActiveMessageAdmission::Admitted), _)
        | (_, false) => ActiveAgentMessageOutcome::AdmissionUncertain,
        // Admitted is classified above; the dummy id is never used for Accepted.
        (outcome, true) => protocol_outcome_from_completion(outcome, true, ""),
    }
}

impl Drop for ActiveMessageCompletion {
    fn drop(&mut self) {
        if let Some(respond_to) = self.respond_to.take() {
            let _ = respond_to.send(lost_completion_outcome(self.outcome, self.is_settled));
        }
    }
}

pub(super) struct ActiveMessageFuture {
    subagent_id: String,
    generation: ActiveChildGeneration,
    parent_session_id: String,
    message_id: String,
    future: Pin<Box<dyn Future<Output = ActiveMessageAdmission> + Send + 'static>>,
    cancellation: Pin<Box<WaitForCancellationFutureOwned>>,
    deadline: Pin<Box<tokio::time::Sleep>>,
    lease: Arc<ActiveMessageAdmissionLease>,
    ingress_permit: Option<OwnedSemaphorePermit>,
    respond_to: Option<oneshot::Sender<ActiveAgentMessageOutcome>>,
}

impl Future for ActiveMessageFuture {
    type Output = ActiveMessageCompletion;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let outcome = match this.future.as_mut().poll(cx) {
            Poll::Ready(admission) => ActiveMessageCompletionOutcome::Admission(admission),
            Poll::Pending if this.cancellation.as_mut().poll(cx).is_ready() => {
                ActiveMessageCompletionOutcome::Cancelled
            }
            Poll::Pending if this.deadline.as_mut().poll(cx).is_ready() => {
                ActiveMessageCompletionOutcome::DeadlineElapsed
            }
            Poll::Pending => return Poll::Pending,
        };
        let is_settled = match outcome {
            ActiveMessageCompletionOutcome::Admission(admission) => this.lease.settle(admission),
            ActiveMessageCompletionOutcome::Cancelled
            | ActiveMessageCompletionOutcome::DeadlineElapsed => this.lease.revoke(),
        };
        Poll::Ready(ActiveMessageCompletion {
            subagent_id: this.subagent_id.clone(),
            generation: this.generation,
            parent_session_id: this.parent_session_id.clone(),
            message_id: this.message_id.clone(),
            respond_to: Some(
                this.respond_to
                    .take()
                    .unwrap_or_else(|| unreachable!("active-message future polled twice")),
            ),
            outcome,
            is_settled,
            _ingress_permit: this
                .ingress_permit
                .take()
                .unwrap_or_else(|| unreachable!("active-message ingress permit taken twice")),
        })
    }
}

impl Drop for ActiveMessageFuture {
    fn drop(&mut self) {
        let outcome = if self.lease.revoke() {
            ActiveAgentMessageOutcome::ChannelClosed
        } else {
            ActiveAgentMessageOutcome::AdmissionUncertain
        };
        if let Some(respond_to) = self.respond_to.take() {
            let _ = respond_to.send(outcome);
        }
    }
}

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub(super) fn handle_send_active_message(&mut self, ingress: ActiveMessageIngress) {
        let ActiveMessageIngress { request, permit } = ingress;
        let crate::implementations::grok_build::task::types::SubagentActiveMessageRequest {
            request,
            parent_session_id,
            respond_to,
        } = request;
        let Some(child) = self.active.get_mut(request.subagent_id()) else {
            let is_owned = self
                .pending
                .get(request.subagent_id())
                .is_some_and(|child| child.request.parent_session_id == parent_session_id)
                || self.queued.iter().any(|child| {
                    child.request.id == request.subagent_id()
                        && child.request.parent_session_id == parent_session_id
                })
                || self
                    .completed
                    .get(request.subagent_id())
                    .is_some_and(|child| child.request.parent_session_id == parent_session_id);
            let outcome = if is_owned {
                ActiveAgentMessageOutcome::NotActiveOrFinalizing
            } else {
                ActiveAgentMessageOutcome::NotFoundOrNotOwned
            };
            let _ = respond_to.send(outcome);
            return;
        };
        if child.request.parent_session_id != parent_session_id {
            let _ = respond_to.send(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
            return;
        }
        if child.request.owner.is_workflow() || child.cancellation.is_cancelled() {
            let _ = respond_to.send(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
            return;
        }
        match child.active_messages.begin_admission() {
            BeginAdmission::Started => {}
            BeginAdmission::Finalizing => {
                let _ = respond_to.send(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
                return;
            }
            BeginAdmission::Saturated => {
                let _ = respond_to.send(ActiveAgentMessageOutcome::Saturated {
                    max_in_flight: MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD,
                });
                return;
            }
        }

        let message_id = uuid::Uuid::now_v7().to_string();
        let lease = ActiveMessageAdmissionLease::new();
        let admission = child
            .control
            .send_active_message(ActiveAgentMessageDelivery::new(
                ActiveAgentMessage {
                    message_id: message_id.clone(),
                    sender_session_id: parent_session_id.clone(),
                    text: request.text().clone(),
                },
                request.operation(),
                Arc::clone(&lease),
            ));
        self.active_messages.push(ActiveMessageFuture {
            subagent_id: request.subagent_id().to_owned(),
            generation: child.generation,
            parent_session_id,
            message_id,
            future: admission,
            cancellation: Box::pin(child.cancellation.clone().cancelled_owned()),
            deadline: Box::pin(tokio::time::sleep(ACTIVE_MESSAGE_ADMISSION_TIMEOUT)),
            lease,
            ingress_permit: Some(permit),
            respond_to: Some(respond_to),
        });
    }

    pub(super) fn finish_active_message(&mut self, mut completion: ActiveMessageCompletion) {
        let Some(respond_to) = completion.respond_to.take() else {
            return;
        };
        let Some(child) = self
            .active
            .get_mut(&completion.subagent_id)
            .filter(|child| {
                child.generation == completion.generation
                    && child.request.parent_session_id == completion.parent_session_id
            })
        else {
            let _ = respond_to.send(lost_completion_outcome(
                completion.outcome,
                completion.is_settled,
            ));
            return;
        };
        let protocol_outcome = protocol_outcome_from_completion(
            completion.outcome,
            completion.is_settled,
            &completion.message_id,
        );
        let _ = respond_to.send(protocol_outcome);
        let terminal_disposition = child
            .active_messages
            .finish_admission(completion.is_settled);
        if let Some(is_clean) = terminal_disposition
            && let Some(output) = self.terminal_outputs.remove(&completion.subagent_id)
        {
            self.finish_terminalized_child(&completion.subagent_id, output, is_clean);
        }
    }

    pub(super) fn handle_active_message_finalizing(
        &mut self,
        subagent_id: String,
        respond_to: oneshot::Sender<bool>,
    ) {
        let Some(child) = self.active.get_mut(&subagent_id) else {
            let _ = respond_to.send(false);
            return;
        };
        child.active_messages.begin_finalizing(respond_to);
    }
}

#[cfg(test)]
#[path = "active_message_tests.rs"]
mod tests;
