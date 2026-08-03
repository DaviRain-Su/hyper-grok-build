//! The command-line entry point, shared by both faces that run the viewport:
//! the standalone `comet-tui` dev binary and the shipped `comet tui` subcommand.
//!
//! Keeping the arg parsing, environment resolution, tracing setup and runtime
//! shape here — rather than in either binary — is the same no-drift discipline
//! the rest of the crate follows: there is one way to start the viewport, so the
//! two entry points cannot disagree about which data dir, port, or frame budget
//! they mean.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;

use crate::Config;

/// Same default as `apps/comet`.
pub const DEFAULT_IPC_PORT: u16 = 27654;
/// 60fps. See [`Config::frame_budget`].
pub const DEFAULT_FPS: u32 = 60;

/// Flags for the terminal viewport. Flattened into the standalone binary's
/// parser and into `comet`'s `tui` subcommand, so both accept the same options.
#[derive(Args, Debug, Default)]
pub struct TuiArgs {
    /// Localhost IPC port the engine serves (env: COMET_IPC_PORT).
    #[arg(long)]
    pub port: Option<u16>,
    /// Engine data directory (env: COMET_DATA_DIR).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Path to the `comet` binary used to start an engine (env: COMET_BIN).
    #[arg(long)]
    pub comet_bin: Option<PathBuf>,
    /// Fail instead of starting an engine when none is listening.
    #[arg(long)]
    pub no_spawn: bool,
    /// Don't capture the mouse. Scrolling becomes keyboard-only, and
    /// drag-to-select works as usual.
    #[arg(long)]
    pub no_mouse: bool,
    /// Redraw ceiling, in frames per second.
    #[arg(long, value_name = "N")]
    pub fps: Option<u32>,
    /// Print whether an engine is listening, then exit.
    #[arg(long)]
    pub probe: bool,
}

/// Resolve args + environment into a [`Config`], set up file-based tracing and a
/// current-thread runtime, and run the viewport to completion. The blocking
/// entry point both binaries call.
pub fn run(args: TuiArgs) -> anyhow::Result<()> {
    let data_dir = args
        .data_dir
        .or_else(|| std::env::var_os("COMET_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(default_data_dir);
    let ipc_port = args
        .port
        .or_else(|| {
            std::env::var("COMET_IPC_PORT")
                .ok()
                .and_then(|port| port.parse().ok())
        })
        .unwrap_or(DEFAULT_IPC_PORT);

    // Logs go to a file, never to stdout: a tracing line written mid-frame would
    // land inside the alternate screen and corrupt it.
    init_tracing(&data_dir);

    let fps = args.fps.unwrap_or(DEFAULT_FPS).clamp(1, 240);
    let config = Config {
        data_dir,
        ipc_port,
        comet_bin: args.comet_bin,
        spawn_daemon: !args.no_spawn,
        mouse: !args.no_mouse
            && std::env::var_os("COMET_TUI_MOUSE").as_deref() != Some("0".as_ref()),
        frame_budget: Duration::from_micros(1_000_000 / u64::from(fps)),
    };

    // A current-thread runtime is the right shape here: the render loop is the
    // only real workload, everything else is I/O waiting on channels, and a
    // work-stealing pool would just add wakeups to an app whose selling point is
    // not having any.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    if args.probe {
        let listening = runtime.block_on(crate::daemon::probe(config.ipc_port));
        println!(
            "engine on 127.0.0.1:{}: {}",
            config.ipc_port,
            if listening {
                "listening"
            } else {
                "not running"
            }
        );
        // Nonzero when there's nothing there, so shell scripts can branch.
        if !listening {
            std::process::exit(1);
        }
        return Ok(());
    }

    match runtime.block_on(crate::run(config)) {
        Ok(outcome) => {
            print!("{}", outcome.farewell());
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// `~/.comet-native`, matching `apps/comet`'s `dirs_data_dir`.
pub fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    PathBuf::from(home).join(".comet-native")
}

/// Warn-level tracing into `{data_dir}/tui.log`. `RUST_LOG` overrides.
fn init_tracing(data_dir: &Path) {
    let _ = std::fs::create_dir_all(data_dir);
    let path = data_dir.join("tui.log");
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        // No log file is better than no app; diagnostics are a nicety here.
        return;
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(move || {
            file.try_clone()
                .map(Box::new)
                .map(|w| w as Box<dyn std::io::Write>)
                .unwrap_or_else(|_| Box::new(std::io::sink()))
        })
        .try_init();
}
