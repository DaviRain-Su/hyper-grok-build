fn main() {
    println!("cargo:rerun-if-env-changed=GROK_VERSION");
    // Forward into rustc so `option_env!("GROK_VERSION")` in lib.rs sees the
    // release-tag version set by CI (`GROK_VERSION=… cargo build …`).
    if let Ok(v) = std::env::var("GROK_VERSION") {
        println!("cargo:rustc-env=GROK_VERSION={v}");
    }
}
