// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// PTY, against the built binary with real SGR clicks: a finished `!`
/// command shows its full output (success and failure), double-click folds
/// the block, and a second double-click restores the full output — never
/// the first/last preview.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
#[cfg(unix)]
async fn bash_full_output_double_click_fold_pty() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} session ready."));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        Some(content.home()),
    )
    .expect("spawn pager");
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");

    // Establish a session so `!` runs as an execute tool with Run chrome.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("start session");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("session ready");

    // 1. Success: 12 lines exceed the streaming window; all visible on finish.
    harness
        .inject_keys(b"! printf 'L%02d\\n' $(seq 1 12)\r")
        .expect("submit bash-mode command");
    harness
        .wait_for_text("L12", Duration::from_secs(30))
        .expect("bash output tail");
    // Live tail can show L06–L12 while L01 is still clipped; wait for
    // expand-on-finish before asserting the head is present.
    harness
        .wait_for_text("L01", Duration::from_secs(15))
        .unwrap_or_else(|_| {
            panic!(
                "finished ! command must not truncate output (L01 missing)\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    for line in ["L03", "L06", "L09"] {
        assert!(
            harness.contains_text(line),
            "finished ! command must not truncate output ({line} missing)\nscreen:\n{}",
            harness.screen_contents()
        );
    }

    // 2. Double-click folds; a second double-click restores the full output.
    harness.inject_keys(b"\t").expect("focus scrollback");
    harness
        .wait_for_text("Ctrl+e:", Duration::from_secs(10))
        .expect("scrollback owns keys");
    let screen = harness.screen_contents();
    let (row, col) = locate_screen_text(&screen, "Run (user)")
        .unwrap_or_else(|| panic!("locate ! block header; screen:\n{screen}"));
    let dbl = format!(
        "{}{}{}{}",
        sgr_mouse(0, row, col, 'M'),
        sgr_mouse(0, row, col, 'm'),
        sgr_mouse(0, row, col, 'M'),
        sgr_mouse(0, row, col, 'm'),
    );
    harness
        .inject_keys(dbl.as_bytes())
        .expect("double-click to fold");
    let gone = std::time::Instant::now() + Duration::from_secs(5);
    while harness.contains_text("L06") && std::time::Instant::now() < gone {
        harness.update(Duration::from_millis(100));
    }
    assert!(
        !harness.contains_text("L06"),
        "double-click must collapse the ! block; got:\n{}",
        harness.screen_contents()
    );
    harness.update(Duration::from_millis(500)); // let the multi-click window lapse
    harness
        .inject_keys(dbl.as_bytes())
        .expect("double-click to expand");
    harness
        .wait_for_text("L06", Duration::from_secs(10))
        .unwrap_or_else(|_| {
            panic!(
                "double-click must restore the FULL output (middle lines); got:\n{}",
                harness.screen_contents()
            )
        });

    // 3. A failing command finishes fully expanded too.
    harness.inject_keys(b"\t").expect("refocus prompt");
    harness
        .wait_for_text("Shift+Tab:mode", Duration::from_secs(10))
        .expect("prompt owns keys");
    harness
        .inject_keys(b"! printf 'E%02d\\n' $(seq 1 12); false\r")
        .expect("submit failing bash-mode command");
    harness
        .wait_for_text("E12", Duration::from_secs(30))
        .expect("failed bash output tail");
    harness
        .wait_for_text("E06", Duration::from_secs(10))
        .unwrap_or_else(|_| {
            panic!(
                "FAILED ! command must show its full output; got:\n{}",
                harness.screen_contents()
            )
        });

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.quit().expect("clean quit");
}
