//! Capture a styled welcome screen from the real `hyper` binary in a PTY and
//! dump it as JSON (Vec<StyledLine>) for an external rasterizer.
//!
//! Usage:
//!   PAGER_BINARY=target/debug/hyper cargo run -p xai-grok-pager-pty-harness \
//!     --example tui_shot -- <lang> <out.json>
//!
//! The harness auto-answers the pager's terminal feature probes
//! (`set_respond_to_queries(true)`), so startup completes without a real
//! terminal emulator on the other end.

use std::io::Write as _;
use std::time::Duration;

use xai_grok_pager_pty_harness::{PtyHarness, pager_binary};

fn main() -> anyhow::Result<()> {
    let lang = std::env::args().nth(1).unwrap_or_else(|| "en".into());
    let out_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/tmp/shot.json".into());

    let home = tempfile::tempdir()?;
    let grok_home = home.path().join(".grok");
    std::fs::create_dir_all(&grok_home)?;
    std::fs::write(
        grok_home.join("config.toml"),
        format!("[ui]\nlanguage = \"{lang}\"\n"),
    )?;

    let binary = pager_binary()?;
    let env: Vec<(String, String)> = vec![
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        ("GROK_HOME".into(), grok_home.display().to_string()),
        ("HOME".into(), home.path().display().to_string()),
        ("GROK_TELEMETRY_ENABLED".into(), "false".into()),
        ("GROK_DISABLE_AUTOUPDATER".into(), "1".into()),
    ];
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let mut harness = PtyHarness::new_inherited_env(&binary, 30, 100, &[], &env_refs, None)?;
    harness.set_respond_to_queries(true);

    // The version badge ("Beta") renders on every welcome variant.
    harness.wait_for_text("Beta", Duration::from_secs(25))?;
    // Let the logo shimmer settle into a stable frame.
    std::thread::sleep(Duration::from_millis(600));
    harness.update(Duration::from_millis(300));

    let styled = harness.screen_styled();
    let mut f = std::fs::File::create(&out_path)?;
    f.write_all(serde_json::to_string_pretty(&styled)?.as_bytes())?;
    eprintln!("wrote {} lines to {out_path}", styled.len());
    Ok(())
}
