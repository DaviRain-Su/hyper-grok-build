// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

const FEEDBACK_PLACEHOLDER_SENTINEL: &str = "Please provide as much detail as possible.";
const FEEDBACK_LABEL_SENTINEL: &str = "How can we improve Grok Build?";
const THANKS_SENTINEL: &str = "Thanks for the feedback";
const INLINE_FEEDBACK: &str = "pty-inline-feedback-report-xyz";
const PANE_FEEDBACK: &str = "pty-pane-feedback-crash-on-empty-xyz";

fn thanks_count(harness: &PtyHarness) -> usize {
    harness.screen_contents().matches(THANKS_SENTINEL).count()
}

/// Bare `/feedback` opens the descriptive freeform pane; typing + Enter submits. `/feedback <text>` skips the pane and submits immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn feedback_slash_opens_descriptive_pane() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} ready for feedback."));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust"],
        Some(content.home()),
    )
    .expect("spawn pager with content");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // Establish a session so SendFeedback has a session_id.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("response rendered");

    // Bare `/feedback` opens the freeform pane with descriptive guidance.
    harness
        .inject_keys(b"/feedback\r")
        .expect("type bare /feedback");
    harness
        .wait_for_text(FEEDBACK_LABEL_SENTINEL, Duration::from_secs(15))
        .expect("feedback pane label should render");
    assert!(
        harness.contains_text(FEEDBACK_PLACEHOLDER_SENTINEL),
        "the empty report box must show what to write\nscreen:\n{}",
        harness.screen_contents()
    );

    // Empty Enter keeps the pane open.
    harness.inject_keys(b"\r").expect("empty enter");
    harness.update(Duration::from_millis(300));
    assert!(
        harness.contains_text(FEEDBACK_LABEL_SENTINEL),
        "empty Enter must keep the feedback pane open\nscreen:\n{}",
        harness.screen_contents()
    );
    assert_eq!(
        thanks_count(&harness),
        0,
        "empty Enter must not thank the user"
    );

    // Type freeform feedback and submit (pane starts in InputMode).
    harness
        .inject_keys(format!("{PANE_FEEDBACK}\r").as_bytes())
        .expect("type + submit pane feedback");
    harness
        .wait_for_text(THANKS_SENTINEL, Duration::from_secs(15))
        .expect("pane submit should thank the user");
    assert_eq!(
        thanks_count(&harness),
        1,
        "exactly one thanks after pane submit"
    );
    assert!(
        !harness.contains_text(FEEDBACK_LABEL_SENTINEL),
        "feedback pane should close after submit\nscreen:\n{}",
        harness.screen_contents()
    );

    // Inline `/feedback <text>` submits without reopening the pane.
    harness
        .inject_keys(format!("/feedback {INLINE_FEEDBACK}\r").as_bytes())
        .expect("type inline /feedback");
    harness
        .wait_until(
            "second thanks for inline submit",
            Duration::from_secs(15),
            |h| thanks_count(h) >= 2,
        )
        .expect("inline feedback should produce a second thanks");

    harness.update(Duration::from_millis(400));
    assert!(
        !harness.contains_text(FEEDBACK_LABEL_SENTINEL),
        "inline /feedback must not open the freeform pane\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
