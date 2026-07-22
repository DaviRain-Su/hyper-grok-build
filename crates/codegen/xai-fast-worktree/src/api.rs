//! Public API for fast worktree creation.
//!
//! This module provides a higher-level, explicit API (builder + enums) that makes
//! behavior clear (what to copy, whether to copy ignored files, and how to finalize).
//!

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Serializes tests that chdir or assert process-CWD scan results (process-global cwd).
/// Gated on `metadata` because every caller lives under that feature's test modules
/// (`gc` / `auto_gc`); without the feature these would be dead under `-D warnings`.
#[cfg(all(test, feature = "metadata"))]
pub(crate) static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, feature = "metadata"))]
pub(crate) fn cwd_test_guard() -> std::sync::MutexGuard<'static, ()> {
    CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Restores process cwd on drop (pair with [`cwd_test_guard`]).
#[cfg(all(test, feature = "metadata"))]
pub(crate) struct CwdGuard(pub PathBuf);

#[cfg(all(test, feature = "metadata"))]
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::copy::CopyStats;
pub use crate::copy::DirtyFilesReport;
use crate::copy::ParallelCopyConfig;

// ============================================================================
// BtrfsDelegate – delegate privileged btrfs ops to an external service
// ============================================================================

/// Result from a delegated btrfs snapshot creation.
#[derive(Debug, Clone)]
pub struct DelegateSnapshotResult {
    /// Path to the actual btrfs snapshot.
    pub snapshot_path: PathBuf,
    /// Path where the worktree is accessible (bind-mounted from `snapshot_path`).
    pub worktree_path: PathBuf,
    /// Whether a bind mount was created from `snapshot_path` to `worktree_path`.
    pub bind_mounted: bool,
}

/// Delegate privileged btrfs operations to an external helper.
///
/// When the caller runs inside a sandbox without `CAP_SYS_ADMIN`, it cannot
/// execute `btrfs subvolume snapshot/delete` directly. This trait lets it
/// delegate those operations to a privileged process (e.g. over IPC).
///
/// Implementations must be `Send + Sync` (shared across threads).
pub trait BtrfsDelegate: Send + Sync {
    /// Create a btrfs snapshot of `source` accessible at `dest`.
    ///
    /// The implementation is expected to:
    /// 1. Detect whether `source` is a btrfs subvolume
    /// 2. Create a snapshot (inside the btrfs filesystem)
    /// 3. Bind mount `dest` from snapshot if source is bind-mounted
    /// 4. Clean up stale git state (lock files, worktree registrations)
    fn create_snapshot(&self, source: &Path, dest: &Path) -> Result<DelegateSnapshotResult>;

    /// Delete a btrfs snapshot worktree.
    ///
    /// If `worktree_path` is a bind mount, the implementation should unmount it,
    /// delete the btrfs snapshot, and clean up the mount point.
    fn delete_snapshot(&self, worktree_path: &Path) -> Result<RemoveReport>;

    /// Mount an overlayfs at `target` in the *caller's* mount namespace.
    ///
    /// A FUSE+overlay worktree needs a new overlay mount, which a rootless
    /// caller can't do (no `CAP_SYS_ADMIN`); the privileged delegate mounts it
    /// inside the caller's namespace (an overlay mount can't be exposed via a
    /// namespace-crossing symlink the way a btrfs snapshot can). Default impl
    /// errors so btrfs-only delegates still compile.
    fn mount_overlay(&self, lower: &Path, upper: &Path, work: &Path, target: &Path) -> Result<()> {
        let _ = (lower, upper, work, target);
        anyhow::bail!("overlay mount delegation not supported by this delegate")
    }

    /// Unmount an overlay worktree previously mounted via [`Self::mount_overlay`]
    /// (in the caller's mount namespace).
    fn unmount_overlay(&self, target: &Path) -> Result<()> {
        let _ = target;
        anyhow::bail!("overlay unmount delegation not supported by this delegate")
    }
}

/// How to treat the source working tree when creating the destination worktree.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum WorkingTreeMode {
    /// Replicate the working tree exactly as-is (including local modifications and untracked files).
    #[default]
    PreserveWorkingTree,
    /// Produce a clean checked-out working tree for tracked files.
    ///
    /// Local modifications and untracked files from the source are not copied.
    CleanTracked,
    /// Produce a clean worktree and also remove any untracked files (equivalent to
    /// `git reset --hard` + `git clean -fd`).
    ///
    /// Note: ignored files are not removed by default `git clean`.
    CleanAll,
}

/// Whether (and how) to copy `.gitignore`'d files after the worktree is ready.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum IgnoredFilesMode {
    /// Do not copy ignored files.
    #[default]
    Skip,
    /// Copy ignored files, optionally skipping additional patterns.
    Copy { skip_patterns: Vec<String> },
    /// Copy ONLY ignored files (no worktree creation), optionally skipping additional patterns.
    /// This is for standalone use via `copy_ignored_only()`.
    CopyOnly { skip_patterns: Vec<String> },
}

/// How to handle BTRFS snapshot optimization on Linux.
///
/// On Linux systems where the source repo is on a BTRFS subvolume,
/// we can use BTRFS snapshots for O(1) worktree creation instead of
/// file-by-file CoW cloning.
///
/// The snapshot creates a complete standalone git repository (not a
/// linked git worktree), which is immediately usable.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum BtrfsMode {
    /// Auto-detect: use BTRFS snapshot if source is on a BTRFS subvolume.
    /// Falls back to file-by-file copy if not on BTRFS or not a subvolume.
    #[default]
    Auto,
    /// Force use of BTRFS snapshot. Returns an error if the source is not
    /// on a BTRFS subvolume.
    Force,
    /// Disable BTRFS snapshot optimization. Always use file-by-file copy.
    Disabled,
}

/// Strategy for creating the worktree.
///
/// Consolidates the choice of linked vs standalone, BTRFS snapshots,
/// and git-native checkout into a single enum.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum CreationMode {
    /// Linked worktree via `git worktree add --no-checkout` followed by
    /// parallel CoW file copy and index finalization. On Linux with BTRFS,
    /// auto-detects and uses instant snapshots when possible.
    ///
    /// This is the fastest mode for large repos on APFS/Btrfs.
    #[default]
    Linked,

    /// Standalone repository copy with its own independent `.git/`
    /// directory (CoW'd from the source). Can be promoted to replace the
    /// source via a simple `rename()`, with no worktree cleanup needed.
    ///
    /// On Linux with BTRFS, auto-detects and uses instant snapshots.
    Standalone,

    /// Plain `git worktree add` with full checkout. Lets git handle the
    /// entire worktree creation including index and working tree
    /// population. Simpler and avoids split-index / index-copy edge
    /// cases, but git does the checkout single-threaded.
    GitCheckout,
}

impl CreationMode {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Linked => "linked",
            Self::Standalone => "standalone",
            Self::GitCheckout => "git",
        }
    }
}

/// A structured report for a copy phase.
#[derive(Clone, Debug, Default)]
pub struct CopyReport {
    pub files_copied: u64,
    pub dirs_created: u64,
    pub symlinks_copied: u64,
    pub files_skipped: u64,
    /// Non-fatal issues encountered during copying.
    pub issues: Vec<String>,
    pub dirty_files: Option<DirtyFilesReport>,
}

impl From<CopyStats> for CopyReport {
    fn from(stats: CopyStats) -> Self {
        Self {
            files_copied: stats.files_copied,
            dirs_created: stats.dirs_created,
            symlinks_copied: stats.symlinks_copied,
            files_skipped: stats.files_skipped,
            issues: stats.issues,
            dirty_files: None,
        }
    }
}

/// Result of creating a worktree via the new API.
#[derive(Debug)]
pub struct WorktreeReport {
    pub worktree_path: PathBuf,
    pub commit: String,
    pub unignored_copy: CopyReport,
    pub ignored_copy: Option<CopyReport>,
}

/// High-level builder API for creating fast git worktrees.
///
/// All operations are **synchronous/blocking**. Callers should use `spawn_blocking`
/// when calling from async contexts.
#[derive(Clone)]
pub struct WorktreeBuilder {
    source: PathBuf,
    dest: PathBuf,
    git_ref: String,
    parallelism: usize,
    channel_buffer: usize,
    ignored_parallelism: usize,
    working_tree: WorkingTreeMode,
    ignored_files: IgnoredFilesMode,
    creation_mode: CreationMode,
    cancellation_token: CancellationToken,
    btrfs_delegate: Option<Arc<dyn BtrfsDelegate>>,
    #[cfg(feature = "metadata")]
    worktree_kind: Option<crate::db::WorktreeKind>,
    #[cfg(feature = "metadata")]
    session_id: Option<String>,
    #[cfg(feature = "metadata")]
    worktree_id: Option<String>,
    #[cfg(feature = "metadata")]
    metadata: Option<serde_json::Value>,
}

impl std::fmt::Debug for WorktreeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorktreeBuilder")
            .field("source", &self.source)
            .field("dest", &self.dest)
            .field("git_ref", &self.git_ref)
            .field("parallelism", &self.parallelism)
            .field("creation_mode", &self.creation_mode)
            .field("btrfs_delegate", &self.btrfs_delegate.is_some())
            .finish_non_exhaustive()
    }
}

impl WorktreeBuilder {
    pub fn new(source: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            dest: dest.into(),
            git_ref: "HEAD".to_string(),
            parallelism: 0,
            channel_buffer: 256,
            ignored_parallelism: 0,
            working_tree: WorkingTreeMode::PreserveWorkingTree,
            ignored_files: IgnoredFilesMode::Skip,
            creation_mode: CreationMode::default(),
            cancellation_token: CancellationToken::new(),
            btrfs_delegate: None,
            #[cfg(feature = "metadata")]
            worktree_kind: None,
            #[cfg(feature = "metadata")]
            session_id: None,
            #[cfg(feature = "metadata")]
            worktree_id: None,
            #[cfg(feature = "metadata")]
            metadata: None,
        }
    }

    /// Set a cancellation token that can be used to stop a copy operation in progress.
    /// When the token is cancelled, the copy will stop as soon as possible.
    pub fn cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = token;
        self
    }

    pub fn git_ref(mut self, git_ref: impl Into<String>) -> Self {
        self.git_ref = git_ref.into();
        self
    }

    pub fn parallelism(mut self, parallelism: usize) -> Self {
        self.parallelism = parallelism;
        self
    }

    pub fn ignored_parallelism(mut self, parallelism: usize) -> Self {
        self.ignored_parallelism = parallelism;
        self
    }

    pub fn channel_buffer(mut self, channel_buffer: usize) -> Self {
        self.channel_buffer = channel_buffer;
        self
    }

    pub fn working_tree_mode(mut self, mode: WorkingTreeMode) -> Self {
        self.working_tree = mode;
        self
    }

    pub fn ignored_files_mode(mut self, mode: IgnoredFilesMode) -> Self {
        self.ignored_files = mode;
        self
    }

    /// Set the worktree creation strategy.
    ///
    /// - `Linked` (default): `git worktree add --no-checkout` + parallel
    ///   CoW file copy + index finalization. Fastest on large repos.
    /// - `Standalone`: Independent `.git/` copy (CoW'd). Can be promoted
    ///   to replace the source via `rename()`.
    /// - `GitCheckout`: Plain `git worktree add` with full checkout. Simpler,
    ///   avoids split-index issues, but single-threaded checkout.
    pub fn creation_mode(mut self, mode: CreationMode) -> Self {
        self.creation_mode = mode;
        self
    }

    /// Set the worktree kind for metadata tracking.
    /// When set, `create()` auto-registers the worktree in the metadata DB.
    #[cfg(feature = "metadata")]
    pub fn worktree_kind(mut self, kind: crate::db::WorktreeKind) -> Self {
        self.worktree_kind = Some(kind);
        self
    }

    /// Set the session ID associated with this worktree.
    #[cfg(feature = "metadata")]
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Override the worktree ID (default: derived from dest path).
    #[cfg(feature = "metadata")]
    pub fn worktree_id(mut self, id: impl Into<String>) -> Self {
        self.worktree_id = Some(id.into());
        self
    }

    /// Set arbitrary metadata to store alongside the worktree record.
    #[cfg(feature = "metadata")]
    pub fn metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Shorthand for `.creation_mode(CreationMode::Standalone)`.
    pub fn standalone(mut self, standalone: bool) -> Self {
        if standalone {
            self.creation_mode = CreationMode::Standalone;
        }
        self
    }

    /// Shorthand for setting the BTRFS snapshot mode (Linux only).
    ///
    /// BTRFS snapshots are automatically used by `Linked` and `Standalone`
    /// modes when the source is on a BTRFS subvolume. This method is only
    /// needed to *force* or *disable* that auto-detection.
    pub fn btrfs_mode(self, mode: BtrfsMode) -> Self {
        // BtrfsMode is now handled inside execute.rs based on CreationMode.
        // This method is kept for backward compatibility with the CLI.
        tracing::warn!(
            ?mode,
            "WorktreeBuilder::btrfs_mode() is deprecated and has no effect. \
             BtrfsMode is now handled automatically based on CreationMode."
        );
        self
    }

    /// Set a delegate for privileged btrfs operations.
    ///
    /// When the caller lacks `CAP_SYS_ADMIN` (e.g., inside a bwrap sandbox),
    /// btrfs snapshot creation/deletion can be delegated to a privileged
    /// process via this trait. The delegate is tried as a fallback when
    /// direct btrfs operations fail or are unavailable.
    pub fn btrfs_delegate(mut self, delegate: Arc<dyn BtrfsDelegate>) -> Self {
        self.btrfs_delegate = Some(delegate);
        self
    }

    /// Create the worktree using the configured options.
    ///
    /// This is a **blocking** operation. Callers should use `spawn_blocking`
    /// when calling from async contexts.
    pub fn create(self) -> Result<WorktreeReport> {
        // Clone source/git_ref/creation_mode for DB registration before the move
        // into WorktreePlan. These are one-per-create, not a hot path.
        #[cfg(feature = "metadata")]
        let meta_fields = (
            self.worktree_kind,
            self.session_id,
            self.worktree_id,
            self.source.clone(),
            self.creation_mode.as_db_str(),
            self.git_ref.clone(),
            self.metadata,
        );

        let plan = crate::worktree::WorktreePlan {
            source: self.source,
            dest: self.dest,
            git_ref: self.git_ref,
            parallelism: self.parallelism,
            channel_buffer: self.channel_buffer,
            working_tree: self.working_tree,
            ignored_files: self.ignored_files,
            ignored_parallelism: self.ignored_parallelism,
            creation_mode: self.creation_mode,
            cancellation_token: self.cancellation_token,
            btrfs_delegate: self.btrfs_delegate,
        };

        let result = crate::worktree::execute_plan(plan).map_err(annotate_disk_full)?;

        #[cfg(feature = "metadata")]
        {
            let (kind, session_id, wt_id, source, creation_mode, git_ref, metadata) = meta_fields;
            if let Some(kind) = kind {
                register_worktree(
                    &result.worktree_path,
                    &source,
                    kind,
                    creation_mode,
                    &git_ref,
                    &result.commit,
                    session_id,
                    wt_id,
                    metadata,
                );
            }
        }

        let mut unignored_copy: CopyReport = result.copy_stats.into();
        unignored_copy.dirty_files = result.dirty_files_report;

        Ok(WorktreeReport {
            worktree_path: result.worktree_path,
            commit: result.commit,
            unignored_copy,
            ignored_copy: result.ignored_stats.map(Into::into),
        })
    }

    /// Copy ONLY `.gitignore`'d (ignored) files from `source` to `dest`.
    ///
    /// This does **not** create or finalize a worktree. It's intended to be run after a
    /// worktree already exists at `dest`, to populate ignored artifacts (node_modules, target, etc.).
    ///
    /// This is a **blocking** operation. Callers should use `spawn_blocking`
    /// when calling from async contexts.
    pub fn copy_ignored_only(self) -> Result<CopyReport> {
        let source = &self.source;
        let dest = &self.dest;

        let num_workers = if self.ignored_parallelism != 0 {
            self.ignored_parallelism
        } else if self.parallelism != 0 {
            self.parallelism
        } else {
            num_cpus::get()
        };

        let skip_patterns = match self.ignored_files {
            IgnoredFilesMode::Skip => vec![],
            IgnoredFilesMode::Copy { skip_patterns } => skip_patterns,
            IgnoredFilesMode::CopyOnly { skip_patterns } => skip_patterns,
        };

        tracing::info!(
            source = %source.display(),
            dest = %dest.display(),
            parallelism = num_workers,
            channel_buffer = self.channel_buffer,
            "copying ignored files (ignored-only)"
        );

        let start = std::time::Instant::now();
        let unignored_paths = crate::copy::collect_unignored_paths(source, num_workers)?;

        let copy_config = ParallelCopyConfig {
            num_workers,
            channel_buffer: self.channel_buffer,
            skip_files: Some(Arc::new(unignored_paths)),
            respect_gitignore: false,
            skip_patterns,
        };

        let copy_result =
            crate::copy::copy_parallel(source, dest, copy_config, self.cancellation_token.clone())?;

        // `copy_parallel` returns Ok with partial stats on cancellation; surface
        // it so an interrupted copy isn't treated as success.
        if self.cancellation_token.is_cancelled() {
            anyhow::bail!("cancelled during ignored-only copy");
        }

        tracing::debug!(
            elapsed = ?start.elapsed(),
            files = copy_result.stats.files_copied,
            dirs = copy_result.stats.dirs_created,
            symlinks = copy_result.stats.symlinks_copied,
            skipped = copy_result.stats.files_skipped,
            "copying ignored files (ignored-only) complete"
        );

        Ok(copy_result.stats.into())
    }
}

/// Error context attached when worktree creation fails on a full disk. The
/// pager matches on it, so this constant is the cross-crate contract.
pub const OUT_OF_DISK_CONTEXT: &str = "not enough free disk space";

/// POSIX disk-full text `git` prints to stderr; the text fallback for the
/// typed `ErrorKind::StorageFull` check.
pub const ENOSPC_OS_MESSAGE: &str = "No space left on device";

/// Detect a disk-full failure anywhere in an error chain.
///
/// Worktree creation touches the disk in many places (reflink/copy of files
/// and the git index, directory creation, `git worktree add`). When the volume
/// fills up the underlying `std::io::Error` reports `ErrorKind::StorageFull` —
/// std maps `ENOSPC` (Linux/macOS) and `ERROR_DISK_FULL` /
/// `ERROR_HANDLE_DISK_FULL` (Windows) onto it, so this is correct on every
/// platform. `git` subcommands instead surface the failure only as stderr text.
fn is_out_of_disk(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::StorageFull
        {
            return true;
        }
        // Fallback for `git` subcommands, which report this only as stderr text.
        cause.to_string().contains(ENOSPC_OS_MESSAGE)
    })
}

/// Promote a disk-full reason to the top of the error chain.
///
/// Downstream layers (the workspace hub, ACP) flatten the `anyhow` chain to its
/// top-level message via `Display`, discarding the root `io::Error`. Without
/// this, a full disk surfaces to the user as an opaque
/// `"failed to copy index from … to …"`. Promoting the reason to the outermost
/// context ensures it survives that flattening; the original chain is preserved
/// underneath for logs (`{:#}` / `{:?}`).
fn annotate_disk_full(err: anyhow::Error) -> anyhow::Error {
    if is_out_of_disk(&err) {
        err.context(OUT_OF_DISK_CONTEXT)
    } else {
        err
    }
}

/// Result of removing a worktree.
#[derive(Clone, Debug)]
pub struct RemoveReport {
    /// Whether a btrfs subvolume delete was used (O(1)) vs git worktree remove (O(n)).
    pub used_btrfs_delete: bool,
    /// Whether a bind mount was unmounted before deletion.
    pub unmounted_bind: bool,
    /// Whether an overlay mount was unmounted before deletion.
    pub unmounted_overlay: bool,
}

/// Remove a worktree, using the fastest available method.
///
/// Detection order:
/// 1. If the worktree is a symlink/bind-mount to a btrfs snapshot, or a direct btrfs subvolume → unmount if needed + `btrfs subvolume delete` (O(1))
/// 2. Otherwise → `rm -rf` + deregister from `.git/worktrees/`
///
/// **Why not `git worktree remove --force`?** On large repos (100K+ files),
/// `git worktree remove` walks all files to delete them (often tens of seconds).
/// Using `rm -rf` + deregistration is ~10x faster because the kernel handles
/// bulk deletion more efficiently, and we avoid git's per-file validation.
///
/// This is a **blocking** operation. Callers should use `spawn_blocking`
/// when calling from async contexts.
pub fn remove_worktree(worktree_path: &std::path::Path) -> Result<RemoveReport> {
    remove_worktree_inner(worktree_path, None)
}

/// Remove a worktree with an optional delegate for privileged btrfs operations.
///
/// When the caller has a `BtrfsDelegate` (e.g., from a sandbox with IPC to a
/// privileged helper), this function uses it as a fallback when direct btrfs
/// operations fail (e.g., due to missing `CAP_SYS_ADMIN`).
pub fn remove_worktree_with_delegate(
    worktree_path: &std::path::Path,
    delegate: Option<Arc<dyn BtrfsDelegate>>,
) -> Result<RemoveReport> {
    remove_worktree_inner(worktree_path, delegate.as_ref())
}

fn remove_worktree_inner(
    worktree_path: &std::path::Path,
    delegate: Option<&Arc<dyn BtrfsDelegate>>,
) -> Result<RemoveReport> {
    let report = remove_worktree_from_disk(worktree_path, delegate)?;

    // Unregister only AFTER a successful on-disk removal: a failed removal (e.g.
    // EPERM on btrfs delete) must keep the record so the worktree stays tracked
    // by list/gc instead of leaking untracked on disk.
    #[cfg(feature = "metadata")]
    unregister_worktree(worktree_path);

    Ok(report)
}

/// Remove the worktree from disk (overlay/btrfs/metadata fast paths or `rm -rf`
/// + deregister), without touching the metadata DB. Returns `Err` if the on-disk
/// removal fails, so the caller can keep the DB record.
fn remove_worktree_from_disk(
    worktree_path: &std::path::Path,
    delegate: Option<&Arc<dyn BtrfsDelegate>>,
) -> Result<RemoveReport> {
    use anyhow::Context;

    #[cfg(not(target_os = "linux"))]
    let _ = delegate;

    // Try overlay removal first (Linux only) — unmount overlay + delete btrfs snapshot
    #[cfg(target_os = "linux")]
    {
        if let Some(report) = try_overlay_remove(worktree_path, delegate)? {
            return Ok(report);
        }
    }

    // Try btrfs metadata-based removal (crash recovery)
    #[cfg(target_os = "linux")]
    {
        if let Some(report) = try_btrfs_remove_from_metadata(worktree_path, delegate)? {
            return Ok(report);
        }
    }

    // Try btrfs fast path (Linux only)
    #[cfg(target_os = "linux")]
    {
        if let Some(report) = try_btrfs_remove(worktree_path, delegate)? {
            return Ok(report);
        }
    }

    // Fast path: rm -rf the worktree directory, then deregister from .git/worktrees/.
    // This is ~10x faster than `git worktree remove --force` on large repos.
    tracing::debug!(
        path = %worktree_path.display(),
        "removing worktree via rm -rf + deregister"
    );

    // Read the worktree's .git file to find the registration dir BEFORE deleting.
    // Linked worktrees have `.git` as a file containing `gitdir: /path/to/.git/worktrees/<name>`.
    let registration_dir = read_worktree_gitdir(worktree_path);

    // symlink_metadata, not `exists()` (which follows the link): a worktree
    // exposed as a symlink — including a now-dangling one — must be unlinked, not
    // skipped. (On Linux, symlinks are normally handled earlier in try_btrfs_remove.)
    match std::fs::symlink_metadata(worktree_path) {
        Ok(md) if md.file_type().is_symlink() => {
            std::fs::remove_file(worktree_path).context(format!(
                "failed to remove worktree symlink: {}",
                worktree_path.display()
            ))?;
        }
        Ok(_) => {
            std::fs::remove_dir_all(worktree_path).context(format!(
                "failed to remove worktree directory: {}",
                worktree_path.display()
            ))?;
        }
        Err(_) => {} // nothing at the path
    }

    // Deregister: remove the `.git/worktrees/<name>/` directory.
    // This is what `git worktree remove` does after deleting the working tree.
    if let Some(reg_dir) = registration_dir
        && reg_dir.exists()
    {
        tracing::debug!(
            registration_dir = %reg_dir.display(),
            "removing worktree registration from .git/worktrees/"
        );
        let _ = std::fs::remove_dir_all(&reg_dir);
    }

    Ok(RemoveReport {
        used_btrfs_delete: false,
        unmounted_bind: false,
        unmounted_overlay: false,
    })
}

/// Report from cleaning up multiple worktrees.
#[derive(Debug, Default)]
pub struct CleanupReport {
    /// Number of worktrees successfully removed.
    pub removed: u64,
    /// Number of overlay mounts unmounted.
    pub overlays_unmounted: u64,
    /// Number of btrfs subvolumes deleted.
    pub btrfs_deleted: u64,
    /// Number of errors encountered (worktrees that couldn't be removed).
    pub errors: u64,
}

/// Remove all worktrees under a directory.
///
/// Scans the given directory for subdirectories (one or two levels deep to
/// handle `~/.grok/worktrees/<repo>/<session>/`) and calls `remove_worktree()`
/// on each. Useful during session teardown to clean up all session worktrees.
///
/// This is a **blocking** operation.
pub fn cleanup_worktrees_in(dir: &std::path::Path) -> CleanupReport {
    cleanup_worktrees_in_with_delegate(dir, None)
}

/// Remove all worktrees under a directory, using an optional delegate for
/// privileged btrfs operations.
///
/// Like `cleanup_worktrees_in`, but forwards the delegate to each
/// `remove_worktree_with_delegate` call so that rootless hosts can clean up
/// btrfs snapshots via a privileged helper.
pub fn cleanup_worktrees_in_with_delegate(
    dir: &std::path::Path,
    delegate: Option<Arc<dyn BtrfsDelegate>>,
) -> CleanupReport {
    let mut report = CleanupReport::default();

    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::debug!(dir = %dir.display(), "cleanup: directory not readable");
        return report;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // symlink_metadata so a symlink-exposed worktree (btrfs snapshot layout),
        // including a now-dangling one, is handled — `is_dir()` follows the link
        // and returns false for a broken symlink, leaking it.
        let Ok(md) = path.symlink_metadata() else {
            continue;
        };
        if md.file_type().is_symlink() {
            // remove_worktree handles the snapshot delete + symlink unlink.
            cleanup_single_worktree(&path, delegate.as_ref(), &mut report);
            continue;
        }
        if !md.is_dir() {
            continue;
        }

        let has_git = path.join(".git").exists();

        if has_git {
            cleanup_single_worktree(&path, delegate.as_ref(), &mut report);
        } else {
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                for sub_entry in sub_entries.flatten() {
                    let sub_path = sub_entry.path();
                    if let Ok(sub_md) = sub_path.symlink_metadata()
                        && (sub_md.file_type().is_symlink() || sub_md.is_dir())
                    {
                        cleanup_single_worktree(&sub_path, delegate.as_ref(), &mut report);
                    }
                }
            }
            let _ = std::fs::remove_dir(&path);
        }
    }

    tracing::info!(
        dir = %dir.display(),
        removed = report.removed,
        overlays = report.overlays_unmounted,
        btrfs = report.btrfs_deleted,
        errors = report.errors,
        "worktree cleanup complete"
    );

    report
}

/// Remove a single worktree and update the report.
fn cleanup_single_worktree(
    path: &std::path::Path,
    delegate: Option<&Arc<dyn BtrfsDelegate>>,
    report: &mut CleanupReport,
) {
    match remove_worktree_inner(path, delegate) {
        Ok(r) => {
            report.removed += 1;
            if r.unmounted_overlay {
                report.overlays_unmounted += 1;
            }
            if r.used_btrfs_delete {
                report.btrfs_deleted += 1;
            }
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to clean up worktree"
            );
            report.errors += 1;
        }
    }
}

/// Scan known overlay roots under `/local/repo-fuse-*/worktrees/` for orphaned
/// overlay snapshots.
///
/// An overlay snapshot is orphaned if its metadata file exists but the
/// `mount_target` doesn't exist or isn't mounted. For each orphan: delete
/// the btrfs snapshot, remove the work dir, and clean up metadata.
///
/// Intended for host startup / periodic cleanup of leftovers from unclean
/// exits.
///
/// This is a **blocking** operation.
#[cfg(target_os = "linux")]
pub fn cleanup_orphaned_overlay_snapshots() -> CleanupReport {
    crate::overlay::cleanup_orphaned_overlay_snapshots()
}

/// Try to remove an overlay worktree.
/// Returns `Ok(Some(report))` if overlay was detected and removed, `Ok(None)` to fall back.
#[cfg(target_os = "linux")]
fn try_overlay_remove(
    worktree_path: &std::path::Path,
    delegate: Option<&Arc<dyn BtrfsDelegate>>,
) -> Result<Option<RemoveReport>> {
    use crate::overlay;

    // Method 1: Check live mountinfo
    if let Some(report) = overlay::try_remove_from_mountinfo(worktree_path, delegate)? {
        return Ok(Some(report));
    }

    // Method 2: Check persisted metadata (crash recovery)
    if let Some(report) = overlay::try_remove_from_metadata(worktree_path, delegate)? {
        return Ok(Some(report));
    }

    Ok(None)
}

/// Read the `gitdir:` pointer from a linked worktree's `.git` file.
///
/// Linked worktrees have `.git` as a plain file containing:
/// ```text
/// gitdir: /path/to/main-repo/.git/worktrees/<name>
/// ```
///
/// Returns the resolved path to the registration directory, or `None`
/// if the worktree doesn't have a `.git` file (standalone repo or missing).
fn read_worktree_gitdir(worktree_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let git_file = worktree_path.join(".git");
    let content = std::fs::read_to_string(&git_file).ok()?;
    let gitdir = content.trim().strip_prefix("gitdir: ")?;
    let path = std::path::Path::new(gitdir);
    // Resolve relative paths against the worktree directory
    let resolved = if path.is_relative() {
        worktree_path.join(path)
    } else {
        path.to_path_buf()
    };
    // Canonicalize to clean up any `..` components
    dunce::canonicalize(&resolved).ok().or(Some(resolved))
}

/// Delete `snapshot_path`, falling back to the delegate's `delete_snapshot`
/// (keyed by `worktree_path`) when the direct btrfs delete fails — e.g. EPERM on
/// a rootless host (no `CAP_SYS_ADMIN`) where only a privileged helper can run
/// `btrfs subvolume delete`.
///
/// `Some` means the delegate handled it; `None` means the direct delete succeeded
/// and the caller still owns local cleanup.
#[cfg(target_os = "linux")]
fn delete_snapshot_with_delegate_fallback(
    snapshot_path: &std::path::Path,
    worktree_path: &std::path::Path,
    delegate: Option<&Arc<dyn BtrfsDelegate>>,
    delete: impl FnOnce(&std::path::Path) -> Result<()>,
) -> Result<Option<RemoveReport>> {
    let Err(e) = delete(snapshot_path) else {
        return Ok(None);
    };
    if let Some(delegate) = delegate {
        tracing::info!(
            path = %worktree_path.display(),
            "btrfs subvolume delete failed, trying delegate"
        );
        match delegate.delete_snapshot(worktree_path) {
            Ok(report) => return Ok(Some(report)),
            Err(delegate_err) => {
                tracing::warn!(error = %delegate_err, "delegate deletion also failed");
            }
        }
    }
    Err(e)
}

/// Try to remove a worktree using btrfs subvolume delete.
/// Returns `Ok(Some(report))` if btrfs was used, `Ok(None)` to fall back to git.
///
/// Handles three cases:
/// 1. **Symlinked worktree** (delegate path): `worktree_path` is a symlink to a
///    btrfs snapshot. Delete the snapshot, then remove the symlink.
/// 2. **Bind-mounted worktree**: `worktree_path` is a bind mount from a btrfs
///    snapshot. Unmount, then delete the snapshot subvolume.
/// 3. **Direct btrfs worktree**: `worktree_path` itself is the btrfs subvolume.
///    Delete it directly.
#[cfg(target_os = "linux")]
fn try_btrfs_remove(
    worktree_path: &std::path::Path,
    delegate: Option<&Arc<dyn BtrfsDelegate>>,
) -> Result<Option<RemoveReport>> {
    use crate::btrfs;
    use anyhow::Context;

    // Case 1: Symlink to a btrfs snapshot (created by the delegate path on
    // rootless hosts). Symlinks cross mount namespaces — this is the
    // counterpart to the privileged helper's symlink creation.
    if worktree_path.is_symlink() {
        let link_target = match std::fs::read_link(worktree_path) {
            Ok(t) => t,
            // Broken/unreadable symlink: unlink it so it isn't left dangling
            // (the `rm -rf` fallback follows the dead link and would miss it).
            Err(_) => {
                let _ = std::fs::remove_file(worktree_path);
                return Ok(None);
            }
        };

        let resolved = if link_target.is_relative() {
            worktree_path
                .parent()
                .unwrap_or(std::path::Path::new("/"))
                .join(&link_target)
        } else {
            link_target
        };

        if let Ok(Some(_)) = btrfs::is_btrfs_subvolume(&resolved) {
            // Refuse to follow a confused/planted symlink into deleting a
            // subvolume outside the snapshot storage (e.g. the live source repo).
            // The symlink itself is just a pointer, so removing it is always safe.
            if !btrfs::is_safe_snapshot_delete_target(&resolved) {
                tracing::warn!(
                    symlink = %worktree_path.display(),
                    target = %resolved.display(),
                    "refusing to delete subvolume outside snapshot storage; removing only the symlink"
                );
                let _ = std::fs::remove_file(worktree_path);
                return Ok(Some(RemoveReport {
                    used_btrfs_delete: false,
                    unmounted_bind: false,
                    unmounted_overlay: false,
                }));
            }

            tracing::info!(
                symlink = %worktree_path.display(),
                target = %resolved.display(),
                "removing symlinked btrfs worktree"
            );

            // Delete snapshot first — if this fails, the symlink still
            // references it so cleanup can be retried.
            //
            // Known residual TOCTOU: validation `lstat`s/canonicalizes then we
            // delete by path (the `btrfs subvolume delete` CLI takes a path, not
            // an fd, so there is no `unlinkat` to close the window). Bounded by:
            // `btrfs` refuses non-subvolumes, the snapshot dir is grok-owned, and
            // `..`/symlink targets are already rejected. Accepted as-is.
            if let Some(report) = delete_snapshot_with_delegate_fallback(
                &resolved,
                worktree_path,
                delegate,
                btrfs::delete_snapshot,
            )? {
                return Ok(Some(report));
            }
            btrfs::remove_btrfs_metadata(&resolved);
            let _ = std::fs::remove_file(worktree_path);

            return Ok(Some(RemoveReport {
                used_btrfs_delete: true,
                unmounted_bind: false,
                unmounted_overlay: false,
            }));
        }

        // Symlink to non-btrfs target — remove symlink, fall through.
        let _ = std::fs::remove_file(worktree_path);
    }

    // Case 2 & 3: Check if the worktree path is a btrfs subvolume.
    let btrfs_info = match btrfs::is_btrfs_subvolume(worktree_path) {
        Ok(Some(info)) => info,
        Ok(None) => return Ok(None), // Not a btrfs subvolume, fall back
        Err(e) => {
            tracing::debug!(
                path = %worktree_path.display(),
                error = %e,
                "btrfs detection failed, falling back to git worktree remove"
            );
            return Ok(None);
        }
    };

    tracing::info!(
        path = %worktree_path.display(),
        bind_mount = ?btrfs_info.bind_mount_source,
        "removing worktree via btrfs subvolume delete (O(1))"
    );

    let mut unmounted_bind = false;

    // Case 2: Legacy bind mount — unmount first, then delete snapshot.
    if btrfs_info.bind_mount_source.is_some() {
        let mut umount_cmd = std::process::Command::new("umount");
        xai_tty_utils::detach_std_command(&mut umount_cmd);
        umount_cmd.stdin(std::process::Stdio::null());
        let output = umount_cmd
            .arg(worktree_path)
            .output()
            .context("failed to execute umount")?;

        if output.status.success() {
            unmounted_bind = true;
            let _ = std::fs::remove_dir(worktree_path);
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                path = %worktree_path.display(),
                stderr = %stderr.trim(),
                "umount failed, attempting direct snapshot deletion"
            );
            // Don't return — proceed to delete the snapshot directly.
            // The mount point may be stale after an unclean host restart.
        }
    }

    // Delete the btrfs subvolume (the actual snapshot)
    let snapshot_path = btrfs_info
        .bind_mount_source
        .as_deref()
        .unwrap_or(worktree_path);

    // Reuse the hardened `btrfs::delete_snapshot` (OsStr args, no lossy
    // `.`-default) rather than re-spawning the command inline.
    if let Some(report) = delete_snapshot_with_delegate_fallback(
        snapshot_path,
        worktree_path,
        delegate,
        btrfs::delete_snapshot,
    )? {
        return Ok(Some(report));
    }

    btrfs::remove_btrfs_metadata(snapshot_path);

    tracing::info!(
        path = %worktree_path.display(),
        "btrfs subvolume deleted successfully"
    );

    Ok(Some(RemoveReport {
        used_btrfs_delete: true,
        unmounted_bind,
        unmounted_overlay: false,
    }))
}

/// Try to remove via persisted btrfs snapshot metadata (crash recovery).
///
/// Scans btrfs mount points for `*.btrfs-meta.json` files whose
/// `mount_target` matches `target`. Works even after the bind mount is gone.
#[cfg(target_os = "linux")]
fn try_btrfs_remove_from_metadata(
    target: &std::path::Path,
    delegate: Option<&Arc<dyn BtrfsDelegate>>,
) -> Result<Option<RemoveReport>> {
    let mount_entries = match crate::mount_info::parse_mountinfo() {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };

    try_btrfs_remove_from_metadata_inner(target, &mount_entries, delegate)
}

#[cfg(target_os = "linux")]
fn try_btrfs_remove_from_metadata_inner(
    target: &std::path::Path,
    mount_entries: &[crate::mount_info::MountEntry],
    delegate: Option<&Arc<dyn BtrfsDelegate>>,
) -> Result<Option<RemoveReport>> {
    use crate::btrfs;

    for entry in mount_entries {
        if entry.fs_type != "btrfs" {
            continue;
        }

        for subdir in btrfs::BTRFS_SNAPSHOT_SUBDIRS {
            let dir = entry.mount_point.join(subdir);
            let Ok(dir_entries) = std::fs::read_dir(&dir) else {
                continue;
            };

            for dir_entry in dir_entries.flatten() {
                let name = dir_entry.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.ends_with(btrfs::BTRFS_META_SUFFIX) {
                    continue;
                }

                let meta_path = dir_entry.path();
                let Ok(content) = std::fs::read_to_string(&meta_path) else {
                    continue;
                };
                let Ok(meta) = serde_json::from_str::<btrfs::BtrfsSnapshotMetadata>(&content)
                else {
                    continue;
                };

                if meta.mount_target != target {
                    continue;
                }

                tracing::info!(
                    target = %target.display(),
                    snapshot = %meta.snapshot_path.display(),
                    "found btrfs snapshot metadata for worktree"
                );

                // `meta.snapshot_path` comes from an attacker-controllable
                // metadata file. Only delete it when it is a contained snapshot
                // subvolume located directly inside the directory we scanned.
                let snapshot_contained = meta.snapshot_path.parent() == Some(dir.as_path())
                    && btrfs::is_safe_snapshot_delete_target(&meta.snapshot_path);

                let target_is_symlink = target.is_symlink();
                let mut unmounted = false;

                // A legacy bind-mount directory must be unmounted before its
                // snapshot subvolume can be deleted; a symlink needs no umount.
                if !target_is_symlink {
                    let mut umount_cmd = std::process::Command::new("umount");
                    xai_tty_utils::detach_std_command(&mut umount_cmd);
                    umount_cmd.stdin(std::process::Stdio::null());
                    if let Ok(output) = umount_cmd.arg(target).output() {
                        unmounted = output.status.success();
                    }
                }

                // Delete the snapshot BEFORE removing the worktree reference, so
                // the link/dir still points at it if deletion fails (retriable) —
                // consistent with `try_btrfs_remove` Case 1.
                let mut deleted = false;
                let mut refused = false;
                if meta.snapshot_path.exists() {
                    if snapshot_contained {
                        if let Err(e) = btrfs::delete_snapshot(&meta.snapshot_path) {
                            // Try delegate fallback for sandboxed/rootless setups.
                            if let Some(delegate) = delegate {
                                tracing::info!(
                                    path = %meta.snapshot_path.display(),
                                    "btrfs delete failed in metadata path, trying delegate"
                                );
                                match delegate.delete_snapshot(target) {
                                    Ok(report) => return Ok(Some(report)),
                                    Err(delegate_err) => {
                                        tracing::warn!(
                                            error = %delegate_err,
                                            "delegate deletion also failed in metadata path"
                                        );
                                    }
                                }
                            }
                            return Err(e);
                        }
                        deleted = true;
                    } else {
                        refused = true;
                        tracing::warn!(
                            snapshot = %meta.snapshot_path.display(),
                            dir = %dir.display(),
                            "refusing to delete btrfs snapshot referenced by metadata: \
                             path is outside the scanned snapshot storage; preserving metadata"
                        );
                    }
                }

                // Remove the worktree reference (symlink file or empty dir). The
                // pointer is always safe to drop regardless of the refusal above.
                if target_is_symlink {
                    let _ = std::fs::remove_file(target);
                } else {
                    let _ = std::fs::remove_dir(target);
                }

                // Discard the metadata only when we handled the snapshot (deleted
                // it, or it was already gone). On refusal, keep it so the orphan
                // scanner can retry / it can be inspected.
                if !refused {
                    let _ = std::fs::remove_file(&meta_path);
                }

                return Ok(Some(RemoveReport {
                    used_btrfs_delete: deleted,
                    unmounted_bind: unmounted,
                    unmounted_overlay: false,
                }));
            }
        }
    }

    Ok(None)
}

/// Scan btrfs mount points for orphaned direct btrfs snapshots.
///
/// A btrfs snapshot is orphaned if its metadata file exists but the
/// `mount_target` is not an active mount point. For each orphan: unmount
/// stale target, delete the btrfs snapshot, and remove metadata.
///
/// This is the btrfs counterpart to `cleanup_orphaned_overlay_snapshots()`.
#[cfg(target_os = "linux")]
pub fn cleanup_orphaned_btrfs_snapshots() -> CleanupReport {
    let mount_entries = match crate::mount_info::parse_mountinfo() {
        Ok(e) => e,
        Err(_) => return CleanupReport::default(),
    };

    cleanup_orphaned_btrfs_snapshots_inner(&mount_entries)
}

/// Whether the symlink at `link` resolves to `target`.
///
/// Returns `false` when `link` is not a symlink or cannot be read. Used to
/// recognize a **live** symlink worktree (current layout) whose `mount_target`
/// never appears in mountinfo, so the orphan scanner does not destroy it.
#[cfg(target_os = "linux")]
fn symlink_resolves_to(link: &std::path::Path, target: &std::path::Path) -> bool {
    if !link.is_symlink() {
        return false;
    }
    match std::fs::read_link(link) {
        Ok(t) if t.is_relative() => {
            link.parent().unwrap_or(std::path::Path::new("/")).join(t) == target
        }
        Ok(t) => t == target,
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn cleanup_orphaned_btrfs_snapshots_inner(
    mount_entries: &[crate::mount_info::MountEntry],
) -> CleanupReport {
    use crate::btrfs;

    let mut report = CleanupReport::default();

    for mount_point in mount_entries
        .iter()
        .filter(|e| e.fs_type == "btrfs")
        .map(|e| &e.mount_point)
    {
        for subdir in btrfs::BTRFS_SNAPSHOT_SUBDIRS {
            let dir = mount_point.join(subdir);
            let Ok(dir_entries) = std::fs::read_dir(&dir) else {
                continue;
            };

            for dir_entry in dir_entries.flatten() {
                let name = dir_entry.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.ends_with(btrfs::BTRFS_META_SUFFIX) {
                    continue;
                }

                let meta_path = dir_entry.path();
                let Ok(content) = std::fs::read_to_string(&meta_path) else {
                    let _ = std::fs::remove_file(&meta_path);
                    continue;
                };
                let Ok(meta) = serde_json::from_str::<btrfs::BtrfsSnapshotMetadata>(&content)
                else {
                    let _ = std::fs::remove_file(&meta_path);
                    continue;
                };

                // A live worktree is active either as a bind mount (appears in
                // mountinfo) or as a symlink resolving to its snapshot (the
                // current layout — `mount_target` is a symlink, never in
                // mountinfo). Both must be treated as active, not orphaned.
                let is_active = mount_entries
                    .iter()
                    .any(|e| e.mount_point == meta.mount_target)
                    || symlink_resolves_to(&meta.mount_target, &meta.snapshot_path);

                if is_active {
                    tracing::debug!(
                        snapshot = %meta.snapshot_path.display(),
                        target = %meta.mount_target.display(),
                        "skipping active btrfs snapshot"
                    );
                    continue;
                }

                // If the mount_target's parent dir is missing we cannot prove the snapshot
                // is orphaned: this scanner runs before restore recreates worktree dirs, so a
                // snapshot about to be re-exposed would be wrongly destroyed. Skipping at worst
                // leaks a true orphan (reclaimed on a later cycle) — strictly safer than deleting.
                if let Some(parent) = meta.mount_target.parent()
                    && !parent.exists()
                {
                    tracing::debug!(
                        snapshot = %meta.snapshot_path.display(),
                        target = %meta.mount_target.display(),
                        "skipping btrfs snapshot: mount_target parent missing (cannot prove orphaned)"
                    );
                    continue;
                }

                // Untrusted metadata: only delete a snapshot contained directly
                // in the directory we scanned. Leave anything else (and its
                // metadata) untouched for inspection.
                let snapshot_contained = meta.snapshot_path.parent() == Some(dir.as_path())
                    && btrfs::is_safe_snapshot_delete_target(&meta.snapshot_path);
                if meta.snapshot_path.exists() && !snapshot_contained {
                    tracing::warn!(
                        snapshot = %meta.snapshot_path.display(),
                        dir = %dir.display(),
                        "refusing to delete btrfs snapshot outside scanned storage"
                    );
                    report.errors += 1;
                    continue;
                }

                tracing::info!(
                    target = %meta.mount_target.display(),
                    snapshot = %meta.snapshot_path.display(),
                    "cleaning up orphaned btrfs snapshot"
                );

                // Remove the worktree reference: a symlink is unlinked; a legacy
                // bind-mount dir is unmounted then removed.
                if meta.mount_target.is_symlink() {
                    let _ = std::fs::remove_file(&meta.mount_target);
                } else {
                    let mut umount_cmd = std::process::Command::new("umount");
                    xai_tty_utils::detach_std_command(&mut umount_cmd);
                    umount_cmd.stdin(std::process::Stdio::null());
                    let _ = umount_cmd.arg(&meta.mount_target).output();
                    let _ = std::fs::remove_dir(&meta.mount_target);
                }

                if meta.snapshot_path.exists() {
                    if let Err(e) = btrfs::delete_snapshot(&meta.snapshot_path) {
                        tracing::warn!(
                            path = %meta.snapshot_path.display(),
                            error = %e,
                            "failed to delete orphaned btrfs snapshot"
                        );
                        report.errors += 1;
                        // Preserve metadata so the orphan scanner can retry
                        // on the next cycle instead of losing track of it.
                        continue;
                    } else {
                        report.btrfs_deleted += 1;
                    }
                }

                let _ = std::fs::remove_file(&meta_path);
                report.removed += 1;
            }
        }
    }

    if report.removed > 0 || report.errors > 0 {
        tracing::info!(
            removed = report.removed,
            btrfs = report.btrfs_deleted,
            errors = report.errors,
            "orphaned btrfs snapshot cleanup complete"
        );
    }

    report
}

#[cfg(feature = "metadata")]
pub(crate) fn register_worktree(
    worktree_path: &std::path::Path,
    source: &std::path::Path,
    kind: crate::db::WorktreeKind,
    creation_mode: &str,
    git_ref: &str,
    commit: &str,
    session_id: Option<String>,
    worktree_id: Option<String>,
    metadata: Option<serde_json::Value>,
) {
    use crate::db;

    let db = match db::WorktreeDb::open_default() {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open worktree DB for registration");
            return;
        }
    };
    // Same canonical path as discovery rebuild / WorktreeDb::get so macOS
    // /var vs /private/var (and other symlink roots) do not create duplicate rows.
    let path = dunce::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    let source = dunce::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    let record = db::WorktreeRecord {
        id: worktree_id.unwrap_or_else(|| db::id_from_path(&path)),
        path,
        source_repo: source.clone(),
        repo_name: db::repo_name_from_path(&source),
        kind,
        creation_mode: creation_mode.to_owned(),
        git_ref: Some(git_ref.to_owned()),
        head_commit: Some(commit.to_owned()),
        session_id,
        creator_pid: Some(std::process::id()),
        created_at: db::now_epoch_secs(),
        last_accessed_at: None,
        status: db::WorktreeStatus::Alive,
        metadata,
    };
    if let Err(e) = db.register(&record) {
        tracing::warn!(error = %e, "failed to register worktree in DB");
    }
}

#[cfg(feature = "metadata")]
fn unregister_worktree(worktree_path: &std::path::Path) {
    if let Ok(db) = crate::db::WorktreeDb::open_default() {
        let path =
            dunce::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
        let _ = db.unregister_by_path(&path);
    }
}

/// Test-only `BtrfsDelegate` that returns a fixed snapshot and counts
/// `delete_snapshot` calls. Shared by the delegate-arm reclaim tests
/// (`worktree::execute`) and the gc-with-delegate tests.
#[cfg(test)]
pub(crate) struct RecordingDelegate {
    pub snapshot_path: PathBuf,
    pub worktree_path: PathBuf,
    pub deletes: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl BtrfsDelegate for RecordingDelegate {
    fn create_snapshot(&self, _source: &Path, _dest: &Path) -> Result<DelegateSnapshotResult> {
        Ok(DelegateSnapshotResult {
            snapshot_path: self.snapshot_path.clone(),
            worktree_path: self.worktree_path.clone(),
            bind_mounted: false,
        })
    }

    fn delete_snapshot(&self, _worktree_path: &Path) -> Result<RemoveReport> {
        self.deletes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(RemoveReport {
            used_btrfs_delete: true,
            unmounted_bind: false,
            unmounted_overlay: false,
        })
    }
}

#[cfg(feature = "metadata")]
pub mod gc {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use anyhow::Result;

    use crate::BtrfsDelegate;
    use crate::db::{ListFilter, WorktreeDb, WorktreeKind, WorktreeStatus};
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    pub struct GcOptions {
        /// Default max age for kinds not in [`Self::max_age_by_kind`].
        /// `None` + empty map disables the age path.
        pub max_age_secs: Option<i64>,
        pub force: bool,
        pub dry_run: bool,
        /// Paths that must not be age-expired (auto path: process cwd).
        #[serde(default)]
        pub protect_paths: Vec<PathBuf>,
        /// Age-path only; equivalent to `max_age_by_kind[kind] = None`.
        /// Honored even when `force=true`. Dead-path unregister still applies.
        #[serde(default)]
        pub skip_kinds: Vec<WorktreeKind>,
        /// Per-kind override of [`Self::max_age_secs`]. `None` = never age-expire.
        /// `force` does not override never-expire.
        #[serde(default)]
        pub max_age_by_kind: BTreeMap<WorktreeKind, Option<i64>>,
    }

    /// `Some(secs)` to age-expire; `None` = never. Order: skip_kinds → map → max_age_secs.
    pub fn effective_max_age(opts: &GcOptions, kind: WorktreeKind) -> Option<i64> {
        if opts.skip_kinds.contains(&kind) {
            return None;
        }
        opts.max_age_by_kind
            .get(&kind)
            .copied()
            .unwrap_or(opts.max_age_secs)
    }

    pub(crate) fn age_path_enabled(opts: &GcOptions) -> bool {
        opts.max_age_secs.is_some() || !opts.max_age_by_kind.is_empty()
    }

    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    pub struct GcReport {
        pub dead_removed: u64,
        pub expired_removed: u64,
        pub skipped_alive: u64,
        /// Expired worktrees whose on-disk removal failed (e.g. EPERM); the
        /// record stays tracked for a later retry. serde(default) so reports
        /// from agents predating this field still deserialize.
        #[serde(default)]
        pub remove_failed: u64,
        // Rebuild / stale-registration hygiene live on `AutoGcReport` (optional
        // auto path), not on every `gc_worktrees` call.
    }

    /// `ret == 0` or non-`ESRCH` errno ⇒ alive (`EPERM`/`EACCES` included).
    #[cfg(unix)]
    fn pid_alive_from_kill(ret: i32, errno: i32) -> bool {
        ret == 0 || errno != libc::ESRCH
    }

    fn is_pid_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            // pid 0 / >i32::MAX select process groups, not a tracked creator_pid.
            if pid == 0 || pid > i32::MAX as u32 {
                return false;
            }
            let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            pid_alive_from_kill(ret, errno)
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            false
        }
    }

    /// Foreign process CWDs for GC age guards.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum LiveCwdScan {
        Ok(Vec<PathBuf>),
        /// No enumerator (Windows/FreeBSD/…); PID guards only.
        #[allow(dead_code)]
        Unsupported,
        /// Enumerator failed or unusable — age path fail-closes.
        Failed,
    }

    /// `force` bypasses CWD requirements; `Failed` blocks age deletes.
    pub(crate) fn age_path_cwd_usable(scan: &LiveCwdScan, force: bool) -> bool {
        force || matches!(scan, LiveCwdScan::Ok(_) | LiveCwdScan::Unsupported)
    }

    fn scan_contains_cwd(cwds: &[PathBuf], path: &Path) -> bool {
        let path_canon = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        cwds.iter().any(|c| {
            c.as_path() == path
                || c == &path_canon
                || dunce::canonicalize(c).is_ok_and(|cc| cc == path_canon)
        })
    }

    /// Fail closed when our own CWD is not visible in the scan.
    ///
    /// `current_dir()` errors also fail closed — we cannot confirm this
    /// process is outside every candidate path without knowing CWD.
    fn validate_cwd_scan(cwds: Vec<PathBuf>) -> LiveCwdScan {
        match std::env::current_dir() {
            Ok(cwd) if scan_contains_cwd(&cwds, &cwd) => LiveCwdScan::Ok(cwds),
            Ok(_) => LiveCwdScan::Failed,
            Err(_) => LiveCwdScan::Failed,
        }
    }

    /// Linux `/proc/<pid>/cwd`, macOS libproc; else [`LiveCwdScan::Unsupported`].
    #[cfg(target_os = "linux")]
    pub(crate) fn live_process_cwds() -> LiveCwdScan {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return LiveCwdScan::Failed;
        };
        let cwds: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.parse::<u32>().is_ok())
            })
            .filter_map(|e| std::fs::read_link(e.path().join("cwd")).ok())
            .collect();
        validate_cwd_scan(cwds)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn live_process_cwds() -> LiveCwdScan {
        macos_live_process_cwds()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn live_process_cwds() -> LiveCwdScan {
        LiveCwdScan::Unsupported
    }

    #[cfg(target_os = "macos")]
    const VIP_PATH_LEN: usize = 1024;

    /// NUL-bounded path from fixed `vip_path` (never scan past `VIP_PATH_LEN`).
    #[cfg(target_os = "macos")]
    fn vip_path_to_pathbuf(path: &[[libc::c_char; 32]; 32]) -> Option<PathBuf> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // SAFETY: fixed-size array layout matches MAXPATHLEN vip_path.
        let bytes = unsafe { std::slice::from_raw_parts(path.as_ptr().cast::<u8>(), VIP_PATH_LEN) };
        let nul = bytes.iter().position(|&b| b == 0)?;
        let s = &bytes[..nul];
        if s.is_empty() {
            return None;
        }
        Some(PathBuf::from(OsStr::from_bytes(s)))
    }

    #[cfg(target_os = "macos")]
    fn macos_live_process_cwds() -> LiveCwdScan {
        let Some(pids) = macos_list_all_pids() else {
            return LiveCwdScan::Failed;
        };
        let expected = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
        let mut out = Vec::with_capacity(pids.len());
        for pid in pids {
            if pid <= 0 {
                continue;
            }
            // SAFETY: zeroed buffer sized for PROC_PIDVNODEPATHINFO.
            let mut info = unsafe { std::mem::zeroed::<libc::proc_vnodepathinfo>() };
            let ret = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDVNODEPATHINFO,
                    0,
                    (&raw mut info).cast(),
                    expected,
                )
            };
            if ret != expected || info.pvi_cdir.vip_vi.vi_stat.vst_dev == 0 {
                continue;
            }
            if let Some(p) = vip_path_to_pathbuf(&info.pvi_cdir.vip_path) {
                out.push(p);
            }
        }
        validate_cwd_scan(out)
    }

    /// `proc_listallpids` returns a **byte** count (probe and fill), not a PID count.
    /// Convert via `size_of::<pid_t>()` / `i32` before allocating or truncating.
    #[cfg(target_os = "macos")]
    fn macos_list_all_pids() -> Option<Vec<i32>> {
        const PID_SIZE: usize = std::mem::size_of::<i32>();
        // SAFETY: null + size 0 is the documented size probe (returns bytes needed).
        let bytes_needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if bytes_needed < 1 {
            return None;
        }
        let mut capacity_pids = (bytes_needed as usize) / PID_SIZE;
        if capacity_pids < 1 {
            return None;
        }
        for _ in 0..4 {
            // Headroom for pids that appear between probe and fill.
            capacity_pids = capacity_pids
                .saturating_add(capacity_pids / 4)
                .max(capacity_pids + 32);
            let mut pids = vec![0i32; capacity_pids];
            let buf_bytes = (pids.len() * PID_SIZE) as i32;
            // SAFETY: kernel writes at most `buf_bytes` into `pids`; return is byte count.
            let n_bytes = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), buf_bytes) };
            if n_bytes < 1 {
                return None;
            }
            let n_pids = (n_bytes as usize) / PID_SIZE;
            if n_pids < pids.len() {
                pids.truncate(n_pids);
                return Some(pids);
            }
            // Buffer was full — grow and retry.
            capacity_pids = n_pids;
        }
        None
    }

    /// True if any `live_cwds` entry sits inside `wt_path` (raw + canonical).
    fn cwd_within(wt_path: &Path, live_cwds: &[PathBuf]) -> bool {
        let wt_canon = dunce::canonicalize(wt_path).unwrap_or_else(|_| wt_path.to_path_buf());
        live_cwds.iter().any(|cwd| {
            if cwd.starts_with(wt_path) || cwd.starts_with(&wt_canon) {
                return true;
            }
            match dunce::canonicalize(cwd) {
                Ok(cwd_canon) => cwd_canon.starts_with(wt_path) || cwd_canon.starts_with(&wt_canon),
                Err(_) => false,
            }
        })
    }

    fn last_active(rec: &crate::db::WorktreeRecord) -> i64 {
        rec.last_accessed_at
            .unwrap_or(rec.created_at)
            .max(rec.created_at)
    }

    fn is_guarded(rec: &crate::db::WorktreeRecord, live_cwds: &[PathBuf]) -> bool {
        rec.creator_pid.is_some_and(is_pid_alive) || cwd_within(Path::new(&rec.path), live_cwds)
    }

    fn is_path_protected(wt_path: &Path, protect_paths: &[PathBuf]) -> bool {
        !protect_paths.is_empty() && cwd_within(wt_path, protect_paths)
    }

    /// Expired + unguarded + not path-protected + not never-expire (pre-remove re-check).
    fn is_reclaimable(
        rec: &crate::db::WorktreeRecord,
        now: i64,
        live_cwds: &[PathBuf],
        opts: &GcOptions,
    ) -> bool {
        let Some(max_age) = effective_max_age(opts, rec.kind) else {
            return false;
        };
        let cutoff = now.saturating_sub(max_age.max(0));
        if is_path_protected(Path::new(&rec.path), &opts.protect_paths) {
            return false;
        }
        last_active(rec) < cutoff && !is_guarded(rec, live_cwds)
    }

    pub fn gc_worktrees(db: &WorktreeDb, opts: &GcOptions) -> Result<GcReport> {
        gc_worktrees_with_delegate(db, opts, None)
    }

    /// Like [`gc_worktrees`], but uses `delegate` to reclaim btrfs snapshots in
    /// the expired path so rootless hosts (no `CAP_SYS_ADMIN`) can delete
    /// snapshots via a privileged helper instead of leaking them on EPERM.
    pub fn gc_worktrees_with_delegate(
        db: &WorktreeDb,
        opts: &GcOptions,
        delegate: Option<Arc<dyn BtrfsDelegate>>,
    ) -> Result<GcReport> {
        let mut report = GcReport::default();
        let now = crate::db::now_epoch_secs();

        // Dead-record reclamation.
        if opts.dry_run {
            // A dry run must not mutate: skip sweep_dead (which flips records to
            // dead) and only COUNT what a real run would reclaim — records that
            // are already dead, or alive with a path that no longer exists (what
            // sweep_dead would mark dead and then unregister).
            let all = db.list(&ListFilter {
                include_dead: true,
                ..Default::default()
            })?;
            report.dead_removed = all
                .iter()
                .filter(|r| r.status == WorktreeStatus::Dead || !Path::new(&r.path).exists())
                .count() as u64;
        } else {
            db.sweep_dead()?;
            let dead = db.list(&ListFilter {
                status: Some(WorktreeStatus::Dead),
                include_dead: true,
                ..Default::default()
            })?;
            for rec in dead {
                // Count only when the row was actually removed.
                if db.unregister(&rec.id).unwrap_or(false) {
                    report.dead_removed += 1;
                }
            }
        }

        // Expired alive-worktree reclamation (liveness-guarded).
        // First-pass CWD scan (cheap filter). Pre-remove re-check rescans so a
        // process that chdir'd into the tree after this snapshot is still guarded.
        // Failed scan → fail closed.
        if age_path_enabled(opts) {
            let cwd_scan = if opts.force {
                LiveCwdScan::Ok(Vec::new())
            } else {
                live_process_cwds()
            };
            if !age_path_cwd_usable(&cwd_scan, opts.force) {
                tracing::warn!(
                    "process CWD scan failed or unusable; skipping age-expiry (fail closed)"
                );
                return Ok(report);
            }
            let live_cwds: &[PathBuf] = match &cwd_scan {
                LiveCwdScan::Ok(v) => v.as_slice(),
                LiveCwdScan::Unsupported => &[],
                LiveCwdScan::Failed => unreachable!("gated by age_path_cwd_usable"),
            };
            let alive = db.list(&ListFilter::default())?;
            for rec in alive {
                let Some(max_age) = effective_max_age(opts, rec.kind) else {
                    // Metrics-only: never-expire kinds are retained on purpose.
                    // Prefer global `max_age_secs` as the "would have expired"
                    // reference when set; when only per-kind ages apply
                    // (`max_age_secs = None`), still count so dry-run/dogfood
                    // skip totals are not under-counted.
                    if let Some(ref_age) = opts.max_age_secs {
                        let ref_cutoff = now.saturating_sub(ref_age.max(0));
                        if last_active(&rec) < ref_cutoff {
                            report.skipped_alive += 1;
                        }
                    } else {
                        report.skipped_alive += 1;
                    }
                    continue;
                };
                let cutoff = now.saturating_sub(max_age.max(0));
                if last_active(&rec) >= cutoff {
                    continue;
                }
                let path = Path::new(&rec.path);
                if !opts.force
                    && (is_guarded(&rec, live_cwds) || is_path_protected(path, &opts.protect_paths))
                {
                    report.skipped_alive += 1;
                    continue;
                }
                if opts.dry_run {
                    if path.exists() {
                        report.expired_removed += 1;
                    }
                    continue;
                }
                // Fresh DB row + fresh CWD scan immediately before remove.
                if !opts.force {
                    match db.get_by_id(&rec.id) {
                        Ok(Some(fresh)) => {
                            let recheck_scan = live_process_cwds();
                            if !age_path_cwd_usable(&recheck_scan, false) {
                                tracing::warn!(
                                    "process CWD scan failed on pre-remove re-check; skipping remaining age-expiry (fail closed)"
                                );
                                return Ok(report);
                            }
                            let recheck_cwds: &[PathBuf] = match &recheck_scan {
                                LiveCwdScan::Ok(v) => v.as_slice(),
                                LiveCwdScan::Unsupported => &[],
                                LiveCwdScan::Failed => {
                                    unreachable!("gated by age_path_cwd_usable")
                                }
                            };
                            if !is_reclaimable(&fresh, now, recheck_cwds, opts) {
                                report.skipped_alive += 1;
                                continue;
                            }
                        }
                        Ok(None) | Err(_) => continue,
                    }
                }
                if path.exists() {
                    match super::remove_worktree_with_delegate(path, delegate.clone()) {
                        Ok(_) => report.expired_removed += 1,
                        Err(e) => {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "failed to remove expired worktree"
                            );
                            report.remove_failed += 1;
                        }
                    }
                } else if db.unregister(&rec.id).unwrap_or(false) {
                    report.expired_removed += 1;
                }
            }
        }

        Ok(report)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[cfg(unix)]
        #[test]
        fn pid_alive_from_kill_decodes_errno() {
            // Testable without needing a process in each errno state.
            assert!(pid_alive_from_kill(0, 0), "ret==0 ⇒ alive");
            assert!(!pid_alive_from_kill(-1, libc::ESRCH), "ESRCH ⇒ dead");
            assert!(
                pid_alive_from_kill(-1, libc::EPERM),
                "EPERM ⇒ alive (owned by another user)"
            );
            assert!(pid_alive_from_kill(-1, libc::EACCES), "EACCES ⇒ alive");
        }

        #[cfg(unix)]
        #[test]
        fn is_pid_alive_true_for_running_processes() {
            assert!(is_pid_alive(std::process::id()));
            // PID 1 (init/launchd) always exists on Unix.
            assert!(is_pid_alive(1), "pid 1 must be detected as alive");
        }

        #[cfg(not(unix))]
        #[test]
        fn is_pid_alive_never_false_alive_on_non_unix() {
            // Safe fallback: never report a pid as alive without a real probe.
            assert!(!is_pid_alive(std::process::id()));
            assert!(!is_pid_alive(0));
            assert!(!is_pid_alive(u32::MAX));
        }

        #[cfg(unix)]
        #[test]
        fn is_pid_alive_false_for_guarded_pids() {
            // pid 0 and pid > i32::MAX are process-group selectors to kill(2), not
            // real tracked pids; the guard short-circuits them to dead.
            assert!(!is_pid_alive(0));
            assert!(!is_pid_alive(u32::MAX));
        }

        #[cfg(unix)]
        #[test]
        fn is_pid_alive_false_for_reaped_child() {
            // A fully reaped child's pid is gone (ESRCH) and must read as dead.
            let mut child = std::process::Command::new("true")
                .spawn()
                .expect("spawn `true`");
            let pid = child.id();
            child.wait().expect("wait on `true`");
            assert!(!is_pid_alive(pid));
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        #[test]
        fn live_process_cwds_includes_own_cwd_after_chdir() {
            let _cwd_lock = crate::api::cwd_test_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            let dir = dunce::canonicalize(tmp.path()).unwrap();
            let _cwd = crate::api::CwdGuard(std::env::current_dir().unwrap());
            std::env::set_current_dir(&dir).expect("chdir into temp");
            let LiveCwdScan::Ok(cwds) = live_process_cwds() else {
                panic!("CWD scan must succeed on this OS after chdir");
            };
            assert!(
                scan_contains_cwd(&cwds, &dir),
                "own CWD {dir:?} must appear in live_process_cwds (got {} entries)",
                cwds.len()
            );
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        #[test]
        fn live_process_cwds_ok_and_nonempty_on_supported_os() {
            let _cwd_lock = crate::api::cwd_test_guard();
            match live_process_cwds() {
                LiveCwdScan::Ok(cwds) => assert!(
                    !cwds.is_empty(),
                    "process CWD scan must observe at least one CWD on this OS"
                ),
                other => panic!("expected LiveCwdScan::Ok on supported OS, got {other:?}"),
            }
        }

        #[test]
        fn validate_cwd_scan_fails_closed_when_self_missing() {
            let _cwd_lock = crate::api::cwd_test_guard();
            assert!(
                matches!(validate_cwd_scan(Vec::new()), LiveCwdScan::Failed),
                "empty scan cannot observe self CWD"
            );
            assert!(
                matches!(
                    validate_cwd_scan(vec![PathBuf::from("/no/such/unrelated/cwd")]),
                    LiveCwdScan::Failed
                ),
                "unrelated paths only ⇒ unusable scan"
            );
            let cwd = std::env::current_dir().unwrap();
            match validate_cwd_scan(vec![cwd.clone()]) {
                LiveCwdScan::Ok(v) => {
                    assert_eq!(v.len(), 1);
                    assert_eq!(v[0], cwd);
                }
                other => panic!("self path must validate: {other:?}"),
            }
        }

        #[test]
        fn age_path_cwd_usable_fail_closed_on_failed_scan() {
            assert!(
                !age_path_cwd_usable(&LiveCwdScan::Failed, false),
                "Failed scan must block age path"
            );
            assert!(
                age_path_cwd_usable(&LiveCwdScan::Failed, true),
                "force bypasses CWD scan requirement"
            );
            assert!(age_path_cwd_usable(
                &LiveCwdScan::Ok(vec![PathBuf::from("/")]),
                false
            ));
            assert!(age_path_cwd_usable(&LiveCwdScan::Unsupported, false));
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn vip_path_to_pathbuf_respects_nul_bound() {
            let mut raw = [[0 as libc::c_char; 32]; 32];
            // "ab" then NUL — rest garbage must not be read.
            raw[0][0] = b'a' as libc::c_char;
            raw[0][1] = b'b' as libc::c_char;
            raw[0][2] = 0;
            raw[0][3] = b'x' as libc::c_char;
            let p = vip_path_to_pathbuf(&raw).expect("path");
            assert_eq!(p, PathBuf::from("ab"));
            // All zeros → None
            assert!(vip_path_to_pathbuf(&[[0 as libc::c_char; 32]; 32]).is_none());
            // No NUL within 1024 → None (not UB)
            let no_nul = [[b'z' as libc::c_char; 32]; 32];
            assert!(vip_path_to_pathbuf(&no_nul).is_none());
        }

        #[test]
        fn cwd_within_matches_nested_and_canonical_paths() {
            let tmp = tempfile::TempDir::new().unwrap();
            let wt = tmp.path().join("wt");
            std::fs::create_dir_all(wt.join("a").join("b")).unwrap();
            // A CWD nested in the tree counts as live; a sibling dir does not.
            assert!(cwd_within(&wt, &[wt.join("a").join("b")]));
            assert!(!cwd_within(&wt, &[tmp.path().join("other")]));
            assert!(!cwd_within(&wt, &[]));
            // protect/cwd entry matching after canonicalize (raw path may differ).
            let nested = wt.join("a").join("b");
            let nested_canon = dunce::canonicalize(&nested).unwrap();
            let wt_canon = dunce::canonicalize(&wt).unwrap();
            assert!(cwd_within(&wt_canon, &[nested]));
            assert!(cwd_within(&wt, &[nested_canon]));
        }

        fn rec_at(path: &str, created_at: i64) -> crate::db::WorktreeRecord {
            crate::db::WorktreeRecord {
                id: "r".to_string(),
                path: path.into(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: crate::db::WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at,
                last_accessed_at: None,
                status: WorktreeStatus::Alive,
                metadata: None,
            }
        }

        #[test]
        fn is_reclaimable_requires_expired_and_unguarded() {
            let now = 1_000;
            // max_age=0 → cutoff=now; created_at=1 is expired.
            let base = GcOptions {
                max_age_secs: Some(0),
                ..Default::default()
            };
            assert!(is_reclaimable(&rec_at("/no/such/wt", 1), now, &[], &base));
            // A recent last_accessed_at within the window protects it.
            let mut fresh = rec_at("/no/such/wt", 1);
            fresh.last_accessed_at = Some(now + 10);
            assert!(!is_reclaimable(&fresh, now, &[], &base));
            // A live creator pid protects it (Unix probe only; non-Unix is never alive).
            let mut live_creator = rec_at("/no/such/wt", 1);
            live_creator.creator_pid = Some(std::process::id());
            #[cfg(unix)]
            assert!(!is_reclaimable(&live_creator, now, &[], &base));
            #[cfg(not(unix))]
            assert!(
                is_reclaimable(&live_creator, now, &[], &base),
                "non-Unix PID probe is fail-closed (never alive)"
            );
            // A live process CWD inside the tree protects it.
            let inside = std::path::PathBuf::from("/no/such/wt/sub");
            assert!(!is_reclaimable(
                &rec_at("/no/such/wt", 1),
                now,
                &[inside],
                &base
            ));
            // protect_paths hit (same semantics as CWD) blocks reclaim.
            let protect = std::path::PathBuf::from("/no/such/wt/nested");
            let with_protect = GcOptions {
                max_age_secs: Some(0),
                protect_paths: vec![protect],
                ..Default::default()
            };
            assert!(!is_reclaimable(
                &rec_at("/no/such/wt", 1),
                now,
                &[],
                &with_protect
            ));
            // skip_kinds / never-expire blocks reclaim even when otherwise expired.
            let skip = GcOptions {
                max_age_secs: Some(0),
                skip_kinds: vec![WorktreeKind::Session],
                ..Default::default()
            };
            assert!(!is_reclaimable(&rec_at("/no/such/wt", 1), now, &[], &skip));
            let never_map = GcOptions {
                max_age_secs: Some(0),
                max_age_by_kind: [(WorktreeKind::Session, None)].into_iter().collect(),
                ..Default::default()
            };
            assert!(!is_reclaimable(
                &rec_at("/no/such/wt", 1),
                now,
                &[],
                &never_map
            ));
            // Finite per-kind cutoff: age=100 with max=50 → reclaimable; max=200 → not.
            let mut aged = rec_at("/no/such/wt", 1);
            aged.last_accessed_at = Some(now - 100);
            let short = GcOptions {
                max_age_secs: Some(10_000),
                max_age_by_kind: [(WorktreeKind::Session, Some(50))].into_iter().collect(),
                ..Default::default()
            };
            assert!(is_reclaimable(&aged, now, &[], &short));
            let long = GcOptions {
                max_age_secs: Some(10),
                max_age_by_kind: [(WorktreeKind::Session, Some(200))].into_iter().collect(),
                ..Default::default()
            };
            assert!(!is_reclaimable(&aged, now, &[], &long));
        }

        #[test]
        fn effective_max_age_precedence() {
            let opts = GcOptions {
                max_age_secs: Some(100),
                skip_kinds: vec![WorktreeKind::Manual],
                max_age_by_kind: [
                    (WorktreeKind::Subagent, Some(10)),
                    (WorktreeKind::Pool, None),
                    // Conflict: map says expire Manual, skip_kinds wins.
                    (WorktreeKind::Manual, Some(1)),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            };
            assert_eq!(effective_max_age(&opts, WorktreeKind::Session), Some(100));
            assert_eq!(effective_max_age(&opts, WorktreeKind::Subagent), Some(10));
            assert_eq!(effective_max_age(&opts, WorktreeKind::Pool), None);
            assert_eq!(
                effective_max_age(&opts, WorktreeKind::Manual),
                None,
                "skip_kinds beats max_age_by_kind for the same kind"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_out_of_disk_detects_storage_full_kind() {
        // Cross-platform: std maps ENOSPC and the Windows disk-full codes onto
        // ErrorKind::StorageFull, so the typed check fires on every OS.
        let io = std::io::Error::from(std::io::ErrorKind::StorageFull);
        let err = anyhow::Error::new(io).context("failed to copy index from a to b");
        assert!(is_out_of_disk(&err));
    }

    #[cfg(unix)]
    #[test]
    fn is_out_of_disk_detects_enospc_io_error() {
        // Real ENOSPC (errno 28 on Linux/macOS) must decode to StorageFull.
        let io = std::io::Error::from_raw_os_error(28);
        assert_eq!(io.kind(), std::io::ErrorKind::StorageFull);
        let err = anyhow::Error::new(io).context("failed to copy index from a to b");
        assert!(is_out_of_disk(&err));
    }

    #[cfg(windows)]
    #[test]
    fn is_out_of_disk_detects_windows_disk_full_codes() {
        // Windows reports a full disk as ERROR_DISK_FULL (112) or
        // ERROR_HANDLE_DISK_FULL (39); std decodes both to StorageFull.
        for code in [112, 39] {
            let io = std::io::Error::from_raw_os_error(code);
            assert_eq!(io.kind(), std::io::ErrorKind::StorageFull);
            let err = anyhow::Error::new(io).context("failed to copy index from a to b");
            assert!(is_out_of_disk(&err));
        }
    }

    #[test]
    fn is_out_of_disk_detects_message_text() {
        // `git` subcommands surface ENOSPC only as stderr text.
        let err = anyhow::anyhow!("git worktree add failed: No space left on device");
        assert!(is_out_of_disk(&err));
    }

    #[test]
    fn is_out_of_disk_ignores_unrelated_errors() {
        let err = anyhow::anyhow!("failed to get HEAD commit from source");
        assert!(!is_out_of_disk(&err));
    }

    #[test]
    fn annotate_disk_full_promotes_reason_to_top_context() {
        let err = anyhow::anyhow!("failed to copy index: No space left on device (os error 28)");
        let annotated = annotate_disk_full(err);
        // Display (top context only) now carries the disk reason, so it
        // survives the workspace/ACP flattening to a single message.
        assert_eq!(annotated.to_string(), OUT_OF_DISK_CONTEXT);
        // The original chain is preserved underneath for logs.
        assert!(format!("{annotated:#}").contains("failed to copy index"));
    }

    #[test]
    fn annotate_disk_full_leaves_other_errors_unchanged() {
        let err = anyhow::anyhow!("some other failure");
        assert_eq!(annotate_disk_full(err).to_string(), "some other failure");
    }

    #[test]
    fn test_copy_report_from_copy_stats() {
        let stats = CopyStats {
            files_copied: 10,
            dirs_created: 3,
            symlinks_copied: 2,
            files_skipped: 5,
            issues: vec!["warning 1".to_string(), "warning 2".to_string()],
        };

        let report: CopyReport = stats.into();
        assert_eq!(report.files_copied, 10);
        assert_eq!(report.dirs_created, 3);
        assert_eq!(report.symlinks_copied, 2);
        assert_eq!(report.files_skipped, 5);
        assert_eq!(report.issues.len(), 2);
        assert!(report.dirty_files.is_none());
    }

    #[test]
    fn test_btrfs_mode_default() {
        let mode = BtrfsMode::default();
        assert_eq!(mode, BtrfsMode::Auto);
    }

    #[test]
    fn test_btrfs_mode_variants() {
        // Test that all variants can be created and compared
        assert_eq!(BtrfsMode::Auto, BtrfsMode::Auto);
        assert_eq!(BtrfsMode::Force, BtrfsMode::Force);
        assert_eq!(BtrfsMode::Disabled, BtrfsMode::Disabled);

        assert_ne!(BtrfsMode::Auto, BtrfsMode::Force);
        assert_ne!(BtrfsMode::Auto, BtrfsMode::Disabled);
        assert_ne!(BtrfsMode::Force, BtrfsMode::Disabled);
    }

    #[test]
    fn test_btrfs_mode_debug() {
        // Test that Debug is implemented
        let auto = format!("{:?}", BtrfsMode::Auto);
        let force = format!("{:?}", BtrfsMode::Force);
        let disabled = format!("{:?}", BtrfsMode::Disabled);

        assert!(auto.contains("Auto"));
        assert!(force.contains("Force"));
        assert!(disabled.contains("Disabled"));
    }

    #[test]
    fn test_btrfs_mode_clone() {
        let mode = BtrfsMode::Force;
        let cloned = mode.clone();
        assert_eq!(mode, cloned);
    }

    #[test]
    fn test_creation_mode_default() {
        let mode = CreationMode::default();
        assert_eq!(mode, CreationMode::Linked);
    }

    #[test]
    fn test_creation_mode_variants() {
        assert_eq!(CreationMode::Linked, CreationMode::Linked);
        assert_eq!(CreationMode::Standalone, CreationMode::Standalone);
        assert_eq!(CreationMode::GitCheckout, CreationMode::GitCheckout);
        assert_ne!(CreationMode::Linked, CreationMode::Standalone);
        assert_ne!(CreationMode::Linked, CreationMode::GitCheckout);
    }

    #[test]
    fn test_worktree_builder_chain() {
        // Test that all builder methods can be chained
        let _builder = WorktreeBuilder::new("/source", "/dest")
            .git_ref("main")
            .parallelism(4)
            .ignored_parallelism(2)
            .channel_buffer(512)
            .working_tree_mode(WorkingTreeMode::CleanAll)
            .ignored_files_mode(IgnoredFilesMode::Copy {
                skip_patterns: vec!["*.log".to_string()],
            })
            .creation_mode(CreationMode::GitCheckout);
    }

    #[test]
    fn test_standalone_shorthand() {
        // .standalone(true) should be equivalent to .creation_mode(Standalone)
        let _builder = WorktreeBuilder::new("/source", "/dest").standalone(true);
    }

    #[test]
    fn copy_ignored_only_returns_err_when_cancelled() {
        let src = tempfile::TempDir::new().unwrap();
        let dest = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("file.txt"), "content").unwrap();

        let token = CancellationToken::new();
        token.cancel();

        let err = WorktreeBuilder::new(src.path(), dest.path())
            .cancellation_token(token)
            .copy_ignored_only()
            .expect_err("a pre-cancelled token must produce an error, not Ok(partial)");

        assert!(
            err.to_string().contains("cancelled"),
            "error should report cancellation, got: {err}"
        );
    }

    #[test]
    fn test_cleanup_report_default() {
        let report = CleanupReport::default();
        assert_eq!(report.removed, 0);
        assert_eq!(report.overlays_unmounted, 0);
        assert_eq!(report.btrfs_deleted, 0);
        assert_eq!(report.errors, 0);
    }

    #[test]
    fn test_cleanup_worktrees_in_empty_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let report = cleanup_worktrees_in(tmp.path());
        assert_eq!(report.removed, 0);
        assert_eq!(report.errors, 0);
    }

    #[test]
    fn test_cleanup_worktrees_in_missing_dir() {
        let report = cleanup_worktrees_in(std::path::Path::new("/nonexistent/path/xyz"));
        assert_eq!(report.removed, 0);
        assert_eq!(report.errors, 0);
    }

    #[test]
    fn test_cleanup_worktrees_in_with_plain_worktrees() {
        xai_test_utils::require_git!();
        use xai_test_utils::git::{git_commit_all, init_git_repo};

        let tmp = tempfile::TempDir::new().unwrap();

        // Create a source repo.
        let repo_path = tmp.path().join("repo");
        std::fs::create_dir(&repo_path).unwrap();
        init_git_repo(&repo_path);
        std::fs::write(repo_path.join("file.txt"), "content").unwrap();
        git_commit_all(&repo_path, "initial");

        // Create two worktrees in a worktrees dir.
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        let wt1 = worktrees_dir.join("wt1");
        let wt2 = worktrees_dir.join("wt2");

        WorktreeBuilder::new(&repo_path, &wt1).create().unwrap();
        WorktreeBuilder::new(&repo_path, &wt2).create().unwrap();

        assert!(wt1.exists());
        assert!(wt2.exists());

        // Cleanup should remove both.
        let report = cleanup_worktrees_in(&worktrees_dir);
        assert_eq!(report.removed, 2);
        assert_eq!(report.errors, 0);
        assert!(!wt1.exists());
        assert!(!wt2.exists());
    }

    #[test]
    fn test_cleanup_worktrees_in_with_nested_dirs() {
        xai_test_utils::require_git!();
        use xai_test_utils::git::{git_commit_all, init_git_repo};

        let tmp = tempfile::TempDir::new().unwrap();

        // Create a source repo.
        let repo_path = tmp.path().join("repo");
        std::fs::create_dir(&repo_path).unwrap();
        init_git_repo(&repo_path);
        std::fs::write(repo_path.join("file.txt"), "content").unwrap();
        git_commit_all(&repo_path, "initial");

        // Create ~/.grok/worktrees/<repo>/<session>/ structure.
        let worktrees_dir = tmp.path().join("worktrees");
        let repo_group = worktrees_dir.join("myrepo");
        std::fs::create_dir_all(&repo_group).unwrap();

        let wt1 = repo_group.join("session-1");
        WorktreeBuilder::new(&repo_path, &wt1).create().unwrap();
        assert!(wt1.exists());

        // Cleanup should find the nested worktree.
        let report = cleanup_worktrees_in(&worktrees_dir);
        assert_eq!(report.removed, 1);
        assert_eq!(report.errors, 0);
        assert!(!wt1.exists());
        // Grouping dir should be removed since it's empty now.
        assert!(!repo_group.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_cleanup_worktrees_in_removes_dangling_symlink() {
        // A worktree exposed as a symlink whose snapshot was already deleted is a
        // dangling symlink; it must be unlinked, not skipped (`is_dir()` follows
        // the link and returns false, which would leak it).
        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        let dangling = worktrees_dir.join("dead-wt");
        std::os::unix::fs::symlink(tmp.path().join("gone-snapshot"), &dangling).unwrap();
        assert!(dangling.symlink_metadata().is_ok());
        assert!(!dangling.is_dir(), "precondition: dangling symlink");

        let report = cleanup_worktrees_in(&worktrees_dir);

        assert!(
            dangling.symlink_metadata().is_err(),
            "dangling symlink worktree must be removed, not skipped"
        );
        assert_eq!(report.removed, 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_cleanup_worktrees_in_removes_nested_dangling_symlink() {
        // Dangling symlink one level deeper (~/.grok/worktrees/<repo>/<session>):
        // the nested branch must also unlink it rather than skip it.
        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        // A grouping dir with NO `.git`, so cleanup recurses into it.
        let repo_group = worktrees_dir.join("myrepo");
        std::fs::create_dir_all(&repo_group).unwrap();

        let dangling = repo_group.join("dead-session");
        std::os::unix::fs::symlink(tmp.path().join("gone-snapshot"), &dangling).unwrap();
        assert!(!dangling.is_dir(), "precondition: dangling symlink");

        let report = cleanup_worktrees_in(&worktrees_dir);

        assert!(
            dangling.symlink_metadata().is_err(),
            "nested dangling symlink worktree must be removed"
        );
        assert_eq!(report.removed, 1);
    }

    #[test]
    fn test_remove_report_has_overlay_field() {
        let report = RemoveReport {
            used_btrfs_delete: false,
            unmounted_bind: false,
            unmounted_overlay: true,
        };
        assert!(report.unmounted_overlay);
        assert!(!report.used_btrfs_delete);
    }

    #[test]
    fn test_creation_mode_as_db_str() {
        assert_eq!(CreationMode::Linked.as_db_str(), "linked");
        assert_eq!(CreationMode::Standalone.as_db_str(), "standalone");
        assert_eq!(CreationMode::GitCheckout.as_db_str(), "git");
    }

    #[test]
    fn test_remove_worktree_with_delegate_no_delegate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nonexistent");
        let result = remove_worktree_with_delegate(&path, None);
        assert!(result.is_ok());
        let report = result.unwrap();
        assert!(!report.used_btrfs_delete);
        assert!(!report.unmounted_bind);
        assert!(!report.unmounted_overlay);
    }

    #[test]
    fn test_remove_worktree_with_delegate_existing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("some-dir");
        std::fs::create_dir(&path).unwrap();
        let result = remove_worktree_with_delegate(&path, None);
        assert!(result.is_ok());
        assert!(!path.exists());
    }

    /// A plain (non-snapshot) linked worktree removed through the delegate-aware
    /// path must still deregister `.git/worktrees/<name>`, and the delegate must
    /// be used only as a fallback — never invoked when the direct removal succeeds.
    #[test]
    fn remove_with_delegate_deregisters_plain_worktree_without_calling_delegate() {
        xai_test_utils::require_git!();
        use xai_test_utils::git::{git_commit_all, init_git_repo};
        // Isolate GROK_HOME so the post-removal unregister writes to a private DB.
        #[cfg(feature = "metadata")]
        let _fx = crate::db::GrokHomeFixture::new();

        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::write(repo.join("file.txt"), "content").unwrap();
        git_commit_all(&repo, "initial");

        let wt = tmp.path().join("worktrees").join("wt1");
        WorktreeBuilder::new(&repo, &wt).create().unwrap();

        // `.git` is a file pointing at `<repo>/.git/worktrees/<name>`.
        let registration_dir =
            read_worktree_gitdir(&wt).expect("linked worktree must have a gitdir pointer");
        assert!(
            registration_dir.exists(),
            "precondition: registration exists"
        );

        let deletes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delegate: Arc<dyn BtrfsDelegate> = Arc::new(RecordingDelegate {
            snapshot_path: PathBuf::from("/unused"),
            worktree_path: PathBuf::from("/unused"),
            deletes: deletes.clone(),
        });

        let report = remove_worktree_with_delegate(&wt, Some(delegate)).unwrap();

        assert!(!wt.exists(), "worktree directory must be removed");
        assert!(
            !registration_dir.exists(),
            "`.git/worktrees/<name>` registration must be deregistered"
        );
        assert!(!report.used_btrfs_delete);
        assert_eq!(
            deletes.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "delegate is a fallback only; a plain worktree removal must not call it"
        );
    }

    /// When the direct `btrfs delete` fails (e.g. EPERM on a rootless host),
    /// the snapshot delete must fall back to the delegate and return its report.
    #[cfg(target_os = "linux")]
    #[test]
    fn delete_fallback_invokes_delegate_when_direct_delete_fails() {
        let deletes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delegate: Arc<dyn BtrfsDelegate> = Arc::new(RecordingDelegate {
            snapshot_path: PathBuf::from("/unused"),
            worktree_path: PathBuf::from("/unused"),
            deletes: deletes.clone(),
        });

        let report = delete_snapshot_with_delegate_fallback(
            Path::new("/mnt/btrfs/worktrees/snap-1"),
            Path::new("/home/u/.grok/worktrees/repo/wt"),
            Some(&delegate),
            |_| anyhow::bail!("operation not permitted (os error 1)"),
        )
        .unwrap()
        .expect("delegate fallback must handle the failed direct delete");
        assert!(report.used_btrfs_delete);
        assert_eq!(deletes.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// A successful direct delete returns `None` (caller does local cleanup) and
    /// never touches the delegate.
    #[cfg(target_os = "linux")]
    #[test]
    fn delete_fallback_skips_delegate_when_direct_delete_succeeds() {
        let deletes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delegate: Arc<dyn BtrfsDelegate> = Arc::new(RecordingDelegate {
            snapshot_path: PathBuf::from("/unused"),
            worktree_path: PathBuf::from("/unused"),
            deletes: deletes.clone(),
        });

        let res = delete_snapshot_with_delegate_fallback(
            Path::new("/snap"),
            Path::new("/wt"),
            Some(&delegate),
            |_| Ok(()),
        )
        .unwrap();
        assert!(res.is_none(), "successful direct delete must return None");
        assert_eq!(deletes.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// With no delegate, a failed direct delete propagates the original error so
    /// the worktree reference is preserved for a retry.
    #[cfg(target_os = "linux")]
    #[test]
    fn delete_fallback_without_delegate_propagates_error() {
        let err = delete_snapshot_with_delegate_fallback(
            Path::new("/snap"),
            Path::new("/wt"),
            None,
            |_| anyhow::bail!("EPERM marker"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("EPERM marker"));
    }

    /// Metadata persisted by the public `write_btrfs_metadata` lets metadata-based
    /// removal locate a worktree purely from its `mount_target` and drop the
    /// symlink + metadata, even when the snapshot subvolume is already gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn metadata_written_by_public_writer_is_found_by_metadata_removal() {
        use crate::btrfs;
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let mount = tmp.path();
        let worktrees_dir = mount.join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        // `dest` is a symlink to a snapshot under <mount>/worktrees/, with metadata.
        let snapshot_path = worktrees_dir.join("snap-1");
        let dest = tmp.path().join("dest-worktree");
        std::os::unix::fs::symlink(&snapshot_path, &dest).unwrap();

        btrfs::write_btrfs_metadata(&snapshot_path, &dest).unwrap();
        let meta_path = btrfs::btrfs_meta_path(&snapshot_path).unwrap();
        assert!(
            meta_path.exists(),
            "metadata must be written next to snapshot"
        );
        assert!(
            dest.symlink_metadata().is_ok(),
            "precondition: symlink exists"
        );

        let entries = vec![MountEntry {
            mount_id: 1,
            parent_id: 0,
            root: "/".to_string(),
            mount_point: mount.to_path_buf(),
            fs_type: "btrfs".to_string(),
            source: "/dev/loop0".to_string(),
            super_options: String::new(),
        }];

        let report = try_btrfs_remove_from_metadata_inner(&dest, &entries, None)
            .unwrap()
            .expect("metadata removal must find the snapshot by mount_target");
        // Snapshot subvolume is already gone, so no btrfs delete is attempted.
        assert!(!report.used_btrfs_delete);
        assert!(
            dest.symlink_metadata().is_err(),
            "the worktree symlink must be removed"
        );
        assert!(
            !meta_path.exists(),
            "metadata must be cleaned up after handling"
        );
    }

    /// Metadata written by the public `write_btrfs_metadata` for a snapshot whose
    /// worktree is gone (orphan) must be discovered and reclaimed by the orphan
    /// scanner.
    #[cfg(target_os = "linux")]
    #[test]
    fn metadata_written_by_public_writer_is_reclaimed_by_orphan_scan() {
        use crate::btrfs;
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let mount = tmp.path();
        let worktrees_dir = mount.join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        // Orphan: the worktree's mount_target no longer exists (symlink lost),
        // so the scanner must treat it as reclaimable rather than active.
        let snapshot_path = worktrees_dir.join("snap-orphan");
        let mount_target = tmp.path().join("gone-dest");
        btrfs::write_btrfs_metadata(&snapshot_path, &mount_target).unwrap();
        let meta_path = btrfs::btrfs_meta_path(&snapshot_path).unwrap();
        assert!(meta_path.exists());

        let entries = vec![MountEntry {
            mount_id: 1,
            parent_id: 0,
            root: "/".to_string(),
            mount_point: mount.to_path_buf(),
            fs_type: "btrfs".to_string(),
            source: "/dev/loop0".to_string(),
            super_options: String::new(),
        }];

        let report = cleanup_orphaned_btrfs_snapshots_inner(&entries);
        assert_eq!(report.removed, 1, "orphaned snapshot must be reclaimed");
        // Snapshot subvolume doesn't exist on disk, so no btrfs delete is attempted.
        assert_eq!(report.btrfs_deleted, 0);
        assert!(!meta_path.exists(), "orphan metadata must be cleaned up");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_cleanup_orphaned_btrfs_snapshots_no_mounts() {
        let report = cleanup_orphaned_btrfs_snapshots_inner(&[]);
        assert_eq!(report.removed, 0);
        assert_eq!(report.errors, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_cleanup_orphaned_btrfs_snapshots_with_metadata() {
        use crate::btrfs;
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        // Orphan: the mount_target is gone but its PARENT dir exists (home is
        // restored), so the scanner can prove it's orphaned and reclaim it.
        let mount_parent = tmp.path().join("home-restored");
        std::fs::create_dir(&mount_parent).unwrap();
        let meta = btrfs::BtrfsSnapshotMetadata {
            kind: std::borrow::Cow::Borrowed("btrfs"),
            snapshot_path: worktrees_dir.join("wt-abc"),
            mount_target: mount_parent.join("gone-target"),
            created_at: "1740000000s-since-epoch".to_string(),
        };
        let meta_path = worktrees_dir.join("wt-abc.btrfs-meta.json");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        let entries = vec![MountEntry {
            mount_id: 1,
            parent_id: 0,
            root: "/".to_string(),
            mount_point: tmp.path().to_path_buf(),
            fs_type: "btrfs".to_string(),
            source: "/dev/loop0".to_string(),
            super_options: String::new(),
        }];

        let report = cleanup_orphaned_btrfs_snapshots_inner(&entries);
        assert_eq!(report.removed, 1);
        // snapshot_path doesn't exist as dir, so no btrfs delete attempted
        assert_eq!(report.btrfs_deleted, 0);
        assert!(!meta_path.exists(), "metadata should be cleaned up");
    }

    /// A snapshot whose `mount_target` parent dir is missing can't be proven orphaned,
    /// so the scanner must skip it rather than destroy one about to be re-exposed.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_cleanup_orphaned_btrfs_skips_when_mount_target_parent_missing() {
        use crate::btrfs;
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        // mount_target lives under a home dir not yet restored. Hermetic: the parent
        // is a path inside this tempdir that the test never creates.
        let snapshot_path = worktrees_dir.join("wt-live");
        std::fs::create_dir(&snapshot_path).unwrap();
        let unrestored_home = tmp.path().join("unrestored-home");
        let mount_target = unrestored_home.join(".grok/worktrees/x/wt-live");
        assert!(
            !mount_target.parent().unwrap().exists(),
            "precondition: mount_target parent must be absent"
        );
        let meta = btrfs::BtrfsSnapshotMetadata {
            kind: std::borrow::Cow::Borrowed("btrfs"),
            snapshot_path: snapshot_path.clone(),
            mount_target,
            created_at: "1740000000s-since-epoch".to_string(),
        };
        let meta_path = worktrees_dir.join("wt-live.btrfs-meta.json");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        let entries = vec![MountEntry {
            mount_id: 1,
            parent_id: 0,
            root: "/".to_string(),
            mount_point: tmp.path().to_path_buf(),
            fs_type: "btrfs".to_string(),
            source: "/dev/loop0".to_string(),
            super_options: String::new(),
        }];

        let report = cleanup_orphaned_btrfs_snapshots_inner(&entries);
        assert_eq!(
            report.removed, 0,
            "must not reclaim while orphan status is unprovable"
        );
        // The guard must skip cleanly: without it, a non-btrfs tempdir host would
        // instead error-class this as "outside scanned storage" (errors == 1).
        assert_eq!(report.errors, 0, "guard must skip cleanly, not error-class");
        assert!(
            meta_path.exists(),
            "metadata must be preserved for a later scan"
        );
        // The guard `continue`s before any delete, so the snapshot dir is untouched.
        assert!(snapshot_path.exists(), "snapshot must not be deleted");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_cleanup_orphaned_btrfs_skips_active() {
        use crate::btrfs;
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        let mount_target = std::path::PathBuf::from("/home/user/.grok/worktrees/active-wt");

        let meta = btrfs::BtrfsSnapshotMetadata {
            kind: std::borrow::Cow::Borrowed("btrfs"),
            snapshot_path: worktrees_dir.join("active-wt"),
            mount_target: mount_target.clone(),
            created_at: "1740000000s-since-epoch".to_string(),
        };
        let meta_path = worktrees_dir.join("active-wt.btrfs-meta.json");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        // mount_target appears in the mount entries (simulates active bind mount)
        let entries = vec![
            MountEntry {
                mount_id: 1,
                parent_id: 0,
                root: "/".to_string(),
                mount_point: tmp.path().to_path_buf(),
                fs_type: "btrfs".to_string(),
                source: "/dev/loop0".to_string(),
                super_options: String::new(),
            },
            MountEntry {
                mount_id: 2,
                parent_id: 1,
                root: "/worktrees/active-wt".to_string(),
                mount_point: mount_target,
                fs_type: "btrfs".to_string(),
                source: "/dev/loop0".to_string(),
                super_options: String::new(),
            },
        ];

        let report = cleanup_orphaned_btrfs_snapshots_inner(&entries);
        assert_eq!(report.removed, 0, "active snapshot should not be removed");
        assert!(
            meta_path.exists(),
            "metadata for active snapshot should remain"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_cleanup_orphaned_btrfs_skips_active_symlink() {
        // Current layout: the live worktree is a SYMLINK to the snapshot and
        // never appears in mountinfo. The orphan scanner must recognize it as
        // active (resolves to snapshot_path) and NOT delete it.
        use crate::btrfs;
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        // The snapshot dir (a plain dir here) and a live symlink pointing at it.
        let snapshot_path = worktrees_dir.join("live-wt");
        std::fs::create_dir(&snapshot_path).unwrap();
        let mount_target = tmp.path().join("worktree-symlink");
        std::os::unix::fs::symlink(&snapshot_path, &mount_target).unwrap();

        let meta = btrfs::BtrfsSnapshotMetadata {
            kind: std::borrow::Cow::Borrowed("btrfs"),
            snapshot_path: snapshot_path.clone(),
            mount_target: mount_target.clone(),
            created_at: "1740000000s-since-epoch".to_string(),
        };
        let meta_path = worktrees_dir.join("live-wt.btrfs-meta.json");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        // No mount entry references the symlink — only the btrfs mount itself.
        let entries = vec![MountEntry {
            mount_id: 1,
            parent_id: 0,
            root: "/".to_string(),
            mount_point: tmp.path().to_path_buf(),
            fs_type: "btrfs".to_string(),
            source: "/dev/loop0".to_string(),
            super_options: String::new(),
        }];

        let report = cleanup_orphaned_btrfs_snapshots_inner(&entries);
        assert_eq!(report.removed, 0, "active symlink worktree must be kept");
        assert!(
            meta_path.exists(),
            "metadata for live worktree should remain"
        );
        assert!(snapshot_path.exists(), "snapshot must not be deleted");
        assert!(
            mount_target.symlink_metadata().is_ok(),
            "live symlink must not be removed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_symlink_resolves_to() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("worktrees").join("snap");
        std::fs::create_dir_all(&target).unwrap();

        // Absolute-target symlink resolving to `target` → true.
        let abs_link = tmp.path().join("abs-link");
        std::os::unix::fs::symlink(&target, &abs_link).unwrap();
        assert!(symlink_resolves_to(&abs_link, &target));

        // Relative-target symlink resolving (via link.parent()) to `target` → true.
        let rel_link = tmp.path().join("rel-link");
        std::os::unix::fs::symlink(std::path::Path::new("worktrees/snap"), &rel_link).unwrap();
        assert!(symlink_resolves_to(&rel_link, &target));

        // Symlink resolving elsewhere → false.
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let wrong_link = tmp.path().join("wrong-link");
        std::os::unix::fs::symlink(&other, &wrong_link).unwrap();
        assert!(!symlink_resolves_to(&wrong_link, &target));

        // Non-symlink path → false.
        assert!(!symlink_resolves_to(&target, &target));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_try_btrfs_remove_from_metadata_finds_match() {
        use crate::btrfs;
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        let mount_target = tmp.path().join("mount-target");
        std::fs::create_dir(&mount_target).unwrap();

        let meta = btrfs::BtrfsSnapshotMetadata {
            kind: std::borrow::Cow::Borrowed("btrfs"),
            snapshot_path: worktrees_dir.join("snap-abc"),
            mount_target: mount_target.clone(),
            created_at: "1740000000s-since-epoch".to_string(),
        };
        let meta_path = worktrees_dir.join("snap-abc.btrfs-meta.json");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        let entries = vec![MountEntry {
            mount_id: 1,
            parent_id: 0,
            root: "/".to_string(),
            mount_point: tmp.path().to_path_buf(),
            fs_type: "btrfs".to_string(),
            source: "/dev/loop0".to_string(),
            super_options: String::new(),
        }];

        // `snapshot_path` is intentionally never created, so the privileged
        // `btrfs subvolume delete` is gated out (btrfs is unavailable in CI; real
        // subvolume deletion is exercised only on a btrfs-capable host). The
        // discriminating signals here are the metadata + dir cleanup.
        let report = try_btrfs_remove_from_metadata_inner(&mount_target, &entries, None)
            .unwrap()
            .expect("should find metadata match");
        // Nothing was deleted (snapshot absent) and this dir branch unmounts
        // nothing on an already-unmounted dir.
        assert!(!report.used_btrfs_delete);
        assert!(!report.unmounted_bind);
        assert!(!meta_path.exists(), "metadata should be cleaned up");
        assert!(!mount_target.exists(), "dir worktree should be removed");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_try_btrfs_remove_from_metadata_removes_symlink_target() {
        use crate::btrfs;
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        // The on-disk snapshot dir (a plain dir here — no real btrfs subvolume,
        // so deletion is skipped, but the symlink + metadata must be cleaned up).
        let snapshot_path = worktrees_dir.join("snap-link");

        // The worktree is exposed at `mount_target` via a symlink to the snapshot.
        let mount_target = tmp.path().join("worktree-symlink");
        std::os::unix::fs::symlink(&snapshot_path, &mount_target).unwrap();
        assert!(mount_target.is_symlink());

        let meta = btrfs::BtrfsSnapshotMetadata {
            kind: std::borrow::Cow::Borrowed("btrfs"),
            snapshot_path: snapshot_path.clone(),
            mount_target: mount_target.clone(),
            created_at: "1740000000s-since-epoch".to_string(),
        };
        let meta_path = worktrees_dir.join("snap-link.btrfs-meta.json");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        let entries = vec![MountEntry {
            mount_id: 1,
            parent_id: 0,
            root: "/".to_string(),
            mount_point: tmp.path().to_path_buf(),
            fs_type: "btrfs".to_string(),
            source: "/dev/loop0".to_string(),
            super_options: String::new(),
        }];

        // NOTE: `snapshot_path` is intentionally never created here, so the
        // privileged `btrfs subvolume delete` is gated out (btrfs is unavailable
        // in CI). This test covers the symlink-vs-dir branch selection and the
        // symlink + metadata cleanup; the real subvolume deletion is exercised
        // only on a btrfs-capable host.
        let result = try_btrfs_remove_from_metadata_inner(&mount_target, &entries, None);
        assert!(result.is_ok());
        let report = result.unwrap().expect("should find metadata match");
        // The symlink branch never unmounts a bind mount.
        assert!(!report.unmounted_bind);
        // No leak: the symlink and the metadata file are both gone.
        assert!(
            mount_target.symlink_metadata().is_err(),
            "symlink worktree should be removed"
        );
        assert!(!meta_path.exists(), "metadata should be cleaned up");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_try_btrfs_remove_from_metadata_removes_legacy_dir_target() {
        // Legacy bind-mount layout: `mount_target` is a real (empty) directory.
        // Exercises the non-symlink `else` branch (umount is a no-op on an
        // already-unmounted empty dir, then `remove_dir`).
        use crate::btrfs;
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        let mount_target = tmp.path().join("mount-target");
        std::fs::create_dir(&mount_target).unwrap();
        assert!(!mount_target.is_symlink());

        let meta = btrfs::BtrfsSnapshotMetadata {
            kind: std::borrow::Cow::Borrowed("btrfs"),
            snapshot_path: worktrees_dir.join("snap-dir"),
            mount_target: mount_target.clone(),
            created_at: "1740000000s-since-epoch".to_string(),
        };
        let meta_path = worktrees_dir.join("snap-dir.btrfs-meta.json");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        let entries = vec![MountEntry {
            mount_id: 1,
            parent_id: 0,
            root: "/".to_string(),
            mount_point: tmp.path().to_path_buf(),
            fs_type: "btrfs".to_string(),
            source: "/dev/loop0".to_string(),
            super_options: String::new(),
        }];

        let report = try_btrfs_remove_from_metadata_inner(&mount_target, &entries, None)
            .unwrap()
            .expect("should find metadata match");
        // No leak: the directory and metadata are both gone.
        assert!(!mount_target.exists(), "dir worktree should be removed");
        assert!(!meta_path.exists(), "metadata should be cleaned up");
        let _ = report;
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_try_btrfs_remove_symlink_to_non_btrfs_target() {
        // A symlink whose target is not a btrfs subvolume: try_btrfs_remove
        // should remove the symlink and fall through (Ok(None) overall once the
        // now-removed path is no longer a btrfs subvolume).
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("plain-target");
        std::fs::create_dir(&target).unwrap();

        let link = tmp.path().join("worktree-symlink");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(link.is_symlink());

        let result = try_btrfs_remove(&link, None);
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "should fall through for non-btrfs"
        );
        assert!(
            link.symlink_metadata().is_err(),
            "non-btrfs symlink should be removed before falling through"
        );
        // The target itself is untouched by the symlink removal.
        assert!(target.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_try_btrfs_remove_from_metadata_no_match() {
        use crate::mount_info::MountEntry;

        let tmp = tempfile::TempDir::new().unwrap();
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).unwrap();

        let entries = vec![MountEntry {
            mount_id: 1,
            parent_id: 0,
            root: "/".to_string(),
            mount_point: tmp.path().to_path_buf(),
            fs_type: "btrfs".to_string(),
            source: "/dev/loop0".to_string(),
            super_options: String::new(),
        }];

        let result = try_btrfs_remove_from_metadata_inner(
            std::path::Path::new("/nonexistent/target"),
            &entries,
            None,
        );
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[cfg(feature = "metadata")]
    mod metadata_integration {
        use super::*;
        use crate::db::{ListFilter, WorktreeDb, WorktreeKind};

        fn db_at(tmp: &tempfile::TempDir) -> WorktreeDb {
            WorktreeDb::open(tmp.path()).unwrap()
        }

        #[test]
        fn register_worktree_writes_correct_fields() {
            // Isolate GROK_HOME so register_worktree's open_default write lands
            // in our own DB (lock + private tmp + restore via the fixture).
            let fx = crate::db::GrokHomeFixture::new();

            // Unique basename → unique id, so a concurrent open_default writer
            // (GROK_HOME is process-global) can't INSERT-OR-REPLACE our row.
            let wt_path = fx.home.join("register-fields-wt");
            std::fs::create_dir(&wt_path).unwrap();
            // register_worktree stores the canonical path (/var → /private/var on macOS).
            let wt_canon = dunce::canonicalize(&wt_path).unwrap_or_else(|_| wt_path.clone());

            super::super::register_worktree(
                &wt_path,
                std::path::Path::new("/src/repo"),
                WorktreeKind::Session,
                "linked",
                "main",
                "abc123",
                Some("test-session".to_string()),
                None,
                None,
            );

            // register_worktree wrote to open_default, which resolves to fx.home.
            // Filter to OUR record by path: concurrent tests may add rows here.
            let db = WorktreeDb::open(&fx.home).unwrap();
            let mine: Vec<_> = db
                .list(&ListFilter::default())
                .unwrap()
                .into_iter()
                .filter(|r| r.path == wt_canon)
                .collect();
            assert_eq!(mine.len(), 1);
            assert_eq!(mine[0].kind, WorktreeKind::Session);
            assert_eq!(mine[0].session_id.as_deref(), Some("test-session"));
            assert_eq!(mine[0].creation_mode, "linked");
            assert_eq!(mine[0].head_commit.as_deref(), Some("abc123"));
            assert!(mine[0].creator_pid.is_some());
        }

        #[test]
        fn unregister_worktree_removes_by_path() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let wt_path = tmp.path().join("wt");

            let record = crate::db::WorktreeRecord {
                id: "test-wt".to_string(),
                path: wt_path.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 100,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();
            assert_eq!(db.list(&ListFilter::default()).unwrap().len(), 1);

            db.unregister_by_path(&wt_path).unwrap();
            assert!(db.list(&ListFilter::default()).unwrap().is_empty());
        }

        #[test]
        fn creation_mode_as_db_str_matches_schema() {
            assert_eq!(CreationMode::Linked.as_db_str(), "linked");
            assert_eq!(CreationMode::Standalone.as_db_str(), "standalone");
            assert_eq!(CreationMode::GitCheckout.as_db_str(), "git");
        }

        #[test]
        fn gc_removes_dead_records() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);

            // Register a record with a nonexistent path
            let record = crate::db::WorktreeRecord {
                id: "dead-1".to_string(),
                path: "/nonexistent/worktree".into(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 100,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            let report = gc::gc_worktrees(&db, &gc::GcOptions::default()).unwrap();
            assert_eq!(report.dead_removed, 1);

            let all = db
                .list(&ListFilter {
                    include_dead: true,
                    ..Default::default()
                })
                .unwrap();
            assert!(all.is_empty());
        }

        #[test]
        fn gc_skips_alive_pids() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let my_pid = std::process::id();

            let record = crate::db::WorktreeRecord {
                id: "alive-wt".to_string(),
                path: "/nonexistent/path".into(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: Some(my_pid),
                created_at: 1, // very old
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            // sweep_dead will mark it dead (path doesn't exist),
            // but gc with max_age should still check liveness for expiry.
            // Since the path doesn't exist, sweep_dead marks it dead first,
            // then dead_removed cleans it. Let's use a real existing path instead.
            let dir = tmp.path().join("real-wt");
            std::fs::create_dir(&dir).unwrap();
            let mut record2 = record.clone();
            record2.id = "alive-wt2".to_string();
            record2.path = dir.clone();
            db.register(&record2).unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: false,
                    dry_run: false,
                    ..Default::default()
                },
            )
            .unwrap();

            // Our PID is alive, so the real-path worktree should be skipped
            assert_eq!(report.skipped_alive, 1);
            // The nonexistent-path one gets swept to dead then removed
            assert_eq!(report.dead_removed, 1);
        }

        #[test]
        fn gc_dry_run_preserves_records() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);

            let record = crate::db::WorktreeRecord {
                id: "dry-1".to_string(),
                path: "/nonexistent".into(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 100,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    dry_run: true,
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(report.dead_removed, 1); // counted as would-be-removed
            // Dry run must NOT mutate: the record is still present AND still
            // Alive (it was never swept to Dead).
            let all = db
                .list(&ListFilter {
                    include_dead: true,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].status, crate::db::WorktreeStatus::Alive);
        }

        #[test]
        fn gc_force_overrides_liveness() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);

            let dir = tmp.path().join("force-wt");
            std::fs::create_dir(&dir).unwrap();

            let record = crate::db::WorktreeRecord {
                id: "force-1".to_string(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: Some(std::process::id()), // our own PID
                created_at: 1,                         // very old
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: true,
                    dry_run: false,
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(report.expired_removed, 1);
            assert_eq!(report.skipped_alive, 0);
        }

        #[test]
        fn gc_clamps_extreme_max_age_without_overflow() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let dir = tmp.path().join("fresh-wt");
            std::fs::create_dir(&dir).unwrap();
            let record = crate::db::WorktreeRecord {
                id: "fresh-1".to_string(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: i64::MAX,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();
            // `now - i64::MIN` would overflow/wrap the cutoff into the future and
            // reclaim everything; the clamp treats any negative age as 0 so the
            // cutoff is `now` and nothing fresh is reclaimed (and no panic).
            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(i64::MIN),
                    force: false,
                    dry_run: false,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(report.expired_removed, 0);
            assert!(dir.exists());
        }

        #[test]
        fn gc_honors_last_accessed_time() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let fresh = tmp.path().join("fresh-access");
            let stale = tmp.path().join("stale-access");
            std::fs::create_dir(&fresh).unwrap();
            std::fs::create_dir(&stale).unwrap();
            let base = crate::db::WorktreeRecord {
                id: String::new(),
                path: std::path::PathBuf::new(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None, // no liveness guard: isolate the age logic
                created_at: 1,     // both are old by creation time
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&crate::db::WorktreeRecord {
                id: "fresh".to_string(),
                path: fresh.clone(),
                last_accessed_at: Some(i64::MAX), // touched within the window
                ..base.clone()
            })
            .unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "stale".to_string(),
                path: stale.clone(),
                last_accessed_at: Some(1), // never re-touched
                ..base
            })
            .unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: false,
                    dry_run: false,
                    ..Default::default()
                },
            )
            .unwrap();

            assert!(
                fresh.exists(),
                "a recently accessed worktree must survive despite an old created_at"
            );
            assert!(
                !stale.exists(),
                "a never-touched expired worktree must be reclaimed"
            );
            assert_eq!(report.expired_removed, 1);
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        fn scan_has_cwd_under(prefix: &std::path::Path) -> bool {
            match gc::live_process_cwds() {
                gc::LiveCwdScan::Ok(cwds) => cwds.iter().any(|p| {
                    p.starts_with(prefix)
                        || dunce::canonicalize(p).is_ok_and(|c| c.starts_with(prefix))
                }),
                _ => false,
            }
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        fn wait_until(pred: impl Fn() -> bool) -> bool {
            use std::time::Duration;
            for _ in 0..200 {
                if pred() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        #[test]
        fn gc_cwd_guard_skips_then_reclaims_expired_worktree() {
            let _cwd_lock = crate::api::cwd_test_guard();
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let dir = tmp.path().join("cwd-wt");
            let nested = dir.join("nested");
            std::fs::create_dir_all(&nested).unwrap();
            let record = crate::db::WorktreeRecord {
                id: "cwd-1".to_string(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .current_dir(&nested)
                .spawn()
                .expect("spawn sleep");
            let want = dunce::canonicalize(&nested).unwrap();
            assert!(
                wait_until(|| scan_has_cwd_under(&want)),
                "live_process_cwds must observe the parked child before GC"
            );

            let opts = gc::GcOptions {
                max_age_secs: Some(0),
                force: false,
                dry_run: false,
                ..Default::default()
            };
            let guarded = gc::gc_worktrees(&db, &opts).unwrap();
            assert_eq!(
                guarded.skipped_alive, 1,
                "a live in-tree CWD must protect the expired worktree"
            );
            assert_eq!(guarded.expired_removed, 0);
            assert!(dir.exists());

            // Once the process exits, the same expired worktree is reclaimed.
            child.kill().ok();
            child.wait().ok();
            assert!(
                wait_until(|| !scan_has_cwd_under(&want)),
                "child CWD must leave the scan after exit before reclaim"
            );
            let reclaimed = gc::gc_worktrees(&db, &opts).unwrap();
            assert_eq!(
                reclaimed.expired_removed, 1,
                "no live process inside ⇒ the expired worktree is reclaimed"
            );
            assert!(!dir.exists());
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        #[test]
        fn gc_age_path_fail_closed_when_scan_unusable() {
            // Pure gate: Failed scan blocks age; force still allows.
            assert!(!gc::age_path_cwd_usable(&gc::LiveCwdScan::Failed, false));
            assert!(gc::age_path_cwd_usable(&gc::LiveCwdScan::Failed, true));
        }

        #[test]
        fn gc_dry_run_with_max_age_does_not_remove_expired() {
            // An expired worktree whose dir exists must be previewed (counted)
            // but never removed under dry_run.
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);

            let dir = tmp.path().join("expired-wt");
            std::fs::create_dir(&dir).unwrap();

            let record = crate::db::WorktreeRecord {
                id: "expired-1".to_string(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None, // no liveness guard
                created_at: 1,     // very old
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: true,
                    dry_run: true,
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(
                report.expired_removed, 1,
                "dry run should count the candidate"
            );
            // No mutation: the dir and the (still Alive) record both survive.
            assert!(dir.exists(), "dry run must not remove the worktree dir");
            let all = db.list(&ListFilter::default()).unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].status, crate::db::WorktreeStatus::Alive);
        }

        #[test]
        fn gc_dry_run_missing_and_expired_counted_once() {
            // A record that is Alive, has a MISSING path, AND is expired must be
            // counted EXACTLY once (a real run sweeps it to dead and unregisters
            // it before the expired loop). It belongs to dead_removed, not both.
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);

            let record = crate::db::WorktreeRecord {
                id: "missing-expired".to_string(),
                path: "/nonexistent/expired-wt".into(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1, // very old → expired
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: true,
                    dry_run: true,
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(
                report.dead_removed, 1,
                "missing path counts as would-be-dead"
            );
            assert_eq!(
                report.expired_removed, 0,
                "must not also be counted in expired_removed"
            );
        }

        #[test]
        fn gc_expired_failed_removal_keeps_record() {
            // When the expired worktree can't be removed, expired_removed must
            // NOT be counted and the DB record must survive (so it stays
            // visible to a later gc).
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);

            // A regular file makes remove_worktree's `remove_dir_all` fail
            // (ENOTDIR) deterministically, even as root.
            let path = tmp.path().join("doomed-wt");
            std::fs::write(&path, b"not a dir").unwrap();

            let record = crate::db::WorktreeRecord {
                id: "doomed-1".to_string(),
                path: path.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: true,
                    dry_run: false,
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(
                report.expired_removed, 0,
                "a failed removal must not be counted"
            );
            assert_eq!(
                report.remove_failed, 1,
                "a failed removal must be surfaced in remove_failed"
            );
            let all = db.list(&ListFilter::default()).unwrap();
            assert_eq!(all.len(), 1, "record must survive a failed removal");
            assert!(path.exists(), "the un-removable path is still present");
        }

        /// True if a record with `path` exists in the DB (assert on our own
        /// record rather than total count: other tests may write to the same
        /// open_default DB concurrently). Matches `register_worktree`'s
        /// canonical path storage (/var vs /private/var on macOS).
        fn record_present(db: &WorktreeDb, path: &std::path::Path) -> bool {
            let canon = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            db.list(&ListFilter::default())
                .unwrap()
                .iter()
                .any(|r| r.path == path || r.path == canon)
        }

        #[test]
        fn db_record_survives_failed_removal() {
            // remove_worktree must keep the DB record when the on-disk removal
            // fails, so the worktree isn't lost from tracking while leaking on
            // disk (unregister only after a successful removal).
            let fx = crate::db::GrokHomeFixture::new();

            // A regular file makes remove_dir_all fail (ENOTDIR) deterministically.
            let wt_path = fx.home.join("doomed-wt");
            std::fs::write(&wt_path, b"not a dir").unwrap();

            // Register via the production registration path (uses open_default).
            super::super::register_worktree(
                &wt_path,
                std::path::Path::new("/src/repo"),
                WorktreeKind::Session,
                "linked",
                "main",
                "abc123",
                None,
                None,
                None,
            );
            let db = WorktreeDb::open(&fx.home).unwrap();
            assert!(
                record_present(&db, &wt_path),
                "precondition: record registered"
            );

            assert!(
                crate::remove_worktree(&wt_path).is_err(),
                "removing a non-directory path must fail"
            );

            assert!(
                record_present(&db, &wt_path),
                "record must survive a failed removal"
            );
        }

        #[test]
        fn db_record_removed_after_successful_removal() {
            // The success direction: a removable worktree must still be
            // unregistered from the DB (catches a regression dropping the
            // unregister).
            xai_test_utils::require_git!();
            use xai_test_utils::git::{git_commit_all, init_git_repo};

            let fx = crate::db::GrokHomeFixture::new();

            // A real repo + a real worktree so remove_worktree succeeds on disk.
            let repo = fx.home.join("repo");
            std::fs::create_dir(&repo).unwrap();
            init_git_repo(&repo);
            std::fs::write(repo.join("f.txt"), "x").unwrap();
            git_commit_all(&repo, "init");
            let wt_path = fx.home.join("live-wt");
            crate::WorktreeBuilder::new(&repo, &wt_path)
                .create()
                .unwrap();

            super::super::register_worktree(
                &wt_path,
                &repo,
                WorktreeKind::Session,
                "linked",
                "main",
                "abc123",
                None,
                None,
                None,
            );
            let db = WorktreeDb::open(&fx.home).unwrap();
            assert!(
                record_present(&db, &wt_path),
                "precondition: record registered"
            );

            crate::remove_worktree(&wt_path).unwrap();

            assert!(!wt_path.exists(), "worktree dir should be gone");
            assert!(
                !record_present(&db, &wt_path),
                "a successful removal must unregister the DB record"
            );
        }

        #[test]
        fn gc_with_delegate_removes_expired_and_unregisters() {
            // gc_worktrees_with_delegate threads the delegate through the expired
            // path and, on a successful removal, counts it and drops the record.
            // (The delegate's btrfs fallback only fires on a real btrfs-delete
            // failure, which needs a btrfs host; here the plain-dir fast path
            // succeeds, so the mock's delete_snapshot is not called.)
            use std::sync::atomic::{AtomicUsize, Ordering};

            // GROK_HOME == the gc DB dir so remove_worktree's open_default
            // unregister hits the same DB the gc record lives in.
            let fx = crate::db::GrokHomeFixture::new();
            let db = WorktreeDb::open(&fx.home).unwrap();

            let dir = fx.home.join("expired-wt");
            std::fs::create_dir(&dir).unwrap();
            let record = crate::db::WorktreeRecord {
                id: "expired-del-1".to_string(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1, // very old → expired
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            let deletes = Arc::new(AtomicUsize::new(0));
            let delegate: Arc<dyn BtrfsDelegate> = Arc::new(super::super::RecordingDelegate {
                snapshot_path: fx.home.join("unused-snap"),
                worktree_path: dir.clone(),
                deletes: Arc::clone(&deletes),
            });

            let report = gc::gc_worktrees_with_delegate(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: true,
                    dry_run: false,
                    ..Default::default()
                },
                Some(delegate),
            )
            .unwrap();

            assert_eq!(
                report.expired_removed, 1,
                "expired worktree should be reclaimed"
            );
            assert!(!dir.exists(), "the worktree dir should be removed");
            assert!(
                db.get("expired-del-1").unwrap().is_none(),
                "the DB record should be unregistered after a successful removal"
            );
            // Plain-dir fast path succeeds without needing the delegate fallback.
            assert_eq!(deletes.load(Ordering::Relaxed), 0);
        }

        #[test]
        fn gc_report_serde_round_trip() {
            let report = gc::GcReport {
                dead_removed: 3,
                expired_removed: 1,
                skipped_alive: 2,
                remove_failed: 4,
            };
            let json = serde_json::to_string(&report).unwrap();
            let deser: gc::GcReport = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.dead_removed, 3);
            assert_eq!(deser.expired_removed, 1);
            assert_eq!(deser.skipped_alive, 2);
            assert_eq!(deser.remove_failed, 4);
        }

        #[test]
        fn gc_options_serde_round_trip() {
            let opts = gc::GcOptions {
                max_age_secs: Some(86400),
                force: true,
                dry_run: false,
                protect_paths: vec![std::path::PathBuf::from("/tmp/p")],
                skip_kinds: vec![WorktreeKind::Manual],
                max_age_by_kind: [(WorktreeKind::Subagent, Some(3600))].into_iter().collect(),
            };
            let json = serde_json::to_string(&opts).unwrap();
            let deser: gc::GcOptions = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.max_age_secs, Some(86400));
            assert!(deser.force);
            assert!(!deser.dry_run);
            assert_eq!(deser.protect_paths, opts.protect_paths);
            assert_eq!(deser.skip_kinds, vec![WorktreeKind::Manual]);
            assert_eq!(
                deser.max_age_by_kind.get(&WorktreeKind::Subagent),
                Some(&Some(3600))
            );
            // Absent new fields deserialize as empty (old agents).
            let legacy = r#"{"max_age_secs":1,"force":false,"dry_run":true}"#;
            let legacy_opts: gc::GcOptions = serde_json::from_str(legacy).unwrap();
            assert!(legacy_opts.protect_paths.is_empty());
            assert!(legacy_opts.skip_kinds.is_empty());
            assert!(legacy_opts.max_age_by_kind.is_empty());

            // JSON null value in map → never-expire (None).
            let with_null = r#"{
                "max_age_secs": 100,
                "force": false,
                "dry_run": false,
                "max_age_by_kind": {"manual": null, "subagent": 3600}
            }"#;
            let null_opts: gc::GcOptions = serde_json::from_str(with_null).unwrap();
            assert_eq!(
                null_opts.max_age_by_kind.get(&WorktreeKind::Manual),
                Some(&None)
            );
            assert_eq!(
                null_opts.max_age_by_kind.get(&WorktreeKind::Subagent),
                Some(&Some(3600))
            );
            let round = serde_json::to_string(&null_opts).unwrap();
            let back: gc::GcOptions = serde_json::from_str(&round).unwrap();
            assert_eq!(back.max_age_by_kind, null_opts.max_age_by_kind);
        }

        #[test]
        fn gc_protect_paths_skips_age_expiry_including_dry_run() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let dir = tmp.path().join("protected-wt");
            let nested = dir.join("nested");
            std::fs::create_dir_all(&nested).unwrap();
            let record = crate::db::WorktreeRecord {
                id: "prot-1".to_string(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&record).unwrap();

            // Protect via a nested path (same canonicalize rules as cwd_within).
            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: false,
                    dry_run: true,
                    protect_paths: vec![nested],
                    skip_kinds: vec![],
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                report.expired_removed, 0,
                "dry_run must not count protect_paths hits as would-expire"
            );
            assert_eq!(report.skipped_alive, 1);
            assert!(dir.exists());

            // Without protect, dry_run would-count it.
            let unguarded = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: false,
                    dry_run: true,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(unguarded.expired_removed, 1);
        }

        #[test]
        fn gc_protect_paths_pre_remove_recheck() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let dir = tmp.path().join("prot-real");
            std::fs::create_dir(&dir).unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "prot-real".to_string(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            })
            .unwrap();
            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: false,
                    dry_run: false,
                    protect_paths: vec![dir.clone()],
                    skip_kinds: vec![],
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(report.expired_removed, 0);
            assert_eq!(report.skipped_alive, 1);
            assert!(dir.exists(), "protect_paths must block real remove");
        }

        #[test]
        fn force_does_not_override_skip_kinds() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let dir = tmp.path().join("manual-force");
            std::fs::create_dir(&dir).unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "manual-force".into(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Manual,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            })
            .unwrap();
            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: true,
                    dry_run: false,
                    protect_paths: vec![],
                    skip_kinds: vec![WorktreeKind::Manual],
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(
                dir.exists(),
                "force must not age-expire kinds listed in skip_kinds"
            );
            assert_eq!(report.expired_removed, 0);
        }

        #[test]
        fn gc_skip_kinds_manual_age_only_not_dead() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let manual_dir = tmp.path().join("manual-alive");
            let session_dir = tmp.path().join("session-alive");
            std::fs::create_dir(&manual_dir).unwrap();
            std::fs::create_dir(&session_dir).unwrap();
            let base = crate::db::WorktreeRecord {
                id: String::new(),
                path: std::path::PathBuf::new(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&crate::db::WorktreeRecord {
                id: "manual".into(),
                path: manual_dir.clone(),
                kind: WorktreeKind::Manual,
                ..base.clone()
            })
            .unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "session".into(),
                path: session_dir.clone(),
                kind: WorktreeKind::Session,
                ..base.clone()
            })
            .unwrap();
            // Dead manual (missing path) still reclaimed on dead path.
            db.register(&crate::db::WorktreeRecord {
                id: "manual-dead".into(),
                path: "/nonexistent/manual-dead".into(),
                kind: WorktreeKind::Manual,
                ..base
            })
            .unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: false,
                    dry_run: false,
                    protect_paths: vec![],
                    skip_kinds: vec![WorktreeKind::Manual],
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(
                manual_dir.exists(),
                "Manual must not age-expire when in skip_kinds"
            );
            assert!(
                !session_dir.exists(),
                "Session must still age-expire under skip_kinds=[Manual]"
            );
            assert_eq!(report.expired_removed, 1);
            assert!(
                report.skipped_alive >= 1,
                "expired skip_kinds must surface in skipped_alive"
            );
            assert_eq!(report.dead_removed, 1, "dead Manual still unregisters");

            // dry_run must not count skipped kinds as would-expire.
            let dir2 = tmp.path().join("manual-dry");
            std::fs::create_dir(&dir2).unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "manual-dry".into(),
                path: dir2.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Manual,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            })
            .unwrap();
            let dry = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: false,
                    dry_run: true,
                    protect_paths: vec![],
                    skip_kinds: vec![WorktreeKind::Manual],
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                dry.expired_removed, 0,
                "dry_run must not count skip_kinds as expired"
            );
            assert!(
                dry.skipped_alive >= 1,
                "dry_run still counts expired skip_kinds as skipped_alive"
            );
            assert!(dir2.exists());
        }

        #[test]
        fn per_kind_max_age_expires_session_not_manual() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let session_dir = tmp.path().join("session-alive");
            let manual_dir = tmp.path().join("manual-alive");
            std::fs::create_dir(&session_dir).unwrap();
            std::fs::create_dir(&manual_dir).unwrap();
            let base = crate::db::WorktreeRecord {
                id: String::new(),
                path: std::path::PathBuf::new(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&crate::db::WorktreeRecord {
                id: "session".into(),
                path: session_dir.clone(),
                kind: WorktreeKind::Session,
                ..base.clone()
            })
            .unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "manual".into(),
                path: manual_dir.clone(),
                kind: WorktreeKind::Manual,
                ..base
            })
            .unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: false,
                    dry_run: false,
                    max_age_by_kind: [(WorktreeKind::Manual, None)].into_iter().collect(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(
                !session_dir.exists(),
                "session must age-expire under default max_age"
            );
            assert!(
                manual_dir.exists(),
                "manual never-expire via max_age_by_kind"
            );
            assert_eq!(report.expired_removed, 1);
            assert!(report.skipped_alive >= 1);
        }

        #[test]
        fn per_kind_shorter_ttl_expires_subagent_keeps_session() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let now = crate::db::now_epoch_secs();
            // Subagent last active 2h ago; session last active 2h ago.
            // subagent TTL=1h → expire; session default=7d → keep.
            let sub_dir = tmp.path().join("sub");
            let sess_dir = tmp.path().join("sess");
            std::fs::create_dir(&sub_dir).unwrap();
            std::fs::create_dir(&sess_dir).unwrap();
            let age = 2 * 3600;
            let base = crate::db::WorktreeRecord {
                id: String::new(),
                path: std::path::PathBuf::new(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: now - age,
                last_accessed_at: Some(now - age),
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&crate::db::WorktreeRecord {
                id: "sub".into(),
                path: sub_dir.clone(),
                kind: WorktreeKind::Subagent,
                ..base.clone()
            })
            .unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "sess".into(),
                path: sess_dir.clone(),
                kind: WorktreeKind::Session,
                ..base
            })
            .unwrap();

            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(7 * 86400),
                    force: false,
                    dry_run: false,
                    max_age_by_kind: [(WorktreeKind::Subagent, Some(3600))].into_iter().collect(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(!sub_dir.exists(), "subagent past 1h TTL must expire");
            assert!(sess_dir.exists(), "session within 7d must stay");
            assert_eq!(report.expired_removed, 1);
        }

        #[test]
        fn force_does_not_override_max_age_by_kind_never() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let dir = tmp.path().join("manual-never");
            std::fs::create_dir(&dir).unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "manual-never".into(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Manual,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            })
            .unwrap();
            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: true,
                    dry_run: false,
                    max_age_by_kind: [(WorktreeKind::Manual, None)].into_iter().collect(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(
                dir.exists(),
                "force must not age-expire never-kinds in max_age_by_kind"
            );
            assert_eq!(report.expired_removed, 0);
        }

        #[test]
        fn dry_run_counts_per_kind_cutoffs() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let now = crate::db::now_epoch_secs();
            let age = 2 * 3600;
            let sub_dir = tmp.path().join("sub-dry");
            let sess_dir = tmp.path().join("sess-dry");
            let man_dir = tmp.path().join("man-dry");
            std::fs::create_dir(&sub_dir).unwrap();
            std::fs::create_dir(&sess_dir).unwrap();
            std::fs::create_dir(&man_dir).unwrap();
            let base = crate::db::WorktreeRecord {
                id: String::new(),
                path: std::path::PathBuf::new(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: now - age,
                last_accessed_at: Some(now - age),
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&crate::db::WorktreeRecord {
                id: "sub-dry".into(),
                path: sub_dir.clone(),
                kind: WorktreeKind::Subagent,
                ..base.clone()
            })
            .unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "sess-dry".into(),
                path: sess_dir.clone(),
                kind: WorktreeKind::Session,
                ..base.clone()
            })
            .unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "man-dry".into(),
                path: man_dir.clone(),
                kind: WorktreeKind::Manual,
                ..base
            })
            .unwrap();

            let dry = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(7 * 86400),
                    force: false,
                    dry_run: true,
                    max_age_by_kind: [
                        (WorktreeKind::Subagent, Some(3600)),
                        (WorktreeKind::Manual, None),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                dry.expired_removed, 1,
                "only subagent past its kind TTL is would-expire"
            );
            assert!(sub_dir.exists() && sess_dir.exists() && man_dir.exists());

            // max_age_secs=0: session+subagent would-expire; manual never → skipped.
            let dry0 = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(0),
                    force: false,
                    dry_run: true,
                    max_age_by_kind: [
                        (WorktreeKind::Subagent, Some(3600)),
                        (WorktreeKind::Manual, None),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(dry0.expired_removed, 2);
            assert!(dry0.skipped_alive >= 1);
        }

        #[test]
        fn max_age_by_kind_only_without_default_enables_age_path() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let pool_dir = tmp.path().join("pool-only");
            let sess_dir = tmp.path().join("sess-unlisted");
            std::fs::create_dir(&pool_dir).unwrap();
            std::fs::create_dir(&sess_dir).unwrap();
            let base = crate::db::WorktreeRecord {
                id: String::new(),
                path: std::path::PathBuf::new(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Session,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            };
            db.register(&crate::db::WorktreeRecord {
                id: "pool-only".into(),
                path: pool_dir.clone(),
                kind: WorktreeKind::Pool,
                ..base.clone()
            })
            .unwrap();
            // Session has no map entry and max_age_secs=None → must not expire.
            db.register(&crate::db::WorktreeRecord {
                id: "sess-unlisted".into(),
                path: sess_dir.clone(),
                kind: WorktreeKind::Session,
                ..base
            })
            .unwrap();
            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: None,
                    force: false,
                    dry_run: false,
                    max_age_by_kind: [(WorktreeKind::Pool, Some(0))].into_iter().collect(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(!pool_dir.exists(), "listed kind expires");
            assert!(
                sess_dir.exists(),
                "unlisted kind must not expire when max_age_secs=None"
            );
            assert_eq!(report.expired_removed, 1);
        }

        #[test]
        fn skip_kinds_beats_max_age_by_kind_on_same_kind() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let dir = tmp.path().join("manual-conflict");
            std::fs::create_dir(&dir).unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "manual-conflict".into(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Manual,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            })
            .unwrap();
            let opts = gc::GcOptions {
                max_age_secs: Some(0),
                force: true,
                dry_run: false,
                skip_kinds: vec![WorktreeKind::Manual],
                max_age_by_kind: [(WorktreeKind::Manual, Some(0))].into_iter().collect(),
                ..Default::default()
            };
            let report = gc::gc_worktrees(&db, &opts).unwrap();
            assert!(
                dir.exists(),
                "skip_kinds must win over max_age_by_kind Some(secs)"
            );
            assert_eq!(report.expired_removed, 0);

            let dry = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    dry_run: true,
                    force: false,
                    ..opts
                },
            )
            .unwrap();
            assert_eq!(
                dry.expired_removed, 0,
                "dry_run must not count skip-winning kinds as would-expire"
            );
            assert!(dry.skipped_alive >= 1);
        }

        #[test]
        fn configurable_manual_can_expire() {
            let tmp = tempfile::TempDir::new().unwrap();
            let db = db_at(&tmp);
            let dir = tmp.path().join("manual-expire");
            std::fs::create_dir(&dir).unwrap();
            db.register(&crate::db::WorktreeRecord {
                id: "manual-exp".into(),
                path: dir.clone(),
                source_repo: "/repo".into(),
                repo_name: "repo".to_string(),
                kind: WorktreeKind::Manual,
                creation_mode: "linked".to_string(),
                git_ref: None,
                head_commit: None,
                session_id: None,
                creator_pid: None,
                created_at: 1,
                last_accessed_at: None,
                status: crate::db::WorktreeStatus::Alive,
                metadata: None,
            })
            .unwrap();
            let report = gc::gc_worktrees(
                &db,
                &gc::GcOptions {
                    max_age_secs: Some(7 * 86400),
                    force: false,
                    dry_run: false,
                    max_age_by_kind: [(WorktreeKind::Manual, Some(0))].into_iter().collect(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(
                !dir.exists(),
                "manual with explicit max_age_by_kind must be configurable to expire"
            );
            assert_eq!(report.expired_removed, 1);
        }

        #[test]
        fn db_stats_serde_round_trip() {
            let stats = crate::db::DbStats {
                total_records: 10,
                alive_count: 7,
                dead_count: 3,
                db_file_bytes: 4096,
            };
            let json = serde_json::to_string(&stats).unwrap();
            let deser: crate::db::DbStats = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.total_records, 10);
            assert_eq!(deser.alive_count, 7);
            assert_eq!(deser.dead_count, 3);
            assert_eq!(deser.db_file_bytes, 4096);
        }
    }
}
