// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Esc double-press policy (idle, non-empty prompt): the **first Esc shows
/// "press again to clear"** and the **second Esc clears the prompt**, recording
/// the cleared text into prompt history (recallable via the Up-arrow history
/// panel). Proves `try_handle_esc_policy`'s idle clear arm +
/// `dispatch_clear_prompt` end-to-end on the real binary.
///
/// Uses [`spawn_esc_double_press_pager`] so a slow inter-press round-trip
/// can't expire the arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn esc_esc_clears_idle_prompt_and_records_history() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} done."));

    let mut harness = spawn_esc_double_press_pager(&content);

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // Establish a real idle session first (so Esc runs the agent policy, not
    // welcome-screen handling), then type a fresh draft to clear.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("first turn rendered");
    harness
        .wait_for_turn_idle(Duration::from_secs(15))
        .expect("turn idle");

    let draft = "ZZCLEARDRAFT";
    harness.inject_keys(draft.as_bytes()).expect("type draft");
    harness
        .wait_for_text(draft, Duration::from_secs(10))
        .expect("draft renders in the composer");

    // Wait for the confirm hint between the presses: it proves the arm landed,
    // and a single `ESC ESC` byte pair collapses to one `Esc` in crossterm.
    harness.inject_keys(keys::ESC).expect("first esc");
    harness
        .wait_for_text("press again to clear", Duration::from_secs(15))
        .expect("first idle Esc must show the clear confirm hint");

    // Second Esc fires the clear.
    harness.inject_keys(keys::ESC).expect("second esc");
    wait_for_labels_absent(&mut harness, &[draft], Duration::from_secs(5));
    assert!(
        !harness.contains_text(draft),
        "second Esc must clear the draft\nscreen:\n{}",
        harness.screen_contents()
    );
    // The confirm hint must be gone once the pending fired.
    assert!(
        !harness.contains_text("press again to clear"),
        "clear-confirm hint must clear after the second Esc fires\nscreen:\n{}",
        harness.screen_contents()
    );

    // The cleared text was recorded into prompt history: Up on the now-empty
    // prompt opens the history panel, whose list surfaces the cleared draft.
    harness.inject_keys(keys::UP).expect("open history panel");
    harness
        .wait_for_text(draft, Duration::from_secs(10))
        .expect("cleared draft recorded in prompt history");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
