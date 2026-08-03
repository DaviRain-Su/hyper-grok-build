//! End-to-end coverage for the reactive managed-MCP re-auth flow that the
//! sibling `reactive_managed_reauth_tests.rs` only exercises at the guard-rail
//! level (owner-scope + cooldown). Here we drive the full
//! `invalidate_cache → get_or_fetch → refresh_managed_clients → re-handshake`
//! loop against a real in-process HTTP MCP server and assert the wire-visible
//! `x.ai/mcp/server_status` pushes that clients consuming only `server_status`
//! (not the `mcp/list` snapshot) depend on.
//!
//! Unlike the unit harness, these tests KEEP `gw_rx` so the forwarded
//! `McpServerStatusPayload`s (`ready`/`managed_token_refreshed` on recovery,
//! `needsauth`/`auth_expired` on terminal exhaustion) can be asserted.
//!
//! The mock is a hand-rolled axum streamable-HTTP server (same shape proven to
//! handshake against rmcp 2.1 in `xai-grok-mcp/tests/repro_sse_flood.rs`): a
//! `POST` that answers `initialize` + `tools/list` while `reject == false` and
//! `401`s while `reject == true`, plus a standing-GET SSE stream. A separate
//! `GET /mcp/configs` route stands in for the cli-chat-proxy managed-config backend fetch, so
//! the re-auth loop's proxy round-trip is real too.

use crate::session::acp_session::support::*;
use crate::session::acp_session::*;
use crate::session::managed_mcp::{MANAGED_MCP_PREFIX, ManagedMcpConfig};
use crate::session::mcp_dispatcher::{
    McpServerStatus, McpServerStatusPayload, McpServerStatusReason, SERVER_STATUS_METHOD,
};
use agent_client_protocol as acp;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use xai_grok_mcp::servers::{ClientStateKind, HttpConfig, McpClient};

const MANAGED: &str = "grok_com_testconnector";

// ── Mock cli-chat-proxy + MCP server ──────────────────────────────────────

#[derive(Clone)]
struct MockState {
    /// While `true`, the MCP `POST` 401s — a silently-revoked token.
    reject: Arc<AtomicBool>,
    /// Count of managed-config fetches (`GET /mcp/configs`) so a cooldown-gated
    /// attempt can be proven to skip the network entirely.
    config_fetches: Arc<AtomicUsize>,
    /// The server's own MCP endpoint, echoed back in the managed config so the
    /// re-fetched config points the client at this same mock.
    mcp_url: String,
}

/// Stand-in for cli-chat-proxy `GET /v1/mcp/configs`: always succeeds (a
/// revoked *connector* token still re-fetches a fresh *proxy* token), so the
/// re-handshake outcome is governed solely by the `reject` flag.
async fn handle_configs(State(s): State<MockState>) -> Response {
    s.config_fetches.fetch_add(1, Ordering::Relaxed);
    let body = serde_json::json!({
        "mcp_servers": [{
            "name": "testconnector",
            "endpoint": s.mcp_url,
            "headers": {"Authorization": "Bearer fresh"},
            "token_expires_at": (Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
            "scope": "workspace",
        }]
    });
    axum::Json(body).into_response()
}

/// MCP streamable-HTTP `POST`. 401s while `reject`; otherwise a minimal valid
/// `initialize` result (with the mandatory `mcp-session-id` header) and an
/// empty `tools/list`, so a re-handshake succeeds.
async fn handle_mcp_post(
    State(s): State<MockState>,
    axum::Json(req): axum::Json<serde_json::Value>,
) -> Response {
    if s.reject.load(Ordering::Relaxed) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match req["method"].as_str() {
        Some("initialize") => {
            let result = serde_json::json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {
                    "protocolVersion": req["params"]["protocolVersion"],
                    "capabilities": {},
                    "serverInfo": {"name": "mock", "version": "0.0.0"},
                },
            });
            ([("mcp-session-id", "mock-session-1")], axum::Json(result)).into_response()
        }
        Some("tools/list") => {
            let result = serde_json::json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {"tools": []},
            });
            axum::Json(result).into_response()
        }
        // notifications/initialized and anything else.
        _ => StatusCode::ACCEPTED.into_response(),
    }
}

/// Standing-GET SSE stream that stays open (a healthy server) — rmcp opens it
/// after `initialize`; a pending body keeps it alive without reconnect churn.
async fn handle_mcp_get() -> Response {
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(futures::stream::pending::<Result<String, std::io::Error>>()),
    )
        .into_response()
}

/// Bind on an ephemeral port and serve the mock. Returns the proxy base URL
/// (`http://addr` — the managed fetch appends `/mcp/configs`), the MCP endpoint
/// URL, and the fetch counter.
async fn spawn_mock(reject: Arc<AtomicBool>) -> (String, String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    let proxy_base = format!("http://{addr}");
    let mcp_url = format!("http://{addr}/mcp");
    let config_fetches = Arc::new(AtomicUsize::new(0));
    let state = MockState {
        reject,
        config_fetches: config_fetches.clone(),
        mcp_url: mcp_url.clone(),
    };
    let app = axum::Router::new()
        .route("/mcp/configs", get(handle_configs))
        .route("/mcp", post(handle_mcp_post).get(handle_mcp_get))
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (proxy_base, mcp_url, config_fetches)
}

// ── Test wiring helpers ────────────────────────────────────────────────────

/// Build an actor with a live (disk-backed, env-free) `AuthManager` holding a
/// valid token and a `ModelsManager` whose cli-chat-proxy points at `proxy_base`
/// — the two pieces the default test actor lacks, both required for the inner
/// re-fetch to reach the mock instead of the real proxy.
async fn actor_with_proxy(
    proxy_base: &str,
    gw_tx: tokio::sync::mpsc::UnboundedSender<xai_acp_lib::AcpClientMessage>,
) -> (SessionActor, tempfile::TempDir) {
    let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = create_test_actor(100, 128_000, 80, gw_tx, persist_tx).await;

    let home = tempfile::tempdir().expect("tempdir");
    let auth_manager = Arc::new(crate::auth::AuthManager::new_test_isolated(
        home.path(),
        crate::auth::GrokComConfig::default(),
    ));
    // Valid (1h) token in-memory only — `auth()` fast-paths it without network.
    auth_manager.hot_swap(crate::auth::GrokAuth {
        expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
        ..crate::auth::GrokAuth::test_default()
    });

    let cfg = crate::agent::config::Config {
        endpoints: crate::agent::config::EndpointsConfig {
            cli_chat_proxy_base_url: Some(proxy_base.to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    actor.models_manager = crate::agent::models::ModelsManager::new(
        None,
        Default::default(),
        acp::ModelId::new("default"),
        auth_manager.clone(),
        cfg,
    );
    actor.auth_manager = Some(auth_manager);
    (actor, home)
}

/// Seed `actor` so it owns a managed client pointed at `mcp_url` with a STALE
/// token, plus the matching config entry `refresh_managed_clients` keys the
/// in-place swap on, plus a `Ready` managed cache.
async fn seed_managed(actor: &SessionActor, mcp_url: &str) {
    {
        let mut st = actor.mcp_state.lock().await;
        // `refresh_managed_clients` matches the owned client to a fresh config
        // by looking up `configs` for an Http server with the same name.
        st.configs = vec![acp::McpServer::Http(
            acp::McpServerHttp::new(MANAGED.to_string(), mcp_url.to_string()).headers(vec![]),
        )];
        st.owned_clients.insert(
            MANAGED.to_string(),
            Arc::new(McpClient::new_http(
                MANAGED.to_string(),
                HttpConfig {
                    url: mcp_url.to_string(),
                    headers: vec![("Authorization".into(), "Bearer stale".into())],
                },
                None,
                None,
            )),
        );
    }
    let handle = actor.managed_mcp_handle.clone();
    let mut configs = HashMap::new();
    configs.insert("Authorization".to_string(), "Bearer fresh".to_string());
    handle.lock().await.complete_fetch(
        vec![ManagedMcpConfig {
            name: "testconnector".to_string(),
            endpoint: mcp_url.to_string(),
            headers: configs,
            token_expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            scope: Some("workspace".to_string()),
            scope_id: None,
            scope_name: None,
        }],
        &handle,
        None,
    );
}

/// Drain all `x.ai/mcp/server_status` pushes currently queued on `gw_rx`.
fn drain_status_pushes(
    gw_rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) -> Vec<McpServerStatusPayload> {
    let mut out = Vec::new();
    while let Ok(msg) = gw_rx.try_recv() {
        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
            && args.request.method.as_ref() == SERVER_STATUS_METHOD
            && let Ok(payload) =
                serde_json::from_str::<McpServerStatusPayload>(args.request.params.get())
        {
            out.push(payload);
        }
    }
    out
}

// ── Case 1: recover-on-second-fetch (the happy path) ───────────────────────

/// A managed token rejected on the first re-handshake (`reject = true`) but
/// fixed before the next attempt recovers end-to-end: the client lands `Ready`
/// and a `ready`/`managed_token_refreshed` status push hits the wire.
///
/// The cooldown clear between attempts stands in for the proactive refresh's
/// `clear_reauth_cooldowns` (the real path that re-enables a re-authorized
/// connector) — driving real wall-clock backoff would be slow and flaky.
#[tokio::test(flavor = "multi_thread")]
async fn recovers_on_second_attempt_and_pushes_managed_token_refreshed() {
    let reject = Arc::new(AtomicBool::new(true));
    let (proxy_base, mcp_url, _fetches) = spawn_mock(reject.clone()).await;

    let (gw_tx, mut gw_rx) = tokio::sync::mpsc::unbounded_channel();

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            // `create_test_actor` spawns a local terminal task, so the actor
            // must be built inside the `LocalSet`.
            let (actor, _home) = actor_with_proxy(&proxy_base, gw_tx).await;
            seed_managed(&actor, &mcp_url).await;

            // Attempt 1: the connector still 401s, so the re-handshake fails.
            let first = actor.reactive_managed_reauth(MANAGED).await;
            assert!(first.is_err(), "first attempt must fail while rejecting");
            // Below the terminal cap, so no NeedsAuth push yet.
            assert!(
                drain_status_pushes(&mut gw_rx).is_empty(),
                "a single non-terminal failure must not push a status",
            );

            // Connector re-authorized; clear the cooldown (proactive-refresh
            // analog) and retry.
            reject.store(false, Ordering::Relaxed);
            actor
                .managed_mcp_handle
                .lock()
                .await
                .clear_reauth_cooldowns();

            actor
                .reactive_managed_reauth(MANAGED)
                .await
                .expect("second attempt must recover once the connector accepts");

            // Client is Ready and no longer parked needs-auth.
            let client = actor
                .mcp_state
                .lock()
                .await
                .get_client(MANAGED)
                .cloned()
                .expect("managed client present");
            assert_eq!(
                client.state_kind().await,
                ClientStateKind::Ready,
                "recovered client must be Ready",
            );
            assert!(
                !actor.mcp_state.lock().await.auth_required.contains(MANAGED),
                "recovered server must not be in auth_required",
            );

            let pushes = drain_status_pushes(&mut gw_rx);
            assert!(
                pushes.iter().any(|p| p.name == MANAGED
                    && p.status == McpServerStatus::Ready
                    && p.reason == McpServerStatusReason::ManagedTokenRefreshed),
                "expected a ready/managed_token_refreshed push, got: {pushes:?}",
            );
        })
        .await;
}

// ── Case 2: terminal after 3 failures ──────────────────────────────────────

/// Three consecutive failed re-auths park the connector in the terminal
/// `auth_required` state, push `needsauth`/`auth_expired`, and gate the 4th
/// immediate attempt (no extra config fetch).
///
/// Deterministic without sleeps: the default test actor has no `auth_manager`,
/// so the inner re-fetch fails fast (no network). Two prior failures are seeded
/// with an injected past `now` so their backoff windows are already elapsed
/// (the cooldown API takes `now`); the third — the real
/// `reactive_managed_reauth` call — is what crosses the terminal cap and fires
/// the NeedsAuth push.
#[tokio::test(flavor = "current_thread")]
async fn terminal_after_three_failures_pushes_needsauth_then_gates() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, mut gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(100, 128_000, 80, gw_tx, persist_tx).await;
            actor
                .mcp_state
                .lock()
                .await
                .owned_clients
                .insert(MANAGED.to_string(), Arc::new(McpClient::stub(MANAGED)));

            // Seed two prior failures whose backoff windows are already in the
            // past (failures = 2, below the cap of 3, so still eligible).
            let past = Utc::now() - chrono::Duration::hours(1);
            {
                let mut h = actor.managed_mcp_handle.lock().await;
                h.record_reauth_failure(MANAGED, past);
                h.record_reauth_failure(MANAGED, past);
                assert!(
                    h.reauth_allowed(MANAGED, Utc::now()),
                    "elapsed window + below cap must be eligible",
                );
                assert!(!h.reauth_is_terminal(MANAGED));
            }

            // Third (real) attempt: inner fails fast (no auth manager) and
            // records the terminal failure.
            let err = actor
                .reactive_managed_reauth(MANAGED)
                .await
                .expect_err("third attempt must fail");
            assert!(err.contains("auth manager"), "got: {err}");

            // Terminal: parked auth_required + NeedsAuth/auth_expired push.
            assert!(
                actor.mcp_state.lock().await.auth_required.contains(MANAGED),
                "exhausted connector must be parked in auth_required",
            );
            assert!(
                actor
                    .managed_mcp_handle
                    .lock()
                    .await
                    .reauth_is_terminal(MANAGED),
            );
            let pushes = drain_status_pushes(&mut gw_rx);
            assert!(
                pushes.iter().any(|p| p.name == MANAGED
                    && p.status == McpServerStatus::NeedsAuth
                    && p.reason == McpServerStatusReason::AuthExpired),
                "expected a needsauth/auth_expired push, got: {pushes:?}",
            );

            // Fourth immediate attempt is cooldown-gated by the terminal state.
            let err4 = actor
                .reactive_managed_reauth(MANAGED)
                .await
                .expect_err("fourth attempt must be gated");
            assert!(err4.contains("cooldown"), "got: {err4}");
            assert!(
                drain_status_pushes(&mut gw_rx).is_empty(),
                "a gated attempt must not push another status",
            );
        })
        .await;
}

// ── Case 3: entry-B classification ─────────────────────────────────────────

/// The mid-session entry-B gate routes an auth-rejection on a managed tool into
/// `reactive_managed_reauth` (observable as an armed cooldown) but leaves a
/// non-auth `Ok(is_error)` body (e.g. a 403 policy denial) untouched. Mirrors
/// the exact classifier (`is_auth_rejection_message`) + managed-prefix gate the
/// loop in `tool_calls.rs` keys on, plus the resulting side effect.
#[tokio::test(flavor = "current_thread")]
async fn entry_b_routes_auth_rejection_but_not_policy_denial() {
    use xai_grok_mcp::servers::{is_auth_rejection_message, parse_mcp_tool_name};

    // Managed-prefix gate: only `grok_com_*` tools enter entry-B.
    let (managed_server, _) =
        parse_mcp_tool_name(&format!("{MANAGED}__create_issue")).expect("qualified name");
    assert!(managed_server.starts_with(MANAGED_MCP_PREFIX));
    let (local_server, _) = parse_mcp_tool_name("github__create_issue").expect("qualified name");
    assert!(!local_server.starts_with(MANAGED_MCP_PREFIX));

    // Classification of both failure shapes entry-B sees:
    //   Err(ToolError)  -> err.to_string()
    //   Ok(is_error)    -> tool_result.prompt_text
    assert!(
        is_auth_rejection_message("MCP error: HTTP 401 Unauthorized"),
        "401 error string must route into re-auth",
    );
    assert!(
        is_auth_rejection_message("authentication required"),
        "auth wording must route into re-auth",
    );
    assert!(
        !is_auth_rejection_message("403 Forbidden: policy denied for this connector"),
        "a 403 policy denial must NOT route into re-auth",
    );

    // Side-effect check: only the auth path arms the cooldown. An owner stub
    // makes the inner re-fetch fail fast (no auth manager), arming the gate.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(100, 128_000, 80, gw_tx, persist_tx).await;
            actor
                .mcp_state
                .lock()
                .await
                .owned_clients
                .insert(MANAGED.to_string(), Arc::new(McpClient::stub(MANAGED)));

            // Non-auth body classified false -> entry-B does NOT call re-auth,
            // so the cooldown stays pristine.
            assert!(
                actor
                    .managed_mcp_handle
                    .lock()
                    .await
                    .reauth_allowed(MANAGED, Utc::now()),
                "no re-auth call yet: cooldown must be clean",
            );

            // Auth body classified true -> entry-B calls re-auth; the failed
            // attempt arms the cooldown window.
            let _ = actor.reactive_managed_reauth(MANAGED).await;
            assert!(
                !actor
                    .managed_mcp_handle
                    .lock()
                    .await
                    .reauth_allowed(MANAGED, Utc::now()),
                "the auth path must have armed the cooldown",
            );
        })
        .await;
}
