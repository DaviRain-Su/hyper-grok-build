//! ACP `SessionUpdate` → comet `AgentEvent` mapping.
//!
//! Reference: `xai-hyper-desktop/src/services/acp_backend.rs::session_notification`
//! already maps every ACP variant to a UI event; this is the same mapping
//! retargeted at comet's `AgentEvent`. `Done`/`AssistantMessageCompleted` are
//! NOT produced here — they come from the `session/prompt` await's `stop_reason`
//! (a separate path in `mod.rs`).

use agent_client_protocol as acp;
use comet_proto::agent::{AgentEvent, DoneStatus, HarnessId, ToolCall as CometToolCall};

/// Map one ACP `SessionUpdate` into zero or more `AgentEvent`s.
///
/// `assistant_message_id` is the id minted at `session/new`; `AssistantMessageCompleted`
/// is emitted from the prompt-resolution path, not here.
pub fn map_update(update: acp::SessionUpdate, _session_id: &acp::SessionId) -> Vec<AgentEvent> {
    match update {
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk { content, .. }) => {
            if let acp::ContentBlock::Text(t) = content {
                if !t.text.is_empty() {
                    return vec![AgentEvent::TextDelta { text: t.text }];
                }
            }
            Vec::new()
        }
        acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk { content, .. }) => {
            if let acp::ContentBlock::Text(t) = content {
                if !t.text.is_empty() {
                    return vec![AgentEvent::ReasoningDelta { text: t.text }];
                }
            }
            Vec::new()
        }
        // User echo is written by the engine; ignore the live echo.
        acp::SessionUpdate::UserMessageChunk(_) => Vec::new(),

        acp::SessionUpdate::ToolCall(tc) => tool_call_events(
            &tc.tool_call_id.0,
            tc.status,
            tc.title.as_str(),
            tc.raw_input.as_ref(),
        ),

        acp::SessionUpdate::ToolCallUpdate(upd) => tool_call_events(
            &upd.tool_call_id.0,
            upd.fields.status.unwrap_or(acp::ToolCallStatus::InProgress),
            upd.fields.title.as_deref().unwrap_or(""),
            upd.fields.raw_input.as_ref(),
        ),
        // Usage/other ACP updates have no direct comet equivalent here; usage is
        // not surfaced (comet parity exclusion: token-usage display).
        _ => Vec::new(),
    }
}

/// Emit a `ToolCall` (always) and, on a terminal status, a `ToolResult`.
fn tool_call_events(
    id: &str,
    status: acp::ToolCallStatus,
    title: &str,
    raw_input: Option<&serde_json::Value>,
) -> Vec<AgentEvent> {
    let call = map_tool_call(title, raw_input);
    let mut out = vec![AgentEvent::ToolCall {
        id: id.to_string(),
        call,
    }];
    match status {
        acp::ToolCallStatus::Completed => out.push(AgentEvent::ToolResult {
            id: id.to_string(),
            is_error: false,
        }),
        acp::ToolCallStatus::Failed => out.push(AgentEvent::ToolResult {
            id: id.to_string(),
            is_error: true,
        }),
        _ => {}
    }
    out
}

/// Map an ACP tool call (title + raw JSON input) to a comet `ToolCall`.
///
/// ACP gives a display `title` and a raw JSON `input`, not a stable tool name,
/// so v1 maps everything to `Unknown { name: title, input }` — the comet UI
/// renders `Unknown` tool calls fine. A richer name-based mapping (like codex's
/// `decode_tool_use`) can come later if hyper exposes stable tool names.
fn map_tool_call(title: &str, raw_input: Option<&serde_json::Value>) -> CometToolCall {
    CometToolCall::Unknown {
        name: title.to_string(),
        input: raw_input.cloned(),
    }
}

/// Translate the ACP `session/prompt` `stop_reason` + the harness `assistant_message_id`
/// into the terminal `AgentEvent`s: `AssistantMessageCompleted` then `Done`.
pub fn done_from_stop(
    stop_reason: &acp::PromptResponse,
    assistant_message_id: &str,
    session_id: &str,
) -> Vec<AgentEvent> {
    let mut out = vec![AgentEvent::AssistantMessageCompleted {
        assistant_message_id: assistant_message_id.to_string(),
    }];
    let status = match stop_reason.stop_reason {
        acp::StopReason::EndTurn => DoneStatus::Completed,
        acp::StopReason::Cancelled => DoneStatus::Interrupted,
        _ => DoneStatus::Errored,
    };
    out.push(AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some(session_id.to_string()),
    });
    // UnusedHarnessId is a compile hint that HarnessId is in scope for callers
    // that build SessionStarted elsewhere; kept to avoid an unused import here.
    let _ = HarnessId::Hyper;
    out
}