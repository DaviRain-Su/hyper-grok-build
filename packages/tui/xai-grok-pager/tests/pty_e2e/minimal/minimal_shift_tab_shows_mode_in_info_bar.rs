// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// Minimal mode shows the Shift+Tab session mode in the one-line info bar directly under the prompt.
/// The mode cycle is shared with the full TUI, but minimal had no persistent indicator, so pressing Shift+Tab changed nothing visible.
/// The first press cycles Normal to Plan with no gate in the way.
/// `crate::minimal::live::render_prompt_info` must then draw a lowercase `plan` flag below the prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_shift_tab_shows_mode_in_info_bar() {
    let content = ContentController::start().await.expect("start content");
    let mut harness = spawn_minimal(&content);
    wait_minimal_ready(&mut harness);

    // Baseline: nothing on the idle screen says "plan"
    // The welcome card hint is "/help for commands …", the status line "minimal · /help", and the empty-prompt placeholder "Build anything"
    // So a lowercase "plan" can only come from the mode flag under test
    // (The shell's transient "Switched to mode: Plan" banner uses a capital P, which we don't match.)
    assert!(
        !harness.contains_text("plan"),
        "precondition: idle minimal screen must not already show 'plan'\nscreen:\n{}",
        harness.screen_contents()
    );

    // Shift+Tab arrives as BackTab (CSI Z); the first press cycles Normal to Plan
    harness.inject_keys(b"\x1b[Z").expect("inject BackTab");
    harness
        .wait_for_text("plan", Duration::from_secs(10))
        .expect("plan flag in the info bar under the prompt after Shift+Tab");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
