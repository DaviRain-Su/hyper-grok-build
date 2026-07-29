use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::notification::ToolNotification;
use async_lsp::lsp_types::{self, Diagnostic};

use super::client::LspClient;
use super::config::LspServerConfig;
use super::dispatch::LspBackendAdapter;
use super::manager::{LspManager, drain_lsp_diagnostics};
use super::restart::restart_monitor;
use super::{LspBackend, LspError, LspOperation, LspToolInput, file_uri};

const MOCK_LSP_SERVER: &str = r#"
import json, sys

def read_message():
    headers = {}
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        if line.strip() == '':
            break
        if ':' in line:
            key, value = line.split(':', 1)
            headers[key.strip()] = value.strip()
    length = int(headers.get('Content-Length', 0))
    if length == 0:
        return None
    body = sys.stdin.read(length)
    return json.loads(body)

def send_message(msg):
    body = json.dumps(msg)
    header = f"Content-Length: {len(body)}\r\n\r\n"
    sys.stdout.write(header)
    sys.stdout.write(body)
    sys.stdout.flush()

def send_diagnostics(uri):
    send_message({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": uri,
            "diagnostics": [
                {
                    "range": {
                        "start": {"line": 0, "character": 5},
                        "end": {"line": 0, "character": 10}
                    },
                    "severity": 1,
                    "source": "mock",
                    "message": "mock error: undeclared variable"
                },
                {
                    "range": {
                        "start": {"line": 2, "character": 0},
                        "end": {"line": 2, "character": 15}
                    },
                    "severity": 2,
                    "source": "mock",
                    "message": "mock warning: unused import"
                }
            ]
        }
    })

while True:
    msg = read_message()
    if msg is None:
        break

    method = msg.get("method")
    msg_id = msg.get("id")

    if method == "initialize":
        send_message({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "definitionProvider": True,
                    "referencesProvider": True
                }
            }
        })
    elif method == "initialized":
        pass
    elif method == "textDocument/didOpen":
        uri = msg["params"]["textDocument"]["uri"]
        send_diagnostics(uri)
    elif method == "textDocument/didChange":
        uri = msg["params"]["textDocument"]["uri"]
        send_diagnostics(uri)
    elif method == "textDocument/didSave":
        pass
    elif method == "textDocument/definition":
        uri = msg["params"]["textDocument"]["uri"]
        send_message({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "uri": uri,
                "range": {
                    "start": {"line": 10, "character": 0},
                    "end": {"line": 10, "character": 20}
                }
            }]
        })
    elif method == "textDocument/references":
        uri = msg["params"]["textDocument"]["uri"]
        send_message({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [
                {
                    "uri": uri,
                    "range": {
                        "start": {"line": 5, "character": 0},
                        "end": {"line": 5, "character": 10}
                    }
                },
                {
                    "uri": uri,
                    "range": {
                        "start": {"line": 15, "character": 3},
                        "end": {"line": 15, "character": 13}
                    }
                }
            ]
        })
    elif method == "shutdown":
        send_message({"jsonrpc": "2.0", "id": msg_id, "result": None})
    elif method == "exit":
        break
"#;

fn write_mock_server() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("mock_lsp.py");
    std::fs::write(&script_path, MOCK_LSP_SERVER).unwrap();
    (dir, script_path)
}

fn write_delayed_diagnostics_server() -> (tempfile::TempDir, std::path::PathBuf) {
    const DELAYED_SERVER: &str = r#"
import json, sys, time

def read_message():
    headers = {}
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        if line.strip() == '':
            break
        if ':' in line:
            key, value = line.split(':', 1)
            headers[key.strip()] = value.strip()
    length = int(headers.get('Content-Length', 0))
    if length == 0:
        return None
    return json.loads(sys.stdin.read(length))

def send_message(msg):
    body = json.dumps(msg)
    sys.stdout.write(f"Content-Length: {len(body)}\r\n\r\n{body}")
    sys.stdout.flush()

while True:
    msg = read_message()
    if msg is None:
        break
    method = msg.get("method")
    msg_id = msg.get("id")
    if method == "initialize":
        send_message({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {"capabilities": {"textDocumentSync": 1}}
        })
    elif method == "initialized":
        pass
    elif method == "textDocument/didOpen":
        time.sleep(1.0)
        send_message({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": msg["params"]["textDocument"]["uri"],
                "diagnostics": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 5}
                    },
                    "severity": 1,
                    "source": "delayed",
                    "message": "delayed diagnostic after restart"
                }]
            }
        })
    elif method == "shutdown":
        send_message({"jsonrpc": "2.0", "id": msg_id, "result": None})
    elif method == "exit":
        break
"#;
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("delayed_lsp.py");
    std::fs::write(&script_path, DELAYED_SERVER).unwrap();
    (dir, script_path)
}

fn write_init_failure_server() -> (tempfile::TempDir, std::path::PathBuf) {
    write_init_failure_server_n_times(3)
}

fn write_slow_init_server(delay_ms: u64) -> (tempfile::TempDir, std::path::PathBuf) {
    let script = format!(
        r#"import json, sys, time

def read_message():
    headers = {{}}
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        if line.strip() == '':
            break
        if ':' in line:
            key, value = line.split(':', 1)
            headers[key.strip()] = value.strip()
    length = int(headers.get('Content-Length', 0))
    if length == 0:
        return None
    return json.loads(sys.stdin.read(length))

def send_message(msg):
    body = json.dumps(msg)
    sys.stdout.write(f"Content-Length: {{len(body)}}\r\n\r\n{{body}}")
    sys.stdout.flush()

while True:
    msg = read_message()
    if msg is None:
        break
    method = msg.get("method")
    msg_id = msg.get("id")
    if method == "initialize":
        time.sleep({delay_ms} / 1000.0)
        send_message({{
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {{"capabilities": {{"textDocumentSync": 1, "definitionProvider": True}}}}
        }})
    elif method == "initialized":
        pass
    elif method == "textDocument/definition":
        uri = msg["params"]["textDocument"]["uri"]
        send_message({{
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{{
                "uri": uri,
                "range": {{
                    "start": {{"line": 1, "character": 0}},
                    "end": {{"line": 1, "character": 5}}
                }}
            }}]
        }})
    elif method == "shutdown":
        send_message({{"jsonrpc": "2.0", "id": msg_id, "result": None}})
    elif method == "exit":
        break
"#
    );
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("slow_init_lsp.py");
    std::fs::write(&script_path, script).unwrap();
    (dir, script_path)
}

fn write_init_failure_server_n_times(
    failures_before_success: usize,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let init_error_payload = format!(
        "{{\"code\": -32603, \"message\": \"init failed on purpose after {} failures\"}}",
        failures_before_success
    );
    let init_error_payload = init_error_payload.replace('"', r#"\""#);
    let script = format!(
        r#"import json, os, sys

FAILURES_BEFORE_SUCCESS = {failures_before_success}
COUNTER_FILE = os.environ["INIT_FAILURE_COUNTER_FILE"]
INIT_ERROR = json.loads("{init_error_payload}")

def read_message():
    headers = {{}}
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        if line.strip() == '':
            break
        if ':' in line:
            key, value = line.split(':', 1)
            headers[key.strip()] = value.strip()
    length = int(headers.get('Content-Length', 0))
    if length == 0:
        return None
    return json.loads(sys.stdin.read(length))

def send_message(msg):
    body = json.dumps(msg)
    sys.stdout.write(f"Content-Length: {{len(body)}}\r\n\r\n{{body}}")
    sys.stdout.flush()

def increment_attempts():
    attempts = 0
    if os.path.exists(COUNTER_FILE):
        with open(COUNTER_FILE, "r", encoding="utf-8") as f:
            content = f.read().strip()
            if content:
                attempts = int(content)
    attempts += 1
    with open(COUNTER_FILE, "w", encoding="utf-8") as f:
        f.write(str(attempts))
    return attempts

while True:
    msg = read_message()
    if msg is None:
        break
    method = msg.get("method")
    msg_id = msg.get("id")
    if method == "initialize":
        attempts = increment_attempts()
        if attempts <= FAILURES_BEFORE_SUCCESS:
            send_message({{"jsonrpc": "2.0", "id": msg_id, "error": INIT_ERROR}})
            break
        send_message({{
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {{"capabilities": {{"textDocumentSync": 1}}}}
        }})
    elif method == "initialized":
        pass
    elif method == "shutdown":
        send_message({{"jsonrpc": "2.0", "id": msg_id, "result": None}})
    elif method == "exit":
        break
"#
    );
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("init_fail_lsp.py");
    std::fs::write(&script_path, script).unwrap();
    (dir, script_path)
}

fn mock_server_config(script_path: &Path) -> LspServerConfig {
    let mut ext_map = HashMap::new();
    ext_map.insert(".ts".to_string(), "typescript".to_string());
    LspServerConfig {
        command: "python3".to_string(),
        args: vec!["-u".to_string(), script_path.to_string_lossy().into_owned()],
        extensions: ext_map,
        startup_timeout: Some(10_000),
        ..Default::default()
    }
}

async fn start_mock_client() -> (tempfile::TempDir, tempfile::TempDir, LspClient) {
    let (script_dir, script_path) = write_mock_server();
    let config = mock_server_config(&script_path);
    let workspace = tempfile::tempdir().unwrap();
    let notify = Arc::new(tokio::sync::Notify::new());
    let client = LspClient::start("mock".to_string(), 1, config, workspace.path(), notify)
        .await
        .expect("mock LSP handshake failed");
    (script_dir, workspace, client)
}

async fn poll_diagnostics(client: &LspClient, path: &Path, expected: usize) -> Vec<Diagnostic> {
    for _ in 0..100 {
        let diags = client.get_diagnostics(path);
        if diags.len() >= expected {
            return diags;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    client.get_diagnostics(path)
}

/// Creates a single-server LspManager with the mock TS server, already initialized.
async fn single_server_manager(script_path: &Path, workspace: &tempfile::TempDir) -> LspManager {
    let mut servers = BTreeMap::new();
    servers.insert("mock-ts".to_string(), mock_server_config(script_path));

    let mut mgr = LspManager::new(
        servers,
        workspace.path().to_path_buf(),
        false,
        crate::notification::ToolNotificationHandle::noop(),
    );
    mgr.ensure_initialized().await;
    mgr
}

/// Wait until the LSP server has published diagnostics for `path`.
/// Polls the shared diagnostics map (not drain) with 10ms sleeps.
async fn wait_for_server(mgr: &LspManager, path: &Path, timeout_ms: u64) {
    let uri = file_uri(path).unwrap().to_string();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        for client in mgr.clients.values() {
            let map = client.diagnostics.read().unwrap();
            if map.contains_key(&uri) {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("wait_for_server timed out after {timeout_ms}ms for {uri}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_did_open_publishes_diagnostics() {
    let (_dir, workspace, mut client) = start_mock_client().await;

    let test_file = workspace.path().join("test.ts");
    std::fs::write(&test_file, "const x = 1;\n").unwrap();
    client.notify_file_change(&test_file, "const x = 1;\n", "typescript");

    let diags = poll_diagnostics(&client, &test_file, 2).await;
    assert_eq!(diags.len(), 2, "expected 2 diagnostics, got {:?}", diags);

    assert_eq!(diags[0].message, "mock error: undeclared variable");
    assert_eq!(
        diags[0].severity,
        Some(lsp_types::DiagnosticSeverity::ERROR)
    );

    assert_eq!(diags[1].message, "mock warning: unused import");
    assert_eq!(
        diags[1].severity,
        Some(lsp_types::DiagnosticSeverity::WARNING)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_goto_definition() {
    let (_dir, workspace, mut client) = start_mock_client().await;
    let test_file = workspace.path().join("test.ts");
    std::fs::write(&test_file, "const x = 1;\n").unwrap();
    client.notify_file_change(&test_file, "const x = 1;\n", "typescript");

    let locations = client.goto_definition(&test_file, 0, 5).await.unwrap();
    assert_eq!(
        locations.len(),
        1,
        "expected 1 location, got {:?}",
        locations
    );
    assert_eq!(locations[0].range.start.line, 10);
    assert_eq!(locations[0].range.start.character, 0);
    assert_eq!(locations[0].range.end.line, 10);
    assert_eq!(locations[0].range.end.character, 20);
}

/// Exercises the production API: init -> notify (fire-and-forget) -> drain -> shutdown.
#[tokio::test(flavor = "current_thread")]
async fn e2e_lsp_manager_full_lifecycle() {
    let (_dir, script_path) = write_mock_server();
    let workspace = tempfile::tempdir().unwrap();

    let mut mgr = single_server_manager(&script_path, &workspace).await;
    assert!(mgr.is_initialized());
    mgr.ensure_initialized().await; // idempotent

    // Fire-and-forget notification.
    let test_file = workspace.path().join("app.ts");
    let content = "let y = 2;\n";
    std::fs::write(&test_file, content).unwrap();
    mgr.notify_file_changed(&test_file, content);
    assert!(mgr.has_pending_diagnostics());

    let pending_before = mgr.pending_count();
    mgr.notify_file_changed(Path::new("readme.md"), "");
    assert_eq!(mgr.pending_count(), pending_before);

    wait_for_server(&mgr, &test_file, 2000).await;

    let mgr = tokio::sync::Mutex::new(mgr);
    let timeout = std::time::Duration::from_secs(2);
    let summary = drain_lsp_diagnostics(&mgr, timeout)
        .await
        .expect("expected diagnostics");
    assert!(
        summary.text.contains("mock error: undeclared variable"),
        "summary: {}",
        summary.text
    );
    assert!(
        summary.text.contains("mock warning: unused import"),
        "summary: {}",
        summary.text
    );
    assert!(summary.text.starts_with("<lsp-diagnostics>"));
    assert!(summary.text.ends_with("</lsp-diagnostics>"));
    assert_eq!(summary.file_count, 1);
    assert_eq!(summary.diagnostic_count, 2);

    assert!(!mgr.lock().await.has_pending_diagnostics());
    assert!(drain_lsp_diagnostics(&mgr, timeout).await.is_none());

    mgr.lock().await.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_did_change_updates_diagnostics() {
    let (_dir, workspace, mut client) = start_mock_client().await;
    let test_file = workspace.path().join("test.ts");
    std::fs::write(&test_file, "const x = 1;\n").unwrap();

    // First open.
    client.notify_file_change(&test_file, "const x = 1;\n", "typescript");
    let diags = poll_diagnostics(&client, &test_file, 2).await;
    assert_eq!(diags.len(), 2);

    // Second call triggers didChange, not didOpen.
    client.notify_file_change(&test_file, "const x = 2;\n", "typescript");
    let diags = poll_diagnostics(&client, &test_file, 2).await;
    assert_eq!(diags.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_spawn_failure_is_graceful() {
    let config = LspServerConfig {
        command: "/nonexistent/binary/that/does/not/exist".to_string(),
        ..Default::default()
    };
    let workspace = tempfile::tempdir().unwrap();
    let notify = Arc::new(tokio::sync::Notify::new());

    let result = LspClient::start("bad".to_string(), 1, config, workspace.path(), notify).await;
    assert!(result.is_err());
    assert!(
        matches!(result.unwrap_err(), LspError::SpawnFailed(_)),
        "expected SpawnFailed"
    );
}

/// 2 good + 1 bad server. Verifies bad one is skipped, routing works,
/// and combined diagnostics summary includes both files.
#[tokio::test(flavor = "current_thread")]
async fn e2e_multi_server_routing() {
    let (_dir, script_path) = write_mock_server();

    let mut ts_ext = HashMap::new();
    ts_ext.insert(".ts".to_string(), "typescript".to_string());
    let mut py_ext = HashMap::new();
    py_ext.insert(".py".to_string(), "python".to_string());

    let mut servers = BTreeMap::new();
    servers.insert(
        "mock-ts".to_string(),
        LspServerConfig {
            command: "python3".to_string(),
            args: vec!["-u".to_string(), script_path.to_string_lossy().into_owned()],
            extensions: ts_ext,
            startup_timeout: Some(10_000),
            ..Default::default()
        },
    );
    servers.insert(
        "mock-py".to_string(),
        LspServerConfig {
            command: "python3".to_string(),
            args: vec!["-u".to_string(), script_path.to_string_lossy().into_owned()],
            extensions: py_ext,
            startup_timeout: Some(10_000),
            ..Default::default()
        },
    );
    servers.insert(
        "bad".to_string(),
        LspServerConfig {
            command: "/nonexistent".to_string(),
            startup_timeout: Some(2_000),
            ..Default::default()
        },
    );

    let workspace = tempfile::tempdir().unwrap();
    let mut mgr = LspManager::new(
        servers,
        workspace.path().to_path_buf(),
        false,
        crate::notification::ToolNotificationHandle::noop(),
    );

    mgr.ensure_initialized().await;
    // Bad server skipped, 2 good ones remain.
    assert_eq!(mgr.clients.len(), 2);

    // .ts and .py route to their respective servers.
    let ts_file = workspace.path().join("app.ts");
    let ts_content = "let x = 1;\n";
    std::fs::write(&ts_file, ts_content).unwrap();
    mgr.notify_file_changed(&ts_file, ts_content);

    let py_file = workspace.path().join("main.py");
    let py_content = "x = 1\n";
    std::fs::write(&py_file, py_content).unwrap();
    mgr.notify_file_changed(&py_file, py_content);

    // .go has no configured server — doesn't add to pending.
    let go_file = workspace.path().join("main.go");
    std::fs::write(&go_file, "package main\n").unwrap();
    let pending_before = mgr.pending_count();
    mgr.notify_file_changed(&go_file, "package main\n");
    assert_eq!(
        mgr.pending_count(),
        pending_before,
        ".go should not add pending"
    );

    wait_for_server(&mgr, &ts_file, 2000).await;
    wait_for_server(&mgr, &py_file, 2000).await;

    let mgr = tokio::sync::Mutex::new(mgr);
    let summary = drain_lsp_diagnostics(&mgr, std::time::Duration::from_secs(2))
        .await
        .expect("expected combined diagnostics");
    assert!(
        summary.text.contains("app.ts"),
        "missing ts: {}",
        summary.text
    );
    assert!(
        summary.text.contains("main.py"),
        "missing py: {}",
        summary.text
    );
    assert_eq!(summary.file_count, 2);
    assert_eq!(summary.diagnostic_count, 4); // 2 per file (error + warning)

    mgr.lock().await.shutdown().await;
}

/// Requires `npx typescript-language-server` on PATH. Run with:
/// `cargo test -p xai-grok-shell e2e_real_typescript_language_server -- --ignored`
#[ignore]
#[tokio::test(flavor = "current_thread")]
async fn e2e_real_typescript_language_server() {
    let workspace = tempfile::tempdir().unwrap();

    std::fs::write(
        workspace.path().join("tsconfig.json"),
        r#"{"compilerOptions": {"strict": true}}"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("package.json"),
        r#"{"name": "test", "dependencies": {"typescript": "*"}}"#,
    )
    .unwrap();

    // Install typescript so the language server can find tsserver.
    let npm = std::process::Command::new("npm")
        .args(["install", "--silent"])
        .current_dir(workspace.path())
        .output()
        .expect("npm install failed");
    assert!(
        npm.status.success(),
        "npm install failed: {}",
        String::from_utf8_lossy(&npm.stderr)
    );

    let ts_file = workspace.path().join("test.ts");
    std::fs::write(&ts_file, "const x: number = 'hello';\n").unwrap();

    let mut ext_map = HashMap::new();
    ext_map.insert(".ts".to_string(), "typescript".to_string());

    let config = LspServerConfig {
        command: "npx".to_string(),
        args: vec![
            "--yes".to_string(),
            "typescript-language-server".to_string(),
            "--stdio".to_string(),
        ],
        extensions: ext_map,
        startup_timeout: Some(30_000),
        ..Default::default()
    };

    let notify = Arc::new(tokio::sync::Notify::new());
    let mut client = LspClient::start("tsserver".to_string(), 1, config, workspace.path(), notify)
        .await
        .expect("real TS language server failed to start");

    assert_eq!(client.server_name(), "tsserver");

    client.notify_file_change(&ts_file, "const x: number = 'hello';\n", "typescript");

    // Real servers take longer — poll for up to 10 seconds.
    let diags = {
        let mut result = vec![];
        for _ in 0..200 {
            result = client.get_diagnostics(&ts_file);
            if !result.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        result
    };

    assert!(
        !diags.is_empty(),
        "expected diagnostics from real TS server, got none"
    );
    let has_type_error = diags
        .iter()
        .any(|d| d.message.contains("not assignable") || d.message.contains("Type"));
    assert!(
        has_type_error,
        "expected type error diagnostic, got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    eprintln!("Real TS server produced {} diagnostics:", diags.len());
    for d in &diags {
        eprintln!(
            "  [{:?}] L{}: {}",
            d.severity,
            d.range.start.line + 1,
            d.message
        );
    }

    client.shutdown().await;
}

/// Simulates the host session diagnostics flow:
/// edit -> notify_file_changed -> drain_lsp_diagnostics -> inject as user message.
#[tokio::test(flavor = "current_thread")]
async fn e2e_session_diagnostics_injection_flow() {
    let (_dir, script_path) = write_mock_server();
    let workspace = tempfile::tempdir().unwrap();
    let mut mgr = single_server_manager(&script_path, &workspace).await;

    // Step 1: tool edits file -> fire-and-forget notify (returns immediately).
    let edited_file = workspace.path().join("component.ts");
    let content = "const x: number = 'wrong_type';\n";
    std::fs::write(&edited_file, content).unwrap();
    mgr.notify_file_changed(&edited_file, content);

    // Step 2: (simulated) other tools run... time passes... LSP server responds.
    wait_for_server(&mgr, &edited_file, 2000).await;

    let mgr = tokio::sync::Mutex::new(mgr);
    let summary = drain_lsp_diagnostics(&mgr, std::time::Duration::from_secs(2))
        .await
        .expect("diagnostics should be available");
    assert!(summary.text.starts_with("<lsp-diagnostics>"));
    assert!(summary.text.ends_with("</lsp-diagnostics>"));
    assert!(
        summary.text.contains("error[L1]"),
        "should have error at line 1: {}",
        summary.text
    );
    assert!(
        summary.text.contains("warn[L3]"),
        "should have warning at line 3: {}",
        summary.text
    );
    assert_eq!(summary.file_count, 1);
    assert_eq!(summary.diagnostic_count, 2);

    // Step 4: the injected user message the model sees.
    let injected = format!("<system-reminder>\n{}\n</system-reminder>", summary.text);
    assert!(injected.contains("mock error: undeclared variable"));
    assert!(injected.contains("mock warning: unused import"));

    // Step 5: .py has no server — notify is a no-op.
    {
        let mut mgr = mgr.lock().await;
        let py_file = workspace.path().join("script.py");
        std::fs::write(&py_file, "x = 1\n").unwrap();
        mgr.notify_file_changed(&py_file, "x = 1\n");
        assert!(!mgr.has_pending_diagnostics(), ".py should not add pending");
        mgr.shutdown().await;
    }
}

/// Exercises the tool dispatch path through dispatch_tool_typed:
/// goToDefinition, findReferences, missing server, missing args.
#[tokio::test(flavor = "current_thread")]
async fn e2e_session_tool_dispatch_flow() {
    use super::{LspOperation, LspToolInput};

    let (_dir, script_path) = write_mock_server();
    let workspace = tempfile::tempdir().unwrap();
    let mut mgr = single_server_manager(&script_path, &workspace).await;

    let ts_file = workspace.path().join("app.ts");
    let content = "function greet() { return 'hi'; }\n";
    std::fs::write(&ts_file, content).unwrap();
    mgr.notify_file_changed(&ts_file, content);

    // goToDefinition
    let result = mgr
        .dispatch_tool_typed(&LspToolInput {
            operation: LspOperation::GoToDefinition,
            file_path: Some(ts_file.to_string_lossy().into_owned()),
            line: Some(0),
            character: Some(9),
            query: None,
        })
        .await;
    assert!(!result.is_error, "should succeed: {}", result.text);
    assert!(
        result.text.contains(":11:1"),
        "expected line 11: {}",
        result.text
    );

    // findReferences
    let result = mgr
        .dispatch_tool_typed(&LspToolInput {
            operation: LspOperation::FindReferences,
            file_path: Some(ts_file.to_string_lossy().into_owned()),
            line: Some(0),
            character: Some(9),
            query: None,
        })
        .await;
    assert!(!result.is_error);
    assert!(result.text.contains(":6:1"), "line 6: {}", result.text);
    assert!(result.text.contains(":16:4"), "line 16: {}", result.text);

    // missing server -> error
    let rs_file = workspace.path().join("lib.rs");
    std::fs::write(&rs_file, "fn main() {}\n").unwrap();
    let result = mgr
        .dispatch_tool_typed(&LspToolInput {
            operation: LspOperation::GoToDefinition,
            file_path: Some(rs_file.to_string_lossy().into_owned()),
            line: Some(0),
            character: Some(3),
            query: None,
        })
        .await;
    assert!(result.is_error);
    assert!(
        result.text.contains("No LSP server configured"),
        "{}",
        result.text
    );

    // missing file_path for position-based operation -> error
    let result = mgr
        .dispatch_tool_typed(&LspToolInput {
            operation: LspOperation::GoToDefinition,
            file_path: None,
            line: None,
            character: None,
            query: None,
        })
        .await;
    assert!(result.is_error);
    assert!(result.text.contains("Required"), "{}", result.text);

    mgr.shutdown().await;
}

/// Verifies the tools_enabled gating logic.
#[tokio::test(flavor = "current_thread")]
async fn e2e_tools_enabled_gating() {
    let (_dir, script_path) = write_mock_server();
    let workspace = tempfile::tempdir().unwrap();

    // tools_enabled=false (default) — tools should NOT be advertised.
    let mut mgr = single_server_manager(&script_path, &workspace).await;
    assert!(!mgr.tools_enabled(), "tools disabled by default");
    mgr.shutdown().await;

    // tools_enabled=true — Arc<dyn LspBackend> would be injected into ToolBridge Resources.
    let mut servers = BTreeMap::new();
    servers.insert("mock-ts".to_string(), mock_server_config(&script_path));
    let mut mgr = LspManager {
        tools_enabled: true,
        ..LspManager::new(
            servers,
            workspace.path().to_path_buf(),
            false,
            crate::notification::ToolNotificationHandle::noop(),
        )
    };
    mgr.ensure_initialized().await;
    assert!(mgr.tools_enabled());
    mgr.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_restart_monitor_preserves_replacement_client() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, script_path) = write_mock_server();
            let workspace = tempfile::tempdir().unwrap();
            let mut mgr = single_server_manager(&script_path, &workspace).await;

            let original_lifecycle_id = mgr.clients.get("mock-ts").unwrap().lifecycle_id;
            let tracked_docs = vec![(
                "file:///tmp/replayed.ts".to_string(),
                "typescript".to_string(),
            )];
            let replacement_lifecycle_id = mgr.alloc_lifecycle_id();
            let replacement = LspClient::start(
                "mock-ts".to_string(),
                replacement_lifecycle_id,
                mock_server_config(&script_path),
                workspace.path(),
                mgr.diagnostics_ready.clone(),
            )
            .await
            .expect("replacement should start");
            mgr.clients.insert("mock-ts".to_string(), replacement);

            let lsp_manager = Arc::new(tokio::sync::Mutex::new(mgr));
            let monitor = tokio::task::spawn_local(restart_monitor(
                Arc::downgrade(&lsp_manager),
                "mock-ts".to_string(),
            ));

            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

            {
                let mut mgr = lsp_manager.lock().await;
                // Mutate in place: LspClient now implements Drop, so moving
                // fields out with `..stale` is rejected.
                let mut stale = mgr.clients.remove("mock-ts").unwrap();
                stale.lifecycle_id = original_lifecycle_id;
                stale.open_documents = tracked_docs
                    .into_iter()
                    .map(|(uri, lang)| (uri, (0, lang)))
                    .collect();
                mgr.clients.insert("mock-ts".to_string(), stale);
                let healthy_lifecycle_id = mgr.alloc_lifecycle_id();
                let healthy = LspClient::start(
                    "mock-ts".to_string(),
                    healthy_lifecycle_id,
                    mock_server_config(&script_path),
                    workspace.path(),
                    mgr.diagnostics_ready.clone(),
                )
                .await
                .expect("healthy replacement should start");
                mgr.clients.insert("mock-ts".to_string(), healthy);
            }

            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

            let current_lifecycle_id = lsp_manager
                .lock()
                .await
                .clients
                .get("mock-ts")
                .map(|c| c.lifecycle_id)
                .expect("healthy client should still be present");
            assert_ne!(current_lifecycle_id, original_lifecycle_id);

            monitor.abort();
            let _ = monitor.await;
            lsp_manager.lock().await.shutdown().await;
        })
        .await;
}

/// The monitor holds only a `Weak` to the manager, so once the sole strong
/// `Arc` drops at session teardown the next poll's upgrade fails and the task
/// must exit. A `Weak`->`Arc` regression would keep the manager (and its child
/// processes) alive and the join would time out.
#[tokio::test(flavor = "current_thread")]
async fn restart_monitor_exits_when_manager_arc_dropped() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, script_path) = write_mock_server();
            let workspace = tempfile::tempdir().unwrap();
            let mgr = single_server_manager(&script_path, &workspace).await;

            // A live client keeps the monitor polling (rather than exiting on a
            // missing client), so the only way out is a failed `Weak` upgrade.
            let lsp_manager = Arc::new(tokio::sync::Mutex::new(mgr));
            let monitor = tokio::task::spawn_local(restart_monitor(
                Arc::downgrade(&lsp_manager),
                "mock-ts".to_string(),
            ));

            drop(lsp_manager);

            tokio::time::timeout(std::time::Duration::from_secs(5), monitor)
                .await
                .expect("monitor must exit once the manager Arc is dropped")
                .expect("monitor task should not panic");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_finalize_no_longer_blocks_on_slow_lsp_startup() {
    let (_dir, script_path) = write_slow_init_server(1_500);
    let workspace = tempfile::tempdir().unwrap();

    let mut ext_map = HashMap::new();
    ext_map.insert(".ts".to_string(), "typescript".to_string());
    let server_config = LspServerConfig {
        command: "python3".to_string(),
        args: vec!["-u".to_string(), script_path.to_string_lossy().into_owned()],
        extensions: ext_map,
        startup_timeout: Some(5_000),
        ..Default::default()
    };
    let mut servers = BTreeMap::new();
    servers.insert("slow".to_string(), server_config);

    let lsp_manager = Arc::new(tokio::sync::Mutex::new(LspManager::new(
        servers,
        workspace.path().to_path_buf(),
        true,
        crate::notification::ToolNotificationHandle::noop(),
    )));
    let adapter = LspBackendAdapter::new(lsp_manager.clone());

    let start = tokio::time::Instant::now();
    adapter.ensure_started_background();
    assert!(
        start.elapsed() < std::time::Duration::from_millis(500),
        "background start trigger should return immediately"
    );
    assert!(
        !adapter.is_ready(),
        "slow startup should not be ready immediately"
    );

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !adapter.is_ready(),
        "adapter should still be starting shortly after trigger"
    );

    adapter
        .ensure_ready()
        .await
        .expect("startup should eventually succeed");
    assert!(
        adapter.is_ready(),
        "adapter should be ready after awaited startup"
    );
    lsp_manager.lock().await.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_first_dispatch_waits_for_background_startup() {
    let (_dir, script_path) = write_slow_init_server(750);
    let workspace = tempfile::tempdir().unwrap();
    let test_file = workspace.path().join("app.ts");
    std::fs::write(&test_file, "const value = 1;\n").unwrap();

    let mut ext_map = HashMap::new();
    ext_map.insert(".ts".to_string(), "typescript".to_string());
    let server_config = LspServerConfig {
        command: "python3".to_string(),
        args: vec!["-u".to_string(), script_path.to_string_lossy().into_owned()],
        extensions: ext_map,
        startup_timeout: Some(5_000),
        ..Default::default()
    };
    let mut servers = BTreeMap::new();
    servers.insert("slow".to_string(), server_config);

    let lsp_manager = Arc::new(tokio::sync::Mutex::new(LspManager::new(
        servers,
        workspace.path().to_path_buf(),
        true,
        crate::notification::ToolNotificationHandle::noop(),
    )));
    let adapter = LspBackendAdapter::new(lsp_manager.clone());
    adapter.ensure_started_background();

    let start = tokio::time::Instant::now();
    let result = adapter
        .dispatch(&LspToolInput {
            operation: LspOperation::GoToDefinition,
            file_path: Some(test_file.to_string_lossy().into_owned()),
            line: Some(0),
            character: Some(6),
            query: None,
        })
        .await;
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(500),
        "dispatch should wait for startup readiness"
    );
    assert!(
        adapter.is_ready(),
        "dispatch should leave the adapter ready"
    );
    assert!(
        !result.is_error
            || result.text.contains("Definition")
            || result.text.contains("No LSP server configured"),
        "result: {}",
        result.text
    );
    lsp_manager.lock().await.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_restart_monitor_emits_failed_on_restart_init_error() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, script_path) = write_init_failure_server();
            let counter_dir = tempfile::tempdir().unwrap();
            let counter_path = counter_dir.path().join("attempts.txt");
            let workspace = tempfile::tempdir().unwrap();
            let (handle, mut rx) = crate::notification::ToolNotificationHandle::channel();

            let mut ext_map = HashMap::new();
            ext_map.insert(".ts".to_string(), "typescript".to_string());
            let mut env = HashMap::new();
            env.insert(
                "INIT_FAILURE_COUNTER_FILE".to_string(),
                counter_path.to_string_lossy().into_owned(),
            );
            let server_config = LspServerConfig {
                command: "python3".to_string(),
                args: vec!["-u".to_string(), script_path.to_string_lossy().into_owned()],
                env,
                extensions: ext_map,
                // Generous startup window: the init-failure server responds to
                // `initialize` (and bumps the on-disk counter) essentially
                // instantly, so a large timeout adds no latency on the happy
                // path. It only removes a cold-start race — with a tight 500ms
                // window a slow python3 spawn under load is killed *before* it
                // increments the counter, so `attempts` (deterministically 3)
                // and the on-disk counter (2) diverge and the test flakes.
                startup_timeout: Some(10_000),
                restart_on_crash: Some(true),
                max_restarts: Some(3),
                ..Default::default()
            };

            let mut servers = BTreeMap::new();
            servers.insert("failing".to_string(), server_config.clone());

            let healthy_script = write_mock_server();
            let healthy_client = LspClient::start(
                "failing".to_string(),
                1,
                mock_server_config(&healthy_script.1),
                workspace.path(),
                Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .expect("healthy client should start");

            let mut mgr = LspManager::new(servers, workspace.path().to_path_buf(), false, handle);
            mgr.clients.insert("failing".to_string(), healthy_client);

            let lsp_manager = Arc::new(tokio::sync::Mutex::new(mgr));
            let monitor = tokio::task::spawn_local(restart_monitor(
                Arc::downgrade(&lsp_manager),
                "failing".to_string(),
            ));

            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            {
                let mut mgr = lsp_manager.lock().await;
                let crashed = mgr.clients.remove("failing").unwrap();
                crashed.main_loop.abort();
                mgr.clients.insert("failing".to_string(), crashed);
            }

            let mut saw_failed = false;
            // Restart backoff is 1s + 2s + 4s = 7s of mandatory sleeps before
            // the budget is exhausted; the deadline only bounds the failure
            // wait (the loop breaks as soon as the notification arrives), so
            // keep it well clear of that floor to stay robust under load.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            while tokio::time::Instant::now() < deadline {
                match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(ToolNotification::LspServerFailed(failed))) => {
                        assert_eq!(failed.server_name, "failing");
                        assert!(failed.error.contains("init failed on purpose"));
                        assert_eq!(failed.attempts, 3);
                        saw_failed = true;
                        break;
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => break,
                    Err(_) => continue,
                }
            }

            assert!(saw_failed, "expected LspServerFailed notification");
            let attempts = std::fs::read_to_string(&counter_path)
                .expect("counter file should exist")
                .trim()
                .parse::<u32>()
                .expect("counter should be an integer");
            assert_eq!(
                attempts, 3,
                "restart init should consume the full retry budget"
            );
            monitor.abort();
            let _ = monitor.await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_drain_timeout_preserves_pending_diagnostics() {
    let (_dir, script_path) = write_delayed_diagnostics_server();
    let workspace = tempfile::tempdir().unwrap();

    let mut ext_map = HashMap::new();
    ext_map.insert(".ts".to_string(), "typescript".to_string());
    let server_config = LspServerConfig {
        command: "python3".to_string(),
        args: vec!["-u".to_string(), script_path.to_string_lossy().into_owned()],
        extensions: ext_map,
        startup_timeout: Some(10_000),
        ..Default::default()
    };
    let mut servers = BTreeMap::new();
    servers.insert("delayed".to_string(), server_config);

    let mut mgr = LspManager::new(
        servers,
        workspace.path().to_path_buf(),
        false,
        crate::notification::ToolNotificationHandle::noop(),
    );
    mgr.ensure_initialized().await;

    let test_file = workspace.path().join("slow.ts");
    std::fs::write(&test_file, "const y = 1;\n").unwrap();
    mgr.notify_file_changed(&test_file, "const y = 1;\n");
    assert!(mgr.is_uri_pending(&test_file));

    let mgr = tokio::sync::Mutex::new(mgr);
    let first = drain_lsp_diagnostics(&mgr, std::time::Duration::from_millis(100)).await;
    assert!(
        first.is_none(),
        "first drain should time out before diagnostics arrive"
    );
    assert!(mgr.lock().await.is_uri_pending(&test_file));

    let second = drain_lsp_diagnostics(&mgr, std::time::Duration::from_secs(2))
        .await
        .expect("pending diagnostics should survive timeout");
    assert!(second.text.contains("delayed diagnostic after restart"));
    assert_eq!(second.file_count, 1);
    assert!(!mgr.lock().await.has_pending_diagnostics());
    mgr.lock().await.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn e2e_restart_replay_requeues_pending_diagnostics() {
    let (_dir, script_path) = write_delayed_diagnostics_server();
    let workspace = tempfile::tempdir().unwrap();

    let mut ext_map = HashMap::new();
    ext_map.insert(".ts".to_string(), "typescript".to_string());
    let server_config = LspServerConfig {
        command: "python3".to_string(),
        args: vec!["-u".to_string(), script_path.to_string_lossy().into_owned()],
        extensions: ext_map,
        startup_timeout: Some(10_000),
        ..Default::default()
    };
    let mut servers = BTreeMap::new();
    servers.insert("delayed".to_string(), server_config.clone());

    let mut mgr = LspManager::new(
        servers,
        workspace.path().to_path_buf(),
        false,
        crate::notification::ToolNotificationHandle::noop(),
    );
    mgr.ensure_initialized().await;

    let test_file = workspace.path().join("restart.ts");
    std::fs::write(&test_file, "const y = 1;\n").unwrap();
    mgr.notify_file_changed(&test_file, "const y = 1;\n");
    assert!(mgr.is_uri_pending(&test_file));

    let tracked_docs = mgr
        .clients
        .get("delayed")
        .expect("server should exist")
        .tracked_documents();
    let lifecycle_id = mgr.alloc_lifecycle_id();
    let mut restarted = LspClient::start(
        "delayed".to_string(),
        lifecycle_id,
        server_config,
        workspace.path(),
        mgr.diagnostics_ready.clone(),
    )
    .await
    .expect("restart should succeed");

    for (uri_str, lang_id) in &tracked_docs {
        let path = PathBuf::from(uri_str.strip_prefix("file://").unwrap());
        let content = std::fs::read_to_string(&path).unwrap();
        restarted.notify_file_change(&path, &content, lang_id);
        mgr.mark_path_pending_diagnostics("delayed", lifecycle_id, &path);
    }
    mgr.clients.insert("delayed".to_string(), restarted);

    let mgr = tokio::sync::Mutex::new(mgr);
    let summary = drain_lsp_diagnostics(&mgr, std::time::Duration::from_secs(3))
        .await
        .expect("replayed document should still produce diagnostics");
    assert!(summary.text.contains("delayed diagnostic after restart"));
    assert_eq!(summary.file_count, 1);

    mgr.lock().await.shutdown().await;
}

/// Polls `try_wait` until the child is reaped or the budget expires; a live
/// child (failed kill) times out and returns false.
#[cfg(unix)]
async fn std_child_died(child: &mut std::process::Child) -> bool {
    for _ in 0..50 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

/// A language-server child enrolled via a `ProcessScope` is reaped by
/// `kill_all`, proving session close reclaims LSP servers even while the owning
/// client is still alive (the wedged-actor case).
#[cfg(unix)]
#[tokio::test]
async fn scope_reaps_enrolled_language_server_child() {
    let (_script_dir, _workspace, mut client) = start_mock_client().await;

    let scope = crate::util::ProcessScope::new();
    assert!(
        client.enroll(Some(&scope)),
        "enroll on an open scope must succeed"
    );
    assert_eq!(
        scope.live_count(),
        1,
        "enroll must register the server group"
    );

    // Take the child handle to observe death; the client keeps the owning
    // Arc<ProcessGroup>, so the scope's weak stays live across kill_all.
    let mut child = client.child_process.take().expect("stdio child");
    scope.kill_all();

    // Prove kill_all killed the child WHILE the client is still alive (the
    // wedged-actor case) — dropping the client first would let its own Drop
    // killpg mask a broken kill_all. `waitid(WNOWAIT)` observes the exit
    // without reaping (signal 0 can't: it succeeds on a zombie), so the
    // SIGKILLed leader stays unreaped and its pgid reserved. nix only
    // exposes waitid on Linux; macOS falls back to the weaker
    // drop-then-reap order below (CI runs the strong branch).
    #[cfg(target_os = "linux")]
    {
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        let mut died = false;
        for _ in 0..50 {
            use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
            match waitid(
                Id::Pid(pid),
                WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG,
            ) {
                Ok(WaitStatus::StillAlive) => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Ok(_) => {
                    died = true;
                    break;
                }
                Err(e) => panic!("waitid on the server child failed: {e}"),
            }
        }
        assert!(
            died,
            "scope.kill_all must kill the enrolled server child while the client is alive"
        );
    }

    // Linux: the leader is dead but unreaped, so its pgid is still reserved —
    // the client Drop's killpg targets the zombie's group, not a reused pgid.
    // macOS: dropping before the reap keeps the same pgid-reservation safety,
    // at the cost of not isolating kill_all from the Drop killpg.
    drop(client);

    assert!(
        std_child_died(&mut child).await,
        "scope.kill_all must reap the enrolled server child"
    );
}

/// Dropping an `LspClient` without `shutdown` still reaps its server child, so a
/// session never orphans a language server when graceful teardown is skipped.
/// Probes the pid with signal 0 (ESRCH once reaped) since the client owns the
/// child handle and waits on it during `Drop`.
#[cfg(unix)]
#[tokio::test]
async fn drop_reaps_server_child_without_shutdown() {
    let (_script_dir, _workspace, client) = start_mock_client().await;

    let pid = client.child_process.as_ref().expect("stdio child").id() as i32;
    let alive = |pid: i32| nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
    assert!(alive(pid), "server child should be running before drop");

    drop(client);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while alive(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !alive(pid),
        "Drop must reap the server child even without shutdown"
    );
}
