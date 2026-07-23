//! PTY: a re-parked wait (new parent output between parks) pushes a fresh
//! parked marker for the new park episode, so the transcript keeps a
//! boundary where each park began, while the persistent "… still running"
//! status row explains the still-running background work.
//!
//! Wire journey, flag-file driven like `endline_park_two_static_markers`:
//! background a flag-gated command, hold on a flag-gated foreground command
//! while the runtime task id is extracted, then script three more rounds on
//! the real id — a short wait (`timeout_ms: 4000`) that expires with the
//! task still running (park #1 + marker), a quick foreground echo, and a
//! long wait (park #2: chrome hidden and a fresh marker for the new episode).
#[allow(unused_imports)]
use super::common::*;

/// Running-turn keybar hint; absent while the parked look is active.
#[cfg(unix)]
const CANCEL_HINT: &str = "Esc:cancel";

/// Between-parks sentinel: collapsed execute blocks render "Run
/// <description>", not the command's stdout.
#[cfg(unix)]
const MIDWORK: &str = "between-parks content";

/// Final scripted answer after park #2's wait returns.
#[cfg(unix)]
const FINAL: &str = "REPARK_FINAL_ANSWER";

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn reparked_wait_repushes_buried_marker() {
    let content = ContentController::start().await.expect("start content");
    // Gates the background command both waits block on (released at the end).
    let park_flag = content.home().join("repark_flag");
    // Gates the id-extraction hold: created once the wait scripts are enqueued.
    let id_ready_flag = content.home().join("repark_id_ready_flag");

    let gated_loop = |flag: &std::path::Path| {
        format!("while [ ! -e {} ]; do /bin/sleep 0.2; done", flag.display())
    };

    // Tool call 1: the flag-gated background command.
    let bg_args = json!({
        "command": gated_loop(&park_flag),
        "description": "flag-gated command",
        "is_background": true
    })
    .to_string();
    let _background_turn =
        expect_tool_turn(&content, "call_repark_bg", "run_terminal_command", bg_args);

    // Tool call 2: the flag-gated foreground hold for id extraction.
    let id_hold_args = json!({
        "command": gated_loop(&id_ready_flag),
        "description": "hold for id extraction"
    })
    .to_string();
    let _id_hold_turn = expect_tool_turn(
        &content,
        "call_repark_id_hold",
        "run_terminal_command",
        id_hold_args,
    );

    // Fallback for the post-wait continuation once park #2's wait returns.
    content.set_response(FINAL);

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust"],
        Some(content.home()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");

    // The runtime task id rides in the follow-up request's tool result
    // (<task-id>…</task-id>) — a UUID minted by the terminal actor.
    let task_id = poll_for(Duration::from_secs(60), || {
        content
            .request_bodies()
            .iter()
            .find_map(|b| extract_task_id(&b.to_string()))
    })
    .unwrap_or_else(|| {
        panic!(
            "no <task-id> in any request body\n--- non-system messages ---\n{}\n--- screen ---\n{}",
            dump_non_system_messages(&content.request_bodies()),
            harness.screen_contents()
        )
    });

    // Tool call 3 — park #1: a short wait that expires with the task still
    // running.
    let short_wait_args = json!({
        "task_ids": [task_id],
        "timeout_ms": 4_000
    })
    .to_string();
    let _short_wait_turn = expect_tool_turn(
        &content,
        "call_repark_wait1",
        "get_command_or_subagent_output",
        short_wait_args,
    );

    // Tool call 4: foreground work between the parks (`MIDWORK` is the
    // on-screen sentinel).
    let midwork_args = json!({
        "command": "echo repark-midwork-done",
        "description": MIDWORK
    })
    .to_string();
    let _midwork_turn = expect_tool_turn(
        &content,
        "call_repark_midwork",
        "run_terminal_command",
        midwork_args,
    );

    // Tool call 5 — park #2: the long wait on the same still-running task.
    let long_wait_args = json!({
        "task_ids": [task_id],
        "timeout_ms": 600_000
    })
    .to_string();
    let _long_wait_turn = expect_tool_turn(
        &content,
        "call_repark_wait2",
        "get_command_or_subagent_output",
        long_wait_args,
    );

    // Everything downstream is scripted — release the id-extraction hold.
    std::fs::write(&id_ready_flag, b"ready").expect("release id-extraction hold");

    // Park #1 marker (plain "Worked for X" — no still-running suffix).
    harness
        .wait_for_text("Worked for", Duration::from_secs(90))
        .unwrap_or_else(|_| {
            panic!(
                "park #1 marker never appeared; screen:\n{}\n--- non-system messages ---\n{}",
                harness.screen_contents(),
                dump_non_system_messages(&content.request_bodies())
            )
        });
    // The parked status row carries the still-running story instead.
    harness
        .wait_for_text("1 command still running", Duration::from_secs(30))
        .unwrap_or_else(|_| {
            panic!(
                "parked watching cue never appeared; screen:\n{}",
                harness.screen_contents()
            )
        });

    // The short wait expires and the same turn resumes.
    harness
        .wait_for_text(MIDWORK, Duration::from_secs(60))
        .unwrap_or_else(|_| {
            panic!(
                "between-parks content never rendered; screen:\n{}\n--- non-system messages ---\n{}",
                harness.screen_contents(),
                dump_non_system_messages(&content.request_bodies())
            )
        });

    // Park #2: the running chrome drops again.
    let chrome_hidden = wait_until(Duration::from_secs(30), || {
        harness.update(Duration::from_millis(100));
        !harness.contains_text(CANCEL_HINT)
    });
    assert!(
        chrome_hidden,
        "park #2 must drop the running chrome ({CANCEL_HINT}); screen:\n{}",
        harness.screen_contents()
    );

    // Park #2 pushes a second marker below the between-parks content (a new
    // park episode after new parent output).
    let repushed = wait_until(Duration::from_secs(30), || {
        harness.update(Duration::from_millis(100));
        harness.screen_contents().matches("Worked for").count() == 2
    });
    assert!(
        repushed,
        "re-park after buried marker must push a fresh marker; screen:\n{}",
        harness.screen_contents()
    );
    let screen = harness.screen_contents();

    // Screen text is row-major: marker, content, re-pushed marker in order.
    let first_marker = screen.find("Worked for").expect("first marker");
    let midwork_at = screen
        .rfind(MIDWORK)
        .expect("between-parks content on screen");
    let second_marker = screen.rfind("Worked for").expect("re-pushed marker");
    assert!(
        first_marker < midwork_at && midwork_at < second_marker,
        "expected marker, content, then the re-pushed marker in order; screen:\n{screen}"
    );
    // The still-running story lives in the status row, not the transcript:
    // no "Worked for" line carries the suffix (line-scoped like the sibling
    // suites — other surfaces may legitimately use the phrase).
    assert!(
        screen
            .lines()
            .filter(|l| l.contains("Worked for"))
            .all(|l| !l.contains("still running")),
        "no marker line may carry the still-running suffix; screen:\n{screen}"
    );
    assert!(
        screen.contains("1 command still running"),
        "the parked status row keeps the still-running cue during park #2; screen:\n{screen}"
    );
    // The parked look still hides spinner and chrome.
    let below_midwork = &screen[midwork_at..];
    assert!(
        !below_midwork
            .chars()
            .any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)),
        "parked look keeps the spinner hidden during park #2; screen:\n{screen}"
    );
    assert!(
        !screen.contains(CANCEL_HINT),
        "parked look keeps the running chrome hidden during park #2; screen:\n{screen}"
    );

    eprintln!("── re-park with buried marker: tail explains the park ──\n{screen}\n── end ──");

    // Liveness: releasing the flag completes the wait and the same turn
    // streams the final answer.
    std::fs::write(&park_flag, b"done").expect("release flag");
    harness
        .wait_for_text(FINAL, Duration::from_secs(90))
        .unwrap_or_else(|_| {
            panic!(
                "post-wait continuation never streamed; screen:\n{}\n--- non-system messages ---\n{}",
                harness.screen_contents(),
                dump_non_system_messages(&content.request_bodies())
            )
        });

    harness
        .wait_for_turn_idle(Duration::from_secs(15))
        .expect("turn idle");
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    write_cast_if_requested(&harness, "reparked_wait_repushes_buried_marker.cast");
    harness.quit().expect("clean quit");
}
