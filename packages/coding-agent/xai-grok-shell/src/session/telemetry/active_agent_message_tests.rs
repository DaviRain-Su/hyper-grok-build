use super::*;
use xai_grok_telemetry::events::TelemetryEvent;
use xai_grok_tools::types::output::{SearchToolOutput, ToolOutput};

fn event_name(event: &ActiveAgentMessageEvent) -> &'static str {
    match event {
        ActiveAgentMessageEvent::Completed(_) => Completed::NAME,
        ActiveAgentMessageEvent::LimitHit(_) => LimitHit::NAME,
        ActiveAgentMessageEvent::Settled(_) => Settled::NAME,
    }
}

#[derive(Default)]
struct CapturingSink {
    events: Vec<ActiveAgentMessageEvent>,
}

impl ActiveAgentMessageEventSink for CapturingSink {
    fn emit(&mut self, event: ActiveAgentMessageEvent) {
        self.events.push(event);
    }
}

fn capture(output: ToolOutput) -> Vec<ActiveAgentMessageEvent> {
    let mut sink = CapturingSink::default();
    record_completed_tool_output_with_sink(&output, 13, &mut sink);
    sink.events
}

#[test]
fn completed_output_projection_emits_exactly_one_completion() {
    let private_identifier = "do-not-emit-this-id";
    let events = capture(ToolOutput::SendSubagentMessage(
        SendSubagentMessageOutput::Accepted {
            message_id: private_identifier.to_owned(),
        },
    ));

    assert_eq!(events.len(), 1);
    assert_eq!(event_name(&events[0]), "active_agent_message_completed");
    assert!(matches!(
        &events[0],
        ActiveAgentMessageEvent::Completed(Completed {
            outcome: Outcome::Accepted,
            duration_ms: 13,
        })
    ));
    let serialized = serde_json::to_string(match &events[0] {
        ActiveAgentMessageEvent::Completed(event) => event,
        _ => unreachable!("one accepted completion event was asserted above"),
    })
    .expect("serialize captured completion");
    assert!(!serialized.contains(private_identifier));
}

#[test]
fn completed_output_projection_emits_limit_only_for_real_oversize() {
    let oversize = capture(ToolOutput::SendSubagentMessage(
        SendSubagentMessageOutput::Limit {
            max_bytes: 8,
            observed_bytes: 9,
        },
    ));
    assert_eq!(
        oversize.iter().map(event_name).collect::<Vec<_>>(),
        [
            "active_agent_message_completed",
            "active_agent_message_limit_hit",
        ],
    );
    assert!(matches!(
        oversize.as_slice(),
        [
            ActiveAgentMessageEvent::Completed(Completed {
                outcome: Outcome::Limit,
                duration_ms: 13,
            }),
            ActiveAgentMessageEvent::LimitHit(LimitHit {
                max_bytes: 8,
                observed_bytes: 9,
            }),
        ]
    ));

    let empty = capture(ToolOutput::SendSubagentMessage(
        SendSubagentMessageOutput::Limit {
            max_bytes: 8,
            observed_bytes: 0,
        },
    ));
    assert_eq!(empty.len(), 1);
    assert!(matches!(
        empty.as_slice(),
        [ActiveAgentMessageEvent::Completed(Completed {
            outcome: Outcome::Invalid,
            duration_ms: 13,
        })]
    ));
}

#[test]
fn immediate_projection_covers_every_current_tool_outcome() {
    use SendSubagentMessageOutput as Send;

    for (output, expected) in [
        (
            Send::Accepted {
                message_id: "m".to_owned(),
            },
            Outcome::Accepted,
        ),
        (Send::NotFoundOrNotOwned, Outcome::NotFoundOrNotOwned),
        (Send::NotActiveOrFinalizing, Outcome::NotActiveOrFinalizing),
        (Send::Saturated { max_in_flight: 8 }, Outcome::Saturated),
        (Send::AdmissionUncertain, Outcome::AdmissionUncertain),
        (
            Send::NotAcceptedBeforeDeadline,
            Outcome::NotAcceptedBeforeDeadline,
        ),
        (Send::Unsupported, Outcome::Unsupported),
        (
            Send::Limit {
                max_bytes: 8,
                observed_bytes: 0,
            },
            Outcome::Invalid,
        ),
        (
            Send::Limit {
                max_bytes: 8,
                observed_bytes: 9,
            },
            Outcome::Limit,
        ),
        (Send::ChannelClosed, Outcome::ChannelClosed),
    ] {
        let events = capture(ToolOutput::SendSubagentMessage(output));
        assert!(matches!(
            events.first(),
            Some(ActiveAgentMessageEvent::Completed(Completed {
                outcome,
                duration_ms: 13,
            })) if *outcome == expected
        ));
    }
}

#[test]
fn completed_output_projection_ignores_other_outputs() {
    assert!(
        capture(ToolOutput::SearchTool(SearchToolOutput {
            result_count: 0,
            content: String::new(),
        }))
        .is_empty()
    );
}

#[test]
fn folded_cancellation_beats_closed_receipt_once_at_settlement_boundary() {
    let status = classify_completed_settlement(ActiveAgentMessageCompletedSettlement {
        is_result_success: false,
        is_result_cancelled: true,
        is_final_receipt_closed: true,
    });
    let admitted_at = Instant::now();
    let mut sink = CapturingSink::default();
    let (_, event) = project_settlement(
        Some(ActiveAgentMessageAdmissionTelemetry {
            admitted_at,
            parent_ctx: TelemetryCtx::new(
                "parent".to_owned(),
                std::sync::Arc::new(tokio::sync::Mutex::new(4)),
            ),
        }),
        status,
        admitted_at + std::time::Duration::from_millis(9),
    )
    .expect("admitted settlement must project");
    sink.emit(ActiveAgentMessageEvent::Settled(event));

    assert!(matches!(
        sink.events.as_slice(),
        [ActiveAgentMessageEvent::Settled(Settled {
            disposition: SettlementDisposition::Cancelled,
            duration_ms: 9,
        })]
    ));
}

#[tokio::test]
async fn settlement_projection_is_closed_and_suppresses_no_admission() {
    let admitted_at = Instant::now();
    for (status, expected) in [
        (
            ActiveAgentMessageSettlementStatus::Completed,
            SettlementDisposition::Completed,
        ),
        (
            ActiveAgentMessageSettlementStatus::Failed,
            SettlementDisposition::Failed,
        ),
        (
            ActiveAgentMessageSettlementStatus::Cancelled,
            SettlementDisposition::Cancelled,
        ),
        (
            ActiveAgentMessageSettlementStatus::ReceiptClosed,
            SettlementDisposition::ReceiptClosed,
        ),
        (
            ActiveAgentMessageSettlementStatus::TimedOut,
            SettlementDisposition::TimedOut,
        ),
        (
            ActiveAgentMessageSettlementStatus::AdmissionUncertain,
            SettlementDisposition::AdmissionUncertain,
        ),
    ] {
        let projected = project_settlement(
            Some(ActiveAgentMessageAdmissionTelemetry {
                admitted_at,
                parent_ctx: TelemetryCtx::new(
                    "parent".to_owned(),
                    std::sync::Arc::new(tokio::sync::Mutex::new(4)),
                ),
            }),
            status,
            admitted_at + std::time::Duration::from_millis(9),
        );
        let Some((ctx, settled)) = projected else {
            panic!("admitted settlement must project");
        };
        assert_eq!(ctx.session_id, "parent");
        assert_eq!(*ctx.prompt_index.lock().await, 4);
        assert!(matches!(
            settled,
            Settled {
                disposition,
                duration_ms: 9,
            } if disposition == expected
        ));
    }
    assert!(
        project_settlement(
            None,
            ActiveAgentMessageSettlementStatus::Completed,
            admitted_at,
        )
        .is_none()
    );
}
