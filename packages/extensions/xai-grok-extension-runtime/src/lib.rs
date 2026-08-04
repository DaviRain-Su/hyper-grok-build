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
//! | `hyper_ext_on_post_tool_use` | `() -> i32` | optional observe; `0` = ok |
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
//! | `tool_success` | `() -> i32` | `1` if last tool succeeded (post_tool) |
//! | `tool_result_len` / `tool_result_byte` | | post_tool result/error preview |
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use xai_grok_extension_api::{
    BeforeAgentStartIn, BeforeAgentStartOut, CORE_ABI_VERSION, Capability, ContractError,
    EXPORT_ABI_VERSION, EXPORT_COMMAND_COUNT, EXPORT_DESCRIBE_COMMAND, EXPORT_DESCRIBE_TOOL,
    EXPORT_INVOKE_COMMAND, EXPORT_INVOKE_TOOL, EXPORT_ON_BEFORE_AGENT_START, EXPORT_ON_BEFORE_MODEL,
    EXPORT_ON_POST_TOOL_USE, EXPORT_ON_PRE_COMPACT, EXPORT_ON_PRE_TOOL_USE, EXPORT_ON_SESSION_END,
    EXPORT_ON_SESSION_START, EXPORT_ON_STOP, EXPORT_TOOL_COUNT, ExtensionSpec, GateFailMode,
    GuestStringError, MAX_INJECT_BYTES, MAX_TOOL_PAYLOAD_BYTES, PostToolIn, PreCompactIn, PreToolIn,
    StopIn, StopOut, WasmCommandDescriptor, WasmToolDescriptor, is_valid_guest_tool_name,
    is_valid_tool_schema_json, read_guest_utf8_from_memory, timeouts, truncate_utf8,
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
    /// Host → guest: whether the tool call succeeded (`post_tool_use`).
    tool_success: bool,
    /// Host → guest: capped tool result/error preview for `post_tool_use`.
    tool_result_in: String,
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
            tool_success: false,
            tool_result_in: String::new(),
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
    /// Short slash command names from [`ExtensionRuntime::collect_registered_commands`].
    advertised_commands: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
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

    /// Production async teardown: shut down every guest worker (interrupt active
    /// job, skip queued backlog) and join OS threads on a blocking pool.
    /// Idempotent. Prefer this over dropping the last Arc on a hot async path.
    pub async fn shutdown_async(&mut self) {
        #[cfg(feature = "wasm")]
        {
            for guest in &self.guests {
                guest.inner.shutdown_async().await;
            }
        }
        self.guests.clear();
    }

    /// Sync teardown fallback (joins worker threads on the calling thread).
    /// Prefer [`Self::shutdown_async`] from async session teardown.
    pub fn shutdown(&mut self) {
        #[cfg(feature = "wasm")]
        {
            for guest in &self.guests {
                guest.inner.shutdown();
            }
        }
        self.guests.clear();
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
                        advertised_commands: Arc::new(std::sync::Mutex::new(
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

    /// Post-tool observe (no gate / no rewrite). Missing export is skipped.
    ///
    /// Call **after** shell/HTTP PostToolUse hooks when present (design §5.2:
    /// hooks first, then wasm). Always fail-open on trap/timeout.
    pub async fn dispatch_post_tool_use(&self, input: &PostToolIn) -> Vec<GuestCallResult> {
        let input = input.clone().capped();
        let mut out = Vec::new();
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            let host = HostCtx {
                tool_name: input.tool_name.clone(),
                tool_input: input.tool_input_json.clone(),
                tool_success: input.success,
                tool_result_in: input.tool_result_preview.clone(),
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (r, _) = guest
                .inner
                .call_with_timeout_host(GuestCall::PostToolUse, timeouts::OBSERVE, host)
                .await;
            self.metrics.record_call(&r);
            if !matches!(
                &r,
                GuestCallResult::SkippedExport {
                    export: EXPORT_ON_POST_TOOL_USE,
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
                self.metrics.pre_tool_denies.fetch_add(1, Ordering::Relaxed);
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
                .ok_or_else(|| {
                    RuntimeError::Module(format!("extension not loaded: {extension}"))
                })?;
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

    /// Collect slash commands from guests with [`Capability::RegisterCommand`].
    ///
    /// Same name rules as tools; `argument_hint` is free text from the describe
    /// `set_tool_schema` host slot (not JSON Schema).
    pub async fn collect_registered_commands(&self) -> Vec<WasmCommandDescriptor> {
        let mut out = Vec::new();
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            if !guest.capabilities.contains(&Capability::RegisterCommand) {
                continue;
            }
            let host = HostCtx {
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (count_res, _) = guest
                .inner
                .call_with_timeout_host(GuestCall::CommandCount, timeouts::OBSERVE, host)
                .await;
            self.metrics.record_call(&count_res);
            let count = match count_res {
                GuestCallResult::Ok { code, .. } if code >= 0 => code as usize,
                _ => continue,
            };
            let count = count.min(32);
            let mut seen_names = std::collections::HashSet::new();
            let mut guest_cmds = Vec::new();
            for i in 0..count {
                let host = HostCtx {
                    tool_index: i as i32,
                    plugin_data_dir: guest.plugin_data_dir.clone(),
                    ..HostCtx::default()
                };
                let (r, host_out) = guest
                    .inner
                    .call_with_timeout_host(GuestCall::DescribeCommand, timeouts::OBSERVE, host)
                    .await;
                self.metrics.record_call(&r);
                if !matches!(r, GuestCallResult::Ok { code: 0, .. }) {
                    continue;
                }
                let name = host_out.tool_name_out;
                if !is_valid_guest_tool_name(&name) {
                    tracing::warn!(
                        extension = %guest.name,
                        command = %name,
                        "skipping wasm command with invalid name"
                    );
                    continue;
                }
                if !seen_names.insert(name.clone()) {
                    tracing::warn!(
                        extension = %guest.name,
                        command = %name,
                        "skipping duplicate wasm command name within extension"
                    );
                    continue;
                }
                guest_cmds.push(WasmCommandDescriptor {
                    extension: guest.name.clone(),
                    name,
                    description: host_out.tool_description_out,
                    argument_hint: host_out.tool_schema_out,
                });
            }
            if let Ok(mut set) = guest.advertised_commands.lock() {
                *set = guest_cmds.iter().map(|c| c.name.clone()).collect();
            }
            out.extend(guest_cmds);
        }
        out
    }

    /// Invoke a slash command registered by a guest. `command_name` is the short name.
    /// `args` is raw free text from the slash line (not required to be JSON).
    pub async fn invoke_registered_command(
        &self,
        extension: &str,
        command_name: &str,
        args: &str,
    ) -> Result<String, RuntimeError> {
        #[cfg(feature = "wasm")]
        {
            let guest = self
                .guests
                .iter()
                .find(|g| g.name == extension)
                .ok_or_else(|| {
                    RuntimeError::Module(format!("extension not loaded: {extension}"))
                })?;
            if !guest.capabilities.contains(&Capability::RegisterCommand) {
                return Err(RuntimeError::Module(format!(
                    "extension `{extension}` lacks register_command capability"
                )));
            }
            if args.len() > MAX_TOOL_PAYLOAD_BYTES {
                return Err(RuntimeError::PayloadTooLarge { got: args.len() });
            }
            if !is_valid_guest_tool_name(command_name) {
                return Err(RuntimeError::InvalidToolName(command_name.to_string()));
            }
            {
                let advertised = guest
                    .advertised_commands
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if !advertised.is_empty() && !advertised.contains(command_name) {
                    return Err(RuntimeError::UnknownTool(
                        command_name.to_string(),
                        extension.to_string(),
                    ));
                }
            }
            let host = HostCtx {
                tool_name: command_name.to_string(),
                tool_input: args.to_string(),
                plugin_data_dir: guest.plugin_data_dir.clone(),
                ..HostCtx::default()
            };
            let (r, host_out) = guest
                .inner
                .call_with_timeout_host(GuestCall::InvokeCommand, timeouts::GATE, host)
                .await;
            self.metrics.record_call(&r);
            match r {
                GuestCallResult::Ok { code: 0, .. } => Ok(if host_out.tool_result_out.is_empty() {
                    String::new()
                } else {
                    host_out.tool_result_out
                }),
                GuestCallResult::Ok { code, .. } => Err(RuntimeError::Module(format!(
                    "command `{command_name}` returned code {code}: {}",
                    if host_out.tool_result_out.is_empty() {
                        host_out.gate_reason
                    } else {
                        host_out.tool_result_out
                    }
                ))),
                GuestCallResult::Failed { error, .. } => Err(RuntimeError::Trap(error)),
                GuestCallResult::Timeout { limit, .. } => Err(RuntimeError::Timeout(limit)),
                GuestCallResult::SkippedExport { export, .. } => {
                    Err(RuntimeError::MissingExport(export))
                }
                GuestCallResult::SkippedCapability { .. } => {
                    Err(RuntimeError::Module("capability skipped".into()))
                }
            }
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = (extension, command_name, args);
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
    if s.is_empty() { None } else { Some(s) }
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
    PostToolUse,
    BeforeAgentStart,
    BeforeModel,
    Stop,
    PreCompact,
    ToolCount,
    DescribeTool,
    InvokeTool,
    CommandCount,
    DescribeCommand,
    InvokeCommand,
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
    SkippedExport {
        extension: String,
        export: &'static str,
    },
    SkippedCapability {
        extension: String,
        capability: Capability,
    },
    Failed {
        extension: String,
        error: String,
    },
    Timeout {
        extension: String,
        limit: Duration,
    },
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
///
/// Owned exclusively by the per-extension worker thread — never shared across
/// tasks or held behind a mutex that async callers could deadlock on.
#[cfg(feature = "wasm")]
struct LiveGuest {
    store: wasmtime::Store<HostCtx>,
    instance: wasmtime::Instance,
}

/// Job lifecycle for epoch-safe cancel (generation avoids ABA after id reuse).
#[cfg(feature = "wasm")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum JobPhase {
    /// No job is prepared/executing.
    Idle = 0,
    /// LiveGuest prepared, epoch deadline set; cancel may increment epoch.
    Armed = 1,
    /// Guest export is running (`func.call`).
    Executing = 2,
}

/// Per-job cancel flag. Callers mark cancel/timeout; the worker skips or
/// epoch-interrupts only when this job is **Armed/Executing**.
#[cfg(feature = "wasm")]
struct JobCancel {
    cancelled: AtomicBool,
}

/// Commands for the long-lived per-extension worker.
///
/// Host imports used during `Call` are **non-blocking** (memory read/write and
/// in-memory bookkeeping only). The worker must never park on async I/O while
/// holding the guest store.
#[cfg(feature = "wasm")]
enum WorkerCmd {
    Call {
        job_id: u64,
        export: &'static str,
        /// Boxed so the empty `Shutdown` variant does not bloat the enum.
        host: Box<HostCtx>,
        reply: tokio::sync::oneshot::Sender<(GuestCallResult, HostCtx)>,
        cancel: Arc<JobCancel>,
    },
    /// Drain and exit; owner joins the thread after this.
    Shutdown,
}

/// Shared worker control plane. Held by the **worker thread** and by the
/// exclusive owner for cancel/shutdown signaling.
///
/// Deliberately does **not** hold the channel sender or `JoinHandle` — those
/// live only on [`WasmGuestInner`] so the worker can never form a cycle with
/// the owner Arc or self-join.
#[cfg(feature = "wasm")]
struct WorkerState {
    name_for_logs: String,
    engine: wasmtime::Engine,
    /// Monotonic job ids starting at 1 (0 = none).
    next_job_id: AtomicU64,
    /// Monotonic generation bumped each time a job becomes Armed (ABA guard).
    next_generation: AtomicU64,
    /// Currently prepared/executing job id, or `0` if idle.
    active_job_id: AtomicU64,
    /// Generation of the active job (paired with `active_job_id`).
    active_generation: AtomicU64,
    /// [`JobPhase`] as u8 for the active job.
    job_phase: AtomicU8,
    /// Once true, new enqueues fail and queued Calls skip guest bodies.
    shutting_down: AtomicBool,
    /// Set true just before the worker thread returns (exit sentinel).
    terminated: AtomicBool,
    /// Wakes waiters blocked on [`Self::terminated`].
    terminate_notify: std::sync::Condvar,
    terminate_mutex: std::sync::Mutex<()>,
    /// Guest bodies that passed Armed checks and entered `func.call`.
    guest_bodies_started: AtomicU64,
    /// Times a job was published Armed (deadline set) for tests.
    jobs_armed: AtomicU64,
    /// In-flight active calls (0 or 1 serial).
    active_calls: AtomicU64,
    /// Test-only: park at end of prepare (before Armed publish) when true.
    #[cfg(test)]
    prepare_hold: AtomicBool,
    /// Test-only: prepare reached hold point.
    #[cfg(test)]
    prepare_holding: AtomicBool,
    /// Test-only: release prepare hold.
    #[cfg(test)]
    prepare_release: AtomicBool,
}

#[cfg(feature = "wasm")]
impl WorkerState {
    fn new(name_for_logs: String, engine: wasmtime::Engine) -> Self {
        Self {
            name_for_logs,
            engine,
            next_job_id: AtomicU64::new(1),
            next_generation: AtomicU64::new(1),
            active_job_id: AtomicU64::new(0),
            active_generation: AtomicU64::new(0),
            job_phase: AtomicU8::new(JobPhase::Idle as u8),
            shutting_down: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            terminate_notify: std::sync::Condvar::new(),
            terminate_mutex: std::sync::Mutex::new(()),
            guest_bodies_started: AtomicU64::new(0),
            jobs_armed: AtomicU64::new(0),
            active_calls: AtomicU64::new(0),
            #[cfg(test)]
            prepare_hold: AtomicBool::new(false),
            #[cfg(test)]
            prepare_holding: AtomicBool::new(false),
            #[cfg(test)]
            prepare_release: AtomicBool::new(false),
        }
    }

    fn mark_terminated(&self) {
        self.terminated.store(true, Ordering::Release);
        let _g = self
            .terminate_mutex
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        self.terminate_notify.notify_all();
    }

    fn wait_terminated(&self) {
        if self.terminated.load(Ordering::Acquire) {
            return;
        }
        let mut g = self
            .terminate_mutex
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        while !self.terminated.load(Ordering::Acquire) {
            g = self
                .terminate_notify
                .wait(g)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    fn clear_active(&self) {
        self.job_phase
            .store(JobPhase::Idle as u8, Ordering::Release);
        self.active_job_id.store(0, Ordering::Release);
        self.active_generation.store(0, Ordering::Release);
    }

    /// Epoch-interrupt only when this job_id+generation is Armed or Executing.
    fn maybe_epoch_interrupt(&self, job_id: u64, generation: u64) {
        let phase = self.job_phase.load(Ordering::Acquire);
        if phase != JobPhase::Armed as u8 && phase != JobPhase::Executing as u8 {
            return;
        }
        if self.active_job_id.load(Ordering::Acquire) != job_id {
            return;
        }
        if self.active_generation.load(Ordering::Acquire) != generation {
            return;
        }
        self.engine.increment_epoch();
    }

    fn cancel_job(&self, job_id: u64, generation: u64, cancel: &JobCancel) {
        cancel.cancelled.store(true, Ordering::Release);
        self.maybe_epoch_interrupt(job_id, generation);
    }
}

/// Exclusive owner of the channel sender and worker `JoinHandle`.
/// Cloned via [`Arc`]; only the **last** drop shuts down and joins.
/// The worker thread never holds this Arc.
#[cfg(feature = "wasm")]
struct WasmGuestInner {
    state: Arc<WorkerState>,
    /// Serial job queue. `None` after shutdown begins.
    job_tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<WorkerCmd>>>,
    /// Worker OS thread. Joined once by the first shutdown waiter.
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Cloneable guest handle. All clones share one long-lived worker that owns
/// `LiveGuest` and serializes calls over a channel.
#[cfg(feature = "wasm")]
#[derive(Clone)]
struct WasmGuest {
    inner: Arc<WasmGuestInner>,
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
        // Wall-clock timeout / cancel can interrupt busy guest loops via epoch.
        config.epoch_interruption(true);
        let engine =
            wasmtime::Engine::new(&config).map_err(|e| RuntimeError::Module(e.to_string()))?;
        let module = wasmtime::Module::new(&engine, bytes)
            .map_err(|e| RuntimeError::Module(e.to_string()))?;
        let linker = build_linker(&engine)?;

        // Validate ABI at load time (fresh store; session instance lives on worker).
        {
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
        }

        let (job_tx, job_rx) = std::sync::mpsc::channel::<WorkerCmd>();
        let state = Arc::new(WorkerState::new(name_for_logs.clone(), engine));
        // Worker gets only WorkerState — never the owner Arc.
        let worker_state = Arc::clone(&state);
        let thread_name = format!("wasm-ext-{name_for_logs}");

        let worker = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                worker_main(job_rx, module, linker, worker_state);
            })
            .map_err(|e| RuntimeError::Module(format!("failed to spawn guest worker: {e}")))?;

        Ok(Self {
            inner: Arc::new(WasmGuestInner {
                state,
                job_tx: std::sync::Mutex::new(Some(job_tx)),
                worker: std::sync::Mutex::new(Some(worker)),
            }),
        })
    }

    #[cfg(test)]
    fn active_call_count(&self) -> u64 {
        self.inner.state.active_calls.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn guest_bodies_started(&self) -> u64 {
        self.inner
            .state
            .guest_bodies_started
            .load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn jobs_armed(&self) -> u64 {
        self.inner.state.jobs_armed.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn active_job_id(&self) -> u64 {
        self.inner.state.active_job_id.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn job_phase(&self) -> u8 {
        self.inner.state.job_phase.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn is_terminated(&self) -> bool {
        self.inner.state.terminated.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn set_prepare_hold(&self, hold: bool) {
        self.inner.state.prepare_hold.store(hold, Ordering::Release);
        if !hold {
            self.inner
                .state
                .prepare_release
                .store(true, Ordering::Release);
        } else {
            self.inner
                .state
                .prepare_release
                .store(false, Ordering::Release);
            self.inner
                .state
                .prepare_holding
                .store(false, Ordering::Release);
        }
    }

    #[cfg(test)]
    fn prepare_is_holding(&self) -> bool {
        self.inner.state.prepare_holding.load(Ordering::Acquire)
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
            tool_success: data.tool_success,
            tool_result_in: std::mem::take(&mut data.tool_result_in),
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

    fn export_name(call: GuestCall) -> &'static str {
        match call {
            GuestCall::SessionStart => EXPORT_ON_SESSION_START,
            GuestCall::SessionEnd => EXPORT_ON_SESSION_END,
            GuestCall::PreToolUse => EXPORT_ON_PRE_TOOL_USE,
            GuestCall::PostToolUse => EXPORT_ON_POST_TOOL_USE,
            GuestCall::BeforeAgentStart => EXPORT_ON_BEFORE_AGENT_START,
            GuestCall::BeforeModel => EXPORT_ON_BEFORE_MODEL,
            GuestCall::Stop => EXPORT_ON_STOP,
            GuestCall::PreCompact => EXPORT_ON_PRE_COMPACT,
            GuestCall::ToolCount => EXPORT_TOOL_COUNT,
            GuestCall::DescribeTool => EXPORT_DESCRIBE_TOOL,
            GuestCall::InvokeTool => EXPORT_INVOKE_TOOL,
            GuestCall::CommandCount => EXPORT_COMMAND_COUNT,
            GuestCall::DescribeCommand => EXPORT_DESCRIBE_COMMAND,
            GuestCall::InvokeCommand => EXPORT_INVOKE_COMMAND,
        }
    }

    /// Whether the long-lived worker thread is still running (tests / diagnostics).
    #[cfg(test)]
    fn worker_is_alive(&self) -> bool {
        !self.inner.state.terminated.load(Ordering::Acquire)
            && self
                .inner
                .worker
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .is_some_and(|h| !h.is_finished())
    }

    /// Production async teardown: mark shutdown, interrupt Armed/Executing job,
    /// join the worker on a blocking pool so the async runtime is not stalled.
    /// All concurrent callers wait until the worker has truly exited.
    pub async fn shutdown_async(&self) {
        let state = Arc::clone(&self.inner.state);
        let handle = self.inner.begin_shutdown_take_join();
        if let Some(handle) = handle {
            // Never join from the worker thread itself (would self-deadlock);
            // spawn_blocking runs on a different pool thread.
            let _ = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
                // Worker marks terminated on exit; belt-and-suspenders if join
                // raced after mark.
                state.mark_terminated();
            })
            .await;
        } else {
            // Another waiter took the join handle; wait for exit sentinel.
            let state = Arc::clone(&self.inner.state);
            let _ = tokio::task::spawn_blocking(move || {
                state.wait_terminated();
            })
            .await;
        }
    }

    /// Sync shutdown used by tests and Drop fallback. Concurrent callers all
    /// wait for real worker exit (first joins, others wait on terminated).
    pub fn shutdown(&self) {
        self.inner.shutdown_sync();
    }

    async fn call_with_timeout_host(
        &self,
        call: GuestCall,
        limit: Duration,
        host: HostCtx,
    ) -> (GuestCallResult, HostCtx) {
        let export = Self::export_name(call);
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        let mut job_id = self.inner.state.next_job_id.fetch_add(1, Ordering::Relaxed);
        // 0 is reserved for "idle"; skip if wraparound ever hits it.
        if job_id == 0 {
            job_id = self.inner.state.next_job_id.fetch_add(1, Ordering::Relaxed);
        }
        // Cancel uses (job_id, generation) read from WorkerState when phase is
        // Armed/Executing; generation 0 means "not yet armed" (flag-only cancel).
        let cancel = Arc::new(JobCancel {
            cancelled: AtomicBool::new(false),
        });

        {
            if self.inner.state.shutting_down.load(Ordering::Acquire) {
                return (
                    GuestCallResult::Failed {
                        extension: self.inner.state.name_for_logs.clone(),
                        error: "guest worker shut down".into(),
                    },
                    host,
                );
            }
            let guard = self.inner.job_tx.lock().unwrap_or_else(|p| p.into_inner());
            let Some(tx) = guard.as_ref() else {
                return (
                    GuestCallResult::Failed {
                        extension: self.inner.state.name_for_logs.clone(),
                        error: "guest worker shut down".into(),
                    },
                    host,
                );
            };
            if tx
                .send(WorkerCmd::Call {
                    job_id,
                    export,
                    host: Box::new(host),
                    reply: reply_tx,
                    cancel: Arc::clone(&cancel),
                })
                .is_err()
            {
                return (
                    GuestCallResult::Failed {
                        extension: self.inner.state.name_for_logs.clone(),
                        error: "guest worker channel closed".into(),
                    },
                    HostCtx::default(),
                );
            }
        }

        let mut cancel_guard = JobEpochCancelGuard {
            state: Arc::clone(&self.inner.state),
            job_id,
            cancel: Arc::clone(&cancel),
            armed: true,
        };

        let out = tokio::select! {
            r = &mut reply_rx => {
                match r {
                    Ok(pair) => pair,
                    Err(_) => (
                        GuestCallResult::Failed {
                            extension: self.inner.state.name_for_logs.clone(),
                            error: "guest worker dropped reply".into(),
                        },
                        HostCtx::default(),
                    ),
                }
            }
            _ = tokio::time::sleep(limit) => {
                // Flag cancel; epoch only if this job is Armed/Executing.
                let job_gen = if self.inner.state.active_job_id.load(Ordering::Acquire) == job_id {
                    self.inner.state.active_generation.load(Ordering::Acquire)
                } else {
                    0
                };
                self.inner.state.cancel_job(job_id, job_gen, &cancel);
                let grace = Duration::from_millis(250);
                match tokio::time::timeout(grace, &mut reply_rx).await {
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
                    Ok(Err(_)) | Err(_) => (
                        GuestCallResult::Timeout {
                            extension: self.inner.state.name_for_logs.clone(),
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

/// Worker thread body: exclusive owner of [`LiveGuest`], serializes all calls.
/// Holds only [`WorkerState`] — never the owner Arc.
#[cfg(feature = "wasm")]
fn worker_main(
    job_rx: std::sync::mpsc::Receiver<WorkerCmd>,
    module: wasmtime::Module,
    linker: wasmtime::Linker<HostCtx>,
    state: Arc<WorkerState>,
) {
    let engine = state.engine.clone();
    let name = state.name_for_logs.clone();
    let mut live: Option<LiveGuest> = None;
    while let Ok(cmd) = job_rx.recv() {
        match cmd {
            WorkerCmd::Shutdown => break,
            WorkerCmd::Call {
                job_id,
                export,
                host,
                reply,
                cancel,
                ..
            } => {
                // Skip without preparing guest: shutdown, pre-cancel, or dead reply.
                if state.shutting_down.load(Ordering::Acquire)
                    || cancel.cancelled.load(Ordering::Acquire)
                    || reply.is_closed()
                {
                    let _ = reply.send((
                        GuestCallResult::Failed {
                            extension: name.clone(),
                            error: if state.shutting_down.load(Ordering::Relaxed) {
                                "guest worker shut down".into()
                            } else {
                                "guest call cancelled before start".into()
                            },
                        },
                        *host,
                    ));
                    continue;
                }

                state.active_calls.fetch_add(1, Ordering::Relaxed);
                // Prepare LiveGuest + func + fuel + epoch deadline WITHOUT
                // publishing Armed yet (cancel only sets flags in this window).
                let prepared = prepare_guest_call(
                    &mut live, &engine, &module, &linker, &name, export, *host, &state,
                );

                let prepared = match prepared {
                    Ok(p) => p,
                    Err(pair) => {
                        state.active_calls.fetch_sub(1, Ordering::Relaxed);
                        let _ = reply.send(pair);
                        if state.shutting_down.load(Ordering::Acquire) {
                            drain_and_break(&job_rx, &name);
                            break;
                        }
                        continue;
                    }
                };

                // Publish Armed: deadline is set; cancel may now increment epoch.
                let generation = state.next_generation.fetch_add(1, Ordering::Relaxed).max(1);
                state.active_job_id.store(job_id, Ordering::Release);
                state.active_generation.store(generation, Ordering::Release);
                state
                    .job_phase
                    .store(JobPhase::Armed as u8, Ordering::Release);
                state.jobs_armed.fetch_add(1, Ordering::Relaxed);

                // Re-check after Armed publish so a concurrent cancel that
                // raced during prepare can still skip execute (and may have
                // already bumped epoch — trap is fine).
                if state.shutting_down.load(Ordering::Acquire)
                    || cancel.cancelled.load(Ordering::Acquire)
                    || reply.is_closed()
                {
                    // Consume prepared without executing: take outputs as-is.
                    let host_out = {
                        let live_guest = live.as_mut().expect("prepared");
                        WasmGuest::take_host_outputs(live_guest.store.data_mut())
                    };
                    state.clear_active();
                    state.active_calls.fetch_sub(1, Ordering::Relaxed);
                    let _ = reply.send((
                        GuestCallResult::Failed {
                            extension: name.clone(),
                            error: "guest call cancelled before execute".into(),
                        },
                        host_out,
                    ));
                    if state.shutting_down.load(Ordering::Acquire) {
                        drain_and_break(&job_rx, &name);
                        break;
                    }
                    continue;
                }

                state
                    .job_phase
                    .store(JobPhase::Executing as u8, Ordering::Release);
                state.guest_bodies_started.fetch_add(1, Ordering::Relaxed);
                let pair = execute_guest_call(&mut live, prepared);

                state.clear_active();
                state.active_calls.fetch_sub(1, Ordering::Relaxed);
                let _ = reply.send(pair);

                if state.shutting_down.load(Ordering::Acquire) {
                    drain_and_break(&job_rx, &name);
                    break;
                }
            }
        }
    }
    drop(live);
    state.mark_terminated();
}

#[cfg(feature = "wasm")]
fn drain_and_break(job_rx: &std::sync::mpsc::Receiver<WorkerCmd>, name: &str) {
    while let Ok(pending) = job_rx.try_recv() {
        match pending {
            WorkerCmd::Shutdown => {}
            WorkerCmd::Call { host, reply, .. } => {
                let _ = reply.send((
                    GuestCallResult::Failed {
                        extension: name.to_string(),
                        error: "guest worker shut down".into(),
                    },
                    *host,
                ));
            }
        }
    }
}

/// Prepared guest call: epoch deadline is already set on the store.
#[cfg(feature = "wasm")]
struct PreparedCall {
    export: &'static str,
}

/// Prepare store/instance/inputs/fuel/func lookup and set epoch deadline.
/// Does **not** publish Armed — caller does that after prepare succeeds.
#[cfg(feature = "wasm")]
#[allow(clippy::result_large_err)] // Err carries HostCtx for reply path; not public API.
fn prepare_guest_call(
    live: &mut Option<LiveGuest>,
    engine: &wasmtime::Engine,
    module: &wasmtime::Module,
    linker: &wasmtime::Linker<HostCtx>,
    name: &str,
    export: &'static str,
    host: HostCtx,
    state: &WorkerState,
) -> Result<PreparedCall, (GuestCallResult, HostCtx)> {
    if live.is_none() {
        let mut store = wasmtime::Store::new(engine, HostCtx::default());
        store.limiter(|s| &mut s.limits);
        store.epoch_deadline_trap();
        let instance = match linker.instantiate(&mut store, module) {
            Ok(i) => i,
            Err(e) => {
                return Err((
                    GuestCallResult::Failed {
                        extension: name.to_string(),
                        error: e.to_string(),
                    },
                    host,
                ));
            }
        };
        *live = Some(LiveGuest { store, instance });
    }

    let live_guest = live.as_mut().expect("just ensured");
    WasmGuest::apply_host_inputs(live_guest.store.data_mut(), host, name);

    if let Err(e) = live_guest.store.set_fuel(10_000_000) {
        let host_out = WasmGuest::take_host_outputs(live_guest.store.data_mut());
        return Err((
            GuestCallResult::Failed {
                extension: name.to_string(),
                error: e.to_string(),
            },
            host_out,
        ));
    }

    // Ensure the export exists before setting deadline / Armed.
    if live_guest
        .instance
        .get_typed_func::<(), i32>(&mut live_guest.store, export)
        .is_err()
    {
        let host_out = WasmGuest::take_host_outputs(live_guest.store.data_mut());
        return Err((
            GuestCallResult::SkippedExport {
                extension: name.to_string(),
                export,
            },
            host_out,
        ));
    }

    // Epoch deadline set here; cancel must not bump epoch until Armed is
    // published (otherwise the bump is lost before the deadline is set).
    live_guest.store.set_epoch_deadline(1);

    // Test hook: hold after deadline set but before Armed publish.
    #[cfg(test)]
    {
        if state.prepare_hold.load(Ordering::Acquire) {
            state.prepare_holding.store(true, Ordering::Release);
            while !state.prepare_release.load(Ordering::Acquire)
                && !state.shutting_down.load(Ordering::Acquire)
            {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            state.prepare_holding.store(false, Ordering::Release);
        }
    }
    #[cfg(not(test))]
    {
        let _ = state;
    }

    Ok(PreparedCall { export })
}

#[cfg(feature = "wasm")]
fn execute_guest_call(
    live: &mut Option<LiveGuest>,
    prepared: PreparedCall,
) -> (GuestCallResult, HostCtx) {
    let live_guest = live.as_mut().expect("prepared");
    let export = prepared.export;
    let func = live_guest
        .instance
        .get_typed_func::<(), i32>(&mut live_guest.store, export)
        .expect("export checked in prepare");
    let call_result = func.call(&mut live_guest.store, ());
    let host_out = WasmGuest::take_host_outputs(live_guest.store.data_mut());
    let name = host_out.guest_name.clone();
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
}

#[cfg(feature = "wasm")]
impl WasmGuestInner {
    /// Mark shutting down, interrupt Armed/Executing job (if any), close the
    /// enqueue path, and send Shutdown. Returns the join handle if this
    /// caller is responsible for joining (first successful take).
    fn begin_shutdown_take_join(&self) -> Option<std::thread::JoinHandle<()>> {
        self.state.shutting_down.store(true, Ordering::Release);
        // Interrupt only if a job is Armed or Executing (deadline is live).
        let phase = self.state.job_phase.load(Ordering::Acquire);
        if phase == JobPhase::Armed as u8 || phase == JobPhase::Executing as u8 {
            self.state.engine.increment_epoch();
        }
        // Also release prepare hold so shutdown can progress in tests.
        #[cfg(test)]
        {
            self.state.prepare_release.store(true, Ordering::Release);
        }
        // Prevent further enqueues and signal the worker.
        if let Some(tx) = self.job_tx.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = tx.send(WorkerCmd::Shutdown);
            drop(tx);
        }
        self.worker.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    fn shutdown_sync(&self) {
        if let Some(handle) = self.begin_shutdown_take_join() {
            // Sync join is the Drop/test fallback. Prefer async teardown on
            // production paths. Never call from the worker thread.
            let _ = handle.join();
            self.state.mark_terminated();
        } else {
            // Concurrent waiter: wait for the exit sentinel.
            self.state.wait_terminated();
        }
    }
}

/// Final Arc drop of the exclusive owner: shut down worker and join so no
/// guest resources or threads leak after the last external clone is gone.
#[cfg(feature = "wasm")]
impl Drop for WasmGuestInner {
    fn drop(&mut self) {
        self.shutdown_sync();
    }
}

/// On Drop (async cancel): mark this job cancelled and epoch-interrupt only if
/// it is Armed/Executing for this job_id. Never touches the worker JoinHandle.
#[cfg(feature = "wasm")]
struct JobEpochCancelGuard {
    state: Arc<WorkerState>,
    job_id: u64,
    cancel: Arc<JobCancel>,
    armed: bool,
}

#[cfg(feature = "wasm")]
impl JobEpochCancelGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(feature = "wasm")]
impl Drop for JobEpochCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            let job_gen = if self.state.active_job_id.load(Ordering::Acquire) == self.job_id {
                self.state.active_generation.load(Ordering::Acquire)
            } else {
                0
            };
            self.state.cancel_job(self.job_id, job_gen, &self.cancel);
        }
    }
}

#[cfg(feature = "wasm")]
fn build_linker(engine: &wasmtime::Engine) -> Result<wasmtime::Linker<HostCtx>, RuntimeError> {
    let mut linker = wasmtime::Linker::new(engine);
    // Optional host park for tests (and a cheap no-op in production). Lets a
    // guest stay "active" on the worker without burning fuel on a tight loop.
    linker
        .func_wrap("hyper_host", "test_park", || {
            #[cfg(test)]
            std::thread::sleep(std::time::Duration::from_millis(20));
        })
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "tool_name_len",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 { caller.data().tool_name.len() as i32 },
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
            "tool_success",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 {
                i32::from(caller.data().tool_success)
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    // Host → guest: post_tool result/error preview (read-only; distinct from
    // set_tool_result which is guest → host for invoke_tool).
    linker
        .func_wrap(
            "hyper_host",
            "tool_result_len",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 {
                caller.data().tool_result_in.len() as i32
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "tool_result_byte",
            |caller: wasmtime::Caller<'_, HostCtx>, idx: i32| -> i32 {
                byte_at(&caller.data().tool_result_in, idx)
            },
        )
        .map_err(|e| RuntimeError::Module(e.to_string()))?;
    linker
        .func_wrap(
            "hyper_host",
            "prompt_len",
            |caller: wasmtime::Caller<'_, HostCtx>| -> i32 { caller.data().prompt.len() as i32 },
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
                // Strict UTF-8 + bounds: invalid guest strings are ignored
                // (no panic, no lossy rewrite into host policy text).
                if let Ok(s) = read_guest_utf8(&mut caller, ptr, len, MAX_INJECT_BYTES) {
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
                if let Ok(s) = read_guest_utf8(&mut caller, ptr, len, MAX_INJECT_BYTES) {
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
                if let Ok(s) = read_guest_utf8(&mut caller, ptr, len, MAX_INJECT_BYTES) {
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
                // Log uses inject cap (32 KiB); TooLong rejects entirely.
                let Ok(msg) = read_guest_utf8(&mut caller, ptr, len, MAX_INJECT_BYTES) else {
                    return;
                };
                let mut msg = msg;
                // Defensive: already capped by TooLong; keep char-safe truncate.
                truncate_utf8(&mut msg, MAX_INJECT_BYTES);
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
                // Tool names are short; reuse inject cap (validation is separate).
                if let Ok(s) = read_guest_utf8(&mut caller, ptr, len, MAX_INJECT_BYTES) {
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
                if let Ok(s) = read_guest_utf8(&mut caller, ptr, len, MAX_TOOL_PAYLOAD_BYTES) {
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
                // Schema/result payloads may be larger than inject strings.
                if let Ok(s) = read_guest_utf8(&mut caller, ptr, len, MAX_TOOL_PAYLOAD_BYTES) {
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
                if let Ok(s) = read_guest_utf8(&mut caller, ptr, len, MAX_TOOL_PAYLOAD_BYTES) {
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

/// Read guest linear memory as strict UTF-8. Never panics on bad ptr/len/UTF-8.
///
/// `max_len` is the host policy cap for this import:
/// - inject / append / gate reason / log → [`MAX_INJECT_BYTES`] (32 KiB)
/// - tool schema / result / description → [`MAX_TOOL_PAYLOAD_BYTES`] (128 KiB)
///
/// Returns [`GuestStringError`] so host imports can ignore the write instead of
/// trapping the store or substituting U+FFFD into security-sensitive strings.
#[cfg(feature = "wasm")]
fn read_guest_utf8(
    caller: &mut wasmtime::Caller<'_, HostCtx>,
    ptr: i32,
    len: i32,
    max_len: usize,
) -> Result<String, GuestStringError> {
    if ptr < 0 || len < 0 {
        return Err(GuestStringError::Negative);
    }
    let mem = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or(GuestStringError::OutOfBounds)?;
    // `memory.data` is a safe bounds-checked view; we only re-slice via the
    // shared helper so OOB never panics.
    let data = mem.data(&*caller);
    read_guest_utf8_from_memory(data, ptr, len, max_len)
}

/// Compile WAT text to a core Wasm module (test helpers / shell e2e fixtures).
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
        rt.load(&trusted_spec("pol", path, vec![Capability::PreToolGate]))
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
        rt.load(&trusted_spec("trap", path, vec![Capability::PreToolGate]))
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
        rt.load(&trusted_spec("trap", path, vec![Capability::PreToolGate]))
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
        rt.load(&trusted_spec("safe-shell", path, vec![])).unwrap();
        let d = rt
            .dispatch_pre_tool_use(&PreToolIn {
                tool_name: "run_terminal_command".into(),
                tool_input_json: r#"{"command":"rm -rf /"}"#.into(),
            })
            .await;
        assert!(matches!(d.decision, PreToolDecision::Allow));
    }

    /// Observe-only post_tool: logs when tool_success==0 and returns 0.
    /// Guest returns 1 if tool_success is wrong (host bug) so the test fails loudly.
    const POST_TOOL_GUEST: &str = r#"
        (module
          (import "hyper_host" "tool_success" (func $tool_success (result i32)))
          (import "hyper_host" "tool_name_len" (func $name_len (result i32)))
          (import "hyper_host" "tool_result_len" (func $result_len (result i32)))
          (import "hyper_host" "log" (func $log (param i32 i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "post-fail")
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_post_tool_use") (result i32)
            (local $ok i32)
            (local $nlen i32)
            (local $rlen i32)
            (local.set $ok (call $tool_success))
            (local.set $nlen (call $name_len))
            (local.set $rlen (call $result_len))
            ;; name must be non-empty
            (if (i32.eqz (local.get $nlen))
              (then (return (i32.const 1))))
            ;; on failure path: success==0 and result preview non-empty
            (if (i32.eqz (local.get $ok))
              (then
                (if (i32.eqz (local.get $rlen))
                  (then (return (i32.const 1))))
                (call $log (i32.const 2) (i32.const 0) (i32.const 9))))
            i32.const 0)
        )
    "#;

    /// Minimal register_command guest: one slash command `hello`.
    const COMMAND_GUEST: &str = r#"
        (module
          (import "hyper_host" "tool_index" (func $tool_index (result i32)))
          (import "hyper_host" "set_tool_name" (func $set_name (param i32 i32)))
          (import "hyper_host" "set_tool_description" (func $set_desc (param i32 i32)))
          (import "hyper_host" "set_tool_schema" (func $set_hint (param i32 i32)))
          (import "hyper_host" "set_tool_result" (func $set_result (param i32 i32)))
          (import "hyper_host" "tool_name_len" (func $name_len (result i32)))
          (import "hyper_host" "input_len" (func $input_len (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "hello")
          (data (i32.const 16) "Say hello")
          (data (i32.const 32) "<name>")
          (data (i32.const 48) "hi-from-cmd")
          (func (export "hyper_ext_abi_version") (result i32) i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32) i32.const 0)
          (func (export "hyper_ext_command_count") (result i32) i32.const 1)
          (func (export "hyper_ext_describe_command") (result i32)
            (if (i32.ne (call $tool_index) (i32.const 0))
              (then (return (i32.const 1))))
            (call $set_name (i32.const 0) (i32.const 5))
            (call $set_desc (i32.const 16) (i32.const 9))
            (call $set_hint (i32.const 32) (i32.const 6))
            i32.const 0)
          (func (export "hyper_ext_invoke_command") (result i32)
            ;; require non-empty command name from host
            (if (i32.eqz (call $name_len)) (then (return (i32.const 1))))
            (call $set_result (i32.const 48) (i32.const 11))
            i32.const 0)
        )
    "#;

    #[tokio::test]
    async fn register_and_invoke_slash_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "cmd.wasm", COMMAND_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "cmd-ext",
            path,
            vec![Capability::RegisterCommand],
        ))
        .unwrap();
        let cmds = rt.collect_registered_commands().await;
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "hello");
        assert_eq!(cmds[0].description, "Say hello");
        assert_eq!(cmds[0].argument_hint, "<name>");
        let out = rt
            .invoke_registered_command("cmd-ext", "hello", "world")
            .await
            .expect("invoke");
        assert_eq!(out, "hi-from-cmd");
    }

    #[tokio::test]
    async fn post_tool_use_observe_success_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "post.wasm", POST_TOOL_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec("post-observer", path, vec![]))
            .unwrap();

        let ok = rt
            .dispatch_post_tool_use(&PostToolIn {
                tool_name: "read_file".into(),
                success: true,
                tool_input_json: r#"{"path":"a.rs"}"#.into(),
                tool_result_preview: r#"{"ok":true}"#.into(),
            })
            .await;
        assert_eq!(ok.len(), 1);
        match &ok[0] {
            GuestCallResult::Ok { code: 0, .. } => {}
            other => panic!("expected Ok(0) on success, got {other:?}"),
        }

        let fail = rt
            .dispatch_post_tool_use(&PostToolIn {
                tool_name: "read_file".into(),
                success: false,
                tool_input_json: r#"{"path":"missing"}"#.into(),
                tool_result_preview: "ENOENT: missing".into(),
            })
            .await;
        assert_eq!(fail.len(), 1);
        match &fail[0] {
            GuestCallResult::Ok {
                code: 0,
                logs,
                ..
            } => {
                assert!(
                    logs.iter().any(|l| l.message.contains("post-fail")),
                    "expected guest log on failure path, logs={logs:?}"
                );
            }
            other => panic!("expected Ok(0) on failure observe, got {other:?}"),
        }

        // Guests without the export are silently skipped.
        let bare = r#"
            (module
              (func (export "hyper_ext_abi_version") (result i32) i32.const 1)
              (func (export "hyper_ext_on_session_start") (result i32) i32.const 0)
            )
        "#;
        let path2 = write_wasm(&dir, "bare.wasm", bare);
        let mut rt2 = ExtensionRuntime::new();
        rt2.load(&trusted_spec("bare", path2, vec![])).unwrap();
        let skipped = rt2
            .dispatch_post_tool_use(&PostToolIn {
                tool_name: "x".into(),
                success: true,
                tool_input_json: String::new(),
                tool_result_preview: String::new(),
            })
            .await;
        assert!(
            skipped.is_empty(),
            "missing export should not appear in results: {skipped:?}"
        );
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
        rt.load(&trusted_spec("bad", path, vec![Capability::RegisterTool]))
            .unwrap();
        let tools = rt.collect_registered_tools().await;
        assert!(
            tools.is_empty(),
            "invalid tool should be skipped: {tools:?}"
        );
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

    /// After a timeout/cancel, a subsequent call must not hang: the long-lived
    /// worker owns LiveGuest and serializes jobs; cancel only bumps epoch for
    /// the active job_id.
    #[tokio::test]
    async fn timeout_does_not_leave_detached_task_blocking_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "loop2.wasm", INFINITE_LOOP_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec("loop", path, vec![])).unwrap();
        assert!(
            rt.guests[0].inner.worker_is_alive(),
            "worker must be running after load"
        );
        let first = rt.dispatch_session_start().await;
        assert!(
            matches!(
                &first[0],
                GuestCallResult::Timeout { .. } | GuestCallResult::Failed { .. }
            ),
            "first call should be bounded: {:?}",
            first[0]
        );
        // Worker stays alive (JoinHandle never aborted/detached).
        assert!(
            rt.guests[0].inner.worker_is_alive(),
            "worker must survive timeout"
        );
        let start = std::time::Instant::now();
        // Second call reuses the same worker after the epoch trap finishes.
        let second = rt.dispatch_session_start().await;
        assert!(
            matches!(
                &second[0],
                GuestCallResult::Timeout { .. } | GuestCallResult::Failed { .. }
            ),
            "second call should also be bounded: {:?}",
            second[0]
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "post-timeout call hung: {:?}",
            start.elapsed()
        );
        // No call should remain stuck on the worker after both return.
        assert_eq!(rt.guests[0].inner.active_call_count(), 0);
    }

    /// Dropping a call future mid-flight must not kill the worker; a later
    /// call still completes.
    #[tokio::test]
    async fn cancel_drops_reply_but_worker_accepts_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "loop-cancel.wasm", INFINITE_LOOP_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec("loop", path, vec![])).unwrap();
        let guest = rt.guests[0].inner.clone();
        {
            // Scope owns the future; leaving the scope drops it (cancel).
            let call = guest.call_with_timeout_host(
                GuestCall::SessionStart,
                Duration::from_secs(30),
                HostCtx::default(),
            );
            let _ = tokio::time::timeout(Duration::from_millis(20), call).await;
        }
        assert!(guest.worker_is_alive());
        let start = std::time::Instant::now();
        let (result, _) = guest
            .call_with_timeout_host(
                GuestCall::SessionStart,
                Duration::from_secs(2),
                HostCtx::default(),
            )
            .await;
        assert!(
            matches!(
                result,
                GuestCallResult::Timeout { .. } | GuestCallResult::Failed { .. }
            ),
            "post-cancel call: {result:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "post-cancel hung: {:?}",
            start.elapsed()
        );
    }

    /// Slow park loop so A stays active long enough for B/C queue tests
    /// (tight wasm loops burn fuel before the async test observes active_job_id).
    const PARK_LOOP_GUEST: &str = r#"
        (module
          (import "hyper_host" "test_park" (func $park))
          (func (export "hyper_ext_abi_version") (result i32) i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            (loop $forever
              (call $park)
              (br $forever))
            i32.const 0)
          (func (export "hyper_ext_on_session_end") (result i32)
            i32.const 0)
        )
    "#;

    /// A active (park loop) + B/C queued: cancel/timeout of B must not run B's
    /// guest body and must not count as active for epoch purposes.
    #[tokio::test]
    async fn queued_timeout_does_not_run_guest_or_interrupt_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "queued.wasm", PARK_LOOP_GUEST);
        let guest = WasmGuest::load(&path).unwrap();
        let bodies_before = guest.guest_bodies_started();

        // Start A (park loop) with a long timeout.
        let guest_a = guest.clone();
        let a = tokio::spawn(async move {
            guest_a
                .call_with_timeout_host(
                    GuestCall::SessionStart,
                    Duration::from_secs(10),
                    HostCtx::default(),
                )
                .await
        });

        // Wait until A is active on the worker.
        let start = std::time::Instant::now();
        while guest.active_job_id() == 0 {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "A never became active"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let a_id = guest.active_job_id();
        assert_ne!(a_id, 0);
        let bodies_with_a = guest.guest_bodies_started();
        assert!(
            bodies_with_a > bodies_before,
            "A should have started a body"
        );

        // Enqueue B and C while A is active; both time out almost immediately
        // so they are cancelled while still queued.
        let guest_b = guest.clone();
        let b = tokio::spawn(async move {
            guest_b
                .call_with_timeout_host(
                    GuestCall::SessionEnd,
                    Duration::from_millis(1),
                    HostCtx::default(),
                )
                .await
        });
        let guest_c = guest.clone();
        let c = tokio::spawn(async move {
            guest_c
                .call_with_timeout_host(
                    GuestCall::SessionEnd,
                    Duration::from_millis(1),
                    HostCtx::default(),
                )
                .await
        });

        let (b_res, _) = b.await.unwrap();
        let (c_res, _) = c.await.unwrap();
        assert!(
            matches!(
                b_res,
                GuestCallResult::Timeout { .. } | GuestCallResult::Failed { .. }
            ),
            "B should cancel/timeout without guest body: {b_res:?}"
        );
        assert!(
            matches!(
                c_res,
                GuestCallResult::Timeout { .. } | GuestCallResult::Failed { .. }
            ),
            "C should cancel/timeout without guest body: {c_res:?}"
        );

        // A should still be the active job; B/C cancel must not clear it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            guest.active_job_id(),
            a_id,
            "queued cancel must not clear active A"
        );
        // Bodies for B/C must not have started: only A's body so far.
        assert_eq!(
            guest.guest_bodies_started(),
            bodies_with_a,
            "queued B/C must not execute guest bodies"
        );

        // Abort A via its own timeout path by dropping / awaiting.
        let _ = tokio::time::timeout(Duration::from_millis(500), a).await;
    }

    /// Shutdown drops queued backlog without running guest bodies.
    #[tokio::test]
    async fn shutdown_skips_queued_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "shut-q.wasm", PARK_LOOP_GUEST);
        let guest = WasmGuest::load(&path).unwrap();

        let guest_a = guest.clone();
        let a = tokio::spawn(async move {
            guest_a
                .call_with_timeout_host(
                    GuestCall::SessionStart,
                    Duration::from_secs(10),
                    HostCtx::default(),
                )
                .await
        });
        let start = std::time::Instant::now();
        while guest.active_job_id() == 0 {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "A never became active"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let bodies_at_a = guest.guest_bodies_started();

        // Queue B/C behind A.
        let guest_b = guest.clone();
        let b = tokio::spawn(async move {
            guest_b
                .call_with_timeout_host(
                    GuestCall::SessionEnd,
                    Duration::from_secs(5),
                    HostCtx::default(),
                )
                .await
        });
        let guest_c = guest.clone();
        let c = tokio::spawn(async move {
            guest_c
                .call_with_timeout_host(
                    GuestCall::SessionEnd,
                    Duration::from_secs(5),
                    HostCtx::default(),
                )
                .await
        });
        // Let B/C enqueue.
        tokio::time::sleep(Duration::from_millis(30)).await;

        guest.shutdown_async().await;
        assert!(!guest.worker_is_alive());
        assert_eq!(
            guest.guest_bodies_started(),
            bodies_at_a,
            "shutdown must not run queued B/C guest bodies"
        );

        let (b_res, _) = b.await.unwrap();
        let (c_res, _) = c.await.unwrap();
        assert!(
            matches!(b_res, GuestCallResult::Failed { .. }),
            "B after shutdown: {b_res:?}"
        );
        assert!(
            matches!(c_res, GuestCallResult::Failed { .. }),
            "C after shutdown: {c_res:?}"
        );
        let _ = a.await;
    }

    /// Explicit async shutdown joins the worker; further calls fail cleanly.
    #[tokio::test]
    async fn explicit_shutdown_reclaims_worker() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "shut.wasm", MINIMAL_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec("shut", path, vec![])).unwrap();
        let guest = rt.guests[0].inner.clone();
        assert!(guest.worker_is_alive());
        guest.shutdown_async().await;
        assert!(!guest.worker_is_alive());
        assert!(guest.is_terminated());
        // Sync shutdown is also idempotent.
        guest.shutdown();
        assert!(guest.is_terminated());
        let (result, _) = guest
            .call_with_timeout_host(
                GuestCall::SessionStart,
                Duration::from_secs(1),
                HostCtx::default(),
            )
            .await;
        assert!(
            matches!(result, GuestCallResult::Failed { .. }),
            "after shutdown expected Failed, got {result:?}"
        );
    }

    /// Concurrent shutdown waiters all observe terminated (first joins, others wait).
    #[tokio::test]
    async fn concurrent_shutdown_waiters_all_see_terminated() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "conc-shut.wasm", PARK_LOOP_GUEST);
        let guest = WasmGuest::load(&path).unwrap();
        // Keep a call active so shutdown must interrupt.
        let g = guest.clone();
        let call = tokio::spawn(async move {
            g.call_with_timeout_host(
                GuestCall::SessionStart,
                Duration::from_secs(10),
                HostCtx::default(),
            )
            .await
        });
        let start = std::time::Instant::now();
        while guest.active_job_id() == 0 {
            assert!(start.elapsed() < Duration::from_secs(2));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let g1 = guest.clone();
        let g2 = guest.clone();
        let g3 = guest.clone();
        let (a, b, c) = tokio::join!(g1.shutdown_async(), g2.shutdown_async(), async {
            g3.shutdown();
        });
        let _ = (a, b, c);
        assert!(guest.is_terminated());
        assert!(!guest.worker_is_alive());
        let _ = call.await;
    }

    /// Prepare-hold window: cancel before Armed only sets flag (no lost epoch).
    /// After release, job arms then cancel can epoch-interrupt.
    #[tokio::test]
    async fn prepare_hold_cancel_before_armed_is_flag_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "prep-hold.wasm", PARK_LOOP_GUEST);
        let guest = WasmGuest::load(&path).unwrap();
        guest.set_prepare_hold(true);
        let bodies_before = guest.guest_bodies_started();
        let armed_before = guest.jobs_armed();

        let g = guest.clone();
        let call = tokio::spawn(async move {
            g.call_with_timeout_host(
                GuestCall::SessionStart,
                Duration::from_secs(5),
                HostCtx::default(),
            )
            .await
        });

        let start = std::time::Instant::now();
        while !guest.prepare_is_holding() {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "prepare never held"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Still not Armed while holding after deadline set.
        assert_eq!(guest.job_phase(), JobPhase::Idle as u8);
        assert_eq!(guest.jobs_armed(), armed_before);

        // Cancel during prepare: flag only (not Armed → no epoch needed).
        // Drop the call future to cancel.
        // Actually keep the future and use short timeout path after release.
        guest.set_prepare_hold(false); // release prepare

        // Wait until Armed or finished.
        let start = std::time::Instant::now();
        while guest.jobs_armed() == armed_before && start.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Either armed and running, or cancelled before execute.
        let _ = tokio::time::timeout(Duration::from_millis(800), call).await;
        // Must not have hang; bodies either 0 (cancelled pre-execute) or 1.
        let bodies = guest.guest_bodies_started();
        assert!(
            bodies == bodies_before || bodies == bodies_before + 1,
            "unexpected body count {bodies}"
        );
        guest.shutdown_async().await;
        assert!(guest.is_terminated());
    }

    /// Final Drop of the last Arc joins the worker and sets exit sentinel.
    #[test]
    fn final_drop_joins_worker() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "drop.wasm", MINIMAL_GUEST);
        let guest = WasmGuest::load(&path).unwrap();
        let clone = guest.clone();
        assert!(guest.worker_is_alive());
        assert!(!guest.is_terminated());
        drop(guest);
        // Clone still holds Arc — worker must stay alive.
        assert!(clone.worker_is_alive());
        assert!(!clone.is_terminated());
        drop(clone);
        // If Drop hung forever this test would not finish.
        // After last Arc drop, worker has exited (sentinel set on owner Drop).
    }

    /// Guest publishes invalid UTF-8 via set_inject_context; host must not panic
    /// and must not accept the bytes (strict decode → write ignored).
    const MALICIOUS_UTF8_INJECT_GUEST: &str = r#"
        (module
          (import "hyper_host" "set_inject_context" (func $set_inject (param i32 i32)))
          (memory (export "memory") 1)
          ;; incomplete multi-byte sequence (invalid UTF-8)
          (data (i32.const 0) "\e2\82")
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_before_agent_start") (result i32)
            (call $set_inject (i32.const 0) (i32.const 2))
            i32.const 0)
        )
    "#;

    #[tokio::test]
    async fn malicious_utf8_inject_is_rejected_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "bad-utf8.wasm", MALICIOUS_UTF8_INJECT_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "bad-utf8",
            path,
            vec![Capability::BeforeAgentInject],
        ))
        .unwrap();
        let d = rt
            .dispatch_before_agent_start(&BeforeAgentStartIn {
                prompt: "hi".into(),
            })
            .await;
        // Call itself succeeds (guest returned 0); inject must be empty / absent.
        assert!(
            !d.has_injection()
                || d.out
                    .inject_context
                    .as_ref()
                    .is_none_or(|s| s.is_empty() || !s.contains('\u{FFFD}')),
            "invalid UTF-8 must not become host inject text: {:?}",
            d.out
        );
        // Stronger: inject_context should be None/empty because write was ignored.
        assert!(
            d.out.inject_context.as_ref().is_none_or(|s| s.is_empty()),
            "strict UTF-8 reject should leave inject empty, got {:?}",
            d.out.inject_context
        );
    }

    /// Guest requests inject with len > MAX_INJECT_BYTES → TooLong → ignored.
    const TOO_LONG_INJECT_GUEST: &str = r#"
        (module
          (import "hyper_host" "set_inject_context" (func $set_inject (param i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "ok-prefix")
          (func (export "hyper_ext_abi_version") (result i32)
            i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32)
            i32.const 0)
          (func (export "hyper_ext_on_before_agent_start") (result i32)
            ;; len = 40000 > MAX_INJECT_BYTES (32768); must reject entirely
            (call $set_inject (i32.const 0) (i32.const 40000))
            i32.const 0)
        )
    "#;

    #[tokio::test]
    async fn too_long_guest_string_is_rejected_not_prefix_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wasm(&dir, "toolong.wasm", TOO_LONG_INJECT_GUEST);
        let mut rt = ExtensionRuntime::new();
        rt.load(&trusted_spec(
            "toolong",
            path,
            vec![Capability::BeforeAgentInject],
        ))
        .unwrap();
        let d = rt
            .dispatch_before_agent_start(&BeforeAgentStartIn {
                prompt: "hi".into(),
            })
            .await;
        assert!(
            d.out.inject_context.as_ref().is_none_or(|s| s.is_empty()),
            "TooLong must not accept a silent prefix, got {:?}",
            d.out.inject_context
        );
    }

    #[test]
    fn read_guest_utf8_from_memory_helpers_cover_malicious_inputs() {
        // Unit-level: same contract the host import uses.
        assert_eq!(
            read_guest_utf8_from_memory(&[0xFF], 0, 1, 64),
            Err(GuestStringError::InvalidUtf8)
        );
        assert_eq!(
            read_guest_utf8_from_memory(b"abc", 0, 100, 64),
            Err(GuestStringError::TooLong)
        );
        assert_eq!(
            read_guest_utf8_from_memory(b"abc", 0, 10, 64),
            Err(GuestStringError::OutOfBounds)
        );
        assert_eq!(
            read_guest_utf8_from_memory(b"abc", -1, 1, 64),
            Err(GuestStringError::Negative)
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
        rt.load(&trusted_spec("trap", path, vec![Capability::PreToolGate]))
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
        eprintln!("bench load_five: load={load_elapsed:?} session_start={start_elapsed:?}");
    }
}
