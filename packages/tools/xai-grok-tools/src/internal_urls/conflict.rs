//! Session-scoped merge-conflict registry for `conflict://` URLs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::register_resource;

/// Which side of a conflict marker region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSide {
    Ours,
    Theirs,
    Base,
    Both,
}

impl ConflictSide {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ours" | "head" => Some(Self::Ours),
            "theirs" => Some(Self::Theirs),
            "base" => Some(Self::Base),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ours => "ours",
            Self::Theirs => "theirs",
            Self::Base => "base",
            Self::Both => "both",
        }
    }
}

/// One registered conflict marker block.
#[derive(Debug, Clone)]
pub struct RegisteredConflict {
    pub id: u32,
    pub file_path: PathBuf,
    /// Full marker block including `<<<<<<<` / `=======` / `>>>>>>>` lines.
    pub marker_block: String,
    pub ours: String,
    pub theirs: String,
    pub base: Option<String>,
    /// Byte offset of the marker block in the file at registration time.
    pub start_byte: usize,
    pub end_byte: usize,
}

/// In-memory conflict registry (session-scoped).
#[derive(Debug, Default)]
pub struct ConflictRegistry {
    next_id: u32,
    by_id: HashMap<u32, RegisteredConflict>,
}

impl ConflictRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `text` for standard git conflict markers and register each block.
    /// Returns the list of newly assigned ids (stable for the session until cleared).
    pub fn register_from_text(&mut self, file_path: PathBuf, text: &str) -> Vec<u32> {
        let mut ids = Vec::new();
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // Find <<<<<<<
            let Some(start) = find_line_start(bytes, i, b"<<<<<<<") else {
                break;
            };
            let after_start = skip_line(bytes, start);
            let Some(mid) = find_line_start(bytes, after_start, b"=======") else {
                break;
            };
            // Optional base section: <<<<<<< / ||||||| / ======= / >>>>>>>
            let base_sep = find_line_start(bytes, after_start, b"|||||||");
            let ours_end = base_sep.unwrap_or(mid);
            let theirs_start = skip_line(bytes, mid);
            let Some(end_line) = find_line_start(bytes, theirs_start, b">>>>>>>") else {
                break;
            };
            let end = skip_line(bytes, end_line);

            let ours = String::from_utf8_lossy(&bytes[after_start..ours_end]).to_string();
            let base = base_sep.map(|b| {
                let base_body_start = skip_line(bytes, b);
                String::from_utf8_lossy(&bytes[base_body_start..mid]).to_string()
            });
            let theirs = String::from_utf8_lossy(&bytes[theirs_start..end_line]).to_string();
            let marker_block = String::from_utf8_lossy(&bytes[start..end]).to_string();

            self.next_id = self.next_id.saturating_add(1);
            let id = self.next_id;
            self.by_id.insert(
                id,
                RegisteredConflict {
                    id,
                    file_path: file_path.clone(),
                    marker_block,
                    ours,
                    theirs,
                    base,
                    start_byte: start,
                    end_byte: end,
                },
            );
            ids.push(id);
            i = end;
        }
        ids
    }

    pub fn get(&self, id: u32) -> Option<&RegisteredConflict> {
        self.by_id.get(&id)
    }

    pub fn list(&self) -> Vec<&RegisteredConflict> {
        let mut v: Vec<_> = self.by_id.values().collect();
        v.sort_by_key(|c| c.id);
        v
    }

    pub fn remove(&mut self, id: u32) -> Option<RegisteredConflict> {
        self.by_id.remove(&id)
    }

    pub fn clear(&mut self) {
        self.by_id.clear();
    }
}

fn find_line_start(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i..].starts_with(needle)
            && (i == 0 || bytes[i - 1] == b'\n')
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn skip_line(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// Shared resource wrapping the registry.
#[derive(Clone, Default)]
pub struct ConflictRegistryResource(pub Arc<Mutex<ConflictRegistry>>);

impl ConflictRegistryResource {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(ConflictRegistry::new())))
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, ConflictRegistry> {
        self.0.lock().expect("conflict registry lock")
    }
}

impl std::fmt::Debug for ConflictRegistryResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConflictRegistryResource").finish()
    }
}

register_resource!("grok_build", "ConflictRegistryResource", ConflictRegistryResource);

/// Apply a write resolution to a conflict id. Returns the updated file bytes
/// when the on-disk marker still matches the registered block.
pub fn resolve_conflict_write(
    registry: &mut ConflictRegistry,
    id: u32,
    content: &str,
) -> Result<(PathBuf, String), String> {
    let conflict = registry
        .get(id)
        .cloned()
        .ok_or_else(|| format!("unknown conflict id {id}; re-read path:conflicts to register"))?;

    let disk = std::fs::read_to_string(&conflict.file_path)
        .map_err(|e| format!("cannot read {}: {e}", conflict.file_path.display()))?;
    if !disk.contains(&conflict.marker_block) {
        return Err(format!(
            "conflict {id} is stale (marker not found on disk); re-register via path:conflicts"
        ));
    }

    let replacement = expand_side_tokens(content, &conflict)?;
    let new_text = disk.replacen(&conflict.marker_block, &replacement, 1);
    std::fs::write(&conflict.file_path, &new_text)
        .map_err(|e| format!("failed to write {}: {e}", conflict.file_path.display()))?;
    registry.remove(id);
    Ok((conflict.file_path, new_text))
}

fn expand_side_tokens(content: &str, c: &RegisteredConflict) -> Result<String, String> {
    let trimmed = content.trim();
    if let Some(side) = ConflictSide::parse(trimmed.strip_prefix('@').unwrap_or(trimmed)) {
        return Ok(match side {
            ConflictSide::Ours => c.ours.clone(),
            ConflictSide::Theirs => c.theirs.clone(),
            ConflictSide::Base => c
                .base
                .clone()
                .ok_or_else(|| "conflict has no base section".to_string())?,
            ConflictSide::Both => format!("{}{}", c.ours, c.theirs),
        });
    }
    Ok(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_resolve_theirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        let text = "pre\n<<<<<<< HEAD\nours line\n=======\ntheirs line\n>>>>>>> branch\npost\n";
        std::fs::write(&path, text).unwrap();

        let mut reg = ConflictRegistry::new();
        let ids = reg.register_from_text(path.clone(), text);
        assert_eq!(ids, vec![1]);

        let (p, out) = resolve_conflict_write(&mut reg, 1, "@theirs").unwrap();
        assert_eq!(p, path);
        assert!(out.contains("theirs line"));
        assert!(!out.contains("<<<<<<<"));
        assert!(reg.get(1).is_none());
    }
}
