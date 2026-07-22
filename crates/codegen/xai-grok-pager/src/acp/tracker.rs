//! AcpUpdateTracker — converts ACP SessionUpdate events into scrollback mutations.
//!
//! This is a stateful streaming machine: it tracks which entries are currently
//! being streamed to (agent message, thinking) and which tool calls are pending.
//! Each `handle_update()` call processes one event and mutates the scrollback.
use crate::acp::meta::{NotificationMeta, user_message_chunk_meta, user_prompt_meta};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::SessionEvent;
use crate::scrollback::blocks::tool::list_dir::ListDirToolCallBlock;
use crate::scrollback::blocks::tool::search::{
    SearchFileMatch, SearchInputMeta, SearchLineMatch, SearchOutputMode, SearchToolCallBlock,
};
use crate::scrollback::blocks::tool::{
    DiscoveredTool, EditHighlightPhase, EditToolCallBlock, ExecuteToolCallBlock,
    IntegrationSearchToolCallBlock, LineRange, MemorySearchToolCallBlock, OtherToolCallBlock,
    ReadMediaKind, ReadToolCallBlock, ToolCallBlock, UseToolCallBlock, WebFetchToolCallBlock,
    WebSearchToolCallBlock,
};
use crate::scrollback::entry::{EntryId, ScrollbackEntry};
use crate::scrollback::state::ScrollbackState;
use crate::scrollback::state::verb_group::verb_group_kind_changed;
use agent_client_protocol as acp;
use chrono::{DateTime, Local, TimeZone};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::debug;
use xai_grok_tools::types::output::{BashOutput, ToolOutput};
use xai_grok_tools::types::output::{ReadFileOutput, SearchToolOutput, WebFetchOutput};
use xai_grok_tools::util::strip_redundant_session_cd;
/// Convert a UTC millisecond timestamp to local time.
fn utc_ms_to_local(ms: i64) -> DateTime<Local> {
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .map(|utc| utc.with_timezone(&Local))
        .unwrap_or_else(Local::now)
}
/// What the agent is currently doing within a turn.
///
/// Derived from the tracker's internal state by [`AcpUpdateTracker::activity()`].
/// Used by the turn status line widget to show context-appropriate indicators.
///
/// Note: `Idle` here means "the tracker has no in-flight work". The caller
/// should check `TurnState` to distinguish true idle (no turn) from waiting
/// (turn started, but no chunks received yet).
/// Why a turn is open but nothing is streaming right now.
///
/// Replaces the old single, opaque "Waiting…" placeholder: instead of treating
/// the absence of activity as one undifferentiated state, the turn-status line
/// names *what* the agent is blocked on. Resolved partly by the tracker (the
/// blocking tool waits it suppresses — see [`AcpUpdateTracker::activity`]) and
/// partly at the view boundary (`Model`/`Subagent`, which need turn-state and
/// the subagent registry the tracker doesn't own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitingReason {
    /// Waiting for the model to (re)start streaming — the first token after the
    /// prompt is sent, or the gap after a tool completes before the next
    /// inference step begins.
    Model,
    /// Blocked on a running foreground subagent (`task` / `spawn_subagent`).
    Subagent,
    /// Blocked polling/awaiting a background task's output
    /// (`get_command_or_subagent_output` / `get_task_output`).
    ///
    /// `task_ids` come from the tool's `raw_input` (empty until it arrives).
    /// `subject` is an optional display name (description preferred, else
    /// command) filled in by the view from live task state — the tracker
    /// itself always leaves it `None`.
    TaskOutput {
        task_ids: Vec<String>,
        subject: Option<String>,
        /// True when the call blocks (`timeout_ms > 0` in raw_input); an
        /// instant poll (0/missing) can't be shortened by interjecting.
        /// Defaults to false until raw_input arrives.
        waits: bool,
    },
    /// Blocked until one or more background tasks finish
    /// (`wait_commands_or_subagents` / `wait_tasks`).
    TasksComplete,
    /// Explicit sleep / await (`Await` / `Sleep …`).
    Sleep,
}
/// Max chars for wait/tool *description* subjects in status UI (matches
/// tool-title truncation in `format_activity_label`).
pub const MAX_ACTIVITY_SUBJECT_CHARS: usize = 40;
/// First non-empty trimmed line, clamped to [`MAX_ACTIVITY_SUBJECT_CHARS`].
pub fn clamp_activity_subject(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| s.trim());
    if line.chars().count() <= MAX_ACTIVITY_SUBJECT_CHARS {
        line.to_string()
    } else {
        line.chars().take(MAX_ACTIVITY_SUBJECT_CHARS).collect()
    }
}
/// Shared in-progress subject label (clamped description/command) used by
/// turn-status, title bar, and dashboard/subagent activity columns.
///
/// Renders as `{subject}…` — no "Waiting for" prefix or quotes — so a
/// description like `Wait 5 seconds` reads cleanly next to the spinner.
pub fn format_waiting_for_subject(subject: &str) -> String {
    let clamped = clamp_activity_subject(subject);
    if clamped.is_empty() {
        "Waiting on task output…".to_string()
    } else {
        format!("{clamped}…")
    }
}
impl WaitingReason {
    /// Unit constructor for a task-output wait with no known ids/subject yet.
    /// A known-blocking task-output wait (the only kind `activity()` shows).
    pub fn task_output() -> Self {
        Self::TaskOutput {
            task_ids: Vec::new(),
            subject: None,
            waits: true,
        }
    }
    /// User-facing spinner label.
    pub fn label(&self) -> String {
        match self {
            Self::Model => "Waiting for response…".to_string(),
            Self::Subagent => "Waiting on subagent…".to_string(),
            Self::TaskOutput {
                subject: Some(subject),
                ..
            } => format_waiting_for_subject(subject),
            Self::TaskOutput { .. } => "Waiting on task output…".to_string(),
            Self::TasksComplete => "Waiting on tasks…".to_string(),
            Self::Sleep => "Sleeping…".to_string(),
        }
    }
    /// Short, stable snake_case label for telemetry / phase-transition logs.
    pub fn as_telemetry_label(&self) -> &'static str {
        match self {
            Self::Model => "waiting_model",
            Self::Subagent => "waiting_subagent",
            Self::TaskOutput { .. } => "waiting_task_output",
            Self::TasksComplete => "waiting_tasks_complete",
            Self::Sleep => "waiting_sleep",
        }
    }
}
/// A suppressed blocking tool's wait, tagged with the stream it was registered
/// under (drives `drop_stale_blocking_waits`).
#[derive(Debug, Clone)]
struct BlockingWait {
    reason: WaitingReason,
    stream_start_ms: Option<i64>,
}
/// `strings`-greppable marker proving a binary carries this fix (kept by `#[used]`).
#[used]
static PAGER_IMPL_WAIT_STATUS_MIDTURN: &str = "PAGER_IMPL_wait_status_midturn";
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnActivity {
    /// Agent is streaming thinking/chain-of-thought content.
    Thinking,
    /// Agent is streaming response text.
    Responding,
    /// A tool is executing.
    ToolRunning {
        /// Tool title (e.g., command name, file path). Used for `Run …`
        /// when no human description is available.
        title: String,
        /// Optional human description from tool input (e.g. bash
        /// `description`). Prefer this over `Run <command>` when set
        /// (renders as `{desc}…`).
        description: Option<String>,
    },
    /// Auto-compaction in progress (mid-turn, agent-initiated).
    AutoCompacting,
    /// A retry is in progress (transient error, empty response, etc.).
    Retrying {
        /// Current retry attempt number (1-indexed).
        attempt: u32,
        /// Maximum number of retries allowed.
        max_retries: u32,
        /// Human-readable reason for the retry.
        reason: String,
    },
    /// Turn is open but nothing is streaming; `reason` says what we're waiting
    /// on. Replaces the implicit "no activity == generic Waiting…" placeholder.
    Waiting(WaitingReason),
}
impl TurnActivity {
    /// Short, stable label for telemetry / profiling logs.
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::Responding => "responding",
            Self::ToolRunning { .. } => "tool_running",
            Self::AutoCompacting => "compacting",
            Self::Retrying { .. } => "retrying",
            Self::Waiting(reason) => reason.as_telemetry_label(),
        }
    }
}
#[derive(Debug, Clone)]
pub struct PendingCompaction {
    pub tokens_before: Option<u64>,
    pub estimate_after: u64,
    pub elapsed_ms: Option<i64>,
    pub last_used: Option<u64>,
}
/// Tracks in-flight streaming state for one agent's turn.
///
/// Converts ACP `SessionUpdate` variants into scrollback entry mutations.
/// Does nothing else — no UI, no networking, just data transformation.
#[derive(Debug, Default)]
pub struct AcpUpdateTracker {
    /// Entry currently receiving AgentMessageChunk deltas.
    /// None between turns or before first message chunk.
    current_agent_msg: Option<EntryId>,
    /// Entry currently receiving AgentThoughtChunk deltas.
    /// None when agent isn't thinking.
    current_thinking: Option<EntryId>,
    /// Tool calls in flight, keyed by ACP tool call ID string.
    /// Stores the base ToolCall for field merging with ToolCallUpdate.
    pending_tools: HashMap<String, PendingTool>,
    /// ToolCallUpdates that arrived before their ToolCall (race condition).
    /// When the ToolCall arrives, we merge and create the entry immediately
    /// as completed.
    orphan_updates: HashMap<String, acp::ToolCallUpdate>,
    /// Last computed thinking elapsed (ms) from server timestamps.
    /// Updated on every thought chunk as `agentTimestampMs - streamStartMs`.
    /// Frozen when thinking ends (passed to `finish_running_with_time`).
    last_thinking_elapsed_ms: Option<i64>,
    /// When true, the next UserMessageChunk will be silently ignored
    /// because we already pushed the user prompt entry directly from
    /// `dispatch_send_prompt`. Reset after one skip.
    skip_next_user_echo: bool,
    /// When true, the next UserMessageChunk is a skill body that follows
    /// a skill metadata chunk. It should be silently absorbed so the
    /// raw skill instructions don't appear in scrollback.
    skip_next_skill_body: bool,
    /// Tool call IDs suppressed from scrollback (e.g. TodoWrite).
    /// Their ToolCallUpdate counterparts are silently dropped too.
    suppressed_tools: std::collections::HashSet<String>,
    /// Suppressed-but-blocking tool calls, keyed by tool-call ID → the reason
    /// the turn is waiting. These tools (`get_command_or_subagent_output`,
    /// `wait_tasks`, `Sleep`, …) are kept out of `pending_tools` (so they never
    /// hit scrollback) but the turn *is* blocked on them — without this the
    /// spinner falls back to a generic "Waiting…". Populated in
    /// `handle_tool_call`, cleared on the suppressed tool's completion update
    /// and in `finish_turn`.
    blocking_waits: std::collections::HashMap<String, BlockingWait>,
    /// Task tool `run_in_background` flags, keyed by `task_id` (subagent_id).
    /// Populated when a task tool call is detected (variant == "Task"),
    /// consumed by the acp_handler when `SubagentSpawned` arrives.
    pub(crate) task_tool_background: std::collections::HashMap<String, bool>,
    /// Tool call IDs marked as background (`is_background=true`).
    ///
    /// First-detection (no scrollback entry yet): defers entry creation until
    /// `x.ai/task_backgrounded` creates a `BgTask` block.
    /// Late-detection (Execute block already exists): suppresses further output
    /// streaming; the existing block is demoted by `handle_task_backgrounded`.
    ///
    /// Value is the optional description from `raw_input.description`.
    pub(crate) bg_deferred_tools: std::collections::HashMap<String, Option<String>>,
    /// Last seen `stream_start_ms` from notification meta.
    /// When this changes, a new LLM streaming response has started — we
    /// finish any in-flight thinking/agent-message entries so the next
    /// chunks create fresh ones instead of appending to stale entries.
    last_stream_start_ms: Option<i64>,
    /// Monotonic count of live parent-agent updates that changed scrollback.
    agent_output_epoch: u64,
    /// Session project cwd for display-only redundant-`cd` stripping.
    /// Set from [`AgentSession::cwd`]; not used for execution.
    session_cwd: Option<PathBuf>,
    /// Compaction-related activity override.
    /// Set by `set_compaction_activity()` from ExtNotification events,
    /// cleared by `finish_turn()`.
    compaction_activity: Option<TurnActivity>,
    pending_compaction: Option<PendingCompaction>,
    /// Retry-related activity override.
    /// Set by `set_retry_activity()` from ExtNotification `RetryState::Retrying`,
    /// auto-cleared when normal streaming data resumes (in `handle_update`)
    /// and on `finish_turn()`.
    retry_activity: Option<TurnActivity>,
    /// Pending ACP commands from the most recent `AvailableCommandsUpdate`.
    /// Consumed by the caller via `take_pending_acp_commands()`. The caller
    /// is responsible for copying to `AgentSession.available_commands` and
    /// bumping `available_commands_generation`.
    pending_acp_commands: Option<Vec<acp::AvailableCommand>>,
    /// Pending agent toolset from the most recent `AvailableCommandsUpdate.meta`.
    /// Format on the wire: `{"tools": ["read_file", ...]}`.
    /// `Some(_)` only if the shell included a tools list this round.
    /// Consumed by the caller via `take_pending_acp_tools()`.
    ///
    /// Invariant: drained synchronously by
    /// `acp_handler::handle_session_notification` immediately after each
    /// `handle_update` call -- so this field never accumulates across
    /// notifications. A meta-less follow-up update intentionally
    /// preserves the previous `Some` (see the assignment in
    /// `handle_update`) so a partial replay can't silently regress the
    /// registry to the unknown-toolset state.
    pending_acp_tools: Option<Vec<String>>,
    /// Live Edit completions awaiting full-file HL (drained via [`Self::take_pending_edit_hl`]).
    pending_edit_hl: Vec<EntryId>,
}
/// A tool call that's been started but not yet completed.
#[derive(Debug)]
struct PendingTool {
    /// Scrollback entry ID, or None if the entry hasn't been created yet.
    /// The entry is deferred until we receive the real tool kind from the
    /// first in-progress update. The initial ToolCall message often has
    /// kind=Other with no useful metadata — creating an entry from it
    /// would show a wrong block type briefly before the real kind arrives.
    entry_id: Option<EntryId>,
    base: acp::ToolCall,
    /// Streaming UTF-8 decoder for incremental bash output deltas.
    utf8_decoder: Utf8Decoder,
    /// Stashed `started_at` from eager creation. The eagerly-created block
    /// is `ToolCallBlock::Other`; when the refinement arrives with the real
    /// kind, `transfer_timing_from` can't cross variant boundaries
    /// (Other → Search, etc.) and would silently drop the timing. This
    /// field preserves the instant so `set_started_at` can apply it to
    /// whatever variant the refined block becomes.
    started_at: Option<std::time::Instant>,
}
/// Streaming UTF-8 decoder for incremental byte deltas.
///
/// When output is split at arbitrary byte offsets, a multi-byte UTF-8
/// character can land across two deltas. Without buffering, both halves
/// would be replaced with U+FFFD by `from_utf8_lossy`, permanently
/// corrupting the character.
///
/// This decoder buffers trailing incomplete bytes from each delta and
/// prepends them to the next one. Only genuinely invalid sequences
/// (not just incomplete ones at the end) produce U+FFFD.
#[derive(Debug, Default)]
struct Utf8Decoder {
    /// Trailing bytes from the last delta that didn't form a complete
    /// UTF-8 character. At most 3 bytes (max continuation length).
    buffer: Vec<u8>,
    /// Reusable output buffer — avoids allocating a new String per delta.
    /// Cleared on each `decode()` call, grows to high-water mark and stays.
    decoded: String,
}
impl Utf8Decoder {
    /// Feed raw bytes and return the decoded string slice.
    ///
    /// Any trailing incomplete UTF-8 sequence is held back in the internal
    /// buffer and will be prepended to the next `decode()` call. Genuinely
    /// invalid byte sequences produce U+FFFD.
    ///
    /// The returned `&str` is valid until the next `decode()` call.
    fn decode(&mut self, piece: &[u8]) -> &str {
        self.decoded.clear();
        self.buffer.extend_from_slice(piece);
        let mut last_invalid_len = None;
        for chunk in self.buffer.utf8_chunks() {
            if let Some(prev) = last_invalid_len.replace(chunk.invalid().len())
                && prev > 0
            {
                self.decoded.push(char::REPLACEMENT_CHARACTER);
            }
            self.decoded.push_str(chunk.valid());
        }
        match last_invalid_len {
            Some(0) => self.buffer.clear(),
            Some(n) => {
                let keep_from = self.buffer.len() - n;
                self.buffer.drain(..keep_from);
            }
            None => self.buffer.clear(),
        }
        &self.decoded
    }
}
impl AcpUpdateTracker {
    pub fn new() -> Self {
        Self::default()
    }
    /// Current boundary for visible live parent-agent output.
    pub(crate) fn agent_output_epoch(&self) -> u64 {
        self.agent_output_epoch
    }
    fn bump_agent_output_epoch(&mut self) {
        self.agent_output_epoch = self.agent_output_epoch.wrapping_add(1);
    }
    /// Record session cwd used when stripping redundant `cd` prefixes in chrome.
    /// No-op when the path is already stored (avoids cloning on every update).
    pub fn set_session_cwd(&mut self, cwd: impl AsRef<Path>) {
        let cwd = cwd.as_ref();
        if self.session_cwd.as_deref() != Some(cwd) {
            self.session_cwd = Some(cwd.to_path_buf());
        }
    }
    /// Current activity within the turn, derived from in-flight state.
    ///
    /// Priority order (highest first):
    /// 1. External overrides: Retrying, AutoCompacting (from ExtNotification)
    /// 2. Known-blocking wait (task output / wait / sleep / foreground
    ///    subagent) — outranks Thinking, ToolRunning, and Responding.
    /// 3. Thinking (agent is in chain-of-thought)
    /// 4. ToolRunning (a tool call is pending / executing)
    /// 5. Responding (agent is streaming text)
    /// 6. None (nothing in-flight; the view turns this into Waiting(Model) or
    ///    Waiting(Subagent) while a turn is running)
    ///
    /// Retry and compaction states are set externally via
    /// `set_retry_activity()` / `set_compaction_activity()` since they
    /// come from ExtNotification, not from standard ACP SessionUpdate messages.
    ///
    /// When [`Self::session_cwd`] is set, execute activity titles omit a leading
    /// `cd <cwd> &&` / `;` that only restates the session working directory.
    pub fn activity(&self) -> Option<TurnActivity> {
        if self.retry_activity.is_some() {
            return self.retry_activity.clone();
        }
        if self.compaction_activity.is_some() {
            return self.compaction_activity.clone();
        }
        if let Some(waiting) = self.activity_known_blocking_wait() {
            return Some(waiting);
        }
        if self.current_thinking.is_some() {
            return Some(TurnActivity::Thinking);
        }
        if let Some(tool) = self.pending_tools.values().next() {
            let description = tool
                .base
                .raw_input
                .as_ref()
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(clamp_activity_subject);
            let title = tool
                .base
                .raw_input
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| tool.base.title.clone());
            let title = peeled_if_changed(&title, self.session_cwd.as_deref()).unwrap_or(title);
            return Some(TurnActivity::ToolRunning { title, description });
        }
        if self.current_agent_msg.is_some() {
            return Some(TurnActivity::Responding);
        }
        None
    }
    /// Spinner activity for a suppressed blocking tool, or `None`. Instant
    /// task-output polls (`timeout_ms` 0/missing) are excluded.
    fn activity_known_blocking_wait(&self) -> Option<TurnActivity> {
        let reason = self.blocking_wait()?;
        if matches!(reason, WaitingReason::TaskOutput { waits: false, .. }) {
            return None;
        }
        Some(TurnActivity::Waiting(reason))
    }
    /// Highest-priority blocking-tool wait currently in flight, if any.
    ///
    /// `blocking_waits` is a map (non-deterministic iteration order), so
    /// collapse it to a single reason by a fixed priority. In practice at most
    /// one blocking tool runs at a time; the ordering only matters for the
    /// degenerate multi-tool case.
    fn blocking_wait(&self) -> Option<WaitingReason> {
        self.blocking_waits
            .values()
            .min_by_key(|w| match &w.reason {
                WaitingReason::TaskOutput { .. } => 0,
                WaitingReason::TasksComplete => 1,
                WaitingReason::Sleep => 2,
                WaitingReason::Subagent => 3,
                WaitingReason::Model => 4,
            })
            .map(|w| w.reason.clone())
    }
    /// Drop waits not registered under `current_stream` (stale earlier rounds,
    /// or an unknown `None` stream); co-batched same-stream waits survive.
    fn drop_stale_blocking_waits(&mut self, current_stream: Option<i64>) {
        self.blocking_waits
            .retain(|_, w| current_stream.is_some() && w.stream_start_ms == current_stream);
    }
    pub fn tool_title(&self, tool_call_id: &str) -> Option<&str> {
        self.pending_tools
            .get(tool_call_id)
            .map(|pending| pending.base.title.as_str())
    }
    /// Get the scrollback entry_id for a pending tool by tool_call_id.
    ///
    /// Used by demotion to find the execute block to swap.
    pub fn pending_tool_entry_id(&self, tool_call_id: &str) -> Option<EntryId> {
        self.pending_tools
            .get(tool_call_id)
            .and_then(|t| t.entry_id)
    }
    /// Remove a tool from pending_tools (for demotion swap).
    ///
    /// Called when an execute block is being swapped to a BgTask block.
    pub fn remove_pending_tool(&mut self, tool_call_id: &str) {
        self.pending_tools.remove(tool_call_id);
    }
    /// Get the tool_call_id of the currently running Execute tool, if any.
    ///
    /// Used by demotion (Ctrl+B) to know which tool to background.
    /// Returns None if no Execute tool is currently pending.
    pub fn running_execute_tool_call_id(&self) -> Option<&str> {
        self.pending_tools
            .iter()
            .find(|(_, tool)| tool.base.kind == acp::ToolKind::Execute && tool.entry_id.is_some())
            .map(|(id, _)| id.as_str())
    }
    /// Set a compaction-related activity override.
    ///
    /// Called by the ACP handler when `ExtNotification` compaction events
    /// arrive. Cleared automatically by `finish_turn()`.
    pub fn set_compaction_activity(&mut self, activity: Option<TurnActivity>) {
        self.compaction_activity = activity;
    }
    pub fn defer_compaction(
        &mut self,
        tokens_before: Option<u64>,
        estimate_after: u64,
        elapsed_ms: Option<i64>,
    ) {
        self.pending_compaction = Some(PendingCompaction {
            tokens_before,
            estimate_after,
            elapsed_ms,
            last_used: None,
        });
    }
    pub fn note_context_used(&mut self, used: u64) {
        if let Some(pending) = self.pending_compaction.as_mut() {
            pending.last_used = Some(used);
        }
    }
    /// Set a retry-related activity override.
    ///
    /// Called by the ACP handler when `ExtNotification` `RetryState::Retrying`
    /// arrives. Auto-cleared when normal streaming data resumes (in
    /// `handle_update`) and on `finish_turn()`.
    pub fn set_retry_activity(&mut self, activity: Option<TurnActivity>) {
        self.retry_activity = activity;
    }
    /// Take pending ACP commands, if any. Returns `None` if no update arrived
    /// since the last drain.
    ///
    /// The caller is the single drain site: it copies the commands to
    /// `AgentSession.available_commands` and bumps the generation counter.
    pub fn take_pending_acp_commands(&mut self) -> Option<Vec<acp::AvailableCommand>> {
        self.pending_acp_commands.take()
    }
    /// Take the agent's most recently advertised tool list, if any.
    ///
    /// Drained alongside `take_pending_acp_commands()` -- the same
    /// `AvailableCommandsUpdate` carries both. `None` means the shell
    /// didn't include a `meta.tools` field (older shell, or no update
    /// since last drain).
    pub fn take_pending_acp_tools(&mut self) -> Option<Vec<String>> {
        self.pending_acp_tools.take()
    }
    /// Drain Edit entry ids that need a background full-file HL job.
    pub fn take_pending_edit_hl(&mut self) -> Vec<EntryId> {
        std::mem::take(&mut self.pending_edit_hl)
    }
    /// Whether `block` is a successful Edit with hunks (worth a full-file HL job).
    fn edit_wants_file_hl(block: &RenderBlock) -> bool {
        matches!(
            block, RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) if edit.error
            .is_none() && ! edit.hunks.is_empty()
        )
    }
    /// Stash `entry_id` for live successful Edits with hunks. Skips replay
    /// because a resume replays every historical edit at once — queueing them
    /// would thundering-herd N full-file jobs — and replayed edits' files may
    /// have changed on disk since, so the styles would not match the hunks.
    fn queue_edit_hl_if_needed(&mut self, entry_id: EntryId, block: &RenderBlock, is_replay: bool) {
        if !is_replay && Self::edit_wants_file_hl(block) {
            self.pending_edit_hl.push(entry_id);
        }
    }
    /// Push a completed tool block, queue its edit-HL upgrade if warranted, and
    /// clear the running state — the shared tail of every completed-tool path.
    /// Evaluates the predicate before `push_block` consumes the block, so the
    /// entry needs no re-fetch.
    ///
    /// The returned id may no longer be in the scrollback: a completed Edit
    /// can coalesce into an adjacent earlier Edit of the same file.
    fn finish_completed_tool(
        &mut self,
        block: RenderBlock,
        scrollback: &mut ScrollbackState,
        is_replay: bool,
    ) -> EntryId {
        let wants_hl = Self::edit_wants_file_hl(&block);
        let id = scrollback.push_block(block);
        if !is_replay && wants_hl {
            self.pending_edit_hl.push(id);
        }
        scrollback.finish_running(id);
        self.try_coalesce_edit(id, scrollback, is_replay);
        id
    }
    /// The Edit block of `entry` if it qualifies for coalescing with an
    /// adjacent same-file Edit: completed successfully with hunks, a
    /// trustworthy one-liner summary, and free of per-entry attachments a
    /// merge would misplace.
    fn coalescable_edit(entry: &ScrollbackEntry) -> Option<&EditToolCallBlock> {
        if entry.is_running || entry.is_pending_user_input || entry.hook_data.is_some() {
            return None;
        }
        let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block else {
            return None;
        };
        (edit.error.is_none() && !edit.hunks.is_empty() && !edit.summary_untrusted).then_some(edit)
    }
    /// Whether the completed Edit entries `earlier` and `later` target the
    /// same file and may merge into one block.
    fn edits_can_merge(
        &self,
        scrollback: &ScrollbackState,
        earlier: EntryId,
        later: EntryId,
    ) -> bool {
        if scrollback.is_committed(earlier) || scrollback.is_committed(later) {
            return false;
        }
        let (Some(a), Some(b)) = (
            scrollback
                .get_by_id(earlier)
                .and_then(Self::coalescable_edit),
            scrollback.get_by_id(later).and_then(Self::coalescable_edit),
        ) else {
            return false;
        };
        if a.prefix != b.prefix {
            return false;
        }
        let cwd = self.session_cwd.as_deref();
        let resolve = |p: &str| crate::render::tool_paths::resolve_tool_path_target(p, cwd);
        match (resolve(&a.path), resolve(&b.path)) {
            (Some(pa), Some(pb)) => pa == pb,
            (None, None) => a.path == b.path,
            _ => false,
        }
    }
    /// Coalesce the just-completed Edit at `entry_id` with strictly adjacent
    /// completed Edits of the same file, so back-to-back edits render as one
    /// block with a summed diffstat. The earlier entry always survives.
    ///
    /// Checks the previous neighbor (sequential completions) and the next one
    /// (parallel calls can complete out of push order, so the pair only
    /// becomes mergeable when the earlier call lands). Loops so runs of 3+
    /// collapse pairwise.
    ///
    /// Ingestion-time only: a later `collapsed_edit_blocks` flip never
    /// merges or unmerges rows that already landed.
    fn try_coalesce_edit(
        &mut self,
        entry_id: EntryId,
        scrollback: &mut ScrollbackState,
        is_replay: bool,
    ) {
        if !crate::appearance::cache::load_collapsed_edit_blocks() {
            return;
        }
        if scrollback
            .get_by_id(entry_id)
            .and_then(Self::coalescable_edit)
            .is_none()
        {
            return;
        }
        let mut survivor = entry_id;
        loop {
            let Some(idx) = scrollback.index_of_id(survivor) else {
                return;
            };
            let prev_id = idx
                .checked_sub(1)
                .and_then(|i| scrollback.get(i))
                .map(|e| e.id);
            if let Some(prev_id) = prev_id
                && self.edits_can_merge(scrollback, prev_id, survivor)
            {
                self.merge_edit_entries(prev_id, survivor, scrollback, is_replay);
                survivor = prev_id;
                continue;
            }
            let next_id = scrollback.get(idx + 1).map(|e| e.id);
            if let Some(next_id) = next_id
                && self.edits_can_merge(scrollback, survivor, next_id)
            {
                self.merge_edit_entries(survivor, next_id, scrollback, is_replay);
                continue;
            }
            return;
        }
    }
    /// Append `removed`'s hunks onto `survivor` (the earlier entry) —
    /// stitching overlapping/adjacent ones into unified hunks — and drop
    /// `removed` from the scrollback and the edit-HL queue.
    fn merge_edit_entries(
        &mut self,
        survivor: EntryId,
        removed: EntryId,
        scrollback: &mut ScrollbackState,
        is_replay: bool,
    ) {
        let (removed_hunks, removed_edit_count) =
            match scrollback.get_by_id(removed).map(|e| &e.block) {
                Some(RenderBlock::ToolCall(ToolCallBlock::Edit(edit))) => {
                    (edit.hunks.clone(), edit.edit_count)
                }
                _ => return,
            };
        if let Some(entry) = scrollback.get_by_id_mut(survivor) {
            if let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &mut entry.block {
                let merged_edit_count = edit.edit_count + removed_edit_count;
                let mut hunks = std::mem::take(&mut edit.hunks);
                hunks.extend(removed_hunks);
                edit.set_hunks(crate::diff::stitch_overlapping_hunks(hunks));
                edit.edit_count = merged_edit_count;
                edit.highlight = EditHighlightPhase::HunkOnly;
            }
            entry.invalidate_cache();
        }
        scrollback.mark_structurally_dirty(survivor);
        scrollback.remove_entry(removed);
        self.pending_edit_hl.retain(|id| *id != removed);
        if !is_replay && !self.pending_edit_hl.contains(&survivor) {
            self.pending_edit_hl.push(survivor);
        }
    }
    /// Process a single SessionUpdate, mutating the scrollback.
    ///
    /// The `meta` carries server-side timestamps used for thinking elapsed time.
    /// Returns true if the scrollback was modified (needs redraw).
    pub fn handle_update(
        &mut self,
        update: acp::SessionUpdate,
        meta: &NotificationMeta,
        scrollback: &mut ScrollbackState,
    ) -> bool {
        if !meta.is_replay {
            debug!(
                target : crate ::tracing::ACP_UPDATE_TARGET, "[acp] {} | {}",
                update_summary(& update), meta_summary(meta),
            );
        }
        if self.retry_activity.is_some() {
            self.retry_activity = None;
        }
        if let Some(new_start) = meta.stream_start_ms {
            if self
                .last_stream_start_ms
                .is_some_and(|prev| prev != new_start)
            {
                let thinking_has_content = self
                    .current_thinking
                    .and_then(|id| scrollback.get_by_id(id))
                    .is_some_and(|e| {
                        if let RenderBlock::Thinking(t) = &e.block {
                            !t.text().is_empty()
                        } else {
                            false
                        }
                    });
                if thinking_has_content {
                    self.finish_thinking(scrollback);
                }
                if let Some(agent_id) = self.current_agent_msg.take() {
                    scrollback.finish_running(agent_id);
                }
                if !meta.is_replay
                    && self.current_thinking.is_none()
                    && self.activity_known_blocking_wait().is_none()
                {
                    self.pre_create_thinking(scrollback);
                }
            }
            self.last_stream_start_ms = Some(new_start);
        }
        let is_agent_output = matches!(
            &update,
            acp::SessionUpdate::AgentMessageChunk(_)
                | acp::SessionUpdate::AgentThoughtChunk(_)
                | acp::SessionUpdate::ToolCall(_)
                | acp::SessionUpdate::ToolCallUpdate(_)
        );
        let changed = match update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                self.blocking_waits.clear();
                self.handle_agent_chunk(chunk, meta, scrollback)
            }
            acp::SessionUpdate::AgentThoughtChunk(thought) => {
                self.drop_stale_blocking_waits(meta.stream_start_ms);
                self.handle_thought_chunk(thought, meta, scrollback)
            }
            acp::SessionUpdate::ToolCall(tc) => {
                self.handle_tool_call(tc, scrollback, meta.is_replay)
            }
            acp::SessionUpdate::ToolCallUpdate(tcu) => {
                self.handle_tool_call_update(tcu, scrollback, meta.is_replay)
            }
            acp::SessionUpdate::UserMessageChunk(chunk) => {
                self.handle_user_message(chunk, meta, scrollback)
            }
            acp::SessionUpdate::AvailableCommandsUpdate(update) => {
                if let Some(t) = parse_tools_meta(update.meta.as_ref()) {
                    self.pending_acp_tools = Some(t);
                }
                self.pending_acp_commands = Some(update.available_commands);
                true
            }
            acp::SessionUpdate::Plan(_) | acp::SessionUpdate::CurrentModeUpdate(_) => false,
            _ => false,
        };
        if is_agent_output && changed && !meta.is_replay {
            self.bump_agent_output_epoch();
        }
        changed
    }
    /// Called when PromptResponse is received (turn complete).
    pub fn finish_turn(&mut self, scrollback: &mut ScrollbackState) {
        self.finish_thinking(scrollback);
        if let Some(agent_id) = self.current_agent_msg.take() {
            scrollback.finish_running(agent_id);
        }
        for (_, pending) in self.pending_tools.drain() {
            if let Some(entry_id) = pending.entry_id {
                scrollback.finish_running(entry_id);
            }
        }
        if let Some(pending) = self.pending_compaction.take() {
            scrollback.push_block(RenderBlock::session_event(
                SessionEvent::CompactionCompleted {
                    tokens_before: pending.tokens_before,
                    tokens_after: pending.last_used.unwrap_or(pending.estimate_after),
                    elapsed_ms: pending.elapsed_ms,
                },
            ));
        }
        self.last_thinking_elapsed_ms = None;
        self.last_stream_start_ms = None;
        self.compaction_activity = None;
        self.retry_activity = None;
        self.suppressed_tools.clear();
        self.blocking_waits.clear();
        self.orphan_updates.clear();
        self.skip_next_skill_body = false;
    }
    /// Finish the current thinking block, passing elapsed time to the entry.
    ///
    /// Empty thinking blocks (pre-created but never received content) are
    /// removed from scrollback — they'd show a misleading "Thought for 0.0s".
    /// Only blocks that received actual thinking tokens are kept.
    fn finish_thinking(&mut self, scrollback: &mut ScrollbackState) {
        if let Some(thinking_id) = self.current_thinking.take() {
            let is_empty = scrollback.get_by_id(thinking_id).is_some_and(
                |e| matches!(& e.block, RenderBlock::Thinking(t) if t.text().is_empty()),
            );
            if is_empty {
                scrollback.remove_entry(thinking_id);
            } else {
                scrollback.finish_running_with_time(thinking_id, self.last_thinking_elapsed_ms);
            }
            self.last_thinking_elapsed_ms = None;
        }
    }
    /// Pre-create a thinking block so "Thinking…" appears immediately
    /// when the turn starts, before the first ThinkingDelta arrives.
    ///
    /// The tracker's `current_thinking` is set so subsequent ThinkingDelta
    /// chunks append to this entry instead of creating a new one.
    /// No-op when `show_thinking_blocks` is off.
    pub fn pre_create_thinking(&mut self, scrollback: &mut ScrollbackState) {
        if !crate::appearance::cache::load_show_thinking_blocks() {
            return;
        }
        if self.current_thinking.is_none() {
            let block = RenderBlock::thinking_streaming();
            let entry_id = scrollback.push_block(block);
            scrollback.set_last_running(true);
            self.current_thinking = Some(entry_id);
        }
    }
    /// Mark that the next UserMessageChunk should be silently dropped.
    ///
    /// Call this from `dispatch_send_prompt` after pushing the user entry
    /// directly, so the ACP echo doesn't produce a duplicate.
    pub fn expect_user_echo(&mut self) {
        self.skip_next_user_echo = true;
    }
    /// Reset stale skip state when no local user block was rendered, so the
    /// agent's user-message broadcast is the one source of the user echo
    /// (e.g. the synthetic cron/bash adoption path) instead of being dropped.
    pub fn clear_user_echo_skip(&mut self) {
        self.skip_next_user_echo = false;
        self.skip_next_skill_body = false;
    }
    /// Whether [`expect_user_echo`] is pending (subagent replay tests).
    #[cfg(test)]
    pub fn expects_user_echo(&self) -> bool {
        self.skip_next_user_echo
    }
    /// Handle an agent message chunk (streaming text).
    fn handle_agent_chunk(
        &mut self,
        chunk: acp::ContentChunk,
        meta: &NotificationMeta,
        scrollback: &mut ScrollbackState,
    ) -> bool {
        self.finish_thinking(scrollback);
        let text = extract_text_from_content(&chunk.content);
        if text.is_empty() {
            return false;
        }
        if self.current_agent_msg.is_none() && text.trim().is_empty() {
            tracing::warn!(
                text = % text.escape_debug(),
                "ignoring whitespace-only agent message chunk (no prior content)"
            );
            return false;
        }
        let is_new = self.current_agent_msg.is_none();
        let id = *self.current_agent_msg.get_or_insert_with(|| {
            let entry_id = scrollback.start_streaming_agent();
            scrollback.set_last_running(true);
            entry_id
        });
        if is_new
            && let Some(ts_ms) = meta.agent_timestamp_ms
            && let Some(entry) = scrollback.get_by_id_mut(id)
        {
            entry.created_at = Some(utc_ms_to_local(ts_ms));
        }
        if meta.is_replay {
            scrollback.push_chunk_to_agent_deferred(id, &text)
        } else {
            scrollback.push_chunk_to_agent(id, &text)
        }
    }
    /// Handle an agent thought chunk (streaming thinking).
    fn handle_thought_chunk(
        &mut self,
        thought: acp::ContentChunk,
        meta: &NotificationMeta,
        scrollback: &mut ScrollbackState,
    ) -> bool {
        if !crate::appearance::cache::load_show_thinking_blocks() {
            return false;
        }
        let text = match &thought.content {
            acp::ContentBlock::Text(t) => &t.text,
            _ => return false,
        };
        if text.is_empty() {
            return false;
        }
        let is_replay = meta.is_replay;
        let id = *self.current_thinking.get_or_insert_with(|| {
            let block = if is_replay {
                RenderBlock::thinking_streaming_replay()
            } else {
                RenderBlock::thinking_streaming()
            };
            let entry_id = scrollback.push_block(block);
            scrollback.set_last_running(true);
            entry_id
        });
        if let (Some(agent_ts), Some(stream_start)) =
            (meta.agent_timestamp_ms, meta.stream_start_ms)
        {
            self.last_thinking_elapsed_ms = Some(agent_ts - stream_start);
        }
        if meta.is_replay {
            scrollback.push_chunk_to_thinking_deferred(id, text)
        } else {
            scrollback.push_chunk_to_thinking(id, text)
        }
    }
    /// Handle a tool call start.
    fn handle_tool_call(
        &mut self,
        tc: acp::ToolCall,
        scrollback: &mut ScrollbackState,
        is_replay: bool,
    ) -> bool {
        self.finish_thinking(scrollback);
        self.current_agent_msg = None;
        if is_todo_tool(&tc)
            || is_bg_plumbing_tool(&tc)
            || is_task_tool(&tc)
            || is_goal_tool(&tc)
            || is_scheduler_tool(&tc)
            || is_workflow_tool(&tc)
        {
            if is_task_tool(&tc) {
                let is_background = tc
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("subagentBackground"))
                    .and_then(serde_json::Value::as_bool);
                if is_background != Some(true) {
                    self.blocking_waits.insert(
                        tc.tool_call_id.0.to_string(),
                        BlockingWait {
                            reason: WaitingReason::Subagent,
                            stream_start_ms: self.last_stream_start_ms,
                        },
                    );
                }
            } else if let Some(reason) = blocking_wait_reason(&tc) {
                self.blocking_waits.insert(
                    tc.tool_call_id.0.to_string(),
                    BlockingWait {
                        reason,
                        stream_start_ms: self.last_stream_start_ms,
                    },
                );
            }
            self.suppressed_tools.insert(tc.tool_call_id.0.to_string());
            return false;
        }
        let tc_id = tc.tool_call_id.0.to_string();
        if let Some(orphan) = self.orphan_updates.remove(&tc_id) {
            let merged = merge_tool_call_update(tc, orphan);
            let block = tool_call_to_block(&merged, self.session_cwd.as_deref());
            self.finish_completed_tool(block, scrollback, is_replay);
            return true;
        }
        let is_completed = matches!(
            tc.status,
            acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
        );
        if is_completed {
            let block = tool_call_to_block(&tc, self.session_cwd.as_deref());
            self.finish_completed_tool(block, scrollback, is_replay);
        } else {
            let block = tool_call_to_block(&tc, self.session_cwd.as_deref());
            let id = scrollback.push_block(block);
            scrollback.set_last_running(true);
            let started_at = Some(std::time::Instant::now());
            self.pending_tools.insert(
                tc_id,
                PendingTool {
                    entry_id: Some(id),
                    base: tc,
                    utf8_decoder: Utf8Decoder::default(),
                    started_at,
                },
            );
        }
        true
    }
    /// Handle a tool call update (streaming output or completion).
    fn handle_tool_call_update(
        &mut self,
        tcu: acp::ToolCallUpdate,
        scrollback: &mut ScrollbackState,
        is_replay: bool,
    ) -> bool {
        let tc_id_str = tcu.tool_call_id.0.to_string();
        if self.bg_deferred_tools.contains_key(&tc_id_str) {
            return false;
        }
        if self.suppressed_tools.contains(&tc_id_str) {
            if let Some(ref raw_input) = tcu.fields.raw_input {
                let variant = raw_input.get("variant").and_then(|v| v.as_str());
                if is_task_variant(variant) {
                    let run_in_bg = raw_input
                        .get("run_in_background")
                        .or_else(|| raw_input.get("background"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    if variant == Some("Task")
                        && let Some(task_id) = raw_input.get("task_id").and_then(|v| v.as_str())
                    {
                        self.task_tool_background
                            .insert(task_id.to_string(), run_in_bg);
                    }
                    if run_in_bg {
                        self.blocking_waits.remove(&tc_id_str);
                    }
                }
                if let Some(WaitingReason::TaskOutput {
                    task_ids, waits, ..
                }) = self
                    .blocking_waits
                    .get_mut(&tc_id_str)
                    .map(|w| &mut w.reason)
                {
                    let extracted = task_ids_from_raw_input(raw_input);
                    if !extracted.is_empty() {
                        *task_ids = extracted;
                    }
                    if raw_input.get("timeout_ms").is_some() {
                        *waits = timeout_waits(Some(raw_input));
                    }
                }
            }
            let status = tcu.fields.status.unwrap_or_default();
            if matches!(
                status,
                acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
            ) {
                self.suppressed_tools.remove(&tc_id_str);
                self.blocking_waits.remove(&tc_id_str);
            }
            return false;
        }
        let status = tcu.fields.status.unwrap_or_default();
        let is_completed = matches!(
            status,
            acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
        );
        let tc_id = tcu.tool_call_id.0.to_string();
        if !is_completed {
            let defer_as_bg = if let Some(pending) = self.pending_tools.get_mut(&tc_id) {
                let bash_output = extract_bash_output_from_value(&tcu.fields.raw_output);
                pending.base.update(tcu.fields);
                if pending.entry_id.is_none() && is_bg_tool(&pending.base) {
                    let desc = extract_raw_field(&pending.base, "description");
                    Some((tc_id.clone(), desc, false))
                } else if pending.entry_id.is_some() && is_bg_tool(&pending.base) {
                    let eid = pending.entry_id;
                    let has_real_command = raw_input_command(&pending.base).is_some();
                    let entry_placeholder = eid
                        .and_then(|id| scrollback.get_by_id(id))
                        .is_some_and(entry_is_execute_placeholder);
                    let drop_placeholder = entry_placeholder && !has_real_command;
                    let desc = extract_raw_field(&pending.base, "description");
                    if drop_placeholder {
                        if let Some(id) = pending.entry_id.take() {
                            scrollback.remove_entry(id);
                        }
                        Some((tc_id.clone(), desc, false))
                    } else {
                        if let Some(entry_id) = pending.entry_id {
                            let mut block =
                                tool_call_to_block(&pending.base, self.session_cwd.as_deref());
                            let mut kind_changed = false;
                            if let Some(entry) = scrollback.get_by_id_mut(entry_id) {
                                if let RenderBlock::ToolCall(new_tc) = &mut block
                                    && let Some(t) = pending.started_at
                                {
                                    new_tc.set_started_at(t);
                                }
                                kind_changed = verb_group_kind_changed(&entry.block, &block);
                                entry.block = block;
                                entry.invalidate_cache();
                            }
                            if kind_changed {
                                scrollback.mark_structurally_dirty(entry_id);
                            }
                        }
                        Some((tc_id.clone(), desc, true))
                    }
                } else {
                    let entry_id = if let Some(entry_id) = pending.entry_id {
                        let block = tool_call_to_block(&pending.base, self.session_cwd.as_deref());
                        scrollback.replace_tool_block(entry_id, block, pending.started_at);
                        entry_id
                    } else {
                        let block = tool_call_to_block(&pending.base, self.session_cwd.as_deref());
                        let id = scrollback.push_block(block);
                        scrollback.set_last_running(true);
                        pending.entry_id = Some(id);
                        id
                    };
                    if let Some(bash_output) = bash_output {
                        if let Some(delta) = &bash_output.output_delta {
                            let text = pending.utf8_decoder.decode(delta);
                            return scrollback.append_execute_output(entry_id, text);
                        }
                        let output_str = String::from_utf8_lossy(&bash_output.output);
                        return scrollback.set_execute_output(entry_id, &output_str);
                    }
                    return true;
                }
            } else {
                return false;
            };
            if let Some((deferred_id, description, keep_in_pending)) = defer_as_bg {
                tracing::debug!(
                    tool_call_id = % deferred_id, keep_in_pending,
                    "Deferring is_background=true tool to bg_deferred_tools"
                );
                if !keep_in_pending {
                    self.pending_tools.remove(&deferred_id);
                }
                self.bg_deferred_tools.insert(deferred_id, description);
                return false;
            }
            unreachable!("both branches above return");
        }
        if let Some(pending) = self.pending_tools.remove(&tc_id) {
            let merged = merge_tool_call_update(pending.base, tcu);
            let block = tool_call_to_block(&merged, self.session_cwd.as_deref());
            if let Some(entry_id) = pending.entry_id {
                if scrollback.replace_tool_block(entry_id, block, pending.started_at)
                    && let Some(entry) = scrollback.get_by_id(entry_id)
                {
                    self.queue_edit_hl_if_needed(entry_id, &entry.block, is_replay);
                }
                scrollback.finish_running(entry_id);
                self.try_coalesce_edit(entry_id, scrollback, is_replay);
            } else {
                self.finish_completed_tool(block, scrollback, is_replay);
            }
            true
        } else {
            self.orphan_updates.insert(tc_id, tcu);
            false
        }
    }
    /// Handle a user message chunk (session replay or live followup).
    ///
    /// If `skip_next_user_echo` is set, this is the ACP echo of a prompt
    /// we already added to scrollback — drop it but still reset tracking
    /// state so the agent's response creates fresh entries.
    fn handle_user_message(
        &mut self,
        chunk: acp::ContentChunk,
        meta: &NotificationMeta,
        scrollback: &mut ScrollbackState,
    ) -> bool {
        let text = extract_text_from_content(&chunk.content);
        if self.skip_next_skill_body {
            self.skip_next_skill_body = false;
            return false;
        }
        if text.is_empty() {
            return false;
        }
        self.finish_thinking(scrollback);
        if let Some(agent_id) = self.current_agent_msg.take() {
            scrollback.finish_running(agent_id);
        }
        for (_, pending) in self.pending_tools.drain() {
            if let Some(entry_id) = pending.entry_id {
                scrollback.finish_running(entry_id);
            }
        }
        if self.skip_next_user_echo {
            self.skip_next_user_echo = false;
            if text.contains("<command-name>") {
                self.skip_next_skill_body = true;
            }
            let prompt_index = chunk
                .meta
                .as_ref()
                .and_then(|m| m.get(user_message_chunk_meta::PROMPT_INDEX))
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            if let Some(pi) = prompt_index {
                for idx in (0..scrollback.len()).rev() {
                    if let Some(entry) = scrollback.get_mut(idx)
                        && let RenderBlock::UserPrompt(ref mut block) = entry.block
                    {
                        if block.is_interjection {
                            continue;
                        }
                        if block.prompt_index.is_none() {
                            block.prompt_index = Some(pi);
                        }
                        break;
                    }
                }
            }
            return false;
        }
        let prompt_index = chunk
            .meta
            .as_ref()
            .and_then(|m| m.get(user_message_chunk_meta::PROMPT_INDEX))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        if let Some(segments) = combined_display_texts_from_chunk(&chunk) {
            let mut last_id = None;
            for seg in &segments {
                last_id = Some(scrollback.push_block(RenderBlock::UserPrompt(
                    crate::scrollback::blocks::UserPromptBlock::new(seg.clone()),
                )));
            }
            if let (Some(pi), Some(id)) = (prompt_index, last_id)
                && let Some(entry) = scrollback.get_by_id_mut(id)
                && let RenderBlock::UserPrompt(ref mut block) = entry.block
            {
                block.prompt_index = Some(pi);
            }
            return true;
        }
        let display_override = match &chunk.content {
            acp::ContentBlock::Text(t) => t
                .meta
                .as_ref()
                .and_then(|m| m.get(user_prompt_meta::DISPLAY_TEXT))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            _ => None,
        };
        let skill_token_ranges = match &chunk.content {
            acp::ContentBlock::Text(t) => t
                .meta
                .as_ref()
                .and_then(|m| m.get(user_prompt_meta::SKILL_TOKEN_RANGES))
                .map(parse_skill_token_ranges)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut block = if let Some(dt) = display_override {
            if text.contains("<command-name>") {
                self.skip_next_skill_body = true;
            }
            let (as_skill, as_cron) = match &chunk.content {
                acp::ContentBlock::Text(t) => {
                    let m = t.meta.as_ref();
                    let skill = m
                        .and_then(|m| m.get(user_prompt_meta::DISPLAY_AS_SKILL))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let cron = m
                        .and_then(|m| m.get(user_prompt_meta::DISPLAY_AS_CRON))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    (skill, cron)
                }
                _ => (false, false),
            };
            if as_cron {
                crate::scrollback::blocks::UserPromptBlock::cron(dt)
            } else if as_skill {
                crate::scrollback::blocks::UserPromptBlock::skill(dt)
            } else {
                crate::scrollback::blocks::UserPromptBlock::new(dt)
            }
        } else if !skill_token_ranges.is_empty() {
            crate::scrollback::blocks::UserPromptBlock::with_skill_tokens(text, skill_token_ranges)
        } else {
            let skill_display =
                xai_grok_tools::implementations::skills::skill::extract_skill_display_text(&text);
            if let Some(display_text) = skill_display {
                self.skip_next_skill_body = true;
                crate::scrollback::blocks::UserPromptBlock::skill(display_text)
            } else if text.starts_with('/') && !text.starts_with("//") {
                crate::scrollback::blocks::UserPromptBlock::skill(text)
            } else if let Some(cmd) = extract_skill_header_command(&text) {
                crate::scrollback::blocks::UserPromptBlock::new(cmd)
            } else if let Some(prompt) = extract_cron_prompt_body(&text) {
                crate::scrollback::blocks::UserPromptBlock::cron(prompt)
            } else if user_message_hidden_from_scrollback(&chunk, meta, &text) {
                return false;
            } else {
                crate::scrollback::blocks::UserPromptBlock::new(text)
            }
        };
        block.prompt_index = prompt_index;
        let entry_id = scrollback.push_block(RenderBlock::UserPrompt(block));
        let ts_ms = meta.turn_start_ms.or(meta.agent_timestamp_ms);
        if let Some(ms) = ts_ms
            && let Some(entry) = scrollback.get_by_id_mut(entry_id)
        {
            entry.created_at = Some(utc_ms_to_local(ms));
        }
        true
    }
}
/// Per-prompt display strings from combine ([`user_prompt_meta::COMBINED_DISPLAY_TEXTS`]).
fn combined_display_texts_from_chunk(chunk: &acp::ContentChunk) -> Option<Vec<String>> {
    let acp::ContentBlock::Text(t) = &chunk.content else {
        return None;
    };
    let arr = t
        .meta
        .as_ref()?
        .get(user_prompt_meta::COMBINED_DISPLAY_TEXTS)?
        .as_array()?;
    let segs: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect();
    (segs.len() >= 2).then_some(segs)
}
/// Parse `skillTokenRanges` content-block meta (`[[start, end], …]`) into
/// byte ranges. Malformed entries are skipped; bounds/boundary validation
/// happens in `UserPromptBlock::with_skill_tokens`.
fn parse_skill_token_ranges(v: &serde_json::Value) -> Vec<std::ops::Range<usize>> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|pair| {
                    let pair = pair.as_array()?;
                    let start = pair.first()?.as_u64()? as usize;
                    let end = pair.get(1)?.as_u64()? as usize;
                    Some(start..end)
                })
                .collect()
        })
        .unwrap_or_default()
}
/// Extract a slash command name from a skill instruction markdown header.
///
/// Matches text starting with `# /command -- ` (the format used by
/// `InjectSkill`). Returns the `## Input` section's content if present,
/// prefixed with the command name. Falls back to just the command name.
///
/// Example: `"# /loop -- schedule a recurring prompt\n\n...\n## Input\n5m check deploy"`
/// → `"/loop 5m check deploy"`
fn extract_skill_header_command(text: &str) -> Option<String> {
    let text = text.strip_prefix("# ")?;
    if !text.starts_with('/') {
        return None;
    }
    let cmd_name = text.split(&[' ', '\n'][..]).next()?;
    if let Some(input_idx) = text.find("## Input\n") {
        let args = text[input_idx + "## Input\n".len()..].trim();
        if !args.is_empty() {
            return Some(format!("{cmd_name} {args}"));
        }
    }
    Some(cmd_name.to_string())
}
/// Whether a `UserMessageChunk` must stay out of scrollback.
///
/// Type-driven (preferred):
/// 1. `ContentChunk._meta.hideFromScrollback` stamped by the shell from
///    [`PromptOrigin::hide_user_echo_from_scrollback`]
/// 2. `SessionNotification._meta.promptId` classified via
///    [`PromptOrigin::from_prompt_id`]
///
/// Legacy fallback (pre-meta sessions only): bare auto-wake text that used to
/// be gated by the system-reminder prefix. Cron is handled earlier by
/// [`extract_cron_prompt_body`].
fn user_message_hidden_from_scrollback(
    chunk: &acp::ContentChunk,
    meta: &NotificationMeta,
    text: &str,
) -> bool {
    if chunk
        .meta
        .as_ref()
        .and_then(|m| m.get(user_message_chunk_meta::HIDE_FROM_SCROLLBACK))
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        return true;
    }
    if let Some(pid) = meta.prompt_id.as_deref()
        && xai_grok_shell::session::PromptOrigin::from_prompt_id(pid)
            .hide_user_echo_from_scrollback()
    {
        return true;
    }
    let t = text.trim_start();
    t.starts_with("<system-reminder>")
        || t.starts_with("<monitor-event")
        || t.trim() == "---"
        || t.lines().next().is_some_and(|first| {
            first.starts_with(|c: char| c.is_ascii_digit())
                && first.contains(" monitor events from ")
                && first.contains(" (use ")
        })
}
/// Extract the user's prompt from `<system-reminder>` cron framing.
///
/// Matches the format produced by `format_scheduled_task_prompt`:
/// `"<system-reminder>\nThis is a scheduled task execution...\n</system-reminder>\n\n<prompt>"`
///
/// Returns the prompt text after the closing tag, or `None` if the text
/// doesn't match the cron framing pattern.
fn extract_cron_prompt_body(text: &str) -> Option<String> {
    if !text.starts_with("<system-reminder>") {
        return None;
    }
    let end_tag = "</system-reminder>";
    let close = text.find(end_tag)?;
    let header = &text[..close];
    if !header.contains("scheduled task execution") {
        return None;
    }
    let body = text[close + end_tag.len()..].trim();
    if body.is_empty() {
        return None;
    }
    Some(body.to_string())
}
/// Merge ToolCallUpdate fields with the base ToolCall.
/// Update fields take precedence when present.
fn merge_tool_call_update(base: acp::ToolCall, update: acp::ToolCallUpdate) -> acp::ToolCall {
    acp::ToolCall::new(
        update.tool_call_id,
        update.fields.title.unwrap_or(base.title),
    )
    .kind(update.fields.kind.unwrap_or(base.kind))
    .status(update.fields.status.unwrap_or(base.status))
    .content(update.fields.content.unwrap_or(base.content))
    .raw_input(update.fields.raw_input.or(base.raw_input))
    .raw_output(update.fields.raw_output.or(base.raw_output))
    .locations(update.fields.locations.unwrap_or(base.locations))
    .meta(base.meta)
}
/// Peeled display form when a redundant leading `cd <cwd>` was stripped, else None.
fn peeled_if_changed(command: &str, session_cwd: Option<&Path>) -> Option<String> {
    let cwd = session_cwd?;
    let stripped = strip_redundant_session_cd(command, cwd);
    (stripped.as_ref() != command).then(|| stripped.into_owned())
}
/// True when `s` is an ACP/function tool id rather than a shell command.
///
/// Eager ToolCall messages often set `title` to the function name
/// (`run_terminal_command`) before `raw_input.command` arrives — using that as
/// the execute header flashes the internal tool name in the TUI.
fn is_execute_tool_function_name(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "run_terminal_command"
            | "run_terminal_cmd"
            | "bash"
            | "shell"
            | "execute"
            | "run_command"
            | "terminal"
    )
}
/// Eager execute-related placeholder that should not be shown to the user.
///
/// Only **empty** execute commands count as placeholders. A real shell
/// invocation of `bash` / `shell` / etc. must not be dropped on late
/// `is_background` (would lose demotion + stdout). Other blocks still
/// matching the tool function name are placeholders.
fn entry_is_execute_placeholder(entry: &crate::scrollback::entry::ScrollbackEntry) -> bool {
    match &entry.block {
        RenderBlock::ToolCall(ToolCallBlock::Execute(ex)) => ex.command.trim().is_empty(),
        RenderBlock::ToolCall(ToolCallBlock::Other(o)) => is_execute_tool_function_name(&o.name),
        _ => false,
    }
}
/// Non-empty `raw_input.command` if present (empty / whitespace treated as missing).
fn raw_input_command(tc: &acp::ToolCall) -> Option<String> {
    extract_raw_field(tc, "command").and_then(|c| {
        let t = c.trim();
        if t.is_empty() { None } else { Some(c) }
    })
}
/// Resolve the shell command for an execute tool call.
///
/// Prefer `raw_input.command`. Do **not** fall back to a title that is only the
/// tool function name (that produces the "Run run_terminal_command" flash).
fn execute_command_from_tool_call(tc: &acp::ToolCall) -> String {
    if let Some(cmd) = raw_input_command(tc) {
        return cmd;
    }
    if !tc.title.is_empty() && !is_execute_tool_function_name(&tc.title) {
        return tc.title.clone();
    }
    String::new()
}
/// Convert an ACP ToolCall to a RenderBlock.
///
/// Parses `tool_call.kind` to create the appropriate block type,
/// extracting fields from `raw_input` JSON when available. `session_cwd` sets
/// execute `header_display` when a leading `cd <cwd>` is redundant.
fn tool_call_to_block(tc: &acp::ToolCall, session_cwd: Option<&Path>) -> RenderBlock {
    let success = !matches!(tc.status, acp::ToolCallStatus::Failed);
    match tc.kind {
        acp::ToolKind::Execute => {
            let command = execute_command_from_tool_call(tc);
            let header_display = peeled_if_changed(&command, session_cwd);
            let description = extract_raw_field(tc, "description");
            let is_bash_mode = tc
                .meta
                .as_ref()
                .and_then(|m| m.get("bash_mode"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(bash) = extract_bash_output_from_value(&tc.raw_output) {
                let output_str = String::from_utf8_lossy(&bash.output);
                let mut block = ExecuteToolCallBlock::new(command).with_output(output_str.as_ref());
                block.bash_mode = is_bash_mode;
                block.header_display = header_display;
                if let Some(desc) = description {
                    block = block.with_description(desc);
                }
                if !success || bash.exit_code != 0 {
                    let error_msg = if let Some(sig) = &bash.signal {
                        sig.clone()
                    } else if bash.exit_code != 0 {
                        format!("exit code {}", bash.exit_code)
                    } else {
                        "Command failed".into()
                    };
                    block = block.with_error(error_msg);
                }
                RenderBlock::ToolCall(ToolCallBlock::Execute(block))
            } else {
                let mut block = ExecuteToolCallBlock::new(command);
                block.bash_mode = is_bash_mode;
                block.header_display = header_display;
                if let Some(desc) = description {
                    block = block.with_description(desc);
                }
                if !success {
                    let text = content_text(tc);
                    let error_msg = if text.is_empty() {
                        "Command failed".to_string()
                    } else {
                        text
                    };
                    block = block.with_error(error_msg);
                }
                RenderBlock::ToolCall(ToolCallBlock::Execute(block))
            }
        }
        acp::ToolKind::Read => {
            let path = extract_raw_field(tc, "file_path")
                .or_else(|| extract_raw_field(tc, "target_file"))
                .or_else(|| extract_raw_field(tc, "path"))
                .unwrap_or_else(|| tc.title.clone());
            let mut block = ReadToolCallBlock::new(&path);
            if let Some(ref raw) = tc.raw_output
                && let Ok(ToolOutput::ReadFile(read_output)) =
                    serde_json::from_value::<ToolOutput>(raw.clone())
            {
                match read_output {
                    ReadFileOutput::FileContent(fc) => {
                        if fc.offset.is_some() || fc.limit.is_some() {
                            let off = fc.offset.unwrap_or(0);
                            let start = off + 1;
                            let end = fc
                                .limit
                                .map_or(fc.total_lines, |lim| (off + lim).min(fc.total_lines));
                            block = block.with_line_range(LineRange::new(start, end));
                        }
                        block = block.with_content(fc.raw_output, fc.total_lines);
                    }
                    ReadFileOutput::FileNotFound(msg)
                    | ReadFileOutput::IsADirectory(msg)
                    | ReadFileOutput::PermissionDenied(msg)
                    | ReadFileOutput::FileTooLarge(msg)
                    | ReadFileOutput::FileReadError(msg)
                    | ReadFileOutput::ImageSizeError(msg) => {
                        block = block.with_error(msg);
                    }
                    ReadFileOutput::ImageContent(_) => {
                        block.media_kind = Some(ReadMediaKind::Image);
                        block.image_ref =
                            crate::prompt_images::ScrollbackImageRef::from_path(&path);
                    }
                    ReadFileOutput::PdfPageImages(pdf) => {
                        block.media_kind = Some(ReadMediaKind::Pdf {
                            pages: pdf.total_pages,
                        });
                    }
                }
            } else if !success {
                let text = content_text(tc);
                block = block.with_error(if text.is_empty() {
                    "Read failed".to_string()
                } else {
                    text
                });
            }
            RenderBlock::ToolCall(ToolCallBlock::Read(block))
        }
        acp::ToolKind::Edit => {
            let raw_path = extract_raw_field(tc, "file_path")
                .or_else(|| extract_raw_field(tc, "filePath"))
                .or_else(|| extract_raw_field(tc, "target_file"))
                .or_else(|| extract_raw_field(tc, "path"));
            let path_from_title = raw_path.is_none();
            let path = raw_path.unwrap_or_else(|| tc.title.clone());
            let untrusted_summary = path_from_title
                || tc
                    .content
                    .iter()
                    .filter(|c| matches!(c, acp::ToolCallContent::Diff(_)))
                    .count()
                    > 1;
            let is_write = is_write_tool(tc);
            let mut block = if success {
                let (hunks, _count) = crate::diff::extract_edit_hunks(tc);
                EditToolCallBlock::new(path, hunks)
            } else {
                let error_msg = extract_edit_error(tc);
                EditToolCallBlock::new(path, vec![]).with_error(error_msg)
            };
            if untrusted_summary {
                block = block.with_untrusted_summary();
            }
            if is_write {
                block = block.with_prefix("Creating ");
            }
            RenderBlock::ToolCall(ToolCallBlock::Edit(block))
        }
        acp::ToolKind::Search
            if matches!(
                extract_raw_field(tc, "variant").as_deref(),
                Some("WebSearch") | Some("XSearch")
            ) || tc.title.starts_with("Web search:")
                || tc.title.starts_with("X search:") =>
        {
            let is_backend = extract_raw_field(tc, "backend").as_deref() == Some("true")
                || tc
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("backend"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            let query = extract_raw_field(tc, "query")
                .or_else(|| {
                    tc.title
                        .strip_prefix("Web search: ")
                        .map(|q| q.trim_matches('"').to_owned())
                })
                .unwrap_or_else(|| {
                    if is_backend {
                        String::new()
                    } else {
                        tc.title.clone()
                    }
                });
            let mut block = WebSearchToolCallBlock::new(query);
            if is_backend {
                let variant = extract_raw_field(tc, "variant").unwrap_or_default();
                if variant == "XSearch" {
                    block.label = Some("X Search ".to_string());
                    block.is_x_search = true;
                }
                if let Some(ref raw) = tc.raw_output {
                    if variant == "XSearch" && raw.get("name").is_some() {
                        let tool_name = raw
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("x_search");
                        let short_type = match tool_name {
                            "x_keyword_search" => "keyword",
                            "x_semantic_search" => "semantic",
                            "x_user_search" => "users",
                            "x_thread_fetch" => "thread",
                            other => other,
                        };
                        let input_str = raw.get("input").and_then(|v| v.as_str()).unwrap_or("{}");
                        let query_text = serde_json::from_str::<serde_json::Value>(input_str)
                            .ok()
                            .and_then(|v| {
                                v.get("query").and_then(|q| q.as_str()).map(String::from)
                            });
                        if let Some(ref q) = query_text {
                            block.query = format!("{short_type}({q})");
                        } else {
                            block.query = short_type.to_string();
                        }
                    } else if variant == "WebSearch" && raw.pointer("/action/type").is_some() {
                        let action_type = raw
                            .pointer("/action/type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        match action_type {
                            "search" => {
                                if let Some(q) =
                                    raw.pointer("/action/query").and_then(|v| v.as_str())
                                {
                                    block.query = q.to_string();
                                }
                                if let Some(arr) =
                                    raw.pointer("/action/sources").and_then(|v| v.as_array())
                                {
                                    block.citations = arr
                                        .iter()
                                        .filter_map(|s| s.get("url").and_then(|u| u.as_str()))
                                        .map(|u| u.to_string())
                                        .collect();
                                }
                            }
                            "open_page" => {
                                if let Some(url) =
                                    raw.pointer("/action/url").and_then(|v| v.as_str())
                                {
                                    block.query = format!("open {url}");
                                    block.citations = vec![url.to_string()];
                                }
                            }
                            "find" | "find_in_page" => {
                                let pattern = raw
                                    .pointer("/action/pattern")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let url = raw
                                    .pointer("/action/url")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                block.query = format!("find \"{pattern}\"");
                                if !url.is_empty() {
                                    block.citations = vec![url.to_string()];
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !block.citations.is_empty() {
                    let sources_list: Vec<String> = block
                        .citations
                        .iter()
                        .enumerate()
                        .map(|(i, url)| format!("{}. {}", i + 1, url))
                        .collect();
                    block.content = Some(sources_list.join("\n"));
                }
            } else {
                if let Some(ref raw) = tc.raw_output
                    && let Ok(ToolOutput::WebSearch(ws)) =
                        serde_json::from_value::<ToolOutput>(raw.clone())
                {
                    if !ws.content.is_empty() {
                        block.content = Some(ws.content);
                    }
                    block.citations = ws.citations;
                }
                if block.content.is_none() {
                    let text = content_text(tc);
                    if !text.is_empty() {
                        block.content = Some(text);
                    }
                }
            }
            if !success {
                block = block.with_error("Web search failed");
            }
            RenderBlock::ToolCall(ToolCallBlock::WebSearch(block))
        }
        acp::ToolKind::Search => {
            let pattern = extract_raw_field(tc, "pattern")
                .or_else(|| extract_raw_field(tc, "glob_pattern"))
                .unwrap_or_else(|| tc.title.clone());
            let meta = extract_search_meta(tc);
            let grep = extract_grep_output(&tc.raw_output).unwrap_or_default();
            let mut block = SearchToolCallBlock::new(pattern);
            block.meta = meta;
            block.match_count = grep.match_count;
            block.file_matches = grep.file_matches;
            block.file_paths = grep.file_paths;
            if !success {
                block.error = Some("Search failed".into());
            }
            RenderBlock::ToolCall(ToolCallBlock::Search(block))
        }
        acp::ToolKind::Fetch => {
            let url = extract_raw_field(tc, "url")
                .or_else(|| tc.title.strip_prefix("Fetch: ").map(str::to_owned))
                .unwrap_or_else(|| tc.title.clone());
            let mut block = WebFetchToolCallBlock::new(url);
            if let Some(ref raw) = tc.raw_output
                && let Ok(ToolOutput::WebFetch(WebFetchOutput::Content(content))) =
                    serde_json::from_value::<ToolOutput>(raw.clone())
            {
                block.status_code = Some(content.status_code);
                block.content_type = Some(content.content_type);
                block.bytes = Some(content.bytes);
            }
            let text = content_text(tc);
            if !text.is_empty() {
                block.output = Some(text);
            }
            if !success {
                block = block.with_error("Fetch failed");
            }
            RenderBlock::ToolCall(ToolCallBlock::WebFetch(block))
        }
        _ if extract_raw_field(tc, "target_directory").is_some() => {
            let path = extract_raw_field(tc, "target_directory").unwrap();
            let mut block = ListDirToolCallBlock::new(make_relative_path(&path));
            if let Some(content) = extract_listdir_content(&tc.raw_output) {
                block = block.with_output(content);
            }
            if !success {
                block = block.with_error("List directory failed");
            }
            RenderBlock::ToolCall(ToolCallBlock::ListDir(block))
        }
        _ if extract_raw_field(tc, "variant").as_deref() == Some("SearchTool") => {
            let query = extract_raw_field(tc, "query").unwrap_or_default();
            let mut block = IntegrationSearchToolCallBlock::new(query);
            block.limit = tc
                .raw_input
                .as_ref()
                .and_then(|v| v.get("limit"))
                .and_then(|v| v.as_u64())
                .map(|n| n as u8);
            if let Some(ref raw) = tc.raw_output
                && let Ok(ToolOutput::SearchTool(SearchToolOutput {
                    result_count,
                    content,
                })) = serde_json::from_value::<ToolOutput>(raw.clone())
            {
                block.result_count = result_count;
                block.results = parse_search_tool_results(&content);
                block.content = Some(content);
            }
            if !success {
                block = block.with_error("Search failed");
            }
            RenderBlock::ToolCall(ToolCallBlock::IntegrationSearch(block))
        }
        _ if extract_raw_field(tc, "variant").as_deref() == Some("UseTool") => {
            let tool_name = extract_raw_field(tc, "tool_name").unwrap_or_else(|| tc.title.clone());
            let mut block = UseToolCallBlock::new(tool_name);
            block.input_args = extract_use_tool_args(tc);
            let text = content_text(tc);
            if !text.is_empty() {
                block.output = Some(text);
            } else if let Some(extracted) = extract_use_tool_output(&tc.raw_output) {
                block.output = Some(extracted);
            }
            if !success {
                block.error = Some(
                    block
                        .output
                        .take()
                        .unwrap_or_else(|| "Tool call failed".into()),
                );
            }
            RenderBlock::ToolCall(ToolCallBlock::UseTool(block))
        }
        _ if matches!(
            extract_raw_field(tc, "variant").as_deref(),
            Some("ImageGen") | Some("ImageToVideo") | Some("ReferenceToVideo") | Some("ImageEdit")
        ) =>
        {
            media_gen_block(tc, success)
        }
        _ if tc.title.starts_with("Memory search:") => {
            let query = tc
                .title
                .strip_prefix("Memory search: ")
                .map(|q| q.trim_matches('"').to_owned())
                .unwrap_or_else(|| tc.title.clone());
            let mut block = MemorySearchToolCallBlock::new(query);
            let text = content_text(tc);
            if !text.is_empty() {
                block.results =
                    crate::scrollback::blocks::tool::memory_search::parse_memory_results(&text);
            }
            if !success {
                block.error = Some("Memory search failed".into());
            }
            RenderBlock::ToolCall(ToolCallBlock::MemorySearch(block))
        }
        _ => {
            if is_execute_tool_function_name(&tc.title) {
                let command = execute_command_from_tool_call(tc);
                let header_display = peeled_if_changed(&command, session_cwd);
                if let Some(bash) = extract_bash_output_from_value(&tc.raw_output) {
                    let output_str = String::from_utf8_lossy(&bash.output);
                    let mut block =
                        ExecuteToolCallBlock::new(command).with_output(output_str.as_ref());
                    block.header_display = header_display;
                    if let Some(desc) = extract_raw_field(tc, "description") {
                        block = block.with_description(desc);
                    }
                    if !success || bash.exit_code != 0 {
                        let error_msg = if let Some(sig) = &bash.signal {
                            sig.clone()
                        } else if bash.exit_code != 0 {
                            format!("exit code {}", bash.exit_code)
                        } else {
                            "Command failed".into()
                        };
                        block = block.with_error(error_msg);
                    }
                    return RenderBlock::ToolCall(ToolCallBlock::Execute(block));
                }
                let mut block = ExecuteToolCallBlock::new(command);
                block.header_display = header_display;
                if let Some(desc) = extract_raw_field(tc, "description") {
                    block = block.with_description(desc);
                }
                if !success {
                    let text = content_text(tc);
                    block = block.with_error(if text.is_empty() {
                        "Command failed".to_string()
                    } else {
                        text
                    });
                }
                return RenderBlock::ToolCall(ToolCallBlock::Execute(block));
            }
            let name = tool_call_title(tc);
            let summary = if tc.title.is_empty() {
                extract_raw_field(tc, "path")
                    .or_else(|| extract_raw_field(tc, "url"))
                    .or_else(|| extract_raw_field(tc, "query"))
                    .unwrap_or_default()
            } else if tc.kind == acp::ToolKind::Other {
                String::new()
            } else {
                format!("{:?}", tc.kind).to_lowercase()
            };
            let (label, ctor): (String, fn(OtherToolCallBlock) -> ToolCallBlock) = if name
                .eq_ignore_ascii_case("skill")
                || name.to_ascii_lowercase().starts_with("skill:")
            {
                let label = match name.find(':') {
                    Some(i) => format!("Skill{}", &name[i..]),
                    None => "Skill".into(),
                };
                (label, ToolCallBlock::Skill)
            } else {
                (name.into_owned(), ToolCallBlock::Other)
            };
            let mut block = OtherToolCallBlock::new(label, summary);
            let ct = content_text(tc);
            if !success {
                block.error = Some(if ct.is_empty() {
                    "Failed".into()
                } else {
                    ct.clone()
                });
            }
            if !ct.is_empty() {
                block.set_output_text(ct);
            }
            RenderBlock::ToolCall(ctor(block))
        }
    }
}
/// Display title for a tool call: its title, or the kind name when empty.
fn tool_call_title(tc: &acp::ToolCall) -> Cow<'_, str> {
    if tc.title.is_empty() {
        Cow::Owned(format!("{:?}", tc.kind))
    } else {
        Cow::Borrowed(&tc.title)
    }
}
/// Build the media block from the typed `raw_output` path.
fn media_gen_block(tc: &acp::ToolCall, success: bool) -> RenderBlock {
    let mut block = OtherToolCallBlock::new(tool_call_title(tc), String::new());
    if !success {
        let err = content_text(tc);
        block.error = Some(if err.is_empty() { "Failed".into() } else { err });
    } else if let Some((path, is_video)) = media_gen_ref(tc) {
        block = block.with_media_ref(path, is_video);
    } else if let Some(text) = media_gen_text(tc) {
        block.set_output_text(text);
    }
    RenderBlock::ToolCall(ToolCallBlock::Other(block))
}
/// Plain-text body of a media-variant tool that returned `ToolOutput::Text`
/// rather than a media file (the free / X Basic SuperGrok-upsell short-circuit).
/// `None` for real media outputs — including ZDR upload-only results — so their
/// typed rendering is untouched.
fn media_gen_text(tc: &acp::ToolCall) -> Option<String> {
    match serde_json::from_value::<ToolOutput>(tc.raw_output.clone()?).ok()? {
        ToolOutput::Text(t) => (!t.text.is_empty()).then_some(t.text),
        _ => None,
    }
}
/// Local `(path, is_video)` from typed `raw_output`.
///
/// Returns `None` when `raw_output` is missing/unparseable, not a media
/// variant, or has no openable local file (ZDR `uploaded_url` / empty path).
fn media_gen_ref(tc: &acp::ToolCall) -> Option<(std::path::PathBuf, bool)> {
    let (media, is_video) =
        match serde_json::from_value::<ToolOutput>(tc.raw_output.clone()?).ok()? {
            ToolOutput::ImageGen(m) | ToolOutput::ImageEdit(m) => (m, false),
            ToolOutput::ImageToVideo(m) | ToolOutput::ReferenceToVideo(m) => (m, true),
            _ => return None,
        };
    if media.uploaded_url.is_some() || media.path.as_os_str().is_empty() {
        return None;
    }
    Some((media.path, is_video))
}
/// Extract text content from a ContentBlock.
fn extract_text_from_content(content: &acp::ContentBlock) -> String {
    match content {
        acp::ContentBlock::Text(t) => t.text.clone(),
        _ => String::new(),
    }
}
/// Extract text from tool call content blocks.
fn content_text(tc: &acp::ToolCall) -> String {
    tc.content
        .iter()
        .filter_map(|c| match c {
            acp::ToolCallContent::Content(acp::Content {
                content: acp::ContentBlock::Text(t),
                ..
            }) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
/// Check if a tool call is bg-task internal plumbing
/// (get_command_or_subagent_output, kill_command_or_subagent,
/// wait_commands_or_subagents, and the external background-await tool).
///
/// These are suppressed from scrollback because the bg task pane provides
/// visibility into task status and output.
fn is_bg_plumbing_tool(tc: &acp::ToolCall) -> bool {
    matches!(
        tc.title.as_str(),
        "get_command_or_subagent_output"
            | "kill_command_or_subagent"
            | "wait_commands_or_subagents"
            | "get_task_output"
            | "kill_task"
            | "wait_tasks"
            | "get_task_or_subagent_output"
            | "kill_task_or_subagent"
            | "wait_tasks_or_subagents"
            | "AwaitShell"
            | "Await"
    ) || tc.title.starts_with("Await:")
        || tc.title.starts_with("Sleep ")
        || tc.title.starts_with("Wait tasks:")
        || tc.title.starts_with("Kill task:")
        || tc
            .raw_input
            .as_ref()
            .and_then(|v| v.get("variant"))
            .and_then(|v| v.as_str())
            .is_some_and(|v| matches!(v, "TaskOutput" | "KillTask" | "WaitTasks"))
}
/// Classify a *blocking* suppressed tool into the [`WaitingReason`] the turn is
/// waiting on, or `None` for suppressed tools that don't block the turn (e.g.
/// `kill_*`, todo/goal/scheduler). Mirrors the title/variant matches in
/// [`is_bg_plumbing_tool`] so the spinner can name the wait instead of falling
/// back to a generic "Waiting…".
fn blocking_wait_reason(tc: &acp::ToolCall) -> Option<WaitingReason> {
    let title = tc.title.as_str();
    let variant = tc
        .raw_input
        .as_ref()
        .and_then(|v| v.get("variant"))
        .and_then(|v| v.as_str());
    if matches!(
        title,
        "get_command_or_subagent_output" | "get_task_output" | "get_task_or_subagent_output"
    ) || variant == Some("TaskOutput")
    {
        let task_ids = tc
            .raw_input
            .as_ref()
            .map(task_ids_from_raw_input)
            .unwrap_or_default();
        return Some(WaitingReason::TaskOutput {
            task_ids,
            subject: None,
            waits: timeout_waits(tc.raw_input.as_ref()),
        });
    }
    if matches!(
        title,
        "wait_commands_or_subagents" | "wait_tasks" | "wait_tasks_or_subagents"
    ) || title.starts_with("Wait tasks:")
        || variant == Some("WaitTasks")
    {
        return Some(WaitingReason::TasksComplete);
    }
    if matches!(title, "Await" | "AwaitShell")
        || title.starts_with("Await:")
        || title.starts_with("Sleep ")
    {
        return Some(WaitingReason::Sleep);
    }
    None
}
/// Whether the wait tool call actually blocks: `timeout_ms > 0` in raw_input.
/// Missing input / missing field / 0 all mean an instant poll.
fn timeout_waits(raw: Option<&serde_json::Value>) -> bool {
    raw.and_then(|v| v.get("timeout_ms"))
        .and_then(|v| v.as_u64())
        .is_some_and(|t| t > 0)
}
/// Extract `task_ids` from a `get_task_output` / wait tool's raw_input JSON.
fn task_ids_from_raw_input(raw: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    if let Some(arr) = raw.get("task_ids").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(id) = v.as_str() {
                let id = id.trim();
                if !id.is_empty() && seen.insert(id.to_string()) {
                    out.push(id.to_string());
                }
            }
        }
    }
    if out.is_empty()
        && let Some(id) = raw.get("task_id").and_then(|v| v.as_str())
    {
        let id = id.trim();
        if !id.is_empty() {
            out.push(id.to_string());
        }
    }
    out
}
/// Check if a tool call is a background execute (`is_background=true`).
///
/// These are deferred from scrollback — the `x.ai/task_backgrounded`
/// notification creates a `BgTask` block instead of an `Execute` block.
///
/// Eager ACP messages often use `kind=Other` with `title=run_terminal_command`
/// before the kind is refined to Execute — still treat those as execute tools
/// when `raw_input` requests background so we don't flash the function name.
fn is_bg_tool(tc: &acp::ToolCall) -> bool {
    let looks_like_execute =
        tc.kind == acp::ToolKind::Execute || is_execute_tool_function_name(&tc.title);
    looks_like_execute
        && tc
            .raw_input
            .as_ref()
            .and_then(|v| v.get("is_background").or_else(|| v.get("background")))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
}
/// Check if an Edit-kind tool call is a whole-file write (write)
/// rather than a targeted replacement (search_replace / edit).
///
/// Detection: a Write-family `rawInput.variant` tag.
fn is_write_tool(tc: &acp::ToolCall) -> bool {
    is_write_variant(
        tc.raw_input
            .as_ref()
            .and_then(|v| v.get("variant"))
            .and_then(|v| v.as_str()),
    )
}
/// Extract the serde variant tag from a tool call's `raw_input.variant`.
///
/// Shared helper for all `is_*_tool` suppression checks — avoids
/// duplicating the `.as_ref()?.get("variant")?.as_str()` chain.
fn extract_variant(tc: &acp::ToolCall) -> Option<&str> {
    tc.raw_input.as_ref()?.get("variant")?.as_str()
}
/// Twin without the optional-toolset spelling.
fn is_task_variant(variant: Option<&str>) -> bool {
    matches!(variant, Some("Task"))
}
/// Twin without the optional-toolset spelling.
fn is_write_variant(variant: Option<&str>) -> bool {
    matches!(variant, Some("Write"))
}
/// Twin without the optional-toolset spelling.
fn is_todo_variant(variant: Option<&str>) -> bool {
    matches!(variant, Some("TodoWrite"))
}
/// Check if a tool call is a todo-related tool.
///
/// Suppressed from scrollback because the dedicated todo pane provides
/// better visibility. Covers the `todo_write` / `TodoWrite` ids, the
/// `Updating plan` title, and TodoWrite-family variant tags.
fn is_todo_tool(tc: &acp::ToolCall) -> bool {
    matches!(
        tc.title.as_str(),
        "todo_write" | "TodoWrite" | "Updating plan"
    ) || is_todo_variant(extract_variant(tc))
}
/// Check if a tool call is a task tool (subagent spawn).
///
/// Suppressed from scrollback because the SubagentBlock (created from
/// SubagentSpawned notification) provides better visibility. Covers the
/// `task` / `Task` / `spawn_subagent` ids and Task-family variant tags.
fn is_task_tool(tc: &acp::ToolCall) -> bool {
    matches!(tc.title.as_str(), "task" | "Task" | "spawn_subagent")
        || is_task_variant(extract_variant(tc))
}
fn is_goal_tool(tc: &acp::ToolCall) -> bool {
    tc.title == "update_goal"
        || tc.title.starts_with("Goal:")
        || matches!(extract_variant(tc), Some("UpdateGoal" | "WorkflowSignal"))
}
fn is_workflow_tool(tc: &acp::ToolCall) -> bool {
    let is_workflow = tc.title == "workflow" || matches!(extract_variant(tc), Some("Workflow"));
    if !is_workflow {
        return false;
    }
    let validate_only = tc.title.starts_with("Validating workflow")
        || tc
            .raw_input
            .as_ref()
            .and_then(|v| v.get("validate_only"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    !validate_only
}
/// Check if a tool call is a scheduler tool (scheduler_create/delete/list).
///
/// Suppressed from scrollback because the tasks pane provides visibility.
/// Uses convention-based prefixes rather than exhaustive names.
fn is_scheduler_tool(tc: &acp::ToolCall) -> bool {
    tc.title.starts_with("scheduler_")
        || extract_variant(tc).is_some_and(|v| v.starts_with("Scheduler"))
}
/// Extract a string field from raw_input JSON.
fn extract_raw_field(tc: &acp::ToolCall, field: &str) -> Option<String> {
    tc.raw_input
        .as_ref()
        .and_then(|v| v.get(field))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}
/// Extract a short, user-friendly error label from a failed Edit tool call.
fn extract_edit_error(tc: &acp::ToolCall) -> String {
    use xai_grok_tools::types::output::SearchReplaceOutput;
    if let Some(ref raw) = tc.raw_output
        && let Ok(ToolOutput::SearchReplace(sr)) = serde_json::from_value::<ToolOutput>(raw.clone())
    {
        return match sr {
            SearchReplaceOutput::InvalidInput(_) => "Invalid input".to_owned(),
            SearchReplaceOutput::FileNotFound(_) => "File not found".to_owned(),
            SearchReplaceOutput::MultipleMatchesFound(_) => "Multiple matches found".to_owned(),
            SearchReplaceOutput::FileAlreadyExists(_) => "File already exists".to_owned(),
            SearchReplaceOutput::FilenameTooLong(_) => "Filename too long".to_owned(),
            SearchReplaceOutput::NoMatchesFound(_) => "No matches found".to_owned(),
            SearchReplaceOutput::EditsApplied(_) => "Edit failed".to_owned(),
        };
    }
    "Edit failed".to_owned()
}
/// Extract search input metadata from a tool call's rawInput.
fn extract_search_meta(tc: &acp::ToolCall) -> SearchInputMeta {
    let raw = match tc.raw_input.as_ref() {
        Some(v) => v,
        None => return SearchInputMeta::default(),
    };
    let str_field = |name: &str| -> Option<String> {
        raw.get(name)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    let bool_field =
        |name: &str| -> bool { raw.get(name).and_then(|v| v.as_bool()).unwrap_or(false) };
    let output_mode_str = raw.get("output_mode").and_then(|v| v.as_str());
    let path = str_field("path")
        .or_else(|| str_field("target_directory"))
        .map(|p| make_relative_path(&p))
        .filter(|p| p != ".");
    SearchInputMeta {
        path,
        glob: str_field("glob"),
        output_mode: SearchOutputMode::from_str_opt(output_mode_str),
        case_insensitive: bool_field("-i"),
        file_type: str_field("type"),
        multiline: bool_field("multiline"),
    }
}
/// Extract BashOutput from a serde_json::Value containing ToolOutput::Bash.
fn extract_bash_output_from_value(raw: &Option<serde_json::Value>) -> Option<BashOutput> {
    let val = raw.as_ref()?;
    match serde_json::from_value::<ToolOutput>(val.clone()) {
        Ok(ToolOutput::Bash(bash)) => Some(bash),
        _ => None,
    }
}
/// Extracted grep search results.
#[derive(Default)]
struct GrepResult {
    match_count: usize,
    file_matches: Vec<SearchFileMatch>,
    /// File paths only (for files_with_matches output mode).
    file_paths: Vec<String>,
}
/// Extract grep search results from raw_output.
fn extract_grep_output(raw: &Option<serde_json::Value>) -> Option<GrepResult> {
    let val = raw.as_ref()?;
    match serde_json::from_value::<ToolOutput>(val.clone()) {
        Ok(ToolOutput::GrepSearch(grep)) => {
            let file_matches: Vec<SearchFileMatch> = grep
                .file_matches
                .into_iter()
                .map(|fm| SearchFileMatch {
                    path: make_relative_path(&fm.path),
                    matches: fm
                        .matches
                        .into_iter()
                        .map(|m| SearchLineMatch {
                            line_number: m.line_number,
                            content: m.content,
                        })
                        .collect(),
                })
                .collect();
            let file_paths = if file_matches.is_empty() && grep.match_count > 0 {
                let stdout_str = String::from_utf8_lossy(&grep.stdout);
                parse_file_paths_from_stdout(&stdout_str)
            } else {
                vec![]
            };
            Some(GrepResult {
                match_count: grep.match_count,
                file_matches,
                file_paths,
            })
        }
        _ => None,
    }
}
/// Parse file paths from grep stdout in workspace_result XML format.
///
/// The stdout format is:
/// ```text
/// <workspace_result workspace_path="/path">
/// Found N files
/// /path/to/file1.rs
/// /path/to/file2.rs
/// </workspace_result>
/// ```
fn parse_file_paths_from_stdout(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('<') && !line.starts_with("Found "))
        .map(make_relative_path)
        .collect()
}
/// Extract directory listing content from rawOutput.
fn extract_listdir_content(raw: &Option<serde_json::Value>) -> Option<String> {
    let val = raw.as_ref()?;
    match serde_json::from_value::<ToolOutput>(val.clone()) {
        Ok(ToolOutput::ListDir(xai_grok_tools::types::output::ListDirOutput::Content(c))) => {
            Some(c.content)
        }
        _ => None,
    }
}
/// Extract the agent's advertised toolset from
/// `AvailableCommandsUpdate.meta`.
///
/// Wire format set by the shell: `{"tools": ["read_file", ...]}`.
/// Returns `None` if `meta` is absent, has no `tools` array, or the
/// array contains no string entries (defensive against future shape
/// drift). An empty `Vec` would mean "the shell told us there are zero
/// tools" -- pager `CommandRegistry::set_available_tools(empty)` then
/// hides every tool-gated command.
fn parse_tools_meta(meta: Option<&acp::Meta>) -> Option<Vec<String>> {
    let arr = meta?.get("tools")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
    )
}
/// Compact one-line description of a `SessionUpdate` for the always-on
/// `acp_update` log target.
///
/// Deliberately avoids serializing payloads: emits variant names, ids,
/// statuses, and *sizes* only, so the line stays O(100B) no matter how large
/// the update is. Full payloads go to the opt-in `acp_update_payload` target.
fn update_summary(update: &acp::SessionUpdate) -> String {
    match update {
        acp::SessionUpdate::UserMessageChunk(chunk) => {
            format!(
                "user_message_chunk {}",
                content_block_summary(&chunk.content)
            )
        }
        acp::SessionUpdate::AgentMessageChunk(chunk) => {
            format!(
                "agent_message_chunk {}",
                content_block_summary(&chunk.content)
            )
        }
        acp::SessionUpdate::AgentThoughtChunk(chunk) => {
            format!(
                "agent_thought_chunk {}",
                content_block_summary(&chunk.content)
            )
        }
        acp::SessionUpdate::ToolCall(tc) => {
            format!(
                "tool_call id={} kind={:?} status={:?} title={:?} content={} raw_input={}",
                tc.tool_call_id.0,
                tc.kind,
                tc.status,
                tc.title,
                tc.content.len(),
                tc.raw_input
                    .as_ref()
                    .map_or_else(|| "none".to_string(), json_size_hint),
            )
        }
        acp::SessionUpdate::ToolCallUpdate(tcu) => {
            let f = &tcu.fields;
            format!(
                "tool_call_update id={} status={:?} title={:?} content={} raw_output={}",
                tcu.tool_call_id.0,
                f.status,
                f.title,
                f.content
                    .as_ref()
                    .map_or_else(|| "none".to_string(), |c| c.len().to_string()),
                f.raw_output
                    .as_ref()
                    .map_or_else(|| "none".to_string(), json_size_hint),
            )
        }
        acp::SessionUpdate::Plan(plan) => format!("plan entries={}", plan.entries.len()),
        acp::SessionUpdate::AvailableCommandsUpdate(u) => {
            format!(
                "available_commands_update commands={}",
                u.available_commands.len()
            )
        }
        acp::SessionUpdate::CurrentModeUpdate(u) => {
            format!("current_mode_update mode={}", u.current_mode_id.0)
        }
        _ => "unknown_update".to_string(),
    }
}
/// Compact description of a `ContentBlock`: type plus payload size in bytes.
fn content_block_summary(content: &acp::ContentBlock) -> String {
    match content {
        acp::ContentBlock::Text(t) => format!("text={}B", t.text.len()),
        acp::ContentBlock::Image(i) => {
            format!("image={}B mime={}", i.data.len(), i.mime_type)
        }
        acp::ContentBlock::Audio(a) => {
            format!("audio={}B mime={}", a.data.len(), a.mime_type)
        }
        acp::ContentBlock::ResourceLink(r) => format!("resource_link={}", r.uri),
        acp::ContentBlock::Resource(_) => "resource".to_string(),
        _ => "unknown_content".to_string(),
    }
}
/// Cheap size descriptor for a `serde_json::Value` without serializing it.
///
/// Strings report byte length; arrays report element count (bash raw_output
/// is a `Vec<u8>`, so element count == output bytes); objects report key
/// count plus the summed size of direct string/array members (one level, no
/// recursion). This keeps the cost O(top-level members), never O(payload).
fn json_size_hint(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::String(s) => format!("str({}B)", s.len()),
        Value::Array(a) => format!("arr({})", a.len()),
        Value::Object(o) => {
            let inner: usize = o
                .values()
                .map(|m| match m {
                    Value::String(s) => s.len(),
                    Value::Array(a) => a.len(),
                    _ => 0,
                })
                .sum();
            format!("obj({} keys, ~{}B)", o.len(), inner)
        }
    }
}
/// Compact rendering of the interesting `NotificationMeta` fields.
fn meta_summary(meta: &NotificationMeta) -> String {
    format!(
        "seq={} tokens={} prompt={} stream_start={}",
        meta.event_seq
            .map_or_else(|| "-".to_string(), |v| v.to_string()),
        meta.total_tokens
            .map_or_else(|| "-".to_string(), |v| v.to_string()),
        meta.prompt_id.as_deref().unwrap_or("-"),
        meta.stream_start_ms
            .map_or_else(|| "-".to_string(), |v| v.to_string()),
    )
}
/// Parse the JSON content from a SearchToolOutput into DiscoveredTool entries.
///
/// Results are grouped by server: `{"results": [{"server": "...", "tools": [...]}]}`.
/// Each tool has `tool_name`, `description`, `score`, and `input_schema`.
fn parse_search_tool_results(content: &str) -> Vec<DiscoveredTool> {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(content) else {
        return Vec::new();
    };
    let Some(groups) = val.get("results").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for group in groups {
        let server = group
            .get("server")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let Some(tools) = group.get("tools").and_then(|v| v.as_array()) else {
            continue;
        };
        for r in tools {
            let Some(name) = r.get("tool_name").and_then(|v| v.as_str()) else {
                continue;
            };
            let description = r
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let score = r.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            out.push(DiscoveredTool {
                name: name.to_owned(),
                server: server.clone(),
                description,
                score,
            });
        }
    }
    out
}
/// Extract output text from a use_tool's raw_output.
///
/// MCP tools don't put content in ACP content blocks — they only set raw_output.
/// This extracts the text from ToolOutput::MCP, ToolOutput::Text, or
/// ToolOutput::Dynamic variants.
fn extract_use_tool_output(raw: &Option<serde_json::Value>) -> Option<String> {
    let val = raw.as_ref()?;
    if let Ok(output) = serde_json::from_value::<ToolOutput>(val.clone()) {
        let text = match output {
            ToolOutput::MCP(mcp) => {
                use xai_grok_tools::types::output::MCPOutputDetails;
                match mcp.output() {
                    MCPOutputDetails::OkayOutput(s) | MCPOutputDetails::Error(s) => s.clone(),
                }
            }
            ToolOutput::Text(text) => text.text,
            ToolOutput::Dynamic(v) => {
                return Some(serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            _ => return None,
        };
        return Some(maybe_pretty_json(&text));
    }
    val.as_str().map(maybe_pretty_json)
}
/// If the string is valid JSON, pretty-print it. Otherwise return as-is.
fn maybe_pretty_json(s: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        serde_json::to_string_pretty(&v).unwrap_or_else(|_| s.to_owned())
    } else {
        s.to_owned()
    }
}
/// Extract input arguments from a use_tool call's raw_input.tool_input.
///
/// Flattens the top-level JSON object into key-value string pairs for display.
/// Nested objects/arrays are rendered as compact JSON strings.
fn extract_use_tool_args(tc: &acp::ToolCall) -> Vec<(String, String)> {
    let Some(raw) = tc.raw_input.as_ref() else {
        return Vec::new();
    };
    let Some(tool_input) = raw.get("tool_input") else {
        return Vec::new();
    };
    let Some(obj) = tool_input.as_object() else {
        return Vec::new();
    };
    obj.iter()
        .map(|(k, v)| {
            let display = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => "null".to_owned(),
                serde_json::Value::Bool(b) => b.to_string(),
                serde_json::Value::Number(n) => n.to_string(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            (k.clone(), display)
        })
        .collect()
}
/// Convert an absolute path to relative by stripping the current working directory.
fn make_relative_path(path: &str) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_str = cwd.to_string_lossy();
        if let Some(rel) = path.strip_prefix(cwd_str.as_ref()) {
            let rel = rel.strip_prefix('/').unwrap_or(rel);
            return if rel.is_empty() {
                ".".to_string()
            } else {
                rel.to_string()
            };
        }
    }
    path.to_string()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    /// Default meta with no timestamps (simulates old grok-shell or tests that
    /// don't care about timing).
    fn meta() -> NotificationMeta {
        NotificationMeta::default()
    }
    fn agent_chunk(text: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text.to_string()),
        )))
    }
    fn thought_chunk(text: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text.to_string()),
        )))
    }
    #[test]
    fn workflow_suppression_keeps_authoring_calls_visible() {
        let wf = |title: &str, raw: serde_json::Value| {
            acp::ToolCall::new(acp::ToolCallId::new(Arc::from("t1")), title.to_string())
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending)
                .raw_input(Some(raw))
        };
        assert!(is_workflow_tool(&wf(
            "Workflow: deep-research",
            serde_json::json!({
            "variant" : "Workflow", "name" : "deep-research" }),
        )));
        assert!(is_workflow_tool(&wf(
            "Workflow: resume run",
            serde_json::json!({ "variant" :
            "Workflow", "resume_from_run_id" : "wf_1" }),
        )));
        assert!(!is_workflow_tool(&wf(
            "Validating workflow 'triage'",
            serde_json::json!({
            "variant" : "Workflow", "script" : "let meta = ...", "validate_only" : true
            }),
        )));
        assert!(is_workflow_tool(&wf(
            "Creating workflow 'triage'",
            serde_json::json!({
            "variant" : "Workflow", "script" : "let meta = ..." }),
        )));
        assert!(!is_workflow_tool(&wf(
            "workflow",
            serde_json::json!({ "validate_only" :
            true }),
        )));
        assert!(is_workflow_tool(&wf(
            "workflow",
            serde_json::json!({ "name" : "goal" }),
        )));
    }
    fn tool_call(id: &str, kind: acp::ToolKind, title: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(acp::ToolCallId::new(Arc::from(id)), title.to_string())
                .kind(kind)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![])
                .locations(vec![]),
        )
    }
    fn tool_call_completed(id: &str, kind: acp::ToolKind, title: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(acp::ToolCallId::new(Arc::from(id)), title.to_string())
                .kind(kind)
                .status(acp::ToolCallStatus::Completed)
                .content(vec![])
                .locations(vec![]),
        )
    }
    fn tool_update_completed(id: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(id)),
            acp::ToolCallUpdateFields::new().status(Some(acp::ToolCallStatus::Completed)),
        ))
    }
    fn user_message(text: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text.to_string()),
        )))
    }
    #[test]
    fn streaming_agent_message() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        assert!(tracker.handle_update(agent_chunk("Hello "), &meta(), &mut sb));
        assert!(tracker.handle_update(agent_chunk("world!"), &meta(), &mut sb));
        assert_eq!(sb.len(), 1);
        assert!(tracker.current_agent_msg.is_some());
    }
    #[test]
    fn agent_output_epoch_tracks_visible_live_output() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        assert!(tracker.handle_update(user_message("prompt"), &meta(), &mut sb));
        assert_eq!(tracker.agent_output_epoch(), 0);
        assert!(tracker.handle_update(agent_chunk("response"), &meta(), &mut sb));
        assert_eq!(tracker.agent_output_epoch(), 1);
        let replay = NotificationMeta {
            is_replay: true,
            ..Default::default()
        };
        assert!(tracker.handle_update(agent_chunk(" replay"), &replay, &mut sb));
        assert_eq!(tracker.agent_output_epoch(), 1);
        assert!(tracker.handle_update(thought_chunk("thinking"), &meta(), &mut sb));
        assert_eq!(tracker.agent_output_epoch(), 2);
        assert!(tracker.handle_update(
            tool_call("read-1", acp::ToolKind::Read, "read_file"),
            &meta(),
            &mut sb,
        ));
        assert_eq!(tracker.agent_output_epoch(), 3);
        assert!(tracker.handle_update(tool_update_completed("read-1"), &meta(), &mut sb));
        assert_eq!(tracker.agent_output_epoch(), 4);
        assert!(!tracker.handle_update(
            tool_call("todo-1", acp::ToolKind::Other, "TodoWrite"),
            &meta(),
            &mut sb,
        ));
        assert_eq!(tracker.agent_output_epoch(), 4);
    }
    #[test]
    fn streaming_thinking() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        assert!(tracker.handle_update(thought_chunk("Let me think"), &meta(), &mut sb));
        assert!(tracker.handle_update(thought_chunk("..."), &meta(), &mut sb));
        assert_eq!(sb.len(), 1);
        assert!(tracker.current_thinking.is_some());
    }
    #[test]
    fn pre_create_thinking_no_op_when_flag_off() {
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.pre_create_thinking(&mut sb);
        assert_eq!(sb.len(), 0);
        assert!(tracker.current_thinking.is_none());
        crate::appearance::cache::set_show_thinking_blocks(true);
    }
    #[test]
    fn thought_chunk_dropped_when_flag_off() {
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        assert!(!tracker.handle_update(thought_chunk("secret reasoning"), &meta(), &mut sb));
        assert_eq!(sb.len(), 0);
        assert!(tracker.current_thinking.is_none());
        crate::appearance::cache::set_show_thinking_blocks(true);
        assert!(tracker.handle_update(thought_chunk("visible now"), &meta(), &mut sb));
        assert_eq!(sb.len(), 1);
        assert!(tracker.current_thinking.is_some());
    }
    #[test]
    fn pre_create_thinking_creates_when_flag_on() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.pre_create_thinking(&mut sb);
        assert_eq!(sb.len(), 1);
        assert!(tracker.current_thinking.is_some());
    }
    #[test]
    fn thinking_then_agent_message() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(thought_chunk("thinking..."), &meta(), &mut sb);
        assert_eq!(sb.len(), 1);
        tracker.handle_update(agent_chunk("Here's my answer"), &meta(), &mut sb);
        assert_eq!(sb.len(), 2);
        assert!(tracker.current_thinking.is_none());
    }
    #[test]
    fn replayed_thinking_uses_server_elapsed_not_local_zero() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let replay_meta = NotificationMeta {
            is_replay: true,
            stream_start_ms: Some(1_000_000),
            agent_timestamp_ms: Some(1_002_000),
            ..NotificationMeta::default()
        };
        tracker.handle_update(thought_chunk("pondering deeply"), &replay_meta, &mut sb);
        tracker.handle_update(agent_chunk("done"), &replay_meta, &mut sb);
        let entries = sb.entries_in_range(0..sb.len());
        let thinking = entries
            .iter()
            .find_map(|e| match &e.block {
                RenderBlock::Thinking(t) => Some(t),
                _ => None,
            })
            .expect("a thinking block should survive replay (non-empty content)");
        assert_eq!(
            thinking.elapsed_time_ms(),
            Some(2000),
            "replayed thinking must use server elapsed, not a ~0ms local-timer freeze"
        );
    }
    #[test]
    fn live_thinking_keeps_local_elapsed_timer() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(thought_chunk("thinking live"), &meta(), &mut sb);
        let entries = sb.entries_in_range(0..sb.len());
        let thinking = entries
            .iter()
            .find_map(|e| match &e.block {
                RenderBlock::Thinking(t) => Some(t),
                _ => None,
            })
            .expect("a live thinking block should exist");
        assert!(
            thinking.elapsed_time_ms().is_some(),
            "live thinking must keep a local elapsed timer (started_at armed)"
        );
    }
    #[test]
    fn tool_call_lifecycle() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Read, "src/main.rs"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.pending_tools.len(), 1);
        tracker.handle_update(tool_update_completed("tc1"), &meta(), &mut sb);
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.pending_tools.len(), 0);
    }
    #[test]
    fn tool_call_already_completed() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call_completed("tc1", acp::ToolKind::Read, "src/main.rs"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.pending_tools.len(), 0);
    }
    #[test]
    fn agent_msg_resets_after_tool() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(agent_chunk("Before tool"), &meta(), &mut sb);
        assert_eq!(sb.len(), 1);
        tracker.handle_update(
            tool_call_completed("tc1", acp::ToolKind::Read, "file.rs"),
            &meta(),
            &mut sb,
        );
        assert!(tracker.current_agent_msg.is_none());
        tracker.handle_update(agent_chunk("After tool"), &meta(), &mut sb);
        assert_eq!(sb.len(), 3);
    }
    #[test]
    fn finish_turn_clears_state() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(agent_chunk("text"), &meta(), &mut sb);
        tracker.handle_update(thought_chunk("thinking"), &meta(), &mut sb);
        assert!(tracker.current_agent_msg.is_some());
        assert!(tracker.current_thinking.is_some());
        tracker.handle_update(tool_update_completed("tc-orphan"), &meta(), &mut sb);
        assert_eq!(tracker.orphan_updates.len(), 1);
        tracker.task_tool_background.insert("task-x".into(), true);
        tracker.finish_turn(&mut sb);
        assert!(tracker.current_agent_msg.is_none());
        assert!(tracker.current_thinking.is_none());
        assert!(tracker.pending_tools.is_empty());
        assert!(
            tracker.orphan_updates.is_empty(),
            "orphaned tool-call updates are turn-scoped"
        );
        assert_eq!(
            tracker.task_tool_background.get("task-x"),
            Some(&true),
            "background Task flags survive turn end for the late SubagentSpawned"
        );
        assert!(
            !sb.needs_animation(),
            "no entries should be running after finish_turn"
        );
    }
    #[test]
    fn user_message_replay() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(user_message("What is Rust?"), &meta(), &mut sb);
        assert_eq!(sb.len(), 1);
    }
    #[test]
    fn empty_chunks_ignored() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        assert!(!tracker.handle_update(agent_chunk(""), &meta(), &mut sb));
        assert!(!tracker.handle_update(thought_chunk(""), &meta(), &mut sb));
        assert_eq!(sb.len(), 0);
    }
    /// Regression test: two turns should create separate agent message entries.
    ///
    /// Previously, handle_user_message() didn't reset current_agent_msg,
    /// so the second turn's agent message chunks got appended to the first
    /// turn's entry, producing concatenated text.
    #[test]
    fn two_turns_separate_agent_messages() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(user_message("whats the current date"), &meta(), &mut sb);
        tracker.handle_update(thought_chunk("thinking about date..."), &meta(), &mut sb);
        tracker.handle_update(
            agent_chunk("The current date is February 8, 2026."),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(
            user_message("whats the weather in london"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(thought_chunk("thinking about weather..."), &meta(), &mut sb);
        tracker.handle_update(
            agent_chunk("I don't have access to weather data."),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 6, "Each turn should have its own entries");
        let entry2 = sb.get(2).expect("entry 2");
        let entry5 = sb.get(5).expect("entry 5");
        assert_ne!(
            entry2.id, entry5.id,
            "Agent messages from different turns must be separate entries"
        );
    }
    /// Regression test: user_message should reset tracking state.
    #[test]
    fn user_message_resets_tracking() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(agent_chunk("hello"), &meta(), &mut sb);
        assert!(tracker.current_agent_msg.is_some());
        tracker.handle_update(user_message("new question"), &meta(), &mut sb);
        assert!(
            tracker.current_agent_msg.is_none(),
            "user_message should reset current_agent_msg"
        );
        assert!(
            tracker.current_thinking.is_none(),
            "user_message should reset current_thinking"
        );
    }
    /// Regression test: exact real-world flow where send_prompt adds user entry
    /// directly to scrollback (bypassing tracker), then tracker receives echo + response.
    ///
    /// This matches what actually happens in the app:
    /// 1. send_prompt() pushes user entry + calls expect_user_echo()
    /// 2. ACP echoes user_message_chunk → tracker skips it (no duplicate)
    /// 3. ACP streams thought_chunk, agent_message_chunk
    /// 4. User sends second prompt via send_prompt
    /// 5. ACP echoes + streams second turn
    ///
    /// The critical invariant: exactly 1 user entry per turn, 2 separate agent messages.
    #[test]
    fn real_flow_two_turns_via_send_prompt() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        sb.push_block(RenderBlock::user_prompt("whats the date"));
        tracker.expect_user_echo();
        let modified = tracker.handle_update(user_message("whats the date"), &meta(), &mut sb);
        assert!(!modified, "echo should be skipped, not modify scrollback");
        assert_eq!(sb.len(), 1, "still just 1 entry (direct push only)");
        tracker.handle_update(thought_chunk("thinking about date..."), &meta(), &mut sb);
        tracker.handle_update(
            agent_chunk("Today's date is February 8, 2026."),
            &meta(),
            &mut sb,
        );
        assert!(
            tracker.current_agent_msg.is_some(),
            "turn 1 agent msg should be tracked"
        );
        tracker.finish_turn(&mut sb);
        sb.push_block(RenderBlock::user_prompt("whats the current weather"));
        tracker.expect_user_echo();
        let modified =
            tracker.handle_update(user_message("whats the current weather"), &meta(), &mut sb);
        assert!(!modified, "second echo should also be skipped");
        assert!(
            tracker.current_agent_msg.is_none(),
            "echo should have reset current_agent_msg"
        );
        tracker.handle_update(thought_chunk("thinking about weather..."), &meta(), &mut sb);
        tracker.handle_update(
            agent_chunk("I don't have access to weather data."),
            &meta(),
            &mut sb,
        );
        let agent_msg_indices: Vec<usize> = (0..sb.len())
            .filter(|&i| matches!(sb.get(i).unwrap().block, RenderBlock::AgentMessage(_)))
            .collect();
        assert_eq!(
            agent_msg_indices.len(),
            2,
            "Should have exactly 2 separate agent message entries, got {}. Total entries: {}",
            agent_msg_indices.len(),
            sb.len(),
        );
        let user_count = (0..sb.len())
            .filter(|&i| matches!(sb.get(i).unwrap().block, RenderBlock::UserPrompt(_)))
            .count();
        assert_eq!(
            user_count, 2,
            "exactly 2 user entries (no duplicates from echo)"
        );
    }
    /// Test: two turns where finish_turn() is called between them
    /// (simulating send_prompt calling finish_turn before new turn).
    /// No echo user_message_chunk — just direct scrollback manipulation + tracker.
    #[test]
    fn two_turns_with_finish_turn_between() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        sb.push_block(RenderBlock::user_prompt("whats the date"));
        tracker.handle_update(thought_chunk("thinking..."), &meta(), &mut sb);
        tracker.handle_update(agent_chunk("Today is February 8, 2026."), &meta(), &mut sb);
        assert!(tracker.current_agent_msg.is_some());
        tracker.finish_turn(&mut sb);
        assert!(tracker.current_agent_msg.is_none());
        sb.push_block(RenderBlock::user_prompt("whats the weather"));
        tracker.handle_update(thought_chunk("thinking about weather..."), &meta(), &mut sb);
        tracker.handle_update(agent_chunk("I can't check weather."), &meta(), &mut sb);
        let agent_msg_count = (0..sb.len())
            .filter(|&i| matches!(sb.get(i).unwrap().block, RenderBlock::AgentMessage(_)))
            .count();
        assert_eq!(
            agent_msg_count, 2,
            "Must have 2 separate agent messages, got {}",
            agent_msg_count,
        );
    }
    /// Test: expect_user_echo skips exactly one echo, then allows normal flow.
    #[test]
    fn expect_user_echo_skips_one() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        sb.push_block(RenderBlock::user_prompt("hello"));
        tracker.expect_user_echo();
        assert!(!tracker.handle_update(user_message("hello"), &meta(), &mut sb));
        assert_eq!(sb.len(), 1, "echo should not add a duplicate");
        assert!(tracker.handle_update(user_message("world"), &meta(), &mut sb));
        assert_eq!(sb.len(), 2, "second message should be added normally");
    }
    /// The echoed promptIndex belongs to the turn-starting prompt: an
    /// interjection that lands between the local push and the echo (laggy
    /// link) must not steal the backfilled index — the shell never numbers
    /// interjections.
    #[test]
    fn echo_prompt_index_backfill_skips_interjections() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let prompt_id = sb.push_block(RenderBlock::user_prompt("real prompt"));
        tracker.expect_user_echo();
        let ij_id = sb.push_block(RenderBlock::interjection_prompt("steer"));
        let echo = acp::SessionUpdate::UserMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                "real prompt".to_string(),
            )))
            .meta(
                serde_json::json!({ "promptIndex" : 3 })
                    .as_object()
                    .cloned(),
            ),
        );
        assert!(
            !tracker.handle_update(echo, &meta(), &mut sb),
            "echo is skipped"
        );
        let prompt_idx = sb.index_of_id(prompt_id).unwrap();
        match &sb.get(prompt_idx).unwrap().block {
            RenderBlock::UserPrompt(b) => assert_eq!(b.prompt_index, Some(3)),
            other => panic!("expected UserPrompt, got {other:?}"),
        }
        let ij_idx = sb.index_of_id(ij_id).unwrap();
        match &sb.get(ij_idx).unwrap().block {
            RenderBlock::UserPrompt(b) => {
                assert!(b.is_interjection);
                assert_eq!(
                    b.prompt_index, None,
                    "interjection must not steal the echoed index"
                );
            }
            other => panic!("expected UserPrompt, got {other:?}"),
        }
    }
    /// Test: session replay (no expect_user_echo) still creates user entries.
    #[test]
    fn session_replay_creates_user_entries() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        assert!(tracker.handle_update(user_message("old question"), &meta(), &mut sb));
        assert_eq!(sb.len(), 1);
        tracker.handle_update(agent_chunk("old answer"), &meta(), &mut sb);
        assert!(tracker.handle_update(user_message("second question"), &meta(), &mut sb));
        assert_eq!(sb.len(), 3);
    }
    /// Skill replay: XML metadata becomes a clean skill block, body is absorbed.
    #[test]
    fn skill_replay_creates_clean_block() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let xml = "<command-name>implement</command-name>\n\
                    <command-message>/implement</command-message>\n\
                    <command-args>fix the rendering bug</command-args>";
        assert!(tracker.handle_update(user_message(xml), &meta(), &mut sb));
        assert_eq!(sb.len(), 1);
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::UserPrompt(block) => {
                assert_eq!(
                    block.skill_token_ranges,
                    vec![0..10],
                    "leading /implement token styled as skill"
                );
                assert_eq!(block.text, "/implement fix the rendering bug");
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
        assert!(
            !tracker.handle_update(user_message("You are an orchestrator..."), &meta(), &mut sb,),
            "skill body should be absorbed",
        );
        assert_eq!(sb.len(), 1, "no new entry for skill body");
    }
    /// Skill replay without args still creates a clean block.
    #[test]
    fn skill_replay_no_args() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let xml = "<command-name>deploy</command-name>\n\
                    <command-message>/deploy</command-message>";
        assert!(tracker.handle_update(user_message(xml), &meta(), &mut sb));
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::UserPrompt(block) => {
                assert_eq!(block.skill_token_ranges, vec![0..7]);
                assert_eq!(block.text, "/deploy");
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
        assert!(!tracker.handle_update(user_message("Deploy instructions"), &meta(), &mut sb,));
        assert_eq!(sb.len(), 1);
    }
    /// Live execution: echo-skip + skill body skip work together.
    #[test]
    fn skill_echo_skips_both_chunks() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        sb.push_block(RenderBlock::skill_prompt("/implement fix bug"));
        tracker.expect_user_echo();
        let xml = "<command-name>implement</command-name>\n\
                    <command-message>/implement</command-message>\n\
                    <command-args>fix bug</command-args>";
        assert!(!tracker.handle_update(user_message(xml), &meta(), &mut sb));
        assert_eq!(sb.len(), 1, "echo should not add a duplicate");
        assert!(!tracker.handle_update(
            user_message("You are an orchestrator..."),
            &meta(),
            &mut sb,
        ));
        assert_eq!(sb.len(), 1, "skill body echo should be absorbed");
        assert!(tracker.handle_update(user_message("follow-up question"), &meta(), &mut sb,));
        assert_eq!(sb.len(), 2);
    }
    /// finish_turn clears stale skip_next_skill_body.
    #[test]
    fn finish_turn_clears_skill_body_skip() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let xml = "<command-name>commit</command-name>\n\
                    <command-message>/commit</command-message>";
        tracker.handle_update(user_message(xml), &meta(), &mut sb);
        assert!(tracker.skip_next_skill_body);
        tracker.finish_turn(&mut sb);
        assert!(!tracker.skip_next_skill_body);
        assert!(tracker.handle_update(user_message("new question"), &meta(), &mut sb,));
        assert_eq!(sb.len(), 2);
    }
    #[test]
    fn tool_update_before_tool_call_race() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        assert!(!tracker.handle_update(tool_update_completed("tc1"), &meta(), &mut sb));
        assert_eq!(sb.len(), 0);
        assert_eq!(tracker.orphan_updates.len(), 1);
        assert!(tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `echo hi`"),
            &meta(),
            &mut sb,
        ));
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.orphan_updates.len(), 0);
        assert_eq!(tracker.pending_tools.len(), 0);
    }
    #[test]
    fn tool_update_before_tool_call_preserves_kind() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(tool_update_completed("tc1"), &meta(), &mut sb);
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `ls`"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(_)) => {}
            other => panic!("Expected Execute block, got {:?}", other),
        }
    }
    #[test]
    fn tool_normal_order_still_works() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Read, "src/lib.rs"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.pending_tools.len(), 1);
        assert_eq!(tracker.orphan_updates.len(), 0);
        tracker.handle_update(tool_update_completed("tc1"), &meta(), &mut sb);
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.pending_tools.len(), 0);
    }
    /// Test thinking elapsed time computed from server timestamps.
    #[test]
    fn thinking_elapsed_from_server_timestamps() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let stream_start = 1700000000000i64;
        let make_meta = |agent_ts: i64| NotificationMeta {
            agent_timestamp_ms: Some(agent_ts),
            stream_start_ms: Some(stream_start),
            ..Default::default()
        };
        tracker.handle_update(
            thought_chunk("Let me think"),
            &make_meta(stream_start + 500),
            &mut sb,
        );
        assert_eq!(tracker.last_thinking_elapsed_ms, Some(500));
        tracker.handle_update(
            thought_chunk("...still thinking"),
            &make_meta(stream_start + 3200),
            &mut sb,
        );
        assert_eq!(tracker.last_thinking_elapsed_ms, Some(3200));
        tracker.handle_update(agent_chunk("Here's my answer"), &meta(), &mut sb);
        assert!(tracker.current_thinking.is_none());
        assert_eq!(tracker.last_thinking_elapsed_ms, None);
    }
    /// Test thinking elapsed is None when server doesn't send timestamps.
    #[test]
    fn thinking_elapsed_none_without_timestamps() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(thought_chunk("thinking"), &meta(), &mut sb);
        assert_eq!(tracker.last_thinking_elapsed_ms, None);
        tracker.handle_update(agent_chunk("done"), &meta(), &mut sb);
        assert_eq!(tracker.last_thinking_elapsed_ms, None);
    }
    #[test]
    fn agent_message_uses_server_timestamp() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let ts_ms = 1700000000000i64;
        let replay_meta = NotificationMeta {
            agent_timestamp_ms: Some(ts_ms),
            is_replay: true,
            ..Default::default()
        };
        tracker.handle_update(agent_chunk("Hello"), &replay_meta, &mut sb);
        let entry = sb.get(0).unwrap();
        let created = entry.created_at.expect("entry should have created_at");
        let expected = utc_ms_to_local(ts_ms);
        assert_eq!(
            created.timestamp(),
            expected.timestamp(),
            "Agent message should use server timestamp, not Local::now()"
        );
    }
    #[test]
    fn user_message_uses_server_timestamp() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let ts_ms = 1700000000000i64;
        let replay_meta = NotificationMeta {
            turn_start_ms: Some(ts_ms),
            is_replay: true,
            ..Default::default()
        };
        tracker.handle_update(user_message("Hello user"), &replay_meta, &mut sb);
        let entry = sb.get(0).unwrap();
        let created = entry.created_at.expect("entry should have created_at");
        let expected = utc_ms_to_local(ts_ms);
        assert_eq!(
            created.timestamp(),
            expected.timestamp(),
            "User message should use server turn_start_ms timestamp"
        );
    }
    #[test]
    fn entry_falls_back_to_now_without_server_timestamp() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let before = chrono::Local::now();
        tracker.handle_update(agent_chunk("live message"), &meta(), &mut sb);
        let after = chrono::Local::now();
        let entry = sb.get(0).unwrap();
        let created = entry.created_at.expect("entry should have created_at");
        assert!(
            created >= before && created <= after,
            "Without server timestamp, should fall back to Local::now()"
        );
    }
    /// Helper: create a ToolCallUpdate with InProgress status and BashOutput raw_output.
    fn tool_update_in_progress(id: &str, output_bytes: &[u8]) -> acp::SessionUpdate {
        use xai_grok_tools::types::output::{BashOutput, ToolOutput};
        let bash = BashOutput {
            output: output_bytes.to_vec(),
            output_for_prompt: String::new(),
            exit_code: 0,
            command: String::new(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: String::new(),
            output_file: String::new(),
            total_bytes: output_bytes.len(),
            output_delta: None,
            was_bare_echo: false,
        };
        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(id)),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::InProgress))
                .raw_output(serde_json::to_value(ToolOutput::Bash(bash)).ok()),
        ))
    }
    /// Helper: create a completed ToolCallUpdate with BashOutput.
    fn tool_update_completed_bash(
        id: &str,
        output_bytes: &[u8],
        exit_code: i32,
    ) -> acp::SessionUpdate {
        use xai_grok_tools::types::output::{BashOutput, ToolOutput};
        let status = if exit_code == 0 {
            acp::ToolCallStatus::Completed
        } else {
            acp::ToolCallStatus::Failed
        };
        let bash = BashOutput {
            output: output_bytes.to_vec(),
            output_for_prompt: String::new(),
            exit_code,
            command: "test".to_string(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: "/tmp".to_string(),
            output_file: String::new(),
            total_bytes: output_bytes.len(),
            output_delta: None,
            was_bare_echo: false,
        };
        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(id)),
            acp::ToolCallUpdateFields::new()
                .status(Some(status))
                .raw_output(serde_json::to_value(ToolOutput::Bash(bash)).ok()),
        ))
    }
    /// Helper: create a ToolCall with raw_input containing command + description.
    fn tool_call_execute_with_desc(
        id: &str,
        command: &str,
        description: &str,
    ) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(
                acp::ToolCallId::new(Arc::from(id)),
                format!("Execute `{}`", command),
            )
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .content(vec![])
            .raw_input(Some(serde_json::json!(
                { "command" : command, "description" : description, }
            )))
            .locations(vec![]),
        )
    }
    /// Streaming execute: InProgress updates push output to the block.
    #[test]
    fn streaming_execute_in_progress() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `cargo build`"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.pending_tools.len(), 1);
        let modified = tracker.handle_update(
            tool_update_in_progress("tc1", b"Compiling"),
            &meta(),
            &mut sb,
        );
        assert!(modified, "InProgress update should modify scrollback");
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert_eq!(exec.output.as_deref(), Some("Compiling"));
            }
            other => panic!("Expected Execute block, got {:?}", other),
        }
        tracker.handle_update(
            tool_update_in_progress("tc1", b"Compiling crate v0.1.0\n  Finished"),
            &meta(),
            &mut sb,
        );
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert_eq!(
                    exec.output.as_deref(),
                    Some("Compiling crate v0.1.0\n  Finished")
                );
            }
            other => panic!("Expected Execute block, got {:?}", other),
        }
        assert_eq!(tracker.pending_tools.len(), 1);
        assert!(sb.get(0).unwrap().is_running);
    }
    /// Streaming execute: completed update replaces block with final output.
    #[test]
    fn streaming_execute_completion() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `echo hello`"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(tool_update_in_progress("tc1", b"hello\n"), &meta(), &mut sb);
        tracker.handle_update(
            tool_update_completed_bash("tc1", b"hello\n", 0),
            &meta(),
            &mut sb,
        );
        assert_eq!(tracker.pending_tools.len(), 0);
        assert!(!sb.get(0).unwrap().is_running);
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert_eq!(exec.output.as_deref(), Some("hello\n"));
                assert!(exec.error.is_none(), "exit code 0 = no error");
            }
            other => panic!("Expected Execute block, got {:?}", other),
        }
    }
    /// Streaming execute: failed command shows error.
    #[test]
    fn streaming_execute_failure() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `false`"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(tool_update_completed_bash("tc1", b"", 1), &meta(), &mut sb);
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert!(exec.error.is_some(), "non-zero exit should set error");
                assert!(
                    exec.error.as_deref().unwrap().contains("exit code 1"),
                    "error should mention exit code"
                );
            }
            other => panic!("Expected Execute block, got {:?}", other),
        }
    }
    /// Execute with description from raw_input.
    #[test]
    fn execute_with_description() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call_execute_with_desc("tc1", "cargo test", "Run the test suite"),
            &meta(),
            &mut sb,
        );
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert_eq!(exec.command, "cargo test");
                assert_eq!(exec.description.as_deref(), Some("Run the test suite"));
            }
            other => panic!("Expected Execute block, got {:?}", other),
        }
    }
    /// InProgress update for non-execute tool is ignored.
    #[test]
    fn in_progress_update_ignored_for_non_execute() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Read, "src/main.rs"),
            &meta(),
            &mut sb,
        );
        let modified = tracker.handle_update(
            tool_update_in_progress("tc1", b"file content"),
            &meta(),
            &mut sb,
        );
        assert!(
            !modified,
            "InProgress with bash output should be ignored for Read blocks"
        );
    }
    /// Output is passed through without modification (no-color mode means
    /// the shell sends clean output without ANSI codes).
    #[test]
    fn streaming_execute_passes_output_through() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `ls`"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(
            tool_update_in_progress("tc1", b"green text"),
            &meta(),
            &mut sb,
        );
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert_eq!(
                    exec.output.as_deref(),
                    Some("green text"),
                    "Output should be passed through as-is"
                );
            }
            other => panic!("Expected Execute block, got {:?}", other),
        }
    }
    /// Verify ToolOutput::Bash round-trips through serde_json::Value correctly.
    /// This mimics the exact path: streaming_local_terminal serializes with
    /// serde_json::to_value(ToolOutput::Bash(...)), and tracker deserializes with
    /// serde_json::from_value::<ToolOutput>(...).
    #[test]
    fn tool_output_bash_serde_roundtrip() {
        use xai_grok_tools::types::output::{BashOutput, ToolOutput};
        let bash = BashOutput {
            output: b"hello world\n".to_vec(),
            output_for_prompt: String::new(),
            exit_code: 0,
            command: "echo hello".to_string(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: "/tmp".to_string(),
            output_file: String::new(),
            total_bytes: 12,
            output_delta: None,
            was_bare_echo: false,
        };
        let value = serde_json::to_value(ToolOutput::Bash(bash)).unwrap();
        assert_eq!(
            value.get("type").and_then(|v| v.as_str()),
            Some("Bash"),
            "ToolOutput should serialize with type tag"
        );
        assert!(value.get("output").is_some(), "Should have output field");
        let deserialized: ToolOutput = serde_json::from_value(value).unwrap();
        match deserialized {
            ToolOutput::Bash(bash) => {
                assert_eq!(bash.output, b"hello world\n");
                assert_eq!(bash.command, "echo hello");
            }
            _ => panic!("Expected ToolOutput::Bash"),
        }
    }
    /// End-to-end test mimicking the exact production notification sequence:
    /// 1. ToolCall (Pending) with raw_input containing BashTool
    /// 2. InProgress ToolCallUpdate with raw_output containing ToolOutput::Bash
    ///    (sent by notification_bridge from LocalTerminalBackend)
    /// 3. Completed ToolCallUpdate with raw_output containing final ToolOutput::Bash
    /// 4. Second Completed ToolCallUpdate (from acp_session completion handler)
    #[test]
    fn production_execute_sequence() {
        use serde_json::json;
        use xai_grok_tools::types::output::{BashOutput, ToolOutput};
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let tc_id = "call_abc123";
        let tc = acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(
                acp::ToolCallId::new(Arc::from(tc_id)),
                "Execute `python tmp/test.py`".to_string(),
            )
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Pending)
            .content(vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
                acp::TextContent::new("Running Python script".to_string()),
            ))])
            .raw_input(Some(json!(
                { "command" : "python tmp/test.py", "description" :
                "Running Python script" }
            )))
            .locations(vec![]),
        );
        tracker.handle_update(tc, &meta(), &mut sb);
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.pending_tools.len(), 1);
        let bash_output = ToolOutput::Bash(BashOutput {
            output_for_prompt: String::new(),
            output: b"Step 1: loading...\n".to_vec(),
            exit_code: 0,
            command: "python tmp/test.py".to_string(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: "/tmp".to_string(),
            output_file: String::new(),
            total_bytes: 19,
            output_delta: None,
            was_bare_echo: false,
        });
        let in_progress = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(tc_id)),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::InProgress))
                .content(Some(vec![acp::ToolCallContent::from(
                    acp::ContentBlock::Text(acp::TextContent::new(
                        "Step 1: loading...\n".to_string(),
                    )),
                )]))
                .raw_output(serde_json::to_value(&bash_output).ok()),
        ));
        let modified = tracker.handle_update(in_progress, &meta(), &mut sb);
        assert!(modified, "InProgress should trigger redraw");
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert_eq!(
                    exec.output.as_deref(),
                    Some("Step 1: loading...\n"),
                    "Streaming output should be set"
                );
            }
            other => panic!("Expected Execute block, got {:?}", other),
        }
        let bash_output2 = ToolOutput::Bash(BashOutput {
            output_for_prompt: String::new(),
            output: b"Step 1: loading...\nStep 2: processing...\n".to_vec(),
            exit_code: 0,
            command: "python tmp/test.py".to_string(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: "/tmp".to_string(),
            output_file: String::new(),
            total_bytes: 40,
            output_delta: None,
            was_bare_echo: false,
        });
        let in_progress2 = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(tc_id)),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::InProgress))
                .raw_output(serde_json::to_value(&bash_output2).ok()),
        ));
        tracker.handle_update(in_progress2, &meta(), &mut sb);
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert_eq!(
                    exec.output.as_deref(),
                    Some("Step 1: loading...\nStep 2: processing...\n"),
                    "Output should be replaced with full buffer"
                );
            }
            other => panic!("Expected Execute block, got {:?}", other),
        }
        let final_bash = ToolOutput::Bash(BashOutput {
            output_for_prompt: String::new(),
            output: b"Step 1: loading...\nStep 2: processing...\nDone!\n".to_vec(),
            exit_code: 0,
            command: "python tmp/test.py".to_string(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: "/tmp".to_string(),
            output_file: String::new(),
            total_bytes: 46,
            output_delta: None,
            was_bare_echo: false,
        });
        let completed = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(tc_id)),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::Completed))
                .raw_output(serde_json::to_value(&final_bash).ok()),
        ));
        tracker.handle_update(completed, &meta(), &mut sb);
        assert_eq!(tracker.pending_tools.len(), 0);
        let entry = sb.get(0).unwrap();
        assert!(!entry.is_running, "Should be marked as not running");
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert!(exec.output.is_some(), "Final block should have output");
                let output = exec.output.as_deref().unwrap();
                assert!(
                    output.contains("Done!"),
                    "Should contain final output, got: {output}"
                );
            }
            other => panic!("Expected Execute block, got {:?}", other),
        }
    }
    #[test]
    fn utf8_decoder_ascii_passthrough() {
        let mut dec = Utf8Decoder::default();
        assert_eq!(dec.decode(b"hello"), "hello");
        assert!(dec.buffer.is_empty());
    }
    #[test]
    fn utf8_decoder_complete_multibyte() {
        let mut dec = Utf8Decoder::default();
        assert_eq!(dec.decode("café".as_bytes()), "café");
        assert!(dec.buffer.is_empty());
    }
    #[test]
    fn utf8_decoder_split_2byte_char() {
        let mut dec = Utf8Decoder::default();
        assert_eq!(dec.decode(&[b'c', b'a', b'f', 0xC3]), "caf");
        assert_eq!(dec.buffer, &[0xC3]);
        assert_eq!(dec.decode(&[0xA9]), "é");
        assert!(dec.buffer.is_empty());
    }
    #[test]
    fn utf8_decoder_split_3byte_char() {
        let mut dec = Utf8Decoder::default();
        assert_eq!(dec.decode(&[0xE2]), "");
        assert_eq!(dec.buffer, &[0xE2]);
        assert_eq!(dec.decode(&[0x9C]), "");
        assert_eq!(dec.buffer, &[0xE2, 0x9C]);
        assert_eq!(dec.decode(&[0x93, b'!']), "✓!");
        assert!(dec.buffer.is_empty());
    }
    #[test]
    fn utf8_decoder_split_4byte_char() {
        let mut dec = Utf8Decoder::default();
        assert_eq!(dec.decode(&[0xF0, 0x9F]), "");
        assert_eq!(dec.buffer, &[0xF0, 0x9F]);
        assert_eq!(dec.decode(&[0xA6, 0x80]), "🦀");
        assert!(dec.buffer.is_empty());
    }
    #[test]
    fn utf8_decoder_genuinely_invalid_byte() {
        let mut dec = Utf8Decoder::default();
        let result = dec.decode(&[b'a', 0xFF, b'b']);
        assert_eq!(result, "a\u{FFFD}b");
        assert!(dec.buffer.is_empty());
    }
    #[test]
    fn utf8_decoder_multiple_feeds() {
        let mut dec = Utf8Decoder::default();
        assert_eq!(dec.decode(b"line1\n"), "line1\n");
        assert_eq!(dec.decode(b"line2\n"), "line2\n");
        assert_eq!(dec.decode("héllo\n".as_bytes()), "héllo\n");
        assert!(dec.buffer.is_empty());
    }
    /// Reproduce the exact ACP message flow for a grep search tool call:
    /// 1. ToolCall with kind=Other, title="grep" (initial, no metadata)
    /// 2. ToolCallUpdate in-progress with kind=search, title="fn main", rawInput
    /// 3. ToolCallUpdate completed with rawOutput containing GrepSearchOutput
    ///
    /// This was broken: kind from in-progress update was lost, so the completed
    /// block rendered as "Other" with no search results.
    #[test]
    fn test_search_tool_call_flow() {
        use xai_grok_tools::types::output::{GrepFileMatch, GrepLineMatch, GrepSearchOutput};
        let mut tracker = AcpUpdateTracker::new();
        let mut scrollback = ScrollbackState::new();
        let tc_id: Arc<str> = Arc::from("toolu_search_001");
        let tool_call = acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(acp::ToolCallId::new(tc_id.clone()), "grep".to_string())
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending),
        );
        tracker.handle_update(tool_call, &meta(), &mut scrollback);
        assert_eq!(scrollback.len(), 1);
        let in_progress = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(tc_id.clone()),
            acp::ToolCallUpdateFields::new()
                .kind(Some(acp::ToolKind::Search))
                .title(Some("fn main".to_string()))
                .raw_input(Some(serde_json::json!(
                    { "variant" : "Grep", "pattern" : "fn main", "path" :
                    "src/", }
                ))),
        ));
        tracker.handle_update(in_progress, &meta(), &mut scrollback);
        assert_eq!(scrollback.len(), 1, "should still be 1 entry");
        let entry = scrollback.get(0).expect("entry exists");
        assert!(
            matches!(
                &entry.block,
                RenderBlock::ToolCall(ToolCallBlock::Search(_))
            ),
            "block should be Search after in-progress update, got: {:?}",
            std::mem::discriminant(&entry.block)
        );
        let grep_output = GrepSearchOutput {
            stdout: vec![],
            stderr: vec![],
            exit_code: 0,
            match_count: 1,
            file_matches: vec![GrepFileMatch {
                path: "/Users/alice/dev/rust/foo/src/main.rs".to_string(),
                matches: vec![GrepLineMatch {
                    line_number: 54,
                    content: "fn main() -> Result<()> {".to_string(),
                }],
            }],
        };
        let completed = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(tc_id.clone()),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::Completed))
                .content(Some(vec![acp::ToolCallContent::Content(
                    acp::Content::new(acp::ContentBlock::Text(acp::TextContent::new(
                        "found 1 matches".to_string(),
                    ))),
                )]))
                .raw_output(serde_json::to_value(ToolOutput::GrepSearch(grep_output)).ok()),
        ));
        tracker.handle_update(completed, &meta(), &mut scrollback);
        assert_eq!(scrollback.len(), 1, "should still be 1 entry");
        let entry = scrollback.get(0).expect("entry exists");
        if let RenderBlock::ToolCall(ToolCallBlock::Search(search)) = &entry.block {
            assert_eq!(search.pattern, "fn main");
            assert_eq!(search.match_count, 1);
            assert_eq!(search.file_matches.len(), 1);
            assert_eq!(
                search.file_matches[0].path,
                "/Users/alice/dev/rust/foo/src/main.rs"
            );
            assert_eq!(search.file_matches[0].matches.len(), 1);
            assert_eq!(search.file_matches[0].matches[0].line_number, 54);
            assert_eq!(
                search.file_matches[0].matches[0].content,
                "fn main() -> Result<()> {"
            );
        } else {
            panic!(
                "Expected Search block after completion, got: {:?}",
                std::mem::discriminant(&entry.block)
            );
        }
    }
    /// ScrollbackState with an explicit `expanded_by_default` shape override
    /// (flag-independent: the `Some` beats the `collapsed_edit_blocks` cache).
    fn edit_config_scrollback(expanded_by_default: bool) -> ScrollbackState {
        use crate::appearance::AppearanceConfig;
        let mut sb = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.edit.expanded_by_default = Some(expanded_by_default);
        sb.set_appearance(appearance);
        sb
    }
    /// ToolCall(Pending) with kind=Other (shell currently sends this).
    fn pending_other_tool_call(tc_id: &Arc<str>) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(
                acp::ToolCallId::new(tc_id.clone()),
                "search_replace".to_string(),
            )
            .kind(acp::ToolKind::Other)
            .status(acp::ToolCallStatus::Pending)
            .content(vec![])
            .locations(vec![]),
        )
    }
    /// Regression test for #199720 follow-up: when an Other(Pending) entry is
    /// upgraded in-place to an Edit block, the entry's `display_mode` must be
    /// reset to the materialize policy's default — Collapsed by default,
    /// Expanded when `expanded_by_default` is set — rather than left at
    /// Other's default.
    ///
    /// Also covers the fast-path Pending→Completed (no in-progress refinement)
    /// where Edit's `finished_display_mode()` returns `None` and `finish_running`
    /// would otherwise leave a stale mode in place.
    #[test]
    fn edit_tool_upgrade_resets_display_mode_to_default() {
        use crate::scrollback::types::DisplayMode;
        /// Drive Pending(Other) → InProgress(Edit) → Completed and return the
        /// display mode observed after the InProgress upgrade and after
        /// completion.
        fn upgrade_path(tc: &str, expanded_by_default: bool) -> (DisplayMode, DisplayMode) {
            let mut tracker = AcpUpdateTracker::new();
            let mut sb = edit_config_scrollback(expanded_by_default);
            let tc_id: Arc<str> = Arc::from(tc);
            tracker.handle_update(pending_other_tool_call(&tc_id), &meta(), &mut sb);
            assert_eq!(sb.len(), 1);
            assert_eq!(sb.get(0).unwrap().display_mode, DisplayMode::Collapsed);
            let in_progress = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(tc_id.clone()),
                acp::ToolCallUpdateFields::new()
                    .kind(Some(acp::ToolKind::Edit))
                    .title(Some("foo.rs".to_string()))
                    .raw_input(Some(serde_json::json!({ "file_path" : "foo.rs" }))),
            ));
            tracker.handle_update(in_progress, &meta(), &mut sb);
            let entry = sb.get(0).expect("entry exists");
            assert!(
                matches!(&entry.block, RenderBlock::ToolCall(ToolCallBlock::Edit(_))),
                "block should be upgraded to Edit after in-progress refinement"
            );
            let after_upgrade = entry.display_mode;
            tracker.handle_update(tool_update_completed(&tc_id), &meta(), &mut sb);
            (after_upgrade, sb.get(0).unwrap().display_mode)
        }
        /// Drive the fast path Pending(Other) → Completed(Edit) with no
        /// in-progress refinement and return the final display mode.
        fn fast_path(tc: &str, expanded_by_default: bool) -> DisplayMode {
            let mut tracker = AcpUpdateTracker::new();
            let mut sb = edit_config_scrollback(expanded_by_default);
            let tc_id: Arc<str> = Arc::from(tc);
            tracker.handle_update(pending_other_tool_call(&tc_id), &meta(), &mut sb);
            assert_eq!(sb.get(0).unwrap().display_mode, DisplayMode::Collapsed);
            let completed = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(tc_id.clone()),
                acp::ToolCallUpdateFields::new()
                    .kind(Some(acp::ToolKind::Edit))
                    .title(Some("foo.rs".to_string()))
                    .raw_input(Some(serde_json::json!({ "file_path" : "foo.rs" })))
                    .status(Some(acp::ToolCallStatus::Completed)),
            ));
            tracker.handle_update(completed, &meta(), &mut sb);
            let entry = sb.get(0).expect("entry exists");
            assert!(
                matches!(&entry.block, RenderBlock::ToolCall(ToolCallBlock::Edit(_))),
                "block should be Edit after fast Pending→Completed"
            );
            entry.display_mode
        }
        let (upgraded, completed) = upgrade_path("toolu_edit_001", false);
        assert_eq!(
            upgraded,
            DisplayMode::Collapsed,
            "collapse shape: Edit upgrade stays Collapsed"
        );
        assert_eq!(
            completed,
            DisplayMode::Collapsed,
            "collapse shape: successful Edit remains Collapsed after completion"
        );
        assert_eq!(
            fast_path("toolu_edit_002", false),
            DisplayMode::Collapsed,
            "collapse shape: fast Pending→Completed Edit ends up Collapsed"
        );
        let (upgraded, completed) = upgrade_path("toolu_edit_003", true);
        assert_eq!(
            upgraded,
            DisplayMode::Expanded,
            "config on: display_mode must be reset to Expanded on upgrade, \
             not left at Other's default (Collapsed)"
        );
        assert_eq!(
            completed,
            DisplayMode::Expanded,
            "config on: successful Edit remains Expanded after completion"
        );
        assert_eq!(
            fast_path("toolu_edit_004", true),
            DisplayMode::Expanded,
            "config on: fast Pending→Completed Edit must end up Expanded"
        );
    }
    /// A manual expand of the collapsed one-liner must survive completion:
    /// once the entry is an Edit, the Edit-to-Edit completion swap preserves
    /// the current mode instead of snapping back to the configured default
    /// (no `respect_manual_folds` pinning required).
    #[test]
    fn edit_manual_expand_survives_completion() {
        use crate::scrollback::types::DisplayMode;
        let mut tracker = AcpUpdateTracker::new();
        let mut sb = edit_config_scrollback(false);
        let tc_id: Arc<str> = Arc::from("toolu_edit_gesture");
        tracker.handle_update(pending_other_tool_call(&tc_id), &meta(), &mut sb);
        let in_progress = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(tc_id.clone()),
            acp::ToolCallUpdateFields::new()
                .kind(Some(acp::ToolKind::Edit))
                .title(Some("foo.rs".to_string()))
                .raw_input(Some(serde_json::json!({ "file_path" : "foo.rs" })))
                .content(Some(vec![acp::ToolCallContent::Diff(
                    acp::Diff::new("foo.rs", "let x = 2;\n".to_string())
                        .old_text(Some("let x = 1;\n".to_string())),
                )])),
        ));
        tracker.handle_update(in_progress, &meta(), &mut sb);
        assert_eq!(sb.get(0).unwrap().display_mode, DisplayMode::Collapsed);
        sb.get_by_id_mut(sb.get(0).unwrap().id)
            .unwrap()
            .set_display_mode(DisplayMode::Expanded);
        tracker.handle_update(tool_update_completed(&tc_id), &meta(), &mut sb);
        assert_eq!(
            sb.get(0).unwrap().display_mode,
            DisplayMode::Expanded,
            "completion must not snap a user-expanded Edit back to Collapsed"
        );
    }
    /// Multi-file (apply_patch shape: several Diff items) and title-fallback
    /// Edits can't be summarized by the one-liner: they materialize Expanded
    /// with the summary marked untrusted, config-independent. Each case
    /// isolates one untrusted signal.
    #[test]
    fn multi_diff_and_title_fallback_edits_default_expanded() {
        use crate::scrollback::types::DisplayMode;
        let diff = |path: &str, old: &str, new: &str| {
            acp::ToolCallContent::Diff(
                acp::Diff::new(path, new.to_string()).old_text(Some(old.to_string())),
            )
        };
        let assert_untrusted_expanded = |tc: acp::ToolCall, label: &str| {
            let mut tracker = AcpUpdateTracker::new();
            let mut sb = edit_config_scrollback(false);
            tracker.handle_update(acp::SessionUpdate::ToolCall(tc), &meta(), &mut sb);
            let entry = sb.get(0).expect("entry exists");
            let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block else {
                panic!("{label}: expected Edit block, got {:?}", entry.block);
            };
            assert!(edit.summary_untrusted, "{label}: summary must be untrusted");
            assert_eq!(
                entry.display_mode,
                DisplayMode::Expanded,
                "{label}: untrusted summaries must not collapse to the one-liner"
            );
        };
        assert_untrusted_expanded(
            acp::ToolCall::new(
                acp::ToolCallId::new(Arc::from("toolu_multi_diff")),
                "Apply patch".to_string(),
            )
            .kind(acp::ToolKind::Edit)
            .status(acp::ToolCallStatus::Completed)
            .raw_input(Some(serde_json::json!({ "file_path" : "a.rs" })))
            .content(vec![
                diff("a.rs", "a1\n", "a2\n"),
                diff("b.rs", "b1\n", "b2\n"),
            ])
            .locations(vec![]),
            "multi_diff",
        );
        assert_untrusted_expanded(
            acp::ToolCall::new(
                acp::ToolCallId::new(Arc::from("toolu_title_fallback")),
                "Apply patch".to_string(),
            )
            .kind(acp::ToolKind::Edit)
            .status(acp::ToolCallStatus::Completed)
            .content(vec![diff("a.rs", "a1\n", "a2\n")])
            .locations(vec![]),
            "title_fallback",
        );
    }
    /// ToolCall(Pending) start for a search_replace edit.
    fn edit_tool_start(id: &str) -> acp::SessionUpdate {
        tool_call(id, acp::ToolKind::Edit, "search_replace")
    }
    /// Diff content replacing one line at `line`, so each scripted edit
    /// yields exactly one `+1/-1` hunk at a distinct position.
    fn edit_diff_content(path: &str, line: usize) -> acp::ToolCallContent {
        acp::ToolCallContent::Diff(
            acp::Diff::new(path, format!("new_{line}"))
                .old_text(Some(format!("old_{line}")))
                .meta(
                    serde_json::json!({ "old_line" : line, "new_line" : line })
                        .as_object()
                        .cloned(),
                ),
        )
    }
    /// Completed update carrying the edit's file_path and one-hunk diff.
    fn edit_tool_complete(id: &str, path: &str, line: usize) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(id)),
            acp::ToolCallUpdateFields::new()
                .kind(Some(acp::ToolKind::Edit))
                .title(Some(path.to_string()))
                .raw_input(Some(serde_json::json!({ "file_path" : path })))
                .content(Some(vec![edit_diff_content(path, line)]))
                .status(Some(acp::ToolCallStatus::Completed)),
        ))
    }
    /// Full Pending → Completed lifecycle for one scripted edit.
    fn run_edit(
        tracker: &mut AcpUpdateTracker,
        sb: &mut ScrollbackState,
        id: &str,
        path: &str,
        line: usize,
    ) {
        tracker.handle_update(edit_tool_start(id), &meta(), sb);
        tracker.handle_update(edit_tool_complete(id, path, line), &meta(), sb);
    }
    /// Pre-completed ToolCall (replay / session-load shape) with the same
    /// one-hunk diff as [`edit_tool_complete`].
    fn edit_tool_precompleted(id: &str, path: &str, line: usize) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(acp::ToolCallId::new(Arc::from(id)), path.to_string())
                .kind(acp::ToolKind::Edit)
                .status(acp::ToolCallStatus::Completed)
                .raw_input(Some(serde_json::json!({ "file_path" : path })))
                .content(vec![edit_diff_content(path, line)])
                .locations(vec![]),
        )
    }
    fn edit_block_at(sb: &ScrollbackState, idx: usize) -> &EditToolCallBlock {
        match &sb.get(idx).expect("entry at index").block {
            RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) => edit,
            other => panic!("expected Edit block at {idx}, got {other:?}"),
        }
    }
    /// Positions of the edited lines, one per hunk, in hunk order.
    fn hunk_lines(edit: &EditToolCallBlock) -> Vec<usize> {
        edit.hunks
            .iter()
            .map(|h| {
                h.iter()
                    .find(|l| l.tag == similar::ChangeTag::Insert)
                    .expect("insert line")
                    .ln
            })
            .collect()
    }
    #[test]
    fn adjacent_same_file_edits_coalesce() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            run_edit(&mut tracker, &mut sb, "e1", "foo.rs", 5);
            run_edit(&mut tracker, &mut sb, "e2", "foo.rs", 40);
            assert_eq!(sb.len(), 1, "two adjacent edits must merge into one entry");
            let edit = edit_block_at(&sb, 0);
            assert_eq!(edit.hunks.len(), 2);
            assert_eq!(edit.edit_count, 2);
            assert_eq!(hunk_lines(edit), vec![5, 40], "hunks keep scrollback order");
            let inserts: usize = edit
                .hunks
                .iter()
                .flatten()
                .filter(|l| l.tag == similar::ChangeTag::Insert)
                .count();
            assert_eq!(inserts, 2);
        })
        .join()
        .unwrap();
    }
    #[test]
    fn overlapping_adjacent_edits_stitch_into_single_hunk() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            for (i, line) in (5..=9).enumerate() {
                run_edit(&mut tracker, &mut sb, &format!("e{i}"), "foo.rs", line);
            }
            assert_eq!(sb.len(), 1);
            let edit = edit_block_at(&sb, 0);
            assert_eq!(edit.hunks.len(), 1, "contiguous hunks stitch into one");
            assert_eq!(
                edit.edit_count, 5,
                "the (N edits) fallback counts merged calls, not stitched hunks"
            );
            let rows: Vec<(similar::ChangeTag, usize)> =
                edit.hunks[0].iter().map(|l| (l.tag, l.ln)).collect();
            let expected: Vec<(similar::ChangeTag, usize)> = (5..=9)
                .flat_map(|ln| {
                    [
                        (similar::ChangeTag::Delete, ln),
                        (similar::ChangeTag::Insert, ln),
                    ]
                })
                .collect();
            assert_eq!(rows, expected);
        })
        .join()
        .unwrap();
    }
    #[test]
    fn coalesce_disabled_when_collapsed_edit_blocks_off() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(false);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            run_edit(&mut tracker, &mut sb, "e1", "foo.rs", 5);
            run_edit(&mut tracker, &mut sb, "e2", "foo.rs", 40);
            assert_eq!(
                sb.len(),
                2,
                "flag off keeps the legacy one-row-per-call transcript"
            );
            assert_eq!(edit_block_at(&sb, 0).hunks.len(), 1);
            assert_eq!(edit_block_at(&sb, 1).hunks.len(), 1);
        })
        .join()
        .unwrap();
    }
    #[test]
    fn three_sequential_edits_chain_into_one() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            run_edit(&mut tracker, &mut sb, "e1", "foo.rs", 5);
            run_edit(&mut tracker, &mut sb, "e2", "foo.rs", 20);
            run_edit(&mut tracker, &mut sb, "e3", "foo.rs", 40);
            assert_eq!(sb.len(), 1);
            let edit = edit_block_at(&sb, 0);
            assert_eq!(edit.hunks.len(), 3);
            assert_eq!(hunk_lines(edit), vec![5, 20, 40]);
        })
        .join()
        .unwrap();
    }
    #[test]
    fn different_files_do_not_coalesce() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            run_edit(&mut tracker, &mut sb, "e1", "foo.rs", 5);
            run_edit(&mut tracker, &mut sb, "e2", "bar.rs", 5);
            assert_eq!(sb.len(), 2, "edits to different files stay separate");
        })
        .join()
        .unwrap();
    }
    #[test]
    fn intervening_entry_breaks_coalesce_run() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            run_edit(&mut tracker, &mut sb, "e1", "foo.rs", 5);
            tracker.handle_update(agent_chunk("first edit done"), &meta(), &mut sb);
            run_edit(&mut tracker, &mut sb, "e2", "foo.rs", 40);
            assert_eq!(
                sb.len(),
                3,
                "a visible entry between edits blocks the merge"
            );
            assert_eq!(edit_block_at(&sb, 0).hunks.len(), 1);
            assert_eq!(edit_block_at(&sb, 2).hunks.len(), 1);
        })
        .join()
        .unwrap();
    }
    #[test]
    fn parallel_out_of_order_completion_coalesces() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            tracker.handle_update(edit_tool_start("e1"), &meta(), &mut sb);
            tracker.handle_update(edit_tool_start("e2"), &meta(), &mut sb);
            tracker.handle_update(edit_tool_complete("e2", "foo.rs", 40), &meta(), &mut sb);
            assert_eq!(sb.len(), 2, "no merge while the earlier call still runs");
            tracker.handle_update(edit_tool_complete("e1", "foo.rs", 5), &meta(), &mut sb);
            assert_eq!(sb.len(), 1, "forward check merges once the earlier lands");
            let edit = edit_block_at(&sb, 0);
            assert_eq!(
                hunk_lines(edit),
                vec![5, 40],
                "push order, not completion order"
            );
        })
        .join()
        .unwrap();
    }
    #[test]
    fn errored_edit_does_not_coalesce() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            run_edit(&mut tracker, &mut sb, "e1", "foo.rs", 5);
            tracker.handle_update(edit_tool_start("e2"), &meta(), &mut sb);
            tracker.handle_update(
                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                    acp::ToolCallId::new(Arc::from("e2")),
                    acp::ToolCallUpdateFields::new()
                        .kind(Some(acp::ToolKind::Edit))
                        .raw_input(Some(serde_json::json!({ "file_path" : "foo.rs" })))
                        .status(Some(acp::ToolCallStatus::Failed)),
                )),
                &meta(),
                &mut sb,
            );
            assert_eq!(sb.len(), 2, "a failed edit never merges");
            assert!(edit_block_at(&sb, 1).error.is_some());
        })
        .join()
        .unwrap();
    }
    #[test]
    fn committed_edit_does_not_coalesce() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            run_edit(&mut tracker, &mut sb, "e1", "foo.rs", 5);
            sb.mark_committed(0);
            run_edit(&mut tracker, &mut sb, "e2", "foo.rs", 40);
            assert_eq!(sb.len(), 2, "a committed row never merges");
            assert_eq!(edit_block_at(&sb, 0).hunks.len(), 1);
            assert_eq!(edit_block_at(&sb, 1).hunks.len(), 1);
        })
        .join()
        .unwrap();
    }
    #[test]
    fn untrusted_summary_edit_does_not_coalesce() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            run_edit(&mut tracker, &mut sb, "e1", "foo.rs", 5);
            let multi_diff =
                acp::ToolCall::new(acp::ToolCallId::new(Arc::from("e2")), "foo.rs".to_string())
                    .kind(acp::ToolKind::Edit)
                    .status(acp::ToolCallStatus::Completed)
                    .raw_input(Some(serde_json::json!({ "file_path" : "foo.rs" })))
                    .content(vec![
                        edit_diff_content("foo.rs", 40),
                        edit_diff_content("bar.rs", 7),
                    ])
                    .locations(vec![]);
            tracker.handle_update(acp::SessionUpdate::ToolCall(multi_diff), &meta(), &mut sb);
            assert_eq!(sb.len(), 2, "an untrusted summary never merges");
            assert!(edit_block_at(&sb, 1).summary_untrusted);
        })
        .join()
        .unwrap();
    }
    #[test]
    fn replay_precompleted_edits_coalesce_without_hl_queue() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            let replay = NotificationMeta {
                is_replay: true,
                ..Default::default()
            };
            tracker.handle_update(edit_tool_precompleted("e1", "foo.rs", 5), &replay, &mut sb);
            tracker.handle_update(edit_tool_precompleted("e2", "foo.rs", 40), &replay, &mut sb);
            assert_eq!(sb.len(), 1, "replayed adjacent edits merge like live ones");
            assert_eq!(hunk_lines(edit_block_at(&sb, 0)), vec![5, 40]);
            assert!(
                tracker.take_pending_edit_hl().is_empty(),
                "replay never queues full-file HL"
            );
        })
        .join()
        .unwrap();
    }
    #[test]
    fn coalesce_repoints_pending_edit_hl_to_survivor() {
        std::thread::spawn(|| {
            crate::appearance::cache::set_collapsed_edit_blocks(true);
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            run_edit(&mut tracker, &mut sb, "e1", "foo.rs", 5);
            run_edit(&mut tracker, &mut sb, "e2", "foo.rs", 40);
            let survivor = sb.get(0).unwrap().id;
            assert_eq!(
                tracker.take_pending_edit_hl(),
                vec![survivor],
                "HL queue holds the survivor exactly once, never the removed id"
            );
        })
        .join()
        .unwrap();
    }
    fn scrollback_with_respect_manual_folds() -> ScrollbackState {
        use crate::appearance::AppearanceConfig;
        let mut sb = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.scroll.respect_manual_folds = true;
        sb.set_appearance(appearance);
        sb
    }
    #[test]
    fn pinned_thinking_keeps_user_mode_across_finish_triggers() {
        use crate::scrollback::types::DisplayMode;
        crate::appearance::cache::set_show_thinking_blocks(true);
        let setup = || {
            let mut sb = scrollback_with_respect_manual_folds();
            let mut tracker = AcpUpdateTracker::new();
            tracker.handle_update(thought_chunk("deep thought"), &meta(), &mut sb);
            sb.prepare_layout(80, 40);
            sb.set_selected(Some(0));
            sb.expand_selected();
            let entry = sb.get(0).unwrap();
            assert!(entry.display_mode_pinned, "manual expand pins the entry");
            assert_eq!(entry.display_mode, DisplayMode::Expanded);
            (tracker, sb)
        };
        let assert_kept = |sb: &ScrollbackState, trigger: &str| {
            let entry = sb.get(0).unwrap();
            assert!(!entry.is_running, "{trigger}: thinking finished");
            assert_eq!(
                entry.display_mode,
                DisplayMode::Expanded,
                "{trigger}: pinned thinking must keep the user's mode"
            );
        };
        let (mut tracker, mut sb) = setup();
        tracker.handle_update(agent_chunk("answer"), &meta(), &mut sb);
        assert_kept(&sb, "agent chunk");
        let (mut tracker, mut sb) = setup();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Read, "src/main.rs"),
            &meta(),
            &mut sb,
        );
        assert_kept(&sb, "tool call");
        let (mut tracker, mut sb) = setup();
        tracker.finish_turn(&mut sb);
        assert_kept(&sb, "finish_turn");
        let mut sb = scrollback_with_respect_manual_folds();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(thought_chunk("deep thought"), &meta_stream(1000), &mut sb);
        sb.prepare_layout(80, 40);
        sb.set_selected(Some(0));
        sb.expand_selected();
        tracker.handle_update(thought_chunk("new stream"), &meta_stream(2000), &mut sb);
        assert_kept(&sb, "stream restart");
        let mut sb = scrollback_with_respect_manual_folds();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(thought_chunk("deep thought"), &meta(), &mut sb);
        tracker.handle_update(agent_chunk("answer"), &meta(), &mut sb);
        let entry = sb.get(0).unwrap();
        assert!(!entry.is_running);
        assert_eq!(
            entry.display_mode,
            DisplayMode::Collapsed,
            "unpinned thinking still auto-collapses"
        );
    }
    #[test]
    fn pinned_execute_keeps_user_mode_across_block_upgrades() {
        use crate::scrollback::types::DisplayMode;
        let mut sb = scrollback_with_respect_manual_folds();
        let mut tracker = AcpUpdateTracker::new();
        let tc_id = "call_pinned_exec";
        tracker.handle_update(
            tool_call(tc_id, acp::ToolKind::Execute, "Execute `sleep 5`"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(
            tool_update_in_progress(tc_id, b"tick 1\n"),
            &meta(),
            &mut sb,
        );
        sb.prepare_layout(80, 40);
        sb.set_selected(Some(0));
        sb.expand_selected();
        let entry = sb.get(0).unwrap();
        assert!(
            entry.display_mode_pinned,
            "expand after the first output tick (which makes Execute foldable) pins the entry"
        );
        assert_eq!(entry.display_mode, DisplayMode::Expanded);
        tracker.handle_update(
            tool_update_in_progress(tc_id, b"tick 1\ntick 2\n"),
            &meta(),
            &mut sb,
        );
        let entry = sb.get(0).unwrap();
        assert_eq!(
            entry.display_mode,
            DisplayMode::Expanded,
            "InProgress block upgrade must not reset a pinned entry (agent Execute default is Collapsed)"
        );
        assert!(entry.display_mode_pinned, "pin survives the block swap");
        tracker.handle_update(tool_update_completed(tc_id), &meta(), &mut sb);
        let entry = sb.get(0).unwrap();
        assert!(!entry.is_running);
        assert_eq!(
            entry.display_mode,
            DisplayMode::Expanded,
            "Completed upgrade + finish must not reset a pinned entry"
        );
    }
    #[test]
    fn respect_manual_folds_off_bypasses_finish_pin_guard() {
        use crate::appearance::AppearanceConfig;
        use crate::scrollback::types::DisplayMode;
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = scrollback_with_respect_manual_folds();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(thought_chunk("deep thought"), &meta(), &mut sb);
        sb.prepare_layout(80, 40);
        sb.set_selected(Some(0));
        sb.expand_selected();
        sb.collapse_selected();
        let entry = sb.get(0).unwrap();
        assert!(entry.display_mode_pinned);
        assert_eq!(entry.display_mode, DisplayMode::Truncated);
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.scroll.respect_manual_folds = false;
        sb.set_appearance(appearance);
        tracker.handle_update(agent_chunk("answer"), &meta(), &mut sb);
        assert_eq!(
            sb.get(0).unwrap().display_mode,
            DisplayMode::Collapsed,
            "flag off: finish applies the sticky mode to a pinned non-Expanded entry"
        );
    }
    /// Helper: build a NotificationMeta with a specific stream_start_ms.
    fn meta_stream(stream_start: i64) -> NotificationMeta {
        NotificationMeta {
            stream_start_ms: Some(stream_start),
            agent_timestamp_ms: Some(stream_start + 100),
            ..Default::default()
        }
    }
    /// Regression test: agent message (stream A) → thinking (stream B) → agent message (stream B).
    ///
    /// Without stream_start_ms boundary detection, stream B's agent message
    /// chunks were appended to stream A's entry because handle_thought_chunk
    /// never resets current_agent_msg.
    #[test]
    fn stream_start_breaks_agent_msg_across_streams() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let stream_a = meta_stream(1000);
        let stream_b = meta_stream(2000);
        tracker.handle_update(thought_chunk("thinking A"), &stream_a, &mut sb);
        tracker.handle_update(agent_chunk("answer A"), &stream_a, &mut sb);
        assert_eq!(sb.len(), 2);
        tracker.handle_update(thought_chunk("thinking B"), &stream_b, &mut sb);
        tracker.handle_update(agent_chunk("answer B"), &stream_b, &mut sb);
        assert_eq!(sb.len(), 4, "Each stream should produce separate entries");
        let agent_indices: Vec<usize> = (0..sb.len())
            .filter(|&i| matches!(sb.get(i).unwrap().block, RenderBlock::AgentMessage(_)))
            .collect();
        assert_eq!(
            agent_indices.len(),
            2,
            "Should have 2 separate agent message entries"
        );
        assert_ne!(
            sb.get(agent_indices[0]).unwrap().id,
            sb.get(agent_indices[1]).unwrap().id,
        );
    }
    /// Same stream_start_ms should NOT break messages — chunks append normally.
    #[test]
    fn same_stream_start_appends_normally() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let stream = meta_stream(1000);
        tracker.handle_update(agent_chunk("Hello "), &stream, &mut sb);
        tracker.handle_update(agent_chunk("world!"), &stream, &mut sb);
        assert_eq!(sb.len(), 1, "Same stream should append to one entry");
    }
    /// stream_start_ms change breaks thinking entries too.
    #[test]
    fn stream_start_breaks_thinking_across_streams() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let stream_a = meta_stream(1000);
        let stream_b = meta_stream(2000);
        tracker.handle_update(thought_chunk("thinking A"), &stream_a, &mut sb);
        assert!(tracker.current_thinking.is_some());
        tracker.handle_update(thought_chunk("thinking B"), &stream_b, &mut sb);
        assert_eq!(sb.len(), 2, "Each stream should get its own thinking entry");
        assert!(
            !sb.get(0).unwrap().is_running,
            "stream A thinking should be finished"
        );
    }
    /// Agent message in stream A, then agent message in stream B (no thinking between).
    #[test]
    fn stream_start_breaks_agent_msg_to_agent_msg() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let stream_a = meta_stream(1000);
        let stream_b = meta_stream(2000);
        tracker.handle_update(agent_chunk("message A"), &stream_a, &mut sb);
        tracker.handle_update(agent_chunk("message B"), &stream_b, &mut sb);
        assert_eq!(
            sb.len(),
            2,
            "Different streams should create separate agent messages"
        );
        assert!(
            !sb.get(0).unwrap().is_running,
            "stream A message should be finished"
        );
    }
    /// No stream_start_ms (old grok-shell) should not break anything.
    #[test]
    fn no_stream_start_ms_preserves_existing_behavior() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(agent_chunk("Hello "), &meta(), &mut sb);
        tracker.handle_update(agent_chunk("world!"), &meta(), &mut sb);
        assert_eq!(
            sb.len(),
            1,
            "Without stream_start_ms, chunks should append normally"
        );
    }
    /// finish_turn resets last_stream_start_ms so the next turn starts fresh.
    #[test]
    fn finish_turn_resets_stream_start() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let stream = meta_stream(1000);
        tracker.handle_update(agent_chunk("turn 1"), &stream, &mut sb);
        assert_eq!(tracker.last_stream_start_ms, Some(1000));
        tracker.finish_turn(&mut sb);
        assert_eq!(
            tracker.last_stream_start_ms, None,
            "finish_turn should reset last_stream_start_ms"
        );
        tracker.handle_update(agent_chunk("turn 2"), &stream, &mut sb);
        assert_eq!(sb.len(), 2);
    }
    #[test]
    fn activity_none_by_default() {
        let tracker = AcpUpdateTracker::new();
        assert_eq!(tracker.activity(), None);
    }
    #[test]
    fn activity_thinking_when_thought_chunks_arrive() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(thought_chunk("hmm..."), &meta(), &mut sb);
        assert_eq!(tracker.activity(), Some(TurnActivity::Thinking));
    }
    #[test]
    fn activity_responding_when_agent_chunks_arrive() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(agent_chunk("Here's my answer"), &meta(), &mut sb);
        assert_eq!(tracker.activity(), Some(TurnActivity::Responding));
    }
    #[test]
    fn activity_thinking_to_responding_transition() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(thought_chunk("thinking..."), &meta(), &mut sb);
        assert_eq!(tracker.activity(), Some(TurnActivity::Thinking));
        tracker.handle_update(agent_chunk("answer"), &meta(), &mut sb);
        assert_eq!(tracker.activity(), Some(TurnActivity::Responding));
    }
    #[test]
    fn activity_tool_running_when_tool_pending() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "cargo test"),
            &meta(),
            &mut sb,
        );
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::ToolRunning {
                title: "cargo test".into(),
                description: None,
            })
        );
    }
    /// Foreground execute tools often carry a human `description` in raw_input
    /// (e.g. sleep with "Wait 5 seconds…"). Surface it for the spinner.
    #[test]
    fn activity_tool_running_prefers_description_from_raw_input() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(
                    acp::ToolCallId::new(Arc::from("tc1")),
                    "run_terminal_command",
                )
                .kind(acp::ToolKind::Execute)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![])
                .raw_input(Some(serde_json::json!(
                    { "command" : "sleep 5 && echo done", "description" :
                    "Wait 5 seconds then print done", }
                )))
                .locations(vec![]),
            ),
            &meta(),
            &mut sb,
        );
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::ToolRunning {
                title: "sleep 5 && echo done".into(),
                description: Some("Wait 5 seconds then print done".into()),
            })
        );
    }
    /// The initial ToolCall registers with kind=Other and title=tool_id
    /// (e.g. "Shell"). When raw_input carries a `command` field, activity()
    /// should show the command instead of the bare tool name.
    #[test]
    fn activity_extracts_command_from_raw_input_regardless_of_kind() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let tc = acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(acp::ToolCallId::new(Arc::from("tc1")), "Shell".to_string())
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![])
                .raw_input(Some(
                    serde_json::json!({ "command" : "gt stack submit --no-edit" }),
                ))
                .locations(vec![]),
        );
        tracker.handle_update(tc, &meta(), &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::ToolRunning {
                title: "gt stack submit --no-edit".into(),
                description: None
            }),
        );
    }
    #[test]
    fn activity_strips_redundant_session_cd_prefix() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let cwd = std::path::PathBuf::from("/proj");
        tracker.set_session_cwd(cwd.clone());
        let command = format!("cd {} && echo hi", cwd.display());
        let tc = acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(
                acp::ToolCallId::new(Arc::from("tc-cd")),
                "Shell".to_string(),
            )
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Pending)
            .content(vec![])
            .raw_input(Some(serde_json::json!({ "command" : command })))
            .locations(vec![]),
        );
        tracker.handle_update(tc, &meta(), &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::ToolRunning {
                title: "echo hi".into(),
                description: None
            }),
        );
    }
    #[test]
    fn activity_strips_windows_shaped_session_cd_on_unix_host() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let cwd = std::path::PathBuf::from(r"C:\Users\a\proj");
        tracker.set_session_cwd(cwd.clone());
        let command = r"cd C:\Users\a\proj && cargo test";
        let tc = acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(
                acp::ToolCallId::new(Arc::from("tc-win")),
                "Shell".to_string(),
            )
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Pending)
            .content(vec![])
            .raw_input(Some(serde_json::json!({ "command" : command })))
            .locations(vec![]),
        );
        tracker.handle_update(tc, &meta(), &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::ToolRunning {
                title: "cargo test".into(),
                description: None
            }),
        );
    }
    #[test]
    fn execute_block_keeps_full_command_sets_header_display_when_peeled() {
        let tc = acp::ToolCall::new(acp::ToolCallId::new(Arc::from("tc-exec")), "Execute")
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .content(vec![])
            .raw_input(Some(
                serde_json::json!({ "command" : "cd /proj && echo hi" }),
            ))
            .locations(vec![]);
        let block = tool_call_to_block(&tc, Some(Path::new("/proj")));
        match &block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(exec)) => {
                assert_eq!(exec.command, "cd /proj && echo hi");
                assert_eq!(exec.header_display.as_deref(), Some("echo hi"));
            }
            other => panic!("expected Execute block, got {other:?}"),
        }
        assert_eq!(block.copy_meta().as_deref(), Some("cd /proj && echo hi"));
        let searchable = block.searchable_text().expect("searchable");
        assert!(
            searchable.contains("cd /proj && echo hi"),
            "searchable_text must retain full command: {searchable}"
        );
    }
    #[test]
    fn activity_none_after_finish_turn() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(agent_chunk("text"), &meta(), &mut sb);
        assert_eq!(tracker.activity(), Some(TurnActivity::Responding));
        tracker.finish_turn(&mut sb);
        assert_eq!(tracker.activity(), None);
    }
    #[test]
    fn activity_compaction_overrides_other_state() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(agent_chunk("text"), &meta(), &mut sb);
        assert_eq!(tracker.activity(), Some(TurnActivity::Responding));
        tracker.set_compaction_activity(Some(TurnActivity::AutoCompacting));
        assert_eq!(tracker.activity(), Some(TurnActivity::AutoCompacting));
        tracker.finish_turn(&mut sb);
        assert_eq!(tracker.activity(), None);
    }
    /// The blocking bg-plumbing tools are kept out of scrollback but the turn
    /// IS blocked on them — `activity()` must name the wait instead of the old
    /// generic `None` (→ "Waiting…"). Task-output tools only advertise once
    /// raw_input proves them blocking (`timeout_ms > 0`); before that the
    /// wait is not shown (display mirrors interject eligibility).
    #[test]
    fn activity_waiting_for_blocking_bg_plumbing_tools() {
        let cases = [
            ("wait_commands_or_subagents", WaitingReason::TasksComplete),
            ("wait_tasks", WaitingReason::TasksComplete),
            ("Await", WaitingReason::Sleep),
            ("Sleep 5s", WaitingReason::Sleep),
        ];
        for (title, expected) in cases {
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            tracker.handle_update(
                tool_call("t1", acp::ToolKind::Other, title),
                &meta(),
                &mut sb,
            );
            assert_eq!(
                tracker.activity(),
                Some(TurnActivity::Waiting(expected.clone())),
                "title {title:?} should produce {expected:?}"
            );
        }
        for title in ["get_command_or_subagent_output", "get_task_output"] {
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            tracker.handle_update(
                tool_call("t1", acp::ToolKind::Other, title),
                &meta(),
                &mut sb,
            );
            assert_eq!(
                tracker.activity(),
                None,
                "{title:?}: unknown blocking-ness must not advertise a wait"
            );
            tracker.handle_update(timeout_update("t1", 30_000), &meta(), &mut sb);
            assert_eq!(
                tracker.activity(),
                Some(TurnActivity::Waiting(WaitingReason::task_output())),
                "{title:?}: known-blocking wait must be named"
            );
        }
    }
    /// A known-blocking wait must beat an open (residual/pre-created) thought entry.
    #[test]
    fn activity_known_blocking_wait_outranks_thinking() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(thought_chunk("planning the wait…"), &meta(), &mut sb);
        assert_eq!(tracker.activity(), Some(TurnActivity::Thinking));
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "get_command_or_subagent_output"),
            &meta(),
            &mut sb,
        );
        tracker.pre_create_thinking(&mut sb);
        assert!(
            tracker.current_thinking.is_some(),
            "precondition: residual/pre-created thinking is live"
        );
        tracker.handle_update(timeout_update("t1", 60_000), &meta(), &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::task_output())),
            "known-blocking wait must beat Thinking for the status spinner"
        );
    }
    /// Thought chunks on the same stream must not erase an in-flight wait.
    #[test]
    fn thought_chunk_does_not_clear_active_blocking_wait() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let m = meta_stream(42);
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "get_command_or_subagent_output"),
            &m,
            &mut sb,
        );
        tracker.handle_update(timeout_update("t1", 60_000), &m, &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::task_output()))
        );
        tracker.handle_update(thought_chunk("still waiting…"), &m, &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::task_output())),
            "active wait must survive same-stream thought chunks"
        );
    }
    /// stream_start rollover must not pre-create a thought block during a wait.
    #[test]
    fn stream_start_does_not_pre_create_thinking_during_blocking_wait() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let m1 = meta_stream(1_000);
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "get_command_or_subagent_output"),
            &m1,
            &mut sb,
        );
        tracker.handle_update(timeout_update("t1", 30_000), &m1, &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::task_output()))
        );
        let m2 = meta_stream(9_999);
        tracker.handle_update(timeout_update("t1", 30_000), &m2, &mut sb);
        assert!(
            tracker.current_thinking.is_none(),
            "must not pre-create thinking while a known-blocking wait is live"
        );
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::task_output()))
        );
    }
    /// Regression: a resumed thought with no `stream_start_ms` must clear a
    /// stale wait (show Thinking, not a stuck wait spinner).
    #[test]
    fn resumed_thought_without_stream_start_clears_stale_wait() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let m = meta_stream(100);
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "get_command_or_subagent_output"),
            &m,
            &mut sb,
        );
        tracker.handle_update(timeout_update("t1", 60_000), &m, &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::task_output())),
            "precondition: sendable wait is live"
        );
        tracker.handle_update(
            thought_chunk("resuming, let me check the output…"),
            &meta(),
            &mut sb,
        );
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Thinking),
            "resumed-round thought (no stream_start) must clear the stale wait"
        );
    }
    /// ToolCallUpdate carrying a `timeout_ms` raw_input (the shape the shell
    /// sends on the first InProgress update).
    fn timeout_update(id: &str, timeout_ms: u64) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(id)),
            acp::ToolCallUpdateFields::new()
                .raw_input(Some(serde_json::json!({ "timeout_ms" : timeout_ms }))),
        ))
    }
    /// A blocking-wait reason is dropped when the suppressed tool completes, so
    /// the spinner stops showing it.
    #[test]
    fn blocking_wait_cleared_on_tool_completion() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "get_command_or_subagent_output"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(timeout_update("t1", 30_000), &meta(), &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::task_output()))
        );
        tracker.handle_update(tool_update_completed("t1"), &meta(), &mut sb);
        assert_eq!(tracker.activity(), None);
    }
    /// `finish_turn` clears any lingering blocking-wait state.
    #[test]
    fn blocking_wait_cleared_by_finish_turn() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "wait_tasks"),
            &meta(),
            &mut sb,
        );
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::TasksComplete))
        );
        tracker.finish_turn(&mut sb);
        assert_eq!(tracker.activity(), None);
    }
    /// `kill_*` is suppressed but doesn't block the turn — no waiting reason.
    #[test]
    fn kill_tool_is_not_a_blocking_wait() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "kill_command_or_subagent"),
            &meta(),
            &mut sb,
        );
        assert_eq!(tracker.activity(), None);
    }
    /// A response stream outranks a still-open blocking wait: show Responding.
    #[test]
    fn streaming_overrides_blocking_wait() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "get_task_output"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(timeout_update("t1", 30_000), &meta(), &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::task_output()))
        );
        tracker.handle_update(agent_chunk("partial"), &meta(), &mut sb);
        assert_eq!(tracker.activity(), Some(TurnActivity::Responding));
    }
    /// A same-stream (co-batched) thought must not clear an active wait.
    #[test]
    fn same_stream_thought_after_wait_tool_keeps_blocking_wait() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let m = meta_stream(100);
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "get_command_or_subagent_output"),
            &m,
            &mut sb,
        );
        tracker.handle_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from("t1")),
                acp::ToolCallUpdateFields::new().raw_input(Some(serde_json::json!(
                    { "task_ids" : ["bg-1"], "timeout_ms" : 180_000, }
                ))),
            )),
            &m,
            &mut sb,
        );
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::TaskOutput {
                task_ids: vec!["bg-1".into()],
                subject: None,
                waits: true,
            }))
        );
        tracker.handle_update(thought_chunk("planning next…"), &m, &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::TaskOutput {
                task_ids: vec!["bg-1".into()],
                subject: None,
                waits: true,
            })),
            "same-stream thought must not clear an active task-output wait"
        );
    }
    /// raw_input with task_ids on the first update populates the wait reason so
    /// the view can resolve a display subject from live bg task state.
    #[test]
    fn task_output_wait_captures_task_ids_from_raw_input_update() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "get_command_or_subagent_output"),
            &meta(),
            &mut sb,
        );
        assert_eq!(tracker.activity(), None);
        tracker.handle_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from("t1")),
                acp::ToolCallUpdateFields::new().raw_input(Some(serde_json::json!(
                    { "task_ids" : ["bg-123", "bg-456"], "timeout_ms" : 30_000,
                    }
                ))),
            )),
            &meta(),
            &mut sb,
        );
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::TaskOutput {
                task_ids: vec!["bg-123".into(), "bg-456".into()],
                subject: None,
                waits: true,
            }))
        );
    }
    /// `waits` derives from raw_input `timeout_ms`: 0/missing are instant
    /// polls (not interject-eligible); only >0 marks a blocking wait.
    #[test]
    fn task_output_waits_tracks_timeout_ms() {
        let tc = |raw: Option<serde_json::Value>| {
            acp::ToolCall::new(
                acp::ToolCallId::new(std::sync::Arc::from("w")),
                "get_task_output",
            )
            .kind(acp::ToolKind::Other)
            .status(acp::ToolCallStatus::Pending)
            .content(vec![])
            .raw_input(raw)
            .locations(vec![])
        };
        let waits = |raw| match blocking_wait_reason(&tc(raw)) {
            Some(WaitingReason::TaskOutput { waits, .. }) => waits,
            other => panic!("expected TaskOutput, got {other:?}"),
        };
        assert!(!waits(None), "missing raw_input defaults to instant poll");
        assert!(!waits(Some(serde_json::json!({ "task_ids" : ["a"] }))));
        assert!(!waits(Some(
            serde_json::json!({ "task_ids" : ["a"], "timeout_ms" : 0 })
        )));
        assert!(waits(Some(
            serde_json::json!({ "task_ids" : ["a"], "timeout_ms" : 1 })
        )));
    }
    #[test]
    fn waiting_reason_label_uses_subject_when_present() {
        assert_eq!(
            WaitingReason::TaskOutput {
                task_ids: vec!["t1".into()],
                subject: Some("compile release".into()),
                waits: false,
            }
            .label(),
            "compile release…"
        );
        assert_eq!(
            WaitingReason::task_output().label(),
            "Waiting on task output…"
        );
        assert_eq!(
            WaitingReason::TaskOutput {
                task_ids: vec![],
                subject: Some("\n  first line  \nsecond".into()),
                waits: false,
            }
            .label(),
            "first line…"
        );
        let long = "x".repeat(80);
        let label = WaitingReason::TaskOutput {
            task_ids: vec![],
            subject: Some(long),
            waits: false,
        }
        .label();
        assert!(label.ends_with('…'));
        let inner = label.strip_suffix('…').unwrap();
        assert_eq!(inner.chars().count(), MAX_ACTIVITY_SUBJECT_CHARS);
    }
    #[test]
    fn format_waiting_for_subject_matches_label_shape() {
        assert_eq!(format_waiting_for_subject("run tests"), "run tests…");
        assert_eq!(format_waiting_for_subject("   "), "Waiting on task output…");
    }
    /// A `task` ToolCall carrying the shell's `_meta.subagentBackground` flag.
    fn task_call_with_bg(id: &str, background: bool) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(acp::ToolCallId::new(Arc::from(id)), "task".to_string())
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![])
                .locations(vec![])
                .meta(Some(
                    [(
                        "subagentBackground".to_string(),
                        serde_json::Value::Bool(background),
                    )]
                    .into_iter()
                    .collect(),
                )),
        )
    }
    /// Shell-stamped foreground (`subagentBackground=false`): the subagent wait
    /// surfaces from frame 1 — no "Waiting for response…" flash.
    #[test]
    fn foreground_stamp_waits_on_subagent_from_frame_one() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(task_call_with_bg("t1", false), &meta(), &mut sb);
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::Subagent)),
            "a foreground-stamped subagent spawn surfaces the wait immediately"
        );
    }
    /// Shell-stamped background (`subagentBackground=true`, the default): the
    /// model keeps working, so no subagent wait surfaces — not even a one-frame
    /// flash.
    #[test]
    fn background_stamp_never_surfaces_subagent_wait() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(task_call_with_bg("t1", true), &meta(), &mut sb);
        assert_eq!(
            tracker.activity(),
            None,
            "a background-stamped subagent spawn must not surface any wait"
        );
    }
    /// Older shell with no `subagentBackground` stamp: fall back to the
    /// provisional foreground assumption (the refinement update still drops it
    /// for a background spawn).
    #[test]
    fn foreground_task_waits_on_subagent_immediately() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "task"),
            &meta(),
            &mut sb,
        );
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::Subagent))
        );
        tracker.finish_turn(&mut sb);
        assert_eq!(tracker.activity(), None);
    }
    /// A background subagent doesn't block the parent: once an update reveals
    /// `run_in_background`, the provisional subagent wait is dropped.
    #[test]
    fn background_task_clears_subagent_wait() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("t1", acp::ToolKind::Other, "task"),
            &meta(),
            &mut sb,
        );
        assert_eq!(
            tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::Subagent))
        );
        let bg_update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("t1")),
            acp::ToolCallUpdateFields::new().raw_input(Some(serde_json::json!(
                { "variant" : "Task", "task_id" : "sa1", "run_in_background"
                : true }
            ))),
        ));
        tracker.handle_update(bg_update, &meta(), &mut sb);
        assert_eq!(tracker.activity(), None);
    }
    fn available_commands_update(names: &[&str]) -> acp::SessionUpdate {
        acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
            names
                .iter()
                .map(|n| acp::AvailableCommand::new(n.to_string(), format!("{n} command")))
                .collect(),
        ))
    }
    #[test]
    fn tracker_captures_available_commands_update() {
        let mut tracker = AcpUpdateTracker::new();
        let mut sb = ScrollbackState::new();
        assert!(tracker.take_pending_acp_commands().is_none());
        let changed = tracker.handle_update(
            available_commands_update(&["flush", "compact"]),
            &meta(),
            &mut sb,
        );
        assert!(changed, "AvailableCommandsUpdate should signal redraw");
        let cmds = tracker
            .take_pending_acp_commands()
            .expect("should have pending");
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "flush");
        assert_eq!(cmds[1].name, "compact");
    }
    #[test]
    fn tracker_single_drain_clears_pending() {
        let mut tracker = AcpUpdateTracker::new();
        let mut sb = ScrollbackState::new();
        tracker.handle_update(available_commands_update(&["flush"]), &meta(), &mut sb);
        assert!(tracker.take_pending_acp_commands().is_some());
        assert!(tracker.take_pending_acp_commands().is_none());
    }
    #[test]
    fn tracker_latest_update_replaces_pending() {
        let mut tracker = AcpUpdateTracker::new();
        let mut sb = ScrollbackState::new();
        tracker.handle_update(available_commands_update(&["old"]), &meta(), &mut sb);
        tracker.handle_update(
            available_commands_update(&["new_a", "new_b"]),
            &meta(),
            &mut sb,
        );
        let cmds = tracker
            .take_pending_acp_commands()
            .expect("should have pending");
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "new_a");
        assert_eq!(cmds[1].name, "new_b");
    }
    #[test]
    fn parse_search_tool_results_grouped_format() {
        let json = serde_json::json!(
            { "results" : [{ "server" : "linear", "tools" : [{ "tool_name" :
            "linear__save_issue", "description" : "Create an issue", "score" : 0.8,
            "parameters" : ["stale_param_a", "stale_param_b"], "input_schema" : { "type"
            : "object", "properties" : { "title" : { "type" : "string" }, "team" : {
            "type" : "string" } }, "required" : ["title"] } }, { "tool_name" :
            "linear__list_issues", "description" : "List issues", "score" : 0.5,
            "parameters" : ["stale_query"], "input_schema" : { "type" : "object",
            "properties" : { "query" : { "type" : "string" } } } }] }, { "server" :
            "slack", "tools" : [{ "tool_name" : "slack__send_message", "description" :
            "Send a message", "score" : 0.3, "input_schema" : {} }] }],
            "total_hidden_tools" : 10, "status" : "ready" }
        );
        let content = serde_json::to_string_pretty(&json).unwrap();
        let results = parse_search_tool_results(&content);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].name, "linear__save_issue");
        assert_eq!(results[0].server, "linear");
        assert_eq!(results[0].description, "Create an issue");
        assert!((results[0].score - 0.8).abs() < f64::EPSILON);
        assert_eq!(results[1].name, "linear__list_issues");
        assert_eq!(results[1].server, "linear");
        assert_eq!(results[2].name, "slack__send_message");
        assert_eq!(results[2].server, "slack");
    }
    #[test]
    fn parse_search_tool_results_old_flat_format_returns_empty() {
        let json = serde_json::json!(
            { "results" : [{ "tool_name" : "linear__save_issue", "server_name" :
            "linear", "description" : "Create an issue", "score" : 0.8 }] }
        );
        let content = serde_json::to_string_pretty(&json).unwrap();
        let results = parse_search_tool_results(&content);
        assert!(
            results.is_empty(),
            "old flat format should not parse: {results:?}"
        );
    }
    #[test]
    fn tracker_extracts_tools_meta_from_available_commands_update() {
        let mut tracker = AcpUpdateTracker::new();
        let mut sb = ScrollbackState::new();
        let update = acp::SessionUpdate::AvailableCommandsUpdate(
            acp::AvailableCommandsUpdate::new(vec![acp::AvailableCommand::new(
                "loop".to_string(),
                "loop".to_string(),
            )])
            .meta(
                serde_json::json!({ "tools" : ["scheduler_create", "read_file"] })
                    .as_object()
                    .cloned(),
            ),
        );
        tracker.handle_update(update, &meta(), &mut sb);
        let tools = tracker
            .take_pending_acp_tools()
            .expect("tools list should be present");
        assert_eq!(tools, vec!["scheduler_create", "read_file"]);
        assert!(tracker.take_pending_acp_tools().is_none());
    }
    #[test]
    fn tracker_tools_meta_absent_when_meta_missing() {
        let mut tracker = AcpUpdateTracker::new();
        let mut sb = ScrollbackState::new();
        tracker.handle_update(available_commands_update(&["loop"]), &meta(), &mut sb);
        assert!(tracker.take_pending_acp_tools().is_none());
        assert!(tracker.take_pending_acp_commands().is_some());
    }
    #[test]
    fn parse_tools_meta_handles_shape_variants() {
        assert_eq!(
            parse_tools_meta(serde_json::json!({ "tools" : ["a", "b"] }).as_object()),
            Some(vec!["a".to_string(), "b".to_string()]),
        );
        assert_eq!(parse_tools_meta(None), None);
        assert_eq!(
            parse_tools_meta(serde_json::json!({ "other" : 1 }).as_object()),
            None,
        );
        assert_eq!(
            parse_tools_meta(serde_json::json!({ "tools" : "nope" }).as_object()),
            None,
        );
        assert_eq!(
            parse_tools_meta(serde_json::json!({ "tools" : ["a", 1, true, "b"] }).as_object()),
            Some(vec!["a".to_string(), "b".to_string()]),
        );
    }
    #[test]
    fn update_summary_is_compact_for_huge_tool_output() {
        let big = serde_json::to_value(vec![0u8; 100_000]).unwrap();
        let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("call-1")),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::InProgress))
                .raw_output(Some(big)),
        ));
        let summary = update_summary(&update);
        assert!(
            summary.len() < 200,
            "summary must not scale with payload: {} bytes",
            summary.len()
        );
        assert!(summary.contains("tool_call_update"), "{summary}");
        assert!(summary.contains("id=call-1"), "{summary}");
        assert!(summary.contains("arr(100000)"), "{summary}");
    }
    #[test]
    fn update_summary_reports_chunk_text_size() {
        let summary = update_summary(&agent_chunk(&"x".repeat(5000)));
        assert!(summary.contains("agent_message_chunk"), "{summary}");
        assert!(summary.contains("text=5000B"), "{summary}");
        assert!(summary.len() < 100, "{summary}");
    }
    #[test]
    fn json_size_hint_shapes() {
        assert_eq!(json_size_hint(&serde_json::json!(null)), "null");
        assert_eq!(json_size_hint(&serde_json::json!("abcd")), "str(4B)");
        assert_eq!(json_size_hint(&serde_json::json!([1, 2, 3])), "arr(3)");
        assert_eq!(
            json_size_hint(&serde_json::json!({ "output" : [1, 2], "cmd" : "ls" })),
            "obj(2 keys, ~4B)"
        );
    }
    #[test]
    fn meta_summary_handles_missing_fields() {
        assert_eq!(
            meta_summary(&NotificationMeta::default()),
            "seq=- tokens=- prompt=- stream_start=-"
        );
        let m = NotificationMeta {
            event_seq: Some(42),
            total_tokens: Some(1234),
            ..Default::default()
        };
        assert_eq!(
            meta_summary(&m),
            "seq=42 tokens=1234 prompt=- stream_start=-"
        );
    }
    #[test]
    fn build_and_parse_tools_meta_round_trip() {
        let names = vec!["scheduler_create".to_string(), "image_gen".to_string()];
        let wire = serde_json::json!({ "tools" : names });
        assert_eq!(parse_tools_meta(wire.as_object()), Some(names));
    }
    #[test]
    fn tracker_finish_turn_does_not_clear_pending_acp_commands() {
        let mut tracker = AcpUpdateTracker::new();
        let mut sb = ScrollbackState::new();
        tracker.handle_update(available_commands_update(&["persist"]), &meta(), &mut sb);
        tracker.finish_turn(&mut sb);
        assert!(tracker.take_pending_acp_commands().is_some());
    }
    #[test]
    fn tracker_finish_turn_does_not_clear_pending_acp_tools() {
        let mut tracker = AcpUpdateTracker::new();
        let mut sb = ScrollbackState::new();
        let update = acp::SessionUpdate::AvailableCommandsUpdate(
            acp::AvailableCommandsUpdate::new(vec![acp::AvailableCommand::new(
                "loop".to_string(),
                "loop".to_string(),
            )])
            .meta(
                serde_json::json!({ "tools" : ["scheduler_create"] })
                    .as_object()
                    .cloned(),
            ),
        );
        tracker.handle_update(update, &meta(), &mut sb);
        tracker.finish_turn(&mut sb);
        let tools = tracker
            .take_pending_acp_tools()
            .expect("pending_acp_tools should survive finish_turn");
        assert_eq!(tools, vec!["scheduler_create"]);
    }
    #[test]
    fn tracker_meta_less_update_preserves_prior_pending_acp_tools() {
        let mut tracker = AcpUpdateTracker::new();
        let mut sb = ScrollbackState::new();
        let with_tools = acp::SessionUpdate::AvailableCommandsUpdate(
            acp::AvailableCommandsUpdate::new(vec![]).meta(
                serde_json::json!({ "tools" : ["scheduler_create"] })
                    .as_object()
                    .cloned(),
            ),
        );
        tracker.handle_update(with_tools, &meta(), &mut sb);
        tracker.handle_update(available_commands_update(&["loop"]), &meta(), &mut sb);
        let tools = tracker
            .take_pending_acp_tools()
            .expect("prior pending tools should be preserved");
        assert_eq!(tools, vec!["scheduler_create"]);
    }
    /// Build a `ToolCall` that mimics the initial ACP register-early payload
    /// emitted by `acp_session.rs`: title comes from the model's function
    /// name, raw_input is None.
    fn initial_tool_call(id: &str, function_name: &str) -> acp::ToolCall {
        acp::ToolCall::new(
            acp::ToolCallId::new(Arc::from(id)),
            function_name.to_string(),
        )
        .kind(acp::ToolKind::Other)
        .status(acp::ToolCallStatus::Pending)
    }
    #[test]
    fn is_task_tool_recognizes_grok_build_variant() {
        assert!(is_task_tool(&initial_tool_call("tc1", "task")));
        let mut with_variant = initial_tool_call("tc2", "anything");
        with_variant.raw_input = Some(serde_json::json!({ "variant" : "Task" }));
        assert!(is_task_tool(&with_variant));
    }
    #[test]
    fn is_task_tool_rejects_unrelated_tools() {
        assert!(!is_task_tool(&initial_tool_call("tc1", "read_file")));
        assert!(!is_task_tool(&initial_tool_call("tc2", "Read")));
        assert!(!is_task_tool(&initial_tool_call("tc3", "todo_write")));
        let mut with_variant = initial_tool_call("tc4", "anything");
        with_variant.raw_input = Some(serde_json::json!({ "variant" : "Bash" }));
        assert!(!is_task_tool(&with_variant));
    }
    #[test]
    fn is_bg_plumbing_tool_recognizes_all_name_generations() {
        assert!(is_bg_plumbing_tool(&initial_tool_call(
            "t1",
            "get_command_or_subagent_output"
        )));
        assert!(is_bg_plumbing_tool(&initial_tool_call(
            "t2",
            "kill_command_or_subagent"
        )));
        assert!(is_bg_plumbing_tool(&initial_tool_call(
            "t3",
            "wait_commands_or_subagents"
        )));
        assert!(is_bg_plumbing_tool(&initial_tool_call(
            "t4",
            "get_task_output"
        )));
        assert!(is_bg_plumbing_tool(&initial_tool_call("t5", "kill_task")));
        assert!(is_bg_plumbing_tool(&initial_tool_call("t6", "wait_tasks")));
        assert!(is_bg_plumbing_tool(&initial_tool_call(
            "t7",
            "get_task_or_subagent_output"
        )));
        assert!(is_bg_plumbing_tool(&initial_tool_call(
            "t8",
            "kill_task_or_subagent"
        )));
        assert!(is_bg_plumbing_tool(&initial_tool_call(
            "t9",
            "wait_tasks_or_subagents"
        )));
        assert!(is_bg_plumbing_tool(&initial_tool_call("t10", "AwaitShell")));
        assert!(is_bg_plumbing_tool(&initial_tool_call("t10b", "Await")));
        let mut with_variant = initial_tool_call("t11", "anything");
        with_variant.raw_input = Some(serde_json::json!({ "variant" : "WaitTasks" }));
        assert!(is_bg_plumbing_tool(&with_variant));
        assert!(!is_bg_plumbing_tool(&initial_tool_call("t12", "read_file")));
        assert!(!is_bg_plumbing_tool(&initial_tool_call(
            "t13",
            "spawn_subagent"
        )));
    }
    #[test]
    fn pascal_case_task_tool_call_is_suppressed_from_scrollback() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Other, "Task"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 0, "PascalCase Task tool must be suppressed");
        assert!(tracker.suppressed_tools.contains("tc1"));
        tracker.handle_update(tool_update_completed("tc1"), &meta(), &mut sb);
        assert_eq!(
            sb.len(),
            0,
            "PascalCase Task updates must also be suppressed"
        );
    }
    #[test]
    fn task_tool_surfaces_as_subagent_wait_not_run_task() {
        for title in ["task", "Task"] {
            let mut sb = ScrollbackState::new();
            let mut tracker = AcpUpdateTracker::new();
            tracker.handle_update(
                tool_call("tc1", acp::ToolKind::Other, title),
                &meta(),
                &mut sb,
            );
            let activity = tracker.activity();
            assert!(
                !matches!(activity, Some(TurnActivity::ToolRunning { .. })),
                "suppressed task tool with title={title:?} must not surface as ToolRunning \
                 (would render as 'Run {title}' in the bottom turn-status spinner)"
            );
            assert_eq!(
                activity,
                Some(TurnActivity::Waiting(WaitingReason::Subagent)),
                "suppressed task tool with title={title:?} should surface as the subagent wait"
            );
        }
    }
    /// Helper: create an InProgress ToolCallUpdate with raw_input containing is_background.
    fn tool_update_in_progress_bg(id: &str, output_bytes: &[u8]) -> acp::SessionUpdate {
        use xai_grok_tools::types::output::{BashOutput, ToolOutput};
        let bash = BashOutput {
            output: output_bytes.to_vec(),
            output_for_prompt: String::new(),
            exit_code: 0,
            command: String::new(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: String::new(),
            output_file: String::new(),
            total_bytes: output_bytes.len(),
            output_delta: None,
            was_bare_echo: false,
        };
        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(id)),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::InProgress))
                .raw_output(serde_json::to_value(ToolOutput::Bash(bash)).ok())
                .raw_input(Some(serde_json::json!(
                    { "command" : "sleep 9999", "is_background" : true,
                    "description" : "long running task" }
                ))),
        ))
    }
    /// Regression: is_bg_tool() detected on first InProgress defers the tool
    /// before any scrollback entry is created.
    #[test]
    fn bg_tool_detected_at_first_update_defers_to_bg() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `sleep 9999`"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.pending_tools.len(), 1);
        let output_epoch = tracker.agent_output_epoch();
        let modified = tracker.handle_update(
            tool_update_in_progress_bg("tc1", b"started"),
            &meta(),
            &mut sb,
        );
        assert!(
            !modified,
            "bg tool deferral should suppress further output streaming"
        );
        assert_eq!(
            tracker.agent_output_epoch(),
            output_epoch,
            "deferral must not bump the epoch (re-pushes the parked marker)"
        );
        assert_eq!(sb.len(), 1, "real execute entry kept for demotion");
        assert!(
            !tracker.pending_tools.is_empty(),
            "tool stays in pending_tools for demotion entry_id"
        );
        assert!(
            tracker.bg_deferred_tools.contains_key("tc1"),
            "tool should be added to bg_deferred_tools"
        );
        assert_eq!(
            tracker.bg_deferred_tools.get("tc1").unwrap().as_deref(),
            Some("long running task"),
            "description should be extracted from raw_input"
        );
    }
    /// Regression: a bg-tool deferral (here dropping the placeholder row) must
    /// not bump `agent_output_epoch` — bumping re-pushed the parked marker.
    #[test]
    fn bg_tool_deferral_does_not_bump_agent_output_epoch() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Other, "run_terminal_command"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        let epoch = tracker.agent_output_epoch();
        let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc1")),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::InProgress))
                .raw_input(Some(serde_json::json!(
                    { "is_background" : true, "description" :
                    "long running task" }
                ))),
        ));
        assert!(!tracker.handle_update(update, &meta(), &mut sb));
        assert_eq!(sb.len(), 0, "placeholder dropped on deferral");
        assert!(tracker.bg_deferred_tools.contains_key("tc1"));
        assert_eq!(
            tracker.agent_output_epoch(),
            epoch,
            "deferral must not bump the epoch (re-pushes the parked marker)"
        );
    }
    /// Eager kind=Other title=`run_terminal_command` must not flash in the TUI.
    #[test]
    fn eager_execute_function_name_is_loading_placeholder_not_label() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Other, "run_terminal_command"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        match &sb.get(0).unwrap().block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(ex)) => {
                assert!(
                    ex.command.is_empty(),
                    "must not use function name as command: {:?}",
                    ex.command
                );
            }
            other => panic!("expected loading Execute, got {other:?}"),
        }
        let _ = tracker.handle_update(tool_update_in_progress_bg("tc1", b""), &meta(), &mut sb);
        assert_eq!(sb.len(), 1, "real command keeps entry for demotion");
        match &sb.get(0).unwrap().block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(ex)) => {
                assert_eq!(ex.command, "sleep 9999");
                assert_eq!(ex.description.as_deref(), Some("long running task"));
            }
            other => panic!("expected refined Execute, got {other:?}"),
        }
        assert!(tracker.bg_deferred_tools.contains_key("tc1"));
        assert!(tracker.pending_tools.contains_key("tc1"));
    }
    /// `raw_input.command: ""` must still map Other+function-name to loading Execute
    /// (not leave a bold `run_terminal_command` Other label).
    #[test]
    fn empty_command_key_still_maps_function_name_to_loading_execute() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Other, "run_terminal_command"),
            &meta(),
            &mut sb,
        );
        let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc1")),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::InProgress))
                .raw_input(Some(serde_json::json!(
                    { "command" : "", "description" : "still loading" }
                ))),
        ));
        tracker.handle_update(update, &meta(), &mut sb);
        assert_eq!(sb.len(), 1);
        match &sb.get(0).unwrap().block {
            RenderBlock::ToolCall(ToolCallBlock::Execute(ex)) => {
                assert!(ex.command.is_empty(), "empty command stays placeholder");
                assert_eq!(ex.description.as_deref(), Some("still loading"));
            }
            other => panic!("expected loading Execute, got {other:?}"),
        }
    }
    /// Real backgrounded `bash` command must not be treated as a placeholder.
    #[test]
    fn real_bash_command_is_not_dropped_on_bg_deferral() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "bash"),
            &meta(),
            &mut sb,
        );
        let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc1")),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::InProgress))
                .kind(Some(acp::ToolKind::Execute))
                .raw_input(Some(serde_json::json!(
                    { "command" : "bash", "is_background" : true, "description"
                    : "start a shell" }
                ))),
        ));
        tracker.handle_update(update, &meta(), &mut sb);
        assert_eq!(
            sb.len(),
            1,
            "real command=bash must be kept for demotion, not dropped as placeholder"
        );
        assert!(tracker.pending_tools.contains_key("tc1"));
        assert!(tracker.bg_deferred_tools.contains_key("tc1"));
    }
    /// A completed function-name `Other` tool call that still carries BashOutput
    /// must preserve the command output + exit-code error (mirror of the Execute
    /// arm), not drop it when the kind was never refined to Execute.
    #[test]
    fn completed_other_function_name_preserves_bash_output() {
        use xai_grok_tools::types::output::{BashOutput, ToolOutput};
        let bash = BashOutput {
            output: b"hello from bg\n".to_vec(),
            output_for_prompt: String::new(),
            exit_code: 3,
            command: "echo hi".to_string(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: "/tmp".to_string(),
            output_file: String::new(),
            total_bytes: 14,
            output_delta: None,
            was_bare_echo: false,
        };
        let tc = acp::ToolCall::new(
            acp::ToolCallId::new(Arc::from("tc1")),
            "run_terminal_command".to_string(),
        )
        .kind(acp::ToolKind::Other)
        .status(acp::ToolCallStatus::Completed)
        .content(vec![])
        .raw_input(Some(serde_json::json!({ "command" : "echo hi" })))
        .raw_output(serde_json::to_value(ToolOutput::Bash(bash)).ok())
        .locations(vec![]);
        match tool_call_to_block(&tc, None) {
            RenderBlock::ToolCall(ToolCallBlock::Execute(ex)) => {
                assert_eq!(ex.command, "echo hi");
                assert_eq!(ex.output.as_deref(), Some("hello from bg\n"));
                assert_eq!(ex.error.as_deref(), Some("exit code 3"));
            }
            other => panic!("expected Execute with output, got {other:?}"),
        }
    }
    /// Regression: when raw_input with is_background=true arrives after the
    /// Execute block was already created (late detection), the tool must still
    /// be moved to bg_deferred_tools so the task_backgrounded handler can
    /// demote the existing entry.
    #[test]
    fn bg_tool_late_detection_defers_existing_entry() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `sleep 9999`"),
            &meta(),
            &mut sb,
        );
        assert_eq!(tracker.pending_tools.len(), 1);
        tracker.handle_update(
            tool_update_in_progress("tc1", b"early output"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1, "Execute block should be created");
        assert!(
            tracker.pending_tools.get("tc1").unwrap().entry_id.is_some(),
            "entry_id should be set"
        );
        let modified = tracker.handle_update(
            tool_update_in_progress_bg("tc1", b"more output"),
            &meta(),
            &mut sb,
        );
        assert!(!modified, "late bg detection should suppress the update");
        assert!(
            tracker.pending_tools.contains_key("tc1"),
            "tool must stay in pending_tools so demotion handler can find entry_id"
        );
        assert!(
            tracker.bg_deferred_tools.contains_key("tc1"),
            "tool should also be in bg_deferred_tools to suppress future updates"
        );
        assert_eq!(sb.len(), 1);
    }
    /// Non-background Execute tools are unaffected by the late-detection path.
    #[test]
    fn non_bg_execute_unaffected_by_late_detection() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `ls`"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(
            tool_update_in_progress("tc1", b"file1.rs"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        assert_eq!(tracker.pending_tools.len(), 1);
        assert!(tracker.bg_deferred_tools.is_empty());
        tracker.handle_update(
            tool_update_in_progress("tc1", b"file1.rs\nfile2.rs"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1, "still one entry");
        assert_eq!(tracker.pending_tools.len(), 1, "still pending");
        assert!(
            tracker.bg_deferred_tools.is_empty(),
            "should not defer non-bg tool"
        );
    }
    /// Regression: handle_user_message must finish_running on pending tool entries
    /// before clearing them, otherwise Execute blocks are orphaned as "running".
    #[test]
    fn handle_user_message_finishes_pending_tool_entries() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `sleep 999`"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(
            tool_update_in_progress("tc1", b"waiting..."),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        assert!(sb.get(0).unwrap().is_running, "tool should be running");
        assert_eq!(tracker.pending_tools.len(), 1);
        tracker.handle_update(user_message("cancel that"), &meta(), &mut sb);
        assert!(
            !sb.get(0).unwrap().is_running,
            "Execute block should be finished by handle_user_message",
        );
        assert!(
            tracker.pending_tools.is_empty(),
            "pending_tools should be drained",
        );
        assert!(
            !sb.needs_animation(),
            "no entries should be animating after user message",
        );
    }
    /// Regression: finish_turn must call finish_running even for tools that are
    /// in bg_deferred_tools. The turn is over — the original Execute block must
    /// not stay orphaned as "running".
    #[test]
    fn finish_turn_finishes_bg_deferred_tool_entries() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            tool_call("tc1", acp::ToolKind::Execute, "Execute `long cmd`"),
            &meta(),
            &mut sb,
        );
        tracker.handle_update(
            tool_update_in_progress("tc1", b"early output"),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        assert!(sb.get(0).unwrap().is_running);
        tracker.handle_update(
            tool_update_in_progress_bg("tc1", b"more output"),
            &meta(),
            &mut sb,
        );
        assert!(
            tracker.bg_deferred_tools.contains_key("tc1"),
            "tool should be in bg_deferred_tools",
        );
        assert!(
            tracker.pending_tools.contains_key("tc1"),
            "tool should still be in pending_tools",
        );
        tracker.finish_turn(&mut sb);
        assert!(
            !sb.get(0).unwrap().is_running,
            "Execute block must be finished even when in bg_deferred_tools",
        );
        assert!(
            tracker.pending_tools.is_empty(),
            "pending_tools should be drained",
        );
        assert!(
            !sb.needs_animation(),
            "no entries should be animating after finish_turn",
        );
        assert!(
            tracker.bg_deferred_tools.contains_key("tc1"),
            "bg_deferred_tools must survive finish_turn",
        );
    }
    /// Helper: create a UserMessageChunk with displayText in content block meta.
    fn user_message_with_display_text(
        raw_text: &str,
        display_text: &str,
        display_as_skill: bool,
    ) -> acp::SessionUpdate {
        let mut meta_map = serde_json::Map::new();
        meta_map.insert(
            "displayText".into(),
            serde_json::Value::String(display_text.into()),
        );
        if display_as_skill {
            meta_map.insert("displayAsSkill".into(), serde_json::Value::Bool(true));
        }
        acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(raw_text.to_string())
                .meta(serde_json::Value::Object(meta_map).as_object().cloned()),
        )))
    }
    /// Replay with displayText in content meta shows clean display text
    /// instead of raw skill instructions.
    #[test]
    fn replay_display_text_override() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let raw = "# /loop -- schedule a recurring prompt\n\nParse the input below...";
        tracker.handle_update(
            user_message_with_display_text(raw, "/loop 1m print current time", true),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::UserPrompt(block) => {
                assert_eq!(
                    block.skill_token_ranges,
                    vec![0..5],
                    "leading /loop token styled as skill"
                );
                assert_eq!(block.text, "/loop 1m print current time");
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
    }
    /// displayText with displayAsSkill=false creates a regular prompt block.
    #[test]
    fn replay_display_text_non_skill() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let raw = "some raw wire content";
        tracker.handle_update(
            user_message_with_display_text(raw, "clean display text", false),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::UserPrompt(block) => {
                assert!(
                    block.skill_token_ranges.is_empty(),
                    "should NOT be styled as skill"
                );
                assert_eq!(block.text, "clean display text");
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
    }
    /// displayText with legacy XML raw text still skips the body block.
    #[test]
    fn replay_display_text_with_legacy_xml_skips_body() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let xml = "<command-name>implement</command-name>\n\
                    <command-message>/implement</command-message>\n\
                    <command-args>fix bug</command-args>";
        tracker.handle_update(
            user_message_with_display_text(xml, "/implement fix bug", true),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::UserPrompt(block) => {
                assert_eq!(block.skill_token_ranges, vec![0..10]);
                assert_eq!(block.text, "/implement fix bug");
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
        assert!(
            !tracker.handle_update(user_message("You are an orchestrator..."), &meta(), &mut sb),
            "skill body should be absorbed",
        );
        assert_eq!(sb.len(), 1, "no new entry for skill body");
    }
    /// Sessions without displayText still work via legacy fallback.
    #[test]
    fn replay_without_display_text_uses_legacy_detection() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let xml = "<command-name>commit</command-name>\n\
                    <command-message>/commit</command-message>\n\
                    <command-args>fix typo</command-args>";
        tracker.handle_update(user_message(xml), &meta(), &mut sb);
        assert_eq!(sb.len(), 1);
        let entry = sb.get(0).unwrap();
        match &entry.block {
            RenderBlock::UserPrompt(block) => {
                assert_eq!(block.skill_token_ranges, vec![0..7]);
                assert_eq!(block.text, "/commit fix typo");
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
        let mut sb2 = ScrollbackState::new();
        let mut tracker2 = AcpUpdateTracker::new();
        tracker2.handle_update(user_message("/help"), &meta(), &mut sb2);
        let entry2 = sb2.get(0).unwrap();
        match &entry2.block {
            RenderBlock::UserPrompt(block) => {
                assert_eq!(block.skill_token_ranges, vec![0..5]);
                assert_eq!(block.text, "/help");
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
    }
    fn user_message_with_chunk_meta(text: &str, chunk_meta: acp::Meta) -> acp::SessionUpdate {
        acp::SessionUpdate::UserMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                text.to_string(),
            )))
            .meta(Some(chunk_meta)),
        )
    }
    fn meta_with_prompt_id(prompt_id: &str) -> NotificationMeta {
        let mut m = meta();
        m.prompt_id = Some(prompt_id.to_string());
        m
    }
    /// Scrollback hide is type-driven: chunk meta `hideFromScrollback` or
    /// notification `promptId` → [`PromptOrigin::hide_user_echo_from_scrollback`].
    #[test]
    fn replay_hides_user_echo_by_origin_type() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let monitor_xml = "\
<monitor-event task_id=\"019e0000-0000-7000-8000-000000000001\">\n\
[CI] IN_PROGRESS=5 SUCCESS=38\n\
</monitor-event>";
        let mut hide_meta = acp::Meta::new();
        hide_meta.insert("hideFromScrollback".into(), serde_json::json!(true));
        assert!(
            !tracker.handle_update(
                user_message_with_chunk_meta(monitor_xml, hide_meta),
                &meta(),
                &mut sb,
            ),
            "hideFromScrollback meta must suppress regardless of text shape"
        );
        assert!(
            !tracker.handle_update(
                user_message("arbitrary model-only body"),
                &meta_with_prompt_id("task-completed-bg-1"),
                &mut sb,
            ),
            "task-completed origin must suppress via promptId"
        );
        assert!(
            !tracker.handle_update(
                user_message("drain body"),
                &meta_with_prompt_id("notifications-019e0000"),
                &mut sb,
            ),
            "notification-drain origin must suppress via promptId"
        );
        assert!(
            tracker.handle_update(
                user_message("please check the CI status"),
                &meta_with_prompt_id("scheduler-fired-abc"),
                &mut sb,
            ),
            "scheduler-fired must still render (cron path is separate)"
        );
        assert!(
            tracker.handle_update(user_message("please check the CI status"), &meta(), &mut sb),
            "real user text must still render"
        );
        assert_eq!(sb.len(), 2);
        assert!(
            !tracker.handle_update(user_message(monitor_xml), &meta(), &mut sb),
            "legacy untyped monitor XML still suppressed"
        );
        assert!(
            !tracker.handle_update(
                user_message("<system-reminder>\nBackground task done.\n</system-reminder>"),
                &meta(),
                &mut sb,
            ),
            "legacy system-reminder still suppressed"
        );
        let batched = "2 monitor events from 1 monitor (use get_command_or_subagent_output \
                       to identify each monitor):\n\n<monitor description=\"ticks\" \
                       task_id=\"t-1\">\n[1] tick-1\n[2] tick-2\n</monitor>";
        assert!(
            !tracker.handle_update(user_message(batched), &meta(), &mut sb),
            "legacy batched drain preamble still suppressed"
        );
        assert!(
            !tracker.handle_update(user_message("---"), &meta(), &mut sb),
            "legacy drain section separator still suppressed"
        );
        assert!(
            tracker.handle_update(
                user_message("what do these monitor events from my run mean (use plain words)?"),
                &meta(),
                &mut sb,
            ),
            "digit anchor: user text with both phrases but no leading count still renders"
        );
        assert_eq!(sb.len(), 3);
    }
    /// Helper: UserMessageChunk with `skillTokenRanges` in content-block meta.
    fn user_message_with_token_ranges(text: &str, ranges: serde_json::Value) -> acp::SessionUpdate {
        let mut meta_map = acp::Meta::new();
        meta_map.insert("skillTokenRanges".into(), ranges);
        acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text.to_string()).meta(Some(meta_map)),
        )))
    }
    /// `skillTokenRanges` meta round-trips into a token-styled block: same
    /// text, same ranges.
    #[test]
    fn replay_skill_token_ranges_styles_block() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            user_message_with_token_ranges(
                "great /pr-workflow all good now",
                serde_json::json!([[6, 18]]),
            ),
            &meta(),
            &mut sb,
        );
        assert_eq!(sb.len(), 1);
        match &sb.get(0).unwrap().block {
            RenderBlock::UserPrompt(block) => {
                assert_eq!(block.text, "great /pr-workflow all good now");
                assert_eq!(block.skill_token_ranges, vec![6..18]);
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
    }
    /// `skillTokenRanges` index the wire text, so a `displayText` override (a
    /// different coordinate space) IGNORES them — `displayAsSkill` keeps
    /// owning that branch. No first-party producer stamps both.
    #[test]
    fn replay_display_text_ignores_skill_token_ranges() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        let mut meta_map = serde_json::Map::new();
        meta_map.insert(
            "displayText".into(),
            serde_json::Value::String("/commit now".into()),
        );
        meta_map.insert("displayAsSkill".into(), serde_json::Value::Bool(true));
        meta_map.insert("skillTokenRanges".into(), serde_json::json!([[3, 10]]));
        tracker.handle_update(
            acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new("raw wire text".to_string()).meta(Some(meta_map)),
            ))),
            &meta(),
            &mut sb,
        );
        match &sb.get(0).unwrap().block {
            RenderBlock::UserPrompt(block) => {
                assert_eq!(block.text, "/commit now", "displayText still applies");
                assert_eq!(
                    block.skill_token_ranges,
                    vec![0..7],
                    "displayAsSkill styling (leading token), not the wire-space ranges"
                );
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
    }
    /// Malformed/out-of-bounds ranges never panic; the block degrades to a
    /// plain prompt (missing meta keeps the legacy fallbacks — pinned above).
    #[test]
    fn replay_malformed_skill_token_ranges_degrade_to_plain() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        tracker.handle_update(
            user_message_with_token_ranges(
                "short text",
                serde_json::json!([[100, 200], "bogus", [3]]),
            ),
            &meta(),
            &mut sb,
        );
        match &sb.get(0).unwrap().block {
            RenderBlock::UserPrompt(block) => {
                assert_eq!(block.text, "short text");
                assert!(
                    block.skill_token_ranges.is_empty(),
                    "invalid ranges dropped"
                );
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
    }
    #[test]
    fn call_mcp_tool_coerced_to_use_tool_renders_block() {
        let tc = acp::ToolCall::new(acp::ToolCallId::new(Arc::from("mcp1")), "grafana__search")
            .kind(acp::ToolKind::Other)
            .status(acp::ToolCallStatus::Completed)
            .content(vec![])
            .raw_input(Some(serde_json::json!(
                { "variant" : "UseTool", "tool_name" : "grafana__search",
                "tool_input" : { "query" : "alerts" } }
            )))
            .locations(vec![]);
        let block = tool_call_to_block(&tc, None);
        let RenderBlock::ToolCall(ToolCallBlock::UseTool(ut)) = block else {
            panic!("expected UseTool block, got {block:?}");
        };
        assert_eq!(ut.tool_name, "grafana__search");
    }
    #[test]
    fn call_mcp_tool_no_raw_input_does_not_panic() {
        let tc = acp::ToolCall::new(
            acp::ToolCallId::new(Arc::from("mcp2")),
            "linear__save_issue",
        )
        .kind(acp::ToolKind::Other)
        .status(acp::ToolCallStatus::Pending)
        .content(vec![])
        .locations(vec![]);
        let _block = tool_call_to_block(&tc, None);
    }
    #[test]
    fn cursor_todo_write_suppressed_by_title() {
        assert!(is_todo_tool(&initial_tool_call("tc1", "TodoWrite")));
        assert!(is_todo_tool(&initial_tool_call("tc2", "Updating plan")));
        assert!(is_todo_tool(&initial_tool_call("tc3", "todo_write")));
    }
    #[test]
    fn todo_write_suppressed_by_variant() {
        let mut tc = initial_tool_call("tc1", "anything");
        tc.raw_input = Some(serde_json::json!({ "variant" : "TodoWrite" }));
        assert!(is_todo_tool(&tc));
    }
    #[test]
    fn pascal_case_todo_write_suppressed_from_scrollback() {
        let mut sb = ScrollbackState::new();
        let mut tracker = AcpUpdateTracker::new();
        for (i, title) in ["TodoWrite", "Updating plan"].iter().enumerate() {
            let id = format!("tc-todo-{i}");
            tracker.handle_update(
                tool_call(&id, acp::ToolKind::Think, title),
                &meta(),
                &mut sb,
            );
            assert_eq!(
                sb.len(),
                0,
                "todo tool with title={title:?} must be suppressed"
            );
        }
    }
    /// Every video ToolInput variant must route through `media_gen_block` so
    /// `[Open Video]` uses the typed `MediaGenOutput.path` (not a regex scrape
    /// of the JSON prompt text — fragile on Windows with %-encoded session dirs).
    #[test]
    fn video_tool_variants_use_typed_path_not_generic_scrape() {
        use crate::scrollback::block::BlockContent;
        let dir = tempfile::tempdir().unwrap();
        let video_path = dir.path().join("1.mp4");
        std::fs::write(&video_path, b"fake-mp4").unwrap();
        let cases: &[(&str, ToolOutput)] = &[
            (
                "ImageToVideo",
                ToolOutput::ImageToVideo(xai_grok_tools::types::output::MediaGenOutput::new(
                    video_path.clone(),
                )),
            ),
            (
                "ReferenceToVideo",
                ToolOutput::ReferenceToVideo(xai_grok_tools::types::output::MediaGenOutput::new(
                    video_path.clone(),
                )),
            ),
        ];
        for (variant, output) in cases {
            let tc = acp::ToolCall::new(
                acp::ToolCallId::new(Arc::from(format!("media-{variant}"))),
                variant.to_string(),
            )
            .kind(acp::ToolKind::Other)
            .status(acp::ToolCallStatus::Completed)
            .content(vec![])
            .raw_input(Some(serde_json::json!({ "variant" : variant })))
            .raw_output(serde_json::to_value(output).ok())
            .locations(vec![]);
            let block = tool_call_to_block(&tc, None);
            let open_path = block
                .inline_open_button()
                .map(|(p, is_video)| {
                    assert!(is_video, "{variant}: expected video open button");
                    p
                })
                .or_else(|| block.video_references().first().map(|r| r.path.clone()))
                .unwrap_or_else(|| panic!("{variant}: missing media ref / open button"));
            assert_eq!(
                open_path, video_path,
                "{variant}: open path must be the typed MediaGenOutput.path"
            );
        }
    }
    #[test]
    fn media_gen_ref_skips_uploaded_only_video() {
        let output =
            ToolOutput::ImageToVideo(xai_grok_tools::types::output::MediaGenOutput::uploaded(
                "https://bucket.example/videos/x.mp4".into(),
            ));
        let tc = acp::ToolCall::new(
            acp::ToolCallId::new(Arc::from("zdr-upload")),
            "image_to_video",
        )
        .kind(acp::ToolKind::Other)
        .status(acp::ToolCallStatus::Completed)
        .content(vec![])
        .raw_input(Some(serde_json::json!({ "variant" : "ImageToVideo" })))
        .raw_output(serde_json::to_value(output).ok())
        .locations(vec![]);
        assert!(
            media_gen_ref(&tc).is_none(),
            "uploaded_url-only media must not claim a local open path"
        );
    }
    /// A tier-restricted (free / X Basic) imagine call short-circuits with the
    /// SuperGrok upsell as `ToolOutput::Text` on a `Completed` status. The media
    /// renderer has no file to open, so it must surface the upsell text in the
    /// card body (not a bare title) and must NOT mark the card as an error.
    #[test]
    fn tier_restricted_media_shows_upsell_text_not_error() {
        let upsell = "Image generation is a SuperGrok feature. Upgrade at \
             https://grok.com/supergrok?referrer=grok-build";
        let output = ToolOutput::Text(xai_grok_tools::types::output::TextOutput::from(upsell));
        let tc = acp::ToolCall::new(
            acp::ToolCallId::new(Arc::from("tier-restricted-img")),
            "image_gen",
        )
        .kind(acp::ToolKind::Other)
        .status(acp::ToolCallStatus::Completed)
        .content(vec![acp::ToolCallContent::Content(acp::Content::new(
            acp::ContentBlock::Text(acp::TextContent::new(upsell)),
        ))])
        .raw_input(Some(serde_json::json!({ "variant" : "ImageGen" })))
        .raw_output(serde_json::to_value(output).ok())
        .locations(vec![]);
        let RenderBlock::ToolCall(ToolCallBlock::Other(block)) = tool_call_to_block(&tc, None)
        else {
            panic!("expected an Other tool-call block");
        };
        assert!(
            block.is_success(),
            "the upsell is a successful result, not an error"
        );
        assert!(
            block
                .output
                .as_deref()
                .unwrap_or_default()
                .contains("SuperGrok"),
            "upsell text must be shown in the card body, got: {:?}",
            block.output
        );
    }
}
