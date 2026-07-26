//! Leader soak: an in-process leader server fronting a REAL `MvpAgent`, hammered
//! by churning `LeaderClient`s until a time budget expires. Asserts the leader
//! neither leaks memory nor accumulates zombie clients, and that no response is
//! ever dropped on a live-client send (`leader.response.send_failed`).
//!
//! Duration is bounded by `LEADER_SOAK_SECS` (default 10s so an ad-hoc
//! `--ignored` run stays quick). RSS growth is bounded by
//! `LEADER_SOAK_MAX_RSS_GROWTH_MB` (default 1024). On-demand today — no CI
//! lane runs it; a real soak is the long form:
//!
//! ```bash
//! LEADER_SOAK_SECS=1200 cargo test -p xai-grok-shell --test test_leader_soak -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{
    AcpAgentGatewayReceiver as GatewayReceiver, AcpAgentGatewaySender as GatewaySender,
    LineBufferedRead,
};
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::agent::mvp_agent::MvpAgent;
use xai_grok_shell::leader::{
    ClientCapabilities, ClientMode, LeaderClient, LeaderServerControlState, LeaderServerMetadata,
    run_leader_server,
};
use xai_grok_test_support::resources::ResourceSnapshot;

const SIMPLEX_BUF: usize = 8 * 1024 * 1024;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `leader.response.send_failed` entries written by THIS process.
fn send_failed_count() -> usize {
    let Some(bytes) = xai_grok_telemetry::unified_log::snapshot_log() else {
        return 0;
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line).is_ok_and(|entry| {
                entry["msg"] == "leader.response.send_failed" && entry["pid"] == std::process::id()
            })
        })
        .count()
}

/// Send one JSON-RPC request through a `LeaderClient` and await the response
/// with the matching id, skipping interleaved notifications.
async fn rpc(client: &mut LeaderClient, payload: String, id: u64, what: &str) -> serde_json::Value {
    client
        .send(payload)
        .unwrap_or_else(|e| panic!("{what}: send failed: {e}"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_else(|| panic!("{what}: timed out waiting for response id {id}"));
        let msg = tokio::time::timeout(remaining, client.recv())
            .await
            .unwrap_or_else(|_| panic!("{what}: timed out waiting for response id {id}"))
            .unwrap_or_else(|| panic!("{what}: connection closed awaiting response id {id}"));
        let json: serde_json::Value = match serde_json::from_str(&msg) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if json["id"] == id && (json.get("result").is_some() || json.get("error").is_some()) {
            assert!(
                json.get("error").is_none(),
                "{what}: error response: {json}"
            );
            return json;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "leader soak; run with --ignored (LEADER_SOAK_SECS bounds the duration)"]
async fn leader_soak_churning_clients_no_leaks_no_zombies() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let server = xai_grok_test_support::MockInferenceServer::start()
        .await
        .unwrap();
    let grok_home = TempDir::new().unwrap();
    let workdir = TempDir::new().unwrap();

    // SAFETY: single-threaded current-thread runtime; set before any agent
    // code reads these process-globals (same pattern as session_load_perf).
    unsafe {
        std::env::set_var("GROK_HOME", grok_home.path());
        std::env::set_var("GROK_CLI_CHAT_PROXY_BASE_URL", server.url());
        std::env::set_var("GROK_XAI_API_BASE_URL", server.url());
        std::env::set_var("XAI_API_KEY", "test-key-for-ci");
        std::env::set_var("GROK_TELEMETRY_ENABLED", "false");
        std::env::set_var("GROK_FEEDBACK_ENABLED", "false");
        std::env::set_var("GROK_TRACE_UPLOAD", "false");
    }

    let sock_path = grok_home.path().join("leader-soak.sock");
    let soak_secs = env_u64("LEADER_SOAK_SECS", 10);
    let max_growth_mb = env_u64("LEADER_SOAK_MAX_RSS_GROWTH_MB", 1024);
    let send_failed_before = send_failed_count();

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // ── Leader server (survives client churn) ────────────────────
            let (acp_tx, mut acp_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let cancel = CancellationToken::new();
            let client_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let control_state = LeaderServerControlState::new(LeaderServerMetadata {
                pid: std::process::id(),
                socket_path: sock_path.clone(),
                lock_path: sock_path.with_extension("lock"),
                ws_url_suffix: String::new(),
                leader_binary_version: env!("CARGO_PKG_VERSION").to_string(),
            });
            let cancel_for_server = cancel.clone();
            let sock_for_server = sock_path.clone();
            let client_count_for_server = client_count.clone();
            tokio::task::spawn_local(async move {
                let _ = run_leader_server(
                    sock_for_server,
                    acp_tx,
                    response_rx,
                    cancel_for_server,
                    true,
                    client_count_for_server,
                    Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    xai_grok_shell::agent::activity::AgentActivity::default(),
                    tokio::sync::watch::channel(true).1,
                    tokio::sync::watch::channel(false).0,
                    tokio::sync::watch::channel(xai_grok_shell::leader::ShutdownReason::Manual).0,
                    None,
                    control_state,
                )
                .await;
            });

            // ── Real agent behind it ──────────────────────────────────────
            // Copied from `run_leader`'s agent-spawn + IPC/stdout bridge
            // blocks in src/agent/app.rs (inside its LocalSet body); kept as
            // a deliberate copy so production stays untouched. Second copy of
            // the same wiring: xai-grok-pager/src/app/leader_cluster/mod.rs
            // (`spawn_leader_generation`) — keep the two copies behaviorally
            // identical.
            let (agent_in_read, agent_in_write) = tokio::io::simplex(SIMPLEX_BUF);
            let (agent_out_read, agent_out_write) = tokio::io::simplex(SIMPLEX_BUF);

            tokio::task::spawn_local(async move {
                let agent_config = AgentConfig::default();
                let auth_manager = Arc::new(agent_config.create_auth_manager());
                let (gw_tx, gw_rx) = tokio::sync::mpsc::unbounded_channel();
                let gateway = GatewaySender::new(gw_tx);
                let agent = MvpAgent::new(gateway, &agent_config, auth_manager, None)
                    .expect("valid agent config");
                let incoming = LineBufferedRead::spawn_local(agent_in_read.compat());
                let (conn, handle_io) = acp::AgentSideConnection::new(
                    agent,
                    agent_out_write.compat_write(),
                    incoming,
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );
                tokio::task::spawn_local(
                    GatewayReceiver::new(gw_rx, conn)
                        .with_on_meta(xai_file_utils::trace_context::span_from_meta_traceparent)
                        .run(),
                );
                let _ = handle_io.await;
            });

            // Leader → agent stdin.
            tokio::task::spawn_local(async move {
                let mut agent_in_write = agent_in_write;
                while let Some(msg) = acp_rx.recv().await {
                    if agent_in_write.write_all(msg.as_bytes()).await.is_err()
                        || agent_in_write.write_all(b"\n").await.is_err()
                    {
                        break;
                    }
                }
            });
            // Agent stdout → leader responses.
            let response_tx_for_agent = response_tx.clone();
            tokio::task::spawn_local(async move {
                let mut reader = BufReader::new(agent_out_read);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let msg = line.trim_end_matches(['\r', '\n']).to_string();
                            if !msg.is_empty() {
                                let _ = response_tx_for_agent.send(msg);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });

            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !sock_path.exists() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(sock_path.exists(), "leader socket never bound");

            // ── One-time initialize + authenticate through the leader ────
            let mut bootstrap = LeaderClient::connect(
                sock_path.clone(),
                "soak-bootstrap",
                ClientMode::Stdio,
                ClientCapabilities::default(),
            )
            .await
            .expect("bootstrap connect");
            rpc(
                &mut bootstrap,
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"_meta":{"startupHints":{"nonInteractive":true,"skipGitStatus":true,"skipProjectLayout":true},"clientType":"soak","clientVersion":"0.0.0-test"}}}"#.to_string(),
                1,
                "initialize",
            )
            .await;
            rpc(
                &mut bootstrap,
                r#"{"jsonrpc":"2.0","id":2,"method":"authenticate","params":{"methodId":"xai.api_key","_meta":{"headless":true}}}"#.to_string(),
                2,
                "authenticate",
            )
            .await;

            let rss_before = ResourceSnapshot::capture();
            let soak_deadline = tokio::time::Instant::now() + Duration::from_secs(soak_secs);
            let workdir_str = workdir.path().to_string_lossy().to_string();
            let mut cycles: u64 = 0;
            let mut turns: u64 = 0;

            // ── Churn: 10 fresh clients per cycle, 2 sessions each, one
            // scripted turn per session, then all disconnect ───────────────
            while tokio::time::Instant::now() < soak_deadline {
                cycles += 1;
                let mut clients = Vec::new();
                for i in 0..10u64 {
                    let client = LeaderClient::connect(
                        sock_path.clone(),
                        "soak-client",
                        ClientMode::Stdio,
                        ClientCapabilities::default(),
                    )
                    .await
                    .unwrap_or_else(|e| panic!("cycle {cycles} client {i} connect: {e}"));
                    clients.push(client);
                }

                for (i, client) in clients.iter_mut().enumerate() {
                    for s in 0..2u64 {
                        let new_id = 100 + s;
                        let resp = rpc(
                            client,
                            format!(
                                r#"{{"jsonrpc":"2.0","id":{new_id},"method":"session/new","params":{{"cwd":"{workdir_str}","mcpServers":[]}}}}"#
                            ),
                            new_id,
                            "session/new",
                        )
                        .await;
                        let sid = resp["result"]["sessionId"]
                            .as_str()
                            .unwrap_or_else(|| panic!("no sessionId in {resp}"))
                            .to_string();

                        let prompt_id = 200 + s;
                        rpc(
                            client,
                            format!(
                                r#"{{"jsonrpc":"2.0","id":{prompt_id},"method":"session/prompt","params":{{"sessionId":"{sid}","prompt":[{{"type":"text","text":"soak c{i} s{s} cycle {cycles}"}}]}}}}"#
                            ),
                            prompt_id,
                            "session/prompt",
                        )
                        .await;
                        turns += 1;
                    }
                }

                // Churn: everyone disconnects; the roster must drain fully.
                for client in clients {
                    client.cancel();
                }
                let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                while client_count.load(std::sync::atomic::Ordering::Relaxed) > 1 {
                    assert!(
                        tokio::time::Instant::now() < drain_deadline,
                        "cycle {cycles}: roster kept {} zombie clients after churn",
                        client_count.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }

            eprintln!("[soak] {cycles} cycles, {turns} turns in {soak_secs}s budget");
            assert!(cycles > 0, "soak budget too small to complete one cycle");

            // ── Convergence: only the bootstrap client remains, and the
            // leader still serves a healthy round-trip ────────────────────
            assert_eq!(
                client_count.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "roster must converge to the bootstrap client after churn"
            );
            let resp = rpc(
                &mut bootstrap,
                format!(
                    r#"{{"jsonrpc":"2.0","id":900,"method":"session/new","params":{{"cwd":"{workdir_str}","mcpServers":[]}}}}"#
                ),
                900,
                "post-soak session/new",
            )
            .await;
            assert!(resp["result"]["sessionId"].is_string());

            // ── No response was ever dropped on a live-client send ────────
            assert_eq!(
                send_failed_count(),
                send_failed_before,
                "leader.response.send_failed must not occur during the soak"
            );

            // ── RSS bound ─────────────────────────────────────────────────
            let rss_after = ResourceSnapshot::capture();
            let growth = rss_after.growth_from(&rss_before);
            if let (Some(before), Some(after), Some(growth_bytes)) =
                (rss_before.rss, rss_after.rss, growth.rss)
            {
                let growth_mb = growth_bytes as f64 / (1024.0 * 1024.0);
                eprintln!(
                    "[soak] rss: {:.1} MB -> {:.1} MB (growth {growth_mb:.1} MB)",
                    before as f64 / (1024.0 * 1024.0),
                    after as f64 / (1024.0 * 1024.0),
                );
                assert!(
                    growth_mb < max_growth_mb as f64,
                    "leader RSS grew {growth_mb:.1} MB over the soak (bound {max_growth_mb} MB)"
                );
            } else {
                eprintln!("[soak] rss measurement unavailable on this platform; bound skipped");
            }

            bootstrap.cancel();
            cancel.cancel();
        })
        .await;
}
