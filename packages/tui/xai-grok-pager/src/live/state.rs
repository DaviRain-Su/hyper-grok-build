//! Live session state — the pager-owned lifecycle for `/live`.
//!
//! Bound to `(generation, AgentId, SessionId)`. One session at a time.
//! Stops idempotently on session/view switch, close/exit/quit, ACP disconnect,
//! and teardown. Mutually exclusive with `/voice` dictation.

use std::sync::atomic::{AtomicBool, Ordering};

use super::LiveCommand;
use super::LiveEvent;
use super::LivePhase;
use super::TranscriptKind;

/// Process-global Live gate for view code without an `AppView`.
pub(crate) static LIVE_ENABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static LIVE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn live_enabled() -> bool {
    LIVE_ENABLED.load(Ordering::Acquire)
}

#[allow(dead_code)]
pub(crate) fn live_active() -> bool {
    LIVE_ACTIVE.load(Ordering::Acquire)
}

/// Update the process-global availability snapshot used by slash surfaces.
pub(crate) fn set_live_enabled(on: bool) {
    LIVE_ENABLED.store(on, Ordering::Release);
}

/// Update the process-global active snapshot used by view-only surfaces.
pub(crate) fn set_live_active(on: bool) {
    LIVE_ACTIVE.store(on, Ordering::Release);
}

/// The AgentId type re-exported from the pager's app module.
pub type AgentId = crate::app::agent::AgentId;

/// A generation counter (monotonic per process) for Live session binding.
pub type Generation = u64;

/// The dictation draft snapshot — preserved across Live start/stop so the
/// user's composer text and cursor are never lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftSnapshot {
    pub text: String,
    pub cursor: usize,
}

/// A delegation lifecycle entry registered when a `LiveEvent::Delegation`
/// submits text to the bound AgentSession.
#[derive(Debug, Clone)]
pub struct DelegationEntry {
    /// The generation this delegation was submitted in.
    pub generation: Generation,
    /// The delegation id (from `LiveEvent::Delegation`).
    pub delegation_id: String,
    /// The AgentId of the bound session at submission time.
    pub agent_id: AgentId,
    /// The SessionId of the bound session at submission time.
    pub session_id: String,
    /// The prompt_id returned by the prompt pipeline for the submitted text.
    pub prompt_id: String,
    /// Whether this delegation has reached a terminal state (turn completed,
    /// prompt error, or cancel/failure).
    pub terminal: bool,
}

/// The Live session lifecycle state.
///
/// One state at a time, so inconsistent combinations are unrepresentable.
/// Production mutates it only through the `AppView::live_*` transition methods
/// (see `app_view.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LiveState {
    /// No Live session in flight.
    #[default]
    Idle,
    /// A start was requested but the bound session is not yet established
    /// (no ACP session id); the start is deferred until `CreateSession`
    /// completes. The draft+cursor are preserved here.
    PendingUnbound {
        agent_id: AgentId,
        draft: DraftSnapshot,
    },
    /// The Live pipeline is spawning; the session is bound.
    ColdStart {
        agent_id: AgentId,
        session_id: String,
        generation: Generation,
        draft: DraftSnapshot,
    },
    /// The Live session is active (pipeline up, visualizer shown).
    Active {
        agent_id: AgentId,
        session_id: String,
        generation: Generation,
        draft: DraftSnapshot,
    },
    /// The Live session is stopping (shutdown sent, awaiting `Closed`).
    Stopping {
        agent_id: AgentId,
        session_id: String,
        generation: Generation,
        draft: DraftSnapshot,
    },
}

impl LiveState {
    /// Whether a Live session is active (the `Active` state).
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    /// Whether a Live start is pending (ColdStart or PendingUnbound).
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::ColdStart { .. } | Self::PendingUnbound { .. })
    }

    /// Whether a Live session is in flight (active, pending, or stopping).
    pub fn is_in_flight(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// The AgentId the Live session is bound to, if any.
    pub fn agent_id(&self) -> Option<AgentId> {
        match self {
            Self::Active { agent_id, .. }
            | Self::ColdStart { agent_id, .. }
            | Self::Stopping { agent_id, .. }
            | Self::PendingUnbound { agent_id, .. } => Some(*agent_id),
            Self::Idle => None,
        }
    }

    /// The SessionId the Live session is bound to, if any.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Active { session_id, .. }
            | Self::ColdStart { session_id, .. }
            | Self::Stopping { session_id, .. } => Some(session_id.as_str()),
            Self::PendingUnbound { .. } | Self::Idle => None,
        }
    }

    /// The generation the Live session is bound to, if any.
    pub fn generation(&self) -> Option<Generation> {
        match self {
            Self::Active { generation, .. }
            | Self::ColdStart { generation, .. }
            | Self::Stopping { generation, .. } => Some(*generation),
            Self::PendingUnbound { .. } | Self::Idle => None,
        }
    }

    /// The preserved draft snapshot, if any.
    pub fn draft(&self) -> Option<&DraftSnapshot> {
        match self {
            Self::Active { draft, .. }
            | Self::ColdStart { draft, .. }
            | Self::Stopping { draft, .. }
            | Self::PendingUnbound { draft, .. } => Some(draft),
            Self::Idle => None,
        }
    }
}

/// The live visualizer display state (updated from `LiveEvent`s).
#[derive(Debug, Clone)]
pub struct LiveVisualizerState {
    /// The current phase (footer).
    pub phase: LivePhase,
    /// The latest audio output level (waveform), `[0.0, 1.0]`.
    pub level: f64,
    /// The user transcript (the current user turn's merged input transcript).
    /// This shows the CURRENT user turn. After finalization, late equal/suffix
    /// deltas are ignored; the first genuinely different delta starts fresh.
    pub user_transcript: String,
    /// The assistant live transcript (the current assistant turn's merged
    /// output transcript, shown in scrollback with a distinct Live label).
    pub assistant_transcript: String,
    /// Peak decay animation: the last peak value.
    pub peak_decay: f64,
    /// Whether the visualizer should render in narrow fallback mode.
    pub narrow: bool,
    /// The last error message (shown in the error phase).
    pub error_message: Option<String>,
    /// Whether the assistant transcript has been flushed to scrollback this
    /// turn (prevents duplicate scrollback blocks).
    pub assistant_flushed: bool,
    /// Whether the mic is muted (toggled by Space).
    pub muted: bool,
    /// Whether a delegation is currently active (working state).
    pub delegation_active: bool,
    /// Role-local turn state for the user (OMP transcript merge). `finalized`
    /// stores only the current finalized turn; `active` means incremental
    /// `input_transcript.added` deltas are building a turn.
    pub user_turn: RoleTurnState,
    /// Role-local turn state for the assistant (OMP transcript merge).
    pub assistant_turn: RoleTurnState,
}

/// Role-local turn state for OMP transcript merge semantics.
///
/// `finalized` stores the final-frame text only while the **current** role turn
/// is finalized. Starting a genuinely new turn clears it. Keeping this marker
/// role-local is what distinguishes a duplicate late `turn.done` from a new
/// turn that happens to contain the same words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleTurnState {
    /// The normalized final-frame text for the current finalized turn. Cleared
    /// when a new incremental turn starts.
    pub finalized: Option<String>,
    /// Whether the current role turn is being built by incremental transcript
    /// deltas. A finalized turn has `active == false` and `finalized.is_some()`.
    pub active: bool,
}

impl Default for LiveVisualizerState {
    fn default() -> Self {
        Self {
            phase: LivePhase::Connecting,
            level: 0.0,
            user_transcript: String::new(),
            assistant_transcript: String::new(),
            peak_decay: 0.0,
            narrow: false,
            error_message: None,
            assistant_flushed: false,
            muted: false,
            delegation_active: false,
            user_turn: RoleTurnState::default(),
            assistant_turn: RoleTurnState::default(),
        }
    }
}

impl LiveVisualizerState {
    /// Apply a [`LiveEvent`] to the visualizer state. Returns whether the
    /// frame should redraw.
    pub fn apply_event(&mut self, event: &LiveEvent) -> bool {
        match event {
            LiveEvent::Phase(phase) => {
                if self.phase != *phase {
                    self.phase = *phase;
                    return true;
                }
                false
            }
            LiveEvent::Levels(level) => {
                self.level = *level;
                if *level > self.peak_decay {
                    self.peak_decay = *level;
                }
                true
            }
            LiveEvent::Transcript { kind, text } => {
                // OMP `#addTranscript` merge semantics (per role):
                // - Start when current is empty. After finalization, ignore a
                //   late equal/suffix delta; another delta starts fresh.
                // - If incoming `starts_with` current → use incoming (the
                //   server re-sent the accumulated text; incoming is
                //   cumulative).
                // - If current `ends_with` incoming → keep current (incoming
                //   is a trailing duplicate already present).
                // - Otherwise concatenate (incoming is a new incremental
                //   chunk).
                match kind {
                    TranscriptKind::Input => {
                        let merged =
                            merge_add_transcript(&self.user_transcript, text, &mut self.user_turn);
                        let changed = merged != self.user_transcript;
                        self.user_transcript = merged;
                        changed
                    }
                    TranscriptKind::Output => {
                        let merged = merge_add_transcript(
                            &self.assistant_transcript,
                            text,
                            &mut self.assistant_turn,
                        );
                        let changed = merged != self.assistant_transcript;
                        self.assistant_transcript = merged;
                        changed
                    }
                }
            }
            LiveEvent::Turn { role, transcript } => {
                // OMP `#finishTranscript` semantics (per role):
                // - Establish/finalize the role turn.
                // - Preserve a longer current when it `starts_with` the final
                //   text (the active incremental build may be ahead of the
                //   finalized snapshot).
                // - Dedup a repeated final (the finalized text is unchanged).
                match role {
                    super::LiveRole::User => {
                        let merged = merge_finish_transcript(
                            &self.user_transcript,
                            transcript,
                            &mut self.user_turn,
                        );
                        let changed = merged != self.user_transcript;
                        self.user_transcript = merged;
                        changed
                    }
                    super::LiveRole::Assistant => {
                        // Capture whether this is a NEW finalize (the
                        // finalized marker changes) vs a duplicate final. A
                        // new finalize must (re)trigger a scrollback flush even
                        // if the merged text equals the streaming build; a
                        // duplicate final must not.
                        let prev_finalized = self.assistant_turn.finalized.clone();
                        let merged = merge_finish_transcript(
                            &self.assistant_transcript,
                            transcript,
                            &mut self.assistant_turn,
                        );
                        let text_changed = merged != self.assistant_transcript;
                        let new_finalize = self.assistant_turn.finalized != prev_finalized;
                        self.assistant_transcript = merged;
                        // A new finalize needs a scrollback flush (exactly
                        // once). Reset the flush flag only on a NEW finalize,
                        // never on a duplicate final.
                        if new_finalize {
                            self.assistant_flushed = false;
                        }
                        text_changed || new_finalize
                    }
                }
            }
            LiveEvent::Delegation { .. } => {
                // Delegations are handled by the broker/event loop, not the
                // visualizer.
                false
            }
            LiveEvent::Error { message } => {
                self.error_message = Some(message.clone());
                true
            }
            LiveEvent::Closed => {
                // The event loop resets the visualizer on close.
                false
            }
        }
    }

    /// Decay the peak (called on animation tick).
    pub fn decay_peak(&mut self, decay_factor: f64) {
        self.peak_decay *= decay_factor;
        if self.peak_decay < 0.001 {
            self.peak_decay = 0.0;
        }
    }

    /// Explicitly clear all assistant-turn display/merge state.
    ///
    /// Normal `turn.done` handling deliberately does **not** call this: OMP
    /// needs the finalized current turn to reject late duplicate deltas/finals.
    pub fn reset_turn(&mut self) {
        self.assistant_transcript.clear();
        self.assistant_flushed = false;
        self.assistant_turn = RoleTurnState::default();
    }
}

/// Identity of the transient Live-assistant scrollback block for the current
/// spoken turn. It is independent of the coding agent's own streaming block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveAssistantTranscriptEntry {
    pub generation: Generation,
    pub agent_id: AgentId,
    pub entry_id: crate::scrollback::entry::EntryId,
}

/// The full Live runtime state stored in `AppView`.
#[derive(Debug, Default)]
pub struct LiveRuntime {
    /// The lifecycle state (idle / pending / cold-start / active / stopping).
    pub state: LiveState,
    /// The visualizer display state.
    pub visualizer: LiveVisualizerState,
    /// The context channel (commands into the pipeline), if the pipeline is up.
    pub cmd_tx: Option<tokio::sync::mpsc::Sender<LiveCommand>>,
    /// Registered delegations: `(generation, delegation_id) -> entry`.
    pub delegations: std::collections::HashMap<(Generation, String), DelegationEntry>,
    /// Monotonic generation counter.
    pub generation_counter: Generation,
    /// The muted state (toggled by Space).
    pub muted: bool,
    /// The delegation broker (wired into ACP ingress).
    pub broker: crate::live::broker::LiveDelegationBroker,
    /// Transient scrollback block used to stream the spoken assistant's current
    /// transcript. Finalized on `turn.done` or Live teardown.
    pub assistant_transcript_entry: Option<LiveAssistantTranscriptEntry>,
    /// Pending **critical** commands (`CompleteDelegation` / `Shutdown`) that
    /// couldn't be sent via `try_send` because the channel was full. Each
    /// entry carries a stable, monotonic sequence ID so the capacity-aware
    /// async drain arm can snapshot/clone an entry **without removing it**,
    /// await `send().await`, and — only on a confirmed successful send —
    /// remove exactly that sequence ID. If the drain arm is cancelled (a
    /// higher-priority `tokio::select` arm wins) or times out, the entry
    /// stays queued and is retried later. This is cancellation-safe: a dropped
    /// pending `send` future can never lose a final command.
    ///
    /// Non-critical commands (commentary, mute toggles) are never queued —
    /// they are shed under pressure — so this vector holds only critical
    /// commands, in insertion order.
    pub pending_cmds: Vec<PendingCritical>,
    /// Monotonic sequence counter for `pending_cmds` entries (cancellation-safe
    /// drain identity).
    pending_seq: u64,
}

/// A pending critical command with a stable sequence ID for cancellation-safe
/// drain. The sequence ID lets the async drain arm clone the command without
/// removing it, then remove exactly this entry once the send is confirmed.
#[derive(Debug, Clone)]
pub struct PendingCritical {
    /// Stable, monotonic identity assigned when the command was queued.
    pub seq: u64,
    /// The critical command (`CompleteDelegation` or `Shutdown`).
    pub cmd: LiveCommand,
}

impl PendingCritical {
    /// Whether this entry holds a critical command.
    pub fn is_critical(&self) -> bool {
        matches!(
            self.cmd,
            LiveCommand::CompleteDelegation { .. } | LiveCommand::Shutdown
        )
    }
}

impl LiveRuntime {
    /// Allocate the next generation.
    pub fn next_generation(&mut self) -> Generation {
        self.generation_counter += 1;
        self.generation_counter
    }

    /// Register a delegation. Returns the entry.
    pub fn register_delegation(
        &mut self,
        generation: Generation,
        delegation_id: String,
        agent_id: AgentId,
        session_id: String,
        prompt_id: String,
    ) -> DelegationEntry {
        let entry = DelegationEntry {
            generation,
            delegation_id: delegation_id.clone(),
            agent_id,
            session_id,
            prompt_id,
            terminal: false,
        };
        self.delegations
            .insert((generation, delegation_id), entry.clone());
        entry
    }

    /// Mark a delegation terminal (idempotent).
    pub fn mark_delegation_terminal(&mut self, generation: Generation, delegation_id: &str) {
        if let Some(entry) = self
            .delegations
            .get_mut(&(generation, delegation_id.to_string()))
        {
            entry.terminal = true;
        }
    }

    /// Check if a delegation is terminal.
    pub fn is_delegation_terminal(&self, generation: Generation, delegation_id: &str) -> bool {
        self.delegations
            .get(&(generation, delegation_id.to_string()))
            .is_some_and(|e| e.terminal)
    }

    /// Check if a delegation is registered.
    pub fn has_delegation(&self, generation: Generation, delegation_id: &str) -> bool {
        self.delegations
            .contains_key(&(generation, delegation_id.to_string()))
    }

    /// Send a best-effort command into the pipeline. If the channel is full,
    /// `CompleteDelegation` and `Shutdown` are queued in `pending_cmds` (with
    /// a fresh sequence ID) for reliable ordered delivery on the next drain.
    /// Commentary (`AppendDelegationContext`) and mute toggles may be shed
    /// under pressure — they are never queued.
    pub fn send_cmd(&mut self, cmd: LiveCommand) {
        if let Some(tx) = &self.cmd_tx {
            if tx.try_send(cmd.clone()).is_ok() {
                return;
            }
            // Channel full — queue critical commands, shed commentary.
            match &cmd {
                LiveCommand::CompleteDelegation { .. } | LiveCommand::Shutdown => {
                    self.pending_seq += 1;
                    self.pending_cmds.push(PendingCritical {
                        seq: self.pending_seq,
                        cmd,
                    });
                }
                LiveCommand::AppendDelegationContext { .. }
                | LiveCommand::AppendSessionContext { .. } => {
                    // Commentary may be shed/throttled.
                    tracing::debug!("Live commentary shed (channel full)");
                }
                LiveCommand::ToggleMute | LiveCommand::SetMuted(_) => {
                    // Mute toggles are best-effort (not critical).
                }
            }
        }
    }

    /// Drain pending commands into the channel. Called at the top of each
    /// event-loop tick (fast path: non-blocking `try_send`). Returns the
    /// number of commands successfully sent. Successfully sent entries are
    /// removed; entries that still don't fit remain queued (preserving order
    /// and sequence IDs).
    ///
    /// This is the synchronous reaper. Critical commands (`CompleteDelegation`,
    /// `Shutdown`) that still can't fit are kept in `pending_cmds` and retried
    /// by the capacity-aware async drain arm in the event loop (see
    /// [`LiveRuntime::snapshot_pending_critical_head`]), which awaits the
    /// bounded channel's `send` with a short timeout so a full channel can't
    /// leave a final `CompleteDelegation` stranded when no unrelated app event
    /// ever wakes the loop.
    pub fn drain_pending_cmds(&mut self) -> usize {
        if self.pending_cmds.is_empty() {
            return 0;
        }
        let Some(tx) = &self.cmd_tx else {
            // Channel gone — drop pending (teardown will handle).
            self.pending_cmds.clear();
            return 0;
        };
        let mut sent = 0;
        let mut remaining = Vec::new();
        for entry in self.pending_cmds.drain(..) {
            if tx.try_send(entry.cmd.clone()).is_ok() {
                sent += 1;
            } else {
                remaining.push(entry);
            }
        }
        self.pending_cmds = remaining;
        sent
    }

    /// Whether there is at least one pending critical command
    /// (`CompleteDelegation` or `Shutdown`) awaiting delivery. Used by the
    /// event loop's capacity-aware async drain arm to decide whether to arm a
    /// `send().await` wake.
    pub fn has_pending_critical(&self) -> bool {
        self.pending_cmds.iter().any(|e| e.is_critical())
    }

    /// Snapshot the head pending critical command **without removing it**,
    /// returning a clone of the command plus its stable sequence ID and a
    /// clone of the channel sender, for the event loop's async drain arm.
    ///
    /// This is **cancellation-safe**: the entry stays in `pending_cmds` until
    /// the drain arm confirms a successful `send().await` and calls
    /// [`LiveRuntime::forget_pending_critical`] with the snapshot's sequence
    /// ID. If the drain arm is cancelled (a higher-priority `tokio::select`
    /// arm wins while the send is pending) or the send times out, the entry
    /// remains queued and is retried on the next iteration (loop-top
    /// `drain_pending_cmds` first, then this arm again). A dropped pending
    /// `send` future can never lose a final command.
    ///
    /// Returns `None` when there are no pending critical commands or no
    /// channel is bound.
    pub fn snapshot_pending_critical_head(
        &self,
    ) -> Option<(u64, LiveCommand, tokio::sync::mpsc::Sender<LiveCommand>)> {
        if !self.has_pending_critical() {
            return None;
        }
        let tx = self.cmd_tx.clone()?;
        let entry = self.pending_cmds.iter().find(|e| e.is_critical())?;
        Some((entry.seq, entry.cmd.clone(), tx))
    }

    /// Remove the pending critical entry with the given sequence ID, exactly
    /// once, after the async drain arm confirmed a successful `send().await`.
    /// This is the ONLY removal path for the async drain arm. If the entry was
    /// already removed (e.g. the loop-top `drain_pending_cmds` delivered it
    /// via `try_send` while the arm was armed), this is a no-op — the command
    /// is never sent twice because the arm's `send().await` either succeeded
    /// (and the receiver got it) or the entry is gone.
    ///
    /// Returns `true` if an entry with that sequence ID was present and
    /// removed.
    pub fn forget_pending_critical(&mut self, seq: u64) -> bool {
        let before = self.pending_cmds.len();
        self.pending_cmds.retain(|e| e.seq != seq);
        self.pending_cmds.len() < before
    }

    /// Hard teardown: drop the channel, reset state, forget delegations,
    /// terminalize broker. Idempotent.
    pub fn teardown(&mut self) {
        self.send_cmd(LiveCommand::Shutdown);
        self.cmd_tx = None;
        self.state = LiveState::Idle;
        self.visualizer = LiveVisualizerState::default();
        self.delegations.clear();
        self.muted = false;
        self.broker.clear();
        self.assistant_transcript_entry = None;
        self.pending_cmds.clear();
        set_live_active(false);
    }

    /// Find the delegation for a given prompt_id in the current generation
    /// (non-terminal only). Used by the broker to correlate ACP ingress.
    pub fn find_delegation_by_prompt_id(&self, prompt_id: &str) -> Option<String> {
        let current_gen = self.generation_counter;
        for ((generation, _), entry) in &self.delegations {
            if *generation == current_gen && entry.prompt_id == prompt_id && !entry.terminal {
                return Some(entry.delegation_id.clone());
            }
        }
        None
    }
}

// ── OMP transcript merge semantics: pure helpers ────────────────────────────
//
// These pure functions implement the OpenAI Manager Protocol (OMP) exact
// transcript merge semantics for `#addTranscript` (incremental
// `*_transcript.added` deltas) and `#finishTranscript` (`turn.done`). They are
// factored out of `LiveVisualizerState::apply_event` so the merge contract can
// be unit-tested directly without constructing `LiveEvent`s.
//
// Invariants:
// - `#addTranscript`: start when current is empty. If the current turn is
//   finalized, ignore an equal or suffix late delta; any other delta starts a
//   new turn. While active, cumulative re-sends replace, suffix duplicates are
//   ignored, and other chunks concatenate.
// - `#finishTranscript`: establish/finalize the role turn; preserve a longer
//   active current when it starts with the final text. Only an exact repeat of
//   this role-local turn's final frame is a duplicate; another final starts a turn.
//
// The visualizer shows the CURRENT user turn; the final assistant scrollback
// contains the full merged/final turn exactly once.

/// OMP `#addTranscript` merge: fold an incoming `*_transcript.added` delta
/// into the role's current turn text, updating the role-local turn state.
/// Returns the new current text.
///
/// - Empty current starts a turn.
/// - A finalized current ignores equal/suffix late deltas; another delta starts
///   a new turn and replaces the old current.
/// - While active, a cumulative incoming value replaces current, a suffix
///   duplicate is ignored, and any other chunk concatenates.
pub fn merge_add_transcript(current: &str, incoming: &str, turn: &mut RoleTurnState) -> String {
    // OMP ignores an empty delta before touching turn state.
    if incoming.is_empty() {
        return current.to_string();
    }

    let was_final = !turn.active && turn.finalized.is_some();
    let (candidate, starts_new_turn) = if current.is_empty() {
        (incoming.to_string(), true)
    } else if was_final {
        // A late cumulative copy or suffix may arrive after `turn.done`. It is
        // still part of the finalized turn, not evidence of a new turn.
        if incoming == current || current.ends_with(incoming) {
            return current.to_string();
        }
        (incoming.to_string(), true)
    } else if incoming.starts_with(current) {
        // Incoming is a cumulative resend of the active turn.
        (incoming.to_string(), false)
    } else if current.ends_with(incoming) {
        // Incoming is a trailing duplicate already present in the active turn.
        return current.to_string();
    } else {
        // Incoming is a new incremental chunk.
        let mut next = String::with_capacity(current.len() + incoming.len());
        next.push_str(current);
        next.push_str(incoming);
        (next, false)
    };

    // OMP normalizes only after merging raw chunks. This preserves a leading
    // space in an incremental delta while avoiding whitespace-only transcripts.
    let normalized = candidate.trim();
    if normalized.is_empty() {
        return current.to_string();
    }
    if starts_new_turn {
        turn.finalized = None;
        turn.active = true;
    }
    normalized.to_string()
}

/// OMP `#finishTranscript` merge: finalize the role's turn with the final
/// transcript text, updating the role-local turn state. Returns the new
/// current text.
///
/// - Establish/finalize the role turn (mark not active, record current).
/// - Preserve a longer active current when it starts with the final text.
/// - Ignore only an exact repeat of the current finalized text. A different
///   final after finalization is a new role-local turn.
pub fn merge_finish_transcript(
    current: &str,
    final_text: &str,
    turn: &mut RoleTurnState,
) -> String {
    // OMP ignores an empty final before touching turn state.
    if final_text.is_empty() {
        return current.to_string();
    }

    let normalized_final = final_text.trim();
    if normalized_final.is_empty() {
        return current.to_string();
    }

    let was_final = !turn.active && turn.finalized.is_some();
    let candidate = if current.is_empty() {
        final_text.to_string()
    } else if was_final {
        // Dedup only within this role-local finalized turn. The marker is
        // cleared by the first genuine add of a new turn, so a later turn may
        // legitimately finish with identical text.
        if turn.finalized.as_deref() == Some(normalized_final) {
            return current.to_string();
        }
        final_text.to_string()
    } else if current.starts_with(final_text) && current.len() > final_text.len() {
        // The active incremental build is ahead of the final snapshot.
        current.to_string()
    } else {
        final_text.to_string()
    };

    let normalized = candidate.trim();
    if normalized.is_empty() {
        return current.to_string();
    }
    let merged = normalized.to_string();
    // Retain the normalized final-frame text for same-turn duplicate detection.
    // A new incremental turn clears it before its own final arrives.
    turn.finalized = Some(normalized_final.to_string());
    turn.active = false;
    merged
}

// These pure functions factor the drain outcome decision out of the async
// event-loop arm so it can be unit-tested without a tokio runtime or a live
// channel. They encode the invariant the arm must uphold: a final critical
// command is removed from the queue EXACTLY when (and only when) its send was
// confirmed successful; on timeout or cancellation it stays queued; the same
// command is never sent twice.

/// The outcome of an attempted critical-command send, as observed by the drain
/// arm. `Sent` is a confirmed successful `send().await` (the receiver owns the
/// value). `Timeout` is a timed-out send (capacity never freed in time).
/// `Cancelled` is the case where a higher-priority `tokio::select` arm won
/// while the send was pending (the future was dropped) — modelled explicitly
/// so the helper proves cancellation leaves the command queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CriticalSendOutcome {
    /// `send().await` resolved `Ok(())` — the receiver got the command.
    Sent,
    /// The send timed out before capacity freed.
    Timeout,
    /// The drain future was cancelled (a higher-priority select arm won).
    Cancelled,
}

/// The result of applying a drain outcome to a snapshot identity: whether the
/// queued entry with this sequence ID should be forgotten (removed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainForget {
    /// Remove the entry with this sequence ID (the send succeeded).
    Forget(u64),
    /// Leave the entry queued (timeout or cancellation — retry later).
    Keep,
}

/// Pure decision: given a snapshot's sequence ID and the observed send
/// outcome, decide whether to forget (remove) that entry or keep it queued.
///
/// - `Sent` → `Forget(seq)` (remove exactly this entry once).
/// - `Timeout` → `Keep` (stays queued for retry).
/// - `Cancelled` → `Keep` (stays queued — the dropped future lost nothing).
///
/// This is the single source of truth for the cancellation-safety invariant.
/// The async arm calls `forget_pending_critical(seq)` only when this returns
/// `Forget(seq)`.
pub fn critical_drain_forget_decision(seq: u64, outcome: CriticalSendOutcome) -> DrainForget {
    match outcome {
        CriticalSendOutcome::Sent => DrainForget::Forget(seq),
        CriticalSendOutcome::Timeout | CriticalSendOutcome::Cancelled => DrainForget::Keep,
    }
}

#[cfg(test)]
mod critical_drain_tests {
    use super::*;

    fn runtime_with_full_channel() -> LiveRuntime {
        let mut runtime = LiveRuntime::default();
        let (tx, _rx) = tokio::sync::mpsc::channel::<LiveCommand>(1);
        runtime.cmd_tx = Some(tx);
        // Fill the single slot.
        runtime
            .cmd_tx
            .as_ref()
            .unwrap()
            .try_send(LiveCommand::ToggleMute)
            .unwrap();
        runtime
    }

    #[test]
    fn cancellation_leaves_command_queued() {
        // Snapshot the head WITHOUT removing it; a cancelled send must leave
        // the entry queued.
        let mut runtime = runtime_with_full_channel();
        runtime.send_cmd(LiveCommand::CompleteDelegation {
            delegation_id: "del-c".to_string(),
            text: "final".to_string(),
        });
        assert!(runtime.has_pending_critical());

        let (seq, _cmd, _tx) = runtime
            .snapshot_pending_critical_head()
            .expect("pending critical present");
        // Snapshot did NOT remove the entry.
        assert!(runtime.has_pending_critical());
        assert_eq!(runtime.pending_cmds.len(), 1);

        // A cancelled send: the decision is Keep, and we never call forget.
        let decision = critical_drain_forget_decision(seq, CriticalSendOutcome::Cancelled);
        assert_eq!(decision, DrainForget::Keep);
        // The entry remains queued.
        assert!(runtime.has_pending_critical());
        assert_eq!(runtime.pending_cmds.len(), 1);
        assert_eq!(runtime.pending_cmds[0].seq, seq);
        assert!(matches!(
            &runtime.pending_cmds[0].cmd,
            LiveCommand::CompleteDelegation { delegation_id, .. }
                if delegation_id == "del-c"
        ));
    }

    #[test]
    fn timeout_keeps_command_queued() {
        let mut runtime = runtime_with_full_channel();
        runtime.send_cmd(LiveCommand::CompleteDelegation {
            delegation_id: "del-t".to_string(),
            text: "final".to_string(),
        });
        let (seq, _cmd, _tx) = runtime
            .snapshot_pending_critical_head()
            .expect("pending critical present");

        let decision = critical_drain_forget_decision(seq, CriticalSendOutcome::Timeout);
        assert_eq!(decision, DrainForget::Keep);
        // Still queued.
        assert!(runtime.has_pending_critical());
        assert_eq!(runtime.pending_cmds.len(), 1);
        assert_eq!(runtime.pending_cmds[0].seq, seq);
    }

    #[test]
    fn capacity_release_sends_once_and_removes() {
        // A successful send forgets exactly the snapshotted entry once; a
        // duplicate forget is a no-op (the same final is never removed/sent
        // twice). The real send is exercised by the async tests; this proves
        // the pure decision + forget contract synchronously.
        let mut runtime = runtime_with_full_channel();
        runtime.send_cmd(LiveCommand::CompleteDelegation {
            delegation_id: "del-s".to_string(),
            text: "\"Agent Final Message\":\n\ndone".to_string(),
        });
        let (seq, _cmd, _tx) = runtime
            .snapshot_pending_critical_head()
            .expect("pending critical present");
        // Still queued (snapshot didn't remove).
        assert_eq!(runtime.pending_cmds.len(), 1);

        // Confirm the decision for a successful send is Forget.
        let decision = critical_drain_forget_decision(seq, CriticalSendOutcome::Sent);
        assert_eq!(decision, DrainForget::Forget(seq));
        // Apply the forget: the entry is removed exactly once.
        assert!(runtime.forget_pending_critical(seq));
        assert!(!runtime.has_pending_critical());
        assert!(runtime.pending_cmds.is_empty());
        // Forgetting again is a no-op (never sent twice).
        assert!(!runtime.forget_pending_critical(seq));
    }

    #[test]
    fn duplicate_drain_cannot_resend() {
        // After a successful send forgets the entry, a second snapshot returns
        // None (nothing to drain), so the same final can never be sent twice.
        let mut runtime = runtime_with_full_channel();
        runtime.send_cmd(LiveCommand::CompleteDelegation {
            delegation_id: "del-d".to_string(),
            text: "final".to_string(),
        });
        let (seq, _cmd, _tx) = runtime
            .snapshot_pending_critical_head()
            .expect("pending critical present");
        // Successful send → forget.
        let decision = critical_drain_forget_decision(seq, CriticalSendOutcome::Sent);
        assert_eq!(decision, DrainForget::Forget(seq));
        assert!(runtime.forget_pending_critical(seq));
        // No more pending critical — a second drain arm snapshot is None.
        assert!(runtime.snapshot_pending_critical_head().is_none());
        assert!(!runtime.has_pending_critical());
    }

    #[test]
    fn snapshot_does_not_remove_and_preserves_order() {
        // Multiple critical commands: snapshotting the head leaves all queued,
        // and forgetting the snapshotted one preserves the order of the rest.
        let mut runtime = runtime_with_full_channel();
        runtime.send_cmd(LiveCommand::CompleteDelegation {
            delegation_id: "del-1".to_string(),
            text: "f1".to_string(),
        });
        runtime.send_cmd(LiveCommand::CompleteDelegation {
            delegation_id: "del-2".to_string(),
            text: "f2".to_string(),
        });
        assert_eq!(runtime.pending_cmds.len(), 2);
        let (seq1, _cmd1, _tx) = runtime
            .snapshot_pending_critical_head()
            .expect("head present");
        // Snapshot didn't remove anything.
        assert_eq!(runtime.pending_cmds.len(), 2);
        // Forget the head; the second remains.
        assert!(runtime.forget_pending_critical(seq1));
        assert_eq!(runtime.pending_cmds.len(), 1);
        // The remaining entry is the second one.
        let (seq2, _cmd2, _tx) = runtime
            .snapshot_pending_critical_head()
            .expect("second now head");
        assert_ne!(seq1, seq2);
        assert!(runtime.forget_pending_critical(seq2));
        assert!(runtime.pending_cmds.is_empty());
    }
}
