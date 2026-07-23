//! Standalone workspace ToolServer for remote sandboxes.
//!
//! Reads OIDC credentials from `~/.grok/auth.json`, connects to a
//! server, exposes workspace tools, and refreshes tokens
//! automatically.
use clap::Parser;
use std::path::PathBuf;
use url::Url;
use xai_grok_workspace::config::WorkspaceServerMetadata;
use xai_grok_workspace::daemonize;
use xai_grok_workspace::diag_server;
use xai_grok_workspace::preview_supervisor::{self, PreviewArgs, PreviewVisibility};
/// OTLP `service.name` for this binary's exported traces/logs/metrics and
/// direct-OTLP fastrace export. Single source so the call sites can't drift.
const SERVICE_NAME: &str = "prod_grok_workspace";
const EXIT_SERVER_ID_INVALID: i32 = 3;
const INVALID_SERVER_ID_MARKER: &str = "workspace-server: invalid --server-id";
fn server_id_startup_error(id: &str) -> Option<String> {
    id.parse::<xai_tool_protocol::ServerId>()
        .err()
        .map(|e| format!("{INVALID_SERVER_ID_MARKER} {id:?}: {e}"))
}
#[derive(Parser)]
#[command(name = "xai-workspace-server")]
#[command(about = "Standalone workspace ToolServer for the server connection")]
struct Args {
    /// Print the capability manifest as JSON to stdout and exit 0. Legacy
    /// binaries reject the unknown flag via clap (non-zero exit), giving the
    /// launcher a definitive feature probe.
    #[arg(long)]
    capabilities: bool,
    #[arg(long, default_value = "wss://computer-hub.grok.com/v1/tools")]
    hub_url: String,
    #[arg(long)]
    auth_config: Option<PathBuf>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Stable server identity for hub registration. Used as the
    /// `server_id` in `servers.list` and `server.bind` so clients
    /// can address this specific workspace server.
    /// When omitted, the SDK default ("workspace-server") is used.
    #[arg(long)]
    server_id: Option<String>,
    /// JSON metadata attached to the tool server registration.
    /// Propagated to `ServerInfo.metadata` in `servers.list` responses.
    #[arg(long)]
    metadata: Option<String>,
    /// Deprecated no-op, accepted for one release so existing callers don't
    /// trip clap: nothing writes or reads this path.
    #[arg(long, hide = true)]
    ready_file: Option<PathBuf>,
    /// Unix-socket path for the in-guest diagnostics HTTP server
    /// (`/ready`, `/statusz`).
    #[cfg(unix)]
    #[arg(long, default_value = diag_server::DEFAULT_DIAG_SOCKET_PATH)]
    diag_socket: PathBuf,
    /// Loopback TCP port for the diagnostics HTTP server (Windows guests,
    /// which lack a reliable Unix-socket HTTP client).
    #[cfg(windows)]
    #[arg(long, default_value_t = diag_server::DEFAULT_DIAG_PORT)]
    diag_port: u16,
    /// Permit a plaintext `ws://` hub on a non-loopback host. Only for a
    /// mesh-secured transport; the bearer crosses the network otherwise.
    #[arg(long)]
    allow_insecure_ws: bool,
    /// Route per-turn uploads through the durable on-disk
    /// upload queue (retries + spill-to-disk) instead of the legacy inline
    /// `gcs::upload_bytes` path.
    ///
    /// Enabled by default. Pass `--upload-queue-enabled false` (or set the
    /// `GROK_WORKSPACE_UPLOAD_QUEUE_ENABLED` env var to `false`) to fall back to
    /// the legacy inline path. Accepts `true`/`false`.
    #[arg(
        long,
        env = "GROK_WORKSPACE_UPLOAD_QUEUE_ENABLED",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    upload_queue_enabled: bool,
    /// Fail `session.bind`s without an explicit toolset closed (RPC-only)
    /// instead of widening to the built-in default catalog.
    #[arg(long)]
    require_explicit_toolset: bool,
    /// Trust project-scoped LSP servers from `<repo>/.grok/lsp.json`.
    /// Defaults off; sandbox opts in only after workspace trust is established.
    #[arg(
        long,
        env = "GROK_WORKSPACE_PROJECT_LSP_TRUSTED",
        default_value_t = false,
        action = clap::ArgAction::Set,
    )]
    project_lsp_trusted: bool,
    /// Confine `x.ai/fs/*` resolution to the workspace root (reject `..`,
    /// absolute-outside-root, symlink escapes). On by default: the standalone
    /// server always backs a remote-sandbox workspace, a real tenant boundary.
    /// Override with `GROK_WORKSPACE_CONFINE_FS_TO_ROOT=false` (e.g. local dev).
    #[arg(
        long,
        env = "GROK_WORKSPACE_CONFINE_FS_TO_ROOT",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    confine_fs_to_workspace_root: bool,
    /// Self-daemonize at startup: double-fork + `setsid()` into a new session
    /// and process group (escaping the launcher's process-group reap),
    /// redirect stdio to a log file, and hold a single-instance pidfile lock.
    ///
    /// Off by default — opt-in, passed by the launcher in the supervised
    /// deployment mode. With the flag absent, startup is unchanged.
    #[arg(long)]
    daemonize: bool,
    /// Where `--daemonize` redirects stdout+stderr. Ignored without
    /// `--daemonize`.
    #[arg(long, default_value = daemonize::DEFAULT_LOG_PATH)]
    log_file: PathBuf,
    /// Single-instance pidfile lock path used with `--daemonize`. Ignored
    /// without `--daemonize`.
    #[arg(long, default_value = daemonize::DEFAULT_PIDFILE_PATH)]
    pid_file: PathBuf,
    #[command(flatten)]
    preview: PreviewCliArgs,
}
/// Preview-proxy supervision flags. Forwarded 1:1 to the
/// `/usr/local/bin/xai-grok-preview-proxy` child (see `cli.rs` for the proxy's
/// flag names). Off by default — when `--preview-enabled` is absent the
/// supervisor is never started and startup is byte-for-byte the non-preview
/// path.
#[derive(clap::Args, Debug)]
struct PreviewCliArgs {
    /// Spawn and supervise the in-sandbox preview-proxy. The launcher passes
    /// this only when the proxy binary was mounted into this container.
    #[arg(long)]
    preview_enabled: bool,
    /// Proxy `--preview-port` (externally exposed listener). Absent ⇒ proxy default.
    #[arg(long)]
    preview_port: Option<u16>,
    /// Proxy `--control-port` (loopback control). Absent ⇒ proxy default.
    #[arg(long)]
    preview_control_port: Option<u16>,
    /// Proxy `--visibility` (`owner` | `public`). Absent ⇒ proxy default.
    /// Validated here so a bad value fails fast instead of crash-looping the proxy.
    #[arg(long, value_enum)]
    preview_visibility: Option<PreviewVisibility>,
    /// Proxy `--instance-suffix` for inbound Host validation.
    #[arg(long)]
    preview_instance_suffix: Option<String>,
    /// Proxy `--auth-redirect`: URL the unauthenticated handshake redirects to.
    /// Required for the default `owner` gate to redirect rather than deny.
    #[arg(long)]
    preview_auth_redirect: Option<String>,
    /// Proxy `--allow-public` org public-policy gate (forwarded only when set).
    #[arg(long)]
    preview_allow_public: bool,
    /// Proxy `--workspace-server-port` (added to the discovery denylist).
    #[arg(long)]
    preview_workspace_server_port: Option<u16>,
}
impl PreviewCliArgs {
    fn into_preview_args(self, workspace_dir: PathBuf) -> PreviewArgs {
        PreviewArgs {
            enabled: self.preview_enabled,
            port: self.preview_port,
            control_port: self.preview_control_port,
            visibility: self.preview_visibility,
            instance_suffix: self.preview_instance_suffix,
            auth_redirect: self.preview_auth_redirect,
            allow_public: self.preview_allow_public,
            workspace_server_port: self.preview_workspace_server_port,
            workspace_dir,
        }
    }
}
/// Capability manifest printed by `--capabilities`, consumed by the sandbox
/// launcher to pick a launch protocol. Additions are backward-compatible.
#[derive(Debug, serde::Serialize)]
struct Capabilities {
    /// The in-guest diagnostics HTTP server (`/ready`, `/statusz`, `/logs`).
    diag: bool,
}
const CAPABILITIES: Capabilities = Capabilities { diag: true };
fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    if args.capabilities {
        println!("{}", serde_json::to_string(&CAPABILITIES)?);
        return Ok(());
    }
    if let Some(msg) = args.server_id.as_deref().and_then(server_id_startup_error) {
        eprintln!("{msg}");
        std::process::exit(EXIT_SERVER_ID_INVALID);
    }
    let cwd = match args.cwd {
        Some(ref p) => dunce::canonicalize(p)?,
        None => std::env::current_dir()?,
    };
    let _pidfile_guard = if args.daemonize {
        let anchor = |p: PathBuf| if p.is_absolute() { p } else { cwd.join(p) };
        args.log_file = anchor(std::mem::take(&mut args.log_file));
        args.pid_file = anchor(std::mem::take(&mut args.pid_file));
        #[cfg(unix)]
        {
            args.diag_socket = anchor(std::mem::take(&mut args.diag_socket));
        }
        args.auth_config = args.auth_config.take().map(anchor);
        daemonize::daemonize(&args.log_file)?;
        match daemonize::PidFile::acquire_or_take_over(&args.pid_file, daemonize::TAKEOVER_GRACE)? {
            Some(guard) => Some(guard),
            None => return Ok(()),
        }
    } else {
        None
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args, cwd))
}
async fn run(args: Args, cwd: PathBuf) -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let donating = xai_computer_hub_sdk::DonatingLogLayer::new_inert();
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(donating.clone())
        .init();
    let direct_otlp = match std::env::var("GROK_WORKSPACE_OTLP_ENDPOINT") {
        Ok(endpoint) if !endpoint.is_empty() => {
            match xai_tracing::init_fastrace(endpoint.clone(), SERVICE_NAME.to_owned(), None) {
                Ok(()) => {
                    tracing::info!(%endpoint, "trace export enabled (direct OTLP)");
                    true
                }
                Err(e) => {
                    tracing::warn!(error = %e, "direct OTLP trace export init failed");
                    false
                }
            }
        }
        _ => false,
    };
    let url = Url::parse(&args.hub_url).map_err(|e| anyhow::anyhow!("invalid --hub-url: {e}"))?;
    {
        use xai_grok_sandbox::{ProfileName, SandboxManager};
        let profile = match std::env::var("GROK_SANDBOX_PROFILE").ok() {
            Some(val) => {
                let parsed = val
                    .parse::<ProfileName>()
                    .expect("ProfileName::from_str is infallible");
                if matches!(parsed, ProfileName::Custom(_)) {
                    tracing::warn!(value = %val,
                        "Unrecognized GROK_SANDBOX_PROFILE, defaulting to workspace");
                    ProfileName::Workspace
                } else {
                    parsed
                }
            }
            None if xai_grok_sandbox::trust_bwrap_marker_for_devbox() => ProfileName::Devbox,
            None => ProfileName::Workspace,
        };
        let profile_name = profile.to_string();
        if profile == ProfileName::Off {
            tracing::info!(profile = %profile_name, "Sandbox explicitly disabled via GROK_SANDBOX_PROFILE=off");
        } else {
            let mut sandbox = SandboxManager::new(profile, &cwd);
            if let Err(e) = sandbox.apply(&cwd) {
                tracing::warn!(error = %e, "Sandbox apply returned error, continuing unsandboxed");
            } else if !sandbox.is_applied() {
                tracing::warn!("Sandbox could not be applied (unsupported platform)");
            }
            sandbox.install();
            let active = xai_grok_sandbox::is_active();
            let status_msg = if active {
                "Workspace server sandbox active"
            } else {
                "Workspace server sandbox NOT active"
            };
            tracing::info!(
                profile = %profile_name,
                active,
                restrict_network_at_known_linux_launches = xai_grok_sandbox::should_restrict_child_network(),
                "{status_msg}"
            );
        }
    }
    let auth_provider = xai_grok_workspace::hub_auth::provider(&url, args.auth_config.as_deref())?;
    tracing::info!(
        hub_url = %url,
        cwd = %cwd.display(),
        "Starting workspace server"
    );
    let cwd_display = cwd.display().to_string();
    let session_id = std::env::var("GROK_SESSION_ID").ok();
    let parsed_metadata = match args.metadata {
        Some(json_str) => Some(
            serde_json::from_str(&json_str)
                .map_err(|e| anyhow::anyhow!("invalid --metadata JSON: {e}"))?,
        ),
        None => None,
    };
    let metadata = WorkspaceServerMetadata::merge_session_metadata(parsed_metadata, session_id);
    let launch_id = metadata
        .as_ref()
        .and_then(|v| v.get("launch_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let diag_handle = diag_server::DiagHandle::new(launch_id);
    #[cfg(unix)]
    let diag_listener = diag_server::DiagListener::Unix(args.diag_socket);
    #[cfg(windows)]
    let diag_listener = diag_server::DiagListener::Tcp(args.diag_port);
    let diag_log_file = args.daemonize.then_some(args.log_file);
    let _diag_server = match diag_server::serve(diag_listener, diag_handle.clone(), diag_log_file)
        .await
    {
        Ok(bound) => {
            tracing::info!(addr = %bound.addr, "diagnostics server listening");
            Some(bound)
        }
        Err(e) => {
            if args.daemonize {
                tracing::error!(error = %e, "{}", diag_server::DIAG_BIND_FAILED_MARKER);
                std::process::exit(diag_server::EXIT_DIAG_BIND_FAILED);
            }
            tracing::warn!(error = %e, "{} (continuing without)", diag_server::DIAG_BIND_FAILED_MARKER);
            None
        }
    };
    tracing::info!(
        cwd = %cwd_display,
        "Workspace server starting — sessions created dynamically via server bind"
    );
    let server_id = args.server_id.clone();
    let status_config = xai_grok_workspace::StatusConfig::from_env();
    let preview_shutdown = if args.preview.preview_enabled {
        let control_port = args.preview.preview_control_port;
        let cfg = args.preview.into_preview_args(cwd.clone());
        let (tx, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(preview_supervisor::supervise_preview(cfg, rx));
        Some((tx, control_port))
    } else {
        None
    };
    let preview_scrape_interval = status_config.preview_activity_scrape_interval;
    xai_grok_workspace::init_metrics();
    let ws_handle = xai_grok_workspace::handle::connect_local_workspace(
        cwd,
        url,
        auth_provider,
        metadata,
        server_id.clone(),
        None,
        args.allow_insecure_ws,
        status_config,
        args.upload_queue_enabled,
        args.project_lsp_trusted,
        Some(diag_handle.clone()),
        args.require_explicit_toolset,
        args.confine_fs_to_workspace_root,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to connect workspace to hub: {e}"))?;
    if let Some((tx, control_port)) = &preview_shutdown {
        tokio::spawn(preview_supervisor::supervise_preview_activity(
            *control_port,
            ws_handle.activity_tracker().clone(),
            preview_scrape_interval,
            tx.subscribe(),
        ));
    }
    let mut donation_pump = None;
    if !direct_otlp {
        match ws_handle.trace_donation_reporter(SERVICE_NAME).await {
            Some((reporter, pump)) => {
                fastrace::set_reporter(reporter, fastrace::collector::Config::default());
                donation_pump = Some(pump);
                tracing::info!("trace export enabled");
            }
            None => tracing::info!("trace export disabled (not connected)"),
        }
    }
    let mut log_donation_pump = None;
    match ws_handle.log_donation_layer(SERVICE_NAME).await {
        Some((sender, pump)) => {
            donating.activate(sender);
            log_donation_pump = Some(pump);
            tracing::info!("log export enabled");
        }
        None => tracing::info!("log export disabled (not connected)"),
    }
    let mut metric_donation_pump = None;
    match ws_handle.metric_donation_reporter(SERVICE_NAME).await {
        Some(pump) => {
            metric_donation_pump = Some(pump);
            tracing::info!("metric export enabled");
        }
        None => tracing::info!("metric export disabled (not connected)"),
    }
    tracing::info!(
        server_id = ?server_id,
        "Workspace server connected to hub. Serving tools."
    );
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    if let Some((tx, _)) = &preview_shutdown {
        let _ = tx.send(true);
    }
    diag_handle.set_shutting_down();
    tracing::info!("Received shutdown signal, draining...");
    let tracker = ws_handle.activity_tracker().clone();
    let grace_budget = xai_grok_workspace::handle::termination_grace_from_env();
    ws_handle
        .two_phase_drain(
            grace_budget,
            xai_grok_workspace::handle::DrainReason::Sigterm,
        )
        .await;
    tracker.set_shutting_down();
    tracing::info!("Shutting down...");
    fastrace::flush();
    if let Some(pump) = &donation_pump {
        pump.drain().await;
    }
    xai_computer_hub_sdk::flush_log_layer();
    if let Some(pump) = &log_donation_pump {
        pump.drain().await;
    }
    if let Some(pump) = &metric_donation_pump {
        pump.drain().await;
    }
    ws_handle.shutdown_hub().await;
    xai_grok_sandbox::flush();
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities_flag_parses_and_defaults_off() {
        let args = Args::try_parse_from(["xai-workspace-server"]).unwrap();
        assert!(!args.capabilities);
        let args = Args::try_parse_from(["xai-workspace-server", "--capabilities"]).unwrap();
        assert!(args.capabilities);
    }
    #[test]
    fn project_lsp_trust_defaults_off_and_is_opt_in() {
        unsafe { std::env::remove_var("GROK_WORKSPACE_PROJECT_LSP_TRUSTED") };
        let args = Args::try_parse_from(["xai-workspace-server"]).unwrap();
        assert!(!args.project_lsp_trusted);
        let args = Args::try_parse_from(["xai-workspace-server", "--project-lsp-trusted", "true"])
            .unwrap();
        assert!(args.project_lsp_trusted);
    }
    #[test]
    fn capabilities_manifest_shape() {
        let value = serde_json::to_value(CAPABILITIES).unwrap();
        assert_eq!(value, serde_json::json!({"diag": true}));
    }
    #[test]
    fn capabilities_probe_of_legacy_binary_exits_nonzero() {
        /// A stand-in for a pre-`--capabilities` Args surface.
        #[derive(Debug, Parser)]
        struct LegacyArgs {
            #[arg(long)]
            daemonize: bool,
        }
        let err = LegacyArgs::try_parse_from(["xai-workspace-server", "--capabilities"])
            .expect_err("a legacy binary must reject the flag");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        assert_ne!(
            err.exit_code(),
            0,
            "the probe relies on a non-zero exit distinguishing legacy binaries"
        );
    }
    #[test]
    fn daemonize_defaults_are_inert() {
        let args = Args::try_parse_from(["xai-workspace-server"]).unwrap();
        assert!(!args.daemonize);
        assert_eq!(args.log_file, PathBuf::from(daemonize::DEFAULT_LOG_PATH));
        assert_eq!(
            args.pid_file,
            PathBuf::from(daemonize::DEFAULT_PIDFILE_PATH)
        );
        assert_eq!(args.ready_file, None);
    }
    #[test]
    fn ready_file_is_accepted_as_a_deprecated_no_op() {
        let args =
            Args::try_parse_from(["xai-workspace-server", "--ready-file", "/tmp/x.ready"]).unwrap();
        assert_eq!(args.ready_file, Some(PathBuf::from("/tmp/x.ready")));
    }
    #[test]
    fn invalid_server_id_produces_the_marker_line() {
        for bad in ["auto:tool:x", ""] {
            let msg = server_id_startup_error(bad)
                .unwrap_or_else(|| panic!("server id {bad:?} must be rejected"));
            assert!(
                msg.starts_with(INVALID_SERVER_ID_MARKER),
                "startup error must START with the greppable marker prefix: {msg}"
            );
        }
        assert_eq!(server_id_startup_error("session-abc-123"), None);
    }
    #[test]
    fn argv_rejection_exit_code_is_distinct_from_server_id_exit_code() {
        let err = Args::try_parse_from(["xai-workspace-server", "--flag-from-the-future"])
            .err()
            .expect("unknown argv must be rejected");
        assert_eq!(err.exit_code(), 2, "clap argv rejection exits 2");
        assert_ne!(err.exit_code(), EXIT_SERVER_ID_INVALID);
        assert_ne!(EXIT_SERVER_ID_INVALID, 0);
        assert_ne!(EXIT_SERVER_ID_INVALID, 1);
    }
    #[test]
    fn preview_defaults_are_inert() {
        let args = Args::try_parse_from(["xai-workspace-server"]).unwrap();
        assert!(!args.preview.preview_enabled);
        let cfg = args.preview.into_preview_args(PathBuf::from("/workspace"));
        assert!(!cfg.enabled);
        assert!(
            cfg.to_argv().is_empty(),
            "an inert config forwards no proxy args"
        );
    }
    #[test]
    fn preview_flags_parse_and_lower_to_supervisor_config() {
        let args = Args::try_parse_from([
            "xai-workspace-server",
            "--preview-enabled",
            "--preview-port",
            "6014",
            "--preview-control-port",
            "6015",
            "--preview-visibility",
            "public",
            "--preview-instance-suffix",
            ".inst.example",
            "--preview-auth-redirect",
            "https://grok.com/preview-auth",
            "--preview-allow-public",
            "--preview-workspace-server-port",
            "8470",
        ])
        .unwrap();
        assert!(args.preview.preview_enabled);
        let cfg = args.preview.into_preview_args(PathBuf::from("/workspace"));
        assert!(cfg.enabled);
        assert_eq!(cfg.port, Some(6014));
        assert_eq!(cfg.control_port, Some(6015));
        assert_eq!(cfg.visibility, Some(PreviewVisibility::Public));
        assert_eq!(cfg.instance_suffix.as_deref(), Some(".inst.example"));
        assert_eq!(
            cfg.auth_redirect.as_deref(),
            Some("https://grok.com/preview-auth")
        );
        assert!(cfg.allow_public);
        assert_eq!(cfg.workspace_server_port, Some(8470));
        assert_eq!(cfg.workspace_dir, PathBuf::from("/workspace"));
        assert_eq!(
            cfg.to_argv(),
            vec![
                "--preview-port",
                "6014",
                "--control-port",
                "6015",
                "--visibility",
                "public",
                "--instance-suffix",
                ".inst.example",
                "--auth-redirect",
                "https://grok.com/preview-auth",
                "--allow-public",
                "--workspace-server-port",
                "8470",
            ],
        );
    }
    #[test]
    fn preview_visibility_rejects_invalid_value() {
        let err = Args::try_parse_from([
            "xai-workspace-server",
            "--preview-enabled",
            "--preview-visibility",
            "nobody",
        ])
        .err()
        .expect("an invalid --preview-visibility must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }
    #[test]
    fn preview_visibility_owner_parses_and_lowers() {
        let args = Args::try_parse_from([
            "xai-workspace-server",
            "--preview-enabled",
            "--preview-visibility",
            "owner",
        ])
        .unwrap();
        let cfg = args.preview.into_preview_args(PathBuf::from("/workspace"));
        assert_eq!(cfg.visibility, Some(PreviewVisibility::Owner));
        assert_eq!(cfg.to_argv(), vec!["--visibility", "owner"]);
    }
}
