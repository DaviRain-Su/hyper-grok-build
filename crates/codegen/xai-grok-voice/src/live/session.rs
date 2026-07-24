//! Codex Live session driver: connects the transport, reuses the existing PCM16
//! capture for mic input, applies the OMP barge-in echo gate and mute, and
//! translates [`LiveCommand`]s into transport messages until
//! [`LiveCommand::Shutdown`].
//!
//! The session reuses [`crate::audio`] capture (PCM16 mono) rather than maudio:
//! capture produces raw PCM16 LE bytes, which the session converts to the f32
//! samples the WebRTC peer consumes. This keeps one mic-capture backend across
//! dictation and live (macOS helper, Linux recorder, Windows cpal) and avoids
//! a second native audio stack.
//!
//! # Barge-in (echo gate)
//! The session implements the exact OMP barge-in rule from
//! `controller.ts`:
//! - `OUTPUT_ACTIVE_LEVEL = 0.015` — output is "active" when `outputLevel >
//!   0.015`.
//! - `MIN_BARGE_IN_LEVEL = 0.04` — minimum mic level to interrupt.
//! - `OUTPUT_ECHO_RATIO = 0.65` — echo threshold scales with output level.
//! - The mic is suppressed (not forwarded to the peer) only when
//!   `outputActive && inputLevel < max(0.04, outputLevel * 0.65)`. Louder user
//!   input passes through to interrupt the model.
//! - Output activity clears promptly when the output track ends (the media peer
//!   forces the shared output-level watch to 0.0).
//!
//! # Terminal failure (finding 1)
//! Exactly one fatal `LiveEvent::Error` is emitted by the **session** (not by
//! media/sideband callbacks). Callbacks publish a once-only signal via a
//! `watch<Option<LiveEvent>>` guarded by an `AtomicBool`; the session loop
//! subscribes, reads the first fatal, and emits the awaited `Error` → `Closing`
//! → `Closed` sequence. Simultaneous media + sideband failures surface only the
//! first.
//!
//! # Output level (finding 2)
//! The current output level is held in a `watch::channel<f64>` shared between
//! the media layer (writer) and the session barge-in gate (reader). The barge-in
//! reads `watch.borrow()` directly — never a stale value from a lossy queue. The
//! media layer forces the watch to 0.0 on every output-task exit and on teardown.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::protocol::{
    LiveClientMessage, LiveContextChannel, build_delegation_context_append, build_session_close,
    build_session_context_append, chunk_live_context,
};
use super::transport::CodexLiveTransport;
use super::types::{CallbackSink, LiveCommand, LiveConfig, LiveEvent, LivePhase, SharedLiveAuth};
use crate::audio;

/// The transport operations [`apply_command`] / [`send_chunked`] need. This is
/// a session-internal seam so the command-application error-propagation logic
/// can be unit-tested with a fake sink (no network). `CodexLiveTransport`
/// implements it; `send` returns `Err` when the sideband is closed and does
/// NOT invoke callbacks, so the caller must surface the error itself.
trait LiveTransportSink {
    /// Serialize and send one control message onto the sideband. Returns
    /// `Err` if the transport is not connected or the sideband is gone.
    fn send<'a>(
        &'a mut self,
        message: &'a LiveClientMessage,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Enable or disable the native audio source (echo gate / mute).
    /// Infallible / best-effort.
    fn set_muted(&mut self, muted: bool);
}

impl LiveTransportSink for CodexLiveTransport {
    fn send<'a>(
        &'a mut self,
        message: &'a LiveClientMessage,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(CodexLiveTransport::send(self, message))
    }

    fn set_muted(&mut self, muted: bool) {
        CodexLiveTransport::set_muted(self, muted);
    }
}

/// Default mic capture sample rate for a live session (16 kHz mono, matching
/// the WebRTC peer's input rate).
const LIVE_INPUT_SAMPLE_RATE: u32 = 16_000;

/// OMP barge-in constants (from `controller.ts`).
const OUTPUT_ACTIVE_LEVEL: f64 = 0.015;
const MIN_BARGE_IN_LEVEL: f64 = 0.04;
const OUTPUT_ECHO_RATIO: f64 = 0.65;

/// Bounded wait for delivering a terminal lifecycle event to the pager. If the
/// pager isn't draining its event channel within this window, the session
/// proceeds with teardown anyway — a stuck consumer must not prevent device
/// release (finding 7). If the receiver is gone, `send` returns Err
/// immediately (no hang).
const TERMINAL_EVENT_TIMEOUT: Duration = Duration::from_secs(2);

/// Clamp a level to `[0, 1]`, returning 0 for non-finite values (OMP
/// `clampLevel`).
fn clamp_level(level: f64) -> f64 {
    if !level.is_finite() || level <= 0.0 {
        0.0
    } else {
        level.min(1.0)
    }
}

/// Compute the RMS level of a mono f32 sample chunk (OMP `microphoneLevel`).
/// Returns 0 for empty input; clamps to `[0, 1]`.
fn microphone_level(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_squares: f64 = samples
        .iter()
        .map(|&s| {
            let s = f64::from(s);
            s * s
        })
        .sum();
    clamp_level((sum_squares / samples.len() as f64).sqrt())
}

/// Run a Codex Live voice session until [`LiveCommand::Shutdown`] (or the
/// command channel closes). Emits [`LiveEvent`]s to `event_tx`.
///
/// The session:
/// 1. Opens the existing PCM16 capture backend (reused from dictation).
/// 2. Connects the [`CodexLiveTransport`] (signaling + sideband + WebRTC peer).
///    A single connect future is maintained; commands arriving during connection
///    are buffered and applied after connect succeeds. `Shutdown` during
///    connection cancels the connect and cleans up safely.
/// 3. Forwards mic PCM (converted to f32) to the peer, applying the OMP
///    barge-in echo gate (suppress mic while output is active and the user
///    isn't loud enough to interrupt) and explicit mute commands. When the
///    mic PCM source closes, the PCM arm is permanently parked so the session
///    continues listen-only without hot-looping or starving command handling.
/// 4. Translates [`LiveCommand`]s into sideband control messages, chunking
///    context appends into ≤500-byte frames. A control-message send failure
///    (e.g. sideband closed) becomes the session's once-only fatal error and
///    drives Error → Closing → Closed — `CodexLiveTransport::send` does not
///    invoke callbacks on send errors, so the session surfaces it itself.
///    Mute commands are infallible (atomic flag + best-effort peer hint).
/// 5. Shuts down idempotently on `Shutdown` or a fatal transport/command error.
pub async fn run_live_session(
    config: LiveConfig,
    auth: SharedLiveAuth,
    mut cmd_rx: mpsc::Receiver<LiveCommand>,
    event_tx: mpsc::Sender<LiveEvent>,
) {
    let phase = |p: LivePhase| {
        let _ = event_tx.try_send(LiveEvent::Phase(p));
    };
    phase(LivePhase::Connecting);

    // Barge-in state: the current output level is held in a `watch` channel so
    // the barge-in decision reads the **latest** level directly from shared
    // state (never a stale value from a lossy queue). The media layer writes
    // here via `CallbackSink::on_output_level`. User mute is independent.
    let (output_level_tx, output_level_rx) = watch::channel(0.0f64);
    let user_muted = Arc::new(AtomicBool::new(false));

    // Fatal (terminal) event signal: a `watch<Option<LiveEvent>>` guarded by an
    // atomic in `CallbackSink` so exactly one fatal event is ever published,
    // even if media + sideband fail simultaneously. The session subscribes and
    // emits the awaited `LiveEvent::Error` itself (then Closing → Closed).
    let (fatal_tx, fatal_rx) = watch::channel(None::<LiveEvent>);

    let callbacks = Arc::new(CallbackSink::new(
        event_tx.clone(),
        output_level_tx.clone(),
        fatal_tx.clone(),
    ));

    // Open mic capture concurrently with the transport connect (both take
    // hundreds of ms). Reuse the existing PCM16 backend.
    let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<u8>>(64);
    let sample_rate = LIVE_INPUT_SAMPLE_RATE;
    let capture_task =
        tokio::task::spawn_blocking(move || audio::spawn_pcm_capture(sample_rate, pcm_tx));

    let mut transport = CodexLiveTransport::new(config, auth, callbacks);

    // --- Finding 3: maintain a single connect future; buffer commands during
    // connection. Only Shutdown cancels the connect. ---
    //
    // The connect future borrows `transport` mutably. We keep it in a
    // dedicated scope so it's dropped before we use `transport` again.
    // Shutdown during connection is handled by setting a flag and breaking
    // the loop (the connect future is then dropped at scope exit).
    let connect_result;
    let mut buffered_commands: Vec<LiveCommand> = Vec::new();
    let mut shutdown_during_connect = false;
    {
        let connect = transport.connect();
        tokio::pin!(connect);

        connect_result = loop {
            tokio::select! {
                biased;
                res = &mut connect => {
                    break res;
                }
                cmd = cmd_rx.recv() => match cmd {
                    Some(LiveCommand::Shutdown) | None => {
                        shutdown_during_connect = true;
                        break Err("Live session shutdown during connection".to_owned());
                    }
                    Some(other) => buffered_commands.push(other),
                }
            }
        };
        // `connect` is dropped here at scope exit, releasing the mutable borrow.
    }

    if shutdown_during_connect {
        // The connect future was dropped at scope exit. We must await capture
        // cleanup before emitting Closed — the capture task was spawned and may
        // still be opening the mic device.
        //
        // Finding 3: do NOT use a detached timeout on the spawn_blocking task.
        // `spawn_pcm_capture` is internally bounded (the subprocess handshake
        // has its own READY_TIMEOUT ~5s, and in-process cpal open is fast), so
        // awaiting the JoinHandle fully is safe and deterministic. A timeout
        // that detaches would leave a task that can later acquire the device.
        // If the task already succeeded, stop the handle to release the device.
        let capture_outcome = capture_task.await;
        match capture_outcome {
            Ok(Ok(handle)) => {
                handle.stop();
            }
            Ok(Err(_)) | Err(_) => {
                // Capture failed or the blocking task panicked — nothing to
                // stop. The device was never acquired (or the error already
                // closed it).
            }
        }
        transport.close().await;
        let _ = output_level_tx.send(0.0);
        phase(LivePhase::Closed);
        deliver_terminal(&event_tx, LiveEvent::Closed).await;
        return;
    }

    // Resolve the mic capture result.
    let capture = match capture_task.await {
        Ok(Ok(handle)) => handle,
        Ok(Err(e)) => {
            transport.close().await;
            let _ = output_level_tx.send(0.0);
            deliver_terminal(
                &event_tx,
                LiveEvent::Error {
                    message: e.to_string(),
                },
            )
            .await;
            phase(LivePhase::Closed);
            deliver_terminal(&event_tx, LiveEvent::Closed).await;
            return;
        }
        Err(join_err) => {
            transport.close().await;
            let _ = output_level_tx.send(0.0);
            deliver_terminal(
                &event_tx,
                LiveEvent::Error {
                    message: format!("voice capture task failed: {join_err}"),
                },
            )
            .await;
            phase(LivePhase::Closed);
            deliver_terminal(&event_tx, LiveEvent::Closed).await;
            return;
        }
    };

    if let Err(e) = connect_result {
        // Capture opened but connect failed: drop capture to release the mic.
        drop(capture);
        transport.close().await;
        let _ = output_level_tx.send(0.0);
        deliver_terminal(&event_tx, LiveEvent::Error { message: e }).await;
        phase(LivePhase::Closed);
        deliver_terminal(&event_tx, LiveEvent::Closed).await;
        return;
    }

    phase(LivePhase::Connected);

    // Apply buffered commands that arrived during connection. A send failure
    // (e.g. sideband already closed) becomes the session's once-only fatal and
    // drives Error → Closing → Closed — the model must not be left waiting.
    let mut buffered_fatal: Option<LiveEvent> = None;
    for cmd in buffered_commands {
        if let Err(e) = apply_command(&cmd, &mut transport, &user_muted).await {
            buffered_fatal = Some(LiveEvent::Error { message: e });
            break;
        }
    }

    // The session loop drives both mic-PCM forward (barge-in gated / muted) and
    // command handling.
    //
    // Finding 1: the loop monitors `fatal_rx` (a `watch` channel) for the
    // single fatal event. When it fires, the loop breaks and the shutdown path
    // emits the awaited Error (read from the watch), then Closing → Closed.
    // Finding 2: the barge-in reads `output_level_rx` (a `watch`) directly —
    // no lossy queue. Output level is forced to 0.0 on teardown below.
    let mut capture = Some(capture);
    let mut fatal_rx = fatal_rx;
    // Seed with a buffered-command fatal (if a command that arrived during
    // connection failed to send) so the loop immediately tears down.
    let mut fatal_event: Option<LiveEvent> = buffered_fatal;
    // Whether the mic PCM source has closed. Once `pcm_rx.recv()` returns
    // `None` (the capture sender dropped), a closed mpsc receiver is
    // immediately ready forever and would hot-loop this biased `select!`,
    // starving `cmd_rx` (Shutdown/CompleteDelegation) and preventing
    // teardown. We permanently disable the PCM arm via `pending()` so the
    // session continues listen-only and command handling stays responsive.
    let mut pcm_closed = false;
    // A command send failure (e.g. sideband closed) becomes the session's
    // once-only fatal error and drives Error → Closing → Closed. Mute
    // commands remain infallible/best-effort.
    let mut command_fatal: Option<LiveEvent> = None;
    loop {
        // If a buffered/connected command send failed, surface it as the
        // session's once-only fatal and tear down — never leave the model
        // waiting while the session reports Connected. `fatal_event` may be
        // pre-seeded by a buffered-command failure (above), so check it first.
        if let Some(ev) = command_fatal.take() {
            fatal_event = Some(ev);
            break;
        }
        if fatal_event.is_some() {
            break;
        }
        tokio::select! {
            biased;
            // Fatal (terminal) event from the transport/media — exactly one.
            // `watch::changed` resolves when the first fatal is published.
            _ = fatal_rx.changed() => {
                if let Some(ev) = fatal_rx.borrow().clone() {
                    fatal_event = Some(ev);
                    break;
                }
            }
            // Forward mic PCM to the peer with the OMP barge-in echo gate.
            // Once the PCM source closes, this arm is parked on `pending()`
            // forever so it cannot hot-loop or starve command handling.
            bytes = pcm_recv(&mut pcm_rx, &mut pcm_closed) => match bytes {
                Some(bytes) => {
                    if user_muted.load(Ordering::Acquire) {
                        continue;
                    }
                    let samples = pcm16_le_to_f32(&bytes);
                    if samples.is_empty() {
                        continue;
                    }
                    // Barge-in rule (OMP controller.ts):
                    //   outputActive = outputLevel > 0.015
                    //   echoThreshold = max(0.04, outputLevel * 0.65)
                    //   suppress if outputActive && inputLevel < echoThreshold
                    //
                    // The output level is read from the shared `watch` — the
                    // latest value written by the media layer, never stale.
                    let out_level = *output_level_rx.borrow();
                    let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
                    let input_level = microphone_level(&samples);
                    let echo_threshold = MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO);
                    if output_active && input_level < echo_threshold {
                        continue;
                    }
                    transport.push_audio(&samples);
                }
                None => {
                    // Mic stream ended (capture stopped). Not fatal; the session
                    // can continue without mic input (e.g. listen-only). The PCM
                    // arm is now permanently parked via `pcm_closed` so this
                    // arm never hot-loops.
                }
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(LiveCommand::Shutdown) | None => break,
                Some(other) => {
                    if let Err(e) = apply_command(&other, &mut transport, &user_muted).await {
                        // A control message (e.g. CompleteDelegation) failed to
                        // reach Codex. `CodexLiveTransport::send` does NOT
                        // invoke callbacks on send errors, so we must surface
                        // it ourselves as the once-only fatal and tear down —
                        // otherwise the model is left waiting while the session
                        // reports Connected.
                        command_fatal = Some(LiveEvent::Error { message: e });
                    }
                }
            },
        }
    }

    // Idempotent shutdown: the loop above exits on Shutdown/closed rx, or on a
    // fatal event. This runs exactly once.
    phase(LivePhase::Closing);
    // Finding 1: if we broke due to a fatal event, the session (not the
    // callback) emits exactly one awaited Error. If we broke due to Shutdown,
    // no Error is emitted (clean shutdown).
    if let Some(ev) = fatal_event.take() {
        deliver_terminal(&event_tx, ev).await;
    }
    // Stop the mic first to release the device, then close the transport.
    if let Some(handle) = capture.take() {
        handle.stop();
    }
    // Best-effort session.close on the sideband.
    let _ = transport.send(&build_session_close()).await;
    transport.close().await;
    // Finding 2: force output level to zero so the barge-in gate clears and no
    // stale state remains after teardown.
    let _ = output_level_tx.send(0.0);
    phase(LivePhase::Closed);
    deliver_terminal(&event_tx, LiveEvent::Closed).await;
    // Drop the capture handle if we somehow still hold it.
    if let Some(handle) = capture.take() {
        handle.stop();
    }
}

/// Deliver a terminal lifecycle event (`Error` or `Closed`) to the pager with a
/// bounded wait so a stuck/slow consumer cannot deadlock the session or prevent
/// device release (finding 7). If the receiver is gone, `send` returns Err
/// immediately (no hang). If the receiver is alive but not draining, we wait at
/// most `TERMINAL_EVENT_TIMEOUT` then proceed regardless.
async fn deliver_terminal(event_tx: &mpsc::Sender<LiveEvent>, event: LiveEvent) {
    let _ = tokio::time::timeout(TERMINAL_EVENT_TIMEOUT, event_tx.send(event)).await;
}

/// Poll the mic PCM receiver, permanently parking on `pending()` once the
/// source has closed. A closed mpsc receiver is immediately ready forever
/// (returning `None` each poll), which would hot-loop a biased `select!` and
/// starve command handling. Once `pcm_closed` is set, this returns
/// `Pending` forever so the session continues listen-only while `cmd_rx`
/// stays responsive.
///
/// Returns `None` exactly once (the first time the source closes); subsequent
/// calls never resolve.
async fn pcm_recv(pcm_rx: &mut mpsc::Receiver<Vec<u8>>, pcm_closed: &mut bool) -> Option<Vec<u8>> {
    if *pcm_closed {
        // Park forever: the closed receiver must not be re-polled.
        std::future::pending::<()>().await;
        return None;
    }
    let result = pcm_rx.recv().await;
    if result.is_none() {
        *pcm_closed = true;
    }
    result
}

/// Apply a single [`LiveCommand`] to the transport. Used both for buffered
/// commands (arrived during connection) and live commands (post-connect).
///
/// Returns `Err(message)` when a control message fails to reach Codex (e.g.
/// the sideband is closed). The caller turns this into the session's once-only
/// fatal error. Mute commands are infallible (they only flip an atomic flag
/// and a best-effort peer hint) and always return `Ok`.
async fn apply_command(
    cmd: &LiveCommand,
    transport: &mut impl LiveTransportSink,
    user_muted: &Arc<AtomicBool>,
) -> Result<(), String> {
    match cmd {
        LiveCommand::ToggleMute => {
            let next = !user_muted.load(Ordering::Acquire);
            user_muted.store(next, Ordering::Release);
            transport.set_muted(next);
            Ok(())
        }
        LiveCommand::SetMuted(muted) => {
            user_muted.store(*muted, Ordering::Release);
            transport.set_muted(*muted);
            Ok(())
        }
        LiveCommand::AppendDelegationContext {
            delegation_id,
            text,
            channel,
        } => {
            send_chunked(
                transport,
                &chunk_live_context(text),
                Some(delegation_id.clone()),
                Some(*channel),
            )
            .await
        }
        LiveCommand::CompleteDelegation {
            delegation_id,
            text,
        } => {
            // OMP #appendFinalResponse: the final result is
            // buildDelegationContextAppend(delegationId, chunk) with NO
            // channel — the channel argument is omitted entirely (not
            // "commentary", not "speakable"). The server treats the final
            // append as the delegation's result.
            send_chunked(
                transport,
                &chunk_live_context(text),
                Some(delegation_id.clone()),
                None,
            )
            .await
        }
        LiveCommand::AppendSessionContext { text, channel } => {
            send_chunked(transport, &chunk_live_context(text), None, Some(*channel)).await
        }
        LiveCommand::Shutdown => {
            // Handled by the caller's loop break; unreachable here.
            Ok(())
        }
    }
}

/// Send chunked context appends to the transport. Each chunk becomes one
/// `delegation.context.append` or `session.context.append` message. Returns
/// `Err` on the first chunk that fails to send; later chunks are NOT attempted
/// (a partial delegation append after a sideband close is not recoverable).
/// `CodexLiveTransport::send` does NOT invoke callbacks on send errors, so the
/// caller must surface the error as the session's once-only fatal itself.
async fn send_chunked(
    transport: &mut impl LiveTransportSink,
    chunks: &[String],
    delegation_id: Option<String>,
    channel: Option<LiveContextChannel>,
) -> Result<(), String> {
    for chunk in chunks {
        let message = match &delegation_id {
            Some(id) => build_delegation_context_append(id, chunk, channel),
            None => build_session_context_append(chunk, channel),
        };
        transport.send(&message).await?;
    }
    Ok(())
}

/// Convert PCM16 little-endian bytes to mono f32 samples in `[-1.0, 1.0]`.
fn pcm16_le_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / i16::MAX as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm16_le_to_f32_converts_correctly() {
        let bytes = [
            0x00, 0x00, // 0
            0xff, 0x7f, // i16::MAX
            0x01, 0x80, // i16::MIN
        ];
        let f = pcm16_le_to_f32(&bytes);
        assert_eq!(f.len(), 3);
        assert!((f[0]).abs() < 1e-6);
        assert!((f[1] - 1.0).abs() < 1e-4);
        assert!((f[2] + 1.0).abs() < 1e-4);
    }

    #[test]
    fn pcm16_le_to_f32_ignores_trailing_odd_byte() {
        let bytes = [0x00, 0x00, 0xff];
        let f = pcm16_le_to_f32(&bytes);
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn pcm16_le_to_f32_empty_input() {
        assert!(pcm16_le_to_f32(&[]).is_empty());
    }

    #[test]
    fn clamp_level_returns_zero_for_non_finite_and_negative() {
        assert_eq!(clamp_level(0.0), 0.0);
        assert_eq!(clamp_level(-1.0), 0.0);
        assert_eq!(clamp_level(f64::NAN), 0.0);
        assert_eq!(clamp_level(f64::INFINITY), 0.0);
        assert_eq!(clamp_level(f64::NEG_INFINITY), 0.0);
    }

    #[test]
    fn clamp_level_caps_at_one() {
        assert_eq!(clamp_level(1.0), 1.0);
        assert_eq!(clamp_level(2.0), 1.0);
        assert!((clamp_level(0.5) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn microphone_level_computes_rms_correctly() {
        let samples = vec![1.0f32; 100];
        assert!((microphone_level(&samples) - 1.0).abs() < 1e-6);

        let samples = vec![0.5f32; 100];
        assert!((microphone_level(&samples) - 0.5).abs() < 1e-6);

        let samples = vec![0.0f32; 100];
        assert_eq!(microphone_level(&samples), 0.0);

        assert_eq!(microphone_level(&[]), 0.0);
    }

    #[test]
    fn microphone_level_clamps_to_one() {
        let samples = vec![2.0f32; 100];
        assert_eq!(microphone_level(&samples), 1.0);
    }

    #[test]
    fn barge_in_thresholds_match_omp() {
        assert_eq!(OUTPUT_ACTIVE_LEVEL, 0.015);
        assert_eq!(MIN_BARGE_IN_LEVEL, 0.04);
        assert_eq!(OUTPUT_ECHO_RATIO, 0.65);
    }

    #[test]
    fn barge_in_suppresses_quiet_input_during_output() {
        let out_level = 0.5;
        let input_level = 0.02;
        let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
        let echo_threshold = MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO);
        assert!(output_active);
        assert!(input_level < echo_threshold);
        assert!(output_active && input_level < echo_threshold);
    }

    #[test]
    fn barge_in_passes_loud_input_during_output() {
        let out_level = 0.5;
        let input_level = 0.4;
        let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
        let echo_threshold = MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO);
        assert!(output_active);
        assert!(input_level >= echo_threshold);
    }

    #[test]
    fn barge_in_passes_all_input_when_output_inactive() {
        let out_level = 0.01;
        let input_level = 0.001;
        let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
        assert!(!output_active);
        assert!(
            !(output_active && input_level < MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO))
        );
    }

    #[test]
    fn barge_in_uses_min_threshold_when_output_low() {
        let out_level = 0.02;
        let input_level = 0.03;
        let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
        let echo_threshold = MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO);
        assert_eq!(echo_threshold, 0.04);
        assert!(output_active);
        assert!(input_level < echo_threshold);
    }

    /// Finding 2: the barge-in gate reads the latest output level from a
    /// `watch` channel directly. Verify that a watch always returns the most
    /// recently written value (no stale queue).
    #[test]
    fn output_level_watch_returns_latest_value() {
        let (tx, rx) = watch::channel(0.0f64);
        assert_eq!(*rx.borrow(), 0.0);
        tx.send(0.5).unwrap();
        assert!((*rx.borrow() - 0.5).abs() < 1e-10);
        tx.send(0.8).unwrap();
        tx.send(0.1).unwrap();
        // The latest write wins — no queue accumulation.
        assert!((*rx.borrow() - 0.1).abs() < 1e-10);
        // Forcing zero clears the gate.
        tx.send(0.0).unwrap();
        assert_eq!(*rx.borrow(), 0.0);
    }

    /// Finding 7: terminal event delivery must not hang when the receiver is
    /// gone. `deliver_terminal` uses a bounded timeout; with a dropped receiver
    /// the send returns Err immediately.
    #[tokio::test]
    async fn deliver_terminal_returns_promptly_when_receiver_gone() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let start = std::time::Instant::now();
        deliver_terminal(&tx, LiveEvent::Closed).await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "deliver_terminal hung on a closed receiver"
        );
    }

    /// Finding 7: terminal event delivery must not block indefinitely on a
    /// slow consumer. A bounded channel that's never drained should time out
    /// within `TERMINAL_EVENT_TIMEOUT`.
    #[tokio::test]
    async fn deliver_terminal_times_out_on_slow_consumer() {
        let (tx, _rx) = mpsc::channel(1);
        // Fill the single-slot channel so the next send would block.
        tx.send(LiveEvent::Phase(LivePhase::Closing)).await.unwrap();
        let start = std::time::Instant::now();
        deliver_terminal(&tx, LiveEvent::Closed).await;
        let elapsed = start.elapsed();
        // It should time out around TERMINAL_EVENT_TIMEOUT (2s), not hang.
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(4),
            "deliver_terminal returned in {elapsed:?} — expected ~2s timeout"
        );
    }

    // --- Fix 1: closed PCM source must not hot-loop or starve commands -----

    /// `pcm_recv` returns `None` exactly once when the source closes, then
    /// permanently parks on `pending()` so the biased `select!` arm cannot
    /// hot-loop.
    #[tokio::test]
    async fn pcm_recv_returns_none_once_then_parks_forever() {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<u8>>(4);
        let mut pcm_closed = false;
        // Send one frame, then close the sender.
        pcm_tx.send(vec![0x00, 0x01]).await.unwrap();
        assert_eq!(
            pcm_recv(&mut pcm_rx, &mut pcm_closed).await,
            Some(vec![0x00, 0x01])
        );
        assert!(!pcm_closed);
        drop(pcm_tx);
        // The close yields None once and sets the flag.
        assert_eq!(pcm_recv(&mut pcm_rx, &mut pcm_closed).await, None);
        assert!(pcm_closed);
        // A subsequent call must never resolve (parked forever). Race it
        // against a timeout to prove it doesn't return immediately (which
        // would be the hot-loop bug).
        let parked = tokio::time::timeout(
            Duration::from_millis(100),
            pcm_recv(&mut pcm_rx, &mut pcm_closed),
        )
        .await;
        assert!(
            parked.is_err(),
            "pcm_recv resolved after close — hot-loop bug"
        );
    }

    /// A closed PCM source must not starve a later command. This simulates the
    /// session's biased `select!` loop: the PCM arm is parked after the source
    /// closes, so `cmd_rx.recv()` stays responsive and a `Shutdown` (or any
    /// command) is delivered promptly — no hot spin consuming CPU.
    #[tokio::test]
    async fn closed_pcm_source_does_not_starve_command() {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<u8>>(4);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<LiveCommand>(4);
        let mut pcm_closed = false;

        // Close the PCM source first.
        drop(pcm_tx);
        // Drain the close notification so the arm parks.
        assert_eq!(pcm_recv(&mut pcm_rx, &mut pcm_closed).await, None);
        assert!(pcm_closed);

        // Now post a command and run one iteration of the biased select. The
        // PCM arm is parked, so the command arm must win immediately.
        cmd_tx.send(LiveCommand::SetMuted(true)).await.unwrap();
        let start = std::time::Instant::now();
        let select_loop = async {
            tokio::select! {
                biased;
                // PCM arm — parked via the helper.
                bytes = pcm_recv(&mut pcm_rx, &mut pcm_closed) => {
                    Err::<(), String>(format!("PCM arm fired with {bytes:?} — should be parked"))
                }
                cmd = cmd_rx.recv() => {
                    assert!(matches!(cmd, Some(LiveCommand::SetMuted(true))));
                    Ok(())
                }
            }
        };
        let outcome = tokio::time::timeout(Duration::from_millis(500), select_loop).await;
        assert!(outcome.is_ok(), "select hung — PCM arm starved the command");
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "command delivery took {:?} — PCM hot-loop starving cmd_rx",
            start.elapsed()
        );
    }

    // --- Fix 2: command send failure must propagate as the fatal error -----

    /// A fake transport sink that records sends and can be programmed to fail
    /// after N successful sends, with configurable mute behavior. Used to
    /// unit-test `apply_command` / `send_chunked` error propagation without a
    /// network.
    struct FakeSink {
        /// `Some(n)` = fail on the (0-indexed) n-th send; `None` = never fail.
        fail_on: Option<usize>,
        sends: usize,
        muted: Option<bool>,
        sent_messages: Vec<String>,
    }

    impl FakeSink {
        fn new() -> Self {
            Self {
                fail_on: None,
                sends: 0,
                muted: None,
                sent_messages: Vec::new(),
            }
        }
        fn fail_on(mut self, n: usize) -> Self {
            self.fail_on = Some(n);
            self
        }
    }

    impl LiveTransportSink for FakeSink {
        fn send<'a>(
            &'a mut self,
            message: &'a LiveClientMessage,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async move {
                let idx = self.sends;
                self.sends += 1;
                self.sent_messages
                    .push(serde_json::to_string(message).unwrap_or_default());
                if matches!(self.fail_on, Some(n) if n == idx) {
                    Err("sideband is closed".to_owned())
                } else {
                    Ok(())
                }
            })
        }
        fn set_muted(&mut self, muted: bool) {
            self.muted = Some(muted);
        }
    }

    /// A `CompleteDelegation` whose chunk send fails must propagate `Err` —
    /// the caller turns it into the once-only fatal. The session must not
    /// leave the model waiting while reporting Connected.
    #[tokio::test]
    async fn apply_command_complete_delegation_propagates_send_error() {
        let mut sink = FakeSink::new().fail_on(0);
        let user_muted = Arc::new(AtomicBool::new(false));
        // "final result text" chunks into a single chunk for this short text.
        let result = apply_command(
            &LiveCommand::CompleteDelegation {
                delegation_id: "del-1".into(),
                text: "done".into(),
            },
            &mut sink,
            &user_muted,
        )
        .await;
        assert!(result.is_err(), "send error was swallowed");
        assert!(
            result.unwrap_err().contains("sideband is closed"),
            "unexpected error message"
        );
        // Exactly one send was attempted (the failing chunk).
        assert_eq!(sink.sends, 1);
    }

    /// Mute commands are infallible: they never call `send`, so a failing sink
    /// does not affect them and they return `Ok`.
    #[tokio::test]
    async fn apply_command_mute_is_infallible() {
        let mut sink = FakeSink::new().fail_on(0);
        let user_muted = Arc::new(AtomicBool::new(false));
        apply_command(&LiveCommand::ToggleMute, &mut sink, &user_muted)
            .await
            .unwrap();
        assert!(user_muted.load(Ordering::Acquire), "mute flag not toggled");
        assert_eq!(sink.muted, Some(true), "peer mute hint not set");
        assert_eq!(sink.sends, 0, "mute command issued a send");

        apply_command(&LiveCommand::SetMuted(false), &mut sink, &user_muted)
            .await
            .unwrap();
        assert!(!user_muted.load(Ordering::Acquire));
        assert_eq!(sink.muted, Some(false));
        assert_eq!(sink.sends, 0);
    }

    /// `send_chunked` must stop at the first failing chunk and propagate the
    /// error; later chunks must NOT be attempted (a partial append after a
    /// sideband close is not recoverable).
    #[tokio::test]
    async fn send_chunked_stops_at_first_failing_chunk() {
        // Build enough text to produce multiple chunks. chunk_live_context
        // splits into ≤500-byte chunks, so a long string yields several.
        let big = "x".repeat(1200);
        let chunks = chunk_live_context(&big);
        assert!(chunks.len() >= 2, "test text must produce multiple chunks");

        let mut sink = FakeSink::new().fail_on(1); // fail on the second chunk
        let result = send_chunked(
            &mut sink,
            &chunks,
            Some("del-2".into()),
            Some(super::super::protocol::LiveContextChannel::Commentary),
        )
        .await;
        assert!(result.is_err());
        // Two sends were attempted (chunk 0 ok, chunk 1 failed) — chunk 2+ not
        // attempted.
        assert_eq!(sink.sends, 2, "later chunks were attempted after a failure");
    }

    /// A successful `send_chunked` sends every chunk and returns `Ok`.
    #[tokio::test]
    async fn send_chunked_sends_all_chunks_on_success() {
        let big = "y".repeat(1100);
        let chunks = chunk_live_context(&big);
        assert!(chunks.len() >= 2);
        let mut sink = FakeSink::new();
        let result = send_chunked(&mut sink, &chunks, None, None).await;
        assert!(result.is_ok());
        assert_eq!(sink.sends, chunks.len());
    }

    /// A delegation-context append that succeeds must produce
    /// `delegation.context.append` messages carrying the delegation id.
    #[tokio::test]
    async fn apply_command_append_delegation_context_sends_with_id() {
        let mut sink = FakeSink::new();
        let user_muted = Arc::new(AtomicBool::new(false));
        apply_command(
            &LiveCommand::AppendDelegationContext {
                delegation_id: "del-3".into(),
                text: "ctx".into(),
                channel: super::super::protocol::LiveContextChannel::Speakable,
            },
            &mut sink,
            &user_muted,
        )
        .await
        .unwrap();
        assert!(sink.sends >= 1);
        assert!(
            sink.sent_messages.iter().all(|m| m.contains("del-3")),
            "delegation id not carried in all chunk messages"
        );
    }
}
