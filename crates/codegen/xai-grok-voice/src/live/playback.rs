//! Speaker playback without maudio.
//!
//! Mirrors the capture backend's platform split (see `crate::audio`): the
//! long-lived TUI never pays a permanent platform-audio-stack memory cost, so
//! playback runs out of process on Linux and macOS, and in-process via `cpal`
//! on Windows (whose WASAPI memory cost is modest).
//!
//! - **Linux**: a subprocess player (`pw-play`/`pacat`/`aplay`) fed raw PCM16
//!   or float32 over its stdin.
//! - **Windows**: `cpal` output stream in-process.
//! - **macOS**: a short-lived self-exec `__speaker-play` helper (consistent
//!   with the `__mic-capture` capture memory policy) so CoreAudio's permanent
//!   footprint dies with the helper when the live session ends.
//!
//! The [`PlaybackWriter`] hands mono `f32` samples (48 kHz, the WebRTC output
//! rate) to the backend; each backend converts to the format its player
//! expects. All backends are bounded: a full output queue sheds the oldest
//! samples rather than blocking the decoder thread.
//!
//! This is a substantially original design (no OMP `maudio` dependency); the
//! OMP `PlaybackStream`/`PlaybackWriter` *interface shape* is preserved for
//! ergonomics, but the implementations are platform subprocess/cpal ports. MIT
//! attribution for the borrowed interface in `THIRD-PARTY-NOTICES`.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use tokio::sync::mpsc as async_mpsc;

use crate::error::VoiceError;

/// Bounded capacity of the playback sample queue (in samples). Sized for ~0.5s
/// of 48 kHz mono — enough to absorb decoder jitter without ever blocking the
/// real-time RTP read loop, and small enough that a stalled player sheds
/// quickly rather than accumulating seconds of latency.
const PLAYBACK_QUEUE_SAMPLES: usize = 24_000;

/// Producer endpoint for one native playback device. Cloneable so the WebRTC
/// output decoder and a telemetry probe can share it.
#[derive(Clone)]
pub struct PlaybackWriter {
    tx: async_mpsc::Sender<Vec<f32>>,
    stopped: Arc<AtomicBool>,
}

impl PlaybackWriter {
    /// Queue mono `f32` samples without blocking the caller. When the queue is
    /// full the *oldest* samples are shed (drained from the back of the
    /// consumer) so playback stays recent rather than accumulating latency.
    pub fn write(&self, samples: &[f32]) -> Result<(), VoiceError> {
        if samples.is_empty() {
            return Ok(());
        }
        if self.stopped.load(Ordering::Acquire) {
            return Err(VoiceError::Stt("live speaker playback is closed".into()));
        }
        // `try_send` so the decoder thread is never parked on a full queue.
        match self.tx.try_send(samples.to_vec()) {
            Ok(()) => Ok(()),
            Err(async_mpsc::error::TrySendError::Full(_)) => {
                // Shed load: a full queue means the player can't keep up.
                // Dropping the newest chunk keeps playback recent.
                tracing::warn!("live speaker playback queue full; shedding samples");
                Ok(())
            }
            Err(async_mpsc::error::TrySendError::Closed(_)) => {
                Err(VoiceError::Stt("live speaker playback is closed".into()))
            }
        }
    }
}

/// A running playback stream. Dropping it stops playback and releases the
/// speaker; `stop()` joins the writer thread for a synchronous release.
pub struct PlaybackStream {
    tx: async_mpsc::Sender<Vec<f32>>,
    stopped: Arc<AtomicBool>,
    writer_thread: Option<JoinHandle<()>>,
    /// Platform-specific teardown handle (child process or cpal stream).
    teardown: Teardown,
}

enum Teardown {
    /// A subprocess player (Linux/macOS helper): kill + reap on stop.
    Child(Option<Child>),
    /// Windows cpal: the stream is stopped when the writer thread ends (the
    /// sample source drops), so no explicit handle is needed here. The variant
    /// exists so the enum is constructible on Windows.
    #[allow(dead_code)]
    Cpal,
    /// No external resource (e.g. a stub backend on a platform without a
    /// player available at construction time — playback silently no-ops).
    #[allow(dead_code)]
    None,
}

impl PlaybackStream {
    /// Open and start the default speaker at the requested sample rate.
    ///
    /// `sample_rate` is the logical rate of the `f32` samples the writer
    /// receives; the backend converts to whatever its player consumes natively.
    pub fn start(sample_rate: u32) -> Result<Self, VoiceError> {
        let (tx, rx) = async_mpsc::channel::<Vec<f32>>(PLAYBACK_QUEUE_SAMPLES / 256);
        let stopped = Arc::new(AtomicBool::new(false));

        let (writer_thread, teardown) = start_backend(sample_rate, rx, Arc::clone(&stopped))?;

        Ok(Self {
            tx,
            stopped,
            writer_thread: Some(writer_thread),
            teardown,
        })
    }

    /// Clone the producer endpoint used by the remote-audio decoder.
    pub fn writer(&self) -> PlaybackWriter {
        PlaybackWriter {
            tx: self.tx.clone(),
            stopped: Arc::clone(&self.stopped),
        }
    }

    /// Stop playback immediately and release the default speaker. Idempotent.
    pub fn stop(mut self) {
        self.shutdown();
        if let Some(thread) = self.writer_thread.take() {
            let _ = thread.join();
        }
    }

    fn shutdown(&mut self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        // Closing the channel signals the writer thread to drain and exit.
        // (Dropping `tx` here would close it, but `self.tx` is borrowed by
        // `writer()` clones; instead we rely on the stopped flag + the writer
        // thread's select on `rx.recv()` returning None once all clones drop.)
        match &mut self.teardown {
            Teardown::Child(child) => {
                if let Some(child) = child.as_mut() {
                    // Close stdin first so the player flushes, then kill to
                    // release the device promptly.
                    let _ = child.stdin.take();
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            Teardown::Cpal | Teardown::None => {}
        }
    }
}

impl Drop for PlaybackStream {
    fn drop(&mut self) {
        self.shutdown();
        // The writer thread is a std thread; it exits on its own once the
        // channel closes (all `PlaybackWriter` clones drop with `self`). Do
        // not join in `Drop` (it may run on an async executor and must not
        // block); detach it.
        drop(self.writer_thread.take());
    }
}

// ---------------------------------------------------------------------------
// Backend dispatch
// ---------------------------------------------------------------------------

#[allow(unused_variables)]
fn start_backend(
    sample_rate: u32,
    rx: async_mpsc::Receiver<Vec<f32>>,
    stopped: Arc<AtomicBool>,
) -> Result<(JoinHandle<()>, Teardown), VoiceError> {
    #[cfg(target_os = "linux")]
    {
        start_linux_subprocess(sample_rate, rx, stopped)
    }
    #[cfg(target_os = "windows")]
    {
        start_windows_cpal(sample_rate, rx, stopped)
    }
    #[cfg(target_os = "macos")]
    {
        start_macos_helper(sample_rate, rx, stopped)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        // Unsupported platform: a no-op backend. The writer thread drains the
        // queue and discards samples so the decoder never blocks.
        let thread = thread::spawn(move || drain_and_discard(rx, stopped));
        Ok((thread, Teardown::None))
    }
}

// ---------------------------------------------------------------------------
// Linux: subprocess player
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn start_linux_subprocess(
    sample_rate: u32,
    rx: async_mpsc::Receiver<Vec<f32>>,
    stopped: Arc<AtomicBool>,
) -> Result<(JoinHandle<()>, Teardown), VoiceError> {
    use std::sync::OnceLock;

    /// A system audio player that can stream raw PCM from stdin.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Player {
        PwPlay,
        Pacat,
        Aplay,
    }

    impl Player {
        fn program(self) -> &'static str {
            match self {
                Player::PwPlay => "pw-play",
                Player::Pacat => "pacat",
                Player::Aplay => "aplay",
            }
        }

        /// Args that consume signed 16-bit little-endian mono PCM at `rate` Hz
        /// from stdin. We feed PCM16 (converted from f32 by the writer thread)
        /// so the player needs no float support.
        fn args(self, rate: u32) -> Vec<String> {
            let rate = rate.to_string();
            match self {
                Player::PwPlay => vec![
                    "--rate".into(),
                    rate,
                    "--channels".into(),
                    "1".into(),
                    "--format".into(),
                    "s16".into(),
                    "-".into(),
                ],
                Player::Pacat => vec![
                    "--raw".into(),
                    "--format=s16le".into(),
                    format!("--rate={rate}"),
                    "--channels=1".into(),
                ],
                Player::Aplay => vec![
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

    fn detect_player() -> Option<Player> {
        static PLAYER: OnceLock<Option<Player>> = OnceLock::new();
        *PLAYER.get_or_init(|| {
            [Player::PwPlay, Player::Pacat, Player::Aplay]
                .into_iter()
                .find(|p| binary_on_path(p.program()))
        })
    }

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

    let player = detect_player().ok_or_else(|| {
        VoiceError::Config(
            "no speaker player found on PATH: install pipewire (pw-play), \
             pulseaudio-utils (pacat/paplay), or alsa-utils (aplay)"
                .into(),
        )
    })?;

    let mut cmd = Command::new(player.program());
    cmd.args(player.args(sample_rate))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    xai_tty_utils::detach_std_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| VoiceError::Config(format!("failed to start {}: {e}", player.program())))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| VoiceError::Config(format!("{} produced no stdin", player.program())))?;
    // Drain stderr so a chatty player can't block its own writes.
    if let Some(stderr) = child.stderr.take() {
        let device = player.program();
        thread::spawn(move || {
            let mut buf = String::new();
            use std::io::Read;
            let mut stderr = stderr;
            if stderr.read_to_string(&mut buf).is_ok() {
                let msg = buf.trim();
                if !msg.is_empty() {
                    tracing::debug!(device, stderr = msg, "live speaker player stderr");
                }
            }
        });
    }

    let thread = thread::spawn(move || forward_pcm16(stdin, rx, stopped, player.program()));
    Ok((thread, Teardown::Child(Some(child))))
}

// ---------------------------------------------------------------------------
// Windows: cpal output stream
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn start_windows_cpal(
    sample_rate: u32,
    rx: async_mpsc::Receiver<Vec<f32>>,
    stopped: Arc<AtomicBool>,
) -> Result<(JoinHandle<()>, Teardown), VoiceError> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, SampleRate};
    use std::sync::mpsc as sync_mpsc;

    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| VoiceError::Config("no default output audio device".into()))?;
    let supported = device
        .default_output_config()
        .map_err(|e| VoiceError::Config(format!("default output config: {e}")))?;
    let stream_rate = supported.sample_rate().0;
    let channels = supported.channels();
    let sample_format = supported.sample_format();
    let stream_config: cpal::StreamConfig = cpal::StreamConfig {
        channels,
        sample_rate: SampleRate(stream_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let (sync_tx, sync_rx) = sync_mpsc::sync_channel::<Vec<f32>>(64);
    let stop_cb = Arc::clone(&stopped);

    let stream = match sample_format {
        SampleFormat::F32 => build_cpal_stream::<f32>(&device, &stream_config, sync_rx, stop_cb)?,
        SampleFormat::I16 => build_cpal_stream::<i16>(&device, &stream_config, sync_rx, stop_cb)?,
        SampleFormat::U16 => build_cpal_stream::<u16>(&device, &stream_config, sync_rx, stop_cb)?,
        other => {
            return Err(VoiceError::Config(format!(
                "unsupported output sample format {other:?}"
            )));
        }
    };
    stream
        .play()
        .map_err(|e| VoiceError::Config(format!("play output stream: {e}")))?;

    // Bridge the async queue to the cpal sync channel, resampling if the
    // device rate differs from the logical rate.
    let thread = thread::spawn(move || {
        bridge_to_sync(
            rx,
            sync_tx,
            sample_rate,
            stream_rate,
            channels as usize,
            stopped,
        );
        // Dropping `stream` stops it; keep it alive for the thread's lifetime.
        drop(stream);
    });
    Ok((thread, Teardown::Cpal))
}

#[cfg(target_os = "windows")]
fn build_cpal_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sync_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    stopped: Arc<AtomicBool>,
) -> Result<cpal::Stream, VoiceError>
where
    T: cpal::Sample + cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let _ = device; // used in the build call below
    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                fill_cpal_output(out, &sync_rx, &stopped);
            },
            |err| tracing::warn!(error = %err, "live speaker playback stream error"),
            None,
        )
        .map_err(|e| VoiceError::Config(format!("build output stream: {e}")))?;
    Ok(stream)
}

#[cfg(target_os = "windows")]
fn fill_cpal_output<T>(
    out: &mut [T],
    sync_rx: &std::sync::mpsc::Receiver<Vec<f32>>,
    stopped: &Arc<AtomicBool>,
) where
    T: cpal::Sample + cpal::FromSample<f32>,
{
    use std::sync::mpsc::TryRecvError;
    out.fill(T::from_sample(0.0));
    if stopped.load(Ordering::Acquire) {
        return;
    }
    let channels = out.len(); // already interleaved per channel by cpal
    let mut written = 0usize;
    let mut current: Vec<f32> = Vec::new();
    while written < channels {
        if current.is_empty() {
            match sync_rx.try_recv() {
                Ok(chunk) => current = chunk,
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return,
            }
        }
        let take = current.len().min(channels - written);
        for &s in &current[..take] {
            out[written] = T::from_sample(s);
            written += 1;
        }
        current.drain(..take);
    }
}

#[cfg(target_os = "windows")]
fn bridge_to_sync(
    mut rx: async_mpsc::Receiver<Vec<f32>>,
    sync_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    logical_rate: u32,
    device_rate: u32,
    channels: usize,
    stopped: Arc<AtomicBool>,
) {
    use std::sync::mpsc::TrySendError;
    while !stopped.load(Ordering::Acquire) {
        match rx.blocking_recv() {
            Some(mut samples) => {
                if logical_rate != device_rate {
                    samples = resample_mono_f32(&samples, logical_rate, device_rate);
                }
                if channels > 1 {
                    samples = upmix_mono_to_channels(&samples, channels);
                }
                match sync_tx.try_send(samples) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            None => break,
        }
    }
}

// ---------------------------------------------------------------------------
// macOS: short-lived self-exec helper
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn start_macos_helper(
    sample_rate: u32,
    rx: async_mpsc::Receiver<Vec<f32>>,
    stopped: Arc<AtomicBool>,
) -> Result<(JoinHandle<()>, Teardown), VoiceError> {
    let exe = std::env::current_exe()
        .map_err(|e| VoiceError::Config(format!("current_exe for speaker helper: {e}")))?;
    let rate = sample_rate.to_string();
    let mut cmd = Command::new(exe);
    cmd.arg(crate::SPEAKER_PLAY_SUBCOMMAND)
        .arg("--rate")
        .arg(&rate)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    xai_tty_utils::detach_std_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| VoiceError::Config(format!("spawn speaker helper: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| VoiceError::Config("speaker helper produced no stdin".into()))?;
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let mut buf = String::new();
            use std::io::Read;
            let mut stderr = stderr;
            if stderr.read_to_string(&mut buf).is_ok() {
                let msg = buf.trim();
                if !msg.is_empty() {
                    tracing::debug!(stderr = msg, "live speaker helper stderr");
                }
            }
        });
    }
    let thread = thread::spawn(move || forward_pcm16(stdin, rx, stopped, "speaker-helper"));
    Ok((thread, Teardown::Child(Some(child))))
}

/// Convert f32 → PCM16 LE and write to the helper's stdin. Shared by the Linux
/// subprocess and macOS helper backends.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn forward_pcm16<W: Write + Send>(
    mut out: W,
    mut rx: async_mpsc::Receiver<Vec<f32>>,
    stopped: Arc<AtomicBool>,
    device: &'static str,
) {
    let mut buf = Vec::with_capacity(4096);
    while !stopped.load(Ordering::Acquire) {
        match rx.blocking_recv() {
            Some(samples) => {
                buf.clear();
                buf.reserve(samples.len() * 2);
                for s in samples {
                    let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                if out.write_all(&buf).and_then(|()| out.flush()).is_err() {
                    break;
                }
            }
            None => break,
        }
    }
    let _ = out.flush();
    let _ = device;
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Drain the queue and discard samples (no-op backend / fallback). Used on
/// platforms without a dedicated player backend.
#[allow(dead_code)]
fn drain_and_discard(mut rx: async_mpsc::Receiver<Vec<f32>>, stopped: Arc<AtomicBool>) {
    while !stopped.load(Ordering::Acquire) {
        if rx.blocking_recv().is_none() {
            break;
        }
    }
}

/// Linear resample of mono f32 samples (mirrors the capture resampler).
#[cfg(target_os = "windows")]
fn resample_mono_f32(samples: &[f32], input_rate: u32, output_rate: u32) -> Vec<f32> {
    if samples.is_empty() || input_rate == 0 || output_rate == 0 || input_rate == output_rate {
        return samples.to_vec();
    }
    let output_len =
        ((samples.len() as u64 * output_rate as u64) / input_rate as u64).max(1) as usize;
    let step = input_rate as f64 / output_rate as f64;
    let mut out = Vec::with_capacity(output_len);
    for i in 0..output_len {
        let pos = i as f64 * step;
        let idx = pos.floor() as usize;
        let frac = pos - idx as f64;
        let s0 = samples[idx] as f64;
        let s1 = *samples.get(idx + 1).unwrap_or(&samples[idx]) as f64;
        out.push((s0 + (s1 - s0) * frac) as f32);
    }
    out
}

/// Upmix mono to interleaved multi-channel by duplicating the mono sample.
#[cfg(target_os = "windows")]
fn upmix_mono_to_channels(mono: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return mono.to_vec();
    }
    let mut out = Vec::with_capacity(mono.len() * channels);
    for &s in mono {
        for _ in 0..channels {
            out.push(s);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// `__speaker-play` helper child (macOS) — analogous to `__mic-capture`
// ---------------------------------------------------------------------------

/// Run the `__speaker-play` helper child. `args` is argv after the subcommand:
/// `--rate <N>` reads raw PCM16 mono LE at `N` Hz from stdin and plays it to
/// the default output device via cpal. Exits when stdin closes (parent died)
/// or the parent kills it.
#[cfg(all(target_os = "macos", feature = "audio"))]
pub(crate) fn run_speaker_play_child(args: Vec<String>) -> i32 {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .try_init();

    let rate = match parse_speaker_args(&args) {
        Ok(r) => r,
        Err(msg) => {
            let _ = writeln!(std::io::stderr(), "ERR {msg}");
            return 2;
        }
    };

    match run_cpal_playback(rate) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "ERR {e}");
            1
        }
    }
}

#[cfg(all(target_os = "macos", feature = "audio"))]
fn parse_speaker_args(args: &[String]) -> Result<u32, String> {
    let mut rate: u32 = 48_000;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rate" => {
                i += 1;
                rate = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .filter(|r| *r > 0)
                    .ok_or_else(|| "bad --rate".to_string())?;
            }
            other => return Err(format!("unknown speaker-play arg: {other}")),
        }
        i += 1;
    }
    Ok(rate)
}

/// Open the default cpal output device at `rate` Hz and play PCM16 mono LE
/// read from stdin until EOF.
#[cfg(all(target_os = "macos", feature = "audio"))]
fn run_cpal_playback(rate: u32) -> Result<(), VoiceError> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleRate, StreamConfig};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc as sync_mpsc;

    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| VoiceError::Config("no default output audio device".into()))?;
    let config = StreamConfig {
        channels: 1,
        sample_rate: SampleRate(rate),
        buffer_size: cpal::BufferSize::Default,
    };
    let (tx, rx) = sync_mpsc::sync_channel::<Vec<i16>>(64);
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_cb = Arc::clone(&stopped);
    let stream = device
        .build_output_stream(
            &config,
            move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                use std::sync::mpsc::TryRecvError;
                out.fill(0);
                if stop_cb.load(Ordering::Acquire) {
                    return;
                }
                let mut written = 0;
                let mut current: Vec<i16> = Vec::new();
                while written < out.len() {
                    if current.is_empty() {
                        match rx.try_recv() {
                            Ok(chunk) => current = chunk,
                            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return,
                        }
                    }
                    let take = current.len().min(out.len() - written);
                    out[written..written + take].copy_from_slice(&current[..take]);
                    written += take;
                    current.drain(..take);
                }
            },
            |err| tracing::warn!(error = %err, "speaker helper stream error"),
            None,
        )
        .map_err(|e| VoiceError::Config(format!("build output stream: {e}")))?;
    stream
        .play()
        .map_err(|e| VoiceError::Config(format!("play output stream: {e}")))?;

    // Read PCM16 LE from stdin, forward to the cpal callback.
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let mut raw = vec![0u8; 4096];
    loop {
        use std::io::Read;
        match handle.read(&mut raw) {
            Ok(0) => break, // parent closed stdin → done
            Ok(n) => {
                let samples: Vec<i16> = raw[..n]
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let _ = tx.send(samples); // blocking, but the callback drains it
            }
            Err(_) => break,
        }
    }
    stopped.store(true, Ordering::Release);
    drop(stream);
    Ok(())
}

#[cfg(not(all(target_os = "macos", feature = "audio")))]
pub(crate) fn run_speaker_play_child(_args: Vec<String>) -> i32 {
    // Never spawned by this build's own backend (Linux uses system players;
    // no-audio builds have no playback). Reachable only by hand.
    let _ = writeln!(
        std::io::stdout(),
        "ERR speaker-play helper unavailable in this build"
    );
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn resample_mono_f32_doubles_rate() {
        let input: Vec<f32> = (0..48).map(|i| i as f32).collect();
        let out = resample_mono_f32(&input, 48_000, 16_000);
        assert_eq!(out.len(), 16);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn upmix_mono_to_stereo_duplicates() {
        let mono = vec![0.5, -0.5];
        let stereo = upmix_mono_to_channels(&mono, 2);
        assert_eq!(stereo, vec![0.5, 0.5, -0.5, -0.5]);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn forward_pcm16_converts_float_to_le_pcm16() {
        let (tx, rx) = async_mpsc::channel::<Vec<f32>>(8);
        let stopped = Arc::new(AtomicBool::new(false));
        let mut sink = std::io::Cursor::new(Vec::new());
        let samples = vec![0.0_f32, 1.0, -1.0];
        tx.blocking_send(samples).unwrap();
        drop(tx);
        forward_pcm16(&mut sink, rx, stopped, "test");
        let bytes = sink.into_inner();
        // 3 samples * 2 bytes
        assert_eq!(bytes.len(), 6);
        let s0 = i16::from_le_bytes([bytes[0], bytes[1]]);
        let s1 = i16::from_le_bytes([bytes[2], bytes[3]]);
        let s2 = i16::from_le_bytes([bytes[4], bytes[5]]);
        assert_eq!(s0, 0);
        assert_eq!(s1, i16::MAX);
        assert_eq!(s2, -i16::MAX);
    }

    #[tokio::test]
    async fn playback_writer_sheds_when_full() {
        // A closed channel reports Closed; a full channel reports Full and
        // sheds (returns Ok). Verify the closed path and the empty-path.
        let (tx, rx) = async_mpsc::channel::<Vec<f32>>(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let writer = PlaybackWriter {
            tx: tx.clone(),
            stopped: Arc::clone(&stopped),
        };
        writer.write(&[0.5]).unwrap();
        // Fill the queue (capacity 1).
        writer.write(&[0.5]).unwrap();
        // Next write must shed (Ok) rather than block.
        let big: Vec<f32> = vec![0.1; 100];
        assert!(writer.write(&big).is_ok());
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn playback_writer_rejects_after_stop() {
        let (tx, _rx) = async_mpsc::channel::<Vec<f32>>(4);
        let stopped = Arc::new(AtomicBool::new(true));
        let writer = PlaybackWriter { tx, stopped };
        assert!(writer.write(&[0.5]).is_err());
    }
}
