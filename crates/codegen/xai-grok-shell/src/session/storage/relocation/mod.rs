//! Durable filesystem and journal building blocks for session relocation.
//!
//! Transaction phase orchestration intentionally lives in the following stack layer.

mod fs;
mod journal;

use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
use self::journal::RelocationPhase;
use self::journal::WriteFailure;
pub(crate) use self::journal::{RelocationJournal, RelocationLease};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RelocationError {
    #[error("invalid relocation {field}: {value:?}")]
    InvalidComponent { field: &'static str, value: String },
    #[error("session {0} already has an active relocation lease")]
    LeaseBusy(String),
    #[error("relocation journal is missing for session {0}")]
    JournalMissing(String),
    #[error("relocation collision at {0}")]
    Collision(PathBuf),
    #[error("relocation state is inconsistent: {0}")]
    Inconsistent(String),
    #[error("atomic no-replace publication is unsupported on this platform or filesystem")]
    UnsupportedPublication,
    #[error("{operation} {path}: {source}", path = path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("decode {path}: {source}", path = path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

type Result<T> = std::result::Result<T, RelocationError>;

#[derive(Debug, Clone)]
pub(crate) struct RelocationStorage {
    grok_home: PathBuf,
}

impl RelocationStorage {
    pub(crate) fn new(grok_home: PathBuf) -> Self {
        Self { grok_home }
    }

    pub(crate) fn acquire(&self, session_id: &str) -> Result<RelocationLease> {
        journal::acquire(&self.grok_home, session_id)
    }

    pub(crate) fn read_journal(&self, session_id: &str) -> Result<RelocationJournal> {
        journal::read(&self.grok_home, session_id)
    }

    fn write_journal(&self, journal: &RelocationJournal) -> std::result::Result<(), WriteFailure> {
        self::journal::write(&self.grok_home, journal, None)
    }

    pub(crate) fn sync_journal_namespace(&self) -> Result<()> {
        journal::sync_namespace(&self.grok_home)
    }

    pub(crate) fn copy_directory(&self, source: &Path, target: &Path) -> Result<()> {
        fs::copy_directory(source, target)
    }

    pub(crate) fn create_directory(&self, path: &Path) -> Result<()> {
        fs::create_dir_durable(path)
    }

    pub(crate) fn remove_directory(&self, path: &Path) -> Result<()> {
        fs::remove_dir_durable(path)
    }

    pub(crate) fn publish_no_replace(&self, source: &Path, target: &Path) -> Result<()> {
        fs::rename_no_replace(source, target)
    }
}

#[cfg(all(test, unix))]
mod tests;
