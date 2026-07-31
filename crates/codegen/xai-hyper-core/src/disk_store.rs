//! Shared on-disk layout for hypercore sessions (host implementations).
//!
//! ```text
//! {root}/{session_id}/
//!   snapshot.json
//!   terminals/{turn_id}.json
//! ```

use std::path::{Path, PathBuf};

use xai_hyper_host::{HostError, TerminalTurnRecord};

/// Filesystem layout under a storage root (e.g. `~/.grok/hypercore`).
#[derive(Debug, Clone)]
pub struct HypercoreSessionStore {
    root: PathBuf,
}

impl HypercoreSessionStore {
    /// Create a store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Storage root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory for one session.
    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(sanitize_component(session_id))
    }

    fn snapshot_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("snapshot.json")
    }

    fn terminal_path(&self, session_id: &str, turn_id: &str) -> PathBuf {
        self.session_dir(session_id)
            .join("terminals")
            .join(format!("{}.json", sanitize_component(turn_id)))
    }

    /// Atomically write snapshot and optional terminal turn record.
    pub async fn commit_snapshot(
        &self,
        session_id: &str,
        snapshot: &[u8],
        terminal: Option<&TerminalTurnRecord>,
    ) -> Result<(), HostError> {
        let dir = self.session_dir(session_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| HostError::Io(format!("mkdir {}: {e}", dir.display())))?;
        let snap_path = self.snapshot_path(session_id);
        let tmp = snap_path.with_extension("json.tmp");
        tokio::fs::write(&tmp, snapshot)
            .await
            .map_err(|e| HostError::Io(format!("write {}: {e}", tmp.display())))?;
        tokio::fs::rename(&tmp, &snap_path)
            .await
            .map_err(|e| HostError::Io(format!("rename snapshot: {e}")))?;

        if let Some(term) = terminal {
            let tdir = dir.join("terminals");
            tokio::fs::create_dir_all(&tdir)
                .await
                .map_err(|e| HostError::Io(format!("mkdir terminals: {e}")))?;
            let tpath = self.terminal_path(session_id, &term.turn_id);
            let bytes = serde_json::to_vec_pretty(term)
                .map_err(|e| HostError::Io(format!("serialize terminal: {e}")))?;
            let ttmp = tpath.with_extension("json.tmp");
            tokio::fs::write(&ttmp, &bytes)
                .await
                .map_err(|e| HostError::Io(format!("write terminal: {e}")))?;
            tokio::fs::rename(&ttmp, &tpath)
                .await
                .map_err(|e| HostError::Io(format!("rename terminal: {e}")))?;
        }
        Ok(())
    }

    /// Load snapshot bytes if present.
    pub async fn load_snapshot(&self, session_id: &str) -> Result<Option<Vec<u8>>, HostError> {
        let path = self.snapshot_path(session_id);
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(HostError::Io(format!("read {}: {e}", path.display()))),
        }
    }

    /// Load a terminal turn record if present.
    pub async fn load_terminal_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<TerminalTurnRecord>, HostError> {
        let path = self.terminal_path(session_id, turn_id);
        match tokio::fs::read(&path).await {
            Ok(b) => {
                let rec = serde_json::from_slice(&b)
                    .map_err(|e| HostError::Io(format!("parse terminal: {e}")))?;
                Ok(Some(rec))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(HostError::Io(format!("read {}: {e}", path.display()))),
        }
    }
}

/// Sanitize path components (session / turn ids).
pub fn sanitize_component(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize_component("../evil/id"), "___evil_id");
    }
}
