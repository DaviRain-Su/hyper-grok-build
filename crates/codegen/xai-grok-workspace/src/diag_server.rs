//! In-guest diagnostics HTTP server (`/ready`, `/statusz`, `/logs`) for the
//! standalone workspace-server.
//!
//! The surface is reachable by any process inside the user's own sandbox
//! (loopback-only TCP, or a 0600 Unix socket) and is never exposed through
//! the sandbox port mapping. `/logs` returns the raw daemon log: treat its
//! output as sensitive and keep the log stream free of secrets.

use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, process};

use anyhow::anyhow;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

/// Default Unix socket path (next to the log/pid files).
#[cfg(unix)]
pub const DEFAULT_DIAG_SOCKET_PATH: &str = "/tmp/workspace-server.sock";

/// Default loopback TCP port for Windows guests.
pub const DEFAULT_DIAG_PORT: u16 = 6016;

/// Grep-able daemon-log marker for a diagnostics bind failure.
pub const DIAG_BIND_FAILED_MARKER: &str = "diagnostics server bind failed";

/// Process exit code for a fatal diagnostics bind failure in `--daemonize` mode.
pub const EXIT_DIAG_BIND_FAILED: i32 = 5;

/// Default `/logs` tail size when `tail_bytes` is not given.
pub const DEFAULT_LOG_TAIL_BYTES: u64 = 64 * 1024;

/// Hard cap on a `/logs` response; larger `tail_bytes` values are clamped.
pub const MAX_LOG_TAIL_BYTES: u64 = 256 * 1024;

/// Hub connection state as reported on `/ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagState {
    Starting,
    Connected,
    Disconnected,
    Failed,
}

/// `/ready` `error_class` when [`DiagState::Failed`] (`hub_auth` / `hub_connect` / `unknown`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    HubAuth,
    HubConnect,
    Unknown,
}

/// Soft cap on `/ready` `error_detail` so guest-local messages stay short.
const MAX_ERROR_DETAIL_BYTES: usize = 256;

/// Response body for `/ready`. The field set is a frozen contract with the
/// sandbox readiness gate: never rename or remove fields; additions are
/// backward-compatible.
#[derive(Debug, Serialize)]
struct ReadyBody {
    /// Serialized as an explicit `null` (never omitted) for nonce-less
    /// launches.
    launch_id: Option<String>,
    state: DiagState,
    pid: u32,
    connected_at: Option<u64>,
    state_changed_at: u64,
    version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_class: Option<ErrorClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_detail: Option<String>,
}

/// Response body for `/statusz`: the `/ready` fields plus debug extras.
#[derive(Debug, Serialize)]
struct StatuszBody {
    #[serde(flatten)]
    ready: ReadyBody,
    os: &'static str,
}

#[derive(Debug)]
struct Inner {
    state: DiagState,
    connected_at: Option<u64>,
    state_changed_at: u64,
    shutting_down: bool,
    error_class: Option<ErrorClass>,
    error_detail: Option<String>,
}

impl Inner {
    fn is_failed(&self) -> bool {
        matches!(self.state, DiagState::Failed)
    }
}

/// Cloneable handle publishing hub lifecycle transitions to the server.
#[derive(Debug, Clone)]
pub struct DiagHandle {
    launch_id: Option<String>,
    inner: Arc<Mutex<Inner>>,
}

impl DiagHandle {
    /// `launch_id` is the caller-minted per-spawn nonce, echoed verbatim on
    /// `/ready` (`null` for nonce-less local launches).
    pub fn new(launch_id: Option<String>) -> Self {
        Self {
            launch_id,
            inner: Arc::new(Mutex::new(Inner {
                state: DiagState::Starting,
                connected_at: None,
                state_changed_at: now_ms(),
                shutting_down: false,
                error_class: None,
                error_detail: None,
            })),
        }
    }

    /// Initial hello completed, or a reconnect's serve replay settled.
    /// No-op after [`Self::set_shutting_down`] or [`Self::set_failed`].
    pub fn set_connected(&self) {
        let mut inner = self.lock();
        if inner.shutting_down || inner.is_failed() {
            return;
        }
        inner.state = DiagState::Connected;
        let now = now_ms();
        inner.connected_at.get_or_insert(now);
        inner.state_changed_at = now;
    }

    /// Server socket dropped. No-op after [`Self::set_failed`].
    pub fn set_disconnected(&self) {
        let mut inner = self.lock();
        if inner.is_failed() {
            return;
        }
        inner.state = DiagState::Disconnected;
        inner.state_changed_at = now_ms();
    }

    /// Latch disconnected for process shutdown; later `set_connected` no-ops.
    /// No-op after [`Self::set_failed`].
    pub fn set_shutting_down(&self) {
        let mut inner = self.lock();
        if inner.is_failed() {
            return;
        }
        inner.shutting_down = true;
        inner.state = DiagState::Disconnected;
        inner.state_changed_at = now_ms();
    }

    /// Terminal connect failure on `/ready`. Sticky; callers dwell before exit.
    pub fn set_failed(&self, error_class: ErrorClass, error_detail: impl Into<String>) {
        let mut inner = self.lock();
        inner.state = DiagState::Failed;
        inner.error_class = Some(error_class);
        inner.error_detail = Some(truncate_error_detail(error_detail.into()));
        inner.state_changed_at = now_ms();
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ready_body(&self) -> ReadyBody {
        let inner = self.lock();
        let failed = inner.is_failed();
        ReadyBody {
            launch_id: self.launch_id.clone(),
            state: inner.state,
            pid: process::id(),
            connected_at: inner.connected_at,
            state_changed_at: inner.state_changed_at,
            version: xai_grok_version::VERSION,
            error_class: failed.then_some(inner.error_class).flatten(),
            error_detail: if failed {
                inner.error_detail.clone()
            } else {
                None
            },
        }
    }

    fn statusz_body(&self) -> StatuszBody {
        StatuszBody {
            ready: self.ready_body(),
            os: env::consts::OS,
        }
    }
}

fn truncate_error_detail(detail: String) -> String {
    if detail.len() <= MAX_ERROR_DETAIL_BYTES {
        return detail;
    }
    let mut end = MAX_ERROR_DETAIL_BYTES;
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail[..end].to_owned()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Where the diagnostics server listens: a Unix socket on Linux, loopback TCP
/// on Windows. Both variants compile everywhere so the TCP path is testable
/// on Linux.
#[derive(Debug, Clone)]
pub enum DiagListener {
    #[cfg(unix)]
    Unix(PathBuf),
    Tcp(u16),
}

/// Shared request state: the lifecycle handle plus the daemon log path
/// (`None` when logs go to a terminal instead of a file — `/logs` is 404).
#[derive(Debug, Clone)]
struct DiagContext {
    handle: DiagHandle,
    log_file: Option<Arc<PathBuf>>,
}

#[derive(Debug, Deserialize)]
struct LogsQuery {
    tail_bytes: Option<u64>,
}

async fn logs(State(ctx): State<DiagContext>, Query(query): Query<LogsQuery>) -> Response {
    let Some(path) = ctx.log_file else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let tail = query
        .tail_bytes
        .unwrap_or(DEFAULT_LOG_TAIL_BYTES)
        .min(MAX_LOG_TAIL_BYTES);
    match tokio::task::spawn_blocking(move || tail_file(&path, tail)).await {
        Ok(Ok(bytes)) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            String::from_utf8_lossy(&bytes).into_owned(),
        )
            .into_response(),
        Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => StatusCode::NOT_FOUND.into_response(),
        // Generic body: an io::Error would echo the log-file path to clients.
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to read log tail");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to read log").into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "log tail task failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to read log").into_response()
        }
    }
}

/// Read at most the last `max` bytes of `path`.
fn tail_file(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut buf = Vec::new();
    // `take` bounds the read even if the file grows underneath us.
    file.take(max).read_to_end(&mut buf)?;
    Ok(buf)
}

fn router(ctx: DiagContext) -> Router {
    Router::new()
        .route(
            "/ready",
            get(|State(ctx): State<DiagContext>| async move {
                let body = ctx.handle.ready_body();
                // Non-2xx for "not ready" so naive HTTP probes agree with
                // consumers that parse `state`. The body is served either way.
                let status = if body.state == DiagState::Connected {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                (status, axum::Json(body))
            }),
        )
        .route(
            "/statusz",
            get(|State(ctx): State<DiagContext>| async move { axum::Json(ctx.handle.statusz_body()) }),
        )
        .route("/logs", get(logs))
        .with_state(ctx)
}

/// Bind the listener and spawn the server task. Binding happens before this
/// returns, so a bind failure surfaces synchronously. `log_file` is the
/// daemon log served by `/logs` (`None` ⇒ `/logs` is 404).
pub async fn serve(
    listener: DiagListener,
    handle: DiagHandle,
    log_file: Option<PathBuf>,
) -> anyhow::Result<BoundDiag> {
    let ctx = DiagContext {
        handle,
        log_file: log_file.map(Arc::new),
    };
    match listener {
        #[cfg(unix)]
        DiagListener::Unix(path) => {
            let _ = fs::remove_file(&path);
            let listener =
                UnixListener::bind(&path).map_err(|e| anyhow!("bind {}: {e}", path.display()))?;
            use std::os::unix::fs::PermissionsExt as _;
            if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to restrict diagnostics socket permissions"
                );
            }
            let task = tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, router(ctx)).await {
                    tracing::warn!(error = %e, "diagnostics server exited");
                }
            });
            Ok(BoundDiag {
                addr: format!("unix:{}", path.display()),
                port: None,
                task,
            })
        }
        DiagListener::Tcp(port) => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
                .await
                .map_err(|e| anyhow!("bind 127.0.0.1:{port}: {e}"))?;
            let local = listener.local_addr()?;
            let task = tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, router(ctx)).await {
                    tracing::warn!(error = %e, "diagnostics server exited");
                }
            });
            Ok(BoundDiag {
                addr: format!("http://{local}"),
                port: Some(local.port()),
                task,
            })
        }
    }
}

/// A successfully bound diagnostics server.
#[derive(Debug)]
pub struct BoundDiag {
    /// Human-readable bound address for the startup log line.
    pub addr: String,
    /// Bound TCP port (`None` for Unix sockets).
    pub port: Option<u16>,
    /// The serve task; held by the production launcher for the process lifetime.
    pub task: JoinHandle<()>,
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    async fn get_json(port: u16, path: &str) -> (u16, Value) {
        let response = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
            .await
            .expect("request");
        let status = response.status().as_u16();
        let body = response.text().await.expect("body");
        (status, serde_json::from_str(&body).expect("json body"))
    }

    #[tokio::test]
    async fn ready_response_contract_is_frozen() {
        let handle = DiagHandle::new(Some("nonce-1".to_owned()));
        let bound = serve(DiagListener::Tcp(0), handle, None)
            .await
            .expect("bind");
        let (status, body) = get_json(bound.port.expect("tcp port"), "/ready").await;

        assert_eq!(status, 503, "not yet connected must not probe as ready");
        let obj = body.as_object().expect("object");
        for key in [
            "launch_id",
            "state",
            "pid",
            "connected_at",
            "state_changed_at",
            "version",
        ] {
            assert!(obj.contains_key(key), "missing frozen key {key}");
        }
        assert_eq!(body["launch_id"], "nonce-1");
        assert_eq!(body["state"], "starting");
        assert_eq!(body["connected_at"], Value::Null);
    }

    #[tokio::test]
    async fn state_follows_hub_lifecycle_and_freezes_connected_at() {
        let handle = DiagHandle::new(None);
        let bound = serve(DiagListener::Tcp(0), handle.clone(), None)
            .await
            .expect("bind");
        let port = bound.port.expect("tcp port");

        handle.set_connected();
        let (status, connected) = get_json(port, "/ready").await;
        assert_eq!(status, 200);
        assert_eq!(connected["state"], "connected");
        assert!(connected["connected_at"].is_u64());
        assert_eq!(connected["launch_id"], Value::Null);

        handle.set_disconnected();
        let (status, disconnected) = get_json(port, "/ready").await;
        assert_eq!(status, 503);
        assert_eq!(disconnected["state"], "disconnected");
        assert_eq!(
            disconnected["connected_at"], connected["connected_at"],
            "connected_at is frozen at first connect and echoed on disconnect"
        );

        handle.set_connected();
        let (status, reconnected) = get_json(port, "/ready").await;
        assert_eq!(status, 200);
        assert_eq!(reconnected["state"], "connected");
        assert_eq!(
            reconnected["connected_at"], connected["connected_at"],
            "reconnect must not re-mint connected_at"
        );
    }

    #[tokio::test]
    async fn shutting_down_latches_disconnected_across_reconnects() {
        let handle = DiagHandle::new(None);
        let bound = serve(DiagListener::Tcp(0), handle.clone(), None)
            .await
            .expect("bind");
        let port = bound.port.expect("tcp port");

        handle.set_connected();
        handle.set_shutting_down();
        // A reconnect settling during the shutdown drain must not republish
        // `connected`.
        handle.set_connected();

        let (status, body) = get_json(port, "/ready").await;
        assert_eq!(status, 503);
        assert_eq!(body["state"], "disconnected");
    }

    #[tokio::test]
    async fn ready_reports_failed_with_error_fields() {
        let handle = DiagHandle::new(Some("nonce-fail".to_owned()));
        let bound = serve(DiagListener::Tcp(0), handle.clone(), None)
            .await
            .expect("bind");
        let port = bound.port.expect("tcp port");

        handle.set_failed(ErrorClass::HubAuth, "handshake auth failed: HTTP 401");
        let (status, body) = get_json(port, "/ready").await;

        assert_eq!(status, 503, "failed is not ready");
        assert_eq!(body["launch_id"], "nonce-fail");
        assert_eq!(body["state"], "failed");
        assert_eq!(body["error_class"], "hub_auth");
        assert_eq!(body["error_detail"], "handshake auth failed: HTTP 401");
        assert!(body["state_changed_at"].is_u64());
        assert!(body["pid"].is_u64());
        assert!(body["version"].is_string());
        let starting = DiagHandle::new(None);
        let bound2 = serve(DiagListener::Tcp(0), starting, None)
            .await
            .expect("bind");
        let (_, start_body) = get_json(bound2.port.expect("tcp port"), "/ready").await;
        assert_eq!(start_body["state"], "starting");
        assert!(
            start_body.get("error_class").is_none(),
            "error_class must be omitted unless failed"
        );
        assert!(
            start_body.get("error_detail").is_none(),
            "error_detail must be omitted unless failed"
        );
    }

    #[tokio::test]
    async fn ready_failed_hub_connect_and_unknown_classes() {
        let handle = DiagHandle::new(None);
        let bound = serve(DiagListener::Tcp(0), handle.clone(), None)
            .await
            .expect("bind");
        let port = bound.port.expect("tcp port");

        handle.set_failed(ErrorClass::HubConnect, "network error: connection refused");
        let (status, body) = get_json(port, "/ready").await;
        assert_eq!(status, 503);
        assert_eq!(body["state"], "failed");
        assert_eq!(body["error_class"], "hub_connect");
        assert_eq!(body["error_detail"], "network error: connection refused");

        handle.set_failed(ErrorClass::Unknown, "something else");
        let (status, body) = get_json(port, "/ready").await;
        assert_eq!(status, 503);
        assert_eq!(body["state"], "failed");
        assert_eq!(body["error_class"], "unknown");
        assert_eq!(body["error_detail"], "something else");
    }

    #[tokio::test]
    async fn failed_is_sticky_against_later_lifecycle_transitions() {
        let handle = DiagHandle::new(None);
        let bound = serve(DiagListener::Tcp(0), handle.clone(), None)
            .await
            .expect("bind");
        let port = bound.port.expect("tcp port");

        handle.set_failed(ErrorClass::HubAuth, "handshake auth failed: HTTP 401");
        handle.set_connected();
        handle.set_disconnected();
        handle.set_shutting_down();

        let (status, body) = get_json(port, "/ready").await;
        assert_eq!(status, 503);
        assert_eq!(body["state"], "failed");
        assert_eq!(body["error_class"], "hub_auth");
        assert_eq!(body["error_detail"], "handshake auth failed: HTTP 401");
    }

    #[test]
    fn error_detail_is_truncated_to_cap() {
        let handle = DiagHandle::new(None);
        let long = "x".repeat(MAX_ERROR_DETAIL_BYTES + 64);
        handle.set_failed(ErrorClass::Unknown, long);
        let body = handle.ready_body();
        let detail = body.error_detail.expect("detail");
        assert_eq!(detail.len(), MAX_ERROR_DETAIL_BYTES);
    }

    #[test]
    fn error_detail_truncation_respects_utf8_char_boundary() {
        let mut long = "a".repeat(MAX_ERROR_DETAIL_BYTES - 1);
        long.push('é');
        assert_eq!(long.len(), MAX_ERROR_DETAIL_BYTES + 1);

        let handle = DiagHandle::new(None);
        handle.set_failed(ErrorClass::Unknown, long);
        let detail = handle.ready_body().error_detail.expect("detail");
        assert!(
            detail.len() <= MAX_ERROR_DETAIL_BYTES,
            "truncated length {}",
            detail.len()
        );
        assert!(
            detail.is_char_boundary(detail.len()),
            "must not split a multi-byte char"
        );
        assert!(detail.ends_with('a') || detail.ends_with('é'));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_serves_ready_and_rebinds_over_stale_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("ws.sock");

        drop(std::os::unix::net::UnixListener::bind(&sock).expect("stale bind"));

        let handle = DiagHandle::new(Some("nonce-uds".to_owned()));
        let _bound = serve(DiagListener::Unix(sock.clone()), handle.clone(), None)
            .await
            .expect("bind over stale socket");
        handle.set_connected();

        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(&sock)
            .expect("socket meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "socket must be owner-only");

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut stream = tokio::net::UnixStream::connect(&sock)
            .await
            .expect("connect");
        stream
            .write_all(b"GET /ready HTTP/1.1\r\nHost: ws\r\nConnection: close\r\n\r\n")
            .await
            .expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
        let body = response.split("\r\n\r\n").nth(1).expect("body");
        let json_start = body.find('{').expect("json start");
        let json_end = body.rfind('}').expect("json end");
        let parsed: Value = serde_json::from_str(&body[json_start..=json_end]).expect("json");
        assert_eq!(parsed["launch_id"], "nonce-uds");
        assert_eq!(parsed["state"], "connected");
    }

    #[tokio::test]
    async fn tcp_bind_conflict_surfaces_as_error() {
        let first = serve(DiagListener::Tcp(0), DiagHandle::new(None), None)
            .await
            .expect("first bind");
        let port = first.port.expect("tcp port");
        let err = serve(DiagListener::Tcp(port), DiagHandle::new(None), None).await;
        assert!(err.is_err(), "second bind on the same port must fail");
    }

    async fn get_text(port: u16, path: &str) -> (u16, Option<String>, String) {
        let response = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
            .await
            .expect("request");
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|v| v.to_str().expect("content-type").to_owned());
        (status, content_type, response.text().await.expect("body"))
    }

    async fn serve_with_log(log_file: Option<PathBuf>) -> u16 {
        let bound = serve(DiagListener::Tcp(0), DiagHandle::new(None), log_file)
            .await
            .expect("bind");
        bound.port.expect("tcp port")
    }

    #[tokio::test]
    async fn logs_tails_requested_bytes_as_plain_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("ws.log");
        fs::write(&log, "0123456789").expect("write log");
        let port = serve_with_log(Some(log)).await;

        let (status, content_type, body) = get_text(port, "/logs?tail_bytes=4").await;
        assert_eq!(status, 200);
        assert_eq!(content_type.as_deref(), Some("text/plain; charset=utf-8"));
        assert_eq!(body, "6789");

        // A tail larger than the file returns the whole file.
        let (status, _, body) = get_text(port, "/logs?tail_bytes=1000").await;
        assert_eq!(status, 200);
        assert_eq!(body, "0123456789");
    }

    #[tokio::test]
    async fn logs_default_and_hard_cap_bound_the_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("ws.log");
        // Larger than the hard cap; ends with a marker to prove we got the tail.
        let mut content = vec![b'a'; (MAX_LOG_TAIL_BYTES + 4096) as usize];
        content.extend_from_slice(b"END-MARKER");
        fs::write(&log, &content).expect("write log");
        let port = serve_with_log(Some(log)).await;

        let (status, _, body) = get_text(port, "/logs").await;
        assert_eq!(status, 200);
        assert_eq!(body.len() as u64, DEFAULT_LOG_TAIL_BYTES);
        assert!(body.ends_with("END-MARKER"));

        let (status, _, body) = get_text(port, "/logs?tail_bytes=999999999").await;
        assert_eq!(status, 200);
        assert_eq!(body.len() as u64, MAX_LOG_TAIL_BYTES, "hard cap applies");
        assert!(body.ends_with("END-MARKER"));
    }

    #[tokio::test]
    async fn logs_tail_is_lossy_utf8() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("ws.log");
        // 'é' is 0xC3 0xA9; a 1-byte tail cuts the sequence mid-char.
        fs::write(&log, "aé").expect("write log");
        let port = serve_with_log(Some(log)).await;

        let (status, _, body) = get_text(port, "/logs?tail_bytes=1").await;
        assert_eq!(status, 200);
        assert_eq!(body, "\u{FFFD}", "a torn UTF-8 boundary must be lossy");
    }

    #[tokio::test]
    async fn logs_without_log_file_is_404() {
        let port = serve_with_log(None).await;
        let (status, _, _) = get_text(port, "/logs").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn logs_missing_log_file_is_404() {
        let dir = tempfile::tempdir().expect("tempdir");
        let port = serve_with_log(Some(dir.path().join("never-created.log"))).await;
        let (status, _, _) = get_text(port, "/logs").await;
        assert_eq!(status, 404);
    }
}
