//! File watcher for detecting external memory edits.
//!
//! Watches `~/.grok/memory/` for `.md` file changes (create, modify, remove) and accumulates the affected paths.
//! The search path checks [`is_dirty`] before each query and syncs the index for all dirty paths:
//! - **created / modified** files are reindexed via `MemoryIndex::reindex_file`
//! - **deleted** files have their stale chunks removed via `MemoryIndex::delete_path`
//!
//! [`is_dirty`]: MemoryFileWatcher::is_dirty

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwap;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Watches the memory directory for `.md` file changes.
///
/// The notify thread inserts dirty paths via `rcu` and the search path swaps them out, so neither side takes a lock.
/// The separate `dirty` flag lets `is_dirty` answer with one atomic load and no allocation.
pub struct MemoryFileWatcher {
    dirty_files: Arc<ArcSwap<HashSet<PathBuf>>>,
    dirty: Arc<AtomicBool>,
    _watcher: RecommendedWatcher,
}

impl MemoryFileWatcher {
    /// Start watching the given memory directory for `.md` file changes.
    ///
    /// Returns `None` if the watcher fails to initialize, after logging a warning.
    pub fn start(memory_dir: &Path) -> Option<Self> {
        let dirty_files: Arc<ArcSwap<HashSet<PathBuf>>> =
            Arc::new(ArcSwap::new(Arc::new(HashSet::new())));
        let dirty = Arc::new(AtomicBool::new(false));

        let df = dirty_files.clone();
        let d = dirty.clone();

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            let Ok(event) = res else { return };
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {}
                _ => return,
            }
            for path in &event.paths {
                if path.extension().is_some_and(|ext| ext == "md") {
                    let path = path.clone();
                    df.rcu(move |old| {
                        let mut new = (**old).clone();
                        new.insert(path.clone());
                        new
                    });
                    d.store(true, Ordering::Relaxed);
                }
            }
        })
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to create memory file watcher");
        })
        .ok()?;

        watcher
            .watch(memory_dir, RecursiveMode::Recursive)
            .map_err(|e| {
                tracing::warn!(
                    path = %memory_dir.display(),
                    error = %e,
                    "failed to watch memory directory"
                );
            })
            .ok()?;

        tracing::info!(
            path = %memory_dir.display(),
            "memory file watcher started"
        );

        Some(Self {
            dirty_files,
            dirty,
            _watcher: watcher,
        })
    }

    /// True if any files have changed since the last `take_dirty`.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Takes all accumulated dirty paths and resets the dirty state.
    pub fn take_dirty(&self) -> Vec<PathBuf> {
        let old = self.dirty_files.swap(Arc::new(HashSet::new()));
        self.dirty.store(false, Ordering::Relaxed);
        old.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_watcher_starts_on_valid_dir() {
        let tmp = TempDir::new().unwrap();
        // In CI or containers the OS may deny inotify watches (e.g. exhausted fs.inotify.max_user_instances), so a `None` result is fine.
        let _watcher = MemoryFileWatcher::start(tmp.path());
    }

    #[test]
    fn test_watcher_initially_clean() {
        let tmp = TempDir::new().unwrap();
        let Some(watcher) = MemoryFileWatcher::start(tmp.path()) else {
            eprintln!("skipping: could not create file watcher (resource limit?)");
            return;
        };
        assert!(!watcher.is_dirty());
        assert!(watcher.take_dirty().is_empty());
    }

    #[test]
    fn test_watcher_detects_md_file_creation() {
        let tmp = TempDir::new().unwrap();
        let Some(watcher) = MemoryFileWatcher::start(tmp.path()) else {
            eprintln!("skipping: could not create file watcher (resource limit?)");
            return;
        };

        std::fs::write(tmp.path().join("test.md"), "hello").unwrap();

        // Give the watcher time to process (debounce and OS event delivery)
        std::thread::sleep(std::time::Duration::from_millis(500));

        assert!(watcher.is_dirty(), "should detect .md creation");
        let dirty = watcher.take_dirty();
        assert!(!dirty.is_empty(), "should have dirty paths");
        assert!(dirty[0].extension().unwrap() == "md");
    }

    #[test]
    fn test_watcher_ignores_non_md_files() {
        let tmp = TempDir::new().unwrap();
        let Some(watcher) = MemoryFileWatcher::start(tmp.path()) else {
            eprintln!("skipping: could not create file watcher (resource limit?)");
            return;
        };

        // Create a non-.md file
        std::fs::write(tmp.path().join("test.txt"), "hello").unwrap();
        std::fs::write(tmp.path().join("index.sqlite"), "db").unwrap();

        std::thread::sleep(std::time::Duration::from_millis(500));

        assert!(
            !watcher.is_dirty(),
            "should not detect non-.md file changes"
        );
    }

    #[test]
    fn test_take_dirty_resets_state() {
        let tmp = TempDir::new().unwrap();
        let Some(watcher) = MemoryFileWatcher::start(tmp.path()) else {
            eprintln!("skipping: could not create file watcher (resource limit?)");
            return;
        };

        std::fs::write(tmp.path().join("a.md"), "content").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let first = watcher.take_dirty();
        assert!(!first.is_empty());
        assert!(!watcher.is_dirty(), "should be clean after take");
        assert!(
            watcher.take_dirty().is_empty(),
            "second take should be empty"
        );
    }
}
