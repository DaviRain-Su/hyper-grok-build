//! Wire test: `log_event(ManualAuth)` must POST to the product events endpoint as
//! `grok-shell-manual_auth` with the `reason`/`trigger`/`token_kind`/`principal`
//! the `distinct(principal)` alert consumes. Mocks the observability backend
//! (real HTTP collector) so the emit->wire path is checked, not just the struct.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use xai_grok_telemetry::client;
use xai_grok_telemetry::config::{TelemetryConfig, TelemetryMode};
use xai_grok_telemetry::events::{AuthTokenKind, ManualAuth, ManualAuthReason, ManualAuthSurface};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_auth_posts_to_events_endpoint_as_grok_shell_manual_auth() {
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = bodies.clone();
    let app = axum::Router::new().route(
        "/events",
        axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(v);
                axum::http::StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/events", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    client::init(
        TelemetryConfig {
            events_url: Some(url),
            events_api_key: Some("test-key".into()),
            mixpanel_enabled: false,
            ..TelemetryConfig::default()
        },
        TelemetryMode::Enabled,
        Some("user-xyz".into()),
        None,
        None,
        None,
        "0.0.0-test".into(),
        None,
        reqwest::Client::new(),
    );

    xai_grok_telemetry::log_event(ManualAuth {
        reason: ManualAuthReason::RefreshTokenRejected,
        trigger: ManualAuthSurface::Turn,
        token_kind: AuthTokenKind::OidcSession,
        principal: Some("user-xyz".into()),
    });

    // The emit is fire-and-forget; poll the collector for the POST.
    let deadline = Instant::now() + Duration::from_secs(5);
    let event = loop {
        let found = bodies.lock().unwrap().iter().find_map(|b| {
            let e = b.get("events")?.get(0)?;
            (e.get("event_name")?.as_str()? == "grok-shell-manual_auth").then(|| e.clone())
        });
        if let Some(e) = found {
            break e;
        }
        assert!(
            Instant::now() < deadline,
            "no grok-shell-manual_auth POST received"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let meta = event.get("event_metadata").expect("event_metadata present");
    assert_eq!(
        meta.get("reason").and_then(|v| v.as_str()),
        Some("refresh_token_rejected"),
    );
    assert_eq!(meta.get("trigger").and_then(|v| v.as_str()), Some("turn"));
    assert_eq!(
        meta.get("token_kind").and_then(|v| v.as_str()),
        Some("oidc_session"),
    );
    assert_eq!(
        meta.get("principal").and_then(|v| v.as_str()),
        Some("user-xyz"),
        "principal must be a queryable top-level metadata field for distinct() counting",
    );

    server.abort();
}
