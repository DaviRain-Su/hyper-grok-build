//! Test fixture for the orphan-reap regression test (`tests/orphan_reap.rs`).
//!
//! Spawns one long-lived child under a [`PtyController`] — exactly the way
//! every PTY e2e test spawns the pager — prints the child PID, then blocks
//! forever. The regression test kills *this process* ungracefully
//! (SIGKILL/SIGTERM, so no Drop runs) and asserts the PTY child does not
//! survive it. `main` (a thread that lives as long as the process) does the
//! spawning, satisfying the pdeathsig spawning-thread requirement.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use portable_pty::PtySize;
use xai_grok_pager_pty_harness::PtyController;
use xai_grok_test_support::TestSandbox;

fn main() -> anyhow::Result<()> {
    let sandbox = TestSandbox::new();
    // The child ignores SIGHUP: when the holder dies its PTY master closes and
    // the kernel HUPs the child's foreground group, which would kill a
    // well-behaved child and mask the leak. The leaked CI pagers were exactly
    // the ones that did not act on that SIGHUP (wedged mid-shutdown), so the
    // fixture models them; only the kernel-side pdeathsig can reap it.
    // `exec` keeps this a single process (no shell grandchild), so the test's
    // liveness probe targets the one PID that must die with the holder
    // (SIG_IGN dispositions survive exec).
    let controller = PtyController::spawn_in_sandbox(
        Path::new("/bin/sh"),
        PtySize {
            rows: 8,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        },
        &["-c", "trap '' HUP; exec sleep 600"],
        &sandbox,
        &[],
        None,
    )?;
    let pid = controller
        .child_pid()
        .ok_or_else(|| anyhow::anyhow!("PTY child has no pid"))?;
    println!("CHILD_PID={pid}");
    std::io::stdout().flush()?;

    // Hold the controller (and its child) until the test kills this process.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
