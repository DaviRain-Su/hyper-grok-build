//! Session search orchestration: querying and background indexing.
//!
//! Mirrors the memory system's architecture:
//! - `execute_search()` runs queries via `search_fts::SessionSearchIndex`
//! - `SearchIndexManager` indexes sessions in the background (debounced)
//! - `notify_session_updated()` is the public hook for session save paths
//!
//! The index is bootstrapped (all sessions indexed) on first search.
//! After that, individual sessions are re-indexed on save/title update
//! via `notify_session_updated()`. Because the SQLite DB is shared with
//! other concurrently running grok processes (which may wipe or downgrade
//! it — older binaries drop-and-restamp the schema on open), every
//! subsequent search re-verifies the on-disk completed-bootstrap marker
//! and re-runs the full bootstrap when it is missing.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;

use super::search_fts::{SessionDoc, SessionSearchIndex, SessionSearchRow};
use super::search_remote_sync;
use super::{
    ContentPeek, PromptExtractEvent, RawLinePeek, RawParamsPeek, StorageAdapter,
    XAI_SESSION_UPDATE_METHOD, collect_prompts_from_events,
};
use crate::session::info::Info;
use crate::session::persistence::Summary;
use crate::session::wire_tags::{REWIND_MARKER, USER_MESSAGE_CHUNK};
use agent_client_protocol as acp;

const SEARCH_INDEX_DEBOUNCE_MS: u64 = 500;
const SEARCH_CONTENT_CHAR_LIMIT: usize = 200_000;
const BOOTSTRAP_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const BOOTSTRAP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Configuration for bootstrap resource limits.
///
/// Phase 1-3 use hardcoded defaults via `BootstrapConfig::default()`.
/// User-configurable overrides via `~/.grok/settings.json` are deferred
/// to a follow-up.
struct BootstrapConfig {
    /// Max concurrent sessions being indexed (default: 4).
    /// Used by the parallel bootstrap pipeline.
    max_concurrent: usize,
    /// Per-session timeout (default: 30 seconds).
    /// Wraps the `spawn_blocking` await — if the timeout fires, the pipeline
    /// moves on but the blocking task continues to completion.
    per_session_timeout: Duration,
    /// Max `updates.jsonl` size to index during bootstrap (default: 30 MB).
    /// Sessions exceeding this are skipped and indexed incrementally later.
    max_file_size: u64,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            per_session_timeout: Duration::from_secs(30),
            max_file_size: 30 * 1024 * 1024, // 30 MB
        }
    }
}

/// Pre-check: skip sessions with excessively large updates files.
///
/// Returns `true` if the file at `updates_path` exceeds `max_size` bytes.
/// Returns `false` if the file doesn't exist or can't be stat'd — let the
/// indexer handle those cases.
fn should_skip_session(updates_path: &Path, max_size: u64) -> bool {
    match std::fs::metadata(updates_path) {
        Ok(meta) => meta.len() > max_size,
        Err(_) => false,
    }
}

/// Internal search request (deserialized from the ACP extension params).
#[derive(Debug, Clone)]
pub struct SessionSearchRequest {
    pub query: String,
    pub cwd: Option<String>,
    pub limit: usize,
    pub offset: usize,
    pub include_content: bool,
}

/// Raw search response returned to the ACP extension handler.
#[derive(Debug, Clone)]
pub struct SessionSearchResponse {
    pub results: Vec<SessionSearchRow>,
    pub next_offset: Option<usize>,
    pub total_estimate: Option<usize>,
    /// True when the FTS5 index is still being bootstrapped. Callers
    /// should re-query after a delay to get results from newly indexed
    /// sessions.
    pub bootstrapping: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionSearchKey {
    session_id: String,
    cwd: String,
}

enum SearchIndexJob {
    Upsert(SessionSearchKey),
    BootstrapAll,
    /// Dispatched for every `BootstrapOnce` after the first: re-verify the
    /// on-disk completed-bootstrap marker, then either clear the eager
    /// `bootstrapping` flag (index intact) or re-run the full bootstrap
    /// (see [`has_completed_bootstrap_marker`]).
    RecheckBootstrap,
}

enum SearchManagerCmd {
    Enqueue { root: PathBuf, job: SearchIndexJob },
    BootstrapOnce { root: PathBuf },
}

struct SearchManagerState {
    workers: HashMap<PathBuf, mpsc::UnboundedSender<SearchIndexJob>>,
    bootstrapped: HashSet<PathBuf>,
    #[expect(dead_code, reason = "carried for future use by worker tasks")]
    progress: Arc<BootstrapProgress>,
}

/// Singleton that manages background session indexing.
///
/// Requires an active tokio runtime on first access (spawns tasks).
///
/// TODO: When multiple grok processes run concurrently, they each have
/// their own `SearchIndexManager` writing to the same SQLite database.
/// WAL mode prevents corruption, but redundant work is done. Consider
/// adding reindex claim coordination (like the memory system's
/// `try_claim_reindex()` / `release_claim()` pattern) if this becomes
/// a problem.
pub struct SearchIndexManager {
    tx: mpsc::UnboundedSender<SearchManagerCmd>,
    progress: Arc<BootstrapProgress>,
}

/// Global singleton — lazily started on first use.
pub static SEARCH_INDEX_MANAGER: LazyLock<SearchIndexManager> =
    LazyLock::new(SearchIndexManager::start);

#[derive(Default)]
pub struct BootstrapProgress {
    pub bootstrapping: AtomicBool,
    pub indexed: AtomicU64,
    pub total: AtomicU64,
    /// Sessions skipped due to size limit or timeout.
    pub skipped: AtomicU64,
    /// Sessions skipped because content hash was unchanged.
    pub unchanged: AtomicU64,
    /// Total bytes of `updates.jsonl` read during this bootstrap.
    pub bytes_read: AtomicU64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchIndexStatus {
    pub bootstrapping: bool,
    pub indexed: u64,
    pub total: u64,
    /// Sessions skipped due to size limit or timeout.
    pub skipped: u64,
    /// Sessions skipped because content hash was unchanged.
    pub unchanged: u64,
}

impl SearchIndexManager {
    fn start() -> Self {
        let progress = Arc::new(BootstrapProgress::default());
        let progress_clone = progress.clone();
        let (tx, mut rx) = mpsc::unbounded_channel::<SearchManagerCmd>();

        tokio::spawn(async move {
            let mut state = SearchManagerState {
                workers: HashMap::new(),
                bootstrapped: HashSet::new(),
                progress: progress_clone,
            };
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    SearchManagerCmd::Enqueue { root, job } => {
                        Self::dispatch(&mut state, root, job);
                    }
                    SearchManagerCmd::BootstrapOnce { root } => {
                        if state.bootstrapped.insert(root.clone()) {
                            Self::dispatch(&mut state, root, SearchIndexJob::BootstrapAll);
                        } else {
                            // Already bootstrapped this process — but the DB
                            // is shared, so don't trust the in-memory flag:
                            // re-verify the on-disk marker (the job also
                            // undoes the eager flag set).
                            // Dispatch through the worker channel so it sequences
                            // after any in-flight BootstrapAll.
                            Self::dispatch(&mut state, root, SearchIndexJob::RecheckBootstrap);
                        }
                    }
                }
            }
        });

        Self { tx, progress }
    }

    /// Queue a bootstrap of all sessions. Idempotent per root_dir, except
    /// that repeat calls re-verify the on-disk completed-bootstrap marker
    /// and re-bootstrap when it is missing (see
    /// [`SearchIndexJob::RecheckBootstrap`]).
    ///
    /// Sets `bootstrapping` eagerly so callers polling the flag see `true`
    /// before the background task even starts processing.
    pub fn bootstrap_once(&self, root: PathBuf) {
        self.progress.bootstrapping.store(true, Ordering::Release);
        let _ = self.tx.send(SearchManagerCmd::BootstrapOnce { root });
    }

    /// Get current bootstrap progress status.
    pub fn status(&self) -> SearchIndexStatus {
        SearchIndexStatus {
            bootstrapping: self.progress.bootstrapping.load(Ordering::Relaxed),
            indexed: self.progress.indexed.load(Ordering::Relaxed),
            total: self.progress.total.load(Ordering::Relaxed),
            skipped: self.progress.skipped.load(Ordering::Relaxed),
            unchanged: self.progress.unchanged.load(Ordering::Relaxed),
        }
    }

    /// Queue an index update for a single session.
    pub fn enqueue(&self, root: PathBuf, session_id: String, cwd: String) {
        let key = SessionSearchKey { session_id, cwd };
        let _ = self.tx.send(SearchManagerCmd::Enqueue {
            root,
            job: SearchIndexJob::Upsert(key),
        });
    }

    fn dispatch(state: &mut SearchManagerState, root: PathBuf, job: SearchIndexJob) {
        let sender = state.workers.entry(root.clone()).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let root_owned = root.clone();
            tokio::spawn(async move {
                let storage: Box<dyn StorageAdapter> = Box::new(
                    super::jsonl::JsonlStorageAdapter::with_root(root_owned.clone()),
                );
                run_worker(&root_owned, storage.as_ref(), rx).await;
            });
            tx
        });
        if sender.send(job).is_err() {
            tracing::warn!("search worker channel closed");
        }
    }
}

/// Trigger indexing for a session that was just saved or updated.
///
/// This is the public hook to call from session persistence paths
/// (e.g., after `update_session_title`, after each prompt turn).
pub fn notify_session_updated(session_id: &str, cwd: &str) {
    let root = crate::util::grok_home::grok_home();
    SEARCH_INDEX_MANAGER.enqueue(root, session_id.to_string(), cwd.to_string());
}

fn search_db_path(root_dir: &Path) -> PathBuf {
    let sessions = root_dir.join("sessions");
    // Best-effort: the journal-mode classifier statfs's the parent dir.
    let _ = std::fs::create_dir_all(&sessions);
    let path = sessions.join("session_search.sqlite");
    // Pre-resolve the per-host sibling (network mounts) so search_remote_sync's
    // raw file ops (exists/compress/replace) target the same file the index
    // opens; resolution is idempotent, so the open re-resolving is a no-op.
    xai_sqlite_journal::JournalMode::for_db_path(&path).effective_db_path(&path)
}

fn sqlite_to_io_error(error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("sqlite error: {error}"))
}

/// Execute a session search query.
///
/// On first call, triggers a background bootstrap that indexes all
/// existing sessions. Waits up to [`BOOTSTRAP_WAIT_TIMEOUT`] for the
/// bootstrap to complete so the query runs against a populated index.
/// Subsequent calls skip the wait (bootstrap is already done).
pub async fn execute_search(
    root_dir: &Path,
    req: &SessionSearchRequest,
) -> io::Result<SessionSearchResponse> {
    let query = req.query.trim();
    if query.is_empty() {
        return Ok(SessionSearchResponse {
            results: Vec::new(),
            next_offset: None,
            total_estimate: Some(0),
            bootstrapping: false,
        });
    }

    SEARCH_INDEX_MANAGER.bootstrap_once(root_dir.to_path_buf());

    let deadline = tokio::time::Instant::now() + BOOTSTRAP_WAIT_TIMEOUT;
    while SEARCH_INDEX_MANAGER
        .progress
        .bootstrapping
        .load(Ordering::Acquire)
    {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(BOOTSTRAP_POLL_INTERVAL).await;
    }
    let db_path = search_db_path(root_dir);
    let cwd = req.cwd.clone();
    let limit = req.limit;
    let offset = req.offset;
    let include_content = req.include_content;
    let query_owned = query.to_string();

    let qr = tokio::task::spawn_blocking(move || {
        let index = SessionSearchIndex::open_or_create(&db_path).map_err(sqlite_to_io_error)?;
        index
            .query(&query_owned, cwd.as_deref(), limit, offset, include_content)
            .map_err(sqlite_to_io_error)
    })
    .await
    .map_err(io::Error::other)??;

    Ok(SessionSearchResponse {
        results: qr.results,
        next_offset: qr.next_offset,
        total_estimate: qr.total_estimate,
        bootstrapping: SEARCH_INDEX_MANAGER
            .progress
            .bootstrapping
            .load(Ordering::Relaxed),
    })
}

async fn run_worker(
    root_dir: &Path,
    storage: &dyn StorageAdapter,
    mut rx: mpsc::UnboundedReceiver<SearchIndexJob>,
) {
    let debounce = std::time::Duration::from_millis(SEARCH_INDEX_DEBOUNCE_MS);
    let mut pending: HashMap<SessionSearchKey, Instant> = HashMap::new();

    loop {
        if pending.is_empty() {
            let Some(job) = rx.recv().await else { break };
            handle_job(root_dir, storage, &mut pending, job, debounce).await;
            continue;
        }

        let next_deadline = pending
            .values()
            .copied()
            .min()
            .unwrap_or_else(|| Instant::now() + debounce);

        tokio::select! {
            maybe_job = rx.recv() => {
                let Some(job) = maybe_job else { break };
                handle_job(root_dir, storage, &mut pending, job, debounce).await;
            }
            _ = tokio::time::sleep_until(next_deadline) => {
                flush_ready(root_dir, storage, &mut pending).await;
            }
        }
    }
}

fn clear_bootstrapping_flag() {
    SEARCH_INDEX_MANAGER
        .progress
        .bootstrapping
        .store(false, Ordering::Release);
}

async fn handle_job(
    root_dir: &Path,
    storage: &dyn StorageAdapter,
    pending: &mut HashMap<SessionSearchKey, Instant>,
    job: SearchIndexJob,
    debounce: std::time::Duration,
) {
    match job {
        SearchIndexJob::Upsert(key) => {
            pending.insert(key, Instant::now() + debounce);
        }
        SearchIndexJob::BootstrapAll => {
            if let Err(e) = reindex_all(root_dir, storage).await {
                tracing::warn!(error = %e, "session search bootstrap failed");
                clear_bootstrapping_flag();
            }
        }
        SearchIndexJob::RecheckBootstrap => match has_completed_bootstrap_marker(root_dir).await {
            Some(true) => clear_bootstrapping_flag(),
            Some(false) => {
                // Marker genuinely absent (index wiped/downgraded/bootstrap
                // never completed — see `has_completed_bootstrap_marker`):
                // without a re-run this process would keep searching an
                // empty index for its whole lifetime.
                tracing::info!(
                    "session search index missing completed-bootstrap marker; re-running bootstrap"
                );
                if let Err(e) = reindex_all(root_dir, storage).await {
                    tracing::warn!(error = %e, "session search re-bootstrap failed");
                    clear_bootstrapping_flag();
                }
            }
            None => {
                // Transient read failure (busy/locked DB, I/O): rebuilding on
                // every such search would be a reindex storm. Skip; the next
                // search retries the probe.
                tracing::debug!(
                    "session search bootstrap marker unreadable; skipping re-bootstrap"
                );
                clear_bootstrapping_flag();
            }
        },
    }
}

/// Tri-state probe for the completed-bootstrap marker (`last_bootstrap_at`
/// in the `meta` table, written at the end of [`reindex_all`]):
/// `Some(true)` marker present, `Some(false)` genuinely absent (bootstrap
/// needed), `None` transient read failure (busy/locked DB — must not be
/// mistaken for absence, or every search under contention would trigger a
/// full rebuild).
///
/// Opening the DB here is itself the healing step for a downgraded index:
/// `open_or_create` performs the upgrade drop, which deletes the marker in
/// the same transaction (see [`SessionSearchIndex::open_or_create`]), so
/// this returns `Some(false)` and the caller re-runs the full bootstrap.
async fn has_completed_bootstrap_marker(root_dir: &Path) -> Option<bool> {
    let db_path = search_db_path(root_dir);
    tokio::task::spawn_blocking(move || {
        search_remote_sync::try_read_last_bootstrap_at(&db_path)
            .map(|marker| marker.is_some())
            .ok()
    })
    .await
    .ok()
    .flatten()
}

async fn flush_ready(
    root_dir: &Path,
    storage: &dyn StorageAdapter,
    pending: &mut HashMap<SessionSearchKey, Instant>,
) {
    let now = Instant::now();
    let ready: Vec<SessionSearchKey> = pending
        .iter()
        .filter_map(|(key, deadline)| (*deadline <= now).then_some(key.clone()))
        .collect();

    for key in ready {
        pending.remove(&key);
        if let Err(e) = upsert_by_key(root_dir, storage, &key).await {
            tracing::warn!(
                error = %e,
                session_id = %key.session_id,
                "failed upserting session in search index"
            );
        }
    }
}

/// Outcome of a single session upsert.
#[derive(Debug)]
enum UpsertOutcome {
    /// Content was indexed (new or changed).
    Indexed { bytes_read: u64 },
    /// Content hash matched existing index entry — no update needed.
    Unchanged { bytes_read: u64 },
    /// No updates file available (storage backend doesn't expose paths).
    NoContent,
}

async fn upsert_by_key(
    root_dir: &Path,
    storage: &dyn StorageAdapter,
    key: &SessionSearchKey,
) -> io::Result<()> {
    let info = Info {
        id: acp::SessionId::new(key.session_id.clone()),
        cwd: key.cwd.clone(),
    };

    match storage.load_summary(&info).await {
        Ok(summary) => upsert_session(root_dir, &summary, storage, &info)
            .await
            .map(|_| ()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            delete_session(root_dir, &key.session_id).await
        }
        Err(e) => Err(e),
    }
}

async fn upsert_session(
    root_dir: &Path,
    summary: &Summary,
    storage: &dyn StorageAdapter,
    info: &Info,
) -> io::Result<UpsertOutcome> {
    // Single-pass direct file I/O: bypass StorageAdapter and open updates.jsonl
    // once, extracting prompts, assistant text, and tool metadata in one pass.
    // Reduces I/O by 3x vs the old 3-call pattern.
    let (content, bytes_read) = if let Some(updates_path) = storage.updates_file_path(info) {
        tokio::task::spawn_blocking(move || {
            collect_all_indexable_content_single_pass(&updates_path)
        })
        .await
        .map_err(io::Error::other)??
    } else {
        // Storage backend doesn't expose file paths — no content to index
        return Ok(UpsertOutcome::NoContent);
    };
    let doc = build_session_doc(summary, content, bytes_read);
    let db_path = search_db_path(root_dir);

    tokio::task::spawn_blocking(move || {
        let index = SessionSearchIndex::open_or_create(&db_path).map_err(sqlite_to_io_error)?;

        // Skip if content hasn't changed
        if let Ok(Some(existing_hash)) = index.get_content_hash(&doc.session_id)
            && existing_hash == doc.content_hash
        {
            return Ok(UpsertOutcome::Unchanged { bytes_read });
        }

        index.upsert_doc(&doc).map_err(sqlite_to_io_error)?;
        Ok(UpsertOutcome::Indexed { bytes_read })
    })
    .await
    .map_err(io::Error::other)?
}

async fn delete_session(root_dir: &Path, session_id: &str) -> io::Result<()> {
    let db_path = search_db_path(root_dir);
    let session_id = session_id.to_string();
    tokio::task::spawn_blocking(move || {
        let index = SessionSearchIndex::open_or_create(&db_path).map_err(sqlite_to_io_error)?;
        index.delete_doc(&session_id).map_err(sqlite_to_io_error)
    })
    .await
    .map_err(io::Error::other)?
}

async fn reindex_all(root_dir: &Path, storage: &dyn StorageAdapter) -> io::Result<()> {
    let config = BootstrapConfig::default();
    let progress = &SEARCH_INDEX_MANAGER.progress;

    // Reset progress counters (bootstrapping flag already set by bootstrap_once)
    progress.indexed.store(0, Ordering::Relaxed);
    progress.skipped.store(0, Ordering::Relaxed);
    progress.unchanged.store(0, Ordering::Relaxed);
    progress.bytes_read.store(0, Ordering::Relaxed);

    let start = Instant::now();
    let summaries = storage.list_sessions(None).await?;
    progress
        .total
        .store(summaries.len() as u64, Ordering::Relaxed);
    let expected_ids: HashSet<String> = summaries.iter().map(|s| s.info.id.to_string()).collect();

    // Pre-compute updates file paths for all sessions (cheap path
    // computation — no I/O). This decouples the parallel pipeline from
    // the StorageAdapter reference which cannot be shared across tasks.
    let sessions: Vec<(Summary, Option<PathBuf>)> = summaries
        .into_iter()
        .map(|s| {
            let path = storage.updates_file_path(&s.info);
            (s, path)
        })
        .collect();

    // Pre-scan: count sessions that will be skipped due to size cap
    let mut skipped_large = 0u64;
    for (_, path) in &sessions {
        if let Some(updates_path) = path
            && should_skip_session(updates_path, config.max_file_size)
        {
            skipped_large += 1;
        }
    }

    tracing::info!(
        total_sessions = sessions.len(),
        skipped_large = skipped_large,
        "session search bootstrap starting"
    );

    // Semaphore-bounded parallel indexing: spawn a task per session,
    // each acquiring a permit before doing the heavy I/O work.
    // max_concurrent (default 4) limits disk I/O contention and keeps
    // the tokio blocking thread pool available for other work.
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
    let progress_arc = SEARCH_INDEX_MANAGER.progress.clone();
    let root_owned = root_dir.to_path_buf();

    let mut join_set = tokio::task::JoinSet::new();

    for (summary, updates_path) in sessions {
        let sem = semaphore.clone();
        let progress = progress_arc.clone();
        let root = root_owned.clone();
        let timeout_dur = config.per_session_timeout;
        let max_file_size = config.max_file_size;

        join_set.spawn(async move {
            // Acquire semaphore permit — this provides backpressure,
            // limiting concurrency to max_concurrent (default 4).
            // Safety: the semaphore is never closed — it lives in an Arc
            // shared only by tasks spawned in this loop, all of which
            // complete before the Arc is dropped.
            let _permit = sem.acquire().await.expect("semaphore is never closed");

            let session_id = summary.info.id.to_string();

            // File size pre-check: skip sessions with oversized updates.jsonl
            if let Some(ref path) = updates_path
                && should_skip_session(path, max_file_size)
            {
                let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                tracing::warn!(
                    session_id = %session_id,
                    file_size = file_size,
                    max_size = max_file_size,
                    "skipping large session during bootstrap"
                );
                // Insert a title-only placeholder so title search still works;
                // insert-if-absent so an existing (fuller) row is never touched.
                let doc = build_session_doc(&summary, String::new(), 0);
                let db_path = search_db_path(&root);
                let title_only = tokio::task::spawn_blocking(move || {
                    SessionSearchIndex::open_or_create(&db_path)
                        .and_then(|index| index.insert_doc_if_absent(&doc))
                        .map_err(sqlite_to_io_error)
                })
                .await;
                if let Err(e) = title_only.map_err(io::Error::other).and_then(|r| r) {
                    tracing::warn!(
                        error = %e,
                        session_id = %session_id,
                        "failed to write title-only index row for large session"
                    );
                }
                progress.skipped.fetch_add(1, Ordering::Relaxed);
                return;
            }

            // Wrap with per-session timeout to prevent pipeline stalls.
            // The inner block is `async move` to own summary, updates_path,
            // and root — the outer block retains session_id and progress
            // for post-timeout error reporting.
            match tokio::time::timeout(timeout_dur, async move {
                // Collect content via spawn_blocking (single-pass I/O)
                let (content, bytes_read) = if let Some(path) = updates_path {
                    match tokio::task::spawn_blocking(move || {
                        collect_all_indexable_content_single_pass(&path)
                    })
                    .await
                    {
                        Ok(Ok(result)) => result,
                        Ok(Err(e)) => return Err(e),
                        Err(e) => return Err(io::Error::other(e)),
                    }
                } else {
                    return Ok(UpsertOutcome::NoContent);
                };

                let doc = build_session_doc(&summary, content, bytes_read);
                let db_path = search_db_path(&root);

                // Each task opens its own SessionSearchIndex connection.
                // SQLite WAL mode handles concurrent readers + serialized writers.
                match tokio::task::spawn_blocking(move || {
                    let index =
                        SessionSearchIndex::open_or_create(&db_path).map_err(sqlite_to_io_error)?;
                    if let Ok(Some(existing_hash)) = index.get_content_hash(&doc.session_id)
                        && existing_hash == doc.content_hash
                    {
                        return Ok(UpsertOutcome::Unchanged { bytes_read });
                    }
                    index.upsert_doc(&doc).map_err(sqlite_to_io_error)?;
                    Ok(UpsertOutcome::Indexed { bytes_read })
                })
                .await
                {
                    Ok(result) => result,
                    Err(e) => Err(io::Error::other(e)),
                }
            })
            .await
            {
                Ok(Ok(outcome)) => match outcome {
                    UpsertOutcome::Indexed { bytes_read } => {
                        progress.bytes_read.fetch_add(bytes_read, Ordering::Relaxed);
                    }
                    UpsertOutcome::Unchanged { bytes_read } => {
                        progress.unchanged.fetch_add(1, Ordering::Relaxed);
                        progress.bytes_read.fetch_add(bytes_read, Ordering::Relaxed);
                    }
                    UpsertOutcome::NoContent => {}
                },
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        session_id = %session_id,
                        "failed to index session for search"
                    );
                    progress.skipped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    // Timeout expired — the spawn_blocking task continues to
                    // completion but the pipeline moves on to the next session.
                    tracing::warn!(
                        session_id = %session_id,
                        timeout_secs = timeout_dur.as_secs(),
                        "session indexing timed out during bootstrap"
                    );
                    progress.skipped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
            progress.indexed.fetch_add(1, Ordering::Relaxed);
        });
    }

    // Drain the JoinSet — wait for all tasks to complete
    while let Some(result) = join_set.join_next().await {
        if let Err(e) = result {
            tracing::warn!(error = %e, "session indexing task panicked");
        }
    }

    // Prune orphaned entries
    let db_path = search_db_path(root_dir);
    tokio::task::spawn_blocking(move || -> io::Result<()> {
        let index = SessionSearchIndex::open_or_create(&db_path).map_err(sqlite_to_io_error)?;
        let indexed_ids = index
            .all_indexed_session_ids()
            .map_err(sqlite_to_io_error)?;

        for id in indexed_ids {
            if !expected_ids.contains(&id) {
                let _ = index.delete_doc(&id);
            }
        }
        Ok(())
    })
    .await
    .map_err(io::Error::other)??;

    let elapsed = start.elapsed();
    tracing::info!(
        indexed = progress.indexed.load(Ordering::Relaxed),
        skipped = progress.skipped.load(Ordering::Relaxed),
        unchanged = progress.unchanged.load(Ordering::Relaxed),
        duration_ms = elapsed.as_millis() as u64,
        bytes_read = progress.bytes_read.load(Ordering::Relaxed),
        "session search bootstrap complete"
    );

    progress.bootstrapping.store(false, Ordering::Release);

    // Record bootstrap completion timestamp in the meta table.
    // Used by remote sync to determine local index staleness.
    let db_path_meta = search_db_path(root_dir);
    if let Err(e) = search_remote_sync::write_last_bootstrap_at(&db_path_meta) {
        tracing::warn!(error = %e, "failed to write last_bootstrap_at metadata");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Zero-copy peek structs for single-pass content collection
//
// Text-bearing fields are `Cow`, not `&str`: serde cannot borrow `&str` from
// JSON strings containing escapes (\n, \", \\, \uXXXX), so borrowing would
// error and silently drop the message from the index. Discriminant-only
// fields (never escaped) stay `&'a str` for the zero-copy fast path.
// ---------------------------------------------------------------------------

/// Selective peek for assistant text extraction (agent_message_chunk content.text).
#[derive(serde::Deserialize)]
struct AgentContentPeek<'a> {
    #[serde(borrow)]
    update: AgentUpdatePeek<'a>,
}

#[derive(serde::Deserialize)]
struct AgentUpdatePeek<'a> {
    #[serde(borrow, default)]
    content: Option<AgentTextPeek<'a>>,
}

#[derive(serde::Deserialize)]
struct AgentTextPeek<'a> {
    #[serde(rename = "type", default)]
    content_type: Option<&'a str>,
    #[serde(borrow, default)]
    text: Option<std::borrow::Cow<'a, str>>,
}

/// Selective peek for user message content extraction (user_message_chunk content.text).
/// Reuses [`ContentPeek`] from `parse_prompt_extract_event` (one source of
/// truth for the peeked fields and their escape-tolerance) but operates on
/// pre-parsed `raw_params` to avoid re-parsing the envelope.
#[derive(serde::Deserialize)]
struct UserContentPeek<'a> {
    #[serde(borrow)]
    update: UserUpdatePeek<'a>,
}

#[derive(serde::Deserialize)]
struct UserUpdatePeek<'a> {
    #[serde(borrow, default)]
    content: Option<ContentPeek<'a>>,
    #[serde(default, rename = "_meta")]
    meta: Option<super::RawChunkMetaPeek>,
}

/// Selective peek for tool call extraction (tool_call title + locations[].path).
#[derive(serde::Deserialize)]
struct ToolCallPeek<'a> {
    #[serde(borrow)]
    update: ToolUpdatePeek<'a>,
}

#[derive(serde::Deserialize)]
struct ToolUpdatePeek<'a> {
    #[serde(borrow, default)]
    title: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default)]
    locations: Option<Vec<ToolLocationPeek<'a>>>,
}

#[derive(serde::Deserialize)]
struct ToolLocationPeek<'a> {
    #[serde(borrow, default)]
    path: Option<std::borrow::Cow<'a, str>>,
}

/// Collect all indexable content from a session in a single pass.
///
/// Opens `updates.jsonl` once with `BufReader`, classifies each line via
/// zero-copy `RawLinePeek` discriminant, and extracts only the fields
/// needed for search indexing. Never materializes full
/// `acp::SessionNotification` objects.
///
/// Replaces the old 3-call pattern (`load_prompts_only` + `load_assistant_text`
/// + `load_tool_metadata`), reducing I/O by 3x and deserialization cost significantly.
fn collect_all_indexable_content_single_pass(updates_path: &Path) -> io::Result<(String, u64)> {
    let file = match std::fs::File::open(updates_path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((String::new(), 0)),
        Err(e) => return Err(e),
    };
    let bytes_read = file.metadata().map(|m| m.len()).unwrap_or(0);
    let reader = io::BufReader::new(file);

    let mut prompt_events: Vec<PromptExtractEvent> = Vec::new();
    let mut assistant_texts: Vec<String> = Vec::new();
    let mut current_assistant: String = String::new();
    let mut tool_meta: Vec<String> = Vec::new();
    let mut assistant_chars = 0usize;
    let mut tool_call_count = 0usize;
    let mut tool_chars_emitted = 0usize;

    const ASSISTANT_MAX_CHARS: usize = 100_000;
    const TOOL_MAX_CALLS: usize = 200;
    const TOOL_MAX_CHARS: usize = 100_000;

    // Helper: flush in-progress assistant text buffer on turn boundary.
    // Must be called in every non-agent_message_chunk branch, matching
    // the existing `collect_assistant_text` flush semantics (mod.rs:883).
    let flush_assistant = |current: &mut String, texts: &mut Vec<String>| {
        if !current.is_empty() {
            let t = current.trim().to_string();
            if !t.is_empty() {
                texts.push(t);
            }
            current.clear();
        }
    };

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "skipping unreadable line in single-pass content collector");
                // Treat I/O errors as a turn boundary: flush assistant
                // text and emit NotUserMessage so prompt accumulation
                // and rewind logic stay consistent with the iterator-based
                // collectors (PromptExtractIterator yields NotUserMessage on Err).
                flush_assistant(&mut current_assistant, &mut assistant_texts);
                prompt_events.push(PromptExtractEvent::NotUserMessage);
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Step 1: Peek at envelope to get method + raw params
        let (raw_params, is_xai) = if let Ok(env) = serde_json::from_str::<RawLinePeek<'_>>(trimmed)
        {
            let raw = env.params.map(|p| p.get()).unwrap_or(trimmed);
            let xai = env.method == Some(XAI_SESSION_UPDATE_METHOD);
            (raw, xai)
        } else {
            (trimmed, false)
        };

        // Step 2: Peek at the sessionUpdate discriminant tag.
        // Preserve the full RawUpdatePeek so rewind_marker can read
        // target_prompt_index without re-parsing the envelope.
        let update_peek = serde_json::from_str::<RawParamsPeek<'_>>(raw_params)
            .ok()
            .and_then(|p| p.update);
        let tag = update_peek.as_ref().map(|u| u.session_update);

        // Content events (user messages, assistant responses, tool calls,
        // thoughts) come from the standard ACP protocol ("session/update").
        // Control events (rewind markers) come from xAI extensions
        // ("_x.ai/session/update"). Dispatch on source first, then tag.
        if !is_xai {
            // ── ACP content events ──────────────────────────────────
            match tag {
                Some(t) if t == *USER_MESSAGE_CHUNK => {
                    flush_assistant(&mut current_assistant, &mut assistant_texts);
                    if let Ok(peek) = serde_json::from_str::<UserContentPeek<'_>>(raw_params)
                        && let Some(content) = peek.update.content
                        && content.content_type == Some("text")
                        && let Some(text) = content.text
                    {
                        if content
                            .meta
                            .as_ref()
                            .is_some_and(|m| m.bash_command.is_some())
                            || peek
                                .update
                                .meta
                                .as_ref()
                                .is_some_and(|m| m.host_turn == Some(true))
                        {
                            prompt_events.push(PromptExtractEvent::NotUserMessage);
                        } else {
                            let prompt_index = peek
                                .update
                                .meta
                                .as_ref()
                                .and_then(|m| m.prompt_index.map(|v| v as usize));
                            prompt_events.push(PromptExtractEvent::UserTextChunk {
                                text: text.into_owned(),
                                prompt_index,
                            });
                        }
                    } else {
                        prompt_events.push(PromptExtractEvent::NotUserMessage);
                    }
                }
                Some("agent_message_chunk") => {
                    // Same assistant turn — no flush
                    if assistant_chars < ASSISTANT_MAX_CHARS
                        && let Ok(peek) = serde_json::from_str::<AgentContentPeek<'_>>(raw_params)
                        && let Some(content) = peek.update.content
                        && content.content_type == Some("text")
                        && let Some(text) = content.text
                        && !text.is_empty()
                    {
                        let sep_cost = usize::from(!current_assistant.is_empty());
                        let budget = ASSISTANT_MAX_CHARS
                            .saturating_sub(assistant_chars)
                            .saturating_sub(sep_cost);
                        if budget > 0 {
                            if sep_cost > 0 {
                                current_assistant.push(' ');
                                assistant_chars += 1;
                            }
                            let mut take = text.len().min(budget);
                            while take > 0 && !text.is_char_boundary(take) {
                                take -= 1;
                            }
                            current_assistant.push_str(&text[..take]);
                            assistant_chars += take;
                        }
                    }
                    prompt_events.push(PromptExtractEvent::NotUserMessage);
                }
                Some("agent_thought_chunk") => {
                    // Thinking tokens are part of the same assistant turn —
                    // do NOT flush. Content is not indexed but we must avoid
                    // treating it as a turn boundary.
                    prompt_events.push(PromptExtractEvent::NotUserMessage);
                }
                Some("tool_call") => {
                    flush_assistant(&mut current_assistant, &mut assistant_texts);
                    if tool_call_count < TOOL_MAX_CALLS {
                        tool_call_count += 1;
                        if let Ok(peek) = serde_json::from_str::<ToolCallPeek<'_>>(raw_params) {
                            if let Some(title) = peek.update.title
                                && !title.is_empty()
                            {
                                let budget = TOOL_MAX_CHARS.saturating_sub(tool_chars_emitted);
                                if budget > 0 {
                                    let mut take = title.len().min(budget);
                                    while take > 0 && !title.is_char_boundary(take) {
                                        take -= 1;
                                    }
                                    tool_meta.push(title[..take].to_string());
                                    tool_chars_emitted += take;
                                }
                            }
                            if let Some(locs) = peek.update.locations {
                                for loc in locs {
                                    if let Some(p) = loc.path
                                        && !p.is_empty()
                                    {
                                        let budget =
                                            TOOL_MAX_CHARS.saturating_sub(tool_chars_emitted);
                                        if budget > 0 {
                                            let mut take = p.len().min(budget);
                                            while take > 0 && !p.is_char_boundary(take) {
                                                take -= 1;
                                            }
                                            tool_meta.push(p[..take].to_string());
                                            tool_chars_emitted += take;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    prompt_events.push(PromptExtractEvent::NotUserMessage);
                }
                _ => {
                    flush_assistant(&mut current_assistant, &mut assistant_texts);
                    prompt_events.push(PromptExtractEvent::NotUserMessage);
                }
            }
        } else {
            // ── xAI control events ──────────────────────────────────
            match tag {
                Some(t) if t == *REWIND_MARKER => {
                    flush_assistant(&mut current_assistant, &mut assistant_texts);
                    if let Some(ref u) = update_peek
                        && let Some(idx) = u.target_prompt_index
                    {
                        prompt_events.push(PromptExtractEvent::RewindTo(idx));
                    } else {
                        prompt_events.push(PromptExtractEvent::NotUserMessage);
                    }
                }
                _ => {
                    flush_assistant(&mut current_assistant, &mut assistant_texts);
                    prompt_events.push(PromptExtractEvent::NotUserMessage);
                }
            }
        }
    }

    // Flush final assistant turn
    if !current_assistant.is_empty() {
        let t = current_assistant.trim().to_string();
        if !t.is_empty() {
            assistant_texts.push(t);
        }
    }

    let prompts = collect_prompts_from_events(prompt_events.into_iter());

    let parts = [
        prompts.join("\n\n"),
        assistant_texts.join("\n"),
        tool_meta.join("\n"),
    ];
    let mut joined = parts.join("\n\n");

    if joined.len() > SEARCH_CONTENT_CHAR_LIMIT {
        // Keep the tail (most recent content is most relevant)
        let mut start = joined.len().saturating_sub(SEARCH_CONTENT_CHAR_LIMIT);
        while start < joined.len() && !joined.is_char_boundary(start) {
            start += 1;
        }
        joined = joined[start..].to_string();
    }

    Ok((joined, bytes_read))
}

/// Result of a delta content collection attempt.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Used once delta path is wired into upsert_session"
    )
)]
enum DeltaResult {
    /// New content extracted from the delta window.
    Content {
        /// Extracted indexable text from the new bytes.
        text: String,
        /// File size at open time — becomes the new `last_indexed_offset`.
        file_size: u64,
    },
    /// The delta window contains a `rewind_marker`, so the caller must fall
    /// back to a full re-read to rebuild prompt history correctly.
    NeedsFullReread,
}

/// Collect indexable content from the *new* portion of `updates.jsonl`,
/// starting at `offset` bytes.
///
/// Uses the same selective peek logic as [`collect_all_indexable_content_single_pass`]
/// but operates only on the delta window. If a `rewind_marker` is encountered,
/// returns [`DeltaResult::NeedsFullReread`] so the caller can fall back to a
/// full re-read (rewind affects prompt history, which requires the full file).
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Used once delta path is wired into upsert_session"
    )
)]
fn collect_delta_content(updates_path: &Path, offset: u64) -> io::Result<DeltaResult> {
    let mut file = std::fs::File::open(updates_path)?;
    let file_size = file.metadata()?.len();

    if file_size <= offset {
        return Ok(DeltaResult::Content {
            text: String::new(),
            file_size,
        });
    }

    file.seek(io::SeekFrom::Start(offset))?;
    let reader = io::BufReader::new(file);

    let mut user_texts: Vec<String> = Vec::new();
    let mut assistant_texts: Vec<String> = Vec::new();
    let mut current_assistant = String::new();
    let mut tool_meta: Vec<String> = Vec::new();

    let flush_assistant = |current: &mut String, texts: &mut Vec<String>| {
        if !current.is_empty() {
            let t = current.trim().to_string();
            if !t.is_empty() {
                texts.push(t);
            }
            current.clear();
        }
    };

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let (raw_params, is_xai) = if let Ok(env) = serde_json::from_str::<RawLinePeek<'_>>(trimmed)
        {
            let raw = env.params.map(|p| p.get()).unwrap_or(trimmed);
            let xai = env.method == Some(XAI_SESSION_UPDATE_METHOD);
            (raw, xai)
        } else {
            (trimmed, false)
        };

        let tag = serde_json::from_str::<RawParamsPeek<'_>>(raw_params)
            .ok()
            .and_then(|p| p.update)
            .map(|u| u.session_update);

        match tag {
            Some(t) if is_xai && t == *REWIND_MARKER => {
                return Ok(DeltaResult::NeedsFullReread);
            }
            Some(t) if !is_xai && t == *USER_MESSAGE_CHUNK => {
                flush_assistant(&mut current_assistant, &mut assistant_texts);
                if let Ok(peek) = serde_json::from_str::<UserContentPeek<'_>>(raw_params)
                    && let Some(content) = peek.update.content
                    && content.content_type == Some("text")
                    && let Some(text) = content.text
                    && content
                        .meta
                        .as_ref()
                        .is_none_or(|m| m.bash_command.is_none())
                    && peek
                        .update
                        .meta
                        .as_ref()
                        .is_none_or(|m| m.host_turn != Some(true))
                {
                    user_texts.push(text.into_owned());
                }
            }
            Some("agent_message_chunk") if !is_xai => {
                if let Ok(peek) = serde_json::from_str::<AgentContentPeek<'_>>(raw_params)
                    && let Some(content) = peek.update.content
                    && content.content_type == Some("text")
                    && let Some(text) = content.text
                    && !text.is_empty()
                {
                    if !current_assistant.is_empty() {
                        current_assistant.push(' ');
                    }
                    current_assistant.push_str(&text);
                }
            }
            Some("tool_call") if !is_xai => {
                flush_assistant(&mut current_assistant, &mut assistant_texts);
                if let Ok(peek) = serde_json::from_str::<ToolCallPeek<'_>>(raw_params) {
                    if let Some(title) = peek.update.title
                        && !title.is_empty()
                    {
                        tool_meta.push(title.into_owned());
                    }
                    if let Some(locs) = peek.update.locations {
                        for loc in locs {
                            if let Some(p) = loc.path
                                && !p.is_empty()
                            {
                                tool_meta.push(p.into_owned());
                            }
                        }
                    }
                }
            }
            _ => {
                flush_assistant(&mut current_assistant, &mut assistant_texts);
            }
        }
    }

    flush_assistant(&mut current_assistant, &mut assistant_texts);

    let user_part = user_texts.join("\n\n");
    let assistant_part = assistant_texts.join("\n");
    let tool_part = tool_meta.join("\n");
    let mut parts: Vec<&str> = Vec::new();
    if !user_part.is_empty() {
        parts.push(&user_part);
    }
    if !assistant_part.is_empty() {
        parts.push(&assistant_part);
    }
    if !tool_part.is_empty() {
        parts.push(&tool_part);
    }
    let text = parts.join("\n\n");

    Ok(DeltaResult::Content { text, file_size })
}

fn build_session_doc(summary: &Summary, content: String, last_indexed_offset: u64) -> SessionDoc {
    let title = summary.display_title().to_owned();

    let mut hasher = blake3::Hasher::new();
    hasher.update(title.as_bytes());
    hasher.update(b"\0");
    hasher.update(content.as_bytes());
    let content_hash = hasher.finalize().to_hex().to_string();

    SessionDoc {
        session_id: summary.info.id.to_string(),
        cwd: summary.info.cwd.clone(),
        updated_at_unix: summary.updated_at.timestamp(),
        title,
        content,
        content_hash,
        last_indexed_offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_search_empty_query() {
        let tmp = tempfile::TempDir::new().unwrap();
        let req = SessionSearchRequest {
            query: "   ".to_string(),
            cwd: None,
            limit: 10,
            offset: 0,
            include_content: false,
        };
        let resp = execute_search(tmp.path(), &req).await.unwrap();
        assert!(resp.results.is_empty());
        assert_eq!(resp.total_estimate, Some(0));
    }

    #[test]
    fn test_execute_search_returns_empty_on_fresh_db() {
        // Test the index directly instead of via `execute_search()` to avoid
        // a race with the global `SEARCH_INDEX_MANAGER` bootstrap worker that
        // concurrently opens the same SQLite DB (flaky "database is locked").
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = search_db_path(tmp.path());
        let index = SessionSearchIndex::open_or_create(&db_path).expect("open fresh DB");
        let result = index.query("hello world", None, 10, 0, false).unwrap();
        assert!(result.results.is_empty());
    }

    fn test_summary(session_id: &str, cwd: &str, title: &str) -> Summary {
        Summary {
            info: Info {
                id: acp::SessionId::new(session_id),
                cwd: cwd.to_string(),
            },
            cwd_generation: 0,
            previous_cwd: None,
            pending_cwd_switch_reminder: None,
            cwd_switch_bookkeeping_generation: 0,
            session_summary: title.to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            num_messages: 0,
            num_chat_messages: 0,
            current_model_id: acp::ModelId::new("test"),
            parent_session_id: None,
            forked_at: None,
            collection_id: None,
            next_trace_turn: 0,
            chat_format_version: 1,
            prompt_display_cwd: None,
            session_kind: None,
            fork_context_source: None,
            fork_parent_prompt_id: None,
            inherited_prefix_len: None,
            hidden: None,
            source_workspace_dir: None,
            git_root_dir: None,
            git_remotes: Vec::new(),
            head_commit: None,
            head_branch: None,
            request_id: None,
            grok_home: None,
            last_active_at: None,
            generated_title: None,
            title_is_manual: false,
            worktree_label: None,
            agent_name: None,
            sandbox_profile: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn test_build_session_doc_hashes_content() {
        let summary = test_summary("test-session", "/workspace", "My session title");

        let doc = build_session_doc(&summary, "prompt text".to_string(), 0);
        assert_eq!(doc.session_id, "test-session");
        assert_eq!(doc.title, "My session title");
        assert_eq!(doc.content, "prompt text");
        assert!(!doc.content_hash.is_empty());

        // Same content + same title → same hash
        let doc2 = build_session_doc(&summary, "prompt text".to_string(), 0);
        assert_eq!(doc.content_hash, doc2.content_hash);
    }

    // ── helpers for single-pass tests ──────────────────────────────────────

    /// Write an updates.jsonl temp file from envelope strings.
    fn write_updates_jsonl(lines: &[String]) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    fn acp_update(session_update_json: &str) -> String {
        format!(
            r#"{{"timestamp":1,"method":"session/update","params":{{"sessionId":"s","update":{session_update_json}}}}}"#
        )
    }

    fn xai_update(session_update_json: &str) -> String {
        format!(
            r#"{{"timestamp":1,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{session_update_json}}}}}"#
        )
    }

    // ── single-pass content collection tests ─────────────────────────────

    #[test]
    fn test_single_pass_extracts_user_prompts() {
        let lines = vec![
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello world"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi there"}}"#,
            ),
        ];
        let f = write_updates_jsonl(&lines);
        let (content, _bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        assert!(
            content.contains("hello world"),
            "should contain user prompt"
        );
    }

    #[test]
    fn test_single_pass_extracts_assistant_text() {
        let lines = vec![
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"assistant reply"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"next prompt"}}"#,
            ),
        ];
        let f = write_updates_jsonl(&lines);
        let (content, _bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        assert!(
            content.contains("assistant reply"),
            "should contain assistant text"
        );
    }

    #[test]
    fn test_single_pass_extracts_tool_metadata() {
        let lines = vec![acp_update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"Read file","kind":"read","locations":[{"path":"/tmp/foo.rs"}]}"#,
        )];
        let f = write_updates_jsonl(&lines);
        let (content, _bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        assert!(content.contains("Read file"), "should contain tool title");
        assert!(
            content.contains("/tmp/foo.rs"),
            "should contain tool location path"
        );
    }

    #[test]
    fn test_single_pass_extracts_text_with_json_escapes() {
        // Escaped JSON strings cannot be borrowed as &str; a regression to
        // borrowed peek fields silently drops these messages from the index.
        let lines = vec![
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"fix the bug\nin main.rs"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"use \"quotes\" and caf\u00e9"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"! echo \"hi\"","_meta":{"bash_command":"echo \"hi\""}}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"Run \"cargo test\"","kind":"execute","locations":[{"path":"/tmp/my\tdir/foo.rs"}]}"#,
            ),
        ];
        let f = write_updates_jsonl(&lines);
        let (content, _bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        assert!(
            content.contains("fix the bug\nin main.rs"),
            "multiline user prompt must be indexed: {content:?}"
        );
        assert!(
            content.contains("use \"quotes\" and caf\u{e9}"),
            "assistant text with escaped quotes and unicode escape must be indexed: {content:?}"
        );
        assert!(
            content.contains("Run \"cargo test\""),
            "tool title with escaped quotes must be indexed: {content:?}"
        );
        assert!(
            content.contains("/tmp/my\tdir/foo.rs"),
            "tool location path with escapes must be indexed: {content:?}"
        );
        assert!(
            !content.contains("echo \"hi\""),
            "escaped bash command must still be excluded from the index: {content:?}"
        );
    }

    #[test]
    fn test_single_pass_handles_rewind() {
        let lines = vec![
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"first prompt"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first reply"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"second prompt"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"second reply"}}"#,
            ),
            xai_update(
                r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"replacement prompt"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"replacement reply"}}"#,
            ),
        ];
        let f = write_updates_jsonl(&lines);
        let (content, _bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        assert!(
            content.contains("first prompt"),
            "first prompt should survive rewind"
        );
        assert!(
            !content.contains("second prompt"),
            "rewound prompt should be removed"
        );
        assert!(
            content.contains("replacement prompt"),
            "replacement prompt should be present"
        );
    }

    #[test]
    fn test_single_pass_thought_chunk_does_not_flush_assistant() {
        // agent_thought_chunk interleaved between agent_message_chunk should
        // NOT break the assistant text into separate entries.
        let lines = vec![
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking about stuff"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}}"#,
            ),
            // A user message ends the assistant turn
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"thanks"}}"#,
            ),
        ];
        let f = write_updates_jsonl(&lines);
        let (content, _bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        // "hello" and "world" should be in the same assistant turn (not split)
        assert!(
            content.contains("hello world"),
            "thought chunk should not flush assistant text: got {content:?}"
        );
    }

    #[test]
    fn test_single_pass_empty_file() {
        let f = write_updates_jsonl(&[]);
        let (content, bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        assert!(content.is_empty() || content.trim().is_empty());
        assert_eq!(bytes, 0, "empty file should report 0 bytes read");
    }

    #[test]
    fn test_single_pass_nonexistent_file() {
        let (content, bytes) =
            collect_all_indexable_content_single_pass(Path::new("/nonexistent/updates.jsonl"))
                .unwrap();
        assert!(content.is_empty());
        assert_eq!(bytes, 0, "nonexistent file should report 0 bytes read");
    }

    #[test]
    fn test_single_pass_assistant_text_cap() {
        // Two 60K chunks in the same turn — the 100K assistant cap should
        // truncate the second chunk.  Total assistant text ≤ 100K.
        let big_text = "x".repeat(60_000);
        let lines = vec![
            acp_update(&format!(
                r#"{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{big_text}"}}}}"#
            )),
            acp_update(&format!(
                r#"{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{big_text}"}}}}"#
            )),
            // Flush the assistant turn
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"q"}}"#,
            ),
        ];
        let f = write_updates_jsonl(&lines);
        let (content, _bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        // Count 'x' chars — the assistant section is the only source of 'x'
        let x_count = content.chars().filter(|&c| c == 'x').count();
        assert!(
            x_count <= 100_000,
            "assistant text should be capped at 100K chars, got {x_count}"
        );
        // Must have truncated the second chunk (60K + 60K > 100K)
        assert!(
            x_count < 120_001,
            "without the cap this would be 120K, got {x_count}"
        );
        // Verify we actually collected substantial text (not accidentally empty)
        assert!(
            x_count > 50_000,
            "should have collected at least the first 60K chunk, got {x_count}"
        );
    }

    #[test]
    fn test_single_pass_tool_call_count_cap() {
        // Generate 250 tool calls — only the first 200 should be indexed
        let lines: Vec<String> = (0..250)
            .map(|i| {
                acp_update(&format!(
                    r#"{{"sessionUpdate":"tool_call","toolCallId":"tc{i}","title":"tool_{i}","kind":"exec","locations":[]}}"#
                ))
            })
            .collect();
        let f = write_updates_jsonl(&lines);
        let (content, _bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        // tool_200 through tool_249 should NOT appear
        assert!(
            !content.contains("tool_200"),
            "tool calls beyond 200 should be ignored"
        );
        assert!(
            !content.contains("tool_249"),
            "tool calls beyond 200 should be ignored"
        );
        // tool_0 and tool_199 should appear
        assert!(content.contains("tool_0"), "first tool should be indexed");
        assert!(
            content.contains("tool_199"),
            "tool #200 (0-indexed) should be indexed"
        );
    }

    #[test]
    fn test_single_pass_tool_chars_cap() {
        // Generate tool calls with long titles that exceed the 100K char budget
        let long_title = "a".repeat(20_000);
        let lines: Vec<String> = (0..10)
            .map(|i| {
                acp_update(&format!(
                    r#"{{"sessionUpdate":"tool_call","toolCallId":"tc{i}","title":"{long_title}","kind":"exec","locations":[]}}"#
                ))
            })
            .collect();
        let f = write_updates_jsonl(&lines);
        let (content, _bytes) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        // 10 * 20K = 200K, but cap is 100K, so 'a' count should be ≤ 100K
        let a_count = content.chars().filter(|&c| c == 'a').count();
        assert!(
            a_count <= 100_000,
            "tool metadata should be capped at 100K chars, got {a_count}"
        );
        // Should have at least some tool metadata
        assert!(
            a_count > 19_000,
            "should have collected at least one tool title, got {a_count}"
        );
    }

    /// A title rename with identical content must produce a different hash,
    /// otherwise the dedup check in `upsert_session` skips the update and
    /// the old title stays in the index until the next full reindex.
    #[test]
    fn test_build_session_doc_title_change_changes_hash() {
        let old = test_summary("s1", "/workspace", "Old title");
        let new = test_summary("s1", "/workspace", "New title");
        let content = "same prompt text".to_string();

        let doc_old = build_session_doc(&old, content.clone(), 0);
        let doc_new = build_session_doc(&new, content, 0);

        assert_ne!(
            doc_old.content_hash, doc_new.content_hash,
            "title change must produce a different hash so dedup doesn't skip the upsert"
        );
    }

    #[test]
    fn test_build_session_doc_prefers_generated_title() {
        let mut summary = test_summary("s1", "/workspace", "session summary");
        summary.generated_title = Some("Generated Title".to_string());
        let doc = build_session_doc(&summary, "content".to_string(), 0);
        assert_eq!(doc.title, "Generated Title");

        summary.generated_title = Some(String::new());
        let doc2 = build_session_doc(&summary, "content".to_string(), 0);
        assert_eq!(doc2.title, "session summary");
    }

    // ── bootstrap config tests ─────────────────────────────────────────────

    #[test]
    fn test_bootstrap_config_defaults() {
        let config = BootstrapConfig::default();
        assert_eq!(config.max_concurrent, 4);
        assert_eq!(config.per_session_timeout, Duration::from_secs(30));
        assert_eq!(config.max_file_size, 30 * 1024 * 1024);
    }

    // ── should_skip_session tests ──────────────────────────────────────────

    #[test]
    fn test_should_skip_session_large_file() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&[0u8; 1024]).unwrap();
        f.flush().unwrap();

        assert!(should_skip_session(f.path(), 512));
    }

    #[test]
    fn test_should_skip_session_small_file() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&[0u8; 1024]).unwrap();
        f.flush().unwrap();

        assert!(!should_skip_session(f.path(), 2048));
    }

    #[test]
    fn test_should_skip_session_exact_limit() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&[0u8; 1024]).unwrap();
        f.flush().unwrap();

        assert!(!should_skip_session(f.path(), 1024));
    }

    #[test]
    fn test_should_skip_session_nonexistent_file() {
        assert!(!should_skip_session(
            Path::new("/nonexistent/updates.jsonl"),
            100
        ));
    }

    // ── progress and status tests ──────────────────────────────────────────

    #[test]
    fn test_bootstrap_progress_extended_defaults() {
        let progress = BootstrapProgress::default();
        assert!(!progress.bootstrapping.load(Ordering::Relaxed));
        assert_eq!(progress.indexed.load(Ordering::Relaxed), 0);
        assert_eq!(progress.total.load(Ordering::Relaxed), 0);
        assert_eq!(progress.skipped.load(Ordering::Relaxed), 0);
        assert_eq!(progress.unchanged.load(Ordering::Relaxed), 0);
        assert_eq!(progress.bytes_read.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_search_index_status_serialization() {
        let status = SearchIndexStatus {
            bootstrapping: true,
            indexed: 10,
            total: 20,
            skipped: 3,
            unchanged: 5,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"skipped\":3"));
        assert!(json.contains("\"unchanged\":5"));
        assert!(json.contains("\"bootstrapping\":true"));
    }

    // ── single-pass bytes_read tests ───────────────────────────────────────

    #[test]
    fn test_single_pass_reports_bytes_read() {
        let lines = vec![acp_update(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}"#,
        )];
        let f = write_updates_jsonl(&lines);
        let file_size = std::fs::metadata(f.path()).unwrap().len();

        let (_content, bytes_read) = collect_all_indexable_content_single_pass(f.path()).unwrap();
        assert_eq!(
            bytes_read, file_size,
            "bytes_read should match the actual file size"
        );
        assert!(
            bytes_read > 0,
            "bytes_read should be non-zero for non-empty file"
        );
    }

    // ── delta indexing tests ───────────────────────────────────────────────

    /// Append new lines to an existing updates.jsonl file and return the
    /// byte offset where the new content starts.
    fn append_updates_jsonl(path: &Path, lines: &[String]) -> u64 {
        use std::io::Write as _;
        let offset = std::fs::metadata(path).unwrap().len();
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        offset
    }

    #[test]
    fn test_delta_append_extracts_new_content() {
        // Write initial content
        let initial = vec![
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello world"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi there"}}"#,
            ),
        ];
        let f = write_updates_jsonl(&initial);
        let offset = std::fs::metadata(f.path()).unwrap().len();

        // Append new content
        let delta = vec![
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"second question"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"second answer"}}"#,
            ),
        ];
        append_updates_jsonl(f.path(), &delta);

        let result = collect_delta_content(f.path(), offset).unwrap();
        match result {
            DeltaResult::Content { text, file_size } => {
                assert!(
                    text.contains("second question"),
                    "delta should contain new user prompt"
                );
                assert!(
                    text.contains("second answer"),
                    "delta should contain new assistant text"
                );
                assert!(
                    !text.contains("hello world"),
                    "delta should not contain pre-offset content"
                );
                assert!(
                    file_size > offset,
                    "file_size should be larger than the starting offset"
                );
            }
            DeltaResult::NeedsFullReread => {
                panic!("expected Content, got NeedsFullReread");
            }
        }
    }

    #[test]
    fn test_delta_rewind_triggers_full_reread() {
        let initial = vec![acp_update(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"first"}}"#,
        )];
        let f = write_updates_jsonl(&initial);
        let offset = std::fs::metadata(f.path()).unwrap().len();

        // Append a rewind marker in the delta window
        let delta = vec![
            xai_update(
                r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"replacement"}}"#,
            ),
        ];
        append_updates_jsonl(f.path(), &delta);

        let result = collect_delta_content(f.path(), offset).unwrap();
        assert!(
            matches!(result, DeltaResult::NeedsFullReread),
            "rewind in delta should trigger NeedsFullReread"
        );
    }

    #[test]
    fn test_delta_no_new_bytes() {
        let lines = vec![acp_update(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}"#,
        )];
        let f = write_updates_jsonl(&lines);
        let file_size = std::fs::metadata(f.path()).unwrap().len();

        // Offset equals file size — no new bytes
        let result = collect_delta_content(f.path(), file_size).unwrap();
        match result {
            DeltaResult::Content { text, .. } => {
                assert!(text.is_empty(), "no new bytes should produce empty text");
            }
            DeltaResult::NeedsFullReread => {
                panic!("expected Content with empty text, got NeedsFullReread");
            }
        }
    }

    #[test]
    fn test_delta_truncation_detected() {
        let lines = vec![acp_update(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello world this is a long message"}}"#,
        )];
        let f = write_updates_jsonl(&lines);
        let file_size = std::fs::metadata(f.path()).unwrap().len();

        // Offset larger than file size — simulates truncation
        let result = collect_delta_content(f.path(), file_size + 100).unwrap();
        match result {
            DeltaResult::Content { text, .. } => {
                assert!(
                    text.is_empty(),
                    "offset beyond file size should produce empty text"
                );
            }
            DeltaResult::NeedsFullReread => {
                panic!("expected Content with empty text for truncation");
            }
        }
    }

    // ── bootstrap_once eager flag tests ────────────────────────────────────
    // NOTE: SEARCH_INDEX_MANAGER is a process-wide singleton, so tests
    // that depend on the `bootstrapping` flag transitioning to `false`
    // are racy when run in parallel (another test's bootstrap_once()
    // can re-set the flag). Only the eager-set test is reliable because
    // the store is synchronous before the channel send.

    #[tokio::test]
    async fn test_bootstrap_once_sets_flag_eagerly() {
        let tmp = tempfile::TempDir::new().unwrap();
        SEARCH_INDEX_MANAGER.bootstrap_once(tmp.path().to_path_buf());
        assert!(
            SEARCH_INDEX_MANAGER
                .progress
                .bootstrapping
                .load(Ordering::Acquire),
            "bootstrapping flag must be true immediately after bootstrap_once()",
        );
    }

    #[tokio::test]
    async fn test_execute_search_completes_on_fresh_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let req = SessionSearchRequest {
            query: "nonexistent-query-xyzzy".to_string(),
            cwd: None,
            limit: 10,
            offset: 0,
            include_content: false,
        };
        let resp = execute_search(tmp.path(), &req).await.unwrap();
        assert!(resp.results.is_empty());
    }

    // ── bootstrap freshness recheck tests ──────────────────────────────────
    // These test the free functions directly (per-tmp-root DB state), not the
    // global SEARCH_INDEX_MANAGER, whose `bootstrapping` flag is process-wide
    // and racy across parallel tests (see NOTE above).

    /// The predicate behind `SearchIndexJob::RecheckBootstrap`: a completed
    /// bootstrap leaves `last_bootstrap_at`; the upgrade drop in
    /// `open_or_create` deletes it, so the probe's own open detects a
    /// downgraded index.
    #[tokio::test]
    async fn test_has_completed_bootstrap_marker_lifecycle() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let db_path = search_db_path(root);

        // No DB file at all → genuinely absent (not a read error).
        assert_eq!(has_completed_bootstrap_marker(root).await, Some(false));

        // A completed bootstrap at the current schema version → marker set.
        search_remote_sync::write_last_bootstrap_at(&db_path).unwrap();
        assert_eq!(has_completed_bootstrap_marker(root).await, Some(true));

        // Simulate an older (pre-ratchet) binary having wiped and re-stamped
        // the DB: version row regressed below current, `last_bootstrap_at`
        // still recent.
        {
            let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
            index
                .set_meta("session_search_schema_version", "3")
                .unwrap();
        }
        assert_eq!(
            has_completed_bootstrap_marker(root).await,
            Some(false),
            "a downgraded index must not count as bootstrapped even with a recent marker"
        );

        // A subsequent completed bootstrap restores the marker.
        search_remote_sync::write_last_bootstrap_at(&db_path).unwrap();
        assert_eq!(has_completed_bootstrap_marker(root).await, Some(true));
    }

    /// End-to-end recheck healing: `RecheckBootstrap` on a marker-less index
    /// re-runs the full bootstrap, which rewrites the marker on completion.
    #[tokio::test]
    async fn test_recheck_bootstrap_reruns_reindex_when_marker_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let storage: Box<dyn StorageAdapter> = Box::new(
            crate::session::storage::jsonl::JsonlStorageAdapter::with_root(root.to_path_buf()),
        );
        let mut pending: HashMap<SessionSearchKey, Instant> = HashMap::new();

        assert_eq!(has_completed_bootstrap_marker(root).await, Some(false));
        handle_job(
            root,
            storage.as_ref(),
            &mut pending,
            SearchIndexJob::RecheckBootstrap,
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(
            has_completed_bootstrap_marker(root).await,
            Some(true),
            "recheck on a marker-less index must re-run the bootstrap, which rewrites the marker"
        );
    }

    /// Regression shape: a v3-era indexer silently extracted "" for
    /// sessions with JSON escapes but still recorded a content hash, so at
    /// the *same* schema version the hash dedup keeps skipping identical
    /// (buggy) re-extractions forever. Pins that the v4 upgrade drop removes
    /// the stub row and its hash, so the next bootstrap re-indexes from
    /// scratch instead of being blocked by the stale hash.
    #[test]
    fn test_upgrade_drop_clears_stub_docs_and_hashes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = search_db_path(tmp.path());

        let summary = test_summary("stub", "/ws", "");
        let stub = build_session_doc(&summary, String::new(), 0);
        {
            let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
            index.upsert_doc(&stub).unwrap();
            // The empty-content stub still records a hash — re-extracting
            // the same (empty) content would dedup to Unchanged.
            assert_eq!(
                index.get_content_hash("stub").unwrap().as_deref(),
                Some(stub.content_hash.as_str())
            );
            index
                .set_meta("session_search_schema_version", "3")
                .unwrap();
        }

        let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
        assert_eq!(
            index.get_content_hash("stub").unwrap(),
            None,
            "the upgrade drop must clear stub rows so their stale hashes cannot block re-indexing"
        );
    }

    // Note: tests for upsert_session_blocking (delta path, truncation
    // fallback, rewind fallback, no-new-bytes skip) are deferred until
    // the delta indexing is wired into upsert_session. The delta content
    // collection function (collect_delta_content) is tested above.
}
