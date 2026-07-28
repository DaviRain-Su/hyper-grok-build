//! Raw `hyper_host` imports and byte helpers.
//!
//! Prefer the free functions on the crate root (`input_contains`, `deny`, …).

#[link(wasm_import_module = "hyper_host")]
unsafe extern "C" {
    #[link_name = "input_len"]
    fn raw_input_len() -> i32;
    #[link_name = "input_byte"]
    fn raw_input_byte(idx: i32) -> i32;
    #[link_name = "tool_name_len"]
    fn raw_tool_name_len() -> i32;
    #[link_name = "tool_name_byte"]
    fn raw_tool_name_byte(idx: i32) -> i32;
    #[link_name = "prompt_len"]
    fn raw_prompt_len() -> i32;
    #[link_name = "prompt_byte"]
    fn raw_prompt_byte(idx: i32) -> i32;
    #[link_name = "set_inject_context"]
    fn raw_set_inject_context(ptr: *const u8, len: i32);
    #[link_name = "set_append_system"]
    fn raw_set_append_system(ptr: *const u8, len: i32);
    #[link_name = "set_gate_reason"]
    fn raw_set_gate_reason(ptr: *const u8, len: i32);
    #[link_name = "log"]
    fn raw_log(level: i32, ptr: *const u8, len: i32);
    #[link_name = "plugin_data_dir_len"]
    fn raw_plugin_data_dir_len() -> i32;
    #[link_name = "plugin_data_dir_byte"]
    fn raw_plugin_data_dir_byte(idx: i32) -> i32;
    #[link_name = "stop_hook_active"]
    fn raw_stop_hook_active() -> i32;
    #[link_name = "tool_index"]
    fn raw_tool_index() -> i32;
    #[link_name = "set_tool_name"]
    fn raw_set_tool_name(ptr: *const u8, len: i32);
    #[link_name = "set_tool_description"]
    fn raw_set_tool_description(ptr: *const u8, len: i32);
    #[link_name = "set_tool_schema"]
    fn raw_set_tool_schema(ptr: *const u8, len: i32);
    #[link_name = "set_tool_result"]
    fn raw_set_tool_result(ptr: *const u8, len: i32);
    #[link_name = "compact_reason_len"]
    fn raw_compact_reason_len() -> i32;
    #[link_name = "compact_reason_byte"]
    fn raw_compact_reason_byte(idx: i32) -> i32;
}

fn read_bytes(len_fn: unsafe extern "C" fn() -> i32, byte_fn: unsafe extern "C" fn(i32) -> i32) -> Vec<u8> {
    let n = unsafe { len_fn() };
    if n <= 0 {
        return Vec::new();
    }
    let n = n as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let b = unsafe { byte_fn(i as i32) };
        if b < 0 {
            break;
        }
        out.push(b as u8);
    }
    out
}

pub fn read_input() -> Vec<u8> {
    read_bytes(raw_input_len, raw_input_byte)
}

pub fn read_tool_name() -> String {
    String::from_utf8_lossy(&read_bytes(raw_tool_name_len, raw_tool_name_byte)).into_owned()
}

pub fn read_prompt() -> Vec<u8> {
    read_bytes(raw_prompt_len, raw_prompt_byte)
}

pub fn read_compact_reason() -> String {
    String::from_utf8_lossy(&read_bytes(raw_compact_reason_len, raw_compact_reason_byte)).into_owned()
}

pub fn tool_index() -> i32 {
    unsafe { raw_tool_index() }
}

pub fn stop_hook_active() -> bool {
    unsafe { raw_stop_hook_active() != 0 }
}

fn write_str(f: unsafe extern "C" fn(*const u8, i32), s: &str) {
    let b = s.as_bytes();
    let len = b.len().min(32 * 1024) as i32;
    unsafe {
        f(b.as_ptr(), len);
    }
}

pub fn set_inject_context(s: &str) {
    write_str(raw_set_inject_context, s);
}

pub fn set_append_system(s: &str) {
    write_str(raw_set_append_system, s);
}

pub fn set_gate_reason(s: &str) {
    write_str(raw_set_gate_reason, s);
}

pub fn set_tool_name(s: &str) {
    write_str(raw_set_tool_name, s);
}

pub fn set_tool_description(s: &str) {
    write_str(raw_set_tool_description, s);
}

pub fn set_tool_schema(s: &str) {
    write_str(raw_set_tool_schema, s);
}

pub fn set_tool_result(s: &str) {
    write_str(raw_set_tool_result, s);
}

/// Guest → host log. `level`: 0=debug, 1=info, 2=warn, 3=error.
pub fn log(level: i32, msg: &str) {
    let b = msg.as_bytes();
    let len = b.len().min(32 * 1024) as i32;
    unsafe {
        raw_log(level, b.as_ptr(), len);
    }
}

/// Absolute plugin data directory path (may be empty if host did not set one).
pub fn read_plugin_data_dir() -> String {
    String::from_utf8_lossy(&read_bytes(
        raw_plugin_data_dir_len,
        raw_plugin_data_dir_byte,
    ))
    .into_owned()
}

/// Substring search over raw bytes.
pub fn bytes_contain(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}
