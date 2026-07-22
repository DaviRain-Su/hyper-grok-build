#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
#[cfg(all(test, feature = "dhat-heap"))]
#[global_allocator]
static DHAT_ALLOC: dhat::Alloc = dhat::Alloc;
pub(crate) use xai_grok_telemetry::unified_log;
pub use xai_tracing_macros::{teprintln, timed, tprintln};
pub mod active_sessions;
pub mod agent;
pub mod auth;
pub mod builtin;
pub mod bundle;
pub mod claude_import;
pub mod claude_import_state;
pub mod cli_models;
pub mod config;
pub use xai_grok_shell_base::cpu_profile;
pub use xai_grok_shell_base::env;
pub mod extensions;
pub use xai_grok_workspace::foreign_sessions;
pub mod heap_profile;
pub use xai_grok_http as http;
pub mod inspect;
pub mod instrumentation;
pub mod leader;
pub mod managed_config;
pub mod main_runtime;
pub mod mcp_doctor;
pub use xai_grok_models as models;
pub mod plugin;
pub mod relay;
pub mod remote;
pub mod sampling;
pub mod session;
pub mod terminal;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tier;
pub mod tools;
pub mod trace_classifier;
pub mod upload;
pub mod util;
