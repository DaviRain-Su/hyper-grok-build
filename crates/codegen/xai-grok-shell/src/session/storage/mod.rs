use async_trait::async_trait;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::extensions::notification::SessionNotification;
use crate::sampling::ConversationItem;
use crate::session::info::Info;
use crate::session::persistence::Summary;
use crate::session::signals::SessionSignals;
use crate::session::wire_tags::{
    AVAILABLE_COMMANDS_UPDATE_PREFIX, REWIND_MARKER, USER_MESSAGE_CHUNK,
};
use crate::tools::todo::TodoState;
use agent_client_protocol as acp;
use xai_grok_sampling_types::ReasoningEffort;
use xai_grok_workspace::session::file_state::RewindPoint;

pub mod jsonl;
pub mod search;
pub mod search_fts;
pub mod search_remote_sync;
pub(crate) mod summary_write;

/// On-disk file names, relative to a session directory. Single source of truth for
/// the storage adapter and the session/state and session/import extensions.
pub(crate) const SUMMARY_FILE: &str = "summary.json";
pub(crate) const PLAN_FILE: &str = "plan.json";
pub(crate) const PLAN_MODE_FILE: &str = "plan_mode.json";
pub(crate) const SIGNALS_FILE: &str = "signals.json";
pub(crate) const GOAL_STATE_FILE: &str = "goal/state.json";
pub(crate) const ANNOUNCEMENT_STATE_FILE: &str = "announcement_state.json";
pub(crate) const CHAT_HISTORY_FILE: &str = "chat_history.jsonl";
pub(crate) const UPDATES_FILE: &str = "updates.jsonl";

/// Write `bytes` to `path` by writing a uniquely named sibling temp file and
/// renaming it over the target, so a crash or a concurrent writer never leaves a
/// torn file. The temp is removed on failure.
pub(crate) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(path);
    match std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Async sibling of [`write_bytes_atomic`].
pub(crate) async fn write_bytes_atomic_async(path: &Path, bytes: Vec<u8>) -> io::Result<()> {
    let tmp = temp_sibling(path);
    let result = match tokio::fs::write(&tmp, bytes).await {
        Ok(()) => tokio::fs::rename(&tmp, path).await,
        Err(e) => Err(e),
    };
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// Serialize `items` to newline-delimited JSON bytes.
fn to_jsonl_bytes<T: serde::Serialize>(items: &[T]) -> io::Result<Vec<u8>> {
    let mut content = Vec::new();
    for item in items {
        serde_json::to_writer(&mut content, item)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        content.push(b'\n');
    }
    Ok(content)
}

/// Write `items` as newline-delimited JSON to `path`, atomically (see
/// [`write_bytes_atomic`]).
pub(crate) fn write_jsonl_atomic<T: serde::Serialize>(path: &Path, items: &[T]) -> io::Result<()> {
    write_bytes_atomic(path, &to_jsonl_bytes(items)?)
}

/// Async sibling of [`write_jsonl_atomic`].
pub(crate) async fn write_jsonl_atomic_async<T: serde::Serialize>(
    path: &Path,
    items: &[T],
) -> io::Result<()> {
    write_bytes_atomic_async(path, to_jsonl_bytes(items)?).await
}

/// A unique sibling temp path, e.g. `summary.json` -> `summary.json.<uuid>.tmp`.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.tmp", uuid::Uuid::now_v7()));
    PathBuf::from(name)
}

/// Rebuild the derived `chat_history.jsonl` cache from `updates.jsonl`, the durable
/// source of truth, so a session restores from its update stream alone.
pub(crate) mod chat_rebuild {
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::path::Path;

    use agent_client_protocol as acp;

    use super::{CHAT_HISTORY_FILE, SessionUpdate, UPDATES_FILE, UpdatesIterator};
    use crate::sampling::{AssistantItem, ContentPart, ConversationItem, ToolCall};

    /// Rebuild `chat_history.jsonl` from `updates.jsonl` alone. Builds a temp file and
    /// renames it over the target, so a failed rebuild leaves the existing cache intact
    /// rather than a truncated partial that load would trust.
    pub(crate) fn rebuild_chat_history(dir: &Path) -> io::Result<usize> {
        use std::io::{Seek, Write};

        let updates_path = dir.join(UPDATES_FILE);
        let Some(iter) = UpdatesIterator::open(&updates_path)? else {
            return Ok(0);
        };

        let chat_path = dir.join(CHAT_HISTORY_FILE);
        let tmp_path = dir.join(format!("{CHAT_HISTORY_FILE}.{}.tmp", uuid::Uuid::now_v7()));
        let file = std::fs::File::create(&tmp_path)?;
        let mut writer = std::io::BufWriter::new(file);
        let mut reducer = ChatReducer::new();

        for result in iter {
            let update = match result {
                Ok(u) => u,
                Err(_) => continue,
            };

            for item in reducer.process(&update) {
                if let Ok(line) = serde_json::to_string(&item) {
                    let _ = writer.write_all(line.as_bytes());
                    let _ = writer.write_all(b"\n");
                }
            }

            // CompactionCheckpoint: truncate file and reset
            if reducer.should_truncate() {
                reducer.clear_truncate_flag();
                let _ = writer.seek(std::io::SeekFrom::Start(0));
                let _ = writer.get_mut().set_len(0);
            }
        }

        for item in reducer.flush() {
            if let Ok(line) = serde_json::to_string(&item) {
                let _ = writer.write_all(line.as_bytes());
                let _ = writer.write_all(b"\n");
            }
        }

        if let Err(e) = writer.flush() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        drop(writer);
        if let Err(e) = std::fs::rename(&tmp_path, &chat_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok(reducer.count())
    }

    /// Reduces ACP session updates into conversation items.
    ///
    /// Turn boundaries: User→Agent flushes user, Agent→User flushes agent,
    /// tool completion flushes agent before emitting result.
    struct ChatReducer {
        user_parts: Vec<ContentPart>,
        agent_text: String,
        agent_tool_calls: Vec<ToolCall>,

        in_user_turn: bool,
        has_agent_content: bool,
        needs_truncate: bool,

        tool_args: HashMap<String, String>,
        emitted_tool_results: HashSet<String>,
        item_count: usize,
    }

    impl ChatReducer {
        fn new() -> Self {
            Self {
                user_parts: Vec::new(),
                agent_text: String::new(),
                agent_tool_calls: Vec::new(),
                in_user_turn: false,
                has_agent_content: false,
                needs_truncate: false,
                tool_args: HashMap::new(),
                emitted_tool_results: HashSet::new(),
                item_count: 0,
            }
        }

        fn process(&mut self, update: &SessionUpdate) -> Vec<ConversationItem> {
            match update {
                SessionUpdate::Acp(n) => self.handle_acp(&n.update),
                SessionUpdate::Xai(n) => self.handle_xai(&n.update),
            }
        }

        fn handle_acp(&mut self, update: &acp::SessionUpdate) -> Vec<ConversationItem> {
            match update {
                acp::SessionUpdate::UserMessageChunk(chunk) => self.on_user_chunk(chunk),
                acp::SessionUpdate::AgentMessageChunk(chunk) => self.on_agent_chunk(chunk),
                acp::SessionUpdate::ToolCall(tc) => self.on_tool_call(tc),
                acp::SessionUpdate::ToolCallUpdate(tc) => self.on_tool_call_update(tc),
                _ => Vec::new(), // AgentThoughtChunk, Retry, Plan not needed
            }
        }

        fn handle_xai(
            &mut self,
            update: &crate::extensions::notification::SessionUpdate,
        ) -> Vec<ConversationItem> {
            use crate::extensions::notification::SessionUpdate as XaiUpdate;

            match update {
                XaiUpdate::CompactionCheckpoint(_) => {
                    self.reset();
                    self.needs_truncate = true;
                    Vec::new()
                }
                _ => Vec::new(), // DiffReview, MemoryFlush, etc. not needed
            }
        }

        fn on_user_chunk(&mut self, chunk: &acp::ContentChunk) -> Vec<ConversationItem> {
            let mut out = Vec::new();

            if !self.in_user_turn {
                out.extend(self.flush_agent());
                self.in_user_turn = true;
            }

            match &chunk.content {
                acp::ContentBlock::Text(t) => {
                    self.user_parts.push(ContentPart::Text {
                        text: std::sync::Arc::<str>::from(t.text.clone()),
                    });
                }
                acp::ContentBlock::Image(img) => {
                    if let Some(uri) = &img.uri {
                        self.user_parts.push(ContentPart::Image {
                            url: std::sync::Arc::<str>::from(uri.clone()),
                        });
                    }
                }
                _ => {} // Audio, Resource, etc. not needed for chat replay
            }

            out
        }

        fn on_agent_chunk(&mut self, chunk: &acp::ContentChunk) -> Vec<ConversationItem> {
            let mut out = Vec::new();

            if self.in_user_turn {
                out.extend(self.flush_user());
                self.in_user_turn = false;
            }

            if let acp::ContentBlock::Text(t) = &chunk.content {
                self.agent_text.push_str(&t.text);
                self.has_agent_content = true;
            }

            out
        }

        fn on_tool_call(&mut self, tc: &acp::ToolCall) -> Vec<ConversationItem> {
            let id = tc.tool_call_id.0.to_string();
            let args = tc
                .raw_input
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();

            self.tool_args.insert(id.clone(), args.clone());
            self.agent_tool_calls.push(ToolCall {
                id: std::sync::Arc::<str>::from(id),
                name: tc.title.clone(),
                arguments: std::sync::Arc::<str>::from(args),
            });

            Vec::new()
        }

        fn on_tool_call_update(&mut self, tc: &acp::ToolCallUpdate) -> Vec<ConversationItem> {
            let id = tc.tool_call_id.0.to_string();
            self.maybe_backfill_args(&id, &tc.fields);

            if Self::is_completed(&tc.fields) && self.emitted_tool_results.insert(id.clone()) {
                return self.emit_tool_result(&id, &tc.fields);
            }
            Vec::new()
        }

        /// Backfill tool arguments from ToolCallUpdate if ToolCall didn't have them.
        fn maybe_backfill_args(&mut self, id: &str, fields: &acp::ToolCallUpdateFields) {
            let Some(raw) = &fields.raw_input else { return };
            let needs_backfill = self.tool_args.get(id).is_none_or(String::is_empty);
            if !needs_backfill {
                return;
            }

            let args = raw.to_string();
            self.tool_args.insert(id.to_string(), args.clone());

            if let Some(call) = self
                .agent_tool_calls
                .iter_mut()
                .find(|c| c.id.as_ref() == id)
            {
                call.arguments = std::sync::Arc::<str>::from(args);
            }
        }

        fn is_completed(fields: &acp::ToolCallUpdateFields) -> bool {
            matches!(
                fields.status,
                Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed)
            )
        }

        fn emit_tool_result(
            &mut self,
            id: &str,
            fields: &acp::ToolCallUpdateFields,
        ) -> Vec<ConversationItem> {
            let mut out = Vec::new();
            out.extend(self.flush_agent());

            let content = extract_tool_result_text(fields);
            let item = ConversationItem::tool_result(id.to_string(), content);
            self.item_count += 1;
            out.push(item);
            out
        }

        fn flush_user(&mut self) -> Option<ConversationItem> {
            if self.user_parts.is_empty() {
                return None;
            }
            let item = ConversationItem::user_with_parts(std::mem::take(&mut self.user_parts));
            self.item_count += 1;
            Some(item)
        }

        fn flush_agent(&mut self) -> Option<ConversationItem> {
            if !self.has_agent_content && self.agent_tool_calls.is_empty() {
                return None;
            }
            let item = ConversationItem::Assistant(AssistantItem {
                content: std::sync::Arc::<str>::from(std::mem::take(&mut self.agent_text)),
                tool_calls: std::mem::take(&mut self.agent_tool_calls),
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            });
            self.has_agent_content = false;
            self.item_count += 1;
            Some(item)
        }

        fn flush(&mut self) -> Vec<ConversationItem> {
            let mut out = Vec::new();
            out.extend(self.flush_user());
            out.extend(self.flush_agent());
            out
        }

        fn reset(&mut self) {
            self.user_parts.clear();
            self.agent_text.clear();
            self.agent_tool_calls.clear();
            self.tool_args.clear();
            self.emitted_tool_results.clear();
            self.in_user_turn = false;
            self.has_agent_content = false;
            self.item_count = 0;
        }

        fn should_truncate(&self) -> bool {
            self.needs_truncate
        }

        fn clear_truncate_flag(&mut self) {
            self.needs_truncate = false;
        }

        fn count(&self) -> usize {
            self.item_count
        }
    }

    /// Extract displayable text from a completed ToolCallUpdate.
    fn extract_tool_result_text(fields: &acp::ToolCallUpdateFields) -> String {
        if let Some(content) = &fields.content {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    acp::ToolCallContent::Content(acp::Content {
                        content: acp::ContentBlock::Text(t),
                        ..
                    }) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                return text;
            }
        }
        if let Some(raw) = &fields.raw_output {
            return raw.to_string();
        }
        String::new()
    }
}

/// Iterator that streams session updates from a JSONL file without loading all into memory.
/// Each call to `next()` reads and parses one line.
pub struct UpdatesIterator {
    reader: BufReader<std::fs::File>,
    line_buffer: String,
}

impl UpdatesIterator {
    /// Create a new iterator over updates in the given file.
    /// Returns None if the file doesn't exist.
    pub fn open(path: &Path) -> io::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = std::fs::File::open(path)?;
        Ok(Some(Self {
            reader: BufReader::new(file),
            line_buffer: String::new(),
        }))
    }

    /// Create a new iterator starting at the given byte offset.
    /// Returns None if the file doesn't exist.
    /// Used for delta replay: read only updates appended after a known offset.
    pub fn open_at(path: &Path, offset: u64) -> io::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = std::fs::File::open(path)?;
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(offset))?;
        Ok(Some(Self {
            reader,
            line_buffer: String::new(),
        }))
    }

    /// Returns the current byte position in the underlying file.
    /// After iterating, this is the offset of the next unread byte (i.e., EOF
    /// if all updates were consumed). Used to record the replay end offset for
    /// subsequent delta replay.
    pub fn stream_position(&mut self) -> io::Result<u64> {
        self.reader.stream_position()
    }
}

impl Iterator for UpdatesIterator {
    type Item = io::Result<SessionUpdate>;

    fn next(&mut self) -> Option<Self::Item> {
        self.line_buffer.clear();
        match self.reader.read_line(&mut self.line_buffer) {
            Ok(0) => None, // EOF
            Ok(_) => {
                let line = self.line_buffer.trim();
                if line.is_empty() {
                    return self.next();
                }
                match SessionUpdateEnvelope::from_str(line) {
                    Ok(update) => Some(Ok(update)),
                    Err(e) => Some(Err(io::Error::new(io::ErrorKind::InvalidData, e))),
                }
            }
            Err(e) => Some(Err(e)),
        }
    }
}

/// Method name for standard ACP session/update notifications.
const ACP_SESSION_UPDATE_METHOD: &str = "session/update";

/// Method name for xAI extension session/update notifications.
pub(crate) const XAI_SESSION_UPDATE_METHOD: &str = "_x.ai/session/update";

/// A unified session update that can be either an ACP notification or an xAI extension notification.
/// This allows storing all session updates in chronological order.
///
/// Note: The `Serialize` implementation produces a format without timestamp (for GCS uploads, etc.).
/// For disk storage with timestamps, use `SessionUpdateEnvelope` via the JSONL adapter methods.
#[derive(Debug, Clone)]
pub enum SessionUpdate {
    /// Standard ACP session/update notification (boxed due to large size)
    Acp(Box<acp::SessionNotification>),
    /// xAI extension session notification (e.g., diff_review)
    Xai(Box<SessionNotification>),
}

impl serde::Serialize for SessionUpdate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(2))?;
        match self {
            SessionUpdate::Acp(notification) => {
                map.serialize_entry("method", ACP_SESSION_UPDATE_METHOD)?;
                map.serialize_entry("params", notification)?;
            }
            SessionUpdate::Xai(notification) => {
                map.serialize_entry("method", XAI_SESSION_UPDATE_METHOD)?;
                map.serialize_entry("params", notification)?;
            }
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for SessionUpdate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize to a JSON value first to handle both envelope and legacy formats
        let value = serde_json::Value::deserialize(deserializer)?;
        SessionUpdateEnvelope::from_value(value).map_err(serde::de::Error::custom)
    }
}

/// The serialized envelope for a session update, including metadata for debugging.
/// This is the typed structure that gets written to updates.jsonl (disk storage only).
///
/// Note: This is separate from `SessionUpdate`'s own serialization to avoid affecting
/// other consumers (e.g., network listeners) who don't need the timestamp metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionUpdateEnvelope {
    /// Unix timestamp (seconds since epoch) when this update was written.
    /// Useful for debugging timing issues in the updates.jsonl file.
    #[serde(default)]
    pub timestamp: u64,
    /// The method name identifying the update type.
    /// Either "session/update" for ACP or "_x.ai/session/update" for xAI extensions.
    pub method: String,
    /// The actual notification payload.
    pub params: serde_json::Value,
}

impl SessionUpdateEnvelope {
    /// Create a new envelope with the current timestamp for disk storage.
    pub(crate) fn from_update(update: &SessionUpdate) -> Result<Self, serde_json::Error> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        match update {
            SessionUpdate::Acp(notification) => Ok(Self {
                timestamp,
                method: ACP_SESSION_UPDATE_METHOD.to_string(),
                params: serde_json::to_value(notification)?,
            }),
            SessionUpdate::Xai(notification) => Ok(Self {
                timestamp,
                method: XAI_SESSION_UPDATE_METHOD.to_string(),
                params: serde_json::to_value(notification)?,
            }),
        }
    }

    /// Convert this envelope back into a SessionUpdate.
    pub(crate) fn into_update(self) -> Result<SessionUpdate, serde_json::Error> {
        if self.method == XAI_SESSION_UPDATE_METHOD {
            let notification: SessionNotification = serde_json::from_value(self.params)?;
            Ok(SessionUpdate::Xai(Box::new(notification)))
        } else {
            // ACP notification (method == "session/update" or unknown)
            let notification: acp::SessionNotification = serde_json::from_value(self.params)?;
            Ok(SessionUpdate::Acp(Box::new(notification)))
        }
    }

    /// Try to parse from a JSON value, handling both envelope format and legacy raw format.
    pub(crate) fn from_value(value: serde_json::Value) -> Result<SessionUpdate, serde_json::Error> {
        // Check if this looks like an envelope (has "method" field)
        if value.get("method").is_some() {
            let envelope: SessionUpdateEnvelope = serde_json::from_value(value)?;
            envelope.into_update()
        } else {
            // Backwards compatibility: old format without envelope wrapper
            // Treat as raw ACP notification
            let notification: acp::SessionNotification = serde_json::from_value(value)?;
            Ok(SessionUpdate::Acp(Box::new(notification)))
        }
    }

    /// Parse a session update directly from a JSON string, avoiding intermediate `Value` allocation.
    ///
    /// Uses a borrowing envelope with `&RawValue` for the params field so the JSON bytes
    /// for the notification payload are only parsed once (directly to the typed struct)
    /// instead of twice (str -> Value -> typed).
    pub(crate) fn from_str(line: &str) -> Result<SessionUpdate, serde_json::Error> {
        #[derive(serde::Deserialize)]
        struct BorrowedEnvelope<'a> {
            #[serde(default)]
            method: Option<&'a str>,
            #[serde(borrow)]
            params: &'a serde_json::value::RawValue,
        }

        // Try to parse as envelope first (has "method" + "params")
        if let Ok(envelope) = serde_json::from_str::<BorrowedEnvelope<'_>>(line) {
            let raw_params = envelope.params.get();
            return if envelope.method == Some(XAI_SESSION_UPDATE_METHOD) {
                let notification: SessionNotification = serde_json::from_str(raw_params)?;
                Ok(SessionUpdate::Xai(Box::new(notification)))
            } else {
                let notification: acp::SessionNotification = serde_json::from_str(raw_params)?;
                Ok(SessionUpdate::Acp(Box::new(notification)))
            };
        }

        // Backwards compatibility: legacy format without envelope
        let notification: acp::SessionNotification = serde_json::from_str(line)?;
        Ok(SessionUpdate::Acp(Box::new(notification)))
    }
}

/// All persisted data for a session
#[derive(Debug, Clone)]
pub struct PersistedData {
    pub summary: Summary,
    pub chat_history: Vec<ConversationItem>,
    /// All session updates (ACP updates and xAI extension updates) in chronological order
    pub updates: Vec<SessionUpdate>,
    pub plan_state: Option<TodoState>,
    /// Persisted plan mode lifecycle state (None for sessions created before plan mode)
    pub plan_mode_state: Option<crate::session::plan_mode::PlanModeSnapshot>,
    /// Rewind points for session rewind functionality
    pub rewind_points: Vec<RewindPoint>,
    /// Persisted session signals (None for sessions created before signals persistence)
    pub signals: Option<SessionSignals>,
    /// Persisted announcement tracking state (None for sessions before this feature)
    pub announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    /// Persisted goal mode orchestration state (None for sessions without goal mode)
    pub goal_mode_state: Option<crate::session::goal_tracker::GoalOrchestration>,
}

/// Persisted data WITHOUT updates - for memory-efficient session loading
#[derive(Debug, Clone)]
pub struct PersistedDataLight {
    pub summary: Summary,
    pub chat_history: Vec<ConversationItem>,
    pub plan_state: Option<TodoState>,
    pub plan_mode_state: Option<crate::session::plan_mode::PlanModeSnapshot>,
    // No `rewind_points` field: the resume path defers them (loaded lazily by
    // `FileStateTracker`). Use `load_session` for the eager set.
    /// Persisted session signals (None for sessions created before signals persistence)
    pub signals: Option<SessionSignals>,
    /// Persisted announcement tracking state (None for sessions before this feature)
    pub announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    /// Persisted goal mode orchestration state (None for sessions without goal mode)
    pub goal_mode_state: Option<crate::session::goal_tracker::GoalOrchestration>,
}

/// Result of copying session data
#[derive(Debug, Clone)]
pub struct CopySessionResult {
    pub chat_messages_copied: usize,
    pub updates_copied: usize,
    pub plan_state_copied: bool,
    /// Whether `plan_mode.json` (plan mode lifecycle state) was copied.
    pub plan_mode_state_copied: bool,
    pub signals_copied: bool,
    /// Whether `tool_state.json` (persisted tool state, e.g. TodoState) was copied.
    pub tool_state_copied: bool,
    /// Whether `announcement_state.json` was copied.
    pub announcement_state_copied: bool,
    /// Number of `compaction/segment_*.md` (+ `INDEX.md`) files copied from the
    /// source session's compaction archive. `0` when disabled or none exist.
    pub compaction_segments_copied: usize,
}

/// Options for copying session data during fork
#[derive(Debug, Clone)]
pub struct CopySessionOptions {
    /// Parent session ID to set in the forked session's summary.
    pub parent_session_id: Option<String>,
    /// Model ID override for the forked session (None = keep source model).
    pub new_model_id: Option<String>,
    /// Truncate copied history to this prompt index (0-based, inclusive).
    pub target_prompt_index: Option<usize>,
    /// When true, skip `transform_conversation_cwd` during copy.
    ///
    /// Set for forks where the child should see the original project path
    /// (e.g. worktree forks with a persisted `display_cwd`). Non-worktree
    /// forks should keep this false so conversation paths are rewritten to
    /// the new cwd.
    pub skip_cwd_transform: bool,
    /// Stable display path for fork sessions. Persisted in the forked
    /// summary so the prompt-facing cwd survives session restore/reload.
    pub prompt_display_cwd: Option<String>,

    // ── Generic fork extensions (used by subagent + worktree forks) ──
    /// Override `session_kind` in the forked summary. Defaults to `"fork"`.
    /// Subagent resume sets `"subagent_resume"`.
    pub session_kind: Option<String>,
    /// How the fork's initial context was bootstrapped: `"new"` or `"forked"`.
    pub fork_context_source: Option<String>,
    /// Parent prompt/turn ID that triggered this fork.
    pub fork_parent_prompt_id: Option<String>,
    /// Whether to copy the plan state file. Defaults to `true`.
    pub copy_plan_state: bool,
    /// Whether to copy the plan mode state file. Defaults to `true`.
    pub copy_plan_mode_state: bool,
    /// Whether to copy the signals file. Defaults to `true`.
    pub copy_signals: bool,
    /// Whether to copy `tool_state.json` (persisted tool state). Defaults to `true`.
    pub copy_tool_state: bool,
    /// Whether to copy `announcement_state.json`. Defaults to `true`.
    pub copy_announcement_state: bool,
    /// Whether to copy the `compaction/` segment archive (`segment_*.md` +
    /// `INDEX.md`, the verbose pre-compaction transcripts). Defaults to
    /// `false` — these can be large and most copy paths don't need them. Forks
    /// enable it so the child retains the parent's pre-compaction history.
    pub copy_compaction_segments: bool,
    /// When true, apply fork-safety filtering to copied chat history:
    /// - Strip synthetic user messages (doom loop warnings, compaction metadata)
    /// - Truncate at the last complete turn boundary
    /// - Remove trailing incomplete assistant responses
    pub fork_filter: bool,
    /// Number of inherited parent conversation items. Stored in the child's
    /// summary so compaction can preserve the inherited prefix.
    pub inherited_prefix_len: Option<usize>,
    /// When true, strip `reasoning` (thinking/reasoning_content) from all
    /// assistant messages in the copied chat history.
    ///
    /// Set for forks so that the new session does not inherit the prior
    /// model's chain-of-thought -- each fork starts with a clean slate
    /// for reasoning on the new prompt.
    pub strip_reasoning: bool,
    /// The original workspace directory this worktree session was spawned from.
    /// Propagated to the forked session's `Summary::source_workspace_dir`.
    pub source_workspace_dir: Option<String>,
}

impl Default for CopySessionOptions {
    fn default() -> Self {
        Self {
            parent_session_id: None,
            new_model_id: None,
            target_prompt_index: None,
            skip_cwd_transform: false,
            prompt_display_cwd: None,
            session_kind: None,
            fork_context_source: None,
            fork_parent_prompt_id: None,
            copy_plan_state: true,
            copy_plan_mode_state: true,
            copy_signals: true,
            copy_tool_state: true,
            copy_announcement_state: true,
            copy_compaction_segments: false,
            fork_filter: false,
            inherited_prefix_len: None,
            strip_reasoning: false,
            source_workspace_dir: None,
        }
    }
}

/// Chunk `_meta.promptIndex` on an ACP `UserMessageChunk`, if present.
fn acp_user_chunk_prompt_index(update: &SessionUpdate) -> Option<usize> {
    let SessionUpdate::Acp(n) = update else {
        return None;
    };
    let acp::SessionUpdate::UserMessageChunk(chunk) = &n.update else {
        return None;
    };
    chunk
        .meta
        .as_ref()
        .and_then(|m| m.get("promptIndex"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
}

fn is_acp_user_message_chunk(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::Acp(n) if matches!(n.update, acp::SessionUpdate::UserMessageChunk(_))
    )
}

/// Tracks user-message runs for turn counting (updates truncate / filter_rewind).
///
/// Progressive: every user run counts until the first `promptIndex` appears;
/// after that only marked runs count (mid-turn phantoms omit the marker).
/// A change of `promptIndex` (including unmarked ↔ marked) opens a new run —
/// matching replay's split so back-to-back cancelled prompts stay distinct.
struct UserRunTurnTracker {
    seen_marker: bool,
    in_user: bool,
    /// `promptIndex` of the current user run (`None` = unmarked / phantom run).
    current_run_pi: Option<usize>,
}

impl UserRunTurnTracker {
    fn new() -> Self {
        Self {
            seen_marker: false,
            in_user: false,
            current_run_pi: None,
        }
    }

    /// Returns true if this user chunk opens a **counted** turn.
    fn on_user_chunk(&mut self, prompt_index: Option<usize>) -> bool {
        if prompt_index.is_some() {
            self.seen_marker = true;
        }
        let counts = if self.seen_marker {
            prompt_index.is_some()
        } else {
            true
        };
        let new_run = if !self.in_user {
            true
        } else if self.seen_marker || prompt_index.is_some() {
            prompt_index != self.current_run_pi
        } else {
            false
        };
        if new_run {
            self.current_run_pi = prompt_index;
            self.in_user = true;
            counts
        } else {
            self.in_user = true;
            false
        }
    }

    fn on_non_user(&mut self) {
        self.in_user = false;
        self.current_run_pi = None;
    }
}

/// Calculate how many updates to keep for a given target prompt index (0-based, inclusive).
///
/// Progressive: unmarked user runs before the first `_meta.promptIndex` count
/// as turns; after the first marker only marked runs count (phantoms omit it).
pub fn updates_truncate_for_prompt(updates: &[SessionUpdate], target_prompt_index: usize) -> usize {
    let mut user_turn_count = 0;
    let mut tracker = UserRunTurnTracker::new();

    for (i, update) in updates.iter().enumerate() {
        if is_acp_user_message_chunk(update) {
            if tracker.on_user_chunk(acp_user_chunk_prompt_index(update)) {
                user_turn_count += 1;
                if user_turn_count > target_prompt_index + 1 {
                    return i;
                }
            }
        } else {
            tracker.on_non_user();
        }
    }

    updates.len()
}

#[derive(Debug)]
pub enum AppendUpdateError {
    NotCommitted(io::Error),
    Committed(io::Error),
}

impl AppendUpdateError {
    pub fn into_io_error(self) -> io::Error {
        match self {
            Self::NotCommitted(error) | Self::Committed(error) => error,
        }
    }
}

impl std::fmt::Display for AppendUpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error) | Self::Committed(error) => error.fmt(formatter),
        }
    }
}

/// Storage adapter trait for session persistence
/// Abstracts over different storage backends (JSONL, SQLite, etc.)
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    /// Initialize a new session or load existing one
    /// Returns the Summary (creates if needed, loads if exists)
    async fn init_session(&self, info: &Info, model_id: acp::ModelId) -> io::Result<Summary>;

    /// Set the session title unconditionally (manual `/rename`); last write
    /// wins. Also marks the title manual (`Summary::title_is_manual`) so
    /// clients restore the prompt-border title on resume.
    async fn update_session_title(&self, info: &Info, session_title: String) -> io::Result<()>;

    /// Set the session title only if the session has no title yet, used by
    /// automatic LLM title generation so it never overwrites a manual
    /// `/rename`. Never marks the title manual. Returns `true` if the title
    /// was written, `false` if an existing title was preserved. The check and
    /// write are atomic under the summary lock, so a concurrent manual rename
    /// always wins.
    async fn set_generated_title_if_absent(
        &self,
        info: &Info,
        session_title: String,
    ) -> io::Result<bool>;

    /// Append a session update (ACP update or xAI extension update) and increment counter
    async fn append_update(&self, info: &Info, update: &SessionUpdate) -> io::Result<()>;

    /// Append one update and report whether the replay record was committed before an error.
    async fn append_update_commit_aware(
        &self,
        info: &Info,
        update: &SessionUpdate,
    ) -> Result<(), AppendUpdateError> {
        self.append_update(info, update)
            .await
            .map_err(AppendUpdateError::NotCommitted)
    }

    /// Append one update durably, preserving whether the replay record committed before failure.
    async fn append_update_durable_commit_aware(
        &self,
        _info: &Info,
        _update: &SessionUpdate,
    ) -> Result<(), AppendUpdateError> {
        Err(AppendUpdateError::NotCommitted(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable session update append is unsupported",
        )))
    }

    /// Append a chat message and increment counter
    async fn append_chat_message(&self, info: &Info, message: &ConversationItem) -> io::Result<()>;

    /// Update the current model in summary (delegates to
    /// `update_current_model_and_agent` with `agent_name = None`).
    async fn update_current_model(&self, info: &Info, model_id: &acp::ModelId) -> io::Result<()> {
        self.update_current_model_and_agent(info, model_id, None, None)
            .await
    }

    /// Update the current model and agent name in summary.
    /// `agent_name` is the resolved agent definition name
    /// persisted so session resume doesn't depend on the mutable model catalog.
    /// `None` leaves the existing `agent_name` unchanged (used by legacy callers
    /// that only update the model ID).
    async fn update_current_model_and_agent(
        &self,
        info: &Info,
        model_id: &acp::ModelId,
        agent_name: Option<&str>,
        reasoning_effort: Option<Option<ReasoningEffort>>,
    ) -> io::Result<()>;

    /// Update the collection ID for telemetry tracing
    async fn update_collection_id(&self, info: &Info, collection_id: &str) -> io::Result<()>;

    /// Update the persisted HEAD commit and branch in summary
    async fn update_git_head(
        &self,
        info: &Info,
        commit: Option<String>,
        branch: Option<String>,
    ) -> io::Result<()>;

    /// Update the monotonic telemetry trace turn counter ("next turn" value).
    async fn update_next_trace_turn(
        &self,
        info: &Info,
        next_trace_turn: u64,
        request_id: Option<&str>,
    ) -> io::Result<()>;

    /// Write/update the plan state
    async fn write_plan_state(&self, info: &Info, state: &TodoState) -> io::Result<()>;

    /// Write/update plan mode lifecycle state
    async fn write_plan_mode_state(
        &self,
        info: &Info,
        state: &crate::session::plan_mode::PlanModeSnapshot,
    ) -> io::Result<()>;

    /// Write/update the session signals snapshot
    async fn write_signals(&self, info: &Info, signals: &SessionSignals) -> io::Result<()>;

    /// Write/update the announcement tracking state
    async fn write_announcement_state(
        &self,
        info: &Info,
        state: &crate::session::announcement_state::AnnouncementState,
    ) -> io::Result<()>;

    /// Write/update the goal mode orchestration state
    async fn write_goal_mode_state(
        &self,
        info: &Info,
        state: &crate::session::goal_tracker::GoalOrchestration,
    ) -> io::Result<()>;

    /// Load all persisted data for a session
    async fn load_session(&self, info: &Info) -> io::Result<PersistedData>;

    /// Load session data WITHOUT updates (for memory efficiency when updates
    /// will be streamed). Implementations also do NOT read rewind points here;
    /// those are deferred and lazily loaded on demand from the path returned by
    /// [`rewind_points_file_path`](StorageAdapter::rewind_points_file_path).
    async fn load_session_without_updates(&self, info: &Info) -> io::Result<PersistedDataLight>;

    /// Loads the summary of the session
    async fn load_summary(&self, info: &Info) -> io::Result<Summary>;

    /// List session summaries, optionally filtered by current working directory.
    /// When `cwd` is `None`, returns summaries for all sessions.
    async fn list_sessions(&self, cwd: Option<&str>) -> io::Result<Vec<Summary>>;

    /// Permanently delete a session's stored data (all files for the
    /// session). Implementations must treat a missing session as success
    /// (idempotent delete).
    async fn delete_session(&self, info: &Info) -> io::Result<()>;

    /// Append a rewind point for session rewind functionality
    async fn append_rewind_point(&self, info: &Info, point: &RewindPoint) -> io::Result<()>;

    /// Load all rewind points for a session
    async fn load_rewind_points(&self, info: &Info) -> io::Result<Vec<RewindPoint>>;

    /// Sync all session files to disk. Called before CopyFile to ensure all writes are persisted.
    async fn sync_session_files(&self, info: &Info) -> io::Result<()>;

    /// Truncate rewind points from a specific prompt index (inclusive)
    /// Used when rewinding to remove future history
    async fn truncate_rewind_points_from(&self, info: &Info, from_index: usize) -> io::Result<()>;

    /// Merge rewind points at indices `>= target_index` into the point at
    /// `target_index - 1` and drop the folded points, as a read-modify-write on
    /// disk (used after a ConversationOnly rewind). Reading the current on-disk
    /// set makes this authoritative: it never relies on a (possibly partially
    /// loaded) in-memory tracker, so historical points can't be lost.
    async fn merge_rewind_points_from(&self, info: &Info, target_index: usize) -> io::Result<()>;

    /// Replace the entire chat history (used for compaction and rewind)
    async fn replace_chat_history(
        &self,
        info: &Info,
        messages: &[ConversationItem],
    ) -> io::Result<()>;

    /// Copy session data from source to target, transforming session IDs
    /// The `options` parameter allows setting parent session tracking and model overrides.
    async fn copy_session_data(
        &self,
        source_info: &Info,
        target_info: &Info,
        options: CopySessionOptions,
    ) -> io::Result<CopySessionResult>;

    /// Load only user prompts from a session's updates file.
    /// This is an optimized method that avoids loading chat_history, plan_state, etc.
    /// Returns user prompts in chronological order.
    async fn load_prompts_only(&self, info: &Info) -> io::Result<Vec<String>>;
    /// Load assistant text content from a session's updates file.
    /// Returns assistant responses in chronological order, extracted from ContentChunk text.
    async fn load_assistant_text(&self, info: &Info) -> io::Result<Vec<String>>;

    /// Load tool metadata from a session's updates file.
    /// Per Phase 1 contract (ACP data model):
    /// - Tool name: from `ToolCall.title` (display name; acp::ToolCall has no .name field)
    /// - File paths: from `ToolCall.locations[].path` (ACP stores locations, not parsed arguments)
    /// - Errors: skipped (no is_error field on acp::SessionUpdate::ToolCallUpdate)
    async fn load_tool_metadata(&self, info: &Info) -> io::Result<Vec<String>>;

    /// Get the path to the updates file for streaming reads.
    /// Returns None if the storage backend doesn't support streaming.
    fn updates_file_path(&self, info: &Info) -> Option<std::path::PathBuf>;

    /// Path to the rewind-points file for lazy/deferred loading, or None if the
    /// backend doesn't persist them to a streamable file. The adapter owns the
    /// on-disk layout, so callers must use this rather than recomputing the path
    /// (it differs for non-default storage modes, e.g. subagent/fork sessions).
    fn rewind_points_file_path(&self, info: &Info) -> Option<std::path::PathBuf>;

    /// Append a feedback entry (user feedback) to feedback.jsonl
    async fn append_feedback(
        &self,
        info: &Info,
        entry: &crate::session::persistence::LocalFeedbackEntry,
    ) -> io::Result<()>;

    /// Append a /btw side question entry to btw_history.jsonl
    async fn append_btw(
        &self,
        info: &Info,
        entry: &crate::session::persistence::BtwEntry,
    ) -> io::Result<()>;

    /// Write a compaction checkpoint file to `compaction_checkpoints/{checkpoint_id}.json`.
    async fn write_compaction_checkpoint(
        &self,
        info: &Info,
        checkpoint: &crate::extensions::notification::CompactionCheckpointFile,
    ) -> io::Result<()>;

    /// Write a compaction request artifact to `compaction_requests/{request_id}.json`.
    /// Captures the exact request sent to the compaction model and the response
    /// (or final error) it produced. Used for offline prompt iteration.
    async fn write_compaction_request(
        &self,
        info: &Info,
        request: &crate::extensions::notification::CompactionRequestFile,
    ) -> io::Result<()>;

    /// Write a recap request artifact to `recap_requests/{request_id}.json`.
    /// Captures the exact request sent for `/recap` or auto recap and the
    /// response (or error). Used for offline recap prompt / garble analysis.
    async fn write_recap_request(
        &self,
        info: &Info,
        request: &crate::extensions::notification::RecapRequestFile,
    ) -> io::Result<()>;

    /// Render+write `compaction/segment_NNN.md` (storage assigns the resume-safe
    /// index) and append its `INDEX.md` row.
    async fn write_compaction_segment(
        &self,
        info: &Info,
        segment: &crate::extensions::notification::CompactionSegmentFile,
    ) -> io::Result<()>;

    /// Read a compaction checkpoint file by its relative path within the session directory.
    async fn read_compaction_checkpoint(
        &self,
        info: &Info,
        checkpoint_file: &str,
    ) -> io::Result<crate::extensions::notification::CompactionCheckpointFile>;
}

pub use jsonl::JsonlStorageAdapter;

/// Extracts `method` and raw `params` from an updates.jsonl envelope
/// without parsing the notification payload.
#[derive(serde::Deserialize)]
pub(crate) struct RawLinePeek<'a> {
    #[serde(default)]
    pub method: Option<&'a str>,
    #[serde(borrow, default)]
    pub params: Option<&'a serde_json::value::RawValue>,
}

/// Peeks at `update.sessionUpdate` tag and `_meta` without full deserialization.
#[derive(serde::Deserialize)]
pub(crate) struct RawParamsPeek<'a> {
    #[serde(borrow, default)]
    pub update: Option<RawUpdatePeek<'a>>,
    #[serde(borrow, default, rename = "_meta")]
    pub meta: Option<&'a serde_json::value::RawValue>,
}

#[derive(serde::Deserialize)]
pub(crate) struct RawUpdatePeek<'a> {
    #[serde(rename = "sessionUpdate")]
    pub session_update: &'a str,
    #[serde(default)]
    pub target_prompt_index: Option<usize>,
    /// Chunk `_meta.promptIndex` when present (owned; not borrowed).
    #[serde(default, rename = "_meta")]
    pub meta: Option<RawChunkMetaPeek>,
}

#[derive(serde::Deserialize)]
pub(crate) struct RawChunkMetaPeek {
    #[serde(default, rename = "promptIndex")]
    pub prompt_index: Option<u64>,
}

/// Filter rewind dead branches from raw JSONL lines.
/// Skips parsing entirely when no rewind markers are present.
///
/// This is the canonical implementation of rewind dead-branch filtering,
/// used by both the initial replay and delta replay paths.
pub(crate) fn filter_rewind_lines<'a>(lines: Vec<&'a str>) -> Vec<&'a str> {
    let has_rewinds = lines.iter().any(|l| l.contains(&*REWIND_MARKER));
    if !has_rewinds {
        return lines;
    }

    let mut result: Vec<&str> = Vec::with_capacity(lines.len());
    let mut prompt_starts: Vec<usize> = Vec::new();
    let mut tracker = UserRunTurnTracker::new();

    for line in &lines {
        let (raw_params, is_xai) = if let Ok(env) = serde_json::from_str::<RawLinePeek<'_>>(line) {
            let raw = env.params.map(|p| p.get()).unwrap_or(line);
            let xai = env.method == Some(XAI_SESSION_UPDATE_METHOD);
            (raw, xai)
        } else {
            (*line, false)
        };

        let peek = serde_json::from_str::<RawParamsPeek<'_>>(raw_params)
            .ok()
            .and_then(|p| p.update);
        let tag = peek
            .as_ref()
            .map(|u| (u.session_update, u.target_prompt_index));

        if is_xai
            && let Some((s, Some(target))) = tag.as_ref().map(|(s, t)| (*s, *t))
            && s == *REWIND_MARKER
        {
            let trunc = prompt_starts.get(target).copied().unwrap_or(result.len());
            result.truncate(trunc);
            prompt_starts.truncate(target);
            tracker.on_non_user();
            continue;
        }

        let is_user_chunk = !is_xai
            && tag
                .as_ref()
                .map(|(s, _)| *s == *USER_MESSAGE_CHUNK)
                .unwrap_or(false);
        if is_user_chunk {
            let pi = peek.as_ref().and_then(|u| {
                u.meta
                    .as_ref()
                    .and_then(|m| m.prompt_index.map(|v| v as usize))
            });
            if tracker.on_user_chunk(pi) {
                prompt_starts.push(result.len());
            }
        } else {
            tracker.on_non_user();
        }
        result.push(line);
    }
    result
}

/// Filter rewind dead branches from typed `SessionUpdate` values.
///
/// This is the typed equivalent of [`filter_rewind_lines`] — same algorithm
/// (prompt-boundary tracking + truncation on `RewindMarker`) but operates on
/// fully-deserialized updates instead of raw JSON strings.
pub fn filter_rewind_updates(updates: Vec<SessionUpdate>) -> Vec<SessionUpdate> {
    let has_rewinds = updates.iter().any(|u| {
        matches!(
            u,
            SessionUpdate::Xai(n) if matches!(
                n.update,
                crate::extensions::notification::SessionUpdate::RewindMarker { .. }
            )
        )
    });
    if !has_rewinds {
        return updates;
    }

    let mut result: Vec<SessionUpdate> = Vec::with_capacity(updates.len());
    let mut prompt_starts: Vec<usize> = Vec::new();
    let mut tracker = UserRunTurnTracker::new();

    for update in updates {
        // Check for rewind marker — truncate back to the target prompt.
        if let SessionUpdate::Xai(ref n) = update
            && let crate::extensions::notification::SessionUpdate::RewindMarker {
                target_prompt_index,
                ..
            } = &n.update
        {
            let trunc = prompt_starts
                .get(*target_prompt_index)
                .copied()
                .unwrap_or(result.len());
            result.truncate(trunc);
            prompt_starts.truncate(*target_prompt_index);
            tracker.on_non_user();
            continue;
        }

        if is_acp_user_message_chunk(&update) {
            if tracker.on_user_chunk(acp_user_chunk_prompt_index(&update)) {
                prompt_starts.push(result.len());
            }
        } else {
            tracker.on_non_user();
        }
        result.push(update);
    }
    result
}

/// Strip `<fork-context>` and `<resume-context>` XML wrappers from user
/// message chunks so replayed/exported prompts show clean text.
///
/// Only modifies `UserMessageChunk` text content; all other update types
/// pass through unchanged. The tags are injected by the subagent fork/resume
/// logic in `subagent.rs`.
pub fn strip_context_wrappers(update: acp::SessionUpdate) -> acp::SessionUpdate {
    let acp::SessionUpdate::UserMessageChunk(mut chunk) = update else {
        return update;
    };
    if let acp::ContentBlock::Text(ref mut t) = chunk.content {
        for tag in &["fork-context", "resume-context"] {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            if let Some(start) = t.text.find(&open)
                && let Some(rel_end) = t.text[start + open.len()..].find(&close)
            {
                let end = start + open.len() + rel_end;
                let remove_end = end + close.len();
                t.text = format!("{}{}", &t.text[..start], t.text[remove_end..].trim_start());
            }
        }
    }
    acp::SessionUpdate::UserMessageChunk(chunk)
}

/// Load session updates from disk, ready for replay or export.
///
/// This is the canonical way to get replay-ready typed updates from a session
/// ID. It:
/// 1. Locates the session directory via [`find_session_dir_by_id`]
/// 2. Opens `updates.jsonl` via [`UpdatesIterator`]
/// 3. Collects all parseable updates (skipping malformed lines)
/// 4. Filters rewind dead branches via [`filter_rewind_updates`]
/// 5. Strips `<fork-context>` / `<resume-context>` wrappers from user messages
///    via [`strip_context_wrappers`]
///
/// Returns `None` if the session is not found or has no `updates.jsonl`.
/// Returns only `SessionUpdate::Acp` updates (xAI-extension updates like
/// rewind markers and compaction signals are consumed by the filter and not
/// included in the output).
pub fn load_updates_for_replay(
    session_id: &str,
) -> std::io::Result<Option<Vec<acp::SessionUpdate>>> {
    let Some(session_dir) = crate::session::persistence::find_session_dir_by_id(session_id) else {
        return Ok(None);
    };
    load_updates_for_replay_from_dir(&session_dir)
}

/// Like [`load_updates_for_replay`], but resolves the session under a specific grok home.
pub fn load_updates_for_replay_at(
    session_id: &str,
    grok_home: &std::path::Path,
) -> std::io::Result<Option<Vec<acp::SessionUpdate>>> {
    let sessions_root = grok_home.join("sessions");
    let Some(session_dir) =
        crate::session::persistence::find_session_dir_by_id_in_root(session_id, &sessions_root)
    else {
        return Ok(None);
    };
    load_updates_for_replay_from_dir(&session_dir)
}

fn load_updates_for_replay_from_dir(
    session_dir: &std::path::Path,
) -> std::io::Result<Option<Vec<acp::SessionUpdate>>> {
    let updates_path = session_dir.join(UPDATES_FILE);
    let Some(iter) = UpdatesIterator::open(&updates_path)? else {
        return Ok(None);
    };

    let all: Vec<SessionUpdate> = iter.filter_map(|r| r.ok()).collect();
    let filtered = filter_rewind_updates(all);

    let acp_updates: Vec<acp::SessionUpdate> = filtered
        .into_iter()
        .filter_map(|u| match u {
            SessionUpdate::Acp(notif) => Some(strip_context_wrappers(notif.update)),
            SessionUpdate::Xai(_) => None,
        })
        .collect();

    Ok(Some(acp_updates))
}

pub(crate) struct PreparedReplay<'a> {
    pub lines: Vec<&'a str>,
    pub mark_replay: bool,
    pub last_tokens: u64,
    /// Highest `eventId` counter across all live (rewind-filtered) lines, used
    /// to re-seed the process-global event counter on resume so post-load live
    /// events keep monotonically increasing ids (see
    /// [`crate::util::event_id::ensure_event_counter_at_least`]). `None` when no
    /// line carried a parseable `eventId` (older shell).
    pub max_event_seq: Option<u64>,
    pub total_live: usize,
    /// Replayed spawns with no matching finish (a rewind can drop the finish) —
    /// `(subagent_id, child_session_id)`, reconciled on load.
    pub unfinished_subagents: Vec<(String, String)>,
}

/// Unpaired spawns across the rewind-filtered timeline. Substring pre-filter
/// keeps non-subagent lines off the JSON path.
fn collect_unfinished_subagents(filtered: &[&str]) -> Vec<(String, String)> {
    use crate::extensions::notification::SessionUpdate as Update;
    let mut pending: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for line in filtered {
        if !line.contains("subagent_spawned") && !line.contains("subagent_finished") {
            continue;
        }
        // Parse the typed notification. Envelope lines nest it under `params`;
        // legacy lines put it at the top level (fall back to the whole line,
        // matching `filter_rewind_lines`).
        let raw = serde_json::from_str::<RawLinePeek<'_>>(line)
            .ok()
            .and_then(|e| e.params.map(|p| p.get()))
            .unwrap_or(line);
        let Ok(notification) = serde_json::from_str::<SessionNotification>(raw) else {
            continue;
        };
        match notification.update {
            Update::SubagentSpawned {
                subagent_id,
                child_session_id,
                ..
            } => {
                pending.insert(subagent_id, child_session_id);
            }
            Update::SubagentFinished { subagent_id, .. } => {
                pending.remove(&subagent_id);
            }
            _ => {}
        }
    }
    pending.into_iter().collect()
}

/// The raw `_meta` object of a persisted line, if any, without allocating a
/// `serde_json::Value`. Handles both the enveloped (`{method,params}`) and legacy
/// (params-at-top-level) on-disk formats.
fn line_meta(line: &str) -> Option<&serde_json::value::RawValue> {
    let env = serde_json::from_str::<RawLinePeek<'_>>(line).ok()?;
    let raw = env.params.map(|p| p.get()).unwrap_or(line);
    serde_json::from_str::<RawParamsPeek<'_>>(raw).ok()?.meta
}

/// The `"update":` object key (a protocol key, not an enum discriminant). The
/// structural `params.update` is the FIRST occurrence in a persisted line: the
/// envelope prefix has no `"update":`, and any nested `"update"` (in `_meta` or a
/// tool's `rawInput`/`rawOutput`) is serialized after it, so the first match delimits it.
const UPDATE_KEY: &str = r#""update":"#;

/// Is this persisted line an `available_commands_update`?
///
/// The slash-command catalog is re-advertised in full after every `session/load`,
/// so the historical copies in `updates.jsonl` are redundant on replay and
/// dominate large sessions (~51% of bytes in pathological cases). The lines stay
/// on disk; this only skips forwarding them to the client.
///
/// A cheap [`AVAILABLE_COMMANDS_UPDATE_PREFIX`] substring pre-filter, then a
/// positional confirm that the value at the first [`UPDATE_KEY`] begins with the
/// ACU discriminant. Reads only the prefix (never the huge `availableCommands`
/// array), so it can't be fooled by the discriminant embedded in `_meta` or a
/// tool payload (never the first `"update":`).
pub(crate) fn line_is_available_commands_update(line: &str) -> bool {
    if !line.contains(&*AVAILABLE_COMMANDS_UPDATE_PREFIX) {
        return false;
    }
    line.find(UPDATE_KEY)
        .map(|pos| {
            line[pos + UPDATE_KEY.len()..]
                .trim_start()
                .starts_with(&*AVAILABLE_COMMANDS_UPDATE_PREFIX)
        })
        .unwrap_or(false)
}

// `_meta` protocol field names (not enum discriminants).
/// `_meta` key holding the running token count. The serde `rename` below must
/// match it by hand (serde attrs can't reference a const).
const TOTAL_TOKENS_KEY: &str = "totalTokens";
/// `_meta` key holding the per-event id used for cursor-based reconnect.
const EVENT_ID_KEY: &str = "eventId";

/// Extract `_meta.totalTokens` from a persisted update line without allocating a
/// `serde_json::Value`. Returns `None` when the line carries no token count.
fn line_total_tokens(line: &str) -> Option<u64> {
    if !line.contains(TOTAL_TOKENS_KEY) {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct TokensPeek {
        #[serde(rename = "totalTokens")]
        total_tokens: Option<u64>,
    }
    serde_json::from_str::<TokensPeek>(line_meta(line)?.get())
        .ok()
        .and_then(|t| t.total_tokens)
}

/// This line's `_meta.eventId`, if any. Cheap peek (no `Value`).
fn line_event_id(line: &str) -> Option<std::borrow::Cow<'_, str>> {
    if !line.contains(EVENT_ID_KEY) {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct EventIdPeek<'a> {
        // `Cow` so an escaped eventId still parses and compares equal
        // (`Option<Cow>` always deserializes owned; `&str` would error).
        #[serde(rename = "eventId", borrow)]
        event_id: Option<std::borrow::Cow<'a, str>>,
    }
    serde_json::from_str::<EventIdPeek<'_>>(line_meta(line)?.get())
        .ok()
        .and_then(|e| e.event_id)
}

/// Does this line's `_meta.eventId` equal `cursor_id`?
fn line_has_event_id(line: &str, cursor_id: &str) -> bool {
    line_event_id(line).as_deref() == Some(cursor_id)
}

/// Rewind-filter, resolve the reconnect cursor, drop redundant command catalogs,
/// and scan `totalTokens`. Pure data processing — no gateway, no async.
///
/// The cursor is resolved BEFORE dropping ACUs: ACUs carry `_meta.eventId` and the
/// post-load re-advertise is usually the *last* persisted event, so an idle client
/// commonly reconnects with an ACU's eventId as its cursor. Resolving against the
/// ACU-inclusive set keeps incremental reconnect cheap instead of a full replay.
pub(crate) fn prepare_replay_lines<'a>(
    raw_contents: &'a str,
    cursor: Option<&str>,
) -> PreparedReplay<'a> {
    let filtered = filter_rewind_lines(
        raw_contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect(),
    );

    // Highest `eventId` counter across all live (rewind-filtered) lines, used to
    // re-seed the process-global event counter on resume so post-load live events
    // keep monotonically increasing ids. eventId is "{sessionId}-{counter}" and
    // session ids contain dashes, so the counter is the suffix after the LAST '-'.
    let mut max_event_seq: Option<u64> = None;
    for line in &filtered {
        if line.contains("eventId")
            && let Ok(env) = serde_json::from_str::<RawLinePeek<'_>>(line)
            && let Some(raw) = env.params.map(|p| p.get())
            && let Ok(pp) = serde_json::from_str::<RawParamsPeek<'_>>(raw)
            && let Some(meta_raw) = pp.meta
            && let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_raw.get())
            && let Some(seq) = meta
                .get("eventId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.rsplit('-').next())
                .and_then(|c| c.parse::<u64>().ok())
        {
            max_event_seq = Some(max_event_seq.map_or(seq, |m| m.max(seq)));
        }
    }

    // Last live update carrying `_meta.totalTokens` (reverse scan, last-wins) over
    // the full surviving timeline, so the count reflects total session state.
    // ACUs never carry `totalTokens`, so scanning the ACU-inclusive set is safe.
    let last_tokens = filtered
        .iter()
        .rev()
        .find_map(|l| line_total_tokens(l))
        .unwrap_or(0);

    // Resolve the reconnect cursor against the ACU-inclusive set. `mark_replay`
    // is true for a full historical replay (no cursor, or cursor not found).
    //
    // The cursor is refused when a FORWARDED tail line lacks an `eventId`:
    // such a line cannot be covered by a future cursor and has no client-side
    // dedup, so re-delivering it as live would re-apply it. Full replay is
    // the safe fallback — the client swaps it in wholesale. Id-less lines
    // come from older binaries or any emitter outside the stamping
    // chokepoints (see `ensure_event_id_meta`). ACU lines are exempt: they
    // are dropped below, never forwarded.
    let cursor_pos = cursor
        .and_then(|id| filtered.iter().rposition(|l| line_has_event_id(l, id)))
        .filter(|&pos| {
            let bounded = filtered[pos + 1..]
                .iter()
                .all(|l| line_is_available_commands_update(l) || line_event_id(l).is_some());
            if !bounded {
                tracing::warn!(
                    "replay: post-cursor tail contains eventId-less lines; full replay instead"
                );
            }
            bounded
        });
    let mark_replay = cursor_pos.is_none();
    let start = cursor_pos.map_or(0, |pos| pos + 1);

    // Single pass: drop ACUs (kept on disk), collect the post-cursor tail to
    // forward, and count the full ACU-free live set for the skip log.
    let mut lines: Vec<&str> = Vec::with_capacity(filtered.len().saturating_sub(start));
    let mut total_live = 0usize;
    for (i, &line) in filtered.iter().enumerate() {
        if line_is_available_commands_update(line) {
            continue;
        }
        total_live += 1;
        if i >= start {
            lines.push(line);
        }
    }

    PreparedReplay {
        lines,
        mark_replay,
        last_tokens,
        max_event_seq,
        total_live,
        unfinished_subagents: collect_unfinished_subagents(&filtered),
    }
}

/// Blank-strip, drop redundant command catalogs, and rewind-filter a raw
/// `updates.jsonl` segment. Shared by the delta-replay path (which has no
/// reconnect cursor); the initial replay path is [`prepare_replay_lines`], which
/// additionally resolves a cursor (and so must see ACUs) before dropping them.
pub(crate) fn filter_delta_replay_lines(contents: &str) -> Vec<&str> {
    let live: Vec<&str> = contents
        .lines()
        .filter(|l| !l.trim().is_empty() && !line_is_available_commands_update(l))
        .collect();
    filter_rewind_lines(live)
}

// ============================================================================
// Selective prompt-extraction parser
// ============================================================================

/// An event yielded by [`PromptExtractIterator`].
///
/// Each event represents the minimal information extracted from one
/// `updates.jsonl` line without deserializing the full typed notification.
#[derive(Debug, PartialEq)]
pub enum PromptExtractEvent {
    /// A text chunk from a `UserMessageChunk` ACP update.
    ///
    /// Multiple consecutive `UserTextChunk` events belong to the same user
    /// message and should be concatenated by the caller. `prompt_index` is the
    /// chunk `_meta.promptIndex` when the turn pipeline stamped one.
    UserTextChunk {
        text: String,
        prompt_index: Option<usize>,
    },

    /// A `RewindMarker` xAI update: truncate accumulated prompts to this index.
    ///
    /// Any in-progress user message should be flushed before truncating.
    RewindTo(usize),

    /// Any other update type — signals that the current user message (if any)
    /// has ended.
    NotUserMessage,
}

impl PromptExtractEvent {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::UserTextChunk {
            text: text.into(),
            prompt_index: None,
        }
    }

    pub fn user_text_pi(text: impl Into<String>, prompt_index: usize) -> Self {
        Self::UserTextChunk {
            text: text.into(),
            prompt_index: Some(prompt_index),
        }
    }
}

/// Iterator that streams [`PromptExtractEvent`]s from a `updates.jsonl` file.
///
/// Unlike [`UpdatesIterator`], this never materialises a full
/// `acp::SessionNotification` or `SessionNotification`. Instead it uses
/// zero-copy `serde_json` deserialization with `&RawValue` to peek at the
/// discriminant field and only extracts the one or two fields actually needed
/// for prompt reconstruction:
///
/// - ACP `"user_message_chunk"` → `update.content.text`
/// - xAI `"rewind_marker"`      → `update.target_prompt_index`
/// - everything else             → [`PromptExtractEvent::NotUserMessage`]
///
/// Parse errors on individual lines are treated conservatively as
/// `NotUserMessage` (matching the "skip malformed line" behavior of the
/// original [`UpdatesIterator`]-based path, but safely terminating any
/// in-progress user-message accumulation).
pub struct PromptExtractIterator {
    reader: std::io::BufReader<std::fs::File>,
    line_buffer: String,
}

impl PromptExtractIterator {
    /// Open a `updates.jsonl` file for selective prompt extraction.
    ///
    /// Returns `None` if the file does not exist.
    pub fn open(path: &std::path::Path) -> std::io::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = std::fs::File::open(path)?;
        Ok(Some(Self {
            reader: std::io::BufReader::new(file),
            line_buffer: String::new(),
        }))
    }
}

impl Iterator for PromptExtractIterator {
    type Item = PromptExtractEvent;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.line_buffer.clear();
            match std::io::BufRead::read_line(&mut self.reader, &mut self.line_buffer) {
                Ok(0) => return None, // EOF
                Err(_) => return Some(PromptExtractEvent::NotUserMessage),
                Ok(_) => {}
            }

            let line = self.line_buffer.trim();
            if line.is_empty() {
                continue;
            }

            return Some(parse_prompt_extract_event(line));
        }
    }
}

/// Assemble accumulated user-prompt strings from a stream of [`PromptExtractEvent`]s.
///
/// Encapsulates the accumulation, flush, and rewind-truncation rules in one
/// place so that every caller — whether reading from disk or from an in-memory
/// iterator — applies identical prompt-extraction semantics:
///
/// - Consecutive `UserTextChunk` events are concatenated into one prompt until
///   a non-user event or a `promptIndex` change opens a new run.
/// - Progressive counting (same as [`UserRunTurnTracker`]): every user run
///   counts until the first `_meta.promptIndex`; after that only marked runs
///   count (mid-turn phantoms are dropped from the list).
/// - `NotUserMessage` flushes any in-progress prompt.
/// - `RewindTo(n)` flushes then truncates the list to `n` **counted** prompts.
///
/// The resulting `Vec` is the resume `prompt_texts` / rewind-picker index
/// space: `prompt_index == prompts.len()` after load, matching live turn
/// stamping (not raw user-message count).
pub fn collect_prompts_from_events(iter: impl Iterator<Item = PromptExtractEvent>) -> Vec<String> {
    let mut prompts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_user = false;
    let mut current_run_pi: Option<usize> = None;
    let mut current_counts = false;
    let mut seen_marker = false;

    fn flush(
        prompts: &mut Vec<String>,
        current: &mut String,
        in_user: &mut bool,
        current_run_pi: &mut Option<usize>,
        current_counts: &mut bool,
    ) {
        if *in_user {
            if *current_counts {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    prompts.push(trimmed);
                }
            }
            current.clear();
            *in_user = false;
            *current_run_pi = None;
            *current_counts = false;
        }
    }

    for event in iter {
        match event {
            PromptExtractEvent::UserTextChunk { text, prompt_index } => {
                if prompt_index.is_some() {
                    seen_marker = true;
                }
                let counts = if seen_marker {
                    prompt_index.is_some()
                } else {
                    true
                };
                let new_run = if !in_user {
                    true
                } else if seen_marker || prompt_index.is_some() {
                    prompt_index != current_run_pi
                } else {
                    false
                };
                if new_run {
                    flush(
                        &mut prompts,
                        &mut current,
                        &mut in_user,
                        &mut current_run_pi,
                        &mut current_counts,
                    );
                    in_user = true;
                    current_run_pi = prompt_index;
                    current_counts = counts;
                    current.push_str(&text);
                } else {
                    current.push_str(&text);
                    if current_run_pi.is_none() && prompt_index.is_some() {
                        current_run_pi = prompt_index;
                        current_counts = true;
                    }
                }
            }
            PromptExtractEvent::RewindTo(target_index) => {
                // Flush any in-progress user message before truncating.
                // Rewinding TO prompt N keeps prompts[0..N].
                flush(
                    &mut prompts,
                    &mut current,
                    &mut in_user,
                    &mut current_run_pi,
                    &mut current_counts,
                );
                prompts.truncate(target_index);
            }
            PromptExtractEvent::NotUserMessage => {
                flush(
                    &mut prompts,
                    &mut current,
                    &mut in_user,
                    &mut current_run_pi,
                    &mut current_counts,
                );
            }
        }
    }

    flush(
        &mut prompts,
        &mut current,
        &mut in_user,
        &mut current_run_pi,
        &mut current_counts,
    );

    prompts
}
/// Collect assistant text from a stream of [`SessionUpdate`]s.
///
/// Extracts `ContentChunk.text` from `AgentMessageChunk` updates.
/// Capped at 100k chars total.
///
/// Note: This collector does not honor rewind markers (unlike PromptExtractIterator).
/// Rewound-away branches may still contribute to FTS index. This is a known limitation;
/// fix by using a rewind-aware replay model (future work).
pub fn collect_assistant_text(
    iter: impl Iterator<Item = io::Result<SessionUpdate>>,
) -> Vec<String> {
    const MAX_CHARS: usize = 100_000;
    let mut texts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars_emitted = 0usize;

    for res in iter {
        let update = match res {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "skipping malformed update in assistant text collector");
                continue;
            }
        };
        match update {
            SessionUpdate::Acp(notification) => {
                match notification.update {
                    acp::SessionUpdate::AgentMessageChunk(chunk) => {
                        if let acp::ContentBlock::Text(text_content) = chunk.content
                            && !text_content.text.is_empty()
                        {
                            // Reserve space for separator before computing budget to avoid overshoot
                            let sep_cost = usize::from(!current.is_empty());
                            let budget = MAX_CHARS
                                .saturating_sub(chars_emitted)
                                .saturating_sub(sep_cost);
                            if budget == 0 {
                                continue;
                            }
                            let text = if text_content.text.len() > budget {
                                // Truncate on a valid UTF-8 char boundary
                                let mut end = budget;
                                while end > 0 && !text_content.text.is_char_boundary(end) {
                                    end -= 1;
                                }
                                &text_content.text[..end]
                            } else {
                                &text_content.text
                            };
                            if !current.is_empty() {
                                current.push(' ');
                                chars_emitted += 1;
                            }
                            current.push_str(text);
                            chars_emitted += text.len();
                        }
                    }
                    _ => {
                        // End of assistant turn
                        if !current.is_empty() {
                            let t = current.trim().to_string();
                            if !t.is_empty() {
                                texts.push(t);
                            }
                            current.clear();
                        }
                    }
                }
            }
            SessionUpdate::Xai(_) => {
                if !current.is_empty() {
                    let t = current.trim().to_string();
                    if !t.is_empty() {
                        texts.push(t);
                    }
                    current.clear();
                }
            }
        }
    }
    if !current.is_empty() {
        let t = current.trim().to_string();
        if !t.is_empty() {
            texts.push(t);
        }
    }
    texts
}

/// Collect tool metadata from a stream of [`SessionUpdate`]s.
///
/// Per Phase 1 contract (ACP data model):
/// - Tool name: from `ToolCall.title` (display name; acp::ToolCall has no .name)
/// - File paths: from `ToolCall.locations[].path` (ACP stores locations, not raw arguments)
/// - Errors: skipped (no is_error on acp::ToolCallUpdate)
///
/// Bounds:
/// - Max 200 tool calls per session
/// - Each extraction capped at 100k chars before final join
///
/// Note: This collector does not honor rewind markers (unlike PromptExtractIterator).
/// Rewound-away branches may still contribute to FTS index. This is a known limitation;
/// fix by using a rewind-aware replay model (future work).
pub fn collect_tool_metadata(iter: impl Iterator<Item = io::Result<SessionUpdate>>) -> Vec<String> {
    let mut meta: Vec<String> = Vec::new();
    let mut tool_call_count = 0usize;
    let mut chars_emitted = 0usize;

    const MAX_TOOL_CALLS: usize = 200;
    const MAX_CHARS: usize = 100_000;

    for res in iter {
        if tool_call_count >= MAX_TOOL_CALLS {
            break;
        }
        let update = match res {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "skipping malformed update in tool metadata collector");
                continue;
            }
        };
        match update {
            SessionUpdate::Acp(notification) => {
                match notification.update {
                    acp::SessionUpdate::ToolCall(tc) => {
                        tool_call_count += 1;

                        // Tool name from .title (acp::ToolCall has no .name field)
                        if !tc.title.is_empty() {
                            let budget = MAX_CHARS.saturating_sub(chars_emitted);
                            if budget == 0 {
                                continue;
                            }
                            let truncated = &tc.title[..tc.title.len().min(budget)];
                            chars_emitted += truncated.len();
                            meta.push(truncated.to_string());
                        }

                        // File paths from locations[].path
                        for loc in &tc.locations {
                            if let Some(path_str) = loc.path.to_str()
                                && !path_str.is_empty()
                            {
                                let budget = MAX_CHARS.saturating_sub(chars_emitted);
                                if budget == 0 {
                                    continue;
                                }
                                let truncated = &path_str[..path_str.len().min(budget)];
                                meta.push(truncated.to_string());
                                chars_emitted += truncated.len();
                            }
                        }
                    }
                    acp::SessionUpdate::ToolCallUpdate(_) => {
                        // Tool results come as ToolCallUpdate; no is_error field available
                    }
                    _ => {}
                }
            }
            SessionUpdate::Xai(_) => {}
        }
    }
    meta
}

// ---------------------------------------------------------------------------
// Selective serde structs — only the fields we care about
// ---------------------------------------------------------------------------

/// Peek inside ACP or xAI `params` to read the `update.sessionUpdate` tag and
/// any fields relevant to `user_message_chunk` or `rewind_marker`.
///
/// Works for both method types because both use the same `update.sessionUpdate`
/// discriminant key in the params JSON.
#[derive(serde::Deserialize)]
struct ParamsPeek<'a> {
    #[serde(borrow)]
    update: UpdatePeek<'a>,
}

#[derive(serde::Deserialize)]
struct UpdatePeek<'a> {
    #[serde(rename = "sessionUpdate")]
    session_update: &'a str,
    /// Present only for `user_message_chunk`.
    #[serde(borrow, default)]
    content: Option<ContentPeek<'a>>,
    /// Chunk `_meta` on ACP updates (carries `promptIndex` for real turns).
    #[serde(default, rename = "_meta")]
    meta: Option<RawChunkMetaPeek>,
    /// Present only for `rewind_marker`.
    target_prompt_index: Option<usize>,
}

/// Selective peek at a `user_message_chunk` content object.
///
/// Shared with the search collectors in [`search`] so the peeked fields and
/// their escape-tolerance cannot drift between the prompt-extraction and
/// indexing paths.
#[derive(serde::Deserialize)]
pub(crate) struct ContentPeek<'a> {
    #[serde(rename = "type", default)]
    pub content_type: Option<&'a str>,
    // `Cow`, not `&str`: serde cannot borrow from JSON strings containing
    // escapes, and the resulting parse error would drop the whole prompt.
    #[serde(borrow, default)]
    pub text: Option<std::borrow::Cow<'a, str>>,
    #[serde(rename = "_meta", default)]
    pub meta: Option<ContentMetaPeek<'a>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct ContentMetaPeek<'a> {
    #[serde(borrow, default)]
    pub bash_command: Option<std::borrow::Cow<'a, str>>,
}

/// Parse one `updates.jsonl` line into a [`PromptExtractEvent`].
///
/// Always returns an event: `NotUserMessage` for every line that is not a
/// user-message chunk or rewind marker (including unparseable ones), so an
/// in-progress prompt is always flushed conservatively.
///
/// Fast path: only those two kinds can produce a non-`NotUserMessage` event, and
/// their discriminant appears verbatim, so a cheap substring pre-check skips the
/// serde peeks for the vast majority of lines. A line merely embedding the
/// discriminant in its content still falls through to the full parse.
pub(crate) fn parse_prompt_extract_event(line: &str) -> PromptExtractEvent {
    if !line.contains(&*USER_MESSAGE_CHUNK) && !line.contains(&*REWIND_MARKER) {
        return PromptExtractEvent::NotUserMessage;
    }

    // Step 1: try to extract the envelope (method + raw params).
    let (raw_params, is_xai) = if let Ok(env) = serde_json::from_str::<RawLinePeek<'_>>(line) {
        let raw = env.params.map(|p| p.get()).unwrap_or(line);
        let xai = env.method == Some(XAI_SESSION_UPDATE_METHOD);
        (raw, xai)
    } else {
        // Not a valid envelope → try legacy format: the line IS the params.
        (line, false)
    };

    // Step 2: parse the discriminant and relevant payload fields in one pass.
    let Ok(peek) = serde_json::from_str::<ParamsPeek<'_>>(raw_params) else {
        // Cannot determine update type → treat conservatively.
        return PromptExtractEvent::NotUserMessage;
    };

    let tag = peek.update.session_update;

    if !is_xai && tag == *USER_MESSAGE_CHUNK {
        if let Some(content) = peek.update.content
            && content.content_type == Some("text")
            && let Some(text) = content.text
        {
            if content
                .meta
                .as_ref()
                .is_some_and(|m| m.bash_command.is_some())
            {
                return PromptExtractEvent::NotUserMessage;
            }
            let prompt_index = peek
                .update
                .meta
                .as_ref()
                .and_then(|m| m.prompt_index.map(|v| v as usize));
            return PromptExtractEvent::UserTextChunk {
                text: text.into_owned(),
                prompt_index,
            };
        }
        // user_message_chunk with non-text content (e.g., image) still ends
        // any in-progress user message.
        return PromptExtractEvent::NotUserMessage;
    }

    if is_xai && tag == *REWIND_MARKER {
        if let Some(idx) = peek.update.target_prompt_index {
            return PromptExtractEvent::RewindTo(idx);
        }
        // Malformed rewind_marker: treat conservatively (flush, no truncate).
        return PromptExtractEvent::NotUserMessage;
    }

    PromptExtractEvent::NotUserMessage
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Wrap an ACP notification as the envelope stored in updates.jsonl.
    fn acp_envelope(session_update_json: &str) -> String {
        format!(
            r#"{{"timestamp":1,"method":"session/update","params":{{"sessionId":"s","update":{session_update_json}}}}}"#
        )
    }

    /// Wrap a xAI notification as the envelope stored in updates.jsonl.
    fn xai_envelope(session_update_json: &str) -> String {
        format!(
            r#"{{"timestamp":1,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{session_update_json}}}}}"#
        )
    }

    // ── parse_prompt_extract_event unit tests ─────────────────────────────────

    #[test]
    fn acp_user_text_chunk_yields_user_text() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::user_text("hello")
        );
    }

    #[test]
    fn acp_user_text_chunk_with_json_escapes_yields_user_text() {
        // Escaped JSON strings cannot be borrowed as &str; a regression to a
        // borrowed peek field would drop this prompt from extraction.
        let line = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"multi\nline \"quoted\" caf\u00e9"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::user_text("multi\nline \"quoted\" caf\u{e9}")
        );
        // An escaped bash command now parses too and must be excluded by the
        // bash_command predicate (it used to be excluded by the parse failure).
        let bash = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"! echo \"hi\"","_meta":{"bash_command":"echo \"hi\""}}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&bash),
            PromptExtractEvent::NotUserMessage
        );
    }

    #[test]
    fn acp_agent_message_chunk_yields_not_user() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"reply"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::NotUserMessage
        );
    }

    #[test]
    fn acp_tool_result_yields_not_user() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"tool_result","toolCallId":"c1","content":[{"type":"text","text":"big output"}]}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::NotUserMessage
        );
    }

    #[test]
    fn xai_rewind_marker_yields_rewind_to() {
        let line = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":3,"created_at":"2024-01-01"}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::RewindTo(3)
        );
    }

    #[test]
    fn xai_rewind_to_zero_yields_rewind_to_zero() {
        let line = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::RewindTo(0)
        );
    }

    #[test]
    fn xai_diff_review_yields_not_user() {
        let line = xai_envelope(r#"{"sessionUpdate":"diff_review","content":[]}"#);
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::NotUserMessage
        );
    }

    /// An ACP `user_message_chunk` with an image content block (not text) must
    /// end the current user message without yielding any text.
    #[test]
    fn acp_user_message_chunk_image_yields_not_user() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"image_url","url":"data:image/png;base64,abc"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::NotUserMessage
        );
    }

    /// Malformed JSON must produce `NotUserMessage` (conservative flush).
    #[test]
    fn malformed_json_yields_not_user() {
        assert_eq!(
            parse_prompt_extract_event("not json at all!!!"),
            PromptExtractEvent::NotUserMessage
        );
    }

    /// Empty string — the iterator skips blanks, but a direct call must still
    /// classify conservatively (the parser always yields an event now).
    #[test]
    fn empty_string_yields_not_user() {
        assert_eq!(
            parse_prompt_extract_event(""),
            PromptExtractEvent::NotUserMessage
        );
    }

    /// A valid JSON object that has no recognisable ACP/xAI shape — NotUserMessage.
    #[test]
    fn unknown_json_object_yields_not_user() {
        assert_eq!(
            parse_prompt_extract_event(r#"{"foo":"bar"}"#),
            PromptExtractEvent::NotUserMessage
        );
    }

    /// Legacy format: raw `acp::SessionNotification` without an outer envelope.
    ///
    /// Old sessions wrote `{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk",...}}`
    /// directly without the `method`/`params` envelope.  The parser must still
    /// extract user text from these lines.
    #[test]
    fn legacy_format_user_message_chunk() {
        let line = r#"{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"legacy prompt"}}}"#;
        assert_eq!(
            parse_prompt_extract_event(line),
            PromptExtractEvent::user_text("legacy prompt")
        );
    }

    #[test]
    fn legacy_format_non_user_update() {
        let line = r#"{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}"#;
        assert_eq!(
            parse_prompt_extract_event(line),
            PromptExtractEvent::NotUserMessage
        );
    }

    // ── PromptExtractIterator integration tests via tempfile ──────────────────

    use std::io::Write as _;

    fn write_updates_file(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    fn collect_events(path: &std::path::Path) -> Vec<PromptExtractEvent> {
        PromptExtractIterator::open(path)
            .unwrap()
            .unwrap()
            .collect()
    }

    #[test]
    fn iterator_single_user_prompt() {
        let chunk = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello world"}}"#,
        );
        let other = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"reply"}}"#,
        );
        let f = write_updates_file(&[&chunk, &other]);

        let events = collect_events(f.path());
        assert_eq!(events[0], PromptExtractEvent::user_text("hello world"));
        assert_eq!(events[1], PromptExtractEvent::NotUserMessage);
    }

    #[test]
    fn iterator_multi_chunk_user_message() {
        let c1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"part1 "}}"#,
        );
        let c2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"part2"}}"#,
        );
        let end = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#,
        );
        let f = write_updates_file(&[&c1, &c2, &end]);

        let events = collect_events(f.path());
        assert_eq!(events[0], PromptExtractEvent::user_text("part1 "));
        assert_eq!(events[1], PromptExtractEvent::user_text("part2"));
        assert_eq!(events[2], PromptExtractEvent::NotUserMessage);
    }

    #[test]
    fn iterator_rewind_marker_truncates() {
        let chunk = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"}}"#,
        );
        let end = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a1"}}"#,
        );
        let rewind = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
        );
        let f = write_updates_file(&[&chunk, &end, &rewind]);

        let events = collect_events(f.path());
        assert_eq!(events[0], PromptExtractEvent::user_text("p1"));
        assert_eq!(events[1], PromptExtractEvent::NotUserMessage);
        assert_eq!(events[2], PromptExtractEvent::RewindTo(0));
    }

    #[test]
    fn iterator_skips_blank_lines() {
        let chunk = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}"#,
        );
        let f = write_updates_file(&["", "   ", &chunk, ""]);

        let events = collect_events(f.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], PromptExtractEvent::user_text("hello"));
    }

    #[test]
    fn iterator_malformed_line_does_not_panic() {
        let bad = "this is not json !!!";
        let good = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"ok"}}"#,
        );
        let f = write_updates_file(&[bad, &good]);

        let events = collect_events(f.path());
        // bad line → NotUserMessage; good line → UserTextChunk
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], PromptExtractEvent::NotUserMessage);
        assert_eq!(events[1], PromptExtractEvent::user_text("ok"));
    }

    #[test]
    fn iterator_nonexistent_file_returns_none() {
        let result =
            PromptExtractIterator::open(std::path::Path::new("/nonexistent/updates.jsonl"));
        assert!(result.unwrap().is_none());
    }

    /// Full round-trip: simulate a session with two user prompts, one rewind,
    /// then a new prompt.  Assemble the events into prompts the same way
    /// `load_user_prompts_from_updates` does.
    #[test]
    fn full_round_trip_with_rewind() {
        // Turn 1: "first prompt"
        let u1a = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"first "}}"#,
        );
        let u1b = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"prompt"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer1"}}"#,
        );
        // Turn 2: "second prompt"
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"second prompt"}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer2"}}"#,
        );
        // Rewind to before turn 2 (keep 1 prompt)
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
        );
        // Turn 2 (after rewind): "new second prompt"
        let u3 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"new second prompt"}}"#,
        );
        let a3 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer3"}}"#,
        );

        let f = write_updates_file(&[&u1a, &u1b, &a1, &u2, &a2, &rw, &u3, &a3]);

        let prompts =
            collect_prompts_from_events(PromptExtractIterator::open(f.path()).unwrap().unwrap());

        assert_eq!(prompts, vec!["first prompt", "new second prompt"]);
    }

    #[test]
    fn collect_prompts_ignores_unmarked_phantoms_when_markers_present() {
        let events = [
            PromptExtractEvent::user_text_pi("hi", 0),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text("!pwd phantom"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text_pi("echo hello", 1),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text("echo hi instead"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text_pi("ty ty", 2),
            PromptExtractEvent::NotUserMessage,
        ];
        let prompts = collect_prompts_from_events(events.into_iter());
        assert_eq!(prompts, vec!["hi", "echo hello", "ty ty"]);
    }

    #[test]
    fn collect_prompts_mixed_unmarked_prefix_then_markers() {
        let events = [
            PromptExtractEvent::user_text("old0"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text("old1"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text_pi("new2", 2),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text("!pwd"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text_pi("new3", 3),
            PromptExtractEvent::NotUserMessage,
        ];
        let prompts = collect_prompts_from_events(events.into_iter());
        assert_eq!(prompts, vec!["old0", "old1", "new2", "new3"]);
    }

    #[test]
    fn parse_extracts_prompt_index_from_update_meta() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi"},"_meta":{"promptIndex":3}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::user_text_pi("hi", 3)
        );
    }

    fn user_chunk(text: &str, prompt_index: Option<usize>) -> SessionUpdate {
        let mut chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
            text.to_string(),
        )));
        if let Some(pi) = prompt_index {
            chunk = chunk.meta(
                serde_json::json!({ "promptIndex": pi })
                    .as_object()
                    .cloned(),
            );
        }
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new("s"),
            acp::SessionUpdate::UserMessageChunk(chunk),
        )))
    }

    fn agent_chunk(text: &str) -> SessionUpdate {
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new("s"),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.to_string()),
            ))),
        )))
    }

    #[test]
    fn updates_truncate_ignores_unmarked_phantoms_when_markers_present() {
        let updates = vec![
            user_chunk("P0", Some(0)),
            agent_chunk("A0"),
            user_chunk("!pwd", None),
            agent_chunk("out"),
            user_chunk("P1", Some(1)),
            agent_chunk("A1"),
            user_chunk("P2", Some(2)),
            agent_chunk("A2"),
        ];
        // Keep through P1 (indices 0,1); cut at start of P2 run.
        let cut = updates_truncate_for_prompt(&updates, 1);
        assert_eq!(cut, 6);
        assert!(matches!(
            &updates[cut],
            SessionUpdate::Acp(n) if matches!(
                &n.update,
                acp::SessionUpdate::UserMessageChunk(c)
                    if matches!(&c.content, acp::ContentBlock::Text(t) if t.text == "P2")
            )
        ));
    }

    #[test]
    fn updates_truncate_splits_consecutive_marked_prompts_without_agent() {
        let updates: Vec<_> = (0..6)
            .map(|i| user_chunk(&format!("P{i}"), Some(i)))
            .collect();
        // Target 2 keeps turns 0 and 1; cut at P2 (index 2).
        assert_eq!(updates_truncate_for_prompt(&updates, 1), 2);
        assert_eq!(updates_truncate_for_prompt(&updates, 2), 3);
        assert_eq!(updates_truncate_for_prompt(&updates, 5), 6);
    }

    /// Mixed stream: unmarked runs before the first promptIndex still count.
    #[test]
    fn updates_truncate_mixed_unmarked_prefix_then_markers() {
        let updates = vec![
            user_chunk("old0", None),
            agent_chunk("A0"),
            user_chunk("old1", None),
            agent_chunk("A1"),
            user_chunk("new2", Some(2)),
            agent_chunk("A2"),
            user_chunk("!pwd", None),
            agent_chunk("out"),
            user_chunk("new3", Some(3)),
            agent_chunk("A3"),
        ];
        // Target 1 keeps old0+old1; cut at new2.
        assert_eq!(updates_truncate_for_prompt(&updates, 1), 4);
        // Target 2 keeps through A2 (and phantom run does not add a turn); cut at new3.
        assert_eq!(updates_truncate_for_prompt(&updates, 2), 8);
        assert_eq!(updates_truncate_for_prompt(&updates, 0), 2);
    }

    #[test]
    fn filter_rewind_mixed_unmarked_prefix_then_markers() {
        let o0 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old0"}}"#,
        );
        let a0 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A0"}}"#,
        );
        let o1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old1"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A1"}}"#,
        );
        let n2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"new2"},"_meta":{"promptIndex":2}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A2"}}"#,
        );
        let n3 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"new3"},"_meta":{"promptIndex":3}}"#,
        );
        // Rewind to target 2: keep turns 0,1 (old0, old1); drop new2+.
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":2,"created_at":"2024-01-01"}"#,
        );
        let after = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"after"},"_meta":{"promptIndex":2}}"#,
        );
        let lines = vec![
            o0.as_str(),
            a0.as_str(),
            o1.as_str(),
            a1.as_str(),
            n2.as_str(),
            a2.as_str(),
            n3.as_str(),
            rw.as_str(),
            after.as_str(),
        ];
        let kept = filter_rewind_lines(lines);
        let texts: Vec<&str> = kept
            .iter()
            .filter_map(|l| {
                if l.contains("\"text\":\"old0\"") {
                    Some("old0")
                } else if l.contains("\"text\":\"old1\"") {
                    Some("old1")
                } else if l.contains("\"text\":\"new2\"") {
                    Some("new2")
                } else if l.contains("\"text\":\"new3\"") {
                    Some("new3")
                } else if l.contains("\"text\":\"after\"") {
                    Some("after")
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(texts, vec!["old0", "old1", "after"]);
    }

    #[test]
    fn filter_rewind_ignores_unmarked_phantoms_when_markers_present() {
        let p0 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"P0"},"_meta":{"promptIndex":0}}"#,
        );
        let a0 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A0"}}"#,
        );
        let phantom = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"!pwd"}}"#,
        );
        let p1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"P1"},"_meta":{"promptIndex":1}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A1"}}"#,
        );
        let p2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"P2"},"_meta":{"promptIndex":2}}"#,
        );
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":2,"created_at":"2024-01-01"}"#,
        );
        let after = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"after"},"_meta":{"promptIndex":2}}"#,
        );
        let lines = vec![
            p0.as_str(),
            a0.as_str(),
            phantom.as_str(),
            p1.as_str(),
            a1.as_str(),
            p2.as_str(),
            rw.as_str(),
            after.as_str(),
        ];
        let kept = filter_rewind_lines(lines);
        let texts: Vec<&str> = kept
            .iter()
            .filter_map(|l| {
                if l.contains("\"text\":\"P0\"") {
                    Some("P0")
                } else if l.contains("!pwd") {
                    Some("phantom")
                } else if l.contains("\"text\":\"P1\"") {
                    Some("P1")
                } else if l.contains("\"text\":\"P2\"") {
                    Some("P2")
                } else if l.contains("\"text\":\"after\"") {
                    Some("after")
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(texts, vec!["P0", "phantom", "P1", "after"]);
    }

    // ── filter_rewind_lines tests ────────────────────────────────────────────

    #[test]
    fn filter_rewind_removes_dead_branch() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"first"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp1"}}"#,
        );
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"second"}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp2"}}"#,
        );
        // Rewind to prompt 1 — kills u2, a2
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
        );
        let u3 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"replacement"}}"#,
        );
        let a3 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp3"}}"#,
        );

        let lines = vec![
            u1.as_str(),
            a1.as_str(),
            u2.as_str(),
            a2.as_str(),
            rw.as_str(),
            u3.as_str(),
            a3.as_str(),
        ];
        let result = filter_rewind_lines(lines);

        // u1, a1 survive. u2, a2, rewind marker removed. u3, a3 added.
        assert_eq!(result.len(), 4);
        assert!(result[0].contains("first"));
        assert!(result[1].contains("resp1"));
        assert!(result[2].contains("replacement"));
        assert!(result[3].contains("resp3"));
    }

    #[test]
    fn filter_rewind_to_zero_clears_all() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"only"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp"}}"#,
        );
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
        );
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"fresh start"}}"#,
        );

        let lines = vec![u1.as_str(), a1.as_str(), rw.as_str(), u2.as_str()];
        let result = filter_rewind_lines(lines);

        assert_eq!(result.len(), 1);
        assert!(result[0].contains("fresh start"));
    }

    #[test]
    fn filter_rewind_double_rewind() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r1"}}"#,
        );
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p2"}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r2"}}"#,
        );
        let u3 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p3"}}"#,
        );
        let a3 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r3"}}"#,
        );
        // Rewind to prompt 2 — kills p3/r3
        let rw1 = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":2,"created_at":"2024-01-01"}"#,
        );
        let u4 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p4"}}"#,
        );
        let a4 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r4"}}"#,
        );
        // Rewind to prompt 1 — kills p2/r2/p4/r4
        let rw2 = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
        );
        let u5 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"final"}}"#,
        );

        let lines = vec![
            u1.as_str(),
            a1.as_str(),
            u2.as_str(),
            a2.as_str(),
            u3.as_str(),
            a3.as_str(),
            rw1.as_str(),
            u4.as_str(),
            a4.as_str(),
            rw2.as_str(),
            u5.as_str(),
        ];
        let result = filter_rewind_lines(lines);

        // Only p1, r1, final survive
        assert_eq!(result.len(), 3);
        assert!(result[0].contains("p1"));
        assert!(result[1].contains("r1"));
        assert!(result[2].contains("final"));
    }

    // ── prepare_replay_lines tests ───────────────────────────────────────────

    /// Envelope with _meta at the params level (where the real agent puts it).
    fn acp_envelope_with_meta(session_update_json: &str, meta_json: &str) -> String {
        format!(
            r#"{{"timestamp":1,"method":"session/update","params":{{"sessionId":"s","update":{session_update_json},"_meta":{meta_json}}}}}"#
        )
    }

    #[test]
    fn prepare_replay_cursor_skips_to_position() {
        let u1 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old"}}"#,
            r#"{"eventId":"ev1"}"#,
        );
        let a1 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old resp"}}"#,
            r#"{"eventId":"ev2"}"#,
        );
        let u2 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"new"}}"#,
            r#"{"eventId":"ev3"}"#,
        );
        let raw = format!("{u1}\n{a1}\n{u2}\n");

        let prepared = prepare_replay_lines(&raw, Some("ev2"));
        // Should skip ev1 and ev2, return only ev3
        assert_eq!(prepared.lines.len(), 1);
        assert!(!prepared.mark_replay);
        assert!(prepared.lines[0].contains("new"));
        assert_eq!(prepared.total_live, 3);
    }

    #[test]
    fn prepare_replay_cursor_not_found_returns_all() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi"}}"#,
        );
        let raw = format!("{u1}\n");

        let prepared = prepare_replay_lines(&raw, Some("nonexistent"));
        assert_eq!(prepared.lines.len(), 1);
        assert!(prepared.mark_replay); // fallback to full replay
    }

    /// A resolved cursor is refused when the tail contains an eventId-less
    /// line (older-binary history): the line has no client-side dedup and no
    /// future cursor can cover it, so an incremental tail would re-apply it.
    /// Full replay is the safe fallback.
    #[test]
    fn prepare_replay_cursor_refused_when_tail_has_event_id_less_line() {
        let a1 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"seen"}}"#,
            r#"{"eventId":"ev1"}"#,
        );
        // xAI-style line persisted by an older binary: no _meta at all.
        let old_xai = r#"{"timestamp":2,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"hook_annotation","message":"trailing"}}}"#;
        let raw = format!("{a1}\n{old_xai}\n");

        let prepared = prepare_replay_lines(&raw, Some("ev1"));
        assert!(
            prepared.mark_replay,
            "an unbounded tail must force a full replay"
        );
        assert_eq!(prepared.lines.len(), 2, "full history is replayed");

        // Same history with the trailing line stamped resolves incrementally.
        let new_xai = r#"{"timestamp":2,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"hook_annotation","message":"trailing"},"_meta":{"eventId":"ev2"}}}"#;
        let raw = format!("{a1}\n{new_xai}\n");
        let prepared = prepare_replay_lines(&raw, Some("ev1"));
        assert!(!prepared.mark_replay);
        assert_eq!(prepared.lines.len(), 1);
        assert!(prepared.lines[0].contains("trailing"));

        // An id-less ACU in the tail is exempt from the refusal — ACUs are
        // dropped before forwarding, so they can never be re-applied.
        let acu =
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#);
        let raw = format!("{a1}\n{acu}\n");
        let prepared = prepare_replay_lines(&raw, Some("ev1"));
        assert!(
            !prepared.mark_replay,
            "a trailing id-less ACU must not force a full replay"
        );
        assert!(
            prepared.lines.is_empty(),
            "the ACU is dropped, never forwarded"
        );
    }

    #[test]
    fn prepare_replay_extracts_max_event_seq() {
        // eventId is "{sessionId}-{counter}" and session ids contain dashes, so
        // the counter is the suffix after the LAST '-'. max_event_seq is the
        // highest counter across all live lines — used to re-seed the global
        // event counter on resume so post-load live events stay monotonic and
        // don't get dropped by the client's eventId dedup.
        let a1 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a"}}"#,
            r#"{"eventId":"019e-abcd-7","totalTokens":100}"#,
        );
        let a2 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"b"}}"#,
            r#"{"eventId":"019e-abcd-42","totalTokens":250}"#,
        );
        // Out-of-order counter (lower than the max) must not lower the result.
        let a3 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"c"}}"#,
            r#"{"eventId":"019e-abcd-13","totalTokens":250}"#,
        );
        let raw = format!("{a1}\n{a2}\n{a3}\n");

        let prepared = prepare_replay_lines(&raw, None);
        assert_eq!(
            prepared.max_event_seq,
            Some(42),
            "max counter across all lines (suffix after last '-')"
        );
        assert_eq!(prepared.last_tokens, 250);
    }

    #[test]
    fn prepare_replay_no_event_ids_yields_none_max_seq() {
        // Lines without a parseable numeric eventId suffix (older shell) yield
        // None, so the counter is left untouched on resume.
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a"}}"#,
        );
        let raw = format!("{a1}\n");
        let prepared = prepare_replay_lines(&raw, None);
        assert_eq!(prepared.max_event_seq, None);
    }

    // ── available_commands_update skip (T1) + single-pass equivalence ─────────

    #[test]
    fn acu_line_detection_exact_and_no_false_positive() {
        let acu =
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#);
        assert!(line_is_available_commands_update(&acu));

        // A user message that merely mentions the phrase must NOT match.
        let user_mentions = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"what is available_commands_update?"}}"#,
        );
        assert!(!line_is_available_commands_update(&user_mentions));
    }

    /// The anchor must reject the discriminant when it sits inside `_meta` (not
    /// at the `params.update` position) — the real update here is a non-ACU.
    #[test]
    fn acu_anchor_ignores_discriminant_in_meta() {
        let line = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#,
            r#"{"sessionUpdate":"available_commands_update"}"#,
        );
        // The exact `"sessionUpdate":"available_commands_update"` substring IS
        // present (in _meta), but it's not anchored to `"update":{`.
        assert!(line.contains(r#""sessionUpdate":"available_commands_update""#));
        assert!(!line_is_available_commands_update(&line));
    }

    /// A NON-ACU line whose `_meta` embeds the FULL unescaped nested anchor
    /// (`{"update":{"sessionUpdate":"available_commands_update",...}}`) passes the
    /// cheap substring pre-filter but must be REJECTED by the positional confirm
    /// (its real `params.update` is a `tool_call`) — so it is never dropped.
    #[test]
    fn acu_confirm_rejects_nested_update_anchor_in_meta() {
        let line = acp_envelope_with_meta(
            r#"{"sessionUpdate":"tool_call","toolCallId":"t","title":"x"}"#,
            r#"{"echo":{"update":{"sessionUpdate":"available_commands_update","availableCommands":[]}}}"#,
        );
        // The discriminant prefix IS present (in _meta) — pre-filter would match...
        assert!(line.contains(&*AVAILABLE_COMMANDS_UPDATE_PREFIX));
        // ...but the structural params.update is a tool_call, so NOT an ACU.
        assert!(!line_is_available_commands_update(&line));

        // And the non-ACU line survives replay (is not dropped).
        let raw = format!("{line}\n");
        let prepared = prepare_replay_lines(&raw, None);
        assert_eq!(prepared.lines.len(), 1, "non-ACU line must not be dropped");
        assert!(prepared.lines[0].contains("tool_call"));
    }

    /// Pin the cross-crate assumption behind [`line_is_available_commands_update`]:
    /// the structural `params.update` serializes BEFORE the optional `_meta`. Run a
    /// genuine ACU through the real write path ([`SessionUpdateEnvelope::from_update`])
    /// and assert its first `"update":` precedes any `"_meta":`, and the detector accepts it.
    #[test]
    fn acu_real_write_path_serializes_update_before_meta() {
        let notif = acp::SessionNotification::new(
            acp::SessionId::new("s"),
            acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(vec![])),
        )
        .meta(serde_json::json!({ "eventId": "ev1" }).as_object().cloned());
        let envelope =
            SessionUpdateEnvelope::from_update(&SessionUpdate::Acp(Box::new(notif))).unwrap();
        let line = serde_json::to_string(&envelope).unwrap();

        let update_idx = line
            .find(UPDATE_KEY)
            .expect("serialized ACU line must contain an \"update\" key");
        if let Some(meta_idx) = line.find(r#""_meta":"#) {
            assert!(
                update_idx < meta_idx,
                "params.update must serialize before _meta: {line}"
            );
        }
        assert!(line_is_available_commands_update(&line));
    }

    #[test]
    fn prepare_replay_drops_available_commands_update() {
        let u = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi"}}"#,
        );
        let acu =
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#);
        let a = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"yo"}}"#,
        );
        let raw = format!("{u}\n{acu}\n{a}\n");

        let prepared = prepare_replay_lines(&raw, None);
        // ACU dropped; the two real updates kept in original order.
        assert_eq!(prepared.lines.len(), 2);
        assert_eq!(prepared.total_live, 2);
        assert!(
            prepared
                .lines
                .iter()
                .all(|l| !l.contains("available_commands_update"))
        );
        assert!(prepared.lines[0].contains("hi"));
        assert!(prepared.lines[1].contains("yo"));
        assert!(prepared.mark_replay);
    }

    #[test]
    fn prepare_replay_scans_last_total_tokens_across_kept_lines() {
        let u = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi"}}"#,
            r#"{"totalTokens":10}"#,
        );
        let acu =
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#);
        let a = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"yo"}}"#,
            r#"{"totalTokens":42}"#,
        );
        let raw = format!("{u}\n{acu}\n{a}\n");

        let prepared = prepare_replay_lines(&raw, None);
        // Last totalTokens wins; ACU lines (no tokens) don't disturb it.
        assert_eq!(prepared.last_tokens, 42);
        assert_eq!(prepared.lines.len(), 2);
    }

    #[test]
    fn prepare_replay_rewind_truncates_and_drops_acu() {
        let u0 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p0"}}"#,
            r#"{"totalTokens":5}"#,
        );
        let acu =
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#);
        let a0 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a0"}}"#,
            r#"{"totalTokens":7}"#,
        );
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
        );
        let u1 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"}}"#,
            r#"{"totalTokens":9}"#,
        );
        let raw = format!("{u0}\n{acu}\n{a0}\n{rw}\n{u1}\n");

        let prepared = prepare_replay_lines(&raw, None);
        // Rewind to 0 kills u0/a0; ACU dropped; only the new p1 survives.
        assert_eq!(prepared.lines.len(), 1);
        assert!(prepared.lines[0].contains("p1"));
        assert_eq!(prepared.total_live, 1);
        // last_tokens recomputed from the surviving timeline (p1 = 9).
        assert_eq!(prepared.last_tokens, 9);
        assert!(prepared.mark_replay);
    }

    /// The single-pass implementation must match an independent reference that
    /// drops ACU then applies the (canonical) rewind filter — for a mixed input.
    #[test]
    fn prepare_replay_single_pass_matches_reference() {
        let lines_src = [
            acp_envelope_with_meta(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p0"}}"#,
                r#"{"totalTokens":3}"#,
            ),
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#),
            acp_envelope_with_meta(
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed"}"#,
                r#"{"totalTokens":11}"#,
            ),
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#),
            acp_envelope(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a0"}}"#,
            ),
        ];
        let raw = format!("{}\n", lines_src.join("\n"));

        // Reference: filter blanks + ACU, then canonical rewind filter, count.
        let reference: Vec<&str> = filter_rewind_lines(
            raw.lines()
                .filter(|l| !l.trim().is_empty() && !line_is_available_commands_update(l))
                .collect(),
        );

        let prepared = prepare_replay_lines(&raw, None);
        assert_eq!(prepared.lines, reference);
        assert_eq!(prepared.total_live, reference.len());
        assert_eq!(prepared.last_tokens, 11); // last kept line carrying tokens
    }

    /// The prompt-extract fast-reject must not be fooled by lines that merely
    /// contain the discriminant substring inside their content — the full parse
    /// still classifies them by the real `sessionUpdate` tag.
    #[test]
    fn fast_reject_handles_discriminant_substring_in_content() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"the user_message_chunk format"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::NotUserMessage
        );
    }

    /// A `rewind_marker` appearing only inside content must NEVER become a
    /// `RewindTo` (which would corrupt prompt_index / turn numbering).
    #[test]
    fn fast_reject_rewind_marker_in_content() {
        // (a) agent message mentioning rewind_marker → NotUserMessage.
        let agent = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"about rewind_marker semantics"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&agent),
            PromptExtractEvent::NotUserMessage
        );

        // (b) an ACP (non-xai) update carrying rewind_marker in content is NOT a
        // real xai rewind_marker → NotUserMessage (no RewindTo).
        let acp_rewindish = acp_envelope(
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"rewind_marker"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&acp_rewindish),
            PromptExtractEvent::NotUserMessage
        );

        // (c) a user_message_chunk whose text contains rewind_marker → still the
        // user text (the discriminant is user_message_chunk).
        let user = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"explain rewind_marker please"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&user),
            PromptExtractEvent::user_text("explain rewind_marker please")
        );
    }

    /// A user prompt whose text contains the literal escaped-JSON ACU
    /// discriminant must NOT be dropped as an `available_commands_update` — the
    /// `"update":{` anchor only matches the real structural discriminant, not the
    /// escaped fragment in content.
    #[test]
    fn acu_drop_ignores_escaped_json_in_content() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"paste: {\"sessionUpdate\":\"available_commands_update\"}"}}"#,
        );
        // The bare phrase appears in the (escaped) content, but it's not at the
        // structural `"update":{"sessionUpdate":...` position, so it's kept.
        assert!(line.contains("available_commands_update"));
        assert!(!line_is_available_commands_update(&line));

        let raw = format!("{line}\n");
        let prepared = prepare_replay_lines(&raw, None);
        assert_eq!(prepared.lines.len(), 1, "user prompt must survive replay");
        assert!(prepared.lines[0].contains("available_commands_update"));
    }

    /// An idle client reconnecting with the cursor pointing at the LAST persisted
    /// event — an ACU (the post-load re-advertise) — must resolve the cursor on the
    /// ACU-inclusive set rather than fall back to full replay.
    #[test]
    fn prepare_replay_cursor_on_dropped_acu_resolves() {
        let u = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi"}}"#,
            r#"{"eventId":"ev1"}"#,
        );
        let a = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"yo"}}"#,
            r#"{"eventId":"ev2"}"#,
        );
        let acu = acp_envelope_with_meta(
            r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#,
            r#"{"eventId":"ev3"}"#,
        );
        let raw = format!("{u}\n{a}\n{acu}\n");

        // Cursor == the ACU's eventId → resolved; nothing after → no replay,
        // and crucially NOT a full replay.
        let prepared = prepare_replay_lines(&raw, Some("ev3"));
        assert!(!prepared.mark_replay, "must not fall back to full replay");
        assert!(prepared.lines.is_empty(), "client is already caught up");

        // Cursor == ev1 → replay ev2, ev3; the ACU (ev3) is dropped from the tail.
        let prepared = prepare_replay_lines(&raw, Some("ev1"));
        assert!(!prepared.mark_replay);
        assert_eq!(prepared.lines.len(), 1);
        assert!(prepared.lines[0].contains("yo"));
    }

    /// A trailing `rewind_marker` empties the live set and yields
    /// `last_tokens == 0` (the `unwrap_or(0)` path).
    #[test]
    fn prepare_replay_trailing_rewind_marker_empties() {
        let u0 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p0"}}"#,
            r#"{"totalTokens":5}"#,
        );
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
        );
        let raw = format!("{u0}\n{rw}\n");
        let prepared = prepare_replay_lines(&raw, None);
        assert!(prepared.lines.is_empty());
        assert_eq!(prepared.total_live, 0);
        assert_eq!(prepared.last_tokens, 0);
    }

    /// An ACU as the final line is dropped without disturbing tokens.
    #[test]
    fn prepare_replay_trailing_acu_dropped() {
        let u = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi"}}"#,
            r#"{"totalTokens":7}"#,
        );
        let acu =
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#);
        let raw = format!("{u}\n{acu}\n");
        let prepared = prepare_replay_lines(&raw, None);
        assert_eq!(prepared.lines.len(), 1);
        assert!(prepared.lines[0].contains("hi"));
        assert_eq!(prepared.last_tokens, 7);
        assert_eq!(prepared.total_live, 1);
    }

    /// Rewind + cursor + ACU together, with explicit expected values.
    #[test]
    fn prepare_replay_rewind_then_cursor_with_acu() {
        let u0 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p0"}}"#,
            r#"{"eventId":"e0","totalTokens":2}"#,
        );
        let a0 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a0"}}"#,
            r#"{"eventId":"e1"}"#,
        );
        let acu0 =
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#);
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
        );
        let u1 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"}}"#,
            r#"{"eventId":"e2","totalTokens":9}"#,
        );
        let acu1 =
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#);
        let a1 = acp_envelope_with_meta(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a1"}}"#,
            r#"{"eventId":"e3","totalTokens":12}"#,
        );
        let raw = format!("{u0}\n{a0}\n{acu0}\n{rw}\n{u1}\n{acu1}\n{a1}\n");

        // Rewind to 0 kills u0/a0/acu0; surviving live = [u1(e2), acu1, a1(e3)].
        // Cursor on e2 → tail = [acu1, a1]; drop acu1 → lines = [a1].
        let prepared = prepare_replay_lines(&raw, Some("e2"));
        assert!(!prepared.mark_replay);
        assert_eq!(prepared.lines.len(), 1);
        assert!(prepared.lines[0].contains("a1"));
        assert_eq!(prepared.last_tokens, 12); // last token-bearing survivor
        assert_eq!(prepared.total_live, 2); // ACU-free survivors: u1, a1
    }

    /// The delta-replay helper (shared with the initial path) drops blanks + ACUs
    /// and applies the canonical rewind filter.
    #[test]
    fn filter_delta_replay_drops_blank_acu_and_rewinds() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"}}"#,
        );
        let acu =
            acp_envelope(r#"{"sessionUpdate":"available_commands_update","availableCommands":[]}"#);
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a1"}}"#,
        );
        // A second prompt that a trailing rewind_marker then discards.
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p2-dead"}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a2-dead"}}"#,
        );
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
        );
        let raw = format!("{u1}\n\n{acu}\n{a1}\n{u2}\n{a2}\n{rw}\n");

        let live = filter_delta_replay_lines(&raw);
        // Blank + ACU dropped; the rewind to prompt 1 truncates the dead branch
        // (u2/a2) and consumes the marker, leaving only p1/a1.
        assert_eq!(live.len(), 2);
        assert!(
            live.iter()
                .all(|l| !l.contains("available_commands_update"))
        );
        assert!(live[0].contains("p1"));
        assert!(live[1].contains("a1"));
        assert!(live.iter().all(|l| !l.contains("dead")));
        assert!(live.iter().all(|l| !l.contains("rewind_marker")));
    }

    #[test]
    fn prepare_replay_reports_spawn_without_finish() {
        let spawn = |id: &str, child: &str| {
            format!(
                r#"{{"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"subagent_spawned","subagent_id":"{id}","parent_session_id":"s","child_session_id":"{child}","subagent_type":"general-purpose","description":"task"}},"_meta":{{"eventId":"s-1"}}}}}}"#
            )
        };
        let finish = |id: &str| {
            format!(
                r#"{{"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"subagent_finished","subagent_id":"{id}","child_session_id":"c{id}","status":"completed","tool_calls":0,"turns":0,"duration_ms":0}},"_meta":{{"eventId":"s-2"}}}}}}"#
            )
        };
        // `a` spawns and finishes (paired); `b` only spawns (orphan).
        let raw = format!(
            "{}\n{}\n{}\n",
            spawn("a", "ca"),
            finish("a"),
            spawn("b", "cb")
        );
        let prepared = prepare_replay_lines(&raw, None);
        assert_eq!(
            prepared.unfinished_subagents,
            vec![("b".to_string(), "cb".to_string())]
        );
    }

    /// Legacy lines put `sessionId`/`update` at the top level (no `params`
    /// envelope); orphan detection must still pair them.
    #[test]
    fn collect_unfinished_subagents_handles_legacy_top_level_lines() {
        let lines = vec![
            r#"{"sessionId":"s","update":{"sessionUpdate":"subagent_spawned","subagent_id":"a","parent_session_id":"s","child_session_id":"ca","subagent_type":"general-purpose","description":"task"}}"#,
            r#"{"sessionId":"s","update":{"sessionUpdate":"subagent_finished","subagent_id":"a","child_session_id":"ca","status":"completed","tool_calls":0,"turns":0,"duration_ms":0}}"#,
            r#"{"sessionId":"s","update":{"sessionUpdate":"subagent_spawned","subagent_id":"b","parent_session_id":"s","child_session_id":"cb","subagent_type":"general-purpose","description":"task"}}"#,
        ];
        // `a` is paired (spawn+finish); `b` only spawned → orphan.
        assert_eq!(
            collect_unfinished_subagents(&lines),
            vec![("b".to_string(), "cb".to_string())]
        );
    }

    /// Resume idempotency seam: the finish the stream reconcile emits must
    /// re-pair the orphan's spawn on the next resume (emit→serialize→collect),
    /// so a second resume doesn't re-emit. Guards a `SubagentFinished` shape drift.
    #[test]
    fn collect_pairs_a_reconcile_emitted_finish_with_its_spawn() {
        use crate::extensions::notification::{SessionNotification, SessionUpdate};

        let spawn = r#"{"sessionId":"s","update":{"sessionUpdate":"subagent_spawned","subagent_id":"sa","parent_session_id":"s","child_session_id":"ca","subagent_type":"general-purpose","description":"task"}}"#.to_string();
        // Build the finish exactly as the stream reconcile emits it.
        let finish = serde_json::to_string(&SessionNotification {
            session_id: acp::SessionId::new("s"),
            update: SessionUpdate::SubagentFinished {
                subagent_id: "sa".into(),
                child_session_id: "ca".into(),
                status: "cancelled".into(),
                error: Some("interrupted by process restart".into()),
                tool_calls: 0,
                turns: 0,
                duration_ms: 0,
                tokens_used: 0,
                output: None,
                will_wake: false,
            },
            meta: None,
        })
        .unwrap();

        assert!(
            collect_unfinished_subagents(&[spawn.as_str(), finish.as_str()]).is_empty(),
            "the emitted finish must re-pair the spawn so a 2nd resume doesn't re-emit"
        );
    }

    // ── collect_assistant_text / collect_tool_metadata tests ──────────────────

    #[test]
    fn collect_assistant_text_extracts_chunks() {
        let lines = vec![
            acp_envelope(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}"#,
            ),
            acp_envelope(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}}"#,
            ),
        ];
        let updates: Vec<_> = lines
            .into_iter()
            .map(|s| Ok(serde_json::from_str(&s).unwrap()))
            .collect();
        let result = collect_assistant_text(updates.into_iter());
        assert_eq!(result, vec!["hello world"]);
    }

    #[test]
    fn collect_assistant_text_caps_at_100k() {
        // Two 60k chunks with non-ASCII, separator, and truncation
        let chunk1 = "x".repeat(60_000) + "café"; // 60k + 5 bytes (café is 5 UTF-8 bytes)
        let chunk2 = "日本語".repeat(20_000); // 60k bytes (3 bytes per char)
        let lines = vec![
            acp_envelope(&format!(
                r#"{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{chunk1}"}}}}"#
            )),
            acp_envelope(&format!(
                r#"{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{chunk2}"}}}}"#
            )),
        ];
        let updates: Vec<_> = lines
            .into_iter()
            .map(|s| Ok(serde_json::from_str(&s).unwrap()))
            .collect();
        let result = collect_assistant_text(updates.into_iter());
        let total: usize = result.iter().map(|s| s.len()).sum();
        assert!(total <= 100_000, "got {total} chars");
        // Verify non-ASCII content is present (not corrupted by truncation)
        assert!(
            result.iter().any(|s| s.contains("café")),
            "non-ASCII should be preserved"
        );
    }

    #[test]
    fn collect_tool_metadata_extracts_title_and_paths() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"Read `/tmp/foo.rs`","kind":"read","locations":[{"path":"/tmp/foo.rs"}]}"#,
        );
        let updates: Vec<_> = vec![Ok(serde_json::from_str(&line).unwrap())];
        let result = collect_tool_metadata(updates.into_iter());
        assert!(result.contains(&"Read `/tmp/foo.rs`".to_string()));
        assert!(result.contains(&"/tmp/foo.rs".to_string()));
    }

    #[test]
    fn collect_tool_metadata_caps_at_200_calls() {
        let mut lines = Vec::new();
        for i in 0..250 {
            lines.push(acp_envelope(&format!(
                r#"{{"sessionUpdate":"tool_call","toolCallId":"tc{i}","title":"tool_{i}","kind":"exec","locations":[]}}"#,
            )));
        }
        let updates: Vec<_> = lines
            .into_iter()
            .map(|s| Ok(serde_json::from_str(&s).unwrap()))
            .collect();
        let result = collect_tool_metadata(updates.into_iter());
        // Should cap at 200 tool calls (title + paths, but paths empty so just titles)
        let titles: Vec<_> = result.iter().filter(|s| s.starts_with("tool_")).collect();
        assert_eq!(titles.len(), 200);
    }

    #[test]
    fn from_str_unknown_xai_variant_deserializes_via_envelope() {
        // Simulates an updates.jsonl line containing a removed variant (e.g. git_branch_update).
        // SessionUpdateEnvelope::from_str must not error — the Unknown catch-all absorbs it.
        let line = xai_envelope(r#"{"sessionUpdate":"git_branch_update","branch":"main"}"#);
        let update = SessionUpdateEnvelope::from_str(&line).unwrap();
        match update {
            SessionUpdate::Xai(notif) => {
                assert_eq!(
                    notif.update,
                    crate::extensions::notification::SessionUpdate::Unknown
                );
            }
            SessionUpdate::Acp(_) => panic!("expected Xai variant"),
        }
    }

    #[test]
    fn from_str_known_xai_variant_still_works() {
        let line = xai_envelope(r#"{"sessionUpdate":"memory_flush_started"}"#);
        let update = SessionUpdateEnvelope::from_str(&line).unwrap();
        match update {
            SessionUpdate::Xai(notif) => {
                assert_eq!(
                    notif.update,
                    crate::extensions::notification::SessionUpdate::MemoryFlushStarted
                );
            }
            SessionUpdate::Acp(_) => panic!("expected Xai variant"),
        }
    }
}
