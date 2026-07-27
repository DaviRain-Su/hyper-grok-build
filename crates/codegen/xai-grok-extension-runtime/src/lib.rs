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
//! | `stop_hook_active` | `() -> i32` | 1 if stop gate already continued |
//! | `compact_reason_len` / `compact_reason_byte` | | pre_compact reason |
//!
//! Component Model + WIT (`hyper:extension@0.1.0`) remains the long-term target.
//! See `docs/design-wasm-extensions.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use xai_grok_extension_api::{
    timeouts, BeforeAgentStartIn, BeforeAgentStartOut, Capability, ContractError, ExtensionSpec,
    PreCompactIn, PreToolIn, StopIn, StopOut, CORE_ABI_VERSION, EXPORT_ABI_VERSION,
    EXPORT_ON_BEFORE_AGENT_START, EXPORT_ON_PRE_COMPACT, EXPORT_ON_PRE_TOOL_USE, EXPORT_ON_SESSION_END,
    EXPORT_ON_SESSION_START, EXPORT_ON_STOP, MAX_INJECT_BYTES,
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
}

/// Host-side state visible to guest imports during a call.
#[derive(Debug, Clone, Default)]
struct HostCtx {
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
}

/// Per-session registry of loaded extensions.
#[derive(Default, Clone)]
pub struct ExtensionRuntime {
    guests: Vec<LoadedGuest>,
}

#[derive(Clone)]
struct LoadedGuest {
    name: String,
    capabilities: Vec<Capability>,
    #[cfg(feature = "wasm")]
    inner: WasmGuest,
    #[cfg(not(feature = "wasm"))]
    _inner: (),
}

impl ExtensionRuntime {
    pub fn new() -> Self {
        Self::default()
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
            return Err(ContractError::NotTrusted.into());
        }
        #[cfg(feature = "wasm")]
        {
            let inner = WasmGuest::load(&spec.wasm_path)?;
            self.guests.push(LoadedGuest {
                name: spec.name.clone(),
                capabilities: spec.capabilities.clone(),
                inner,
            });
            Ok(())
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = spec;
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
        let mut merged = BeforeAgentStartOut::default();
        let mut results = Vec::new();
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            if !guest.capabilities.contains(&Capability::BeforeAgentInject) {
                results.push((
                    guest.name.clone(),
                    GuestCallResult::SkippedCapability {
                        extension: guest.name.clone(),
                        capability: Capability::BeforeAgentInject,
                    },
                ));
                continue;
            }
            let host = HostCtx {
                prompt: input.prompt.clone(),
                ..HostCtx::default()
            };
            let (r, host_out) = guest
                .inner
                .call_with_timeout_host(GuestCall::BeforeAgentStart, timeouts::BEFORE_AGENT, host)
                .await;
            if matches!(&r, GuestCallResult::Ok { code: 0, .. }) {
                let piece = BeforeAgentStartOut {
                    inject_context: non_empty(host_out.inject_context),
                    append_system: non_empty(host_out.append_system),
                };
                // Tag source extension for multi-plugin merge readability.
                let piece = tag_extension_out(piece, &guest.name);
                merged = merged.merge_append(piece);
            }
            results.push((guest.name.clone(), r));
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = input;
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
                ..HostCtx::default()
            };
            let (r, _) = guest
                .inner
                .call_with_timeout_host(GuestCall::Stop, timeouts::GATE, host)
                .await;
            let blocked = matches!(&r, GuestCallResult::Ok { code: 1, .. });
            let name = guest.name.clone();
            results.push((name.clone(), r));
            if blocked {
                return StopDispatch {
                    decision: StopOut::Block {
                        reason: format!("blocked by wasm extension `{name}`"),
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
                ..HostCtx::default()
            };
            let (r, _) = guest
                .inner
                .call_with_timeout_host(GuestCall::PreCompact, timeouts::OBSERVE, host)
                .await;
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
    /// Trap/timeout/missing export = fail-open.
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
                ..HostCtx::default()
            };
            let (r, _) = guest
                .inner
                .call_with_timeout_host(GuestCall::PreToolUse, timeouts::GATE, host)
                .await;
            let denied = matches!(&r, GuestCallResult::Ok { code: 1, .. });
            let name = guest.name.clone();
            results.push((name.clone(), r));
            if denied {
                return PreToolDispatch {
                    decision: PreToolDecision::Deny {
                        extension: name.clone(),
                        reason: format!(
                            "denied by wasm extension `{name}` (tool `{}`)",
                            input.tool_name
                        ),
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

    async fn dispatch_all_observe(
        &self,
        call: GuestCall,
        timeout: Duration,
    ) -> Vec<GuestCallResult> {
        let mut out = Vec::with_capacity(self.guests.len());
        #[cfg(feature = "wasm")]
        for guest in &self.guests {
            let (r, _) = guest
                .inner
                .call_with_timeout_host(call, timeout, HostCtx::default())
                .await;
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
    Stop,
    PreCompact,
}

/// Outcome of one guest invocation (for UI / scrollback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestCallResult {
    Ok { extension: String, code: i32 },
    SkippedExport { extension: String, export: &'static str },
    SkippedCapability {
        extension: String,
        capability: Capability,
    },
    Failed { extension: String, error: String },
    Timeout { extension: String, limit: Duration },
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

#[cfg(feature = "wasm")]
#[derive(Clone)]
struct WasmGuest {
    name_for_logs: String,
    engine: wasmtime::Engine,
    module: Arc<wasmtime::Module>,
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
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine =
            wasmtime::Engine::new(&config).map_err(|e| RuntimeError::Module(e.to_string()))?;
        let module = wasmtime::Module::new(&engine, bytes)
            .map_err(|e| RuntimeError::Module(e.to_string()))?;

        // Validate ABI at load time.
        let mut store = wasmtime::Store::new(&engine, HostCtx::default());
        store
            .set_fuel(1_000_000)
            .map_err(|e| RuntimeError::Module(e.to_string()))?;
        let linker = build_linker(&engine)?;
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
        })
    }

    async fn call_with_timeout_host(
        &self,
        call: GuestCall,
        limit: Duration,
        host: HostCtx,
    ) -> (GuestCallResult, HostCtx) {
        let engine = self.engine.clone();
        let module = Arc::clone(&self.module);
        let name = self.name_for_logs.clone();
        let export = match call {
            GuestCall::SessionStart => EXPORT_ON_SESSION_START,
            GuestCall::SessionEnd => EXPORT_ON_SESSION_END,
            GuestCall::PreToolUse => EXPORT_ON_PRE_TOOL_USE,
            GuestCall::BeforeAgentStart => EXPORT_ON_BEFORE_AGENT_START,
            GuestCall::Stop => EXPORT_ON_STOP,
            GuestCall::PreCompact => EXPORT_ON_PRE_COMPACT,
        };

        let join = tokio::task::spawn_blocking(move || {
            let mut store = wasmtime::Store::new(&engine, host);
            if let Err(e) = store.set_fuel(10_000_000) {
                let host = store.into_data();
                return (
                    GuestCallResult::Failed {
                        extension: name,
                        error: e.to_string(),
                    },
                    host,
                );
            }
            let linker = match build_linker(&engine) {
                Ok(l) => l,
                Err(e) => {
                    let host = store.into_data();
                    return (
                        GuestCallResult::Failed {
                            extension: name,
                            error: e.to_string(),
                        },
                        host,
                    );
                }
            };
            let instance = match linker.instantiate(&mut store, &module) {
                Ok(i) => i,
                Err(e) => {
                    let host = store.into_data();
                    return (
                        GuestCallResult::Failed {
                            extension: name,
                            error: e.to_string(),
                        },
                        host,
                    );
                }
            };
            let func = match instance.get_typed_func::<(), i32>(&mut store, export) {
                Ok(f) => f,
                Err(_) => {
                    let host = store.into_data();
                    return (
                        GuestCallResult::SkippedExport {
                            extension: name,
                            export,
                        },
                        host,
                    );
                }
            };
            let result = match func.call(&mut store, ()) {
                Ok(code) => GuestCallResult::Ok {
                    extension: name,
                    code,
                },
                Err(e) => GuestCallResult::Failed {
                    extension: name,
                    error: e.to_string(),
                },
            };
            let host = store.into_data();
            (result, host)
        });

        match tokio::time::timeout(limit, join).await {
            Ok(Ok(pair)) => pair,
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
}
