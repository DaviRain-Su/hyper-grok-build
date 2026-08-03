//! Memory subsystem telemetry. Routes through `log_event` (product tier,
//! `Enabled` mode only). No PII or user content -- only counts, scores,
//! durations, and config values.

use serde::Serialize;

#[derive(Serialize)]
pub struct MemorySessionInit {
    pub session_id: String,
    pub memory_enabled: bool,
    pub watcher_config_enabled: bool,
    pub watcher_started: bool,
    pub temporal_decay_enabled: bool,
    pub mmr_enabled: bool,
    pub mmr_lambda: f64,
    pub half_life_days: f64,
    pub embedding_dimensions: usize,
    pub total_chunks: usize,
    pub total_files: usize,
    pub has_global_memory_md: bool,
    pub has_workspace_memory_md: bool,
}

#[derive(Serialize)]
pub struct MemorySearch {
    pub session_id: String,
    pub query_length: usize,
    pub keyword_count: usize,
    pub result_count: usize,
    pub top_score: f64,
    pub min_score_threshold: f64,
    pub search_mode: String,
    pub duration_ms: u64,
    pub vec_available: bool,
    pub source: String,
}

#[derive(Serialize)]
pub struct MemorySearchEmpty {
    pub session_id: String,
    pub query_length: usize,
    pub keyword_count: usize,
    pub min_score_threshold: f64,
    pub search_mode: String,
    pub duration_ms: u64,
    pub vec_available: bool,
    pub source: String,
}

#[derive(Serialize)]
pub struct MemoryFlushStart {
    pub session_id: String,
    pub trigger: String,
    pub conversation_len: usize,
    pub user_message_count: usize,
}

#[derive(Serialize)]
pub struct MemoryFlushComplete {
    pub session_id: String,
    pub trigger: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub response_length: usize,
    pub accepted_length: usize,
    pub was_truncated: bool,
}

#[derive(Serialize)]
pub struct MemoryInjection {
    pub session_id: String,
    pub was_greeting_fallback: bool,
    pub result_count: usize,
    pub total_snippet_chars: usize,
    pub top_score: f64,
    pub configured_min_score: f64,
    pub injection_duration_ms: u64,
}

#[derive(Serialize)]
pub struct MemoryReindex {
    pub session_id: String,
    pub source: String,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub embedded: usize,
    pub duration_ms: u64,
    pub trigger: String,
}

#[derive(Serialize)]
pub struct MemoryWatcherSync {
    pub session_id: String,
    pub dirty_file_count: usize,
    pub claimed: bool,
    pub reindexed_count: usize,
    pub embedded_count: usize,
    pub duration_ms: u64,
}

#[derive(Serialize)]
pub struct MemorySessionSummary {
    pub session_id: String,
    pub session_duration_secs: u64,
    pub flush_count: u64,
    pub flush_success_count: u64,
    pub flush_error_count: u64,
    pub tool_search_count: u64,
    pub injection_count: u64,
    pub recovery_search_count: u64,
    pub total_chunks_at_end: usize,
    pub chunks_added_this_session: usize,
    pub session_end_result: String,
    pub dream_count: u64,
    pub dream_success_count: u64,
    pub dream_error_count: u64,
}
