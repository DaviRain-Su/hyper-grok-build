//! Agent spawning — creates the agent process and ACP channels.
//!
//! Simplified to only support GrokShell (in-process) mode.
//! Subprocess and remote modes can be added later if needed.

use std::io::IsTerminal;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use xai_grok_telemetry::startup::{self, StartupPhase};

use xai_acp_lib::{
    AcpAgentChannel, AcpClientChannel, AcpClientTx, AcpGatewayReceiver, AcpGatewaySender,
    acp_channels,
};
use xai_grok_shell::{
    agent::{MvpAgent, activity::SESSION_FLUSH_GRACE, config::Config as AgentConfig},
    auth::AuthManager,
    util::grok_home::grok_home,
};

use super::router::ProviderRouterAgent;

/// Extra slack when joining the agent OS thread after cancel so the flush
/// can finish and the thread can unwind.
const AGENT_JOIN_SLACK: Duration = Duration::from_secs(2);

/// Grace for the worker runtime's teardown after the run loop exits: a plain
/// drop waits out every in-flight `spawn_blocking` task (non-abortable), so a
/// long detached archive build would otherwise hold `/quit` for its duration.
const WORKER_RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Bounded worker-runtime teardown; see [`WORKER_RUNTIME_SHUTDOWN_GRACE`].
///
/// A timed-out blocking task is abandoned, not cancelled — safe only because
/// the worker exits with the process, which reaps the leftover thread.
fn shutdown_worker_runtime(rt: tokio::runtime::Runtime) {
    rt.shutdown_timeout(WORKER_RUNTIME_SHUTDOWN_GRACE);
}

/// How long the join stays silent before telling an interactive user why exit
/// is taking a moment. Short joins (the common case) print nothing.
const JOIN_NOTICE_AFTER: Duration = Duration::from_millis(1500);

/// Stderr notice after a slow join. Covers the whole SessionEnd pipeline
/// (hooks, telemetry sync, upload drain, memory, optional dream) — not
/// hooks alone, so the copy is intentionally not "session hooks".
const JOIN_NOTICE: &str = "Finishing session…";

/// Result of spawning a child agent.
pub struct SpawnedAgent {
    /// Agent worker OS thread. Hand to [`AgentShutdownGuard`] so the worker is
    /// cancelled and joined — letting session actors finish SessionEnd teardown
    /// (hooks, telemetry, uploads, memory) — on every exit path.
    pub thread_handle: thread::JoinHandle<Result<()>>,
    pub channel: AcpClientChannel,
    pub cancel: CancellationToken,
    /// The agent's `AuthManager`, shared so pager-side consumers (e.g. the voice
    /// channel) resolve the same refreshing bearer as chat traffic.
    pub auth_manager: std::sync::Arc<AuthManager>,
}

/// The single teardown mechanism for an in-process agent: cancels the worker
/// and joins it on drop, so session actors always get
/// `SessionCommand::Shutdown` (SessionEnd hooks, telemetry drain, memory)
/// before the process exits — on normal return, `?` bail, or panic unwind alike.
///
/// Hold one from every site that calls [`spawn_grok_shell`] (headless, the TUI,
/// `models`, `worktree`, `share`). Scope-end drop is the default; the TUI is the
/// one caller that drops it explicitly, because the join has to happen before
/// background processes are reaped (see `app::run`).
pub struct AgentShutdownGuard {
    cancel: CancellationToken,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl AgentShutdownGuard {
    /// Guard an in-process agent worker. A `None` thread makes the guard a
    /// no-op cancel (leader mode has no in-process worker to join).
    pub fn new(cancel: CancellationToken, thread: Option<thread::JoinHandle<Result<()>>>) -> Self {
        Self { cancel, thread }
    }
}

impl Drop for AgentShutdownGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let Some(handle) = self.thread.take() else {
            return;
        };
        let timeout = SESSION_FLUSH_GRACE + AGENT_JOIN_SLACK;
        match join_agent_thread(handle, timeout) {
            JoinOutcome::Joined => {}
            JoinOutcome::Failed(error) => {
                tracing::warn!(%error, "agent worker exited with error after cancel");
            }
            JoinOutcome::Panicked(panic) => {
                tracing::warn!(%panic, "agent worker panicked after cancel");
            }
            JoinOutcome::TimedOut => {
                tracing::warn!(
                    timeout_ms = timeout.as_millis() as u64,
                    "agent worker did not exit within grace after cancel; \
                     SessionEnd teardown (hooks/telemetry/uploads) may be incomplete"
                );
            }
            JoinOutcome::HelperLost => {
                tracing::warn!("agent worker join helper disappeared; proceeding");
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum JoinOutcome {
    Joined,
    Failed(String),
    Panicked(String),
    TimedOut,
    HelperLost,
}

/// Wait up to `timeout` for a cancelled agent worker to exit.
///
/// The blocking `join` runs on a helper thread so this stays callable from
/// `Drop` — which cannot await — while every caller sits on the async runtime.
/// On timeout that helper is abandoned rather than joined; this is safe **only
/// because every caller is on its way out of the process**, so the OS reaps the
/// thread at exit. Do not reuse this outside teardown.
fn join_agent_thread(handle: thread::JoinHandle<Result<()>>, timeout: Duration) -> JoinOutcome {
    use std::sync::mpsc::RecvTimeoutError;

    let span = xai_grok_telemetry::session_end::join_span();
    let start = Instant::now();

    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(handle.join());
    });

    let quiet = timeout.min(JOIN_NOTICE_AFTER);
    let mut notice_shown = false;
    let outcome = match rx.recv_timeout(quiet) {
        Ok(result) => classify_join(result),
        Err(RecvTimeoutError::Disconnected) => JoinOutcome::HelperLost,
        Err(RecvTimeoutError::Timeout) => {
            if std::io::stderr().is_terminal() {
                eprintln!("{JOIN_NOTICE}");
                notice_shown = true;
            }
            match rx.recv_timeout(timeout.saturating_sub(quiet)) {
                Ok(result) => classify_join(result),
                Err(RecvTimeoutError::Timeout) => JoinOutcome::TimedOut,
                Err(RecvTimeoutError::Disconnected) => JoinOutcome::HelperLost,
            }
        }
    };

    let outcome_label: &'static str = (&outcome).into();
    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    xai_grok_telemetry::session_end::record_join(&span, outcome_label, elapsed_ms, notice_shown);
    crate::unified_log::write_direct_info(
        "session_end.worker_join",
        Some(serde_json::json!({
            "elapsed_ms": elapsed_ms,
            "outcome": outcome_label,
            "notice_shown": notice_shown,
        })),
    );
    outcome
}

fn classify_join(result: thread::Result<Result<()>>) -> JoinOutcome {
    match result {
        Ok(Ok(())) => JoinOutcome::Joined,
        Ok(Err(e)) => JoinOutcome::Failed(e.to_string()),
        Err(payload) => JoinOutcome::Panicked(panic_message(payload)),
    }
}

/// Render a panic payload as text — `panic!` payloads are `&str` or `String`,
/// so the log shows the message instead of an opaque `Any`.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Spawn a GrokShell agent in a background thread.
pub async fn spawn_grok_shell(
    agent_config: AgentConfig,
    cancel: &CancellationToken,
    memory_config: Option<xai_grok_shell::config::MemoryConfig>,
) -> Result<SpawnedAgent> {
    let auth_manager = std::sync::Arc::new(AuthManager::new(
        &grok_home(),
        agent_config.grok_com_config.clone(),
    ));
    auth_manager.configure_refresher(
        agent_config.grok_com_config.auth_provider_command.clone(),
        None,
    );
    // Pause token refreshes across system sleep so an OIDC refresh can't
    // straddle a suspend (which can revoke the refresh token and force
    // re-login). No-op where the OS listener is unavailable.
    auth_manager.start_system_power_listener();

    let agent_cancel = cancel.child_token();

    auth_manager.prewarm_auth_refresh(agent_cancel.child_token());
    // Dropping a token does not cancel it: a `?` exit below creates no
    // SpawnedAgent and no AgentShutdownGuard, so this guard cancels the
    // prewarm instead. Disarmed where ownership transfers to SpawnedAgent.
    let cancel_prewarm_unless_spawned = agent_cancel.clone().drop_guard();

    xai_grok_shell::agent::app::apply_otel_config(&auth_manager, &agent_config.grok_com_config);

    xai_grok_shell::agent::models::startup_prefetch::begin_before_policy_gate(&agent_config);

    xai_grok_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;

    // Run the full bootstrap sequence: config resolution, process-level
    // singletons, and model catalog construction.
    let (agent_config, models_manager) =
        xai_grok_shell::agent::init::bootstrap(&agent_config, &auth_manager, None)
            .map_err(anyhow::Error::new)?;
    models_manager.spawn_background_refresh();

    let (acp_client, acp_agent) = acp_channels();

    // Clone before `auth_manager` is moved into the agent closure below, so the
    // pager (voice channel) can share the same refreshing bearer.
    let auth_manager_for_pager = auth_manager.clone();

    let skills_paths = agent_config.skills.paths.clone();

    let spawn_fn: Box<dyn FnOnce(AcpClientTx) -> Result<Rc<ProviderRouterAgent>> + Send + 'static> = {
        Box::new(move |client_tx| {
            let gateway = AcpGatewaySender::new(client_tx);

            let mut agent =
                MvpAgent::with_models(gateway.clone(), &agent_config, auth_manager, models_manager);
            if let Some(mc) = memory_config {
                agent.set_memory_config(mc);
            }
            Ok(Rc::new(ProviderRouterAgent::new(Rc::new(agent), gateway)))
        })
    };

    // Spawn the agent thread with direct dispatch
    startup::enter(StartupPhase::WorkerSpawn);
    let handle =
        spawn_agent_thread_direct(spawn_fn, acp_agent, agent_cancel.clone(), skills_paths).await?;

    // The spawn succeeded: the caller's AgentShutdownGuard owns cancellation now.
    let agent_cancel = cancel_prewarm_unless_spawned.disarm();
    Ok(SpawnedAgent {
        thread_handle: handle,
        channel: acp_client,
        cancel: agent_cancel,
        auth_manager: auth_manager_for_pager,
    })
}

/// Spawn an agent in a dedicated thread with direct RPC dispatch.
///
/// The agent runs on a single-threaded tokio LocalSet runtime.
/// RPC requests go directly to the agent via Rc, bypassing simplex pipes.
async fn spawn_agent_thread_direct(
    spawn_agent: Box<dyn FnOnce(AcpClientTx) -> Result<Rc<ProviderRouterAgent>> + Send + 'static>,
    channel: AcpAgentChannel,
    cancel: CancellationToken,
    skills_paths: Vec<String>,
) -> Result<thread::JoinHandle<Result<()>>> {
    // Off the UI worker: failure must fail spawn, not start ACP.
    let rt = tokio::task::spawn_blocking(|| {
        let mut builder = tokio::runtime::Builder::new_current_thread();
        xai_tty_utils::runtime::build_with_blocking_pool(builder.enable_all())
    })
    .await
    .map_err(|e| anyhow::anyhow!("agent runtime worker join: {e}"))?
    .map_err(|e| {
        tracing::error!(error = %e, "failed to start agent runtime");
        anyhow::anyhow!("failed to start agent runtime: {e}")
    })?;
    Ok(thread::Builder::new()
        .name("acp-agent-worker".into())
        .spawn(move || -> Result<()> {
            let local = tokio::task::LocalSet::new();
            let result = local.block_on(&rt, async move {
                let client_tx = channel.tx.clone();
                let agent_rc = spawn_agent(client_tx)?;

                // Direct dispatch: RPC requests go straight to the agent
                let gw_rx =
                    AcpGatewayReceiver::new(channel.rx, agent_rc.clone()).with_tracing(true);
                tokio::task::spawn_local(gw_rx.run());

                let _skills_watcher = {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let workspace_user_dir =
                        xai_grok_agent::prompt::workspace_user::optional_workspace_user_dir();
                    xai_grok_shell::config::watcher::SkillsFileWatcher::start(
                        Some(cwd.as_path()),
                        workspace_user_dir.as_deref(),
                        &skills_paths,
                    )
                    .map(|(mut watcher, mut skills_rx)| {
                        let agent = agent_rc.clone();
                        tokio::task::spawn_local(async move {
                            while let Some(change) = skills_rx.recv().await {
                                let created_discovery_dir = watcher.refresh_new_discovery_dirs();
                                match change {
                                    xai_grok_shell::config::watcher::DiscoveryChange::Skills => {
                                        tracing::info!(
                                            "skill directory changed on disk; reloading skills for all sessions"
                                        );
                                        agent.reload_skills_all_sessions();
                                        if created_discovery_dir {
                                            agent.advertise_commands_all_sessions();
                                        }
                                    }
                                    xai_grok_shell::config::watcher::DiscoveryChange::Workflows => {
                                        tracing::info!(
                                            "workflow directory changed on disk; re-advertising commands for all sessions"
                                        );
                                        agent.advertise_commands_all_sessions();
                                    }
                                }
                            }
                        })
                    })
                };
                tokio::task::yield_now().await;

                // Keep running until cancelled, then flush every live session
                // actor (SessionEnd hooks + memory save) before the LocalSet /
                // agent drop. Session actors live on dedicated OS threads and
                // only exit cleanly on SessionCommand::Shutdown; without this
                // flush, /exit and headless quit race process death and skip
                // SessionEnd. Mirrors leader auto-update / relaunch.
                cancel.cancelled().await;
                agent_rc.flush_all_sessions(SESSION_FLUSH_GRACE).await;
                xai_grok_telemetry::session_ctx::drain_at_process_exit().await;
                anyhow::Result::Ok(())
            });
            // LocalSet before runtime, as an implicit scope-end drop would do.
            drop(local);
            shutdown_worker_runtime(rt);
            result
        })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Teardown must stay grace-bounded with a non-abortable blocking task
    /// still in flight (a plain drop would wait it out).
    #[test]
    fn worker_runtime_teardown_bounded_despite_inflight_blocking_task() {
        let mut builder = tokio::runtime::Builder::new_current_thread();
        let rt = xai_tty_utils::runtime::build_with_blocking_pool(builder.enable_all()).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        rt.handle().spawn_blocking(move || {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_secs(6));
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("blocking task must start");
        let start = std::time::Instant::now();
        shutdown_worker_runtime(rt);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "teardown blocked on the blocking task: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn join_reports_clean_worker_exit() {
        let handle = thread::spawn(|| Ok(()));
        assert_eq!(
            join_agent_thread(handle, Duration::from_secs(5)),
            JoinOutcome::Joined
        );
    }

    #[test]
    fn join_reports_worker_error() {
        let handle = thread::spawn(|| Err(anyhow::anyhow!("flush failed")));
        assert_eq!(
            join_agent_thread(handle, Duration::from_secs(5)),
            JoinOutcome::Failed("flush failed".to_string())
        );
    }

    /// The timeout branch the built-binary e2e cannot reach: a wedged worker
    /// (e.g. a hung SessionEnd hook) is abandoned once the budget elapses
    /// instead of holding the process open indefinitely.
    #[test]
    fn join_abandons_wedged_worker_at_budget() {
        let handle = thread::spawn(|| {
            thread::sleep(Duration::from_secs(30));
            Ok(())
        });
        let started = std::time::Instant::now();
        assert_eq!(
            join_agent_thread(handle, Duration::from_millis(50)),
            JoinOutcome::TimedOut
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "join must return at its budget, not wait out the worker"
        );
    }

    #[test]
    fn panic_payloads_render_as_text() {
        assert_eq!(
            classify_join(Err(Box::new("boom"))),
            JoinOutcome::Panicked("boom".to_string())
        );
        assert_eq!(
            classify_join(Err(Box::new("boom".to_string()))),
            JoinOutcome::Panicked("boom".to_string())
        );
        assert_eq!(
            classify_join(Err(Box::new(7u32))),
            JoinOutcome::Panicked("non-string panic payload".to_string())
        );
    }
}
