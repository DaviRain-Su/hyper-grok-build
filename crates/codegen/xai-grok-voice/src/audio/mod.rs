//! Microphone capture (optional `audio` feature).
//!
//! Two backends share one interface (`spawn_pcm_capture`,
//! `capture_pcm_for_duration`, `CaptureHandle`):
//! - non-Linux (macOS/Windows): `cpal` (coreaudio/wasapi), linked into the binary;
//! - Linux: a subprocess recorder (`pw-record`/`parec`/`arecord`), because the
//!   static-musl release binary cannot link `cpal` -> `alsa-sys`. See
//!   [`capture_linux`] for the full rationale.

#[cfg(not(target_os = "linux"))]
mod capture;
#[cfg(not(target_os = "linux"))]
pub use capture::{CaptureHandle, capture_pcm_for_duration, input_device_info, spawn_pcm_capture};

#[cfg(target_os = "linux")]
mod capture_linux;
#[cfg(target_os = "linux")]
pub use capture_linux::{
    CaptureHandle, capture_pcm_for_duration, input_device_info, spawn_pcm_capture,
};
