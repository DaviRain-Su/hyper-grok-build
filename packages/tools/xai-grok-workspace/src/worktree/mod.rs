//! Git worktree operations: create, list, remove, apply.
//!
//! Moved from `xai-grok-shell/src/session/worktree.rs` into the workspace
//! crate so that the remote workspace-server can drive worktree lifecycle
//! without pulling in the full shell.
//!
//! Session-aware operations (`resume_session_in_worktree`,
//! `rehydrate_session_in_worktree`, `resolve_session_repo_wide`) remain in
//! the shell because they orchestrate session lifecycle (persistence, auth,
//! registry) which are client-side concerns. They call workspace ops for
//! the worktree/git parts.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use git2::{DiffOptions, Oid, Repository};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;
use xai_fast_worktree::{BtrfsDelegate, IgnoredFilesMode, WorkingTreeMode, WorktreeBuilder};

use crate::session::git::{
    GitFileChange, change_type_from_git2_delta, find_git_root_from_path,
    find_main_repo_root_from_path, git_cli,
};

// Canonical in xai-grok-workspace-types; re-exported for existing paths.
pub use xai_grok_workspace_types::rpc::worktree::{
    ApplyMode, ApplyWorktreeRequest, ApplyWorktreeResponse, CopiedChangesSummary,
    CreateWorktreeFromWorktreeRequestWire, CreateWorktreeFromWorktreeResponse,
    CreateWorktreeRequest, CreateWorktreeResponse, DirtyStateSummary, FileConflict,
    RemoveWorktreeRequest, RemoveWorktreeResponse, WorktreeCopyMode, WorktreeType,
};

const WORKTREE_LOG: &str = "xai_worktree";

/// Map a [`WorktreeType`] to the fast-worktree crate's `CreationMode`.
pub(crate) fn to_creation_mode(t: WorktreeType) -> xai_fast_worktree::CreationMode {
    match t {
        WorktreeType::Linked => xai_fast_worktree::CreationMode::Linked,
        WorktreeType::Standalone => xai_fast_worktree::CreationMode::Standalone,
        WorktreeType::Git => xai_fast_worktree::CreationMode::GitCheckout,
    }
}

// ============================================================================
// Btrfs delegate factory -- injected by binaries that link a concrete
// snapshot helper delegate
// ============================================================================

/// Process-global factory producing the btrfs delegate, if any.
///
/// The concrete delegate (a privileged snapshot helper used on hosts without
/// `CAP_SYS_ADMIN`) lives in a separate crate so this crate carries no extra
/// proto dependency. Binaries that want the delegate register a factory at
/// startup via [`set_btrfs_delegate_factory`]; binaries that never register one
/// use direct btrfs and, failing that, fall through to the copy path.
type BtrfsDelegateFactory = Box<dyn Fn() -> Option<Arc<dyn BtrfsDelegate>> + Send + Sync>;

static BTRFS_DELEGATE_FACTORY: OnceLock<BtrfsDelegateFactory> = OnceLock::new();

/// Register the process-global btrfs delegate factory.
///
/// Call once at startup, before any worktree operation. Subsequent calls are
/// ignored (first registration wins) with a warning.
pub fn set_btrfs_delegate_factory(factory: BtrfsDelegateFactory) {
    if BTRFS_DELEGATE_FACTORY.set(factory).is_err() {
        tracing::warn!("btrfs delegate factory already registered; ignoring");
    }
}

/// Build an `Arc<dyn BtrfsDelegate>` from the registered factory.
///
/// Returns `Some` on rootless hosts (no `CAP_SYS_ADMIN`) when a factory is
/// registered, `None` otherwise. The name predates the factory indirection:
/// the concrete factory still performs env-based detection (capabilities,
/// helper endpoint, timeouts) on every call.
pub fn btrfs_delegate_from_env() -> Option<Arc<dyn BtrfsDelegate>> {
    BTRFS_DELEGATE_FACTORY.get().and_then(|f| f())
}

fn get_head_commit(repo: &Repository) -> Result<String> {
    let head = repo.head()?;
    let commit = head.peel_to_commit()?;
    Ok(commit.id().to_string())
}

// ============================================================================
// In-progress tracking
// ============================================================================

// Process-local, best-effort dedup of duplicate async spawns within one process —
// NOT a cross-process lock: in proxy mode `prepare` (hub) and creation (shell) are
// different processes, so correctness does not depend on it.
static WORKTREE_IN_PROGRESS: OnceLock<TokioMutex<HashSet<String>>> = OnceLock::new();

fn worktree_registry() -> &'static TokioMutex<HashSet<String>> {
    WORKTREE_IN_PROGRESS.get_or_init(|| TokioMutex::new(HashSet::new()))
}

pub async fn is_worktree_in_progress(session_id: &str) -> bool {
    worktree_registry().lock().await.contains(session_id)
}

/// Atomically claim `session_id` for an in-flight creation: returns `true` if
/// the caller won the claim (no prior owner) and `false` if a creation is
/// already in progress. `prepare_*` deliberately only *reads* the marker (a
/// marker set in prepare would never clear in proxy mode, wedging retries), so a
/// `prepare` race can spawn two async creators for one session; doing
/// contains+insert under one lock here lets the loser bail, leaving one creator.
pub async fn claim_worktree_in_progress(session_id: &str) -> bool {
    worktree_registry()
        .lock()
        .await
        .insert(session_id.to_string())
}

pub async fn mark_worktree_complete(session_id: &str) {
    worktree_registry().lock().await.remove(session_id);
}

// ============================================================================
// Background Copy Infrastructure
// ============================================================================

/// Default parallelism config for background tasks.
/// This will leave some cores free in case foreground tasks are handled.
pub const DEFAULT_BG_PARALLELISM: usize = 2;

/// Tracks a background ignored file copy task for cancellation.
struct BackgroundCopyTask {
    /// Cancellation token for async cancellation via tokio::select!
    /// Also used by the sync copy engine via is_cancelled()
    cancellation_token: CancellationToken,
}

/// Context for managing background copy operations.
/// Stores active copy tasks and allows cancellation when worktrees are removed.
/// Using `Arc<Mutex>` to support spawning tasks across threads.
#[derive(Default, Clone)]
pub struct BackgroundCopyContext {
    tasks: Arc<Mutex<HashMap<String, BackgroundCopyTask>>>,
}

impl BackgroundCopyContext {
    /// Create a new empty context.
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a background copy task for a worktree.
    fn register(&self, worktree_path: String, cancellation_token: CancellationToken) {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(worktree_path, BackgroundCopyTask { cancellation_token });
    }

    /// Unregister a background copy task.
    fn unregister(&self, worktree_path: &str) {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(worktree_path);
    }

    /// Cancel a background copy task.
    /// Returns true if a task was cancelled, false if no task was running.
    pub fn cancel(&self, worktree_path: &str) -> bool {
        let task = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(worktree_path);

        if let Some(task) = task {
            // Cancel the token -- this triggers both:
            // 1. tokio::select! cancellation branch (async)
            // 2. The sync copy engine via is_cancelled() check
            task.cancellation_token.cancel();
            true
        } else {
            false
        }
    }
}

/// RAII guard that registers a background copy task on creation and unregisters on drop.
/// This ensures proper cleanup even if the task panics or exits early.
pub struct BackgroundCopyGuard {
    worktree_path: String,
    context: BackgroundCopyContext,
}

impl BackgroundCopyGuard {
    /// Create a new guard and register the background copy task.
    pub fn new(
        context: BackgroundCopyContext,
        worktree_path: String,
        cancellation_token: CancellationToken,
    ) -> Self {
        context.register(worktree_path.clone(), cancellation_token);
        Self {
            worktree_path,
            context,
        }
    }
}

impl Drop for BackgroundCopyGuard {
    fn drop(&mut self) {
        self.context.unregister(&self.worktree_path);
    }
}

/// Run background ignored file copy task.
pub async fn run_background_ignored_copy<N: WorktreeNotificationSender>(
    context: BackgroundCopyContext,
    session_id: String,
    source_path: String,
    worktree_path: String,
    skip_patterns: Vec<String>,
    notifier: N,
) {
    let cancellation_token = CancellationToken::new();

    notifier
        .send_worktree_status(WorktreeStatus::CopyingIgnored {
            session_id: session_id.clone(),
            worktree_path: worktree_path.clone(),
            message: "Copying ignored files in background...".to_string(),
        })
        .await;

    let token_for_copy = cancellation_token.clone();
    let source = source_path.clone();
    let dest = worktree_path.clone();
    let patterns = skip_patterns.clone();

    // Run the copy in a blocking task (copy_ignored_only does blocking I/O)
    let copy_handle = tokio::task::spawn_blocking(move || {
        // Build and run the copy with cancellation support
        // The token's is_cancelled() method is used by the sync copy engine
        let builder = WorktreeBuilder::new(&source, &dest)
            .ignored_files_mode(IgnoredFilesMode::CopyOnly {
                skip_patterns: patterns,
            })
            .parallelism(DEFAULT_BG_PARALLELISM)
            .cancellation_token(token_for_copy.clone());

        let result = builder.copy_ignored_only();

        (result, token_for_copy.is_cancelled())
    });

    // Get abort handle before moving copy_handle into select!
    let abort_handle = copy_handle.abort_handle();

    // Use tokio::select! to race the copy against cancellation
    let copy_result = {
        // Register the task using the guard pattern -- automatically unregisters on drop
        let _guard =
            BackgroundCopyGuard::new(context, worktree_path.clone(), cancellation_token.clone());

        tokio::select! {
            biased;

            // Cancellation branch -- wins immediately when token is cancelled
            // The sync copy engine will also see this via is_cancelled()
            _ = cancellation_token.cancelled() => {
                // Abort the blocking task
                abort_handle.abort();
                // Return a cancelled result
                None
            }

            // Normal completion branch
            result = copy_handle => Some(result)
        }
    };

    // Send completion notification
    match copy_result {
        Some(Ok((Ok(report), was_cancelled))) => {
            if was_cancelled {
                notifier
                    .send_worktree_status(WorktreeStatus::IgnoredCopyError {
                        session_id,
                        worktree_path,
                        message: "Background copy was cancelled".to_string(),
                        cancelled: true,
                    })
                    .await;
            } else {
                notifier
                    .send_worktree_status(WorktreeStatus::IgnoredCopyComplete {
                        session_id,
                        worktree_path,
                        files_copied: report.files_copied,
                        dirs_created: report.dirs_created,
                    })
                    .await;
            }
        }
        Some(Ok((Err(e), _))) => {
            notifier
                .send_worktree_status(WorktreeStatus::IgnoredCopyError {
                    session_id,
                    worktree_path,
                    message: format!("Background copy failed: {}", e),
                    cancelled: false,
                })
                .await;
        }
        Some(Err(e)) => {
            // Task was aborted (JoinError)
            let cancelled = e.is_cancelled();
            notifier
                .send_worktree_status(WorktreeStatus::IgnoredCopyError {
                    session_id,
                    worktree_path,
                    message: if cancelled {
                        "Background copy was cancelled".to_string()
                    } else {
                        format!("Background copy task failed: {}", e)
                    },
                    cancelled,
                })
                .await;
        }
        None => {
            // Cancelled via tokio::select!
            notifier
                .send_worktree_status(WorktreeStatus::IgnoredCopyError {
                    session_id,
                    worktree_path,
                    message: "Background copy was cancelled".to_string(),
                    cancelled: true,
                })
                .await;
        }
    }
}

// ============================================================================
// Request / Response types
// ============================================================================

fn default_copy_mode() -> WorktreeCopyMode {
    WorktreeCopyMode::Dirty
}

pub struct PrepareWorktreeResult {
    pub response: Result<CreateWorktreeResponse>,
    pub spawn_task: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status")]
pub enum WorktreeStatus {
    // === EXISTING VARIANTS (unchanged for backward compatibility) ===
    #[serde(rename = "progress")]
    Progress {
        #[serde(rename = "sessionId")]
        session_id: String,
        message: String,
    },
    #[serde(rename = "created")]
    Created {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "worktreePath")]
        worktree_path: String,
        commit: String,
        /// Working directory root of the source repo/worktree (via `workdir()`).
        /// Clients strip this prefix from `source_path` to compute the
        /// subdirectory offset inside the new worktree.
        #[serde(rename = "sourceGitRoot", skip_serializing_if = "Option::is_none")]
        source_git_root: Option<String>,
        /// NEW optional field -- only present when dirty copying is used
        #[serde(rename = "copiedChanges", skip_serializing_if = "Option::is_none")]
        copied_changes: Option<CopiedChangesSummary>,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(rename = "sessionId")]
        session_id: String,
        message: String,
    },

    // === NEW VARIANTS (additive -- old clients ignore unknown status values) ===
    /// Emitted when analyzing the source worktree for dirty state
    #[serde(rename = "analyzing")]
    Analyzing {
        #[serde(rename = "sessionId")]
        session_id: String,
        message: String,
    },

    /// Emitted with source worktree information and dirty state summary
    #[serde(rename = "sourceInfo")]
    SourceInfo {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "sourceCommit")]
        source_commit: String,
        #[serde(rename = "sourceBranch")]
        source_branch: Option<String>,
        #[serde(rename = "dirtyState")]
        dirty_state: DirtyStateSummary,
    },

    /// Emitted during dirty file copying with progress
    #[serde(rename = "copyingChanges")]
    CopyingChanges {
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Phase: "staged", "modified", "untracked", "deletions"
        phase: String,
        current: u32,
        total: u32,
        #[serde(rename = "currentFile")]
        current_file: Option<String>,
    },

    // === BACKGROUND IGNORED FILE COPY VARIANTS ===
    /// Background ignored file copy started
    #[serde(rename = "copyingIgnored")]
    CopyingIgnored {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "worktreePath")]
        worktree_path: String,
        message: String,
    },

    /// Background ignored file copy completed
    #[serde(rename = "ignoredCopyComplete")]
    IgnoredCopyComplete {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "worktreePath")]
        worktree_path: String,
        #[serde(rename = "filesCopied")]
        files_copied: u64,
        #[serde(rename = "dirsCreated")]
        dirs_created: u64,
    },

    /// Background ignored file copy failed/cancelled
    #[serde(rename = "ignoredCopyError")]
    IgnoredCopyError {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "worktreePath")]
        worktree_path: String,
        message: String,
        cancelled: bool,
    },

    /// Worktree creation was cancelled via the cancellation token.
    /// The partial worktree (if any) has been cleaned up.
    #[serde(rename = "cancelled")]
    Cancelled {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
}

#[async_trait::async_trait]
pub trait WorktreeNotificationSender {
    async fn send_worktree_status(&self, progress: WorktreeStatus);
}

// ============================================================================
// Human-Readable Worktree Naming
// ============================================================================

/// Maximum length for a sanitized label.
pub const MAX_LABEL_LEN: usize = 64;
/// Maximum suffix attempts for collision resolution.
pub const MAX_COLLISION_SUFFIX: u32 = 100;

/// Metadata key for the human-readable worktree label.
pub const META_KEY_LABEL: &str = "label";
/// Metadata key for whether the label was user-provided.
pub const META_KEY_USER_PROVIDED: &str = "user_provided";

/// Sanitize a user-provided label into a filesystem-safe directory name.
///
/// Lowercases, replaces spaces/underscores with hyphens, strips non-alphanumeric
/// characters (except hyphens -- dots are removed by this filter, making `.` and
/// `..` impossible), deduplicates consecutive hyphens, trims leading/trailing
/// hyphens, and truncates to [`MAX_LABEL_LEN`] characters.
pub fn sanitize_label(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    for ch in lower.chars() {
        match ch {
            ' ' | '_' => out.push('-'),
            c if c.is_ascii_alphanumeric() || c == '-' => out.push(c),
            _ => {}
        }
    }
    // Collapse consecutive hyphens.
    let collapsed = collapse_hyphens(&out);
    // Trim leading/trailing hyphens.
    let trimmed = collapsed.trim_matches('-');
    if trimmed.is_empty() {
        return String::new();
    }
    // Truncate to MAX_LABEL_LEN (clean break at hyphen boundary).
    truncate_label(trimmed)
}

pub fn collapse_hyphens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_hyphen = false;
    for ch in s.chars() {
        if ch == '-' {
            if !prev_hyphen {
                out.push('-');
            }
            prev_hyphen = true;
        } else {
            out.push(ch);
            prev_hyphen = false;
        }
    }
    out
}

pub fn truncate_label(s: &str) -> String {
    if s.len() <= MAX_LABEL_LEN {
        return s.to_owned();
    }
    let truncated = &s[..MAX_LABEL_LEN];
    truncated.trim_end_matches('-').to_owned()
}

/// Generate an automatic label: `YYYY-MM-DD-<uuid_prefix>`.
pub fn auto_label() -> String {
    let date = chrono::Local::now().format("%Y-%m-%d");
    let uuid_hex = uuid::Uuid::new_v4().simple().to_string();
    let short = &uuid_hex[..8];
    format!("{date}-{short}")
}

/// Derive a worktree label from optional user input.
///
/// If the user provides a non-empty name, sanitize it; otherwise generate
/// an automatic label.
pub fn derive_worktree_label(user_input: Option<&str>) -> String {
    match user_input {
        Some(name) if !name.trim().is_empty() => {
            let sanitized = sanitize_label(name);
            if sanitized.is_empty() {
                auto_label()
            } else {
                sanitized
            }
        }
        _ => auto_label(),
    }
}

/// Derive a collision-resistant repository slug from the git root path.
///
/// Uses the last 2 path components (skipping home-directory boilerplate
/// and dot-prefixed segments) joined by `-`. Falls back to `"repo"` when
/// no suitable components exist.
pub fn repo_slug(git_root: &Path) -> String {
    let components: Vec<&str> = git_root
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .filter(|s| !s.is_empty() && *s != "home" && *s != "Users" && !s.starts_with('.'))
        .collect();
    if components.is_empty() {
        return "repo".to_owned();
    }
    let take = 2.min(components.len());
    let raw = components[components.len() - take..].join("-");
    let slug = sanitize_label(&raw);
    if slug.is_empty() {
        "repo".to_owned()
    } else {
        slug
    }
}

/// Resolve a worktree directory name, appending a numeric suffix on collision.
///
/// Checks whether `base_dir/label` already exists on disk; if so, tries
/// `label-2`, `label-3`, ... up to [`MAX_COLLISION_SUFFIX`].
pub fn resolve_label_collision(base_dir: &Path, label: &str) -> String {
    let candidate = base_dir.join(label);
    if !candidate.exists() {
        return label.to_owned();
    }
    for i in 2..=MAX_COLLISION_SUFFIX {
        let suffixed = format!("{label}-{i}");
        if !base_dir.join(&suffixed).exists() {
            return suffixed;
        }
    }
    // Fallback: auto-generate a unique label.
    auto_label()
}

// ============================================================================
// Worktree Base Directory Resolution
// ============================================================================

/// Resolve the grok home for worktree paths via the **same** resolver used for
/// `worktrees.db` (`xai_fast_worktree::resolve_grok_home`), so checkout dirs and
/// the metadata DB always live under the same `.grok` tree. That resolver
/// canonicalizes its `$HOME` fallback to match `xai_grok_config::grok_home()`,
/// so worktree paths also agree with trust/hooks and other grok-home paths.
fn grok_home() -> std::path::PathBuf {
    xai_fast_worktree::resolve_grok_home().unwrap_or_else(|_| {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join(".grok")
    })
}

/// Returns `~/.grok/worktrees/<repo_slug>` for the given git root.
///
/// Uses [`repo_slug`] to derive a collision-resistant directory name from
/// the last two meaningful path components.
pub fn worktree_base_dir(git_root: &Path) -> std::path::PathBuf {
    let slug = repo_slug(git_root);
    grok_home().join("worktrees").join(slug)
}

/// Resolves the worktree base directory (`~/.grok/worktrees/<repo_name>`)
/// for a given source path, correctly handling grok-managed worktrees.
///
/// When `source_path` is already under `~/.grok/worktrees/<repo>/...`, the
/// repo name is derived from the directory structure directly. This avoids
/// `find_main_repo_root_from_path`, which misidentifies standalone worktrees
/// as the main repo root (returning the worktree itself instead of the
/// original repo).
///
/// For paths outside the grok worktree directory, falls back to
/// `find_main_repo_root_from_path` + `worktree_base_dir`.
pub fn worktree_base_dir_for_source(source_path: &Path) -> Result<std::path::PathBuf> {
    let worktrees_dir = grok_home().join("worktrees");

    if let Ok(suffix) = source_path.strip_prefix(&worktrees_dir) {
        if let Some(component) = suffix.components().next() {
            Ok(worktrees_dir.join(component))
        } else {
            Ok(worktrees_dir.join("repo"))
        }
    } else {
        let git_root = find_main_repo_root_from_path(source_path)?;
        Ok(worktree_base_dir(&git_root))
    }
}

fn resolve_worktree_path(req: &CreateWorktreeRequest, git_root: &Path) -> String {
    if let Some(ref path) = req.worktree_path {
        return path.clone();
    }

    let base = worktree_base_dir(git_root);
    let label = derive_worktree_label(req.label.as_deref());
    let dir_name = resolve_label_collision(&base, &label);
    base.join(dir_name).to_string_lossy().to_string()
}

/// Build the label metadata JSON to persist in the worktree DB record.
pub fn build_label_metadata(label: &str, user_provided: bool) -> serde_json::Value {
    serde_json::json!({
        (META_KEY_LABEL): label,
        (META_KEY_USER_PROVIDED): user_provided,
    })
}

/// Extract the label from the resolved worktree path (last path component).
pub fn label_from_path(worktree_path: &str) -> String {
    Path::new(worktree_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Walk up from `cwd` (staying within `~/.grok/worktrees/`) to its registered
/// worktree record.
///
/// Shared resolver for [`lookup_worktree_label`] and [`touch_worktree_for_cwd`];
/// returns the open DB alongside the record so callers can issue follow-up
/// queries. Returns `None` for non-worktree paths (without opening the DB) or
/// when the DB is unavailable.
fn worktree_record_for_cwd(cwd: &str) -> Option<(WorktreeDb, WorktreeRecord)> {
    let worktrees_dir = grok_home().join("worktrees");
    let mut path = Path::new(cwd);
    if !path.starts_with(&worktrees_dir) {
        return None;
    }
    let db = match open_db() {
        Ok(db) => db,
        Err(e) => {
            // Loud like register_worktree: a broken DB silently disables both
            // label lookup and gc liveness touches.
            tracing::warn!(error = %e, "failed to open worktree DB for cwd lookup");
            return None;
        }
    };
    while path.starts_with(&worktrees_dir) && path != worktrees_dir {
        if let Ok(Some(record)) = db.get(&path.to_string_lossy()) {
            return Some((db, record));
        }
        path = path.parent()?;
    }
    None
}

/// The recorded source repo of the grok-managed worktree containing `cwd`, if any.
///
/// Thin wrapper over [`worktree_record_for_cwd`] that drops the DB handle;
/// returns `None` (without DB I/O) for paths outside `~/.grok/worktrees/`.
pub(crate) fn source_repo_for_cwd(cwd: &str) -> Option<std::path::PathBuf> {
    worktree_record_for_cwd(cwd).map(|(_db, rec)| rec.source_repo)
}

/// Look up the worktree label for a cwd by querying the worktree DB.
///
/// Resolves the containing worktree via [`worktree_record_for_cwd`], then
/// extracts the `"label"` key from its metadata. Returns `None` for
/// non-worktree paths or when the DB is unavailable.
pub fn lookup_worktree_label(cwd: &str) -> Option<String> {
    let (_db, record) = worktree_record_for_cwd(cwd)?;
    record
        .metadata
        .as_ref()
        .and_then(|m| m.get(META_KEY_LABEL))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Record activity on the worktree containing `cwd` (best-effort, infallible).
///
/// Updates `last_accessed_at` in the worktree DB so `gc` expires worktrees by
/// last use rather than creation time. Non-worktree paths are a no-op.
pub fn touch_worktree_for_cwd(cwd: &str) {
    if let Some((db, record)) = worktree_record_for_cwd(cwd)
        && let Err(e) = db.touch(&record.id)
    {
        // A failing touch silently degrades expiry back to created_at —
        // leave log evidence without bothering callers.
        tracing::debug!(error = %e, id = %record.id, "worktree touch failed");
    }
}

// ============================================================================
// Worktree Lifecycle: Create
// ============================================================================

pub async fn prepare_worktree_creation(req: &CreateWorktreeRequest) -> PrepareWorktreeResult {
    let source_path = Path::new(&req.source_path);
    let git_root = match find_main_repo_root_from_path(source_path) {
        Ok(root) => root,
        Err(e) => {
            return PrepareWorktreeResult {
                response: Err(anyhow::anyhow!("Invalid source path: {}", e)),
                spawn_task: false,
            };
        }
    };

    let worktree_path = resolve_worktree_path(req, &git_root);
    let source_git_root = find_git_root_from_path(source_path)
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    if is_worktree_in_progress(&req.session_id).await {
        return PrepareWorktreeResult {
            response: Ok(CreateWorktreeResponse::Creating {
                session_id: req.session_id.clone(),
                worktree_path: worktree_path.clone(),
                source_git_root,
            }),
            spawn_task: false,
        };
    }

    // If worktree exists, return its HEAD
    if tokio::fs::metadata(&worktree_path).await.is_ok() {
        let commit = git_cli(Path::new(&worktree_path), &["rev-parse", "HEAD"])
            .await
            .unwrap_or_default();
        return PrepareWorktreeResult {
            response: Ok(CreateWorktreeResponse::Exists {
                session_id: req.session_id.clone(),
                worktree_path,
                commit,
                source_git_root,
            }),
            spawn_task: false,
        };
    }

    // Verify source is a valid git repository/worktree
    if git_cli(source_path, &["rev-parse", "--git-dir"])
        .await
        .is_err()
    {
        return PrepareWorktreeResult {
            response: Err(anyhow::anyhow!("Not a git repository or worktree")),
            spawn_task: false,
        };
    }

    // Don't set the marker here: in proxy mode the shell never spawns
    // `create_worktree_async`, so a marker set here would never clear and would wedge
    // every retry in `Creating`. The async entrypoint owns it; `prepare` only reads it.
    PrepareWorktreeResult {
        response: Ok(CreateWorktreeResponse::Creating {
            session_id: req.session_id.clone(),
            worktree_path,
            source_git_root,
        }),
        spawn_task: true,
    }
}

pub async fn create_worktree_async<N: WorktreeNotificationSender + Clone + 'static>(
    req: CreateWorktreeRequest,
    notifier: N,
    copy_context: BackgroundCopyContext,
) {
    let session_id = req.session_id.clone();
    // A `prepare` race can spawn two creators for one session (prepare only reads
    // the marker). The loser bails before any work and must not clear the
    // winner's marker; the winner's terminal status is keyed by session_id, so a
    // silent return yields exactly one creation and one client notification.
    if !claim_worktree_in_progress(&session_id).await {
        return;
    }
    let result = create_worktree_streaming(&req, &notifier).await;
    mark_worktree_complete(&session_id).await;

    // Check if we should run background ignored file copy
    let should_copy_ignored_in_background =
        req.copy_ignored_in_background && matches!(&result, WorktreeStatus::Created { .. });

    // Extract worktree_path before sending notification
    let worktree_path = if let WorktreeStatus::Created { worktree_path, .. } = &result {
        Some(worktree_path.clone())
    } else {
        None
    };

    notifier.send_worktree_status(result).await;

    // Run background ignored file copy if requested (spawned as separate task)
    // Using spawn_local because the notifier uses non-Send futures internally
    if should_copy_ignored_in_background && let Some(worktree_path) = worktree_path {
        let notifier = notifier.clone();
        tokio::task::spawn_local(async move {
            run_background_ignored_copy(
                copy_context,
                session_id,
                req.source_path,
                worktree_path,
                req.ignored_skip_patterns,
                notifier,
            )
            .await;
        });
    }
}

pub async fn create_worktree_streaming<N: WorktreeNotificationSender>(
    req: &CreateWorktreeRequest,
    notifier: &N,
) -> WorktreeStatus {
    let start = std::time::Instant::now();
    let source_path = Path::new(&req.source_path);
    let session_id = req.session_id.clone();
    let git_root = match find_main_repo_root_from_path(source_path) {
        Ok(root) => root,
        Err(e) => {
            tracing::warn!(
                target: WORKTREE_LOG,
                session_id = %session_id,
                source = %req.source_path,
                error = %e,
                "CREATE_ERROR: invalid source path"
            );
            return WorktreeStatus::Error {
                session_id,
                message: format!("Invalid source path: {}", e),
            };
        }
    };

    let worktree_path_str = resolve_worktree_path(req, &git_root);

    tracing::info!(
        target: WORKTREE_LOG,
        session_id = %session_id,
        source = %req.source_path,
        dest = %worktree_path_str,
        copy_mode = ?req.copy_mode,
        worktree_type = ?req.worktree_type,
        git_ref = req.git_ref.as_deref().unwrap_or("HEAD"),
        "CREATE_START: creating worktree via WorktreeBuilder"
    );

    // Emit progress notification
    notifier
        .send_worktree_status(WorktreeStatus::Progress {
            session_id: session_id.clone(),
            message: "Creating worktree with fast CoW copy...".to_string(),
        })
        .await;

    // Map WorktreeCopyMode to xai_fast_worktree::WorkingTreeMode
    let working_tree_mode = match req.copy_mode {
        WorktreeCopyMode::Dirty => WorkingTreeMode::PreserveWorkingTree,
        WorktreeCopyMode::Clean => WorkingTreeMode::CleanAll,
    };

    // Use xai-fast-worktree for high-performance worktree creation
    // Note: WorktreeBuilder::create() is a blocking operation, so we use spawn_blocking
    let source_path = req.source_path.clone();
    let dest_path = worktree_path_str.clone();
    let git_ref = req.git_ref.clone();
    // Determine worktree type, preserving the .git.is_dir() guard for Standalone mode.
    // A linked worktree has a `.git` *file* pointing to the main repo; a real repo has a `.git` *directory*.
    let requested_type = req.worktree_type.unwrap_or(WorktreeType::Linked);
    let git_dir_is_directory = std::path::Path::new(&req.source_path).join(".git").is_dir();
    let creation_mode = if requested_type == WorktreeType::Standalone {
        if git_dir_is_directory {
            WorktreeType::Standalone
        } else {
            // Standalone requested but source is a linked worktree -- fall back to Linked
            tracing::warn!(
                target: WORKTREE_LOG,
                session_id = %session_id,
                source = %req.source_path,
                requested_type = ?requested_type,
                git_dir_is_directory,
                "WORKTREE_MODE_FALLBACK: standalone requested from linked worktree source; falling back to linked"
            );
            tracing::warn!(
                "Standalone mode requested but source has a .git file (linked worktree), \
                 falling back to Linked mode"
            );
            WorktreeType::Linked
        }
    } else {
        requested_type
    };
    tracing::info!(
        target: WORKTREE_LOG,
        session_id = %session_id,
        source = %req.source_path,
        requested_type = ?requested_type,
        final_creation_mode = ?creation_mode,
        git_dir_is_directory,
        git_ref = req.git_ref.as_deref().unwrap_or("HEAD"),
        "WORKTREE_MODE_RESOLVED: create worktree creation mode"
    );
    let session_id_for_builder = session_id.clone();
    let btrfs_delegate = btrfs_delegate_from_env();
    let user_provided_label = req.worktree_path.is_none()
        && req
            .label
            .as_ref()
            .is_some_and(|n| !n.trim().is_empty() && !sanitize_label(n).is_empty());
    let label_for_meta = label_from_path(&worktree_path_str);
    let label_metadata = build_label_metadata(&label_for_meta, user_provided_label);
    let report = match tokio::task::spawn_blocking(move || {
        let mut builder = WorktreeBuilder::new(&source_path, &dest_path)
            .working_tree_mode(working_tree_mode)
            .ignored_files_mode(IgnoredFilesMode::Skip)
            .creation_mode(to_creation_mode(creation_mode))
            .worktree_kind(xai_fast_worktree::WorktreeKind::Session)
            .session_id(session_id_for_builder)
            .metadata(label_metadata);

        // Apply git_ref if specified (branch, tag, or commit SHA)
        if let Some(ref git_ref) = git_ref {
            builder = builder.git_ref(git_ref);
        }

        // Wire up btrfs delegate for rootless snapshot support.
        if let Some(delegate) = btrfs_delegate {
            builder = builder.btrfs_delegate(delegate);
        }

        builder.create()
    })
    .await
    {
        Ok(Ok(report)) => report,
        Ok(Err(e)) => {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            tracing::warn!(
                target: WORKTREE_LOG,
                session_id = %session_id,
                dest = %worktree_path_str,
                elapsed_ms,
                error = %e,
                "CREATE_ERROR: WorktreeBuilder::create failed"
            );
            return WorktreeStatus::Error {
                session_id,
                message: format!("Worktree creation failed: {}", e),
            };
        }
        Err(e) => {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            tracing::warn!(
                target: WORKTREE_LOG,
                session_id = %session_id,
                dest = %worktree_path_str,
                elapsed_ms,
                error = %e,
                "CREATE_PANIC: WorktreeBuilder task panicked"
            );
            return WorktreeStatus::Error {
                session_id,
                message: format!("Worktree creation task failed: {}", e),
            };
        }
    };

    // Map WorktreeReport to CopiedChangesSummary
    let (dirty_modified, dirty_untracked, dirty_deleted) =
        if req.copy_mode == WorktreeCopyMode::Dirty {
            report
                .unignored_copy
                .dirty_files
                .as_ref()
                .map(|d| {
                    (
                        d.modified_files as u32,
                        d.untracked_files as u32,
                        d.deleted_files as u32,
                    )
                })
                .unwrap_or((0, 0, 0))
        } else {
            (0, 0, 0)
        };

    // Collect warnings from both unignored and ignored copies
    let mut warnings = report.unignored_copy.issues;
    let ignored_files_copied = if let Some(ignored) = report.ignored_copy {
        warnings.extend(ignored.issues);
        ignored.files_copied as u32
    } else {
        0
    };

    let copied_changes = CopiedChangesSummary {
        staged_copied: report.unignored_copy.files_copied as u32,
        modified_copied: dirty_modified,
        untracked_copied: ignored_files_copied + dirty_untracked,
        deletions_applied: dirty_deleted,
        warnings,
    };

    let absolute_path = report.worktree_path.to_string_lossy().to_string();

    let elapsed_ms = start.elapsed().as_millis() as u64;
    tracing::debug!(
        session_id = %req.session_id,
        path = %absolute_path,
        commit = %report.commit,
        copy_mode = ?req.copy_mode,
        files_copied = copied_changes.staged_copied,
        elapsed = ?start.elapsed(),
        "fast worktree created"
    );
    tracing::info!(
        target: WORKTREE_LOG,
        session_id = %req.session_id,
        path = %absolute_path,
        commit = %report.commit,
        copy_mode = ?req.copy_mode,
        files_copied = copied_changes.staged_copied,
        modified_copied = copied_changes.modified_copied,
        untracked_copied = copied_changes.untracked_copied,
        deletions_applied = copied_changes.deletions_applied,
        elapsed_ms,
        "CREATE_OK: worktree created successfully"
    );

    // source_path was shadowed as a String for the spawn_blocking closure above.
    let source_git_root = find_git_root_from_path(Path::new(&req.source_path))
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    WorktreeStatus::Created {
        session_id: req.session_id.clone(),
        worktree_path: absolute_path,
        commit: report.commit,
        source_git_root,
        copied_changes: Some(copied_changes),
    }
}

// ============================================================================
// Remove Worktree
// ============================================================================

pub async fn remove_worktree(
    req: &RemoveWorktreeRequest,
    copy_context: &BackgroundCopyContext,
) -> Result<RemoveWorktreeResponse> {
    let resolved = match (&req.worktree_path, &req.id_or_path) {
        (Some(_), Some(_)) => {
            anyhow::bail!("exactly one of worktreePath or idOrPath must be set, not both")
        }
        (Some(path), None) => path.clone(),
        (None, Some(id)) => match resolve_worktree_by_id_or_path(id)? {
            Some(p) => p.display().to_string(),
            None => anyhow::bail!("worktree not found: {id}"),
        },
        (None, None) => anyhow::bail!("either worktreePath or idOrPath must be set"),
    };
    let worktree_path = Path::new(&resolved);

    tracing::info!(
        target: WORKTREE_LOG,
        path = %resolved,
        force = req.force,
        dry_run = req.dry_run,
        "REMOVE_START: removing worktree"
    );

    // jj workspace: detect by .jj/repo and route to jj-specific cleanup.
    if worktree_path.join(".jj").join("repo").exists() {
        if req.dry_run {
            return Ok(RemoveWorktreeResponse {
                removed: false,
                resolved_path: Some(resolved),
            });
        }
        tracing::info!(target: WORKTREE_LOG, path = %resolved, "REMOVE_JJ: using jj workspace forget + rm");
        remove_jj_workspace(&resolved).await?;
        return Ok(RemoveWorktreeResponse {
            removed: true,
            resolved_path: Some(resolved),
        });
    }

    if req.dry_run {
        return Ok(RemoveWorktreeResponse {
            removed: false,
            resolved_path: Some(resolved),
        });
    }

    let remove_start = std::time::Instant::now();

    let was_copying = copy_context.cancel(&resolved);
    if was_copying {
        tracing::info!(
            target: WORKTREE_LOG,
            path = %resolved,
            "REMOVE_CANCEL_BG_COPY: cancelled background ignored file copy"
        );
    }

    let wt_path = worktree_path.to_path_buf();
    let force = req.force;

    // The btrfs delegate is used only as a fallback when a direct btrfs op fails
    // (rootless hosts lack CAP_SYS_ADMIN for direct subvolume ops).
    let delegate = btrfs_delegate_from_env();
    match tokio::task::spawn_blocking(move || {
        xai_fast_worktree::remove_worktree_with_delegate(&wt_path, delegate)
    })
    .await
    {
        Ok(Ok(report)) => {
            let elapsed_ms = remove_start.elapsed().as_millis() as u64;
            if report.used_btrfs_delete {
                tracing::info!(
                    worktree_path = %resolved,
                    unmounted_bind = report.unmounted_bind,
                    "Worktree removed via btrfs subvolume delete (O(1))"
                );
            }
            tracing::info!(
                target: WORKTREE_LOG,
                path = %resolved,
                elapsed_ms,
                used_btrfs = report.used_btrfs_delete,
                "REMOVE_OK: worktree removed successfully"
            );
            Ok(RemoveWorktreeResponse {
                removed: true,
                resolved_path: Some(resolved),
            })
        }
        Ok(Err(e)) => {
            if force {
                tracing::info!(
                    target: WORKTREE_LOG,
                    path = %resolved,
                    error = %e,
                    "REMOVE_FAST_FAILED: trying git worktree remove --force"
                );
                let git_root = find_main_repo_root_from_path(worktree_path)?;
                git_cli(&git_root, &["worktree", "remove", "--force", &resolved]).await?;
                let elapsed_ms = remove_start.elapsed().as_millis() as u64;
                tracing::info!(
                    target: WORKTREE_LOG,
                    path = %resolved,
                    elapsed_ms,
                    "REMOVE_OK_FORCE: worktree removed via git CLI --force"
                );
                Ok(RemoveWorktreeResponse {
                    removed: true,
                    resolved_path: Some(resolved),
                })
            } else {
                let elapsed_ms = remove_start.elapsed().as_millis() as u64;
                tracing::warn!(
                    target: WORKTREE_LOG,
                    path = %resolved,
                    elapsed_ms,
                    error = %e,
                    "REMOVE_ERROR: fast remove failed (force=false)"
                );
                Err(e)
            }
        }
        Err(e) => {
            let elapsed_ms = remove_start.elapsed().as_millis() as u64;
            tracing::warn!(
                target: WORKTREE_LOG,
                path = %resolved,
                elapsed_ms,
                error = %e,
                "REMOVE_PANIC: remove_worktree task panicked"
            );
            Err(anyhow::anyhow!("remove_worktree task failed: {}", e))
        }
    }
}

/// Recreate a disposed subagent worktree at `dest` from `snapshot_ref`, using
/// the durable snapshot stored in `source_repo`. `session_id` tags the
/// re-registered DB record (create-path parity). Returns the rehydrated worktree
/// path. The blocking fast-worktree work runs on a blocking thread.
pub async fn rehydrate_subagent_worktree(
    dest: &Path,
    source_repo: &Path,
    snapshot_ref: &str,
    session_id: Option<&str>,
) -> Result<std::path::PathBuf> {
    let dest = dest.to_path_buf();
    let source_repo = source_repo.to_path_buf();
    let snapshot_ref = snapshot_ref.to_string();
    let session_id = session_id.map(str::to_owned);
    let report = tokio::task::spawn_blocking(move || {
        xai_fast_worktree::rehydrate_worktree_from_ref(
            &dest,
            &source_repo,
            &snapshot_ref,
            session_id.as_deref(),
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("rehydrate_subagent_worktree task failed: {e}"))??;
    Ok(report.worktree_path)
}

/// Snapshot a subagent worktree's working state into `ref_name` and make it
/// durable in `source_repo`, returning the ref name to persist as
/// `snapshot_ref`. Does NOT touch the directory: the caller persists the ref
/// FIRST, then removes the worktree via [`remove_subagent_worktree`], so a
/// failed (or not-yet-persisted) removal never strands a snapshot the resume
/// path can't find.
///
/// Standalone worktrees keep the snapshot in their own `.git`, which is
/// destroyed on removal — so after capturing, the snapshot is transferred into
/// `source_repo` (which survives the worktree) and verified to resolve there
/// before returning `Ok`. The blocking fast-worktree work runs on a blocking
/// thread.
pub async fn snapshot_subagent_worktree(
    worktree_path: &Path,
    source_repo: &Path,
    ref_name: &str,
) -> Result<String> {
    let worktree_path = worktree_path.to_path_buf();
    let source_repo = source_repo.to_path_buf();
    let ref_name = ref_name.to_string();
    tokio::task::spawn_blocking(move || -> Result<String> {
        let message = format!("subagent worktree snapshot {ref_name}");
        // Capture into the worktree's git, then make it durable in the source
        // repo (and verify) so it survives the worktree's deletion.
        xai_fast_worktree::snapshot_worktree_to_ref(&worktree_path, &ref_name, &message)?;
        xai_fast_worktree::transfer_snapshot_to_repo(&worktree_path, &source_repo, &ref_name)?;
        Ok(ref_name)
    })
    .await
    .map_err(|e| anyhow::anyhow!("snapshot_subagent_worktree task failed: {e}"))?
}

/// Remove a subagent worktree directory. Call only AFTER
/// [`snapshot_subagent_worktree`] succeeded and its ref was persisted, so a
/// removal failure is safe to treat as best-effort (the durable ref already
/// backs resume). The blocking fast-worktree work runs on a blocking thread.
pub async fn remove_subagent_worktree(worktree_path: &Path) -> Result<()> {
    let worktree_path = worktree_path.to_path_buf();
    // On rootless hosts the snapshot delete needs the privileged helper; without
    // the delegate the btrfs delete hits EPERM and the snapshot leaks.
    let delegate = btrfs_delegate_from_env();
    tokio::task::spawn_blocking(move || {
        xai_fast_worktree::remove_worktree_with_delegate(&worktree_path, delegate)
    })
    .await
    .map_err(|e| anyhow::anyhow!("remove_subagent_worktree task failed: {e}"))??;
    Ok(())
}

/// Test-only thin wrapper: snapshot then remove (capture-first). NOT for
/// production use — the completion path drives [`snapshot_subagent_worktree`] and
/// [`remove_subagent_worktree`] separately so it can persist the ref between the
/// two steps (removing without persisting first is a crash-safety footgun).
#[cfg(test)]
async fn snapshot_and_remove_subagent_worktree(
    worktree_path: &Path,
    source_repo: &Path,
    ref_name: &str,
) -> Result<String> {
    let snapshot_ref = snapshot_subagent_worktree(worktree_path, source_repo, ref_name).await?;
    remove_subagent_worktree(worktree_path).await?;
    Ok(snapshot_ref)
}

// ============================================================================
// Create Worktree from Existing Worktree (Fork Flow)
// ============================================================================

/// Request to create a new worktree from an existing worktree.
/// Used during session forking to create a copy of another worktree's state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorktreeFromWorktreeRequest {
    /// Path to the source worktree to copy from
    pub source_worktree_path: String,
    /// Session ID for the new worktree (client-provided optimistic ID)
    pub new_session_id: String,
    /// Copy mode: "clean" or "dirty" (default: "dirty")
    #[serde(default = "default_copy_mode")]
    pub copy_mode: WorktreeCopyMode,
    /// Git ref (branch, tag, or commit SHA) to checkout in the worktree.
    /// If not specified, defaults to HEAD of the source worktree.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// Worktree creation type: "linked", "standalone", or "git".
    /// If not specified, the agent's config default will be used.
    #[serde(default)]
    pub worktree_type: Option<WorktreeType>,
    /// Human-readable label for the worktree directory name.
    /// When absent, an automatic `YYYY-MM-DD-<uuid>` label is generated.
    #[serde(default)]
    pub label: Option<String>,
    /// Optional cancellation token. When tripped, the file copy is aborted
    /// mid-flight and the partial worktree is cleaned up.
    #[serde(skip)]
    pub cancellation_token: Option<tokio_util::sync::CancellationToken>,
    /// Destination path pinned by `prepare_worktree_from_worktree` so the
    /// async creation reuses the same directory returned to the client.
    #[serde(skip)]
    pub resolved_dest_path: Option<String>,
}

impl CreateWorktreeFromWorktreeRequest {
    /// Project onto the lean wire request, dropping the two `#[serde(skip)]`
    /// runtime-only fields (which never ride the wire anyway). Consumes `self`
    /// so the owned string fields move instead of cloning.
    pub fn into_wire(self) -> CreateWorktreeFromWorktreeRequestWire {
        CreateWorktreeFromWorktreeRequestWire {
            source_worktree_path: self.source_worktree_path,
            new_session_id: self.new_session_id,
            copy_mode: self.copy_mode,
            git_ref: self.git_ref,
            worktree_type: self.worktree_type,
            label: self.label,
        }
    }
}

impl From<CreateWorktreeFromWorktreeRequestWire> for CreateWorktreeFromWorktreeRequest {
    fn from(w: CreateWorktreeFromWorktreeRequestWire) -> Self {
        Self {
            source_worktree_path: w.source_worktree_path,
            new_session_id: w.new_session_id,
            copy_mode: w.copy_mode,
            git_ref: w.git_ref,
            worktree_type: w.worktree_type,
            label: w.label,
            // Runtime-only fields, never on the wire.
            cancellation_token: None,
            resolved_dest_path: None,
        }
    }
}

/// Resolve the target worktree path for a fork operation.
///
/// When the source path is already inside `~/.grok/worktrees/<repo>/`, the
/// repo name is derived from the directory structure rather than calling
/// `find_main_repo_root_from_path` (which would return the standalone
/// worktree root itself, causing nested paths).
fn resolve_fork_worktree_path(
    source_worktree_path: &Path,
    _new_session_id: &str,
    label: Option<&str>,
) -> Result<String> {
    let base = worktree_base_dir_for_source(source_worktree_path)?;
    let dir_name = derive_worktree_label(label);
    let dir_name = resolve_label_collision(&base, &dir_name);
    Ok(base.join(dir_name).to_string_lossy().into_owned())
}

/// Prepare for creating a worktree from another worktree (fork flow).
/// Returns the prepared result with sync response and whether to spawn an async task.
pub async fn prepare_worktree_from_worktree(
    req: &CreateWorktreeFromWorktreeRequest,
) -> PrepareWorktreeResult {
    let source_path = Path::new(&req.source_worktree_path);

    // Verify the source path is a valid git worktree
    if git_cli(source_path, &["rev-parse", "--git-dir"])
        .await
        .is_err()
    {
        return PrepareWorktreeResult {
            response: Err(anyhow::anyhow!(
                "Source path is not a valid git repository or worktree: {}",
                req.source_worktree_path
            )),
            spawn_task: false,
        };
    }

    let worktree_path =
        match resolve_fork_worktree_path(source_path, &req.new_session_id, req.label.as_deref()) {
            Ok(path) => path,
            Err(e) => {
                return PrepareWorktreeResult {
                    response: Err(anyhow::anyhow!("Failed to resolve worktree path: {}", e)),
                    spawn_task: false,
                };
            }
        };

    let git_root_str = find_git_root_from_path(source_path)
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    // Check if creation is already in progress
    if is_worktree_in_progress(&req.new_session_id).await {
        return PrepareWorktreeResult {
            response: Ok(CreateWorktreeResponse::Creating {
                session_id: req.new_session_id.clone(),
                worktree_path: worktree_path.clone(),
                source_git_root: git_root_str,
            }),
            spawn_task: false,
        };
    }

    // If worktree already exists, return its HEAD commit
    if tokio::fs::metadata(&worktree_path).await.is_ok() {
        let commit = git_cli(Path::new(&worktree_path), &["rev-parse", "HEAD"])
            .await
            .unwrap_or_default();
        return PrepareWorktreeResult {
            response: Ok(CreateWorktreeResponse::Exists {
                session_id: req.new_session_id.clone(),
                worktree_path,
                commit,
                source_git_root: git_root_str,
            }),
            spawn_task: false,
        };
    }

    // Don't set the marker here: in proxy mode the shell never spawns
    // `create_worktree_from_worktree_async`, so a marker set here would never clear and
    // would wedge the session. The async entrypoint owns it; `prepare` only reads it.
    PrepareWorktreeResult {
        response: Ok(CreateWorktreeResponse::Creating {
            session_id: req.new_session_id.clone(),
            worktree_path,
            source_git_root: git_root_str,
        }),
        spawn_task: true,
    }
}

/// Create a worktree from another worktree asynchronously (fork flow).
/// Sends progress notifications through the notifier and final status when complete.
pub async fn create_worktree_from_worktree_async<N: WorktreeNotificationSender>(
    mut req: CreateWorktreeFromWorktreeRequest,
    notifier: N,
) {
    let session_id = req.new_session_id.clone();
    // See create_worktree_async: dedup concurrent fork creators via the atomic
    // claim so the loser bails without clearing the winner's marker.
    if !claim_worktree_in_progress(&session_id).await {
        return;
    }
    let pinned_path = req.resolved_dest_path.take();
    let result = create_worktree_from_worktree_streaming(&req, &notifier, pinned_path).await;
    mark_worktree_complete(&session_id).await;
    notifier.send_worktree_status(result).await;
}

/// Best-effort removal of a partially-created worktree on a cancelled fork.
///
/// Runs the blocking delegate-aware removal off the reactor; failures are logged
/// rather than swallowed so a leaked snapshot stays visible.
async fn cleanup_cancelled_worktree(worktree_path: &str) {
    let path = std::path::PathBuf::from(worktree_path);
    let delegate = btrfs_delegate_from_env();
    match tokio::task::spawn_blocking(move || {
        xai_fast_worktree::remove_worktree_with_delegate(&path, delegate)
    })
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(
            target: WORKTREE_LOG,
            path = %worktree_path,
            error = %e,
            "FORK_CANCEL_CLEANUP_FAIL: failed to remove cancelled worktree"
        ),
        Err(e) => tracing::warn!(
            target: WORKTREE_LOG,
            path = %worktree_path,
            error = %e,
            "FORK_CANCEL_CLEANUP_PANIC: cancelled worktree removal task panicked"
        ),
    }
}

/// Internal streaming implementation for creating worktree from another worktree.
pub async fn create_worktree_from_worktree_streaming<N: WorktreeNotificationSender>(
    req: &CreateWorktreeFromWorktreeRequest,
    notifier: &N,
    pinned_dest_path: Option<String>,
) -> WorktreeStatus {
    let start = std::time::Instant::now();
    let source_path = Path::new(&req.source_worktree_path);
    let session_id = req.new_session_id.clone();

    tracing::info!(
        target: WORKTREE_LOG,
        session_id = %session_id,
        source = %req.source_worktree_path,
        copy_mode = ?req.copy_mode,
        worktree_type = ?req.worktree_type,
        git_ref = req.git_ref.as_deref().unwrap_or("HEAD"),
        has_cancel_token = req.cancellation_token.is_some(),
        "FORK_START: creating worktree from existing worktree"
    );

    let git_root = find_git_root_from_path(source_path).ok();

    let worktree_path_str = if let Some(resolved) = pinned_dest_path {
        resolved
    } else {
        match resolve_fork_worktree_path(source_path, &req.new_session_id, req.label.as_deref()) {
            Ok(path) => path,
            Err(e) => {
                return WorktreeStatus::Error {
                    session_id,
                    message: format!("Failed to resolve worktree path: {}", e),
                };
            }
        }
    };

    // Emit analyzing notification
    notifier
        .send_worktree_status(WorktreeStatus::Analyzing {
            session_id: session_id.clone(),
            message: "Analyzing source worktree for fork...".to_string(),
        })
        .await;

    // Emit progress notification
    notifier
        .send_worktree_status(WorktreeStatus::Progress {
            session_id: session_id.clone(),
            message: "Creating forked worktree with fast CoW copy...".to_string(),
        })
        .await;

    // Map WorktreeCopyMode to xai_fast_worktree::WorkingTreeMode
    let working_tree_mode = match req.copy_mode {
        WorktreeCopyMode::Dirty => WorkingTreeMode::PreserveWorkingTree,
        WorktreeCopyMode::Clean => WorkingTreeMode::CleanAll,
    };

    // Use xai-fast-worktree -- it handles copying from any worktree path
    let source_worktree_path = req.source_worktree_path.clone();
    let dest_path = worktree_path_str.clone();
    let git_ref = req.git_ref.clone();
    let cancel_token = req.cancellation_token.clone();
    // Determine worktree type, preserving the .git.is_dir() guard for Standalone mode.
    let requested_type = req.worktree_type.unwrap_or(WorktreeType::Linked);
    let git_dir_is_directory = std::path::Path::new(&req.source_worktree_path)
        .join(".git")
        .is_dir();
    let creation_mode = if requested_type == WorktreeType::Standalone {
        if git_dir_is_directory {
            WorktreeType::Standalone
        } else {
            tracing::warn!(
                target: WORKTREE_LOG,
                session_id = %session_id,
                source = %req.source_worktree_path,
                requested_type = ?requested_type,
                git_dir_is_directory,
                "WORKTREE_MODE_FALLBACK: standalone requested from linked worktree source; falling back to linked"
            );
            tracing::warn!(
                "Standalone mode requested but source has a .git file (linked worktree), \
                 falling back to Linked mode"
            );
            WorktreeType::Linked
        }
    } else {
        requested_type
    };
    tracing::info!(
        target: WORKTREE_LOG,
        session_id = %session_id,
        source = %req.source_worktree_path,
        requested_type = ?requested_type,
        final_creation_mode = ?creation_mode,
        git_dir_is_directory,
        git_ref = req.git_ref.as_deref().unwrap_or("HEAD"),
        "WORKTREE_MODE_RESOLVED: fork worktree creation mode"
    );
    let builder_result = {
        tracing::info!(
            target: WORKTREE_LOG,
            session_id = %session_id,
            worktree_type = ?creation_mode,
            has_cancel_token = cancel_token.is_some(),
            "FORK_BUILDER_START: entering spawn_blocking for WorktreeBuilder::create()"
        );
        let session_id_for_builder = session_id.clone();
        let btrfs_delegate = btrfs_delegate_from_env();
        let label_for_meta = label_from_path(&worktree_path_str);
        let label_metadata = build_label_metadata(&label_for_meta, false);
        tokio::task::spawn_blocking(move || {
            let mut builder = WorktreeBuilder::new(&source_worktree_path, &dest_path)
                .working_tree_mode(working_tree_mode)
                .ignored_files_mode(IgnoredFilesMode::Skip)
                .creation_mode(to_creation_mode(creation_mode))
                .worktree_kind(xai_fast_worktree::WorktreeKind::Fork)
                .session_id(session_id_for_builder)
                .metadata(label_metadata);

            // Apply git_ref if specified (branch, tag, or commit SHA)
            if let Some(ref git_ref) = git_ref {
                builder = builder.git_ref(git_ref);
            }

            // Apply cancellation token if provided (fork cancel support)
            if let Some(token) = cancel_token {
                builder = builder.cancellation_token(token);
            }

            if let Some(delegate) = btrfs_delegate {
                builder = builder.btrfs_delegate(delegate);
            }

            builder.create()
        })
        .await
    };
    let report = match builder_result {
        Ok(Ok(report)) => {
            tracing::info!(
                target: WORKTREE_LOG,
                session_id = %session_id,
                elapsed_ms = start.elapsed().as_millis() as u64,
                files_copied = report.unignored_copy.files_copied,
                "FORK_BUILDER_DONE: WorktreeBuilder::create() completed successfully"
            );
            report
        }
        Ok(Err(e)) => {
            let was_cancelled = req
                .cancellation_token
                .as_ref()
                .is_some_and(|t| t.is_cancelled());
            tracing::info!(
                target: WORKTREE_LOG,
                session_id = %session_id,
                elapsed_ms = start.elapsed().as_millis() as u64,
                was_cancelled,
                error = %e,
                "FORK_BUILDER_ERR: WorktreeBuilder::create() returned error"
            );
            if was_cancelled {
                cleanup_cancelled_worktree(&worktree_path_str).await;
                return WorktreeStatus::Cancelled { session_id };
            }
            return WorktreeStatus::Error {
                session_id,
                message: format!("Worktree creation failed: {}", e),
            };
        }
        Err(e) => {
            return WorktreeStatus::Error {
                session_id,
                message: format!("Worktree creation task failed: {}", e),
            };
        }
    };

    // Check if the token was tripped after the builder returned Ok
    let was_cancelled = req
        .cancellation_token
        .as_ref()
        .is_some_and(|t| t.is_cancelled());
    if was_cancelled {
        tracing::info!(
            target: WORKTREE_LOG,
            session_id = %session_id,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "FORK_BUILDER_CANCELLED_POST: token tripped after builder returned Ok, cleaning up"
        );
        cleanup_cancelled_worktree(&worktree_path_str).await;
        return WorktreeStatus::Cancelled { session_id };
    }

    // Build CopiedChangesSummary from the report
    let (dirty_modified, dirty_untracked, dirty_deleted) =
        if req.copy_mode == WorktreeCopyMode::Dirty {
            report
                .unignored_copy
                .dirty_files
                .as_ref()
                .map(|d| {
                    (
                        d.modified_files as u32,
                        d.untracked_files as u32,
                        d.deleted_files as u32,
                    )
                })
                .unwrap_or((0, 0, 0))
        } else {
            (0, 0, 0)
        };

    let mut warnings = report.unignored_copy.issues;
    let ignored_files_copied = if let Some(ignored) = report.ignored_copy {
        warnings.extend(ignored.issues);
        ignored.files_copied as u32
    } else {
        0
    };

    let copied_changes = CopiedChangesSummary {
        staged_copied: report.unignored_copy.files_copied as u32,
        modified_copied: dirty_modified,
        untracked_copied: ignored_files_copied + dirty_untracked,
        deletions_applied: dirty_deleted,
        warnings,
    };

    let absolute_path = report.worktree_path.to_string_lossy().to_string();

    let elapsed_ms = start.elapsed().as_millis() as u64;
    tracing::debug!(
        session_id = %req.new_session_id,
        source = %req.source_worktree_path,
        path = %absolute_path,
        commit = %report.commit,
        copy_mode = ?req.copy_mode,
        files_copied = copied_changes.staged_copied,
        elapsed = ?start.elapsed(),
        "forked worktree created"
    );
    tracing::info!(
        target: WORKTREE_LOG,
        session_id = %req.new_session_id,
        source = %req.source_worktree_path,
        path = %absolute_path,
        commit = %report.commit,
        copy_mode = ?req.copy_mode,
        files_copied = copied_changes.staged_copied,
        modified_copied = copied_changes.modified_copied,
        untracked_copied = copied_changes.untracked_copied,
        deletions_applied = copied_changes.deletions_applied,
        elapsed_ms,
        "FORK_OK: forked worktree created successfully"
    );

    WorktreeStatus::Created {
        session_id: req.new_session_id.clone(),
        worktree_path: absolute_path,
        commit: report.commit,
        source_git_root: git_root.map(|p| p.to_string_lossy().to_string()),
        copied_changes: Some(copied_changes),
    }
}

/// Synchronously create a worktree from another worktree and return the response.
/// Use this when you need the result directly (no notification stream).
pub async fn create_worktree_from_worktree_sync(
    req: &CreateWorktreeFromWorktreeRequest,
) -> Result<CreateWorktreeFromWorktreeResponse> {
    let source_path = Path::new(&req.source_worktree_path);
    let source_git_root = find_git_root_from_path(source_path)
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    let worktree_path_str =
        resolve_fork_worktree_path(source_path, &req.new_session_id, req.label.as_deref())?;

    // Check if worktree already exists
    if tokio::fs::metadata(&worktree_path_str).await.is_ok() {
        let commit = git_cli(Path::new(&worktree_path_str), &["rev-parse", "HEAD"])
            .await
            .ok();
        return Ok(CreateWorktreeFromWorktreeResponse {
            status: "exists".to_string(),
            new_session_id: req.new_session_id.clone(),
            worktree_path: worktree_path_str,
            commit,
            copied_changes: None,
            source_git_root,
        });
    }

    // Map WorktreeCopyMode to xai_fast_worktree::WorkingTreeMode
    let working_tree_mode = match req.copy_mode {
        WorktreeCopyMode::Dirty => WorkingTreeMode::PreserveWorkingTree,
        WorktreeCopyMode::Clean => WorkingTreeMode::CleanAll,
    };

    let source_worktree_path = req.source_worktree_path.clone();
    let dest_path = worktree_path_str.clone();
    let git_ref = req.git_ref.clone();
    let requested_type = req.worktree_type.unwrap_or(WorktreeType::Linked);
    let git_dir_is_directory = std::path::Path::new(&req.source_worktree_path)
        .join(".git")
        .is_dir();
    let creation_mode = if requested_type == WorktreeType::Standalone {
        if git_dir_is_directory {
            WorktreeType::Standalone
        } else {
            tracing::warn!(
                target: WORKTREE_LOG,
                session_id = %req.new_session_id,
                source = %req.source_worktree_path,
                requested_type = ?requested_type,
                git_dir_is_directory,
                "WORKTREE_MODE_FALLBACK: standalone requested from linked worktree source; falling back to linked"
            );
            tracing::warn!(
                "Standalone mode requested but source has a .git file (linked worktree), \
                 falling back to Linked mode"
            );
            WorktreeType::Linked
        }
    } else {
        requested_type
    };
    tracing::info!(
        target: WORKTREE_LOG,
        session_id = %req.new_session_id,
        source = %req.source_worktree_path,
        requested_type = ?requested_type,
        final_creation_mode = ?creation_mode,
        git_dir_is_directory,
        git_ref = req.git_ref.as_deref().unwrap_or("HEAD"),
        "WORKTREE_MODE_RESOLVED: sync fork worktree creation mode"
    );
    let session_id_for_builder = req.new_session_id.clone();
    let btrfs_delegate = btrfs_delegate_from_env();
    let label_for_meta = label_from_path(&worktree_path_str);
    let label_metadata = build_label_metadata(&label_for_meta, false);
    let report = tokio::task::spawn_blocking(move || {
        let mut builder = WorktreeBuilder::new(&source_worktree_path, &dest_path)
            .working_tree_mode(working_tree_mode)
            .ignored_files_mode(IgnoredFilesMode::Skip)
            .creation_mode(to_creation_mode(creation_mode))
            .worktree_kind(xai_fast_worktree::WorktreeKind::Fork)
            .session_id(session_id_for_builder)
            .metadata(label_metadata);

        // Apply git_ref if specified (branch, tag, or commit SHA)
        if let Some(ref git_ref) = git_ref {
            builder = builder.git_ref(git_ref);
        }

        if let Some(delegate) = btrfs_delegate {
            builder = builder.btrfs_delegate(delegate);
        }

        builder.create()
    })
    .await
    .map_err(|e| anyhow::anyhow!("Worktree creation task failed: {}", e))??;

    // Build CopiedChangesSummary from the report
    let (dirty_modified, dirty_untracked, dirty_deleted) =
        if req.copy_mode == WorktreeCopyMode::Dirty {
            report
                .unignored_copy
                .dirty_files
                .as_ref()
                .map(|d| {
                    (
                        d.modified_files as u32,
                        d.untracked_files as u32,
                        d.deleted_files as u32,
                    )
                })
                .unwrap_or((0, 0, 0))
        } else {
            (0, 0, 0)
        };

    let mut warnings = report.unignored_copy.issues;
    let ignored_files_copied = if let Some(ignored) = report.ignored_copy {
        warnings.extend(ignored.issues);
        ignored.files_copied as u32
    } else {
        0
    };

    let copied_changes = CopiedChangesSummary {
        staged_copied: report.unignored_copy.files_copied as u32,
        modified_copied: dirty_modified,
        untracked_copied: ignored_files_copied + dirty_untracked,
        deletions_applied: dirty_deleted,
        warnings,
    };

    Ok(CreateWorktreeFromWorktreeResponse {
        status: "created".to_string(),
        new_session_id: req.new_session_id.clone(),
        worktree_path: report.worktree_path.to_string_lossy().to_string(),
        commit: Some(report.commit),
        copied_changes: Some(copied_changes),
        source_git_root,
    })
}

// ============================================================================
// Apply Worktree
// ============================================================================

#[derive(Debug)]
struct ApplyContext {
    base_commit: String,
    changed_files: Vec<GitFileChange>,
}

/// Get all files that differ between the main repo and the worktree.
async fn get_apply_context(worktree_path: &str) -> Result<ApplyContext> {
    let wt_path = worktree_path.to_string();

    tokio::task::spawn_blocking(move || {
        let wt_repo = Repository::open(&wt_path)?;
        let wt_head = get_head_commit(&wt_repo)?;

        // Find the main repository (via commondir)
        let main_git_dir = wt_repo.commondir().to_path_buf();
        let main_repo_path = main_git_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Invalid git repository structure"))?;
        let main_repo = Repository::open(main_repo_path)?;
        let main_head = get_head_commit(&main_repo)?;

        let mut changed_files = Vec::new();
        let mut seen_paths = HashSet::new();

        // 1. Get committed changes: diff from main repo HEAD to worktree HEAD
        let main_oid = Oid::from_str(&main_head)?;
        let wt_oid = Oid::from_str(&wt_head)?;
        let main_tree = wt_repo.find_commit(main_oid)?.tree()?;
        let wt_tree = wt_repo.find_commit(wt_oid)?.tree()?;

        // Compare committed changes: main HEAD to worktree HEAD
        let mut opts = DiffOptions::new();
        let diff = wt_repo.diff_tree_to_tree(Some(&main_tree), Some(&wt_tree), Some(&mut opts))?;

        for (idx, delta) in diff.deltas().enumerate() {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            if path.is_empty() {
                continue;
            }

            let change_type = change_type_from_git2_delta(delta.status());

            let (additions, deletions) = git2::Patch::from_diff(&diff, idx)
                .ok()
                .flatten()
                .and_then(|p| p.line_stats().ok())
                .map(|(_, a, d)| (a as u64, d as u64))
                .unwrap_or((0, 0));

            seen_paths.insert(path.clone());
            changed_files.push(GitFileChange {
                path,
                old_path: None,
                change_type,
                staged: None,
                additions,
                deletions,
                patch: None,
                patch_bytes: None,
                patch_lines: None,
                old_text: None,
                new_text: None,
            });
        }

        // 2. Get dirty (uncommitted) changes in worktree
        let mut dirty_opts = DiffOptions::new();
        dirty_opts.include_untracked(true);

        if let Ok(dirty_diff) =
            wt_repo.diff_tree_to_workdir_with_index(Some(&wt_tree), Some(&mut dirty_opts))
        {
            for delta in dirty_diff.deltas() {
                let path = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();

                if path.is_empty() || seen_paths.contains(&path) {
                    continue;
                }

                let change_type = change_type_from_git2_delta(delta.status());

                seen_paths.insert(path.clone());
                changed_files.push(GitFileChange {
                    path,
                    old_path: None,
                    change_type,
                    staged: None,
                    additions: 0,
                    deletions: 0,
                    patch: None,
                    patch_bytes: None,
                    patch_lines: None,
                    old_text: None,
                    new_text: None,
                });
            }
        }

        Ok(ApplyContext {
            base_commit: main_head,
            changed_files,
        })
    })
    .await?
}

async fn get_file_at_commit(worktree_path: &str, commit: &str, path: &str) -> Option<String> {
    git_cli(
        Path::new(worktree_path),
        &["show", &format!("{}:{}", commit, path)],
    )
    .await
    .ok()
}

async fn apply_file_content(dest: &Path, content: Option<&String>) -> bool {
    match content {
        Some(data) => {
            if let Some(parent) = dest.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            tokio::fs::write(dest, data).await.is_ok()
        }
        None => {
            let _ = tokio::fs::remove_file(dest).await;
            true
        }
    }
}

pub async fn apply_worktree(req: &ApplyWorktreeRequest) -> Result<ApplyWorktreeResponse> {
    let worktree_path = &req.worktree_path;
    let git_root = find_main_repo_root_from_path(Path::new(worktree_path))?;
    let git_root_str = git_root.to_string_lossy().to_string();
    let ctx = get_apply_context(worktree_path).await?;

    if ctx.changed_files.is_empty() {
        return Ok(ApplyWorktreeResponse::Success {
            files: vec![],
            git_root: git_root_str,
        });
    }

    let mut files = Vec::new();
    let mut conflicts = Vec::new();

    for file_change in ctx.changed_files {
        let worktree_file = Path::new(worktree_path).join(&file_change.path);
        let main_file = git_root.join(&file_change.path);
        let theirs = tokio::fs::read_to_string(&worktree_file).await.ok();

        if req.mode == ApplyMode::Overwrite {
            if apply_file_content(&main_file, theirs.as_ref()).await {
                files.push(file_change);
            }
            continue;
        }

        // Merge mode
        let base = get_file_at_commit(worktree_path, &ctx.base_commit, &file_change.path).await;
        let ours = tokio::fs::read_to_string(&main_file).await.ok();

        if base == ours {
            if apply_file_content(&main_file, theirs.as_ref()).await {
                files.push(file_change);
            }
        } else if base != theirs {
            conflicts.push(FileConflict {
                path: file_change.path,
                change_type: file_change.change_type,
                base,
                ours,
                theirs,
            });
        }
    }

    if conflicts.is_empty() {
        Ok(ApplyWorktreeResponse::Success {
            files,
            git_root: git_root_str,
        })
    } else {
        Ok(ApplyWorktreeResponse::Conflicts { files, conflicts })
    }
}

// ============================================================================
// Jujutsu workspace isolation
// ============================================================================

use crate::session::git::{jj_cli, jj_cli_mut};

/// Short commit ID of the working-copy commit in a jj workspace.
async fn jj_commit_id(cwd: &Path) -> Option<String> {
    jj_cli(
        cwd,
        &[
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "commit_id.shortest(12)",
        ],
    )
    .await
    .ok()
}

/// Create a jj workspace for subagent isolation.
pub async fn create_jj_workspace(
    req: &CreateWorktreeFromWorktreeRequest,
) -> Result<CreateWorktreeFromWorktreeResponse> {
    let source_path = Path::new(&req.source_worktree_path);
    let source_git_root = find_git_root_from_path(source_path)
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    let dest = resolve_fork_worktree_path(source_path, &req.new_session_id, req.label.as_deref())?;

    if tokio::fs::metadata(&dest).await.is_ok() {
        let commit = jj_commit_id(Path::new(&dest)).await;
        return Ok(CreateWorktreeFromWorktreeResponse {
            status: "exists".to_string(),
            new_session_id: req.new_session_id.clone(),
            worktree_path: dest,
            commit,
            copied_changes: None,
            source_git_root,
        });
    }

    let name = req.new_session_id.replace(['/', '\\', '.'], "-");

    // Ensure parent directory exists -- jj workspace add doesn't create it.
    if let Some(parent) = Path::new(&dest).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tracing::info!(
        source = %req.source_worktree_path,
        dest = %dest,
        name = %name,
        "creating jj workspace"
    );

    jj_cli_mut(
        source_path,
        &["workspace", "add", &dest, "--name", &name, "-r", "@"],
    )
    .await
    .map_err(|e| anyhow::anyhow!("jj workspace add failed: {e}"))?;

    let commit = jj_commit_id(Path::new(&dest)).await;
    tracing::info!(dest = %dest, commit = ?commit, "jj workspace created");

    Ok(CreateWorktreeFromWorktreeResponse {
        status: "created".to_string(),
        new_session_id: req.new_session_id.clone(),
        worktree_path: dest,
        commit,
        copied_changes: None,
        source_git_root,
    })
}

/// Remove a jj workspace: forget + delete directory.
pub async fn remove_jj_workspace(workspace_path: &str) -> Result<()> {
    let path = Path::new(workspace_path);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .replace(['/', '\\', '.'], "-");

    if let Ok(root) = find_git_root_from_path(path) {
        let _ = jj_cli_mut(&root, &["workspace", "forget", &name]).await;
    }

    if path.exists() {
        tracing::info!(path = %workspace_path, "removing jj workspace directory");
        tokio::fs::remove_dir_all(path).await?;
    }
    Ok(())
}

// ============================================================================
// Resume / Rehydrate types (types only -- impl stays in shell)
// ============================================================================

/// Request to resume an existing session in a fresh worktree.
///
/// ACP equivalent of `grok -w -r <session_id>` (optionally with `--ref`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionInWorktreeRequest {
    pub session_id: String,
    pub source_cwd: String,
    #[serde(default = "default_copy_mode")]
    pub copy_mode: WorktreeCopyMode,
    /// Falls back to the agent's configured default when absent.
    #[serde(default)]
    pub worktree_type: Option<WorktreeType>,
    /// Whether to restore the session's original working-tree state in the worktree.
    #[serde(default)]
    pub restore_code: Option<bool>,
    /// Branch, tag, or commit to base the worktree on (CLI `--ref` / `--worktree-ref`).
    /// When set, the worktree is a clean checkout of this ref (dirty overlay is ignored).
    #[serde(default)]
    pub git_ref: Option<String>,
}

/// Response from `x.ai/git/worktree/resume_session`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionInWorktreeResponse {
    /// The *forked* session ID (not the original) -- load this in the worktree.
    pub session_id: String,
    pub worktree_path: String,
    /// Working directory inside the worktree, preserving any subdirectory
    /// offset from `source_cwd`.
    pub effective_cwd: String,
    pub remote_restored: bool,
    pub parent_session_id: String,
    pub chat_messages_copied: usize,
    pub updates_copied: usize,
    /// Whether working-tree state was restored.
    #[serde(default)]
    pub code_restored: bool,
    /// Human-readable summary of what was restored (for display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_summary: Option<String>,
    /// Restoration depth: `Full` (HEAD + staged/unstaged/untracked),
    /// `HeadOnly` (HEAD checkout only), or `None` when no
    /// restore was attempted. Serialises as `"full"` / `"head_only"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_degree: Option<crate::session::git::RestoreDegree>,
}

/// Request to rehydrate a session in a worktree, preserving the original
/// session identity. Designed for host recovery after restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RehydrateSessionRequest {
    /// Original session ID to restore (preserved as-is).
    pub session_id: String,
    /// The CWD the session was using.
    pub source_cwd: String,
    /// Path to the main git repository root.
    pub repo_root: String,
    /// Worktree path to recreate.
    #[serde(default)]
    pub worktree_path: Option<String>,
}

/// Response from `x.ai/session/rehydrate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RehydrateSessionResponse {
    /// Same session ID as the request (identity preserved).
    pub session_id: String,
    /// Same worktree path as the request (recreated in place).
    pub worktree_path: String,
    /// Same CWD as the request.
    pub effective_cwd: String,
    pub codebase_restored: bool,
    pub session_state_restored: bool,
    pub memory_restored: bool,
    pub warnings: Vec<String>,
}

// ============================================================================
// Worktree Management / DB
// ============================================================================

use xai_fast_worktree::{
    AutoGcOptions, DbStats, GcOptions, GcReport, ListFilter, WorktreeAutoGcLayer, WorktreeDb,
    WorktreeKind, WorktreeRecord, gc_worktrees as fw_gc_worktrees, maybe_auto_gc,
    rebuild_worktree_db, resolve_grok_home, resolve_worktree_auto_gc_from_layers,
};

pub fn open_db() -> Result<WorktreeDb> {
    WorktreeDb::open_default()
}

pub fn list_worktrees(
    repo: Option<&str>,
    types: &[String],
    include_all: bool,
) -> Result<Vec<WorktreeRecord>> {
    let db = open_db()?;

    let kind = if types.len() == 1 {
        Some(WorktreeKind::from_str_lossy(&types[0]))
    } else {
        None
    };

    let filter = ListFilter {
        repo_name: repo.map(str::to_owned),
        source_repo: None,
        kind,
        status: None,
        include_dead: include_all,
    };

    let mut records = db.list(&filter)?;

    // Client-side filter when multiple --type values are given.
    if types.len() > 1 {
        records.retain(|r| types.iter().any(|t| t == r.kind.as_str()));
    }

    Ok(records)
}

pub fn show_worktree(id_or_path: &str) -> Result<Option<WorktreeRecord>> {
    let db = open_db()?;
    db.get(id_or_path)
}

pub fn gc_worktrees_mgmt(
    dry_run: bool,
    max_age_secs: Option<i64>,
    force: bool,
) -> Result<GcReport> {
    let db = open_db()?;
    let opts = GcOptions {
        max_age_secs,
        force,
        dry_run,
        ..Default::default()
    };
    fw_gc_worktrees(&db, &opts)
}

/// Map settings → resolve layer (shared by shell + workspace).
pub fn worktree_auto_gc_layer_from_settings(
    s: &xai_grok_config_types::WorktreeAutoGcSettings,
) -> WorktreeAutoGcLayer {
    use std::collections::BTreeMap;
    use xai_grok_config_types::WorktreeKindMaxAge;

    let max_age_by_kind = s
        .max_age_by_kind
        .as_ref()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let kind = WorktreeKind::from_str_opt(k)?;
                    let age = match v {
                        WorktreeKindMaxAge::Secs(n) => Some(*n),
                        WorktreeKindMaxAge::Never => None,
                    };
                    Some((kind, age))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    WorktreeAutoGcLayer {
        enabled: s.enabled,
        max_age_secs: s.max_age_secs,
        min_interval_secs: s.min_interval_secs,
        dry_run: s.dry_run,
        include_orphan_snapshots: s.include_orphan_snapshots,
        max_age_by_kind,
        include_rebuild: s.include_rebuild,
        rebuild_min_interval_secs: s.rebuild_min_interval_secs,
    }
}

/// Env + `$GROK_HOME/config.toml` only — **`remote=None` is intentional**.
///
/// Workspace handle startup has no remote-settings blob (unlike shell agent
/// init, which resolves env > TOML > remote). Remote `worktree_auto_gc`
/// kill-switch / staged rollout therefore does not apply on pure-workspace
/// processes; use `GROK_WORKTREE_AUTO_GC=0` / `GROK_WORKTREE_AUTO_GC_DRY_RUN=1`
/// or local TOML until remote is plumbed into `make_workspace_handle`.
fn resolve_worktree_auto_gc_local() -> xai_fast_worktree::ResolvedWorktreeAutoGc {
    use xai_grok_config_types::WorktreeAutoGcSettings;

    let local = if let Ok(home) = resolve_grok_home() {
        let path = home.join("config.toml");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(root) = text.parse::<toml::Value>()
        {
            root.get("worktree")
                .and_then(|w| w.get("auto_gc"))
                // toml::Value only deserializes by value (no &Value Deserializer).
                .and_then(|v| WorktreeAutoGcSettings::deserialize(v.clone()).ok())
        } else {
            None
        }
    } else {
        None
    };
    let layer = local.as_ref().map(worktree_auto_gc_layer_from_settings);
    // remote=None: see doc comment on this function.
    resolve_worktree_auto_gc_from_layers(layer.as_ref(), None)
}

/// Sync auto-GC for handle startup (caller must `spawn_blocking`).
pub fn run_auto_gc_best_effort() {
    let opts = AutoGcOptions::from_resolved(resolve_worktree_auto_gc_local());
    if let Err(e) = WorktreeDb::open_default().and_then(|db| maybe_auto_gc(&db, &opts)) {
        tracing::warn!(error = %e, "auto worktree gc failed");
    }
}

pub fn worktree_db_stats() -> Result<DbStats> {
    let db = open_db()?;
    db.stats()
}

pub fn worktree_db_rebuild() -> Result<xai_fast_worktree::RebuildReport> {
    let home = resolve_grok_home()?;
    let db = WorktreeDb::open(&home)?;
    rebuild_worktree_db(&db, &home)
}

pub fn worktree_db_path() -> Result<std::path::PathBuf> {
    let home = resolve_grok_home()?;
    Ok(home.join("worktrees.db"))
}

/// Resolve an ID-or-path string to a worktree path via DB lookup,
/// falling back to treating it as a filesystem path.
pub fn resolve_worktree_by_id_or_path(id_or_path: &str) -> Result<Option<std::path::PathBuf>> {
    let db = open_db()?;
    if let Some(rec) = db.get(id_or_path)? {
        return Ok(Some(rec.path));
    }
    let p = std::path::PathBuf::from(id_or_path);
    if p.exists() { Ok(Some(p)) } else { Ok(None) }
}

// ============================================================================
// Repo-wide candidate enumeration (for worktree resume)
// ============================================================================

/// Build a deduplicated, deterministically-ordered list of candidate cwds
/// for the same repository as `current_cwd`.
///
/// Order: exact cwd first, then main repo root (if different), then all
/// tracked worktree paths for the same source repo, sorted alphabetically.
pub fn candidate_worktree_cwds_for_same_repo(current_cwd: &std::path::Path) -> Result<Vec<String>> {
    let main_root = find_main_repo_root_from_path(current_cwd)?;
    let db_records = match open_db() {
        Ok(db) => {
            let filter = xai_fast_worktree::ListFilter {
                source_repo: Some(main_root.clone()),
                include_dead: true,
                ..Default::default()
            };
            db.list(&filter).unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };
    let fs_paths = scan_worktree_dirs_on_disk(&main_root);
    Ok(build_candidate_list(
        &current_cwd.to_string_lossy(),
        &main_root.to_string_lossy(),
        &db_records,
        &fs_paths,
    ))
}

/// Scan `~/.grok/worktrees/<repo_name>/` for subdirectories not tracked
/// in the DB. Returns a sorted list of absolute directory paths.
fn scan_worktree_dirs_on_disk(main_repo_root: &std::path::Path) -> Vec<String> {
    let base = worktree_base_dir(main_repo_root);
    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut paths: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        // Only include directories that look like git worktrees.
        .filter(|e| e.path().join(".git").exists())
        .filter_map(|e| {
            dunce::canonicalize(e.path())
                .ok()
                .and_then(|p| p.to_str().map(String::from))
        })
        .collect();
    paths.sort();
    paths
}

/// Pure logic for candidate list construction. Separated for deterministic
/// testing without git repos or a real worktree DB.
///
/// `fs_paths` contains directories discovered via filesystem scan of the
/// worktree base dir. These act as a fallback for worktrees not tracked
/// in the DB (e.g. created before the DB existed or after DB corruption).
pub fn build_candidate_list(
    current_cwd: &str,
    main_repo_root: &str,
    db_records: &[xai_fast_worktree::WorktreeRecord],
    fs_paths: &[String],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();

    let mut add = |path: &str| {
        if !path.is_empty() && seen.insert(path.to_owned()) {
            candidates.push(path.to_owned());
        }
    };

    add(current_cwd);
    add(main_repo_root);

    let mut wt_paths: Vec<&str> = db_records
        .iter()
        .map(|r| r.path.to_str().unwrap_or_default())
        .collect();
    wt_paths.sort();
    for p in wt_paths {
        add(p);
    }

    for p in fs_paths {
        add(p);
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Defaults preserve today's behavior: 30s request, 5s connect. These are
    /// the values the pager's factory closure feeds into the snapshot helper
    /// (which stores them verbatim), so this guards the helper RPC timeout
    /// defaults without referencing the delegate crate.
    #[test]
    fn default_status_config_timeouts_match_legacy_hardcoded_values() {
        let cfg = crate::StatusConfig::default();
        assert_eq!(cfg.agent_rpc_timeout, Duration::from_secs(30));
        assert_eq!(cfg.agent_connect_timeout, Duration::from_secs(5));
    }

    // ── snapshot_and_remove_subagent_worktree ────────────────────────────

    /// Run a git command in `dir` and return trimmed stdout (test-only helper).
    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Create a source repo (one committed file) plus a worktree of it.
    fn repo_with_worktree(temp: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        use xai_test_utils::git::{git_commit_all, init_git_repo};
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::write(repo.join("tracked.txt"), "original").unwrap();
        git_commit_all(&repo, "initial");

        let wt = temp.path().join("wt");
        WorktreeBuilder::new(&repo, &wt).create().unwrap();
        (repo, wt)
    }

    /// Happy path: the snapshot ref resolves and the worktree dir is removed.
    #[tokio::test]
    async fn snapshot_and_remove_captures_then_deletes() {
        xai_test_utils::require_git!();
        let temp = tempfile::TempDir::new().unwrap();
        let (repo, wt) = repo_with_worktree(&temp);

        // Dirty the worktree so the snapshot has real working state to capture.
        std::fs::write(wt.join("tracked.txt"), "edited").unwrap();
        std::fs::write(wt.join("untracked.txt"), "brand new").unwrap();

        let ref_name = "refs/grok/subagents/dispose-1";
        let returned = snapshot_and_remove_subagent_worktree(&wt, &repo, ref_name)
            .await
            .unwrap();

        // Returns the ref name to persist, the ref resolves from the main repo,
        // and the worktree directory is gone.
        assert_eq!(returned, ref_name);
        assert!(!git_out(&repo, &["rev-parse", ref_name]).is_empty());
        assert!(
            !wt.exists(),
            "worktree dir should be removed after snapshot"
        );
    }

    /// Full cycle (LINKED worktree): snapshot+remove, then rehydrate restores
    /// content byte-for-byte.
    #[tokio::test]
    async fn snapshot_and_remove_then_rehydrate_round_trips() {
        xai_test_utils::require_git!();
        let temp = tempfile::TempDir::new().unwrap();
        let (repo, wt) = repo_with_worktree(&temp);

        std::fs::write(wt.join("tracked.txt"), "edited").unwrap();
        std::fs::write(wt.join("untracked.txt"), "brand new").unwrap();

        let ref_name = "refs/grok/subagents/dispose-2";
        snapshot_and_remove_subagent_worktree(&wt, &repo, ref_name)
            .await
            .unwrap();
        assert!(!wt.exists());

        // Layer B rehydrate from the same ref recreates the exact working state.
        let restored = rehydrate_subagent_worktree(&wt, &repo, ref_name, Some("dispose-2"))
            .await
            .unwrap();
        assert_eq!(restored, wt);
        assert_eq!(
            std::fs::read(wt.join("tracked.txt")).unwrap(),
            b"edited",
            "tracked edit must survive the snapshot/rehydrate round trip"
        );
        assert_eq!(
            std::fs::read(wt.join("untracked.txt")).unwrap(),
            b"brand new",
            "untracked file must survive the snapshot/rehydrate round trip"
        );
    }

    /// Full cycle (STANDALONE worktree, the production default): the snapshot
    /// lives in the worktree's OWN `.git`, so it would be destroyed on removal
    /// unless transferred into the source repo. This is the exact case the live
    /// E2E caught; it FAILS without `transfer_snapshot_to_repo`.
    #[tokio::test]
    async fn snapshot_standalone_worktree_durable_after_removal_round_trips() {
        xai_test_utils::require_git!();
        use xai_test_utils::git::{git_commit_all, init_git_repo};
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::write(repo.join("tracked.txt"), "original").unwrap();
        git_commit_all(&repo, "initial");

        // Standalone: independent `.git` (own object store + refs).
        let wt = temp.path().join("subagent-standalone");
        WorktreeBuilder::new(&repo, &wt)
            .standalone(true)
            .create()
            .unwrap();
        std::fs::write(wt.join("tracked.txt"), "edited").unwrap();
        std::fs::write(wt.join("untracked.txt"), "brand new").unwrap();

        let ref_name = "refs/grok/subagents/standalone-1";
        let returned = snapshot_subagent_worktree(&wt, &repo, ref_name)
            .await
            .unwrap();
        assert_eq!(returned, ref_name);

        // The ref must be durable in the SOURCE repo even though the snapshot was
        // created in the standalone's own `.git`.
        assert!(
            !git_out(&repo, &["rev-parse", &format!("{ref_name}^{{commit}}")]).is_empty(),
            "snapshot ref must resolve in the source repo after transfer"
        );

        remove_subagent_worktree(&wt).await.unwrap();
        assert!(!wt.exists());
        // Ref still resolves after the standalone `.git` is gone (the bug).
        assert!(!git_out(&repo, &["rev-parse", ref_name]).is_empty());

        // Layer B rehydrate resolves the ref against the source repo.
        let restored = rehydrate_subagent_worktree(&wt, &repo, ref_name, Some("standalone-1"))
            .await
            .unwrap();
        assert_eq!(restored, wt);
        assert_eq!(
            std::fs::read(wt.join("tracked.txt")).unwrap(),
            b"edited",
            "tracked edit must survive standalone snapshot/rehydrate"
        );
        assert_eq!(
            std::fs::read(wt.join("untracked.txt")).unwrap(),
            b"brand new",
            "untracked file must survive standalone snapshot/rehydrate"
        );
    }

    /// Invariant: a failed snapshot must NOT remove the worktree directory.
    #[tokio::test]
    async fn snapshot_failure_preserves_worktree() {
        xai_test_utils::require_git!();
        let temp = tempfile::TempDir::new().unwrap();

        // A plain directory (no git HEAD) makes `snapshot_worktree_to_ref` fail,
        // so removal must never run.
        let not_a_worktree = temp.path().join("plain");
        std::fs::create_dir(&not_a_worktree).unwrap();
        std::fs::write(not_a_worktree.join("keep.txt"), "precious").unwrap();

        let result = snapshot_and_remove_subagent_worktree(
            &not_a_worktree,
            &not_a_worktree,
            "refs/grok/subagents/dispose-3",
        )
        .await;

        assert!(result.is_err(), "snapshot of a non-worktree must fail");
        assert!(
            not_a_worktree.join("keep.txt").exists(),
            "directory must be preserved when the snapshot fails"
        );
    }

    // ── worktree_record_for_cwd / touch_worktree_for_cwd ─────────────────

    // Crate-shared env lock + env guards bundled as ONE value so the env
    // restores before the lock releases by struct field order (see lib.rs),
    // regardless of how the caller binds the fixture's return.
    use crate::LockedTestEnv;

    /// Point `GROK_HOME` at an isolated tempdir (`resolve_grok_home` re-reads
    /// the env per call by design) and register one worktree record at
    /// `<home>/worktrees/repo/wt` with no `last_accessed_at`.
    ///
    /// Returns `(env, home, worktree dir)`; the [`LockedTestEnv`] holds the lock
    /// and restores `GROK_HOME` on drop (before releasing the lock), so the
    /// caller may bind it any way.
    fn worktree_db_fixture(
        temp: &tempfile::TempDir,
    ) -> (LockedTestEnv, std::path::PathBuf, std::path::PathBuf) {
        // Canonicalize so macOS /var -> /private/var agrees between the stored
        // record path and `db.get`'s canonicalized query path.
        let root = dunce::canonicalize(temp.path()).unwrap();
        let home = root.join("grok-home");
        let wt = home.join("worktrees").join("repo").join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        // Acquire the lock, then set the env under it (LockedTestEnv restores the
        // env before releasing the lock on drop).
        let env = LockedTestEnv::lock().set("GROK_HOME", &home);

        let db = WorktreeDb::open(&home).unwrap();
        let record = WorktreeRecord {
            id: "wt".to_string(),
            path: wt.clone(),
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
            status: xai_fast_worktree::WorktreeStatus::Alive,
            metadata: Some(build_label_metadata("my-label", true)),
        };
        db.register(&record).unwrap();
        (env, home, wt)
    }

    #[test]
    fn touch_worktree_for_cwd_sets_last_accessed_for_nested_cwd() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_env, home, wt) = worktree_db_fixture(&temp);

        let nested = wt.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        touch_worktree_for_cwd(&nested.to_string_lossy());

        let rec = WorktreeDb::open(&home)
            .unwrap()
            .get_by_id("wt")
            .unwrap()
            .unwrap();
        assert!(
            rec.last_accessed_at.is_some(),
            "ancestor walk must resolve the record and write last_accessed_at"
        );
    }

    #[test]
    fn touch_worktree_for_cwd_ignores_non_worktree_paths() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_env, home, _wt) = worktree_db_fixture(&temp);

        // Outside <home>/worktrees entirely.
        touch_worktree_for_cwd("/definitely/not/a/worktree");
        // The worktrees dir itself is excluded by the walk.
        touch_worktree_for_cwd(&home.join("worktrees").to_string_lossy());

        let rec = WorktreeDb::open(&home)
            .unwrap()
            .get_by_id("wt")
            .unwrap()
            .unwrap();
        assert!(
            rec.last_accessed_at.is_none(),
            "non-worktree cwds must not touch any record"
        );
    }

    #[test]
    fn lookup_worktree_label_resolves_from_nested_cwd() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_env, _home, wt) = worktree_db_fixture(&temp);

        let nested = wt.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            lookup_worktree_label(&nested.to_string_lossy()).as_deref(),
            Some("my-label")
        );
        assert_eq!(lookup_worktree_label("/elsewhere"), None);
    }

    /// A cancelled fork's cleanup must remove the partial worktree directory AND
    /// deregister it from the source repo's `.git/worktrees/`, not merely `rm -rf`
    /// the directory. delegate is `None` here, exercising the direct removal path.
    #[tokio::test]
    async fn cleanup_cancelled_worktree_removes_dir_and_deregisters() {
        xai_test_utils::require_git!();
        use xai_test_utils::git::{git_commit_all, init_git_repo};

        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::write(repo.join("tracked.txt"), "original").unwrap();
        git_commit_all(&repo, "initial");

        // Unique basename → unique DB id, so a concurrent open_default writer
        // can't clobber this row (GrokHomeFixture is not visible across crates).
        let wt = temp.path().join("fork-cancel-wt");
        WorktreeBuilder::new(&repo, &wt).create().unwrap();

        // Capture the `.git/worktrees/<name>` registration the cleanup must drop.
        let git_ptr = std::fs::read_to_string(wt.join(".git")).unwrap();
        let gitdir = git_ptr
            .trim()
            .strip_prefix("gitdir: ")
            .expect("linked worktree has a gitdir pointer");
        let reg_dir = {
            let p = Path::new(gitdir);
            if p.is_relative() {
                wt.join(p)
            } else {
                p.to_path_buf()
            }
        };
        assert!(reg_dir.exists(), "precondition: registration exists");

        cleanup_cancelled_worktree(&wt.to_string_lossy()).await;

        assert!(!wt.exists(), "cancelled worktree dir must be removed");
        assert!(
            !reg_dir.exists(),
            "`.git/worktrees/<name>` registration must be deregistered"
        );
    }

    /// `prepare_worktree_creation` must not leave the in-progress marker set when no
    /// async creation follows: in proxy mode the shell never spawns the async task, so
    /// a marker set in prepare would never clear and would wedge every retry in `Creating`.
    #[tokio::test]
    async fn prepare_does_not_strand_in_progress_marker() {
        xai_test_utils::require_git!();
        use xai_test_utils::git::{git_commit_all, init_git_repo};

        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::write(repo.join("tracked.txt"), "x").unwrap();
        git_commit_all(&repo, "initial");

        // Unique session id so this test's marker can't collide with others
        // sharing the process under `cargo test`.
        let session_id = format!("no-strand-{}", std::process::id());
        // dest does not exist yet → prepare reaches the spawn_task=true branch.
        let dest = temp.path().join("new-wt");
        let req = CreateWorktreeRequest {
            session_id: session_id.clone(),
            source_path: repo.to_string_lossy().into_owned(),
            worktree_path: Some(dest.to_string_lossy().into_owned()),
            copy_mode: WorktreeCopyMode::Dirty,
            git_ref: None,
            copy_ignored_in_background: false,
            ignored_skip_patterns: vec![],
            worktree_type: None,
            label: None,
        };

        let result = prepare_worktree_creation(&req).await;
        assert!(result.spawn_task, "fresh creation must request a spawn");
        assert!(
            matches!(result.response, Ok(CreateWorktreeResponse::Creating { .. })),
            "must report Creating"
        );
        assert!(
            !is_worktree_in_progress(&session_id).await,
            "prepare must not strand the in-progress marker (proxy wedge)"
        );
    }

    /// Records whether the in-progress marker was set at each status notification,
    /// so the test can inspect what the notifier observed mid-creation.
    #[derive(Clone)]
    struct MarkerProbeNotifier {
        session_id: String,
        seen_in_progress: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    }

    #[async_trait::async_trait]
    impl WorktreeNotificationSender for MarkerProbeNotifier {
        async fn send_worktree_status(&self, _progress: WorktreeStatus) {
            let in_progress = is_worktree_in_progress(&self.session_id).await;
            self.seen_in_progress.lock().unwrap().push(in_progress);
        }
    }

    /// `create_worktree_async` must set the in-progress marker before streaming (so a
    /// concurrent prepare dedups) and clear it after. The probe observes it mid-creation.
    #[tokio::test]
    async fn create_worktree_async_holds_marker_during_creation_and_clears_after() {
        xai_test_utils::require_git!();
        use xai_test_utils::git::{git_commit_all, init_git_repo};

        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::write(repo.join("tracked.txt"), "x").unwrap();
        git_commit_all(&repo, "initial");

        let session_id = format!("async-marker-{}", std::process::id());
        let dest = temp.path().join("async-wt");
        let req = CreateWorktreeRequest {
            session_id: session_id.clone(),
            source_path: repo.to_string_lossy().into_owned(),
            worktree_path: Some(dest.to_string_lossy().into_owned()),
            copy_mode: WorktreeCopyMode::Dirty,
            git_ref: None,
            copy_ignored_in_background: false,
            ignored_skip_patterns: vec![],
            worktree_type: None,
            label: None,
        };

        let notifier = MarkerProbeNotifier {
            session_id: session_id.clone(),
            seen_in_progress: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        };

        assert!(
            !is_worktree_in_progress(&session_id).await,
            "precondition: not in progress before creation"
        );

        create_worktree_async(req, notifier.clone(), BackgroundCopyContext::new()).await;

        let observed = notifier.seen_in_progress.lock().unwrap().clone();
        assert!(
            observed.iter().any(|&in_progress| in_progress),
            "marker must be set during creation; observed: {observed:?}"
        );
        assert!(
            !is_worktree_in_progress(&session_id).await,
            "marker must be cleared after create_worktree_async completes"
        );
        assert!(dest.exists(), "worktree should have been created");
    }

    /// Fork-flow mirror of `prepare_does_not_strand_in_progress_marker`:
    /// `prepare_worktree_from_worktree` must not strand the marker in the proxy case.
    #[tokio::test]
    async fn fork_prepare_does_not_strand_in_progress_marker() {
        xai_test_utils::require_git!();
        let temp = tempfile::TempDir::new().unwrap();
        let (_repo, wt) = repo_with_worktree(&temp);

        let new_session_id = format!("fork-no-strand-{}", std::process::id());
        let req = CreateWorktreeFromWorktreeRequest {
            source_worktree_path: wt.to_string_lossy().into_owned(),
            new_session_id: new_session_id.clone(),
            copy_mode: WorktreeCopyMode::Dirty,
            git_ref: None,
            worktree_type: None,
            label: None,
            cancellation_token: None,
            resolved_dest_path: None,
        };

        let result = prepare_worktree_from_worktree(&req).await;
        assert!(result.spawn_task, "fresh fork must request a spawn");
        assert!(
            matches!(result.response, Ok(CreateWorktreeResponse::Creating { .. })),
            "must report Creating"
        );
        assert!(
            !is_worktree_in_progress(&new_session_id).await,
            "fork prepare must not strand the in-progress marker (proxy wedge)"
        );
    }

    /// Counts terminal worktree statuses — one per creator that ran to completion.
    #[derive(Clone)]
    struct TerminalStatusCounter {
        terminal: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl WorktreeNotificationSender for TerminalStatusCounter {
        async fn send_worktree_status(&self, progress: WorktreeStatus) {
            if matches!(
                progress,
                WorktreeStatus::Created { .. }
                    | WorktreeStatus::Error { .. }
                    | WorktreeStatus::Cancelled { .. }
            ) {
                self.terminal
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Two concurrent `create_worktree_async` calls for one session must dedup to
    /// a single creator: the loser bails (no second creation, no spurious terminal
    /// status) and the marker is cleared once the winner finishes.
    #[tokio::test]
    async fn concurrent_create_worktree_async_dedups_to_single_creator() {
        xai_test_utils::require_git!();
        use xai_test_utils::git::{git_commit_all, init_git_repo};

        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::write(repo.join("tracked.txt"), "x").unwrap();
        git_commit_all(&repo, "initial");

        let session_id = format!("async-dedup-{}", std::process::id());
        let dest = temp.path().join("dedup-wt");
        let make_req = || CreateWorktreeRequest {
            session_id: session_id.clone(),
            source_path: repo.to_string_lossy().into_owned(),
            worktree_path: Some(dest.to_string_lossy().into_owned()),
            copy_mode: WorktreeCopyMode::Dirty,
            git_ref: None,
            copy_ignored_in_background: false,
            ignored_skip_patterns: vec![],
            worktree_type: None,
            label: None,
        };
        let notifier = TerminalStatusCounter {
            terminal: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };

        tokio::join!(
            create_worktree_async(make_req(), notifier.clone(), BackgroundCopyContext::new()),
            create_worktree_async(make_req(), notifier.clone(), BackgroundCopyContext::new()),
        );

        assert_eq!(
            notifier.terminal.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "exactly one creator runs to a terminal status; the duplicate bails"
        );
        assert!(
            !is_worktree_in_progress(&session_id).await,
            "marker must be cleared after the winning creator completes"
        );
        assert!(
            dest.exists(),
            "the winning creator must have created the worktree"
        );
    }
}
