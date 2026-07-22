//! Pre-compaction memory flush logic.
//!
//! Before compacting the conversation, the session actor can optionally run
//! a "flush turn" that asks the model to summarize important information
//! for storage in memory files.
//!
//! This module provides:
//! - `should_flush()` — threshold check for when to trigger the flush
//! - `FLUSH_SYSTEM_PROMPT` — the prompt sent to the model during flush
//! - `process_flush_response()` — quality controls on the model's output
//! - `is_semantically_duplicate()` — embedding-based dedup gate before writing
//!
//! The session actor orchestrates the flush by:
//! 1. Setting `is_flushing = true` (suppresses auto-compact)
//! 2. Sending `FLUSH_SYSTEM_PROMPT` to the model (no tools offered)
//! 3. Calling `process_flush_response()` on the result
//! 4. Calling `is_semantically_duplicate()` to skip near-duplicate content
//! 5. Writing to `MemoryStorage::write_daily_log()` if accepted and not duplicate
//! 6. Setting `is_flushing = false`

use crate::config::MemoryFlushConfig;
use crate::sampling::{ChatRequestMessage, Role};
// Pure text helpers moved into the memory subsystem (breaks the
// dream <-> memory_flush module cycle).
use crate::session::memory::text_utils::{has_markdown_headers, is_no_reply};

/// Memory log target — matches `xai_grok_telemetry::memory_log::TARGET`.
const LOG: &str = "xai_memory";

/// Check whether a memory flush should run before the next compaction.
///
/// Returns `true` when ALL of the following are met:
/// - `flush_config.enabled` is `true`
/// - This flush hasn't already run for the current compaction cycle
///   (`last_flush_compaction != current_compaction_count`)
/// - Token usage has reached the flush threshold (compact threshold
///   minus `soft_threshold_tokens` headroom)
///
/// The flush threshold sits below the compact threshold so the flush
/// completes before the context window overflows.
pub fn should_flush(
    total_tokens: u64,
    context_window: u64,
    compact_threshold_percent: u8,
    flush_config: &MemoryFlushConfig,
    last_flush_compaction: u64,
    current_compaction_count: u64,
) -> bool {
    if !flush_config.enabled {
        tracing::debug!(target: LOG, "MEMORY_FLUSH_CHECK: disabled");
        return false;
    }
    if last_flush_compaction == current_compaction_count {
        tracing::debug!(target: LOG,
            "MEMORY_FLUSH_CHECK: already flushed this cycle (cycle={current_compaction_count})");
        return false;
    }
    let should = xai_token_estimation::exceeds_threshold_with_headroom(
        total_tokens,
        context_window,
        compact_threshold_percent,
        flush_config.soft_threshold_tokens,
    );
    // Approximate threshold for log readability; the decision uses scaled
    // arithmetic above and may differ by 1 token at non-round windows.
    let flush_threshold = context_window
        .saturating_mul(compact_threshold_percent as u64)
        .saturating_sub(flush_config.soft_threshold_tokens.saturating_mul(100))
        / 100;
    tracing::info!(target: LOG,
        "MEMORY_FLUSH_CHECK: tokens={total_tokens} threshold={flush_threshold} \
         window={context_window} pct={compact_threshold_percent} soft={soft} -> {result}",
        soft = flush_config.soft_threshold_tokens,
        result = if should { "FLUSH" } else { "skip" },
    );
    should
}

// ---------------------------------------------------------------------------
// Flush prompt and response processing
// ---------------------------------------------------------------------------

/// System prompt injected for the flush model call.
pub const FLUSH_SYSTEM_PROMPT: &str = "\
You are a memory assistant. Extract ALL useful information from this conversation \
that would help you be more effective in future sessions with this user. \
Write a concise markdown summary with ## headers covering:

- **Decisions & rationale** — what was chosen and why
- **Technical context** — architecture, APIs, patterns, tools, file paths discussed
- **Debugging techniques & tools** — external APIs, CLI commands, query patterns, \
investigation workflows, or services discovered or used during debugging
- **Problems & solutions** — bugs found, how they were fixed, workarounds

Omit any section where there is nothing substantive to report. \
Do NOT include user preferences like OS, shell, or editor — these belong in global memory. \
Do NOT include an ephemeral progress section — transient status is not useful for future sessions.

Respond with NO_REPLY if nothing genuinely useful was learned — a routine task \
that followed standard patterns, brief Q&A, or sessions with no novel decisions \
or discoveries are not worth persisting. Only write content that a future session \
would concretely benefit from.";

/// System prompt for incremental (delta) flushes after the first flush.
///
/// Used when `flush_count > 0` and previous flush content is available.
/// The caller appends the previous flush output after this prompt.
pub const FLUSH_DELTA_SYSTEM_PROMPT: &str = "\
You are a memory assistant performing an incremental update. The previous \
flush output for this session is shown below. Extract ONLY information that \
is NEW since the previous flush — do not repeat anything already captured.

Write a concise markdown summary with ## headers covering only NEW items in:
- **Decisions & rationale** — new decisions since last flush
- **Technical context** — new architecture, APIs, patterns discovered
- **Debugging techniques** — new techniques used since last flush
- **Problems & solutions** — new bugs found and fixes

Omit any section that has no new content. Do NOT include user preferences \
(OS, shell, paths) — these are captured in global memory.
Do NOT include 'Current state' — this is ephemeral and not useful for future sessions.

Respond with NO_REPLY if nothing genuinely new and useful has happened since \
the previous flush. Routine changes that follow standard patterns are not worth \
an incremental update.

--- Previous flush content ---
";

/// Result of processing the model's flush response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushResult {
    /// Model indicated nothing to store (empty response or NO_REPLY).
    NothingToStore,
    /// Response was accepted after quality checks. Contains the content to write.
    /// The caller should run [`is_semantically_duplicate()`] before writing.
    Accepted(String),
    /// Response was rejected by quality controls.
    Rejected(String),
}

/// Process the model's flush response, applying quality controls.
///
/// Quality checks:
/// 1. Empty/whitespace-only → `NothingToStore`
/// 2. Matches `NO_REPLY` pattern → `NothingToStore`
/// 3. Exceeds `max_flush_write_chars` → truncated
/// 4. Must contain at least one markdown header (`##`) → `Rejected` if not
pub fn process_flush_response(response: &str, config: &MemoryFlushConfig) -> FlushResult {
    let trimmed = response.trim();
    let len = trimmed.len();
    let preview: String = trimmed.chars().take(200).collect();

    tracing::info!(target: LOG,
        "MEMORY_FLUSH_RESPONSE: len={len} preview=\"{preview}\"");

    // Check for empty
    if trimmed.is_empty() {
        tracing::info!(target: LOG,
            "MEMORY_FLUSH_RESPONSE: empty → NothingToStore");
        return FlushResult::NothingToStore;
    }

    // Check for NO_REPLY
    if is_no_reply(trimmed) {
        tracing::info!(target: LOG,
            "MEMORY_FLUSH_RESPONSE: matches NO_REPLY pattern → NothingToStore");
        return FlushResult::NothingToStore;
    }

    // Truncate if too long (use char count for consistency with .chars().take())
    let content = if trimmed.chars().count() > config.max_flush_write_chars {
        tracing::warn!(target: LOG,
            "MEMORY_FLUSH_RESPONSE: truncated from {len} to {} chars",
            config.max_flush_write_chars);
        trimmed
            .chars()
            .take(config.max_flush_write_chars)
            .collect::<String>()
    } else {
        trimmed.to_string()
    };

    // Must contain at least one markdown header for structure
    if !has_markdown_headers(&content) {
        tracing::info!(target: LOG,
            "MEMORY_FLUSH_RESPONSE: no markdown headers → Rejected");
        return FlushResult::Rejected(
            "flush response lacks markdown structure (no ## headers)".to_string(),
        );
    }

    tracing::info!(target: LOG,
        "MEMORY_FLUSH_RESPONSE: accepted ({} chars, has headers)", content.len());
    FlushResult::Accepted(content)
}

/// Check if content is substantially similar to any existing memory chunk.
///
/// Returns `true` if an exact blake3 hash match is found (should skip write).
/// Uses open-per-query to avoid `!Send` issues with `rusqlite::Connection`.
pub fn is_duplicate(content: &str, db_path: &std::path::Path) -> bool {
    let content_hash = blake3::hash(content.as_bytes()).to_hex().to_string();

    // Journal-mode-aware open: never mmap a legacy WAL -shm on network
    // mounts (SIGBUS); see xai_sqlite_journal::JournalMode::open_readonly.
    let conn = match xai_sqlite_journal::JournalMode::for_db_path(db_path).open_readonly(db_path) {
        Ok(c) => c,
        Err(_) => {
            tracing::debug!(target: LOG, "MEMORY_FLUSH_DEDUP: can't open DB, allowing write");
            return false;
        }
    };

    let exact_match: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM chunks WHERE hash = ?1)",
            rusqlite::params![content_hash],
            |r| r.get(0),
        )
        .unwrap_or(false);

    tracing::info!(target: LOG,
        "MEMORY_FLUSH_DEDUP: hash={hash} duplicate={exact_match}",
        hash = &content_hash[..12],
    );

    exact_match
}

/// Cosine similarity threshold above which flush content is considered a
/// semantic duplicate of an existing memory chunk. A value of 0.92 is
/// conservative — it catches near-identical rephrasings while allowing
/// content that adds meaningful new information to pass through.
///
/// Used as the fallback when no config override is set.
pub(crate) const SEMANTIC_DEDUP_SIMILARITY_THRESHOLD: f64 = 0.92;

/// Maximum L2 distance between two unit-norm embedding vectors (used to
/// convert sqlite-vec L2 distances to cosine similarity).
const MAX_L2_DISTANCE: f64 = 2.0;

/// Number of nearest neighbors to check during semantic dedup.
const SEMANTIC_DEDUP_KNN_LIMIT: usize = 3;

/// Check if flush content is semantically similar to existing memory chunks.
///
/// Uses the embedding provider to embed the flush content, then runs a KNN
/// search against the memory index. If any result exceeds `threshold`,
/// considers the content a duplicate.
///
/// `threshold` is the cosine similarity cutoff (0.0–1.0). Pass
/// `SEMANTIC_DEDUP_SIMILARITY_THRESHOLD` for the compiled-in default, or
/// a value from config for remote/local overrides.
///
/// Falls back gracefully: returns `false` (allow write) if embeddings are
/// unavailable, the index has no vector support, or any step fails.
///
/// Structured as sync/async/sync phases so `&MemoryIndex` (which contains
/// `!Send` `rusqlite::Connection`) is never held across `.await` boundaries,
/// matching the pattern used in `search.rs` and `backend.rs`.
pub async fn is_semantically_duplicate(
    content: &str,
    index: &crate::session::memory::MemoryIndex,
    embedding_provider: Option<&dyn crate::session::memory::embedding::EmbeddingProvider>,
    threshold: f64,
) -> bool {
    // Phase 1 (sync): check prerequisites — borrows index, no .await
    let provider = match embedding_provider {
        Some(p) => p,
        None => {
            tracing::debug!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: no embedding provider, skipping");
            return false;
        }
    };

    if !index.vec_available() {
        tracing::debug!(target: LOG,
            "MEMORY_FLUSH_SEMANTIC_DEDUP: sqlite-vec not available, skipping");
        return false;
    }

    // Phase 2 (async): embed — no &index borrow across this .await
    let embedding = match provider.embed_batch(&[content]).await {
        Ok(mut vecs) if !vecs.is_empty() => vecs.swap_remove(0),
        Ok(_) => {
            tracing::warn!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: embedding returned empty result");
            return false;
        }
        Err(e) => {
            tracing::warn!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: embedding failed: {e}");
            return false;
        }
    };

    // Phase 3 (sync): vector search + threshold check — borrows index, no .await
    let neighbors = match index.vector_search(&embedding, SEMANTIC_DEDUP_KNN_LIMIT) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: vector search failed: {e}");
            return false;
        }
    };

    let mut max_sim = 0.0_f64;
    for (chunk_id, distance) in &neighbors {
        let similarity = (1.0 - (*distance as f64 / MAX_L2_DISTANCE)).clamp(0.0, 1.0);
        max_sim = max_sim.max(similarity);
        if similarity > threshold {
            tracing::info!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: duplicate detected \
                 (chunk={chunk_id}, similarity={similarity:.4}, \
                 threshold={threshold})");
            return true;
        }
    }

    tracing::info!(target: LOG,
        "MEMORY_FLUSH_SEMANTIC_DEDUP: no duplicate \
         (checked={}, max_similarity={max_sim:.4}, \
         threshold={threshold})",
        neighbors.len());
    false
}

/// Select a recent window from simplified chat messages for the flush model.
///
/// Starts with the last `recent_message_count` messages, then expands backward
/// to the nearest `User` message so the window always starts on a user
/// boundary. The returned window may be larger than `recent_message_count`.
/// System messages are excluded since the flush adds its own system prompt.
pub fn select_flush_window(
    messages: Vec<ChatRequestMessage>,
    recent_message_count: usize,
) -> Vec<ChatRequestMessage> {
    let messages: Vec<_> = messages
        .into_iter()
        .filter(|m| m.role != Role::System)
        .collect();

    let total = messages.len();
    let mut start = total.saturating_sub(recent_message_count);
    while start > 0 && messages[start].role != Role::User {
        start -= 1;
    }
    messages.into_iter().skip(start).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_flush_config() -> MemoryFlushConfig {
        MemoryFlushConfig::default()
    }

    #[test]
    fn test_should_flush_disabled() {
        let config = MemoryFlushConfig {
            enabled: false,
            ..default_flush_config()
        };
        assert!(!should_flush(90_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_already_flushed_this_cycle() {
        let config = default_flush_config();
        // same compaction count → already flushed
        assert!(!should_flush(90_000, 100_000, 85, &config, 1, 1));
    }

    #[test]
    fn test_should_flush_below_threshold() {
        let config = default_flush_config();
        // 100K context, 85% compact = 85K, flush at 85K - 4K = 81K
        // 50K tokens → below threshold
        assert!(!should_flush(50_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_at_threshold() {
        let config = default_flush_config();
        // 100K context, 85% compact = 85K, flush at 85K - 4K = 81K
        // 81K tokens → at threshold → should flush
        assert!(should_flush(81_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_above_threshold() {
        let config = default_flush_config();
        assert!(should_flush(83_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_custom_soft_threshold() {
        let config = MemoryFlushConfig {
            soft_threshold_tokens: 10_000,
            ..default_flush_config()
        };
        // 100K context, 85% compact = 85K, flush at 85K - 10K = 75K
        assert!(!should_flush(74_000, 100_000, 85, &config, 0, 1));
        assert!(should_flush(75_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_different_compaction_cycles() {
        let config = default_flush_config();
        // First cycle: should flush (counter is pre-incremented to 1 in run_compact)
        assert!(should_flush(82_000, 100_000, 85, &config, 0, 1));
        // After flush (same cycle): should not flush again
        assert!(!should_flush(82_000, 100_000, 85, &config, 1, 1));
        // New cycle: should flush again
        assert!(should_flush(82_000, 100_000, 85, &config, 1, 2));
    }

    #[test]
    fn test_should_flush_non_round_window() {
        // cw=10_001, pct=85, soft=4_000. Scaled boundary:
        // used*100 >= 10_001*85 - 4_000*100 = 850_085 - 400_000 = 450_085
        // -> false at used=4500, true at used=4501.
        let config = MemoryFlushConfig {
            soft_threshold_tokens: 4_000,
            ..default_flush_config()
        };
        assert!(!should_flush(4_499, 10_001, 85, &config, 0, 1));
        assert!(!should_flush(4_500, 10_001, 85, &config, 0, 1));
        assert!(should_flush(4_501, 10_001, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_same_counter_values_blocks() {
        let config = default_flush_config();
        // Equal counters → "already flushed this cycle" guard fires.
        // Both starting at 0 is the initial state; pre-increment in
        // maybe_pre_compaction_flush() prevents this from blocking the first flush.
        assert!(!should_flush(82_000, 100_000, 85, &config, 0, 0));
        assert!(!should_flush(82_000, 100_000, 85, &config, 5, 5));
    }

    // -----------------------------------------------------------------------
    // process_flush_response tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_flush_response_empty() {
        let config = default_flush_config();
        assert_eq!(
            process_flush_response("", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("   ", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("\n\n", &config),
            FlushResult::NothingToStore
        );
    }

    #[test]
    fn test_flush_response_no_reply_variants() {
        let config = default_flush_config();
        assert_eq!(
            process_flush_response("NO_REPLY", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("no reply", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("No-Reply", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("noreply", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("  NO_REPLY  ", &config),
            FlushResult::NothingToStore
        );
    }

    #[test]
    fn test_flush_response_accepted() {
        let config = default_flush_config();
        let content = "## Key Decisions\n\nWe chose Rust for performance.";
        assert_eq!(
            process_flush_response(content, &config),
            FlushResult::Accepted(content.to_string())
        );
    }

    #[test]
    fn test_flush_response_rejected_no_headers() {
        let config = default_flush_config();
        let content = "Just some plain text without any markdown headers at all.";
        assert!(matches!(
            process_flush_response(content, &config),
            FlushResult::Rejected(_)
        ));
    }

    #[test]
    fn test_flush_response_truncated() {
        let config = MemoryFlushConfig {
            max_flush_write_chars: 50,
            ..default_flush_config()
        };
        let content = format!("# Title\n\n{}", "x".repeat(100));
        let result = process_flush_response(&content, &config);
        if let FlushResult::Accepted(text) = result {
            assert!(text.chars().count() <= 50);
        } else {
            panic!("expected Accepted, got {result:?}");
        }
    }

    #[test]
    fn test_flush_response_h1_header_accepted() {
        let config = default_flush_config();
        let content = "# Top Level\n\nSome content.";
        assert!(matches!(
            process_flush_response(content, &config),
            FlushResult::Accepted(_)
        ));
    }

    #[test]
    fn test_flush_system_prompt_content() {
        assert!(FLUSH_SYSTEM_PROMPT.contains("markdown"));
        assert!(FLUSH_SYSTEM_PROMPT.contains("NO_REPLY"));
        assert!(
            !FLUSH_SYSTEM_PROMPT.contains("Always write something"),
            "old bias toward always writing should be removed"
        );
        assert!(
            FLUSH_SYSTEM_PROMPT.contains("genuinely useful"),
            "prompt should bias toward NO_REPLY for low-value sessions"
        );
        assert!(!FLUSH_SYSTEM_PROMPT.contains("User preferences"));
    }

    #[test]
    fn test_delta_system_prompt_content() {
        assert!(FLUSH_DELTA_SYSTEM_PROMPT.contains("incremental update"));
        assert!(FLUSH_DELTA_SYSTEM_PROMPT.contains("NO_REPLY"));
        assert!(FLUSH_DELTA_SYSTEM_PROMPT.contains("Previous flush content"));
        assert!(!FLUSH_DELTA_SYSTEM_PROMPT.contains("User preferences"));
        assert!(
            FLUSH_DELTA_SYSTEM_PROMPT.contains("genuinely new and useful"),
            "delta prompt should use same selectivity standard as primary prompt"
        );
    }

    #[test]
    fn test_select_flush_window_expands_to_user_boundary() {
        let mut messages = vec![ChatRequestMessage::user("early question")];
        for i in 0..20 {
            messages.push(ChatRequestMessage::assistant(
                format!("response {i}"),
                "",
                None,
            ));
        }

        let window = select_flush_window(messages, 20);

        assert_eq!(window.len(), 21);
        assert_eq!(window[0].role, Role::User);
    }

    #[test]
    fn test_select_flush_window_filters_system_messages() {
        let messages = vec![
            ChatRequestMessage::system("you are helpful"),
            ChatRequestMessage::user("hi"),
            ChatRequestMessage::assistant("hello", "", None),
        ];

        let window = select_flush_window(messages, 20);

        assert!(window.iter().all(|m| m.role != Role::System));
        assert_eq!(window.len(), 2);
    }

    #[test]
    fn test_select_flush_window_short_conversation() {
        let messages = vec![
            ChatRequestMessage::user("hi"),
            ChatRequestMessage::assistant("hello", "", None),
        ];

        let window = select_flush_window(messages, 20);

        assert_eq!(window.len(), 2);
        assert_eq!(window[0].role, Role::User);
    }

    // -----------------------------------------------------------------------
    // is_semantically_duplicate tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_semantic_dedup_no_provider_allows_write() {
        use crate::session::memory::{MemoryIndex, MemoryStorage, index::init_sqlite_vec};
        use tempfile::TempDir;

        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage =
            MemoryStorage::with_paths(tmp.path().join("global"), tmp.path().join("workspace"));
        let index = MemoryIndex::open_or_create(&db_path, storage, Default::default(), 4).unwrap();

        // No embedding provider → always returns false (allow write).
        let result = is_semantically_duplicate(
            "## Test\n\nSome content.",
            &index,
            None,
            SEMANTIC_DEDUP_SIMILARITY_THRESHOLD,
        )
        .await;
        assert!(!result, "should allow write when no embedding provider");
    }

    #[tokio::test]
    async fn test_semantic_dedup_no_similar_content() {
        use crate::session::memory::embedding::MockEmbeddingProvider;
        use crate::session::memory::{MemoryIndex, MemoryStorage, index::init_sqlite_vec};
        use tempfile::TempDir;

        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage =
            MemoryStorage::with_paths(tmp.path().join("global"), tmp.path().join("workspace"));
        let index = MemoryIndex::open_or_create(&db_path, storage, Default::default(), 4).unwrap();

        let provider = MockEmbeddingProvider { dimensions: 4 };

        // Empty index → no neighbors → not a duplicate.
        let result = is_semantically_duplicate(
            "## New Content\n\nFresh ideas here.",
            &index,
            Some(&provider),
            SEMANTIC_DEDUP_SIMILARITY_THRESHOLD,
        )
        .await;
        assert!(!result, "should not be duplicate against empty index");
    }

    #[tokio::test]
    async fn test_semantic_dedup_detects_identical_content() {
        use crate::session::memory::embedding::{EmbeddingProvider, MockEmbeddingProvider};
        use crate::session::memory::{MemoryIndex, MemoryStorage, index::init_sqlite_vec};
        use tempfile::TempDir;

        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage =
            MemoryStorage::with_paths(tmp.path().join("global"), tmp.path().join("workspace"));
        let mut index =
            MemoryIndex::open_or_create(&db_path, storage, Default::default(), 4).unwrap();

        let provider = MockEmbeddingProvider { dimensions: 4 };
        let content = "## Decisions\n\nWe chose Rust for memory safety.";

        // Index a file containing the same content.
        let file_path = tmp.path().join("existing.md");
        std::fs::write(&file_path, content).unwrap();
        index.reindex_file(&file_path, "session").unwrap();

        // Embed the existing chunk.
        let existing_embedding = provider.embed_batch(&[content]).await.unwrap();
        let chunk_id = format!("{}:0", file_path.to_string_lossy());
        index
            .upsert_embedding(&chunk_id, &existing_embedding[0])
            .unwrap();

        // Same content → identical embedding → distance 0 → similarity 1.0 → duplicate.
        let result = is_semantically_duplicate(
            content,
            &index,
            Some(&provider),
            SEMANTIC_DEDUP_SIMILARITY_THRESHOLD,
        )
        .await;
        assert!(result, "identical content should be detected as duplicate");
    }

    #[tokio::test]
    async fn test_semantic_dedup_allows_different_content() {
        use crate::session::memory::embedding::{EmbeddingProvider, MockEmbeddingProvider};
        use crate::session::memory::{MemoryIndex, MemoryStorage, index::init_sqlite_vec};
        use tempfile::TempDir;

        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage =
            MemoryStorage::with_paths(tmp.path().join("global"), tmp.path().join("workspace"));
        let mut index =
            MemoryIndex::open_or_create(&db_path, storage, Default::default(), 4).unwrap();

        let provider = MockEmbeddingProvider { dimensions: 4 };
        let existing = "## Decisions\n\nWe chose Rust for memory safety.";

        // Index and embed existing content.
        let file_path = tmp.path().join("existing.md");
        std::fs::write(&file_path, existing).unwrap();
        index.reindex_file(&file_path, "session").unwrap();
        let emb = provider.embed_batch(&[existing]).await.unwrap();
        let chunk_id = format!("{}:0", file_path.to_string_lossy());
        index.upsert_embedding(&chunk_id, &emb[0]).unwrap();

        // Different content should not be flagged as duplicate.
        let novel = "## Architecture\n\nThe API uses Python FastAPI with async handlers.";
        let result = is_semantically_duplicate(
            novel,
            &index,
            Some(&provider),
            SEMANTIC_DEDUP_SIMILARITY_THRESHOLD,
        )
        .await;
        assert!(
            !result,
            "different content should not be flagged as duplicate"
        );
    }
}
