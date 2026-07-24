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
/// Control events — `Delegation` (`delegation.created`) and `Turn`
/// (`turn.done`) — are delivered **reliably and in order** to `event_tx` (the
/// pager-facing channel). `Transcript` deltas are coalesced/shed under pressure
/// (a missing intermediate delta is recoverable; the final `turn.done` carries
/// the full transcript). Output levels are written to a dedicated
/// `watch::Sender<f64>` (`output_level_tx`) so the session's barge-in gate
/// always reads the **latest** level directly from shared state — never a stale
/// value from a lossy queue — and the latest level is also forwarded to the
/// pager as a lossy `LiveEvent::Levels` for meter UI.
///
/// # Queue headroom (no silent control loss)
/// Levels and transcript deltas are coalesced (drop-newest) once the pager
/// channel's available capacity drops to [`CALLBACK_CONTROL_RESERVE`], so a
/// 20 Hz level/transcript flood can never consume the capacity a delegation or
/// final turn needs. If a control event's `try_send` nevertheless fails (the
/// reserved capacity is genuinely saturated by control events — a stalled
/// pager consumer), the sink publishes one explicit fatal overflow via the
/// non-sheddable `fatal_tx` watch and the session closes — never a silent drop
/// of protocol state.
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
    /// Reserved headroom (in slots) in `event_tx` for control events. Levels
    /// and transcript deltas are coalesced once the channel's available
    /// capacity drops to this many slots. Production uses
    /// [`CALLBACK_CONTROL_RESERVE`]; tests may pass a smaller value.
    control_reserve: usize,
}

/// Reserved headroom in the pager-facing `event_tx` channel for **control**
/// events (`Delegation`, `Turn`). Output levels and `Transcript` deltas are
/// coalesced once the channel's available capacity drops to this many slots,
/// so a level/transcript flood can never consume the capacity a delegation or
/// final turn needs. Sized for the pager's 128-slot channel.
pub(super) const CALLBACK_CONTROL_RESERVE: usize = 32;

impl CallbackSink {
    /// Build a sink from the pager-facing `event_tx`. The caller retains the
    /// `watch::Receiver`s for the session loop. Uses the production
    /// [`CALLBACK_CONTROL_RESERVE`] headroom.
    pub fn new(
        event_tx: mpsc::Sender<LiveEvent>,
        output_level_tx: watch::Sender<f64>,
        fatal_tx: watch::Sender<Option<LiveEvent>>,
    ) -> Self {
        Self::with_reserve(
            event_tx,
            output_level_tx,
            fatal_tx,
            CALLBACK_CONTROL_RESERVE,
        )
    }

    /// Build a sink with an explicit control-event reserve (for tests with
    /// small channels). Production should use [`CallbackSink::new`].
    #[allow(dead_code)]
    pub fn with_reserve(
        event_tx: mpsc::Sender<LiveEvent>,
        output_level_tx: watch::Sender<f64>,
        fatal_tx: watch::Sender<Option<LiveEvent>>,
        control_reserve: usize,
    ) -> Self {
        Self {
            event_tx,
            output_level_tx,
            fatal_tx,
            fatal_reported: Arc::new(AtomicBool::new(false)),
            control_reserve,
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

    /// Deliver a **control** event (`Delegation`, `Turn`) reliably and in
    /// order. Levels and transcript deltas are coalesced before they can fill
    /// [`CALLBACK_CONTROL_RESERVE`] headroom, so this normally succeeds. If
    /// the reserved capacity is genuinely saturated by control events (a
    /// stalled pager consumer) or the receiver is gone, publish one explicit
    /// fatal overflow via the non-sheddable `fatal_tx` watch so the session
    /// closes — never a silent drop of protocol state.
    fn send_control(&self, event: LiveEvent) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.event_tx.try_send(event.clone()) {
            self.report_fatal(LiveEvent::Error {
                message:
                    "Live event queue is saturated with control events; closing to avoid silent protocol loss"
                        .to_owned(),
            });
        }
        // `TrySendError::Closed(_)` is ignored — the session is shutting down
        // and a control event during teardown is not a protocol-correctness
        // violation (the fatal watch, if not already set, is published by the
        // session's own close path).
    }

    /// Forward a **sheddable** event (`Transcript` delta, `Levels`) with
    /// drop-newest coalescing. Reserves `control_reserve` headroom for control
    /// events: once the pager channel's available capacity drops to the
    /// reserve, the event is dropped rather than consuming a control slot. The
    /// reliable state (e.g. the level `watch`) is unaffected.
    fn send_sheddable(&self, event: LiveEvent) {
        // `mpsc::Sender::capacity` returns the number of messages the channel
        // can currently accept. Keep at least `control_reserve` slots free for
        // control events. A reserve >= the channel capacity sheds all
        // sheddable events (used by tests that want only control traffic).
        if self.event_tx.capacity() <= self.control_reserve {
            return;
        }
        let _ = self.event_tx.try_send(event);
    }
}

impl super::transport::LiveTransportCallbacks for CallbackSink {
    fn on_event(&self, event: LiveServerEvent) {
        if let Some(live_event) = server_event_to_live_event(event) {
            match live_event {
                // Critical (Error) events are NOT forwarded to event_tx here —
                // the session emits exactly one awaited Error itself after
                // receiving the fatal watch signal. Forwarding here would
                // risk duplicate or shed errors.
                LiveEvent::Error { .. } => {
                    self.report_fatal(live_event);
                }
                // Control events (Delegation, Turn) are reliable and in order;
                // a saturated control queue becomes one explicit fatal overflow.
                LiveEvent::Delegation { .. } | LiveEvent::Turn { .. } => {
                    self.send_control(live_event);
                }
                // Transcript deltas and levels are coalesced under pressure.
                LiveEvent::Transcript { .. } | LiveEvent::Levels(_) => {
                    self.send_sheddable(live_event);
                }
                // Phase/Closed are emitted by the session itself; a callback
                // never produces them. Defensive: treat as sheddable.
                LiveEvent::Phase(_) | LiveEvent::Closed => {
                    self.send_sheddable(live_event);
                }
            }
        }
    }

    fn on_output_level(&self, level: f64) {
        // Write the latest level to the shared watch state (reliable for the
        // barge-in gate) and forward a lossy, coalesced meter event to the
        // pager. The watch is authoritative; the queued event is best-effort
        // and never consumes control-event headroom.
        let clamped = if level.is_finite() && level >= 0.0 {
            level.min(1.0)
        } else {
            0.0
        };
        let _ = self.output_level_tx.send(clamped);
        self.send_sheddable(LiveEvent::Levels(clamped));
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
        // Use a small reserve so sheddable events (transcript/levels) still
        // flow in these unit tests while control headroom remains protected.
        let sink = CallbackSink::with_reserve(event_tx, output_level_tx, fatal_tx, 4);
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

    /// Build a sink with an explicit channel bound + control reserve, for the
    /// reliability tests below.
    fn make_sink_with(
        bound: usize,
        reserve: usize,
    ) -> (
        CallbackSink,
        mpsc::Receiver<LiveEvent>,
        watch::Receiver<f64>,
        watch::Receiver<Option<LiveEvent>>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(bound);
        let (output_level_tx, output_level_rx) = watch::channel(0.0);
        let (fatal_tx, fatal_rx) = watch::channel(None);
        let sink = CallbackSink::with_reserve(event_tx, output_level_tx, fatal_tx, reserve);
        (sink, event_rx, output_level_rx, fatal_rx)
    }

    /// A delegation (`delegation.created`) arriving after a level flood must
    /// be delivered — the level flood cannot consume the reserved control
    /// headroom. The receiver is not drained during the flood.
    #[tokio::test]
    async fn level_flood_cannot_consume_delegation_slot() {
        // Channel of 16, reserve of 8: levels stop at 8 queued, leaving 8 for
        // control.
        let (sink, mut event_rx, _level_rx, fatal_rx) = make_sink_with(16, 8);
        // Flood levels well past the bound.
        for i in 0..256 {
            sink.on_output_level(0.1 + (i as f64) * 1e-4);
        }
        // Now inject a delegation — it must arrive (reliable control path).
        sink.on_event(LiveServerEvent::DelegationCreated {
            id: "del-1".into(),
            content: vec![super::super::protocol::LiveInputTextContent::input_text(
                "do work",
            )],
        });
        // Drain: the delegation must be present. Levels may have been coalesced.
        let mut saw_delegation = false;
        while let Ok(ev) = event_rx.try_recv() {
            if let LiveEvent::Delegation { id, .. } = ev {
                assert_eq!(id, "del-1");
                saw_delegation = true;
            }
        }
        assert!(saw_delegation, "delegation was shed by a level flood");
        // No fatal should have been published (no control saturation).
        assert!(fatal_rx.borrow().is_none());
    }

    /// A final turn (`turn.done`) arriving after a transcript delta flood
    /// must be delivered reliably; transcript deltas may be coalesced but the
    /// final turn is never silently dropped.
    #[tokio::test]
    async fn final_turn_survives_transcript_flood() {
        let (sink, mut event_rx, _level_rx, fatal_rx) = make_sink_with(16, 8);
        // Flood transcript deltas.
        for i in 0..256 {
            sink.on_event(LiveServerEvent::TranscriptAdded {
                kind: super::super::protocol::TranscriptKind::Output,
                text: format!("delta-{i}"),
            });
        }
        // Inject the final turn.
        sink.on_event(LiveServerEvent::TurnDone {
            role: super::super::protocol::LiveRole::Assistant,
            transcript: "final answer".into(),
        });
        let mut saw_turn = false;
        while let Ok(ev) = event_rx.try_recv() {
            if let LiveEvent::Turn { transcript, .. } = ev {
                assert_eq!(transcript, "final answer");
                saw_turn = true;
            }
        }
        assert!(saw_turn, "final turn was shed by a transcript flood");
        assert!(fatal_rx.borrow().is_none());
    }

    /// When the control-event queue is genuinely saturated by control events
    /// (a stalled pager consumer), a further delegation must publish one
    /// explicit fatal overflow via the non-sheddable `fatal_tx` watch — never
    /// a silent drop. Transcript deltas under the same saturation are shed
    /// silently (coalesced), not fatal.
    #[tokio::test]
    async fn control_saturation_reports_fatal_overflow_not_silent_loss() {
        // Channel of 4, reserve of 2: control fills the 4 slots, then a 5th
        // control event saturates the reserve.
        let (sink, event_rx, _level_rx, fatal_rx) = make_sink_with(4, 2);
        // Fill all 4 slots with control events (no draining).
        for i in 0..4 {
            sink.on_event(LiveServerEvent::TurnDone {
                role: super::super::protocol::LiveRole::User,
                transcript: format!("t{i}"),
            });
        }
        assert_eq!(event_rx.len(), 4);
        // A transcript delta under saturation is coalesced (shed), not fatal.
        sink.on_event(LiveServerEvent::TranscriptAdded {
            kind: super::super::protocol::TranscriptKind::Input,
            text: "shed me".into(),
        });
        assert!(
            fatal_rx.borrow().is_none(),
            "transcript shedding must not be fatal"
        );
        // A fifth control event must trigger the fatal overflow.
        sink.on_event(LiveServerEvent::DelegationCreated {
            id: "del-fatal".into(),
            content: vec![super::super::protocol::LiveInputTextContent::input_text(
                "x",
            )],
        });
        let published = fatal_rx.borrow().clone();
        match published {
            Some(LiveEvent::Error { message }) => {
                assert!(
                    message.contains("saturated with control events"),
                    "unexpected fatal message: {message}"
                );
            }
            other => panic!("expected fatal overflow, got {other:?}"),
        }
    }

    /// Levels and transcripts are coalesced once the reserve is reached: they
    /// never consume the final `reserve` slots, leaving them for control.
    #[tokio::test]
    async fn sheddable_events_stop_at_reserve_boundary() {
        // Channel of 8, reserve of 4: sheddable events stop at 4 queued.
        let (sink, event_rx, _level_rx, _fatal_rx) = make_sink_with(8, 4);
        for i in 0..64 {
            sink.on_output_level(0.3 + (i as f64) * 1e-4);
        }
        assert!(
            event_rx.len() <= 4,
            "level flood queued {} events, expected <= 4 (reserve boundary)",
            event_rx.len()
        );
        // The reliable watch still carries the latest level.
        // (Verified in on_output_level_updates_watch_and_forwards_lossy_event.)
    }

    /// `report_fatal` is once-only: a second control-saturation does not
    /// publish a second fatal (and does not clear the watch).
    #[tokio::test]
    async fn control_saturation_fatal_is_once_only() {
        // Channel of 2: two control events fill it; the third saturates.
        let (sink, _event_rx, _level_rx, fatal_rx) = make_sink_with(2, 1);
        sink.on_event(LiveServerEvent::DelegationCreated {
            id: "d1".into(),
            content: vec![],
        });
        sink.on_event(LiveServerEvent::DelegationCreated {
            id: "d2".into(),
            content: vec![],
        });
        // No fatal yet (channel full but not overflowing).
        assert!(
            fatal_rx.borrow().is_none(),
            "fatal published before saturation"
        );
        // Third control event saturates → fatal.
        sink.on_event(LiveServerEvent::TurnDone {
            role: super::super::protocol::LiveRole::Assistant,
            transcript: "t".into(),
        });
        let first = fatal_rx.borrow().clone();
        assert!(
            matches!(first, Some(LiveEvent::Error { .. })),
            "saturation did not publish a fatal"
        );
        // A fourth control event must not clear/replace the fatal (once-only).
        sink.on_event(LiveServerEvent::TurnDone {
            role: super::super::protocol::LiveRole::User,
            transcript: "t2".into(),
        });
        let second = fatal_rx.borrow().clone();
        assert!(
            matches!(second, Some(LiveEvent::Error { .. })),
            "fatal watch was cleared by a second saturation"
        );
    }
}
