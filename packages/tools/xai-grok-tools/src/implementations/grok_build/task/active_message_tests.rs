use super::*;
use crate::implementations::grok_build::task::coordinator::ActiveMessageAdmission;

#[test]
fn admission_lease_settlement_table_is_exact() {
    use ActiveMessageAdmission::{Admitted, ChannelClosed, Rejected, Unsupported};
    use ActiveMessageLeaseState::{Claimed, Committed, Open, Revoked};

    for (state, admission, is_settled, expected_state) in [
        (Committed, Admitted, true, Committed),
        (Revoked, Admitted, false, Revoked),
        (Open, Admitted, false, Open),
        (Claimed, Admitted, false, Claimed),
        (Open, Unsupported, true, Revoked),
        (Revoked, Unsupported, true, Revoked),
        (Claimed, Unsupported, false, Claimed),
        (Committed, Unsupported, false, Committed),
        (Open, Rejected, true, Revoked),
        (Revoked, Rejected, true, Revoked),
        (Claimed, Rejected, false, Claimed),
        (Committed, Rejected, false, Committed),
        (Open, ChannelClosed, true, Revoked),
        (Revoked, ChannelClosed, true, Revoked),
        (Claimed, ChannelClosed, false, Claimed),
        (Committed, ChannelClosed, false, Committed),
    ] {
        let lease = ActiveMessageAdmissionLease::from_state(state);
        assert_eq!(is_settled, lease.settle(admission));
        assert_eq!(expected_state, lease.state());
    }
}

#[test]
fn public_message_literals_keep_their_three_field_shapes() {
    let _message = ActiveAgentMessage {
        message_id: "message".to_owned(),
        sender_session_id: "parent".to_owned(),
        text: Arc::from("follow up"),
    };
    let (respond_to, _) = tokio::sync::oneshot::channel();
    let _request = SubagentActiveMessageRequest {
        request: ActiveAgentMessageRequest::try_new("child", "follow up").unwrap(),
        parent_session_id: "parent".to_owned(),
        respond_to,
    };
}

#[test]
fn request_enforces_utf8_byte_cap() {
    let exact = "é".repeat(MAX_ACTIVE_AGENT_MESSAGE_BYTES / 2);
    assert!(ActiveAgentMessageRequest::try_new("sub-1", exact).is_ok());

    let oversized = "é".repeat(MAX_ACTIVE_AGENT_MESSAGE_BYTES / 2 + 1);
    assert_eq!(
        ActiveAgentMessageRequest::try_new("sub-1", oversized).unwrap_err(),
        ActiveAgentMessageOutcome::Limit {
            max_bytes: MAX_ACTIVE_AGENT_MESSAGE_BYTES,
            observed_bytes: MAX_ACTIVE_AGENT_MESSAGE_BYTES + 2,
        }
    );
    assert_eq!(
        ActiveAgentMessageRequest::try_new("sub-1", "").unwrap_err(),
        ActiveAgentMessageOutcome::Limit {
            max_bytes: MAX_ACTIVE_AGENT_MESSAGE_BYTES,
            observed_bytes: 0,
        }
    );
}
