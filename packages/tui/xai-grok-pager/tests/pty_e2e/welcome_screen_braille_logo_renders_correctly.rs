// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// The logo uses Unicode Braille Pattern characters (U+2800-U+28FF).
/// A writer-thread regression (`WriteFile` instead of `WriteConsoleW` on Windows, or a missing `SetConsoleOutputCP(65001)`) garbles the output.
/// Such a writer misinterprets the multi-byte UTF-8 characters as individual legacy code-page bytes.
///
/// This test asserts that specific Braille characters from the logo appear intact in the PTY screen buffer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn welcome_screen_braille_logo_renders_correctly() {
    let content = ContentController::start().await.expect("start content");

    let binary = pager_binary().expect("resolve pager binary");
    // Use a tall terminal so pick_logo() selects the 7-line logo (26 rows or more)
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    let screen = harness.screen_contents();

    // The logo contains distinctive Braille characters
    // A writer thread sending raw UTF-8 through a code-page-dependent API mangles each 3-byte character into 3 single-byte ones (e.g. Cyrillic).
    // Check for a few that only appear in the logo, not in any ASCII menu label
    //
    // From logo07.txt line 2: ⣠⣾⠿⠛
    assert!(
        screen.contains('⣾'),
        "Braille character ⣾ (U+28FE) not found in screen — \
         logo may be garbled by code-page misinterpretation.\n\
         Screen contents:\n{screen}"
    );
    assert!(
        screen.contains('⣿'),
        "Braille character ⣿ (U+28FF) not found in screen — \
         logo may be garbled.\n\
         Screen contents:\n{screen}"
    );

    harness.quit().expect("clean quit");
}
