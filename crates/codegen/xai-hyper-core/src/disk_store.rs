//! Shared on-disk layout for hypercore sessions (host implementations).
//!
//! ```text
//! {root}/{session_id}/
//!   state.v2.json          # authoritative atomic snapshot + terminals
//!   state.v2.lock          # cross-process exclusive lock for RMW
//!   snapshot.json          # legacy (never auto-deleted/modified)
//!   terminals/{sanitize}.json  # legacy (never auto-deleted/modified)
//! ```
//!
//! New writers always publish a single `state.v2.json` via unique temp + durable
//! replace under an exclusive lock.
//!
//! **Legacy fallback rules**
//! - **Snapshot**: read legacy `snapshot.json` only when `state.v2.json` is
//!   absent (`NotFound`). If v2 exists but is corrupt / wrong version → fail
//!   closed (no snapshot fallback).
//! - **Terminal**: when v2 is absent, or when a valid v2 is present but the raw
//!   turn_id is missing from the map, fall back to the sanitized legacy path.
//!   The parsed record is returned only if `record.turn_id` equals the
//!   requested raw id (sanitize collisions yield `None`; files are never
//!   modified). Corrupt / wrong-version v2 never falls back.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use xai_hyper_host::{HostError, TerminalTurnRecord};

/// Authoritative session state format version.
pub const STATE_FORMAT_VERSION: u32 = 2;

const STATE_FILE_NAME: &str = "state.v2.json";
const STATE_LOCK_NAME: &str = "state.v2.lock";

/// Filesystem layout under a storage root (e.g. `~/.grok/hypercore`).
#[derive(Debug, Clone)]
pub struct HypercoreSessionStore {
    root: PathBuf,
    /// Test-only publish failpoint (shared across clones of this store).
    ///
    /// - `0`: disabled
    /// - `1`: fail after temp write/sync, before atomic replace (old state remains)
    /// - `2`: fail after atomic replace, during directory sync (new state visible)
    publish_failpoint: Arc<AtomicU8>,
}

/// Single-file authoritative state (snapshot bytes + terminal map).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionStateV2 {
    format_version: u32,
    /// Opaque transcript snapshot bytes (not re-encoded structurally).
    #[serde(with = "snapshot_bytes")]
    snapshot: Vec<u8>,
    /// Terminal records keyed by raw `turn_id` (must equal `record.turn_id`).
    terminals: BTreeMap<String, TerminalTurnRecord>,
}

mod snapshot_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Snapshots are JSON text; store as a UTF-8 string when possible so the
        // file stays inspectable. Fall back to a JSON array of bytes otherwise.
        match std::str::from_utf8(bytes) {
            Ok(s) => serializer.serialize_str(s),
            Err(_) => serializer.serialize_bytes(bytes),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum SnapshotWire {
            Text(String),
            Bytes(Vec<u8>),
        }
        match SnapshotWire::deserialize(deserializer)? {
            SnapshotWire::Text(s) => Ok(s.into_bytes()),
            SnapshotWire::Bytes(b) => Ok(b),
        }
    }
}

impl HypercoreSessionStore {
    /// Create a store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            publish_failpoint: Arc::new(AtomicU8::new(0)),
        }
    }

    /// Test-only: inject a publish failure (see [`HypercoreSessionStore`] docs).
    #[cfg(test)]
    pub(crate) fn set_publish_failpoint(&self, value: u8) {
        self.publish_failpoint.store(value, Ordering::SeqCst);
    }

    /// Storage root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory for one session.
    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(sanitize_component(session_id))
    }

    fn state_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join(STATE_FILE_NAME)
    }

    fn state_lock_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join(STATE_LOCK_NAME)
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
    ///
    /// Cross-process exclusive lock on `state.v2.lock`, then read-modify-write
    /// of a single `state.v2.json`. Legacy layout files are never modified.
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

        let store = self.clone();
        let session_id = session_id.to_owned();
        let snapshot = snapshot.to_vec();
        let terminal = terminal.cloned();

        tokio::task::spawn_blocking(move || {
            store.commit_snapshot_blocking(&session_id, &snapshot, terminal.as_ref())
        })
        .await
        .map_err(|e| HostError::Io(format!("commit_snapshot join: {e}")))?
    }

    fn commit_snapshot_blocking(
        &self,
        session_id: &str,
        snapshot: &[u8],
        terminal: Option<&TerminalTurnRecord>,
    ) -> Result<(), HostError> {
        let dir = self.session_dir(session_id);
        let lock_path = self.state_lock_path(session_id);
        let state_path = self.state_path(session_id);

        let lock = ExclusiveLock::acquire(&lock_path)
            .map_err(|e| HostError::Io(format!("lock {}: {e}", lock_path.display())))?;

        let mut state = match read_state_file(&state_path) {
            Ok(Some(s)) => s,
            Ok(None) => SessionStateV2 {
                format_version: STATE_FORMAT_VERSION,
                snapshot: Vec::new(),
                terminals: BTreeMap::new(),
            },
            // Corrupt / wrong version: do not overwrite, do not legacy-fallback.
            Err(e) => return Err(e),
        };

        // Validate terminal (incl. legacy exact-id conflict) before mutating
        // snapshot so a conflict cannot advance on-disk snapshot.
        if let Some(term) = terminal {
            match state.terminals.get(&term.turn_id) {
                Some(existing) if existing == term => {
                    // Idempotent: identical full record already in v2 map.
                }
                Some(_) => {
                    return Err(HostError::Io(format!(
                        "terminal turn_id conflict in {}: existing record differs for turn_id {:?}",
                        state_path.display(),
                        term.turn_id
                    )));
                }
                None => {
                    // First insert for this raw id: consult legacy under the same lock.
                    let legacy_path = self.terminal_path(session_id, &term.turn_id);
                    match read_legacy_terminal_exact(&legacy_path, &term.turn_id)? {
                        None => {
                            // Missing, or sanitize collision (other raw id) → free to insert.
                            state.terminals.insert(term.turn_id.clone(), term.clone());
                        }
                        Some(existing) if &existing == term => {
                            // Same full record on legacy → idempotent promote into v2.
                            state.terminals.insert(term.turn_id.clone(), term.clone());
                        }
                        Some(_) => {
                            return Err(HostError::Io(format!(
                                "terminal turn_id conflict with legacy {}: existing record differs for turn_id {:?}",
                                legacy_path.display(),
                                term.turn_id
                            )));
                        }
                    }
                }
            }
        }

        state.format_version = STATE_FORMAT_VERSION;
        state.snapshot = snapshot.to_vec();

        let bytes = serde_json::to_vec_pretty(&state)
            .map_err(|e| HostError::Io(format!("serialize {}: {e}", state_path.display())))?;

        let failpoint = self.publish_failpoint.load(Ordering::SeqCst);
        let result = durable_publish(&dir, &state_path, &bytes, failpoint);
        // Hold lock until publish (and directory sync) complete.
        drop(lock);
        result
    }

    /// Load snapshot bytes if present.
    ///
    /// Uses `state.v2.json` when present; legacy `snapshot.json` only if v2 is
    /// absent. Corrupt / wrong-version v2 fails closed (no fallback).
    pub async fn load_snapshot(&self, session_id: &str) -> Result<Option<Vec<u8>>, HostError> {
        let state_path = self.state_path(session_id);
        let legacy_path = self.snapshot_path(session_id);
        tokio::task::spawn_blocking(move || match read_state_file(&state_path)? {
            Some(s) => Ok(Some(s.snapshot)),
            None => match std::fs::read(&legacy_path) {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(HostError::Io(format!(
                    "read {}: {e}",
                    legacy_path.display()
                ))),
            },
        })
        .await
        .map_err(|e| HostError::Io(format!("load_snapshot join: {e}")))?
    }

    /// Load a terminal turn record if present.
    ///
    /// Prefer raw key in valid `state.v2.json`. On map miss (or when v2 is
    /// absent), fall back to legacy with **exact** `turn_id` match. Corrupt /
    /// wrong-version v2 fails closed (no fallback).
    pub async fn load_terminal_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<TerminalTurnRecord>, HostError> {
        let store = self.clone();
        let session_id = session_id.to_owned();
        let turn_id = turn_id.to_owned();
        tokio::task::spawn_blocking(move || {
            store.load_terminal_turn_blocking(&session_id, &turn_id)
        })
        .await
        .map_err(|e| HostError::Io(format!("load_terminal_turn join: {e}")))?
    }

    fn load_terminal_turn_blocking(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<TerminalTurnRecord>, HostError> {
        match read_state_file(&self.state_path(session_id))? {
            Some(state) => {
                if let Some(rec) = state.terminals.get(turn_id) {
                    return Ok(Some(rec.clone()));
                }
                // Valid v2, raw key miss → per-key legacy fallback (exact id).
                read_legacy_terminal_exact(&self.terminal_path(session_id, turn_id), turn_id)
            }
            // No v2 → legacy only.
            None => read_legacy_terminal_exact(&self.terminal_path(session_id, turn_id), turn_id),
        }
    }

    /// Test helper: write legacy flat layout without going through v2 commit.
    #[cfg(test)]
    pub async fn write_legacy_for_test(
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
        tokio::fs::write(&snap_path, snapshot)
            .await
            .map_err(|e| HostError::Io(format!("write {}: {e}", snap_path.display())))?;
        if let Some(term) = terminal {
            let tdir = dir.join("terminals");
            tokio::fs::create_dir_all(&tdir)
                .await
                .map_err(|e| HostError::Io(format!("mkdir terminals: {e}")))?;
            let tpath = self.terminal_path(session_id, &term.turn_id);
            let bytes = serde_json::to_vec_pretty(term)
                .map_err(|e| HostError::Io(format!("serialize terminal: {e}")))?;
            tokio::fs::write(&tpath, &bytes)
                .await
                .map_err(|e| HostError::Io(format!("write {}: {e}", tpath.display())))?;
        }
        Ok(())
    }

    /// Test helper: path to authoritative state file.
    #[cfg(test)]
    pub fn state_v2_path_for_test(&self, session_id: &str) -> PathBuf {
        self.state_path(session_id)
    }

    /// Test helper: path to legacy snapshot.
    #[cfg(test)]
    pub fn legacy_snapshot_path_for_test(&self, session_id: &str) -> PathBuf {
        self.snapshot_path(session_id)
    }

    /// Test helper: path to legacy terminal file.
    #[cfg(test)]
    pub fn legacy_terminal_path_for_test(&self, session_id: &str, turn_id: &str) -> PathBuf {
        self.terminal_path(session_id, turn_id)
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

fn read_state_file(path: &Path) -> Result<Option<SessionStateV2>, HostError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(HostError::Io(format!("read {}: {e}", path.display())));
        }
    };
    let state: SessionStateV2 = serde_json::from_slice(&bytes).map_err(|e| {
        HostError::Io(format!(
            "parse {}: corrupt or invalid state.v2.json: {e}",
            path.display()
        ))
    })?;
    validate_state(&state, path)?;
    Ok(Some(state))
}

/// Read a legacy terminal file; return `Some` only when `record.turn_id`
/// exactly equals `turn_id`.
///
/// - Missing file → `Ok(None)`
/// - Sanitize collision (file present, different raw `turn_id`) → `Ok(None)`
///   (file is never deleted or modified)
/// - Parse / I/O errors → `Err` (fail closed)
fn read_legacy_terminal_exact(
    path: &Path,
    turn_id: &str,
) -> Result<Option<TerminalTurnRecord>, HostError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(HostError::Io(format!("read {}: {e}", path.display())));
        }
    };
    let rec: TerminalTurnRecord = serde_json::from_slice(&bytes)
        .map_err(|e| HostError::Io(format!("parse terminal {}: {e}", path.display())))?;
    if rec.turn_id == turn_id {
        Ok(Some(rec))
    } else {
        // Sanitize collision: same path component, different raw id.
        Ok(None)
    }
}

fn validate_state(state: &SessionStateV2, path: &Path) -> Result<(), HostError> {
    if state.format_version != STATE_FORMAT_VERSION {
        return Err(HostError::Io(format!(
            "unsupported state format_version {} in {} (expected {STATE_FORMAT_VERSION})",
            state.format_version,
            path.display()
        )));
    }
    for (key, rec) in &state.terminals {
        if key != &rec.turn_id {
            return Err(HostError::Io(format!(
                "terminal map key {:?} != record.turn_id {:?} in {}",
                key,
                rec.turn_id,
                path.display()
            )));
        }
    }
    Ok(())
}

/// Unique temp path in `dir` (pid + monotonic nonce); never a fixed `.tmp`.
fn unique_temp_path(dir: &Path, final_name: &str) -> PathBuf {
    static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);
    let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        "{final_name}.{}.{nonce}.{nanos}.tmp",
        std::process::id()
    ))
}

fn durable_publish(
    dir: &Path,
    final_path: &Path,
    bytes: &[u8],
    failpoint: u8,
) -> Result<(), HostError> {
    let final_name = final_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(STATE_FILE_NAME);
    let tmp_path = unique_temp_path(dir, final_name);

    let write_result = (|| -> Result<(), HostError> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        let mut file = opts
            .open(&tmp_path)
            .map_err(|e| HostError::Io(format!("create temp {}: {e}", tmp_path.display())))?;
        file.write_all(bytes)
            .map_err(|e| HostError::Io(format!("write temp {}: {e}", tmp_path.display())))?;
        file.sync_all()
            .map_err(|e| HostError::Io(format!("sync temp {}: {e}", tmp_path.display())))?;
        drop(file);

        if failpoint == 1 {
            return Err(HostError::Io(format!(
                "injected failpoint: before publish of {}",
                final_path.display()
            )));
        }

        atomic_replace(&tmp_path, final_path).map_err(|e| {
            HostError::Io(format!(
                "atomic replace {} -> {}: {e}",
                tmp_path.display(),
                final_path.display()
            ))
        })?;

        // Temp has been consumed by rename/replace; on success the path is gone.

        if failpoint == 2 {
            return Err(HostError::Io(format!(
                "injected failpoint: after publish, before dir sync of {}",
                dir.display()
            )));
        }

        sync_dir(dir).map_err(|e| HostError::Io(format!("sync dir {}: {e}", dir.display())))?;
        Ok(())
    })();

    if write_result.is_err() {
        // Best-effort cleanup of this attempt's temp only; never touch final/legacy.
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_result
}

#[cfg(unix)]
fn atomic_replace(tmp_path: &Path, final_path: &Path) -> io::Result<()> {
    std::fs::rename(tmp_path, final_path)
}

#[cfg(windows)]
fn atomic_replace(tmp_path: &Path, final_path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    fn wide_extended(path: &Path) -> io::Result<Vec<u16>> {
        let path = std::path::absolute(path)?;
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        // Already extended (`\\?\…` or `\\?\UNC\…`) — do not double-wrap.
        // `\\?\` is U+005C U+005C U+003F U+005C.
        if wide.starts_with(&[0x5c, 0x5c, 0x3f, 0x5c]) {
            wide.push(0);
            return Ok(wide);
        }
        let unc = wide.starts_with(&[0x5c, 0x5c]); // \\
        let mut result = if unc { r"\\?\UNC\" } else { r"\\?\" }
            .encode_utf16()
            .collect::<Vec<_>>();
        if unc {
            wide.drain(..2);
        }
        result.extend(wide);
        result.push(0);
        Ok(result)
    }

    let from = wide_extended(tmp_path)?;
    let to = wide_extended(final_path)?;
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(tmp_path: &Path, final_path: &Path) -> io::Result<()> {
    std::fs::rename(tmp_path, final_path)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let f = std::fs::File::open(dir)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// RAII exclusive advisory lock; released on drop.
struct ExclusiveLock {
    file: std::fs::File,
}

impl ExclusiveLock {
    fn acquire(lock_path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(id: &str, req: &str, asst: &str) -> TerminalTurnRecord {
        TerminalTurnRecord {
            turn_id: id.into(),
            request_text: req.into(),
            assistant_text: asst.into(),
            stop_reason: Some("end_turn".into()),
        }
    }

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize_component("../evil/id"), "___evil_id");
        // Collision that v2 raw keys must avoid:
        assert_eq!(sanitize_component("a/b"), sanitize_component("a?b"));
    }

    #[tokio::test]
    async fn legacy_flat_readable_when_no_v2() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let snap = br#"{"schema_version":1,"session_id":"s1"}"#;
        let t = term("t1", "hi", "hello");
        store
            .write_legacy_for_test("s1", snap, Some(&t))
            .await
            .unwrap();

        assert_eq!(
            store.load_snapshot("s1").await.unwrap().as_deref(),
            Some(snap.as_slice())
        );
        assert_eq!(store.load_terminal_turn("s1", "t1").await.unwrap(), Some(t));
        assert!(!store.state_v2_path_for_test("s1").exists());
    }

    #[tokio::test]
    async fn first_v2_commit_leaves_legacy_bytes_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let snap = br#"{"legacy":true}"#;
        let t = term("t1", "hi", "hello");
        store
            .write_legacy_for_test("s1", snap, Some(&t))
            .await
            .unwrap();

        let legacy_snap = store.legacy_snapshot_path_for_test("s1");
        let legacy_term = store.legacy_terminal_path_for_test("s1", "t1");
        let snap_before = std::fs::read(&legacy_snap).unwrap();
        let term_before = std::fs::read(&legacy_term).unwrap();

        let new_snap = br#"{"v2":true}"#;
        let t2 = term("t2", "next", "ok");
        store
            .commit_snapshot("s1", new_snap, Some(&t2))
            .await
            .unwrap();

        assert_eq!(std::fs::read(&legacy_snap).unwrap(), snap_before);
        assert_eq!(std::fs::read(&legacy_term).unwrap(), term_before);
        assert!(legacy_snap.exists());
        assert!(legacy_term.exists());

        assert_eq!(
            store.load_snapshot("s1").await.unwrap().as_deref(),
            Some(new_snap.as_slice())
        );
        assert_eq!(
            store.load_terminal_turn("s1", "t2").await.unwrap(),
            Some(t2)
        );
        // Legacy terminal still reachable when not in v2 map.
        assert_eq!(store.load_terminal_turn("s1", "t1").await.unwrap(), Some(t));
    }

    #[tokio::test]
    async fn v2_commit_publishes_snapshot_and_terminal_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let snap = b"snap-bytes";
        let t = term("turn-a", "q", "a");
        store.commit_snapshot("sess", snap, Some(&t)).await.unwrap();
        assert_eq!(
            store.load_snapshot("sess").await.unwrap().as_deref(),
            Some(snap.as_slice())
        );
        assert_eq!(
            store.load_terminal_turn("sess", "turn-a").await.unwrap(),
            Some(t)
        );
    }

    #[tokio::test]
    async fn terminal_none_updates_snapshot_keeps_existing_terminals() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let t = term("keep-me", "q", "a");
        store
            .commit_snapshot("sess", b"v1", Some(&t))
            .await
            .unwrap();
        store.commit_snapshot("sess", b"v2", None).await.unwrap();
        assert_eq!(
            store.load_snapshot("sess").await.unwrap().as_deref(),
            Some(b"v2".as_slice())
        );
        assert_eq!(
            store.load_terminal_turn("sess", "keep-me").await.unwrap(),
            Some(t)
        );
    }

    #[tokio::test]
    async fn fail_before_publish_keeps_old_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let t1 = term("t1", "q", "a");
        store
            .commit_snapshot("sess", b"old", Some(&t1))
            .await
            .unwrap();

        let t2 = term("t2", "q2", "a2");
        store.set_publish_failpoint(1);
        let err = store
            .commit_snapshot("sess", b"new", Some(&t2))
            .await
            .unwrap_err();
        store.set_publish_failpoint(0);
        assert!(format!("{err}").contains("before publish"), "{err}");

        assert_eq!(
            store.load_snapshot("sess").await.unwrap().as_deref(),
            Some(b"old".as_slice())
        );
        assert_eq!(
            store.load_terminal_turn("sess", "t1").await.unwrap(),
            Some(t1.clone())
        );
        assert_eq!(store.load_terminal_turn("sess", "t2").await.unwrap(), None);

        // Re-open store (new instance) sees the same.
        let store2 = HypercoreSessionStore::new(dir.path());
        assert_eq!(
            store2.load_snapshot("sess").await.unwrap().as_deref(),
            Some(b"old".as_slice())
        );
        assert_eq!(store2.load_terminal_turn("sess", "t2").await.unwrap(), None);
        assert_eq!(
            store2.load_terminal_turn("sess", "t1").await.unwrap(),
            Some(t1)
        );
    }

    #[tokio::test]
    async fn fail_after_publish_new_state_fully_visible() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let t1 = term("t1", "q", "a");
        store
            .commit_snapshot("sess", b"old", Some(&t1))
            .await
            .unwrap();

        let t2 = term("t2", "q2", "a2");
        store.set_publish_failpoint(2);
        let err = store
            .commit_snapshot("sess", b"new", Some(&t2))
            .await
            .unwrap_err();
        store.set_publish_failpoint(0);
        assert!(format!("{err}").contains("after publish"), "{err}");

        // New snapshot + both terminals fully visible despite Err.
        assert_eq!(
            store.load_snapshot("sess").await.unwrap().as_deref(),
            Some(b"new".as_slice())
        );
        assert_eq!(
            store.load_terminal_turn("sess", "t1").await.unwrap(),
            Some(t1)
        );
        assert_eq!(
            store.load_terminal_turn("sess", "t2").await.unwrap(),
            Some(t2.clone())
        );

        // Ambiguous post-publish error: same-id load still finds terminal.
        assert_eq!(
            store.load_terminal_turn("sess", "t2").await.unwrap(),
            Some(t2)
        );
    }

    #[tokio::test]
    async fn corrupt_state_no_fallback_and_commit_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let session = "sess";
        let state_path = store.state_v2_path_for_test(session);
        tokio::fs::create_dir_all(store.session_dir(session))
            .await
            .unwrap();
        // Also plant legacy that must not be used while v2 exists (even if corrupt).
        store
            .write_legacy_for_test(session, b"legacy-snap", Some(&term("t-leg", "q", "a")))
            .await
            .unwrap();
        tokio::fs::write(&state_path, b"{not json").await.unwrap();

        let load_err = store.load_snapshot(session).await.unwrap_err();
        assert!(
            format!("{load_err}").contains("corrupt") || format!("{load_err}").contains("parse"),
            "{load_err}"
        );
        let term_err = store
            .load_terminal_turn(session, "t-leg")
            .await
            .unwrap_err();
        assert!(
            format!("{term_err}").contains("corrupt") || format!("{term_err}").contains("parse"),
            "{term_err}"
        );

        let before = tokio::fs::read(&state_path).await.unwrap();
        let commit_err = store
            .commit_snapshot(session, b"should-not-write", Some(&term("t-new", "q", "a")))
            .await
            .unwrap_err();
        assert!(
            format!("{commit_err}").contains("corrupt")
                || format!("{commit_err}").contains("parse"),
            "{commit_err}"
        );
        assert_eq!(tokio::fs::read(&state_path).await.unwrap(), before);
    }

    #[tokio::test]
    async fn wrong_version_no_fallback_and_commit_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let session = "sess";
        let state_path = store.state_v2_path_for_test(session);
        tokio::fs::create_dir_all(store.session_dir(session))
            .await
            .unwrap();
        let bad = serde_json::json!({
            "format_version": 99,
            "snapshot": "x",
            "terminals": {}
        });
        tokio::fs::write(&state_path, serde_json::to_vec_pretty(&bad).unwrap())
            .await
            .unwrap();
        store
            .write_legacy_for_test(session, b"legacy", None)
            .await
            .unwrap();

        let err = store.load_snapshot(session).await.unwrap_err();
        assert!(format!("{err}").contains("format_version"), "{err}");
        let before = tokio::fs::read(&state_path).await.unwrap();
        let cerr = store
            .commit_snapshot(session, b"nope", None)
            .await
            .unwrap_err();
        assert!(format!("{cerr}").contains("format_version"), "{cerr}");
        assert_eq!(tokio::fs::read(&state_path).await.unwrap(), before);
    }

    #[tokio::test]
    async fn raw_turn_ids_with_same_sanitize_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        assert_eq!(sanitize_component("a/b"), sanitize_component("a?b"));
        let t1 = term("a/b", "q1", "a1");
        let t2 = term("a?b", "q2", "a2");
        store
            .commit_snapshot("sess", b"s1", Some(&t1))
            .await
            .unwrap();
        store
            .commit_snapshot("sess", b"s2", Some(&t2))
            .await
            .unwrap();
        assert_eq!(
            store.load_terminal_turn("sess", "a/b").await.unwrap(),
            Some(t1)
        );
        assert_eq!(
            store.load_terminal_turn("sess", "a?b").await.unwrap(),
            Some(t2)
        );
    }

    #[tokio::test]
    async fn same_id_different_record_rejected_identical_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let t = term("tid", "req", "asst");
        store
            .commit_snapshot("sess", b"s1", Some(&t))
            .await
            .unwrap();
        // Identical → ok
        store
            .commit_snapshot("sess", b"s2", Some(&t))
            .await
            .unwrap();
        assert_eq!(
            store.load_snapshot("sess").await.unwrap().as_deref(),
            Some(b"s2".as_slice())
        );
        // Different content → conflict
        let other = term("tid", "req", "DIFFERENT");
        let err = store
            .commit_snapshot("sess", b"s3", Some(&other))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("conflict"), "{err}");
        assert_eq!(
            store.load_snapshot("sess").await.unwrap().as_deref(),
            Some(b"s2".as_slice())
        );
        assert_eq!(
            store.load_terminal_turn("sess", "tid").await.unwrap(),
            Some(t)
        );
    }

    #[tokio::test]
    async fn concurrent_commits_retain_both_terminals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store_a = HypercoreSessionStore::new(root.clone());
        let store_b = HypercoreSessionStore::new(root);

        let t_a = term("turn-a", "qa", "aa");
        let t_b = term("turn-b", "qb", "ab");

        let h1 = tokio::spawn({
            let store = store_a.clone();
            let t = t_a.clone();
            async move { store.commit_snapshot("shared", b"snap-a", Some(&t)).await }
        });
        let h2 = tokio::spawn({
            let store = store_b.clone();
            let t = t_b.clone();
            async move { store.commit_snapshot("shared", b"snap-b", Some(&t)).await }
        });
        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();

        let store = HypercoreSessionStore::new(dir.path());
        assert_eq!(
            store.load_terminal_turn("shared", "turn-a").await.unwrap(),
            Some(t_a)
        );
        assert_eq!(
            store.load_terminal_turn("shared", "turn-b").await.unwrap(),
            Some(t_b)
        );
        // Snapshot is last writer; either is fine.
        let snap = store.load_snapshot("shared").await.unwrap().unwrap();
        assert!(snap == b"snap-a" || snap == b"snap-b", "{snap:?}");
    }

    #[tokio::test]
    async fn unique_temps_no_fixed_tmp_collision() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        // Rapid commits must not stomp a shared fixed .tmp name.
        for i in 0..20 {
            let snap = format!("snap-{i}");
            let t = term(&format!("t{i}"), "q", "a");
            store
                .commit_snapshot("sess", snap.as_bytes(), Some(&t))
                .await
                .unwrap();
        }
        // Fixed-name temps must not remain / be used.
        let session_dir = store.session_dir("sess");
        let fixed = session_dir.join("state.v2.json.tmp");
        assert!(!fixed.exists(), "must not use fixed state.v2.json.tmp");
        // No leftover .tmp files after successful commits.
        let leftovers: Vec<_> = std::fs::read_dir(&session_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover temps: {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
        assert_eq!(
            store.load_snapshot("sess").await.unwrap().as_deref(),
            Some(b"snap-19".as_slice())
        );
    }

    #[tokio::test]
    async fn map_key_mismatch_rejected_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let session = "sess";
        let state_path = store.state_v2_path_for_test(session);
        tokio::fs::create_dir_all(store.session_dir(session))
            .await
            .unwrap();
        let bad = serde_json::json!({
            "format_version": 2,
            "snapshot": "x",
            "terminals": {
                "wrong-key": {
                    "turn_id": "real-id",
                    "request_text": "q",
                    "assistant_text": "a",
                    "stop_reason": null
                }
            }
        });
        tokio::fs::write(&state_path, serde_json::to_vec_pretty(&bad).unwrap())
            .await
            .unwrap();
        let err = store
            .load_terminal_turn(session, "real-id")
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("turn_id") || format!("{err}").contains("key"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn legacy_terminal_exact_id_only_no_v2() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        // Plant legacy under sanitize("a/b") == sanitize("a?b") with raw id a/b.
        let owner = term("a/b", "q", "a");
        store
            .write_legacy_for_test("sess", b"legacy-snap", Some(&owner))
            .await
            .unwrap();
        assert_eq!(sanitize_component("a/b"), sanitize_component("a?b"));

        // Exact raw id hits.
        assert_eq!(
            store.load_terminal_turn("sess", "a/b").await.unwrap(),
            Some(owner)
        );
        // Colliding sanitize form must not return the other id's record.
        assert_eq!(store.load_terminal_turn("sess", "a?b").await.unwrap(), None);
    }

    #[tokio::test]
    async fn legacy_terminal_exact_id_on_v2_map_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let owner = term("a/b", "q", "a");
        store
            .write_legacy_for_test("sess", b"legacy-snap", Some(&owner))
            .await
            .unwrap();
        // Create valid v2 without that terminal key (other turn only).
        let other = term("other", "qo", "ao");
        store
            .commit_snapshot("sess", b"v2-snap", Some(&other))
            .await
            .unwrap();

        // Raw miss → legacy exact-id fallback.
        assert_eq!(
            store.load_terminal_turn("sess", "a/b").await.unwrap(),
            Some(owner)
        );
        // Sanitize collision still None.
        assert_eq!(store.load_terminal_turn("sess", "a?b").await.unwrap(), None);
        // v2 key still works.
        assert_eq!(
            store.load_terminal_turn("sess", "other").await.unwrap(),
            Some(other)
        );
    }

    #[tokio::test]
    async fn commit_rejects_legacy_exact_id_conflict_snapshot_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let legacy = term("tid", "req", "legacy-asst");
        store
            .write_legacy_for_test("sess", b"legacy-snap", Some(&legacy))
            .await
            .unwrap();

        let different = term("tid", "req", "new-asst");
        let err = store
            .commit_snapshot("sess", b"should-not-land", Some(&different))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("conflict"), "{err}");
        // No v2 published; snapshot still legacy.
        assert!(!store.state_v2_path_for_test("sess").exists());
        assert_eq!(
            store.load_snapshot("sess").await.unwrap().as_deref(),
            Some(b"legacy-snap".as_slice())
        );
        assert_eq!(
            store.load_terminal_turn("sess", "tid").await.unwrap(),
            Some(legacy.clone())
        );

        // Identical full record → idempotent promote into v2.
        store
            .commit_snapshot("sess", b"promoted", Some(&legacy))
            .await
            .unwrap();
        assert_eq!(
            store.load_snapshot("sess").await.unwrap().as_deref(),
            Some(b"promoted".as_slice())
        );
        assert_eq!(
            store.load_terminal_turn("sess", "tid").await.unwrap(),
            Some(legacy)
        );
    }

    #[tokio::test]
    async fn commit_allows_insert_when_legacy_is_sanitize_collision() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let owner = term("a/b", "q1", "a1");
        store
            .write_legacy_for_test("sess", b"legacy", Some(&owner))
            .await
            .unwrap();
        // Insert colliding sanitize form — legacy file belongs to a/b, not a?b.
        let collider = term("a?b", "q2", "a2");
        store
            .commit_snapshot("sess", b"v2", Some(&collider))
            .await
            .unwrap();
        assert_eq!(
            store.load_terminal_turn("sess", "a?b").await.unwrap(),
            Some(collider)
        );
        assert_eq!(
            store.load_terminal_turn("sess", "a/b").await.unwrap(),
            Some(owner)
        );
    }

    #[tokio::test]
    async fn commit_fails_closed_on_legacy_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());
        let session = "sess";
        let tdir = store.session_dir(session).join("terminals");
        tokio::fs::create_dir_all(&tdir).await.unwrap();
        let path = store.legacy_terminal_path_for_test(session, "tid");
        tokio::fs::write(&path, b"{not-json").await.unwrap();

        let err = store
            .commit_snapshot(session, b"nope", Some(&term("tid", "q", "a")))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("parse") || format!("{err}").contains("terminal"),
            "{err}"
        );
        assert!(!store.state_v2_path_for_test(session).exists());
    }

    #[tokio::test]
    async fn snapshot_wire_utf8_and_non_utf8_byte_exact() {
        let dir = tempfile::tempdir().unwrap();
        let store = HypercoreSessionStore::new(dir.path());

        let utf8 = br#"{"schema_version":2,"session_id":"s","items":[]}"#;
        store.commit_snapshot("utf8", utf8, None).await.unwrap();
        assert_eq!(
            store.load_snapshot("utf8").await.unwrap().as_deref(),
            Some(utf8.as_slice())
        );

        let binary = b"\xff\xfe\x00not-utf8\x80";
        store.commit_snapshot("bin", binary, None).await.unwrap();
        assert_eq!(
            store.load_snapshot("bin").await.unwrap().as_deref(),
            Some(binary.as_slice())
        );
    }
}
