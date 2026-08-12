//! Session search orchestration: querying and background indexing.
//!
//! The FTS index is bootstrapped on first search and updated per session via
//! `notify_session_updated()`. The SQLite DB is shared with other grok
//! processes (older binaries may wipe or downgrade it on open), so every
//! search re-verifies the on-disk completed-bootstrap marker, and the
//! bootstrap itself is cross-process single-flight.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::StorageAdapter;
use super::search_bootstrap::{
    BootstrapOutcome, BootstrapProgress, BootstrappingGuard, bootstrap_with_lease,
    has_completed_bootstrap_marker, try_bootstrap_with_lease,
};
use super::search_content::{
    UpsertOutcome, build_session_doc, collect_all_indexable_content_single_pass,
    upsert_unless_unchanged,
};
use super::search_db::{
    HealAwareLogCounter, log_session_index_failure, search_db_path, with_search_index,
};
use super::search_fts::{META_KEY_BOOTSTRAP_CLAIM, META_KEY_LAST_BOOTSTRAP, SessionSearchRow};
#[cfg(test)]
use super::search_fts::{META_KEY_SCHEMA_VERSION, SessionSearchIndex};
use super::search_recovery;
use crate::session::info::Info;
use crate::session::persistence::Summary;
use agent_client_protocol as acp;

const SEARCH_INDEX_DEBOUNCE_MS: u64 = 500;
const BOOTSTRAP_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const BOOTSTRAP_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    /// True while the index is still bootstrapping; callers should re-query.
    /// Also true when a live claim exists without a completion marker, so a
    /// peer mid-rebuild or a dead claimant within its lease is visible.
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
    /// Re-verify the on-disk completed-bootstrap marker; re-run the full
    /// bootstrap when it is missing.
    RecheckBootstrap,
}

enum SearchManagerCmd {
    Enqueue { root: PathBuf, job: SearchIndexJob },
    BootstrapOnce { root: PathBuf },
}

struct SearchManagerState {
    workers: HashMap<PathBuf, mpsc::UnboundedSender<SearchIndexJob>>,
    bootstrapped: HashSet<PathBuf>,
}

/// Singleton that manages background session indexing.
///
/// Requires an active tokio runtime on first access (spawns tasks).
pub struct SearchIndexManager {
    tx: mpsc::UnboundedSender<SearchManagerCmd>,
    pub(super) progress: Arc<BootstrapProgress>,
}

/// Global singleton — lazily started on first use.
pub static SEARCH_INDEX_MANAGER: LazyLock<SearchIndexManager> =
    LazyLock::new(SearchIndexManager::start);

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
        let (tx, mut rx) = mpsc::unbounded_channel::<SearchManagerCmd>();

        tokio::spawn(async move {
            let mut state = SearchManagerState {
                workers: HashMap::new(),
                bootstrapped: HashSet::new(),
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
                            // The DB is shared: re-verify the on-disk marker,
                            // sequenced after any in-flight BootstrapAll.
                            Self::dispatch(&mut state, root, SearchIndexJob::RecheckBootstrap);
                        }
                    }
                }
            }
        });

        Self { tx, progress }
    }

    /// Queue a bootstrap of all sessions (idempotent per root; repeat calls
    /// re-verify the on-disk marker). Sets `bootstrapping` eagerly so
    /// pollers see `true` before the background task starts.
    pub fn bootstrap_once(&self, root: PathBuf) {
        self.progress.begin_bootstrapping();
        let _ = self.tx.send(SearchManagerCmd::BootstrapOnce { root });
    }

    pub fn status(&self) -> SearchIndexStatus {
        SearchIndexStatus {
            bootstrapping: self.progress.is_bootstrapping(),
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
pub fn notify_session_updated(session_id: &str, cwd: &str) {
    let root = crate::util::grok_home::grok_home();
    SEARCH_INDEX_MANAGER.enqueue(root, session_id.to_string(), cwd.to_string());
}

/// Execute a session search query, waiting up to [`BOOTSTRAP_WAIT_TIMEOUT`]
/// for a first-call bootstrap so the query runs against a populated index.
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

    let epoch = search_recovery::CacheEpoch::now();
    let deadline = tokio::time::Instant::now() + BOOTSTRAP_WAIT_TIMEOUT;
    while SEARCH_INDEX_MANAGER.progress.is_bootstrapping() {
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

    let (query_result, claim_in_flight) = tokio::task::spawn_blocking(move || {
        with_search_index(&db_path, |index| {
            let result =
                index.query(&query_owned, cwd.as_deref(), limit, offset, include_content)?;
            let claim_in_flight = index.get_meta(META_KEY_BOOTSTRAP_CLAIM)?.is_some()
                && index.get_meta(META_KEY_LAST_BOOTSTRAP)?.is_none();
            Ok((result, claim_in_flight))
        })
    })
    .await
    .map_err(io::Error::other)??;

    let healed = epoch.changed();
    if healed {
        SEARCH_INDEX_MANAGER.bootstrap_once(root_dir.to_path_buf());
    }

    Ok(SessionSearchResponse {
        results: query_result.results,
        next_offset: query_result.next_offset,
        total_estimate: query_result.total_estimate,
        bootstrapping: healed
            || SEARCH_INDEX_MANAGER.progress.is_bootstrapping()
            || claim_in_flight,
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

static BOOTSTRAP_FAIL_LOG: HealAwareLogCounter = HealAwareLogCounter::new(4);

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
            let _bootstrapping = BootstrappingGuard::new(&SEARCH_INDEX_MANAGER.progress);
            match bootstrap_with_lease(root_dir, storage, &SEARCH_INDEX_MANAGER.progress).await {
                Ok(BootstrapOutcome::Done) => {}
                Ok(BootstrapOutcome::RunAgain) => {
                    SEARCH_INDEX_MANAGER.bootstrap_once(root_dir.to_path_buf());
                }
                Err(e) => BOOTSTRAP_FAIL_LOG.warn(
                    "bootstrap failures",
                    "session search bootstrap failed",
                    None,
                    Some(&e),
                ),
            }
        }
        SearchIndexJob::RecheckBootstrap => {
            let _bootstrapping = BootstrappingGuard::new(&SEARCH_INDEX_MANAGER.progress);
            match has_completed_bootstrap_marker(root_dir).await {
                Some(true) => {}
                Some(false) => {
                    tracing::info!(
                        "session search index missing completed-bootstrap marker; entering bootstrap gate"
                    );
                    match try_bootstrap_with_lease(
                        root_dir,
                        storage,
                        &SEARCH_INDEX_MANAGER.progress,
                    )
                    .await
                    {
                        Ok(BootstrapOutcome::Done) => {}
                        Ok(BootstrapOutcome::RunAgain) => {
                            SEARCH_INDEX_MANAGER.bootstrap_once(root_dir.to_path_buf());
                        }
                        Err(e) => BOOTSTRAP_FAIL_LOG.warn(
                            "bootstrap failures",
                            "session search re-bootstrap failed",
                            None,
                            Some(&e),
                        ),
                    }
                }
                // Transient read failure: rebuilding on every one would be a
                // reindex storm; the next search retries the probe.
                None => {
                    tracing::debug!(
                        "session search bootstrap marker unreadable; skipping re-bootstrap"
                    );
                }
            }
        }
    }
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
            log_session_index_failure(
                &key.session_id,
                &e,
                "failed upserting session in search index",
            );
        }
    }
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
    let (content, bytes_read) = if let Some(updates_path) = storage.updates_file_path(info) {
        tokio::task::spawn_blocking(move || {
            collect_all_indexable_content_single_pass(&updates_path)
        })
        .await
        .map_err(io::Error::other)??
    } else {
        return Ok(UpsertOutcome::NoContent);
    };
    let doc = build_session_doc(summary, content);
    let db_path = search_db_path(root_dir);

    tokio::task::spawn_blocking(move || {
        with_search_index(&db_path, |index| {
            upsert_unless_unchanged(index, &doc, bytes_read)
        })
    })
    .await
    .map_err(io::Error::other)?
}

async fn delete_session(root_dir: &Path, session_id: &str) -> io::Result<()> {
    let db_path = search_db_path(root_dir);
    let session_id = session_id.to_string();
    tokio::task::spawn_blocking(move || {
        with_search_index(&db_path, |index| index.delete_doc(&session_id))
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::storage::search_content::test_summary;

    #[test]
    #[cfg(unix)]
    fn search_db_path_tightens_sessions_root() {
        let tmp = tempfile::TempDir::new().unwrap();

        let _ = search_db_path(tmp.path());

        assert_eq!(
            crate::test_support::unix_mode(&tmp.path().join("sessions")),
            0o700,
            "sessions root must be 0700"
        );
    }

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

    #[test]
    fn test_bootstrap_progress_extended_defaults() {
        let progress = BootstrapProgress::default();
        assert!(!progress.is_bootstrapping());
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
            SEARCH_INDEX_MANAGER.progress.is_bootstrapping(),
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
        let stub = build_session_doc(&summary, String::new());
        {
            let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
            index.upsert_doc(&stub).unwrap();
            // The empty-content stub still records a hash — re-extracting
            // the same (empty) content would dedup to Unchanged.
            assert_eq!(
                index.get_content_hash("stub").unwrap().as_deref(),
                Some(stub.content_hash.as_str())
            );
            index.set_meta(META_KEY_SCHEMA_VERSION, "3").unwrap();
        }

        let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
        assert_eq!(
            index.get_content_hash("stub").unwrap(),
            None,
            "the upgrade drop must clear stub rows so their stale hashes cannot block re-indexing"
        );
    }
}
