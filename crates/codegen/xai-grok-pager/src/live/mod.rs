//! Codex Live (`/live`): real-time bidirectional voice sessions against the
//! Codex sideband, integrated into the pager's session/event-loop/dispatch
//! lifecycle.
//!
//! Layering (pager-owned):
//! - **Live gate** — default **on**. `GROK_CODEX_LIVE=0` or a requirements/config
//!   `[features] codex_live: false` kill switch disables it. Independent of the
//!   xAI `/voice` subscription tier (Codex Live is a separate channel).
//! - **Session scope** — `/live` is bound to `(generation, AgentId, SessionId)`
//!   and stops idempotently on session/view switch, close/exit/quit, ACP
//!   disconnect, and teardown.
//! - **Mutual exclusion with `/voice`** — starting either deterministically
//!   stops/blocks the other.
//! - **Delegation** — `LiveEvent::Delegation` submits literal plain text through
//!   the existing prompt/effect pipeline to the bound current AgentSession,
//!   preserving the draft, and the [`broker::LiveDelegationBroker`] correlates
//!   ACP ingress to flush commentary and send `CompleteDelegation` exactly once.
//! - **UI** — a fixed-height 5-row full-width visualizer replaces the editor
//!   while Live is active (phases, waveform, user transcript, phase footer).
//!
//! ## Feature model
//! The `codex-live` cargo feature enables this module. It currently uses local
//! test doubles for the `xai_grok_voice::live` public contract (see `doubles`
//! below) so the pager compiles and tests pass without the real audio/network
//! stack. When the voice crate's `live` feature stabilizes, the `codex-live`
//! feature will forward `xai-grok-voice/live` and the re-exports will switch to
//! the real types (minor compile adaptation only — the contract shapes match).

pub mod auth;
pub mod broker;
pub mod config;
pub mod gate;
pub mod handle;
pub mod prompts;
pub mod state;
pub mod visualizer;

#[cfg(test)]
mod tests;

// ── Public type re-exports ──────────────────────────────────────────────────
//
// We always compile against the local doubles for now. When the voice crate's
// `live` API is stable, this will switch to:
//
//   #[cfg(feature = "voice-live")]
//   pub use xai_grok_voice::live::{...};
//
// The doubles match the expected public contract so the switch is a drop-in.

pub use self::doubles::{
    LiveAuth, LiveAuthProvider, LiveCommand, LiveConfig, LiveContextChannel, LiveDelegation,
    LiveEvent, LiveLevels, LivePhase, LiveRole, SharedLiveAuth, run_live_session,
};

mod doubles {
    //! Test doubles for the `xai_grok_voice::live` public API contract.
    //!
    //! These match the expected shapes (`LiveAuth { bearer, account_id }`,
    //! object-safe `LiveAuthProvider`/`SharedLiveAuth`, `LiveConfig`,
    //! `LiveContextChannel`, `LiveCommand`, `LiveEvent`, `LivePhase`,
    //! `LiveRole`, `run_live_session`) so the pager's integration code
    //! type-checks and unit tests run without the real audio/network stack.

    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    use tokio::sync::mpsc;

    #[derive(Debug, Clone)]
    pub struct LiveAuth {
        pub bearer: String,
        pub account_id: String,
    }

    pub trait LiveAuthProvider: Send + Sync + std::fmt::Debug {
        fn live_auth(&self) -> Pin<Box<dyn Future<Output = Option<LiveAuth>> + Send + '_>>;
    }

    pub type SharedLiveAuth = Arc<dyn LiveAuthProvider>;

    #[derive(Debug, Clone, Default)]
    pub struct LiveConfig {
        pub codex_base: String,
        pub sideband_base: String,
        pub session_id: String,
        pub instructions: String,
        pub voice: String,
        pub client_version: String,
    }

    #[derive(Debug, Clone)]
    pub struct LiveContextChannel {
        tx: mpsc::Sender<LiveCommand>,
    }

    impl LiveContextChannel {
        pub fn pair(capacity: usize) -> (Self, mpsc::Receiver<LiveCommand>) {
            let (tx, rx) = mpsc::channel(capacity);
            (Self { tx }, rx)
        }
        pub fn from_sender(tx: mpsc::Sender<LiveCommand>) -> Self {
            Self { tx }
        }
        pub fn try_send(&self, cmd: LiveCommand) -> bool {
            self.tx.try_send(cmd).is_ok()
        }
        pub fn is_closed(&self) -> bool {
            self.tx.is_closed()
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub enum LivePhase {
        #[default]
        Connecting,
        Listening,
        Working,
        Speaking,
        Muted,
        Error,
    }

    impl LivePhase {
        pub fn as_key(self) -> &'static str {
            match self {
                Self::Connecting => "connecting",
                Self::Listening => "listening",
                Self::Working => "working",
                Self::Speaking => "speaking",
                Self::Muted => "muted",
                Self::Error => "error",
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub enum LiveRole {
        #[default]
        User,
        Assistant,
    }

    impl LiveRole {
        pub fn as_key(self) -> &'static str {
            match self {
                Self::User => "user",
                Self::Assistant => "assistant",
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct LiveLevels {
        pub user_peak: f32,
        pub assistant_peak: f32,
        pub user_rms: f32,
        pub assistant_rms: f32,
    }

    #[derive(Debug, Clone)]
    pub struct LiveDelegation {
        pub id: String,
        pub text: String,
    }

    #[derive(Debug)]
    pub enum LiveEvent {
        Phase(LivePhase),
        Levels(LiveLevels),
        Transcript {
            role: LiveRole,
            text: String,
            finalized: bool,
        },
        Delegation(LiveDelegation),
        Error {
            message: String,
        },
        Closed,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum LiveCommand {
        ToggleMute,
        SetMuted(bool),
        AppendDelegationContext {
            delegation_id: String,
            text: String,
        },
        CompleteDelegation {
            delegation_id: String,
            final_message: String,
        },
        AppendSessionContext {
            text: String,
        },
        Shutdown,
    }

    pub async fn run_live_session(
        _config: LiveConfig,
        _auth: SharedLiveAuth,
        mut cmd_rx: mpsc::Receiver<LiveCommand>,
        _event_tx: mpsc::Sender<LiveEvent>,
    ) {
        // Stub: drain commands until Shutdown or channel close.
        while let Some(cmd) = cmd_rx.recv().await {
            if matches!(cmd, LiveCommand::Shutdown) {
                break;
            }
        }
    }
}
