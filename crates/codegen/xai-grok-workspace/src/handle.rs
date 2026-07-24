//! [`WorkspaceHandle`] -- public handle to a workspace instance.
use fastrace::future::FutureExt as _;
use fastrace::local::LocalSpan;
use prometheus::{
    Histogram, HistogramVec, IntCounter, IntCounterVec, register_histogram, register_histogram_vec,
    register_int_counter, register_int_counter_vec,
};
use std::path::PathBuf;
use std::sync::Arc;
use xai_hunk_tracker::{HunkTrackerActor, HunkTrackerHandle, TrackingMode};
use xai_tool_protocol::ToolServerStatusPayload;
use xai_tool_protocol::turn_hook::TurnHookOutcome;
/// Default SIGTERM drain budget (ms); override via
/// `GROK_WORKSPACE_TERMINATION_GRACE_MS`. 45s fits under the K8s grace period.
const DEFAULT_TERMINATION_GRACE_MS: u64 = 45_000;
/// preStop-hook drain marker; override via `GROK_WORKSPACE_DRAINING_FILE`.
const DEFAULT_DRAINING_FILE: &str = "/tmp/workspace-server.draining";
static DRAIN_STARTED_TOTAL: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(|| {
    register_int_counter_vec!(
        "grok_workspace_drain_started_total",
        "Graceful drains started, by trigger reason",
        &["reason"]
    )
    .unwrap()
});
static DRAIN_COMPLETED_TOTAL: std::sync::LazyLock<IntCounterVec> = std::sync::LazyLock::new(|| {
    register_int_counter_vec!(
        "grok_workspace_drain_completed_total",
        "Graceful drains completed, by outcome",
        &["outcome"]
    )
    .unwrap()
});
static DRAIN_DURATION: std::sync::LazyLock<Histogram> = std::sync::LazyLock::new(|| {
    register_histogram!(
        "grok_workspace_drain_duration_seconds",
        "Wall-clock duration of a graceful two-phase drain",
        vec![0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0]
    )
    .unwrap()
});
static DRAIN_LOST_ITEMS_TOTAL: std::sync::LazyLock<IntCounter> = std::sync::LazyLock::new(|| {
    register_int_counter!(
        "grok_workspace_drain_lost_items_total",
        "Upload-queue items still pending when a drain deadline was exceeded (expected 0)"
    )
    .unwrap()
});
static PRODUCER_SPAWNED_AFTER_DRAIN_TOTAL: std::sync::LazyLock<IntCounter> =
    std::sync::LazyLock::new(|| {
        register_int_counter!(
            "grok_workspace_producer_spawned_after_drain_total",
            "Artifact producers spawned after a drain started — still tracked, but \
             their artifacts may miss the drain's queue flush (expected 0)"
        )
        .unwrap()
    });
/// `session.bind` resolutions advertising zero model-facing tools, by reason.
/// At most one reason is counted per zero-tool bind.
static WORKSPACE_BIND_ZERO_TOOLS_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_bind_zero_tools_total",
            "session.bind resolutions advertising zero model-facing tools, by reason",
            &["reason"]
        )
        .unwrap()
    });
/// `session.bind` resolutions that FAILED the bind (the server reports
/// bind-unavailable and the harness re-provisions), by reason. Distinct from
/// [`WORKSPACE_BIND_ZERO_TOOLS_TOTAL`], which counts binds that *completed*
/// while advertising zero model-facing tools.
static WORKSPACE_BIND_FAILED_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_bind_failed_total",
            "session.bind resolutions that failed the bind, by reason",
            &["reason"]
        )
        .unwrap()
    });
/// Pinned tool ids this binary could not serve at `session.bind`.
static WORKSPACE_BIND_UNSERVED_TOOLS_TOTAL: std::sync::LazyLock<IntCounter> =
    std::sync::LazyLock::new(|| {
        register_int_counter!(
            "grok_workspace_bind_unserved_tools_total",
            "Pinned tool ids unknown to this binary at session.bind (reported, not served)"
        )
        .unwrap()
    });
/// Model-facing tools advertised per successful `session.bind` (the RPC infra
/// handler is not counted). Catches silent shrinkage of a session's toolset.
static WORKSPACE_BIND_ADVERTISED_TOOLS: std::sync::LazyLock<Histogram> =
    std::sync::LazyLock::new(|| {
        register_histogram!(
            "grok_workspace_bind_advertised_tools",
            "Model-facing tools advertised per successful session.bind",
            vec![
                0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0, 16.0, 20.0, 30.0
            ]
        )
        .unwrap()
    });
/// Tripwire, expected 0 in production. `path="swap"`: a toolset swap found
/// the outgoing toolset's `Terminal` resource pointing at a backend other
/// than the session-owned one — a resolve path bypassed the session-owned
/// backend, and that backend's background tasks die with the old toolset.
/// Non-zero means background tasks were (or are about to be) killed by a
/// toolset swap: page the owning team. (`path="actor"` — actor-loop
/// channel-closure detection — is not emitted yet.)
pub(crate) static WORKSPACE_TERMINAL_BACKEND_ORPHANED_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_terminal_backend_orphaned_total",
            "Terminal backends detected orphaned from their session, by detection path \
             (tripwire, expected 0)",
            &["path"]
        )
        .unwrap()
    });
/// Environment-capture (`workspace_environment.json`) blocking task panics
/// (tripwire, expected 0). A non-zero rate means `WorkspaceEnvironment::capture`
/// is faulting for real sessions and dropping the artifact.
static ENV_CAPTURE_PANIC_TOTAL: std::sync::LazyLock<IntCounter> = std::sync::LazyLock::new(|| {
    register_int_counter!(
        "grok_workspace_env_capture_panic_total",
        "Environment-capture blocking task panics (tripwire, expected 0)"
    )
    .unwrap()
});
use crate::capability::CapabilityMode;
use crate::config::{
    AgentSessionConfig, DEFAULT_EVENT_BUFFER_CAPACITY, HookSourceConfig, WorkspaceConfig,
};
use crate::diag_server::DiagHandle;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::session::swap_policy::{
    DeferReason, SessionSnapshot, SwapAction, SwapDecision, SwapPolicy, SwapTrigger,
    record_swap_decision, record_toolset_swap,
};
use crate::session::tool_config::resolve_session_toolset;
use crate::session::{WorkspaceSession, WorkspaceShared};
use crate::telemetry::dc_log;
use crate::workspace_ops::{
    GetFileEntry, GetFileResult, GetFilesRes, PutFileEntry, PutFileResult, PutFilesRes,
};
use xai_file_utils::events::types::CancellationCategory;
use xai_file_utils::events::{Event, SessionRelationship, TurnOutcomeLabel};
use xai_file_utils::queue::EnqueueOutcome;
use xai_tool_protocol::turn_hook::{AfterTurnAckPayload, AfterTurnAckStatus};
/// Per-domain checkpoint captures, by domain and turn outcome.
pub(crate) static REWIND_CHECKPOINT_CAPTURE_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_rewind_checkpoint_capture_total",
            "Total rewind-checkpoint domain captures",
            &["domain", "outcome"]
        )
        .unwrap()
    });
/// Checkpoint finalizes, by turn outcome.
pub(crate) static REWIND_CHECKPOINT_FINALIZE_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_rewind_checkpoint_finalize_total",
            "Total rewind-checkpoint finalizes",
            &["outcome"]
        )
        .unwrap()
    });
/// Per-domain restores (the user-initiated `rewind_to` path), by domain and result.
pub(crate) static REWIND_RESTORE_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_rewind_restore_total",
            "Total rewind-checkpoint domain restores",
            &["domain", "result"]
        )
        .unwrap()
    });
/// Duration of per-domain capture operations.
pub(crate) static REWIND_CHECKPOINT_DURATION: std::sync::LazyLock<HistogramVec> =
    std::sync::LazyLock::new(|| {
        register_histogram_vec!(
            "grok_workspace_rewind_checkpoint_duration_seconds",
            "Duration of rewind-checkpoint per-domain capture operations",
            &["domain"],
            vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0]
        )
        .unwrap()
    });
/// Correctness canary: non-`Completed` `after_turn` boundaries that produced
/// a rewind finalize. Stays 0 unless `workspace_rewind_all_outcomes` is on.
pub(crate) static REWIND_NON_COMPLETED_FINALIZE_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "grok_workspace_rewind_non_completed_finalize_total",
            "Non-Completed after_turn boundaries that produced a rewind finalize",
            &["outcome"]
        )
        .unwrap()
    });
/// `domain` label for the rewind metrics. Typed so the closed fs/hunk/git
/// vocabulary can't be mistyped at a call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RewindDomain {
    Fs,
    Hunk,
    Git,
}
impl RewindDomain {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RewindDomain::Fs => "fs",
            RewindDomain::Hunk => "hunk",
            RewindDomain::Git => "git",
        }
    }
}
/// Map a turn outcome to a stable, bounded `outcome` metric label. The catch-all
/// keeps label cardinality bounded (`TurnHookOutcome` is `#[non_exhaustive]`).
pub(crate) fn rewind_outcome_label(outcome: TurnHookOutcome) -> &'static str {
    match outcome {
        TurnHookOutcome::Completed => "completed",
        TurnHookOutcome::Cancelled => "cancelled",
        TurnHookOutcome::Error => "error",
        _ => "other",
    }
}
/// Map a restore result to its `result` metric label.
pub(crate) fn rewind_result_label(success: bool) -> &'static str {
    if success { "success" } else { "failure" }
}
/// Record a per-domain checkpoint capture, labeled by turn outcome.
pub(crate) fn record_rewind_capture(domain: RewindDomain, outcome: TurnHookOutcome) {
    REWIND_CHECKPOINT_CAPTURE_TOTAL
        .with_label_values(&[domain.as_str(), rewind_outcome_label(outcome)])
        .inc();
}
/// Observe how long a per-domain capture operation took (seconds).
pub(crate) fn observe_rewind_capture_duration(domain: RewindDomain, seconds: f64) {
    REWIND_CHECKPOINT_DURATION
        .with_label_values(&[domain.as_str()])
        .observe(seconds);
}
/// Record a per-domain restore, labeled by result (success/failure).
pub(crate) fn record_rewind_restore(domain: RewindDomain, success: bool) {
    REWIND_RESTORE_TOTAL
        .with_label_values(&[domain.as_str(), rewind_result_label(success)])
        .inc();
}
/// Record the metrics common to every finalize: FS-domain capture + finalize
/// counter (both by `outcome`) + FS capture duration. Shared by the RPC finalize
/// and the non-`Completed` cross-over so the two paths can't drift.
pub(crate) fn record_fs_finalize(outcome: TurnHookOutcome, fs_capture_seconds: f64) {
    observe_rewind_capture_duration(RewindDomain::Fs, fs_capture_seconds);
    record_rewind_capture(RewindDomain::Fs, outcome);
    REWIND_CHECKPOINT_FINALIZE_TOTAL
        .with_label_values(&[rewind_outcome_label(outcome)])
        .inc();
}
/// Record the correctness canary: a non-`Completed` `after_turn` boundary that
/// produced a finalize.
pub(crate) fn record_non_completed_finalize_canary(outcome: TurnHookOutcome) {
    REWIND_NON_COMPLETED_FINALIZE_TOTAL
        .with_label_values(&[rewind_outcome_label(outcome)])
        .inc();
}
/// Zero-init this module's metric families. See [`crate::init_metrics`].
pub(crate) fn init_metrics() {
    for reason in [DrainReason::Sigterm, DrainReason::Evict] {
        DRAIN_STARTED_TOTAL
            .with_label_values(&[reason.as_str()])
            .inc_by(0);
    }
    for outcome in [
        DrainOutcome::Full,
        DrainOutcome::Partial,
        DrainOutcome::ProducersTimeout,
        DrainOutcome::Timeout,
    ] {
        DRAIN_COMPLETED_TOTAL
            .with_label_values(&[outcome.as_str()])
            .inc_by(0);
    }
    DRAIN_LOST_ITEMS_TOTAL.inc_by(0);
    PRODUCER_SPAWNED_AFTER_DRAIN_TOTAL.inc_by(0);
    WORKSPACE_BIND_UNSERVED_TOOLS_TOTAL.inc_by(0);
    ENV_CAPTURE_PANIC_TOTAL.inc_by(0);
    std::sync::LazyLock::force(&DRAIN_DURATION);
    std::sync::LazyLock::force(&WORKSPACE_BIND_ADVERTISED_TOOLS);
    for reason in [
        "workspace_shutdown",
        "session_lookup_failed",
        "session_error",
    ] {
        WORKSPACE_BIND_FAILED_TOTAL
            .with_label_values(&[reason])
            .inc_by(0);
    }
    for reason in ["empty_after_filter", "missing_tool_config"] {
        WORKSPACE_BIND_ZERO_TOOLS_TOTAL
            .with_label_values(&[reason])
            .inc_by(0);
    }
    WORKSPACE_TERMINAL_BACKEND_ORPHANED_TOTAL
        .with_label_values(&["swap"])
        .inc_by(0);
    for domain in [RewindDomain::Fs, RewindDomain::Hunk, RewindDomain::Git] {
        for outcome in ["completed", "cancelled", "error", "other"] {
            REWIND_CHECKPOINT_CAPTURE_TOTAL
                .with_label_values(&[domain.as_str(), outcome])
                .inc_by(0);
        }
        for result in ["success", "failure"] {
            REWIND_RESTORE_TOTAL
                .with_label_values(&[domain.as_str(), result])
                .inc_by(0);
        }
        let _ = REWIND_CHECKPOINT_DURATION.with_label_values(&[domain.as_str()]);
    }
    for outcome in ["completed", "cancelled", "error", "other"] {
        REWIND_CHECKPOINT_FINALIZE_TOTAL
            .with_label_values(&[outcome])
            .inc_by(0);
        REWIND_NON_COMPLETED_FINALIZE_TOTAL
            .with_label_values(&[outcome])
            .inc_by(0);
    }
}
/// Outcome of a hub `session.bind` against an already-existing session
/// (see [`WorkspaceHandle::rebind_existing_hub_session`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RebindOutcome {
    /// Same (or no) explicit toolset — session reused untouched.
    Reused,
    /// Changed explicit toolset — re-resolved and swapped in.
    Reresolved,
    /// Changed explicit toolset, but the re-resolve failed; existing kept.
    ReresolveFailed,
    /// Changed explicit toolset, but the session's toolset is externally
    /// owned (local-bind shape) — nothing was resolved or swapped; the
    /// existing toolset (and fingerprint) kept. Reused-semantics for the
    /// bind reply: advertise the KEPT toolset, drop any unserved set from
    /// the unapplied resolve.
    KeptExternallyOwned,
    /// Changed explicit toolset while the session had tool calls in flight
    /// (`explicit → different-explicit` transition only) — existing kept;
    /// a later rebind with no calls in flight applies the correction.
    ReresolveDeferredInFlight,
}
/// What [`WorkspaceHandle::resolve_and_swap_session_toolset`] actually did —
/// so no caller can mistake a deliberate skip for an installed swap (the
/// skip leaves toolset AND fingerprint untouched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a skip means the config was NOT applied; callers must not report success"]
pub(crate) enum SwapOutcome {
    /// Toolset re-resolved and installed; fingerprint updated.
    Swapped,
    /// Identical fingerprint ([`SwapDecision::Reuse`]): the live toolset
    /// already reflects the config, nothing resolved or changed.
    Reused,
    /// Externally-owned (local-bind) toolset: rebuild skipped, nothing
    /// changed. See `toolset_terminal_is_session_owned`.
    SkippedExternallyOwned,
}
/// Public handle to a workspace instance. Owns shared state (sessions,
/// MCP snapshot, tool config, event bus) and session lifecycle.
#[derive(Clone)]
pub struct WorkspaceHandle {
    pub(crate) shared: Arc<WorkspaceShared>,
}
impl WorkspaceHandle {
    /// `None` when not connected. Never hands out an owned
    /// `ToolServer` — a clone-drop begins server teardown.
    pub async fn trace_donation_reporter(
        &self,
        service_name: &str,
    ) -> Option<(
        xai_computer_hub_sdk::HubDonatingReporter,
        xai_computer_hub_sdk::TraceDonationPump,
    )> {
        self.shared
            .hub_handle
            .lock()
            .await
            .as_ref()
            .map(|hub| hub.server.trace_donation_reporter(service_name))
    }
    /// Post-connect entry point for the log export layer, the analogue of
    /// [`Self::trace_donation_reporter`]. Returns `None` when not connected
    /// (the layer stays inert). On
    /// `Some`, yields a [`LogDonationSender`] to swap into the
    /// already-installed inert `DonatingLogLayer` plus a drain handle.
    /// Never hands out an owned `ToolServer` — a clone-drop begins server
    /// teardown.
    ///
    /// [`LogDonationSender`]: xai_computer_hub_sdk::LogDonationSender
    pub async fn log_donation_layer(
        &self,
        service_name: &str,
    ) -> Option<(
        xai_computer_hub_sdk::LogDonationSender,
        xai_computer_hub_sdk::LogDonationPump,
    )> {
        self.shared
            .hub_handle
            .lock()
            .await
            .as_ref()
            .map(|hub| hub.server.log_donation_layer(service_name))
    }
    /// Post-connect entry point for metric export, the analogue of
    /// [`Self::trace_donation_reporter`]. Returns `None` when not connected
    /// (no reporter is spawned). On
    /// `Some`, spawns the periodic Prometheus-registry gather → OTLP →
    /// export pump and yields a drain handle. Never hands out an owned
    /// `ToolServer` — a clone-drop begins server teardown.
    pub async fn metric_donation_reporter(
        &self,
        service_name: &str,
    ) -> Option<xai_computer_hub_sdk::MetricDonationPump> {
        self.shared
            .hub_handle
            .lock()
            .await
            .as_ref()
            .map(|hub| hub.server.metric_donation_reporter(service_name))
    }
    /// Construct a handle with zero sessions.
    ///
    /// Sessions are created explicitly via [`Self::create_session`] or
    /// [`Self::fork_session`]. There is no implicit "main" session —
    /// callers (TUI, workspace-server binary) create their first
    /// session after construction.
    ///
    /// # Panics
    /// Requires a Tokio runtime to be entered (for broadcast channel).
    pub fn new(config: WorkspaceConfig) -> WorkspaceResult<Self> {
        Self::build(
            config,
            ephemeral_workspace_home(),
            None,
            true,
            false,
            events_enabled(),
            rewind_all_outcomes_from_env(),
            tool_defs_enabled(),
            crate::upload::environment::WorkspaceIdentity::default(),
        )
    }
    /// Construct a handle with an explicit `$GROK_WORKSPACE_HOME` and a
    /// pre-spawned [`UploadQueue`](xai_file_utils::queue::UploadQueue).
    ///
    /// [`connect_local_workspace`] calls this so the queue is backed by the
    /// proxy storage config; [`Self::new`] takes the queue-less path for tests
    /// and local mode.
    ///
    /// # Panics
    /// Requires a Tokio runtime to be entered (for broadcast channel).
    pub(crate) fn new_with_data_collection(
        config: WorkspaceConfig,
        workspace_home: std::path::PathBuf,
        upload_queue: Arc<xai_file_utils::queue::UploadQueue>,
        upload_queue_enabled: bool,
        data_collection_disabled: bool,
        identity: crate::upload::environment::WorkspaceIdentity,
    ) -> WorkspaceResult<Self> {
        Self::build(
            config,
            workspace_home,
            Some(upload_queue),
            upload_queue_enabled,
            data_collection_disabled,
            events_enabled(),
            rewind_all_outcomes_from_env(),
            tool_defs_enabled(),
            identity,
        )
    }
    fn build(
        config: WorkspaceConfig,
        workspace_home: std::path::PathBuf,
        upload_queue: Option<Arc<xai_file_utils::queue::UploadQueue>>,
        _upload_queue_enabled: bool,
        data_collection_disabled: bool,
        events_enabled: bool,
        workspace_rewind_all_outcomes: bool,
        tool_defs_enabled: bool,
        identity: crate::upload::environment::WorkspaceIdentity,
    ) -> WorkspaceResult<Self> {
        let sessions = std::collections::HashMap::new();
        let local_registry = xai_computer_hub_sdk::LocalRegistry::new();
        let capacity = if config.event_buffer_capacity == 0 {
            DEFAULT_EVENT_BUFFER_CAPACITY
        } else {
            config.event_buffer_capacity
        };
        let (events, _drop_rx) = tokio::sync::broadcast::channel(capacity);
        let (hook_registry, hook_load_errors) = {
            use xai_grok_hooks::discovery::{HookSource, load_hooks_from_sources};
            fn to_hook_source(s: &HookSourceConfig) -> HookSource<'_> {
                match s {
                    HookSourceConfig::SettingsFile(p) => HookSource::SettingsFile(p.as_path()),
                    HookSourceConfig::Directory(p) => HookSource::Directory(p.as_path()),
                }
            }
            let global_refs: Vec<HookSource<'_>> = config
                .hook_global_sources
                .iter()
                .map(to_hook_source)
                .collect();
            let project_refs: Vec<HookSource<'_>> = config
                .hook_project_sources
                .iter()
                .map(to_hook_source)
                .collect();
            let (registry, errors) = load_hooks_from_sources(&global_refs, &project_refs);
            for err in &errors {
                tracing::warn!(error = %err, "hook discovery error (non-fatal)");
            }
            tracing::info!(
                hook_count = registry.len(),
                error_count = errors.len(),
                "hook discovery complete"
            );
            (registry, errors)
        };
        let lsp: Option<Arc<dyn xai_grok_tools::implementations::lsp::LspBackend>> = {
            let sourced =
                xai_grok_tools::implementations::lsp::config::load_servers_with_plugins_sourced(
                    &config.root_cwd,
                    &[],
                    &[],
                    &[],
                    &[],
                );
            let servers =
                xai_grok_tools::implementations::lsp::config::filter_project_lsp_when_untrusted(
                    sourced,
                    config.project_lsp_trusted,
                );
            if servers.is_empty() {
                None
            } else {
                use xai_grok_tools::implementations::lsp::{
                    LspBackend, LspBackendAdapter, LspManager,
                };
                let mgr = Arc::new(tokio::sync::Mutex::new(LspManager::new(
                    servers,
                    config.root_cwd.clone(),
                    true,
                    xai_grok_tools::notification::ToolNotificationHandle::noop(),
                )));
                let adapter = Arc::new(LspBackendAdapter::new(mgr));
                adapter.ensure_started_background();
                Some(adapter)
            }
        };
        let session_event_writers: Arc<
            dashmap::DashMap<String, xai_file_utils::events::EventWriter>,
        > = Arc::new(dashmap::DashMap::new());
        let activity_tracker = Arc::new(
            crate::activity::ActivityTracker::with_prune_window(
                config.status_config.session_idle_prune,
            )
            .with_idle_ignores_background(config.status_config.idle_ignores_background)
            .with_preview_activity_window_ms(
                config.status_config.preview_activity_window.as_millis() as u64,
            ),
        );
        activity_tracker.set_event_writers(session_event_writers.clone());
        if let Some(queue) = &upload_queue {
            activity_tracker.set_upload_queue_stats(queue.stats_arc());
            queue
                .stats()
                .set_transition_notify(activity_tracker.notify_handle());
        }
        let producer_tasks = tokio_util::task::TaskTracker::new();
        activity_tracker.set_producer_tasks(producer_tasks.clone());
        let shared = WorkspaceShared {
            default_tool_config: config.default_tool_config,
            require_explicit_toolset: config.require_explicit_toolset,
            confine_fs_to_workspace_root: config.confine_fs_to_workspace_root,
            root_cwd: config.root_cwd.clone(),
            sessions: parking_lot::RwLock::new(sessions),
            session_factory: config.session_factory,
            mcp_tools_snapshot: arc_swap::ArcSwap::new(Arc::new(vec![])),
            events,
            respect_gitignore: config.respect_gitignore,
            memory_config: config.memory_config,
            hook_registry: Arc::new(parking_lot::RwLock::new(hook_registry)),
            hook_load_errors,
            skills_config: config.skills_config,
            plugin_discovery_config: config.plugin_discovery_config,
            hub_handle: tokio::sync::Mutex::new(None),
            hub_tools_snapshot: arc_swap::ArcSwap::new(Arc::new(vec![])),
            hub_config: config.hub_config,
            auth_provider: config.auth_provider,
            activity_notify_handle: arc_swap::ArcSwap::new(Arc::new(None)),
            client_ext_sink: arc_swap::ArcSwap::new(Arc::new(None)),
            local_registry,
            activity_tracker,
            status_config: config.status_config,
            server_metadata: config.server_metadata,
            identity,
            fuzzy_searches: Arc::new(tokio::sync::Mutex::new(
                crate::file_system::FuzzySearchManager::new(std::time::Duration::from_secs(300)),
            )),
            lsp,
            codebase_indexes: Arc::new(parking_lot::Mutex::new(
                crate::file_system::CodebaseIndexManager::new(),
            )),
            workspace_rewind_all_outcomes,
            workspace_home,
            upload_queue,
            data_collection_disabled,
            events_enabled,
            tool_defs_enabled,
            tool_defs_last_emit: dashmap::DashMap::new(),
            session_event_writers,
            inflight_enqueues: dashmap::DashMap::new(),
            producer_tasks,
            #[cfg(test)]
            post_resolve_test_hook: parking_lot::Mutex::new(None),
            client_fs_hash_memo: Default::default(),
        };
        Ok(Self {
            shared: Arc::new(shared),
        })
    }
    #[allow(dead_code)]
    pub fn shared(&self) -> &Arc<WorkspaceShared> {
        &self.shared
    }
    pub fn activity_tracker(&self) -> &std::sync::Arc<crate::activity::ActivityTracker> {
        &self.shared.activity_tracker
    }
    /// The [`ToolServer`](xai_computer_hub_sdk::ToolServer) for this
    /// workspace, if a server connection is active.
    ///
    /// Non-blocking: returns `None` both when no server is connected and when the
    /// handle is momentarily locked (e.g. a concurrent connect), so callers
    /// must treat `None` as "no server available right now" and degrade gracefully.
    pub fn hub_server(&self) -> Option<xai_computer_hub_sdk::ToolServer> {
        self.shared.hub_server()
    }
    /// Like [`Self::hub_server`] but awaits the connection lock instead of returning
    /// `None` on contention, so a transient `connect_hub` lock is not mistaken
    /// for "no server". `None` means no server is connected. Use from async callers.
    pub async fn hub_server_blocking(&self) -> Option<xai_computer_hub_sdk::ToolServer> {
        self.shared.hub_server_blocking().await
    }
    /// Get the workspace root directory.
    pub(crate) fn root_cwd(&self) -> crate::error::WorkspaceResult<PathBuf> {
        Ok(self.shared.root_cwd.clone())
    }
    /// Create a new top-level session from the workspace's default config.
    ///
    /// Unlike [`fork_session`](Self::fork_session), this does not inherit
    /// from a parent — it creates a fresh session with
    /// `CapabilityMode::All` and the workspace's `root_cwd`. Both the
    /// TUI and server use this as the primary session creation path.
    ///
    /// Returns the newly created session, or an error if a session with
    /// the given ID already exists.
    pub fn create_session(
        &self,
        session_id: impl Into<String>,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        self.create_session_with_cwd(session_id, None)
    }
    /// Create a session with an optional CWD override, using the workspace
    /// default toolset and `CapabilityMode::All`.
    pub fn create_session_with_cwd(
        &self,
        session_id: impl Into<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        self.create_session_with_config(session_id, cwd, None, CapabilityMode::All, None, false)
    }
    /// Create a session with an optional CWD override, per-session toolset, and
    /// capability mode. Bind-time entry point; `tool_config: None` uses the default.
    /// `viewer_ctx` is `None` for sessions that don't go through the server bind path.
    pub fn create_session_with_config(
        &self,
        session_id: impl Into<String>,
        cwd: Option<std::path::PathBuf>,
        tool_config: Option<xai_grok_tools::registry::types::ToolServerConfig>,
        capability: CapabilityMode,
        viewer_ctx: Option<xai_tool_runtime::WorkspaceViewerContext>,
        system_notifications: bool,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        let session_id = session_id.into();
        let session_cwd = cwd.unwrap_or_else(|| self.shared.root_cwd.clone());
        let (hunk_event_tx, _hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let hunk_cancel = tokio_util::sync::CancellationToken::new();
        let hunk_tracker = HunkTrackerActor::spawn(
            session_id.clone(),
            session_cwd.clone(),
            hunk_event_tx,
            TrackingMode::AllDirty,
            hunk_cancel.clone(),
        );
        let result = self.create_session_with_tracker_inner(
            session_id,
            session_cwd,
            hunk_tracker,
            Some(hunk_cancel.clone()),
            tool_config,
            capability,
            viewer_ctx,
            system_notifications,
        );
        if result.is_err() {
            hunk_cancel.cancel();
        }
        result
    }
    /// Create a session that reuses an existing hunk tracker (already rooted at
    /// `cwd`) instead of spawning a new one, so the workspace session and the
    /// agent share a single per-session tracker. `tool_config: None` uses the default.
    pub fn create_session_with_tracker(
        &self,
        session_id: impl Into<String>,
        cwd: std::path::PathBuf,
        hunk_tracker: HunkTrackerHandle,
        tool_config: Option<xai_grok_tools::registry::types::ToolServerConfig>,
        capability: CapabilityMode,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        self.create_session_with_tracker_and_viewer_ctx(
            session_id,
            cwd,
            hunk_tracker,
            tool_config,
            capability,
            None,
            false,
        )
    }
    /// Variant of [`create_session_with_tracker`](Self::create_session_with_tracker)
    /// that carries a session-bind viewer context. The tracker is externally
    /// owned, so the session stores no cancel token for it.
    pub fn create_session_with_tracker_and_viewer_ctx(
        &self,
        session_id: impl Into<String>,
        cwd: std::path::PathBuf,
        hunk_tracker: HunkTrackerHandle,
        tool_config: Option<xai_grok_tools::registry::types::ToolServerConfig>,
        capability: CapabilityMode,
        viewer_ctx: Option<xai_tool_runtime::WorkspaceViewerContext>,
        system_notifications: bool,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        self.create_session_with_tracker_inner(
            session_id,
            cwd,
            hunk_tracker,
            None,
            tool_config,
            capability,
            viewer_ctx,
            system_notifications,
        )
    }
    /// Shared creation body. `hunk_tracker_cancel` is `Some` only for
    /// workspace-spawned trackers, whose actor lifetime the session then
    /// owns; externally owned trackers pass `None`.
    #[allow(clippy::too_many_arguments)]
    fn create_session_with_tracker_inner(
        &self,
        session_id: impl Into<String>,
        cwd: std::path::PathBuf,
        hunk_tracker: HunkTrackerHandle,
        hunk_tracker_cancel: Option<tokio_util::sync::CancellationToken>,
        tool_config: Option<xai_grok_tools::registry::types::ToolServerConfig>,
        capability: CapabilityMode,
        viewer_ctx: Option<xai_tool_runtime::WorkspaceViewerContext>,
        system_notifications: bool,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        let session_id = session_id.into();
        if session_id.is_empty() {
            return Err(WorkspaceError::EmptyAgentId);
        }
        let mut sessions = self.shared.sessions.write();
        if self.shared.activity_tracker.is_draining() {
            return Err(WorkspaceError::ShuttingDown);
        }
        if sessions.contains_key(&session_id) {
            return Err(WorkspaceError::SessionAlreadyExists(session_id));
        }
        let session_env = Arc::new(std::collections::HashMap::new());
        let config = tool_config.unwrap_or_else(|| self.shared.default_tool_config.clone());
        let mcp_snapshot = self.shared.mcp_tools_snapshot.load_full();
        let hub_snapshot = self.shared.hub_tools_snapshot.load_full();
        let system_notify_channel = system_notifications
            .then(xai_grok_tools::notification::types::ToolNotificationHandle::channel);
        let system_notify_handle = system_notify_channel.as_ref().map(|(h, _)| h.clone());
        let (effective, toolset, terminal_backend) = {
            let _span = LocalSpan::enter_with_local_parent("tool_server.toolset_resolve")
                .with_property(|| ("session_id", session_id.clone()));
            resolve_session_toolset(
                config,
                capability,
                &mcp_snapshot,
                &hub_snapshot,
                cwd.clone(),
                session_env.clone(),
                &session_id,
                self.shared.session_factory.as_ref(),
                Some(self.shared.local_registry.clone()),
                self.shared.lsp.clone(),
                viewer_ctx.clone(),
                self.shared
                    .compose_session_notification_handle(system_notify_handle),
            )
        }?;
        let session = Arc::new(WorkspaceSession::new(
            session_id.clone(),
            cwd,
            session_env,
            capability,
            0,
            u32::MAX,
            Arc::new(effective),
            toolset,
            terminal_backend,
            hunk_tracker,
            hunk_tracker_cancel,
            viewer_ctx,
            system_notifications,
            system_notify_channel,
        ));
        tracing::info!(session_id = %session_id, "create_session: new session created");
        sessions.insert(session_id, session.clone());
        record_toolset_swap(
            &self.shared.activity_tracker,
            "create",
            session.session_id(),
        );
        Ok(session)
    }
    /// Update a session's tool config with auth and serialization; the RPC
    /// handler derives `caller_session_id` from the server-bound envelope.
    /// Swap gating (retryable `TurnActive`, stale heal): [`SwapPolicy::evaluate`].
    pub(crate) async fn update_tool_config(
        &self,
        caller_session_id: &str,
        session_id: &str,
        new_config: xai_grok_tools::registry::types::ToolServerConfig,
    ) -> crate::error::WorkspaceResult<()> {
        let session = self
            .session(session_id)
            .ok_or_else(|| crate::error::WorkspaceError::SessionNotFound(session_id.to_owned()))?;
        if caller_session_id != session_id {
            return Err(crate::error::WorkspaceError::Unauthorized {
                caller: caller_session_id.to_owned(),
                target: session_id.to_owned(),
            });
        }
        match self
            .resolve_and_swap_session_toolset(&session, new_config, SwapTrigger::UpdateRpc)
            .await?
        {
            SwapOutcome::Swapped | SwapOutcome::Reused => Ok(()),
            SwapOutcome::SkippedExternallyOwned => Err(
                crate::error::WorkspaceError::ToolsetExternallyOwned(session_id.to_owned()),
            ),
        }
    }
    /// Re-resolve `new_config` against the session's frozen bind-time inputs
    /// and atomically swap its toolset (`ToolsChanged`). Update-RPC entry:
    /// gated by [`SwapPolicy::evaluate`], twice (entry + post-resolve).
    pub(crate) async fn resolve_and_swap_session_toolset(
        &self,
        session: &Arc<crate::session::WorkspaceSession>,
        new_config: xai_grok_tools::registry::types::ToolServerConfig,
        trigger: SwapTrigger,
    ) -> crate::error::WorkspaceResult<SwapOutcome> {
        let _update_guard = session.update_lock.lock().await;
        let session_id = session.session_id();
        let new_fingerprint = serde_json::to_value(&new_config).ok();
        let snapshot = SessionSnapshot::capture(
            session,
            &self.shared.activity_tracker,
            new_fingerprint.as_ref(),
        )
        .await;
        match SwapPolicy::evaluate(&snapshot, trigger) {
            SwapDecision::Reuse => {
                tracing::debug!(
                    session_id = %session_id,
                    trigger = trigger.metric_label(),
                    "toolset config identical to the stored bind fingerprint — \
                     reused untouched"
                );
                Ok(SwapOutcome::Reused)
            }
            SwapDecision::Skip(reason) => {
                record_swap_decision(
                    &self.shared.activity_tracker,
                    trigger,
                    session_id,
                    SwapAction::Skipped(reason),
                );
                tracing::warn!(
                    session_id = %session_id,
                    trigger = trigger.metric_label(),
                    "toolset swap skipped: toolset terminal backend is externally \
                     owned (local bind)"
                );
                Ok(SwapOutcome::SkippedExternallyOwned)
            }
            SwapDecision::Defer(reason) => {
                record_swap_decision(
                    &self.shared.activity_tracker,
                    trigger,
                    session_id,
                    SwapAction::Deferred(reason),
                );
                tracing::info!(
                    session_id = %session_id,
                    trigger = trigger.metric_label(),
                    "toolset mutation rejected: turn active — retry at the turn boundary"
                );
                Err(crate::error::WorkspaceError::TurnActive(
                    session_id.to_owned(),
                ))
            }
            SwapDecision::Apply => {
                self.resolve_and_swap_session_toolset_locked(
                    session,
                    new_config,
                    new_fingerprint,
                    trigger,
                )
                .await
            }
        }
    }
    /// The [`SwapDecision::Apply`] arm: resolve `new_config` (whose
    /// fingerprint `new_fingerprint` must be) and install it. Callers hold
    /// `update_lock` and evaluated [`SwapPolicy`] to `Apply` under that hold.
    async fn resolve_and_swap_session_toolset_locked(
        &self,
        session: &Arc<crate::session::WorkspaceSession>,
        new_config: xai_grok_tools::registry::types::ToolServerConfig,
        new_fingerprint: Option<serde_json::Value>,
        trigger: SwapTrigger,
    ) -> crate::error::WorkspaceResult<SwapOutcome> {
        let session_id = session.session_id().to_owned();
        let mcp_snapshot = self.shared.mcp_tools_snapshot.load_full();
        let hub_snapshot = self.shared.hub_tools_snapshot.load_full();
        let cwd = session.cwd().to_path_buf();
        let session_env = session.session_env().clone();
        let cap = session.capability_mode();
        let factory = self.shared.session_factory.clone();
        let lr = self.shared.local_registry.clone();
        let lsp = self.shared.lsp.clone();
        let sid = session_id.to_owned();
        let viewer_ctx = session.viewer_ctx().cloned();
        let notification_handle = self
            .shared
            .compose_session_notification_handle(session.system_notify_handle());
        let terminal_backend = session.terminal_backend().clone();
        let resolve_result = tokio::task::spawn_blocking(move || {
            crate::session::tool_config::resolve_session_toolset_rebuild(
                new_config,
                cap,
                &mcp_snapshot,
                &hub_snapshot,
                cwd,
                session_env,
                &sid,
                factory.as_ref(),
                Some(lr),
                lsp,
                viewer_ctx,
                notification_handle,
                terminal_backend,
            )
        })
        .await
        .map_err(|e| crate::error::WorkspaceError::JoinError(e.to_string()))?;
        let (effective, new_toolset) = resolve_result?;
        #[cfg(test)]
        if let Some(hook) = self.shared.post_resolve_test_hook.lock().as_ref() {
            hook();
        }
        if trigger.rechecks_after_resolve() {
            let snapshot = SessionSnapshot::capture(
                session,
                &self.shared.activity_tracker,
                new_fingerprint.as_ref(),
            )
            .await;
            match SwapPolicy::evaluate(&snapshot, trigger) {
                SwapDecision::Apply => {}
                SwapDecision::Reuse => {
                    tracing::debug!(
                        session_id = %session_id,
                        trigger = trigger.metric_label(),
                        "resolved toolset discarded post-resolve: a concurrent \
                         bind installed the identical fingerprint during the \
                         re-resolve"
                    );
                    return Ok(SwapOutcome::Reused);
                }
                SwapDecision::Skip(reason) => {
                    record_swap_decision(
                        &self.shared.activity_tracker,
                        trigger,
                        &session_id,
                        SwapAction::Skipped(reason),
                    );
                    tracing::warn!(
                        session_id = %session_id,
                        trigger = trigger.metric_label(),
                        "toolset swap skipped: toolset terminal backend is externally \
                         owned (local bind)"
                    );
                    return Ok(SwapOutcome::SkippedExternallyOwned);
                }
                SwapDecision::Defer(reason) => {
                    let reason = match reason {
                        DeferReason::TurnActive => DeferReason::TurnActiveLate,
                        other => other,
                    };
                    record_swap_decision(
                        &self.shared.activity_tracker,
                        trigger,
                        &session_id,
                        SwapAction::Deferred(reason),
                    );
                    tracing::info!(
                        session_id = %session_id,
                        trigger = trigger.metric_label(),
                        "toolset mutation rejected post-resolve: a turn started during \
                         the re-resolve — resolved toolset discarded; retry at the \
                         turn boundary"
                    );
                    return Err(crate::error::WorkspaceError::TurnActive(session_id));
                }
            }
        }
        session
            .replace_carrying_browser_service(Arc::new(effective), new_toolset)
            .await;
        session.set_bind_tool_config_fingerprint(new_fingerprint);
        session.clear_stale_resolve();
        record_swap_decision(
            &self.shared.activity_tracker,
            trigger,
            &session_id,
            SwapAction::Applied,
        );
        let _ = self
            .shared
            .events
            .send(xai_grok_workspace_types::WorkspaceEvent::ToolsChanged {
                session_id: session_id.to_owned(),
            });
        Ok(SwapOutcome::Swapped)
    }
    /// Hub `session.bind` against an existing session: reuse, or re-resolve
    /// and swap per the owner-rebind policy rows (incl. the identical stale
    /// heal). `explicit_cfg=None` never overwrites; `None` = session vanished.
    pub(crate) async fn rebind_existing_hub_session(
        &self,
        session_id: &str,
        explicit_cfg: Option<xai_grok_tools::registry::types::ToolServerConfig>,
        bind_fingerprint: Option<serde_json::Value>,
    ) -> Option<(Arc<crate::session::WorkspaceSession>, RebindOutcome)> {
        let session = self.session(session_id)?;
        let Some(cfg) = explicit_cfg else {
            return Some((session, RebindOutcome::Reused));
        };
        let outcome = {
            let _update_guard = session.update_lock.lock().await;
            let snapshot = SessionSnapshot::capture(
                &session,
                &self.shared.activity_tracker,
                bind_fingerprint.as_ref(),
            )
            .await;
            match SwapPolicy::evaluate(&snapshot, SwapTrigger::OwnerRebind) {
                SwapDecision::Reuse => RebindOutcome::Reused,
                SwapDecision::Defer(reason) => {
                    record_swap_decision(
                        &self.shared.activity_tracker,
                        SwapTrigger::OwnerRebind,
                        session_id,
                        SwapAction::Deferred(reason),
                    );
                    tracing::warn!(
                        session_id = %session_id,
                        in_flight = snapshot.in_flight_calls(),
                        "session.bind: rebind swap (changed explicit toolset or stale-heal \
                         re-apply) deferred: tool calls in flight — keeping the existing \
                         toolset"
                    );
                    RebindOutcome::ReresolveDeferredInFlight
                }
                SwapDecision::Skip(reason) => {
                    record_swap_decision(
                        &self.shared.activity_tracker,
                        SwapTrigger::OwnerRebind,
                        session_id,
                        SwapAction::Skipped(reason),
                    );
                    tracing::warn!(
                        session_id = %session_id,
                        "session.bind: rebind carried a changed toolset config, but the \
                         session's toolset is externally owned (local bind) — keeping the \
                         existing toolset; the new config did NOT take effect"
                    );
                    RebindOutcome::KeptExternallyOwned
                }
                SwapDecision::Apply => {
                    match self
                        .resolve_and_swap_session_toolset_locked(
                            &session,
                            cfg,
                            bind_fingerprint,
                            SwapTrigger::OwnerRebind,
                        )
                        .await
                    {
                        Ok(SwapOutcome::Swapped) => {
                            tracing::info!(
                                session_id = %session_id,
                                "session.bind: rebind carried a changed toolset config — re-resolved \
                                 and swapped"
                            );
                            RebindOutcome::Reresolved
                        }
                        Ok(SwapOutcome::Reused) => RebindOutcome::Reused,
                        Ok(SwapOutcome::SkippedExternallyOwned) => {
                            tracing::warn!(
                                session_id = %session_id,
                                "session.bind: rebind carried a changed toolset config, but the \
                                 session's toolset is externally owned (local bind) — keeping the \
                                 existing toolset; the new config did NOT take effect"
                            );
                            RebindOutcome::KeptExternallyOwned
                        }
                        Err(e) => {
                            record_swap_decision(
                                &self.shared.activity_tracker,
                                SwapTrigger::OwnerRebind,
                                session_id,
                                SwapAction::ApplyFailed,
                            );
                            tracing::warn!(
                                session_id = %session_id, error = %e,
                                "session.bind: rebind toolset re-resolve failed — keeping the \
                                 existing toolset"
                            );
                            RebindOutcome::ReresolveFailed
                        }
                    }
                }
            }
        };
        Some((session, outcome))
    }
    pub async fn on_before_turn(
        &self,
        session_id: &str,
        payload: &xai_tool_protocol::turn_hook::BeforeTurnPayload,
    ) {
        self.sync_session_yolo_mode(session_id, payload.yolo_mode);
        let before_handle = self
            .on_turn_boundary(
                session_id,
                crate::session::checkpoint::TurnBoundary::turn_start(payload.turn_number),
            )
            .await;
        tracing::debug!(
            session = %session_id,
            turn = payload.turn_number,
            model = %payload.model_id,
            "workspace: before_turn processed"
        );
        self.shared
            .session_event_writer(session_id)
            .emit(Event::TurnStarted {
                session_id: session_id.to_owned(),
                turn_number: payload.turn_number,
                model_id: payload.model_id.clone(),
                yolo_mode: payload.yolo_mode,
                conversation_message_count: payload.conversation_message_count,
                session_relationship: decode_session_relationship(&payload.session_relationship),
                schema_version: payload.schema_version.clone(),
                redirect_kind: None,
            });
        if let Some(handle) = before_handle {
            self.shared
                .inflight_enqueues
                .insert((session_id.to_owned(), payload.turn_number), handle);
        }
    }
    /// Fire-and-forget `after_turn` hook path (legacy shells / local mode):
    /// turn-end work with detached enqueue handles, no ack. New shells use
    /// the request/response path ([`Self::compute_turn_injections`]) instead.
    pub async fn on_after_turn(
        &self,
        session_id: &str,
        payload: &xai_tool_protocol::turn_hook::AfterTurnPayload,
    ) {
        let _ = self.process_after_turn(session_id, payload).await;
    }
    async fn process_after_turn(
        &self,
        session_id: &str,
        payload: &xai_tool_protocol::turn_hook::AfterTurnPayload,
    ) -> (
        Option<tokio::task::JoinHandle<EnqueueOutcome>>,
        Option<tokio::task::JoinHandle<EnqueueOutcome>>,
    ) {
        let after_handle = self
            .on_turn_boundary(
                session_id,
                crate::session::checkpoint::TurnBoundary::turn_end(
                    payload.turn_number,
                    payload.duration_ms,
                    payload.outcome,
                    payload.written_repo_paths.clone(),
                ),
            )
            .await;
        tracing::debug!(
            session = %session_id,
            turn = payload.turn_number,
            outcome = ?payload.outcome,
            "workspace: after_turn processed"
        );
        self.shared
            .session_event_writer(session_id)
            .emit(Event::TurnEnded {
                outcome: turn_outcome_label(payload.outcome),
                cancellation_category: decode_cancellation_category(
                    payload.cancellation_category.as_deref(),
                ),
                cancellation_context: payload.cancellation_context.clone(),
            });
        self.spawn_tool_state_upload(session_id, payload.turn_number);
        let before_handle = self
            .shared
            .inflight_enqueues
            .remove(&(session_id.to_owned(), payload.turn_number))
            .map(|(_, handle)| handle);
        (before_handle, after_handle)
    }
    /// Answer a request/response `turn_hook` (sampler/shell → workspace).
    ///
    /// Both phases run the same turn-boundary work as their fire-and-forget
    /// hook counterparts (the server-side sampler signals turns ONLY through
    /// this request channel): `Before` drives [`Self::on_before_turn`]
    /// (including the YOLO-state sync) and answers with a no-op reply
    /// (injections are not computed yet); `After` runs the turn-end work,
    /// awaits this turn's enqueue outcomes under [`after_turn_watchdog`]
    /// (which MUST undercut the requester's hook timeout), and returns the
    /// artifact ack on `HookReply::after_turn_ack`.
    ///
    /// Each phase must be signalled through exactly ONE channel per client —
    /// fire-and-forget hook or request — otherwise its work runs twice.
    pub async fn compute_turn_injections(
        &self,
        session_id: &str,
        request: &xai_tool_protocol::turn_hook::TurnHookRequest,
    ) -> xai_tool_protocol::turn_hook::HookReply {
        use xai_tool_protocol::turn_hook::{HookReply, TurnHookRequest};
        match request {
            TurnHookRequest::Before(payload) => {
                self.on_before_turn(session_id, payload).await;
                HookReply::default()
            }
            TurnHookRequest::After(payload) => {
                let (before_handle, after_handle) =
                    self.process_after_turn(session_id, payload).await;
                let no_handle_skip_reason = if self.shared.data_collection_disabled {
                    "data_collection_disabled"
                } else {
                    "no_upload_queue"
                };
                let (status, artifact_count, error_message) = resolve_after_turn_ack(
                    before_handle,
                    after_handle,
                    after_turn_watchdog(),
                    no_handle_skip_reason,
                )
                .await;
                tracing::debug!(
                    session_id = %session_id,
                    turn_number = payload.turn_number,
                    ?status,
                    artifact_count,
                    "after_turn ack returned on hook reply"
                );
                HookReply {
                    after_turn_ack: Some(AfterTurnAckPayload {
                        turn_number: payload.turn_number,
                        status,
                        error_message,
                        artifact_count,
                    }),
                    ..HookReply::default()
                }
            }
            _ => HookReply::default(),
        }
    }
    /// Sync a before-turn hook's YOLO state into the session, emitting
    /// `YoloToggled` on transitions. No-op for unknown sessions.
    fn sync_session_yolo_mode(&self, session_id: &str, yolo_mode: bool) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let was = session.yolo_mode();
        if was != yolo_mode {
            tracing::info!(
                session = %session_id,
                from = was,
                to = yolo_mode,
                "workspace: yolo_mode changed via before-turn hook"
            );
            session.set_yolo_mode(yolo_mode);
            self.on_yolo_toggled(session_id, yolo_mode);
        }
    }
    /// Spawn an artifact-producer future tracked in the producer `TaskTracker`
    /// so status counts it and the durability idle gate withholds `idle_since_ms`
    /// while it runs; pokes status on start and completion. (The graceful drain
    /// added in the next PR awaits these tasks in phase 1.5 before flushing the
    /// queue — this PR only wires the tracking + idle-withholding.) Spawns after
    /// drain start stay tracked (the idle gate must not go blind) but are warned
    /// + counted as at-risk of missing the queue flush.
    pub(crate) fn spawn_producer<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if self.shared.activity_tracker.drain_started() {
            tracing::warn!(
                "producer spawned after drain start — artifact may miss the queue flush"
            );
            PRODUCER_SPAWNED_AFTER_DRAIN_TOTAL.inc();
        }
        let activity = self.shared.activity_tracker.clone();
        let tracked = self.shared.producer_tasks.track_future(fut);
        let handle = tokio::spawn(async move {
            let out = tracked.await;
            activity.poke();
            out
        });
        self.shared.activity_tracker.poke();
        handle
    }
    /// Spawn a fire-and-forget per-turn `tool_state.json` snapshot + upload to
    /// `{session_id}/turn_{N}/tool_state.json`. No-op when
    /// `GROK_WORKSPACE_TOOL_STATE_ENABLED` is off, opted out,
    /// there is no upload queue (local/test mode), or the
    /// session is unknown — legacy behavior unchanged.
    fn spawn_tool_state_upload(&self, session_id: &str, turn_number: u64) {
        if !crate::session::tool_config::tool_state_enabled() {
            return;
        }
        if self.shared.data_collection_disabled {
            return;
        }
        let Some(upload_queue) = self.shared.upload_queue.clone() else {
            dc_log!(
                debug,
                session_id = %session_id,
                turn_number,
                phase = "tool_state",
                outcome = "skipped",
                skip_reason = "no_upload_queue",
                "workspace: tool_state upload skipped — no upload queue"
            );
            crate::upload::record_upload_outcome("tool_state", "skipped");
            crate::upload::record_upload_skipped("tool_state", "no_upload_queue");
            return;
        };
        let Some(session) = self.session(session_id) else {
            dc_log!(
                warn,
                session_id = %session_id,
                turn_number,
                phase = "tool_state",
                outcome = "skipped",
                skip_reason = "no_session",
                "workspace: tool_state upload skipped — no bound session"
            );
            crate::upload::record_upload_outcome("tool_state", "skipped");
            crate::upload::record_upload_skipped("tool_state", "no_session");
            return;
        };
        let session_id = session_id.to_owned();
        self.spawn_producer(async move {
            if persist_and_enqueue_tool_state(
                session,
                session_id.clone(),
                turn_number,
                upload_queue,
            )
            .await
            .is_err()
            {
                dc_log!(
                    warn,
                    session_id = %session_id,
                    turn_number,
                    error_category = "enqueue_failed",
                    "workspace: tool_state upload failed"
                );
                crate::upload::record_upload_failed("tool_state", "enqueue_failed");
                crate::upload::record_upload_outcome("tool_state", "failed");
            }
        });
    }
    /// Drain the workspace's upload queue, waiting up to `deadline` for in-flight
    /// uploads to finish. Returns the number of items still pending after the
    /// deadline (0 when no queue is configured). Called from the workspace-server
    /// SIGTERM handler on graceful shutdown.
    pub async fn drain_upload_queue(&self, deadline: std::time::Duration) -> usize {
        match &self.shared.upload_queue {
            Some(queue) => queue.drain(deadline).await,
            None => 0,
        }
    }
    /// Serialize the session's workspace-side toolset to the Chat Completions
    /// tool-definitions shape and enqueue it (fire-and-forget) at the
    /// session-root path `{session_id}/workspace_tool_definitions.json`.
    ///
    /// This is the WORKSPACE-side subset; the shell's `tool_definitions.json`
    /// remains the source of truth for the full set the model sees — consumers
    /// union the two on `session_id`. Ordering is best-effort: the bind
    /// emission bypasses the 5s debounce (so it can't suppress the immediate
    /// post-bind `ToolsChanged` re-emit), and queue dispatch has no per-path
    /// ordering, so a stale baseline-only write may rarely clobber a fresher
    /// baseline+MCP snapshot — accepted as telemetry-only.
    ///
    /// No-op when the `GROK_WORKSPACE_TOOL_DEFS_ENABLED` flag is off, no upload
    /// queue is wired, or the session is unknown.
    pub(crate) fn emit_workspace_tool_definitions(&self, session_id: &str) {
        if !self.shared.tool_defs_enabled {
            return;
        }
        if !is_safe_object_segment(session_id) {
            self.shared.tool_defs_last_emit.remove(session_id);
            tracing::warn!(%session_id, "tool_defs: unsafe session id, skipping");
            return;
        }
        let Some(upload_queue) = self.shared.upload_queue.clone() else {
            return;
        };
        let Some((object_path, bytes)) = self.workspace_tool_definitions_payload(session_id) else {
            if self.session(session_id).is_none() {
                self.shared.tool_defs_last_emit.remove(session_id);
            }
            tracing::debug!(%session_id, "tool_defs: no payload, skipping");
            return;
        };
        let session_id = session_id.to_owned();
        self.spawn_producer(async move {
            let _ = enqueue_workspace_tool_definitions(
                &upload_queue,
                &session_id,
                &object_path,
                &bytes,
            )
            .await;
        });
    }
    /// Build the `(gcs_path, json_bytes)` payload for a session's workspace-side
    /// tool definitions, or `None` for an unknown session. Uses the same
    /// serializer as the shell's `tool_definitions.json`, so the two artifacts
    /// share a byte-identical element shape. Free of flag/queue gating for
    /// direct unit testing.
    fn workspace_tool_definitions_payload(&self, session_id: &str) -> Option<(String, Vec<u8>)> {
        let session = self.session(session_id)?;
        let definitions = session.toolset().tool_definitions();
        let bytes = serde_json::to_vec_pretty(&definitions)
            .inspect_err(|e| {
                tracing::warn!(%session_id, error = %e, "failed to serialize workspace tool definitions");
            })
            .ok()?;
        Some((workspace_tool_definitions_path(session_id), bytes))
    }
    /// Preemption-aware graceful drain: phase 1 waits for tool calls, phase 1.5
    /// for artifact producers, phase 2 flushes the upload queue (budgets per the
    /// `phase*_budget` helpers). Shared by the SIGTERM and server-evict triggers so
    /// they can't diverge.
    ///
    /// The preStop drain marker is (re)written at every phase boundary — not
    /// just once at the start — with the live total of outstanding durability
    /// work: active tool calls + background tasks (phase 1), in-flight artifact
    /// producers that have not yet enqueued (phase 1.5), and queued uploads
    /// (phase 2). This keeps a preStop hook from reading `0` while a tool call
    /// is still running (queue and producers both empty) or while later phases
    /// have yet to flush newly-produced work.
    ///
    /// Returns that same outstanding total after the deadline, so `0` means a
    /// fully clean drain — consistent with the final marker and
    /// [`DrainOutcome::Full`]; a wedged producer or tool call keeps it non-zero.
    pub async fn two_phase_drain(
        &self,
        grace_budget: std::time::Duration,
        reason: DrainReason,
    ) -> usize {
        let tracker = self.shared.activity_tracker.clone();
        let start = std::time::Instant::now();
        tracker.set_draining();
        tracker.poke();
        DRAIN_STARTED_TOTAL
            .with_label_values(&[reason.as_str()])
            .inc();
        let active_at_start = tracker.total_active() as usize;
        let pending_at_start = self.upload_queue_pending();
        let producers_at_start = self.shared.producer_tasks.len();
        let drain_file = draining_file_path();
        write_draining_marker(
            &drain_file,
            active_at_start + producers_at_start + pending_at_start,
        );
        dc_log!(
            info,
            drain_reason = reason.as_str(),
            grace_ms = grace_budget.as_millis() as u64,
            active_at_start,
            pending_at_start,
            producers_at_start,
            "workspace: two-phase drain commencing"
        );
        let phase1 = phase1_budget(grace_budget);
        let tools_idle = tokio::time::timeout(phase1, tracker.wait_until_tools_idle())
            .await
            .is_ok();
        if !tools_idle {
            tracing::warn!(
                active = tracker.total_active(),
                "drain phase 1 deadline exceeded — tool calls still in flight"
            );
        }
        write_draining_marker(&drain_file, self.outstanding_drain_work());
        let producers_done = wait_for_producers_idle(
            &self.shared.producer_tasks,
            phase15_budget(grace_budget.saturating_sub(start.elapsed())),
        )
        .await;
        if !producers_done {
            tracing::warn!(
                producers = self.shared.producer_tasks.len(),
                "drain phase 1.5 deadline exceeded — artifact producers still in flight"
            );
        }
        write_draining_marker(&drain_file, self.outstanding_drain_work());
        let phase2 = grace_budget.saturating_sub(start.elapsed());
        let unfinished = self.drain_upload_queue(phase2).await;
        let producers_unfinished = self.shared.producer_tasks.len();
        let active_unfinished = self.shared.activity_tracker.total_active() as usize;
        let total_unfinished = active_unfinished + producers_unfinished + unfinished;
        let outcome =
            classify_drain_outcome(tools_idle, producers_done, producers_unfinished, unfinished);
        DRAIN_COMPLETED_TOTAL
            .with_label_values(&[outcome.as_str()])
            .inc();
        DRAIN_DURATION.observe(start.elapsed().as_secs_f64());
        if unfinished > 0 {
            DRAIN_LOST_ITEMS_TOTAL.inc_by(unfinished as u64);
        }
        write_draining_marker(&drain_file, total_unfinished);
        if total_unfinished > 0 {
            tracing::warn!(
                reason = reason.as_str(),
                outcome = outcome.as_str(),
                active_unfinished,
                producers_unfinished,
                unfinished,
                total_unfinished,
                duration_ms = start.elapsed().as_millis() as u64,
                "workspace: two-phase drain finished with work still outstanding"
            );
        } else {
            tracing::info!(
                reason = reason.as_str(),
                outcome = outcome.as_str(),
                duration_ms = start.elapsed().as_millis() as u64,
                "workspace: two-phase drain complete"
            );
        }
        total_unfinished
    }
    /// Live pending upload-queue depth (0 when no queue is configured).
    fn upload_queue_pending(&self) -> usize {
        self.shared
            .upload_queue
            .as_ref()
            .map(|q| q.stats().pending.load(std::sync::atomic::Ordering::Relaxed) as usize)
            .unwrap_or(0)
    }
    /// Live total of outstanding durability work the two-phase drain must wait
    /// on: active tool calls + background tasks (phase 1) + in-flight artifact
    /// producers that have not yet enqueued (phase 1.5) + queued uploads
    /// (phase 2). Used to refresh the preStop drain marker at each phase
    /// boundary so it is never `0` while any phase still has work.
    fn outstanding_drain_work(&self) -> usize {
        self.shared.activity_tracker.total_active() as usize
            + self.shared.producer_tasks.len()
            + self.upload_queue_pending()
    }
    /// Bookkeeping for a cancelled in-flight tool call: marks it as
    /// completed in the activity tracker. Does **not** abort execution
    /// of the tool — that requires `CancellationToken` plumbing (future work).
    pub fn cancel_tool_call(&self, session_id: &str, call_id: &str) {
        self.shared.activity_tracker.tool_call_completed(
            call_id,
            Some(session_id),
            xai_file_utils::events::ToolOutcome::Cancelled,
        );
        tracing::info!(%session_id, %call_id, "cancel_tool_call: marked as completed");
    }
    /// Cancel all in-flight tool calls for a session. Called when a
    /// session-wide Cancel hook arrives (no specific `call_id`).
    pub fn cancel_all_tool_calls(&self, session_id: &str) {
        let count = self
            .shared
            .activity_tracker
            .cancel_all_session_calls(session_id);
        tracing::info!(%session_id, count, "cancel_all_tool_calls: marked all as completed");
    }
    /// Clean up workspace state for a session that has ended.
    /// Does **not** drop the session — that is handled by the server's
    /// `unbind_session` lifecycle.
    pub fn on_session_ended(&self, session_id: &str) {
        self.shared.activity_tracker.session_ended(session_id);
        self.shared.session_event_writers.remove(session_id);
        self.shared
            .inflight_enqueues
            .retain(|(sid, _), _| sid != session_id);
        self.shared.tool_defs_last_emit.remove(session_id);
        tracing::info!(%session_id, "session_ended cleanup completed");
    }
    /// Record a YOLO / always-approve mode toggle into the session's
    /// `events.jsonl`. These volatile-config mutations are shell-owned; this is
    /// the workspace-side emission entry point invoked by the server/shell forwarding
    /// layer when it observes a `SetYoloMode` command for a bound session. A no-op
    /// when events recording is disabled.
    pub fn on_yolo_toggled(&self, session_id: &str, enabled: bool) {
        self.shared
            .session_event_writer(session_id)
            .emit(Event::YoloToggled { enabled });
        tracing::debug!(%session_id, enabled, "workspace: yolo toggle recorded");
    }
    /// Record an MCP server enable/disable toggle into the session's
    /// `events.jsonl`. Like [`on_yolo_toggled`](Self::on_yolo_toggled), this is
    /// the workspace-side emission point for a shell-owned mutation; the server/shell
    /// forwarding layer calls it when it observes an MCP toggle for a bound
    /// session. A no-op when events recording is disabled.
    pub fn on_mcp_server_toggled(&self, session_id: &str, server_name: &str, enabled: bool) {
        self.shared
            .session_event_writer(session_id)
            .emit(Event::McpServerToggled {
                server_name: server_name.to_owned(),
                enabled,
            });
        tracing::debug!(%session_id, %server_name, enabled, "workspace: mcp toggle recorded");
    }
    /// Returns a cloned snapshot of the hook registry, disconnected
    /// from the workspace's live state.
    ///
    /// The registry is loaded once at workspace construction from the
    /// global and project sources in `WorkspaceConfig`; mid-session
    /// reloads (e.g. plugin hook appending) mutate the live registry
    /// in place via the `RwLock` on `WorkspaceShared`. The returned
    /// clone is not affected by subsequent mutations.
    pub fn hook_registry(&self) -> xai_grok_hooks::discovery::HookRegistry {
        self.shared.hook_registry.read().clone()
    }
    /// Non-fatal errors from the initial hook discovery pass at
    /// workspace construction time.
    ///
    /// Empty when all hook files parsed cleanly. Not updated on
    /// mid-session hook mutations (e.g. plugin hook appending).
    pub fn hook_load_errors(&self) -> &[xai_grok_hooks::error::HookError] {
        &self.shared.hook_load_errors
    }
    /// Canonicalize the workspace root directory.
    /// Called once per batch and passed to `resolve_service_path` for each file.
    pub(crate) async fn canonical_root(&self) -> WorkspaceResult<PathBuf> {
        Self::canonicalize_root_dir(&self.root_cwd()?).await
    }
    /// Canonicalize a confinement root directory.
    async fn canonicalize_root_dir(root: &std::path::Path) -> WorkspaceResult<PathBuf> {
        #[allow(clippy::disallowed_methods)]
        let canonical = tokio::fs::canonicalize(root).await.map_err(|e| {
            WorkspaceError::HubError(format!("failed to canonicalize workspace root: {e}"))
        })?;
        Ok(dunce::simplified(&canonical).to_path_buf())
    }
    /// Resolve a caller-provided path safely. Accepts a path relative to the
    /// workspace root, or an absolute path that resolves within the root;
    /// either form is confined to the root (paths that escape are rejected).
    /// Two-layer defense: textual normalization + symlink containment.
    ///
    /// # TOCTOU caveat
    /// The symlink check is point-in-time. If a symlink is created between
    /// resolution and I/O, containment is not guaranteed. Defense-in-depth
    /// (e.g., `O_NOFOLLOW`, mount namespaces) would be needed for hostile
    /// workspace environments, which is out of scope for this service-level API.
    pub(crate) async fn resolve_service_path(
        &self,
        req_path: &str,
        canonical_root: &std::path::Path,
    ) -> WorkspaceResult<PathBuf> {
        let root = self.root_cwd()?;
        Self::resolve_path_within_root(req_path, &root, canonical_root).await
    }
    /// Core of [`Self::resolve_service_path`], parameterized over the
    /// confinement root (see [`Self::confine_to_root`]).
    async fn resolve_path_within_root(
        req_path: &str,
        root: &std::path::Path,
        canonical_root: &std::path::Path,
    ) -> WorkspaceResult<PathBuf> {
        use std::path::{Component, Path};
        if req_path.is_empty() {
            return Err(WorkspaceError::HubError("empty path not allowed".into()));
        }
        let path = Path::new(req_path);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let mut components = Vec::new();
        for component in joined.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !components.is_empty()
                        && !matches!(components.last(), Some(Component::RootDir))
                    {
                        components.pop();
                    }
                }
                c => components.push(c),
            }
        }
        let normalized: PathBuf = components.into_iter().collect();
        if !normalized.starts_with(root) && !normalized.starts_with(canonical_root) {
            return Err(WorkspaceError::HubError(format!(
                "path escapes workspace root: {req_path}"
            )));
        }
        const MAX_SYMLINK_HOPS: usize = 40;
        let mut symlink_hops = 0usize;
        let mut check_path = normalized.clone();
        loop {
            #[allow(clippy::disallowed_methods)]
            match tokio::fs::canonicalize(&check_path).await {
                Ok(canonical) => {
                    let canonical = dunce::simplified(&canonical).to_path_buf();
                    if !canonical.starts_with(canonical_root) {
                        return Err(WorkspaceError::HubError(format!(
                            "path resolves outside workspace root (symlink escape): {req_path}"
                        )));
                    }
                    break;
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.kind() == std::io::ErrorKind::NotADirectory =>
                {
                    if let Ok(md) = tokio::fs::symlink_metadata(&check_path).await
                        && md.file_type().is_symlink()
                    {
                        if symlink_hops >= MAX_SYMLINK_HOPS {
                            return Err(WorkspaceError::HubError(format!(
                                "path resolves outside workspace root (unresolved symlink chain): {req_path}"
                            )));
                        }
                        let Ok(target) = tokio::fs::read_link(&check_path).await else {
                            return Err(WorkspaceError::HubError(format!(
                                "failed to resolve symlink for containment: {req_path}"
                            )));
                        };
                        symlink_hops += 1;
                        check_path = if target.is_absolute() {
                            target
                        } else {
                            check_path
                                .parent()
                                .map(|p| p.join(&target))
                                .unwrap_or(target)
                        };
                        continue;
                    }
                    match check_path.parent() {
                        Some(parent) if parent != check_path => {
                            check_path = parent.to_path_buf();
                        }
                        _ => {
                            tracing::warn!(
                                "symlink containment: parent chain exhausted without canonicalize for {req_path}"
                            );
                            break;
                        }
                    }
                }
                Err(e) => {
                    return Err(WorkspaceError::HubError(format!(
                        "failed to verify path containment: {e}"
                    )));
                }
            }
        }
        Ok(normalized)
    }
    /// Confine `path` to the workspace root (reject `..`, absolute-outside-root,
    /// symlink escapes) when confinement is enabled. Returns the resolved path and
    /// an optional walk root: `Some(root)` confines a `list`, `None` leaves it
    /// unconfined. Off by default (see
    /// [`WorkspaceConfig::confine_fs_to_workspace_root`](crate::config::WorkspaceConfig::confine_fs_to_workspace_root)):
    /// the absolute `path` is returned as-is, following out-of-root symlinks.
    pub async fn confine_to_workspace_root(
        &self,
        path: &std::path::Path,
    ) -> WorkspaceResult<(PathBuf, Option<PathBuf>)> {
        if !self.shared.confine_fs_to_workspace_root {
            return Ok((path.to_path_buf(), None));
        }
        let path_str = path.to_str().ok_or_else(|| {
            WorkspaceError::HubError(format!("non-UTF-8 path: {}", path.display()))
        })?;
        let canonical_root = self.canonical_root().await?;
        let confined = self.resolve_service_path(path_str, &canonical_root).await?;
        Ok((confined, Some(canonical_root)))
    }
    /// Like [`Self::confine_to_workspace_root`] but against an alternative trusted
    /// root (e.g. a worktree session cwd). Same gate; unconfined by default.
    pub async fn confine_to_root(
        &self,
        path: &std::path::Path,
        root: &std::path::Path,
    ) -> WorkspaceResult<(PathBuf, Option<PathBuf>)> {
        if !self.shared.confine_fs_to_workspace_root {
            return Ok((path.to_path_buf(), None));
        }
        let path_str = path.to_str().ok_or_else(|| {
            WorkspaceError::HubError(format!("non-UTF-8 path: {}", path.display()))
        })?;
        let canonical_root = Self::canonicalize_root_dir(root).await?;
        let confined = Self::resolve_path_within_root(path_str, root, &canonical_root).await?;
        Ok((confined, Some(canonical_root)))
    }
    /// Write files to the workspace filesystem (service-level, no hunk tracking).
    ///
    /// Files are written sequentially. If file N fails, files 1..N-1 are
    /// already on disk and will NOT be rolled back. Callers must inspect
    /// per-file results in the response to detect partial failures.
    pub async fn put_files(&self, files: Vec<PutFileEntry>) -> WorkspaceResult<PutFilesRes> {
        let canonical_root = self.canonical_root().await?;
        let mut results = Vec::with_capacity(files.len());
        for entry in files {
            let result = self.put_single_file(&entry, &canonical_root).await;
            results.push(result);
        }
        Ok(PutFilesRes { results })
    }
    async fn put_single_file(
        &self,
        entry: &PutFileEntry,
        canonical_root: &std::path::Path,
    ) -> PutFileResult {
        let resolved = match self.resolve_service_path(&entry.path, canonical_root).await {
            Ok(p) => p,
            Err(e) => {
                return PutFileResult {
                    path: entry.path.clone(),
                    ok: false,
                    error: Some(e.to_string()),
                    hash: None,
                };
            }
        };
        if entry.create_dirs
            && let Some(parent) = resolved.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return PutFileResult {
                path: entry.path.clone(),
                ok: false,
                error: Some(format!("failed to create directories: {e}")),
                hash: None,
            };
        }
        let write_result = if entry.append {
            use tokio::io::AsyncWriteExt;
            async {
                let mut f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&resolved)
                    .await?;
                f.write_all(entry.content.as_bytes()).await?;
                f.flush().await
            }
            .await
        } else {
            tokio::fs::write(&resolved, entry.content.as_bytes()).await
        };
        match write_result {
            Ok(()) => {
                let hash = sha256_hex(entry.content.as_bytes());
                PutFileResult {
                    path: entry.path.clone(),
                    ok: true,
                    error: None,
                    hash: Some(hash),
                }
            }
            Err(e) => PutFileResult {
                path: entry.path.clone(),
                ok: false,
                error: Some(e.to_string()),
                hash: None,
            },
        }
    }
    /// Read files from the workspace filesystem with optional cache
    /// validation and byte-range support.
    ///
    /// Files are read sequentially. Each result includes:
    /// - `exists`: whether the file exists on disk.
    /// - `content`: file content (full or requested byte range as UTF-8).
    /// - `hash`: SHA-256 hex digest of the **full** file content.
    /// - `matched`: true if `if_none_match` matched the current hash.
    /// - `size`: total file size in bytes.
    pub async fn get_files(&self, files: Vec<GetFileEntry>) -> WorkspaceResult<GetFilesRes> {
        let canonical_root = self.canonical_root().await?;
        let mut results = Vec::with_capacity(files.len());
        for entry in files {
            let result = self.get_single_file(&entry, &canonical_root).await;
            results.push(result);
        }
        Ok(GetFilesRes { results })
    }
    async fn get_single_file(
        &self,
        entry: &GetFileEntry,
        canonical_root: &std::path::Path,
    ) -> GetFileResult {
        let resolved = match self.resolve_service_path(&entry.path, canonical_root).await {
            Ok(p) => p,
            Err(e) => {
                return GetFileResult {
                    path: entry.path.clone(),
                    exists: false,
                    content: None,
                    hash: None,
                    matched: false,
                    size: None,
                    error: Some(e.to_string()),
                };
            }
        };
        let is_chunked = entry.offset.is_some() || entry.length.is_some();
        let metadata = match tokio::fs::metadata(&resolved).await {
            Ok(m) => m,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::NotADirectory =>
            {
                return GetFileResult {
                    path: entry.path.clone(),
                    exists: false,
                    content: None,
                    hash: None,
                    matched: false,
                    size: None,
                    error: None,
                };
            }
            Err(e) => {
                return GetFileResult {
                    path: entry.path.clone(),
                    exists: true,
                    content: None,
                    hash: None,
                    matched: false,
                    size: None,
                    error: Some(e.to_string()),
                };
            }
        };
        let file_size = metadata.len();
        if is_chunked {
            let req_offset = entry.offset.unwrap_or(0);
            let req_length = entry.length.unwrap_or(file_size.saturating_sub(req_offset));
            let read_result = stream_hash_and_range(&resolved, req_offset, req_length).await;
            match read_result {
                Ok((hash, chunk_bytes, _streamed)) => {
                    if let Some(ref etag) = entry.if_none_match
                        && *etag == hash
                    {
                        return GetFileResult {
                            path: entry.path.clone(),
                            exists: true,
                            content: None,
                            hash: Some(hash),
                            matched: true,
                            size: Some(file_size),
                            error: None,
                        };
                    }
                    match String::from_utf8(chunk_bytes) {
                        Ok(content) => GetFileResult {
                            path: entry.path.clone(),
                            exists: true,
                            content: Some(content),
                            hash: Some(hash),
                            matched: false,
                            size: Some(file_size),
                            error: None,
                        },
                        Err(e) => GetFileResult {
                            path: entry.path.clone(),
                            exists: true,
                            content: None,
                            hash: Some(hash),
                            matched: false,
                            size: Some(file_size),
                            error: Some(format!("not valid UTF-8 in range: {e}")),
                        },
                    }
                }
                Err(e) => GetFileResult {
                    path: entry.path.clone(),
                    exists: true,
                    content: None,
                    hash: None,
                    matched: false,
                    size: Some(file_size),
                    error: Some(e.to_string()),
                },
            }
        } else {
            match tokio::fs::read(&resolved).await {
                Ok(bytes) => {
                    let hash = sha256_hex(&bytes);
                    if let Some(ref etag) = entry.if_none_match
                        && *etag == hash
                    {
                        return GetFileResult {
                            path: entry.path.clone(),
                            exists: true,
                            content: None,
                            hash: Some(hash),
                            matched: true,
                            size: Some(file_size),
                            error: None,
                        };
                    }
                    match String::from_utf8(bytes) {
                        Ok(content) => GetFileResult {
                            path: entry.path.clone(),
                            exists: true,
                            content: Some(content),
                            hash: Some(hash),
                            matched: false,
                            size: Some(file_size),
                            error: None,
                        },
                        Err(e) => GetFileResult {
                            path: entry.path.clone(),
                            exists: true,
                            content: None,
                            hash: Some(hash),
                            matched: false,
                            size: Some(file_size),
                            error: Some(format!("not valid UTF-8: {e}")),
                        },
                    }
                }
                Err(e) => GetFileResult {
                    path: entry.path.clone(),
                    exists: true,
                    content: None,
                    hash: None,
                    matched: false,
                    size: Some(file_size),
                    error: Some(e.to_string()),
                },
            }
        }
    }
    /// Open a fuzzy file search index rooted at the workspace cwd.
    pub async fn fuzzy_open(
        &self,
        root: Option<&std::path::Path>,
        request_id: Option<String>,
        hidden: bool,
        session_id: Option<String>,
        target_client_id: crate::file_system::TargetClientId,
    ) -> String {
        let search_root = root.unwrap_or(&self.shared.root_cwd);
        let mut manager = self.shared.fuzzy_searches.lock().await;
        manager.open(
            search_root,
            request_id,
            hidden,
            session_id,
            target_client_id,
        )
    }
    /// Routing info (session id + target client) stored for a search at open
    /// time, read by the notification driver to address status updates.
    pub async fn fuzzy_routing(
        &self,
        search_id: &str,
    ) -> (Option<String>, crate::file_system::TargetClientId) {
        let manager = self.shared.fuzzy_searches.lock().await;
        (
            manager.get_session_id(search_id),
            manager.get_target_client_id(search_id),
        )
    }
    /// Run one poll tick for an active fuzzy search. Returns the next batch of
    /// results (paths absolutized against the search root) or a signal to keep
    /// polling / stop. Drives the `x.ai/search/fuzzy/status` notification loop.
    pub async fn fuzzy_poll(
        &self,
        search_id: &str,
        min_generation: usize,
        has_query: bool,
        query_version: usize,
        limit: usize,
    ) -> crate::file_system::FuzzyPollOutcome {
        use crate::file_system::FuzzyPollOutcome;
        let mut manager = self.shared.fuzzy_searches.lock().await;
        if !manager.is_current_query(search_id, query_version) {
            return FuzzyPollOutcome::Stale;
        }
        let root = manager.get_root(search_id);
        match manager.get_results_filtered(search_id, min_generation, has_query) {
            None => {
                if manager.get_results(search_id).is_none() {
                    FuzzyPollOutcome::Closed
                } else {
                    FuzzyPollOutcome::Pending
                }
            }
            Some(mut data) => {
                data.matches.truncate(limit);
                if let Some(root) = &root {
                    for m in &mut data.matches {
                        let path_str = m.path.to_string();
                        if !path_str.starts_with('/') {
                            m.path = root.join(&path_str).to_string_lossy().into_owned().into();
                        }
                    }
                }
                FuzzyPollOutcome::Update(data)
            }
        }
    }
    /// Update the query for an active fuzzy search.
    /// Returns (min_generation, has_query, query_version) if the search exists.
    pub async fn fuzzy_change(
        &self,
        search_id: &str,
        query: &str,
        dirs_only: bool,
    ) -> Option<(usize, bool, usize)> {
        let mut manager = self.shared.fuzzy_searches.lock().await;
        manager.change(search_id, query, dirs_only)
    }
    /// Get fuzzy search results.
    pub async fn fuzzy_get_results(
        &self,
        search_id: &str,
    ) -> Option<crate::file_system::FuzzySearchData> {
        let mut manager = self.shared.fuzzy_searches.lock().await;
        manager.get_results(search_id)
    }
    /// Close a fuzzy search.
    pub async fn fuzzy_close(&self, search_id: &str) -> bool {
        let mut manager = self.shared.fuzzy_searches.lock().await;
        manager.close(search_id)
    }
    /// Install the sink used to deliver workspace-originated ext-notifications
    /// to the client (gateway in local mode, hub in proxy mode).
    pub fn set_client_ext_sink(&self, sink: crate::session::ClientExtSink) {
        self.shared.client_ext_sink.store(Arc::new(Some(sink)));
    }
    /// Whether a client ext-notification sink has been installed.
    pub fn has_client_ext_sink(&self) -> bool {
        self.shared.client_ext_sink.load().is_some()
    }
    /// Deliver an ext-notification to the client via the installed sink.
    /// No-op when no sink is set.
    pub fn emit_client_ext(&self, method: String, params: serde_json::Value) {
        if let Some(sink) = self.shared.client_ext_sink.load_full().as_ref() {
            sink(method, params);
        }
    }
    /// Drive the `x.ai/search/fuzzy/status` stream for an active search: poll
    /// until done / closed / superseded, emitting each new result batch to the
    /// client through the ext-notification sink. Co-located with the manager so
    /// it polls in-process in both local and proxy mode.
    pub async fn run_fuzzy_notifications(
        &self,
        search_id: String,
        min_generation: usize,
        has_query: bool,
        query_version: usize,
        limit: usize,
    ) {
        use crate::file_system::FuzzyPollOutcome;
        use std::time::Duration;
        use tokio::time::interval;
        let (session_id, target_client_id) = self.fuzzy_routing(&search_id).await;
        let context_id = session_id.unwrap_or_else(|| "agent".to_string());
        let mut poll_interval = interval(Duration::from_millis(25));
        let mut last_generation: Option<usize> = None;
        let max_polls = 400;
        poll_interval.tick().await;
        for _ in 0..max_polls {
            poll_interval.tick().await;
            let data = match self
                .fuzzy_poll(&search_id, min_generation, has_query, query_version, limit)
                .await
            {
                FuzzyPollOutcome::Stale | FuzzyPollOutcome::Closed => break,
                FuzzyPollOutcome::Pending => continue,
                FuzzyPollOutcome::Update(data) => data,
            };
            if last_generation == Some(data.generation) {
                if data.done {
                    break;
                }
                continue;
            }
            last_generation = Some(data.generation);
            let mut params = serde_json::json!({
                "sessionId": context_id.as_str(),
                "searchId": search_id.as_str(),
                "matches": serde_json::to_value(&data.matches).unwrap_or_default(),
                "total": data.total,
                "done": data.done,
                "generation": data.generation,
            });
            if !target_client_id.is_none() {
                params["_meta"] = serde_json::json!({
                    "targetClientId": serde_json::to_value(&target_client_id).unwrap_or_default(),
                });
            }
            self.emit_client_ext("x.ai/search/fuzzy/status".to_string(), params);
            if data.done {
                break;
            }
        }
    }
    /// Run a content search (ripgrep) and return results.
    /// Run a streaming content (ripgrep) search rooted at `cwd`, emitting each
    /// batch as `x.ai/search/content/status` via the client sink, and returning
    /// the final result. Co-located with the sink so it streams in both modes.
    pub async fn run_content_search(
        &self,
        cwd: std::path::PathBuf,
        context_id: String,
        params: crate::file_system::ContentSearchParams,
    ) -> crate::error::WorkspaceResult<crate::file_system::ContentSearchData> {
        let handle = self.clone();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        crate::file_system::content_search_streaming(&cwd, &params, cancel, move |batch| {
            let params = serde_json::json!({
                "sessionId": context_id.as_str(),
                "files": serde_json::to_value(&batch.files).unwrap_or_default(),
                "totalMatches": batch.total_matches,
                "totalFiles": batch.total_files,
                "done": batch.done,
                "truncated": batch.truncated,
            });
            handle.emit_client_ext("x.ai/search/content/status".to_string(), params);
        })
        .await
        .map_err(|e| WorkspaceError::HubError(e.to_string()))
    }
    pub fn get_or_create_codebase_index(
        &self,
        cwd: std::path::PathBuf,
    ) -> (Arc<xai_codebase_graph::IndexManagerHandle>, bool) {
        self.shared.codebase_indexes.lock().get_or_create(cwd)
    }
    pub fn get_codebase_index(
        &self,
        cwd: &std::path::Path,
    ) -> Option<Arc<xai_codebase_graph::IndexManagerHandle>> {
        self.shared.codebase_indexes.lock().get(cwd)
    }
    fn spawn_codebase_index_event_forwarder(&self) -> tokio::task::JoinHandle<()> {
        let shared = self.shared.clone();
        let root_cwd = self.shared.root_cwd.clone();
        let index_root =
            crate::session::git::find_git_root_from_path(&root_cwd).unwrap_or(root_cwd);
        tokio::spawn(async move {
            let mut rx = shared.events.subscribe();
            loop {
                match rx.recv().await {
                    Ok(xai_grok_workspace_types::WorkspaceEvent::FsChanged { ref path, kind }) => {
                        if let Some(idx) = shared.codebase_indexes.lock().get(&index_root) {
                            let event =
                                crate::fs_notify::ws_event_to_codebase_graph_event(path, kind);
                            if let Err(e) = idx.send_event(event) {
                                tracing::debug!(error = %e, "codebase graph: fs event forward failed");
                            }
                        }
                    }
                    Ok(xai_grok_workspace_types::WorkspaceEvent::GitHeadChanged { .. }) => {
                        let idx_opt = shared.codebase_indexes.lock().get(&index_root);
                        if let Some(idx) = idx_opt {
                            crate::fs_notify::refresh_codebase_graph_after_head_change(
                                &idx,
                                &index_root,
                                &shared.events,
                            )
                            .await;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "codebase index event forwarder lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            tracing::debug!("codebase index event forwarder exited");
        })
    }
    /// Re-emit `workspace_tool_definitions.json` on every `ToolsChanged` event,
    /// debounced per session via [`tool_defs_reemit_gate`] so a cascade of
    /// reclassifications does not churn the file. Returns `None` (no task, no
    /// broadcast subscriber) when the feature flag is off; exits when the
    /// broadcast channel closes. The returned handle is tracked on `HubHandle`
    /// so shutdown aborts it — a reconnect must not stack a second subscriber.
    fn spawn_tool_definitions_event_forwarder(&self) -> Option<tokio::task::JoinHandle<()>> {
        if !self.shared.tool_defs_enabled {
            return None;
        }
        let handle = self.clone();
        Some(tokio::spawn(async move {
            let mut rx = handle.shared.events.subscribe();
            loop {
                match rx.recv().await {
                    Ok(xai_grok_workspace_types::WorkspaceEvent::ToolsChanged { session_id }) => {
                        if tool_defs_reemit_gate(
                            handle.shared.tool_defs_enabled,
                            &handle.shared.tool_defs_last_emit,
                            &session_id,
                            std::time::Instant::now(),
                            TOOL_DEFS_DEBOUNCE,
                        ) {
                            handle.emit_workspace_tool_definitions(&session_id);
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "tool definitions event forwarder lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            tracing::debug!("tool definitions event forwarder exited");
        }))
    }
    /// Post-creation session setup (browser service seeding, etc.).
    ///
    /// When the optional browser backend is enabled, seeds a fresh per-session `BrowserService`
    /// into the toolset unless one is already present (idempotent — safe
    /// against double-finalize on concurrent on-demand session creation).
    /// Toolset rebuilds carry the handle forward via
    /// [`WorkspaceSession::replace_carrying_browser_service`](crate::session::WorkspaceSession::replace_carrying_browser_service).
    ///
    /// Holds the session's `update_lock` for the whole read-check-insert so
    /// it cannot interleave with a concurrent toolset rebuild (which swaps
    /// in a fresh `FinalizedToolset` under the same lock) — otherwise the
    /// seed could land in a just-replaced, stale toolset and the live one
    /// would miss the browser service.
    ///
    /// Also the initial `workspace_tool_definitions.json` emission point.
    pub(crate) async fn finalize_session_setup(&self, session: &crate::session::WorkspaceSession) {
        let _update_guard = session.update_lock.lock().await;
        self.emit_workspace_tool_definitions(session.session_id());
        self.maybe_emit_environment(session.session_id(), session.cwd());
    }
    /// Emit `workspace_environment.json` once at session bind. Emission is
    /// unconditional except for the legitimate suppression conditions below:
    /// it is a no-op when opted out or when
    /// there is no upload queue. Runs as a tracked producer task so the bind
    /// path never waits on the enqueue and the drain/idle gating still sees the
    /// in-flight work.
    fn maybe_emit_environment(&self, session_id: &str, cwd: &std::path::Path) {
        if self.shared.data_collection_disabled {
            return;
        }
        let trace_parent = fastrace::collector::SpanContext::current_local_parent();
        let this = self.clone();
        let session_id = session_id.to_owned();
        let cwd = cwd.to_path_buf();
        self.spawn_producer(async move {
            let _ = this
                .emit_environment_artifact(&session_id, &cwd, trace_parent)
                .await;
        });
    }
    /// Build and enqueue the environment artifact at the session-root path.
    /// Flag-independent core (the flag check lives in `maybe_emit_environment`)
    /// so it is unit-testable; returns `None` when there is no upload queue.
    async fn emit_environment_artifact(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        trace_parent: Option<fastrace::collector::SpanContext>,
    ) -> Option<xai_file_utils::queue::EnqueueOutcome> {
        let upload_queue = self.shared.upload_queue.clone()?;
        if !is_safe_object_segment(session_id) {
            tracing::warn!(%session_id, "environment: unsafe session id, skipping");
            return None;
        }
        let env = {
            let session_id_owned = session_id.to_owned();
            let cwd = cwd.to_path_buf();
            let identity = self.shared.identity().clone();
            let server_id = self.shared.server_id();
            let sandbox_id = self.shared.server_metadata_typed().sandbox_id;
            match tokio::task::spawn_blocking(move || {
                crate::upload::environment::WorkspaceEnvironment::capture(
                    &session_id_owned,
                    &cwd,
                    &identity,
                    server_id,
                    sandbox_id,
                )
            })
            .in_span(
                fastrace::Span::root(
                    "tool_server.session_bind.environment_capture",
                    trace_parent.unwrap_or_else(xai_tracing::local_or_random_span_ctx),
                )
                .with_properties(|| {
                    [
                        ("session_id", session_id.to_owned()),
                        ("force_tracing", "true".to_owned()),
                    ]
                }),
            )
            .await
            {
                Ok(env) => env,
                Err(e) if e.is_cancelled() => {
                    tracing::debug!(%session_id, "environment: capture cancelled during shutdown");
                    return None;
                }
                Err(e) => {
                    dc_log!(
                        warn,
                        session_id = %session_id,
                        "workspace: environment capture panicked"
                    );
                    ENV_CAPTURE_PANIC_TOTAL.inc();
                    tracing::warn!(
                        %session_id,
                        error = %e,
                        "workspace: environment capture task panicked"
                    );
                    return None;
                }
            }
        };
        let bytes = match env.to_json_bytes() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "workspace: failed to serialize workspace_environment.json"
                );
                return None;
            }
        };
        let gcs_path = format!("{session_id}/workspace_environment.json");
        let outcome = upload_queue
            .enqueue_bytes_blocking(
                &bytes,
                &gcs_path,
                "application/json",
                "workspace_environment",
                session_id,
                0,
            )
            .await;
        match &outcome {
            xai_file_utils::queue::EnqueueOutcome::Failed { reason: _ } => {
                dc_log!(
                    warn,
                    session_id = %session_id,
                    error_category = "enqueue_failed",
                    "workspace: environment artifact enqueue failed"
                );
                crate::upload::record_upload_failed("workspace_environment", "enqueue_failed");
                crate::upload::record_upload_outcome("workspace_environment", "failed");
            }
            _ => {
                dc_log!(
                    info,
                    session_id = %session_id,
                    bytes = bytes.len(),
                    "workspace: environment artifact enqueued"
                );
                crate::upload::record_upload_outcome("workspace_environment", "succeeded");
            }
        }
        Some(outcome)
    }
    /// Start MCP servers for a session and bridge them to the server.
    pub async fn start_session_mcp_servers(
        &self,
        session_id: &str,
        configs: Vec<agent_client_protocol::McpServer>,
    ) -> crate::error::WorkspaceResult<crate::mcp::McpStartResult> {
        use crate::mcp::{
            McpClientTransportAdapter, McpStartFailure, McpStartResult, QualifiedMcpToolHandler,
            make_bridge_config, server_name_from_mcp_error,
        };
        use xai_computer_hub_mcp_adapter::McpBridge;
        use xai_computer_hub_sdk::ToolServerHandler as _;
        use xai_grok_mcp::servers::MCP_TOOL_NAME_DELIMITER;
        use xai_tool_protocol::SessionId;
        let tool_server = {
            let hub_guard = self.shared.hub_handle.lock().await;
            let hub = hub_guard
                .as_ref()
                .ok_or_else(|| WorkspaceError::HubError("no hub connection".into()))?;
            hub.server.clone()
        };
        let session = self
            .session(session_id)
            .ok_or_else(|| WorkspaceError::SessionNotFound(session_id.to_owned()))?;
        let sid = SessionId::new(session_id)
            .map_err(|e| WorkspaceError::HubError(format!("invalid session_id: {e}")))?;
        {
            let mut tool_ids = session.mcp_tool_ids.lock().await;
            for tid in tool_ids.drain(..) {
                let _ = tool_server.unregister_tool_dynamic(&tid, &sid).await;
            }
            let mut existing_bridges = session.mcp_bridges.lock().await;
            existing_bridges.clear();
            let mut state = session.mcp_state.lock().await;
            state.owned_clients.clear();
        }
        let session_id_owned = session_id.to_owned();
        let event_writer = self.shared.session_event_writer(session_id);
        let rt_handle = tokio::runtime::Handle::current();
        let mcp_results: Vec<
            Result<xai_grok_mcp::servers::McpClient, xai_grok_mcp::servers::McpError>,
        > = tokio::task::spawn_blocking(move || {
            use std::collections::HashMap;
            use xai_grok_mcp::oauth_config::McpOAuthConfigMap;
            use xai_grok_mcp::servers::{McpClientTimeoutOverrides, McpMetaConfigMap};
            let overrides_map: HashMap<String, McpClientTimeoutOverrides> = HashMap::new();
            let meta_config_map = McpMetaConfigMap::new();
            let oauth_config_map = McpOAuthConfigMap::new();
            rt_handle.block_on(xai_grok_mcp::servers::start_mcp_servers(
                configs,
                Some(&session_id_owned),
                &overrides_map,
                &meta_config_map,
                &oauth_config_map,
                &event_writer,
                xai_grok_mcp::servers::OauthInteractivity::Interactive,
            ))
        })
        .await
        .map_err(|e| WorkspaceError::JoinError(e.to_string()))?;
        let mcp_state = session.mcp_state.clone();
        let mut started = Vec::new();
        let mut failed = Vec::new();
        let mut bridges = Vec::new();
        let mut registered_tool_ids = Vec::new();
        for result in mcp_results {
            match result {
                Ok(client) => {
                    let server_name = client.server_name().to_owned();
                    let client = Arc::new(client);
                    {
                        let mut state = mcp_state.lock().await;
                        state
                            .owned_clients
                            .insert(server_name.clone(), Arc::clone(&client));
                    }
                    let transport: Arc<dyn xai_computer_hub_mcp_adapter::McpTransport> =
                        Arc::new(McpClientTransportAdapter::new(Arc::clone(&client)));
                    let bridge_config = make_bridge_config(sid.clone(), &server_name);
                    match McpBridge::connect(transport, &bridge_config).await {
                        Ok(handle) => {
                            for handler in handle.bridge.handlers() {
                                let qualified_name = format!(
                                    "{}{}{}",
                                    server_name,
                                    MCP_TOOL_NAME_DELIMITER,
                                    handler.tool_id()
                                );
                                let qualified = match QualifiedMcpToolHandler::try_new(
                                    qualified_name.clone(),
                                    handler.clone(),
                                ) {
                                    Some(h) => Arc::new(h),
                                    None => continue,
                                };
                                if let Err(e) = tool_server
                                    .register_tool_dynamic(qualified, vec![sid.clone()])
                                    .await
                                {
                                    tracing::warn!(
                                        server = %server_name,
                                        tool = %qualified_name,
                                        error = %e,
                                        "failed to register MCP tool on hub"
                                    );
                                } else if let Ok(tid) =
                                    xai_tool_protocol::ToolId::new(&qualified_name)
                                {
                                    registered_tool_ids.push(tid);
                                }
                            }
                            bridges.push(handle);
                            started.push(server_name);
                        }
                        Err(e) => {
                            {
                                let mut state = mcp_state.lock().await;
                                state.owned_clients.remove(&server_name);
                            }
                            tracing::warn!(
                                server = %server_name,
                                error = %e,
                                "McpBridge::connect failed"
                            );
                            failed.push(McpStartFailure {
                                name: server_name,
                                error: e.to_string(),
                            });
                        }
                    }
                }
                Err(e) => {
                    let name = server_name_from_mcp_error(&e).to_owned();
                    tracing::warn!(
                        server = %name,
                        error = %e,
                        "MCP server start failed"
                    );
                    failed.push(McpStartFailure {
                        name,
                        error: e.to_string(),
                    });
                }
            }
        }
        {
            let mut session_bridges = session.mcp_bridges.lock().await;
            session_bridges.extend(bridges);
        }
        {
            let mut ids = session.mcp_tool_ids.lock().await;
            ids.extend(registered_tool_ids);
        }
        tracing::info!(
            session_id = %session_id,
            started = ?started,
            failed_count = failed.len(),
            "session MCP servers initialized"
        );
        if !started.is_empty() {
            let _ =
                self.shared
                    .events
                    .send(xai_grok_workspace_types::WorkspaceEvent::ToolsChanged {
                        session_id: session_id.to_owned(),
                    });
        }
        Ok(McpStartResult { started, failed })
    }
    /// Unregister all MCP tools for a session from the server.
    pub async fn teardown_session_mcp(&self, session_id: &str) {
        let tool_server = {
            let hub_guard = self.shared.hub_handle.lock().await;
            match hub_guard.as_ref() {
                Some(hub) => hub.server.clone(),
                None => return,
            }
        };
        let session = match self.session(session_id) {
            Some(s) => s,
            None => return,
        };
        let sid = match xai_tool_protocol::SessionId::new(session_id) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut tool_ids = session.mcp_tool_ids.lock().await;
        for tid in tool_ids.drain(..) {
            let _ = tool_server.unregister_tool_dynamic(&tid, &sid).await;
        }
        let mut bridges = session.mcp_bridges.lock().await;
        bridges.clear();
        let mut state = session.mcp_state.lock().await;
        state.owned_clients.clear();
    }
    /// Look up an existing session.
    pub fn session(&self, session_id: &str) -> Option<Arc<WorkspaceSession>> {
        self.shared.sessions.read().get(session_id).cloned()
    }
    /// IDs of all sessions currently bound to this workspace.
    pub fn session_ids(&self) -> Vec<String> {
        self.shared.sessions.read().keys().cloned().collect()
    }
    pub fn session_count(&self) -> usize {
        self.shared.sessions.read().len()
    }
    /// Fork a new subagent session. Clones (not references) the parent's
    /// tool config and env. Enforces capability subset and fork budget.
    ///
    /// Forks go through the same post-creation setup as hub-bound sessions
    /// ([`Self::finalize_session_setup`]): each fork gets its own browser
    /// service rather than sharing the parent's tabs.
    pub async fn fork_session(
        &self,
        config: AgentSessionConfig,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        if config.agent_id.is_empty() {
            return Err(WorkspaceError::EmptyAgentId);
        }
        let parent_id = config.parent_session_id.clone().ok_or_else(|| {
            WorkspaceError::ParentSessionNotFound(
                "fork_session requires an explicit parent_session_id".into(),
            )
        })?;
        let parent = self
            .shared
            .sessions
            .read()
            .get(&parent_id)
            .cloned()
            .ok_or_else(|| WorkspaceError::ParentSessionNotFound(parent_id.clone()))?;
        if !config.capability_mode.is_subset_of(parent.capability_mode) {
            return Err(WorkspaceError::CapabilityWidening {
                parent: parent.capability_mode,
                child: config.capability_mode,
            });
        }
        if parent.fork_budget == 0 {
            return Err(WorkspaceError::MaxDepthExceeded { parent: parent_id });
        }
        let new_depth = parent.depth.saturating_add(1);
        let new_fork_budget = parent.fork_budget.saturating_sub(1).min(config.max_depth);
        let baseline = config
            .tool_config
            .clone()
            .unwrap_or_else(|| (*parent.effective_tool_config()).clone());
        let cwd = config
            .cwd_override
            .clone()
            .unwrap_or_else(|| parent.cwd.clone());
        let mut env: std::collections::HashMap<String, String> = (**parent.session_env()).clone();
        env.extend(config.extra_env.clone());
        let session_env = Arc::new(env);
        let mcp_snapshot = self.shared.mcp_tools_snapshot.load_full();
        let hub_snapshot = self.shared.hub_tools_snapshot.load_full();
        let inherited_viewer_ctx = parent.viewer_ctx().cloned();
        let (effective, toolset, terminal_backend) = resolve_session_toolset(
            baseline,
            config.capability_mode,
            &mcp_snapshot,
            &hub_snapshot,
            cwd.clone(),
            session_env.clone(),
            &config.agent_id,
            self.shared.session_factory.as_ref(),
            Some(self.shared.local_registry.clone()),
            self.shared.lsp.clone(),
            inherited_viewer_ctx.clone(),
            self.shared.compose_session_notification_handle(None),
        )?;
        let (hunk_event_tx, _hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let hunk_cancel = tokio_util::sync::CancellationToken::new();
        let hunk_tracker = HunkTrackerActor::spawn(
            config.agent_id.clone(),
            cwd.clone(),
            hunk_event_tx,
            TrackingMode::AllDirty,
            hunk_cancel.clone(),
        );
        let session = Arc::new(WorkspaceSession::new(
            config.agent_id.clone(),
            cwd,
            session_env,
            config.capability_mode,
            new_depth,
            new_fork_budget,
            Arc::new(effective),
            toolset,
            terminal_backend,
            hunk_tracker,
            Some(hunk_cancel),
            inherited_viewer_ctx,
            false,
            None,
        ));
        {
            let mut sessions = self.shared.sessions.write();
            if self.shared.activity_tracker.is_draining() {
                session.cancel_hunk_tracker();
                return Err(WorkspaceError::ShuttingDown);
            }
            if sessions.contains_key(&config.agent_id) {
                session.cancel_hunk_tracker();
                return Err(WorkspaceError::SessionAlreadyExists(config.agent_id));
            }
            sessions.insert(config.agent_id.clone(), session.clone());
        }
        record_toolset_swap(&self.shared.activity_tracker, "fork", session.session_id());
        self.finalize_session_setup(&session).await;
        Ok(session)
    }
    /// Remove a session.
    pub fn drop_session(&self, caller_session_id: &str, session_id: &str) -> WorkspaceResult<()> {
        if caller_session_id != session_id {
            return Err(WorkspaceError::Unauthorized {
                caller: caller_session_id.to_owned(),
                target: session_id.to_owned(),
            });
        }
        let mut sessions = self.shared.sessions.write();
        let Some(session) = sessions.remove(session_id) else {
            return Err(WorkspaceError::SessionNotFound(session_id.to_owned()));
        };
        drop(sessions);
        session.abort_system_notify_forwarder();
        session.shutdown_terminal_backend();
        session.cancel_hunk_tracker();
        self.shared.tool_defs_last_emit.remove(session_id);
        Ok(())
    }
    /// Re-resolve every session's toolset against `new_snapshot` and
    /// emit one `WorkspaceEvent::ToolsChanged` per session.
    pub fn on_mcp_snapshot_changed(
        &self,
        new_snapshot: Vec<xai_grok_tools::registry::types::ToolConfig>,
    ) -> usize {
        self.shared.mcp_tools_snapshot.store(Arc::new(new_snapshot));
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                self.shared
                    .re_resolve_all_sessions("mcp_snapshot_changed", true),
            )
        })
    }
    /// Bulk-replace hub tool configs and re-resolve every session.
    pub fn on_hub_tools_changed(
        &self,
        new_hub_tools: Vec<xai_grok_tools::registry::types::ToolConfig>,
    ) -> usize {
        self.shared
            .hub_tools_snapshot
            .store(Arc::new(new_hub_tools));
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                self.shared
                    .re_resolve_all_sessions("hub_tools_changed", true),
            )
        })
    }
    /// Per-`session.bind` handler resolver: resolves the bind metadata into a
    /// session toolset (fail-closed in strict mode) and returns the handlers
    /// plus the bind-report fields. Extracted from `connect_hub` so tests can
    /// drive the full bind path without a hub connection.
    pub(crate) fn session_bind_resolver(
        &self,
        catalog: Arc<Vec<Arc<dyn xai_computer_hub_sdk::ToolServerHandler>>>,
        rpc_tool_id: xai_tool_protocol::ToolId,
    ) -> xai_computer_hub_sdk::SessionHandlerResolver {
        let weak_shared = Arc::downgrade(&self.shared);
        Arc::new(
            move |sid: xai_tool_protocol::SessionId, params: Option<serde_json::Value>| {
                let catalog = catalog.clone();
                let rpc_tool_id = rpc_tool_id.clone();
                let weak_shared = weak_shared.clone();
                let bind_parent = params
                    .as_ref()
                    .and_then(|p| p.pointer("/trace_context"))
                    .and_then(serde_json::Value::as_str)
                    .and_then(fastrace::collector::SpanContext::decode_w3c_traceparent)
                    .unwrap_or_else(xai_tracing::local_or_random_span_ctx);
                let bind_span = fastrace::Span::root("tool_server.session_bind", bind_parent)
                    .with_properties(|| {
                        [
                            ("session_id", sid.to_string()),
                            ("force_tracing", "true".to_owned()),
                        ]
                    });
                Box::pin(
                async move {
                    let Some(shared) = weak_shared.upgrade() else {
                        WORKSPACE_BIND_FAILED_TOTAL
                            .with_label_values(&["workspace_shutdown"])
                            .inc();
                        return Err(
                            xai_tool_runtime::ToolError::service_unavailable(
                                "workspace is shutting down; cannot bind session",
                            ),
                        );
                    };
                    let ws = WorkspaceHandle { shared };
                    let sid_str = sid.to_string();
                    let params = params.unwrap_or(serde_json::Value::Null);
                    let bind_cwd = params
                        .pointer("/cwd")
                        .and_then(serde_json::Value::as_str)
                        .map(std::path::PathBuf::from);
                    let bind_config = params
                        .pointer("/metadata")
                        .map(crate::config::WorkspaceBindConfig::from_metadata)
                        .unwrap_or_default();
                    let empty_toolset = || xai_grok_tools::registry::types::ToolServerConfig {
                        tools: vec![],
                        behavior_preset: None,
                    };
                    let mut resolve_zero_reason: Option<&'static str> = None;
                    let mut resolve_error: Option<String> = None;
                    let mut unserved_tool_ids: Vec<String> = Vec::new();
                    let known_ids = ws.shared.session_factory.known_tool_ids();
                    let known_id = |id: &str| known_ids.contains(id);
                    let require_explicit = ws.shared.require_explicit_toolset;
                    let tool_config = match bind_config
                        .resolve(&known_id, require_explicit)
                    {
                        crate::config::ResolvedToolset::Toolset(resolved) => {
                            unserved_tool_ids = resolved.unserved_tool_ids;
                            Some(resolved.toolset)
                        }
                        crate::config::ResolvedToolset::UseDefault => None,
                        crate::config::ResolvedToolset::MissingToolConfig => {
                            if bind_config.rpc_only {
                                tracing::info!(
                                    session_id = %sid_str,
                                    "session.bind: rpc_only bind with no toolset — \
                                     failing closed with an empty toolset"
                                );
                            } else {
                                tracing::warn!(
                                    session_id = %sid_str,
                                    "session.bind: no explicit tool configuration passed and this \
                                     workspace requires one — failing closed with an empty toolset"
                                );
                            }
                            resolve_zero_reason = Some("missing_tool_config");
                            resolve_error = Some(
                                format!(
                                "missing_tool_config: no usable explicit tool configuration \
                                 on session.bind (absent, or dropped as malformed — see \
                                 server logs) and this workspace requires one (presets are \
                                 not supported; server version {})",
                                xai_grok_version::VERSION
                            ),
                            );
                            Some(empty_toolset())
                        }
                        crate::config::ResolvedToolset::InvalidToolConfig(err) => {
                            tracing::warn!(
                                session_id = %sid_str, error = %err,
                                "session.bind: invalid tool config entry — failing closed with an empty toolset"
                            );
                            resolve_zero_reason = Some("invalid_tool_config");
                            resolve_error = Some(
                                format!(
                                "invalid_tool_config: {err} (server version {})",
                                xai_grok_version::VERSION
                            ),
                            );
                            Some(empty_toolset())
                        }
                    };
                    let (explicit_cfg, bind_fingerprint) = match (
                        &tool_config,
                        resolve_zero_reason,
                    ) {
                        (Some(cfg), None) if !cfg.tools.is_empty() => {
                            (Some(cfg.clone()), serde_json::to_value(cfg).ok())
                        }
                        _ => (None, None),
                    };
                    let capability = bind_config
                        .capability_mode
                        .unwrap_or(crate::capability::CapabilityMode::All);
                    let yolo_mode = bind_config.yolo_mode.unwrap_or(false);
                    tracing::info!(
                        session_id = %sid_str,
                        cwd = ?bind_cwd,
                        preset = ?bind_config.preset,
                        capability = ?capability,
                        yolo_mode,
                        "session.bind: resolving workspace session toolset"
                    );
                    let created = {
                        let _span = LocalSpan::enter_with_local_parent(
                                "tool_server.session_bind.create_session",
                            )
                            .with_property(|| ("session_id", sid_str.clone()));
                        ws.create_session_with_config(
                            sid_str.clone(),
                            bind_cwd,
                            tool_config,
                            capability,
                            bind_config.viewer_ctx.clone(),
                            bind_config.system_notifications,
                        )
                    };
                    let session = match created {
                        Ok(session) => {
                            session.set_yolo_mode(yolo_mode);
                            session
                                .set_bind_tool_config_fingerprint_if_unset(
                                    bind_fingerprint.clone(),
                                );
                            ws.finalize_session_setup(&session)
                                .in_span(
                                    fastrace::Span::enter_with_local_parent(
                                            "tool_server.session_bind.finalize",
                                        )
                                        .with_property(|| ("session_id", sid_str.clone())),
                                )
                                .await;
                            tracing::info!(
                                session_id = %sid_str,
                                "workspace session created for hub bind"
                            );
                            session
                        }
                        Err(crate::error::WorkspaceError::SessionAlreadyExists(_)) => {
                            match ws
                                .rebind_existing_hub_session(
                                    &sid_str,
                                    explicit_cfg,
                                    bind_fingerprint,
                                )
                                .await
                            {
                                Some((session, RebindOutcome::Reresolved)) => session,
                                Some((session, _)) => {
                                    unserved_tool_ids.clear();
                                    if resolve_zero_reason != Some("invalid_tool_config")
                                        && !session.effective_tool_config().tools.is_empty()
                                    {
                                        resolve_error = None;
                                        resolve_zero_reason = None;
                                    }
                                    session
                                }
                                None => {
                                    WORKSPACE_BIND_FAILED_TOTAL
                                        .with_label_values(&["session_lookup_failed"])
                                        .inc();
                                    return Err(
                                        xai_tool_runtime::ToolError::service_unavailable(
                                            format!(
                                            "session rebind raced teardown for `{sid_str}`; retry"
                                        ),
                                        ),
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                session_id = %sid_str, error = %e,
                                "failed to create workspace session for hub bind"
                            );
                            WORKSPACE_BIND_FAILED_TOTAL
                                .with_label_values(&["session_error"])
                                .inc();
                            return Err(
                                xai_tool_runtime::ToolError::service_unavailable(
                                    format!("failed to create workspace session: {e}"),
                                ),
                            );
                        }
                    };
                    let mut handlers = {
                        let _span = LocalSpan::enter_with_local_parent(
                                "tool_server.session_bind.handlers",
                            )
                            .with_property(|| ("session_id", sid_str.clone()));
                        build_session_routed_handlers(&session.toolset(), &ws)
                    };
                    let advertised: Vec<String> = handlers
                        .iter()
                        .map(|h| h.tool_id().as_str().to_owned())
                        .collect();
                    WORKSPACE_BIND_ADVERTISED_TOOLS.observe(advertised.len() as f64);
                    if advertised.is_empty() {
                        let reason = resolve_zero_reason.unwrap_or("empty_after_filter");
                        let skip_zero_metric = bind_config.rpc_only
                            && reason == "missing_tool_config";
                        if skip_zero_metric {
                            tracing::info!(
                                session_id = %sid_str,
                                reason,
                                "session.bind: advertising zero model-facing tools (rpc_only)"
                            );
                        } else {
                            tracing::warn!(
                                session_id = %sid_str,
                                "session.bind: advertising zero model-facing tools (RPC handler only)"
                            );
                            WORKSPACE_BIND_ZERO_TOOLS_TOTAL
                                .with_label_values(&[reason])
                                .inc();
                        }
                    }
                    handlers
                        .extend(
                            catalog
                                .iter()
                                .filter(|h| h.tool_id() == rpc_tool_id)
                                .cloned(),
                        );
                    if !unserved_tool_ids.is_empty() {
                        WORKSPACE_BIND_UNSERVED_TOOLS_TOTAL
                            .inc_by(unserved_tool_ids.len() as u64);
                        tracing::warn!(
                            session_id = %sid_str,
                            unserved = ?unserved_tool_ids,
                            "session.bind: serving partial pinned toolset"
                        );
                    }
                    tracing::info!(
                        session_id = %sid_str,
                        advertised = advertised.len(),
                        tools = ?advertised,
                        unserved = ?unserved_tool_ids,
                        "session.bind: advertising finalized session toolset"
                    );
                    Ok(xai_computer_hub_sdk::ResolvedSessionHandlers {
                        handlers,
                        unserved_tool_ids,
                        resolve_error,
                    })
                }
                    .in_span(bind_span),
            )
            },
        )
    }
    /// Connect to the server, start the tool server (provider
    /// direction) and notification listener (consumer direction).
    ///
    /// No-op if no `hub_config` was provided or already connected.
    ///
    /// The tool server exposes the workspace's main session tools so
    /// the server can dispatch `tool_call_request` frames to them. The
    /// notification listener updates `hub_tools_snapshot` and
    /// re-resolves every session's toolset whenever the server announces
    /// tool changes.
    pub async fn connect_hub(&self) -> WorkspaceResult<()> {
        use crate::hub::{HubHandle, apply_tools_changed, hub_result};
        tracing::info!("WorkspaceHandle::connect_hub — starting");
        let hub_config = match &self.shared.hub_config {
            Some(c) => {
                let mut cfg = c.clone();
                cfg.activity_tracker = Some(self.shared.activity_tracker.clone());
                cfg
            }
            None => {
                tracing::info!("WorkspaceHandle::connect_hub — no hub config, skipping");
                return Ok(());
            }
        };
        let mut hub_guard = self.shared.hub_handle.lock().await;
        if hub_guard.is_some() {
            return Ok(());
        }
        tracing::info!(url = %hub_config.url, "WorkspaceHandle::connect_hub — connecting to hub");
        let (template_handlers, rpc_tool_id) = {
            let session_env = Arc::new(std::collections::HashMap::new());
            let mcp_snapshot = self.shared.mcp_tools_snapshot.load_full();
            let hub_snapshot = self.shared.hub_tools_snapshot.load_full();
            let (_, template_toolset, _template_backend) = resolve_session_toolset(
                self.shared.default_tool_config.clone(),
                crate::capability::CapabilityMode::All,
                &mcp_snapshot,
                &hub_snapshot,
                self.shared.root_cwd.clone(),
                session_env,
                "__template__",
                self.shared.session_factory.as_ref(),
                Some(self.shared.local_registry.clone()),
                self.shared.lsp.clone(),
                None,
                None,
            )?;
            let mut handlers = build_session_routed_handlers(&template_toolset, self);
            let tool_names: Vec<String> = handlers
                .iter()
                .map(|h| h.tool_id().as_str().to_owned())
                .collect();
            let rpc_handler: Arc<dyn xai_computer_hub_sdk::ToolServerHandler> =
                Arc::new(crate::hub_server::WorkspaceRpcHandler::new(self.clone()));
            let rpc_tool_id = rpc_handler.tool_id();
            handlers.push(rpc_handler);
            tracing::info!(
                tool_count = handlers.len(),
                tools = ?tool_names,
                "Registering server tool catalog on hub"
            );
            (handlers, rpc_tool_id)
        };
        let catalog: Arc<Vec<Arc<dyn xai_computer_hub_sdk::ToolServerHandler>>> =
            Arc::new(template_handlers.clone());
        let resolver = self.session_bind_resolver(catalog, rpc_tool_id);
        let mut handle = hub_result(
            HubHandle::connect(
                &hub_config,
                self.shared.status_config.ws_ping,
                self.shared.status_config.ws_reconnect_backoff.clone(),
                template_handlers,
                self.shared.server_metadata.clone(),
                Some(resolver),
            )
            .await,
        )?;
        tracing::info!("WorkspaceHandle::connect_hub — connected, starting server + listeners");
        let (activity_notify_handle, activity_notify_rx) =
            xai_grok_tools::notification::types::ToolNotificationHandle::channel();
        let activity_feed_task = tokio::spawn(run_activity_feed(
            self.shared.activity_tracker.clone(),
            activity_notify_rx,
        ));
        handle.set_activity_feed_task(activity_feed_task);
        self.shared
            .activity_notify_handle
            .store(Arc::new(Some(activity_notify_handle)));
        let server = handle.server.clone();
        let server_task = tokio::spawn(async move {
            if let Err(e) = server.run().await {
                tracing::warn!(error = %e, "hub tool server run loop exited with error");
            }
        });
        handle.set_server_task(server_task);
        let mut notification_rx = handle.server.subscribe_notifications();
        let shared = self.shared.clone();
        let listener_task = tokio::spawn(async move {
            while let Some(notification) = notification_rx.recv().await {
                match notification {
                    xai_computer_hub_sdk::HubNotification::ToolsChanged {
                        added,
                        removed,
                        updated,
                        ..
                    } => {
                        let current = shared.hub_tools_snapshot.load_full();
                        let new_tools = apply_tools_changed(&current, &added, &removed, &updated);
                        shared.hub_tools_snapshot.store(Arc::new(new_tools));
                        shared
                            .re_resolve_all_sessions("hub_notification", true)
                            .await;
                    }
                    other => {
                        tracing::debug!(?other, "hub notification (unhandled type)");
                    }
                }
            }
            tracing::debug!("hub notification listener exited");
        });
        handle.set_notification_task(listener_task);
        let hub_warn_threshold = self.shared.status_config.hub_warn_threshold;
        let hub_backoff_base = self.shared.status_config.hub_backoff_base;
        /// Compute exponential backoff: `base` * 2^min(n, 7).
        fn hub_backoff(base: std::time::Duration, consecutive_errors: u32) -> std::time::Duration {
            base.saturating_mul(2u32.pow(consecutive_errors.min(7)))
        }
        let events_rx = self.shared.events.subscribe();
        let server_for_events = handle.server.clone();
        let event_publisher_task = tokio::spawn(async move {
            let mut rx = events_rx;
            let mut consecutive_errors: u32 = 0;
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let payload =
                            serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                        let frame = xai_tool_protocol::ToolNotificationFrame::custom(
                            xai_tool_protocol::ToolId::new(
                                crate::hub_ids::WORKSPACE_EVENTS_TOOL_ID,
                            )
                            .expect("constant tool id"),
                            "workspace_event",
                            payload,
                        );
                        if let Err(e) = server_for_events.send_notification(frame).await {
                            consecutive_errors += 1;
                            if consecutive_errors <= hub_warn_threshold {
                                tracing::warn!(error = %e, "failed to send workspace event to hub");
                            } else {
                                tracing::debug!(error = %e, consecutive = consecutive_errors, "workspace event send failed (backoff)");
                            }
                            tokio::time::sleep(hub_backoff(hub_backoff_base, consecutive_errors))
                                .await;
                        } else {
                            consecutive_errors = 0;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "workspace event publisher lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            tracing::debug!("workspace event publisher exited");
        });
        handle.set_event_publisher_task(event_publisher_task);
        let tracker_for_status = self.shared.activity_tracker.clone();
        let server_conn = handle.server.connection().clone();
        let heartbeat = self.shared.status_config.heartbeat;
        let keepalive = self.shared.status_config.keepalive;
        let status_publisher_task = tokio::spawn(async move {
            /// Attempt to send a status frame.
            ///
            /// Returns `Some(true)` on success, `Some(false)` on transport
            /// failure (hub unreachable), and `None` when the send was
            /// skipped due to a local error (serialization, id allocation)
            /// that does not indicate a dead connection.
            async fn send_status(
                conn: &xai_computer_hub_sdk::HubConnection,
                payload: ToolServerStatusPayload,
            ) -> Option<bool> {
                let params = match serde_json::to_value(&payload) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to serialize tool server status");
                        return None;
                    }
                };
                let request_id = match conn.try_alloc_request_id() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to alloc request id for status");
                        return None;
                    }
                };
                let req = xai_tool_protocol::JsonRpcRequest {
                    jsonrpc: xai_tool_protocol::JsonRpcVersion,
                    id: xai_tool_protocol::JsonRpcId::from_request_id(&request_id),
                    session_id: None,
                    method: xai_tool_protocol::Method::ToolServerStatus
                        .as_wire_str()
                        .to_owned(),
                    params,
                };
                if let Err(e) = conn.call_request(request_id, &req).await {
                    tracing::debug!(error = %e, "tool_server.status send failed");
                    return Some(false);
                }
                Some(true)
            }
            fn dedup_key(p: &ToolServerStatusPayload) -> ToolServerStatusPayload {
                let mut k = p.clone();
                k.uptime_ms = 0;
                k
            }
            let mut last_sent: std::collections::HashMap<Option<String>, ToolServerStatusPayload> =
                std::collections::HashMap::new();
            let mut consecutive_errors: u32 = 0;
            let mut last_successful_send = std::time::Instant::now();
            {
                let payload = tracker_for_status.snapshot();
                if send_status(&server_conn, payload.clone()).await == Some(true) {
                    last_sent.insert(None, payload);
                    last_successful_send = std::time::Instant::now();
                }
            }
            const MIN_REPUBLISH_INTERVAL: std::time::Duration =
                std::time::Duration::from_millis(250);
            let mut last_cycle = tokio::time::Instant::now() - MIN_REPUBLISH_INTERVAL;
            loop {
                tracker_for_status.wait_for_change(heartbeat).await;
                let since_last = last_cycle.elapsed();
                if since_last < MIN_REPUBLISH_INTERVAL {
                    tokio::time::sleep(MIN_REPUBLISH_INTERVAL - since_last).await;
                }
                last_cycle = tokio::time::Instant::now();
                let mut any_attempt = false;
                let mut any_success = false;
                let session_ids = tracker_for_status.known_sessions();
                for sid in &session_ids {
                    let payload = tracker_for_status.snapshot_session(sid);
                    let key = Some(sid.clone());
                    if last_sent.get(&key).map(dedup_key) == Some(dedup_key(&payload)) {
                        continue;
                    }
                    if let Some(ok) = send_status(&server_conn, payload.clone()).await {
                        any_attempt = true;
                        if ok {
                            any_success = true;
                            last_sent.insert(key, payload);
                            last_successful_send = std::time::Instant::now();
                        }
                    }
                }
                last_sent.retain(|k, _| match k {
                    None => true,
                    Some(sid) => session_ids.iter().any(|s| s == sid),
                });
                let payload = tracker_for_status.snapshot();
                let needs_send = last_sent.get(&None).map(dedup_key) != Some(dedup_key(&payload));
                let force_keepalive =
                    !needs_send && !any_success && last_successful_send.elapsed() >= keepalive;
                if (needs_send || force_keepalive)
                    && let Some(ok) = send_status(&server_conn, payload.clone()).await
                {
                    any_attempt = true;
                    if ok {
                        any_success = true;
                        last_sent.insert(None, payload);
                        last_successful_send = std::time::Instant::now();
                    }
                }
                if any_attempt && !any_success {
                    consecutive_errors += 1;
                    if consecutive_errors <= hub_warn_threshold {
                        tracing::warn!(
                            "status publisher: hub unreachable ({} consecutive failed cycles)",
                            consecutive_errors,
                        );
                    } else {
                        tracing::debug!(
                            consecutive = consecutive_errors,
                            "status publish failed (backoff)"
                        );
                    }
                    tokio::time::sleep(hub_backoff(hub_backoff_base, consecutive_errors)).await;
                } else if any_success {
                    consecutive_errors = 0;
                }
            }
        });
        handle.set_status_publisher_task(status_publisher_task);
        {
            let (ext_tx, mut ext_rx) =
                tokio::sync::mpsc::unbounded_channel::<(String, serde_json::Value)>();
            let server_for_ext = handle.server.clone();
            let ext_task = tokio::spawn(async move {
                while let Some((method, params)) = ext_rx.recv().await {
                    let frame = xai_tool_protocol::ToolNotificationFrame::custom(
                        xai_tool_protocol::ToolId::new(
                            crate::hub_ids::WORKSPACE_CLIENT_EXT_NOTIFICATIONS_TOOL_ID,
                        )
                        .expect("constant tool id"),
                        "client_ext_notification",
                        serde_json::json!({ "method": method, "params": params }),
                    );
                    let _ = server_for_ext.send_notification(frame).await;
                }
            });
            handle.set_client_ext_forwarder_task(ext_task);
            self.set_client_ext_sink(Arc::new(move |method, params| {
                let _ = ext_tx.send((method, params));
            }));
        }
        handle.set_codebase_index_forwarder_task(self.spawn_codebase_index_event_forwarder());
        if let Some(task) = self.spawn_tool_definitions_event_forwarder() {
            handle.set_tool_defs_forwarder_task(task);
        }
        *hub_guard = Some(handle);
        Ok(())
    }
    /// Shutdown the server connection, if active.
    pub async fn shutdown_hub(&self) {
        let handle = self.shared.hub_handle.lock().await.take();
        if let Some(h) = handle {
            h.shutdown().await;
        }
    }
}
/// Build one [`SessionRoutedToolHandler`](crate::hub::SessionRoutedToolHandler)
/// per tool in `toolset`, keyed by client (function) name. Shared by the
/// connect-time catalog and the per-`session.bind` resolver so the two
/// construction paths cannot drift.
///
/// `finalize` already rejects duplicate client names, so the `seen` set is
/// defense-in-depth: it guards a regression from ever emitting two handlers
/// with the same `tool_id` (which would duplicate the bind response and
/// silently first-win at dispatch).
fn build_session_routed_handlers(
    toolset: &xai_grok_tools::registry::types::FinalizedToolset,
    ws: &WorkspaceHandle,
) -> Vec<Arc<dyn xai_computer_hub_sdk::ToolServerHandler>> {
    let tool_kinds = toolset.tool_kinds();
    let mut seen = std::collections::HashSet::new();
    let mut handlers = Vec::new();
    for def in toolset.tool_definitions() {
        if !seen.insert(def.function.name.clone()) {
            tracing::warn!(
                tool = %def.function.name,
                "duplicate client name in finalized toolset; skipping"
            );
            continue;
        }
        let mut desc = xai_tool_types::ToolDescription::new(
            def.function.name.clone(),
            def.function.description.clone().unwrap_or_default(),
        );
        desc.arguments_schema = Some(def.function.parameters.clone());
        desc.kind = tool_kinds.get(&def.function.name).cloned();
        match crate::hub::SessionRoutedToolHandler::new(
            def.function.name.clone(),
            desc,
            Some(def.function.parameters.clone()),
            ws.clone(),
        ) {
            Ok(handler) => {
                handlers.push(Arc::new(handler) as Arc<dyn xai_computer_hub_sdk::ToolServerHandler>)
            }
            Err(e) => {
                tracing::warn!(
                    tool = %def.function.name,
                    error = %e,
                    "client name is not a valid ToolId; skipping hub registration"
                );
            }
        }
    }
    handlers
}
/// Apply a tool notification to the ActivityTracker background-task count.
/// `started` must precede `completed`, else the unknown `completed` no-ops and
/// strands the count.
pub(crate) fn apply_background_task_notification(
    tracker: &crate::activity::ActivityTracker,
    notification: &xai_grok_tools::notification::types::ToolNotification,
) {
    use xai_grok_tools::notification::types::ToolNotification;
    match notification {
        ToolNotification::BashExecutionBackgrounded(bg) => {
            tracker.background_task_started(&bg.task_id);
        }
        ToolNotification::TaskCompleted(snap) => {
            tracker.background_task_completed(&snap.task_id);
        }
        _ => {}
    }
}
/// Tracker-only drain of the session tool-notification stream — not a network
/// send, so the hibernation decrement isn't delayed by send backoff and
/// notifications aren't misattributed across sessions.
pub(crate) async fn run_activity_feed(
    tracker: Arc<crate::activity::ActivityTracker>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<
        xai_grok_tools::notification::types::ToolNotification,
    >,
) {
    while let Some(notification) = rx.recv().await {
        apply_background_task_notification(&tracker, &notification);
    }
}
/// Compute SHA-256 hex digest.
fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(data))
}
/// What triggered a [`WorkspaceHandle::two_phase_drain`] — the metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainReason {
    /// Process received SIGTERM / Ctrl-C (standalone `workspace_server`).
    Sigterm,
    /// Hub sent `tool_server.evict`.
    Evict,
}
impl DrainReason {
    /// Stable `reason` label for `grok_workspace_drain_started_total`.
    pub fn as_str(self) -> &'static str {
        match self {
            DrainReason::Sigterm => "sigterm",
            DrainReason::Evict => "evict",
        }
    }
}
/// Terminal classification of a two-phase drain — the metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Tools, producers, and the upload queue all finished within budget.
    Full,
    /// Tool calls still in flight at the phase-1 deadline.
    Partial,
    /// Producers still in flight at the phase-1.5 deadline (artifacts never queued).
    ProducersTimeout,
    /// Upload-queue deadline exceeded with items still pending (lost on exit).
    Timeout,
}
impl DrainOutcome {
    /// Stable `outcome` label for `grok_workspace_drain_completed_total`.
    pub fn as_str(self) -> &'static str {
        match self {
            DrainOutcome::Full => "full",
            DrainOutcome::Partial => "partial",
            DrainOutcome::ProducersTimeout => "producers_timeout",
            DrainOutcome::Timeout => "timeout",
        }
    }
}
/// Phase-1 (in-flight tool call) budget: one third of the total grace budget.
/// Phases 1.5 and 2 split the remainder.
fn phase1_budget(grace_budget: std::time::Duration) -> std::time::Duration {
    grace_budget / 3
}
/// Phase-1.5 (artifact producer) budget: half the post-phase-1 remainder, so a
/// wedged producer can't starve the phase-2 flush of already-enqueued items.
fn phase15_budget(remaining: std::time::Duration) -> std::time::Duration {
    remaining / 2
}
/// Poll the producer tracker until it reports zero in-flight tasks or `budget`
/// elapses; `true` = idle reached. Replaces `close()` + `wait()` so the
/// tracker stays open (reusable after a non-terminal drain).
async fn wait_for_producers_idle(
    tracker: &tokio_util::task::TaskTracker,
    budget: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while !tracker.is_empty() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    true
}
/// Classify a drain by the earliest phase that blew its deadline:
/// tools (`Partial`) > producers (`ProducersTimeout`) > queue (`Timeout`) >
/// clean (`Full`). `producers_unfinished` is the final producer count after
/// phase 2 (a producer can be spawned *during* phase 2, after `producers_done`
/// was latched in phase 1.5); it is checked so `Full` and the drain marker
/// agree — `Full` requires that no producer work remains, matching the marker /
/// return total (active tool calls + producers + queue), which is `0` only when
/// `tools_idle`, no producers remain, and the queue is empty.
fn classify_drain_outcome(
    tools_idle: bool,
    producers_done: bool,
    producers_unfinished: usize,
    unfinished: usize,
) -> DrainOutcome {
    if !tools_idle {
        DrainOutcome::Partial
    } else if !producers_done || producers_unfinished > 0 {
        DrainOutcome::ProducersTimeout
    } else if unfinished > 0 {
        DrainOutcome::Timeout
    } else {
        DrainOutcome::Full
    }
}
/// The SIGTERM drain budget from `GROK_WORKSPACE_TERMINATION_GRACE_MS`
/// (default [`DEFAULT_TERMINATION_GRACE_MS`]). The hub-evict path uses the
/// hub-provided `grace_period_ms` instead.
pub fn termination_grace_from_env() -> std::time::Duration {
    grace_budget_from_raw(std::env::var("GROK_WORKSPACE_TERMINATION_GRACE_MS").ok())
}
/// Pure parse of the termination-grace env value: a positive integer ms wins,
/// anything else (absent, unparseable, zero) falls back to the default.
fn grace_budget_from_raw(raw: Option<String>) -> std::time::Duration {
    let ms = raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(DEFAULT_TERMINATION_GRACE_MS);
    std::time::Duration::from_millis(ms)
}
/// Path of the preStop drain marker (`GROK_WORKSPACE_DRAINING_FILE` or
/// [`DEFAULT_DRAINING_FILE`]).
fn draining_file_path() -> std::path::PathBuf {
    std::env::var("GROK_WORKSPACE_DRAINING_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(DEFAULT_DRAINING_FILE))
}
/// Atomically write `outstanding` (total durability work still pending: upload
/// queue depth + in-flight artifact producers) to the drain marker (temp +
/// fsync + rename) so the preStop hook never reads a torn value and never sees
/// `0` while a producer could still enqueue. Best-effort. The temp name is
/// unique (pid + counter) so concurrent evict drains don't race on a fixed
/// `.tmp`.
fn write_draining_marker(path: &std::path::Path, outstanding: usize) {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".{}.{}.draining.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = std::path::PathBuf::from(tmp);
    let result = std::fs::File::create(&tmp)
        .and_then(|mut f| {
            f.write_all(outstanding.to_string().as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        tracing::warn!(path = %path.display(), error = %e, "failed to write drain marker");
        let _ = std::fs::remove_file(&tmp);
    }
}
/// Stream a file once: SHA-256 over every byte while capturing the
/// `[offset, offset + length)` overlap. Returns
/// `(hash_hex, range_bytes, total_streamed_bytes)`.
///
/// Shared by [`WorkspaceHandle::get_files`]' chunked reads and the
/// `file_system::client_fs` ops so the overlap arithmetic lives in one
/// place.
pub(crate) async fn stream_hash_and_range(
    path: &std::path::Path,
    offset: u64,
    length: u64,
) -> std::io::Result<(String, Vec<u8>, u64)> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let req_end = offset.saturating_add(length);
    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut chunk = Vec::new();
    let mut pos: u64 = 0;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        let start = pos.max(offset);
        let end = (pos + n as u64).min(req_end);
        if start < end {
            let local_start = (start - pos) as usize;
            let local_end = (end - pos) as usize;
            chunk.extend_from_slice(&buf[local_start..local_end]);
        }
        pos += n as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), chunk, pos))
}
/// Create a [`WorkspaceHandle`] and connect it to the hub.
///
/// This is the shared setup used by both the standalone `workspace_server`
/// binary and the TUI's in-process local workspace server. The workspace
/// registers its tools on the server so external clients can reach them.
/// Sessions are bound dynamically by clients calling `bind_server`.
///
/// `confine_fs_to_workspace_root` confines `x.ai/fs/*` resolution to the root.
/// The standalone workspace server defaults it on (it always backs a remote
/// sandbox; override via `GROK_WORKSPACE_CONFINE_FS_TO_ROOT`); the CLI leader
/// passes `false`.
///
/// Returns the connected handle (caller should keep it alive for the
/// lifetime of the server connection).
pub async fn connect_local_workspace(
    cwd: std::path::PathBuf,
    hub_url: url::Url,
    auth: xai_computer_hub_sdk::SharedAuthProvider,
    metadata: Option<serde_json::Value>,
    server_id: Option<String>,
    alpha_test_key: Option<String>,
    allow_insecure_ws: bool,
    status_config: crate::status_config::StatusConfig,
    upload_queue_enabled: bool,
    project_lsp_trusted: bool,
    diag: Option<DiagHandle>,
    require_explicit_toolset: bool,
    confine_fs_to_workspace_root: bool,
) -> WorkspaceResult<WorkspaceHandle> {
    use crate::session::tool_config::WorkspaceSessionContextFactory;
    let identity: crate::upload::environment::WorkspaceIdentity =
        auth.identity().map(Into::into).unwrap_or_default();
    let workspace_home = resolve_workspace_home();
    std::fs::create_dir_all(&workspace_home).map_err(|e| {
        WorkspaceError::HubError(format!(
            "failed to create workspace home {}: {e}",
            workspace_home.display()
        ))
    })?;
    let api_base_url = std::env::var("GROK_CLI_CHAT_PROXY_BASE_URL")
        .unwrap_or_else(|_| "https://cli-chat-proxy.grok.com/v1".to_string());
    let data_collection_disabled =
        std::env::var("GROK_WORKSPACE_DATA_COLLECTION_DISABLED").as_deref() != Ok("false");
    let mut factory = WorkspaceSessionContextFactory::with_auth(auth.clone(), api_base_url.clone());
    if crate::session::tool_config::tool_state_enabled() {
        factory = factory.with_tool_state_home(workspace_home.clone());
    }
    let hub_cfg = crate::hub::HubConfig {
        url: hub_url,
        auth: auth.clone(),
        activity_tracker: None,
        server_id,
        alpha_test_key,
        allow_insecure_ws,
        diag,
    };
    let tool_config = xai_grok_agent::workspace_grok_build_toolset();
    let mut ws_config = WorkspaceConfig::new_for_proxy(
        cwd,
        Arc::new(factory),
        hub_cfg,
        auth.clone(),
        metadata,
        status_config,
        tool_config,
    );
    ws_config.project_lsp_trusted = project_lsp_trusted;
    ws_config.require_explicit_toolset = require_explicit_toolset;
    ws_config.confine_fs_to_workspace_root = confine_fs_to_workspace_root;
    if let Ok(dir) = std::env::var("GROK_WORKSPACE_SERVER_SKILLS_DIR")
        && !dir.is_empty()
    {
        ws_config.skills_config.server_skill_dirs = vec![dir];
    }
    if let Ok(dir) = std::env::var("GROK_WORKSPACE_BUNDLED_SKILLS_DIR")
        && !dir.is_empty()
    {
        let allowlist = std::env::var("GROK_WORKSPACE_BUNDLED_SKILLS_ALLOWLIST").ok();
        ws_config
            .skills_config
            .ignore
            .extend(bundled_allowlist_ignore_dirs(&dir, allowlist.as_deref()));
        ws_config.skills_config.bundled_skill_dirs = vec![dir];
    }
    let proxy_storage = Arc::new(crate::upload::ProxyStorageConfig::new(
        auth.clone(),
        api_base_url.clone(),
        identity.clone(),
    ));
    let trace_source: Arc<dyn xai_file_utils::queue::TraceExportSource> = Arc::new(
        crate::upload::WorkspaceTraceExportSource::new(proxy_storage.clone()),
    );
    let upload_queue = Arc::new(xai_file_utils::queue::UploadQueue::spawn(
        &workspace_home,
        trace_source,
        xai_file_utils::queue::UploadRetryPolicy::default(),
    ));
    if data_collection_disabled {
        crate::recovery::purge_spilled_items(&workspace_home);
    } else {
        let report = crate::recovery::run_startup_recovery(&workspace_home, &upload_queue).await;
        tracing::info!(?report, "workspace startup restart-recovery scan complete");
    }
    upload_queue.cleanup_orphans(xai_file_utils::queue::DEFAULT_MAX_AGE);
    crate::upload::spawn_queue_stats_sampler(
        upload_queue.clone(),
        std::time::Duration::from_secs(15),
    );
    if crate::session::tool_config::tool_state_enabled() {
        let home = workspace_home.clone();
        tokio::spawn(async move {
            crate::recovery::cleanup_stale_sessions(
                &home,
                crate::recovery::DEFAULT_SESSION_MAX_AGE,
            )
            .await;
        });
    }
    tokio::task::spawn_blocking(|| {
        crate::worktree::run_auto_gc_best_effort();
    });
    let ws_handle = WorkspaceHandle::new_with_data_collection(
        ws_config,
        workspace_home,
        upload_queue,
        upload_queue_enabled,
        data_collection_disabled,
        identity,
    )
    .map_err(|e| WorkspaceError::HubError(format!("failed to create workspace: {e}")))?;
    ws_handle.connect_hub().await?;
    Ok(ws_handle)
}
/// Resolve `$GROK_WORKSPACE_HOME` — the workspace-owned on-disk state root.
///
/// Precedence:
/// 1. `$GROK_WORKSPACE_HOME` (operator override).
/// 2. `<grok_home>/workspace`, where `<grok_home>` honours `$GROK_HOME` and
///    otherwise falls back to `~/.grok` (see [`xai_grok_config::grok_home`]).
pub fn resolve_workspace_home() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GROK_WORKSPACE_HOME")
        && !p.trim().is_empty()
    {
        return std::path::PathBuf::from(p);
    }
    xai_grok_config::grok_home().join("workspace")
}
/// Skill `ignore` entries for the allow-list: subdirs of `dir` not in the
/// comma-separated list (`bundled__` prefix optional). Blank list → none;
/// unreadable `dir` → ignore `dir` itself (fail closed).
fn bundled_allowlist_ignore_dirs(dir: &str, allowlist: Option<&str>) -> Vec<String> {
    let allowed: std::collections::HashSet<&str> = allowlist
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim())
        .map(|s| s.strip_prefix("bundled__").unwrap_or(s))
        .filter(|s| !s.is_empty())
        .collect();
    if allowed.is_empty() {
        return vec![];
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(
                dir,
                %err,
                "bundled skills dir unreadable; allow-list ignores the whole dir"
            );
            return vec![dir.to_string()];
        }
    };
    let mut dirs: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let stripped = name.strip_prefix("bundled__").unwrap_or(&name);
            !allowed.contains(stripped)
        })
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    dirs
}
/// Whether per-session `events.jsonl` recording is enabled
/// (`GROK_WORKSPACE_EVENTS_ENABLED=true`). Any other value — including unset —
/// keeps the legacy behaviour: [`WorkspaceShared::session_event_writer`] hands
/// back [`EventWriter::noop()`](xai_file_utils::events::EventWriter::noop)
/// and no `events.jsonl` is ever opened.
fn events_enabled() -> bool {
    std::env::var("GROK_WORKSPACE_EVENTS_ENABLED").as_deref() == Ok("true")
}
/// Watchdog for awaiting enqueue outcomes when answering an `After` turn
/// hook. MUST undercut the requester's 10s hook deadline or the reply (and
/// its ack) arrives after the requester gave up. Default 8s; override via
/// `GROK_WORKSPACE_AFTER_TURN_WATCHDOG_MS` (malformed values fall back).
fn after_turn_watchdog() -> std::time::Duration {
    const DEFAULT_MS: u64 = 8_000;
    let ms = std::env::var("GROK_WORKSPACE_AFTER_TURN_WATCHDOG_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MS);
    std::time::Duration::from_millis(ms)
}
/// Whether per-session `workspace_tool_definitions.json` emission is enabled
/// (`GROK_WORKSPACE_TOOL_DEFS_ENABLED=true`; any other value keeps legacy
/// behaviour).
fn tool_defs_enabled() -> bool {
    std::env::var("GROK_WORKSPACE_TOOL_DEFS_ENABLED").as_deref() == Ok("true")
}
/// Debounce window for `ToolsChanged`-driven re-emission: at most one re-emit
/// per session per window.
pub(crate) const TOOL_DEFS_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(5);
/// Session-root GCS object path for a session's workspace-side tool
/// definitions (same cadence convention as `workspace_environment.json`).
fn workspace_tool_definitions_path(session_id: &str) -> String {
    format!("{session_id}/workspace_tool_definitions.json")
}
/// Whether `s` is safe to interpolate as the leading segment of a GCS object
/// key: non-empty, no separators, `..`, or NUL (RPC ids are a trust boundary).
fn is_safe_object_segment(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains('\\') && !s.contains("..") && !s.contains('\0')
}
/// Per-session re-emit gate: `true` (recording `now` as last-emit) only when
/// `enabled` and at least `window` elapsed since the previous re-emit. Disabled
/// records no state, so flipping the flag on later is never pre-empted by
/// suppressed-while-off events; the check-and-set is atomic via the dashmap
/// entry API so concurrent events for one session cannot both pass.
fn tool_defs_reemit_gate(
    enabled: bool,
    last_emit: &dashmap::DashMap<String, std::time::Instant>,
    session_id: &str,
    now: std::time::Instant,
    window: std::time::Duration,
) -> bool {
    if !enabled {
        return false;
    }
    if let Some(prev) = last_emit.get(session_id)
        && now.saturating_duration_since(*prev) < window
    {
        return false;
    }
    use dashmap::mapref::entry::Entry;
    match last_emit.entry(session_id.to_owned()) {
        Entry::Occupied(mut e) => {
            if now.saturating_duration_since(*e.get()) >= window {
                e.insert(now);
                true
            } else {
                false
            }
        }
        Entry::Vacant(e) => {
            e.insert(now);
            true
        }
    }
}
/// Enqueue serialized workspace tool definitions at `object_path`, mapping the
/// outcome to a log line. Shared by `emit_workspace_tool_definitions` (which
/// spawns it) and the unit tests (which await it).
async fn enqueue_workspace_tool_definitions(
    upload_queue: &xai_file_utils::queue::UploadQueue,
    session_id: &str,
    object_path: &str,
    bytes: &[u8],
) -> xai_file_utils::queue::EnqueueOutcome {
    use xai_file_utils::queue::EnqueueOutcome;
    let outcome = upload_queue
        .enqueue_bytes_blocking(
            bytes,
            object_path,
            "application/json",
            "workspace_tool_definitions",
            session_id,
            0,
        )
        .await;
    match &outcome {
        EnqueueOutcome::Enqueued
        | EnqueueOutcome::FellBackToInline
        | EnqueueOutcome::Deduplicated => {
            tracing::info!(
                %session_id,
                object_path = %object_path,
                bytes = bytes.len(),
                outcome = ?outcome,
                "workspace: tool definitions enqueued"
            );
        }
        EnqueueOutcome::Failed { reason } => {
            tracing::warn!(
                %session_id,
                object_path = %object_path,
                error = %reason,
                "workspace: tool definitions enqueue failed"
            );
        }
    }
    outcome
}
/// Single source of truth for mapping a turn-hook outcome to the `events.jsonl`
/// [`TurnOutcomeLabel`]. Kept as one `match` so the two enums cannot drift and
/// the mapping is never duplicated across call sites.
fn turn_outcome_label(outcome: xai_tool_protocol::turn_hook::TurnHookOutcome) -> TurnOutcomeLabel {
    use xai_tool_protocol::turn_hook::TurnHookOutcome;
    match outcome {
        TurnHookOutcome::Completed => TurnOutcomeLabel::Completed,
        TurnHookOutcome::Cancelled => TurnOutcomeLabel::Cancelled,
        TurnHookOutcome::Error => TurnOutcomeLabel::Error,
        _ => TurnOutcomeLabel::Error,
    }
}
/// Decode the wire `session_relationship` string into the `events.jsonl`
/// enum. Unknown values map to the safe default `Primary`; the snake_case
/// forms are pinned by `session_relationship_wire_forms_round_trip`.
fn decode_session_relationship(s: &str) -> SessionRelationship {
    match s {
        "subagent" => SessionRelationship::Subagent,
        _ => SessionRelationship::Primary,
    }
}
/// Decode the bare snake_case `cancellation_category` string into the
/// `events.jsonl` enum; unrecognised values decode to `None` rather than
/// failing the whole `TurnEnded` emission.
fn decode_cancellation_category(s: Option<&str>) -> Option<CancellationCategory> {
    s.and_then(|s| {
        serde_json::from_value::<CancellationCategory>(serde_json::Value::String(s.to_owned())).ok()
    })
}
/// Await both per-phase enqueue handles and reduce them to the wire ack triple
/// `(status, artifact_count, error_message)`. No handles at all means nothing
/// is on disk → `Skipped` with `no_handle_skip_reason` as the diagnostic.
async fn resolve_after_turn_ack(
    before_handle: Option<tokio::task::JoinHandle<EnqueueOutcome>>,
    after_handle: Option<tokio::task::JoinHandle<EnqueueOutcome>>,
    watchdog: std::time::Duration,
    no_handle_skip_reason: &str,
) -> (AfterTurnAckStatus, u32, Option<String>) {
    if before_handle.is_none() && after_handle.is_none() {
        return (
            AfterTurnAckStatus::Skipped,
            0,
            Some(no_handle_skip_reason.to_owned()),
        );
    }
    let (before, after) = tokio::join!(
        await_enqueue_outcome(before_handle, watchdog, "before_enqueue"),
        await_enqueue_outcome(after_handle, watchdog, "after_enqueue"),
    );
    reduce_enqueue_outcomes(&before, &after)
}
/// Await one enqueue handle under a watchdog, mapping every failure mode
/// (missing handle, join error, timeout) to [`EnqueueOutcome::Failed`]. On
/// timeout the task is detached, not aborted — we only stop blocking the ack.
async fn await_enqueue_outcome(
    handle: Option<tokio::task::JoinHandle<EnqueueOutcome>>,
    watchdog: std::time::Duration,
    phase: &str,
) -> EnqueueOutcome {
    let Some(handle) = handle else {
        return EnqueueOutcome::Failed {
            reason: format!("no inflight enqueue for {phase}"),
        };
    };
    match tokio::time::timeout(watchdog, handle).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(join_err)) => EnqueueOutcome::Failed {
            reason: format!("{phase} enqueue task failed to join: {join_err}"),
        },
        Err(_elapsed) => EnqueueOutcome::Failed {
            reason: "watchdog_timeout".to_owned(),
        },
    }
}
/// Reduce the two per-phase [`EnqueueOutcome`]s to the wire ack triple.
/// `artifact_count` counts only durably-spilled phases (`FellBackToInline` is
/// a success for `status` but not durable, so it does not count); any `Failed`
/// wins the `status`, carrying the first failure reason. `Skipped` is never
/// produced here — the no-queue case is handled by [`resolve_after_turn_ack`].
fn reduce_enqueue_outcomes(
    before: &EnqueueOutcome,
    after: &EnqueueOutcome,
) -> (AfterTurnAckStatus, u32, Option<String>) {
    let durable = |o: &EnqueueOutcome| matches!(o, EnqueueOutcome::Enqueued);
    let artifact_count = durable(before) as u32 + durable(after) as u32;
    let first_failure = [before, after].into_iter().find_map(|o| match o {
        EnqueueOutcome::Failed { reason } => Some(reason.clone()),
        _ => None,
    });
    match first_failure {
        Some(reason) => (AfterTurnAckStatus::Failed, artifact_count, Some(reason)),
        None => (AfterTurnAckStatus::Enqueued, artifact_count, None),
    }
}
/// Per-process ephemeral workspace home for handles constructed without a
/// backing upload queue (tests, local mode). Never the real grok home —
/// only [`connect_local_workspace`] resolves `$GROK_WORKSPACE_HOME` — so the
/// queue-less default path can never collide with a real workspace's state dir.
fn ephemeral_workspace_home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("grok-workspace-ephemeral-{}", std::process::id()))
}
/// Resolve `workspace_rewind_all_outcomes` from `GROK_WORKSPACE_REWIND_ALL_OUTCOMES` (default off).
fn rewind_all_outcomes_from_env() -> bool {
    xai_grok_config::env_bool("GROK_WORKSPACE_REWIND_ALL_OUTCOMES").unwrap_or(false)
}
/// Flush the session toolset's `ResourcesPersistence` to disk (a fresh
/// snapshot, waiting for the atomic-rename write to land), then read the bytes
/// back and enqueue them for the given turn. Extracted from
/// `spawn_tool_state_upload` so the path is unit-testable without a live turn.
async fn persist_and_enqueue_tool_state(
    session: Arc<crate::session::WorkspaceSession>,
    session_id: String,
    turn_number: u64,
    upload_queue: Arc<xai_file_utils::queue::UploadQueue>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let toolset = session.toolset();
    let state_path = toolset.save_and_flush_persistence().await.to_path_buf();
    let bytes = tokio::fs::read(&state_path).await.map_err(|e| {
        format!(
            "failed to read flushed tool_state from {}: {e}",
            state_path.display()
        )
    })?;
    crate::upload::upload_tool_state_queued(bytes, session_id, turn_number, upload_queue).await
}
/// `ToolHandle` adapter that delegates to a workspace session's
/// [`FinalizedToolset`]. Used by [`WorkspaceHandle::create_local_harness`]
/// to populate a [`LocalRegistry`] for in-process tool dispatch.
///
/// This is the same dispatch pattern as [`SessionRoutedToolHandler`] in
/// `hub.rs`, but implements `ToolHandle` (for `LocalRegistry`) instead
/// of `ToolServerHandler` (for `ToolServer`).
struct SessionToolHandle {
    tool_id: xai_tool_protocol::ToolId,
    desc: xai_tool_types::ToolDescription,
    workspace: WorkspaceHandle,
    session_id: String,
}
impl SessionToolHandle {
    fn new(
        tool_name: String,
        desc: xai_tool_types::ToolDescription,
        workspace: WorkspaceHandle,
        session_id: String,
    ) -> Result<Self, xai_tool_protocol::IdError> {
        Ok(Self {
            tool_id: xai_tool_protocol::ToolId::new(tool_name)?,
            desc,
            workspace,
            session_id,
        })
    }
    fn name(&self) -> &str {
        self.tool_id.as_str()
    }
}
impl std::fmt::Debug for SessionToolHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionToolHandle")
            .field("tool_name", &self.name())
            .field("session_id", &self.session_id)
            .finish()
    }
}
#[async_trait::async_trait]
impl xai_tool_runtime::ToolDyn for SessionToolHandle {
    fn id(&self) -> xai_tool_protocol::ToolId {
        self.tool_id.clone()
    }
    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        self.desc.clone()
    }
    async fn execute(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        args: serde_json::Value,
    ) -> xai_tool_runtime::ToolStream<xai_tool_runtime::TypedToolOutput> {
        use xai_tool_runtime::{ToolError, ToolErrorKind, ToolStreamItem, terminal_only};
        let session = match self.workspace.session(&self.session_id) {
            Some(s) => s,
            None => {
                return terminal_only(Err(ToolError::new(
                    ToolErrorKind::InvalidArguments,
                    format!("session not bound: {}", self.session_id),
                )));
            }
        };
        let toolset = session.toolset();
        let call_id = ctx.call_id.to_string();
        let tool_id = self.id();
        let tool_name = self.name().to_owned();
        let session_label = self.session_id.clone();
        tracing::debug!(
            tool = %self.name(),
            call_id = %call_id,
            session = %self.session_id,
            "local harness: dispatching tool call"
        );
        let inner = toolset.call_streaming(self.name(), args, &call_id, None);
        Box::pin(async_stream::stream! {
            use futures::StreamExt;
            let mut inner = inner;
            while let Some(item) = inner.next().await {
                match item {
                    // Rollout gate lives downstream in the sampler.
                    ToolStreamItem::Progress(p) => {
                        yield ToolStreamItem::Progress(p);
                    }
                    ToolStreamItem::Terminal(Ok(run_result)) => {
                        yield ToolStreamItem::Terminal(Ok(
                            run_result.into_typed_tool_output(tool_id),
                        ));
                        return;
                    }
                    ToolStreamItem::Terminal(Err(e)) => {
                        tracing::error!(
                            tool = %tool_name,
                            session = %session_label,
                            error = %e,
                            "local harness tool call failed"
                        );
                        yield ToolStreamItem::Terminal(Err(ToolError::new(
                            ToolErrorKind::TerminalError,
                            e.to_string(),
                        )));
                        return;
                    }
                }
            }
            // Defensive fallback: every terminal arm above `return`s, so this
            // is only reached if the inner `call_streaming` stream ended
            // without a terminal. That is unreachable under the
            // `call_streaming` contract (it yields exactly one terminal on
            // every code path), but emit a terminal here anyway so the
            // "exactly one Terminal" invariant is enforced locally rather
            // than merely inherited from the inner layer.
            yield ToolStreamItem::Terminal(Err(ToolError::new(
                ToolErrorKind::TerminalError,
                "tool stream ended without a terminal",
            )));
        })
    }
}
impl WorkspaceHandle {
    /// Create a local-only [`ToolHarness`] backed by this workspace's
    /// session toolset.
    ///
    /// Tools are dispatched in-process via a [`LocalRegistry`] — no hub
    /// connection needed. Each tool is resolved dynamically from the
    /// session's live [`FinalizedToolset`] at call time, so tool config
    /// hot-reloads (via `update_tool_config()`) take effect automatically.
    pub fn create_local_harness(
        &self,
        session_id: &str,
    ) -> WorkspaceResult<xai_computer_hub_sdk::ToolHarness> {
        let session = self
            .session(session_id)
            .ok_or_else(|| WorkspaceError::SessionNotFound(session_id.to_string()))?;
        let toolset = session.toolset();
        let registry = xai_computer_hub_sdk::LocalRegistry::new();
        for def in toolset.tool_definitions() {
            let tool_name = def.function.name.clone();
            let desc = xai_tool_types::ToolDescription::new(
                tool_name.clone(),
                def.function.description.clone().unwrap_or_default(),
            );
            match SessionToolHandle::new(tool_name, desc, self.clone(), session_id.to_string()) {
                Ok(tool) => {
                    registry.register_dyn(Arc::new(tool) as Arc<dyn xai_tool_runtime::ToolDyn>);
                }
                Err(e) => {
                    tracing::warn!(
                        tool = %def.function.name,
                        error = %e,
                        "client name is not a valid ToolId; skipping local-harness registration"
                    );
                }
            }
        }
        let session_id = xai_tool_protocol::SessionId::new(session_id.to_string())
            .map_err(|e| WorkspaceError::HubError(format!("invalid session id: {e}")))?;
        Ok(xai_computer_hub_sdk::ToolHarness::local_only_with(
            registry,
            session_id,
            xai_tool_runtime::TypedExtensions::default(),
        ))
    }
}
impl WorkspaceHandle {
    /// Minimal handle for local mode (no hub). Requires Tokio runtime.
    ///
    /// `identity` is stored for parity with the standalone path; this local
    /// path has no upload queue, so no environment artifact is emitted.
    pub fn new_minimal(
        cwd: std::path::PathBuf,
        identity: crate::upload::environment::WorkspaceIdentity,
        project_lsp_trusted: bool,
    ) -> WorkspaceResult<Self> {
        use crate::session::tool_config::WorkspaceSessionContextFactory;
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: xai_grok_tools::registry::types::ToolServerConfig {
                tools: vec![],
                behavior_preset: None,
            },
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: Arc::new(WorkspaceSessionContextFactory::new()),
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        Self::build(
            config,
            ephemeral_workspace_home(),
            None,
            true,
            false,
            events_enabled(),
            rewind_all_outcomes_from_env(),
            tool_defs_enabled(),
            identity,
        )
    }
}
#[cfg(any(test, feature = "test-support"))]
impl WorkspaceHandle {
    fn test_config(
        root_cwd: std::path::PathBuf,
        factory: std::sync::Arc<
            crate::session::tool_config::test_support::TestSessionContextFactory,
        >,
    ) -> crate::config::WorkspaceConfig {
        use crate::config::{DEFAULT_EVENT_BUFFER_CAPACITY, WorkspaceConfig};
        use crate::session::tool_config::test_support::baseline_config;
        WorkspaceConfig {
            root_cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        }
    }
    /// Test handle backed by a temp dir. Zero sessions; `TempDir` kept alive via `Arc`.
    pub fn for_test() -> Self {
        use crate::session::tool_config::test_support::TestSessionContextFactory;
        let factory = std::sync::Arc::new(TestSessionContextFactory::new());
        let root_cwd = factory.temp.path().to_path_buf();
        Self::new(Self::test_config(root_cwd, factory))
            .expect("test workspace handle construction must succeed")
    }
    /// Like [`Self::for_test`] but rooted at `root` (must exist on disk).
    pub fn for_test_in(root: &std::path::Path) -> Self {
        use crate::session::tool_config::test_support::TestSessionContextFactory;
        let factory = std::sync::Arc::new(TestSessionContextFactory::new());
        Self::new(Self::test_config(root.to_path_buf(), factory))
            .expect("test workspace handle construction must succeed")
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::capability::CapabilityMode;
    use crate::config::{AgentSessionConfig, DEFAULT_EVENT_BUFFER_CAPACITY, WorkspaceConfig};
    use crate::error::WorkspaceError;
    use crate::session::tool_config::resolve_session_toolset;
    use crate::session::tool_config::test_support::{
        TestSessionContextFactory, baseline_config, tc,
    };
    use std::sync::Arc;
    use xai_grok_tools::registry::types::ToolServerConfig;
    use xai_grok_tools::types::tool::ToolKind;
    use xai_grok_workspace_types::WorkspaceEvent;
    /// Create a test workspace handle with a "main" session pre-created.
    pub(crate) fn make_handle() -> WorkspaceHandle {
        make_handle_with_rewind_all_outcomes(false)
    }
    /// [`make_handle`] with `require_explicit_toolset` (strict sandbox mode).
    pub(crate) fn make_strict_handle() -> WorkspaceHandle {
        make_handle_with_options(false, true)
    }
    /// [`make_handle`] with fs confinement on (mirrors a remote-sandbox server).
    pub(crate) fn make_confining_handle() -> WorkspaceHandle {
        make_handle_inner(false, false, Default::default(), true)
    }
    /// [`make_handle`] with an explicit `workspace_rewind_all_outcomes` value.
    pub(crate) fn make_handle_with_rewind_all_outcomes(enabled: bool) -> WorkspaceHandle {
        make_handle_inner(enabled, false, Default::default(), false)
    }
    pub(crate) fn make_handle_with_options(
        rewind_all_outcomes: bool,
        require_explicit_toolset: bool,
    ) -> WorkspaceHandle {
        make_handle_inner(
            rewind_all_outcomes,
            require_explicit_toolset,
            Default::default(),
            false,
        )
    }
    fn make_handle_inner(
        rewind_all_outcomes: bool,
        require_explicit_toolset: bool,
        status_config: crate::StatusConfig,
        confine_fs_to_workspace_root: bool,
    ) -> WorkspaceHandle {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: Some(Arc::new(xai_computer_hub_sdk::AuthCredential::bearer(
                "test-token",
            ))),
            server_metadata: None,
            status_config,
            project_lsp_trusted: true,
            require_explicit_toolset,
            confine_fs_to_workspace_root,
        };
        let handle = WorkspaceHandle::build(
            config,
            ephemeral_workspace_home(),
            None,
            true,
            false,
            false,
            rewind_all_outcomes,
            false,
            crate::upload::environment::WorkspaceIdentity::default(),
        )
        .expect("handle construction should succeed");
        handle
            .create_session("main")
            .expect("create main session should succeed");
        handle
    }
    pub(crate) const BASH_CCO_STUB_NAME: &str = "bash_cco_stub";
    pub(crate) const BASH_CCO_STUB_STDOUT: &str = "cco-stdout";
    #[derive(Debug)]
    pub(crate) struct BashCcoStub;
    impl xai_grok_tools::types::tool_metadata::ToolMetadata for BashCcoStub {
        fn kind(&self) -> ToolKind {
            ToolKind::Execute
        }
        fn tool_namespace(&self) -> xai_grok_tools::types::tool::ToolNamespace {
            xai_grok_tools::types::tool::ToolNamespace::MCP
        }
        fn description_template(&self) -> &str {
            "bash cco stub"
        }
    }
    impl xai_tool_runtime::Tool for BashCcoStub {
        type Args = serde_json::Value;
        type Output = xai_grok_tools::types::output::ToolOutput;
        fn id(&self) -> xai_tool_protocol::ToolId {
            xai_tool_protocol::ToolId::new(BASH_CCO_STUB_NAME).expect("valid tool id")
        }
        fn description(
            &self,
            _ctx: &::xai_tool_runtime::ListToolsContext,
        ) -> xai_tool_types::ToolDescription {
            xai_tool_types::ToolDescription::new(BASH_CCO_STUB_NAME, "bash cco stub")
        }
        async fn run(
            &self,
            _ctx: xai_tool_runtime::ToolCallContext,
            _input: serde_json::Value,
        ) -> Result<xai_grok_tools::types::output::ToolOutput, xai_tool_runtime::ToolError>
        {
            let output = BASH_CCO_STUB_STDOUT.as_bytes();
            Ok(xai_grok_tools::types::output::ToolOutput::Bash(
                xai_grok_tools::types::output::BashOutput {
                    output: output.to_vec(),
                    output_for_prompt:
                        xai_grok_tools::types::output::BashOutput::make_output_for_prompt(
                            BASH_CCO_STUB_STDOUT,
                        ),
                    exit_code: 0,
                    command: format!("echo {BASH_CCO_STUB_STDOUT}"),
                    truncated: false,
                    signal: None,
                    timed_out: false,
                    description: None,
                    current_dir: "/tmp".into(),
                    output_file: String::new(),
                    total_bytes: output.len(),
                    output_delta: None,
                    was_bare_echo: false,
                },
            ))
        }
    }
    pub(crate) fn register_bash_cco_stub(handle: &WorkspaceHandle) {
        let session = handle.session("main").expect("main session present");
        session
            .toolset()
            .register_tool(
                BASH_CCO_STUB_NAME.to_owned(),
                BashCcoStub,
                Some(serde_json::json!({"type": "object", "properties": {}})),
            )
            .expect("register bash_cco_stub");
    }
    pub(crate) fn assert_bash_cco_terminal(typed: &xai_tool_runtime::TypedToolOutput) {
        use xai_tool_runtime::ToolOutput as _;
        let resp = typed
            .chat_completion_output()
            .expect("bash chat_completion_output must be preserved");
        let cer = resp
            .result
            .as_ref()
            .and_then(|r| r.code_execution_result.as_ref())
            .expect("code_execution_result");
        assert_eq!(cer.stdout, BASH_CCO_STUB_STDOUT);
        assert_eq!(cer.exit_code, 0);
        assert!(!cer.command_timed_out);
    }
    pub(crate) async fn drain_terminal_ok(
        mut stream: impl futures::Stream<
            Item = xai_tool_runtime::ToolStreamItem<xai_tool_runtime::TypedToolOutput>,
        > + Unpin,
    ) -> xai_tool_runtime::TypedToolOutput {
        use futures::StreamExt;
        use xai_tool_runtime::ToolStreamItem;
        while let Some(item) = stream.next().await {
            match item {
                ToolStreamItem::Terminal(Ok(t)) => return t,
                ToolStreamItem::Progress(_) => {}
                ToolStreamItem::Terminal(Err(e)) => {
                    panic!("expected Terminal(Ok), got Err: {e}")
                }
            }
        }
        panic!("stream ended without terminal")
    }
    #[tokio::test]
    async fn local_harness_preserves_bash_chat_completion_output() {
        use xai_tool_runtime::ToolCallContext;
        let handle = make_handle();
        register_bash_cco_stub(&handle);
        let harness = handle.create_local_harness("main").expect("local harness");
        let tool_id = xai_tool_protocol::ToolId::new(BASH_CCO_STUB_NAME).expect("valid tool id");
        let stream = harness
            .call(tool_id, serde_json::json!({}), ToolCallContext::default())
            .await;
        let typed = drain_terminal_ok(stream).await;
        assert_bash_cco_terminal(&typed);
    }
    /// No connection ⇒ every export entry point returns `None`, so the
    /// binary leaves the `DonatingLogLayer` inert and spawns no metric reporter.
    /// This is the flag-free "activate only on connection" contract that log
    /// and metric export share with the pre-existing `trace_donation_reporter`.
    #[tokio::test]
    async fn donation_entry_points_are_inert_without_a_hub() {
        let handle = make_handle();
        assert!(
            handle
                .trace_donation_reporter("prod_grok_workspace")
                .await
                .is_none(),
            "trace export must stay inert without a connection"
        );
        assert!(
            handle
                .log_donation_layer("prod_grok_workspace")
                .await
                .is_none(),
            "log export must stay inert without a connection"
        );
        assert!(
            handle
                .metric_donation_reporter("prod_grok_workspace")
                .await
                .is_none(),
            "metric export must stay inert without a connection"
        );
    }
    #[test]
    fn rewind_outcome_label_maps_each_variant() {
        assert_eq!(
            rewind_outcome_label(TurnHookOutcome::Completed),
            "completed"
        );
        assert_eq!(
            rewind_outcome_label(TurnHookOutcome::Cancelled),
            "cancelled"
        );
        assert_eq!(rewind_outcome_label(TurnHookOutcome::Error), "error");
    }
    #[test]
    fn rewind_domain_and_result_labels_are_stable() {
        assert_eq!(RewindDomain::Fs.as_str(), "fs");
        assert_eq!(RewindDomain::Hunk.as_str(), "hunk");
        assert_eq!(RewindDomain::Git.as_str(), "git");
        assert_eq!(rewind_result_label(true), "success");
        assert_eq!(rewind_result_label(false), "failure");
    }
    /// The per-bind handler builder maps the session's finalized toolset 1:1 —
    /// one handler per `tool_definitions()` entry, keyed by client name, with no
    /// extra handlers and no RPC handler (that is appended by the resolver /
    /// `connect_hub`, not here). The resolver-level "no intersection, no silent
    /// drop" guarantee is covered by
    /// [`resolver_advertises_tool_absent_from_connect_catalog`].
    #[tokio::test]
    async fn build_session_routed_handlers_covers_finalized_toolset() {
        let handle = make_handle();
        let session = handle.session("main").expect("main session exists");
        let toolset = session.toolset();
        let expected: std::collections::HashSet<String> = toolset
            .tool_definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert!(
            expected.contains("read_file"),
            "baseline toolset should expose read_file"
        );
        let handlers = build_session_routed_handlers(&toolset, &handle);
        let got: std::collections::HashSet<String> = handlers
            .iter()
            .map(|h| h.tool_id().as_str().to_owned())
            .collect();
        assert_eq!(handlers.len(), expected.len(), "one handler per tool def");
        assert_eq!(
            got, expected,
            "advertised handlers must equal the finalized toolset (no intersection)"
        );
    }
    #[tokio::test]
    async fn build_session_routed_handlers_skips_invalid_client_name_without_panic() {
        let handle = make_handle();
        let mut renamed = tc("GrokBuild:read_file", Some(ToolKind::Read));
        renamed.name_override = Some("bad name!".to_owned());
        let session = handle
            .create_session_with_config(
                "sess-invalid-name",
                None,
                Some(ToolServerConfig {
                    tools: vec![renamed, tc("GrokBuild:grep", Some(ToolKind::Read))],
                    behavior_preset: None,
                }),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session with invalidly renamed tool");
        let handlers = build_session_routed_handlers(&session.toolset(), &handle);
        let names: Vec<String> = handlers
            .iter()
            .map(|h| h.tool_id().as_str().to_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n == "bad name!"),
            "the invalid client name must be skipped: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "grep"),
            "valid tools must still get handlers: {names:?}"
        );
    }
    /// Regression for the deleted catalog intersection. Reproduces the
    /// `session.bind` resolver tail's composition — `build_session_routed_handlers`
    /// for the session toolset, plus the single RPC handler filtered from the
    /// connect-time catalog — and proves a session tool whose client name is
    /// ABSENT from that (grok-build) catalog is still advertised. The old
    /// `catalog ∩ session-names` filter silently dropped exactly such tools
    /// (grok-build renames → 6/11).
    #[tokio::test]
    async fn resolver_advertises_tool_absent_from_connect_catalog() {
        let handle = make_handle();
        let catalog_toolset = handle
            .session("main")
            .expect("main session exists")
            .toolset();
        let mut catalog = build_session_routed_handlers(&catalog_toolset, &handle);
        let rpc_handler: Arc<dyn xai_computer_hub_sdk::ToolServerHandler> =
            Arc::new(crate::hub_server::WorkspaceRpcHandler::new(handle.clone()));
        let rpc_tool_id = rpc_handler.tool_id();
        catalog.push(rpc_handler);
        let catalog_names: std::collections::HashSet<String> = catalog
            .iter()
            .map(|h| h.tool_id().as_str().to_owned())
            .collect();
        let mut renamed = tc("GrokBuild:read_file", Some(ToolKind::Read));
        renamed.name_override = Some("non_catalog_tool".to_owned());
        let session = handle
            .create_session_with_config(
                "sess-non-catalog",
                None,
                Some(ToolServerConfig {
                    tools: vec![renamed],
                    behavior_preset: None,
                }),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session with renamed tool");
        assert!(
            !catalog_names.contains("non_catalog_tool"),
            "precondition: the renamed tool must be absent from the catalog"
        );
        let toolset = session.toolset();
        let mut handlers = build_session_routed_handlers(&toolset, &handle);
        handlers.extend(
            catalog
                .iter()
                .filter(|h| h.tool_id() == rpc_tool_id)
                .cloned(),
        );
        let advertised: std::collections::HashSet<String> = handlers
            .iter()
            .map(|h| h.tool_id().as_str().to_owned())
            .collect();
        assert!(
            advertised.contains("non_catalog_tool"),
            "a session tool absent from the catalog must still be advertised"
        );
        assert_eq!(
            handlers
                .iter()
                .filter(|h| h.tool_id() == rpc_tool_id)
                .count(),
            1,
            "exactly one RPC handler appended"
        );
        let mut expected: std::collections::HashSet<String> = toolset
            .tool_definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        expected.insert(rpc_tool_id.as_str().to_owned());
        assert_eq!(advertised, expected);
    }
    /// Client names advertised by a session's current toolset.
    fn session_tool_names(session: &Arc<crate::session::WorkspaceSession>) -> Vec<String> {
        session
            .toolset()
            .tool_definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect()
    }
    /// The sandbox-resume regression (`workspace_tool_coverage_incomplete`): a
    /// session created by a metadata-less bind resolves the workspace default;
    /// a later rebind that carries the client's explicit toolset must
    /// re-resolve and swap it in — not silently reuse the default — so the
    /// bind response advertises the configured (renamed) tools. A repeat
    /// rebind with the identical config is a no-op reuse.
    #[tokio::test]
    async fn rebind_with_changed_explicit_toolset_reresolves_and_swaps() {
        let handle = make_handle();
        let session = handle
            .create_session_with_config("resumed", None, None, CapabilityMode::All, None, false)
            .expect("create default-resolved session");
        session.set_bind_tool_config_fingerprint(None);
        assert!(
            session_tool_names(&session)
                .iter()
                .all(|n| n != "renamed_read"),
            "precondition: the default toolset must not carry the override name"
        );
        let mut renamed = tc("GrokBuild:read_file", Some(ToolKind::Read));
        renamed.name_override = Some("renamed_read".to_owned());
        let cfg = ToolServerConfig {
            tools: vec![renamed],
            behavior_preset: None,
        };
        let fingerprint = serde_json::to_value(&cfg).ok();
        let (rebound, outcome) = handle
            .rebind_existing_hub_session("resumed", Some(cfg.clone()), fingerprint.clone())
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reresolved);
        assert_eq!(
            session_tool_names(&rebound),
            vec!["renamed_read".to_owned()],
            "the rebind must swap in the explicit toolset's resolution"
        );
        let (_, outcome) = handle
            .rebind_existing_hub_session("resumed", Some(cfg), fingerprint)
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reused);
    }
    /// A rebind without an explicit toolset (default resolution, or the
    /// fail-closed placeholders which the caller maps to `None`) must never
    /// downgrade an explicitly-configured session to the default toolset.
    #[tokio::test]
    async fn rebind_without_explicit_toolset_reuses_existing() {
        let handle = make_handle();
        let mut renamed = tc("GrokBuild:read_file", Some(ToolKind::Read));
        renamed.name_override = Some("renamed_read".to_owned());
        let cfg = ToolServerConfig {
            tools: vec![renamed],
            behavior_preset: None,
        };
        let session = handle
            .create_session_with_config(
                "configured",
                None,
                Some(cfg.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create configured session");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg).ok());
        let (rebound, outcome) = handle
            .rebind_existing_hub_session("configured", None, None)
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reused);
        assert_eq!(
            session_tool_names(&rebound),
            vec!["renamed_read".to_owned()],
            "a metadata-less rebind must not clobber the configured toolset"
        );
    }
    /// The create arm's fingerprint write is set-if-unset: a concurrent
    /// rebind that already swapped in its toolset (and recorded its
    /// fingerprint under `update_lock`) must not be clobbered by the create
    /// task's deferred write, or a later identical rebind would `Reused`-skip
    /// against a fingerprint that no longer describes the live toolset.
    #[tokio::test]
    async fn create_fingerprint_write_does_not_clobber_concurrent_rebind() {
        let handle = make_handle();
        let session = handle
            .create_session_with_config("racy", None, None, CapabilityMode::All, None, false)
            .expect("create session");
        let mut renamed = tc("GrokBuild:read_file", Some(ToolKind::Read));
        renamed.name_override = Some("renamed_read".to_owned());
        let cfg_b = ToolServerConfig {
            tools: vec![renamed],
            behavior_preset: None,
        };
        let fp_b = serde_json::to_value(&cfg_b).ok();
        let (_, outcome) = handle
            .rebind_existing_hub_session("racy", Some(cfg_b.clone()), fp_b.clone())
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reresolved);
        let fp_a = serde_json::to_value(&ToolServerConfig {
            tools: vec![tc("GrokBuild:list_dir", Some(ToolKind::ListDir))],
            behavior_preset: None,
        })
        .ok();
        session.set_bind_tool_config_fingerprint_if_unset(fp_a);
        let (rebound, outcome) = handle
            .rebind_existing_hub_session("racy", Some(cfg_b), fp_b)
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reused);
        assert_eq!(
            session_tool_names(&rebound),
            vec!["renamed_read".to_owned()]
        );
    }
    /// A vanished session yields `None` (the caller falls back to RPC-only).
    #[tokio::test]
    async fn rebind_missing_session_returns_none() {
        let handle = make_handle();
        assert!(
            handle
                .rebind_existing_hub_session("no-such-session", None, None)
                .await
                .is_none()
        );
    }
    fn swap_rejected_count(reason: &str, trigger: &str) -> u64 {
        crate::session::swap_policy::WORKSPACE_TOOLSET_SWAP_REJECTED_TOTAL
            .with_label_values(&[reason, trigger])
            .get()
    }
    /// The lazy-bind / resume-correction regression lock: a
    /// default-resolved session (stored fingerprint `None`) must accept the
    /// owner's explicit-config rebind even mid-turn with a call in flight —
    /// the owner bind is designed to land mid-turn, and deferring it would
    /// serve a toolset that contradicts the config-built prompt.
    #[tokio::test]
    async fn rebind_none_to_explicit_swaps_mid_turn() {
        let handle = make_handle();
        let session = handle
            .create_session_with_config("lazy", None, None, CapabilityMode::All, None, false)
            .expect("create default-resolved session");
        session.set_bind_tool_config_fingerprint(None);
        let tracker = handle.activity_tracker().clone();
        tracker.turn_started("lazy", 1);
        tracker.tool_call_started("lazy-c1", "read_file", Some("lazy"));
        let cfg = explicit_cfg("renamed_read");
        let fingerprint = serde_json::to_value(&cfg).ok();
        let (rebound, outcome) = handle
            .rebind_existing_hub_session("lazy", Some(cfg), fingerprint)
            .await
            .expect("session exists");
        assert_eq!(
            outcome,
            RebindOutcome::Reresolved,
            "a None → explicit correction must swap even mid-turn with calls in flight"
        );
        assert_eq!(
            session_tool_names(&rebound),
            vec!["renamed_read".to_owned()]
        );
    }
    /// `explicit → different-explicit` under dispatch: the rebind keeps the
    /// existing toolset (`ReresolveDeferredInFlight`, counted); once the
    /// call completes, a later rebind applies the correction.
    #[tokio::test]
    async fn rebind_explicit_to_explicit_with_in_flight_call_defers_then_corrects() {
        use xai_file_utils::events::ToolOutcome;
        let rejected_before = swap_rejected_count("in_flight", "owner_rebind");
        let handle = make_handle();
        let cfg_a = explicit_cfg("read_a");
        let session = handle
            .create_session_with_config(
                "busy",
                None,
                Some(cfg_a.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session with cfg A");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg_a).ok());
        let tracker = handle.activity_tracker().clone();
        tracker.tool_call_started("busy-c1", "read_a", Some("busy"));
        let cfg_b = explicit_cfg("read_b");
        let fp_b = serde_json::to_value(&cfg_b).ok();
        let (kept, outcome) = handle
            .rebind_existing_hub_session("busy", Some(cfg_b.clone()), fp_b.clone())
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::ReresolveDeferredInFlight);
        assert_eq!(
            session_tool_names(&kept),
            vec!["read_a".to_owned()],
            "the existing toolset must be kept while a call is in flight"
        );
        assert!(
            swap_rejected_count("in_flight", "owner_rebind") > rejected_before,
            "the deferred swap must be counted"
        );
        tracker.tool_call_completed("busy-c1", Some("busy"), ToolOutcome::Success);
        let (rebound, outcome) = handle
            .rebind_existing_hub_session("busy", Some(cfg_b), fp_b)
            .await
            .expect("session exists");
        assert_eq!(
            outcome,
            RebindOutcome::Reresolved,
            "the correction must apply once no calls are in flight"
        );
        assert_eq!(session_tool_names(&rebound), vec!["read_b".to_owned()]);
    }
    /// A reconnect's identical `session.bind` heals a stale session: reuse
    /// without the marker, defer in-flight, rebuild + clear once idle.
    #[tokio::test]
    async fn rebind_identical_reapply_repairs_stale_resolve() {
        use xai_file_utils::events::ToolOutcome;
        let handle = make_handle();
        let cfg = explicit_cfg("renamed_read");
        let fingerprint = serde_json::to_value(&cfg).ok();
        let session = handle
            .create_session_with_config(
                "stale-rebind",
                None,
                Some(cfg.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session");
        session.set_bind_tool_config_fingerprint(fingerprint.clone());
        let toolset_before = session.toolset();
        let (_, outcome) = handle
            .rebind_existing_hub_session("stale-rebind", Some(cfg.clone()), fingerprint.clone())
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reused);
        assert!(
            Arc::ptr_eq(&session.toolset(), &toolset_before),
            "without the stale marker the identical rebind must not rebuild"
        );
        session.mark_stale_resolve();
        let tracker = handle.activity_tracker().clone();
        tracker.tool_call_started("stale-c1", "read_file", Some("stale-rebind"));
        let rejected_before = swap_rejected_count("in_flight", "owner_rebind");
        let (kept, outcome) = handle
            .rebind_existing_hub_session("stale-rebind", Some(cfg.clone()), fingerprint.clone())
            .await
            .expect("session exists");
        assert_eq!(
            outcome,
            RebindOutcome::ReresolveDeferredInFlight,
            "the heal must defer while a call is in flight"
        );
        assert!(
            Arc::ptr_eq(&kept.toolset(), &toolset_before),
            "the deferred heal must keep the existing toolset"
        );
        assert!(kept.stale_resolve(), "the deferred heal keeps the marker");
        assert!(
            swap_rejected_count("in_flight", "owner_rebind") > rejected_before,
            "the deferred heal must be counted"
        );
        tracker.tool_call_completed("stale-c1", Some("stale-rebind"), ToolOutcome::Success);
        let (healed, outcome) = handle
            .rebind_existing_hub_session("stale-rebind", Some(cfg), fingerprint)
            .await
            .expect("session exists");
        assert_eq!(
            outcome,
            RebindOutcome::Reresolved,
            "the idle reconnect must repair the stale toolset"
        );
        assert!(
            !Arc::ptr_eq(&healed.toolset(), &toolset_before),
            "the heal must install a freshly resolved toolset"
        );
        assert!(
            !healed.stale_resolve(),
            "a successful install must clear the stale marker"
        );
    }
    /// The RPC path rejects a mid-turn config change with the retryable
    /// `TurnActive` error (counted); the retry at the turn boundary succeeds.
    #[tokio::test]
    async fn update_tool_config_rejects_mid_turn_then_succeeds_at_boundary() {
        let rejected_before = swap_rejected_count("turn_active", "update_tool_config");
        let handle = make_handle();
        handle.activity_tracker().turn_started("main", 1);
        let cfg = explicit_cfg("renamed_read");
        let err = handle
            .update_tool_config("main", "main", cfg.clone())
            .await
            .expect_err("a mid-turn config change must be rejected");
        assert!(
            matches!(err, WorkspaceError::TurnActive(ref s) if s == "main"),
            "got {err:?}"
        );
        assert!(
            swap_rejected_count("turn_active", "update_tool_config") > rejected_before,
            "the rejection must be counted"
        );
        let session = handle.session("main").expect("main session exists");
        assert!(
            session_tool_names(&session)
                .iter()
                .all(|n| n != "renamed_read"),
            "the rejected config must not take effect"
        );
        handle.activity_tracker().turn_completed("main", 1, 0);
        handle
            .update_tool_config("main", "main", cfg)
            .await
            .expect("the retry at the turn boundary must succeed");
        let session = handle.session("main").expect("main session exists");
        assert_eq!(
            session_tool_names(&session),
            vec!["renamed_read".to_owned()]
        );
    }
    /// TOCTOU lock: a turn that starts DURING the re-resolve (after the
    /// entry check passed) must still abort the install — the resolved
    /// toolset is discarded, the fingerprint stays unchanged, and the
    /// rejection is counted under `reason="turn_active_late"`. The retry at
    /// the turn boundary then succeeds.
    #[tokio::test]
    async fn update_tool_config_rejects_turn_started_during_resolve() {
        let late_rejected_before = swap_rejected_count("turn_active_late", "update_tool_config");
        let handle = make_handle();
        let session = handle.session("main").expect("main session exists");
        let toolset_before = session.toolset();
        let hook_handle = handle.clone();
        *handle.shared.post_resolve_test_hook.lock() = Some(Box::new(move || {
            hook_handle.activity_tracker().turn_started("main", 7);
        }));
        let cfg = explicit_cfg("late_read");
        let err = handle
            .update_tool_config("main", "main", cfg.clone())
            .await
            .expect_err("a turn starting mid-resolve must abort the install");
        assert!(
            matches!(err, WorkspaceError::TurnActive(ref s) if s == "main"),
            "got {err:?}"
        );
        assert!(
            swap_rejected_count("turn_active_late", "update_tool_config") > late_rejected_before,
            "the post-resolve rejection must be counted distinctly"
        );
        let session = handle.session("main").expect("main session exists");
        assert!(
            Arc::ptr_eq(&session.toolset(), &toolset_before),
            "the resolved toolset must be discarded, not installed"
        );
        assert!(
            session.bind_tool_config_matches(None),
            "the unapplied config's fingerprint must NOT be recorded"
        );
        *handle.shared.post_resolve_test_hook.lock() = None;
        handle.activity_tracker().turn_completed("main", 7, 0);
        handle
            .update_tool_config("main", "main", cfg)
            .await
            .expect("the retry at the turn boundary must succeed");
        let session = handle.session("main").expect("main session exists");
        assert_eq!(session_tool_names(&session), vec!["late_read".to_owned()]);
    }
    /// Re-applying the session's current config mid-turn stays allowed
    /// (matching fingerprint), so hot-reload re-applies keep working
    /// during turns.
    #[tokio::test]
    async fn update_tool_config_reapply_of_current_config_allowed_mid_turn() {
        let handle = make_handle();
        let cfg = explicit_cfg("renamed_read");
        let session = handle
            .create_session_with_config(
                "hot",
                None,
                Some(cfg.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg).ok());
        handle.activity_tracker().turn_started("hot", 1);
        handle
            .update_tool_config("hot", "hot", cfg)
            .await
            .expect("an identical-config re-apply must not be turn_active-rejected");
    }
    #[tokio::test]
    async fn update_tool_config_identical_reapply_repairs_stale_resolve() {
        let handle = make_handle();
        let cfg = explicit_cfg("renamed_read");
        let session = handle
            .create_session_with_config(
                "stale",
                None,
                Some(cfg.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg).ok());
        let toolset_before = session.toolset();
        handle
            .update_tool_config("stale", "stale", cfg.clone())
            .await
            .expect("an identical re-apply must succeed");
        assert!(
            Arc::ptr_eq(&session.toolset(), &toolset_before),
            "without the stale marker the identical re-apply must not rebuild"
        );
        session.mark_stale_resolve();
        let rejected_before = swap_rejected_count("turn_active", "update_tool_config");
        handle.activity_tracker().turn_started("stale", 1);
        let err = handle
            .update_tool_config("stale", "stale", cfg.clone())
            .await
            .expect_err("a mid-turn recovery re-apply must be rejected");
        assert!(
            matches!(err, WorkspaceError::TurnActive(ref s) if s == "stale"),
            "got {err:?}"
        );
        assert!(
            swap_rejected_count("turn_active", "update_tool_config") > rejected_before,
            "the rejected recovery must be counted"
        );
        assert!(
            session.stale_resolve(),
            "the rejected recovery must keep the stale marker"
        );
        assert!(
            Arc::ptr_eq(&session.toolset(), &toolset_before),
            "the rejected recovery must not install"
        );
        handle.activity_tracker().turn_completed("stale", 1, 0);
        handle
            .update_tool_config("stale", "stale", cfg.clone())
            .await
            .expect("the boundary retry must repair the stale toolset");
        let session = handle.session("stale").expect("session exists");
        assert!(
            !Arc::ptr_eq(&session.toolset(), &toolset_before),
            "the recovery re-apply must install a freshly resolved toolset"
        );
        assert!(
            !session.stale_resolve(),
            "a successful install must clear the stale marker"
        );
        assert!(
            session.bind_tool_config_matches(serde_json::to_value(&cfg).ok().as_ref()),
            "the stored fingerprint must be unchanged by the identical recovery"
        );
    }
    /// The `Terminal` resource of a session's current toolset.
    async fn toolset_terminal(
        toolset: &Arc<xai_grok_tools::registry::types::FinalizedToolset>,
    ) -> Arc<dyn xai_grok_tools::computer::types::TerminalBackend> {
        let res = toolset.resources.lock().await;
        res.get::<xai_grok_tools::types::resources::Terminal>()
            .map(|t| t.0.clone())
            .expect("toolset must carry a Terminal resource")
    }
    fn orphaned_swap_count() -> u64 {
        WORKSPACE_TERMINAL_BACKEND_ORPHANED_TOTAL
            .with_label_values(&["swap"])
            .get()
    }
    fn explicit_cfg(name_override: &str) -> ToolServerConfig {
        let mut renamed = tc("GrokBuild:read_file", Some(ToolKind::Read));
        renamed.name_override = Some(name_override.to_owned());
        ToolServerConfig {
            tools: vec![renamed],
            behavior_preset: None,
        }
    }
    /// Background-capable toolset (execute + task-output + kill), the shape
    /// the restart-recovery and RPC-survival tests resolve.
    pub(crate) fn background_capable_cfg() -> ToolServerConfig {
        ToolServerConfig {
            tools: vec![
                tc("GrokBuild:read_file", Some(ToolKind::Read)),
                tc("GrokBuild:run_terminal_cmd", Some(ToolKind::Execute)),
                tc(
                    "GrokBuild:get_task_output",
                    Some(ToolKind::BackgroundTaskAction),
                ),
                tc("GrokBuild:kill_task", Some(ToolKind::KillTaskAction)),
            ],
            behavior_preset: None,
        }
    }
    /// A minimal bash-kind [`TerminalRunRequest`] for `command`, writing
    /// output under `out_dir`.
    ///
    /// [`TerminalRunRequest`]: xai_grok_tools::computer::types::TerminalRunRequest
    pub(crate) fn terminal_run_request(
        command: &str,
        out_dir: &std::path::Path,
        tool_call_id: &str,
    ) -> xai_grok_tools::computer::types::TerminalRunRequest {
        xai_grok_tools::computer::types::TerminalRunRequest {
            command: command.to_string(),
            working_directory: out_dir.to_path_buf(),
            env: std::collections::HashMap::new(),
            timeout: std::time::Duration::from_secs(60),
            output_byte_limit: 4096,
            output_file: out_dir.join(format!("{tool_call_id}.out")),
            notification_handle: xai_grok_tools::notification::ToolNotificationHandle::noop(),
            tool_call_id: tool_call_id.to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: xai_grok_tools::computer::types::TaskKind::Bash,
            owner_session_id: None,
            description: None,
        }
    }
    /// Start a `sleep 30` background task on `session`'s owned backend and
    /// return its handle. Shared by the swap-survival, rebind-survival, and
    /// restart tests.
    pub(crate) async fn start_background_sleep(
        session: &Arc<crate::session::WorkspaceSession>,
        out_dir: &std::path::Path,
        tool_call_id: &str,
    ) -> xai_grok_tools::computer::types::BackgroundHandle {
        session
            .terminal_backend()
            .run_background(terminal_run_request("sleep 30", out_dir, tool_call_id))
            .await
            .expect("start background task")
    }
    /// A rebind that swaps in a different explicit toolset must rebuild the
    /// toolset AROUND the session-owned terminal backend, not a fresh one —
    /// that identity is what keeps background tasks alive across the swap.
    #[tokio::test]
    async fn rebind_swap_preserves_session_terminal_backend() {
        let orphaned_before = orphaned_swap_count();
        let handle = make_handle();
        let cfg_a = explicit_cfg("read_a");
        let session = handle
            .create_session_with_config(
                "owned",
                None,
                Some(cfg_a.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session with cfg A");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg_a).ok());
        let backend = session.terminal_backend().clone();
        assert!(
            Arc::ptr_eq(&backend, &toolset_terminal(&session.toolset()).await),
            "create must wire the session-owned backend into the toolset"
        );
        let cfg_b = explicit_cfg("read_b");
        let fingerprint_b = serde_json::to_value(&cfg_b).ok();
        let (rebound, outcome) = handle
            .rebind_existing_hub_session("owned", Some(cfg_b), fingerprint_b)
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reresolved);
        assert_eq!(session_tool_names(&rebound), vec!["read_b".to_owned()]);
        assert!(
            Arc::ptr_eq(&backend, rebound.terminal_backend()),
            "the session-owned backend must not be replaced by a swap"
        );
        assert!(
            Arc::ptr_eq(&backend, &toolset_terminal(&rebound.toolset()).await),
            "the swapped-in toolset must reference the session-owned backend"
        );
        assert_eq!(
            orphaned_swap_count(),
            orphaned_before,
            "the orphaned-backend tripwire must stay 0"
        );
    }
    /// A snapshot-driven `re_resolve_all_sessions` rebuild (MCP snapshot
    /// change) must also rebuild around the session-owned backend — with a
    /// LIVE background task riding through the rebuild. This is the
    /// regression lock for snapshot-triggered swaps killing background
    /// tasks by minting a fresh backend per session.
    #[tokio::test]
    async fn re_resolve_all_sessions_preserves_session_terminal_backend() {
        let orphaned_before = orphaned_swap_count();
        let handle = make_handle();
        let session = handle.session("main").expect("main session exists");
        let backend = session.terminal_backend().clone();
        let out_dir = tempfile::tempdir().expect("temp dir");
        let bg = start_background_sleep(&session, out_dir.path(), "snapshot-bg").await;
        handle.shared.mcp_tools_snapshot.store(Arc::new(vec![tc(
            "GrokBuild:read_file",
            Some(ToolKind::Read),
        )]));
        let rebuilt = handle
            .shared
            .re_resolve_all_sessions("mcp_snapshot_changed", true)
            .await;
        assert!(rebuilt >= 1, "the main session must be rebuilt");
        let session = handle.session("main").expect("main session still exists");
        assert!(
            Arc::ptr_eq(&backend, session.terminal_backend()),
            "the session-owned backend must survive a snapshot rebuild"
        );
        let new_terminal = toolset_terminal(&session.toolset()).await;
        assert!(
            Arc::ptr_eq(&backend, &new_terminal),
            "the rebuilt toolset must reference the session-owned backend"
        );
        assert!(
            !new_terminal
                .get_task(&bg.task_id)
                .await
                .expect("the task table must survive the snapshot rebuild")
                .completed,
            "the task's process must still be running after the rebuild"
        );
        assert_eq!(
            orphaned_swap_count(),
            orphaned_before,
            "the orphaned-backend tripwire must stay 0"
        );
        new_terminal.kill_task(&bg.task_id).await;
    }
    /// A local-bound session (external toolset installed via
    /// `bind_local_session`: the toolset keeps the shell's backend, the
    /// session-owned backend is an idle decoy) must be SKIPPED by
    /// snapshot-driven rebuilds — rebuilding around the decoy would detach
    /// tools from the shell's live task table — and must not fire the
    /// orphan tripwire (the mismatch is the local-bind contract).
    #[tokio::test]
    async fn local_bound_session_skips_snapshot_rebuild() {
        let orphaned_before = orphaned_swap_count();
        let handle = make_handle();
        let donor = handle
            .create_session_with_config(
                "donor",
                None,
                Some(explicit_cfg("read_donor")),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create donor session");
        let local = handle
            .create_session_with_config(
                "local",
                None,
                Some(explicit_cfg("read_local")),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create local session");
        let external_toolset = donor.toolset();
        local.replace(local.effective_tool_config(), external_toolset.clone());
        assert!(
            !local.toolset_terminal_is_session_owned().await,
            "precondition: the installed toolset's Terminal must be external"
        );
        handle.shared.mcp_tools_snapshot.store(Arc::new(vec![tc(
            "GrokBuild:read_file",
            Some(ToolKind::Read),
        )]));
        handle
            .shared
            .re_resolve_all_sessions("mcp_snapshot_changed", true)
            .await;
        let local = handle.session("local").expect("local session still exists");
        assert!(
            Arc::ptr_eq(&local.toolset(), &external_toolset),
            "the local-bound session's toolset must be untouched by the rebuild"
        );
        assert!(
            Arc::ptr_eq(
                &toolset_terminal(&local.toolset()).await,
                donor.terminal_backend()
            ),
            "the external (shell) backend must still ride the toolset"
        );
        assert_eq!(
            orphaned_swap_count(),
            orphaned_before,
            "the skip must not fire the orphaned-backend tripwire"
        );
        let outcome = handle
            .resolve_and_swap_session_toolset(
                &local,
                explicit_cfg("read_new"),
                SwapTrigger::UpdateRpc,
            )
            .await
            .expect("the skip is not an internal error at the choke point");
        assert_eq!(outcome, SwapOutcome::SkippedExternallyOwned);
        assert!(
            Arc::ptr_eq(&local.toolset(), &external_toolset),
            "the choke point must not swap an externally-owned toolset"
        );
        assert_eq!(orphaned_swap_count(), orphaned_before);
        let err = handle
            .update_tool_config("local", "local", explicit_cfg("read_new"))
            .await
            .expect_err("update_tool_config must refuse an externally-owned toolset");
        assert!(
            matches!(err, crate::error::WorkspaceError::ToolsetExternallyOwned(ref s) if s == "local"),
            "expected ToolsetExternallyOwned, got: {err:?}"
        );
        assert!(
            Arc::ptr_eq(&local.toolset(), &external_toolset),
            "the refused update must leave the toolset untouched"
        );
        let fp_local = serde_json::to_value(explicit_cfg("read_local")).ok();
        local.set_bind_tool_config_fingerprint(fp_local.clone());
        let cfg_new = explicit_cfg("read_new2");
        let fp_new = serde_json::to_value(&cfg_new).ok();
        let (rebound, outcome) = handle
            .rebind_existing_hub_session("local", Some(cfg_new), fp_new.clone())
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::KeptExternallyOwned);
        assert!(
            Arc::ptr_eq(&rebound.toolset(), &external_toolset),
            "the rebind must keep the externally-owned toolset"
        );
        assert!(
            rebound.bind_tool_config_matches(fp_local.as_ref()),
            "the stored fingerprint must be unchanged by the skipped swap"
        );
        assert!(
            !rebound.bind_tool_config_matches(fp_new.as_ref()),
            "the unapplied config's fingerprint must NOT be recorded"
        );
        assert_eq!(orphaned_swap_count(), orphaned_before);
        handle
            .update_tool_config("local", "local", explicit_cfg("read_local"))
            .await
            .expect("an identical config on an externally-owned toolset is a no-op success");
        assert!(
            Arc::ptr_eq(&local.toolset(), &external_toolset),
            "the identical no-op must leave the externally-owned toolset untouched"
        );
        assert!(
            local.bind_tool_config_matches(fp_local.as_ref()),
            "the identical no-op must leave the stored fingerprint untouched"
        );
        assert_eq!(orphaned_swap_count(), orphaned_before);
    }
    /// A background task started before a toolset swap must still be
    /// queryable through the NEW toolset's `Terminal` resource — the
    /// swap ⇒ empty task table + SIGKILL incident class.
    #[tokio::test]
    async fn background_task_survives_toolset_swap() {
        let orphaned_before = orphaned_swap_count();
        let handle = make_handle();
        let cfg_a = explicit_cfg("read_a");
        let session = handle
            .create_session_with_config(
                "bg",
                None,
                Some(cfg_a.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg_a).ok());
        let out_dir = tempfile::tempdir().expect("temp dir");
        let bg = start_background_sleep(&session, out_dir.path(), "bg-task").await;
        let cfg_b = explicit_cfg("read_b");
        let fingerprint_b = serde_json::to_value(&cfg_b).ok();
        let (rebound, outcome) = handle
            .rebind_existing_hub_session("bg", Some(cfg_b), fingerprint_b)
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reresolved);
        let new_terminal = toolset_terminal(&rebound.toolset()).await;
        let task = new_terminal
            .get_task(&bg.task_id)
            .await
            .expect("the task table must survive the toolset swap");
        assert!(
            !task.completed,
            "the task's process must still be running after the swap"
        );
        assert_eq!(
            orphaned_swap_count(),
            orphaned_before,
            "the orphaned-backend tripwire must stay 0"
        );
        new_terminal.kill_task(&bg.task_id).await;
    }
    /// Test factory whose sessions own a PERSISTENT-shell backend (the
    /// production factory shape). The plain [`TestSessionContextFactory`]
    /// builds a non-persistent backend, which tracks no shell cwd — hence
    /// this wrapper for the shell-state-survival test.
    struct PersistentShellFactory {
        inner: TestSessionContextFactory,
    }
    impl crate::config::SessionContextFactory for PersistentShellFactory {
        fn build_session_context(
            &self,
            session_id: &str,
            cwd: std::path::PathBuf,
            session_env: Arc<std::collections::HashMap<String, String>>,
            backend: Arc<dyn xai_grok_tools::computer::types::TerminalBackend>,
        ) -> xai_grok_tools::registry::types::SessionContext {
            self.inner
                .build_session_context(session_id, cwd, session_env, backend)
        }
        fn build_terminal_backend(&self) -> crate::config::SessionTerminalBackend {
            crate::config::SessionTerminalBackend::local(
                xai_grok_tools::computer::local::LocalTerminalBackend::with_persistent_shell(),
            )
        }
        fn registry_builder(&self) -> xai_grok_tools::registry::types::ToolRegistryBuilder {
            self.inner.registry_builder()
        }
    }
    /// [`make_handle`] shape around a [`PersistentShellFactory`]; no
    /// pre-created session.
    fn make_persistent_shell_handle() -> WorkspaceHandle {
        let factory = Arc::new(PersistentShellFactory {
            inner: TestSessionContextFactory::new(),
        });
        let root_cwd = factory.inner.temp.path().to_path_buf();
        let config = WorkspaceConfig {
            root_cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        WorkspaceHandle::build(
            config,
            ephemeral_workspace_home(),
            None,
            true,
            false,
            false,
            false,
            false,
            crate::upload::environment::WorkspaceIdentity::default(),
        )
        .expect("handle construction should succeed")
    }
    /// The persistent shell's state (a model-issued `cd`) survives a
    /// `Reresolved` toolset swap, because the shell lives inside the
    /// session-owned backend — the isolation-matrix #3 "persistent-shell
    /// cwd preserved" sub-assert, on the production backend shape
    /// (`with_persistent_shell`). Unix-only, like the persistent shell.
    #[cfg(unix)]
    #[tokio::test]
    async fn reresolved_swap_preserves_persistent_shell_cwd() {
        let handle = make_persistent_shell_handle();
        let root = handle.root_cwd().expect("root cwd");
        let cfg_a = explicit_cfg("read_a");
        let session = handle
            .create_session_with_config(
                "shell-swap",
                None,
                Some(cfg_a.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg_a).ok());
        std::fs::create_dir_all(root.join("swap_kept_dir")).expect("create subdir");
        let result = session
            .terminal_backend()
            .run(terminal_run_request("cd swap_kept_dir", &root, "shell-cd"))
            .await
            .expect("cd through the persistent shell");
        assert_eq!(
            result.exit_code,
            Some(0),
            "cd must succeed: {}",
            result.combined_output
        );
        let cwd_before = session
            .terminal_backend()
            .get_shell_cwd()
            .await
            .expect("the persistent shell must track a cwd after a command");
        assert_eq!(
            cwd_before.file_name().and_then(|n| n.to_str()),
            Some("swap_kept_dir"),
            "the shell must have entered the subdir: {}",
            cwd_before.display()
        );
        let cfg_b = explicit_cfg("read_b");
        let (rebound, outcome) = handle
            .rebind_existing_hub_session(
                "shell-swap",
                Some(cfg_b.clone()),
                serde_json::to_value(&cfg_b).ok(),
            )
            .await
            .expect("session exists");
        assert_eq!(outcome, RebindOutcome::Reresolved);
        let cwd_after = toolset_terminal(&rebound.toolset())
            .await
            .get_shell_cwd()
            .await
            .expect("the swapped-in toolset's terminal must still track the shell cwd");
        assert_eq!(
            cwd_after, cwd_before,
            "the persistent shell's cwd must survive the toolset swap"
        );
    }
    /// Each fork owns its own fresh backend: fork teardown kills only the
    /// fork's tasks, never the parent's.
    #[tokio::test]
    async fn fork_session_owns_distinct_terminal_backend() {
        let handle = make_handle();
        let parent = handle.session("main").expect("main session exists");
        let fork = handle
            .fork_session(fork_cfg_with(
                "fork-backend",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork succeeds");
        assert!(
            !Arc::ptr_eq(parent.terminal_backend(), fork.terminal_backend()),
            "a fork must own its own backend, not share the parent's"
        );
        assert!(
            Arc::ptr_eq(
                fork.terminal_backend(),
                &toolset_terminal(&fork.toolset()).await
            ),
            "the fork's toolset must reference the fork-owned backend"
        );
    }
    /// Poll `backend` with a trivial command until its actor refuses it —
    /// proving an explicit shutdown, since callers still hold live `Arc`s.
    /// Shared by the `drop_session` and hub-evict teardown tests.
    pub(crate) async fn assert_backend_stops(
        backend: &Arc<dyn xai_grok_tools::computer::types::TerminalBackend>,
    ) {
        let out_dir = tempfile::tempdir().expect("temp dir");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let request = terminal_run_request("true", out_dir.path(), "probe");
            if backend.run(request).await.is_err() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "backend actor must stop after an explicit shutdown even with live Arcs"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    /// `drop_session` shuts the backend down explicitly: the actor stops even
    /// while other `Arc`s to the backend are still alive (teardown must not
    /// depend on the last toolset `Arc` dropping).
    #[tokio::test]
    async fn drop_session_shuts_down_terminal_backend_explicitly() {
        let handle = make_handle();
        let session = handle
            .create_session_with_config("doomed", None, None, CapabilityMode::All, None, false)
            .expect("create session");
        let retained_backend = session.terminal_backend().clone();
        let retained_toolset = session.toolset();
        drop(session);
        handle.drop_session("doomed", "doomed").expect("drop");
        assert_backend_stops(&retained_backend).await;
        drop(retained_toolset);
    }
    async fn assert_hunk_tracker_stops(tracker: &xai_hunk_tracker::HunkTrackerHandle) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !tracker.is_closed() {
            assert!(
                std::time::Instant::now() < deadline,
                "hunk-tracker actor must stop within the deadline despite live \
                 handle clones"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    /// `drop_session` cancels the workspace-spawned hunk-tracker actor even
    /// while a leaked `HunkTrackerHandle` clone keeps its command channel
    /// open. Rationale on `cancel_hunk_tracker`.
    #[tokio::test]
    async fn drop_session_cancels_workspace_spawned_hunk_tracker() {
        let handle = make_handle();
        let session = handle
            .create_session_with_config("doomed-ht", None, None, CapabilityMode::All, None, false)
            .expect("create session");
        let leaked_tracker = session.hunk_tracker().clone();
        assert!(
            !leaked_tracker.is_closed(),
            "precondition: the actor is alive while the session exists"
        );
        drop(session);
        handle.drop_session("doomed-ht", "doomed-ht").expect("drop");
        assert_hunk_tracker_stops(&leaked_tracker).await;
    }
    /// Same guarantee for the fork spawn site.
    #[tokio::test]
    async fn drop_session_cancels_forked_session_hunk_tracker() {
        let handle = make_handle();
        let child = handle
            .fork_session(fork_cfg_with(
                "child-ht",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        let leaked_tracker = child.hunk_tracker().clone();
        assert!(
            !leaked_tracker.is_closed(),
            "precondition: the actor is alive while the session exists"
        );
        drop(child);
        handle.drop_session("child-ht", "child-ht").expect("drop");
        assert_hunk_tracker_stops(&leaked_tracker).await;
    }
    /// The inverse guarantee: a tracker bound via `create_session_with_tracker`
    /// is externally owned, so `drop_session` must NOT cancel it. The agent
    /// shares such trackers with the workspace session.
    #[tokio::test]
    async fn drop_session_leaves_externally_owned_hunk_tracker_alive() {
        let handle = make_handle();
        let cwd = handle.shared.root_cwd.clone();
        let (hunk_event_tx, _hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let owner_cancel = tokio_util::sync::CancellationToken::new();
        let tracker = HunkTrackerActor::spawn(
            "external-ht".to_string(),
            cwd.clone(),
            hunk_event_tx,
            TrackingMode::AllDirty,
            owner_cancel.clone(),
        );
        let session = handle
            .create_session_with_tracker(
                "external-ht",
                cwd,
                tracker.clone(),
                None,
                CapabilityMode::All,
            )
            .expect("create session");
        assert!(
            !tracker.is_closed(),
            "precondition: the actor is alive while the session exists"
        );
        drop(session);
        handle
            .drop_session("external-ht", "external-ht")
            .expect("drop");
        let _ = tracker.get_all_hunks().await;
        assert!(
            !tracker.is_closed(),
            "drop_session must not cancel an externally owned hunk tracker"
        );
        owner_cancel.cancel();
        assert_hunk_tracker_stops(&tracker).await;
    }
    /// Isolation matrix #5: a workspace process restart loses tasks (they are
    /// process state — physics), and what's pinned here is the recovery UX:
    /// the same session id recreates cleanly on the fresh process, the task
    /// table starts empty (loss is visible, not silent), and `get_task_output`
    /// for the lost id returns the informative not-found message.
    #[tokio::test]
    async fn restarted_workspace_recreates_session_and_reports_lost_task() {
        let handle_a = make_handle();
        let session_a = handle_a
            .create_session_with_config(
                "reborn",
                None,
                Some(background_capable_cfg()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session");
        let out_dir = tempfile::tempdir().expect("temp dir");
        let bg = start_background_sleep(&session_a, out_dir.path(), "restart-bg").await;
        assert!(
            session_a
                .terminal_backend()
                .get_task(&bg.task_id)
                .await
                .is_some(),
            "precondition: the task exists in the first process"
        );
        let handle_b = make_handle();
        let session_b = handle_b
            .create_session_with_config(
                "reborn",
                None,
                Some(background_capable_cfg()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("the session must recreate cleanly after a restart");
        assert!(
            session_b.terminal_backend().list_tasks().await.is_empty(),
            "precondition: a fresh handle must start with an empty task table"
        );
        let result = session_b
            .toolset()
            .call(
                "get_task_output",
                serde_json::json!({"task_ids": [bg.task_id.clone()]}),
                "restart-probe",
                None,
            )
            .await
            .expect("get_task_output must answer, not error");
        let xai_grok_tools::types::output::ToolOutput::TaskOutput(
            xai_tool_types::TaskOutputOutput::TaskNotFound(msg),
        ) = &result.output
        else {
            panic!("expected TaskNotFound, got: {:?}", result.output);
        };
        assert!(
            msg.contains(&format!("Task {} not found", bg.task_id)),
            "the message must name the lost task id: {msg}"
        );
        assert!(
            msg.contains("No background tasks or subagents exist in this session"),
            "the message must say the restarted session has no tasks: {msg}"
        );
        session_a.terminal_backend().kill_task(&bg.task_id).await;
    }
    /// The typed helpers feed the registry and the targeted counters advance.
    /// Counters are monotonic, so `after > before` is robust despite the
    /// process-global registry and parallel tests (capture, restore, canary).
    #[test]
    fn rewind_metric_helpers_record_observable_effects() {
        let capture_labels = [
            RewindDomain::Git.as_str(),
            rewind_outcome_label(TurnHookOutcome::Cancelled),
        ];
        let restore_labels = [RewindDomain::Fs.as_str(), rewind_result_label(true)];
        let canary_label = [rewind_outcome_label(TurnHookOutcome::Error)];
        let capture_before = REWIND_CHECKPOINT_CAPTURE_TOTAL
            .with_label_values(&capture_labels)
            .get();
        let restore_before = REWIND_RESTORE_TOTAL
            .with_label_values(&restore_labels)
            .get();
        let canary_before = REWIND_NON_COMPLETED_FINALIZE_TOTAL
            .with_label_values(&canary_label)
            .get();
        record_rewind_capture(RewindDomain::Git, TurnHookOutcome::Cancelled);
        observe_rewind_capture_duration(RewindDomain::Hunk, 0.002);
        record_rewind_restore(RewindDomain::Fs, true);
        record_rewind_restore(RewindDomain::Git, false);
        record_fs_finalize(TurnHookOutcome::Completed, 0.001);
        record_non_completed_finalize_canary(TurnHookOutcome::Error);
        assert!(
            REWIND_CHECKPOINT_CAPTURE_TOTAL
                .with_label_values(&capture_labels)
                .get()
                > capture_before,
            "capture counter must advance"
        );
        assert!(
            REWIND_RESTORE_TOTAL
                .with_label_values(&restore_labels)
                .get()
                > restore_before,
            "restore counter must advance"
        );
        assert!(
            REWIND_NON_COMPLETED_FINALIZE_TOTAL
                .with_label_values(&canary_label)
                .get()
                > canary_before,
            "canary counter must advance"
        );
    }
    /// The client ext-notification sink is invoked with the emitted method +
    /// params, and is no-op until installed.
    #[tokio::test]
    async fn client_ext_sink_receives_emitted_notification() {
        let handle = make_handle();
        assert!(!handle.has_client_ext_sink());
        handle.emit_client_ext("x.ai/noop".to_string(), serde_json::json!({}));
        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink_captured = captured.clone();
        handle.set_client_ext_sink(Arc::new(move |method, params| {
            sink_captured.lock().push((method, params));
        }));
        assert!(handle.has_client_ext_sink());
        handle.emit_client_ext(
            "x.ai/search/fuzzy/status".to_string(),
            serde_json::json!({"a": 1}),
        );
        let got = captured.lock();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "x.ai/search/fuzzy/status");
        assert_eq!(got[0].1, serde_json::json!({"a": 1}));
    }
    /// End-to-end local streaming: open + change a fuzzy search over real files,
    /// run the notification driver, and assert a correctly-shaped
    /// `x.ai/search/fuzzy/status` is delivered through the sink with the match.
    #[tokio::test]
    async fn fuzzy_change_streams_status_through_sink() {
        use crate::file_system::TargetClientId;
        let handle = make_handle();
        let cwd = handle.root_cwd().unwrap();
        std::fs::write(cwd.join("alpha_widget.rs"), b"").unwrap();
        std::fs::write(cwd.join("beta_gadget.rs"), b"").unwrap();
        let captured = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
        let sink_captured = captured.clone();
        handle.set_client_ext_sink(Arc::new(move |method, params| {
            if method == "x.ai/search/fuzzy/status" {
                sink_captured.lock().push(params);
            }
        }));
        let search_id = handle
            .fuzzy_open(
                Some(cwd.as_path()),
                None,
                false,
                Some("sess-1".into()),
                TargetClientId::None,
            )
            .await;
        let (min_gen, has_query, query_version) = handle
            .fuzzy_change(&search_id, "alpha_widget", false)
            .await
            .expect("search should exist");
        handle
            .run_fuzzy_notifications(search_id.clone(), min_gen, has_query, query_version, 50)
            .await;
        let got = captured.lock();
        assert!(
            !got.is_empty(),
            "expected at least one fuzzy status notification"
        );
        let last = got.last().unwrap();
        assert_eq!(last["sessionId"], "sess-1");
        assert_eq!(last["searchId"], serde_json::json!(search_id));
        let matches = last["matches"].as_array().expect("matches array");
        assert!(
            matches.iter().any(|m| m["path"]
                .as_str()
                .is_some_and(|p| p.contains("alpha_widget"))),
            "expected alpha_widget in matches, got: {last}"
        );
    }
    /// Like [`make_handle`] but with `events_enabled = true` and a known
    /// `workspace_home` (returned `TempDir`) so tests can read the per-session
    /// `events.jsonl`. Bypasses the env flag via the private `build` seam so the
    /// assertion never races a sibling test's process environment.
    pub(crate) fn make_handle_with_events() -> (WorkspaceHandle, tempfile::TempDir) {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        let home = tempfile::tempdir().unwrap();
        let handle = WorkspaceHandle::build(
            config,
            home.path().to_path_buf(),
            None,
            true,
            false,
            true,
            false,
            false,
            crate::upload::environment::WorkspaceIdentity::default(),
        )
        .expect("handle construction should succeed");
        (handle, home)
    }
    /// Full wiring: a turn with a tool call, the volatile-config toggles, and a
    /// representative `Mcp*` event all land in the per-session `events.jsonl`
    /// with truthful field content.
    #[tokio::test]
    async fn events_jsonl_captures_turn_tool_toggle_and_mcp_variants() {
        use xai_file_utils::events::ToolOutcome;
        use xai_tool_protocol::turn_hook::{AfterTurnPayload, BeforeTurnPayload, TurnHookOutcome};
        let (handle, home) = make_handle_with_events();
        let sid = "sess-int";
        handle
            .on_before_turn(
                sid,
                &BeforeTurnPayload {
                    turn_number: 7,
                    model_id: "grok-4".to_owned(),
                    yolo_mode: false,
                    conversation_message_count: 5,
                    session_relationship: "subagent".to_owned(),
                    schema_version: "1.0".to_owned(),
                },
            )
            .await;
        let tracker = handle.activity_tracker();
        tracker.tool_call_started("c1", "read_file", Some(sid));
        tracker.tool_call_completed("c1", Some(sid), ToolOutcome::Success);
        handle.on_yolo_toggled(sid, true);
        handle.on_mcp_server_toggled(sid, "linear", false);
        handle.shared().session_event_writer(sid).emit(
            xai_file_utils::events::Event::McpToolCallStarted {
                server_name: "linear".into(),
                tool_name: "list_issues".into(),
                call_id: "mcp-1".into(),
                timeout_sec: 30,
            },
        );
        handle
            .on_after_turn(
                sid,
                &AfterTurnPayload {
                    turn_number: 7,
                    outcome: TurnHookOutcome::Completed,
                    duration_ms: 1234,
                    tool_call_count: 1,
                    model_id: "grok-4".to_owned(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                },
            )
            .await;
        let path = home.path().join("sessions").join(sid).join("events.jsonl");
        let text = std::fs::read_to_string(&path).expect("events.jsonl must exist");
        let events: Vec<serde_json::Value> = text
            .trim()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let by_type = |t: &str| {
            events
                .iter()
                .find(|e| e["type"] == t)
                .unwrap_or_else(|| panic!("{t} event missing from events.jsonl"))
        };
        let ts = by_type("turn_started");
        assert_eq!(ts["session_id"], sid);
        assert_eq!(ts["turn_number"], 7);
        assert_eq!(ts["model_id"], "grok-4");
        assert_eq!(ts["yolo_mode"], false);
        assert_eq!(ts["conversation_message_count"], 5);
        assert_eq!(ts["session_relationship"], "subagent");
        assert_eq!(ts["schema_version"], "1.0");
        assert_eq!(by_type("tool_started")["tool_name"], "read_file");
        let tc = by_type("tool_completed");
        assert_eq!(tc["tool_name"], "read_file");
        assert_eq!(tc["outcome"], "success");
        assert_eq!(by_type("yolo_toggled")["enabled"], true);
        let mcp_toggle = by_type("mcp_server_toggled");
        assert_eq!(mcp_toggle["server_name"], "linear");
        assert_eq!(mcp_toggle["enabled"], false);
        let mcp_call = by_type("mcp_tool_call_started");
        assert_eq!(mcp_call["server_name"], "linear");
        assert_eq!(mcp_call["tool_name"], "list_issues");
        assert_eq!(by_type("turn_ended")["outcome"], "completed");
        let pos = |t: &str| events.iter().position(|e| e["type"] == t).unwrap();
        assert!(
            pos("turn_started") < pos("tool_started"),
            "turn_started must precede tool_started"
        );
        assert!(
            pos("tool_completed") < pos("turn_ended"),
            "tool_completed must precede turn_ended"
        );
    }
    /// Both before-turn hook delivery styles sync YOLO state into the session.
    #[tokio::test]
    async fn before_turn_hooks_sync_session_yolo_mode() {
        use xai_tool_protocol::turn_hook::{BeforeTurnPayload, TurnHookRequest};
        let handle = make_handle();
        let session = handle.session("main").expect("main session");
        assert!(!session.yolo_mode(), "fail-closed default");
        handle
            .on_before_turn(
                "main",
                &BeforeTurnPayload {
                    turn_number: 1,
                    model_id: "grok-4".to_owned(),
                    yolo_mode: true,
                    ..Default::default()
                },
            )
            .await;
        assert!(session.yolo_mode(), "on_before_turn must sync yolo on");
        let reply = handle
            .compute_turn_injections(
                "main",
                &TurnHookRequest::Before(BeforeTurnPayload {
                    turn_number: 2,
                    model_id: "grok-4".to_owned(),
                    yolo_mode: false,
                    ..Default::default()
                }),
            )
            .await;
        assert_eq!(
            reply,
            xai_tool_protocol::turn_hook::HookReply::default(),
            "reply stays a behavior-neutral no-op"
        );
        assert!(
            !session.yolo_mode(),
            "compute_turn_injections must sync yolo off"
        );
        handle
            .compute_turn_injections(
                "never-bound",
                &TurnHookRequest::Before(BeforeTurnPayload {
                    turn_number: 1,
                    model_id: "grok-4".to_owned(),
                    yolo_mode: true,
                    ..Default::default()
                }),
            )
            .await;
    }
    /// YOLO transitions emit `yolo_toggled` in events.jsonl; repeats don't.
    #[tokio::test]
    async fn before_turn_yolo_transition_emits_yolo_toggled_event() {
        use xai_tool_protocol::turn_hook::BeforeTurnPayload;
        let (handle, home) = make_handle_with_events();
        let sid = "sess-yolo";
        let _session = handle
            .create_session_with_config(sid, None, None, CapabilityMode::All, None, false)
            .expect("create session");
        for (turn, yolo) in [(1, true), (2, true), (3, false)] {
            handle
                .on_before_turn(
                    sid,
                    &BeforeTurnPayload {
                        turn_number: turn,
                        model_id: "grok-4".to_owned(),
                        yolo_mode: yolo,
                        ..Default::default()
                    },
                )
                .await;
        }
        let path = home.path().join("sessions").join(sid).join("events.jsonl");
        let text = std::fs::read_to_string(&path).expect("events.jsonl must exist");
        let toggles: Vec<bool> = text
            .trim()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["type"] == "yolo_toggled")
            .map(|e| e["enabled"].as_bool().unwrap())
            .collect();
        assert_eq!(
            toggles,
            vec![true, false],
            "exactly one toggle per transition (turn 2 repeats true → no re-emit)"
        );
        let turn_yolo: Vec<bool> = text
            .trim()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["type"] == "turn_started")
            .map(|e| e["yolo_mode"].as_bool().unwrap())
            .collect();
        assert_eq!(
            turn_yolo,
            vec![true, true, false],
            "turn_started must carry the per-turn yolo state"
        );
    }
    /// Flag-off preservation: `WorkspaceHandle::new` resolves `events_enabled`
    /// from the (unset) env var, so the whole emission path must stay a noop —
    /// no session writers cached, no `sessions/` dir created.
    #[tokio::test]
    async fn events_disabled_keeps_noop_and_writes_nothing() {
        use xai_file_utils::events::ToolOutcome;
        use xai_tool_protocol::turn_hook::{AfterTurnPayload, BeforeTurnPayload, TurnHookOutcome};
        let handle = make_handle();
        assert!(
            !handle.shared().events_enabled,
            "test precondition: events must be disabled"
        );
        let sid = "main";
        handle
            .on_before_turn(
                sid,
                &BeforeTurnPayload {
                    turn_number: 1,
                    model_id: "grok-4".to_owned(),
                    yolo_mode: false,
                    conversation_message_count: 0,
                    session_relationship: "primary".to_owned(),
                    schema_version: "1.0".to_owned(),
                },
            )
            .await;
        let tracker = handle.activity_tracker();
        tracker.tool_call_started("c1", "read_file", Some(sid));
        tracker.tool_call_completed("c1", Some(sid), ToolOutcome::Success);
        handle.on_yolo_toggled(sid, true);
        handle.on_mcp_server_toggled(sid, "linear", true);
        handle
            .on_after_turn(
                sid,
                &AfterTurnPayload {
                    turn_number: 1,
                    outcome: TurnHookOutcome::Completed,
                    duration_ms: 1,
                    tool_call_count: 1,
                    model_id: "grok-4".to_owned(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                },
            )
            .await;
        assert!(
            handle.shared().session_event_writers.is_empty(),
            "flag-off must not cache any session writer (EventWriter::noop preserved)"
        );
        let sessions_dir = handle.shared().workspace_home().join("sessions");
        assert!(
            !sessions_dir.exists(),
            "flag-off must not create the sessions dir or any events.jsonl"
        );
    }
    /// `on_session_ended` must evict the session's `events.jsonl` writer from the
    /// shared map (releasing the open file descriptor) without losing any events
    /// already written to disk.
    #[tokio::test]
    async fn session_end_evicts_event_writer_without_data_loss() {
        use xai_tool_protocol::turn_hook::BeforeTurnPayload;
        let (handle, home) = make_handle_with_events();
        let sid = "sess-evict";
        handle
            .on_before_turn(
                sid,
                &BeforeTurnPayload {
                    turn_number: 1,
                    model_id: "grok-4".to_owned(),
                    yolo_mode: false,
                    conversation_message_count: 0,
                    session_relationship: "primary".to_owned(),
                    schema_version: "1.0".to_owned(),
                },
            )
            .await;
        assert!(
            handle.shared().session_event_writers.contains_key(sid),
            "writer must be cached after the turn opens it"
        );
        let path = home.path().join("sessions").join(sid).join("events.jsonl");
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(
            before.contains("turn_started"),
            "TurnStarted must be persisted before eviction"
        );
        handle.on_session_ended(sid);
        assert!(
            !handle.shared().session_event_writers.contains_key(sid),
            "writer must be evicted from the map on session end (fd released)"
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "evicting the writer must not lose already-written events"
        );
    }
    /// `on_session_ended` must evict this session's in-flight enqueue handles
    /// (mid-turn deaths would otherwise leak them) without touching other
    /// sessions' entries.
    #[tokio::test]
    async fn session_end_evicts_inflight_enqueues() {
        let handle = make_handle();
        let shared = handle.shared();
        shared.inflight_enqueues.insert(
            ("sess-gone".to_owned(), 1),
            tokio::spawn(async { EnqueueOutcome::Enqueued }),
        );
        shared.inflight_enqueues.insert(
            ("sess-gone".to_owned(), 2),
            tokio::spawn(async { EnqueueOutcome::Enqueued }),
        );
        shared.inflight_enqueues.insert(
            ("sess-stay".to_owned(), 1),
            tokio::spawn(async { EnqueueOutcome::Enqueued }),
        );
        handle.on_session_ended("sess-gone");
        assert!(
            !shared
                .inflight_enqueues
                .contains_key(&("sess-gone".to_owned(), 1)),
            "ending a session must evict its in-flight enqueue handles"
        );
        assert!(
            !shared
                .inflight_enqueues
                .contains_key(&("sess-gone".to_owned(), 2)),
            "every turn of the ending session must be evicted"
        );
        assert!(
            shared
                .inflight_enqueues
                .contains_key(&("sess-stay".to_owned(), 1)),
            "other sessions' in-flight enqueues must be preserved"
        );
    }
    /// `on_session_ended` evicts the session's tool-defs debounce entry (no
    /// per-session leak in a long-lived hub server).
    #[tokio::test]
    async fn session_end_evicts_tool_defs_debounce_entry() {
        let handle = make_handle();
        let sid = "sess-tool-defs-evict";
        assert!(tool_defs_reemit_gate(
            true,
            &handle.shared().tool_defs_last_emit,
            sid,
            std::time::Instant::now(),
            TOOL_DEFS_DEBOUNCE,
        ));
        assert!(
            handle.shared().tool_defs_last_emit.contains_key(sid),
            "debounce entry must be recorded after a gated re-emit"
        );
        handle.on_session_ended(sid);
        assert!(
            !handle.shared().tool_defs_last_emit.contains_key(sid),
            "debounce entry must be evicted on session end (no per-session leak)"
        );
    }
    /// The RPC `drop_session` path evicts the debounce entry like
    /// `on_session_ended` does.
    #[tokio::test]
    async fn drop_session_evicts_tool_defs_debounce_entry() {
        let handle = make_handle();
        let sid = "main";
        assert!(tool_defs_reemit_gate(
            true,
            &handle.shared().tool_defs_last_emit,
            sid,
            std::time::Instant::now(),
            TOOL_DEFS_DEBOUNCE,
        ));
        handle.drop_session(sid, sid).expect("drop main session");
        assert!(
            !handle.shared().tool_defs_last_emit.contains_key(sid),
            "drop_session must evict the debounce entry"
        );
    }
    /// Object-key segment safety: separators, traversal, and NUL are refused.
    #[test]
    fn is_safe_object_segment_rejects_traversal() {
        assert!(is_safe_object_segment("sess-1_a"));
        assert!(!is_safe_object_segment(""));
        assert!(!is_safe_object_segment("a/b"));
        assert!(!is_safe_object_segment("a\\b"));
        assert!(!is_safe_object_segment("../etc"));
        assert!(!is_safe_object_segment("a\0b"));
    }
    /// The single `TurnHookOutcome → TurnOutcomeLabel` mapping used by
    /// `on_after_turn` must be exhaustive and stable.
    #[test]
    fn turn_outcome_label_maps_every_variant() {
        use xai_file_utils::events::TurnOutcomeLabel;
        use xai_tool_protocol::turn_hook::TurnHookOutcome;
        assert!(matches!(
            turn_outcome_label(TurnHookOutcome::Completed),
            TurnOutcomeLabel::Completed
        ));
        assert!(matches!(
            turn_outcome_label(TurnHookOutcome::Cancelled),
            TurnOutcomeLabel::Cancelled
        ));
        assert!(matches!(
            turn_outcome_label(TurnHookOutcome::Error),
            TurnOutcomeLabel::Error
        ));
    }
    pub(crate) fn fork_cfg_with(
        agent_id: &str,
        capability: CapabilityMode,
        tool_config: Option<ToolServerConfig>,
        parent: Option<&str>,
    ) -> AgentSessionConfig {
        let mut c = AgentSessionConfig::new(agent_id);
        c.capability_mode = capability;
        c.tool_config = tool_config;
        c.parent_session_id = parent.map(|p| p.to_owned());
        c
    }
    /// Resolver pointing at a never-listening port; tests assert only on the
    /// synchronous enqueue bookkeeping, never on upload completion.
    struct UnreachableSource;
    impl xai_file_utils::queue::TraceExportSource for UnreachableSource {
        fn resolve(&self) -> xai_file_utils::TraceExportConfig {
            xai_file_utils::TraceExportConfig {
                bucket_url: None,
                service_account_key: None,
                upload_method: xai_file_utils::UploadMethod::Proxy {
                    proxy_base_url: "http://127.0.0.1:1/v1".to_string(),
                    user_token: String::new(),
                    deployment_key: None,
                    alpha_test_key: None,
                },
                prefix_dir: None,
                gcs_prefix: None,
                absolute_paths: false,
                archive_name_override: None,
            }
        }
    }
    /// Upload queue whose worker never deletes an enqueued item mid-test
    /// (1h backoff after the first fast failure).
    fn spawn_test_queue(home: &std::path::Path) -> Arc<xai_file_utils::queue::UploadQueue> {
        let policy = xai_file_utils::queue::UploadRetryPolicy {
            initial_delay: std::time::Duration::from_secs(3600),
            ..Default::default()
        };
        Arc::new(xai_file_utils::queue::UploadQueue::spawn(
            home,
            Arc::new(UnreachableSource),
            policy,
        ))
    }
    /// `WorkspaceHandle::new` (the test/default path, not `connect_local_workspace`)
    /// must use an ephemeral temp `workspace_home` — never the real
    /// `$GROK_WORKSPACE_HOME` — must NOT configure an upload queue, and must leave
    /// the legacy inline-upload path inert (no storage config). This pins the
    /// flag-off defaults so uploads never start implicitly
    /// and `new` stays runtime-light (no queue worker spawned).
    #[tokio::test]
    async fn new_defaults_to_ephemeral_home_and_inert_legacy_upload() {
        let handle = make_handle();
        let shared = handle.shared();
        let home = shared.workspace_home();
        assert!(
            home.starts_with(std::env::temp_dir()),
            "default workspace_home must live under the temp dir, got {}",
            home.display()
        );
        assert_ne!(
            home,
            resolve_workspace_home(),
            "default construction must NOT use the real $GROK_WORKSPACE_HOME"
        );
        assert!(
            shared.upload_queue().is_none(),
            "default construction must not configure an upload queue"
        );
    }
    /// `persist_and_enqueue_tool_state` runs the real save→read→enqueue chain
    /// and the item enters the queue.
    #[tokio::test]
    async fn persist_and_enqueue_tool_state_enqueues_for_session() {
        let handle = make_handle();
        let session = handle.session("main").expect("main session present");
        let queue_home = tempfile::TempDir::new().unwrap();
        let queue = spawn_test_queue(queue_home.path());
        let before = queue
            .stats()
            .enqueued
            .load(std::sync::atomic::Ordering::Relaxed);
        super::persist_and_enqueue_tool_state(session, "main".to_string(), 3, queue.clone())
            .await
            .expect("persist + enqueue must succeed");
        assert_eq!(
            queue
                .stats()
                .enqueued
                .load(std::sync::atomic::Ordering::Relaxed),
            before + 1,
            "the session's tool_state must be flushed, read, and enqueued"
        );
    }
    /// Flag OFF ⇒ `spawn_tool_state_upload` enqueues nothing, even with a live
    /// session and a configured upload queue.
    #[tokio::test]
    async fn tool_state_upload_is_noop_when_flag_off() {
        use crate::session::tool_config::test_support::TestSessionContextFactory;
        let _env = crate::session::tool_config::TOOL_STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("GROK_WORKSPACE_TOOL_STATE_ENABLED") };
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let queue_home = tempfile::TempDir::new().unwrap();
        let queue = spawn_test_queue(queue_home.path());
        let handle = WorkspaceHandle::new_with_data_collection(
            WorkspaceHandle::test_config(cwd, factory),
            queue_home.path().to_path_buf(),
            queue.clone(),
            false,
            false,
            crate::upload::environment::WorkspaceIdentity::default(),
        )
        .expect("queue-backed handle construction");
        handle.create_session("main").expect("create main session");
        let before = queue
            .stats()
            .enqueued
            .load(std::sync::atomic::Ordering::Relaxed);
        handle.spawn_tool_state_upload("main", 1);
        drop(_env);
        tokio::task::yield_now().await;
        assert_eq!(
            queue
                .stats()
                .enqueued
                .load(std::sync::atomic::Ordering::Relaxed),
            before,
            "flag off ⇒ spawn_tool_state_upload must enqueue nothing"
        );
    }
    /// Opt-out (`data_collection_disabled`) ⇒ no tool_state export even
    /// with the feature flag on, a live session, and a configured queue.
    #[tokio::test]
    async fn tool_state_upload_is_noop_when_data_collection_disabled() {
        use crate::session::tool_config::test_support::TestSessionContextFactory;
        let _env = crate::session::tool_config::TOOL_STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GROK_WORKSPACE_TOOL_STATE_ENABLED", "true") };
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let queue_home = tempfile::TempDir::new().unwrap();
        let queue = spawn_test_queue(queue_home.path());
        let handle = WorkspaceHandle::new_with_data_collection(
            WorkspaceHandle::test_config(cwd, factory),
            queue_home.path().to_path_buf(),
            queue.clone(),
            true,
            true,
            Default::default(),
        )
        .expect("queue-backed handle construction");
        handle.create_session("main").expect("create main session");
        let before = queue
            .stats()
            .enqueued
            .load(std::sync::atomic::Ordering::Relaxed);
        handle.spawn_tool_state_upload("main", 1);
        unsafe { std::env::remove_var("GROK_WORKSPACE_TOOL_STATE_ENABLED") };
        drop(_env);
        tokio::task::yield_now().await;
        assert_eq!(
            queue
                .stats()
                .enqueued
                .load(std::sync::atomic::Ordering::Relaxed),
            before,
            "opt-out ⇒ spawn_tool_state_upload must enqueue nothing"
        );
    }
    /// Queue-backed handle with an explicit `identity` and a
    /// `{sandbox_id, mode}` server-metadata blob; the returned `TempDir` must
    /// outlive the handle. The proxy points at a dead local port. Collection
    /// is enabled (not opted out).
    fn make_queue_backed_handle(
        identity: crate::WorkspaceIdentity,
    ) -> (WorkspaceHandle, tempfile::TempDir) {
        make_queue_backed_handle_with(identity, false)
    }
    /// [`make_queue_backed_handle`] with an explicit opt-out
    /// verdict so gating tests can exercise the suppression path.
    fn make_queue_backed_handle_with(
        identity: crate::WorkspaceIdentity,
        data_collection_disabled: bool,
    ) -> (WorkspaceHandle, tempfile::TempDir) {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: Some(serde_json::json!({
                "sandbox_id": "sb_test123",
                "mode": "remote",
            })),
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        let home = tempfile::tempdir().expect("workspace home tempdir");
        let auth: xai_computer_hub_sdk::SharedAuthProvider = Arc::new(
            xai_computer_hub_sdk::auth::AuthCredential::bearer("test-token"),
        );
        let proxy = Arc::new(crate::upload::ProxyStorageConfig::new(
            auth,
            "http://127.0.0.1:1/v1".to_string(),
            identity.clone(),
        ));
        let source: Arc<dyn xai_file_utils::queue::TraceExportSource> =
            Arc::new(crate::upload::WorkspaceTraceExportSource::new(proxy));
        let policy = xai_file_utils::queue::UploadRetryPolicy {
            max_attempts: 1,
            ..Default::default()
        };
        let queue = Arc::new(xai_file_utils::queue::UploadQueue::spawn(
            home.path(),
            source,
            policy,
        ));
        let handle = WorkspaceHandle::new_with_data_collection(
            config,
            home.path().to_path_buf(),
            queue,
            true,
            data_collection_disabled,
            identity,
        )
        .expect("queue-backed handle construction");
        (handle, home)
    }
    fn enqueued_count(handle: &WorkspaceHandle) -> u64 {
        handle
            .shared
            .upload_queue()
            .expect("queue present")
            .stats()
            .enqueued
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Accessors expose the threaded identity and parse the metadata blob.
    #[tokio::test]
    async fn shared_accessors_expose_identity_and_sandbox_id() {
        let identity = crate::WorkspaceIdentity::new(
            "user-7",
            Some("Team".to_string()),
            Some("team-7".to_string()),
        );
        let (handle, _home) = make_queue_backed_handle(identity);
        let shared = handle.shared();
        assert_eq!(shared.identity().user_id, "user-7");
        assert!(shared.identity().is_team());
        assert_eq!(shared.identity().team_id().as_deref(), Some("team-7"));
        assert!(shared.auth_provider().is_none());
        assert_eq!(
            shared.server_metadata_typed().sandbox_id.as_deref(),
            Some("sb_test123")
        );
        assert_eq!(shared.server_id(), None);
    }
    /// `server_metadata_typed` defaults cleanly when no metadata is configured.
    #[tokio::test]
    async fn server_metadata_typed_defaults_without_metadata() {
        let handle = make_handle();
        assert_eq!(handle.shared().server_metadata_typed().sandbox_id, None);
    }
    /// With a queue present, the environment artifact is enqueued
    /// (`enqueued` is bumped synchronously, so the assertion is race-free).
    #[tokio::test]
    async fn environment_artifact_enqueued_when_queue_present() {
        let identity = crate::WorkspaceIdentity::new("user-7", Some("User".to_string()), None);
        let (handle, _home) = make_queue_backed_handle(identity);
        assert_eq!(enqueued_count(&handle), 0);
        let outcome = handle
            .emit_environment_artifact("sess-env", std::path::Path::new("/work"), None)
            .await;
        assert!(
            matches!(
                outcome,
                Some(xai_file_utils::queue::EnqueueOutcome::Enqueued)
            ),
            "expected Enqueued, got {outcome:?}"
        );
        assert_eq!(
            enqueued_count(&handle),
            1,
            "the environment artifact must reach the queue"
        );
    }
    /// Without a queue (tests / local mode) emission is a silent no-op.
    #[tokio::test]
    async fn environment_artifact_noop_without_queue() {
        let handle = make_handle();
        assert!(handle.shared.upload_queue().is_none());
        let outcome = handle
            .emit_environment_artifact("sess-env", std::path::Path::new("/work"), None)
            .await;
        assert!(outcome.is_none(), "no queue ⇒ no enqueue");
    }
    /// End-to-end with a real queue: emission is unconditional (no env flag),
    /// so a bound session enqueues exactly one environment artifact and
    /// registers a producer task.
    #[tokio::test]
    async fn maybe_emit_environment_enqueues_with_queue() {
        let identity = crate::WorkspaceIdentity::new("user-7", None, None);
        let (handle, _home) = make_queue_backed_handle(identity);
        assert_eq!(enqueued_count(&handle), 0);
        handle.maybe_emit_environment("sess-on", std::path::Path::new("/work"));
        assert_eq!(
            handle.shared.producer_tasks.len(),
            1,
            "environment emission must register in the producer tracker"
        );
        for _ in 0..200 {
            if enqueued_count(&handle) >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            enqueued_count(&handle),
            1,
            "emission must enqueue the environment artifact"
        );
    }
    /// Opt-out suppresses emission: no producer is
    /// spawned and nothing reaches the queue. This is the real suppression
    /// condition that survived the removal of the env-flag gate.
    #[tokio::test]
    async fn maybe_emit_environment_suppressed_under_zdr() {
        let identity = crate::WorkspaceIdentity::new("user-7", None, None);
        let (handle, _home) = make_queue_backed_handle_with(identity, true);
        assert_eq!(enqueued_count(&handle), 0);
        handle.maybe_emit_environment("sess-off", std::path::Path::new("/work"));
        assert_eq!(
            handle.shared.producer_tasks.len(),
            0,
            "opt-out must not spawn an environment producer"
        );
        tokio::task::yield_now().await;
        assert_eq!(
            enqueued_count(&handle),
            0,
            "opt-out must not enqueue the environment artifact"
        );
    }
    #[tokio::test]
    async fn fork_session_inherits_parent_tool_config_when_none() {
        let handle = make_handle();
        let parent = handle.session("main").expect("main session present");
        let parent_baseline = parent.effective_tool_config();
        let parent_ids: Vec<String> = parent_baseline.tools.iter().map(|t| t.id.clone()).collect();
        let child = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        let child_baseline = child.effective_tool_config();
        let child_ids: Vec<String> = child_baseline.tools.iter().map(|t| t.id.clone()).collect();
        assert_eq!(child_ids, parent_ids);
        let new_parent_baseline = ToolServerConfig {
            tools: vec![tc("GrokBuild:read_file", Some(ToolKind::Read))],
            behavior_preset: None,
        };
        let factory = handle.shared.session_factory.clone();
        let mcp_snapshot = handle.shared.mcp_tools_snapshot.load_full();
        let hub_snapshot = handle.shared.hub_tools_snapshot.load_full();
        let (eff, ts, _backend) = resolve_session_toolset(
            new_parent_baseline,
            parent.capability_mode(),
            &mcp_snapshot,
            &hub_snapshot,
            parent.cwd().to_path_buf(),
            parent.session_env().clone(),
            "main",
            factory.as_ref(),
            None,
            None,
            None,
            None,
        )
        .expect("re-resolve should succeed");
        parent.replace(Arc::new(eff), ts);
        let child_after: Vec<String> = child
            .effective_tool_config()
            .tools
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(
            child_after, child_ids,
            "child baseline must not change when parent is mutated"
        );
    }
    #[tokio::test]
    async fn fork_session_uses_explicit_tool_config_when_provided() {
        let handle = make_handle();
        let custom = ToolServerConfig {
            tools: vec![
                tc("GrokBuild:read_file", Some(ToolKind::Read)),
                tc("GrokBuild:list_dir", Some(ToolKind::ListDir)),
            ],
            behavior_preset: None,
        };
        let child = handle
            .fork_session(fork_cfg_with(
                "explicit",
                CapabilityMode::ReadWrite,
                Some(custom.clone()),
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        let baseline_ids: Vec<String> = child
            .effective_tool_config()
            .tools
            .iter()
            .map(|t| t.id.clone())
            .collect();
        let custom_ids: Vec<String> = custom.tools.iter().map(|t| t.id.clone()).collect();
        assert_eq!(baseline_ids, custom_ids);
    }
    #[tokio::test]
    async fn fork_session_uses_main_session_when_parent_session_id_is_none() {
        let handle = make_handle();
        let marker_config = ToolServerConfig {
            tools: vec![tc("GrokBuild:read_file", Some(ToolKind::Read))],
            behavior_preset: None,
        };
        let main = handle.session("main").expect("main present");
        let factory = handle.shared.session_factory.clone();
        let mcp_snapshot = handle.shared.mcp_tools_snapshot.load_full();
        let hub_snapshot = handle.shared.hub_tools_snapshot.load_full();
        let (eff, ts, _backend) = resolve_session_toolset(
            marker_config,
            main.capability_mode(),
            &mcp_snapshot,
            &hub_snapshot,
            main.cwd().to_path_buf(),
            main.session_env().clone(),
            "main",
            factory.as_ref(),
            None,
            None,
            None,
            None,
        )
        .expect("re-resolve should succeed");
        main.replace(Arc::new(eff), ts);
        let child = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        let baseline_ids: Vec<String> = child
            .effective_tool_config()
            .tools
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(baseline_ids, vec!["GrokBuild:read_file".to_string()]);
    }
    #[tokio::test]
    async fn fork_session_uses_named_parent_when_parent_session_id_is_set() {
        let handle = make_handle();
        let custom = ToolServerConfig {
            tools: vec![tc("GrokBuild:read_file", Some(ToolKind::Read))],
            behavior_preset: None,
        };
        handle
            .fork_session(fork_cfg_with(
                "intermediate",
                CapabilityMode::ReadWrite,
                Some(custom.clone()),
                Some("main"),
            ))
            .await
            .expect("intermediate fork should succeed");
        let leaf = handle
            .fork_session(fork_cfg_with(
                "leaf",
                CapabilityMode::ReadWrite,
                None,
                Some("intermediate"),
            ))
            .await
            .expect("leaf fork should succeed");
        let baseline_ids: Vec<String> = leaf
            .effective_tool_config()
            .tools
            .iter()
            .map(|t| t.id.clone())
            .collect();
        let custom_ids: Vec<String> = custom.tools.iter().map(|t| t.id.clone()).collect();
        assert_eq!(baseline_ids, custom_ids);
    }
    #[test]
    fn fork_session_concurrent_same_id_only_one_winner() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(8)
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();
        let handle = Arc::new(make_handle());
        let mut handles = vec![];
        for _ in 0..16 {
            let h = handle.clone();
            let g = rt.handle().clone();
            handles.push(std::thread::spawn(move || {
                g.block_on(h.fork_session({
                    let mut c = AgentSessionConfig::new("racer");
                    c.parent_session_id = Some("main".into());
                    c
                }))
            }));
        }
        let mut wins = 0;
        let mut losses = 0;
        for jh in handles {
            let res = jh.join().expect("thread panic");
            match res {
                Ok(_) => wins += 1,
                Err(WorkspaceError::SessionAlreadyExists(id)) => {
                    assert_eq!(id, "racer");
                    losses += 1;
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!(wins, 1, "exactly one fork must succeed");
        assert_eq!(losses, 15, "the other 15 must see SessionAlreadyExists");
    }
    #[tokio::test]
    async fn fork_session_empty_agent_id_rejected() {
        let handle = make_handle();
        let err = handle
            .fork_session({
                let mut c = AgentSessionConfig::new("");
                c.parent_session_id = Some("main".into());
                c
            })
            .await
            .expect_err("empty agent_id must error");
        assert!(matches!(err, WorkspaceError::EmptyAgentId), "got {err:?}");
    }
    #[tokio::test]
    async fn fork_session_capability_widening_rejected() {
        let handle = make_handle();
        handle
            .fork_session(fork_cfg_with(
                "ro",
                CapabilityMode::ReadOnly,
                None,
                Some("main"),
            ))
            .await
            .expect("readonly fork ok");
        let err = handle
            .fork_session(fork_cfg_with(
                "widen",
                CapabilityMode::All,
                None,
                Some("ro"),
            ))
            .await
            .expect_err("widening must error");
        assert!(
            matches!(
                err,
                WorkspaceError::CapabilityWidening {
                    parent: CapabilityMode::ReadOnly,
                    child: CapabilityMode::All
                }
            ),
            "got {err:?}"
        );
    }
    /// A fork that races a terminal drain must be rejected by the same
    /// shutdown gate as `create_session`, so it can't repopulate the session
    /// map while the shared upload queue is being flushed/closed.
    #[tokio::test]
    async fn fork_session_rejected_while_draining() {
        let handle = make_handle();
        handle.activity_tracker().set_draining();
        let err = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect_err("fork must be rejected while draining");
        assert!(matches!(err, WorkspaceError::ShuttingDown), "got {err:?}");
    }
    #[tokio::test]
    async fn fork_session_capability_widening_readwrite_to_execute_rejected() {
        let handle = make_handle();
        handle
            .fork_session(fork_cfg_with(
                "rw",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("rw fork ok");
        let err = handle
            .fork_session(fork_cfg_with(
                "exe",
                CapabilityMode::Execute,
                None,
                Some("rw"),
            ))
            .await
            .expect_err("incomparable widen must error");
        assert!(matches!(err, WorkspaceError::CapabilityWidening { .. }));
    }
    #[tokio::test]
    async fn fork_session_max_depth_rejected_when_budget_zero() {
        let handle = make_handle();
        let mut cfg = AgentSessionConfig::new("budgeted");
        cfg.parent_session_id = Some("main".into());
        cfg.max_depth = 0;
        let child = handle.fork_session(cfg).await.expect("budgeted fork ok");
        assert_eq!(child.fork_budget(), 0);
        let err = handle
            .fork_session(fork_cfg_with(
                "grandchild",
                CapabilityMode::ReadWrite,
                None,
                Some("budgeted"),
            ))
            .await
            .expect_err("further fork must error");
        assert!(matches!(err, WorkspaceError::MaxDepthExceeded { .. }));
    }
    #[tokio::test]
    async fn fork_session_parent_session_not_found_errors() {
        let handle = make_handle();
        let mut cfg = AgentSessionConfig::new("orphan");
        cfg.parent_session_id = Some("ghost".into());
        let err = handle
            .fork_session(cfg)
            .await
            .expect_err("missing parent must error");
        match err {
            WorkspaceError::ParentSessionNotFound(id) => assert_eq!(id, "ghost"),
            other => panic!("unexpected: {other:?}"),
        }
    }
    #[tokio::test]
    async fn fork_session_finalize_error_propagated() {
        let handle = make_handle();
        let bad = ToolServerConfig {
            tools: vec![tc("DoesNotExist:nope", Some(ToolKind::Read))],
            behavior_preset: None,
        };
        let cfg = fork_cfg_with("bogus", CapabilityMode::ReadOnly, Some(bad), Some("main"));
        let err = handle
            .fork_session(cfg)
            .await
            .expect_err("bogus id must error");
        assert!(matches!(err, WorkspaceError::Finalize(_)), "got {err:?}");
    }
    #[tokio::test]
    async fn fork_session_extra_env_layered_on_parent() {
        let handle = make_handle();
        let mut intermediate_cfg = AgentSessionConfig::new("parent_env");
        intermediate_cfg
            .extra_env
            .insert("INHERITED".into(), "from_parent".into());
        intermediate_cfg
            .extra_env
            .insert("OVERRIDDEN".into(), "old_value".into());
        intermediate_cfg.parent_session_id = Some("main".into());
        let parent = handle
            .fork_session(intermediate_cfg)
            .await
            .expect("parent ok");
        assert_eq!(
            parent.session_env().get("INHERITED").map(String::as_str),
            Some("from_parent")
        );
        let mut child_cfg = AgentSessionConfig::new("child_env");
        child_cfg.parent_session_id = Some("parent_env".into());
        child_cfg
            .extra_env
            .insert("OVERRIDDEN".into(), "new_value".into());
        child_cfg
            .extra_env
            .insert("CHILD_ONLY".into(), "yes".into());
        let child = handle.fork_session(child_cfg).await.expect("child ok");
        assert_eq!(
            child.session_env().get("INHERITED").map(String::as_str),
            Some("from_parent"),
            "parent var must be inherited"
        );
        assert_eq!(
            child.session_env().get("OVERRIDDEN").map(String::as_str),
            Some("new_value"),
            "extra_env must override parent var"
        );
        assert_eq!(
            child.session_env().get("CHILD_ONLY").map(String::as_str),
            Some("yes"),
            "extra_env must add new var"
        );
    }
    #[tokio::test]
    async fn fork_session_cwd_override_used_when_set() {
        let handle = make_handle();
        let alt = std::env::temp_dir().join("xai-grok-workspace-test-cwd-override");
        std::fs::create_dir_all(&alt).expect("create alt cwd");
        let mut cfg = AgentSessionConfig::new("cwdchild");
        cfg.cwd_override = Some(alt.clone());
        cfg.parent_session_id = Some("main".into());
        let child = handle.fork_session(cfg).await.expect("ok");
        assert_eq!(child.cwd(), alt);
    }
    #[tokio::test]
    async fn fork_session_inheritance_arc_distinct() {
        let handle = make_handle();
        let main = handle.session("main").expect("main");
        let child = handle
            .fork_session({
                let mut c = AgentSessionConfig::new("kid");
                c.parent_session_id = Some("main".into());
                c
            })
            .await
            .expect("ok");
        assert!(
            !Arc::ptr_eq(
                &main.effective_tool_config(),
                &child.effective_tool_config()
            ),
            "child must hold its own Arc<ToolServerConfig>"
        );
        assert!(
            !Arc::ptr_eq(&main.toolset(), &child.toolset()),
            "child must hold its own Arc<FinalizedToolset>"
        );
    }
    #[tokio::test]
    async fn fork_session_empty_baseline_tools_succeeds() {
        let handle = make_handle();
        let empty = ToolServerConfig {
            tools: vec![],
            behavior_preset: None,
        };
        let child = handle
            .fork_session(fork_cfg_with(
                "empty",
                CapabilityMode::ReadOnly,
                Some(empty),
                Some("main"),
            ))
            .await
            .expect("empty tool set is valid");
        assert!(child.toolset().tool_definitions().is_empty());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_mcp_snapshot_changed_emits_per_session_events_and_rebuilds() {
        let handle = make_handle();
        handle
            .fork_session(fork_cfg_with(
                "subA",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("subA ok");
        handle
            .fork_session(fork_cfg_with(
                "subB",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("subB ok");
        let mut rx = handle.shared.events.subscribe();
        let mcp_tool = tc("GrokBuild:read_file", Some(ToolKind::Read));
        let rebuilt = handle.on_mcp_snapshot_changed(vec![mcp_tool]);
        assert_eq!(rebuilt, 3, "main + 2 subagents");
        let mut got: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for _ in 0..3 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("event arrives")
                .expect("not closed");
            match ev {
                WorkspaceEvent::ToolsChanged { session_id } => {
                    got.insert(session_id);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(
            got,
            ["main".to_string(), "subA".to_string(), "subB".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<String>>()
        );
    }
    #[tokio::test]
    async fn shared_accessors_round_trip() {
        let handle = make_handle();
        assert!(handle.shared().root_cwd().to_str().is_some());
        assert!(!handle.shared().respect_gitignore());
        assert!(handle.shared().memory_config().is_none());
        assert!(handle.shared().mcp_tools_snapshot().is_empty());
        assert!(!handle.shared().default_tool_config().tools.is_empty());
    }
    #[tokio::test]
    async fn hook_registry_empty_when_no_sources() {
        let handle = make_handle();
        let registry = handle.hook_registry();
        assert!(registry.is_empty(), "no sources => empty registry");
        assert!(
            handle.hook_load_errors().is_empty(),
            "no sources => no errors"
        );
    }
    #[tokio::test]
    async fn hook_registry_loads_from_settings_file() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let settings_path = cwd.join("claude_settings.json");
        std::fs::write(
            &settings_path,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo ok"}]}]}}"#,
        )
        .expect("write settings");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![HookSourceConfig::SettingsFile(settings_path)],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("ok");
        let registry = handle.hook_registry();
        assert!(!registry.is_empty(), "settings file should yield hooks");
        assert!(handle.hook_load_errors().is_empty());
    }
    #[tokio::test]
    async fn hook_registry_loads_from_directory() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let hooks_dir = cwd.join("hooks");
        std::fs::create_dir_all(&hooks_dir).expect("mkdir");
        std::fs::write(
            hooks_dir.join("my_hook.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        )
        .expect("write hook file");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![HookSourceConfig::Directory(hooks_dir)],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("ok");
        let registry = handle.hook_registry();
        assert!(!registry.is_empty(), "directory source should yield hooks");
    }
    #[tokio::test]
    async fn hook_registry_snapshot_is_disconnected() {
        let handle = make_handle();
        let snap1 = handle.hook_registry();
        assert!(snap1.is_empty());
        {
            let spec = xai_grok_hooks::config::HookSpec {
                name: "injected".into(),
                event: xai_grok_hooks::event::HookEventName::SessionStart,
                handler_type: xai_grok_hooks::config::HandlerType::Command,
                configured_matcher: None,
                matcher: None,
                enabled: true,
                command: Some("echo injected".into()),
                command_raw: Some("echo injected".into()),
                url: None,
                url_raw: None,
                timeout_ms: 10_000,
                source_dir: std::path::PathBuf::from("/tmp"),
                extra_env: std::collections::HashMap::new(),
            };
            handle.shared.hook_registry.write().append_specs(vec![spec]);
        }
        assert!(snap1.is_empty(), "snapshot must not see live mutations");
        let snap2 = handle.hook_registry();
        assert!(!snap2.is_empty(), "fresh snapshot must see mutation");
    }
    #[tokio::test]
    async fn hook_load_errors_reported_for_bad_file() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let bad_path = cwd.join("bad_settings.json");
        std::fs::write(&bad_path, "NOT VALID JSON").expect("write bad file");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![HookSourceConfig::SettingsFile(bad_path)],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("construction must still succeed");
        assert!(
            !handle.hook_load_errors().is_empty(),
            "bad JSON must produce load errors"
        );
    }
    #[tokio::test]
    async fn hook_registry_global_and_project_sources_merge() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let global_settings = cwd.join("global.json");
        std::fs::write(
                &global_settings,
                r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo global"}]}]}}"#,
            )
            .expect("write");
        let project_settings = cwd.join("project.json");
        std::fs::write(
            &project_settings,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo project"}]}]}}"#,
        )
        .expect("write");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![HookSourceConfig::SettingsFile(global_settings)],
            hook_project_sources: vec![HookSourceConfig::SettingsFile(project_settings)],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("ok");
        let registry = handle.hook_registry();
        assert_eq!(registry.len(), 2, "both sources must contribute hooks");
    }
    #[tokio::test]
    async fn hook_registry_missing_source_is_non_fatal() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let missing = cwd.join("does_not_exist.json");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![HookSourceConfig::SettingsFile(missing)],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("must not panic on missing source");
        assert!(handle.hook_registry().is_empty());
        assert!(
            handle.hook_load_errors().is_empty(),
            "missing file should not produce errors"
        );
    }
    #[tokio::test]
    async fn hook_registry_empty_directory_yields_empty_registry() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let empty_dir = cwd.join("empty_hooks");
        std::fs::create_dir_all(&empty_dir).expect("mkdir");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![HookSourceConfig::Directory(empty_dir)],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("ok");
        assert!(handle.hook_registry().is_empty());
        assert!(handle.hook_load_errors().is_empty());
    }
    #[tokio::test]
    async fn hub_tools_snapshot_starts_empty() {
        let handle = make_handle();
        assert!(handle.shared().hub_tools_snapshot().is_empty());
        assert!(handle.shared().hub_server().is_none());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_hub_tools_changed_emits_per_session_events() {
        let handle = make_handle();
        handle
            .fork_session(fork_cfg_with(
                "hubA",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("hubA ok");
        let mut rx = handle.shared.events.subscribe();
        let hub_tool = tc("hub:remote_exec", None);
        let rebuilt = handle.on_hub_tools_changed(vec![hub_tool]);
        assert_eq!(rebuilt, 2, "main + 1 subagent");
        let mut got: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for _ in 0..2 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("event arrives")
                .expect("not closed");
            match ev {
                WorkspaceEvent::ToolsChanged { session_id } => {
                    got.insert(session_id);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(
            got,
            ["main".to_string(), "hubA".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<String>>()
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_hub_tools_changed_updates_snapshot() {
        let handle = make_handle();
        assert!(handle.shared().hub_tools_snapshot().is_empty());
        let hub_tool = tc("hub:remote_exec", None);
        handle.on_hub_tools_changed(vec![hub_tool]);
        let snapshot = handle.shared().hub_tools_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, "hub:remote_exec");
    }
    #[tokio::test]
    async fn connect_hub_noop_when_no_config() {
        let handle = make_handle();
        let result = handle.connect_hub().await;
        assert!(result.is_ok());
        assert!(handle.shared().hub_server().is_none());
    }
    #[test]
    fn workspace_shared_auth_provider_uses_workspace_config() {
        let temp = tempfile::tempdir().unwrap();
        let service_auth: xai_computer_hub_sdk::SharedAuthProvider = Arc::new(
            xai_computer_hub_sdk::auth::AuthCredential::bearer("xai-service-token"),
        );
        let hub_auth: xai_computer_hub_sdk::SharedAuthProvider = Arc::new(
            xai_computer_hub_sdk::auth::AuthCredential::bearer("hub-token"),
        );
        let hub_cfg = crate::hub::HubConfig {
            url: url::Url::parse("ws://127.0.0.1:9/ws").unwrap(),
            auth: hub_auth.clone(),
            activity_tracker: None,
            server_id: Some("server-1".to_string()),
            alpha_test_key: None,
            allow_insecure_ws: true,
            diag: None,
        };
        let config = WorkspaceConfig::new_for_proxy(
            temp.path().to_path_buf(),
            Arc::new(TestSessionContextFactory::new()),
            hub_cfg,
            service_auth.clone(),
            None,
            Default::default(),
            baseline_config(),
        );
        let handle = WorkspaceHandle::build(
            config,
            ephemeral_workspace_home(),
            None,
            true,
            false,
            false,
            false,
            false,
            crate::upload::environment::WorkspaceIdentity::default(),
        )
        .expect("handle construction should succeed");
        let shared_auth = handle
            .shared()
            .auth_provider()
            .expect("WorkspaceConfig auth provider must populate WorkspaceShared");
        assert_eq!(shared_auth.current(), service_auth.current());
        assert_ne!(shared_auth.current(), hub_auth.current());
    }
    #[tokio::test]
    async fn shutdown_hub_noop_when_not_connected() {
        let handle = make_handle();
        handle.shutdown_hub().await;
        assert!(handle.shared().hub_server().is_none());
    }
    #[tokio::test]
    async fn codebase_index_forwarder_abort_releases_shared() {
        let handle = make_handle();
        tokio::task::yield_now().await;
        let before = Arc::strong_count(handle.shared());
        let task = handle.spawn_codebase_index_event_forwarder();
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        assert!(Arc::strong_count(handle.shared()) > before);
        task.abort();
        let _ = task.await;
        assert_eq!(
            Arc::strong_count(handle.shared()),
            before,
            "abort must drop the forwarder's WorkspaceShared ref"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_normal() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let resolved = handle
            .resolve_service_path("src/main.rs", &canonical_root)
            .await
            .expect("normal path should resolve");
        assert_eq!(resolved, root.join("src/main.rs"));
    }
    #[tokio::test]
    async fn resolve_service_path_rejects_empty() {
        let handle = make_handle();
        let canonical_root = handle.canonical_root().await.unwrap();
        let err = handle
            .resolve_service_path("", &canonical_root)
            .await
            .expect_err("empty path must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("empty path"),
            "error should mention empty path: {msg}"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_rejects_absolute_outside_root() {
        let handle = make_handle();
        let canonical_root = handle.canonical_root().await.unwrap();
        let err = handle
            .resolve_service_path("/etc/passwd", &canonical_root)
            .await
            .expect_err("absolute path outside root must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("escapes workspace root"),
            "error should mention escape: {msg}"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_accepts_absolute_within_root() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let rel = handle
            .resolve_service_path("src/main.rs", &canonical_root)
            .await
            .expect("relative path should resolve");
        let abs_input = root.join("src/main.rs");
        let abs = handle
            .resolve_service_path(abs_input.to_str().expect("utf-8 path"), &canonical_root)
            .await
            .expect("absolute path within root should resolve");
        assert_eq!(abs, rel);
    }
    #[tokio::test]
    async fn resolve_service_path_rejects_escape() {
        let handle = make_handle();
        let canonical_root = handle.canonical_root().await.unwrap();
        let err = handle
            .resolve_service_path("../../etc/passwd", &canonical_root)
            .await
            .expect_err("escape path must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("path escapes workspace root"),
            "error should mention escape: {msg}"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_allows_dotdot_within_root() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let resolved = handle
            .resolve_service_path("src/../lib.rs", &canonical_root)
            .await
            .expect("dotdot within root should resolve");
        assert_eq!(resolved, root.join("lib.rs"));
    }
    #[tokio::test]
    async fn resolve_service_path_rejects_symlink_escape() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let outside = tempfile::tempdir().expect("create outside dir");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "top secret").expect("write secret");
        let link_path = root.join("escape_link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link_path).expect("create symlink");
        #[cfg(not(unix))]
        {
            return;
        }
        let err = handle
            .resolve_service_path("escape_link/secret.txt", &canonical_root)
            .await
            .expect_err("symlink escape must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("symlink escape"),
            "error should mention symlink escape: {msg}"
        );
    }
    /// A *dangling* leaf symlink (target missing, outside root) must be rejected:
    /// `canonicalize` fails NotFound, so the leaf is resolved via `read_link`.
    #[tokio::test]
    #[cfg(unix)]
    async fn resolve_service_path_rejects_dangling_symlink_escape() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let outside = tempfile::tempdir().expect("create outside dir");
        std::os::unix::fs::symlink(outside.path().join("new.txt"), root.join("lnk"))
            .expect("create symlink");
        let err = handle
            .resolve_service_path("lnk", &canonical_root)
            .await
            .expect_err("dangling symlink escape must be rejected");
        assert!(
            format!("{err}").contains("symlink escape"),
            "error should mention symlink escape: {err}"
        );
    }
    /// A multi-hop chain of dangling in-root links ending outside the root must
    /// be followed and rejected (not fall through the ancestor walk).
    #[tokio::test]
    #[cfg(unix)]
    async fn resolve_service_path_rejects_dangling_symlink_chain() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let outside = tempfile::tempdir().expect("outside");
        for i in 0..3 {
            std::os::unix::fs::symlink(
                root.join(format!("lnk{}", i + 1)),
                root.join(format!("lnk{i}")),
            )
            .expect("chain link");
        }
        std::os::unix::fs::symlink(outside.path().join("x"), root.join("lnk3")).expect("tail link");
        let err = handle
            .resolve_service_path("lnk0", &canonical_root)
            .await
            .expect_err("dangling symlink chain escaping root must be rejected");
        assert!(
            format!("{err}").contains("symlink escape")
                || format!("{err}").contains("unresolved symlink chain"),
            "unexpected error: {err}"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_nested_subdir() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let resolved = handle
            .resolve_service_path("a/b/c/d.txt", &canonical_root)
            .await
            .expect("deeply nested path should resolve");
        assert_eq!(resolved, root.join("a/b/c/d.txt"));
    }
    #[tokio::test]
    async fn resolve_service_path_dot_current_dir() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let resolved = handle
            .resolve_service_path("./src/./main.rs", &canonical_root)
            .await
            .expect("dot segments should be stripped");
        assert_eq!(resolved, root.join("src/main.rs"));
    }
    #[tokio::test]
    async fn confine_to_root_accepts_path_within_alternative_root() {
        let handle = make_confining_handle();
        let alt = tempfile::tempdir().expect("create alt root");
        let alt_root = alt.path().to_path_buf();
        let target = alt_root.join("src/foo.rs");
        let (confined, _canonical) = handle
            .confine_to_root(&target, &alt_root)
            .await
            .expect("path within the alternative root should resolve");
        assert_eq!(confined, target);
        handle
            .confine_to_workspace_root(&target)
            .await
            .expect_err("path outside the workspace root must be rejected");
    }
    #[tokio::test]
    async fn confine_to_root_rejects_dotdot_escape() {
        let handle = make_confining_handle();
        let alt = tempfile::tempdir().expect("create alt root");
        let err = handle
            .confine_to_root(std::path::Path::new("../../etc/passwd"), alt.path())
            .await
            .expect_err("dotdot escape from the alternative root must be rejected");
        assert!(
            format!("{err}").contains("path escapes workspace root"),
            "error should mention escape: {err}"
        );
    }
    #[tokio::test]
    async fn confine_to_root_rejects_absolute_path_outside_root() {
        let handle = make_confining_handle();
        let alt = tempfile::tempdir().expect("create alt root");
        let err = handle
            .confine_to_root(std::path::Path::new("/etc/passwd"), alt.path())
            .await
            .expect_err("absolute path outside the alternative root must be rejected");
        assert!(
            format!("{err}").contains("escapes workspace root"),
            "error should mention escape: {err}"
        );
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn confine_to_root_rejects_symlink_escape() {
        let handle = make_confining_handle();
        let alt = tempfile::tempdir().expect("create alt root");
        let outside = tempfile::tempdir().expect("create outside dir");
        std::fs::write(outside.path().join("secret.txt"), "top secret").expect("write secret");
        std::os::unix::fs::symlink(outside.path(), alt.path().join("escape_link"))
            .expect("create symlink");
        let err = handle
            .confine_to_root(&alt.path().join("escape_link/secret.txt"), alt.path())
            .await
            .expect_err("symlink escaping the alternative root must be rejected");
        assert!(
            format!("{err}").contains("symlink escape"),
            "error should mention symlink escape: {err}"
        );
    }
    /// Off by default: an out-of-root absolute path is passed through, not rejected.
    #[tokio::test]
    async fn confine_to_workspace_root_unconfined_by_default_allows_escape() {
        let handle = make_handle();
        let outside = tempfile::tempdir().expect("create outside dir");
        let target = outside.path().join("secret.txt");
        let (resolved, walk_root) = handle
            .confine_to_workspace_root(&target)
            .await
            .expect("unconfined resolution must not reject an outside path");
        assert_eq!(resolved, target, "path is passed through unchanged");
        assert!(
            walk_root.is_none(),
            "no confining walk root when confinement is off"
        );
    }
    /// Off by default: a symlink escaping the root is followed, not rejected.
    #[tokio::test]
    #[cfg(unix)]
    async fn confine_to_workspace_root_unconfined_by_default_follows_symlink() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let outside = tempfile::tempdir().expect("create outside dir");
        std::fs::write(outside.path().join("secret.txt"), "ok").expect("write secret");
        std::os::unix::fs::symlink(outside.path(), root.join("escape_link"))
            .expect("create symlink");
        let link_path = root.join("escape_link/secret.txt");
        let (resolved, walk_root) = handle
            .confine_to_workspace_root(&link_path)
            .await
            .expect("unconfined resolution must follow a symlink out of the root");
        assert_eq!(resolved, link_path);
        assert!(walk_root.is_none());
    }
    #[tokio::test]
    async fn per_session_hunk_tracker_isolation() {
        let handle = make_handle();
        let child = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        child.hunk_tracker().record_agent_write(
            std::path::PathBuf::from("/tmp/test-file.rs"),
            "fn main() {}".to_string(),
            0,
            None,
        );
        let child_hunks = child.hunk_tracker().get_all_hunks().await;
        assert!(
            !child_hunks.is_empty(),
            "child session should have tracked hunks"
        );
        let main = handle.session("main").expect("main session present");
        let main_hunks = main.hunk_tracker().get_all_hunks().await;
        assert!(
            main_hunks.is_empty(),
            "main session hunk tracker must be isolated from child: got {} hunks",
            main_hunks.len()
        );
    }
    #[tokio::test]
    async fn cancel_tool_call_marks_call_completed() {
        let handle = make_handle();
        let tracker = handle.activity_tracker();
        tracker.tool_call_started("call-1", "read_file", Some("main"));
        assert_eq!(tracker.snapshot().active_tool_calls, 1);
        handle.cancel_tool_call("main", "call-1");
        assert_eq!(
            tracker.snapshot().active_tool_calls,
            0,
            "cancel_tool_call should mark the call as completed"
        );
    }
    #[tokio::test]
    async fn cancel_tool_call_unknown_id_is_noop() {
        let handle = make_handle();
        handle.cancel_tool_call("main", "never-started");
        assert_eq!(handle.activity_tracker().snapshot().active_tool_calls, 0);
    }
    #[tokio::test]
    async fn on_session_ended_clears_turn_active() {
        let handle = make_handle();
        let tracker = handle.activity_tracker();
        tracker.turn_started("main", 1);
        assert!(tracker.is_turn_active("main"));
        handle.on_session_ended("main");
        assert!(
            !tracker.is_turn_active("main"),
            "on_session_ended should clear turn_active"
        );
    }
    #[tokio::test]
    async fn on_session_ended_unknown_session_is_noop() {
        let handle = make_handle();
        let tracker = handle.activity_tracker();
        let sessions_before = tracker.known_sessions();
        handle.on_session_ended("nonexistent");
        assert_eq!(
            tracker.known_sessions(),
            sessions_before,
            "on_session_ended must not create a new session entry"
        );
    }
    #[tokio::test]
    async fn fork_session_inherits_viewer_ctx_from_parent() {
        let handle = make_handle();
        handle.drop_session("main", "main").expect("drop main");
        let parent = handle
            .create_session_with_tracker_and_viewer_ctx(
                "main",
                handle.root_cwd().unwrap(),
                xai_hunk_tracker::HunkTrackerHandle::noop(),
                None,
                CapabilityMode::All,
                Some(xai_tool_runtime::WorkspaceViewerContext {
                    stream_tool_progress: true,
                }),
                false,
            )
            .expect("create parent");
        assert!(parent.viewer_ctx().is_some());
        let child = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        let inherited = child.viewer_ctx().expect("child inherits viewer_ctx");
        assert!(
            inherited.stream_tool_progress,
            "child must inherit the parent's stream_tool_progress flag"
        );
    }
    /// Build the resolver exactly the way `connect_hub` does: session catalog
    /// handlers + the workspace RPC handler.
    fn bind_resolver_fixture(
        handle: &WorkspaceHandle,
    ) -> xai_computer_hub_sdk::SessionHandlerResolver {
        let catalog_toolset = handle.session("main").expect("main session").toolset();
        let mut catalog = build_session_routed_handlers(&catalog_toolset, handle);
        let rpc_handler: Arc<dyn xai_computer_hub_sdk::ToolServerHandler> =
            Arc::new(crate::hub_server::WorkspaceRpcHandler::new(handle.clone()));
        let rpc_tool_id = rpc_handler.tool_id();
        catalog.push(rpc_handler);
        handle.session_bind_resolver(Arc::new(catalog), rpc_tool_id)
    }
    fn handler_names(resolved: &xai_computer_hub_sdk::ResolvedSessionHandlers) -> Vec<String> {
        resolved
            .handlers
            .iter()
            .map(|h| h.tool_id().as_str().to_owned())
            .collect()
    }
    /// Strict mode, preset-only bind: the full resolver path fails closed —
    /// RPC-only advertise + a `missing_tool_config` reason in the bind report.
    #[tokio::test]
    async fn strict_bind_without_explicit_toolset_fails_closed_end_to_end() {
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let resolved = resolver(
            xai_tool_protocol::SessionId::new("bind-e2e-strict").unwrap(),
            Some(serde_json::json!({
                "metadata": {"preset": "grok-computer", "capability_mode": "all"},
            })),
        )
        .await
        .expect("bind must succeed");
        assert_eq!(
            handler_names(&resolved),
            vec![crate::hub_ids::WORKSPACE_RPC_TOOL_ID.to_owned()],
            "must advertise the RPC handler only"
        );
        let reason = resolved.resolve_error.expect("resolve_error must be set");
        assert!(
            reason.starts_with("missing_tool_config:"),
            "reason must name the fail-closed cause: {reason}"
        );
        assert!(
            reason.contains(xai_grok_version::VERSION),
            "reason must carry the server version: {reason}"
        );
    }
    #[tokio::test]
    async fn strict_rpc_only_bind_fails_closed_with_resolve_error_end_to_end() {
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let resolved = resolver(
            xai_tool_protocol::SessionId::new("bind-e2e-rpc-only").unwrap(),
            Some(serde_json::json!({
                "metadata": {
                    "capability_mode": "read_write",
                    "rpc_only": true,
                    "system_notifications": true,
                },
            })),
        )
        .await
        .expect("bind must succeed");
        assert_eq!(
            handler_names(&resolved),
            vec![crate::hub_ids::WORKSPACE_RPC_TOOL_ID.to_owned()],
        );
        let reason = resolved.resolve_error.expect("resolve_error must be set");
        assert!(reason.starts_with("missing_tool_config:"), "{reason}");
    }
    /// Strict mode, explicit `tools`: resolves and advertises the configured
    /// tool with no resolve_error.
    #[tokio::test]
    async fn strict_bind_with_explicit_toolset_serves_it_end_to_end() {
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let resolved = resolver(
            xai_tool_protocol::SessionId::new("bind-e2e-tools").unwrap(),
            Some(serde_json::json!({
                "metadata": {"tools": [{"id": "GrokBuild:read_file"}]},
            })),
        )
        .await
        .expect("bind must succeed");
        let names = handler_names(&resolved);
        assert!(
            names.iter().any(|n| n == "read_file"),
            "configured tool must be advertised: {names:?}"
        );
        assert_eq!(resolved.resolve_error, None);
        assert!(resolved.unserved_tool_ids.is_empty());
    }
    /// Lax mode (CLI/local embedders), metadata-less bind: falls back to the
    /// default catalog with no resolve_error.
    #[tokio::test]
    async fn lax_bind_without_metadata_uses_default_catalog_end_to_end() {
        let handle = make_handle();
        let resolver = bind_resolver_fixture(&handle);
        let resolved = resolver(
            xai_tool_protocol::SessionId::new("bind-e2e-lax").unwrap(),
            None,
        )
        .await
        .expect("bind must succeed");
        let names = handler_names(&resolved);
        assert!(
            names.iter().any(|n| n == "read_file") && names.iter().any(|n| n == "grep"),
            "default catalog must be advertised: {names:?}"
        );
        assert_eq!(resolved.resolve_error, None);
    }
    /// A rebind whose explicit config is REJECTED (invalid entry) keeps the
    /// fail-closed reason even though the healthy session's previous toolset
    /// is reused — the client must learn its new config did not take effect.
    #[tokio::test]
    async fn rejected_rebind_config_keeps_resolve_error_end_to_end() {
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let sid = xai_tool_protocol::SessionId::new("bind-e2e-rejected").unwrap();
        let first = resolver(
            sid.clone(),
            Some(serde_json::json!({
                "metadata": {"tools": [{"id": "GrokBuild:read_file"}]},
            })),
        )
        .await
        .expect("healthy bind");
        assert_eq!(first.resolve_error, None);
        let second = resolver(
            sid,
            Some(serde_json::json!({
                "metadata": {"tools": [{"id": "GrokBuild:read_file", "params_json": "{not json"}]},
            })),
        )
        .await
        .expect("rejected rebind still advertises the previous toolset");
        assert!(
            handler_names(&second).iter().any(|n| n == "read_file"),
            "previous toolset must still be served"
        );
        let reason = second
            .resolve_error
            .expect("rejected config must keep the fail-closed reason");
        assert!(reason.starts_with("invalid_tool_config:"), "{reason}");
    }
    /// An explicit EMPTY toolset (RPC-only clients, e.g. deploy binds) must
    /// reuse an existing session unchanged — never swap its tools away.
    #[tokio::test]
    async fn explicit_empty_toolset_rebind_never_swaps_session_tools() {
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let sid = xai_tool_protocol::SessionId::new("bind-e2e-rpc-only").unwrap();
        let first = resolver(
            sid.clone(),
            Some(serde_json::json!({
                "metadata": {"tools": [{"id": "GrokBuild:read_file"}]},
            })),
        )
        .await
        .expect("agent bind");
        assert!(handler_names(&first).iter().any(|n| n == "read_file"));
        let rpc_bind = resolver(
            sid,
            Some(serde_json::json!({
                "metadata": {"tool_config": {"tools": []}},
            })),
        )
        .await
        .expect("rpc-only rebind");
        assert!(
            handler_names(&rpc_bind).iter().any(|n| n == "read_file"),
            "agent session tools must survive an RPC-only rebind"
        );
        assert_eq!(rpc_bind.resolve_error, None);
    }
    /// Rebind heal end-to-end: a strict fail-closed bind leaves the session
    /// empty; a corrected rebind with explicit tools rebuilds and advertises
    /// them with the report cleared.
    #[tokio::test]
    async fn strict_rebind_with_corrected_toolset_heals_end_to_end() {
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let sid = xai_tool_protocol::SessionId::new("bind-e2e-heal").unwrap();
        let first = resolver(
            sid.clone(),
            Some(serde_json::json!({"metadata": {"preset": "grok-computer"}})),
        )
        .await
        .expect("fail-closed bind still succeeds with an RPC-only advertise");
        assert!(first.resolve_error.is_some(), "first bind must fail closed");
        let second = resolver(
            sid,
            Some(serde_json::json!({
                "metadata": {"tools": [{"id": "GrokBuild:read_file"}]},
            })),
        )
        .await
        .expect("bind must succeed");
        let names = handler_names(&second);
        assert!(
            names.iter().any(|n| n == "read_file"),
            "corrected rebind must advertise the explicit toolset: {names:?}"
        );
        assert_eq!(
            second.resolve_error, None,
            "healed rebind must not carry the stale fail-closed reason"
        );
    }
    /// Owner bind: capability `all` + explicit toolset (strict servers fail
    /// closed otherwise).
    fn owner_full_bind_metadata() -> serde_json::Value {
        serde_json::json!({
            "metadata": {
                "capability_mode": "all",
                "tools": [
                    {"id": "GrokBuild:read_file"},
                    {"id": "GrokBuild:search_replace"},
                    {"id": "GrokBuild:grep"},
                    {"id": "GrokBuild:list_dir"},
                ],
            },
        })
    }
    const OWNER_TOOLS: [&str; 4] = ["read_file", "search_replace", "grep", "list_dir"];
    #[track_caller]
    fn assert_advertises_owner_tools(names: &[String], context: &str) {
        for tool in OWNER_TOOLS {
            assert!(
                names.iter().any(|n| n == tool),
                "{context}: owner tool `{tool}` missing from advertised set {names:?}"
            );
        }
    }
    /// Consumer-shaped rebinds against a live owner session must `Reuse` it
    /// unchanged — never shrink its toolset or narrow its frozen capability.
    #[tokio::test]
    async fn owner_toolset_survives_concurrent_consumer_shaped_rebinds() {
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let sid = xai_tool_protocol::SessionId::new("bind-e2e-consumer-storm").unwrap();
        let owner = resolver(sid.clone(), Some(owner_full_bind_metadata()))
            .await
            .expect("owner bind");
        assert_advertises_owner_tools(&handler_names(&owner), "owner bind");
        assert_eq!(owner.resolve_error, None);
        let consumer_shapes: Vec<Option<serde_json::Value>> = vec![
            Some(serde_json::json!({"metadata": {"capability_mode": "read_only"}})),
            Some(serde_json::json!({"metadata": {"capability_mode": "read_write"}})),
            None,
            Some(serde_json::json!({"metadata": {"tool_config": {"tools": []}}})),
        ];
        let storm = futures::future::join_all(
            consumer_shapes
                .iter()
                .cycle()
                .take(12)
                .cloned()
                .map(|metadata| resolver(sid.clone(), metadata)),
        )
        .await;
        for (i, result) in storm.into_iter().enumerate() {
            let resolved = result.expect("consumer-shaped rebind must not error");
            assert_advertises_owner_tools(
                &handler_names(&resolved),
                &format!("consumer-shaped rebind #{i}"),
            );
            assert_eq!(
                resolved.resolve_error, None,
                "reuse against a healthy owner session must not surface a resolve error"
            );
        }
        let session = handle
            .session("bind-e2e-consumer-storm")
            .expect("owner session survives the storm");
        assert_eq!(
            session.capability_mode(),
            CapabilityMode::All,
            "consumer-shaped rebinds must never narrow the owner's frozen capability"
        );
        assert_advertises_owner_tools(
            &session
                .toolset()
                .tool_definitions()
                .into_iter()
                .map(|d| d.function.name)
                .collect::<Vec<_>>(),
            "post-storm session toolset",
        );
    }
    /// On a fresh workspace-server the FIRST bind freezes `capability_mode`: consumer-shaped
    /// first binds strand the session narrow (why consumers never bind); owner-first is whole.
    #[tokio::test]
    async fn restored_server_first_bind_ordering_decides_capability_and_toolset() {
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let sid = xai_tool_protocol::SessionId::new("bind-e2e-restore-read-first").unwrap();
        let read_first = resolver(
            sid.clone(),
            Some(serde_json::json!({"metadata": {"capability_mode": "read_only"}})),
        )
        .await
        .expect("consumer-shaped bind resolves");
        assert_eq!(
            handler_names(&read_first),
            vec![crate::hub_ids::WORKSPACE_RPC_TOOL_ID.to_owned()],
            "strict fail-closed create advertises the RPC handler only"
        );
        let agent = resolver(sid, Some(owner_full_bind_metadata()))
            .await
            .expect("agent bind resolves");
        let names = handler_names(&agent);
        assert!(
            names.iter().any(|n| n == "read_file"),
            "agent bind heals the read-class toolset: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "search_replace"),
            "frozen read_only capability keeps filtering Edit-class tools — \
             the incident's shrunken toolset: {names:?}"
        );
        let session = handle
            .session("bind-e2e-restore-read-first")
            .expect("session exists");
        assert_eq!(
            session.capability_mode(),
            CapabilityMode::ReadOnly,
            "the consumer-shaped first bind froze the capability for good"
        );
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let sid = xai_tool_protocol::SessionId::new("bind-e2e-restore-write-first").unwrap();
        resolver(
            sid.clone(),
            Some(serde_json::json!({"metadata": {"capability_mode": "read_write"}})),
        )
        .await
        .expect("consumer-shaped bind resolves");
        resolver(sid, Some(owner_full_bind_metadata()))
            .await
            .expect("agent bind resolves");
        let session = handle
            .session("bind-e2e-restore-write-first")
            .expect("session exists");
        assert_eq!(
            session.capability_mode(),
            CapabilityMode::ReadWrite,
            "the agent's `all` must not take on a session a deploy/write-shaped \
             bind created first — this narrower freeze is why deploy and fs \
             writes are consumers now"
        );
        let handle = make_strict_handle();
        let resolver = bind_resolver_fixture(&handle);
        let sid = xai_tool_protocol::SessionId::new("bind-e2e-restore-owner-first").unwrap();
        let owner = resolver(sid, Some(owner_full_bind_metadata()))
            .await
            .expect("owner bind resolves");
        assert_advertises_owner_tools(&handler_names(&owner), "owner-first bind");
        assert_eq!(owner.resolve_error, None);
        let session = handle
            .session("bind-e2e-restore-owner-first")
            .expect("session exists");
        assert_eq!(
            session.capability_mode(),
            CapabilityMode::All,
            "owner-first ordering yields the full capability the agent declared"
        );
    }
    /// Isolation matrix #1–#3 through the REAL `session.bind` resolver (the
    /// closure `connect_hub` installs — the exact path both a soft rebind and
    /// an SDK dead-loop FULL rebind re-run): with a live background task,
    /// an identical rebind (`Reused`) and a changed-explicit-toolset rebind
    /// (`Reresolved`, driven with no in-flight tool calls) both keep the
    /// session-owned backend (`Arc::ptr_eq`) and the running task, while the
    /// changed rebind swaps the advertised handler set.
    ///
    /// The remaining matrix-#3 sub-asserts live beside the swap tests above:
    /// persistent-shell cwd preservation
    /// (`reresolved_swap_preserves_persistent_shell_cwd`) and the
    /// snapshot-driven rebuild with a live task
    /// (`re_resolve_all_sessions_preserves_session_terminal_backend`).
    #[tokio::test]
    async fn bind_flow_rebinds_keep_backend_and_task_alive_end_to_end() {
        let orphaned_before = orphaned_swap_count();
        let handle = make_handle();
        let resolver = bind_resolver_fixture(&handle);
        let sid = xai_tool_protocol::SessionId::new("bind-e2e-bg").unwrap();
        let bg_metadata = serde_json::json!({
            "metadata": {"tools": [
                {"id": "GrokBuild:read_file"},
                {"id": "GrokBuild:run_terminal_cmd"},
                {"id": "GrokBuild:get_task_output"},
                {"id": "GrokBuild:kill_task"},
            ]},
        });
        let first = resolver(sid.clone(), Some(bg_metadata.clone()))
            .await
            .expect("owner bind");
        assert!(
            handler_names(&first)
                .iter()
                .any(|n| n == "run_terminal_cmd"),
            "owner bind must serve the execute tool"
        );
        let session = handle.session("bind-e2e-bg").expect("session created");
        let backend = session.terminal_backend().clone();
        let out_dir = tempfile::tempdir().expect("temp dir");
        let bg = start_background_sleep(&session, out_dir.path(), "bind-e2e-bg-task").await;
        let reused = resolver(sid.clone(), Some(bg_metadata))
            .await
            .expect("identical rebind");
        assert!(
            handler_names(&reused)
                .iter()
                .any(|n| n == "run_terminal_cmd"),
            "a reused rebind keeps advertising the existing toolset"
        );
        let session = handle.session("bind-e2e-bg").expect("session kept");
        assert!(
            Arc::ptr_eq(&backend, session.terminal_backend()),
            "an identical rebind must keep the session-owned backend"
        );
        assert!(
            !backend
                .get_task(&bg.task_id)
                .await
                .expect("task listed across the reused rebind")
                .completed,
            "the task must still be running after the reused rebind"
        );
        let swapped = resolver(
            sid,
            Some(serde_json::json!({
                "metadata": {"tools": [{"id": "GrokBuild:read_file"}]},
            })),
        )
        .await
        .expect("changed-toolset rebind");
        let names = handler_names(&swapped);
        assert!(
            names.iter().any(|n| n == "read_file")
                && !names.iter().any(|n| n == "run_terminal_cmd"),
            "the changed rebind must advertise the NEW toolset only: {names:?}"
        );
        let session = handle.session("bind-e2e-bg").expect("session kept");
        assert!(
            Arc::ptr_eq(&backend, session.terminal_backend()),
            "a toolset-swapping rebind must keep the session-owned backend"
        );
        assert!(
            Arc::ptr_eq(&backend, &toolset_terminal(&session.toolset()).await),
            "the swapped-in toolset must reference the session-owned backend"
        );
        assert!(
            !backend
                .get_task(&bg.task_id)
                .await
                .expect("task table must survive the toolset swap")
                .completed,
            "the task's process must still be running after the swap"
        );
        assert_eq!(
            orphaned_swap_count(),
            orphaned_before,
            "the orphaned-backend tripwire must stay 0"
        );
        backend.kill_task(&bg.task_id).await;
    }
    /// Dropping and rebinding a session with the same ID surfaces the
    /// new `viewer_ctx` (kill-switch for mid-session staleness).
    #[tokio::test]
    async fn drop_then_rebind_session_replaces_viewer_ctx_value() {
        let handle = make_handle();
        handle.drop_session("main", "main").expect("drop main");
        let s1 = handle
            .create_session_with_tracker_and_viewer_ctx(
                "main",
                handle.root_cwd().unwrap(),
                xai_hunk_tracker::HunkTrackerHandle::noop(),
                None,
                CapabilityMode::All,
                Some(xai_tool_runtime::WorkspaceViewerContext {
                    stream_tool_progress: true,
                }),
                false,
            )
            .expect("first bind");
        assert_eq!(s1.viewer_ctx().map(|c| c.stream_tool_progress), Some(true));
        handle.drop_session("main", "main").expect("drop");
        let s2 = handle
            .create_session_with_tracker_and_viewer_ctx(
                "main",
                handle.root_cwd().unwrap(),
                xai_hunk_tracker::HunkTrackerHandle::noop(),
                None,
                CapabilityMode::All,
                Some(xai_tool_runtime::WorkspaceViewerContext {
                    stream_tool_progress: false,
                }),
                false,
            )
            .expect("second bind");
        assert_eq!(
            s2.viewer_ctx().map(|c| c.stream_tool_progress),
            Some(false),
            "rebind must surface the new viewer_ctx value"
        );
    }
    fn enq() -> EnqueueOutcome {
        EnqueueOutcome::Enqueued
    }
    fn inline() -> EnqueueOutcome {
        EnqueueOutcome::FellBackToInline
    }
    fn failed(reason: &str) -> EnqueueOutcome {
        EnqueueOutcome::Failed {
            reason: reason.to_owned(),
        }
    }
    /// Both archives durably enqueued → `Enqueued`, `artifact_count == 2`.
    #[test]
    fn reduce_outcomes_both_enqueued() {
        let (status, count, msg) = reduce_enqueue_outcomes(&enq(), &enq());
        assert_eq!(status, AfterTurnAckStatus::Enqueued);
        assert_eq!(count, 2);
        assert_eq!(msg, None);
    }
    /// A single failure makes the whole ack `Failed` and carries the reason,
    /// while still counting the durable sibling toward `artifact_count`.
    #[test]
    fn reduce_outcomes_one_failed_one_enqueued() {
        let (status, count, msg) = reduce_enqueue_outcomes(&enq(), &failed("disk full"));
        assert_eq!(status, AfterTurnAckStatus::Failed);
        assert_eq!(count, 1, "the durable before-archive still counts");
        assert_eq!(msg.as_deref(), Some("disk full"));
    }
    /// The FIRST failure reason wins when both phases fail.
    #[test]
    fn reduce_outcomes_both_failed_reports_first_reason() {
        let (status, count, msg) =
            reduce_enqueue_outcomes(&failed("before boom"), &failed("after boom"));
        assert_eq!(status, AfterTurnAckStatus::Failed);
        assert_eq!(count, 0);
        assert_eq!(msg.as_deref(), Some("before boom"));
    }
    /// Inline fallback is a success for the status but is NOT on the durable
    /// spill, so it does not add to `artifact_count`.
    #[test]
    fn reduce_outcomes_inline_fallback_counts_as_success_not_durable() {
        let (status, count, msg) = reduce_enqueue_outcomes(&enq(), &inline());
        assert_eq!(status, AfterTurnAckStatus::Enqueued);
        assert_eq!(
            count, 1,
            "inline fallback is not durably on the queue spill"
        );
        assert_eq!(msg, None);
        let (status, count, _) = reduce_enqueue_outcomes(&inline(), &inline());
        assert_eq!(status, AfterTurnAckStatus::Enqueued);
        assert_eq!(count, 0);
    }
    /// No durable-queue handles at all (queue disabled / not proxy) → `Skipped`.
    #[tokio::test]
    async fn resolve_ack_skipped_when_no_handles() {
        let (status, count, msg) = resolve_after_turn_ack(
            None,
            None,
            std::time::Duration::from_secs(5),
            "no_upload_queue",
        )
        .await;
        assert_eq!(status, AfterTurnAckStatus::Skipped);
        assert_eq!(count, 0);
        assert_eq!(msg.as_deref(), Some("no_upload_queue"));
        let (status, count, msg) = resolve_after_turn_ack(
            None,
            None,
            std::time::Duration::from_secs(5),
            "data_collection_disabled",
        )
        .await;
        assert_eq!(status, AfterTurnAckStatus::Skipped);
        assert_eq!(count, 0);
        assert_eq!(msg.as_deref(), Some("data_collection_disabled"));
    }
    /// Two real enqueue tasks that both report `Enqueued` resolve to a clean
    /// `Enqueued` ack with `artifact_count == 2`.
    #[tokio::test]
    async fn resolve_ack_awaits_real_handles() {
        let before = tokio::spawn(async { EnqueueOutcome::Enqueued });
        let after = tokio::spawn(async { EnqueueOutcome::Enqueued });
        let (status, count, msg) = resolve_after_turn_ack(
            Some(before),
            Some(after),
            std::time::Duration::from_secs(5),
            "no_upload_queue",
        )
        .await;
        assert_eq!(status, AfterTurnAckStatus::Enqueued);
        assert_eq!(count, 2);
        assert_eq!(msg, None);
    }
    /// A before-turn enqueue that outlives the watchdog is reported as
    /// `Failed { "watchdog_timeout" }` WITHOUT blocking the ack on the slow task.
    #[tokio::test]
    async fn resolve_ack_watchdog_trips_on_slow_before() {
        let before = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            EnqueueOutcome::Enqueued
        });
        let after = tokio::spawn(async { EnqueueOutcome::Enqueued });
        let start = std::time::Instant::now();
        let (status, count, msg) = resolve_after_turn_ack(
            Some(before),
            Some(after),
            std::time::Duration::from_millis(50),
            "no_upload_queue",
        )
        .await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "watchdog must not block the ack on the slow before-turn task"
        );
        assert_eq!(status, AfterTurnAckStatus::Failed);
        assert_eq!(count, 1, "only the after archive landed durably");
        assert_eq!(msg.as_deref(), Some("watchdog_timeout"));
    }
    /// `await_enqueue_outcome(None, ..)` maps a missing handle to a truthful
    /// `Failed` (not a panic / not a silent success).
    #[tokio::test]
    async fn await_missing_handle_is_failed() {
        let outcome =
            await_enqueue_outcome(None, std::time::Duration::from_secs(1), "before_enqueue").await;
        assert!(matches!(outcome, EnqueueOutcome::Failed { .. }));
    }
    /// The hand-written decode `match` must not drift from the enum's
    /// serde snake_case forms.
    #[test]
    fn session_relationship_wire_forms_round_trip() {
        for variant in [SessionRelationship::Primary, SessionRelationship::Subagent] {
            let wire = serde_json::to_value(variant).unwrap();
            let wire = wire.as_str().unwrap();
            let decoded = decode_session_relationship(wire);
            assert_eq!(
                serde_json::to_value(decoded).unwrap().as_str(),
                Some(wire),
                "{variant:?} must round-trip through decode_session_relationship"
            );
        }
        assert!(matches!(
            decode_session_relationship("nonsense"),
            SessionRelationship::Primary
        ));
    }
    /// The workspace decodes the bare snake_case `cancellation_category` string
    /// back into the enum; unknown / absent values decode to `None`.
    #[test]
    fn cancellation_category_decode_round_trips() {
        assert_eq!(
            decode_cancellation_category(Some("hook_denied")),
            Some(CancellationCategory::HookDenied),
        );
        assert_eq!(
            decode_cancellation_category(Some("permission_rejected")),
            Some(CancellationCategory::PermissionRejected),
        );
        assert_eq!(decode_cancellation_category(Some("not_a_category")), None);
        assert_eq!(decode_cancellation_category(None), None);
    }
    /// Without a durable upload queue (tests / local mode) a before turn
    /// produces no enqueue handle, so nothing is registered in
    /// `inflight_enqueues` and the ack machinery has nothing to await.
    #[tokio::test]
    async fn no_upload_queue_registers_no_inflight_enqueue() {
        use xai_tool_protocol::turn_hook::BeforeTurnPayload;
        let handle = make_handle();
        handle
            .on_before_turn(
                "main",
                &BeforeTurnPayload {
                    turn_number: 1,
                    model_id: "grok-4".to_owned(),
                    yolo_mode: false,
                    conversation_message_count: 0,
                    session_relationship: "primary".to_owned(),
                    schema_version: "1.0".to_owned(),
                },
            )
            .await;
        assert!(
            handle.shared().inflight_enqueues.is_empty(),
            "queue-less mode must not store any inflight before-turn enqueue handle"
        );
    }
    /// The request/response `After` turn hook performs the turn-end work and
    /// returns the ack on the reply: queue-less mode is a truthful `Skipped`
    /// with the `no_upload_queue` diagnostic, and a stored inflight before-turn
    /// entry is evicted by the turn-end path.
    #[tokio::test]
    async fn compute_turn_injections_after_returns_skipped_ack_without_queue() {
        use xai_tool_protocol::turn_hook::{AfterTurnPayload, TurnHookOutcome, TurnHookRequest};
        let handle = make_handle();
        handle.shared().inflight_enqueues.insert(
            ("main".to_owned(), 3),
            tokio::spawn(async { EnqueueOutcome::Enqueued }),
        );
        let reply = handle
            .compute_turn_injections(
                "main",
                &TurnHookRequest::After(AfterTurnPayload {
                    turn_number: 3,
                    outcome: TurnHookOutcome::Completed,
                    duration_ms: 10,
                    tool_call_count: 0,
                    model_id: "grok-4".to_owned(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                }),
            )
            .await;
        let ack = reply
            .after_turn_ack
            .expect("After reply must carry the ack");
        assert_eq!(ack.turn_number, 3);
        assert_eq!(ack.status, AfterTurnAckStatus::Failed);
        assert_eq!(ack.artifact_count, 1);
        assert!(
            handle
                .shared()
                .inflight_enqueues
                .get(&("main".to_owned(), 3))
                .is_none(),
            "the After path must evict the inflight before-turn entry"
        );
        assert!(reply.injections.is_empty());
        let reply = handle
            .compute_turn_injections(
                "main",
                &TurnHookRequest::After(AfterTurnPayload {
                    turn_number: 4,
                    outcome: TurnHookOutcome::Completed,
                    duration_ms: 10,
                    tool_call_count: 0,
                    model_id: "grok-4".to_owned(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                }),
            )
            .await;
        let ack = reply
            .after_turn_ack
            .expect("After reply must carry the ack");
        assert_eq!(ack.status, AfterTurnAckStatus::Skipped);
        assert_eq!(ack.error_message.as_deref(), Some("no_upload_queue"));
    }
    /// A `Before` request answers with a no-op reply (no ack) while driving
    /// the same turn-start work as the fire-and-forget hook — the request
    /// channel is the only turn signal the server-side sampler sends.
    #[tokio::test]
    async fn compute_turn_injections_before_runs_turn_start_and_replies_noop() {
        use xai_tool_protocol::turn_hook::{BeforeTurnPayload, HookReply, TurnHookRequest};
        let handle = make_handle();
        let reply = handle
            .compute_turn_injections(
                "main",
                &TurnHookRequest::Before(BeforeTurnPayload {
                    turn_number: 9,
                    ..BeforeTurnPayload::default()
                }),
            )
            .await;
        assert_eq!(reply, HookReply::default());
        assert!(
            handle
                .activity_tracker()
                .known_sessions()
                .iter()
                .any(|s| s == "main"),
            "Before request must drive on_before_turn (activity tracking)"
        );
    }
    /// The extended after-turn cancellation pair is decoded into the
    /// `TurnEnded` line: the category string becomes the enum's snake_case form
    /// and the context object passes through verbatim.
    #[tokio::test]
    async fn after_turn_decodes_cancellation_fields_into_events_jsonl() {
        use xai_tool_protocol::turn_hook::{AfterTurnPayload, BeforeTurnPayload, TurnHookOutcome};
        let (handle, home) = make_handle_with_events();
        let sid = "sess-cancel";
        handle
            .on_before_turn(
                sid,
                &BeforeTurnPayload {
                    turn_number: 2,
                    model_id: "grok-4".to_owned(),
                    yolo_mode: false,
                    conversation_message_count: 0,
                    session_relationship: "primary".to_owned(),
                    schema_version: "1.0".to_owned(),
                },
            )
            .await;
        handle
            .on_after_turn(
                sid,
                &AfterTurnPayload {
                    turn_number: 2,
                    outcome: TurnHookOutcome::Cancelled,
                    duration_ms: 10,
                    tool_call_count: 0,
                    model_id: "grok-4".to_owned(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: Some("permission_rejected".to_owned()),
                    cancellation_context: Some(serde_json::json!({ "recovery": false })),
                },
            )
            .await;
        let path = home.path().join("sessions").join(sid).join("events.jsonl");
        let text = std::fs::read_to_string(&path).expect("events.jsonl must exist");
        let ended = text
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|e| e["type"] == "turn_ended")
            .expect("turn_ended must be present");
        assert_eq!(ended["outcome"], "cancelled");
        assert_eq!(ended["cancellation_category"], "permission_rejected");
        assert_eq!(
            ended["cancellation_context"],
            serde_json::json!({ "recovery": false })
        );
    }
    /// The default watchdog must undercut the requester's 10s hook timeout.
    #[test]
    fn after_turn_watchdog_default_is_8s() {
        assert_eq!(after_turn_watchdog(), std::time::Duration::from_secs(8));
    }
    fn bundled_dir_fixture(subdirs: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        for name in subdirs {
            std::fs::create_dir(tmp.path().join(name)).expect("create subdir");
        }
        std::fs::write(tmp.path().join("BUILD.bazel"), b"").expect("create file");
        tmp
    }
    #[test]
    fn bundled_allowlist_blank_ignores_nothing() {
        let tmp = bundled_dir_fixture(&["bundled__pdf", "bundled__xlsx"]);
        let dir = tmp.path().to_string_lossy().into_owned();
        for allowlist in [None, Some(""), Some("  "), Some(" , ,")] {
            assert_eq!(
                bundled_allowlist_ignore_dirs(&dir, allowlist),
                Vec::<String>::new(),
                "allowlist {allowlist:?} must produce no ignore entries"
            );
        }
    }
    #[test]
    fn workspace_tool_definitions_path_is_session_root() {
        assert_eq!(
            workspace_tool_definitions_path("sess-1"),
            "sess-1/workspace_tool_definitions.json"
        );
    }
    #[test]
    fn tool_defs_reemit_gate_flag_off_never_emits_and_records_nothing() {
        let map = dashmap::DashMap::new();
        let now = std::time::Instant::now();
        assert!(!tool_defs_reemit_gate(
            false,
            &map,
            "s",
            now,
            TOOL_DEFS_DEBOUNCE
        ));
        assert!(
            map.is_empty(),
            "flag-off must not record any debounce state (legacy path stays inert)"
        );
        assert!(tool_defs_reemit_gate(
            true,
            &map,
            "s",
            now,
            TOOL_DEFS_DEBOUNCE
        ));
    }
    #[test]
    fn tool_defs_reemit_gate_debounces_within_5s_window() {
        let map = dashmap::DashMap::new();
        let window = std::time::Duration::from_secs(5);
        let t0 = std::time::Instant::now();
        assert!(tool_defs_reemit_gate(true, &map, "s", t0, window));
        assert!(!tool_defs_reemit_gate(
            true,
            &map,
            "s",
            t0 + std::time::Duration::from_secs(1),
            window
        ));
        assert!(!tool_defs_reemit_gate(
            true,
            &map,
            "s",
            t0 + std::time::Duration::from_millis(4_999),
            window
        ));
        assert!(tool_defs_reemit_gate(
            true,
            &map,
            "s",
            t0 + std::time::Duration::from_secs(5),
            window
        ));
        assert!(!tool_defs_reemit_gate(
            true,
            &map,
            "s",
            t0 + std::time::Duration::from_secs(6),
            window
        ));
    }
    #[test]
    fn tool_defs_reemit_gate_is_per_session() {
        let map = dashmap::DashMap::new();
        let now = std::time::Instant::now();
        assert!(tool_defs_reemit_gate(
            true,
            &map,
            "a",
            now,
            TOOL_DEFS_DEBOUNCE
        ));
        assert!(tool_defs_reemit_gate(
            true,
            &map,
            "b",
            now,
            TOOL_DEFS_DEBOUNCE
        ));
        assert!(!tool_defs_reemit_gate(
            true,
            &map,
            "a",
            now,
            TOOL_DEFS_DEBOUNCE
        ));
    }
    #[tokio::test]
    async fn workspace_tool_definitions_payload_matches_chat_completions_shape() {
        let handle = make_handle();
        let (path, bytes) = handle
            .workspace_tool_definitions_payload("main")
            .expect("payload for an existing session");
        assert_eq!(path, "main/workspace_tool_definitions.json");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        let arr = parsed.as_array().expect("a JSON array of tool definitions");
        assert!(!arr.is_empty(), "baseline session must expose tools");
        for def in arr {
            assert_eq!(
                def["type"], "function",
                "tool def must be type=function: {def}"
            );
            let function = &def["function"];
            assert!(
                function["name"].as_str().is_some_and(|n| !n.is_empty()),
                "function.name must be a non-empty string: {def}"
            );
            assert!(
                function["parameters"].is_object(),
                "function.parameters must be a JSON object: {def}"
            );
            let keys: std::collections::BTreeSet<&str> = function
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert!(
                keys.is_subset(&["name", "description", "parameters"].into_iter().collect()),
                "unexpected function keys {keys:?}"
            );
        }
        let names: std::collections::BTreeSet<&str> = arr
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect();
        for expected in ["read_file", "search_replace", "grep", "list_dir"] {
            assert!(
                names.contains(expected),
                "missing baseline tool {expected}: {names:?}"
            );
        }
    }
    #[test]
    fn bundled_allowlist_ignores_complement() {
        let tmp = bundled_dir_fixture(&["bundled__pdf", "bundled__xlsx", "bundled__docx"]);
        let dir = tmp.path().to_string_lossy().into_owned();
        let got = bundled_allowlist_ignore_dirs(&dir, Some("xlsx, pdf"));
        let want = vec![
            tmp.path()
                .join("bundled__docx")
                .to_string_lossy()
                .into_owned(),
        ];
        assert_eq!(got, want);
    }
    #[test]
    fn bundled_allowlist_strips_bundled_prefix() {
        let tmp = bundled_dir_fixture(&["bundled__pdf", "xlsx", "bundled__skip"]);
        let dir = tmp.path().to_string_lossy().into_owned();
        let got = bundled_allowlist_ignore_dirs(&dir, Some("bundled__pdf,bundled__xlsx"));
        let want = vec![
            tmp.path()
                .join("bundled__skip")
                .to_string_lossy()
                .into_owned(),
        ];
        assert_eq!(got, want);
    }
    #[test]
    fn bundled_allowlist_unreadable_dir_fails_closed() {
        let got = bundled_allowlist_ignore_dirs("/nonexistent/bundled-skills", Some("pdf"));
        assert_eq!(got, vec!["/nonexistent/bundled-skills".to_string()]);
    }
    /// Unique skill names: discovery also reads the dev machine's `~/.grok`.
    #[tokio::test]
    async fn bundled_allowlist_filters_discovery() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for name in ["allowlist-e2e-kept", "allowlist-e2e-blocked"] {
            let skill_dir = tmp.path().join(format!("bundled__{name}"));
            std::fs::create_dir(&skill_dir).expect("create subdir");
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: test\n---\nbody"),
            )
            .expect("write SKILL.md");
        }
        let dir = tmp.path().to_string_lossy().into_owned();
        let cwd = tempfile::tempdir().expect("tempdir");
        let mut config = crate::discovery::SkillsConfig {
            bundled_skill_dirs: vec![dir.clone()],
            ..Default::default()
        };
        config.ignore.extend(bundled_allowlist_ignore_dirs(
            &dir,
            Some("allowlist-e2e-kept"),
        ));
        let skills = crate::discovery::discover_skills(cwd.path(), &config).await;
        let names: Vec<&str> = skills
            .iter()
            .filter_map(|s| s["name"].as_str())
            .filter(|n| n.starts_with("allowlist-e2e-"))
            .collect();
        assert_eq!(
            names,
            vec!["allowlist-e2e-kept"],
            "only the allowlisted skill survives"
        );
    }
    #[tokio::test]
    async fn workspace_tool_definitions_payload_none_for_unknown_session() {
        let handle = make_handle();
        assert!(
            handle.workspace_tool_definitions_payload("ghost").is_none(),
            "unknown session yields no payload"
        );
    }
    /// Handle backed by a real upload queue and a pre-created "main" session;
    /// `tool_defs_enabled` and `upload_queue_enabled` are injected via `build`
    /// so tests never race process env.
    fn make_handle_with_queue_routing(
        tool_defs_enabled: bool,
        upload_queue_enabled: bool,
    ) -> (
        WorkspaceHandle,
        Arc<xai_file_utils::queue::UploadQueue>,
        tempfile::TempDir,
    ) {
        use xai_computer_hub_sdk::auth::{AuthCredential, AuthProvider};
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            hub_config: None,
            auth_provider: None,
            server_metadata: None,
            status_config: Default::default(),
            project_lsp_trusted: true,
            require_explicit_toolset: false,
            confine_fs_to_workspace_root: false,
        };
        let home = tempfile::tempdir().unwrap();
        let auth: Arc<dyn AuthProvider> = Arc::new(AuthCredential::bearer("test-token"));
        let proxy = Arc::new(crate::upload::ProxyStorageConfig::new(
            auth,
            "https://proxy.example/v1".to_string(),
            crate::upload::environment::WorkspaceIdentity::default(),
        ));
        let source: Arc<dyn xai_file_utils::queue::TraceExportSource> =
            Arc::new(crate::upload::WorkspaceTraceExportSource::new(proxy));
        let queue = Arc::new(xai_file_utils::queue::UploadQueue::spawn(
            home.path(),
            source,
            xai_file_utils::queue::UploadRetryPolicy::default(),
        ));
        let handle = WorkspaceHandle::build(
            config,
            home.path().to_path_buf(),
            Some(queue.clone()),
            upload_queue_enabled,
            false,
            false,
            false,
            tool_defs_enabled,
            crate::upload::environment::WorkspaceIdentity::default(),
        )
        .expect("handle construction should succeed");
        handle.create_session("main").expect("create main session");
        (handle, queue, home)
    }
    /// [`make_handle_with_queue_routing`] with the legacy (queue-routing off)
    /// default used by most tests.
    fn make_handle_with_queue(
        tool_defs_enabled: bool,
    ) -> (
        WorkspaceHandle,
        Arc<xai_file_utils::queue::UploadQueue>,
        tempfile::TempDir,
    ) {
        make_handle_with_queue_routing(tool_defs_enabled, false)
    }
    async fn wait_enqueued(queue: &xai_file_utils::queue::UploadQueue, want: u64) {
        use std::sync::atomic::Ordering;
        for _ in 0..200 {
            if queue.stats().enqueued.load(Ordering::Relaxed) >= want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for {want} enqueued, got {}",
            queue.stats().enqueued.load(Ordering::Relaxed)
        );
    }
    #[tokio::test]
    async fn emit_workspace_tool_definitions_enqueues_when_enabled() {
        let (handle, queue, _home) = make_handle_with_queue(true);
        handle.emit_workspace_tool_definitions("main");
        wait_enqueued(&queue, 1).await;
        assert_eq!(
            queue
                .stats()
                .enqueued
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "flag-on emission must enqueue exactly one artifact"
        );
    }
    #[tokio::test]
    async fn emit_workspace_tool_definitions_noop_when_flag_off() {
        let (handle, queue, _home) = make_handle_with_queue(false);
        handle.emit_workspace_tool_definitions("main");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            queue
                .stats()
                .enqueued
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "flag-off must not enqueue (legacy behaviour preserved)"
        );
    }
    #[tokio::test]
    async fn enqueue_workspace_tool_definitions_reports_enqueued_at_session_root() {
        let (handle, queue, _home) = make_handle_with_queue(true);
        let (path, bytes) = handle
            .workspace_tool_definitions_payload("main")
            .expect("payload for an existing session");
        assert_eq!(path, "main/workspace_tool_definitions.json");
        let outcome = enqueue_workspace_tool_definitions(&queue, "main", &path, &bytes).await;
        assert_eq!(outcome, xai_file_utils::queue::EnqueueOutcome::Enqueued);
        assert_eq!(
            queue
                .stats()
                .enqueued
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }
    #[test]
    fn phase1_budget_is_one_third_of_grace() {
        assert_eq!(
            phase1_budget(std::time::Duration::from_secs(45)),
            std::time::Duration::from_secs(15)
        );
        assert_eq!(
            phase1_budget(std::time::Duration::from_secs(120)),
            std::time::Duration::from_secs(40)
        );
    }
    #[test]
    fn phase15_budget_is_half_of_remaining() {
        assert_eq!(
            phase15_budget(std::time::Duration::from_secs(30)),
            std::time::Duration::from_secs(15)
        );
        assert_eq!(
            phase15_budget(std::time::Duration::ZERO),
            std::time::Duration::ZERO
        );
    }
    #[test]
    fn classify_drain_outcome_covers_all_arms() {
        assert_eq!(
            classify_drain_outcome(false, false, 0, 1),
            DrainOutcome::Partial
        );
        assert_eq!(
            classify_drain_outcome(false, true, 0, 0),
            DrainOutcome::Partial
        );
        assert_eq!(
            classify_drain_outcome(true, false, 0, 2),
            DrainOutcome::ProducersTimeout
        );
        assert_eq!(
            classify_drain_outcome(true, false, 0, 0),
            DrainOutcome::ProducersTimeout
        );
        assert_eq!(
            classify_drain_outcome(true, true, 1, 0),
            DrainOutcome::ProducersTimeout
        );
        assert_eq!(
            classify_drain_outcome(true, true, 0, 3),
            DrainOutcome::Timeout
        );
        assert_eq!(classify_drain_outcome(true, true, 0, 0), DrainOutcome::Full);
    }
    #[test]
    fn drain_reason_and_outcome_labels_are_stable() {
        assert_eq!(DrainReason::Sigterm.as_str(), "sigterm");
        assert_eq!(DrainReason::Evict.as_str(), "evict");
        assert_eq!(DrainOutcome::Full.as_str(), "full");
        assert_eq!(DrainOutcome::Partial.as_str(), "partial");
        assert_eq!(DrainOutcome::ProducersTimeout.as_str(), "producers_timeout");
        assert_eq!(DrainOutcome::Timeout.as_str(), "timeout");
    }
    #[test]
    fn grace_budget_from_raw_parses_and_falls_back() {
        let d = |ms| std::time::Duration::from_millis(ms);
        assert_eq!(grace_budget_from_raw(None), d(DEFAULT_TERMINATION_GRACE_MS));
        assert_eq!(grace_budget_from_raw(Some("120000".into())), d(120_000));
        assert_eq!(grace_budget_from_raw(Some("  90000 ".into())), d(90_000));
        assert_eq!(
            grace_budget_from_raw(Some("0".into())),
            d(DEFAULT_TERMINATION_GRACE_MS)
        );
        assert_eq!(
            grace_budget_from_raw(Some("nonsense".into())),
            d(DEFAULT_TERMINATION_GRACE_MS)
        );
    }
    #[test]
    fn write_draining_marker_writes_count_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace-server.draining");
        write_draining_marker(&path, 5);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "5");
        let leftover_tmp = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".draining.tmp"));
        assert!(!leftover_tmp, "temp file must be renamed away");
        write_draining_marker(&path, 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0");
    }
    #[tokio::test]
    async fn two_phase_drain_no_queue_marks_draining_and_returns_zero() {
        let handle = make_handle();
        let tracker = handle.activity_tracker().clone();
        assert!(!tracker.is_draining());
        let unfinished = handle
            .two_phase_drain(std::time::Duration::from_millis(300), DrainReason::Sigterm)
            .await;
        assert_eq!(unfinished, 0, "no queue → nothing pending to lose");
        assert!(
            tracker.is_draining(),
            "drain must mark the tracker draining"
        );
        let snap = tracker.snapshot();
        assert_eq!(
            snap.status,
            xai_tool_protocol::ToolServerLifecycleStatus::Draining
        );
        assert!(
            snap.drain_started_ms.is_some(),
            "drain_started_ms must be stamped at drain start"
        );
    }
    #[tokio::test]
    async fn spawn_producer_is_counted_and_withholds_idle() {
        let handle = make_handle();
        let tracker = handle.activity_tracker().clone();
        assert_eq!(tracker.snapshot().artifact_producers_inflight, 0);
        let gate = Arc::new(tokio::sync::Notify::new());
        let gate2 = gate.clone();
        let join = handle.spawn_producer(async move { gate2.notified().await });
        let snap = tracker.snapshot();
        assert_eq!(snap.artifact_producers_inflight, 1);
        assert!(
            snap.idle_since_ms.is_none(),
            "an in-flight producer must report the workspace busy"
        );
        gate.notify_one();
        join.await.expect("producer must finish");
        let snap = tracker.snapshot();
        assert_eq!(snap.artifact_producers_inflight, 0);
        assert!(
            snap.idle_since_ms.is_some(),
            "idle must be restored after the producer completes"
        );
    }
    /// A producer spawned after a drain has started stays TRACKED (the idle
    /// gate must keep seeing it) and is counted as at-risk.
    #[tokio::test]
    async fn spawn_producer_after_drain_start_stays_tracked() {
        let handle = make_handle();
        handle.shared.activity_tracker.set_draining();
        let before = PRODUCER_SPAWNED_AFTER_DRAIN_TOTAL.get();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let join = handle.spawn_producer(async move {
            let _ = rx.await;
            42
        });
        assert_eq!(
            handle.shared.producer_tasks.len(),
            1,
            "a late producer must remain visible to the durability idle gate"
        );
        assert_eq!(
            PRODUCER_SPAWNED_AFTER_DRAIN_TOTAL.get(),
            before + 1,
            "the at-risk late spawn must be counted"
        );
        let _ = tx.send(());
        assert_eq!(join.await.expect("task must run"), 42);
    }
    /// The producer tracker survives a completed drain: a workspace that keeps
    /// running after a hub evict still tracks (and idle-gates) new producers.
    #[tokio::test]
    async fn producer_tracker_usable_after_drain() {
        let handle = make_handle();
        handle
            .two_phase_drain(std::time::Duration::from_millis(200), DrainReason::Evict)
            .await;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let join = handle.spawn_producer(async move {
            let _ = rx.await;
            7
        });
        assert_eq!(
            handle.shared.producer_tasks.len(),
            1,
            "post-drain spawns must still be tracked (TaskTracker never closed)"
        );
        let _ = tx.send(());
        assert_eq!(join.await.expect("task must run"), 7);
    }
    #[tokio::test]
    async fn tool_state_upload_registers_producer() {
        let _env = crate::session::tool_config::TOOL_STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GROK_WORKSPACE_TOOL_STATE_ENABLED", "true") };
        let (handle, _queue, _home) = make_handle_with_queue(false);
        assert_eq!(handle.shared.producer_tasks.len(), 0);
        handle.spawn_tool_state_upload("main", 1);
        unsafe { std::env::remove_var("GROK_WORKSPACE_TOOL_STATE_ENABLED") };
        drop(_env);
        assert_eq!(
            handle.shared.producer_tasks.len(),
            1,
            "tool_state upload must register in the producer tracker"
        );
    }
    #[tokio::test]
    async fn tool_definitions_emit_registers_producer() {
        let (handle, _queue, _home) = make_handle_with_queue(true);
        assert_eq!(handle.shared.producer_tasks.len(), 0);
        handle.emit_workspace_tool_definitions("main");
        assert_eq!(
            handle.shared.producer_tasks.len(),
            1,
            "tool-definitions emission must register in the producer tracker"
        );
    }
    /// The drain must wait for a slow producer (phase 1.5) so its artifact
    /// reaches the queue before the queue drain runs: the producer enqueues an
    /// item the unreachable test queue can never upload, so `unfinished == 1`
    /// is only observable if the enqueue landed before phase 2 concluded.
    #[tokio::test]
    async fn two_phase_drain_waits_for_producer_then_drains_queue() {
        use std::sync::atomic::Ordering;
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let queue_home = tempfile::TempDir::new().unwrap();
        let queue = spawn_test_queue(queue_home.path());
        let handle = WorkspaceHandle::new_with_data_collection(
            WorkspaceHandle::test_config(cwd, factory),
            queue_home.path().to_path_buf(),
            queue.clone(),
            true,
            false,
            crate::upload::environment::WorkspaceIdentity::default(),
        )
        .expect("queue-backed handle construction");
        let produced = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let produced2 = produced.clone();
        let queue2 = queue.clone();
        handle.spawn_producer(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let _ = enqueue_workspace_tool_definitions(&queue2, "main", "main/x.json", b"{}").await;
            produced2.store(true, Ordering::SeqCst);
        });
        let unfinished = handle
            .two_phase_drain(
                std::time::Duration::from_millis(1_500),
                DrainReason::Sigterm,
            )
            .await;
        assert!(
            produced.load(Ordering::SeqCst),
            "drain must wait for the in-flight producer"
        );
        assert_eq!(
            unfinished, 1,
            "the producer's artifact must be in the queue when the queue drain times out"
        );
    }
    /// Phase 1.5 is capped at half the post-phase-1 remainder: a producer that
    /// would finish within the total budget (at 400ms of 600ms) but past the
    /// cap (300ms) is cut off there, preserving the phase-2 floor.
    #[tokio::test(start_paused = true)]
    async fn drain_phase15_is_capped_at_half_the_remaining_budget() {
        let handle = make_handle();
        let _join = handle.spawn_producer(async {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        });
        let t0 = tokio::time::Instant::now();
        let unfinished = handle
            .two_phase_drain(std::time::Duration::from_millis(600), DrainReason::Sigterm)
            .await;
        let elapsed = t0.elapsed();
        assert_eq!(
            unfinished, 1,
            "the producer cut off at the phase-1.5 cap is still in flight, so it \
             counts as outstanding work in the returned total"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(400),
            "phase 1.5 must give up at the cap, not wait for the \
             400ms producer; drained in {elapsed:?}"
        );
    }
    /// A wedged producer must not starve the phase-2 queue flush: items
    /// already durably enqueued before the drain still get drain time and are
    /// truthfully counted at the end.
    #[tokio::test]
    async fn drain_wedged_producer_does_not_starve_queue_flush() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let queue_home = tempfile::TempDir::new().unwrap();
        let queue = spawn_test_queue(queue_home.path());
        let handle = WorkspaceHandle::new_with_data_collection(
            WorkspaceHandle::test_config(cwd, factory),
            queue_home.path().to_path_buf(),
            queue.clone(),
            true,
            false,
            crate::upload::environment::WorkspaceIdentity::default(),
        )
        .expect("queue-backed handle construction");
        let outcome =
            enqueue_workspace_tool_definitions(&queue, "main", "main/pre.json", b"{}").await;
        assert_eq!(outcome, xai_file_utils::queue::EnqueueOutcome::Enqueued);
        let _join = handle.spawn_producer(std::future::pending::<()>());
        let before = DRAIN_COMPLETED_TOTAL
            .with_label_values(&[DrainOutcome::ProducersTimeout.as_str()])
            .get();
        let unfinished = handle
            .two_phase_drain(std::time::Duration::from_millis(600), DrainReason::Sigterm)
            .await;
        assert_eq!(
            unfinished, 2,
            "the returned total counts the pre-enqueued queue item (still observed \
             by the queue drain) plus the wedged producer"
        );
        assert!(
            DRAIN_COMPLETED_TOTAL
                .with_label_values(&[DrainOutcome::ProducersTimeout.as_str()])
                .get()
                > before,
            "the wedged producer dominates the outcome label"
        );
    }
    /// A producer that outlives the whole grace budget classifies as
    /// `producers_timeout` and must not wedge the drain.
    #[tokio::test(start_paused = true)]
    async fn two_phase_drain_producer_exceeding_budget_times_out() {
        let handle = make_handle();
        let _join = handle.spawn_producer(std::future::pending::<()>());
        let before = DRAIN_COMPLETED_TOTAL
            .with_label_values(&[DrainOutcome::ProducersTimeout.as_str()])
            .get();
        let unfinished = handle
            .two_phase_drain(std::time::Duration::from_millis(300), DrainReason::Sigterm)
            .await;
        assert_eq!(
            unfinished, 1,
            "no queue, but the wedged producer is outstanding work, so the returned \
             total is 1 (it was 0 when the return value ignored producers)"
        );
        assert!(
            DRAIN_COMPLETED_TOTAL
                .with_label_values(&[DrainOutcome::ProducersTimeout.as_str()])
                .get()
                > before,
            "the drain must classify as producers_timeout"
        );
    }
}
