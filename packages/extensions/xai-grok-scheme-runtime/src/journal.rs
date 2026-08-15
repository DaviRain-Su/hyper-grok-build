//! Append-only redefine journal — the durable half of the self-modification
//! layer (zag `zag-live` semantics, simplified to a single journal file):
//!
//! - one fsynced line per entry;
//! - a torn **final** line is truncated on read; an unreadable entry anywhere
//!   earlier is [`JournalError::Corrupt`] (fail closed, never mid-file skip);
//! - `commit` promotes all pending redefines to committed;
//! - `quarantine` drops all pending redefines from every future replay;
//! - effective state = committed + pending (journal is fsynced **before** a
//!   redefine is applied to the live image, so replay after a crash restores
//!   exactly what was journaled).

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::sexp::Sexp;

pub const JOURNAL_FILE: &str = "journal.sexp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalEntry {
    Redefine {
        plugin: String,
        event: String,
        source: String,
        ts: i64,
    },
    Commit {
        ts: i64,
    },
    Quarantine {
        ts: i64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal corrupt at line {line}: {detail}")]
    Corrupt { line: usize, detail: String },
}

impl JournalEntry {
    fn now_ts() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    pub fn redefine(plugin: &str, event: &str, source: &str) -> Self {
        Self::Redefine {
            plugin: plugin.to_string(),
            event: event.to_string(),
            source: source.to_string(),
            ts: Self::now_ts(),
        }
    }

    pub fn commit() -> Self {
        Self::Commit { ts: Self::now_ts() }
    }

    pub fn quarantine() -> Self {
        Self::Quarantine { ts: Self::now_ts() }
    }

    fn to_sexp(&self) -> Sexp {
        match self {
            Self::Redefine {
                plugin,
                event,
                source,
                ts,
            } => Sexp::list(vec![
                Sexp::sym("redefine"),
                Sexp::str(plugin.clone()),
                Sexp::sym(event),
                Sexp::str(source.clone()),
                Sexp::Int(*ts),
            ]),
            Self::Commit { ts } => Sexp::list(vec![Sexp::sym("commit"), Sexp::Int(*ts)]),
            Self::Quarantine { ts } => Sexp::list(vec![Sexp::sym("quarantine"), Sexp::Int(*ts)]),
        }
    }

    fn from_sexp(v: &Sexp) -> Option<Self> {
        match v.head_sym()? {
            "redefine" => Some(Self::Redefine {
                plugin: v.arg(0)?.as_str()?.to_string(),
                event: match v.arg(1)? {
                    Sexp::Sym(s) => s.clone(),
                    _ => return None,
                },
                source: v.arg(2)?.as_str()?.to_string(),
                ts: v.arg(3)?.as_int()?,
            }),
            "commit" => Some(Self::Commit {
                ts: v.arg(0)?.as_int()?,
            }),
            "quarantine" => Some(Self::Quarantine {
                ts: v.arg(0)?.as_int()?,
            }),
            _ => None,
        }
    }
}

/// One effective redefine to replay at image boot (last write per
/// (plugin, event) wins by replay order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRedefine {
    pub plugin: String,
    pub event: String,
    pub source: String,
    /// False = still pending (journaled but not yet committed).
    pub committed: bool,
}

/// Snapshot for `/live status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalStatus {
    pub committed: usize,
    pub pending: usize,
}

pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(JOURNAL_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry: single write + fsync (fsync-before-apply invariant).
    pub fn append(&self, entry: &JournalEntry) -> Result<(), JournalError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut line = entry.to_sexp().render();
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    /// Load all entries. A torn final line is silently truncated; a bad entry
    /// anywhere earlier fails closed.
    pub fn load(&self) -> Result<Vec<JournalEntry>, JournalError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let lines: Vec<&str> = raw.lines().collect();
        let mut entries = Vec::with_capacity(lines.len());
        for (idx, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let parsed = Sexp::parse(line).ok().and_then(|v| JournalEntry::from_sexp(&v));
            match parsed {
                Some(entry) => entries.push(entry),
                None if idx + 1 == lines.len() && !raw.ends_with('\n') => {
                    // Torn tail from a crash mid-append: drop it.
                    tracing::warn!("scheme journal: truncating torn final line");
                    break;
                }
                None => {
                    return Err(JournalError::Corrupt {
                        line: idx + 1,
                        detail: format!("unreadable entry: {line}"),
                    });
                }
            }
        }
        Ok(entries)
    }

    /// Compute the effective replay set + status from the entry log.
    pub fn effective(entries: &[JournalEntry]) -> (Vec<EffectiveRedefine>, JournalStatus) {
        let mut committed: Vec<EffectiveRedefine> = Vec::new();
        let mut pending: Vec<EffectiveRedefine> = Vec::new();
        for entry in entries {
            match entry {
                JournalEntry::Redefine {
                    plugin,
                    event,
                    source,
                    ..
                } => pending.push(EffectiveRedefine {
                    plugin: plugin.clone(),
                    event: event.clone(),
                    source: source.clone(),
                    committed: false,
                }),
                JournalEntry::Commit { .. } => {
                    for mut p in pending.drain(..) {
                        p.committed = true;
                        committed.push(p);
                    }
                }
                JournalEntry::Quarantine { .. } => pending.clear(),
            }
        }
        let status = JournalStatus {
            committed: committed.len(),
            pending: pending.len(),
        };
        let mut all = committed;
        all.extend(pending);
        (all, status)
    }

    /// Convenience: load + effective.
    pub fn load_effective(&self) -> Result<(Vec<EffectiveRedefine>, JournalStatus), JournalError> {
        let entries = self.load()?;
        Ok(Self::effective(&entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_load_effective_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::new(dir.path());
        assert!(j.load().unwrap().is_empty());

        j.append(&JournalEntry::redefine("p1", "pre-tool-use", "(lambda (ctx) '(allow))"))
            .unwrap();
        j.append(&JournalEntry::commit()).unwrap();
        j.append(&JournalEntry::redefine("p1", "stop", "(lambda (ctx) '(continue))"))
            .unwrap();

        let (effective, status) = j.load_effective().unwrap();
        assert_eq!(status, JournalStatus { committed: 1, pending: 1 });
        assert_eq!(effective.len(), 2);
        assert!(effective[0].committed);
        assert_eq!(effective[0].event, "pre-tool-use");
        assert!(!effective[1].committed);

        // Quarantine drops the pending entry only.
        j.append(&JournalEntry::quarantine()).unwrap();
        let (effective, status) = j.load_effective().unwrap();
        assert_eq!(status, JournalStatus { committed: 1, pending: 0 });
        assert_eq!(effective.len(), 1);
    }

    #[test]
    fn torn_tail_truncated_midfile_corrupt_fails() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::new(dir.path());
        j.append(&JournalEntry::redefine("p", "stop", "(lambda (ctx) '(continue))"))
            .unwrap();

        // Torn tail: partial write without trailing newline.
        let mut raw = std::fs::read_to_string(j.path()).unwrap();
        raw.push_str("(redefine \"p\" stop \"(lam");
        std::fs::write(j.path(), &raw).unwrap();
        assert_eq!(j.load().unwrap().len(), 1);

        // Mid-file garbage with a valid entry after it = corrupt.
        let good = JournalEntry::commit();
        let mut corrupted = String::from("this is not a journal line\n");
        corrupted.push_str(&{
            let mut l = good.to_sexp_test();
            l.push('\n');
            l
        });
        std::fs::write(j.path(), &corrupted).unwrap();
        assert!(matches!(j.load(), Err(JournalError::Corrupt { line: 1, .. })));
    }

    #[test]
    fn escaped_sources_survive() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::new(dir.path());
        let tricky = "(lambda (ctx)\n  (if (string=? \"a\\\\b\" x) '(deny \"say \\\"no\\\"\") '(allow)))";
        j.append(&JournalEntry::redefine("p", "pre-tool-use", tricky)).unwrap();
        let entries = j.load().unwrap();
        match &entries[0] {
            JournalEntry::Redefine { source, .. } => assert_eq!(source, tricky),
            other => panic!("unexpected entry {other:?}"),
        }
    }

    impl JournalEntry {
        fn to_sexp_test(&self) -> String {
            self.to_sexp().render()
        }
    }
}
