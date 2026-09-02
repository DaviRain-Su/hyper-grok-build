//! Session lifecycle event structs.
//!
//! The structs moved to `xai-grok-telemetry` in the telemetry crate split.
//! This module re-exports them so the existing import path in shell keeps working.

pub(crate) use xai_grok_telemetry::session_metrics::{
    DoomLoopDetected, DoomLoopRecovery, SessionContextSnapshot, SessionStartKind, SessionStarted,
    TraceUploadAttempted, TraceUploadFailed, TraceUploadSkipped, TraceUploadSucceeded, Turn,
    TurnCompletedLifecycle,
};
