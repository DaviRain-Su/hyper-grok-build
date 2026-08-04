//! Local status for the offline Hyper desktop controller.

use comet_engine::{Engine, EngineConfig, InstanceLock};

/// `comet status`: report local engine + IPC + Hyper binary resolution.
pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let _auth = Engine::build_auth(&config).await;
    println!("Mode:     local-link only (cloud disabled)");
    println!("Data dir: {}", config.data_dir.display());
    println!("Auth:     offline desktop engine; agent auth via Hyper login");
    println!(
        "Harness:  {:?} (override with COMET_HARNESS)",
        config.default_harness
    );
    match comet_harness::resolve_hyper_bin() {
        Ok(bin) => println!("Hyper:    {}", bin.display()),
        Err(e) => println!("Hyper:    NOT FOUND ({e})"),
    }
    match InstanceLock::holder(&config.data_dir) {
        Some(pid) => println!("Engine:   running (pid {pid})"),
        None => println!("Engine:   not running"),
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.ipc_port));
    let ipc = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500));
    println!(
        "IPC:      {} 127.0.0.1:{}",
        if ipc.is_ok() {
            "listening on"
        } else {
            "not listening on"
        },
        config.ipc_port
    );
    Ok(())
}
