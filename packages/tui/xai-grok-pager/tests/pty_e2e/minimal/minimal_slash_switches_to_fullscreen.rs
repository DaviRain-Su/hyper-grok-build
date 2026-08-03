// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// `/fullscreen` from a minimal session re-execs the pager without `--minimal`
/// and with `--resume <id>`, reopening the same conversation under the
/// fullscreen alt-screen TUI. The reverse of `minimal_slash_switches_from_fullscreen`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_slash_switches_to_fullscreen() {
    let content = ContentController::start().await.expect("start content");
    let sentinel = turn_sentinel(1);
    content.set_response(format!("{sentinel} minimal payload."));

    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");

    let mut harness =
        spawn_minimal_in_dir(&content, DEFAULT_ROWS, DEFAULT_COLS, &[], project.path());
    wait_minimal_ready(&mut harness);

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit turn");
    harness
        .wait_for_full_text(&sentinel, Duration::from_secs(30))
        .expect("turn committed in minimal");

    // Switch back to fullscreen. Wait for the dropdown row so Enter confirms
    // the command (not a bare paste of the text).
    inject_keys_paced(&mut harness, b"/fullscreen");
    harness
        .wait_for_text(
            "Reopen this session in fullscreen mode",
            Duration::from_secs(5),
        )
        .expect("slash dropdown offers /fullscreen");
    harness.update(Duration::from_millis(150));
    harness.inject_keys(b"\r").expect("submit /fullscreen");

    // Prior turn content is already on the minimal screen, so we cannot use
    // `wait_for_text(sentinel)` as the transition signal — it would return
    // immediately. Wait until the minimal idle status line is gone (proves we
    // left scrollback-native mode) while the prior turn remains visible.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        let left_minimal = !screen.contains(MINIMAL_IDLE_SENTINEL)
            && !screen.contains(MINIMAL_SWITCH_BACK_IDLE_SENTINEL)
            && !screen.contains("Reopen this session in fullscreen mode");
        let history_present = screen.contains(&sentinel) || harness.full_text().contains(&sentinel);
        if left_minimal && history_present {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "/fullscreen did not leave minimal mode with history intact\nscreen:\n{}\nfull:\n{}",
                harness.screen_contents(),
                harness.full_text()
            );
        }
    }

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked after /fullscreen\nscreen:\n{}",
        harness.screen_contents()
    );

    // Slash-command mode switches are session-scoped: `/fullscreen` relaunch
    // must not write `[ui] screen_mode` (manual config only).
    let config_path = content.home().join(".grok").join("config.toml");
    // Brief settle so a fire-and-forget write would have landed if still present.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        harness.update(Duration::from_millis(100));
    }
    let body = std::fs::read_to_string(&config_path).unwrap_or_default();
    assert!(
        !body.contains("screen_mode"),
        "/fullscreen must not persist [ui] screen_mode; config.toml:\n{body}"
    );

    harness.quit().expect("clean quit");
}
