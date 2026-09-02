//! `WorkspaceRequest` is the outer envelope (matched on at the transport-layer dispatch).
//! The three inner enums (`ToolRequest`, `WorkspaceOpsRequest`, `SessionLifecycleRequest`) are the actual per-domain RPC payloads.

pub mod ops;
pub mod session;
pub mod tool;

use serde::{Deserialize, Serialize};

pub use ops::WorkspaceOpsRequest;
pub use session::SessionLifecycleRequest;
pub use tool::{ToolCallArgs, ToolRequest};

/// Outer-envelope wire request.
///
/// Each variant maps to one of the streaming gRPC RPCs (`Tool`, `Ops`, `Session`).
/// The fourth RPC, `Events`, is a separate subscription type and does not appear here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkspaceRequest {
    Tool(ToolRequest),
    Ops(WorkspaceOpsRequest),
    Session(SessionLifecycleRequest),
}
