//! File-backed data store for the local dashboard.
//!
//! The dashboard deliberately reads the same persisted artifacts the agent
//! already owns. It never mutates a session. All paths are kept below the
//! configured Grok home and symlinks are rejected before files are opened.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tracing::debug;

const SUMMARY_FILE: &str = "summary.json";
const SIGNALS_FILE: &str = "signals.json";
const EVENTS_FILE: &str = "events.jsonl";
const CHAT_HISTORY_FILE: &str = "chat_history.jsonl";
const UNIFIED_LOG_FILE: &str = "unified.jsonl";
const INDEX_TTL: Duration = Duration::from_secs(15);
const MAX_JSON_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TIMELINE_TAIL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CHAT_TAIL_BYTES: u64 = 24 * 1024 * 1024;
const MAX_LOG_TAIL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MESSAGE_CHARS: usize = 100_000;

/// High-level metadata used by overview and session-list pages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    pub cwd: String,
    pub model_id: String,
    pub agent_name: Option<String>,
    pub session_kind: Option<String>,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    pub turn_count: u32,
    pub tokens_used: u64,
    pub context_window_tokens: u64,
    pub context_window_usage: u8,
    pub tool_call_count: u32,
    pub error_count: u32,
    pub compaction_count: u32,
    pub avg_response_time_ms: Option<u64>,
    pub session_duration_seconds: u64,
    pub peak_rss_bytes: Option<u64>,
    pub is_active: bool,
    pub git_branch: Option<String>,
}

/// The `info` object persisted in summary.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryInfo {
    pub id: String,
    pub cwd: String,
}

/// Subset of summary.json needed by the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
pub struct Summary {
    pub info: SummaryInfo,
    #[serde(default)]
    pub session_summary: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_active_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub num_messages: usize,
    #[serde(default)]
    pub num_chat_messages: usize,
    #[serde(default)]
    pub current_model_id: String,
    pub session_kind: Option<String>,
    pub git_root_dir: Option<String>,
    #[serde(default)]
    pub git_remotes: Vec<String>,
    pub head_commit: Option<String>,
    pub head_branch: Option<String>,
    pub agent_name: Option<String>,
    pub generated_title: Option<String>,
    pub parent_session_id: Option<String>,
    pub hidden: Option<bool>,
    pub sandbox_profile: Option<String>,
    pub reasoning_effort: Option<serde_json::Value>,
}

/// Subset of signals.json used by the dashboard. Defaults keep old sessions
/// readable as fields are added over time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionSignals {
    pub turn_count: u32,
    pub user_message_count: u32,
    pub assistant_message_count: u32,
    pub error_count: u32,
    pub tool_failure_count: u32,
    pub cancellation_count: u32,
    pub compaction_count: u32,
    pub total_tokens_before_compaction: u64,
    pub context_window_usage: u8,
    pub context_tokens_used: u64,
    pub context_window_tokens: u64,
    pub tool_call_count: u32,
    pub tools_used: Vec<String>,
    pub models_used: Vec<String>,
    pub primary_model_id: Option<String>,
    pub session_duration_seconds: u64,
    pub avg_time_to_first_token_ms: Option<u64>,
    pub avg_response_time_ms: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    pub meta: SessionMeta,
    pub summary: Summary,
    pub signals: Option<SessionSignals>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub timestamp: Option<String>,
    pub tool_calls: Vec<ToolCallInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallInfo {
    pub name: String,
    pub call_id: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    TurnStarted,
    TurnEnded,
    PhaseChanged,
    FirstToken,
    LoopStarted,
    ToolStarted,
    ToolCompleted,
    PermissionRequested,
    PermissionResolved,
    Interjected,
    YoloToggled,
    GoalAutoPaused,
    TodoGateFired,
    TodoGateExhausted,
    LazinessClassifierFired,
    LazinessNudgeFired,
    LazinessClassifierAborted,
    GoalClassifierFired,
    GoalClassifierVerdict,
    GoalClassifierFailOpen,
    GoalClassifierFailClosed,
    GoalClassifierCapReached,
    GoalClassifierMidTurnDeferred,
    GoalClassifierDroppedAfterCap,
    GoalClassifierPendingQueueCleared,
    GoalPlannerFired,
    GoalPlannerCompleted,
    GoalPlannerFailClosed,
    GoalStrategistFired,
    GoalStrategistCompleted,
    GoalStrategistFailed,
    GoalStrategistContractRestoreFailed,
    GoalSummarizerFired,
    GoalSummarizerCompleted,
    GoalSummarizerFailOpen,
    GoalRoleModelResolved,
    GoalRoleModelFailOpen,
    GoalVerifierSkepticVerdict,
    GoalVerifierAggregateVerdict,
    GoalPrematureStopDetected,
    McpConfigResolved,
    McpManagedConfigResult,
    McpOauthDiscoveryTimeout,
    McpServerStarting,
    McpServerConnected,
    McpServerFailed,
    McpToolRegistrationFailed,
    McpInitCompleted,
    McpInitCancelled,
    McpToolCallStarted,
    McpToolCallCompleted,
    McpTransportError,
    McpTransportDecodeError,
    McpTransportReconnect,
    McpAuthRetry,
    McpHealthCheck,
    McpServerToggled,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
pub struct TimelineEvent {
    pub ts: DateTime<Utc>,
    #[serde(rename = "type")]
    pub kind: EventKind,
    pub session_id: Option<String>,
    pub turn_number: Option<u64>,
    pub model_id: Option<String>,
    pub phase: Option<String>,
    pub tool_name: Option<String>,
    pub duration_ms: Option<u64>,
    pub outcome: Option<String>,
    #[serde(default, flatten)]
    pub details: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub timestamp: String,
    pub source: String,
    pub pid: Option<u32>,
    pub level: String,
    pub session_id: Option<String>,
    pub message: String,
    pub context: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedMetric {
    pub name: String,
    pub value: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCharts {
    pub event_counts: Vec<NamedMetric>,
    pub tool_duration_ms: Vec<NamedMetric>,
    pub context_tokens_used: u64,
    pub context_window_tokens: u64,
    pub context_window_usage: u8,
    pub error_count: u32,
    pub compaction_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerMetrics {
    pub total_sessions: usize,
    pub active_sessions: usize,
    pub total_tokens_used: u64,
    pub total_tool_calls: u64,
    pub total_turns: u64,
    pub total_errors: u64,
    pub total_compactions: u64,
    pub avg_response_time_ms: Option<u64>,
    pub models: Vec<NamedMetric>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessResource {
    pub session_id: String,
    pub pid: u32,
    pub cwd: String,
    pub opened_at: String,
    pub rss_bytes: Option<u64>,
    pub footprint_bytes: Option<u64>,
    pub allocated_bytes: Option<u64>,
    pub sample_timestamp_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMetrics {
    pub grok_home: String,
    pub sessions_bytes: u64,
    pub logs_bytes: u64,
    pub memtrace_bytes: u64,
    pub processes: Vec<ProcessResource>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionQuery {
    pub query: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub active: Option<bool>,
    pub offset: usize,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    pub items: Vec<SessionMeta>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ActiveSessionEntry {
    pub session_id: String,
    pub pid: u32,
    pub cwd: String,
    pub opened_at: String,
}

struct CachedIndex {
    loaded_at: Instant,
    sessions: Vec<SessionMeta>,
    paths: HashMap<String, PathBuf>,
}

/// Read-only dashboard store.
pub struct DashboardStore {
    grok_home: PathBuf,
    active_sessions: RwLock<HashMap<String, ActiveSessionEntry>>,
    index: RwLock<Option<Arc<CachedIndex>>>,
    refresh_gate: Mutex<()>,
}

impl DashboardStore {
    pub fn new(grok_home: PathBuf) -> Self {
        Self {
            grok_home,
            active_sessions: RwLock::new(HashMap::new()),
            index: RwLock::new(None),
            refresh_gate: Mutex::new(()),
        }
    }

    pub fn grok_home(&self) -> &Path {
        &self.grok_home
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.grok_home.join("sessions")
    }

    pub async fn refresh_active_sessions(&self) -> Result<()> {
        let path = self.grok_home.join("active_sessions.json");
        let entries = match read_bounded_async(&path, MAX_JSON_BYTES).await {
            Ok(bytes) => serde_json::from_slice::<Vec<ActiveSessionEntry>>(&bytes)
                .with_context(|| format!("parse {}", path.display()))?,
            Err(error) if is_not_found(&error) => Vec::new(),
            Err(error) => return Err(error),
        };
        let mut active = self.active_sessions.write().await;
        *active = entries
            .into_iter()
            .map(|entry| (entry.session_id.clone(), entry))
            .collect();
        Ok(())
    }

    pub async fn active_session_entries(&self) -> Vec<ActiveSessionEntry> {
        if let Err(error) = self.refresh_active_sessions().await {
            debug!(%error, "unable to refresh active sessions");
        }
        self.active_sessions
            .read()
            .await
            .values()
            .cloned()
            .collect()
    }

    pub async fn invalidate(&self) {
        *self.index.write().await = None;
    }

    async fn ensure_index(&self) -> Result<Arc<CachedIndex>> {
        if let Some(index) = self.index.read().await.as_ref()
            && index.loaded_at.elapsed() < INDEX_TTL
        {
            return Ok(index.clone());
        }

        let _gate = self.refresh_gate.lock().await;
        if let Some(index) = self.index.read().await.as_ref()
            && index.loaded_at.elapsed() < INDEX_TTL
        {
            return Ok(index.clone());
        }

        let sessions_root = self.sessions_root();
        let index = tokio::task::spawn_blocking(move || scan_session_index(&sessions_root))
            .await
            .context("dashboard index task failed")??;
        let index = Arc::new(index);
        *self.index.write().await = Some(index.clone());
        Ok(index)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        if let Err(error) = self.refresh_active_sessions().await {
            debug!(%error, "unable to refresh active sessions");
        }
        let active: HashSet<String> = self.active_sessions.read().await.keys().cloned().collect();
        let index = self.ensure_index().await?;
        let mut sessions = index.sessions.clone();
        for session in &mut sessions {
            session.is_active = active.contains(&session.id);
        }
        Ok(sessions)
    }

    pub async fn query_sessions(&self, query: SessionQuery) -> Result<SessionPage> {
        let mut sessions = self.list_sessions().await?;
        let needle = query.query.as_deref().map(str::to_ascii_lowercase);
        let cwd = query.cwd.as_deref().map(str::to_ascii_lowercase);
        let model = query.model.as_deref().map(str::to_ascii_lowercase);
        sessions.retain(|session| {
            query
                .active
                .is_none_or(|active| session.is_active == active)
                && cwd
                    .as_ref()
                    .is_none_or(|cwd| session.cwd.to_ascii_lowercase().contains(cwd))
                && model
                    .as_ref()
                    .is_none_or(|model| session.model_id.to_ascii_lowercase().contains(model))
                && needle.as_ref().is_none_or(|needle| {
                    session.id.to_ascii_lowercase().contains(needle)
                        || session.cwd.to_ascii_lowercase().contains(needle)
                        || session.model_id.to_ascii_lowercase().contains(needle)
                        || session
                            .title
                            .as_deref()
                            .unwrap_or_default()
                            .to_ascii_lowercase()
                            .contains(needle)
                })
        });
        let total = sessions.len();
        let limit = query.limit.unwrap_or(100).clamp(1, 500);
        let offset = query.offset.min(total);
        let items = sessions.into_iter().skip(offset).take(limit).collect();
        Ok(SessionPage {
            items,
            total,
            offset,
            limit,
        })
    }

    pub async fn get_session(&self, session_id: &str) -> Result<Option<SessionDetail>> {
        validate_session_id(session_id)?;
        if let Err(error) = self.refresh_active_sessions().await {
            debug!(%error, "unable to refresh active sessions");
        }
        let index = self.ensure_index().await?;
        let Some(session_dir) = index.paths.get(session_id).cloned() else {
            return Ok(None);
        };
        let summary_path = session_dir.join(SUMMARY_FILE);
        if !safe_regular_file_below(&self.grok_home, &summary_path)? {
            bail!("session summary is not a safe regular file");
        }
        let summary: Summary = read_json_async(&summary_path).await?;
        let signals_path = session_dir.join(SIGNALS_FILE);
        let signals: Option<SessionSignals> =
            if safe_regular_file_below(&self.grok_home, &signals_path)? {
                Some(read_json_async(&signals_path).await?)
            } else {
                None
            };
        let active = self.active_sessions.read().await.contains_key(session_id);
        let meta = session_meta(&summary, signals.as_ref(), active);
        Ok(Some(SessionDetail {
            meta,
            summary,
            signals,
        }))
    }

    pub async fn get_timeline(&self, session_id: &str, limit: usize) -> Result<Vec<TimelineEvent>> {
        let Some(path) = self.session_file(session_id, EVENTS_FILE).await? else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 10_000);
        let lines = tokio::task::spawn_blocking(move || {
            read_tail_lines(&path, limit, MAX_TIMELINE_TAIL_BYTES)
        })
        .await
        .context("timeline reader task failed")??;
        Ok(lines
            .into_iter()
            .filter_map(|line| match serde_json::from_str(&line) {
                Ok(event) => Some(event),
                Err(error) => {
                    debug!(%error, "skipping unparseable events.jsonl line");
                    None
                }
            })
            .collect())
    }

    pub async fn get_chat_history(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<ChatMessage>> {
        let Some(path) = self.session_file(session_id, CHAT_HISTORY_FILE).await? else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 2_000);
        let lines =
            tokio::task::spawn_blocking(move || read_tail_lines(&path, limit, MAX_CHAT_TAIL_BYTES))
                .await
                .context("chat reader task failed")??;
        Ok(lines
            .into_iter()
            .filter_map(
                |line| match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(value) => chat_message_from_value(value),
                    Err(error) => {
                        debug!(%error, "skipping unparseable chat_history.jsonl line");
                        None
                    }
                },
            )
            .collect())
    }

    pub async fn get_logs(
        &self,
        limit: usize,
        level: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<LogEntry>> {
        if let Some(session_id) = session_id {
            validate_session_id(session_id)?;
        }
        let path = self.grok_home.join("logs").join(UNIFIED_LOG_FILE);
        if !safe_regular_file_below(&self.grok_home, &path)? {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 5_000);
        let read_limit = (limit.saturating_mul(8)).clamp(limit, 20_000);
        let lines = tokio::task::spawn_blocking(move || {
            read_tail_lines(&path, read_limit, MAX_LOG_TAIL_BYTES)
        })
        .await
        .context("log reader task failed")??;
        let level = level.map(str::to_ascii_lowercase);
        let mut entries: Vec<_> = lines
            .into_iter()
            .rev()
            .filter_map(|line| serde_json::from_str::<UnifiedLogRaw>(&line).ok())
            .filter(|entry| {
                level
                    .as_ref()
                    .is_none_or(|level| entry.lvl.to_ascii_lowercase() == *level)
                    && session_id.is_none_or(|session_id| entry.sid.as_deref() == Some(session_id))
            })
            .take(limit)
            .map(LogEntry::from)
            .collect();
        entries.reverse();
        Ok(entries)
    }

    pub async fn session_charts(&self, session_id: &str) -> Result<Option<SessionCharts>> {
        let Some(detail) = self.get_session(session_id).await? else {
            return Ok(None);
        };
        let timeline = self.get_timeline(session_id, 10_000).await?;
        let mut event_counts = BTreeMap::<String, u64>::new();
        let mut tool_durations = BTreeMap::<String, u64>::new();
        for event in timeline {
            *event_counts
                .entry(format_event_kind(event.kind))
                .or_default() += 1;
            if let (Some(tool), Some(duration)) = (event.tool_name, event.duration_ms) {
                *tool_durations.entry(tool).or_default() += duration;
            }
        }
        let signals = detail.signals.unwrap_or_default();
        Ok(Some(SessionCharts {
            event_counts: map_metrics(event_counts),
            tool_duration_ms: map_metrics(tool_durations),
            context_tokens_used: signals.context_tokens_used,
            context_window_tokens: signals.context_window_tokens,
            context_window_usage: signals.context_window_usage,
            error_count: signals.error_count,
            compaction_count: signals.compaction_count,
        }))
    }

    pub async fn server_metrics(&self) -> Result<ServerMetrics> {
        let sessions = self.list_sessions().await?;
        let active_sessions = sessions.iter().filter(|session| session.is_active).count();
        let mut model_counts = BTreeMap::<String, u64>::new();
        let mut response_total = 0u128;
        let mut response_samples = 0u64;
        for session in &sessions {
            *model_counts.entry(session.model_id.clone()).or_default() += 1;
            if let Some(ms) = session.avg_response_time_ms {
                response_total += u128::from(ms);
                response_samples += 1;
            }
        }
        let avg_response_time_ms =
            (response_samples > 0).then(|| (response_total / u128::from(response_samples)) as u64);
        Ok(ServerMetrics {
            total_sessions: sessions.len(),
            active_sessions,
            total_tokens_used: sessions.iter().map(|session| session.tokens_used).sum(),
            total_tool_calls: sessions
                .iter()
                .map(|session| u64::from(session.tool_call_count))
                .sum(),
            total_turns: sessions
                .iter()
                .map(|session| u64::from(session.turn_count))
                .sum(),
            total_errors: sessions
                .iter()
                .map(|session| u64::from(session.error_count))
                .sum(),
            total_compactions: sessions
                .iter()
                .map(|session| u64::from(session.compaction_count))
                .sum(),
            avg_response_time_ms,
            models: map_metrics(model_counts),
        })
    }

    pub async fn resource_metrics(&self) -> Result<ResourceMetrics> {
        let entries = self.active_session_entries().await;
        let grok_home = self.grok_home.clone();
        tokio::task::spawn_blocking(move || {
            let sessions_dir = grok_home.join("sessions");
            let logs_dir = grok_home.join("logs");
            let memtrace_dir = grok_home.join("memtrace");
            let processes = entries
                .into_iter()
                .map(|entry| process_resource(&memtrace_dir, entry))
                .collect();
            Ok(ResourceMetrics {
                grok_home: grok_home.display().to_string(),
                sessions_bytes: directory_size(&sessions_dir),
                logs_bytes: directory_size(&logs_dir),
                memtrace_bytes: directory_size(&memtrace_dir),
                processes,
            })
        })
        .await
        .context("resource metrics task failed")?
    }

    pub async fn session_file(&self, session_id: &str, file: &str) -> Result<Option<PathBuf>> {
        validate_session_id(session_id)?;
        if Path::new(file).components().count() != 1 {
            bail!("invalid session file name");
        }
        let index = self.ensure_index().await?;
        let Some(session_dir) = index.paths.get(session_id) else {
            return Ok(None);
        };
        let path = session_dir.join(file);
        if safe_regular_file_below(&self.grok_home, &path)? {
            Ok(Some(path))
        } else {
            Ok(None)
        }
    }
}

fn scan_session_index(sessions_root: &Path) -> Result<CachedIndex> {
    if !safe_directory(sessions_root)? {
        return Ok(CachedIndex {
            loaded_at: Instant::now(),
            sessions: Vec::new(),
            paths: HashMap::new(),
        });
    }
    let mut sessions = Vec::new();
    let mut paths = HashMap::new();
    for cwd_entry in std::fs::read_dir(sessions_root)? {
        let Ok(cwd_entry) = cwd_entry else { continue };
        let cwd_dir = cwd_entry.path();
        if !safe_directory(&cwd_dir).unwrap_or(false) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&cwd_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let session_dir = entry.path();
            if !safe_directory(&session_dir).unwrap_or(false) {
                continue;
            }
            let summary_path = session_dir.join(SUMMARY_FILE);
            if !safe_regular_file_below(sessions_root, &summary_path).unwrap_or(false) {
                continue;
            }
            let summary: Summary = match read_json_bounded(&summary_path) {
                Ok(summary) => summary,
                Err(error) => {
                    debug!(path = %summary_path.display(), %error, "skipping unreadable session summary");
                    continue;
                }
            };
            if summary.hidden.unwrap_or(false) || !valid_index_id(&summary.info.id) {
                continue;
            }
            let signals_path = session_dir.join(SIGNALS_FILE);
            let signals: Option<SessionSignals> =
                if safe_regular_file_below(sessions_root, &signals_path).unwrap_or(false) {
                    read_json_bounded(&signals_path).ok()
                } else {
                    None
                };
            let meta = session_meta(&summary, signals.as_ref(), false);
            paths.insert(meta.id.clone(), session_dir);
            sessions.push(meta);
        }
    }
    sessions.sort_by(|left, right| {
        right
            .last_active_at
            .cmp(&left.last_active_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(CachedIndex {
        loaded_at: Instant::now(),
        sessions,
        paths,
    })
}

fn session_meta(
    summary: &Summary,
    signals: Option<&SessionSignals>,
    is_active: bool,
) -> SessionMeta {
    let signals = signals.cloned().unwrap_or_default();
    let title = summary
        .generated_title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| {
            (!summary.session_summary.trim().is_empty()).then_some(summary.session_summary.as_str())
        })
        .map(str::to_owned);
    SessionMeta {
        id: summary.info.id.clone(),
        cwd: summary.info.cwd.clone(),
        model_id: if summary.current_model_id.is_empty() {
            signals
                .primary_model_id
                .clone()
                .unwrap_or_else(|| "unknown".to_owned())
        } else {
            summary.current_model_id.clone()
        },
        agent_name: summary.agent_name.clone(),
        session_kind: summary.session_kind.clone(),
        title,
        created_at: summary.created_at,
        last_active_at: summary.last_active_at.unwrap_or(summary.updated_at),
        turn_count: signals.turn_count,
        tokens_used: signals.context_tokens_used,
        context_window_tokens: signals.context_window_tokens,
        context_window_usage: signals.context_window_usage,
        tool_call_count: signals.tool_call_count,
        error_count: signals.error_count,
        compaction_count: signals.compaction_count,
        avg_response_time_ms: signals.avg_response_time_ms,
        session_duration_seconds: signals.session_duration_seconds,
        peak_rss_bytes: signals.peak_rss_bytes,
        is_active,
        git_branch: summary.head_branch.clone(),
    }
}

fn chat_message_from_value(value: serde_json::Value) -> Option<ChatMessage> {
    let object = value.as_object()?;
    let role = object
        .get("role")
        .or_else(|| object.get("type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let content = object
        .get("content")
        .map(extract_text)
        .filter(|text| !text.is_empty())
        .or_else(|| object.get("summary").map(extract_text))
        .unwrap_or_default();
    let timestamp = object
        .get("timestamp")
        .or_else(|| object.get("ts"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let tool_calls: Vec<ToolCallInfo> = object
        .get("tool_calls")
        .or_else(|| object.get("toolCalls"))
        .and_then(serde_json::Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    let call = call.as_object()?;
                    Some(ToolCallInfo {
                        name: call
                            .get("name")
                            .or_else(|| call.get("title"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("tool")
                            .to_owned(),
                        call_id: call
                            .get("id")
                            .or_else(|| call.get("call_id"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        status: call
                            .get("status")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if content.is_empty() && tool_calls.is_empty() {
        return None;
    }
    Some(ChatMessage {
        role,
        content: truncate_chars(content, MAX_MESSAGE_CHARS),
        timestamp,
        tool_calls,
    })
}

fn extract_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(extract_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("summary_text"))
            .or_else(|| object.get("content"))
            .map(extract_text)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn process_resource(memtrace_dir: &Path, entry: ActiveSessionEntry) -> ProcessResource {
    let sample = latest_memtrace_sample(memtrace_dir, entry.pid);
    ProcessResource {
        session_id: entry.session_id,
        pid: entry.pid,
        cwd: entry.cwd,
        opened_at: entry.opened_at,
        rss_bytes: sample.as_ref().and_then(|sample| sample.rss_bytes),
        footprint_bytes: sample.as_ref().and_then(|sample| sample.footprint_bytes),
        allocated_bytes: sample
            .as_ref()
            .and_then(|sample| sample.alloc.as_ref())
            .and_then(|alloc| alloc.allocated),
        sample_timestamp_ms: sample.map(|sample| sample.ts_ms),
    }
}

fn latest_memtrace_sample(memtrace_dir: &Path, pid: u32) -> Option<MemtraceSample> {
    if !safe_directory(memtrace_dir).ok()? {
        return None;
    }
    let suffix = format!("-{pid}.jsonl");
    let mut candidates: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(memtrace_dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.ends_with(&suffix) || !safe_regular_file(&path).ok()? {
                return None;
            }
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    let path = &candidates.first()?.1;
    read_tail_lines(path, 32, 256 * 1024)
        .ok()?
        .into_iter()
        .rev()
        .filter_map(|line| serde_json::from_str::<MemtraceSample>(&line).ok())
        .find(|sample| sample.kind == "sample")
}

fn directory_size(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.file_type().is_symlink() {
        return 0;
    }
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| directory_size(&entry.path()))
        .sum()
}

fn map_metrics(map: BTreeMap<String, u64>) -> Vec<NamedMetric> {
    let mut metrics: Vec<_> = map
        .into_iter()
        .map(|(name, value)| NamedMetric { name, value })
        .collect();
    metrics.sort_by(|left, right| {
        right
            .value
            .cmp(&left.value)
            .then_with(|| left.name.cmp(&right.name))
    });
    metrics
}

fn format_event_kind(kind: EventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if valid_index_id(session_id) {
        Ok(())
    } else {
        bail!("invalid session id")
    }
}

fn valid_index_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 255
        && Path::new(session_id).components().count() == 1
        && matches!(
            Path::new(session_id).components().next(),
            Some(Component::Normal(_))
        )
}

fn safe_directory(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn safe_regular_file(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Verify every component below the configured root without following a
/// descendant symlink. The root itself is trusted and may intentionally be a
/// symlink (for example, when GROK_HOME lives on another local volume).
fn safe_regular_file_below(root: &Path, path: &Path) -> Result<bool> {
    let Ok(relative) = path.strip_prefix(root) else {
        return Ok(false);
    };
    let mut current = root.to_owned();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Ok(false);
        };
        current.push(name);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            return Ok(false);
        }
        if components.peek().is_some() {
            if !metadata.is_dir() {
                return Ok(false);
            }
        } else {
            return Ok(metadata.is_file());
        }
    }
    Ok(false)
}

fn read_json_bounded<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = read_bounded(path, MAX_JSON_BYTES)?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

async fn read_json_async<T: serde::de::DeserializeOwned + Send + 'static>(
    path: &Path,
) -> Result<T> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_json_bounded(&path))
        .await
        .context("JSON reader task failed")?
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular file {}", path.display());
    }
    if metadata.len() > max_bytes {
        bail!("file exceeds dashboard read limit: {}", path.display());
    }
    std::fs::read(path).with_context(|| format!("read {}", path.display()))
}

async fn read_bounded_async(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_bounded(&path, max_bytes))
        .await
        .context("bounded file reader task failed")?
}

fn read_tail_lines(path: &Path, limit: usize, max_bytes: u64) -> Result<Vec<String>> {
    if !safe_regular_file(path)? {
        return Ok(Vec::new());
    }
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((len - start).min(max_bytes) as usize);
    file.read_to_end(&mut bytes)?;
    if start > 0
        && let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n')
    {
        bytes.drain(..=first_newline);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(limit)
        .map(str::to_owned)
        .collect();
    lines.reverse();
    Ok(lines)
}

fn truncate_chars(mut text: String, max_chars: usize) -> String {
    let Some((index, _)) = text.char_indices().nth(max_chars) else {
        return text;
    };
    text.truncate(index);
    text.push_str("\n… [truncated]");
    text
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|source| source.downcast_ref::<std::io::Error>())
        .any(|error| error.kind() == std::io::ErrorKind::NotFound)
}

#[derive(Debug, Deserialize)]
struct UnifiedLogRaw {
    #[serde(default)]
    ts: String,
    #[serde(default)]
    src: String,
    pid: Option<u32>,
    #[serde(default)]
    lvl: String,
    sid: Option<String>,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    ctx: serde_json::Value,
}

impl From<UnifiedLogRaw> for LogEntry {
    fn from(value: UnifiedLogRaw) -> Self {
        Self {
            timestamp: value.ts,
            source: value.src,
            pid: value.pid,
            level: value.lvl,
            session_id: value.sid,
            message: value.msg,
            context: value.ctx,
        }
    }
}

#[derive(Debug, Deserialize)]
struct MemtraceSample {
    #[serde(default)]
    ts_ms: u64,
    #[serde(default)]
    kind: String,
    rss_bytes: Option<u64>,
    footprint_bytes: Option<u64>,
    alloc: Option<MemtraceAlloc>,
}

#[derive(Debug, Deserialize)]
struct MemtraceAlloc {
    allocated: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fixture(root: &Path, id: &str) -> PathBuf {
        let session_dir = root.join("sessions").join("%2Ftmp%2Fproject").join(id);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join(SUMMARY_FILE),
            serde_json::json!({
                "info": { "id": id, "cwd": "/tmp/project" },
                "session_summary": "Dashboard fixture",
                "created_at": "2026-07-23T00:00:00Z",
                "updated_at": "2026-07-23T00:10:00Z",
                "last_active_at": "2026-07-23T00:09:00Z",
                "current_model_id": "test/model",
                "num_messages": 3,
                "num_chat_messages": 2,
                "head_branch": "dev"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            session_dir.join(SIGNALS_FILE),
            serde_json::json!({
                "turnCount": 2,
                "contextTokensUsed": 1234,
                "contextWindowTokens": 10000,
                "contextWindowUsage": 12,
                "toolCallCount": 4,
                "errorCount": 1,
                "compactionCount": 1,
                "avgResponseTimeMs": 42
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            session_dir.join(EVENTS_FILE),
            concat!(
                "{\"ts\":\"2026-07-23T00:00:01Z\",\"type\":\"turn_started\",\"session_id\":\"session-a\",\"turn_number\":0}\n",
                "{\"ts\":\"2026-07-23T00:00:02Z\",\"type\":\"tool_completed\",\"tool_name\":\"read_file\",\"duration_ms\":25,\"outcome\":\"success\"}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            session_dir.join(CHAT_HISTORY_FILE),
            concat!(
                "{\"type\":\"user\",\"content\":\"hello\"}\n",
                "{\"type\":\"assistant\",\"content\":\"hi\",\"tool_calls\":[{\"name\":\"read_file\",\"id\":\"1\"}]}\n"
            ),
        )
        .unwrap();
        session_dir
    }

    #[tokio::test]
    async fn lists_and_loads_fixture_session() {
        let temp = TempDir::new().unwrap();
        write_fixture(temp.path(), "session-a");
        std::fs::write(
            temp.path().join("active_sessions.json"),
            r#"[{"session_id":"session-a","pid":42,"cwd":"/tmp/project","opened_at":"2026-07-23T00:00:00Z"}]"#,
        )
        .unwrap();
        let store = DashboardStore::new(temp.path().to_owned());
        let sessions = store.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].is_active);
        assert_eq!(sessions[0].tokens_used, 1234);
        let detail = store.get_session("session-a").await.unwrap().unwrap();
        assert_eq!(detail.meta.git_branch.as_deref(), Some("dev"));
        assert_eq!(detail.signals.unwrap().avg_response_time_ms, Some(42));
    }

    #[tokio::test]
    async fn parses_timeline_and_chat_tails() {
        let temp = TempDir::new().unwrap();
        write_fixture(temp.path(), "session-a");
        let store = DashboardStore::new(temp.path().to_owned());
        let timeline = store.get_timeline("session-a", 10).await.unwrap();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[1].kind, EventKind::ToolCompleted);
        let chat = store.get_chat_history("session-a", 10).await.unwrap();
        assert_eq!(chat.len(), 2);
        assert_eq!(chat[1].tool_calls[0].name, "read_file");
    }

    #[tokio::test]
    async fn rejects_traversal_and_symlinked_session_files() {
        let temp = TempDir::new().unwrap();
        let session_dir = write_fixture(temp.path(), "session-a");
        let store = DashboardStore::new(temp.path().to_owned());
        assert!(store.get_session("../escape").await.is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            std::fs::remove_file(session_dir.join(EVENTS_FILE)).unwrap();
            symlink("/etc/passwd", session_dir.join(EVENTS_FILE)).unwrap();
            let events = store.get_timeline("session-a", 10).await.unwrap();
            assert!(events.is_empty());
        }
    }

    #[test]
    fn tail_reader_returns_last_complete_lines() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tail.jsonl");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert_eq!(read_tail_lines(&path, 2, 1024).unwrap(), ["two", "three"]);
    }

    #[cfg(unix)]
    #[test]
    fn safe_file_check_rejects_a_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join(UNIFIED_LOG_FILE), "secret\n").unwrap();
        symlink(outside.path(), root.path().join("logs")).unwrap();

        let candidate = root.path().join("logs").join(UNIFIED_LOG_FILE);
        assert!(!safe_regular_file_below(root.path(), &candidate).unwrap());
    }
}
