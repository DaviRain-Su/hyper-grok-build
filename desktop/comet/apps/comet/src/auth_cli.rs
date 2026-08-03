//! Local status for the offline Hyper desktop controller.

use comet_engine::{Engine, EngineConfig, InstanceLock};

/// `comet status`: report local engine + IPC. Always "signed in" for offline mode.
pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let _auth = Engine::build_auth(&config).await;
    println!("Mode:     local-link only (cloud disabled)");
    println!("Data dir: {}", config.data_dir.display());
    println!("Auth:     offline (no WorkOS / edge)");
    println!("Harness:  hyper (override with COMET_HARNESS)");
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
