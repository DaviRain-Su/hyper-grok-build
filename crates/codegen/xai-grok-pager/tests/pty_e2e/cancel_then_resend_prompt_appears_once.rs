// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// The `cancel_then_send` confounder: submit, Ctrl+C (pristine rewind), then
/// Enter to resend the restored text. The prompt must appear EXACTLY ONCE in
/// scrollback and in each wire request — the rewound turn's copy must not
/// survive in session history and pair with the resend as 2x.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn cancel_then_resend_prompt_appears_once() {
    const RESEND_PROMPT: &str = "resend me exactly once";

    let content = ContentController::start().await.expect("start content");
    // Turn 1 is rewound pre-first-token (the 30s pacing guarantees the
    // pristine window); turn 2 is the resend's reply, streamed after the
    // pacing is dropped below.
    let _rewound_turn =
        content.expect_agent_turn("rewound turn before first token", "GONE never streams.");
    let _resent_turn =
        content.expect_agent_turn("resent prompt turn", "RESENT_REPLY to the restored prompt.");
    content.set_chunk_delay(Some(Duration::from_secs(30)));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{RESEND_PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_until(
            "prompt block committed and composer cleared",
            Duration::from_secs(30),
            |h| block_lines_containing(h, RESEND_PROMPT) == 1 && !composer_holds(h, RESEND_PROMPT),
        )
        .expect("prompt block committed");
    harness
        .wait_for_text("Waiting for response", Duration::from_secs(25))
        .expect("turn running pre-first-token");

    harness.inject_keys(keys::CTRL_C).expect("Ctrl+C rewind");
    // Require the rewound state to *settle*, not just flash true: the pager
    // clears the scrollback block optimistically, but the shell's trim of the
    // rewound copy from session history is a separate round-trip. On a slow,
    // contended runner, resending during that gap pairs the stale copy with
    // the resend in one wire request (the 2x this test guards). Holding the
    // rewound state continuously lets the trim land first.
    harness
        .wait_until_stable(
            "rewound prompt restored after session history trim",
            Duration::from_secs(30),
            Duration::from_millis(1500),
            |h| composer_holds(h, RESEND_PROMPT) && block_lines_containing(h, RESEND_PROMPT) == 0,
        )
        .expect("rewound prompt restored after history trim");

    // Resend the restored text as a fresh turn.
    content.set_chunk_delay(None);
    harness.inject_keys(b"\r").expect("Enter resends");
    harness
        .wait_for_text("RESENT_REPLY", Duration::from_secs(90))
        .expect("resent turn reply");

    // Exactly once in scrollback (block back, composer empty again).
    harness
        .wait_until(
            "resent prompt rendered exactly once",
            Duration::from_secs(30),
            |h| block_lines_containing(h, RESEND_PROMPT) == 1 && !composer_holds(h, RESEND_PROMPT),
        )
        .expect("resent prompt rendered exactly once");

    // Exactly once per wire request: the rewound copy must have been cut from
    // session history, so no request pairs a stale copy with the resend.
    for body in content.request_bodies() {
        // Chat Completions carries `messages`; the Responses shape `input`.
        let items = body["messages"]
            .as_array()
            .or_else(|| body["input"].as_array());
        let users: Vec<&serde_json::Value> = items
            .into_iter()
            .flatten()
            .filter(|m| {
                m["role"] == "user"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains(RESEND_PROMPT))
            })
            .collect();
        assert!(
            users.len() <= 1,
            "prompt duplicated in one request (stale rewound copy + resend): {body}"
        );
    }

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.quit().expect("clean quit");
}
