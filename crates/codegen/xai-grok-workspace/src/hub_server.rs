//! Workspace-side RPC handler for server-proxied workspace method calls.
//!
//! [`WorkspaceRpcHandler`] implements [`ToolServerHandler`] and dispatches
//! `workspace.*` JSON-RPC methods to [`WorkspaceHandle`]. Registered on
//! the `ToolServer` with tool_id `workspace_rpc`.
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::handle::WorkspaceHandle;
use crate::hub_ids::WORKSPACE_RPC_TOOL_ID;
use crate::rpc_envelope::{RpcEnvelope, envelope_err};
use crate::workspace_ops::WorkspaceOp;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use prometheus::{HistogramVec, IntCounterVec, register_histogram_vec, register_int_counter_vec};
use serde_json::Value;
use xai_computer_hub_sdk::ToolServerHandler;
use xai_grok_tools::computer::types::TaskKind;
use xai_grok_tools::implementations::grok_build::scheduler::interval::interval_to_human;
use xai_grok_tools::implementations::grok_build::scheduler::types::{
    SchedulerCommand, SchedulerHandle,
};
use xai_grok_tools::registry::types::FinalizedToolset;
use xai_grok_tools::types::resources::Terminal;
use xai_grok_workspace_types::rpc::workspace::{
    BackgroundTaskSnapshotWire, ScheduledTaskSnapshotWire, TasksSnapshotResponse,
};
use xai_tool_protocol::{HookEvent, HookFrame, SessionId, ToolId, ToolServerEvictParams};
use xai_tool_runtime::{
    ToolCallContext, ToolError, ToolErrorKind, ToolStream, TypedToolOutput, terminal_only,
};
use xai_tool_types::ToolDescription;
/// Deprecation monitor for the self-attested `caller_session_id` param:
/// `kind="param_mismatch"` — the param disagreed with the server-bound envelope
/// session (envelope trusted); `kind="envelope_absent"` — no envelope
/// session, the param was used as a compat fallback. Enforcement
/// (envelope-only identity) waits for this to be flat zero.
static WORKSPACE_RPC_CALLER_MISMATCH_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_rpc_caller_mismatch_total",
            "Mutation RPCs whose caller_session_id param was not backed by a matching \
             server-bound envelope session, by method and kind",
            &["method", "kind"]
        )
        .unwrap()
    });
/// Audit trail for the deliberate-mutation RPC surface
/// (`update_tool_config` / `drop_session` / `configure_mcp`).
static WORKSPACE_RPC_MUTATION_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_rpc_mutation_total",
            "Session-mutating workspace RPC calls, by method and outcome",
            &["method", "outcome"]
        )
        .unwrap()
    });
/// Every dispatched `workspace.*` RPC, by method and result. Unrecognized
/// methods collapse to the `unknown` label to keep cardinality bounded.
static WORKSPACE_RPC_REQUESTS_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_rpc_requests_total",
            "Workspace RPC dispatches, by method and result",
            &["method", "result"]
        )
        .unwrap()
    });
/// Per-method wall-clock duration of a `workspace.*` RPC dispatch.
static WORKSPACE_RPC_DURATION_SECONDS: std::sync::LazyLock<HistogramVec> =
    std::sync::LazyLock::new(|| {
        register_histogram_vec!(
            "grok_workspace_rpc_duration_seconds",
            "Workspace RPC dispatch duration",
            &["method"],
            vec![
                0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0
            ]
        )
        .unwrap()
    });
const UNKNOWN_METHOD_LABEL: &str = "unknown";
/// Prefix of the [`WorkspaceError::HubError`] for an unrecognized method. Shared
/// by the dispatch default arm and the metric classifier so the "collapse to
/// `unknown`" decision cannot drift from the error it keys on.
const UNKNOWN_METHOD_ERR_PREFIX: &str = "unknown workspace method:";
/// Zero-init this module's metric families. See [`crate::init_metrics`].
pub(crate) fn init_metrics() {
    WORKSPACE_RPC_REQUESTS_TOTAL
        .with_label_values(&[UNKNOWN_METHOD_LABEL, "error"])
        .inc_by(0);
    let _ = WORKSPACE_RPC_DURATION_SECONDS.with_label_values(&[UNKNOWN_METHOD_LABEL]);
}
/// Resolve the caller identity for a mutation RPC: the server-bound envelope
/// session is authoritative; the deprecated `caller_session_id` param is
/// only used when no envelope session exists (old call paths). Both
/// divergences are counted on [`WORKSPACE_RPC_CALLER_MISMATCH_TOTAL`].
fn resolve_mutation_caller<'a>(
    method: &'static str,
    bound_session: Option<&'a str>,
    param_caller: Option<&'a str>,
) -> WorkspaceResult<&'a str> {
    match (bound_session, param_caller) {
        (Some(envelope), Some(param)) => {
            if envelope != param {
                WORKSPACE_RPC_CALLER_MISMATCH_TOTAL
                    .with_label_values(&[method, "param_mismatch"])
                    .inc();
                tracing::warn!(
                    method, envelope_session = % envelope, param_caller = % param,
                    "caller_session_id param disagrees with the server-bound envelope session; \
                     trusting the envelope"
                );
            }
            Ok(envelope)
        }
        (Some(envelope), None) => Ok(envelope),
        (None, Some(param)) => {
            WORKSPACE_RPC_CALLER_MISMATCH_TOTAL
                .with_label_values(&[method, "envelope_absent"])
                .inc();
            Ok(param)
        }
        (None, None) => Err(WorkspaceError::HubError(format!(
            "{method}: missing caller identity (no bound session and no caller_session_id)"
        ))),
    }
}
/// Audit-log and count a mutation RPC on [`WORKSPACE_RPC_MUTATION_TOTAL`].
/// Failures log at WARN because that arm carries rejected cross-session
/// forgeries (`Unauthorized`), the audit trail's most interesting event.
fn record_mutation_rpc<T>(
    method: &'static str,
    caller: &str,
    target: &str,
    result: &WorkspaceResult<T>,
) {
    let outcome = match result {
        Ok(_) => "ok",
        Err(_) => "error",
    };
    WORKSPACE_RPC_MUTATION_TOTAL
        .with_label_values(&[method, outcome])
        .inc();
    match result {
        Ok(_) => tracing::info!(method, caller, target, "workspace mutation rpc"),
        Err(e) => {
            tracing::warn!(
                method, caller, target, error = % e, "workspace mutation rpc failed"
            );
        }
    }
}
/// No-op notifier for RPC-driven worktree creation.
struct NoOpNotifier;
#[async_trait]
impl crate::worktree::WorktreeNotificationSender for NoOpNotifier {
    async fn send_worktree_status(&self, _progress: crate::worktree::WorktreeStatus) {}
}
/// Env escape hatch for the client-facing `workspace.client_fs_*` ops.
///
/// Default **on**; setting `WORKSPACE_CLIENT_FS_QUERIES=0` (or `false`)
/// disables the ops with a graceful `HubError` that the remote caller
/// maps to a fallback. Read per call — flipping the variable needs no
/// process restart and tests can toggle it under a lock.
fn client_fs_queries_enabled() -> bool {
    !matches!(
        std::env::var("WORKSPACE_CLIENT_FS_QUERIES").as_deref(),
        Ok("0") | Ok("false")
    )
}
/// Reject `workspace.client_fs_*` dispatch when the escape hatch is off.
fn ensure_client_fs_queries_enabled() -> WorkspaceResult<()> {
    if client_fs_queries_enabled() {
        Ok(())
    } else {
        Err(WorkspaceError::HubError(
            "client fs queries disabled on this workspace".into(),
        ))
    }
}
/// Generic dispatch helper: deserialize params, execute, serialize result.
async fn dispatch_op<Op: WorkspaceOp>(
    params: Value,
    ws: &WorkspaceHandle,
    session_id: Option<&str>,
) -> WorkspaceResult<Value> {
    let req: Op = serde_json::from_value(params)
        .map_err(|e| WorkspaceError::HubError(format!("invalid params for {}: {e}", Op::METHOD)))?;
    let result = req.execute(ws, session_id).await?;
    serde_json::to_value(result)
        .map_err(|e| WorkspaceError::HubError(format!("{}: {e}", Op::METHOD)))
}
/// List a session's outstanding (not-completed) background terminal tasks from
/// the session toolset's `TerminalBackend` resource, mapped to the slim wire
/// DTO. Empty when the session has no terminal backend. Source of truth for the
/// `workspace.list_background_tasks` RPC (post-compaction system-reminder state).
async fn list_outstanding_background_tasks(
    toolset: &xai_grok_tools::registry::types::FinalizedToolset,
) -> Vec<xai_grok_workspace_types::rpc::workspace::BackgroundTaskSummaryWire> {
    use xai_grok_tools::computer::types::TaskKind;
    use xai_grok_tools::types::resources::Terminal;
    use xai_grok_tools::types::tool::ToolKind;
    use xai_grok_workspace_types::rpc::workspace::BackgroundTaskSummaryWire;
    let terminal = {
        let res = toolset.resources.lock().await;
        res.get::<Terminal>().map(|t| t.0.clone())
    };
    let Some(terminal) = terminal else {
        return Vec::new();
    };
    let execute_name = toolset.tool_name_for_kind(ToolKind::Execute);
    let monitor_name = toolset.tool_name_for_kind(ToolKind::Monitor);
    terminal
        .list_tasks()
        .await
        .into_iter()
        .filter(|t| !t.completed)
        .map(|t| {
            let command = t
                .display_command
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or(t.command);
            let tool_name = match t.kind {
                TaskKind::Monitor => monitor_name.clone(),
                TaskKind::Bash => execute_name.clone(),
            };
            BackgroundTaskSummaryWire {
                task_id: t.task_id,
                command,
                tool_name,
            }
        })
        .collect()
}
/// Point-in-time snapshot of the session's outstanding background terminal
/// tasks and live scheduled tasks.
async fn tasks_snapshot(toolset: &FinalizedToolset) -> TasksSnapshotResponse {
    let (terminal, scheduler) = {
        let res = toolset.resources.lock().await;
        (
            res.get::<Terminal>().map(|t| t.0.clone()),
            res.get::<SchedulerHandle>().cloned(),
        )
    };
    let background_tasks = match terminal {
        Some(terminal) => terminal
            .list_tasks()
            .await
            .into_iter()
            .filter(|t| !t.completed)
            .map(|t| {
                let command = t
                    .display_command
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(t.command);
                BackgroundTaskSnapshotWire {
                    task_id: t.task_id,
                    command,
                    kind: match t.kind {
                        TaskKind::Bash => "bash".to_owned(),
                        TaskKind::Monitor => "monitor".to_owned(),
                    },
                    started_at: DateTime::<Utc>::from(t.start_time).to_rfc3339(),
                }
            })
            .collect(),
        None => Vec::new(),
    };
    let scheduled_tasks = match scheduler {
        Some(handle) => {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = handle.0.send(SchedulerCommand::List { reply: reply_tx });
            reply_rx
                .await
                .map(|snapshot| snapshot.tasks)
                .unwrap_or_default()
                .into_iter()
                .map(|t| ScheduledTaskSnapshotWire {
                    task_id: t.id.clone(),
                    prompt: t.prompt.clone(),
                    human_schedule: interval_to_human(t.interval_secs),
                    next_fire_at: t.next_fire_at().to_rfc3339(),
                    recurring: t.recurring,
                    created_at: t.created_at.to_rfc3339(),
                })
                .collect()
        }
        None => Vec::new(),
    };
    TasksSnapshotResponse {
        background_tasks,
        scheduled_tasks,
    }
}
/// List the session's TODO items (via `todo_write`) from the session toolset's
/// `State<TodoState>` resource, mapped to the slim wire DTO. Empty when the
/// session has no todo state. Source of truth for the `workspace.list_todos`
/// RPC (post-compaction system-reminder state).
async fn list_session_todos(
    toolset: &xai_grok_tools::registry::types::FinalizedToolset,
) -> Vec<xai_grok_workspace_types::rpc::workspace::TodoSummaryWire> {
    use xai_grok_tools::implementations::grok_build::todo::{TodoState, TodoStatus};
    use xai_grok_tools::types::resources::State;
    use xai_grok_workspace_types::rpc::workspace::TodoSummaryWire;
    let res = toolset.resources.lock().await;
    let Some(state) = res.get::<State<TodoState>>() else {
        return Vec::new();
    };
    state
        .0
        .todo_items_with_ids()
        .map(|(id, item)| {
            let status = match item.status {
                TodoStatus::Pending => "pending",
                TodoStatus::InProgress => "in_progress",
                TodoStatus::Completed => "completed",
                TodoStatus::Cancelled => "cancelled",
            };
            TodoSummaryWire {
                id: id.to_string(),
                content: item.content.clone(),
                status: status.to_string(),
            }
        })
        .collect()
}
/// Routes JSON-RPC `workspace.*` method calls to [`WorkspaceHandle`].
pub(crate) struct WorkspaceRpcHandler {
    workspace: WorkspaceHandle,
}
impl WorkspaceRpcHandler {
    pub(crate) fn new(workspace: WorkspaceHandle) -> Self {
        Self { workspace }
    }
    /// Route a `workspace.*` method; `bound_session` is the caller's server-bound session.
    async fn dispatch(
        &self,
        method: &str,
        params: Value,
        bound_session: Option<&str>,
    ) -> WorkspaceResult<Value> {
        use crate::file_system::ContentSearchRequest;
        use crate::file_system::{
            FsDeleteFileReq, FsExistsReq, FsListReq, FsReadFileReq, FsWriteFileReq,
        };
        use crate::session::checkpoint::TurnBoundary;
        use crate::workspace_ops::*;
        use crate::worktree::{ApplyWorktreeRequest, CreateWorktreeRequest, RemoveWorktreeRequest};
        use xai_grok_workspace_types::rpc::git::{GitBranchInfoReq, GitMetadataReq};
        use xai_grok_workspace_types::rpc::search::FuzzyStatusReq;
        use xai_grok_workspace_types::rpc::skills::DiscoverPluginsReq;
        use xai_grok_workspace_types::rpc::workspace::{
            ConfigureMcpReq, DropSessionReq, InstallPluginReq, ListBackgroundTasksReq,
            ListBackgroundTasksResponse, ListTodosReq, ListTodosResponse, LoadEnvrcReq,
            LoadPermissionsReq, LoadProjectConfigReq, RefreshPluginsReq, ResolveFileReferencesReq,
            TasksSnapshotReq, ToolDefinitionsReq, UpdateToolConfigReq,
        };
        use xai_grok_workspace_types::rpc::worktree::WorktreeCreateSyncReq;
        tracing::debug!(method, "workspace rpc dispatch");
        let params = if params.is_null() {
            serde_json::json!({})
        } else {
            params
        };
        match method {
            <WorkspaceInfoReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                let cwd_str = cwd.to_string_lossy().to_string();
                let os = std::env::consts::OS;
                let shell = std::env::var("SHELL")
                    .ok()
                    .and_then(|s| {
                        std::path::Path::new(&s)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                    })
                    .unwrap_or_else(|| "sh".to_string());
                Ok(serde_json::json!({ "os" : os, "shell" : shell, "cwd" : cwd_str, }))
            }
            <GitStatusReq as WorkspaceRpc>::METHOD => {
                static DEPRECATION_WARNING: std::sync::Once = std::sync::Once::new();
                DEPRECATION_WARNING.call_once(|| {
                    tracing::warn!(
                        "workspace.git_status is deprecated and will be removed in a future \
                         release. Use workspace.git_status_ext with format: \"prompt\" instead."
                    );
                });
                let cwd = self.workspace.root_cwd()?;
                let result = crate::file_system::git_status(cwd)
                    .await
                    .map_err(|e| WorkspaceError::HubError(e.to_string()))?;
                Ok(Value::String(result))
            }
            <GitBranchInfoReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                match crate::session::git::git_info(&cwd).await {
                    Ok(info) => serde_json::to_value(info)
                        .map_err(|e| WorkspaceError::HubError(e.to_string())),
                    Err(_) => Ok(Value::Null),
                }
            }
            <ToolDefinitionsReq as WorkspaceRpc>::METHOD => {
                let session_id = params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkspaceError::HubError("missing session_id".into()))?;
                let session = self
                    .workspace
                    .session(session_id)
                    .ok_or_else(|| WorkspaceError::SessionNotFound(session_id.into()))?;
                let defs = session.toolset().tool_definitions();
                serde_json::to_value(defs).map_err(|e| WorkspaceError::HubError(e.to_string()))
            }
            <ListBackgroundTasksReq as WorkspaceRpc>::METHOD => {
                let session_id = params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkspaceError::HubError("missing session_id".into()))?;
                let session = self
                    .workspace
                    .session(session_id)
                    .ok_or_else(|| WorkspaceError::SessionNotFound(session_id.into()))?;
                let toolset = session.toolset();
                let tasks = list_outstanding_background_tasks(toolset.as_ref()).await;
                serde_json::to_value(ListBackgroundTasksResponse { tasks })
                    .map_err(|e| WorkspaceError::HubError(e.to_string()))
            }
            <TasksSnapshotReq as WorkspaceRpc>::METHOD => {
                let session_id = params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkspaceError::HubError("missing session_id".into()))?;
                let session = self
                    .workspace
                    .session(session_id)
                    .ok_or_else(|| WorkspaceError::SessionNotFound(session_id.into()))?;
                let toolset = session.toolset();
                let snapshot = tasks_snapshot(toolset.as_ref()).await;
                serde_json::to_value(snapshot).map_err(|e| WorkspaceError::HubError(e.to_string()))
            }
            <ListTodosReq as WorkspaceRpc>::METHOD => {
                let session_id = params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkspaceError::HubError("missing session_id".into()))?;
                let session = self
                    .workspace
                    .session(session_id)
                    .ok_or_else(|| WorkspaceError::SessionNotFound(session_id.into()))?;
                let toolset = session.toolset();
                let todos = list_session_todos(toolset.as_ref()).await;
                serde_json::to_value(ListTodosResponse { todos })
                    .map_err(|e| WorkspaceError::HubError(e.to_string()))
            }
            <UpdateToolConfigReq as WorkspaceRpc>::METHOD => {
                let caller = resolve_mutation_caller(
                    "update_tool_config",
                    bound_session,
                    params
                        .get("caller_session_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty()),
                )?;
                let session_id = params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkspaceError::HubError("missing session_id".into()))?
                    .to_owned();
                let new_config = serde_json::from_value(
                    params
                        .get("new_config")
                        .cloned()
                        .ok_or_else(|| WorkspaceError::HubError("missing new_config".into()))?,
                )
                .map_err(|e| WorkspaceError::HubError(format!("invalid new_config: {e}")))?;
                let result = self
                    .workspace
                    .update_tool_config(caller, &session_id, new_config)
                    .await;
                record_mutation_rpc("update_tool_config", caller, &session_id, &result);
                result.map(|()| Value::Null)
            }
            <DropSessionReq as WorkspaceRpc>::METHOD => {
                let caller = resolve_mutation_caller(
                    "drop_session",
                    bound_session,
                    params
                        .get("caller_session_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty()),
                )?;
                let target = params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkspaceError::HubError("missing session_id".into()))?;
                let result = self.workspace.drop_session(caller, target);
                record_mutation_rpc("drop_session", caller, target, &result);
                result.map(|()| Value::Null)
            }
            <ResolveFileReferencesReq as WorkspaceRpc>::METHOD => {
                let refs: Vec<String> = params
                    .get("refs")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                let cwd = self.workspace.root_cwd()?;
                let mut results = Vec::new();
                for ref_path in &refs {
                    let full_path = if std::path::Path::new(ref_path).is_absolute() {
                        std::path::PathBuf::from(ref_path)
                    } else {
                        cwd.join(ref_path)
                    };
                    let exists = full_path.exists();
                    let content = if exists {
                        tokio::fs::read_to_string(&full_path).await.ok()
                    } else {
                        None
                    };
                    results.push(serde_json::json!(
                        { "path" : full_path.to_string_lossy(), "ref" : ref_path,
                        "exists" : exists, "content" : content, }
                    ));
                }
                Ok(Value::Array(results))
            }
            <PutFilesReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<PutFilesReq>(params, &self.workspace, None).await
            }
            <GetFilesReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GetFilesReq>(params, &self.workspace, None).await
            }
            <FsListReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<FsListReq>(params, &self.workspace, None).await
            }
            <FsExistsReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<FsExistsReq>(params, &self.workspace, None).await
            }
            <FsReadFileReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<FsReadFileReq>(params, &self.workspace, None).await
            }
            <FsWriteFileReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<FsWriteFileReq>(params, &self.workspace, None).await
            }
            <FsDeleteFileReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<FsDeleteFileReq>(params, &self.workspace, None).await
            }
            <ClientFsListReq as WorkspaceRpc>::METHOD => {
                ensure_client_fs_queries_enabled()?;
                dispatch_op::<ClientFsListReq>(params, &self.workspace, None).await
            }
            <ClientFsStatReq as WorkspaceRpc>::METHOD => {
                ensure_client_fs_queries_enabled()?;
                dispatch_op::<ClientFsStatReq>(params, &self.workspace, None).await
            }
            <ClientFsReadFileReq as WorkspaceRpc>::METHOD => {
                ensure_client_fs_queries_enabled()?;
                dispatch_op::<ClientFsReadFileReq>(params, &self.workspace, None).await
            }
            <DiscoverSkillsReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                let skills =
                    crate::discovery::discover_skills(&cwd, self.workspace.shared.skills_config())
                        .await;
                Ok(Value::Array(skills))
            }
            <DiscoverAgentsMdReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                let files = crate::discovery::discover_agents_md(&cwd).await;
                Ok(Value::Array(files))
            }
            <DiscoverPluginsReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                let plugins = crate::discovery::discover_plugins(
                    &cwd,
                    self.workspace.shared.plugin_discovery_config(),
                    &crate::discovery::PluginTrustStore::load(),
                    true,
                );
                Ok(Value::Array(plugins))
            }
            <HookRegistryReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HookRegistryReq>(params, &self.workspace, None).await
            }
            <LoadProjectConfigReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                Ok(crate::discovery::load_project_config(&cwd))
            }
            <LoadPermissionsReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                Ok(crate::discovery::load_permissions(&cwd).await)
            }
            <LoadEnvrcReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                let env = crate::envrc::load_envrc_or_empty(&cwd);
                serde_json::to_value(env).map_err(|e| WorkspaceError::HubError(e.to_string()))
            }
            <InstallPluginReq as WorkspaceRpc>::METHOD => {
                let _ = params;
                Ok(Value::Null)
            }
            <RefreshPluginsReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                let plugins = crate::discovery::discover_plugins(
                    &cwd,
                    self.workspace.shared.plugin_discovery_config(),
                    &crate::discovery::PluginTrustStore::load(),
                    true,
                );
                Ok(Value::Array(plugins))
            }
            <ConfigureMcpReq as WorkspaceRpc>::METHOD => {
                let session_id = bound_session.ok_or_else(|| {
                    WorkspaceError::HubError("configure_mcp requires a bound session".into())
                })?;
                let configs: Vec<agent_client_protocol::McpServer> = serde_json::from_value(
                    params
                        .get("mcp_servers")
                        .cloned()
                        .unwrap_or(Value::Array(vec![])),
                )
                .map_err(|e| WorkspaceError::HubError(format!("invalid mcp_servers: {e}")))?;
                let result = async {
                    if self.workspace.session(session_id).is_none() {
                        tracing::info!(
                            session_id,
                            "workspace.configure_mcp: session not found, creating on demand"
                        );
                        match self
                            .workspace
                            .create_session_with_config(
                                session_id,
                                None,
                                None,
                                crate::capability::CapabilityMode::All,
                                None,
                                true,
                            )
                        {
                            Ok(session) => {
                                self.workspace.finalize_session_setup(&session).await;
                            }
                            Err(WorkspaceError::SessionAlreadyExists(_)) => {
                                tracing::debug!(
                                    session_id,
                                    "workspace.configure_mcp: session created concurrently, using existing"
                                );
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    self.workspace.start_session_mcp_servers(session_id, configs).await
                }
                    .await;
                record_mutation_rpc("configure_mcp", "self", session_id, &result);
                serde_json::to_value(&result?)
                    .map_err(|e| WorkspaceError::HubError(format!("serialize McpStartResult: {e}")))
            }
            <GitMetadataReq as WorkspaceRpc>::METHOD => {
                let cwd = self.workspace.root_cwd()?;
                let metadata =
                    crate::session::git::resolve_persisted_session_git_metadata_sync(&cwd);
                Ok(serde_json::to_value(metadata).unwrap_or(Value::Null))
            }
            <FuzzyStatusReq as WorkspaceRpc>::METHOD => {
                let search_id = params
                    .get("search_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkspaceError::HubError("missing search_id".into()))?;
                let results = self.workspace.fuzzy_get_results(search_id).await;
                match results {
                    Some(data) => serde_json::to_value(data)
                        .map_err(|e| WorkspaceError::HubError(e.to_string())),
                    None => Ok(Value::Null),
                }
            }
            "workspace.worktree_create_from_worktree"
            | <CreateWorktreeFromWorktreeSyncReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<CreateWorktreeFromWorktreeSyncReq>(params, &self.workspace, None)
                    .await
            }
            <PrepareWorktreeFromWorktreeReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<PrepareWorktreeFromWorktreeReq>(params, &self.workspace, None).await
            }
            <WorktreeCreateSyncReq as WorkspaceRpc>::METHOD => {
                let req: crate::worktree::CreateWorktreeRequest = serde_json::from_value(params)
                    .map_err(|e| {
                        WorkspaceError::HubError(format!("invalid create_sync params: {e}"))
                    })?;
                let result = crate::worktree::create_worktree_streaming(&req, &NoOpNotifier).await;
                serde_json::to_value(result).map_err(|e| WorkspaceError::HubError(e.to_string()))
            }
            <GitStatusExtReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitStatusExtReq>(params, &self.workspace, None).await
            }
            <GitFilesReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitFilesReq>(params, &self.workspace, None).await
            }
            <GitDiffReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitDiffReq>(params, &self.workspace, None).await
            }
            <GitStageReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitStageReq>(params, &self.workspace, None).await
            }
            <GitStageContentReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitStageContentReq>(params, &self.workspace, None).await
            }
            <GitUnstageReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitUnstageReq>(params, &self.workspace, None).await
            }
            <GitDiscardReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitDiscardReq>(params, &self.workspace, None).await
            }
            <GitCommitReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitCommitReq>(params, &self.workspace, None).await
            }
            <GitCheckoutReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitCheckoutReq>(params, &self.workspace, None).await
            }
            <GitStashReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitStashReq>(params, &self.workspace, None).await
            }
            <GitInfoReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitInfoReq>(params, &self.workspace, None).await
            }
            <GitBranchesReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitBranchesReq>(params, &self.workspace, None).await
            }
            <GitCollectChangesReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitCollectChangesReq>(params, &self.workspace, None).await
            }
            <GitResolveRootReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitResolveRootReq>(params, &self.workspace, None).await
            }
            <GitCurrentCommitReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitCurrentCommitReq>(params, &self.workspace, None).await
            }
            <DetectVcsKindReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<DetectVcsKindReq>(params, &self.workspace, None).await
            }
            <GitCheckoutCommitReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<GitCheckoutCommitReq>(params, &self.workspace, None).await
            }
            <HunkSingleActionReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkSingleActionReq>(params, &self.workspace, bound_session).await
            }
            <HunkFileActionReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkFileActionReq>(params, &self.workspace, bound_session).await
            }
            <HunkTurnActionReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkTurnActionReq>(params, &self.workspace, bound_session).await
            }
            <HunkAllActionReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkAllActionReq>(params, &self.workspace, bound_session).await
            }
            <HunkGetAllFileContentsReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkGetAllFileContentsReq>(params, &self.workspace, bound_session)
                    .await
            }
            <HunkGetSessionSummaryReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkGetSessionSummaryReq>(params, &self.workspace, bound_session)
                    .await
            }
            <HunkGetAllHunksReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkGetAllHunksReq>(params, &self.workspace, bound_session).await
            }
            <HunkGetStagedFilesReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkGetStagedFilesReq>(params, &self.workspace, bound_session).await
            }
            <HunkGetFilteredHunksReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkGetFilteredHunksReq>(params, &self.workspace, bound_session).await
            }
            <HunkGetFileSummariesReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<HunkGetFileSummariesReq>(params, &self.workspace, bound_session).await
            }
            <CodeGotoDefinitionReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<CodeGotoDefinitionReq>(params, &self.workspace, None).await
            }
            <CodeGotoReferencesReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<CodeGotoReferencesReq>(params, &self.workspace, None).await
            }
            <CodeFindDefinitionsReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<CodeFindDefinitionsReq>(params, &self.workspace, None).await
            }
            <CodeFindReferencesReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<CodeFindReferencesReq>(params, &self.workspace, None).await
            }
            <CodeIndexStatusReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<CodeIndexStatusReq>(params, &self.workspace, None).await
            }
            <FuzzyOpenReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<FuzzyOpenReq>(params, &self.workspace, None).await
            }
            <FuzzyChangeReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<FuzzyChangeReq>(params, &self.workspace, None).await
            }
            <FuzzyCloseReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<FuzzyCloseReq>(params, &self.workspace, None).await
            }
            <ContentSearchRequest as WorkspaceRpc>::METHOD => {
                dispatch_op::<ContentSearchRequest>(params, &self.workspace, None).await
            }
            <CreateWorktreeRequest as WorkspaceRpc>::METHOD => {
                dispatch_op::<CreateWorktreeRequest>(params, &self.workspace, None).await
            }
            <RemoveWorktreeRequest as WorkspaceRpc>::METHOD => {
                dispatch_op::<RemoveWorktreeRequest>(params, &self.workspace, None).await
            }
            <ApplyWorktreeRequest as WorkspaceRpc>::METHOD => {
                dispatch_op::<ApplyWorktreeRequest>(params, &self.workspace, None).await
            }
            <WorktreeListReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<WorktreeListReq>(params, &self.workspace, None).await
            }
            <WorktreeShowReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<WorktreeShowReq>(params, &self.workspace, None).await
            }
            <WorktreeGcReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<WorktreeGcReq>(params, &self.workspace, None).await
            }
            <WorktreeDbStatsReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<WorktreeDbStatsReq>(params, &self.workspace, None).await
            }
            <WorktreeDbRebuildReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<WorktreeDbRebuildReq>(params, &self.workspace, None).await
            }
            <WorktreeDbPathReq as WorkspaceRpc>::METHOD => {
                dispatch_op::<WorktreeDbPathReq>(params, &self.workspace, None).await
            }
            <BeginPromptReq as WorkspaceRpc>::METHOD => {
                let req: BeginPromptReq = serde_json::from_value(params).map_err(|e| {
                    WorkspaceError::HubError(format!("invalid params for begin_prompt: {e}"))
                })?;
                self.workspace
                    .session(&req.session_id)
                    .ok_or_else(|| WorkspaceError::SessionNotFound(req.session_id.clone()))?;
                self.workspace
                    .on_turn_boundary(
                        &req.session_id,
                        TurnBoundary::rewind_begin(req.prompt_index),
                    )
                    .await;
                Ok(Value::Null)
            }
            <EndPromptReq as WorkspaceRpc>::METHOD => {
                let req: EndPromptReq = serde_json::from_value(params).map_err(|e| {
                    WorkspaceError::HubError(format!("invalid params for end_prompt: {e}"))
                })?;
                self.workspace
                    .session(&req.session_id)
                    .ok_or_else(|| WorkspaceError::SessionNotFound(req.session_id.clone()))?;
                self.workspace
                    .on_turn_boundary(
                        &req.session_id,
                        TurnBoundary::rewind_finalize(req.prompt_index),
                    )
                    .await;
                Ok(Value::Null)
            }
            <GetRewindPointsReq as WorkspaceRpc>::METHOD => {
                let req: GetRewindPointsReq = serde_json::from_value(params).map_err(|e| {
                    WorkspaceError::HubError(format!("invalid params for get_rewind_points: {e}"))
                })?;
                let session = self
                    .workspace
                    .session(&req.session_id)
                    .ok_or_else(|| WorkspaceError::SessionNotFound(req.session_id.clone()))?;
                let points = session
                    .file_state_tracker()
                    .get_rewind_points_normalized(session.cwd())
                    .await;
                serde_json::to_value(points).map_err(|e| WorkspaceError::HubError(e.to_string()))
            }
            <RewindToReq as WorkspaceRpc>::METHOD => {
                let req: RewindToReq = serde_json::from_value(params).map_err(|e| {
                    WorkspaceError::HubError(format!("invalid params for rewind_to: {e}"))
                })?;
                self.workspace
                    .session(&req.session_id)
                    .ok_or_else(|| WorkspaceError::SessionNotFound(req.session_id.clone()))?;
                let response = self
                    .workspace
                    .rewind_to(&req.session_id, req.target_prompt_index)
                    .await;
                serde_json::to_value(response).map_err(|e| WorkspaceError::HubError(e.to_string()))
            }
            _ => {
                tracing::warn!(method, "unknown workspace rpc method");
                Err(WorkspaceError::HubError(format!(
                    "{UNKNOWN_METHOD_ERR_PREFIX} {method}"
                )))
            }
        }
    }
}
#[async_trait]
impl ToolServerHandler for WorkspaceRpcHandler {
    fn tool_id(&self) -> ToolId {
        ToolId::new(WORKSPACE_RPC_TOOL_ID).expect("constant is a valid ToolId")
    }
    fn description(&self) -> ToolDescription {
        ToolDescription::new(
            WORKSPACE_RPC_TOOL_ID,
            "Routes workspace RPC calls to the local workspace handle.",
        )
    }
    fn input_schema(&self) -> Option<Value> {
        Some(serde_json::json!(
            { "type" : "object", "properties" : { "method" : { "type" : "string",
            "description" : "The workspace.* method to invoke" }, "params" : { "type"
            : "object", "description" : "Method parameters" } }, "required" :
            ["method"] }
        ))
    }
    async fn handle_call(&self, ctx: ToolCallContext, args: Value) -> ToolStream<TypedToolOutput> {
        let tool_id = self.tool_id();
        let method = match args.get("method").and_then(Value::as_str) {
            Some(m) => m,
            None => {
                return terminal_only(Err(ToolError::new(
                    ToolErrorKind::InvalidArguments,
                    "missing required field: method",
                )));
            }
        };
        tracing::debug!("workspace rpc call from server");
        let params = args
            .get("params")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let bound_session = ctx.extensions.get::<xai_tool_runtime::SessionContext>();
        let start = std::time::Instant::now();
        let result = self
            .dispatch(
                method,
                params,
                bound_session.as_deref().map(|s| s.0.as_str()),
            )
            .await;
        let is_unknown_method = matches!(
            & result, Err(WorkspaceError::HubError(msg)) if msg
            .starts_with(UNKNOWN_METHOD_ERR_PREFIX)
        );
        let method_label = if is_unknown_method {
            UNKNOWN_METHOD_LABEL
        } else {
            method
        };
        WORKSPACE_RPC_REQUESTS_TOTAL
            .with_label_values(&[method_label, if result.is_ok() { "ok" } else { "error" }])
            .inc();
        WORKSPACE_RPC_DURATION_SECONDS
            .with_label_values(&[method_label])
            .observe(start.elapsed().as_secs_f64());
        let envelope = match result {
            Ok(value) => RpcEnvelope::ok(value),
            Err(ref e) => envelope_err(e),
        };
        let envelope =
            serde_json::to_value(envelope).expect("RpcEnvelope<Value> serialization is infallible");
        terminal_only(Ok(TypedToolOutput::from_value(tool_id, envelope)))
    }
    async fn handle_hook(&self, session_id: SessionId, frame: HookFrame) {
        match frame.event {
            HookEvent::Cancel => {
                if let Some(call_id) = &frame.call_id {
                    tracing::info!(% session_id, % call_id, "cancel hook received");
                    self.workspace
                        .cancel_tool_call(session_id.as_str(), call_id.as_str());
                } else {
                    tracing::info!(% session_id, "cancel hook received (session-wide)");
                    self.workspace.cancel_all_tool_calls(session_id.as_str());
                }
            }
            HookEvent::SessionEnded => {
                tracing::info!(% session_id, "session_ended hook received");
                self.workspace
                    .teardown_session_mcp(session_id.as_str())
                    .await;
                self.workspace.on_session_ended(session_id.as_str());
            }
            HookEvent::Custom { kind, payload } => {
                use xai_tool_protocol::turn_hook::{
                    AFTER_TURN_KIND, AfterTurnPayload, BEFORE_TURN_KIND, BeforeTurnPayload,
                };
                match kind.as_str() {
                    BEFORE_TURN_KIND => {
                        match serde_json::from_value::<BeforeTurnPayload>(payload) {
                            Ok(p) => {
                                tracing::info!(
                                    session = % session_id, turn = p.turn_number, model = % p
                                    .model_id, "before_turn hook received"
                                );
                                self.workspace.on_before_turn(session_id.as_str(), &p).await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = % e, "before_turn payload deserialization failed"
                                );
                            }
                        }
                    }
                    AFTER_TURN_KIND => match serde_json::from_value::<AfterTurnPayload>(payload) {
                        Ok(p) => {
                            tracing::info!(
                                session = % session_id, turn = p.turn_number, outcome = ? p
                                .outcome, duration_ms = p.duration_ms,
                                "after_turn hook received"
                            );
                            self.workspace.on_after_turn(session_id.as_str(), &p).await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = % e, "after_turn payload deserialization failed"
                            );
                        }
                    },
                    _ => {
                        tracing::debug!(
                            kind = % kind, session = % session_id,
                            "unrecognized custom hook kind"
                        );
                    }
                }
            }
            HookEvent::Pause | HookEvent::Resume => {
                tracing::debug!(
                    % session_id, event = ? frame.event, "hook not yet implemented"
                );
            }
        }
    }
    async fn handle_hook_request(&self, session_id: SessionId, frame: HookFrame) -> Option<Value> {
        use xai_tool_protocol::turn_hook::{self, TurnHookRequest};
        let HookEvent::Custom { kind, payload } = frame.event else {
            return None;
        };
        if kind != turn_hook::TURN_HOOK_KIND {
            return None;
        }
        let no_op = || serde_json::to_value(turn_hook::HookReply::default()).ok();
        if self.workspace.shared.activity_tracker.is_draining()
            || self.workspace.session(session_id.as_str()).is_none()
        {
            return no_op();
        }
        let request: TurnHookRequest = match serde_json::from_value(payload) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = % e, % session_id, "invalid turn hook request");
                return no_op();
            }
        };
        let reply = self
            .workspace
            .compute_turn_injections(session_id.as_str(), &request)
            .await;
        Some(serde_json::to_value(&reply).unwrap_or(Value::Null))
    }
    /// Hub-issued `tool_server.evict`. Always tears the evicted session down
    /// (MCP bridges + activity/writer state, like the `SessionEnded` hook), then
    /// runs the global two-phase drain **only** when no other session survives —
    /// a global drain shuts down the *shared* upload queue, which must not happen
    /// while another session is live. Idempotent across fan-out and safe for an
    /// already-gone session id.
    ///
    /// Contract: the server-supplied `grace_period_ms` budgets the drain and is
    /// therefore honored only when evicting the **last** live session. For a
    /// multi-session workspace the evicted session is dropped immediately
    /// (no per-session drain) because the shared upload queue cannot be flushed
    /// or closed without affecting the survivors.
    async fn handle_evict(&self, params: ToolServerEvictParams) {
        let sid = params.session_id.as_str();
        self.workspace.teardown_session_mcp(sid).await;
        self.workspace.on_session_ended(sid);
        let (became_empty, start_drain) = {
            let mut sessions = self.workspace.shared.sessions.write();
            if let Some(session) = sessions.remove(sid) {
                session.abort_system_notify_forwarder();
                session.shutdown_terminal_backend();
                session.cancel_hunk_tracker();
            }
            let empty = sessions.is_empty();
            let already_winding_down = self.workspace.activity_tracker().is_draining();
            let start = empty && !already_winding_down;
            if start {
                self.workspace.activity_tracker().set_draining();
            }
            (empty, start)
        };
        if !start_drain {
            if became_empty {
                tracing::info!(
                    session = % params.session_id, reason = % params.reason,
                    "workspace: hub evict — already draining/shutting down; dropped session only"
                );
            } else {
                tracing::info!(
                    session = % params.session_id, reason = % params.reason,
                    "workspace: hub evict — other sessions live; dropped session only"
                );
            }
            return;
        }
        let grace = std::time::Duration::from_millis(params.grace_period_ms);
        tracing::info!(
            session = % params.session_id, reason = % params.reason, grace_period_ms =
            params.grace_period_ms,
            "workspace: hub evict — last session; commencing two-phase drain"
        );
        let unfinished = self
            .workspace
            .two_phase_drain(grace, crate::handle::DrainReason::Evict)
            .await;
        if unfinished > 0 {
            tracing::warn!(
                session = % params.session_id, unfinished,
                "workspace: hub evict drain left items pending"
            );
        }
        self.workspace.activity_tracker().set_shutting_down();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilityMode;
    use crate::handle::tests::{background_capable_cfg, make_handle, start_background_sleep};
    use xai_grok_tools::implementations::grok_build::scheduler::types::{
        ScheduledTask, SchedulerState,
    };
    use xai_grok_tools::types::resources::State;
    use xai_tool_protocol::turn_hook;
    /// Helper: consume the first item from a ToolStream.
    async fn next_item(
        stream: &mut ToolStream<TypedToolOutput>,
    ) -> Option<xai_tool_runtime::ToolStreamItem<TypedToolOutput>> {
        use std::task::Context;
        std::future::poll_fn(|cx: &mut Context<'_>| stream.as_mut().poll_next(cx)).await
    }
    fn turn_hook_frame(session: &str, req: &turn_hook::TurnHookRequest) -> HookFrame {
        HookFrame::custom_request(
            SessionId::new(session).unwrap(),
            "hk-test".to_owned(),
            turn_hook::TURN_HOOK_KIND.to_owned(),
            serde_json::to_value(req).unwrap(),
        )
    }
    #[tokio::test]
    async fn handle_hook_request_turn_hook_returns_reply() {
        let handler = WorkspaceRpcHandler::new(make_handle());
        let req = turn_hook::TurnHookRequest::Before(turn_hook::BeforeTurnPayload {
            turn_number: 1,
            model_id: "grok-3".to_owned(),
            yolo_mode: false,
            conversation_message_count: 0,
            session_relationship: "primary".to_owned(),
            schema_version: "1.0".to_owned(),
        });
        let value = handler
            .handle_hook_request(
                SessionId::new("main").unwrap(),
                turn_hook_frame("main", &req),
            )
            .await
            .expect("turn hook claimed");
        let reply: turn_hook::HookReply = serde_json::from_value(value).unwrap();
        assert_eq!(reply, turn_hook::HookReply::default());
    }
    #[tokio::test]
    async fn handle_hook_request_ignores_non_turn_hook_kind() {
        let handler = WorkspaceRpcHandler::new(make_handle());
        let frame = HookFrame::custom_request(
            SessionId::new("main").unwrap(),
            "hk-x".to_owned(),
            "some_other_kind".to_owned(),
            serde_json::json!({}),
        );
        assert!(
            handler
                .handle_hook_request(SessionId::new("main").unwrap(), frame)
                .await
                .is_none()
        );
    }
    #[tokio::test]
    async fn handle_hook_request_unbound_session_is_noop() {
        let handler = WorkspaceRpcHandler::new(make_handle());
        let req = turn_hook::TurnHookRequest::Before(turn_hook::BeforeTurnPayload {
            turn_number: 1,
            model_id: "grok-3".to_owned(),
            yolo_mode: false,
            conversation_message_count: 0,
            session_relationship: "primary".to_owned(),
            schema_version: "1.0".to_owned(),
        });
        let value = handler
            .handle_hook_request(
                SessionId::new("never-bound").unwrap(),
                turn_hook_frame("never-bound", &req),
            )
            .await
            .expect("fail-open no-op reply");
        let reply: turn_hook::HookReply = serde_json::from_value(value).unwrap();
        assert_eq!(reply, turn_hook::HookReply::default());
    }
    #[tokio::test]
    async fn dispatch_unknown_method_returns_hub_error() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.nonexistent", Value::Null, None)
            .await;
        assert!(matches!(result, Err(WorkspaceError::HubError(msg)) if msg
            .contains("unknown workspace method")));
    }
    /// A hub evict runs the two-phase drain then settles into terminal
    /// ShuttingDown (not a lingering Draining) for an evicted workspace.
    #[tokio::test]
    async fn handle_evict_triggers_two_phase_drain() {
        use xai_tool_protocol::ToolServerLifecycleStatus;
        let handle = make_handle();
        let tracker = handle.activity_tracker().clone();
        let handler = WorkspaceRpcHandler::new(handle);
        assert!(!tracker.is_draining(), "not draining before evict");
        handler
            .handle_evict(ToolServerEvictParams {
                session_id: SessionId::new("main").expect("valid session id"),
                reason: "preemption".into(),
                grace_period_ms: 200,
            })
            .await;
        let snap = tracker.snapshot();
        assert_eq!(
            snap.status,
            ToolServerLifecycleStatus::ShuttingDown,
            "an evicted workspace must end in terminal ShuttingDown, not lingering Draining"
        );
        assert!(
            snap.drain_started_ms.is_some(),
            "evict drain must stamp drain_started_ms"
        );
    }
    /// A hub evict shuts the evicted session's terminal backend down
    /// explicitly: the actor stops even while other `Arc`s to the backend are
    /// still alive (mirrors `drop_session_shuts_down_terminal_backend_explicitly`).
    #[tokio::test]
    async fn handle_evict_shuts_down_terminal_backend_explicitly() {
        let handle = make_handle();
        let session = handle.session("main").expect("main session exists");
        let retained_backend = session.terminal_backend().clone();
        let retained_toolset = session.toolset();
        drop(session);
        let handler = WorkspaceRpcHandler::new(handle);
        handler
            .handle_evict(ToolServerEvictParams {
                session_id: SessionId::new("main").expect("valid session id"),
                reason: "preemption".into(),
                grace_period_ms: 100,
            })
            .await;
        crate::handle::tests::assert_backend_stops(&retained_backend).await;
        drop(retained_toolset);
    }
    /// Isolation matrix #1/#3 at the RPC surface: `workspace.list_background_tasks`
    /// (the post-compaction reminder source of truth) stays truthful across
    /// both rebind shapes. The task stays listed through a `Reused` rebind
    /// AND a `Reresolved` toolset swap — reading it through each rebind's
    /// CURRENT toolset — and leaves the list only when explicitly killed.
    #[tokio::test]
    async fn list_background_tasks_rpc_stays_truthful_across_rebinds() {
        use crate::capability::CapabilityMode;
        use crate::handle::RebindOutcome;
        use crate::handle::tests::{background_capable_cfg, start_background_sleep};
        use crate::session::tool_config::test_support::tc;
        use xai_grok_tools::registry::types::ToolServerConfig;
        use xai_grok_workspace_types::rpc::workspace::ListBackgroundTasksResponse;
        let handle = make_handle();
        let cfg = background_capable_cfg();
        let session = handle
            .create_session_with_config(
                "bg-rpc",
                None,
                Some(cfg.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create background-capable session");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg).ok());
        let out_dir = tempfile::tempdir().expect("temp dir");
        let bg = start_background_sleep(&session, out_dir.path(), "bg-rpc-task").await;
        let handler = WorkspaceRpcHandler::new(handle.clone());
        async fn list_tasks(
            handler: &WorkspaceRpcHandler,
        ) -> Vec<xai_grok_workspace_types::rpc::workspace::BackgroundTaskSummaryWire> {
            let value = handler
                .dispatch(
                    "workspace.list_background_tasks",
                    serde_json::json!({ "session_id" : "bg-rpc" }),
                    Some("bg-rpc"),
                )
                .await
                .expect("list_background_tasks rpc");
            serde_json::from_value::<ListBackgroundTasksResponse>(value)
                .expect("decode response")
                .tasks
        }
        let tasks = list_tasks(&handler).await;
        assert_eq!(tasks.len(), 1, "the running task must be listed");
        assert_eq!(tasks[0].task_id, bg.task_id);
        assert_eq!(
            tasks[0].tool_name.as_deref(),
            Some("run_terminal_cmd"),
            "the creator tool is named from the live toolset"
        );
        let (_, outcome) = handle
            .rebind_existing_hub_session(
                "bg-rpc",
                Some(cfg.clone()),
                serde_json::to_value(&cfg).ok(),
            )
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reused);
        let tasks = list_tasks(&handler).await;
        assert_eq!(tasks.len(), 1, "the task must survive a reused rebind");
        let read_only = ToolServerConfig {
            tools: vec![tc(
                "GrokBuild:read_file",
                Some(xai_grok_tools::types::tool::ToolKind::Read),
            )],
            behavior_preset: None,
        };
        let (_, outcome) = handle
            .rebind_existing_hub_session(
                "bg-rpc",
                Some(read_only.clone()),
                serde_json::to_value(&read_only).ok(),
            )
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reresolved);
        let tasks = list_tasks(&handler).await;
        assert_eq!(tasks.len(), 1, "the task must survive the toolset swap");
        assert_eq!(tasks[0].task_id, bg.task_id);
        assert_eq!(
            tasks[0].tool_name, None,
            "the swapped-in toolset has no execute tool to name"
        );
        session.terminal_backend().kill_task(&bg.task_id).await;
        let tasks = list_tasks(&handler).await;
        assert!(
            tasks.is_empty(),
            "a killed task must leave the outstanding list: {tasks:?}"
        );
    }
    /// `workspace.tasks_snapshot` (GC-614 part 3): returns the outstanding
    /// background task with kind/started_at, plus scheduled tasks (empty when
    /// no scheduler resource exists), and drops the task once killed.
    #[tokio::test]
    async fn tasks_snapshot_rpc_lists_outstanding_background_tasks() {
        let handle = make_handle();
        let cfg = background_capable_cfg();
        let session = handle
            .create_session_with_config(
                "snap-rpc",
                None,
                Some(cfg.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create background-capable session");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg).ok());
        let out_dir = tempfile::tempdir().expect("temp dir");
        let bg = start_background_sleep(&session, out_dir.path(), "snap-rpc-task").await;
        let handler = WorkspaceRpcHandler::new(handle.clone());
        async fn snapshot(handler: &WorkspaceRpcHandler) -> TasksSnapshotResponse {
            let value = handler
                .dispatch(
                    "workspace.tasks_snapshot",
                    serde_json::json!({ "session_id" : "snap-rpc" }),
                    Some("snap-rpc"),
                )
                .await
                .expect("tasks_snapshot rpc");
            serde_json::from_value(value).expect("decode response")
        }
        let snap = snapshot(&handler).await;
        assert_eq!(
            snap.background_tasks.len(),
            1,
            "the running task must be listed"
        );
        let task = &snap.background_tasks[0];
        assert_eq!(task.task_id, bg.task_id);
        assert_eq!(task.kind, "bash");
        assert!(
            DateTime::parse_from_rfc3339(&task.started_at).is_ok(),
            "started_at must be RFC3339: {}",
            task.started_at
        );
        assert!(
            snap.scheduled_tasks.is_empty(),
            "no scheduler resource in this toolset: {:?}",
            snap.scheduled_tasks
        );
        session.terminal_backend().kill_task(&bg.task_id).await;
        let snap = snapshot(&handler).await;
        assert!(
            snap.background_tasks.is_empty(),
            "a killed task must leave the snapshot: {:?}",
            snap.background_tasks
        );
        {
            let toolset = session.toolset();
            let mut resources = toolset.resources.lock().await;
            let state = resources.get_or_default::<State<SchedulerState>>();
            let mut task = ScheduledTask::new(300, "check CI".into(), true, false);
            task.id = "loop-1".into();
            state.tasks.push(task);
        }
        let snap = snapshot(&handler).await;
        assert_eq!(snap.scheduled_tasks.len(), 1);
        let loop_task = &snap.scheduled_tasks[0];
        assert_eq!(loop_task.task_id, "loop-1");
        assert_eq!(loop_task.prompt, "check CI");
        assert_eq!(loop_task.human_schedule, "every 5 minutes");
        assert!(loop_task.recurring);
        assert!(
            DateTime::parse_from_rfc3339(&loop_task.next_fire_at).is_ok(),
            "next_fire_at must be RFC3339: {}",
            loop_task.next_fire_at
        );
    }
    /// Evicting one session while another is live must NOT global-drain (which
    /// would close the shared queue for the survivor) — even when the evicted
    /// id is no longer in the session map.
    #[tokio::test]
    async fn handle_evict_keeps_queue_when_other_sessions_live() {
        let handle = make_handle();
        handle
            .create_session("other")
            .expect("create second session");
        let tracker = handle.activity_tracker().clone();
        let handler = WorkspaceRpcHandler::new(handle);
        handler
            .handle_evict(ToolServerEvictParams {
                session_id: SessionId::new("ghost").expect("valid session id"),
                reason: "idle_timeout".into(),
                grace_period_ms: 200,
            })
            .await;
        assert!(
            !tracker.is_draining(),
            "evict of an absent id with live sessions must not global-drain"
        );
    }
    /// Evicting one of several live sessions removes *that* session (full
    /// teardown), keeps the survivors, and does not global-drain the shared
    /// queue. The drain decision is made on the post-removal map.
    #[tokio::test]
    async fn handle_evict_nonlast_removes_session_and_preserves_survivors() {
        let handle = make_handle();
        handle
            .create_session("other")
            .expect("create second session");
        let tracker = handle.activity_tracker().clone();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        handler
            .handle_evict(ToolServerEvictParams {
                session_id: SessionId::new("other").expect("valid session id"),
                reason: "idle_timeout".into(),
                grace_period_ms: 200,
            })
            .await;
        assert!(
            handle.session("other").is_none(),
            "the evicted session must be removed from the map"
        );
        assert!(
            handle.session("main").is_some(),
            "a surviving session must be kept"
        );
        assert!(
            !tracker.is_draining(),
            "evicting a non-last session must not global-drain the shared queue"
        );
    }
    /// Once a terminal evict drain has started, a racing `bind`/create must be
    /// rejected so the shared upload queue is never torn down under a fresh
    /// session (race #3).
    #[tokio::test]
    async fn bind_rejected_after_evict_drain() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        handler
            .handle_evict(ToolServerEvictParams {
                session_id: SessionId::new("main").expect("valid session id"),
                reason: "preemption".into(),
                grace_period_ms: 100,
            })
            .await;
        assert!(matches!(
            handle.create_session("late"),
            Err(WorkspaceError::ShuttingDown)
        ));
    }
    /// A duplicate / retried evict of the last session must not re-run the
    /// drain or downgrade terminal `ShuttingDown` back to `Draining`.
    #[tokio::test]
    async fn repeat_evict_does_not_redrain() {
        use xai_tool_protocol::ToolServerLifecycleStatus;
        let handle = make_handle();
        let tracker = handle.activity_tracker().clone();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = || ToolServerEvictParams {
            session_id: SessionId::new("main").expect("valid session id"),
            reason: "preemption".into(),
            grace_period_ms: 100,
        };
        handler.handle_evict(params()).await;
        assert_eq!(
            tracker.snapshot().status,
            ToolServerLifecycleStatus::ShuttingDown
        );
        handler.handle_evict(params()).await;
        assert_eq!(
            tracker.snapshot().status,
            ToolServerLifecycleStatus::ShuttingDown,
            "a repeat evict must not downgrade terminal ShuttingDown to Draining"
        );
    }
    #[tokio::test]
    async fn dispatch_tool_definitions_returns_known_tools() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!({ "session_id" : "main" });
        let result = handler
            .dispatch("workspace.tool_definitions", params, None)
            .await;
        let value = result.expect("should succeed");
        let arr = value.as_array().expect("should be array");
        assert!(!arr.is_empty(), "main session should have tools");
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|d| {
                d.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        assert!(
            names.contains(&"read_file"),
            "should contain read_file: {names:?}"
        );
    }
    #[tokio::test]
    async fn dispatch_tool_definitions_unknown_session() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!({ "session_id" : "ghost" });
        let result = handler
            .dispatch("workspace.tool_definitions", params, None)
            .await;
        assert!(matches!(result, Err(WorkspaceError::SessionNotFound(_))));
    }
    #[tokio::test]
    async fn dispatch_get_all_hunks_returns_array() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.get_all_hunks", Value::Null, Some("main"))
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_array());
    }
    #[tokio::test]
    async fn dispatch_get_session_summary_returns_object_or_null() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.get_session_summary", Value::Null, Some("main"))
            .await;
        let value = result.expect("should succeed");
        assert!(
            value.is_object() || value.is_null(),
            "expected object or null, got {value}"
        );
    }
    #[tokio::test]
    async fn dispatch_discover_skills_returns_array() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.discover_skills", Value::Null, None)
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_array());
    }
    #[tokio::test]
    async fn dispatch_load_envrc_returns_object() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.load_envrc", Value::Null, None)
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_object());
    }
    #[tokio::test]
    async fn dispatch_drop_session_self_succeeds() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        let params = serde_json::json!(
            { "caller_session_id" : "main", "session_id" : "main" }
        );
        let result = handler
            .dispatch("workspace.drop_session", params, None)
            .await;
        assert!(result.is_ok(), "dropping own session should succeed");
        assert!(handle.session("main").is_none(), "session should be gone");
    }
    #[tokio::test]
    async fn dispatch_update_tool_config_missing_params() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.update_tool_config", serde_json::json!({}), None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(ref msg)) if msg
            .contains("missing")),
            "got {result:?}"
        );
    }
    fn caller_mismatch_count(method: &str, kind: &str) -> u64 {
        WORKSPACE_RPC_CALLER_MISMATCH_TOTAL
            .with_label_values(&[method, kind])
            .get()
    }
    fn baseline_config_value() -> Value {
        serde_json::to_value(crate::session::tool_config::test_support::baseline_config())
            .expect("baseline config serializes")
    }
    /// With both an envelope session and a (spoofed) param, the envelope
    /// wins: the call is authorized as the envelope session and the
    /// mismatch is counted.
    #[tokio::test]
    async fn dispatch_update_tool_config_envelope_overrides_param() {
        let mismatch_before = caller_mismatch_count("update_tool_config", "param_mismatch");
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!(
            { "caller_session_id" : "spoofed", "session_id" : "main", "new_config" :
            baseline_config_value(), }
        );
        let result = handler
            .dispatch("workspace.update_tool_config", params, Some("main"))
            .await;
        assert!(
            result.is_ok(),
            "envelope caller == target must authorize: {result:?}"
        );
        assert!(
            caller_mismatch_count("update_tool_config", "param_mismatch") > mismatch_before,
            "the param/envelope disagreement must be counted"
        );
    }
    /// A forged `caller_session_id` param cannot authorize a cross-session
    /// mutation: the envelope session is the caller and differs from the
    /// target, so the target's caller-equals-target check rejects it.
    #[tokio::test]
    async fn dispatch_update_tool_config_envelope_cross_session_unauthorized() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        let params = serde_json::json!(
            { "caller_session_id" : "main", "session_id" : "main", "new_config" :
            baseline_config_value(), }
        );
        let result = handler
            .dispatch("workspace.update_tool_config", params, Some("other"))
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::Unauthorized { .. })),
            "got {result:?}"
        );
        assert!(
            handle.session("main").is_some(),
            "the target session must be untouched"
        );
    }
    /// Compat: without an envelope session (old call paths) the param is
    /// still honored, and the fallback is counted for the deprecation
    /// monitor.
    #[tokio::test]
    async fn dispatch_update_tool_config_param_fallback_without_envelope() {
        let absent_before = caller_mismatch_count("update_tool_config", "envelope_absent");
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!(
            { "caller_session_id" : "main", "session_id" : "main", "new_config" :
            baseline_config_value(), }
        );
        let result = handler
            .dispatch("workspace.update_tool_config", params, None)
            .await;
        assert!(result.is_ok(), "param fallback must authorize: {result:?}");
        assert!(
            caller_mismatch_count("update_tool_config", "envelope_absent") > absent_before,
            "the envelope-absent fallback must be counted"
        );
    }
    /// The intended steady state once clients drop the deprecated param:
    /// envelope-only identity (no `caller_session_id` in params) authorizes.
    /// Counter non-advance is asserted by
    /// [`resolve_mutation_caller_clean_arms_count_nothing`], which uses a
    /// test-unique method label — the real label is shared with concurrently
    /// running dispatch tests, so an equality assert here would flake.
    #[tokio::test]
    async fn dispatch_update_tool_config_envelope_only_without_param() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!(
            { "session_id" : "main", "new_config" : baseline_config_value(), }
        );
        let result = handler
            .dispatch("workspace.update_tool_config", params, Some("main"))
            .await;
        assert!(
            result.is_ok(),
            "envelope-only identity must authorize: {result:?}"
        );
    }
    /// The two clean `resolve_mutation_caller` arms — envelope-only and
    /// envelope+matching-param — resolve to the envelope without ticking
    /// either deprecation-monitor kind.
    #[test]
    fn resolve_mutation_caller_clean_arms_count_nothing() {
        const METHOD: &str = "test_clean_arms";
        let mismatch_before = caller_mismatch_count(METHOD, "param_mismatch");
        let absent_before = caller_mismatch_count(METHOD, "envelope_absent");
        let caller = resolve_mutation_caller(METHOD, Some("sess"), None)
            .expect("envelope-only must resolve");
        assert_eq!(caller, "sess");
        let caller = resolve_mutation_caller(METHOD, Some("sess"), Some("sess"))
            .expect("matching param must resolve");
        assert_eq!(caller, "sess");
        assert_eq!(
            caller_mismatch_count(METHOD, "param_mismatch"),
            mismatch_before,
            "clean arms must not count a mismatch"
        );
        assert_eq!(
            caller_mismatch_count(METHOD, "envelope_absent"),
            absent_before,
            "clean arms must not count an envelope-absent fallback"
        );
    }
    /// `drop_session` gets the same envelope-derived identity: a spoofed
    /// param is ignored when the envelope authorizes the drop, and the
    /// mutation audit counter advances.
    #[tokio::test]
    async fn dispatch_drop_session_envelope_overrides_param() {
        let mutation_before = WORKSPACE_RPC_MUTATION_TOTAL
            .with_label_values(&["drop_session", "ok"])
            .get();
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        let params = serde_json::json!(
            { "caller_session_id" : "spoofed", "session_id" : "main" }
        );
        let result = handler
            .dispatch("workspace.drop_session", params, Some("main"))
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(handle.session("main").is_none(), "session should be gone");
        assert!(
            WORKSPACE_RPC_MUTATION_TOTAL
                .with_label_values(&["drop_session", "ok"])
                .get()
                > mutation_before,
            "the mutation audit counter must advance"
        );
    }
    /// A cross-session drop forged via the param is rejected off the
    /// envelope identity and the target survives.
    #[tokio::test]
    async fn dispatch_drop_session_envelope_cross_session_unauthorized() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        let params = serde_json::json!(
            { "caller_session_id" : "main", "session_id" : "main" }
        );
        let result = handler
            .dispatch("workspace.drop_session", params, Some("observer-ish"))
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::Unauthorized { .. })),
            "got {result:?}"
        );
        assert!(
            handle.session("main").is_some(),
            "the target session must survive"
        );
    }
    /// `configure_mcp`'s on-demand session create opts into system
    /// notifications, like every other sandbox-path creator.
    #[tokio::test]
    async fn dispatch_configure_mcp_on_demand_create_enables_system_notifications() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        let _ = handler
            .dispatch(
                "workspace.configure_mcp",
                serde_json::json!({ "mcp_servers" : [] }),
                Some("mcp-fresh"),
            )
            .await;
        let session = handle
            .session("mcp-fresh")
            .expect("session created on demand");
        assert!(
            session.system_notifications(),
            "the on-demand created session must forward system notifications"
        );
    }
    #[tokio::test]
    async fn dispatch_hunk_action_unknown_action() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!(
            { "action" : { "hunk_id" : "test-id", "action" : "dance" } }
        );
        let result = handler
            .dispatch("workspace.hunk_action", params, None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(_))),
            "expected HubError for invalid action enum, got {result:?}"
        );
    }
    #[tokio::test]
    async fn dispatch_hunk_action_malformed_json() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!({ "action" : "not-an-object" });
        let result = handler
            .dispatch("workspace.hunk_action", params, None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(_))),
            "expected HubError for malformed action, got {result:?}"
        );
    }
    #[tokio::test]
    async fn dispatch_hunk_action_missing_action_field() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!({});
        let result = handler
            .dispatch("workspace.hunk_action", params, None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(ref msg)) if msg
            .contains("missing field")),
            "got {result:?}"
        );
    }
    #[tokio::test]
    async fn dispatch_hunk_file_action_missing_path() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!({ "action" : "accept" });
        let result = handler
            .dispatch("workspace.hunk_file_action", params, None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(ref msg)) if msg
            .contains("missing field")),
            "got {result:?}"
        );
    }
    #[tokio::test]
    async fn dispatch_hunk_turn_action_missing_prompt_index() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!({ "action" : "accept" });
        let result = handler
            .dispatch("workspace.hunk_turn_action", params, None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(ref msg)) if msg
            .contains("missing field")),
            "got {result:?}"
        );
    }
    #[tokio::test]
    async fn dispatch_hunk_all_action_invalid_action() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!({ "action" : "explode" });
        let result = handler
            .dispatch("workspace.hunk_all_action", params, None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(_))),
            "expected HubError for invalid action enum, got {result:?}"
        );
    }
    #[tokio::test]
    async fn dispatch_hunk_get_all_file_contents_returns_array() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch(
                "workspace.hunk_get_all_file_contents",
                Value::Null,
                Some("main"),
            )
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_array());
    }
    #[tokio::test]
    async fn dispatch_hunk_get_staged_files_returns_array() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.hunk_get_staged_files", Value::Null, Some("main"))
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_array());
    }
    #[tokio::test]
    async fn dispatch_fuzzy_open_returns_search_id() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch(
                "workspace.fuzzy_open",
                serde_json::json!({ "hidden" : false }),
                None,
            )
            .await;
        let value = result.expect("should succeed");
        assert!(
            value.as_str().is_some_and(|s| !s.is_empty()),
            "response should be a non-empty search_id string: {value}"
        );
    }
    #[tokio::test]
    async fn dispatch_fuzzy_close_unknown_id() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch(
                "workspace.fuzzy_close",
                serde_json::json!({ "search_id" : "nonexistent" }),
                None,
            )
            .await;
        let value = result.expect("should succeed");
        assert!(!value.as_bool().expect("response should be a bool"));
    }
    #[tokio::test]
    async fn dispatch_fuzzy_change_missing_search_id() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch(
                "workspace.fuzzy_change",
                serde_json::json!({ "query" : "test" }),
                None,
            )
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(ref msg)) if msg
            .contains("missing field")),
            "got {result:?}"
        );
    }
    #[tokio::test]
    async fn dispatch_fuzzy_search_missing_search_id() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.fuzzy_search", serde_json::json!({}), None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(ref msg)) if msg
            .contains("missing search_id")),
            "got {result:?}"
        );
    }
    #[tokio::test]
    async fn dispatch_fuzzy_open_then_close_roundtrip() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let open_result = handler
            .dispatch(
                "workspace.fuzzy_open",
                serde_json::json!({ "hidden" : false }),
                None,
            )
            .await
            .expect("open should succeed");
        let search_id = open_result
            .as_str()
            .expect("open response should be a search_id string")
            .to_owned();
        let close_result = handler
            .dispatch(
                "workspace.fuzzy_close",
                serde_json::json!({ "search_id" : search_id }),
                None,
            )
            .await
            .expect("close should succeed");
        assert!(
            close_result
                .as_bool()
                .expect("close response should be a bool")
        );
        let close_again = handler
            .dispatch(
                "workspace.fuzzy_close",
                serde_json::json!({ "search_id" : search_id }),
                None,
            )
            .await
            .expect("close again should succeed");
        assert!(
            !close_again
                .as_bool()
                .expect("close-again response should be a bool")
        );
    }
    #[tokio::test]
    async fn handle_call_wraps_in_envelope_with_value() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let mut ctx = ToolCallContext::default();
        ctx.extensions
            .insert(xai_tool_runtime::SessionContext("main".to_owned()));
        let args = serde_json::json!(
            { "method" : "workspace.get_session_summary", "params" : {} }
        );
        let mut stream = handler.handle_call(ctx, args).await;
        let item = next_item(&mut stream).await.expect("should have terminal");
        match item {
            xai_tool_runtime::ToolStreamItem::Terminal(Ok(typed)) => {
                let ok_val = typed
                    .value
                    .get("ok")
                    .expect("envelope should have 'ok' key");
                assert!(
                    ok_val.is_object() || ok_val.is_null(),
                    "ok value should be object or null, got {ok_val}"
                );
            }
            other => panic!("expected Terminal(Ok), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn handle_call_error_envelope() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let ctx = ToolCallContext::default();
        let args = serde_json::json!(
            { "method" : "workspace.nonexistent", "params" : {} }
        );
        let mut stream = handler.handle_call(ctx, args).await;
        let item = next_item(&mut stream).await.expect("should have terminal");
        match item {
            xai_tool_runtime::ToolStreamItem::Terminal(Ok(typed)) => {
                assert!(
                    typed.value.get("err").is_some(),
                    "envelope should have 'err' key: {}",
                    typed.value
                );
                let err = typed.value.get("err").unwrap();
                assert!(err.get("code").is_some());
                assert!(err.get("message").is_some());
            }
            other => panic!("expected Terminal(Ok(envelope)), got {other:?}"),
        }
    }
    /// `handle_call` records the RPC metrics: a known method increments its
    /// per-method `ok` series, and an unrecognized method collapses to
    /// `method="unknown",result="error"` — never creating a per-bad-method
    /// series (the cardinality-bounding guarantee).
    #[tokio::test]
    async fn handle_call_records_rpc_metrics_and_collapses_unknown_method() {
        let handler = WorkspaceRpcHandler::new(make_handle());
        let ok_before = WORKSPACE_RPC_REQUESTS_TOTAL
            .with_label_values(&["workspace.get_session_summary", "ok"])
            .get();
        let dur_samples_before = WORKSPACE_RPC_DURATION_SECONDS
            .with_label_values(&["workspace.get_session_summary"])
            .get_sample_count();
        let mut ctx = ToolCallContext::default();
        ctx.extensions
            .insert(xai_tool_runtime::SessionContext("main".to_owned()));
        let mut stream = handler
            .handle_call(
                ctx,
                serde_json::json!(
                    { "method" : "workspace.get_session_summary", "params" : {} }
                ),
            )
            .await;
        let _ = next_item(&mut stream).await;
        assert!(
            WORKSPACE_RPC_REQUESTS_TOTAL
                .with_label_values(&["workspace.get_session_summary", "ok"])
                .get()
                > ok_before,
            "a known ok RPC must increment its per-method ok counter"
        );
        assert!(
            WORKSPACE_RPC_DURATION_SECONDS
                .with_label_values(&["workspace.get_session_summary"])
                .get_sample_count()
                > dur_samples_before,
            "the dispatch must observe the per-method duration histogram"
        );
        const BOGUS: &str = "workspace.__test_bogus_method_zzz";
        let unknown_before = WORKSPACE_RPC_REQUESTS_TOTAL
            .with_label_values(&[UNKNOWN_METHOD_LABEL, "error"])
            .get();
        let mut stream = handler
            .handle_call(
                ToolCallContext::default(),
                serde_json::json!({ "method" : BOGUS, "params" : {} }),
            )
            .await;
        let _ = next_item(&mut stream).await;
        assert!(
            WORKSPACE_RPC_REQUESTS_TOTAL
                .with_label_values(&[UNKNOWN_METHOD_LABEL, "error"])
                .get()
                > unknown_before,
            "an unrecognized method must increment the collapsed unknown/error counter"
        );
        let has_bogus_series = prometheus::gather()
            .iter()
            .filter(|mf| mf.name() == "grok_workspace_rpc_requests_total")
            .flat_map(|mf| mf.get_metric())
            .any(|m| {
                m.get_label()
                    .iter()
                    .any(|l| l.name() == "method" && l.value() == BOGUS)
            });
        assert!(
            !has_bogus_series,
            "the raw bad method must collapse to `unknown`, never its own series"
        );
    }
    #[tokio::test]
    async fn dispatch_git_stage_non_git_dir_returns_error() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.git_stage", serde_json::json!({}), None)
            .await;
        assert!(result.is_err(), "non-git dir should error");
    }
    #[tokio::test]
    async fn dispatch_git_commit_missing_message_returns_error() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.git_commit", serde_json::json!({}), None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(ref msg)) if msg
            .contains("missing field"))
        );
    }
    #[tokio::test]
    async fn dispatch_git_checkout_missing_branch_returns_error() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.git_checkout", serde_json::json!({}), None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(ref msg)) if msg
            .contains("missing field"))
        );
    }
    #[tokio::test]
    async fn dispatch_git_stage_content_missing_fields() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let result = handler
            .dispatch("workspace.git_stage_content", serde_json::json!({}), None)
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::HubError(ref msg)) if msg
            .contains("missing"))
        );
    }
    #[tokio::test]
    async fn handle_hook_before_turn_sets_turn_state() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        let payload = turn_hook::BeforeTurnPayload {
            turn_number: 1,
            model_id: "grok-3".to_string(),
            yolo_mode: false,
            conversation_message_count: 0,
            session_relationship: "primary".to_string(),
            schema_version: "1.0".to_string(),
        };
        let frame = HookFrame {
            session_id: SessionId::new("main").unwrap(),
            tool_id: None,
            call_id: None,
            hook_id: None,
            event: HookEvent::Custom {
                kind: turn_hook::BEFORE_TURN_KIND.to_string(),
                payload: serde_json::to_value(&payload).unwrap(),
            },
            trace_context: None,
        };
        handler
            .handle_hook(SessionId::new("main").unwrap(), frame)
            .await;
        let tracker = handle.activity_tracker();
        assert!(
            tracker.known_sessions().contains(&"main".to_string()),
            "before_turn hook should create a session entry in the activity tracker"
        );
    }
    #[tokio::test]
    async fn handle_hook_after_turn_does_not_panic() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        handle.activity_tracker().turn_started("main", 1);
        let payload = turn_hook::AfterTurnPayload {
            turn_number: 1,
            outcome: turn_hook::TurnHookOutcome::Completed,
            duration_ms: 500,
            tool_call_count: 3,
            model_id: "grok-3".to_string(),
            written_repo_paths: Vec::new(),
            cancellation_category: None,
            cancellation_context: None,
        };
        let frame = HookFrame {
            session_id: SessionId::new("main").unwrap(),
            tool_id: None,
            call_id: None,
            hook_id: None,
            event: HookEvent::Custom {
                kind: turn_hook::AFTER_TURN_KIND.to_string(),
                payload: serde_json::to_value(&payload).unwrap(),
            },
            trace_context: None,
        };
        handler
            .handle_hook(SessionId::new("main").unwrap(), frame)
            .await;
    }
    #[tokio::test]
    async fn handle_hook_malformed_payload_does_not_panic() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let frame = HookFrame {
            session_id: SessionId::new("main").unwrap(),
            tool_id: None,
            call_id: None,
            hook_id: None,
            event: HookEvent::Custom {
                kind: turn_hook::BEFORE_TURN_KIND.to_string(),
                payload: serde_json::json!({ "garbage" : true }),
            },
            trace_context: None,
        };
        handler
            .handle_hook(SessionId::new("main").unwrap(), frame)
            .await;
    }
    #[tokio::test]
    async fn handle_hook_unrecognized_custom_kind_does_not_panic() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let frame = HookFrame {
            session_id: SessionId::new("main").unwrap(),
            tool_id: None,
            call_id: None,
            hook_id: None,
            event: HookEvent::Custom {
                kind: "unknown_kind".to_string(),
                payload: serde_json::json!({}),
            },
            trace_context: None,
        };
        handler
            .handle_hook(SessionId::new("main").unwrap(), frame)
            .await;
    }
    #[tokio::test]
    async fn handle_hook_cancel_marks_call_completed() {
        use xai_tool_protocol::ToolCallId;
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        let tracker = handle.activity_tracker();
        tracker.tool_call_started("call-42", "read_file", Some("main"));
        assert_eq!(tracker.snapshot().active_tool_calls, 1);
        let frame = HookFrame::cancel(
            SessionId::new("main").unwrap(),
            ToolId::new("read_file").unwrap(),
            ToolCallId::new("call-42").unwrap(),
        );
        handler
            .handle_hook(SessionId::new("main").unwrap(), frame)
            .await;
        assert_eq!(
            tracker.snapshot().active_tool_calls,
            0,
            "cancel hook should mark the call as completed"
        );
    }
    #[tokio::test]
    async fn handle_hook_cancel_without_call_id_cancels_all_session_calls() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        let tracker = handle.activity_tracker();
        tracker.tool_call_started("call-a", "grep", Some("main"));
        tracker.tool_call_started("call-b", "read_file", Some("main"));
        tracker.tool_call_started("call-c", "write", Some("other"));
        assert_eq!(tracker.snapshot().active_tool_calls, 3);
        let frame = HookFrame {
            session_id: SessionId::new("main").unwrap(),
            tool_id: None,
            call_id: None,
            hook_id: None,
            event: HookEvent::Cancel,
            trace_context: None,
        };
        handler
            .handle_hook(SessionId::new("main").unwrap(), frame)
            .await;
        assert_eq!(
            tracker.snapshot_session("main").active_tool_calls,
            0,
            "session-wide cancel should complete all calls for the session"
        );
        assert_eq!(
            tracker.snapshot_session("other").active_tool_calls,
            1,
            "cancel must not affect calls in other sessions"
        );
        assert_eq!(tracker.snapshot().active_tool_calls, 1);
    }
    #[tokio::test]
    async fn handle_hook_session_ended_clears_turn_active() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle.clone());
        let tracker = handle.activity_tracker();
        tracker.turn_started("main", 1);
        assert!(tracker.is_turn_active("main"));
        let frame = HookFrame::session_ended(SessionId::new("main").unwrap());
        handler
            .handle_hook(SessionId::new("main").unwrap(), frame)
            .await;
        assert!(
            !tracker.is_turn_active("main"),
            "session_ended hook should clear turn_active"
        );
    }
    use crate::workspace_ops::{GetFilesRes, PutFilesRes};
    /// Helper: compute SHA-256 hex digest for test assertions.
    fn test_sha256(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(data))
    }
    #[tokio::test]
    async fn dispatch_put_files_writes_and_returns_hash() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!(
            { "files" : [{ "path" : "test_file.txt", "content" : "hello world" }] }
        );
        let result = handler
            .dispatch("workspace.put_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: PutFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].ok, "write should succeed");
        let expected_hash = test_sha256(b"hello world");
        assert_eq!(
            res.results[0].hash.as_deref(),
            Some(expected_hash.as_str()),
            "hash should be SHA-256 of written content"
        );
        assert!(res.results[0].error.is_none(), "no error expected");
        let on_disk = std::fs::read_to_string(root.join("test_file.txt")).unwrap();
        assert_eq!(on_disk, "hello world");
    }
    #[tokio::test]
    async fn dispatch_put_files_rejects_path_traversal() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!(
            { "files" : [{ "path" : "../escape.txt", "content" : "evil" }] }
        );
        let result = handler
            .dispatch("workspace.put_files", params, None)
            .await
            .expect("dispatch itself should succeed");
        let res: PutFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(!res.results[0].ok, "path traversal should be rejected");
        assert!(
            res.results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("escapes workspace root"),
            "error should mention escape: {:?}",
            res.results[0].error
        );
    }
    #[tokio::test]
    async fn handle_hook_pause_resume_are_noops() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        for event in [HookEvent::Pause, HookEvent::Resume] {
            let frame = HookFrame {
                session_id: SessionId::new("main").unwrap(),
                tool_id: None,
                call_id: None,
                hook_id: None,
                event,
                trace_context: None,
            };
            handler
                .handle_hook(SessionId::new("main").unwrap(), frame)
                .await;
        }
    }
    #[tokio::test]
    async fn dispatch_put_files_rejects_absolute_outside_root() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!(
            { "files" : [{ "path" : "/etc/passwd", "content" : "evil" }] }
        );
        let result = handler
            .dispatch("workspace.put_files", params, None)
            .await
            .expect("dispatch itself should succeed");
        let res: PutFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(
            !res.results[0].ok,
            "absolute path outside root should be rejected"
        );
        assert!(
            res.results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("escapes workspace root"),
            "error should mention escape: {:?}",
            res.results[0].error
        );
    }
    #[tokio::test]
    async fn dispatch_put_files_accepts_absolute_within_root() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let abs = root.join("sub/abs.txt");
        let params = serde_json::json!(
            { "files" : [{ "path" : abs.to_str().expect("utf-8 path"), "content" :
            "hello" }] }
        );
        let result = handler
            .dispatch("workspace.put_files", params, None)
            .await
            .expect("dispatch itself should succeed");
        let res: PutFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(
            res.results[0].ok,
            "absolute path within root should be accepted: {:?}",
            res.results[0].error
        );
        assert_eq!(
            std::fs::read_to_string(root.join("sub/abs.txt")).unwrap(),
            "hello"
        );
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn dispatch_put_files_rejects_symlink_escape() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let outside = tempfile::tempdir().expect("create outside dir");
        std::os::unix::fs::symlink(outside.path(), root.join("escape_link"))
            .expect("create symlink");
        let params = serde_json::json!(
            { "files" : [{ "path" : "escape_link/evil.txt", "content" : "pwned" }] }
        );
        let result = handler
            .dispatch("workspace.put_files", params, None)
            .await
            .expect("dispatch itself should succeed");
        let res: PutFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(!res.results[0].ok, "symlink escape should be rejected");
        assert!(
            res.results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("symlink escape"),
            "error should mention symlink: {:?}",
            res.results[0].error
        );
        assert!(
            !outside.path().join("evil.txt").exists(),
            "file must not be created outside workspace"
        );
    }
    #[tokio::test]
    async fn dispatch_put_files_partial_failure() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!(
            { "files" : [{ "path" : "good.txt", "content" : "valid content" }, { "path" :
            "../bad.txt", "content" : "should fail" },] }
        );
        let result = handler
            .dispatch("workspace.put_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: PutFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 2);
        assert!(res.results[0].ok, "first file should succeed");
        assert!(res.results[0].hash.is_some(), "first file should have hash");
        assert!(!res.results[1].ok, "second file should fail");
        assert!(
            res.results[1].error.is_some(),
            "second file should have error"
        );
        assert!(
            res.results[1].hash.is_none(),
            "failed file should have no hash"
        );
        let on_disk = std::fs::read_to_string(root.join("good.txt")).unwrap();
        assert_eq!(on_disk, "valid content");
    }
    #[tokio::test]
    async fn dispatch_get_files_reads_existing_file() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let content = "read me back";
        std::fs::write(root.join("readable.txt"), content).unwrap();
        let params = serde_json::json!({ "files" : [{ "path" : "readable.txt" }] });
        let result = handler
            .dispatch("workspace.get_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: GetFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].exists, "file should exist");
        assert_eq!(
            res.results[0].content.as_deref(),
            Some(content),
            "content should match what was written"
        );
        let expected_hash = test_sha256(content.as_bytes());
        assert_eq!(
            res.results[0].hash.as_deref(),
            Some(expected_hash.as_str()),
            "hash should be SHA-256 of file content"
        );
        assert!(!res.results[0].matched);
        assert_eq!(
            res.results[0].size,
            Some(content.len() as u64),
            "size should match content length"
        );
        assert!(res.results[0].error.is_none());
    }
    #[tokio::test]
    async fn dispatch_get_files_nonexistent_returns_not_exists() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let params = serde_json::json!(
            { "files" : [{ "path" : "does_not_exist.txt" }] }
        );
        let result = handler
            .dispatch("workspace.get_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: GetFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(!res.results[0].exists, "file should not exist");
        assert!(res.results[0].content.is_none());
        assert!(res.results[0].hash.is_none());
        assert!(!res.results[0].matched);
        assert!(
            res.results[0].error.is_none(),
            "missing file is not an error"
        );
    }
    #[tokio::test]
    async fn dispatch_get_files_io_error_returns_exists_true() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        std::fs::create_dir_all(root.join("a_directory")).unwrap();
        let params = serde_json::json!({ "files" : [{ "path" : "a_directory" }] });
        let result = handler
            .dispatch("workspace.get_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: GetFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].exists, "directory exists on disk");
        assert!(
            res.results[0].error.is_some(),
            "reading a directory as file should fail: {:?}",
            res.results[0]
        );
        assert!(res.results[0].content.is_none(), "no content on error");
    }
    #[tokio::test]
    async fn dispatch_get_files_non_utf8_returns_error_with_hash() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let binary_content: &[u8] = b"\xff\xfe\x00\x01";
        std::fs::write(root.join("binary.bin"), binary_content).unwrap();
        let params = serde_json::json!({ "files" : [{ "path" : "binary.bin" }] });
        let result = handler
            .dispatch("workspace.get_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: GetFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].exists, "file should exist");
        assert!(
            res.results[0].content.is_none(),
            "non-UTF-8 content should be None"
        );
        let expected_hash = test_sha256(binary_content);
        assert_eq!(
            res.results[0].hash.as_deref(),
            Some(expected_hash.as_str()),
            "hash should be SHA-256 of file content even for non-UTF-8 files"
        );
        assert!(
            res.results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("not valid UTF-8"),
            "error should mention UTF-8: {:?}",
            res.results[0].error
        );
        assert_eq!(
            res.results[0].size,
            Some(4),
            "size should still be reported"
        );
    }
    #[tokio::test]
    async fn dispatch_get_files_cache_hit() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let content = "cacheable content";
        std::fs::write(root.join("cached.txt"), content).unwrap();
        let expected_hash = test_sha256(content.as_bytes());
        let params = serde_json::json!(
            { "files" : [{ "path" : "cached.txt", "if_none_match" : expected_hash }] }
        );
        let result = handler
            .dispatch("workspace.get_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: GetFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].exists);
        assert!(res.results[0].matched, "should be a cache hit");
        assert!(
            res.results[0].content.is_none(),
            "content should be omitted on cache hit"
        );
        assert_eq!(
            res.results[0].hash.as_deref(),
            Some(expected_hash.as_str()),
            "hash should still be returned"
        );
        assert!(res.results[0].error.is_none());
    }
    #[tokio::test]
    async fn dispatch_get_files_cache_miss() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let content = "fresh content";
        std::fs::write(root.join("stale.txt"), content).unwrap();
        let params = serde_json::json!(
            { "files" : [{ "path" : "stale.txt", "if_none_match" :
            "0000000000000000000000000000000000000000000000000000000000000000" }] }
        );
        let result = handler
            .dispatch("workspace.get_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: GetFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].exists);
        assert!(!res.results[0].matched, "should be a cache miss");
        assert_eq!(
            res.results[0].content.as_deref(),
            Some(content),
            "content should be returned on miss"
        );
        let expected_hash = test_sha256(content.as_bytes());
        assert_eq!(
            res.results[0].hash.as_deref(),
            Some(expected_hash.as_str()),
            "current hash should be returned"
        );
    }
    #[tokio::test]
    async fn dispatch_put_then_get_round_trip() {
        let handle = make_handle();
        let handler = WorkspaceRpcHandler::new(handle);
        let content = "round trip content";
        let put_params = serde_json::json!(
            { "files" : [{ "path" : "round_trip.txt", "content" : content }] }
        );
        let put_result = handler
            .dispatch("workspace.put_files", put_params, None)
            .await
            .expect("put should succeed");
        let put_res: PutFilesRes = serde_json::from_value(put_result).unwrap();
        assert!(put_res.results[0].ok);
        let put_hash = put_res.results[0].hash.clone().unwrap();
        let get_params = serde_json::json!(
            { "files" : [{ "path" : "round_trip.txt" }] }
        );
        let get_result = handler
            .dispatch("workspace.get_files", get_params, None)
            .await
            .expect("get should succeed");
        let get_res: GetFilesRes = serde_json::from_value(get_result).unwrap();
        assert!(get_res.results[0].exists);
        assert_eq!(
            get_res.results[0].content.as_deref(),
            Some(content),
            "content should match what was written"
        );
        assert_eq!(
            get_res.results[0].hash.as_deref(),
            Some(put_hash.as_str()),
            "get hash should match put hash"
        );
    }
    #[tokio::test]
    async fn dispatch_put_files_append_mode() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let params1 = serde_json::json!(
            { "files" : [{ "path" : "chunked.txt", "content" : "hello", "append" : false
            }] }
        );
        let res1 = handler
            .dispatch("workspace.put_files", params1, None)
            .await
            .expect("first chunk should succeed");
        let put1: PutFilesRes = serde_json::from_value(res1).unwrap();
        assert!(put1.results[0].ok);
        let chunk1_hash = put1.results[0].hash.clone().unwrap();
        assert_eq!(
            chunk1_hash,
            test_sha256(b"hello"),
            "hash should be of the appended chunk, not full file"
        );
        let params2 = serde_json::json!(
            { "files" : [{ "path" : "chunked.txt", "content" : " world", "append" : true
            }] }
        );
        let res2 = handler
            .dispatch("workspace.put_files", params2, None)
            .await
            .expect("second chunk should succeed");
        let put2: PutFilesRes = serde_json::from_value(res2).unwrap();
        assert!(put2.results[0].ok);
        let chunk2_hash = put2.results[0].hash.clone().unwrap();
        assert_eq!(
            chunk2_hash,
            test_sha256(b" world"),
            "hash should be of the appended chunk only"
        );
        let on_disk = std::fs::read_to_string(root.join("chunked.txt")).unwrap();
        assert_eq!(on_disk, "hello world");
    }
    #[tokio::test]
    async fn dispatch_get_files_byte_range() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let content = "0123456789";
        std::fs::write(root.join("range.txt"), content).unwrap();
        let params = serde_json::json!(
            { "files" : [{ "path" : "range.txt", "offset" : 3, "length" : 4 }] }
        );
        let result = handler
            .dispatch("workspace.get_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: GetFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].exists);
        assert_eq!(
            res.results[0].content.as_deref(),
            Some("3456"),
            "should return only the requested byte range"
        );
        let full_hash = test_sha256(content.as_bytes());
        assert_eq!(
            res.results[0].hash.as_deref(),
            Some(full_hash.as_str()),
            "hash should be of the full file, not the chunk"
        );
        assert!(!res.results[0].matched);
        assert_eq!(
            res.results[0].size,
            Some(content.len() as u64),
            "size should be full file size"
        );
    }
    #[tokio::test]
    async fn dispatch_get_files_byte_range_cache_hit() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let handler = WorkspaceRpcHandler::new(handle);
        let content = "abcdefghij";
        std::fs::write(root.join("range_cache.txt"), content).unwrap();
        let full_hash = test_sha256(content.as_bytes());
        let params = serde_json::json!(
            { "files" : [{ "path" : "range_cache.txt", "offset" : 2, "length" : 3,
            "if_none_match" : full_hash, }] }
        );
        let result = handler
            .dispatch("workspace.get_files", params, None)
            .await
            .expect("dispatch should succeed");
        let res: GetFilesRes = serde_json::from_value(result).unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].exists);
        assert!(res.results[0].matched, "should be a cache hit");
        assert!(
            res.results[0].content.is_none(),
            "content should be omitted on cache hit"
        );
        assert_eq!(
            res.results[0].hash.as_deref(),
            Some(full_hash.as_str()),
            "hash should still be returned"
        );
        assert_eq!(res.results[0].size, Some(10));
    }
    /// Every type with a `WorkspaceRpc` impl must be routed by `dispatch()`.
    ///
    /// Each entry is compiler-checked via `<X as WorkspaceRpc>::METHOD`.
    /// Dispatching `{}` may fail with any per-method error (invalid params,
    /// session not found, not a git repo) — only an "unknown workspace
    /// method" error fails the test.
    #[tokio::test]
    async fn dispatch_knows_every_typed_method() {
        use crate::file_system::{
            ContentSearchRequest, FsDeleteFileReq, FsExistsReq, FsListReq, FsReadFileReq,
            FsWriteFileReq,
        };
        use crate::workspace_ops::*;
        use crate::worktree::{ApplyWorktreeRequest, CreateWorktreeRequest, RemoveWorktreeRequest};
        use xai_grok_workspace_types::rpc::git::{GitBranchInfoReq, GitMetadataReq};
        use xai_grok_workspace_types::rpc::search::FuzzyStatusReq;
        use xai_grok_workspace_types::rpc::skills::DiscoverPluginsReq;
        use xai_grok_workspace_types::rpc::workspace::{
            ConfigureMcpReq, DropSessionReq, InstallPluginReq, LoadEnvrcReq, LoadPermissionsReq,
            LoadProjectConfigReq, RefreshPluginsReq, ResolveFileReferencesReq, ToolDefinitionsReq,
            UpdateToolConfigReq,
        };
        use xai_grok_workspace_types::rpc::worktree::WorktreeCreateSyncReq;
        let handler = WorkspaceRpcHandler::new(make_handle());
        let methods = [
            <WorkspaceInfoReq as WorkspaceRpc>::METHOD,
            <GitStatusReq as WorkspaceRpc>::METHOD,
            <DiscoverSkillsReq as WorkspaceRpc>::METHOD,
            <DiscoverAgentsMdReq as WorkspaceRpc>::METHOD,
            <GitStatusExtReq as WorkspaceRpc>::METHOD,
            <GitFilesReq as WorkspaceRpc>::METHOD,
            <GitDiffReq as WorkspaceRpc>::METHOD,
            <GitStageReq as WorkspaceRpc>::METHOD,
            <GitStageContentReq as WorkspaceRpc>::METHOD,
            <GitUnstageReq as WorkspaceRpc>::METHOD,
            <GitDiscardReq as WorkspaceRpc>::METHOD,
            <GitCommitReq as WorkspaceRpc>::METHOD,
            <GitCheckoutReq as WorkspaceRpc>::METHOD,
            <GitStashReq as WorkspaceRpc>::METHOD,
            <GitInfoReq as WorkspaceRpc>::METHOD,
            <GitBranchesReq as WorkspaceRpc>::METHOD,
            <GitResolveRootReq as WorkspaceRpc>::METHOD,
            <GitCurrentCommitReq as WorkspaceRpc>::METHOD,
            <DetectVcsKindReq as WorkspaceRpc>::METHOD,
            <GitCheckoutCommitReq as WorkspaceRpc>::METHOD,
            <GitBranchInfoReq as WorkspaceRpc>::METHOD,
            <GitMetadataReq as WorkspaceRpc>::METHOD,
            <PutFilesReq as WorkspaceRpc>::METHOD,
            <GetFilesReq as WorkspaceRpc>::METHOD,
            <FsListReq as WorkspaceRpc>::METHOD,
            <FsExistsReq as WorkspaceRpc>::METHOD,
            <FsReadFileReq as WorkspaceRpc>::METHOD,
            <FsWriteFileReq as WorkspaceRpc>::METHOD,
            <FsDeleteFileReq as WorkspaceRpc>::METHOD,
            <HunkSingleActionReq as WorkspaceRpc>::METHOD,
            <HunkFileActionReq as WorkspaceRpc>::METHOD,
            <HunkTurnActionReq as WorkspaceRpc>::METHOD,
            <HunkAllActionReq as WorkspaceRpc>::METHOD,
            <HunkGetAllFileContentsReq as WorkspaceRpc>::METHOD,
            <HunkGetSessionSummaryReq as WorkspaceRpc>::METHOD,
            <HunkGetAllHunksReq as WorkspaceRpc>::METHOD,
            <HunkGetStagedFilesReq as WorkspaceRpc>::METHOD,
            <HunkGetFilteredHunksReq as WorkspaceRpc>::METHOD,
            <HunkGetFileSummariesReq as WorkspaceRpc>::METHOD,
            <CodeGotoDefinitionReq as WorkspaceRpc>::METHOD,
            <CodeGotoReferencesReq as WorkspaceRpc>::METHOD,
            <CodeFindDefinitionsReq as WorkspaceRpc>::METHOD,
            <CodeFindReferencesReq as WorkspaceRpc>::METHOD,
            <CodeIndexStatusReq as WorkspaceRpc>::METHOD,
            <ContentSearchRequest as WorkspaceRpc>::METHOD,
            <FuzzyOpenReq as WorkspaceRpc>::METHOD,
            <FuzzyChangeReq as WorkspaceRpc>::METHOD,
            <FuzzyCloseReq as WorkspaceRpc>::METHOD,
            <FuzzyStatusReq as WorkspaceRpc>::METHOD,
            <CreateWorktreeRequest as WorkspaceRpc>::METHOD,
            <WorktreeCreateSyncReq as WorkspaceRpc>::METHOD,
            <RemoveWorktreeRequest as WorkspaceRpc>::METHOD,
            <ApplyWorktreeRequest as WorkspaceRpc>::METHOD,
            <WorktreeListReq as WorkspaceRpc>::METHOD,
            <WorktreeShowReq as WorkspaceRpc>::METHOD,
            <WorktreeDbPathReq as WorkspaceRpc>::METHOD,
            <WorktreeDbStatsReq as WorkspaceRpc>::METHOD,
            <PrepareWorktreeFromWorktreeReq as WorkspaceRpc>::METHOD,
            <CreateWorktreeFromWorktreeSyncReq as WorkspaceRpc>::METHOD,
            <BeginPromptReq as WorkspaceRpc>::METHOD,
            <EndPromptReq as WorkspaceRpc>::METHOD,
            <GetRewindPointsReq as WorkspaceRpc>::METHOD,
            <RewindToReq as WorkspaceRpc>::METHOD,
            <HookRegistryReq as WorkspaceRpc>::METHOD,
            <LoadProjectConfigReq as WorkspaceRpc>::METHOD,
            <LoadPermissionsReq as WorkspaceRpc>::METHOD,
            <LoadEnvrcReq as WorkspaceRpc>::METHOD,
            <ToolDefinitionsReq as WorkspaceRpc>::METHOD,
            <ResolveFileReferencesReq as WorkspaceRpc>::METHOD,
            <UpdateToolConfigReq as WorkspaceRpc>::METHOD,
            <DropSessionReq as WorkspaceRpc>::METHOD,
            <ConfigureMcpReq as WorkspaceRpc>::METHOD,
            <InstallPluginReq as WorkspaceRpc>::METHOD,
            <RefreshPluginsReq as WorkspaceRpc>::METHOD,
            <DiscoverPluginsReq as WorkspaceRpc>::METHOD,
        ];
        let skipped_global_db_mutators = [
            <WorktreeGcReq as WorkspaceRpc>::METHOD,
            <WorktreeDbRebuildReq as WorkspaceRpc>::METHOD,
        ];
        assert_eq!(skipped_global_db_mutators.len(), 2);
        for method in methods {
            let result = handler.dispatch(method, serde_json::json!({}), None).await;
            if let Err(e) = &result {
                assert!(
                    !e.to_string().contains("unknown workspace method"),
                    "dispatch does not know {method}: {e}"
                );
            }
        }
    }
}
