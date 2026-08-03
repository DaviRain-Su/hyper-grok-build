//! Hyper desktop / local control entry (`comet` binary).
//!
//! Local-link only: the gpui UI or TUI attach to a local engine over localhost
//! IPC, and the engine drives the Hyper agent (`hyper agent stdio` / ACP).
//! Cloud edge, WorkOS multi-device sync, and remote release update are not
//! wired — this fork is intentionally offline.

mod auth_cli;
mod daemon;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "comet",
    about = "Hyper local desktop controller (offline / local-link only)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (local daemon mode).
    Headless,
    /// Show local engine + IPC status.
    Status,
    /// Log in to the Hyper/xAI agent (OAuth). Not a cloud multi-device sign-in.
    AgentLogin {
        #[arg(long)]
        device_auth: bool,
    },
    /// Manage `comet headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Terminal viewport over the same local engine.
    Tui(comet_tui::cli::TuiArgs),
}

#[derive(Subcommand)]
enum DaemonCommand {
    Install,
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if !matches!(cli.command, Some(Command::Tui(_))) {
        let default_filter = match &cli.command {
            None | Some(Command::Headless) => "info,loro_internal=warn,loro=warn",
            Some(_) => "warn",
        };
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| default_filter.into()),
            )
            .init();
    }

    match cli.command {
        Some(Command::Tui(args)) => comet_tui::cli::run(args),
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = comet_engine::Engine::new(local_engine_config());
                engine.run().await
            })
        }
        Some(Command::Status) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::status(local_engine_config()))
        }
        Some(Command::AgentLogin { device_auth }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async move {
                match comet_engine::agent_login(device_auth).await {
                    Ok(()) => {
                        eprintln!("Agent login complete.");
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("Agent login failed: {e}");
                        Err(anyhow::anyhow!("agent login failed: {e}"))
                    }
                }
            })
        }
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&local_engine_config().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            // Headed: probe COMET_IPC_PORT / embed engine in-process (local only).
            comet_ui::run_app(comet_ui::UiConfig {
                data_dir: data_dir(),
                ipc_port: ipc_port(),
                // Placeholder URL — local-link mode never dials the edge.
                edge_url: "http://127.0.0.1:0".into(),
                workos_client_id: None,
                edge_token: None,
                org_id: Some("local".into()),
                default_harness: comet_ui::HarnessId::Hyper,
            });
            Ok(())
        }
    }
}

/// Always offline: no WorkOS, no edge bearer → engine skips cloud room sync.
fn local_engine_config() -> comet_engine::EngineConfig {
    comet_engine::EngineConfig {
        data_dir: data_dir(),
        edge_url: "http://127.0.0.1:0".into(),
        ipc_port: ipc_port(),
        default_harness: harness_from_env(),
        org_id: Some(
            std::env::var("COMET_ORG_ID")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "local".into()),
        ),
        workos_client_id: None,
        edge_token: None,
    }
}

fn harness_from_env() -> comet_engine::HarnessId {
    match std::env::var("COMET_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => comet_engine::HarnessId::Mock,
        Ok("codex") => comet_engine::HarnessId::Codex,
        Ok("cursor") => comet_engine::HarnessId::Cursor,
        Ok("hyper") => comet_engine::HarnessId::Hyper,
        _ => comet_engine::HarnessId::Hyper,
    }
}

fn ipc_port() -> u16 {
    std::env::var("COMET_IPC_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(27654)
}

fn data_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("COMET_DATA_DIR") {
        return std::path::PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").expect("HOME not set");
    // Prefer Hyper-scoped data when available; keep comet-native for
    // continuity with existing local installs of this controller.
    std::path::PathBuf::from(home).join(".comet-native")
}
