//! Hypercore **host** surface.
//!
//! Implementations own secrets, outbound connections, and durable storage.
//! [`crate::HyperHost`] is the only I/O boundary the core engine may use.
//!
//! See `docs/design-hypercore.md`.

#![deny(missing_docs)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Host API major version. Bump on breaking trait / type changes.
///
/// v2: tool definitions on model requests, tool-call chunks, richer chat
/// messages, `list_tools`, tool call ids on invoke/result.
pub const HYPER_HOST_API: u32 = 2;

/// Stable session identifier (opaque string; UUID recommended).
pub type SessionId = String;

/// Client-supplied turn id for idempotency.
pub type TurnId = String;

/// Host-side errors. Core maps these into its own error type.
#[derive(Debug, Error)]
pub enum HostError {
    /// Capability not available on this host (e.g. tools in early phases).
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// I/O or storage failure.
    #[error("io: {0}")]
    Io(String),
    /// Model / transport failure.
    #[error("transport: {0}")]
    Transport(String),
    /// Other failure.
    #[error("{0}")]
    Message(String),
}

/// One tool the model may call (schema only; execution is host-side).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ToolDefinition {
    /// Tool name (unique in the request).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for arguments.
    pub input_schema: serde_json::Value,
}

/// Tool call attached to an assistant message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ChatToolCall {
    /// Model-assigned call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as a JSON string (or object serialized to string).
    pub arguments: String,
}

/// One message in the simplified chat view.
///
/// Roles: `"system"`, `"user"`, `"assistant"`, `"tool"`.
/// Assistant rows may carry [`Self::tool_calls`]; tool rows set
/// [`Self::tool_call_id`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ChatMessage {
    /// Role: `"system"`, `"user"`, `"assistant"`, or `"tool"`.
    pub role: String,
    /// Plain text body (tool result payload for `"tool"`).
    pub content: String,
    /// Assistant tool calls (empty unless role is assistant).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ChatToolCall>,
    /// For role `"tool"`: which call this result answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// System / user / plain assistant text message.
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Assistant message that requests tool calls (optional text).
    pub fn assistant_tools(content: impl Into<String>, tool_calls: Vec<ChatToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Tool result message.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// Request for the host to open (or resume) a model stream.
///
/// Credentials must **not** appear here; the host attaches them itself.
#[derive(Debug, Clone)]
pub struct ModelStreamRequest {
    /// Session this turn belongs to.
    pub session_id: SessionId,
    /// Idempotent turn id from the client.
    pub turn_id: TurnId,
    /// Logical model id (host may map to a real upstream id).
    pub model: String,
    /// Conversation so far, including the new user turn / tool results.
    pub messages: Vec<ChatMessage>,
    /// Tools available for this stream (may be empty).
    pub tools: Vec<ToolDefinition>,
    /// Optional structured-output JSON Schema for the model response.
    pub json_schema: Option<serde_json::Value>,
}

/// Chunks from a model stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelChunk {
    /// UTF-8 assistant text delta.
    TextDelta(String),
    /// A complete tool call from the model (aggregated; not partial args).
    ToolCall {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments JSON string.
        arguments: String,
    },
    /// Stream finished successfully.
    Done {
        /// Optional stop reason (`end_turn`, `tool_calls`, `length`, …).
        stop_reason: Option<String>,
    },
}

/// Async stream of [`ModelChunk`] owned by the host.
#[async_trait]
pub trait ModelStream: Send {
    /// Next chunk, or `Ok(None)` if the stream ended without an explicit [`ModelChunk::Done`].
    async fn next_chunk(&mut self) -> Result<Option<ModelChunk>, HostError>;
}

/// Durable record of a completed turn (for idempotent replay).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct TerminalTurnRecord {
    /// Client turn id.
    pub turn_id: TurnId,
    /// Original user text.
    pub request_text: String,
    /// Full assistant text for this turn (final assistant text, not intermediate).
    pub assistant_text: String,
    /// Optional stop reason.
    pub stop_reason: Option<String>,
}

/// Host-side tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct HostToolCall {
    /// Model-assigned call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// JSON arguments.
    pub arguments: serde_json::Value,
}

/// Host tool result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct HostToolResult {
    /// Matching [`HostToolCall::id`].
    pub call_id: String,
    /// Whether the tool succeeded.
    pub ok: bool,
    /// Payload (text or JSON string).
    pub content: String,
}

/// Platform host: secrets, transport, persistence, tools.
#[async_trait]
pub trait HyperHost: Send + Sync {
    /// Open a model stream for this turn. Host attaches credentials.
    async fn open_model_stream(
        &self,
        req: ModelStreamRequest,
    ) -> Result<Box<dyn ModelStream>, HostError>;

    /// Atomically persist session snapshot bytes and optional terminal turn.
    async fn commit_snapshot(
        &self,
        session_id: &str,
        snapshot: &[u8],
        terminal: Option<&TerminalTurnRecord>,
    ) -> Result<(), HostError>;

    /// Load the latest snapshot for cold restore, if any.
    async fn load_snapshot(&self, session_id: &str) -> Result<Option<Vec<u8>>, HostError>;

    /// Load a previously committed terminal turn (idempotent replay).
    async fn load_terminal_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<TerminalTurnRecord>, HostError>;

    /// Tools available for the next model stream. Default: none.
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>, HostError> {
        Ok(Vec::new())
    }

    /// Optional host tools. Default: unsupported.
    async fn invoke_tool(&self, _call: HostToolCall) -> Result<HostToolResult, HostError> {
        Err(HostError::Unsupported("tools"))
    }

    /// Wall clock for timestamps and tests.
    fn now_unix_ms(&self) -> u64;
}
