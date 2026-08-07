// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

const FEEDBACK_LABEL_SENTINEL: &str = "How can we improve Grok Build?";
const FEEDBACK_PLACEHOLDER_SENTINEL: &str = "Please provide as much detail as possible.";
const SESSION_GATE_SENTINEL: &str = "No active session";
const THANKS_SENTINEL: &str = "Thanks for the feedback";
const PANE_FEEDBACK: &str = "minimal-pty-feedback-report-xyz";

/// Minimal: bare `/feedback` without a session shows a system notice; after a session exists the freeform pane opens and submits like full TUI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn minimal_feedback_session_gate_and_pane() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} minimal ready."));

    let mut harness = spawn_minimal(&content);
    wait_minimal_ready(&mut harness);

    // Pre-session: guard must be visible as system text (toast is invisible).
    harness
        .inject_keys(b"/feedback\r")
        .expect("bare /feedback before session");
    harness
        .wait_for_full_text(SESSION_GATE_SENTINEL, Duration::from_secs(15))
        .expect("minimal session gate must use a system notice");
    assert!(
        !harness.contains_text(FEEDBACK_LABEL_SENTINEL),
        "pane must not open without a session\nscreen:\n{}",
        harness.screen_contents()
    );

    // Establish a session, then open the pane and submit.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("response rendered");

    harness
        .inject_keys(b"/feedback\r")
        .expect("bare /feedback with session");
    harness
        .wait_for_text(FEEDBACK_LABEL_SENTINEL, Duration::from_secs(15))
        .expect("feedback pane label in minimal");
    assert!(
        harness.contains_text(FEEDBACK_PLACEHOLDER_SENTINEL),
        "composer placeholder must render in minimal\nscreen:\n{}",
        harness.screen_contents()
    );

    harness
        .inject_keys(format!("{PANE_FEEDBACK}\r").as_bytes())
        .expect("submit freeform feedback");
    harness
        .wait_for_full_text(THANKS_SENTINEL, Duration::from_secs(15))
        .expect("minimal pane submit should thank the user");
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
