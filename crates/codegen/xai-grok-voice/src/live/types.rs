//! Public API contract for the Codex Live subsystem.
//!
//! These types are the surface the pager adapts onto. Internals (transport,
//! media, protocol) may be refined, but these names/shapes stay stable so the
//! pager's `codex-live` feature wiring is a thin adapter.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::protocol::{LiveContextChannel, LiveRole, LiveServerEvent};

/// Resolved Codex (ChatGPT-OAuth) credential for a live call.
#[derive(Debug, Clone)]
pub struct LiveAuth {
    /// Bearer access token sent as `Authorization: Bearer <bearer>`.
    pub bearer: String,
    /// ChatGPT account id sent as `chatgpt-account-id` (optional).
    pub account_id: Option<String>,
}

/// Async, object-safe auth provider for the live subsystem.
///
/// The pager adapts its refreshing OAuth/session provider onto this trait so a
/// long-lived live session can resolve a fresh bearer at each connection (a
/// bearer rotates ~15 min) and force a refresh on a once-only 401.
pub trait LiveAuthProvider: std::fmt::Debug + Send + Sync + 'static {
    /// Resolve a [`LiveAuth`] (bearer + optional account id) for the next
    /// connection. Returns `None` when no credential is available.
    fn bearer_account(&self) -> Pin<Box<dyn Future<Output = Option<LiveAuth>> + Send + '_>>;

    /// Force a credential refresh (e.g. on a 401). The provider may no-op if it
    /// cannot refresh; the next `bearer_account` call returns the freshest
    /// available token.
    fn force_refresh(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Shared provider handed to the live session/transport.
pub type SharedLiveAuth = Arc<dyn LiveAuthProvider>;

/// Live session configuration.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// Codex backend base URL (e.g. `https://chatgpt.com/backend-api`). The
    /// signaling path `/realtime/calls?intent=quicksilver&architecture=avas`
    /// is appended to this.
    pub codex_base: String,
    /// Sideband base URL. When unset, the sideband wss URL is derived from the
    /// server-assigned call id as `wss://api.openai.com/v1/live/<callId>`.
    pub sideband_base: Option<String>,
    /// Pager-stamped session id; sent as `session-id`/`thread-id`.
    pub session_id: String,
    /// System instructions for the live model.
    pub instructions: String,
    /// Output voice id (e.g. `alloy`).
    pub voice: String,
    /// Pinned Codex client version sent as `version` and in the `User-Agent`.
    pub client_version: String,
}

/// Commands from the pager event loop to the live session.
#[derive(Debug, Clone)]
pub enum LiveCommand {
    /// Toggle the mic mute state.
    ToggleMute,
    /// Set the mic mute state explicitly.
    SetMuted(bool),
    /// Append context to a server-created delegation, chunked into ≤500-byte
    /// appends. `text` is split by the session before sending.
    AppendDelegationContext {
        delegation_id: String,
        text: String,
        channel: LiveContextChannel,
    },
    /// Mark a delegation complete with its final text.
    CompleteDelegation { delegation_id: String, text: String },
    /// Append context to the live session outside a delegation, chunked.
    AppendSessionContext {
        text: String,
        channel: LiveContextChannel,
    },
    /// Tear down the live session (idempotent).
    Shutdown,
}

/// Events emitted by [`super::run_live_session`] to the pager event loop.
#[derive(Debug, Clone)]
pub enum LiveEvent {
    /// The session reached a new phase (connecting/connected/closing/closed).
    Phase(LivePhase),
    /// Output (speaker) audio level, `[0.0, 1.0]`.
    Levels(f64),
    /// A transcript event (input or output).
    Transcript { kind: TranscriptKind, text: String },
    /// A delegation was created by the server.
    Delegation { id: String, content: Vec<String> },
    /// A turn completed.
    Turn { role: LiveRole, transcript: String },
    /// A non-fatal or fatal error. Fatal errors are followed by [`LiveEvent::Closed`].
    Error { message: String },
    /// The session has fully closed.
    Closed,
}

/// Lifecycle phase of a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePhase {
    Connecting,
    Connected,
    Closing,
    Closed,
}

/// Which transcript stream an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptKind {
    Input,
    Output,
}

/// Convert a protocol [`LiveServerEvent`] into a public [`LiveEvent`]. Returns
/// `None` for events with no public representation (e.g. `session.*` updates
/// that the session handles internally, or `output_audio.delta` which is media,
/// not a transcript).
pub(super) fn server_event_to_live_event(event: LiveServerEvent) -> Option<LiveEvent> {
    match event {
        LiveServerEvent::TranscriptAdded { kind, text } => Some(LiveEvent::Transcript {
            kind: match kind {
                super::protocol::TranscriptKind::Input => TranscriptKind::Input,
                super::protocol::TranscriptKind::Output => TranscriptKind::Output,
            },
            text,
        }),
        LiveServerEvent::DelegationCreated { id, content } => Some(LiveEvent::Delegation {
            id,
            content: content.into_iter().map(|c| c.text).collect(),
        }),
        LiveServerEvent::TurnDone { role, transcript } => {
            Some(LiveEvent::Turn { role, transcript })
        }
        LiveServerEvent::Error { message } => Some(LiveEvent::Error { message }),
        LiveServerEvent::Session { .. }
        | LiveServerEvent::OutputAudioDelta { .. }
        | LiveServerEvent::Unknown { .. } => None,
    }
}

/// Adapter that turns transport callbacks into events on an mpsc sender.
///
/// Non-level events (transcript, delegation, turn, error) are forwarded to
/// `event_tx` (the pager-facing channel). Output levels are sent to a separate
/// internal `levels_tx` so the session loop can drive the echo gate (muting the
/// mic while the model is speaking) *and* forward the level to the pager.
pub(super) struct CallbackSink {
    pub event_tx: mpsc::Sender<LiveEvent>,
    pub levels_tx: mpsc::Sender<f64>,
}

impl super::transport::LiveTransportCallbacks for CallbackSink {
    fn on_event(&self, event: LiveServerEvent) {
        if let Some(live_event) = server_event_to_live_event(event) {
            // best-effort: if the receiver is gone the session is shutting down.
            let _ = self.event_tx.try_send(live_event);
        }
    }

    fn on_output_level(&self, level: f64) {
        // Drive the echo gate (session loop) and forward to the pager.
        let _ = self.levels_tx.try_send(level);
    }
}
