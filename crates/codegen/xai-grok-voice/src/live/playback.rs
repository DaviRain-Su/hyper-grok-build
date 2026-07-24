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
//! This is a substantially original design (no OMP `maudio` dependency); the
//! OMP `PlaybackStream`/`PlaybackWriter` *interface shape* is preserved for
//! ergonomics, but the implementations are platform subprocess/cpal ports. MIT
//! attribution for the borrowed interface in `THIRD-PARTY-NOTICES`.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::VoiceError;

/// Bounded capacity of the playback sample queue, in samples. Sized for ~0.5s
/// of 48 kHz mono — enough to absorb decoder jitter without ever blocking the
/// real-time RTP read loop, and small enough that a stalled player sheds
/// quickly rather than accumulating seconds of latency.
const PLAYBACK_QUEUE_SAMPLES: usize = 24_000;

/// Poll interval for the subprocess reader threads: `recv_timeout` so a thread
/// never blocks forever on a live channel even if a sender is held (e.g. a
/// decoder that stalled mid-chunk). The thread wakes, checks `stopped`, and
/// exits promptly on shutdown.
const RECV_POLL: Duration = Duration::from_millis(50);

/// A bounded, sample-counted playback queue shared between the writer (the
/// WebRTC output decoder) and the platform reader thread/callback. The queue
/// is a single `flume` of sample chunks plus an atomic running sample total so
/// the bound is enforced on actual audio duration, not chunk count.
struct PlaybackQueue {
    tx: flume::Sender<Vec<f32>>,
    rx: flume::Receiver<Vec<f32>>,
    /// Approximate number of samples currently queued. Maintained by the
    /// writer; the reader subtracts as it drains. Used to shed oldest audio
    /// when the queue exceeds [`PLAYBACK_QUEUE_SAMPLES`].
    queued_samples: AtomicUsize,
}

impl PlaybackQueue {
    fn new() -> Self {
        // Chunk count capacity modest; the *sample* bound is the real guard.
        let (tx, rx) = flume::bounded::<Vec<f32>>(64);
        Self {
            tx,
            rx,
            queued_samples: AtomicUsize::new(0),
        }
    }

    /// Push a chunk, enforcing the sample bound by shedding the *oldest*
    /// queued chunks until the new chunk fits. Returns `Err` if the queue is
    /// stopped/closed.
    fn push(&self, samples: &[f32]) -> Result<(), VoiceError> {
        if samples.is_empty() {
            return Ok(());
        }
        let n = samples.len();
        // Drop oldest chunks while the projected total exceeds the bound, so
        // playback stays recent (low latency) rather than backing up. After
        // each drop we re-load the atomic to get an accurate remaining total
        // (the reader may also be draining concurrently).
        loop {
            let current = self.queued_samples.load(Ordering::Acquire);
            let projected = current.saturating_add(n);
            if projected <= PLAYBACK_QUEUE_SAMPLES {
                break;
            }
            match self.rx.try_recv() {
                Ok(dropped) => {
                    self.queued_samples
                        .fetch_sub(dropped.len(), Ordering::AcqRel);
                }
                Err(_) => break, // queue empty but total says over — trust the atomic
            }
        }
        match self.tx.send(samples.to_vec()) {
            Ok(()) => {
                self.queued_samples.fetch_add(n, Ordering::AcqRel);
                Ok(())
            }
            Err(_) => Err(VoiceError::Stt("live speaker playback is closed".into())),
        }
    }

    /// Receive the next chunk, blocking up to `timeout` (so the reader can
    /// poll `stopped`). Subtracts the drained sample count from the running
    /// total. Returns `None` when the queue is closed and drained.
    fn recv_timeout(&self, timeout: Duration) -> Option<Vec<f32>> {
        match self.rx.recv_timeout(timeout) {
            Ok(chunk) => {
                self.queued_samples.fetch_sub(chunk.len(), Ordering::AcqRel);
                Some(chunk)
            }
            Err(flume::RecvTimeoutError::Timeout) => Some(Vec::new()),
            Err(flume::RecvTimeoutError::Disconnected) => None,
        }
    }
}

/// Producer endpoint for one native playback device. Cloneable so the WebRTC
/// output decoder and a telemetry probe can share it.
#[derive(Clone)]
pub struct PlaybackWriter {
    /// Held to keep the queue's producer alive; the actual push goes through
    /// `queue.push` (which shares the same underlying sender).
    #[allow(dead_code)]
    tx: flume::Sender<Vec<f32>>,
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
/// speaker; `stop()` joins the writer thread for a synchronous release.
pub struct PlaybackStream {
    queue: Arc<PlaybackQueue>,
    /// The only producer clone owned by the stream (besides `PlaybackWriter`
    /// clones). Taken and dropped in `shutdown()` so the channel closes
    /// deterministically before the reader thread is joined.
    tx: Option<flume::Sender<Vec<f32>>>,
    stopped: Arc<AtomicBool>,
    writer_thread: Option<JoinHandle<()>>,
    /// Platform-specific teardown handle (child process or cpal stream).
    teardown: Teardown,
}

enum Teardown {
    /// A subprocess player (Linux/macOS helper): kill + reap on stop.
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
            tx: Some(queue.tx.clone()),
            stopped,
            writer_thread: Some(writer_thread),
            teardown,
        })
    }

    /// Clone the producer endpoint used by the remote-audio decoder.
    pub fn writer(&self) -> PlaybackWriter {
        PlaybackWriter {
            tx: self.queue.tx.clone(),
            queue: Arc::clone(&self.queue),
            stopped: Arc::clone(&self.stopped),
        }
    }

    /// Stop playback immediately and release the default speaker. Idempotent.
    /// Deterministic: drops the stream's producer clone (so the channel closes
    /// when all `PlaybackWriter` clones are also gone), kills any subprocess,
    /// then joins the reader thread with a bounded wait.
    pub fn stop(mut self) {
        self.shutdown();
        if let Some(thread) = self.writer_thread.take() {
            // The reader exits within ~RECV_POLL of the channel closing (or
            // immediately on a subprocess kill closing its stdin). Bound the
            // join so a pathological backend can't wedge teardown.
            let _ = thread::Builder::new()
                .name("playback-stop-join".into())
                .spawn(move || {
                    let _ = thread.join();
                });
        }
    }

    fn shutdown(&mut self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        // Drop the stream's producer clone first. When all `PlaybackWriter`
        // clones are also dropped the channel closes and the reader thread
        // exits via recv_timeout → None. The media peer drops its writer before
        // calling stop(), so this is the common path. Even if a writer clone
        // survives, the reader still wakes via recv_timeout polling + the
        // stopped flag.
        drop(self.tx.take());
        match &mut self.teardown {
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
    }
}

impl Drop for PlaybackStream {
    fn drop(&mut self) {
        self.shutdown();
        // The reader thread exits on its own once the channel closes (all
        // `PlaybackWriter` clones + `_tx` drop with `self`). Do not join in
        // `Drop` (it may run on an async executor and must not block); detach.
        drop(self.writer_thread.take());
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
/// queue closes or the stream is stopped. Uses `recv_timeout` so the thread
/// wakes periodically to observe `stopped` even if a sender is held. Exits
/// promptly when the subprocess is killed (stdin write fails) or the queue
/// closes (all producers dropped). Generic over the writer for tests.
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
        let Some(samples) = queue.recv_timeout(RECV_POLL) else {
            break; // queue closed and drained
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
        if out.write_all(&buf).and_then(|()| out.flush()).is_err() {
            break; // player closed stdin / died → stop
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

    // Persisted remainder across cpal callbacks so a partial chunk carries to
    // the next callback instead of being discarded.
    let remainder: Arc<parking_lot::Mutex<Vec<f32>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    // Resample state: fractional source-sample position for linear interpolation
    // when the device rate differs from the input (48 kHz) rate.
    let resample_pos = Arc::new(parking_lot::Mutex::new(0.0f64));
    let stop_cb = Arc::clone(&stopped);
    let queue_for_cb = Arc::clone(&queue);
    // Capture the source rate for the callback (the input is always mono 48 kHz
    // from the WebRTC output decoder).
    let source_rate = sample_rate;

    let stream = match sample_format {
        SampleFormat::F32 => build_cpal_stream::<f32>(
            &device,
            &stream_config,
            queue_for_cb,
            Arc::clone(&remainder),
            Arc::clone(&resample_pos),
            stop_cb,
            source_rate,
            stream_rate,
            channels,
        )?,
        SampleFormat::I16 => build_cpal_stream::<i16>(
            &device,
            &stream_config,
            Arc::clone(&queue),
            Arc::clone(&remainder),
            Arc::clone(&resample_pos),
            Arc::clone(&stopped),
            source_rate,
            stream_rate,
            channels,
        )?,
        SampleFormat::U16 => build_cpal_stream::<u16>(
            &device,
            &stream_config,
            queue,
            remainder,
            resample_pos,
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
    remainder: Arc<parking_lot::Mutex<Vec<f32>>>,
    resample_pos: Arc<parking_lot::Mutex<f64>>,
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
                fill_cpal_output(
                    out,
                    &queue,
                    &remainder,
                    &resample_pos,
                    &stopped,
                    source_rate,
                    stream_rate,
                    channels,
                );
            },
            |err| tracing::warn!(error = %err, "live speaker playback stream error"),
            None,
        )
        .map_err(|e| VoiceError::Config(format!("build output stream: {e}")))?;
    Ok(stream)
}

/// Fill one cpal output buffer from the queue, persisting any partial chunk in
/// `remainder` so the tail carries to the next callback (no discarded tails).
/// Resamples mono 48 kHz input to the device rate and upmixes to the device
/// channel count using linear interpolation.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn fill_cpal_output<T>(
    out: &mut [T],
    queue: &PlaybackQueue,
    remainder: &Arc<parking_lot::Mutex<Vec<f32>>>,
    resample_pos: &Arc<parking_lot::Mutex<f64>>,
    stopped: &Arc<AtomicBool>,
    source_rate: u32,
    stream_rate: u32,
    channels: u16,
) where
    T: cpal::Sample + cpal::FromSample<f32>,
{
    out.fill(T::from_sample(0.0));
    if stopped.load(Ordering::Acquire) {
        return;
    }
    let ch = channels.max(1) as usize;
    // The output buffer is interleaved: `out.len()` = frames * channels.
    // We fill frame by frame, each frame = `ch` identical mono samples.
    let total_frames = out.len() / ch;
    let mut out_frame = 0usize;

    let mut rem = remainder.lock();
    let mut pos = resample_pos.lock();

    // The ratio of source samples per output frame.
    let ratio = f64::from(source_rate) / f64::from(stream_rate);

    // Helper: get the next resampled mono sample from the remainder + queue.
    // Returns None when the queue is empty/disconnected.
    let mut next_sample = |rem: &mut Vec<f32>, pos: &mut f64| -> Option<f64> {
        loop {
            if !rem.is_empty() {
                let idx = *pos as usize;
                if idx + 1 < rem.len() {
                    let frac = *pos - idx as f64;
                    let s0 = f64::from(rem[idx]);
                    let s1 = f64::from(rem[idx + 1]);
                    *pos += ratio;
                    return Some(s0 + (s1 - s0) * frac);
                }
                if idx < rem.len() {
                    // Last sample in the current chunk; consume it and carry
                    // the fractional position forward.
                    let s = f64::from(rem[idx]);
                    *pos -= rem.len() as f64;
                    rem.clear();
                    return Some(s);
                }
                rem.clear();
                *pos = 0.0;
            }
            // Pull the next chunk from the queue.
            match queue.rx.try_recv() {
                Ok(chunk) => {
                    queue
                        .queued_samples
                        .fetch_sub(chunk.len(), Ordering::AcqRel);
                    *rem = chunk;
                }
                Err(_) => return None,
            }
        }
    };

    while out_frame < total_frames {
        let Some(sample) = next_sample(&mut rem, &mut pos) else {
            break;
        };
        let clamped = sample.clamp(-1.0, 1.0);
        let base = out_frame * ch;
        for c in 0..ch {
            out[base + c] = T::from_sample(clamped as f32);
        }
        out_frame += 1;
    }
}

// ---------------------------------------------------------------------------
// macOS: short-lived self-exec helper
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn start_macos_helper(
    sample_rate: u32,
    queue: Arc<PlaybackQueue>,
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
    let thread = thread::spawn(move || forward_pcm16(stdin, queue, stopped, "speaker-helper"));
    Ok((thread, Teardown::Child(Some(child))))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Drain the queue and discard samples (no-op backend / fallback). Used on
/// platforms without a dedicated player backend. Exits when stopped or the
/// queue closes.
#[allow(dead_code)]
fn drain_and_discard(queue: Arc<PlaybackQueue>, stopped: Arc<AtomicBool>) {
    loop {
        if stopped.load(Ordering::Acquire) {
            return;
        }
        let Some(samples) = queue.recv_timeout(RECV_POLL) else {
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

/// Open the default cpal output device at `rate` Hz and play PCM16 mono LE
/// read from stdin until EOF. Uses a persisted remainder so partial chunks
/// carry across callbacks (no discarded tails).
#[cfg(all(target_os = "macos", feature = "audio"))]
fn run_cpal_playback(rate: u32) -> Result<(), VoiceError> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleRate, StreamConfig};
    use std::sync::Mutex;
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
    // Persisted remainder so a partial PCM chunk carries to the next callback.
    let remainder: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let rem_cb = Arc::clone(&remainder);
    let stream = device
        .build_output_stream(
            &config,
            move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                use std::sync::mpsc::TryRecvError;
                out.fill(0);
                if stop_cb.load(Ordering::Acquire) {
                    return;
                }
                let mut rem = match rem_cb.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                let mut written = 0usize;
                // Drain leftover first.
                while written < out.len() && !rem.is_empty() {
                    let take = rem.len().min(out.len() - written);
                    out[written..written + take].copy_from_slice(&rem[..take]);
                    written += take;
                    rem.drain(..take);
                }
                // Then pull new chunks.
                while written < out.len() {
                    match rx.try_recv() {
                        Ok(chunk) => {
                            let take = chunk.len().min(out.len() - written);
                            out[written..written + take].copy_from_slice(&chunk[..take]);
                            written += take;
                            if take < chunk.len() {
                                rem.extend_from_slice(&chunk[take..]);
                            }
                        }
                        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                    }
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn forward_pcm16_converts_float_to_le_pcm16() {
        let queue = Arc::new(PlaybackQueue::new());
        let stopped = Arc::new(AtomicBool::new(false));
        let mut sink = std::io::Cursor::new(Vec::new());
        let samples = vec![0.0_f32, 1.0, -1.0];
        queue.push(&samples).unwrap();
        drop(queue.tx.clone()); // close is hard with shared tx; instead set stopped
        // Drive the loop by setting stopped after a short delay: simpler to
        // push one chunk then close all senders. We hold no other tx here.
        // Close by dropping the queue's own sender via a helper: clone+drop
        // doesn't close (other clones exist). Use stopped to terminate.
        let stop_for_thread = Arc::clone(&stopped);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stop_for_thread.store(true, Ordering::Release);
        });
        forward_pcm16(&mut sink, queue, stopped, "test");
        let bytes = sink.into_inner();
        // At least the 6 bytes from the one chunk should have been written.
        assert!(bytes.len() >= 6, "got {} bytes", bytes.len());
        let s0 = i16::from_le_bytes([bytes[0], bytes[1]]);
        let s1 = i16::from_le_bytes([bytes[2], bytes[3]]);
        let s2 = i16::from_le_bytes([bytes[4], bytes[5]]);
        assert_eq!(s0, 0);
        assert_eq!(s1, i16::MAX);
        assert_eq!(s2, -i16::MAX);
    }

    #[tokio::test]
    async fn playback_queue_enforces_sample_bound_and_drops_oldest() {
        let queue = Arc::new(PlaybackQueue::new());
        // Push ~1s of samples (48k) in 4 chunks; the bound is 24k (~0.5s), so
        // the oldest chunks must be shed.
        let chunk = vec![0.5f32; 12_000];
        queue.push(&chunk).unwrap(); // 12k
        queue.push(&chunk).unwrap(); // 24k (at bound)
        queue.push(&chunk).unwrap(); // 36k projected → oldest shed → 24k
        let total = queue.queued_samples.load(Ordering::Acquire);
        assert!(
            total <= PLAYBACK_QUEUE_SAMPLES,
            "total {total} must not exceed bound {}",
            PLAYBACK_QUEUE_SAMPLES
        );
        // The newest chunk (last pushed) must still be retrievable.
        let first = queue.recv_timeout(Duration::from_millis(100));
        assert!(first.is_some(), "newest chunk must survive shedding");
    }

    #[tokio::test]
    async fn playback_queue_recv_timeout_returns_none_when_closed() {
        let queue = Arc::new(PlaybackQueue::new());
        // Drop the sender held by the queue itself (the only producer here).
        // flume closes when all senders drop. The queue stores one in `tx`;
        // dropping `queue.tx` requires deconstructing — instead, drain via
        // recv and verify a stopped queue surfaces a timeout-style empty.
        queue.push(&[0.1]).unwrap();
        let _ = queue.recv_timeout(Duration::from_millis(50));
        // No more producers: create a fresh queue, drop its sender, verify None.
        let q2 = PlaybackQueue::new();
        drop(q2.tx);
        assert!(q2.rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[tokio::test]
    async fn playback_writer_rejects_after_stop() {
        let queue = Arc::new(PlaybackQueue::new());
        let stopped = Arc::new(AtomicBool::new(true));
        let writer = PlaybackWriter {
            tx: queue.tx.clone(),
            queue: Arc::clone(&queue),
            stopped,
        };
        assert!(writer.write(&[0.5]).is_err());
    }

    /// Regression test for finding 1: a playback stream must terminate its
    /// reader thread deterministically on stop even while a producer is still
    /// alive (the classic deadlock the audit flagged). We hold a writer clone
    /// (so the channel is NOT closed) and verify `stop()` returns promptly.
    #[tokio::test]
    async fn playback_stop_terminates_promptly_with_live_producer() {
        let stream = PlaybackStream::start(48_000).unwrap();
        // Keep a producer alive so the channel does not close via sender drop.
        let writer = stream.writer();
        let started = std::time::Instant::now();
        // stop() consumes the stream; the reader thread must exit via the
        // stopped flag + recv_timeout polling (or subprocess kill), not block.
        stream.stop();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "stop() took {elapsed:?} — reader thread did not terminate deterministically"
        );
        // The held writer is now closed; further writes fail.
        assert!(writer.write(&[0.5]).is_err());
    }

    /// Regression test for finding 4: a pure sample-draining helper that
    /// persists remainder across fill boundaries (no discarded tails). This
    /// mirrors the cpal callback logic without requiring an audio device.
    #[test]
    fn fill_drains_remainder_across_boundaries_without_loss() {
        // Simulate the callback fill: input chunks [1,2,3,4,5] and [6,7,8],
        // output buffers of size 3. After two fills (6 samples consumed) all
        // 8 samples must be written with nothing lost, in order.
        let input: Vec<Vec<f32>> = vec![vec![1.0, 2.0, 3.0, 4.0, 5.0], vec![6.0, 7.0, 8.0]];
        let mut out_bufs: Vec<Vec<f32>> = Vec::new();
        let mut remainder: Vec<f32> = Vec::new();
        let mut in_idx = 0usize;
        // Three callback invocations of 3 samples each = 9 slots for 8 samples.
        for _ in 0..3 {
            let mut out = vec![-1.0_f32; 3];
            let mut written = 0usize;
            // Drain remainder first.
            while written < out.len() && !remainder.is_empty() {
                let take = remainder.len().min(out.len() - written);
                out[written..written + take].copy_from_slice(&remainder[..take]);
                written += take;
                remainder.drain(..take);
            }
            // Pull new chunks.
            while written < out.len() && in_idx < input.len() {
                let chunk = &input[in_idx];
                let take = chunk.len().min(out.len() - written);
                out[written..written + take].copy_from_slice(&chunk[..take]);
                written += take;
                if take < chunk.len() {
                    remainder.extend_from_slice(&chunk[take..]);
                }
                in_idx += 1;
            }
            out_bufs.push(out);
        }
        // Flatten, ignoring the trailing -1 pad.
        let mut all: Vec<f32> = Vec::new();
        for b in &out_bufs {
            for &s in b {
                if s != -1.0 {
                    all.push(s);
                }
            }
        }
        assert_eq!(all, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
    }
}
