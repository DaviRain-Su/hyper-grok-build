//! Hypercore session / turn engine.
//!
//! Owns conversation state and turn orchestration. All I/O goes through
//! [`xai_hyper_host::HyperHost`].
//!
//! - Phase 0: [`mock::MockHost`] echo stream (always available).
//! - Phase 1: [`native::NativeHost`] (feature `native`, default) — disk
//!   snapshots under `~/.grok/hypercore/` + real model stream via sampler.
//! - Phase 3 types: multi-step tool loop via `list_tools` / `invoke_tool`.
//!
//! See `docs/design-hypercore.md`.

#![deny(missing_docs)]

pub mod disk_store;
pub mod mock;

#[cfg(feature = "native")]
pub mod native;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use xai_hyper_host::{
    ChatMessage, ChatToolCall, HostError, HostToolCall, HostToolResult, HyperHost, ModelChunk,
    ModelStreamRequest, SessionId, TerminalTurnRecord, ToolDefinition, TurnId,
};

/// Snapshot schema version (JSON).
///
/// - v1: plain role+content items
/// - v2: optional tool_calls / tool_call_id on items
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 2;

/// Default max model→tool→model steps inside one user turn.
pub const DEFAULT_MAX_TOOL_STEPS: u32 = 64;

/// Core-side errors.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Host failure.
    #[error("host: {0}")]
    Host(#[from] HostError),
    /// Snapshot decode / schema problem.
    #[error("snapshot: {0}")]
    Snapshot(String),
    /// Invalid client input.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Tool loop exceeded [`CoreConfig::max_tool_steps`].
    #[error("tool loop limit exceeded ({0})")]
    ToolLoopLimit(u32),
    /// Other.
    #[error("{0}")]
    Message(String),
}

/// Logical core config (no secrets).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct CoreConfig {
    /// Logical model id passed to the host.
    pub model: String,
    /// Max transcript messages kept (system + user/assistant/tool). Soft cap.
    pub max_messages: usize,
    /// Max model/tool iterations per user turn.
    #[serde(default = "default_max_tool_steps")]
    pub max_tool_steps: u32,
}

fn default_max_tool_steps() -> u32 {
    DEFAULT_MAX_TOOL_STEPS
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            model: "mock-echo".into(),
            max_messages: 256,
            max_tool_steps: DEFAULT_MAX_TOOL_STEPS,
        }
    }
}

/// Client turn request.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRequest {
    /// Idempotent turn id (client-generated).
    pub turn_id: TurnId,
    /// User text.
    pub text: String,
    /// Optional structured-output JSON Schema for this turn.
    pub json_schema: Option<serde_json::Value>,
    /// Tools for this turn. `None` → [`HyperHost::list_tools`]. `Some(vec![])` → no tools.
    pub tools: Option<Vec<ToolDefinition>>,
}

/// Result of a shell/host tool batch for one model step.
#[derive(Debug, Clone)]
pub enum ToolBatchResult {
    /// Apply tool results and sample the model again.
    Continue(Vec<HostToolResult>),
    /// Apply tool results and **end** the user turn (no further model call).
    ///
    /// Used for structured-output acceptance, permission cancel, etc.
    Finish(Vec<HostToolResult>),
}

/// One tool invocation recorded on a completed turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TurnToolCall {
    /// Call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments JSON string.
    pub arguments: String,
    /// Whether the host reported success.
    pub ok: bool,
    /// Result content.
    pub content: String,
}

/// Outcome of [`HyperCore::submit_turn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// Turn id.
    pub turn_id: TurnId,
    /// Full final assistant text for this turn (last assistant text segment).
    pub assistant_text: String,
    /// `true` if this was a terminal replay (no new model call).
    pub replayed: bool,
    /// Tools invoked during this turn (empty on plain / replay).
    pub tools_called: Vec<TurnToolCall>,
    /// Events emitted during the turn (order preserved).
    pub events: Vec<CoreEvent>,
}

/// Events visible to clients (no secrets).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    /// Status line.
    Status {
        /// Status text.
        text: String,
    },
    /// Turn accepted / started.
    TurnStarted {
        /// Turn id.
        turn_id: TurnId,
    },
    /// Streaming assistant text.
    AssistantDelta {
        /// Turn id.
        turn_id: TurnId,
        /// UTF-8 chunk.
        text: String,
    },
    /// Model requested a tool call.
    ToolCall {
        /// Turn id.
        turn_id: TurnId,
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments JSON string.
        arguments: String,
    },
    /// Host finished executing a tool.
    ToolResult {
        /// Turn id.
        turn_id: TurnId,
        /// Call id.
        call_id: String,
        /// Success flag.
        ok: bool,
        /// Result payload.
        content: String,
    },
    /// Turn finished and committed.
    TurnCommitted {
        /// Turn id.
        turn_id: TurnId,
        /// Optional stop reason.
        stop_reason: Option<String>,
        /// Whether this was an idempotent replay.
        replayed: bool,
    },
    /// Turn failed.
    TurnFailed {
        /// Turn id.
        turn_id: TurnId,
        /// Error message.
        error: String,
    },
}

/// Tool call stored on an assistant transcript row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct TranscriptToolCall {
    /// Call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments JSON string.
    pub arguments: String,
}

/// One transcript row in the session snapshot.
///
/// v1 snapshots only had `role` + `content`; v2 adds optional tool fields
/// (serde defaults keep v1 readable).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct TranscriptItem {
    /// Role: system / user / assistant / tool.
    pub role: String,
    /// Text content.
    pub content: String,
    /// Assistant tool calls (v2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<TranscriptToolCall>,
    /// Tool result → call id (v2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl TranscriptItem {
    /// Plain text row.
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// Durable session snapshot (JSON).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct SessionSnapshot {
    /// Schema version.
    pub schema_version: u32,
    /// Session id.
    pub session_id: SessionId,
    /// Transcript items.
    pub items: Vec<TranscriptItem>,
    /// Completed turn count.
    pub completed_turns: u64,
    /// Logical model id.
    pub model: String,
    /// Reserved for forward-compatible extensions.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

/// Hypercore engine bound to one session and one host.
pub struct HyperCore<H: HyperHost> {
    host: H,
    session_id: SessionId,
    config: CoreConfig,
    items: Vec<TranscriptItem>,
    completed_turns: u64,
}

impl<H: HyperHost> HyperCore<H> {
    /// Restore from host snapshot, or create a fresh session.
    pub async fn restore_or_new(
        host: H,
        session_id: impl Into<SessionId>,
        config: CoreConfig,
    ) -> Result<Self, CoreError> {
        let session_id = session_id.into();
        if session_id.trim().is_empty() {
            return Err(CoreError::Invalid("session_id must be non-empty".into()));
        }

        let (items, completed_turns, model) = match host.load_snapshot(&session_id).await? {
            Some(bytes) => {
                let snap = decode_snapshot(&bytes)?;
                if snap.session_id != session_id {
                    return Err(CoreError::Snapshot(format!(
                        "session_id mismatch: snap={} request={}",
                        snap.session_id, session_id
                    )));
                }
                let model = if snap.model.is_empty() {
                    config.model.clone()
                } else {
                    snap.model
                };
                (snap.items, snap.completed_turns, model)
            }
            None => (Vec::new(), 0, config.model.clone()),
        };

        Ok(Self {
            host,
            session_id,
            config: CoreConfig {
                model,
                max_messages: config.max_messages,
                max_tool_steps: config.max_tool_steps,
            },
            items,
            completed_turns,
        })
    }

    /// Session id.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Transcript items (read-only view).
    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    /// Number of completed turns.
    pub fn completed_turns(&self) -> u64 {
        self.completed_turns
    }

    /// Replace in-memory transcript (not persisted until the next successful commit).
    ///
    /// Used by the shell to seed context from `chat_state` before
    /// [`Self::submit_turn`] appends the current user message.
    pub fn seed_transcript(&mut self, items: Vec<TranscriptItem>, completed_turns: u64) {
        self.items = items;
        self.completed_turns = completed_turns;
        self.trim_messages();
    }

    /// Encode current state as snapshot bytes.
    pub fn export_snapshot(&self) -> Result<Vec<u8>, CoreError> {
        let snap = SessionSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            session_id: self.session_id.clone(),
            items: self.items.clone(),
            completed_turns: self.completed_turns,
            model: self.config.model.clone(),
            extensions: serde_json::Map::new(),
        };
        serde_json::to_vec(&snap).map_err(|e| CoreError::Snapshot(e.to_string()))
    }

    /// Submit a user turn using [`HyperHost::invoke_tool`] for tool execution.
    ///
    /// Requires `H: Clone` so the host can be invoked without conflicting with
    /// `&mut self` during the tool loop.
    pub async fn submit_turn(&mut self, req: TurnRequest) -> Result<TurnOutcome, CoreError>
    where
        H: Clone,
    {
        let host = self.host.clone();
        self.submit_turn_with_tools(req, |_assistant_text, calls| {
            let host = host.clone();
            async move {
                let mut out = Vec::with_capacity(calls.len());
                for call in calls {
                    let result = match host.invoke_tool(call.clone()).await {
                        Ok(r) => r,
                        Err(e) => HostToolResult {
                            call_id: call.id,
                            ok: false,
                            content: format!("tool error: {e}"),
                        },
                    };
                    out.push(result);
                }
                ToolBatchResult::Continue(out)
            }
        })
        .await
    }

    /// Submit a user turn with a **batch** tool invoker (shell path).
    ///
    /// `invoke_batch(assistant_text, calls)` receives the intermediate assistant
    /// text for this model step plus every tool call, and returns
    /// [`ToolBatchResult::Continue`] (sample again) or
    /// [`ToolBatchResult::Finish`] (end the user turn after applying results).
    ///
    /// When the model emits tool calls, runs model→tools→model until `end_turn`,
    /// `Finish`, or [`CoreConfig::max_tool_steps`].
    pub async fn submit_turn_with_tools<F, Fut>(
        &mut self,
        req: TurnRequest,
        invoke_batch: F,
    ) -> Result<TurnOutcome, CoreError>
    where
        F: FnMut(String, Vec<HostToolCall>) -> Fut,
        Fut: std::future::Future<Output = ToolBatchResult>,
    {
        let turn_id = req.turn_id.trim().to_string();
        let text = req.text.trim().to_string();
        if turn_id.is_empty() {
            return Err(CoreError::Invalid("turn_id must be non-empty".into()));
        }
        if text.is_empty() {
            return Err(CoreError::Invalid("text must be non-empty".into()));
        }

        // Idempotent replay: never open a second model stream.
        if let Some(term) = self
            .host
            .load_terminal_turn(&self.session_id, &turn_id)
            .await?
        {
            let events = vec![
                CoreEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                },
                CoreEvent::AssistantDelta {
                    turn_id: turn_id.clone(),
                    text: term.assistant_text.clone(),
                },
                CoreEvent::TurnCommitted {
                    turn_id: turn_id.clone(),
                    stop_reason: term.stop_reason.clone(),
                    replayed: true,
                },
            ];
            return Ok(TurnOutcome {
                turn_id,
                assistant_text: term.assistant_text,
                replayed: true,
                tools_called: Vec::new(),
                events,
            });
        }

        self.run_model_tool_loop(
            turn_id,
            text.clone(),
            req.tools,
            req.json_schema,
            Some(text),
            invoke_batch,
        )
        .await
    }

    /// Continue model→tool steps **without** appending a new user message.
    ///
    /// Used after mid-turn compaction: reseed from chat_state, then keep
    /// sampling until end_turn. `turn_id` must be unique (e.g. `{prompt}-c{n}`)
    /// so terminal idempotency does not collide with the pre-compact segment.
    pub async fn continue_turn_with_tools<F, Fut>(
        &mut self,
        turn_id: impl Into<String>,
        request_text: impl Into<String>,
        tools: Option<Vec<ToolDefinition>>,
        json_schema: Option<serde_json::Value>,
        invoke_batch: F,
    ) -> Result<TurnOutcome, CoreError>
    where
        F: FnMut(String, Vec<HostToolCall>) -> Fut,
        Fut: std::future::Future<Output = ToolBatchResult>,
    {
        let turn_id = turn_id.into().trim().to_string();
        let request_text = request_text.into();
        if turn_id.is_empty() {
            return Err(CoreError::Invalid("turn_id must be non-empty".into()));
        }
        self.run_model_tool_loop(
            turn_id,
            request_text,
            tools,
            json_schema,
            None,
            invoke_batch,
        )
        .await
    }

    /// Shared model→tool loop for [`Self::submit_turn_with_tools`] and
    /// [`Self::continue_turn_with_tools`].
    async fn run_model_tool_loop<F, Fut>(
        &mut self,
        turn_id: String,
        request_text: String,
        tools: Option<Vec<ToolDefinition>>,
        json_schema: Option<serde_json::Value>,
        append_user: Option<String>,
        mut invoke_batch: F,
    ) -> Result<TurnOutcome, CoreError>
    where
        F: FnMut(String, Vec<HostToolCall>) -> Fut,
        Fut: std::future::Future<Output = ToolBatchResult>,
    {
        let mut events = vec![
            CoreEvent::Status {
                text: if append_user.is_some() {
                    "turn started".into()
                } else {
                    "turn continued".into()
                },
            },
            CoreEvent::TurnStarted {
                turn_id: turn_id.clone(),
            },
        ];

        let rollback_len = self.items.len();
        if let Some(user_text) = append_user {
            self.items.push(TranscriptItem::text("user", user_text));
            self.trim_messages();
        }

        let tools = match tools {
            Some(t) => t,
            None => self.host.list_tools().await?,
        };
        let max_steps = self.config.max_tool_steps.max(1);
        let mut tools_called: Vec<TurnToolCall> = Vec::new();
        let mut final_assistant = String::new();
        let mut stop_reason: Option<String> = None;

        for step in 0..max_steps {
            let messages: Vec<ChatMessage> = self
                .items
                .iter()
                .map(transcript_to_chat_message)
                .collect();

            let stream_req = ModelStreamRequest {
                session_id: self.session_id.clone(),
                turn_id: turn_id.clone(),
                model: self.config.model.clone(),
                messages,
                tools: tools.clone(),
                json_schema: json_schema.clone(),
            };

            let mut stream = match self.host.open_model_stream(stream_req).await {
                Ok(s) => s,
                Err(e) => {
                    self.items.truncate(rollback_len);
                    let err = e.to_string();
                    events.push(CoreEvent::TurnFailed {
                        turn_id: turn_id.clone(),
                        error: err.clone(),
                    });
                    return Err(CoreError::Host(e));
                }
            };

            let mut assistant = String::new();
            let mut pending_calls: Vec<TranscriptToolCall> = Vec::new();
            let mut step_stop: Option<String> = None;

            loop {
                match stream.next_chunk().await {
                    Ok(Some(ModelChunk::TextDelta(delta))) => {
                        if !delta.is_empty() {
                            assistant.push_str(&delta);
                            events.push(CoreEvent::AssistantDelta {
                                turn_id: turn_id.clone(),
                                text: delta,
                            });
                        }
                    }
                    Ok(Some(ModelChunk::ToolCall {
                        id,
                        name,
                        arguments,
                    })) => {
                        events.push(CoreEvent::ToolCall {
                            turn_id: turn_id.clone(),
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        });
                        pending_calls.push(TranscriptToolCall {
                            id,
                            name,
                            arguments,
                        });
                    }
                    Ok(Some(ModelChunk::Done { stop_reason: sr })) => {
                        step_stop = sr;
                        break;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        self.items.truncate(rollback_len);
                        events.push(CoreEvent::TurnFailed {
                            turn_id: turn_id.clone(),
                            error: e.to_string(),
                        });
                        return Err(CoreError::Host(e));
                    }
                }
            }

            if pending_calls.is_empty() {
                self.items.push(TranscriptItem {
                    role: "assistant".into(),
                    content: assistant.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
                self.trim_messages();
                final_assistant = assistant;
                stop_reason = step_stop.or_else(|| Some("end_turn".into()));
                break;
            }

            self.items.push(TranscriptItem {
                role: "assistant".into(),
                content: assistant.clone(),
                tool_calls: pending_calls.clone(),
                tool_call_id: None,
            });
            self.trim_messages();
            if !assistant.is_empty() {
                final_assistant = assistant.clone();
            }

            let host_calls: Vec<HostToolCall> = pending_calls
                .iter()
                .map(|call| HostToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: parse_tool_args(&call.arguments),
                })
                .collect();
            let batch = invoke_batch(assistant, host_calls).await;
            let (results, finish_after) = match batch {
                ToolBatchResult::Continue(r) => (r, false),
                ToolBatchResult::Finish(r) => (r, true),
            };

            for (idx, call) in pending_calls.iter().enumerate() {
                let result = results.get(idx).cloned().unwrap_or_else(|| HostToolResult {
                    call_id: call.id.clone(),
                    ok: false,
                    content: "tool invoker returned no result for this call".into(),
                });
                let result = if result.call_id.is_empty() {
                    HostToolResult {
                        call_id: call.id.clone(),
                        ..result
                    }
                } else {
                    result
                };
                events.push(CoreEvent::ToolResult {
                    turn_id: turn_id.clone(),
                    call_id: result.call_id.clone(),
                    ok: result.ok,
                    content: result.content.clone(),
                });
                tools_called.push(TurnToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    ok: result.ok,
                    content: result.content.clone(),
                });
                self.items.push(TranscriptItem {
                    role: "tool".into(),
                    content: result.content,
                    tool_calls: Vec::new(),
                    tool_call_id: Some(call.id.clone()),
                });
                self.trim_messages();
            }

            if finish_after {
                stop_reason = step_stop.or_else(|| Some("end_turn".into()));
                break;
            }

            if step + 1 >= max_steps {
                self.items.truncate(rollback_len);
                events.push(CoreEvent::TurnFailed {
                    turn_id: turn_id.clone(),
                    error: format!("tool loop limit exceeded ({max_steps})"),
                });
                return Err(CoreError::ToolLoopLimit(max_steps));
            }
        }

        self.completed_turns = self.completed_turns.saturating_add(1);

        let terminal = TerminalTurnRecord {
            turn_id: turn_id.clone(),
            request_text,
            assistant_text: final_assistant.clone(),
            stop_reason: stop_reason.clone(),
        };
        let snapshot = self.export_snapshot()?;
        self.host
            .commit_snapshot(&self.session_id, &snapshot, Some(&terminal))
            .await?;

        events.push(CoreEvent::TurnCommitted {
            turn_id: turn_id.clone(),
            stop_reason,
            replayed: false,
        });

        Ok(TurnOutcome {
            turn_id,
            assistant_text: final_assistant,
            replayed: false,
            tools_called,
            events,
        })
    }

    fn trim_messages(&mut self) {
        let max = self.config.max_messages.max(2);
        while self.items.len() > max {
            // Prefer dropping oldest non-system.
            if let Some(i) = self.items.iter().position(|m| m.role != "system") {
                self.items.remove(i);
            } else {
                self.items.remove(0);
            }
        }
    }
}

fn transcript_to_chat_message(i: &TranscriptItem) -> ChatMessage {
    ChatMessage {
        role: i.role.clone(),
        content: i.content.clone(),
        tool_calls: i
            .tool_calls
            .iter()
            .map(|t| ChatToolCall {
                id: t.id.clone(),
                name: t.name.clone(),
                arguments: t.arguments.clone(),
            })
            .collect(),
        tool_call_id: i.tool_call_id.clone(),
    }
}

fn parse_tool_args(arguments: &str) -> serde_json::Value {
    let t = arguments.trim();
    if t.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(t).unwrap_or_else(|_| serde_json::json!({ "raw": arguments }))
}

fn decode_snapshot(bytes: &[u8]) -> Result<SessionSnapshot, CoreError> {
    let snap: SessionSnapshot = serde_json::from_slice(bytes)
        .map_err(|e| CoreError::Snapshot(format!("json: {e}")))?;
    if snap.schema_version == 0 || snap.schema_version > SNAPSHOT_SCHEMA_VERSION {
        return Err(CoreError::Snapshot(format!(
            "unsupported schema_version {}",
            snap.schema_version
        )));
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockHost;

    #[tokio::test]
    async fn restore_submit_commit_and_idempotent_turn() {
        let host = MockHost::new();
        let session = "sess-phase0-1";

        let mut core = HyperCore::restore_or_new(
            host.clone(),
            session,
            CoreConfig {
                model: "mock-echo".into(),
                max_messages: 32,
                max_tool_steps: 8,
            },
        )
        .await
        .expect("new session");

        assert_eq!(core.completed_turns(), 0);
        assert!(core.items().is_empty());

        let turn_id = "turn-1".to_string();
        let out = core
            .submit_turn(TurnRequest {
                turn_id: turn_id.clone(),
                text: "hello core".into(),
                json_schema: None,
                tools: None,
            })
            .await
            .expect("first turn");

        assert!(!out.replayed);
        assert!(out.assistant_text.contains("hello core"));
        assert_eq!(host.model_stream_opens(), 1);
        assert_eq!(core.completed_turns(), 1);
        assert_eq!(core.items().len(), 2);
        assert!(matches!(
            out.events.last(),
            Some(CoreEvent::TurnCommitted { replayed: false, .. })
        ));

        // Fresh core from same host storage.
        let mut core2 =
            HyperCore::restore_or_new(host.clone(), session, CoreConfig::default())
                .await
                .expect("restore");
        assert_eq!(core2.completed_turns(), 1);
        assert_eq!(core2.items().len(), 2);

        // Same turn_id: replay, no new model open.
        let out2 = core2
            .submit_turn(TurnRequest {
                turn_id: turn_id.clone(),
                text: "hello core".into(),
                json_schema: None,
                tools: None,
            })
            .await
            .expect("replay");
        assert!(out2.replayed);
        assert_eq!(out2.assistant_text, out.assistant_text);
        assert_eq!(host.model_stream_opens(), 1);
        assert!(matches!(
            out2.events.last(),
            Some(CoreEvent::TurnCommitted { replayed: true, .. })
        ));
    }

    #[tokio::test]
    async fn second_turn_increments_and_opens_again() {
        let host = MockHost::new();
        let mut core = HyperCore::restore_or_new(host.clone(), "s2", CoreConfig::default())
            .await
            .unwrap();

        core.submit_turn(TurnRequest {
            turn_id: "t1".into(),
            text: "one".into(),
            json_schema: None,
            tools: None,
        })
        .await
        .unwrap();
        core.submit_turn(TurnRequest {
            turn_id: "t2".into(),
            text: "two".into(),
            json_schema: None,
            tools: None,
        })
        .await
        .unwrap();

        assert_eq!(core.completed_turns(), 2);
        assert_eq!(host.model_stream_opens(), 2);
        assert_eq!(core.items().len(), 4);
    }

    #[tokio::test]
    async fn empty_turn_id_rejected() {
        let host = MockHost::new();
        let mut core = HyperCore::restore_or_new(host.clone(), "s3", CoreConfig::default())
            .await
            .unwrap();
        let err = core
            .submit_turn(TurnRequest {
                turn_id: "  ".into(),
                text: "x".into(),
                json_schema: None,
                tools: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Invalid(_)));
        assert_eq!(host.model_stream_opens(), 0);
    }

    #[tokio::test]
    async fn snapshot_round_trip_bytes() {
        let host = MockHost::new();
        let mut core = HyperCore::restore_or_new(host, "s4", CoreConfig::default())
            .await
            .unwrap();
        core.submit_turn(TurnRequest {
            turn_id: "t".into(),
            text: "ping".into(),
            json_schema: None,
            tools: None,
        })
        .await
        .unwrap();
        let bytes = core.export_snapshot().unwrap();
        let snap: SessionSnapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snap.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(snap.session_id, "s4");
        assert_eq!(snap.completed_turns, 1);
    }

    #[tokio::test]
    async fn snapshot_v1_still_loads() {
        let host = MockHost::new();
        let v1 = br#"{"schema_version":1,"session_id":"s-v1","items":[{"role":"user","content":"hi"},{"role":"assistant","content":"yo"}],"completed_turns":1,"model":"m","extensions":{}}"#;
        host.commit_snapshot("s-v1", v1, None).await.unwrap();
        let core = HyperCore::restore_or_new(host, "s-v1", CoreConfig::default())
            .await
            .unwrap();
        assert_eq!(core.completed_turns(), 1);
        assert_eq!(core.items().len(), 2);
        assert!(core.items()[0].tool_calls.is_empty());
    }

    #[tokio::test]
    async fn tool_loop_with_mock_echo_tool() {
        let host = MockHost::with_echo_tool();
        let mut core = HyperCore::restore_or_new(host.clone(), "tool-sess", CoreConfig::default())
            .await
            .unwrap();

        let out = core
            .submit_turn(TurnRequest {
                turn_id: "t-tool".into(),
                text: "please use the tool".into(),
                json_schema: None,
                tools: None,
            })
            .await
            .expect("tool turn");

        assert!(!out.replayed);
        assert_eq!(out.tools_called.len(), 1);
        assert_eq!(out.tools_called[0].name, "echo");
        assert!(out.tools_called[0].ok);
        assert!(out.assistant_text.contains("tool done"));
        // user + assistant(tool_calls) + tool result + final assistant
        assert!(core.items().len() >= 4);
        // Two model opens: first requests tool, second final text
        assert_eq!(host.model_stream_opens(), 2);
        assert!(out.events.iter().any(|e| matches!(e, CoreEvent::ToolCall { .. })));
        assert!(out.events.iter().any(|e| matches!(e, CoreEvent::ToolResult { .. })));
    }
}
