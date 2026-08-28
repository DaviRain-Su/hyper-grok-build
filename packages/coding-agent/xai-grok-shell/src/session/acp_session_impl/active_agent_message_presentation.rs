//! Content-free presentation for active-agent message tool calls.

use agent_client_protocol as acp;
use xai_grok_tools::implementations::grok_build::send_subagent_message::SendSubagentMessageInput;

pub(super) fn active_agent_message_tool_call_display(
    _message: &SendSubagentMessageInput,
) -> (String, acp::ToolKind) {
    (
        "Sending message to subagent".to_owned(),
        acp::ToolKind::Other,
    )
}

#[cfg(test)]
#[path = "active_agent_message_presentation_tests.rs"]
mod tests;
