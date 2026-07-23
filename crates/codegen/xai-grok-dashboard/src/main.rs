//! Standalone dashboard binary.

use std::net::SocketAddr;

use anyhow::Result;
use clap::Parser;
use xai_grok_dashboard::{DashboardServerConfig, serve};

#[derive(Debug, Parser)]
#[command(
    name = "hyper-dashboard",
    version,
    about = "Local web observability for Hyper sessions"
)]
struct Args {
    /// Loopback address for the HTTP server.
    #[arg(long, default_value = "127.0.0.1:9090")]
    bind: SocketAddr,

    /// Do not open the default browser.
    #[arg(long)]
    no_open: bool,

    /// Override the Grok home directory (defaults to $GROK_HOME or ~/.grok).
    #[arg(long)]
    grok_home: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("xai_grok_dashboard=info")),
        )
        .init();

    let args = Args::parse();
    serve(DashboardServerConfig {
        bind: args.bind,
        open_browser: !args.no_open,
        grok_home: args.grok_home.unwrap_or_else(xai_grok_config::grok_home),
    })
    .await
}
