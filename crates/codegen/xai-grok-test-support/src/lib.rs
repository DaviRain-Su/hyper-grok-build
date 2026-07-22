#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
//! Shared test utilities for grok-build crates: mock inference server, SSE
//! generators, ACP stdio client, headless runner, env sandbox.
//!
//! Provides:
//! - [`MockInferenceServer`] — Mock /v1/chat/completions + /v1/responses with request logging
//! - [`GrokStdioClient`] — ACP client that drives `grok agent stdio` as a subprocess
//! - [`RawStdioClient`] — raw-wire ACP driver for bytes the typed client can't
//!   produce (Foundation `\/` methods, string UUID ids)
//! - [`leader::LeaderStdioClient`] — ACP client that drives `grok agent --leader stdio` (unix)
//! - [`run_headless`] — Run `grok -p` against the mock server and capture output
//! - [`git_workdir`] — Create a temp directory with git repo (forces libgit2 init)
//! - [`grok_binary`] — Resolve the grok binary path (GROK_BINARY env or cargo_bin)
//! - [`spawn_counting_server`] — Connection-counting HTTP/1.1 server for wire/pooling tests
//! - [`uds_proxy::UdsProxy`] — Frame-aware fault-injection proxy for leader IPC sockets (unix)
/// Multiply a harness timeout by `GROK_TEST_TIMEOUT_SCALE` (positive integer,
/// default 1). CI lanes on shared runner pools raise it so pool load slows
/// tests instead of failing them (see the Grok Build merge CI workflow).
pub fn scaled(base: std::time::Duration) -> std::time::Duration {
    let scale = std::env::var("GROK_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(1);
    base * scale
}
pub mod acp_client;
pub mod counting_server;
pub mod env;
pub mod headless;
mod inference_override;
#[cfg(unix)]
pub mod leader;
pub mod mock_server;
mod process;
pub mod scripted;
pub mod sse;
#[cfg(unix)]
pub mod uds_proxy;
pub use acp_client::{GrokStdioClient, RawStdioClient};
pub use counting_server::spawn_counting_server;
pub use env::{EnvGuard, git_workdir, grok_binary};
pub use headless::{
    HeadlessResult, assert_headless_success, assert_no_crashes, run_headless,
    run_headless_with_cmd, run_headless_with_env, stderr_tail,
};
pub use inference_override::{InferenceEndpoint, InferenceExpectation, InferenceRequestMatcher};
pub use mock_server::{
    MockInferenceServer, MockModelEntry, ScriptedResponse, SseEvent, StorageUpload,
};
