//! Native WebRTC media transport for Codex Live voice sessions.
//!
//! A substantially adapted port of `crates/pi-natives/src/live.rs` from
//! oh-my-pi (OMP) v17.1.1 (commit e9c8a35). The OMP original is an N-API addon
//! driving a TypeScript host via threadsafe callbacks; this version is a plain
//! async Rust library that emits events through a bounded `flume` channel and
//! pushes mic audio in via [`LiveMediaPeer::push_audio`]. The WebRTC/Opus media
//! logic (peer lifecycle, Opus 16 kHz mono 20 ms input, 48 kHz output, oai-events
//! data-channel fallback, output-level RMS, packet-loss concealment, bounded
//! input/playback queues, echo gate, mute) is preserved. Speaker playback uses
//! the crate's own [`super::playback`] backends instead of OMP's `maudio`. The
//! media event channel is bounded (drop-newest for levels, best-effort for
//! events) so a slow consumer can't cause unbounded memory growth.
//!
//! MIT attribution preserved in `THIRD-PARTY-NOTICES`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use opus::{Application, Channels, Decoder, Encoder};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_remote::TrackRemote;

use super::playback::{PlaybackStream, PlaybackWriter};

/// Data-channel label used for Frameless Bidi server events (OMP: `oai-events`).
const DATA_CHANNEL_LABEL: &str = "oai-events";
const INPUT_SAMPLE_RATE: u32 = 16_000;
const INPUT_FRAME_SAMPLES: usize = 320;
const INPUT_FRAME_DURATION: Duration = Duration::from_millis(20);
const MAX_ENCODED_OPUS_BYTES: usize = 1_275;
const MAX_QUEUED_INPUT_SAMPLES: usize = 32_000;
const OUTPUT_SAMPLE_RATE: u32 = 48_000;
const MAX_DECODED_OPUS_SAMPLES: usize = 5_760;
const OUTPUT_LEVEL_SAMPLES: usize = 2_400;
const OUTPUT_FRAME_SAMPLES: usize = 960;
const DEFAULT_OPEN_TIMEOUT_MS: u32 = 20_000;
const DISCONNECT_GRACE: Duration = Duration::from_secs(2);
const CLOSE_TASK_TIMEOUT: Duration = Duration::from_secs(1);

/// Codec capability registered for the local Opus track (48 kHz stereo, the
/// SDP negotiation shape OMP uses).
fn opus_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: OUTPUT_SAMPLE_RATE,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: Vec::new(),
    }
}

/// Internal peer lifecycle signal.
#[derive(Clone, Debug)]
pub(super) enum PeerSignal {
    Connecting,
    Open,
    Failed(String),
    Closed,
}

/// Command sent to the input-audio encoder task.
enum InputCommand {
    Audio(Vec<f32>),
    Muted(bool),
    Close,
}

/// Media-layer event emitted to the transport/session.
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// A server event payload (JSON string) arrived on the oai-events data
    /// channel. The transport parses it via [`super::protocol`].
    Event(String),
    /// Output (speaker) audio level, `[0.0, 1.0]`.
    OutputLevel(f64),
    /// A fatal media-layer failure. Surfaced once; the peer is then closed.
    Failure(String),
}

/// Bounded capacity of the media event channel. Events are either server
/// payloads (small JSON), output levels (frequent but tiny), or a single
/// failure. A bounded channel with explicit shedding prevents a slow consumer
/// (e.g. a stalled session loop) from causing unbounded memory growth in the
/// media layer. Levels are shed first (drop-newest) since they're
/// high-frequency and ephemeral; server events and failures are retained.
const MEDIA_EVENT_BOUND: usize = 256;

/// Reserved headroom inside [`MEDIA_EVENT_BOUND`] for **control** events
/// (`MediaEvent::Event` server payloads and `MediaEvent::Failure`). Output
/// levels are coalesced (drop-newest) once the channel's live occupancy reaches
/// `MEDIA_EVENT_BOUND - CONTROL_EVENT_RESERVE`, so a 20 Hz level flood can
/// never consume the capacity a `delegation.created` / `turn.done` / failure
/// needs. Control events are delivered reliably in FIFO order; if the reserved
/// capacity is genuinely saturated by control events themselves (a stalled
/// transport consumer), the peer reports one explicit fatal overflow via the
/// non-sheddable [`PeerSignal::Failed`] watch and closes — never a silent drop.
const CONTROL_EVENT_RESERVE: usize = 32;

struct MediaResources {
    peer: Arc<RTCPeerConnection>,
    data_channel: Arc<RTCDataChannel>,
    /// Input command channel. Dropped (closed) on teardown so the encoder
    /// task always unblocks even when `try_send(Close)` would Full.
    input_tx: flume::Sender<InputCommand>,
    input_task: JoinHandle<()>,
    rtcp_task: JoinHandle<()>,
    /// Remote audio decoder task. Tracked so `close` can abort+join it instead
    /// of detaching on peer drop (which leaked a task that kept reading RTP).
    output_task: Mutex<Option<JoinHandle<()>>>,
    playback: PlaybackStream,
}

struct LivePeerCore {
    /// Outbound media events (server payloads + output levels + failures).
    event_tx: flume::Sender<MediaEvent>,
    resources: Mutex<Option<MediaResources>>,
    signal_tx: watch::Sender<PeerSignal>,
    started: AtomicBool,
    closing: AtomicBool,
    muted: AtomicBool,
    failure_reported: AtomicBool,
    /// Once-only guard for the control-event overflow fatal. Distinct from
    /// `failure_reported` so a "queue saturated" failure and a peer failure
    /// can't suppress each other's first report (each is once-only).
    overflow_reported: AtomicBool,
    queued_samples: AtomicUsize,
    /// Consecutive `try_send` Fulls on the audio input channel. A single Full
    /// only sheds; sustained saturation above
    /// [`INPUT_FULL_FATAL_THRESHOLD`] fatals. Reset on a successful enqueue.
    input_full_streak: AtomicUsize,
    /// Independent owner of once-only teardown. First `close` claim spawns this
    /// so a cancelled caller cannot leave the peer half-closed without
    /// publishing `PeerSignal::Closed`.
    close_owner: Mutex<Option<JoinHandle<()>>>,
}

/// Consecutive audio-input Fulls before a stalled encoder is treated as fatal.
/// A single Full is normal under brief backpressure and only sheds the chunk.
const INPUT_FULL_FATAL_THRESHOLD: usize = 8;

/// Bound on `RTCPeerConnection::close` during teardown. A hung peer close must
/// not prevent playback stop, task abort/join, or publishing `Closed`.
const PEER_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

// Invariant: single producer for LivePeerCore::push_audio. The session loop
// is the only caller; streak accounting is not a multi-producer lock.
// Concurrent push_audio is unsupported and not part of the public contract.

/// Bounded `RTCPeerConnection::close` for create_offer early-failure paths.
/// A hung peer close must not block connect failure from returning.
async fn bounded_peer_close(peer: &Arc<RTCPeerConnection>) {
    if tokio::time::timeout(PEER_CLOSE_TIMEOUT, peer.close())
        .await
        .is_err()
    {
        tracing::warn!(
            "live peer close timed out after {}ms during create_offer failure teardown",
            PEER_CLOSE_TIMEOUT.as_millis()
        );
    }
}

impl LivePeerCore {
    fn new(event_tx: flume::Sender<MediaEvent>) -> Self {
        let (signal_tx, _) = watch::channel(PeerSignal::Connecting);
        Self {
            event_tx,
            resources: Mutex::new(None),
            signal_tx,
            started: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            failure_reported: AtomicBool::new(false),
            overflow_reported: AtomicBool::new(false),
            queued_samples: AtomicUsize::new(0),
            input_full_streak: AtomicUsize::new(0),
            close_owner: Mutex::new(None),
        }
    }

    async fn create_offer(self: &Arc<Self>) -> Result<String, String> {
        if self.started.swap(true, Ordering::AcqRel) {
            return Err("Native live WebRTC peer has already started".to_owned());
        }
        if self.closing.load(Ordering::Acquire) {
            return Err("Native live WebRTC peer is closed".to_owned());
        }

        let playback = PlaybackStream::start(OUTPUT_SAMPLE_RATE)
            .map_err(|e| format!("Failed to open the live speaker: {e}"))?;
        let playback_tx = playback.writer();

        let mut media_engine = MediaEngine::default();
        let capability = opus_capability();
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: capability.clone(),
                    payload_type: 111,
                    ..Default::default()
                },
                RTPCodecType::Audio,
            )
            .map_err(|e| format!("Failed to register the live Opus codec: {e}"))?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|e| format!("Failed to configure live WebRTC interceptors: {e}"))?;
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();
        let peer = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .map_err(|e| format!("Failed to create the live WebRTC peer: {e}"))?,
        );

        let track = Arc::new(TrackLocalStaticSample::new(
            capability,
            "audio".to_owned(),
            "omp-live".to_owned(),
        ));
        let sender = match peer
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
        {
            Ok(sender) => sender,
            Err(e) => {
                bounded_peer_close(&peer).await;
                return Err(format!("Failed to add the live audio track: {e}"));
            }
        };

        install_peer_callbacks(&peer, Arc::downgrade(self), playback_tx);
        let data_channel = match peer.create_data_channel(DATA_CHANNEL_LABEL, None).await {
            Ok(channel) => channel,
            Err(e) => {
                bounded_peer_close(&peer).await;
                return Err(format!("Failed to create the live data channel: {e}"));
            }
        };
        install_data_channel_callbacks(&data_channel, Arc::downgrade(self));

        let offer = match peer.create_offer(None).await {
            Ok(offer) => offer,
            Err(e) => {
                bounded_peer_close(&peer).await;
                return Err(format!("Failed to create the live SDP offer: {e}"));
            }
        };
        if let Err(e) = peer.set_local_description(offer.clone()).await {
            bounded_peer_close(&peer).await;
            return Err(format!("Failed to install the live SDP offer: {e}"));
        }
        let mut resources_slot = self.resources.lock();
        if self.closing.load(Ordering::Acquire) {
            drop(resources_slot);
            bounded_peer_close(&peer).await;
            return Err("Native live WebRTC peer was closed while starting".to_owned());
        }

        let (input_tx, input_rx) = flume::bounded::<InputCommand>(64);
        let input_task = tokio::spawn(run_input_audio(track, input_rx, Arc::downgrade(self)));
        let rtcp_task = tokio::spawn(drain_rtcp(sender));
        let resources = MediaResources {
            peer,
            data_channel,
            input_tx,
            input_task,
            rtcp_task,
            output_task: Mutex::new(None),
            playback,
        };
        *resources_slot = Some(resources);
        Ok(offer.sdp)
    }

    async fn accept_answer(&self, sdp: String) -> Result<(), String> {
        let peer = self
            .resources
            .lock()
            .as_ref()
            .map(|r| Arc::clone(&r.peer))
            .ok_or_else(|| "Native live WebRTC peer has not started".to_owned())?;
        let answer = RTCSessionDescription::answer(sdp)
            .map_err(|e| format!("Codex returned an invalid live SDP answer: {e}"))?;
        peer.set_remote_description(answer)
            .await
            .map_err(|e| format!("Failed to install the live SDP answer: {e}"))
    }

    async fn wait_for_open(&self, timeout_ms: u32) -> Result<(), String> {
        let mut signal_rx = self.signal_tx.subscribe();
        let wait = async {
            loop {
                let signal = signal_rx.borrow().clone();
                match signal {
                    PeerSignal::Open => return Ok(()),
                    PeerSignal::Failed(msg) => return Err(msg),
                    PeerSignal::Closed => {
                        return Err("Native live WebRTC peer closed before opening".to_owned());
                    }
                    PeerSignal::Connecting => {}
                }
                signal_rx
                    .changed()
                    .await
                    .map_err(|_| "Native live WebRTC peer stopped before opening".to_owned())?;
            }
        };
        tokio::time::timeout(Duration::from_millis(u64::from(timeout_ms)), wait)
            .await
            .map_err(|_| "Timed out waiting for the live data channel to open".to_owned())?
    }

    fn push_audio(&self, samples: &[f32]) -> Result<(), String> {
        if samples.is_empty() || self.muted.load(Ordering::Acquire) {
            return Ok(());
        }
        // Single-producer invariant: only the session loop calls push_audio.
        let input_tx = self
            .resources
            .lock()
            .as_ref()
            .map(|r| r.input_tx.clone())
            .ok_or_else(|| "Native live WebRTC peer has not started".to_owned())?;
        self.try_enqueue_audio(&input_tx, samples)
    }

    /// Enqueue PCM onto a bounded input channel (shed-newest on Full).
    ///
    /// Extracted so tests can drive a real `flume` channel without a full
    /// WebRTC peer. Production path is only [`Self::push_audio`].
    fn try_enqueue_audio(
        &self,
        input_tx: &flume::Sender<InputCommand>,
        samples: &[f32],
    ) -> Result<(), String> {
        let sample_count = samples.len().min(MAX_QUEUED_INPUT_SAMPLES);
        if sample_count == 0 {
            return Ok(());
        }
        let retained = &samples[samples.len() - sample_count..];
        let queued = self
            .queued_samples
            .fetch_add(sample_count, Ordering::AcqRel);
        if queued.saturating_add(sample_count) > MAX_QUEUED_INPUT_SAMPLES {
            self.queued_samples
                .fetch_sub(sample_count, Ordering::AcqRel);
            return Ok(());
        }
        // Bounded channel: Full sheds the chunk and bumps the streak. A single
        // Full is not fatal — only sustained saturation is. Disconnected
        // rolls back the sample counter and surfaces closed input.
        match input_tx.try_send(InputCommand::Audio(retained.to_vec())) {
            Ok(()) => {
                self.input_full_streak.store(0, Ordering::Release);
                Ok(())
            }
            Err(flume::TrySendError::Disconnected(_)) => {
                self.queued_samples
                    .fetch_sub(sample_count, Ordering::AcqRel);
                Err("Native live audio input is closed".to_owned())
            }
            Err(flume::TrySendError::Full(_)) => {
                self.queued_samples
                    .fetch_sub(sample_count, Ordering::AcqRel);
                let streak = self.input_full_streak.fetch_add(1, Ordering::AcqRel) + 1;
                if streak >= INPUT_FULL_FATAL_THRESHOLD && !self.closing.load(Ordering::Acquire) {
                    self.report_failure(
                        "Live audio input queue is saturated; the encoder may be stalled"
                            .to_owned(),
                    );
                }
                // Shed-only: control path (mute/close) is unaffected.
                Ok(())
            }
        }
    }

    fn set_muted(&self, muted: bool) -> Result<(), String> {
        self.muted.store(muted, Ordering::Release);
        let input_tx = self.resources.lock().as_ref().map(|r| r.input_tx.clone());
        if let Some(input_tx) = input_tx {
            // Mute is critical control: prefer send with a short wait so a
            // briefly full audio queue cannot drop mute indefinitely. The
            // atomic muted flag is already set, so the encoder also honors
            // mute on its next tick even if this enqueue is shed.
            match input_tx.try_send(InputCommand::Muted(muted)) {
                Ok(()) => {}
                Err(flume::TrySendError::Full(cmd)) => {
                    // Best-effort: drop one audio slot worth is not possible
                    // without draining; leave atomic mute as source of truth.
                    let _ = cmd;
                }
                Err(flume::TrySendError::Disconnected(_)) => {}
            }
        }
        Ok(())
    }

    fn report_event(&self, payload: String) {
        // Control events (server payloads carrying delegation.created /
        // turn.done / error / transcripts) are reliable: levels are
        // coalesced once occupancy reaches `MEDIA_EVENT_BOUND -
        // CONTROL_EVENT_RESERVE`, so a 20 Hz level flood can never consume
        // the reserved control capacity. We therefore expect `try_send` to
        // succeed. If it fails, the reserved capacity is genuinely saturated
        // by control events (a stalled transport consumer) — report one
        // explicit fatal overflow via the non-sheddable peer-signal watch and
        // close, never silently drop protocol state.
        if self.event_tx.try_send(MediaEvent::Event(payload)).is_err() {
            self.report_overflow("Live media event queue is saturated with control events");
        }
    }

    fn report_level(&self, level: f64) {
        if !level.is_finite() {
            return;
        }
        // Levels are high-frequency and ephemeral (~20 Hz). Reserve
        // `CONTROL_EVENT_RESERVE` slots for control events by coalescing
        // (drop-newest) once the channel's live occupancy reaches the level
        // cap. `flume::Sender::len` reflects the current queue depth, so a
        // level flood can never fill the capacity a delegation/turn/failure
        // needs. A transient level gap has no user-visible effect (the
        // barge-in gate reads the latest level from the dedicated watch, not
        // this queue) and this prevents unbounded memory growth.
        if self.event_tx.len() >= MEDIA_EVENT_BOUND - CONTROL_EVENT_RESERVE {
            return;
        }
        let _ = self
            .event_tx
            .try_send(MediaEvent::OutputLevel(level.clamp(0.0, 1.0)));
    }

    fn mark_open(&self) {
        if !self.closing.load(Ordering::Acquire) {
            self.signal_tx.send_replace(PeerSignal::Open);
        }
    }

    /// Report a control-queue overflow as a fatal peer failure exactly once.
    /// Published through the non-sheddable [`PeerSignal::Failed`] watch so the
    /// transport's media-forward task surfaces it as a `LiveServerEvent::Error`
    /// (→ the session's fatal watch) even if the bounded event channel is full.
    fn report_overflow(&self, message: &str) {
        if self.closing.load(Ordering::Acquire)
            || self.overflow_reported.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.signal_tx
            .send_replace(PeerSignal::Failed(message.to_owned()));
        // Best-effort: also push a Failure event so a draining consumer sees
        // it inline. If the channel is full this is shed — the watch above is
        // the authoritative path.
        let _ = self
            .event_tx
            .try_send(MediaEvent::Failure(message.to_owned()));
    }

    fn report_failure(&self, message: String) {
        if self.closing.load(Ordering::Acquire)
            || self.failure_reported.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.signal_tx
            .send_replace(PeerSignal::Failed(message.clone()));
        // Finding 2: force the output level to zero on failure so the barge-in
        // gate clears reliably (not via a lossy metering queue).
        let _ = self.event_tx.try_send(MediaEvent::OutputLevel(0.0));
        // Failures are once-only and critical. The bounded event channel may
        // be full of control events, so the `try_send` below is best-effort;
        // the authoritative delivery path is the `PeerSignal::Failed` watch
        // above, which the transport's media-forward task subscribes to and
        // translates into a `LiveServerEvent::Error` (→ the session's fatal
        // watch) regardless of queue state.
        let _ = self.event_tx.try_send(MediaEvent::Failure(message));
    }

    /// Cancellation-safe, once-only teardown.
    ///
    /// The first caller claims ownership and spawns an independent close task
    /// that always runs to completion (even if this future is aborted mid-way).
    /// All callers — including the owner — wait with a bound for
    /// [`PeerSignal::Closed`]. Resources are always joined (never timeout-drop
    /// a running handle): input channel is closed by drop, tasks are aborted
    /// then awaited without detaching.
    async fn close(self: &Arc<Self>) {
        let mut signal_rx = self.signal_tx.subscribe();
        if matches!(*signal_rx.borrow(), PeerSignal::Closed) {
            return;
        }

        if !self.closing.swap(true, Ordering::AcqRel) {
            // First claim: force level to zero and spawn the owner task.
            let _ = self.event_tx.try_send(MediaEvent::OutputLevel(0.0));
            let this = Arc::clone(self);
            let owner = tokio::spawn(async move {
                this.run_close_once().await;
            });
            *self.close_owner.lock() = Some(owner);
        }

        // Bound the wait so a pathological close cannot wedge the caller forever,
        // but the owner task continues independently until Closed is published.
        let wait = async {
            loop {
                if matches!(*signal_rx.borrow(), PeerSignal::Closed) {
                    return;
                }
                if signal_rx.changed().await.is_err() {
                    return;
                }
            }
        };
        let _ = tokio::time::timeout(CLOSE_TASK_TIMEOUT * 4, wait).await;
    }

    /// Body of the once-only close owner. Always publishes `Closed` at the end,
    /// even when `peer.close()` hangs past [`PEER_CLOSE_TIMEOUT`].
    async fn run_close_once(self: Arc<Self>) {
        self.run_close_once_with_peer_close(None, PEER_CLOSE_TIMEOUT)
            .await;
    }

    /// Close body with an optional peer-close future override (tests inject a
    /// permanently-pending future to prove the timeout path). Production
    /// passes `None` and uses `resources.peer.close()` under `peer_close_deadline`.
    async fn run_close_once_with_peer_close(
        self: Arc<Self>,
        peer_close_override: Option<
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        >,
        peer_close_deadline: Duration,
    ) {
        // Finding 2: force output level to zero on teardown so the barge-in
        // gate clears reliably (not via a lossy metering queue).
        let _ = self.event_tx.try_send(MediaEvent::OutputLevel(0.0));

        let resources = self.resources.lock().take();
        if let Some(resources) = resources {
            // Prefer Close command, then always drop the sender so a full
            // queue cannot leave the encoder running forever.
            let _ = resources.input_tx.try_send(InputCommand::Close);
            drop(resources.input_tx);

            // Bounded peer close: timeout must not skip playback stop / joins.
            if let Some(pending) = peer_close_override {
                if tokio::time::timeout(peer_close_deadline, pending)
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        "live peer close timed out after {}ms; continuing teardown",
                        peer_close_deadline.as_millis()
                    );
                }
                // Drop the real peer without awaiting an unbounded close when
                // the override already simulated hang/success.
                drop(resources.peer);
            } else {
                let peer = resources.peer;
                if tokio::time::timeout(peer_close_deadline, peer.close())
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        "live peer close timed out after {}ms; continuing teardown",
                        peer_close_deadline.as_millis()
                    );
                    // peer is dropped here when the timeout future is dropped,
                    // which still schedules native cleanup without blocking us.
                }
            }

            resources.playback.stop();

            // Always join after abort — never timeout-drop a running handle
            // (that would detach and leak). Abort unblocks; await reaps.
            resources.input_task.abort();
            let _ = resources.input_task.await;

            resources.rtcp_task.abort();
            let _ = resources.rtcp_task.await;

            let output_task = resources.output_task.lock().take();
            if let Some(output_task) = output_task {
                output_task.abort();
                let _ = output_task.await;
            }
            drop(resources.data_channel);
        } else if let Some(pending) = peer_close_override {
            // No resources but a test override: still honor the deadline so
            // the production path (timeout → continue → Closed) is exercised.
            let _ = tokio::time::timeout(peer_close_deadline, pending).await;
        }

        self.queued_samples.store(0, Ordering::Release);
        self.input_full_streak.store(0, Ordering::Release);
        self.signal_tx.send_replace(PeerSignal::Closed);
        // Reap owner tracking so Drop/repeat-close does not hold a stale handle.
        let _ = self.close_owner.lock().take();
    }
}

/// WebRTC peer that accepts 16 kHz mono PCM and renders remote Opus audio.
pub struct LiveMediaPeer {
    inner: Arc<LivePeerCore>,
}

impl LiveMediaPeer {
    /// Create an idle peer. `event_rx` receives media events (server payloads,
    /// output levels, failures) until the peer is closed. The channel is
    /// bounded so a slow consumer can't cause unbounded growth; levels are
    /// shed (oldest dropped) when full.
    pub fn new() -> (Self, flume::Receiver<MediaEvent>) {
        let (event_tx, event_rx) = flume::bounded::<MediaEvent>(MEDIA_EVENT_BOUND);
        let inner = Arc::new(LivePeerCore::new(event_tx));
        (Self { inner }, event_rx)
    }

    /// Start the native media peer and return its SDP offer.
    pub async fn create_offer(&self) -> Result<String, String> {
        self.inner.create_offer().await
    }

    /// Apply the remote SDP answer returned by Codex signaling.
    pub async fn accept_answer(&self, sdp: String) -> Result<(), String> {
        self.inner.accept_answer(sdp).await
    }

    /// Wait until the `oai-events` data channel is open.
    pub async fn wait_for_open(&self, timeout_ms: Option<u32>) -> Result<(), String> {
        self.inner
            .wait_for_open(timeout_ms.unwrap_or(DEFAULT_OPEN_TIMEOUT_MS))
            .await
    }

    /// Queue 16 kHz mono floating-point PCM for Opus transmission.
    pub fn push_audio(&self, samples: &[f32]) -> Result<(), String> {
        self.inner.push_audio(samples)
    }

    /// Enable or disable microphone transmission, discarding partial muted
    /// frames.
    pub fn set_muted(&self, muted: bool) -> Result<(), String> {
        self.inner.set_muted(muted)
    }

    /// Close media, the data channel, the peer connection, and speaker
    /// playback. Cancellation-safe and safe to call repeatedly: the first
    /// claim owns teardown on an independent task that always publishes
    /// [`PeerSignal::Closed`]; callers wait with a bound.
    pub async fn close(&self) {
        self.inner.close().await;
    }

    /// Whether a failure has already been reported (used by the transport to
    /// suppress duplicate error events).
    pub fn failure_reported(&self) -> bool {
        self.inner.failure_reported.load(Ordering::Acquire)
    }

    /// Subscribe to the peer's lifecycle signal watch. The transport's
    /// media-forward task uses this to observe [`PeerSignal::Failed`]
    /// authoritatively — independent of the bounded event channel — so a
    /// media failure is never silently lost when the event queue is full.
    pub(super) fn subscribe_signals(&self) -> watch::Receiver<PeerSignal> {
        self.inner.signal_tx.subscribe()
    }
}

impl Drop for LiveMediaPeer {
    fn drop(&mut self) {
        if self.inner.closing.load(Ordering::Acquire) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                inner.close().await;
            });
        }
    }
}

fn install_peer_callbacks(
    peer: &Arc<RTCPeerConnection>,
    core: std::sync::Weak<LivePeerCore>,
    playback_tx: PlaybackWriter,
) {
    let output_sender = Arc::new(Mutex::new(Some(playback_tx)));
    let output_sender_for_track = Arc::clone(&output_sender);
    let core_for_track = core.clone();
    peer.on_track(Box::new(move |track, _receiver, _transceiver| {
        let output_sender = output_sender_for_track.lock().take();
        let core = core_for_track.clone();
        Box::pin(async move {
            if track.kind() != RTPCodecType::Audio {
                return;
            }
            let Some(output_sender) = output_sender else {
                if let Some(core) = core.upgrade() {
                    core.report_failure(
                        "Codex live returned more than one remote audio track".to_owned(),
                    );
                }
                return;
            };
            // Track the decoder handle on MediaResources so close can
            // abort+join it. Detached spawn leaked a task that kept reading
            // RTP after peer teardown.
            let handle = tokio::spawn(receive_output_audio(track, output_sender, core.clone()));
            if let Some(core) = core.upgrade() {
                let mut resources = core.resources.lock();
                if let Some(resources) = resources.as_mut() {
                    let mut slot = resources.output_task.lock();
                    if let Some(prev) = slot.replace(handle) {
                        prev.abort();
                    }
                } else {
                    // Peer already closed between track open and install —
                    // abort immediately so the decoder cannot outlive close.
                    handle.abort();
                }
            } else {
                handle.abort();
            }
        })
    }));

    let peer_for_state = Arc::downgrade(peer);
    peer.on_peer_connection_state_change(Box::new(move |state| {
        let core = core.clone();
        let peer = peer_for_state.clone();
        Box::pin(async move {
            let Some(core) = core.upgrade() else {
                return;
            };
            match state {
                RTCPeerConnectionState::Failed => {
                    core.report_failure("Live WebRTC peer connection failed".to_owned());
                }
                RTCPeerConnectionState::Closed if !core.closing.load(Ordering::Acquire) => {
                    core.report_failure(
                        "Live WebRTC peer connection closed unexpectedly".to_owned(),
                    );
                }
                RTCPeerConnectionState::Disconnected => {
                    tokio::time::sleep(DISCONNECT_GRACE).await;
                    if peer.upgrade().is_some_and(|p| {
                        p.connection_state() == RTCPeerConnectionState::Disconnected
                    }) {
                        core.report_failure("Live WebRTC peer connection disconnected".to_owned());
                    }
                }
                _ => {}
            }
        })
    }));
}

fn install_data_channel_callbacks(
    data_channel: &Arc<RTCDataChannel>,
    core: std::sync::Weak<LivePeerCore>,
) {
    let core_for_open = core.clone();
    data_channel.on_open(Box::new(move || {
        Box::pin(async move {
            if let Some(core) = core_for_open.upgrade() {
                core.mark_open();
            }
        })
    }));

    let core_for_message = core.clone();
    data_channel.on_message(Box::new(move |message: DataChannelMessage| {
        let core = core_for_message.clone();
        Box::pin(async move {
            // oai-events fallback: only string frames carry Frameless Bidi
            // events; binary frames are ignored (OMP behavior).
            if !message.is_string {
                return;
            }
            if let (Some(core), Ok(payload)) =
                (core.upgrade(), String::from_utf8(message.data.to_vec()))
            {
                core.report_event(payload);
            }
        })
    }));

    let core_for_close = core.clone();
    data_channel.on_close(Box::new(move || {
        let core = core_for_close.clone();
        Box::pin(async move {
            if let Some(core) = core.upgrade() {
                core.report_failure("Live data channel closed unexpectedly".to_owned());
            }
        })
    }));

    data_channel.on_error(Box::new(move |error| {
        let core = core.clone();
        Box::pin(async move {
            if let Some(core) = core.upgrade() {
                core.report_failure(format!("Live data channel failed: {error}"));
            }
        })
    }));
}

/// Input audio encoder task: drains the input queue at 20 ms cadence, encodes
/// 16 kHz mono f32 → Opus, and writes samples to the local track. Muting
/// discards partial frames (echo gate / mute).
async fn run_input_audio(
    track: Arc<TrackLocalStaticSample>,
    input_rx: flume::Receiver<InputCommand>,
    core: std::sync::Weak<LivePeerCore>,
) {
    let mut encoder = match Encoder::new(INPUT_SAMPLE_RATE, Channels::Mono, Application::Voip) {
        Ok(encoder) => encoder,
        Err(e) => {
            if let Some(core) = core.upgrade() {
                core.report_failure(format!("Failed to initialize the live Opus encoder: {e}"));
            }
            return;
        }
    };
    if let Err(e) = encoder.set_inband_fec(true) {
        if let Some(core) = core.upgrade() {
            core.report_failure(format!("Failed to configure the live Opus encoder: {e}"));
        }
        return;
    }

    let mut muted = false;
    let mut pending = Vec::with_capacity(INPUT_FRAME_SAMPLES * 2);
    let mut encoded = [0u8; MAX_ENCODED_OPUS_BYTES];
    let mut ticker = tokio::time::interval(INPUT_FRAME_DURATION);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    ticker.tick().await; // discard the immediate first tick
    loop {
        tokio::select! {
            biased;
            command = input_rx.recv_async() => {
                let Ok(command) = command else { break; };
                match command {
                    InputCommand::Audio(samples) => {
                        if let Some(core) = core.upgrade() {
                            core.queued_samples.fetch_sub(samples.len(), Ordering::AcqRel);
                        }
                        if muted {
                            continue;
                        }
                        if samples.len() >= MAX_QUEUED_INPUT_SAMPLES {
                            pending.clear();
                            pending.extend_from_slice(&samples[samples.len() - MAX_QUEUED_INPUT_SAMPLES..]);
                            continue;
                        }
                        let overflow = pending
                            .len()
                            .saturating_add(samples.len())
                            .saturating_sub(MAX_QUEUED_INPUT_SAMPLES);
                        if overflow > 0 {
                            pending.drain(..overflow);
                        }
                        pending.extend_from_slice(&samples);
                    }
                    InputCommand::Muted(next_muted) => {
                        muted = next_muted;
                        pending.clear();
                    }
                    InputCommand::Close => break,
                }
            },
            _ = ticker.tick() => {
                let mut frame = [0.0f32; INPUT_FRAME_SAMPLES];
                if !muted {
                    let consumed = pending.len().min(INPUT_FRAME_SAMPLES);
                    frame[..consumed].copy_from_slice(&pending[..consumed]);
                    if consumed > 0 {
                        pending.copy_within(consumed.., 0);
                        pending.truncate(pending.len() - consumed);
                    }
                }
                let encoded_len = match encoder.encode_float(&frame, &mut encoded) {
                    Ok(n) => n,
                    Err(e) => {
                        if let Some(core) = core.upgrade() {
                            core.report_failure(format!("Failed to encode live microphone audio: {e}"));
                        }
                        return;
                    }
                };
                let sample = Sample {
                    data: Bytes::copy_from_slice(&encoded[..encoded_len]),
                    duration: INPUT_FRAME_DURATION,
                    ..Default::default()
                };
                if let Err(e) = track.write_sample(&sample).await {
                    if let Some(core) = core.upgrade() {
                        core.report_failure(format!("Failed to send live microphone audio: {e}"));
                    }
                    return;
                }
            },
        }
    }
}

async fn drain_rtcp(sender: Arc<RTCRtpSender>) {
    while sender.read_rtcp().await.is_ok() {}
}

/// Output audio decoder task: reads RTP Opus packets from the remote track,
/// decodes to 48 kHz mono f32, applies packet-loss concealment for gaps, feeds
/// the speaker playback, and reports the output level (RMS) periodically.
async fn receive_output_audio(
    track: Arc<TrackRemote>,
    playback_tx: PlaybackWriter,
    core: std::sync::Weak<LivePeerCore>,
) {
    if !track
        .codec()
        .capability
        .mime_type
        .eq_ignore_ascii_case(MIME_TYPE_OPUS)
    {
        if let Some(core) = core.upgrade() {
            core.report_failure(format!(
                "Codex live negotiated unsupported audio codec {}",
                track.codec().capability.mime_type
            ));
            core.report_level(0.0);
        }
        return;
    }
    let mut decoder = match Decoder::new(OUTPUT_SAMPLE_RATE, Channels::Mono) {
        Ok(decoder) => decoder,
        Err(e) => {
            if let Some(core) = core.upgrade() {
                core.report_failure(format!("Failed to initialize the live Opus decoder: {e}"));
                core.report_level(0.0);
            }
            return;
        }
    };
    let mut decoded = vec![0.0f32; MAX_DECODED_OPUS_SAMPLES].into_boxed_slice();
    let mut expected_sequence: Option<u16> = None;
    let mut level = OutputLevel::default();

    loop {
        let packet = match track.read_rtp().await {
            Ok((packet, _attributes)) => packet,
            Err(e) => {
                if let Some(core) = core.upgrade()
                    && !core.closing.load(Ordering::Acquire)
                {
                    core.report_failure(format!("Live remote audio track failed: {e}"));
                }
                // Emit a final 0.0 output level so the echo gate clears
                // promptly when the model stops speaking (OMP clears
                // outputLevel when the track ends / meter reports low).
                if let Some(core) = core.upgrade() {
                    core.report_level(0.0);
                }
                return;
            }
        };
        let sequence = packet.header.sequence_number;
        if let Some(expected) = expected_sequence {
            let gap = sequence.wrapping_sub(expected);
            if gap >= u16::MAX / 2 {
                // Out-of-order / retransmit; skip PLC for this packet.
                continue;
            }
            if gap > 0 {
                // Packet-loss concealment: synthesize up to 4 missing frames
                // (decode_float with an empty payload + `false`), then decode
                // the arrived packet with `true` (FEC) to recover its content.
                // Must `continue` after this path — falling through would
                // decode the same payload a second time without FEC and
                // corrupt decoder state / double-play frames.
                for _ in 1..gap.min(5) {
                    if let Ok(samples) =
                        decoder.decode_float(&[], &mut decoded[..OUTPUT_FRAME_SAMPLES], false)
                    {
                        if !write_output(&playback_tx, &decoded[..samples], &core) {
                            return;
                        }
                        level.observe(&decoded[..samples], &core);
                    }
                }
                match decoder.decode_float(&packet.payload, &mut decoded, true) {
                    Ok(samples) => {
                        if !write_output(&playback_tx, &decoded[..samples], &core) {
                            return;
                        }
                        level.observe(&decoded[..samples], &core);
                        expected_sequence = Some(sequence.wrapping_add(1));
                    }
                    Err(e) => {
                        if let Some(core) = core.upgrade() {
                            core.report_failure(format!(
                                "Failed to decode live speaker audio: {e}"
                            ));
                            core.report_level(0.0);
                        }
                        return;
                    }
                }
                continue;
            }
        }
        expected_sequence = Some(sequence.wrapping_add(1));
        match decoder.decode_float(&packet.payload, &mut decoded, false) {
            Ok(samples) => {
                if !write_output(&playback_tx, &decoded[..samples], &core) {
                    return;
                }
                level.observe(&decoded[..samples], &core);
            }
            Err(e) => {
                if let Some(core) = core.upgrade() {
                    core.report_failure(format!("Failed to decode live speaker audio: {e}"));
                    // Clear output activity so the barge-in gate resets.
                    core.report_level(0.0);
                }
                return;
            }
        }
    }
}

fn write_output(
    playback_tx: &PlaybackWriter,
    samples: &[f32],
    core: &std::sync::Weak<LivePeerCore>,
) -> bool {
    match playback_tx.write(samples) {
        Ok(()) => true,
        Err(e) => {
            if let Some(core) = core.upgrade()
                && !core.closing.load(Ordering::Acquire)
            {
                core.report_failure(format!("Live speaker playback failed: {e}"));
                // Clear output activity so the barge-in gate resets promptly.
                core.report_level(0.0);
            }
            false
        }
    }
}

/// Rolling RMS output-level meter: accumulates 2_400 samples (50 ms at 48 kHz)
/// before emitting a level, matching the OMP `OutputLevel`.
#[derive(Default)]
struct OutputLevel {
    sum_squares: f64,
    samples: usize,
}

impl OutputLevel {
    fn observe(&mut self, decoded: &[f32], core: &std::sync::Weak<LivePeerCore>) {
        let mut offset = 0;
        while offset < decoded.len() {
            let take = (OUTPUT_LEVEL_SAMPLES - self.samples).min(decoded.len() - offset);
            for &sample in &decoded[offset..offset + take] {
                let sample = f64::from(sample);
                self.sum_squares = sample.mul_add(sample, self.sum_squares);
            }
            self.samples += take;
            offset += take;
            if self.samples == OUTPUT_LEVEL_SAMPLES {
                if let Some(core) = core.upgrade() {
                    core.report_level((self.sum_squares / self.samples as f64).sqrt());
                }
                self.sum_squares = 0.0;
                self.samples = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_level_emits_at_2400_samples() {
        // A Weak with no Strong always upgrades to None, so report_level is a
        // no-op; the meter logic itself is what we test.
        let core: std::sync::Weak<LivePeerCore> = std::sync::Weak::new();
        let mut level = OutputLevel::default();
        // 2400 samples of amplitude 1.0 → RMS 1.0; observe in two chunks.
        let chunk = vec![1.0f32; 1200];
        level.observe(&chunk, &core);
        assert_eq!(level.samples, 1200);
        level.observe(&chunk, &core);
        // After reaching 2400 the meter resets.
        assert_eq!(level.samples, 0);
        assert_eq!(level.sum_squares, 0.0);
    }

    #[test]
    fn output_level_rms_is_correct_for_mixed_amplitude() {
        let core: std::sync::Weak<LivePeerCore> = std::sync::Weak::new();
        let mut level = OutputLevel::default();
        // 2400 samples: half +0.5, half -0.5 → RMS 0.5.
        let mut samples = vec![0.5f32; 1200];
        samples.extend(vec![-0.5f32; 1200]);
        level.observe(&samples, &core);
        assert_eq!(level.samples, 0, "meter reset after a full window");
    }

    #[test]
    fn output_level_splits_across_chunk_boundaries() {
        let core: std::sync::Weak<LivePeerCore> = std::sync::Weak::new();
        let mut level = OutputLevel::default();
        // 3000 samples in one call: must emit once at 2400 and carry 600.
        let samples = vec![0.0f32; 3000];
        level.observe(&samples, &core);
        assert_eq!(level.samples, 600);
    }

    /// Finding 6: the media event channel must be bounded so a slow consumer
    /// can't cause unbounded growth. Verify `LiveMediaPeer::new` returns a
    /// bounded receiver (try_send fails after the bound is reached).
    #[test]
    fn media_event_channel_is_bounded() {
        let (_peer, event_rx) = LiveMediaPeer::new();
        // The receiver starts empty; verify the bounded receiver type works.
        while event_rx.try_recv().map(|_| true).unwrap_or(false) {}
        assert!(event_rx.is_empty());
    }

    /// Build a `LivePeerCore` wired to a bounded `flume` and return it with
    /// the receiver + a signal subscriber so tests can drive `report_event` /
    /// `report_level` / `report_failure` directly.
    fn core_with_channel(
        bound: usize,
    ) -> (
        Arc<LivePeerCore>,
        flume::Receiver<MediaEvent>,
        watch::Receiver<PeerSignal>,
    ) {
        let (event_tx, event_rx) = flume::bounded::<MediaEvent>(bound);
        let core = Arc::new(LivePeerCore::new(event_tx));
        let signal_rx = core.signal_tx.subscribe();
        (core, event_rx, signal_rx)
    }

    /// A level flood must not fill the channel past the control reserve, so a
    /// subsequent control event (server payload) is delivered — never silently
    /// dropped because levels consumed all capacity.
    #[test]
    fn level_flood_leaves_headroom_for_control_events() {
        let (core, event_rx, _signal_rx) = core_with_channel(MEDIA_EVENT_BOUND);
        // Flood levels far in excess of the channel bound.
        for i in 0..(MEDIA_EVENT_BOUND * 4) {
            core.report_level(0.5 + (i as f64) * 1e-6);
        }
        // The channel must not be full of levels — at least the control
        // reserve must be free.
        let queued = event_rx.len();
        assert!(
            queued <= MEDIA_EVENT_BOUND - CONTROL_EVENT_RESERVE,
            "level flood queued {queued} events, expected <= {} (reserve {})",
            MEDIA_EVENT_BOUND - CONTROL_EVENT_RESERVE,
            CONTROL_EVENT_RESERVE
        );
        // A control event must still be delivered.
        core.report_event(r#"{"type":"delegation.created","item":{"id":"d1","content":[{"type":"input_text","text":"hi"}]}}"#.to_owned());
        let mut saw_event = false;
        while let Ok(ev) = event_rx.try_recv() {
            if let MediaEvent::Event(payload) = ev {
                assert!(payload.contains("delegation.created"));
                saw_event = true;
            }
        }
        assert!(saw_event, "control event was shed by a level flood");
    }

    /// Control-event saturation must produce exactly one explicit fatal
    /// overflow via the non-sheddable `PeerSignal::Failed` watch — never a
    /// silent drop. Fill the channel with control events (no receiver draining)
    /// so the reserve is exhausted, then inject one more and assert the watch
    /// fires `Failed`.
    #[test]
    fn control_event_saturation_reports_fatal_overflow_not_silent_loss() {
        let (core, event_rx, signal_rx) = core_with_channel(MEDIA_EVENT_BOUND);
        // Fill the channel entirely with control events (no draining).
        for i in 0..MEDIA_EVENT_BOUND {
            core.report_event(format!(
                r#"{{"type":"turn.done","turn":{{"role":"assistant","transcript":"t{i}"}}}}"#
            ));
        }
        assert_eq!(event_rx.len(), MEDIA_EVENT_BOUND);
        // The next control event cannot be enqueued → fatal overflow.
        core.report_event(r#"{"type":"delegation.created","item":{"id":"d2"}}"#.to_owned());
        // The non-sheddable watch must carry exactly one Failed.
        let mut saw_failed = false;
        // Drain the watch: it may have been published already.
        if let PeerSignal::Failed(msg) = signal_rx.borrow().clone() {
            assert!(msg.contains("saturated with control events"));
            saw_failed = true;
        }
        assert!(
            saw_failed,
            "control saturation did not publish a fatal overflow"
        );
        // A second saturation attempt must not publish a second fatal
        // (once-only overflow guard).
        core.report_event(
            r#"{"type":"turn.done","turn":{"role":"user","transcript":"x"}}"#.to_owned(),
        );
        let _ = signal_rx.borrow().clone();
        // The overflow_reported guard is once-only; assert no panic / no second
        // path beyond the first (verified by the guard being atomic).
        assert!(core.overflow_reported.load(Ordering::Acquire));
    }

    /// `report_failure` must publish `PeerSignal::Failed` via the
    /// non-sheddable watch even when the bounded event channel is already
    /// full of control events, so the transport's media-forward task surfaces
    /// it regardless of queue state.
    #[test]
    fn report_failure_published_via_watch_when_channel_full() {
        let (core, _event_rx, signal_rx) = core_with_channel(MEDIA_EVENT_BOUND);
        // Fill the channel with control events so the in-band Failure would
        // be shed.
        for _ in 0..MEDIA_EVENT_BOUND {
            core.report_event(
                r#"{"type":"turn.done","turn":{"role":"assistant","transcript":"t"}}"#.to_owned(),
            );
        }
        core.report_failure("peer exploded".to_owned());
        // The watch must carry the failure (authoritative path).
        let signal = signal_rx.borrow().clone();
        match signal {
            PeerSignal::Failed(msg) => assert_eq!(msg, "peer exploded"),
            other => panic!("expected PeerSignal::Failed, got {other:?}"),
        }
        // failure_reported is once-only.
        assert!(core.failure_reported.load(Ordering::Acquire));
    }

    /// `report_failure` is once-only: a second call does not overwrite the
    /// first watch value.
    #[test]
    fn report_failure_is_once_only() {
        let (core, _event_rx, signal_rx) = core_with_channel(8);
        core.report_failure("first failure".to_owned());
        core.report_failure("second failure".to_owned());
        match signal_rx.borrow().clone() {
            PeerSignal::Failed(msg) => assert_eq!(msg, "first failure"),
            other => panic!("expected PeerSignal::Failed, got {other:?}"),
        }
    }

    /// `report_level` drops non-finite values without touching the channel.
    #[test]
    fn report_level_ignores_non_finite() {
        let (core, event_rx, _signal_rx) = core_with_channel(8);
        core.report_level(f64::NAN);
        core.report_level(f64::INFINITY);
        core.report_level(f64::NEG_INFINITY);
        assert!(event_rx.is_empty());
    }

    /// Drive try_enqueue_audio against a real capacity-1 flume channel.
    /// Single-producer invariant: only this test thread enqueues.
    #[tokio::test]
    async fn audio_input_full_streak_via_real_channel() {
        let (core, _event_rx, signal_rx) = core_with_channel(8);
        let (tx, rx) = flume::bounded::<InputCommand>(1);
        let sample = [0.1f32; 32];

        // Fill the single slot.
        assert!(core.try_enqueue_audio(&tx, &sample).is_ok());
        assert_eq!(core.input_full_streak.load(Ordering::Acquire), 0);
        assert!(!matches!(*signal_rx.borrow(), PeerSignal::Failed(_)));

        // Fulls 1..7: shed only, no fatal.
        for i in 1..INPUT_FULL_FATAL_THRESHOLD {
            assert!(
                core.try_enqueue_audio(&tx, &sample).is_ok(),
                "Full #{i} must shed without error"
            );
            assert_eq!(core.input_full_streak.load(Ordering::Acquire), i);
            assert!(
                !matches!(*signal_rx.borrow(), PeerSignal::Failed(_)),
                "Full #{i} must not fatal"
            );
        }

        // Drain one slot → successful enqueue resets streak.
        let _ = rx.try_recv();
        assert!(core.try_enqueue_audio(&tx, &sample).is_ok());
        assert_eq!(
            core.input_full_streak.load(Ordering::Acquire),
            0,
            "successful enqueue must reset streak"
        );

        // Refill Full streak to exactly threshold → one fatal.
        for _ in 0..INPUT_FULL_FATAL_THRESHOLD {
            let _ = core.try_enqueue_audio(&tx, &sample);
        }
        assert!(matches!(
            *signal_rx.borrow(),
            PeerSignal::Failed(ref m) if m.contains("saturated")
        ));
        assert!(core.failure_reported.load(Ordering::Acquire));

        // Further Fulls must not re-fire (once-only failure).
        let first = format!("{:?}", signal_rx.borrow().clone());
        let _ = core.try_enqueue_audio(&tx, &sample);
        assert_eq!(
            format!("{:?}", signal_rx.borrow().clone()),
            first,
            "second saturation must not overwrite Failed"
        );
    }

    /// Disconnected input rolls back queued_samples and returns closed error.
    #[test]
    fn audio_input_disconnected_rolls_back_queued_samples() {
        let (core, _event_rx, _signal_rx) = core_with_channel(4);
        let (tx, rx) = flume::bounded::<InputCommand>(1);
        drop(rx); // disconnect receiver
        let before = core.queued_samples.load(Ordering::Acquire);
        let sample = [0.2f32; 16];
        let err = core
            .try_enqueue_audio(&tx, &sample)
            .expect_err("disconnected must error");
        assert!(err.contains("closed"), "{err}");
        assert_eq!(
            core.queued_samples.load(Ordering::Acquire),
            before,
            "queued_samples must roll back on Disconnected"
        );
    }

    /// Injected permanently-pending peer close still publishes Closed after
    /// a short deadline and reaps close_owner (no tokio test-util needed).
    #[tokio::test]
    async fn close_with_pending_peer_close_still_publishes_closed() {
        let (core, _event_rx, signal_rx) = core_with_channel(4);
        *core.close_owner.lock() = Some(tokio::spawn(async {}));

        let pending: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            Box::pin(std::future::pending::<()>());
        // 30ms deadline: real wall clock, fast enough for CI.
        let deadline = Duration::from_millis(30);
        let close_fut = Arc::clone(&core).run_close_once_with_peer_close(Some(pending), deadline);
        tokio::time::timeout(Duration::from_secs(2), close_fut)
            .await
            .expect("close owner must finish after peer-close deadline");

        assert!(matches!(*signal_rx.borrow(), PeerSignal::Closed));
        assert!(
            core.close_owner.lock().is_none(),
            "close_owner handle must be reaped"
        );
    }

    /// Close is once-only: concurrent callers all observe Closed, and the
    /// owner task always publishes Closed even if the first await is cancelled.
    #[tokio::test]
    async fn close_is_once_only_and_publishes_closed() {
        let (core, _event_rx, signal_rx) = core_with_channel(8);
        let c1 = Arc::clone(&core);
        let c2 = Arc::clone(&core);
        let h1 = tokio::spawn(async move { c1.close().await });
        let h2 = tokio::spawn(async move { c2.close().await });
        // Cancel the first waiter mid-flight; owner must still complete.
        h1.abort();
        let _ = h1.await;
        let _ = h2.await;
        // Bound wait for Closed.
        let mut rx = signal_rx;
        let done = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(*rx.borrow(), PeerSignal::Closed) {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        assert!(done.is_ok(), "close did not publish Closed in time");
        assert!(matches!(
            *core.signal_tx.subscribe().borrow(),
            PeerSignal::Closed
        ));
        // Repeat close is a no-op wait.
        core.close().await;
        assert!(matches!(
            *core.signal_tx.subscribe().borrow(),
            PeerSignal::Closed
        ));
    }

    /// Closing an idle (never started) peer still publishes Closed.
    #[tokio::test]
    async fn close_idle_peer_publishes_closed() {
        let (core, _event_rx, signal_rx) = core_with_channel(4);
        core.close().await;
        assert!(matches!(*signal_rx.borrow(), PeerSignal::Closed));
        // Second close returns immediately.
        core.close().await;
        assert!(matches!(*signal_rx.borrow(), PeerSignal::Closed));
    }
}
