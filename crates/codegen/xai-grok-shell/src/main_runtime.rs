//! Process-wide handle to the main (multi-thread) tokio runtime, recorded at
//! startup so synchronous call sites that lack a runtime context can still
//! drive async work on the main runtime — where the shared `reqwest` client
//! and its I/O live.
//!
//! Why this matters for auth refresh: the Kimi/Codex token refresh holds
//! `auth.json.lock` for the duration of its network call. The blocking
//! entry points used to run that refresh on a *side-thread* current-thread
//! runtime. A `reqwest` client that was first warmed on the main runtime
//! does not honor a `tokio::time::timeout` driven by a *different* runtime's
//! timer, so against a stalled peer (e.g. a fake-ip proxy that completes the
//! TCP handshake then never responds) the refresh — and the lock it holds —
//! wedged indefinitely, blocking every subsequent launch on the 45s lock
//! timeout and making the TUI appear permanently stuck at startup.
//!
//! Running the refresh on the main runtime (via [`Handle::block_on`] from the
//! non-runtime caller thread, which is the intended, safe use) keeps the
//! future on the same reactor as the shared client, so `tokio::time::timeout`
//! fires and the lock is released promptly.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Handle;

static MAIN_HANDLE: OnceLock<Handle> = OnceLock::new();

/// Record the main runtime handle. Called once at startup from `async_main`,
/// which runs on the main runtime, so `Handle::current()` is valid. A later
/// call is intentionally ignored: auth resolvers must stay pinned to the
/// process runtime that owns the shared HTTP clients.
pub fn set_main_runtime_handle(handle: &Handle) {
    let _ = MAIN_HANDLE.set(handle.clone());
}

/// The main runtime handle, if [`set_main_runtime_handle`] has been called.
pub fn main_runtime_handle() -> Option<Handle> {
    MAIN_HANDLE.get().cloned()
}

/// Drive `future` on the process main runtime from a plain OS thread.
///
/// The private current-thread runtime is an early-init/test fallback only.
/// Callers must invoke this from outside a Tokio runtime; Kimi/Codex use it
/// inside their dedicated refresh side threads after a current-thread ACP
/// worker determines that it cannot block itself.
pub(crate) fn block_on_main_or_new_current_thread<F>(
    main: Option<Handle>,
    future: F,
) -> Option<F::Output>
where
    F: Future,
{
    if let Some(main) = main {
        return Some(main.block_on(future));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    Some(runtime.block_on(future))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the fake-ip/stalled-proxy wedge: the shared HTTP client
    /// is first exercised on the multi-thread main runtime, then a plain side
    /// thread drives a stalled request through that same runtime handle. The
    /// timeout must fire promptly rather than depending on a private runtime's
    /// unrelated reactor.
    #[test]
    fn side_thread_uses_main_runtime_for_stalled_http_timeout() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();

        let (base_url, server) = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let app = axum::Router::new()
                .route(
                    "/warm",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                )
                .route(
                    "/stall",
                    axum::routing::get(|| async {
                        std::future::pending::<()>().await;
                        axum::http::StatusCode::OK
                    }),
                );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            (base_url, server)
        });

        runtime.block_on(async {
            crate::http::shared_client()
                .get(format!("{base_url}/warm"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
        });

        let stalled_url = format!("{base_url}/stall");
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = block_on_main_or_new_current_thread(Some(handle), async move {
                tokio::time::timeout(
                    std::time::Duration::from_millis(150),
                    crate::http::shared_client().get(stalled_url).send(),
                )
                .await
                .is_err()
            });
            let _ = result_tx.send(result);
        });

        let timed_out = match result_rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(result) => result,
            Err(error) => {
                server.abort();
                runtime.shutdown_background();
                panic!("stalled request did not respect the main-runtime timeout: {error}");
            }
        };
        worker.join().unwrap();
        server.abort();
        assert_eq!(timed_out, Some(true));
    }
}
