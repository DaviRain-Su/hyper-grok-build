//! API-agnostic conversation representation.
//!
//! The canonical types now live in `xai_grok_sampling_types::conversation`.
//! This module re-exports them and adds grok-shell-specific types
//! (`ConversationRequestTrace`) that depend on internal crate types.

// Re-export everything from the standalone crate.
pub use xai_grok_sampling_types::conversation::*;

// ============================================================================
// grok-shell-specific types (depend on internal crate types)
// ============================================================================

/// Tracing context for conversation requests.
///
/// Stays in `xai-grok-shell` because it references
/// `TraceExportConfig` (a shell-internal type) and the
/// `ArtifactTracker` from the upload pipeline. The legacy
/// `stream_via_*` path used `artifact_tracker` to spawn fire-and-
/// forget GCS uploads of the request payload; that path was later removed
/// without re-wiring trace upload through the new sampler. The field
/// is kept so the struct shape stays compatible with persisted snapshots
/// and so trace upload can be re-enabled on the sampler path without a
/// schema change.
#[derive(Debug, Clone)]
pub struct ConversationRequestTrace {
    pub gcs_config: crate::session::repo_changes::TraceExportConfig,
    #[expect(
        dead_code,
        reason = "retained for snapshot compat; wire when sampler path uploads traces"
    )]
    pub(crate) artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
}

// `ConversationRequestTrace` satisfies the `TraceContext` trait bounds
// (`Clone + Send + Sync + Debug + 'static`) via the blanket impl, so it can
// be stored in `ConversationRequest.trace` and `ChatCompletionRequest.trace`
// via `Box::new(trace)`.
//
// Tests for conversation types now live in xai-grok-sampling-types crate.
