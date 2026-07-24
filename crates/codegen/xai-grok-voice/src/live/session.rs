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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::protocol::{
    build_delegation_context_append, build_session_close, build_session_context_append,
    chunk_live_context,
};
use super::transport::CodexLiveTransport;
use super::types::{CallbackSink, LiveCommand, LiveConfig, LiveEvent, LivePhase, SharedLiveAuth};
use crate::audio;

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
///    isn't loud enough to interrupt) and explicit mute commands.
/// 4. Translates [`LiveCommand`]s into sideband control messages, chunking
///    context appends into ≤500-byte frames.
/// 5. Shuts down idempotently on `Shutdown` or a fatal transport error.
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

    // Apply buffered commands that arrived during connection.
    for cmd in buffered_commands {
        apply_command(&cmd, &mut transport, &user_muted).await;
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
    let mut fatal_event: Option<LiveEvent> = None;
    loop {
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
            bytes = pcm_rx.recv() => match bytes {
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
                    // can continue without mic input (e.g. listen-only).
                }
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(LiveCommand::Shutdown) | None => break,
                Some(other) => {
                    apply_command(&other, &mut transport, &user_muted).await;
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

/// Apply a single [`LiveCommand`] to the transport. Used both for buffered
/// commands (arrived during connection) and live commands (post-connect).
async fn apply_command(
    cmd: &LiveCommand,
    transport: &mut CodexLiveTransport,
    user_muted: &Arc<AtomicBool>,
) {
    match cmd {
        LiveCommand::ToggleMute => {
            let next = !user_muted.load(Ordering::Acquire);
            user_muted.store(next, Ordering::Release);
            transport.set_muted(next);
        }
        LiveCommand::SetMuted(muted) => {
            user_muted.store(*muted, Ordering::Release);
            transport.set_muted(*muted);
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
            .await;
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
            .await;
        }
        LiveCommand::AppendSessionContext { text, channel } => {
            send_chunked(transport, &chunk_live_context(text), None, Some(*channel)).await;
        }
        LiveCommand::Shutdown => {
            // Handled by the caller's loop break; unreachable here.
        }
    }
}

/// Send chunked context appends to the transport. Each chunk becomes one
/// `delegation.context.append` or `session.context.append` message. Errors are
/// surfaced as `LiveEvent::Error` by the transport's callbacks; here we only
/// log at debug (a failed send during shutdown is expected).
async fn send_chunked(
    transport: &CodexLiveTransport,
    chunks: &[String],
    delegation_id: Option<String>,
    channel: Option<super::protocol::LiveContextChannel>,
) {
    for chunk in chunks {
        let message = match &delegation_id {
            Some(id) => build_delegation_context_append(id, chunk, channel),
            None => build_session_context_append(chunk, channel),
        };
        if transport.send(&message).await.is_err() {
            break;
        }
    }
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
}
