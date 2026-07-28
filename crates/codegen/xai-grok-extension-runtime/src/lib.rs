//! Hyper WASM extension runtime (Phase 0/1).
//!
//! ## Core-wasm bootstrap ABI
//!
//! | Export | Signature | Meaning |
//! |--------|-----------|---------|
//! | `hyper_ext_abi_version` | `() -> i32` | Must equal [`CORE_ABI_VERSION`] |
//! | `hyper_ext_on_session_start` | `() -> i32` | `0` = ok |
//! | `hyper_ext_on_session_end` | `() -> i32` | optional |
//! | `hyper_ext_on_pre_tool_use` | `() -> i32` | `0` allow, `1` deny |
//! | `hyper_ext_on_before_agent_start` | `() -> i32` | optional; uses set_inject/set_append |
//! | `hyper_ext_on_stop` | `() -> i32` | `0` allow stop, `1` block |
//! | `hyper_ext_on_pre_compact` | `() -> i32` | optional observe |
//!
//! Host imports under module `hyper_host` (for gate handlers):
//!
//! | Import | Signature | Meaning |
//! |--------|-----------|---------|
//! | `tool_name_len` | `() -> i32` | UTF-8 length of current tool name |
//! | `tool_name_byte` | `(i32) -> i32` | byte at index, or `-1` |
//! | `input_len` | `() -> i32` | UTF-8 length of tool input JSON |
//! | `input_byte` | `(i32) -> i32` | byte at index, or `-1` |
//! | `prompt_len` / `prompt_byte` | | user prompt for before_agent_start |
//! | `set_inject_context` / `set_append_system` | `(ptr,len)` | guest memory UTF-8 |
//! | `set_gate_reason` | `(ptr,len)` | deny/stop reason string for host UI |
//! | `log` | `(level,ptr,len)` | guest → host log (0=debug…3=error) |
//! | `plugin_data_dir_len` / `plugin_data_dir_byte` | plugin data dir path |
//! | `stop_hook_active` | `() -> i32` | 1 if stop gate already continued |
//! | `compact_reason_len` / `compact_reason_byte` | | pre_compact reason |
//!
//! Component Model + WIT (`hyper:extension@0.1.0`) remains the long-term target.
//! See `docs/design-wasm-extensions.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use xai_grok_extension_api::{
    is_valid_guest_tool_name, is_valid_tool_schema_json, timeouts, BeforeAgentStartIn,
    BeforeAgentStartOut, Capability, ContractError, ExtensionSpec, GateFailMode, PreCompactIn,
    PreToolIn, StopIn, StopOut, WasmToolDescriptor, CORE_ABI_VERSION, EXPORT_ABI_VERSION,
    EXPORT_DESCRIBE_TOOL, EXPORT_INVOKE_TOOL, EXPORT_ON_BEFORE_AGENT_START, EXPORT_ON_BEFORE_MODEL,
    EXPORT_ON_PRE_COMPACT, EXPORT_ON_PRE_TOOL_USE, EXPORT_ON_SESSION_END, EXPORT_ON_SESSION_START,
    EXPORT_ON_STOP, EXPORT_TOOL_COUNT, MAX_INJECT_BYTES, MAX_TOOL_PAYLOAD_BYTES,
};

/// Errors from loading or calling a guest.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("wasm feature disabled; rebuild with default `wasm` feature")]
    WasmDisabled,
    #[error("failed to read wasm at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("wasm module error: {0}")]
    Module(String),
    #[error("guest trap or call failed: {0}")]
    Trap(String),
    #[error("guest call timed out after {0:?}")]
    Timeout(Duration),
    #[error("unsupported ABI version {got} (host expects {CORE_ABI_VERSION})")]
    AbiMismatch { got: i32 },
    #[error("required export `{0}` missing")]
    MissingExport(&'static str),
    #[error("tool payload too large: {got} bytes (max {MAX_TOOL_PAYLOAD_BYTES})")]
    PayloadTooLarge { got: usize },
    #[error("invalid tool name `{0}`")]
    InvalidToolName(String),
    #[error("tool `{0}` was not advertised by extension `{1}`")]
    UnknownTool(String, String),
    #[error("tool arguments must be valid JSON")]
    InvalidToolArgs,
}

/// Log level for guest → host `hyper_host.log` (matches SDK constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum GuestLogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl GuestLogLevel {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::Debug,
            2 => Self::Warn,
            3 => Self::Error,
            _ => Self::Info,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// One guest log line captured during a call (for tests / optional UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestLogLine {
    pub level: GuestLogLevel,
    pub message: String,
}

/// Host-side state visible to guest imports during a call.
struct HostCtx {
    /// Extension name for tracing (set by runtime before each call).
    guest_name: String,
    /// Absolute plugin data directory (may be empty).
    plugin_data_dir: String,
    tool_name: String,
    tool_input: String,
    /// User prompt for `before_agent_start`.
    prompt: String,
    /// Written by guest via `set_inject_context`.
    inject_context: String,
    /// Written by guest via `set_append_system`.
    append_system: String,
    stop_hook_active: bool,
    compact_reason: String,
    /// Written by guest via `set_gate_reason` (deny / stop block message).
    gate_reason: String,
    /// Index for `describe_tool`.
    tool_index: i32,
    /// Written by guest during describe_tool / invoke_tool.
    tool_name_out: String,
    tool_description_out: String,
    tool_schema_out: String,
    tool_result_out: String,
    /// Captured `hyper_host.log` lines (capped).
    guest_logs: Vec<GuestLogLine>,
    /// Resource limits (must live with the Store; Oracle memory-bound fix).
    limits: wasmtime::StoreLimits,
}

impl Default for HostCtx {
    fn default() -> Self {
        Self {
            guest_name: String::new(),
            plugin_data_dir: String::new(),
            tool_name: String::new(),
            tool_input: String::new(),
            prompt: String::new(),
            inject_context: String::new(),
            append_system: String::new(),
            stop_hook_active: false,
            compact_reason: String::new(),
            gate_reason: String::new(),
            tool_index: 0,
            tool_name_out: String::new(),
            tool_description_out: String::new(),
            tool_schema_out: String::new(),
            tool_result_out: String::new(),
            guest_logs: Vec::new(),
            // 256 pages * 64KiB = 16MiB max linear memory (rustc guests often need >1MiB).
            limits: wasmtime::StoreLimitsBuilder::new()
                .memory_size(256 * 64 * 1024)
                .instances(1)
                .memories(1)
                .tables(4)
                .build(),
        }
    }
}

/// Process-wide-ish counters for one [`ExtensionRuntime`] (shared across clones).
#[derive(Debug, Default)]
pub struct ExtensionMetrics {
    pub loads_ok: AtomicU64,
    pub loads_failed: AtomicU64,
    pub calls_ok: AtomicU64,
    pub calls_failed: AtomicU64,
    pub calls_timeout: AtomicU64,
    pub pre_tool_denies: AtomicU64,
    pub stop_blocks: AtomicU64,
    pub tools_collected: AtomicU64,
    pub tools_invoked_ok: AtomicU64,
    pub tools_invoked_err: AtomicU64,
    pub guest_log_lines: AtomicU64,
}

/// Snapshot of [`ExtensionMetrics`] for logs / tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtensionMetricsSnapshot {
    pub loads_ok: u64,
    pub loads_failed: u64,
    pub calls_ok: u64,
    pub calls_failed: u64,
    pub calls_timeout: u64,
    pub pre_tool_denies: u64,
    pub stop_blocks: u64,
    pub tools_collected: u64,
    pub tools_invoked_ok: u64,
    pub tools_invoked_err: u64,
    pub guest_log_lines: u64,
}

impl std::fmt::Display for ExtensionMetricsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "loads_ok={} loads_failed={} calls_ok={} calls_failed={} calls_timeout={} \
             pre_tool_denies={} stop_blocks={} tools_collected={} tools_invoked_ok={} \
             tools_invoked_err={} guest_log_lines={}",
            self.loads_ok,
            self.loads_failed,
            self.calls_ok,
            self.calls_failed,
            self.calls_timeout,
            self.pre_tool_denies,
            self.stop_blocks,
            self.tools_collected,
            self.tools_invoked_ok,
            self.tools_invoked_err,
            self.guest_log_lines,
        )
    }
}

impl ExtensionMetricsSnapshot {
    /// Emit a structured ops log line (filter with `RUST_LOG=wasm_extension=info`).
    pub fn log_tracing(&self, reason: &str) {
        tracing::info!(
            target: "wasm_extension",
            reason = %reason,
            loads_ok = self.loads_ok,
            loads_failed = self.loads_failed,
            calls_ok = self.calls_ok,
            calls_failed = self.calls_failed,
            calls_timeout = self.calls_timeout,
            pre_tool_denies = self.pre_tool_denies,
            stop_blocks = self.stop_blocks,
            tools_collected = self.tools_collected,
            tools_invoked_ok = self.tools_invoked_ok,
            tools_invoked_err = self.tools_invoked_err,
            guest_log_lines = self.guest_log_lines,
            "wasm extension metrics"
        );
    }

    /// True when any failure-ish counter is non-zero (ops dashboards / alerts).
    pub fn has_failures(&self) -> bool {
        self.loads_failed > 0
            || self.calls_failed > 0
            || self.calls_timeout > 0
            || self.tools_invoked_err > 0
    }
}

impl ExtensionMetrics {
    pub fn snapshot(&self) -> ExtensionMetricsSnapshot {
        ExtensionMetricsSnapshot {
            loads_ok: self.loads_ok.load(Ordering::Relaxed),
            loads_failed: self.loads_failed.load(Ordering::Relaxed),
            calls_ok: self.calls_ok.load(Ordering::Relaxed),
            calls_failed: self.calls_failed.load(Ordering::Relaxed),
            calls_timeout: self.calls_timeout.load(Ordering::Relaxed),
            pre_tool_denies: self.pre_tool_denies.load(Ordering::Relaxed),
            stop_blocks: self.stop_blocks.load(Ordering::Relaxed),
            tools_collected: self.tools_collected.load(Ordering::Relaxed),
            tools_invoked_ok: self.tools_invoked_ok.load(Ordering::Relaxed),
            tools_invoked_err: self.tools_invoked_err.load(Ordering::Relaxed),
            guest_log_lines: self.guest_log_lines.load(Ordering::Relaxed),
        }
    }

    fn record_call(&self, r: &GuestCallResult) {
        match r {
            GuestCallResult::Ok { logs, .. } => {
                self.calls_ok.fetch_add(1, Ordering::Relaxed);
                self.guest_log_lines
                    .fetch_add(logs.len() as u64, Ordering::Relaxed);
            }
            GuestCallResult::Failed { .. } => {
                self.calls_failed.fetch_add(1, Ordering::Relaxed);
            }
            GuestCallResult::Timeout { .. } => {
                self.calls_timeout.fetch_add(1, Ordering::Relaxed);
            }
            GuestCallResult::SkippedExport { .. } | GuestCallResult::SkippedCapability { .. } => {}
        }
    }
}

/// Per-session registry of loaded extensions.
#[derive(Clone)]
pub struct ExtensionRuntime {
    guests: Vec<LoadedGuest>,
    /// Process/runtime default when a guest does not set `gate_fail` in its
    /// `plugin.json` `runtime` block.
    gate_fail: GateFailMode,
    /// Shared counters (survives [`Clone`] of the runtime for async dispatch).
    metrics: Arc<ExtensionMetrics>,
}

impl Default for ExtensionRuntime {
    fn default() -> Self {
        Self {
            guests: Vec::new(),
            gate_fail: GateFailMode::from_env(),
            metrics: Arc::new(ExtensionMetrics::default()),
        }
    }
}

#[derive(Clone)]
struct LoadedGuest {
    name: String,
    capabilities: Vec<Capability>,
    /// Per-guest override; `None` uses [`ExtensionRuntime::gate_fail`].
    gate_fail: Option<GateFailMode>,
    /// Absolute plugin data dir (empty if unknown).
    plugin_data_dir: String,
    /// Short tool names last returned by [`ExtensionRuntime::collect_registered_tools`].
    /// Empty until first successful collect; when non-empty, invoke must match.
    advertised_tools: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    #[cfg(feature = "wasm")]
    inner: WasmGuest,
    #[cfg(not(feature = "wasm"))]
    _inner: (),
}

impl ExtensionRuntime {
    fn effective_gate_fail(&self, guest: &LoadedGuest) -> GateFailMode {
        guest.gate_fail.unwrap_or(self.gate_fail)
    }
}

impl ExtensionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_gate_fail(mut self, mode: GateFailMode) -> Self {
        self.gate_fail = mode;
        self
    }

    pub fn set_gate_fail(&mut self, mode: GateFailMode) {
        self.gate_fail = mode;
    }

    pub fn gate_fail(&self) -> GateFailMode {
        self.gate_fail
    }

    /// Operational counters for this runtime (shared across clones).
    pub fn metrics(&self) -> ExtensionMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Snapshot + structured log (session end / plugin reload / ops).
    pub fn log_metrics(&self, reason: &str) {
        self.metrics().log_tracing(reason);
    }

    pub fn len(&self) -> usize {
        self.guests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.guests.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.guests.iter().map(|g| g.name.as_str())
    }

    /// Whether any loaded guest has the given capability.
    pub fn has_capability(&self, cap: Capability) -> bool {
        self.guests.iter().any(|g| g.capabilities.contains(&cap))
    }

    /// Replace contents by loading every trusted spec (skips untrusted / load errors).
    pub fn rebuild_from_specs(&mut self, specs: impl IntoIterator<Item = ExtensionSpec>) {
        self.guests.clear();
        for spec in specs {
            if let Err(e) = self.load(&spec) {
                tracing::warn!(
                    plugin = %spec.name,
                    error = %e,
                    "failed to load wasm extension; skipping"
                );
            }
        }
    }

    /// Load a trusted extension. Untrusted specs return [`ContractError::NotTrusted`].
    pub fn load(&mut self, spec: &ExtensionSpec) -> Result<(), RuntimeError> {
        if !spec.may_load() {
            self.metrics.loads_failed.fetch_add(1, Ordering::Relaxed);
            return Err(ContractError::NotTrusted.into());
        }
        #[cfg(feature = "wasm")]
        {
            match WasmGuest::load(&spec.wasm_path) {
                Ok(inner) => {
                    let plugin_data_dir = spec
                        .plugin_data_dir
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    self.guests.push(LoadedGuest {
                        name: spec.name.clone(),
                        capabilities: spec.capabilities.clone(),
                        gate_fail: spec.gate_fail,
                        plugin_data_dir,
                        advertised_tools: Arc::new(std::sync::Mutex::new(
                            std::collections::HashSet::new(),
                        )),
                        inner,
                    });
                    self.metrics.loads_ok.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Err(e) => {
                    self.metrics.loads_failed.fetch_add(1, Ordering::Relaxed);
                    Err(e)
                }
            }
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = spec;
            self.metrics.loads_failed.fetch_add(1, Ordering::Relaxed);
            Err(RuntimeError::WasmDisabled)
        }
    }

    pub async fn dispatch_session_start(&self) -> Vec<GuestCallResult> {
        self.dispatch_all_observe(GuestCall::SessionStart, timeouts::OBSERVE)
            .await
    }

    pub async fn dispatch_session_end(&self) -> Vec<GuestCallResult> {
        self.dispatch_all_observe(GuestCall::SessionEnd, timeouts::OBSERVE)
            .await
    }

    /// Before agent start: merge inject/append from guests with
    /// [`Capability::BeforeAgentInject`]. Trap/timeout = fail-open (no inject).
    pub async fn dispatch_before_agent_start(
        &self,
        input: &BeforeAgentStartIn,
    ) -> BeforeAgentStartDispatch {
        self.dispatch_inject_event(
            input,
            Capability::BeforeAgentInject,
            GuestCall::BeforeAgentStart,
            timeouts::BEFORE_AGENT,
        )
        .await
    }

    /// Before each model round (tool loop): inject only, no history rewrite.
    pub async fn dispatch_before_model(
        &self,
        input: &BeforeAgentStartIn,
    ) -> BeforeAgentStartDispatch {
        self.dispatch_inject_event(
            input,
            Capability::BeforeModelInject,
            GuestCall::BeforeModel,
            timeouts::BEFORE_AGENT,
        )
        .await
    }

    async fn dispatch_inject_event(
        &self,
        input: &BeforeAgentStartIn,
        cap: Capability,
        call: GuestCall,
        timeout: Duration,
    ) -> BeforeAgentStartDispatch {
        let mut merged = BeforeAgentStartOut::default();
        let mut results = Vec::new();
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            if !guest.capabilities.contains(&cap) {
                results.push((
                    guest.name.clone(),
                    GuestCallResult::SkippedCapability {
                        extension: guest.name.clone(),
                        capability: cap,
                    },
                ));
                continue;
            }
            let host = HostCtx {
                prompt: input.prompt.clone(),
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (r, host_out) = guest
                .inner
                .call_with_timeout_host(call, timeout, host)
                .await;
            self.metrics.record_call(&r);
            if matches!(&r, GuestCallResult::Ok { code: 0, .. }) {
                let piece = BeforeAgentStartOut {
                    inject_context: non_empty(host_out.inject_context),
                    append_system: non_empty(host_out.append_system),
                };
                let piece = tag_extension_out(piece, &guest.name);
                merged = merged.merge_append(piece);
            }
            results.push((guest.name.clone(), r));
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = (input, call, timeout, cap);
            let _ = &results;
        }
        BeforeAgentStartDispatch {
            out: merged.truncated(),
            results,
        }
    }

    /// Stop gate: first block wins among guests with [`Capability::StopGate`].
    pub async fn dispatch_stop(&self, input: &StopIn) -> StopDispatch {
        let mut results = Vec::new();
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            if !guest.capabilities.contains(&Capability::StopGate) {
                results.push((
                    guest.name.clone(),
                    GuestCallResult::SkippedCapability {
                        extension: guest.name.clone(),
                        capability: Capability::StopGate,
                    },
                ));
                continue;
            }
            let host = HostCtx {
                stop_hook_active: input.stop_hook_active,
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (r, host_out) = guest
                .inner
                .call_with_timeout_host(GuestCall::Stop, timeouts::GATE, host)
                .await;
            self.metrics.record_call(&r);
            let name = guest.name.clone();
            let blocked = matches!(&r, GuestCallResult::Ok { code: 1, .. });
            let failed_closed = self.effective_gate_fail(guest) == GateFailMode::Closed
                && matches!(
                    &r,
                    GuestCallResult::Failed { .. } | GuestCallResult::Timeout { .. }
                );
            results.push((name.clone(), r));
            if blocked || failed_closed {
                self.metrics.stop_blocks.fetch_add(1, Ordering::Relaxed);
                let reason = if !host_out.gate_reason.is_empty() {
                    host_out.gate_reason
                } else if failed_closed {
                    format!("wasm extension `{name}` failed closed (trap/timeout on stop)")
                } else {
                    format!("blocked by wasm extension `{name}`")
                };
                return StopDispatch {
                    decision: StopOut::Block { reason },
                    results,
                };
            }
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = input;
            let _ = &results;
        }
        StopDispatch {
            decision: StopOut::Continue,
            results,
        }
    }

    /// Pre-compact observe (no rewrite in Phase 3).
    pub async fn dispatch_pre_compact(&self, input: &PreCompactIn) -> Vec<GuestCallResult> {
        let mut out = Vec::new();
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            let host = HostCtx {
                compact_reason: input.reason.clone(),
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (r, _) = guest
                .inner
                .call_with_timeout_host(GuestCall::PreCompact, timeouts::OBSERVE, host)
                .await;
            self.metrics.record_call(&r);
            // Missing export is fine (optional handler).
            if !matches!(
                &r,
                GuestCallResult::SkippedExport {
                    export: EXPORT_ON_PRE_COMPACT,
                    ..
                }
            ) {
                out.push(r);
            }
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = input;
        }
        out
    }

    /// Pre-tool gate: first deny wins among guests with [`Capability::PreToolGate`].
    /// Trap/timeout: [`GateFailMode::Open`] allows; [`GateFailMode::Closed`] denies.
    pub async fn dispatch_pre_tool_use(&self, input: &PreToolIn) -> PreToolDispatch {
        let input = input.clone().capped();
        let mut results = Vec::new();
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            if !guest.capabilities.contains(&Capability::PreToolGate) {
                results.push((
                    guest.name.clone(),
                    GuestCallResult::SkippedCapability {
                        extension: guest.name.clone(),
                        capability: Capability::PreToolGate,
                    },
                ));
                continue;
            }
            let host = HostCtx {
                tool_name: input.tool_name.clone(),
                tool_input: input.tool_input_json.clone(),
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (r, host_out) = guest
                .inner
                .call_with_timeout_host(GuestCall::PreToolUse, timeouts::GATE, host)
                .await;
            self.metrics.record_call(&r);
            let name = guest.name.clone();
            let denied = matches!(&r, GuestCallResult::Ok { code: 1, .. });
            let failed_closed = self.effective_gate_fail(guest) == GateFailMode::Closed
                && matches!(
                    &r,
                    GuestCallResult::Failed { .. } | GuestCallResult::Timeout { .. }
                );
            results.push((name.clone(), r));
            if denied || failed_closed {
                self.metrics
                    .pre_tool_denies
                    .fetch_add(1, Ordering::Relaxed);
                let reason = if !host_out.gate_reason.is_empty() {
                    host_out.gate_reason
                } else if failed_closed {
                    format!(
                        "wasm extension `{name}` failed closed (trap/timeout on tool `{}`)",
                        input.tool_name
                    )
                } else {
                    format!(
                        "denied by wasm extension `{name}` (tool `{}`)",
                        input.tool_name
                    )
                };
                return PreToolDispatch {
                    decision: PreToolDecision::Deny {
                        extension: name,
                        reason,
                    },
                    results,
                };
            }
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = input;
            let _ = &results;
        }
        PreToolDispatch {
            decision: PreToolDecision::Allow,
            results,
        }
    }

    /// Load-only validation (ABI + required exports). Used by `plugin validate --load`.
    pub fn validate_wasm_file(path: &Path) -> Result<(), RuntimeError> {
        #[cfg(feature = "wasm")]
        {
            WasmGuest::load(path).map(|_| ())
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = path;
            Err(RuntimeError::WasmDisabled)
        }
    }

    /// Collect tools from guests with [`Capability::RegisterTool`].
    ///
    /// Drops tools with empty/invalid names, invalid JSON Schema, or duplicate
    /// short names within the same extension (Oracle validation). Updates each
    /// guest's advertised-tool set used by [`Self::invoke_registered_tool`].
    pub async fn collect_registered_tools(&self) -> Vec<WasmToolDescriptor> {
        let mut out = Vec::new();
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            if !guest.capabilities.contains(&Capability::RegisterTool) {
                continue;
            }
            let host = HostCtx {
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (count_res, _) = guest
                .inner
                .call_with_timeout_host(GuestCall::ToolCount, timeouts::OBSERVE, host)
                .await;
            self.metrics.record_call(&count_res);
            let count = match count_res {
                GuestCallResult::Ok { code, .. } if code >= 0 => code as usize,
                _ => continue,
            };
            // Cap tools per extension to avoid abuse.
            let count = count.min(32);
            let mut seen_names = std::collections::HashSet::new();
            let mut guest_tools = Vec::new();
            for i in 0..count {
                let host = HostCtx {
                    tool_index: i as i32,
                    plugin_data_dir: guest.plugin_data_dir.clone(),
                    ..HostCtx::default()
                };
                let (r, host_out) = guest
                    .inner
                    .call_with_timeout_host(GuestCall::DescribeTool, timeouts::OBSERVE, host)
                    .await;
                self.metrics.record_call(&r);
                if !matches!(r, GuestCallResult::Ok { code: 0, .. }) {
                    continue;
                }
                let name = host_out.tool_name_out;
                if !is_valid_guest_tool_name(&name) {
                    tracing::warn!(
                        extension = %guest.name,
                        tool = %name,
                        "skipping wasm tool with invalid name"
                    );
                    continue;
                }
                if !seen_names.insert(name.clone()) {
                    tracing::warn!(
                        extension = %guest.name,
                        tool = %name,
                        "skipping duplicate wasm tool name within extension"
                    );
                    continue;
                }
                let schema = if host_out.tool_schema_out.is_empty() {
                    r#"{"type":"object","properties":{}}"#.to_string()
                } else {
                    host_out.tool_schema_out
                };
                if !is_valid_tool_schema_json(&schema) {
                    tracing::warn!(
                        extension = %guest.name,
                        tool = %name,
                        "skipping wasm tool with invalid JSON Schema (must be object)"
                    );
                    continue;
                }
                guest_tools.push(WasmToolDescriptor {
                    extension: guest.name.clone(),
                    name,
                    description: host_out.tool_description_out,
                    input_schema_json: schema,
                });
            }
            if let Ok(mut set) = guest.advertised_tools.lock() {
                *set = guest_tools.iter().map(|t| t.name.clone()).collect();
            }
            out.extend(guest_tools);
        }
        self.metrics
            .tools_collected
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        out
    }

    /// Invoke a tool registered by a guest. `tool_name` is the **short** name
    /// from the guest (not the `wasm_*` client name).
    ///
    /// Rejects oversized payloads (no UTF-8 byte-slice truncate), non-JSON
    /// args, invalid names, and names not advertised by the last
    /// [`Self::collect_registered_tools`] for that extension (when any were
    /// collected).
    pub async fn invoke_registered_tool(
        &self,
        extension: &str,
        tool_name: &str,
        args_json: &str,
    ) -> Result<String, RuntimeError> {
        #[cfg(feature = "wasm")]
        {
            let guest = self
                .guests
                .iter()
                .find(|g| g.name == extension)
                .ok_or_else(|| RuntimeError::Module(format!("extension not loaded: {extension}")))?;
            if !guest.capabilities.contains(&Capability::RegisterTool) {
                return Err(RuntimeError::Module(format!(
                    "extension `{extension}` lacks register_tool capability"
                )));
            }
            if args_json.len() > MAX_TOOL_PAYLOAD_BYTES {
                self.metrics
                    .tools_invoked_err
                    .fetch_add(1, Ordering::Relaxed);
                return Err(RuntimeError::PayloadTooLarge {
                    got: args_json.len(),
                });
            }
            if !is_valid_guest_tool_name(tool_name) {
                self.metrics
                    .tools_invoked_err
                    .fetch_add(1, Ordering::Relaxed);
                return Err(RuntimeError::InvalidToolName(tool_name.to_string()));
            }
            {
                let advertised = guest
                    .advertised_tools
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if !advertised.is_empty() && !advertised.contains(tool_name) {
                    self.metrics
                        .tools_invoked_err
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(RuntimeError::UnknownTool(
                        tool_name.to_string(),
                        extension.to_string(),
                    ));
                }
            }
            if serde_json::from_str::<serde_json::Value>(args_json).is_err() {
                self.metrics
                    .tools_invoked_err
                    .fetch_add(1, Ordering::Relaxed);
                return Err(RuntimeError::InvalidToolArgs);
            }
            let host = HostCtx {
                tool_name: tool_name.to_string(),
                tool_input: args_json.to_string(),
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (r, host_out) = guest
                .inner
                .call_with_timeout_host(GuestCall::InvokeTool, timeouts::GATE, host)
                .await;
            self.metrics.record_call(&r);
            match r {
                GuestCallResult::Ok { code: 0, .. } => {
                    self.metrics
                        .tools_invoked_ok
                        .fetch_add(1, Ordering::Relaxed);
                    Ok(if host_out.tool_result_out.is_empty() {
                        "ok".into()
                    } else {
                        host_out.tool_result_out
                    })
                }
                GuestCallResult::Ok { code, .. } => {
                    self.metrics
                        .tools_invoked_err
                        .fetch_add(1, Ordering::Relaxed);
                    Err(RuntimeError::Module(format!(
                        "invoke_tool returned {code}: {}",
                        host_out.gate_reason
                    )))
                }
                GuestCallResult::Failed { error, .. } => {
                    self.metrics
                        .tools_invoked_err
                        .fetch_add(1, Ordering::Relaxed);
                    Err(RuntimeError::Trap(error))
                }
                GuestCallResult::Timeout { limit, .. } => {
                    self.metrics
                        .tools_invoked_err
                        .fetch_add(1, Ordering::Relaxed);
                    Err(RuntimeError::Timeout(limit))
                }
                GuestCallResult::SkippedExport { export, .. } => {
                    self.metrics
                        .tools_invoked_err
                        .fetch_add(1, Ordering::Relaxed);
                    Err(RuntimeError::MissingExport(export))
                }
                GuestCallResult::SkippedCapability { .. } => {
                    self.metrics
                        .tools_invoked_err
                        .fetch_add(1, Ordering::Relaxed);
                    Err(RuntimeError::Module("capability skipped".into()))
                }
            }
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = (extension, tool_name, args_json);
            Err(RuntimeError::WasmDisabled)
        }
    }

    async fn dispatch_all_observe(
        &self,
        call: GuestCall,
        timeout: Duration,
    ) -> Vec<GuestCallResult> {
        let mut out = Vec::with_capacity(self.guests.len());
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            let host = HostCtx {
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (r, _) = guest
                .inner
                .call_with_timeout_host(call, timeout, host)
                .await;
            self.metrics.record_call(&r);
            out.push(r);
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = (call, timeout);
        }
        out
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn tag_extension_out(mut out: BeforeAgentStartOut, name: &str) -> BeforeAgentStartOut {
    if let Some(ref mut s) = out.inject_context {
        *s = format!("[wasm:{name}] {s}");
    }
    if let Some(ref mut s) = out.append_system {
        *s = format!("[wasm:{name}] {s}");
    }
    out
}

#[derive(Debug, Clone, Copy)]
enum GuestCall {
    SessionStart,
    SessionEnd,
    PreToolUse,
    BeforeAgentStart,
    BeforeModel,
    Stop,
    PreCompact,
    ToolCount,
    DescribeTool,
    InvokeTool,
}

/// Outcome of one guest invocation (for UI / scrollback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestCallResult {
    Ok {
        extension: String,
        code: i32,
        /// Guest `hyper_host.log` lines from this call (capped).
        logs: Vec<GuestLogLine>,
    },
    SkippedExport { extension: String, export: &'static str },
    SkippedCapability {
        extension: String,
        capability: Capability,
    },
    Failed { extension: String, error: String },
    Timeout { extension: String, limit: Duration },
}

impl GuestCallResult {
    pub fn logs(&self) -> &[GuestLogLine] {
        match self {
            Self::Ok { logs, .. } => logs,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolDecision {
    Allow,
    Deny { extension: String, reason: String },
}

#[derive(Debug)]
pub struct PreToolDispatch {
    pub decision: PreToolDecision,
    pub results: Vec<(String, GuestCallResult)>,
}

/// Aggregated inject/append from all capable guests.
#[derive(Debug, Clone)]
pub struct BeforeAgentStartDispatch {
    pub out: BeforeAgentStartOut,
    pub results: Vec<(String, GuestCallResult)>,
}

impl BeforeAgentStartDispatch {
    pub fn has_injection(&self) -> bool {
        self.out.inject_context.is_some() || self.out.append_system.is_some()
    }
}

#[derive(Debug)]
pub struct StopDispatch {
    pub decision: StopOut,
    pub results: Vec<(String, GuestCallResult)>,
}

// ---------------------------------------------------------------------------
// wasmtime backend
// ---------------------------------------------------------------------------

/// Retained store + instance so guest globals/memory survive across calls
/// (Pi-like stateful lifecycle within one session runtime).
#[cfg(feature = "wasm")]
struct LiveGuest {
    store: wasmtime::Store<HostCtx>,
    instance: wasmtime::Instance,
}

#[cfg(feature = "wasm")]
#[derive(Clone)]
struct WasmGuest {
    name_for_logs: String,
    engine: wasmtime::Engine,
    module: Arc<wasmtime::Module>,
    /// Cached linker (host imports) — avoids re-registering funcs each call.
    linker: Arc<wasmtime::Linker<HostCtx>>,
    /// Session-retained instance (shared across [`ExtensionRuntime`] clones).
    live: Arc<std::sync::Mutex<Option<LiveGuest>>>,
}

#[cfg(feature = "wasm")]
impl WasmGuest {
    fn load(path: &Path) -> Result<Self, RuntimeError> {
        let bytes = std::fs::read(path).map_err(|source| RuntimeError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_bytes(
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("extension")
                .to_string(),
            &bytes,
        )
    }

    fn from_bytes(name_for_logs: String, bytes: &[u8]) -> Result<Self, RuntimeError> {
        // Cap module size to reduce compile/OOM risk (Oracle).
        const MAX_WASM_BYTES: usize = 8 * 1024 * 1024;
        if bytes.len() > MAX_WASM_BYTES {
            return Err(RuntimeError::Module(format!(
                "wasm module too large: {} bytes (max {MAX_WASM_BYTES})",
                bytes.len()
            )));
        }
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        // Wall-clock timeout can interrupt busy guest loops via epoch trap.
        config.epoch_interruption(true);
        let engine =
            wasmtime::Engine::new(&config).map_err(|e| RuntimeError::Module(e.to_string()))?;
        let module = wasmtime::Module::new(&engine, bytes)
            .map_err(|e| RuntimeError::Module(e.to_string()))?;
        let linker = Arc::new(build_linker(&engine)?);

        // Validate ABI at load time (fresh store; live instance created on first call).
        let mut store = wasmtime::Store::new(&engine, HostCtx::default());
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(1_000_000)
            .map_err(|e| RuntimeError::Module(e.to_string()))?;
        store.set_epoch_deadline(1);
        store.epoch_deadline_trap();
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| RuntimeError::Module(e.to_string()))?;
        let abi = instance
            .get_typed_func::<(), i32>(&mut store, EXPORT_ABI_VERSION)
            .map_err(|_| RuntimeError::MissingExport(EXPORT_ABI_VERSION))?;
        let got = abi
            .call(&mut store, ())
            .map_err(|e| RuntimeError::Trap(e.to_string()))?;
        if got != CORE_ABI_VERSION {
            return Err(RuntimeError::AbiMismatch { got });
        }
        let _ = instance
            .get_typed_func::<(), i32>(&mut store, EXPORT_ON_SESSION_START)
            .map_err(|_| RuntimeError::MissingExport(EXPORT_ON_SESSION_START))?;

        Ok(Self {
            name_for_logs,
            engine,
            module: Arc::new(module),
            linker,
            live: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    fn apply_host_inputs(data: &mut HostCtx, host: HostCtx, guest_name: &str) {
        // Keep resource limits attached to the store; only swap call inputs/outputs.
        let limits = std::mem::replace(&mut data.limits, HostCtx::default().limits);
        let plugin_data_dir = host.plugin_data_dir.clone();
        *data = host;
        data.limits = limits;
        data.guest_name = guest_name.to_string();
        data.plugin_data_dir = plugin_data_dir;
        data.inject_context.clear();
        data.append_system.clear();
        data.gate_reason.clear();
        data.tool_name_out.clear();
        data.tool_description_out.clear();
        data.tool_schema_out.clear();
        data.tool_result_out.clear();
        data.guest_logs.clear();
    }

    fn take_host_outputs(data: &mut HostCtx) -> HostCtx {
        HostCtx {
            guest_name: std::mem::take(&mut data.guest_name),
            plugin_data_dir: std::mem::take(&mut data.plugin_data_dir),
            tool_name: std::mem::take(&mut data.tool_name),
            tool_input: std::mem::take(&mut data.tool_input),
            prompt: std::mem::take(&mut data.prompt),
            inject_context: std::mem::take(&mut data.inject_context),
            append_system: std::mem::take(&mut data.append_system),
            stop_hook_active: data.stop_hook_active,
            compact_reason: std::mem::take(&mut data.compact_reason),
            gate_reason: std::mem::take(&mut data.gate_reason),
            tool_index: data.tool_index,
            tool_name_out: std::mem::take(&mut data.tool_name_out),
            tool_description_out: std::mem::take(&mut data.tool_description_out),
            tool_schema_out: std::mem::take(&mut data.tool_schema_out),
            tool_result_out: std::mem::take(&mut data.tool_result_out),
            guest_logs: std::mem::take(&mut data.guest_logs),
            limits: HostCtx::default().limits,
        }
    }

    async fn call_with_timeout_host(
        &self,
        call: GuestCall,
        limit: Duration,
        host: HostCtx,
    ) -> (GuestCallResult, HostCtx) {
        let engine = self.engine.clone();
        let module = Arc::clone(&self.module);
        let linker = Arc::clone(&self.linker);
        let live = Arc::clone(&self.live);
        let name = self.name_for_logs.clone();
        let export = match call {
            GuestCall::SessionStart => EXPORT_ON_SESSION_START,
            GuestCall::SessionEnd => EXPORT_ON_SESSION_END,
            GuestCall::PreToolUse => EXPORT_ON_PRE_TOOL_USE,
            GuestCall::BeforeAgentStart => EXPORT_ON_BEFORE_AGENT_START,
            GuestCall::BeforeModel => EXPORT_ON_BEFORE_MODEL,
            GuestCall::Stop => EXPORT_ON_STOP,
            GuestCall::PreCompact => EXPORT_ON_PRE_COMPACT,
            GuestCall::ToolCount => EXPORT_TOOL_COUNT,
            GuestCall::DescribeTool => EXPORT_DESCRIBE_TOOL,
            GuestCall::InvokeTool => EXPORT_INVOKE_TOOL,
        };

        // If this future is dropped (cancel/shutdown) while the guest is still
        // running, increment the epoch so fuel/epoch traps release the live mutex.
        let mut cancel_guard = EpochCancelGuard {
            engine: self.engine.clone(),
            armed: true,
        };

        let mut join = tokio::task::spawn_blocking(move || {
            let mut guard = match live.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };

            if guard.is_none() {
                let mut store = wasmtime::Store::new(&engine, HostCtx::default());
                store.limiter(|s| &mut s.limits);
                store.epoch_deadline_trap();
                let instance = match linker.instantiate(&mut store, &module) {
                    Ok(i) => i,
                    Err(e) => {
                        return (
                            GuestCallResult::Failed {
                                extension: name,
                                error: e.to_string(),
                            },
                            host,
                        );
                    }
                };
                *guard = Some(LiveGuest { store, instance });
            }

            let live_guest = guard.as_mut().expect("just ensured");
            Self::apply_host_inputs(live_guest.store.data_mut(), host, &name);

            if let Err(e) = live_guest.store.set_fuel(10_000_000) {
                let host_out = Self::take_host_outputs(live_guest.store.data_mut());
                return (
                    GuestCallResult::Failed {
                        extension: name,
                        error: e.to_string(),
                    },
                    host_out,
                );
            }
            // Trap after one epoch increment (timeout or Drop cancel path).
            live_guest.store.set_epoch_deadline(1);

            let func = match live_guest
                .instance
                .get_typed_func::<(), i32>(&mut live_guest.store, export)
            {
                Ok(f) => f,
                Err(_) => {
                    let host_out = Self::take_host_outputs(live_guest.store.data_mut());
                    return (
                        GuestCallResult::SkippedExport {
                            extension: name,
                            export,
                        },
                        host_out,
                    );
                }
            };
            let call_result = func.call(&mut live_guest.store, ());
            let host_out = Self::take_host_outputs(live_guest.store.data_mut());
            let result = match call_result {
                Ok(code) => GuestCallResult::Ok {
                    extension: name,
                    code,
                    logs: host_out.guest_logs.clone(),
                },
                Err(e) => GuestCallResult::Failed {
                    extension: name,
                    error: e.to_string(),
                },
            };
            (result, host_out)
        });

        // Keep the JoinHandle on timeout so we can epoch-interrupt and await
        // the trap (dropping the handle would detach and leak the mutex hold).
        let out = tokio::select! {
            r = &mut join => {
                match r {
                    Ok(pair) => pair,
                    Err(join_err) => (
                        GuestCallResult::Failed {
                            extension: self.name_for_logs.clone(),
                            error: join_err.to_string(),
                        },
                        HostCtx::default(),
                    ),
                }
            }
            _ = tokio::time::sleep(limit) => {
                self.engine.increment_epoch();
                let grace = Duration::from_millis(250);
                match tokio::time::timeout(grace, join).await {
                    Ok(Ok((result, host_out))) => match result {
                        GuestCallResult::Failed { extension, .. } => (
                            GuestCallResult::Timeout {
                                extension,
                                limit,
                            },
                            host_out,
                        ),
                        other => (other, host_out),
                    },
                    Ok(Err(join_err)) => (
                        GuestCallResult::Failed {
                            extension: self.name_for_logs.clone(),
                            error: join_err.to_string(),
                        },
                        HostCtx::default(),
                    ),
                    Err(_) => (
                        GuestCallResult::Timeout {
                            extension: self.name_for_logs.clone(),
                            limit,
                        },
                        HostCtx::default(),
                    ),
                }
            }
        };
        cancel_guard.disarm();
        out
    }
}

/// Increments the engine epoch when dropped while still armed so a cancelled
/// async caller can interrupt a busy guest (Oracle H3).
#[cfg(feature = "wasm")]
struct EpochCancelGuard {
    engine: wasmtime::Engine,
    armed: bool,
}

#[cfg(feature = "wasm")]
impl EpochCancelGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(feature = "wasm")]
impl Drop for EpochCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.engine.increment_epoch();
        }
    }
}

#[cfg(feature = "wasm")]
fn build_linker(engine: &wasmtime::Engine) -> Result<wasmtime::Linker<HostCtx>, RuntimeError> {
    let mut linker = wasmtime::Linker::new(engine);
    linker
        .func_wrap(
            "hyper_host",
            "tool_name_len",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 {
                caller.data().tool_name.len() as i32
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "tool_name_byte",
            |caller: wasmtime::Caller<'_, HostCtx>, idx: i32| -> i32 {
                byte_at(&caller.data().tool_name, idx)
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "input_len",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 {
                caller.data().tool_input.len() as i32
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "input_byte",
            |caller: wasmtime::Caller<'_, HostCtx>, idx: i32| -> i32 {
                byte_at(&caller.data().tool_input, idx)
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "prompt_len",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 {
                caller.data().prompt.len() as i32
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "prompt_byte",
            |caller: wasmtime::Caller<'_, HostCtx>, idx: i32| -> i32 {
                byte_at(&caller.data().prompt, idx)
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "set_inject_context",
            |mut caller: wasmtime::Caller<'_, HostCtx>, ptr: i32, len: i32| {
                if let Some(s) = read_guest_utf8(&mut caller, ptr, len) {
                    caller.data_mut().inject_context = s;
                }
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "set_append_system",
            |mut caller: wasmtime::Caller<'_, HostCtx>, ptr: i32, len: i32| {
                if let Some(s) = read_guest_utf8(&mut caller, ptr, len) {
                    caller.data_mut().append_system = s;
                }
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "set_gate_reason",
            |mut caller: wasmtime::Caller<'_, HostCtx>, ptr: i32, len: i32| {
                if let Some(s) = read_guest_utf8(&mut caller, ptr, len) {
                    caller.data_mut().gate_reason = s;
                }
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    // Guest → host log (production observability; partial UI Host API).
    // level: 0=debug 1=info 2=warn 3=error; msg is UTF-8 in guest memory.
    linker
        .func_wrap(
            "hyper_host",
            "log",
            |mut caller: wasmtime::Caller<'_, HostCtx>, level: i32, ptr: i32, len: i32| {
                let Some(mut msg) = read_guest_utf8(&mut caller, ptr, len) else {
                    return;
                };
                // Cap message size (same ballpark as inject).
                if msg.len() > MAX_INJECT_BYTES {
                    msg.truncate(MAX_INJECT_BYTES);
                }
                let lvl = GuestLogLevel::from_i32(level);
                let guest = caller.data().guest_name.clone();
                match lvl {
                    GuestLogLevel::Debug => {
                        tracing::debug!(target: "wasm_extension", extension = %guest, "{msg}");
                    }
                    GuestLogLevel::Info => {
                        tracing::info!(target: "wasm_extension", extension = %guest, "{msg}");
                    }
                    GuestLogLevel::Warn => {
                        tracing::warn!(target: "wasm_extension", extension = %guest, "{msg}");
                    }
                    GuestLogLevel::Error => {
                        tracing::error!(target: "wasm_extension", extension = %guest, "{msg}");
                    }
                }
                let logs = &mut caller.data_mut().guest_logs;
                if logs.len() < 64 {
                    logs.push(GuestLogLine {
                        level: lvl,
                        message: msg,
                    });
                }
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "plugin_data_dir_len",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 {
                caller.data().plugin_data_dir.len() as i32
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "plugin_data_dir_byte",
            |caller: wasmtime::Caller<'_, HostCtx>, idx: i32| -> i32 {
                byte_at(&caller.data().plugin_data_dir, idx)
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "stop_hook_active",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 {
                i32::from(caller.data().stop_hook_active)
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "compact_reason_len",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 {
                caller.data().compact_reason.len() as i32
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "compact_reason_byte",
            |caller: wasmtime::Caller<'_, HostCtx>, idx: i32| -> i32 {
                byte_at(&caller.data().compact_reason, idx)
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "tool_index",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 { caller.data().tool_index },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "set_tool_name",
            |mut caller: wasmtime::Caller<'_, HostCtx>, ptr: i32, len: i32| {
                if let Some(s) = read_guest_utf8(&mut caller, ptr, len) {
                    caller.data_mut().tool_name_out = s;
                }
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "set_tool_description",
            |mut caller: wasmtime::Caller<'_, HostCtx>, ptr: i32, len: i32| {
                if let Some(s) = read_guest_utf8(&mut caller, ptr, len) {
                    caller.data_mut().tool_description_out = s;
                }
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "set_tool_schema",
            |mut caller: wasmtime::Caller<'_, HostCtx>, ptr: i32, len: i32| {
                if let Some(s) = read_guest_utf8(&mut caller, ptr, len) {
                    caller.data_mut().tool_schema_out = s;
                }
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "set_tool_result",
            |mut caller: wasmtime::Caller<'_, HostCtx>, ptr: i32, len: i32| {
                if let Some(s) = read_guest_utf8(&mut caller, ptr, len) {
                    caller.data_mut().tool_result_out = s;
                }
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    Ok(linker)
}

fn byte_at(s: &str, idx: i32) -> i32 {
    if idx < 0 {
        return -1;
    }
    s.as_bytes()
        .get(idx as usize)
        .copied()
        .map(|b| b as i32)
        .unwrap_or(-1)
}

#[cfg(feature = "wasm")]
fn read_guest_utf8(
    caller: &mut wasmtime::Caller<'_, HostCtx>,
    ptr: i32,
    len: i32,
) -> Option<String> {
    if ptr < 0 || len < 0 {
        return None;
    }
    let len = (len as usize).min(MAX_INJECT_BYTES);
    let mem = caller.get_export("memory")?.into_memory()?;
    let data = mem.data(caller);
    let start = ptr as usize;
    let end = start.checked_add(len)?;
    let slice = data.get(start..end)?;
    // Lossy so a bad guest cannot trap the host on invalid UTF-8.
    Some(String::from_utf8_lossy(slice).into_owned())
}

#[cfg(test)]
pub fn wat_to_wasm(wat: &str) -> Result<Vec<u8>, String> {
    wat::parse_str(wat).map_err(|e| e.to_string())
}

#[cfg(all(test, feature = "wasm"))]
mod tests {
    use super::*;
    use std::io::Write;

    const MINIMAL_GUEST: &str = r#"
        (module
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_session_end") (result i32)
            i32.const 0)
        )
    "#;

    /// Denies when tool input contains ASCII `rm -rf` (naive substring).
    const SAFE_SHELL_GUEST: &str = r#"
        (module
          (import "hyper_host" "input_len" (func $input_len (result i32)))
          (import "hyper_host" "input_byte" (func $input_byte (param i32) (result i32)))
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_pre_tool_use") (result i32)
            (local $i i32)
            (local $n i32)
            (local $b0 i32) (local $b1 i32) (local $b2 i32)
            (local $b3 i32) (local $b4 i32) (local $b5 i32)
            (local.set $n (call $input_len))
            (local.set $i (i32.const 0))
            (block $done
              (loop $scan
                (br_if $done (i32.ge_s (local.get $i) (local.get $n)))
                ;; look for "rm -rf" = 72 6d 20 2d 72 66
                (local.set $b0 (call $input_byte (local.get $i)))
                (local.set $b1 (call $input_byte (i32.add (local.get $i) (i32.const 1))))
                (local.set $b2 (call $input_byte (i32.add (local.get $i) (i32.const 2))))
                (local.set $b3 (call $input_byte (i32.add (local.get $i) (i32.const 3))))
                (local.set $b4 (call $input_byte (i32.add (local.get $i) (i32.const 4))))
                (local.set $b5 (call $input_byte (i32.add (local.get $i) (i32.const 5))))
                (if (i32.and
                      (i32.and
                        (i32.and (i32.eq (local.get $b0) (i32.const 0x72))
                                 (i32.eq (local.get $b1) (i32.const 0x6d)))
                        (i32.and (i32.eq (local.get $b2) (i32.const 0x20))
                                 (i32.eq (local.get $b3) (i32.const 0x2d))))
                      (i32.and (i32.eq (local.get $b4) (i32.const 0x72))
                               (i32.eq (local.get $b5) (i32.const 0x66))))
                  (then (return (i32.const 1))))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (br $scan)
              )
            )
            i32.const 0
          )
        )
    "#;

    const BAD_ABI_GUEST: &str = r#"
        (module
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 99)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
        )
    "#;

    const TRAP_GUEST: &str = r#"
        (module
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            unreachable)
        )
    "#;

    const DENY_WITH_REASON: &str = r#"
        (module
          (import "hyper_host" "set_gate_reason" (func $set_reason (param i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "custom-deny-reason")
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_pre_tool_use") (result i32)
            (call $set_reason (i32.const 0) (i32.const 18))
            i32.const 1)
        )
    "#;

    const TRAP_ON_PRE_TOOL: &str = r#"
        (module
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_pre_tool_use") (result i32)
            unreachable)
        )
    "#;

    fn write_wasm(dir: &tempfile::TempDir, name: &str, wat: &str) -> PathBuf {
        let path = dir.path().join(name);
        let bytes = wat_to_wasm(wat).expect("wat");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&bytes).unwrap();
        path
    }

    fn trusted_spec(name: &str, path: PathBuf, caps: Vec<Capability>) -> ExtensionSpec {
        ExtensionSpec {
            name: name.into(),
            wasm_path: path,
            capabilities: caps,
            trusted: true,
            gate_fail: None,
            plugin_data_dir: None,
        }
    }

    fn trusted_spec_gate(
        name: &str,
        path: PathBuf,
        caps: Vec<Capability>,
        gate_fail: GateFailMode,
    ) -> ExtensionSpec {
        ExtensionSpec {
            name: name.into(),
            wasm_path: path,
            capabilities: caps,
            trusted: true,
            gate_fail: Some(gate_fail),
            plugin_data_dir: None,
        }
    }

    #[tokio::test]
    async fn load_minimal_and_session_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "ok.wasm", MINIMAL_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec("ok", path, vec![])).unwrap();
        assert_eq!(rt.len(), 1);
        let results = rt.dispatch_session_start().await;
        assert!(matches!(&results[0], GuestCallResult::Ok { code: 0, .. }));
    }

    #[tokio::test]
    async fn reject_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "x.wasm", MINIMAL_GUEST);
        let mut rt = ExtensionRuntime::new();
        let mut spec = trusted_spec("x", path, vec![]);
        spec.trusted = false;
        let err = rt.load(&spec).unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Contract(ContractError::NotTrusted)
        ));
    }

    #[tokio::test]
    async fn reject_bad_abi() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "bad.wasm", BAD_ABI_GUEST);
        let mut rt = ExtensionRuntime::new();
        let err = rt.load(&trusted_spec("bad", path, vec![])).unwrap_err();
        assert!(matches!(err, RuntimeError::AbiMismatch { got: 99 }));
    }

    #[tokio::test]
    async fn trap_on_session_start_is_fail_open_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "trap.wasm", TRAP_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec("trap", path, vec![])).unwrap();
        let results = rt.dispatch_session_start().await;
        assert!(matches!(results[0], GuestCallResult::Failed { .. }));
    }

    #[tokio::test]
    async fn deny_with_custom_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "deny.wasm", DENY_WITH_REASON);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "pol",
            path,
            vec![Capability::PreToolGate],
        ))
        .unwrap();
        let d = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "run_terminal_command".into(),
                tool_input_json: "{}".into(),
            })
            .await;
        match d.decision {
            PreToolDecision::Deny { reason, .. } => {
                assert!(reason.contains("custom-deny-reason"), "{reason}");
            }
            _ => panic!("expected deny"),
        }
    }

    #[tokio::test]
    async fn fail_closed_denies_on_trap() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "trap-tool.wasm", TRAP_ON_PRE_TOOL);
        let mut rt = ExtensionRuntime::new().with_gate_fail(GateFailMode::Closed);
        rt.load(&trusted_spec(
            "trap",
            path,
            vec![Capability::PreToolGate],
        ))
        .unwrap();
        let d = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "x".into(),
                tool_input_json: "{}".into(),
            })
            .await;
        assert!(matches!(d.decision, PreToolDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn fail_open_allows_on_trap() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "trap-tool2.wasm", TRAP_ON_PRE_TOOL);
        let mut rt = ExtensionRuntime::new().with_gate_fail(GateFailMode::Open);
        rt.load(&trusted_spec(
            "trap",
            path,
            vec![Capability::PreToolGate],
        ))
        .unwrap();
        let d = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "x".into(),
                tool_input_json: "{}".into(),
            })
            .await;
        assert!(matches!(d.decision, PreToolDecision::Allow));
    }

    /// Registers one "echo" tool that returns the input JSON.
    const ECHO_TOOL_GUEST: &str = r#"
        (module
          (import "hyper_host" "tool_index" (func $tool_index (result i32)))
          (import "hyper_host" "set_tool_name" (func $set_name (param i32 i32)))
          (import "hyper_host" "set_tool_description" (func $set_desc (param i32 i32)))
          (import "hyper_host" "set_tool_schema" (func $set_schema (param i32 i32)))
          (import "hyper_host" "set_tool_result" (func $set_result (param i32 i32)))
          (import "hyper_host" "input_len" (func $input_len (result i32)))
          (import "hyper_host" "input_byte" (func $input_byte (param i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "echo")
          (data (i32.const 16) "Echo args JSON back")
          (data (i32.const 48) "{\"type\":\"object\",\"properties\":{}}")
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_tool_count") (result i32)
            i32.const 1)
          (func (export "hyper_ext_describe_tool") (result i32)
            (call $set_name (i32.const 0) (i32.const 4))
            (call $set_desc (i32.const 16) (i32.const 19))
            (call $set_schema (i32.const 48) (i32.const 33))
            i32.const 0)
          (func (export "hyper_ext_invoke_tool") (result i32)
            (local $i i32) (local $n i32) (local $b i32)
            ;; copy input into memory at 128
            (local.set $n (call $input_len))
            (if (i32.gt_s (local.get $n) (i32.const 256))
              (then (local.set $n (i32.const 256))))
            (local.set $i (i32.const 0))
            (block $done
              (loop $copy
                (br_if $done (i32.ge_s (local.get $i) (local.get $n)))
                (local.set $b (call $input_byte (local.get $i)))
                (i32.store8 (i32.add (i32.const 128) (local.get $i)) (local.get $b))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (br $copy)
              )
            )
            (call $set_result (i32.const 128) (local.get $n))
            i32.const 0)
        )
    "#;

    #[tokio::test]
    async fn register_and_invoke_echo_tool() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "echo.wasm", ECHO_TOOL_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "echo-ext",
            path,
            vec![Capability::RegisterTool],
        ))
        .unwrap();
        let tools = rt.collect_registered_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].client_name(), "wasm_echo-ext_echo");
        let out = rt
            .invoke_registered_tool("echo-ext", "echo", r#"{"x":1}"#)
            .await
            .unwrap();
        assert!(out.contains("\"x\""), "{out}");
    }

    #[tokio::test]
    async fn invoke_rejects_oversized_payload_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "echo2.wasm", ECHO_TOOL_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "echo-ext",
            path,
            vec![Capability::RegisterTool],
        ))
        .unwrap();
        let _ = rt.collect_registered_tools().await;
        // Multibyte UTF-8 so a naïve byte truncate would panic mid-codepoint.
        let mut body = "é".repeat(MAX_TOOL_PAYLOAD_BYTES);
        body.push('x');
        let args = format!(r#"{{"msg":"{body}"}}"#);
        assert!(args.len() > MAX_TOOL_PAYLOAD_BYTES);
        let err = rt
            .invoke_registered_tool("echo-ext", "echo", &args)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::PayloadTooLarge { .. }),
            "expected PayloadTooLarge, got {err:?}"
        );
    }

    #[tokio::test]
    async fn invoke_rejects_unknown_and_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "echo3.wasm", ECHO_TOOL_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "echo-ext",
            path,
            vec![Capability::RegisterTool],
        ))
        .unwrap();
        let _ = rt.collect_registered_tools().await;
        let err = rt
            .invoke_registered_tool("echo-ext", "not_a_tool", "{}")
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::UnknownTool(_, _)), "{err:?}");
        let err = rt
            .invoke_registered_tool("echo-ext", "echo", "not-json")
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::InvalidToolArgs), "{err:?}");
    }

    #[tokio::test]
    async fn e2e_load_checked_in_rust_template_wasm() {
        // Integration-style: load the official Rust template's extension.wasm
        // from the examples tree (checked into git).
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples/rust-guest-template/extension.wasm");
        if !path.is_file() {
            eprintln!("skip: no rust-guest-template/extension.wasm");
            return;
        }
        ExtensionRuntime::validate_wasm_file(&path).expect("validate load");
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "rust-guest-template",
            path,
            vec![
                Capability::PreToolGate,
                Capability::BeforeAgentInject,
                Capability::RegisterTool,
            ],
        ))
        .unwrap();
        let deny = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "run_terminal_command".into(),
                tool_input_json: r#"{"command":"rm -rf /tmp"}"#.into(),
            })
            .await;
        assert!(matches!(deny.decision, PreToolDecision::Deny { .. }));
        let allow = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "run_terminal_command".into(),
                tool_input_json: r#"{"command":"ls"}"#.into(),
            })
            .await;
        assert!(matches!(allow.decision, PreToolDecision::Allow));
        let inj = rt
            .dispatch_before_agent_start(&BeforeAgentStartIn {
                prompt: "hi".into(),
            })
            .await;
        assert!(inj.has_injection());
        let tools = rt.collect_registered_tools().await;
        assert!(
            tools.iter().any(|t| t.name == "echo"),
            "template should register echo tool: {tools:?}"
        );
    }

    #[tokio::test]
    async fn e2e_sdk_path_guard_and_stop_once() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
        let guard = root.join("sdk-path-guard/extension.wasm");
        let stop = root.join("sdk-stop-once/extension.wasm");
        if !guard.is_file() || !stop.is_file() {
            eprintln!("skip: run scripts/check-extensions.sh to build example wasm");
            return;
        }
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "sdk-path-guard",
            guard,
            vec![Capability::PreToolGate],
        ))
        .unwrap();
        rt.load(&trusted_spec(
            "sdk-stop-once",
            stop,
            vec![Capability::StopGate],
        ))
        .unwrap();
        let deny = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "run_terminal_command".into(),
                tool_input_json: r#"{"command":"mkfs.ext4 /dev/sda"}"#.into(),
            })
            .await;
        assert!(matches!(deny.decision, PreToolDecision::Deny { .. }));
        let block = rt
            .dispatch_stop(&StopIn {
                stop_hook_active: false,
            })
            .await;
        assert!(matches!(block.decision, StopOut::Block { .. }));
        let cont = rt
            .dispatch_stop(&StopIn {
                stop_hook_active: true,
            })
            .await;
        assert!(matches!(cont.decision, StopOut::Continue));
    }

    #[tokio::test]
    async fn safe_shell_denies_rm_rf() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "safe.wasm", SAFE_SHELL_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "safe-shell",
            path,
            vec![Capability::PreToolGate],
        ))
        .unwrap();
        let deny = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "run_terminal_command".into(),
                tool_input_json: r#"{"command":"rm -rf /tmp/x"}"#.into(),
            })
            .await;
        assert!(matches!(deny.decision, PreToolDecision::Deny { .. }));

        let allow = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "run_terminal_command".into(),
                tool_input_json: r#"{"command":"ls -la"}"#.into(),
            })
            .await;
        assert!(matches!(allow.decision, PreToolDecision::Allow));
    }

    #[tokio::test]
    async fn pre_tool_without_capability_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "safe.wasm", SAFE_SHELL_GUEST);
        let mut rt = ExtensionRuntime::new();
        // Module can deny, but capability not granted → skipped → allow.
        rt.load(&trusted_spec("safe-shell", path, vec![]))
            .unwrap();
        let d = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "run_terminal_command".into(),
                tool_input_json: r#"{"command":"rm -rf /"}"#.into(),
            })
            .await;
        assert!(matches!(d.decision, PreToolDecision::Allow));
    }

    /// Static inject via guest memory + set_inject_context.
    const INJECT_GUEST: &str = r#"
        (module
          (import "hyper_host" "set_inject_context" (func $set_inject (param i32 i32)))
          (import "hyper_host" "set_append_system" (func $set_append (param i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "policy: no secrets in logs")
          (data (i32.const 32) "ext-system-note")
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_before_agent_start") (result i32)
            (call $set_inject (i32.const 0) (i32.const 26))
            (call $set_append (i32.const 32) (i32.const 15))
            i32.const 0)
        )
    "#;

    #[tokio::test]
    async fn before_agent_start_injects_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "inject.wasm", INJECT_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "policy",
            path,
            vec![Capability::BeforeAgentInject],
        ))
        .unwrap();
        let d = rt
            .dispatch_before_agent_start(&BeforeAgentStartIn {
                prompt: "hello".into(),
            })
            .await;
        assert!(d.has_injection());
        let inj = d.out.inject_context.unwrap();
        assert!(inj.contains("policy: no secrets in logs"), "{inj}");
        assert!(inj.contains("[wasm:policy]"), "{inj}");
        let app = d.out.append_system.unwrap();
        assert!(app.contains("ext-system-note"), "{app}");
    }

    const STOP_BLOCK_GUEST: &str = r#"
        (module
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_stop") (result i32)
            i32.const 1)
        )
    "#;

    #[tokio::test]
    async fn stop_gate_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "stop.wasm", STOP_BLOCK_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec("stopper", path, vec![Capability::StopGate]))
            .unwrap();
        assert!(rt.has_capability(Capability::StopGate));
        let d = rt
            .dispatch_stop(&StopIn {
                stop_hook_active: false,
            })
            .await;
        assert!(matches!(d.decision, StopOut::Block { .. }));
    }

    #[test]
    fn from_bytes_missing_export() {
        let wat = r#"(module (func (export "hyper_ext_abi_version") (result i32) i32.const 1))"#;
        let bytes = wat_to_wasm(wat).unwrap();
        match WasmGuest::from_bytes("m".into(), &bytes) {
            Ok(_) => panic!("expected MissingExport(session_start), got Ok"),
            Err(RuntimeError::MissingExport(EXPORT_ON_SESSION_START)) => {}
            Err(e) => panic!("expected MissingExport(session_start), got {e:?}"),
        }
    }

    /// Global counter increments across calls only when the instance is retained.
    const STATEFUL_COUNTER_GUEST: &str = r#"
        (module
          (import "hyper_host" "set_tool_result" (func $set_result (param i32 i32)))
          (import "hyper_host" "set_tool_name" (func $set_name (param i32 i32)))
          (import "hyper_host" "set_tool_description" (func $set_desc (param i32 i32)))
          (import "hyper_host" "set_tool_schema" (func $set_schema (param i32 i32)))
          (memory (export "memory") 1)
          (global $count (mut i32) (i32.const 0))
          (data (i32.const 0) "counter")
          (data (i32.const 16) "stateful counter")
          (data (i32.const 48) "{\"type\":\"object\",\"properties\":{}}")
          (data (i32.const 96) "1")
          (data (i32.const 98) "2")
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_tool_count") (result i32)
            i32.const 1)
          (func (export "hyper_ext_describe_tool") (result i32)
            (call $set_name (i32.const 0) (i32.const 7))
            (call $set_desc (i32.const 16) (i32.const 16))
            (call $set_schema (i32.const 48) (i32.const 33))
            i32.const 0)
          (func (export "hyper_ext_invoke_tool") (result i32)
            (global.set $count (i32.add (global.get $count) (i32.const 1)))
            (if (i32.eq (global.get $count) (i32.const 1))
              (then (call $set_result (i32.const 96) (i32.const 1)))
              (else (call $set_result (i32.const 98) (i32.const 1))))
            i32.const 0)
        )
    "#;

    #[tokio::test]
    async fn retained_instance_preserves_guest_globals() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "state.wasm", STATEFUL_COUNTER_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "stateful",
            path,
            vec![Capability::RegisterTool],
        ))
        .unwrap();
        let a = rt
            .invoke_registered_tool("stateful", "counter", "{}")
            .await
            .unwrap();
        let b = rt
            .invoke_registered_tool("stateful", "counter", "{}")
            .await
            .unwrap();
        assert_eq!(a, "1", "first invoke should see count=1");
        assert_eq!(b, "2", "second invoke should see retained count=2, got {b}");
    }

    #[tokio::test]
    async fn per_extension_fail_closed_overrides_runtime_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "trap-tool3.wasm", TRAP_ON_PRE_TOOL);
        // Runtime default open, guest manifest closed → deny on trap.
        let mut rt = ExtensionRuntime::new().with_gate_fail(GateFailMode::Open);
        rt.load(&trusted_spec_gate(
            "trap",
            path,
            vec![Capability::PreToolGate],
            GateFailMode::Closed,
        ))
        .unwrap();
        let d = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "x".into(),
                tool_input_json: "{}".into(),
            })
            .await;
        assert!(matches!(d.decision, PreToolDecision::Deny { .. }));
    }

    const BAD_TOOL_NAME_GUEST: &str = r#"
        (module
          (import "hyper_host" "set_tool_name" (func $set_name (param i32 i32)))
          (import "hyper_host" "set_tool_schema" (func $set_schema (param i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "bad name")
          (data (i32.const 16) "not-json")
          (func (export "hyper_ext_abi_version") (result i32) i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32) i32.const 0)
          (func (export "hyper_ext_tool_count") (result i32) i32.const 1)
          (func (export "hyper_ext_describe_tool") (result i32)
            (call $set_name (i32.const 0) (i32.const 8))
            (call $set_schema (i32.const 16) (i32.const 8))
            i32.const 0)
        )
    "#;

    #[tokio::test]
    async fn collect_tools_skips_invalid_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "bad-tool.wasm", BAD_TOOL_NAME_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "bad",
            path,
            vec![Capability::RegisterTool],
        ))
        .unwrap();
        let tools = rt.collect_registered_tools().await;
        assert!(tools.is_empty(), "invalid tool should be skipped: {tools:?}");
    }

    /// Busy loop until fuel/epoch kills it (no host imports).
    const INFINITE_LOOP_GUEST: &str = r#"
        (module
          (func (export "hyper_ext_abi_version") (result i32) i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            (loop $forever (br $forever))
            i32.const 0)
        )
    "#;

    #[tokio::test]
    async fn busy_guest_is_bounded_by_fuel_or_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "loop.wasm", INFINITE_LOOP_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec("loop", path, vec![])).unwrap();
        let start = std::time::Instant::now();
        let results = rt.dispatch_session_start().await;
        // Fuel often wins first on a tight loop; epoch path also maps to Timeout.
        assert!(
            matches!(
                &results[0],
                GuestCallResult::Timeout { .. } | GuestCallResult::Failed { .. }
            ),
            "expected Timeout or Failed, got {:?}",
            results[0]
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "bounded path too slow: {:?}",
            start.elapsed()
        );
    }

    /// Phase 4 budget: load + session_start for N=5 minimal guests should stay
    /// well under a second on a normal debug build (design target ~100ms for
    /// release; we only enforce a soft CI ceiling here).
    /// Guest emits a log line via `hyper_host.log` during session_start.
    const LOG_GUEST: &str = r#"
        (module
          (import "hyper_host" "log" (func $log (param i32 i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "hello-from-guest")
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            ;; level=1 (info), ptr=0, len=16
            (call $log (i32.const 1) (i32.const 0) (i32.const 16))
            i32.const 0)
        )
    "#;

    #[tokio::test]
    async fn guest_host_log_is_captured() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "log.wasm", LOG_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec("logger", path, vec![])).unwrap();
        let results = rt.dispatch_session_start().await;
        match &results[0] {
            GuestCallResult::Ok { code: 0, logs, .. } => {
                assert_eq!(logs.len(), 1, "{logs:?}");
                assert_eq!(logs[0].level, GuestLogLevel::Info);
                assert_eq!(logs[0].message, "hello-from-guest");
            }
            other => panic!("expected Ok with logs, got {other:?}"),
        }
        let m = rt.metrics();
        assert!(m.loads_ok >= 1);
        assert!(m.calls_ok >= 1);
        assert!(m.guest_log_lines >= 1);
    }

    /// Reads plugin_data_dir and writes it to set_gate_reason (via deny path).
    const DATA_DIR_GUEST: &str = r#"
        (module
          (import "hyper_host" "plugin_data_dir_len" (func $dir_len (result i32)))
          (import "hyper_host" "plugin_data_dir_byte" (func $dir_byte (param i32) (result i32)))
          (import "hyper_host" "set_gate_reason" (func $set_reason (param i32 i32)))
          (memory (export "memory") 1)
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_pre_tool_use") (result i32)
            (local $i i32) (local $n i32) (local $b i32)
            (local.set $n (call $dir_len))
            (if (i32.gt_s (local.get $n) (i32.const 200))
              (then (local.set $n (i32.const 200))))
            (local.set $i (i32.const 0))
            (block $done
              (loop $copy
                (br_if $done (i32.ge_s (local.get $i) (local.get $n)))
                (local.set $b (call $dir_byte (local.get $i)))
                (i32.store8 (local.get $i) (local.get $b))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (br $copy)
              )
            )
            (call $set_reason (i32.const 0) (local.get $n))
            i32.const 1)
        )
    "#;

    #[tokio::test]
    async fn plugin_data_dir_visible_to_guest() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "datadir.wasm", DATA_DIR_GUEST);
        let data = dir.path().join("plugin-data");
        std::fs::create_dir_all(&data).unwrap();
        let mut rt = ExtensionRuntime::new();
        let mut spec = trusted_spec("data-guest", path, vec![Capability::PreToolGate]);
        spec.plugin_data_dir = Some(data.clone());
        rt.load(&spec).unwrap();
        let d = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "x".into(),
                tool_input_json: "{}".into(),
            })
            .await;
        match d.decision {
            PreToolDecision::Deny { reason, .. } => {
                assert!(
                    reason.contains("plugin-data") || reason.contains(&*data.to_string_lossy()),
                    "reason should include data dir path, got {reason}"
                );
            }
            other => panic!("expected deny with data dir reason, got {other:?}"),
        }
        assert_eq!(rt.metrics().pre_tool_denies, 1);
    }

    #[tokio::test]
    async fn metrics_count_load_and_deny() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "trap-m.wasm", TRAP_ON_PRE_TOOL);
        let mut rt = ExtensionRuntime::new().with_gate_fail(GateFailMode::Closed);
        rt.load(&trusted_spec(
            "trap",
            path,
            vec![Capability::PreToolGate],
        ))
        .unwrap();
        assert_eq!(rt.metrics().loads_ok, 1);
        let _ = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "x".into(),
                tool_input_json: "{}".into(),
            })
            .await;
        let m = rt.metrics();
        assert!(m.calls_failed >= 1 || m.calls_timeout >= 1, "{m:?}");
        assert_eq!(m.pre_tool_denies, 1);
        assert!(m.has_failures() || m.pre_tool_denies > 0);
        assert!(m.to_string().contains("loads_ok=1"), "{m}");
        // Smoke: structured log path does not panic.
        rt.log_metrics("unit_test");
    }

    #[tokio::test]
    async fn load_five_minimal_guests_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = ExtensionRuntime::new();
        let t0 = std::time::Instant::now();
        for i in 0..5 {
            let path = write_wasm(&dir, &format!("g{i}.wasm"), MINIMAL_GUEST);
            rt.load(&trusted_spec(&format!("guest-{i}"), path, vec![]))
                .unwrap();
        }
        let load_elapsed = t0.elapsed();
        assert_eq!(rt.len(), 5);
        let t1 = std::time::Instant::now();
        let results = rt.dispatch_session_start().await;
        let start_elapsed = t1.elapsed();
        assert_eq!(results.len(), 5);
        assert!(
            results
                .iter()
                .all(|r| matches!(r, GuestCallResult::Ok { code: 0, .. })),
            "all session_start should succeed: {results:?}"
        );
        // Generous CI ceiling (debug + cold wasmtime compile of tiny modules).
        assert!(
            load_elapsed < Duration::from_secs(10),
            "load 5 guests took {load_elapsed:?} (budget 10s debug)"
        );
        assert!(
            start_elapsed < Duration::from_secs(5),
            "session_start×5 took {start_elapsed:?} (budget 5s debug)"
        );
        eprintln!(
            "bench load_five: load={load_elapsed:?} session_start={start_elapsed:?}"
        );
    }
}
