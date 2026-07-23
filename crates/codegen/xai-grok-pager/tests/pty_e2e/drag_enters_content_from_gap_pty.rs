// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Single-line message; the drag enters it at the tail word.
const GAPDEEP_LINE: &str = "GAPDEEP alpha beta gamma delta epsilon";

const ENTRY_WORD: &str = "epsilon";

/// PTY: a mouse-down on the blank gap below the conversation (between the
/// turn marker and the prompt box) arms an anchor-less drag — dead space
/// is a valid drag start — and the anchor materializes at the first drag
/// position that lands on selectable text: here a word inside the last
/// message. The payload is the entry-to-release slice of that single line
/// — not a snap to the press-nearest text, not a block copy.
///
/// `SSH_CONNECTION` forces the OSC 52 clipboard route for readback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn drag_enters_content_from_gap_pty() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(GAPDEEP_LINE.to_string());

    let binary = pager_binary().expect("resolve pager binary");
    let overrides: Vec<(String, String)> = vec![(
        "SSH_CONNECTION".into(),
        "scripted-test 1 127.0.0.1 2".into(),
    )];
    let env_refs: Vec<(&str, &str)> = overrides
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let mut harness = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &env_refs,
        Some(content.home()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(ENTRY_WORD, Duration::from_secs(45))
        .expect("message rendered");
    harness
        .wait_for_text("Worked for", Duration::from_secs(20))
        .expect("turn marker rendered");

    harness.inject_keys(b"\t").expect("focus scrollback");
    let _ = harness.wait_for_text("Space:prompt", Duration::from_secs(10));
    harness.update(Duration::from_millis(1500));

    let screen = harness.screen_contents();
    let (msg_row, _) = locate_screen_text(&screen, "GAPDEEP")
        .unwrap_or_else(|| panic!("could not locate GAPDEEP; screen:\n{screen}"));
    let (entry_row, entry_col) = locate_screen_text(&screen, ENTRY_WORD)
        .unwrap_or_else(|| panic!("could not locate {ENTRY_WORD:?}; screen:\n{screen}"));
    assert_eq!(entry_row, msg_row, "setup: single unwrapped message line");
    let (marker_row, _) = locate_screen_text(&screen, "Worked for")
        .unwrap_or_else(|| panic!("could not locate the turn marker; screen:\n{screen}"));
    assert!(marker_row > msg_row, "setup: marker below the message");

    // The gap row below the marker must be fully blank (above the prompt box).
    let gap_row = marker_row + 1;
    let gap_line = screen.lines().nth(gap_row as usize).unwrap_or("");
    assert!(
        gap_line.trim().is_empty(),
        "setup: the press row must be a blank gap; line: {gap_line:?}"
    );

    // PRESS in the gap, then drag up into the message. The motion samples
    // jump the marker row deliberately (terminals coalesce motion): the
    // same-row column clamp makes the marker's line hittable at any column
    // of its row, so a sample there would anchor the drag on the marker —
    // the correct first-text-entered answer for that path, but not this
    // test's subject. First sample on the message = anchor at the word's
    // first column; then extend to its last column and release.
    let head_col = entry_col + ENTRY_WORD.len() as u16 - 1;
    let mut drag = String::new();
    drag.push_str(&sgr_mouse(0, gap_row, entry_col, 'M'));
    drag.push_str(&sgr_mouse(32, entry_row, entry_col, 'M'));
    drag.push_str(&sgr_mouse(32, entry_row, head_col, 'M'));
    drag.push_str(&sgr_mouse(0, entry_row, head_col, 'm'));
    harness
        .inject_keys(drag.as_bytes())
        .expect("press the gap, drag up into the message");

    let payloads = wait_for_osc52_payloads(&mut harness, Duration::from_secs(10));
    assert!(
        !payloads.is_empty(),
        "expected an OSC 52 clipboard write after release; screen:\n{}",
        harness.screen_contents()
    );
    let joined = payloads.join("\n");
    assert_eq!(
        joined, ENTRY_WORD,
        "payload must be the entry-to-release slice of the entered line \
         (anchor at text entry, not at the press or a snap); payloads={payloads:?}"
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
