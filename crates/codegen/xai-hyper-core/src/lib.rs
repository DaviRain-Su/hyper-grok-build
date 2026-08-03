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
    /// Host/shell aborted the turn without committing (permission cancel, compact
    /// restart, etc.). Transcript is restored to the pre-turn checkpoint; no
    /// terminal record or snapshot is written.
    #[error("aborted: {0}")]
    Aborted(String),
    /// Same `turn_id` was already committed with a different request text.
    #[error("turn_id conflict: {0}")]
    TurnIdConflict(String),
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
    /// Soft target for transcript length (system + user/assistant/tool).
    ///
    /// Trim drops whole atomic groups (never splits a tool exchange). While a
    /// tool exchange is active at the tail, the whole **active-turn suffix**
    /// (rightmost prior `user` through that exchange) is protected and may
    /// temporarily exceed this cap.
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
    /// Used for **successful** terminal outcomes only (e.g. structured-output
    /// acceptance). Still commits snapshot + terminal + `TurnCommitted`.
    Finish(Vec<HostToolResult>),
    /// Abort the turn **without** applying tool results, incrementing
    /// `completed_turns`, writing snapshot/terminal, or emitting
    /// `TurnCommitted`. Core restores the full pre-turn transcript checkpoint.
    Abort {
        /// Human-readable abort reason (surfaced as [`CoreError::Aborted`]).
        reason: String,
    },
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
    /// [`ToolBatchResult::Continue`] (sample again),
    /// [`ToolBatchResult::Finish`] (apply results and end the user turn), or
    /// [`ToolBatchResult::Abort`] (restore checkpoint; no commit).
    ///
    /// When the model emits tool calls, runs model→tools→model until `end_turn`,
    /// `Finish`, `Abort`, or [`CoreConfig::max_tool_steps`].
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

        if let Some(outcome) = self.check_terminal_replay(&turn_id, &text).await? {
            return Ok(outcome);
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
        let request_text_trimmed = request_text.trim().to_string();
        if turn_id.is_empty() {
            return Err(CoreError::Invalid("turn_id must be non-empty".into()));
        }

        if let Some(outcome) = self
            .check_terminal_replay(&turn_id, &request_text_trimmed)
            .await?
        {
            return Ok(outcome);
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

    /// Idempotent terminal replay shared by submit and continue.
    ///
    /// Same `turn_id` + same trimmed `request_text` → replay without opening a
    /// model stream. Same `turn_id` + different text → [`CoreError::TurnIdConflict`].
    async fn check_terminal_replay(
        &self,
        turn_id: &str,
        request_text: &str,
    ) -> Result<Option<TurnOutcome>, CoreError> {
        let Some(term) = self
            .host
            .load_terminal_turn(&self.session_id, turn_id)
            .await?
        else {
            return Ok(None);
        };

        let stored = term.request_text.trim();
        let incoming = request_text.trim();
        if stored != incoming {
            return Err(CoreError::TurnIdConflict(format!(
                "turn_id `{turn_id}` already committed with different request_text"
            )));
        }

        let events = vec![
            CoreEvent::TurnStarted {
                turn_id: turn_id.to_string(),
            },
            CoreEvent::AssistantDelta {
                turn_id: turn_id.to_string(),
                text: term.assistant_text.clone(),
            },
            CoreEvent::TurnCommitted {
                turn_id: turn_id.to_string(),
                stop_reason: term.stop_reason.clone(),
                replayed: true,
            },
        ];
        Ok(Some(TurnOutcome {
            turn_id: turn_id.to_string(),
            assistant_text: term.assistant_text,
            replayed: true,
            tools_called: Vec::new(),
            events,
        }))
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

        // Full transactional checkpoint: trim_messages may drop old prefixes, so
        // truncate-to-len is insufficient for rollback after mid-turn growth.
        let checkpoint_items = self.items.clone();
        let checkpoint_completed = self.completed_turns;

        if let Some(user_text) = append_user {
            self.items.push(TranscriptItem::text("user", user_text));
            self.trim_messages();
        }

        let restore_checkpoint = |core: &mut Self| {
            core.items = checkpoint_items.clone();
            core.completed_turns = checkpoint_completed;
        };

        let tools = match tools {
            Some(t) => t,
            None => match self.host.list_tools().await {
                Ok(t) => t,
                Err(e) => {
                    restore_checkpoint(self);
                    return Err(CoreError::Host(e));
                }
            },
        };
        let max_steps = self.config.max_tool_steps.max(1);
        let mut tools_called: Vec<TurnToolCall> = Vec::new();
        let mut final_assistant = String::new();
        let mut stop_reason: Option<String> = None;

        for step in 0..max_steps {
            let messages: Vec<ChatMessage> =
                self.items.iter().map(transcript_to_chat_message).collect();

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
                    restore_checkpoint(self);
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
                        restore_checkpoint(self);
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
                ToolBatchResult::Abort { reason } => {
                    restore_checkpoint(self);
                    // No tool results applied, no completed_turns bump, no
                    // snapshot/terminal/TurnCommitted.
                    return Err(CoreError::Aborted(reason));
                }
            };

            // Push the full tool-result batch first, then trim once so a
            // multi-call exchange is never half-trimmed mid-batch.
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
            }
            self.trim_messages();

            if finish_after {
                stop_reason = step_stop.or_else(|| Some("end_turn".into()));
                break;
            }

            if step + 1 >= max_steps {
                restore_checkpoint(self);
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
        let snapshot = match self.export_snapshot() {
            Ok(s) => s,
            Err(e) => {
                restore_checkpoint(self);
                return Err(e);
            }
        };
        if let Err(e) = self
            .host
            .commit_snapshot(&self.session_id, &snapshot, Some(&terminal))
            .await
        {
            restore_checkpoint(self);
            return Err(CoreError::Host(e));
        }

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

    /// Drop oldest complete transcript groups until `items.len() <= max_messages`
    /// (soft target), without splitting a tool exchange.
    ///
    /// Groups:
    /// - each `system` row (singleton);
    /// - `ToolExchange` = assistant with non-empty `tool_calls` + all immediately
    ///   following consecutive `role == "tool"` rows (even unknown/mismatched ids);
    /// - every other row (including orphan tools) as a singleton.
    ///
    /// When the transcript **tail** is a `ToolExchange`, protect the whole
    /// **active-turn suffix**: from the rightmost `role == "user"` `Other` group
    /// before that exchange through the tail (current user, any same-turn prior
    /// exchanges / assistant rows, and the active exchange). If no such user
    /// exists, fall back to protecting only the active exchange. When the tail
    /// is not a `ToolExchange`, nothing is protected by this rule and completed
    /// turns may be trimmed group-by-group.
    ///
    /// Prefer dropping non-system groups outside the protected suffix; excess
    /// older system rows may be dropped, but at least one system is kept when
    /// any exist. If nothing safe remains, allow temporary soft-cap overflow.
    /// Re-partitions after each deletion for simplicity.
    fn trim_messages(&mut self) {
        let max = self.config.max_messages.max(2);
        while self.items.len() > max {
            let groups = partition_transcript_groups(&self.items);
            if groups.is_empty() {
                break;
            }

            // Active-turn suffix: when tail is a ToolExchange, protect from the
            // rightmost prior user (if any) through the end of the transcript.
            let protected_from = active_turn_protected_from(&self.items, &groups);

            let system_count = groups
                .iter()
                .filter(|g| g.kind == TranscriptGroupKind::System)
                .count();

            // 1) Oldest non-system group outside the protected active-turn suffix.
            let mut delete: Option<(usize, usize)> = None;
            for g in &groups {
                if g.kind == TranscriptGroupKind::System {
                    continue;
                }
                if let Some(pf) = protected_from
                    && g.start >= pf
                {
                    continue;
                }
                delete = Some((g.start, g.end));
                break;
            }

            // 2) Else drop oldest excess system (keep ≥1 system if any).
            if delete.is_none()
                && system_count > 1
                && let Some(g) = groups
                    .iter()
                    .find(|g| g.kind == TranscriptGroupKind::System)
            {
                delete = Some((g.start, g.end));
            }

            match delete {
                Some((start, end)) => {
                    self.items.drain(start..end);
                }
                // Nothing safe to drop (protected suffix / sole system).
                // Allow temporary overflow of the soft cap.
                None => break,
            }
        }
    }
}

/// Start index of the protected active-turn suffix, if any.
///
/// When the last group is a [`TranscriptGroupKind::ToolExchange`], returns the
/// start of the rightmost `user` [`TranscriptGroupKind::Other`] group before
/// that exchange, or the exchange start if no such user exists. When the tail
/// is not a tool exchange, returns `None` (no suffix protection).
fn active_turn_protected_from(
    items: &[TranscriptItem],
    groups: &[TranscriptGroup],
) -> Option<usize> {
    let active = groups
        .last()
        .filter(|g| g.kind == TranscriptGroupKind::ToolExchange)?;
    let mut user_start: Option<usize> = None;
    for g in groups {
        if g.start >= active.start {
            break;
        }
        if g.kind == TranscriptGroupKind::Other
            && items.get(g.start).is_some_and(|item| item.role == "user")
        {
            user_start = Some(g.start);
        }
    }
    Some(user_start.unwrap_or(active.start))
}

/// Kind of an atomic transcript group used by [`HyperCore::trim_messages`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptGroupKind {
    System,
    ToolExchange,
    Other,
}

/// Half-open index range `[start, end)` of one atomic group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptGroup {
    start: usize,
    end: usize,
    kind: TranscriptGroupKind,
}

/// Partition transcript rows into atomic groups for soft-cap trimming.
fn partition_transcript_groups(items: &[TranscriptItem]) -> Vec<TranscriptGroup> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let item = &items[i];
        if item.role == "system" {
            groups.push(TranscriptGroup {
                start: i,
                end: i + 1,
                kind: TranscriptGroupKind::System,
            });
            i += 1;
            continue;
        }
        if item.role == "assistant" && !item.tool_calls.is_empty() {
            let start = i;
            i += 1;
            // Consume every following tool row so the exchange stays atomic
            // even when ids are unknown/mismatched.
            while i < items.len() && items[i].role == "tool" {
                i += 1;
            }
            groups.push(TranscriptGroup {
                start,
                end: i,
                kind: TranscriptGroupKind::ToolExchange,
            });
            continue;
        }
        // Plain user/assistant, orphan tool, or other — singleton.
        groups.push(TranscriptGroup {
            start: i,
            end: i + 1,
            kind: TranscriptGroupKind::Other,
        });
        i += 1;
    }
    groups
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
    let snap: SessionSnapshot =
        serde_json::from_slice(bytes).map_err(|e| CoreError::Snapshot(format!("json: {e}")))?;
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use xai_hyper_host::ModelStream;

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
            Some(CoreEvent::TurnCommitted {
                replayed: false,
                ..
            })
        ));

        // Fresh core from same host storage.
        let mut core2 = HyperCore::restore_or_new(host.clone(), session, CoreConfig::default())
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
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolCall { .. }))
        );
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, CoreEvent::ToolResult { .. }))
        );
    }

    #[tokio::test]
    async fn continue_turn_without_user_append() {
        let host = MockHost::new();
        let mut core = HyperCore::restore_or_new(host.clone(), "cont-sess", CoreConfig::default())
            .await
            .unwrap();
        // Pretend chat already has user+assistant history (post-compact seed).
        core.seed_transcript(
            vec![
                TranscriptItem::text("user", "earlier"),
                TranscriptItem::text("assistant", "reply"),
                TranscriptItem::text("user", "now continue"),
            ],
            1,
        );
        let before = core.items().len();
        let out = core
            .continue_turn_with_tools(
                "t-cont",
                "now continue",
                Some(Vec::new()),
                None,
                |_text, calls| async move {
                    // No tools expected from mock echo without with_echo_tool.
                    assert!(calls.is_empty());
                    ToolBatchResult::Continue(vec![])
                },
            )
            .await
            .expect("continue");
        assert!(!out.replayed);
        assert!(out.assistant_text.contains("now continue") || !out.assistant_text.is_empty());
        // No extra user row — only assistant appended.
        assert_eq!(core.items().len(), before + 1);
        assert_eq!(core.items().last().unwrap().role, "assistant");
        assert_eq!(host.model_stream_opens(), 1);
    }

    #[tokio::test]
    async fn abort_does_not_commit_and_restores_full_checkpoint_through_trim() {
        let host = MockHost::with_echo_tool();
        let mut core = HyperCore::restore_or_new(
            host.clone(),
            "abort-sess",
            CoreConfig {
                model: "mock-echo".into(),
                // Small cap so append + tool rows would drop the system prefix
                // if we only truncated by length.
                max_messages: 3,
                max_tool_steps: 8,
            },
        )
        .await
        .unwrap();

        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys-keep"),
                TranscriptItem::text("user", "u0"),
                TranscriptItem::text("assistant", "a0"),
            ],
            1,
        );
        let before_items = core.items().to_vec();
        let before_turns = core.completed_turns();
        assert_eq!(before_items.len(), 3);

        let err = core
            .submit_turn_with_tools(
                TurnRequest {
                    turn_id: "t-abort".into(),
                    text: "please use the tool".into(),
                    json_schema: None,
                    tools: None,
                },
                |_text, _calls| async move {
                    ToolBatchResult::Abort {
                        reason: "permission cancelled".into(),
                    }
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(err, CoreError::Aborted(ref r) if r == "permission cancelled"));
        assert_eq!(core.completed_turns(), before_turns);
        assert_eq!(core.items(), before_items.as_slice());
        // Full restore includes the system prefix that trim would have dropped.
        assert_eq!(core.items()[0].content, "sys-keep");
        assert!(
            !core
                .items()
                .iter()
                .any(|i| i.role == "user" && i.content == "please use the tool")
        );
        // No terminal / no TurnCommitted path: same turn_id can run for real.
        assert!(
            host.load_terminal_turn("abort-sess", "t-abort")
                .await
                .unwrap()
                .is_none()
        );

        let out = core
            .submit_turn_with_tools(
                TurnRequest {
                    turn_id: "t-abort".into(),
                    text: "please use the tool".into(),
                    json_schema: None,
                    tools: None,
                },
                |_text, calls| async move {
                    ToolBatchResult::Continue(
                        calls
                            .into_iter()
                            .map(|c| HostToolResult {
                                call_id: c.id,
                                ok: true,
                                content: "echoed".into(),
                            })
                            .collect(),
                    )
                },
            )
            .await
            .expect("retry after abort");
        assert!(!out.replayed);
        assert_eq!(core.completed_turns(), before_turns + 1);
        assert!(
            host.load_terminal_turn("abort-sess", "t-abort")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn abort_does_not_mutate_persisted_snapshot_or_terminal() {
        use xai_hyper_host::TerminalTurnRecord;

        let host = MockHost::with_echo_tool();
        let session = "abort-persist-sess";

        // Seed plain text history (no tool rows) so the echo-tool mock still
        // emits a tool call on the next submit. Commit that state as baseline
        // without going through a tool-loop turn (which would leave tool rows
        // and make the mock skip tool emission).
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
        .unwrap();
        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys"),
                TranscriptItem::text("user", "prior"),
                TranscriptItem::text("assistant", "prior reply"),
            ],
            1,
        );
        let baseline_snap = core.export_snapshot().expect("export baseline");
        let baseline_term = TerminalTurnRecord {
            turn_id: "t-baseline".into(),
            request_text: "prior".into(),
            assistant_text: "prior reply".into(),
            stop_reason: Some("end_turn".into()),
        };
        host.commit_snapshot(session, &baseline_snap, Some(&baseline_term))
            .await
            .unwrap();

        let mem_items = core.items().to_vec();
        let mem_turns = core.completed_turns();
        assert_eq!(mem_turns, 1);

        // Abort mid tool-loop: host storage must stay byte-identical.
        let err = core
            .submit_turn_with_tools(
                TurnRequest {
                    turn_id: "t-abort-persist".into(),
                    text: "please use the tool".into(),
                    json_schema: None,
                    tools: None,
                },
                |_text, _calls| async move {
                    ToolBatchResult::Abort {
                        reason: "cancel after tools scheduled".into(),
                    }
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Aborted(_)));

        let after_snap = host
            .load_snapshot(session)
            .await
            .unwrap()
            .expect("snapshot still present");
        assert_eq!(
            after_snap, baseline_snap,
            "Abort must not rewrite host snapshot bytes"
        );

        let after_term = host
            .load_terminal_turn(session, "t-baseline")
            .await
            .unwrap()
            .expect("baseline terminal still present");
        assert_eq!(after_term, baseline_term);
        assert!(
            host.load_terminal_turn(session, "t-abort-persist")
                .await
                .unwrap()
                .is_none()
        );

        assert_eq!(core.completed_turns(), mem_turns);
        assert_eq!(core.items(), mem_items.as_slice());
    }

    #[tokio::test]
    async fn finish_still_commits_successfully() {
        let host = MockHost::with_echo_tool();
        let mut core =
            HyperCore::restore_or_new(host.clone(), "finish-sess", CoreConfig::default())
                .await
                .unwrap();

        let out = core
            .submit_turn_with_tools(
                TurnRequest {
                    turn_id: "t-finish".into(),
                    text: "structured please".into(),
                    json_schema: None,
                    tools: None,
                },
                |_text, calls| async move {
                    ToolBatchResult::Finish(
                        calls
                            .into_iter()
                            .map(|c| HostToolResult {
                                call_id: c.id,
                                ok: true,
                                content: "accepted".into(),
                            })
                            .collect(),
                    )
                },
            )
            .await
            .expect("finish");

        assert!(!out.replayed);
        assert_eq!(core.completed_turns(), 1);
        assert!(out.events.iter().any(|e| matches!(
            e,
            CoreEvent::TurnCommitted {
                replayed: false,
                ..
            }
        )));
        assert!(
            host.load_terminal_turn("finish-sess", "t-finish")
                .await
                .unwrap()
                .is_some()
        );
        // Finish applies tool results before ending (user + assistant(tool) + tool).
        assert!(core.items().iter().any(|i| i.role == "tool"));
    }

    #[tokio::test]
    async fn terminal_same_id_different_text_is_conflict() {
        let host = MockHost::new();
        let mut core =
            HyperCore::restore_or_new(host.clone(), "conflict-sess", CoreConfig::default())
                .await
                .unwrap();

        core.submit_turn(TurnRequest {
            turn_id: "t-same".into(),
            text: "first text".into(),
            json_schema: None,
            tools: None,
        })
        .await
        .unwrap();

        let err = core
            .submit_turn(TurnRequest {
                turn_id: "t-same".into(),
                text: "different text".into(),
                json_schema: None,
                tools: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::TurnIdConflict(_)));
        // Conflict must not open another stream.
        assert_eq!(host.model_stream_opens(), 1);

        // continue path shares the same check.
        let err2 = core
            .continue_turn_with_tools(
                "t-same",
                "also different",
                Some(Vec::new()),
                None,
                |_t, _c| async move { ToolBatchResult::Continue(vec![]) },
            )
            .await
            .unwrap_err();
        assert!(matches!(err2, CoreError::TurnIdConflict(_)));
        assert_eq!(host.model_stream_opens(), 1);
    }

    #[tokio::test]
    async fn terminal_same_id_same_text_replays() {
        let host = MockHost::new();
        let mut core =
            HyperCore::restore_or_new(host.clone(), "replay-sess", CoreConfig::default())
                .await
                .unwrap();

        let first = core
            .submit_turn(TurnRequest {
                turn_id: "t-replay".into(),
                text: "  hello  ".into(),
                json_schema: None,
                tools: None,
            })
            .await
            .unwrap();
        assert!(!first.replayed);

        let second = core
            .submit_turn(TurnRequest {
                turn_id: "t-replay".into(),
                // trim-equal to first request.
                text: "hello".into(),
                json_schema: None,
                tools: None,
            })
            .await
            .unwrap();
        assert!(second.replayed);
        assert_eq!(second.assistant_text, first.assistant_text);
        assert_eq!(host.model_stream_opens(), 1);

        // continue with same id + same trimmed text also replays.
        let third = core
            .continue_turn_with_tools(
                "t-replay",
                "hello",
                Some(Vec::new()),
                None,
                |_t, _c| async move { ToolBatchResult::Continue(vec![]) },
            )
            .await
            .unwrap();
        assert!(third.replayed);
        assert_eq!(host.model_stream_opens(), 1);
    }

    /// Host that emits two tool calls on the first open and captures every
    /// [`ModelStreamRequest`] so tests can assert legal tool-exchange structure.
    #[derive(Clone)]
    struct MultiToolCaptureHost {
        opens: Arc<AtomicUsize>,
        captured: Arc<Mutex<Vec<ModelStreamRequest>>>,
    }

    impl MultiToolCaptureHost {
        fn new() -> Self {
            Self {
                opens: Arc::new(AtomicUsize::new(0)),
                captured: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn open_count(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }

        fn captured(&self) -> Vec<ModelStreamRequest> {
            self.captured.lock().expect("captured").clone()
        }
    }

    struct StaticStream {
        chunks: std::vec::IntoIter<ModelChunk>,
    }

    #[async_trait::async_trait]
    impl ModelStream for StaticStream {
        async fn next_chunk(&mut self) -> Result<Option<ModelChunk>, HostError> {
            Ok(self.chunks.next())
        }
    }

    #[async_trait::async_trait]
    impl HyperHost for MultiToolCaptureHost {
        async fn open_model_stream(
            &self,
            req: ModelStreamRequest,
        ) -> Result<Box<dyn ModelStream>, HostError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            let n = self.opens.load(Ordering::SeqCst);
            self.captured.lock().expect("captured").push(req);

            let chunks = if n == 1 {
                vec![
                    ModelChunk::ToolCall {
                        id: "call_a".into(),
                        name: "alpha".into(),
                        arguments: r#"{"x":1}"#.into(),
                    },
                    ModelChunk::ToolCall {
                        id: "call_b".into(),
                        name: "beta".into(),
                        arguments: r#"{"y":2}"#.into(),
                    },
                    ModelChunk::Done {
                        stop_reason: Some("tool_calls".into()),
                    },
                ]
            } else {
                vec![
                    ModelChunk::TextDelta("final after tools".into()),
                    ModelChunk::Done {
                        stop_reason: Some("end_turn".into()),
                    },
                ]
            };
            Ok(Box::new(StaticStream {
                chunks: chunks.into_iter(),
            }))
        }

        async fn commit_snapshot(
            &self,
            _session_id: &str,
            _snapshot: &[u8],
            _terminal: Option<&xai_hyper_host::TerminalTurnRecord>,
        ) -> Result<(), HostError> {
            Ok(())
        }

        async fn load_snapshot(&self, _session_id: &str) -> Result<Option<Vec<u8>>, HostError> {
            Ok(None)
        }

        async fn load_terminal_turn(
            &self,
            _session_id: &str,
            _turn_id: &str,
        ) -> Result<Option<xai_hyper_host::TerminalTurnRecord>, HostError> {
            Ok(None)
        }

        fn now_unix_ms(&self) -> u64 {
            0
        }
    }

    fn assert_tool_exchanges_intact(items: &[TranscriptItem]) {
        let groups = partition_transcript_groups(items);
        for g in groups {
            if g.kind != TranscriptGroupKind::ToolExchange {
                continue;
            }
            let parent = &items[g.start];
            assert_eq!(parent.role, "assistant");
            assert!(!parent.tool_calls.is_empty());
            for item in items.iter().take(g.end).skip(g.start + 1) {
                assert_eq!(item.role, "tool");
            }
        }
        // No orphan tool may sit immediately after a non-tool-call assistant
        // or user without its parent — every tool after an exchange parent is
        // inside a ToolExchange group; remaining tools are singletons only
        // when not preceded by assistant(tool_calls).
        let mut i = 0;
        while i < items.len() {
            if items[i].role == "assistant" && !items[i].tool_calls.is_empty() {
                i += 1;
                while i < items.len() && items[i].role == "tool" {
                    i += 1;
                }
                continue;
            }
            // Orphan tools are allowed as seed anomalies but must not appear
            // mid-exchange (already covered by partition).
            i += 1;
        }
    }

    #[test]
    fn partition_groups_system_exchange_and_orphan_tool() {
        let items = vec![
            TranscriptItem::text("system", "s"),
            TranscriptItem::text("user", "u"),
            TranscriptItem {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![
                    TranscriptToolCall {
                        id: "c1".into(),
                        name: "t".into(),
                        arguments: "{}".into(),
                    },
                    TranscriptToolCall {
                        id: "c2".into(),
                        name: "t".into(),
                        arguments: "{}".into(),
                    },
                ],
                tool_call_id: None,
            },
            TranscriptItem {
                role: "tool".into(),
                content: "r1".into(),
                tool_calls: Vec::new(),
                tool_call_id: Some("c1".into()),
            },
            // Unknown id still joins the exchange (consecutive tool rows).
            TranscriptItem {
                role: "tool".into(),
                content: "r2".into(),
                tool_calls: Vec::new(),
                tool_call_id: Some("unknown".into()),
            },
            TranscriptItem::text("assistant", "plain"),
            // Orphan only when not immediately after an exchange parent.
            TranscriptItem {
                role: "tool".into(),
                content: "orphan".into(),
                tool_calls: Vec::new(),
                tool_call_id: Some("lone".into()),
            },
        ];
        let groups = partition_transcript_groups(&items);
        assert_eq!(groups.len(), 5);
        assert_eq!(groups[0].kind, TranscriptGroupKind::System);
        assert_eq!(groups[1].kind, TranscriptGroupKind::Other); // user
        assert_eq!(groups[2].kind, TranscriptGroupKind::ToolExchange);
        assert_eq!(groups[2].start, 2);
        assert_eq!(groups[2].end, 5); // assistant + 2 tools (incl unknown id)
        assert_eq!(groups[3].kind, TranscriptGroupKind::Other); // plain assistant
        assert_eq!(groups[4].kind, TranscriptGroupKind::Other); // orphan tool
        assert_eq!(groups[4].start, 6);
    }

    #[tokio::test]
    async fn group_aware_trim_protects_active_multi_tool_exchange() {
        // max_messages=3 soft target; system + history, then assistant with 2
        // calls + 2 tool rows must stay together even when len > 3.
        let host = MultiToolCaptureHost::new();
        let mut core = HyperCore::restore_or_new(
            host.clone(),
            "trim-active-exchange",
            CoreConfig {
                model: "multi-tool".into(),
                max_messages: 3,
                max_tool_steps: 8,
            },
        )
        .await
        .unwrap();

        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys"),
                TranscriptItem::text("user", "old-u"),
                TranscriptItem::text("assistant", "old-a"),
            ],
            1,
        );
        assert_eq!(core.items().len(), 3);

        let out = core
            .submit_turn_with_tools(
                TurnRequest {
                    turn_id: "t-multi".into(),
                    text: "run two tools".into(),
                    json_schema: None,
                    tools: Some(Vec::new()),
                },
                |_text, calls| async move {
                    assert_eq!(calls.len(), 2);
                    ToolBatchResult::Continue(
                        calls
                            .into_iter()
                            .map(|c| HostToolResult {
                                call_id: c.id,
                                ok: true,
                                content: "ok".into(),
                            })
                            .collect(),
                    )
                },
            )
            .await
            .expect("multi tool turn");

        assert!(!out.replayed);
        assert_eq!(out.tools_called.len(), 2);
        assert_eq!(host.open_count(), 2);

        // Second model request must see a legal active-turn suffix: current
        // user before parent assistant(tool_calls), matching tool rows, and
        // system retained.
        let captured = host.captured();
        assert_eq!(captured.len(), 2);
        let second = &captured[1].messages;

        let system_idxs: Vec<usize> = second
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == "system")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(system_idxs.len(), 1, "exactly one system on second open");

        let user_idx = second
            .iter()
            .position(|m| m.role == "user" && m.content == "run two tools")
            .expect("current user must survive soft-cap trim into second request");
        let parent_idx = second
            .iter()
            .position(|m| m.role == "assistant" && !m.tool_calls.is_empty())
            .expect("parent assistant(tool_calls) on second open");
        assert!(
            user_idx < parent_idx,
            "current user must precede tool-call parent: user={user_idx} parent={parent_idx}"
        );

        let call_ids: Vec<&str> = second[parent_idx]
            .tool_calls
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert!(call_ids.contains(&"call_a"));
        assert!(call_ids.contains(&"call_b"));
        let mut seen = Vec::new();
        let mut j = parent_idx + 1;
        while j < second.len() && second[j].role == "tool" {
            seen.push(second[j].tool_call_id.clone().unwrap_or_default());
            j += 1;
        }
        assert!(
            seen.contains(&"call_a".to_string()) && seen.contains(&"call_b".to_string()),
            "tool rows must answer parent call ids: {seen:?}"
        );

        // Ideal soft-cap path: only system + active-turn suffix remain.
        let roles: Vec<&str> = second.iter().map(|m| m.role.as_str()).collect();
        if roles == ["system", "user", "assistant", "tool", "tool"] {
            assert_eq!(second[1].content, "run two tools");
        } else {
            // Extra history is allowed; relative order of suffix must hold.
            assert!(roles.contains(&"system"));
            assert!(user_idx < parent_idx);
        }

        // After final plain assistant, any remaining exchange is whole or gone.
        assert_tool_exchanges_intact(core.items());
        assert!(
            core.items()
                .iter()
                .any(|it| it.role == "assistant" && it.content.contains("final after tools"))
        );
        // Soft cap may still be exceeded only if protected content remains;
        // with final plain assistant, active exchange is no longer protected
        // and older groups should have been dropped toward max.
        let groups = partition_transcript_groups(core.items());
        let exchange_count = groups
            .iter()
            .filter(|g| g.kind == TranscriptGroupKind::ToolExchange)
            .count();
        // Either the whole exchange remains (with system/final) or it was
        // dropped as a unit — never partial.
        if exchange_count == 0 {
            assert!(
                !core.items().iter().any(|it| it.role == "tool"),
                "no orphan tools after exchange drop"
            );
        } else {
            assert_eq!(exchange_count, 1);
            let ex = groups
                .iter()
                .find(|g| g.kind == TranscriptGroupKind::ToolExchange)
                .unwrap();
            assert_eq!(ex.end - ex.start, 3); // assistant + 2 tools
        }
    }

    #[tokio::test]
    async fn group_aware_trim_keeps_parent_and_tool_rows_mid_batch() {
        // Drive trim after assistant(tool_calls) with max=3 and verify the
        // parent is retained even before tool results arrive.
        let host = MultiToolCaptureHost::new();
        let mut core = HyperCore::restore_or_new(
            host.clone(),
            "trim-mid-batch",
            CoreConfig {
                model: "multi-tool".into(),
                max_messages: 3,
                max_tool_steps: 8,
            },
        )
        .await
        .unwrap();

        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys"),
                TranscriptItem::text("user", "u0"),
                TranscriptItem::text("assistant", "a0"),
            ],
            0,
        );

        // Abort after tools are scheduled but before results would commit —
        // first we need a path that observes mid-loop state. Use a custom
        // invoker that inspects core items via side channel is hard; instead
        // seed an in-progress exchange and call trim directly.
        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys"),
                TranscriptItem::text("user", "u0"),
                TranscriptItem::text("assistant", "a0"),
                TranscriptItem::text("user", "u1"),
                TranscriptItem {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![
                        TranscriptToolCall {
                            id: "c1".into(),
                            name: "t1".into(),
                            arguments: "{}".into(),
                        },
                        TranscriptToolCall {
                            id: "c2".into(),
                            name: "t2".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    tool_call_id: None,
                },
            ],
            0,
        );
        // seed_transcript trims: active-turn suffix (user u1 + assistant w/
        // calls) protected; older non-system dropped toward soft cap 3.
        let items = core.items();
        assert!(
            items
                .iter()
                .any(|i| i.role == "assistant" && i.tool_calls.len() == 2),
            "parent assistant with tool_calls must survive soft-cap trim: {items:?}"
        );
        assert!(
            items.iter().any(|i| i.role == "user" && i.content == "u1"),
            "current user anchors active-turn suffix: {items:?}"
        );
        assert!(
            items.iter().any(|i| i.role == "system"),
            "system kept when possible"
        );
        // system + user + parent assistant may fit exactly at soft cap 3.
        assert!(items.len() >= 3);
        assert!(
            !items.iter().any(|i| i.role == "user" && i.content == "u0"),
            "older turn history outside suffix should drop"
        );

        // Append tool rows (as batch would) and trim once.
        let mut items = core.items().to_vec();
        items.push(TranscriptItem {
            role: "tool".into(),
            content: "r1".into(),
            tool_calls: Vec::new(),
            tool_call_id: Some("c1".into()),
        });
        items.push(TranscriptItem {
            role: "tool".into(),
            content: "r2".into(),
            tool_calls: Vec::new(),
            tool_call_id: Some("c2".into()),
        });
        core.seed_transcript(items, 0);
        let items = core.items();
        assert_tool_exchanges_intact(items);
        let user_idx = items
            .iter()
            .position(|i| i.role == "user" && i.content == "u1")
            .expect("current user present");
        let parent_idx = items
            .iter()
            .position(|i| i.role == "assistant" && i.tool_calls.len() == 2)
            .expect("parent present");
        assert!(user_idx < parent_idx);
        assert_eq!(
            items.get(parent_idx + 1).map(|i| i.role.as_str()),
            Some("tool")
        );
        assert_eq!(
            items.get(parent_idx + 2).map(|i| i.role.as_str()),
            Some("tool")
        );
        // Soft cap 3 but protected active-turn suffix + system → len > max.
        assert!(items.len() > 3);
    }

    #[tokio::test]
    async fn active_turn_suffix_keeps_user_and_exchange_over_soft_cap() {
        // Pure unit: old history + current user + active two-tool exchange.
        // max=3 → old rows drop; system + user + full exchange remain (len>max).
        let host = MockHost::new();
        let mut core = HyperCore::restore_or_new(
            host,
            "trim-active-suffix",
            CoreConfig {
                model: "mock-echo".into(),
                max_messages: 3,
                max_tool_steps: 8,
            },
        )
        .await
        .unwrap();

        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys"),
                TranscriptItem::text("user", "old-u"),
                TranscriptItem::text("assistant", "old-a"),
                TranscriptItem::text("user", "current"),
                TranscriptItem {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![
                        TranscriptToolCall {
                            id: "c1".into(),
                            name: "t1".into(),
                            arguments: "{}".into(),
                        },
                        TranscriptToolCall {
                            id: "c2".into(),
                            name: "t2".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    tool_call_id: None,
                },
                TranscriptItem {
                    role: "tool".into(),
                    content: "r1".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("c1".into()),
                },
                TranscriptItem {
                    role: "tool".into(),
                    content: "r2".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("c2".into()),
                },
            ],
            0,
        );

        let items = core.items();
        assert!(items.len() > 3, "soft overflow while active turn protected");
        assert!(items.iter().any(|i| i.role == "system"));
        assert!(
            !items
                .iter()
                .any(|i| i.role == "user" && i.content == "old-u"),
            "old history outside suffix should drop"
        );
        assert!(
            !items
                .iter()
                .any(|i| i.role == "assistant" && i.content == "old-a")
        );

        let roles: Vec<&str> = items.iter().map(|i| i.role.as_str()).collect();
        assert_eq!(
            roles,
            ["system", "user", "assistant", "tool", "tool"],
            "expected system + active-turn suffix: {roles:?}"
        );
        assert_eq!(items[1].content, "current");
        assert_eq!(items[2].tool_calls.len(), 2);
        assert_tool_exchanges_intact(items);

        // continue/seed path without a *new* append: rightmost existing user
        // still anchors protection (re-trim same transcript).
        let again = items.to_vec();
        core.seed_transcript(again, 0);
        let roles2: Vec<&str> = core.items().iter().map(|i| i.role.as_str()).collect();
        assert_eq!(roles2, ["system", "user", "assistant", "tool", "tool"]);
        assert_eq!(core.items()[1].content, "current");
    }

    #[tokio::test]
    async fn active_turn_suffix_keeps_prior_same_turn_exchanges() {
        // Same turn: system, current user, completed ex1, active ex2 — tiny max.
        // Entire user→ex1→ex2 suffix must remain.
        let host = MockHost::new();
        let mut core = HyperCore::restore_or_new(
            host,
            "trim-multi-ex-suffix",
            CoreConfig {
                model: "mock-echo".into(),
                max_messages: 2,
                max_tool_steps: 8,
            },
        )
        .await
        .unwrap();

        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys"),
                TranscriptItem::text("user", "old-drop"),
                TranscriptItem::text("assistant", "old-drop-a"),
                TranscriptItem::text("user", "current-turn"),
                // completed exchange 1
                TranscriptItem {
                    role: "assistant".into(),
                    content: "step1".into(),
                    tool_calls: vec![TranscriptToolCall {
                        id: "ex1".into(),
                        name: "t".into(),
                        arguments: "{}".into(),
                    }],
                    tool_call_id: None,
                },
                TranscriptItem {
                    role: "tool".into(),
                    content: "ex1-result".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("ex1".into()),
                },
                // active exchange 2 (tail)
                TranscriptItem {
                    role: "assistant".into(),
                    content: "step2".into(),
                    tool_calls: vec![
                        TranscriptToolCall {
                            id: "ex2a".into(),
                            name: "t".into(),
                            arguments: "{}".into(),
                        },
                        TranscriptToolCall {
                            id: "ex2b".into(),
                            name: "t".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    tool_call_id: None,
                },
                TranscriptItem {
                    role: "tool".into(),
                    content: "ex2a-result".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("ex2a".into()),
                },
                TranscriptItem {
                    role: "tool".into(),
                    content: "ex2b-result".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("ex2b".into()),
                },
            ],
            0,
        );

        let items = core.items();
        assert!(items.len() > 2);
        assert!(
            !items
                .iter()
                .any(|i| i.content == "old-drop" || i.content == "old-drop-a"),
            "pre-turn history must be droppable: {items:?}"
        );
        assert!(items.iter().any(|i| i.role == "system"));

        let user_idx = items
            .iter()
            .position(|i| i.role == "user" && i.content == "current-turn")
            .expect("current user");
        let ex1_idx = items
            .iter()
            .position(|i| i.role == "assistant" && i.tool_calls.iter().any(|c| c.id == "ex1"))
            .expect("completed same-turn exchange");
        let ex2_idx = items
            .iter()
            .position(|i| i.role == "assistant" && i.tool_calls.iter().any(|c| c.id == "ex2a"))
            .expect("active exchange");
        assert!(user_idx < ex1_idx && ex1_idx < ex2_idx);
        // Full ex1 + ex2 tool rows still present.
        assert!(
            items
                .iter()
                .any(|i| i.tool_call_id.as_deref() == Some("ex1"))
        );
        assert!(
            items
                .iter()
                .any(|i| i.tool_call_id.as_deref() == Some("ex2a"))
        );
        assert!(
            items
                .iter()
                .any(|i| i.tool_call_id.as_deref() == Some("ex2b"))
        );
        assert_tool_exchanges_intact(items);

        let groups = partition_transcript_groups(items);
        let pf = active_turn_protected_from(items, &groups).expect("protected suffix");
        assert_eq!(pf, user_idx);
        assert_eq!(
            groups.last().unwrap().kind,
            TranscriptGroupKind::ToolExchange
        );
    }

    #[tokio::test]
    async fn text_only_history_still_trims_oldest_non_system() {
        let host = MockHost::new();
        let mut core = HyperCore::restore_or_new(
            host,
            "trim-text",
            CoreConfig {
                model: "mock-echo".into(),
                max_messages: 3,
                max_tool_steps: 8,
            },
        )
        .await
        .unwrap();

        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys"),
                TranscriptItem::text("user", "u0"),
                TranscriptItem::text("assistant", "a0"),
                TranscriptItem::text("user", "u1"),
                TranscriptItem::text("assistant", "a1"),
            ],
            2,
        );
        // Soft target 3: system + last two plain messages.
        assert_eq!(core.items().len(), 3);
        assert_eq!(core.items()[0].role, "system");
        assert_eq!(core.items()[0].content, "sys");
        assert_eq!(core.items()[1].content, "u1");
        assert_eq!(core.items()[2].content, "a1");
    }

    #[tokio::test]
    async fn orphan_tool_seed_no_panic_and_old_exchange_atomic() {
        let host = MockHost::new();
        let mut core = HyperCore::restore_or_new(
            host,
            "trim-orphan",
            CoreConfig {
                model: "mock-echo".into(),
                max_messages: 3,
                max_tool_steps: 8,
            },
        )
        .await
        .unwrap();

        // Seed: system, old complete exchange, orphan tool, recent text.
        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys"),
                TranscriptItem {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![TranscriptToolCall {
                        id: "old".into(),
                        name: "t".into(),
                        arguments: "{}".into(),
                    }],
                    tool_call_id: None,
                },
                TranscriptItem {
                    role: "tool".into(),
                    content: "old-result".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("old".into()),
                },
                TranscriptItem {
                    role: "tool".into(),
                    content: "orphan".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("ghost".into()),
                },
                TranscriptItem::text("user", "new"),
                TranscriptItem::text("assistant", "reply"),
            ],
            1,
        );

        assert_tool_exchanges_intact(core.items());
        // No panic; len converges toward soft cap.
        assert!(
            core.items().len() <= 3 || {
                // If still over, only because of protected tail — tail is plain
                // assistant so should be <= 3.
                false
            }
        );
        assert_eq!(core.items().len(), 3);
        assert_eq!(core.items()[0].role, "system");

        // Old exchange either fully present or fully gone.
        let has_old_parent = core
            .items()
            .iter()
            .any(|i| i.role == "assistant" && i.tool_calls.iter().any(|c| c.id == "old"));
        let has_old_tool = core
            .items()
            .iter()
            .any(|i| i.role == "tool" && i.tool_call_id.as_deref() == Some("old"));
        assert_eq!(
            has_old_parent, has_old_tool,
            "old exchange must be all-or-nothing"
        );
        // With max=3 and recent user+assistant, old exchange should be gone.
        assert!(!has_old_parent);
        // Orphan tool also dropped as a singleton.
        assert!(
            !core
                .items()
                .iter()
                .any(|i| i.tool_call_id.as_deref() == Some("ghost"))
        );
    }

    #[tokio::test]
    async fn completed_exchange_dropped_as_whole_after_final_assistant() {
        let host = MockHost::new();
        let mut core = HyperCore::restore_or_new(
            host,
            "trim-drop-exchange",
            CoreConfig {
                model: "mock-echo".into(),
                max_messages: 3,
                max_tool_steps: 8,
            },
        )
        .await
        .unwrap();

        core.seed_transcript(
            vec![
                TranscriptItem::text("system", "sys"),
                TranscriptItem {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![
                        TranscriptToolCall {
                            id: "c1".into(),
                            name: "t".into(),
                            arguments: "{}".into(),
                        },
                        TranscriptToolCall {
                            id: "c2".into(),
                            name: "t".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    tool_call_id: None,
                },
                TranscriptItem {
                    role: "tool".into(),
                    content: "r1".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("c1".into()),
                },
                TranscriptItem {
                    role: "tool".into(),
                    content: "r2".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("c2".into()),
                },
                TranscriptItem::text("assistant", "final plain"),
            ],
            1,
        );

        // Final plain assistant is at tail → exchange is not active/protected.
        // Soft cap 3: system + ? ; exchange (3 rows) is one group — either keep
        // system+exchange (len 4 > 3 soft, but exchange not protected so should
        // drop exchange and keep system + final) or similar.
        assert_tool_exchanges_intact(core.items());
        let has_any_tool = core.items().iter().any(|i| i.role == "tool");
        let has_parent = core
            .items()
            .iter()
            .any(|i| i.role == "assistant" && !i.tool_calls.is_empty());
        assert_eq!(has_any_tool, has_parent);
        // Prefer system + final plain; exchange should be fully dropped.
        assert!(!has_any_tool, "completed exchange dropped as a unit");
        assert_eq!(core.items().last().unwrap().content, "final plain");
        assert!(core.items().iter().any(|i| i.role == "system"));
        assert!(core.items().len() <= 3);
    }
}
