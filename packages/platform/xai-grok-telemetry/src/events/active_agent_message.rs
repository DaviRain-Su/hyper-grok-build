//! Content-free product telemetry for active messages between agents.

use serde::Serialize;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentMessageOutcome {
    /// The child host admitted the message; this does not describe follow-up completion.
    Accepted,
    NotFoundOrNotOwned,
    NotActiveOrFinalizing,
    Saturated,
    AdmissionUncertain,
    NotAcceptedBeforeDeadline,
    Unsupported,
    /// The request was invalid without exceeding the configured byte cap.
    Invalid,
    /// The request exceeded the configured byte cap.
    Limit,
    ChannelClosed,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct ActiveAgentMessageCompleted {
    pub outcome: ActiveAgentMessageOutcome,
    pub duration_ms: u64,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct ActiveAgentMessageLimitHit {
    pub max_bytes: u64,
    pub observed_bytes: u64,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentMessageSettlementDisposition {
    Completed,
    Failed,
    Cancelled,
    ReceiptClosed,
    TimedOut,
    AdmissionUncertain,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct ActiveAgentMessageSettled {
    pub disposition: ActiveAgentMessageSettlementDisposition,
    pub duration_ms: u64,
}

#[cfg(test)]
#[path = "active_agent_message_tests.rs"]
mod tests;
