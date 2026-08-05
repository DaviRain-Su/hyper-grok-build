//! `agent_hub` — peer messaging among Main and live subagents (OMP-inspired).
//!
//! Distinct from the workspace remote hub (`xai_grok_workspace::hub`).

mod bus;

pub use bus::{
    AgentBus, AgentHubMessage, MAIN_PEER_ID, MAX_MAILBOX_DEPTH, MAX_MESSAGE_BYTES, PeerInfo,
    PeerStatus, PeerWakeFn, SendOutcome,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::register_resource;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

/// Resource: session-scoped peer bus (shared by Main and children).
#[derive(Clone)]
pub struct AgentBusResource(pub AgentBus);

impl AgentBusResource {
    pub fn bus(&self) -> &AgentBus {
        &self.0
    }
}

impl std::fmt::Debug for AgentBusResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentBusResource").finish()
    }
}

register_resource!("grok_build", "AgentBusResource", AgentBusResource);

/// Resource: this session's roster id on the bus (`Main` or subagent UUID).
#[derive(Clone, Debug)]
pub struct AgentSelfIdResource(pub String);

register_resource!("grok_build", "AgentSelfIdResource", AgentSelfIdResource);

/// Canonical tool id.
pub const AGENT_HUB_TOOL_NAME: &str = "agent_hub";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentHubOp {
    #[default]
    List,
    Send,
    Inbox,
    Wait,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentHubInput {
    #[schemars(
        description = "Operation: list peers, send a message, drain inbox, or wait for mail"
    )]
    pub op: AgentHubOp,
    #[serde(default)]
    #[schemars(description = "Target peer id for send (from list). Also filters wait.")]
    pub to: Option<String>,
    #[serde(default)]
    #[schemars(description = "Message body for send (plain prose, max 8KiB)")]
    pub text: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional message id this send is replying to")]
    pub reply_to: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Wait timeout in ms (wait only). Capped at 30000. Default 5000."
    )]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHubPeerRow {
    pub id: String,
    pub label: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHubMessageRow {
    pub id: String,
    pub from: String,
    pub to: String,
    pub text: String,
    pub reply_to: Option<String>,
    pub ts_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHubOutput {
    pub ok: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers: Option<Vec<AgentHubPeerRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<AgentHubMessageRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

impl AgentHubOutput {
    fn fail(summary: impl Into<String>) -> Self {
        Self {
            ok: false,
            summary: summary.into(),
            peers: None,
            messages: None,
            message_id: None,
        }
    }
}

fn msg_rows(msgs: Vec<AgentHubMessage>) -> Vec<AgentHubMessageRow> {
    msgs.into_iter()
        .map(|m| AgentHubMessageRow {
            id: m.id,
            from: m.from,
            to: m.to,
            text: m.text,
            reply_to: m.reply_to,
            ts_unix_ms: m.ts_unix_ms,
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct AgentHubTool;

impl crate::types::tool_metadata::ToolMetadata for AgentHubTool {
    fn kind(&self) -> ToolKind {
        // Allowed under ReadOnly capability (coordination, not workspace mutation).
        ToolKind::Plan
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Coordinate with live peer agents in this session (Main + running subagents).

Ops:
- list: roster of peer ids/labels/status
- send: fire-and-forget message to `to` (plain prose only)
- inbox: drain your mailbox (non-blocking)
- wait: block until mail arrives or timeout_ms (max 30000)

Rules:
- Use list before send; never invent peer ids
- Do not use agent_hub for questions tools can answer (grep, read, build)
- Subagents share this bus with Main; depth is still one (children cannot spawn children)
"#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }

    fn is_read_only(&self) -> bool {
        // Mailbox coordination only — safe for read-only subagents (scout/reviewer).
        true
    }
}

impl xai_tool_runtime::Tool for AgentHubTool {
    type Args = AgentHubInput;
    type Output = AgentHubOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(AGENT_HUB_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            AGENT_HUB_TOOL_NAME,
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

    #[tracing::instrument(name = "new_tool.agent_hub", skip_all, fields(op = ?input.op))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: AgentHubInput,
    ) -> Result<AgentHubOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;
        let (bus, self_id) = {
            let res = resources.lock().await;
            let bus = res
                .get::<AgentBusResource>()
                .cloned()
                .ok_or_else(|| {
                    xai_tool_runtime::ToolError::invalid_arguments(
                        "agent_hub unavailable: AgentBusResource not installed for this session",
                    )
                })?;
            let self_id = res
                .get::<AgentSelfIdResource>()
                .map(|s| s.0.clone())
                .unwrap_or_else(|| MAIN_PEER_ID.to_string());
            (bus, self_id)
        };

        match input.op {
            AgentHubOp::List => {
                let peers = bus.bus().list();
                let rows: Vec<_> = peers
                    .into_iter()
                    .map(|p| AgentHubPeerRow {
                        id: p.id,
                        label: p.label,
                        status: match p.status {
                            PeerStatus::Running => "running".into(),
                            PeerStatus::Gone => "gone".into(),
                        },
                    })
                    .collect();
                let summary = if rows.is_empty() {
                    "No peers registered on the agent bus.".into()
                } else {
                    format!(
                        "Peers ({}): {}",
                        rows.len(),
                        rows.iter()
                            .map(|r| format!("{} [{}] ({})", r.id, r.label, r.status))
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                };
                Ok(AgentHubOutput {
                    ok: true,
                    summary,
                    peers: Some(rows),
                    messages: None,
                    message_id: None,
                })
            }
            AgentHubOp::Send => {
                let to = input.to.as_deref().unwrap_or("").trim();
                if to.is_empty() {
                    return Ok(AgentHubOutput::fail("send requires `to` (peer id from list)"));
                }
                let text = input.text.as_deref().unwrap_or("").trim();
                if text.is_empty() {
                    return Ok(AgentHubOutput::fail("send requires non-empty `text`"));
                }
                match bus.bus().send(&self_id, to, text, input.reply_to.as_deref()) {
                    SendOutcome::Delivered { message_id } => Ok(AgentHubOutput {
                        ok: true,
                        summary: format!("delivered to {to} as {message_id}"),
                        peers: None,
                        messages: None,
                        message_id: Some(message_id),
                    }),
                    SendOutcome::Failed { reason } => Ok(AgentHubOutput::fail(reason)),
                }
            }
            AgentHubOp::Inbox => {
                let msgs = bus.bus().inbox(&self_id);
                let n = msgs.len();
                let summary = if n == 0 {
                    "inbox empty".into()
                } else {
                    format!("drained {n} message(s)")
                };
                Ok(AgentHubOutput {
                    ok: true,
                    summary,
                    peers: None,
                    messages: Some(msg_rows(msgs)),
                    message_id: None,
                })
            }
            AgentHubOp::Wait => {
                let timeout_ms = input.timeout_ms.unwrap_or(5_000).min(30_000);
                let msgs = bus
                    .bus()
                    .wait_inbox(&self_id, std::time::Duration::from_millis(timeout_ms))
                    .await;
                let n = msgs.len();
                let summary = if n == 0 {
                    format!("wait timed out after {timeout_ms}ms; inbox empty")
                } else {
                    format!("received {n} message(s)")
                };
                Ok(AgentHubOutput {
                    ok: true,
                    summary,
                    peers: None,
                    messages: Some(msg_rows(msgs)),
                    message_id: None,
                })
            }
        }
    }
}

impl AgentHubOutput {
    /// Flatten for prompt / `ToolOutput::Text` conversion.
    pub fn prompt_text(&self) -> String {
        let mut out = self.summary.clone();
        if let Some(peers) = &self.peers {
            for p in peers {
                out.push_str(&format!("\n- {} [{}] status={}", p.id, p.label, p.status));
            }
        }
        if let Some(msgs) = &self.messages {
            for m in msgs {
                out.push_str(&format!(
                    "\n--- from {} id={} ---\n{}",
                    m.from, m.id, m.text
                ));
            }
        }
        out
    }
}

impl xai_tool_runtime::ToolOutput for AgentHubOutput {
    fn model_output(&self) -> Vec<xai_tool_runtime::ContentBlock> {
        vec![xai_tool_runtime::ContentBlock::Text {
            text: self.prompt_text(),
        }]
    }
}

impl From<AgentHubOutput> for crate::types::output::ToolOutput {
    fn from(o: AgentHubOutput) -> Self {
        crate::types::output::ToolOutput::Text(o.prompt_text().into())
    }
}
