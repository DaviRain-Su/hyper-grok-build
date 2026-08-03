use super::*;
use crate::headless::reducer::StreamEvent;
use pretty_assertions::assert_eq;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogs;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture_logs(f: impl FnOnce()) -> String {
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    logs.text()
}

fn make_ext_notif(
    method: &str,
    update: serde_json::Value,
) -> xai_acp_lib::AcpArgsBox<acp::ExtNotification> {
    let payload = serde_json::json!({
        "sessionId": "sess-1",
        "update": update,
    });
    let raw = serde_json::value::to_raw_value(&payload).unwrap();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    xai_acp_lib::AcpArgs {
        request: acp::ExtNotification::new(method, raw.into()),
        response_tx: tx,
    }
    .boxed()
}

#[test]
fn headless_task_backgrounded_parses_task_id() {
    let notif = make_ext_notif(
        "x.ai/task_backgrounded",
        serde_json::json!({
            "sessionUpdate": "task_backgrounded",
            "task_id": "task-abc",
        }),
    );
    assert!(matches!(
        handle_ext_notification(&notif),
        ExtEvent::TaskBackgrounded { task_id, is_monitor: false } if task_id == "task-abc"
    ));
}

#[test]
fn headless_task_backgrounded_numeric_task_id_is_coerced() {
    let notif = make_ext_notif(
        "x.ai/task_backgrounded",
        serde_json::json!({
            "sessionUpdate": "task_backgrounded",
            "task_id": 4242,
        }),
    );
    assert!(matches!(
        handle_ext_notification(&notif),
        ExtEvent::TaskBackgrounded { task_id, is_monitor: false } if task_id == "4242"
    ));
}

#[test]
fn headless_task_completed_numeric_task_id_is_coerced() {
    let notif = make_ext_notif(
        "x.ai/task_completed",
        serde_json::json!({
            "sessionUpdate": "task_completed",
            "task_snapshot": { "task_id": 4242 }
        }),
    );
    assert!(matches!(
        handle_ext_notification(&notif),
        ExtEvent::TaskCompleted { task_id } if task_id == "4242"
    ));
}

#[test]
fn headless_task_backgrounded_with_monitor_description_is_monitor() {
    let notif = make_ext_notif(
        "x.ai/task_backgrounded",
        serde_json::json!({
            "sessionUpdate": "task_backgrounded",
            "task_id": "mon-1",
            "monitor_description": "watching logs",
        }),
    );
    assert!(matches!(
        handle_ext_notification(&notif),
        ExtEvent::TaskBackgrounded { task_id, is_monitor: true } if task_id == "mon-1"
    ));
}

#[test]
fn headless_task_completed_parses_task_id() {
    let notif = make_ext_notif(
        "x.ai/task_completed",
        serde_json::json!({
            "sessionUpdate": "task_completed",
            "task_snapshot": { "task_id": "task-abc" }
        }),
    );
    assert!(matches!(
        handle_ext_notification(&notif),
        ExtEvent::TaskCompleted { task_id } if task_id == "task-abc"
    ));
}

#[test]
fn headless_subagent_spawned_and_finished_parse() {
    let spawned = make_ext_notif(
        "x.ai/session_notification",
        serde_json::json!({
            "sessionUpdate": "subagent_spawned",
            "subagent_id": "sub-1",
            "parent_session_id": "p",
            "child_session_id": "c",
            "subagent_type": "explore",
            "description": "test"
        }),
    );
    assert!(matches!(
        handle_ext_notification(&spawned),
        ExtEvent::SubagentSpawned { subagent_id } if subagent_id == "sub-1"
    ));
    let finished = make_ext_notif(
        "x.ai/session_notification",
        serde_json::json!({
            "sessionUpdate": "subagent_finished",
            "subagent_id": "sub-1",
            "child_session_id": "c",
            "status": "completed",
            "tool_calls": 0,
            "turns": 1,
            "duration_ms": 5
        }),
    );
    assert!(matches!(
        handle_ext_notification(&finished),
        ExtEvent::SubagentFinished { subagent_id } if subagent_id == "sub-1"
    ));
}

#[test]
fn headless_response_completed_parses_per_response_fields() {
    let notif = make_ext_notif(
        "x.ai/session_notification",
        serde_json::json!({
            "sessionUpdate": "response_completed",
            "message_id": "msg_01",
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 4,
                "cache_read_input_tokens": 2,
                "cache_creation_input_tokens": 0,
            },
            "signature": "sig-xyz",
            "stop_sequence": "<END>",
        }),
    );
    let ExtEvent::Stream(event) = handle_ext_notification(&notif) else {
        panic!("expected Stream event");
    };
    let StreamEvent::ResponseCompleted {
        message_id,
        stop_reason,
        usage,
        signature,
        stop_sequence,
    } = *event
    else {
        panic!("expected ResponseCompleted");
    };
    assert_eq!(message_id.as_deref(), Some("msg_01"));
    assert_eq!(stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(signature.as_deref(), Some("sig-xyz"));
    assert_eq!(stop_sequence.as_deref(), Some("<END>"));
    let usage = usage.expect("usage present");
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.cache_read_input_tokens, 2);
}

#[test]
fn headless_response_started_parses_per_response_fields() {
    let notif = make_ext_notif(
        "x.ai/session_notification",
        serde_json::json!({
            "sessionUpdate": "response_started",
            "message_id": "msg_01",
            "model": "grok-4",
            "input_tokens": 42,
            "cache_read_input_tokens": 7,
            "cache_creation_input_tokens": 3,
        }),
    );
    let ExtEvent::Stream(event) = handle_ext_notification(&notif) else {
        panic!("expected Stream event");
    };
    let StreamEvent::ResponseStarted {
        message_id,
        model,
        input_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
    } = *event
    else {
        panic!("expected ResponseStarted");
    };
    assert_eq!(message_id.as_deref(), Some("msg_01"));
    assert_eq!(model.as_deref(), Some("grok-4"));
    assert_eq!(input_tokens, 42);
    assert_eq!(cache_read_input_tokens, 7);
    assert_eq!(cache_creation_input_tokens, 3);
}

#[test]
fn headless_reasoning_completed_parses_signature() {
    let notif = make_ext_notif(
        "x.ai/session_notification",
        serde_json::json!({
            "sessionUpdate": "reasoning_completed",
            "signature": "sig-xyz",
        }),
    );
    let ExtEvent::Stream(event) = handle_ext_notification(&notif) else {
        panic!("expected Stream event");
    };
    let StreamEvent::ReasoningCompleted { signature } = *event else {
        panic!("expected ReasoningCompleted");
    };
    assert_eq!(signature.as_deref(), Some("sig-xyz"));
}

#[test]
fn headless_undecodable_known_background_task_errors_not_silent() {
    let notif = make_ext_notif(
        "x.ai/task_backgrounded",
        serde_json::json!({
            "sessionUpdate": "task_backgrounded",
            "task_id": { "nested": "object" },
        }),
    );
    let mut is_none = false;
    let logs = capture_logs(|| {
        is_none = matches!(handle_ext_notification(&notif), ExtEvent::None);
    });
    assert!(is_none, "undecodable known method degrades to None");
    assert!(
        logs.contains("task_backgrounded"),
        "log names the method: {logs}"
    );
    assert!(logs.contains("ERROR"), "logged at error level: {logs}");
}

#[test]
fn headless_task_backgrounded_mismatched_tag_errors_not_silent() {
    let notif = make_ext_notif(
        "x.ai/task_backgrounded",
        serde_json::json!({
            "sessionUpdate": "task_completed",
            "task_id": "task-abc",
        }),
    );
    let mut is_none = false;
    let logs = capture_logs(|| {
        is_none = matches!(handle_ext_notification(&notif), ExtEvent::None);
    });
    assert!(is_none, "mismatched-tag known method degrades to None");
    assert!(
        logs.contains("task_backgrounded"),
        "log names the method: {logs}"
    );
    assert!(logs.contains("ERROR"), "logged at error level: {logs}");
}

#[test]
fn headless_task_completed_mismatched_tag_errors_not_silent() {
    let notif = make_ext_notif(
        "x.ai/task_completed",
        serde_json::json!({
            "sessionUpdate": "task_backgrounded",
            "task_snapshot": { "task_id": "task-abc" },
        }),
    );
    let mut is_none = false;
    let logs = capture_logs(|| {
        is_none = matches!(handle_ext_notification(&notif), ExtEvent::None);
    });
    assert!(is_none, "mismatched-tag known method degrades to None");
    assert!(
        logs.contains("task_completed"),
        "log names the method: {logs}"
    );
    assert!(logs.contains("ERROR"), "logged at error level: {logs}");
}

#[test]
fn headless_malformed_known_response_boundary_warns_not_silent() {
    let notif = make_ext_notif(
        "x.ai/session_notification",
        serde_json::json!({
            "sessionUpdate": "response_completed",
            "usage": "not-an-object",
        }),
    );
    let mut is_none = false;
    let logs = capture_logs(|| {
        is_none = matches!(handle_ext_notification(&notif), ExtEvent::None);
    });
    assert!(is_none, "malformed known notification degrades to None");
    assert!(
        logs.contains("session notification"),
        "warning describes the malformed session notification: {logs}"
    );
    assert!(logs.contains("WARN"), "logged at warn level: {logs}");
}

#[test]
fn headless_session_update_unknown_method_is_none() {
    let payload = serde_json::json!({
        "sessionId": "sess-1",
        "update": {
            "sessionUpdate": "subagent_spawned",
            "subagent_id": "sub-1"
        }
    });
    let raw = serde_json::value::to_raw_value(&payload).unwrap();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let notif = xai_acp_lib::AcpArgs {
        request: acp::ExtNotification::new("x.ai/other", raw.into()),
        response_tx: tx,
    }
    .boxed();
    assert!(matches!(handle_ext_notification(&notif), ExtEvent::None));
}

#[test]
fn headless_session_notification_task_tag_errors_not_silent() {
    for tag in ["task_backgrounded", "task_completed"] {
        let notif = make_ext_notif(
            "x.ai/session_notification",
            serde_json::json!({
                "sessionUpdate": tag,
                "task_id": "task-abc",
            }),
        );
        let mut is_none = false;
        let logs = capture_logs(|| {
            is_none = matches!(handle_ext_notification(&notif), ExtEvent::None);
        });
        assert!(is_none, "misrouted lifecycle tag degrades to None ({tag})");
        assert!(logs.contains(tag), "log names the tag {tag}: {logs}");
        assert!(
            logs.contains("ERROR"),
            "logged at error level ({tag}): {logs}"
        );
    }
}

#[test]
fn headless_session_notification_unknown_tag_is_clean_ignore() {
    let notif = make_ext_notif(
        "x.ai/session_notification",
        serde_json::json!({ "sessionUpdate": "totally_unknown_display_tag" }),
    );
    let mut is_none = false;
    let logs = capture_logs(|| {
        is_none = matches!(handle_ext_notification(&notif), ExtEvent::None);
    });
    assert!(is_none, "unknown display tag degrades to None");
    assert!(
        !logs.contains("ERROR"),
        "an unknown display tag is a clean ignore, not an error: {logs}"
    );
}
