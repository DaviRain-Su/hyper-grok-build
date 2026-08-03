//! Release CI helper: verify packaged Hyper archives against the producer
//! contract (unique root binary, allowlisted notices, complete bundled/**,
//! no dangerous paths) and optional SHA256SUMS integrity.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = xai_grok_update::run_verify_release_cli(&args) {
        eprintln!("hyper-verify-release-archive: error: {error:#}");
        std::process::exit(1);
    }
}
