//! Hyper WASM guest — written with **xai-grok-extension-sdk** (recommended path).
//!
//! Lifecycle hooks and tools are ordinary named Rust functions. The
//! `#[hyper_plugin]` procedural macro generates the stable `hyper_ext_*` ABI
//! exports around them.
//!
//! ```bash
//! rustup target add wasm32-unknown-unknown
//! cargo build --release --target wasm32-unknown-unknown
//! cp target/wasm32-unknown-unknown/release/hyper_ext_rust_guest_template.wasm \
//!    ./extension.wasm
//! ```

#![allow(unused)]

use xai_grok_extension_sdk::prelude::*;

#[hyper_plugin]
mod plugin {
    use super::*;

    // capability: pre_tool_gate
    #[hyper_hook(pre_tool_use)]
    fn guard_destructive_commands() -> i32 {
        if input_contains("rm -rf") {
            deny("rust-guest-template: blocked rm -rf in tool input")
        } else {
            allow()
        }
    }

    // observe-only (no capability): post_tool_use
    #[hyper_hook(post_tool_use)]
    fn observe_tool_result() -> i32 {
        if tool_success() {
            log_info(&format!("post_tool ok: {}", tool_name()));
        } else {
            log_warn(&format!(
                "post_tool fail: {} preview={}",
                tool_name(),
                tool_result_preview()
            ));
        }
        0
    }

    // capability: before_agent_inject
    #[hyper_hook(before_agent_start)]
    fn add_agent_guidance() -> i32 {
        inject_context("Rust SDK guest: prefer dedicated tools over recursive shell search.");
        allow()
    }

    // capability: register_tool
    #[hyper_tool(
        description = "Echo tool_input JSON back (SDK register_tool demo)",
        schema = r#"{"type":"object","properties":{"msg":{"type":"string"}}}"#
    )]
    fn echo(args: &str) -> i32 {
        tool_result(args);
        allow()
    }
}
