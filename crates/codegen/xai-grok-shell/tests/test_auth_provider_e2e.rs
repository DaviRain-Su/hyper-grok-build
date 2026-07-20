//! End-to-end test for `[auth_provider.<name>]` per-model credential helpers.
//!
//! Runs the built grok binary headless against the mock inference server with
//! a config-defined BYOK model whose bearer comes from a mock auth binary (a
//! script the test writes to disk). The harness's `XAI_API_KEY` stands in for
//! the session-tier credential.
//!
//! `#[ignore]` (needs a built binary). The CI lifecycle lanes run it against
//! the release artifact via `GROK_BINARY`; run locally (auto-builds the pager):
//! ```bash
//! cargo test -p xai-grok-shell --test test_auth_provider_e2e -- --ignored
//! ```
//!
//! Unix-only: the mock helpers are `sh` scripts run via `sh -c`.
#![cfg(unix)]

use xai_grok_test_support::*;

#[tokio::test]
#[ignore]
async fn provider_backed_model_sends_minted_token_on_the_wire() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");
    let workdir = git_workdir();
    let home = tempfile::TempDir::new().unwrap();

    let grok_home = home.path().join(".grok");
    std::fs::create_dir_all(&grok_home).expect("create .grok home");

    let counter = grok_home.join("mint-count");
    let helper = grok_home.join("mock-auth.sh");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\necho run >> {}\nprintf 'gateway-tok-1'\n",
            counter.display()
        ),
    )
    .expect("write mock auth binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    std::fs::write(
        grok_home.join("config.toml"),
        format!(
            r#"[auth_provider.gateway]
command = "{helper}"
token_ttl_secs = 3600

[model.proxied-gateway]
model = "mock-gateway-model"
base_url = "{base}"
context_window = 200000
auth_provider = "gateway"
"#,
            helper = helper.display(),
            // Already ends in `/v1`.
            base = server.url(),
        ),
    )
    .expect("write config.toml");

    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args([
        "-p",
        "say hi",
        "--yolo",
        "--model",
        "proxied-gateway",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ])
    .arg("--cwd")
    .arg(workdir.path())
    .current_dir(workdir.path())
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
    xai_grok_test_support::env::test_env_cmd_tokio(&mut cmd, &server.url(), home.path());
    // Don't attach to a developer's ambient leader; spawn fresh against the mock.
    cmd.env_remove("GROK_LEADER_SOCKET");

    let result = run_headless_with_cmd(cmd).await;
    assert_headless_success(&result, "auth provider e2e", Some(&server));

    let runs = std::fs::read_to_string(&counter)
        .expect("helper must have run")
        .lines()
        .count();
    assert_eq!(runs, 1, "one turn mints exactly once");

    let requests = server.requests();
    // The mock server plays both the first-party and the provider role on
    // one host, so the leak assertions are scoped to the inference path.
    let chat = requests
        .iter()
        .find(|e| e.method == "POST" && e.path.contains("chat/completions"))
        .unwrap_or_else(|| {
            panic!(
                "no POST /v1/chat/completions request logged; requests:\n{}",
                server.request_log_summary()
            )
        });
    assert_eq!(
        chat.authorization.as_deref(),
        Some("Bearer gateway-tok-1"),
        "the inference request must carry the minted provider token; requests:\n{}",
        server.request_log_summary()
    );
    // `test-key-for-ci` is the harness's XAI_API_KEY; it stands in for the
    // session credential, which resolves below the provider arm.
    assert!(
        !requests.iter().any(|e| {
            e.path.contains("chat/completions")
                && e.authorization
                    .as_deref()
                    .is_some_and(|a| a.contains("test-key-for-ci"))
        }),
        "the session credential must never reach the provider-backed endpoint; requests:\n{}",
        server.request_log_summary()
    );
}

/// A model that references an undefined `[auth_provider.<name>]` must fail
/// closed end to end: the request goes out without a bearer (which the mock
/// rejects), and the session credential is never substituted onto the wire.
#[tokio::test]
#[ignore]
async fn undefined_provider_fails_closed_and_never_leaks_session_key() {
    // Reject any bearer: nothing legitimate can satisfy this, since the model
    // references a provider that is never defined.
    let server = MockInferenceServer::start_with_required_auth(
        vec![MockModelEntry::new("mock-gateway-model")],
        "never-issued-token",
    )
    .await
    .expect("start mock server");
    let workdir = git_workdir();
    let home = tempfile::TempDir::new().unwrap();

    let grok_home = home.path().join(".grok");
    std::fs::create_dir_all(&grok_home).expect("create .grok home");

    // Model references `gateway`, but no `[auth_provider.gateway]` table exists.
    std::fs::write(
        grok_home.join("config.toml"),
        format!(
            r#"[model.proxied-gateway]
model = "mock-gateway-model"
base_url = "{base}"
context_window = 200000
auth_provider = "gateway"
"#,
            base = server.url(),
        ),
    )
    .expect("write config.toml");

    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args([
        "-p",
        "say hi",
        "--yolo",
        "--model",
        "proxied-gateway",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ])
    .arg("--cwd")
    .arg(workdir.path())
    .current_dir(workdir.path())
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
    xai_grok_test_support::env::test_env_cmd_tokio(&mut cmd, &server.url(), home.path());
    cmd.env_remove("GROK_LEADER_SOCKET");

    // The turn is expected to fail (the mock 401s the unauthenticated request);
    // we assert on the wire, not the exit code.
    let _ = run_headless_with_cmd(cmd).await;

    let requests = server.requests();
    // Non-vacuity: the model was actually exercised.
    assert!(
        requests
            .iter()
            .any(|e| e.method == "POST" && e.path.contains("chat/completions")),
        "no chat request attempted; requests:\n{}",
        server.request_log_summary()
    );
    assert!(
        !requests.iter().any(|e| {
            e.path.contains("chat/completions")
                && e.authorization
                    .as_deref()
                    .is_some_and(|a| a.contains("test-key-for-ci"))
        }),
        "an undefined provider must fail closed, never sending the session \
         credential; requests:\n{}",
        server.request_log_summary()
    );
}

/// The documented `args` + JSON-output shape, end to end: a provider with
/// `args = [...]` runs the helper directly (no shell) and parses a JSON
/// `{access_token, expires_in}` payload, and the minted token reaches the wire.
#[tokio::test]
#[ignore]
async fn provider_with_args_and_json_output_sends_minted_token() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");
    let workdir = git_workdir();
    let home = tempfile::TempDir::new().unwrap();

    let grok_home = home.path().join(".grok");
    std::fs::create_dir_all(&grok_home).expect("create .grok home");

    // The helper records the args it was invoked with (proving direct exec, no
    // shell) and prints a JSON token payload.
    let seen_args = grok_home.join("seen-args");
    let helper = grok_home.join("mock-auth-json.sh");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf '%s' \"$*\" > {}\n\
             printf '{{\"access_token\":\"gateway-tok-json\",\"expires_in\":3600}}'\n",
            seen_args.display()
        ),
    )
    .expect("write mock auth binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    std::fs::write(
        grok_home.join("config.toml"),
        format!(
            r#"[auth_provider.gateway]
command = "{helper}"
args = ["--profile", "corp"]
token_ttl_secs = 3600
timeout_secs = 10

[model.proxied-gateway]
model = "mock-gateway-model"
base_url = "{base}"
context_window = 200000
auth_provider = "gateway"
"#,
            helper = helper.display(),
            base = server.url(),
        ),
    )
    .expect("write config.toml");

    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args([
        "-p",
        "say hi",
        "--yolo",
        "--model",
        "proxied-gateway",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ])
    .arg("--cwd")
    .arg(workdir.path())
    .current_dir(workdir.path())
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
    xai_grok_test_support::env::test_env_cmd_tokio(&mut cmd, &server.url(), home.path());
    cmd.env_remove("GROK_LEADER_SOCKET");

    let result = run_headless_with_cmd(cmd).await;
    assert_headless_success(&result, "auth provider args/json e2e", Some(&server));

    let args = std::fs::read_to_string(&seen_args).expect("helper must have run");
    assert_eq!(
        args, "--profile corp",
        "args must be passed directly to the helper with no shell"
    );

    let requests = server.requests();
    let chat = requests
        .iter()
        .find(|e| e.method == "POST" && e.path.contains("chat/completions"))
        .unwrap_or_else(|| {
            panic!(
                "no POST /v1/chat/completions request logged; requests:\n{}",
                server.request_log_summary()
            )
        });
    assert_eq!(
        chat.authorization.as_deref(),
        Some("Bearer gateway-tok-json"),
        "the JSON access_token must reach the wire; requests:\n{}",
        server.request_log_summary()
    );
}
