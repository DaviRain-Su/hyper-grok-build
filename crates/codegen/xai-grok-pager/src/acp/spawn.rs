//! Agent spawning — creates the agent process and ACP channels.
//!
//! Simplified to only support GrokShell (in-process) mode.
//! Subprocess and remote modes can be added later if needed.

use std::rc::Rc;
use std::thread;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use xai_acp_lib::{
    AcpAgentChannel, AcpClientChannel, AcpClientTx, AcpGatewayReceiver, AcpGatewaySender,
    acp_channels,
};
use xai_grok_shell::{
    agent::{MvpAgent, config::Config as AgentConfig, models::RefreshStrategy},
    auth::AuthManager,
    util::grok_home::grok_home,
};

use super::router::ProviderRouterAgent;

/// Result of spawning a child agent.
pub struct SpawnedAgent {
    /// Kept alive so the thread isn't detached. Will be used for graceful shutdown.
    pub _thread_handle: thread::JoinHandle<Result<()>>,
    pub channel: AcpClientChannel,
    pub cancel: CancellationToken,
    /// The agent's `AuthManager`, shared so pager-side consumers (e.g. the voice
    /// channel) resolve the same refreshing bearer as chat traffic.
    pub auth_manager: std::sync::Arc<AuthManager>,
}

/// Spawn a GrokShell agent in a background thread.
///
/// Returns the ACP client channel for communication and a cancellation token.
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

    // Best-effort refresh of managed policy before bootstrap reads it (repairs a wrong-identity/missing
    // cache). Never errors — the OS-protected system/MDM layers still apply.
    xai_grok_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;

    // Run the full bootstrap sequence: config resolution, process-level
    // singletons, and model catalog construction.
    let (agent_config, models_manager) =
        xai_grok_shell::agent::init::bootstrap(&agent_config, &auth_manager, None)
            .map_err(|e| anyhow::anyhow!(e))?;
    models_manager
        .list_models(RefreshStrategy::OnlineIfUncached)
        .await;

    let agent_cancel = cancel.child_token();
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
    let handle =
        spawn_agent_thread_direct(spawn_fn, acp_agent, agent_cancel.clone(), skills_paths)?;

    Ok(SpawnedAgent {
        _thread_handle: handle,
        channel: acp_client,
        cancel: agent_cancel,
        auth_manager: auth_manager_for_pager,
    })
}

/// Spawn an agent in a dedicated thread with direct RPC dispatch.
///
/// The agent runs on a single-threaded tokio LocalSet runtime.
/// RPC requests go directly to the agent via Rc, bypassing simplex pipes.
fn spawn_agent_thread_direct(
    spawn_agent: Box<dyn FnOnce(AcpClientTx) -> Result<Rc<ProviderRouterAgent>> + Send + 'static>,
    channel: AcpAgentChannel,
    cancel: CancellationToken,
    skills_paths: Vec<String>,
) -> Result<thread::JoinHandle<Result<()>>> {
    Ok(thread::Builder::new()
        .name("acp-agent-worker".into())
        .spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
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

                // Keep running until cancelled
                cancel.cancelled().await;
                anyhow::Result::Ok(())
            })
        })?)
}
