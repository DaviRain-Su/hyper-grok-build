//! Hyper WASM guest — written with **xai-grok-extension-sdk** (recommended path).
//!
//! Uses **declarative macros only** (`hyper_extension!` / `macro_rules!`) —
//! no proc-macro attributes.
//!
//! ```bash
//! rustup target add wasm32-unknown-unknown
//! cargo build --release --target wasm32-unknown-unknown
//! cp target/wasm32-unknown-unknown/release/hyper_ext_rust_guest_template.wasm \
//!    ./extension.wasm
//! ```

#![allow(unused)]

use xai_grok_extension_sdk::prelude::*;

xai_grok_extension_sdk::hyper_extension! {
    // capability: pre_tool_gate
    pre_tool_use: || {
        if input_contains("rm -rf") {
            deny("rust-guest-template: blocked rm -rf in tool input")
        } else {
            allow()
        }
    },
    // capability: before_agent_inject
    before_agent_start: || {
        inject_context("Rust SDK guest: prefer dedicated tools over recursive shell search.");
        allow()
    },
    // capability: register_tool
    tools: {
        echo {
            description: "Echo tool_input JSON back (SDK register_tool demo)",
            schema: r#"{"type":"object","properties":{"msg":{"type":"string"}}}"#,
            invoke: |args| {
                tool_result(args);
                allow()
            }
        }
    }
}
