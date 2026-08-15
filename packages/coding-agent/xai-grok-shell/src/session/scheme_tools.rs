//! Register tools advertised by scheme extensions onto the session tool bridge.
//!
//! Mirror of [`crate::session::wasm_tools`] for the scheme live runtime:
//! plugins with the `register_tool` capability call `register-tool!` in their
//! policy script; the host collects descriptors at session start and forwards
//! `run` into the image via `invoke-tool`.

use xai_grok_extension_api::SchemeToolDescriptor;
use xai_grok_scheme_runtime::SchemeRuntime;
use xai_grok_tools::types::tool::{ToolKind, ToolNamespace};
use xai_grok_tools::types::tool_metadata::ToolMetadata;
use xai_tool_runtime::{Tool, ToolCallContext, ToolError, ToolId};
use xai_tool_types::ToolDescription;

/// Client name prefix for scheme extension tools (`scheme_...`).
pub const SCHEME_TOOL_PREFIX: &str = "scheme_";

/// Dynamic tool that forwards `run` into the scheme live image.
pub struct SchemeExtensionTool {
    runtime: SchemeRuntime,
    extension: String,
    short_name: String,
    description: String,
    tool_id: String,
}

impl std::fmt::Debug for SchemeExtensionTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemeExtensionTool")
            .field("extension", &self.extension)
            .field("short_name", &self.short_name)
            .field("tool_id", &self.tool_id)
            .finish_non_exhaustive()
    }
}

impl SchemeExtensionTool {
    pub fn new(runtime: SchemeRuntime, desc: SchemeToolDescriptor, client_name: String) -> Self {
        let short_name = desc.name;
        let description = if desc.description.is_empty() {
            format!("Scheme extension tool `{short_name}`")
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

impl ToolMetadata for SchemeExtensionTool {
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

impl Tool for SchemeExtensionTool {
    type Args = serde_json::Value;
    type Output = String;

    fn id(&self) -> ToolId {
        ToolId::new(&self.tool_id).unwrap_or_else(|_| {
            let fallback = format!("scheme_fallback_{}", self.tool_id.len());
            ToolId::new(&fallback)
                .unwrap_or_else(|_| ToolId::new("scheme_fallback").expect("static tool id"))
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
            .map_err(ToolError::not_implemented)
    }
}

/// Drop session-owned `scheme_*` tools from the shared ToolBridge.
pub fn unregister_session_scheme_tools(
    bridge: &xai_grok_tools::bridge::ToolBridge,
    previously_registered: &mut Vec<String>,
) -> usize {
    let mut n = 0usize;
    for name in previously_registered.drain(..) {
        if bridge.unregister_tool_by_name(&name) {
            tracing::debug!(tool = %name, "unregistered session-owned scheme tool");
            n += 1;
        }
    }
    n
}

/// Unregister this session's previously registered scheme tools, then
/// re-register from the runtime with session-scoped client names (same
/// collision rules as `sync_wasm_tools_to_bridge`).
pub async fn sync_scheme_tools_to_bridge(
    bridge: &xai_grok_tools::bridge::ToolBridge,
    runtime: &SchemeRuntime,
    previously_registered: &mut Vec<String>,
    session_id: &str,
) -> usize {
    unregister_session_scheme_tools(bridge, previously_registered);
    let tools = runtime.collect_registered_tools().await;
    let mut registered = 0usize;
    let session_key = Some(session_id);
    for desc in tools {
        let mut client = desc.client_name_for_session(session_key);
        if !client.starts_with(SCHEME_TOOL_PREFIX) {
            tracing::warn!(tool = %client, "skipping non-scheme_* client name");
            continue;
        }
        let schema = desc.parsed_schema();
        let mut attempt = 0u32;
        loop {
            let tool = SchemeExtensionTool::new(runtime.clone(), desc.clone(), client.clone());
            match bridge
                .register_mcp_tools(client.clone(), tool, Some(schema.clone()))
                .await
            {
                Ok(()) => {
                    tracing::info!(tool = %client, "registered scheme extension tool");
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
                            "failed to register scheme tool after retries"
                        );
                        break;
                    }
                    client = format!("{client}_{attempt}");
                }
            }
        }
    }
    registered
}
