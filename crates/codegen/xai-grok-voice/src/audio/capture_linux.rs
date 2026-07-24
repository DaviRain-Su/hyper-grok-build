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
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc as async_mpsc;

use super::pipe::{self, READ_CHUNK};
use crate::error::VoiceError;

/// How long to wait after spawning before deciding the recorder started cleanly.
/// A missing device or a stopped audio server makes the recorder exit within a
/// few ms; this surfaces that as an error instead of a session that "listens"
/// but never produces audio (mirrors the `cpal` backend's open handshake).
const START_GRACE: Duration = Duration::from_millis(300);

/// A system audio recorder that can stream raw PCM16 mono to stdout.
#[derive(Clone, Copy, PartialEq, Eq)]
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
        match self {
            Recorder::PwRecord => Self::pw_record_args(rate, pw_record_supports_raw()),
            Recorder::Parec => {
                let rate = rate.to_string();
                vec![
                    "--raw".into(),
                    "--format=s16le".into(),
                    format!("--rate={rate}"),
                    "--channels=1".into(),
                ]
            }
            Recorder::Arecord => {
                let rate = rate.to_string();
                vec![
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
                ]
            }
        }
    }

    /// Build `pw-record` argv for mono PCM16 at `rate` Hz.
    ///
    /// **`--raw` is version-dependent:**
    /// - Newer PipeWire (≈1.6+) defaults to a libsndfile container (WAV/AU).
    ///   Without `--raw`, writing to a pipe fails with
    ///   "this file format does not support pipe writing".
    /// - Older PipeWire (e.g. Ubuntu 24.04 / 1.0.5) does **not** accept
    ///   `--raw` at all ("未识别的选项" / "unrecognized option") and already
    ///   treats stdout target `-` as raw PCM — so we must omit the flag.
    fn pw_record_args(rate: u32, with_raw: bool) -> Vec<String> {
        let rate = rate.to_string();
        let mut args = Vec::with_capacity(9);
        if with_raw {
            args.push("--raw".into());
        }
        args.push("--rate".into());
        args.push(rate);
        args.push("--channels".into());
        args.push("1".into());
        args.push("--format".into());
        args.push("s16".into());
        args.push("-".into());
        args
    }
}

/// Whether this machine's `pw-record` advertises `--raw` (once per process).
///
/// Probes `--help` rather than trial-spawning capture so we never open the mic
/// just to learn CLI flags. Option names stay English even when the help text
/// is localized.
fn pw_record_supports_raw() -> bool {
    static SUPPORTS: OnceLock<bool> = OnceLock::new();
    *SUPPORTS.get_or_init(probe_pw_record_supports_raw)
}

fn probe_pw_record_supports_raw() -> bool {
    let output = match Command::new("pw-record").arg("--help").output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = {
        let mut s = String::with_capacity(output.stdout.len() + output.stderr.len());
        s.push_str(&String::from_utf8_lossy(&output.stdout));
        s.push_str(&String::from_utf8_lossy(&output.stderr));
        s
    };
    help_lists_raw_flag(&text)
}

/// Pure helper: true when help text lists a `--raw` option.
///
/// Ignores error lines such as `unrecognized option '--raw'` / `未识别的选项
/// "--raw"` so a failed trial-run dump is never mistaken for support.
fn help_lists_raw_flag(help: &str) -> bool {
    help.lines().any(|line| {
        let t = line.trim_start();
        // Option listings start with the flag (after indent), e.g.
        // `      --raw                            Record raw PCM`.
        // Error lines put prose before the flag (`未识别的选项 "--raw"`).
        if t.starts_with("--raw") {
            return true;
        }
        // Forms like `-r, --raw` / `--foo | --raw`.
        if let Some(idx) = t.find("--raw") {
            let before = t[..idx].to_ascii_lowercase();
            return !before.contains("option")
                && !before.contains("选项")
                && !before.contains("unrecognized")
                && !before.contains("unknown");
        }
        false
    })
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
///
/// For `pw-record`, prefers `--raw` when the binary advertises it. If the
/// help probe is wrong and the child dies complaining about an unknown
/// `--raw` flag, retries once without it (PipeWire 1.0.x path).
fn spawn_recorder(sample_rate: u32) -> Result<(Recorder, Child), VoiceError> {
    let recorder = require_recorder()?;
    let args = recorder.args(sample_rate);
    match spawn_recorder_with_args(recorder, &args) {
        Ok(child) => Ok((recorder, child)),
        Err(err) if recorder == Recorder::PwRecord && args.iter().any(|a| a == "--raw") => {
            if stderr_rejects_raw_flag(err_message(&err)) {
                tracing::warn!("pw-record rejected --raw; retrying without it (older PipeWire)");
                let fallback = Recorder::pw_record_args(sample_rate, false);
                spawn_recorder_with_args(recorder, &fallback).map(|child| (recorder, child))
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

fn err_message(err: &VoiceError) -> &str {
    match err {
        VoiceError::Config(msg)
        | VoiceError::Auth(msg)
        | VoiceError::Stt(msg)
        | VoiceError::WebSocket(msg) => msg.as_str(),
    }
}

fn stderr_rejects_raw_flag(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    (lower.contains("--raw") || msg.contains("\"--raw\"") || msg.contains("`--raw`"))
        && (lower.contains("unrecognized")
            || lower.contains("unknown option")
            || lower.contains("invalid option")
            || msg.contains("未识别的选项")
            || msg.contains("无效的选项"))
}

fn spawn_recorder_with_args(recorder: Recorder, args: &[String]) -> Result<Child, VoiceError> {
    let mut cmd = Command::new(recorder.program());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // setsid detach via the sanctioned helper (workspace subprocess rule): the
    // recorder writes to a pipe and must not share the pager's controlling TTY.
    xai_tty_utils::detach_std_command(&mut cmd);
    let mut child = cmd
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
        Ok(None) => Ok(child),
        Err(e) => Err(VoiceError::Config(format!(
            "failed to poll {}: {e}",
            recorder.program()
        ))),
    }
}

/// Stop handle for the recorder subprocess (owns the child + reader thread).
pub use super::pipe::ChildCaptureHandle as CaptureHandle;

/// Spawn subprocess capture; PCM16 LE chunks are forwarded to `pcm_tx`.
pub fn spawn_pcm_capture(
    sample_rate: u32,
    pcm_tx: async_mpsc::Sender<Vec<u8>>,
) -> Result<CaptureHandle, VoiceError> {
    let (recorder, mut child) = spawn_recorder(sample_rate)?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(VoiceError::Config(format!(
            "{} produced no stdout",
            recorder.program()
        )));
    };

    pipe::drain_stderr(&mut child, recorder.program());

    let stop = Arc::new(AtomicBool::new(false));
    let stop_reader = Arc::clone(&stop);
    let device = recorder.program();
    let reader = thread::spawn(move || pipe::forward_pcm(stdout, pcm_tx, stop_reader, device));

    tracing::info!(
        recorder = recorder.program(),
        sample_rate,
        "voice capture stream (subprocess)"
    );

    Ok(CaptureHandle::new(child, stop, reader))
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
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(VoiceError::Config(format!(
            "{} produced no stdout",
            recorder.program()
        )));
    };
    pipe::drain_stderr(&mut child, recorder.program());

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

        // New PipeWire: --raw required so stdout is pure PCM16 (not WAV/AU).
        let pw_raw = Recorder::pw_record_args(48_000, true);
        assert!(pw_raw.contains(&"--raw".to_string()));
        let r = pw_raw.iter().position(|a| a == "--rate").unwrap();
        assert_eq!(pw_raw[r + 1], "48000");
        let f = pw_raw.iter().position(|a| a == "--format").unwrap();
        assert_eq!(pw_raw[f + 1], "s16");
        let c = pw_raw.iter().position(|a| a == "--channels").unwrap();
        assert_eq!(pw_raw[c + 1], "1");
        assert_eq!(pw_raw.last().unwrap(), "-"); // stdout target

        // Old PipeWire (e.g. 1.0.5): --raw is unrecognized; `-` is already raw.
        let pw_old = Recorder::pw_record_args(48_000, false);
        assert!(!pw_old.contains(&"--raw".to_string()));
        assert_eq!(pw_old.first().map(String::as_str), Some("--rate"));
        assert_eq!(pw_old.last().unwrap(), "-");
    }

    #[test]
    fn help_lists_raw_flag_detects_option_listings_only() {
        assert!(help_lists_raw_flag(
            "Usage: pw-record [options]\n      --raw                            Record raw PCM\n      --rate                           Sample rate\n"
        ));
        assert!(help_lists_raw_flag("  -r, --raw   raw PCM to stdout\n"));
        // Ubuntu 24.04 / PipeWire 1.0.5 style help (no --raw).
        assert!(!help_lists_raw_flag(
            "pw-record [选项] [<文件>|-]\n      --rate                            采样率\n      --channels                        通道数\n      --format                          采样格式\n"
        ));
        // Failed trial with unrecognized flag must not look like support.
        assert!(!help_lists_raw_flag(
            "pw-record: 未识别的选项 \"--raw\"\npw-record [选项] [<文件>|-]\n      --rate                            采样率\n"
        ));
        assert!(!help_lists_raw_flag(
            "pw-record: unrecognized option '--raw'\n      --rate\n"
        ));
    }

    #[test]
    fn stderr_rejects_raw_flag_matches_locale_variants() {
        assert!(stderr_rejects_raw_flag(
            "pw-record exited immediately (exit status: 1): pw-record: 未识别的选项 \"--raw\""
        ));
        assert!(stderr_rejects_raw_flag(
            "pw-record: unrecognized option '--raw'"
        ));
        assert!(!stderr_rejects_raw_flag(
            "this file format does not support pipe writing"
        ));
        assert!(!stderr_rejects_raw_flag("no device available"));
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
