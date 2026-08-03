//! xai-chat-state — Actor-based chat state management for xAI agents.
//!
//! This crate extracts conversation state management from `xai-grok-shell`'s
//! `acp_session.rs` into a standalone actor. It follows the same actor pattern
//! as `xai-hunk-tracker`:
//!
//! ```text
//! ┌────────────────┐                  ┌──────────────────────────────────────┐
//! │ SessionActor   │ ─── Command ───▶ │        ChatStateActor                │
//! │  (push_user,   │                  │  (runs in dedicated tokio task)      │
//! │   build_req)   │                  │                                      │
//! └────────────────┘                  │  State (no locks needed):            │
//!                                     │  - conversation: Vec<ConversationItem>│
//! ┌────────────────┐                  │  - sampling_config: SamplingConfig   │
//! │   Query (e.g.  │ ── Cmd+Oneshot ─▶│  - prompt_index: usize              │
//! │  get_conv)     │ ◀── Response ────│  - total_tokens: u64                │
//! └────────────────┘                  │                                      │
//!                                     │         │ ChatStateEvent             │
//!                                     │         ▼                            │
//!                                     │  ┌──────────────────┐               │
//!                                     │  │ event_tx         │───▶ Session   │
//!                                     │  └──────────────────┘               │
//!                                     └──────────────────────────────────────┘
//! ```

pub mod actor;
pub mod commands;
pub mod compaction_mode;
pub mod compaction_transcript;
pub mod compaction_utils;
pub mod conversation_util;
pub mod events;
pub mod handle;
pub mod persistence;
pub mod types;
pub mod usage;

// Re-export main types for convenience
pub use actor::ChatStateActor;
pub use actor::state::{
    estimate_conversation_tokens, estimate_item_tokens, estimate_messages_tokens,
    estimate_system_message_tokens, estimate_tool_definition_tokens,
    estimate_tool_definitions_tokens,
};
pub use commands::{ModelMetadata, StrictAppendAck, StrictAppendError};
pub use compaction_mode::CompactionMode;
pub use compaction_transcript::CompactionDetail;
pub use events::ChatStateEvent;
pub use handle::ChatStateHandle;
pub use persistence::{
    ChatPersistence, MockChatPersistence, MockPersistenceReceiver, NullChatPersistence,
    PersistenceRecord,
};
pub use types::*;
pub use usage::{UsageLedger, UsageTotals};
