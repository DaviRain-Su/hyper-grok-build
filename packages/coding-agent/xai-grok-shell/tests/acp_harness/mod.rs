//! Drives a real in-process `MvpAgent` over ACP on duplex pipes.
//! This lives outside `tests/common/` because that compiles into every integration binary and would pull the transport stack into all of them.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use agent_client_protocol::{self as acp, Agent as _};
use serde_json::json;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use xai_acp_lib::{
    AcpAgentGatewayReceiver as GatewayReceiver, AcpAgentGatewaySender as GatewaySender,
    LineBufferedRead,
};
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::agent::mvp_agent::MvpAgent;

/// Matches production's `MAX_BUFFER_SIZE` in `agent::app`.
pub const DUPLEX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub const RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// This is compiled into each including binary, so a client only one uses is dead code in the others.
#[allow(dead_code)]
pub struct AutoApproveClient;

#[async_trait::async_trait(?Send)]
impl acp::Client for AutoApproveClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        Ok(acp::RequestPermissionResponse::new(allow_once(&args)))
    }

    async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
        Ok(())
    }
}

pub fn allow_once(args: &acp::RequestPermissionRequest) -> acp::RequestPermissionOutcome {
    args.options
        .iter()
        .find(|o| o.kind == acp::PermissionOptionKind::AllowOnce)
        .or(args.options.first())
        .map(|o| {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                o.option_id.clone(),
            ))
        })
        .unwrap_or(acp::RequestPermissionOutcome::Cancelled)
}

/// Client-half ends of the duplex pair linking a client to a stood-up agent.
pub struct AgentPipes {
    pub to_agent: tokio::io::DuplexStream,
    pub from_agent: tokio::io::DuplexStream,
}

/// Stand up `MvpAgent` plus its ACP connection and IO tasks on the current `LocalSet`.
/// Callers wanting another topology build the same pieces elsewhere and hand [`connect_client`] the pipes.
pub fn spawn_agent_local() -> AgentPipes {
    let (c2a_a, c2a_b) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let (a2c_a, a2c_b) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);

    let agent_config = AgentConfig::default();
    let auth_manager = Arc::new(agent_config.create_auth_manager());
    let (gw_tx, gw_rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = MvpAgent::new(GatewaySender::new(gw_tx), &agent_config, auth_manager, None)
        .expect("valid config");

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

    AgentPipes {
        to_agent: c2a_a,
        from_agent: a2c_b,
    }
}

/// IO tasks spawn on the current `LocalSet`.
pub async fn connect_and_auth<C>(
    client: C,
    client_type: &str,
) -> (acp::ClientSideConnection, acp::InitializeResponse)
where
    C: acp::Client + 'static,
{
    let pipes = spawn_agent_local();
    connect_client(client, client_type, pipes).await
}

/// Initialize plus API-key auth over `pipes`; the one handshake every harness topology shares.
pub async fn connect_client<C>(
    client: C,
    client_type: &str,
    pipes: AgentPipes,
) -> (acp::ClientSideConnection, acp::InitializeResponse)
where
    C: acp::Client + 'static,
{
    let AgentPipes {
        to_agent,
        from_agent,
    } = pipes;
    let client_incoming = LineBufferedRead::spawn_local(from_agent.compat());
    let (client_conn, client_io) =
        acp::ClientSideConnection::new(client, to_agent.compat_write(), client_incoming, |fut| {
            tokio::task::spawn_local(fut);
        });
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
                        "clientType": client_type,
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

    // API-key auth so sessions resolve the mock's `test-model`.
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

    (client_conn, init)
}

// Dead-code allows below: same per-binary compilation as `AutoApproveClient` above; each helper is used by some including test binaries, not all
#[allow(dead_code)]
pub async fn ext_method(
    conn: &acp::ClientSideConnection,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let params_json =
        serde_json::value::RawValue::from_string(params.to_string()).expect("serialize ext params");
    let resp = tokio::time::timeout(
        RPC_TIMEOUT,
        conn.ext_method(acp::ExtRequest::new(method, Arc::from(params_json))),
    )
    .await
    .unwrap_or_else(|_| panic!("{method} timed out"))
    .unwrap_or_else(|e| panic!("{method} failed: {e}"));
    serde_json::from_str(resp.0.get()).unwrap_or_else(|e| panic!("{method}: bad response: {e}"))
}

#[allow(dead_code)]
pub async fn new_session(
    conn: &acp::ClientSideConnection,
    cwd: &std::path::Path,
) -> acp::SessionId {
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

#[allow(dead_code)]
pub async fn prompt_turn(
    conn: &acp::ClientSideConnection,
    session_id: &acp::SessionId,
    text: &str,
) {
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

fn set_test_env(grok_home: &std::path::Path, server_url: &str) {
    // SAFETY: the only live threads are the mock's HTTP workers, which never read env.
    unsafe {
        std::env::set_var("GROK_HOME", grok_home);
        std::env::set_var("GROK_CLI_CHAT_PROXY_BASE_URL", server_url);
        std::env::set_var("GROK_XAI_API_BASE_URL", server_url);
        std::env::set_var("XAI_API_KEY", "test-key-for-ci");
        std::env::set_var("GROK_TELEMETRY_ENABLED", "false");
        std::env::set_var("GROK_FEEDBACK_ENABLED", "false");
        std::env::set_var("GROK_TRACE_UPLOAD", "false");
        // Turn summaries fire one more request to the same mock endpoint after the turn, on a spawned task
        // The race makes request-count assertions flaky
        std::env::set_var("GROK_TURN_SUMMARY", "false");
    }
}

/// Runs `body` against a mock inference server with `GROK_HOME` isolated to a
/// temp dir. `body` gets the cwd and the mock, and opens its own connection,
/// since each test wants a different `acp::Client`.
pub fn run_agent_test<F, Fut>(body: F)
where
    F: FnOnce(std::path::PathBuf, std::rc::Rc<xai_grok_test_support::MockInferenceServer>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    run_agent_test_with_models(
        vec![xai_grok_test_support::MockModelEntry::new("test-model")],
        body,
    )
}

/// [`run_agent_test`] with a custom `/v1/models` catalog.
#[allow(dead_code)]
pub fn run_agent_test_with_models<F, Fut>(
    models: Vec<xai_grok_test_support::MockModelEntry>,
    body: F,
) where
    F: FnOnce(std::path::PathBuf, std::rc::Rc<xai_grok_test_support::MockInferenceServer>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _env_guard = hold_global_env();
    xai_grok_extra_ca::ensure_default_crypto_provider();

    // Own thread: agent startup blocks on a models prefetch and would starve the mock.
    let mock_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("mock runtime");
    let server = std::rc::Rc::new(
        mock_rt
            .block_on(xai_grok_test_support::MockInferenceServer::start_with_models(models))
            .expect("mock server"),
    );
    let grok_home = tempfile::TempDir::new().expect("grok home");
    let workdir = tempfile::TempDir::new().expect("workdir");
    set_test_env(grok_home.path(), &server.url());

    let agent_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("agent runtime");
    let local = tokio::task::LocalSet::new();
    agent_rt.block_on(local.run_until(body(
        workdir.path().to_path_buf(),
        std::rc::Rc::clone(&server),
    )));
}

fn hold_global_env() -> MutexGuard<'static, ()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
