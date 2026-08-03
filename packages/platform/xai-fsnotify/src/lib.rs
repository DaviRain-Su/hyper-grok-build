//! Local-filesystem event source. Single causal stream of wire-ready
//! [`FsEvent`]s on one broadcast channel. The `xai-grok-workspace` layer
//! translates these into `WorkspaceEvent`s with git-enrichment I/O.
//!
//! Single workspace root only; multi-root composition (parent + worktrees)
//! lives in the workspace layer.

mod error;
mod event;
mod paths;
mod source;
mod state;
mod watcher;

pub use error::FsNotifyError;
pub use event::{FsEvent, FsEventKind, GitMetaKind};
pub use source::{
    FsConfig, FsEventSource, FsWatcherStats, STATS_TARGET, set_runtime_handle, shared, stats,
};
pub use state::SETTLE_MS;
