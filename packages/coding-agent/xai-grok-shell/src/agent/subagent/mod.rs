//! Shell child runtime adapter and presentation.
//!
//! Lifecycle state and command scheduling live in the shared
//! `xai-grok-tools` coordinator actor. This module keeps shell-specific
//! child-session construction, ACP presentation, persistence, and trace work.
//! The parent-side lifecycle and presentation entry points live in `spawn.rs`.
//!
//! ## Design
//!
//! - `run_shell_child()` runs one shell child behind `ChildRunner`.
//! - Pending/active/completed, waiters, deadlines, and cancellation are actor-owned.
//! - Child sessions share the parent's hunk tracker, filesystem, terminal, and env
//!   so that edits, bash commands, and file reads go through the same backends.
#![deny(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
use crate::agent::config::{resolve_credentials, sampling_config_for_model};
use crate::agent::models::resolve_catalog_key;
use crate::extensions::notification::{SessionNotification, SessionUpdate};
use crate::session::{
    self, SessionCommand, SessionHandle, SessionThread,
    commands::PromptTurnResult as SubagentPromptTurnResult, fs_watch::FsWatchCapabilities,
    info::Info as SessionInfo,
};
use crate::terminal::AsyncTerminalRunner;
use crate::tools::ToolContext;
use crate::upload::trace::{
    GCS_SCHEMA_VERSION, PromptMetadata, TurnResultMetadata, local_sandbox_telemetry,
    upload_metadata, upload_session_state, upload_subagent_metadata, upload_turn_result,
};
use crate::upload::turn::{PromptTraceContext, complete_prompt_trace};
use agent_client_protocol as acp;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_agent::config::{McpInheritance, ModelOverride, PermissionMode};
use xai_grok_sampling_types::conversation::ConversationItem;
use xai_grok_session_events::types::CancellationCategory;
use xai_grok_subagent_resolution::ResumeSourceData;
use xai_grok_tools::implementations::grok_build::monitor::types::MonitorEventBuffer;
use xai_grok_tools::implementations::grok_build::task::types::*;
use xai_grok_tools::types::tool::ToolKind;
use xai_grok_workspace::file_system::AsyncFileSystem;
use xai_hunk_tracker::HunkTrackerHandle;
mod attempt_runner;
mod spawn;
pub(crate) use spawn::{
    ChildControl, ChildRunOutput, LocalBoxFuture, StartedChild, SubagentProgress,
    emit_subagent_notification, spawn_subagent_coordinator, worker_runtime,
};
mod attempt_store;
mod handle_request;
pub(crate) use handle_request::run_shell_child;
/// Clamp for a resolved sampling-limit override; `Semaphore::new` panics past
/// `Semaphore::MAX_PERMITS`. See [`SubagentsConfig::resolve_sampling_limit`].
///
/// [`SubagentsConfig::resolve_sampling_limit`]: crate::config::SubagentsConfig::resolve_sampling_limit
pub(crate) const MAX_SUBAGENT_SAMPLING_LIMIT: usize = 512;
/// How the child session's initial context was bootstrapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InitialContextSource {
    /// Fresh session — no inherited history.
    New,
    /// Parent history as `<background_context>` (harness-only chat-prefix fork).
    Forked,
    /// Resumed from a previously completed peer subagent. The child inherits
    /// the source's raw transcript, tool state, and model. System prompt and
    /// prompt context are freshly rendered from the current agent definition.
    Resumed,
}
/// Captured parent-side tier inputs for resolving
/// `auto_compact_threshold_percent` once the subagent's actual model id is
/// known. Stored on [`SubagentSpawnContext`] so the resolver can run at
/// spawn time and the per-model lookup honors the SUBAGENT's model rather
/// than the parent's.
#[derive(Debug, Clone, Default)]
pub(crate) struct AutoCompactThresholdTiers {
    /// `cfg.session.auto_compact_threshold_percent` (user global TOML).
    pub user_session: Option<u8>,
    /// Subset of `cfg.config_models` whose `auto_compact_threshold_percent`
    /// is set, keyed by the model entry's id (the table key in
    /// `[model.<id>]`). Looked up by the subagent's resolved model id at
    /// spawn time so user per-model overrides for the subagent's model are
    /// honored (not just the parent's).
    pub user_per_model: std::collections::HashMap<String, u8>,
    /// `cfg.remote_settings.auto_compact_threshold_percent` (GB global).
    pub remote_global: Option<u8>,
}
impl AutoCompactThresholdTiers {
    /// Slice the parent's `Config` into the four tier inputs we'll resolve
    /// against later. Only fields relevant to the auto-compact threshold
    /// are captured; the parent's `Config` is not held by reference.
    pub(crate) fn capture(cfg: &crate::agent::config::Config) -> Self {
        let user_per_model = cfg
            .config_models
            .iter()
            .filter_map(|(k, v)| v.auto_compact_threshold_percent.map(|t| (k.clone(), t)))
            .collect();
        Self {
            user_session: cfg.session.auto_compact_threshold_percent,
            user_per_model,
            remote_global: cfg
                .remote_settings
                .as_ref()
                .and_then(|r| r.auto_compact_threshold_percent),
        }
    }
}
/// Everything the coordinator needs from MvpAgent to spawn a child session.
/// Avoids passing `&MvpAgent` (which would require the coordinator to know
/// about the full agent struct). Built by `MvpAgent::build_subagent_spawn_context()`.
pub(crate) struct SubagentSpawnContext {
    /// Parent's LSP runtime — inherited via ToolContext, same as fs/terminal.
    pub lsp: Option<std::sync::Arc<dyn xai_grok_tools::implementations::lsp::LspBackend>>,
    /// Root session's process scope, inherited so the subagent's own child
    /// processes are reaped when the parent session closes. It is the root's
    /// (not an intermediate parent's) because xai-grok-tools task/coordinator.rs
    /// `handle_command`'s Spawn arm re-parents nested Spawn requests to the root
    /// parent, so every subagent resolves back to the root session.
    pub process_scope: Option<xai_tty_utils::ProcessScope>,
    /// Parent's client-registered hooks, inherited so the subagent's tool calls hit the
    /// same PreToolUse gate and its events fire the same observe hooks over the parent's
    /// connection. Empty when the parent has none. Filled by the coordinator after the
    /// context is built (an async snapshot from the parent session actor).
    pub client_hooks: crate::extensions::hooks::ClientHooks,
    pub sampling_config: xai_grok_sampler::SamplerConfig,
    pub managed_mcp_proxy_base_url: String,
    /// The staging auth header value propagated from the parent. Used
    /// when materialising subagent `SamplerConfig`s for auth-flow tracking
    /// and for `inject_url_derived_headers` in the construction helpers.
    pub alpha_test_key: Option<String>,
    pub auth_method_id: acp::AuthMethodId,
    pub model_id: acp::ModelId,
    pub auth: Option<crate::auth::GrokAuth>,
    pub parent_cwd: PathBuf,
    pub parent_session_id: String,
    /// The parent's cutoff at spawn, applied to the child's first turn. `None` if unset.
    pub inherited_tool_overrides: Option<xai_grok_sampling_types::ToolOverrides>,
    pub yolo_mode: bool,
    pub subagent_event_tx: mpsc::UnboundedSender<SubagentEvent>,
    pub parent_depth: u32,
    pub subagents_max_depth: u32,
    pub workflow_max_concurrent_agents: usize,
    pub media_gen_batch_limits: xai_grok_tools::media_gen_limits::MediaGenBatchLimits,
    /// Inference idle timeout (secs), resolved from the parent's model config at spawn-context creation time.
    pub inference_idle_timeout_secs: u64,
    /// Tier inputs for resolving `auto_compact_threshold_percent` at
    /// spawn time — once the subagent's actual model id is known.
    /// Lazy because the subagent may be assigned a different model from
    /// the parent (via `[subagents.models]` or `AgentDefinition.model`);
    /// we want the resolver's per-model
    /// tiers to be looked up against the SUBAGENT's model, not the
    /// parent's. Call [`Self::resolve_auto_compact_threshold_percent`]
    /// once the subagent's `effective_sampling_config.model` is known.
    pub auto_compact_threshold_tiers: AutoCompactThresholdTiers,
    /// Parent's hunk tracker handle — cheap Clone, backed by an mpsc channel
    /// to the parent's HunkTrackerActor. Subagent edits are attributed to
    /// the same hunk tracker so the parent sees all file changes.
    pub hunk_tracker_handle: HunkTrackerHandle,
    /// Parent's hunk-tracking gate, inherited so a disabled parent's subagent
    /// also skips the per-event forward instead of paying it into a noop handle.
    pub hunk_tracking_enabled: bool,
    /// Parent's filesystem implementation (LocalFs or AcpSessionFs).
    /// Shared so the child reads/writes the same working tree.
    pub fs: Arc<dyn AsyncFileSystem>,
    /// Parent's terminal runner — shared so bash commands run in the
    /// same terminal environment (env vars, cwd, color settings).
    pub terminal: Arc<dyn AsyncTerminalRunner>,
    /// Parent's terminal backend — shared so background tasks, monitors, and
    /// scheduled tasks survive subagent exit. When `Some`, the subagent session
    /// reuses this backend instead of creating a new `LocalTerminalBackend`.
    pub parent_terminal_backend: Option<Arc<dyn xai_grok_tools::computer::types::TerminalBackend>>,
    /// Parent's notification handle for reparenting on subagent exit.
    /// When a subagent exits, its surviving tasks (monitors, bg commands)
    /// need their notification handles swapped to this so events route
    /// to the parent's notification bridge.
    pub parent_notification_handle:
        Option<xai_grok_tools::notification::types::ToolNotificationHandle>,
    /// Parent's scheduler handle. When `Some`, the subagent reuses the
    /// parent's scheduler actor so scheduled tasks survive subagent exit.
    pub parent_scheduler_handle:
        Option<xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerHandle>,
    /// Parent's session environment variables (.envrc + color settings).
    /// Shared so the child inherits the same env without re-loading.
    pub session_env: Arc<HashMap<String, String>>,
    /// Parent's memory config — shared so the child can access the same
    /// cross-session memory store.
    pub memory_config: Option<crate::config::MemoryConfig>,
    /// Resolved sampling config for web_search.
    pub web_search_sampling_config: Option<xai_grok_sampler::SamplerConfig>,
    /// Resolved config for web fetch.
    pub web_fetch_config: xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig,
    /// Image generation config (parent-inherited).
    pub image_gen_config: xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig,
    /// Resolved config for video generation.
    pub video_gen_config: xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig,
    /// Resolved config for the deploy service.
    pub app_builder_deployer_config:
        xai_grok_tools::implementations::grok_build::app_builder::AppBuilderDeployerConfig,
    /// Whether the write_file tool is enabled.
    pub write_file_enabled: bool,
    /// Whether goal mode (`/goal`) is enabled.
    pub goal_enabled: bool,
    pub background_workflows_enabled: bool,
    /// Child policy for exposing `ask_user_question`. Always false; the
    /// parent session's setting must not cross the subagent boundary.
    pub ask_user_question_enabled: bool,
    /// Whether the parent session is non-interactive (headless `-p` / SDK),
    /// copied onto the child's `StartupHints` so its prompt omits interactive
    /// guidance.
    pub parent_non_interactive: bool,
    /// Parent session command channel. Carries lifecycle notifications the
    /// parent persists (`SubagentSpawned` / `SubagentFinished`) and — when
    /// goal mode is on — transient `SubagentProgress` ticks the parent
    /// consumes for token accounting without persisting.
    pub parent_cmd_tx: Option<mpsc::UnboundedSender<SessionCommand>>,
    /// Root-parent peer bus for `agent_hub` (shared Arc with Main + siblings).
    pub agent_bus: Option<xai_grok_tools::implementations::grok_build::AgentBusResource>,
    /// Parent session info — used to locate parent session directory.
    pub parent_session_info: Option<SessionInfo>,
    /// Subagent roles config for role-based config layering.
    pub subagent_roles:
        std::collections::HashMap<String, xai_grok_subagent_resolution::config::SubagentRole>,
    /// Subagent personas config for persona/SOUL layering.
    pub subagent_personas:
        std::collections::HashMap<String, xai_grok_subagent_resolution::config::SubagentPersona>,
    /// Parent session's ChatStateHandle — used to read the actual live
    /// sampling config and credentials from the parent session actor (async).
    /// Cheap Clone (mpsc sender). `None` when parent SessionHandle not found.
    pub parent_chat_state: Option<xai_chat_state::ChatStateHandle>,
    /// Parent session's resolved turn limit, for subagent inheritance.
    pub parent_max_turns: Option<usize>,
    /// All available models for resolving model IDs from overrides.
    pub available_models: indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
    /// Per-subagent model ID overrides from config.toml `[subagents.models]`.
    pub subagent_model_overrides: std::collections::HashMap<String, String>,
    /// Per-agent reasoning effort pins from `[subagents.effort]` (empty when
    /// no pin table is configured).
    pub subagent_effort_overrides: std::collections::HashMap<String, String>,
    /// Per-subagent enable/disable toggles from config.toml `[subagents.toggle]`.
    /// Omitted agents default to enabled (`true`).
    pub subagent_toggle: std::collections::HashMap<String, bool>,
    /// Whether web search is force-disabled via `--disable-web-search`.
    /// Inherited from the parent session.
    pub disable_web_search: bool,
    /// Whether the runtime turn-end TodoGate is force-enabled via
    /// `--todo-gate`. Inherited from the parent session.
    pub todo_gate: bool,
    /// Remote settings snapshot from the parent session. Used to resolve
    /// `ReminderPolicy.todo_gate` (CLI > remote > default) for the subagent.
    pub remote_settings: Option<crate::util::config::RemoteSettings>,
    /// Inherited `--laziness-debug-log <path>` from the parent session.
    /// Subagent classifier fires append to the same log file. `None`
    /// when the parent did not enable debug mode.
    pub laziness_debug_log: Option<std::path::PathBuf>,
    pub backend_tools_enabled: bool,
    /// Whether tools should respect `.gitignore` patterns.
    /// Inherited from the parent session.
    pub respect_gitignore: bool,
    /// Whether to enrich path-not-found errors with hints.
    /// Inherited from the parent session.
    pub path_not_found_hints: bool,
    /// Plugin registry for plugin-aware agent lookup.
    pub plugin_registry: Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>,
    /// Shared models manager for etag-triggered refresh.
    pub models_manager: crate::agent::models::ModelsManager,
    /// Pre-resolved file tool overrides (hashline vs standard) from the parent.
    /// `None` means use the standard (default) file tools.
    pub file_tool_overrides: Option<Vec<xai_grok_tools::registry::types::ToolConfig>>,
    /// Parent session's agent config snapshot.
    pub agent_config: Option<crate::agent::config::Config>,
    /// GCS bucket URL for trace uploads.
    /// For proxy upload mode this is a placeholder — the actual bucket
    /// is determined by the proxy from user ACLs.
    pub gcs_bucket_url: Option<String>,
    /// GCS upload method (direct or proxy).
    pub gcs_upload_method: Option<crate::session::repo_changes::UploadMethod>,
    pub hook_registry: Option<std::sync::Arc<xai_grok_hooks::discovery::HookRegistry>>,
    pub permission_handle: Option<xai_grok_workspace::permission::PermissionHandle>,
    pub worktree_type: crate::util::config::WorktreeType,
    pub api_key_provider: Option<xai_grok_tools::types::SharedApiKeyProvider>,
    pub image_description_model: String,
    /// Dual-mode workspace operations handle.
    pub workspace_ops: xai_grok_workspace::WorkspaceOps,
    pub auth_manager: std::sync::Arc<crate::auth::AuthManager>,
    /// The parent SessionActor's live
    /// `Auth401AttributionCallback`, captured at spawn time.
    /// Subagents inherit this so the child's `OaiCompatClient` 401
    /// sites emit attribution under the parent's session id, joined
    /// with the parent's live `AuthManager`.
    ///
    /// Note: this is the load-bearing source of the inherited
    /// callback. Reading from `ctx.sampling_config.attribution_callback`
    /// would not work because the baseline `MvpAgent.sampling_config`
    /// goes through `agent/config.rs::sampling_config_for_model`
    /// which always sets that field to `None`.
    pub attribution_callback: Option<xai_grok_sampler::SharedAttributionCallback>,
    /// Parent session's agent name (e.g. "grok-build").
    pub parent_agent_name: Option<String>,
    /// `agent_type` of the parent's current model — the harness-flavor fallback
    /// when `parent_agent_name` is not a recognized harness, e.g. a custom
    /// client profile keeps its own name but runs a strict-harness model.
    /// `None` when the model is not in the catalog.
    pub parent_model_agent_type: Option<String>,
    pub allowed_subagent_types: Option<Vec<String>>,
    /// Parent's MCP server configs for resolving named references in agent mcpServers.
    ///
    /// NOTE: This is a snapshot from `SessionHandle` (populated at spawn_session_actor
    /// time). Servers added later via `UpdateMcpServers` (managed MCPs, plugin reload)
    /// will not appear here. Named references only resolve against the initial config.
    pub parent_mcp_configs: Vec<agent_client_protocol::McpServer>,
    /// Parent's managed MCP state handle (Arc-shared, no re-fetch).
    pub managed_mcp_state: crate::session::managed_mcp::ManagedMcpStateHandle,
    /// Snapshot of the parent session's MCP client pool at spawn time.
    pub parent_mcp_pool: Option<crate::session::mcp_servers::SharedMcpPool>,
    /// Exact parent tool schema for verbatim non-workflow forks.
    pub parent_tool_definitions: Option<Vec<xai_grok_sampling_types::ToolSpec>>,
    /// Pre-discovered skills from the parent session, captured at spawn time.
    pub parent_skills: Option<Vec<xai_grok_tools::implementations::skills::types::SkillInfo>>,
    /// Parent's skills config for the child's SkillManager.
    pub parent_skills_config: xai_grok_agent::prompt::skills::SkillsConfig,
    /// Parent's resolved vendor-compat config, inherited by the child so its
    /// skills / rules / AGENTS.md discovery honors the same vendor toggles.
    pub parent_compat: xai_grok_tools::types::compat::CompatConfig,
    /// Shared completion reservations held by auto-wake prompts.
    pub task_completion_reservations:
        Option<xai_grok_tools::reminders::task_completion::TaskCompletionReservations>,
    /// Channel for requesting trace uploads for synthetic auto-wake turns.
    pub synthetic_trace_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>>,
    /// Resolved name of the `BackgroundTaskAction` tool in the parent's toolset.
    pub task_output_tool_name: String,
    /// Whether auto-wake is enabled. When `false`, subagent completions
    /// are not injected as synthetic prompts.
    pub auto_wake_enabled: bool,
    /// Parent's live goal-loop gate (shared `Arc`). When set, the subagent
    /// auto-wake synthetic prompt is suppressed so an async completion wake
    /// doesn't derail the parent mid-`/goal`; surfaces 2/3 still drain it.
    pub goal_loop_active: Arc<std::sync::atomic::AtomicBool>,
    /// The process tree's shared subagent turn-sampling semaphore. See
    /// [`crate::config::SubagentsConfig::resolve_sampling_limit`].
    pub subagent_sampling_semaphore: Arc<tokio::sync::Semaphore>,
}
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<SubagentSpawnContext>()
};
pub(crate) fn strip_ask_user_question_tool(tools: &mut Vec<xai_grok_sampling_types::ToolSpec>) {
    tools.retain(|tool| tool.name != "ask_user_question");
}
impl SubagentSpawnContext {
    /// Would installing a live bearer resolver strip this subagent's only
    /// credential? A wired resolver is the sampler's sole auth source, so
    /// with no session key at spawn it must not displace a real fallback
    /// key (env `XAI_API_KEY`). Keyed on the resolved config key, not the
    /// session cache alone — the cache is empty in exactly the post-wake /
    /// mid-refresh states the resolver targets, and gating on it would
    /// freeze the subagent for life. Shared by all three resolver-wiring
    /// paths so they cannot drift.
    fn would_strip_fallback_key(&self, resolved_api_key: Option<&str>) -> bool {
        self.auth.is_none() && resolved_api_key.is_some()
    }
    /// Resolve `auto_compact_threshold_percent` for the subagent's actual
    /// model id (the one selected by `resolve_subagent_sampling_config`,
    /// not the parent's). Walks the same precedence as the main session's
    /// resolver: env > user [model.<id>] > user [session] > GB per-model
    /// > GB global > 85.
    ///
    /// The GB per-model tier is read from `available_models` (the same
    /// catalog used to pick the subagent's `SamplerConfig`); user TOML and
    /// GB global tiers are sourced from the parent's snapshot captured at
    /// spawn-context build time.
    pub(crate) fn resolve_auto_compact_threshold_percent(&self, subagent_model_id: &str) -> u8 {
        let gb_per_model =
            crate::agent::config::find_model_by_id(&self.available_models, subagent_model_id)
                .and_then(|e| e.info.auto_compact_threshold_percent);
        crate::util::config::resolve_auto_compact_threshold_percent_from_tiers(
            self.auto_compact_threshold_tiers
                .user_per_model
                .get(subagent_model_id)
                .copied(),
            self.auto_compact_threshold_tiers.user_session,
            gb_per_model,
            self.auto_compact_threshold_tiers.remote_global,
        )
    }
    /// Resolve the 429 wait-attempt budget against the subagent's own model id.
    pub(crate) fn resolve_subagent_rate_limit_max_attempts(&self, subagent_model_id: &str) -> u32 {
        let per_model =
            crate::agent::config::find_model_by_id(&self.available_models, subagent_model_id)
                .and_then(|e| e.info.subagent_rate_limit_max_attempts);
        let remote = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.subagent_rate_limit_max_attempts);
        crate::agent::mvp_agent::resolve_subagent_rate_limit_max_attempts(
            per_model,
            remote,
            crate::agent::mvp_agent::subagent_rate_limit_max_attempts_env(),
        )
    }
    /// Bind a spawned subagent by the parent session's `--tools`/
    /// `--disallowed-tools`/`--permission-mode` restrictions.
    fn apply_session_cli_overrides(&self, def: &mut xai_grok_agent::config::AgentDefinition) {
        if let Some(ref cfg) = self.agent_config {
            cfg.cli_agent_overrides.apply_to_subagent_definition(def);
        }
    }
    /// Not `Config::feature`: the parent's tiers resolve against the
    /// subagent's own remote settings snapshot.
    fn resolve_feature(&self, feature: crate::agent::config::Feature) -> bool {
        use crate::agent::config::FeatureSources;
        let mut sources = self.agent_config.as_ref().map_or_else(
            || FeatureSources::from_process_env(feature),
            |parent| parent.feature_sources(feature),
        );
        sources.remote = feature.remote_value(self.remote_settings.as_ref());
        feature.resolve(sources).value
    }
    pub(crate) fn resolve_compaction_verbatim_input(&self) -> bool {
        self.resolve_feature(crate::agent::config::Feature::CompactionVerbatimInput)
    }
    pub(crate) fn resolve_compaction_tool_choice(
        &self,
    ) -> crate::util::config::CompactionToolChoice {
        crate::util::config::resolve_compaction_tool_choice_from(
            crate::agent::config::env_string(crate::util::config::ENV_COMPACTION_TOOL_CHOICE)
                .as_deref(),
            self.agent_config
                .as_ref()
                .and_then(|c| c.features.compaction_tool_choice.as_deref()),
            self.remote_settings
                .as_ref()
                .and_then(|r| r.compaction_tool_choice.as_deref()),
        )
    }
    /// Whether a completed subagent's working copy is saved into the repo as a
    /// git ref and its directory deleted.
    pub(crate) fn resolve_subagent_worktree_snapshot_enabled(&self) -> bool {
        self.resolve_feature(crate::agent::config::Feature::SubagentWorktreeSnapshot)
    }
}
/// Shell runtime handle retained while a child is active.
pub(crate) struct ShellChildRuntime {
    pub child_handle: SessionHandle,
    pub _child_thread: SessionThread,
}
impl ChildControl for ShellChildRuntime {
    type ProgressFuture = LocalBoxFuture<SubagentProgress>;
    fn progress(&self) -> Self::ProgressFuture {
        let signals = self.child_handle.signals_handle.clone();
        Box::pin(async move {
            let snapshot = signals.snapshot().await.unwrap_or_default();
            SubagentProgress {
                turn_count: snapshot.turn_count,
                tool_call_count: snapshot.tool_call_count,
                tokens_used: snapshot.context_tokens_used,
                context_window_tokens: snapshot.context_window_tokens,
                context_usage_pct: snapshot.context_window_usage,
                tools_used: snapshot.tools_used,
                error_count: snapshot.error_count,
            }
        })
    }
    fn cancel(&self) {
        let _ =
            self.child_handle
                .cmd_tx
                .send(SessionCommand::Cancel(crate::session::CancelOptions {
                    cancel_subagents: true,
                    kill_background_tasks: true,
                    trigger: Some(crate::session::CancelTrigger::Shutdown),
                    ..Default::default()
                }));
        let _ = self.child_handle.cmd_tx.send(SessionCommand::Shutdown(
            crate::session::ShutdownKind::Graceful,
        ));
    }
}
#[derive(Default)]
pub(crate) struct ShellCompletionData {
    auto_wake_enabled: bool,
    task_completion_reservations:
        Option<xai_grok_tools::reminders::task_completion::TaskCompletionReservations>,
    parent_cmd_tx: Option<mpsc::UnboundedSender<SessionCommand>>,
    task_output_tool_name: String,
    synthetic_trace_tx:
        Option<mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>>,
    goal_loop_active: Arc<std::sync::atomic::AtomicBool>,
    telemetry_tokens: u64,
    spawned_notification_emitted: bool,
    persisted_output_dir: Option<PathBuf>,
}
impl ShellCompletionData {
    fn from_context(ctx: &SubagentSpawnContext) -> Self {
        Self {
            auto_wake_enabled: ctx.auto_wake_enabled,
            task_completion_reservations: ctx.task_completion_reservations.clone(),
            parent_cmd_tx: ctx.parent_cmd_tx.clone(),
            task_output_tool_name: ctx.task_output_tool_name.clone(),
            synthetic_trace_tx: ctx.synthetic_trace_tx.clone(),
            goal_loop_active: Arc::clone(&ctx.goal_loop_active),
            telemetry_tokens: 0,
            spawned_notification_emitted: false,
            persisted_output_dir: None,
        }
    }
    pub(crate) fn persisted_output_dir(&self) -> Option<&Path> {
        self.persisted_output_dir.as_deref()
    }
    fn set_persisted_output_dir(&mut self, path: Option<PathBuf>) {
        self.persisted_output_dir = path;
    }
}
pub(crate) struct SubagentPresentation {
    is_turn_active: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) synthetic_trace_tx:
        Option<mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>>,
}
impl SubagentPresentation {
    pub(crate) fn new() -> Self {
        Self {
            is_turn_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            synthetic_trace_tx: None,
        }
    }
    pub(crate) fn turn_active_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.is_turn_active)
    }
}
/// Resume provenance metadata for a subagent.
#[derive(Debug, Clone, Default)]
pub(crate) struct SubagentProvenance {
    pub(crate) fork_parent_prompt_id: Option<String>,
    /// ID of the source subagent this session was resumed from.
    pub(crate) resumed_from: Option<String>,
}

/// Resolve the sampling config and model ID for a subagent.
///
/// Subagents inherit the parent session's model by default. Only an
/// EXPLICIT per-agent pin can override that inheritance; there is no global
/// default model and no parent-model gate. Precedence (highest to lowest):
///
///   1. `config.toml [subagents.models].{agent_name}` override, if it
///      resolves to a known model. Applies unconditionally.
///
///   2. `AgentDefinition.model = Override(id)`, if it resolves to a known
///      model. Applies unconditionally.
///
///   3. Inherit the parent session's actual live sampling config (from
///      `ChatStateHandle`).
///
/// Both explicit pins apply regardless of which model the parent is on. If a
/// pin references an unknown model it is ignored (with a `tracing::warn!`)
/// and resolution falls through to the next priority.
///
/// NOTE: the persona/role/runtime override (`effective_runtime.model`) is
/// applied by the caller (`run_shell_child`) BEFORE this function
/// runs, so it is not handled here.
///
/// NOTE: `agent_type` and `use_concise` on the resolved model are
/// intentionally ignored. Subagent prompt/toolset is always determined by
/// the `AgentDefinition`, not the model. See design spec
/// "Behavioral Rules section 3".
async fn resolve_subagent_sampling_config(
    agent_name: &str,
    agent_model: &xai_grok_agent::config::ModelOverride,
    ctx: &SubagentSpawnContext,
) -> (xai_grok_sampler::SamplerConfig, acp::ModelId) {
    let (parent_config, parent_mid) = read_parent_sampling_config(ctx).await;
    let try_pin = |model_id: &str, source: &'static str, unknown_msg: &'static str| {
        match resolve_model_override_to_config(model_id, ctx) {
            Some((config, canonical_id)) => {
                log_subagent_model_resolution(
                    agent_name,
                    source,
                    &config,
                    &canonical_id,
                    &parent_config,
                );
                Some((config, canonical_id))
            }
            None => {
                tracing::warn!(agent = agent_name, model_id, "{unknown_msg}");
                None
            }
        }
    };
    if let Some(model_id) = ctx.subagent_model_overrides.get(agent_name)
        && let Some(resolved) = try_pin(
            model_id,
            "config_override",
            "Subagent model override references unknown model, falling through to inherit",
        )
    {
        return resolved;
    }
    if let ModelOverride::Override(model_id) = agent_model
        && let Some(resolved) = try_pin(
            model_id,
            "agent_definition",
            "Agent definition model references unknown model, falling through to inherit",
        )
    {
        return resolved;
    }
    log_subagent_model_resolution(
        agent_name,
        "inherit_parent",
        &parent_config,
        &parent_mid,
        &parent_config,
    );
    (parent_config, parent_mid)
}
/// Resolve a subagent's effective sampling config + model id, honoring the
/// model-resolution precedence (Key Decision #16).
///
/// An explicit `runtime_override_model` — the goal role model or a persona
/// override carried on `effective_runtime.model` — is resolved HERE, BEFORE
/// [`resolve_subagent_sampling_config`] (where the user `[subagents.models]`
/// pin and `AgentDefinition.model` apply). So a goal/persona override WINS
/// over a user per-agent pin. An override that does not resolve to a known
/// model warns and falls through to the pin path; `None` (inherit) hands
/// precedence back to the pin path entirely (pin > agent-def > inherit).
///
/// Extracted from `run_shell_child` so the precedence is unit-testable
/// without spawning a child session.
async fn resolve_effective_model_config(
    runtime_override_model: Option<&str>,
    subagent_type: &str,
    definition_model: &xai_grok_agent::config::ModelOverride,
    ctx: &SubagentSpawnContext,
) -> (xai_grok_sampler::SamplerConfig, acp::ModelId) {
    if let Some(model_id) = runtime_override_model {
        if let Some(resolved) = resolve_model_override_to_config(model_id, ctx) {
            return resolved;
        }
        tracing::warn!(
            model_id,
            "Runtime model override references unknown model, falling through"
        );
    }
    resolve_subagent_sampling_config(subagent_type, definition_model, ctx).await
}
/// Truncate an API key to a safe prefix for logging. Counts characters, not
/// bytes: a configured key with a multi-byte character would panic a byte
/// slice, and this only ever runs to build a log line.
fn key_prefix(key: &Option<String>) -> String {
    match key {
        Some(k) => k.chars().take(8).collect(),
        None => "<none>".to_string(),
    }
}
/// Emit a unified log entry recording which model and credentials a subagent
/// resolved to, and how they compare to the parent's.
fn log_subagent_model_resolution(
    agent_name: &str,
    priority: &str,
    resolved: &xai_grok_sampler::SamplerConfig,
    resolved_id: &acp::ModelId,
    parent: &xai_grok_sampler::SamplerConfig,
) {
    let child_key = key_prefix(&resolved.api_key);
    let parent_key = key_prefix(&parent.api_key);
    let keys_match = resolved.api_key == parent.api_key;
    xai_grok_telemetry::unified_log::debug(
        "subagent model resolved",
        None,
        Some(serde_json::json!({
            "agent": agent_name,
            "priority": priority,
            "child_model": resolved_id.0.as_ref(),
            "child_base_url": &resolved.base_url,
            "child_key_prefix": child_key,
            "parent_model": &parent.model,
            "parent_base_url": &parent.base_url,
            "parent_key_prefix": parent_key,
            "keys_match": keys_match,
        })),
    );
}
/// Session-token bearer resolver for a subagent config, over the parent's
/// `AuthManager` (wire-valid only). Without it the subagent runs forever on
/// the `api_key` frozen at spawn and 401s once the parent rotates the token.
/// Gated exactly like the parent session's resolver
/// (`auth_method::session_token_auth_gate`); all three subagent config paths
/// go through this.
fn session_bearer_resolver(
    ctx: &SubagentSpawnContext,
    byok: crate::agent::auth_method::ModelByok,
    base_url: &str,
) -> Option<xai_grok_sampler::SharedBearerResolver> {
    use crate::agent::auth_method;
    auth_method::session_token_auth_gate(
        auth_method::is_session_based_method(&ctx.auth_method_id),
        byok,
        crate::util::is_xai_api_url(base_url),
    )
    .then(|| {
        crate::auth::credential_provider::WireValidBearerResolver::shared(ctx.auth_manager.clone())
    })
}
/// [`session_bearer_resolver`] for an inherited config, where only the model
/// string is known: BYOK comes from the catalog memo.
fn inherited_bearer_resolver(
    ctx: &SubagentSpawnContext,
    model: &str,
    base_url: &str,
) -> Option<xai_grok_sampler::SharedBearerResolver> {
    let byok = crate::agent::config::resolve_model_auth_facts_and_provider(model)
        .0
        .byok;
    session_bearer_resolver(ctx, byok, base_url)
}
fn parent_catalog_model_id(ctx: &SubagentSpawnContext, routing_model: &str) -> acp::ModelId {
    let models = ctx.models_manager.models();
    resolve_catalog_key(&models, &ctx.model_id)
        .or_else(|| resolve_catalog_key(&models, &acp::ModelId::new(routing_model)))
        .unwrap_or_else(|| ctx.model_id.clone())
}
/// Read the parent session's actual current sampling config.
///
/// Prefers the live state from `ChatStateHandle` (authoritative). Falls back
/// to the baseline on `SubagentSpawnContext` if the actor is unavailable.
/// The returned [`acp::ModelId`] is the parent session catalog id (`ctx.model_id`),
/// not the process-global default or chat-state routing slug.
async fn read_parent_sampling_config(
    ctx: &SubagentSpawnContext,
) -> (xai_grok_sampler::SamplerConfig, acp::ModelId) {
    if let Some(ref chat_state) = ctx.parent_chat_state {
        if let Some(cfg) = chat_state.get_sampling_config().await {
            let creds = chat_state.get_credentials().await;
            // OAuth subagent sessions keep the live bearer resolver, headers,
            // and dialect of the parent's catalog platform (stamped api_key
            // alone goes stale). Never choose from origin alone: Kimi and
            // Codex may share a user-configured reverse proxy.
            let catalog = ctx.models_manager.models();
            let parent_model =
                crate::agent::config::find_model_by_id(&catalog, ctx.model_id.0.as_ref());
            let oauth_platform = match parent_model {
                Some(model) => crate::agent::config::oauth_platform_for_model(model),
                None => crate::agent::config::oauth_platform_for_base_url(&cfg.base_url),
            };
            let oauth_origin = xai_grok_models::PlatformId::KimiCode
                .base_url_matches(&cfg.base_url)
                || xai_grok_models::PlatformId::OpenAiCodex.base_url_matches(&cfg.base_url);
            let same_baseline_route = ctx.sampling_config.model == cfg.model
                && ctx.sampling_config.base_url.trim_end_matches('/')
                    == cfg.base_url.trim_end_matches('/');
            let (bearer_resolver, codex_oauth_active, kimi_dialect, kimi_oauth_active) =
                match parent_model {
                    Some(model) => {
                        // Hybrid Kimi: static own key + !platform_oauth_active is
                        // not OAuth — use the shared helper (do not reimplement).
                        let kimi_oauth = crate::agent::config::model_uses_kimi_code_oauth(model);
                        (
                            crate::agent::config::kimi_code_bearer_resolver_for_model(model)
                                .or_else(|| {
                                    crate::agent::config::openai_codex_bearer_resolver_for_model(
                                        model,
                                    )
                                }),
                            crate::agent::config::model_uses_openai_codex_oauth(model),
                            crate::agent::config::model_uses_kimi_request_dialect(model),
                            kimi_oauth,
                        )
                    }
                    None => {
                        // URL-only fallback (catalog miss). Shared Kimi/Codex
                        // proxies fail-closed via `oauth_platform = None`.
                        // Unique Kimi origin precedence:
                        // 1) trusted same-route baseline resolver → OAuth
                        // 2) live key equals persisted Kimi catalog token
                        //    (compare only, never log) → OAuth
                        // 3) live AuthType::ApiKey that is not the persisted
                        //    Kimi token → static (keep key, no resolver)
                        // 4) SessionToken / unrelated / empty key → install
                        //    Kimi identity resolver (pre-login or fail-open
                        //    to live OAuth refresh); strip stamped key
                        let trusted_baseline_resolver =
                            same_baseline_route && ctx.sampling_config.bearer_resolver.is_some();
                        let live_key = creds.api_key.as_deref().filter(|k| !k.trim().is_empty());
                        let matches_persisted_kimi = live_key.is_some_and(|key| {
                            crate::auth::kimi::kimi_code_catalog_access_token_cached()
                                .as_deref()
                                .is_some_and(|persisted| persisted == key)
                        });
                        let live_static_api_key = live_key.is_some()
                            && creds.auth_type == xai_chat_state::AuthType::ApiKey
                            && !matches_persisted_kimi;
                        let bearer_resolver = match oauth_platform {
                            Some(xai_grok_models::PlatformId::KimiCode)
                                if trusted_baseline_resolver =>
                            {
                                ctx.sampling_config.bearer_resolver.clone()
                            }
                            Some(xai_grok_models::PlatformId::KimiCode) if live_static_api_key => {
                                None
                            }
                            Some(xai_grok_models::PlatformId::KimiCode) => {
                                crate::agent::config::kimi_code_bearer_resolver_for_base_url(
                                    &cfg.base_url,
                                )
                            }
                            Some(xai_grok_models::PlatformId::OpenAiCodex) => {
                                crate::agent::config::openai_codex_bearer_resolver_for_base_url(
                                    &cfg.base_url,
                                )
                            }
                            _ if !oauth_origin && same_baseline_route => {
                                ctx.sampling_config.bearer_resolver.clone()
                            }
                            _ => None,
                        };
                        let kimi_oauth_active = oauth_platform
                            == Some(xai_grok_models::PlatformId::KimiCode)
                            && bearer_resolver.is_some();
                        (
                            bearer_resolver,
                            oauth_platform == Some(xai_grok_models::PlatformId::OpenAiCodex),
                            oauth_platform == Some(xai_grok_models::PlatformId::KimiCode)
                                || xai_grok_models::PlatformId::MoonshotCn
                                    .base_url_matches(&cfg.base_url)
                                || xai_grok_models::PlatformId::MoonshotAi
                                    .base_url_matches(&cfg.base_url),
                            kimi_oauth_active,
                        )
                    }
                };
            let responses_codex_dialect = cfg.api_backend
                == crate::sampling::ApiBackend::CodexResponses
                || cfg.adapter_kind == xai_grok_models::AdapterKind::OpenAiCodex
                || codex_oauth_active;
            let auth_scheme = parent_model
                .map(|model| model.info().auth_scheme)
                .or_else(|| {
                    crate::agent::config::try_resolve_model_credentials(&cfg.model, None)
                        .map(|resolved| resolved.auth_scheme)
                })
                .unwrap_or_default();
            let mut extra_headers = cfg.extra_headers;
            crate::agent::config::inject_url_derived_headers(
                &mut extra_headers,
                creds.alpha_test_key.as_deref(),
                &cfg.base_url,
            );
            crate::agent::config::align_oauth_headers_with_platform(
                &mut extra_headers,
                oauth_platform.filter(|platform| {
                    *platform != xai_grok_models::PlatformId::OpenAiCodex || codex_oauth_active
                }),
                &cfg.base_url,
            );
            if oauth_platform == Some(xai_grok_models::PlatformId::KimiCode) && !kimi_oauth_active {
                crate::agent::config::remove_kimi_device_headers(&mut extra_headers);
            }
            let fail_closed_ambiguous_oauth =
                parent_model.is_none() && oauth_origin && oauth_platform.is_none();
            // OAuth / fail-closed paths clear the stamped key first. Evaluate
            // strip_guard against the *effective* key so a stale SessionToken
            // (or OAuth marker) cannot suppress a live Kimi/Codex resolver.
            let effective_api_key =
                if kimi_oauth_active || codex_oauth_active || fail_closed_ambiguous_oauth {
                    None
                } else {
                    creds.api_key
                };
            let strip_guard = ctx.would_strip_fallback_key(effective_api_key.as_deref());
            let catalog_model_id = parent_catalog_model_id(ctx, &cfg.model);
            let supports_backend_search = ctx
                .models_manager
                .model_supports_backend_search(catalog_model_id.0.as_ref());
            let extra_response_includes = crate::agent::config::response_include_extensions(
                supports_backend_search,
                &cfg.api_backend,
                &cfg.base_url,
            );

            let inherited = xai_grok_sampler::SamplerConfig {
                api_key: effective_api_key,
                base_url: cfg.base_url,
                model: cfg.model.clone(),
                max_completion_tokens: cfg.max_completion_tokens,
                temperature: cfg.temperature,
                top_p: cfg.top_p,
                api_backend: cfg.api_backend,
                adapter_kind: cfg.adapter_kind,
                request_compat: cfg.request_compat,
                endpoint_path: cfg.endpoint_path,
                auth_scheme,
                extra_headers,
                extra_response_includes,
                query_params: cfg.query_params.clone(),
                env_http_headers: cfg.env_http_headers.clone(),
                context_window: cfg.context_window.get(),
                client_version: creds.client_version,
                reasoning_effort: cfg.reasoning_effort,
                force_http1: false,
                max_retries: None,
                stream_tool_calls: cfg.stream_tool_calls.unwrap_or(false),
                idle_timeout_secs: None,
                client_identifier: ctx.sampling_config.client_identifier.clone(),
                deployment_id: ctx.sampling_config.deployment_id.clone(),
                user_id: ctx.sampling_config.user_id.clone(),
                origin_client: ctx.sampling_config.origin_client.clone(),
                attribution_callback: ctx.attribution_callback.clone(),
                bearer_resolver: if strip_guard { None } else { bearer_resolver },
                supports_backend_search,

                compactions_remaining: ctx
                    .models_manager
                    .model_compactions_remaining(catalog_model_id.0.as_ref()),
                compaction_at_tokens: ctx
                    .models_manager
                    .model_compaction_at_tokens(catalog_model_id.0.as_ref()),
                doom_loop_recovery: ctx.sampling_config.doom_loop_recovery,
                header_injector: ctx.sampling_config.header_injector.clone(),
                responses_codex_dialect,
                bedrock_request_metadata: ctx.sampling_config.bedrock_request_metadata.clone(),
                bedrock_headers: ctx.sampling_config.bedrock_headers.clone(),
                bedrock_profile: ctx.sampling_config.bedrock_profile.clone(),
                kimi_dialect,
            };
            let model_id = ctx.model_id.clone();
            let global_model_id = ctx.models_manager.current_model_id();
            xai_grok_telemetry::unified_log::debug(
                "subagent read parent config (live)",
                None,
                Some(serde_json::json!({
                    "parent_model": &inherited.model,
                    "parent_base_url": &inherited.base_url,
                    "parent_key_prefix": key_prefix(&inherited.api_key),
                    "session_model_id": model_id.0.as_ref(),
                    "global_model_id": global_model_id.0.as_ref(),
                    "source": "chat_state",
                })),
            );
            return (inherited, model_id);
        }
        tracing::warn!(
            "Parent chat state actor returned None for sampling config, \
             falling back to spawn context baseline"
        );
    }
    xai_grok_telemetry::unified_log::warn(
        "subagent read parent config (fallback)",
        None,
        Some(serde_json::json!({
            "parent_model": &ctx.sampling_config.model,
            "parent_base_url": &ctx.sampling_config.base_url,
            "parent_key_prefix": key_prefix(&ctx.sampling_config.api_key),
            "source": "spawn_context_baseline",
            "has_chat_state": ctx.parent_chat_state.is_some(),
        })),
    );
    let mut fallback = ctx.sampling_config.clone();
    fallback.bearer_resolver = if ctx.would_strip_fallback_key(fallback.api_key.as_deref()) {
        None
    } else {
        inherited_bearer_resolver(ctx, &fallback.model, &fallback.base_url)
    };
    let catalog_model_id = parent_catalog_model_id(ctx, &fallback.model);
    fallback.supports_backend_search = ctx
        .models_manager
        .model_supports_backend_search(catalog_model_id.0.as_ref());
    fallback.extra_response_includes = crate::agent::config::response_include_extensions(
        fallback.supports_backend_search,
        &fallback.api_backend,
        &fallback.base_url,
    );
    fallback.compactions_remaining = ctx
        .models_manager
        .model_compactions_remaining(catalog_model_id.0.as_ref());
    fallback.compaction_at_tokens = ctx
        .models_manager
        .model_compaction_at_tokens(catalog_model_id.0.as_ref());
    (fallback, ctx.model_id.clone())
}
/// `AuthType` for a subagent: BYOK ⇒ `ApiKey` (don't overwrite the BYOK
/// key); session-based ACP method ⇒ `SessionToken` (keep refresh wired);
/// otherwise `ApiKey`.
fn subagent_auth_type(
    model: Option<&crate::agent::config::ModelEntry>,
    auth_method_id: &acp::AuthMethodId,
) -> xai_chat_state::AuthType {
    if model.is_some_and(|m| m.has_own_credentials()) {
        xai_chat_state::AuthType::ApiKey
    } else if crate::agent::auth_method::is_session_based_method(auth_method_id) {
        xai_chat_state::AuthType::SessionToken
    } else {
        xai_chat_state::AuthType::ApiKey
    }
}
/// Resolve a model override string (config key or model ID) to a
/// `(SamplerConfig, ModelId)` pair.
fn resolve_model_override_to_config(
    model_id: &str,
    ctx: &SubagentSpawnContext,
) -> Option<(xai_grok_sampler::SamplerConfig, acp::ModelId)> {
    let entry = crate::agent::config::find_model_by_id(&ctx.available_models, model_id).cloned()?;
    let canonical_model_id = if ctx.available_models.contains_key(model_id) {
        acp::ModelId::new(model_id)
    } else {
        acp::ModelId::new(entry.info().model.clone())
    };
    let session_key = ctx.auth.as_ref().map(|a| a.key.as_str());
    let has_session_key = session_key.is_some();
    let mut credentials = resolve_credentials(&entry, session_key);
    credentials.auth_type = subagent_auth_type(Some(&entry), &ctx.auth_method_id);
    let resolved_auth_type = credentials.auth_type;
    let mut config = sampling_config_for_model(
        &entry,
        credentials,
        ctx.alpha_test_key.clone(),
        ctx.sampling_config.client_version.clone(),
        ctx.sampling_config.deployment_id.clone(),
        ctx.sampling_config.user_id.clone(),
    );
    config.bearer_resolver = if !ctx.would_strip_fallback_key(config.api_key.as_deref())
        && resolved_auth_type == xai_chat_state::AuthType::SessionToken
    {
        session_bearer_resolver(
            ctx,
            if entry.has_own_credentials() {
                crate::agent::auth_method::ModelByok::Byok
            } else {
                crate::agent::auth_method::ModelByok::NotByok
            },
            &config.base_url,
        )
    } else {
        None
    };
    xai_grok_telemetry::unified_log::debug(
        "subagent resolve_model_override_to_config",
        None,
        Some(serde_json::json!({
            "model_id": model_id,
            "canonical_model": canonical_model_id.0.as_ref(),
            "resolved_model_raw": &config.model,
            "base_url": &config.base_url,
            "key_prefix": key_prefix(&config.api_key),
            "has_own_credentials": entry.has_own_credentials(),
            "has_session_key": has_session_key,
            "auth_type": format!("{:?}", resolved_auth_type),
            "auth_method_id": ctx.auth_method_id.0.as_ref(),
        })),
    );
    Some((config, canonical_model_id))
}
/// Leading items to preserve across compaction on resume: the System head only, so the
/// resumed body (the child's own work) stays compactable. Returns 0 when there's no
/// leading System; the spawn path then inserts one and bumps the prefix to 1.
pub(crate) fn resume_inherited_prefix_len(
    conversation: &[xai_grok_sampling_types::conversation::ConversationItem],
) -> usize {
    conversation
        .iter()
        .take_while(|i| matches!(i, ConversationItem::System(_)))
        .count()
}
/// How a subagent's initial conversation was bootstrapped.
struct InitialContext {
    source: InitialContextSource,
    copy_error: Option<String>,
    prefix_len: Option<usize>,
    conversation: Vec<xai_grok_sampling_types::conversation::ConversationItem>,
    /// True only for a verbatim mirror-fork (parent items copied byte-for-byte).
    /// Gates sending the parent tool snapshot so the child's full request prefix
    /// matches the parent. A summarized-fork fallback leaves this false.
    verbatim_fork: bool,
}
/// Resume bootstrap: preserve only the System head (see `resume_inherited_prefix_len`).
fn resume_initial_context(
    conversation: Vec<xai_grok_sampling_types::conversation::ConversationItem>,
) -> InitialContext {
    InitialContext {
        source: InitialContextSource::Resumed,
        copy_error: None,
        prefix_len: Some(resume_inherited_prefix_len(&conversation)),
        conversation,
        verbatim_fork: false,
    }
}
/// Apply `fork_filter_chat` then normalize; empty or System-only input (no
/// `<background_context>` produced) fails open to `New`.
fn forked_initial_context(
    mut items: Vec<xai_grok_sampling_types::conversation::ConversationItem>,
) -> InitialContext {
    crate::sampling::fork_filter_chat(&mut items);
    if items.is_empty() {
        return InitialContext {
            source: InitialContextSource::New,
            copy_error: Some("empty parent conversation".to_string()),
            prefix_len: None,
            conversation: vec![],
            verbatim_fork: false,
        };
    }
    let (conversation, prefix_len) =
        xai_grok_subagent_resolution::context::normalize_forked_context(items);
    if prefix_len < 2 {
        return InitialContext {
            source: InitialContextSource::New,
            copy_error: Some("no inheritable parent content".to_string()),
            prefix_len: None,
            conversation: vec![],
            verbatim_fork: false,
        };
    }
    InitialContext {
        source: InitialContextSource::Forked,
        copy_error: None,
        prefix_len: Some(prefix_len),
        conversation,
        verbatim_fork: false,
    }
}
/// A verbatim mirror requires a coherent tail: the conversation must end on a
/// plain assistant text response (a clean turn boundary). A dangling assistant
/// (unanswered tool calls), a trailing ToolResult (mid-turn), or a trailing
/// user/reasoning means the prefix would be incoherent, so the caller falls back
/// to the summarized path instead of partial-trimming.
fn conversation_tail_is_complete(
    items: &[xai_grok_sampling_types::conversation::ConversationItem],
) -> bool {
    matches!(
        items.last(),
        Some(ConversationItem::Assistant(a)) if a.tool_calls.is_empty()
    )
}
/// Decide the live-fork context.
///
/// Verbatim mirror (the cache-preserving path): when the parent fits the child
/// window (same 80% guard as resume) AND ends at a clean turn boundary, keep the
/// items BYTE-FOR-BYTE. We deliberately do NOT run `fork_filter_chat` here — its
/// step 1 strips synthetic-reason user items (`<system-reminder>`s, drained
/// monitor events, doom-loop warnings) that the parent actually sent and cached;
/// stripping them would diverge the child prefix at the first removed item and
/// cap radix reuse there. At planner spawn the conversation is between turns
/// (the `/goal` user message is not yet pushed), so the tail is already complete
/// and no trimming is needed; an incomplete tail falls back to summarized.
///
/// Summarized fallback (oversize OR incomplete tail): the reasoning-aware
/// `fork_filter_chat` drops synthetics + trims the incomplete tail, then
/// `normalize_forked_context` summarizes. (This is the ONLY path that filters;
/// the verbatim path never does.)
///
/// Input that is empty or only `System` item(s) — before OR after filtering —
/// inherited nothing, so it fails open to `New` rather than a hollow fork.
fn verbatim_or_normalize_fork(
    items: Vec<xai_grok_sampling_types::conversation::ConversationItem>,
    child_context_window: u64,
) -> InitialContext {
    if !items
        .iter()
        .any(|i| !matches!(i, ConversationItem::System(_)))
    {
        return InitialContext {
            source: InitialContextSource::New,
            copy_error: Some("forked parent conversation has no inheritable content".to_string()),
            prefix_len: None,
            conversation: vec![],
            verbatim_fork: false,
        };
    }
    let estimated_tokens = xai_chat_state::estimate_conversation_tokens(&items);
    const SAFE_FORK_PERCENT: u64 = 80;
    let threshold = child_context_window * SAFE_FORK_PERCENT / 100;
    if estimated_tokens <= threshold && conversation_tail_is_complete(&items) {
        let prefix_len = items.len();
        return InitialContext {
            source: InitialContextSource::Forked,
            copy_error: None,
            prefix_len: Some(prefix_len),
            conversation: items,
            verbatim_fork: true,
        };
    }
    let mut filtered = items;
    crate::sampling::fork_filter_chat(&mut filtered);
    if !filtered
        .iter()
        .any(|i| !matches!(i, ConversationItem::System(_)))
    {
        return InitialContext {
            source: InitialContextSource::New,
            copy_error: Some("no inheritable parent content after filtering".to_string()),
            prefix_len: None,
            conversation: vec![],
            verbatim_fork: false,
        };
    }
    let (conversation, prefix_len) =
        xai_grok_subagent_resolution::context::normalize_forked_context(filtered);
    InitialContext {
        source: InitialContextSource::Forked,
        copy_error: None,
        prefix_len: Some(prefix_len),
        conversation,
        verbatim_fork: false,
    }
}
/// `true` only when the fork actually summarized (ran `normalize_forked_context`).
/// A verbatim mirror-fork inherits items as-is and never normalizes, so it reports
/// `false` even though its source is `Forked`.
fn fork_context_normalized(source: &InitialContextSource, verbatim_fork: bool) -> bool {
    matches!(source, InitialContextSource::Forked) && !verbatim_fork
}
/// Stamp `subagent_fork` / `forked` on the child summary (live path; disk copy already stamps).
fn stamp_live_fork_session_metadata(
    child_session_info: &SessionInfo,
    parent_session_id: &str,
    parent_prompt_id: Option<String>,
    model_id: &str,
    inherited_prefix_len: Option<usize>,
    fork_context_source: &str,
) {
    let dir = match session::persistence::ensure_owner_only_session_dir(child_session_info) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(error = %e, "live fork: could not create child session dir for metadata stamp");
            return;
        }
    };
    let summary_path = dir.join("summary.json");
    let model = acp::ModelId::new(model_id);
    let mut summary = std::fs::read(&summary_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .or_else(|| session::persistence::Summary::new(child_session_info, model).ok());
    let Some(ref mut summary) = summary else {
        tracing::warn!("live fork: could not load or create child summary");
        return;
    };
    summary.session_kind = Some("subagent_fork".to_string());
    summary.fork_context_source = Some(fork_context_source.to_string());
    summary.parent_session_id = Some(parent_session_id.to_string());
    summary.fork_parent_prompt_id = parent_prompt_id;
    summary.inherited_prefix_len = inherited_prefix_len;
    summary.forked_at = Some(chrono::Utc::now());
    if let Ok(bytes) = serde_json::to_vec_pretty(summary)
        && let Err(e) = std::fs::write(&summary_path, bytes)
    {
        tracing::warn!(error = %e, "live fork: failed to write forked session summary");
    }
}
enum BootstrapInitialContext {
    Ready(InitialContext),
    /// Explicit resume_from failed — abort spawn (fail closed).
    ResumeAbort(String),
}
/// Phase 3: resume (fail-closed on copy error) > fork (live then disk, fail-open) > New.
/// Unresolved non-empty resume is aborted by the caller before this runs.
async fn bootstrap_initial_context(
    request: &SubagentRequest,
    resume_source: Option<&ResumeSourceData>,
    ctx: &SubagentSpawnContext,
    child_session_info: &SessionInfo,
    child_session_dir: &std::path::Path,
    effective_model_id: &str,
    child_context_window: u64,
) -> BootstrapInitialContext {
    if request.fork_context && request.resume_from.is_some() {
        tracing::info!(
            subagent_id = %request.id,
            resume_from = ?request.resume_from,
            resume_resolved = resume_source.is_some(),
            "resume_from and fork_context both set; resolved resume wins (fail-closed on copy error, never forks)"
        );
    }
    if let Some(source) = resume_source {
        let source_session_info = SessionInfo {
            id: acp::SessionId::new(source.child_session_id.clone()),
            cwd: source.child_cwd.clone(),
        };
        let storage = crate::session::storage::jsonl::JsonlStorageAdapter::with_root(
            crate::util::grok_home::grok_home(),
        );
        let copy_options = crate::session::storage::CopySessionOptions {
            parent_session_id: Some(source.child_session_id.clone()),
            new_model_id: Some(effective_model_id.to_string()),
            session_kind: Some("subagent_resume".to_string()),
            fork_context_source: Some("resumed".to_string()),
            fork_parent_prompt_id: request.parent_prompt_id.clone(),
            copy_plan_state: false,
            copy_plan_mode_state: false,
            copy_signals: false,
            copy_tool_state: true,
            fork_filter: false,
            ..Default::default()
        };
        use crate::session::storage::StorageAdapter as _;
        return match storage
            .copy_session_data(&source_session_info, child_session_info, copy_options)
            .await
        {
            Ok(result) => {
                let conversation = match storage.load_chat_history_from_dir(child_session_dir) {
                    Ok(items) if !items.is_empty() => items,
                    Ok(_) => {
                        return BootstrapInitialContext::ResumeAbort(format!(
                            "Cannot resume from subagent '{}': \
                             copied transcript is empty",
                            source.subagent_id,
                        ));
                    }
                    Err(e) => {
                        return BootstrapInitialContext::ResumeAbort(format!(
                            "Cannot resume from subagent '{}': \
                             failed to load copied transcript: {e}",
                            source.subagent_id,
                        ));
                    }
                };
                let estimated_tokens = xai_chat_state::estimate_conversation_tokens(&conversation);
                const SAFE_RESUME_PERCENT: u64 = 80;
                let threshold = child_context_window * SAFE_RESUME_PERCENT / 100;
                if estimated_tokens > threshold {
                    return BootstrapInitialContext::ResumeAbort(format!(
                        "Cannot resume from subagent '{}': source transcript \
                         (~{estimated_tokens} tokens) exceeds {SAFE_RESUME_PERCENT}% of \
                         the model's context window ({child_context_window} tokens). \
                         The source conversation is too large for the current model.",
                        source.subagent_id,
                    ));
                }
                tracing::info!(
                    subagent_id = %request.id,
                    source_subagent = %source.subagent_id,
                    chat_messages = result.chat_messages_copied,
                    tool_state = result.tool_state_copied,
                    estimated_tokens,
                    "Resume-copied source child session data into new child"
                );
                BootstrapInitialContext::Ready(resume_initial_context(conversation))
            }
            Err(e) => BootstrapInitialContext::ResumeAbort(format!(
                "Cannot resume from subagent '{}': failed to copy source session data: {e}",
                source.subagent_id,
            )),
        };
    }
    if !request.fork_context {
        return BootstrapInitialContext::Ready(InitialContext {
            source: InitialContextSource::New,
            copy_error: None,
            prefix_len: None,
            conversation: vec![],
            verbatim_fork: false,
        });
    }
    let live_items = match ctx.parent_chat_state.as_ref() {
        Some(chat_state) => {
            let items = chat_state.get_conversation().await;
            if items.is_empty() { None } else { Some(items) }
        }
        None => None,
    };
    if let Some(items) = live_items {
        let ctx_out = verbatim_or_normalize_fork(items, child_context_window);
        tracing::info!(
            subagent_id = %request.id,
            subagent_type = %request.subagent_type,
            loaded_items = ctx_out.conversation.len(),
            source = ?ctx_out.source,
            verbatim = ctx_out.verbatim_fork,
            "Forked context from live parent_chat_state"
        );
        if matches!(ctx_out.source, InitialContextSource::Forked) {
            let marker = if ctx_out.verbatim_fork {
                "forked_verbatim"
            } else {
                "forked_summarized"
            };
            stamp_live_fork_session_metadata(
                child_session_info,
                &ctx.parent_session_id,
                request.parent_prompt_id.clone(),
                effective_model_id,
                ctx_out.prefix_len,
                marker,
            );
        }
        return BootstrapInitialContext::Ready(ctx_out);
    }
    if let Some(ref parent_info) = ctx.parent_session_info {
        let storage = crate::session::storage::jsonl::JsonlStorageAdapter::with_root(
            crate::util::grok_home::grok_home(),
        );
        let copy_options = crate::session::storage::CopySessionOptions {
            parent_session_id: Some(ctx.parent_session_id.clone()),
            new_model_id: Some(effective_model_id.to_string()),
            session_kind: Some("subagent_fork".to_string()),
            fork_context_source: Some("forked".to_string()),
            fork_parent_prompt_id: request.parent_prompt_id.clone(),
            copy_plan_state: false,
            copy_plan_mode_state: false,
            copy_signals: false,
            copy_tool_state: false,
            fork_filter: true,
            ..Default::default()
        };
        use crate::session::storage::StorageAdapter as _;
        return match storage
            .copy_session_data(parent_info, child_session_info, copy_options)
            .await
        {
            Ok(result) => {
                tracing::info!(
                    subagent_id = %request.id,
                    subagent_type = %request.subagent_type,
                    chat_messages = result.chat_messages_copied,
                    tool_state = result.tool_state_copied,
                    "Fork-copied parent session data into child (disk fallback)"
                );
                let items = storage
                    .load_chat_history_from_dir(child_session_dir)
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            error = %e,
                            "Failed to load forked chat history, starting with empty context"
                        );
                        vec![]
                    });
                BootstrapInitialContext::Ready(forked_initial_context(items))
            }
            Err(e) => {
                let err_msg = format!("{e}");
                tracing::warn!(
                    subagent_id = %request.id,
                    subagent_type = %request.subagent_type,
                    error = %e,
                    "Failed to fork-copy parent session, falling back to fresh"
                );
                BootstrapInitialContext::Ready(InitialContext {
                    source: InitialContextSource::New,
                    copy_error: Some(err_msg),
                    prefix_len: None,
                    conversation: vec![],
                    verbatim_fork: false,
                })
            }
        };
    }
    tracing::warn!(
        subagent_id = %request.id,
        subagent_type = %request.subagent_type,
        "fork_context=true but no live parent conversation or parent_session_info; falling back to fresh"
    );
    BootstrapInitialContext::Ready(InitialContext {
        source: InitialContextSource::New,
        copy_error: Some("parent conversation unavailable".to_string()),
        prefix_len: None,
        conversation: vec![],
        verbatim_fork: false,
    })
}
/// Resolve the effective working directory for a child session.
///
/// Precedence: worktree path > `override_cwd` (non-empty) > parent cwd. The
/// caller selects `override_cwd`: a resumed child inherits the source's
/// effective cwd, a fresh spawn honors its `request.cwd`.
fn resolve_child_cwd(
    worktree_path: Option<&Path>,
    override_cwd: Option<&str>,
    parent_cwd: &Path,
) -> PathBuf {
    worktree_path
        .map(Path::to_path_buf)
        .or_else(|| override_cwd.filter(|s| !s.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| parent_cwd.to_path_buf())
}
/// The cwd a resumed child inherits from its source subagent, or `None` when
/// there is nothing to inherit (the caller then falls back to the parent cwd).
///
/// Only non-worktree sources inherit here — worktree-backed sources are reused
/// by the worktree path. The cwd is existence-checked because a source can be
/// pinned into a sibling's worktree that the snapshot stack later disposes;
/// resume otherwise skips cwd validation.
fn resume_inherited_cwd(source: Option<&ResumeSourceData>) -> Option<&str> {
    let source = source?;
    if source.worktree_path.is_some() || source.child_cwd.is_empty() {
        return None;
    }
    if !Path::new(&source.child_cwd).is_dir() {
        tracing::warn!(
            source_subagent_id = %source.subagent_id,
            child_cwd = %source.child_cwd,
            "Resume source cwd no longer exists; using parent workspace"
        );
        return None;
    }
    Some(source.child_cwd.as_str())
}
/// Select the cwd override for a child: a resume inherits the source's cwd
/// (never its own `request.cwd`); a fresh spawn uses `request.cwd`.
fn select_override_cwd<'a>(
    resume_source: Option<&'a ResumeSourceData>,
    request_cwd: Option<&'a str>,
) -> Option<&'a str> {
    if resume_source.is_some() {
        resume_inherited_cwd(resume_source)
    } else {
        request_cwd
    }
}
fn durable_resume_source_for(
    id: &str,
    parent_session_id: &str,
    parent_cwd: &Path,
) -> Option<ResumeSourceData> {
    let parent_info = SessionInfo {
        id: acp::SessionId::new(parent_session_id),
        cwd: parent_cwd.to_string_lossy().into_owned(),
    };
    let meta_path = session::persistence::session_dir(&parent_info)
        .join("subagents")
        .join(id)
        .join("meta.json");
    let data = std::fs::read_to_string(meta_path).ok()?;
    let meta: SubagentMeta = serde_json::from_str(&data).ok()?;
    if meta.parent_session_id != parent_session_id
        || !matches!(meta.status.as_str(), "completed" | "failed" | "cancelled")
    {
        return None;
    }
    Some(ResumeSourceData {
        subagent_id: meta.subagent_id,
        child_session_id: meta.child_session_id,
        child_cwd: meta.child_cwd.unwrap_or_default(),
        worktree_path: meta.worktree_path.map(PathBuf::from),
        snapshot_ref: meta.snapshot_ref,
        subagent_type: meta.subagent_type,
        persona: meta.persona,
        model_id: meta.effective_model_id,
    })
}
/// Resolve the MCP pool a child subagent should import from its parent.
///
/// Inheritance applies to **every** agent source (built-in, user, project,
/// and plugin). Plugin agents are not excluded: the parent already connected
/// these servers for the session. Agent-owned `mcpServers` (spawned by the
/// child itself) are handled separately and remain blocked for plugins.
///
/// Returns `None` when there is no parent pool or `inheritance` is
/// [`McpInheritance::None`] (avoids an empty import call downstream).
fn resolve_inherited_mcp_pool(
    parent_pool: Option<crate::session::mcp_servers::SharedMcpPool>,
    inheritance: &xai_grok_agent::config::McpInheritance,
) -> Option<crate::session::mcp_servers::SharedMcpPool> {
    parent_pool.and_then(|pool| filter_pool_by_inheritance(pool, inheritance))
}
/// Apply `McpInheritance` filtering to a parent MCP pool snapshot.
///
/// Returns `None` for `McpInheritance::None` (no pool at all — avoids
/// an empty import call downstream). For `Named`/`Except`, retains or
/// removes the matching server names in-place.
fn filter_pool_by_inheritance(
    mut pool: crate::session::mcp_servers::SharedMcpPool,
    inheritance: &xai_grok_agent::config::McpInheritance,
) -> Option<crate::session::mcp_servers::SharedMcpPool> {
    match inheritance {
        McpInheritance::All => Some(pool),
        McpInheritance::None => None,
        McpInheritance::Named(names) => {
            let before = pool.server_names().count();
            pool.retain_clients(|name| names.iter().any(|n| n == name));
            tracing::debug!(
                before,
                after = pool.server_names().count(),
                ?names,
                "MCP inheritance: Named filter applied"
            );
            Some(pool)
        }
        McpInheritance::Except(names) => {
            let before = pool.server_names().count();
            pool.retain_clients(|name| !names.iter().any(|n| n == name));
            tracing::debug!(
                before,
                after = pool.server_names().count(),
                ?names,
                "MCP inheritance: Except filter applied"
            );
            Some(pool)
        }
    }
}
/// Whether a subagent may declare its own agent-owned `mcpServers`.
///
/// Plugin agents cannot: untrusted packages must not spawn MCP processes or
/// open network MCP endpoints. Parent-pool inheritance is independent and
/// always available subject to [`McpInheritance`].
fn agent_owned_mcp_servers_allowed(is_plugin_agent: bool) -> bool {
    !is_plugin_agent
}
/// Resolve a subagent type name to its `AgentDefinition`, with the parent
/// session's CLI tool/permission overrides already applied (so the spawn path
/// can never obtain a definition that skips them).
fn resolve_agent_definition(
    subagent_type: &str,
    ctx: &SubagentSpawnContext,
) -> Option<xai_grok_agent::config::AgentDefinition> {
    let cli_agents = ctx
        .agent_config
        .as_ref()
        .map(|config| config.cli_agents.as_slice())
        .unwrap_or_default();
    let resolution_context = xai_grok_subagent_resolution::DefinitionResolutionContext {
        cwd: &ctx.parent_cwd,
        plugins: ctx.plugin_registry.as_deref(),
        cli_agents,
        toggles: &ctx.subagent_toggle,
        allowed_types: ctx.allowed_subagent_types.as_deref(),
    };
    let mut def = xai_grok_subagent_resolution::discover_agent_definition(
        subagent_type,
        &resolution_context,
    )?;
    ctx.apply_session_cli_overrides(&mut def);
    Some(def)
}
fn available_agent_names(ctx: &SubagentSpawnContext) -> Vec<String> {
    let cli_agents = ctx
        .agent_config
        .as_ref()
        .map(|config| config.cli_agents.as_slice())
        .unwrap_or_default();
    xai_grok_subagent_resolution::available_agent_names(
        &xai_grok_subagent_resolution::DefinitionResolutionContext {
            cwd: &ctx.parent_cwd,
            plugins: ctx.plugin_registry.as_deref(),
            cli_agents,
            toggles: &ctx.subagent_toggle,
            allowed_types: ctx.allowed_subagent_types.as_deref(),
        },
    )
}
/// Minimal per-session context for `validate_subagent_type`.
/// Avoids the heavy `SubagentSpawnContext` clone on the validation hot path.
#[derive(Default)]
pub(crate) struct SubagentValidationContext {
    pub parent_cwd: PathBuf,
    pub plugin_registry: Option<Arc<xai_grok_agent::plugins::PluginRegistry>>,
    pub subagent_toggle: HashMap<String, bool>,
    pub allowed_subagent_types: Option<Vec<String>>,
    pub cli_agent_names: Vec<String>,
}
/// Synchronously validate a subagent type against discovery + toggle + allow-list.
/// `Unknown { available }` is sorted by `str::cmp` for stable rendering.
pub(crate) fn validate_subagent_type(
    subagent_type: &str,
    ctx: &SubagentValidationContext,
) -> SubagentValidateTypeOutcome {
    let context = xai_grok_subagent_resolution::DefinitionValidationContext {
        cwd: &ctx.parent_cwd,
        plugins: ctx.plugin_registry.as_deref(),
        cli_agent_names: &ctx.cli_agent_names,
        toggles: &ctx.subagent_toggle,
        allowed_types: ctx.allowed_subagent_types.as_deref(),
    };
    match xai_grok_subagent_resolution::validate_agent_name(subagent_type, &context) {
        Ok(()) => SubagentValidateTypeOutcome::Ok,
        Err(xai_grok_subagent_resolution::ResolutionError::Unknown { available, .. }) => {
            SubagentValidateTypeOutcome::Unknown { available }
        }
        Err(xai_grok_subagent_resolution::ResolutionError::Disabled { .. }) => {
            SubagentValidateTypeOutcome::Disabled
        }
        Err(xai_grok_subagent_resolution::ResolutionError::NotAllowed { allowed, .. }) => {
            SubagentValidateTypeOutcome::NotAllowed { allowed }
        }
        Err(
            xai_grok_subagent_resolution::ResolutionError::PersonaResolution(_)
            | xai_grok_subagent_resolution::ResolutionError::ResumeValidation(_),
        ) => SubagentValidateTypeOutcome::ValidationUnavailable,
    }
}
/// Gate an already-resolved subagent type against the `[subagents.toggle]`
/// disable map and the parent's allow-list.
///
/// The caller must have already confirmed the type resolves to an
/// `AgentDefinition`; this checks ONLY the toggle + allow-list gates,
/// returning `Ok` when the type may run and `Disabled` / `NotAllowed`
/// otherwise (never `Unknown` / `ValidationUnavailable`). Shared by
/// [`run_shell_child`] and [`describe_subagent_type`] so both apply
/// identical gates.
fn gate_subagent_type(
    subagent_type: &str,
    ctx: &SubagentSpawnContext,
) -> SubagentValidateTypeOutcome {
    let cli_agents = ctx
        .agent_config
        .as_ref()
        .map(|config| config.cli_agents.as_slice())
        .unwrap_or_default();
    let resolution_context = xai_grok_subagent_resolution::DefinitionResolutionContext {
        cwd: &ctx.parent_cwd,
        plugins: ctx.plugin_registry.as_deref(),
        cli_agents,
        toggles: &ctx.subagent_toggle,
        allowed_types: ctx.allowed_subagent_types.as_deref(),
    };
    match xai_grok_subagent_resolution::gate_agent_definition(subagent_type, &resolution_context) {
        Ok(()) => SubagentValidateTypeOutcome::Ok,
        Err(xai_grok_subagent_resolution::ResolutionError::Disabled { .. }) => {
            SubagentValidateTypeOutcome::Disabled
        }
        Err(xai_grok_subagent_resolution::ResolutionError::NotAllowed { allowed, .. }) => {
            SubagentValidateTypeOutcome::NotAllowed { allowed }
        }
        Err(
            xai_grok_subagent_resolution::ResolutionError::Unknown { .. }
            | xai_grok_subagent_resolution::ResolutionError::PersonaResolution(_)
            | xai_grok_subagent_resolution::ResolutionError::ResumeValidation(_),
        ) => SubagentValidateTypeOutcome::ValidationUnavailable,
    }
}
pub(crate) fn subagent_harness_flavor_is_representable(agent_type: &str) -> bool {
    xai_grok_subagent_resolution::subagent_harness_flavor_is_representable(agent_type)
}
/// Apply the harness-dependent toolset/prompt re-selection to a resolved
/// agent definition.
///
/// The harness flavor (alternate vs grok-build) normally follows the PARENT
/// agent: `GrokBuildOrchestrator` parents give children
/// the alternate harness; the orchestrator keeps children lean, and other parents
/// inherit the file-tool override (hashline vs standard). A `/goal` role may
/// pass `harness_agent_type` to OVERRIDE that flavor regardless of the parent
/// (so a grok-build session can run an alternate-harness verifier and vice-versa);
/// `None` for every non-goal spawn ⇒ the parent decides (unchanged). The base
/// toolset stays role-dependent on `subagent_type` (general-purpose →
/// implementer, else explorer), so the role keeps a capable toolset on the
/// chosen harness.
///
/// Extracted so both [`run_shell_child`] (real spawn) and
/// [`describe_subagent_type`] (read-only probe) build the SAME `tool_config`
/// for a given `(subagent_type, harness_agent_type, parent_name)` — no
/// duplication.
fn resolve_subagent_toolset(
    subagent_type: &str,
    harness_agent_type: Option<&str>,
    ctx: &SubagentSpawnContext,
    definition: &mut xai_grok_agent::config::AgentDefinition,
) {
    let resolution_context = xai_grok_subagent_resolution::HarnessToolsetContext {
        harness_override: harness_agent_type,
        parent_agent_name: ctx.parent_agent_name.as_deref(),
        parent_model_agent_type: ctx.parent_model_agent_type.as_deref(),
        file_tool_overrides: ctx.file_tool_overrides.as_deref(),
    };
    xai_grok_subagent_resolution::apply_harness_toolset(
        subagent_type,
        &resolution_context,
        definition,
    );
}
/// Map a resolved `ToolServerConfig` into a [`SubagentTypeSummary`].
///
/// Keys on each entry's `ToolConfig.kind` (first tool per kind wins).
/// Entries with `kind: None` — `from_id`/MCP/custom tools — are SKIPPED, so
/// this is NOT a byte-for-byte equivalent of the finalize-time `kind_to_name`
/// map (which keys on the registry `entry.kind`); the two agree for the
/// builtin goal toolsets, where every tool's kind is populated by
/// `From<&T: Tool>`, but diverge for `kind: None` tools (which carry no
/// capability signal anyway). The client-facing name is
/// `ToolConfig::resolve_client_name(default_id)` where `default_id` is the
/// unqualified tool id (the `"<namespace>:"` prefix on `tc.id` is stripped),
/// so a `name_override` is reflected. The read/search/execute flags are what
/// the per-role capability gates key on.
fn summarize_tool_config(
    config: &xai_grok_tools::registry::types::ToolServerConfig,
) -> SubagentTypeSummary {
    let mut tool_names: HashMap<ToolKind, String> = HashMap::new();
    for tc in &config.tools {
        let Some(kind) = tc.kind else { continue };
        let default_id = tc.id.rsplit(':').next().unwrap_or(tc.id.as_str());
        let client_name = tc.resolve_client_name(default_id);
        tool_names.entry(kind).or_insert(client_name);
    }
    SubagentTypeSummary {
        can_read: tool_names.contains_key(&ToolKind::Read),
        can_search: tool_names.contains_key(&ToolKind::Search),
        can_execute: tool_names.contains_key(&ToolKind::Execute),
        tool_names,
    }
}
/// Describe a subagent type's resolved toolset WITHOUT spawning it.
///
/// Runs the same resolution path as [`run_shell_child`] —
/// [`resolve_agent_definition`] + [`gate_subagent_type`] +
/// [`resolve_subagent_toolset`] — then summarizes the resulting
/// `tool_config`. Backs the `SubagentEvent::DescribeType` drain arm; the
/// parent uses the summary for the per-role capability gate and prompt
/// rendering before committing a configured `/goal` `{model, agent_type}` pair.
///
/// `harness_agent_type` is the `/goal`-only harness override: when set it must
/// resolve to an `AgentDefinition` via this module's [`resolve_agent_definition`]
/// (name-based project/plugin/builtin lookup — `by_name_in_cwd_with_plugins` +
/// `BuiltinAgentName`). That is equivalent to the main session for builtin
/// harness names but does NOT apply the main session's env / ACP-profile /
/// strict-harness precedence. An unresolvable harness returns `Unknown` so the
/// `/goal` caller fails open to the session harness; otherwise it decides the
/// summarized toolset's flavor. `None` (every non-goal probe) defers the flavor
/// to the parent agent (unchanged).
pub(crate) fn describe_subagent_type(
    subagent_type: &str,
    harness_agent_type: Option<&str>,
    ctx: &SubagentSpawnContext,
) -> SubagentDescribeOutcome {
    if let Some(harness) = harness_agent_type
        && resolve_agent_definition(harness, ctx).is_none()
    {
        return SubagentDescribeOutcome::Unknown {
            available: available_agent_names(ctx),
        };
    }
    let Some(mut definition) = resolve_agent_definition(subagent_type, ctx) else {
        return SubagentDescribeOutcome::Unknown {
            available: available_agent_names(ctx),
        };
    };
    match gate_subagent_type(subagent_type, ctx) {
        SubagentValidateTypeOutcome::Disabled => return SubagentDescribeOutcome::Disabled,
        SubagentValidateTypeOutcome::NotAllowed { allowed } => {
            return SubagentDescribeOutcome::NotAllowed { allowed };
        }
        SubagentValidateTypeOutcome::Unknown { available } => {
            return SubagentDescribeOutcome::Unknown { available };
        }
        SubagentValidateTypeOutcome::ValidationUnavailable => {
            return SubagentDescribeOutcome::Unavailable;
        }
        SubagentValidateTypeOutcome::Ok => {}
        _ => return SubagentDescribeOutcome::Unavailable,
    }
    resolve_subagent_toolset(subagent_type, harness_agent_type, ctx, &mut definition);
    SubagentDescribeOutcome::Ok(summarize_tool_config(&definition.tool_config))
}
/// Resolve a subagent's turn limit: its own `maxTurns` wins, else inherit the parent's.
fn resolve_subagent_max_turns(
    definition_max_turns: Option<u32>,
    parent_max_turns: Option<usize>,
) -> Option<usize> {
    definition_max_turns
        .map(|v| v as usize)
        .or(parent_max_turns)
}

/// Effective per-run limits resolved from an agent definition and the parent
/// session. Only `max_turns` inherits; tool and wall-clock limits are explicit
/// agent policies so an unbounded parent does not silently gain a deadline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SubagentExecutionBudget {
    max_turns: Option<usize>,
    max_tool_calls: Option<u32>,
    timeout_secs: Option<u64>,
    finalize_grace_secs: Option<u64>,
}

impl SubagentExecutionBudget {
    fn resolve(
        definition: &xai_grok_agent::config::AgentDefinition,
        parent_max_turns: Option<usize>,
    ) -> Self {
        let timeout_secs = definition.timeout_secs;
        let finalize_grace_secs = timeout_secs.map(|timeout| {
            definition
                .finalize_grace_secs
                .unwrap_or(30)
                .min(timeout.saturating_sub(1).max(1))
        });
        Self {
            max_turns: resolve_subagent_max_turns(definition.max_turns, parent_max_turns),
            max_tool_calls: definition.max_tool_calls,
            timeout_secs,
            finalize_grace_secs,
        }
    }

    fn is_unbounded(self) -> bool {
        self.max_turns.is_none() && self.max_tool_calls.is_none() && self.timeout_secs.is_none()
    }

    fn wire(self) -> Option<crate::extensions::notification::SubagentBudgetInfo> {
        if self.is_unbounded() {
            return None;
        }
        Some(crate::extensions::notification::SubagentBudgetInfo {
            max_turns: self.max_turns.and_then(|v| u32::try_from(v).ok()),
            max_tool_calls: self.max_tool_calls,
            timeout_secs: self.timeout_secs,
            finalize_grace_secs: self.finalize_grace_secs,
        })
    }

    /// Trigger a final-answer reminder before the hard tool-call limit. The
    /// reserve grows for large budgets but is capped so investigation still
    /// gets most of the configured allowance (40 calls finalizes at 32).
    fn finalize_at_tool_calls(self) -> Option<u32> {
        self.max_tool_calls.map(|limit| {
            let reserve = (limit / 5).clamp(1, 8);
            limit.saturating_sub(reserve).max(1)
        })
    }

    /// The model-call count is the same unit as `max_turns`. Reserve the final
    /// round for a recommendation without more tools.
    fn finalize_at_model_calls(self) -> Option<u64> {
        self.max_turns
            .map(|limit| u64::try_from(limit.saturating_sub(1).max(1)).unwrap_or(u64::MAX))
    }

    fn finalize_at_elapsed(self) -> Option<std::time::Duration> {
        self.timeout_secs.map(|timeout| {
            std::time::Duration::from_secs(
                timeout
                    .saturating_sub(self.finalize_grace_secs.unwrap_or(1))
                    .max(1),
            )
        })
    }
}

/// Add the resolved numbers to the child prompt so the model can cooperate
/// with the runtime supervisor instead of discovering the hard limit by being
/// cancelled. This applies to custom bounded agents as well as Oracle.
fn append_execution_budget_prompt(
    definition: &mut xai_grok_agent::config::AgentDefinition,
    budget: SubagentExecutionBudget,
) {
    if budget.is_unbounded() {
        return;
    }
    let mut limits = Vec::new();
    if let Some(turns) = budget.max_turns {
        limits.push(format!("{turns} model/tool-use rounds"));
    }
    if let Some(calls) = budget.max_tool_calls {
        limits.push(format!("{calls} tool calls"));
    }
    if let Some(seconds) = budget.timeout_secs {
        limits.push(format!("{seconds} seconds total wall-clock time"));
    }
    let reminder = format!(
        "\n\n<execution_budget>\nYour execution budget is {}. Build a hypothesis, inspect only discriminating evidence, and leave enough budget to produce the required final answer. When warned that the budget is nearly exhausted, call no more tools and answer immediately from the evidence already collected.\n</execution_budget>",
        limits.join(", ")
    );
    definition
        .prompt_body
        .get_or_insert_with(String::new)
        .push_str(&reminder);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubagentBudgetTrigger {
    FinalizingTurns,
    FinalizingToolCalls,
    FinalizingTimeout,
    MaxToolCalls,
    Timeout,
}

impl SubagentBudgetTrigger {
    fn code(self) -> u8 {
        match self {
            Self::FinalizingTurns => 1,
            Self::FinalizingToolCalls => 2,
            Self::FinalizingTimeout => 3,
            Self::MaxToolCalls => 4,
            Self::Timeout => 5,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::FinalizingTurns),
            2 => Some(Self::FinalizingToolCalls),
            3 => Some(Self::FinalizingTimeout),
            4 => Some(Self::MaxToolCalls),
            5 => Some(Self::Timeout),
            _ => None,
        }
    }

    fn termination_reason(self) -> &'static str {
        match self {
            Self::FinalizingTurns => "max_turns_finalize",
            Self::FinalizingToolCalls => "max_tool_calls_finalize",
            Self::FinalizingTimeout => "timeout_finalize",
            Self::MaxToolCalls => "max_tool_calls",
            Self::Timeout => "timeout",
        }
    }

    fn is_hard(self) -> bool {
        matches!(self, Self::MaxToolCalls | Self::Timeout)
    }
}

struct SubagentBudgetMonitor {
    state: Arc<std::sync::atomic::AtomicU8>,
    stop: CancellationToken,
}

impl SubagentBudgetMonitor {
    fn finish(self) -> Option<SubagentBudgetTrigger> {
        self.stop.cancel();
        SubagentBudgetTrigger::from_code(self.state.load(std::sync::atomic::Ordering::Acquire))
    }
}

fn budget_exhausted_message(
    trigger: SubagentBudgetTrigger,
    budget: SubagentExecutionBudget,
) -> String {
    match trigger {
        SubagentBudgetTrigger::MaxToolCalls => format!(
            "subagent tool-call budget exhausted (limit: {})",
            budget.max_tool_calls.unwrap_or_default()
        ),
        SubagentBudgetTrigger::Timeout => format!(
            "subagent wall-clock budget exhausted (limit: {}s)",
            budget.timeout_secs.unwrap_or_default()
        ),
        _ => "subagent execution budget requested finalization".to_string(),
    }
}

fn can_use_partial_budget_result(
    hard_budget_exhausted: bool,
    final_text: &str,
    structured_output_required: bool,
) -> bool {
    hard_budget_exhausted && !structured_output_required && !final_text.trim().is_empty()
}

fn budget_finalization_message(
    trigger: SubagentBudgetTrigger,
    budget: SubagentExecutionBudget,
) -> String {
    let reason = match trigger {
        SubagentBudgetTrigger::FinalizingTurns => format!(
            "the model/tool-use round budget is nearly exhausted ({})",
            budget.max_turns.unwrap_or_default()
        ),
        SubagentBudgetTrigger::FinalizingToolCalls => format!(
            "the tool-call budget is nearly exhausted ({}/{})",
            budget.finalize_at_tool_calls().unwrap_or_default(),
            budget.max_tool_calls.unwrap_or_default()
        ),
        SubagentBudgetTrigger::FinalizingTimeout => format!(
            "the wall-clock budget is nearly exhausted ({} seconds remain)",
            budget.finalize_grace_secs.unwrap_or_default()
        ),
        SubagentBudgetTrigger::MaxToolCalls | SubagentBudgetTrigger::Timeout => {
            "the execution budget is exhausted".to_string()
        }
    };
    format!(
        "<system-reminder>\n{reason}. Stop investigating now. Do not call any more tools. Return the best answer supported by the evidence already collected, follow the required output headings, state unknowns honestly, and include exact verification steps for the working agent.\n</system-reminder>"
    )
}

/// Watch a bounded child without modifying the generic session loop. Near a
/// limit, interject a no-more-tools finalization reminder at the next safe
/// model boundary. Hard tool/time limits still send Cancel + Shutdown signals,
/// so a model that ignores the reminder cannot run indefinitely.
fn spawn_subagent_budget_monitor(
    budget: SubagentExecutionBudget,
    child_handle: &SessionHandle,
    started_at: std::time::Instant,
    cancel_token: CancellationToken,
) -> Option<SubagentBudgetMonitor> {
    if budget.is_unbounded() {
        return None;
    }
    let state = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let stop = CancellationToken::new();
    let monitor = SubagentBudgetMonitor {
        state: state.clone(),
        stop: stop.clone(),
    };
    let signals_handle = child_handle.signals_handle.clone();
    let chat_state_handle = child_handle.chat_state_handle.clone();
    let cmd_tx = child_handle.cmd_tx.clone();
    tokio::task::spawn_local(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = cancel_token.cancelled() => break,
                _ = interval.tick() => {}
            }
            let elapsed = started_at.elapsed();
            let signals = signals_handle.snapshot().await.unwrap_or_default();
            let model_calls = chat_state_handle
                .try_get_session_usage()
                .await
                .map(|usage| usage.totals.model_calls)
                .unwrap_or_default();

            let hard = if budget
                .timeout_secs
                .is_some_and(|limit| elapsed >= std::time::Duration::from_secs(limit))
            {
                Some(SubagentBudgetTrigger::Timeout)
            } else if budget
                .max_tool_calls
                .is_some_and(|limit| signals.tool_call_count >= limit)
            {
                Some(SubagentBudgetTrigger::MaxToolCalls)
            } else {
                None
            };
            if let Some(trigger) = hard {
                state.store(trigger.code(), std::sync::atomic::Ordering::Release);
                let _ = cmd_tx.send(SessionCommand::Cancel(crate::session::CancelOptions {
                    cancel_subagents: true,
                    kill_background_tasks: true,
                    history: Default::default(),
                    trigger: Some(crate::session::CancelTrigger::Client(format!(
                        "subagent_{}",
                        trigger.termination_reason()
                    ))),
                    user_initiated: false,
                }));
                cancel_token.cancel();
                break;
            }

            if state.load(std::sync::atomic::Ordering::Acquire) != 0 {
                continue;
            }
            let soft = if budget
                .finalize_at_elapsed()
                .is_some_and(|limit| elapsed >= limit)
            {
                Some(SubagentBudgetTrigger::FinalizingTimeout)
            } else if budget
                .finalize_at_tool_calls()
                .is_some_and(|limit| signals.tool_call_count >= limit)
            {
                Some(SubagentBudgetTrigger::FinalizingToolCalls)
            } else if budget
                .finalize_at_model_calls()
                .is_some_and(|limit| model_calls >= limit)
            {
                Some(SubagentBudgetTrigger::FinalizingTurns)
            } else {
                None
            };
            if let Some(trigger) = soft
                && state
                    .compare_exchange(
                        0,
                        trigger.code(),
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    )
                    .is_ok()
            {
                let _ = cmd_tx.send(SessionCommand::Interject {
                    text: budget_finalization_message(trigger, budget),
                    id: Some(format!("subagent-budget-{}", trigger.termination_reason())),
                    images: Vec::new(),
                });
            }
        }
    });
    Some(monitor)
}

/// What to do with a resumed subagent's isolated worktree directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResumeWorktreeAction {
    /// Existing real directory on disk — reuse after path validation.
    Reuse,
    /// Directory missing but a snapshot ref exists — rehydrate from it.
    Rehydrate,
    /// Directory missing and no snapshot — caller must fail closed (no shared
    /// workspace fallback when isolation was recorded on the source).
    Shared,
}
/// Decide how to recover a resumed subagent's worktree from its on-disk state
/// and whether a durable snapshot is available. Pure so the three outcomes are
/// unit-testable without git/async.
///
/// Priority: reuse an existing directory first; only rehydrate when the dir is
/// missing *and* a snapshot is available; otherwise signal fail-closed `Shared`.
///
/// `Shared` is a *signal that recovery is impossible*, not permission to share
/// the parent workspace: `handle_request` aborts the spawn fail-closed.
fn resume_worktree_action(dir_exists: bool, snapshot_ref: Option<&str>) -> ResumeWorktreeAction {
    if dir_exists {
        ResumeWorktreeAction::Reuse
    } else if snapshot_ref.is_some() {
        ResumeWorktreeAction::Rehydrate
    } else {
        ResumeWorktreeAction::Shared
    }
}

/// Filesystem identity of a validated worktree (for TOCTOU rechecks).
///
/// On Unix this is `(dev, ino)`. On Windows we fall back to the canonical
/// path string plus file metadata size/modified when available — this cannot
/// fully eliminate same-user rename races without platform openat APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeIdentity {
    pub path: std::path::PathBuf,
    #[cfg(unix)]
    pub dev: u64,
    #[cfg(unix)]
    pub ino: u64,
    #[cfg(not(unix))]
    pub modified_ms: Option<u128>,
}

impl WorktreeIdentity {
    fn capture(path: &std::path::Path) -> Result<Self, String> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| {
            format!(
                "failed to read metadata for worktree '{}': {e}",
                path.display()
            )
        })?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "worktree '{}' became a symbolic link during validation",
                path.display()
            ));
        }
        if !meta.is_dir() {
            return Err(format!("worktree '{}' is not a directory", path.display()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                path: path.to_path_buf(),
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let modified_ms = meta.modified().ok().and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis())
            });
            Ok(Self {
                path: path.to_path_buf(),
                modified_ms,
            })
        }
    }

    /// Re-stat `path` and ensure it still refers to the same directory object.
    pub(crate) fn matches_path(&self, path: &std::path::Path) -> Result<(), String> {
        let now = Self::capture(path)?;
        if now.path != self.path {
            // Compare via re-canonicalize of the live path.
            let live = dunce::canonicalize(path)
                .map_err(|e| format!("re-canonicalize of '{}' failed: {e}", path.display()))?;
            if live != self.path {
                return Err(format!(
                    "worktree path changed: expected '{}', now '{}'",
                    self.path.display(),
                    live.display()
                ));
            }
        }
        #[cfg(unix)]
        {
            if now.dev != self.dev || now.ino != self.ino {
                return Err(format!(
                    "worktree inode replaced at '{}' (was dev={} ino={}, now dev={} ino={})",
                    self.path.display(),
                    self.dev,
                    self.ino,
                    now.dev,
                    now.ino
                ));
            }
        }
        #[cfg(not(unix))]
        {
            if now.modified_ms != self.modified_ms {
                return Err(format!(
                    "worktree metadata changed at '{}' (possible replacement)",
                    self.path.display()
                ));
            }
        }
        Ok(())
    }
}

/// Pure Unix mode check for managed worktree **leaf** bases: owner-only access
/// (no group/world bits). Extracted for unit tests without needing root.
#[cfg(unix)]
pub(crate) fn unix_mode_is_owner_only(mode: u32) -> bool {
    // Ignore file-type bits in the high nibble; check permission bits only.
    (mode & 0o077) == 0
}

/// Pure: group/world write bits must be zero (0755 ok; 0775/0777 not).
#[cfg(unix)]
pub(crate) fn unix_mode_no_group_world_write(mode: u32) -> bool {
    (mode & 0o022) == 0
}

/// Pure: sticky bit set (`S_ISVTX` = 0o1000).
#[cfg(unix)]
pub(crate) fn unix_mode_has_sticky(mode: u32) -> bool {
    (mode & 0o1000) != 0
}

/// Pure policy for one parent-chain component (not the leaf base itself).
///
/// Rules:
/// - owner must be `euid` or root (0)
/// - group/world write bits must be 0, **except** root-owned + sticky
///   (classic `/tmp` = `drwxrwxrwt`)
#[cfg(unix)]
pub(crate) fn unix_parent_component_is_safe(owner_uid: u32, mode: u32, euid: u32) -> bool {
    let is_root = owner_uid == 0;
    let is_self = owner_uid == euid;
    if !is_root && !is_self {
        return false;
    }
    if unix_mode_no_group_world_write(mode) {
        return true;
    }
    // Only root-owned sticky may retain world/group write (e.g. /tmp).
    is_root && unix_mode_has_sticky(mode)
}

/// Pure policy for XDG_RUNTIME_DIR (or similar runtime root) itself.
/// Must be owned by euid, non-symlink (checked elsewhere), and either
/// owner-only or at least free of group/world write.
#[cfg(unix)]
pub(crate) fn unix_xdg_runtime_dir_mode_ok(mode: u32) -> bool {
    unix_mode_is_owner_only(mode) || unix_mode_no_group_world_write(mode)
}

/// Walk every existing ancestor of `path` (not including `path` itself if it
/// does not yet exist) and enforce the Unix parent-chain policy.
///
/// Each component must be a non-symlink directory owned by eUID or root,
/// without group/world write unless root-owned + sticky.
#[cfg(unix)]
pub(crate) fn validate_unix_parent_chain(path: &std::path::Path) -> Result<(), String> {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;

    let euid = unsafe { libc::geteuid() };
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot resolve cwd for parent chain: {e}"))?
            .join(path)
    };

    // Collect ancestors from root down to parent of `path`.
    let mut acc = std::path::PathBuf::new();
    let components: Vec<_> = abs.components().collect();
    // Drop the final component (the leaf); validate parents only.
    let parent_comps = if components.len() > 1 {
        &components[..components.len() - 1]
    } else {
        &components[..]
    };

    for component in parent_comps {
        acc.push(component.as_os_str());
        match component {
            Component::RootDir | Component::Prefix(_) => continue,
            _ => {}
        }
        let meta = fs::symlink_metadata(&acc)
            .map_err(|e| format!("cannot lstat parent component '{}': {e}", acc.display()))?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "parent component '{}' is a symbolic link; refusing",
                acc.display()
            ));
        }
        if !meta.is_dir() {
            return Err(format!(
                "parent component '{}' is not a directory; refusing",
                acc.display()
            ));
        }
        let mode = meta.mode();
        let owner = meta.uid();
        if !unix_parent_component_is_safe(owner, mode, euid) {
            return Err(format!(
                "parent component '{}' owner={} mode={:o} is not a safe parent \
                 (need owner euid/root, no group/world write unless root+sticky)",
                acc.display(),
                owner,
                mode & 0o7777
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn validate_unix_parent_chain(_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}

/// True when `dir` is a usable XDG_RUNTIME_DIR: real dir, owner=euid, safe mode,
/// no symlink components on the path, and parents satisfy the safe parent chain.
#[cfg(unix)]
fn xdg_runtime_dir_is_usable(dir: &std::path::Path) -> bool {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    if dir.as_os_str().is_empty() {
        return false;
    }
    let abs = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(dir),
            Err(_) => return false,
        }
    };
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return false;
    }
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return false;
    }
    if !unix_xdg_runtime_dir_mode_ok(meta.mode()) {
        return false;
    }
    // Reject symlink ancestors of XDG itself.
    if reject_symlink_components(&abs).is_err() {
        return false;
    }
    // Parents of XDG must be safe (root 0755 / sticky /tmp ok). XDG itself is
    // checked above as euid-owned with no g/w write.
    if validate_unix_parent_chain(&abs).is_err() {
        return false;
    }
    true
}

/// User-private home fallback: `~/.grok/subagent-worktrees` (per-user, not /tmp).
fn subagent_home_worktree_base() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".grok").join("subagent-worktrees"))
}

/// Per-UID private temp/home root for subagent worktrees.
///
/// Selection order (Unix):
/// 1. `XDG_RUNTIME_DIR/grok-subagent-worktrees-<uid>` only if XDG_RUNTIME_DIR
///    is a real dir, owner=eUID, mode owner-only (or no g/w write), no symlink chain.
///    Insecure XDG → soft-skip (do not Err).
/// 2. `temp_dir()/grok-subagent-worktrees-<uid>` only if the temp **parent**
///    satisfies the safe parent chain (root-owned sticky `/tmp` ok).
/// 3. Else `~/.grok/subagent-worktrees` (user-private home path).
///
/// Never uses a fixed shared name under `/tmp`.
///
/// **Windows:** path namespaced by username; ACL not enforced (degraded).
pub(crate) fn subagent_temp_worktree_base() -> std::path::PathBuf {
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        let leaf = format!("grok-subagent-worktrees-{uid}");

        if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            let p = std::path::PathBuf::from(&runtime);
            if xdg_runtime_dir_is_usable(&p) {
                return p.join(&leaf);
            }
            tracing::warn!(
                xdg_runtime_dir = %runtime,
                "XDG_RUNTIME_DIR unsafe for subagent worktrees; falling back"
            );
        }

        let temp_candidate = std::env::temp_dir().join(&leaf);
        // Parent of temp_candidate must be a safe parent chain (e.g. /tmp sticky).
        if validate_unix_parent_chain(&temp_candidate).is_ok() {
            return temp_candidate;
        }
        tracing::warn!(
            temp = %temp_candidate.display(),
            "temp parent chain unsafe for subagent worktrees; using home fallback"
        );

        if let Some(home_base) = subagent_home_worktree_base() {
            return home_base;
        }
        // Last resort: still return temp path (ensure_real_dir / parent chain
        // will fail closed at use time if unsafe).
        temp_candidate
    }
    #[cfg(not(unix))]
    {
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "user".into());
        let safe: String = user
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        std::env::temp_dir().join(format!("grok-subagent-worktrees-{safe}"))
    }
}

/// Ensure `path` exists as a real (non-symlink) directory suitable as a managed
/// worktree **leaf** base, and that its parent chain is safe.
///
/// On Unix, **every** use requires for the leaf:
/// - owner == current eUID
/// - mode has no group/world bits (effectively `0o700`)
///
/// And for every parent component:
/// - non-symlink directory
/// - owner eUID or root
/// - no group/world write unless root-owned + sticky (`/tmp`)
///
/// If the directory is newly created, `chmod 0700` **must** succeed (`Err` on
/// failure). Existing insecure leaves/parents are **rejected**, never
/// auto-chmod'd into acceptance.
///
/// **Windows:** only non-symlink directory existence; NTFS ACLs degraded.
pub(crate) fn ensure_real_dir(path: &std::path::Path) -> Result<(), String> {
    use std::fs;

    // Parent chain first (before create) so we never mkdir under an unsafe parent.
    validate_unix_parent_chain(path)?;

    let created = !path.exists();
    if created {
        // Ensure parents exist only if parent chain already validated for the
        // full path; create_dir_all may create intermediate dirs — those are
        // owned by us. Re-validate after create.
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create parent of managed worktree base '{}': {e}",
                    path.display()
                )
            })?;
            // Intermediate parents we just created must be owner-only too
            // if they sit under a sticky /tmp (e.g. /tmp/foo created by us).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Only chmod the deepest missing segment chain we own.
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        fs::create_dir(path)
            .or_else(|_| {
                // Race: another process created it.
                if path.is_dir() {
                    Ok(())
                } else {
                    fs::create_dir_all(path).map(|_| ())
                }
            })
            .map_err(|e| {
                format!(
                    "failed to create managed worktree base '{}': {e}",
                    path.display()
                )
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| {
                format!(
                    "failed to chmod 0700 managed worktree base '{}': {e}",
                    path.display()
                )
            })?;
        }
    }
    let meta = fs::symlink_metadata(path).map_err(|e| {
        format!(
            "managed worktree base '{}' is not accessible: {e}",
            path.display()
        )
    })?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "managed worktree base '{}' is a symbolic link; refusing",
            path.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "managed worktree base '{}' is not a directory",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(format!(
                "managed worktree base '{}' owner uid {} != current euid {}; refusing \
                 (will not chown/chmod an existing insecure base)",
                path.display(),
                meta.uid(),
                euid
            ));
        }
        let mode = meta.mode();
        if !unix_mode_is_owner_only(mode) {
            return Err(format!(
                "managed worktree base '{}' mode {:o} has group/world bits; \
                 refusing (expected owner-only 0700; will not auto-chmod)",
                path.display(),
                mode & 0o777
            ));
        }
        // Parent chain again after create (intermediates may have appeared).
        validate_unix_parent_chain(path)?;
    }
    let _ = created;
    Ok(())
}

/// Walk every component from the filesystem root (or relative start) to
/// `path` and reject any symlink component. `path` must already exist.
fn reject_symlink_components(path: &std::path::Path) -> Result<(), String> {
    use std::fs;
    let mut acc = std::path::PathBuf::new();
    // Absolute paths: start empty and rebuild. Relative: start from cwd component.
    for component in path.components() {
        acc.push(component.as_os_str());
        // Skip root/prefix-only components that may not support lstat the same way.
        use std::path::Component;
        match component {
            Component::RootDir | Component::Prefix(_) => continue,
            _ => {}
        }
        match fs::symlink_metadata(&acc) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "path component '{}' is a symbolic link; refusing worktree under it",
                    acc.display()
                ));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(format!(
                    "cannot lstat path component '{}': {e}",
                    acc.display()
                ));
            }
        }
    }
    Ok(())
}

/// True when `child` is a strict descendant of `base` (both already canonical).
fn is_strict_descendant(child: &std::path::Path, base: &std::path::Path) -> bool {
    child.starts_with(base) && child != base
}

/// Validate a resumed (or freshly created) subagent worktree path.
///
/// Fail-closed (no lexical `starts_with` success path):
/// 1. `dest` must exist, not be a symlink, and be a directory.
/// 2. Every path component to `dest` must be non-symlink.
/// 3. Canonicalize `dest`; must still be a directory.
/// 4. Allowed managed bases must exist (created if needed), not be symlinks,
///    and canonicalize successfully; every base component non-symlink.
/// 5. Canonical dest must be a **strict** descendant of a canonical base.
/// 6. Dest must not equal parent session cwd.
/// 7. Basename must be exactly `subagent-{id}` when `subagent_id` is provided
///    (production naming; no unprefixed legacy directories).
///
/// Returns a [`WorktreeIdentity`] (canonical path + Unix dev/ino) for callers
/// to recheck immediately before child start.
///
/// **TOCTOU honesty:** same-user rename races cannot be fully eliminated
/// without openat/O_NOFOLLOW across platforms; the inode recheck narrows the
/// window but is not a capability-style handle.
pub(crate) fn validate_subagent_worktree_path(
    dest: &std::path::Path,
    source_cwd: &std::path::Path,
    parent_cwd: &std::path::Path,
    subagent_id: Option<&str>,
) -> Result<WorktreeIdentity, String> {
    use std::fs;

    // 1. dest exists, not symlink, is dir.
    match fs::symlink_metadata(dest) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "subagent worktree path '{}' is a symbolic link; refusing to use it",
                    dest.display()
                ));
            }
            if !meta.is_dir() {
                return Err(format!(
                    "subagent worktree path '{}' is not a directory",
                    dest.display()
                ));
            }
        }
        Err(e) => {
            return Err(format!(
                "subagent worktree path '{}' is not accessible: {e}",
                dest.display()
            ));
        }
    }

    // 2. No symlink components on the path to dest (use absolute form when possible).
    let abs_dest = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot resolve cwd for relative worktree: {e}"))?
            .join(dest)
    };
    reject_symlink_components(&abs_dest)?;

    // 3. Canonicalize dest.
    let canonical = dunce::canonicalize(dest).map_err(|e| {
        format!(
            "failed to canonicalize subagent worktree '{}': {e}",
            dest.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "canonicalized worktree '{}' is not a directory",
            canonical.display()
        ));
    }
    // Re-check components on the canonical path too.
    reject_symlink_components(&canonical)?;

    // 4–5. Managed bases: create if needed, non-symlink, Unix owner+0700,
    // canonicalize, strict contain. Temp fallback is per-UID private.
    let mut allowed_bases: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(base) = crate::session::worktree::worktree_base_dir_for_source(source_cwd) {
        allowed_bases.push(base);
    }
    allowed_bases.push(subagent_temp_worktree_base());

    let mut matched_base: Option<std::path::PathBuf> = None;
    let mut base_errors: Vec<String> = Vec::new();
    for base in &allowed_bases {
        if let Err(e) = ensure_real_dir(base) {
            base_errors.push(e);
            continue;
        }
        if let Err(e) = reject_symlink_components(base) {
            base_errors.push(e);
            continue;
        }
        let base_canon = match dunce::canonicalize(base) {
            Ok(p) => p,
            Err(e) => {
                base_errors.push(format!(
                    "failed to canonicalize managed base '{}': {e}",
                    base.display()
                ));
                continue;
            }
        };
        if let Err(e) = reject_symlink_components(&base_canon) {
            base_errors.push(e);
            continue;
        }
        // Re-verify owner+mode on the canonical base path (not just the
        // pre-canonical lexical path).
        if let Err(e) = ensure_real_dir(&base_canon) {
            base_errors.push(e);
            continue;
        }
        if is_strict_descendant(&canonical, &base_canon) {
            matched_base = Some(base_canon);
            break;
        }
    }
    if matched_base.is_none() {
        let detail = if base_errors.is_empty() {
            "not a strict descendant of any managed base".to_string()
        } else {
            format!(
                "not under a valid managed base ({})",
                base_errors.join("; ")
            )
        };
        return Err(format!(
            "subagent worktree '{}' is outside managed worktree bases \
             (expected under ~/.grok/worktrees/... or per-UID temp \
             grok-subagent-worktrees-<uid>): {detail}",
            canonical.display()
        ));
    }

    // 6. Not parent cwd (equality only; containment under parent without a
    // managed base already fails step 5).
    if let Ok(parent_canon) = dunce::canonicalize(parent_cwd)
        && canonical == parent_canon
    {
        return Err(format!(
            "subagent worktree '{}' resolves to the parent session cwd; \
             isolation refused",
            canonical.display()
        ));
    }
    // Silence unused warning when all bases failed before assignment use.
    let _ = matched_base;

    // 7. Strict basename identity: production format is always `subagent-{id}`.
    if let Some(id) = subagent_id {
        if !xai_tool_types::is_safe_task_id(id) {
            return Err(format!(
                "subagent id {id:?} is not a safe path segment for worktree naming"
            ));
        }
        let expected = format!("subagent-{id}");
        let name = canonical.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name != expected {
            return Err(format!(
                "subagent worktree basename '{name}' must be exactly '{expected}' \
                 (agent metadata must not point at another subagent's directory)"
            ));
        }
    }

    // Capture identity for pre-spawn recheck.
    WorktreeIdentity::capture(&canonical)
}

/// Final pre-spawn check: re-validate path and confirm identity still matches.
pub(crate) fn recheck_worktree_identity(
    expected: &WorktreeIdentity,
    source_cwd: &std::path::Path,
    parent_cwd: &std::path::Path,
    subagent_id: Option<&str>,
) -> Result<WorktreeIdentity, String> {
    let again =
        validate_subagent_worktree_path(&expected.path, source_cwd, parent_cwd, subagent_id)?;
    expected.matches_path(&again.path)?;
    #[cfg(unix)]
    {
        if again.dev != expected.dev || again.ino != expected.ino {
            return Err(format!(
                "worktree identity changed before spawn at '{}' \
                 (dev/ino {}/{} → {}/{})",
                expected.path.display(),
                expected.dev,
                expected.ino,
                again.dev,
                again.ino
            ));
        }
    }
    Ok(again)
}

/// Whether agent-memory write tools (`search_replace` / `write`) may be
/// injected for a subagent under `capability_mode`.
///
/// Read-only and execute-only modes must not gain memory write tools;
/// `None` (no ceiling) and explicit write modes may.
pub(crate) fn memory_writes_allowed(
    capability_mode: Option<xai_tool_types::SubagentCapabilityMode>,
) -> bool {
    matches!(
        capability_mode,
        None | Some(xai_tool_types::SubagentCapabilityMode::ReadWrite)
            | Some(xai_tool_types::SubagentCapabilityMode::All)
    )
}

/// Apply memory-tool injection + capability re-clamp on a definition.
///
/// Mirrors the production path in `handle_request` so tests can assert the
/// final tool list after inject → filter without spinning a full child.
pub(crate) fn apply_memory_tools_to_definition(
    definition: &mut xai_grok_agent::config::AgentDefinition,
    capability_mode: Option<xai_tool_types::SubagentCapabilityMode>,
    allow_nested_subagents: bool,
) {
    use xai_grok_tools::implementations::{grok_build, opencode};

    let allow_writes = memory_writes_allowed(capability_mode);
    let mut memory_tools: Vec<xai_grok_tools::registry::types::ToolConfig> =
        vec![(&grok_build::ReadFileTool).into()];
    if allow_writes {
        memory_tools.push((&grok_build::SearchReplaceTool).into());
        memory_tools.push((&opencode::OpenCodeWriteTool).into());
    }
    for tc in memory_tools {
        if !definition.tool_config.tools.iter().any(|t| t.id == tc.id) {
            definition.tool_config.tools.push(tc);
        }
    }
    if let Some(mode) = capability_mode {
        xai_grok_subagent_resolution::apply_child_tool_policy(
            definition,
            Some(mode),
            allow_nested_subagents,
        );
    }
}
/// The parent session's working directory — the source path for a subagent
/// worktree. Prefers the reconstructed `SessionInfo` cwd, falling back to
/// `parent_cwd`.
fn parent_source_cwd(ctx: &SubagentSpawnContext) -> std::path::PathBuf {
    ctx.parent_session_info
        .as_ref()
        .map(|i| std::path::PathBuf::from(&i.cwd))
        .unwrap_or_else(|| std::path::PathBuf::from(&ctx.parent_cwd))
}
/// Effective permission mode for a spawned subagent. Plugin agents never honor a
/// non-default mode; under the pin, `bypassPermissions` downgrades to `Default`
/// so a repo/profile/`--agents` def can't restore auto-approve. Caller logs it.
fn resolve_subagent_permission_mode(
    requested: xai_grok_agent::config::PermissionMode,
    is_plugin: bool,
    policy_block: Option<&'static str>,
) -> xai_grok_agent::config::PermissionMode {
    if is_plugin {
        return PermissionMode::Default;
    }
    if policy_block.is_some() && requested == PermissionMode::BypassPermissions {
        return PermissionMode::Default;
    }
    requested
}
/// Main repo root for a subagent's source: the durable repo a completion snapshot is transferred into and the repo a resume rehydrates from — both arms MUST resolve this identically.
fn resolve_subagent_source_repo(ctx: &SubagentSpawnContext) -> std::path::PathBuf {
    let source_cwd = parent_source_cwd(ctx);
    xai_grok_workspace::session::git::find_main_repo_root_from_path(&source_cwd)
        .unwrap_or(source_cwd)
}
enum SubagentWaitOutcome {
    Cancelled,
    TurnResult(Box<Result<SubagentPromptTurnResult, oneshot::error::RecvError>>),
}
async fn await_subagent_turn_or_cancellation(
    prompt_rx: oneshot::Receiver<SubagentPromptTurnResult>,
    cancel_token: CancellationToken,
) -> SubagentWaitOutcome {
    tokio::select! {
        _ = cancel_token.cancelled() => SubagentWaitOutcome::Cancelled,
        turn_result = prompt_rx => SubagentWaitOutcome::TurnResult(Box::new(turn_result)),
    }
}
async fn signals_snapshot_counts(child_handle: &SessionHandle) -> (u32, u32) {
    child_handle
        .signals_handle
        .snapshot()
        .await
        .map(|snapshot| (snapshot.tool_call_count, snapshot.turn_count))
        .unwrap_or((0, 0))
}
fn cancellation_error_message(
    category: Option<xai_grok_session_events::types::CancellationCategory>,
    context: Option<&crate::session::commands::CancellationContext>,
) -> String {
    let detail = context.and_then(|ctx| {
        let tool = ctx.tool_name.as_deref();
        let reason = ctx.reason.as_deref();
        let hook = ctx.hook_name.as_deref();
        match (tool, reason, hook) {
            (Some(t), Some(r), Some(h)) => Some(format!("{r} for tool `{t}` (hook: {h})")),
            (Some(t), Some(r), None) => Some(format!("{r} for tool `{t}`")),
            (Some(t), None, _) => Some(format!("tool `{t}`")),
            _ => None,
        }
    });
    match (category, &detail) {
        (Some(CancellationCategory::PermissionRejected), Some(d)) => {
            format!("Subagent turn was cancelled: user rejected permission — {d}")
        }
        (Some(CancellationCategory::PermissionRejected), None) => {
            "Subagent turn was cancelled: user rejected a permission prompt".to_string()
        }
        (Some(CancellationCategory::PermissionCancelled), _) => {
            "Subagent turn was cancelled: user cancelled a permission prompt".to_string()
        }
        (Some(CancellationCategory::HookDenied), Some(d)) => {
            format!("Subagent turn was cancelled: hook denied — {d}")
        }
        (Some(CancellationCategory::HookDenied), None) => {
            "Subagent turn was cancelled: blocked by a hook".to_string()
        }
        (Some(CancellationCategory::MidTurnAbort), _) => {
            "Subagent turn was cancelled: aborted mid-turn".to_string()
        }
        _ => "Subagent turn was cancelled".to_string(),
    }
}
fn telemetry_owner_kind(
    request: &SubagentRequest,
) -> xai_grok_telemetry::events::SubagentOwnerKind {
    if request.owner.is_workflow() {
        xai_grok_telemetry::events::SubagentOwnerKind::Workflow
    } else if request.from_scheduler_loop() {
        xai_grok_telemetry::events::SubagentOwnerKind::SchedulerLoop
    } else {
        xai_grok_telemetry::events::SubagentOwnerKind::Task
    }
}
fn failure_result(request: &SubagentRequest, error: &str) -> SubagentResult {
    SubagentResult {
        success: false,
        error: Some(error.to_string()),
        subagent_id: request.id.clone(),
        child_session_id: request.id.clone(),
        ..Default::default()
    }
}
fn cancelled_result(request: &SubagentRequest, error: &str) -> SubagentResult {
    SubagentResult {
        success: false,
        cancelled: true,
        error: Some(error.to_string()),
        subagent_id: request.id.clone(),
        child_session_id: request.id.clone(),
        ..Default::default()
    }
}
fn child_run_output(
    result: SubagentResult,
    completion_data: ShellCompletionData,
    snapshot_ref: Option<String>,
) -> ChildRunOutput<ShellCompletionData> {
    ChildRunOutput {
        result,
        completion_data,
        snapshot_ref,
    }
}
/// Persist a failure after `SubagentSpawned`; lifecycle delivery stays actor-owned.
fn fail_subagent(
    error: &str,
    subagent_id: &str,
    child_session_id: &acp::SessionId,
    subagent_meta_dir: &Path,
    duration_ms: u64,
    gcs_ctx: &GcsUploadContext,
) -> SubagentResult {
    let result = SubagentResult {
        success: false,
        error: Some(error.to_string()),
        subagent_id: subagent_id.to_string(),
        child_session_id: child_session_id.0.to_string(),
        duration_ms,
        ..Default::default()
    };
    persist_subagent_completion(subagent_meta_dir, &result, gcs_ctx);
    result
}
/// Tear down a child whose pending-to-active promotion lost to cancellation.
#[allow(clippy::too_many_arguments)]
async fn cancel_pending_shell_child(
    child_cmd_tx: &mpsc::UnboundedSender<SessionCommand>,
    subagent_id: &str,
    child_session_id: &acp::SessionId,
    subagent_meta_dir: &Path,
    worktree_path: Option<&Path>,
    worktree_freshly_created: bool,
    duration_ms: u64,
    gcs_ctx: &GcsUploadContext,
) -> SubagentResult {
    let _ = child_cmd_tx.send(SessionCommand::Shutdown(
        crate::session::ShutdownKind::Graceful,
    ));
    if worktree_freshly_created
        && let Some(wt_path) = worktree_path
        && let Err(e) = crate::session::worktree::remove_subagent_worktree(wt_path).await
    {
        tracing::warn!(
            subagent_id,
            worktree_path = %wt_path.display(),
            error = %e,
            "failed to remove pristine worktree for killed-while-pending subagent"
        );
    }
    let result = SubagentResult {
        success: false,
        cancelled: true,
        error: Some("Subagent was cancelled".to_string()),
        subagent_id: subagent_id.to_string(),
        child_session_id: child_session_id.0.to_string(),
        duration_ms,
        ..Default::default()
    };
    persist_subagent_completion(subagent_meta_dir, &result, gcs_ctx);
    result
}
/// Progress notification emission interval.
const PROGRESS_PUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Change signature for the progress-publisher dedupe:
/// `(turn_count, tool_call_count, context_usage_pct, error_count, tokens_used)`.
///
/// `tokens_used` is part of the signature so rising child token spend always
/// publishes a tick: goal token accounting (subagent records, live totals,
/// and the turn-end budget check) keys off prompt token movement, which can
/// climb while turn/tool counts and the coarse context-usage *percent* bucket
/// stay flat. Omitting it would stall those updates until the heartbeat or an
/// unrelated field moved.
type ProgressSignature = (u32, u32, u8, u32, u64);
/// Whether a progress tick should be emitted given the previous and current
/// [`ProgressSignature`]s. Emits on any change, or when `heartbeat_due`
/// forces a keep-alive after an idle gap.
fn progress_tick_should_emit(
    prev: ProgressSignature,
    cur: ProgressSignature,
    heartbeat_due: bool,
) -> bool {
    cur != prev || heartbeat_due
}
/// Parent-actor tick channel for [`spawn_progress_publisher`]: goal token
/// accounting is the only consumer, so a goal-disabled session sends no
/// per-tick commands at all.
fn goal_tick_cmd_tx(
    goal_enabled: bool,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
) -> Option<mpsc::UnboundedSender<SessionCommand>> {
    if goal_enabled {
        parent_cmd_tx.cloned()
    } else {
        None
    }
}
/// Spawn a background task that periodically emits `SubagentProgress`
/// notifications on the parent session's notification channel.
///
/// The publisher samples the child's `SessionSignalsHandle` every
/// [`PROGRESS_PUBLISH_INTERVAL`] and emits a `SubagentProgress`
/// notification if the subagent is still running. It stops automatically
/// when `cancel_token` is cancelled (subagent completion/cancellation).
///
/// When `parent_cmd_tx` is `Some`, each tick is also delivered to the
/// parent `SessionActor` so goal mode can advance its live subagent
/// token accounting; the actor's `SubagentProgress` arm never persists
/// these ticks.
///
/// Notifications are **not** persisted to JSONL — they are transient UI
/// hints, not authoritative lifecycle events. The TUI can resync via
/// `x.ai/subagent/list_running` on reconnect.
#[allow(clippy::too_many_arguments)]
fn spawn_progress_publisher(
    signals_handle: crate::session::signals::SessionSignalsHandle,
    gateway: GatewaySender,
    parent_session_id: String,
    subagent_id: String,
    child_session_id: String,
    started_at: std::time::Instant,
    cancel_token: tokio_util::sync::CancellationToken,
    parent_cmd_tx: Option<mpsc::UnboundedSender<SessionCommand>>,
) -> tokio_util::task::AbortOnDropHandle<()> {
    tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        let mut interval = tokio::time::interval(PROGRESS_PUBLISH_INTERVAL);
        interval.tick().await;
        let mut last_signature: ProgressSignature = (0, 0, 0, 0, 0);
        let mut last_emit_at = tokio::time::Instant::now();
        let heartbeat_max = tokio::time::Duration::from_secs(8);
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                _ = interval.tick() => {}
            }
            let signals = match signals_handle.snapshot().await {
                Some(s) => s,
                None => break,
            };
            let sig: ProgressSignature = (
                signals.turn_count,
                signals.tool_call_count,
                signals.context_window_usage,
                signals.error_count,
                signals.context_tokens_used,
            );
            let heartbeat_due = last_emit_at.elapsed() >= heartbeat_max;
            if !progress_tick_should_emit(last_signature, sig, heartbeat_due) {
                continue;
            }
            last_signature = sig;
            last_emit_at = tokio::time::Instant::now();
            let duration_ms = started_at.elapsed().as_millis() as u64;
            let update = SessionUpdate::SubagentProgress {
                subagent_id: subagent_id.clone(),
                parent_session_id: parent_session_id.clone(),
                child_session_id: child_session_id.clone(),
                duration_ms,
                turn_count: signals.turn_count,
                tool_call_count: signals.tool_call_count,
                tokens_used: signals.context_tokens_used,
                context_window_tokens: signals.context_window_tokens,
                context_usage_pct: signals.context_window_usage,
                tools_used: signals.tools_used,
                error_count: signals.error_count,
            };
            let notification = SessionNotification {
                session_id: acp::SessionId::new(parent_session_id.clone()),
                update,
                meta: None,
            };
            let params = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
                .ok();
            if let Some(ref cmd_tx) = parent_cmd_tx {
                let _ = cmd_tx.send(SessionCommand::XaiSessionNotification { notification });
            }
            if let Some(params) = params {
                let ext_notification =
                    acp::ExtNotification::new("x.ai/session_notification", params.into());
                gateway.forward_fire_and_forget(ext_notification);
            }
        }
    }))
}
#[cfg(test)]
mod progress_publisher_tests {
    use super::{ProgressSignature, progress_tick_should_emit};
    const BASE: ProgressSignature = (3, 7, 12, 0, 30_000);
    #[test]
    fn token_only_change_emits() {
        let cur: ProgressSignature = (3, 7, 12, 0, 45_000);
        assert!(progress_tick_should_emit(BASE, cur, false));
    }
    #[test]
    fn unchanged_without_heartbeat_skips() {
        assert!(!progress_tick_should_emit(BASE, BASE, false));
    }
    #[test]
    fn heartbeat_forces_emit_when_unchanged() {
        assert!(progress_tick_should_emit(BASE, BASE, true));
    }
}
/// Metadata stored as `meta.json` in the child session directory.
/// Links the child session back to its parent.
///
/// For the GCS-persisted artifact (`subagent.json`), see [`SubagentSessionMetadata`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SubagentMeta {
    pub subagent_id: String,
    pub parent_session_id: String,
    pub child_session_id: String,
    pub subagent_type: String,
    pub description: String,
    pub prompt: String,
    /// "running" | "completed" | "failed" | "cancelled"
    pub status: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Effective context source after bootstrap: "new" or "resumed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_context_source: Option<String>,
    /// True only for a summarized (normalized) fork; false for verbatim
    /// mirror-forks, resume, and new sessions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub context_normalized: bool,
    /// Error message if fork-copy failed and fell back to fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_copy_error: Option<String>,
    /// Named persona applied to this subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// ID of the source subagent this session was resumed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    /// Effective cwd used by the child session. Persisted for durable
    /// `resume_from` reconstruction after in-memory cache eviction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_cwd: Option<String>,
    /// Worktree path if the child used `isolation=worktree`. Persisted
    /// for durable `resume_from` reconstruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// Durable git ref holding a snapshot of the child's worktree working
    /// state. Persisted so a deleted worktree can be rehydrated on resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_ref: Option<String>,
    /// Effective model ID used by the child session. Persisted for
    /// durable `resume_from` identity validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_model_id: Option<String>,
}
/// Canonical subagent metadata for GCS persistence (`subagent.json`).
///
/// Contains the full subagent identity, provenance, and execution state.
/// Uploaded to `{session_id}/subagent.json` in GCS and optionally mirrored
/// locally. Schema is versioned for forward compatibility.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubagentSessionMetadata {
    pub schema_version: u32,
    pub session_id: String,
    pub session_kind: String,
    pub subagent_id: String,
    pub child_session_id: String,
    pub parent_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_prompt_id: Option<String>,
    pub subagent_type: String,
    /// Human-readable spawn description: the task tool's `description`
    /// argument, or the fixed role label for harness-spawned goal subagents
    /// ("goal plan writer", "goal achievement skeptic", ...). All goal roles
    /// share `subagent_type = "general-purpose"`, so this is what identifies
    /// them in the artifact.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default)]
    pub context_normalized: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation_mode: Option<String>,
    #[serde(default)]
    pub depth: u32,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_copy_error: Option<String>,
    /// ID of the source subagent this session was resumed from (`resume_from`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
}
impl SubagentSessionMetadata {
    /// Current schema version.
    pub(crate) const SCHEMA_VERSION: u32 = 1;
    /// Build from a `SubagentMeta` + additional runtime context.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_meta(
        meta: &SubagentMeta,
        model_id: Option<&str>,
        cwd: Option<&str>,
        worktree_path: Option<&str>,
        isolation_mode: Option<&str>,
        capability_mode: Option<&str>,
        reasoning_effort: Option<&str>,
        role: Option<&str>,
        parent_prompt_id: Option<&str>,
        depth: u32,
    ) -> Self {
        let session_kind = if meta.resumed_from.is_some() {
            "subagent_resume"
        } else {
            "subagent"
        };
        Self {
            schema_version: Self::SCHEMA_VERSION,
            session_id: meta.child_session_id.clone(),
            session_kind: session_kind.to_string(),
            subagent_id: meta.subagent_id.clone(),
            child_session_id: meta.child_session_id.clone(),
            parent_session_id: meta.parent_session_id.clone(),
            parent_prompt_id: parent_prompt_id.map(str::to_string),
            subagent_type: meta.subagent_type.clone(),
            description: meta.description.clone(),
            role: role.map(str::to_string),
            persona: meta.persona.clone(),
            context_normalized: meta.context_normalized,
            capability_mode: capability_mode.map(str::to_string),
            reasoning_effort: reasoning_effort.map(str::to_string),
            model_id: model_id.map(str::to_string),
            cwd: cwd.map(str::to_string),
            worktree_path: worktree_path.map(str::to_string),
            isolation_mode: isolation_mode.map(str::to_string),
            depth,
            started_at: meta.started_at.to_rfc3339(),
            completed_at: meta.completed_at.map(|t| t.to_rfc3339()),
            status: meta.status.clone(),
            duration_ms: meta.duration_ms,
            tool_calls: meta.tool_calls,
            turns: meta.turns,
            error: meta.error.clone(),
            fork_copy_error: meta.fork_copy_error.clone(),
            resumed_from: meta.resumed_from.clone(),
        }
    }
}
/// Write via a same-directory temp file and rename, so a crash mid-write
/// cannot leave a torn `meta.json` or `output.json`.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(tmp.path(), contents)?;
    tmp.persist(path)?;
    Ok(())
}
/// Write `meta.json`. Returns `true` on success so callers on the resume-pointer
/// path can gate worktree disposal on a durable write.
fn write_subagent_meta(dir: &Path, meta: &SubagentMeta) -> bool {
    let json = match serde_json::to_string_pretty(meta) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize subagent meta");
            return false;
        }
    };
    if let Err(e) = atomic_write(&dir.join("meta.json"), &json) {
        tracing::warn!(error = %e, "failed to write subagent meta");
        return false;
    }
    true
}
/// Borrowed output schema so persistence does not copy the text.
#[derive(serde::Serialize)]
struct SubagentOutputFileRef<'a> {
    schema_version: u32,
    output: &'a str,
}
const SUBAGENT_OUTPUT_SCHEMA_VERSION: u32 = 1;
fn write_subagent_output(dir: &Path, output: &str) -> bool {
    let file = SubagentOutputFileRef {
        schema_version: SUBAGENT_OUTPUT_SCHEMA_VERSION,
        output,
    };
    let json = match serde_json::to_string(&file) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize subagent output");
            return false;
        }
    };
    if let Err(e) = atomic_write(&dir.join("output.json"), &json) {
        tracing::warn!(error = %e, "failed to write subagent output");
        return false;
    }
    // Plain markdown for `agent://` reads (mirrors OMP's `<id>.md` artifacts).
    if let Err(e) = atomic_write(&dir.join("output.md"), output) {
        tracing::warn!(error = %e, "failed to write subagent output.md");
    }
    true
}
pub(crate) fn read_subagent_output(dir: &Path) -> Option<String> {
    if let Ok(md) = std::fs::read_to_string(dir.join("output.md"))
        && !md.is_empty()
    {
        return Some(md);
    }
    #[derive(serde::Deserialize)]
    struct OutputFile {
        schema_version: u32,
        output: String,
    }
    let data = std::fs::read_to_string(dir.join("output.json")).ok()?;
    let file: OutputFile = serde_json::from_str(&data).ok()?;
    (file.schema_version == SUBAGENT_OUTPUT_SCHEMA_VERSION).then_some(file.output)
}
/// Extra runtime context for GCS artifact upload. `SubagentMeta` doesn't
/// persist these fields, so they're carried from the spawn site.
#[derive(Clone)]
struct GcsUploadContext {
    bucket_url: Option<String>,
    upload_method: Option<crate::session::repo_changes::UploadMethod>,
    model_id: Option<String>,
    cwd: Option<String>,
    isolation_mode: Option<String>,
    capability_mode: Option<String>,
    reasoning_effort: Option<String>,
    role_name: Option<String>,
    parent_prompt_id: Option<String>,
    depth: u32,
    auth_manager: std::sync::Arc<crate::auth::AuthManager>,
}
/// Persist the durable worktree `snapshot_ref` into the on-disk `meta.json`
/// after completion, so `resumable_source_for` can rehydrate the disposed
/// worktree on resume. Returns `true` only when the ref is persisted to disk;
/// any read/parse/write failure is `warn!`-logged (this is the critical resume
/// pointer) so the caller keeps the worktree rather than removing it without a
/// recoverable ref. Also re-asserts the terminal `status` so a failed
/// `persist_subagent_completion` write can't leave a non-terminal record that
/// `resumable_source_for` rejects after the worktree is removed.
fn update_subagent_meta_snapshot_ref(dir: &Path, snapshot_ref: &str, status: &str) -> bool {
    let meta_path = dir.join("meta.json");
    let mut meta = match std::fs::read_to_string(&meta_path) {
        Ok(data) => match serde_json::from_str::<SubagentMeta>(&data) {
            Ok(meta) => meta,
            Err(e) => {
                tracing::warn!(error = %e, "failed to parse subagent meta; snapshot_ref not persisted (resume pointer lost)");
                return false;
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "failed to read subagent meta; snapshot_ref not persisted (resume pointer lost)");
            return false;
        }
    };
    meta.snapshot_ref = Some(snapshot_ref.to_string());
    meta.status = status.to_string();
    write_subagent_meta(dir, &meta)
}
#[must_use]
fn persist_subagent_output(dir: &Path, result: &SubagentResult) -> Option<PathBuf> {
    (result.success && !result.output.is_empty() && write_subagent_output(dir, &result.output))
        .then(|| dir.to_path_buf())
}
fn persist_subagent_completion(dir: &Path, result: &SubagentResult, gcs_ctx: &GcsUploadContext) {
    let meta_path = dir.join("meta.json");
    if let Ok(data) = std::fs::read_to_string(&meta_path)
        && let Ok(mut meta) = serde_json::from_str::<SubagentMeta>(&data)
    {
        meta.status = result.status().to_string();
        meta.completed_at = Some(chrono::Utc::now());
        meta.duration_ms = Some(result.duration_ms);
        meta.tool_calls = Some(result.tool_calls);
        meta.turns = Some(result.turns);
        meta.error = result.error.clone();
        write_subagent_meta(dir, &meta);
        if let (Some(bucket), Some(method)) = (&gcs_ctx.bucket_url, &gcs_ctx.upload_method) {
            let gcs_meta = SubagentSessionMetadata::from_meta(
                &meta,
                gcs_ctx.model_id.as_deref(),
                gcs_ctx.cwd.as_deref(),
                result.worktree_path.as_deref(),
                gcs_ctx.isolation_mode.as_deref(),
                gcs_ctx.capability_mode.as_deref(),
                gcs_ctx.reasoning_effort.as_deref(),
                gcs_ctx.role_name.as_deref(),
                gcs_ctx.parent_prompt_id.as_deref(),
                gcs_ctx.depth,
            );
            let bucket = bucket.clone();
            let method = method.clone();
            let auth_for_spawn = gcs_ctx.auth_manager.clone();
            tokio::spawn(async move {
                upload_subagent_metadata(&gcs_meta, &bucket, method, auth_for_spawn).await;
            });
        }
    }
}
pub(crate) const ORPHAN_RECONCILE_REASON: &str = "interrupted by process restart";
pub(crate) const LIVE_ORPHAN_RECONCILE_REASON: &str = "orphaned while parent session stayed live";
/// `SubagentFinished` for a force-terminated orphan; interrupt counts are zeroed.
fn cancelled_orphan_finish(
    subagent_id: String,
    child_session_id: String,
    duration_ms: u64,
    reason: &str,
) -> SessionUpdate {
    SessionUpdate::SubagentFinished {
        subagent_id,
        child_session_id,
        status: "cancelled".to_string(),
        error: Some(reason.to_owned()),
        termination_reason: Some("process_restart".to_string()),
        usage: None,

        tool_calls: 0,
        turns: 0,
        duration_ms,
        tokens_used: 0,
        output: None,
        will_wake: false,
    }
}
/// Flip a stale `running` meta to `cancelled` and emit the missing finish.
/// On meta-write failure returns `false` and skips the notify, so a reload re-heals.
fn finalize_orphaned_subagent(
    subagent_meta_dir: &Path,
    mut meta: SubagentMeta,
    gateway: &GatewaySender,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    reason: &str,
) -> bool {
    if !is_on_disk_meta_running(subagent_meta_dir) {
        return false;
    }
    let completed_at = chrono::Utc::now();
    let duration_ms = (completed_at - meta.started_at).num_milliseconds().max(0) as u64;
    meta.status = "cancelled".to_string();
    meta.completed_at = Some(completed_at);
    meta.duration_ms = Some(duration_ms);
    meta.tool_calls = Some(0);
    meta.turns = Some(0);
    meta.error = Some(reason.to_owned());
    if !write_subagent_meta(subagent_meta_dir, &meta) {
        return false;
    }
    emit_subagent_notification(
        gateway,
        &meta.parent_session_id,
        cancelled_orphan_finish(meta.subagent_id, meta.child_session_id, duration_ms, reason),
        parent_cmd_tx,
    );
    true
}
/// Persist a coordinator-held terminal outcome over a stale `running` meta.
/// Same write-then-emit durability as [`finalize_orphaned_subagent`].
fn persist_running_meta_as_finish(
    subagent_meta_dir: &Path,
    mut meta: SubagentMeta,
    finish: &SessionUpdate,
) -> bool {
    let SessionUpdate::SubagentFinished {
        status,
        error,
        tool_calls,
        turns,
        duration_ms,
        ..
    } = finish
    else {
        return false;
    };
    if !is_on_disk_meta_running(subagent_meta_dir) {
        return false;
    }
    meta.status = status.clone();
    meta.completed_at = Some(chrono::Utc::now());
    meta.duration_ms = Some(*duration_ms);
    meta.tool_calls = Some(*tool_calls);
    meta.turns = Some(*turns);
    meta.error = error.clone();
    write_subagent_meta(subagent_meta_dir, &meta)
}
fn is_on_disk_meta_running(subagent_meta_dir: &Path) -> bool {
    let Ok(data) = std::fs::read_to_string(subagent_meta_dir.join("meta.json")) else {
        return false;
    };
    serde_json::from_str::<SubagentMeta>(&data).is_ok_and(|meta| meta.status == "running")
}
/// Parse `meta_path` and return it only when it is a stale `running` orphan
/// owned by `parent_session_id` and not tracked live. Malformed metas → `None`.
fn running_orphan_meta(meta_path: &Path, parent_session_id: &str) -> Option<SubagentMeta> {
    let data = std::fs::read_to_string(meta_path).ok()?;
    let meta: SubagentMeta = serde_json::from_str(&data).ok()?;
    if meta.status != "running" || meta.parent_session_id != parent_session_id {
        return None;
    }
    Some(meta)
}
fn completed_finish_from_inspection(inspection: &SubagentInspection) -> Option<SessionUpdate> {
    let (status, error, tool_calls, turns) = match &inspection.snapshot.status {
        SubagentSnapshotStatus::Completed {
            tool_calls, turns, ..
        } => ("completed", None, *tool_calls, *turns),
        SubagentSnapshotStatus::Failed { error } => ("failed", Some(error.clone()), 0, 0),
        SubagentSnapshotStatus::Cancelled { reason } => ("cancelled", reason.clone(), 0, 0),
        SubagentSnapshotStatus::Initializing | SubagentSnapshotStatus::Running { .. } => {
            return None;
        }
    };
    Some(SessionUpdate::SubagentFinished {
        subagent_id: inspection.snapshot.subagent_id.clone(),
        child_session_id: inspection.child_session_id.clone(),
        status: status.to_owned(),
        error,
        termination_reason: None,
        usage: None,
        tool_calls,
        turns,
        duration_ms: inspection.snapshot.duration_ms,
        tokens_used: 0,
        output: None,
        will_wake: false,
    })
}
/// Heal subagents stuck "Running" after a dead process: emit exactly one
/// `SubagentFinished` per id, unioning two id-keyed sources (so a crash orphan
/// in both heals once) — `unfinished` replayed spawns whose finish a rewind
/// dropped (or a forked-in subagent with no meta), and on-disk `running` metas.
/// Skipping ids still active or pending: a `running` meta → `cancelled` (unless
/// the coordinator still holds its terminal result, then re-emit that); a terminal
/// meta that survived a rewound finish re-emits its real outcome; a no-meta
/// replayed spawn → `cancelled`. Runs after replay so the finish orders after the spawn.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn reconcile_orphaned_subagents_with_backend(
    unfinished: &[(String, String)],
    backend: &xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend,
    session_dir: &Path,
    parent_session_id: &str,
    gateway: &GatewaySender,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    orphan_reason: &str,
    heal_lock: Arc<tokio::sync::Mutex<()>>,
) {
    let _heal_guard = heal_lock.lock().await;
    let subagents_dir = session_dir.join("subagents");
    let mut candidates: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();
    for (id, child) in unfinished {
        candidates.insert(id.clone(), Some(child.clone()));
    }
    if let Ok(entries) = std::fs::read_dir(&subagents_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if running_orphan_meta(&entry.path().join("meta.json"), parent_session_id).is_some()
                && let Some(id) = name.to_str()
            {
                candidates.entry(id.to_string()).or_insert(None);
            }
        }
    }
    for (subagent_id, spawn_child) in candidates {
        let inspection = backend.inspect(&subagent_id).await;
        if inspection
            .as_ref()
            .is_some_and(|inspection| inspection.snapshot.is_running())
        {
            continue;
        }
        let subagent_dir = subagents_dir.join(&subagent_id);
        let meta = std::fs::read_to_string(subagent_dir.join("meta.json"))
            .ok()
            .and_then(|data| serde_json::from_str::<SubagentMeta>(&data).ok());
        match meta {
            Some(m) if m.parent_session_id != parent_session_id => {}
            Some(m) if m.status == "running" => {
                if let Some(finish) = inspection
                    .as_ref()
                    .and_then(completed_finish_from_inspection)
                {
                    if persist_running_meta_as_finish(&subagent_dir, m, &finish) {
                        tracing::info!(
                            subagent_id = %subagent_id,
                            parent_session_id,
                            "Re-emitting finish for completed subagent with a lost terminal meta write"
                        );
                        emit_subagent_notification(
                            gateway,
                            parent_session_id,
                            finish,
                            parent_cmd_tx,
                        );
                    }
                } else {
                    tracing::info!(
                        subagent_id = %m.subagent_id,
                        parent_session_id,
                        "Reconciling orphaned subagent left running by a previous process"
                    );
                    finalize_orphaned_subagent(
                        &subagent_dir,
                        m,
                        gateway,
                        parent_cmd_tx,
                        orphan_reason,
                    );
                }
            }
            Some(m) => {
                if spawn_child.is_none() {
                    continue;
                }
                tracing::info!(
                    subagent_id = %subagent_id,
                    parent_session_id,
                    status = %m.status,
                    "Re-emitting finish for rewound subagent (terminal meta survived)"
                );
                emit_subagent_notification(
                    gateway,
                    parent_session_id,
                    SessionUpdate::SubagentFinished {
                        subagent_id,
                        child_session_id: m.child_session_id,
                        status: m.status,
                        error: m.error,
                        termination_reason: None,
                        usage: None,
                        tool_calls: m.tool_calls.unwrap_or(0),
                        turns: m.turns.unwrap_or(0),
                        duration_ms: m.duration_ms.unwrap_or(0),
                        tokens_used: 0,
                        output: None,
                        will_wake: false,
                    },
                    parent_cmd_tx,
                );
            }
            None => {
                let Some(child_session_id) = spawn_child else {
                    continue;
                };
                tracing::info!(
                    subagent_id = %subagent_id,
                    parent_session_id,
                    "Reconciling inherited subagent with no local meta (cancelled)"
                );
                emit_subagent_notification(
                    gateway,
                    parent_session_id,
                    cancelled_orphan_finish(subagent_id, child_session_id, 0, orphan_reason),
                    parent_cmd_tx,
                );
            }
        }
    }
    drop(_heal_guard);
}
/// Same heal as resume, for a parent that never restarts. `unfinished` is
/// empty so only on-disk `running` metas are candidates. Persist-first plus
/// the per-parent lock make a second tick, sequential or overlapping, a
/// no-op.
pub(crate) async fn reconcile_live_orphaned_subagents(
    backend: &xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend,
    session_dir: &Path,
    parent_session_id: &str,
    gateway: &GatewaySender,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    heal_lock: Arc<tokio::sync::Mutex<()>>,
) {
    reconcile_orphaned_subagents_with_backend(
        &[],
        backend,
        session_dir,
        parent_session_id,
        gateway,
        parent_cmd_tx,
        LIVE_ORPHAN_RECONCILE_REASON,
        heal_lock,
    )
    .await;
}
#[cfg(test)]
mod tests;
