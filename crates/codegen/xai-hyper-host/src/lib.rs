//! Hypercore **host** surface (Phase 0).
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
pub const HYPER_HOST_API: u32 = 1;

/// Stable session identifier (opaque string; UUID recommended).
pub type SessionId = String;

/// Client-supplied turn id for idempotency.
pub type TurnId = String;

/// Host-side errors. Core maps these into its own error type.
#[derive(Debug, Error)]
pub enum HostError {
    /// Capability not available on this host (e.g. tools in Phase 0).
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

/// One message in the simplified Phase 0 chat view (not full sampling-types).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ChatMessage {
    /// Role: `"system"`, `"user"`, or `"assistant"`.
    pub role: String,
    /// Plain text body.
    pub content: String,
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
    /// Conversation so far, including the new user turn.
    pub messages: Vec<ChatMessage>,
}

/// Chunks from a model stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelChunk {
    /// UTF-8 assistant text delta.
    TextDelta(String),
    /// Stream finished successfully.
    Done {
        /// Optional stop reason (`end_turn`, `length`, …).
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
    /// Full assistant text for this turn.
    pub assistant_text: String,
    /// Optional stop reason.
    pub stop_reason: Option<String>,
}

/// Host-side tool invocation (Phase 0: unused; default returns unsupported).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct HostToolCall {
    /// Tool name.
    pub name: String,
    /// JSON arguments.
    pub arguments: serde_json::Value,
}

/// Host tool result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct HostToolResult {
    /// Whether the tool succeeded.
    pub ok: bool,
    /// Payload (text or JSON).
    pub content: String,
}

/// Platform host: secrets, transport, persistence.
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

    /// Optional host tools. Phase 0 default: unsupported.
    async fn invoke_tool(&self, _call: HostToolCall) -> Result<HostToolResult, HostError> {
        Err(HostError::Unsupported("tools"))
    }

    /// Wall clock for timestamps and tests.
    fn now_unix_ms(&self) -> u64;
}
