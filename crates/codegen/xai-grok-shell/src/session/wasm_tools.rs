//! Register tools advertised by WASM extensions onto the session tool bridge.

use xai_grok_extension_api::WasmToolDescriptor;
use xai_grok_extension_runtime::ExtensionRuntime;
use xai_grok_tools::types::tool::{ToolKind, ToolNamespace};
use xai_grok_tools::types::tool_metadata::ToolMetadata;
use xai_tool_runtime::{Tool, ToolCallContext, ToolError, ToolId};
use xai_tool_types::ToolDescription;

/// Client name prefix for WASM extension tools (`wasm_...`).
pub const WASM_TOOL_PREFIX: &str = "wasm_";

/// Telemetry categories for wasm gate denials (no free-form guest text).
pub const DENY_CAT_EXPLICIT: &str = "explicit_deny";
pub const DENY_CAT_TRAP_CLOSED: &str = "trap_fail_closed";
pub const DENY_CAT_TIMEOUT_CLOSED: &str = "timeout_fail_closed";

/// Map a host-facing deny reason string to a telemetry category.
///
/// Free-form guest reasons always map to [`DENY_CAT_EXPLICIT`]; only host-generated
/// fail-closed messages are classified as trap/timeout.
pub fn deny_category_from_reason(reason: &str) -> &'static str {
    let r = reason.to_ascii_lowercase();
    if !r.contains("failed closed") {
        return DENY_CAT_EXPLICIT;
    }
    // Host phrases: "trap/timeout", "timeout", or "trap".
    let has_timeout = r.contains("timeout");
    let has_trap = r.contains("trap");
    match (has_trap, has_timeout) {
        (false, true) => DENY_CAT_TIMEOUT_CLOSED,
        (true, false) => DENY_CAT_TRAP_CLOSED,
        // Combined host wording uses trap/timeout — treat as trap_fail_closed.
        (true, true) => DENY_CAT_TRAP_CLOSED,
        (false, false) => DENY_CAT_TRAP_CLOSED,
    }
}

/// Emit runtime counters to tracing **and** the product/dual telemetry funnel.
///
/// - Always: structured log (`target=wasm_extension`, same as
///   [`ExtensionRuntime::log_metrics`](xai_grok_extension_runtime::ExtensionRuntime::log_metrics)).
/// - Product Mixpanel / events: only when `telemetry_enabled`.
/// - External OTEL: via [`log_event_dual`] when the external stream is active.
pub fn emit_runtime_metrics(
    telemetry_enabled: bool,
    reason: &str,
    runtime: &ExtensionRuntime,
) {
    let snap = runtime.metrics();
    snap.log_tracing(reason);
    xai_grok_telemetry::session_ctx::log_event_dual(
        telemetry_enabled,
        xai_grok_telemetry::events::WasmExtensionMetrics {
            reason: reason.to_string(),
            extension_count: runtime.len() as u32,
            loads_ok: snap.loads_ok,
            loads_failed: snap.loads_failed,
            calls_ok: snap.calls_ok,
            calls_failed: snap.calls_failed,
            calls_timeout: snap.calls_timeout,
            pre_tool_denies: snap.pre_tool_denies,
            stop_blocks: snap.stop_blocks,
            tools_collected: snap.tools_collected,
            tools_invoked_ok: snap.tools_invoked_ok,
            tools_invoked_err: snap.tools_invoked_err,
            guest_log_lines: snap.guest_log_lines,
        },
    );
}

/// Product telemetry for a wasm pre_tool deny (in addition to [`HookBlocked`]
/// with `hook_name = wasm:{ext}`). Uses a **category**, never free-form guest text.
pub fn emit_wasm_extension_blocked(
    telemetry_enabled: bool,
    extension: &str,
    tool_name: &str,
    category: &str,
) {
    xai_grok_telemetry::session_ctx::log_event_dual(
        telemetry_enabled,
        xai_grok_telemetry::events::WasmExtensionBlocked {
            extension: extension.to_string(),
            tool_name: Some(tool_name.to_string()),
            category: category.to_string(),
        },
    );
}

/// Dynamic tool that forwards `run` into a loaded WASM guest.
pub struct WasmExtensionTool {
    runtime: ExtensionRuntime,
    extension: String,
    short_name: String,
    description: String,
    tool_id: String,
}

impl std::fmt::Debug for WasmExtensionTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmExtensionTool")
            .field("extension", &self.extension)
            .field("short_name", &self.short_name)
            .field("tool_id", &self.tool_id)
            .finish_non_exhaustive()
    }
}

impl WasmExtensionTool {
    pub fn new(
        runtime: ExtensionRuntime,
        desc: WasmToolDescriptor,
        client_name: String,
    ) -> Self {
        let short_name = desc.name;
        let description = if desc.description.is_empty() {
            format!("WASM extension tool `{short_name}`")
        } else {
            desc.description
        };
        Self {
            runtime,
            extension: desc.extension,
            short_name,
            description,
            tool_id: client_name,
        }
    }
}

impl ToolMetadata for WasmExtensionTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::MCP
    }

    fn description_template(&self) -> &str {
        &self.description
    }
}

impl Tool for WasmExtensionTool {
    type Args = serde_json::Value;
    type Output = String;

    fn id(&self) -> ToolId {
        // client_name is already sanitized; fall back to a unique-ish id.
        ToolId::new(&self.tool_id).unwrap_or_else(|_| {
            let fallback = format!("wasm_fallback_{}", self.tool_id.len());
            ToolId::new(&fallback).unwrap_or_else(|_| {
                ToolId::new("wasm_fallback").expect("static tool id")
            })
        })
    }

    fn description(&self, _ctx: &xai_tool_runtime::ListToolsContext) -> ToolDescription {
        ToolDescription::new(&self.tool_id, &self.description)
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        input: serde_json::Value,
    ) -> Result<String, ToolError> {
        let args = input.to_string();
        self.runtime
            .invoke_registered_tool(&self.extension, &self.short_name, &args)
            .await
            .map_err(|e| ToolError::not_implemented(e.to_string()))
    }
}

/// Drop session-owned `wasm_*` tools from the shared ToolBridge.
///
/// Must run on session end / shutdown so closed sessions do not leave tools
/// visible to other sessions (production leak fix).
pub fn unregister_session_wasm_tools(
    bridge: &xai_grok_tools::bridge::ToolBridge,
    previously_registered: &mut Vec<String>,
) -> usize {
    let mut n = 0usize;
    for name in previously_registered.drain(..) {
        if bridge.unregister_tool_by_name(&name) {
            tracing::debug!(tool = %name, "unregistered session-owned wasm tool");
            n += 1;
        }
    }
    n
}

/// Unregister only tools this session previously registered, then re-register
/// from the extension runtime with **session-scoped client names** so concurrent
/// sessions do not collide on the shared ToolBridge (Oracle finding).
///
/// `session_id` is shortened into the client name via
/// [`xai_grok_extension_api::short_session_token`].
pub async fn sync_wasm_tools_to_bridge(
    bridge: &xai_grok_tools::bridge::ToolBridge,
    runtime: &ExtensionRuntime,
    previously_registered: &mut Vec<String>,
    session_id: &str,
) -> usize {
    unregister_session_wasm_tools(bridge, previously_registered);
    let tools = runtime.collect_registered_tools().await;
    let mut registered = 0usize;
    let session_key = Some(session_id);
    for desc in tools {
        let mut client = desc.client_name_for_session(session_key);
        if !client.starts_with(WASM_TOOL_PREFIX) {
            tracing::warn!(tool = %client, "skipping non-wasm_* client name");
            continue;
        }
        // Collision fallback: append numeric suffix (another session/tool race).
        let schema = desc.parsed_schema();
        let mut attempt = 0u32;
        loop {
            let tool = WasmExtensionTool::new(runtime.clone(), desc.clone(), client.clone());
            match bridge
                .register_mcp_tools(client.clone(), tool, Some(schema.clone()))
                .await
            {
                Ok(()) => {
                    tracing::info!(tool = %client, "registered wasm extension tool");
                    previously_registered.push(client);
                    registered += 1;
                    break;
                }
                Err(e) => {
                    attempt += 1;
                    if attempt > 8 {
                        tracing::warn!(
                            tool = %client,
                            error = %e,
                            "failed to register wasm tool after retries"
                        );
                        break;
                    }
                    client = format!("{client}_{attempt}");
                    tracing::debug!(
                        tool = %client,
                        attempt,
                        "retrying wasm tool registration with unique suffix"
                    );
                }
            }
        }
    }
    registered
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use xai_grok_extension_api::{Capability, ExtensionSpec};
    use xai_grok_telemetry::events::TelemetryEvent;

    #[test]
    fn telemetry_event_names_are_stable() {
        assert_eq!(
            xai_grok_telemetry::events::WasmExtensionMetrics::NAME,
            "wasm_extension_metrics"
        );
        assert_eq!(
            xai_grok_telemetry::events::WasmExtensionBlocked::NAME,
            "wasm_extension_blocked"
        );
    }

    #[test]
    fn deny_category_classifies_host_fail_closed() {
        assert_eq!(
            deny_category_from_reason(
                "wasm extension `x` failed closed (trap/timeout on tool `t`)"
            ),
            DENY_CAT_TRAP_CLOSED
        );
        assert_eq!(
            deny_category_from_reason("denied by wasm extension `x` (tool `t`)"),
            DENY_CAT_EXPLICIT
        );
        assert_eq!(
            deny_category_from_reason("guest said: user@secret.example/token"),
            DENY_CAT_EXPLICIT
        );
    }

    /// Session-level smoke: load fixture guest → register session-scoped tools
    /// on a real ToolBridge → unregister cleanly (production multi-session path).
    #[tokio::test]
    async fn sync_and_unregister_wasm_tools_smoke() {
        let wasm = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../xai-grok-extension-runtime/examples/rust-guest-template/extension.wasm");
        if !wasm.is_file() {
            eprintln!("skip: no rust-guest-template/extension.wasm");
            return;
        }

        let mut rt = ExtensionRuntime::new();
        rt.load(&ExtensionSpec {
            name: "smoke-ext".into(),
            wasm_path: wasm,
            capabilities: vec![Capability::RegisterTool],
            trusted: true,
            gate_fail: None,
            plugin_data_dir: Some(PathBuf::from("/tmp/hyper-ext-smoke-data")),
        })
        .expect("load fixture wasm");

        let bridge = xai_grok_tools::bridge::ToolBridge::for_test();
        let mut owned = Vec::new();
        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let n = sync_wasm_tools_to_bridge(&bridge, &rt, &mut owned, session_id).await;
        assert!(n >= 1, "expected at least one wasm tool from template");
        assert_eq!(owned.len(), n);
        for name in &owned {
            assert!(
                name.starts_with(WASM_TOOL_PREFIX),
                "client name must be wasm_*: {name}"
            );
            assert!(
                name.contains("aaaaaaaa") || name.contains("smoke"),
                "session or ext fragment expected in {name}"
            );
            assert!(
                bridge.tool_kind(name).is_some(),
                "registered tool should be visible on bridge: {name}"
            );
        }

        // Metrics should have collected tools.
        let m = rt.metrics();
        assert!(m.loads_ok >= 1);
        assert!(m.tools_collected >= 1);

        let dropped = unregister_session_wasm_tools(&bridge, &mut owned);
        assert_eq!(dropped, n);
        assert!(owned.is_empty());
    }

    #[tokio::test]
    async fn two_sessions_get_distinct_client_names() {
        let wasm = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../xai-grok-extension-runtime/examples/rust-guest-template/extension.wasm");
        if !wasm.is_file() {
            return;
        }
        let mut rt = ExtensionRuntime::new();
        rt.load(&ExtensionSpec {
            name: "echo-ext".into(),
            wasm_path: wasm,
            capabilities: vec![Capability::RegisterTool],
            trusted: true,
            gate_fail: None,
            plugin_data_dir: None,
        })
        .unwrap();

        let bridge = xai_grok_tools::bridge::ToolBridge::for_test();
        let mut a = Vec::new();
        let mut b = Vec::new();
        let n1 = sync_wasm_tools_to_bridge(&bridge, &rt, &mut a, "11111111-1111-1111-1111-111111111111")
            .await;
        let n2 = sync_wasm_tools_to_bridge(&bridge, &rt, &mut b, "22222222-2222-2222-2222-222222222222")
            .await;
        assert!(n1 >= 1 && n2 >= 1);
        // Names must not collide across sessions on the shared bridge.
        for name in &a {
            assert!(!b.contains(name), "session B must not share name {name}");
        }
        unregister_session_wasm_tools(&bridge, &mut a);
        unregister_session_wasm_tools(&bridge, &mut b);
    }
}
