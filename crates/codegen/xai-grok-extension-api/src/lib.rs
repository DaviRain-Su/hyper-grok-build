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
    /// Client-facing tool name: `wasm_{extension}_{name}` (sanitized).
    pub fn client_name(&self) -> String {
        let ext = sanitize_tool_token(&self.extension);
        let name = sanitize_tool_token(&self.name);
        format!("wasm_{ext}_{name}")
    }

    pub fn parsed_schema(&self) -> serde_json::Value {
        serde_json::from_str(&self.input_schema_json).unwrap_or_else(|_| {
            serde_json::json!({"type": "object", "properties": {}})
        })
    }
}

fn sanitize_tool_token(s: &str) -> String {
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
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolGate => "pre_tool_gate",
            Self::BeforeAgentInject => "before_agent_inject",
            Self::StopGate => "stop_gate",
            Self::RegisterTool => "register_tool",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pre_tool_gate" => Some(Self::PreToolGate),
            "before_agent_inject" => Some(Self::BeforeAgentInject),
            "stop_gate" => Some(Self::StopGate),
            "register_tool" => Some(Self::RegisterTool),
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
}

impl ExtensionSpec {
    /// Untrusted plugins must not be instantiated (design R5).
    pub fn may_load(&self) -> bool {
        self.trusted
    }

    pub fn allows(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
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

fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreToolIn {
    pub tool_name: String,
    pub tool_input_json: String,
}

impl PreToolIn {
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
    }

    #[test]
    fn untrusted_may_not_load() {
        let spec = ExtensionSpec {
            name: "x".into(),
            wasm_path: PathBuf::from("extension.wasm"),
            capabilities: vec![Capability::PreToolGate],
            trusted: false,
        };
        assert!(!spec.may_load());
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
    }
}
