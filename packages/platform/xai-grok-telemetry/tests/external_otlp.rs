//! Integration test for the external OTEL stream against an in-process OTLP collector.
//! Covers wire payloads, delta temporality, canary absence at the wire layer with gates off, flush on shutdown within 2 s, and post-shutdown silence.

mod otlp_collector;

use otlp_collector as col;
use xai_grok_telemetry::external::IdentityAttrs;

const CANARY_MODEL: &str = "sk-CANARYabcdefghij1234567890";
const CANARY_PROMPT: &str = "CANARY_PROMPT_TEXT do not export";
const CANARY_MCP: &str = "canary-internal-mcp-server";
const CANARY_CMD: &str = "CANARY_BASH_COMMAND_ls_la";
const CANARY_RESPONSE: &str = "CANARY_ASSISTANT_PROSE do not export";
const OAUTH_EMAIL: &str = "otel.parity@example.com";

fn deny_decision(
    command: &str,
    tool_use_id: &str,
) -> xai_grok_telemetry::events::PermissionDecisionRecord {
    xai_grok_telemetry::events::PermissionDecisionRecord {
        payload: xai_grok_telemetry::events::PermissionDecisionPayload {
            tool_name: "run_terminal_cmd".into(),
            access_kind: xai_grok_telemetry::events::AccessKind::Bash,
            decision: xai_grok_telemetry::events::PermissionOutcome::Deny,
            wait_ms: 10,
            permission_mode: xai_grok_telemetry::enums::PermissionMode::Ask,
            source: Some("user_reject".into()),
            subagent_session_id: None,
            subagent_type: None,
            manager_prompt_attempted: None,
            prompt_outcome: None,
            prompt_outcome_detail: None,
            remember_tool_approvals: None,
            decision_reason: None,
            classifier_source: None,
            classifier_verdict: None,
            security_findings: None,
            classifier_latency_ms: None,
            auto_denials_consecutive: None,
            auto_denials_total: None,
        },
        tool_input: xai_grok_telemetry::events::ExternalToolInput {
            parameters: Some(serde_json::json!({ "command": command })),
            tool_use_id: Some(tool_use_id.into()),
        },
    }
}

#[test]
fn external_stream_end_to_end() {
    let collected = col::Collected::default();
    let endpoint = col::start_collector(collected.clone());

    // Resolve through the real config path (double opt-in, gates off).
    let mut cfg = xai_grok_telemetry::external::ExternalOtelConfig::resolve_with(
        |name| match name {
            "GROK_EXTERNAL_OTEL" => Some("1".into()),
            "OTEL_LOGS_EXPORTER" | "OTEL_METRICS_EXPORTER" => Some("otlp".into()),
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(endpoint.clone()),
            // Keep intervals short so the test is fast; flush() forces anyway.
            "OTEL_METRIC_EXPORT_INTERVAL" => Some("200".into()),
            "OTEL_BLRP_SCHEDULE_DELAY" => Some("100".into()),
            _ => None,
        },
        None,
    )
    .expect("double opt-in must resolve");
    cfg.client = xai_grok_telemetry::external::config::ExternalClientInfo {
        service_version: "0.0.0-test".into(),
        client_version: "0.0.0-test".into(),
        app_entrypoint: "cli".into(),
    };

    xai_grok_telemetry::external::init(Some(cfg));
    assert!(xai_grok_telemetry::external::is_active());

    // OAuth identity is not a content gate — email must still export with gates off.
    xai_grok_telemetry::external::set_identity(IdentityAttrs {
        user_id: Some("user-gates-off".into()),
        email: Some(OAUTH_EMAIL.into()),
        organization_id: None,
        team_id: None,
        deployment_id: None,
    });

    // Emit through the same funnel production uses, with the product events client never initialized and no auth at all
    // The external sink must fire anyway: it does not depend on product telemetry being enabled
    assert!(!xai_grok_telemetry::is_enabled());
    xai_grok_telemetry::log_event(xai_grok_telemetry::events::SessionNew {
        session_id: "sess-int-1".into(),
        client_identifier: None,
        client_version: None,
        is_git_repo: true,
        permission_mode: xai_grok_telemetry::enums::PermissionMode::Ask,
    });
    xai_grok_telemetry::log_event(xai_grok_telemetry::events::SessionHarness {
        session_id: "sess-int-1".into(),
        client_identifier: Some("grok-pager".into()),
        model_id: "grok-4".into(),
        agent_name: "grok-build-plan".into(),
        permission_mode: xai_grok_telemetry::enums::PermissionMode::Ask,
        mcp_server_names: vec![CANARY_MCP.into()],
        plugin_names: vec![],
        skill_names: vec![],
        lsp_server_names: vec![],
        hook_names: vec![],
        agents_md_dir_names: vec![],
        memory_enabled: false,
        memory_retrieval_mode: xai_grok_telemetry::events::MemoryRetrievalMode::Disabled,
        is_git_repo: true,
        auto_update: None,
    });
    xai_grok_telemetry::log_event(xai_grok_telemetry::events::PromptSubmitted {
        prompt_length: CANARY_PROMPT.len(),
        model_id: "grok-4".into(),
        client_identifier: None,
        screen_mode: None,
        prompt_text: Some(CANARY_PROMPT.into()),
        command_name: None,
    });
    // The model id is a canary: the scrub that runs when the metric increments must keep it out of the metrics body
    xai_grok_telemetry::log_event(xai_grok_telemetry::events::ModelResponseReceived {
        model_id: CANARY_MODEL.into(),
        duration_ms: 5,
        stop_reason: Some("stop".into()),
        prompt_tokens: Some(11),
        completion_tokens: Some(7),
        reasoning_tokens: None,
        cached_prompt_tokens: None,
        cache_creation_tokens: None,
        cost_usd_ticks: None,
    });
    xai_grok_telemetry::log_event(xai_grok_telemetry::events::ToolCallCompleted {
        tool_name: "run_terminal_cmd".into(),
        outcome: xai_grok_session_events::types::ToolOutcome::Success,
        hook_rewrote: false,
        duration_ms: 3,
        tool_result_size_bytes: None,
        file_path: None,
        parameters: Some(serde_json::json!({ "command": CANARY_CMD })),
        tool_use_id: Some("call-gates-off".into()),
        tool_output: None,
        error_message: None,
    });
    xai_grok_telemetry::log_event(deny_decision(CANARY_CMD, "call-deny-off"));
    xai_grok_telemetry::external::emit(&xai_grok_telemetry::events::AssistantResponse {
        response_length: CANARY_RESPONSE.len(),
        response_text: Some(CANARY_RESPONSE.into()),
    });

    xai_grok_telemetry::external::flush();

    assert!(
        col::wait_until(std::time::Duration::from_secs(10), || {
            collected.logs_len() > 0 && collected.metrics_len() > 0
        }),
        "collector must receive both signals"
    );

    // ── Logs payload ────────────────────────────────────────────────────
    let logs = col::decode_logs(&collected);
    let mut event_names: Vec<String> = vec![];
    let mut resource_service_name = None;
    for req in &logs {
        for rl in &req.resource_logs {
            if let Some(resource) = &rl.resource {
                for kv in &resource.attributes {
                    if kv.key == "service.name"
                        && let Some(v) = &kv.value
                    {
                        resource_service_name = Some(format!("{v:?}"));
                    }
                }
            }
            for sl in &rl.scope_logs {
                assert_eq!(
                    sl.scope.as_ref().map(|s| s.name.as_str()),
                    Some("ai.xai.grok_code")
                );
                for record in &sl.log_records {
                    event_names.push(record.event_name.clone());
                }
            }
        }
    }
    assert!(
        resource_service_name
            .as_deref()
            .is_some_and(|s| s.contains("grok-cli")),
        "service.name=grok-cli is a wire commitment: {resource_service_name:?}"
    );
    for expected in [
        "grok_code.session_start",
        "grok_code.user_prompt",
        "grok_code.api_request",
    ] {
        assert!(
            event_names.iter().any(|n| n == expected),
            "missing {expected} in {event_names:?}"
        );
    }
    // session_start arrives exactly once per emission (no double-send from the funnel)
    assert_eq!(
        event_names
            .iter()
            .filter(|n| *n == "grok_code.session_start")
            .count(),
        1
    );

    // ── Metrics payload: names + Delta temporality + session.count == 1 ──
    let metrics = col::decode_metrics(&collected);
    let mut metric_names = vec![];
    let mut session_count_total = 0u64;
    for req in &metrics {
        for rm in &req.resource_metrics {
            for sm in &rm.scope_metrics {
                for metric in &sm.metrics {
                    metric_names.push(metric.name.clone());
                    use opentelemetry_proto::tonic::metrics::v1::metric::Data;
                    if let Some(Data::Sum(sum)) = &metric.data {
                        assert_eq!(
                            sum.aggregation_temporality,
                            opentelemetry_proto::tonic::metrics::v1::AggregationTemporality::Delta
                                as i32,
                            "default temporality must be Delta (CC parity)"
                        );
                        if metric.name == "grok_code.session.count" {
                            for dp in &sum.data_points {
                                if let Some(
                                    opentelemetry_proto::tonic::metrics::v1::number_data_point::Value::AsInt(v),
                                ) = dp.value
                                {
                                    session_count_total += v as u64;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        metric_names.iter().any(|n| n == "grok_code.session.count"),
        "missing session.count in {metric_names:?}"
    );
    assert!(metric_names.iter().any(|n| n == "grok_code.token.usage"));
    assert_eq!(
        session_count_total, 1,
        "session.count must increment exactly once per SessionNew"
    );

    // ── Canary absence at the HTTP layer (raw bytes, both signals) ──────
    let raw_logs = collected.raw_logs();
    let raw_metrics = collected.raw_metrics();
    for (label, raw) in [("logs", &raw_logs), ("metrics", &raw_metrics)] {
        let haystack = String::from_utf8_lossy(raw);
        assert!(
            !haystack.contains("CANARY"),
            "canary reached the {label} wire: gates are off / scrub failed"
        );
        assert!(
            !haystack.contains(CANARY_MCP),
            "MCP server name reached the {label} wire"
        );
    }
    // Prompt length exported, text not (already covered by the canary scan).
    // Email is identity: present on logs and metrics even with content gates off.
    let prompt = col::find_event(&collected, "grok_code.user_prompt").expect("user_prompt");
    assert_eq!(
        prompt.attrs.get("user.email").and_then(|v| v.as_str()),
        Some(OAUTH_EMAIL)
    );
    assert!(!prompt.attrs.contains_key("prompt"));
    let assistant = col::find_event(&collected, "grok_code.assistant_response")
        .expect("assistant_response present with gates off");
    assert!(
        assistant.attrs.contains_key("response_length"),
        "response_length is always-on"
    );
    assert!(
        !assistant.attrs.contains_key("response"),
        "gated response must be absent with gates off"
    );
    assert!(!assistant.has_body, "no record may carry a body");
    let decision = col::find_event(&collected, "grok_code.tool_decision").expect("tool_decision");
    assert!(!decision.attrs.contains_key("tool_parameters"));
    assert!(!decision.attrs.contains_key("full_command"));
    let tool = col::find_event(&collected, "grok_code.tool_result").expect("tool_result");
    assert!(!tool.attrs.contains_key("full_command"));
    assert!(!tool.attrs.contains_key("tool_parameters"));
    assert!(!tool.attrs.contains_key("tool_input"));
    assert!(!tool.attrs.contains_key("tool_output"));
    let sessions = col::find_metric(&collected, "grok_code.session.count");
    assert!(
        sessions
            .iter()
            .any(|p| p.attrs.get("user.email").and_then(|v| v.as_str()) == Some(OAUTH_EMAIL)),
        "user.email must ride metrics when OAuth identity is set"
    );

    // ── Shutdown: ≤ 2 s + post-shutdown silence ─────────────────────────
    let start = std::time::Instant::now();
    xai_grok_telemetry::external::shutdown();
    assert!(
        start.elapsed() <= std::time::Duration::from_millis(2500),
        "shutdown watchdog must bound exit at ~2s (took {:?})",
        start.elapsed()
    );
    assert!(!xai_grok_telemetry::external::is_active());

    let logs_before = collected.logs_len();
    xai_grok_telemetry::log_event(xai_grok_telemetry::events::PromptSubmitted {
        prompt_length: 1,
        model_id: "grok-4".into(),
        client_identifier: None,
        screen_mode: None,
        prompt_text: None,
        command_name: None,
    });
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert_eq!(
        collected.logs_len(),
        logs_before,
        "no exports after shutdown"
    );

    // Idempotent shutdown: second call is a no-op, not an error/panic.
    xai_grok_telemetry::external::shutdown();
}
