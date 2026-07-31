//! In-memory [`HyperHost`] for Phase 0 tests and demos.
//!
//! Model stream echoes `echo: {last_user_text}` in two text deltas then `Done`.
//! [`MockHost`] is cheap to clone (shared interior).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use xai_hyper_host::{
    HostError, HostToolCall, HostToolResult, HyperHost, ModelChunk, ModelStream, ModelStreamRequest,
    TerminalTurnRecord,
};

#[derive(Debug, Default)]
struct MockHostInner {
    snapshots: Mutex<HashMap<String, Vec<u8>>>,
    terminals: Mutex<HashMap<(String, String), TerminalTurnRecord>>,
    stream_opens: AtomicU64,
    clock_ms: Mutex<Option<u64>>,
}

/// Thread-safe mock host: memory snapshots + terminal turns + open counter.
#[derive(Debug, Clone, Default)]
pub struct MockHost {
    inner: Arc<MockHostInner>,
}

impl MockHost {
    /// Create an empty mock host.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MockHostInner::default()),
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

    async fn invoke_tool(&self, _call: HostToolCall) -> Result<HostToolResult, HostError> {
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
