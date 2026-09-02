//! `send_subagent_message` — send an active message to an owned subagent.

use crate::implementations::grok_build::task::backend::SubagentBackendResource;
use crate::implementations::grok_build::task::types::{
    ActiveAgentMessageOperation, ActiveAgentMessageOutcome, ActiveAgentMessageRequest,
    SubagentDepthCounter,
};
use crate::types::tool::{ToolKind, ToolNamespace};

pub const SEND_SUBAGENT_MESSAGE_TOOL_NAME: &str = "send_subagent_message";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SendSubagentMessageInput {
    /// ID of the active subagent that should receive the message.
    pub subagent_id: String,
    /// Text to send to the subagent.
    pub text: String,
    /// Queue for a later turn instead of steering the active turn.
    #[serde(default)]
    #[schemars(default)]
    pub queue: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[non_exhaustive]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SendSubagentMessageOutput {
    Accepted {
        message_id: String,
    },
    NotFoundOrNotOwned,
    NotActiveOrFinalizing,
    Saturated {
        max_in_flight: usize,
    },
    AdmissionUncertain,
    NotAcceptedBeforeDeadline,
    Unsupported,
    Limit {
        max_bytes: usize,
        observed_bytes: usize,
    },
    ChannelClosed,
}

/// Delivery classification shared by tool hosts and presentations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendSubagentMessageDisposition {
    /// Admission was confirmed.
    Accepted,
    /// Admission was definitely rejected.
    Rejected,
    /// Admission or delivery could not be confirmed.
    Unconfirmed,
}

impl SendSubagentMessageOutput {
    /// Classify this output without collapsing uncertainty into failure.
    pub fn disposition(&self) -> SendSubagentMessageDisposition {
        match self {
            Self::Accepted { .. } => SendSubagentMessageDisposition::Accepted,
            Self::AdmissionUncertain => SendSubagentMessageDisposition::Unconfirmed,
            Self::NotFoundOrNotOwned
            | Self::NotActiveOrFinalizing
            | Self::Saturated { .. }
            | Self::NotAcceptedBeforeDeadline
            | Self::Unsupported
            | Self::Limit { .. }
            | Self::ChannelClosed => SendSubagentMessageDisposition::Rejected,
        }
    }
}

impl From<ActiveAgentMessageOutcome> for SendSubagentMessageOutput {
    fn from(outcome: ActiveAgentMessageOutcome) -> Self {
        match outcome {
            ActiveAgentMessageOutcome::Accepted { message_id } => Self::Accepted { message_id },
            ActiveAgentMessageOutcome::NotFoundOrNotOwned => Self::NotFoundOrNotOwned,
            ActiveAgentMessageOutcome::NotActiveOrFinalizing => Self::NotActiveOrFinalizing,
            ActiveAgentMessageOutcome::Saturated { max_in_flight } => {
                Self::Saturated { max_in_flight }
            }
            ActiveAgentMessageOutcome::AdmissionUncertain => Self::AdmissionUncertain,
            ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline => Self::NotAcceptedBeforeDeadline,
            ActiveAgentMessageOutcome::Unsupported => Self::Unsupported,
            ActiveAgentMessageOutcome::Limit {
                max_bytes,
                observed_bytes,
            } => Self::Limit {
                max_bytes,
                observed_bytes,
            },
            ActiveAgentMessageOutcome::ChannelClosed => Self::ChannelClosed,
        }
    }
}

impl std::fmt::Display for SendSubagentMessageOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Accepted { message_id } => {
                write!(f, "Message accepted (message_id: {message_id}).")
            }
            Self::NotFoundOrNotOwned => {
                f.write_str("Subagent not found or not owned by this session.")
            }
            Self::NotActiveOrFinalizing => f.write_str("Subagent is not active or is finalizing."),
            Self::Saturated { max_in_flight } => write!(
                f,
                "Message admission is saturated (maximum {max_in_flight} in flight)."
            ),
            Self::AdmissionUncertain => f.write_str(
                "Message admission could not be confirmed; the message may or may not have been accepted.",
            ),
            Self::NotAcceptedBeforeDeadline => {
                f.write_str("Message was not accepted before the delivery deadline.")
            }
            Self::Unsupported => {
                f.write_str("Active agent messages are unsupported in this context.")
            }
            Self::Limit {
                max_bytes,
                observed_bytes,
            } => write!(
                f,
                "Message size is invalid: observed {observed_bytes} bytes; maximum is {max_bytes} bytes."
            ),
            Self::ChannelClosed => {
                f.write_str("Message was not accepted because the subagent channel closed.")
            }
        }
    }
}

impl xai_tool_runtime::ToolOutput for SendSubagentMessageOutput {}

#[derive(Debug, Default)]
pub struct SendSubagentMessageTool;

impl crate::types::tool_metadata::ToolMetadata for SendSubagentMessageTool {
    fn kind(&self) -> ToolKind {
        ToolKind::ActiveAgentMessage
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        "Send a follow-up message to an active subagent owned by this session. By default it steers the current turn at its next safe point; set queue to true to wait for a later turn."
    }
}

impl xai_tool_runtime::Tool for SendSubagentMessageTool {
    type Args = SendSubagentMessageInput;
    type Output = SendSubagentMessageOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(SEND_SUBAGENT_MESSAGE_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            SEND_SUBAGENT_MESSAGE_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.send_subagent_message",
        skip_all,
        fields(subagent_id = %input.subagent_id)
    )]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: SendSubagentMessageInput,
    ) -> Result<SendSubagentMessageOutput, xai_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let (depth, backend) = {
            let res = resources.lock().await;
            (
                res.get::<SubagentDepthCounter>().map(|value| value.0),
                res.get::<SubagentBackendResource>().cloned(),
            )
        };

        let (Some(0), Some(backend)) = (depth, backend) else {
            return Ok(SendSubagentMessageOutput::Unsupported);
        };
        let operation = if input.queue {
            ActiveAgentMessageOperation::Queue
        } else {
            ActiveAgentMessageOperation::Steer
        };
        let request = match ActiveAgentMessageRequest::try_new_with_operation(
            input.subagent_id,
            input.text,
            operation,
        ) {
            Ok(request) => request,
            Err(outcome) => return Ok(outcome.into()),
        };

        Ok(backend.backend().send_active_message(request).await.into())
    }
}

#[cfg(test)]
#[path = "send_subagent_message_tests.rs"]
mod tests;
