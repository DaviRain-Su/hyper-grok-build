//! Shared contract types for Hyper WASM extensions.
//!
//! This crate is dependency-light (serde + thiserror) so hooks, agent, and
//! the extension runtime can all depend on it without pulling wasmtime.
//!
//! ## Versions
//!
//! - [`WIT_PACKAGE`] / [`WIT_VERSION`] — Component Model target
//!   (`docs/design-wasm-extensions.md`, `wit/extension.wit`).
//! - [`CORE_ABI_VERSION`] — Phase 0 core-wasm bootstrap exported by guests
//!   as `hyper_ext_abi_version`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Component Model package name (WIT).
pub const WIT_PACKAGE: &str = "hyper:extension";
/// Component Model package version (WIT).
pub const WIT_VERSION: &str = "0.1.0";
/// Full `package` string as in the WIT file.
pub const WIT_PACKAGE_FULL: &str = "hyper:extension@0.1.0";

/// Phase 0 core-wasm ABI version. Guests export `hyper_ext_abi_version() -> i32`.
pub const CORE_ABI_VERSION: i32 = 1;

/// How gate handlers treat guest trap / timeout.
///
/// Default [`GateFailMode::Open`] matches classic hooks (fail-open). Set
/// [`GateFailMode::Closed`] for hard security policies (deny on trap/timeout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateFailMode {
    /// Trap/timeout → allow (log only).
    #[default]
    Open,
    /// Trap/timeout on a gate capability → deny/block.
    Closed,
}

impl GateFailMode {
    /// Parse `open` / `closed` (case-insensitive). Unknown → Open.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "closed" | "fail-closed" | "fail_closed" => Self::Closed,
            _ => Self::Open,
        }
    }

    /// From `GROK_EXTENSION_GATE_FAIL` env (`open` | `closed`).
    pub fn from_env() -> Self {
        std::env::var("GROK_EXTENSION_GATE_FAIL")
            .map(|v| Self::parse(&v))
            .unwrap_or_default()
    }
}

/// Export name: guest returns [`CORE_ABI_VERSION`].
pub const EXPORT_ABI_VERSION: &str = "hyper_ext_abi_version";
/// Export name: session start handler; return `0` on success.
pub const EXPORT_ON_SESSION_START: &str = "hyper_ext_on_session_start";
/// Export name: session end handler; return `0` on success.
pub const EXPORT_ON_SESSION_END: &str = "hyper_ext_on_session_end";
/// Export name: pre-tool gate; return `0` allow, `1` deny (Phase 0: no reason string yet).
pub const EXPORT_ON_PRE_TOOL_USE: &str = "hyper_ext_on_pre_tool_use";
/// Export name: before agent start; return `0` on success.
/// Guest may call `hyper_host.set_inject_context` / `set_append_system` with
/// pointers into its exported `memory`.
pub const EXPORT_ON_BEFORE_AGENT_START: &str = "hyper_ext_on_before_agent_start";
/// Export name: turn stop gate; return `0` continue (allow stop), `1` block.
pub const EXPORT_ON_STOP: &str = "hyper_ext_on_stop";
/// Export name: pre-compaction observe; return `0` on success.
pub const EXPORT_ON_PRE_COMPACT: &str = "hyper_ext_on_pre_compact";
/// Return number of tools this guest registers (requires `register_tool`).
pub const EXPORT_TOOL_COUNT: &str = "hyper_ext_tool_count";
/// Describe tool at `tool_index` via `set_tool_*` host imports.
pub const EXPORT_DESCRIBE_TOOL: &str = "hyper_ext_describe_tool";
/// Invoke tool named in host `tool_name` with `tool_input`; write result via `set_tool_result`.
pub const EXPORT_INVOKE_TOOL: &str = "hyper_ext_invoke_tool";
/// Before each model round (tool loop iteration); inject via set_inject_context.
pub const EXPORT_ON_BEFORE_MODEL: &str = "hyper_ext_on_before_model";

/// One tool advertised by a WASM guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmToolDescriptor {
    pub extension: String,
    pub name: String,
    pub description: String,
    /// JSON Schema object (as string). Empty → default empty object schema.
    pub input_schema_json: String,
}

impl WasmToolDescriptor {
    /// Client-facing tool name without session scope: `wasm_{extension}_{name}`.
    ///
    /// Prefer [`Self::client_name_for_session`] when registering on a shared
    /// ToolBridge so concurrent sessions do not collide.
    pub fn client_name(&self) -> String {
        self.client_name_for_session(None)
    }

    /// Client-facing tool name, optionally session-scoped.
    ///
    /// With a session key: `wasm_{session}_{extension}_{name}` (all tokens
    /// sanitized). Session keys are shortened to 12 alphanumeric chars so
    /// UUIDs stay within tool-id length budgets.
    pub fn client_name_for_session(&self, session_key: Option<&str>) -> String {
        let ext = sanitize_tool_token(&self.extension);
        let name = sanitize_tool_token(&self.name);
        match session_key
            .map(short_session_token)
            .filter(|s| !s.is_empty())
        {
            Some(sk) => format!("wasm_{sk}_{ext}_{name}"),
            None => format!("wasm_{ext}_{name}"),
        }
    }

    pub fn parsed_schema(&self) -> serde_json::Value {
        serde_json::from_str(&self.input_schema_json)
            .unwrap_or_else(|_| serde_json::json!({"type": "object", "properties": {}}))
    }
}

/// Max length for a guest-advertised short tool name.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Whether a guest tool short-name is acceptable for registration.
pub fn is_valid_guest_tool_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_TOOL_NAME_LEN {
        return false;
    }
    // ToolId segments are [a-zA-Z0-9_-]+ after sanitization; allow `.` in the
    // guest name (sanitized to `_`) for author convenience.
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// True when `schema_json` parses as a JSON object (or is empty → default).
pub fn is_valid_tool_schema_json(schema_json: &str) -> bool {
    if schema_json.is_empty() {
        return true;
    }
    match serde_json::from_str::<serde_json::Value>(schema_json) {
        Ok(v) => v.is_object(),
        Err(_) => false,
    }
}

/// Sanitize a token for use inside a `wasm_*` client tool name.
pub fn sanitize_tool_token(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Compact session id fragment for tool client names (max 12 alnum chars).
pub fn short_session_token(session_id: &str) -> String {
    session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect()
}

/// Default wall-clock timeouts (design §7.3).
pub mod timeouts {
    use super::Duration;

    pub const INIT: Duration = Duration::from_secs(2);
    pub const OBSERVE: Duration = Duration::from_secs(1);
    pub const GATE: Duration = Duration::from_secs(2);
    pub const BEFORE_AGENT: Duration = Duration::from_secs(1);
}

/// Max UTF-8 bytes for inject / append strings (host-enforced).
pub const MAX_INJECT_BYTES: usize = 32 * 1024;
/// Align with hooks: large tool payloads are capped.
pub const MAX_TOOL_PAYLOAD_BYTES: usize = 128 * 1024;

/// MVP lifecycle events (design §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionEvent {
    SessionStart,
    BeforeAgentStart,
    PreToolUse,
    PostToolUse,
    Stop,
    PreCompact,
    SessionEnd,
}

impl ExtensionEvent {
    pub const ALL: &'static [ExtensionEvent] = &[
        ExtensionEvent::SessionStart,
        ExtensionEvent::BeforeAgentStart,
        ExtensionEvent::PreToolUse,
        ExtensionEvent::PostToolUse,
        ExtensionEvent::Stop,
        ExtensionEvent::PreCompact,
        ExtensionEvent::SessionEnd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::BeforeAgentStart => "before_agent_start",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::Stop => "stop",
            Self::PreCompact => "pre_compact",
            Self::SessionEnd => "session_end",
        }
    }

    /// Capability required for gate/inject effects (observe always allowed when loaded).
    pub fn required_capability(self) -> Option<Capability> {
        match self {
            Self::PreToolUse => Some(Capability::PreToolGate),
            Self::BeforeAgentStart => Some(Capability::BeforeAgentInject),
            Self::Stop => Some(Capability::StopGate),
            _ => None,
        }
    }
}

impl fmt::Display for ExtensionEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Manifest-declared capabilities (design §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    PreToolGate,
    BeforeAgentInject,
    StopGate,
    /// Guest may expose tools via `hyper_ext_tool_count` / describe / invoke.
    RegisterTool,
    /// Per-model-round inject (system-reminder), not full history rewrite.
    BeforeModelInject,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolGate => "pre_tool_gate",
            Self::BeforeAgentInject => "before_agent_inject",
            Self::StopGate => "stop_gate",
            Self::RegisterTool => "register_tool",
            Self::BeforeModelInject => "before_model_inject",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pre_tool_gate" => Some(Self::PreToolGate),
            "before_agent_inject" => Some(Self::BeforeAgentInject),
            "stop_gate" => Some(Self::StopGate),
            "register_tool" => Some(Self::RegisterTool),
            "before_model_inject" => Some(Self::BeforeModelInject),
            _ => None,
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `plugin.json` `runtime` block (design §6 / §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeManifest {
    /// Relative path to the wasm module (default `extension.wasm`).
    #[serde(default = "default_wasm_path")]
    pub wasm: String,
    /// Expected WIT package, e.g. `hyper:extension@0.1.0`.
    #[serde(default = "default_wit")]
    pub wit: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Per-extension gate fail mode. When set, overrides the process default
    /// (`GROK_EXTENSION_GATE_FAIL` / runtime-level setting) for this guest only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_fail: Option<GateFailMode>,
}

fn default_wasm_path() -> String {
    "extension.wasm".into()
}

fn default_wit() -> String {
    WIT_PACKAGE_FULL.into()
}

impl Default for RuntimeManifest {
    fn default() -> Self {
        Self {
            wasm: default_wasm_path(),
            wit: default_wit(),
            capabilities: Vec::new(),
            gate_fail: None,
        }
    }
}

impl RuntimeManifest {
    pub fn parsed_capabilities(&self) -> Vec<Capability> {
        self.capabilities
            .iter()
            .filter_map(|s| Capability::parse(s))
            .collect()
    }

    pub fn has_capability(&self, cap: Capability) -> bool {
        self.parsed_capabilities().contains(&cap)
    }
}

/// How a guest was discovered for loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSpec {
    pub name: String,
    pub wasm_path: PathBuf,
    pub capabilities: Vec<Capability>,
    pub trusted: bool,
    /// Per-extension gate fail mode; `None` inherits the runtime/process default.
    pub gate_fail: Option<GateFailMode>,
    /// Absolute plugin data directory (`~/.grok/plugin-data/<id>/`), if known.
    /// Exposed read-only to the guest via `hyper_host.plugin_data_dir_*`.
    pub plugin_data_dir: Option<PathBuf>,
}

impl ExtensionSpec {
    /// Untrusted plugins must not be instantiated (design R5).
    pub fn may_load(&self) -> bool {
        self.trusted
    }

    pub fn allows(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }

    /// Effective gate fail mode for this spec.
    pub fn effective_gate_fail(&self, runtime_default: GateFailMode) -> GateFailMode {
        self.gate_fail.unwrap_or(runtime_default)
    }
}

// --- Event I/O types (host-side; map to WIT later) ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStartIn {
    pub session_id: String,
    pub cwd: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeforeAgentStartIn {
    pub prompt: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BeforeAgentStartOut {
    pub inject_context: Option<String>,
    pub append_system: Option<String>,
}

impl BeforeAgentStartOut {
    /// Host-side truncation (design §8).
    pub fn truncated(mut self) -> Self {
        if let Some(ref mut s) = self.inject_context {
            truncate_utf8(s, MAX_INJECT_BYTES);
        }
        if let Some(ref mut s) = self.append_system {
            truncate_utf8(s, MAX_INJECT_BYTES);
        }
        self
    }

    pub fn merge_append(mut self, other: BeforeAgentStartOut) -> Self {
        self.inject_context = merge_opt_string(self.inject_context, other.inject_context);
        self.append_system = merge_opt_string(self.append_system, other.append_system);
        self
    }
}

fn merge_opt_string(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(mut x), Some(y)) => {
            if !x.is_empty() && !y.is_empty() {
                x.push('\n');
            }
            x.push_str(&y);
            Some(x)
        }
    }
}

/// Truncate to at most `max` **bytes** on a UTF-8 char boundary (never panics).
pub fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Structured errors when decoding guest↔host string memory.
///
/// Host code must not panic on malicious ptr/len/UTF-8; callers map these to
/// trap-free rejections (skip the write, return a failed call result, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GuestStringError {
    /// `ptr` or `len` was negative.
    #[error("guest string pointer or length is negative")]
    Negative,
    /// Guest `len` exceeds the host policy maximum (rejected before any slice).
    #[error("guest string length exceeds host maximum")]
    TooLong,
    /// Slice would read past linear memory (or pointer arithmetic overflow).
    #[error("guest string pointer/length is out of bounds")]
    OutOfBounds,
    /// Bytes are not valid UTF-8 (strict; no lossy substitution).
    #[error("guest string is not valid UTF-8")]
    InvalidUtf8,
}

/// Decode a guest-provided byte slice as strict UTF-8.
///
/// Never panics. Invalid sequences return [`GuestStringError::InvalidUtf8`].
pub fn decode_guest_utf8(bytes: &[u8]) -> Result<String, GuestStringError> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| GuestStringError::InvalidUtf8)
}

/// Read a UTF-8 string from a linear-memory-like buffer with guest `ptr`/`len`.
///
/// - Rejects negative `ptr`/`len` without wrapping.
/// - Rejects `len > max_len` with [`GuestStringError::TooLong`] **before**
///   any memory slice or UTF-8 decode (never silently accepts a prefix).
/// - Bounds-checks with checked arithmetic (no panic on OOB).
/// - Requires **strict** UTF-8 (no `from_utf8_lossy`).
pub fn read_guest_utf8_from_memory(
    memory: &[u8],
    ptr: i32,
    len: i32,
    max_len: usize,
) -> Result<String, GuestStringError> {
    if ptr < 0 || len < 0 {
        return Err(GuestStringError::Negative);
    }
    let len = len as usize;
    if len > max_len {
        return Err(GuestStringError::TooLong);
    }
    let start = ptr as usize;
    let end = start
        .checked_add(len)
        .ok_or(GuestStringError::OutOfBounds)?;
    let slice = memory
        .get(start..end)
        .ok_or(GuestStringError::OutOfBounds)?;
    decode_guest_utf8(slice)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreToolIn {
    pub tool_name: String,
    pub tool_input_json: String,
}

impl PreToolIn {
    /// Cap tool input on a **char boundary** (never panic mid-UTF-8).
    /// Prefer rejecting at higher layers when full JSON is required.
    pub fn capped(mut self) -> Self {
        if self.tool_input_json.len() > MAX_TOOL_PAYLOAD_BYTES {
            truncate_utf8(&mut self.tool_input_json, MAX_TOOL_PAYLOAD_BYTES);
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolOut {
    Allow,
    Deny { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostToolIn {
    pub tool_name: String,
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopIn {
    pub stop_hook_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOut {
    Continue,
    Block { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreCompactIn {
    pub reason: String,
}

/// Errors from host-side contract validation (not wasmtime traps).
#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    #[error("extension is not trusted; runtime will not load")]
    NotTrusted,
    #[error("missing capability `{0}` for event effect")]
    MissingCapability(Capability),
    #[error("unsupported WIT package `{0}` (expected {WIT_PACKAGE_FULL} or compatible 0.1.x)")]
    UnsupportedWit(String),
}

/// Whether a WIT package string is acceptable for this host.
pub fn wit_compatible(wit: &str) -> bool {
    let w = wit.trim();
    w == WIT_PACKAGE_FULL || w == WIT_PACKAGE || w.starts_with("hyper:extension@0.1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_parse_roundtrip() {
        for cap in [
            Capability::PreToolGate,
            Capability::BeforeAgentInject,
            Capability::StopGate,
            Capability::RegisterTool,
            Capability::BeforeModelInject,
        ] {
            assert_eq!(Capability::parse(cap.as_str()), Some(cap));
        }
        assert_eq!(Capability::parse("nope"), None);
        let d = WasmToolDescriptor {
            extension: "my-ext".into(),
            name: "echo.tool".into(),
            description: "d".into(),
            input_schema_json: "{}".into(),
        };
        assert_eq!(d.client_name(), "wasm_my-ext_echo_tool");
        assert_eq!(
            d.client_name_for_session(Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890")),
            "wasm_a1b2c3d4e5f6_my-ext_echo_tool"
        );
        assert!(is_valid_guest_tool_name("echo_tool"));
        assert!(!is_valid_guest_tool_name(""));
        assert!(!is_valid_guest_tool_name("bad name"));
        assert!(is_valid_tool_schema_json(r#"{"type":"object"}"#));
        assert!(!is_valid_tool_schema_json("[1,2]"));
        assert!(!is_valid_tool_schema_json("not-json"));
    }

    #[test]
    fn untrusted_may_not_load() {
        let spec = ExtensionSpec {
            name: "x".into(),
            wasm_path: PathBuf::from("extension.wasm"),
            capabilities: vec![Capability::PreToolGate],
            trusted: false,
            gate_fail: None,
            plugin_data_dir: None,
        };
        assert!(!spec.may_load());
        assert_eq!(
            spec.effective_gate_fail(GateFailMode::Open),
            GateFailMode::Open
        );
        let closed = ExtensionSpec {
            gate_fail: Some(GateFailMode::Closed),
            ..spec
        };
        assert_eq!(
            closed.effective_gate_fail(GateFailMode::Open),
            GateFailMode::Closed
        );
    }

    #[test]
    fn inject_merge_and_truncate() {
        let a = BeforeAgentStartOut {
            inject_context: Some("a".into()),
            append_system: None,
        };
        let b = BeforeAgentStartOut {
            inject_context: Some("b".into()),
            append_system: Some("sys".into()),
        };
        let m = a.merge_append(b);
        assert_eq!(m.inject_context.as_deref(), Some("a\nb"));
        assert_eq!(m.append_system.as_deref(), Some("sys"));

        let huge = "x".repeat(MAX_INJECT_BYTES + 50);
        let t = BeforeAgentStartOut {
            inject_context: Some(huge),
            append_system: None,
        }
        .truncated();
        assert!(t.inject_context.unwrap().len() <= MAX_INJECT_BYTES);
    }

    #[test]
    fn guest_utf8_rejects_invalid_and_oob_without_panic() {
        // Valid ASCII.
        assert_eq!(decode_guest_utf8(b"hello").as_deref(), Ok("hello"));
        // Multibyte OK.
        assert_eq!(decode_guest_utf8("é".as_bytes()).as_deref(), Ok("é"));
        // Malicious / truncated multi-byte sequence.
        assert_eq!(
            decode_guest_utf8(&[0xE2, 0x82]), // incomplete €
            Err(GuestStringError::InvalidUtf8)
        );
        assert_eq!(
            decode_guest_utf8(&[0xFF, 0xFE]),
            Err(GuestStringError::InvalidUtf8)
        );

        let mem = b"hello-world";
        assert_eq!(
            read_guest_utf8_from_memory(mem, 0, 5, 1024).as_deref(),
            Ok("hello")
        );
        // OOB past end.
        assert_eq!(
            read_guest_utf8_from_memory(mem, 8, 16, 1024),
            Err(GuestStringError::OutOfBounds)
        );
        // Negative ptr/len.
        assert_eq!(
            read_guest_utf8_from_memory(mem, -1, 2, 1024),
            Err(GuestStringError::Negative)
        );
        assert_eq!(
            read_guest_utf8_from_memory(mem, 0, -3, 1024),
            Err(GuestStringError::Negative)
        );
        // TooLong: reject before slicing — never silently accept a prefix.
        let big = vec![b'a'; 64];
        assert_eq!(
            read_guest_utf8_from_memory(&big, 0, 10_000, 8),
            Err(GuestStringError::TooLong)
        );
        // Exact max is allowed when memory is large enough.
        assert_eq!(
            read_guest_utf8_from_memory(&big, 0, 8, 8).as_deref(),
            Ok("aaaaaaaa")
        );
        // Invalid UTF-8 inside bounds.
        let bad = [b'a', 0xFF, b'b'];
        assert_eq!(
            read_guest_utf8_from_memory(&bad, 0, 3, 1024),
            Err(GuestStringError::InvalidUtf8)
        );
    }

    #[test]
    fn truncate_utf8_never_splits_codepoint() {
        // "é" is 2 bytes; max=1 must not panic and must yield empty or prior chars.
        let mut s = "aé".to_string();
        truncate_utf8(&mut s, 2); // 'a' + first byte of é → back up to "a"
        assert_eq!(s, "a");
        let mut s = "é".to_string();
        truncate_utf8(&mut s, 1);
        assert_eq!(s, "");
    }

    #[test]
    fn wit_compat() {
        assert!(wit_compatible(WIT_PACKAGE_FULL));
        assert!(wit_compatible("hyper:extension@0.1.9"));
        assert!(!wit_compatible("hyper:extension@0.2.0"));
    }

    #[test]
    fn runtime_manifest_defaults() {
        let m: RuntimeManifest = serde_json::from_str("{}").unwrap();
        assert_eq!(m.wasm, "extension.wasm");
        assert_eq!(m.wit, WIT_PACKAGE_FULL);
        assert_eq!(m.gate_fail, None);
        let m2: RuntimeManifest =
            serde_json::from_str(r#"{"gate_fail":"closed","capabilities":["pre_tool_gate"]}"#)
                .unwrap();
        assert_eq!(m2.gate_fail, Some(GateFailMode::Closed));
    }
}
