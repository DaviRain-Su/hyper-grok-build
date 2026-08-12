//! Index-access plumbing for the session search SQLite cache.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::search_fts::{self, SessionSearchIndex};
use super::search_recovery;

pub(super) fn search_db_path(root_dir: &Path) -> PathBuf {
    let sessions = root_dir.join("sessions");
    // Best-effort: the journal-mode classifier statfs's the parent dir.
    // Owner-only root: the index duplicates session text into the db file.
    let _ = crate::util::grok_home::create_dir_all_owner_only(&sessions);
    let path = sessions.join("session_search.sqlite");
    // Pre-resolve the per-host sibling so raw file ops target the same file
    // the index opens.
    xai_sqlite_journal::JournalMode::for_db_path(&path).effective_db_path(&path)
}

pub(super) fn sqlite_to_io_error(error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("sqlite error: {error}"))
}

/// Rate-limits a repetitive log site: the first `cap` events go to `warn`,
/// the rest to `debug`; the budget resets when the search cache is healed.
pub(super) struct HealAwareLogCounter {
    count: AtomicU64,
    epoch_seen: AtomicU64,
    cap: u64,
}

impl HealAwareLogCounter {
    pub(super) const fn new(cap: u64) -> Self {
        Self {
            count: AtomicU64::new(0),
            epoch_seen: AtomicU64::new(0),
            cap,
        }
    }

    /// Rate-limited warn: within the budget, a tracing warn plus an entry
    /// in the always-on `unified.jsonl`; past it, tracing debug only.
    pub(super) fn warn(
        &self,
        kind: &str,
        message: &str,
        session_id: Option<&str>,
        error: Option<&io::Error>,
    ) {
        let error_text = error.map(|e| e.to_string());
        if self.should_warn(kind) {
            tracing::warn!(error = error_text.as_deref(), session_id, "{message}");
            xai_grok_telemetry::unified_log::emit(
                xai_grok_telemetry::unified_log::LogLevel::Warn,
                message,
                session_id,
                error_text.map(|e| serde_json::json!({ "error": e })),
            );
        } else {
            tracing::debug!(error = error_text.as_deref(), session_id, "{message}");
        }
    }

    fn should_warn(&self, kind: &str) -> bool {
        let epoch = search_recovery::current_epoch();
        if self.epoch_seen.swap(epoch, Ordering::Relaxed) != epoch {
            self.count.store(0, Ordering::Relaxed);
        }
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        if n < self.cap {
            return true;
        }
        if n == self.cap {
            tracing::warn!(cap = self.cap, "further {kind} will be logged at debug");
        }
        false
    }
}

static INDEX_FAIL_LOG: HealAwareLogCounter = HealAwareLogCounter::new(8);
static BOOTSTRAP_TIMEOUT_LOG: HealAwareLogCounter = HealAwareLogCounter::new(8);

pub(super) fn log_session_index_failure(session_id: &str, error: &io::Error, message: &str) {
    INDEX_FAIL_LOG.warn("index failures", message, Some(session_id), Some(error));
}

pub(super) fn log_bootstrap_timeout(session_id: &str, timeout_secs: u64) {
    let message = format!("session indexing timed out during bootstrap after {timeout_secs}s");
    BOOTSTRAP_TIMEOUT_LOG.warn("bootstrap timeouts", &message, Some(session_id), None);
}

/// Open the index (self-heals unusable files) and run `op`.
pub(super) fn with_search_index<R>(
    db_path: &Path,
    op: impl Fn(&SessionSearchIndex) -> Result<R, rusqlite::Error>,
) -> io::Result<R> {
    search_fts::with_index(db_path, op).map_err(sqlite_to_io_error)
}

/// Run `op` on a blocking thread via [`with_search_index`].
pub(super) async fn with_search_index_blocking<R: Send + 'static>(
    db_path: &Path,
    op: impl Fn(&SessionSearchIndex) -> Result<R, rusqlite::Error> + Send + 'static,
) -> io::Result<R> {
    let path = db_path.to_path_buf();
    tokio::task::spawn_blocking(move || with_search_index(&path, op))
        .await
        .map_err(io::Error::other)?
}
