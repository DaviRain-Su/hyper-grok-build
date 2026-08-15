use super::*;
use std::path::PathBuf;

/// A single undecodable line on an MCP stdio server's stdout must NOT
/// collapse the transport: if the decode error surfaced as `None`, the
/// service would read it as EOF → "Transport closed" → `tools/list` fails
/// and the connector "shows but doesn't work". The resilient transport
/// skips the bad line and keeps reading, so a stray stdout log line never
/// takes the whole server down.
#[tokio::test]
async fn resilient_transport_skips_undecodable_line_and_keeps_stream_alive() {
    // `server_out` is the writer half (the fake server's stdout); the
    // transport reads framed JSON-RPC from `client_in`.
    let (mut server_out, client_in) = tokio::io::duplex(64 * 1024);
    let mut transport = ResilientRwTransport::new(
        client_in,
        tokio::io::sink(),
        "fwbuild".to_string(),
        xai_grok_session_events::EventWriter::noop(),
    );

    let valid = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
    // A stray non-JSON log line — the shape that, under rmcp's stock
    // transport, decodes to an error and closes the connection.
    let garbage = "info: fwbuild started, listening on stdio";
    server_out
        .write_all(format!("{valid}\n{garbage}\n{valid}\n").as_bytes())
        .await
        .unwrap();
    // Dropping the writer half signals a clean end-of-stream.
    drop(server_out);

    assert!(
        transport.receive().await.is_some(),
        "first valid message must be received"
    );
    assert!(
        transport.receive().await.is_some(),
        "the undecodable line must be skipped and the next valid message delivered"
    );
    assert!(
        transport.receive().await.is_none(),
        "only a genuine end-of-stream yields None"
    );
}

fn make_stdio_server(name: &str, command: &str) -> acp::McpServer {
    acp::McpServer::Stdio(acp::McpServerStdio::new(name, PathBuf::from(command)))
}

fn make_http_server(name: &str, url: &str) -> acp::McpServer {
    acp::McpServer::Http(acp::McpServerHttp::new(name, url))
}

#[test]
fn plan_stdio_spawn_windows_resolves_bare_launcher_to_cmd_shim() {
    let args = vec!["-y".to_string(), "@scope/pkg".to_string()];
    let (program, spawn_args) = plan_stdio_spawn("npx", &args, true, |c| {
        assert_eq!(c, "npx");
        Some(PathBuf::from(r"C:\path\npx.cmd"))
    });
    assert_eq!(program, OsString::from(r"C:\path\npx.cmd"));
    assert_eq!(spawn_args, args);
}

#[test]
fn plan_stdio_spawn_windows_unresolved_falls_back_to_raw_command() {
    let args = vec!["-y".to_string(), "@scope/pkg".to_string()];
    let (program, spawn_args) = plan_stdio_spawn("npx", &args, true, |_| None);
    assert_eq!(program, OsString::from("npx"));
    assert_eq!(spawn_args, args);
}

#[test]
fn plan_stdio_spawn_windows_backslash_path_command_used_as_is_without_resolving() {
    let args = vec!["--config".to_string(), "x.json".to_string()];
    let (program, spawn_args) = plan_stdio_spawn(r"C:\tools\server.exe", &args, true, |_| {
        panic!("resolver must not be consulted for a command with a backslash separator")
    });
    assert_eq!(program, OsString::from(r"C:\tools\server.exe"));
    assert_eq!(spawn_args, args);
}

#[test]
fn plan_stdio_spawn_windows_forward_slash_path_command_used_as_is_without_resolving() {
    let args = vec!["--port".to_string(), "8080".to_string()];
    let (program, spawn_args) = plan_stdio_spawn("C:/tools/server.exe", &args, true, |_| {
        panic!("resolver must not be consulted for a command with a forward-slash separator")
    });
    assert_eq!(program, OsString::from("C:/tools/server.exe"));
    assert_eq!(spawn_args, args);
}

#[test]
fn plan_stdio_spawn_non_windows_never_resolves() {
    let args = vec!["-y".to_string(), "pkg".to_string()];
    let (program, spawn_args) = plan_stdio_spawn("npx", &args, false, |_| {
        panic!("resolver must not be consulted on non-Windows")
    });
    assert_eq!(program, OsString::from("npx"));
    assert_eq!(spawn_args, args);
}

#[test]
fn stdio_path_override_matches_path_case_insensitively() {
    let mk = |name: &str, value: &str| acp::EnvVariable::new(name, value);

    let env = vec![mk("FOO", "bar"), mk("Path", r"C:\node")];
    assert_eq!(stdio_path_override(&env), Some(r"C:\node"));

    let env_upper = vec![mk("PATH", "/custom/bin")];
    assert_eq!(stdio_path_override(&env_upper), Some("/custom/bin"));

    let env_none = vec![mk("FOO", "bar")];
    assert_eq!(stdio_path_override(&env_none), None);
}

#[test]
fn is_figma_mcp_matches_name_and_host() {
    assert!(is_figma_mcp("figma", "https://example.com/mcp"));
    assert!(is_figma_mcp("Figma", "https://example.com/mcp"));
    assert!(is_figma_mcp("grok_com_figma", "https://example.com/mcp"));
    assert!(is_figma_mcp("GROK_COM_FIGMA", "https://example.com/mcp"));
    assert!(is_figma_mcp("grok_com_FIGMA", "https://example.com/mcp"));
    assert!(is_figma_mcp("other", "https://mcp.figma.com/mcp"));
    assert!(is_figma_mcp("other", "https://figma.com/mcp"));
    assert!(!is_figma_mcp("linear", "https://mcp.linear.app/mcp"));
    assert!(!is_figma_mcp("figma_extra", "https://example.com/mcp"));
    assert!(!is_figma_mcp("grok_com_linear", "https://example.com/mcp"));
    assert!(!is_figma_mcp("linear", "not-a-url"));
    assert!(!is_figma_mcp("linear", "https://notfigma.com/mcp"));
    assert!(!is_figma_mcp("linear", "https://figma.com.evil/mcp"));
}

#[test]
fn ensure_figma_user_agent_sets_grok_cli_when_missing() {
    let mut headers = reqwest::header::HeaderMap::new();
    ensure_figma_user_agent(&mut headers, "figma", "https://mcp.figma.com/mcp");
    assert_eq!(
        headers.get(reqwest::header::USER_AGENT).unwrap(),
        "grok-cli"
    );

    let mut host_only = reqwest::header::HeaderMap::new();
    ensure_figma_user_agent(&mut host_only, "other", "https://mcp.figma.com/mcp");
    assert_eq!(
        host_only.get(reqwest::header::USER_AGENT).unwrap(),
        "grok-cli"
    );
}

#[test]
fn ensure_figma_user_agent_does_not_overwrite_existing() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("custom-ua"),
    );
    ensure_figma_user_agent(&mut headers, "figma", "https://mcp.figma.com/mcp");
    assert_eq!(
        headers.get(reqwest::header::USER_AGENT).unwrap(),
        "custom-ua"
    );
}

#[test]
fn ensure_figma_user_agent_skips_non_figma() {
    let mut headers = reqwest::header::HeaderMap::new();
    ensure_figma_user_agent(&mut headers, "linear", "https://mcp.linear.app/mcp");
    assert!(!headers.contains_key(reqwest::header::USER_AGENT));

    let mut invalid_url = reqwest::header::HeaderMap::new();
    ensure_figma_user_agent(&mut invalid_url, "linear", "not-a-url");
    assert!(!invalid_url.contains_key(reqwest::header::USER_AGENT));
}

#[cfg(unix)]
#[test]
fn safe_stdio_child_drop_without_entered_runtime_reaps_child() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let (transport, pid) = rt.block_on(async {
        let mut cmd = Command::new("sleep");
        cmd.arg("30").kill_on_drop(true);
        xai_grok_tools::util::detach_command(&mut cmd);
        let (transport, _stderr) = SafeTokioChildProcess::spawn(
            cmd,
            None,
            "test".to_string(),
            xai_grok_session_events::EventWriter::noop(),
        )
        .expect("spawn test child");
        let pid = transport.id().expect("spawned child pid");
        (transport, pid)
    });

    drop(rt);
    drop(transport);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !unix_process_exists(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    panic!("MCP child process {pid} was not reaped after no-runtime drop");
}

#[cfg(unix)]
fn unix_process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// `scope.kill_all()` reaps an enrolled MCP child even when its owner never
/// runs Drop. Non-vacuous: dropping the `Some(&scope)` enrollment makes this
/// time out.
#[cfg(unix)]
#[tokio::test]
async fn scope_kill_all_reaps_enrolled_mcp_child_while_owner_wedged() {
    use std::time::Duration;

    let scope = ProcessScope::new();

    let mut cmd = Command::new("sleep");
    cmd.arg("600").kill_on_drop(true);
    xai_grok_tools::util::detach_command(&mut cmd);
    let (mut child_process, _stderr) = SafeTokioChildProcess::spawn(
        cmd,
        Some(&scope),
        "wedge-test".to_string(),
        xai_grok_session_events::EventWriter::noop(),
    )
    .expect("spawn enrolled MCP child");
    assert_eq!(
        scope.live_count(),
        1,
        "the enrolled MCP child group must be tracked by the scope"
    );

    // Wedge: owner never runs Drop, so kill_all is the only reclaim path.
    scope.kill_all();

    // Take only the handle, not the group, so kill-on-drop can't mask a
    // missing enrollment.
    let mut child = child_process.child.take().expect("child handle present");
    // Null the strong Arc<ProcessGroup> before reaping the leader below:
    // holding it across the reap would let `child_process`'s later Drop
    // killpg a reusable pgid — the PID-reuse pattern the Weak ownership
    // contract exists to prevent.
    child_process.process_group = None;
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("scope.kill_all must have SIGKILL'd the enrolled MCP child group")
        .expect("wait on the reclaimed child succeeds");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "the MCP child must have been SIGKILL'd by the scope, not have exited cleanly"
    );
}

#[test]
fn test_mcp_state_new() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let state = McpState::new(configs.clone());

    assert_eq!(state.configs.len(), 1);
    assert!(state.owned_clients.is_empty());
    assert!(!state.is_initialized());
    assert!(!state.is_initializing());
    assert!(!state.has_finished_init());
    assert!(matches!(state.init_progress(), InitProgress::NotStarted));
    assert_eq!(state.generation, 0);
}

#[test]
fn test_mcp_state_update_configs_returns_false_when_unchanged() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let mut state = McpState::new(configs.clone());

    // Same configs should return false
    let changed = state.update_configs(configs.clone());
    assert!(!changed);
    assert_eq!(state.generation, 0); // Generation should not change
}

#[test]
fn test_mcp_state_update_configs_returns_true_when_changed() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let mut state = McpState::new(configs);

    // Different configs should return true
    let new_configs = vec![make_stdio_server("test2", "/bin/test2")];
    let changed = state.update_configs(new_configs);
    assert!(changed);
    assert_eq!(state.generation, 1); // Generation should increment
}

#[test]
fn test_mcp_state_update_configs_resets_initialized() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let mut state = McpState::new(configs);
    // Drive the state machine into Finished{handshaking:{"a"}} so
    // the reset path has both the lifecycle flag AND a per-server
    // entry to clear.
    assert!(state.try_start_init());
    state.mark_servers_initializing(["a".to_string()]);
    state.finish_init();
    assert!(state.has_finished_init());
    assert!(state.is_server_handshaking("a"));

    let new_configs = vec![make_stdio_server("test2", "/bin/test2")];
    let changed = state.update_configs(new_configs);
    assert!(changed);
    // update_configs must drop us back to NotStarted — neither
    // lifecycle flag set nor any per-server progress carried over.
    assert!(!state.is_initialized());
    assert!(!state.is_initializing());
    assert!(!state.has_finished_init());
    assert!(matches!(state.init_progress(), InitProgress::NotStarted));
}

#[tokio::test]
async fn acp_servers_survive_update_configs_clear() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let mut state = McpState::new(vec![make_http_server("http-srv", "http://localhost")]);
    state.set_acp_servers(
        vec![AcpServerEntry {
            name: "sdk-tools".to_string(),
            server_id: "srv_0".to_string(),
        }],
        Arc::new(NoopInvoker),
    );
    assert!(state.has_acp_servers());
    assert_eq!(state.build_pending_acp_clients(&HashMap::new()).len(), 1);

    // A config change clears owned clients/configs (proven by the generation bump)
    // but must NOT drop the separately-held acp servers — otherwise the in-process
    // SDK tools would silently vanish on every `update_configs`.
    let changed = state.update_configs(vec![make_http_server("other", "http://other")]);
    assert!(changed);
    assert_eq!(state.generation, 1);
    assert!(
        state.has_acp_servers(),
        "acp servers must survive update_configs"
    );
    let pending = state.build_pending_acp_clients(&HashMap::new());
    assert_eq!(pending.len(), 1, "acp clients rebuild after the clear");
    assert_eq!(pending[0].server_name(), "sdk-tools");
}

#[tokio::test]
async fn acp_overrides_apply_to_built_clients() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let mut overrides = HashMap::new();
    overrides.insert(
        "sdk-tools".to_string(),
        McpClientTimeoutOverrides {
            tool_timeout_sec: Some(123),
            ..Default::default()
        },
    );

    let mut state = McpState::new(vec![]);
    state.set_acp_servers(
        vec![AcpServerEntry {
            name: "sdk-tools".to_string(),
            server_id: "srv_0".to_string(),
        }],
        Arc::new(NoopInvoker),
    );

    let pending = state.build_pending_acp_clients(&overrides);
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].tool_timeout_sec(),
        123,
        "config.toml tool_timeout_sec override must reach the SDK client"
    );
}

/// In-process SDK (ACP) clients must never get a liveness watcher: the
/// dispatcher can't recover them (no `configs` entry), so a proactive
/// `TransportClosed` would evict the client with no recovery. Guards both
/// the `is_acp` predicate (across transports) and the `arm_liveness_watcher`
/// self-gate that depends on it. HTTP/stdio must report `false` so they
/// keep their watchers.
#[tokio::test]
async fn acp_clients_are_not_liveness_watched() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let acp = McpClient::new_acp(
        "sdk".to_string(),
        "srv_0".to_string(),
        Arc::new(NoopInvoker),
        None,
        None,
    );
    assert!(acp.is_acp());
    assert!(!acp.is_http());

    let http = McpClient::new_http(
        "http".to_string(),
        HttpConfig {
            url: "http://localhost/api/mcp".to_string(),
            headers: vec![],
        },
        None,
        None,
    );
    assert!(!http.is_acp());

    // Stub stands in for a no-transport / Stdio client (reconnect = None).
    assert!(!McpClient::stub("stdio").is_acp());

    // The gate that prevents the evict-on-close bug: arming is a no-op for ACP.
    assert!(
        !Arc::new(acp)
            .arm_liveness_watcher(Duration::from_millis(500))
            .await
    );
}

#[test]
fn test_mark_servers_initializing_clears_prior_init_failure() {
    // A server that failed a previous init is recorded in `init_failed`
    // (so the status snapshot reports it Unavailable). Starting a fresh
    // init attempt for that server must clear the failure flag so a
    // successful retry can surface as Ready again.
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
    state.init_failed.insert("a".to_string(), String::new());
    state.init_failed.insert("b".to_string(), String::new());

    state.mark_servers_initializing(["a".to_string()]);

    assert!(
        !state.init_failed.contains_key("a"),
        "fresh init attempt must clear the prior failure for that server",
    );
    assert!(
        state.init_failed.contains_key("b"),
        "servers not in this init attempt must keep their failure flag",
    );
}

#[test]
fn test_record_init_failure_keeps_auth_and_init_failed_disjoint() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);

    // Auth failures are owned by `auth_required` only — never `init_failed` —
    // so a later successful authentication (which clears `auth_required` and
    // registers tools) is not left stuck as Unavailable with zero tools.
    state.record_init_failure("auth-srv", true, None);
    assert!(state.auth_required.contains("auth-srv"));
    assert!(
        !state.init_failed.contains_key("auth-srv"),
        "auth-required failures must not also be flagged init_failed",
    );

    // Non-auth failures (handshake/`tools/list` error or timeout) → init_failed,
    // and their cause is retained for the model-facing reminder.
    state.record_init_failure(
        "dead-srv",
        false,
        Some("tools/list failed: boom".to_string()),
    );
    assert!(!state.auth_required.contains("dead-srv"));
    assert_eq!(
        state.init_failed.get("dead-srv").map(String::as_str),
        Some("tools/list failed: boom"),
    );

    // A fresh init attempt clears the failure entry and its cause.
    state.mark_servers_initializing(["dead-srv".to_string()]);
    assert!(!state.init_failed.contains_key("dead-srv"));
}

#[test]
fn test_clear_init_failed_removes_entry() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
    state.record_init_failure("dead-srv", false, Some("boom".to_string()));
    assert!(state.init_failed.contains_key("dead-srv"));

    // Symmetric with record_init_failure: the reactive re-auth path clears
    // a prior failure so a recovered server is not stuck Unavailable.
    state.clear_init_failed("dead-srv");
    assert!(!state.init_failed.contains_key("dead-srv"));
    // Idempotent: clearing an absent entry is a no-op.
    state.clear_init_failed("never-seen");
}

#[test]
fn test_mcp_state_update_configs_increments_generation() {
    let mut state = McpState::new(vec![]);

    // Each change should increment generation
    state.update_configs(vec![make_stdio_server("a", "/bin/a")]);
    assert_eq!(state.generation, 1);

    state.update_configs(vec![make_stdio_server("b", "/bin/b")]);
    assert_eq!(state.generation, 2);

    state.update_configs(vec![make_stdio_server("c", "/bin/c")]);
    assert_eq!(state.generation, 3);
}

#[test]
fn test_mcp_servers_equal_empty_lists() {
    let a: Vec<acp::McpServer> = vec![];
    let b: Vec<acp::McpServer> = vec![];
    assert!(mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_identical_configs() {
    let a = vec![make_stdio_server("test", "/bin/test")];
    let b = vec![make_stdio_server("test", "/bin/test")];
    assert!(mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_different_names() {
    let a = vec![make_stdio_server("test1", "/bin/test")];
    let b = vec![make_stdio_server("test2", "/bin/test")];
    assert!(!mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_different_lengths() {
    let a = vec![make_stdio_server("test", "/bin/test")];
    let b = vec![
        make_stdio_server("test", "/bin/test"),
        make_stdio_server("test2", "/bin/test2"),
    ];
    assert!(!mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_different_types() {
    let a = vec![make_stdio_server("test", "/bin/test")];
    let b = vec![make_http_server("test", "http://localhost")];
    assert!(!mcp_servers_equal(&a, &b));
}

#[test]
fn test_mcp_servers_equal_order_matters() {
    let a = vec![
        make_stdio_server("a", "/bin/a"),
        make_stdio_server("b", "/bin/b"),
    ];
    let b = vec![
        make_stdio_server("b", "/bin/b"),
        make_stdio_server("a", "/bin/a"),
    ];
    // Order matters since we're comparing JSON serialization
    assert!(!mcp_servers_equal(&a, &b));
}

#[test]
fn test_try_start_init_prevents_concurrent_init() {
    let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);

    // First call should succeed
    assert!(state.try_start_init());
    assert!(state.is_initializing());
    assert!(!state.is_initialized());

    // Second call should fail (already initializing)
    assert!(!state.try_start_init());
}

#[test]
fn test_try_start_init_fails_when_initialized() {
    let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);
    // Drive to Finished{empty} via the typed API.
    assert!(state.try_start_init());
    state.finish_init();
    assert!(state.is_initialized());

    // Second `try_start_init` must be rejected: we're already done.
    assert!(!state.try_start_init());
    assert!(!state.is_initializing());
    assert!(state.is_initialized(), "is_initialized stays true");
}

#[test]
fn test_finish_init_clears_initializing() {
    let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);

    state.try_start_init();
    assert!(state.is_initializing());
    assert!(!state.is_initialized());

    state.finish_init();
    assert!(!state.is_initializing());
    assert!(state.is_initialized());
}

#[test]
fn test_cancel_init_clears_initializing() {
    let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);

    state.try_start_init();
    assert!(state.is_initializing());

    state.cancel_init();
    assert!(!state.is_initializing());
    assert!(!state.is_initialized()); // Should NOT be marked as initialized
}

#[test]
fn test_update_configs_resets_initializing() {
    let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);
    state.try_start_init();
    assert!(state.is_initializing());

    // Updating configs should reset initializing flag
    state.update_configs(vec![make_stdio_server("test2", "/bin/test2")]);
    assert!(!state.is_initializing());
    assert!(!state.is_initialized());
}

#[test]
fn test_parse_mcp_meta_config_with_tool_timeouts_ms() {
    let meta = serde_json::json!({
        "mcpConfig": {
            "github": {
                "toolTimeoutMs": 60000,
                "toolTimeoutsMs": {
                    "create_issue": 120000,
                    "search": 30000
                }
            }
        }
    })
    .as_object()
    .cloned()
    .unwrap();
    let map = parse_mcp_meta_config(Some(&meta));
    let github = map.get("github").unwrap();
    assert_eq!(github.tool_timeout_ms, Some(60000));
    let tt = github.tool_timeouts_ms.as_ref().unwrap();
    assert_eq!(tt.get("create_issue"), Some(&120000));
    assert_eq!(tt.get("search"), Some(&30000));
}

#[test]
fn test_parse_mcp_meta_config_without_tool_timeouts_ms() {
    let meta = serde_json::json!({
        "mcpConfig": {
            "github": {
                "toolTimeoutMs": 60000
            }
        }
    })
    .as_object()
    .cloned()
    .unwrap();
    let map = parse_mcp_meta_config(Some(&meta));
    let github = map.get("github").unwrap();
    assert_eq!(github.tool_timeout_ms, Some(60000));
    assert!(github.tool_timeouts_ms.is_none());
    assert!(github.expose_image_base64.is_none());
}

/// Locks in the `exposeImageBase64` camelCase wire-format contract.
#[test]
fn test_parse_mcp_meta_config_with_expose_image_base64() {
    let meta = serde_json::json!({
        "mcpConfig": {
            "grafana": { "exposeImageBase64": true },
            "linear":  { "exposeImageBase64": false },
        }
    })
    .as_object()
    .cloned()
    .unwrap();
    let map = parse_mcp_meta_config(Some(&meta));
    assert_eq!(map.get("grafana").unwrap().expose_image_base64, Some(true));
    assert_eq!(map.get("linear").unwrap().expose_image_base64, Some(false));
}

#[test]
fn test_tool_timeout_for_returns_per_tool_override() {
    let mut tool_timeouts = HashMap::new();
    tool_timeouts.insert("create_issue".to_string(), 120u64);
    tool_timeouts.insert("search".to_string(), 30u64);

    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(10),
        tool_timeout_sec: Some(60),
        tool_timeouts: Some(tool_timeouts),
        ..Default::default()
    };
    let client = McpClient::new_http(
        "github".to_string(),
        HttpConfig {
            url: String::new(),
            headers: vec![],
        },
        Some(&overrides),
        None,
    );

    // Per-tool overrides
    assert_eq!(client.tool_timeout_for("create_issue"), 120);
    assert_eq!(client.tool_timeout_for("search"), 30);
    // Falls back to server-level default
    assert_eq!(client.tool_timeout_for("list_repos"), 60);
    assert_eq!(client.tool_timeout_for(""), 60);
}

#[test]
fn test_tool_timeout_for_empty_map_returns_default() {
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(10),
        tool_timeout_sec: Some(45),
        ..Default::default()
    };
    let client = McpClient::new_http(
        "test".to_string(),
        HttpConfig {
            url: String::new(),
            headers: vec![],
        },
        Some(&overrides),
        None,
    );

    // All tools should get the server-level default
    assert_eq!(client.tool_timeout_for("any_tool"), 45);
    assert_eq!(client.tool_timeout_sec(), 45);
}

#[test]
fn test_load_timeouts_startup_precedence() {
    // No override -> the standalone default (env/config resolved by the shell).
    assert_eq!(
        McpClient::load_timeouts(None, None).0,
        DEFAULT_STARTUP_TIMEOUT_SECS
    );

    // A per-server `startup_timeout_sec` (injected by the shell) wins over the default...
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(7),
        ..Default::default()
    };
    assert_eq!(McpClient::load_timeouts(Some(&overrides), None).0, 7);

    // ...and `_meta.startup_timeout_ms` wins over that.
    let meta = McpServerMetaConfig {
        startup_timeout_ms: Some(12_000),
        ..Default::default()
    };
    assert_eq!(
        McpClient::load_timeouts(Some(&overrides), Some(&meta)).0,
        12
    );
}

#[test]
fn test_update_configs_diff_no_change() {
    let configs = vec![make_stdio_server("test", "/bin/test")];
    let mut state = McpState::new(configs.clone());
    assert!(state.update_configs_diff(configs).is_none());
    assert_eq!(state.generation, 0);
}

#[test]
fn test_update_configs_diff_added() {
    let configs = vec![make_stdio_server("a", "/bin/a")];
    let mut state = McpState::new(configs);

    let new_configs = vec![
        make_stdio_server("a", "/bin/a"),
        make_stdio_server("b", "/bin/b"),
    ];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert_eq!(diff.retained, vec!["a"]);
    assert_eq!(diff.added, vec!["b"]);
    assert!(diff.removed.is_empty());
    assert_eq!(state.generation, 1);
}

#[test]
fn test_update_configs_diff_removed() {
    let configs = vec![
        make_stdio_server("a", "/bin/a"),
        make_stdio_server("b", "/bin/b"),
    ];
    let mut state = McpState::new(configs);

    let new_configs = vec![make_stdio_server("a", "/bin/a")];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert_eq!(diff.retained, vec!["a"]);
    assert!(diff.added.is_empty());
    assert_eq!(diff.removed, vec!["b"]);
}

#[test]
fn test_update_configs_diff_changed() {
    let configs = vec![make_stdio_server("a", "/bin/a")];
    let mut state = McpState::new(configs);

    let new_configs = vec![make_stdio_server("a", "/bin/a_v2")];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert!(diff.retained.is_empty());
    assert_eq!(diff.added, vec!["a"]);
    assert_eq!(diff.removed, vec!["a"]);
}

#[test]
fn test_update_configs_diff_auth_required_cleanup() {
    let configs = vec![
        make_stdio_server("keep", "/bin/keep"),
        make_stdio_server("remove", "/bin/remove"),
    ];
    let mut state = McpState::new(configs);
    state.auth_required.insert("remove".to_string());
    state.auth_required.insert("keep".to_string());

    let new_configs = vec![make_stdio_server("keep", "/bin/keep")];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert_eq!(diff.retained, vec!["keep"]);
    assert_eq!(diff.removed, vec!["remove"]);
    assert!(state.auth_required.contains("keep"));
    assert!(!state.auth_required.contains("remove"));
}

#[test]
fn test_update_configs_diff_empty_to_nonempty() {
    let mut state = McpState::new(vec![]);
    let new_configs = vec![make_stdio_server("a", "/bin/a")];
    let diff = state
        .update_configs_diff(new_configs)
        .expect("should detect change");
    assert!(diff.retained.is_empty());
    assert_eq!(diff.added, vec!["a"]);
    assert!(diff.removed.is_empty());
}

#[test]
fn test_update_configs_diff_nonempty_to_empty() {
    let configs = vec![make_stdio_server("a", "/bin/a")];
    let mut state = McpState::new(configs);
    let diff = state
        .update_configs_diff(vec![])
        .expect("should detect change");
    assert!(diff.retained.is_empty());
    assert!(diff.added.is_empty());
    assert_eq!(diff.removed, vec!["a"]);
}

/// Two MCP servers exposing a tool with the same raw name must produce
/// `McpErasedTool` instances with **distinct** `ToolId`s (qualified with
/// the server name). Regression test for a bug where `McpErasedTool::id()`
/// returned the unqualified name, causing the second registration to
/// silently overwrite the first in the `LocalRegistry`.
#[test]
fn test_mcp_erased_tool_id_is_qualified() {
    use xai_tool_runtime::Tool;

    let mcp_state = Arc::new(Mutex::new(McpState::new(vec![])));

    let tool_a = McpErasedTool {
        tool: McpTool::new(
            "SearchUsers".to_string(),
            "Search users".to_string(),
            "calendar".to_string(),
            Arc::clone(&mcp_state),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };
    let tool_b = McpErasedTool {
        tool: McpTool::new(
            "SearchUsers".to_string(),
            "Search users".to_string(),
            "teams".to_string(),
            Arc::clone(&mcp_state),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };

    let id_a = tool_a.id();
    let id_b = tool_b.id();

    // IDs must be qualified with the server name.
    assert_eq!(id_a.as_str(), "calendar__SearchUsers");
    assert_eq!(id_b.as_str(), "teams__SearchUsers");

    // And therefore distinct.
    assert_ne!(id_a, id_b);
}

/// Registering two MCP tools with the same raw name from different servers
/// into a `LocalRegistry` must preserve both entries (no silent overwrite).
#[test]
fn test_same_raw_name_different_servers_no_local_registry_collision() {
    use xai_computer_hub_sdk::LocalRegistry;
    use xai_tool_runtime::Tool;

    let mcp_state = Arc::new(Mutex::new(McpState::new(vec![])));
    let registry = LocalRegistry::new();

    let tool_a = McpErasedTool {
        tool: McpTool::new(
            "SearchUsers".to_string(),
            "Search users on calendar".to_string(),
            "calendar".to_string(),
            Arc::clone(&mcp_state),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };
    let tool_b = McpErasedTool {
        tool: McpTool::new(
            "SearchUsers".to_string(),
            "Search users on teams".to_string(),
            "teams".to_string(),
            Arc::clone(&mcp_state),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };

    let id_a = tool_a.id();
    let id_b = tool_b.id();

    // First registration should not displace anything.
    let displaced_a = registry.register(tool_a);
    assert!(
        displaced_a.is_none(),
        "first registration should not displace"
    );

    // Second registration should also not displace anything (distinct IDs).
    let displaced_b = registry.register(tool_b);
    assert!(
        displaced_b.is_none(),
        "second registration must not overwrite first"
    );

    // Both tools must be independently resolvable.
    assert!(
        registry.find(&id_a).is_some(),
        "calendar tool must be found"
    );
    assert!(registry.find(&id_b).is_some(), "teams tool must be found");
    assert_eq!(registry.len(), 2);
}

fn make_test_client(name: &str) -> Arc<McpClient> {
    // Same shape as the no-transport placeholder.
    Arc::new(McpClient::stub(name))
}

#[test]
fn test_shared_mcp_pool_from_empty_state() {
    let state = McpState::new(vec![]);
    let pool = SharedMcpPool::from_state(&state);
    assert_eq!(pool.len(), 0);
    assert_eq!(pool.server_names().count(), 0);
    assert!(pool.configs().is_empty());
    assert!(pool.meta_config_map().is_empty());
    assert!(pool.get_client("anything").is_none());
}

#[test]
fn test_shared_mcp_pool_len_matches_client_count() {
    let mut state = McpState::new(vec![]);
    for name in ["alpha", "beta", "gamma"] {
        state
            .owned_clients
            .insert(name.to_string(), make_test_client(name));
    }
    let pool = SharedMcpPool::from_state(&state);
    assert_eq!(pool.len(), 3);
    assert_eq!(pool.len(), pool.server_names().count());
}

#[test]
fn test_shared_mcp_pool_snapshot_shares_arc_clients() {
    let mut state = McpState::new(vec![make_stdio_server("github", "/bin/gh")]);
    let client = make_test_client("github");
    state
        .owned_clients
        .insert("github".to_string(), Arc::clone(&client));

    let pool = SharedMcpPool::from_state(&state);
    let pool_client = pool.get_client("github").expect("should find client");

    // Must point to the same allocation (shared transport)
    assert!(Arc::ptr_eq(&client, pool_client));
}

#[test]
fn test_shared_mcp_pool_get_client_missing() {
    let mut state = McpState::new(vec![]);
    state
        .owned_clients
        .insert("a".to_string(), make_test_client("a"));
    let pool = SharedMcpPool::from_state(&state);

    assert!(pool.get_client("a").is_some());
    assert!(pool.get_client("nonexistent").is_none());
    assert!(pool.get_client("").is_none());
}

#[test]
fn test_shared_mcp_pool_server_names() {
    let mut state = McpState::new(vec![]);
    for name in ["alpha", "beta", "gamma"] {
        state
            .owned_clients
            .insert(name.to_string(), make_test_client(name));
    }

    let pool = SharedMcpPool::from_state(&state);
    let mut names: Vec<&str> = pool.server_names().collect();
    names.sort();
    assert_eq!(names, vec!["alpha", "beta", "gamma"]);
}

#[test]
fn test_shared_mcp_pool_snapshot_independent_of_state_mutations() {
    let mut state = McpState::new(vec![make_stdio_server("srv", "/bin/srv")]);
    state
        .owned_clients
        .insert("srv".to_string(), make_test_client("srv"));

    let pool = SharedMcpPool::from_state(&state);

    // Mutate state after snapshot
    state.owned_clients.clear();
    state.configs.clear();

    // Pool retains original data
    assert_eq!(pool.server_names().count(), 1);
    assert!(pool.get_client("srv").is_some());
    assert_eq!(pool.configs().len(), 1);
}

#[test]
fn test_shared_mcp_pool_meta_config_preserved() {
    let mut meta = McpMetaConfigMap::new();
    meta.insert(
        "github".to_string(),
        McpServerMetaConfig {
            startup_timeout_ms: Some(5000),
            tool_timeout_ms: Some(120000),
            tool_timeouts_ms: None,
            expose_image_base64: None,
        },
    );
    let state = McpState::new_with_meta(vec![make_http_server("github", "http://gh.local")], meta);
    let pool = SharedMcpPool::from_state(&state);

    let mc = pool
        .meta_config_map()
        .get("github")
        .expect("should have meta config");
    assert_eq!(mc.startup_timeout_ms, Some(5000));
    assert_eq!(mc.tool_timeout_ms, Some(120000));
}

#[test]
fn test_shared_mcp_pool_clone_shares_arcs() {
    let mut state = McpState::new(vec![]);
    let client = make_test_client("svc");
    state
        .owned_clients
        .insert("svc".to_string(), Arc::clone(&client));

    let pool = SharedMcpPool::from_state(&state);
    let pool2 = pool.clone();

    // Both clones share the same Arc<McpClient>
    let c1 = pool.get_client("svc").unwrap();
    let c2 = pool2.get_client("svc").unwrap();
    assert!(Arc::ptr_eq(c1, c2));
}

// ── owned/shared split behavioral tests ─────────────────────────

#[test]
fn test_get_client_owned_overrides_shared() {
    let mut state = McpState::new(vec![]);
    let shared = make_test_client("srv");
    let owned = make_test_client("srv");
    state
        .shared_clients
        .insert("srv".to_string(), Arc::clone(&shared));
    state
        .owned_clients
        .insert("srv".to_string(), Arc::clone(&owned));

    let got = state.get_client("srv").unwrap();
    assert!(Arc::ptr_eq(got, &owned));
    assert!(!Arc::ptr_eq(got, &shared));
}

#[test]
fn test_get_client_falls_through_to_shared() {
    let mut state = McpState::new(vec![]);
    let shared = make_test_client("srv");
    state
        .shared_clients
        .insert("srv".to_string(), Arc::clone(&shared));

    let got = state.get_client("srv").unwrap();
    assert!(Arc::ptr_eq(got, &shared));
    assert!(state.get_client("missing").is_none());
}

#[test]
fn test_all_clients_deduplicates_shared_by_owned() {
    let mut state = McpState::new(vec![]);
    state
        .owned_clients
        .insert("a".to_string(), make_test_client("a"));
    state
        .shared_clients
        .insert("a".to_string(), make_test_client("a-shared"));
    state
        .shared_clients
        .insert("b".to_string(), make_test_client("b-shared"));

    let all: Vec<_> = state.all_clients().map(|(n, _)| n.as_str()).collect();
    // "a" appears once (from owned), "b" from shared
    assert_eq!(all.iter().filter(|&&n| n == "a").count(), 1);
    assert!(all.contains(&"b"));
    assert_eq!(all.len(), 2);

    // The "a" entry must be the owned client, not the shared one
    let (_, a_client) = state.all_clients().find(|(n, _)| *n == "a").unwrap();
    assert!(Arc::ptr_eq(a_client, state.owned_clients.get("a").unwrap()));
}

#[test]
fn test_import_shared_clients_skips_config_collisions() {
    // Child has a config entry named "github" — importing a shared
    // client with the same name must be skipped.
    let mut state = McpState::new(vec![make_stdio_server("github", "/bin/gh")]);
    let mut pool_clients = HashMap::new();
    pool_clients.insert("github".to_string(), make_test_client("github"));
    pool_clients.insert("linear".to_string(), make_test_client("linear"));
    let pool = SharedMcpPool {
        clients: pool_clients,
        configs: vec![],
        meta_config_map: McpMetaConfigMap::new(),
    };

    state.import_shared_clients(&pool);

    assert!(
        !state.shared_clients.contains_key("github"),
        "github should be skipped — collides with child config"
    );
    assert!(
        state.shared_clients.contains_key("linear"),
        "linear should be imported — no collision"
    );
}

#[test]
fn test_update_configs_preserves_shared_clients() {
    let mut state = McpState::new(vec![make_stdio_server("old", "/bin/old")]);
    state
        .owned_clients
        .insert("old".to_string(), make_test_client("old"));
    let shared = make_test_client("inherited");
    state
        .shared_clients
        .insert("inherited".to_string(), Arc::clone(&shared));

    let changed = state.update_configs(vec![make_stdio_server("new", "/bin/new")]);

    assert!(changed);
    assert!(state.owned_clients.is_empty(), "owned should be cleared");
    assert_eq!(state.shared_clients.len(), 1, "shared should be untouched");
    assert!(Arc::ptr_eq(
        state.shared_clients.get("inherited").unwrap(),
        &shared
    ));
}

#[test]
fn test_update_configs_diff_preserves_shared_clients() {
    let mut state = McpState::new(vec![
        make_stdio_server("keep", "/bin/keep"),
        make_stdio_server("drop", "/bin/drop"),
    ]);
    state
        .owned_clients
        .insert("keep".to_string(), make_test_client("keep"));
    state
        .owned_clients
        .insert("drop".to_string(), make_test_client("drop"));
    let shared = make_test_client("inherited");
    state
        .shared_clients
        .insert("inherited".to_string(), Arc::clone(&shared));

    // New config removes "drop", keeps "keep"
    let diff = state
        .update_configs_diff(vec![make_stdio_server("keep", "/bin/keep")])
        .expect("configs changed");

    assert!(diff.removed.contains(&"drop".to_string()));
    assert!(diff.retained.contains(&"keep".to_string()));
    assert!(!state.owned_clients.contains_key("drop"));
    assert!(state.owned_clients.contains_key("keep"));
    // Shared clients must be completely untouched
    assert!(Arc::ptr_eq(
        state.shared_clients.get("inherited").unwrap(),
        &shared
    ));
}

#[test]
fn test_from_state_captures_both_owned_and_shared() {
    let mut state = McpState::new(vec![]);
    let owned = make_test_client("owned-srv");
    let shared = make_test_client("shared-srv");
    state
        .owned_clients
        .insert("owned-srv".to_string(), Arc::clone(&owned));
    state
        .shared_clients
        .insert("shared-srv".to_string(), Arc::clone(&shared));

    let pool = SharedMcpPool::from_state(&state);

    assert!(Arc::ptr_eq(pool.get_client("owned-srv").unwrap(), &owned));
    assert!(Arc::ptr_eq(pool.get_client("shared-srv").unwrap(), &shared));
    assert_eq!(pool.server_names().count(), 2);
}

#[test]
fn test_retain_clients_keeps_matching() {
    let mut state = McpState::new(vec![]);
    for name in ["github", "linear", "slack"] {
        state
            .owned_clients
            .insert(name.to_string(), make_test_client(name));
    }
    let mut pool = SharedMcpPool::from_state(&state);

    pool.retain_clients(|name| name == "github" || name == "slack");

    assert!(pool.get_client("github").is_some());
    assert!(pool.get_client("slack").is_some());
    assert!(pool.get_client("linear").is_none());
    assert_eq!(pool.server_names().count(), 2);
}

#[test]
fn test_retain_clients_remove_all() {
    let mut state = McpState::new(vec![]);
    state
        .owned_clients
        .insert("srv".to_string(), make_test_client("srv"));
    let mut pool = SharedMcpPool::from_state(&state);

    pool.retain_clients(|_| false);

    assert_eq!(pool.server_names().count(), 0);
    assert!(pool.get_client("srv").is_none());
}

#[test]
fn test_retain_clients_keep_all() {
    let mut state = McpState::new(vec![]);
    for name in ["a", "b", "c"] {
        state
            .owned_clients
            .insert(name.to_string(), make_test_client(name));
    }
    let mut pool = SharedMcpPool::from_state(&state);

    pool.retain_clients(|_| true);

    assert_eq!(pool.server_names().count(), 3);
}

#[test]
fn test_retain_clients_preserves_arc_identity() {
    let mut state = McpState::new(vec![]);
    let client = make_test_client("keep");
    state
        .owned_clients
        .insert("keep".to_string(), Arc::clone(&client));
    state
        .owned_clients
        .insert("drop".to_string(), make_test_client("drop"));
    let mut pool = SharedMcpPool::from_state(&state);

    pool.retain_clients(|name| name == "keep");

    assert!(Arc::ptr_eq(pool.get_client("keep").unwrap(), &client));
}

fn make_mcp_tool(server_name: &str, name: &str) -> McpTool {
    McpTool::new(
        name.to_string(),
        "test desc".to_string(),
        server_name.to_string(),
        Arc::new(Mutex::new(McpState::new(vec![]))),
        serde_json::json!({}),
        None,
    )
}

#[test]
fn qualified_mcp_name_parser_accepts_structurally_valid_tool_ids() {
    for (name, expected) in [
        ("linear__list_issues", ("linear", "list_issues")),
        ("123__lookup", ("123", "lookup")),
        ("server:scope__tool", ("server:scope", "tool")),
    ] {
        let (id, server, tool) = parse_mcp_qualified_name(name).expect("valid qualified ID");
        assert_eq!(id.as_str(), name);
        assert_eq!((server, tool), expected);
        assert_eq!(
            parse_mcp_tool_name(name),
            Some((expected.0.to_owned(), expected.1.to_owned()))
        );
    }
}

#[test]
fn qualified_mcp_name_parser_rejects_malformed_names() {
    for name in [
        "server__part__tool",
        "server__tool__part",
        "foo___bar",
        "foo____bar",
        "__tool",
        "server__",
        "server",
        "",
        "server__bad.tool",
    ] {
        assert!(
            parse_mcp_qualified_name(name).is_none(),
            "unexpectedly accepted {name:?}"
        );
    }
}

#[test]
fn into_registration_validates_qualified_name() {
    let registration = make_mcp_tool("linear", "list_issues")
        .into_registration()
        .expect("should register");
    assert_eq!(registration.name, "linear__list_issues");

    for (server, tool) in [
        ("server__part", "tool"),
        ("server", "tool__part"),
        ("foo_", "bar"),
        ("foo", "_bar"),
        ("foo_", "_bar"),
        ("", "tool"),
        ("server", ""),
    ] {
        assert!(
            make_mcp_tool(server, tool).into_registration().is_none(),
            "unexpectedly registered {server:?} and {tool:?}"
        );
    }
}

#[test]
fn into_registration_preserves_provider_name_policy() {
    for qualified in ["123__lookup", "server:scope__tool"] {
        assert!(parse_mcp_qualified_name(qualified).is_some());
        let (server, tool) = qualified.split_once("__").unwrap();
        assert!(make_mcp_tool(server, tool).into_registration().is_none());
    }

    let server_61 = format!("a{}", "b".repeat(60));
    let server_62 = format!("a{}", "b".repeat(61));
    let valid_64 = format!("{server_61}__b");
    let invalid_65 = format!("{server_62}__b");
    assert_eq!(valid_64.len(), 64);
    assert_eq!(invalid_65.len(), 65);
    assert!(parse_mcp_qualified_name(&valid_64).is_some());
    assert!(parse_mcp_qualified_name(&invalid_65).is_some());
    assert!(make_mcp_tool(&server_61, "b").into_registration().is_some());
    assert!(make_mcp_tool(&server_62, "b").into_registration().is_none());
}

// ── is_retriable_transport_error tests ───────────────────────────

#[test]
fn test_is_retriable_transport_closed() {
    assert!(is_retriable_transport_error(&ServiceError::TransportClosed));
}

#[test]
fn test_is_retriable_transport_send() {
    let err = ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
        "test",
        std::any::TypeId::of::<()>(),
        Box::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "connection reset",
        )),
    ));
    assert!(is_retriable_transport_error(&err));
}

#[test]
fn test_not_retriable_unexpected_response() {
    assert!(!is_retriable_transport_error(
        &ServiceError::UnexpectedResponse
    ));
}

#[test]
fn test_not_retriable_cancelled() {
    assert!(!is_retriable_transport_error(&ServiceError::Cancelled {
        reason: Some("shutdown".to_string()),
    }));
}

#[test]
fn test_not_retriable_timeout() {
    assert!(!is_retriable_transport_error(&ServiceError::Timeout {
        timeout: std::time::Duration::from_secs(30),
    }));
}

fn mcp_service_err(code: i32) -> ServiceError {
    ServiceError::McpError(rmcp::ErrorData::new(
        rmcp::model::ErrorCode(code),
        "boom",
        None,
    ))
}

#[test]
fn should_recover_mcp_error_recovers_everything_outside_excluded_set() {
    assert!(should_recover_mcp_error(-32603));
    assert!(should_recover_mcp_error(-32002));
    assert!(should_recover_mcp_error(-32000));
    assert!(should_recover_mcp_error(-32099));
    assert!(should_recover_mcp_error(-32100));
    assert!(should_recover_mcp_error(0));
    assert!(should_recover_mcp_error(1));
    assert!(should_recover_mcp_error(i32::MIN));
    assert!(should_recover_mcp_error(i32::MAX));
}

#[test]
fn should_recover_mcp_error_skips_deterministic_client_errors() {
    assert!(!should_recover_mcp_error(-32700));
    assert!(!should_recover_mcp_error(-32600));
    assert!(!should_recover_mcp_error(-32601));
    assert!(!should_recover_mcp_error(-32602));
}

#[test]
fn should_recover_service_error_http_mcperror_recoverable() {
    assert!(should_recover_service_error(
        &mcp_service_err(-32603),
        true,
        false,
    ));
}

#[test]
fn should_recover_service_error_http_mcperror_invalid_params_skipped() {
    assert!(!should_recover_service_error(
        &mcp_service_err(-32602),
        true,
        false,
    ));
}

#[test]
fn should_recover_service_error_stdio_mcperror_not_recovered() {
    assert!(!should_recover_service_error(
        &mcp_service_err(-32603),
        false,
        false,
    ));
}

#[test]
fn should_recover_service_error_mcperror_at_most_once_per_dispatch() {
    assert!(!should_recover_service_error(
        &mcp_service_err(-32603),
        true,
        true,
    ));
}

#[test]
fn should_recover_service_error_http_mcperror_auth_rejection_not_recovered() {
    let auth_err = ServiceError::McpError(rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32603),
        "Unauthorized: token expired",
        None,
    ));
    assert!(!should_recover_service_error(&auth_err, true, false));
    let session_err = ServiceError::McpError(rmcp::ErrorData::new(
        rmcp::model::ErrorCode(-32603),
        "session not found",
        None,
    ));
    assert!(should_recover_service_error(&session_err, true, false));
}

#[test]
fn should_recover_service_error_transport_errors_always_recover() {
    assert!(should_recover_service_error(
        &ServiceError::TransportClosed,
        true,
        false
    ));
    assert!(should_recover_service_error(
        &ServiceError::TransportClosed,
        false,
        false
    ));
    assert!(should_recover_service_error(
        &ServiceError::TransportClosed,
        true,
        true
    ));
}

#[test]
fn should_recover_service_error_other_non_transport_not_recovered() {
    assert!(!should_recover_service_error(
        &ServiceError::UnexpectedResponse,
        true,
        false
    ));
    assert!(!should_recover_service_error(
        &ServiceError::Timeout {
            timeout: std::time::Duration::from_secs(30),
        },
        true,
        false
    ));
}

#[tokio::test]
async fn recover_and_retry_surfaces_original_error_when_recover_fails() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(1),
        ..Default::default()
    };
    let client = Arc::new(McpClient::new_http(
        "wedged".to_string(),
        config,
        Some(&overrides),
        None,
    ));

    let tool = McpErasedTool {
        tool: McpTool::new(
            "do_thing".to_string(),
            "desc".to_string(),
            "wedged".to_string(),
            Arc::new(Mutex::new(McpState::new(vec![]))),
            serde_json::json!({"type": "object"}),
            None,
        ),
    };

    let original = mcp_service_err(-32603);
    let expected = original.to_string();
    let params = CallToolRequestParams::new("do_thing");

    let mut reconnect_attempted = false;
    let mut is_timeout = false;
    let ew = xai_grok_session_events::EventWriter::noop();

    let err = tool
        .recover_and_retry(
            &client,
            params,
            std::time::Duration::from_secs(1),
            1,
            original,
            &mut reconnect_attempted,
            &mut is_timeout,
            &ew,
        )
        .await
        .expect_err("recover must fail against an unreachable host");

    assert_eq!(err.to_string(), expected, "original error must be surfaced");
    assert!(reconnect_attempted, "reconnect attempt must be flagged");
    assert!(!is_timeout, "a recover failure is not a tool timeout");
}

use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum CallToolBehavior {
    ErrorThenOk { code: i32 },
    AlwaysError { code: i32 },
    HangThenOk { hang_ms: u64 },
    ErrorThenHang { code: i32, hang_ms: u64 },
}

#[derive(Clone)]
struct FakeMcpHandles {
    inits: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    init_version: Arc<parking_lot::Mutex<Option<String>>>,
}

#[derive(Clone)]
struct FakeMcpState {
    behavior: CallToolBehavior,
    handles: FakeMcpHandles,
}

async fn fake_handle_post(
    axum::extract::State(state): axum::extract::State<FakeMcpState>,
    axum::Json(req): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = req["id"].clone();
    let ok = || {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "result": {"content": [{"type": "text", "text": "ok"}], "isError": false},
        })
    };
    let err = |code: i32, msg: String| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "error": {"code": code, "message": msg},
        })
    };
    match req["method"].as_str() {
        Some("initialize") => {
            state.handles.inits.fetch_add(1, Ordering::Relaxed);
            *state.handles.init_version.lock() =
                req["params"]["protocolVersion"].as_str().map(str::to_owned);
            let result = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.clone(),
                "result": {
                    "protocolVersion": req["params"]["protocolVersion"].clone(),
                    "capabilities": {},
                    "serverInfo": {"name": "fake", "version": "0.0.0"},
                },
            });
            ([("mcp-session-id", "fake-session")], axum::Json(result)).into_response()
        }
        Some("tools/list") => axum::Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]},
        }))
        .into_response(),
        Some("tools/call") => {
            let n = state.handles.calls.fetch_add(1, Ordering::Relaxed);
            match state.behavior {
                CallToolBehavior::ErrorThenOk { code } => {
                    if n == 0 {
                        axum::Json(err(code, "session expired".to_string())).into_response()
                    } else {
                        axum::Json(ok()).into_response()
                    }
                }
                CallToolBehavior::AlwaysError { code } => {
                    axum::Json(err(code, format!("attempt {}", n + 1))).into_response()
                }
                CallToolBehavior::HangThenOk { hang_ms } => {
                    if n == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(hang_ms)).await;
                    }
                    axum::Json(ok()).into_response()
                }
                CallToolBehavior::ErrorThenHang { code, hang_ms } => {
                    if n == 0 {
                        axum::Json(err(code, "session expired".to_string())).into_response()
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(hang_ms)).await;
                        axum::Json(ok()).into_response()
                    }
                }
            }
        }
        _ => axum::http::StatusCode::ACCEPTED.into_response(),
    }
}

async fn fake_handle_get() -> axum::response::Response {
    use axum::response::IntoResponse;
    let body =
        axum::body::Body::from_stream(futures::stream::pending::<Result<String, std::io::Error>>());
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

async fn spawn_fake_mcp(behavior: CallToolBehavior) -> (String, FakeMcpHandles) {
    let handles = FakeMcpHandles {
        inits: Arc::new(AtomicUsize::new(0)),
        calls: Arc::new(AtomicUsize::new(0)),
        init_version: Arc::new(parking_lot::Mutex::new(None)),
    };
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::get(fake_handle_get).post(fake_handle_post),
        )
        .with_state(FakeMcpState {
            behavior,
            handles: handles.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/mcp"), handles)
}

fn fake_http_client(url: &str, tool_timeout_sec: u64) -> Arc<McpClient> {
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(5),
        tool_timeout_sec: Some(tool_timeout_sec),
        ..Default::default()
    };
    Arc::new(McpClient::new_http(
        "fake".to_string(),
        HttpConfig {
            url: url.to_string(),
            headers: vec![],
        },
        Some(&overrides),
        None,
    ))
}

fn fake_echo_tool() -> McpErasedTool {
    McpErasedTool {
        tool: McpTool::new(
            "echo".to_string(),
            "echo desc".to_string(),
            "fake".to_string(),
            Arc::new(Mutex::new(McpState::new(vec![]))),
            serde_json::json!({"type": "object"}),
            None,
        ),
    }
}

fn event_types(jsonl: &str) -> Vec<serde_json::Value> {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_mcperror_recovers_then_retry_succeeds() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::ErrorThenOk { code: -32603 }).await;
    let client = fake_http_client(&url, 5);
    let tool = fake_echo_tool();
    let tmp = tempfile::tempdir().unwrap();
    let ew = xai_grok_session_events::EventWriter::open(tmp.path());

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let out = tool
        .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
        .await
        .expect("recovered call should succeed");

    assert!(
        !out.is_error.unwrap_or(false),
        "retry should return a success result"
    );
    assert!(reconnect, "reconnect_attempted must be set");
    assert!(!is_timeout);
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        2,
        "one failed + one retried tools/call"
    );
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        2,
        "initial handshake + one recovery re-init"
    );
    assert_eq!(
        handles.init_version.lock().as_deref(),
        Some("2025-11-25"),
        "initialize must offer protocolVersion 2025-11-25"
    );

    let jsonl = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
    let events = event_types(&jsonl);
    assert!(
        events.iter().any(|e| e["type"] == "mcp_transport_error"),
        "expected mcp_transport_error in {jsonl}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "mcp_transport_reconnect" && e["success"] == true),
        "expected a successful mcp_transport_reconnect in {jsonl}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_retry_failure_surfaces_retry_error() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::AlwaysError { code: -32603 }).await;
    let client = fake_http_client(&url, 5);
    let tool = fake_echo_tool();
    let ew = xai_grok_session_events::EventWriter::noop();

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let err = tool
        .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
        .await
        .expect_err("both attempts fail");

    let msg = err.to_string();
    assert!(msg.contains("attempt 2"), "want retry error, got: {msg}");
    assert!(
        !msg.contains("attempt 1"),
        "must not surface the original error: {msg}"
    );
    assert!(reconnect);
    assert!(!is_timeout);
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        2,
        "one failed + one retried tools/call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_invalid_params_not_recovered() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::AlwaysError { code: -32602 }).await;
    let client = fake_http_client(&url, 5);
    let tool = fake_echo_tool();
    let ew = xai_grok_session_events::EventWriter::noop();

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let err = tool
        .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
        .await
        .expect_err("invalid params surfaced as-is");

    assert!(err.to_string().contains("attempt 1"), "got: {err}");
    assert!(!reconnect, "invalid-params must not trigger recovery");
    assert!(!is_timeout);
    assert_eq!(handles.calls.load(Ordering::Relaxed), 1, "no retry POST");
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        1,
        "no recovery re-init"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_outer_timeout_resets_transport_no_retry() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::HangThenOk { hang_ms: 3000 }).await;
    let client = fake_http_client(&url, 1);
    let tool = fake_echo_tool();
    let ew = xai_grok_session_events::EventWriter::noop();

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let err = tool
        .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
        .await
        .expect_err("call must time out");

    assert!(err.to_string().contains("timed out"), "got: {err}");
    assert!(is_timeout, "is_timeout must be set");
    assert!(reconnect, "timeout arm flags the reconnect after resetting");
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        1,
        "timed-out call is NOT retried"
    );
    assert!(matches!(
        client.state_kind().await,
        ClientStateKind::Pending
    ));
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        1,
        "no re-init during the timed-out dispatch"
    );

    let mut reconnect2 = false;
    let mut is_timeout2 = false;
    let out = tool
        .try_call_tool(&client, &raw, &mut reconnect2, &mut is_timeout2, &ew)
        .await
        .expect("second dispatch should re-init and succeed");
    assert!(!out.is_error.unwrap_or(false));
    assert!(!is_timeout2);
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        2,
        "second dispatch re-initialized the session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn try_call_tool_http_retry_timeout_surfaces_timeout() {
    let (url, handles) = spawn_fake_mcp(CallToolBehavior::ErrorThenHang {
        code: -32603,
        hang_ms: 3000,
    })
    .await;
    let client = fake_http_client(&url, 1);
    let tool = fake_echo_tool();
    let ew = xai_grok_session_events::EventWriter::noop();

    let mut reconnect = false;
    let mut is_timeout = false;
    let raw = serde_json::json!({});
    let err = tool
        .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
        .await
        .expect_err("the retried call must time out");

    assert!(err.to_string().contains("timed out"), "got: {err}");
    assert!(is_timeout, "retry-timeout must set is_timeout");
    assert!(reconnect, "recovery was attempted");
    assert_eq!(
        handles.calls.load(Ordering::Relaxed),
        2,
        "the retry tools/call was attempted"
    );
    assert_eq!(
        handles.inits.load(Ordering::Relaxed),
        2,
        "recovery re-initialized before the retry"
    );
}

// ── new_http stores http_config tests ────────────────────────────

#[test]
fn test_new_http_stores_http_config() {
    let config = HttpConfig {
        url: "http://localhost:5000/api/mcp".to_string(),
        headers: vec![("x-token".to_string(), "abc".to_string())],
    };
    let client = McpClient::new_http("example-mcp".to_string(), config, None, None);
    let stored = client
        .http_config
        .as_ref()
        .expect("http_config should be Some");
    assert_eq!(stored.url, "http://localhost:5000/api/mcp");
    assert_eq!(stored.headers.len(), 1);
    assert_eq!(stored.headers[0].0, "x-token");
}

#[test]
fn test_new_stdio_has_no_http_config() {
    // Stdio clients must NOT have http_config — they can't reconnect via HTTP.
    let client = McpClient::stub("stdio-srv");
    assert!(client.http_config.is_none());
}

// ── http_headers_match / refresh_managed_clients guard tests ─────

#[test]
fn http_headers_match_compares_full_set_order_insensitively() {
    let config = HttpConfig {
        url: "http://localhost:5000/api/mcp".to_string(),
        headers: vec![
            ("authorization".to_string(), "Bearer t".to_string()),
            ("x-scope".to_string(), "read".to_string()),
        ],
    };
    let client = McpClient::new_http("managed".to_string(), config, None, None);

    let equal: HashMap<String, String> = [
        ("x-scope".to_string(), "read".to_string()),
        ("authorization".to_string(), "Bearer t".to_string()),
    ]
    .into_iter()
    .collect();
    assert!(client.http_headers_match(&equal));

    let changed_value: HashMap<String, String> = [
        ("authorization".to_string(), "Bearer NEW".to_string()),
        ("x-scope".to_string(), "read".to_string()),
    ]
    .into_iter()
    .collect();
    assert!(!client.http_headers_match(&changed_value));

    let missing_key: HashMap<String, String> =
        [("authorization".to_string(), "Bearer t".to_string())]
            .into_iter()
            .collect();
    assert!(!client.http_headers_match(&missing_key));
}

#[test]
fn http_headers_match_handles_duplicate_stored_keys() {
    // Duplicate stored key must not mask a missing fresh key by inflating
    // the stored length to match.
    let config = HttpConfig {
        url: "http://localhost:5000/api/mcp".to_string(),
        headers: vec![
            ("authorization".to_string(), "Bearer t".to_string()),
            ("authorization".to_string(), "Bearer t".to_string()),
        ],
    };
    let client = McpClient::new_http("managed".to_string(), config, None, None);

    let two_distinct: HashMap<String, String> = [
        ("authorization".to_string(), "Bearer t".to_string()),
        ("x-scope".to_string(), "read".to_string()),
    ]
    .into_iter()
    .collect();
    assert!(!client.http_headers_match(&two_distinct));

    let single: HashMap<String, String> = [("authorization".to_string(), "Bearer t".to_string())]
        .into_iter()
        .collect();
    assert!(client.http_headers_match(&single));
}

#[test]
fn http_headers_match_false_for_non_http_client() {
    let client = McpClient::stub("stdio-srv");
    let headers: HashMap<String, String> = [("authorization".to_string(), "Bearer t".to_string())]
        .into_iter()
        .collect();
    assert!(!client.http_headers_match(&headers));
}

#[test]
fn refresh_managed_clients_keeps_arc_when_headers_unchanged() {
    let url = "http://localhost:5000/api/mcp";
    let mut state = McpState::new(vec![make_http_server("managed", url)]);
    let config = HttpConfig {
        url: url.to_string(),
        headers: vec![("authorization".to_string(), "Bearer t".to_string())],
    };
    state.owned_clients.insert(
        "managed".to_string(),
        Arc::new(McpClient::new_http(
            "managed".to_string(),
            config,
            None,
            None,
        )),
    );
    let before = Arc::clone(state.owned_clients.get("managed").unwrap());

    let fresh: HashMap<String, String> = [("authorization".to_string(), "Bearer t".to_string())]
        .into_iter()
        .collect();
    state.refresh_managed_clients(std::iter::once((url, &fresh)));

    let after = state.owned_clients.get("managed").unwrap();
    assert!(
        Arc::ptr_eq(&before, after),
        "unchanged headers must not rebuild the client"
    );
}

#[test]
fn refresh_managed_clients_installs_new_arc_when_headers_differ() {
    let url = "http://localhost:5000/api/mcp";
    let mut state = McpState::new(vec![make_http_server("managed", url)]);
    let config = HttpConfig {
        url: url.to_string(),
        headers: vec![("authorization".to_string(), "Bearer old".to_string())],
    };
    state.owned_clients.insert(
        "managed".to_string(),
        Arc::new(McpClient::new_http(
            "managed".to_string(),
            config,
            None,
            None,
        )),
    );
    let before = Arc::clone(state.owned_clients.get("managed").unwrap());

    let fresh: HashMap<String, String> = [("authorization".to_string(), "Bearer new".to_string())]
        .into_iter()
        .collect();
    state.refresh_managed_clients(std::iter::once((url, &fresh)));

    let after = state.owned_clients.get("managed").unwrap();
    assert!(
        !Arc::ptr_eq(&before, after),
        "changed headers must install a fresh client"
    );
    assert!(after.http_headers_match(&fresh));
}

// ── reset_transport tests ────────────────────────────────────────

#[tokio::test]
async fn test_reset_transport_succeeds_for_http_client() {
    let config = HttpConfig {
        url: "http://127.0.0.1:9/api/mcp".to_string(),
        headers: vec![],
    };
    let client = McpClient::new_http("example-mcp".to_string(), config, None, None);
    assert!(client.reset_transport().await);
}

#[tokio::test]
async fn test_reset_transport_fails_for_stub() {
    // Stub has `reconnect = None`, simulating a Stdio client.
    let client = McpClient::stub("stdio-srv");
    assert!(!client.reset_transport().await);
}

#[tokio::test]
async fn test_reset_transport_is_idempotent() {
    let config = HttpConfig {
        url: "http://127.0.0.1:9/api/mcp".to_string(),
        headers: vec![],
    };
    let client = McpClient::new_http("example-mcp".to_string(), config, None, None);

    // Multiple resets should all succeed.
    assert!(client.reset_transport().await);
    assert!(client.reset_transport().await);
    assert!(client.reset_transport().await);
}

#[tokio::test]
async fn test_reset_transport_makes_ensure_initialized_retry_handshake() {
    // Port 1 on loopback refuses immediately (ECONNREFUSED -> HandshakeFailed),
    // so each handshake fails fast instead of waiting out the connect timeout.
    let config = HttpConfig {
        url: "http://127.0.0.1:1/unreachable".to_string(),
        headers: vec![],
    };
    let client = McpClient::new_http("test".to_string(), config, None, None);

    // First ensure_initialized will fail (unreachable server) but proves
    // the client attempts a handshake from the Pending state.
    let err1 = client.ensure_initialized().await.unwrap_err();
    assert!(
        matches!(
            err1,
            McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
        ),
        "first init should fail: {err1}"
    );

    // Reset puts the client back into Pending with a fresh transport.
    assert!(client.reset_transport().await);

    // Second ensure_initialized should attempt another handshake (not
    // return a cached error). It will fail again with the same kind of
    // error, proving the reset restored the transport.
    let err2 = client.ensure_initialized().await.unwrap_err();
    assert!(
        matches!(
            err2,
            McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
        ),
        "second init after reset should also attempt handshake: {err2}"
    );
}

#[tokio::test]
async fn recover_errors_for_client_with_no_restorable_transport() {
    // A stub has `reconnect = None` (like Stdio): `recover` can't rebuild it.
    let err = Arc::new(McpClient::stub("stdio"))
        .recover()
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::ClientError(_)), "got {err}");
}

#[tokio::test]
async fn reset_transport_rebuilds_acp_client() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;

    struct NoopInvoker;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for NoopInvoker {
        async fn invoke(
            &self,
            _server_id: &str,
            _message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let client = McpClient::new_acp(
        "sdk-tools".to_string(),
        "srv_0".to_string(),
        Arc::new(NoopInvoker),
        None,
        None,
    );

    // ACP clients restore from `reconnect`, unlike Stdio.
    assert!(client.reset_transport().await);
    assert!(
        matches!(
            &*client.state.lock().await,
            ClientState::Pending(PendingTransport::Acp { .. })
        ),
        "reset_transport should restore the ACP transport to Pending"
    );
}

/// End-to-end reconnect-THEN-SUCCEED for the `try_call_tool` retry arm: the one
/// piece otherwise covered only by its parts (`is_retriable_transport_error`,
/// `reset_transport_*`, `ensure_initialized_*`).
///
/// Drives the REAL `McpErasedTool::try_call_tool` against a real
/// `McpClient`. The first `call_tool` hits a real `RunningService`
/// whose transport is already closed, so it returns a genuine,
/// retriable `ServiceError::TransportClosed`; the arm must then flag
/// `reconnect_attempted`, run the real `reset_transport` +
/// `ensure_initialized` re-handshake (rebuilding the ACP transport
/// against a working echo server), and return the SECOND attempt's
/// `Ok` result.
///
/// Why a separately-built dead service instead of failing the initial
/// connection: the ACP bridge transport can only be torn down from the
/// rmcp side, so a fresh real service is built over a raw duplex whose
/// server answers `initialize` then drops — closing the transport so
/// the first `call_tool` observes `TransportClosed`. Everything from
/// the retriable-error gate through the successful retry is real code.
#[tokio::test]
async fn try_call_tool_reconnects_then_succeeds_after_retriable_transport_error() {
    use crate::acp_transport::AcpReverseInvoker;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // Working in-process echo server for the post-reconnect retry.
    struct EchoSdkServer;
    #[async_trait::async_trait]
    impl AcpReverseInvoker for EchoSdkServer {
        async fn invoke(
            &self,
            _server_id: &str,
            message: serde_json::Value,
            _timeout: Duration,
        ) -> Result<serde_json::Value, String> {
            let id = message
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let method = message
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or_default();
            let result = match method {
                "initialize" => serde_json::json!({
                    "protocolVersion": message["params"]["protocolVersion"],
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "echo", "version": "0.0.0" },
                }),
                "tools/call" => serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": message["params"]["arguments"]["text"]
                            .as_str()
                            .unwrap_or_default(),
                    }],
                    "isError": false,
                }),
                other => return Err(format!("unexpected method {other}")),
            };
            Ok(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }
    }

    // A real `RunningService` whose transport is already closed: the
    // server answers `initialize`, consumes the `initialized`
    // notification (so the client's handshake send succeeds), then drops
    // its duplex ends. The next `call_tool` therefore observes a real
    // `ServiceError::TransportClosed`.
    async fn dead_service() -> McpService {
        let (client_read, server_write) = tokio::io::duplex(64 * 1024); // server -> client
        let (server_read, client_write) = tokio::io::duplex(64 * 1024); // client -> server
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_read);
            let mut writer = server_write;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                if msg.get("method").and_then(|m| m.as_str()) == Some("initialize") {
                    let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
                    let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": msg["params"]["protocolVersion"],
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "dead", "version": "0.0.0" },
                    }});
                    let mut encoded = serde_json::to_string(&resp).unwrap();
                    encoded.push('\n');
                    let _ = writer.write_all(encoded.as_bytes()).await;
                    let _ = writer.flush().await;
                    // Drain the `initialized` notification, then drop to close.
                    let _ = reader.read_line(&mut line).await;
                    return;
                }
            }
        });
        let handler = GrokClientHandler {
            info: McpClient::make_client_info("dead"),
            server_name: "dead".to_string(),
            notify_tx: Arc::new(parking_lot::Mutex::new(None)),
        };
        let transport = rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(
            client_read,
            client_write,
        );
        Arc::new(
            handler
                .serve(transport)
                .await
                .expect("dead-service handshake"),
        )
    }

    // ACP client whose `reconnect` snapshot rebuilds against the echo server.
    let client = Arc::new(McpClient::new_acp(
        "sdk".to_string(),
        "srv_0".to_string(),
        Arc::new(EchoSdkServer),
        None,
        None,
    ));
    // Inject the closed real service so the FIRST `call_tool` fails retriably.
    let dead = dead_service().await;
    *client.state.lock().await = ClientState::Ready(dead);

    let erased = McpErasedTool {
        tool: McpTool::new(
            "echo".to_string(),
            "echo".to_string(),
            "sdk".to_string(),
            Arc::new(Mutex::new(McpState::new(vec![]))),
            serde_json::json!({}),
            None,
        ),
    };

    let raw = serde_json::json!({ "text": "after reconnect" });
    let mut reconnect_attempted = false;
    let mut is_timeout = false;
    let ew = xai_grok_session_events::EventWriter::noop();
    let result = erased
        .try_call_tool(
            &client,
            &raw,
            &mut reconnect_attempted,
            &mut is_timeout,
            &ew,
        )
        .await
        .expect("retry after reconnect should succeed");

    // The Ok came from the SECOND attempt — the dead service cannot echo,
    // so this text proves the rebuilt transport served the retry.
    assert_eq!(
        result.content[0].as_text().expect("text content").text,
        "after reconnect"
    );
    assert!(
        reconnect_attempted,
        "retriable transport error must set reconnect_attempted"
    );
    assert!(
        !is_timeout,
        "successful retry must not be flagged as timeout"
    );
    // reset_transport + re-handshake replaced the dead service with a live one.
    assert!(matches!(&*client.state.lock().await, ClientState::Ready(_)));
}

#[test]
fn is_auth_rejection_message_matches_auth_signals() {
    // The verbatim string captured in production for a managed handshake.
    assert!(is_auth_rejection_message(
        "MCP server 'grok_com_notion' handshake failed: Auth required, when send initialize request"
    ));
    assert!(is_auth_rejection_message("401 Unauthorized"));
    assert!(is_auth_rejection_message("unauthorized"));
    assert!(is_auth_rejection_message("Authentication required"));
    assert!(is_auth_rejection_message("authentication failed"));
    assert!(is_auth_rejection_message("status: 401"));
    assert!(is_auth_rejection_message("HTTP status 401"));
    assert!(is_auth_rejection_message("server returned status code 401"));
    assert!(is_auth_rejection_message("HTTP 401"));
    assert!(is_auth_rejection_message("error 401"));
    // rmcp worker fatal context uses Debug form without spaces.
    assert!(is_auth_rejection_message(
        "worker quit with fatal: Transport channel closed, when Auth(AuthorizationRequired)"
    ));
    let auth_req = McpError::AuthRequired {
        server: "clickhouse".into(),
    };
    assert!(auth_req.is_auth_rejection());
    assert_eq!(auth_req.server_name(), Some("clickhouse"));
}

#[test]
fn auth_required_records_as_auth_not_init_failed_and_maps_category() {
    // Pre-spawn gate is owned by the auth state machine: it lands in
    // `auth_required` (recoverable via re-auth) and never `init_failed`.
    let mut state = McpState::new(vec![]);
    state.record_init_failure("oauth-srv", true, None);
    assert!(state.auth_required.contains("oauth-srv"));
    assert!(!state.init_failed.contains_key("oauth-srv"));

    // AuthRequired carries the AuthRequired telemetry category, not ClientError.
    let err = McpError::AuthRequired {
        server: "oauth-srv".into(),
    };
    assert!(matches!(
        err.error_category(),
        xai_grok_session_events::McpErrorCategory::AuthRequired
    ));
}

#[test]
fn is_auth_rejection_message_rejects_non_auth() {
    // Transport / timeout / spawn wording is never an auth rejection.
    assert!(!is_auth_rejection_message("Transport closed"));
    assert!(!is_auth_rejection_message(
        "MCP server 'x' timed out after 30s"
    ));
    assert!(!is_auth_rejection_message(
        "Failed to spawn MCP server 'x': No such file or directory"
    ));
    // 403/forbidden is a non-auth policy denial in this stack, not auth.
    assert!(!is_auth_rejection_message("403 Forbidden"));
    assert!(!is_auth_rejection_message("forbidden"));
    // Incidental digits must not trip the status-anchored 401 patterns.
    assert!(!is_auth_rejection_message("request took 401ms"));
    assert!(!is_auth_rejection_message("connect 10.0.4.01:443"));
    assert!(!is_auth_rejection_message("read 401 bytes"));
    // A status literal followed by another alphanumeric is a different
    // token: a longer number (4012) or an adjacent unit (401ms).
    assert!(!is_auth_rejection_message("http 4012"));
    assert!(!is_auth_rejection_message("error 4012"));
    assert!(!is_auth_rejection_message("status: 4012"));
    assert!(!is_auth_rejection_message("http 401ms"));
    assert!(!is_auth_rejection_message("error 401ms"));
    // ...but a trailing punctuation/whitespace still matches.
    assert!(is_auth_rejection_message("http 401."));
    assert!(is_auth_rejection_message("error 401: token expired"));
}

#[test]
fn mcp_error_is_auth_rejection_delegates() {
    assert!(McpError::ClientError("Auth required".to_string()).is_auth_rejection());
    assert!(!McpError::ClientError("Transport closed".to_string()).is_auth_rejection());
    assert!(
        !McpError::Timeout {
            server: "x".to_string(),
            timeout_secs: 30,
        }
        .is_auth_rejection()
    );
    assert!(
        !McpError::SpawnFailed {
            server: "x".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "401 Unauthorized"),
        }
        .is_auth_rejection()
    );
    // HandshakeFailed is the production carrier: its `source` Display must
    // surface the auth substring for the delegation to fire.
    assert!(
        McpError::HandshakeFailed {
            server: "x".to_string(),
            source: Box::new(ClientInitializeError::ConnectionClosed(
                "Auth required, when send initialize request".to_string()
            )),
        }
        .is_auth_rejection()
    );
    assert!(
        !McpError::HandshakeFailed {
            server: "x".to_string(),
            source: Box::new(ClientInitializeError::ConnectionClosed(
                "transport closed".to_string()
            )),
        }
        .is_auth_rejection()
    );
}

#[test]
fn format_mcp_image_default_emits_only_data_uri() {
    let out = format_mcp_image("image/png", "AAAA", false);
    assert_eq!(out, "data:image/png;base64,AAAA");
    assert!(!out.contains("<mcp_image_base64"));
}

#[test]
fn format_mcp_image_expose_emits_data_uri_and_raw_block() {
    let out = format_mcp_image("image/png", "AAAA", true);
    assert!(out.contains("data:image/png;base64,AAAA"));
    assert!(out.contains("<mcp_image_base64 mime=\"image/png\">\nAAAA\n</mcp_image_base64>"));
}

/// Wrapper must not re-match the extractor regex, else the raw copy gets stripped too.
#[test]
fn format_mcp_image_expose_raw_block_has_no_data_prefix() {
    let out = format_mcp_image("image/jpeg", "ZZZZ", true);
    assert_eq!(out.matches("data:image/").count(), 1);
}

#[test]
fn load_expose_image_base64_defaults_to_false() {
    assert!(!McpClient::load_expose_image_base64(None, None));
}

#[test]
fn load_expose_image_base64_uses_overrides_when_meta_unset() {
    let overrides = McpClientTimeoutOverrides {
        expose_image_base64: Some(true),
        ..Default::default()
    };
    assert!(McpClient::load_expose_image_base64(Some(&overrides), None));
}

#[test]
fn load_expose_image_base64_meta_wins_over_overrides() {
    let overrides = McpClientTimeoutOverrides {
        expose_image_base64: Some(true),
        ..Default::default()
    };
    let meta = McpServerMetaConfig {
        expose_image_base64: Some(false),
        ..Default::default()
    };
    assert!(!McpClient::load_expose_image_base64(
        Some(&overrides),
        Some(&meta)
    ));
}

#[test]
fn load_expose_image_base64_meta_falls_through_when_none() {
    let overrides = McpClientTimeoutOverrides {
        expose_image_base64: Some(true),
        ..Default::default()
    };
    let meta = McpServerMetaConfig::default(); // expose_image_base64 = None
    assert!(McpClient::load_expose_image_base64(
        Some(&overrides),
        Some(&meta)
    ));
}

/// End-to-end: override → constructor → public getter.
/// New constructors should add a similar assertion.
#[test]
fn new_http_propagates_expose_image_base64_override_to_getter() {
    let config = HttpConfig {
        url: "http://localhost/api/mcp".to_string(),
        headers: vec![],
    };
    let overrides = McpClientTimeoutOverrides {
        expose_image_base64: Some(true),
        ..Default::default()
    };
    let client = McpClient::new_http(
        "grafana".to_string(),
        config.clone(),
        Some(&overrides),
        None,
    );
    assert!(client.expose_image_base64());

    let client_default = McpClient::new_http("grafana".to_string(), config, None, None);
    assert!(!client_default.expose_image_base64());
}

// ------------------------------------------------------------------
// ensure_initialized single-flight + Notify behavior (regression
// suite for the "MCP client already initializing" doom-loop).
// ------------------------------------------------------------------

/// `ensure_initialized` on a stub (no transport) must surface a
/// clear, actionable configuration error — never the legacy
/// "already initializing" sentinel which leaked into model-visible
/// tool results and triggered retry loops that exhausted the
/// per-tick prompt budget.
#[tokio::test]
async fn ensure_initialized_on_empty_client_returns_no_transport_error() {
    let client = McpClient::stub("test-server");

    let err = client.ensure_initialized().await.unwrap_err();
    let msg = err.to_string();

    assert!(
        msg.contains("no transport configured"),
        "expected clear 'no transport configured' error, got: {msg}"
    );
    assert!(
        !msg.contains("already initializing"),
        "regression: legacy fast-fail sentinel surfaced: {msg}"
    );
}

/// Drive `N` `ensure_initialized` calls concurrently against an
/// unreachable HTTP server with a tight startup timeout. Every
/// caller must surface a real handshake error (`Timeout` or
/// `HandshakeFailed`); none may surface the legacy
/// "MCP client already initializing" sentinel which the
/// pre-fix branch emitted whenever a caller observed
/// `Pending(None)` while another caller was running the handshake.
///
/// The race window is intentionally widened by using an unreachable
/// host (`192.0.2.1:1` — TEST-NET-1, guaranteed unrouteable) so the
/// handshake stalls for `startup_timeout_sec` and every concurrent
/// caller spawned after the first observes `Initializing` instead
/// of `Pending`.
#[tokio::test]
async fn ensure_initialized_concurrent_callers_never_see_legacy_fast_fail() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(1),
        ..Default::default()
    };
    let client = Arc::new(McpClient::new_http(
        "test-server".to_string(),
        config,
        Some(&overrides),
        None,
    ));

    let mut handles = Vec::new();
    for _ in 0..5 {
        let c = Arc::clone(&client);
        handles.push(tokio::spawn(async move { c.ensure_initialized().await }));
    }

    for (idx, handle) in handles.into_iter().enumerate() {
        let result = handle.await.expect("task did not panic");
        let err = result.expect_err("unreachable host must fail");
        let msg = err.to_string();
        assert!(
            !msg.contains("MCP client already initializing"),
            "caller {idx}: legacy fast-fail sentinel surfaced: {msg}"
        );
        assert!(
            matches!(
                err,
                McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
            ),
            "caller {idx}: expected handshake failure, got: {err}"
        );
    }
}

/// A caller that finds `ClientState::Initializing` must park on
/// `init_done` and wake up when the holder publishes a new state,
/// then take the freshly-restored transport for its own retry.
///
/// We exercise the wake path directly (without an actual concurrent
/// handshake) by manually transitioning state to `Initializing`,
/// spawning a parker, then transitioning back to `Pending` and
/// firing `notify_waiters`. The parker should retry against the
/// restored (still-unreachable) transport and surface a normal
/// handshake error rather than the wait-timeout error.
#[tokio::test]
async fn ensure_initialized_parked_caller_retries_after_notify() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(1),
        ..Default::default()
    };
    let client = Arc::new(McpClient::new_http(
        "test-server".to_string(),
        config.clone(),
        Some(&overrides),
        None,
    ));

    // Simulate an in-flight handshake by another task: pretend
    // that task took the transport and entered Initializing.
    *client.state.lock().await = ClientState::Initializing;

    // Spawn the parker. It must observe Initializing and park on
    // `init_done` rather than fail-fast.
    let parker_client = Arc::clone(&client);
    let parker = tokio::spawn(async move { parker_client.ensure_initialized().await });

    // Give the parker a chance to reach the await on `init_done`.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Publish a fresh Pending transport and notify — simulates the
    // holder's failure-path restore.
    *client.state.lock().await = ClientState::Pending(PendingTransport::Http(config.clone()));
    client.init_done.notify_waiters();

    // The parker should wake, take the transport, run its own
    // handshake (which fails against the unreachable host), and
    // surface a regular handshake error — never the wait-timeout
    // error and never the legacy fast-fail.
    let err = parker
        .await
        .expect("parker did not panic")
        .expect_err("unreachable host must fail");
    let msg = err.to_string();
    assert!(
        !msg.contains("MCP client already initializing"),
        "regression: legacy fast-fail sentinel: {msg}"
    );
    assert!(
        !msg.contains("init still in progress"),
        "parker should not hit wait-timeout when notified: {msg}"
    );
    assert!(
        matches!(
            err,
            McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
        ),
        "expected handshake failure, got: {err}"
    );
}

/// If a caller is parked on `Initializing` and the holder is
/// dropped without notifying (cancellation-storm edge case), the
/// parker must eventually surface a clear `init still in progress`
/// timeout error rather than block indefinitely.
///
/// Without the inflight-wait timeout, a wedged client (one whose
/// drop guard couldn't acquire the lock to restore) would silently
/// stall every future `ensure_initialized` caller until process
/// restart. The 1 s margin past `startup_timeout_sec` keeps the
/// happy path snappy while still bounding the worst case.
#[tokio::test]
async fn ensure_initialized_inflight_wait_times_out_when_holder_silent() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
    };
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(0),
        ..Default::default()
    };
    let client = McpClient::new_http("test-server".to_string(), config, Some(&overrides), None);

    // Wedge the slot in Initializing with no live holder.
    *client.state.lock().await = ClientState::Initializing;

    let err = client.ensure_initialized().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("init still in progress"),
        "expected wait-timeout error, got: {msg}"
    );
    assert!(
        !msg.contains("already initializing"),
        "regression: legacy fast-fail sentinel: {msg}"
    );
}

/// When the holder task is cancelled (`abort()`) mid-handshake, the
/// `InitGuard` drop impl restores `Pending(transport)` on a
/// best-effort basis so a follow-on caller can retry without
/// requiring an explicit `reset_transport`.
#[tokio::test]
async fn ensure_initialized_drop_guard_restores_state_after_holder_aborted() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
    };
    let overrides = McpClientTimeoutOverrides {
        // Long enough that the holder is guaranteed to still be
        // inside try_handshake when we abort it.
        startup_timeout_sec: Some(10),
        ..Default::default()
    };
    let client = Arc::new(McpClient::new_http(
        "test-server".to_string(),
        config,
        Some(&overrides),
        None,
    ));

    let holder_client = Arc::clone(&client);
    let holder = tokio::spawn(async move { holder_client.ensure_initialized().await });

    // Wait for the holder to enter Initializing.
    let started = std::time::Instant::now();
    loop {
        if matches!(&*client.state.lock().await, ClientState::Initializing) {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "holder never reached Initializing"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Cancel the holder mid-handshake. The drop guard should
    // restore Pending so the next caller can retry.
    holder.abort();
    let _ = holder.await;

    // The drop guard restores best-effort via `try_lock` and notifies.
    // Wait briefly for it to settle.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    match &*client.state.lock().await {
        ClientState::Pending(_) => {} // expected
        other => panic!(
            "expected Pending after holder abort + drop guard, found {}",
            state_label(other)
        ),
    }
}

/// `McpState::is_initialized()` MUST require both the early
/// `finish_init` flag AND an empty `initializing_servers` set.
///
/// The session actor's `start_mcp_servers` path calls `finish_init`
/// **early** (right after spawning processes, before any handshake
/// completes) so non-MCP work can proceed in parallel. Tool dispatch
/// and the Blocking-strategy prompt guard, however, must NOT
/// observe "initialized" until every per-server handshake is done —
/// otherwise the model's first tool call races the background
/// `get_tool_registrations` handshake and the
/// `McpClient::ensure_initialized` window described above triggers.
#[test]
fn test_mcp_state_is_initialized_requires_empty_initializing_servers() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);

    // NotStarted: neither flag set, no per-server work.
    assert!(!state.is_initialized());
    assert!(!state.is_initializing());
    assert!(!state.has_finished_init());
    assert!(matches!(state.init_progress(), InitProgress::NotStarted));

    // Starting: try_start_init fired, per-server names registered,
    // finish_init has NOT yet fired. is_initializing() is true.
    assert!(state.try_start_init());
    state.mark_servers_initializing(["a".to_string()]);
    assert!(!state.is_initialized());
    assert!(state.is_initializing());
    assert!(!state.has_finished_init());
    assert!(matches!(
        state.init_progress(),
        InitProgress::Starting { .. }
    ));

    // Finished + handshakes outstanding: actor called finish_init
    // early but the per-server background handshake is still in
    // flight. is_initialized() must be FALSE during this window.
    state.finish_init();
    assert!(
        !state.is_initialized(),
        "is_initialized() must wait for per-server handshakes"
    );
    assert!(
        state.is_initializing(),
        "is_initializing() must report in-flight per-server work"
    );
    assert!(state.has_finished_init());
    assert!(state.is_server_handshaking("a"));
    assert_eq!(state.handshaking_servers_count(), 1);

    // Finished + empty: background task has reported the handshake
    // complete. Now and only now is the pool fully initialized.
    state.mark_server_ready("a");
    assert!(state.is_initialized());
    assert!(!state.is_initializing());
    assert!(state.has_finished_init());
    assert!(!state.is_server_handshaking("a"));
    assert_eq!(state.handshaking_servers_count(), 0);
}

/// Locks in the typed-state contract: the `init_progress` field
/// makes nonsensical combinations like "initialized AND
/// initializing" structurally unrepresentable. Every legal state
/// has exactly one [`InitProgress`] variant; every transition is
/// driven through the typed methods.
#[test]
fn test_init_progress_state_machine_invariants() {
    let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);

    // Invariant: try_start_init is one-shot per cycle.
    assert!(state.try_start_init());
    assert!(!state.try_start_init(), "double try_start_init is rejected");

    // Invariant: mark_all_servers_ready clears handshaking in
    // both Starting and Finished states; never resurrects them.
    state.mark_servers_initializing(["a".to_string(), "b".to_string()]);
    assert_eq!(state.handshaking_servers_count(), 2);
    state.mark_all_servers_ready();
    assert_eq!(state.handshaking_servers_count(), 0);
    assert!(
        matches!(state.init_progress(), InitProgress::Starting { .. }),
        "mark_all_servers_ready preserves the lifecycle variant"
    );

    // Invariant: finish_init from Starting → Finished preserves
    // (or in this case, the now-empty) handshaking set.
    state.finish_init();
    assert!(state.is_initialized());
    assert!(matches!(
        state.init_progress(),
        InitProgress::Finished { .. }
    ));

    // Invariant: cancel_init returns us cleanly to NotStarted,
    // ready for a new try_start_init.
    state.cancel_init();
    assert!(matches!(state.init_progress(), InitProgress::NotStarted));
    assert!(state.try_start_init(), "cancel_init re-enables init");
}

fn state_label(s: &ClientState) -> &'static str {
    match s {
        ClientState::Empty => "Empty",
        ClientState::Pending(_) => "Pending",
        ClientState::Initializing => "Initializing",
        ClientState::Ready(_) => "Ready",
    }
}

// -- is_healthy / state_kind --------------------------------------
//
// These tests cover the cheap, non-blocking predicate. They focus
// on the state-machine inspection: any
// non-`Ready` variant returns `false` for `is_healthy`, and
// `state_kind` projects every variant onto the matching
// [`ClientStateKind`].
//
// The two `Ready` cases
// (`is_healthy_ready_open_returns_true` and
// `is_healthy_transport_closed_returns_false`) require a real
// `RunningService<RoleClient, InitializeRequestParams>`, which can
// only be constructed through rmcp's `serve_client` path. That
// path needs a peer that responds to the MCP initialize
// handshake, and this crate intentionally does NOT enable rmcp's
// `server` feature (see `Cargo.toml`). Wiring up a hand-rolled
// JSON-RPC responder over `tokio::io::duplex` would balloon the
// test scaffolding far beyond what these tests need. We therefore
// exercise the `Ready` arm indirectly: the cheap predicate is a
// single `match` on the state mutex plus
// `Peer::is_transport_closed`, which is upstream-tested in rmcp
// itself (`rmcp-2.1.0/tests/test_close_connection.rs`).

#[tokio::test]
async fn is_healthy_empty_returns_false() {
    let client = McpClient::stub("empty");
    // `stub` starts in `ClientState::Empty`.
    assert!(matches!(*client.state.lock().await, ClientState::Empty));
    assert!(!client.is_healthy().await);
    assert_eq!(client.state_kind().await, ClientStateKind::Empty);
}

#[tokio::test]
async fn is_healthy_pending_returns_false() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
    };
    let client = McpClient::new_http("pending".to_string(), config, None, None);
    // `new_http` constructs with `ClientState::Pending(_)`.
    assert!(matches!(
        *client.state.lock().await,
        ClientState::Pending(_)
    ));
    assert!(!client.is_healthy().await);
    assert_eq!(client.state_kind().await, ClientStateKind::Pending);
}

#[tokio::test]
async fn is_healthy_initializing_returns_false() {
    let client = McpClient::stub("initializing");
    *client.state.lock().await = ClientState::Initializing;
    assert!(!client.is_healthy().await);
    assert_eq!(client.state_kind().await, ClientStateKind::Initializing);
}

/// `is_healthy` MUST NOT trigger a handshake. Regression guard:
/// the previous implementation called `ensure_initialized`, which
/// for a `Pending` HTTP client pointing at an unreachable host
/// would block for `startup_timeout_sec` seconds. The cheap
/// predicate must return immediately.
#[tokio::test]
async fn is_healthy_pending_does_not_block_on_handshake() {
    let config = HttpConfig {
        url: "http://192.0.2.1:1/unreachable".to_string(),
        headers: vec![],
    };
    // Force a generous startup timeout — if the predicate
    // regressed to going through ensure_initialized, this test
    // would hang for ~10 s. We assert it completes in well under
    // a second.
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(10),
        ..Default::default()
    };
    let client = McpClient::new_http(
        "pending-unreachable".to_string(),
        config,
        Some(&overrides),
        None,
    );
    let start = std::time::Instant::now();
    let healthy = client.is_healthy().await;
    let elapsed = start.elapsed();
    assert!(!healthy);
    // 1 s bound: the cheap path is microseconds, so this is a 10×
    // safety margin against cold-runtime / contended-CI jitter while
    // still firing well inside the 10 s blocking window that a
    // regressed predicate (back through `ensure_initialized`) would
    // sit in.
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "is_healthy must be a cheap state inspection, took {elapsed:?}"
    );
}

#[test]
fn make_client_info_pins_protocol_version() {
    assert_eq!(
        McpClient::make_client_info("test-srv").protocol_version,
        rmcp::model::ProtocolVersion::V_2025_11_25
    );
}

// -- GrokClientHandler --------------------------------------
//
// The handler's notification routing is the only behavior worth
// unit-testing here; `get_info` is a literal `info.clone()` and
// doesn't merit a test. `NotificationContext` is non-trivial to
// construct outside of an rmcp `RunningService`, so we exercise
// the routing through the `emit` helper that the trait methods
// call. If the trait wiring (one-line `async move { self.emit(...) }`)
// ever regresses, the integration tests against a real MCP
// server will catch it.

#[tokio::test]
async fn client_handler_routes_tools_changed() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<McpClientEvent>();
    let handler = GrokClientHandler {
        info: McpClient::make_client_info("test"),
        server_name: "test".to_string(),
        notify_tx: Arc::new(parking_lot::Mutex::new(Some(tx))),
    };
    handler.emit(McpClientEvent::ToolsChanged {
        server: handler.server_name.clone(),
    });
    let ev = rx.recv().await.expect("event arrived");
    match ev {
        McpClientEvent::ToolsChanged { server } => assert_eq!(server, "test"),
        other => panic!("expected ToolsChanged, got {other:?}"),
    }
}

/// Contract: when `notify_tx` is `None` (subagent snapshot,
/// no dispatcher), `emit` is a no-op and the trait methods
/// must not panic.
#[tokio::test]
async fn client_handler_no_dispatcher_is_silent() {
    let handler = GrokClientHandler {
        info: McpClient::make_client_info("test"),
        server_name: "test".to_string(),
        notify_tx: Arc::new(parking_lot::Mutex::new(None)),
    };
    handler.emit(McpClientEvent::ToolsChanged {
        server: "test".to_string(),
    });
    // No assertion needed — reaching this line means no panic.
}

/// Contract: get_info returns a clone of the stored ClientInfo.
#[tokio::test]
async fn client_handler_get_info_round_trips() {
    let info = McpClient::make_client_info("test-srv");
    let handler = GrokClientHandler {
        info: info.clone(),
        server_name: "test-srv".to_string(),
        notify_tx: Arc::new(parking_lot::Mutex::new(None)),
    };
    let got = handler.get_info();
    // ClientInfo doesn't derive PartialEq; check the visible
    // fields the constructor sets.
    assert_eq!(got.client_info.name, info.client_info.name);
    assert_eq!(got.client_info.version, info.client_info.version);
}

// A sender wired *after* the handler is constructed must still
// reach the live rmcp service loop. This test exercises the
// post-construction wiring path: build a handler from a client
// whose slot is `None`, then install a sender via
// `client.set_event_tx` and verify the handler picks it up (the
// handler holds a clone of the same shared Arc slot).
#[tokio::test]
async fn client_handler_observes_post_handshake_set_event_tx() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<McpClientEvent>();
    // McpClient::stub initializes notify_tx as `Arc<Mutex<None>>`.
    let client = Arc::new(McpClient::stub("test"));

    // Build the handler BEFORE wiring the sender — emulates
    // the production flow where `make_client_handler` is called
    // during `try_handshake` and the dispatcher is wired
    // separately.
    let handler = client.make_client_handler();

    // Confirm the slot is `None` at handler-construction time.
    assert!(handler.notify_tx.lock().is_none());

    // Now wire the sender on the client. Because the handler
    // holds a CLONE OF THE SAME ARC, this mutation is observed
    // by the handler's next `emit`.
    client.set_event_tx(Some(tx));

    handler.emit(McpClientEvent::ToolsChanged {
        server: "test".to_string(),
    });
    let ev = rx.recv().await.expect("event arrived");
    match ev {
        McpClientEvent::ToolsChanged { server } => assert_eq!(server, "test"),
        other => panic!("expected ToolsChanged, got {other:?}"),
    }
}

// Mirrors the post-construction wiring on the `ensure_initialized`
// emit path: even though `Ready` / `HandshakeFailed` fire from
// inside `try_handshake`, the slot is read at emit time through the
// SAME shared Arc, so wiring `set_event_tx` BEFORE the handshake is
// sufficient to capture these events.
#[tokio::test]
async fn event_tx_clone_observes_set_event_tx() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<McpClientEvent>();
    let client = McpClient::stub("test");
    assert!(client.event_tx_clone().is_none());
    client.set_event_tx(Some(tx));
    assert!(client.event_tx_clone().is_some());
    client.set_event_tx(None);
    assert!(client.event_tx_clone().is_none());
}

// An `ensure_initialized`-emitted `Ready` event must NOT be
// conflated with a restart. This unit test exercises the event
// level; the wire-level mapping ("Ready → reason=initialized, NOT
// restart_succeeded") is covered by host integration tests.
#[test]
fn config_added_kind_carries_correct_server_name() {
    let ev = McpClientEvent::ConfigAdded {
        server: "srv".to_string(),
    };
    assert_eq!(ev.server_name(), Some("srv"));
}

#[test]
fn apply_stdio_env_session_id_cannot_be_shadowed() {
    let mut cmd = Command::new("true");
    let env = vec![acp::EnvVariable::new("GROK_SESSION_ID", "spoofed")];
    apply_stdio_env(&mut cmd, &env, Some("sess-real"));

    let value = cmd
        .as_std()
        .get_envs()
        .find(|(k, _)| *k == "GROK_SESSION_ID")
        .and_then(|(_, v)| v)
        .map(|v| v.to_string_lossy().into_owned());
    assert_eq!(value.as_deref(), Some("sess-real"));
}
