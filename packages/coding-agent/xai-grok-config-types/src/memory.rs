//! Memory-system configuration value types, extracted from xai-grok-shell
//! (config dependency inversion).
//!
//! These are the leaf `[memory.*]` and `[compaction.*]` sub-config structs.
//! The `MemoryConfig` aggregate and its `resolve()` loader stay in
//! `xai-grok-shell` — `resolve()` depends on `toml` and on shell-internal
//! flag resolution, and is part of shell's public API (cross-crate caller).

use serde::{Deserialize, Serialize};

/// Index and chunking configuration (`[memory.index]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MemoryIndexConfig {
    /// Maximum chunk size in characters (approx tokens × 4).
    pub max_chunk_chars: usize,
    /// Character overlap between consecutive chunks.
    pub chunk_overlap_chars: usize,
}

impl Default for MemoryIndexConfig {
    fn default() -> Self {
        Self {
            max_chunk_chars: 1600,
            chunk_overlap_chars: 320,
        }
    }
}

/// Embedding provider configuration (`[memory.embedding]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MemoryEmbeddingConfig {
    /// Provider type: `"api"`, `"local"`, or `"auto"`.
    pub provider: String,
    /// Model name for the embedding API. `None` disables vector embeddings.
    pub model: Option<String>,
    /// Embedding vector dimensions.
    pub dimensions: usize,
}

impl Default for MemoryEmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: "api".to_string(),
            model: None,
            dimensions: 1024,
        }
    }
}

/// Hybrid search scoring configuration (`[memory.search]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MemorySearchConfig {
    /// Maximum number of search results to return.
    pub max_results: usize,
    /// Minimum score threshold for inclusion.
    pub min_score: f32,
    /// Weight for vector similarity in hybrid scoring.
    pub vector_weight: f32,
    /// Weight for BM25 text similarity in hybrid scoring.
    pub text_weight: f32,
    /// **Deprecated** — use `temporal_decay` instead.
    ///
    /// Per-day decay factor for recency boosting (0.0–1.0).
    /// When `temporal_decay.enabled` is true, this field is ignored.
    /// When `temporal_decay.enabled` is false and this is set, it is
    /// converted to an approximate half-life for backward compatibility:
    /// `half_life ≈ -1 / log₂(recency_decay)`.
    pub recency_decay: f32,
    /// Temporal decay configuration for time-aware scoring.
    pub temporal_decay: TemporalDecayConfig,
    /// MMR diversity re-ranking configuration (opt-in).
    pub mmr: MmrConfig,
    /// Source-type weight multipliers: all default to 1.0.
    pub source_weights: std::collections::HashMap<String, f32>,
}

impl Default for MemorySearchConfig {
    fn default() -> Self {
        let mut source_weights = std::collections::HashMap::new();
        source_weights.insert("workspace".to_string(), 1.0);
        source_weights.insert("session".to_string(), 1.0);
        source_weights.insert("global".to_string(), 1.0);

        Self {
            max_results: 6,
            min_score: 0.35,
            vector_weight: 0.7,
            text_weight: 0.3,
            recency_decay: DEFAULT_RECENCY_DECAY,
            temporal_decay: TemporalDecayConfig::default(),
            mmr: MmrConfig::default(),
            source_weights,
        }
    }
}

/// Temporal decay configuration for time-aware search scoring.
///
/// Controls how memory chunk scores decay over time. Chunks from
/// "evergreen" sources (`global`, `workspace`) are exempt from decay
/// since they contain curated long-term knowledge. Only `session`
/// chunks decay, using an exponential half-life formula:
///
/// ```text
/// decayed_score = base_score × e^(-λ × age_days)
/// where λ = ln(2) / half_life_days
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct TemporalDecayConfig {
    /// Whether temporal decay is enabled.
    pub enabled: bool,
    /// Number of days after which a session chunk's score is halved.
    pub half_life_days: f64,
}

impl Default for TemporalDecayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            half_life_days: 7.0,
        }
    }
}

/// MMR (Maximal Marginal Relevance) diversity re-ranking configuration.
///
/// When enabled, re-ranks search results to penalize redundancy. Uses
/// Jaccard similarity on tokenized snippets to measure inter-result
/// similarity, then greedily selects results that balance relevance
/// with diversity:
///
/// ```text
/// MMR(d) = λ × relevance(d) - (1-λ) × max_similarity(d, selected)
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MmrConfig {
    /// Whether MMR re-ranking is enabled. Default: false (opt-in).
    pub enabled: bool,
    /// Trade-off between relevance and diversity.
    /// 0.0 = maximum diversity, 1.0 = pure relevance (no re-ranking).
    /// Clamped to [0.0, 1.0] at parse time. Default: 0.7.
    #[serde(deserialize_with = "deserialize_clamped_unit")]
    pub lambda: f64,
}

impl Default for MmrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lambda: 0.7,
        }
    }
}

/// Deserialize an `f64` clamped to [0.0, 1.0].
///
/// Used for fields where values outside the unit interval are meaningless
/// (e.g. cosine similarity thresholds, trade-off lambdas).
fn deserialize_clamped_unit<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = f64::deserialize(deserializer)?;
    Ok(v.clamp(0.0, 1.0))
}

/// Like [`deserialize_clamped_unit`] but for `Option<f64>` fields.
fn deserialize_clamped_unit_option<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<f64> = Option::deserialize(deserializer)?;
    Ok(v.map(|x| x.clamp(0.0, 1.0)))
}

/// Default value for the legacy `recency_decay` field.
pub const DEFAULT_RECENCY_DECAY: f32 = 0.95;

impl MemorySearchConfig {
    /// Resolve the effective half-life for temporal decay.
    ///
    /// Priority order:
    /// 1. `temporal_decay.enabled = true` → use `temporal_decay.half_life_days`
    /// 2. `temporal_decay.enabled = false` AND `recency_decay` differs from
    ///    the default (0.95) → convert the legacy per-day factor to an
    ///    approximate half-life: `half_life ≈ -1.0 / log₂(recency_decay)`.
    ///    This preserves behavior for users who only set `recency_decay`.
    /// 3. Otherwise → `None` (decay fully disabled).
    pub fn effective_half_life_days(&self) -> Option<f64> {
        if self.temporal_decay.enabled {
            if self.temporal_decay.half_life_days <= 0.0 {
                tracing::warn!(
                    half_life_days = self.temporal_decay.half_life_days,
                    "temporal_decay.half_life_days must be positive, disabling decay"
                );
                return None;
            }
            return Some(self.temporal_decay.half_life_days);
        }

        // Legacy backward compat: if the user explicitly set recency_decay
        // to a non-default value, convert it to an approximate half-life.
        if (self.recency_decay - DEFAULT_RECENCY_DECAY).abs() > f32::EPSILON
            && self.recency_decay > 0.0
            && self.recency_decay < 1.0
        {
            let half_life = -1.0 / (self.recency_decay as f64).log2();
            tracing::info!(
                recency_decay = self.recency_decay,
                converted_half_life_days = half_life,
                "converting legacy recency_decay to temporal decay half-life; \
                 consider migrating to [memory.search.temporal_decay]"
            );
            return Some(half_life);
        }

        None
    }
}

/// First-turn memory injection configuration (`[memory.initial_injection]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MemoryInitialInjectionConfig {
    /// Whether to search memory and inject a reminder on the first turn.
    pub enabled: bool,
    /// Optional score threshold override for first-turn injection.
    /// When `None`, the first-turn search uses the historical default of `0.0`
    /// (no threshold filtering).
    pub min_score: Option<f32>,
}

impl Default for MemoryInitialInjectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_score: None,
        }
    }
}

/// Session lifecycle configuration (`[memory.session]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MemorySessionConfig {
    /// Whether to auto-save a session summary to memory on session end.
    pub save_on_end: bool,
}

impl Default for MemorySessionConfig {
    fn default() -> Self {
        Self { save_on_end: true }
    }
}

/// autoDream consolidation configuration (`[memory.dream]`).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct MemoryDreamConfig {
    /// Whether autoDream background consolidation is enabled.
    pub enabled: bool,
    /// Minimum hours between consolidations.
    pub min_hours: u64,
    /// Minimum sessions since last consolidation to trigger.
    pub min_sessions: u64,
    /// Seconds before a stale dream lock is reclaimed.
    pub stale_lock_secs: u64,
    /// Periodic dream check interval in seconds.
    /// `None` = disabled (dream only at session end or via /dream).
    /// When set, the session actor checks dream gates on this interval.
    pub check_interval_secs: Option<u64>,
}

impl Default for MemoryDreamConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_hours: 4,
            min_sessions: 3,
            stale_lock_secs: 3600,
            check_interval_secs: None,
        }
    }
}

/// File watcher configuration for detecting external memory edits (`[memory.watcher]`).
///
/// When enabled, watches `~/.grok/memory/` for `.md` file changes (create,
/// modify, delete) and syncs the index on the next `memory_search` call:
/// - Created/modified files are reindexed.
/// - Deleted files have their stale chunks removed from the index.
///
/// Events are coalesced in a lock-free `ArcSwap` set; sync runs at most once
/// per search call when dirty files are present and the claim is acquired.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MemoryWatcherConfig {
    /// Whether the file watcher is enabled. Default: true (when memory is enabled).
    pub enabled: bool,
    /// Seconds after which a reindex claim is considered stale (crashed agent).
    /// Default: 60.
    pub stale_claim_secs: i64,
}

impl Default for MemoryWatcherConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            stale_claim_secs: 60,
        }
    }
}

/// Garbage collection for orphaned workspace memory directories (`[memory.gc]`).
///
/// On session init, directories under `~/.grok/memory/` are scanned:
/// - `tmp*` dirs: empty ones removed unconditionally, non-empty ones removed
///   after 7 days.
/// - Other workspaces with no session files: removed after `max_age_days`.
/// - Non-empty non-tmp workspaces: never touched.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct MemoryGcConfig {
    pub max_age_days: u64,
}

impl Default for MemoryGcConfig {
    fn default() -> Self {
        Self { max_age_days: 30 }
    }
}

/// Pre-compaction memory flush configuration (`[compaction.memory_flush]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryFlushConfig {
    /// Whether the flush step is enabled before compaction.
    pub enabled: bool,
    /// Token headroom before the compact threshold to trigger flush.
    pub soft_threshold_tokens: u64,
    /// Model to use for the flush turn. `None` = session's primary model.
    pub flush_model: Option<String>,
    /// Max characters the flush response may write to memory.
    pub max_flush_write_chars: usize,
    /// Idle timeout in seconds: when no user message is received for this
    /// duration, a background flush is triggered automatically.
    /// `None` = disabled (flush only before compaction).
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
    /// Cosine similarity threshold for semantic dedup of flush content.
    /// When `None`, falls back to the compiled-in default (0.92).
    /// Clamped to [0.0, 1.0] at parse time.
    #[serde(default, deserialize_with = "deserialize_clamped_unit_option")]
    pub semantic_dedup_threshold: Option<f64>,
}

impl Default for MemoryFlushConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            soft_threshold_tokens: 4000,
            flush_model: None,
            max_flush_write_chars: 8000,
            idle_timeout_secs: None,
            semantic_dedup_threshold: None,
        }
    }
}

/// Tool-result pruning configuration (`[compaction.pruning]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PruningConfig {
    /// Whether pruning is enabled.
    pub enabled: bool,
    /// Number of recent turns whose tool results are never pruned.
    pub keep_last_n_turns: usize,
    /// Character threshold above which old tool results are soft-trimmed.
    pub soft_trim_threshold: usize,
    /// Characters to keep from the start of a soft-trimmed result.
    pub soft_trim_head: usize,
    /// Characters to keep from the end of a soft-trimmed result.
    pub soft_trim_tail: usize,
    /// Turn age after which tool results are hard-cleared (replaced with placeholder).
    pub hard_clear_age_turns: usize,
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_last_n_turns: 3,
            soft_trim_threshold: 4000,
            soft_trim_head: 1500,
            soft_trim_tail: 1500,
            hard_clear_age_turns: 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_config_defaults_match() {
        assert_eq!(MemoryIndexConfig::default().max_chunk_chars, 1600);
        assert_eq!(MemoryEmbeddingConfig::default().dimensions, 1024);
        let s = MemorySearchConfig::default();
        assert_eq!(s.max_results, 6);
        assert_eq!(s.recency_decay, DEFAULT_RECENCY_DECAY);
        assert!(s.temporal_decay.enabled);
        assert!(!s.mmr.enabled);
        assert!(MemorySessionConfig::default().save_on_end);
        assert_eq!(MemoryGcConfig::default().max_age_days, 30);
        assert_eq!(PruningConfig::default().keep_last_n_turns, 3);
    }

    #[test]
    fn mmr_lambda_is_clamped_on_deserialize() {
        let m: MmrConfig = serde_json::from_str(r#"{"enabled": true, "lambda": 5.0}"#).unwrap();
        assert_eq!(m.lambda, 1.0);
        let m: MmrConfig = serde_json::from_str(r#"{"enabled": true, "lambda": -3.0}"#).unwrap();
        assert_eq!(m.lambda, 0.0);
    }

    #[test]
    fn flush_semantic_dedup_threshold_clamped_option() {
        let f: MemoryFlushConfig =
            serde_json::from_str(r#"{"semantic_dedup_threshold": 2.0}"#).unwrap();
        assert_eq!(f.semantic_dedup_threshold, Some(1.0));
        let f: MemoryFlushConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(f.semantic_dedup_threshold, None);
    }

    #[test]
    fn effective_half_life_prefers_temporal_decay() {
        let mut s = MemorySearchConfig::default();
        s.temporal_decay.enabled = true;
        s.temporal_decay.half_life_days = 14.0;
        assert_eq!(s.effective_half_life_days(), Some(14.0));
    }

    #[test]
    fn effective_half_life_converts_legacy_recency_decay() {
        let mut s = MemorySearchConfig::default();
        s.temporal_decay.enabled = false;
        s.recency_decay = 0.5; // non-default → converted
        let hl = s.effective_half_life_days().unwrap();
        assert!(
            (hl - 1.0).abs() < 1e-9,
            "0.5 per-day decay ⇒ ~1 day half-life, got {hl}"
        );
    }

    #[test]
    fn effective_half_life_none_when_disabled_and_default_recency() {
        let mut s = MemorySearchConfig::default();
        s.temporal_decay.enabled = false;
        // recency_decay left at default ⇒ no decay
        assert_eq!(s.effective_half_life_days(), None);
    }
}
