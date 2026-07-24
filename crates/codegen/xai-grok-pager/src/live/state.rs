//! Live session state — the pager-owned lifecycle for `/live`.
//!
//! Bound to `(generation, AgentId, SessionId)`. One session at a time.
//! Stops idempotently on session/view switch, close/exit/quit, ACP disconnect,
//! and teardown. Mutually exclusive with `/voice` dictation.

use std::sync::atomic::{AtomicBool, Ordering};

use super::{LiveCommand, LiveContextChannel, LiveEvent, LiveLevels, LivePhase, LiveRole};

/// Process-global Live gate for view code without an `AppView`.
pub(crate) static LIVE_ENABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static LIVE_ACTIVE: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
pub(crate) fn live_enabled() -> bool {
    LIVE_ENABLED.load(Ordering::Acquire)
}

#[allow(dead_code)]
pub(crate) fn live_active() -> bool {
    LIVE_ACTIVE.load(Ordering::Acquire)
}

/// Test helper for the process-global Live gate.
pub fn set_live_enabled_for_test(on: bool) {
    LIVE_ENABLED.store(on, Ordering::Release);
}

/// Test helper for the process-global Live-active flag.
pub fn set_live_active_for_test(on: bool) {
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
    /// The delegation id (from `LiveDelegation`).
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
    /// completes.
    PendingUnbound { agent_id: AgentId },
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
            | Self::PendingUnbound { agent_id } => Some(*agent_id),
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
            | Self::Stopping { draft, .. } => Some(draft),
            Self::PendingUnbound { .. } | Self::Idle => None,
        }
    }
}

/// The live visualizer display state (updated from `LiveEvent`s).
#[derive(Debug, Clone, Default)]
pub struct LiveVisualizerState {
    /// The current phase (footer).
    pub phase: LivePhase,
    /// The latest audio levels (waveform).
    pub levels: LiveLevels,
    /// The user transcript (accumulated finalized user segments).
    pub user_transcript: String,
    /// The assistant live transcript (streaming, shown in scrollback).
    pub assistant_transcript: String,
    /// Whether the assistant transcript has been finalized this turn.
    pub assistant_finalized: bool,
    /// Peak decay animation: the last peak value and its timestamp (ms).
    pub peak_decay: f32,
    /// Whether the visualizer should render in narrow fallback mode.
    pub narrow: bool,
    /// The last error message (shown in the error phase).
    pub error_message: Option<String>,
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
            LiveEvent::Levels(levels) => {
                self.levels = levels.clone();
                // Peak decay: track the max of user/assistant peaks.
                let peak = levels.user_peak.max(levels.assistant_peak);
                if peak > self.peak_decay {
                    self.peak_decay = peak;
                }
                true
            }
            LiveEvent::Transcript {
                role,
                text,
                finalized,
            } => {
                match role {
                    LiveRole::User => {
                        if *finalized {
                            if !self.user_transcript.is_empty() {
                                self.user_transcript.push(' ');
                            }
                            self.user_transcript.push_str(text);
                        }
                    }
                    LiveRole::Assistant => {
                        if *finalized {
                            // Coalesce: append a space + the finalized segment.
                            if !self.assistant_transcript.is_empty() {
                                self.assistant_transcript.push(' ');
                            }
                            self.assistant_transcript.push_str(text);
                            self.assistant_finalized = true;
                        } else {
                            // Live partial: replace the last partial (coalesce
                            // role-local transcript: a new partial supersedes
                            // the previous partial, keeping finalized segments).
                            self.assistant_transcript = text.clone();
                        }
                    }
                }
                true
            }
            LiveEvent::Delegation(_) => {
                // Delegations are handled by the broker/event loop, not the
                // visualizer. No redraw needed here.
                false
            }
            LiveEvent::Error { message } => {
                self.phase = LivePhase::Error;
                self.error_message = Some(message.clone());
                true
            }
            LiveEvent::Closed => {
                // The event loop resets the visualizer on close; nothing to
                // do here.
                false
            }
        }
    }

    /// Decay the peak (called on animation tick).
    pub fn decay_peak(&mut self, decay_factor: f32) {
        self.peak_decay *= decay_factor;
        if self.peak_decay < 0.01 {
            self.peak_decay = 0.0;
        }
    }

    /// Reset for a new turn (assistant transcript cleared).
    pub fn reset_turn(&mut self) {
        self.assistant_transcript.clear();
        self.assistant_finalized = false;
    }
}

/// The full Live runtime state stored in `AppView`.
#[derive(Debug, Default)]
pub struct LiveRuntime {
    /// The lifecycle state (idle / pending / cold-start / active / stopping).
    pub state: LiveState,
    /// The visualizer display state.
    pub visualizer: LiveVisualizerState,
    /// The context channel (commands into the pipeline), if the pipeline is up.
    pub cmd_channel: Option<LiveContextChannel>,
    /// Registered delegations: `(generation, delegation_id) -> entry`.
    pub delegations: std::collections::HashMap<(Generation, String), DelegationEntry>,
    /// Monotonic generation counter.
    pub generation_counter: Generation,
    /// The muted state (toggled by Space).
    pub muted: bool,
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

    /// Send a best-effort command into the pipeline (no-op if it isn't up).
    pub fn send_cmd(&self, cmd: LiveCommand) {
        if let Some(ch) = &self.cmd_channel {
            ch.try_send(cmd);
        }
    }

    /// Hard teardown: drop the channel, reset state, forget delegations.
    pub fn teardown(&mut self) {
        self.send_cmd(LiveCommand::Shutdown);
        self.cmd_channel = None;
        self.state = LiveState::Idle;
        self.visualizer = LiveVisualizerState::default();
        self.delegations.clear();
        self.muted = false;
        LIVE_ACTIVE.store(false, Ordering::Release);
    }
}
