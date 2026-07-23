//! Registry-churn regression gate: a real in-process `MvpAgent` on duplex
//! ACP pipes churns sessions through create, prompt, and close, then
//! asserts via `x.ai/debug/agent` that every registry count returns
//! to its pre-churn baseline. Deterministic counts, no memory thresholds.
//! Counts the echo workload never populates are pinned at their zero
//! baseline only.
use agent_client_protocol::{self as acp, Agent as _};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use xai_acp_lib::{
    AcpAgentGatewayReceiver as GatewayReceiver, AcpAgentGatewaySender as GatewaySender,
    LineBufferedRead,
};
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::agent::mvp_agent::MvpAgent;
use xai_grok_test_support::MockInferenceServer;
/// Matches production's `MAX_BUFFER_SIZE` in `agent::app`.
const DUPLEX_BUFFER_BYTES: usize = 8 * 1024 * 1024;
/// Enough that a per-cycle leak is unambiguous; well under a minute
/// against the loopback mock.
const CHURN_SESSIONS: usize = 15;
const CONCURRENT_SESSIONS: usize = 4;
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
/// Field names are the wire contract (`RegistrySnapshot` in
/// `agent/mvp_agent/session_lifecycle.rs`); `deny_unknown_fields` forces a
/// new server-side count to be mirrored and asserted here.
#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Counts {
    sessions: usize,
    session_threads: usize,
    dispatch_locks: usize,
    session_turn_numbers: usize,
    permission_event_receivers: usize,
    model_unavailable_sessions: usize,
    session_live_state: usize,
    session_index_claims: usize,
    require_gateway_sessions: usize,
    subagent_pending: usize,
    subagent_active: usize,
    subagent_completed: usize,
    workspace_bindings: Option<usize>,
}
struct AutoApproveClient;
#[async_trait::async_trait(?Send)]
impl acp::Client for AutoApproveClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let outcome = args
            .options
            .iter()
            .find(|o| o.kind == acp::PermissionOptionKind::AllowOnce)
            .or(args.options.first())
            .map(|o| {
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    o.option_id.clone(),
                ))
            })
            .unwrap_or(acp::RequestPermissionOutcome::Cancelled);
        Ok(acp::RequestPermissionResponse::new(outcome))
    }
    async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
        Ok(())
    }
}
async fn ext_method(
    conn: &acp::ClientSideConnection,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let raw =
        serde_json::value::RawValue::from_string(params.to_string()).expect("serialize ext params");
    let resp = tokio::time::timeout(
        RPC_TIMEOUT,
        conn.ext_method(acp::ExtRequest::new(method, Arc::from(raw))),
    )
    .await
    .unwrap_or_else(|_| panic!("{method} timed out"))
    .unwrap_or_else(|e| panic!("{method} failed: {e}"));
    serde_json::from_str(resp.0.get()).unwrap_or_else(|e| panic!("{method}: bad response: {e}"))
}
async fn read_counts(conn: &acp::ClientSideConnection) -> Counts {
    let resp = ext_method(conn, "x.ai/debug/agent", json!({})).await;
    serde_json::from_value(resp["result"]["registries"].clone())
        .unwrap_or_else(|e| panic!("x.ai/debug/agent: bad registries payload: {e}\n{resp}"))
}
async fn new_session(conn: &acp::ClientSideConnection, cwd: &std::path::Path) -> acp::SessionId {
    tokio::time::timeout(
        RPC_TIMEOUT,
        conn.new_session(
            acp::NewSessionRequest::new(cwd.to_path_buf())
                .meta(json!({ "modelId": "test-model" }).as_object().cloned()),
        ),
    )
    .await
    .expect("session/new timed out")
    .expect("session/new failed")
    .session_id
}
async fn prompt_turn(conn: &acp::ClientSideConnection, session_id: &acp::SessionId, text: &str) {
    let resp = tokio::time::timeout(
        RPC_TIMEOUT,
        conn.prompt(acp::PromptRequest::new(
            session_id.clone(),
            vec![acp::ContentBlock::Text(acp::TextContent::new(
                text.to_owned(),
            ))],
        )),
    )
    .await
    .unwrap_or_else(|_| panic!("prompt on {} timed out", session_id.0))
    .unwrap_or_else(|e| panic!("prompt on {} failed: {e}", session_id.0));
    assert!(
        matches!(resp.stop_reason, acp::StopReason::EndTurn),
        "expected EndTurn on {}, got {:?}",
        session_id.0,
        resp.stop_reason
    );
}
async fn close_session(conn: &acp::ClientSideConnection, session_id: &acp::SessionId) {
    let resp = ext_method(
        conn,
        "x.ai/session/close",
        json!({ "sessionId": session_id.0.as_ref() }),
    )
    .await;
    assert_eq!(
        resp["result"]["success"],
        json!(true),
        "x.ai/session/close on {} failed: {resp}",
        session_id.0
    );
}
async fn churn_one(conn: &acp::ClientSideConnection, cwd: &std::path::Path, label: usize) {
    let sid = new_session(conn, cwd).await;
    prompt_turn(conn, &sid, &format!("churn ping {label}")).await;
    close_session(conn, &sid).await;
}
/// Builds the in-process agent from the environment and returns an
/// initialized, authenticated client connection over duplex pipes. IO
/// tasks spawn on the current `LocalSet`.
async fn connect_and_auth() -> acp::ClientSideConnection {
    let agent_config = AgentConfig::default();
    let auth_manager = Arc::new(agent_config.create_auth_manager());
    let (gw_tx, gw_rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(gw_tx);
    let agent = MvpAgent::new(gateway, &agent_config, auth_manager, None).expect("valid config");
    let (c2a_a, c2a_b) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let (a2c_a, a2c_b) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let agent_incoming = LineBufferedRead::spawn_local(c2a_b.compat());
    let (agent_conn, agent_io) =
        acp::AgentSideConnection::new(agent, a2c_a.compat_write(), agent_incoming, |fut| {
            tokio::task::spawn_local(fut);
        });
    tokio::task::spawn_local(
        GatewayReceiver::new(gw_rx, agent_conn)
            .with_on_meta(xai_file_utils::trace_context::span_from_meta_traceparent)
            .run(),
    );
    tokio::task::spawn_local(agent_io);
    let client_incoming = LineBufferedRead::spawn_local(a2c_b.compat());
    let (client_conn, client_io) = acp::ClientSideConnection::new(
        AutoApproveClient,
        c2a_a.compat_write(),
        client_incoming,
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    tokio::task::spawn_local(client_io);
    let init = tokio::time::timeout(
        RPC_TIMEOUT,
        client_conn.initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                .client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                )
                .meta(
                    json!({
                        "startupHints": {
                            "nonInteractive": true,
                            "skipGitStatus": true,
                            "skipProjectLayout": true,
                        },
                        "clientType": "registry-churn-test",
                        "clientVersion": "0.0-test",
                    })
                    .as_object()
                    .cloned(),
                ),
        ),
    )
    .await
    .expect("initialize timed out")
    .expect("initialize failed");
    let method = init
        .auth_methods
        .iter()
        .find(|m| &*m.id().0 == "xai.api_key")
        .expect("xai.api_key auth method not advertised");
    tokio::time::timeout(
        RPC_TIMEOUT,
        client_conn.authenticate(
            acp::AuthenticateRequest::new(method.id().clone())
                .meta(json!({ "headless": true }).as_object().cloned()),
        ),
    )
    .await
    .expect("authenticate timed out")
    .expect("authenticate failed");
    client_conn
}
/// Single `#[test]` in this binary: the env mutation below relies on
/// nothing else running concurrently (same safety argument as
/// `git_contention_e2e`).
#[test]
fn session_churn_returns_registry_snapshot_to_baseline() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("mock runtime");
    let server = mock_rt
        .block_on(MockInferenceServer::start())
        .expect("mock server");
    let grok_home = TempDir::new().expect("grok home");
    let workdir = TempDir::new().expect("workdir");
    unsafe {
        std::env::set_var("GROK_HOME", grok_home.path());
        std::env::set_var("GROK_CLI_CHAT_PROXY_BASE_URL", server.url());
        std::env::set_var("GROK_XAI_API_BASE_URL", server.url());
        std::env::set_var("XAI_API_KEY", "test-key-for-ci");
        std::env::set_var("GROK_TELEMETRY_ENABLED", "false");
        std::env::set_var("GROK_FEEDBACK_ENABLED", "false");
        std::env::set_var("GROK_TRACE_UPLOAD", "false");
    }
    let agent_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("agent runtime");
    let local = tokio::task::LocalSet::new();
    agent_rt.block_on(local.run_until(async move {
        let client_conn = connect_and_auth().await;
        churn_one(&client_conn, workdir.path(), 0).await;
        let baseline = read_counts(&client_conn).await;
        assert_eq!(
            baseline.sessions, 0,
            "warmup session must be fully removed before baseline"
        );
        assert_eq!(
            baseline.workspace_bindings,
            Some(0),
            "warmup must have built the local workspace and released its binding"
        );
        assert_eq!(
            (
                baseline.subagent_pending,
                baseline.subagent_active,
                baseline.subagent_completed
            ),
            (0, 0, 0),
            "baseline must have no subagent entries"
        );
        for i in 1..=CHURN_SESSIONS {
            churn_one(&client_conn, workdir.path(), i).await;
        }
        let conn = &client_conn;
        let cwd = workdir.path();
        let concurrent: Vec<acp::SessionId> =
            futures::future::join_all((0..CONCURRENT_SESSIONS).map(|_| new_session(conn, cwd)))
                .await;
        let mid = read_counts(&client_conn).await;
        assert_eq!(
            mid.sessions, CONCURRENT_SESSIONS,
            "the snapshot must observe the open concurrent sessions"
        );
        futures::future::join_all(concurrent.iter().enumerate().map(|(i, sid)| async move {
            prompt_turn(conn, sid, &format!("concurrent ping {i}")).await;
        }))
        .await;
        futures::future::join_all(concurrent.iter().map(|sid| close_session(conn, sid))).await;
        let after = read_counts(&client_conn).await;
        assert_eq!(
            after, baseline,
            "session churn must return every registry count to baseline \
             (a growing count means a spawn-time map is missing its \
             remove_session release)"
        );
    }));
}
