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