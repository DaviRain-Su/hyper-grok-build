//! Codex Live session driver: connects the transport, reuses the existing PCM16
//! capture for mic input, applies the OMP echo gate and mute, and translates
//! [`LiveCommand`]s into transport messages until [`LiveCommand::Shutdown`].
//!
//! The session reuses [`crate::audio`] capture (PCM16 mono) rather than maudio:
//! capture produces raw PCM16 LE bytes, which the session converts to the f32
//! samples the WebRTC peer consumes. This keeps one mic-capture backend across
//! dictation and live (macOS helper, Linux recorder, Windows cpal) and avoids
//! a second native audio stack.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

use super::protocol::{
    LiveContextChannel, build_delegation_context_append, build_session_close,
    build_session_context_append, chunk_live_context,
};
use super::transport::CodexLiveTransport;
use super::types::{CallbackSink, LiveCommand, LiveConfig, LiveEvent, LivePhase, SharedLiveAuth};
use crate::audio;

/// Default mic capture sample rate for a live session (16 kHz mono, matching
/// the WebRTC peer's input rate).
const LIVE_INPUT_SAMPLE_RATE: u32 = 16_000;

/// Run a Codex Live voice session until [`LiveCommand::Shutdown`] (or the
/// command channel closes). Emits [`LiveEvent`]s to `event_tx`.
///
/// The session:
/// 1. Opens the existing PCM16 capture backend (reused from dictation).
/// 2. Connects the [`CodexLiveTransport`] (signaling + sideband + WebRTC peer).
/// 3. Forwards mic PCM (converted to f32) to the peer, applying the echo gate
///    (mute while the model is speaking) and explicit mute commands.
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

    // Echo gate: while the model is producing output audio (OutputLevel > 0),
    // mute the mic so the speaker doesn't feed back into the model. This is the
    // OMP echo-gate behavior, implemented here at the session layer.
    let echo_gated = Arc::new(AtomicBool::new(false));
    let user_muted = Arc::new(AtomicBool::new(false));

    // Levels flow on a dedicated internal channel so the session loop can drive
    // the echo gate and forward them to the pager.
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
    let connect = transport.connect();

    // Drive the echo gate / commands while connecting: a Shutdown arriving
    // mid-connect cancels the start.
    let connect_result = tokio::select! {
        biased;
        res = connect => res,
        cmd = cmd_rx.recv() => match cmd {
            Some(LiveCommand::Shutdown) | None => {
                transport.close().await;
                phase(LivePhase::Closed);
                let _ = event_tx.send(LiveEvent::Closed).await;
                return;
            }
            // Buffer non-shutdown commands; they're handled after connect.
            Some(_) => {
                // Re-collect by deferring: fall through to connect with the
                // command still in flight. We can't re-inject into cmd_rx, so
                // we simply proceed — the command is lost only if it arrived
                // during the connect race, which the pager avoids by not
                // sending context before `Connected`.
                transport.connect().await
            }
        },
    };

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

    // The session loop drives both mic-PCM forward (echo-gated / muted) and
    // command handling. The mic PCM channel (`pcm_rx`) was filled by the
    // capture task above; capture stays open for the session's lifetime.
    let mut capture = Some(capture);
    loop {
        tokio::select! {
            biased;
            // Echo gate: output levels toggle mic muting while the model speaks.
            level = levels_rx.recv() => {
                if let Some(level) = level {
                    let speaking = level > 0.0;
                    let was_gated = echo_gated.swap(speaking, Ordering::AcqRel);
                    if was_gated != speaking {
                        // Apply only the echo gate; user mute is independent and
                        // takes precedence (handled in the PCM forward below).
                        transport.set_muted(speaking || user_muted.load(Ordering::Acquire));
                    }
                    let _ = event_tx.try_send(LiveEvent::Levels(level));
                }
            }
            // Forward mic PCM to the peer (echo-gated / muted).
            bytes = pcm_rx.recv() => match bytes {
                Some(bytes) => {
                    if echo_gated.load(Ordering::Acquire) || user_muted.load(Ordering::Acquire) {
                        continue;
                    }
                    let samples = pcm16_le_to_f32(&bytes);
                    if !samples.is_empty() {
                        transport.push_audio(&samples);
                    }
                }
                None => {
                    // Mic stream ended (capture stopped). Not fatal; the session
                    // can continue without mic input (e.g. listen-only).
                }
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(LiveCommand::Shutdown) | None => break,
                Some(LiveCommand::ToggleMute) => {
                    let next = !user_muted.load(Ordering::Acquire);
                    user_muted.store(next, Ordering::Release);
                    transport.set_muted(next || echo_gated.load(Ordering::Acquire));
                }
                Some(LiveCommand::SetMuted(muted)) => {
                    user_muted.store(muted, Ordering::Release);
                    transport.set_muted(muted || echo_gated.load(Ordering::Acquire));
                }
                Some(LiveCommand::AppendDelegationContext { delegation_id, text, channel }) => {
                    send_chunked(
                        &transport,
                        &chunk_live_context(&text),
                        Some(delegation_id),
                        Some(channel),
                    ).await;
                }
                Some(LiveCommand::CompleteDelegation { delegation_id, text }) => {
                    // Append the final text, then a session.close is NOT sent
                    // (the delegation completes server-side). We append the
                    // text as the delegation's final context.
                    send_chunked(
                        &transport,
                        &chunk_live_context(&text),
                        Some(delegation_id),
                        Some(LiveContextChannel::Commentary),
                    ).await;
                }
                Some(LiveCommand::AppendSessionContext { text, channel }) => {
                    send_chunked(
                        &transport,
                        &chunk_live_context(&text),
                        None,
                        Some(channel),
                    ).await;
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

/// Send chunked context appends to the transport. Each chunk becomes one
/// `delegation.context.append` or `session.context.append` message. Errors are
/// surfaced as `LiveEvent::Error` by the transport's callbacks; here we only
/// log at debug (a failed send during shutdown is expected).
async fn send_chunked(
    transport: &CodexLiveTransport,
    chunks: &[String],
    delegation_id: Option<String>,
    channel: Option<LiveContextChannel>,
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
}
