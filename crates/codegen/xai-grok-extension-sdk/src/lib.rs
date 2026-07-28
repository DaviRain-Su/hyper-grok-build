//! # Hyper Extension SDK (Rust-first)
//!
//! Safe helpers for writing Hyper WASM guests without hand-rolling
//! `hyper_host` imports and `ptr`/`len` calls.
//!
//! ## Quick start
//!
//! ```ignore
//! use xai_grok_extension_sdk::prelude::*;
//!
//! extension_boilerplate!();
//!
//! #[no_mangle]
//! pub extern "C" fn hyper_ext_on_pre_tool_use() -> i32 {
//!     if input_contains("rm -rf") {
//!         deny("blocked rm -rf")
//!     } else {
//!         allow()
//!     }
//! }
//! ```
//!
//! Build with `cargo build --release --target wasm32-unknown-unknown`.
//! See `xai-grok-extension-runtime/examples/rust-guest-template`.

#![allow(clippy::missing_safety_doc)]

pub mod host;
pub mod prelude;

/// Must match host [`CORE_ABI_VERSION`](xai_grok_extension_api is host-side).
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

/// Current tool name from the host (pre_tool / invoke).
pub fn tool_name() -> String {
    host::read_tool_name()
}

/// Current tool input / args JSON from the host.
pub fn tool_input_json() -> String {
    String::from_utf8_lossy(&host::read_input()).into_owned()
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

/// Emit the minimal required exports: abi_version, session_start, session_end.
///
/// Expand in the guest crate root:
/// ```ignore
/// xai_grok_extension_sdk::extension_boilerplate!();
/// ```
///
/// Custom lifecycle handlers without dropping the macro:
/// ```ignore
/// fn on_start() -> i32 {
///     // warm caches, etc.
///     0
/// }
/// xai_grok_extension_sdk::extension_boilerplate! {
///     session_start: on_start,
/// }
/// // or: session_start: || { 0 }, session_end: my_end
/// ```
#[macro_export]
macro_rules! extension_boilerplate {
    () => {
        $crate::extension_boilerplate!(session_start: || 0i32, session_end: || 0i32);
    };
    (session_start: $start:expr) => {
        $crate::extension_boilerplate!(session_start: $start, session_end: || 0i32);
    };
    (session_end: $end:expr) => {
        $crate::extension_boilerplate!(session_start: || 0i32, session_end: $end);
    };
    (session_start: $start:expr, session_end: $end:expr $(,)?) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_abi_version() -> i32 {
            $crate::CORE_ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_session_start() -> i32 {
            ($start)()
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn hyper_ext_on_session_end() -> i32 {
            ($end)()
        }
    };
}
