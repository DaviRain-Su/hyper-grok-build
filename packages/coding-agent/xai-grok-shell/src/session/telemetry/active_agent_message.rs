//! Shell projection and emission for active-agent message lifecycle telemetry.

use std::time::Instant;

use xai_grok_telemetry::TelemetryCtx;
use xai_grok_telemetry::events::{
    ActiveAgentMessageCompleted as Completed, ActiveAgentMessageLimitHit as LimitHit,
    ActiveAgentMessageOutcome as Outcome, ActiveAgentMessageSettled as Settled,
    ActiveAgentMessageSettlementDisposition as SettlementDisposition,
};
use xai_grok_tools::implementations::grok_build::send_subagent_message::SendSubagentMessageOutput;
use xai_grok_tools::types::output::ToolOutput;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ActiveAgentMessageEvent {
    Completed(Completed),
    LimitHit(LimitHit),
    Settled(Settled),
}

#[derive(Clone)]
pub(crate) struct ActiveAgentMessageAdmissionTelemetry {
    pub admitted_at: Instant,
    pub parent_ctx: TelemetryCtx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActiveAgentMessageSettlementStatus {
    Completed,
    Failed,
    Cancelled,
    ReceiptClosed,
    TimedOut,
    AdmissionUncertain,
}

pub(crate) struct ActiveAgentMessageCompletedSettlement {
    pub is_result_success: bool,
    pub is_result_cancelled: bool,
    pub is_final_receipt_closed: bool,
}

pub(crate) fn classify_completed_settlement(
    settlement: ActiveAgentMessageCompletedSettlement,
) -> ActiveAgentMessageSettlementStatus {
    if settlement.is_result_cancelled {
        ActiveAgentMessageSettlementStatus::Cancelled
    } else if settlement.is_final_receipt_closed {
        ActiveAgentMessageSettlementStatus::ReceiptClosed
    } else if settlement.is_result_success {
        ActiveAgentMessageSettlementStatus::Completed
    } else {
        ActiveAgentMessageSettlementStatus::Failed
    }
}

pub(crate) trait ActiveAgentMessageEventSink {
    fn emit(&mut self, event: ActiveAgentMessageEvent);
}

pub(crate) struct ProductEventSink;

impl ActiveAgentMessageEventSink for ProductEventSink {
    fn emit(&mut self, event: ActiveAgentMessageEvent) {
        match event {
            ActiveAgentMessageEvent::Completed(event) => {
                xai_grok_telemetry::session_ctx::log_event(event);
            }
            ActiveAgentMessageEvent::LimitHit(event) => {
                xai_grok_telemetry::session_ctx::log_event(event);
            }
            ActiveAgentMessageEvent::Settled(event) => {
                xai_grok_telemetry::session_ctx::log_event(event);
            }
        }
    }
}

fn emit_immediate_events<S: ActiveAgentMessageEventSink + ?Sized>(
    output: &ToolOutput,
    duration_ms: u64,
    sink: &mut S,
) {
    let ToolOutput::SendSubagentMessage(output) = output else {
        return;
    };
    let (outcome, limit_hit) = match output {
        SendSubagentMessageOutput::Accepted { .. } => (Outcome::Accepted, None),
        SendSubagentMessageOutput::NotFoundOrNotOwned => (Outcome::NotFoundOrNotOwned, None),
        SendSubagentMessageOutput::NotActiveOrFinalizing => (Outcome::NotActiveOrFinalizing, None),
        SendSubagentMessageOutput::Saturated { .. } => (Outcome::Saturated, None),
        SendSubagentMessageOutput::AdmissionUncertain => (Outcome::AdmissionUncertain, None),
        SendSubagentMessageOutput::NotAcceptedBeforeDeadline => {
            (Outcome::NotAcceptedBeforeDeadline, None)
        }
        SendSubagentMessageOutput::Unsupported => (Outcome::Unsupported, None),
        SendSubagentMessageOutput::Limit {
            max_bytes,
            observed_bytes,
        } if observed_bytes > max_bytes => (
            Outcome::Limit,
            Some(LimitHit {
                max_bytes: u64::try_from(*max_bytes).unwrap_or(u64::MAX),
                observed_bytes: u64::try_from(*observed_bytes).unwrap_or(u64::MAX),
            }),
        ),
        SendSubagentMessageOutput::Limit { .. } => (Outcome::Invalid, None),
        SendSubagentMessageOutput::ChannelClosed => (Outcome::ChannelClosed, None),
        _ => (Outcome::AdmissionUncertain, None),
    };
    sink.emit(ActiveAgentMessageEvent::Completed(Completed {
        outcome,
        duration_ms,
    }));
    if let Some(limit_hit) = limit_hit {
        sink.emit(ActiveAgentMessageEvent::LimitHit(limit_hit));
    }
}

pub(crate) fn record_completed_tool_output(output: &ToolOutput, duration_ms: u64) {
    #[cfg(test)]
    if try_emit_test_event(output, duration_ms) {
        return;
    }

    emit_immediate_events(output, duration_ms, &mut ProductEventSink);
}

#[cfg(test)]
thread_local! {
    static TEST_EVENT_SINK: std::cell::RefCell<Option<Box<dyn ActiveAgentMessageEventSink>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn try_emit_test_event(output: &ToolOutput, duration_ms: u64) -> bool {
    TEST_EVENT_SINK.with_borrow_mut(|slot| {
        let Some(sink) = slot.as_mut() else {
            return false;
        };
        emit_immediate_events(output, duration_ms, sink.as_mut());
        true
    })
}

#[cfg(test)]
struct CapturedEventSink {
    events: std::rc::Rc<std::cell::RefCell<Vec<ActiveAgentMessageEvent>>>,
}

#[cfg(test)]
impl ActiveAgentMessageEventSink for CapturedEventSink {
    fn emit(&mut self, event: ActiveAgentMessageEvent) {
        self.events.borrow_mut().push(event);
    }
}

#[cfg(test)]
struct TestEventSinkGuard(Option<Box<dyn ActiveAgentMessageEventSink>>);

#[cfg(test)]
impl Drop for TestEventSinkGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        TEST_EVENT_SINK.with_borrow_mut(|slot| *slot = previous);
    }
}

#[cfg(test)]
pub(crate) async fn capture_product_events<F: std::future::Future>(
    future: F,
) -> (F::Output, Vec<ActiveAgentMessageEvent>) {
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let previous = TEST_EVENT_SINK.with_borrow_mut(|slot| {
        slot.replace(Box::new(CapturedEventSink {
            events: std::rc::Rc::clone(&events),
        }))
    });
    let guard = TestEventSinkGuard(previous);
    let output = future.await;
    drop(guard);
    let captured = {
        let mut events = events.borrow_mut();
        std::mem::take(&mut *events)
    };
    (output, captured)
}

#[cfg(test)]
fn record_completed_tool_output_with_sink(
    output: &ToolOutput,
    duration_ms: u64,
    sink: &mut impl ActiveAgentMessageEventSink,
) {
    emit_immediate_events(output, duration_ms, sink);
}

fn project_settlement(
    admission: Option<ActiveAgentMessageAdmissionTelemetry>,
    status: ActiveAgentMessageSettlementStatus,
    settled_at: Instant,
) -> Option<(TelemetryCtx, Settled)> {
    let admission = admission?;
    let disposition = match status {
        ActiveAgentMessageSettlementStatus::Completed => SettlementDisposition::Completed,
        ActiveAgentMessageSettlementStatus::Failed => SettlementDisposition::Failed,
        ActiveAgentMessageSettlementStatus::Cancelled => SettlementDisposition::Cancelled,
        ActiveAgentMessageSettlementStatus::ReceiptClosed => SettlementDisposition::ReceiptClosed,
        ActiveAgentMessageSettlementStatus::TimedOut => SettlementDisposition::TimedOut,
        ActiveAgentMessageSettlementStatus::AdmissionUncertain => {
            SettlementDisposition::AdmissionUncertain
        }
    };
    Some((
        admission.parent_ctx,
        Settled {
            disposition,
            duration_ms: settled_at
                .saturating_duration_since(admission.admitted_at)
                .as_millis() as u64,
        },
    ))
}

pub(crate) async fn record_settlement(
    admission: Option<ActiveAgentMessageAdmissionTelemetry>,
    status: ActiveAgentMessageSettlementStatus,
) {
    let Some((parent_ctx, event)) = project_settlement(admission, status, Instant::now()) else {
        return;
    };
    xai_grok_telemetry::with_session_ctx(parent_ctx, async {
        ProductEventSink.emit(ActiveAgentMessageEvent::Settled(event));
    })
    .await;
}

#[cfg(test)]
#[path = "active_agent_message_tests.rs"]
mod tests;
