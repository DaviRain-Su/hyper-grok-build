//! Public API contract for the Codex Live subsystem.
//!
//! These types are the surface the pager adapts onto. Internals (transport,
//! media, protocol) may be refined, but these names/shapes stay stable so the
//! pager's `codex-live` feature wiring is a thin adapter.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{mpsc, watch};

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
/// Non-level events (transcript, delegation, turn) are forwarded to
/// `event_tx` (the pager-facing channel) via best-effort `try_send`. Output
/// levels are written to a dedicated `watch::Sender<f64>` (`output_level_tx`)
/// so the session's barge-in gate always reads the **latest** level directly
/// from shared state — never a stale value from a lossy queue. The latest level
/// is also forwarded to the pager as a lossy `LiveEvent::Levels` for meter UI.
///
/// **Critical (fatal) events** are delivered through a dedicated
/// `watch::Sender<Option<LiveEvent>>` (`fatal_tx`) guarded by an
/// `AtomicBool` (`fatal_reported`) so exactly **one** fatal event is ever
/// surfaced — even if the media peer and the sideband fail simultaneously. The
/// session monitors `fatal_rx` in its main `select!` and emits the awaited
/// `LiveEvent::Error` itself (via a blocking-timeout `send`), then `Closing`,
/// then `Closed`. The watch channel is non-sheddable: a single `send_replace`
/// atomically publishes the first fatal payload; subsequent failures are
/// suppressed by the atomic. The callback never emits `LiveEvent::Error` to
/// `event_tx` directly — only the session does, guaranteeing exactly-once
/// terminal delivery.
pub(super) struct CallbackSink {
    pub event_tx: mpsc::Sender<LiveEvent>,
    /// Latest output level, shared via `watch` so the barge-in gate reads
    /// reliable state. The media layer writes here directly (and forces 0.0
    /// on teardown / output-task exits).
    pub output_level_tx: watch::Sender<f64>,
    /// Once-only fatal-event signal. The first failure publishes
    /// `Some(LiveEvent::Error)` here; the session subscribes and acts.
    pub fatal_tx: watch::Sender<Option<LiveEvent>>,
    /// Guards `fatal_tx` so only the first fatal event is published.
    fatal_reported: Arc<AtomicBool>,
}

impl CallbackSink {
    /// Build a sink from the pager-facing `event_tx`. The caller retains the
    /// `watch::Receiver`s for the session loop.
    pub fn new(
        event_tx: mpsc::Sender<LiveEvent>,
        output_level_tx: watch::Sender<f64>,
        fatal_tx: watch::Sender<Option<LiveEvent>>,
    ) -> Self {
        Self {
            event_tx,
            output_level_tx,
            fatal_tx,
            fatal_reported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Publish a fatal event exactly once. Returns `true` if this call won the
    /// race (was the first to report), `false` if a fatal was already reported.
    pub fn report_fatal(&self, event: LiveEvent) -> bool {
        if self.fatal_reported.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.fatal_tx.send_replace(Some(event));
        true
    }

    /// Whether a fatal event has already been reported.
    #[allow(dead_code)]
    pub fn fatal_reported(&self) -> bool {
        self.fatal_reported.load(Ordering::Acquire)
    }
}

impl super::transport::LiveTransportCallbacks for CallbackSink {
    fn on_event(&self, event: LiveServerEvent) {
        if let Some(live_event) = server_event_to_live_event(event.clone()) {
            // Critical (Error) events are NOT forwarded to event_tx here —
            // the session emits exactly one awaited Error itself after
            // receiving the fatal watch signal. Forwarding here would risk
            // duplicate or shed errors.
            if matches!(live_event, LiveEvent::Error { .. }) {
                self.report_fatal(live_event);
                return;
            }
            // Non-critical events (transcript, delegation, turn): best-effort.
            // If the receiver is gone the session is shutting down.
            let _ = self.event_tx.try_send(live_event);
        }
    }

    fn on_output_level(&self, level: f64) {
        // Write the latest level to the shared watch state (reliable for the
        // barge-in gate) and forward a lossy meter event to the pager.
        let clamped = if level.is_finite() && level >= 0.0 {
            level.min(1.0)
        } else {
            0.0
        };
        let _ = self.output_level_tx.send(clamped);
        let _ = self.event_tx.try_send(LiveEvent::Levels(clamped));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::transport::LiveTransportCallbacks;
    use std::sync::atomic::Ordering;

    fn make_sink() -> (
        CallbackSink,
        mpsc::Receiver<LiveEvent>,
        watch::Receiver<f64>,
        watch::Receiver<Option<LiveEvent>>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(16);
        let (output_level_tx, output_level_rx) = watch::channel(0.0);
        let (fatal_tx, fatal_rx) = watch::channel(None);
        let sink = CallbackSink::new(event_tx, output_level_tx, fatal_tx);
        (sink, event_rx, output_level_rx, fatal_rx)
    }

    #[test]
    fn fatal_reported_exactly_once_under_concurrent_callers() {
        let (sink, _event_rx, _level_rx, fatal_rx) = make_sink();
        let arc = Arc::new(sink);
        let mut handles = Vec::new();
        for i in 0..8u32 {
            let sink = Arc::clone(&arc);
            handles.push(std::thread::spawn(move || {
                sink.report_fatal(LiveEvent::Error {
                    message: format!("failure-{i}"),
                })
            }));
        }
        let won: u32 = handles.into_iter().map(|h| h.join().unwrap() as u32).sum();
        // Exactly one caller wins.
        assert_eq!(won, 1);
        // The published fatal is the winner's (some failure-N).
        let published = fatal_rx.borrow().clone();
        assert!(matches!(published, Some(LiveEvent::Error { .. })));
        // A later call loses.
        assert!(!arc.report_fatal(LiveEvent::Error {
            message: "late".into()
        }));
        assert!(arc.fatal_reported());
    }

    #[test]
    fn on_event_routes_error_to_fatal_not_event_tx() {
        let (sink, mut event_rx, _level_rx, fatal_rx) = make_sink();
        sink.on_event(LiveServerEvent::Error {
            message: "boom".into(),
        });
        // The error must NOT appear on event_tx (the session emits it).
        assert!(event_rx.try_recv().is_err());
        // It must appear on the fatal watch.
        let published = fatal_rx.borrow().clone();
        assert!(matches!(
            published,
            Some(LiveEvent::Error { message }) if message == "boom"
        ));
    }

    #[test]
    fn on_event_routes_non_critical_to_event_tx() {
        let (sink, mut event_rx, _level_rx, fatal_rx) = make_sink();
        sink.on_event(LiveServerEvent::TranscriptAdded {
            kind: super::super::protocol::TranscriptKind::Input,
            text: "hi".into(),
        });
        let ev = event_rx.try_recv().unwrap();
        assert!(matches!(ev, LiveEvent::Transcript { .. }));
        // No fatal published.
        assert!(fatal_rx.borrow().is_none());
    }

    #[test]
    fn on_output_level_updates_watch_and_forwards_lossy_event() {
        let (sink, mut event_rx, level_rx, _fatal_rx) = make_sink();
        sink.on_output_level(0.42);
        // The watch carries the latest level (reliable for barge-in).
        assert!((*level_rx.borrow() - 0.42).abs() < 1e-9);
        // A lossy meter event is forwarded to the pager.
        let ev = event_rx.try_recv().unwrap();
        assert!(matches!(ev, LiveEvent::Levels(l) if (l - 0.42).abs() < 1e-9));
    }

    #[test]
    fn on_output_level_clamps_non_finite_and_negative() {
        let (sink, _event_rx, level_rx, _fatal_rx) = make_sink();
        sink.on_output_level(f64::NAN);
        assert_eq!(*level_rx.borrow(), 0.0);
        sink.on_output_level(-5.0);
        assert_eq!(*level_rx.borrow(), 0.0);
        sink.on_output_level(2.0);
        assert!((*level_rx.borrow() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fatal_reported_atomic_is_acqrel_consistent() {
        let (sink, _event_rx, _level_rx, _fatal_rx) = make_sink();
        assert!(!sink.fatal_reported());
        assert!(sink.report_fatal(LiveEvent::Error {
            message: "first".into()
        }));
        assert!(sink.fatal_reported());
        // Re-check with explicit load to verify ordering.
        assert!(sink.fatal_reported.load(Ordering::Acquire));
    }
}
