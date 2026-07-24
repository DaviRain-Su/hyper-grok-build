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
//!   emits a final 0.0 level).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::mpsc;

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

/// Store the current output level as a bit-packed `AtomicU64` so the barge-in
/// decision can be made per mic chunk without a mutex. The level is stored as
/// the bits of an `f64`.
fn store_output_level(atomic: &AtomicU64, level: f64) {
    atomic.store(level.to_bits(), Ordering::Release);
}

fn load_output_level(atomic: &AtomicU64) -> f64 {
    f64::from_bits(atomic.load(Ordering::Acquire))
}

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

    // Barge-in state: the current output level (bit-packed f64) drives the
    // echo gate decision per mic chunk. User mute is independent.
    let output_level = Arc::new(AtomicU64::new(0.0f64.to_bits()));
    let user_muted = Arc::new(AtomicBool::new(false));

    // Levels flow on a dedicated internal channel so the session loop can drive
    // the barge-in gate and forward them to the pager.
    let (levels_tx, mut levels_rx) = mpsc::channel::<f64>(64);

    let callbacks = Arc::new(CallbackSink {
        event_tx: event_tx.clone(),
        levels_tx,
    });

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
                    // The single connect future resolved. Break with the result.
                    break res;
                }
                cmd = cmd_rx.recv() => match cmd {
                    Some(LiveCommand::Shutdown) | None => {
                        // Shutdown during connection: set the flag and break.
                        // The connect future is dropped at scope exit
                        // (canceling it safely).
                        shutdown_during_connect = true;
                        break Err("Live session shutdown during connection".to_owned());
                    }
                    // Buffer non-shutdown commands; they're applied after connect.
                    Some(other) => buffered_commands.push(other),
                }
            }
        };
        // `connect` is dropped here at scope exit, releasing the mutable borrow.
    }

    if shutdown_during_connect {
        // The connect future was dropped at scope exit. Clean up.
        transport.close().await;
        phase(LivePhase::Closed);
        let _ = event_tx.send(LiveEvent::Closed).await;
        return;
    }

    // Resolve the mic capture result.
    let capture = match capture_task.await {
        Ok(Ok(handle)) => handle,
        Ok(Err(e)) => {
            transport.close().await;
            let _ = event_tx
                .send(LiveEvent::Error {
                    message: e.to_string(),
                })
                .await;
            phase(LivePhase::Closed);
            let _ = event_tx.send(LiveEvent::Closed).await;
            return;
        }
        Err(join_err) => {
            transport.close().await;
            let _ = event_tx
                .send(LiveEvent::Error {
                    message: format!("voice capture task failed: {join_err}"),
                })
                .await;
            phase(LivePhase::Closed);
            let _ = event_tx.send(LiveEvent::Closed).await;
            return;
        }
    };

    if let Err(e) = connect_result {
        // Capture opened but connect failed: drop capture to release the mic.
        drop(capture);
        transport.close().await;
        let _ = event_tx.send(LiveEvent::Error { message: e }).await;
        phase(LivePhase::Closed);
        let _ = event_tx.send(LiveEvent::Closed).await;
        return;
    }

    phase(LivePhase::Connected);

    // Apply buffered commands that arrived during connection.
    for cmd in buffered_commands {
        apply_command(&cmd, &mut transport, &output_level, &user_muted, &event_tx).await;
    }

    // The session loop drives both mic-PCM forward (barge-in gated / muted) and
    // command handling. The mic PCM channel (`pcm_rx`) was filled by the
    // capture task above; capture stays open for the session's lifetime.
    let mut capture = Some(capture);
    loop {
        tokio::select! {
            biased;
            // Output levels: update the barge-in gate state and forward to pager.
            level = levels_rx.recv() => {
                if let Some(level) = level {
                    let clamped = clamp_level(level);
                    store_output_level(&output_level, clamped);
                    let _ = event_tx.try_send(LiveEvent::Levels(clamped));
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
                    let out_level = load_output_level(&output_level);
                    let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
                    let input_level = microphone_level(&samples);
                    let echo_threshold = MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO);
                    if output_active && input_level < echo_threshold {
                        // Suppress: the user's input is likely echo, not a
                        // barge-in. Skip forwarding this chunk.
                        continue;
                    }
                    // The user is loud enough to interrupt (or the model is
                    // silent): forward the mic audio to the peer.
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
                    apply_command(&other, &mut transport, &output_level, &user_muted, &event_tx).await;
                }
            },
        }
    }

    // Idempotent shutdown: the loop above only exits on Shutdown/closed rx,
    // so this runs exactly once.
    phase(LivePhase::Closing);
    // Stop the mic first to release the device, then close the transport.
    if let Some(handle) = capture.take() {
        handle.stop();
    }
    // Best-effort session.close on the sideband.
    let _ = transport.send(&build_session_close()).await;
    transport.close().await;
    phase(LivePhase::Closed);
    let _ = event_tx.send(LiveEvent::Closed).await;
    // Drop the capture handle if we somehow still hold it.
    if let Some(handle) = capture.take() {
        handle.stop();
    }
}

/// Apply a single [`LiveCommand`] to the transport. Used both for buffered
/// commands (arrived during connection) and live commands (post-connect).
async fn apply_command(
    cmd: &LiveCommand,
    transport: &mut CodexLiveTransport,
    output_level: &Arc<AtomicU64>,
    user_muted: &Arc<AtomicBool>,
    event_tx: &mpsc::Sender<LiveEvent>,
) {
    match cmd {
        LiveCommand::ToggleMute => {
            let next = !user_muted.load(Ordering::Acquire);
            user_muted.store(next, Ordering::Release);
            // The barge-in gate is per-chunk (not a global mute), so user mute
            // only affects whether mic PCM is forwarded at all. We don't need
            // to call transport.set_muted for the echo gate — but we do call it
            // so the peer's encoder knows to discard partial frames (OMP
            // behavior: muting clears pending input).
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
            let _ = event_tx;
            let _ = output_level;
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
            // Sideband closed mid-send; stop chunking.
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
        let bytes = [0x00, 0x00, 0xff]; // one full sample + 1 odd byte
        let f = pcm16_le_to_f32(&bytes);
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn pcm16_le_to_f32_empty_input() {
        assert!(pcm16_le_to_f32(&[]).is_empty());
    }

    // --- Barge-in (finding 7) tests ---

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
        // Full-scale sine → RMS ~0.707 for a full-scale sine, but we use
        // constant amplitude. Amplitude 1.0 → RMS 1.0.
        let samples = vec![1.0f32; 100];
        assert!((microphone_level(&samples) - 1.0).abs() < 1e-6);

        // Amplitude 0.5 → RMS 0.5.
        let samples = vec![0.5f32; 100];
        assert!((microphone_level(&samples) - 0.5).abs() < 1e-6);

        // Silence → 0.
        let samples = vec![0.0f32; 100];
        assert_eq!(microphone_level(&samples), 0.0);

        // Empty → 0.
        assert_eq!(microphone_level(&[]), 0.0);
    }

    #[test]
    fn microphone_level_clamps_to_one() {
        // Amplitude > 1.0 should clamp.
        let samples = vec![2.0f32; 100];
        assert_eq!(microphone_level(&samples), 1.0);
    }

    /// Verify the exact OMP barge-in thresholds.
    #[test]
    fn barge_in_thresholds_match_omp() {
        assert_eq!(OUTPUT_ACTIVE_LEVEL, 0.015);
        assert_eq!(MIN_BARGE_IN_LEVEL, 0.04);
        assert_eq!(OUTPUT_ECHO_RATIO, 0.65);
    }

    /// Verify the barge-in suppression rule: suppress when outputActive and
    /// inputLevel < echoThreshold; pass through otherwise.
    #[test]
    fn barge_in_suppresses_quiet_input_during_output() {
        // Output active at 0.5, input at 0.02 (quiet):
        // echoThreshold = max(0.04, 0.5 * 0.65) = max(0.04, 0.325) = 0.325
        // 0.02 < 0.325 → suppress
        let out_level = 0.5;
        let input_level = 0.02;
        let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
        let echo_threshold = MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO);
        assert!(output_active);
        assert!(input_level < echo_threshold);
        assert!(output_active && input_level < echo_threshold); // suppress
    }

    #[test]
    fn barge_in_passes_loud_input_during_output() {
        // Output active at 0.5, input at 0.4 (loud):
        // echoThreshold = max(0.04, 0.325) = 0.325
        // 0.4 >= 0.325 → pass through (barge-in!)
        let out_level = 0.5;
        let input_level = 0.4;
        let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
        let echo_threshold = MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO);
        assert!(output_active);
        assert!(input_level >= echo_threshold); // not suppressed
    }

    #[test]
    fn barge_in_passes_all_input_when_output_inactive() {
        // Output at 0.01 (< 0.015), any input → not suppressed.
        let out_level = 0.01;
        let input_level = 0.001;
        let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
        assert!(!output_active);
        // When output is not active, the suppression condition is false
        // regardless of input level.
        assert!(
            !(output_active && input_level < MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO))
        );
    }

    #[test]
    fn barge_in_uses_min_threshold_when_output_low() {
        // Output active at 0.02 (> 0.015), input at 0.03:
        // echoThreshold = max(0.04, 0.02 * 0.65) = max(0.04, 0.013) = 0.04
        // 0.03 < 0.04 → suppress (MIN_BARGE_IN_LEVEL dominates)
        let out_level = 0.02;
        let input_level = 0.03;
        let output_active = out_level > OUTPUT_ACTIVE_LEVEL;
        let echo_threshold = MIN_BARGE_IN_LEVEL.max(out_level * OUTPUT_ECHO_RATIO);
        assert_eq!(echo_threshold, 0.04); // MIN dominates
        assert!(output_active);
        assert!(input_level < echo_threshold); // suppress
    }

    #[test]
    fn output_level_atomic_store_load_roundtrips() {
        let atomic = Arc::new(AtomicU64::new(0.0f64.to_bits()));
        store_output_level(&atomic, 0.5);
        assert!((load_output_level(&atomic) - 0.5).abs() < 1e-10);
        store_output_level(&atomic, 0.0);
        assert_eq!(load_output_level(&atomic), 0.0);
    }
}
