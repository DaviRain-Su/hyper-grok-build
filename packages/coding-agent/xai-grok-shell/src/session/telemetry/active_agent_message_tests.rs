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

fn capture(
    output: ToolOutput,
    requested_operation: ActiveAgentMessageOperation,
) -> Vec<ActiveAgentMessageEvent> {
    let mut sink = CapturingSink::default();
    record_completed_tool_output_with_sink(&output, requested_operation, 13, &mut sink);
    sink.events
}

fn admission(
    admitted_at: Instant,
    requested_operation: ActiveAgentMessageOperation,
    effective_operation: ActiveAgentMessageOperation,
    fallback_reason: Option<FallbackReason>,
) -> ActiveAgentMessageAdmissionTelemetry {
    ActiveAgentMessageAdmissionTelemetry::new(
        admitted_at,
        TelemetryCtx::new(
            "parent".to_owned(),
            std::sync::Arc::new(tokio::sync::Mutex::new(4)),
        ),
        requested_operation,
        effective_operation,
        fallback_reason,
    )
}

#[test]
fn completed_output_projection_emits_exactly_one_completion() {
    let private_identifier = "do-not-emit-this-id";
    let events = capture(
        ToolOutput::SendSubagentMessage(SendSubagentMessageOutput::Accepted {
            message_id: private_identifier.to_owned(),
        }),
        ActiveAgentMessageOperation::Steer,
    );

    assert_eq!(events.len(), 1);
    assert_eq!(event_name(&events[0]), "active_agent_message_completed");
    assert!(matches!(
        &events[0],
        ActiveAgentMessageEvent::Completed(Completed {
            outcome: Outcome::Accepted,
            requested_operation: Operation::Steer,
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
    let oversize = capture(
        ToolOutput::SendSubagentMessage(SendSubagentMessageOutput::Limit {
            max_bytes: 8,
            observed_bytes: 9,
        }),
        ActiveAgentMessageOperation::Steer,
    );
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
                requested_operation: Operation::Steer,
                duration_ms: 13,
            }),
            ActiveAgentMessageEvent::LimitHit(LimitHit {
                max_bytes: 8,
                observed_bytes: 9,
            }),
        ]
    ));

    let empty = capture(
        ToolOutput::SendSubagentMessage(SendSubagentMessageOutput::Limit {
            max_bytes: 8,
            observed_bytes: 0,
        }),
        ActiveAgentMessageOperation::Queue,
    );
    assert_eq!(empty.len(), 1);
    assert!(matches!(
        empty.as_slice(),
        [ActiveAgentMessageEvent::Completed(Completed {
            outcome: Outcome::Invalid,
            requested_operation: Operation::Queue,
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
        let events = capture(
            ToolOutput::SendSubagentMessage(output),
            ActiveAgentMessageOperation::Steer,
        );
        assert!(matches!(
            events.first(),
            Some(ActiveAgentMessageEvent::Completed(Completed {
                outcome,
                requested_operation: Operation::Steer,
                duration_ms: 13,
            })) if *outcome == expected
        ));
    }
}

#[test]
fn completed_output_projection_ignores_other_outputs() {
    assert!(
        capture(
            ToolOutput::SearchTool(SearchToolOutput {
                result_count: 0,
                content: String::new(),
            }),
            ActiveAgentMessageOperation::Steer,
        )
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
        Some(admission(
            admitted_at,
            ActiveAgentMessageOperation::Steer,
            ActiveAgentMessageOperation::Steer,
            None,
        )),
        status,
        admitted_at + std::time::Duration::from_millis(9),
    )
    .expect("admitted settlement must project");
    sink.emit(ActiveAgentMessageEvent::Settled(event));

    assert!(matches!(
        sink.events.as_slice(),
        [ActiveAgentMessageEvent::Settled(Settled {
            disposition: SettlementDisposition::Cancelled,
            requested_operation: Operation::Steer,
            effective_operation: Operation::Steer,
            fallback_disposition: FallbackDisposition::NotApplicable,
            fallback_reason: None,
            safe_point_latency_ms: None,
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
        let telemetry = admission(
            admitted_at,
            ActiveAgentMessageOperation::Steer,
            ActiveAgentMessageOperation::Steer,
            None,
        );
        telemetry.record_safe_point_delivery(admitted_at + std::time::Duration::from_millis(3));
        let projected = project_settlement(
            Some(telemetry),
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
                requested_operation: Operation::Steer,
                effective_operation: Operation::Steer,
                fallback_disposition: FallbackDisposition::NotApplicable,
                fallback_reason: None,
                safe_point_latency_ms: Some(3),
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

#[test]
fn fallback_projection_reports_idle_and_terminal_queue_reasons() {
    let admitted_at = Instant::now();
    for (effective, reason) in [
        (ActiveAgentMessageOperation::Queue, FallbackReason::Idle),
        (
            ActiveAgentMessageOperation::Steer,
            FallbackReason::Completion,
        ),
        (
            ActiveAgentMessageOperation::Steer,
            FallbackReason::SoftCancel,
        ),
        (ActiveAgentMessageOperation::Steer, FallbackReason::Rewind),
    ] {
        let telemetry = admission(
            admitted_at,
            ActiveAgentMessageOperation::Steer,
            effective,
            (reason == FallbackReason::Idle).then_some(reason),
        );
        if reason != FallbackReason::Idle {
            telemetry.record_fallback(reason);
        }
        let (_, settled) = project_settlement(
            Some(telemetry),
            ActiveAgentMessageSettlementStatus::Completed,
            admitted_at,
        )
        .expect("fallback settlement must project");
        assert_eq!(
            settled,
            Settled {
                disposition: SettlementDisposition::Completed,
                requested_operation: Operation::Steer,
                effective_operation: operation(effective),
                fallback_disposition: FallbackDisposition::Queued,
                fallback_reason: Some(reason),
                safe_point_latency_ms: None,
                duration_ms: 0,
            }
        );
    }
}
