use super::*;
use crate::events::TelemetryEvent;

#[test]
fn event_names_are_stable() {
    assert_eq!(
        ActiveAgentMessageCompleted::NAME,
        "active_agent_message_completed"
    );
    assert_eq!(
        ActiveAgentMessageLimitHit::NAME,
        "active_agent_message_limit_hit"
    );
    assert_eq!(
        ActiveAgentMessageSettled::NAME,
        "active_agent_message_settled"
    );
}

#[test]
fn immediate_outcomes_are_a_closed_content_free_contract() {
    let outcomes = [
        ActiveAgentMessageOutcome::Accepted,
        ActiveAgentMessageOutcome::NotFoundOrNotOwned,
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        ActiveAgentMessageOutcome::Saturated,
        ActiveAgentMessageOutcome::AdmissionUncertain,
        ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
        ActiveAgentMessageOutcome::Unsupported,
        ActiveAgentMessageOutcome::Invalid,
        ActiveAgentMessageOutcome::Limit,
        ActiveAgentMessageOutcome::ChannelClosed,
    ];
    assert_eq!(
        outcomes.map(|value| serde_json::to_value(value).expect("serialize outcome")),
        [
            "accepted",
            "not_found_or_not_owned",
            "not_active_or_finalizing",
            "saturated",
            "admission_uncertain",
            "not_accepted_before_deadline",
            "unsupported",
            "invalid",
            "limit",
            "channel_closed",
        ],
    );
    assert_eq!(
        serde_json::to_value(ActiveAgentMessageCompleted {
            outcome: ActiveAgentMessageOutcome::Accepted,
            duration_ms: 7,
        })
        .expect("serialize completion"),
        serde_json::json!({"outcome": "accepted", "duration_ms": 7}),
    );
}

#[test]
fn limit_and_settlement_payloads_use_fixed_width_content_free_fields() {
    assert_eq!(
        serde_json::to_value(ActiveAgentMessageLimitHit {
            max_bytes: 8_u64,
            observed_bytes: 9_u64,
        })
        .expect("serialize limit"),
        serde_json::json!({"max_bytes": 8, "observed_bytes": 9}),
    );

    let dispositions = [
        ActiveAgentMessageSettlementDisposition::Completed,
        ActiveAgentMessageSettlementDisposition::Failed,
        ActiveAgentMessageSettlementDisposition::Cancelled,
        ActiveAgentMessageSettlementDisposition::ReceiptClosed,
        ActiveAgentMessageSettlementDisposition::TimedOut,
        ActiveAgentMessageSettlementDisposition::AdmissionUncertain,
    ];
    assert_eq!(
        dispositions.map(|value| serde_json::to_value(value).expect("serialize disposition")),
        [
            "completed",
            "failed",
            "cancelled",
            "receipt_closed",
            "timed_out",
            "admission_uncertain",
        ],
    );
    assert_eq!(
        serde_json::to_value(ActiveAgentMessageSettled {
            disposition: ActiveAgentMessageSettlementDisposition::Completed,
            duration_ms: 11,
        })
        .expect("serialize settlement"),
        serde_json::json!({"disposition": "completed", "duration_ms": 11}),
    );
}
