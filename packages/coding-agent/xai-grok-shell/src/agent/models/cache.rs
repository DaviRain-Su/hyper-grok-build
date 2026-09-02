use super::*;

// ── Disk cache ──────────────────────────────────────────────────────────────

pub(crate) const MODELS_CACHE_FILE: &str = "models_cache.json";
pub(crate) const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Resolve the on-disk models catalog path for production / harness use.
///
/// Honors non-empty `GROK_MODELS_CACHE_PATH` (empty string = unset, for
/// subprocess harnesses). Otherwise `grok_home()/models_cache.json`.
///
/// **Tests must not rely on this for isolation** — pass an explicit path
/// via [`ModelsCacheManager::with_path`] / per-test `TempDir` instead of
/// racing the OnceLock-cached real home.
pub(crate) fn models_cache_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GROK_MODELS_CACHE_PATH")
        && !p.is_empty()
    {
        return std::path::PathBuf::from(p);
    }
    crate::util::grok_home::grok_home().join(MODELS_CACHE_FILE)
}

pub(crate) fn is_fresh(fetched_at: DateTime<Utc>, ttl: std::time::Duration) -> bool {
    let Ok(ttl) = ChronoDuration::from_std(ttl) else {
        return false;
    };
    let age = Utc::now().signed_duration_since(fetched_at);
    age >= ChronoDuration::zero() && age < ttl
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ModelsCache {
    pub(crate) fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) grok_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auth_method: Option<CacheAuthMethod>,
    /// The models-list URL this catalog was fetched from.
    /// `load_fresh` compares it, so a cache written against another backend is a miss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) etag: Option<String>,
    pub(crate) models: IndexMap<String, ModelEntry>,
}

pub(crate) struct CacheResult {
    pub(crate) models: IndexMap<String, ModelEntry>,
    pub(crate) etag: Option<String>,
}

pub(crate) struct ModelsCacheManager {
    pub(crate) path: std::path::PathBuf,
    pub(crate) ttl: std::time::Duration,
}

impl ModelsCacheManager {
    pub(crate) fn new() -> Self {
        Self {
            path: models_cache_path(),
            ttl: CACHE_TTL,
        }
    }

    /// Explicit path (tests / injected home). Prefer over mutating `GROK_HOME`.
    pub(crate) fn with_path(path: std::path::PathBuf) -> Self {
        Self {
            path,
            ttl: CACHE_TTL,
        }
    }

    pub(crate) fn load_fresh(
        &self,
        expected_auth: &CacheAuthMethod,
        expected_origin: &str,
    ) -> Option<CacheResult> {
        let data = std::fs::read(&self.path).ok()?;
        let cache: ModelsCache = serde_json::from_slice(&data).ok()?;
        if cache.grok_version.as_deref() != Some(xai_grok_version::VERSION) {
            tracing::debug!("models cache version mismatch");
            return None;
        }
        if cache.auth_method.as_ref() != Some(expected_auth) {
            tracing::debug!("models cache auth method mismatch");
            return None;
        }
        if cache.origin.as_deref() != Some(expected_origin) {
            tracing::debug!(
                cached = ?cache.origin,
                expected = expected_origin,
                "models cache origin mismatch"
            );
            return None;
        }
        if !is_fresh(cache.fetched_at, self.ttl) {
            tracing::debug!("models cache is stale");
            return None;
        }
        tracing::debug!(count = cache.models.len(), "loaded models from disk cache");
        Some(CacheResult {
            models: cache.models,
            etag: cache.etag,
        })
    }

    pub(crate) fn persist(
        &self,
        models: &IndexMap<String, ModelEntry>,
        etag: Option<&str>,
        auth_method: CacheAuthMethod,
        origin: &str,
    ) {
        let cache = ModelsCache {
            fetched_at: Utc::now(),
            grok_version: Some(xai_grok_version::VERSION.to_string()),
            auth_method: Some(auth_method),
            origin: Some(origin.to_string()),
            etag: etag.map(|s| s.to_string()),
            models: models.clone(),
        };
        self.atomic_write(&cache);
    }

    pub(crate) async fn renew_ttl(&self, expected_auth: &CacheAuthMethod, expected_origin: &str) {
        let data = match tokio::fs::read(&self.path).await {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(error = %e, "models cache TTL renewal: read failed");
                return;
            }
        };
        let Ok(mut cache) = serde_json::from_slice::<ModelsCache>(&data) else {
            return;
        };
        if cache.auth_method.as_ref() != Some(expected_auth) {
            tracing::debug!("models cache TTL renewal skipped: auth method mismatch");
            return;
        }
        if cache.origin.as_deref() != Some(expected_origin) {
            tracing::debug!("models cache TTL renewal skipped: origin mismatch");
            return;
        }
        cache.fetched_at = Utc::now();
        self.atomic_write_async(&cache).await;
        tracing::debug!("models cache TTL renewed");
    }

    pub(crate) fn invalidate(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::info!("models disk cache invalidated"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, "failed to invalidate models disk cache"),
        }
    }

    pub(crate) fn atomic_write(&self, cache: &ModelsCache) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        sweep_stale_tmp(&self.path, self.ttl);
        let Ok(json) = serde_json::to_vec_pretty(cache) else {
            return;
        };
        let tmp = unique_tmp_path(&self.path);
        if std::fs::write(&tmp, &json).is_ok() {
            if std::fs::rename(&tmp, &self.path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub(crate) async fn atomic_write_async(&self, cache: &ModelsCache) {
        if let Some(parent) = self.path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        sweep_stale_tmp(&self.path, self.ttl);
        let Ok(json) = serde_json::to_vec_pretty(cache) else {
            return;
        };
        let tmp = unique_tmp_path(&self.path);
        if tokio::fs::write(&tmp, &json).await.is_ok() {
            if tokio::fs::rename(&tmp, &self.path).await.is_err() {
                let _ = tokio::fs::remove_file(&tmp).await;
            }
        } else {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }
}

pub(crate) fn read_capped(path: &std::path::Path, max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > max {
        tracing::debug!("settings cache exceeds size cap");
        return None;
    }
    Some(buf)
}

pub(crate) fn unique_tmp_path(path: &std::path::Path) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("json.tmp.{}.{n}", std::process::id()))
}

pub(crate) fn sweep_stale_tmp(path: &std::path::Path, ttl: std::time::Duration) {
    let (Some(parent), Some(stem)) = (path.parent(), path.file_name().and_then(|s| s.to_str()))
    else {
        return;
    };
    let prefix = format!("{stem}.tmp.");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let is_stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > ttl);
        if is_stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn write_private_atomic(path: &std::path::Path, ttl: std::time::Duration, bytes: &[u8]) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    sweep_stale_tmp(path, ttl);
    let tmp = unique_tmp_path(path);
    let written = {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .and_then(|mut f| f.write_all(bytes))
                .is_ok()
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, bytes).is_ok()
        }
    };
    if written && std::fs::rename(&tmp, path).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(&tmp);
}
