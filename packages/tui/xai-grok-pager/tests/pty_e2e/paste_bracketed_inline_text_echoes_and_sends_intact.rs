// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// A short text paste (below the 4-line chip threshold) must echo inline in the prompt right away, and submitting must send it to the model intact.
/// The echo stays synchronous even while a clipboard attachment probe runs off the UI thread.
///
/// The bracketed-paste clipboard probe is cfg(macos/windows), so on Linux CI this test never touches a real clipboard.
/// On a macOS dev machine the probe MAY read the real host clipboard and attach an incidental image chip.
/// Every assert is therefore a contains check on a unique sentinel, never whole-prompt or whole-message equality.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn paste_bracketed_inline_text_echoes_and_sends_intact() {
    const LINE_A: &str = "PASTEECHOAAA first pasted line";
    const LINE_B: &str = "PASTEECHOBBB second pasted line";

    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} inline paste turn."));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // A paste on the welcome screen promotes to a new session and re-processes the same event through its prompt (ActionThenForward)
    // The payload therefore lands in the full-featured session prompt
    harness
        .inject_keys(format!("\x1b[200~{LINE_A}\n{LINE_B}\x1b[201~").as_bytes())
        .expect("bracketed-paste two-line payload");

    // Two lines are below the chip threshold: both echo inline, promptly.
    harness
        .wait_for_text("PASTEECHOAAA", Duration::from_secs(2))
        .expect("first pasted line echoes inline in the prompt within 2s");
    harness
        .wait_for_text("PASTEECHOBBB", Duration::from_secs(2))
        .expect("second pasted line echoes inline in the prompt within 2s");

    harness.inject_keys(b"\r").expect("submit pasted prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("mock response for the pasted prompt");

    let blobs = all_user_message_blobs(&content);
    assert!(
        blobs
            .iter()
            .any(|m| m.contains(LINE_A) && m.contains(LINE_B)),
        "submitted user message must carry both pasted sentinel lines; got {blobs:?}"
    );

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
