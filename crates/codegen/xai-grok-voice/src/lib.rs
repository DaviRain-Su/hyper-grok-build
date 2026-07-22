//! Voice input for Grok Build CLI: an xAI streaming STT client and the
//! [`run_voice_pipeline`] task that emits [`VoiceEvent`]s for the pager.
//!
//! Voice is dictation only: mic → streaming STT → transcript into the prompt box.

#[cfg(feature = "audio")]
pub mod audio;
pub mod auth;
pub mod config;
pub mod error;
pub mod event;
pub mod language;
pub mod pcm;
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
/// Production CLI builds enable it on every OS: macOS/Windows link `cpal`
/// (coreaudio/wasapi), while Linux shells out to a system recorder
/// (`pw-record`/`parec`/`arecord`) so the static-musl binary links no audio
/// library. Bazel builds drop `audio` (no capture in the test sandbox).
///
/// On Linux a `true` value means capture is *compiled in*; whether a recorder
/// is actually installed is reported when a session starts. Consumers gate voice
/// on this so a no-audio build never advertises a mic it can't open.
pub const AUDIO_SUPPORTED: bool = cfg!(feature = "audio");
