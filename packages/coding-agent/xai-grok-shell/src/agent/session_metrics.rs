//! Session lifecycle event structs.
//!
//! Re-exported from `xai-grok-telemetry` after the telemetry crate split.
//! The structs themselves live in the telemetry crate; this module preserves
//! the existing import path so nothing else in shell needs to change.

pub(crate) use xai_grok_telemetry::session_metrics::{
    DoomLoopRecovery, SessionStarted, TraceUploadAttempted, TraceUploadFailed, TraceUploadSkipped,
    TraceUploadSucceeded, Turn, TurnCompletedLifecycle,
};
