//! End-to-end tests for the `--debug` firehose file logging.
//!
//! Runs the built grok binary against the mock inference server with a
//! caller-owned `$GROK_HOME`, then inspects `~/.grok/debug/`:
//! - the `--debug` FLAG drives the firehose end to end through the master switch:
//!   a live `agent` session launched with `--debug` writes a non-empty per-session
//!   `~/.grok/debug/<sessionId>.txt` with first-party content, and does NOT enable
//!   sampling/instrumentation. Regression for the master switch having bundled
//!   `GROK_LOG_SAMPLING`/`GROK_INSTRUMENTATION`, whose global `TargetFilterLayer`
//!   suppressed every other target and starved the firehose.
//! - `--debug` (headless) runs cleanly without crashing arg-parsing (smoke).
//! - no `--debug` writes no firehose files.
//! - a live `agent` session (explicit `GROK_DEBUG_LOG=1`) writes a per-session
//!   `~/.grok/debug/<sessionId>.txt` with real first-party content + `latest.txt`.
//! - `--debug-file <path>` writes one explicit file and bypasses per-session
//!   routing entirely (no `~/.grok/debug/` files).
//! - `GROK_LOG_FILE=<path>` writes that explicit file (back-compat single file).
//!
//! Per-session content is asserted via the live `agent`, not the headless run:
//! the agent's `run_session` future runs under the `session` span (carrying
//! `session_id`), so its first-party debug events route to `<sessionId>.txt`.
//! This is the same `init_tracing_simple("agent")` path the spawned leader uses,
//! so it covers leader capture deterministically without a flaky detached
//! process. Buffered logs from runs that DO log are not lost: the firehose
//! worker guards are flushed at process exit via `debug_log::flush()` (normal +
//! signal exit paths).
//!
//! `#[ignore]` (they need a built binary). Run locally (auto-builds the pager):
//! ```bash
//! cargo test -p xai-grok-shell --test test_debug_logging -- --ignored
//! ```

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use xai_grok_test_support::*;

/// Run an async body inside a `LocalSet` (required by ACP's `!Send` futures).
async fn with_local_set<F, Fut>(f: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    tokio::task::LocalSet::new().run_until(f()).await;
}

/// The per-session firehose directory under a pinned `$GROK_HOME`.
fn debug_dir(home: &Path) -> PathBuf {
    home.join(".grok").join("debug")
}

/// List firehose `*.txt` files under `~/.grok/debug` (excluding the `latest.txt`
/// symlink). Empty if the dir is missing.
fn firehose_txt_files(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(debug_dir(home)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".txt") && n != "latest.txt")
        })
        .collect()
}

/// Build a headless `grok -p` command with a pinned `$GROK_HOME` so the firehose
/// lands under `<home>/.grok/debug`. Firehose env knobs are cleared so the test
/// is hermetic regardless of the developer's shell.
fn debug_cmd(
    server: &MockInferenceServer,
    home: &Path,
    workdir: &Path,
    extra: &[&str],
) -> (tokio::process::Command, TestSandbox) {
    let mut sandbox = TestSandbox::builder().mock_url(server.url()).build();
    sandbox
        .set_env("HOME", home)
        .set_env("USERPROFILE", home)
        .set_env("GROK_HOME", home.join(".grok"));
    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args(["-p", "say hi", "--yolo", "--output-format", "json"])
        .args(extra)
        .arg("--cwd")
        .arg(workdir)
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    sandbox.apply_to_tokio_command(&mut cmd);
    (cmd, sandbox)
}

/// Poll up to 50×100ms for the per-session firehose at `path` to become non-empty
/// (its worker flushes asynchronously while the agent process stays alive), then
/// assert it carries first-party (`xai_grok`) content. Panics with the captured
/// stderr tail if it never fills. Shared by the live-agent tests.
async fn read_session_firehose_when_ready(path: &Path, client: &GrokStdioClient) -> String {
    let mut content = None;
    for _ in 0..50 {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            content = Some(text);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let content = content.unwrap_or_else(|| {
        panic!(
            "no non-empty per-session firehose {path:?}\nstderr:\n{}",
            stderr_tail(&client.stderr(), 800)
        )
    });
    // The firehose filter routes first-party crate logs here; assert that rather
    // than a bare non-empty check.
    assert!(
        content.contains("xai_grok"),
        "session firehose {path:?} should contain first-party logs, got {} bytes",
        content.len()
    );
    content
}

/// `--debug` (headless) runs cleanly: arg-parsing + the master switch + tracing
/// init don't crash. Per-session routing + content is proven deterministically by
/// the live `agent` tests (incl. `debug_flag_master_switch_enables_firehose`); a
/// headless `grok -p` client is near-silent, so its lazily-opened firehose may
/// legitimately stay empty here — file existence is intentionally not asserted.
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn debug_flag_enables_firehose_without_crashing() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");
    let workdir = git_workdir();
    let home = TempDir::new().expect("create temp home");

    let (cmd, sandbox) = debug_cmd(&server, home.path(), workdir.workspace(), &["--debug"]);
    let result = run_headless_in_sandbox(cmd, sandbox).await;

    assert_headless_success(&result, "grok --debug headless", Some(&server));
    assert_no_crashes(&result.stderr);
}

/// Without `--debug` (and no firehose env), no firehose files are written.
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn no_debug_flag_writes_no_debug_dir() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");
    let workdir = git_workdir();
    let home = TempDir::new().expect("create temp home");

    let (cmd, sandbox) = debug_cmd(&server, home.path(), workdir.workspace(), &[]);
    let result = run_headless_in_sandbox(cmd, sandbox).await;

    assert_headless_success(&result, "grok headless (no --debug)", Some(&server));
    assert!(
        firehose_txt_files(home.path()).is_empty(),
        "no firehose *.txt expected without --debug, found: {:?}",
        firehose_txt_files(home.path())
    );
}

/// A live `agent` session writes `~/.grok/debug/<sessionId>.txt` with real
/// first-party content, and points `latest.txt` at it. This is the same
/// `init_tracing_simple("agent")` path the spawned leader uses, so it covers
/// leader capture deterministically without a flaky detached process.
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn agent_session_writes_named_session_file() {
    with_local_set(|| async {
        let server = MockInferenceServer::start()
            .await
            .expect("start mock server");
        let workdir = git_workdir();
        let mut sandbox = TestSandbox::new();
        sandbox.set_env("GROK_DEBUG_LOG", "1");
        let grok_home = sandbox.grok_home().to_path_buf();

        let client =
            GrokStdioClient::spawn_with_sandbox(&server, workdir.workspace(), sandbox).await;
        client.initialize_with_timeout().await;
        let session_id = client
            .create_session_with_timeout(workdir.workspace())
            .await;
        // New session ids are UUID v7 (filesystem-safe), so the firehose file is
        // named verbatim `<sessionId>.txt`.
        let sid = session_id.0.to_string();
        let _ = client.prompt_with_timeout(&session_id, "say hi").await;

        let session_file = grok_home.join("debug").join(format!("{sid}.txt"));
        read_session_firehose_when_ready(&session_file, &client).await;

        // `latest.txt` is a sibling symlink pointing at the just-opened session
        // file, so `tail -f ~/.grok/debug/latest.txt` follows the live session.
        #[cfg(unix)]
        {
            let link = grok_home.join("debug").join("latest.txt");
            let target = std::fs::read_link(&link)
                .unwrap_or_else(|e| panic!("latest.txt should be a symlink ({link:?}): {e}"));
            assert_eq!(target, Path::new(&format!("{sid}.txt")));
        }
    })
    .await;
}

/// The `--debug` FLAG (not `GROK_DEBUG_LOG` directly) drives the firehose end to
/// end through the master switch. Regression: the master switch used to also set
/// `GROK_LOG_SAMPLING`/`GROK_INSTRUMENTATION`, whose `TargetFilterLayer` globally
/// suppresses every non-matching target — starving the firehose so `--debug`
/// produced no logs. Drives a real agent session with `--debug` and asserts the
/// per-session file has first-party content (would FAIL pre-fix), and that
/// sampling/instrumentation are NOT enabled by `--debug`.
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn debug_flag_master_switch_enables_firehose() {
    with_local_set(|| async {
        let server = MockInferenceServer::start()
            .await
            .expect("start mock server");
        let workdir = git_workdir();
        let sandbox = TestSandbox::new();
        let grok_home = sandbox.grok_home().to_path_buf();

        // Drive `grok --debug agent stdio`: the master switch (which runs before
        // the agent dispatch) must be what enables the firehose — NOT a direct
        // GROK_DEBUG_LOG env. The sandbox baseline excludes inherited firehose
        // toggles, so the `--debug` flag is the only thing enabling logging here.
        let client = GrokStdioClient::spawn_with_sandbox_env_and_args(
            &server,
            workdir.workspace(),
            sandbox,
            &[],
            &["--debug"],
        )
        .await;
        client.initialize_with_timeout().await;
        let session_id = client
            .create_session_with_timeout(workdir.workspace())
            .await;
        let sid = session_id.0.to_string();
        let _ = client.prompt_with_timeout(&session_id, "say hi").await;

        let session_file = grok_home.join("debug").join(format!("{sid}.txt"));
        read_session_firehose_when_ready(&session_file, &client).await;

        // Slimming guard: `--debug` must NOT enable sampling. The agent spawn
        // clears GROK_LOG_SAMPLING (hermetic), so the sampling layer stays off and
        // `~/.grok/logs/sampling.jsonl` is never written — the `--debug`
        // set-if-unset must not flip it on (the pre-fix code did, starving the
        // firehose). Instrumentation isn't checked: the harness pins
        // GROK_INSTRUMENTATION=disabled, so that assertion would be vacuous.
        let sampling = grok_home.join("logs").join("sampling.jsonl");
        let len = std::fs::metadata(&sampling).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            len, 0,
            "--debug must not enable sampling, found {len} bytes at {sampling:?}"
        );
    })
    .await;
}

/// `--debug-file <path>` writes one explicit file and bypasses per-session
/// routing entirely (no `~/.grok/debug/` files created).
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn debug_file_flag_writes_single_file_and_bypasses_routing() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");
    let workdir = git_workdir();
    let home = TempDir::new().expect("create temp home");
    let explicit = home.path().join("explicit-firehose.txt");
    let explicit_str = explicit.to_string_lossy().into_owned();

    let (cmd, sandbox) = debug_cmd(
        &server,
        home.path(),
        workdir.workspace(),
        &["--debug-file", &explicit_str],
    );
    let result = run_headless_in_sandbox(cmd, sandbox).await;

    assert_headless_success(&result, "grok --debug-file", Some(&server));
    assert_no_crashes(&result.stderr);
    assert!(
        explicit.exists(),
        "explicit --debug-file path not written: {explicit:?}\nstderr tail:\n{}",
        stderr_tail(&result.stderr, 800)
    );
    // Routing bypassed: nothing should land in the per-session debug dir.
    assert!(
        firehose_txt_files(home.path()).is_empty(),
        "--debug-file must bypass per-session routing, found: {:?}",
        firehose_txt_files(home.path())
    );
}

/// `GROK_LOG_FILE=<path>` (no `--debug`) writes that exact file (back-compat).
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn grok_log_file_explicit_path_is_written() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");
    let workdir = git_workdir();
    let home = TempDir::new().expect("create temp home");
    let custom = home.path().join("custom-log-file.log");

    let (cmd, mut sandbox) = debug_cmd(&server, home.path(), workdir.workspace(), &[]);
    sandbox.set_env("GROK_LOG_FILE", &custom);
    let result = run_headless_in_sandbox(cmd, sandbox).await;

    assert_headless_success(&result, "grok GROK_LOG_FILE=path", Some(&server));
    assert_no_crashes(&result.stderr);
    assert!(
        custom.exists(),
        "explicit GROK_LOG_FILE path not written: {custom:?}\nstderr tail:\n{}",
        stderr_tail(&result.stderr, 800)
    );
    // Single-file mode bypasses per-session routing.
    assert!(
        firehose_txt_files(home.path()).is_empty(),
        "GROK_LOG_FILE must bypass per-session routing, found: {:?}",
        firehose_txt_files(home.path())
    );
}
