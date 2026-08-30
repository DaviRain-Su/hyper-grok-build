//! Bounded protocol types for messages sent to active subagents.

use std::sync::Arc;

use educe::Educe;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use xai_tool_types::is_not_sentinel;

/// Maximum UTF-8 byte length of one in-memory V0 agent message.
pub const MAX_ACTIVE_AGENT_MESSAGE_BYTES: usize = 32 * 1024;

/// Closed delivery operation. No bool below the model adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveAgentMessageOperation {
    Queue,
    Steer,
}

/// Bounded caller request for the internal active-descendant route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveAgentMessageRequest {
    subagent_id: String,
    text: Arc<str>,
    operation: ActiveAgentMessageOperation,
}

impl ActiveAgentMessageRequest {
    pub fn try_new(
        subagent_id: impl Into<String>,
        text: impl Into<Arc<str>>,
    ) -> Result<Self, ActiveAgentMessageOutcome> {
        Self::try_new_with_operation(subagent_id, text, ActiveAgentMessageOperation::Queue)
    }

    pub fn try_new_with_operation(
        subagent_id: impl Into<String>,
        text: impl Into<Arc<str>>,
        operation: ActiveAgentMessageOperation,
    ) -> Result<Self, ActiveAgentMessageOutcome> {
        let subagent_id = subagent_id.into();
        if !is_not_sentinel(&subagent_id) {
            return Err(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
        }
        let text = text.into();
        if text.is_empty() {
            return Err(ActiveAgentMessageOutcome::Limit {
                max_bytes: MAX_ACTIVE_AGENT_MESSAGE_BYTES,
                observed_bytes: 0,
            });
        }
        if text.len() > MAX_ACTIVE_AGENT_MESSAGE_BYTES {
            return Err(ActiveAgentMessageOutcome::Limit {
                max_bytes: MAX_ACTIVE_AGENT_MESSAGE_BYTES,
                observed_bytes: text.len(),
            });
        }
        Ok(Self {
            subagent_id: subagent_id.trim().to_owned(),
            text,
            operation,
        })
    }

    pub fn subagent_id(&self) -> &str {
        &self.subagent_id
    }

    pub fn text(&self) -> &Arc<str> {
        &self.text
    }

    pub fn operation(&self) -> ActiveAgentMessageOperation {
        self.operation
    }
}

/// Server-authored message admitted by one active child runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveAgentMessage {
    pub message_id: String,
    pub sender_session_id: String,
    pub text: Arc<str>,
}

/// Coordinator-authorized delivery with synchronous insertion authority.
#[derive(Debug, Clone)]
pub struct ActiveAgentMessageDelivery {
    message: ActiveAgentMessage,
    operation: ActiveAgentMessageOperation,
    admission_lease: Arc<ActiveMessageAdmissionLease>,
}

impl ActiveAgentMessageDelivery {
    pub(crate) fn new(
        message: ActiveAgentMessage,
        operation: ActiveAgentMessageOperation,
        admission_lease: Arc<ActiveMessageAdmissionLease>,
    ) -> Self {
        Self {
            message,
            operation,
            admission_lease,
        }
    }

    pub fn message(&self) -> &ActiveAgentMessage {
        &self.message
    }

    pub fn operation(&self) -> ActiveAgentMessageOperation {
        self.operation
    }

    /// Run synchronous protected-row insertion only while admission is open.
    pub fn commit_admission<T>(&self, insert: impl FnOnce() -> T) -> Option<T> {
        self.admission_lease.commit_admission(insert)
    }

    #[cfg(test)]
    pub(crate) fn mark_admission_uncertain(&self) {
        self.admission_lease.state.store(
            ActiveMessageLeaseState::Claimed as u8,
            std::sync::atomic::Ordering::Release,
        );
    }
}

/// Result owned by a child host after coordinator authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveMessageAdmission {
    Admitted,
    Unsupported,
    ChannelClosed,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ActiveMessageLeaseState {
    Open,
    Claimed,
    Committed,
    Revoked,
}

pub(crate) struct ActiveMessageAdmissionLease {
    state: std::sync::atomic::AtomicU8,
}

impl std::fmt::Debug for ActiveMessageAdmissionLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveMessageAdmissionLease")
            .field(
                "state",
                &self.state.load(std::sync::atomic::Ordering::Acquire),
            )
            .finish()
    }
}

impl ActiveMessageAdmissionLease {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::atomic::AtomicU8::new(ActiveMessageLeaseState::Open as u8),
        })
    }

    /// Run synchronous protected-row insertion only while this lease is open.
    ///
    /// The closure cannot be async, so the claim cannot cross an await point.
    pub fn commit_admission<T>(&self, insert: impl FnOnce() -> T) -> Option<T> {
        self.state
            .compare_exchange(
                ActiveMessageLeaseState::Open as u8,
                ActiveMessageLeaseState::Claimed as u8,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()?;
        let inserted = insert();
        self.state.store(
            ActiveMessageLeaseState::Committed as u8,
            std::sync::atomic::Ordering::Release,
        );
        Some(inserted)
    }

    pub(crate) fn settle(&self, admission: ActiveMessageAdmission) -> bool {
        let state = self.state.load(std::sync::atomic::Ordering::Acquire);
        match admission {
            ActiveMessageAdmission::Admitted => state == ActiveMessageLeaseState::Committed as u8,
            ActiveMessageAdmission::Unsupported
            | ActiveMessageAdmission::ChannelClosed
            | ActiveMessageAdmission::Rejected => match state {
                value if value == ActiveMessageLeaseState::Open as u8 => self
                    .state
                    .compare_exchange(
                        ActiveMessageLeaseState::Open as u8,
                        ActiveMessageLeaseState::Revoked as u8,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    )
                    .is_ok(),
                value if value == ActiveMessageLeaseState::Revoked as u8 => true,
                _ => false,
            },
        }
    }

    pub(crate) fn revoke(&self) -> bool {
        match self.state.compare_exchange(
            ActiveMessageLeaseState::Open as u8,
            ActiveMessageLeaseState::Revoked as u8,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(value) => value == ActiveMessageLeaseState::Revoked as u8,
        }
    }

    #[cfg(test)]
    fn from_state(state: ActiveMessageLeaseState) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::atomic::AtomicU8::new(state as u8),
        })
    }

    #[cfg(test)]
    fn state(&self) -> ActiveMessageLeaseState {
        match self.state.load(std::sync::atomic::Ordering::Acquire) {
            value if value == ActiveMessageLeaseState::Open as u8 => ActiveMessageLeaseState::Open,
            value if value == ActiveMessageLeaseState::Claimed as u8 => {
                ActiveMessageLeaseState::Claimed
            }
            value if value == ActiveMessageLeaseState::Committed as u8 => {
                ActiveMessageLeaseState::Committed
            }
            value if value == ActiveMessageLeaseState::Revoked as u8 => {
                ActiveMessageLeaseState::Revoked
            }
            _ => unreachable!("invalid active-message lease state"),
        }
    }
}

/// Closed result of the internal active-descendant route.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActiveAgentMessageOutcome {
    Accepted {
        message_id: String,
    },
    NotFoundOrNotOwned,
    NotActiveOrFinalizing,
    Saturated {
        max_in_flight: usize,
    },
    /// A claimed or committed admission could not be resolved conclusively.
    AdmissionUncertain,
    /// The open admission lease was revoked before the deadline.
    NotAcceptedBeforeDeadline,
    Unsupported,
    Limit {
        max_bytes: usize,
        observed_bytes: usize,
    },
    ChannelClosed,
}

/// Coordinator command envelope with server-bound sender identity.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentActiveMessageRequest {
    pub request: ActiveAgentMessageRequest,
    pub parent_session_id: String,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<ActiveAgentMessageOutcome>,
}

#[derive(Debug)]
pub(crate) struct ActiveMessageIngress {
    pub(crate) request: SubagentActiveMessageRequest,
    pub(crate) permit: OwnedSemaphorePermit,
}

#[cfg(test)]
#[path = "active_message_tests.rs"]
mod tests;
