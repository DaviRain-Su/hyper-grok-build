//! Microphone capture on Linux via a subprocess recorder.
//!
//! The release CLI ships as a fully-static `*-unknown-linux-musl` binary, so it
//! cannot link `cpal` -> `alsa-sys` (a `NEEDED libasound.so.2`) without losing
//! the static guarantee enforced by the release build. Statically linking ALSA
//! is no help either: it reaches the user's real device (PulseAudio/PipeWire)
//! through plugins it loads via `dlopen`, which a static musl binary can't do.
//!
//! Instead, capture mic audio by spawning the system recorder (`pw-record`,
//! `parec`, or `arecord`) and reading raw PCM16 mono from its stdout — no native
//! audio library is linked into the binary at all. The recorders are asked for
//! signed 16-bit little-endian mono at the STT sample rate, which is exactly the
//! format the pipeline forwards, so there is no downmix/resample step.
//!
//! This module exposes the same interface as the `cpal` backend
//! (`spawn_pcm_capture`, `capture_pcm_for_duration`, `CaptureHandle`) so the
//! pipeline and probe are backend-agnostic.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::mpsc as async_mpsc;

use crate::error::VoiceError;

/// PCM read size from the recorder's stdout (bytes) — ~64 ms at 16 kHz mono
/// PCM16. Small enough to stream responsively, large enough to avoid syscall
/// churn on the reader thread.
const READ_CHUNK: usize = 2048;

/// How long to wait after spawning before deciding the recorder started cleanly.
/// A missing device or a stopped audio server makes the recorder exit within a
/// few ms; this surfaces that as an error instead of a session that "listens"
/// but never produces audio (mirrors the `cpal` backend's open handshake).
const START_GRACE: Duration = Duration::from_millis(300);

/// A system audio recorder that can stream raw PCM16 mono to stdout.
#[derive(Clone, Copy)]
enum Recorder {
    /// PipeWire's `pw-record`.
    PwRecord,
    /// PulseAudio's `parec`.
    Parec,
    /// ALSA's `arecord` (alsa-utils).
    Arecord,
}

impl Recorder {
    fn program(self) -> &'static str {
        match self {
            Recorder::PwRecord => "pw-record",
            Recorder::Parec => "parec",
            Recorder::Arecord => "arecord",
        }
    }

    /// Args that emit signed 16-bit little-endian mono PCM at `rate` Hz to
    /// stdout. (`pw-record`/`pw-cat` and `arecord` take an explicit `-` stdout
    /// target; `parec` writes raw to stdout by default.)
    fn args(self, rate: u32) -> Vec<String> {
        let rate = rate.to_string();
        match self {
            Recorder::PwRecord => vec![
                "--rate".into(),
                rate,
                "--channels".into(),
                "1".into(),
                "--format".into(),
                "s16".into(),
                "-".into(),
            ],
            Recorder::Parec => vec![
                "--raw".into(),
                "--format=s16le".into(),
                format!("--rate={rate}"),
                "--channels=1".into(),
            ],
            Recorder::Arecord => vec![
                "-q".into(),
                "-t".into(),
                "raw".into(),
                "-f".into(),
                "S16_LE".into(),
                "-c".into(),
                "1".into(),
                "-r".into(),
                rate,
                "-".into(),
            ],
        }
    }
}

/// First recorder found on `PATH`, preferring PipeWire > PulseAudio > ALSA so we
/// go through the user's configured audio server (and its default input device)
/// rather than grabbing a raw ALSA `hw:` device.
fn detect_recorder() -> Option<Recorder> {
    detect_recorder_with(binary_on_path)
}

/// [`detect_recorder`] with the `PATH` probe injected, so the preference order
/// is unit-testable without process-global `PATH` mutation.
fn detect_recorder_with(available: impl Fn(&str) -> bool) -> Option<Recorder> {
    [Recorder::PwRecord, Recorder::Parec, Recorder::Arecord]
        .into_iter()
        .find(|r| available(r.program()))
}

/// Whether `name` resolves to an executable regular file on any `PATH` entry
/// (so a stray non-executable file can't shadow a working recorder).
fn binary_on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        dir.join(name)
            .metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// The detected recorder, or a `VoiceError` naming the packages to install.
fn require_recorder() -> Result<Recorder, VoiceError> {
    detect_recorder().ok_or_else(|| {
        VoiceError::Config(
            "no microphone recorder found on PATH: install pipewire (pw-record), \
             pulseaudio-utils (parec), or alsa-utils (arecord)"
                .into(),
        )
    })
}

/// Spawn the chosen recorder with stdout/stderr piped, and confirm it didn't
/// exit immediately (no device, audio server down). On success the child is
/// running with `stdout` available for reading.
fn spawn_recorder(sample_rate: u32) -> Result<(Recorder, Child), VoiceError> {
    let recorder = require_recorder()?;

    let mut child = Command::new(recorder.program())
        .args(recorder.args(sample_rate))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| VoiceError::Config(format!("failed to start {}: {e}", recorder.program())))?;

    thread::sleep(START_GRACE);
    match child.try_wait() {
        Ok(Some(status)) => {
            let mut stderr = String::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_string(&mut stderr);
            }
            let stderr = stderr.trim();
            Err(VoiceError::Config(format!(
                "{} exited immediately ({status}){}",
                recorder.program(),
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                },
            )))
        }
        Ok(None) => Ok((recorder, child)),
        Err(e) => Err(VoiceError::Config(format!(
            "failed to poll {}: {e}",
            recorder.program()
        ))),
    }
}

/// Stop handle for the recorder subprocess (owns the child + reader thread).
pub struct CaptureHandle {
    /// `Some` until `stop()` or `Drop` consumes it (kill + reap).
    child: Option<Child>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    peak: Arc<AtomicU16>,
}

impl CaptureHandle {
    /// Session peak of recorder-delivered PCM (metered before load-shed).
    pub fn peak_meter(&self) -> Arc<AtomicU16> {
        Arc::clone(&self.peak)
    }

    /// Stop capture: kill the recorder, reap it, and join the reader thread so
    /// the input device is released before returning.
    ///
    /// Dropping a `CaptureHandle` also kills and reaps the recorder (see
    /// `Drop`), but without joining the reader; call `stop()` when you must be
    /// sure the device is freed before continuing.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        // Always kill the recorder so the mic is released even when `stop()` was
        // never called — e.g. the STT session ended on its own (server close /
        // error). Killing closes the child's stdout, so the reader thread's
        // blocking `read` returns 0 and it exits. `Drop` must never block (it
        // may run on an async executor), so the reap happens on a detached
        // thread — without it every drop-path teardown (session supersede, STT
        // error, connect failure) would leave a zombie until the pager exits.
        self.stop.store(true, Ordering::Release);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            // `Builder::spawn` (not `thread::spawn`) so spawn failure under
            // thread exhaustion degrades to kill-without-reap instead of a
            // panic — a panicking `Drop` during unwind would abort.
            let _ = thread::Builder::new()
                .name("voice-capture-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

/// Spawn subprocess capture; PCM16 LE chunks are forwarded to `pcm_tx`.
pub fn spawn_pcm_capture(
    sample_rate: u32,
    pcm_tx: async_mpsc::Sender<Vec<u8>>,
) -> Result<CaptureHandle, VoiceError> {
    let (recorder, mut child) = spawn_recorder(sample_rate)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| VoiceError::Config(format!("{} produced no stdout", recorder.program())))?;

    drain_stderr(&mut child, recorder.program());

    let stop = Arc::new(AtomicBool::new(false));
    let stop_reader = Arc::clone(&stop);
    let peak = Arc::new(AtomicU16::new(0));
    let peak_reader = Arc::clone(&peak);
    let device = recorder.program();
    let reader =
        thread::spawn(move || forward_pcm(stdout, pcm_tx, stop_reader, peak_reader, device));

    tracing::info!(
        recorder = recorder.program(),
        sample_rate,
        "voice capture stream (subprocess)"
    );

    Ok(CaptureHandle {
        child: Some(child),
        stop,
        reader: Some(reader),
        peak,
    })
}

/// Drain the recorder's stderr to EOF on a detached thread so a chatty recorder
/// (xrun/underrun warnings, etc.) can't fill the pipe buffer and block its own
/// writes — which would stall capture, since the hot path never reads stderr.
/// Non-empty output is logged at debug for diagnostics. The thread ends on its
/// own when the child exits (EOF), so it is not joined.
fn drain_stderr(child: &mut Child, device: &'static str) {
    let Some(mut stderr) = child.stderr.take() else {
        return;
    };
    thread::spawn(move || {
        let mut buf = String::new();
        if stderr.read_to_string(&mut buf).is_ok() {
            let msg = buf.trim();
            if !msg.is_empty() {
                tracing::debug!(device, stderr = msg, "voice recorder stderr");
            }
        }
    });
}

/// Forward raw PCM from the recorder's stdout to the async STT sender until the
/// recorder stops (EOF on kill), the consumer goes away, or `stop` is set.
/// Generic over the reader for tests; production passes the child's stdout.
fn forward_pcm(
    mut stdout: impl Read,
    pcm_tx: async_mpsc::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU16>,
    device: &'static str,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut dropped = 0u64;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match stdout.read(&mut buf) {
            // EOF: the recorder closed stdout (killed by teardown or exited).
            Ok(0) => break,
            Ok(n) => {
                // Before try_send: shed chunks must still move the peak meter.
                peak.fetch_max(crate::pcm::peak_abs_i16_le(&buf[..n]), Ordering::Relaxed);
                // Never park this thread on the channel: `stop()` joins it, so a
                // send that waits on a stalled STT consumer would turn teardown
                // into a hang. Shed load instead when the consumer is behind —
                // the same strategy as the cpal backend's real-time callback.
                // (`read` itself is unblocked by the kill-on-stop path: killing
                // the recorder closes stdout, so a waiting `read` returns 0.)
                match pcm_tx.try_send(buf[..n].to_vec()) {
                    Ok(()) => {}
                    Err(async_mpsc::error::TrySendError::Full(_)) => dropped += 1,
                    // Consumer is gone: the session ended; stop capturing.
                    Err(async_mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            Err(e) => {
                tracing::warn!(device, error = %e, "voice capture read error");
                break;
            }
        }
    }
    if dropped > 0 {
        tracing::warn!(
            device,
            dropped,
            "voice capture dropped PCM chunks (slow consumer)"
        );
    }
}

/// Recorder that would be spawned, without recording ([`crate::probe::input_device_info`]).
pub fn input_device_info() -> Result<crate::probe::InputDeviceInfo, VoiceError> {
    let recorder = require_recorder()?;
    Ok(crate::probe::InputDeviceInfo {
        name: recorder.program().to_string(),
        detail: "system recorder; uses the audio server's default input".to_string(),
    })
}

/// Record mono PCM16 LE for a fixed duration (probe / diagnostics).
pub fn capture_pcm_for_duration(
    sample_rate: u32,
    seconds: u32,
) -> Result<(Vec<u8>, u32), VoiceError> {
    let (recorder, mut child) = spawn_recorder(sample_rate)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| VoiceError::Config(format!("{} produced no stdout", recorder.program())))?;
    drain_stderr(&mut child, recorder.program());

    let duration = Duration::from_secs(seconds.max(1) as u64);
    let deadline = Instant::now() + duration;

    // Watchdog: kill the recorder at the deadline so a `read` that is blocked
    // waiting for PCM (recorder alive but idle / stalled pipe) gets EOF instead
    // of running past the requested duration. Killing at the deadline also ends
    // a healthy capture, so the read loop below needs no between-read deadline
    // check beyond its backstop.
    // Deliberately not joined: if the recorder dies early we return without
    // waiting out the full duration, and the watchdog's late `kill` on an
    // already-reaped `Child` is a harmless `InvalidInput` (std tracks the reap,
    // so no PID-reuse hazard).
    let child = Arc::new(Mutex::new(child));
    let watchdog_child = Arc::clone(&child);
    thread::spawn(move || {
        thread::sleep(duration);
        let mut child = watchdog_child.lock().expect("watchdog lock poisoned");
        let _ = child.kill();
    });

    let mut pcm = Vec::new();
    let mut chunks = 0u32;
    let mut buf = vec![0u8; READ_CHUNK];
    // Small slack past the deadline: the kill's EOF (`Ok(0)`) is the intended
    // exit; the time check is a backstop against a pathological pipe.
    while Instant::now() < deadline + Duration::from_secs(1) {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                chunks += 1;
                pcm.extend_from_slice(&buf[..n]);
            }
            Err(_) => break,
        }
    }

    {
        let mut child = child.lock().expect("child lock poisoned");
        let _ = child.kill();
        let _ = child.wait();
    }
    Ok((pcm, chunks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_pcm_meters_shed_chunks() {
        // One loud sample per READ_CHUNK read; capacity 1 forces the second
        // read to shed. Both must register in the peak meter.
        let mut pcm = vec![0u8; 2 * READ_CHUNK];
        pcm[..2].copy_from_slice(&5_000i16.to_le_bytes());
        pcm[READ_CHUNK..READ_CHUNK + 2].copy_from_slice(&(-9_000i16).to_le_bytes());

        let (tx, mut rx) = async_mpsc::channel::<Vec<u8>>(1);
        let peak = Arc::new(AtomicU16::new(0));
        forward_pcm(
            std::io::Cursor::new(pcm),
            tx,
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&peak),
            "test",
        );

        assert_eq!(peak.load(Ordering::Relaxed), 9_000);
        let first = rx.try_recv().expect("first chunk forwarded");
        assert_eq!(crate::pcm::peak_abs_i16_le(&first), 5_000);
        assert!(rx.try_recv().is_err(), "second chunk shed (channel full)");
    }

    #[test]
    fn arecord_args_are_raw_s16_mono() {
        let args = Recorder::Arecord.args(16_000);
        assert!(args.contains(&"S16_LE".to_string()));
        assert!(args.contains(&"raw".to_string()));
        // mono
        let c = args.iter().position(|a| a == "-c").unwrap();
        assert_eq!(args[c + 1], "1");
        // rate
        let r = args.iter().position(|a| a == "-r").unwrap();
        assert_eq!(args[r + 1], "16000");
        // stdout target
        assert_eq!(args.last().unwrap(), "-");
    }

    #[test]
    fn parec_and_pw_args_carry_rate_format_and_mono() {
        let parec = Recorder::Parec.args(24_000);
        assert!(parec.contains(&"--raw".to_string()));
        assert!(parec.contains(&"--format=s16le".to_string()));
        assert!(parec.contains(&"--rate=24000".to_string()));
        assert!(parec.contains(&"--channels=1".to_string()));

        let pw = Recorder::PwRecord.args(48_000);
        let r = pw.iter().position(|a| a == "--rate").unwrap();
        assert_eq!(pw[r + 1], "48000");
        let f = pw.iter().position(|a| a == "--format").unwrap();
        assert_eq!(pw[f + 1], "s16");
        let c = pw.iter().position(|a| a == "--channels").unwrap();
        assert_eq!(pw[c + 1], "1");
        assert_eq!(pw.last().unwrap(), "-"); // stdout target
    }

    #[test]
    fn recorder_preference_is_pipewire_then_pulse_then_alsa() {
        // All present: PipeWire wins (routes through the user's audio server).
        let all = detect_recorder_with(|_| true);
        assert!(matches!(all, Some(Recorder::PwRecord)));

        // No PipeWire: PulseAudio next.
        let no_pw = detect_recorder_with(|p| p != "pw-record");
        assert!(matches!(no_pw, Some(Recorder::Parec)));

        // alsa-utils only: arecord is the last resort.
        let alsa_only = detect_recorder_with(|p| p == "arecord");
        assert!(matches!(alsa_only, Some(Recorder::Arecord)));

        assert!(detect_recorder_with(|_| false).is_none());
    }
}
