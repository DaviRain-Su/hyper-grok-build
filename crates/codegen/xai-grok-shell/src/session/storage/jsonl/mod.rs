use super::{PersistedData, SessionUpdateEnvelope, StorageAdapter, updates_truncate_for_prompt};
use crate::sampling::types::ChatRequestMessage;
use crate::sampling::{
    ContentPart, ConversationItem, conversation_truncate_for_prompt, transform_conversation_cwd,
};
use crate::session::info::Info;
use crate::session::persistence::{CHAT_FORMAT_VERSION, Summary};
use crate::tools::todo::TodoState;
use agent_client_protocol as acp;
use async_trait::async_trait;
use fs2::FileExt;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, Write};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use xai_grok_workspace::session::file_state::RewindPoint;
#[derive(Clone)]
enum SessionDirMode {
    FromRoot(PathBuf),
    Explicit(PathBuf),
}
#[derive(Clone, Copy)]
pub(crate) enum AppendDurability {
    Buffered,
    Durable,
}
/// JSONL storage under `{root}/sessions/{url_encoded_cwd}/{session_id}/`.
#[derive(Clone)]
pub struct JsonlStorageAdapter {
    dir_mode: SessionDirMode,
    #[cfg(test)]
    update_append_probe: Option<std::sync::Arc<AppendProbe>>,
}
#[cfg(test)]
type AppendProbe = dyn Fn(AppendDurability) -> io::Result<()> + Send + Sync;
impl Default for JsonlStorageAdapter {
    fn default() -> Self {
        Self::new()
    }
}
impl JsonlStorageAdapter {
    pub fn new() -> Self {
        Self {
            dir_mode: SessionDirMode::FromRoot(crate::util::grok_home::grok_home()),
            #[cfg(test)]
            update_append_probe: None,
        }
    }
    pub fn with_root(root_dir: PathBuf) -> Self {
        Self {
            dir_mode: SessionDirMode::FromRoot(root_dir),
            #[cfg(test)]
            update_append_probe: None,
        }
    }
    /// Create an adapter that writes directly to `session_dir`, bypassing
    /// the `{root}/sessions/{cwd}/{id}/` path computation.
    ///
    /// Used for subagent child sessions whose files live under the parent's
    /// session directory: `{parent_session_dir}/subagents/{subagent_id}/`.
    pub fn with_explicit_session_dir(session_dir: PathBuf) -> Self {
        Self {
            dir_mode: SessionDirMode::Explicit(session_dir),
            #[cfg(test)]
            update_append_probe: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn with_update_append_probe(
        session_dir: PathBuf,
        append_probe: impl Fn(AppendDurability) -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            dir_mode: SessionDirMode::Explicit(session_dir),
            update_append_probe: Some(std::sync::Arc::new(append_probe)),
        }
    }
    /// Load chat history from a specific directory.
    /// Used by fork bootstrap to load the copied parent conversation.
    pub fn load_chat_history_from_dir(
        &self,
        dir: &std::path::Path,
    ) -> std::io::Result<Vec<ConversationItem>> {
        let chat_file = dir.join(super::CHAT_HISTORY_FILE);
        self.read_chat_history_sync(chat_file, CHAT_FORMAT_VERSION)
    }
    fn session_dir(&self, info: &Info) -> PathBuf {
        match &self.dir_mode {
            SessionDirMode::FromRoot(root) => root
                .join("sessions")
                .join(crate::util::grok_home::encode_cwd_dirname(&info.cwd))
                .join(info.id.to_string()),
            SessionDirMode::Explicit(dir) => dir.clone(),
        }
    }
    pub(super) fn updates_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::UPDATES_FILE)
    }
    fn chat_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::CHAT_HISTORY_FILE)
    }
    fn ensure_chat_history(&self, info: &Info, chat_format_version: u8) -> io::Result<()> {
        if chat_format_version != crate::session::persistence::CHAT_FORMAT_VERSION {
            return Ok(());
        }
        let chat_file = self.chat_file(info);
        if std::fs::metadata(&chat_file).map(|m| m.len()).unwrap_or(0) == 0 {
            super::chat_rebuild::rebuild_chat_history(&self.session_dir(info))?;
        }
        Ok(())
    }
    fn summary_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::SUMMARY_FILE)
    }
    fn summary_lock_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info)
            .join(format!("{}.lock", super::SUMMARY_FILE))
    }
    fn plan_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::PLAN_FILE)
    }
    fn plan_mode_state_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::PLAN_MODE_FILE)
    }
    fn signals_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::SIGNALS_FILE)
    }
    fn announcement_state_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::ANNOUNCEMENT_STATE_FILE)
    }
    fn goal_mode_state_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::GOAL_STATE_FILE)
    }
    fn rewind_points_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join("rewind_points.jsonl")
    }
    fn feedback_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join("feedback.jsonl")
    }
    fn btw_history_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join("btw_history.jsonl")
    }
    /// Enumerate all session directories, optionally filtered by cwd.
    ///
    /// Returns the path to each session directory (not the summary file).
    /// Shared by both `list_sessions` (full scan) and `list_sessions_recent`
    /// (mtime-based tail).
    fn scan_session_dirs(&self, cwd: Option<&str>) -> Vec<PathBuf> {
        let root_dir = match &self.dir_mode {
            SessionDirMode::FromRoot(root) => root.clone(),
            SessionDirMode::Explicit(_) => return Vec::new(),
        };
        let sessions_root = root_dir.join("sessions");
        if !sessions_root.exists() {
            return Vec::new();
        }
        let mut scan_cwds: Vec<PathBuf> = Vec::new();
        if let Some(cwd_str) = cwd {
            let enc = crate::util::grok_home::encode_cwd_dirname(cwd_str);
            scan_cwds.push(sessions_root.join(enc));
        } else {
            match std::fs::read_dir(&sessions_root) {
                Ok(it) => {
                    for entry in it.flatten() {
                        let p = entry.path();
                        if p.is_dir() {
                            scan_cwds.push(p);
                        }
                    }
                }
                Err(_) => return Vec::new(),
            }
        }
        let mut session_dirs = Vec::new();
        for cwd_dir in scan_cwds {
            let it = match std::fs::read_dir(&cwd_dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for entry in it.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    session_dirs.push(path);
                }
            }
        }
        session_dirs
    }
    fn list_sessions_sync(&self, cwd: Option<&str>) -> io::Result<Vec<Summary>> {
        let session_dirs = self.scan_session_dirs(cwd);
        let mut summaries = Vec::new();
        for session_dir in session_dirs {
            let summary_path = session_dir.join(super::SUMMARY_FILE);
            match std::fs::read(&summary_path) {
                Ok(bytes) => {
                    if let Ok(summary) = serde_json::from_slice::<Summary>(&bytes)
                        && !summary.is_hidden()
                    {
                        summaries.push(summary);
                    }
                }
                Err(_) => continue,
            }
        }
        summaries.sort_by_cached_key(|s| {
            (
                std::cmp::Reverse(s.last_active_at.unwrap_or(s.updated_at)),
                s.info.id.0.to_string(),
            )
        });
        Ok(summaries)
    }
    /// List the N most recently modified session summaries across all
    /// workspaces.
    ///
    /// Instead of reading every `summary.json` (expensive at scale — ~12K
    /// files), this stats each file to get its mtime, sorts by mtime, and
    /// only reads the top `limit` files. On a machine with ~12K sessions
    /// this reduces cold-boot `workspace_list` from ~3s to ~200ms.
    /// Final order among candidates uses `last_active_at` else `updated_at`.
    pub async fn list_sessions_recent(&self, limit: usize) -> io::Result<Vec<Summary>> {
        let session_dirs = self.scan_session_dirs(None);
        let mut candidates: Vec<(PathBuf, std::time::SystemTime)> =
            Vec::with_capacity(session_dirs.len());
        for session_dir in session_dirs {
            let summary_path = session_dir.join(super::SUMMARY_FILE);
            if let Ok(meta) = std::fs::metadata(&summary_path)
                && let Ok(mtime) = meta.modified()
            {
                candidates.push((summary_path, mtime));
            }
        }
        candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        candidates.truncate(limit);
        let mut summaries = Vec::with_capacity(candidates.len());
        for (summary_path, _) in candidates {
            match std::fs::read(&summary_path) {
                Ok(bytes) => {
                    if let Ok(summary) = serde_json::from_slice::<Summary>(&bytes)
                        && !summary.is_hidden()
                    {
                        summaries.push(summary);
                    }
                }
                Err(_) => continue,
            }
        }
        summaries.sort_by_cached_key(|s| {
            (
                std::cmp::Reverse(s.last_active_at.unwrap_or(s.updated_at)),
                s.info.id.0.to_string(),
            )
        });
        Ok(summaries)
    }
    async fn append_jsonl<T: serde::Serialize>(&self, path: PathBuf, data: &T) -> io::Result<()> {
        let mut line =
            serde_json::to_vec(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        self.append_jsonl_line(path, line).await
    }
    async fn append_jsonl_line(&self, path: PathBuf, line: Vec<u8>) -> io::Result<()> {
        Self::append_jsonl_line_blocking(path, line, AppendDurability::Buffered).await
    }
    async fn append_jsonl_line_blocking(
        path: PathBuf,
        line: Vec<u8>,
        durability: AppendDurability,
    ) -> io::Result<()> {
        tokio::task::spawn_blocking(move || Self::append_jsonl_line_sync(&path, line, durability))
            .await
            .map_err(io::Error::other)?
    }
    /// Append one JSONL record, healing a torn tail before writing.
    ///
    /// Appends are not crash-atomic: a process kill / `ENOSPC` mid-`write_all`
    /// (e.g. the auto-update leader relaunch aborting a persistence actor
    /// mid-append) leaves the file ending in a *partial* record with no
    /// trailing newline. Because append failures are logged-and-continued by
    /// the persistence actor, a plain `O_APPEND` write of the next record
    /// would concatenate it onto that partial line, producing a merged line
    /// that fails to parse (``expected `,` or `}` at line 1 column N``) and —
    /// before the readers became corruption-tolerant — bricked session resume.
    ///
    /// Before writing, check the last byte: if it isn't `\n`, prepend one so
    /// the torn record is terminated as its own (single) corrupt line. This
    /// bounds the damage of any torn write to exactly one record, which the
    /// lenient readers (e.g. [`Self::read_chat_history_sync`]) then skip.
    fn append_jsonl_line_sync(
        path: &Path,
        line: Vec<u8>,
        durability: AppendDurability,
    ) -> io::Result<()> {
        Self::append_jsonl_line_sync_with(path, line, durability, Self::sync_file_durable, || {
            Self::sync_parent_directory(path)
        })
    }
    fn append_jsonl_line_sync_with(
        path: &Path,
        mut line: Vec<u8>,
        durability: AppendDurability,
        mut sync_file: impl FnMut(&std::fs::File) -> io::Result<()>,
        mut sync_parent: impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        debug_assert!(line.ends_with(b"\n"), "JSONL record must end with \\n");
        let lock = Self::lock_append(path)?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .read(true)
                .create(true)
                .append(true)
                .open(path)?;
            let len = file.metadata()?.len();
            if len > 0 {
                file.seek(io::SeekFrom::Start(len - 1))?;
                let mut last = [0u8; 1];
                file.read_exact(&mut last)?;
                if last[0] != b'\n' {
                    tracing::warn!(
                        path = % path.display(),
                        "jsonl file has a torn trailing line (previous append crashed mid-write?); terminating it before appending"
                    );
                    line.insert(0, b'\n');
                }
            }
            file.write_all(&line)?;
            file.flush()?;
            if matches!(durability, AppendDurability::Durable) {
                sync_file(&file)?;
                drop(file);
                sync_parent()?;
            } else {
                drop(file);
            }
            Ok(())
        })();
        let _ = lock.unlock();
        result
    }
    /// Lock tail healing, append, and barriers through `<target>.jsonl.lock`.
    /// Full-file [`Self::write_jsonl`] atomic-rename rewrites bypass this append-only lock.
    fn lock_append(path: &Path) -> io::Result<std::fs::File> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("jsonl.lock"))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }
    #[cfg(target_os = "macos")]
    fn sync_file_durable(file: &std::fs::File) -> io::Result<()> {
        file.sync_all()?;
        Self::fullfsync_raw(file.as_raw_fd())
    }
    #[cfg(target_os = "macos")]
    fn fullfsync_raw(fd: std::os::fd::RawFd) -> io::Result<()> {
        let result = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    fn sync_file_durable(file: &std::fs::File) -> io::Result<()> {
        file.sync_all()
    }
    #[cfg(windows)]
    fn sync_file_durable(file: &std::fs::File) -> io::Result<()> {
        file.sync_all()
    }
    #[cfg(not(any(unix, windows)))]
    fn sync_file_durable(_file: &std::fs::File) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable file sync is unsupported on this platform",
        ))
    }
    #[cfg(unix)]
    fn sync_parent_directory(path: &Path) -> io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "update has no parent"))?;
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(windows)]
    fn sync_parent_directory(_path: &Path) -> io::Result<()> {
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    fn sync_parent_directory(_path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable directory sync is unsupported on this platform",
        ))
    }
    /// Write a full JSONL file (rewriting all items), crash-atomically: serialize
    /// to a temp file then rename over the target, so a crash / `ENOSPC` mid-write
    /// can't truncate the existing file (e.g. lose `rewind_points.jsonl` history).
    async fn write_jsonl<T: serde::Serialize>(&self, path: PathBuf, items: &[T]) -> io::Result<()> {
        super::write_jsonl_atomic_async(&path, items).await
    }
    fn read_jsonl<T: serde::de::DeserializeOwned>(&self, path: PathBuf) -> io::Result<Vec<T>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut file = OpenOptions::new().read(true).open(&path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let mut items = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let item: T = serde_json::from_str(line)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            items.push(item);
        }
        Ok(items)
    }
    /// Append a session update to the updates.jsonl file, wrapping it in an envelope with timestamp.
    pub(super) async fn append_update_to_file(
        &self,
        path: PathBuf,
        update: &super::SessionUpdate,
        durability: AppendDurability,
    ) -> io::Result<()> {
        #[cfg(test)]
        if let Some(append_probe) = &self.update_append_probe {
            append_probe(durability)?;
        }
        let envelope = SessionUpdateEnvelope::from_update(update)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut line = serde_json::to_vec(&envelope)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        Self::append_jsonl_line_blocking(path, line, durability).await
    }
    async fn append_update_with_bookkeeping(
        &self,
        info: &Info,
        update: &super::SessionUpdate,
        durability: AppendDurability,
    ) -> Result<(), super::AppendUpdateError> {
        self.append_update_to_file(self.updates_file(info), update, durability)
            .await
            .map_err(super::AppendUpdateError::NotCommitted)?;
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                record_activity: true,
                messages: Some(super::summary_write::CounterOp::Increment(1)),
                ..Default::default()
            },
        )
        .await
        .map_err(super::AppendUpdateError::Committed)
    }
    /// Read session updates from an updates.jsonl file, handling both envelope and legacy formats.
    ///
    /// Uses direct string-to-typed deserialization (via `SessionUpdateEnvelope::from_str`)
    /// with a borrowing envelope and `&RawValue` to avoid intermediate `Value` allocation.
    ///
    /// Corruption-tolerant like [`Self::read_chat_history_sync`]: updates are
    /// display/replay data appended non-atomically, so a torn line (crashed or
    /// racing append) is skipped with a warning instead of failing the caller
    /// (session load, fork copy). The live replay path is already lenient;
    /// this keeps the fork path from bricking on the same corruption.
    fn read_updates_jsonl(&self, path: PathBuf) -> io::Result<Vec<super::SessionUpdate>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents = std::fs::read(&path)?;
        let mut skipped_lines: usize = 0;
        let mut updates = Vec::new();
        for line in contents.split(|b| *b == b'\n') {
            let line = line.trim_ascii();
            if line.is_empty() {
                continue;
            }
            let parsed = std::str::from_utf8(line)
                .map_err(|e| e.to_string())
                .and_then(|s| SessionUpdateEnvelope::from_str(s).map_err(|e| e.to_string()));
            match parsed {
                Ok(update) => updates.push(update),
                Err(error) => {
                    skipped_lines += 1;
                    if skipped_lines == 1 {
                        tracing::warn!(
                            error = % error, path = % path.display(),
                            "skipping unparseable updates.jsonl line (torn append?)"
                        );
                    }
                }
            }
        }
        if skipped_lines > 0 {
            tracing::warn!(
                skipped = skipped_lines, loaded = updates.len(), path = % path.display(),
                "skipped unparseable session update lines"
            );
        }
        Ok(updates)
    }
    /// Write summary to disk atomically (sync version for `spawn_blocking`).
    ///
    /// A plain `std::fs::write` truncates before writing, so a concurrent reader
    /// may see an empty file. Temp-file + rename avoids this.
    fn write_summary_sync(&self, info: &Info, summary: &Summary) -> io::Result<()> {
        let summary_path = self.summary_file(info);
        let bytes = serde_json::to_vec_pretty(summary)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic(&summary_path, &bytes)
    }
    fn read_summary_sync(&self, info: &Info) -> io::Result<Summary> {
        let path = self.summary_file(info);
        let bytes = std::fs::read(&path)?;
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("summary.json is empty (0 bytes): {}", path.display()),
            ));
        }
        serde_json::from_slice::<Summary>(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    fn read_optional_json_sync<T: serde::de::DeserializeOwned>(
        &self,
        path: &Path,
    ) -> io::Result<Option<T>> {
        if !path.exists() {
            return Ok(None);
        }
        match std::fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Ok(None),
            Ok(s) => match serde_json::from_str::<T>(&s) {
                Ok(v) => Ok(Some(v)),
                Err(e) => {
                    tracing::warn!(?e, "failed parsing json; returning None");
                    Ok(None)
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(?e, "failed reading json; returning None");
                }
                Ok(None)
            }
        }
    }
    /// Read chat history from JSONL file, handling both legacy ChatRequestMessage format
    /// (version 0) and new ConversationItem format (version >= 1).
    ///
    /// Uses line-by-line format detection with fallback to handle mixed-format files
    /// that can occur when continuing an old session with a newer binary.
    ///
    /// ## Corruption tolerance (torn / interleaved appends)
    ///
    /// Appends to `chat_history.jsonl` are not crash-atomic: a process kill
    /// mid-append (auto-update leader relaunch), `ENOSPC`, or two writers
    /// racing (a second persistence actor on reconnect) can leave a torn or
    /// merged line — the classic symptom is a serde error like
    /// ``expected `,` or `}` at line 1 column 571``. Failing the whole load on
    /// one bad line bricks the session forever ("Couldn't load session:
    /// FS_OTHER"), which is strictly worse than resuming without the damaged
    /// record. Unparseable / undecodable lines are therefore *skipped* with a
    /// warning, and the first time corruption is detected the raw file is
    /// preserved as `chat_history.jsonl.corrupt` next to the original — the
    /// post-load snapshot rewrite (`persist_chat_history_jsonl_sync`) scrubs
    /// the bad lines from the live file, so the quarantine copy is the only
    /// surviving evidence for debugging / manual recovery.
    ///
    /// Lines are split on raw `\n` bytes and parsed with `from_slice` so a
    /// write torn mid-UTF-8-codepoint poisons only its own line, not the
    /// whole-file `read_to_string`.
    ///
    /// ## Legacy reasoning reconstruction (in-memory upgrade)
    ///
    /// Older sessions stored reasoning either inline on the
    /// assistant (`AssistantItem.reasoning`) or, for early
    /// backend-search sessions, as `AssistantItem.raw_output: Vec<Value>`.
    /// Newer sessions don't have those fields on `AssistantItem` so serde
    /// would silently drop them. We pre-extract them via
    /// [`xai_grok_sampling_types::upgrade_legacy_reasoning`] and emit
    /// sibling `Reasoning` / `BackendToolCall` items *before* the
    /// corresponding assistant — matching the order
    /// `response_to_conversation_items` would produce. The file on disk
    /// is not rewritten; this is a load-time-only transform so resumed
    /// sessions get sibling-shape replay without any disk-write risk.
    /// Idempotent: newer sessions have no `reasoning` / `raw_output` /
    /// `reasoning_content` fields, so the upgrader produces no siblings.
    /// The upgrader runs only for lines that decode successfully, so a
    /// skipped corrupt line never emits orphaned siblings or pollutes the
    /// sibling-dedup set.
    fn read_chat_history_sync(
        &self,
        path: PathBuf,
        chat_format_version: u8,
    ) -> io::Result<Vec<ConversationItem>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents = std::fs::read(&path)?;
        let mut sibling_btc_ids_seen: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut upgraded_reasoning_count: usize = 0;
        let mut upgraded_btc_count: usize = 0;
        let mut skipped_lines: usize = 0;
        let mut first_skipped: Option<(usize, String)> = None;
        let mut skip_line = |line_no: usize, error: String| {
            skipped_lines += 1;
            if first_skipped.is_none() {
                first_skipped = Some((line_no, error));
            }
        };
        let mut items = Vec::new();
        for (line_idx, line) in contents.split(|b| *b == b'\n').enumerate() {
            let line = line.trim_ascii();
            if line.is_empty() {
                continue;
            }
            let raw: serde_json::Value = match serde_json::from_slice(line) {
                Ok(raw) => raw,
                Err(e) => {
                    skip_line(line_idx + 1, e.to_string());
                    continue;
                }
            };
            let item_result = if chat_format_version >= CHAT_FORMAT_VERSION {
                serde_json::from_value::<ConversationItem>(raw.clone()).or_else(|e| {
                    serde_json::from_value::<ChatRequestMessage>(raw.clone())
                        .map(ConversationItem::from)
                        .map_err(|_| e)
                })
            } else {
                serde_json::from_value::<ChatRequestMessage>(raw.clone())
                    .map(ConversationItem::from)
                    .or_else(|e| {
                        serde_json::from_value::<ConversationItem>(raw.clone()).map_err(|_| e)
                    })
            };
            let item = match item_result {
                Ok(item) => item,
                Err(e) => {
                    skip_line(line_idx + 1, e.to_string());
                    continue;
                }
            };
            let siblings =
                xai_grok_sampling_types::upgrade_legacy_reasoning(&raw, &mut sibling_btc_ids_seen);
            for sib in siblings {
                match &sib {
                    ConversationItem::Reasoning(_) => upgraded_reasoning_count += 1,
                    ConversationItem::BackendToolCall(_) => upgraded_btc_count += 1,
                    _ => {}
                }
                items.push(sib);
            }
            if let ConversationItem::BackendToolCall(b) = &item {
                sibling_btc_ids_seen.insert(b.id().to_string());
            }
            items.push(item);
        }
        let stripped = strip_invalid_images(&mut items);
        if first_skipped.is_some() || stripped > 0 {
            let quarantine = path.with_extension("jsonl.corrupt");
            if !quarantine.exists()
                && let Err(e) = std::fs::copy(&path, &quarantine)
            {
                tracing::warn!(
                    error = % e, path = % quarantine.display(),
                    "failed to write chat history quarantine copy"
                );
            }
        }
        if let Some((first_line, first_error)) = first_skipped {
            tracing::warn!(
                skipped = skipped_lines, loaded = items.len(), first_line, first_error =
                % first_error, path = % path.display(),
                "skipped unparseable chat history lines (torn or interleaved \
                 append — crashed mid-write or concurrent writer?); loading \
                 the session without them, original preserved as *.corrupt"
            );
        }
        if stripped > 0 {
            tracing::warn!(
                count = stripped, path = % path.display(),
                "stripped invalid images from loaded chat history, original \
                 preserved as *.corrupt"
            );
        }
        if upgraded_reasoning_count > 0 || upgraded_btc_count > 0 {
            tracing::info!(
                upgraded_reasoning = upgraded_reasoning_count,
                upgraded_backend_tool_calls = upgraded_btc_count,
                "reconstructed legacy reasoning siblings from pre-sibling-split session"
            );
        }
        Ok(items)
    }
    /// Apply a typed [`SummaryPatch`](super::summary_write::SummaryPatch) to
    /// this session's `summary.json` under an exclusive sidecar lock, so the
    /// read-modify-write serializes against every other writer (including a
    /// second persistence actor on reconnect, or another process). This is the
    /// only path live sessions use to mutate the summary.
    pub(crate) async fn apply_summary_patch(
        &self,
        info: &Info,
        patch: super::summary_write::SummaryPatch,
    ) -> io::Result<()> {
        self.apply_summary_patch_reporting(info, patch).await?;
        Ok(())
    }
    /// Like [`Self::apply_summary_patch`], but returns whether a
    /// `generated_title_if_absent` was applied (see [`Summary::apply_patch`]).
    async fn apply_summary_patch_reporting(
        &self,
        info: &Info,
        patch: super::summary_write::SummaryPatch,
    ) -> io::Result<bool> {
        let summary_path = self.summary_file(info);
        let lock_path = self.summary_lock_file(info);
        tokio::task::spawn_blocking(move || {
            super::summary_write::apply_patch_locked(&summary_path, &lock_path, &patch)
        })
        .await
        .map_err(io::Error::other)?
    }
}
/// Transform session ID in a SessionUpdate
fn transform_session_id_in_update(
    update: super::SessionUpdate,
    new_id: &acp::SessionId,
) -> super::SessionUpdate {
    match update {
        super::SessionUpdate::Acp(notification) => {
            let mut inner = (*notification).clone();
            inner.session_id = new_id.clone();
            super::SessionUpdate::Acp(Box::new(inner))
        }
        super::SessionUpdate::Xai(notification) => {
            let mut inner = (*notification).clone();
            inner.session_id = new_id.clone();
            super::SessionUpdate::Xai(Box::new(inner))
        }
    }
}
/// Apply fork-safety filtering to chat history before copying.
///
/// 1. Removes synthetic user messages (doom loop warnings, compaction metadata)
/// 2. Truncates at the last complete turn boundary. A complete turn runs
///    `User → Assistant → (matching ToolResults)`, possibly across multiple
///    Assistant/ToolResult cycles, with `Reasoning` siblings interleaved
///    throughout (real grok-build turns emit `[reasoning, assistant, tool
///    results, reasoning, assistant, ...]`). The scan treats everything
///    except `Assistant` as transparent and only advances the boundary when an
///    Assistant closes every tool call it made, so it survives reasoning
///    interleaving. Trailing incomplete turns — including a trailing
///    user/reasoning tail with no matching assistant response (e.g. the
///    in-flight `/goal` turn) — are removed so the child never sees an
///    incoherent partial turn.
///
/// Also used by the live parent-chat fork path (summarized fallback only — the
/// verbatim mirror path keeps items unfiltered to preserve cached synthetics).
///
/// NOTE: this is one of two reasoning-aware turn-boundary scanners that must move
/// together — the other is `count_complete_turns` in
/// `xai-grok-subagent-resolution/src/context.rs` (it counts turns in the same
/// filtered list during summarization). Keep their notions of a "complete turn"
/// in sync if the turn item model changes.
pub(crate) fn fork_filter_chat(items: &mut Vec<ConversationItem>) {
    items.retain(|item| match item {
        ConversationItem::User(u) => u.synthetic_reason.is_none(),
        _ => true,
    });
    let mut last_complete_end = 0;
    let mut i = 0;
    while i < items.len() {
        match &items[i] {
            ConversationItem::System(_) => {
                last_complete_end = i + 1;
                i += 1;
            }
            ConversationItem::Assistant(asst) => {
                let expected: std::collections::HashSet<&str> =
                    asst.tool_calls.iter().map(|tc| tc.id.as_ref()).collect();
                let mut found = std::collections::HashSet::new();
                let mut j = i + 1;
                while j < items.len() {
                    match &items[j] {
                        ConversationItem::ToolResult(tr) => {
                            if expected.contains(tr.tool_call_id.as_str()) {
                                found.insert(tr.tool_call_id.as_str());
                            }
                            j += 1;
                        }
                        ConversationItem::Reasoning(_) | ConversationItem::BackendToolCall(_) => {
                            j += 1;
                        }
                        _ => break,
                    }
                }
                if found == expected {
                    last_complete_end = j;
                    i = j;
                } else {
                    break;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    items.truncate(last_complete_end);
}
impl JsonlStorageAdapter {
    /// Fully synchronous version of `copy_session_data` for use inside
    /// `spawn_blocking`. Identical logic but uses `std::fs::write` instead
    /// of `tokio::fs::write`, so the entire copy runs on a blocking thread
    /// without nesting `spawn_blocking` calls.
    pub fn copy_session_data_sync(
        &self,
        source_info: &Info,
        target_info: &Info,
        options: super::CopySessionOptions,
    ) -> io::Result<super::CopySessionResult> {
        let target_dir = self.session_dir(target_info);
        std::fs::create_dir_all(&target_dir)?;
        let source_summary = self.read_summary_sync(source_info)?;
        let chat_format_version = source_summary.chat_format_version;
        let mut chat_to_copy: Vec<ConversationItem> =
            self.read_chat_history_sync(self.chat_file(source_info), chat_format_version)?;
        let mut updates_to_copy: Vec<super::SessionUpdate> =
            self.read_updates_jsonl(self.updates_file(source_info))?;
        if let Some(target_idx) = options.target_prompt_index {
            chat_to_copy.truncate(conversation_truncate_for_prompt(&chat_to_copy, target_idx));
            updates_to_copy.truncate(updates_truncate_for_prompt(&updates_to_copy, target_idx));
        }
        if options.fork_filter {
            fork_filter_chat(&mut chat_to_copy);
            updates_to_copy.clear();
        }
        let inherited_prefix_len = if options.fork_filter {
            Some(chat_to_copy.len())
        } else {
            options.inherited_prefix_len
        };
        if !options.skip_cwd_transform && source_info.cwd != target_info.cwd {
            transform_conversation_cwd(&mut chat_to_copy, &source_info.cwd, &target_info.cwd);
        }
        if options.strip_reasoning {
            chat_to_copy = xai_chat_state::compaction_utils::strip_reasoning_blocks(chat_to_copy);
        }
        let num_chat_messages = chat_to_copy.len();
        let num_messages = updates_to_copy.len();
        let target_model_id = options
            .new_model_id
            .map(acp::ModelId::new)
            .unwrap_or(source_summary.current_model_id);
        let target_summary = crate::session::persistence::Summary {
            info: target_info.clone(),
            session_summary: source_summary.session_summary,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            num_messages,
            num_chat_messages,
            current_model_id: target_model_id,
            parent_session_id: options.parent_session_id,
            forked_at: Some(chrono::Utc::now()),
            collection_id: None,
            next_trace_turn: 0,
            chat_format_version: CHAT_FORMAT_VERSION,
            prompt_display_cwd: options.prompt_display_cwd,
            session_kind: Some(options.session_kind.unwrap_or_else(|| "fork".to_string())),
            fork_context_source: options.fork_context_source,
            fork_parent_prompt_id: options.fork_parent_prompt_id,
            inherited_prefix_len,
            hidden: None,
            source_workspace_dir: options.source_workspace_dir,
            git_root_dir: None,
            git_remotes: Vec::new(),
            head_commit: source_summary.head_commit,
            head_branch: source_summary.head_branch,
            request_id: None,
            grok_home: crate::session::persistence::grok_home_string(),
            last_active_at: source_summary.last_active_at,
            generated_title: source_summary.generated_title,
            title_is_manual: source_summary.title_is_manual,
            worktree_label: source_summary.worktree_label,
            agent_name: source_summary.agent_name,
            sandbox_profile: source_summary.sandbox_profile,
            reasoning_effort: source_summary.reasoning_effort,
        };
        let summary_bytes = serde_json::to_vec_pretty(&target_summary)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(self.summary_file(target_info), summary_bytes)?;
        let mut chat_content = Vec::new();
        for item in &chat_to_copy {
            let mut line = serde_json::to_vec(item)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            line.push(b'\n');
            chat_content.extend(line);
        }
        std::fs::write(self.chat_file(target_info), chat_content)?;
        let transformed_updates: Vec<super::SessionUpdate> = updates_to_copy
            .into_iter()
            .map(|u| transform_session_id_in_update(u, &target_info.id))
            .collect();
        let mut updates_content = Vec::new();
        for update in &transformed_updates {
            let envelope = SessionUpdateEnvelope::from_update(update)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let mut line = serde_json::to_vec(&envelope)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            line.push(b'\n');
            updates_content.extend(line);
        }
        std::fs::write(self.updates_file(target_info), updates_content)?;
        let plan_copied = if options.copy_plan_state {
            let plan_path = self.plan_file(source_info);
            if plan_path.exists() {
                std::fs::write(self.plan_file(target_info), std::fs::read(&plan_path)?)?;
                true
            } else {
                false
            }
        } else {
            false
        };
        let signals_copied = if options.copy_signals {
            let signals_path = self.signals_file(source_info);
            if signals_path.exists() {
                std::fs::write(
                    self.signals_file(target_info),
                    std::fs::read(&signals_path)?,
                )?;
                true
            } else {
                false
            }
        } else {
            false
        };
        let plan_mode_state_copied = if options.copy_plan_mode_state {
            let plan_mode_path = self.plan_mode_state_file(source_info);
            if plan_mode_path.exists() {
                std::fs::write(
                    self.plan_mode_state_file(target_info),
                    std::fs::read(&plan_mode_path)?,
                )?;
                true
            } else {
                false
            }
        } else {
            false
        };
        let tool_state_copied = if options.copy_tool_state {
            let tool_state_path = self.session_dir(source_info).join("tool_state.json");
            if tool_state_path.is_file() {
                std::fs::write(
                    self.session_dir(target_info).join("tool_state.json"),
                    std::fs::read(&tool_state_path)?,
                )?;
                true
            } else {
                if tool_state_path.is_dir() {
                    tracing::warn!(
                        ? tool_state_path, session_id = % source_info.id,
                        "tool_state.json is a directory (not a file); skipping copy",
                    );
                }
                false
            }
        } else {
            false
        };
        let announcement_state_copied = if options.copy_announcement_state {
            let ann_path = self.announcement_state_file(source_info);
            if ann_path.exists() {
                std::fs::write(
                    self.announcement_state_file(target_info),
                    std::fs::read(&ann_path)?,
                )?;
                true
            } else {
                false
            }
        } else {
            false
        };
        let compaction_segments_copied = if options.copy_compaction_segments {
            let src_dir = self
                .session_dir(source_info)
                .join(xai_chat_state::compaction_transcript::COMPACTION_DIR);
            let mut copied = 0usize;
            if src_dir.is_dir() {
                let dst_dir = self
                    .session_dir(target_info)
                    .join(xai_chat_state::compaction_transcript::COMPACTION_DIR);
                std::fs::create_dir_all(&dst_dir)?;
                for entry in std::fs::read_dir(&src_dir)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        std::fs::copy(entry.path(), dst_dir.join(entry.file_name()))?;
                        copied += 1;
                    }
                }
            }
            copied
        } else {
            0
        };
        Ok(super::CopySessionResult {
            chat_messages_copied: num_chat_messages,
            updates_copied: num_messages,
            plan_state_copied: plan_copied,
            plan_mode_state_copied,
            signals_copied,
            tool_state_copied,
            announcement_state_copied,
            compaction_segments_copied,
        })
    }
}
/// Next `segment_NNN` index in `compaction_dir`: one past the highest existing
/// segment, or 0 when none exist. Resume-safe — derived from disk, not memory.
async fn next_compaction_segment_index(compaction_dir: &std::path::Path) -> u64 {
    let Ok(mut entries) = tokio::fs::read_dir(compaction_dir).await else {
        return 0;
    };
    let mut next = 0u64;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(n) = entry
            .file_name()
            .to_str()
            .and_then(xai_chat_state::compaction_transcript::parse_segment_index)
        {
            next = next.max(n + 1);
        }
    }
    next
}
#[async_trait]
impl StorageAdapter for JsonlStorageAdapter {
    async fn init_session(&self, info: &Info, model_id: acp::ModelId) -> io::Result<Summary> {
        let dir = self.session_dir(info);
        std::fs::create_dir_all(&dir)?;
        let summary_path = self.summary_file(info);
        if Path::new(&summary_path).exists() {
            tracing::info!("Loading existing session from JSONL");
            let bytes = tokio::fs::read(&summary_path).await?;
            serde_json::from_slice::<Summary>(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        } else {
            tracing::info!("Creating new session in JSONL");
            let mut summary = Summary::new(info, model_id)?;
            summary.sandbox_profile = xai_grok_sandbox::configured_profile_name().map(String::from);
            self.write_summary_sync(info, &summary)?;
            Ok(summary)
        }
    }
    async fn update_session_title(&self, info: &Info, session_title: String) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                generated_title: Some(session_title),
                ..Default::default()
            },
        )
        .await
    }
    async fn set_generated_title_if_absent(
        &self,
        info: &Info,
        session_title: String,
    ) -> io::Result<bool> {
        self.apply_summary_patch_reporting(
            info,
            super::summary_write::SummaryPatch {
                generated_title_if_absent: Some(session_title),
                ..Default::default()
            },
        )
        .await
    }
    async fn append_update(&self, info: &Info, update: &super::SessionUpdate) -> io::Result<()> {
        self.append_update_commit_aware(info, update)
            .await
            .map_err(super::AppendUpdateError::into_io_error)
    }
    async fn append_update_commit_aware(
        &self,
        info: &Info,
        update: &super::SessionUpdate,
    ) -> Result<(), super::AppendUpdateError> {
        self.append_update_with_bookkeeping(info, update, AppendDurability::Buffered)
            .await
    }
    async fn append_update_durable_commit_aware(
        &self,
        info: &Info,
        update: &super::SessionUpdate,
    ) -> Result<(), super::AppendUpdateError> {
        self.append_update_with_bookkeeping(info, update, AppendDurability::Durable)
            .await
    }
    async fn append_chat_message(&self, info: &Info, message: &ConversationItem) -> io::Result<()> {
        self.append_jsonl(self.chat_file(info), message).await?;
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                record_activity: true,
                chat_messages: Some(super::summary_write::CounterOp::Increment(1)),
                chat_format_version: Some(CHAT_FORMAT_VERSION),
                ..Default::default()
            },
        )
        .await
    }
    async fn update_current_model_and_agent(
        &self,
        info: &Info,
        model_id: &acp::ModelId,
        agent_name: Option<&str>,
        reasoning_effort: Option<Option<xai_grok_sampling_types::ReasoningEffort>>,
    ) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                model: Some(super::summary_write::ModelPatch {
                    model_id: model_id.clone(),
                    agent_name: agent_name.map(String::from),
                    reasoning_effort,
                }),
                ..Default::default()
            },
        )
        .await
    }
    async fn update_collection_id(&self, info: &Info, collection_id: &str) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                collection_id: Some(collection_id.to_string()),
                ..Default::default()
            },
        )
        .await
    }
    async fn update_git_head(
        &self,
        info: &Info,
        commit: Option<String>,
        branch: Option<String>,
    ) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                git_head: Some(super::summary_write::GitHeadPatch { commit, branch }),
                ..Default::default()
            },
        )
        .await
    }
    async fn update_next_trace_turn(
        &self,
        info: &Info,
        next_trace_turn: u64,
        request_id: Option<&str>,
    ) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                trace_turn: Some(super::summary_write::TraceTurnPatch {
                    next_trace_turn,
                    request_id: request_id.map(String::from),
                }),
                ..Default::default()
            },
        )
        .await
    }
    async fn write_plan_state(&self, info: &Info, state: &TodoState) -> io::Result<()> {
        let state_json = serde_json::to_vec_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(self.plan_file(info), state_json).await
    }
    async fn write_plan_mode_state(
        &self,
        info: &Info,
        state: &crate::session::plan_mode::PlanModeSnapshot,
    ) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic_async(&self.plan_mode_state_file(info), json).await
    }
    async fn write_signals(
        &self,
        info: &Info,
        signals: &crate::session::signals::SessionSignals,
    ) -> io::Result<()> {
        let signals_json = serde_json::to_vec(signals)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic_async(&self.signals_file(info), signals_json).await
    }
    async fn write_announcement_state(
        &self,
        info: &Info,
        state: &crate::session::announcement_state::AnnouncementState,
    ) -> io::Result<()> {
        let json =
            serde_json::to_vec(state).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic_async(&self.announcement_state_file(info), json).await
    }
    async fn write_goal_mode_state(
        &self,
        info: &Info,
        state: &crate::session::goal_tracker::GoalOrchestration,
    ) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let target = self.goal_mode_state_file(info);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        super::write_bytes_atomic_async(&target, json).await
    }
    async fn load_session(&self, info: &Info) -> io::Result<PersistedData> {
        let summary = self.read_summary_sync(info)?;
        let chat_file = self.chat_file(info);
        self.ensure_chat_history(info, summary.chat_format_version)?;
        let chat_history = self.read_chat_history_sync(chat_file, summary.chat_format_version)?;
        let updates = self.read_updates_jsonl(self.updates_file(info))?;
        let plan_state = self.read_optional_json_sync::<TodoState>(&self.plan_file(info))?;
        let plan_mode_state = self
            .read_optional_json_sync::<crate::session::plan_mode::PlanModeSnapshot>(
                &self.plan_mode_state_file(info),
            )?;
        let signals = self.read_optional_json_sync::<crate::session::signals::SessionSignals>(
            &self.signals_file(info),
        )?;
        let announcement_state = self
            .read_optional_json_sync::<crate::session::announcement_state::AnnouncementState>(
                &self.announcement_state_file(info),
            )?;
        let goal_mode_state = self
            .read_optional_json_sync::<crate::session::goal_tracker::GoalOrchestration>(
                &self.goal_mode_state_file(info),
            )?;
        let rewind_points = self.read_jsonl::<RewindPoint>(self.rewind_points_file(info))?;
        let result = PersistedData {
            summary,
            chat_history,
            updates,
            plan_state,
            plan_mode_state,
            rewind_points,
            signals,
            announcement_state,
            goal_mode_state,
        };
        tracing::info!(
            session_id = % info.id, num_chat_messages = result.chat_history.len(),
            num_updates = result.updates.len(), has_plan = result.plan_state.is_some(),
            has_signals = result.signals.is_some(), num_rewind_points = result
            .rewind_points.len(), chat_format_version = result.summary
            .chat_format_version, "Session data loaded successfully from JSONL"
        );
        Ok(result)
    }
    /// Resume path: loads everything except updates and rewind points. Rewind
    /// points can be huge (full file-content snapshots) and are needed only on an
    /// actual rewind, so they're deferred — loaded lazily by `FileStateTracker`.
    async fn load_session_without_updates(
        &self,
        info: &Info,
    ) -> io::Result<super::PersistedDataLight> {
        tracing::info!("Loading session data (without updates) from JSONL");
        let summary = self.read_summary_sync(info)?;
        let chat_file = self.chat_file(info);
        self.ensure_chat_history(info, summary.chat_format_version)?;
        let chat_history = self.read_chat_history_sync(chat_file, summary.chat_format_version)?;
        let plan_state = self.read_optional_json_sync::<TodoState>(&self.plan_file(info))?;
        let plan_mode_state = self
            .read_optional_json_sync::<crate::session::plan_mode::PlanModeSnapshot>(
                &self.plan_mode_state_file(info),
            )?;
        let signals = self.read_optional_json_sync::<crate::session::signals::SessionSignals>(
            &self.signals_file(info),
        )?;
        let announcement_state = self
            .read_optional_json_sync::<crate::session::announcement_state::AnnouncementState>(
                &self.announcement_state_file(info),
            )?;
        let goal_mode_state = self
            .read_optional_json_sync::<crate::session::goal_tracker::GoalOrchestration>(
                &self.goal_mode_state_file(info),
            )?;
        let result = super::PersistedDataLight {
            summary,
            chat_history,
            plan_state,
            plan_mode_state,
            signals,
            announcement_state,
            goal_mode_state,
        };
        tracing::info!(
            session_id = % info.id, num_chat_messages = result.chat_history.len(),
            has_plan = result.plan_state.is_some(), has_signals = result.signals
            .is_some(), chat_format_version = result.summary.chat_format_version,
            "Session data loaded (without updates, rewind points deferred) from JSONL"
        );
        Ok(result)
    }
    async fn load_summary(&self, info: &Info) -> io::Result<Summary> {
        let info_clone = info.clone();
        let summary_handle = {
            let info = info_clone.clone();
            let adapter_clone = self.clone();
            tokio::task::spawn_blocking(move || {
                let adapter = adapter_clone;
                adapter.read_summary_sync(&info)
            })
        };
        let summary = summary_handle.await.map_err(io::Error::other)??;
        Ok(summary)
    }
    async fn list_sessions(&self, cwd: Option<&str>) -> io::Result<Vec<Summary>> {
        let adapter = self.clone();
        let cwd = cwd.map(str::to_owned);
        tokio::task::spawn_blocking(move || adapter.list_sessions_sync(cwd.as_deref()))
            .await
            .map_err(io::Error::other)?
    }
    async fn delete_session(&self, info: &Info) -> io::Result<()> {
        let dir = self.session_dir(info);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
    async fn append_rewind_point(&self, info: &Info, point: &RewindPoint) -> io::Result<()> {
        self.append_jsonl(self.rewind_points_file(info), point)
            .await
    }
    async fn load_rewind_points(&self, info: &Info) -> io::Result<Vec<RewindPoint>> {
        let info_clone = info.clone();
        let adapter_clone = self.clone();
        tokio::task::spawn_blocking(move || {
            let adapter = adapter_clone;
            let path = adapter.rewind_points_file(&info_clone);
            adapter.read_jsonl::<RewindPoint>(path)
        })
        .await
        .map_err(io::Error::other)?
    }
    async fn truncate_rewind_points_from(&self, info: &Info, from_index: usize) -> io::Result<()> {
        let points = self.load_rewind_points(info).await?;
        let filtered: Vec<RewindPoint> = points
            .into_iter()
            .filter(|p| p.prompt_index < from_index)
            .collect();
        self.write_jsonl(self.rewind_points_file(info), &filtered)
            .await
    }
    async fn merge_rewind_points_from(&self, info: &Info, target_index: usize) -> io::Result<()> {
        let points = self.load_rewind_points(info).await?;
        let merged =
            xai_grok_workspace::session::file_state::merge_rewind_points_from(points, target_index);
        self.write_jsonl(self.rewind_points_file(info), &merged)
            .await
    }
    async fn sync_session_files(&self, info: &Info) -> io::Result<()> {
        let info_clone = info.clone();
        let adapter_clone = self.clone();
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            use std::fs::OpenOptions;
            let adapter = adapter_clone;
            let files_to_sync = [
                adapter.updates_file(&info_clone),
                adapter.chat_file(&info_clone),
                adapter.summary_file(&info_clone),
                adapter.plan_file(&info_clone),
                adapter.rewind_points_file(&info_clone),
            ];
            for file_path in &files_to_sync {
                if file_path.exists()
                    && let Ok(file) = OpenOptions::new().write(true).open(file_path)
                {
                    let _ = file.sync_all();
                }
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }
    async fn replace_chat_history(
        &self,
        info: &Info,
        messages: &[ConversationItem],
    ) -> io::Result<()> {
        self.write_jsonl(self.chat_file(info), messages).await?;
        let new_count = messages.len();
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                chat_messages: Some(super::summary_write::CounterOp::Set(new_count)),
                chat_format_version: Some(CHAT_FORMAT_VERSION),
                ..Default::default()
            },
        )
        .await
    }
    async fn copy_session_data(
        &self,
        source_info: &Info,
        target_info: &Info,
        options: super::CopySessionOptions,
    ) -> io::Result<super::CopySessionResult> {
        let storage = self.clone();
        let source = source_info.clone();
        let target = target_info.clone();
        tokio::task::spawn_blocking(move || {
            storage.copy_session_data_sync(&source, &target, options)
        })
        .await
        .map_err(|e| io::Error::other(format!("spawn_blocking panicked: {e}")))?
    }
    async fn load_prompts_only(&self, info: &Info) -> io::Result<Vec<String>> {
        let updates_path = self.updates_file(info);
        if !updates_path.exists() {
            return Ok(Vec::new());
        }
        tokio::task::spawn_blocking(move || {
            let Some(iter) = super::PromptExtractIterator::open(&updates_path)? else {
                return Ok(Vec::new());
            };
            Ok(super::collect_prompts_from_events(iter))
        })
        .await
        .map_err(io::Error::other)?
    }
    #[tracing::instrument(skip_all, fields(session_id = %info.id))]
    async fn load_assistant_text(&self, info: &Info) -> io::Result<Vec<String>> {
        let updates_path = self.updates_file(info);
        if !updates_path.exists() {
            return Ok(Vec::new());
        }
        tokio::task::spawn_blocking(move || {
            let Some(iter) = super::UpdatesIterator::open(&updates_path)? else {
                return Ok(Vec::new());
            };
            Ok(super::collect_assistant_text(iter))
        })
        .await
        .map_err(io::Error::other)?
    }
    #[tracing::instrument(skip_all, fields(session_id = %info.id))]
    async fn load_tool_metadata(&self, info: &Info) -> io::Result<Vec<String>> {
        let updates_path = self.updates_file(info);
        if !updates_path.exists() {
            return Ok(Vec::new());
        }
        tokio::task::spawn_blocking(move || {
            let Some(iter) = super::UpdatesIterator::open(&updates_path)? else {
                return Ok(Vec::new());
            };
            Ok(super::collect_tool_metadata(iter))
        })
        .await
        .map_err(io::Error::other)?
    }
    fn updates_file_path(&self, info: &Info) -> Option<std::path::PathBuf> {
        Some(self.updates_file(info))
    }
    fn rewind_points_file_path(&self, info: &Info) -> Option<std::path::PathBuf> {
        Some(self.rewind_points_file(info))
    }
    async fn append_feedback(
        &self,
        info: &Info,
        entry: &crate::session::persistence::LocalFeedbackEntry,
    ) -> io::Result<()> {
        let path = self.feedback_file(info);
        self.append_jsonl(path, entry).await
    }
    async fn append_btw(
        &self,
        info: &Info,
        entry: &crate::session::persistence::BtwEntry,
    ) -> io::Result<()> {
        let path = self.btw_history_file(info);
        self.append_jsonl(path, entry).await
    }
    async fn write_compaction_checkpoint(
        &self,
        info: &Info,
        checkpoint: &crate::extensions::notification::CompactionCheckpointFile,
    ) -> io::Result<()> {
        let dir = self.session_dir(info).join("compaction_checkpoints");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.json", checkpoint.checkpoint_id));
        let bytes = serde_json::to_vec_pretty(checkpoint)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(path, bytes).await
    }
    async fn write_compaction_request(
        &self,
        info: &Info,
        request: &crate::extensions::notification::CompactionRequestFile,
    ) -> io::Result<()> {
        let dir = self.session_dir(info).join("compaction_requests");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.json", request.request_id));
        let bytes = serde_json::to_vec_pretty(request)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(path, bytes).await
    }
    async fn write_recap_request(
        &self,
        info: &Info,
        request: &crate::extensions::notification::RecapRequestFile,
    ) -> io::Result<()> {
        let dir = self.session_dir(info).join("recap_requests");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.json", request.request_id));
        let bytes = serde_json::to_vec_pretty(request)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(path, bytes).await
    }
    async fn write_compaction_segment(
        &self,
        info: &Info,
        segment: &crate::extensions::notification::CompactionSegmentFile,
    ) -> io::Result<()> {
        use tokio::io::AsyncWriteExt;
        use xai_chat_state::compaction_transcript::{
            COMPACTION_DIR, INDEX_FILE, INDEX_HEADER, extract_keywords, render_index_row,
            render_segment_md, segment_filename,
        };
        let base = self.session_dir(info).join(COMPACTION_DIR);
        tokio::fs::create_dir_all(&base).await?;
        let index = next_compaction_segment_index(&base).await;
        let md = render_segment_md(
            &segment.items,
            &segment.summary,
            index,
            segment.detail,
            &segment.timestamp,
        );
        tokio::fs::write(base.join(segment_filename(index)), md.as_bytes()).await?;
        let index_path = base.join(INDEX_FILE);
        let needs_header = !tokio::fs::try_exists(&index_path).await.unwrap_or(false);
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)
            .await?;
        if needs_header {
            f.write_all(INDEX_HEADER.as_bytes()).await?;
        }
        let keywords = extract_keywords(&segment.summary);
        let row = render_index_row(index, segment.items.len(), md.len(), &keywords);
        f.write_all(row.as_bytes()).await?;
        f.flush().await?;
        Ok(())
    }
    async fn read_compaction_checkpoint(
        &self,
        info: &Info,
        checkpoint_file: &str,
    ) -> io::Result<crate::extensions::notification::CompactionCheckpointFile> {
        let path = self.session_dir(info).join(checkpoint_file);
        let bytes = tokio::fs::read(&path).await?;
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}
/// Max decoded size for a data-URI image loaded from persisted history.
/// Generous (20 MB) — fresh images use 5 MB, but loaded ones just need sanity-checking.
const MAX_LOADED_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Strip data-URI images the API would reject (see
/// [`persisted_image_reject_reason`](crate::session::image_normalize::persisted_image_reject_reason):
/// malformed/oversized payloads, truncated or API-rejected formats,
/// dimensions outside the floors/ceiling) from loaded conversation items,
/// so a poisoned history recovers instead of 400ing on every turn.
/// User parts become a text placeholder; `ToolResultItem.images` entries
/// are removed. HTTP(S) URLs are left untouched.
///
/// Returns the number of images stripped.
pub(crate) fn strip_invalid_images(items: &mut [ConversationItem]) -> usize {
    fn invalid(part: &ContentPart) -> bool {
        match part {
            ContentPart::Image { url } => url.starts_with("data:") && !is_valid_data_uri_image(url),
            _ => false,
        }
    }
    let mut stripped = 0usize;
    for item in items.iter_mut() {
        match item {
            ConversationItem::User(user) => {
                for part in user.content.iter_mut() {
                    if invalid(part) {
                        *part = ContentPart::Text {
                            text: std::sync::Arc::<str>::from(
                                "[image removed \u{2014} invalid data]",
                            ),
                        };
                        stripped += 1;
                    }
                }
            }
            ConversationItem::ToolResult(t) => {
                let before = t.images.len();
                t.images.retain(|part| !invalid(part));
                stripped += before - t.images.len();
            }
            _ => {}
        }
    }
    stripped
}
/// Check that a `data:` URI has a valid `;base64,` header and decodable payload
/// within the size limit.
fn is_valid_data_uri_image(url: &str) -> bool {
    use base64::Engine as _;
    let after_data = match url.strip_prefix("data:") {
        Some(s) => s,
        None => return false,
    };
    let comma = match after_data.find(',') {
        Some(i) => i,
        None => return false,
    };
    let header = &after_data[..comma];
    let payload = &after_data[comma + 1..];
    if !header
        .as_bytes()
        .windows(7)
        .any(|w| w.eq_ignore_ascii_case(b";base64"))
    {
        return false;
    }
    if payload.len() * 3 / 4 > MAX_LOADED_IMAGE_BYTES {
        return false;
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(payload) else {
        return false;
    };
    match crate::session::image_normalize::persisted_image_reject_reason(&bytes) {
        None => true,
        Some(reason) => {
            tracing::warn!(reason, "stripping unsendable image from loaded history");
            false
        }
    }
}
#[cfg(test)]
mod durable_tests;
#[cfg(test)]
mod tests;
