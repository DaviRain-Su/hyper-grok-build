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
    pub fn new(runtime: ExtensionRuntime, desc: WasmToolDescriptor) -> Self {
        let tool_id = desc.client_name();
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
            tool_id,
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
        ToolId::new(&self.tool_id).unwrap_or_else(|_| {
            ToolId::new("wasm_tool").expect("static tool id")
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
            .map_err(|e| ToolError::invalid_arguments(e.to_string()))
    }
}

/// Unregister previous `wasm_*` tools and re-register from the extension runtime.
pub async fn sync_wasm_tools_to_bridge(
    bridge: &xai_grok_tools::bridge::ToolBridge,
    runtime: &ExtensionRuntime,
) -> usize {
    let removed = bridge.unregister_tools_by_prefix(WASM_TOOL_PREFIX);
    if removed > 0 {
        tracing::info!(removed, "unregistered prior wasm_* tools");
    }
    let tools = runtime.collect_registered_tools().await;
    let mut registered = 0usize;
    for desc in tools {
        let client = desc.client_name();
        let schema = desc.parsed_schema();
        let tool = WasmExtensionTool::new(runtime.clone(), desc);
        match bridge
            .register_mcp_tools(client.clone(), tool, Some(schema))
            .await
        {
            Ok(()) => {
                tracing::info!(tool = %client, "registered wasm extension tool");
                registered += 1;
            }
            Err(e) => {
                tracing::warn!(tool = %client, error = %e, "failed to register wasm tool");
            }
        }
    }
    registered
}

