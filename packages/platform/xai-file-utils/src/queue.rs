//! Spill-to-disk upload queue for cloud storage trace artifacts.
//!
//! Decouples data capture (inline, synchronous) from network upload (background, async).
//! Artifacts are written to temp files on disk at capture time, then uploaded by a
//! background worker with retries and error budget. This prevents data loss when
//! uploads fail transiently (429 rate limits, proxy restarts, network blips).
//!
//! The worker processes up to `max_concurrent` items in parallel using a semaphore.
//! Each item is spawned as an independent tokio task with its own retry loop.
//! The circuit breaker pauses dispatch (not in-flight tasks) when too many failures
//! accumulate without any successes.
use crate::gcs::{StorageConfig, upload_bytes, upload_file, upload_stream};
use crate::storage_client::{Auth401AttributionCallback, HttpUploadError};
use crate::{BlobCompression, TraceExportConfig, UploadMethod};
use anyhow::Context;
use async_compression::tokio::bufread::ZstdEncoder;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{Notify, mpsc, oneshot};
use tracing::Instrument;
use xai_circuit_breaker::{Disposition, RetryPolicy};
use xai_grok_auth::AuthCredentialProvider;
/// Resolves current upload credentials at upload time, plus optional
/// hooks the queue worker uses to wire refresh-aware credentials and
/// `auth_401_attribution` emission into the per-upload `StorageClient`.
///
/// The agent implements this by delegating to its AuthManager, ensuring fresh
/// tokens even when items have been queued for minutes. This avoids stale-token
/// failures on retried items whose original credentials may have expired.
///
/// `proxy_attribution`, `proxy_credentials`, and `proxy_http_client` mirror
/// the same-named methods on [`StorageConfig`]. They default to `None` so existing
/// implementors (tests, no-auth direct-mode resolvers) keep compiling without
/// changes; the queue worker calls them on every dispatch and stitches the
/// returned `Option`s onto the resolved [`TraceExportConfig`] before handing
/// it to the upload helpers.
pub trait TraceExportSource: Send + Sync {
    fn resolve(&self) -> TraceExportConfig;
    /// Async variant. Override to drive auth refresh; default delegates to sync.
    fn resolve_async(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TraceExportConfig> + Send + '_>> {
        Box::pin(std::future::ready(self.resolve()))
    }
    /// 401-attribution callback for the per-upload `StorageClient`. Default
    /// `None` keeps the pre-existing behavior (no attribution events).
    fn proxy_attribution(&self) -> Option<Arc<dyn Auth401AttributionCallback>> {
        None
    }
    /// Refresh-aware credential provider for the per-upload `StorageClient`.
    /// Default `None` keeps the pre-existing behavior (the static `user_token`
    /// snapshot baked into the resolved `TraceExportConfig` is used).
    fn proxy_credentials(&self) -> Option<Arc<dyn AuthCredentialProvider>> {
        None
    }
    /// Tuned `reqwest::Client` for the per-upload `StorageClient`. Default
    /// `None` falls back to `reqwest::Client::new()` inside the helpers.
    fn proxy_http_client(&self) -> Option<reqwest::Client> {
        None
    }
    /// Park-on-401 recovery signal: a future resolving `true` iff credentials
    /// changed within `timeout`. `failed_bearer` is the token the rejected
    /// attempt used — implementations must resolve `true` immediately when
    /// the current credential already differs, or a rotation landing between
    /// wait slices is missed and retry stalls until the probe interval.
    /// `None` (the default) means no recovery is possible — static creds,
    /// S3/direct mode, or IdP-confirmed permanent failure — and the worker
    /// drops the auth-failed item immediately instead of parking it.
    fn wait_for_auth_recovery(
        &self,
        failed_bearer: Option<&str>,
        timeout: Duration,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>>> {
        let _ = (failed_bearer, timeout);
        None
    }
    /// Whether the resolver holds a credential worth a real wire attempt — an
    /// unexpired token (in memory or on disk), or a static key. Default `true`
    /// always probes.
    fn has_usable_credential(&self) -> bool {
        true
    }
}
/// Worker-side wrapper that bundles a resolved `TraceExportConfig` with the
/// optional attribution / credentials / http_client provided by the
/// `TraceExportSource`. Constructed once per dispatch attempt so a token
/// rotation between attempts is reflected on the next try.
struct ResolvedStorageConfig {
    config: TraceExportConfig,
    attribution: Option<Arc<dyn Auth401AttributionCallback>>,
    credentials: Option<Arc<dyn AuthCredentialProvider>>,
    http_client: Option<reqwest::Client>,
}
impl ResolvedStorageConfig {
    /// Resolve config with fresh auth via `resolve_async`.
    async fn from_resolver_async(resolver: &Arc<dyn TraceExportSource>) -> Self {
        Self {
            config: resolver.resolve_async().await,
            attribution: resolver.proxy_attribution(),
            credentials: resolver.proxy_credentials(),
            http_client: resolver.proxy_http_client(),
        }
    }
    /// Bearer this resolved config puts on the wire — `snapshot()` mirrors
    /// `HttpAuth::apply` for provider-backed configs; the static fallback
    /// mirrors `GrokAuthCredentials::apply` precedence (deployment key wins).
    fn wire_bearer(&self) -> Option<String> {
        if let Some(ref creds) = self.credentials {
            return creds.snapshot().token;
        }
        match self.config.upload_method() {
            UploadMethod::Proxy {
                user_token,
                deployment_key,
                ..
            } => deployment_key
                .clone()
                .or_else(|| (!user_token.is_empty()).then(|| user_token.clone())),
            _ => None,
        }
    }
}
impl StorageConfig for ResolvedStorageConfig {
    fn bucket_url(&self) -> &str {
        self.config.bucket_url()
    }
    fn upload_method(&self) -> &UploadMethod {
        self.config.upload_method()
    }
    fn proxy_attribution(&self) -> Option<Arc<dyn Auth401AttributionCallback>> {
        self.attribution.clone()
    }
    fn proxy_credentials(&self) -> Option<Arc<dyn AuthCredentialProvider>> {
        self.credentials.clone()
    }
    fn proxy_http_client(&self) -> Option<reqwest::Client> {
        self.http_client.clone()
    }
}
/// Default max age for upload queue items (2 hours).
///
/// Used by both the retry policy (`max_age`) and the startup orphan cleanup
/// (`cleanup_orphaned_uploads`). Kept as a constant so the two stay in sync —
/// if the cleanup threshold is shorter than the retry max_age, a process restart
/// can delete temp files that the previous worker was still trying to upload.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(2 * 60 * 60);
/// Retry policy for individual queue items.
#[derive(Clone, Debug)]
pub struct UploadRetryPolicy {
    /// Max attempts per item before giving up.
    pub max_attempts: u32,
    /// Initial backoff delay.
    pub initial_delay: Duration,
    /// Max backoff delay.
    pub max_delay: Duration,
    /// Backoff multiplier.
    pub multiplier: f64,
    /// Max age — items older than this are dropped to prevent unbounded growth.
    pub max_age: Duration,
    /// Minimum wall time between wire probe attempts while parked for auth
    /// recovery — the fallback for 401s that heal server-side without a
    /// client credential rotation. Env override:
    /// `GROK_UPLOAD_QUEUE_AUTH_PROBE_SECS`.
    pub auth_park_probe_interval: Duration,
}
pub const DEFAULT_AUTH_PARK_PROBE_INTERVAL: Duration = Duration::from_secs(300);
/// Smallest probe interval a `GROK_UPLOAD_QUEUE_AUTH_PROBE_SECS` override may
/// set. Probes can't fire faster than `AUTH_PARK_WAIT_INTERVAL` regardless, so
/// this exists mainly to reject the degenerate `0` (whole-second granularity
/// means a non-zero value already floors at one second).
const MIN_AUTH_PARK_PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Resolve a `GROK_UPLOAD_QUEUE_AUTH_PROBE_SECS` override (seconds) into a probe
/// interval. `0` is rejected (`None`) so a misconfiguration can't turn every
/// parked upload into a per-wait-slice retry storm; other values are floored at
/// [`MIN_AUTH_PARK_PROBE_INTERVAL`].
fn auth_park_probe_override(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs).max(MIN_AUTH_PARK_PROBE_INTERVAL))
}
impl Default for UploadRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(120),
            multiplier: 2.0,
            max_age: DEFAULT_MAX_AGE,
            auth_park_probe_interval: DEFAULT_AUTH_PARK_PROBE_INTERVAL,
        }
    }
}
impl UploadRetryPolicy {
    fn backoff_delay(&self, attempt: u32) -> Duration {
        let base_ms = self.initial_delay.as_millis() as f64 * self.multiplier.powi(attempt as i32);
        let capped_ms = base_ms.min(self.max_delay.as_millis() as f64);
        Duration::from_millis(capped_ms as u64)
    }
}
/// Default disk budget for the upload queue temp directory.
const DEFAULT_MAX_QUEUE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Bounded channel capacity — if full, enqueue falls back to inline upload.
const CHANNEL_CAPACITY: usize = 256;
/// Circuit breaker: pause after this many consecutive failures.
const CIRCUIT_BREAKER_THRESHOLD: u32 = 20;
/// Circuit breaker cooldown period.
const CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(60);
/// Default max concurrent uploads in the background worker.
const DEFAULT_MAX_CONCURRENT: usize = 8;
/// Total in-flight byte budget for inline-fallback uploads. Bounds resident
/// memory when uploads pile up under throttling (429s) on a multi-GB dataset.
/// 256 MiB balances upload parallelism against a hard memory lid.
const MAX_INLINE_FALLBACK_INFLIGHT_BYTES: u64 = 256 * 1024 * 1024;
/// Bytes per inline-fallback semaphore permit — see [`inline_fallback_permits`].
const INLINE_FALLBACK_PERMIT_BYTES: u64 = 1024 * 1024;
/// Total permits held by the inline-fallback semaphore (= 256).
const INLINE_FALLBACK_TOTAL_PERMITS: u32 =
    (MAX_INLINE_FALLBACK_INFLIGHT_BYTES / INLINE_FALLBACK_PERMIT_BYTES) as u32;
/// Map an upload size to inline-fallback permits: 1 MiB units rounded up, floor
/// of 1, clamped to the total. The clamp keeps a multi-GB file from requesting
/// more permits than the semaphore holds (which would deadlock `acquire_many`)
/// or overflowing `u32`.
fn inline_fallback_permits(size_bytes: u64) -> u32 {
    let units = size_bytes.div_ceil(INLINE_FALLBACK_PERMIT_BYTES);
    units.clamp(1, INLINE_FALLBACK_TOTAL_PERMITS as u64) as u32
}
/// A queue-owned temp file the worker uploads then deletes. Both variants are
/// owned (the queue never holds a caller's working-tree path); they differ only
/// in disk-budget accounting.
enum UploadSource {
    /// A temp file whose real disk cost equals its size (in-memory artifacts
    /// written to disk, or files copied into the queue dir).
    OwnedTemp(PathBuf),
    /// A reflink/CoW (or real-copy fallback) snapshot of a working-tree file,
    /// taken at enqueue (see `enqueue_file_reference`). `disk_bytes` is its REAL
    /// disk cost — 0 for a reflink (CoW shares blocks), the file size for a copy
    /// — used for budget accounting instead of the (large) logical size.
    OwnedSnapshot { path: PathBuf, disk_bytes: u64 },
}
impl UploadSource {
    /// Filesystem path of the artifact bytes.
    fn path(&self) -> &Path {
        match self {
            UploadSource::OwnedTemp(p) | UploadSource::OwnedSnapshot { path: p, .. } => p,
        }
    }
    /// Real disk bytes this item contributes to the queue budget (0 for a
    /// reflink snapshot, which shares blocks with the source until modified).
    fn disk_bytes(&self, fallback_size: u64) -> u64 {
        match self {
            UploadSource::OwnedTemp(_) => fallback_size,
            UploadSource::OwnedSnapshot { disk_bytes, .. } => *disk_bytes,
        }
    }
}
/// Schema version stamped on every [`QueueItemSidecar`]; bumped only on
/// breaking manifest-shape changes.
pub const QUEUE_ITEM_SIDECAR_SCHEMA_VERSION: u32 = 1;
/// Sidecar manifest written as `<temp>.meta.json` next to a queue temp file by
/// [`UploadQueue::enqueue_bytes_blocking`] (the fire-and-forget paths write the
/// temp file alone). It carries everything a fresh process needs to re-enqueue
/// the upload after a restart — the temp-file name alone is lossy (truncated
/// `session_id`, no GCS path). Read by `xai_grok_workspace::recovery`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueItemSidecar {
    /// Manifest schema version (see [`QUEUE_ITEM_SIDECAR_SCHEMA_VERSION`]).
    #[serde(default = "default_sidecar_schema_version")]
    pub schema_version: u32,
    /// Session that produced the artifact.
    pub session_id: String,
    /// Turn the artifact belongs to.
    pub turn_number: u64,
    /// Destination object path in cloud storage.
    pub gcs_path: String,
    /// MIME type for the upload.
    pub content_type: String,
    pub artifact_name: String,
    /// RFC3339 timestamp of when the item was first enqueued.
    pub enqueued_at: String,
    /// Hex SHA-256 of the temp-file contents, verified at recovery time so a
    /// corrupt temp file is dropped instead of re-uploaded.
    pub sha256: String,
}
fn default_sidecar_schema_version() -> u32 {
    QUEUE_ITEM_SIDECAR_SCHEMA_VERSION
}
/// A pending upload in the spill-to-disk queue.
struct UploadQueueItem {
    /// Source of the artifact bytes and whether the queue owns the file.
    source: UploadSource,
    /// Recovery sidecar path (set only by `enqueue_bytes_blocking`); deleted
    /// with the temp file on every terminal outcome.
    sidecar_path: Option<PathBuf>,
    /// Destination path in cloud storage (e.g., "{session_id}/turn_0/metadata.json").
    gcs_path: String,
    /// Parent span captured at enqueue time so the upload links back to the caller's trace.
    parent_span: tracing::Span,
    /// MIME type for the upload.
    content_type: String,
    /// Human-readable label for logging.
    artifact_name: String,
    /// Number of upload attempts so far.
    attempts: u32,
    /// When this item was first enqueued.
    enqueued_at: Instant,
    /// Optional completion signal for callers that need to block until done.
    completion_tx: Option<oneshot::Sender<anyhow::Result<UploadCompletion>>>,
    /// Grok client version string, stamped on the `gcs_queue_upload` tracing span.
    /// Copied from `UploadQueue::client_version` at enqueue time.
    client_version: Option<String>,
    /// When true, the upload worker compresses the file with zstd before uploading.
    compress: bool,
    /// Un-marks this item's `gcs_path` from the in-flight set on drop (any
    /// terminal outcome). `None` when not dedup-tracked; held only for its `Drop`.
    _in_flight: Option<InFlightGuard>,
}
/// Completion info returned by the upload worker after a successful upload.
#[derive(Debug)]
pub struct UploadCompletion {
    pub gcs_url: String,
    pub compression: BlobCompression,
    pub original_size: u64,
    pub stored_size: u64,
}
/// Result of enqueueing a file with optional compression.
pub struct EnqueueResult {
    pub completion_rx: oneshot::Receiver<anyhow::Result<UploadCompletion>>,
    pub original_size: u64,
}
/// Shared statistics for monitoring and disk budget enforcement.
pub struct UploadQueueStats {
    /// Items counted from enqueue acceptance until upload completion; includes
    /// the [`inflight`](Self::inflight) subset.
    pub pending: AtomicU64,
    /// Total bytes of pending temp files on disk.
    pub pending_bytes: AtomicU64,
    /// Pending items actively uploading right now (a subset of `pending`).
    pub inflight: AtomicU64,
    /// Cumulative items enqueued for background upload.
    pub enqueued: AtomicU64,
    /// Cumulative enqueue attempts dropped because an identical `gcs_path` was
    /// already in flight (local content dedup).
    pub deduplicated: AtomicU64,
    /// Cumulative successful uploads.
    pub uploaded: AtomicU64,
    /// Cumulative failed uploads (exhausted budget, includes expired items).
    pub failed: AtomicU64,
    /// Circuit breaker activations (cumulative count of trips).
    pub circuit_breaker_trips: AtomicU64,
    /// `true` while the breaker is currently paused; cleared after the
    /// cooldown. Distinct from the cumulative `circuit_breaker_trips`.
    pub circuit_breaker_active: AtomicBool,
    /// Times enqueue fell back to inline (queue full or disk budget exceeded).
    pub enqueue_fallbacks: AtomicU64,
    /// Temp files we couldn't remove (non-`NotFound`). Bumped by `try_remove_temp`.
    pub leaked_temp_files: AtomicU64,
    /// Reference uploads skipped because the source was missing or its content
    /// no longer matched `expected_sha256` (corruption guard). Non-fatal.
    pub reference_stale: AtomicU64,
    /// Items that entered the parked-for-auth state. An item parks at most once.
    pub auth_parked: AtomicU64,
    /// Orphan-sweep deletions of a lone queue file (temp without sidecar or
    /// vice versa). Surfaced as `cleanup_orphan_mismatched_total`; only bumped
    /// by [`UploadQueue::cleanup_orphans`], not the legacy free function.
    pub cleanup_orphan_mismatched: AtomicU64,
    /// Optional listener pinged on each pending-count transition so a status
    /// publisher can republish immediately. Wired via `set_transition_notify`.
    transition_notify: OnceLock<Arc<Notify>>,
    /// Internal listener for [`UploadQueue::wait_idle`]. Separate from the
    /// single-slot `transition_notify` so idle-waiters never compete with the
    /// external status publisher for the one wiring.
    idle_notify: Notify,
}
impl Default for UploadQueueStats {
    fn default() -> Self {
        Self::new()
    }
}
impl UploadQueueStats {
    pub fn new() -> Self {
        Self {
            pending: AtomicU64::new(0),
            pending_bytes: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            enqueued: AtomicU64::new(0),
            deduplicated: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            circuit_breaker_trips: AtomicU64::new(0),
            circuit_breaker_active: AtomicBool::new(false),
            enqueue_fallbacks: AtomicU64::new(0),
            leaked_temp_files: AtomicU64::new(0),
            reference_stale: AtomicU64::new(0),
            auth_parked: AtomicU64::new(0),
            cleanup_orphan_mismatched: AtomicU64::new(0),
            transition_notify: OnceLock::new(),
            idle_notify: Notify::new(),
        }
    }
    /// Wire an external transition listener. Set once; a second call is a
    /// no-op (the first notifier wins).
    pub fn set_transition_notify(&self, notify: Arc<Notify>) {
        let _ = self.transition_notify.set(notify);
    }
    /// Wake the wired transition listener, if any, and any idle-waiters.
    fn notify_transition(&self) {
        if let Some(notify) = self.transition_notify.get() {
            notify.notify_waiters();
        }
        self.idle_notify.notify_waiters();
    }
}
/// Remove `path`; on non-`NotFound` failure, warn and bump `leaked_temp_files`
/// (when `stats` is `Some`). `stats` is optional for callers without a live
/// queue handle (e.g. the startup sweep).
pub fn try_remove_temp(path: &Path, stats: Option<&UploadQueueStats>) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "Failed to remove upload-queue temp file; leaked"
        );
        if let Some(s) = stats {
            s.leaked_temp_files.fetch_add(1, Ordering::Relaxed);
        }
    }
}
/// Delete the queue-owned temp file backing `source`. Both variants are
/// queue-owned (a working-tree source is snapshotted at enqueue, never enqueued
/// directly), so this always removes the file.
fn remove_owned_source(source: &UploadSource, stats: Option<&UploadQueueStats>) {
    try_remove_temp(source.path(), stats);
}
/// Delete a queue item's temp file and sidecar (if any) as a pair on every
/// terminal outcome, so a done item never leaves a `.meta.json` for the
/// restart-recovery scanner to re-process.
fn remove_item_files(item: &UploadQueueItem, stats: Option<&UploadQueueStats>) {
    remove_owned_source(&item.source, stats);
    if let Some(sidecar) = &item.sidecar_path {
        try_remove_temp(sidecar, stats);
    }
}
/// Shutdown state for the background worker, taken by `drain()`.
struct DrainState {
    shutdown_tx: oneshot::Sender<()>,
    worker_handle: tokio::task::JoinHandle<()>,
}
/// Handle for submitting artifacts to the background upload queue.
///
/// Clone-able — share across the agent struct and upload call sites.
/// The background worker is spawned once at creation time and runs until
/// the sender side is dropped (or `drain()` is called on shutdown).
#[derive(Clone)]
pub struct UploadQueue {
    tx: mpsc::Sender<UploadQueueItem>,
    queue_dir: PathBuf,
    resolver: Arc<dyn TraceExportSource>,
    stats: Arc<UploadQueueStats>,
    max_queue_bytes: u64,
    /// Grok client version string stamped on every `gcs_queue_upload` tracing span.
    /// Enables per-version breakdown of upload failures in analytics dashboards.
    pub client_version: Option<String>,
    drain_state: Arc<Mutex<Option<DrainState>>>,
    /// Byte-budget semaphore for inline-fallback uploads (disk budget exhausted /
    /// channel full); each upload acquires [`inline_fallback_permits`] for its
    /// size. Bounds memory + concurrency for the path-streaming variants, and
    /// concurrency only for the bytes variant (`spawn_inline_upload`).
    inline_fallback_semaphore: Arc<tokio::sync::Semaphore>,
    /// Destinations currently queued or uploading, so a duplicate enqueue is
    /// dropped before it spills a second copy to disk.
    uploads_in_flight: Arc<Mutex<HashSet<String>>>,
}
/// Marks one `gcs_path` as in flight; un-marks it from
/// [`UploadQueue::uploads_in_flight`] on drop.
struct InFlightGuard {
    gcs_path: String,
    in_flight: Arc<Mutex<HashSet<String>>>,
}
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut set = match self.in_flight.lock() {
            Ok(set) => set,
            Err(poisoned) => poisoned.into_inner(),
        };
        set.remove(&self.gcs_path);
    }
}
/// Only objects named by their content hash (`sha256_<hex>`) are safe to dedup on
/// path: a stable path with mutable content would drop a changed re-upload.
fn is_content_addressed(gcs_path: &str) -> bool {
    gcs_path
        .rsplit('/')
        .next()
        .is_some_and(|object| object.starts_with("sha256_"))
}
/// Marker error for [`UploadQueue::enqueue_blocking`] when the worker is shut
/// down (channel closed, or worker aborted before sending a completion).
/// Downcastable so callers can distinguish "queue unavailable" (retry another
/// way) from a genuine upload failure (already retried by the worker).
#[derive(Debug)]
pub struct QueueClosed;
impl std::fmt::Display for QueueClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upload queue worker is shut down")
    }
}
impl std::error::Error for QueueClosed {}
/// Structured outcome of [`UploadQueue::enqueue_bytes_blocking`].
///
/// Distinguishes the three terminal states of an enqueue attempt so callers
/// can report a truthful per-artifact status without inspecting queue
/// internals. The value is returned once the worker has accepted the item
/// (durably on disk) or a fallback / failure has been decided — it does NOT
/// reflect cloud-upload completion. Use [`UploadQueue::enqueue_blocking`] when
/// you need to await the upload itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// Bytes were written to `upload_queue/` as a `.tmp` file AND accepted by
    /// the background worker channel. The worker owns the cloud upload and its
    /// retry policy from here on.
    Enqueued,
    /// The disk budget was exceeded or the worker channel was full, so an
    /// inline fallback upload was spawned (bounded by the inline-fallback
    /// byte-budget semaphore). The bytes are not on the queue's disk spill but
    /// an upload is in flight.
    FellBackToInline,
    /// The temp file could not be written, or the worker is shut down. The
    /// artifact was not handed off anywhere; the caller should log and skip.
    Failed { reason: String },
    /// An identical `gcs_path` was already in flight, so this enqueue was skipped.
    Deduplicated,
}
/// Internal outcome of [`UploadQueue::enqueue_core`], the shared body behind
/// [`UploadQueue::enqueue`] and [`UploadQueue::enqueue_bytes_blocking`].
///
/// The core performs all the common bookkeeping (temp-file write, disk-budget
/// check, item construction, stats, `try_send`) and the inline fallback for the
/// over-budget / channel-full branches. The *closed-channel* branch is the one
/// place the two public methods diverge, so the core stops there and lets each
/// caller decide (`enqueue` inline-falls-back; `enqueue_bytes_blocking` reports
/// `Failed`).
enum EnqueueAttempt {
    /// The temp file could not be written; nothing was enqueued.
    WriteError(anyhow::Error),
    /// An identical `gcs_path` is already queued/uploading; nothing was written.
    Deduplicated,
    /// Item written and accepted by the worker channel.
    Sent,
    /// Over disk budget or channel full: temp removed / pending rolled back,
    /// `enqueue_fallbacks` bumped, and an inline fallback upload already spawned.
    InlineFallback,
    /// Worker channel is closed (shut down): temp removed and `pending` /
    /// `pending_bytes` rolled back, but NO fallback spawned and
    /// `enqueue_fallbacks` NOT bumped — the caller owns that decision.
    ChannelClosed,
}
impl UploadQueue {
    /// Create the queue, initialize the temp directory, and spawn the background worker.
    pub fn spawn(
        grok_home: &Path,
        resolver: Arc<dyn TraceExportSource>,
        retry_policy: UploadRetryPolicy,
    ) -> Self {
        Self::spawn_with_concurrency(grok_home, resolver, retry_policy, DEFAULT_MAX_CONCURRENT)
    }
    /// Create the queue with explicit concurrency limit for the background worker.
    pub fn spawn_with_concurrency(
        grok_home: &Path,
        resolver: Arc<dyn TraceExportSource>,
        mut retry_policy: UploadRetryPolicy,
        max_concurrent: usize,
    ) -> Self {
        let queue_dir = grok_home.join("upload_queue");
        if let Err(e) = std::fs::create_dir_all(&queue_dir) {
            tracing::warn!(error = %e, "Failed to create upload queue dir");
        }
        if let Some(raw_secs) = std::env::var("GROK_UPLOAD_QUEUE_AUTH_PROBE_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
        {
            match auth_park_probe_override(raw_secs) {
                Some(interval) => retry_policy.auth_park_probe_interval = interval,
                None => {
                    tracing::warn!(
                        "Ignoring GROK_UPLOAD_QUEUE_AUTH_PROBE_SECS={raw_secs}: a zero probe \
                     interval would re-attempt every parked upload on every wait slice. \
                     Keeping the {}s default.",
                        DEFAULT_AUTH_PARK_PROBE_INTERVAL.as_secs(),
                    )
                }
            }
        }
        let max_queue_bytes = std::env::var("GROK_UPLOAD_QUEUE_MAX_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_QUEUE_BYTES);
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let worker_resolver = resolver.clone();
        let worker_stats = stats.clone();
        let worker_handle = tokio::spawn(upload_worker(
            rx,
            shutdown_rx,
            worker_resolver,
            retry_policy,
            worker_stats,
            max_concurrent,
        ));
        let drain_state = Arc::new(Mutex::new(Some(DrainState {
            shutdown_tx,
            worker_handle,
        })));
        Self {
            tx,
            queue_dir,
            resolver,
            stats,
            max_queue_bytes,
            client_version: None,
            drain_state,
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }
    /// Mark `gcs_path` as in flight; the guard un-marks it on drop, or `None` if
    /// an identical upload is already in flight (skip it). Only the queued path is
    /// deduped; the inline fallback frees the guard on return.
    fn mark_in_flight(&self, gcs_path: &str) -> Option<InFlightGuard> {
        let mut set = match self.uploads_in_flight.lock() {
            Ok(set) => set,
            Err(poisoned) => poisoned.into_inner(),
        };
        if set.insert(gcs_path.to_string()) {
            Some(InFlightGuard {
                gcs_path: gcs_path.to_string(),
                in_flight: self.uploads_in_flight.clone(),
            })
        } else {
            self.stats.deduplicated.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                gcs_path,
                "upload queue: skipping duplicate in-flight upload"
            );
            None
        }
    }
    /// Set the grok client version to stamp on every `gcs_queue_upload` span.
    pub fn with_client_version(mut self, version: impl Into<String>) -> Self {
        self.client_version = Some(version.into());
        self
    }
    /// Override the temp-dir disk budget. Test seam to force the over-budget
    /// inline-fallback path without mutating the process-global env var.
    pub fn with_max_queue_bytes(mut self, max_bytes: u64) -> Self {
        self.max_queue_bytes = max_bytes;
        self
    }
    /// Enqueue bytes for background upload. Writes to temp file, returns immediately.
    ///
    /// Falls back to inline upload (current behavior) if the queue channel is full
    /// or the disk budget is exceeded.
    pub async fn enqueue(
        &self,
        content: &[u8],
        gcs_path: &str,
        content_type: &str,
        artifact_name: &str,
        session_id: &str,
        turn_number: u64,
    ) -> anyhow::Result<()> {
        match self.enqueue_core(
            content,
            gcs_path,
            content_type,
            artifact_name,
            session_id,
            turn_number,
            false,
        ) {
            EnqueueAttempt::WriteError(e) => Err(e),
            EnqueueAttempt::Sent
            | EnqueueAttempt::InlineFallback
            | EnqueueAttempt::Deduplicated => Ok(()),
            EnqueueAttempt::ChannelClosed => {
                tracing::debug!("Upload queue closed, falling back to inline upload");
                self.stats.enqueue_fallbacks.fetch_add(1, Ordering::Relaxed);
                self.spawn_inline_upload(content, gcs_path, content_type);
                Ok(())
            }
        }
    }
    /// Enqueue bytes for background upload, reporting a structured
    /// [`EnqueueOutcome`] instead of `Result<()>`.
    ///
    /// Mirrors [`Self::enqueue`] — same temp-file write, over-budget check and
    /// channel handling — but maps each terminal branch to a distinct
    /// [`EnqueueOutcome`] so callers can surface a truthful per-artifact
    /// status. Returns once the worker has accepted the item (durably on disk);
    /// it does NOT block on the cloud upload. Use [`Self::enqueue_blocking`] for
    /// the await-upload-completion contract.
    ///
    /// The one behavioural difference from [`Self::enqueue`]: a *closed* worker
    /// channel maps to [`EnqueueOutcome::Failed`] (no inline fallback) because a
    /// shut-down worker means the artifact is lost. A *full* channel still falls
    /// back to inline upload ([`EnqueueOutcome::FellBackToInline`]), exactly as
    /// [`Self::enqueue`] does.
    pub async fn enqueue_bytes_blocking(
        &self,
        content: &[u8],
        gcs_path: &str,
        content_type: &str,
        artifact_name: &str,
        session_id: &str,
        turn_number: u64,
    ) -> EnqueueOutcome {
        match self.enqueue_core(
            content,
            gcs_path,
            content_type,
            artifact_name,
            session_id,
            turn_number,
            true,
        ) {
            EnqueueAttempt::WriteError(e) => EnqueueOutcome::Failed {
                reason: e.to_string(),
            },
            EnqueueAttempt::Deduplicated => EnqueueOutcome::Deduplicated,
            EnqueueAttempt::Sent => EnqueueOutcome::Enqueued,
            EnqueueAttempt::InlineFallback => EnqueueOutcome::FellBackToInline,
            EnqueueAttempt::ChannelClosed => {
                tracing::debug!("Upload queue closed; enqueue_bytes_blocking reporting Failed");
                EnqueueOutcome::Failed {
                    reason: "upload queue worker is shut down".to_string(),
                }
            }
        }
    }
    /// Re-enqueue an existing on-disk pair (temp + sidecar) left by a prior
    /// process life, without rewriting either file. Used by startup recovery.
    ///
    /// Reusing the original pair keeps the sidecar's `enqueued_at` anchored to
    /// the first spill, so repeated restarts cannot slide the recovery max-age
    /// window indefinitely (a fresh pair per boot would reset the clock each
    /// time). The worker owns the pair from `Enqueued` onward and deletes both
    /// files on every terminal outcome, exactly as for a normal enqueue.
    ///
    /// On `Failed` (worker shut down, channel full, or over the disk budget)
    /// the pair is left untouched so a later startup can retry; no inline
    /// fallback is attempted — recovery runs pre-hub-connect where blocking on
    /// cloud I/O would delay registration.
    pub fn enqueue_recovered(
        &self,
        temp_path: &Path,
        sidecar_path: &Path,
        sidecar: &QueueItemSidecar,
    ) -> EnqueueOutcome {
        let size = file_size(temp_path);
        if self.over_disk_budget(size) {
            return EnqueueOutcome::Failed {
                reason: "over disk budget".to_string(),
            };
        }
        let item = UploadQueueItem {
            source: UploadSource::OwnedTemp(temp_path.to_path_buf()),
            gcs_path: sidecar.gcs_path.clone(),
            content_type: sidecar.content_type.clone(),
            artifact_name: sidecar.artifact_name.clone(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: Some(sidecar_path.to_path_buf()),
            completion_tx: None,
            client_version: self.client_version.clone(),
            compress: false,
            parent_span: tracing::Span::current(),
            _in_flight: None,
        };
        self.stats.pending.fetch_add(1, Ordering::Relaxed);
        self.stats.pending_bytes.fetch_add(size, Ordering::Relaxed);
        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        match self.tx.try_send(item) {
            Ok(()) => EnqueueOutcome::Enqueued,
            Err(e) => {
                self.stats.pending.fetch_sub(1, Ordering::Relaxed);
                self.stats.pending_bytes.fetch_sub(size, Ordering::Relaxed);
                self.stats.enqueued.fetch_sub(1, Ordering::Relaxed);
                self.stats.notify_transition();
                let reason = match e {
                    mpsc::error::TrySendError::Closed(_) => "upload queue worker is shut down",
                    mpsc::error::TrySendError::Full(_) => "upload queue channel full",
                };
                EnqueueOutcome::Failed {
                    reason: reason.to_string(),
                }
            }
        }
    }
    /// Shared body behind [`Self::enqueue`] and [`Self::enqueue_bytes_blocking`].
    ///
    /// Writes the temp file, checks the disk budget, builds the queue item, and
    /// `try_send`s it — performing all stats bookkeeping and the inline fallback
    /// for the over-budget / channel-full branches. The closed-channel branch is
    /// left to the caller (the two methods diverge only there), so its
    /// `enqueue_fallbacks`/inline decision is NOT taken here. See
    /// [`EnqueueAttempt`].
    ///
    /// When `write_sidecar` is true, a [`QueueItemSidecar`] is written next to
    /// the temp file — but only after the disk-budget gate passes, so the
    /// over-budget fallback never pays for a sidecar it would immediately
    /// delete. All cleanup branches remove temp and sidecar together,
    /// preserving the pair invariant.
    fn enqueue_core(
        &self,
        content: &[u8],
        gcs_path: &str,
        content_type: &str,
        artifact_name: &str,
        session_id: &str,
        turn_number: u64,
        write_sidecar: bool,
    ) -> EnqueueAttempt {
        let in_flight = if is_content_addressed(gcs_path) {
            match self.mark_in_flight(gcs_path) {
                Some(guard) => Some(guard),
                None => return EnqueueAttempt::Deduplicated,
            }
        } else {
            None
        };
        let temp_path = match self.write_temp_file(content, artifact_name, session_id, turn_number)
        {
            Ok(p) => p,
            Err(e) => return EnqueueAttempt::WriteError(e),
        };
        let size = content.len() as u64;
        if self.over_disk_budget(size) {
            try_remove_temp(&temp_path, Some(&self.stats));
            self.stats.enqueue_fallbacks.fetch_add(1, Ordering::Relaxed);
            self.spawn_inline_upload(content, gcs_path, content_type);
            return EnqueueAttempt::InlineFallback;
        }
        let sidecar_path = if write_sidecar {
            match self.write_sidecar_file(
                &temp_path,
                content,
                gcs_path,
                content_type,
                artifact_name,
                session_id,
                turn_number,
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    try_remove_temp(&temp_path, Some(&self.stats));
                    return EnqueueAttempt::WriteError(e);
                }
            }
        } else {
            None
        };
        let item = UploadQueueItem {
            source: UploadSource::OwnedTemp(temp_path),
            gcs_path: gcs_path.to_string(),
            content_type: content_type.to_string(),
            artifact_name: artifact_name.to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path,
            completion_tx: None,
            client_version: self.client_version.clone(),
            compress: false,
            parent_span: tracing::Span::current(),
            _in_flight: in_flight,
        };
        self.stats.pending.fetch_add(1, Ordering::Relaxed);
        self.stats.pending_bytes.fetch_add(size, Ordering::Relaxed);
        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        match self.tx.try_send(item) {
            Ok(()) => {
                self.stats.notify_transition();
                EnqueueAttempt::Sent
            }
            Err(e) => {
                let closed = matches!(&e, mpsc::error::TrySendError::Closed(_));
                let rejected = e.into_inner();
                remove_item_files(&rejected, Some(&self.stats));
                self.stats.pending.fetch_sub(1, Ordering::Relaxed);
                self.stats.pending_bytes.fetch_sub(size, Ordering::Relaxed);
                self.stats.enqueued.fetch_sub(1, Ordering::Relaxed);
                self.stats.notify_transition();
                if closed {
                    EnqueueAttempt::ChannelClosed
                } else {
                    self.stats.enqueue_fallbacks.fetch_add(1, Ordering::Relaxed);
                    self.spawn_inline_upload(content, gcs_path, content_type);
                    EnqueueAttempt::InlineFallback
                }
            }
        }
    }
    /// Enqueue bytes and block until upload completes. Returns the upload URL on success.
    ///
    /// Used for `block_for_upload` mode where the caller must await completion
    /// (e.g., metadata.json enrichment on the proxy). Writes the recovery
    /// sidecar like [`Self::enqueue_bytes_blocking`], so an item outliving the
    /// waiter (cancelled confirmation, process exit mid-retry) spills as a
    /// pair the next run re-enqueues.
    pub async fn enqueue_blocking(
        &self,
        content: &[u8],
        gcs_path: &str,
        content_type: &str,
        artifact_name: &str,
        session_id: &str,
        turn_number: u64,
    ) -> anyhow::Result<String> {
        let temp_path = self.write_temp_file(content, artifact_name, session_id, turn_number)?;
        let sidecar_path = match self.write_sidecar_file(
            &temp_path,
            content,
            gcs_path,
            content_type,
            artifact_name,
            session_id,
            turn_number,
        ) {
            Ok(p) => p,
            Err(e) => {
                try_remove_temp(&temp_path, Some(&self.stats));
                return Err(e);
            }
        };
        let size = content.len() as u64;
        let (tx, rx) = oneshot::channel();
        let item = UploadQueueItem {
            source: UploadSource::OwnedTemp(temp_path),
            gcs_path: gcs_path.to_string(),
            content_type: content_type.to_string(),
            artifact_name: artifact_name.to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: Some(sidecar_path),
            completion_tx: Some(tx),
            client_version: self.client_version.clone(),
            compress: false,
            parent_span: tracing::Span::current(),
            _in_flight: None,
        };
        self.stats.pending.fetch_add(1, Ordering::Relaxed);
        self.stats.pending_bytes.fetch_add(size, Ordering::Relaxed);
        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        match self.tx.try_send(item) {
            Ok(()) => self.stats.notify_transition(),
            Err(e) => {
                let closed = matches!(&e, mpsc::error::TrySendError::Closed(_));
                let rejected = e.into_inner();
                self.stats.pending.fetch_sub(1, Ordering::Relaxed);
                self.stats.pending_bytes.fetch_sub(size, Ordering::Relaxed);
                self.stats.enqueued.fetch_sub(1, Ordering::Relaxed);
                self.stats.notify_transition();
                if closed {
                    remove_item_files(&rejected, Some(&self.stats));
                    return Err(anyhow::Error::new(QueueClosed).context("upload queue closed"));
                }
                if let Some(sidecar) = &rejected.sidecar_path {
                    try_remove_temp(sidecar, Some(&self.stats));
                }
                self.stats.enqueue_fallbacks.fetch_add(1, Ordering::Relaxed);
                self.spawn_inline_upload_owned_snapshot(
                    rejected.source.path().to_path_buf(),
                    gcs_path.to_string(),
                    content_type.to_string(),
                    size,
                    rejected.completion_tx,
                );
            }
        }
        rx.await
            .map_err(|_| {
                anyhow::Error::new(QueueClosed).context("worker dropped completion channel")
            })?
            .map(|c| c.gcs_url)
    }
    /// Enqueue a file for background upload.
    ///
    /// Copies the source file to the queue directory (reflink on APFS/btrfs).
    pub async fn enqueue_file(
        &self,
        source_path: &Path,
        gcs_path: &str,
        content_type: &str,
        artifact_name: &str,
        session_id: &str,
        turn_number: u64,
    ) -> anyhow::Result<()> {
        let in_flight = if is_content_addressed(gcs_path) {
            match self.mark_in_flight(gcs_path) {
                Some(guard) => Some(guard),
                None => return Ok(()),
            }
        } else {
            None
        };
        let size = std::fs::metadata(source_path)
            .with_context(|| format!("Failed to stat {} for upload queue", source_path.display()))?
            .len();
        if self.over_disk_budget(size) {
            self.stats.enqueue_fallbacks.fetch_add(1, Ordering::Relaxed);
            self.spawn_inline_upload_from_path(
                source_path.to_path_buf(),
                gcs_path.to_string(),
                content_type.to_string(),
                size,
            );
            return Ok(());
        }
        let dest_name = temp_file_name(artifact_name, session_id, turn_number);
        let dest_path = self.queue_dir.join(dest_name);
        std::fs::copy(source_path, &dest_path)
            .with_context(|| format!("Failed to copy {} to queue", source_path.display()))?;
        let item = UploadQueueItem {
            source: UploadSource::OwnedTemp(dest_path),
            gcs_path: gcs_path.to_string(),
            content_type: content_type.to_string(),
            artifact_name: artifact_name.to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: None,
            completion_tx: None,
            client_version: self.client_version.clone(),
            compress: false,
            parent_span: tracing::Span::current(),
            _in_flight: in_flight,
        };
        self.stats.pending.fetch_add(1, Ordering::Relaxed);
        self.stats.pending_bytes.fetch_add(size, Ordering::Relaxed);
        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self.tx.try_send(item) {
            if matches!(&e, mpsc::error::TrySendError::Closed(_)) {
                tracing::debug!("Upload queue closed, falling back to inline upload");
            }
            let rejected = e.into_inner();
            remove_owned_source(&rejected.source, Some(&self.stats));
            self.stats.pending.fetch_sub(1, Ordering::Relaxed);
            self.stats.pending_bytes.fetch_sub(size, Ordering::Relaxed);
            self.stats.notify_transition();
            self.stats.enqueue_fallbacks.fetch_add(1, Ordering::Relaxed);
            self.spawn_inline_upload_from_path(
                source_path.to_path_buf(),
                gcs_path.to_string(),
                content_type.to_string(),
                size,
            );
            Ok(())
        } else {
            Ok(())
        }
    }
    /// Enqueue a file for upload, optionally zstd-compressed at upload time
    /// (only when `compress = true` and file >= 128 bytes). On budget-gate
    /// fallback the upload goes inline uncompressed regardless of `compress`.
    pub async fn enqueue_file_blocking(
        &self,
        source_path: &Path,
        gcs_path: &str,
        content_type: &str,
        artifact_name: &str,
        session_id: &str,
        turn_number: u64,
        compress: bool,
    ) -> anyhow::Result<EnqueueResult> {
        let source_size = file_size(source_path);
        let in_flight = if is_content_addressed(gcs_path) {
            match self.mark_in_flight(gcs_path) {
                Some(guard) => Some(guard),
                None => {
                    let (tx, rx) = oneshot::channel();
                    let _ = tx.send(Err(anyhow::anyhow!(
                        "deduplicated: identical gcs_path already in flight"
                    )));
                    return Ok(EnqueueResult {
                        completion_rx: rx,
                        original_size: source_size,
                    });
                }
            }
        } else {
            None
        };
        if self.over_disk_budget(source_size) {
            self.stats.enqueue_fallbacks.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.spawn_inline_upload_blocking(
                source_path.to_path_buf(),
                gcs_path.to_string(),
                content_type.to_string(),
                source_size,
                tx,
            );
            return Ok(EnqueueResult {
                completion_rx: rx,
                original_size: source_size,
            });
        }
        let dest_name = temp_file_name(artifact_name, session_id, turn_number);
        let dest_path = self.queue_dir.join(&dest_name);
        move_or_copy_to_queue(source_path, &dest_path, &self.queue_dir, &self.stats)?;
        let original_size = file_size(&dest_path);
        let (tx, rx) = oneshot::channel();
        let item = UploadQueueItem {
            source: UploadSource::OwnedTemp(dest_path),
            gcs_path: gcs_path.to_string(),
            content_type: content_type.to_string(),
            artifact_name: artifact_name.to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: None,
            completion_tx: Some(tx),
            client_version: self.client_version.clone(),
            compress,
            parent_span: tracing::Span::current(),
            _in_flight: in_flight,
        };
        self.stats.pending.fetch_add(1, Ordering::Relaxed);
        self.stats
            .pending_bytes
            .fetch_add(original_size, Ordering::Relaxed);
        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self.tx.send(item).await {
            let rejected = e.0;
            remove_owned_source(&rejected.source, Some(&self.stats));
            self.stats.pending.fetch_sub(1, Ordering::Relaxed);
            self.stats
                .pending_bytes
                .fetch_sub(original_size, Ordering::Relaxed);
            self.stats.notify_transition();
            return Err(anyhow::anyhow!("Upload queue closed"));
        }
        Ok(EnqueueResult {
            completion_rx: rx,
            original_size,
        })
    }
    /// Enqueue a working-tree file by taking an immutable reflink/CoW snapshot of
    /// it into the queue dir, verifying that snapshot against `expected_sha256`,
    /// then uploading the snapshot (never the live source).
    ///
    /// Snapshotting at enqueue closes the verify-then-upload corruption window:
    /// verify and upload operate on the SAME bytes, so a later mutation of the
    /// working-tree file cannot poison the content-addressed object.
    ///
    /// Reflink-vs-copy disk budgeting is handled at the `snapshot_route` gate
    /// below. A stale snapshot (source changed since the manifest hash) is
    /// discarded and the completion resolves to a non-fatal `Err`. Mirrors
    /// `enqueue_file`'s channel-full/closed fallback. Returns an [`EnqueueResult`].
    pub async fn enqueue_file_reference(
        &self,
        source_path: &Path,
        expected_sha256: &str,
        gcs_path: &str,
        content_type: &str,
        artifact_name: &str,
        session_id: &str,
        turn_number: u64,
    ) -> anyhow::Result<EnqueueResult> {
        let original_size = std::fs::metadata(source_path)
            .with_context(|| {
                format!(
                    "Failed to stat {} for upload queue snapshot",
                    source_path.display()
                )
            })?
            .len();
        let (tx, rx) = oneshot::channel();
        let in_flight = if is_content_addressed(gcs_path) {
            match self.mark_in_flight(gcs_path) {
                Some(guard) => Some(guard),
                None => {
                    let _ = tx.send(Err(anyhow::anyhow!(
                        "deduplicated: identical gcs_path already in flight"
                    )));
                    return Ok(EnqueueResult {
                        completion_rx: rx,
                        original_size,
                    });
                }
            }
        } else {
            None
        };
        let snapshot = self
            .queue_dir
            .join(temp_file_name(artifact_name, session_id, turn_number));
        let disk_bytes = match reflink_copy::reflink_or_copy(source_path, &snapshot) {
            Ok(copied) => copied.unwrap_or(0),
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "Failed to snapshot {} into upload queue",
                    source_path.display()
                )));
            }
        };
        match check_snapshot(&snapshot, expected_sha256) {
            SnapshotCheck::Match => {}
            SnapshotCheck::Stale => {
                try_remove_temp(&snapshot, Some(&self.stats));
                self.stats.reference_stale.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(Err(anyhow::anyhow!(
                    "reference snapshot did not match expected sha256; upload skipped"
                )));
                return Ok(EnqueueResult {
                    completion_rx: rx,
                    original_size,
                });
            }
            SnapshotCheck::Io(e) => {
                try_remove_temp(&snapshot, Some(&self.stats));
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(Err(e));
                return Ok(EnqueueResult {
                    completion_rx: rx,
                    original_size,
                });
            }
        }
        tracing::debug!(
            session_id,
            turn_number,
            gcs_path,
            size_bytes = original_size,
            disk_bytes,
            reflinked = disk_bytes == 0,
            "Enqueueing reference snapshot upload"
        );
        if snapshot_route(disk_bytes, self.over_disk_budget(disk_bytes))
            == SnapshotRoute::InlineFallback
        {
            self.stats.enqueue_fallbacks.fetch_add(1, Ordering::Relaxed);
            self.spawn_inline_upload_owned_snapshot(
                snapshot,
                gcs_path.to_string(),
                content_type.to_string(),
                original_size,
                Some(tx),
            );
            return Ok(EnqueueResult {
                completion_rx: rx,
                original_size,
            });
        }
        let item = UploadQueueItem {
            source: UploadSource::OwnedSnapshot {
                path: snapshot,
                disk_bytes,
            },
            gcs_path: gcs_path.to_string(),
            content_type: content_type.to_string(),
            artifact_name: artifact_name.to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: None,
            completion_tx: Some(tx),
            client_version: self.client_version.clone(),
            compress: false,
            parent_span: tracing::Span::current(),
            _in_flight: in_flight,
        };
        self.stats.pending.fetch_add(1, Ordering::Relaxed);
        self.stats
            .pending_bytes
            .fetch_add(disk_bytes, Ordering::Relaxed);
        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self.tx.try_send(item) {
            if matches!(&e, mpsc::error::TrySendError::Closed(_)) {
                tracing::debug!("Upload queue closed, falling back to inline snapshot upload");
            }
            let rejected = e.into_inner();
            self.stats.pending.fetch_sub(1, Ordering::Relaxed);
            self.stats
                .pending_bytes
                .fetch_sub(disk_bytes, Ordering::Relaxed);
            self.stats.notify_transition();
            self.stats.enqueue_fallbacks.fetch_add(1, Ordering::Relaxed);
            self.spawn_inline_upload_owned_snapshot(
                rejected.source.path().to_path_buf(),
                gcs_path.to_string(),
                content_type.to_string(),
                original_size,
                rejected.completion_tx,
            );
        }
        Ok(EnqueueResult {
            completion_rx: rx,
            original_size,
        })
    }
    /// Bounded, NON-terminal flush: wait until every queued item has reached a
    /// terminal outcome (`pending == 0`) or `timeout` elapses, and return the
    /// remaining pending count (0 = flushed). Unlike [`Self::drain`] the
    /// worker keeps running either way, so later enqueues proceed normally —
    /// this is the per-turn flush; `drain` is for process shutdown.
    ///
    /// `pending == 0` means every accepted item settled (uploaded, or dropped
    /// by retry/terminal policy); it does not cover inline-fallback tasks,
    /// which leave `pending` at spawn.
    pub async fn wait_idle(&self, timeout: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.stats.idle_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let pending = self.stats.pending.load(Ordering::Relaxed) as usize;
            if pending == 0 {
                return 0;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return pending;
            }
            let slice = deadline.min(now + Duration::from_millis(250));
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(slice) => {}
            }
        }
    }
    /// Drain remaining items with a deadline. Called on graceful shutdown.
    ///
    /// Signals the worker to stop accepting new items, process all remaining
    /// channel items, and wait for in-flight uploads to complete.
    /// Returns 0 on success, or the pending count if the deadline is exceeded.
    /// On timeout the worker task is aborted, which also aborts any still-running
    /// upload tasks (they live in the worker's `JoinSet`); their artifacts stay
    /// on disk for next-session orphan recovery.
    /// Double drain is a no-op (returns 0).
    pub async fn drain(&self, deadline: Duration) -> usize {
        let span = tracing::info_span!(
            "upload_queue.drain",
            deadline_secs = deadline.as_secs(),
            remaining = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        async {
            let current_span = tracing::Span::current();
            let state = self
                .drain_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let Some(state) = state else {
                current_span.record("outcome", "noop");
                current_span.record("remaining", 0usize);
                return 0;
            };
            let _ = state.shutdown_tx.send(());
            let handle = state.worker_handle;
            tokio::pin!(handle);
            match tokio::time::timeout(deadline, &mut handle).await {
                Ok(Ok(())) => {
                    current_span.record("outcome", "completed");
                    current_span.record("remaining", 0usize);
                    0
                }
                Ok(Err(e)) => {
                    let remaining = self.stats.pending.load(Ordering::Relaxed) as usize;
                    current_span.record("outcome", "panicked");
                    current_span.record("remaining", remaining);
                    tracing::warn!(error = %e, "Upload queue worker panicked during drain");
                    remaining
                }
                Err(_) => {
                    let remaining = self.stats.pending.load(Ordering::Relaxed) as usize;
                    current_span.record("outcome", "timed_out");
                    current_span.record("remaining", remaining);
                    tracing::debug!("Upload queue drain timed out");
                    handle.abort();
                    remaining
                }
            }
        }
        .instrument(span)
        .await
    }
    /// Current queue statistics.
    pub fn stats(&self) -> &UploadQueueStats {
        &self.stats
    }
    /// Get a shared reference to the stats Arc for cross-component sharing.
    ///
    /// Used to pass the stats to the feedback manager's periodic signal sync,
    /// which snapshots upload queue metrics into the session signals.
    pub fn stats_arc(&self) -> Arc<UploadQueueStats> {
        self.stats.clone()
    }
    /// Clean up orphaned entries from previous sessions.
    ///
    /// Called at startup to remove files and directories older than `max_age`
    /// that were left behind by crashes or ungraceful shutdowns. Deleted lone
    /// queue files (temp without sidecar, or vice versa) are counted in
    /// `cleanup_orphan_mismatched`.
    pub fn cleanup_orphans(&self, max_age: Duration) {
        cleanup_queue_dir(&self.queue_dir, max_age, Some(&self.stats));
    }
    fn write_temp_file(
        &self,
        content: &[u8],
        artifact_name: &str,
        session_id: &str,
        turn_number: u64,
    ) -> anyhow::Result<PathBuf> {
        let name = temp_file_name(artifact_name, session_id, turn_number);
        let path = self.queue_dir.join(name);
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write temp file {}", path.display()))?;
        Ok(path)
    }
    /// Write the [`QueueItemSidecar`] manifest for `temp_path` atomically
    /// (write `<final>.tmp` → fsync → rename). Only the manifest is written
    /// atomically — the temp file itself is a plain write; that asymmetry is
    /// fine because recovery re-hashes the temp bytes and drops the pair on a
    /// `sha256` mismatch, so a torn temp is detected rather than re-uploaded.
    fn write_sidecar_file(
        &self,
        temp_path: &Path,
        content: &[u8],
        gcs_path: &str,
        content_type: &str,
        artifact_name: &str,
        session_id: &str,
        turn_number: u64,
    ) -> anyhow::Result<PathBuf> {
        let sidecar = QueueItemSidecar {
            schema_version: QUEUE_ITEM_SIDECAR_SCHEMA_VERSION,
            session_id: session_id.to_string(),
            turn_number,
            gcs_path: gcs_path.to_string(),
            content_type: content_type.to_string(),
            artifact_name: artifact_name.to_string(),
            enqueued_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            sha256: crate::sha256_hex(content),
        };
        let json =
            serde_json::to_vec_pretty(&sidecar).context("serialize queue item sidecar manifest")?;
        let final_path = sidecar_path_for(temp_path);
        write_atomic(&final_path, &json)?;
        Ok(final_path)
    }
    fn over_disk_budget(&self, additional_bytes: u64) -> bool {
        self.stats.pending_bytes.load(Ordering::Relaxed) + additional_bytes > self.max_queue_bytes
    }
    /// Inline-upload fallback for `enqueue_file_blocking` when over the disk
    /// budget. Streams from `source_path` via `upload_file` and resolves the
    /// caller's `oneshot`. Always uncompressed. Streaming from disk means the
    /// byte semaphore bounds both resident memory and upload concurrency.
    fn spawn_inline_upload_blocking(
        &self,
        source_path: PathBuf,
        gcs_path: String,
        content_type: String,
        original_size: u64,
        completion_tx: oneshot::Sender<anyhow::Result<UploadCompletion>>,
    ) {
        use tracing::Instrument;
        let resolver = self.resolver.clone();
        let semaphore = self.inline_fallback_semaphore.clone();
        let permits = inline_fallback_permits(original_size);
        let parent_span = tracing::Span::current();
        tokio::spawn(
            async move {
                let _permit = semaphore
                    .acquire_many_owned(permits)
                    .await
                    .map_err(|e| {
                        tracing::warn!(error = %e, "inline-fallback semaphore closed; proceeding ungated")
                    })
                    .ok();
                let wrapped = ResolvedStorageConfig::from_resolver_async(&resolver)
                    .await;
                let result = match upload_file(
                        &wrapped,
                        &gcs_path,
                        &source_path,
                        &content_type,
                    )
                    .await
                {
                    Ok(url) => {
                        Ok(UploadCompletion {
                            gcs_url: url,
                            compression: BlobCompression::None,
                            original_size,
                            stored_size: original_size,
                        })
                    }
                    Err(e) => {
                        tracing::warn!(gcs_path, error = %e, "Inline blocking upload failed");
                        Err(e)
                    }
                };
                let _ = completion_tx.send(result);
            }
                .instrument(parent_span),
        );
    }
    /// Inline fallback for `enqueue_file_reference` when the channel is full /
    /// closed or an over-budget copy-fallback snapshot must not accumulate in the
    /// queue. Streams the queue-OWNED snapshot via `upload_file` (bounded by the
    /// byte-budget semaphore), resolves `completion_tx`, and ALWAYS deletes the
    /// snapshot afterward.
    fn spawn_inline_upload_owned_snapshot(
        &self,
        snapshot: PathBuf,
        gcs_path: String,
        content_type: String,
        original_size: u64,
        completion_tx: Option<oneshot::Sender<anyhow::Result<UploadCompletion>>>,
    ) {
        use tracing::Instrument;
        let resolver = self.resolver.clone();
        let semaphore = self.inline_fallback_semaphore.clone();
        let stats = self.stats.clone();
        let permits = inline_fallback_permits(original_size);
        let parent_span = tracing::Span::current();
        tokio::spawn(
            async move {
                let _permit = semaphore
                    .acquire_many_owned(permits)
                    .await
                    .map_err(|e| {
                        tracing::warn!(error = %e, "inline-fallback semaphore closed; proceeding ungated")
                    })
                    .ok();
                let wrapped = ResolvedStorageConfig::from_resolver_async(&resolver)
                    .await;
                let result = match upload_file(
                        &wrapped,
                        &gcs_path,
                        &snapshot,
                        &content_type,
                    )
                    .await
                {
                    Ok(url) => {
                        Ok(UploadCompletion {
                            gcs_url: url,
                            compression: BlobCompression::None,
                            original_size,
                            stored_size: original_size,
                        })
                    }
                    Err(e) => {
                        tracing::warn!(gcs_path, error = %e, "Inline snapshot fallback upload failed");
                        Err(e)
                    }
                };
                try_remove_temp(&snapshot, Some(&stats));
                if let Some(tx) = completion_tx {
                    let _ = tx.send(result);
                }
            }
                .instrument(parent_span),
        );
    }
    /// Fire-and-forget inline fallback for `enqueue_file` (over-budget /
    /// channel-full), streaming from `source_path` via `upload_file` (multipart
    /// for large files) rather than reading the file into memory. Streaming from
    /// disk means the byte semaphore bounds both resident memory and concurrency.
    fn spawn_inline_upload_from_path(
        &self,
        source_path: PathBuf,
        gcs_path: String,
        content_type: String,
        size: u64,
    ) {
        use tracing::Instrument;
        let resolver = self.resolver.clone();
        let semaphore = self.inline_fallback_semaphore.clone();
        let permits = inline_fallback_permits(size);
        let parent_span = tracing::Span::current();
        tokio::spawn(
            async move {
                let _permit = semaphore
                    .acquire_many_owned(permits)
                    .await
                    .map_err(|e| {
                        tracing::warn!(error = %e, "inline-fallback semaphore closed; proceeding ungated")
                    })
                    .ok();
                let wrapped = ResolvedStorageConfig::from_resolver_async(&resolver)
                    .await;
                if let Err(e) = upload_file(
                        &wrapped,
                        &gcs_path,
                        &source_path,
                        &content_type,
                    )
                    .await
                {
                    tracing::warn!(gcs_path, error = %e, "Inline fallback upload failed");
                }
            }
                .instrument(parent_span),
        );
    }
    /// Fire-and-forget inline fallback for the bytes-based `enqueue`
    /// (over-budget / channel-full). The owned `Vec` must be allocated before the
    /// spawn (the borrow can't cross it), so the semaphore bounds only upload
    /// concurrency, not memory — acceptable because this path carries only small
    /// in-memory artifacts; multi-GB files use the path-streaming variants above.
    fn spawn_inline_upload(&self, content: &[u8], gcs_path: &str, content_type: &str) {
        use tracing::Instrument;
        let resolver = self.resolver.clone();
        let semaphore = self.inline_fallback_semaphore.clone();
        let permits = inline_fallback_permits(content.len() as u64);
        let content = content.to_vec();
        let gcs_path = gcs_path.to_string();
        let content_type = content_type.to_string();
        let parent_span = tracing::Span::current();
        tokio::spawn(
            async move {
                let _permit = semaphore
                    .acquire_many_owned(permits)
                    .await
                    .map_err(|e| {
                        tracing::warn!(error = %e, "inline-fallback semaphore closed; proceeding ungated")
                    })
                    .ok();
                let wrapped = ResolvedStorageConfig::from_resolver_async(&resolver)
                    .await;
                if let Err(e) = upload_bytes(
                        &wrapped,
                        &gcs_path,
                        &content,
                        &content_type,
                    )
                    .await
                {
                    tracing::warn!(gcs_path, error = %e, "Inline fallback upload failed");
                }
            }
                .instrument(parent_span),
        );
    }
}
/// A worker concurrency slot paired with its semaphore so a parked item can
/// release the slot (parking does zero wire I/O) and re-acquire it before
/// resuming. Without release, `max_concurrent` parked items would pin every
/// slot for up to `max_age` — collapsing throughput and stalling drain, since
/// the dispatch loop blocks on `acquire_owned()` and stops polling the
/// shutdown signal.
struct ConcurrencyPermit {
    semaphore: Arc<tokio::sync::Semaphore>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl ConcurrencyPermit {
    /// Drop the held slot (no-op if already released).
    fn release(&mut self) {
        self.permit = None;
    }
    /// Re-acquire a slot, awaiting if all are currently taken (no-op if already
    /// held).
    async fn reacquire(&mut self) {
        if self.permit.is_none() {
            self.permit = Some(
                self.semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("semaphore closed unexpectedly"),
            );
        }
    }
}
/// Acquire a semaphore permit and spawn the upload task for a single queue item.
async fn dispatch_item(
    item: UploadQueueItem,
    semaphore: &Arc<tokio::sync::Semaphore>,
    resolver: &Arc<dyn TraceExportSource>,
    retry_policy: &UploadRetryPolicy,
    stats: &Arc<UploadQueueStats>,
    consecutive_failures: &Arc<AtomicU32>,
    draining: &Arc<std::sync::atomic::AtomicBool>,
    tasks: &mut tokio::task::JoinSet<()>,
) {
    let permit = semaphore
        .clone()
        .acquire_owned()
        .await
        .expect("semaphore closed unexpectedly");
    let concurrency = ConcurrencyPermit {
        semaphore: semaphore.clone(),
        permit: Some(permit),
    };
    let resolver = resolver.clone();
    let retry_policy = retry_policy.clone();
    let stats = stats.clone();
    let consecutive_failures = consecutive_failures.clone();
    let draining = draining.clone();
    let span = tracing::info_span!(
        parent: item.parent_span.clone(),
        "gcs_queue_upload",
        artifact = %item.artifact_name,
        gcs_path = %item.gcs_path,
        client_version = %item.client_version.as_deref().unwrap_or("unknown"),
    );
    tasks.spawn(
        async move {
            process_item(
                item,
                &resolver,
                &retry_policy,
                &stats,
                &consecutive_failures,
                &draining,
                Some(concurrency),
            )
            .await;
        }
        .instrument(span),
    );
}
/// Hold the circuit breaker open for one [`CIRCUIT_BREAKER_COOLDOWN`] period,
/// returning `true` if a shutdown interrupted it. Sets `circuit_breaker_active`
/// on entry and always clears it before returning (even on shutdown, so the
/// gauge never stays stuck `true` while draining).
async fn circuit_breaker_cooldown(
    stats: &Arc<UploadQueueStats>,
    mut shutdown_rx: Pin<&mut oneshot::Receiver<()>>,
) -> bool {
    stats.circuit_breaker_active.store(true, Ordering::Relaxed);
    stats.notify_transition();
    let interrupted = tokio::select! {
        _ = tokio::time::sleep(CIRCUIT_BREAKER_COOLDOWN) => false,
        _ = shutdown_rx.as_mut() => {
            tracing::debug!("upload_queue.shutdown_signal");
            true
        }
    };
    stats.circuit_breaker_active.store(false, Ordering::Relaxed);
    stats.notify_transition();
    interrupted
}
/// Concurrent background worker that processes the upload queue.
///
/// Dispatches up to `max_concurrent` items in parallel using a semaphore.
/// Each item is processed in its own spawned task with an independent retry loop.
/// The circuit breaker pauses the dispatch loop (preventing new tasks from starting)
/// while in-flight tasks continue to completion.
///
/// The worker exits when either:
/// - The channel is closed (all senders dropped)
/// - A shutdown signal is received via `shutdown_rx` (from `drain()`)
///
/// On shutdown signal, the worker closes the receiver, drains all remaining
/// buffered items (bypassing the circuit breaker), and waits for all in-flight
/// tasks to complete via semaphore.
async fn upload_worker(
    mut rx: mpsc::Receiver<UploadQueueItem>,
    shutdown_rx: oneshot::Receiver<()>,
    resolver: Arc<dyn TraceExportSource>,
    retry_policy: UploadRetryPolicy,
    stats: Arc<UploadQueueStats>,
    max_concurrent: usize,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let consecutive_failures = Arc::new(AtomicU32::new(0));
    let draining_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    tokio::pin!(shutdown_rx);
    let draining = loop {
        if consecutive_failures.load(Ordering::Relaxed) >= CIRCUIT_BREAKER_THRESHOLD {
            tracing::warn!(
                failures = consecutive_failures.load(Ordering::Relaxed),
                "Upload queue circuit breaker tripped, pausing dispatch"
            );
            stats.circuit_breaker_trips.fetch_add(1, Ordering::Relaxed);
            if circuit_breaker_cooldown(&stats, shutdown_rx.as_mut()).await {
                break true;
            }
            consecutive_failures.store(0, Ordering::Relaxed);
        }
        tokio::select! {
            item = rx.recv() => {
                match item {
                    Some(item) => {
                        dispatch_item(
                            item, &semaphore, &resolver, &retry_policy,
                            &stats, &consecutive_failures, &draining_flag, &mut tasks,
                        ).await;
                        // Reap finished tasks so the JoinSet doesn't grow
                        // unbounded over the worker's lifetime.
                        while tasks.try_join_next().is_some() {}
                    }
                    None => break false,
                }
            }
            _ = &mut shutdown_rx => {
                tracing::debug!("upload_queue.shutdown_signal");
                break true;
            }
        }
    };
    draining_flag.store(true, Ordering::Relaxed);
    if draining {
        rx.close();
        while let Some(item) = rx.recv().await {
            dispatch_item(
                item,
                &semaphore,
                &resolver,
                &retry_policy,
                &stats,
                &consecutive_failures,
                &draining_flag,
                &mut tasks,
            )
            .await;
        }
    }
    while tasks.join_next().await.is_some() {}
    tracing::debug!("Upload queue worker exiting (all tasks drained)");
}
/// Minimum file size to attempt compression.
const COMPRESS_MIN_BYTES: u64 = 128;
/// Wraps an `AsyncRead` and counts total bytes read through it.
struct CountingReader<R> {
    inner: R,
    bytes_read: Arc<AtomicU64>,
}
impl<R: AsyncRead + Unpin> AsyncRead for CountingReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let n = buf.filled().len() - before;
            this.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
        }
        result
    }
}
/// Outcome of verifying a freshly-taken snapshot against `expected_sha256`.
enum SnapshotCheck {
    /// Content matches — safe to upload.
    Match,
    /// Hash mismatch or the snapshot vanished (NotFound): the source changed
    /// between the manifest hash and enqueue. Skip as stale.
    Stale,
    /// A transient read error while hashing our own fresh snapshot — a hard
    /// (non-stale) failure; must NOT be attributed to `reference_stale`.
    Io(anyhow::Error),
}
/// Where a verified reference snapshot should go.
#[derive(Debug, PartialEq, Eq)]
enum SnapshotRoute {
    /// Enqueue normally (a reflink, or a copy that fits the disk budget).
    Queue,
    /// Over-budget real copy — upload inline (bounded) instead of letting it
    /// accumulate in the queue.
    InlineFallback,
}
/// Reflink snapshots (`disk_bytes == 0`, ~0 real disk) always queue; only a real
/// copy that would exceed the budget routes to the bounded inline fallback.
fn snapshot_route(disk_bytes: u64, over_budget: bool) -> SnapshotRoute {
    if disk_bytes > 0 && over_budget {
        SnapshotRoute::InlineFallback
    } else {
        SnapshotRoute::Queue
    }
}
/// Verify the (immutable) snapshot at `path`. Streamed in 8 KB chunks via the
/// shared `sha256_hex_from_file` — never a whole-file read, so multi-GB
/// snapshots stay off the heap. Distinguishes a genuine mismatch/missing
/// (→ `Stale`) from a transient read error (→ `Io`).
fn check_snapshot(path: &Path, expected_sha256: &str) -> SnapshotCheck {
    match crate::sha256_hex_from_file(path, None) {
        Ok(actual) if actual == expected_sha256 => SnapshotCheck::Match,
        Ok(_) => SnapshotCheck::Stale,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SnapshotCheck::Stale,
        Err(e) => SnapshotCheck::Io(
            anyhow::Error::new(e).context(format!("Failed to hash snapshot {}", path.display())),
        ),
    }
}
/// Settle an item leaving the queue: drop `inflight` FIRST (so it never
/// exceeds `pending`), then `pending`/`pending_bytes`, then notify.
fn settle_pending(stats: &UploadQueueStats, accounted_bytes: u64) {
    stats.inflight.fetch_sub(1, Ordering::Relaxed);
    stats.pending.fetch_sub(1, Ordering::Relaxed);
    stats
        .pending_bytes
        .fetch_sub(accounted_bytes, Ordering::Relaxed);
    stats.notify_transition();
}
/// Process a single upload queue item: age check, upload with retries, optional streaming compression.
async fn process_item(
    mut item: UploadQueueItem,
    resolver: &Arc<dyn TraceExportSource>,
    retry_policy: &UploadRetryPolicy,
    stats: &Arc<UploadQueueStats>,
    consecutive_failures: &Arc<AtomicU32>,
    draining: &Arc<std::sync::atomic::AtomicBool>,
    mut permit: Option<ConcurrencyPermit>,
) {
    let size = file_size(item.source.path());
    let accounted_bytes = item.source.disk_bytes(size);
    stats.inflight.fetch_add(1, Ordering::Relaxed);
    stats.notify_transition();
    if item.enqueued_at.elapsed() > retry_policy.max_age {
        tracing::warn!(
            age_secs = item.enqueued_at.elapsed().as_secs(),
            outcome = "expired",
            "Dropping expired upload queue item"
        );
        remove_item_files(&item, Some(stats));
        stats.failed.fetch_add(1, Ordering::Relaxed);
        settle_pending(stats, accounted_bytes);
        notify_completion(&mut item, Err(anyhow::anyhow!("expired")));
        return;
    }
    let result = upload_with_retries(
        &mut item,
        resolver,
        retry_policy,
        size,
        stats,
        draining,
        permit.as_mut(),
    )
    .await;
    match result {
        Ok((url, compression, stored_size)) => {
            let compressed = matches!(compression, BlobCompression::Zstd);
            tracing::info!(
                attempts = item.attempts,
                size_bytes = size,
                compressed,
                outcome = "success",
                "GCS queue upload completed"
            );
            consecutive_failures.store(0, Ordering::Relaxed);
            remove_item_files(&item, Some(stats));
            stats.uploaded.fetch_add(1, Ordering::Relaxed);
            notify_completion(
                &mut item,
                Ok(UploadCompletion {
                    gcs_url: url,
                    compression,
                    original_size: size,
                    stored_size,
                }),
            );
        }
        Err(e) => {
            let terminal = matches!(upload_disposition(&e), Disposition::Terminal);
            if !terminal {
                consecutive_failures.fetch_add(1, Ordering::Relaxed);
            }
            tracing::warn!(
                attempts = item.attempts,
                size_bytes = size,
                outcome = if terminal { "dropped" } else { "exhausted" },
                error = ?e,
                "Upload queue item failed permanently"
            );
            remove_item_files(&item, Some(stats));
            stats.failed.fetch_add(1, Ordering::Relaxed);
            notify_completion(&mut item, Err(e));
        }
    }
    settle_pending(stats, accounted_bytes);
}
/// Shared status-code classifier for the storage upload queue.
const STORAGE_RETRY_POLICY: RetryPolicy = RetryPolicy::client_storage();
/// Returns `true` if the error indicates an HTTP 401 or 403 response.
///
/// These auth errors will never succeed with the same request — retrying
/// wastes time and generates log noise. This is the direct-mode (`gcloud-storage`)
/// string fallback: direct-mode errors are unstructured anyhow messages, so we
/// scrape for 401/403. Proxy-mode errors carry a structured `HttpUploadError`
/// and are classified by status code in `upload_disposition`.
fn is_non_retryable_error(error: &anyhow::Error) -> bool {
    let msg = format!("{:#}", error);
    msg.contains("HTTP 401")
        || msg.contains("HTTP 403")
        || msg.contains("401 Unauthorized")
        || msg.contains("403 Forbidden")
}
/// Disposition for a failed storage upload. Proxy-mode errors carry a
/// structured `HttpUploadError` and are classified by the shared
/// `RetryPolicy`; direct-mode (gcloud) errors are unstructured strings, so
/// 401/403 are detected by message scraping as a safety net.
fn upload_disposition(error: &anyhow::Error) -> Disposition {
    if let Some(http) = error.downcast_ref::<HttpUploadError>() {
        return STORAGE_RETRY_POLICY
            .classify(http.status_code)
            .unwrap_or(Disposition::Retryable);
    }
    if is_non_retryable_error(error) {
        return Disposition::AuthRefresh;
    }
    Disposition::Retryable
}
/// Park-loop iteration granularity: bounds how long a parked task takes to
/// notice `draining` / `max_age`.
const AUTH_PARK_WAIT_INTERVAL: Duration = Duration::from_secs(5);
/// Upload with retries, exponential backoff, and credential refresh.
///
/// On each attempt, resolves fresh credentials from the resolver and uploads the
/// queue-owned temp/snapshot via `upload_file` (which streams from disk on every
/// backend and keeps the multipart / signed-URL path for large files), or, for
/// compressible owned temps, streams through a zstd encoder. Snapshots are
/// immutable and already verified at enqueue, so the worker just uploads them.
///
/// On 400/403/404, aborts immediately (terminal — malformed path, ZDR / opt-out
/// rejection, or not-owned session). On 401, re-resolves credentials and
/// retries once; if the retry also 401s, the item parks until auth recovers
/// (releasing its concurrency permit while parked) rather than dropping.
async fn upload_with_retries(
    item: &mut UploadQueueItem,
    resolver: &Arc<dyn TraceExportSource>,
    policy: &UploadRetryPolicy,
    original_size: u64,
    stats: &Arc<UploadQueueStats>,
    draining: &Arc<std::sync::atomic::AtomicBool>,
    mut permit: Option<&mut ConcurrencyPermit>,
) -> anyhow::Result<(String, BlobCompression, u64)> {
    let should_compress = item.compress && original_size >= COMPRESS_MIN_BYTES;
    let mut auth_retried = false;
    let mut parked = false;
    loop {
        item.attempts += 1;
        let wrapped = ResolvedStorageConfig::from_resolver_async(resolver).await;
        let last_wire_attempt = Instant::now();
        let attempt_bearer = wrapped.wire_bearer();
        let result = if should_compress {
            stream_compress_upload(&wrapped, &item.gcs_path, item.source.path()).await
        } else {
            upload_file(
                &wrapped,
                &item.gcs_path,
                item.source.path(),
                &item.content_type,
            )
            .await
            .map(|url| (url, BlobCompression::None, original_size))
        };
        match result {
            Ok(r) => {
                tracing::debug!(attempt = item.attempts, "Upload queue item succeeded");
                return Ok(r);
            }
            Err(e) => match upload_disposition(&e) {
                Disposition::Terminal => {
                    tracing::warn!(
                        attempt = item.attempts,
                        error = ?e,
                        "Storage upload failed with a terminal client error (400/403/404); dropping artifact"
                    );
                    return Err(e);
                }
                Disposition::AuthRefresh => {
                    if !auth_retried {
                        tracing::info!(
                            attempt = item.attempts,
                            error = ?e,
                            "Auth error, re-resolving credentials for one retry"
                        );
                        auth_retried = true;
                        continue;
                    }
                    let failed_bearer = attempt_bearer;
                    if let Some(p) = permit.as_deref_mut() {
                        p.release();
                    }
                    let mut wake = false;
                    loop {
                        if draining.load(Ordering::Relaxed) {
                            tracing::warn!(
                                attempt = item.attempts,
                                parked,
                                "Auth error persists and queue is draining, aborting"
                            );
                            return Err(e);
                        }
                        if wake {
                            if item.enqueued_at.elapsed() >= policy.max_age {
                                tracing::warn!(
                                    attempt = item.attempts,
                                    age_secs = item.enqueued_at.elapsed().as_secs(),
                                    "Parked item exceeded max_age waiting for auth recovery, aborting"
                                );
                                return Err(e);
                            }
                            break;
                        }
                        let Some(wait) = resolver.wait_for_auth_recovery(
                            failed_bearer.as_deref(),
                            AUTH_PARK_WAIT_INTERVAL,
                        ) else {
                            tracing::warn!(
                                attempt = item.attempts,
                                parked,
                                error = ?e,
                                "Auth error persists after credential refresh, aborting"
                            );
                            return Err(e);
                        };
                        if !parked {
                            parked = true;
                            stats.auth_parked.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                attempt = item.attempts,
                                gcs_path = %item.gcs_path,
                                "401 persists after credential refresh; parking item until auth recovers"
                            );
                            notify_completion(
                                item,
                                Err(anyhow::anyhow!(
                                    "upload parked: credentials rejected (HTTP 401); \
                                     retrying in background until auth recovers"
                                )),
                            );
                        }
                        if item.enqueued_at.elapsed() >= policy.max_age {
                            tracing::warn!(
                                attempt = item.attempts,
                                age_secs = item.enqueued_at.elapsed().as_secs(),
                                "Parked item exceeded max_age waiting for auth recovery, aborting"
                            );
                            return Err(e);
                        }
                        wake = wait.await
                            || (last_wire_attempt.elapsed() >= policy.auth_park_probe_interval
                                && resolver.has_usable_credential());
                    }
                    if let Some(p) = permit.as_deref_mut() {
                        p.reacquire().await;
                    }
                    auth_retried = false;
                    continue;
                }
                Disposition::Retryable => {
                    if item.attempts >= policy.max_attempts {
                        return Err(e);
                    }
                    let delay = policy.backoff_delay(item.attempts - 1);
                    tracing::debug!(
                        attempt = item.attempts,
                        delay_ms = delay.as_millis() as u64,
                        error = ?e,
                        "Upload queue item failed, retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            },
        }
    }
}
/// Open file, wrap in streaming zstd encoder with byte counter, upload to cloud storage.
async fn stream_compress_upload<C: StorageConfig>(
    config: &C,
    gcs_path: &str,
    file_path: &Path,
) -> anyhow::Result<(String, BlobCompression, u64)> {
    let file = tokio::fs::File::open(file_path)
        .await
        .with_context(|| format!("Failed to open {} for compression", file_path.display()))?;
    let reader = tokio::io::BufReader::new(file);
    let encoder = ZstdEncoder::new(reader);
    let bytes_written = Arc::new(AtomicU64::new(0));
    let counting = CountingReader {
        inner: encoder,
        bytes_read: bytes_written.clone(),
    };
    let url = upload_stream(config, gcs_path, counting, "application/zstd").await?;
    Ok((
        url,
        BlobCompression::Zstd,
        bytes_written.load(Ordering::Relaxed),
    ))
}
/// Send completion signal if a block_for_upload caller is waiting.
fn notify_completion(item: &mut UploadQueueItem, result: anyhow::Result<UploadCompletion>) {
    if let Some(tx) = item.completion_tx.take() {
        let _ = tx.send(result);
    }
}
/// Generate a unique temp file name for a queued artifact.
///
/// Includes a random suffix to avoid collisions when multiple blobs with the
/// same SHA256 prefix are enqueued within the same millisecond.
fn temp_file_name(artifact_name: &str, session_id: &str, turn_number: u64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let short_id = if session_id.len() > 8 {
        &session_id[session_id.len() - 8..]
    } else {
        session_id
    };
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}_turn{}_{}_{}_{}",
        short_id, turn_number, artifact_name, ts, seq
    )
}
/// Filename suffix of a [`QueueItemSidecar`] manifest (`<temp>.meta.json`).
pub const SIDECAR_SUFFIX: &str = ".meta.json";
/// Sidecar manifest path for a queue temp file: `<temp>.meta.json`. The suffix
/// is appended (not an extension swap) because temp file names already contain
/// dots that `Path::with_extension` would mangle.
pub fn sidecar_path_for(temp_path: &Path) -> PathBuf {
    let mut name = temp_path.as_os_str().to_owned();
    name.push(SIDECAR_SUFFIX);
    PathBuf::from(name)
}
/// Inverse of [`sidecar_path_for`]: the temp file a sidecar describes, or
/// `None` if `sidecar` does not carry the [`SIDECAR_SUFFIX`].
pub fn temp_path_for_sidecar(sidecar: &Path) -> Option<PathBuf> {
    let name = sidecar.file_name()?.to_str()?;
    let stem = name.strip_suffix(SIDECAR_SUFFIX)?;
    Some(sidecar.with_file_name(stem))
}
/// Write `bytes` to `path` atomically: write to `<path>.tmp`, fsync, then
/// rename over `path`. A crash mid-write leaves at most a `<path>.tmp` partial
/// (swept by the orphan janitor), never a torn `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    {
        let mut file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("Failed to create {}", tmp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to fsync {}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to rename {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}
/// Get file size, returning 0 if the file doesn't exist.
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
fn copy_to_queue(source: &Path, dest: &Path) -> anyhow::Result<()> {
    std::fs::copy(source, dest)
        .with_context(|| format!("Failed to copy {} to queue", source.display()))?;
    Ok(())
}
/// Cheap rename if both paths are in `same_dir_hint`; else copy. On rename
/// failure in the same-dir case, copies then removes source via `try_remove_temp`.
fn move_or_copy_to_queue(
    source: &Path,
    dest: &Path,
    same_dir_hint: &Path,
    stats: &UploadQueueStats,
) -> anyhow::Result<()> {
    move_or_copy_to_queue_with(
        source,
        dest,
        same_dir_hint,
        stats,
        |s, d| std::fs::rename(s, d),
        copy_to_queue,
    )
}
/// Test harness for `move_or_copy_to_queue` with injectable rename/copy fns.
fn move_or_copy_to_queue_with(
    source: &Path,
    dest: &Path,
    same_dir_hint: &Path,
    stats: &UploadQueueStats,
    rename_fn: impl Fn(&Path, &Path) -> std::io::Result<()>,
    copy_fn: impl Fn(&Path, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if source.parent() == Some(same_dir_hint) {
        match rename_fn(source, dest) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    source = %source.display(),
                    error = %e,
                    "rename within queue_dir failed; falling back to copy + remove"
                );
                copy_fn(source, dest)?;
                try_remove_temp(source, Some(stats));
                return Ok(());
            }
        }
    }
    copy_fn(source, dest)
}
static LAST_ORPHANS_CLEANED: AtomicU64 = AtomicU64::new(0);
/// Number of orphaned entries cleaned by the last `cleanup_orphaned_uploads` call.
pub fn last_orphans_cleaned() -> u64 {
    LAST_ORPHANS_CLEANED.load(Ordering::Relaxed)
}
/// Clean up orphaned upload queue entries from previous sessions.
///
/// Called at agent startup to remove files and directories older than `max_age`
/// that were left behind by crashes or ungraceful shutdowns. Returns the number
/// of entries removed.
pub fn cleanup_orphaned_uploads(grok_home: &Path, max_age: Duration) -> u64 {
    let cleaned = cleanup_queue_dir(&grok_home.join("upload_queue"), max_age, None);
    LAST_ORPHANS_CLEANED.store(cleaned, Ordering::Relaxed);
    cleaned
}
/// Sweep entries older than `max_age`. `scratch/` is treated specially:
/// recurse one level so per-session subdirs are aged independently (its own
/// mtime stays fresh as new sessions land). `scratch/` itself is preserved.
///
/// When `stats` is `Some`, each deleted lone queue file (temp without sidecar
/// or vice versa) bumps `cleanup_orphan_mismatched`. Pairing is decided against
/// a name snapshot taken before any deletion, so the count is independent of
/// visit order.
fn cleanup_queue_dir(queue_dir: &Path, max_age: Duration, stats: Option<&UploadQueueStats>) -> u64 {
    let entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(queue_dir) {
        Ok(e) => e.flatten().collect(),
        Err(_) => return 0,
    };
    let all_names: HashSet<std::ffi::OsString> = entries.iter().map(|e| e.file_name()).collect();
    let mut cleaned = 0u64;
    let mut cleaned_bytes = 0u64;
    for entry in &entries {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name();
        let is_scratch_root = metadata.is_dir() && name == "scratch";
        if is_scratch_root {
            let (sub_cleaned, sub_bytes) = cleanup_scratch_subdirs(&path, max_age);
            cleaned += sub_cleaned;
            cleaned_bytes += sub_bytes;
            continue;
        }
        let age = pair_age(&path, &name, &all_names).unwrap_or_else(|| {
            metadata
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .unwrap_or(Duration::MAX)
        });
        if age <= max_age {
            continue;
        }
        if metadata.is_dir() {
            let size = dir_size(&path).unwrap_or(0);
            if std::fs::remove_dir_all(&path).is_ok() {
                cleaned += 1;
                cleaned_bytes += size;
            }
        } else if std::fs::remove_file(&path).is_ok() {
            cleaned += 1;
            cleaned_bytes += metadata.len();
            if let Some(stats) = stats
                && is_mismatched_queue_file(&name, &all_names)
            {
                stats
                    .cleanup_orphan_mismatched
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if cleaned > 0 {
        tracing::info!(
            cleaned,
            cleaned_bytes,
            dir = %queue_dir.display(),
            "Cleaned up orphaned upload queue entries from previous session"
        );
    }
    cleaned
}
/// True when `name` is a queue file whose temp↔sidecar partner is absent from
/// `all_names`.
/// Age of a queue file derived from its (or its companion's) sidecar
/// `enqueued_at`, or `None` when the file has no parseable sidecar — the
/// caller then falls back to mtime. Future-dated timestamps (clock skew) map
/// to `Duration::ZERO` so skew never expires live data.
fn pair_age(
    path: &Path,
    name: &std::ffi::OsStr,
    all_names: &HashSet<std::ffi::OsString>,
) -> Option<Duration> {
    let name_str = name.to_string_lossy();
    let sidecar_path = if name_str.ends_with(SIDECAR_SUFFIX) {
        path.to_path_buf()
    } else {
        let companion = format!("{name_str}{SIDECAR_SUFFIX}");
        if !all_names.contains(std::ffi::OsStr::new(companion.as_str())) {
            return None;
        }
        sidecar_path_for(path)
    };
    let bytes = std::fs::read(&sidecar_path).ok()?;
    let sidecar: QueueItemSidecar = serde_json::from_slice(&bytes).ok()?;
    let dt = chrono::DateTime::parse_from_rfc3339(&sidecar.enqueued_at).ok()?;
    let enqueued: std::time::SystemTime = dt.with_timezone(&chrono::Utc).into();
    Some(
        std::time::SystemTime::now()
            .duration_since(enqueued)
            .unwrap_or(Duration::ZERO),
    )
}
fn is_mismatched_queue_file(
    name: &std::ffi::OsStr,
    all_names: &HashSet<std::ffi::OsString>,
) -> bool {
    let name_str = name.to_string_lossy();
    if let Some(stem) = name_str.strip_suffix(SIDECAR_SUFFIX) {
        !all_names.contains(std::ffi::OsStr::new(stem))
    } else {
        let sidecar = format!("{name_str}{SIDECAR_SUFFIX}");
        !all_names.contains(std::ffi::OsStr::new(sidecar.as_str()))
    }
}
/// Reap `scratch/<sid>/` subdirs older than `max_age`. Returns
/// `(removed_count, removed_bytes)`.
///
/// Assumes `scratch/<sid>/` is flat: a nested layer would mask in-use
/// directories from the mtime check. Generalise to recursive probing when
/// that assumption changes.
fn cleanup_scratch_subdirs(scratch_dir: &Path, max_age: Duration) -> (u64, u64) {
    let entries = match std::fs::read_dir(scratch_dir) {
        Ok(e) => e,
        Err(_) => return (0, 0),
    };
    let mut cleaned = 0u64;
    let mut cleaned_bytes = 0u64;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let age = metadata
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .unwrap_or(Duration::MAX);
        if age <= max_age {
            continue;
        }
        let path = entry.path();
        if metadata.is_dir() {
            let size = dir_size(&path).unwrap_or(0);
            if std::fs::remove_dir_all(&path).is_ok() {
                cleaned += 1;
                cleaned_bytes += size;
            }
        } else if std::fs::remove_file(&path).is_ok() {
            cleaned += 1;
            cleaned_bytes += metadata.len();
        }
    }
    (cleaned, cleaned_bytes)
}
/// Recursively compute the total size of a directory tree.
fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::UploadMethod;
    /// Mock credential resolver for tests.
    struct MockResolver;
    impl TraceExportSource for MockResolver {
        fn resolve(&self) -> TraceExportConfig {
            TraceExportConfig {
                bucket_url: Some("gs://test-bucket".to_string()),
                service_account_key: None,
                prefix_dir: None,
                gcs_prefix: None,
                absolute_paths: false,
                archive_name_override: None,
                upload_method: UploadMethod::Direct {
                    service_account_key: None,
                },
            }
        }
    }
    /// Test wrapper for [`upload_with_retries`] supplying fresh stats, a
    /// never-draining flag, and no concurrency permit (these tests don't
    /// exercise the worker semaphore).
    async fn run_upload_with_retries(
        item: &mut UploadQueueItem,
        resolver: &Arc<dyn TraceExportSource>,
        policy: &UploadRetryPolicy,
    ) -> anyhow::Result<(String, BlobCompression, u64)> {
        upload_with_retries(
            item,
            resolver,
            policy,
            100,
            &Arc::new(UploadQueueStats::new()),
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await
    }
    #[tokio::test]
    async fn transition_notify_wakes_wired_listener() {
        let stats = Arc::new(UploadQueueStats::new());
        let notify = Arc::new(Notify::new());
        stats.set_transition_notify(notify.clone());
        let waiter = {
            let n = notify.clone();
            tokio::spawn(async move { n.notified().await })
        };
        tokio::task::yield_now().await;
        stats.notify_transition();
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("listener must wake on a queue transition")
            .expect("waiter task should not panic");
    }
    /// A shutdown that interrupts the breaker cooldown must not leave
    /// `circuit_breaker_active` stuck `true`.
    #[tokio::test]
    async fn circuit_breaker_cooldown_clears_active_flag_on_shutdown() {
        let stats = Arc::new(UploadQueueStats::new());
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown_tx.send(()).unwrap();
        tokio::pin!(shutdown_rx);
        let interrupted = circuit_breaker_cooldown(&stats, shutdown_rx.as_mut()).await;
        assert!(
            interrupted,
            "a delivered shutdown must interrupt the cooldown"
        );
        assert!(
            !stats.circuit_breaker_active.load(Ordering::Relaxed),
            "the live breaker gauge must be cleared when shutdown interrupts an active breaker"
        );
    }
    /// Unwired stats treat the transition ping as a no-op; set is once-only.
    #[test]
    fn transition_notify_without_listener_is_noop() {
        let stats = UploadQueueStats::new();
        stats.notify_transition();
        let first = Arc::new(Notify::new());
        let second = Arc::new(Notify::new());
        stats.set_transition_notify(first.clone());
        stats.set_transition_notify(second);
        assert!(
            stats.transition_notify.get().is_some(),
            "a notifier must be installed after the first set"
        );
    }
    /// The per-turn flush contract: empty queue returns immediately, a missed
    /// deadline reports (never aborts) the remaining count, and a settle wakes
    /// the waiter — all without touching the worker.
    #[tokio::test]
    async fn wait_idle_reports_pending_and_wakes_on_settle() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir,
            resolver: Arc::new(MockResolver),
            stats: stats.clone(),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        assert_eq!(
            queue.wait_idle(Duration::from_millis(10)).await,
            0,
            "empty queue is already idle"
        );
        stats.pending.fetch_add(2, Ordering::Relaxed);
        assert_eq!(
            queue.wait_idle(Duration::from_millis(50)).await,
            2,
            "deadline reports the remaining count"
        );
        let settle_stats = stats.clone();
        let settle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            settle_stats.pending.store(0, Ordering::Relaxed);
            settle_stats.notify_transition();
        });
        assert_eq!(
            queue.wait_idle(Duration::from_secs(5)).await,
            0,
            "a settle wakes the waiter before the deadline"
        );
        settle.await.unwrap();
    }
    /// A blocking enqueue spills as a temp + sidecar pair before any await,
    /// so an item outliving its waiter (cancelled confirmation, process exit)
    /// is exactly what `run_startup_recovery` re-enqueues next run.
    #[tokio::test]
    async fn blocking_enqueue_spills_recoverable_sidecar_pair() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(1);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats,
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        let content = b"session-state-bytes";
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            queue.enqueue_blocking(
                content,
                "sess-1234/turn_7/tool_state.json",
                "application/gzip",
                "session_state",
                "sess-1234",
                7,
            ),
        )
        .await;
        let sidecars: Vec<_> = std::fs::read_dir(&queue_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(SIDECAR_SUFFIX))
            .collect();
        assert_eq!(sidecars.len(), 1, "one sidecar spilled");
        let sidecar: QueueItemSidecar =
            serde_json::from_slice(&std::fs::read(&sidecars[0]).unwrap()).unwrap();
        assert_eq!(sidecar.gcs_path, "sess-1234/turn_7/tool_state.json");
        assert_eq!(
            sidecar.sha256,
            crate::sha256_hex(content),
            "recovery's corruption guard must accept the pair"
        );
        let temp_file = temp_path_for_sidecar(&sidecars[0]).unwrap();
        assert!(temp_file.exists(), "the pair's temp file is in place");
    }
    /// A blocking enqueue rejected by the channel (full here; closed behaves
    /// the same) must roll back `pending` before any await, so a cancelled or
    /// failed hand-off can never leak the counter and poison `wait_idle` into
    /// full-budget stalls for the rest of the session.
    #[tokio::test]
    async fn rejected_blocking_enqueue_does_not_leak_pending() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(1);
        let queue = UploadQueue {
            tx,
            queue_dir,
            resolver: Arc::new(MockResolver),
            stats: stats.clone(),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        let filler = tokio::time::timeout(
            Duration::from_millis(200),
            queue.enqueue_blocking(
                b"filler",
                "s/turn_0/a.json",
                "application/json",
                "a",
                "s",
                0,
            ),
        )
        .await;
        assert!(
            filler.is_err(),
            "no worker: the accepted item never settles"
        );
        assert_eq!(
            stats.pending.load(Ordering::Relaxed),
            1,
            "the accepted item is the only pending one"
        );
        let overflow = tokio::time::timeout(
            Duration::from_millis(200),
            queue.enqueue_blocking(
                b"overflow",
                "s/turn_0/b.json",
                "application/json",
                "b",
                "s",
                0,
            ),
        )
        .await;
        drop(overflow);
        assert_eq!(
            stats.pending.load(Ordering::Relaxed),
            1,
            "a rejected hand-off must not leak pending"
        );
        assert_eq!(
            stats.enqueued.load(Ordering::Relaxed),
            1,
            "a diverted item must not count as enqueued"
        );
        assert_eq!(
            stats.enqueue_fallbacks.load(Ordering::Relaxed),
            1,
            "the overflow item diverted to the inline fallback"
        );
    }
    #[test]
    fn retry_policy_backoff_increases_exponentially() {
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            multiplier: 2.0,
            ..Default::default()
        };
        assert_eq!(policy.backoff_delay(0), Duration::from_secs(1));
        assert_eq!(policy.backoff_delay(1), Duration::from_secs(2));
        assert_eq!(policy.backoff_delay(2), Duration::from_secs(4));
        assert_eq!(policy.backoff_delay(3), Duration::from_secs(8));
    }
    #[test]
    fn retry_policy_backoff_capped_at_max() {
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            multiplier: 2.0,
            ..Default::default()
        };
        assert_eq!(policy.backoff_delay(5), Duration::from_secs(10));
        assert_eq!(policy.backoff_delay(10), Duration::from_secs(10));
    }
    #[test]
    fn auth_park_probe_override_rejects_zero_and_floors() {
        assert_eq!(auth_park_probe_override(0), None);
        assert_eq!(auth_park_probe_override(1), Some(Duration::from_secs(1)));
        assert_eq!(auth_park_probe_override(2), Some(Duration::from_secs(2)));
        assert_eq!(
            auth_park_probe_override(600),
            Some(Duration::from_secs(600))
        );
    }
    #[test]
    fn temp_file_name_is_unique() {
        let a = temp_file_name("metadata", "session-abc123", 0);
        let b = temp_file_name("metadata", "session-abc123", 0);
        assert_ne!(
            a, b,
            "temp file names should be unique (counter suffix differs)"
        );
    }
    #[test]
    fn temp_file_name_contains_components() {
        let name = temp_file_name("config", "019abc-def0-1234", 3);
        assert!(name.contains("turn3"), "should contain turn number");
        assert!(name.contains("config"), "should contain artifact name");
    }
    #[test]
    fn with_client_version_sets_field() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir,
            resolver: Arc::new(MockResolver),
            stats,
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        assert!(queue.client_version.is_none(), "starts as None");
        let queue = queue.with_client_version("1.2.3-test");
        assert_eq!(
            queue.client_version.as_deref(),
            Some("1.2.3-test"),
            "with_client_version sets the field"
        );
    }
    #[tokio::test]
    async fn enqueue_copies_client_version_onto_item() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats,
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: Some("0.1.42".to_string()),
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        queue
            .enqueue(
                b"data",
                "session/turn_0/test.json",
                "application/json",
                "test",
                "session-123",
                0,
            )
            .await
            .unwrap();
        let item = rx.recv().await.expect("item enqueued");
        assert_eq!(
            item.client_version.as_deref(),
            Some("0.1.42"),
            "enqueued item carries client_version from the queue"
        );
    }
    /// Build a worker-less queue (no spawned worker; caller owns `rx`) for the
    /// `enqueue_bytes_blocking` outcome tests. Mirrors the inline literals used
    /// by the other unit tests above.
    fn build_test_queue(
        queue_dir: PathBuf,
        tx: mpsc::Sender<UploadQueueItem>,
        stats: Arc<UploadQueueStats>,
        max_queue_bytes: u64,
    ) -> UploadQueue {
        UploadQueue {
            tx,
            queue_dir,
            resolver: Arc::new(MockResolver),
            stats,
            max_queue_bytes,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }
    #[tokio::test]
    async fn enqueue_bytes_blocking_returns_enqueued_on_happy_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(
            queue_dir.clone(),
            tx,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let outcome = queue
            .enqueue_bytes_blocking(
                b"archive-bytes",
                "sess/turn_0/before_changes.tar.gz",
                "application/gzip",
                "before_changes",
                "session-xyz",
                0,
            )
            .await;
        assert_eq!(outcome, EnqueueOutcome::Enqueued);
        let item = rx.recv().await.expect("item should be enqueued");
        assert_eq!(item.gcs_path, "sess/turn_0/before_changes.tar.gz");
        assert_eq!(stats.enqueued.load(Ordering::Relaxed), 1);
        assert_eq!(stats.enqueue_fallbacks.load(Ordering::Relaxed), 0);
        let mut names: Vec<String> = std::fs::read_dir(&queue_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names.len(),
            2,
            "temp + sidecar written to queue dir: {names:?}"
        );
        assert_eq!(
            names.iter().filter(|n| n.ends_with(SIDECAR_SUFFIX)).count(),
            1,
            "exactly one sidecar manifest accompanies the temp file"
        );
    }
    #[tokio::test]
    async fn enqueue_dedups_identical_gcs_path_until_item_settles() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(
            queue_dir.clone(),
            tx,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let blob = "changes_dedup/v2/blobs/sha256_aaa";
        let first = queue
            .enqueue_bytes_blocking(
                b"video",
                blob,
                "application/octet-stream",
                "dedup_aaa",
                "s",
                0,
            )
            .await;
        assert_eq!(first, EnqueueOutcome::Enqueued);
        let dup = queue
            .enqueue_bytes_blocking(
                b"video",
                blob,
                "application/octet-stream",
                "dedup_aaa",
                "s",
                1,
            )
            .await;
        assert_eq!(dup, EnqueueOutcome::Deduplicated);
        assert_eq!(stats.deduplicated.load(Ordering::Relaxed), 1);
        let other = queue
            .enqueue_bytes_blocking(
                b"other",
                "changes_dedup/v2/blobs/sha256_bbb",
                "application/octet-stream",
                "dedup_bbb",
                "s",
                1,
            )
            .await;
        assert_eq!(other, EnqueueOutcome::Enqueued);
        let temp_files = std::fs::read_dir(&queue_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with(SIDECAR_SUFFIX))
            .count();
        assert_eq!(temp_files, 2, "duplicate must not spill a second copy");
        let first_item = rx.recv().await.expect("first item buffered");
        assert_eq!(first_item.gcs_path, blob);
        drop(first_item);
        let after_settle = queue
            .enqueue_bytes_blocking(
                b"video",
                blob,
                "application/octet-stream",
                "dedup_aaa",
                "s",
                2,
            )
            .await;
        assert_eq!(
            after_settle,
            EnqueueOutcome::Enqueued,
            "re-enqueue allowed once the in-flight copy settled"
        );
    }
    #[tokio::test]
    async fn non_content_addressed_path_is_never_deduped() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(
            queue_dir.clone(),
            tx,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let path = "s/workspace_tool_definitions.json";
        let first = queue
            .enqueue_bytes_blocking(b"v1", path, "application/json", "tools", "s", 0)
            .await;
        assert_eq!(first, EnqueueOutcome::Enqueued);
        let second = queue
            .enqueue_bytes_blocking(b"v2-updated", path, "application/json", "tools", "s", 1)
            .await;
        assert_eq!(
            second,
            EnqueueOutcome::Enqueued,
            "mutable-content re-upload on a stable path must not be dropped"
        );
        assert_eq!(stats.deduplicated.load(Ordering::Relaxed), 0);
        let temp_files = std::fs::read_dir(&queue_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with(SIDECAR_SUFFIX))
            .count();
        assert_eq!(temp_files, 2, "both uploads must be queued (no path dedup)");
        let _ = rx.recv().await;
        let _ = rx.recv().await;
    }
    #[tokio::test]
    async fn enqueue_file_reference_dedups_before_snapshotting() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("video.bin");
        std::fs::write(&source, b"reference-bytes").unwrap();
        let sha = crate::sha256_hex_from_file(&source, None).unwrap();
        let blob = format!("changes_dedup/v2/blobs/sha256_{sha}");
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(
            queue_dir.clone(),
            tx,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let _first = queue
            .enqueue_file_reference(
                &source,
                &sha,
                &blob,
                "application/octet-stream",
                "dedup",
                "s",
                0,
            )
            .await
            .expect("first reference enqueues");
        let dup = queue
            .enqueue_file_reference(
                &source,
                &sha,
                &blob,
                "application/octet-stream",
                "dedup",
                "s",
                1,
            )
            .await
            .expect("dup reference returns Ok");
        let dup_result = dup.completion_rx.await.expect("completion resolves");
        assert!(
            dup_result.is_err(),
            "deduplicated reference resolves non-fatally"
        );
        assert_eq!(stats.deduplicated.load(Ordering::Relaxed), 1);
        let snapshots = std::fs::read_dir(&queue_dir).unwrap().count();
        assert_eq!(snapshots, 1, "duplicate reference must not snapshot again");
        let _ = rx.recv().await;
    }
    #[tokio::test]
    async fn enqueue_file_dedups_identical_gcs_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("blob.bin");
        std::fs::write(&source, b"file-bytes").unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(
            queue_dir.clone(),
            tx,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let blob = "changes_dedup/v2/blobs/sha256_file";
        queue
            .enqueue_file(
                &source,
                blob,
                "application/octet-stream",
                "dedup_file",
                "s",
                0,
            )
            .await
            .unwrap();
        queue
            .enqueue_file(
                &source,
                blob,
                "application/octet-stream",
                "dedup_file",
                "s",
                1,
            )
            .await
            .unwrap();
        assert_eq!(stats.deduplicated.load(Ordering::Relaxed), 1);
        let copies = std::fs::read_dir(&queue_dir).unwrap().count();
        assert_eq!(
            copies, 1,
            "duplicate enqueue_file must not copy a second time"
        );
    }
    #[tokio::test]
    async fn enqueue_file_blocking_dedup_resolves_completion() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("blob.bin");
        std::fs::write(&source, b"file-bytes").unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(
            queue_dir.clone(),
            tx,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let blob = "changes_dedup/v2/blobs/sha256_fb";
        let _first = queue
            .enqueue_file_blocking(
                &source,
                blob,
                "application/octet-stream",
                "dedup_fb",
                "s",
                0,
                false,
            )
            .await
            .expect("first enqueues");
        let dup = queue
            .enqueue_file_blocking(
                &source,
                blob,
                "application/octet-stream",
                "dedup_fb",
                "s",
                1,
                false,
            )
            .await
            .expect("dup returns Ok");
        let dup_result = dup.completion_rx.await.expect("completion resolves");
        assert!(
            dup_result.is_err(),
            "deduplicated enqueue_file_blocking resolves non-fatally"
        );
        assert_eq!(stats.deduplicated.load(Ordering::Relaxed), 1);
    }
    #[tokio::test]
    async fn enqueue_bytes_blocking_falls_back_to_inline_when_over_budget() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(queue_dir.clone(), tx, stats.clone(), 0);
        let outcome = queue
            .enqueue_bytes_blocking(
                b"too-big",
                "sess/turn_1/after_changes.tar.gz",
                "application/gzip",
                "after_changes",
                "session-xyz",
                1,
            )
            .await;
        assert_eq!(outcome, EnqueueOutcome::FellBackToInline);
        assert_eq!(stats.enqueue_fallbacks.load(Ordering::Relaxed), 1);
        let entries = std::fs::read_dir(&queue_dir).unwrap().count();
        assert_eq!(entries, 0, "temp file removed on over-budget fallback");
    }
    #[tokio::test]
    async fn enqueue_bytes_blocking_returns_failed_when_worker_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        drop(rx);
        let queue = build_test_queue(
            queue_dir.clone(),
            tx,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let outcome = queue
            .enqueue_bytes_blocking(
                b"bytes",
                "sess/turn_2/after_changes.tar.gz",
                "application/gzip",
                "after_changes",
                "session-xyz",
                2,
            )
            .await;
        assert!(
            matches!(outcome, EnqueueOutcome::Failed { .. }),
            "closed worker channel must map to Failed, got {outcome:?}"
        );
        assert_eq!(stats.enqueue_fallbacks.load(Ordering::Relaxed), 0);
        assert_eq!(stats.pending.load(Ordering::Relaxed), 0);
        assert_eq!(stats.pending_bytes.load(Ordering::Relaxed), 0);
        let entries = std::fs::read_dir(&queue_dir).unwrap().count();
        assert_eq!(entries, 0, "temp file removed when the worker is closed");
    }
    #[tokio::test]
    async fn enqueue_bytes_blocking_returns_failed_when_temp_write_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("does/not/exist");
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(queue_dir, tx, stats.clone(), DEFAULT_MAX_QUEUE_BYTES);
        let outcome = queue
            .enqueue_bytes_blocking(
                b"bytes",
                "sess/turn_3/after_changes.tar.gz",
                "application/gzip",
                "after_changes",
                "session-xyz",
                3,
            )
            .await;
        assert!(
            matches!(outcome, EnqueueOutcome::Failed { .. }),
            "temp-write failure must map to Failed, got {outcome:?}"
        );
        assert_eq!(stats.enqueued.load(Ordering::Relaxed), 0);
        assert_eq!(stats.enqueue_fallbacks.load(Ordering::Relaxed), 0);
    }
    /// `enqueue_bytes_blocking` writes a temp+sidecar pair whose fields
    /// describe the bytes, and stamps the sidecar path onto the item.
    #[tokio::test]
    async fn enqueue_bytes_blocking_writes_sidecar_manifest_alongside_tmp() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(
            queue_dir.clone(),
            tx,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let content = b"archive-bytes-payload";
        let outcome = queue
            .enqueue_bytes_blocking(
                content,
                "session-xyz/turn_7/before_changes.tar.gz",
                "application/gzip",
                "before_changes.tar.gz",
                "session-xyz",
                7,
            )
            .await;
        assert_eq!(outcome, EnqueueOutcome::Enqueued);
        let mut names: Vec<String> = std::fs::read_dir(&queue_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names.len(),
            2,
            "temp + sidecar written as a pair: {names:?}"
        );
        let sidecar_name = names
            .iter()
            .find(|n| n.ends_with(SIDECAR_SUFFIX))
            .expect("a .meta.json sidecar was written")
            .clone();
        let temp_name = names
            .iter()
            .find(|n| !n.ends_with(SIDECAR_SUFFIX))
            .expect("the archive temp file was written")
            .clone();
        assert_eq!(sidecar_name, format!("{temp_name}{SIDECAR_SUFFIX}"));
        let item = rx.recv().await.expect("item handed to the worker");
        assert_eq!(
            item.sidecar_path.as_ref().unwrap(),
            &queue_dir.join(&sidecar_name)
        );
        let raw = std::fs::read(queue_dir.join(&sidecar_name)).unwrap();
        let sidecar: QueueItemSidecar = serde_json::from_slice(&raw).unwrap();
        assert_eq!(sidecar.schema_version, QUEUE_ITEM_SIDECAR_SCHEMA_VERSION);
        assert_eq!(sidecar.session_id, "session-xyz");
        assert_eq!(sidecar.turn_number, 7);
        assert_eq!(sidecar.gcs_path, "session-xyz/turn_7/before_changes.tar.gz");
        assert_eq!(sidecar.content_type, "application/gzip");
        assert_eq!(sidecar.artifact_name, "before_changes.tar.gz");
        assert_eq!(sidecar.sha256, crate::sha256_hex(content));
        assert!(!sidecar.enqueued_at.is_empty(), "enqueued_at timestamp set");
    }
    /// The fire-and-forget `enqueue` keeps the legacy single-temp-file shape:
    /// no sidecar written, no sidecar path on the item.
    #[tokio::test]
    async fn enqueue_does_not_write_sidecar_legacy_fast_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = build_test_queue(
            queue_dir.clone(),
            tx,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        queue
            .enqueue(
                b"legacy-bytes",
                "session-xyz/turn_0/metadata.json",
                "application/json",
                "metadata.json",
                "session-xyz",
                0,
            )
            .await
            .unwrap();
        let names: Vec<String> = std::fs::read_dir(&queue_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            1,
            "exactly one temp file, no sidecar: {names:?}"
        );
        assert!(
            !names[0].ends_with(SIDECAR_SUFFIX),
            "legacy enqueue must not write a .meta.json sidecar"
        );
        let item = rx.recv().await.expect("item handed to the worker");
        assert!(
            item.sidecar_path.is_none(),
            "legacy enqueue item carries no sidecar path"
        );
    }
    #[test]
    fn stats_initial_values() {
        let stats = UploadQueueStats::new();
        assert_eq!(stats.pending.load(Ordering::Relaxed), 0);
        assert_eq!(stats.pending_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(stats.enqueued.load(Ordering::Relaxed), 0);
        assert_eq!(stats.uploaded.load(Ordering::Relaxed), 0);
        assert_eq!(stats.failed.load(Ordering::Relaxed), 0);
        assert_eq!(stats.circuit_breaker_trips.load(Ordering::Relaxed), 0);
        assert_eq!(stats.enqueue_fallbacks.load(Ordering::Relaxed), 0);
        assert_eq!(stats.leaked_temp_files.load(Ordering::Relaxed), 0);
        assert_eq!(stats.reference_stale.load(Ordering::Relaxed), 0);
        assert_eq!(stats.cleanup_orphan_mismatched.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn over_disk_budget_respects_limit() {
        let stats = Arc::new(UploadQueueStats::new());
        stats.pending_bytes.store(7_000_000_000, Ordering::Relaxed);
        let queue = UploadQueue {
            tx: mpsc::channel(1).0,
            queue_dir: PathBuf::from("/tmp"),
            resolver: Arc::new(MockResolver),
            stats,
            max_queue_bytes: 8_000_000_000,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        assert!(!queue.over_disk_budget(500_000_000));
        assert!(queue.over_disk_budget(1_500_000_000));
    }
    #[test]
    fn cleanup_orphans_removes_old_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stale = queue_dir.join("stale_file.json");
        std::fs::write(&stale, b"old data").unwrap();
        let two_hours_ago = std::time::SystemTime::now() - Duration::from_secs(7200);
        let times = std::fs::FileTimes::new().set_modified(two_hours_ago);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(times)
            .unwrap();
        let fresh = queue_dir.join("fresh_file.json");
        std::fs::write(&fresh, b"new data").unwrap();
        let queue = UploadQueue {
            tx: mpsc::channel(1).0,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats: Arc::new(UploadQueueStats::new()),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        queue.cleanup_orphans(Duration::from_secs(3600));
        assert!(!stale.exists(), "stale file should be deleted");
        assert!(fresh.exists(), "fresh file should be kept");
    }
    #[test]
    fn cleanup_orphans_removes_stale_directories() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stale_dir = queue_dir.join("other_stale");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("a.txt"), b"old").unwrap();
        let three_hours_ago = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
        let ft = filetime::FileTime::from_system_time(three_hours_ago);
        filetime::set_file_mtime(&stale_dir, ft).unwrap();
        let fresh_dir = queue_dir.join("scratch_fresh");
        std::fs::create_dir_all(&fresh_dir).unwrap();
        std::fs::write(fresh_dir.join("data.txt"), b"keep me").unwrap();
        let stale_file = queue_dir.join("stale.gz");
        std::fs::write(&stale_file, b"old").unwrap();
        filetime::set_file_mtime(&stale_file, ft).unwrap();
        let queue = UploadQueue {
            tx: mpsc::channel(1).0,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats: Arc::new(UploadQueueStats::new()),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        queue.cleanup_orphans(Duration::from_secs(3600));
        assert!(
            !stale_dir.exists(),
            "stale non-scratch directory tree should be removed"
        );
        assert!(!stale_file.exists(), "stale file should be removed");
        assert!(fresh_dir.exists(), "fresh directory should be preserved");
        assert!(
            fresh_dir.join("data.txt").exists(),
            "files inside fresh directory should be preserved"
        );
    }
    /// Stale `scratch/<sid>/` is reaped; `scratch/` and fresh siblings survive.
    #[test]
    fn cleanup_orphans_recurses_into_scratch_subdirs() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        let scratch_dir = queue_dir.join("scratch");
        std::fs::create_dir_all(&scratch_dir).unwrap();
        let stale_session = scratch_dir.join("old-session-abc");
        std::fs::create_dir_all(&stale_session).unwrap();
        std::fs::write(stale_session.join("pre_edit.txt"), b"old copy").unwrap();
        let three_hours_ago = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
        let ft = filetime::FileTime::from_system_time(three_hours_ago);
        filetime::set_file_mtime(&stale_session, ft).unwrap();
        let fresh_session = scratch_dir.join("fresh-session-xyz");
        std::fs::create_dir_all(&fresh_session).unwrap();
        std::fs::write(fresh_session.join("hot.txt"), b"keep").unwrap();
        let now = std::time::SystemTime::now();
        let fresh_ft = filetime::FileTime::from_system_time(now);
        filetime::set_file_mtime(&fresh_session, fresh_ft).unwrap();
        let queue = UploadQueue {
            tx: mpsc::channel(1).0,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats: Arc::new(UploadQueueStats::new()),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        queue.cleanup_orphans(Duration::from_secs(3600));
        assert!(
            scratch_dir.exists(),
            "scratch/ itself must be preserved across sweeps"
        );
        assert!(
            !stale_session.exists(),
            "stale scratch/<sid>/ subdir should be removed"
        );
        assert!(
            fresh_session.exists(),
            "fresh scratch/<sid>/ subdir should be preserved"
        );
        assert!(
            fresh_session.join("hot.txt").exists(),
            "files inside fresh session subdir should be preserved"
        );
    }
    #[tokio::test]
    async fn enqueue_writes_temp_file_and_returns_ok() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats: stats.clone(),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        let result = queue
            .enqueue(
                b"test content",
                "session/turn_0/config.json",
                "application/json",
                "config",
                "session-123",
                0,
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(stats.pending.load(Ordering::Relaxed), 1);
        assert!(stats.pending_bytes.load(Ordering::Relaxed) > 0);
        let files: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let content = std::fs::read(files[0].path()).unwrap();
        assert_eq!(content, b"test content");
    }
    #[tokio::test]
    async fn enqueue_file_copies_to_queue() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("source.tar.gz");
        std::fs::write(&source, b"tarball bytes").unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats: stats.clone(),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        let result = queue
            .enqueue_file(
                &source,
                "session/turn_0/repo_changes.tar.gz",
                "application/gzip",
                "repo_changes",
                "session-456",
                0,
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(stats.pending.load(Ordering::Relaxed), 1);
        let files: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let content = std::fs::read(files[0].path()).unwrap();
        assert_eq!(content, b"tarball bytes");
    }
    #[tokio::test]
    async fn enqueue_file_blocking_returns_receiver_and_copies() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("blob.bin");
        std::fs::write(&source, b"dedup blob content").unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats: stats.clone(),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        let result = queue
            .enqueue_file_blocking(
                &source,
                "changes_dedup/v2/blobs/sha256_abc123",
                "application/octet-stream",
                "dedup_abc123",
                "session-789",
                1,
                false,
            )
            .await;
        assert!(result.is_ok());
        let enqueue_result = result.unwrap();
        assert_eq!(enqueue_result.original_size, 18);
        assert_eq!(stats.pending.load(Ordering::Relaxed), 1);
        assert!(stats.pending_bytes.load(Ordering::Relaxed) > 0);
        let files: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let content = std::fs::read(files[0].path()).unwrap();
        assert_eq!(content, b"dedup blob content");
        assert!(
            source.exists(),
            "outside-queue source must be preserved (copy fallback)"
        );
    }
    #[tokio::test]
    async fn enqueue_file_blocking_stores_plain_file_even_with_compress_true() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("big.txt");
        let content = "hello world, this is compressible text!\n".repeat(30);
        std::fs::write(&source, &content).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats: stats.clone(),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        let result = queue
            .enqueue_file_blocking(
                &source,
                "patches/sha256_abc",
                "application/octet-stream",
                "patches_abc",
                "session-comp",
                0,
                true,
            )
            .await
            .unwrap();
        assert_eq!(result.original_size, content.len() as u64);
        let files: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let queued = std::fs::read(files[0].path()).unwrap();
        assert_eq!(queued.len(), content.len());
        let item = rx.recv().await.expect("item enqueued");
        assert!(item.compress);
    }
    /// Sources already in `queue_dir` are renamed (not copied) — no double-on-disk.
    #[tokio::test]
    async fn enqueue_file_blocking_renames_when_source_inside_queue_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = queue_dir.join("dedup_abc_0_0");
        std::fs::write(&source, b"dedup blob content").unwrap();
        #[cfg(unix)]
        let src_inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&source).unwrap().ino()
        };
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats: stats.clone(),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        queue
            .enqueue_file_blocking(
                &source,
                "changes_dedup/v2/blobs/sha256_abc",
                "application/octet-stream",
                "dedup_abc",
                "session-rename",
                1,
                false,
            )
            .await
            .unwrap();
        assert!(
            !source.exists(),
            "source file inside queue_dir must be moved, not copied"
        );
        let files: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "expected one file after rename");
        assert_eq!(
            std::fs::read(files[0].path()).unwrap(),
            b"dedup blob content"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let dest_inode = std::fs::metadata(files[0].path()).unwrap().ino();
            assert_eq!(
                src_inode, dest_inode,
                "rename(2) preserves inode; a copy+remove regression would allocate a new inode"
            );
        }
    }
    /// When both rename and copy fail, source is preserved and Err is returned.
    #[test]
    fn move_or_copy_to_queue_rename_then_copy_failure_keeps_source() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = queue_dir.join("dedup_src_0_0");
        std::fs::write(&source, b"payload").unwrap();
        let dest = queue_dir.join("dedup_src_dest");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("blocker"), b"x").unwrap();
        let stats = UploadQueueStats::new();
        let result = move_or_copy_to_queue(&source, &dest, &queue_dir, &stats);
        assert!(result.is_err(), "rename+copy onto a directory must fail");
        assert!(source.exists(), "source must remain on rename+copy failure");
    }
    /// Budget gate diverts to inline upload: no staging, `enqueue_fallbacks`
    /// bumps, `pending_bytes` unchanged.
    #[tokio::test]
    async fn enqueue_file_blocking_budget_gate_fallback() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("src.bin");
        std::fs::write(&source, vec![0xCD; 200]).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let max_queue_bytes: u64 = 1000;
        stats
            .pending_bytes
            .store(max_queue_bytes - 100, Ordering::Relaxed);
        let pre_pending = stats.pending_bytes.load(Ordering::Relaxed);
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver: Arc::new(MockResolver),
            stats: stats.clone(),
            max_queue_bytes,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        let result = queue
            .enqueue_file_blocking(
                &source,
                "gcs/path",
                "application/octet-stream",
                "dedup_x",
                "session-budget",
                0,
                false,
            )
            .await
            .expect("budget fallback must return Ok(EnqueueResult)");
        let _ = result.completion_rx.await;
        let staged: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert!(
            staged.is_empty(),
            "budget fallback must NOT stage a temp file in queue_dir"
        );
        assert_eq!(
            stats.enqueue_fallbacks.load(Ordering::Relaxed),
            1,
            "budget gate must bump enqueue_fallbacks exactly once"
        );
        assert_eq!(
            stats.pending_bytes.load(Ordering::Relaxed),
            pre_pending,
            "budget fallback must NOT bump pending_bytes"
        );
        assert!(
            source.exists(),
            "source must NOT be moved on the fallback path"
        );
    }
    /// Rename-fail / copy-succeed in same-dir: source is removed by the
    /// post-copy `try_remove_temp` so we don't hold two copies.
    #[test]
    fn move_or_copy_to_queue_rename_fail_copy_succeed_removes_source() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = queue_dir.join("dedup_src_0_0");
        std::fs::write(&source, b"payload").unwrap();
        let dest = queue_dir.join("dedup_src_dest");
        let stats = UploadQueueStats::new();
        let result = move_or_copy_to_queue_with(
            &source,
            &dest,
            &queue_dir,
            &stats,
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "forced rename failure",
                ))
            },
            copy_to_queue,
        );
        assert!(result.is_ok(), "rename-fail + copy-succeed must return Ok");
        assert!(
            !source.exists(),
            "source must be removed via try_remove_temp after copy succeeds"
        );
        assert!(dest.exists(), "dest must contain the copied payload");
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        assert_eq!(
            stats.leaked_temp_files.load(Ordering::Relaxed),
            0,
            "successful try_remove_temp must NOT bump leaked_temp_files"
        );
    }
    /// `try_remove_temp` bumps the counter on real errors but stays silent on `NotFound`.
    #[test]
    fn try_remove_temp_bumps_counter_on_real_error_but_not_notfound() {
        let stats = Arc::new(UploadQueueStats::new());
        let missing = PathBuf::from("/definitely/does/not/exist/leaked-temp.bin");
        try_remove_temp(&missing, Some(&stats));
        assert_eq!(
            stats.leaked_temp_files.load(Ordering::Relaxed),
            0,
            "NotFound must not bump leaked_temp_files"
        );
        let temp = tempfile::TempDir::new().unwrap();
        let dir_as_file = temp.path().join("a_directory");
        std::fs::create_dir(&dir_as_file).unwrap();
        try_remove_temp(&dir_as_file, Some(&stats));
        assert_eq!(
            stats.leaked_temp_files.load(Ordering::Relaxed),
            1,
            "real (non-NotFound) errors must bump leaked_temp_files"
        );
        assert!(dir_as_file.exists());
        let dir2 = temp.path().join("a_directory_2");
        std::fs::create_dir(&dir2).unwrap();
        let prev = stats.leaked_temp_files.load(Ordering::Relaxed);
        try_remove_temp(&dir2, None);
        assert_eq!(
            stats.leaked_temp_files.load(Ordering::Relaxed),
            prev,
            "None stats arg must NOT touch the counter"
        );
        assert!(dir2.exists(), "directory should still be present");
    }
    #[tokio::test]
    async fn counting_reader_tracks_bytes() {
        use tokio::io::AsyncReadExt;
        let data = b"hello world, counting reader test data";
        let reader = &data[..];
        let counter = Arc::new(AtomicU64::new(0));
        let mut counting = CountingReader {
            inner: reader,
            bytes_read: counter.clone(),
        };
        let mut buf = Vec::new();
        counting.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, data);
        assert_eq!(counter.load(Ordering::Relaxed), data.len() as u64);
    }
    #[tokio::test]
    async fn streaming_zstd_produces_valid_compressed_output() {
        use async_compression::tokio::bufread::ZstdDecoder;
        use tokio::io::AsyncReadExt;
        let content = "hello world, this is compressible text!\n".repeat(30);
        let reader = tokio::io::BufReader::new(content.as_bytes());
        let encoder = ZstdEncoder::new(reader);
        let counter = Arc::new(AtomicU64::new(0));
        let mut counting = CountingReader {
            inner: encoder,
            bytes_read: counter.clone(),
        };
        let mut compressed = Vec::new();
        counting.read_to_end(&mut compressed).await.unwrap();
        assert_eq!(&compressed[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        assert_eq!(counter.load(Ordering::Relaxed), compressed.len() as u64);
        assert!(compressed.len() < content.len());
        let mut decoder = ZstdDecoder::new(tokio::io::BufReader::new(&compressed[..]));
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).await.unwrap();
        assert_eq!(decompressed, content.as_bytes());
    }
    #[test]
    fn compress_decision_size_threshold() {
        let decide = |compress: bool, size: u64| -> bool { compress && size >= COMPRESS_MIN_BYTES };
        assert!(decide(true, 128));
        assert!(decide(true, 1000));
        assert!(!decide(true, 127));
        assert!(!decide(true, 1));
        assert!(!decide(false, 1000));
        assert!(!decide(false, 128));
    }
    #[tokio::test]
    async fn streaming_zstd_handles_incompressible_data() {
        use tokio::io::AsyncReadExt;
        let mut rng: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let content: Vec<u8> = (0..1024)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng as u8
            })
            .collect();
        let reader = tokio::io::BufReader::new(&content[..]);
        let encoder = ZstdEncoder::new(reader);
        let counter = Arc::new(AtomicU64::new(0));
        let mut counting = CountingReader {
            inner: encoder,
            bytes_read: counter.clone(),
        };
        let mut compressed = Vec::new();
        counting.read_to_end(&mut compressed).await.unwrap();
        assert_eq!(&compressed[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        assert_eq!(counter.load(Ordering::Relaxed), compressed.len() as u64);
    }
    struct CountingResolver {
        count: Arc<AtomicU32>,
        proxy_base_url: String,
    }
    impl TraceExportSource for CountingResolver {
        fn resolve(&self) -> TraceExportConfig {
            self.count.fetch_add(1, Ordering::SeqCst);
            TraceExportConfig {
                bucket_url: None,
                service_account_key: None,
                prefix_dir: None,
                gcs_prefix: None,
                absolute_paths: false,
                archive_name_override: None,
                upload_method: UploadMethod::Proxy {
                    proxy_base_url: self.proxy_base_url.clone(),
                    user_token: "test-token".to_string(),
                    deployment_key: None,
                    alpha_test_key: None,
                },
            }
        }
    }
    #[test]
    fn non_retryable_error_detects_proxy_401() {
        let err = anyhow::anyhow!("Upload to 'path': HTTP 401 - Unauthorized");
        assert!(is_non_retryable_error(&err));
    }
    #[test]
    fn non_retryable_error_detects_proxy_403() {
        let err = anyhow::anyhow!("Upload to 'path': HTTP 403 - Forbidden");
        assert!(is_non_retryable_error(&err));
    }
    #[test]
    fn non_retryable_error_detects_direct_mode_errors() {
        assert!(is_non_retryable_error(&anyhow::anyhow!("401 Unauthorized")));
        assert!(is_non_retryable_error(&anyhow::anyhow!("403 Forbidden")));
    }
    #[test]
    fn non_retryable_error_ignores_retryable_statuses() {
        assert!(!is_non_retryable_error(&anyhow::anyhow!(
            "HTTP 429 - Too Many Requests"
        )));
        assert!(!is_non_retryable_error(&anyhow::anyhow!(
            "HTTP 500 - Internal Server Error"
        )));
        assert!(!is_non_retryable_error(&anyhow::anyhow!(
            "HTTP 503 - Service Unavailable"
        )));
    }
    #[test]
    fn non_retryable_error_ignores_network_errors() {
        assert!(!is_non_retryable_error(&anyhow::anyhow!(
            "Connection refused"
        )));
        assert!(!is_non_retryable_error(&anyhow::anyhow!(
            "DNS resolution failed"
        )));
        assert!(!is_non_retryable_error(&anyhow::anyhow!("timeout")));
    }
    #[test]
    fn non_retryable_error_detects_chained_errors() {
        let inner = anyhow::anyhow!("HTTP 401 - token expired");
        let outer = inner.context("Streaming upload failed for session/turn_0/metadata.json");
        assert!(is_non_retryable_error(&outer));
    }
    fn http_err(status_code: u16) -> anyhow::Error {
        HttpUploadError {
            status_code,
            message: format!("op: HTTP {status_code}"),
        }
        .into()
    }
    #[test]
    fn upload_disposition_structured_terminal() {
        for code in [400u16, 403, 404] {
            assert_eq!(upload_disposition(&http_err(code)), Disposition::Terminal);
            let wrapped = http_err(code).context("Streaming upload failed for s/turn_0/x");
            assert_eq!(upload_disposition(&wrapped), Disposition::Terminal);
        }
    }
    #[test]
    fn upload_disposition_structured_auth_and_retryable() {
        assert_eq!(upload_disposition(&http_err(401)), Disposition::AuthRefresh);
        for code in [429u16, 500, 503] {
            assert_eq!(upload_disposition(&http_err(code)), Disposition::Retryable);
        }
    }
    #[test]
    fn upload_disposition_unstructured_is_not_terminal() {
        assert_eq!(
            upload_disposition(&anyhow::anyhow!(
                "HTTP 503 - upstream said HTTP 404 Not Found"
            )),
            Disposition::Retryable
        );
        assert_eq!(
            upload_disposition(&anyhow::anyhow!("Connection reset")),
            Disposition::Retryable
        );
    }
    #[test]
    fn upload_disposition_breaker_open_is_retryable() {
        let err: anyhow::Error = HttpUploadError {
            status_code: 503,
            message: "upload: circuit breaker open; retry after 1.0s".to_string(),
        }
        .into();
        assert_eq!(upload_disposition(&err), Disposition::Retryable);
    }
    #[test]
    fn upload_disposition_direct_mode_auth_fallback() {
        assert_eq!(
            upload_disposition(&anyhow::anyhow!("403 Forbidden")),
            Disposition::AuthRefresh
        );
    }
    #[tokio::test]
    async fn upload_with_retries_resolves_credentials_each_attempt() {
        let count = Arc::new(AtomicU32::new(0));
        let resolver: Arc<dyn TraceExportSource> = Arc::new(CountingResolver {
            count: count.clone(),
            proxy_base_url: "http://127.0.0.1:1".to_string(),
        });
        let policy = UploadRetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            multiplier: 1.0,
            max_age: DEFAULT_MAX_AGE,
            auth_park_probe_interval: DEFAULT_AUTH_PARK_PROBE_INTERVAL,
        };
        let mut item = UploadQueueItem {
            source: UploadSource::OwnedTemp(PathBuf::from("/nonexistent/upload_queue_test_file")),
            gcs_path: "test/path".to_string(),
            content_type: "application/json".to_string(),
            artifact_name: "test".to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: None,
            completion_tx: None,
            client_version: None,
            compress: false,
            parent_span: tracing::Span::none(),
            _in_flight: None,
        };
        let result = run_upload_with_retries(&mut item, &resolver, &policy).await;
        assert!(result.is_err());
        assert_eq!(item.attempts, 3, "should exhaust all retry attempts");
        assert_eq!(
            count.load(Ordering::SeqCst),
            3,
            "resolver.resolve() called each attempt"
        );
    }
    /// Exercises the 401 abort path end-to-end via a mock axum server.
    ///
    /// On the first 401, `upload_with_retries` re-resolves credentials and
    /// retries once. If the second attempt also returns 401, it aborts.
    #[tokio::test]
    async fn upload_with_retries_aborts_on_persistent_auth_error() {
        use axum::{
            Router, body::Body, extract::State, http::StatusCode, response::IntoResponse,
            routing::post,
        };
        #[derive(Clone)]
        struct TestState {
            request_count: Arc<AtomicU32>,
        }
        async fn handler_401(State(s): State<TestState>, _body: Body) -> impl IntoResponse {
            s.request_count.fetch_add(1, Ordering::SeqCst);
            (StatusCode::UNAUTHORIZED, "Invalid token")
        }
        let state = TestState {
            request_count: Arc::new(AtomicU32::new(0)),
        };
        let app = Router::new()
            .route("/v1/storage", post(handler_401))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let resolve_count = Arc::new(AtomicU32::new(0));
        let resolver: Arc<dyn TraceExportSource> = Arc::new(CountingResolver {
            count: resolve_count.clone(),
            proxy_base_url: format!("http://{}/v1", addr),
        });
        let policy = UploadRetryPolicy {
            max_attempts: 5,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            multiplier: 1.0,
            max_age: DEFAULT_MAX_AGE,
            auth_park_probe_interval: DEFAULT_AUTH_PARK_PROBE_INTERVAL,
        };
        let temp = tempfile::TempDir::new().unwrap();
        let file_path = temp.path().join("test.json");
        std::fs::write(&file_path, b"test data").unwrap();
        let mut item = UploadQueueItem {
            source: UploadSource::OwnedTemp(file_path),
            gcs_path: "session/turn_0/test.json".to_string(),
            content_type: "application/json".to_string(),
            artifact_name: "test".to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: None,
            completion_tx: None,
            client_version: None,
            compress: false,
            parent_span: tracing::Span::none(),
            _in_flight: None,
        };
        let result = run_upload_with_retries(&mut item, &resolver, &policy).await;
        assert!(result.is_err());
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("401"),
            "error should mention 401: {}",
            err_msg
        );
        assert_eq!(
            item.attempts, 2,
            "should retry once after auth error then abort"
        );
        assert_eq!(
            resolve_count.load(Ordering::SeqCst),
            2,
            "credentials re-resolved once for the auth retry"
        );
        assert_eq!(
            state.request_count.load(Ordering::SeqCst),
            2,
            "two HTTP requests: initial + one auth retry"
        );
    }
    /// Ignores the worker's `timeout` in favor of the short `wait_slice` —
    /// early-returning waits are tolerated by the park loop, and tests
    /// shouldn't sit through the production 5s interval.
    struct ParkingResolver {
        proxy_base_url: String,
        token_gen: tokio::sync::watch::Sender<u64>,
        hook_enabled: bool,
        wait_slice: Duration,
        seen_bearers: Mutex<Vec<Option<String>>>,
        usable: std::sync::atomic::AtomicBool,
    }
    impl ParkingResolver {
        fn new(proxy_base_url: String) -> Self {
            Self {
                proxy_base_url,
                token_gen: tokio::sync::watch::channel(0).0,
                hook_enabled: true,
                wait_slice: Duration::from_millis(10),
                seen_bearers: Mutex::new(Vec::new()),
                usable: std::sync::atomic::AtomicBool::new(true),
            }
        }
        fn signal_recovery(&self) {
            self.token_gen.send_modify(|g| *g += 1);
        }
        fn set_usable(&self, v: bool) {
            self.usable.store(v, Ordering::SeqCst);
        }
    }
    impl TraceExportSource for ParkingResolver {
        fn has_usable_credential(&self) -> bool {
            self.usable.load(Ordering::SeqCst)
        }
        fn resolve(&self) -> TraceExportConfig {
            TraceExportConfig {
                bucket_url: None,
                service_account_key: None,
                prefix_dir: None,
                gcs_prefix: None,
                absolute_paths: false,
                archive_name_override: None,
                upload_method: UploadMethod::Proxy {
                    proxy_base_url: self.proxy_base_url.clone(),
                    user_token: "test-token".to_string(),
                    deployment_key: None,
                    alpha_test_key: None,
                },
            }
        }
        fn wait_for_auth_recovery(
            &self,
            failed_bearer: Option<&str>,
            _timeout: Duration,
        ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>>>
        {
            self.seen_bearers
                .lock()
                .unwrap()
                .push(failed_bearer.map(str::to_owned));
            if !self.hook_enabled {
                return None;
            }
            let mut rx = self.token_gen.subscribe();
            let slice = self.wait_slice;
            Some(Box::pin(async move {
                if *rx.borrow() > 0 {
                    return true;
                }
                tokio::select! {
                    r = rx.changed() => r.is_ok(),
                    _ = tokio::time::sleep(slice) => false,
                }
            }))
        }
    }
    #[derive(Clone)]
    struct FlippableAuthState {
        request_count: Arc<AtomicU32>,
        unauthorized: Arc<std::sync::atomic::AtomicBool>,
    }
    async fn flippable_auth_handler(
        axum::extract::State(s): axum::extract::State<FlippableAuthState>,
        _body: axum::body::Body,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        s.request_count.fetch_add(1, Ordering::SeqCst);
        if s.unauthorized.load(Ordering::SeqCst) {
            return (axum::http::StatusCode::UNAUTHORIZED, "Invalid token").into_response();
        }
        (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"bucket":"b","path":"p","size":1,"content_type":"application/json","generation":1}"#,
        )
            .into_response()
    }
    async fn spawn_flippable_server(initially_unauthorized: bool) -> (FlippableAuthState, String) {
        use axum::{Router, routing::post};
        let state = FlippableAuthState {
            request_count: Arc::new(AtomicU32::new(0)),
            unauthorized: Arc::new(std::sync::atomic::AtomicBool::new(initially_unauthorized)),
        };
        let app = Router::new()
            .route("/v1/storage", post(flippable_auth_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (state, format!("http://{}/v1", addr))
    }
    fn park_test_item(
        temp: &tempfile::TempDir,
    ) -> (
        UploadQueueItem,
        oneshot::Receiver<anyhow::Result<UploadCompletion>>,
    ) {
        let file_path = temp.path().join("test.json");
        std::fs::write(&file_path, b"test data").unwrap();
        let (tx, rx) = oneshot::channel();
        (
            UploadQueueItem {
                source: UploadSource::OwnedTemp(file_path),
                gcs_path: "session/turn_0/test.json".to_string(),
                content_type: "application/json".to_string(),
                artifact_name: "test".to_string(),
                attempts: 0,
                enqueued_at: Instant::now(),
                sidecar_path: None,
                completion_tx: Some(tx),
                client_version: None,
                compress: false,
                parent_span: tracing::Span::none(),
                _in_flight: None,
            },
            rx,
        )
    }
    /// The pre-park behavior dropped the artifact at this exact point.
    #[tokio::test]
    async fn parked_item_uploads_after_auth_recovery() {
        let (state, url) = spawn_flippable_server(true).await;
        let resolver = Arc::new(ParkingResolver::new(url));
        let resolver_dyn: Arc<dyn TraceExportSource> = resolver.clone();
        let stats = Arc::new(UploadQueueStats::new());
        let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let temp = tempfile::TempDir::new().unwrap();
        let (mut item, completion_rx) = park_test_item(&temp);
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            ..Default::default()
        };
        let task = {
            let resolver = resolver_dyn.clone();
            let stats = stats.clone();
            let draining = draining.clone();
            tokio::spawn(async move {
                upload_with_retries(&mut item, &resolver, &policy, 100, &stats, &draining, None)
                    .await
            })
        };
        let parked_err = tokio::time::timeout(Duration::from_secs(5), completion_rx)
            .await
            .expect("waiter released before recovery")
            .expect("completion channel alive");
        let msg = format!(
            "{:#}",
            parked_err.expect_err("parked notification is an Err")
        );
        assert!(
            msg.contains("parked"),
            "waiter sees the parked marker: {msg}"
        );
        assert_eq!(stats.auth_parked.load(Ordering::Relaxed), 1);
        let requests_while_parked = state.request_count.load(Ordering::SeqCst);
        assert_eq!(
            requests_while_parked, 2,
            "initial attempt + one refresh retry"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            state.request_count.load(Ordering::SeqCst),
            requests_while_parked,
            "no probe traffic while parked"
        );
        state.unauthorized.store(false, Ordering::SeqCst);
        resolver.signal_recovery();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("parked item resumes after recovery")
            .expect("task join");
        assert!(result.is_ok(), "upload succeeds after recovery: {result:?}");
        assert_eq!(state.request_count.load(Ordering::SeqCst), 3);
        assert_eq!(
            resolver.seen_bearers.lock().unwrap().first(),
            Some(&Some("test-token".to_owned())),
            "hook receives the bearer the rejected attempt used"
        );
    }
    /// Recovery detection must be level-triggered: the park loop rebuilds its
    /// wait future every slice, so a `signal_recovery()` that lands in the gap
    /// between one slice finishing and the next subscribe must still be seen.
    /// An edge-triggered watch loses that signal, leaving the item parked for a
    /// full `auth_park_probe_interval` (300s) and timing out the resume wait.
    #[tokio::test]
    async fn parking_resolver_recovery_is_level_triggered() {
        let (_state, url) = spawn_flippable_server(true).await;
        let resolver = ParkingResolver::new(url);
        resolver.signal_recovery();
        let wait = resolver
            .wait_for_auth_recovery(Some("test-token"), AUTH_PARK_WAIT_INTERVAL)
            .expect("hook enabled");
        assert!(
            wait.await,
            "recovery signaled before subscribe must still wake the parked item"
        );
    }
    /// A parked item releases its concurrency permit (parking does zero wire
    /// I/O) so other uploads keep flowing during an auth outage, then
    /// re-acquires it before resuming. Without release, `max_concurrent` parked
    /// items would pin every worker slot for up to `max_age` and stall
    /// dispatch/drain.
    #[tokio::test]
    async fn parked_item_releases_concurrency_permit() {
        let (state, url) = spawn_flippable_server(true).await;
        let resolver = Arc::new(ParkingResolver::new(url));
        let resolver_dyn: Arc<dyn TraceExportSource> = resolver.clone();
        let stats = Arc::new(UploadQueueStats::new());
        let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let temp = tempfile::TempDir::new().unwrap();
        let (mut item, _completion_rx) = park_test_item(&temp);
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            ..Default::default()
        };
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let held = semaphore.clone().acquire_owned().await.unwrap();
        assert_eq!(semaphore.available_permits(), 0);
        let mut concurrency = ConcurrencyPermit {
            semaphore: semaphore.clone(),
            permit: Some(held),
        };
        let task = {
            let resolver = resolver_dyn.clone();
            let stats = stats.clone();
            let draining = draining.clone();
            tokio::spawn(async move {
                let r = upload_with_retries(
                    &mut item,
                    &resolver,
                    &policy,
                    100,
                    &stats,
                    &draining,
                    Some(&mut concurrency),
                )
                .await;
                (r, concurrency.permit.is_some())
            })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while semaphore.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("parked item releases its concurrency permit");
        assert_eq!(stats.auth_parked.load(Ordering::Relaxed), 1);
        state.unauthorized.store(false, Ordering::SeqCst);
        resolver.signal_recovery();
        let (result, held_after) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("parked item resumes after recovery")
            .expect("task join");
        assert!(result.is_ok(), "upload succeeds after recovery: {result:?}");
        assert!(
            held_after,
            "permit re-acquired before the post-park wire attempt"
        );
    }
    /// Without a recovery hook the item is dropped, never parked: the waiter
    /// must receive the original 401 error, not the parked marker.
    #[tokio::test]
    async fn no_hook_drops_without_park_marker() {
        let (_state, url) = spawn_flippable_server(true).await;
        let mut resolver = ParkingResolver::new(url);
        resolver.hook_enabled = false;
        let resolver: Arc<dyn TraceExportSource> = Arc::new(resolver);
        let stats = Arc::new(UploadQueueStats::new());
        let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let temp = tempfile::TempDir::new().unwrap();
        let (mut item, completion_rx) = park_test_item(&temp);
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            ..Default::default()
        };
        let result =
            upload_with_retries(&mut item, &resolver, &policy, 100, &stats, &draining, None).await;
        assert!(result.is_err());
        assert_eq!(
            stats.auth_parked.load(Ordering::Relaxed),
            0,
            "no park entry without a recovery hook"
        );
        assert!(
            item.completion_tx.is_some(),
            "completion stays with the caller's terminal error path"
        );
        drop(item);
        let waiter = completion_rx.await;
        assert!(
            waiter.is_err(),
            "oneshot closes without a parked notification"
        );
    }
    /// Draining and a recovery wake racing: the wake must re-run the guards
    /// and never reach the wire once draining is set.
    #[tokio::test]
    async fn parked_wake_revalidates_drain_before_wire() {
        let (state, url) = spawn_flippable_server(true).await;
        let resolver = Arc::new(ParkingResolver::new(url));
        let resolver_dyn: Arc<dyn TraceExportSource> = resolver.clone();
        let stats = Arc::new(UploadQueueStats::new());
        let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let temp = tempfile::TempDir::new().unwrap();
        let (mut item, _completion_rx) = park_test_item(&temp);
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            ..Default::default()
        };
        let task = {
            let resolver = resolver_dyn.clone();
            let stats = stats.clone();
            let draining = draining.clone();
            tokio::spawn(async move {
                upload_with_retries(&mut item, &resolver, &policy, 100, &stats, &draining, None)
                    .await
            })
        };
        while stats.auth_parked.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        state.unauthorized.store(false, Ordering::SeqCst);
        draining.store(true, Ordering::Relaxed);
        resolver.signal_recovery();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("bails out promptly")
            .expect("task join");
        assert!(result.is_err(), "drain wins over a pending wake");
        assert_eq!(
            state.request_count.load(Ordering::SeqCst),
            2,
            "no wire attempt after draining is set"
        );
    }
    /// With a recovery hook that never fires, the probe interval still
    /// retries: a server-side 401 blip heals without a client token change.
    #[tokio::test]
    async fn parked_item_probe_retries_without_token_change() {
        let (state, url) = spawn_flippable_server(true).await;
        let resolver: Arc<dyn TraceExportSource> = Arc::new(ParkingResolver::new(url));
        let stats = Arc::new(UploadQueueStats::new());
        let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let temp = tempfile::TempDir::new().unwrap();
        let (mut item, _completion_rx) = park_test_item(&temp);
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            auth_park_probe_interval: Duration::from_millis(50),
            ..Default::default()
        };
        {
            let state = state.clone();
            let stats = stats.clone();
            tokio::spawn(async move {
                while stats.auth_parked.load(Ordering::Relaxed) == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                state.unauthorized.store(false, Ordering::SeqCst);
            });
        }
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            upload_with_retries(&mut item, &resolver, &policy, 100, &stats, &draining, None),
        )
        .await
        .expect("probe path resumes the upload");
        assert!(result.is_ok(), "upload succeeds via probe: {result:?}");
        assert_eq!(stats.auth_parked.load(Ordering::Relaxed), 1);
        assert!(
            state.request_count.load(Ordering::SeqCst) >= 3,
            "initial + refresh retry + at least one probe"
        );
    }
    #[tokio::test]
    async fn parked_item_skips_probe_without_usable_credential() {
        let (state, url) = spawn_flippable_server(true).await;
        let resolver = Arc::new(ParkingResolver::new(url));
        resolver.set_usable(false);
        let resolver_dyn: Arc<dyn TraceExportSource> = resolver.clone();
        let stats = Arc::new(UploadQueueStats::new());
        let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let temp = tempfile::TempDir::new().unwrap();
        let (mut item, _completion_rx) = park_test_item(&temp);
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            auth_park_probe_interval: Duration::from_millis(20),
            ..Default::default()
        };
        {
            let state = state.clone();
            let stats = stats.clone();
            let resolver = resolver.clone();
            tokio::spawn(async move {
                while stats.auth_parked.load(Ordering::Relaxed) == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let at_park = state.request_count.load(Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(150)).await;
                assert_eq!(
                    state.request_count.load(Ordering::SeqCst),
                    at_park,
                    "no blind wire probe while the credential is unusable",
                );
                state.unauthorized.store(false, Ordering::SeqCst);
                resolver.set_usable(true);
                resolver.signal_recovery();
            });
        }
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            upload_with_retries(
                &mut item,
                &resolver_dyn,
                &policy,
                100,
                &stats,
                &draining,
                None,
            ),
        )
        .await
        .expect("upload resumes once the credential is usable");
        assert!(
            result.is_ok(),
            "upload succeeds after creds recover: {result:?}"
        );
        assert_eq!(stats.auth_parked.load(Ordering::Relaxed), 1);
    }
    /// Draining flips while an item is parked → the item bails out promptly
    /// (legacy drop) instead of holding `drain()` until its timeout.
    #[tokio::test]
    async fn parked_item_bails_out_on_drain() {
        let (state, url) = spawn_flippable_server(true).await;
        let resolver: Arc<dyn TraceExportSource> = Arc::new(ParkingResolver::new(url));
        let stats = Arc::new(UploadQueueStats::new());
        let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let temp = tempfile::TempDir::new().unwrap();
        let (mut item, _completion_rx) = park_test_item(&temp);
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            ..Default::default()
        };
        let task = {
            let resolver = resolver.clone();
            let stats = stats.clone();
            let draining = draining.clone();
            tokio::spawn(async move {
                upload_with_retries(&mut item, &resolver, &policy, 100, &stats, &draining, None)
                    .await
            })
        };
        while stats.auth_parked.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        draining.store(true, Ordering::Relaxed);
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("parked item bails out promptly when draining")
            .expect("task join");
        assert!(result.is_err(), "drain bail-out is a failure outcome");
        assert_eq!(
            state.request_count.load(Ordering::SeqCst),
            2,
            "no extra wire attempts on drain bail-out"
        );
    }
    /// A parked item that outlives `max_age` is dropped (disk bound holds).
    #[tokio::test]
    async fn parked_item_expires_at_max_age() {
        let (_state, url) = spawn_flippable_server(true).await;
        let resolver: Arc<dyn TraceExportSource> = Arc::new(ParkingResolver::new(url));
        let stats = Arc::new(UploadQueueStats::new());
        let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let temp = tempfile::TempDir::new().unwrap();
        let (mut item, _completion_rx) = park_test_item(&temp);
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            max_age: Duration::from_millis(150),
            ..Default::default()
        };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            upload_with_retries(&mut item, &resolver, &policy, 100, &stats, &draining, None),
        )
        .await
        .expect("expires instead of parking forever");
        assert!(result.is_err(), "max_age bound enforced while parked");
        assert_eq!(stats.auth_parked.load(Ordering::Relaxed), 1);
    }
    /// A terminal client status (400/403/404) must abort on the FIRST attempt: one
    /// HTTP request, one credential resolve, no backoff.
    async fn assert_terminal_status_aborts_immediately(status: axum::http::StatusCode) {
        use axum::{Router, body::Body, extract::State, response::IntoResponse, routing::post};
        #[derive(Clone)]
        struct TestState {
            request_count: Arc<AtomicU32>,
            status: axum::http::StatusCode,
        }
        async fn handler(State(s): State<TestState>, _body: Body) -> impl IntoResponse {
            s.request_count.fetch_add(1, Ordering::SeqCst);
            (s.status, "terminal")
        }
        let state = TestState {
            request_count: Arc::new(AtomicU32::new(0)),
            status,
        };
        let app = Router::new()
            .route("/v1/storage", post(handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let resolve_count = Arc::new(AtomicU32::new(0));
        let resolver: Arc<dyn TraceExportSource> = Arc::new(CountingResolver {
            count: resolve_count.clone(),
            proxy_base_url: format!("http://{}/v1", addr),
        });
        let policy = UploadRetryPolicy {
            max_attempts: 5,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            multiplier: 1.0,
            max_age: DEFAULT_MAX_AGE,
            auth_park_probe_interval: DEFAULT_AUTH_PARK_PROBE_INTERVAL,
        };
        let temp = tempfile::TempDir::new().unwrap();
        let file_path = temp.path().join("test.json");
        std::fs::write(&file_path, b"test data").unwrap();
        let mut item = UploadQueueItem {
            source: UploadSource::OwnedTemp(file_path),
            gcs_path: "session/turn_0/test.json".to_string(),
            content_type: "application/json".to_string(),
            artifact_name: "test".to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: None,
            completion_tx: None,
            client_version: None,
            compress: false,
            parent_span: tracing::Span::none(),
            _in_flight: None,
        };
        let result = run_upload_with_retries(&mut item, &resolver, &policy).await;
        assert!(result.is_err(), "terminal {status} must fail");
        assert_eq!(
            item.attempts, 1,
            "terminal {status} must abort on the first attempt with no retries"
        );
        assert_eq!(
            resolve_count.load(Ordering::SeqCst),
            1,
            "credentials resolved exactly once (no retry) for terminal {status}"
        );
        assert_eq!(
            state.request_count.load(Ordering::SeqCst),
            1,
            "exactly one HTTP request — no retry budget burned on terminal {status}"
        );
    }
    #[tokio::test]
    async fn upload_with_retries_aborts_immediately_on_404() {
        assert_terminal_status_aborts_immediately(axum::http::StatusCode::NOT_FOUND).await;
    }
    #[tokio::test]
    async fn upload_with_retries_aborts_immediately_on_400() {
        assert_terminal_status_aborts_immediately(axum::http::StatusCode::BAD_REQUEST).await;
    }
    #[tokio::test]
    async fn upload_with_retries_aborts_immediately_on_403() {
        assert_terminal_status_aborts_immediately(axum::http::StatusCode::FORBIDDEN).await;
    }
    /// 401 on first attempt, then success on retry with fresh credentials.
    #[tokio::test]
    async fn upload_with_retries_recovers_after_auth_refresh() {
        use axum::{
            Router, body::Body, extract::State, http::StatusCode, response::IntoResponse,
            routing::post,
        };
        #[derive(Clone)]
        struct TestState {
            request_count: Arc<AtomicU32>,
        }
        async fn handler_401_then_ok(State(s): State<TestState>, _body: Body) -> impl IntoResponse {
            let n = s.request_count.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                (StatusCode::UNAUTHORIZED, "Invalid token").into_response()
            } else {
                let body = r#"{"bucket":"b","path":"p","size":9,"content_type":"application/json","generation":1}"#;
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
                    .into_response()
            }
        }
        let state = TestState {
            request_count: Arc::new(AtomicU32::new(0)),
        };
        let app = Router::new()
            .route("/v1/storage", post(handler_401_then_ok))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let resolve_count = Arc::new(AtomicU32::new(0));
        let resolver: Arc<dyn TraceExportSource> = Arc::new(CountingResolver {
            count: resolve_count.clone(),
            proxy_base_url: format!("http://{}/v1", addr),
        });
        let policy = UploadRetryPolicy {
            max_attempts: 5,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            multiplier: 1.0,
            max_age: DEFAULT_MAX_AGE,
            auth_park_probe_interval: DEFAULT_AUTH_PARK_PROBE_INTERVAL,
        };
        let temp = tempfile::TempDir::new().unwrap();
        let file_path = temp.path().join("test.json");
        std::fs::write(&file_path, b"test data").unwrap();
        let mut item = UploadQueueItem {
            source: UploadSource::OwnedTemp(file_path),
            gcs_path: "session/turn_0/test.json".to_string(),
            content_type: "application/json".to_string(),
            artifact_name: "test".to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: None,
            completion_tx: None,
            client_version: None,
            compress: false,
            parent_span: tracing::Span::none(),
            _in_flight: None,
        };
        let result = run_upload_with_retries(&mut item, &resolver, &policy).await;
        assert!(result.is_ok(), "should succeed after auth refresh");
        assert_eq!(item.attempts, 2, "first attempt 401, second attempt OK");
        assert_eq!(
            resolve_count.load(Ordering::SeqCst),
            2,
            "credentials resolved twice"
        );
        assert_eq!(
            state.request_count.load(Ordering::SeqCst),
            2,
            "two HTTP requests total"
        );
    }
    #[tokio::test]
    async fn drain_no_pending_returns_zero() {
        let temp = tempfile::TempDir::new().unwrap();
        let resolver: Arc<dyn TraceExportSource> = Arc::new(MockResolver);
        let queue = UploadQueue::spawn(temp.path(), resolver, UploadRetryPolicy::default());
        let result = queue.drain(Duration::from_secs(1)).await;
        assert_eq!(result, 0);
    }
    #[tokio::test]
    async fn double_drain_is_noop() {
        let temp = tempfile::TempDir::new().unwrap();
        let resolver: Arc<dyn TraceExportSource> = Arc::new(MockResolver);
        let queue = UploadQueue::spawn(temp.path(), resolver, UploadRetryPolicy::default());
        assert_eq!(queue.drain(Duration::from_secs(1)).await, 0);
        assert_eq!(queue.drain(Duration::from_secs(1)).await, 0);
    }
    #[tokio::test]
    async fn enqueue_after_drain_falls_back_to_inline() {
        let temp = tempfile::TempDir::new().unwrap();
        let resolver: Arc<dyn TraceExportSource> = Arc::new(MockResolver);
        let queue = UploadQueue::spawn(temp.path(), resolver, UploadRetryPolicy::default());
        queue.drain(Duration::from_secs(1)).await;
        let before = queue.stats().enqueue_fallbacks.load(Ordering::Relaxed);
        queue
            .enqueue(b"data", "test/path", "text/plain", "test", "sess", 0)
            .await
            .unwrap();
        let after = queue.stats().enqueue_fallbacks.load(Ordering::Relaxed);
        assert!(
            after > before,
            "enqueue after drain should fall back to inline upload"
        );
    }
    async fn spawn_test_server(app: axum::Router) -> Arc<dyn TraceExportSource> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Arc::new(CountingResolver {
            count: Arc::new(AtomicU32::new(0)),
            proxy_base_url: format!("http://{}/v1", addr),
        })
    }
    #[tokio::test]
    async fn drain_processes_pending_items() {
        use axum::{Router, body::Body, http::StatusCode, response::IntoResponse, routing::post};
        async fn ok_handler(_body: Body) -> impl IntoResponse {
            let body =
                r#"{"bucket":"b","path":"p","size":4,"content_type":"text/plain","generation":1}"#;
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
        }
        let app = Router::new().route("/v1/storage", post(ok_handler));
        let resolver = spawn_test_server(app).await;
        let temp = tempfile::TempDir::new().unwrap();
        let queue = UploadQueue::spawn(temp.path(), resolver, UploadRetryPolicy::default());
        queue
            .enqueue(
                b"payload",
                "session/turn_0/test.json",
                "application/json",
                "test",
                "sess-drain",
                0,
            )
            .await
            .unwrap();
        let result = queue.drain(Duration::from_secs(5)).await;
        assert_eq!(result, 0, "all items should be processed during drain");
        assert_eq!(
            queue.stats().uploaded.load(Ordering::Relaxed),
            1,
            "one item should have been uploaded"
        );
    }
    /// A full enqueue→process cycle settles `inflight` and `pending` to zero
    /// and pings the wired transition listener.
    #[tokio::test]
    async fn drain_settles_inflight_and_pending_to_zero() {
        use axum::{Router, body::Body, http::StatusCode, response::IntoResponse, routing::post};
        async fn ok_handler(_body: Body) -> impl IntoResponse {
            let body =
                r#"{"bucket":"b","path":"p","size":4,"content_type":"text/plain","generation":1}"#;
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
        }
        let app = Router::new().route("/v1/storage", post(ok_handler));
        let resolver = spawn_test_server(app).await;
        let temp = tempfile::TempDir::new().unwrap();
        let queue = UploadQueue::spawn(temp.path(), resolver, UploadRetryPolicy::default());
        let notify = Arc::new(Notify::new());
        queue.stats().set_transition_notify(notify.clone());
        let pings = Arc::new(AtomicU64::new(0));
        let pings_task = {
            let pings = pings.clone();
            let notify = notify.clone();
            tokio::spawn(async move {
                loop {
                    notify.notified().await;
                    pings.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        tokio::task::yield_now().await;
        queue
            .enqueue(
                b"payload",
                "session/turn_0/test.json",
                "application/json",
                "test",
                "sess-inflight",
                0,
            )
            .await
            .unwrap();
        let result = queue.drain(Duration::from_secs(5)).await;
        assert_eq!(result, 0, "item processed during drain");
        let stats = queue.stats();
        assert_eq!(
            stats.inflight.load(Ordering::Relaxed),
            0,
            "inflight must settle back to zero after the upload completes"
        );
        assert_eq!(
            stats.pending.load(Ordering::Relaxed),
            0,
            "pending must settle back to zero"
        );
        assert_eq!(stats.uploaded.load(Ordering::Relaxed), 1);
        assert!(
            pings.load(Ordering::SeqCst) > 0,
            "the wired transition listener must have been pinged across enqueue/complete"
        );
        pings_task.abort();
    }
    #[tokio::test]
    async fn drain_timeout_returns_pending_count() {
        use axum::{Router, body::Body, http::StatusCode, response::IntoResponse, routing::post};
        async fn slow_handler(_body: Body) -> impl IntoResponse {
            tokio::time::sleep(Duration::from_secs(60)).await;
            (StatusCode::OK, "ok")
        }
        let app = Router::new().route("/v1/storage", post(slow_handler));
        let resolver = spawn_test_server(app).await;
        let temp = tempfile::TempDir::new().unwrap();
        let policy = UploadRetryPolicy {
            max_attempts: 1,
            ..Default::default()
        };
        let queue = UploadQueue::spawn_with_concurrency(temp.path(), resolver, policy, 1);
        queue
            .enqueue(
                b"payload",
                "session/turn_0/slow.json",
                "application/json",
                "slow",
                "sess-timeout",
                0,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let result = queue.drain(Duration::from_millis(100)).await;
        assert!(result > 0, "should have pending items after timeout");
    }
    /// A parked item releases its semaphore permit, so the worker's drain must
    /// wait on the spawned task (not just permit availability) — otherwise it
    /// reports completion while the parked upload is still running and `pending`
    /// is still nonzero.
    #[tokio::test]
    async fn drain_waits_for_parked_task_to_bail() {
        let (_state, url) = spawn_flippable_server(true).await;
        let resolver: Arc<dyn TraceExportSource> = Arc::new(ParkingResolver::new(url));
        let temp = tempfile::TempDir::new().unwrap();
        let policy = UploadRetryPolicy {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            ..Default::default()
        };
        let queue = UploadQueue::spawn_with_concurrency(temp.path(), resolver, policy, 1);
        queue
            .enqueue(
                b"payload",
                "session/turn_0/park.json",
                "application/json",
                "park",
                "sess-park-drain",
                0,
            )
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while queue.stats().auth_parked.load(Ordering::Relaxed) == 0 {
            assert!(std::time::Instant::now() < deadline, "item never parked");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let result = queue.drain(Duration::from_secs(5)).await;
        assert_eq!(result, 0, "drain completes after the parked task bails");
        assert_eq!(
            queue.stats().pending.load(Ordering::Relaxed),
            0,
            "drain waited for the parked task to finish before returning"
        );
    }
    #[test]
    fn cleanup_orphaned_uploads_stores_count_in_static() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let three_hours_ago = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
        let ft = filetime::FileTime::from_system_time(three_hours_ago);
        for name in ["stale_a.json", "stale_b.json"] {
            let path = queue_dir.join(name);
            std::fs::write(&path, b"old").unwrap();
            filetime::set_file_mtime(&path, ft).unwrap();
        }
        std::fs::write(queue_dir.join("fresh.json"), b"new").unwrap();
        let cleaned = cleanup_orphaned_uploads(temp.path(), Duration::from_secs(3600));
        assert_eq!(cleaned, 2, "should report 2 stale files removed");
        assert_eq!(
            last_orphans_cleaned(),
            2,
            "static should match the returned count"
        );
        assert!(queue_dir.join("fresh.json").exists());
    }
    /// The byte-budget permit math: 1 MiB units rounded up, floor of 1, and a
    /// hard clamp to the semaphore's total so an oversized file never requests
    /// more permits than exist (which would deadlock `acquire_many` / overflow
    /// `u32`).
    #[test]
    fn inline_fallback_permits_clamps_and_never_overflows() {
        assert_eq!(inline_fallback_permits(0), 1);
        assert_eq!(inline_fallback_permits(1), 1);
        assert_eq!(inline_fallback_permits(INLINE_FALLBACK_PERMIT_BYTES), 1);
        assert_eq!(inline_fallback_permits(INLINE_FALLBACK_PERMIT_BYTES + 1), 2);
        assert_eq!(inline_fallback_permits(2 * INLINE_FALLBACK_PERMIT_BYTES), 2);
        assert_eq!(
            inline_fallback_permits(MAX_INLINE_FALLBACK_INFLIGHT_BYTES),
            INLINE_FALLBACK_TOTAL_PERMITS
        );
        assert_eq!(
            inline_fallback_permits(MAX_INLINE_FALLBACK_INFLIGHT_BYTES + 1),
            INLINE_FALLBACK_TOTAL_PERMITS
        );
        let huge = 8u64 * 1024 * 1024 * 1024;
        let permits = inline_fallback_permits(huge);
        assert_eq!(permits, INLINE_FALLBACK_TOTAL_PERMITS);
        let permits_max = inline_fallback_permits(u64::MAX);
        assert_eq!(permits_max, INLINE_FALLBACK_TOTAL_PERMITS);
        let sem = tokio::sync::Semaphore::new(INLINE_FALLBACK_TOTAL_PERMITS as usize);
        let acquired = sem.try_acquire_many(permits);
        assert!(
            acquired.is_ok(),
            "clamped permits must be acquirable from the semaphore"
        );
    }
    /// The over-budget `enqueue_file` fallback streams the source file **at
    /// upload time**, not at enqueue time. This would FAIL against a slurp
    /// implementation (`std::fs::read` at enqueue): we hold the upload parked on
    /// the (0-permit) semaphore, overwrite the source with *different* bytes
    /// after `enqueue_file` returns, then release the permit and assert the
    /// backend received the **new** bytes — proving the read happened at upload
    /// time from the path, not eagerly into memory. Also checks `enqueue_fallbacks`
    /// bumps, no temp copy is staged, the source is preserved, and `pending_bytes`
    /// is untouched.
    #[tokio::test]
    async fn enqueue_file_over_budget_streams_source_at_upload_time() {
        use axum::{
            Router, body::Bytes, extract::State, http::StatusCode, response::IntoResponse,
            routing::post,
        };
        #[derive(Clone)]
        struct TestState {
            request_count: Arc<AtomicU32>,
            last_body: Arc<Mutex<Vec<u8>>>,
        }
        async fn ok_handler(State(s): State<TestState>, body: Bytes) -> impl IntoResponse {
            *s.last_body.lock().unwrap() = body.to_vec();
            s.request_count.fetch_add(1, Ordering::SeqCst);
            let resp = r#"{"bucket":"b","path":"p","size":1,"content_type":"application/octet-stream","generation":1}"#;
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                resp,
            )
        }
        let state = TestState {
            request_count: Arc::new(AtomicU32::new(0)),
            last_body: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/v1/storage", post(ok_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let resolver: Arc<dyn TraceExportSource> = Arc::new(CountingResolver {
            count: Arc::new(AtomicU32::new(0)),
            proxy_base_url: format!("http://{}/v1", addr),
        });
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("image.bin");
        let original = vec![0xAAu8; 4096];
        std::fs::write(&source, &original).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let max_queue_bytes: u64 = 1000;
        stats
            .pending_bytes
            .store(max_queue_bytes, Ordering::Relaxed);
        let pre_pending = stats.pending_bytes.load(Ordering::Relaxed);
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver,
            stats: stats.clone(),
            max_queue_bytes,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(0)),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        queue
            .enqueue_file(
                &source,
                "session/turn_0/image.bin",
                "application/octet-stream",
                "image",
                "session-over",
                0,
            )
            .await
            .expect("over-budget enqueue_file must return Ok");
        assert_eq!(
            stats.enqueue_fallbacks.load(Ordering::Relaxed),
            1,
            "over-budget must bump enqueue_fallbacks exactly once"
        );
        let staged: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert!(
            staged.is_empty(),
            "over-budget fallback must not stage a temp copy in queue_dir"
        );
        assert!(
            source.exists(),
            "source must remain on disk for path streaming"
        );
        assert_eq!(
            stats.pending_bytes.load(Ordering::Relaxed),
            pre_pending,
            "over-budget fallback must not bump pending_bytes"
        );
        assert_eq!(
            state.request_count.load(Ordering::SeqCst),
            0,
            "upload must not run while the semaphore holds no permits"
        );
        let updated = vec![0xBBu8; 4096];
        std::fs::write(&source, &updated).unwrap();
        queue
            .inline_fallback_semaphore
            .add_permits(INLINE_FALLBACK_TOTAL_PERMITS as usize);
        for _ in 0..200 {
            if state.request_count.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            state.request_count.load(Ordering::SeqCst),
            1,
            "inline fallback should stream-upload exactly once from the source path"
        );
        assert_eq!(
            *state.last_body.lock().unwrap(),
            updated,
            "backend must receive the UPDATED bytes (streamed at upload time); a \
             slurp at enqueue time would have sent the original bytes"
        );
    }
    /// Over budget AND the source is missing: `enqueue_file` returns `Err`
    /// (the stat fails) instead of silently returning `Ok` and spawning a
    /// streaming upload of a non-existent path. No fallback is counted.
    #[tokio::test]
    async fn enqueue_file_over_budget_missing_source_returns_err() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let missing = temp.path().join("does_not_exist.bin");
        let stats = Arc::new(UploadQueueStats::new());
        let max_queue_bytes: u64 = 1000;
        stats
            .pending_bytes
            .store(max_queue_bytes, Ordering::Relaxed);
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir,
            resolver: Arc::new(MockResolver),
            stats: stats.clone(),
            max_queue_bytes,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        let result = queue
            .enqueue_file(
                &missing,
                "session/turn_0/missing.bin",
                "application/octet-stream",
                "missing",
                "session-missing",
                0,
            )
            .await;
        assert!(
            result.is_err(),
            "missing source over budget must return Err, not silently Ok"
        );
        assert_eq!(
            stats.enqueue_fallbacks.load(Ordering::Relaxed),
            0,
            "no inline fallback should be spawned for a missing source"
        );
    }
    /// The `enqueue_file` channel-full / closed `try_send`-failure branch streams
    /// from the source path and performs the decrement bookkeeping. Triggered by
    /// dropping the receiver so `try_send` returns `Closed` (same fallback code
    /// path as a full channel). Asserts `enqueue_fallbacks` bumps, `pending`/
    /// `pending_bytes` are decremented back to zero, the source is preserved, and
    /// the inline upload reaches the backend.
    #[tokio::test]
    async fn enqueue_file_channel_full_streams_from_source_path() {
        use axum::{
            Router, body::Body, extract::State, http::StatusCode, response::IntoResponse,
            routing::post,
        };
        #[derive(Clone)]
        struct TestState {
            request_count: Arc<AtomicU32>,
        }
        async fn ok_handler(State(s): State<TestState>, _body: Body) -> impl IntoResponse {
            s.request_count.fetch_add(1, Ordering::SeqCst);
            let resp = r#"{"bucket":"b","path":"p","size":1,"content_type":"application/octet-stream","generation":1}"#;
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                resp,
            )
        }
        let state = TestState {
            request_count: Arc::new(AtomicU32::new(0)),
        };
        let app = Router::new()
            .route("/v1/storage", post(ok_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let resolver: Arc<dyn TraceExportSource> = Arc::new(CountingResolver {
            count: Arc::new(AtomicU32::new(0)),
            proxy_base_url: format!("http://{}/v1", addr),
        });
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("blob.bin");
        std::fs::write(&source, vec![0xCDu8; 2048]).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        drop(rx);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver,
            stats: stats.clone(),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        queue
            .enqueue_file(
                &source,
                "session/turn_0/blob.bin",
                "application/octet-stream",
                "blob",
                "session-chanfull",
                0,
            )
            .await
            .expect("channel-full enqueue_file must return Ok");
        assert_eq!(
            stats.enqueue_fallbacks.load(Ordering::Relaxed),
            1,
            "channel-full must bump enqueue_fallbacks exactly once"
        );
        assert_eq!(
            stats.pending.load(Ordering::Relaxed),
            0,
            "channel-full fallback must decrement pending back to zero"
        );
        assert_eq!(
            stats.pending_bytes.load(Ordering::Relaxed),
            0,
            "channel-full fallback must decrement pending_bytes back to zero"
        );
        let staged: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert!(
            staged.is_empty(),
            "channel-full fallback must remove the rejected staged copy"
        );
        assert!(source.exists(), "source must remain on disk for streaming");
        for _ in 0..200 {
            if state.request_count.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            state.request_count.load(Ordering::SeqCst),
            1,
            "channel-full fallback should stream-upload exactly once from the source path"
        );
    }
    /// The byte-budget semaphore actually bounds inline-fallback concurrency:
    /// firing more uploads than the permit budget allows, the observed peak
    /// concurrency never exceeds the budget, and the excess tasks make progress
    /// only after permits free up. Deterministic — uses a manual-reset gate
    /// (`Semaphore::new(0)` + `add_permits`) and a counting "entered" semaphore
    /// instead of sleeps. If the semaphore gating were deleted, peak concurrency
    /// would equal the number of fired tasks and this test would fail.
    ///
    /// This exercises `spawn_inline_upload_from_path`. The bytes helper
    /// (`spawn_inline_upload`) and the blocking helper (`spawn_inline_upload_blocking`)
    /// use the byte-identical `acquire_many_owned(inline_fallback_permits(..))` gating
    /// idiom against the same shared semaphore, so the concurrency bound proven
    /// here applies to all three; they are not separately parameterized.
    #[tokio::test]
    async fn inline_fallback_semaphore_bounds_concurrency() {
        use axum::{Router, body::Body, http::StatusCode, response::IntoResponse, routing::post};
        /// Resolver that parks each inline-upload task while it holds its permit,
        /// recording peak concurrency. It parks in `resolve_async` (after the
        /// permit is acquired, before `upload_file` opens the file) so the bound
        /// is observed before any real upload. After release it returns a config
        /// pointing at a fast mock server so the permit frees quickly and the
        /// next wave can run.
        struct ConcurrencyResolver {
            inflight: Arc<AtomicU32>,
            peak: Arc<AtomicU32>,
            started: Arc<AtomicU32>,
            /// add_permits(1) on entry; the test waits on this to count parked tasks.
            entered: Arc<tokio::sync::Semaphore>,
            /// starts at 0; the test releases tasks via add_permits.
            gate: Arc<tokio::sync::Semaphore>,
            proxy_base_url: String,
        }
        impl TraceExportSource for ConcurrencyResolver {
            fn resolve(&self) -> TraceExportConfig {
                TraceExportConfig {
                    bucket_url: None,
                    service_account_key: None,
                    prefix_dir: None,
                    gcs_prefix: None,
                    absolute_paths: false,
                    archive_name_override: None,
                    upload_method: UploadMethod::Proxy {
                        proxy_base_url: self.proxy_base_url.clone(),
                        user_token: "t".to_string(),
                        deployment_key: None,
                        alpha_test_key: None,
                    },
                }
            }
            fn resolve_async(
                &self,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TraceExportConfig> + Send + '_>>
            {
                Box::pin(async move {
                    let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    self.peak.fetch_max(now, Ordering::SeqCst);
                    self.started.fetch_add(1, Ordering::SeqCst);
                    self.entered.add_permits(1);
                    let _ = self.gate.acquire().await;
                    self.inflight.fetch_sub(1, Ordering::SeqCst);
                    self.resolve()
                })
            }
        }
        async fn ok_handler(_body: Body) -> impl IntoResponse {
            let resp = r#"{"bucket":"b","path":"p","size":1,"content_type":"application/octet-stream","generation":1}"#;
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                resp,
            )
        }
        let app = Router::new().route("/v1/storage", post(ok_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        const BUDGET: usize = 4;
        const PERMITS_PER_TASK_BYTES: u64 = 2 * 1024 * 1024;
        const EXPECTED_PEAK: u32 = 2;
        const FIRED: usize = 6;
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let resolver = Arc::new(ConcurrencyResolver {
            inflight: Arc::new(AtomicU32::new(0)),
            peak: Arc::new(AtomicU32::new(0)),
            started: Arc::new(AtomicU32::new(0)),
            entered: entered.clone(),
            gate: gate.clone(),
            proxy_base_url: format!("http://{}/v1", addr),
        });
        let peak = resolver.peak.clone();
        let started = resolver.started.clone();
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("blob.bin");
        std::fs::write(&source, b"x").unwrap();
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = UploadQueue {
            tx,
            queue_dir: queue_dir.clone(),
            resolver: resolver.clone(),
            stats: Arc::new(UploadQueueStats::new()),
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(BUDGET)),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        assert_eq!(inline_fallback_permits(PERMITS_PER_TASK_BYTES), 2);
        for _ in 0..FIRED {
            queue.spawn_inline_upload_from_path(
                source.clone(),
                "gcs/path".to_string(),
                "application/octet-stream".to_string(),
                PERMITS_PER_TASK_BYTES,
            );
        }
        let _first_wave = entered
            .acquire_many(EXPECTED_PEAK)
            .await
            .expect("entered semaphore not closed");
        assert_eq!(
            resolver.inflight.load(Ordering::SeqCst),
            EXPECTED_PEAK,
            "exactly the budget's worth of tasks should be in-flight"
        );
        let extra = tokio::time::timeout(Duration::from_millis(300), entered.acquire()).await;
        assert!(
            extra.is_err(),
            "no task beyond the permit budget may run concurrently"
        );
        assert!(
            peak.load(Ordering::SeqCst) <= EXPECTED_PEAK,
            "peak concurrency {} exceeded the permit budget {}",
            peak.load(Ordering::SeqCst),
            EXPECTED_PEAK
        );
        gate.add_permits(FIRED);
        for _ in 0..200 {
            if started.load(Ordering::SeqCst) as usize >= FIRED {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            started.load(Ordering::SeqCst) as usize,
            FIRED,
            "all fired tasks must eventually run once permits free up"
        );
        assert!(
            peak.load(Ordering::SeqCst) <= EXPECTED_PEAK,
            "peak concurrency {} must never exceed the permit budget {} across all waves",
            peak.load(Ordering::SeqCst),
            EXPECTED_PEAK
        );
    }
    /// An axum app whose `/v1/storage` handler returns 200 + a parseable upload
    /// response and counts requests. Returns `(resolver, request_count)`.
    async fn spawn_ok_server() -> (Arc<dyn TraceExportSource>, Arc<AtomicU32>) {
        use axum::{
            Router, body::Body, extract::State, http::StatusCode, response::IntoResponse,
            routing::post,
        };
        async fn ok_handler(State(s): State<Arc<AtomicU32>>, _body: Body) -> impl IntoResponse {
            s.fetch_add(1, Ordering::SeqCst);
            let resp = r#"{"bucket":"b","path":"p","size":1,"content_type":"application/octet-stream","generation":1}"#;
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                resp,
            )
        }
        let count = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/v1/storage", post(ok_handler))
            .with_state(count.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let resolver: Arc<dyn TraceExportSource> = Arc::new(CountingResolver {
            count: Arc::new(AtomicU32::new(0)),
            proxy_base_url: format!("http://{}/v1", addr),
        });
        (resolver, count)
    }
    /// Build an `OwnedSnapshot` queue item directly, to test disk-budget
    /// accounting deterministically regardless of whether the test FS reflinks.
    fn owned_snapshot_item(
        path: PathBuf,
        disk_bytes: u64,
        completion_tx: Option<oneshot::Sender<anyhow::Result<UploadCompletion>>>,
    ) -> UploadQueueItem {
        UploadQueueItem {
            source: UploadSource::OwnedSnapshot { path, disk_bytes },
            gcs_path: "changes_dedup/v2/blobs/sha256_snap".to_string(),
            content_type: "application/octet-stream".to_string(),
            artifact_name: "snap".to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: None,
            completion_tx,
            client_version: None,
            compress: false,
            parent_span: tracing::Span::none(),
            _in_flight: None,
        }
    }
    fn test_queue(
        tx: mpsc::Sender<UploadQueueItem>,
        queue_dir: PathBuf,
        resolver: Arc<dyn TraceExportSource>,
        stats: Arc<UploadQueueStats>,
        max_queue_bytes: u64,
    ) -> UploadQueue {
        UploadQueue {
            tx,
            queue_dir,
            resolver,
            stats,
            max_queue_bytes,
            client_version: None,
            drain_state: Arc::new(Mutex::new(None)),
            inline_fallback_semaphore: Arc::new(tokio::sync::Semaphore::new(
                INLINE_FALLBACK_TOTAL_PERMITS as usize,
            )),
            uploads_in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }
    /// CORE regression: the snapshot is immutable, so mutating the working-tree
    /// source AFTER enqueue does not change the uploaded bytes. This FAILS against
    /// the old verify-then-reupload-source approach (which would stream the new
    /// bytes to the content-addressed `sha256_<expected>` path).
    #[tokio::test]
    async fn reference_snapshot_immutable_to_source_mutation() {
        use axum::{
            Router, body::Bytes, extract::State, http::StatusCode, response::IntoResponse,
            routing::post,
        };
        async fn capture(State(s): State<Arc<Mutex<Vec<u8>>>>, body: Bytes) -> impl IntoResponse {
            *s.lock().unwrap() = body.to_vec();
            let resp = r#"{"bucket":"b","path":"p","size":1,"content_type":"application/octet-stream","generation":1}"#;
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                resp,
            )
        }
        let captured = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/storage", post(capture))
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let resolver: Arc<dyn TraceExportSource> = Arc::new(CountingResolver {
            count: Arc::new(AtomicU32::new(0)),
            proxy_base_url: format!("http://{}/v1", addr),
        });
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("image.bin");
        let original = vec![0xABu8; 4096];
        std::fs::write(&source, &original).unwrap();
        let sha = crate::sha256_hex_from_file(&source, None).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = test_queue(
            tx,
            queue_dir,
            resolver.clone(),
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let result = queue
            .enqueue_file_reference(
                &source,
                &sha,
                "gcs/p",
                "application/octet-stream",
                "dedup_x",
                "sess",
                0,
            )
            .await
            .unwrap();
        let item = rx.recv().await.expect("snapshot enqueued");
        let snapshot_path = item.source.path().to_path_buf();
        std::fs::write(&source, vec![0xFFu8; 4096]).unwrap();
        let consecutive = Arc::new(AtomicU32::new(0));
        process_item(
            item,
            &resolver,
            &UploadRetryPolicy::default(),
            &stats,
            &consecutive,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await;
        assert!(matches!(result.completion_rx.await, Ok(Ok(_))));
        assert_eq!(
            *captured.lock().unwrap(),
            original,
            "uploaded bytes are the immutable snapshot, not the mutated source"
        );
        assert!(source.exists(), "original working-tree source untouched");
        assert!(
            !snapshot_path.exists(),
            "owned snapshot deleted after upload"
        );
        assert_eq!(stats.uploaded.load(Ordering::Relaxed), 1);
    }
    /// Source changed before the snapshot (sim: `expected_sha256` doesn't match
    /// current content) → stale skip: nothing enqueued, completion resolves Err,
    /// `reference_stale` bumps, source preserved, snapshot removed.
    #[tokio::test]
    async fn reference_snapshot_stale_at_enqueue_is_skipped() {
        let (resolver, request_count) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("image.bin");
        std::fs::write(&source, vec![0x11u8; 4096]).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = test_queue(
            tx,
            queue_dir.clone(),
            resolver,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let result = queue
            .enqueue_file_reference(
                &source,
                &"0".repeat(64),
                "gcs/p",
                "application/octet-stream",
                "dedup_x",
                "sess",
                0,
            )
            .await
            .unwrap();
        assert!(
            matches!(result.completion_rx.await, Ok(Err(_))),
            "stale snapshot resolves Err"
        );
        assert!(
            rx.try_recv().is_err(),
            "nothing enqueued for a stale snapshot"
        );
        assert_eq!(stats.reference_stale.load(Ordering::Relaxed), 1);
        assert_eq!(request_count.load(Ordering::SeqCst), 0, "never uploaded");
        assert!(source.exists(), "source preserved");
        let leftover: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert!(leftover.is_empty(), "stale snapshot deleted from queue dir");
    }
    /// The snapshot's bytes equal the source — reflink and copy-fallback both
    /// produce correct content regardless of FS support.
    #[tokio::test]
    async fn reference_snapshot_content_matches_source() {
        let (resolver, _rc) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("image.bin");
        let bytes: Vec<u8> = (0u32..5000).map(|i| i as u8).collect();
        std::fs::write(&source, &bytes).unwrap();
        let sha = crate::sha256_hex_from_file(&source, None).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = test_queue(tx, queue_dir, resolver, stats, DEFAULT_MAX_QUEUE_BYTES);
        queue
            .enqueue_file_reference(
                &source,
                &sha,
                "gcs/p",
                "application/octet-stream",
                "dedup_x",
                "sess",
                0,
            )
            .await
            .unwrap();
        let item = rx.recv().await.expect("snapshot enqueued");
        assert_eq!(
            std::fs::read(item.source.path()).unwrap(),
            bytes,
            "snapshot bytes equal the source"
        );
    }
    /// A reflink snapshot (`disk_bytes == 0`) contributes 0 to the budget gauge:
    /// `process_item` subtracts 0, leaving `pending_bytes` at its primed value.
    #[tokio::test]
    async fn owned_snapshot_reflink_zero_disk_bytes_not_budget_counted() {
        let (resolver, _rc) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let snap = temp.path().join("snap.bin");
        std::fs::write(&snap, vec![0x11u8; 4096]).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        stats.pending_bytes.store(7_000, Ordering::Relaxed);
        let consecutive = Arc::new(AtomicU32::new(0));
        let item = owned_snapshot_item(snap, 0, None);
        process_item(
            item,
            &resolver,
            &UploadRetryPolicy::default(),
            &stats,
            &consecutive,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await;
        assert_eq!(stats.uploaded.load(Ordering::Relaxed), 1);
        assert_eq!(
            stats.pending_bytes.load(Ordering::Relaxed),
            7_000,
            "reflink snapshot subtracts 0 disk bytes"
        );
    }
    /// A copy-fallback snapshot (`disk_bytes == size`) IS counted: `process_item`
    /// subtracts exactly its `disk_bytes`. The real copy-fallback BRANCH in
    /// `enqueue_file_reference` (`reflink_or_copy` → `Ok(Some(n))`) only fires on a
    /// non-CoW FS, which the test FS isn't; this construction-shortcut test is the
    /// deterministic coverage for that branch's accounting.
    #[tokio::test]
    async fn owned_snapshot_copy_disk_bytes_counted() {
        let (resolver, _rc) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let snap = temp.path().join("snap.bin");
        std::fs::write(&snap, vec![0x11u8; 4096]).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        stats.pending_bytes.store(4_096, Ordering::Relaxed);
        let consecutive = Arc::new(AtomicU32::new(0));
        let item = owned_snapshot_item(snap, 4_096, None);
        process_item(
            item,
            &resolver,
            &UploadRetryPolicy::default(),
            &stats,
            &consecutive,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await;
        assert_eq!(
            stats.pending_bytes.load(Ordering::Relaxed),
            0,
            "copy snapshot subtracts its disk bytes"
        );
    }
    /// `check_snapshot` keeps the three outcomes distinct: a match → `Match`; a
    /// hash mismatch and a missing (NotFound) snapshot → `Stale`; and a transient
    /// read error (reading a directory as a file, non-NotFound on Linux/macOS) →
    /// `Io`. Mutation-resistant: collapsing `Io` into `Stale` (`Io` is mapped
    /// to `failed`, not `reference_stale`) fails this test.
    #[test]
    fn check_snapshot_classifies_io_distinct_from_stale() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("blob.bin");
        std::fs::write(&file, vec![0x11u8; 4096]).unwrap();
        let sha = crate::sha256_hex_from_file(&file, None).unwrap();
        assert!(matches!(check_snapshot(&file, &sha), SnapshotCheck::Match));
        assert!(matches!(
            check_snapshot(&file, &"0".repeat(64)),
            SnapshotCheck::Stale
        ));
        assert!(matches!(
            check_snapshot(&temp.path().join("gone.bin"), &sha),
            SnapshotCheck::Stale
        ));
        assert!(matches!(
            check_snapshot(temp.path(), &sha),
            SnapshotCheck::Io(_)
        ));
    }
    /// `snapshot_route` gates ONLY over-budget real copies: a reflink
    /// (`disk_bytes == 0`) always queues even when over budget; an under-budget
    /// copy queues; only an over-budget copy routes to the inline fallback.
    #[test]
    fn snapshot_route_gates_only_over_budget_copies() {
        assert_eq!(snapshot_route(0, true), SnapshotRoute::Queue);
        assert_eq!(snapshot_route(0, false), SnapshotRoute::Queue);
        assert_eq!(snapshot_route(4096, false), SnapshotRoute::Queue);
        assert_eq!(snapshot_route(4096, true), SnapshotRoute::InlineFallback);
    }
    /// On a CLOSED channel `enqueue_file_reference` falls back to a bounded inline
    /// upload of the owned snapshot (mirrors `enqueue_file`): completion resolves
    /// Ok, `enqueue_fallbacks` bumps, the snapshot is deleted, source preserved.
    #[tokio::test]
    async fn enqueue_file_reference_channel_closed_falls_back_inline() {
        let (resolver, request_count) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("image.bin");
        std::fs::write(&source, vec![0x11u8; 1024]).unwrap();
        let sha = crate::sha256_hex_from_file(&source, None).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        drop(rx);
        let queue = test_queue(
            tx,
            queue_dir.clone(),
            resolver,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let result = queue
            .enqueue_file_reference(
                &source,
                &sha,
                "gcs/p",
                "application/octet-stream",
                "dedup_x",
                "sess",
                0,
            )
            .await
            .unwrap();
        assert!(
            matches!(result.completion_rx.await, Ok(Ok(_))),
            "closed channel falls back to a successful inline upload"
        );
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            1,
            "streamed inline once"
        );
        assert_eq!(stats.enqueue_fallbacks.load(Ordering::Relaxed), 1);
        assert!(source.exists(), "source preserved");
        let leftover: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert!(leftover.is_empty(), "snapshot deleted by inline fallback");
        assert_eq!(
            stats.pending_bytes.load(Ordering::Relaxed),
            0,
            "gauge rolled back"
        );
    }
    /// On a FULL channel `enqueue_file_reference` also falls back to a bounded
    /// inline upload (never blocks or drops).
    #[tokio::test]
    async fn enqueue_file_reference_channel_full_falls_back_inline() {
        let (resolver, request_count) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("image.bin");
        std::fs::write(&source, vec![0x11u8; 1024]).unwrap();
        let sha = crate::sha256_hex_from_file(&source, None).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(owned_snapshot_item(temp.path().join("dummy.bin"), 0, None))
            .expect("first send fills the single slot");
        let queue = test_queue(
            tx,
            queue_dir.clone(),
            resolver,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let result = queue
            .enqueue_file_reference(
                &source,
                &sha,
                "gcs/p",
                "application/octet-stream",
                "dedup_x",
                "sess",
                0,
            )
            .await
            .unwrap();
        assert!(
            matches!(result.completion_rx.await, Ok(Ok(_))),
            "full channel falls back to a successful inline upload"
        );
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            1,
            "streamed inline once"
        );
        assert_eq!(stats.enqueue_fallbacks.load(Ordering::Relaxed), 1);
        assert!(source.exists(), "source preserved");
        let leftover: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert!(
            leftover.is_empty(),
            "reference snapshot deleted by inline fallback"
        );
    }
    /// Real enqueue → process round-trip: `pending_bytes` adds exactly the
    /// snapshot's `disk_bytes` at enqueue and subtracts it at completion, back to
    /// baseline (0). FS-independent — ties the add to the recorded disk_bytes
    /// whether the test FS reflinks (0) or copies (size).
    #[tokio::test]
    async fn reference_enqueue_process_pending_bytes_round_trip() {
        let (resolver, _rc) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("image.bin");
        std::fs::write(&source, vec![0x11u8; 4096]).unwrap();
        let sha = crate::sha256_hex_from_file(&source, None).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = test_queue(
            tx,
            queue_dir,
            resolver.clone(),
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        queue
            .enqueue_file_reference(
                &source,
                &sha,
                "gcs/p",
                "application/octet-stream",
                "dedup_x",
                "sess",
                0,
            )
            .await
            .unwrap();
        let item = rx.recv().await.expect("snapshot enqueued");
        let disk_bytes = match &item.source {
            UploadSource::OwnedSnapshot { disk_bytes, .. } => *disk_bytes,
            other => panic!("expected OwnedSnapshot, got {:?}", other.path()),
        };
        assert_eq!(
            stats.pending_bytes.load(Ordering::Relaxed),
            disk_bytes,
            "enqueue added exactly the snapshot's disk_bytes"
        );
        let consecutive = Arc::new(AtomicU32::new(0));
        process_item(
            item,
            &resolver,
            &UploadRetryPolicy::default(),
            &stats,
            &consecutive,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await;
        assert_eq!(
            stats.pending_bytes.load(Ordering::Relaxed),
            0,
            "completion subtracted disk_bytes back to baseline"
        );
        assert_eq!(stats.pending.load(Ordering::Relaxed), 0);
    }
    /// A missing source at enqueue surfaces as `Err` (the stat fails) — no
    /// snapshot is created.
    #[tokio::test]
    async fn enqueue_file_reference_missing_source_errors() {
        let (resolver, _rc) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let missing = temp.path().join("gone.bin");
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = test_queue(
            tx,
            queue_dir.clone(),
            resolver,
            stats,
            DEFAULT_MAX_QUEUE_BYTES,
        );
        let err = queue
            .enqueue_file_reference(
                &missing,
                &"0".repeat(64),
                "gcs/p",
                "application/octet-stream",
                "dedup_x",
                "sess",
                0,
            )
            .await;
        assert!(err.is_err(), "missing source must return Err");
        let leftover: Vec<_> = std::fs::read_dir(&queue_dir).unwrap().flatten().collect();
        assert!(
            leftover.is_empty(),
            "no snapshot created for a missing source"
        );
    }
    /// A 0-byte source snapshots and verifies fine (empty-file sha matches).
    #[tokio::test]
    async fn enqueue_file_reference_zero_byte_source_succeeds() {
        let (resolver, _rc) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let source = temp.path().join("empty.bin");
        std::fs::write(&source, b"").unwrap();
        let sha = crate::sha256_hex_from_file(&source, None).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let queue = test_queue(
            tx,
            queue_dir,
            resolver,
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        queue
            .enqueue_file_reference(
                &source,
                &sha,
                "gcs/p",
                "application/octet-stream",
                "dedup_x",
                "sess",
                0,
            )
            .await
            .unwrap();
        let item = rx.recv().await.expect("0-byte snapshot enqueued");
        assert!(matches!(item.source, UploadSource::OwnedSnapshot { .. }));
        assert_eq!(stats.reference_stale.load(Ordering::Relaxed), 0);
        assert_eq!(std::fs::metadata(item.source.path()).unwrap().len(), 0);
    }
    /// A retry-exhausted `process_item` deletes the owned snapshot.
    #[tokio::test]
    async fn process_item_owned_snapshot_failure_deletes_snapshot() {
        use axum::{Router, body::Body, http::StatusCode, response::IntoResponse, routing::post};
        async fn h401(_b: Body) -> impl IntoResponse {
            (StatusCode::UNAUTHORIZED, "no")
        }
        let app = Router::new().route("/v1/storage", post(h401));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let resolver: Arc<dyn TraceExportSource> = Arc::new(CountingResolver {
            count: Arc::new(AtomicU32::new(0)),
            proxy_base_url: format!("http://{}/v1", addr),
        });
        let temp = tempfile::TempDir::new().unwrap();
        let snap = temp.path().join("snap.bin");
        std::fs::write(&snap, vec![0x11u8; 256]).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let consecutive = Arc::new(AtomicU32::new(0));
        let policy = UploadRetryPolicy {
            max_attempts: 5,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            multiplier: 1.0,
            max_age: DEFAULT_MAX_AGE,
            auth_park_probe_interval: DEFAULT_AUTH_PARK_PROBE_INTERVAL,
        };
        let item = owned_snapshot_item(snap.clone(), 0, None);
        process_item(
            item,
            &resolver,
            &policy,
            &stats,
            &consecutive,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await;
        assert_eq!(stats.failed.load(Ordering::Relaxed), 1);
        assert!(!snap.exists(), "snapshot deleted after upload failure");
    }
    /// An expired `process_item` (age-check drop) deletes the owned snapshot.
    #[tokio::test]
    async fn process_item_owned_snapshot_expiry_deletes_snapshot() {
        let (resolver, request_count) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let snap = temp.path().join("snap.bin");
        std::fs::write(&snap, vec![0x11u8; 256]).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let consecutive = Arc::new(AtomicU32::new(0));
        let policy = UploadRetryPolicy {
            max_age: Duration::ZERO,
            ..Default::default()
        };
        let mut item = owned_snapshot_item(snap.clone(), 0, None);
        item.enqueued_at = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        process_item(
            item,
            &resolver,
            &policy,
            &stats,
            &consecutive,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await;
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            0,
            "expired: no upload"
        );
        assert!(!snap.exists(), "snapshot deleted on expiry");
    }
    /// An owned-temp item is deleted after a successful upload.
    #[tokio::test]
    async fn process_item_owned_temp_deleted_after_success() {
        let (resolver, request_count) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let owned = temp.path().join("owned_temp.bin");
        std::fs::write(&owned, vec![0x33u8; 256]).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let consecutive = Arc::new(AtomicU32::new(0));
        let policy = UploadRetryPolicy::default();
        let item = UploadQueueItem {
            source: UploadSource::OwnedTemp(owned.clone()),
            gcs_path: "session/turn_0/owned.bin".to_string(),
            content_type: "application/octet-stream".to_string(),
            artifact_name: "owned".to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: None,
            completion_tx: None,
            client_version: None,
            compress: false,
            parent_span: tracing::Span::none(),
            _in_flight: None,
        };
        process_item(
            item,
            &resolver,
            &policy,
            &stats,
            &consecutive,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await;
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        assert_eq!(stats.uploaded.load(Ordering::Relaxed), 1);
        assert!(!owned.exists(), "owned temp must be deleted after upload");
    }
    /// A successful upload deletes both the temp file and its sidecar.
    #[tokio::test]
    async fn process_item_deletes_sidecar_with_temp_after_success() {
        let (resolver, request_count) = spawn_ok_server().await;
        let temp = tempfile::TempDir::new().unwrap();
        let owned = temp.path().join("owned_temp.bin");
        std::fs::write(&owned, vec![0x44u8; 256]).unwrap();
        let sidecar = sidecar_path_for(&owned);
        std::fs::write(&sidecar, br#"{"schema_version":1}"#).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let consecutive = Arc::new(AtomicU32::new(0));
        let policy = UploadRetryPolicy::default();
        let item = UploadQueueItem {
            source: UploadSource::OwnedTemp(owned.clone()),
            gcs_path: "session/turn_0/owned.bin".to_string(),
            content_type: "application/octet-stream".to_string(),
            artifact_name: "owned".to_string(),
            attempts: 0,
            enqueued_at: Instant::now(),
            sidecar_path: Some(sidecar.clone()),
            completion_tx: None,
            client_version: None,
            compress: false,
            parent_span: tracing::Span::none(),
            _in_flight: None,
        };
        process_item(
            item,
            &resolver,
            &policy,
            &stats,
            &consecutive,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await;
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        assert_eq!(stats.uploaded.load(Ordering::Relaxed), 1);
        assert!(!owned.exists(), "temp deleted after upload");
        assert!(
            !sidecar.exists(),
            "sidecar deleted together with temp after upload"
        );
    }
    /// The orphan sweep deletes lone temp/sidecar files and counts them as
    /// mismatched.
    #[test]
    fn cleanup_orphans_counts_lone_files_as_mismatched() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stale = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
        let ft = filetime::FileTime::from_system_time(stale);
        let lone_tmp = queue_dir.join("aa_turn0_before_changes.tar.gz_1_0");
        std::fs::write(&lone_tmp, b"orphan archive").unwrap();
        filetime::set_file_mtime(&lone_tmp, ft).unwrap();
        let lone_sidecar = queue_dir.join("bb_turn0_after_changes.tar.gz_2_0.meta.json");
        std::fs::write(&lone_sidecar, b"{}").unwrap();
        filetime::set_file_mtime(&lone_sidecar, ft).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let queue = test_queue(
            mpsc::channel(1).0,
            queue_dir.clone(),
            Arc::new(MockResolver),
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        queue.cleanup_orphans(Duration::from_secs(3600));
        assert!(!lone_tmp.exists(), "lone temp swept");
        assert!(!lone_sidecar.exists(), "lone sidecar swept");
        assert_eq!(
            stats.cleanup_orphan_mismatched.load(Ordering::Relaxed),
            2,
            "both lone files counted as mismatched"
        );
    }
    /// A stale matched temp+sidecar pair is swept but not counted as mismatched.
    #[test]
    fn cleanup_orphans_does_not_count_matched_pair() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let stale = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
        let ft = filetime::FileTime::from_system_time(stale);
        let tmp = queue_dir.join("cc_turn1_before_changes.tar.gz_3_0");
        std::fs::write(&tmp, b"paired archive").unwrap();
        filetime::set_file_mtime(&tmp, ft).unwrap();
        let sidecar = sidecar_path_for(&tmp);
        std::fs::write(&sidecar, b"{}").unwrap();
        filetime::set_file_mtime(&sidecar, ft).unwrap();
        let stats = Arc::new(UploadQueueStats::new());
        let queue = test_queue(
            mpsc::channel(1).0,
            queue_dir.clone(),
            Arc::new(MockResolver),
            stats.clone(),
            DEFAULT_MAX_QUEUE_BYTES,
        );
        queue.cleanup_orphans(Duration::from_secs(3600));
        assert!(!tmp.exists(), "stale temp removed");
        assert!(!sidecar.exists(), "stale sidecar removed");
        assert_eq!(
            stats.cleanup_orphan_mismatched.load(Ordering::Relaxed),
            0,
            "a matched pair must not be counted as mismatched"
        );
    }
    /// The janitor derives a pair's age from the sidecar's `enqueued_at` (same
    /// source as the recovery scan), falling back to mtime only when no
    /// parseable sidecar exists. mtime and `enqueued_at` disagreeing must not
    /// produce a deletion recovery would have disagreed with.
    #[test]
    fn cleanup_orphans_uses_sidecar_age_for_pairs() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let make_pair = |stem: &str, enqueued_at: chrono::DateTime<chrono::Utc>, old_mtime| {
            let tmp = queue_dir.join(stem);
            std::fs::write(&tmp, b"bytes").unwrap();
            let sidecar = QueueItemSidecar {
                schema_version: 1,
                session_id: "s".to_string(),
                turn_number: 1,
                gcs_path: "s/turn_1/a".to_string(),
                content_type: "application/gzip".to_string(),
                artifact_name: "a".to_string(),
                enqueued_at: enqueued_at.to_rfc3339(),
                sha256: "0".repeat(64),
            };
            let sc = sidecar_path_for(&tmp);
            std::fs::write(&sc, serde_json::to_vec(&sidecar).unwrap()).unwrap();
            if old_mtime {
                let stale = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
                let ft = filetime::FileTime::from_system_time(stale);
                filetime::set_file_mtime(&tmp, ft).unwrap();
                filetime::set_file_mtime(&sc, ft).unwrap();
            }
            (tmp, sc)
        };
        let (keep_tmp, keep_sc) = make_pair("aa_turn1_keep.tar.gz_1_0", chrono::Utc::now(), true);
        let (drop_tmp, drop_sc) = make_pair(
            "bb_turn1_drop.tar.gz_2_0",
            chrono::Utc::now() - chrono::Duration::hours(3),
            false,
        );
        cleanup_queue_dir(&queue_dir, Duration::from_secs(2 * 3600), None);
        assert!(keep_tmp.exists(), "fresh-by-sidecar temp kept");
        assert!(keep_sc.exists(), "fresh-by-sidecar sidecar kept");
        assert!(!drop_tmp.exists(), "expired-by-sidecar temp removed");
        assert!(!drop_sc.exists(), "expired-by-sidecar sidecar removed");
    }
    /// `remove_owned_source` deletes both variants — both are queue-owned (a
    /// working-tree source is snapshotted, never enqueued directly).
    #[test]
    fn remove_owned_source_deletes_both_variants() {
        let temp = tempfile::TempDir::new().unwrap();
        let owned_path = temp.path().join("owned.bin");
        std::fs::write(&owned_path, b"owned").unwrap();
        remove_owned_source(&UploadSource::OwnedTemp(owned_path.clone()), None);
        assert!(!owned_path.exists(), "owned temp should be removed");
        let snap_path = temp.path().join("snap.bin");
        std::fs::write(&snap_path, b"snapshot").unwrap();
        remove_owned_source(
            &UploadSource::OwnedSnapshot {
                path: snap_path.clone(),
                disk_bytes: 0,
            },
            None,
        );
        assert!(!snap_path.exists(), "owned snapshot should be removed");
    }
    /// The orphan sweep only touches the queue dir; a stale working-tree
    /// reference source living outside it is never deleted.
    #[test]
    fn cleanup_orphans_never_deletes_reference_source() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue_dir = temp.path().join("upload_queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        let worktree = temp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let ref_source = worktree.join("image.bin");
        std::fs::write(&ref_source, b"durable working-tree file").unwrap();
        let three_hours_ago = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
        let ft = filetime::FileTime::from_system_time(three_hours_ago);
        filetime::set_file_mtime(&ref_source, ft).unwrap();
        let cleaned = cleanup_orphaned_uploads(temp.path(), Duration::from_secs(3600));
        assert_eq!(cleaned, 0, "nothing in queue_dir to clean");
        assert!(
            ref_source.exists(),
            "a reference source outside queue_dir must never be swept"
        );
    }
}
