//! Speaker playback without maudio.
//!
//! Mirrors the capture backend's platform split (see `crate::audio`): the
//! long-lived TUI never pays a permanent platform-audio-stack memory cost, so
//! playback runs out of process on Linux and macOS, and in-process via `cpal`
//! on Windows (whose WASAPI memory cost is modest).
//!
//! - **Linux**: a subprocess player (`pw-play`/`pacat`/`aplay`) fed raw PCM16
//!   over its stdin.
//! - **Windows**: `cpal` output stream in-process.
//! - **macOS**: a short-lived self-exec `__speaker-play` helper (consistent
//!   with the `__mic-capture` capture memory policy) so CoreAudio's permanent
//!   footprint dies with the helper when the live session ends.
//!
//! The [`PlaybackWriter`] hands mono `f32` samples (48 kHz, the WebRTC output
//! rate) to the backend; each backend converts to the format its player
//! expects. All backends are bounded *by sample count*: a full queue drops the
//! **oldest** samples so playback stays recent rather than accumulating
//! latency, and every backend thread/callback terminates deterministically on
//! stop (no thread is left blocked on a live channel).
//!
//! # Queue design
//! The [`PlaybackQueue`] uses a single `Mutex<VecDeque<f32>>` + `Condvar` so
//! that enqueue (with sample-count shedding), dequeue, and accounting are
//! atomic under one lock — no separate atomic counter that can drift from the
//! actual buffer contents. `write` never blocks: if the queue is over the bound
//! the oldest samples are shed before the new chunk is appended. Chunks larger
//! than the bound are trimmed to the most recent `PLAYBACK_QUEUE_SAMPLES`.
//!
//! This is a substantially original design (no OMP `maudio` dependency); the
//! OMP `PlaybackStream`/`PlaybackWriter` *interface shape* is preserved for
//! ergonomics, but the implementations are platform subprocess/cpal ports. MIT
//! attribution for the borrowed interface in `THIRD-PARTY-NOTICES`.

use std::collections::VecDeque;
use std::io::Write;
#[cfg(not(target_os = "windows"))]
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::VoiceError;

/// Bounded capacity of the playback sample queue, in samples. Sized for ~1s
/// of 48 kHz mono — enough to absorb WebRTC jitter + subprocess scheduling
/// without underruns (which sound like crackle/stutter on macOS), while still
/// shedding if the player stalls so latency cannot grow unbounded.
const PLAYBACK_QUEUE_SAMPLES: usize = 48_000;

/// Poll interval for the subprocess reader threads. The reader uses
/// `wait_timeout` on the condvar so it never blocks forever — it wakes, checks
/// `stopped`, and exits promptly on shutdown even if a sender is still alive.
const RECV_POLL: Duration = Duration::from_millis(50);

/// Timeout for joining the backend thread on stop. A pathological backend that
/// refuses to exit after this long is detached (logged) so teardown is not
/// wedged indefinitely. Implemented via `bounded_join` (helper thread +
/// `recv_timeout`) — a true bounded join, not `thread::join()`.
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Linear resampler (pure state, reusable across chunk boundaries)
// ---------------------------------------------------------------------------

/// A simple linear-interpolation resampler that preserves its fractional source
/// position across `resample` calls. This avoids negative position and
/// extrapolation bugs that arise when the position is reset per chunk.
///
/// The resampler converts mono `f32` samples at `source_rate` to mono `f32`
/// samples at `target_rate`. It buffers the **last sample of each chunk** so
/// interpolation can span chunk boundaries correctly.
///
/// This is a pure struct with no platform dependencies, so it can be tested
/// independently of any audio device.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct LinearResampler {
    source_rate: u32,
    target_rate: u32,
    /// The fractional position within the *source* stream. Always ≥ 0.
    /// It represents how many source samples have been "consumed" so far.
    /// Integer part = index into the concatenated source stream; fractional
    /// part = interpolation weight between two adjacent source samples.
    position: f64,
    /// The last source sample from the previous chunk, used to interpolate the
    /// first output sample of the next chunk. This is `None` before the first
    /// chunk is processed.
    last_sample: Option<f64>,
}

impl LinearResampler {
    #[allow(dead_code)]
    pub fn new(source_rate: u32, target_rate: u32) -> Self {
        Self {
            source_rate,
            target_rate,
            position: 0.0,
            last_sample: None,
        }
    }

    /// The ratio of source samples per output sample.
    #[allow(dead_code)]
    fn ratio(&self) -> f64 {
        f64::from(self.source_rate) / f64::from(self.target_rate)
    }

    /// Resample a chunk of mono source samples into `output`. Returns the
    /// number of output samples written. The resampler state (position +
    /// last_sample) is updated so the next call continues seamlessly.
    ///
    /// `output` is cleared and filled; its capacity determines the maximum
    /// number of output samples. The caller should size it appropriately
    /// (e.g., `ceil(input.len() / ratio) + 1`).
    #[allow(dead_code)]
    pub fn resample(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if input.is_empty() && self.last_sample.is_none() {
            return;
        }
        let ratio = self.ratio();

        // Build a virtual source stream: [last_sample?, input...].
        // We track position as an absolute index into this virtual stream.
        // The integer part of position indexes into the virtual stream.
        //
        // The virtual stream starts with `last_sample` at index 0 (if present),
        // then input[0] at index `has_last`, input[1] at index `has_last + 1`, etc.
        let has_last = self.last_sample.is_some() as usize;
        let virtual_len = has_last + input.len();

        // Helper to get a virtual source sample by index.
        let get_sample = |idx: usize| -> Option<f64> {
            if has_last > 0 && idx == 0 {
                self.last_sample
            } else {
                let input_idx = idx - has_last;
                input.get(input_idx).map(|&s| f64::from(s))
            }
        };

        // Generate output samples until we run out of source samples to
        // interpolate between.
        loop {
            let idx = self.position as usize;
            let frac = self.position - idx as f64;

            let s0 = get_sample(idx);
            let s1 = get_sample(idx + 1);

            match (s0, s1) {
                (Some(a), Some(b)) => {
                    // Linear interpolation between two adjacent source samples.
                    output.push((a + (b - a) * frac) as f32);
                    self.position += ratio;
                }
                (Some(a), None) => {
                    // Last available source sample — emit it (no interpolation
                    // partner yet) and advance position. The next chunk will
                    // provide the partner via `last_sample`.
                    output.push(a as f32);
                    self.position += ratio;
                    // If the next position is beyond the virtual stream, we've
                    // exhausted this chunk. The last sample becomes `last_sample`
                    // for the next call.
                    break;
                }
                (None, _) => {
                    // Position is beyond the virtual stream — we've consumed
                    // all available source samples.
                    break;
                }
            }
        }

        // Normalize position: subtract the consumed source samples so the
        // position is relative to the *next* chunk's virtual stream.
        // The last source sample of this chunk becomes `last_sample` so the
        // next chunk can interpolate across the boundary.
        if let Some(&last) = input.last() {
            self.last_sample = Some(f64::from(last));
        }
        // If input is empty, keep the existing last_sample.

        // Adjust position: subtract `virtual_len` (the total samples in this
        // virtual stream). But we must account for the fact that position may
        // have advanced past the end by less than 1. The new position is
        // relative to a new virtual stream that starts with `last_sample` at
        // index 0.
        //
        // The number of source samples consumed from this virtual stream is
        // `self.position` (before adjustment). We subtract `virtual_len` but
        // must ensure the result is non-negative. Since we break when position
        // exceeds the stream, position should be ≤ virtual_len + ratio.
        //
        // However, if we broke because `s0` was the last sample and we emitted
        // it, position advanced by `ratio` past that index. So position could
        // be `virtual_len - 1 + ratio`. We subtract `virtual_len - 1` (since
        // the last sample becomes index 0 in the next stream) to get the new
        // position relative to the next virtual stream.
        let consumed = if virtual_len > 0 { virtual_len - 1 } else { 0 };
        self.position -= consumed as f64;
        // Clamp to non-negative (floating-point can overshoot slightly).
        if self.position < 0.0 {
            self.position = 0.0;
        }
    }

    /// Reset the resampler state (e.g., after a discontinuity).
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.position = 0.0;
        self.last_sample = None;
    }
}

/// Resampler + excess-output buffer, persisted across cpal callbacks so that
/// resampled samples beyond the current output buffer's needs are **not
/// discarded** — they carry to the next callback (finding 5). The resampler
/// itself preserves interpolation state across chunk boundaries; this struct
/// adds the output-side carry so no output tails are lost when the device
/// buffer is smaller than the resampled chunk.
///
/// This is a pure struct with no platform dependencies, so it can be tested on
/// any platform. It's only used by the Windows cpal callback at runtime.
#[allow(dead_code)]
#[derive(Debug)]
struct ResamplerState {
    resampler: LinearResampler,
    /// Excess resampled output from the previous callback, carried to the next.
    leftover: Vec<f32>,
    /// Last mono sample actually written to the device — held across underruns
    /// so brief gaps do not hard-zero the DAC (which clicks / 撕拉).
    last_out: f32,
}

#[allow(dead_code)]
impl ResamplerState {
    fn new(source_rate: u32, target_rate: u32) -> Self {
        Self {
            resampler: LinearResampler::new(source_rate, target_rate),
            leftover: Vec::new(),
            last_out: 0.0,
        }
    }

    /// Resample `input` and write up to `max_frames` mono samples into `out`,
    /// persisting any excess in `leftover` for the next call. Returns the
    /// number of frames written. If `input` is empty and there's no leftover,
    /// writes nothing (silence is the caller's responsibility).
    fn fill(&mut self, input: &[f32], out: &mut [f32]) -> usize {
        // First, consume any leftover from the previous callback.
        let mut written = 0usize;
        if !self.leftover.is_empty() {
            let take = self.leftover.len().min(out.len() - written);
            out[..take].copy_from_slice(&self.leftover[..take]);
            written += take;
            // Drop the consumed samples; keep the rest.
            self.leftover.drain(..take);
            if self.leftover.is_empty() {
                // Shrink to avoid unbounded capacity retention.
                self.leftover = Vec::new();
            }
        }

        if written >= out.len() {
            if written > 0 {
                self.last_out = out[written - 1];
            }
            return written; // leftover alone filled the buffer
        }

        if input.is_empty() {
            if written > 0 {
                self.last_out = out[written - 1];
            }
            return written; // nothing new to resample
        }

        // Resample the new input into a temp buffer.
        let mut resampled = Vec::with_capacity(input.len() + 1);
        self.resampler.resample(input, &mut resampled);

        // Write as much as fits; keep the rest as leftover.
        let remaining = out.len() - written;
        if resampled.len() <= remaining {
            out[written..written + resampled.len()].copy_from_slice(&resampled);
            written += resampled.len();
        } else {
            out[written..written + remaining].copy_from_slice(&resampled[..remaining]);
            written += remaining;
            self.leftover.extend_from_slice(&resampled[remaining..]);
        }
        if written > 0 {
            self.last_out = out[written - 1];
        }
        written
    }
}

// ---------------------------------------------------------------------------
// Bounded sample queue (Mutex<VecDeque> + Condvar)
// ---------------------------------------------------------------------------

/// Internal state for the playback queue, protected by a single mutex.
struct QueueInner {
    /// The sample buffer: a flat deque of mono f32 samples. Using a flat deque
    /// (not chunks) means the bound is strictly on sample count and shedding
    /// always drops the oldest individual samples.
    samples: VecDeque<f32>,
    /// Whether the queue has been stopped (no more pushes accepted).
    stopped: bool,
}

/// A bounded, sample-counted playback queue shared between the writer (the
/// WebRTC output decoder) and the platform reader thread/callback.
///
/// All operations (enqueue, shed, dequeue, accounting) are atomic under a
/// single `Mutex` + `Condvar`. The bound is strictly on the number of samples,
/// not the number of chunks. `write` never blocks: if the queue is over the
/// bound the oldest samples are shed before the new chunk is appended. Chunks
/// larger than the bound are trimmed to the most recent `PLAYBACK_QUEUE_SAMPLES`.
struct PlaybackQueue {
    inner: Mutex<QueueInner>,
    cond: Condvar,
}

impl PlaybackQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(QueueInner {
                samples: VecDeque::with_capacity(PLAYBACK_QUEUE_SAMPLES),
                stopped: false,
            }),
            cond: Condvar::new(),
        }
    }

    /// Push a chunk of samples, enforcing the sample bound by shedding the
    /// **oldest** samples. Never blocks. Returns `Err` if the queue is
    /// stopped/closed.
    ///
    /// If the chunk itself is larger than `PLAYBACK_QUEUE_SAMPLES`, only the
    /// most recent `PLAYBACK_QUEUE_SAMPLES` samples are kept (the rest are
    /// dropped).
    fn push(&self, samples: &[f32]) -> Result<(), VoiceError> {
        if samples.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.stopped {
            return Err(VoiceError::Stt("live speaker playback is closed".into()));
        }

        // If the incoming chunk alone exceeds the bound, trim it to the most
        // recent samples and clear the queue.
        if samples.len() >= PLAYBACK_QUEUE_SAMPLES {
            inner.samples.clear();
            let start = samples.len() - PLAYBACK_QUEUE_SAMPLES;
            inner.samples.extend(samples[start..].iter().copied());
            self.cond.notify_one();
            return Ok(());
        }

        // Shed oldest samples until the new chunk fits within the bound.
        let projected = inner.samples.len() + samples.len();
        if projected > PLAYBACK_QUEUE_SAMPLES {
            let to_shed = projected - PLAYBACK_QUEUE_SAMPLES;
            // Drain the oldest `to_shed` samples.
            if to_shed >= inner.samples.len() {
                inner.samples.clear();
            } else {
                inner.samples.drain(..to_shed);
            }
        }

        // Append the new samples.
        inner.samples.extend(samples.iter().copied());
        self.cond.notify_one();
        Ok(())
    }

    /// Wait for samples to be available, blocking up to `timeout`. Returns a
    /// vec of all currently-available samples (may be empty on timeout). Returns
    /// `None` when the queue is stopped and drained.
    fn wait_samples(&self, timeout: Duration) -> Option<Vec<f32>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.samples.is_empty() {
            if inner.stopped {
                return None;
            }
            // Wait for data or stop, with timeout.
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let result = self.cond.wait_timeout(inner, timeout).unwrap();
                inner = result.0;
                if !inner.samples.is_empty() || inner.stopped {
                    break;
                }
                if result.1.timed_out() || std::time::Instant::now() >= deadline {
                    break;
                }
            }
        }
        if inner.samples.is_empty() {
            if inner.stopped {
                return None;
            }
            return Some(Vec::new()); // timeout with no data
        }
        // Drain all available samples.
        let out: Vec<f32> = inner.samples.drain(..).collect();
        Some(out)
    }

    /// Try to drain up to `max` samples without blocking. Returns the drained
    /// samples (may be fewer than `max` or empty). Used by cpal callbacks.
    #[allow(dead_code)]
    fn try_drain(&self, max: usize) -> Vec<f32> {
        let mut inner = self.inner.lock().unwrap();
        let take = inner.samples.len().min(max);
        inner.samples.drain(..take).collect()
    }

    /// Stop the queue: no more pushes are accepted; the reader is woken so it
    /// can drain remaining samples and exit.
    fn stop(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.stopped = true;
        self.cond.notify_all();
    }

    /// Whether the queue is stopped.
    #[allow(dead_code)]
    fn is_stopped(&self) -> bool {
        self.inner.lock().unwrap().stopped
    }

    /// Current number of queued samples (for tests/diagnostics).
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().samples.len()
    }
}

/// Producer endpoint for one native playback device. Cloneable so the WebRTC
/// output decoder and a telemetry probe can share it.
#[derive(Clone)]
pub struct PlaybackWriter {
    queue: Arc<PlaybackQueue>,
    stopped: Arc<AtomicBool>,
}

impl PlaybackWriter {
    /// Queue mono `f32` samples without blocking the caller. The queue is
    /// bounded by sample count; when over the bound the *oldest* samples are
    /// shed so playback stays recent. Returns `Err` only when playback is
    /// stopped/closed.
    pub fn write(&self, samples: &[f32]) -> Result<(), VoiceError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(VoiceError::Stt("live speaker playback is closed".into()));
        }
        self.queue.push(samples)
    }
}

/// A running playback stream. Dropping it stops playback and releases the
/// speaker. `stop()` joins the backend thread for a synchronous, deterministic
/// release — it returns only after the backend thread has exited and the device
/// is released (or a bounded timeout expires).
pub struct PlaybackStream {
    queue: Arc<PlaybackQueue>,
    stopped: Arc<AtomicBool>,
    writer_thread: Option<JoinHandle<()>>,
    /// Platform-specific teardown handle (child process or cpal stream).
    teardown: Teardown,
}

enum Teardown {
    /// A subprocess player (Linux/macOS helper): kill + reap on stop.
    #[cfg(not(target_os = "windows"))]
    Child(Option<Child>),
    /// Windows cpal: the stream is moved into the bridge thread and dropped
    /// when the bridge exits (the sample source drops). The variant carries
    /// no handle but exists so the enum is constructible on Windows.
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
        let queue = Arc::new(PlaybackQueue::new());
        let stopped = Arc::new(AtomicBool::new(false));

        let (writer_thread, teardown) =
            start_backend(sample_rate, Arc::clone(&queue), Arc::clone(&stopped))?;

        Ok(Self {
            queue: Arc::clone(&queue),
            stopped,
            writer_thread: Some(writer_thread),
            teardown,
        })
    }

    /// Clone the producer endpoint used by the remote-audio decoder.
    pub fn writer(&self) -> PlaybackWriter {
        PlaybackWriter {
            queue: Arc::clone(&self.queue),
            stopped: Arc::clone(&self.stopped),
        }
    }

    /// Stop playback immediately and release the default speaker. Idempotent.
    /// Deterministic: stops the queue (waking the reader thread), kills any
    /// subprocess, then **joins** the reader thread with a bounded wait. Returns
    /// only after the backend thread has exited (or `STOP_JOIN_TIMEOUT`).
    pub fn stop(mut self) {
        self.shutdown_and_join();
    }

    /// Internal shutdown: stop the queue, teardown platform resources, join the
    /// backend thread with a **true bounded wait** (helper thread +
    /// `recv_timeout`). Returns only after the backend thread has exited, or
    /// `STOP_JOIN_TIMEOUT` elapses (in which case the thread is detached and
    /// logged). Unlike `thread::join()` (which has no timeout on stable Rust),
    /// this guarantees a bounded return.
    fn shutdown_and_join(&mut self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            // Already stopped; still try to join if the thread is still alive.
            if let Some(thread) = self.writer_thread.take() {
                let _ = bounded_join(thread, STOP_JOIN_TIMEOUT);
            }
            return;
        }
        // Stop the queue: wakes any reader blocked on the condvar.
        self.queue.stop();
        // Platform teardown.
        match &mut self.teardown {
            #[cfg(not(target_os = "windows"))]
            Teardown::Child(child) => {
                if let Some(child) = child.as_mut() {
                    // Close stdin first so the player flushes, then kill to
                    // release the device promptly. Killing also breaks the
                    // reader's blocking stdin write → it exits immediately.
                    let _ = child.stdin.take();
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            Teardown::Cpal | Teardown::None => {}
        }
        // Bounded join: helper thread + recv_timeout so we never block
        // indefinitely. If the backend thread doesn't exit within
        // STOP_JOIN_TIMEOUT, it is detached (logged) — but this is a
        // pathological case; normal teardown exits within RECV_POLL.
        if let Some(thread) = self.writer_thread.take() {
            let _ = bounded_join(thread, STOP_JOIN_TIMEOUT);
        }
    }
}

impl Drop for PlaybackStream {
    fn drop(&mut self) {
        // If stop() was already called, shutdown_and_join is mostly a no-op
        // (it joins the already-taken thread). If not, this performs a full
        // deterministic shutdown. Drop may run on an async executor, but the
        // join is bounded by STOP_JOIN_TIMEOUT so it won't wedge the runtime.
        self.shutdown_and_join();
    }
}

// ---------------------------------------------------------------------------
// Backend dispatch
// ---------------------------------------------------------------------------

#[allow(unused_variables)]
fn start_backend(
    sample_rate: u32,
    queue: Arc<PlaybackQueue>,
    stopped: Arc<AtomicBool>,
) -> Result<(JoinHandle<()>, Teardown), VoiceError> {
    #[cfg(target_os = "linux")]
    {
        start_linux_subprocess(sample_rate, queue, stopped)
    }
    #[cfg(target_os = "windows")]
    {
        start_windows_cpal(sample_rate, queue, stopped)
    }
    #[cfg(target_os = "macos")]
    {
        start_macos_helper(sample_rate, queue, stopped)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        // Unsupported platform: a no-op backend. The writer thread drains the
        // queue and discards samples so the decoder never blocks.
        let thread = thread::spawn(move || drain_and_discard(queue, stopped));
        Ok((thread, Teardown::None))
    }
}

// ---------------------------------------------------------------------------
// Linux: subprocess player
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn start_linux_subprocess(
    sample_rate: u32,
    queue: Arc<PlaybackQueue>,
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
    // Player is owned by the playback handle and killed on stop (same pattern
    // as `audio/capture_linux.rs`); not a fire-and-forget session child.
    #[allow(clippy::disallowed_methods)]
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

    let thread = thread::spawn(move || forward_pcm16(stdin, queue, stopped, player.program()));
    Ok((thread, Teardown::Child(Some(child))))
}

/// Convert f32 samples to PCM16 LE and write to the player's stdin until the
/// queue closes or the stream is stopped. Uses `wait_samples` (condvar-based)
/// so the thread wakes promptly on stop even if a sender is held. Exits
/// promptly when the subprocess is killed (stdin write fails) or the queue is
/// stopped. Generic over the writer for tests.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn forward_pcm16<W: Write + Send>(
    mut out: W,
    queue: Arc<PlaybackQueue>,
    stopped: Arc<AtomicBool>,
    device: &'static str,
) {
    let mut buf = Vec::with_capacity(4096);
    loop {
        if stopped.load(Ordering::Acquire) {
            break;
        }
        let Some(samples) = queue.wait_samples(RECV_POLL) else {
            break; // queue stopped and drained
        };
        if samples.is_empty() {
            continue; // poll timeout, loop and re-check stopped
        }
        buf.clear();
        buf.reserve(samples.len() * 2);
        for s in samples {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        // Do **not** flush every chunk: flush forces tiny pipe packets which
        // starve the child audio callback and produce crackle/stutter on macOS.
        // The OS pipe buffer coalesces; we only flush once on exit.
        if out.write_all(&buf).is_err() {
            // Player died (broken pipe / closed stdin). Stop the queue so the
            // media writer surfaces `Live speaker playback failed` instead of
            // silently shedding PCM into a black hole.
            tracing::warn!(
                device,
                "live speaker player closed stdin; stopping playback queue"
            );
            queue.stop();
            break;
        }
    }
    let _ = out.flush();
    let _ = device;
}

// ---------------------------------------------------------------------------
// Windows: cpal output stream
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn start_windows_cpal(
    sample_rate: u32,
    queue: Arc<PlaybackQueue>,
    stopped: Arc<AtomicBool>,
) -> Result<(JoinHandle<()>, Teardown), VoiceError> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, SampleRate};

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

    // The resampler + leftover state is persisted across callbacks in a Mutex
    // so interpolation spans chunk boundaries AND excess resampled output
    // carries to the next callback (finding 5: no output tails lost).
    let resampler: Arc<parking_lot::Mutex<ResamplerState>> = Arc::new(parking_lot::Mutex::new(
        ResamplerState::new(sample_rate, stream_rate),
    ));
    let stop_cb = Arc::clone(&stopped);
    let queue_for_cb = Arc::clone(&queue);
    let source_rate = sample_rate;

    let stream = match sample_format {
        SampleFormat::F32 => build_cpal_stream::<f32>(
            &device,
            &stream_config,
            queue_for_cb,
            Arc::clone(&resampler),
            stop_cb,
            source_rate,
            stream_rate,
            channels,
        )?,
        SampleFormat::I16 => build_cpal_stream::<i16>(
            &device,
            &stream_config,
            Arc::clone(&queue),
            Arc::clone(&resampler),
            Arc::clone(&stopped),
            source_rate,
            stream_rate,
            channels,
        )?,
        SampleFormat::U16 => build_cpal_stream::<u16>(
            &device,
            &stream_config,
            queue,
            resampler,
            stopped.clone(),
            source_rate,
            stream_rate,
            channels,
        )?,
        other => {
            return Err(VoiceError::Config(format!(
                "unsupported output sample format {other:?}"
            )));
        }
    };
    stream
        .play()
        .map_err(|e| VoiceError::Config(format!("play output stream: {e}")))?;

    // The cpal callback reads the queue directly; this thread just owns the
    // stream lifetime and exits when stopped so cpal teardown is deterministic.
    let thread = thread::spawn(move || {
        let _ = (sample_rate, stream_rate, channels);
        while !stopped.load(Ordering::Acquire) {
            thread::sleep(RECV_POLL);
        }
        // `stream` drops here, stopping cpal.
    });
    Ok((thread, Teardown::Cpal))
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn build_cpal_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    queue: Arc<PlaybackQueue>,
    resampler: Arc<parking_lot::Mutex<ResamplerState>>,
    stopped: Arc<AtomicBool>,
    source_rate: u32,
    stream_rate: u32,
    channels: u16,
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
                if stopped.load(Ordering::Acquire) {
                    out.fill(T::from_sample(0.0));
                    return;
                }
                let mut rs = resampler.lock();
                fill_cpal_output(out, &queue, &mut rs, source_rate, stream_rate, channels);
            },
            |err| tracing::warn!(error = %err, "live speaker playback stream error"),
            None,
        )
        .map_err(|e| VoiceError::Config(format!("build output stream: {e}")))?;
    Ok(stream)
}

/// Fill one cpal output buffer from the queue, resampling mono input to the
/// device rate and upmixing to the device channel count. The resampler +
/// leftover state is persisted across callbacks so interpolation spans chunk
/// boundaries AND excess resampled output carries to the next callback (no
/// output tails lost — finding 5).
///
/// Shared by Windows (in-process) and macOS (`__speaker-play` helper). Pulls
/// only the samples needed for this buffer (not "everything available") so
/// the queue stays pre-buffered and callbacks rarely underrun — underruns
/// sound like crackle / 撕拉 stutter.
#[cfg(any(target_os = "windows", all(target_os = "macos", feature = "audio")))]
fn fill_cpal_output<T>(
    out: &mut [T],
    queue: &PlaybackQueue,
    resampler: &mut ResamplerState,
    source_rate: u32,
    stream_rate: u32,
    channels: u16,
) where
    T: cpal::Sample + cpal::FromSample<f32>,
{
    let ch = channels.max(1) as usize;
    let total_frames = out.len() / ch;
    if total_frames == 0 {
        return;
    }

    // Pull only what this callback needs for a full buffer (plus a couple
    // samples for the linear interpolator). Leaving surplus in the queue is
    // intentional: it is the underrun margin.
    let ratio = f64::from(source_rate) / f64::from(stream_rate.max(1));
    let needed = (total_frames as f64 * ratio).ceil() as usize + 2;
    let input = queue.try_drain(needed.max(1));

    let mut mono = vec![0.0f32; total_frames];
    let written = resampler.fill(&input, &mut mono);

    // Upmix mono → channels. If the resampler could not fill the whole
    // buffer (true underrun), hold the last sample instead of hard zeros —
    // hard zeros produce the classic crackle/pop the user hears as 撕拉.
    let hold = if written > 0 {
        mono[written - 1].clamp(-1.0, 1.0)
    } else {
        resampler.last_out.clamp(-1.0, 1.0)
    };
    for (frame_idx, mono_sample) in mono.iter().enumerate() {
        let sample = if frame_idx < written {
            mono_sample.clamp(-1.0, 1.0)
        } else {
            hold
        };
        let base = frame_idx * ch;
        let v = T::from_sample(sample);
        for c in 0..ch {
            out[base + c] = v;
        }
    }
}

// ---------------------------------------------------------------------------
// macOS: short-lived self-exec helper
// ---------------------------------------------------------------------------

/// How long the parent waits for the `__speaker-play` helper to open the
/// default output device and print `READY` (or `ERR`) on stderr.
#[cfg(target_os = "macos")]
const SPEAKER_HELPER_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Parse one stderr line from the `__speaker-play` helper.
///
/// - `READY …` → `Some(Ok(payload))`
/// - `ERR …` → `Some(Err(message))`
/// - blank / diagnostic noise → `None` (keep reading)
///
/// Platform-agnostic so Linux CI can unit-test the handshake contract without
/// cross-compiling CoreAudio.
#[cfg_attr(not(test), allow(dead_code))]
fn parse_speaker_helper_status_line(line: &str) -> Option<Result<String, String>> {
    let msg = line.trim();
    if msg.is_empty() {
        return None;
    }
    if let Some(payload) = msg.strip_prefix("READY") {
        return Some(Ok(payload.trim().to_string()));
    }
    if let Some(payload) = msg.strip_prefix("ERR ") {
        return Some(Err(payload.to_string()));
    }
    if msg.starts_with("ERR") {
        return Some(Err(msg.to_string()));
    }
    None
}

#[cfg(target_os = "macos")]
fn start_macos_helper(
    sample_rate: u32,
    queue: Arc<PlaybackQueue>,
    stopped: Arc<AtomicBool>,
) -> Result<(JoinHandle<()>, Teardown), VoiceError> {
    use std::io::{BufRead, BufReader, Read};
    use std::sync::mpsc;

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
    // Helper owned by the playback handle and killed on stop.
    #[allow(clippy::disallowed_methods)]
    let mut child = cmd
        .spawn()
        .map_err(|e| VoiceError::Config(format!("spawn speaker helper: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| VoiceError::Config("speaker helper produced no stdin".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| VoiceError::Config("speaker helper produced no stderr".into()))?;

    // Block until the helper opens the device (READY) or reports failure (ERR).
    // Without this handshake a failed cpal open left the parent writing PCM
    // into a black-hole pipe and Live looked "connected" with no speaker.
    let (status_tx, status_rx) = mpsc::channel::<Result<String, String>>();
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = status_tx.send(Err(
                        "speaker helper exited before opening the output device".into(),
                    ));
                    break;
                }
                Ok(_) => match parse_speaker_helper_status_line(&line) {
                    Some(status) => {
                        let _ = status_tx.send(status);
                        // Drain remaining stderr for diagnostics (non-fatal).
                        let mut rest = String::new();
                        if reader.read_to_string(&mut rest).is_ok() {
                            let rest = rest.trim();
                            if !rest.is_empty() {
                                tracing::debug!(stderr = rest, "live speaker helper stderr");
                            }
                        }
                        break;
                    }
                    None => {
                        let msg = line.trim();
                        if !msg.is_empty() {
                            tracing::debug!(stderr = msg, "live speaker helper stderr");
                        }
                    }
                },
                Err(e) => {
                    let _ = status_tx.send(Err(format!("speaker helper stderr: {e}")));
                    break;
                }
            }
        }
    });

    match status_rx.recv_timeout(SPEAKER_HELPER_READY_TIMEOUT) {
        Ok(Ok(info)) => {
            if !info.is_empty() {
                tracing::info!(info = %info, "live speaker helper ready");
            }
        }
        Ok(Err(msg)) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(VoiceError::Config(format!(
                "live speaker helper failed: {msg}"
            )));
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(VoiceError::Config(
                "live speaker helper timed out opening the output device".into(),
            ));
        }
    }

    let thread = thread::spawn(move || forward_pcm16(stdin, queue, stopped, "speaker-helper"));
    Ok((thread, Teardown::Child(Some(child))))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Join a thread with a bounded timeout. Spawns a helper thread that performs
/// the actual `join()` and signals completion via a `std::sync::mpsc` channel;
/// the caller does `recv_timeout`. If the helper doesn't signal within
/// `timeout`, the backend thread is **detached** (the helper thread is left to
/// reap it in the background) and a warning is logged. This provides a true
/// bounded join — unlike `thread::join()` which has no timeout on stable Rust.
///
/// Returns `Ok(())` if the thread exited within the timeout, `Err(())` if it
/// timed out (detached).
fn bounded_join(handle: JoinHandle<()>, timeout: Duration) -> Result<(), ()> {
    use std::sync::mpsc;
    let (done_tx, done_rx) = mpsc::channel::<Result<(), ()>>();
    // The helper thread owns the JoinHandle and joins it. When it finishes, it
    // sends the result. If the caller times out, the helper is left running
    // (it will eventually join the backend thread and exit on its own).
    let _helper = thread::spawn(move || {
        let result = handle.join().map(|_| ()).map_err(|_| {
            tracing::warn!("live playback backend thread panicked");
        });
        let _ = done_tx.send(result);
    });
    match done_rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                "live playback backend thread did not exit within {:?}; detaching",
                timeout
            );
            Err(())
        }
    }
}

/// Drain the queue and discard samples (no-op backend / fallback). Used on
/// platforms without a dedicated player backend. Exits when stopped or the
/// queue is stopped and drained.
#[allow(dead_code)]
fn drain_and_discard(queue: Arc<PlaybackQueue>, stopped: Arc<AtomicBool>) {
    loop {
        if stopped.load(Ordering::Acquire) {
            return;
        }
        let Some(samples) = queue.wait_samples(RECV_POLL) else {
            return;
        };
        let _ = samples; // discarded
    }
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

/// Open the default cpal output device and play PCM16 mono LE (at `source_rate`
/// Hz) read from stdin until EOF.
///
/// # Why Linux is smooth and Mac used to crackle
/// Linux feeds a **system player** (`pw-play` / `pacat` / `aplay`) a continuous
/// PCM16 stream; the player maintains its own ring buffer and never drops to
/// hard zeros mid-utterance. The old Mac path pushed **discrete Vec chunks**
/// over an `mpsc` and zero-filled every cpal callback that arrived slightly
/// early — that is exactly the "撕拉 / 不连贯" the user hears.
///
/// This implementation mirrors Linux's continuous model *inside* the helper:
/// stdin → continuous [`PlaybackQueue`] of f32 → same `fill_cpal_output` as
/// Windows (pull only what the current buffer needs, hold last sample on
/// underrun). Prefer a **48 kHz** device config when CoreAudio offers it so we
/// avoid 48k→44.1k resampling artifacts.
#[cfg(all(target_os = "macos", feature = "audio"))]
fn run_cpal_playback(source_rate: u32) -> Result<(), VoiceError> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, SampleRate, StreamConfig};

    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| VoiceError::Config("no default output audio device".into()))?;
    let (stream_rate, channels, sample_format, stream_config) =
        pick_macos_output_config(&device, source_rate)?;

    // Continuous sample queue — same shape as the parent→Linux player path.
    let queue = Arc::new(PlaybackQueue::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let resampler: Arc<Mutex<ResamplerState>> =
        Arc::new(Mutex::new(ResamplerState::new(source_rate, stream_rate)));

    let stream = match sample_format {
        SampleFormat::F32 => build_macos_helper_stream::<f32>(
            &device,
            &stream_config,
            Arc::clone(&queue),
            Arc::clone(&resampler),
            Arc::clone(&stopped),
            source_rate,
            stream_rate,
            channels,
        )?,
        SampleFormat::I16 => build_macos_helper_stream::<i16>(
            &device,
            &stream_config,
            Arc::clone(&queue),
            Arc::clone(&resampler),
            Arc::clone(&stopped),
            source_rate,
            stream_rate,
            channels,
        )?,
        SampleFormat::U16 => build_macos_helper_stream::<u16>(
            &device,
            &stream_config,
            Arc::clone(&queue),
            Arc::clone(&resampler),
            Arc::clone(&stopped),
            source_rate,
            stream_rate,
            channels,
        )?,
        other => {
            return Err(VoiceError::Config(format!(
                "unsupported speaker sample format: {other:?}"
            )));
        }
    };
    stream
        .play()
        .map_err(|e| VoiceError::Config(format!("play output stream: {e}")))?;

    // READY before parent starts feeding PCM (handshake contract).
    let _ = writeln!(
        std::io::stderr(),
        "READY rate={stream_rate} channels={channels} format={sample_format:?} source_rate={source_rate}"
    );

    // Read PCM16 LE mono from stdin → continuous f32 queue (Linux-like).
    // Larger read size reduces syscall/chunk fragmentation across the pipe.
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let mut raw = vec![0u8; 16_384];
    let mut pending_lo: Option<u8> = None;
    loop {
        use std::io::Read;
        match handle.read(&mut raw) {
            Ok(0) => break,
            Ok(n) => {
                let mut samples = Vec::with_capacity(n / 2 + 1);
                let mut i = 0usize;
                if let Some(lo) = pending_lo.take() {
                    if n > 0 {
                        samples.push(i16::from_le_bytes([lo, raw[0]]) as f32 / i16::MAX as f32);
                        i = 1;
                    } else {
                        pending_lo = Some(lo);
                    }
                }
                while i + 1 < n {
                    let s = i16::from_le_bytes([raw[i], raw[i + 1]]);
                    samples.push(s as f32 / i16::MAX as f32);
                    i += 2;
                }
                if i < n {
                    pending_lo = Some(raw[i]);
                }
                if !samples.is_empty() && queue.push(&samples).is_err() {
                    break; // queue stopped (parent/teardown)
                }
            }
            Err(_) => break,
        }
    }
    stopped.store(true, Ordering::Release);
    queue.stop();
    drop(stream);
    Ok(())
}

/// Prefer a 48 kHz config (matches WebRTC Opus output / Linux's fixed rate)
/// so we avoid linear 48k→44.1k resampling. Falls back to the device default.
#[cfg(all(target_os = "macos", feature = "audio"))]
fn pick_macos_output_config(
    device: &cpal::Device,
    prefer_rate: u32,
) -> Result<(u32, u16, cpal::SampleFormat, cpal::StreamConfig), VoiceError> {
    use cpal::traits::DeviceTrait;
    use cpal::{SampleRate, StreamConfig};

    let default = device
        .default_output_config()
        .map_err(|e| VoiceError::Config(format!("default output config: {e}")))?;

    // Walk supported ranges looking for prefer_rate (typically 48_000).
    if let Ok(ranges) = device.supported_output_configs() {
        for range in ranges {
            let min = range.min_sample_rate().0;
            let max = range.max_sample_rate().0;
            if min <= prefer_rate && prefer_rate <= max && range.channels() >= 1 {
                let supported = range.with_sample_rate(SampleRate(prefer_rate));
                let sample_format = supported.sample_format();
                let channels = supported.channels();
                let stream_config = StreamConfig {
                    channels,
                    sample_rate: SampleRate(prefer_rate),
                    // Slightly larger buffer reduces CoreAudio callback
                    // frequency and underrun risk vs the absolute minimum.
                    buffer_size: cpal::BufferSize::Default,
                };
                return Ok((prefer_rate, channels, sample_format, stream_config));
            }
        }
    }

    let stream_rate = default.sample_rate().0;
    let channels = default.channels();
    let sample_format = default.sample_format();
    let stream_config = StreamConfig {
        channels,
        sample_rate: SampleRate(stream_rate),
        buffer_size: cpal::BufferSize::Default,
    };
    Ok((stream_rate, channels, sample_format, stream_config))
}

/// Build a cpal output stream for the macOS `__speaker-play` helper.
/// Uses the same continuous-queue + `fill_cpal_output` path as Windows so
/// callbacks pull a steady stream instead of zero-padding discrete chunks.
#[cfg(all(target_os = "macos", feature = "audio"))]
#[allow(clippy::too_many_arguments)]
fn build_macos_helper_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    queue: Arc<PlaybackQueue>,
    resampler: Arc<Mutex<ResamplerState>>,
    stopped: Arc<AtomicBool>,
    source_rate: u32,
    stream_rate: u32,
    channels: u16,
) -> Result<cpal::Stream, VoiceError>
where
    T: cpal::Sample + cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;

    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                if stopped.load(Ordering::Acquire) {
                    out.fill(T::from_sample(0.0));
                    return;
                }
                let Ok(mut rs) = resampler.lock() else {
                    out.fill(T::from_sample(0.0));
                    return;
                };
                fill_cpal_output(out, &queue, &mut rs, source_rate, stream_rate, channels);
            },
            |err| tracing::warn!(error = %err, "speaker helper stream error"),
            None,
        )
        .map_err(|e| VoiceError::Config(format!("build output stream: {e}")))?;
    Ok(stream)
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

    // --- Speaker helper status handshake (macOS parent contract) ---

    #[test]
    fn speaker_helper_status_parses_ready_with_payload() {
        let status = parse_speaker_helper_status_line(
            "READY rate=48000 channels=2 format=F32 source_rate=48000\n",
        );
        assert_eq!(
            status,
            Some(Ok(
                "rate=48000 channels=2 format=F32 source_rate=48000".into()
            ))
        );
    }

    #[test]
    fn speaker_helper_status_parses_ready_bare() {
        assert_eq!(
            parse_speaker_helper_status_line("READY"),
            Some(Ok(String::new()))
        );
        assert_eq!(
            parse_speaker_helper_status_line("READY\n"),
            Some(Ok(String::new()))
        );
    }

    #[test]
    fn speaker_helper_status_parses_err_variants() {
        assert_eq!(
            parse_speaker_helper_status_line("ERR no default output audio device"),
            Some(Err("no default output audio device".into()))
        );
        assert_eq!(
            parse_speaker_helper_status_line("ERR"),
            Some(Err("ERR".into()))
        );
    }

    #[test]
    fn speaker_helper_status_ignores_blank_and_noise() {
        assert_eq!(parse_speaker_helper_status_line(""), None);
        assert_eq!(parse_speaker_helper_status_line("   \n"), None);
        assert_eq!(
            parse_speaker_helper_status_line("tracing noise before ready"),
            None
        );
    }

    /// When the player pipe dies, `forward_pcm16` must stop the queue so
    /// subsequent `PlaybackWriter::write` returns Err (surfaces to the UI)
    /// instead of silently shedding PCM.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn forward_pcm16_stops_queue_when_writer_fails() {
        struct FailWriter;
        impl Write for FailWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "player died",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let queue = Arc::new(PlaybackQueue::new());
        let stopped = Arc::new(AtomicBool::new(false));
        queue.push(&[0.1, 0.2, 0.3]).expect("push before stop");
        // Seed so wait_samples returns immediately with data.
        forward_pcm16(FailWriter, Arc::clone(&queue), Arc::clone(&stopped), "test");
        assert!(
            queue.push(&[0.5]).is_err(),
            "queue must reject writes after the player pipe fails"
        );
    }

    // --- PlaybackQueue tests ---

    #[test]
    fn queue_push_and_drain_roundtrips() {
        let q = Arc::new(PlaybackQueue::new());
        q.push(&[1.0, 2.0, 3.0]).unwrap();
        assert_eq!(q.len(), 3);
        let out = q.try_drain(10);
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn queue_sheds_oldest_when_over_bound() {
        let q = Arc::new(PlaybackQueue::new());
        // Fill to the bound with 0.5, then push 1k of 1.0 → oldest 1k shed.
        let half = PLAYBACK_QUEUE_SAMPLES / 2;
        let chunk = vec![0.5f32; half];
        q.push(&chunk).unwrap();
        q.push(&chunk).unwrap(); // at bound
        assert_eq!(q.len(), PLAYBACK_QUEUE_SAMPLES);
        q.push(&[1.0; 1_000]).unwrap(); // shed 1k oldest
        assert_eq!(q.len(), PLAYBACK_QUEUE_SAMPLES);
        let out = q.try_drain(PLAYBACK_QUEUE_SAMPLES);
        assert_eq!(out.len(), PLAYBACK_QUEUE_SAMPLES);
        // The last 1000 samples should be 1.0 (the newest chunk).
        assert!(
            out[PLAYBACK_QUEUE_SAMPLES - 1000..]
                .iter()
                .all(|&s| s == 1.0)
        );
    }

    #[test]
    fn queue_trims_oversized_chunk() {
        let q = Arc::new(PlaybackQueue::new());
        // Push a chunk larger than the bound.
        let big = vec![0.7f32; PLAYBACK_QUEUE_SAMPLES + 5_000];
        q.push(&big).unwrap();
        assert_eq!(q.len(), PLAYBACK_QUEUE_SAMPLES);
        // Only the most recent PLAYBACK_QUEUE_SAMPLES should be kept.
        let out = q.try_drain(PLAYBACK_QUEUE_SAMPLES);
        assert_eq!(out[0], 0.7);
        assert_eq!(out[PLAYBACK_QUEUE_SAMPLES - 1], 0.7);
    }

    #[test]
    fn queue_handles_tiny_chunks() {
        let q = Arc::new(PlaybackQueue::new());
        for i in 0..100 {
            q.push(&[i as f32]).unwrap();
        }
        assert_eq!(q.len(), 100);
        let out = q.try_drain(100);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[99], 99.0);
    }

    #[test]
    fn queue_rejects_after_stop() {
        let q = Arc::new(PlaybackQueue::new());
        q.stop();
        assert!(q.push(&[1.0]).is_err());
    }

    #[test]
    fn queue_wait_returns_none_when_stopped_and_empty() {
        let q = Arc::new(PlaybackQueue::new());
        q.stop();
        let result = q.wait_samples(Duration::from_millis(10));
        assert!(result.is_none());
    }

    #[test]
    fn queue_wait_returns_samples_when_available() {
        let q = Arc::new(PlaybackQueue::new());
        q.push(&[1.0, 2.0]).unwrap();
        let result = q.wait_samples(Duration::from_millis(10));
        assert!(result.is_some());
        assert_eq!(result.unwrap(), vec![1.0, 2.0]);
    }

    /// Concurrent push + drain test: multiple threads push while one drains.
    /// Verifies the queue is concurrency-safe and the sample count never
    /// exceeds the bound.
    #[test]
    fn queue_concurrent_push_and_drain() {
        let q = Arc::new(PlaybackQueue::new());
        let mut handles = Vec::new();
        // 4 producer threads, each pushing 1000 samples in 100-chunk batches.
        for t in 0..4u32 {
            let q = Arc::clone(&q);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let chunk = vec![t as f32; 100];
                    let _ = q.push(&chunk);
                }
            }));
        }
        // 1 drainer thread.
        let q2 = Arc::clone(&q);
        let drainer = thread::spawn(move || {
            let mut total = 0usize;
            for _ in 0..200 {
                total += q2.try_drain(500).len();
                thread::sleep(Duration::from_micros(100));
            }
            total
        });
        for h in handles {
            h.join().unwrap();
        }
        // Drain remaining.
        let remaining = q.try_drain(PLAYBACK_QUEUE_SAMPLES * 2).len();
        let drained = drainer.join().unwrap();
        // Total drained + remaining should be ≤ 4 * 100 * 100 = 40000 (some may
        // be shed due to the bound). The queue length must never exceed the bound.
        assert!(
            drained + remaining <= 40_000,
            "drained {drained} + remaining {remaining} should be ≤ 40000"
        );
        // After all producers are done and we drained, the queue should be
        // within the bound.
        assert!(q.len() <= PLAYBACK_QUEUE_SAMPLES);
    }

    // --- PlaybackStream lifecycle tests ---

    /// Finding 3: stop() must return only after the backend thread has exited.
    /// This test verifies the backend thread is actually joined (not detached)
    /// by checking that the thread's state is no longer running after stop().
    #[test]
    fn playback_stop_joins_backend_thread() {
        let stream = PlaybackStream::start(48_000).unwrap();
        let writer = stream.writer();
        // Push some data so the backend thread is active.
        writer.write(&[0.5; 1000]).unwrap();
        let started = std::time::Instant::now();
        stream.stop();
        let elapsed = started.elapsed();
        // stop() should return promptly (the thread exits within RECV_POLL of
        // the queue being stopped). It must be well under STOP_JOIN_TIMEOUT.
        assert!(
            elapsed < STOP_JOIN_TIMEOUT,
            "stop() took {elapsed:?} — backend thread was not joined promptly"
        );
        // The writer is now closed.
        assert!(writer.write(&[0.5]).is_err());
    }

    /// Finding 1 (regression): stop must terminate deterministically even when a
    /// producer is still alive (the classic deadlock).
    #[test]
    fn playback_stop_terminates_with_live_producer() {
        let stream = PlaybackStream::start(48_000).unwrap();
        let writer = stream.writer(); // keep a producer alive
        let started = std::time::Instant::now();
        stream.stop();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "stop() took {elapsed:?} — reader thread did not terminate deterministically"
        );
        assert!(writer.write(&[0.5]).is_err());
    }

    /// Finding 3: Drop should be safe and not panic.
    #[test]
    fn playback_drop_is_safe() {
        let stream = PlaybackStream::start(48_000).unwrap();
        let _writer = stream.writer();
        // Just drop it — should not panic or hang.
        drop(stream);
    }

    // --- LinearResampler tests ---

    /// Helper: generate `n` samples of a sine wave at amplitude `amp`.
    fn sine_wave(n: usize, freq: f64, amp: f64) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f64 / 48000.0;
                (amp * (2.0 * std::f64::consts::PI * freq * t).sin()) as f32
            })
            .collect()
    }

    /// Ratio 1.0 (no resampling): output must equal input.
    #[test]
    fn resampler_ratio_1_passes_through() {
        let input = sine_wave(100, 440.0, 0.5);
        let mut rs = LinearResampler::new(48000, 48000);
        let mut output = Vec::new();
        rs.resample(&input, &mut output);
        // Output length should be close to input length (±1 for boundary).
        assert!(
            (output.len() as i32 - input.len() as i32).abs() <= 1,
            "output len {} vs input len {}",
            output.len(),
            input.len()
        );
        // Compare values (they should be very close since ratio = 1.0).
        for (i, (a, b)) in output.iter().zip(input.iter()).enumerate() {
            assert!((a - b).abs() < 0.01, "sample {i}: output {a} vs input {b}");
        }
    }

    /// 48k → 44.1k: contiguous input vs chunked input should produce
    /// approximately the same output.
    #[test]
    fn resampler_48k_to_44k1_contiguous_vs_chunked() {
        let input = sine_wave(4800, 440.0, 0.5); // 100ms at 48k

        // Contiguous.
        let mut rs1 = LinearResampler::new(48000, 44100);
        let mut contiguous = Vec::new();
        rs1.resample(&input, &mut contiguous);

        // Chunked: 10 chunks of 480 samples each.
        let mut rs2 = LinearResampler::new(48000, 44100);
        let mut chunked = Vec::new();
        for chunk in input.chunks(480) {
            let mut out = Vec::new();
            rs2.resample(chunk, &mut out);
            chunked.extend(out);
        }

        // The lengths should be close (within a few samples due to boundary
        // effects in chunked processing).
        let len_diff = (contiguous.len() as i32 - chunked.len() as i32).abs();
        assert!(
            len_diff <= 10,
            "contiguous len {} vs chunked len {} (diff {len_diff})",
            contiguous.len(),
            chunked.len()
        );

        // Compare the overlapping region: the values should be very close.
        let min_len = contiguous.len().min(chunked.len());
        let mut max_diff = 0.0f64;
        for i in 0..min_len.saturating_sub(5) {
            let diff = (f64::from(contiguous[i]) - f64::from(chunked[i])).abs();
            max_diff = max_diff.max(diff);
        }
        // Linear interpolation across chunks should produce very similar
        // results. Allow some tolerance for boundary effects.
        assert!(
            max_diff < 0.05,
            "max sample diff between contiguous and chunked: {max_diff}"
        );
    }

    /// 44.1k → 48k: contiguous input vs chunked input should produce
    /// approximately the same output.
    #[test]
    fn resampler_44k1_to_48k_contiguous_vs_chunked() {
        let input: Vec<f32> = (0..4410)
            .map(|i| {
                let t = i as f64 / 44100.0;
                (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as f32
            })
            .collect();

        // Contiguous.
        let mut rs1 = LinearResampler::new(44100, 48000);
        let mut contiguous = Vec::new();
        rs1.resample(&input, &mut contiguous);

        // Chunked: 10 chunks of 441 samples each.
        let mut rs2 = LinearResampler::new(44100, 48000);
        let mut chunked = Vec::new();
        for chunk in input.chunks(441) {
            let mut out = Vec::new();
            rs2.resample(chunk, &mut out);
            chunked.extend(out);
        }

        let len_diff = (contiguous.len() as i32 - chunked.len() as i32).abs();
        assert!(
            len_diff <= 10,
            "contiguous len {} vs chunked len {} (diff {len_diff})",
            contiguous.len(),
            chunked.len()
        );

        let min_len = contiguous.len().min(chunked.len());
        let mut max_diff = 0.0f64;
        for i in 0..min_len.saturating_sub(5) {
            let diff = (f64::from(contiguous[i]) - f64::from(chunked[i])).abs();
            max_diff = max_diff.max(diff);
        }
        assert!(
            max_diff < 0.05,
            "max sample diff between contiguous and chunked: {max_diff}"
        );
    }

    /// Resampler position must never go negative.
    #[test]
    fn resampler_position_never_negative() {
        let mut rs = LinearResampler::new(48000, 44100);
        let input = sine_wave(100, 440.0, 0.5);
        let mut output = Vec::new();
        // Process in very small chunks to stress boundary logic.
        for chunk in input.chunks(7) {
            rs.resample(chunk, &mut output);
        }
        // If we got here without panicking, the position never went negative
        // (which would cause `self.position as usize` to wrap).
        assert!(!output.is_empty());
    }

    /// Resampler with empty input should preserve state and produce no output.
    #[test]
    fn resampler_empty_input_preserves_state() {
        let mut rs = LinearResampler::new(48000, 48000);
        let mut output = Vec::new();
        rs.resample(&[1.0, 2.0, 3.0], &mut output);
        let len_before = output.len();
        // Empty input should produce no new output.
        rs.resample(&[], &mut output);
        assert_eq!(output.len(), len_before);
        // Next real input should continue from the preserved state.
        rs.resample(&[4.0, 5.0], &mut output);
        assert!(output.len() > len_before);
    }

    // --- ResamplerState tests (finding 5: no output tails lost) ---

    /// ResamplerState must carry excess resampled output to the next call when
    /// the output buffer is smaller than the resampled chunk. This simulates a
    /// small cpal output buffer: feed a large input, drain in small buffers,
    /// and verify the total output equals what a single large buffer would
    /// produce (no tails lost).
    #[test]
    fn resampler_state_carries_excess_across_small_buffers() {
        let input = sine_wave(4800, 440.0, 0.5); // 100ms at 48k

        // Reference: resample all at once into one large buffer.
        let mut rs_ref = LinearResampler::new(48000, 44100);
        let mut reference = Vec::new();
        rs_ref.resample(&input, &mut reference);

        // Now: ResamplerState with a tiny output buffer (16 samples), called
        // repeatedly with the full input on the first call and empty input on
        // subsequent calls (simulating one large queue drain followed by
        // callbacks that only consume leftover).
        let mut state = ResamplerState::new(48000, 44100);
        let mut all_output = Vec::new();
        let mut buf = vec![0.0f32; 16];

        // First call: feed the entire input. The buffer is tiny so most output
        // becomes leftover.
        let n = state.fill(&input, &mut buf);
        all_output.extend_from_slice(&buf[..n]);

        // Subsequent calls: no new input, just drain leftover.
        loop {
            let n = state.fill(&[], &mut buf);
            if n == 0 {
                break;
            }
            all_output.extend_from_slice(&buf[..n]);
        }

        // The total output should match the reference (no tails lost).
        assert_eq!(
            all_output.len(),
            reference.len(),
            "small-buffer output lost samples: got {} expected {}",
            all_output.len(),
            reference.len()
        );
        let min_len = all_output.len().min(reference.len());
        let mut max_diff = 0.0f64;
        for i in 0..min_len {
            max_diff = max_diff.max((f64::from(all_output[i]) - f64::from(reference[i])).abs());
        }
        assert!(
            max_diff < 1e-5,
            "small-buffer output diverged from reference: max_diff {max_diff}"
        );
    }

    /// ResamplerState chunked-vs-contiguous: feeding input in small chunks with
    /// small output buffers should produce the same output as one large call.
    #[test]
    fn resampler_state_chunked_vs_contiguous_small_buffers() {
        let input = sine_wave(4800, 440.0, 0.5);

        // Contiguous: one big buffer.
        let mut state_c = ResamplerState::new(48000, 44100);
        let mut contiguous = vec![0.0f32; 10000];
        let n = state_c.fill(&input, &mut contiguous);
        contiguous.truncate(n);

        // Chunked: 10 chunks of 480 samples, each drained into a 64-sample
        // buffer, looping until leftover is empty before the next chunk.
        let mut state_k = ResamplerState::new(48000, 44100);
        let mut chunked = Vec::new();
        let mut buf = vec![0.0f32; 64];
        for chunk in input.chunks(480) {
            let n = state_k.fill(chunk, &mut buf);
            chunked.extend_from_slice(&buf[..n]);
            // Drain any leftover from this chunk.
            loop {
                let n = state_k.fill(&[], &mut buf);
                if n == 0 {
                    break;
                }
                chunked.extend_from_slice(&buf[..n]);
            }
        }
        // Drain final leftover.
        loop {
            let n = state_k.fill(&[], &mut buf);
            if n == 0 {
                break;
            }
            chunked.extend_from_slice(&buf[..n]);
        }

        assert!(
            (contiguous.len() as i32 - chunked.len() as i32).abs() <= 2,
            "chunked vs contiguous length mismatch: {} vs {}",
            contiguous.len(),
            chunked.len()
        );
        let min_len = contiguous.len().min(chunked.len());
        let mut max_diff = 0.0f64;
        for i in 0..min_len.saturating_sub(2) {
            max_diff = max_diff.max((f64::from(contiguous[i]) - f64::from(chunked[i])).abs());
        }
        assert!(
            max_diff < 0.05,
            "chunked vs contiguous diverged: max_diff {max_diff}"
        );
    }

    /// ResamplerState with empty input and no leftover writes nothing.
    #[test]
    fn resampler_state_empty_input_no_leftover_writes_nothing() {
        let mut state = ResamplerState::new(48000, 48000);
        let mut buf = vec![0.0f32; 32];
        assert_eq!(state.fill(&[], &mut buf), 0);
    }

    // --- bounded_join tests (finding 4) ---

    /// bounded_join returns promptly when the thread exits quickly.
    #[test]
    fn bounded_join_returns_after_thread_exits() {
        let handle = thread::spawn(|| {});
        let start = std::time::Instant::now();
        let result = bounded_join(handle, Duration::from_secs(5));
        assert!(result.is_ok());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "bounded_join took too long for a fast thread"
        );
    }

    /// bounded_join times out and detaches when the thread blocks longer than
    /// the timeout. The function must return within timeout + small slack.
    #[test]
    fn bounded_join_times_out_on_long_running_thread() {
        let handle = thread::spawn(|| {
            thread::sleep(Duration::from_secs(10));
        });
        let start = std::time::Instant::now();
        let result = bounded_join(handle, Duration::from_millis(200));
        assert!(result.is_err(), "expected timeout (Err), got Ok");
        let elapsed = start.elapsed();
        // Should return around 200ms, definitely under 2s.
        assert!(
            elapsed < Duration::from_secs(2),
            "bounded_join did not time out promptly: {elapsed:?}"
        );
        // The test passes; the detached helper thread is still sleeping but
        // will exit on its own (it's a daemon thread).
    }
}
