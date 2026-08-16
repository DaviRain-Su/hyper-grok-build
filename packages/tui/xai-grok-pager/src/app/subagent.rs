//! Subagent business types.
//!
//! Tracking state for spawned child sessions. [`SubagentInfo`] is the single
//! source of truth — used by both the subagent pane (display) and the
//! permission view (provenance labels).
use serde::Deserialize;
use std::sync::Arc;
use std::time::Instant;
use xai_grok_shell::session::storage::{
    ReplayEmission, ReplayLookupFallback, ReplayPathHint, ReplayedUpdate, replay_would_emit,
    stream_replay_updates_at_hinted,
};
/// Enriched subagent tracking info.
///
/// Keyed by `child_session_id` in `AgentView::subagent_sessions`.
/// Populated from `SubagentSpawned` notifications, updated by
/// `SubagentProgress` and `SubagentFinished`.
#[derive(Debug, Clone)]
pub struct SubagentInfo {
    pub subagent_id: Arc<str>,
    pub child_session_id: Arc<str>,
    pub description: Arc<str>,
    pub subagent_type: Arc<str>,
    pub persona: Option<Arc<str>>,
    pub role: Option<Arc<str>>,
    pub model: Option<Arc<str>>,
    /// "new" or "resumed".
    pub context_source: Option<Arc<str>>,
    pub resumed_from: Option<Arc<str>>,
    /// "read-only", "read-write", "execute", or "all".
    pub capability_mode: Option<Arc<str>>,
    pub workflow_run_id: Option<Arc<str>>,
    /// Effective runtime limits reported when the child was spawned.
    pub budget: Option<xai_grok_shell::extensions::notification::SubagentBudgetInfo>,
    /// Whether the context was normalized into `<background_context>`.
    pub context_normalized: bool,
    pub parent_prompt_id: Option<Arc<str>>,
    pub started_at: Instant,
    /// Wall-clock time of the most recent `SubagentProgress` /
    /// `SubagentFinished` update. For
    /// running subagents this is the "last activity" timestamp the
    /// dashboard uses for sort + age display; for finished subagents
    /// this is the finish time, not the start.
    ///
    /// Initialised to `started_at` so that brand-new subagents with
    /// no progress notifications yet still sort correctly.
    pub last_progress_at: Instant,
    /// Lifecycle dedup tombstone: a child emits one terminal transition, so
    /// duplicate finishes must not re-finalize it and duplicate spawns must
    /// never replace this terminal state.
    pub finished: bool,
    /// "completed", "failed", or "cancelled".
    pub status: Option<Arc<str>>,
    pub error: Option<Arc<str>>,
    /// Stable reason for runtime-driven finalization/cancellation.
    pub termination_reason: Option<Arc<str>>,
    /// Cumulative model usage captured at completion.
    pub usage: Option<xai_grok_shell::extensions::notification::SubagentUsageInfo>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: Option<u64>,
    pub tool_calls: Option<u32>,
    pub turns: Option<u32>,
    pub turn_count: Option<u32>,
    pub tool_call_count: Option<u32>,
    pub tokens_used: Option<u64>,
    pub context_window_tokens: Option<u64>,
    /// 0-100.
    pub context_usage_pct: Option<u8>,
    pub tools_used: Vec<Arc<str>>,
    pub error_count: Option<u32>,
    /// Live activity label ("Thinking", "Running: cargo build") mirroring
    /// the scrollback block's field; feeds the tasks pane row and the
    /// dashboard activity column. Cleared on `SubagentFinished`.
    pub activity_label: Option<String>,
    /// Affects scrollback rendering (background shows "started:"/"completed:").
    pub is_background: bool,
    /// Set on kill request, cleared on `SubagentFinished`.
    pub pending_kill: bool,
    /// When the kill request was sent. Used to auto-clear `pending_kill`
    /// after a timeout so the user can retry if the notification is lost.
    pub kill_requested_at: Option<Instant>,
    /// Set on spawn, updated on finish.
    pub scrollback_entry_id: Option<crate::scrollback::entry::EntryId>,
    pub prompt: Option<Arc<str>>,
    pub child_cwd: Option<Arc<str>>,
    pub worktree_path: Option<Arc<str>>,
    /// Where the child's authoritative transcript lives. Drives the
    /// deferred replay-on-open and the eviction decision for the view.
    pub transcript: ChildTranscript,
}
/// Where a child's authoritative transcript lives.
///
/// A single state feeds both the replay-on-open and the eviction predicates
/// so the two cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChildTranscript {
    /// No disk copy proven yet: the next fullscreen open replays
    /// `updates.jsonl`. Failed and `Empty` replays stay here so a lagging
    /// child persistence flush is retried instead of settling on a husk.
    #[default]
    NeedsReplay,
    /// An emitting replay proved disk reproduces the transcript: the
    /// retained view may be dropped and rebuilt.
    DiskBacked,
    /// The in-memory view is the only copy (disk resolved to nothing while
    /// the view held content). Exempt from eviction, like a background
    /// child's: evicting would lose the transcript permanently.
    MemoryOnly,
}
impl ChildTranscript {
    /// Whether the next fullscreen open must attempt the disk replay.
    pub(crate) fn needs_replay(self) -> bool {
        matches!(self, Self::NeedsReplay)
    }
    /// Whether the retained view may be dropped and rebuilt from disk later.
    pub(crate) fn evictable(self) -> bool {
        matches!(self, Self::DiskBacked)
    }
    /// Record a replay attempt: only an emitting replay proves the disk
    /// copy. `Empty` and failed replays stay `NeedsReplay` so the next open
    /// retries (post-eviction, disk is the only copy).
    pub(crate) fn record_replay(&mut self, outcome: &std::io::Result<ReplayEmission>) {
        if matches!(outcome, Ok(ReplayEmission::Emitted)) {
            *self = Self::DiskBacked;
        }
    }
    /// The view was reset to the prompt-only baseline: rebuild on next open.
    pub(crate) fn evicted(&mut self) {
        *self = Self::NeedsReplay;
    }
    /// Disk cannot reproduce the content the view holds: never evict.
    pub(crate) fn discovered_memory_only(&mut self) {
        *self = Self::MemoryOnly;
    }
}
impl SubagentInfo {
    /// Whether the subagent is currently running (not finished).
    pub fn is_running(&self) -> bool {
        !self.finished
    }
    /// Elapsed time since spawn.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }
    /// Display-ready elapsed duration.
    /// Uses authoritative `duration_ms` from SubagentFinished when available,
    /// falls back to live wall-clock elapsed for running subagents.
    pub fn display_elapsed(&self) -> std::time::Duration {
        if self.finished {
            self.duration_ms
                .map(std::time::Duration::from_millis)
                .unwrap_or_else(|| self.elapsed())
        } else {
            self.elapsed()
        }
    }
}
/// Minimal pager-side view of the shell's on-disk `SubagentMeta`.
#[derive(Debug, Deserialize)]
struct SubagentMetaSlice {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    child_cwd: Option<String>,
    #[serde(default)]
    worktree_path: Option<String>,
}
/// Grok home for the replay path. In production this is just `grok_home()`; the
/// whole test override below is `#[cfg(test)]`, so no thread-local or dead
/// always-false branch ships in release.
#[cfg(not(test))]
fn effective_grok_home() -> std::path::PathBuf {
    xai_grok_shell::util::grok_home::grok_home()
}
#[cfg(test)]
thread_local! {
    static REPLAY_GROK_HOME: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}
/// Override grok home for disk-replay unit tests (thread-local).
#[cfg(test)]
pub(crate) fn set_replay_grok_home_for_tests(home: Option<std::path::PathBuf>) {
    REPLAY_GROK_HOME.with(|h| *h.borrow_mut() = home);
}
#[cfg(test)]
fn effective_grok_home() -> std::path::PathBuf {
    if let Some(home) = REPLAY_GROK_HOME.with(|h| h.borrow().clone()) {
        return home;
    }
    xai_grok_shell::util::grok_home::grok_home()
}
/// Best-effort enrichment from the shell's on-disk `meta.json`.
pub(crate) fn enrich_from_meta(
    info: &mut SubagentInfo,
    parent_cwd: &std::path::Path,
    parent_session_id: &str,
) {
    enrich_from_meta_with_home(info, &effective_grok_home(), parent_cwd, parent_session_id);
}
fn enrich_from_meta_with_home(
    info: &mut SubagentInfo,
    grok_home: &std::path::Path,
    parent_cwd: &std::path::Path,
    parent_session_id: &str,
) {
    let meta_path = grok_home
        .join("sessions")
        .join(urlencoding::encode(&parent_cwd.to_string_lossy()).as_ref())
        .join(parent_session_id)
        .join("subagents")
        .join(info.subagent_id.as_ref())
        .join("meta.json");
    let content = match std::fs::read_to_string(&meta_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "meta.json not found");
            return;
        }
    };
    let meta: SubagentMetaSlice = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "meta.json parse failed");
            return;
        }
    };
    info.prompt = meta.prompt.map(Arc::from);
    info.child_cwd = meta.child_cwd.map(Arc::from);
    info.worktree_path = meta.worktree_path.map(Arc::from);
}
/// Best-effort streamed replay of a child's inherited conversation.
///
/// `Err`: read failed, so callers must not mark the child replayed (the next
/// open retries; post-eviction, disk is the only copy). `Ok(Empty)`: nothing
/// on disk, so callers holding detached content restore it. The `child_cwd`
/// hint skips the full RelocationView scan when it matches.
pub(crate) fn replay_inherited_updates(
    child_view: &mut crate::app::agent_view::AgentView,
    child_session_id: &str,
    parent_cwd: &std::path::Path,
    child_cwd: Option<&std::path::Path>,
    fallback: ReplayLookupFallback,
) -> std::io::Result<ReplayEmission> {
    let home = effective_grok_home();
    let hint = ReplayPathHint {
        parent_cwd: Some(parent_cwd),
        child_cwd,
        fallback,
    };
    child_view.scrollback.begin_batch();
    let outcome =
        stream_replay_updates_at_hinted(child_session_id, &home, hint, |update| match update {
            ReplayedUpdate::Acp(update, meta) => {
                let mut meta = crate::acp::meta::NotificationMeta::from_json(meta.as_ref());
                meta.is_replay = true;
                child_view
                    .session
                    .handle_update(update, &meta, &mut child_view.scrollback);
            }
            ReplayedUpdate::Xai(update) => {
                crate::app::acp_handler::apply_child_view_session_event(child_view, &update, false);
            }
        });
    child_view.scrollback.end_batch();
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!(session_id = %child_session_id, error = %e, "failed to read updates for replay");
            return Err(e);
        }
    };
    if outcome == ReplayEmission::Emitted {
        crate::memory_release::release_retained_memory_with("subagent-replay");
    }
    Ok(outcome)
}
/// Compare two strings after collapsing internal whitespace (no allocation).
pub(crate) fn subagent_prompt_text_eq(a: &str, b: &str) -> bool {
    let mut aw = a.split_whitespace();
    let mut bw = b.split_whitespace();
    loop {
        match (aw.next(), bw.next()) {
            (Some(x), Some(y)) if x == y => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}
/// True when replay (or prior injection) already surfaced the subagent task prompt.
pub(crate) fn child_scrollback_already_shows_prompt(
    scrollback: &crate::scrollback::state::ScrollbackState,
    prompt: &str,
) -> bool {
    if prompt.trim().is_empty() {
        return false;
    }
    for i in 0..scrollback.len() {
        let Some(entry) = scrollback.entry(i) else {
            continue;
        };
        let block_text = match &entry.block {
            crate::scrollback::block::RenderBlock::UserPrompt(b) => Some(b.text.as_str()),
            _ => None,
        };
        if let Some(t) = block_text
            && subagent_prompt_text_eq(t, prompt)
        {
            return true;
        }
    }
    false
}
/// True when a scrollback holds nothing beyond injected task prompts.
fn scrollback_is_prompt_only(scrollback: &crate::scrollback::state::ScrollbackState) -> bool {
    let len = scrollback.len();
    if len == 0 {
        return true;
    }
    for i in 0..len {
        let Some(entry) = scrollback.entry(i) else {
            continue;
        };
        match &entry.block {
            crate::scrollback::block::RenderBlock::UserPrompt(_) => {}
            _ => return false,
        }
    }
    true
}
/// True when a scrollback holds nothing a fresh rebuild would not recreate:
/// injected task prompts plus the `TurnCompleted` footer stamped after a
/// rebuild. Restoring such a husk as the authoritative copy would settle it
/// `MemoryOnly` and permanently suppress the replay of a late persistence
/// flush.
fn scrollback_is_rebuildable_husk(scrollback: &crate::scrollback::state::ScrollbackState) -> bool {
    for i in 0..scrollback.len() {
        let Some(entry) = scrollback.entry(i) else {
            continue;
        };
        match &entry.block {
            crate::scrollback::block::RenderBlock::UserPrompt(_) => {}
            crate::scrollback::block::RenderBlock::SessionEvent(b)
                if matches!(
                    b.event,
                    crate::scrollback::blocks::SessionEvent::TurnCompleted { .. }
                ) => {}
            _ => return false,
        }
    }
    true
}
/// Replay child `updates.jsonl` on fullscreen open when not yet replayed:
/// finished foreground children always rebuild from disk; live/background
/// views hydrate only while still prompt-only.
pub(crate) fn ensure_subagent_child_replayed(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) {
    let Some(info) = parent.subagent_sessions.get(child_sid) else {
        return;
    };
    if !info.transcript.needs_replay() {
        return;
    }
    let finished = info.finished;
    let is_background = info.is_background;
    let resumed = info.resumed_from.is_some() || info.context_source.as_deref() == Some("resumed");
    let finished_elapsed = finished
        .then_some(info.duration_ms)
        .flatten()
        .map(std::time::Duration::from_millis);
    let Some(child_view) = parent.subagent_views.get(child_sid) else {
        return;
    };
    if (!finished || is_background) && !scrollback_is_prompt_only(&child_view.scrollback) {
        return;
    }
    let taken = if finished && !is_background {
        reset_child_view_to_prompt(parent, child_sid)
    } else {
        None
    };
    let fallback = if (finished && !is_background) || resumed {
        ReplayLookupFallback::Relocation
    } else {
        ReplayLookupFallback::HintedOnly
    };
    let outcome = replay_child_and_mark(parent, child_sid, fallback);
    let restore = match &outcome {
        Ok(ReplayEmission::Emitted) => false,
        Ok(ReplayEmission::Empty) => taken
            .as_ref()
            .is_some_and(|t| !scrollback_is_rebuildable_husk(&t.scrollback)),
        Err(_) => true,
    };
    let mut restored = false;
    if restore
        && let Some(taken) = taken
        && let Some(child_view) = parent.subagent_views.get_mut(child_sid)
    {
        child_view.restore_replay_rebuilt_state(taken);
        restored = true;
        if matches!(outcome, Ok(ReplayEmission::Empty))
            && let Some(info) = parent.subagent_sessions.get_mut(child_sid)
        {
            info.transcript.discovered_memory_only();
        }
    }
    let parent_turn_running =
        parent.session.state.is_turn_running() || parent.session.state.is_cancelling();
    if let Some(child_view) = parent.subagent_views.get_mut(child_sid) {
        match finished_elapsed {
            Some(elapsed) if outcome.is_ok() && !restored => {
                finalize_finished_child_view(child_view, elapsed)
            }
            Some(_) => {}
            None if !parent_turn_running => {
                child_view.scrollback.finish_all_running();
            }
            None => {}
        }
    }
}
/// Replay the child's on-disk transcript and record the outcome on
/// [`SubagentInfo::transcript`]: only an emitting replay marks the child
/// `DiskBacked`; `Empty` and failed reads stay `NeedsReplay` so the next
/// open retries (a lagging persistence flush must not settle as replayed).
/// Shared by the eager live-spawn path and the deferred open.
pub(crate) fn replay_child_and_mark(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
    fallback: ReplayLookupFallback,
) -> std::io::Result<ReplayEmission> {
    let parent_cwd = parent.session.cwd.clone();
    let child_cwd = parent
        .subagent_sessions
        .get(child_sid)
        .and_then(|info| info.child_cwd.clone());
    let mut outcome = Ok(ReplayEmission::Empty);
    if let Some(child_view) = parent.subagent_views.get_mut(child_sid) {
        outcome = replay_inherited_updates(
            child_view,
            child_sid,
            &parent_cwd,
            child_cwd.as_deref().map(std::path::Path::new),
            fallback,
        );
    }
    if let Some(info) = parent.subagent_sessions.get_mut(child_sid) {
        info.transcript.record_replay(&outcome);
    }
    outcome
}
/// Reset a child view to the resume-state baseline: detach every
/// replay-rebuilt field, drop the media caches, and re-inject the task
/// prompt (`expect_user_echo` lets a later replay dedup its on-disk echo).
///
/// Returns the detached state so a rebuild that produced nothing can
/// restore it (eviction drops it). Lossless because the replay fails before
/// any callback, and restore raises the id floors past the discarded
/// sibling's allocations.
#[must_use = "dropping the detached state destroys the only in-memory copy; eviction must drop it explicitly"]
fn reset_child_view_to_prompt(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) -> Option<crate::app::agent_view::ReplayRebuiltState> {
    let prompt = parent
        .subagent_sessions
        .get(child_sid)
        .and_then(|info| info.prompt.clone())
        .filter(|p| !p.trim().is_empty());
    let child_view = parent.subagent_views.get_mut(child_sid)?;
    let taken = child_view.take_replay_rebuilt_state();
    child_view.inline_media_cache = Default::default();
    child_view.inline_media_load_failed = Default::default();
    if let Some(prompt) = prompt {
        child_view
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::user_prompt(
                prompt.as_ref(),
            ));
        child_view.session.tracker.expect_user_echo();
    }
    Some(taken)
}
/// Evict a finished child view's retained transcript. Child views live for
/// the whole process, so without this every finished scrollback + tracker is
/// retained forever. The view resets to the resume-state baseline (task
/// prompt only) and first open rebuilds it from disk via
/// [`ensure_subagent_child_replayed`], footer included.
///
/// Returns `false` when a guard applies and the caller should finalize in
/// place: the child open fullscreen (never blanked under the user; dashboard
/// *peek* tails do blank, like resumed sessions), unfinished children,
/// background children (they receive post-finish output not on disk), and
/// memory-only transcripts (disk has nothing to rebuild from).
///
/// Content no replay ever verified (`NeedsReplay`: a live-streamed child,
/// or a restored failed rebuild) is dropped only after a disk probe proves
/// the persisted transcript would actually emit on replay. A probe miss
/// keeps the view (and `NeedsReplay`, so later opens/closes still retry): a
/// child whose persistence never wrote, wrote a non-emitting prefix, or
/// whose finish raced the flush, cannot lose its only copy to a close. The
/// next open's `Empty` rebuild then restores it and settles `MemoryOnly`.
pub(crate) fn evict_finished_child_view(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) -> bool {
    if parent.active_subagent.as_deref() == Some(child_sid) {
        return false;
    }
    let Some(info) = parent.subagent_sessions.get(child_sid) else {
        return false;
    };
    if !info.finished
        || info.is_background
        || matches!(info.transcript, ChildTranscript::MemoryOnly)
    {
        return false;
    }
    let Some(child_view) = parent.subagent_views.get(child_sid) else {
        if let Some(info) = parent.subagent_sessions.get_mut(child_sid) {
            info.transcript.evicted();
        }
        return true;
    };
    let had_content = !scrollback_is_prompt_only(&child_view.scrollback)
        || !child_view.inline_media_cache.is_empty();
    if !info.transcript.evictable() && had_content {
        let child_cwd = info.child_cwd.clone();
        let hint = ReplayPathHint {
            parent_cwd: Some(&parent.session.cwd),
            child_cwd: child_cwd.as_deref().map(std::path::Path::new),
            fallback: ReplayLookupFallback::HintedOnly,
        };
        if !replay_would_emit(child_sid, &effective_grok_home(), hint).unwrap_or(false) {
            return false;
        }
    }
    if let Some(info) = parent.subagent_sessions.get_mut(child_sid) {
        info.transcript.evicted();
    }
    drop(reset_child_view_to_prompt(parent, child_sid));
    if had_content {
        crate::memory_release::request_release_after_draw_with("subagent-evict");
    }
    true
}
/// Finalize a finished child view: end the turn and append the `TurnCompleted`
/// footer. Shared by the live `SubagentFinished` path and the deferred resume path.
///
/// Idempotent on the *trailing* footer: kill reconciliation or a late replay
/// rebuild that re-finalizes an already-finished child must not append a second
/// completed line. An earlier turn's `TurnCompleted` deeper in the transcript
/// must not suppress a later turn's footer.
pub(crate) fn finalize_finished_child_view(
    child_view: &mut crate::app::agent_view::AgentView,
    elapsed: std::time::Duration,
) {
    child_view
        .session
        .tracker
        .finish_turn(&mut child_view.scrollback);
    child_view.scrollback.finish_all_running();
    let already_has_trailing_completed_footer = child_view.scrollback.last().is_some_and(|e| {
        matches!(
            &e.block,
            crate::scrollback::block::RenderBlock::SessionEvent(seb)
                if matches!(
                    seb.event,
                    crate::scrollback::blocks::SessionEvent::TurnCompleted { .. }
                )
        )
    });
    if already_has_trailing_completed_footer {
        return;
    }
    child_view
        .scrollback
        .push_block(crate::scrollback::block::RenderBlock::session_event(
            crate::scrollback::blocks::SessionEvent::TurnCompleted {
                elapsed: Some(elapsed),
            },
        ));
}
fn join_meta_parts(parts: &[Option<&str>]) -> String {
    let non_empty: Vec<&str> = parts.iter().copied().flatten().collect();
    if non_empty.is_empty() {
        String::new()
    } else {
        non_empty.join(" \u{00b7} ")
    }
}
/// Collapse `(persona, role)` to a single label when both refer to the same
/// title. Comparison is case-insensitive after trimming surrounding whitespace.
///
/// Behavior:
/// - Either side that is `Some(s)` where `s.trim().is_empty()` is treated as
///   `None` first, so a stray empty/whitespace-only string never sneaks into
///   the joined output as a leading separator.
/// - Both present and titles match -> returns `(Some(persona), None)` so
///   callers render only the persona once.
/// - Both present and titles differ -> returns the inputs unchanged.
/// - Either or both absent -> returns the inputs unchanged.
///
/// ASCII-only comparison is intentional: persona/role identifiers in this
/// codebase are ASCII slugs (lowercase names from the bundle registry).
/// `eq_ignore_ascii_case` is allocation-free; switching to Unicode case
/// folding would allocate per render and is not needed here.
///
/// Lifetimes on `persona` and `role` are independent (`'a`, `'b`) so the two
/// inputs do not need to share a borrow scope.
///
/// This is the single source of truth for the role/persona dedup in subagent
/// metadata strings; the scrollback `(persona · role · model)` parenthetical
/// funnels through it via [`format_subagent_meta`].
fn dedup_persona_role<'a, 'b>(
    persona: Option<&'a str>,
    role: Option<&'b str>,
) -> (Option<&'a str>, Option<&'b str>) {
    let persona = persona.filter(|s| !s.trim().is_empty());
    let role = role.filter(|s| !s.trim().is_empty());
    match (persona, role) {
        (Some(p), Some(r)) if p.trim().eq_ignore_ascii_case(r.trim()) => (Some(p), None),
        _ => (persona, role),
    }
}
pub(crate) fn format_type_label(subagent_type: &str) -> &str {
    match subagent_type {
        "general-purpose" => "general",
        other => other,
    }
}
pub(crate) fn format_context_badge(info: &SubagentInfo) -> &str {
    match info.context_source.as_deref() {
        Some("resumed") => "resumed",
        Some("forked") => "forked",
        _ => "",
    }
}
/// Parse a leading `[tag]` prefix from a description.
///
/// Returns `(Some(tag), rest_after_close_bracket)` if the description begins
/// with `[<non-empty>]`, otherwise `(None, description)` unchanged.
pub(crate) fn parse_tag_prefix(description: &str) -> (Option<&str>, &str) {
    if let Some(rest) = description.strip_prefix('[')
        && let Some(close) = rest.find(']')
    {
        let tag = rest[..close].trim();
        if !tag.is_empty() {
            return (Some(tag), rest[close + 1..].trim_start());
        }
    }
    (None, description)
}
/// Single consolidated label + display description for a subagent row.
///
/// Precedence for the label (highest first):
/// 1. `persona` — semantic, parent-supplied at spawn time.
/// 2. `role`    — config-defined preset.
/// 3. `subagent_type` (only when **not** `general-purpose`) — `explore`,
///    `plan`, or any custom type carries real signal.
/// 4. `[tag]` parsed from the description — fallback when nothing above
///    identifies the agent and `subagent_type` is the meaningless default.
/// 5. `"general"` — final fallback when `subagent_type == "general-purpose"`
///    and no persona / role / tag is present.
///
/// The returned label has its first character capitalized for display
/// (e.g. `explore` → `Explore`, `implementer` → `Implementer`). Personas,
/// roles, and tags are conventionally lowercase ASCII slugs, so callers
/// expect the rendering to do the title-casing.
///
/// The returned description always has any leading `[tag]` prefix stripped,
/// regardless of whether the tag was used as the label, so callers never
/// render `[tag]` bracket noise inline.
pub(crate) fn format_subagent_label(info: &SubagentInfo) -> (String, String) {
    let (tag, clean_desc) = parse_tag_prefix(&info.description);
    let raw_label = if let Some(p) = info
        .persona
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        p.to_string()
    } else if let Some(r) = info
        .role
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        r.to_string()
    } else if info.subagent_type.as_ref() != "general-purpose" {
        format_type_label(&info.subagent_type).to_string()
    } else if let Some(tag) = tag {
        tag.to_string()
    } else {
        "general".to_string()
    };
    let mut chars = raw_label.chars();
    let label = match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => raw_label,
    };
    (label, clean_desc.to_string())
}
pub(crate) fn format_subagent_meta(
    persona: Option<&str>,
    role: Option<&str>,
    model: Option<&str>,
) -> String {
    let (persona, role) = dedup_persona_role(persona, role);
    let bare = join_meta_parts(&[persona, role, model]);
    if bare.is_empty() {
        bare
    } else {
        format!(" ({bare})")
    }
}
/// Format a [`TurnActivity`] into a concise display label.
///
/// Used in the subagent scrollback block and the fullscreen title bar.
/// Callers handle the `None` activity / "Waiting" case separately.
pub(crate) fn format_activity_label(activity: &crate::acp::tracker::TurnActivity) -> String {
    use crate::acp::tracker::TurnActivity;
    match activity {
        TurnActivity::Thinking => "Thinking".to_string(),
        TurnActivity::Responding => "Responding".to_string(),
        TurnActivity::ToolRunning { title, description } => {
            if let Some(desc) = description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                crate::acp::tracker::format_waiting_for_subject(desc)
            } else if title.is_empty() {
                "Running tool".to_string()
            } else {
                let first_line = title.lines().next().unwrap_or(title);
                let max_len = crate::acp::tracker::MAX_ACTIVITY_SUBJECT_CHARS;
                if first_line.len() <= max_len {
                    format!("Running: {first_line}")
                } else {
                    let char_count = first_line.chars().count();
                    if char_count <= max_len {
                        format!("Running: {first_line}")
                    } else {
                        let truncated: String = first_line.chars().take(max_len).collect();
                        format!("Running: {truncated}\u{2026}")
                    }
                }
            }
        }
        TurnActivity::AutoCompacting => "Compacting".to_string(),
        TurnActivity::Retrying {
            attempt,
            max_retries,
            ..
        } => {
            format!("Retrying ({attempt}/{max_retries})")
        }
        TurnActivity::WritingToolCall(writing) => writing.label(),
        TurnActivity::Waiting(reason) => reason.label(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::meta::NotificationMeta;
    use crate::acp::model_state::ModelState;
    use crate::acp::tracker::AcpUpdateTracker;
    use crate::app::agent::{AgentId, AgentSession, AgentState};
    use crate::app::agent_view::AgentView;
    use crate::scrollback::block::RenderBlock;
    use crate::scrollback::state::ScrollbackState;
    use agent_client_protocol as acp;
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;
    fn make_info() -> SubagentInfo {
        SubagentInfo {
            subagent_id: "sa-1".into(),
            child_session_id: "cs-1".into(),
            description: "test task".into(),
            subagent_type: "explore".into(),
            persona: None,
            role: None,
            model: None,
            context_source: None,
            resumed_from: None,
            capability_mode: None,
            workflow_run_id: None,
            budget: None,
            context_normalized: false,
            parent_prompt_id: None,
            started_at: Instant::now(),
            last_progress_at: Instant::now(),
            finished: false,
            status: None,
            error: None,
            termination_reason: None,
            usage: None,
            duration_ms: None,
            tool_calls: None,
            turns: None,
            turn_count: None,
            tool_call_count: None,
            tokens_used: None,
            context_window_tokens: None,
            context_usage_pct: None,
            tools_used: Vec::new(),
            error_count: None,
            activity_label: None,
            is_background: false,
            pending_kill: false,
            kill_requested_at: None,
            scrollback_entry_id: None,
            prompt: None,
            child_cwd: None,
            worktree_path: None,
            transcript: Default::default(),
        }
    }
    fn make_min_child_view() -> AgentView {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let session = AgentSession {
            id: AgentId(0),
            acp_tx: tx,
            session_id: Some(acp::SessionId::new(Arc::from("child"))),
            models: ModelState::default(),
            state: AgentState::Idle,
            tracker: AcpUpdateTracker::new(),
            cwd: PathBuf::from("/tmp"),
            is_worktree: false,
            forked_from: None,
            pending_prompts: VecDeque::new(),
            next_queue_id: 0,
            yolo_mode: false,
            auto_mode: false,
            prompt_history: Vec::new(),
            prompt_history_loading: false,
            loading_replay: false,
            restore_degree: None,
            rate_limited: false,
            model_incompatible: false,
            credit_limit_blocked: false,
            free_usage_blocked: false,
            available_commands: Vec::new(),
            available_commands_generation: 0,
            available_tools: None,
            model_switch_pending: false,
            user_model_preference: None,
            deferred_model_switch: None,
            bg_tasks: BTreeMap::new(),
            bg_tool_call_to_task: HashMap::new(),
            scheduled_tasks: HashMap::new(),
            in_flight_prompt: None,
            compact_held_prompt: None,
            current_prompt_id: None,
            created_via_new: false,
        };
        AgentView::new(session, ScrollbackState::new())
    }
    fn seed_tool_call(view: &mut AgentView) {
        view.session.tracker.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new(Arc::from("tc1")), "Read foo")
                    .kind(acp::ToolKind::Other)
                    .status(acp::ToolCallStatus::Pending)
                    .content(vec![])
                    .locations(vec![]),
            ),
            &NotificationMeta::default(),
            &mut view.scrollback,
        );
    }
    #[test]
    fn child_scrollback_already_shows_prompt_matches_user_prompt() {
        let mut view = make_min_child_view();
        view.scrollback
            .push_block(RenderBlock::user_prompt("  scan src/  \n"));
        assert!(child_scrollback_already_shows_prompt(
            &view.scrollback,
            "scan src/"
        ));
    }
    #[test]
    fn child_scrollback_already_shows_prompt_false_when_absent() {
        let view = make_min_child_view();
        assert!(!child_scrollback_already_shows_prompt(
            &view.scrollback,
            "scan src/"
        ));
    }
    #[test]
    fn child_scrollback_already_shows_prompt_false_for_empty_needle() {
        let mut view = make_min_child_view();
        view.scrollback
            .push_block(RenderBlock::user_prompt("anything"));
        assert!(!child_scrollback_already_shows_prompt(&view.scrollback, ""));
        assert!(!child_scrollback_already_shows_prompt(
            &view.scrollback,
            "   "
        ));
    }
    #[test]
    fn prompt_only_true_for_empty_scrollback() {
        let view = make_min_child_view();
        assert!(scrollback_is_prompt_only(&view.scrollback));
    }
    #[test]
    fn prompt_only_true_for_injected_prompt() {
        let mut view = make_min_child_view();
        view.scrollback
            .push_block(RenderBlock::user_prompt("scan src/"));
        assert!(scrollback_is_prompt_only(&view.scrollback));
    }
    #[test]
    fn prompt_only_false_when_tool_call_present() {
        let mut view = make_min_child_view();
        seed_tool_call(&mut view);
        assert!(!scrollback_is_prompt_only(&view.scrollback));
    }
    #[test]
    fn prompt_only_false_when_prompt_and_tool_call() {
        let mut view = make_min_child_view();
        view.scrollback
            .push_block(RenderBlock::user_prompt("scan src/"));
        seed_tool_call(&mut view);
        assert!(!scrollback_is_prompt_only(&view.scrollback));
    }
    #[test]
    fn ensure_subagent_child_replayed_skips_when_transcript_settled() {
        let mut parent = make_min_child_view();
        let child_sid = "child-skip";
        let mut child = make_min_child_view();
        child
            .scrollback
            .push_block(RenderBlock::user_prompt("task only"));
        parent
            .subagent_views
            .insert(child_sid.to_string(), Box::new(child));
        let mut info = make_info();
        info.child_session_id = child_sid.into();
        info.transcript = ChildTranscript::DiskBacked;
        parent.subagent_sessions.insert(child_sid.to_string(), info);
        ensure_subagent_child_replayed(&mut parent, child_sid);
        let child = parent.subagent_views.get(child_sid).unwrap();
        assert_eq!(child.scrollback.len(), 1);
        assert!(matches!(
            child.scrollback.entry(0).unwrap().block,
            RenderBlock::UserPrompt(_)
        ));
    }
    /// The child-transcript replay purges exactly once when it actually
    /// parsed an `updates.jsonl` transient — and never when the load no-ops
    /// (missing file) or the open takes the already-replayed skip path. The
    /// purge lives inside `replay_inherited_updates` so BOTH producers (the
    /// eager live-spawn path and this deferred first-open path) are covered.
    #[test]
    fn ensure_subagent_child_replayed_releases_retained_memory_once() {
        use crate::memory_release::test_support;
        test_support::install_counting_hook();
        let child_sid = "child-purge-real";
        let home = tempfile::tempdir().unwrap();
        let session_dir = home
            .path()
            .join("sessions")
            .join(urlencoding::encode("/tmp").as_ref())
            .join(child_sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        let tool_line = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"Read foo","kind":"read","locations":[{{"path":"/tmp/foo"}}]}}}}}}"#
        );
        std::fs::write(session_dir.join("updates.jsonl"), tool_line + "\n").unwrap();
        set_replay_grok_home_for_tests(Some(home.path().to_path_buf()));
        let mut parent = make_min_child_view();
        parent
            .subagent_views
            .insert(child_sid.to_string(), Box::new(make_min_child_view()));
        let mut info = make_info();
        info.child_session_id = child_sid.into();
        parent.subagent_sessions.insert(child_sid.to_string(), info);
        let before = test_support::calls();
        ensure_subagent_child_replayed(&mut parent, child_sid);
        assert_eq!(
            test_support::calls(),
            before + 1,
            "a real replay must purge after the parsed transient drops"
        );
        assert!(
            !parent.subagent_sessions[child_sid]
                .transcript
                .needs_replay(),
            "fixture sanity: the emitting replay must settle the transcript"
        );
        let before = test_support::calls();
        ensure_subagent_child_replayed(&mut parent, child_sid);
        assert_eq!(
            test_support::calls(),
            before,
            "the skip path allocates nothing and must not purge"
        );
        let ghost_sid = "child-purge-ghost";
        parent
            .subagent_views
            .insert(ghost_sid.to_string(), Box::new(make_min_child_view()));
        let mut ghost = make_info();
        ghost.child_session_id = ghost_sid.into();
        parent
            .subagent_sessions
            .insert(ghost_sid.to_string(), ghost);
        let before = test_support::calls();
        ensure_subagent_child_replayed(&mut parent, ghost_sid);
        assert_eq!(
            test_support::calls(),
            before,
            "a no-op replay (missing transcript) must not purge"
        );
        assert!(
            parent.subagent_sessions[ghost_sid]
                .transcript
                .needs_replay(),
            "nothing on disk proves nothing: the next open retries"
        );
        let empty_sid = "child-purge-empty";
        let empty_dir = home
            .path()
            .join("sessions")
            .join(urlencoding::encode("/tmp").as_ref())
            .join(empty_sid);
        std::fs::create_dir_all(&empty_dir).unwrap();
        std::fs::write(empty_dir.join("summary.json"), "{}").unwrap();
        std::fs::write(empty_dir.join("updates.jsonl"), "").unwrap();
        parent
            .subagent_views
            .insert(empty_sid.to_string(), Box::new(make_min_child_view()));
        let mut empty = make_info();
        empty.child_session_id = empty_sid.into();
        parent
            .subagent_sessions
            .insert(empty_sid.to_string(), empty);
        let before = test_support::calls();
        ensure_subagent_child_replayed(&mut parent, empty_sid);
        assert_eq!(
            test_support::calls(),
            before,
            "an empty replay (zero updates parsed) must not purge"
        );
        assert!(
            parent.subagent_sessions[empty_sid]
                .transcript
                .needs_replay()
        );
        set_replay_grok_home_for_tests(None);
    }
    /// A rebuilt transcript must show the run's original timestamps: the
    /// replay threads each persisted line's `_meta` through the tracker, so
    /// `created_at` comes from `agentTimestampMs`/`turnStartMs` (as on live
    /// arrival and main-session resume), not the rebuild wall clock. Covers
    /// both the injected task prompt (retro-stamped by the echo dedup) and a
    /// replayed agent message.
    #[test]
    fn rebuilt_child_transcript_keeps_persisted_timestamps() {
        use chrono::TimeZone;
        let child_sid = "child-timestamps";
        let prompt_ms: i64 = 1_700_000_000_000;
        let msg_ms: i64 = 1_700_000_060_000;
        let home = tempfile::tempdir().unwrap();
        let session_dir = home
            .path()
            .join("sessions")
            .join(urlencoding::encode("/tmp").as_ref())
            .join(child_sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        let echo = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"user_message_chunk","content":{{"type":"text","text":"scan src/"}}}},"_meta":{{"agentTimestampMs":{prompt_ms},"turnStartMs":{prompt_ms}}}}}}}"#
        );
        let msg = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"done"}}}},"_meta":{{"agentTimestampMs":{msg_ms}}}}}}}"#
        );
        std::fs::write(
            session_dir.join("updates.jsonl"),
            format!("{echo}\n{msg}\n"),
        )
        .unwrap();
        set_replay_grok_home_for_tests(Some(home.path().to_path_buf()));
        let mut parent = make_min_child_view();
        parent
            .subagent_views
            .insert(child_sid.to_string(), Box::new(make_min_child_view()));
        let mut info = make_info();
        info.child_session_id = child_sid.into();
        info.prompt = Some("scan src/".into());
        info.finished = true;
        info.duration_ms = Some(1_000);
        parent.subagent_sessions.insert(child_sid.to_string(), info);
        ensure_subagent_child_replayed(&mut parent, child_sid);
        let expected = |ms: i64| {
            chrono::Utc
                .timestamp_millis_opt(ms)
                .single()
                .unwrap()
                .with_timezone(&chrono::Local)
        };
        let child = parent.subagent_views.get(child_sid).unwrap();
        let mut prompt_seen = false;
        let mut msg_seen = false;
        for i in 0..child.scrollback.len() {
            let entry = child.scrollback.entry(i).unwrap();
            match &entry.block {
                RenderBlock::UserPrompt(_) => {
                    assert_eq!(
                        entry.created_at,
                        Some(expected(prompt_ms)),
                        "injected task prompt must carry the persisted turn start, not the rebuild time"
                    );
                    prompt_seen = true;
                }
                RenderBlock::AgentMessage(_) => {
                    assert_eq!(
                        entry.created_at,
                        Some(expected(msg_ms)),
                        "replayed agent message must carry the persisted timestamp, not the rebuild time"
                    );
                    msg_seen = true;
                }
                _ => {}
            }
        }
        assert!(prompt_seen, "fixture must produce a user prompt entry");
        assert!(msg_seen, "fixture must produce an agent message entry");
        set_replay_grok_home_for_tests(None);
    }
    #[test]
    fn replay_inherited_updates_batches_and_collapses_tools() {
        let home = tempfile::tempdir().unwrap();
        let child_sid = "child-batch";
        let session_dir = home
            .path()
            .join("sessions")
            .join(urlencoding::encode("/tmp").as_ref())
            .join(child_sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        let user = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"user_message_chunk","content":{{"type":"text","text":"go"}}}}}}}}"#
        );
        let tool = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"tool_call","toolCallId":"t1","title":"bash","kind":"execute","status":"pending"}}}}}}"#
        );
        let ip = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"in_progress","content":[{{"type":"text","text":"out"}}]}}}}}}"#
        );
        let done = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{{"type":"text","text":"out"}}]}}}}}}"#
        );
        let agent_msg = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"ok"}}}}}}}}"#
        );
        std::fs::write(
            session_dir.join("updates.jsonl"),
            format!("{user}\n{tool}\n{ip}\n{done}\n{agent_msg}\n"),
        )
        .unwrap();
        set_replay_grok_home_for_tests(Some(home.path().to_path_buf()));
        let mut view = make_min_child_view();
        assert!(matches!(
            replay_inherited_updates(
                &mut view,
                child_sid,
                std::path::Path::new("/tmp"),
                None,
                ReplayLookupFallback::Relocation,
            ),
            Ok(ReplayEmission::Emitted)
        ));
        assert!(
            !view.scrollback.in_batch(),
            "end_batch must run after streamed apply"
        );
        assert_eq!(
            view.scrollback.turn_count(),
            1,
            "end_batch must rebuild turns once after the stream"
        );
        let tools = (0..view.scrollback.len())
            .filter(|i| {
                view.scrollback
                    .entry(*i)
                    .is_some_and(|e| matches!(e.block, RenderBlock::ToolCall(_)))
            })
            .count();
        assert_eq!(tools, 1, "ToolCall+updates must collapse to one block");
        set_replay_grok_home_for_tests(None);
    }
    #[test]
    fn replay_inherited_updates_ends_batch_on_read_error() {
        let home = tempfile::tempdir().unwrap();
        let child_sid = "child-read-err";
        let session_dir = home
            .path()
            .join("sessions")
            .join(urlencoding::encode("/tmp").as_ref())
            .join(child_sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        std::fs::create_dir(session_dir.join("updates.jsonl")).unwrap();
        set_replay_grok_home_for_tests(Some(home.path().to_path_buf()));
        let mut view = make_min_child_view();
        assert!(
            replay_inherited_updates(
                &mut view,
                child_sid,
                std::path::Path::new("/tmp"),
                None,
                ReplayLookupFallback::Relocation,
            )
            .is_err()
        );
        assert!(
            !view.scrollback.in_batch(),
            "end_batch must run after a read error"
        );
        set_replay_grok_home_for_tests(None);
    }
    #[test]
    fn replay_inherited_updates_uses_child_cwd_hint() {
        let home = tempfile::tempdir().unwrap();
        let child_sid = "child-wt-hint";
        let child_cwd = "/work/wt";
        let session_dir = home
            .path()
            .join("sessions")
            .join(xai_grok_config::encode_cwd_dirname(child_cwd))
            .join(child_sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        let user = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"user_message_chunk","content":{{"type":"text","text":"from-wt"}}}}}}}}"#
        );
        std::fs::write(session_dir.join("updates.jsonl"), format!("{user}\n")).unwrap();
        set_replay_grok_home_for_tests(Some(home.path().to_path_buf()));
        let mut view = make_min_child_view();
        assert!(matches!(
            replay_inherited_updates(
                &mut view,
                child_sid,
                std::path::Path::new("/tmp"),
                Some(std::path::Path::new(child_cwd)),
                ReplayLookupFallback::Relocation,
            ),
            Ok(ReplayEmission::Emitted)
        ));
        assert_ne!(
            view.scrollback.len(),
            0,
            "child_cwd hint must locate the worktree transcript"
        );
        set_replay_grok_home_for_tests(None);
    }
    #[test]
    fn subagent_meta_empty() {
        assert_eq!(format_subagent_meta(None, None, None), "");
    }
    #[test]
    fn subagent_meta_all_fields() {
        assert_eq!(
            format_subagent_meta(Some("researcher"), Some("analyst"), Some("grok-3")),
            " (researcher \u{00b7} analyst \u{00b7} grok-3)"
        );
    }
    #[test]
    fn subagent_meta_partial_skips_nones() {
        assert_eq!(
            format_subagent_meta(Some("researcher"), None, Some("grok-3")),
            " (researcher \u{00b7} grok-3)"
        );
    }
    #[test]
    fn type_label_abbreviates_general_purpose() {
        assert_eq!(format_type_label("general-purpose"), "general");
    }
    #[test]
    fn type_label_passes_through_known_types() {
        assert_eq!(format_type_label("explore"), "explore");
        assert_eq!(format_type_label("plan"), "plan");
    }
    #[test]
    fn type_label_passes_through_unknown() {
        assert_eq!(format_type_label("custom-agent"), "custom-agent");
    }
    #[test]
    fn context_badge_resumed() {
        let mut info = make_info();
        info.context_source = Some("resumed".into());
        assert_eq!(format_context_badge(&info), "resumed");
    }
    #[test]
    fn context_badge_forked() {
        let mut info = make_info();
        info.context_source = Some("forked".into());
        assert_eq!(format_context_badge(&info), "forked");
    }
    #[test]
    fn context_badge_new_returns_empty() {
        let mut info = make_info();
        info.context_source = Some("new".into());
        assert_eq!(format_context_badge(&info), "");
    }
    #[test]
    fn context_badge_none_returns_empty() {
        assert_eq!(format_context_badge(&make_info()), "");
    }
    #[test]
    fn subagent_meta_collapses_duplicate_persona_role() {
        assert_eq!(
            format_subagent_meta(Some("reviewer"), Some("reviewer"), Some("grok-3")),
            " (reviewer \u{00b7} grok-3)"
        );
    }
    #[test]
    fn subagent_meta_keeps_distinct_persona_role() {
        assert_eq!(
            format_subagent_meta(Some("researcher"), Some("analyst"), None),
            " (researcher \u{00b7} analyst)"
        );
    }
    #[test]
    fn subagent_meta_only_role_when_persona_absent() {
        assert_eq!(
            format_subagent_meta(None, Some("reviewer"), None),
            " (reviewer)"
        );
    }
    #[test]
    fn subagent_meta_only_persona_when_role_absent() {
        assert_eq!(
            format_subagent_meta(Some("reviewer"), None, None),
            " (reviewer)"
        );
    }
    #[test]
    fn subagent_meta_drops_both_empty_persona_role() {
        assert_eq!(
            format_subagent_meta(Some(""), Some(" "), Some("grok-3")),
            " (grok-3)"
        );
    }
    #[test]
    fn label_uses_persona_when_set() {
        let mut info = make_info();
        info.persona = Some("implementer".into());
        info.role = Some("any".into());
        info.subagent_type = "general-purpose".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "Implementer");
        assert_eq!(desc, "test task");
    }
    #[test]
    fn label_falls_back_to_role_when_no_persona() {
        let mut info = make_info();
        info.role = Some("analyst".into());
        info.subagent_type = "general-purpose".into();
        let (label, _) = format_subagent_label(&info);
        assert_eq!(label, "Analyst");
    }
    #[test]
    fn label_uses_subagent_type_when_meaningful() {
        let mut info = make_info();
        info.subagent_type = "explore".into();
        info.description = "[deep-dive] find auth code".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "Explore");
        assert_eq!(desc, "find auth code");
    }
    #[test]
    fn label_falls_back_to_tag_when_general_purpose() {
        let mut info = make_info();
        info.subagent_type = "general-purpose".into();
        info.description = "[security-fix] patch XSS".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "Security-fix");
        assert_eq!(desc, "patch XSS");
    }
    #[test]
    fn label_final_fallback_general() {
        let mut info = make_info();
        info.subagent_type = "general-purpose".into();
        info.description = "do a thing".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "General");
        assert_eq!(desc, "do a thing");
    }
    #[test]
    fn label_strips_tag_prefix_even_when_unused() {
        let mut info = make_info();
        info.persona = Some("reviewer".into());
        info.subagent_type = "general-purpose".into();
        info.description = "[review] check the diff".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "Reviewer");
        assert_eq!(desc, "check the diff");
    }
    #[test]
    fn label_treats_whitespace_persona_as_absent() {
        let mut info = make_info();
        info.persona = Some("   ".into());
        info.role = Some("analyst".into());
        info.subagent_type = "general-purpose".into();
        let (label, _) = format_subagent_label(&info);
        assert_eq!(label, "Analyst");
    }
    #[test]
    fn label_treats_empty_tag_as_absent() {
        let mut info = make_info();
        info.subagent_type = "general-purpose".into();
        info.description = "[] do something".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "General");
        assert_eq!(desc, "[] do something");
    }
    #[test]
    fn label_unclosed_bracket_leaves_description_alone() {
        let mut info = make_info();
        info.subagent_type = "general-purpose".into();
        info.description = "[broken description".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "General");
        assert_eq!(desc, "[broken description");
    }
    #[test]
    fn label_custom_subagent_type_passes_through_with_capitalization() {
        let mut info = make_info();
        info.subagent_type = "custom-agent".into();
        let (label, _) = format_subagent_label(&info);
        assert_eq!(label, "Custom-agent");
    }
    #[test]
    fn label_preserves_already_capitalized_persona() {
        let mut info = make_info();
        info.persona = Some("Reviewer".into());
        let (label, _) = format_subagent_label(&info);
        assert_eq!(label, "Reviewer");
    }
    fn write_meta_json(dir: &std::path::Path, subagent_id: &str, json: &str) {
        let meta_dir = dir.join("subagents").join(subagent_id);
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::write(meta_dir.join("meta.json"), json).unwrap();
    }
    /// Build a session dir matching the path formula used by `enrich_from_meta_with_home`.
    fn setup_enrichment_dir(
        grok_home: &std::path::Path,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> std::path::PathBuf {
        let sessions_dir = grok_home
            .join("sessions")
            .join(urlencoding::encode(&cwd.to_string_lossy()).as_ref())
            .join(session_id);
        std::fs::create_dir_all(&sessions_dir).unwrap();
        sessions_dir
    }
    #[test]
    fn enrich_from_meta_populates_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = std::path::Path::new("/home/user/project");
        let session_id = "sess-abc";
        let session_dir = setup_enrichment_dir(tmp.path(), cwd, session_id);
        let json = r#"{"prompt":"do stuff","child_cwd":"/tmp/work","worktree_path":"/tmp/wt"}"#;
        write_meta_json(&session_dir, "sa-1", json);
        let mut info = make_info();
        enrich_from_meta_with_home(&mut info, tmp.path(), cwd, session_id);
        assert_eq!(info.prompt.as_deref(), Some("do stuff"));
        assert_eq!(info.child_cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(info.worktree_path.as_deref(), Some("/tmp/wt"));
    }
    #[test]
    fn enrich_from_meta_missing_file_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut info = make_info();
        enrich_from_meta_with_home(
            &mut info,
            tmp.path(),
            std::path::Path::new("/nowhere"),
            "no-session",
        );
        assert!(info.prompt.is_none());
        assert!(info.child_cwd.is_none());
        assert!(info.worktree_path.is_none());
    }
    #[test]
    fn enrich_from_meta_malformed_json_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = std::path::Path::new("/home/user");
        let session_dir = setup_enrichment_dir(tmp.path(), cwd, "sess-x");
        write_meta_json(&session_dir, "sa-1", "not json{{{");
        let mut info = make_info();
        enrich_from_meta_with_home(&mut info, tmp.path(), cwd, "sess-x");
        assert!(info.prompt.is_none());
    }
    #[test]
    fn deserialize_meta_slice_ignores_unknown_fields() {
        let json = r#"{"prompt":"hi","unknown_field":42,"nested":{"a":1}}"#;
        let meta: SubagentMetaSlice = serde_json::from_str(json).unwrap();
        assert_eq!(meta.prompt.as_deref(), Some("hi"));
        assert!(meta.child_cwd.is_none());
        assert!(meta.worktree_path.is_none());
    }
    #[test]
    fn enrich_from_meta_partial_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = std::path::Path::new("/home/user");
        let session_dir = setup_enrichment_dir(tmp.path(), cwd, "sess-p");
        write_meta_json(&session_dir, "sa-1", r#"{"prompt":"only prompt"}"#);
        let mut info = make_info();
        enrich_from_meta_with_home(&mut info, tmp.path(), cwd, "sess-p");
        assert_eq!(info.prompt.as_deref(), Some("only prompt"));
        assert!(info.child_cwd.is_none());
        assert!(info.worktree_path.is_none());
    }
    #[test]
    fn activity_label_thinking() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(format_activity_label(&TurnActivity::Thinking), "Thinking");
    }
    #[test]
    fn activity_label_responding() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::Responding),
            "Responding",
        );
    }
    #[test]
    fn activity_label_auto_compacting() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::AutoCompacting),
            "Compacting",
        );
    }
    #[test]
    fn activity_label_retrying() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::Retrying {
                attempt: 2,
                max_retries: 5,
                reason: "rate limited".into(),
            }),
            "Retrying (2/5)",
        );
    }
    #[test]
    fn activity_label_waiting_reasons() {
        use crate::acp::tracker::{TurnActivity, WaitingReason};
        assert_eq!(
            format_activity_label(&TurnActivity::Waiting(WaitingReason::subagent())),
            "Waiting on subagent…",
        );
        assert_eq!(
            format_activity_label(&TurnActivity::Waiting(WaitingReason::task_output())),
            "Waiting on task output…",
        );
        assert_eq!(
            format_activity_label(&TurnActivity::Waiting(WaitingReason::TaskOutput {
                task_ids: vec!["t1".into()],
                subject: Some("run tests".into()),
                waits: false,
            })),
            "run tests…",
        );
    }
    #[test]
    fn activity_label_tool_running_empty_title() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::ToolRunning {
                title: String::new(),
                description: None
            }),
            "Running tool",
        );
    }
    #[test]
    fn activity_label_tool_running_short_title() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::ToolRunning {
                title: "cargo build".into(),
                description: None
            }),
            "Running: cargo build",
        );
    }
    #[test]
    fn activity_label_tool_running_exactly_at_limit() {
        use crate::acp::tracker::TurnActivity;
        let title = "a".repeat(40);
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title: title.clone(),
            description: None,
        });
        assert_eq!(result, format!("Running: {title}"));
        assert!(!result.contains('\u{2026}'), "no ellipsis at boundary");
    }
    #[test]
    fn activity_label_tool_running_truncates_long_title() {
        use crate::acp::tracker::TurnActivity;
        let title = "a".repeat(60);
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title,
            description: None,
        });
        let expected_prefix = "Running: ".to_string() + "a".repeat(40).as_str();
        assert!(result.starts_with(&expected_prefix));
        assert!(result.ends_with('\u{2026}'), "truncated with ellipsis");
    }
    #[test]
    fn activity_label_tool_running_multibyte_under_char_limit() {
        use crate::acp::tracker::TurnActivity;
        let title: String = "\u{00e9}".repeat(35);
        assert!(title.len() > 40, "byte length exceeds threshold");
        assert!(title.chars().count() <= 40, "char count within limit");
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title: title.clone(),
            description: None,
        });
        assert_eq!(result, format!("Running: {title}"));
        assert!(!result.contains('\u{2026}'), "no spurious ellipsis");
    }
    #[test]
    fn activity_label_tool_running_multibyte_over_char_limit() {
        use crate::acp::tracker::TurnActivity;
        let title: String = "\u{00e9}".repeat(45);
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title,
            description: None,
        });
        assert!(result.ends_with('\u{2026}'), "truncated with ellipsis");
        let after_prefix = result.strip_prefix("Running: ").unwrap();
        let content_chars: Vec<char> = after_prefix.chars().collect();
        assert_eq!(content_chars.len(), 41);
    }
    #[test]
    fn activity_label_tool_running_multiline_uses_first_line() {
        use crate::acp::tracker::TurnActivity;
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title: "first line\nsecond line".into(),
            description: None,
        });
        assert_eq!(result, "Running: first line");
    }
}
