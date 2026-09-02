//! Telemetry engine for Grok Build sessions.
//! Covers product events, Mixpanel emission, Sentry error reporting, OpenTelemetry tracing, and the structured unified log.
//!
//! Extracted from `xai-file-utils` so telemetry has its own ownership boundary (see CODEOWNERS).
//! Consumers that only want event tracking and inference metrics no longer pull in Mixpanel/HTTP/identity dependencies.

pub mod activity;
mod appender;
pub mod client;
pub mod config;
pub mod context;
pub mod debug_log;
pub mod enums;
pub mod events;
pub mod external;
pub mod hooks_log;
pub mod http;
pub mod id;
pub mod instrumentation;
pub mod memory_log;
pub mod memory_telemetry;
pub mod otel_layer;
pub(crate) mod otlp_http;
pub mod process_info;
pub mod process_metrics;
pub mod prompt_timing;
pub(crate) mod redact_common;
pub mod region;
pub mod sampling_log;
pub mod sentry;
pub mod session_ctx;
pub mod session_end;
pub mod session_metrics;
pub mod span_profile;
pub mod startup;
pub mod subagent_spawn;
pub mod unified_log;

pub use client::{
    Metadata, TelemetryClient, UserContext, init, init_if_needed, is_enabled,
    is_session_metrics_enabled,
};
pub use events::TelemetryEvent;
pub use session_ctx::{
    EmitterOrigin, TelemetryCtx, emit_event, emit_event_with_origin, log_event, log_event_dual,
    log_session_event, log_session_event_with_origin, spawn_local_in_session_ctx, with_session_ctx,
};
