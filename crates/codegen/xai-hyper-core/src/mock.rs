//! In-memory [`HyperHost`] for Phase 0/3 tests and demos.
//!
//! Default: model stream echoes `echo: {last_user_text}` in two text deltas then `Done`.
//! With [`MockHost::with_echo_tool`]: first open emits a tool call; after a tool
//! result is present, second open returns final text.
//!
//! [`MockHost`] is cheap to clone (shared interior).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use xai_hyper_host::{
    HostError, HostToolCall, HostToolResult, HyperHost, ModelChunk, ModelStream, ModelStreamRequest,
    TerminalTurnRecord, ToolDefinition,
};

#[derive(Debug, Default)]
struct MockHostInner {
    snapshots: Mutex<HashMap<String, Vec<u8>>>,
    terminals: Mutex<HashMap<(String, String), TerminalTurnRecord>>,
    stream_opens: AtomicU64,
    clock_ms: Mutex<Option<u64>>,
    /// When true, advertise `echo` tool and drive a one-step tool loop.
    echo_tool: bool,
}

/// Thread-safe mock host: memory snapshots + terminal turns + open counter.
#[derive(Debug, Clone, Default)]
pub struct MockHost {
    inner: Arc<MockHostInner>,
}

impl MockHost {
    /// Create an empty mock host (plain echo, no tools).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MockHostInner::default()),
        }
    }

    /// Mock host that lists an `echo` tool and exercises the tool loop.
    pub fn with_echo_tool() -> Self {
        Self {
            inner: Arc::new(MockHostInner {
                echo_tool: true,
                ..MockHostInner::default()
            }),
        }
    }

    /// How many times [`HyperHost::open_model_stream`] was called.
    pub fn model_stream_opens(&self) -> u64 {
        self.inner.stream_opens.load(Ordering::SeqCst)
    }

    /// Override wall clock for deterministic tests.
    pub fn set_now_unix_ms(&self, ms: u64) {
        *self.inner.clock_ms.lock().expect("clock") = Some(ms);
    }
}

struct EchoStream {
    chunks: std::vec::IntoIter<ModelChunk>,
}

#[async_trait]
impl ModelStream for EchoStream {
    async fn next_chunk(&mut self) -> Result<Option<ModelChunk>, HostError> {
        Ok(self.chunks.next())
    }
}

#[async_trait]
impl HyperHost for MockHost {
    async fn open_model_stream(
        &self,
        req: ModelStreamRequest,
    ) -> Result<Box<dyn ModelStream>, HostError> {
        self.inner.stream_opens.fetch_add(1, Ordering::SeqCst);

        if self.inner.echo_tool {
            let has_tool_result = req.messages.iter().any(|m| m.role == "tool");
            if !has_tool_result {
                // First step: request the echo tool with last user text as arg.
                let user = req
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.as_str())
                    .unwrap_or("");
                let args = serde_json::json!({ "text": user }).to_string();
                let chunks = vec![
                    ModelChunk::ToolCall {
                        id: "call_echo_1".into(),
                        name: "echo".into(),
                        arguments: args,
                    },
                    ModelChunk::Done {
                        stop_reason: Some("tool_calls".into()),
                    },
                ];
                return Ok(Box::new(EchoStream {
                    chunks: chunks.into_iter(),
                }));
            }
            // After tool result: final assistant text.
            let tool_content = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "tool")
                .map(|m| m.content.as_str())
                .unwrap_or("");
            let full = format!("tool done: {tool_content}");
            let chunks = vec![
                ModelChunk::TextDelta(full),
                ModelChunk::Done {
                    stop_reason: Some("end_turn".into()),
                },
            ];
            return Ok(Box::new(EchoStream {
                chunks: chunks.into_iter(),
            }));
        }

        let user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let full = format!("echo: {user}");
        // Two deltas so clients can practice streaming.
        let mid = full.len().div_ceil(2).min(full.len());
        let (a, b) = full.split_at(mid);
        let chunks = vec![
            ModelChunk::TextDelta(a.to_string()),
            ModelChunk::TextDelta(b.to_string()),
            ModelChunk::Done {
                stop_reason: Some("end_turn".into()),
            },
        ];
        Ok(Box::new(EchoStream {
            chunks: chunks.into_iter(),
        }))
    }

    async fn commit_snapshot(
        &self,
        session_id: &str,
        snapshot: &[u8],
        terminal: Option<&TerminalTurnRecord>,
    ) -> Result<(), HostError> {
        self.inner
            .snapshots
            .lock()
            .expect("snapshots")
            .insert(session_id.to_string(), snapshot.to_vec());
        if let Some(t) = terminal {
            self.inner.terminals.lock().expect("terminals").insert(
                (session_id.to_string(), t.turn_id.clone()),
                t.clone(),
            );
        }
        Ok(())
    }

    async fn load_snapshot(&self, session_id: &str) -> Result<Option<Vec<u8>>, HostError> {
        Ok(self
            .inner
            .snapshots
            .lock()
            .expect("snapshots")
            .get(session_id)
            .cloned())
    }

    async fn load_terminal_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<TerminalTurnRecord>, HostError> {
        Ok(self
            .inner
            .terminals
            .lock()
            .expect("terminals")
            .get(&(session_id.to_string(), turn_id.to_string()))
            .cloned())
    }

    async fn list_tools(&self) -> Result<Vec<ToolDefinition>, HostError> {
        if self.inner.echo_tool {
            Ok(vec![ToolDefinition {
                name: "echo".into(),
                description: "Echo back the text argument".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
                    },
                    "required": ["text"]
                }),
            }])
        } else {
            Ok(Vec::new())
        }
    }

    async fn invoke_tool(&self, call: HostToolCall) -> Result<HostToolResult, HostError> {
        if call.name == "echo" {
            let text = call
                .arguments
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Ok(HostToolResult {
                call_id: call.id,
                ok: true,
                content: format!("echoed: {text}"),
            });
        }
        Err(HostError::Unsupported("tools"))
    }

    fn now_unix_ms(&self) -> u64 {
        if let Some(ms) = *self.inner.clock_ms.lock().expect("clock") {
            return ms;
        }
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}
