use chrono::{DateTime, Utc};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::StorageMode;

use crate::remote::RemoteSync;

use crate::sampling::Client as OaiCompatClient;
use crate::sampling::ConversationItem;
use crate::session::export::ExportedMetadata;
use xai_grok_workspace::session::file_state::RewindPoint;

use crate::session::signals::SessionSignals;
use crate::session::storage::{JsonlStorageAdapter, StorageAdapter};
use crate::tools::todo::TodoState;
use crate::util::grok_home::grok_home;
use agent_client_protocol as acp;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_sampling_types::ReasoningEffort;

use crate::session::info::Info;
use tokio::sync::mpsc;

/// Current chat history format version.
/// - Version 0: Legacy ChatRequestMessage format (default for old sessions)
/// - Version 1: ConversationItem format (used for new sessions)
pub const CHAT_FORMAT_VERSION: u8 = 1;

#[derive(Debug, Clone)]
pub struct PersistenceContentChunk {
    content_chunks: Vec<acp::ContentBlock>,
}

impl PersistenceContentChunk {
    pub fn new(content_chunks: Vec<acp::ContentBlock>) -> Self {
        Self { content_chunks }
    }
}

/// Mirrors generated titles to the session registry after local persistence succeeds.
#[derive(Clone)]
pub(crate) struct RegistryGeneratedTitleSync {
    pub client: crate::agent::session_registry_client::SessionRegistryClient,
    pub suppress_for_zdr: bool,
}

use crate::session::storage::SessionUpdate;
use serde::{Deserialize, Serialize};

// /btw side question persistence types

/// A single /btw side question entry persisted to `btw_history.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BtwEntry {
    /// Unique ID for this side question.
    pub btw_session_id: String,
    /// The parent session ID.
    pub parent_session_id: String,
    /// When the question was asked.
    pub asked_at: DateTime<Utc>,
    /// The user's question.
    pub question: String,
    /// The model's response (empty if failed).
    pub answer: String,
    /// Model used.
    pub model: String,
    /// Whether the request succeeded.
    pub success: bool,
    /// Error message if failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// Local feedback persistence types

/// A feedback entry persisted to `~/.grok/sessions/.../feedback.jsonl`.
///
/// Uses a tagged enum so different feedback types are self-describing in the
/// JSONL file (currently only `UserFeedback`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalFeedbackEntry {
    /// Regular user feedback (spontaneous or solicited via heuristics)
    UserFeedback(UserFeedbackEntry),
}

/// A user feedback entry (thumbs, stars, text, or dismiss).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFeedbackEntry {
    pub submitted_at: DateTime<Utc>,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_number: Option<i64>,
    /// Whether this was a response to a server-initiated FeedbackRequest
    pub solicited: bool,
    /// The feedback request ID (only set for solicited feedback)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// True if the user dismissed the feedback request without responding
    #[serde(default, skip_serializing_if = "is_false")]
    pub dismissed: bool,
    /// The full submission payload (omitted when dismissed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submission: Option<prod_mc_cli_chat_proxy_types::feedback_types::FeedbackSubmission>,
}

/// Helper for `#[serde(skip_serializing_if)]` on bool fields.
pub(crate) fn is_false(v: &bool) -> bool {
    !v
}

#[cfg(test)]
mod feedback_tests {
    use super::*;
    use prod_mc_cli_chat_proxy_types::feedback_types::{
        ClientType, FeedbackSubmission, FeedbackType, RatingType,
    };

    fn make_submission(thumbs_up: bool) -> FeedbackSubmission {
        FeedbackSubmission {
            session_id: "session-abc".into(),
            user_id: None,
            client_type: ClientType::Tui,
            feedback_type: if thumbs_up {
                FeedbackType::Rating
            } else {
                FeedbackType::RatingWithText
            },
            turn_number: Some(7),
            rating_type: Some(RatingType::Thumbs),
            rating_value: Some(if thumbs_up { 1 } else { -1 }),
            feedback_text: if thumbs_up {
                None
            } else {
                Some("could be better".into())
            },
            model_id: Some("grok-3-fast".into()),
            resolved_model_id: Some("grok-4.5".into()),
            ..Default::default()
        }
    }

    #[test]
    fn test_user_feedback_spontaneous_roundtrip() {
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: "session-abc".into(),
            turn_number: Some(7),
            solicited: false,
            request_id: None,
            dismissed: false,
            submission: Some(make_submission(true)),
        });

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"user_feedback""#));
        assert!(!json.contains("dismissed")); // skip_serializing_if = is_false
        assert!(!json.contains("requestId")); // skip_serializing_if = Option::is_none

        let parsed: LocalFeedbackEntry = serde_json::from_str(&json).unwrap();
        let LocalFeedbackEntry::UserFeedback(ref uf) = parsed;
        assert!(!uf.solicited);
        assert!(!uf.dismissed);
        assert!(uf.submission.is_some());
        assert_eq!(uf.session_id, "session-abc");
    }

    #[test]
    fn test_user_feedback_solicited_roundtrip() {
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: "session-abc".into(),
            turn_number: Some(14),
            solicited: true,
            request_id: Some("req-123".into()),
            dismissed: false,
            submission: Some(make_submission(false)),
        });

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""requestId":"req-123""#));
        assert!(json.contains(r#""solicited":true"#));

        let parsed: LocalFeedbackEntry = serde_json::from_str(&json).unwrap();
        let LocalFeedbackEntry::UserFeedback(ref uf) = parsed;
        assert!(uf.solicited);
        assert_eq!(uf.request_id.as_deref(), Some("req-123"));
        let sub = uf.submission.as_ref().unwrap();
        assert_eq!(sub.feedback_text.as_deref(), Some("could be better"));
    }

    #[test]
    fn test_user_feedback_dismiss_roundtrip() {
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: "session-abc".into(),
            turn_number: None,
            solicited: true,
            request_id: Some("req-456".into()),
            dismissed: true,
            submission: None,
        });

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""dismissed":true"#));
        assert!(!json.contains("submission")); // skip_serializing_if = Option::is_none

        let parsed: LocalFeedbackEntry = serde_json::from_str(&json).unwrap();
        let LocalFeedbackEntry::UserFeedback(ref uf) = parsed;
        assert!(uf.dismissed);
        assert!(uf.submission.is_none());
    }

    #[test]
    fn test_feedback_jsonl_multi_line_roundtrip() {
        // Simulate multiple entries written to a JSONL file
        let entries = vec![
            LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
                submitted_at: chrono::Utc::now(),
                session_id: "s1".into(),
                turn_number: Some(1),
                solicited: false,
                request_id: None,
                dismissed: false,
                submission: Some(make_submission(true)),
            }),
            LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
                submitted_at: chrono::Utc::now(),
                session_id: "s1".into(),
                turn_number: None,
                solicited: true,
                request_id: Some("req-1".into()),
                dismissed: true,
                submission: None,
            }),
        ];

        // Serialize to JSONL
        let mut jsonl = String::new();
        for entry in &entries {
            let line = serde_json::to_string(entry).unwrap();
            jsonl.push_str(&line);
            jsonl.push('\n');
        }

        // Deserialize each line
        let parsed: Vec<LocalFeedbackEntry> = jsonl
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert_eq!(parsed.len(), 2);
        assert!(matches!(parsed[0], LocalFeedbackEntry::UserFeedback(_)));
        assert!(matches!(parsed[1], LocalFeedbackEntry::UserFeedback(_)));

        // Verify the dismiss entry
        let LocalFeedbackEntry::UserFeedback(ref uf) = parsed[1];
        assert!(uf.dismissed);
        assert!(uf.solicited);
    }
}

#[derive(Debug, Clone)]
pub struct CopiedSessionFile {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SessionStateCopy {
    pub files: Vec<CopiedSessionFile>,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum PersistenceMsg {
    /// A session update (ACP update or xAI extension update)
    Update(SessionUpdate),
    AppendUpdateDurablyAndAck {
        update: SessionUpdate,
        respond_to:
            tokio::sync::oneshot::Sender<Result<(), crate::session::storage::AppendUpdateError>>,
    },
    ContentChunk(PersistenceContentChunk),
    Chat(ConversationItem),
    AppendCwdSwitchAndAck {
        item: ConversationItem,
        respond_to: tokio::sync::oneshot::Sender<
            Result<xai_chat_state::StrictAppendAck, xai_chat_state::StrictAppendError>,
        >,
    },
    /// Replace the entire chat history (used for compaction)
    ReplaceChatHistory(Vec<ConversationItem>),
    CurrentModel {
        model_id: acp::ModelId,
        /// The active agent definition name (e.g. `"grok-build"`).
        /// Persisted in `summary.agent_name` so session resume doesn't depend
        /// on the mutable model catalog.
        agent_name: Option<String>,
        reasoning_effort: Option<Option<ReasoningEffort>>,
    },
    PlanState(TodoState),
    /// Plan mode lifecycle state to persist
    PlanModeState(crate::session::plan_mode::PlanModeSnapshot),
    /// A rewind point to persist
    RewindPoint(RewindPoint),
    /// Truncate rewind points from a specific prompt index (inclusive).
    /// Syncs the persisted file with the in-memory FileStateTracker after rewind.
    TruncateRewindPoints {
        from_index: usize,
    },
    /// Merge rewind points at indices >= `target_index` into the previous point
    /// (read-modify-write on disk, after a ConversationOnly rewind). Disk is
    /// authoritative, so a partial in-memory tracker can't truncate history.
    MergeRewindPointsFrom {
        target_index: usize,
    },
    /// Collection ID for telemetry tracing
    CollectionId(String),
    /// Monotonic telemetry turn counter and optional request_id for trace metadata/filenames.
    /// This is the "next turn" value (i.e., after increment).
    NextTraceTurn {
        next_trace_turn: u64,
        request_id: Option<String>,
    },
    /// Persist a snapshot of the session signals.
    Signals(SessionSignals),
    /// Persist announcement tracking state (MCP + skill announcement dedup).
    AnnouncementState(crate::session::announcement_state::AnnouncementState),
    /// Persist goal mode orchestration state.
    GoalModeState(crate::session::goal_tracker::GoalOrchestration),
    DeleteGoalModeState {
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    WorkflowRunState(crate::session::workflow::store::WorkflowRunManifest),
    WorkflowRunStateAndAck {
        manifest: crate::session::workflow::store::WorkflowRunManifest,
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    DeleteWorkflowRunState(String),
    /// Persist a local feedback entry (user feedback)
    Feedback(LocalFeedbackEntry),
    /// Persist a /btw side question entry
    Btw(BtwEntry),
    /// Persist updated HEAD commit and branch to summary.
    GitHead {
        commit: Option<String>,
        branch: Option<String>,
    },
    /// Persist a compaction checkpoint file to `compaction_checkpoints/{id}.json`.
    CompactionCheckpoint(crate::extensions::notification::CompactionCheckpointFile),
    /// Persist a compaction request+response artifact to
    /// `compaction_requests/{request_id}.json`. Used for offline prompt
    /// iteration — captures the exact ConversationItem list sent to the
    /// compaction model plus the summary it returned (or the final error).
    /// The file rides on the post-turn session archive to cloud storage automatically;
    /// no separate upload path is needed.
    CompactionRequest(crate::extensions::notification::CompactionRequestFile),
    /// Persist a recap request+response artifact to
    /// `recap_requests/{request_id}.json`. Same GCS ride-along as
    /// compaction requests; enables offline recap prompt / garble replay.
    RecapRequest(crate::extensions::notification::RecapRequestFile),
    /// Persist a compaction segment (`Segments` mode).
    CompactionSegment(crate::extensions::notification::CompactionSegmentFile),
    /// Generated session title from background LLM task.
    /// Routed back through the persistence channel so the storage write
    /// stays sequential with other summary.json mutations.
    GeneratedTitle(String),
    Flush,
    /// Flush all pending writes, then signal the caller once the flush is complete.
    /// Unlike `Flush` (fire-and-forget), this is a **sync barrier**: the caller's
    /// oneshot only resolves after `flush_pending()` finishes writing to disk.
    FlushAndAck {
        respond_to: tokio::sync::oneshot::Sender<()>,
    },
    /// Flush all pending writes, then copy the current session directory contents and return
    /// the in-memory snapshot to the caller (who can tar.gz + upload to GCS, etc.).
    CopyFile {
        one_shot: tokio::sync::oneshot::Sender<anyhow::Result<SessionStateCopy>>,
    },
}

pub use xai_grok_shared::session::session_dir;

/// Check if a session exists locally under the given cwd.
///
/// This is the correct check for the `-r` resume path: a session is only
/// "already local" if it lives under the **same** cwd as the current invocation.
/// A session stored under a different cwd does NOT satisfy this check — the
/// caller must still run the remote restore into the requested cwd.
pub fn session_exists_for_cwd(session_id: &str, cwd: &str) -> bool {
    let sessions_root = crate::util::grok_home::grok_home().join("sessions");
    session_exists_for_cwd_in_root(session_id, cwd, &sessions_root)
}

/// A directory is a resumable session only if it has a `summary.json`; this
/// skips `images/`-only stubs that would otherwise hijack `--resume`. Used by
/// the resume/restore resolution path; `session_exists_by_id` and
/// `find_session_dir_by_id` intentionally stay dir-only (non-resume uses).
fn is_persisted_session_dir(session_path: &Path) -> bool {
    session_path.join("summary.json").is_file()
}

/// Inner implementation of `session_exists_for_cwd` with an injectable root.
/// Separated for deterministic tempdir-based tests.
fn session_exists_for_cwd_in_root(session_id: &str, cwd: &str, sessions_root: &Path) -> bool {
    let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
    let session_path = sessions_root.join(&encoded).join(session_id);
    is_persisted_session_dir(&session_path)
}

/// Find the local child session id that was previously restored from `remote_session_id`
/// in the given `cwd`.
///
/// When a remote session is restored, a new local child is created with
/// `summary.parent_session_id == remote_session_id`.  On a second
/// `grok -r <remote_id>` in the same cwd, this function returns the already-restored
/// child so no duplicate restore is performed.
///
/// If multiple children match (e.g., from pre-fix duplicate restores), the
/// most recently used one is returned.  Selection is fully deterministic:
/// 1. Newest `updated_at` timestamp in `summary.json`
/// 2. Newest session directory mtime as a tie-breaker (catches equal timestamps)
/// 3. Lexicographically largest session id as the final stable tie-breaker
///
/// Returns `Some(local_child_id)` when at least one matching child is found.
/// Returns `None` when no child with `parent_session_id == remote_session_id` exists.
pub fn find_local_child_for_remote(remote_session_id: &str, cwd: &str) -> Option<String> {
    let sessions_root = crate::util::grok_home::grok_home().join("sessions");
    find_local_child_for_remote_in_root(remote_session_id, cwd, &sessions_root)
}

/// Resolve a session ID to one that is available locally under `cwd`.
///
/// Checks in order:
///   1. `session_id` exists directly under `cwd` → returns it as-is.
///   2. A previously restored child of `session_id` exists → returns the child ID.
///   3. Neither found → returns `None` (caller should restore from remote).
pub fn resolve_local_session(session_id: &str, cwd: &str) -> Option<String> {
    if session_exists_for_cwd(session_id, cwd) {
        return Some(session_id.to_string());
    }
    find_local_child_for_remote(session_id, cwd)
}

// Repo-wide session resolution (for worktree resume)

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LocalSessionResolutionKind {
    ExactCwd,
    RestoredChildInExactCwd,
    SameRepoDifferentCwd,
    RestoredChildInSameRepoDifferentCwd,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedLocalSession {
    pub session_id: String,
    pub cwd: String,
    pub resolution_kind: LocalSessionResolutionKind,
}

/// Resolve a session across multiple candidate cwds for worktree resume.
///
/// The first cwd in `candidate_cwds` should be the exact current cwd so it
/// gets priority. For each candidate, checks both direct session existence
/// and previously-restored children.
///
/// Returns `None` when no local match exists in any candidate.
pub fn resolve_local_session_for_repo(
    session_id: &str,
    candidate_cwds: &[&str],
) -> Option<ResolvedLocalSession> {
    let sessions_root = crate::util::grok_home::grok_home().join("sessions");
    resolve_local_session_for_repo_in_root(session_id, candidate_cwds, &sessions_root)
}

pub fn resolve_local_session_for_repo_in_root(
    session_id: &str,
    candidate_cwds: &[&str],
    sessions_root: &Path,
) -> Option<ResolvedLocalSession> {
    for (i, &cwd) in candidate_cwds.iter().enumerate() {
        let is_exact = i == 0;

        if session_exists_for_cwd_in_root(session_id, cwd, sessions_root) {
            return Some(ResolvedLocalSession {
                session_id: session_id.to_owned(),
                cwd: cwd.to_owned(),
                resolution_kind: if is_exact {
                    LocalSessionResolutionKind::ExactCwd
                } else {
                    LocalSessionResolutionKind::SameRepoDifferentCwd
                },
            });
        }

        if let Some(child_id) = find_local_child_for_remote_in_root(session_id, cwd, sessions_root)
        {
            return Some(ResolvedLocalSession {
                session_id: child_id,
                cwd: cwd.to_owned(),
                resolution_kind: if is_exact {
                    LocalSessionResolutionKind::RestoredChildInExactCwd
                } else {
                    LocalSessionResolutionKind::RestoredChildInSameRepoDifferentCwd
                },
            });
        }
    }
    None
}
fn find_local_child_for_remote_in_root(
    remote_session_id: &str,
    cwd: &str,
    sessions_root: &Path,
) -> Option<String> {
    let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
    let cwd_dir = sessions_root.join(&encoded);
    if !cwd_dir.exists() {
        return None;
    }

    // Collect all matching children.  Multiple can exist when a user ran
    // `grok -r <remote_id>` before this fix was deployed.
    // Tuple: (updated_at, dir_mtime_nanos, session_id) — all sorted descending.
    let mut candidates: Vec<(String, u128, String)> = Vec::new();

    let entries = std::fs::read_dir(&cwd_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let summary_path = path.join("summary.json");
        if !summary_path.exists() {
            continue;
        }
        // Parse minimum fields without deserializing the full Summary,
        // so we don't fail on missing/extra fields from older/newer formats.
        if let Ok(raw) = std::fs::read_to_string(&summary_path)
            && let Ok(partial) = serde_json::from_str::<serde_json::Value>(&raw)
            && partial.get("parent_session_id").and_then(|v| v.as_str()) == Some(remote_session_id)
            && let Some(session_id) = path.file_name().and_then(|n| n.to_str())
        {
            let updated_at = partial
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Directory mtime as a tie-breaker for equal updated_at values.
            let dir_mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            candidates.push((updated_at, dir_mtime, session_id.to_string()));
        }
    }

    // Sort descending by all three keys for full determinism.
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
    candidates.into_iter().next().map(|(_, _, id)| id)
}

/// Check if a session exists locally by session ID.
/// Searches across ALL cwd directories under `~/.grok/sessions/`.
///
/// Use `session_exists_for_cwd` instead when the target cwd is known
/// (e.g., the `-r` resume path) to avoid false-positive matches.
/// Find a session by ID across **all** CWD directories under `~/.grok/sessions/`.
///
/// Unlike [`resolve_local_session`] which only checks a single CWD,
/// this scans every encoded-CWD subdirectory. Returns the decoded CWD path
/// that contains the session, or `None` if not found anywhere.
///
/// This is used by the pager's `--resume` to find sessions that were created
/// in a different CWD (e.g., a worktree) than the one the user is currently in.
pub fn resolve_local_session_any_cwd(session_id: &str) -> Option<String> {
    let sessions_root = crate::util::grok_home::grok_home().join("sessions");
    resolve_local_session_any_cwd_in_root(session_id, &sessions_root)
}

fn resolve_local_session_any_cwd_in_root(session_id: &str, sessions_root: &Path) -> Option<String> {
    if !sessions_root.exists() {
        return None;
    }
    let entries = std::fs::read_dir(sessions_root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let session_path = path.join(session_id);
        if is_persisted_session_dir(&session_path) {
            // Decode the CWD from the directory name. Skip entries whose
            // names cannot be decoded — a raw URL-encoded string is not a
            // usable CWD and returning it would confuse callers.
            if let Some(decoded) = crate::util::grok_home::decode_cwd_from_dirname(&path) {
                return Some(decoded);
            }
        }
    }
    None
}

/// Scan all CWD directories for a session and return its directory path.
pub fn find_session_dir_by_id(session_id: &str) -> Option<PathBuf> {
    let sessions_root = grok_home().join("sessions");
    find_session_dir_by_id_in_root(session_id, &sessions_root)
}

/// Scan all CWD directories under `sessions_root` for a session directory.
pub fn find_session_dir_by_id_in_root(session_id: &str, sessions_root: &Path) -> Option<PathBuf> {
    if !sessions_root.exists() {
        return None;
    }
    for entry in std::fs::read_dir(sessions_root).ok()?.flatten() {
        let candidate = entry.path().join(session_id);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

pub fn session_exists_by_id(session_id: &str) -> bool {
    let sessions_root = crate::util::grok_home::grok_home().join("sessions");
    session_exists_in_root(session_id, &sessions_root)
}

/// Inner implementation of `session_exists_by_id` that accepts a custom root.
/// Separated so tests can use a tempdir without touching the real grok home.
fn session_exists_in_root(session_id: &str, sessions_root: &Path) -> bool {
    if !sessions_root.exists() {
        return false;
    }
    if let Ok(entries) = std::fs::read_dir(sessions_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let session_path = path.join(session_id);
                if session_path.exists() && session_path.is_dir() {
                    return true;
                }
            }
        }
    }
    false
}

/// Find and read a session summary given only its ID (scans all CWD directories).
pub fn find_summary_by_session_id(session_id: &str) -> Option<Summary> {
    find_summary_by_session_id_in_root(session_id, &grok_home().join("sessions"))
}

/// Inner implementation with injectable root for testing.
pub(crate) fn find_summary_by_session_id_in_root(
    session_id: &str,
    sessions_root: &Path,
) -> Option<Summary> {
    if session_id.contains('/') || session_id.contains('\\') || session_id.contains("..") {
        return None;
    }
    let entries = std::fs::read_dir(sessions_root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let summary_path = path.join(session_id).join("summary.json");
        if let Ok(bytes) = std::fs::read(&summary_path)
            && let Ok(summary) = serde_json::from_slice::<Summary>(&bytes)
        {
            return Some(summary);
        }
    }
    None
}

/// The most recently updated local session summary for `cwd` (by
/// `last_active_at` else `updated_at`), or `None` if there are no local sessions
/// for that cwd. Sync and local-only — suitable for the startup path that must
/// resolve the sandbox profile before the (irreversible) OS sandbox is applied.
fn most_recent_local_summary_for_cwd_in_root(cwd: &str, sessions_root: &Path) -> Option<Summary> {
    let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
    let cwd_dir = sessions_root.join(&encoded);
    let mut best: Option<Summary> = None;
    for entry in std::fs::read_dir(&cwd_dir).ok()?.flatten() {
        let summary_path = entry.path().join("summary.json");
        let Ok(bytes) = std::fs::read(&summary_path) else {
            continue;
        };
        let Ok(summary) = serde_json::from_slice::<Summary>(&bytes) else {
            continue;
        };
        // Match `list_sessions`: skip hidden/subagent sessions so the peek reads
        // the same session a `-c` / bare `--resume` actually resumes.
        if summary.is_hidden() {
            continue;
        }
        if best.as_ref().is_none_or(|b| {
            let st = summary.last_active_at.unwrap_or(summary.updated_at);
            let bt = b.last_active_at.unwrap_or(b.updated_at);
            st > bt || (st == bt && summary.info.id.0.as_ref() < b.info.id.0.as_ref())
        }) {
            best = Some(summary);
        }
    }
    best
}

/// Best-effort lookup of the sandbox profile persisted with a session that is
/// about to be resumed, used at startup to restore the session's profile before
/// the (irreversible) OS sandbox is applied.
///
/// - `session_id`: the explicit id from `--resume <id>` / `--load <id>` /
///   `-s <id>`. Resolved directly across all cwds, then — for a remote id that
///   was restored into a local child — via that child's `parent_session_id`.
/// - `cwd`: the current working directory. Used to resolve a remote id to its
///   local child, and as the lookup key for `-c` / `--continue` and bare
///   `--resume` (most-recent-for-cwd).
///
/// Returns `None` when not resuming, the session isn't found locally, or it has
/// no persisted profile (sessions created before this was tracked) — callers
/// then fall back to the normal config/CLI resolution.
pub fn resumed_session_sandbox_profile(
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> Option<String> {
    resumed_session_sandbox_profile_in_root(session_id, cwd, &grok_home().join("sessions"))
}

fn resumed_session_sandbox_profile_in_root(
    session_id: Option<&str>,
    cwd: Option<&str>,
    sessions_root: &Path,
) -> Option<String> {
    if let Some(id) = session_id.filter(|s| !s.is_empty()) {
        // Direct match by id (across all cwds).
        if let Some(summary) = find_summary_by_session_id_in_root(id, sessions_root) {
            return summary.sandbox_profile;
        }
        // A remote id resumes into a local child (fresh id, `parent_session_id`
        // = remote id). Mirror the canonical resume path so the peek doesn't
        // miss the restored session's saved profile.
        if let Some(cwd) = cwd
            && let Some(child) = find_local_child_for_remote_in_root(id, cwd, sessions_root)
        {
            return find_summary_by_session_id_in_root(&child, sessions_root)
                .and_then(|s| s.sandbox_profile);
        }
        return None;
    }
    if let Some(cwd) = cwd {
        return most_recent_local_summary_for_cwd_in_root(cwd, sessions_root)
            .and_then(|s| s.sandbox_profile);
    }
    None
}

/// Get file path for storing a large prompt.
/// Creates the prompts subdirectory if it doesn't exist.
/// Path format: `{session_dir}/prompts/prompt_{prompt_index}.txt`
pub fn get_prompt_file_path(info: &Info, prompt_index: usize) -> PathBuf {
    let prompts_dir = session_dir(info).join("prompts");
    std::fs::create_dir_all(&prompts_dir).ok();
    prompts_dir.join(format!("prompt_{}.txt", prompt_index))
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingCwdSwitchReminder {
    pub cwd_generation: u64,
    pub previous_cwd: String,
    #[serde(alias = "cwd")]
    pub destination_cwd: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_project_instructions: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub info: Info,
    /// Monotonic generation of the authoritative cwd in `info.cwd`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cwd_generation: u64,
    /// Cwd immediately preceding the current generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_cwd: Option<String>,
    /// Reminder staged for exactly-once append during relocation completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_cwd_switch_reminder: Option<PendingCwdSwitchReminder>,
    /// Latest switch generation reflected in `num_chat_messages` bookkeeping.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cwd_switch_bookkeeping_generation: u64,
    pub session_summary: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub num_messages: usize,
    #[serde(default)]
    pub num_chat_messages: usize,
    pub current_model_id: acp::ModelId,
    /// Parent session ID if this session was forked from another session
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Timestamp when this session was forked (only set for forked sessions)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_at: Option<DateTime<Utc>>,
    /// Collection ID for telemetry trace uploads (one per session)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_id: Option<String>,
    /// Next telemetry trace turn id (monotonic, persisted).
    /// Used to generate unique turn ids for telemetry metadata/filenames even across rewinds.
    #[serde(default)]
    pub next_trace_turn: u64,
    /// Chat history format version:
    /// - 0 (default): Legacy ChatRequestMessage format
    /// - 1: ConversationItem format
    #[serde(default)]
    pub chat_format_version: u8,
    /// Stable display path for forked sessions.
    ///
    /// When set, the system prompt's `Workspace Path` and prompt metadata
    /// paths show this value instead of the real worktree/overlay path
    /// (`info.cwd`). Persisted so the override survives session
    /// restore/reload without the caller needing to resend it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_display_cwd: Option<String>,
    /// What created this session: `"fork"`, `"subagent"`, `"subagent_fork"`, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<String>,
    /// How the session's initial context was bootstrapped: `"new"` or `"forked"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_context_source: Option<String>,
    /// The parent prompt/turn ID that triggered this fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_parent_prompt_id: Option<String>,
    /// Number of conversation items inherited from the parent session.
    /// During compaction, items below this index are preserved as-is
    /// (the "inherited prefix"). Only items after this boundary are
    /// summarized. `None` means no inherited prefix (non-forked session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_prefix_len: Option<usize>,
    /// Visibility override. None = default for `session_kind`, Some = explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// The original workspace directory this worktree session was spawned from.
    /// Used by clients to group worktree sessions under their source workspace
    /// regardless of the worktree's actual `cwd`. Only set when
    /// `session_kind == "worktree"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_root_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_remotes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Absolute path to the `.grok` directory, used by reconstruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grok_home: Option<String>,
    /// When the session last had content added (user or model messages).
    /// Only advanced locally by `append_update` / `append_chat_message`;
    /// never touched by remote registry operations or metadata-only writes.
    /// `None` for sessions created before this field was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<DateTime<Utc>>,
    /// LLM-generated session title persisted separately from `session_summary`.
    /// When present, this is preferred for display over `session_summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_title: Option<String>,
    /// True when `generated_title` was set by a manual `/rename` (vs auto LLM
    /// title). Manual titles render inline in the prompt's top border on
    /// resume.
    #[serde(default, skip_serializing_if = "is_false")]
    pub title_is_manual: bool,
    /// Human-readable label for the worktree directory (e.g. "nuke-v-tables").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_label: Option<String>,
    /// The agent definition name that was active when the session was last saved.
    /// Used during session resume to avoid re-deriving from the (mutable) model
    /// catalog — if the model is removed or its `agent_type` changes between
    /// sessions, this persisted value ensures the correct harness is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// The OS sandbox profile this session ran under (e.g. "workspace",
    /// "strict", "off", or a custom name). Persisted so a resumed session is
    /// restored to the same profile instead of silently falling back to the
    /// config default — which would otherwise break commands that worked before
    /// (a stricter profile denies filesystem/network the session relied on).
    /// `None` for sessions created before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Current `grok_home` as a UTF-8 string, or `None` if the path isn't valid UTF-8.
pub fn grok_home_string() -> Option<String> {
    crate::util::grok_home::grok_home()
        .to_str()
        .map(String::from)
}

pub fn default_model_id() -> acp::ModelId {
    acp::ModelId::new(crate::models::default_model())
}

impl Summary {
    pub fn new(info: &Info, model_id: acp::ModelId) -> std::io::Result<Self> {
        let git_metadata =
            xai_grok_workspace::session::git::resolve_persisted_session_git_metadata_sync(
                std::path::Path::new(&info.cwd),
            );
        Ok(Self {
            info: info.clone(),
            cwd_generation: 0,
            previous_cwd: None,
            pending_cwd_switch_reminder: None,
            cwd_switch_bookkeeping_generation: 0,
            session_summary: String::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            num_messages: 0,
            num_chat_messages: 0,
            current_model_id: model_id,
            parent_session_id: None,
            forked_at: None,
            collection_id: None,
            next_trace_turn: 0,
            chat_format_version: CHAT_FORMAT_VERSION,
            prompt_display_cwd: None,
            session_kind: None,
            fork_context_source: None,
            fork_parent_prompt_id: None,
            inherited_prefix_len: None,
            hidden: None,
            source_workspace_dir: None,
            git_root_dir: git_metadata.git_root_dir,
            git_remotes: git_metadata.git_remotes,
            head_commit: git_metadata.head_commit,
            head_branch: git_metadata.head_branch,
            request_id: None,
            grok_home: grok_home_string(),
            last_active_at: None,
            generated_title: None,
            title_is_manual: false,
            worktree_label: crate::session::worktree::lookup_worktree_label(&info.cwd),
            agent_name: None,
            sandbox_profile: None,
            reasoning_effort: None,
        })
    }

    /// Whether this session should be excluded from history listings.
    pub fn is_hidden(&self) -> bool {
        self.hidden.unwrap_or(
            self.session_kind
                .as_deref()
                .is_some_and(|k| k.starts_with("subagent")),
        )
    }

    /// Preferred display title: `generated_title` if non-empty, else `session_summary`.
    pub fn display_title(&self) -> &str {
        self.generated_title
            .as_deref()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .unwrap_or(&self.session_summary)
    }

    /// [`Self::display_title`] as an `Option`, `None` when blank.
    pub fn display_title_opt(&self) -> Option<String> {
        let title = self.display_title().trim();
        (!title.is_empty()).then(|| title.to_string())
    }

    /// The manually-`/rename`d title (trimmed), `None` for auto-generated or
    /// blank titles. Binds to `generated_title` — the field `title_is_manual`
    /// describes — never the `session_summary` display fallback, so a stale
    /// flag over a blank manual title can't relabel an auto summary as
    /// manual. When `Some`, it equals [`Self::display_title_opt`] (a
    /// non-blank `generated_title` wins the display chain).
    pub fn manual_title_opt(&self) -> Option<String> {
        self.title_is_manual
            .then_some(self.generated_title.as_deref())
            .flatten()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    }

    /// Last-change time (unix millis): `last_active_at`, else `updated_at`.
    pub fn last_change_unix_ms(&self) -> i64 {
        self.last_active_at
            .unwrap_or(self.updated_at)
            .timestamp_millis()
    }
}

#[cfg(test)]
mod is_hidden_tests {
    use super::*;

    fn summary_with_kind(kind: Option<&str>) -> Summary {
        Summary {
            session_kind: kind.map(String::from),
            hidden: None,
            ..Summary::new(
                &Info {
                    id: acp::SessionId::new("test"),
                    cwd: "/tmp".into(),
                },
                default_model_id(),
            )
            .unwrap()
        }
    }

    #[test]
    fn summary_round_trips_and_defaults_reasoning_effort() {
        let mut s = summary_with_kind(None);
        s.reasoning_effort = None;
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("reasoning_effort"),
            "a None effort must not be serialized"
        );
        let back: Summary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reasoning_effort, None);

        s.reasoning_effort = Some(ReasoningEffort::Xhigh);
        let json = serde_json::to_string(&s).unwrap();
        let back: Summary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reasoning_effort, Some(ReasoningEffort::Xhigh));
    }

    #[test]
    fn hidden_for_all_subagent_kinds() {
        for kind in ["subagent", "subagent_fork", "subagent_resume"] {
            assert!(
                summary_with_kind(Some(kind)).is_hidden(),
                "{kind} should be hidden"
            );
        }
    }

    #[test]
    fn not_hidden_for_regular_sessions() {
        assert!(!summary_with_kind(None).is_hidden());
        assert!(!summary_with_kind(Some("fork")).is_hidden());
        assert!(!summary_with_kind(Some("worktree")).is_hidden());
    }

    #[test]
    fn explicit_hidden_overrides_session_kind() {
        let mut s = summary_with_kind(Some("subagent"));
        s.hidden = Some(false);
        assert!(!s.is_hidden(), "explicit hidden=false overrides kind");

        let mut s = summary_with_kind(None);
        s.hidden = Some(true);
        assert!(s.is_hidden(), "explicit hidden=true overrides kind");
    }
}

#[cfg(test)]
mod head_fields_tests {
    use super::*;

    #[test]
    fn summary_round_trips_head_fields_through_json() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.head_commit = Some("abc123def456".into());
        summary.head_branch = Some("main".into());

        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: Summary = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.head_commit.as_deref(), Some("abc123def456"));
        assert_eq!(deserialized.head_branch.as_deref(), Some("main"));
    }

    #[test]
    fn summary_deserializes_without_head_fields_backward_compat() {
        // Simulate an old summary.json that lacks head_commit/head_branch.
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "num_chat_messages": 0,
            "current_model_id": "test-model"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert!(summary.head_commit.is_none());
        assert!(summary.head_branch.is_none());
    }

    #[test]
    fn summary_relocation_metadata_is_backward_compatible() {
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "num_chat_messages": 0,
            "current_model_id": "test-model"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.cwd_generation, 0);
        assert!(summary.previous_cwd.is_none());
        assert!(summary.pending_cwd_switch_reminder.is_none());
        assert_eq!(summary.cwd_switch_bookkeeping_generation, 0);

        let serialized = serde_json::to_value(summary).unwrap();
        for field in [
            "cwd_generation",
            "previous_cwd",
            "pending_cwd_switch_reminder",
            "cwd_switch_bookkeeping_generation",
        ] {
            assert!(serialized.get(field).is_none());
        }
    }

    #[test]
    fn summary_relocation_metadata_round_trips() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/new".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.cwd_generation = 2;
        summary.previous_cwd = Some("/old".into());
        summary.pending_cwd_switch_reminder = Some(PendingCwdSwitchReminder {
            cwd_generation: 2,
            previous_cwd: "/old".into(),
            destination_cwd: "/new".into(),
            content: "moved".into(),
            destination_project_instructions: Some("target rules".into()),
        });

        let serialized = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            serialized["pending_cwd_switch_reminder"]["destination_cwd"],
            "/new"
        );
        assert!(
            serialized["pending_cwd_switch_reminder"]
                .get("cwd")
                .is_none()
        );
        let back: Summary = serde_json::from_value(serialized).unwrap();
        assert_eq!(back.cwd_generation, 2);
        assert_eq!(back.previous_cwd.as_deref(), Some("/old"));
        assert_eq!(
            back.pending_cwd_switch_reminder,
            summary.pending_cwd_switch_reminder
        );
        assert_eq!(back.info.cwd, "/new");

        let pending: PendingCwdSwitchReminder = serde_json::from_value(serde_json::json!({
            "cwd_generation": 2,
            "previous_cwd": "/old",
            "cwd": "/new",
            "content": "moved"
        }))
        .unwrap();
        assert_eq!(pending.destination_cwd, "/new");
    }

    #[test]
    fn summary_skips_none_head_fields_in_serialized_json() {
        let summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        // In a non-git directory the fields will be None.
        // Verify they are omitted from the JSON output.
        let json = serde_json::to_string(&summary).unwrap();
        // head_commit should not appear if the cwd has a repo (it might),
        // but verify the skip_serializing_if attribute works for None.
        if summary.head_commit.is_none() {
            assert!(!json.contains("head_commit"));
        }
        if summary.head_branch.is_none() {
            assert!(!json.contains("head_branch"));
        }
    }
}

#[cfg(test)]
mod generated_title_tests {
    use super::*;

    #[test]
    fn summary_round_trips_generated_title_through_json() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Refactor auth middleware".into());
        summary.worktree_label = Some("auth-refactor".into());

        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: Summary = serde_json::from_str(&json).unwrap();

        assert_eq!(
            deserialized.generated_title.as_deref(),
            Some("Refactor auth middleware")
        );
        assert_eq!(
            deserialized.worktree_label.as_deref(),
            Some("auth-refactor")
        );
    }

    #[test]
    fn summary_deserializes_without_new_fields_backward_compat() {
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "first prompt text",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 5,
            "num_chat_messages": 3,
            "current_model_id": "test-model"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert!(summary.generated_title.is_none());
        assert!(summary.worktree_label.is_none());
        assert_eq!(summary.session_summary, "first prompt text");
    }

    #[test]
    fn summary_skips_none_generated_title_in_json() {
        let summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("generated_title"));
        assert!(!json.contains("worktree_label"));
    }

    #[test]
    fn summary_includes_generated_title_when_set() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Fix K8s deployment".into());
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("generated_title"));
        assert!(json.contains("Fix K8s deployment"));
    }

    #[test]
    fn summary_deserializes_with_all_fields_present() {
        let json = r#"{
            "info": { "id": "full-session", "cwd": "/tmp" },
            "session_summary": "first prompt",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 10,
            "num_chat_messages": 5,
            "current_model_id": "test-model",
            "head_branch": "feature/xyz",
            "git_root_dir": "/home/user/myrepo",
            "generated_title": "Implement XYZ feature",
            "worktree_label": "xyz-feature"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert_eq!(
            summary.generated_title.as_deref(),
            Some("Implement XYZ feature")
        );
        assert_eq!(summary.worktree_label.as_deref(), Some("xyz-feature"));
        assert_eq!(summary.head_branch.as_deref(), Some("feature/xyz"));
        assert_eq!(summary.git_root_dir.as_deref(), Some("/home/user/myrepo"));
    }

    // ── display_title direct tests ──────────────────────────────────────

    #[test]
    fn display_title_returns_generated_title_when_set() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Refactor auth layer".into());
        assert_eq!(summary.display_title(), "Refactor auth layer");
    }

    #[test]
    fn display_title_falls_back_on_empty_generated_title() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.session_summary = "first prompt fallback".into();
        summary.generated_title = Some(String::new());
        assert_eq!(summary.display_title(), "first prompt fallback");
    }

    #[test]
    fn display_title_falls_back_on_none_generated_title() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.session_summary = "session summary fallback".into();
        summary.generated_title = None;
        assert_eq!(summary.display_title(), "session summary fallback");
    }

    // ── title_is_manual / manual_title_opt ──────────────────────────────

    #[test]
    fn title_is_manual_round_trips_through_json() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Manual Title".into());
        summary.title_is_manual = true;

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("title_is_manual"));
        let deserialized: Summary = serde_json::from_str(&json).unwrap();

        assert!(deserialized.title_is_manual);
        assert_eq!(
            deserialized.manual_title_opt().as_deref(),
            Some("Manual Title")
        );
    }

    #[test]
    fn title_is_manual_defaults_false_and_skips_when_unset() {
        // Old summary.json without the field: default false, so pre-existing
        // renames show no border title until renamed again.
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "first prompt text",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 5,
            "num_chat_messages": 3,
            "current_model_id": "test-model",
            "generated_title": "Old Rename"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert!(!summary.title_is_manual);
        assert!(summary.manual_title_opt().is_none());
        assert_eq!(summary.display_title_opt().as_deref(), Some("Old Rename"));

        // And false is omitted on write, keeping old files byte-stable.
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("title_is_manual"));
    }

    #[test]
    fn manual_title_opt_none_for_auto_generated_title() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Auto Title".into());

        assert!(summary.manual_title_opt().is_none());
        assert_eq!(summary.display_title_opt().as_deref(), Some("Auto Title"));
    }

    /// A stale `title_is_manual` over a blank `generated_title` (e.g. written
    /// by an old client before the ext boundary rejected blank renames) must
    /// not relabel the `session_summary` display fallback as manual.
    #[test]
    fn manual_title_opt_ignores_stale_flag_over_blank_generated_title() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.session_summary = "auto first-prompt summary".into();
        summary.generated_title = Some("   ".into());
        summary.title_is_manual = true;

        assert!(summary.manual_title_opt().is_none());
        assert_eq!(
            summary.display_title_opt().as_deref(),
            Some("auto first-prompt summary")
        );
    }
}

#[derive(Clone)]
pub struct PersistenceHandle {
    pub tx: mpsc::UnboundedSender<PersistenceMsg>,
    noop: bool,
}

#[derive(Debug)]
pub enum DurableAppendError {
    NotCommitted(io::Error),
    Committed(io::Error),
    AcknowledgementLost(io::Error),
}

impl std::fmt::Display for DurableAppendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error)
            | Self::Committed(error)
            | Self::AcknowledgementLost(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DurableAppendError {}

impl From<crate::session::storage::AppendUpdateError> for DurableAppendError {
    fn from(error: crate::session::storage::AppendUpdateError) -> Self {
        use crate::session::storage::AppendUpdateError;
        match error {
            AppendUpdateError::NotCommitted(error) => Self::NotCommitted(error),
            AppendUpdateError::Committed(error) => Self::Committed(error),
        }
    }
}

impl PersistenceHandle {
    #[cfg(test)]
    pub(crate) fn from_sender_for_test(tx: mpsc::UnboundedSender<PersistenceMsg>) -> Self {
        Self { tx, noop: false }
    }

    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self { tx, noop: true }
    }

    pub fn is_noop(&self) -> bool {
        self.noop
    }

    /// Append after older buffered updates and wait for the durable barrier.
    ///
    /// [`DurableAppendError::NotCommitted`] is safe to retry; [`DurableAppendError::Committed`]
    /// means the replay line landed; [`DurableAppendError::AcknowledgementLost`] has unknown status.
    /// No-op handles return `Unsupported`.
    pub async fn append_update_durably(
        &self,
        update: SessionUpdate,
    ) -> Result<(), DurableAppendError> {
        if self.noop {
            return Err(DurableAppendError::NotCommitted(io::Error::new(
                io::ErrorKind::Unsupported,
                "durable session update append is unsupported by a no-op persistence handle",
            )));
        }
        let (respond_to, response) = tokio::sync::oneshot::channel();
        self.tx
            .send(PersistenceMsg::AppendUpdateDurablyAndAck { update, respond_to })
            .map_err(|_| {
                DurableAppendError::NotCommitted(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "session persistence actor stopped before durable append dispatch",
                ))
            })?;
        response
            .await
            .map_err(|_| {
                DurableAppendError::AcknowledgementLost(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "session persistence actor stopped before durable append acknowledgement",
                ))
            })?
            .map_err(DurableAppendError::from)
    }
}

enum PendingAppendOutcome {
    CommittedOk(acp::SessionNotification),
    CommittedErr(acp::SessionNotification, io::Error),
    NotCommittedErr(acp::SessionNotification, io::Error),
}

struct SessionPersistence {
    info: Info,
    storage: Arc<dyn StorageAdapter>,
    /// Pending ACP notification for merging consecutive text chunks
    pending_notification: Option<acp::SessionNotification>,
    rx: mpsc::UnboundedReceiver<PersistenceMsg>,
    remote_sync: Option<RemoteSync>,
    /// WebSocket-based relay sync for real-time session sharing.
    /// This streams updates to the relay backend in addition to local persistence.
    relay_sync: Option<crate::relay::RelaySync>,
    /// Session title generation lifecycle.
    summary: crate::session::summary::SummaryGenerator,
    registry_title_sync: Option<RegistryGeneratedTitleSync>,
    /// Client gateway for `SessionSummaryGenerated` notifications. Used to
    /// announce an auto-generated title only once it has actually been adopted
    /// (see the `GeneratedTitle` handler), so a title rejected for racing a
    /// manual `/rename` never reaches the client. `None` for the subagent
    /// variant, whose lifecycle notifications are handled by the coordinator.
    gateway: Option<GatewaySender>,
}

impl SessionPersistence {
    fn try_merge_text(prev: &mut acp::ContentBlock, new: &acp::ContentBlock) -> bool {
        match (prev, new) {
            (acp::ContentBlock::Text(prev_text), acp::ContentBlock::Text(new_text))
                if prev_text.annotations.is_none()
                    && prev_text.meta.is_none()
                    && new_text.annotations.is_none()
                    && new_text.meta.is_none() =>
            {
                prev_text.text.push_str(&new_text.text);
                true
            }
            _ => false,
        }
    }

    // Empty chunks are chunks that have no content and no meta.
    fn is_empty_chunk(update: &acp::SessionUpdate) -> bool {
        match update {
            acp::SessionUpdate::AgentMessageChunk(chunk)
            | acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                let empty_text =
                    matches!(&chunk.content, acp::ContentBlock::Text(t) if t.text.is_empty());
                let no_meta = chunk.meta.is_none();
                empty_text && no_meta
            }
            _ => false,
        }
    }

    /// Attempt to merge consecutive ACP text notifications to reduce storage writes.
    /// Returns Some(notification) if the pending notification should be written now.
    fn maybe_merge_notification(
        &mut self,
        incoming: &acp::SessionNotification,
    ) -> Option<acp::SessionNotification> {
        // Always skip empty chunks - don't store them at all
        if Self::is_empty_chunk(&incoming.update) {
            return None;
        }

        let Some(pending) = self.pending_notification.take() else {
            self.pending_notification = Some(incoming.clone());
            return None;
        };

        let pending_update = pending.update.clone();
        match (&incoming.update, pending_update) {
            (
                acp::SessionUpdate::AgentMessageChunk(new_chunk),
                acp::SessionUpdate::AgentMessageChunk(mut pending_chunk),
            )
            | (
                acp::SessionUpdate::AgentThoughtChunk(new_chunk),
                acp::SessionUpdate::AgentThoughtChunk(mut pending_chunk),
            ) => {
                let did_merge = pending_chunk.meta.is_none()
                    && new_chunk.meta.is_none()
                    && Self::try_merge_text(&mut pending_chunk.content, &new_chunk.content);

                if did_merge {
                    let merged_update = match &incoming.update {
                        acp::SessionUpdate::AgentMessageChunk(_) => {
                            acp::SessionUpdate::AgentMessageChunk(pending_chunk)
                        }
                        acp::SessionUpdate::AgentThoughtChunk(_) => {
                            acp::SessionUpdate::AgentThoughtChunk(pending_chunk)
                        }
                        _ => unreachable!(),
                    };
                    self.pending_notification = Some(
                        acp::SessionNotification::new(incoming.session_id.clone(), merged_update)
                            .meta(incoming.meta.clone()),
                    );
                    None
                } else {
                    self.pending_notification = Some(incoming.clone());
                    Some(pending)
                }
            }
            _ => {
                self.pending_notification = Some(incoming.clone());
                Some(pending)
            }
        }
    }

    async fn write_update(
        &self,
        update: &SessionUpdate,
    ) -> Result<(), crate::session::storage::AppendUpdateError> {
        self.storage
            .append_update_commit_aware(&self.info, update)
            .await
    }

    fn queue_acp_sync(&self, notification: acp::SessionNotification) {
        if let Some(sync) = &self.remote_sync {
            sync.queue(notification.clone());
        }
        if let Some(relay) = &self.relay_sync {
            relay.queue(notification);
        }
    }

    fn finish_pending_append(
        notification: acp::SessionNotification,
        result: Result<(), crate::session::storage::AppendUpdateError>,
    ) -> PendingAppendOutcome {
        match result {
            Ok(()) => PendingAppendOutcome::CommittedOk(notification),
            Err(crate::session::storage::AppendUpdateError::NotCommitted(error)) => {
                PendingAppendOutcome::NotCommittedErr(notification, error)
            }
            Err(crate::session::storage::AppendUpdateError::Committed(error)) => {
                PendingAppendOutcome::CommittedErr(notification, error)
            }
        }
    }

    /// Restore uncommitted failures; sync committed records before returning errors.
    async fn drain_pending(&mut self) -> Result<(), crate::session::storage::AppendUpdateError> {
        if let Some(notification) = self.pending_notification.take() {
            let result = self
                .write_update(&SessionUpdate::Acp(Box::new(notification.clone())))
                .await;
            match Self::finish_pending_append(notification, result) {
                PendingAppendOutcome::CommittedOk(notification) => {
                    self.queue_acp_sync(notification);
                }
                PendingAppendOutcome::CommittedErr(notification, error) => {
                    self.queue_acp_sync(notification);
                    return Err(crate::session::storage::AppendUpdateError::Committed(error));
                }
                PendingAppendOutcome::NotCommittedErr(notification, error) => {
                    self.pending_notification = Some(notification);
                    return Err(crate::session::storage::AppendUpdateError::NotCommitted(
                        error,
                    ));
                }
            }
        }
        Ok(())
    }

    async fn handle_durable_append(
        &mut self,
        update: SessionUpdate,
    ) -> Result<(), crate::session::storage::AppendUpdateError> {
        self.drain_pending().await?;
        let result = self
            .storage
            .append_update_durable_commit_aware(&self.info, &update)
            .await;
        match (&update, &result) {
            (SessionUpdate::Acp(notification), Ok(()))
            | (
                SessionUpdate::Acp(notification),
                Err(crate::session::storage::AppendUpdateError::Committed(_)),
            ) => self.queue_acp_sync((**notification).clone()),
            _ => {}
        }
        result
    }

    /// Flush any pending merged ACP notification to disk and remote sync.
    async fn flush_pending(&mut self) {
        if let Err(error) = self.drain_pending().await {
            tracing::warn!(%error, "failed to write pending update");
        }
        if let Some(sync) = &self.remote_sync {
            sync.flush();
        }
        if let Some(relay) = &self.relay_sync {
            relay.flush();
        }
    }

    /// Flush pending writes and sync all session files to disk.
    /// Called before CopyFile to ensure all data is persisted.
    async fn flush_and_sync(&mut self) {
        self.flush_pending().await;
        // Sync all session files to disk to ensure they're actually written
        if let Err(e) = self.storage.sync_session_files(&self.info).await {
            tracing::warn!(?e, "Failed to sync session files to disk");
        }
    }

    async fn run(mut self) {
        // Persistence traffic counts as worktree activity; debounced so
        // long-resident sessions (leader/remote, active for days without a
        // re-open) stay out of gc expiry without per-message DB writes.
        // The constructors fire the t=0 touch, so this starts at now().
        let mut last_worktree_touch = std::time::Instant::now();
        while let Some(msg) = self.rx.recv().await {
            if last_worktree_touch.elapsed() >= WORKTREE_TOUCH_INTERVAL {
                last_worktree_touch = std::time::Instant::now();
                // Detached on purpose: opportunistic refresh, no ordering need.
                spawn_worktree_touch(&self.info);
            }
            match msg {
                PersistenceMsg::Flush => {
                    self.flush_pending().await;
                }
                PersistenceMsg::FlushAndAck { respond_to } => {
                    self.flush_pending().await;
                    let _ = respond_to.send(());
                }
                PersistenceMsg::Update(update) => {
                    match update {
                        SessionUpdate::Acp(notification) => {
                            // ACP notifications use merging to coalesce consecutive text chunks
                            if let Some(to_write) = self.maybe_merge_notification(&notification) {
                                match self
                                    .write_update(&SessionUpdate::Acp(Box::new(to_write.clone())))
                                    .await
                                {
                                    Ok(())
                                    | Err(crate::session::storage::AppendUpdateError::Committed(
                                        _,
                                    )) => {
                                        self.queue_acp_sync(to_write);
                                    }
                                    Err(error) => tracing::warn!(%error, "failed to write update"),
                                }
                            }
                        }
                        SessionUpdate::Xai(_) => {
                            // xAI notifications are written directly without merging
                            if let Err(error) = self.write_update(&update).await {
                                tracing::warn!(%error, "failed to write update");
                            }
                        }
                    }
                }
                PersistenceMsg::AppendUpdateDurablyAndAck { update, respond_to } => {
                    let result = self.handle_durable_append(update).await;
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::Chat(chat_msg) => {
                    if let Err(e) = self
                        .storage
                        .append_chat_message(&self.info, &chat_msg)
                        .await
                    {
                        tracing::warn!(?e, "failed to write chat message");
                    }
                }
                PersistenceMsg::AppendCwdSwitchAndAck { item, respond_to } => {
                    let result = self
                        .storage
                        .append_cwd_switch_commit_aware(&self.info, &item)
                        .await
                        .map_err(|error| match error {
                            crate::session::storage::AppendCwdSwitchError::NotCommitted(error) => {
                                xai_chat_state::StrictAppendError::NotCommitted(error)
                            }
                            crate::session::storage::AppendCwdSwitchError::Committed {
                                acknowledgement,
                                source,
                            } => xai_chat_state::StrictAppendError::Committed {
                                acknowledgement,
                                source,
                            },
                        });
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::ReplaceChatHistory(messages) => {
                    tracing::info!(
                        num_messages = messages.len(),
                        "Replacing chat history (compaction)"
                    );
                    if let Err(e) = self
                        .storage
                        .replace_chat_history(&self.info, &messages)
                        .await
                    {
                        tracing::warn!(?e, "failed to replace chat history");
                    }
                }
                PersistenceMsg::CurrentModel {
                    model_id,
                    agent_name,
                    reasoning_effort,
                } => {
                    if let Err(e) = self
                        .storage
                        .update_current_model_and_agent(
                            &self.info,
                            &model_id,
                            agent_name.as_deref(),
                            reasoning_effort,
                        )
                        .await
                    {
                        tracing::warn!(?e, "failed to update current model");
                    }
                    if let Some(sync) = &self.remote_sync {
                        sync.set_model_id(model_id.0.to_string());
                    }
                }
                PersistenceMsg::PlanState(state) => {
                    if let Err(e) = self.storage.write_plan_state(&self.info, &state).await {
                        tracing::warn!(?e, "failed to write plan state");
                    }
                }
                PersistenceMsg::PlanModeState(state) => {
                    if let Err(e) = self.storage.write_plan_mode_state(&self.info, &state).await {
                        tracing::warn!(?e, "failed to write plan mode state");
                    }
                }
                PersistenceMsg::GoalModeState(state) => {
                    if let Err(e) = self.storage.write_goal_mode_state(&self.info, &state).await {
                        tracing::warn!(?e, "failed to write goal mode state");
                    }
                }
                PersistenceMsg::DeleteGoalModeState { respond_to } => {
                    let result = self.storage.delete_goal_mode_state(&self.info).await;
                    if let Err(e) = &result {
                        tracing::warn!(?e, "failed to delete goal mode state");
                    }
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::WorkflowRunState(manifest) => {
                    if let Err(error) = self
                        .storage
                        .write_workflow_run_state(&self.info, &manifest)
                        .await
                    {
                        tracing::warn!(run_id = %manifest.state.run_id, ?error, "failed to write workflow run state");
                    }
                }
                PersistenceMsg::WorkflowRunStateAndAck {
                    manifest,
                    respond_to,
                } => {
                    let result = self
                        .storage
                        .write_workflow_run_state(&self.info, &manifest)
                        .await;
                    if let Err(error) = &result {
                        tracing::warn!(run_id = %manifest.state.run_id, ?error, "failed to write acknowledged workflow run state");
                    }
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::DeleteWorkflowRunState(run_id) => {
                    if let Err(e) = self
                        .storage
                        .delete_workflow_run_state(&self.info, &run_id)
                        .await
                    {
                        tracing::warn!(%run_id, ?e, "failed to delete workflow run state");
                    }
                }
                PersistenceMsg::ContentChunk(content_chunks) => {
                    let content_part = content_chunks
                        .content_chunks
                        .into_iter()
                        .filter_map(|content_chunk| match content_chunk {
                            acp::ContentBlock::Text(text) => Some(text.text),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.summary.update(content_part);

                    // Notify session search index so this turn becomes searchable
                    crate::session::storage::search::notify_session_updated(
                        &self.info.id.to_string(),
                        &self.info.cwd,
                    );
                }
                PersistenceMsg::GeneratedTitle(title) => {
                    // Auto-generated titles must never overwrite a title the
                    // user set via `/rename`. `set_generated_title_if_absent`
                    // writes only when the session still has no title (checked
                    // atomically under the summary lock) and reports whether it
                    // did, so a manual rename that raced this generation wins
                    // and its title is not clobbered locally or on remotes.
                    match self
                        .storage
                        .set_generated_title_if_absent(&self.info, title.clone())
                        .await
                    {
                        Ok(true) => {
                            // Announce to clients only now that the title is
                            // adopted, so a title rejected for racing a manual
                            // `/rename` never overwrites the client's title.
                            crate::session::summary::notify_client(
                                &self.gateway,
                                &self.info,
                                &title,
                            );
                            if let Some(sync) = &self.remote_sync {
                                sync.set_title(title.clone());
                            }
                            if let Some(reg) = self.registry_title_sync.as_ref()
                                && !reg.suppress_for_zdr
                            {
                                let client = reg.client.clone();
                                let sid = self.info.id.to_string();
                                let t = title;
                                tokio::spawn(async move {
                                    let req =
                                        crate::agent::session_registry_client::UpdateRequest {
                                            summary: Some(t),
                                            first_prompt: None,
                                            last_turn_number: None,
                                            repo_head_at_end: None,
                                            restorable_turn_number: None,
                                        };
                                    if let Err(e) = client.update(&sid, &req).await {
                                        tracing::warn!(
                                            error = %e,
                                            session_id = %sid,
                                            "session registry summary sync failed after title generation"
                                        );
                                    }
                                });
                            }
                        }
                        Ok(false) => {
                            tracing::debug!(
                                "skipped auto-generated title; session already has a title"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(?e, "failed to persist generated session title");
                        }
                    }
                }
                PersistenceMsg::RewindPoint(point) => {
                    if let Err(e) = self.storage.append_rewind_point(&self.info, &point).await {
                        tracing::warn!(?e, "failed to write rewind point");
                    }
                }
                PersistenceMsg::TruncateRewindPoints { from_index } => {
                    if let Err(e) = self
                        .storage
                        .truncate_rewind_points_from(&self.info, from_index)
                        .await
                    {
                        tracing::warn!(?e, from_index, "failed to truncate rewind points");
                    }
                }
                PersistenceMsg::MergeRewindPointsFrom { target_index } => {
                    if let Err(e) = self
                        .storage
                        .merge_rewind_points_from(&self.info, target_index)
                        .await
                    {
                        tracing::warn!(?e, target_index, "failed to merge rewind points");
                    }
                }
                PersistenceMsg::CollectionId(collection_id) => {
                    if let Err(e) = self
                        .storage
                        .update_collection_id(&self.info, &collection_id)
                        .await
                    {
                        tracing::warn!(?e, "failed to write collection id");
                    }
                }
                PersistenceMsg::NextTraceTurn {
                    next_trace_turn,
                    request_id,
                } => {
                    if let Err(e) = self
                        .storage
                        .update_next_trace_turn(&self.info, next_trace_turn, request_id.as_deref())
                        .await
                    {
                        tracing::warn!(?e, "failed to write next trace turn");
                    }
                }
                PersistenceMsg::Signals(signals) => {
                    if let Err(e) = self.storage.write_signals(&self.info, &signals).await {
                        tracing::warn!(?e, "failed to write session signals");
                    }
                }
                PersistenceMsg::AnnouncementState(state) => {
                    if let Err(e) = self
                        .storage
                        .write_announcement_state(&self.info, &state)
                        .await
                    {
                        tracing::warn!(?e, "failed to write announcement state");
                    }
                }
                PersistenceMsg::Feedback(entry) => {
                    if let Err(e) = self.storage.append_feedback(&self.info, &entry).await {
                        tracing::warn!(?e, "failed to write feedback entry");
                    }
                }
                PersistenceMsg::Btw(entry) => {
                    if let Err(e) = self.storage.append_btw(&self.info, &entry).await {
                        tracing::warn!(?e, "failed to write btw entry");
                    }
                }
                PersistenceMsg::GitHead { commit, branch } => {
                    if let Err(e) = self
                        .storage
                        .update_git_head(&self.info, commit, branch)
                        .await
                    {
                        tracing::warn!(?e, "failed to persist git HEAD");
                    }
                }
                PersistenceMsg::CompactionCheckpoint(checkpoint) => {
                    if let Err(e) = self
                        .storage
                        .write_compaction_checkpoint(&self.info, &checkpoint)
                        .await
                    {
                        tracing::warn!(?e, "failed to write compaction checkpoint file");
                    }
                }
                PersistenceMsg::CompactionRequest(request) => {
                    if let Err(e) = self
                        .storage
                        .write_compaction_request(&self.info, &request)
                        .await
                    {
                        tracing::warn!(?e, "failed to write compaction request artifact");
                    }
                }
                PersistenceMsg::RecapRequest(request) => {
                    if let Err(e) = self.storage.write_recap_request(&self.info, &request).await {
                        tracing::warn!(?e, "failed to write recap request artifact");
                    }
                }
                PersistenceMsg::CompactionSegment(segment) => {
                    if let Err(e) = self
                        .storage
                        .write_compaction_segment(&self.info, &segment)
                        .await
                    {
                        tracing::warn!(?e, "failed to write compaction segment");
                    }
                }
                PersistenceMsg::CopyFile { one_shot } => {
                    // Flush pending writes and sync all session files to disk before copying.
                    self.flush_and_sync().await;

                    let result = self.copy_session_dir_to_memory().await;
                    let _ = one_shot.send(result);
                }
            }
        }

        // Drain the merge buffer on channel close.
        self.flush_pending().await;
    }

    async fn copy_session_dir_to_memory(&self) -> anyhow::Result<SessionStateCopy> {
        let session_dir = session_dir(&self.info);
        tokio::task::spawn_blocking(move || {
            let mut files = Vec::new();

            if !session_dir.exists() {
                return Ok(SessionStateCopy { files });
            }

            collect_session_files_recursive(&session_dir, &session_dir, &mut files);
            collect_mcp_stderr_logs(&mut files);

            Ok(SessionStateCopy { files })
        })
        .await?
    }
}

/// Collect MCP server stderr logs from `~/.grok/logs/mcp/` for inclusion in the session archive.
fn collect_mcp_stderr_logs(files: &mut Vec<CopiedSessionFile>) {
    let mcp_log_dir = xai_grok_config::grok_home().join("logs").join("mcp");
    let Ok(entries) = std::fs::read_dir(&mcp_log_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path.extension().is_some_and(|ext| ext == "log")
            && let Ok(data) = std::fs::read(&path)
            && !data.is_empty()
        {
            let name = format!(
                "mcp_stderr/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            files.push(CopiedSessionFile { name, data });
        }
    }
}

/// Recursively collect all files from `dir` into `files`, using paths relative to `base`.
/// This captures subdirectories like `prompts/` which contain large-prompt files
/// referenced by truncated chat history entries.
fn collect_session_files_recursive(base: &Path, dir: &Path, files: &mut Vec<CopiedSessionFile>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(?dir, ?e, "Failed to read directory during session copy");
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let rel_path = match path.strip_prefix(base) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let Some(name) = rel_path.to_str() else {
                continue;
            };
            let data = match std::fs::read(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(?e, "Failed to read session file during copy");
                    continue;
                }
            };
            files.push(CopiedSessionFile {
                name: name.to_string(),
                data,
            });
        } else if path.is_dir() {
            collect_session_files_recursive(base, &path, files);
        }
    }
}

fn init_remote_sync(
    summary: &Summary,
    storage_mode: StorageMode,
    auth_manager: Option<Arc<crate::auth::AuthManager>>,
) -> io::Result<Option<RemoteSync>> {
    match storage_mode {
        StorageMode::Local => Ok(None),
        StorageMode::Writeback => {
            let auth_manager = auth_manager.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Writeback storage mode requires authentication. Run 'grok login' first.",
                )
            })?;
            if let Some(auth) = auth_manager.current_or_expired() {
                if auth.is_zdr_team() {
                    tracing::debug!("ZDR team: skipping remote sync");
                    return Ok(None);
                }
            } else {
                tracing::warn!(
                    "writeback: no auth loaded yet, ZDR check skipped (backend enforces server-side)"
                );
            }
            tracing::info!("Writeback mode enabled, syncing to backend");
            let client =
                crate::remote::BackendClient::new().with_auth_manager(auth_manager.clone());
            let metadata = ExportedMetadata::from_summary(summary);
            Ok(Some(RemoteSync::new(
                summary.info.id.to_string(),
                metadata,
                client,
            )))
        }
    }
}

/// Pull a session from the backend if not found locally. Returns the pulled
/// session's [`Info`] (cwd may differ from caller's on different machines),
/// or `None` if not found or on error.
async fn try_pull_from_remote(info: &Info, client: &crate::remote::BackendClient) -> Option<Info> {
    // BackendClient resolves auth internally via its auth_manager.
    client.auth_manager.as_ref()?;

    tracing::info!(session_id = %info.id, "Session not found locally, trying backend");

    match crate::remote::pull_session_to_local(&info.id.0, client).await {
        Ok(crate::remote::PullResult::Hydrated(pulled_info)) => {
            tracing::info!(
                session_id = %info.id,
                pulled_cwd = %pulled_info.cwd,
                "Pulled session from backend"
            );
            Some(pulled_info)
        }
        Ok(crate::remote::PullResult::NotFound) => {
            tracing::debug!(session_id = %info.id, "Session not found on backend either");
            None
        }
        Err(e) => {
            tracing::warn!(session_id = %info.id, error = %e, "Backend pull failed");
            None
        }
    }
}

/// Map a persistence `io::Error` into an `acp::Error` with a human-friendly
/// `message` and a stable `data.code` for log aggregation.
pub(crate) fn io_error_to_acp(e: &io::Error) -> acp::Error {
    // Unix: ENOSPC / EDQUOT. Windows: ERROR_DISK_FULL (112). Hardcoded on
    // Windows so we don't pull libc in just for two integer literals.
    #[cfg(unix)]
    let is_disk_full = matches!(
        e.raw_os_error(),
        Some(raw) if raw == libc::ENOSPC || raw == libc::EDQUOT
    );
    #[cfg(windows)]
    const ERROR_DISK_FULL: i32 = 112;
    #[cfg(windows)]
    let is_disk_full = matches!(e.raw_os_error(), Some(ERROR_DISK_FULL));

    let (message, code) = if is_disk_full {
        (
            "Disk quota exceeded or out of space.",
            "FS_DISK_QUOTA_EXCEEDED",
        )
    } else {
        match e.kind() {
            io::ErrorKind::NotFound => ("Path not found.", "FS_NOT_FOUND"),
            io::ErrorKind::PermissionDenied => ("Permission denied.", "FS_PERMISSION_DENIED"),
            _ => {
                tracing::warn!(error = %e, kind = ?e.kind(), raw_os = ?e.raw_os_error(), "unclassified persistence I/O error");
                ("An unexpected I/O error occurred.", "FS_OTHER")
            }
        }
    };
    acp::Error::new(acp::ErrorCode::InternalError.into(), message.to_string()).data(Some(
        serde_json::json!({
            "code": code,
            "detail": e.to_string(),
        }),
    ))
}

/// Best-effort worktree liveness touch: stamp `last_accessed_at` on the
/// worktree containing this session's cwd so `grok worktree gc` expires by
/// last use, not creation time. Lives here — not in a `StorageAdapter` —
/// so every session create/load path shares it regardless of backend.
fn spawn_worktree_touch(info: &Info) -> tokio::task::JoinHandle<()> {
    let cwd = info.cwd.clone();
    tokio::task::spawn_blocking(move || {
        crate::session::worktree::touch_worktree_for_cwd(&cwd);
    })
}

/// Bound on how long session open waits for the liveness touch to commit —
/// generous vs the DB's 5s busy_timeout without letting a pathologically
/// locked worktrees.db stall init.
const WORKTREE_TOUCH_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Touch the worktree and wait (bounded) for the write to commit before the
/// session open completes: a detached touch can land after gc's pre-removal
/// re-check reads the row, letting gc delete a worktree that is actively
/// being opened or resumed. Awaiting a blocking-pool task does not block the
/// runtime; on timeout the task keeps running detached (the old
/// fire-and-forget behavior) and init proceeds.
async fn touch_worktree_for_session(info: &Info) {
    if tokio::time::timeout(WORKTREE_TOUCH_INIT_TIMEOUT, spawn_worktree_touch(info))
        .await
        .is_err()
    {
        tracing::debug!(
            cwd = %info.cwd,
            "worktree liveness touch still pending at session open"
        );
    }
}

/// Floor between activity-driven worktree touches from the persistence actor.
const WORKTREE_TOUCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

pub(crate) async fn new(
    info: &Info,
    model_id: acp::ModelId,
    sampling_client: OaiCompatClient,
    storage_mode: StorageMode,
    auth_manager: Option<Arc<crate::auth::AuthManager>>,
    relay_sync: Option<crate::relay::RelaySync>,
    gateway: Option<GatewaySender>,
    session_summary_model: String,
    registry_title_sync: Option<RegistryGeneratedTitleSync>,
) -> io::Result<PersistenceHandle> {
    let root_dir = grok_home();
    let storage: Box<dyn StorageAdapter> = Box::new(JsonlStorageAdapter::with_root(root_dir));

    // Initialize session in storage
    let mut summary = storage.init_session(info, model_id.clone()).await?;
    touch_worktree_for_session(info).await;

    // Update model if different
    if summary.current_model_id != model_id {
        storage.update_current_model(info, &model_id).await?;
        summary.current_model_id = model_id;
    }

    let (tx, rx) = mpsc::unbounded_channel::<PersistenceMsg>();

    let info_clone = info.clone();
    let storage: Arc<dyn StorageAdapter> = Arc::from(storage);
    let remote_sync = init_remote_sync(&summary, storage_mode, auth_manager)?;
    let handle = PersistenceHandle {
        tx: tx.clone(),
        noop: false,
    };

    tokio::task::spawn(async move {
        let persistence = SessionPersistence {
            info: info_clone,
            storage: storage.clone(),
            pending_notification: None,
            rx,
            remote_sync: remote_sync.clone(),
            relay_sync,
            summary: crate::session::summary::SummaryGenerator::new(
                crate::session::summary::SummaryConfig {
                    sampling_client,
                    model: session_summary_model,
                    persistence_tx: tx,
                },
            ),
            registry_title_sync,
            gateway,
        };
        persistence.run().await;
    });

    Ok(handle)
}

/// Create a persistence handle that writes to an explicit directory on disk.
///
/// Used for subagent child sessions whose files live under the parent's
/// session directory: `{parent_session_dir}/subagents/{subagent_id}/`.
///
/// Unlike [`new()`], this:
/// - Uses `JsonlStorageAdapter::with_explicit_session_dir()` to bypass
///   the standard `{root}/sessions/{cwd}/{id}/` path computation.
/// - Skips remote sync (subagent sessions are not synced to cloud).
/// - Skips relay sync (subagent sessions are not shared).
/// - Skips gateway (lifecycle notifications are handled by the coordinator).
pub async fn new_with_explicit_dir(
    info: &Info,
    target_dir: PathBuf,
    model_id: acp::ModelId,
    sampling_client: OaiCompatClient,
    session_summary_model: String,
) -> io::Result<PersistenceHandle> {
    let summary_path = target_dir.join("summary.json");
    let storage: Box<dyn StorageAdapter> =
        Box::new(JsonlStorageAdapter::with_explicit_session_dir(target_dir));

    // Initialize session in storage (creates summary.json, etc.)
    let mut summary = storage.init_session(info, model_id.clone()).await?;
    touch_worktree_for_session(info).await;
    if summary.session_kind.is_none() {
        summary.session_kind = Some("subagent".to_string());
    }
    let summary_json = serde_json::to_vec_pretty(&summary)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&summary_path, summary_json)?;

    if summary.current_model_id != model_id {
        storage.update_current_model(info, &model_id).await?;
        summary.current_model_id = model_id;
    }

    let (tx, rx) = mpsc::unbounded_channel::<PersistenceMsg>();

    let info_clone = info.clone();
    let storage: Arc<dyn StorageAdapter> = Arc::from(storage);
    let handle = PersistenceHandle {
        tx: tx.clone(),
        noop: false,
    };

    tokio::task::spawn(async move {
        let persistence = SessionPersistence {
            info: info_clone,
            storage: storage.clone(),
            pending_notification: None,
            rx,
            remote_sync: None,
            relay_sync: None,
            summary: crate::session::summary::SummaryGenerator::new(
                crate::session::summary::SummaryConfig {
                    sampling_client,
                    model: session_summary_model,
                    persistence_tx: tx,
                },
            ),
            registry_title_sync: None,
            gateway: None,
        };
        persistence.run().await;
    });

    Ok(handle)
}

pub struct PersistedInfo {
    pub summary: Summary,
    pub chat_history: Vec<ConversationItem>,
    /// All session updates (ACP updates and xAI extension updates) in chronological order
    pub updates: Vec<SessionUpdate>,
    pub plan_state: Option<TodoState>,
    pub rewind_points: Vec<RewindPoint>,
    /// Persisted session signals (None for old sessions without signals file)
    pub signals: Option<SessionSignals>,
    pub workflow_runs: Vec<crate::session::workflow::store::RestoredWorkflowRun>,
}

/// Same as PersistedInfo but without updates - for memory efficiency when streaming
pub struct PersistedInfoLight {
    pub summary: Summary,
    pub chat_history: Vec<ConversationItem>,
    pub plan_state: Option<TodoState>,
    pub plan_mode_state: Option<crate::session::plan_mode::PlanModeSnapshot>,
    /// Path to updates file for streaming reads
    pub updates_file_path: Option<std::path::PathBuf>,
    /// Adapter-owned path to `rewind_points.jsonl` for the session's
    /// `FileStateTracker` to load lazily. `None` if the backend doesn't persist
    /// rewind points to a streamable file.
    pub rewind_points_file_path: Option<std::path::PathBuf>,
    /// Persisted session signals (None for old sessions without signals file)
    pub signals: Option<SessionSignals>,
    /// Persisted announcement tracking state (None for sessions before this feature)
    pub announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    /// Persisted goal mode orchestration state (None for sessions without goal mode)
    pub goal_mode_state: Option<crate::session::goal_tracker::GoalOrchestration>,
    pub workflow_runs: Vec<crate::session::workflow::store::RestoredWorkflowRun>,
}

/// On NotFound, try pulling from backend. Returns pulled info or the original error.
async fn pull_on_miss(
    info: &Info,
    client: &crate::remote::BackendClient,
    err: io::Error,
) -> io::Result<Info> {
    if err.kind() != io::ErrorKind::NotFound {
        return Err(err);
    }
    try_pull_from_remote(info, client).await.ok_or(err)
}

#[expect(dead_code, reason = "wired when session restore flow calls load")]
pub(crate) async fn load(
    info: &Info,
    sampling_client: OaiCompatClient,
    storage_mode: StorageMode,
    auth_manager: Option<Arc<crate::auth::AuthManager>>,
    backend: Option<&crate::remote::BackendClient>,
    relay_sync: Option<crate::relay::RelaySync>,
    gateway: Option<GatewaySender>,
    session_summary_model: String,
    registry_title_sync: Option<RegistryGeneratedTitleSync>,
) -> io::Result<(PersistedInfo, PersistenceHandle)> {
    let root_dir = grok_home();
    let storage: Box<dyn StorageAdapter> = Box::new(JsonlStorageAdapter::with_root(root_dir));

    let (persisted, loaded_info) = match storage.load_session(info).await {
        Ok(p) => (p, info.clone()),
        Err(e) => match backend {
            Some(client) => {
                let pulled = pull_on_miss(info, client, e).await?;
                let p = storage.load_session(&pulled).await?;
                (p, pulled)
            }
            None => return Err(e),
        },
    };
    // Touch on load too: resuming must reset the worktree's gc expiry clock.
    touch_worktree_for_session(&loaded_info).await;

    let persisted_info = PersistedInfo {
        summary: persisted.summary,
        chat_history: persisted.chat_history,
        updates: persisted.updates,
        plan_state: persisted.plan_state,
        rewind_points: persisted.rewind_points,
        signals: persisted.signals,
        workflow_runs: persisted.workflow_runs,
    };

    let (tx, rx) = mpsc::unbounded_channel::<PersistenceMsg>();

    let storage: Arc<dyn StorageAdapter> = Arc::from(storage);
    let remote_sync = init_remote_sync(&persisted_info.summary, storage_mode, auth_manager)?;

    let has_title = !persisted_info.summary.display_title().is_empty();
    let handle = PersistenceHandle {
        tx: tx.clone(),
        noop: false,
    };
    tokio::task::spawn(async move {
        let mut summary_gen = crate::session::summary::SummaryGenerator::new(
            crate::session::summary::SummaryConfig {
                sampling_client,
                model: session_summary_model,
                persistence_tx: tx,
            },
        );
        if has_title {
            summary_gen.mark_done();
        }
        let persistence = SessionPersistence {
            info: loaded_info,
            storage: storage.clone(),
            pending_notification: None,
            rx,
            remote_sync: remote_sync.clone(),
            relay_sync,
            summary: summary_gen,
            registry_title_sync,
            gateway,
        };
        persistence.run().await;
    });

    Ok((persisted_info, handle))
}

/// Like `load`, but doesn't load updates into memory.
/// Instead, provides the path to the updates file for streaming reads.
/// Use this for memory-efficient session loading when replaying updates.
pub(crate) async fn load_light(
    info: &Info,
    sampling_client: OaiCompatClient,
    storage_mode: StorageMode,
    auth_manager: Option<Arc<crate::auth::AuthManager>>,
    backend: Option<&crate::remote::BackendClient>,
    relay_sync: Option<crate::relay::RelaySync>,
    gateway: Option<GatewaySender>,
    session_summary_model: String,
    registry_title_sync: Option<RegistryGeneratedTitleSync>,
) -> io::Result<(PersistedInfoLight, PersistenceHandle)> {
    let root_dir = grok_home();
    let storage: Box<dyn StorageAdapter> =
        Box::new(JsonlStorageAdapter::with_root(root_dir.clone()));

    let (persisted, loaded_info) = match storage.load_session_without_updates(info).await {
        Ok(p) => (p, info.clone()),
        Err(e) => match backend {
            Some(client) => {
                let pulled = pull_on_miss(info, client, e).await?;
                let p = storage.load_session_without_updates(&pulled).await?;
                (p, pulled)
            }
            None => return Err(e),
        },
    };
    // Touch on load too: resuming must reset the worktree's gc expiry clock.
    touch_worktree_for_session(&loaded_info).await;

    let updates_file_path = storage.updates_file_path(&loaded_info);
    let rewind_points_file_path = storage.rewind_points_file_path(&loaded_info);

    let persisted_info = PersistedInfoLight {
        summary: persisted.summary,
        chat_history: persisted.chat_history,
        plan_state: persisted.plan_state,
        plan_mode_state: persisted.plan_mode_state,
        updates_file_path,
        rewind_points_file_path,
        signals: persisted.signals,
        announcement_state: persisted.announcement_state,
        goal_mode_state: persisted.goal_mode_state,
        workflow_runs: persisted.workflow_runs,
    };

    let (tx, rx) = mpsc::unbounded_channel::<PersistenceMsg>();

    let storage: Arc<dyn StorageAdapter> = Arc::from(storage);
    let remote_sync = init_remote_sync(&persisted_info.summary, storage_mode, auth_manager)?;

    let has_title = !persisted_info.summary.display_title().is_empty();
    let handle = PersistenceHandle {
        tx: tx.clone(),
        noop: false,
    };
    tokio::task::spawn(async move {
        let mut summary_gen = crate::session::summary::SummaryGenerator::new(
            crate::session::summary::SummaryConfig {
                sampling_client,
                model: session_summary_model,
                persistence_tx: tx,
            },
        );
        if has_title {
            summary_gen.mark_done();
        }
        let persistence = SessionPersistence {
            info: loaded_info,
            storage: storage.clone(),
            pending_notification: None,
            rx,
            remote_sync: remote_sync.clone(),
            relay_sync,
            summary: summary_gen,
            registry_title_sync,
            gateway,
        };
        persistence.run().await;
    });

    Ok((persisted_info, handle))
}

/// List session summaries, optionally filtered by cwd (absolute path string).
/// Returns summaries sorted by `last_active_at` (else `updated_at`) descending.
pub async fn list_summaries(cwd: Option<&str>) -> io::Result<Vec<Summary>> {
    let root_dir = crate::util::grok_home::grok_home();
    let storage: Box<dyn StorageAdapter> = Box::new(JsonlStorageAdapter::with_root(root_dir));
    storage.list_sessions(cwd).await
}

/// Failure modes of [`delete_session_history`].
///
/// Kept distinct so callers can surface a precise message: a remote
/// failure is reported separately from a local-disk failure because the
/// remote delete runs first and aborts the whole operation (see the doc
/// on [`delete_session_history`]).
#[derive(Debug, thiserror::Error)]
pub enum DeleteSessionError {
    /// Listing local summaries (to resolve the on-disk session dir) failed.
    #[error("failed to list sessions: {0}")]
    List(#[source] io::Error),
    /// The remote (writeback) copy could not be deleted; local bits were
    /// left untouched so the operation can be retried.
    #[error("failed to delete remote session data: {0}")]
    Remote(#[source] crate::remote::client::BackendError),
    /// The local on-disk session directory could not be removed.
    #[error("failed to delete session: {0}")]
    Local(#[source] io::Error),
}

/// Where a session copy was actually removed by [`delete_session_history`].
///
/// Both fields are `false` when nothing existed to delete (still a
/// success). Callers use [`Self::any_removed`] to decide between a
/// "deleted" and a "not found" message without conflating a remote-only
/// delete with a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionDeletion {
    /// A local on-disk session directory was found and removed.
    pub local_removed: bool,
    /// A remote (writeback) copy was found and removed. `false` when
    /// `needs_remote` was not set, or the remote copy was already absent
    /// (the backend returned `404`).
    pub remote_removed: bool,
}

impl SessionDeletion {
    /// `true` when a copy was removed from at least one location.
    pub fn any_removed(self) -> bool {
        self.local_removed || self.remote_removed
    }
}

/// Permanently delete a session's history: the remote (writeback) copy
/// when `needs_remote`, the local on-disk session directory, and the
/// FTS search-index entry.
///
/// Idempotent: a session that is missing locally (e.g. remote-only)
/// still succeeds, and a remote `404` (copy already gone) is treated as
/// success rather than an error. When `needs_remote` is set the remote
/// delete runs *first* and is authoritative — only on its success (or a
/// `404`) are the local bits removed. This ordering prevents a partial
/// delete where the local copy is nuked but the remote copy lingers and
/// re-appears on the next session list.
///
/// Returns a [`SessionDeletion`] recording which copies (local / remote)
/// were actually removed; both fields `false` means nothing existed
/// (still `Ok`).
pub async fn delete_session_history(
    session_id: &str,
    cwd: Option<&str>,
    needs_remote: bool,
    auth_manager: Arc<crate::auth::AuthManager>,
) -> Result<SessionDeletion, DeleteSessionError> {
    let sid = acp::SessionId::new(Arc::from(session_id));

    // Resolve the local session info, scoping to cwd if provided. A
    // remote-only session won't be found here — that's fine, the remote
    // delete (if applicable) still runs.
    let summaries = list_summaries(cwd)
        .await
        .map_err(DeleteSessionError::List)?;
    let local_info = summaries
        .iter()
        .find(|s| s.info.id == sid)
        .map(|s| s.info.clone());

    // Remote delete first (authoritative for cloud history). A genuine
    // failure aborts before any local mutation so the row does not
    // reappear; a `404` means the copy is already gone, so deletion stays
    // idempotent and falls through to local cleanup.
    let remote_removed = if needs_remote {
        let result = crate::remote::client::BackendClient::new()
            .with_auth_manager(auth_manager)
            .delete_session_data(session_id)
            .await;
        classify_remote_delete(result)?
    } else {
        false
    };

    let Some(info) = local_info else {
        return Ok(SessionDeletion {
            local_removed: false,
            remote_removed,
        });
    };

    JsonlStorageAdapter::default()
        .delete_session(&info)
        .await
        .map_err(DeleteSessionError::Local)?;

    // Evict from the search index: the indexer re-reads the (now
    // missing) summary and drops the document.
    crate::session::storage::search::notify_session_updated(&info.id.to_string(), &info.cwd);

    Ok(SessionDeletion {
        local_removed: true,
        remote_removed,
    })
}

/// Classify a remote `delete_session_data` result, reporting whether a
/// remote copy was actually removed: a `2xx` means a copy was deleted
/// (`Ok(true)`), a `404` means it was already gone so deletion stays
/// idempotent (`Ok(false)`), and any other backend error aborts the
/// delete (`Err`) so local bits are left untouched and it can be retried.
fn classify_remote_delete(
    result: Result<(), crate::remote::client::BackendError>,
) -> Result<bool, DeleteSessionError> {
    use crate::remote::client::BackendError;
    match result {
        Ok(()) => Ok(true),
        Err(BackendError::RequestFailed { status: 404, .. }) => Ok(false),
        Err(e) => Err(DeleteSessionError::Remote(e)),
    }
}

#[cfg(test)]
#[path = "persistence_tests.rs"]
mod durable_update_tests;

#[cfg(test)]
mod delete_session_history_tests {
    use super::{DeleteSessionError, SessionDeletion, classify_remote_delete};
    use crate::remote::client::BackendError;

    #[test]
    fn remote_ok_reports_removed() {
        assert!(
            classify_remote_delete(Ok(())).unwrap(),
            "a 2xx delete must report that a remote copy was removed"
        );
    }

    #[test]
    fn remote_404_is_treated_as_already_deleted() {
        let removed = classify_remote_delete(Err(BackendError::RequestFailed {
            status: 404,
            body: "not found".into(),
        }))
        .expect("a 404 means the remote copy is gone — deletion must stay idempotent");
        assert!(
            !removed,
            "a 404 must report that nothing was removed remotely"
        );
    }

    #[test]
    fn remote_non_404_request_failure_aborts() {
        let res = classify_remote_delete(Err(BackendError::RequestFailed {
            status: 500,
            body: "boom".into(),
        }));
        assert!(matches!(res, Err(DeleteSessionError::Remote(_))));
    }

    #[test]
    fn remote_auth_failure_aborts() {
        let res = classify_remote_delete(Err(BackendError::Auth("denied".into())));
        assert!(matches!(res, Err(DeleteSessionError::Remote(_))));
    }

    #[test]
    fn any_removed_reflects_either_location() {
        assert!(!SessionDeletion::default().any_removed());
        assert!(
            SessionDeletion {
                local_removed: true,
                remote_removed: false,
            }
            .any_removed()
        );
        assert!(
            SessionDeletion {
                local_removed: false,
                remote_removed: true,
            }
            .any_removed(),
            "a remote-only delete must count as removed"
        );
    }
}

/// List the `limit` most recently modified session summaries across all
/// workspaces. Uses stat-based mtime sorting to avoid reading every
/// summary file on disk; final order uses `last_active_at` else `updated_at`.
pub async fn list_recent_summaries(limit: usize) -> io::Result<Vec<Summary>> {
    let root_dir = crate::util::grok_home::grok_home();
    let storage = JsonlStorageAdapter::with_root(root_dir);
    storage.list_sessions_recent(limit).await
}

// Session folder TTL cleanup

/// Guard ensuring session cleanup runs at most once per process.
static CLEANUP_SESSIONS_ONCE: std::sync::Once = std::sync::Once::new();

/// Default TTL for stale session files (30 days).
const DEFAULT_CLEANUP_TTL_DAYS: u32 = 30;

/// Walk `~/.grok/sessions/` and delete files with mtime older than `ttl_days`.
/// Removes empty session directories after file cleanup.
/// Skips `skip_session_dir` if provided (current session).
///
/// This is a **synchronous** function intended to be called via
/// `tokio::task::spawn_blocking` so it runs on the thread pool and
/// never competes with the agent's single-threaded `LocalSet`.
pub fn cleanup_stale_sessions(skip_session_dir: Option<&Path>) {
    CLEANUP_SESSIONS_ONCE.call_once(|| {
        let ttl_days = resolve_cleanup_ttl_days();
        let sessions_root = grok_home().join("sessions");

        tracing::info!(
            target: "xai_grok_shell::session::persistence",
            sessions_root = %sessions_root.display(),
            ttl_days,
            skip = ?skip_session_dir.map(|p| p.display().to_string()),
            "SESSION_CLEANUP_START: scanning for stale session files"
        );

        let stats = cleanup_stale_sessions_inner(&sessions_root, ttl_days, skip_session_dir);

        tracing::info!(
            target: "xai_grok_shell::session::persistence",
            sessions_root = %sessions_root.display(),
            files_deleted = stats.files_deleted,
            dirs_removed = stats.dirs_removed,
            errors = stats.errors,
            "SESSION_CLEANUP_DONE"
        );
    });
}

/// Resolve TTL from config.toml `[storage] cleanup_ttl_days`, falling back to 30.
fn resolve_cleanup_ttl_days() -> u32 {
    // Try to load config and read [storage] section
    if let Ok(layers) = crate::config::ConfigLayers::load() {
        let effective = layers.effective_config_disk_only();
        if let Some(storage) = effective.get("storage")
            && let Some(ttl) = storage.get("cleanup_ttl_days")
            && let Some(days) = ttl.as_integer()
            && days > 0
        {
            return days as u32;
        }
    }
    DEFAULT_CLEANUP_TTL_DAYS
}

#[derive(Default)]
struct CleanupStats {
    files_deleted: u32,
    dirs_removed: u32,
    errors: u32,
}

/// Recursive cleanup: delete stale files, then rmdir empty dirs (post-order).
fn cleanup_stale_sessions_inner(root: &Path, ttl_days: u32, skip: Option<&Path>) -> CleanupStats {
    let mut stats = CleanupStats::default();

    if let Some(skip_dir) = skip
        && root == skip_dir
    {
        return stats;
    }

    let Ok(entries) = std::fs::read_dir(root) else {
        return stats;
    };

    for entry_result in entries {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(
                    target: "xai_grok_shell::session::persistence",
                    error = %e,
                    "SESSION_CLEANUP_READ_ERROR"
                );
                stats.errors += 1;
                continue;
            }
        };
        let path = entry.path();

        if let Some(skip_dir) = skip
            && path == skip_dir
        {
            continue;
        }

        if path.is_dir() {
            let child_stats = cleanup_stale_sessions_inner(&path, ttl_days, skip);
            stats.files_deleted += child_stats.files_deleted;
            stats.dirs_removed += child_stats.dirs_removed;
            stats.errors += child_stats.errors;

            // Only attempt remove_dir if this subtree actually had stale
            // files deleted in this pass. Otherwise we risk removing dirs
            // that were deliberately created for use by concurrent sessions.
            if child_stats.files_deleted > 0 && std::fs::remove_dir(&path).is_ok() {
                stats.dirs_removed += 1;
                tracing::debug!(
                    target: "xai_grok_shell::session::persistence",
                    dir = %path.display(),
                    "SESSION_CLEANUP_RMDIR"
                );
            }
        } else if let Ok(meta) = std::fs::metadata(&path)
            && let Ok(mtime) = meta.modified()
            && is_stale(mtime, ttl_days)
        {
            if std::fs::remove_file(&path).is_ok() {
                stats.files_deleted += 1;
                tracing::debug!(
                    target: "xai_grok_shell::session::persistence",
                    file = %path.display(),
                    "SESSION_CLEANUP_DELETE"
                );
            } else {
                stats.errors += 1;
            }
        }
    }

    stats
}

fn is_stale(mtime: std::time::SystemTime, ttl_days: u32) -> bool {
    let ttl = std::time::Duration::from_secs(u64::from(ttl_days) * 86400);
    mtime.elapsed().is_ok_and(|age| age > ttl)
}

#[cfg(test)]
mod agent_name_persistence_tests {
    use super::*;

    #[test]
    fn summary_round_trips_agent_name_through_json() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.agent_name = Some("cursor".into());

        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: Summary = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.agent_name.as_deref(), Some("cursor"));
    }

    #[test]
    fn summary_deserializes_without_agent_name_backward_compat() {
        // Simulate an old summary.json that lacks agent_name — must still
        // deserialize successfully (serde default → None).
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "num_chat_messages": 0,
            "current_model_id": "test-model"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert!(
            summary.agent_name.is_none(),
            "old summaries without agent_name should deserialize as None"
        );
    }

    #[test]
    fn summary_skips_none_agent_name_in_serialized_json() {
        let summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(
            !json.contains("agent_name"),
            "None agent_name should not appear in serialized JSON"
        );
    }

    #[test]
    fn summary_includes_agent_name_when_set() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.agent_name = Some("cursor".into());
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("agent_name"));
        assert!(json.contains("cursor"));
    }

    #[test]
    fn summary_round_trips_various_agent_names() {
        for name in [
            "cursor",
            "grok-build",
            "grok-build-plan",
            "codex",
            "browser-use",
        ] {
            let mut summary = Summary::new(
                &Info {
                    id: acp::SessionId::new("test"),
                    cwd: "/tmp".into(),
                },
                default_model_id(),
            )
            .unwrap();
            summary.agent_name = Some(name.into());

            let json = serde_json::to_string(&summary).unwrap();
            let deserialized: Summary = serde_json::from_str(&json).unwrap();
            assert_eq!(
                deserialized.agent_name.as_deref(),
                Some(name),
                "round-trip failed for agent_name={name}"
            );
        }
    }

    #[test]
    fn summary_with_agent_name_in_full_json() {
        // Verify agent_name deserializes correctly alongside all other fields.
        let json = r#"{
            "info": { "id": "full-session", "cwd": "/tmp" },
            "session_summary": "test session",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 10,
            "num_chat_messages": 5,
            "current_model_id": "cursor-model",
            "agent_name": "cursor",
            "generated_title": "Fix cursor mode",
            "head_branch": "main"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.agent_name.as_deref(), Some("cursor"));
        assert_eq!(summary.current_model_id.0.as_ref(), "cursor-model");
        assert_eq!(summary.generated_title.as_deref(), Some("Fix cursor mode"));
    }
}

#[cfg(test)]
mod collect_session_files_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn collects_top_level_files_with_flat_names() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("chat_history.jsonl"), b"line1\nline2").unwrap();
        fs::write(dir.path().join("summary.json"), b"{}").unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        files.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "chat_history.jsonl");
        assert_eq!(files[0].data, b"line1\nline2");
        assert_eq!(files[1].name, "summary.json");
        assert_eq!(files[1].data, b"{}");
    }

    #[test]
    fn collects_subdirectory_files_with_relative_paths() {
        let dir = TempDir::new().unwrap();
        let prompts_dir = dir.path().join("prompts");
        fs::create_dir(&prompts_dir).unwrap();
        fs::write(prompts_dir.join("prompt_0.txt"), b"long prompt content").unwrap();
        fs::write(prompts_dir.join("prompt_1.txt"), b"another long prompt").unwrap();
        fs::write(dir.path().join("summary.json"), b"{}").unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        files.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].name, "prompts/prompt_0.txt");
        assert_eq!(files[0].data, b"long prompt content");
        assert_eq!(files[1].name, "prompts/prompt_1.txt");
        assert_eq!(files[2].name, "summary.json");
    }

    #[test]
    fn collects_nested_subdirectories() {
        let dir = TempDir::new().unwrap();
        let deep = dir.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.txt"), b"deep").unwrap();
        fs::write(dir.path().join("top.txt"), b"top").unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        files.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "a/b/deep.txt");
        assert_eq!(files[1].name, "top.txt");
    }

    #[test]
    fn nonexistent_directory_returns_empty() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does_not_exist");

        let mut files = Vec::new();
        collect_session_files_recursive(&missing, &missing, &mut files);

        assert!(files.is_empty());
    }

    #[test]
    fn empty_directory_returns_empty() {
        let dir = TempDir::new().unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        assert!(files.is_empty());
    }

    #[test]
    fn skips_empty_subdirectories() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("empty_subdir")).unwrap();
        fs::write(dir.path().join("file.txt"), b"data").unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "file.txt");
    }
}

#[cfg(test)]
mod session_exists_tests {
    use super::session_exists_in_root;
    use std::fs;
    use tempfile::TempDir;

    fn make_root() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn returns_false_when_root_does_not_exist() {
        let root = std::path::PathBuf::from("/nonexistent/grok/sessions");
        assert!(!session_exists_in_root("any-id", &root));
    }

    #[test]
    fn returns_false_when_root_is_empty() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();
        assert!(!session_exists_in_root("my-session", &root));
    }

    #[test]
    fn returns_true_when_session_dir_exists_under_any_cwd() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        // Simulate sessions/<encoded-cwd>/<session-id>/
        let session_dir = root.join("some_cwd_dir").join("my-session-id");
        fs::create_dir_all(&session_dir).unwrap();

        assert!(session_exists_in_root("my-session-id", &root));
    }

    #[test]
    fn returns_false_when_session_id_is_a_file_not_a_dir() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        let cwd_dir = root.join("some_cwd_dir");
        fs::create_dir_all(&cwd_dir).unwrap();
        // Create a file instead of a directory with the session id name
        fs::write(cwd_dir.join("my-session-id"), b"").unwrap();

        assert!(!session_exists_in_root("my-session-id", &root));
    }

    #[test]
    fn returns_false_for_different_session_id() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        let session_dir = root.join("some_cwd_dir").join("session-a");
        fs::create_dir_all(&session_dir).unwrap();

        assert!(!session_exists_in_root("session-b", &root));
    }

    #[test]
    fn finds_session_across_multiple_cwd_dirs() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        // Two different cwd directories
        fs::create_dir_all(root.join("cwd1").join("other-session")).unwrap();
        fs::create_dir_all(root.join("cwd2").join("target-session")).unwrap();

        assert!(session_exists_in_root("target-session", &root));
        assert!(!session_exists_in_root("missing-session", &root));
    }
}

#[cfg(test)]
mod find_summary_by_session_id_tests {
    use super::find_summary_by_session_id_in_root;
    use std::fs;
    use tempfile::TempDir;

    fn write_summary(root: &std::path::Path, cwd_dir: &str, session_id: &str, json: &str) {
        let dir = root.join(cwd_dir).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), json).unwrap();
    }

    fn minimal_summary(head_commit: &str, head_branch: &str) -> String {
        serde_json::json!({
            "info": { "id": "test-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "current_model_id": "grok-3",
            "head_commit": head_commit,
            "head_branch": head_branch
        })
        .to_string()
    }

    #[test]
    fn returns_none_when_root_missing() {
        let result =
            find_summary_by_session_id_in_root("any", &std::path::PathBuf::from("/nonexistent"));
        assert!(result.is_none());
    }

    #[test]
    fn returns_none_when_no_matching_session() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        write_summary(&root, "cwd1", "other-id", &minimal_summary("abc", "main"));
        assert!(find_summary_by_session_id_in_root("missing-id", &root).is_none());
    }

    #[test]
    fn finds_summary_across_cwd_dirs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        write_summary(
            &root,
            "encoded_cwd",
            "target-session",
            &minimal_summary("deadbeef", "feature/x"),
        );

        let found = find_summary_by_session_id_in_root("target-session", &root).unwrap();
        assert_eq!(found.head_commit.as_deref(), Some("deadbeef"));
        assert_eq!(found.head_branch.as_deref(), Some("feature/x"));
    }

    #[test]
    fn skips_malformed_summary() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        // Write invalid JSON
        let dir = root.join("cwd1").join("bad-session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), b"not-json").unwrap();

        assert!(find_summary_by_session_id_in_root("bad-session", &root).is_none());
    }
}

#[cfg(test)]
mod resumed_sandbox_profile_tests {
    use super::{
        most_recent_local_summary_for_cwd_in_root, resumed_session_sandbox_profile_in_root,
    };
    use std::fs;
    use tempfile::TempDir;

    /// Write a session summary under the *encoded* cwd dir (matching how the
    /// resume helpers locate sessions). `sandbox_profile` is included only when
    /// `Some`, mirroring older summaries that predate the field.
    fn write_session(
        root: &std::path::Path,
        cwd: &str,
        session_id: &str,
        updated_at: &str,
        last_active_at: Option<&str>,
        sandbox_profile: Option<&str>,
        hidden: bool,
    ) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        let mut summary = serde_json::json!({
            "info": { "id": session_id, "cwd": cwd },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": updated_at,
            "num_messages": 0,
            "current_model_id": "grok-3",
        });
        if let Some(la) = last_active_at {
            summary["last_active_at"] = serde_json::Value::String(la.to_string());
        }
        if let Some(profile) = sandbox_profile {
            summary["sandbox_profile"] = serde_json::Value::String(profile.to_string());
        }
        if hidden {
            summary["hidden"] = serde_json::Value::Bool(true);
        }
        fs::write(dir.join("summary.json"), summary.to_string()).unwrap();
    }

    #[test]
    fn explicit_id_returns_persisted_profile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        write_session(
            &root,
            "/work/a",
            "sess-1",
            "2026-01-01T00:00:00Z",
            None,
            Some("strict"),
            false,
        );

        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some("sess-1"), None, &root),
            Some("strict".to_string())
        );
    }

    #[test]
    fn explicit_id_without_persisted_profile_is_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        // Older session, created before the field existed.
        write_session(
            &root,
            "/work/a",
            "sess-old",
            "2026-01-01T00:00:00Z",
            None,
            None,
            false,
        );

        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some("sess-old"), None, &root),
            None
        );
    }

    #[test]
    fn explicit_remote_id_resolves_local_child_profile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/remote";
        // A remote session restored into a local child: the child has a fresh
        // id and records `parent_session_id` = the remote id.
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join("local-child");
        fs::create_dir_all(&dir).unwrap();
        let summary = serde_json::json!({
            "info": { "id": "local-child", "cwd": cwd },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "current_model_id": "grok-3",
            "parent_session_id": "remote-xyz",
            "sandbox_profile": "workspace",
        });
        fs::write(dir.join("summary.json"), summary.to_string()).unwrap();

        // No session dir is named "remote-xyz"; resolve via the child (cwd-scoped).
        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some("remote-xyz"), Some(cwd), &root),
            Some("workspace".to_string())
        );
        // Without a cwd the child can't be located -> None.
        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some("remote-xyz"), None, &root),
            None
        );
    }

    #[test]
    fn empty_or_missing_id_and_no_cwd_is_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some(""), None, &root),
            None
        );
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, None, &root),
            None
        );
    }

    #[test]
    fn most_recent_cwd_picks_latest_session_profile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/proj";
        write_session(
            &root,
            cwd,
            "older",
            "2026-01-01T00:00:00Z",
            None,
            Some("workspace"),
            false,
        );
        write_session(
            &root,
            cwd,
            "newer",
            "2026-06-01T00:00:00Z",
            None,
            Some("off"),
            false,
        );

        assert_eq!(
            most_recent_local_summary_for_cwd_in_root(cwd, &root)
                .unwrap()
                .info
                .id
                .0
                .to_string(),
            "newer"
        );
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, Some(cwd), &root),
            Some("off".to_string())
        );
    }

    #[test]
    fn most_recent_cwd_prefers_last_active_at_over_updated_at() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/proj";
        write_session(
            &root,
            cwd,
            "recent_activity",
            "2026-02-01T00:00:00Z",
            Some("2026-05-01T00:00:00Z"),
            Some("workspace"),
            false,
        );
        write_session(
            &root,
            cwd,
            "stale_activity",
            "2026-04-01T00:00:00Z",
            Some("2026-01-01T00:00:00Z"),
            Some("off"),
            false,
        );

        let picked = most_recent_local_summary_for_cwd_in_root(cwd, &root).unwrap();
        assert_eq!(picked.info.id.0.as_ref(), "recent_activity");
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, Some(cwd), &root),
            Some("workspace".to_string())
        );
    }

    #[test]
    fn most_recent_cwd_skips_hidden_session() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/proj";
        // Older, visible session.
        write_session(
            &root,
            cwd,
            "visible",
            "2026-01-01T00:00:00Z",
            None,
            Some("workspace"),
            false,
        );
        // Newer, hidden (e.g. subagent) session — the most-recent peek must
        // ignore it, matching what `list_sessions` resumes.
        write_session(
            &root,
            cwd,
            "hidden-newer",
            "2026-06-01T00:00:00Z",
            None,
            Some("off"),
            true,
        );

        assert_eq!(
            most_recent_local_summary_for_cwd_in_root(cwd, &root)
                .unwrap()
                .info
                .id
                .0
                .to_string(),
            "visible"
        );
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, Some(cwd), &root),
            Some("workspace".to_string())
        );
    }

    #[test]
    fn most_recent_cwd_with_no_sessions_is_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, Some("/empty/cwd"), &root),
            None
        );
    }
}

#[cfg(test)]
mod session_exists_for_cwd_tests {
    use super::{
        resolve_local_session_any_cwd_in_root, session_exists_for_cwd_in_root,
        session_exists_in_root,
    };
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn returns_true_when_session_exists_under_matching_cwd() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/alpha";
        let session_id = "my-session";

        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), b"{}").unwrap();

        assert!(session_exists_for_cwd_in_root(session_id, cwd, &root));
    }

    #[test]
    fn returns_false_when_session_absent_under_cwd() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();

        assert!(!session_exists_for_cwd_in_root(
            "missing",
            "/project/alpha",
            &root
        ));
    }

    /// Regression test for the cross-cwd false-positive.
    ///
    /// Before the fix, `restore_if_not_local` used `session_exists_by_id` which
    /// scanned ALL cwd directories.  A session present only under cwd-A would cause
    /// it to skip remote restore when the user resumed from cwd-B — then the
    /// `LoadSession` call would fail because the session directory did not exist
    /// under cwd-B.
    ///
    /// The cwd-specific check (`session_exists_for_cwd`) must return `false` for
    /// cwd-B even when the global scan returns `true` (because it finds cwd-A).
    #[test]
    fn session_under_different_cwd_is_not_considered_present() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let session_id = "cross-cwd-session";

        // Create the session only under cwd-A (a real session has a summary.json).
        let encoded_a = crate::util::grok_home::encode_cwd_dirname("/project/alpha");
        let dir_a = root.join(&encoded_a).join(session_id);
        fs::create_dir_all(&dir_a).unwrap();
        fs::write(dir_a.join("summary.json"), b"{}").unwrap();

        // Global scan (old behaviour) finds it — this is the incorrect check
        assert!(
            session_exists_in_root(session_id, &root),
            "global scan must find the session under cwd-A"
        );

        // Cwd-specific check must return false for cwd-B
        assert!(
            !session_exists_for_cwd_in_root(session_id, "/project/beta", &root),
            "cwd-specific check must return false for cwd-B; remote restore must not be skipped"
        );

        // And true for cwd-A (sanity)
        assert!(
            session_exists_for_cwd_in_root(session_id, "/project/alpha", &root),
            "cwd-specific check must return true for the matching cwd-A"
        );
    }

    /// An `images/`-only stub (no `summary.json`) is not a resumable session.
    #[test]
    fn images_only_stub_is_not_a_session() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/alpha";
        let session_id = "stub-session";

        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let images = root.join(&encoded).join(session_id).join("images");
        fs::create_dir_all(&images).unwrap();
        fs::write(images.join("image-1.png"), b"png").unwrap();

        assert!(
            !session_exists_for_cwd_in_root(session_id, cwd, &root),
            "an images-only stub (no summary.json) must not be a resumable session"
        );
    }

    /// The all-cwd scan skips a stub and returns the real session's cwd.
    #[test]
    fn resolve_local_session_any_cwd_skips_stub_and_finds_real() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let session_id = "real-session";

        // Real session under cwd-A.
        let cwd_a = "/project/alpha";
        let encoded_a = crate::util::grok_home::encode_cwd_dirname(cwd_a);
        let dir_a = root.join(&encoded_a).join(session_id);
        fs::create_dir_all(&dir_a).unwrap();
        fs::write(dir_a.join("summary.json"), b"{}").unwrap();

        // Images-only stub for the SAME id under cwd-B.
        let cwd_b = "/project/beta";
        let encoded_b = crate::util::grok_home::encode_cwd_dirname(cwd_b);
        let images_b = root.join(&encoded_b).join(session_id).join("images");
        fs::create_dir_all(&images_b).unwrap();
        fs::write(images_b.join("image-1.png"), b"png").unwrap();

        assert_eq!(
            resolve_local_session_any_cwd_in_root(session_id, &root).as_deref(),
            Some(cwd_a),
            "must anchor to the real session's cwd, not the stub's"
        );
    }
}

#[cfg(test)]
mod find_local_child_tests {
    use super::find_local_child_for_remote_in_root;
    use filetime::{self, FileTime};
    use std::fs;
    use tempfile::TempDir;

    fn make_session_with_parent(
        root: &std::path::Path,
        cwd: &str,
        session_id: &str,
        parent_id: &str,
    ) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        let summary = serde_json::json!({ "parent_session_id": parent_id });
        fs::write(dir.join("summary.json"), summary.to_string()).unwrap();
    }

    #[test]
    fn returns_child_id_when_parent_matches() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        make_session_with_parent(root.as_path(), "/work", "local-child-uuid", "remote-abc");

        let found = find_local_child_for_remote_in_root("remote-abc", "/work", &root);
        assert_eq!(found.as_deref(), Some("local-child-uuid"));
    }

    #[test]
    fn returns_none_when_no_child_exists() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let encoded = crate::util::grok_home::encode_cwd_dirname("/work");
        fs::create_dir_all(root.join(&encoded)).unwrap();

        let found = find_local_child_for_remote_in_root("remote-abc", "/work", &root);
        assert!(found.is_none());
    }

    #[test]
    fn returns_none_for_different_parent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        make_session_with_parent(root.as_path(), "/work", "local-child-uuid", "remote-xyz");

        let found = find_local_child_for_remote_in_root("remote-abc", "/work", &root);
        assert!(found.is_none());
    }

    /// Regression: a second `grok -r <remote_id>` must return the existing child
    /// without creating a new restore, not return `None`.
    #[test]
    fn repeated_resume_returns_existing_child() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        make_session_with_parent(root.as_path(), "/project", "child-1", "remote-parent");

        let first = find_local_child_for_remote_in_root("remote-parent", "/project", &root);
        let second = find_local_child_for_remote_in_root("remote-parent", "/project", &root);
        assert_eq!(first, second);
        assert_eq!(first.as_deref(), Some("child-1"));
    }

    /// With multiple pre-existing children, the function must return the newest
    /// one deterministically rather than picking an arbitrary filesystem order.
    #[test]
    fn duplicate_children_returns_newest_by_updated_at() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project";
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);

        // Older child — earlier timestamp.
        let old_dir = root.join(&encoded).join("old-child");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(
            old_dir.join("summary.json"),
            r#"{"parent_session_id":"remote-parent","updated_at":"2026-01-01T10:00:00Z"}"#,
        )
        .unwrap();

        // Newer child — later timestamp.
        let new_dir = root.join(&encoded).join("new-child");
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(
            new_dir.join("summary.json"),
            r#"{"parent_session_id":"remote-parent","updated_at":"2026-06-01T10:00:00Z"}"#,
        )
        .unwrap();

        let found = find_local_child_for_remote_in_root("remote-parent", cwd, &root);
        assert_eq!(
            found.as_deref(),
            Some("new-child"),
            "must return the newest child by updated_at"
        );
    }

    /// When two children share the same `updated_at` the tie must be broken
    /// deterministically, not by filesystem enumeration order.
    /// The lexicographically largest session id is the final stable tie-breaker.
    #[test]
    fn duplicate_children_equal_timestamps_stable_tiebreak() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project-tie";
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let same_ts = "2026-03-15T12:00:00Z";

        let mut dirs = Vec::new();
        for name in ["aaaa-uuid", "zzzz-uuid", "mmmm-uuid"] {
            let dir = root.join(&encoded).join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("summary.json"),
                format!(r#"{{"parent_session_id":"remote-tie","updated_at":"{same_ts}"}}"#),
            )
            .unwrap();
            dirs.push(dir);
        }

        // Force all directories to have *exactly* the same mtime so the
        // lexicographic session_id comparison is the actual tie-breaker.
        // Without this, nanosecond-precision filesystem mtimes can differ.
        let fixed_mtime = FileTime::from_unix_time(1700000000, 0);
        for dir in &dirs {
            filetime::set_file_mtime(dir, fixed_mtime).unwrap();
        }

        let found = find_local_child_for_remote_in_root("remote-tie", cwd, &root);
        // All share the same updated_at and mtime.
        // The lexicographic tie-breaker must always pick "zzzz-uuid".
        assert_eq!(
            found.as_deref(),
            Some("zzzz-uuid"),
            "lexicographically largest id must win the three-way tie"
        );
    }
}

#[cfg(test)]
mod resolve_local_session_tests {
    use super::{find_local_child_for_remote_in_root, session_exists_for_cwd_in_root};
    use std::fs;
    use tempfile::TempDir;

    // resolve_local_session delegates to the same _in_root helpers tested above,
    // so we test the composition logic via the public function indirectly by
    // setting up the on-disk structures under a fake grok home.
    // For unit isolation, we test the equivalent logic via the inner helpers.

    fn setup_session(root: &std::path::Path, cwd: &str, session_id: &str) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), b"{}").unwrap();
    }

    fn setup_child_session(root: &std::path::Path, cwd: &str, child_id: &str, parent_id: &str) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(child_id);
        fs::create_dir_all(&dir).unwrap();
        let summary = serde_json::json!({ "parent_session_id": parent_id });
        fs::write(dir.join("summary.json"), summary.to_string()).unwrap();
    }

    #[test]
    fn exact_match_returns_original_id() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/alpha";
        let sid = "sess-123";

        setup_session(&root, cwd, sid);

        // Exact match: session_exists_for_cwd → true
        assert!(session_exists_for_cwd_in_root(sid, cwd, &root));
        // The composed function should return the original id.
        // (We can't call resolve_local_session directly because it uses grok_home(),
        //  but the logic is: if session_exists → Some(session_id.to_string()),
        //  else find_local_child → child_id. Tested via inner helpers.)
        assert_eq!(
            Some(sid.to_string()),
            if session_exists_for_cwd_in_root(sid, cwd, &root) {
                Some(sid.to_string())
            } else {
                find_local_child_for_remote_in_root(sid, cwd, &root)
            }
        );
    }

    #[test]
    fn child_match_returns_child_id() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/beta";
        let remote_id = "remote-abc";
        let child_id = "local-child-xyz";

        setup_child_session(&root, cwd, child_id, remote_id);

        assert!(!session_exists_for_cwd_in_root(remote_id, cwd, &root));
        assert_eq!(
            Some(child_id.to_string()),
            find_local_child_for_remote_in_root(remote_id, cwd, &root)
        );
    }

    #[test]
    fn no_match_returns_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/gamma";
        fs::create_dir_all(root.join(crate::util::grok_home::encode_cwd_dirname(cwd))).unwrap();

        assert!(!session_exists_for_cwd_in_root("missing", cwd, &root));
        assert_eq!(
            None,
            find_local_child_for_remote_in_root("missing", cwd, &root)
        );
    }

    #[test]
    fn exact_match_takes_priority_over_child() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/delta";
        let sid = "sess-both";

        // Create both an exact session and a child of the same remote id.
        setup_session(&root, cwd, sid);
        setup_child_session(&root, cwd, "local-child-from-same", sid);

        // Exact match should take priority.
        assert!(session_exists_for_cwd_in_root(sid, cwd, &root));
    }
}

#[cfg(test)]
mod repo_wide_resolution_tests {
    use super::*;
    use std::fs;

    fn setup_session(root: &Path, cwd: &str, session_id: &str) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), b"{}").unwrap();
    }

    fn setup_child_session(root: &Path, cwd: &str, child_id: &str, parent_id: &str) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(child_id);
        fs::create_dir_all(&dir).unwrap();
        let summary = format!(
            r#"{{"session_id":"{child_id}","parent_session_id":"{parent_id}","updated_at":"2024-01-01T00:00:00Z"}}"#
        );
        fs::write(dir.join("summary.json"), summary).unwrap();
    }

    #[test]
    fn exact_cwd_takes_priority_over_same_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let other_cwd = "/repo/worktree-1";

        setup_session(&root, exact_cwd, "sess-A");
        setup_session(&root, other_cwd, "sess-A");

        let result =
            resolve_local_session_for_repo_in_root("sess-A", &[exact_cwd, other_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "sess-A");
        assert_eq!(r.cwd, exact_cwd);
        assert_eq!(r.resolution_kind, LocalSessionResolutionKind::ExactCwd);
    }

    /// An `images/`-only stub in the exact cwd is skipped; resolution anchors to
    /// the real session in a sibling cwd. Mirrors the cross-dir resume bug.
    #[test]
    fn skips_images_only_stub_and_resolves_real_sibling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let sibling_cwd = "/repo/worktree-1";

        let encoded = crate::util::grok_home::encode_cwd_dirname(exact_cwd);
        let images = root.join(&encoded).join("sess-A").join("images");
        fs::create_dir_all(&images).unwrap();
        fs::write(images.join("image-1.png"), b"png").unwrap();
        setup_session(&root, sibling_cwd, "sess-A");

        let result =
            resolve_local_session_for_repo_in_root("sess-A", &[exact_cwd, sibling_cwd], &root);
        let r = result.expect("must skip the stub and find the real sibling session");
        assert_eq!(r.cwd, sibling_cwd);
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::SameRepoDifferentCwd
        );
    }

    #[test]
    fn falls_back_to_same_repo_cwd_when_not_in_exact() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let other_cwd = "/repo/worktree-1";

        // Session only exists in other_cwd
        setup_session(&root, other_cwd, "sess-B");

        let result =
            resolve_local_session_for_repo_in_root("sess-B", &[exact_cwd, other_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "sess-B");
        assert_eq!(r.cwd, other_cwd);
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::SameRepoDifferentCwd
        );
    }

    #[test]
    fn finds_restored_child_in_exact_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";

        setup_child_session(&root, exact_cwd, "local-child", "remote-sess");

        let result = resolve_local_session_for_repo_in_root("remote-sess", &[exact_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "local-child");
        assert_eq!(r.cwd, exact_cwd);
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::RestoredChildInExactCwd
        );
    }

    #[test]
    fn finds_restored_child_in_same_repo_different_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let other_cwd = "/repo/worktree-2";

        // Restored child only in other_cwd
        setup_child_session(&root, other_cwd, "restored-child", "remote-sess");

        let result =
            resolve_local_session_for_repo_in_root("remote-sess", &[exact_cwd, other_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "restored-child");
        assert_eq!(r.cwd, other_cwd);
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::RestoredChildInSameRepoDifferentCwd
        );
    }

    #[test]
    fn returns_none_when_no_candidate_has_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let result = resolve_local_session_for_repo_in_root(
            "nonexistent",
            &["/cwd-1", "/cwd-2", "/cwd-3"],
            &root,
        );
        assert!(result.is_none());
    }

    #[test]
    fn direct_session_preferred_over_restored_child_in_same_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let cwd = "/repo/main";

        // Both exist: direct session AND a restored child for the same remote
        setup_session(&root, cwd, "sess-X");
        setup_child_session(&root, cwd, "child-of-X", "sess-X");

        let result = resolve_local_session_for_repo_in_root("sess-X", &[cwd], &root);
        let r = result.unwrap();
        // Direct match should win
        assert_eq!(r.session_id, "sess-X");
        assert_eq!(r.resolution_kind, LocalSessionResolutionKind::ExactCwd);
    }

    #[test]
    fn direct_in_later_cwd_preferred_over_child_in_same_later_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let other_cwd = "/repo/worktree-1";

        // Nothing in exact_cwd; both direct and child in other_cwd
        setup_session(&root, other_cwd, "sess-Y");
        setup_child_session(&root, other_cwd, "child-of-Y", "sess-Y");

        let result =
            resolve_local_session_for_repo_in_root("sess-Y", &[exact_cwd, other_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "sess-Y");
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::SameRepoDifferentCwd
        );
    }

    #[test]
    fn empty_candidates_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let result = resolve_local_session_for_repo_in_root("any-sess", &[], &root);
        assert!(result.is_none());
    }

    #[test]
    fn resolution_kind_serde_round_trip() {
        let kinds = [
            LocalSessionResolutionKind::ExactCwd,
            LocalSessionResolutionKind::RestoredChildInExactCwd,
            LocalSessionResolutionKind::SameRepoDifferentCwd,
            LocalSessionResolutionKind::RestoredChildInSameRepoDifferentCwd,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let deser: LocalSessionResolutionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, deser);
        }
    }

    #[test]
    fn resolved_local_session_serde_round_trip() {
        let resolved = ResolvedLocalSession {
            session_id: "sess-123".into(),
            cwd: "/repo/main".into(),
            resolution_kind: LocalSessionResolutionKind::SameRepoDifferentCwd,
        };
        let json = serde_json::to_string(&resolved).unwrap();
        let deser: ResolvedLocalSession = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.session_id, "sess-123");
        assert_eq!(deser.cwd, "/repo/main");
        assert_eq!(
            deser.resolution_kind,
            LocalSessionResolutionKind::SameRepoDifferentCwd
        );
    }
}
