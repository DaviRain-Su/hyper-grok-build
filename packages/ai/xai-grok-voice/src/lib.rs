//! Voice input for Grok Build CLI: an xAI streaming STT client and the [`run_voice_pipeline`] task that emits [`VoiceEvent`]s for the pager.
//!
//! Voice is dictation only: mic → streaming STT → transcript into the prompt
//! box. With the `live` feature, it also drives Codex Live real-time voice
//! sessions via [`live::run_live_session`].
//!
//! On macOS and Linux the mic is opened in a short-lived subprocess, so the long-lived TUI never pays the audio stack's permanent memory cost.
//! See [`audio`] and [`maybe_run_capture_subprocess`].

#[cfg(feature = "audio")]
pub mod audio;
pub mod auth;
pub mod config;
pub mod error;
pub mod event;
pub mod language;
#[cfg(feature = "live")]
pub mod live;
pub mod pipeline;
pub mod probe;
pub mod stt;

pub use auth::{SharedVoiceAuth, StaticVoiceAuth, VoiceAuthProvider};
pub use config::VoiceConfig;
pub use error::VoiceError;
pub use event::VoiceEvent;
pub use language::{
    STT_LANGUAGE_AUTO, STT_LANGUAGE_DEFAULT, STT_LANGUAGES, SttLanguage, canonicalize_stt_language,
    language_for_api, stt_language_by_code,
};
pub use pipeline::{VoiceCommand, run_voice_pipeline};
#[cfg(feature = "audio")]
pub use probe::run_mic_only_probe;
pub use probe::{
    InputDeviceInfo, VoiceProbeOptions, VoiceProbeReport, format_probe_report, input_device_info,
    run_streaming_probe,
};

/// Whether this build can capture microphone audio (the `audio` feature).
/// Production CLI builds enable it on every OS: macOS and Windows link `cpal` (coreaudio/wasapi).
/// Linux shells out to a system recorder (`pw-record`/`parec`/`arecord`) so the static-musl binary links no audio library.
/// Bazel builds drop `audio` (no capture in the test sandbox).
///
/// On Linux a `true` value means capture is *compiled in*; whether a recorder is actually installed is reported when a session starts.
/// Consumers gate voice on this so a no-audio build never advertises a mic it can't open.
pub const AUDIO_SUPPORTED: bool = cfg!(feature = "audio");

/// Hidden subcommand consumers re-exec themselves with to capture microphone audio in a short-lived helper process on macOS.
/// See [`audio::capture_subprocess`](audio) for why capture is out of process.
/// Intercepted via [`maybe_run_capture_subprocess`] at the very top of `main`, before any TUI/agent/tokio init, so the child stays minimal.
pub const MIC_CAPTURE_SUBCOMMAND: &str = "__mic-capture";

/// Hidden subcommand the macOS live speaker-playback backend re-execs itself
/// with (`__speaker-play`), consistent with the [`MIC_CAPTURE_SUBCOMMAND`]
/// capture memory policy: CoreAudio's permanent footprint dies with the helper
/// when the live session ends. Intercepted via [`maybe_run_capture_subprocess`]
/// alongside the mic helper. Only the `live` feature's playback backend spawns
/// this; dictation capture never does.
pub const SPEAKER_PLAY_SUBCOMMAND: &str = "__speaker-play";

/// If this process was re-exec'd as a hidden audio helper (mic capture or
/// speaker playback), run it and return `Some(exit_code)`; otherwise `None` (a
/// normal invocation). Call at the very top of `main` in every binary that
/// links this crate with `audio` (the pager composition root and
/// `voice-probe`), mirroring the pager's mermaid render child intercept.
///
/// Preserved compatibility: the original contract dispatched only
/// [`MIC_CAPTURE_SUBCOMMAND`]; it now also dispatches [`SPEAKER_PLAY_SUBCOMMAND`]
/// so a single `main` intercept covers both helpers. Binaries that never enable
/// `live` are unaffected — the speaker branch is compiled out and unreachable.
pub fn maybe_run_capture_subprocess() -> Option<i32> {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let subcommand = argv.get(1).and_then(|a| a.to_str());
    match subcommand {
        Some(s) if s == MIC_CAPTURE_SUBCOMMAND => run_mic_capture_child(argv),
        Some(s) if s == SPEAKER_PLAY_SUBCOMMAND => run_speaker_play_child(argv),
        _ => None,
    }
}

/// Run the `__mic-capture` helper child (macOS/Windows cpal backend).
#[allow(unused_variables)]
fn run_mic_capture_child(argv: Vec<std::ffi::OsString>) -> Option<i32> {
    #[cfg(all(feature = "audio", not(target_os = "linux")))]
    {
        // Skip argv[0] (binary) and argv[1] (subcommand); the rest are flags.
        let args: Vec<String> = argv
            .into_iter()
            .skip(2)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        Some(audio::run_capture_child_cli(args))
    }
    #[cfg(not(all(feature = "audio", not(target_os = "linux"))))]
    {
        // Never spawned by this build's own parent backend (Linux uses system
        // recorders; no-audio builds have no capture). Reachable only by hand.
        // `write!` not `println!`: never panics on a closed pipe.
        use std::io::Write;
        let _ = writeln!(
            std::io::stdout(),
            "ERR mic-capture helper unavailable in this build"
        );
        Some(2)
    }
}

/// Run the `__speaker-play` helper child (macOS live speaker playback). Only
/// compiled in with the `live` feature (which implies `audio`); off-feature or
/// off-macOS builds report the helper as unavailable.
#[allow(unused_variables)]
fn run_speaker_play_child(argv: Vec<std::ffi::OsString>) -> Option<i32> {
    #[cfg(feature = "live")]
    {
        let args: Vec<String> = argv
            .into_iter()
            .skip(2)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        Some(live::playback::run_speaker_play_child(args))
    }
    #[cfg(not(feature = "live"))]
    {
        use std::io::Write;
        let _ = writeln!(
            std::io::stdout(),
            "ERR speaker-play helper unavailable in this build"
        );
        Some(2)
    }
}

/// Whether `argv` (the full process argv, incl. argv[0]) invokes the hidden
/// mic-capture helper — i.e. argv[1] is [`MIC_CAPTURE_SUBCOMMAND`]. Pure so the
/// dispatch decision is unit-testable without mutating the process's real args.
/// (Kept for the intercept tests; the live dispatch matches subcommands
/// directly.)
#[allow(dead_code)]
fn is_capture_subcommand(argv: &[std::ffi::OsString]) -> bool {
    argv.get(1).and_then(|a| a.to_str()) == Some(MIC_CAPTURE_SUBCOMMAND)
}

#[cfg(test)]
mod intercept_tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<std::ffi::OsString> {
        items.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn capture_subcommand_matches_only_argv1() {
        assert!(is_capture_subcommand(&argv(&["grok", "__mic-capture"])));
        assert!(is_capture_subcommand(&argv(&[
            "grok",
            "__mic-capture",
            "--rate",
            "16000"
        ])));
        assert!(!is_capture_subcommand(&argv(&["grok"])));
        assert!(!is_capture_subcommand(&argv(&["grok", "chat"])));
        assert!(!is_capture_subcommand(&argv(&[
            "grok",
            "chat",
            "__mic-capture"
        ])));
    }

    #[test]
    fn speaker_subcommand_is_distinct_from_mic_capture() {
        // The two hidden helpers must not collide.
        assert_ne!(MIC_CAPTURE_SUBCOMMAND, SPEAKER_PLAY_SUBCOMMAND);
        assert_eq!(MIC_CAPTURE_SUBCOMMAND, "__mic-capture");
        assert_eq!(SPEAKER_PLAY_SUBCOMMAND, "__speaker-play");
        // A speaker-play argv is not a mic-capture argv.
        assert!(!is_capture_subcommand(&argv(&["grok", "__speaker-play"])));
    }
}
