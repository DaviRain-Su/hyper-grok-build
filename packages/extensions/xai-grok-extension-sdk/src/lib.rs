//! # Hyper Extension SDK (Rust-first)
//!
//! Safe helpers for writing Hyper WASM guests without hand-rolling
//! `hyper_host` imports and `ptr`/`len` calls.
//!
//! ## Authoring style
//!
//! Prefer the [`hyper_plugin`] procedural macro. Hook and tool implementations
//! remain ordinary named Rust functions, so rust-analyzer can navigate to them
//! and compiler diagnostics point at the author code instead of a token DSL.
//!
//! ## Quick start
//!
//! ```ignore
//! use xai_grok_extension_sdk::prelude::*;
//!
//! #[hyper_plugin]
//! mod plugin {
//!     use super::*;
//!
//!     #[hyper_hook(pre_tool_use)]
//!     fn guard_destructive_commands() -> i32 {
//!         if input_contains("rm -rf") {
//!             deny("blocked rm -rf")
//!         } else {
//!             allow()
//!         }
//!     }
//!
//!     #[hyper_hook(before_agent_start)]
//!     fn add_agent_guidance() -> i32 {
//!         inject_context("prefer dedicated tools over recursive shell search");
//!         allow()
//!     }
//!
//!     #[hyper_tool(
//!         description = "Echo args JSON",
//!         schema = r#"{"type":"object","properties":{"msg":{"type":"string"}}}"#
//!     )]
//!     fn echo(args: &str) -> i32 {
//!         tool_result(args);
//!         allow()
//!     }
//! }
//! ```
//!
//! The older `hyper_extension!` and component `macro_rules!` exports remain
//! available for source compatibility, but new plugins should use attributes.
//!
//! Build with `cargo build --release --target wasm32-unknown-unknown`.
//! See `xai-grok-extension-runtime/examples/rust-guest-template`.

#![allow(clippy::missing_safety_doc)]

pub mod host;
#[macro_use]
mod macros;
pub mod prelude;

pub use xai_grok_extension_macros::{hyper_command, hyper_hook, hyper_plugin, hyper_tool};

/// Must match host CORE_ABI_VERSION.
pub const CORE_ABI_VERSION: i32 = 1;

/// Gate decision: allow the action (tool / stop).
#[inline]
pub fn allow() -> i32 {
    0
}

/// Gate decision: deny / block, with a human-readable reason for the host UI.
#[inline]
pub fn deny(reason: &str) -> i32 {
    host::set_gate_reason(reason);
    1
}

/// Inject a system-reminder style note (before_agent / before_model).
#[inline]
pub fn inject_context(text: &str) {
    host::set_inject_context(text);
}

/// Append a system-extension fragment.
#[inline]
pub fn append_system(text: &str) {
    host::set_append_system(text);
}

/// Whether the current tool input JSON contains `needle` (UTF-8 substring).
pub fn input_contains(needle: &str) -> bool {
    host::bytes_contain(&host::read_input(), needle.as_bytes())
}

/// Current tool name from the host (pre_tool / post_tool / invoke).
pub fn tool_name() -> String {
    host::read_tool_name()
}

/// Current tool input / args JSON from the host.
pub fn tool_input_json() -> String {
    String::from_utf8_lossy(&host::read_input()).into_owned()
}

/// Whether the tool call succeeded (meaningful during `post_tool_use`).
#[inline]
pub fn tool_success() -> bool {
    host::tool_success()
}

/// Capped tool result / error preview from the host (`post_tool_use`).
/// Empty when the host did not supply a preview.
pub fn tool_result_preview() -> String {
    host::read_tool_result_preview()
}

/// Current user prompt (before_agent); may be empty on before_model rounds.
pub fn prompt() -> String {
    String::from_utf8_lossy(&host::read_prompt()).into_owned()
}

/// Tool index when describing tools (`hyper_ext_describe_tool`).
#[inline]
pub fn tool_index() -> i32 {
    host::tool_index()
}

/// Whether the stop gate already continued this turn.
#[inline]
pub fn stop_hook_active() -> bool {
    host::stop_hook_active()
}

/// Advertise tool metadata while handling `hyper_ext_describe_tool`.
pub fn describe_tool(name: &str, description: &str, json_schema: &str) {
    host::set_tool_name(name);
    host::set_tool_description(description);
    host::set_tool_schema(json_schema);
}

/// Return a tool result while handling `hyper_ext_invoke_tool`.
#[inline]
pub fn tool_result(text: &str) {
    host::set_tool_result(text);
}

/// Standard empty JSON object schema for tools with no parameters.
pub const EMPTY_OBJECT_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;

/// Guest log levels for [`log`] / [`log_info`] (match host `GuestLogLevel`).
pub const LOG_DEBUG: i32 = 0;
pub const LOG_INFO: i32 = 1;
pub const LOG_WARN: i32 = 2;
pub const LOG_ERROR: i32 = 3;

/// Structured guest → host log (appears under tracing target `wasm_extension`).
#[inline]
pub fn log(level: i32, msg: &str) {
    host::log(level, msg);
}

#[inline]
pub fn log_debug(msg: &str) {
    log(LOG_DEBUG, msg);
}

#[inline]
pub fn log_info(msg: &str) {
    log(LOG_INFO, msg);
}

#[inline]
pub fn log_warn(msg: &str) {
    log(LOG_WARN, msg);
}

#[inline]
pub fn log_error(msg: &str) {
    log(LOG_ERROR, msg);
}

/// Host-provided plugin data directory path string (`~/.grok/plugin-data/<id>/`
/// when known). **Metadata only** — the bootstrap guest has no filesystem
/// import; do not assume you can open files at this path from Wasm.
/// Empty when the host did not wire a path (e.g. bare runtime tests).
pub fn plugin_data_dir() -> String {
    host::read_plugin_data_dir()
}
