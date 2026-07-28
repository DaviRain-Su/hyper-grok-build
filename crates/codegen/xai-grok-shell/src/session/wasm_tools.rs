//! Register tools advertised by WASM extensions onto the session tool bridge.

use xai_grok_extension_api::WasmToolDescriptor;
use xai_grok_extension_runtime::ExtensionRuntime;
use xai_grok_tools::types::tool::{ToolKind, ToolNamespace};
use xai_grok_tools::types::tool_metadata::ToolMetadata;
use xai_tool_runtime::{Tool, ToolCallContext, ToolError, ToolId};
use xai_tool_types::ToolDescription;

/// Client name prefix for WASM extension tools (`wasm_...`).
pub const WASM_TOOL_PREFIX: &str = "wasm_";

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
