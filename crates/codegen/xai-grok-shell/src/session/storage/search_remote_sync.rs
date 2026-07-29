//! GCS-based remote index sync for session search.
//!
//! On bootstrap completion, compresses `session_search.sqlite` with zstd
//! and uploads to GCS (async, fire-and-forget, debounced to at most once
//! per hour). On startup, if the local index is stale (missing, no
//! `last_bootstrap_at`, or `last_bootstrap_at` > 1 hour older than remote),
//! downloads and decompresses the remote index before running incremental
//! bootstrap.
//!
//! Gated behind `RemoteSyncConfig::enabled` (default false).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use super::search_fts::SessionSearchIndex;

/// GCS bucket for session search index sync (same as session traces);
/// `None` makes remote sync a no-op.
const SEARCH_INDEX_BUCKET: Option<&str> = crate::upload::gcs::SESSION_TRACES_BUCKET;

/// GCS object name for the compressed index.
const REMOTE_INDEX_OBJECT: &str = "session_search.sqlite.zst";

/// Zstd compression level (balance of speed vs ratio).
const ZSTD_COMPRESSION_LEVEL: i32 = 3;

/// Minimum interval between uploads (1 hour).
const UPLOAD_DEBOUNCE: Duration = Duration::from_secs(3600);

/// Staleness threshold: if local `last_bootstrap_at` is more than this
/// duration older than the remote object's timestamp, download the remote.
const STALENESS_THRESHOLD: Duration = Duration::from_secs(3600);

/// SQLite meta key for the last successful bootstrap timestamp (unix secs).
const META_KEY_LAST_BOOTSTRAP: &str = "last_bootstrap_at";

// Configuration

/// Configuration for remote index sync.
///
/// Parsed from `[session_search.remote_sync]` in `~/.grok/config.toml`.
/// Default: disabled.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct RemoteSyncConfig {
    /// Whether remote sync is enabled.
    pub enabled: bool,
    /// GCS prefix for the remote index (directory structure in the bucket).
    /// Defaults to `"session_search_index"`.
    pub gcs_prefix: String,
}

impl Default for RemoteSyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gcs_prefix: "session_search_index".to_string(),
        }
    }
}

// Debounce state (global, per-process)

/// Unix timestamp (seconds) of the last successful upload.
/// `0` means no upload has occurred this process lifetime.
static LAST_UPLOAD_AT: AtomicI64 = AtomicI64::new(0);

/// Returns true if enough time has passed since the last upload.
fn upload_debounce_ok() -> bool {
    let last = LAST_UPLOAD_AT.load(Ordering::Relaxed);
    if last == 0 {
        return true;
    }
    let now = chrono::Utc::now().timestamp();
    (now - last) >= UPLOAD_DEBOUNCE.as_secs() as i64
}

/// Record that an upload just completed.
fn record_upload() {
    LAST_UPLOAD_AT.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
}

// Compression / decompression

/// Compress `src` with zstd level 3, writing to `dst`.
fn compress_file(src: &Path, dst: &Path) -> io::Result<u64> {
    let input = std::fs::File::open(src)?;
    let output = std::fs::File::create(dst)?;
    let mut encoder = zstd::Encoder::new(output, ZSTD_COMPRESSION_LEVEL)?;
    let bytes = io::copy(&mut io::BufReader::new(input), &mut encoder)?;
    encoder.finish()?;
    Ok(bytes)
}

/// Decompress a zstd-compressed file from `src` to `dst`.
fn decompress_file(src: &Path, dst: &Path) -> io::Result<u64> {
    let input = std::fs::File::open(src)?;
    let mut decoder = zstd::Decoder::new(input)?;
    let output = std::fs::File::create(dst)?;
    let bytes = io::copy(&mut decoder, &mut io::BufWriter::new(output))?;
    Ok(bytes)
}

// Staleness check

/// Read `last_bootstrap_at` from the sqlite meta table.
///
/// Returns `None` if the DB doesn't exist, can't be opened, or the key
/// is missing.
pub fn read_last_bootstrap_at(db_path: &Path) -> Option<i64> {
    if !db_path.exists() {
        return None;
    }
    let index = SessionSearchIndex::open_or_create(db_path).ok()?;
    index
        .get_meta(META_KEY_LAST_BOOTSTRAP)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
}

/// Like [`read_last_bootstrap_at`] but preserves read failures, so callers
/// can tell "marker genuinely absent" apart from "could not read the DB"
/// (transient busy/locked/I/O). A missing DB file is a true absence, not an
/// error.
pub fn try_read_last_bootstrap_at(db_path: &Path) -> Result<Option<i64>, String> {
    if !db_path.exists() {
        return Ok(None);
    }
    let index = SessionSearchIndex::open_or_create(db_path).map_err(|e| e.to_string())?;
    let value = index
        .get_meta(META_KEY_LAST_BOOTSTRAP)
        .map_err(|e| e.to_string())?;
    Ok(value.and_then(|v| v.parse::<i64>().ok()))
}

/// Write `last_bootstrap_at` into the sqlite meta table.
pub fn write_last_bootstrap_at(db_path: &Path) -> io::Result<()> {
    let index =
        SessionSearchIndex::open_existing(db_path).map_err(|e| io::Error::other(e.to_string()))?;
    let now = chrono::Utc::now().timestamp();
    index
        .set_meta(META_KEY_LAST_BOOTSTRAP, &now.to_string())
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Remove the completed-bootstrap marker.
pub fn clear_last_bootstrap_at(db_path: &Path) -> io::Result<()> {
    let index =
        SessionSearchIndex::open_existing(db_path).map_err(|e| io::Error::other(e.to_string()))?;
    index
        .delete_meta(META_KEY_LAST_BOOTSTRAP)
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Determine whether the local index is stale enough to warrant downloading
/// the remote copy.
///
/// Returns `true` if:
/// - The local DB file doesn't exist, or
/// - There is no `last_bootstrap_at` in the meta table, or
/// - `last_bootstrap_at` is more than [`STALENESS_THRESHOLD`] old compared
///   to `remote_timestamp_unix` (0 if unknown — always stale).
pub fn is_local_stale(db_path: &Path, remote_timestamp_unix: i64) -> bool {
    let Some(local_ts) = read_last_bootstrap_at(db_path) else {
        return true; // no local timestamp → stale
    };
    if remote_timestamp_unix == 0 {
        // Remote timestamp unknown; if we have a local bootstrap, trust it.
        return false;
    }
    (remote_timestamp_unix - local_ts) > STALENESS_THRESHOLD.as_secs() as i64
}

// GCS object path helpers

fn gcs_object_path(config: &RemoteSyncConfig) -> String {
    format!("{}/{}", config.gcs_prefix, REMOTE_INDEX_OBJECT)
}

// Upload (fire-and-forget, debounced)

/// Compress and upload the local search index to GCS.
///
/// This is a fire-and-forget operation: errors are logged but not
/// propagated. The upload is debounced to at most once per hour.
///
/// Called after bootstrap completion when remote sync is enabled.
pub async fn maybe_upload_index(
    db_path: PathBuf,
    config: RemoteSyncConfig,
    gcs_config: xai_file_utils::TraceExportConfig,
    auth_manager: Option<std::sync::Arc<crate::auth::AuthManager>>,
) {
    if !config.enabled {
        return;
    }
    if !upload_debounce_ok() {
        tracing::debug!("skipping search index upload (debounce)");
        return;
    }
    if !db_path.exists() {
        tracing::debug!("skipping search index upload (no local DB)");
        return;
    }

    tokio::spawn(async move {
        if let Err(e) = upload_index_inner(&db_path, &config, &gcs_config, auth_manager).await {
            tracing::warn!(error = %e, "search index GCS upload failed");
        }
    });
}

async fn upload_index_inner(
    db_path: &Path,
    config: &RemoteSyncConfig,
    gcs_config: &xai_file_utils::TraceExportConfig,
    auth_manager: Option<std::sync::Arc<crate::auth::AuthManager>>,
) -> io::Result<()> {
    let db_path = db_path.to_path_buf();
    let compressed_path = db_path.with_extension("sqlite.zst.tmp");

    // Compress on blocking thread
    let src = db_path.clone();
    let dst = compressed_path.clone();
    let original_size = tokio::task::spawn_blocking(move || -> io::Result<u64> {
        let size = compress_file(&src, &dst)?;
        Ok(size)
    })
    .await
    .map_err(io::Error::other)??;

    // Read compressed bytes
    let compressed_bytes = tokio::fs::read(&compressed_path).await?;
    let compressed_size = compressed_bytes.len() as u64;

    // Upload to GCS
    let object_path = gcs_object_path(config);
    let upload_config = crate::upload::gcs::WithAuth::with_auth(gcs_config, auth_manager);
    match xai_file_utils::gcs::upload_bytes(
        &upload_config,
        &object_path,
        &compressed_bytes,
        "application/zstd",
    )
    .await
    {
        Ok(_url) => {
            record_upload();
            tracing::info!(
                original_bytes = original_size,
                compressed_bytes = compressed_size,
                object_path = %object_path,
                "search index uploaded to GCS"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "GCS upload_bytes failed for search index");
        }
    }

    // Clean up temp file
    let _ = tokio::fs::remove_file(&compressed_path).await;

    Ok(())
}

// Download (on startup, if stale)

/// Check the remote index and download it if the local copy is stale.
///
/// Called before bootstrap when remote sync is enabled. If the remote
/// index is newer, it replaces the local `session_search.sqlite`.
///
/// Returns `true` if a remote index was downloaded and installed.
pub async fn maybe_download_index(
    db_path: &Path,
    config: &RemoteSyncConfig,
    gcs_config: &xai_file_utils::TraceExportConfig,
    auth_manager: Option<std::sync::Arc<crate::auth::AuthManager>>,
) -> bool {
    if !config.enabled {
        return false;
    }

    match download_index_inner(db_path, config, gcs_config, auth_manager).await {
        Ok(downloaded) => downloaded,
        Err(e) => {
            tracing::warn!(error = %e, "search index GCS download failed, falling back to local bootstrap");
            false
        }
    }
}

async fn download_index_inner(
    db_path: &Path,
    config: &RemoteSyncConfig,
    gcs_config: &xai_file_utils::TraceExportConfig,
    auth_manager: Option<std::sync::Arc<crate::auth::AuthManager>>,
) -> io::Result<bool> {
    let object_path = gcs_object_path(config);

    // TODO: Implement GCS object metadata check (HEAD request) to get
    // the remote object's last-modified timestamp. For now, use 0 which
    // means "unknown" — `is_local_stale` will return false if we have a
    // local bootstrap timestamp.
    //
    // When implemented, this should use the GCS JSON API:
    // GET https://storage.googleapis.com/storage/v1/b/{bucket}/o/{object}
    // to retrieve the `updated` field as the remote timestamp.
    let remote_timestamp: i64 = 0;

    if !is_local_stale(db_path, remote_timestamp) {
        tracing::debug!("local search index is fresh, skipping remote download");
        return Ok(false);
    }

    tracing::info!(
        object_path = %object_path,
        "local search index is stale, downloading from GCS"
    );

    // Download compressed index via GCS
    // TODO: Use a proper GCS download API. For now, we attempt to
    // construct a download URL and fetch via reqwest. This works for
    // publicly readable buckets or when the user has ambient GCP
    // credentials. For proxy-mode setups, a download-via-proxy helper
    // would be needed (the existing upload helpers don't have a download
    // counterpart).
    let Some(bucket) = SEARCH_INDEX_BUCKET else {
        tracing::debug!("no search index bucket compiled in, skipping remote download");
        return Ok(false);
    };
    let download_url = format!(
        "https://storage.googleapis.com/storage/v1/b/{}/o/{}?alt=media",
        bucket,
        urlencoding::encode(&object_path),
    );

    let upload_config = crate::upload::gcs::WithAuth::with_auth(gcs_config, auth_manager);

    // Try download via reqwest with proxy credentials if available.
    // This is a best-effort path — if the bucket requires auth and
    // we don't have the right credentials, it will fail gracefully.
    let client = upload_config.proxy_http_client().unwrap_or_default();

    let response = client
        .get(&download_url)
        .send()
        .await
        .map_err(|e| io::Error::other(format!("GCS download request failed: {e}")))?;

    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "GCS download returned HTTP {}: object may not exist yet",
            response.status()
        )));
    }

    let compressed_bytes = response
        .bytes()
        .await
        .map_err(|e| io::Error::other(format!("GCS download body read failed: {e}")))?;

    // Write compressed data to temp file and decompress
    let compressed_path = db_path.with_extension("sqlite.zst.download");
    tokio::fs::write(&compressed_path, &compressed_bytes).await?;

    let src = compressed_path.clone();
    let dst = db_path.to_path_buf();
    let dst_tmp = db_path.with_extension("sqlite.remote.tmp");
    let dst_final = dst.clone();

    tokio::task::spawn_blocking(move || -> io::Result<()> {
        // Decompress to a temp file first, then atomically rename
        decompress_file(&src, &dst_tmp)?;
        // Atomic rename to avoid partial-file issues
        std::fs::rename(&dst_tmp, &dst_final)?;
        Ok(())
    })
    .await
    .map_err(io::Error::other)??;

    // Clean up compressed download
    let _ = tokio::fs::remove_file(&compressed_path).await;

    tracing::info!(
        compressed_bytes = compressed_bytes.len(),
        "search index downloaded and installed from GCS"
    );

    Ok(true)
}

// Proxy helpers

/// Extension trait on `TraceExportConfigWithAuth` to expose `proxy_http_client`
/// for download. This works because the upload gcs module already implements
/// `StorageConfig` for the wrapper type.
trait ProxyHttpClient {
    fn proxy_http_client(&self) -> Option<reqwest::Client>;
}

impl ProxyHttpClient for crate::upload::gcs::TraceExportConfigWithAuth {
    fn proxy_http_client(&self) -> Option<reqwest::Client> {
        <Self as xai_file_utils::gcs::StorageConfig>::proxy_http_client(self)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_sync_config_default() {
        let config = RemoteSyncConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.gcs_prefix, "session_search_index");
    }

    #[test]
    fn test_remote_sync_config_deserialize() {
        let toml_str = r#"
            enabled = true
            gcs_prefix = "custom/prefix"
        "#;
        let config: RemoteSyncConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.gcs_prefix, "custom/prefix");
    }

    #[test]
    fn test_gcs_object_path() {
        let config = RemoteSyncConfig {
            enabled: true,
            gcs_prefix: "my_prefix".to_string(),
        };
        assert_eq!(
            gcs_object_path(&config),
            "my_prefix/session_search.sqlite.zst"
        );
    }

    #[test]
    fn test_compress_decompress_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let original = tmp.path().join("original.db");
        let compressed = tmp.path().join("compressed.zst");
        let decompressed = tmp.path().join("decompressed.db");

        // Write test data
        let test_data = b"Hello, this is test data for zstd compression roundtrip!";
        std::fs::write(&original, test_data).unwrap();

        // Compress
        let bytes_read = compress_file(&original, &compressed).unwrap();
        assert!(bytes_read > 0);
        assert!(compressed.exists());

        // Verify compressed file is different from original
        let compressed_bytes = std::fs::read(&compressed).unwrap();
        assert_ne!(&compressed_bytes[..], &test_data[..]);

        // Decompress
        let bytes_written = decompress_file(&compressed, &decompressed).unwrap();
        assert_eq!(bytes_written, test_data.len() as u64);

        // Verify roundtrip
        let result = std::fs::read(&decompressed).unwrap();
        assert_eq!(&result[..], &test_data[..]);
    }

    #[test]
    fn test_compress_large_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let original = tmp.path().join("large.db");
        let compressed = tmp.path().join("large.zst");

        // 1 MB of repeated data (should compress well)
        let test_data: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();
        std::fs::write(&original, &test_data).unwrap();

        compress_file(&original, &compressed).unwrap();

        let original_size = std::fs::metadata(&original).unwrap().len();
        let compressed_size = std::fs::metadata(&compressed).unwrap().len();

        // Repeated data should compress significantly
        assert!(
            compressed_size < original_size / 2,
            "compressed ({compressed_size}) should be much smaller than original ({original_size})"
        );
    }

    #[test]
    fn test_is_local_stale_no_db() {
        // No DB file → stale
        assert!(is_local_stale(Path::new("/nonexistent/db.sqlite"), 0));
    }

    #[test]
    fn test_is_local_stale_no_meta() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("session_search.sqlite");

        // Create DB without last_bootstrap_at
        let _index = SessionSearchIndex::open_or_create(&db_path).unwrap();

        // No bootstrap timestamp → stale
        assert!(is_local_stale(&db_path, 100));
    }

    #[test]
    fn test_is_local_stale_fresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("session_search.sqlite");

        let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
        let now = chrono::Utc::now().timestamp();
        index
            .set_meta(META_KEY_LAST_BOOTSTRAP, &now.to_string())
            .unwrap();

        // Remote timestamp is only 10 seconds ahead → not stale
        assert!(!is_local_stale(&db_path, now + 10));
    }

    #[test]
    fn test_is_local_stale_old() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("session_search.sqlite");

        let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
        let old_ts = chrono::Utc::now().timestamp() - 7200; // 2 hours ago
        index
            .set_meta(META_KEY_LAST_BOOTSTRAP, &old_ts.to_string())
            .unwrap();

        // Remote is 2 hours newer → stale
        let remote_ts = chrono::Utc::now().timestamp();
        assert!(is_local_stale(&db_path, remote_ts));
    }

    #[test]
    fn test_is_local_stale_remote_unknown() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("session_search.sqlite");

        let index = SessionSearchIndex::open_or_create(&db_path).unwrap();
        let now = chrono::Utc::now().timestamp();
        index
            .set_meta(META_KEY_LAST_BOOTSTRAP, &now.to_string())
            .unwrap();

        // Remote timestamp 0 (unknown) with local bootstrap → not stale
        assert!(!is_local_stale(&db_path, 0));
    }

    #[test]
    fn test_upload_debounce_initial() {
        // On fresh process start (LAST_UPLOAD_AT == 0), debounce allows upload
        // Note: can't reset the static in tests, but initial 0 → true
        assert!(upload_debounce_ok());
    }

    #[test]
    fn test_read_write_last_bootstrap_at() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("session_search.sqlite");

        // Before writing, should be None
        assert_eq!(read_last_bootstrap_at(&db_path), None);

        // Create DB and write timestamp
        write_last_bootstrap_at(&db_path).unwrap();

        // Should now have a reasonable timestamp
        let ts = read_last_bootstrap_at(&db_path).unwrap();
        let now = chrono::Utc::now().timestamp();
        assert!(
            (now - ts).abs() < 5,
            "timestamp should be within 5 seconds of now"
        );
    }
}
