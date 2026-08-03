// Per-test-case module for the `pty_e2e` integration test crate.
//
// Regression pin: Tab inside the `ask_user_question` card used to hand focus
// to the scrollback while the card stayed drawn and the shortcuts bar kept
// advertising card keys.
#[allow(unused_imports)]
use super::common::*;

const FIRST_QUESTION: &str = "What kind of work are you most interested in right now?";
const SECOND_QUESTION: &str = "How deep should the answer go?";

/// Answer rows of each question in render order.
const FIRST_ROWS: [&str; 4] = [
    "Writing or editing code",
    "Explaining the codebase",
    "Debugging a failure",
    "Type your answer here",
];
const SECOND_ROWS: [&str; 3] = ["A quick summary", "Every detail", "Type your answer here"];

const DONE_SENTINEL: &str = "QUESTIONTABDONE";

const TAB: &[u8] = b"\t";
/// `BackTab` — the xterm encoding of Shift+Tab.
const SHIFT_TAB: &[u8] = b"\x1b[Z";

const FOCUSED_HINT: &str = "Tab:next answer";

fn ask_user_question_args() -> String {
    let option =
        |label: &str, description: &str| json!({ "label": label, "description": description });
    json!({
        "questions": [
            {
                "question": FIRST_QUESTION,
                "options": [
                    option(FIRST_ROWS[0], "Implement features or fix bugs"),
                    option(FIRST_ROWS[1], "Understand how things work"),
                    option(FIRST_ROWS[2], "Track down a flake"),
                ],
            },
            {
                "question": SECOND_QUESTION,
                "options": [
                    option(SECOND_ROWS[0], "Just the headline"),
                    option(SECOND_ROWS[1], "Walk me through it"),
                ],
            },
        ]
    })
    .to_string()
}

/// Text of the answer row that currently carries the cursor band.
///
/// Reading the styled screen is the only way to observe cursor position from
/// outside the process.
fn cursor_row(harness: &PtyHarness) -> Option<String> {
    let labels: Vec<&str> = FIRST_ROWS
        .iter()
        .chain(SECOND_ROWS.iter())
        .copied()
        .collect();
    let mut rows: Vec<(String, String)> = Vec::new();
    for line in harness.screen_styled() {
        let text: String = line.runs.iter().map(|r| r.text.as_str()).collect();
        let Some(label) = labels.iter().find(|label| text.contains(**label)) else {
            continue;
        };
        let bg = line
            .runs
            .iter()
            .find(|run| run.text.contains(*label))
            .and_then(|run| run.bg.clone())
            .unwrap_or_default();
        rows.push(((*label).to_string(), bg));
    }
    rows.iter()
        .find(|(_, bg)| rows.iter().filter(|(_, other)| other == bg).count() == 1)
        .map(|(label, _)| label.clone())
}

fn expect_cursor_row(harness: &mut PtyHarness, expected: &str, step: &str) {
    let outcome = harness.wait_until(
        &format!("{step}: cursor on {expected:?}"),
        CURSOR_TIMEOUT,
        |h| cursor_row(h).as_deref() == Some(expected),
    );
    eprintln!("[cursor] {step}: {:?}", cursor_row(harness));
    outcome.unwrap_or_else(|e| panic!("{e}"));
}

fn expect_text(harness: &mut PtyHarness, needle: &str, step: &str) {
    eprintln!("[screen] {step}: waiting for {needle:?}");
    harness
        .wait_for_text(needle, CURSOR_TIMEOUT)
        .unwrap_or_else(|e| panic!("{step}: {e}"));
}

const CURSOR_TIMEOUT: Duration = Duration::from_secs(10);

/// Every string leaf of a recorded request body.
fn string_leaves(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => out.push(text.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|item| string_leaves(item, out)),
        serde_json::Value::Object(fields) => {
            fields.values().for_each(|field| string_leaves(field, out))
        }
        _ => {}
    }
}

/// Drive a real two-question `ask_user_question` card all the way round its
/// answer walk, both directions, and submit from where Tab left off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn question_tab_cycles_answers() {
    let content = ContentController::start().await.expect("start content");
    let _turn = expect_tool_turn(
        &content,
        "call_ask_tab",
        "ask_user_question",
        ask_user_question_args(),
    );
    content.set_response(DONE_SENTINEL);

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
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(FIRST_ROWS[2], Duration::from_secs(30))
        .expect("question card renders");
    expect_cursor_row(
        &mut harness,
        FIRST_ROWS[0],
        "card opens on the first answer",
    );
    write_screen_dump_if_requested(&harness, "question_tab_00_card_open");

    harness.inject_keys(b" ").expect("Space marks the answer");

    for (step, expected) in FIRST_ROWS.iter().enumerate().skip(1) {
        harness.inject_keys(TAB).expect("Tab");
        expect_cursor_row(&mut harness, expected, &format!("Tab #{step}"));
    }
    write_screen_dump_if_requested(&harness, "question_tab_01_first_question_walked");

    harness.inject_keys(TAB).expect("Tab into question 2");
    expect_cursor_row(&mut harness, SECOND_ROWS[0], "Tab crosses into question 2");
    expect_text(
        &mut harness,
        "[2/2]",
        "question counter follows the answer walk",
    );
    write_screen_dump_if_requested(&harness, "question_tab_02_second_question");

    harness.inject_keys(SHIFT_TAB).expect("Shift+Tab back");
    expect_cursor_row(&mut harness, FIRST_ROWS[3], "Shift+Tab crosses back");
    harness.inject_keys(TAB).expect("Tab into question 2 again");
    expect_cursor_row(&mut harness, SECOND_ROWS[0], "and forward again");

    for _ in 0..SECOND_ROWS.len() {
        harness.inject_keys(TAB).expect("Tab");
    }
    expect_cursor_row(
        &mut harness,
        FIRST_ROWS[0],
        "Tab past the last answer wraps",
    );
    expect_text(&mut harness, "[1/2]", "the wrap lands back on question 1");
    write_screen_dump_if_requested(&harness, "question_tab_03_wrapped_to_first");
    assert!(
        harness.contains_text(FOCUSED_HINT),
        "the card still owns the keyboard after the wrap\nscreen:\n{}",
        harness.screen_contents()
    );

    harness
        .inject_keys(SHIFT_TAB)
        .expect("Shift+Tab wraps back");
    expect_cursor_row(&mut harness, SECOND_ROWS[2], "Shift+Tab wraps to the last");
    expect_text(
        &mut harness,
        "[2/2]",
        "the backwards wrap lands on question 2",
    );
    write_screen_dump_if_requested(&harness, "question_tab_04_wrapped_to_last");

    harness.inject_keys(SHIFT_TAB).expect("Shift+Tab");
    expect_cursor_row(
        &mut harness,
        SECOND_ROWS[1],
        "Shift+Tab back onto an answer",
    );
    harness.inject_keys(b"\r").expect("submit answers");
    harness
        .wait_for_text(DONE_SENTINEL, Duration::from_secs(30))
        .expect("agent turn resumes after the answers");
    write_screen_dump_if_requested(&harness, "question_tab_99_submitted");

    let mut leaves = Vec::new();
    for body in content.request_bodies() {
        string_leaves(&body, &mut leaves);
    }
    let answered: Vec<&String> = leaves
        .iter()
        .filter(|leaf| leaf.contains("has answered your questions"))
        .collect();
    eprintln!("[tool result] {answered:?}");
    for expected in [
        format!("\"{FIRST_QUESTION}\"=\"{}\"", FIRST_ROWS[0]),
        format!("\"{SECOND_QUESTION}\"=\"{}\"", SECOND_ROWS[1]),
    ] {
        assert!(
            answered.iter().any(|leaf| leaf.contains(&expected)),
            "tool result should carry {expected:?}, got {answered:?}"
        );
    }

    harness.quit().expect("clean quit");
}
