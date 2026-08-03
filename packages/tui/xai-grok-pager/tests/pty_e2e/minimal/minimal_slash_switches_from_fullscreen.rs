// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// `/minimal` from a fullscreen session re-execs the pager with `--minimal
/// --resume <id>` so the same conversation reopens under scrollback-native
/// rendering. Proves the end-to-end screen-mode switch path (slash command →
/// quit → exec → resume in minimal) that unit tests cannot cover.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_slash_switches_from_fullscreen() {
    let content = ContentController::start().await.expect("start content");
    let sentinel = turn_sentinel(1);
    content.set_response(format!("{sentinel} fullscreen payload."));

    // Stable project dir so the resumed session is findable by id after re-exec.
    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");

    let binary = pager_binary().expect("resolve pager binary");
    // Start fullscreen (default), standalone. Enable query responses *before*
    // the re-exec so the post-switch minimal probe does not silently downgrade
    // to full-height inline.
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--no-leader"],
        Some(project.path()),
    )
    .expect("spawn fullscreen pager");
    harness.set_respond_to_queries(true);

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // Establish a real session with content so `--resume` has history to load.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit turn");
    harness
        .wait_for_text(&sentinel, Duration::from_secs(30))
        .expect("mock response in fullscreen");

    // Switch: `/minimal` should re-exec into scrollback-native mode with the
    // same session. Pace keystrokes so the slash dropdown opens rather than
    // paste-coalescing, then confirm once the description row is visible.
    inject_keys_paced(&mut harness, b"/minimal");
    harness
        .wait_for_text(
            "Reopen this session in minimal (scrollback-native) mode",
            Duration::from_secs(5),
        )
        .expect("slash dropdown offers /minimal");
    harness.update(Duration::from_millis(150));
    harness.inject_keys(b"\r").expect("submit /minimal");

    // After the relaunch the PTY stays live (Unix: same process via `exec`;
    // Windows: child on the same console with the parent parked in `wait`);
    // wait for minimal's idle status. A `/minimal` re-exec shows the
    // switch-back form (`… /fullscreen to go back · /help`), not the cold-start
    // `minimal · /help` sentinel alone.
    harness
        .wait_for_text(MINIMAL_SWITCH_BACK_IDLE_SENTINEL, Duration::from_secs(45))
        .unwrap_or_else(|e| {
            panic!(
                "/minimal did not reopen session in minimal mode: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    harness
        .wait_for_full_text(&sentinel, Duration::from_secs(30))
        .unwrap_or_else(|e| {
            panic!(
                "prior turn must be present after /minimal resume: {e}\nfull:\n{}",
                harness.full_text()
            )
        });

    // Main-screen clear on relaunch: "Reopening session…" was printed just
    // before exec and must not remain above the resumed UI (the clear wipes
    // residual main-buffer detritus so the welcome card sits at the top).
    let screen = harness.screen_contents();
    assert!(
        !screen.contains("Reopening session"),
        "main screen should be cleared on /minimal relaunch; leftover reopen text:\n{screen}"
    );
    assert!(
        screen.contains("Grok Build") || harness.full_text().contains("Grok Build"),
        "welcome card should re-anchor at top after /minimal relaunch\nscreen:\n{screen}"
    );

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked after /minimal\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
