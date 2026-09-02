use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

pub(crate) struct SessionMemory {
    /// Memory storage handle for writing flush output (None when memory disabled).
    /// Wrapped in `RefCell` to allow `/memory on|off` toggle from `&Arc<SessionActor>`.
    pub storage: RefCell<Option<crate::session::memory::MemoryStorage>>,
    /// Whether to write a session summary to memory on session end.
    pub save_on_end: bool,
    /// `None` when memory is disabled.
    pub backend_params: Option<crate::session::memory::MemoryBackendParams>,
    /// First-turn memory injection behavior resolved from local and remote config.
    pub initial_injection_config: crate::config::MemoryInitialInjectionConfig,
    /// Per-process latch: the first-turn injection decision already ran in this session segment.
    /// Cross-segment idempotency comes from `conversation_has_memory_context`, not this flag.
    pub context_injected: AtomicBool,
    pub flush_config: crate::config::MemoryFlushConfig,
    /// When `true`, auto-compact checks are suppressed during memory flush.
    pub is_flushing: AtomicBool,
    /// The compaction count at which the last flush ran (once-per-cycle guard).
    pub last_flush_compaction: AtomicU64,
    pub flush_count: AtomicU64,
    /// Content from the most recent successful flush, used for delta prompts.
    /// Wrapped in `RefCell` because `SessionActor` is single-threaded (LocalSet).
    pub last_flush_content: RefCell<Option<String>>,
    pub flush_success_count: AtomicU64,
    pub flush_error_count: AtomicU64,
    /// Counts model-initiated `memory_search` tool calls.
    /// Wrapped in `RefCell` to allow `/memory on|off` toggle from `&Arc<SessionActor>`.
    pub search_counter: RefCell<Option<Arc<AtomicU64>>>,
    /// Counts first-turn memory context injections.
    pub injection_count: AtomicU64,
    /// Counts post-compaction memory re-injection searches.
    pub compaction_recovery_count: AtomicU64,
    /// Total memory chunks added across all sources.
    pub chunks_added: Arc<AtomicU64>,
    /// Handle to the startup reindex+embed task, taken and awaited by the launch dream. `None`
    /// once taken, or when memory was not indexed at launch.
    pub init_reindex_handle: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// autoDream consolidation config.
    pub dream_config: crate::config::MemoryDreamConfig,
    pub dream_count: AtomicU64,
    pub dream_success_count: AtomicU64,
    pub dream_error_count: AtomicU64,
}

impl SessionMemory {
    pub(crate) fn is_enabled(&self) -> bool {
        self.storage.borrow().is_some()
    }

    /// Clone the storage out of the `RefCell`, dropping the borrow immediately.
    pub(crate) fn storage(&self) -> Option<crate::session::memory::MemoryStorage> {
        self.storage.borrow().clone()
    }

    /// Returns `true` if acquired, `false` if another flush is already in progress.
    pub(crate) fn try_acquire_flush_lock(&self) -> bool {
        self.is_flushing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    }

    pub(crate) fn release_flush_lock(&self) {
        self.is_flushing
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a flush result and increment the appropriate counter.
    ///
    /// "written" increments success, "error" increments error.
    /// Anything else ("nothing_to_store", "rejected") increments only the total flush count.
    pub(crate) fn record_flush_result(&self, outcome: &str) {
        self.flush_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match outcome {
            "written" => {
                self.flush_success_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            "error" => {
                self.flush_error_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub(crate) fn record_dream_result(&self, success: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        self.dream_count.fetch_add(1, Relaxed);
        if success {
            self.dream_success_count.fetch_add(1, Relaxed);
        } else {
            self.dream_error_count.fetch_add(1, Relaxed);
        }
    }

    /// Record a neutral dream outcome (nothing to consolidate / skipped).
    pub(crate) fn record_dream_neutral(&self) {
        self.dream_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Open (or create) the memory index for the current workspace.
    pub(crate) fn open_index(
        &self,
        storage: &crate::session::memory::MemoryStorage,
    ) -> Option<crate::session::memory::MemoryIndex> {
        let embed_dims = self
            .backend_params
            .as_ref()
            .and_then(|p| p.embed_config.as_ref())
            .map_or(1024, |c| c.dimensions);
        let db_path = storage.workspace_dir().join("index.sqlite");
        crate::session::memory::MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            Default::default(),
            embed_dims,
        )
        .ok()
    }

    /// Await the startup reindex+embed task if it is still tracked; `None` returns at once.
    pub(crate) async fn await_init_reindex(&self) {
        let handle = self.init_reindex_handle.borrow_mut().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Reindex a file and embed new chunks when embedding is configured.
    pub(crate) async fn reindex_and_embed(&self, path: &std::path::Path, source: &str) {
        let Some(storage) = self.storage.borrow().clone() else {
            return;
        };
        if let Some(mut index) = self.open_index(&storage) {
            let _ = index.reindex_file(path, source);
            if let Some(ref params) = self.backend_params
                && let Some(provider) = params.make_embedding_provider().await
            {
                crate::session::memory::embed_missing_chunks(&index, &provider).await;
            }
        }
    }

    /// Remove chunks for the given file paths from the search index.
    ///
    /// Used after dream consolidation deletes processed session files so that stale chunks don't linger in the index.
    /// Best-effort: errors are logged but don't propagate.
    pub(crate) fn delete_paths_from_index(&self, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let Some(storage) = self.storage.borrow().clone() else {
            return;
        };
        if let Some(mut index) = self.open_index(&storage) {
            let mut total_removed = 0usize;
            for path in paths {
                match index.delete_path(path) {
                    Ok(n) => total_removed += n,
                    Err(e) => {
                        tracing::warn!(
                            target: xai_grok_telemetry::memory_log::TARGET,
                            path = %path.display(),
                            error = %e,
                            "DREAM_CLEANUP: failed to remove chunks from index"
                        );
                    }
                }
            }
            if total_removed > 0 {
                tracing::info!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    chunks_removed = total_removed,
                    files = paths.len(),
                    "DREAM_CLEANUP: removed stale chunks from index"
                );
            }
        }
    }

    /// Collect telemetry counters for session-end summary.
    pub(crate) fn telemetry_snapshot(&self) -> MemoryTelemetry {
        use std::sync::atomic::Ordering::Relaxed;
        MemoryTelemetry {
            flush_count: self.flush_count.load(Relaxed),
            flush_success_count: self.flush_success_count.load(Relaxed),
            flush_error_count: self.flush_error_count.load(Relaxed),
            tool_search_count: self
                .search_counter
                .borrow()
                .as_ref()
                .map_or(0, |c| c.load(Relaxed)),
            injection_count: self.injection_count.load(Relaxed),
            compaction_recovery_count: self.compaction_recovery_count.load(Relaxed),
            chunks_added: self.chunks_added.load(Relaxed),
            dream_count: self.dream_count.load(Relaxed),
            dream_success_count: self.dream_success_count.load(Relaxed),
            dream_error_count: self.dream_error_count.load(Relaxed),
        }
    }
}

/// Snapshot of memory telemetry counters for session-end logging.
pub(crate) struct MemoryTelemetry {
    pub flush_count: u64,
    pub flush_success_count: u64,
    pub flush_error_count: u64,
    pub tool_search_count: u64,
    pub injection_count: u64,
    pub compaction_recovery_count: u64,
    pub chunks_added: u64,
    pub dream_count: u64,
    pub dream_success_count: u64,
    pub dream_error_count: u64,
}
