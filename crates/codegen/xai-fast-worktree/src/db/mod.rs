//! SQLite-backed metadata database for tracking worktrees.
//!
//! Gated behind the `metadata` cargo feature. When disabled, all DB operations
//! compile away to no-ops.

mod queries;
mod schema;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use xai_sqlite_journal::JournalMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeKind {
    Session,
    Ab,
    Pool,
    Fork,
    Manual,
    Subagent,
}

impl WorktreeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Ab => "ab",
            Self::Pool => "pool",
            Self::Fork => "fork",
            Self::Manual => "manual",
            Self::Subagent => "subagent",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        Self::from_str_exact(s).unwrap_or(Self::Manual)
    }

    /// Exact known kind key. Unknown → None (unlike [`Self::from_str_lossy`]).
    pub fn from_str_exact(s: &str) -> Option<Self> {
        match s {
            "session" => Some(Self::Session),
            "ab" => Some(Self::Ab),
            "pool" => Some(Self::Pool),
            "fork" => Some(Self::Fork),
            "manual" => Some(Self::Manual),
            "subagent" => Some(Self::Subagent),
            _ => None,
        }
    }

    /// Config key parse: trim + case-insensitive; unknown → None.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        let t = s.trim();
        if let Some(k) = Self::from_str_exact(t) {
            return Some(k);
        }
        // Only allocate lowercase when needed.
        if t.bytes().any(|b| b.is_ascii_uppercase()) {
            Self::from_str_exact(&t.to_ascii_lowercase())
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeStatus {
    Alive,
    Dead,
}

impl WorktreeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::Dead => "dead",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "alive" => Self::Alive,
            "dead" => Self::Dead,
            _ => Self::Dead,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub id: String,
    pub path: PathBuf,
    pub source_repo: PathBuf,
    pub repo_name: String,
    pub kind: WorktreeKind,
    pub creation_mode: String,
    pub git_ref: Option<String>,
    pub head_commit: Option<String>,
    pub session_id: Option<String>,
    pub creator_pid: Option<u32>,
    pub created_at: i64,
    pub last_accessed_at: Option<i64>,
    pub status: WorktreeStatus,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Default)]
pub struct ListFilter {
    pub repo_name: Option<String>,
    pub source_repo: Option<PathBuf>,
    pub kind: Option<WorktreeKind>,
    pub status: Option<WorktreeStatus>,
    pub include_dead: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbStats {
    pub total_records: u64,
    pub alive_count: u64,
    pub dead_count: u64,
    pub db_file_bytes: u64,
}

pub struct WorktreeDb {
    conn: Connection,
}

impl WorktreeDb {
    /// Open (or create) the DB at `grok_home/worktrees.db`.
    pub fn open(grok_home: &Path) -> Result<Self> {
        Self::open_at(&grok_home.join("worktrees.db"))
    }

    /// Open with an explicit path.
    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create dir for DB: {}", parent.display()))?;
        }
        // The mode decision statfs's the parent dir created above.
        Self::open_at_with_journal_mode(path, JournalMode::for_db_path(path))
    }

    /// Open with an explicit journal mode — the seam tests use to exercise
    /// the network-filesystem decision on a local disk.
    fn open_at_with_journal_mode(path: &Path, journal_mode: JournalMode) -> Result<Self> {
        // Per-host sibling on network mounts (see JournalMode::effective_db_path).
        let path = journal_mode.effective_db_path(path);
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open worktree DB: {}", path.display()))?;
        let db = Self { conn };
        db.set_journal_mode(journal_mode)?;
        // Normal statement timeout, now that the conversion budget is done.
        db.conn
            .busy_timeout(std::time::Duration::from_millis(5000))?;
        db.init_schema()?;
        Ok(db)
    }

    /// Put the database in `mode`'s journal mode, retrying on `SQLITE_BUSY`
    /// under one absolute deadline (~10s total).
    ///
    /// Conversion-lock acquisition only partially honors `busy_timeout` (see
    /// `JournalMode::apply`, the single source of truth): a second process
    /// opening the same file at the same instant can still get `SQLITE_BUSY`
    /// immediately. Without a retry that opener's `open_at` fails, and callers
    /// like `register_worktree`/`unregister_worktree` swallow the error
    /// (best-effort) — silently dropping worktree tracking, exactly what this DB
    /// exists to prevent. A bounded retry rides out the concurrent converter
    /// (which finishes in microseconds), while the deadline plus a per-attempt
    /// `busy_timeout` cap keeps a held legacy lock from stalling startup by
    /// `attempts x busy_timeout`. Once converted the setting persists (WAL) or
    /// re-applies as a no-op (TRUNCATE), so later opens are cheap.
    fn set_journal_mode(&self, mode: JournalMode) -> Result<()> {
        use rusqlite::ErrorCode;
        use std::time::{Duration, Instant};
        // Total conversion budget; each attempt waits at most 1s for locks.
        const DEADLINE: Duration = Duration::from_secs(10);
        let start = Instant::now();
        let mut last_err = None;
        loop {
            let remaining = DEADLINE.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            self.conn
                .busy_timeout(remaining.min(Duration::from_millis(1000)))?;
            match mode.apply(&self.conn) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let busy = matches!(
                        &e,
                        rusqlite::Error::SqliteFailure(f, _)
                            if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
                    );
                    if !busy {
                        return Err(e).with_context(|| {
                            format!("failed to set journal mode {}", mode.as_str())
                        });
                    }
                    last_err = Some(e);
                    // Brief pause so fail-fast busy errors don't spin hot.
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        Err(last_err.expect("deadline allows at least one attempt")).with_context(|| {
            format!(
                "failed to set journal mode {} (database busy after {:?})",
                mode.as_str(),
                start.elapsed()
            )
        })
    }

    /// Open the default DB at `~/.grok/worktrees.db`.
    ///
    /// Discovers grok home via `$GROK_HOME`, falling back to the canonicalized
    /// `$HOME/.grok` (matching `xai_grok_config::grok_home`).
    /// Path is resolved fresh each call (~1µs env var read) to support
    /// test overrides. Each call opens its own connection — callers in hot
    /// paths should cache the `WorktreeDb` instance.
    pub fn open_default() -> Result<Self> {
        Self::open(&resolve_grok_home()?)
    }

    /// Open an in-memory DB (for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory DB")?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(schema::INIT_SQL)
            .context("failed to init worktree DB schema")?;

        let stored: Option<String> = self
            .conn
            .query_row(schema::GET_META, ["schema_version"], |row| row.get(0))
            .ok();

        let needs_update = match stored {
            None => true,
            Some(v) => v.parse::<u32>().unwrap_or(0) < schema::SCHEMA_VERSION,
        };
        if needs_update {
            self.conn.execute(
                schema::UPSERT_META,
                rusqlite::params!["schema_version", schema::SCHEMA_VERSION.to_string()],
            )?;
        }

        Ok(())
    }

    pub fn register(&self, record: &WorktreeRecord) -> Result<()> {
        queries::register(&self.conn, record)
    }

    pub fn unregister(&self, id: &str) -> Result<bool> {
        queries::unregister(&self.conn, id)
    }

    pub fn unregister_by_path(&self, path: &Path) -> Result<bool> {
        queries::unregister_by_path(&self.conn, path)
    }

    pub fn mark_dead(&self, id: &str) -> Result<bool> {
        queries::mark_dead(&self.conn, id)
    }

    pub fn touch(&self, id: &str) -> Result<bool> {
        queries::touch(&self.conn, id)
    }

    /// Look up a worktree by its DB ID only (no label or path fallback).
    pub fn get_by_id(&self, id: &str) -> Result<Option<WorktreeRecord>> {
        queries::get_by_id(&self.conn, id)
    }

    /// Look up by ID, label, or path.
    ///
    /// If `id_or_path` contains `/`, it's treated as a path (canonicalized
    /// before lookup). Otherwise it's looked up first as a DB ID, then as a
    /// worktree label (stored in `metadata.label`).
    pub fn get(&self, id_or_path: &str) -> Result<Option<WorktreeRecord>> {
        if id_or_path.contains('/') {
            let canon = PathBuf::from(id_or_path);
            let canon = dunce::canonicalize(&canon).unwrap_or(canon);
            queries::get_by_path(&self.conn, &canon)
        } else {
            let by_id = queries::get_by_id(&self.conn, id_or_path)?;
            if by_id.is_some() {
                return Ok(by_id);
            }
            queries::get_by_label(&self.conn, id_or_path)
        }
    }

    /// Look up a worktree by its label (stored in metadata JSON).
    pub fn get_by_label(&self, label: &str) -> Result<Option<WorktreeRecord>> {
        queries::get_by_label(&self.conn, label)
    }

    pub fn list(&self, filter: &ListFilter) -> Result<Vec<WorktreeRecord>> {
        queries::list(&self.conn, filter)
    }

    pub fn stats(&self) -> Result<DbStats> {
        queries::stats(&self.conn)
    }

    /// Mark all records whose paths no longer exist on disk as dead.
    /// Returns the number of records marked.
    pub fn sweep_dead(&self) -> Result<u64> {
        queries::sweep_dead(&self.conn)
    }

    /// Read a value from the `meta` table. `Ok(None)` when the key is absent.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        match self
            .conn
            .query_row(schema::GET_META, [key], |row| row.get(0))
        {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("failed to read meta key {key}")),
        }
    }

    /// Insert or replace a `meta` table value.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(schema::UPSERT_META, rusqlite::params![key, value])
            .with_context(|| format!("failed to write meta key {key}"))?;
        Ok(())
    }

    /// Test-only: run raw SQL (e.g. drop tables to force fail-closed paths).
    #[cfg(test)]
    pub(crate) fn execute_batch_for_test(&self, sql: &str) -> Result<()> {
        self.conn
            .execute_batch(sql)
            .context("execute_batch_for_test failed")?;
        Ok(())
    }
}

/// Derive a worktree ID from its destination path: `<basename>-<hash of full path>`
/// (the last component, minus any `worktree-` prefix, plus a full-path hash).
///
/// The basename alone collides across repos, and `INSERT OR REPLACE` would then evict
/// the other repo's record; hashing the full path keeps distinct worktrees distinct.
pub fn id_from_path(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let base = name.strip_prefix("worktree-").unwrap_or(&name);
    format!("{base}-{}", crate::copy::shard::short_path_hash(path))
}

/// Extract the repo name (last component) from a source repo path.
pub fn repo_name_from_path(source: &Path) -> String {
    source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string())
}

pub fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn resolve_grok_home() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("GROK_HOME") {
        return Ok(PathBuf::from(v));
    }
    let home = PathBuf::from(std::env::var("HOME").context("neither $GROK_HOME nor $HOME is set")?);
    // Canonicalize the home dir so worktree paths share the same physical .grok
    // tree as trust/hooks even when it is symlinked. The dunce canonicalization
    // must stay in sync with xai_grok_config::default_grok_home();
    // home resolution deliberately differs ($HOME here vs std::env::home_dir()).
    Ok(dunce::canonicalize(&home).unwrap_or(home).join(".grok"))
}

/// Serializes tests that mutate the process-global `GROK_HOME` env var so they
/// don't clobber each other under `cargo test`, where tests share one process
/// (nextest isolates per-process, but the suite must also pass under `cargo test`).
#[cfg(test)]
static GROK_HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test-only isolation for code that resolves the DB via `open_default()`.
///
/// Holds [`GROK_HOME_ENV_LOCK`] (serializing concurrent setters), points
/// `GROK_HOME` at a fresh private tmp dir, and restores the prior value on drop.
/// Use instead of hand-rolling the lock + restore guard + tmp dir per test.
///
/// `Drop` restores `GROK_HOME` before `_lock` releases, so the env is correct
/// before another waiting setter proceeds.
#[cfg(test)]
pub(crate) struct GrokHomeFixture {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<std::ffi::OsString>,
    /// The isolated grok home; pass to `WorktreeDb::open` to read the same DB
    /// `open_default()` writes to.
    pub home: PathBuf,
    _tmp: tempfile::TempDir,
}

#[cfg(test)]
impl GrokHomeFixture {
    pub(crate) fn new() -> Self {
        let lock = GROK_HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("grok-home");
        std::fs::create_dir_all(&home).unwrap();
        // Warm up the DB (journal-mode conversion + schema) before exposing it
        // via GROK_HOME, sparing the test hot loop set_journal_mode's retry
        // sleeps. This open has exclusive access (nothing reaches the path
        // until GROK_HOME points here); set_journal_mode's retry is the actual
        // race fix.
        let _ = WorktreeDb::open(&home);
        let prev = std::env::var_os("GROK_HOME");
        unsafe { std::env::set_var("GROK_HOME", &home) };
        Self {
            _lock: lock,
            prev,
            home,
            _tmp: tmp,
        }
    }
}

#[cfg(test)]
impl Drop for GrokHomeFixture {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(p) => std::env::set_var("GROK_HOME", p),
                None => std::env::remove_var("GROK_HOME"),
            }
        }
    }
}

#[cfg(test)]
mod tests;
