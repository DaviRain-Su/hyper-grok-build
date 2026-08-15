//! The seam between the index and whatever owns the sessions on disk.
//!
//! The index reads four fields per session plus the path of its transcript,
//! and it reads that transcript through a caller-supplied extractor, so the
//! `updates.jsonl` wire format stays owned by the session store rather than
//! being duplicated here.

use std::io;
use std::path::{Path, PathBuf};

/// The projection of a stored session that the index actually indexes.
#[derive(Debug, Clone)]
pub struct IndexableSession {
    pub session_id: String,
    pub cwd: String,
    /// Last-modified stamp, unix seconds; the recency sort key.
    pub updated_at_unix: i64,
    /// Display title, already resolved by the store (generated title first,
    /// falling back to the session summary).
    pub title: String,
    /// Transcript to extract searchable text from, or `None` when the store
    /// does not expose one (such a session is indexed title-only).
    pub updates_path: Option<PathBuf>,
}

/// Read-only enumeration of the local session store.
#[async_trait::async_trait]
pub trait SessionSource: Send + Sync {
    /// Every session under this store, in no particular order.
    async fn list_sessions(&self) -> io::Result<Vec<IndexableSession>>;

    /// One session by identity. `Ok(None)` means the session is gone and its
    /// index row should be dropped; an `Err` is a transient read failure and
    /// leaves the row alone.
    async fn load_session(
        &self,
        session_id: &str,
        cwd: &str,
    ) -> io::Result<Option<IndexableSession>>;
}

/// Opens the session store rooted at one grok home.
pub type SessionSourceFactory = fn(PathBuf) -> Box<dyn SessionSource>;

/// Blocking extraction of a transcript's searchable text plus the bytes read.
/// Always called from a blocking thread.
pub type ContentExtractor = fn(&Path) -> io::Result<(String, u64)>;

/// Asked before this crate creates the database, indexes into it, or reads it. Deletes ignore it.
/// Asked at each decision rather than read once, because a host can turn the index off long
/// after the manager has started.
pub type IndexEnabled = fn() -> bool;

#[cfg(test)]
pub(crate) fn index_on() -> bool {
    true
}

#[cfg(test)]
pub(crate) fn index_off() -> bool {
    false
}
