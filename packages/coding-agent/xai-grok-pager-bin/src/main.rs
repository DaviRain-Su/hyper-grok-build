#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
#[cfg(all(feature = "jemalloc", feature = "release-dist", unix))]
mod jemalloc_malloc_conf {
    /// jemalloc looks up `extern const char *malloc_conf` — a thin pointer,
    /// not a Rust `&[u8]` fat pointer.
    #[repr(transparent)]
    struct MallocConfPtr(*const u8);
    unsafe impl Sync for MallocConfPtr {}
    static CONF: [u8; 63] = *b"prof:true,prof_active:false,lg_prof_sample:19,prof_final:false\0";
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    #[used]
    #[unsafe(export_name = "malloc_conf")]
    static MALLOC_CONF: MallocConfPtr = MallocConfPtr(CONF.as_ptr());
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[used]
    #[unsafe(export_name = "_rjem_malloc_conf")]
    static MALLOC_CONF: MallocConfPtr = MallocConfPtr(CONF.as_ptr());
}
use anyhow::Result;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use tokio_util::sync::CancellationToken;
use xai_grok_pager::app::cli::DashboardArgs;
use xai_grok_pager::app::{
    AgentCmd, Command, HeadlessArgs, LeaderMgmtArgs, LeaderMgmtCommand, LeaderMode,
    LeaderTargetArgs, PagerArgs, join_early_prefetch, resolve_leader_mode, resolve_use_leader,
    warn_leader_disabled_by_sandbox,
};
use xai_grok_pager::app::{WorkspaceMgmtArgs, WorkspaceMgmtCommand, WorkspaceStartArgs};
use xai_grok_pager::client_identity::PAGER_CLIENT_VERSION;
use xai_grok_shell::agent::app::{run_headless, run_leader, run_stdio_agent};
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::leader::{
    ClientCapabilities, ClientMode, ControlCommand, LeaderCapabilities, LeaderDescriptor,
    LeaderRegistration, LeaderTarget, leader_is_older_than,
};
use xai_grok_shell::leader::{
    ControlPayload, LeaderClient, LeaderEnvUrls, connect_or_spawn, socket_path_for_ws_url,
};
use xai_grok_telemetry::process_info::{
    Entrypoint, Interactivity, ProcessIdentity, ReleaseChannel, set_identity, set_release_channel,
};
fn process_identity(command: Option<&Command>, is_interactive: bool) -> Option<ProcessIdentity> {
    use xai_grok_telemetry::process_info::LeaderMode::Standalone;
    let (entrypoint, interactivity) = match command {
        Some(Command::Agent(_)) => return None,
        Some(Command::Dashboard(_)) => return None,
        Some(Command::Codex(_)) => return None,
        Some(Command::Login { .. }) => (Entrypoint::Cli, Interactivity::Interactive),
        Some(
            Command::Inspect { .. }
            | Command::Doctor(_)
            | Command::Leader(_)
            | Command::Logout { .. }
            | Command::Nexus { .. }
            | Command::Mcp(_)
            | Command::Plugin(_)
            | Command::Memory(_)
            | Command::Models
            | Command::Sessions(_)
            | Command::Setup { .. }
            | Command::Share(_)
            | Command::Wrap(_)
            | Command::Export(_)
            | Command::Trace(_)
            | Command::Update { .. }
            | Command::Version { .. }
            | Command::Completions { .. }
            | Command::Worktree(_)
            | Command::DiskUsage(_)
            | Command::Workspace(_),
        ) => (Entrypoint::Cli, Interactivity::Unattended),
        None if is_interactive => return None,
        None => (Entrypoint::Headless, Interactivity::Unattended),
    };
    Some(ProcessIdentity {
        entrypoint,
        leader: Standalone,
        interactivity,
    })
}
use std::env;
use xai_grok_update::{UpdateConfig, auto_update};

/// Community builds skip the upstream version-policy gate (fork releases
/// follow their own cadence; the required-range check is for official builds).
fn enforce_version_policy_or_exit_community() {
    if cfg!(feature = "community-build") {
        return;
    }
    xai_grok_update::enforce_version_policy_or_exit();
}
/// Apply headless args to an existing config, only overriding values that are
/// explicitly set. This allows environment defaults to be preserved when
/// specific args are not provided.
fn apply_headless_args_to_config(args: &HeadlessArgs, config: &mut AgentConfig) {
    if let Some(v) = &args.grok_ws_origin {
        config.grok_com_config.grok_ws_origin = v.clone();
    }
    if let Some(v) = &args.grok_ws_url {
        config.grok_com_config.grok_ws_url = v.clone();
    }
}
/// Apply global endpoint CLI args to an existing config.
fn apply_agent_endpoint_args(
    agent_args: &xai_grok_pager::app::AgentArgs,
    config: &mut AgentConfig,
) {
    if let Some(v) = &agent_args.cli_chat_proxy_base_url {
        config.endpoints.cli_chat_proxy_base_url = Some(v.clone());
    }
    if let Some(v) = &agent_args.xai_api_base_url {
        config.endpoints.xai_api_base_url = v.clone();
    }
}
/// Resolve --agent-profile path: canonicalize and verify the file exists.
fn resolve_agent_profile_path(path: &std::path::Path) -> std::path::PathBuf {
    match dunce::canonicalize(path) {
        Ok(abs) if abs.is_file() => abs,
        Ok(abs) => {
            eprintln!(
                "error: --agent-profile path is not a file: {}",
                abs.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: --agent-profile path '{}': {}", path.display(), e);
            std::process::exit(1);
        }
    }
}
/// Print startup information for the serve command.
fn print_serve_startup_info(bind_addr: SocketAddr, secret: &str) {
    eprintln!();
    eprintln!("   Grok agent server starting...");
    eprintln!();
    eprintln!("   Address:  {}:{}", bind_addr.ip(), bind_addr.port());
    eprintln!("   Secret:   {}", secret);
    eprintln!();
    eprintln!(
        "   WebSocket URL: ws://{}/ws?server-key={}",
        bind_addr, secret
    );
    eprintln!();
}
/// Entrypoint tag for `grok -p`; keys the quiet stderr default in `init_tracing_simple`.
const HEADLESS_ENTRYPOINT: &str = "headless";
/// Initialize simple tracing for non-TUI agent modes.
fn init_tracing_simple(app_entrypoint: &'static str) {
    use tracing_subscriber::{EnvFilter, Layer as _, fmt, layer::SubscriberExt as _};
    use xai_grok_telemetry::debug_log::RMCP_SSE_NOISE_TARGET;
    let default_filter = if app_entrypoint == HEADLESS_ENTRYPOINT {
        "off"
    } else {
        "error"
    };
    let env_filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter.add_directive(
            format!("{RMCP_SSE_NOISE_TARGET}=error")
                .parse()
                .expect("static rmcp directive must parse"),
        ),
        Err(_) => EnvFilter::new(default_filter),
    };
    let fmt_layer = fmt::layer()
        .with_target(false)
        .with_ansi(true)
        .with_writer(std::io::stderr);
    let registry = tracing_subscriber::registry()
        .with(fmt_layer.with_filter(env_filter))
        .with(xai_grok_telemetry::sampling_log::layer())
        .with(xai_grok_telemetry::instrumentation::layer())
        .with(xai_grok_telemetry::hooks_log::layer())
        .with(xai_grok_telemetry::otel_layer::build_otel_layer(
            xai_grok_telemetry::otel_layer::OtelClientInfo {
                client_name: "grok-pager",
                client_version: xai_grok_version::VERSION,
                service_version: env!("VERSION_WITH_COMMIT"),
                app_entrypoint,
            },
            xai_grok_shell::auth::credential_provider::build_default_otel_layer_config(),
        ));
    xai_grok_telemetry::debug_log::install_firehose(registry, app_entrypoint);
    xai_grok_telemetry::external::init(
        xai_grok_shell::agent::config::resolve_external_otel_config(
            xai_grok_telemetry::external::config::ExternalClientInfo {
                service_version: env!("VERSION_WITH_COMMIT").to_owned(),
                client_version: xai_grok_version::VERSION.to_owned(),
                app_entrypoint: app_entrypoint.to_owned(),
            },
        ),
    );
}
/// The local dashboard must not initialize product analytics or external OTEL.
/// It only needs a small stderr subscriber for server diagnostics.
fn init_dashboard_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("xai_grok_dashboard=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Run the browser dashboard before pager initialization. This keeps the
/// read-only observer out of memtrace, Sentry, crash-session cleanup, managed
/// policy checks, and other TUI startup side effects.
fn run_web_dashboard(args: &DashboardArgs) -> Result<()> {
    init_dashboard_tracing();
    let mut config = xai_grok_dashboard::DashboardServerConfig::new(xai_grok_config::grok_home());
    if let Some(bind) = args.bind {
        config.bind = bind;
    }
    config.open_browser = !args.no_open;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    run_and_shutdown(
        runtime,
        xai_grok_dashboard::serve(config),
        RUNTIME_SHUTDOWN_GRACE,
    )
}

/// `grok setup`: rendering + exit codes only; fetch logic lives in `xai_grok_shell::managed_config`.
/// `json` prints the served configuration instead of installing it.
async fn run_setup_command(json: bool) {
    use xai_grok_shell::managed_config::{self, SetupOutcome};
    if !managed_config::has_principal() {
        eprintln!("No deployment key or team sign-in found.");
        eprintln!();
        eprintln!("To install managed configuration, sign in with a team using `grok login`,");
        eprintln!("or set a deployment key:");
        eprintln!();
        if cfg!(unix) {
            eprintln!("  export GROK_DEPLOYMENT_KEY=<your-key>");
        } else {
            eprintln!("  $env:GROK_DEPLOYMENT_KEY=\"<your-key>\"");
        }
        eprintln!("  grok setup");
        eprintln!();
        eprintln!("Or add the key to ~/.grok/config.toml:");
        eprintln!();
        eprintln!("  [endpoints]");
        eprintln!("  deployment_key = \"<your-key>\"");
        eprintln!();
        eprintln!(
            "If you don't have a deployment key, contact your organization's Grok administrator."
        );
        std::process::exit(1);
    }
    if json {
        match managed_config::fetch_setup_report().await {
            Ok(report) => {
                let out = serde_json::to_string_pretty(&report)
                    .expect("setup report has no non-serializable values");
                println!("{out}");
                if !report.configured {
                    eprintln!(
                        "Your team doesn't have a managed configuration yet. A team admin can set one up at console.x.ai."
                    );
                }
            }
            Err(e) => {
                eprintln!("Couldn't fetch managed configuration. {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    match managed_config::run_setup().await {
        SetupOutcome::Installed => eprintln!("Applied managed configuration."),
        SetupOutcome::NothingConfigured => {
            eprintln!(
                "Your team doesn't have a managed configuration yet. A team admin can set one up at console.x.ai."
            );
        }
        SetupOutcome::Skipped => {
            eprintln!(
                "Managed configuration was not applied this run (another process held the apply lock, or the credential changed during the fetch). Run `grok setup` again."
            );
        }
        SetupOutcome::Failed(e) => {
            eprintln!("Couldn't apply managed configuration. {e}");
            std::process::exit(1);
        }
    }
}
async fn run_leader_mgmt(args: LeaderMgmtArgs) -> Result<()> {
    match args.command {
        LeaderMgmtCommand::Kill => kill_leaders().await,
        LeaderMgmtCommand::List { json } => {
            let leaders = xai_grok_shell::leader::discover_leaders().await;
            if json {
                let payload: Vec<_> = leaders.iter().map(leader_descriptor_json).collect();
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::Value::Array(payload))?
                );
            } else if leaders.is_empty() {
                println!("No leader candidates found.");
            } else {
                for d in &leaders {
                    print_leader_descriptor(d);
                }
            }
            Ok(())
        }
        LeaderMgmtCommand::Info { target, json } => {
            let (descriptor, client) = connect_to_leader(&target).await?;
            let info = match ensure_control_caps(client.registration()) {
                Ok(_) => client
                    .send_control(ControlCommand::GetLeaderInfo)
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                Err(_) => None,
            };
            if json {
                let payload = leader_info_json(&descriptor, client.registration(), info.as_ref())?;
                println!("{}", serde_json::to_string(&payload)?);
            } else if let Some(info) = info {
                println!("{info:#?}");
            } else {
                print_leader_descriptor(&descriptor);
                eprintln!(
                    "  (detailed info unavailable — leader does not advertise control capabilities)"
                );
            }
            client.cancel();
            Ok(())
        }
    }
}
async fn kill_leaders() -> Result<()> {
    let leaders = xai_grok_shell::leader::discover_leaders().await;
    if leaders.is_empty() {
        eprintln!("No leader candidates found.");
        return Ok(());
    }
    let mut killed = 0u32;
    let mut cleaned = 0u32;
    for d in &leaders {
        let Some(pid) = leader_pid(d) else {
            continue;
        };
        if !xai_grok_shell::util::is_grok_process(pid) {
            if let Some(ref lock) = d.lock_path {
                eprintln!("  PID {pid} is not a grok process, removing stale lock");
                let _ = std::fs::remove_file(lock);
                cleaned += 1;
            }
            if let Some(ref sock) = d.socket_path {
                let _ = std::fs::remove_file(sock);
            }
            continue;
        }
        eprintln!("  Killing leader PID {pid}");
        if let Err(e) = xai_grok_shell::util::kill_process_by_pid(pid) {
            eprintln!("  warning: failed to terminate PID {pid}: {e}");
            continue;
        }
        killed += 1;
    }
    if killed > 0 {
        eprintln!("Killed {killed} leader process(es).");
    } else if cleaned > 0 {
        eprintln!("No live leader processes found (cleaned up {cleaned} stale lock(s)).");
    } else {
        eprintln!("No live leader processes found.");
    }
    Ok(())
}
fn resolve_target(args: &LeaderTargetArgs) -> LeaderTarget {
    match args.pid {
        Some(pid) => LeaderTarget::Pid(pid),
        None => LeaderTarget::Environment(xai_grok_shell::env::GrokBuildEnvironment::Production),
    }
}
async fn connect_to_leader(
    args: &LeaderTargetArgs,
) -> Result<(LeaderDescriptor, xai_grok_shell::leader::LeaderClient)> {
    let target = resolve_target(args);
    let selection = xai_grok_shell::leader::resolve_leader_target(target)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let socket_path = selection
        .socket_path()
        .ok_or_else(|| anyhow::anyhow!("resolved leader target did not include a socket path"))?;
    let client = xai_grok_shell::leader::LeaderClient::connect(
        socket_path.to_path_buf(),
        "grok-pager-leader-cli",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await?;
    Ok((selection.descriptor, client))
}
/// Prefer socket-verified live PID over a possibly-recycled lock file PID.
fn leader_pid(d: &LeaderDescriptor) -> Option<u32> {
    d.live_info.as_ref().map(|li| li.pid).or(d.pid_from_lock)
}
fn print_leader_descriptor(d: &LeaderDescriptor) {
    let pid = leader_pid(d)
        .map(|p| p.to_string())
        .unwrap_or_else(|| "?".into());
    let sock = d
        .socket_path
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "?".into());
    let state = format!("{:?}", d.classification);
    eprintln!("  PID {pid} ({state}) -- {sock}");
}
fn leader_descriptor_json(d: &LeaderDescriptor) -> serde_json::Value {
    serde_json::json!({
        "pid": leader_pid(d),
        "pidFromLock": d.pid_from_lock,
        "pidLive": d.live_info.as_ref().map(|li| li.pid),
        "classification": format!("{:?}", d.classification),
        "socketPath": d.socket_path.as_deref().map(|p| p.display().to_string()),
        "lockPath": d.lock_path.as_deref().map(|p| p.display().to_string()),
        "wsUrlSuffix": d.ws_url_suffix,
    })
}
fn leader_info_json(
    d: &LeaderDescriptor,
    reg: &LeaderRegistration,
    info: Option<&xai_grok_shell::leader::ControlPayload>,
) -> Result<serde_json::Value> {
    let mut val = leader_descriptor_json(d);
    val["clientId"] = serde_json::json!(reg.client_id);
    if let Some(info) = info {
        val["info"] = serde_json::to_value(info)?;
    }
    Ok(val)
}
fn ensure_control_caps(reg: &LeaderRegistration) -> Result<&LeaderCapabilities> {
    reg.leader_capabilities
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Leader does not advertise capabilities (legacy version)"))
}
/// Env override for the `grok workspace` gate: any truthy value enables the
/// command locally, a falsy one disables it — bypassing the remote settings flag.
const WORKSPACE_COMMAND_ENV: &str = "GROK_WORKSPACE_COMMAND";
/// Resolution of the `grok workspace` gate. `Unknown` is kept separate from
/// `Disabled` so we don't tell the user the flag is off when the settings were
/// simply never read (both fail closed, but `Unknown` earns an honest message).
#[derive(Debug, PartialEq, Eq)]
enum WorkspaceGate {
    Enabled,
    Disabled,
    Unknown,
}
/// The `GROK_WORKSPACE_COMMAND` override, if set (`Some(true)`/`Some(false)`);
/// `None` defers to the remote settings flag.
fn workspace_command_env_override() -> Option<bool> {
    std::env::var(WORKSPACE_COMMAND_ENV)
        .ok()
        .map(|v| env_flag_enabled(&v))
}
/// Resolve the gate. Precedence: env override > remote `Some(true)` >
/// loaded-but-off (`Disabled`) > settings-not-loaded (`Unknown`).
fn workspace_command_gate(
    env_override: Option<bool>,
    remote_settings: Option<&xai_grok_shell::util::config::RemoteSettings>,
) -> WorkspaceGate {
    if let Some(enabled) = env_override {
        return if enabled {
            WorkspaceGate::Enabled
        } else {
            WorkspaceGate::Disabled
        };
    }
    match remote_settings {
        Some(rs) if rs.workspace_command_enabled.unwrap_or(false) => WorkspaceGate::Enabled,
        Some(_) => WorkspaceGate::Disabled,
        None => WorkspaceGate::Unknown,
    }
}
/// Truthy parse for grok on/off env vars: everything enables except the common
/// falsy spellings (`0`, `false`, `off`, `no`, empty).
fn env_flag_enabled(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "off" | "no"
    )
}
/// Blocking fetch of remote settings via the startup prefetch path.
fn fetch_remote_settings() -> Option<xai_grok_shell::util::config::RemoteSettings> {
    join_early_prefetch(xai_grok_shell::agent::models::start_early_prefetch(None))
}
async fn run_workspace_mgmt(args: WorkspaceMgmtArgs) -> Result<()> {
    if matches!(
        &args.command,
        WorkspaceMgmtCommand::Start(_)
            | WorkspaceMgmtCommand::Restart(_)
            | WorkspaceMgmtCommand::Resume { .. }
    ) && let Some(profile) = xai_grok_sandbox::requested_confinement_profile()
    {
        anyhow::bail!(
            "`grok workspace` start/restart/resume is unavailable under sandbox profile '{profile}': \
             those commands (re)activate shared-leader workspace exposure that this session cannot \
             prove is confined by that profile. Disable the profile at the source that selected it \
             (CLI, env, config, or a managed requirement)."
        );
    }
    let env_override = workspace_command_env_override();
    let remote_settings = if env_override.is_none() {
        fetch_remote_settings()
    } else {
        None
    };
    match workspace_command_gate(env_override, remote_settings.as_ref()) {
        WorkspaceGate::Enabled => {}
        WorkspaceGate::Disabled => {
            anyhow::bail!(
                "`grok workspace` is not enabled for this account \
             (gated by a server-side feature flag that is currently off)."
            )
        }
        WorkspaceGate::Unknown => {
            anyhow::bail!(
                "Could not load your settings for `grok workspace`. Check your \
             network connection (run `grok login` if you are signed out), then \
             try again."
            )
        }
    }
    match args.command {
        WorkspaceMgmtCommand::Start(a) => {
            workspace_start(a, false, remote_settings.or_else(fetch_remote_settings)).await
        }
        WorkspaceMgmtCommand::Restart(a) => {
            workspace_start(a, true, remote_settings.or_else(fetch_remote_settings)).await
        }
        WorkspaceMgmtCommand::Pause { target, json } => {
            workspace_control(&target, json, ControlCommand::WorkspacePause).await
        }
        WorkspaceMgmtCommand::Resume { target, json } => {
            workspace_control(&target, json, ControlCommand::WorkspaceResume).await
        }
        WorkspaceMgmtCommand::Stop { target, json } => {
            workspace_control(&target, json, ControlCommand::WorkspaceStop).await
        }
        WorkspaceMgmtCommand::Status { target, json } => {
            workspace_control(&target, json, ControlCommand::WorkspaceStatus).await
        }
    }
}
fn ensure_workspace_caps(reg: &LeaderRegistration) -> Result<()> {
    let caps = ensure_control_caps(reg)?;
    if !caps.workspace_exposure {
        anyhow::bail!(
            "the running leader does not support workspace exposure — stop the \
             leader process and re-run to pick up the new version"
        );
    }
    Ok(())
}
async fn connect_workspace_control(
    agent_config: &AgentConfig,
    target: &LeaderTargetArgs,
) -> Result<LeaderClient> {
    if target.pid.is_some() {
        let (_descriptor, client) = connect_to_leader(target).await?;
        return Ok(client);
    }
    let ws_url = &agent_config.grok_com_config.grok_ws_url;
    let socket = socket_path_for_ws_url(ws_url);
    LeaderClient::connect(
        socket,
        "grok-workspace-cli",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!(
            "no running leader for this environment ({e}). \
             Start a grok session, or run `grok workspace start`."
        )
    })
}
async fn workspace_control(
    target: &LeaderTargetArgs,
    json: bool,
    command: ControlCommand,
) -> Result<()> {
    let agent_config = xai_grok_shell::config::load_agent_config_disk_only()
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
    let client = connect_workspace_control(&agent_config, target).await?;
    ensure_workspace_caps(client.registration())?;
    let payload = client.send_control(command).await??;
    render_workspace_payload(&payload, json);
    client.cancel();
    Ok(())
}
async fn workspace_start(
    args: WorkspaceStartArgs,
    restart: bool,
    remote_settings: Option<xai_grok_shell::util::config::RemoteSettings>,
) -> Result<()> {
    use xai_grok_shell::auth::ensure_authenticated;
    xai_grok_shell::util::config::set_remote_campaigns_from_settings(remote_settings.as_ref());
    let raw_config = xai_grok_shell::config::load_effective_config()
        .map_err(|e| anyhow::anyhow!("Failed to load config: {e}"))?;
    let agent_config = AgentConfig::new_from_toml_cfg(&raw_config)
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
    let (use_leader, _) = resolve_use_leader(
        args.leader,
        args.no_leader,
        &raw_config,
        remote_settings.as_ref(),
        true,
        xai_grok_sandbox::requested_confinement_profile(),
    );
    if !use_leader {
        anyhow::bail!(
            "`grok workspace` requires leader mode (the workspace is shared via the leader).\n\
             Enable it with `[cli] use_leader = true` in ~/.grok/config.toml, or pass --leader."
        );
    }
    ensure_authenticated(
        &agent_config.grok_com_config,
        false,
        Some("No cached credentials found. Run `grok login` first."),
    )
    .await?;
    let env_urls = LeaderEnvUrls::from(&agent_config.grok_com_config);
    let capabilities = ClientCapabilities {
        client_version: Some(PAGER_CLIENT_VERSION.to_string()),
        ..Default::default()
    };
    let conn = connect_or_spawn(
        "grok-workspace-cli",
        ClientMode::Stdio,
        &env_urls,
        capabilities,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to start or connect to leader: {e}"))?;
    drop(conn);
    let target = LeaderTargetArgs::default();
    let client = connect_workspace_control(&agent_config, &target).await?;
    ensure_workspace_caps(client.registration())?;
    if restart {
        let _ = client.send_control(ControlCommand::WorkspaceStop).await;
    }
    let cwd = match args.cwd {
        Some(p) => p,
        None => std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("cannot determine current directory: {e}"))?,
    };
    let cwd = std::path::absolute(&cwd).unwrap_or(cwd);
    let payload = client
        .send_control(ControlCommand::WorkspaceStart {
            hub_url: args.hub_url.clone(),
            cwd: cwd.display().to_string(),
        })
        .await??;
    render_workspace_payload(&payload, args.json);
    client.cancel();
    Ok(())
}
fn render_workspace_payload(payload: &ControlPayload, json: bool) {
    let ControlPayload::WorkspaceStatus {
        state,
        hub_url,
        cwd,
        uptime_ms,
        active_tool_calls,
        sessions,
        pid,
    } = payload
    else {
        eprintln!("unexpected control response: {payload:?}");
        return;
    };
    if json {
        let value = serde_json::json!({
            "state": state,
            "hubUrl": hub_url,
            "cwd": cwd,
            "uptimeMs": uptime_ms,
            "activeToolCalls": active_tool_calls,
            "sessions": sessions,
            "pid": pid,
        });
        println!("{}", serde_json::to_string(&value).unwrap_or_default());
        return;
    }
    if state == "none" {
        println!("Workspace exposure: not running (leader PID {pid})");
        return;
    }
    println!("Workspace exposure: {state}");
    if let Some(url) = hub_url {
        println!("  hub:      {url}");
    }
    if let Some(dir) = cwd {
        println!("  cwd:      {dir}");
    }
    println!("  uptime:   {}s", uptime_ms / 1000);
    println!("  active:   {active_tool_calls} tool call(s)");
    let session_list = if sessions.is_empty() {
        "-".to_string()
    } else {
        sessions.join(", ")
    };
    println!("  sessions: {} ({session_list})", sessions.len());
    println!("  leader:   PID {pid}");
}
/// How to rebuild one session's `session/load` after a leader reconnect.
#[derive(Default, Clone)]
struct CachedSession {
    /// Verbatim `session/load` request JSON (preferred replay form: preserves
    /// the client's exact cwd / mcpServers / meta). `None` when the session
    /// was only ever created via `session/new` — the load is synthesized.
    load_request_json: Option<String>,
    /// `cwd` captured from `session/new` / `session/load` params.
    cwd: Option<String>,
    /// `mcpServers` captured from `session/new` / `session/load` params.
    mcp_servers_json: Option<String>,
}
/// ACP state cached from the stdio stream for replay after leader reconnect.
///
/// Tracks EVERY session the external client has open (IDE clients drive
/// multiple sessions over one bridge), not just the most recent one — a
/// leader crash must restore all of them or the others die with
/// "unknown session id" on their next prompt.
#[derive(Default, Clone)]
struct StdioReplayState {
    initialize_json: Option<String>,
    /// Sessions to restore on reconnect, keyed by session id, in first-seen
    /// order (Vec keeps replay order deterministic).
    sessions: Vec<(String, CachedSession)>,
    /// cwd/mcp from the most recent `session/new` REQUEST whose response has
    /// not been observed yet. Folded into `sessions` when the response
    /// carrying the assigned session id arrives. Never replayed while
    /// unconfirmed (the id is unknown; the client's own request died with the
    /// old leader and is its to retry).
    pending_new: Option<CachedSession>,
    /// Most recently created/loaded session id — reported in
    /// `x.ai/leader_reconnected` as the primary restored session.
    last_session_id: Option<String>,
}
impl StdioReplayState {
    fn upsert_session(&mut self, sid: &str, cached: CachedSession) {
        if let Some((_, existing)) = self.sessions.iter_mut().find(|(id, _)| id == sid) {
            *existing = cached;
        } else {
            self.sessions.push((sid.to_string(), cached));
        }
    }
    fn remove_session(&mut self, sid: &str) {
        self.sessions.retain(|(id, _)| id != sid);
        if self.last_session_id.as_deref() == Some(sid) {
            self.last_session_id = None;
        }
    }
    /// A resume names a session the client is already attached to, so an entry
    /// from its original `session/load` is the better one to replay: it carries
    /// the client's `_meta`, which a synthesized load cannot reproduce.
    fn insert_session_if_new(&mut self, sid: &str, cached: CachedSession) {
        if !self.sessions.iter().any(|(id, _)| id == sid) {
            self.sessions.push((sid.to_string(), cached));
        }
    }
}
/// `sessionId`, `cwd`, and `mcpServers` off a session request's params.
fn cached_session_from_params(
    params: &serde_json::Value,
    verbatim: Option<&str>,
) -> Option<(String, CachedSession)> {
    let sid = params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(|v| v.as_str())?;
    Some((
        sid.to_string(),
        CachedSession {
            load_request_json: verbatim.map(str::to_string),
            cwd: params
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            mcp_servers_json: params
                .get("mcpServers")
                .and_then(|m| serde_json::to_string(m).ok()),
        },
    ))
}
/// Methods whose requests the replay cache reads. One list shared by the
/// prefilter and the match below so the two cannot drift apart; quoted JSON
/// spellings so prose mentioning a method does not trigger a parse.
const CACHED_METHODS: &[&str] = &[
    "\"initialize\"",
    "\"session/new\"",
    "\"session/load\"",
    "\"session/resume\"",
    "\"session/close\"",
    "\"x.ai/session/close\"",
    "\"_x.ai/session/close\"",
];
fn cache_outgoing_acp_state(msg: &str, state: &std::sync::Mutex<StdioReplayState>) {
    if !CACHED_METHODS.iter().any(|m| msg.contains(m)) {
        return;
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(msg) else {
        return;
    };
    let method = json
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    match method {
        "initialize" => {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.initialize_json = Some(msg.to_string());
        }
        "session/load" => {
            let Some(params) = json.get("params") else {
                return;
            };
            let Some((sid, cached)) = cached_session_from_params(params, Some(msg)) else {
                return;
            };
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.upsert_session(&sid, cached);
            s.last_session_id = Some(sid);
        }
        "session/resume" => {
            let Some(params) = json.get("params") else {
                return;
            };
            let Some((sid, cached)) = cached_session_from_params(params, None) else {
                return;
            };
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.insert_session_if_new(&sid, cached);
            s.last_session_id = Some(sid);
        }
        "session/new" => {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            let params = json.get("params");
            s.pending_new = Some(CachedSession {
                load_request_json: None,
                cwd: params
                    .and_then(|p| p.get("cwd"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                mcp_servers_json: params
                    .and_then(|p| p.get("mcpServers"))
                    .and_then(|m| serde_json::to_string(m).ok()),
            });
        }
        "session/close" | "x.ai/session/close" | "_x.ai/session/close" => {
            if let Some(sid) = json
                .get("params")
                .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                .and_then(|v| v.as_str())
            {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.remove_session(sid);
            }
        }
        _ => {}
    }
}
fn cache_incoming_session_id(msg: &str, state: &std::sync::Mutex<StdioReplayState>) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(msg) else {
        return;
    };
    if let Some(sid) = json
        .get("result")
        .and_then(|r| r.get("sessionId").or_else(|| r.get("session_id")))
        .and_then(|v| v.as_str())
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = s.pending_new.take() {
            s.upsert_session(sid, pending);
        }
        s.last_session_id = Some(sid.to_string());
    }
}
/// Synthetic JSON-RPC id for the `session/load` the bridge constructs itself
/// (when the external client only ever sent `session/new`). A string id can
/// never collide with a numeric id the external client may have in flight.
const REPLAY_LOAD_REQUEST_ID: &str = "x.ai/leader-replay/session-load";
/// Max silence between two messages from the leader during a replayed request.
/// A `session/load` streams replay notifications continuously once it starts,
/// but the pre-replay phase (MCP resolution, session file reads) can be quiet
/// for a while on large sessions.
const REPLAY_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Overall deadline for one replayed request's response.
const REPLAY_RESPONSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
/// Outcome of one replayed request (see [`replay_request_until_response`]).
enum ReplayOutcome {
    /// The response for the replayed request arrived with a `result`.
    ResponseOk,
    /// The response arrived but carried an `error` (e.g. session not found).
    ResponseErr,
    /// The connection closed / timed out / stdout broke before the response.
    Failed,
}
/// True when `msg` is the JSON-RPC *response* to the request with `expected_id`
/// (no `method` key + matching `id`).
fn parse_replay_response(msg: &str, expected_id: &serde_json::Value) -> Option<ReplayOutcome> {
    let json = serde_json::from_str::<serde_json::Value>(msg).ok()?;
    if json.get("method").is_some() {
        return None;
    }
    if json.get("id") != Some(expected_id) {
        return None;
    }
    if json.get("error").is_some() {
        Some(ReplayOutcome::ResponseErr)
    } else {
        Some(ReplayOutcome::ResponseOk)
    }
}
/// Send one replayed request to the (new) leader and pump messages until its
/// response arrives.
///
/// `session/load` emits the full replay stream (session/update notifications)
/// BEFORE its response, so "wait for the next message" is not "wait for the
/// response". Everything that is not the response itself is forwarded verbatim
/// to the external client's stdout — exactly what the pre-reconnect stream
/// would have carried. Only the response to the replayed request is swallowed
/// (the external client already received a response for its original send and
/// must not see a duplicate or unknown-id response).
///
/// Returning before the `session/load` response is the root cause of the
/// "unknown session id" failures after a leader crash: the bridge declared
/// the reconnect complete while the new leader was still loading the session,
/// and the client's next `session/prompt` raced (and lost against) the load.
async fn replay_request_until_response(
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    request_json: &str,
    what: &str,
) -> ReplayOutcome {
    use tokio::io::AsyncWriteExt as _;
    let expected_id = serde_json::from_str::<serde_json::Value>(request_json)
        .ok()
        .and_then(|v| v.get("id").cloned());
    if tx.send(request_json.to_string()).is_err() {
        tracing::warn!(what, "replay: failed to send request");
        return ReplayOutcome::Failed;
    }
    let Some(expected_id) = expected_id else {
        return ReplayOutcome::ResponseOk;
    };
    let deadline = tokio::time::Instant::now() + REPLAY_RESPONSE_DEADLINE;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            tracing::warn!(what, "replay: response deadline exceeded");
            return ReplayOutcome::Failed;
        }
        let per_recv = REPLAY_RECV_TIMEOUT.min(deadline - now);
        match tokio::time::timeout(per_recv, rx.recv()).await {
            Ok(Some(msg)) => {
                if let Some(outcome) = parse_replay_response(&msg, &expected_id) {
                    tracing::debug!(
                        what,
                        ok = matches!(outcome, ReplayOutcome::ResponseOk),
                        "replay: response received"
                    );
                    return outcome;
                }
                if stdout.write_all(msg.as_bytes()).await.is_err()
                    || stdout.write_all(b"\n").await.is_err()
                    || stdout.flush().await.is_err()
                {
                    tracing::warn!(what, "replay: stdout closed while forwarding");
                    return ReplayOutcome::Failed;
                }
            }
            Ok(None) => {
                tracing::warn!(what, "replay: leader closed before response");
                return ReplayOutcome::Failed;
            }
            Err(_) => {
                tracing::warn!(what, "replay: timed out waiting for response");
                return ReplayOutcome::Failed;
            }
        }
    }
}
/// Build the `session/load` JSON to replay for one cached session: the
/// verbatim client request when available, else a synthesized load from the
/// captured `session/new` parameters.
fn replay_load_json(sid: &str, cached: &CachedSession) -> Option<String> {
    if let Some(ref verbatim) = cached.load_request_json {
        return Some(verbatim.clone());
    }
    let cwd = cached.cwd.as_deref()?;
    let mut params = serde_json::json!({
        "sessionId": sid,
        "cwd": cwd,
    });
    if let Some(ref mcp_raw) = cached.mcp_servers_json
        && let Ok(mcp_val) = serde_json::from_str::<serde_json::Value>(mcp_raw)
    {
        params["mcpServers"] = mcp_val;
    }
    Some(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": REPLAY_LOAD_REQUEST_ID,
            "method": "session/load",
            "params": params,
        })
        .to_string(),
    )
}
/// Replay cached `initialize` + every cached `session/load` to a freshly
/// (re-)elected leader, blocking until the leader has actually finished
/// loading EACH session (loads are sent strictly sequentially, each awaiting
/// its response — the synthesized-id reuse relies on this ordering).
///
/// Returns the primary restored session id (the most recently active one,
/// falling back to any successfully restored session). `None` when there was
/// nothing to replay or every restore failed — callers emit
/// `x.ai/leader_reconnected` with empty params in that case, signalling the
/// external client to re-establish state itself.
async fn replay_acp_state_after_reconnect(
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    state: &StdioReplayState,
) -> Option<String> {
    if let Some(ref init_json) = state.initialize_json {
        match replay_request_until_response(tx, rx, stdout, init_json, "initialize").await {
            ReplayOutcome::ResponseOk => {}
            ReplayOutcome::ResponseErr => {
                tracing::warn!("replay: initialize was rejected by new leader");
                return None;
            }
            ReplayOutcome::Failed => return None,
        }
    } else {
        tracing::debug!("replay: no cached initialize; skipping ACP replay");
        return None;
    }
    if state.sessions.is_empty() {
        tracing::debug!("replay: no sessions to replay");
        return None;
    }
    let mut restored: Vec<String> = Vec::new();
    for (sid, cached) in &state.sessions {
        let Some(load_json) = replay_load_json(sid, cached) else {
            tracing::warn!(session_id = %sid, "replay: no way to rebuild session/load; skipping");
            continue;
        };
        match replay_request_until_response(tx, rx, stdout, &load_json, "session/load").await {
            ReplayOutcome::ResponseOk => {
                tracing::info!(session_id = %sid, "replay: session restored");
                restored.push(sid.clone());
            }
            ReplayOutcome::ResponseErr => {
                tracing::warn!(
                    session_id = %sid,
                    "replay: session/load was rejected by new leader"
                );
            }
            ReplayOutcome::Failed => {
                tracing::warn!(
                    session_id = %sid,
                    "replay: transport failure during session/load; aborting remaining replays"
                );
                break;
            }
        }
    }
    state
        .last_session_id
        .as_ref()
        .filter(|sid| restored.iter().any(|r| r == *sid))
        .cloned()
        .or_else(|| restored.last().cloned())
}
/// Flush observability, then exit. Used by the agent/headless signal handler.
///
/// Does NOT write terminal escape codes — agent mode never enables TUI modes.
/// The TUI has its own signal handler (`app::signal_handler`) that does the
/// full crossterm teardown.
fn shutdown_and_flush_telemetry(exit_code: i32) -> ! {
    xai_grok_telemetry::sentry::flush_on_shutdown();
    xai_grok_telemetry::otel_layer::shutdown_otel();
    xai_grok_telemetry::debug_log::flush();
    std::process::exit(exit_code);
}
async fn forward_stdio_line_to_leader(
    line: Vec<u8>,
    leader_tx: &tokio::sync::Mutex<tokio::sync::mpsc::UnboundedSender<String>>,
    replay_state: &std::sync::Mutex<StdioReplayState>,
    cancel: &CancellationToken,
) {
    let line = String::from_utf8_lossy(&line);
    let mut trimmed = line.trim_end_matches(['\r', '\n']).to_string();
    if trimmed.is_empty() {
        return;
    }
    cache_outgoing_acp_state(&trimmed, replay_state);
    let send_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        {
            let tx = leader_tx.lock().await;
            match tx.send(trimmed) {
                Ok(()) => break,
                Err(tokio::sync::mpsc::error::SendError(v)) => trimmed = v,
            }
        }
        if cancel.is_cancelled() || tokio::time::Instant::now() >= send_deadline {
            tracing::error!(
                "stdio bridge: dropping client message after reconnect retries were exhausted"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}
/// Emitted by both leader guards (server mode and leader-connect) so the two sites
/// can't drift.
const PLUGIN_DIR_LEADER_WARNING: &str = "grok: --plugin-dir is ignored in leader mode; run with --no-leader to \
     load per-process plugins";
/// Run the `agent` subcommand, dispatching to the appropriate mode.
async fn run_agent_command(
    agent_args: Box<xai_grok_pager::app::AgentArgs>,
    permission_mode_flag: Option<String>,
    trust: bool,
    no_auto_update: bool,
    disable_web_search: bool,
    update_config: &UpdateConfig,
) -> Result<()> {
    let _signal_flush = tokio::spawn(async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            use xai_grok_pager::app::signal_handler::next_signal_code;
            let mut term = signal(SignalKind::terminate()).ok();
            let mut hup = signal(SignalKind::hangup()).ok();
            let code = next_signal_code(&mut term, &mut hup).await;
            shutdown_and_flush_telemetry(code);
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_ok() {
                shutdown_and_flush_telemetry(130);
            }
        }
    });
    if matches!(
        agent_args.mode,
        Some(AgentCmd::Leader(_) | AgentCmd::Stdio | AgentCmd::Headless(_) | AgentCmd::Serve(_))
    ) {
        xai_grok_shell::agent::app::suppress_otel();
    }
    init_tracing_simple("agent");
    let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
    xai_grok_telemetry::instrumentation::install_panic_hook();
    if trust {
        match std::env::current_dir() {
            Ok(cwd) => xai_grok_shell::agent::folder_trust::grant_folder_trust(&cwd),
            Err(e) => {
                tracing::warn!(error = %e, "--trust: failed to resolve cwd; folder not trusted")
            }
        }
    }
    let early_prefetch = xai_grok_shell::agent::models::start_early_prefetch(None);
    xai_grok_shell::agent::mvp_agent::warm_async_http_client();
    let is_stdio = matches!(agent_args.mode, Some(AgentCmd::Stdio));
    let is_leader = matches!(agent_args.mode, Some(AgentCmd::Leader(_)));
    if !is_stdio && !is_leader {
        eprintln!(
            "Grok Build (pager) - v{}",
            xai_grok_version::display_version_with_commit(
                env!("VERSION_WITH_COMMIT"),
                xai_grok_update::channel_label(),
            )
        );
        if should_check_for_updates(no_auto_update) {
            auto_update::run_update_if_available(
                auto_update::UpdateRunMode::NonBlocking,
                false,
                auto_update::CliUpdateTrigger::AutoBackground,
                update_config,
            )
            .await
            .ok();
        }
    }
    let remote_settings = join_early_prefetch(early_prefetch);
    xai_grok_shell::util::config::set_remote_campaigns_from_settings(remote_settings.as_ref());
    let raw_config = xai_grok_shell::config::load_effective_config()
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    let mut agent_config = AgentConfig::new_from_toml_cfg(&raw_config)
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {}", e))?;
    agent_config.default_model_override = agent_args.model.clone();
    agent_config.reasoning_effort_override = agent_args
        .reasoning_effort
        .as_deref()
        .and_then(xai_grok_shell::sampling::types::parse_canonical_effort_token);
    let launch_yolo = xai_grok_shell::util::config::effective_yolo_for_launch(
        agent_args.yolo,
        permission_mode_flag.as_deref(),
        None,
    );
    if let Some(warning) = launch_yolo.blocked_warning {
        eprintln!("grok: {warning}");
    }
    agent_config.default_yolo_mode = launch_yolo.yolo;
    agent_config.default_auto_mode = xai_grok_shell::util::config::effective_auto_for_launch(
        agent_args.yolo,
        permission_mode_flag.as_deref(),
        None,
        xai_grok_shell::util::config::PermissionMode::Ask,
    );
    agent_config.agent_profile_path = agent_args
        .agent_profile
        .as_deref()
        .map(resolve_agent_profile_path);
    agent_config.client_version = Some(PAGER_CLIENT_VERSION.to_string());
    if is_leader && !agent_args.plugin_dirs.is_empty() {
        eprintln!("{PLUGIN_DIR_LEADER_WARNING}");
    } else {
        agent_config.plugins.cli_plugin_dirs = agent_args.canonical_plugin_dirs();
    }
    apply_agent_endpoint_args(&agent_args, &mut agent_config);
    agent_config.remote_settings = remote_settings.clone();
    agent_config.resolve_runtime_fields(&xai_grok_shell::agent::config::RuntimeResolutionContext {
        raw_config: &raw_config,
        remote_settings: remote_settings.as_ref(),
        is_headless: !is_leader,
        cli_subagents: None,
        cli_web_search_model: None,
        cli_session_summary_model: None,
        memory_enabled_override: None,
        disable_web_search,
        todo_gate: false,
        laziness_debug_log: None,
        storage_mode: None,
    });
    let agent_memory_config = agent_config.memory_config.clone();
    let leader_eligible = matches!(
        &agent_args.mode,
        None | Some(AgentCmd::Stdio) | Some(AgentCmd::Headless(_))
    );
    let requested_confinement = xai_grok_sandbox::requested_confinement_profile();
    let LeaderMode {
        use_leader,
        policy_disable_reason,
        disabled_by_confinement,
    } = resolve_leader_mode(
        agent_args.leader,
        agent_args.no_leader,
        &raw_config,
        remote_settings.as_ref(),
        leader_eligible,
        requested_confinement,
    );
    tracing::info!(
        use_leader,
        ?policy_disable_reason,
        sandbox_profile = ?requested_confinement,
        leader_disabled_by_sandbox = disabled_by_confinement.is_some(),
        "leader mode resolved"
    );
    if let Some(profile) = disabled_by_confinement {
        warn_leader_disabled_by_sandbox(profile);
    }
    use xai_grok_telemetry::process_info::LeaderMode::{Attached, Standalone};
    set_identity(ProcessIdentity {
        entrypoint: match &agent_args.mode {
            Some(AgentCmd::Stdio) => Entrypoint::Embedded,
            Some(AgentCmd::Leader(_)) => Entrypoint::Leader,
            Some(AgentCmd::Serve(_)) => Entrypoint::Workspace,
            Some(AgentCmd::Headless(_)) | None => Entrypoint::Headless,
        },
        leader: if use_leader || matches!(agent_args.mode, Some(AgentCmd::Leader(_))) {
            Attached
        } else {
            Standalone
        },
        interactivity: Interactivity::Unattended,
    });
    let managed_install = is_managed_install(
        std::env::current_exe().ok(),
        &auto_update::managed_application(),
    );
    if stdio_auto_update_enabled(
        is_stdio,
        use_leader,
        should_check_for_updates(no_auto_update),
        managed_install,
    ) {
        let update_config = update_config.clone();
        tokio::spawn(async move {
            auto_update::run_update_if_available(
                auto_update::UpdateRunMode::NonBlocking,
                false,
                auto_update::CliUpdateTrigger::AutoBackground,
                &update_config,
            )
            .await
            .ok();
        });
    } else if is_stdio && !use_leader && !managed_install {
        tracing::debug!("stdio auto-update skipped: not the managed install");
    }
    if use_leader {
        if !agent_args.plugin_dirs.is_empty() {
            eprintln!("{PLUGIN_DIR_LEADER_WARNING}");
        }
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt;
        use tokio::sync::Mutex as TokioMutex;
        use xai_grok_shell::leader::{
            ClientCapabilities, ClientMode, LeaderReconnector, ReconnectPolicy, connect_or_spawn,
        };
        let mode = match &agent_args.mode {
            Some(AgentCmd::Stdio) => ClientMode::Stdio,
            Some(AgentCmd::Headless(_)) | None => ClientMode::Headless,
            _ => ClientMode::Stdio,
        };
        let env_urls = xai_grok_shell::leader::LeaderEnvUrls::from(&agent_config.grok_com_config);
        let default_model = agent_config
            .default_model_override
            .clone()
            .or(agent_config.models.default.clone());
        let client_type = std::env::args().collect::<Vec<_>>().join(" ");
        let capabilities = ClientCapabilities {
            yolo_mode: launch_yolo.yolo,
            auto_mode: agent_config.default_auto_mode && !launch_yolo.yolo,
            default_model,
            client_version: Some(PAGER_CLIENT_VERSION.to_string()),
            code_nav_enabled: false,
            terminal: false,
            fs_read: false,
            fs_write: false,
            status_line: false,
        };
        let conn = connect_or_spawn(&client_type, mode, &env_urls, capabilities.clone()).await?;
        let (tx, rx) = conn.into_channels();
        let (status_tx, _status_rx) = LeaderReconnector::status_channel();
        let reconnector = LeaderReconnector::new(
            &client_type,
            mode,
            env_urls.clone(),
            capabilities,
            status_tx,
        );
        let cancel = CancellationToken::new();
        match mode {
            ClientMode::Stdio => {
                if let Err(error) = xai_tty_utils::kill_current_process_on_parent_death() {
                    tracing::warn!(
                        %error,
                        "failed to bind to parent death; stdio bridge will not die \
                         with its parent — stdin EOF remains the only cleanup"
                    );
                }
                let replay_state = Arc::new(std::sync::Mutex::new(StdioReplayState::default()));
                let leader_tx = Arc::new(TokioMutex::new(tx));
                let leader_tx_stdin = leader_tx.clone();
                let replay_state_stdin = replay_state.clone();
                let cancel_stdin = cancel.clone();
                let stdin_task = tokio::spawn(async move {
                    let mut stdin_lines = xai_acp_lib::spawn_stdin_line_reader();
                    loop {
                        tokio::select! {
                            biased;
                            _ = cancel_stdin.cancelled() => break,
                            maybe_line = stdin_lines.recv() => {
                                let Some(line) = maybe_line else { break };
                                forward_stdio_line_to_leader(
                                    line,
                                    &leader_tx_stdin,
                                    &replay_state_stdin,
                                    &cancel_stdin,
                                )
                                .await;
                            }
                        }
                    }
                });
                let cancel_stdout = cancel.clone();
                let stdout_task = tokio::spawn(async move {
                    let mut stdout = tokio::io::stdout();
                    let mut rx = rx;
                    loop {
                        match rx.recv().await {
                            Some(ref msg) => {
                                if msg.contains("\"sessionId\"") || msg.contains("\"session_id\"") {
                                    cache_incoming_session_id(msg, &replay_state);
                                }
                                if stdout.write_all(msg.as_bytes()).await.is_err()
                                    || stdout.write_all(b"\n").await.is_err()
                                    || stdout.flush().await.is_err()
                                {
                                    break;
                                }
                            }
                            None => {
                                tracing::warn!(
                                    "Leader disconnected (stdio), attempting reconnect..."
                                );
                                match reconnector
                                    .reconnect(ReconnectPolicy::bounded(), &cancel_stdout)
                                    .await
                                {
                                    Ok((new_tx, mut new_rx, _disconnect_rx)) => {
                                        tracing::info!("Reconnected to leader (stdio)");
                                        let replayed_session_id = {
                                            let mut tx_guard = leader_tx.lock().await;
                                            *tx_guard = new_tx;
                                            let state = replay_state
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .clone();
                                            replay_acp_state_after_reconnect(
                                                &tx_guard,
                                                &mut new_rx,
                                                &mut stdout,
                                                &state,
                                            )
                                            .await
                                        };
                                        rx = new_rx;
                                        reconnector.notify_connected();
                                        let params = match replayed_session_id {
                                            Some(ref sid) => {
                                                serde_json::json!({ "sessionId": sid }).to_string()
                                            }
                                            None => "{}".to_string(),
                                        };
                                        let notification = format!(
                                            r#"{{"jsonrpc":"2.0","method":"x.ai/leader_reconnected","params":{params}}}"#
                                        );
                                        let _ = stdout.write_all(notification.as_bytes()).await;
                                        let _ = stdout.write_all(b"\n").await;
                                        let _ = stdout.flush().await;
                                        continue;
                                    }
                                    Err(e) => {
                                        tracing::error!(error = %e, "Failed to reconnect (stdio)");
                                        cancel_stdout.cancel();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                });
                tokio::select! {
                    _ = stdin_task => {}
                    _ = stdout_task => {}
                }
                return Ok(());
            }
            ClientMode::Headless => {
                drop(tx);
                let mut rx = rx;
                loop {
                    match rx.recv().await {
                        Some(_) => continue,
                        None => {
                            tracing::warn!(
                                "Leader disconnected (headless), attempting reconnect..."
                            );
                            match reconnector
                                .reconnect(ReconnectPolicy::bounded(), &cancel)
                                .await
                            {
                                Ok((_new_tx, new_rx, _disconnect_rx)) => {
                                    tracing::info!("Reconnected to leader (headless)");
                                    rx = new_rx;
                                    reconnector.notify_connected();
                                    continue;
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "Failed to reconnect (headless)");
                                    break;
                                }
                            }
                        }
                    }
                }
                return Ok(());
            }
        }
    }
    match agent_args.mode {
        Some(AgentCmd::Stdio) => run_stdio_agent(&agent_config, None, agent_memory_config).await,
        Some(AgentCmd::Headless(a)) => {
            let mut agent_config = agent_config.clone();
            apply_headless_args_to_config(&a, &mut agent_config);
            run_headless(
                &agent_config,
                agent_args.reauthenticate,
                agent_memory_config,
            )
            .await
        }
        Some(AgentCmd::Serve(a)) => {
            let mut agent_config = agent_config.clone();
            apply_headless_args_to_config(&a.headless, &mut agent_config);
            let secret = a.get_secret();
            let server_config = xai_grok_shell::agent::ServerConfig {
                bind_addr: a.bind,
                secret: secret.clone(),
            };
            print_serve_startup_info(a.bind, &secret);
            xai_grok_shell::agent::run_agent_server(server_config, agent_config).await
        }
        Some(AgentCmd::Leader(a)) => {
            let mut agent_config = agent_config.clone();
            apply_headless_args_to_config(&a.headless, &mut agent_config);
            let leader_auto_update = if !should_check_for_updates(
                no_auto_update || a.no_auto_update,
            ) {
                tracing::info!("Leader auto-update disabled");
                None
            } else {
                let update_config_for_leader = update_config.clone();
                Some(xai_grok_shell::agent::app::LeaderAutoUpdateConfig {
                    check_interval: std::time::Duration::from_secs(60 * 60),
                    check_fn: Box::new(move || {
                        let uc = update_config_for_leader.clone();
                        Box::pin(async move {
                            let current_config = xai_grok_shell::util::config::load_config().await;
                            if current_config.cli.auto_update == Some(false) {
                                return false;
                            }
                            match auto_update::ensure_latest_on_disk(&uc).await {
                                Ok(outcome) => {
                                    if let Some(v) = &outcome.installed {
                                        if let Err(e) = xai_grok_shell::managed_config::sync().await
                                        {
                                            tracing::warn!(
                                                "Leader auto-update: managed config refresh failed: {e}"
                                            );
                                        }
                                        tracing::info!(
                                            "Leader auto-update: v{v} installed successfully"
                                        );
                                    } else if outcome.relaunch_needed {
                                        tracing::info!(
                                            "Leader auto-update: newer binary already on disk, \
                                             relaunching without download"
                                        );
                                    }
                                    outcome.relaunch_needed
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Leader auto-update: check/download failed, \
                                         staying alive: {e:#}"
                                    );
                                    false
                                }
                            }
                        })
                    }),
                })
            };
            run_leader(
                &agent_config,
                a.no_exit_on_disconnect,
                a.relay_on_demand,
                leader_auto_update,
                agent_memory_config,
            )
            .await
        }
        None => {
            let mut agent_config = agent_config.clone();
            apply_headless_args_to_config(&agent_args.headless, &mut agent_config);
            run_headless(
                &agent_config,
                agent_args.reauthenticate,
                agent_memory_config,
            )
            .await
        }
    }
}
/// Raise the per-process fd soft limit toward the hard limit.
///
/// Default soft limits (256 macOS, commonly 1024 Linux) are easily exceeded:
/// each session thread's runtime costs ~3 fds, and a wide parallel subagent
/// wave adds spawn-burst transients. Targets 65536 on Linux and 8192 on macOS.
///
/// Best-effort: never blocks startup (containers/cgroups may pin limits).
#[cfg(unix)]
fn raise_fd_limit() {
    #[cfg(target_os = "macos")]
    const TARGET: libc::rlim_t = 8192;
    #[cfg(not(target_os = "macos"))]
    const TARGET: libc::rlim_t = 65536;
    unsafe {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return;
        }
        let new_cur = rlim.rlim_max.min(TARGET);
        if new_cur <= rlim.rlim_cur {
            return;
        }
        let old = rlim.rlim_cur;
        rlim.rlim_cur = new_cur;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
            tracing::trace!(old, new = new_cur, "raised RLIMIT_NOFILE");
        }
    }
}
#[cfg(not(unix))]
fn raise_fd_limit() {}
/// Consume the bare `dashboard` soft-subcommand and mark the terminal
/// dashboard for startup. `dashboard --web` is a real command and deliberately
/// remains in `args.command` for the async dispatcher below.
fn flag_dashboard_at_startup_if_requested(args: &mut PagerArgs) -> Result<()> {
    let Some(Command::Dashboard(dashboard_args)) = &args.command else {
        return Ok(());
    };
    if dashboard_args.web {
        return Ok(());
    }
    if !xai_grok_pager::views::dashboard::dashboard_enabled() {
        anyhow::bail!(
            "the Agent Dashboard is disabled. Enable it by removing \
             `[dashboard] enabled = false` from ~/.grok/config.toml and \
             unsetting GROK_AGENT_DASHBOARD=0."
        );
    }
    args.command = None;
    unsafe { std::env::set_var("GROK_OPEN_DASHBOARD_AT_STARTUP", "1") };
    Ok(())
}
const RUNTIME_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
const GROK_WORKER_THREADS_ENV: &str = "GROK_WORKER_THREADS";
/// tokio defaults to one worker per logical CPU. On a host with hundreds of
/// CPUs that can exhaust a cgroup thread budget at startup and abort under
/// `panic = "abort"`. A terminal UI is I/O-bound, so cap at 8.
const DEFAULT_MAX_WORKER_THREADS: NonZeroUsize = NonZeroUsize::new(8).unwrap();
/// How `GROK_WORKER_THREADS` resolved.
#[derive(Debug, PartialEq, Eq)]
enum WorkerCount {
    Accepted(NonZeroUsize),
    Clamped {
        requested: i128,
        used: NonZeroUsize,
        cores: NonZeroUsize,
    },
    Ignored {
        value: String,
        used: NonZeroUsize,
    },
}
impl WorkerCount {
    fn used(&self) -> NonZeroUsize {
        match self {
            Self::Accepted(used) | Self::Clamped { used, .. } | Self::Ignored { used, .. } => *used,
        }
    }
    fn notice(&self) -> Option<String> {
        match self {
            Self::Accepted(_) => None,
            Self::Clamped {
                requested,
                used,
                cores,
            } => Some(format!(
                "grok: clamped {GROK_WORKER_THREADS_ENV}={requested} to {used} (valid range is 1..={cores})"
            )),
            Self::Ignored { value, .. } => Some(format!(
                "grok: ignoring {GROK_WORKER_THREADS_ENV}={value:?} (not a valid integer)"
            )),
        }
    }
}
fn cli_worker_threads() -> NonZeroUsize {
    let cores = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
    let resolved = match std::env::var(GROK_WORKER_THREADS_ENV) {
        Ok(value) => worker_threads_from(Some(&value), cores),
        Err(std::env::VarError::NotPresent) => worker_threads_from(None, cores),
        Err(std::env::VarError::NotUnicode(value)) => WorkerCount::Ignored {
            value: value.to_string_lossy().into_owned(),
            used: default_worker_threads(cores),
        },
    };
    if let Some(notice) = resolved.notice() {
        eprintln!("{notice}");
    }
    resolved.used()
}
fn worker_threads_from(env_override: Option<&str>, cores: NonZeroUsize) -> WorkerCount {
    match env_override {
        Some(value) => resolve_worker_override(value, cores),
        None => WorkerCount::Accepted(default_worker_threads(cores)),
    }
}
fn default_worker_threads(cores: NonZeroUsize) -> NonZeroUsize {
    cores.min(DEFAULT_MAX_WORKER_THREADS)
}
fn resolve_worker_override(value: &str, cores: NonZeroUsize) -> WorkerCount {
    let Ok(requested) = value.trim().parse::<i128>() else {
        return WorkerCount::Ignored {
            value: value.to_owned(),
            used: default_worker_threads(cores),
        };
    };
    let clamped = requested.clamp(1, cores.get() as i128) as usize;
    let used = NonZeroUsize::new(clamped).expect("clamp floor of 1 guarantees non-zero");
    if requested == used.get() as i128 {
        WorkerCount::Accepted(used)
    } else {
        WorkerCount::Clamped {
            requested,
            used,
            cores,
        }
    }
}
/// A plain runtime drop blocks forever on an uncancellable in-flight blocking
/// task; `shutdown_timeout` abandons it after `grace` so exit can't hang.
fn run_and_shutdown<F: std::future::Future>(
    runtime: tokio::runtime::Runtime,
    fut: F,
    grace: std::time::Duration,
) -> F::Output {
    let output = runtime.block_on(fut);
    runtime.shutdown_timeout(grace);
    output
}
/// Return freed-but-retained jemalloc pages to the OS.
///
/// `arena.<MALLCTL_ARENAS_ALL>.purge` madvises away all dirty/muzzy pages in
/// every arena. The pager invokes this (via the `memory_release` seam) right
/// after known memory cliffs — e.g. dropping a session load's replay
/// transient — so a long-session resume doesn't leave hundreds of MB of dead
/// pages counted against the process for its lifetime (macOS keeps
/// `MADV_FREE`d pages in RSS until systemwide pressure).
#[cfg(all(feature = "jemalloc", unix))]
fn purge_jemalloc_retained_pages() {
    static NAME: &[u8] = b"arena.4096.purge\0";
    let ret = unsafe {
        tikv_jemalloc_sys::mallctl(
            NAME.as_ptr().cast(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        static WARN_ONCE: std::sync::Once = std::sync::Once::new();
        WARN_ONCE.call_once(|| {
            tracing::warn!(
                errno = ret,
                "jemalloc arena purge mallctl failed; retained-page release is inert"
            );
        });
    }
}
/// Allocator gauges for the memory trace (`memory_trace` seam): advance the
/// jemalloc epoch so the `stats.*` reads are current, then read each gauge.
/// Returns `None` if any mallctl fails (trace records the absence). Rides
/// the `tikv-jemalloc-ctl` raw helpers (introduced by the heap-profile
/// hooks below) instead of hand-rolled mallctl.
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_allocator_stats() -> Option<xai_grok_pager::memory_trace::AllocatorStats> {
    /// SAFETY: callers pass fixed NUL-terminated `stats.*` size_t ctl names.
    unsafe fn gauge(name: &[u8]) -> Option<u64> {
        unsafe {
            tikv_jemalloc_ctl::raw::read::<usize>(name)
                .ok()
                .map(|v| v as u64)
        }
    }
    unsafe {
        tikv_jemalloc_ctl::raw::write(b"epoch\0", 1u64).ok()?;
        Some(xai_grok_pager::memory_trace::AllocatorStats {
            allocated: gauge(b"stats.allocated\0")?,
            active: gauge(b"stats.active\0")?,
            resident: gauge(b"stats.resident\0")?,
            mapped: gauge(b"stats.mapped\0")?,
            retained: gauge(b"stats.retained\0")?,
            metadata: gauge(b"stats.metadata\0")?,
        })
    }
}
/// Full jemalloc statistics dump for threshold snapshots
/// (`malloc_stats_print` default human-readable format, arena detail
/// included) — the artifact the GCS memory-trace upload ships for offline
/// analysis. Raw `tikv_jemalloc_sys` because jemalloc-ctl has no
/// callback-form stats_print.
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_stats_dump() -> String {
    unsafe extern "C" fn append(opaque: *mut std::ffi::c_void, msg: *const std::ffi::c_char) {
        unsafe {
            let out = &mut *opaque.cast::<String>();
            out.push_str(&std::ffi::CStr::from_ptr(msg).to_string_lossy());
        }
    }
    let mut out = String::new();
    unsafe {
        tikv_jemalloc_sys::malloc_stats_print(
            Some(append),
            (&raw mut out).cast(),
            std::ptr::null(),
        );
    }
    out
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_heap_stats() -> Option<xai_grok_shell::heap_profile::JemallocStats> {
    unsafe {
        tikv_jemalloc_ctl::raw::write(b"epoch\0", 1u64).ok()?;
        let allocated = tikv_jemalloc_ctl::raw::read::<usize>(b"stats.allocated\0").ok()? as u64;
        let resident = tikv_jemalloc_ctl::raw::read::<usize>(b"stats.resident\0").ok()? as u64;
        Some(xai_grok_shell::heap_profile::JemallocStats {
            allocated,
            resident,
        })
    }
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_set_prof_active(active: bool) -> bool {
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.active\0", active).is_ok() }
}
#[cfg(all(test, feature = "jemalloc", unix))]
fn jemalloc_read_prof_active() -> Option<bool> {
    unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"prof.active\0").ok() }
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_prof_available() -> bool {
    unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"opt.prof\0").unwrap_or(false) }
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_dump_to_path(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    if !jemalloc_prof_available() {
        return Err("opt.prof false".into());
    }
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", c.as_ptr()) }.map_err(|e| e.to_string())
}
#[cfg(all(feature = "jemalloc", unix))]
fn install_heap_profile_hooks() {
    xai_grok_shell::heap_profile::install(xai_grok_shell::heap_profile::HeapProfileHooks {
        stats: jemalloc_heap_stats,
        set_prof_active: jemalloc_set_prof_active,
        dump_to_path: jemalloc_dump_to_path,
        prof_available: jemalloc_prof_available,
    });
}
fn version_text(channel_label: &str) -> String {
    // Community Hyper builds ship the `hyper` binary; keep the official
    // product name only when this crate is built without `community-build`.
    let product = if cfg!(feature = "community-build") {
        "hyper"
    } else {
        "grok"
    };
    format!(
        "{product} {}\n",
        xai_grok_version::display_version_with_commit(env!("VERSION_WITH_COMMIT"), channel_label,)
    )
}
fn write_version(writer: &mut impl std::io::Write, channel_label: &str) -> std::io::Result<()> {
    writer.write_all(version_text(channel_label).as_bytes())
}
fn dispatch_version_if_requested(args: &PagerArgs) -> bool {
    if !args.version {
        return false;
    }
    if let Err(error) = write_version(
        &mut std::io::stdout().lock(),
        xai_grok_update::channel_label(),
    ) {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
    true
}
fn dispatch_doctor_if_requested(args: &PagerArgs) -> bool {
    let Some(Command::Doctor(doctor_args)) = &args.command else {
        return false;
    };
    if let Err(error) = xai_grok_pager::doctor_cmd::run(doctor_args.clone()) {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
    true
}

fn run_bedrock_cli_login(profile: Option<&str>, chain: bool) -> anyhow::Result<()> {
    let home = xai_grok_config::grok_home();
    if let Some(profile) = profile.map(str::trim).filter(|p| !p.is_empty()) {
        xai_grok_shell::auth::store_bedrock_profile(&home, profile)
            .map_err(|e| anyhow::anyhow!("Failed to save Amazon Bedrock profile: {e}"))?;
        println!("✓ Amazon Bedrock will use AWS profile '{profile}'");
        return Ok(());
    }
    if chain {
        xai_grok_shell::auth::store_bedrock_credential_chain(&home).map_err(|e| {
            anyhow::anyhow!("Failed to save Amazon Bedrock credential-chain marker: {e}")
        })?;
        println!("✓ Amazon Bedrock will use the existing AWS SDK credential chain");
        return Ok(());
    }
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "Amazon Bedrock login needs an interactive terminal for bearer-token entry. \
             For AWS credentials run `grok login --bedrock --profile <name>` or \
             `grok login --bedrock --chain`."
        );
    }
    println!("Amazon Bedrock login:");
    println!("  1) paste a Bedrock bearer token (stored securely as a platform key), or");
    println!(
        "  2) submit an empty token and rerun with --profile <name> or --chain for AWS credentials."
    );
    let token = rpassword::prompt_password("Bearer token: ")?;
    let token = token.trim();
    if token.is_empty() {
        anyhow::bail!(
            "No token entered. Use `grok login --bedrock --profile <name>` or `grok login --bedrock --chain`."
        );
    }
    xai_grok_shell::auth::store_platform_api_key(&home, "amazon-bedrock", token, None)
        .map_err(|e| anyhow::anyhow!("Failed to save Amazon Bedrock bearer token: {e}"))?;
    println!("✓ Amazon Bedrock bearer token saved to ~/.grok/auth.json");
    Ok(())
}

fn main() {
    xai_grok_version::set_full_version(env!("VERSION_WITH_COMMIT"));
    xai_grok_telemetry::startup::mark_process_start();
    if let Some(code) = xai_grok_pager::app::mermaid_worker::maybe_run_render_subprocess() {
        std::process::exit(code);
    }
    if let Some(code) = xai_grok_pager::voice::maybe_run_capture_subprocess() {
        std::process::exit(code);
    }
    set_release_channel(ReleaseChannel::from_label(
        xai_grok_update::channel_name().unwrap_or_default(),
    ));
    let args = PagerArgs::parse_cli();
    if dispatch_version_if_requested(&args) || dispatch_doctor_if_requested(&args) {
        return;
    }
    if let Some(Command::Dashboard(dashboard_args)) = &args.command
        && dashboard_args.web
    {
        if let Err(error) = run_web_dashboard(dashboard_args) {
            eprintln!("Error: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    xai_grok_pager_minimal::install();
    #[cfg(all(feature = "jemalloc", unix))]
    xai_grok_pager::memory_release::install_release_hook(purge_jemalloc_retained_pages);
    #[cfg(all(feature = "jemalloc", unix))]
    {
        xai_grok_pager::memory_trace::install_allocator_stats_provider(jemalloc_allocator_stats);
        xai_grok_pager::memory_trace::install_allocator_dump_provider(jemalloc_stats_dump);
    }
    #[cfg(all(feature = "jemalloc", unix))]
    install_heap_profile_hooks();
    xai_grok_pager::memory_trace::start(xai_grok_pager::memory_trace::default_dir());
    raise_fd_limit();
    if let Err(e) = xai_grok_config::validate_requirements() {
        eprintln!("Couldn't start Grok: {e}");
        eprintln!();
        eprintln!(
            "Update Grok to a version the policy allows, or ask your administrator \
             to fix the managed requirements."
        );
        std::process::exit(2);
    }
    let _sentry_guard = xai_grok_telemetry::sentry::init(xai_grok_telemetry::sentry::Config {
        client: "grok-pager",
        client_version: PAGER_CLIENT_VERSION,
        release: env!("VERSION_WITH_COMMIT"),
        disabled: xai_grok_shell::agent::config::is_error_reporting_disabled_sync(),
    });
    xai_grok_pager::docs::extract_user_guide_docs(&xai_grok_shell::util::grok_home::grok_home());
    xai_crash_handler::install_terminal_restore_only();
    if xai_grok_shell::util::config::load_crash_handler_enabled_sync() {
        let crash_dir = xai_grok_shell::util::grok_home::grok_home().join("crash");
        if let Some(report) = xai_crash_handler::check_previous_crash(&crash_dir) {
            eprintln!("Grok crashed during your last session.");
            eprintln!("  Signal:  {}", report.signal_name);
            eprintln!("  Version: {}", report.app_version);
            eprintln!("  Report:  {}", report.report_path.display());
            eprintln!();
        }
        if !xai_crash_handler::install(xai_crash_handler::CrashHandlerConfig {
            app_version: env!("VERSION_WITH_COMMIT").to_string(),
            crash_dir: crash_dir.clone(),
        }) {
            eprintln!(
                "warning: crash handler enabled but failed to install (check permissions on {})",
                crash_dir.display()
            );
        }
    }
    let crashed = xai_grok_active_sessions::collect_crashed().unwrap_or_default();
    if !crashed.is_empty() {
        tracing::info!(
            count = crashed.len(),
            "Found crashed sessions from a previous run"
        );
    }
    let workers = cli_worker_threads();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(workers.get()).enable_all();
    let runtime =
        xai_tty_utils::runtime::build_with_blocking_pool(&mut builder).unwrap_or_else(|e| {
            eprintln!("grok: failed to start tokio runtime: {e}");
            shutdown_and_flush_telemetry(1);
        });
    let result = run_and_shutdown(runtime, async_main(args), RUNTIME_SHUTDOWN_GRACE);
    xai_grok_telemetry::debug_log::flush();
    if let Err(e) = result {
        xai_tty_utils::restore_native_stderr();
        match e.downcast_ref::<xai_grok_pager::app::StartupFailure>() {
            Some(startup) => eprintln!("{}", startup.user_report()),
            None => eprintln!("Error: {e:#}"),
        }
        drop(_sentry_guard);
        std::process::exit(1);
    }
}
async fn async_main(args: PagerArgs) -> Result<()> {
    // Record the main runtime handle so sync call sites with no runtime
    // context (e.g. the Kimi bearer resolver on a plain std thread) can run
    // their async work on the main runtime, where the shared reqwest client
    // lives and `tokio::time::timeout` therefore fires.
    xai_grok_shell::main_runtime::set_main_runtime_handle(&tokio::runtime::Handle::current());
    xai_grok_extra_ca::ensure_default_crypto_provider();
    let mut args = args.apply_cwd()?;
    if let Some(ref mode) = args.compaction_mode {
        unsafe { std::env::set_var("GROK_COMPACTION_MODE", mode) };
    }
    if let Some(ref detail) = args.compaction_detail {
        unsafe { std::env::set_var("GROK_COMPACTION_DETAIL", detail) };
    }
    if args.chat() {
        unsafe {
            std::env::set_var(xai_grok_shell::agent::chat_modes::GROK_CHAT_MODE_ENV, "1");
        }
    }
    if let Some(ref socket) = args.leader_socket {
        unsafe { std::env::set_var(xai_grok_shell::leader::LEADER_SOCKET_ENV, socket) };
    }
    if let Some(ref path) = args.debug_file {
        unsafe {
            std::env::set_var("GROK_DEBUG_LOG", path);
            std::env::remove_var("GROK_LOG_FILE");
        }
    }
    if args.debug || args.debug_file.is_some() {
        let set_if_unset = |k: &str, v: &str| {
            if std::env::var_os(k).is_none() {
                unsafe { std::env::set_var(k, v) };
            }
        };
        set_if_unset("GROK_DEBUG_LOG", "1");
        set_if_unset("GROK_HOOKS_LOG", "1");
    }
    if let Some(Command::Completions { shell }) = &args.command {
        xai_grok_pager::completions_cmd::run(*shell);
        return Ok(());
    }
    if let Some(Command::Wrap(ref wrap_args)) = args.command {
        return xai_grok_pager::wrap_cmd::run(wrap_args);
    }
    args.pin_local_resume_target()?;
    let saved_profile = args.saved_resume_profile();
    let sandbox_profile_arg = match args.startup_sandbox_profile(saved_profile.as_deref()) {
        xai_grok_pager::app::cli::SandboxStartup::Apply(profile) => profile,
        xai_grok_pager::app::cli::SandboxStartup::Conflict { requested, saved } => {
            eprintln!(
                "error: cannot resume this session under sandbox profile '{requested}' — \
                 it was created with '{saved}'. Omit --sandbox to resume with '{saved}', \
                 or start a new session to use '{requested}'."
            );
            std::process::exit(1);
        }
    };
    xai_grok_shell::config::apply_sandbox(
        None,
        sandbox_profile_arg.as_deref(),
        args.cwd.as_deref(),
    );
    // `grok codex` is a shim over the native OpenAI Codex platform: rewrite
    // the invocation into the standard pager flows (TUI / headless) pinned
    // to an `openai-codex/*` model before interactive-mode detection.
    if let Some(Command::Codex(codex_args)) = args.command.clone() {
        match xai_grok_pager::codex::rewrite_pager_args(&codex_args, &mut args).await? {
            xai_grok_pager::codex::CodexRewrite::Handled => return Ok(()),
            xai_grok_pager::codex::CodexRewrite::Continue => {}
        }
    }
    flag_dashboard_at_startup_if_requested(&mut args)?;
    let is_interactive = args.command.is_none()
        && args.single.is_none()
        && args.prompt_json.is_none()
        && args.prompt_file.is_none();
    xai_grok_shell::http::set_client_name(if is_interactive {
        xai_grok_workspace::permission::ClientType::GrokPager
    } else {
        xai_grok_workspace::permission::ClientType::Generic
    });
    if let Some(identity) = process_identity(args.command.as_ref(), is_interactive) {
        set_identity(identity);
    }
    let update_config = build_update_config();
    if let Some(command) = args.command.take() {
        match command {
            Command::Version { json } => {
                if json {
                    let payload = serde_json::json!({
                        "currentVersion": env!("VERSION_WITH_COMMIT"),
                        "channel": xai_grok_update::channel_name().unwrap_or("unknown"),
                    });
                    println!("{}", serde_json::to_string(&payload)?);
                } else {
                    write_version(
                        &mut std::io::stdout().lock(),
                        xai_grok_update::channel_label(),
                    )?;
                }
                return Ok(());
            }
            Command::Agent(agent_args) => {
                if args.leader || args.no_leader {
                    let flag = if args.leader {
                        "--leader"
                    } else {
                        "--no-leader"
                    };
                    anyhow::bail!(
                        "top-level {flag} applies to the pager TUI, not the agent subcommand. \
                         Use `grok-pager agent {flag}` instead."
                    );
                }
                enforce_version_policy_or_exit_community();
                return run_agent_command(
                    agent_args,
                    args.permission_mode_flag.clone(),
                    args.trust,
                    args.no_auto_update,
                    args.disable_web_search,
                    &update_config,
                )
                .await;
            }
            Command::Codex(_) => {
                unreachable!("grok codex is rewritten into pager args before dispatch");
            }
            Command::Doctor(_) => {
                unreachable!("doctor was consumed before runtime startup");
            }
            Command::Inspect { json } => {
                let cwd = std::env::current_dir().unwrap_or_default();
                xai_grok_shell::inspect::inspect(&cwd, json).await?;
                return Ok(());
            }
            Command::Setup { json } => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                run_setup_command(json).await;
                return Ok(());
            }
            Command::Mcp(mcp_args) => {
                init_tracing_simple("cli");
                return xai_grok_pager::mcp_cmd::run(mcp_args).await;
            }
            Command::Plugin(plugin_args) => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                return xai_grok_pager::plugin_cmd::run(plugin_args).await;
            }
            Command::Models => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                let agent_config = xai_grok_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return xai_grok_pager::models::list_available_models(&agent_config).await;
            }
            Command::Leader(leader_args) => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                return run_leader_mgmt(leader_args).await;
            }
            Command::Worktree(worktree_args) => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                let agent_config = xai_grok_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return xai_grok_pager::worktree_cmd::run(worktree_args, &agent_config).await;
            }
            Command::DiskUsage(disk_usage_args) => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                return xai_grok_pager::disk_usage_cmd::run(disk_usage_args);
            }
            Command::Workspace(workspace_args) => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                return run_workspace_mgmt(workspace_args).await;
            }
            Command::Sessions(sessions_args) => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                let agent_config = xai_grok_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return xai_grok_pager::sessions_cmd::run(sessions_args, &agent_config).await;
            }
            Command::Share(ref share_args) => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                let agent_config = xai_grok_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return xai_grok_pager::share_cmd::run(share_args, &agent_config).await;
            }
            Command::Export(export_args) => {
                init_tracing_simple("cli");
                return xai_grok_pager::export_cmd::run(export_args);
            }
            Command::Trace(trace_args) => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                let agent_config = xai_grok_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return xai_grok_pager::trace_cmd::run(trace_args, &agent_config).await;
            }
            Command::Memory(memory_args) => {
                return xai_grok_pager::memory_cmd::run(memory_args);
            }
            Command::Update {
                check,
                json,
                force_reinstall,
                version,
                alpha,
                stable,
                enterprise,
                trigger,
                auto,
            } => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                let channel_switch = get_channel_switch(alpha, stable, enterprise);
                let trigger = resolve_update_trigger(trigger.as_deref(), auto);
                return run_update_command(
                    check,
                    json,
                    force_reinstall,
                    version,
                    channel_switch,
                    trigger,
                    &update_config,
                )
                .await;
            }
            Command::Login {
                legacy: _,
                oauth,
                device_auth,
                kimi,
                openai,
                claude,
                github,
                radius,
                bedrock,
                profile,
                chain,
                devbox,
            } => {
                init_tracing_simple("cli");
                let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
                if kimi {
                    xai_grok_shell::auth::kimi::run_kimi_code_login().await?;
                } else if openai {
                    let method = if device_auth {
                        xai_grok_shell::auth::openai_codex::CodexLoginMethod::DeviceCode
                    } else {
                        xai_grok_shell::auth::openai_codex::CodexLoginMethod::Browser
                    };
                    xai_grok_shell::auth::openai_codex::run_openai_codex_login(None, method)
                        .await?;
                } else if claude {
                    xai_grok_shell::auth::anthropic_claude::run_anthropic_claude_login(None)
                        .await?;
                } else if github {
                    xai_grok_shell::auth::github_copilot::run_github_copilot_login(None).await?;
                } else if radius {
                    let method = if device_auth {
                        xai_grok_shell::auth::radius::RadiusLoginMethod::DeviceCode
                    } else {
                        xai_grok_shell::auth::radius::RadiusLoginMethod::Browser
                    };
                    xai_grok_shell::auth::radius::run_radius_login(None, method).await?;
                } else if bedrock {
                    run_bedrock_cli_login(profile.as_deref(), chain)?;
                } else {
                    let config = xai_grok_shell::config::load_agent_config_disk_only()
                        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                    xai_grok_shell::auth::run_cli_login(&config, oauth, device_auth, devbox)
                        .await?;
                }

                println!();
                xai_grok_shell::instrumentation::finalize_and_exit(0);
            }
            Command::Logout {
                kimi,
                openai,
                claude,
                github,
                radius,
                bedrock,
                all,
            } => {
                init_tracing_simple("cli");
                if all {
                    let config = xai_grok_shell::config::load_agent_config_disk_only()
                        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                    xai_grok_shell::auth::run_cli_logout_all(&config)?;
                } else if kimi {
                    xai_grok_shell::auth::run_cli_logout_kimi()?;
                } else if openai {
                    xai_grok_shell::auth::run_cli_logout_openai_codex()?;
                } else if claude {
                    xai_grok_shell::auth::run_cli_logout_anthropic_claude()?;
                } else if github {
                    xai_grok_shell::auth::run_cli_logout_github_copilot()?;
                } else if radius {
                    xai_grok_shell::auth::run_cli_logout_radius()?;
                } else if bedrock {
                    let home = xai_grok_config::grok_home();
                    xai_grok_shell::auth::clear_bedrock_auth(&home)
                        .map_err(|e| anyhow::anyhow!("Failed to clear Amazon Bedrock auth: {e}"))?;
                    println!("✓ Amazon Bedrock auth removed from ~/.grok/auth.json");
                } else {
                    let config = xai_grok_shell::config::load_agent_config_disk_only()
                        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                    xai_grok_shell::auth::run_cli_logout(&config)?;
                }
                xai_grok_shell::instrumentation::finalize_and_exit(0);
            }
            Command::Nexus {
                key,
                base_url,
                clear,
            } => {
                init_tracing_simple("cli");
                let home = xai_grok_config::grok_home();
                if clear {
                    xai_grok_shell::auth::clear_platform_api_key(&home, "nexus")
                        .map_err(|e| anyhow::anyhow!("Failed to clear Nexus key: {e}"))?;
                    println!("✓ Nexus key removed from ~/.grok/auth.json");
                } else if let Some(key) = key {
                    xai_grok_shell::auth::store_platform_api_key(
                        &home,
                        "nexus",
                        &key,
                        base_url.as_deref(),
                    )
                    .map_err(|e| anyhow::anyhow!("Failed to save Nexus key: {e}"))?;
                    println!("✓ Nexus key saved to ~/.grok/auth.json");
                    if let Some(base) = base_url.as_deref() {
                        println!("  gateway root: {base}");
                    }
                    println!("  models appear as nexus/* after launch.");
                } else {
                    // No key, no --clear → print onboarding guide + current status.
                    println!(
                        "{}",
                        xai_grok_pager::slash::commands::nexus::nexus_setup_guide()
                    );
                    match xai_grok_shell::auth::read_platform_api_key(&home, "nexus") {
                        Some(_) => {
                            let root = xai_grok_shell::auth::read_platform_base_url(&home, "nexus")
                                .unwrap_or_else(|| {
                                    xai_grok_pager::slash::commands::nexus::nexus_default_root()
                                        .to_string()
                                });
                            println!("\n当前状态: 已配置 (网关: {root})");
                        }
                        None => println!("\n当前状态: 未配置"),
                    }
                }

                xai_grok_shell::instrumentation::finalize_and_exit(0);
            }
            Command::Wrap(ref wrap_args) => {
                return xai_grok_pager::wrap_cmd::run(wrap_args);
            }
            Command::Completions { shell } => {
                xai_grok_pager::completions_cmd::run(shell);
                return Ok(());
            }
            Command::Dashboard(dashboard_args) => {
                if !dashboard_args.web {
                    unreachable!("bare dashboard is consumed before command dispatch");
                }
                init_dashboard_tracing();
                let mut config =
                    xai_grok_dashboard::DashboardServerConfig::new(xai_grok_config::grok_home());
                if let Some(bind) = dashboard_args.bind {
                    config.bind = bind;
                }
                config.open_browser = !dashboard_args.no_open;
                return xai_grok_dashboard::serve(config).await;
            }
        }
    }
    let headless_prompt = xai_grok_pager::headless::HeadlessPrompt::from_args(
        args.single.as_deref(),
        args.prompt_json.as_deref(),
        args.prompt_file.as_deref(),
    )?;
    if let Some(prompt) = headless_prompt {
        init_tracing_simple(HEADLESS_ENTRYPOINT);
        let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
        enforce_version_policy_or_exit_community();
        let launch_yolo = xai_grok_shell::util::config::effective_yolo_for_launch(
            args.yolo,
            args.permission_mode_flag.as_deref(),
            None,
        );
        if let Some(warning) = launch_yolo.blocked_warning {
            eprintln!("grok: {warning}");
        }
        let json_schema = args
            .json_schema
            .as_deref()
            .map(xai_grok_pager::headless::parse_json_schema)
            .transpose()?;
        if json_schema.is_some()
            && args.output_format == xai_grok_pager::headless::OutputFormat::Plain
        {
            args.output_format = xai_grok_pager::headless::OutputFormat::Json;
        }
        return xai_grok_pager::headless::run_single_turn(
            prompt,
            args.verbatim,
            xai_grok_pager::headless::HeadlessOptions {
                session_id: args.session_id.clone(),
                resume: args.resume_session.or(args.load_session),
                resume_title_pinned: args.resume_target_pinned,
                cwd: args.cwd,
                yolo: launch_yolo.yolo,
                trust: args.trust,
                output_format: args.output_format,
                include_partial_messages: args.include_partial_messages,
                json_schema,
                model: args.model,
                rules: args.rules,
                system_prompt_override: args.system_prompt_override.clone(),
                continue_last_session: args.continue_last_session,
                fork_session: args.fork_session,
                worktree: args.worktree,
                restore_code: args.restore_code,
                agent: args.agent.clone(),
                agents_json: args.agents_json.clone(),
                cli_tools: args.cli_tools.clone(),
                cli_disallowed_tools: args.cli_disallowed_tools.clone(),
                disable_web_search: args.disable_web_search,
                allow_rules: args.allow_rules.clone(),
                deny_rules: args.deny_rules.clone(),
                max_turns: args.max_turns,
                permission_mode_flag: args.permission_mode_flag.clone(),
                reasoning_effort: args.reasoning_effort.clone(),
                wait_for_background: !args.no_wait_for_background,
                background_wait_timeout: std::time::Duration::from_secs(
                    args.background_wait_timeout_secs,
                ),
            },
        )
        .await;
    }
    enforce_version_policy_or_exit_community();
    let _otel_guard = xai_grok_telemetry::otel_layer::otel_guard();
    type UpdateWaitHandle = tokio::task::JoinHandle<std::io::Result<std::process::ExitStatus>>;
    let bg_update_wait: std::sync::Arc<tokio::sync::Mutex<Option<UpdateWaitHandle>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let bg_update_rx: Option<tokio::sync::oneshot::Receiver<Option<auto_update::UpdateAvailable>>> =
        if should_check_for_updates(args.no_auto_update) {
            let update_config = update_config.clone();
            let wait_slot = bg_update_wait.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let check = auto_update::check_update_background(&update_config).await;
                if let Some(mut child) = check.download {
                    *wait_slot.lock().await = Some(tokio::spawn(async move { child.wait().await }));
                }
                let _ = tx.send(check.update);
            });
            Some(rx)
        } else {
            None
        };
    let result = xai_grok_pager::app::run(args, bg_update_rx).await;
    xai_grok_sandbox::flush();
    match result {
        Ok(true) => {
            let adopted = bg_update_wait.lock().await.take();
            let command = if cfg!(feature = "community-build") {
                "hyper"
            } else {
                "grok"
            };
            if finish_update_on_exit(adopted, &update_config).await {
                eprintln!("Update installed. Run `{command}` to start.");
            } else {
                eprintln!("Update did not complete. Run `{command} update` to retry.");
            }
            Ok(())
        }
        Ok(false) => Ok(()),
        Err(e) => Err(e),
    }
}
/// Complete the update after a quit-for-update (Ctrl+U) exit. Returns `true`
/// when an update path completed without a reported failure.
///
/// Prefers awaiting the parked waiter for the background `grok update` child
/// spawned at startup — the download is usually already done or in flight.
/// Only when there is no waiter (spawn failed, or no download was needed
/// because the target was already on disk) or the child failed does this
/// fall back to a fresh blocking `grok update`, which itself resolves to
/// "Already up to date" without downloading when the disk is current.
async fn finish_update_on_exit(
    adopted: Option<tokio::task::JoinHandle<std::io::Result<std::process::ExitStatus>>>,
    update_config: &UpdateConfig,
) -> bool {
    let run_blocking = |reason: Option<String>| async move {
        if let Some(reason) = reason {
            eprintln!("{reason}");
        }
        auto_update::run_update_if_available(
            auto_update::UpdateRunMode::Blocking,
            false,
            auto_update::CliUpdateTrigger::UserCommand,
            update_config,
        )
        .await
        .is_ok()
    };
    match adopted {
        Some(handle) => {
            eprintln!("Waiting for the update download to finish...");
            match handle.await {
                Ok(Ok(status)) if status.success() => true,
                Ok(Ok(status)) => {
                    run_blocking(Some(format!(
                        "Background update exited with {status}; retrying..."
                    )))
                    .await
                }
                Ok(Err(e)) => {
                    run_blocking(Some(format!(
                        "Could not wait for the background update ({e}); retrying..."
                    )))
                    .await
                }
                Err(join_err) => {
                    run_blocking(Some(format!(
                        "Background update waiter failed ({join_err}); retrying..."
                    )))
                    .await
                }
            }
        }
        None => run_blocking(None).await,
    }
}
/// Build an [`UpdateConfig`] from the current environment and config files.
fn build_update_config() -> UpdateConfig {
    let environment = xai_grok_shell::env::GrokBuildEnvironment::from_flags(false, false);
    let mut config = UpdateConfig::from_environment(&environment);
    cryptify::flow_stmt!({
        {
            config.deployment_key =
                xai_grok_shell::agent::config::EndpointsConfig::default().deployment_key;
        }
    });
    config.npm_registry = std::env::var(obfstr::obfstr!("GROK_NPM_REGISTRY"))
        .ok()
        .or_else(xai_grok_shell::util::config::load_npm_registry_sync);
    if let Ok(root) = xai_grok_shell::config::load_effective_config_disk_only()
        && let Some(ch) = xai_grok_shell::util::config::channel_from_toml_opt(&root)
    {
        config.channel = ch;
    }
    config
}
/// Central gate for auto-update checks; add new suppression rules here,
/// not at call sites.
fn should_check_for_updates(no_auto_update_flag: bool) -> bool {
    if cfg!(debug_assertions) {
        return false;
    }
    if no_auto_update_flag {
        return false;
    }
    let disabled =
        |name: &str| std::env::var_os(name).is_some_and(|v| env_flag_enabled(&v.to_string_lossy()));
    !(disabled("GROK_DISABLE_AUTOUPDATER")
        || cfg!(feature = "community-build") && disabled("HYPER_DISABLE_AUTOUPDATER"))
}
/// Gate for the stdio agent's background auto-update: only the direct stdio
/// agent, from the managed install. Other modes update in `run_agent_command`.
fn stdio_auto_update_enabled(
    is_stdio: bool,
    use_leader: bool,
    updates_enabled: bool,
    managed_install: bool,
) -> bool {
    is_stdio && !use_leader && updates_enabled && managed_install
}
/// True when `exe` is the binary the product's managed application resolves
/// to (`~/.hyper/bin/hyper` for community builds, `~/.grok/bin/grok` for
/// official builds). Both sides are canonicalized; any failure reports
/// unmanaged and skips automatic updates.
fn is_managed_install(
    exe: Option<std::path::PathBuf>,
    managed_application: &std::path::Path,
) -> bool {
    if managed_application.as_os_str().is_empty() {
        return false;
    }
    let Some(exe) = exe else {
        return false;
    };
    match (
        dunce::canonicalize(&exe),
        dunce::canonicalize(managed_application),
    ) {
        (Ok(exe), Ok(managed)) => exe == managed,
        _ => false,
    }
}
/// Map the mutually-exclusive channel flags to a channel name. clap enforces
/// that at most one is set, so the order is irrelevant.
fn get_channel_switch(alpha: bool, stable: bool, enterprise: bool) -> Option<&'static str> {
    if alpha {
        Some("alpha")
    } else if stable {
        Some("stable")
    } else if enterprise {
        Some("enterprise")
    } else {
        None
    }
}
/// Handle `grok-pager update [--check] [--json] [--force-reinstall] [--version X] [--alpha|--stable|--enterprise]`.
/// --trigger is the one representation; --auto is the compat alias from
/// older parents. Unknown values fall back to user_command (a human is the
/// only caller that can produce them).
fn resolve_update_trigger(flag: Option<&str>, auto: bool) -> auto_update::CliUpdateTrigger {
    if let Some(flag) = flag {
        match flag.parse() {
            Ok(t) => return t,
            Err(e) => tracing::warn!("{e}; recording user_command"),
        }
    }
    if auto {
        auto_update::CliUpdateTrigger::AutoBackground
    } else {
        auto_update::CliUpdateTrigger::UserCommand
    }
}
async fn run_update_command(
    check: bool,
    json: bool,
    force_reinstall: bool,
    version: Option<String>,
    channel_switch: Option<&str>,
    trigger: auto_update::CliUpdateTrigger,
    base_update_config: &UpdateConfig,
) -> Result<()> {
    if cfg!(feature = "community-build")
        && channel_switch.is_some_and(|channel| channel != "stable")
    {
        anyhow::bail!("Hyper community releases support only the stable channel");
    }
    if json && !check {
        anyhow::bail!("--json requires --check");
    }
    let mut update_config = base_update_config.clone();
    if check {
        if version.is_some() {
            anyhow::bail!("--version cannot be used with --check");
        }
        auto_update::apply_channel_switch(channel_switch, &mut update_config).await;
        let status = auto_update::check_update_status(&update_config).await;
        auto_update::print_update_status(&status, json)?;
        return Ok(());
    }
    if let Some(ref v) = version
        && semver::Version::parse(v).is_err()
    {
        anyhow::bail!(
            "'{}' is not a valid version. Expected semver like 0.1.150",
            v
        );
    }
    let telemetry_cfg = xai_grok_shell::config::load_agent_config_disk_only()
        .map_err(|e| tracing::warn!("grok update: telemetry init skipped (agent config: {e})"))
        .ok();
    if let Some(agent_cfg) = telemetry_cfg {
        let auth_manager = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
            &xai_grok_shell::util::grok_home::grok_home(),
            agent_cfg.grok_com_config.clone(),
        ));
        xai_grok_shell::agent::init::update_telemetry_config(&agent_cfg, &auth_manager);
    }
    let result = auto_update::run_update(
        force_reinstall,
        version.as_deref(),
        channel_switch,
        &mut update_config,
        trigger,
    )
    .await;
    if let Ok(Some(installed_version)) = &result {
        let allow_same_version =
            cfg!(feature = "community-build") && auto_update::running_binary_differs_from_managed();
        signal_leaders_to_relaunch(installed_version, allow_same_version).await;
    }
    xai_grok_telemetry::session_ctx::drain_pending(xai_grok_telemetry::session_ctx::CLI_DRAIN)
        .await;
    result?;
    Ok(())
}
/// After a successful product update, ask any stale running leader on this
/// machine to relaunch onto the active binary (bounded grace; running sessions
/// close and reconnect via `session/load`). Community checksum republishes may
/// request a same-semver relaunch; downgrades remain forbidden server-side.
///
/// Best-effort and non-fatal: discovery/connect/control failures are logged and
/// skipped. The leader re-checks the directional version guard authoritatively;
/// the pager-side `live_info` check just avoids connecting to newer leaders.
async fn signal_leaders_to_relaunch(installed_version: &str, allow_same_version: bool) {
    for d in xai_grok_shell::leader::discover_leaders().await {
        if d.classification != xai_grok_shell::leader::LeaderDiscoveryState::Reachable {
            continue;
        }
        let Some(socket_path) = d.socket_path.clone() else {
            continue;
        };
        if let Some(ref live) = d.live_info {
            let older = leader_is_older_than(&live.leader_binary_version, installed_version);
            let equal = matches!(
                (
                    semver::Version::parse(&live.leader_binary_version),
                    semver::Version::parse(installed_version),
                ),
                (Ok(leader), Ok(installed)) if leader == installed
            );
            if !(older || allow_same_version && equal) {
                continue;
            }
        }
        let client = match xai_grok_shell::leader::LeaderClient::connect(
            socket_path,
            "grok-pager-update",
            ClientMode::Stdio,
            ClientCapabilities::default(),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "Could not connect to leader to signal relaunch");
                continue;
            }
        };
        if !client.registration().supports_relaunch() {
            client.cancel();
            continue;
        }
        match client
            .send_control(ControlCommand::RelaunchForUpdate {
                to_version: installed_version.to_string(),
                allow_same_version,
            })
            .await
        {
            Ok(Ok(xai_grok_shell::leader::ControlPayload::Relaunching {
                from_version,
                to_version,
                ..
            })) => {
                eprintln!("  ↻ Relaunching shared session (leader {from_version} → {to_version})…");
            }
            Ok(Ok(xai_grok_shell::leader::ControlPayload::RelaunchDeclined { reason })) => {
                tracing::debug!(%reason, "Leader declined relaunch");
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::debug!(error = %e.message, "Leader relaunch control error");
            }
            Err(e) => {
                tracing::debug!(error = %e, "Leader relaunch ack not received (leader may be exiting)");
            }
        }
        client.cancel();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_caps_the_core_count() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        assert_eq!(default_worker_threads(nz(360)), DEFAULT_MAX_WORKER_THREADS);
        assert_eq!(default_worker_threads(nz(4)), nz(4));
    }
    #[test]
    fn worker_threads_from_selects_default_or_override() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let cores = nz(360);
        assert_eq!(
            worker_threads_from(None, cores),
            WorkerCount::Accepted(default_worker_threads(cores))
        );
        assert_eq!(
            worker_threads_from(Some("16"), cores),
            WorkerCount::Accepted(nz(16))
        );
    }
    #[test]
    fn override_in_range_is_used_without_a_notice() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let cores = nz(360);
        assert_eq!(
            resolve_worker_override("16", cores),
            WorkerCount::Accepted(nz(16))
        );
        assert_eq!(resolve_worker_override("16", cores).notice(), None);
        assert_eq!(resolve_worker_override(" 8 ", cores).used().get(), 8);
        assert_eq!(
            resolve_worker_override("360", cores),
            WorkerCount::Accepted(cores)
        );
    }
    #[test]
    fn override_out_of_range_is_clamped_with_a_notice() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let cores = nz(360);
        assert_eq!(
            resolve_worker_override("100000", cores),
            WorkerCount::Clamped {
                requested: 100000,
                used: cores,
                cores
            }
        );
        assert_eq!(
            resolve_worker_override("0", cores),
            WorkerCount::Clamped {
                requested: 0,
                used: nz(1),
                cores
            }
        );
        assert_eq!(
            resolve_worker_override("-1", cores),
            WorkerCount::Clamped {
                requested: -1,
                used: nz(1),
                cores
            }
        );
        assert_eq!(
            resolve_worker_override("100000", cores).notice().unwrap(),
            "grok: clamped GROK_WORKER_THREADS=100000 to 360 (valid range is 1..=360)"
        );
    }
    #[test]
    fn override_unparseable_is_ignored_with_a_notice() {
        let cores = NonZeroUsize::new(360).unwrap();
        for value in ["abc", "", "99999999999999999999999999999999999999999"] {
            let ignored = resolve_worker_override(value, cores);
            assert!(matches!(ignored, WorkerCount::Ignored { .. }), "{value}");
            assert_eq!(ignored.used(), default_worker_threads(cores), "{value}");
        }
        assert_eq!(
            resolve_worker_override("abc", cores).notice().unwrap(),
            "grok: ignoring GROK_WORKER_THREADS=\"abc\" (not a valid integer)"
        );
    }
    #[test]
    fn version_output_writer_preserves_channel_aware_contract() {
        xai_grok_version::set_full_version(env!("VERSION_WITH_COMMIT"));
        for (label, expected_suffix) in [
            (" [alpha]", " [alpha]\n"),
            (" [stable]", " [stable]\n"),
            ("", ")\n"),
        ] {
            let mut output = Vec::new();
            write_version(&mut output, label).unwrap();
            let output = String::from_utf8(output).unwrap();
            let product = if cfg!(feature = "community-build") {
                "hyper "
            } else {
                "grok "
            };
            assert!(output.starts_with(product), "{output:?}");
            assert!(output.contains(env!("VERSION_WITH_COMMIT")));
            assert!(output.ends_with(expected_suffix), "{output:?}");
        }
    }
    #[test]
    fn version_flags_and_doctor_are_distinct_early_intents() {
        let version = PagerArgs::try_parse_from(["grok", "--version"]).unwrap();
        assert!(version.version);
        assert!(version.command.is_none());
        let short = PagerArgs::try_parse_from(["grok", "-v"]).unwrap();
        assert!(short.version);
        assert!(short.command.is_none());
        let subcommand = PagerArgs::try_parse_from(["grok", "version"]).unwrap();
        assert!(!subcommand.version);
        assert!(matches!(
            subcommand.command,
            Some(Command::Version { json: false })
        ));
    }
    #[cfg(all(feature = "jemalloc", unix))]
    struct TempHeapDump(std::path::PathBuf);
    #[cfg(all(feature = "jemalloc", unix))]
    impl TempHeapDump {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "grok-jemalloc-{label}-{}-{}.heap",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn assert_nonempty_dump(&self) {
            let meta = std::fs::metadata(&self.0).expect("dump file missing after prof.dump");
            assert!(meta.len() > 0, "empty dump file");
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    impl Drop for TempHeapDump {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    fn require_opt_prof() -> bool {
        if jemalloc_prof_available() {
            return true;
        }
        eprintln!(
            "skip jemalloc prof checks: opt.prof false \
             (release-dist static conf, or MALLOC_CONF=prof:true,prof_active:false,lg_prof_sample={})",
            xai_grok_shell::heap_profile::LG_PROF_SAMPLE
        );
        false
    }
    #[cfg(all(feature = "jemalloc", unix))]
    fn assert_prof_active(expected: bool) {
        assert_eq!(jemalloc_read_prof_active(), Some(expected));
    }
    /// Restores process-global `prof.active` on drop (panic-safe for serial tests).
    #[cfg(all(feature = "jemalloc", unix))]
    struct ProfActiveGuard {
        previous: bool,
    }
    #[cfg(all(feature = "jemalloc", unix))]
    impl ProfActiveGuard {
        fn set(active: bool) -> Self {
            let previous = jemalloc_read_prof_active().unwrap_or(false);
            assert!(
                jemalloc_set_prof_active(active),
                "failed to set prof.active={active}"
            );
            Self { previous }
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    impl Drop for ProfActiveGuard {
        fn drop(&mut self) {
            let _ = jemalloc_set_prof_active(self.previous);
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    fn assert_stats_sane(stats: xai_grok_shell::heap_profile::JemallocStats) {
        assert!(stats.allocated > 0, "allocated={}", stats.allocated);
        assert!(stats.resident > 0, "resident={}", stats.resident);
        assert!(
            stats.resident >= stats.allocated,
            "resident {} < allocated {}",
            stats.resident,
            stats.allocated
        );
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn jemalloc_stats_readable_after_epoch() {
        assert_stats_sane(jemalloc_heap_stats().expect("stats readable"));
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn jemalloc_prof_active_round_trip_and_dump() {
        if !require_opt_prof() {
            return;
        }
        assert_prof_active(false);
        {
            let _guard = ProfActiveGuard::set(true);
            assert_prof_active(true);
        }
        assert_prof_active(false);
        let dump = TempHeapDump::new("direct");
        jemalloc_dump_to_path(dump.path()).expect("prof.dump");
        dump.assert_nonempty_dump();
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn jemalloc_dump_rejects_interior_nul_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;
        if !require_opt_prof() {
            return;
        }
        let path = Path::new(OsStr::from_bytes(b"/tmp/grok-jemalloc-\0.heap"));
        let err = jemalloc_dump_to_path(path).expect_err("interior NUL must fail");
        assert!(
            err.to_ascii_lowercase().contains("nul"),
            "unexpected error: {err}"
        );
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn install_heap_profile_hooks_wires_shell_apis() {
        install_heap_profile_hooks();
        assert_stats_sane(
            xai_grok_shell::heap_profile::stats().expect("shell stats after install"),
        );
        if !require_opt_prof() {
            assert!(!xai_grok_shell::heap_profile::prof_available());
            return;
        }
        assert!(xai_grok_shell::heap_profile::prof_available());
        assert_prof_active(false);
        {
            let _guard = ProfActiveGuard::set(true);
            assert_prof_active(true);
            assert!(xai_grok_shell::heap_profile::set_prof_active(true));
            assert_prof_active(true);
        }
        assert_prof_active(false);
        assert!(xai_grok_shell::heap_profile::set_prof_active(false));
        assert_prof_active(false);
        let dump = TempHeapDump::new("shell");
        xai_grok_shell::heap_profile::dump_to_path(dump.path()).expect("shell dump");
        dump.assert_nonempty_dump();
    }
    #[cfg(unix)]
    #[test]
    fn is_managed_install_matches_only_the_active_application_target() {
        let home =
            std::env::temp_dir().join(format!("grok-pager-managed-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("downloads")).unwrap();
        let managed = home.join("bin").join("hyper");
        assert!(!is_managed_install(Some(managed.clone()), &managed));
        assert!(!is_managed_install(None, &managed));
        assert!(!is_managed_install(
            Some(managed.clone()),
            std::path::Path::new("")
        ));
        let target = home.join("downloads").join("hyper-1.2.3");
        std::fs::write(&target, b"binary").unwrap();
        std::os::unix::fs::symlink(&target, &managed).unwrap();
        assert!(is_managed_install(Some(managed.clone()), &managed));
        assert!(is_managed_install(Some(target.clone()), &managed));
        let pinned = home.join("bin").join("hyper-9.9.9");
        std::fs::write(&pinned, b"binary").unwrap();
        assert!(!is_managed_install(Some(pinned), &managed));
        let _ = std::fs::remove_dir_all(&home);
    }
    /// Pins the gate composition; a dropped conjunct fails its named case.
    #[test]
    fn stdio_auto_update_requires_direct_stdio_enabled_and_managed() {
        assert!(stdio_auto_update_enabled(true, false, true, true));
        assert!(
            !stdio_auto_update_enabled(true, true, true, true),
            "leader bridge"
        );
        assert!(
            !stdio_auto_update_enabled(false, false, true, true),
            "non-stdio"
        );
        assert!(
            !stdio_auto_update_enabled(true, false, false, true),
            "updates off"
        );
        assert!(
            !stdio_auto_update_enabled(true, false, true, false),
            "pinned binary"
        );
    }
    use clap::Parser as _;
    /// `grok dashboard` flags the startup hook without forcing leader mode —
    /// the dashboard is independent of leader mode, so the launch keeps
    /// whatever leader setting the user (or config) chose.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn dashboard_subcommand_flags_startup_without_forcing_leader() {
        let mut args = PagerArgs::try_parse_from(["grok", "dashboard"]).unwrap();
        assert!(!args.leader, "fixture: no explicit --leader");
        flag_dashboard_at_startup_if_requested(&mut args).unwrap();
        assert!(!args.leader, "dashboard must NOT force leader mode");
        assert!(
            args.command.is_none(),
            "soft subcommand must be consumed so the interactive path runs",
        );
        assert_eq!(
            std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").as_deref(),
            Ok("1"),
            "startup hook flag must be set",
        );
        unsafe { std::env::remove_var("GROK_OPEN_DASHBOARD_AT_STARTUP") };
    }
    /// `dashboard --web` remains a real command for async dispatch and does
    /// not set the terminal-dashboard startup flag.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn web_dashboard_subcommand_is_not_consumed() {
        let mut args = PagerArgs::try_parse_from([
            "grok",
            "dashboard",
            "--web",
            "--bind",
            "127.0.0.1:9191",
            "--no-open",
        ])
        .unwrap();
        flag_dashboard_at_startup_if_requested(&mut args).unwrap();
        let Some(Command::Dashboard(dashboard)) = args.command else {
            panic!("web dashboard command must remain for async dispatch");
        };
        assert!(dashboard.web);
        assert_eq!(dashboard.bind, Some("127.0.0.1:9191".parse().unwrap()));
        assert!(dashboard.no_open);
        assert!(std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").is_err());
    }
    /// `grok dashboard --no-leader` is allowed — the dashboard does not
    /// require a leader, so the combination launches into the dashboard in
    /// non-leader mode.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn dashboard_subcommand_allows_no_leader() {
        let mut args = PagerArgs::try_parse_from(["grok", "--no-leader", "dashboard"]).unwrap();
        flag_dashboard_at_startup_if_requested(&mut args)
            .expect("--no-leader + dashboard must be allowed");
        assert!(args.no_leader, "--no-leader must be preserved");
        assert!(!args.leader, "dashboard must not force leader mode");
        assert!(
            args.command.is_none(),
            "soft subcommand must be consumed so the interactive path runs",
        );
        assert_eq!(
            std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").as_deref(),
            Ok("1"),
            "startup hook flag must be set",
        );
        unsafe { std::env::remove_var("GROK_OPEN_DASHBOARD_AT_STARTUP") };
    }
    /// `GROK_AGENT_DASHBOARD=0` disables the feature — the subcommand
    /// must error visibly before the TUI starts.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn dashboard_subcommand_errors_when_disabled() {
        unsafe { std::env::set_var("GROK_AGENT_DASHBOARD", "0") };
        let mut args = PagerArgs::try_parse_from(["grok", "dashboard"]).unwrap();
        let result = flag_dashboard_at_startup_if_requested(&mut args);
        unsafe { std::env::remove_var("GROK_AGENT_DASHBOARD") };
        let err = result.expect_err("disabled dashboard must error");
        assert!(err.to_string().contains("disabled"), "got: {err}");
        assert!(
            std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").is_err(),
            "failure path must not flag the startup hook",
        );
    }
    #[test]
    fn workspace_command_gate_resolution() {
        use xai_grok_shell::util::config::RemoteSettings;
        let on = RemoteSettings {
            workspace_command_enabled: Some(true),
            ..RemoteSettings::default()
        };
        let off = RemoteSettings::default();
        assert_eq!(
            workspace_command_gate(None, Some(&on)),
            WorkspaceGate::Enabled
        );
        assert_eq!(
            workspace_command_gate(None, Some(&off)),
            WorkspaceGate::Disabled
        );
        assert_eq!(workspace_command_gate(None, None), WorkspaceGate::Unknown);
        assert_eq!(
            workspace_command_gate(Some(true), Some(&off)),
            WorkspaceGate::Enabled
        );
        assert_eq!(
            workspace_command_gate(Some(true), None),
            WorkspaceGate::Enabled
        );
        assert_eq!(
            workspace_command_gate(Some(false), Some(&on)),
            WorkspaceGate::Disabled
        );
        assert_eq!(
            workspace_command_gate(Some(false), None),
            WorkspaceGate::Disabled
        );
    }
    #[serial_test::serial(GROK_WORKSPACE_COMMAND)]
    #[test]
    fn workspace_command_env_override_parsing() {
        unsafe { std::env::remove_var("GROK_WORKSPACE_COMMAND") };
        assert_eq!(workspace_command_env_override(), None);
        unsafe { std::env::set_var("GROK_WORKSPACE_COMMAND", "1") };
        assert_eq!(workspace_command_env_override(), Some(true));
        unsafe { std::env::set_var("GROK_WORKSPACE_COMMAND", "off") };
        assert_eq!(workspace_command_env_override(), Some(false));
        unsafe { std::env::remove_var("GROK_WORKSPACE_COMMAND") };
    }
    fn make_state() -> std::sync::Mutex<StdioReplayState> {
        std::sync::Mutex::new(StdioReplayState::default())
    }
    #[test]
    fn cache_initialize_request() {
        let state = make_state();
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        cache_outgoing_acp_state(msg, &state);
        let s = state.lock().unwrap();
        assert_eq!(s.initialize_json.as_deref(), Some(msg));
    }
    #[test]
    fn cache_session_load_preserves_full_request() {
        let state = make_state();
        let msg = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp","mcpServers":[]}}"#;
        cache_outgoing_acp_state(msg, &state);
        let s = state.lock().unwrap();
        let (sid, cached) = &s.sessions[0];
        assert_eq!(sid, "s1");
        assert_eq!(cached.load_request_json.as_deref(), Some(msg));
        assert_eq!(cached.cwd.as_deref(), Some("/tmp"));
        assert!(cached.mcp_servers_json.is_some());
        assert_eq!(s.last_session_id.as_deref(), Some("s1"));
    }
    #[test]
    fn cache_session_new_is_pending_until_response_assigns_id() {
        let state = make_state();
        let load = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#;
        cache_outgoing_acp_state(load, &state);
        let new = r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/home"}}"#;
        cache_outgoing_acp_state(new, &state);
        {
            let s = state.lock().unwrap();
            assert_eq!(s.sessions.len(), 1);
            assert_eq!(s.sessions[0].0, "s1");
            assert!(s.pending_new.is_some());
            assert_eq!(
                s.pending_new.as_ref().unwrap().cwd.as_deref(),
                Some("/home")
            );
        }
        cache_incoming_session_id(
            r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s2"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert!(s.pending_new.is_none());
        assert_eq!(s.sessions.len(), 2);
        assert_eq!(s.sessions[1].0, "s2");
        assert_eq!(s.sessions[1].1.cwd.as_deref(), Some("/home"));
        assert_eq!(s.last_session_id.as_deref(), Some("s2"));
    }
    #[test]
    fn cache_session_close_stops_replaying_it() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"_x.ai/session/close","params":{"sessionId":"s1"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert!(s.sessions.is_empty(), "closed session must not be replayed");
        assert!(s.last_session_id.is_none());
    }
    /// The standard close spelling must stop the replay exactly like the ext
    /// spelling: adopting `session/close` without teaching the cache would
    /// resurrect closed sessions on every leader reconnect.
    #[test]
    fn cache_standard_session_close_stops_replaying_it() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/close","params":{"sessionId":"s1"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert!(s.sessions.is_empty(), "closed session must not be replayed");
        assert!(s.last_session_id.is_none());
    }
    /// A resume-only session must survive a leader restart: the cache
    /// synthesizes a load entry, since a new leader has no turn to reattach to.
    #[test]
    fn cache_session_resume_registers_unknown_sessions_for_replay() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"s1","cwd":"/proj"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        let (sid, cached) = &s.sessions[0];
        assert_eq!(sid, "s1");
        assert_eq!(
            cached.load_request_json, None,
            "a resume must not be replayed verbatim; the replay synthesizes a load"
        );
        assert_eq!(cached.cwd.as_deref(), Some("/proj"));
        assert_eq!(s.last_session_id.as_deref(), Some("s1"));
    }
    /// A resume must not displace the original load's entry: that entry
    /// carries the client's `_meta`, which a synthesized load cannot reproduce.
    #[test]
    fn cache_session_resume_does_not_displace_the_original_load() {
        let state = make_state();
        let load = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp","_meta":{"noReplay":true}}}"#;
        cache_outgoing_acp_state(load, &state);
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/resume","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert_eq!(s.sessions.len(), 1, "one session, one replay entry");
        assert_eq!(
            s.sessions[0].1.load_request_json.as_deref(),
            Some(load),
            "the original load, with its _meta, must survive the resume"
        );
    }
    /// An UNCONFIRMED `session/new` (leader died before its response) must not
    /// be replayed — its id was never assigned — but previously loaded
    /// sessions still restore.
    #[tokio::test]
    async fn replay_after_unconfirmed_session_new_restores_prior_sessions() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        let load_a = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"session-A","cwd":"/old"}}"#;
        cache_outgoing_acp_state(load_a, &state);
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/new"}}"#,
            &state,
        );
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let load = leader_rx.recv().await.unwrap();
            assert!(load.contains("session-A"), "unexpected replay: {load}");
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#.to_string())
                .unwrap();
        });
        let s = state.lock().unwrap().clone();
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &s).await;
        assert_eq!(
            result.as_deref(),
            Some("session-A"),
            "prior session must be restored even though the new one was unconfirmed"
        );
        responder.await.unwrap();
    }
    #[test]
    fn fallback_replay_json_escapes_special_chars() {
        let cached = CachedSession {
            load_request_json: None,
            cwd: Some(r#"C:\Users\test path"#.into()),
            mcp_servers_json: None,
        };
        let json = replay_load_json(r#"session"with"quotes"#, &cached)
            .expect("cwd present → load synthesized");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("fallback replay JSON must be valid");
        assert_eq!(
            parsed["params"]["sessionId"].as_str().unwrap(),
            r#"session"with"quotes"#
        );
        assert_eq!(
            parsed["params"]["cwd"].as_str().unwrap(),
            r#"C:\Users\test path"#
        );
        assert_eq!(parsed["id"].as_str(), Some(REPLAY_LOAD_REQUEST_ID));
    }
    #[test]
    fn cache_incoming_session_id_from_response() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp"}}"#,
            &state,
        );
        let msg = r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"abc123"}}"#;
        cache_incoming_session_id(msg, &state);
        let s = state.lock().unwrap();
        assert_eq!(s.last_session_id.as_deref(), Some("abc123"));
        assert_eq!(s.sessions.len(), 1);
        assert_eq!(s.sessions[0].0, "abc123");
    }
    /// A multi-session client (IDE driving several sessions over one bridge)
    /// gets EVERY session replayed after a reconnect, in first-seen order.
    #[tokio::test]
    async fn replay_restores_all_cached_sessions() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-1","cwd":"/a"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/b"}}"#,
            &state,
        );
        cache_incoming_session_id(
            r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"sess-2"}}"#,
            &state,
        );
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let load1 = leader_rx.recv().await.unwrap();
            assert!(load1.contains("sess-1"), "expected sess-1 first: {load1}");
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#.to_string())
                .unwrap();
            let load2 = leader_rx.recv().await.unwrap();
            assert!(load2.contains("sess-2"), "expected sess-2 second: {load2}");
            let load2_json: serde_json::Value = serde_json::from_str(&load2).unwrap();
            assert_eq!(load2_json["id"].as_str(), Some(REPLAY_LOAD_REQUEST_ID));
            response_tx
                .send(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": REPLAY_LOAD_REQUEST_ID,
                        "result": {}
                    })
                    .to_string(),
                )
                .unwrap();
        });
        let s = state.lock().unwrap().clone();
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &s).await;
        assert_eq!(result.as_deref(), Some("sess-2"));
        responder.await.unwrap();
    }
    /// One broken session must not doom the rest: a rejected load is skipped
    /// and the remaining sessions still restore.
    #[tokio::test]
    async fn replay_skips_rejected_session_and_restores_the_rest() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-bad","cwd":"/a"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/load","params":{"sessionId":"sess-good","cwd":"/b"}}"#,
            &state,
        );
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let _bad = leader_rx.recv().await.unwrap();
            response_tx
                .send(
                    r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params","data":"unknown session id"}}"#
                        .to_string(),
                )
                .unwrap();
            let good = leader_rx.recv().await.unwrap();
            assert!(good.contains("sess-good"));
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#.to_string())
                .unwrap();
        });
        let s = state.lock().unwrap().clone();
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &s).await;
        assert_eq!(result.as_deref(), Some("sess-good"));
        responder.await.unwrap();
    }
    #[test]
    fn cache_incoming_ignores_non_session_response() {
        let state = make_state();
        let msg = r#"{"jsonrpc":"2.0","id":1,"result":{"models":[]}}"#;
        cache_incoming_session_id(msg, &state);
        let s = state.lock().unwrap();
        assert!(s.last_session_id.is_none());
        assert!(s.sessions.is_empty());
    }
    #[tokio::test]
    async fn replay_with_no_cached_state_returns_none() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let state = StdioReplayState::default();
        let mut sink = Vec::new();
        let result = replay_acp_state_after_reconnect(&tx, &mut rx, &mut sink, &state).await;
        assert!(result.is_none());
    }
    #[tokio::test]
    async fn replay_sends_initialize_and_session_load() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let _load = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#.to_string())
                .unwrap();
        });
        let mut rx = response_rx;
        let mut sink = Vec::new();
        let result = replay_acp_state_after_reconnect(&leader_tx, &mut rx, &mut sink, &state).await;
        assert_eq!(result.as_deref(), Some("s1"));
        assert!(
            sink.is_empty(),
            "responses to replayed requests must be swallowed, not forwarded"
        );
        responder.await.unwrap();
    }
    /// Regression test for the post-leader-crash "unknown session id" bug.
    ///
    /// `session/load` streams replay notifications BEFORE its response. The
    /// old drain logic consumed exactly one message per replayed request and
    /// returned — declaring the reconnect complete while the new leader was
    /// still loading the session. The replay must instead:
    ///   1. wait for the actual `session/load` RESPONSE (matched by id),
    ///   2. forward interleaved notifications to the client verbatim,
    ///   3. swallow only the responses to the replayed requests.
    #[tokio::test]
    async fn replay_waits_for_load_response_through_notifications() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":8,"method":"session/load","params":{"sessionId":"s9","cwd":"/tmp"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(
                    r#"{"jsonrpc":"2.0","method":"x.ai/leader/version_mismatch","params":{}}"#
                        .to_string(),
                )
                .unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#.to_string())
                .unwrap();
            let _load = leader_rx.recv().await.unwrap();
            for i in 0..3 {
                response_tx
                    .send(
                        format!(
                        r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s9","n":{i}}}}}"#
                    ),
                    )
                    .unwrap();
            }
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":8,"result":{}}"#.to_string())
                .unwrap();
        });
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &state).await;
        assert_eq!(
            result.as_deref(),
            Some("s9"),
            "replay must succeed once the load response arrives"
        );
        let forwarded = String::from_utf8(sink).unwrap();
        let lines: Vec<&str> = forwarded.lines().collect();
        assert_eq!(
            lines.len(),
            4,
            "expected exactly the 4 notifications, got: {lines:?}"
        );
        assert!(lines[0].contains("version_mismatch"));
        assert!(lines[1].contains(r#""n":0"#));
        assert!(lines[2].contains(r#""n":1"#));
        assert!(lines[3].contains(r#""n":2"#));
        assert!(!forwarded.contains(r#""id":7"#), "init response leaked");
        assert!(!forwarded.contains(r#""id":8"#), "load response leaked");
        responder.await.unwrap();
    }
    /// A `session/load` rejected by the new leader (error response) must
    /// surface as a failed replay (`None`) so the bridge emits
    /// `x.ai/leader_reconnected` with empty params and the external client
    /// knows to re-establish state itself.
    #[tokio::test]
    async fn replay_returns_none_when_load_is_rejected() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"gone","cwd":"/tmp"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let _load = leader_rx.recv().await.unwrap();
            response_tx
                .send(
                    r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params","data":"unknown session id"}}"#
                        .to_string(),
                )
                .unwrap();
        });
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &state).await;
        assert!(result.is_none(), "rejected load must not claim success");
        responder.await.unwrap();
    }
    /// The synthetic fallback `session/load` (client only ever sent
    /// `session/new`) uses a string request id that cannot collide with the
    /// external client's numeric ids — and the response matcher honors it.
    #[tokio::test]
    async fn replay_fallback_load_uses_reserved_string_id() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp"}}"#,
            &state_mutex,
        );
        cache_incoming_session_id(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-new"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let load = leader_rx.recv().await.unwrap();
            let load_json: serde_json::Value = serde_json::from_str(&load).unwrap();
            assert_eq!(
                load_json["id"].as_str(),
                Some(REPLAY_LOAD_REQUEST_ID),
                "fallback load must use the reserved string id"
            );
            response_tx
                .send(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": REPLAY_LOAD_REQUEST_ID,
                        "result": {}
                    })
                    .to_string(),
                )
                .unwrap();
        });
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &state).await;
        assert_eq!(result.as_deref(), Some("s-new"));
        responder.await.unwrap();
    }
    fn multi_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime")
    }
    #[test]
    fn run_and_shutdown_bounds_teardown_despite_stuck_blocking_task() {
        use std::time::{Duration, Instant};
        let grace = Duration::from_millis(200);
        let ceiling = grace * 8;
        let stuck_sleep = Duration::from_secs(10);
        let runtime = multi_thread_runtime();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        runtime.spawn_blocking(move || {
            let _ = started_tx.send(());
            std::thread::sleep(stuck_sleep);
        });
        started_rx.recv().expect("blocking task must start");
        let start = Instant::now();
        let out = run_and_shutdown(runtime, async { 7_u32 }, grace);
        let elapsed = start.elapsed();
        assert_eq!(out, 7, "must return the future's output");
        assert!(
            elapsed >= grace,
            "returned in {elapsed:?}, before the {grace:?} grace — timeout not exercised",
        );
        assert!(
            elapsed < ceiling,
            "teardown took {elapsed:?}; stuck task must be abandoned under {ceiling:?}",
        );
    }
    #[test]
    fn run_and_shutdown_is_fast_without_blocking_work() {
        use std::time::{Duration, Instant};
        let runtime = multi_thread_runtime();
        let grace = Duration::from_secs(5);
        let start = Instant::now();
        let out = run_and_shutdown(runtime, async { 42_u32 }, grace);
        let elapsed = start.elapsed();
        assert_eq!(out, 42, "must pass the future's output through");
        assert!(
            elapsed < grace,
            "clean teardown took {elapsed:?}; grace must be a ceiling, not a floor",
        );
    }
    #[test]
    fn run_and_shutdown_passes_err_output_through() {
        use std::time::Duration;
        let runtime = multi_thread_runtime();
        let out = run_and_shutdown(
            runtime,
            async { Err::<(), String>("boom".to_string()) },
            Duration::from_secs(5),
        );
        assert_eq!(
            out,
            Err("boom".to_string()),
            "Err output must pass through unchanged",
        );
    }
}
