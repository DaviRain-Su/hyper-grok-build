//! `dap_debug` — DAP (Debug Adapter Protocol) tool **stub**.
//!
//! Full adapter process lifecycle (launch/attach/step/breakpoints) is not
//! wired yet. The tool is registered so models and docs have a stable id;
//! every op returns a structured status explaining how to enable future work
//! and what ops will mean when the adapter lands.
//!
//! Enable advertisement via `[features] dap_debug = true` (when config is
//! wired) or treat as always present but inert.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

pub const DAP_DEBUG_TOOL_NAME: &str = "dap_debug";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum DapDebugOp {
    /// Report stub status and planned capability surface.
    #[default]
    Status,
    /// Reserved: launch a debuggee under a DAP adapter.
    Launch,
    /// Reserved: attach to a running process / port.
    Attach,
    /// Reserved: continue / step / pause / disconnect (string in `command`).
    Control,
    /// Reserved: set or clear breakpoints.
    Breakpoints,
    /// Reserved: evaluate expression in the current frame.
    Evaluate,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DapDebugInput {
    #[serde(default)]
    #[schemars(description = "Operation: status (default), launch, attach, control, breakpoints, evaluate")]
    pub op: DapDebugOp,
    #[serde(default)]
    #[schemars(description = "Program path or attach target (reserved)")]
    pub program: Option<String>,
    #[serde(default)]
    #[schemars(description = "Adapter command override, e.g. lldb-dap (reserved)")]
    pub adapter: Option<String>,
    #[serde(default)]
    #[schemars(description = "Control command when op=control: continue|next|step_in|step_out|pause|disconnect")]
    pub command: Option<String>,
    #[serde(default)]
    #[schemars(description = "Expression for evaluate (reserved)")]
    pub expression: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapDebugOutput {
    pub ok: bool,
    pub stub: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planned_ops: Option<Vec<String>>,
}

impl DapDebugOutput {
    fn stub(summary: impl Into<String>) -> Self {
        Self {
            ok: false,
            stub: true,
            summary: summary.into(),
            planned_ops: Some(vec![
                "status".into(),
                "launch".into(),
                "attach".into(),
                "control".into(),
                "breakpoints".into(),
                "evaluate".into(),
            ]),
        }
    }
}

#[derive(Debug, Default)]
pub struct DapDebugTool;

impl crate::types::tool_metadata::ToolMetadata for DapDebugTool {
    fn kind(&self) -> ToolKind {
        // Execute-class: when live, drives external debuggee processes.
        ToolKind::Execute
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Debug Adapter Protocol (DAP) session control — **stub**.

Ops (reserved until an adapter is wired):
- status: report whether DAP is available
- launch / attach: start or attach a debuggee
- control: continue, next, step_in, step_out, pause, disconnect
- breakpoints: set/clear
- evaluate: expression in the current stack frame

Today every op returns a stub response. Prefer run_terminal_command with a
local debugger for real debugging until this tool is fully implemented.
"#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }

    fn is_read_only(&self) -> bool {
        // Stub never mutates; when live, launch/control will not be read-only.
        true
    }
}

impl xai_tool_runtime::Tool for DapDebugTool {
    type Args = DapDebugInput;
    type Output = DapDebugOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(DAP_DEBUG_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            DAP_DEBUG_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "new_tool.dap_debug", skip_all, fields(op = ?input.op))]
    async fn run(
        &self,
        _ctx: xai_tool_runtime::ToolCallContext,
        input: DapDebugInput,
    ) -> Result<DapDebugOutput, xai_tool_runtime::ToolError> {
        let summary = match input.op {
            DapDebugOp::Status => {
                "dap_debug is a stub: no DAP adapter process is managed yet. \
                 Configure [features] dap_debug = true for future enablement; \
                 use a local debugger via the shell for now."
                    .to_string()
            }
            DapDebugOp::Launch => format!(
                "dap_debug launch is not implemented (program={:?}, adapter={:?})",
                input.program, input.adapter
            ),
            DapDebugOp::Attach => format!(
                "dap_debug attach is not implemented (program={:?}, adapter={:?})",
                input.program, input.adapter
            ),
            DapDebugOp::Control => format!(
                "dap_debug control is not implemented (command={:?})",
                input.command
            ),
            DapDebugOp::Breakpoints => {
                "dap_debug breakpoints is not implemented".to_string()
            }
            DapDebugOp::Evaluate => format!(
                "dap_debug evaluate is not implemented (expression={:?})",
                input.expression
            ),
        };
        Ok(DapDebugOutput::stub(summary))
    }
}

impl xai_tool_runtime::ToolOutput for DapDebugOutput {
    fn model_output(&self) -> Vec<xai_tool_runtime::ContentBlock> {
        let mut text = self.summary.clone();
        if let Some(ops) = &self.planned_ops {
            text.push_str("\nPlanned ops: ");
            text.push_str(&ops.join(", "));
        }
        vec![xai_tool_runtime::ContentBlock::Text { text }]
    }
}

impl From<DapDebugOutput> for crate::types::output::ToolOutput {
    fn from(o: DapDebugOutput) -> Self {
        let mut text = o.summary;
        if let Some(ops) = o.planned_ops {
            text.push_str("\nPlanned ops: ");
            text.push_str(&ops.join(", "));
        }
        crate::types::output::ToolOutput::Text(text.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::resources::Resources;
    use crate::types::tool_metadata::test_ctx;

    #[tokio::test]
    async fn status_is_stub() {
        let out = xai_tool_runtime::Tool::run(
            &DapDebugTool,
            test_ctx(Resources::new().into_shared()),
            DapDebugInput {
                op: DapDebugOp::Status,
                program: None,
                adapter: None,
                command: None,
                expression: None,
            },
        )
        .await
        .unwrap();
        assert!(out.stub);
        assert!(!out.ok);
        assert!(out.summary.contains("stub"));
    }

    #[tokio::test]
    async fn all_ops_return_stub() {
        for op in [
            DapDebugOp::Launch,
            DapDebugOp::Attach,
            DapDebugOp::Control,
            DapDebugOp::Breakpoints,
            DapDebugOp::Evaluate,
        ] {
            let out = xai_tool_runtime::Tool::run(
                &DapDebugTool,
                test_ctx(Resources::new().into_shared()),
                DapDebugInput {
                    op,
                    program: Some("/bin/true".into()),
                    adapter: Some("lldb-dap".into()),
                    command: Some("continue".into()),
                    expression: Some("1+1".into()),
                },
            )
            .await
            .unwrap();
            assert!(out.stub, "{op:?}");
            assert!(!out.ok, "{op:?}");
            assert!(out.summary.contains("not implemented"), "{:?}", out.summary);
        }
    }
}
