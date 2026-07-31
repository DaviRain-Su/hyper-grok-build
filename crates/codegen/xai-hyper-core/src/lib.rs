//! Hypercore session / turn engine.
//!
//! Owns conversation state and turn orchestration. All I/O goes through
//! [`xai_hyper_host::HyperHost`].
//!
//! - Phase 0: [`mock::MockHost`] echo stream (always available).
//! - Phase 1: [`native::NativeHost`] (feature `native`, default) — disk
//!   snapshots under `~/.grok/hypercore/` + real model stream via sampler.
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
    ChatMessage, HostError, HyperHost, ModelChunk, ModelStreamRequest, SessionId, TerminalTurnRecord,
    TurnId,
};

/// Snapshot schema version (JSON). Bump when breaking snapshot shape.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

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
    /// Max transcript messages kept (system + user/assistant). Soft cap.
    pub max_messages: usize,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            model: "mock-echo".into(),
            max_messages: 256,
        }
    }
}

/// Client turn request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRequest {
    /// Idempotent turn id (client-generated).
    pub turn_id: TurnId,
    /// User text.
    pub text: String,
}

/// Outcome of [`HyperCore::submit_turn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// Turn id.
    pub turn_id: TurnId,
    /// Full assistant text for this turn.
    pub assistant_text: String,
    /// `true` if this was a terminal replay (no new model call).
    pub replayed: bool,
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

/// One transcript row in the Phase 0 snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct TranscriptItem {
    /// Role.
    pub role: String,
    /// Text.
    pub content: String,
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
    /// Used by the shell bypass to seed context from `chat_state` before
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

    /// Submit a user turn. Replays terminal records for the same `turn_id`.
    pub async fn submit_turn(&mut self, req: TurnRequest) -> Result<TurnOutcome, CoreError> {
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
                events,
            });
        }

        let mut events = vec![
            CoreEvent::Status {
                text: "turn started".into(),
            },
            CoreEvent::TurnStarted {
                turn_id: turn_id.clone(),
            },
        ];

        // Append user message for this turn (not yet committed until success).
        self.items.push(TranscriptItem {
            role: "user".into(),
            content: text.clone(),
        });
        self.trim_messages();

        let messages: Vec<ChatMessage> = self
            .items
            .iter()
            .map(|i| ChatMessage {
                role: i.role.clone(),
                content: i.content.clone(),
            })
            .collect();

        let stream_req = ModelStreamRequest {
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
            model: self.config.model.clone(),
            messages,
        };

        let mut stream = match self.host.open_model_stream(stream_req).await {
            Ok(s) => s,
            Err(e) => {
                // Roll back uncommitted user message.
                self.items.pop();
                let err = e.to_string();
                events.push(CoreEvent::TurnFailed {
                    turn_id: turn_id.clone(),
                    error: err.clone(),
                });
                return Err(CoreError::Host(e));
            }
        };

        let mut assistant = String::new();
        let mut stop_reason = None;
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
                Ok(Some(ModelChunk::Done { stop_reason: sr })) => {
                    stop_reason = sr;
                    break;
                }
                Ok(None) => break,
                Err(e) => {
                    self.items.pop();
                    events.push(CoreEvent::TurnFailed {
                        turn_id: turn_id.clone(),
                        error: e.to_string(),
                    });
                    return Err(CoreError::Host(e));
                }
            }
        }

        self.items.push(TranscriptItem {
            role: "assistant".into(),
            content: assistant.clone(),
        });
        self.trim_messages();
        self.completed_turns = self.completed_turns.saturating_add(1);

        let terminal = TerminalTurnRecord {
            turn_id: turn_id.clone(),
            request_text: text,
            assistant_text: assistant.clone(),
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
            assistant_text: assistant,
            replayed: false,
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
        })
        .await
        .unwrap();
        core.submit_turn(TurnRequest {
            turn_id: "t2".into(),
            text: "two".into(),
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
        })
        .await
        .unwrap();
        let bytes = core.export_snapshot().unwrap();
        let snap: SessionSnapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snap.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(snap.session_id, "s4");
        assert_eq!(snap.completed_turns, 1);
    }
}
