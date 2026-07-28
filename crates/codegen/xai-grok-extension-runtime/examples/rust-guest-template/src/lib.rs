//! Hyper WASM guest — written with **xai-grok-extension-sdk** (recommended path).
//!
//! ```bash
//! rustup target add wasm32-unknown-unknown
//! cargo build --release --target wasm32-unknown-unknown
//! cp target/wasm32-unknown-unknown/release/hyper_ext_rust_guest_template.wasm \
//!    ./extension.wasm
//! ```

#![allow(unused)]

use xai_grok_extension_sdk::prelude::*;

// abi_version + session_start/end
xai_grok_extension_sdk::extension_boilerplate!();

/// Requires capability `pre_tool_gate`.
#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_on_pre_tool_use() -> i32 {
    if input_contains("rm -rf") {
        deny("rust-guest-template: blocked rm -rf in tool input")
    } else {
        allow()
    }
}

/// Requires capability `before_agent_inject`.
#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_on_before_agent_start() -> i32 {
    inject_context("Rust SDK guest: prefer dedicated tools over recursive shell search.");
    allow()
}

/// Requires capability `before_model_inject`.
#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_on_before_model() -> i32 {
    allow()
}

#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_on_stop() -> i32 {
    allow()
}

#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_on_pre_compact() -> i32 {
    allow()
}

// ── register_tool demo (capability `register_tool`) ───────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_tool_count() -> i32 {
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_describe_tool() -> i32 {
    if tool_index() != 0 {
        return 1;
    }
    describe_tool(
        "echo",
        "Echo tool_input JSON back (SDK register_tool demo)",
        r#"{"type":"object","properties":{"msg":{"type":"string"}}}"#,
    );
    allow()
}

#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_invoke_tool() -> i32 {
    let args = tool_input_json();
    tool_result(&args);
    allow()
}
