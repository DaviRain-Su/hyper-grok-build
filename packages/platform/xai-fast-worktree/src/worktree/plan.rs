//! Worktree execution planning.
//!
//! `WorktreePlan` makes the worktree creation pipeline explicit and testable.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::{BtrfsDelegate, CreationMode, IgnoredFilesMode, WorkingTreeMode};

#[derive(Clone)]
pub(crate) struct WorktreePlan {
    // Note: manual Debug impl below (Arc<dyn BtrfsDelegate> isn't Debug)
    pub source: PathBuf,
    pub dest: PathBuf,
    pub git_ref: String,
    pub parallelism: usize,
    pub channel_buffer: usize,
    pub working_tree: WorkingTreeMode,
    pub ignored_files: IgnoredFilesMode,
    pub ignored_parallelism: usize,
    /// Strategy for worktree creation (linked, standalone, or git checkout).
    pub creation_mode: CreationMode,
    /// Cancellation token for aborting file copy mid-flight.
    pub cancellation_token: CancellationToken,
    /// Optional delegate for privileged btrfs operations (used when the caller
    /// lacks CAP_SYS_ADMIN, e.g., inside a bwrap sandbox).
    /// Only read on Linux (in `try_btrfs_delegate`).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub btrfs_delegate: Option<Arc<dyn BtrfsDelegate>>,
}

impl std::fmt::Debug for WorktreePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorktreePlan")
            .field("source", &self.source)
            .field("dest", &self.dest)
            .field("git_ref", &self.git_ref)
            .field("parallelism", &self.parallelism)
            .field("working_tree", &self.working_tree)
            .field("creation_mode", &self.creation_mode)
            .field("has_btrfs_delegate", &self.btrfs_delegate.is_some())
            .finish()
    }
}

impl WorktreePlan {
    pub(crate) fn effective_parallelism(&self) -> usize {
        if self.parallelism == 0 {
            num_cpus::get()
        } else {
            self.parallelism
        }
    }

    pub(crate) fn effective_ignored_parallelism(&self) -> usize {
        if self.ignored_parallelism == 0 {
            num_cpus::get()
        } else {
            self.ignored_parallelism
        }
    }
}
