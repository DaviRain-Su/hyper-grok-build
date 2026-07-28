//! Minimal **Rust-first** Hyper WASM extension guest (core-wasm bootstrap ABI).
//!
//! Build:
//! ```bash
//! rustup target add wasm32-unknown-unknown
//! cargo build --release --target wasm32-unknown-unknown
//! cp target/wasm32-unknown-unknown/release/hyper_ext_rust_guest_template.wasm \
//!    ../my-plugin/extension.wasm
//! ```
//!
//! Host imports live in module `hyper_host` (see design-wasm-extensions.md).

#![allow(unused)]

// ── Host imports ───────────────────────────────────────────────────────────

#[link(wasm_import_module = "hyper_host")]
extern "C" {
    fn input_len() -> i32;
    fn input_byte(idx: i32) -> i32;
    fn tool_name_len() -> i32;
    fn tool_name_byte(idx: i32) -> i32;
    fn prompt_len() -> i32;
    fn prompt_byte(idx: i32) -> i32;
    fn set_inject_context(ptr: *const u8, len: i32);
    fn set_append_system(ptr: *const u8, len: i32);
    fn stop_hook_active() -> i32;
}

// ── Required / recommended exports ─────────────────────────────────────────

/// Must return `1` (CORE_ABI_VERSION).
#[no_mangle]
pub extern "C" fn hyper_ext_abi_version() -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn hyper_ext_on_session_start() -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn hyper_ext_on_session_end() -> i32 {
    0
}

/// Return `0` allow, `1` deny. Requires capability `pre_tool_gate`.
#[no_mangle]
pub extern "C" fn hyper_ext_on_pre_tool_use() -> i32 {
    // Example: deny if tool input JSON contains ASCII "rm -rf"
    if input_contains(b"rm -rf") {
        1
    } else {
        0
    }
}

/// Optional. Requires capability `before_agent_inject`.
#[no_mangle]
pub extern "C" fn hyper_ext_on_before_agent_start() -> i32 {
    // Static policy inject (guest linear memory).
    let msg = b"Rust guest: prefer dedicated tools over recursive shell search.";
    unsafe {
        set_inject_context(msg.as_ptr(), msg.len() as i32);
    }
    0
}

/// Return `0` allow stop, `1` block. Requires capability `stop_gate`.
#[no_mangle]
pub extern "C" fn hyper_ext_on_stop() -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn hyper_ext_on_pre_compact() -> i32 {
    0
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn input_contains(needle: &[u8]) -> bool {
    let n = unsafe { input_len() };
    if n < 0 {
        return false;
    }
    let n = n as usize;
    if needle.is_empty() || n < needle.len() {
        return false;
    }
    let mut i = 0usize;
    while i + needle.len() <= n {
        let mut ok = true;
        for (j, b) in needle.iter().enumerate() {
            let got = unsafe { input_byte((i + j) as i32) };
            if got < 0 || (got as u8) != *b {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
        i += 1;
    }
    false
}
