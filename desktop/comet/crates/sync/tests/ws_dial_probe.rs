//! Probe: dial a wss room URL through the production Happy-Eyeballs WS dial
//! (comet_sync::dial_ws). Used to confirm the remote-edge `websocket: dial
//! timeout` fix: tokio-tungstenite `connect_async` connects sequentially and
//! hangs on a blackholed IPv6 route; dial_ws races all resolved addresses so a
//! dead IPv6 family is dropped and IPv4 wins — the same reason `curl -4`
//! (Happy Eyeballs) reaches the edge while `curl -6` times out.
//!
//! Run:
//!   PROBE_WS_URL="wss://host/workspace/org1/ws?token=alice@org1" \
//!     cargo test -p comet-sync --test ws_dial_probe -- --nocapture

use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn probe_ws_dial() {
    // Skips silently when PROBE_WS_URL is unset (it's a manual diagnostic
    // against a live edge, not a CI test).
    let Ok(url) = std::env::var("PROBE_WS_URL") else {
        eprintln!("PROBE_WS_URL unset; skipping live-edge dial probe");
        return;
    };
    eprintln!("dialing (happy-eyeballs): {url}");
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(12), comet_sync::dial_ws(&url)).await;
    let elapsed = started.elapsed();
    match result {
        Ok(Ok(_ws)) => {
            eprintln!("WS_UPGRADE_OK in {elapsed:?}");
        }
        Ok(Err(e)) => {
            eprintln!("WS_DIAL_ERROR in {elapsed:?}: {e:?}");
            panic!("dial errored (not a timeout): {e}");
        }
        Err(_) => {
            eprintln!("WS_DIAL_TIMEOUT after {elapsed:?}");
            panic!("dial timed out (happy-eyeballs did not help)");
        }
    }
}
