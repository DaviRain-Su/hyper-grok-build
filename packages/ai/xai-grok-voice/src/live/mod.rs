//! Codex Live voice subsystem — real-time bidirectional voice sessions over
//! WebRTC/Opus against the Codex (ChatGPT) Frameless Bidi backend.
//!
//! This is a separately feature-gated (`live`) subsystem layered on top of the
//! existing dictation (`audio`) capture. It ports the native media + protocol
//! stack from oh-my-pi (OMP) v17.1.1 (commit e9c8a35) — `packages/coding-agent/
//! src/live/{protocol,transport,attestation}.ts` and
//! `crates/pi-natives/src/{live,audio,devicecheck}.rs` — into pure Rust,
//! preserving the exact wire behavior and MIT attribution.
//!
//! # Module layout
//! - [`types`]: the public API contract the pager adapts onto.
//! - [`protocol`]: the Frameless Bidi wire protocol (parser, builders,
//!   chunking, call-id, serde payloads).
//! - [`attestation`]: macOS arm64 DeviceCheck/CBOR attestation (cfg-gated).
//! - [`media`]: the WebRTC peer, Opus 16k/20ms input, 48k output, oai-events
//!   data-channel fallback, output-level RMS, packet-loss concealment, bounded
//!   input/playback queues.
//! - [`playback`]: speaker playback without maudio (Linux subprocess, Windows
//!   cpal, macOS self-exec helper).
//! - [`transport`]: Codex signaling + sideband wss, exact headers, retry/
//!   timeouts, once-only 401 forced refresh, idempotent shutdown, proxy.
//! - [`session`]: [`run_live_session`], reusing existing PCM16 capture,
//!   applying the OMP echo gate and mute.
//!
//! # Feature isolation
//! `live` implies `audio` and pulls in WebRTC/Opus (pinned). `--no-default-
//! features` and `--features audio` compile **none** of this module and link no
//! WebRTC/Opus code.

pub mod attestation;
pub mod media;
pub mod playback;
pub mod protocol;
pub mod session;
pub mod transport;
pub mod types;

pub use protocol::{LiveContextChannel, LiveRole};
pub use types::{
    LiveAuth, LiveAuthProvider, LiveCommand, LiveConfig, LiveEvent, LivePhase, SharedLiveAuth,
    TranscriptKind,
};

pub use session::run_live_session;
