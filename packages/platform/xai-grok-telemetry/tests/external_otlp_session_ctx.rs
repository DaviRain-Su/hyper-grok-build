//! Wire test for ambient-context injection: when events are emitted inside a
//! `with_session_ctx` scope, the external records must carry `session.id`,
//! `turn_number`, `prompt.id`, and a monotonic `event.sequence` — and
//! `prompt.id` must appear on events ONLY, never on metrics (unbounded
//! cardinality). Complements the other wire tests, which emit outside any ctx.

mod otlp_collector;

use std::sync::Arc;

use otlp_collector as col;
use xai_grok_telemetry::external;

#[test]
fn ambient_ctx_injects_session_turn_and_prompt_id() {
    let collected = col::Collected::default();
    let endpoint = col::start_collector(collected.clone());

    let mut cfg = external::ExternalOtelConfig::resolve_with(
        |name| match name {
            "GROK_EXTERNAL_OTEL" => Some("1".into()),
            "OTEL_LOGS_EXPORTER" | "OTEL_METRICS_EXPORTER" => Some("otlp".into()),
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(endpoint.clone()),
            "OTEL_METRIC_EXPORT_INTERVAL" => Some("150".into()),
            "OTEL_BLRP_SCHEDULE_DELAY" => Some("100".into()),
            _ => None,
        },
        None,
    )
    .expect("double opt-in must resolve");
    cfg.client = external::config::ExternalClientInfo {
        service_version: "0.0.0-test".into(),
        client_version: "0.0.0-test".into(),
        app_entrypoint: "cli".into(),
    };
    external::init(Some(cfg));
    assert!(external::is_active());

    // Emit inside a session ctx (turn_number = 3) so the ambient snapshot is
    // populated. `log_event` is synchronous and runs within the task-local
    // scope of `with_session_ctx`.
    let ctx = xai_grok_telemetry::TelemetryCtx::new(
        "sess-ctx".to_owned(),
        Arc::new(tokio::sync::Mutex::new(3usize)),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    rt.block_on(xai_grok_telemetry::with_session_ctx(ctx, async {
        xai_grok_telemetry::session_ctx::begin_prompt_id();
        xai_grok_telemetry::log_event(xai_grok_telemetry::events::PromptSubmitted {
            prompt_length: 42,
            model_id: "grok-4".into(),
            client_identifier: None,
            screen_mode: None,
            prompt_text: None,
        });
        xai_grok_telemetry::log_event(xai_grok_telemetry::events::ModelResponseReceived {
            model_id: "grok-4".into(),
            duration_ms: 5,
            stop_reason: Some("stop".into()),
            prompt_tokens: Some(11),
            completion_tokens: None,
            reasoning_tokens: None,
            cached_prompt_tokens: None,
        });
    }));

    external::flush();
    assert!(
        col::wait_until(std::time::Duration::from_secs(10), || {
            !collected.logs.lock().unwrap().is_empty()
                && !collected.metrics.lock().unwrap().is_empty()
        }),
        "collector must receive both signals"
    );

    // ── Event carries session.id, turn_number, prompt.id, event.sequence ──
    let prompt = col::find_event(&collected, "grok_code.user_prompt").expect("user_prompt present");
    assert_eq!(
        prompt.attrs.get("session.id").and_then(|v| v.as_str()),
        Some("sess-ctx"),
        "ambient session.id injected onto events"
    );
    assert_eq!(
        prompt.attrs.get("turn_number").and_then(|v| v.as_i64()),
        Some(3),
        "ambient turn_number injected onto events"
    );
    let prompt_id = prompt
        .attrs
        .get("prompt.id")
        .and_then(|v| v.as_str())
        .expect("prompt.id injected onto events");
    assert!(!prompt_id.is_empty(), "prompt.id must be a real uuid");
    assert!(
        prompt.attrs.contains_key("event.sequence"),
        "event.sequence injected onto every event"
    );

    // ── prompt.id / turn_number NEVER on metrics ────────────────────────
    let tokens = col::find_metric(&collected, "grok_code.token.usage");
    assert!(!tokens.is_empty(), "token.usage must export");
    for p in &tokens {
        assert!(
            !p.attrs.contains_key("prompt.id"),
            "prompt.id must never reach metrics"
        );
        assert!(
            !p.attrs.contains_key("turn_number"),
            "turn_number must never reach metrics"
        );
        // session.id DOES flow to metrics from the ambient ctx (cardinality
        // opt-in, default on).
        assert_eq!(
            p.attrs.get("session.id").and_then(|v| v.as_str()),
            Some("sess-ctx")
        );
    }

    external::shutdown();
}
