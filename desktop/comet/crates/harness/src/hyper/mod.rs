//! Hyper harness: drive `hyper agent stdio` (ACP / agent-client-protocol).
//!
//! ACP is JSON-RPC over stdio; the `agent-client-protocol` crate's
//! `ClientSideConnection` is `!Send`, so it runs on a dedicated current-thread
//! runtime + `LocalSet` (a std thread), mirroring the proven pattern in
//! `xai-hyper-desktop/.../acp_backend.rs`. Events are bridged to a tokio mpsc
//! the engine consumes as the `BoxStream<AgentEvent>`.
//!
//! v1 (this file): single turn driven by `session/prompt`; mid-run steers are
//! forwarded as the existing `x.ai/interject` ACP extension (the grok agent
//! merges them into the running turn); interrupt via ACP `cancel`;
//! `request_permission` is bridged to `RunControls.request_input`. A multi-turn
//! refinement (subsequent steers as fresh `session/prompt` turns with a `Done`
//! per turn) is tracked as a TODO — see the run loop.

pub mod catalog;
pub mod ensure;
pub mod normalize;
pub mod rpc;

use std::sync::Arc;

use agent_client_protocol as acp;
use async_trait::async_trait;
use comet_proto::agent::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use acp::Agent as _;
use crate::{Harness, HarnessError, RunControls, SteerMessage};

pub use ensure::{ensure_hyper_bin, ensure_hyper_bin_blocking};
pub use rpc::{default_desktop_bin_dir, resolve_hyper_bin};

/// Trigger the agent's own login by spawning `<bin> login`.
///
/// `device_auth = false` → `--oauth` (browser flow: the agent opens auth.x.ai,
/// the user authenticates, the agent writes its auth file e.g. ~/.grok/auth.json).
/// `device_auth = true` → `--device-auth` (device code; headless/remote).
/// Returns `Ok` when the login subprocess exits 0. The agent owns the OAuth
/// dance and credential storage; comet only triggers + waits.
///
/// Ensures the Hyper CLI is present first (download to the desktop default
/// path when missing).
pub async fn agent_login(device_auth: bool) -> Result<(), HarnessError> {
    let exe = ensure_hyper_bin().await?;
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("login").arg(if device_auth { "--device-auth" } else { "--oauth" });
    // Inherit stdio so a terminal user sees the agent's login progress / code.
    let status = cmd
        .status()
        .await
        .map_err(|e| HarnessError::Protocol(format!("spawn {} login: {e}", exe.display())))?;
    if status.success() {
        Ok(())
    } else {
        Err(HarnessError::Protocol(format!("agent login exited {status}")))
    }
}

pub struct HyperHarness;

impl HyperHarness {
    pub fn new() -> Self {
        Self
    }
}

impl Default for HyperHarness {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Harness for HyperHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Hyper
    }
    fn display_name(&self) -> &str {
        "Hyper"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        catalog::REASONING_LEVELS
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        catalog::live_models().await
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = ensure_hyper_bin().await?;
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        let RunControls {
            request_input,
            mut steering,
            interrupt,
        } = controls;
        let request_input: Arc<dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync> =
            Arc::from(request_input);

        let event_tx_thread = event_tx.clone();
        std::thread::Builder::new()
            .name("hyper-harness".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = event_tx_thread
                            .try_send(Err(HarnessError::Io(e)));
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    // Spawn the agent subprocess here (the compat transport
                    // types are !Send, so they must be created inside the
                    // thread, not captured across it).
                    // Inherit parent env so GROK_HOME / auth / plugin paths match
                    // the Hyper TUI. Explicitly pass HYPER_AGENT_BIN when set so
                    // nested tools see the same binary.
                    let mut cmd = tokio::process::Command::new(&exe);
                    cmd.args(["agent", "stdio"])
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::inherit())
                        .current_dir(&request.cwd)
                        .kill_on_drop(true)
                        .env("HYPER_AGENT_BIN", &exe);
                    // Clear cloud comet edge vars so a host shell cannot re-enable
                    // WorkOS mode inside nested tooling by accident.
                    cmd.env_remove("COMET_EDGE_TOKEN")
                        .env_remove("COMET_WORKOS_CLIENT_ID")
                        .env_remove("COMET_EDGE_URL");
                    let mut child = match cmd.spawn()
                    {
                        Ok(c) => c,
                        Err(e) => {
                            let err = match e.kind() {
                                std::io::ErrorKind::NotFound => HarnessError::NotInstalled(exe.display().to_string()),
                                _ => HarnessError::Io(e),
                            };
                            let _ = event_tx.send(Ok(AgentEvent::Error { message: format!("{err:#}") })).await;
                            let _ = event_tx.send(Ok(AgentEvent::Done {
                                status: DoneStatus::Errored, result: None,
                                error: Some(format!("{err:#}")), session_id: None,
                            })).await;
                            return;
                        }
                    };
                    let outgoing = match child.stdin.take() {
                        Some(s) => s.compat_write(),
                        None => return,
                    };
                    let incoming = match child.stdout.take() {
                        Some(s) => s.compat(),
                        None => return,
                    };
                    let result = run_acp(
                        incoming, outgoing, request, request_input,
                        &mut steering, interrupt, event_tx.clone(),
                    ).await;
                    let _ = child.start_kill();
                    if let Err(e) = result {
                        let _ = event_tx.send(Ok(AgentEvent::Error { message: format!("{e:#}") })).await;
                        let _ = event_tx.send(Ok(AgentEvent::Done {
                            status: DoneStatus::Errored, result: None,
                            error: Some(format!("{e:#}")), session_id: None,
                        })).await;
                    }
                });
            })
            .map_err(|e| HarnessError::Io(std::io::Error::other(e)))?;

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|e| (e, rx))
        })
        .boxed())
    }
}

/// The ACP `Client` impl that turns `session_notification` → `AgentEvent`
/// (via `normalize`) and bridges `request_permission` → `RunControls.request_input`.
struct HyperClient {
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    request_input: Arc<dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync>,
}

#[async_trait(?Send)]
impl acp::Client for HyperClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let title = args
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| args.tool_call.tool_call_id.0.to_string());
        // Synthesize a single yes/no question from the permission options.
        let options: Vec<String> = args.options.iter().map(|o| o.name.clone()).collect();
        let questions = vec![UserInputQuestion {
            id: args.tool_call.tool_call_id.0.as_ref().to_string(),
            header: "Permission".into(),
            question: title,
            options,
            multi_select: false,
        }];
        let rx = (self.request_input)(questions);
        let answers = match rx.await {
            Ok(a) => a,
            Err(_) => {
                return Ok(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Cancelled,
                ));
            }
        };
        // Map the first answer's first label back to an ACP option id.
        let chosen = answers.first().and_then(|a| a.labels.first()).cloned();
        let outcome = match chosen {
            Some(label) => {
                // Find the option whose name matches the label; fall back to AllowOnce.
                let opt = args
                    .options
                    .iter()
                    .find(|o| o.name == label)
                    .or_else(|| {
                        args.options
                            .iter()
                            .find(|o| o.kind == acp::PermissionOptionKind::AllowOnce)
                    });
                match opt {
                    Some(o) => acp::RequestPermissionOutcome::Selected(
                        acp::SelectedPermissionOutcome::new(o.option_id.clone()),
                    ),
                    None => acp::RequestPermissionOutcome::Cancelled,
                }
            }
            None => acp::RequestPermissionOutcome::Cancelled,
        };
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let events = normalize::map_update(args.update, &args.session_id);
        for ev in events {
            if self.event_tx.send(Ok(ev)).await.is_err() {
                break; // consumer gone
            }
        }
        Ok(())
    }
}

/// Run one ACP session on the LocalSet over injected transports: handshake,
/// emit `SessionStarted`, drive turns via `session/prompt` (subsequent turns
/// from the steering mailbox), forward mid-run steers as `x.ai/interject`,
/// handle interrupt, and emit a terminal `Done`. `run()` spawns the subprocess
/// and passes the child's stdin/stdout here; tests inject in-memory pipes.
async fn run_acp<R, W>(
    incoming: R,
    outgoing: W,
    request: RunRequest,
    request_input: Arc<dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync>,
    steering: &mut mpsc::Receiver<SteerMessage>,
    interrupt: CancellationToken,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> Result<(), HarnessError>
where
    R: futures::AsyncRead + Unpin + 'static,
    W: futures::AsyncWrite + Unpin + 'static,
{
    let client = HyperClient {
        event_tx: event_tx.clone(),
        request_input,
    };
    let (conn, io_task) =
        acp::ClientSideConnection::new(client, outgoing, incoming, |fut: futures::future::LocalBoxFuture<'static, ()>| {
            tokio::task::spawn_local(fut);
        });
    tokio::task::spawn_local(io_task);

    // initialize
    let init = conn
        .initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                .client_info(
                    acp::Implementation::new("comet-hyper", env!("CARGO_PKG_VERSION"))
                        .title("Comet → Hyper"),
                )
                .client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                )
                .meta(
                    serde_json::json!({
                        "startupHints": {
                            "nonInteractive": true,
                            "skipGitStatus": true,
                            "skipProjectLayout": true
                        },
                        "clientType": "comet-hyper",
                        "clientVersion": env!("CARGO_PKG_VERSION")
                    })
                    .as_object()
                    .cloned(),
                ),
        )
        .await
        .map_err(|e| HarnessError::Protocol(format!("initialize: {e}")))?;

    // authenticate: prefer cached_token, then xai.api_key, then the first method.
    if !init.auth_methods.is_empty() {
        let preferred = ["cached_token", "xai.api_key"];
        let mut authed = false;
        for want in preferred {
            if let Some(method) = init.auth_methods.iter().find(|m| m.id().0.as_ref() == want) {
                if conn
                    .authenticate(
                        acp::AuthenticateRequest::new(method.id().clone())
                            .meta(serde_json::json!({"headless": true}).as_object().cloned()),
                    )
                    .await
                    .is_ok()
                {
                    authed = true;
                    break;
                }
            }
        }
        if !authed {
            if let Some(method) = init.auth_methods.first() {
                let _ = conn
                    .authenticate(
                        acp::AuthenticateRequest::new(method.id().clone())
                            .meta(serde_json::json!({"headless": true}).as_object().cloned()),
                    )
                    .await;
            }
        }
    }

    // new session
    let new_session = conn
        .new_session(acp::NewSessionRequest::new(request.cwd.clone()).mcp_servers(vec![]))
        .await
        .map_err(|e| HarnessError::Protocol(format!("session/new: {e}")))?;
    let session_id = new_session.session_id.clone();
    let model = request
        .model
        .clone()
        .or_else(|| {
            new_session
                .models
                .as_ref()
                .map(|m| m.current_model_id.0.as_ref().to_string())
        })
        .unwrap_or_default();
    let mut assistant_message_id = uuid::Uuid::new_v4().to_string();

    let _ = event_tx
        .send(Ok(AgentEvent::SessionStarted {
            harness: HarnessId::Hyper,
            model,
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.0.as_ref().to_string(),
            assistant_message_id: assistant_message_id.clone(),
        }))
        .await;

    // Drive turns. Turn 1 uses `request.prompt`; each subsequent turn uses
    // the next steer from the mailbox (comet's "steer = next turn" model).
    // During each turn's `session/prompt` await, any *further* steers are
    // forwarded as `x.ai/interject` (mid-run injection the grok agent merges
    // into the running turn). Each turn emits its own `Done` from `stop_reason`.
    let mut cancelled = run_turn(
        &conn,
        &session_id,
        request.prompt.clone(),
        &mut assistant_message_id,
        &mut *steering,
        &interrupt,
        &event_tx,
    )
    .await?;

    while !cancelled {
        // The next steer becomes the next turn.
        let steer = match steering.recv().await {
            Some(s) => s,
            None => break, // mailbox closed → run ends
        };
        let next_id = uuid::Uuid::new_v4().to_string();
        let _ = event_tx
            .send(Ok(AgentEvent::Steered {
                assistant_message_id: Some(assistant_message_id.clone()),
                next_assistant_message_id: Some(next_id.clone()),
            }))
            .await;
        assistant_message_id = next_id;
        cancelled = run_turn(
            &conn,
            &session_id,
            steer.prompt,
            &mut assistant_message_id,
            &mut *steering,
            &interrupt,
            &event_tx,
        )
        .await?;
    }

    if cancelled {
        let _ = event_tx
            .send(Ok(AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: Some(session_id.0.as_ref().to_string()),
            }))
            .await;
    }

    // Best-effort cleanup (the subprocess is owned by `run()`'s thread; the
    // injected-transport test path has no child to kill here).
    Ok(())
}

/// Run one turn: await `session/prompt` while forwarding mid-run steers as
/// `x.ai/interject`. Emits `Done` from the prompt `stop_reason`. Returns `true`
/// if interrupted (caller emits the `Interrupted` `Done`).
async fn run_turn(
    conn: &acp::ClientSideConnection,
    session_id: &acp::SessionId,
    prompt_text: String,
    assistant_message_id: &mut String,
    steering: &mut mpsc::Receiver<SteerMessage>,
    interrupt: &CancellationToken,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> Result<bool, HarnessError> {
    let prompt_fut = conn.prompt(acp::PromptRequest::new(
        session_id.clone(),
        vec![acp::ContentBlock::Text(acp::TextContent::new(prompt_text))],
    ));
    futures::pin_mut!(prompt_fut);
    loop {
        tokio::select! {
            biased;
            _ = interrupt.cancelled() => {
                let _ = conn.cancel(acp::CancelNotification::new(session_id.clone())).await;
                return Ok(true);
            }
            resp = &mut prompt_fut => match resp {
                Ok(stop) => {
                    for ev in normalize::done_from_stop(&stop, assistant_message_id, &session_id.0) {
                        let _ = event_tx.send(Ok(ev)).await;
                    }
                    return Ok(false);
                }
                Err(e) => {
                    let _ = event_tx.send(Ok(AgentEvent::Error { message: format!("session/prompt: {e}") })).await;
                    let _ = event_tx.send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(format!("session/prompt: {e}")),
                        session_id: Some(session_id.0.as_ref().to_string()),
                    })).await;
                    return Ok(false);
                }
            },
            steer = steering.recv() => match steer {
                Some(s) => {
                    let _ = conn
                        .ext_method(rpc::interject_request(session_id, &s.prompt, s.message_id.as_deref())?)
                        .await;
                    let _ = event_tx
                        .send(Ok(AgentEvent::Steered {
                            assistant_message_id: Some(assistant_message_id.clone()),
                            next_assistant_message_id: Some(assistant_message_id.clone()),
                        }))
                        .await;
                }
                None => {
                    // Mailbox closed mid-turn: let the running turn finish.
                }
            }
        }
    }
}

/// A no-op ACP `Client` for the `models()` meta query — no turn runs, so
/// `request_permission`/`session_notification` are never exercised.
struct MetaClient;

#[async_trait(?Send)]
impl acp::Client for MetaClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Cancelled,
        ))
    }
    async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
        Ok(())
    }
}

/// Initialize a `hyper agent stdio` connection over injected transports and
/// return the model catalog parsed from its `initialize` `_meta.modelState`.
/// `run()`'s thread / the e2e test inject the transports; `live_models()` uses
/// this with a real subprocess spawn.
pub(super) async fn init_meta_over<R, W>(
    incoming: R,
    outgoing: W,
) -> Result<Vec<Model>, HarnessError>
where
    R: futures::AsyncRead + Unpin + 'static,
    W: futures::AsyncWrite + Unpin + 'static,
{
    let (conn, io_task) =
        acp::ClientSideConnection::new(MetaClient, outgoing, incoming, |fut: futures::future::LocalBoxFuture<'static, ()>| {
            tokio::task::spawn_local(fut);
        });
    tokio::task::spawn_local(io_task);

    // modelState is on the initialize response _meta (available before auth).
    let init = conn
        .initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                .client_info(
                    acp::Implementation::new("comet-hyper", env!("CARGO_PKG_VERSION"))
                        .title("Comet → Hyper"),
                )
                .client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                )
                .meta(
                    serde_json::json!({
                        "startupHints": { "nonInteractive": true, "skipGitStatus": true, "skipProjectLayout": true }
                    })
                    .as_object()
                    .cloned(),
                ),
        )
        .await
        .map_err(|e| HarnessError::Protocol(format!("initialize: {e}")))?;

    let live = catalog::models_from_meta(&init.meta);
    if live.is_empty() {
        // No modelState in init meta — fall back to the static catalog.
        Ok(catalog::static_fallback())
    } else {
        Ok(live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_surface() {
        let h = HyperHarness::new();
        assert_eq!(h.id(), HarnessId::Hyper);
        assert_eq!(h.display_name(), "Hyper");
        assert!(h.supports_steering());
        assert_eq!(h.steering_mode(), SteeringMode::StepBoundary);
        assert_eq!(h.reasoning_levels(), catalog::REASONING_LEVELS);
    }

    #[test]
    fn static_fallback_is_non_empty() {
        // The static fallback is what `models()` returns when the live ACP
        // `initialize` meta carries no `modelState` (or the binary is absent).
        let models = catalog::static_fallback();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| !m.id.is_empty()));
    }

    // ── e2e mock roundtrip ───────────────────────────────────────────────
    //
    // An in-process ACP stub agent (AgentSideConnection) is wired to the
    // harness's run_acp (ClientSideConnection) over piper pipes — no real
    // `hyper` binary needed. Asserts the AgentEvent stream is
    // SessionStarted → TextDelta → AssistantMessageCompleted → Done{Completed}.

    use acp::Agent as _;
    use acp::Client as _;
    use std::sync::{Arc, Mutex};

    struct StubAgent {
        prompt_rx: Mutex<Option<oneshot::Receiver<()>>>,
        prompts: Mutex<Vec<String>>,
    }
    impl StubAgent {
        fn new(prompt_rx: oneshot::Receiver<()>) -> Self {
            Self {
                prompt_rx: Mutex::new(Some(prompt_rx)),
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Agent for StubAgent {
        async fn initialize(
            &self,
            args: acp::InitializeRequest,
        ) -> acp::Result<acp::InitializeResponse> {
            Ok(acp::InitializeResponse::new(args.protocol_version)
                .agent_capabilities(acp::AgentCapabilities::new())
                .agent_info(
                    acp::Implementation::new("stub-hyper", "0.0.0").title("Stub Hyper"),
                ))
        }
        async fn authenticate(
            &self,
            _args: acp::AuthenticateRequest,
        ) -> acp::Result<acp::AuthenticateResponse> {
            Ok(acp::AuthenticateResponse::default())
        }
        async fn new_session(
            &self,
            _args: acp::NewSessionRequest,
        ) -> acp::Result<acp::NewSessionResponse> {
            Ok(acp::NewSessionResponse::new(acp::SessionId::new("test-session")))
        }
        async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
            self.prompts.lock().unwrap().push(format!("{} block(s)", args.prompt.len()));
            // Block until the test releases us, so a notification can be
            // pushed mid-turn before EndTurn resolves.
            if let Some(rx) = self.prompt_rx.lock().unwrap().take() {
                let _ = rx.await;
            }
            Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
        }
        async fn cancel(&self, _args: acp::CancelNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    fn empty_request() -> RunRequest {
        RunRequest {
            prompt: "hello".into(),
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            cwd: ".".into(),
            sandbox: comet_proto::agent::SandboxLevel::DangerFullAccess,
            auto_approve: true,
            resume: None,
            attachments: Vec::new(),
        }
    }

    async fn collect_until_done(
        rx: &mut mpsc::Receiver<Result<AgentEvent, HarnessError>>,
    ) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        loop {
            match rx.recv().await {
                Some(Ok(e)) => {
                    let is_done = matches!(e, AgentEvent::Done { .. });
                    out.push(e);
                    if is_done {
                        break;
                    }
                }
                Some(Err(e)) => panic!("harness error in stream: {e}"),
                None => break, // channel closed (io_task dropped) — stop anyway
            }
        }
        out
    }

    #[tokio::test]
    async fn e2e_drives_a_turn_and_ends_with_done() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (cta_rx, cta_tx) = piper::pipe(1024); // client -> agent
                let (atc_rx, atc_tx) = piper::pipe(1024); // agent -> client

                let (release_tx, release_rx) = oneshot::channel::<()>();
                let stub = Arc::new(StubAgent::new(release_rx));
                let (agent_conn, agent_io) =
                    acp::AgentSideConnection::new(stub.clone(), atc_tx, cta_rx, |fut| {
                        tokio::task::spawn_local(fut);
                    });
                tokio::task::spawn_local(agent_io);

                let request_input: Arc<
                    dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync,
                > = Arc::new(|_| {
                    let (_tx, rx) = oneshot::channel();
                    rx // never answered — no permission reverse-request in this test
                });
                let (steer_tx, mut steer_rx) = mpsc::channel::<SteerMessage>(8);
                let interrupt = CancellationToken::new();
                let (event_tx, mut event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);

                let run_task = tokio::task::spawn_local(async move {
                    run_acp(
                        atc_rx,
                        cta_tx,
                        empty_request(),
                        request_input,
                        &mut steer_rx,
                        interrupt,
                        event_tx,
                    )
                    .await
                });

                // Let the handshake + session/prompt round-trip proceed until
                // the stub's prompt blocks on release_rx.
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(stub.prompts.lock().unwrap().len(), 1, "prompt should have arrived");

                // Push a TextDelta mid-turn, then release the turn.
                let _ = agent_conn
                    .session_notification(acp::SessionNotification::new(
                        acp::SessionId::new("test-session"),
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new("hi from agent".to_string())),
                        )),
                    ))
                    .await;
                drop(steer_tx); // mailbox closed → run ends after this turn
                let _ = release_tx.send(());

                let events = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    collect_until_done(&mut event_rx),
                )
                .await
                .expect("timed out waiting for Done");
                let _ = run_task.await;

                // Expected: SessionStarted, TextDelta("hi from agent"),
                // AssistantMessageCompleted, Done{Completed}.
                let mut it = events.iter();
                assert!(matches!(it.next(), Some(AgentEvent::SessionStarted { harness: HarnessId::Hyper, .. })));
                assert!(matches!(it.next(), Some(AgentEvent::TextDelta { text }) if text == "hi from agent"));
                assert!(matches!(it.next(), Some(AgentEvent::AssistantMessageCompleted { .. })));
                assert!(matches!(it.next(), Some(AgentEvent::Done { status: DoneStatus::Completed, .. })));
                assert!(it.next().is_none(), "no more events after Done");
            })
            .await;
    }

    // ── live models() parsing ─────────────────────────────────────────────
    // A stub whose `initialize` returns a `modelState` in `_meta`; assert
    // `init_meta_over` parses it into the comet Model catalog.

    struct ModelsStubAgent;
    #[async_trait::async_trait(?Send)]
    impl acp::Agent for ModelsStubAgent {
        async fn initialize(
            &self,
            args: acp::InitializeRequest,
        ) -> acp::Result<acp::InitializeResponse> {
            Ok(acp::InitializeResponse::new(args.protocol_version)
                .agent_capabilities(acp::AgentCapabilities::new())
                .agent_info(acp::Implementation::new("stub-hyper", "0.0.0").title("Stub"))
                .meta(
                    serde_json::json!({
                        "modelState": {
                            "currentModelId": "grok-4",
                            "availableModels": [
                                { "modelId": "grok-4", "name": "Grok 4" },
                                { "modelId": "custom-model", "name": "Custom" }
                            ]
                        }
                    })
                    .as_object()
                    .cloned(),
                ))
        }
        async fn authenticate(
            &self,
            _args: acp::AuthenticateRequest,
        ) -> acp::Result<acp::AuthenticateResponse> {
            Ok(acp::AuthenticateResponse::default())
        }
        async fn new_session(
            &self,
            _args: acp::NewSessionRequest,
        ) -> acp::Result<acp::NewSessionResponse> {
            Ok(acp::NewSessionResponse::new(acp::SessionId::new("x")))
        }
        async fn prompt(&self, _args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
            Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
        }
        async fn cancel(&self, _args: acp::CancelNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn e2e_models_from_init_meta() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (cta_rx, cta_tx) = piper::pipe(1024);
                let (atc_rx, atc_tx) = piper::pipe(1024);
                let stub = Arc::new(ModelsStubAgent);
                let (_agent_conn, agent_io) =
                    acp::AgentSideConnection::new(stub, atc_tx, cta_rx, |fut| {
                        tokio::task::spawn_local(fut);
                    });
                tokio::task::spawn_local(agent_io);

                let models = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    init_meta_over(atc_rx, cta_tx),
                )
                .await
                .expect("timed out waiting for models")
                .expect("init_meta_over errored");

                assert_eq!(models.len(), 2);
                assert!(models
                    .iter()
                    .any(|m| m.id == "grok-4" && m.label == "Grok 4"));
                assert!(models
                    .iter()
                    .any(|m| m.id == "custom-model" && m.label == "Custom"));
                assert!(models
                    .iter()
                    .all(|m| m.reasoning_levels == catalog::REASONING_LEVELS));
            })
            .await;
    }
}