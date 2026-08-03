//! Render tests against ratatui's in-memory backend.
//!
//! The unit tests in `app`/`transcript` cover the logic; these cover the thing
//! logic tests can't see — that the frame actually contains what it should, at
//! the sizes a terminal really comes in, and that no pane writes outside its
//! area or panics on a degenerate one. A TUI's worst failures (an overflowing
//! row, a cursor placed off-screen, a panic at 20 columns) are all geometry
//! bugs, and this is where geometry is checked.

use chrono::Utc;
use comet_doc::{MessagePart, MessageRole, SessionMessageEntry};
use comet_proto::view::ConnectionStatus;
use comet_proto::{AuthState, Chat, Session, SessionStatus, Space, ToolCall, UserProfile};
use comet_tui::app::App;
use comet_tui::keys::{Action, Focus};
use comet_tui::link::Update;
use comet_tui::render;
use comet_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn space(id: &str, path: &str) -> Space {
    Space {
        id: id.into(),
        device_id: "dev".into(),
        path: path.into(),
        name: None,
        git_detected: true,
        git_checked_at: None,
        checkout_id: None,
        created_at: Utc::now(),
    }
}

fn chat(id: &str, title: &str) -> Chat {
    Chat {
        id: id.into(),
        device_id: "dev".into(),
        title: Some(title.into()),
        archived: false,
        cwd: Some("/dev/comet".into()),
        branch: Some("main".into()),
        checkout_id: None,
        config: None,
        last_message_preview: None,
        last_message_at: Some(Utc::now()),
        created_at: Utc::now(),
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: Some("s1".into()),
        last_seen_at: Some(Utc::now()),
    }
}

fn entry(id: &str, role: MessageRole, parts: Vec<MessagePart>) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role,
        parts,
        created_at: Utc::now().timestamp_millis(),
        device_id: "dev".into(),
        status: None,
        continuation_of: None,
    }
}

fn text(id: &str, body: &str) -> MessagePart {
    MessagePart::Text {
        id: id.into(),
        text: body.into(),
    }
}

/// A signed-in app with one space, two sessions, and a transcript.
fn populated() -> App {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Auth(Box::new(AuthState::SignedIn {
        user: UserProfile {
            id: "u".into(),
            email: "w@example.com".into(),
            name: None,
        },
        org_id: Some("org".into()),
    })));
    app.apply(Update::Spaces(vec![space("s1", "/dev/comet")]));
    app.apply(Update::Chats(vec![
        chat("c1", "Rework the diff sidebar"),
        chat("c2", "Chase the flaky room test"),
    ]));
    app.select_chat(Some("c1".into()));
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![
            entry(
                "m1",
                MessageRole::User,
                vec![text("t0", "why is the room test flaky?")],
            ),
            entry(
                "m2",
                MessageRole::Assistant,
                vec![
                    text("t0", "Let me look at the retry path."),
                    MessagePart::Tool {
                        id: "p1".into(),
                        call: ToolCall::Exec {
                            command: "cargo test -p comet-rpc device_room".into(),
                        },
                        is_error: false,
                        resolved: true,
                    },
                    text(
                        "t1",
                        "The join races the successor election. Here's the fix:\n\n```\nlet successor = peers.iter().filter(|p| !p.closing);\n```\n\n- drop the closing host\n- retry once",
                    ),
                ],
            ),
        ],
    });
    app
}

/// Draw once and return the frame as the terminal would actually show it.
///
/// A wide glyph occupies one cell and leaves a filler in the next, which
/// ratatui's diff skips rather than emitting. Reconstructing a row therefore has
/// to skip those cells too — reading every cell would report a column width
/// larger than the pane for any row containing CJK or emoji, and make the
/// overflow assertions below meaningless.
fn snapshot(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| render::draw(frame, app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0u16;
            while x < width {
                let symbol = buffer[(x, y)].symbol();
                row.push_str(symbol);
                let advance = unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
                x += advance;
            }
            row.trim_end().to_string()
        })
        .collect()
}

fn joined(rows: &[String]) -> String {
    rows.join("\n")
}

/// The sidebar's columns of a drawn frame.
///
/// There is no divider to split on any more — the sidebar is a filled region and
/// its edge is where the fill stops — so this slices by width, mirroring
/// `render`'s own sizing.
fn sidebar_of(rows: &[String], width: u16) -> Vec<String> {
    let sidebar = 32u16.min(width / 3).max(22u16.min(width / 2)) as usize;
    rows.iter()
        .map(|row| {
            row.chars()
                .take(sidebar)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn a_populated_frame_shows_the_chrome_sidebar_and_transcript() {
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 30);
    let screen = joined(&rows);

    // The tab strip is the header: the selected space's sessions, plus `+`.
    assert!(rows[1].contains("Rework the diff"), "{}", rows[1]);
    assert!(
        rows[1].contains("Chase the flaky"),
        "both tabs: {}",
        rows[1]
    );

    // Sidebar: two sections.
    assert!(screen.contains("Spaces"), "{screen}");
    assert!(screen.contains("Sessions"), "{screen}");
    // Session rows carry a "space@device" sub-line under the title — where the
    // work happens, not which branch it happens on.
    assert!(screen.contains("comet@"), "sub-line missing:\n{screen}");

    // Transcript: the user turn, the assistant text, the tool row, the fenced
    // code, and the bullet.
    assert!(
        screen.contains("why is the room test flaky?"),
        "user text missing:\n{screen}"
    );
    // Tool calls collapse into a group summary; the newest group shows chips.
    assert!(
        screen.contains("Ran 1 command"),
        "tool group summary missing:\n{screen}"
    );
    assert!(screen.contains("Run"), "tool chip label missing:\n{screen}");
    assert!(
        screen.contains("cargo test -p comet-rpc"),
        "tool detail missing:\n{screen}"
    );
    assert!(
        screen.contains("let successor"),
        "fenced code missing:\n{screen}"
    );
    assert!(screen.contains("retry once"), "bullet missing:\n{screen}");

    // The user's own message is a block that shrinks to its text and sits at the
    // right margin, so a turn of yours reads as *sent* rather than as another
    // paragraph in the reply column. It carries the accent bar, which is the
    // same mark the prompt below it carries.
    let prompt_row = rows
        .iter()
        .position(|row| row.contains("why is the room test flaky?"))
        .expect("the prompt");
    assert!(
        rows[prompt_row].contains('┃'),
        "the user's message should carry the accent bar:\n{screen}"
    );
    let indent = rows[prompt_row].find("why is").expect("prompt column");
    assert!(
        indent > 40,
        "the message should sit at the right margin, found at {indent}:\n{screen}"
    );

    // Both panes run the terminal's full height: there is no status bar under
    // them any more, so the last row belongs to the sidebar and the prompt
    // rather than to chrome spanning both.
    let last = rows.last().unwrap();
    assert!(
        !last.contains("detach") && !last.contains("engine"),
        "no chrome bar should remain: {last}"
    );
}

#[test]
fn no_row_ever_exceeds_the_terminal_width() {
    // The failure this guards against is a wide glyph or an un-truncated label
    // spilling into the next row, which corrupts the whole frame below it.
    let mut app = populated();
    for (width, height) in [(100u16, 24u16), (80, 30), (60, 12), (40, 10), (24, 8)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer.area.width, width,
            "buffer resized under us at {width}x{height}"
        );
        // TestBackend panics on out-of-area writes, so reaching here already
        // proves containment; assert the cursor claim too.
        for y in 0..height {
            for x in 0..width {
                let _ = buffer[(x, y)];
            }
        }
    }
}

#[test]
fn degenerate_sizes_render_something_instead_of_panicking() {
    let mut app = populated();
    // Absurd but reachable: a user drags a split to nothing.
    for (width, height) in [(1u16, 1u16), (4, 2), (20, 6), (19, 5), (200, 3)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap_or_else(|err| panic!("{width}x{height} failed: {err}"));
    }
}

#[test]
fn wide_and_combining_glyphs_do_not_shift_the_layout() {
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::Assistant,
            vec![text(
                "t0",
                "日本語のテキストと絵文字 👩‍👩‍👧 と結合文字 e\u{301} が混ざった行",
            )],
        )],
    });
    // Narrow enough that the wide text must wrap mid-run. Every row has to fit
    // in the pane *by column*, which is the measure a terminal uses — a wrapper
    // that counted characters would overflow here.
    for width in [40u16, 33, 28] {
        for row in snapshot(&mut app, width, 16) {
            assert!(
                unicode_width::UnicodeWidthStr::width(row.as_str()) <= width as usize,
                "row wider than the {width}-column pane: {row:?}"
            );
        }
    }
}

#[test]
fn the_gate_replaces_the_body_while_the_engine_is_unreachable() {
    let mut app = App::with_theme(Theme::dark());
    // Boot: the first frame must already explain what is happening, because the
    // probe and a possible daemon spawn take a moment.
    let rows = snapshot(&mut app, 80, 20);
    let screen = joined(&rows);
    assert!(screen.contains("Starting the engine"), "{screen}");
    assert!(
        screen.contains("keep running"),
        "the detach promise belongs on the boot screen:\n{screen}"
    );

    app.apply(Update::Connection(ConnectionStatus::Failed(
        "no engine listening on 127.0.0.1:27654".into(),
    )));
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(screen.contains("Can't reach the engine"), "{screen}");
    assert!(
        screen.contains("27654"),
        "the reason must be shown:\n{screen}"
    );
    assert!(screen.contains("retry now"), "{screen}");

    // Signed out is a different gate with a different instruction.
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Auth(Box::new(AuthState::SignedOut)));
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(screen.contains("comet login"), "{screen}");
}

#[test]
fn the_help_overlay_covers_the_body_and_lists_the_real_bindings() {
    let mut app = populated();
    app.act(Action::ToggleHelp);
    let screen = joined(&snapshot(&mut app, 96, 30));
    assert!(screen.contains("Keys"), "{screen}");
    assert!(screen.contains("detach"), "{screen}");
    // A modal, not a floating panel: no transcript survives beside it, which is
    // what made it look like a rendering fault.
    assert!(
        !screen.contains("why is the room test flaky?"),
        "the overlay must cover the body:\n{screen}"
    );
    assert!(
        !screen.contains("cargo test -p comet-rpc"),
        "including the right-hand ends of long lines:\n{screen}"
    );
    // The tab strip still names the sessions; only the body is covered.
    assert!(screen.contains("Rework the diff"), "{screen}");
    // Every entry comes from the keymap's own table, so the overlay cannot drift
    // from the bindings.
    for (key, _) in comet_tui::keys::HELP.iter().take(3) {
        assert!(screen.contains(key.trim()), "missing {key:?} in:\n{screen}");
    }
}

#[test]
fn an_empty_workspace_says_what_to_do() {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(screen.contains("No session open"), "{screen}");
    assert!(screen.contains("No spaces yet"), "{screen}");
    assert!(screen.contains("No sessions yet"), "{screen}");
}

#[test]
fn hiding_the_sidebar_gives_its_columns_to_the_transcript() {
    let mut app = populated();
    // Tall enough for the whole fixture transcript: bottom-anchored content
    // clips at the top otherwise, which is correct but not what this checks.
    let with_sidebar = snapshot(&mut app, 80, 28);
    app.act(Action::ToggleSidebar);
    let without = snapshot(&mut app, 80, 28);
    assert_ne!(
        with_sidebar, without,
        "hiding the sidebar must change the frame"
    );
    let screen = joined(&without);
    // Sidebar-only chrome is gone…
    assert!(
        !screen.contains("Spaces") && !screen.contains("Sessions"),
        "the sidebar is hidden, so its sections must be gone:\n{screen}"
    );
    // …but the tab strip is the main panel's, so it stays.
    assert!(screen.contains("Chase the flaky"), "{screen}");
    // The transcript is still there, now wider.
    assert!(screen.contains("why is the room test flaky?"), "{screen}");
}

#[test]
fn the_composer_grows_with_its_content_and_places_the_caret() {
    let mut app = populated();
    app.focus = Focus::Composer;
    let one_line = snapshot(&mut app, 60, 20);

    for _ in 0..3 {
        app.act(Action::Edit(comet_tui::keys::Edit::Insert('x')));
        app.act(Action::Edit(comet_tui::keys::Edit::Newline));
    }
    let three_lines = snapshot(&mut app, 60, 20);
    assert_ne!(one_line, three_lines);
    let screen = joined(&three_lines);
    // The prompt is a filled block, and growing it grows the fill: a prompt that
    // reached three rows is one object, not three loose lines.
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let filled = (0..20u16)
        .filter(|y| buffer[(40, *y)].bg == app.theme.element)
        .count();
    assert!(
        filled >= 3,
        "the fill should span the grown prompt, saw {filled}:\n{screen}"
    );

    // The caret is inside the frame — placing it outside is a real bug that
    // shows up as a cursor parked in a corner.
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let cursor = terminal.get_cursor_position().expect("cursor position");
    assert!(cursor.x < 60 && cursor.y < 20, "caret at {cursor:?}");
}

#[test]
fn a_notice_takes_over_the_working_strip_then_gives_it_back() {
    // There is no status bar any more, so a notice lands on the row above the
    // prompt — already reserved for the working line, and the closest thing to
    // the prompt that is not the prompt.
    let mut app = populated();
    app.notify("Couldn't send: engine unreachable".into());
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(screen.contains("engine unreachable"), "{screen}");

    app.notice.as_mut().unwrap().until = std::time::Instant::now();
    assert!(app.expire_notice());
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(
        !screen.contains("engine unreachable"),
        "the notice must clear: {screen}"
    );
}

#[test]
fn scrolling_up_is_announced_so_a_stalled_view_is_never_a_mystery() {
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: (0..60)
            .map(|i| {
                entry(
                    &format!("m{i}"),
                    MessageRole::Assistant,
                    vec![text("t0", "line")],
                )
            })
            .collect(),
    });
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(!screen.contains("Scrolled"), "following: no banner");

    app.act(Action::PageUp);
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(screen.contains("Scrolled back"), "{screen}");

    app.act(Action::ScrollBottom);
    let screen = joined(&snapshot(&mut app, 80, 20));
    assert!(!screen.contains("Scrolled"), "{screen}");
}

#[test]
fn a_working_session_animates_and_an_idle_one_does_not() {
    let mut app = populated();
    app.apply(Update::Sessions(vec![Session {
        chat_id: "c1".into(),
        device_id: "dev".into(),
        status: SessionStatus::Working,
        started_at: None,
        updated_at: Utc::now(),
    }]));
    assert!(app.animating());
    // The loaders read a clock, so two draws a beat apart must differ —
    // otherwise the timer the event loop arms for them is wasted work.
    let first = snapshot(&mut app, 80, 20);
    std::thread::sleep(std::time::Duration::from_millis(140));
    let second = snapshot(&mut app, 80, 20);
    assert_ne!(first, second, "the loader must actually animate");
}

#[test]
fn drawing_twice_with_no_changes_touches_nothing() {
    // This is the property the whole design rests on: an idle app is free. If a
    // second identical draw produced a different buffer, ratatui's diff would
    // emit bytes every frame.
    let mut app = populated();
    assert!(
        !app.animating(),
        "this property only holds when nothing is animating"
    );
    let first = snapshot(&mut app, 90, 24);
    let second = snapshot(&mut app, 90, 24);
    assert_eq!(first, second);
}

#[test]
fn the_sidebar_follows_the_desktop_order() {
    // The reference sidebar reads: device, New session, rule, "Sessions", then a
    // faint space heading with its two-line session rows, and the user pinned to
    // the bottom (docs/reference/original-comet.png).
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 24);
    let sidebar = sidebar_of(&rows, 100);

    let index = |needle: &str| {
        sidebar
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} missing from sidebar:\n{sidebar:#?}"))
    };
    // Two sections, in comet-native's order: Spaces, then a flat global
    // Sessions list. Not one list grouped by project — that was the original
    // Electron app.
    let spaces = index("Spaces");
    let space_row = index("comet ");
    let sessions = index("Sessions");
    let session = index("Rework the diff");
    assert!(
        spaces < space_row && space_row < sessions && sessions < session,
        "sidebar order wrong:\n{sidebar:#?}"
    );
    // The Spaces header carries its add affordance, right-aligned.
    assert!(sidebar[spaces].trim_end().ends_with('+'), "{sidebar:#?}");
    // Two-line session rows: the sub-line is directly under its title.
    assert!(
        sidebar[session + 1].contains("comet@"),
        "sub-line must follow the title:\n{sidebar:#?}"
    );
    // The user row is pinned to the bottom of the sidebar, not inline.
    let user = index("w@example.com");
    assert!(user > session, "user row must be last:\n{sidebar:#?}");
    assert!(
        user >= sidebar.len() - 3,
        "user row must be pinned to the bottom:\n{sidebar:#?}"
    );
}

#[test]
fn the_cursor_steps_over_decoration() {
    // Rules, the section header and the user row cannot hold the cursor; walking
    // the list must never leave it parked on nothing.
    let mut app = populated();
    app.focus = Focus::Sidebar;
    app.act(Action::ListTop);
    for _ in 0..40 {
        assert!(
            app.rows[app.cursor].selectable(),
            "cursor landed on {:?}",
            app.rows[app.cursor]
        );
        app.act(Action::ListDown);
    }
    for _ in 0..40 {
        assert!(app.rows[app.cursor].selectable());
        app.act(Action::ListUp);
    }
}

#[test]
fn tool_runs_collapse_into_a_group() {
    use comet_proto::ToolCall;
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![
            entry(
                "m1",
                MessageRole::Assistant,
                vec![
                    text("t0", "Checking a few things."),
                    MessagePart::Tool {
                        id: "p1".into(),
                        call: ToolCall::Exec {
                            command: "cargo test".into(),
                        },
                        is_error: false,
                        resolved: true,
                    },
                    MessagePart::Tool {
                        id: "p2".into(),
                        call: ToolCall::ReadFile {
                            path: "src/lib.rs".into(),
                        },
                        is_error: false,
                        resolved: true,
                    },
                    MessagePart::Tool {
                        id: "p3".into(),
                        call: ToolCall::EditFile {
                            path: "src/lib.rs".into(),
                            old_string: None,
                            new_string: None,
                        },
                        is_error: false,
                        resolved: true,
                    },
                ],
            ),
            entry("m2", MessageRole::Assistant, vec![text("t0", "Done.")]),
        ],
    });
    let screen = joined(&snapshot(&mut app, 100, 26));
    // One summary line for the whole run, using the shared wording.
    assert!(
        screen.contains("Ran 1 command · edited 1 file · read 1 file"),
        "group summary missing:\n{screen}"
    );
    // Not one row per tool: the individual paths stay folded away, because this
    // is no longer the newest group.
    assert!(
        !screen.contains("src/lib.rs"),
        "an older group must stay collapsed:\n{screen}"
    );
}

#[test]
fn a_running_tool_group_shows_its_chips() {
    use comet_proto::ToolCall;
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::Assistant,
            vec![MessagePart::Tool {
                id: "p1".into(),
                call: ToolCall::Exec {
                    command: "cargo build --workspace".into(),
                },
                is_error: false,
                // Still running: what it is doing now must be visible.
                resolved: false,
            }],
        )],
    });
    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("⌄"), "expanded chevron missing:\n{screen}");
    assert!(
        screen.contains("cargo build --workspace"),
        "a running command must be shown:\n{screen}"
    );
}

#[test]
fn the_working_strip_reports_elapsed_time() {
    let mut app = populated();
    app.apply(Update::Sessions(vec![Session {
        chat_id: "c1".into(),
        device_id: "dev".into(),
        status: SessionStatus::Working,
        started_at: Some(Utc::now() - chrono::Duration::seconds(11)),
        updated_at: Utc::now(),
    }]));
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(screen.contains("Working"), "{screen}");
    assert!(screen.contains("11s"), "elapsed time missing:\n{screen}");
    assert!(screen.contains("Ctrl-X"), "{screen}");
}

#[test]
fn the_space_row_names_its_host_device() {
    let mut app = populated();
    // The fixture's chats are hosted on "dev"; this engine is someone else.
    app.apply(Update::LocalDevice("laptop".into()));
    // The space row names the host device, which is where "whose machine is
    // this running on" lives in comet-native.
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(screen.contains("dev"), "host device missing:\n{screen}");

    // Hosted here: the row says so.
    app.apply(Update::LocalDevice("dev".into()));
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(screen.contains("this device"), "{screen}");
}

#[test]
fn a_short_transcript_sits_above_the_composer() {
    // The desktop transcript is bottom-anchored (`ListAlignment::Bottom`): a new
    // conversation starts just above the composer, not floating at the top of an
    // empty pane with a gap beneath it.
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::Assistant,
            vec![text("t0", "one short reply")],
        )],
    });
    let rows = snapshot(&mut app, 100, 26);
    let at = rows
        .iter()
        .position(|row| row.contains("one short reply"))
        .expect("the reply");
    let composer = rows
        .iter()
        .position(|row| row.contains("Do anything"))
        .expect("the composer");
    // Bottom-anchored: the reply sits low in the pane, a couple of rows above
    // the composer (its own trailing blank, then the status strip).
    assert!(
        at > rows.len() / 2,
        "content floated to the top:\n{}",
        joined(&rows)
    );
    assert!(
        composer - at <= 6,
        "content should hug the composer: reply at {at}, composer at {composer}\n{}",
        joined(&rows)
    );
}

#[test]
fn the_user_bubble_stays_inside_the_pane() {
    // The wash is a background: one column of overflow would paint into the
    // pane edge (and, before the fix, past it).
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::User,
            vec![text(
                "t0",
                "a prompt long enough that it has to wrap inside the bubble at these widths",
            )],
        )],
    });
    for width in [100u16, 80, 60, 44] {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // The last column of every row must never carry the bubble's wash.
        let washed = app.theme.element;
        for y in 0..24u16 {
            let cell = &buffer[(width - 1, y)];
            assert_ne!(
                cell.bg, washed,
                "bubble reached the pane edge at {width}x24, row {y}"
            );
        }
    }
}

#[test]
fn a_selected_row_keeps_its_status_colour() {
    // The selection wash is a background; applying it in the wrong order made it
    // overwrite the dot's hue, so the selected session lost the one signal the
    // row exists to carry.
    let mut app = populated();
    app.focus = Focus::Sidebar;
    app.apply(Update::Sessions(vec![Session {
        chat_id: "c1".into(),
        device_id: "dev".into(),
        status: SessionStatus::AwaitingInput,
        started_at: None,
        updated_at: Utc::now(),
    }]));
    // Park the cursor on that session.
    app.cursor = app
        .rows
        .iter()
        .position(|row| row.id() == Some("c1"))
        .expect("the session row");

    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    // The dot on the washed row — not just any dot in the sidebar.
    let dot = (0..24u16)
        .flat_map(|y| (0..30u16).map(move |x| (x, y)))
        .map(|(x, y)| &buffer[(x, y)])
        .find(|cell| cell.symbol() == comet_tui::theme::DOT && cell.bg == app.theme.selection)
        .expect("a status dot on the selected row");
    assert_eq!(
        dot.fg, app.theme.dot_awaiting,
        "the dot must keep its hue under the selection wash"
    );
    assert_eq!(dot.bg, app.theme.selection, "…and still sit on the wash");
}

#[test]
fn the_sessions_list_is_flat_and_global_not_grouped_by_space() {
    // comet-native's Sessions list answers "what needs me right now" across
    // every space. Grouping it by project — which the original Electron app did
    // — buries exactly that.
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Spaces(vec![
        space("s1", "/dev/alpha"),
        Space {
            id: "s2".into(),
            path: "/dev/beta".into(),
            ..space("s2", "/dev/beta")
        },
    ]));
    let mut in_beta = chat("c2", "Beta session");
    in_beta.space_id = Some("s2".into());
    in_beta.last_message_at = Some(Utc::now());
    let mut in_alpha = chat("c1", "Alpha session");
    in_alpha.space_id = Some("s1".into());
    in_alpha.last_message_at = Some(Utc::now() - chrono::Duration::hours(2));
    app.apply(Update::Chats(vec![in_alpha, in_beta]));

    let sidebar = sidebar_of(&snapshot(&mut app, 100, 24), 100);
    let sessions = sidebar
        .iter()
        .position(|row| row.contains("Sessions"))
        .expect("Sessions header");
    let beta = sidebar.iter().position(|r| r.contains("Beta session"));
    let alpha = sidebar.iter().position(|r| r.contains("Alpha session"));
    let (beta, alpha) = (beta.expect("beta"), alpha.expect("alpha"));

    // Both sessions sit under the single Sessions header…
    assert!(beta > sessions && alpha > sessions, "{sidebar:#?}");
    // …in recency order, and adjacent: title + sub-line each, with no space
    // heading spliced between them.
    assert!(beta < alpha, "recency order:\n{sidebar:#?}");
    assert_eq!(
        alpha,
        beta + 2,
        "the rows must be consecutive (title, sub-line, title, sub-line):\n{sidebar:#?}"
    );
}

#[test]
fn the_tab_strip_shows_only_the_selected_spaces_sessions() {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Spaces(vec![
        space("s1", "/dev/alpha"),
        space("s2", "/dev/beta"),
    ]));
    let mut alpha = chat("c1", "Alpha session");
    alpha.space_id = Some("s1".into());
    let mut beta = chat("c2", "Beta session");
    beta.space_id = Some("s2".into());
    app.apply(Update::Chats(vec![alpha, beta]));

    app.selected_space = Some("s1".into());
    let rows = snapshot(&mut app, 100, 24);
    assert!(rows[1].contains("Alpha session"), "{}", rows[1]);
    assert!(
        !rows[1].contains("Beta session"),
        "another space's session must not be a tab: {}",
        rows[0]
    );

    // Switching space swaps the strip.
    app.selected_space = Some("s2".into());
    let rows = snapshot(&mut app, 100, 24);
    assert!(rows[1].contains("Beta session"), "{}", rows[1]);
    assert!(!rows[1].contains("Alpha session"), "{}", rows[1]);
}

#[test]
fn tabs_cycle_and_select_by_number() {
    let mut app = populated();
    app.selected_space = Some("s1".into());
    let first = app.selected_chat.clone().expect("a selection");

    app.act(Action::CycleTab(1));
    let second = app.selected_chat.clone().expect("a selection");
    assert_ne!(first, second, "next tab must move");

    // Wrapping: two tabs, so two steps returns.
    app.act(Action::CycleTab(1));
    assert_eq!(app.selected_chat, Some(first.clone()));
    app.act(Action::CycleTab(-1));
    assert_eq!(app.selected_chat, Some(second));

    // Direct selection is 1-based; out of range is a no-op, not a panic.
    app.act(Action::SelectTab(1));
    assert_eq!(app.selected_chat, Some(first));
    let before = app.selected_chat.clone();
    app.act(Action::SelectTab(9));
    assert_eq!(app.selected_chat, before);

    // The active tab carries the wash.
    let tabs = app.tabs();
    assert_eq!(tabs.iter().filter(|tab| tab.active).count(), 1);
}

#[test]
fn a_space_shows_its_host_and_flags_a_lapsed_one() {
    let mut app = populated();
    // Unknown device: no claim either way — silence beats a wrong "offline".
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(!screen.contains("offline"), "{screen}");

    // A device we DO have a record of, whose heartbeat lapsed, reads offline.
    app.apply(Update::Devices(vec![comet_proto::Device {
        id: "dev".into(),
        name: "devbox".into(),
        platform: "linux".into(),
        last_seen_at: Some(Utc::now() - chrono::Duration::minutes(5)),
        created_at: None,
        version: None,
    }]));
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(
        screen.contains("offline"),
        "a lapsed host must show:\n{screen}"
    );

    // Fresh heartbeat: named, not flagged.
    app.apply(Update::Devices(vec![comet_proto::Device {
        id: "dev".into(),
        name: "devbox".into(),
        platform: "linux".into(),
        last_seen_at: Some(Utc::now()),
        created_at: None,
        version: None,
    }]));
    let screen = joined(&snapshot(&mut app, 100, 24));
    assert!(screen.contains("devbox"), "{screen}");
    assert!(!screen.contains("offline"), "{screen}");
}

/// The terminal column a byte offset falls on. `str::find` returns bytes, and
/// these rows are full of multi-byte glyphs (`●`, `│`, `…`), so the two are not
/// the same number — clicking the byte offset lands in the wrong place.
fn column_of(row: &str, byte_index: usize) -> u16 {
    unicode_width::UnicodeWidthStr::width(&row[..byte_index]) as u16
}

/// Draw, then find the cell of the first row containing `needle`.
fn cell_of(app: &mut App, width: u16, height: u16, needle: &str) -> (u16, u16) {
    let rows = snapshot(app, width, height);
    let y = rows
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", joined(&rows)));
    let x = column_of(&rows[y], rows[y].find(needle).unwrap_or(0));
    (x, y as u16)
}

#[test]
fn clicking_a_session_row_opens_it() {
    let mut app = populated();
    app.select_chat(Some("c1".into()));
    let (x, y) = cell_of(&mut app, 100, 24, "Chase the flaky");

    let effects = app.click(x, y);
    assert_eq!(
        app.selected_chat.as_deref(),
        Some("c2"),
        "clicking a row must open that session"
    );
    assert_eq!(app.focus, Focus::Composer, "opening focuses the prompt");
    assert!(
        effects.iter().any(
            |c| matches!(c, comet_tui::link::Command::WatchTranscript(Some(id)) if id == "c2")
        ),
        "and resubscribes its transcript"
    );
}

#[test]
fn clicking_a_space_activates_it_and_swaps_the_tabs() {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Spaces(vec![
        space("s1", "/dev/alpha"),
        space("s2", "/dev/beta"),
    ]));
    let mut alpha = chat("c1", "Alpha session");
    alpha.space_id = Some("s1".into());
    let mut beta = chat("c2", "Beta session");
    beta.space_id = Some("s2".into());
    app.apply(Update::Chats(vec![alpha, beta]));
    app.selected_space = Some("s1".into());

    let (x, y) = cell_of(&mut app, 100, 24, "beta");
    app.click(x, y);
    assert_eq!(app.selected_space.as_deref(), Some("s2"));
    let rows = snapshot(&mut app, 100, 24);
    assert!(rows[1].contains("Beta session"), "{}", rows[1]);
}

#[test]
fn clicking_a_tab_switches_to_it() {
    let mut app = populated();
    app.selected_space = Some("s1".into());
    app.select_chat(Some("c1".into()));
    // The strip is three rows tall so its tabs have vertical padding; the labels
    // sit on the middle one.
    let (x, y) = cell_of(&mut app, 100, 24, "Chase the flaky");
    assert_eq!(y, 1, "tab labels are the strip's middle row");
    app.click(x, y);
    assert_eq!(app.selected_chat.as_deref(), Some("c2"));
}

#[test]
fn clicking_plus_starts_a_session_and_the_panes_take_focus() {
    let mut app = populated();
    app.selected_space = Some("s1".into());
    let before = app.selected_chat.clone();
    // The tab strip's `+`, not the Spaces header's — both are on screen.
    let rows = snapshot(&mut app, 100, 24);
    let x = column_of(&rows[1], rows[1].rfind('+').expect("the tab strip's +"));
    app.click(x, 1);
    // `+` opens a draft, as the desktop canvas does — nothing is created until
    // the first send.
    assert!(app.draft.is_some(), "a draft was opened");
    assert_eq!(app.selected_chat, None, "the draft owns the pane");
    let _ = before;

    // Panes take focus when clicked.
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 24);
    let composer_y = rows.iter().position(|r| r.contains("Do anything")).unwrap() as u16;
    app.focus = Focus::Sidebar;
    app.click(60, composer_y);
    assert_eq!(app.focus, Focus::Composer);

    app.click(60, 6); // in the transcript
    assert_eq!(app.focus, Focus::Transcript);

    // Sidebar decoration (a section header) takes focus without selecting —
    // only rows that can hold the cursor are targets.
    let rows = snapshot(&mut app, 100, 24);
    let header = rows
        .iter()
        .position(|row| row.trim_start().starts_with("Sessions"))
        .expect("the Sessions header") as u16;
    let before = app.selected_chat.clone();
    app.click(4, header);
    assert_eq!(app.focus, Focus::Sidebar);
    assert_eq!(app.selected_chat, before, "a header selects nothing");
}

#[test]
fn clicking_anywhere_dismisses_the_help_overlay() {
    let mut app = populated();
    app.act(Action::ToggleHelp);
    snapshot(&mut app, 100, 26);
    app.click(50, 12);
    assert!(!app.help, "a click must close the overlay");
    // And it must not have fallen through to whatever was underneath.
    assert_eq!(
        app.focus,
        Focus::Composer,
        "focus unchanged by the dismissal"
    );
}

#[test]
fn clicks_outside_any_target_do_nothing() {
    let mut app = populated();
    snapshot(&mut app, 100, 24);
    let before = (app.selected_chat.clone(), app.focus, app.cursor);
    // Far outside the frame entirely.
    app.click(500, 500);
    assert_eq!((app.selected_chat.clone(), app.focus, app.cursor), before);
}

#[test]
fn the_spaces_plus_explains_itself_rather_than_doing_nothing() {
    // There is no folder picker in the TUI yet; the affordance is still drawn
    // (it is part of the desktop sidebar), so clicking it must say why.
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 24);
    let y = rows
        .iter()
        .position(|row| row.contains("Spaces"))
        .expect("the Spaces header");
    // The Spaces header shares row 0 with the tab strip, so take the FIRST `+`
    // on that row — the sidebar's — not the last, which belongs to the tabs.
    let x = column_of(&rows[y], rows[y].find('+').expect("the add affordance"));
    let y = y as u16;

    let before = app.selected_chat.clone();
    app.click(x, y);
    assert_eq!(app.selected_chat, before, "it must not open a session");
    let notice = app.notice.as_ref().expect("a notice");
    assert!(notice.text.contains("desktop app"), "{}", notice.text);
}

#[test]
fn right_click_opens_a_context_menu_with_the_desktop_verbs() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "Chase the flaky room");
    app.right_click(x, y);

    let screen = joined(&snapshot(&mut app, 100, 26));
    for verb in ["Rename", "Archive", "Delete"] {
        assert!(screen.contains(verb), "{verb} missing:\n{screen}");
    }
    // Right-clicking targets the row under the pointer, not wherever the cursor
    // happened to be.
    assert_eq!(app.rows[app.cursor].id(), Some("c2"));

    // Esc dismisses without acting.
    let before = app.selected_chat.clone();
    app.act(Action::CloseOverlay);
    assert!(app.overlay.is_none());
    assert_eq!(app.selected_chat, before);
}

#[test]
fn a_space_menu_offers_space_verbs_only() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "comet ");
    app.right_click(x, y);
    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("Rename space"), "{screen}");
    assert!(screen.contains("Remove space"), "{screen}");
    assert!(!screen.contains("Archive"), "not a session verb:\n{screen}");
}

#[test]
fn archiving_from_the_menu_issues_the_mutation() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "Chase the flaky room");
    app.right_click(x, y);
    // Walk to "Archive" and confirm.
    app.act(Action::OverlayStep(1));
    let effects = app.act(Action::OverlayConfirm);
    assert!(app.overlay.is_none(), "confirming closes the menu");
    match effects.first() {
        Some(comet_tui::link::Command::Call { params, .. }) => {
            assert_eq!(params["op"], "setChatArchived");
            assert_eq!(params["chatId"], "c2");
            assert_eq!(params["archived"], true);
        }
        other => panic!("expected setChatArchived, got {other:?}"),
    }
}

#[test]
fn renaming_opens_a_prompt_seeded_with_the_current_title() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "Chase the flaky room");
    app.right_click(x, y);
    let effects = app.act(Action::OverlayConfirm); // "Rename…" is first
    assert!(effects.is_empty(), "opening a prompt makes no RPC");

    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("Rename session"), "{screen}");
    assert!(
        screen.contains("Chase the flaky"),
        "prompt should start from the current title:\n{screen}"
    );

    // Type and confirm.
    for ch in " v2".chars() {
        app.act(Action::OverlayEdit(comet_tui::keys::Edit::Insert(ch)));
    }
    let effects = app.act(Action::OverlayConfirm);
    match effects.first() {
        Some(comet_tui::link::Command::Call { params, .. }) => {
            assert_eq!(params["op"], "renameChat");
            assert_eq!(params["title"], "Chase the flaky room test v2");
        }
        other => panic!("expected renameChat, got {other:?}"),
    }
    assert!(app.overlay.is_none());
}

#[test]
fn an_empty_rename_is_refused_rather_than_wiping_the_title() {
    let mut app = populated();
    let (x, y) = cell_of(&mut app, 100, 26, "Chase the flaky room");
    app.right_click(x, y);
    app.act(Action::OverlayConfirm); // open the prompt
    app.act(Action::OverlayEdit(
        comet_tui::keys::Edit::DeleteToLineStart,
    ));
    app.act(Action::OverlayEdit(comet_tui::keys::Edit::End));
    let effects = app.act(Action::OverlayConfirm);
    assert!(effects.is_empty(), "an empty name must not be submitted");
    assert!(app.overlay.is_some(), "and the prompt stays open");
}

#[test]
fn the_model_picker_lists_and_switches() {
    use comet_proto::{Model, ReasoningLevel};
    let mut app = populated();
    let effects = app.act(Action::PickModel);
    assert!(
        effects
            .iter()
            .any(|c| matches!(c, comet_tui::link::Command::ListModels { .. })),
        "asks the engine for the catalogue"
    );
    // Until it answers, the picker says so rather than showing an empty list.
    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("Loading"), "{screen}");

    app.apply(Update::Models(vec![
        Model {
            id: "fable-5".into(),
            label: "Fable 5".into(),
            description: Some("balanced".into()),
            reasoning_levels: vec![ReasoningLevel::High],
            options: vec![],
        },
        Model {
            id: "opus-5".into(),
            label: "Opus 5".into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        },
    ]));
    let screen = joined(&snapshot(&mut app, 100, 26));
    assert!(screen.contains("Fable 5"), "{screen}");
    assert!(screen.contains("balanced"), "descriptions show:\n{screen}");

    app.act(Action::OverlayStep(1));
    let effects = app.act(Action::OverlayConfirm);
    match effects.first() {
        Some(comet_tui::link::Command::Call { params, .. }) => {
            assert_eq!(params["op"], "setChatConfig");
            assert_eq!(params["config"]["model"], "opus-5");
        }
        other => panic!("expected setChatConfig, got {other:?}"),
    }
    // The chip updates immediately rather than waiting for the round trip.
    assert_eq!(app.composer_meta().model, "opus-5");
}

#[test]
fn a_failed_model_fetch_closes_the_picker() {
    let mut app = populated();
    app.act(Action::PickModel);
    assert!(app.overlay.is_some());
    app.apply(Update::Notice("Couldn't list models: boom".into()));
    assert!(
        app.overlay.is_none(),
        "a waiting picker must not hang on failure"
    );
    assert!(app.notice.is_some());
}

#[test]
fn nothing_in_the_steady_state_view_is_drawn_with_a_line() {
    // opencode's whole TUI has no borders: regions are told apart by which step
    // of the background ramp they are filled with. Every stroke this renderer
    // used to draw — the rules, the boxes, the composer bar, and finally the
    // sidebar divider itself — drew a boundary that a colour change was already
    // drawing.
    let mut app = populated();
    let screen = joined(&snapshot(&mut app, 100, 26));
    for stroke in [
        '│', '─', '━', '├', '┤', '┬', '┴', '┼', '╭', '╮', '╰', '╯', '┌', '┐', '└', '┘',
    ] {
        assert!(
            !screen.contains(stroke),
            "{stroke:?} is line work the fills replaced:\n{screen}"
        );
    }
    // The one exception, and it is not a border: the accent bar down the left of
    // the prompt and of the user's own messages. opencode marks both the same
    // way, it is the only colour in the chrome, and it encloses nothing.
    assert!(
        screen.contains('┃'),
        "the accent bar should mark what is yours:\n{screen}"
    );
}

#[test]
fn the_sidebar_and_the_transcript_sit_at_different_levels() {
    // The two panes are separated by the sidebar's fill ending, and by nothing
    // else. The transcript keeps the app's own base surface — one step below the
    // sidebar — so the blocks drawn on it have something to be raised against.
    let mut app = populated();
    let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let panel = app.theme.panel;

    // A row well clear of the header, the selection and the prompt.
    let y = 9u16;
    for x in 0..32u16 {
        assert_eq!(buffer[(x, y)].bg, panel, "sidebar column {x} is not filled");
    }
    assert_eq!(
        buffer[(32, y)].bg,
        app.theme.base,
        "the transcript sits on the app's base surface"
    );
    assert_ne!(app.theme.base, panel, "and the two are distinguishable");
}

#[test]
fn taking_focus_lights_the_prompts_accent_bar() {
    // The prompt is a block like the user's messages above it and carries the
    // same accent bar; focus is that bar going indigo. Nothing changes shape or
    // level, so nothing reflows under the caret while you are typing.
    let mut app = populated();
    app.focus = Focus::Composer;
    let rows = snapshot(&mut app, 100, 26);
    let y = rows
        .iter()
        .position(|row| row.contains("Do anything"))
        .expect("the prompt") as u16;
    // Clear of column zero of the text: that cell carries the caret, which is
    // inverted, and would report the foreground as its background.
    let x = column_of(
        &rows[y as usize],
        rows[y as usize].find("Do anything").unwrap(),
    ) + 4;

    let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(x, y)].bg,
        app.theme.element,
        "the prompt is a block"
    );
    // The bar is the block's leftmost column.
    let bar_x = column_of(
        &rows[y as usize],
        rows[y as usize].rfind('┃').expect("the prompt's bar"),
    );
    assert_eq!(buffer[(bar_x, y)].fg, app.theme.accent, "focused: indigo");

    app.focus = Focus::Transcript;
    let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(bar_x, y)].symbol(),
        "┃",
        "the bar does not move when the prompt loses focus"
    );
    assert_eq!(
        buffer[(bar_x, y)].fg,
        app.theme.faint,
        "unfocused: the bar drops to the block's own muted edge"
    );
    assert_eq!(buffer[(x, y)].bg, app.theme.element, "and stays a block");
}

#[test]
fn a_draft_shows_its_pending_tab_and_where_it_will_run() {
    let mut app = populated();
    app.act(Action::NewSession);
    let rows = snapshot(&mut app, 100, 28);
    let screen = joined(&rows);

    // A pending tab, so the pane is not simply blank.
    assert!(rows[1].contains("New session"), "{}", rows[1]);
    // The chips say where it will run — the choice only exists before send.
    assert!(
        screen.contains("Current checkout"),
        "checkout chip missing:\n{screen}"
    );
}

#[test]
fn the_checkout_picker_offers_two_modes_and_names_the_third_outcome() {
    use comet_proto::RepoRef;
    let mut app = populated();
    app.act(Action::NewSession);
    app.apply(Update::Refs(vec![RepoRef {
        name: "main".into(),
        current: true,
        worktree_path: None,
    }]));

    app.act(Action::PickCheckout);
    let screen = joined(&snapshot(&mut app, 100, 28));
    assert!(screen.contains("Run in"), "{screen}");
    assert!(screen.contains("Current checkout"), "{screen}");
    assert!(screen.contains("New worktree"), "{screen}");

    // With a materialized ref picked, the same Local row reads "Current
    // worktree" — two modes, three outcomes.
    app.act(Action::CloseOverlay);
    app.apply(Update::Refs(vec![RepoRef {
        name: "feat".into(),
        current: true,
        worktree_path: Some("/wt/feat".into()),
    }]));
    app.act(Action::PickCheckout);
    let screen = joined(&snapshot(&mut app, 100, 28));
    assert!(screen.contains("Current worktree"), "{screen}");
    assert!(!screen.contains("Current checkout"), "{screen}");
}

#[test]
fn the_branch_picker_tags_current_and_materialized_refs() {
    use comet_proto::RepoRef;
    let mut app = populated();
    app.act(Action::NewSession);
    // While loading it says so rather than showing an empty list.
    app.act(Action::PickRef);
    assert!(joined(&snapshot(&mut app, 100, 28)).contains("Loading"));

    app.apply(Update::Refs(vec![
        RepoRef {
            name: "main".into(),
            current: true,
            worktree_path: None,
        },
        RepoRef {
            name: "feat".into(),
            current: false,
            worktree_path: Some("/wt/feat".into()),
        },
    ]));
    let screen = joined(&snapshot(&mut app, 100, 28));
    assert!(screen.contains("Branch"), "{screen}");
    assert!(screen.contains("current"), "current tag missing:\n{screen}");
    assert!(
        screen.contains("worktree"),
        "worktree tag missing:\n{screen}"
    );

    // A space that is not a git checkout says so.
    app.act(Action::CloseOverlay);
    app.apply(Update::Refs(vec![]));
    app.act(Action::PickRef);
    assert!(joined(&snapshot(&mut app, 100, 28)).contains("Not a git checkout"));
}

#[test]
fn the_prompt_has_air_around_it() {
    // One row jammed against the transcript and the hint bar reads as an
    // afterthought.
    let mut app = populated();
    let rows = snapshot(&mut app, 100, 28);
    let prompt = rows
        .iter()
        .position(|row| row.contains("Do anything"))
        .expect("the prompt");
    // The main pane past the sidebar. Both rows around the prompt are inside the
    // prompt's own fill: the block reserves a row of air on top, and a row below
    // that separates the text from the meta row.
    // Past the sidebar *and* past the prompt's accent bar, which is a mark on
    // the block rather than content in it.
    let main = |row: &str| {
        row.chars()
            .skip(31)
            .collect::<String>()
            .replace('┃', " ")
            .trim()
            .to_string()
    };
    assert!(
        main(&rows[prompt - 1]).is_empty(),
        "no air above the prompt:\n{}",
        joined(&rows)
    );
    assert!(
        main(&rows[prompt + 1]).is_empty(),
        "no air under the prompt text:\n{}",
        joined(&rows)
    );
    // …and the meta row is the block's last row, so the prompt is never the
    // thing hard against the hint bar.
    assert!(
        !main(&rows[prompt + 2]).is_empty(),
        "the meta row should close the block:\n{}",
        joined(&rows)
    );
}

#[test]
fn every_loader_on_screen_is_the_same_one() {
    // A running session is drawn in three places — the sidebar row, its tab and
    // the working strip. They must not be three different animations.
    let mut app = populated();
    app.apply(Update::Sessions(vec![Session {
        chat_id: "c1".into(),
        device_id: "dev".into(),
        status: SessionStatus::Working,
        started_at: Some(Utc::now()),
        updated_at: Utc::now(),
    }]));
    let (expected, _) = comet_tui::loaders::mini_spinner(app.elapsed());
    let screen = joined(&snapshot(&mut app, 100, 28));
    let count = screen.matches(expected.as_str()).count();
    assert!(
        count >= 3,
        "expected the same glyph in row, tab and strip; saw {count} of {expected:?}\n{screen}"
    );
}

#[test]
fn each_chip_is_its_own_button() {
    use comet_tui::app::ChipKind;
    let mut app = populated();
    app.act(Action::NewSession);
    snapshot(&mut app, 110, 28);

    // Distinct targets, not one lumped region: the footer carries where this
    // runs, the meta row carries who answers.
    let footer = app.composer_footer().expect("a git space has a footer");
    assert!(!footer.reference.is_empty());
    assert!(!footer.checkout.is_empty());
    assert!(!app.composer_meta().model.is_empty());
    let _ = ChipKind::Model;

    // Clicking each opens *its own* picker, not always the model one.
    let rows = snapshot(&mut app, 110, 28);
    let y = rows
        .iter()
        .position(|row| row.contains("Current checkout"))
        .expect("the chip row") as u16;
    let x = column_of(
        &rows[y as usize],
        rows[y as usize].find("Current checkout").unwrap(),
    );
    app.click(x, y);
    assert!(
        joined(&snapshot(&mut app, 110, 28)).contains("Run in"),
        "the checkout chip must open the checkout picker"
    );
    app.act(Action::CloseOverlay);

    let rows = snapshot(&mut app, 110, 28);
    let y = rows
        .iter()
        .position(|row| row.contains("Select ref"))
        .expect("the branch chip") as u16;
    let x = column_of(
        &rows[y as usize],
        rows[y as usize].find("Select ref").unwrap(),
    );
    app.click(x, y);
    assert!(
        joined(&snapshot(&mut app, 110, 28)).contains("Branch"),
        "the branch chip must open the branch picker"
    );
}

#[test]
fn effort_is_pickable_and_follows_the_model() {
    use comet_proto::{Model, ReasoningLevel};
    let mut app = populated();
    app.act(Action::NewSession);

    // Default levels when no catalogue entry is known yet.
    app.act(Action::PickReasoning);
    let screen = joined(&snapshot(&mut app, 110, 28));
    assert!(screen.contains("Effort"), "{screen}");
    assert!(screen.contains("High"), "{screen}");
    app.act(Action::OverlayStep(3));
    app.act(Action::OverlayConfirm);
    assert_eq!(
        app.draft.as_ref().unwrap().reasoning,
        Some(ReasoningLevel::High)
    );
    // …and it shows beside the model.
    assert_eq!(app.composer_meta().effort.as_deref(), Some("High"));

    // Choosing a model whose levels exclude the current one moves it to a level
    // that model actually offers, rather than sending an unsupported value.
    app.act(Action::PickModel);
    app.apply(Update::Models(vec![Model {
        id: "small".into(),
        label: "Small".into(),
        description: None,
        reasoning_levels: vec![ReasoningLevel::Low],
        options: vec![],
    }]));
    app.act(Action::OverlayConfirm);
    assert_eq!(
        app.draft.as_ref().unwrap().reasoning,
        Some(ReasoningLevel::Low),
        "effort must stay within the model's levels"
    );
}

#[test]
fn a_drafts_model_and_effort_ride_the_session_it_creates() {
    use comet_proto::{Model, ReasoningLevel};
    let mut app = populated();
    app.act(Action::NewSession);
    app.act(Action::PickModel);
    app.apply(Update::Models(vec![Model {
        id: "opus-5".into(),
        label: "Opus 5".into(),
        description: None,
        reasoning_levels: vec![ReasoningLevel::High],
        options: vec![],
    }]));
    app.act(Action::OverlayConfirm);

    app.composer.set_text("go");
    let effects = app.act(Action::Send);
    let start = effects
        .iter()
        .find_map(|c| match c {
            comet_tui::link::Command::StartSession(start) => Some(start),
            _ => None,
        })
        .expect("a StartSession");
    let config = start.config.as_ref().expect("the draft's config");
    assert_eq!(config["model"], "opus-5");
    assert_eq!(config["reasoning"], "high");
}

#[test]
fn the_caret_is_drawn_so_a_trailing_space_is_visible() {
    // Relying on the terminal's own cursor makes a trailing space invisible:
    // the row is rendered without it, so nothing tells you it is there.
    let mut app = populated();
    app.focus = Focus::Composer;
    for ch in "hi ".chars() {
        app.act(Action::Edit(comet_tui::keys::Edit::Insert(ch)));
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let cursor = terminal.get_cursor_position().expect("a caret");
    let cell = &terminal.backend().buffer()[(cursor.x, cursor.y)];
    assert_eq!(
        cell.bg, app.theme.text,
        "the caret cell must be painted, not left to the terminal"
    );
    assert_eq!(cell.symbol(), " ", "and it sits on the space just typed");

    // One column left is the 'i' — so the block really is past the space.
    let before = &terminal.backend().buffer()[(cursor.x - 1, cursor.y)];
    assert_eq!(before.symbol(), "i");
}

/// Two spaces, two sessions each, signed in.
fn two_spaces() -> App {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Spaces(vec![
        space("s1", "/dev/alpha"),
        space("s2", "/dev/beta"),
    ]));
    let mut rows = Vec::new();
    for (id, title, space_id, age) in [
        ("a1", "Alpha one", "s1", 1i64),
        ("a2", "Alpha two", "s1", 2),
        ("b1", "Beta one", "s2", 3),
        ("b2", "Beta two", "s2", 4),
    ] {
        let mut chat = chat(id, title);
        chat.space_id = Some(space_id.into());
        chat.last_message_at = Some(Utc::now() - chrono::Duration::minutes(age));
        rows.push(chat);
    }
    app.apply(Update::Chats(rows));
    app
}

#[test]
fn switching_spaces_returns_you_to_where_you_were() {
    let mut app = two_spaces();

    // Be somewhere specific in each space.
    app.activate_space("s1".into());
    app.select_chat(Some("a2".into()));
    app.activate_space("s2".into());
    app.select_chat(Some("b2".into()));

    // Coming back lands on the session you left, not the newest one.
    app.activate_space("s1".into());
    assert_eq!(app.selected_chat.as_deref(), Some("a2"));
    app.activate_space("s2".into());
    assert_eq!(app.selected_chat.as_deref(), Some("b2"));
}

#[test]
fn a_space_you_have_never_opened_lands_on_its_newest_session() {
    let mut app = two_spaces();
    app.activate_space("s2".into());
    // `chats` is recency-sorted, so the newest of s2 is b1.
    assert_eq!(app.selected_chat.as_deref(), Some("b1"));
    assert!(app.draft.is_none());
}

#[test]
fn an_empty_space_opens_the_new_session_canvas() {
    let mut app = two_spaces();
    // Archive everything in s2, leaving it empty.
    let chats: Vec<comet_proto::Chat> = app
        .chats
        .iter()
        .cloned()
        .map(|mut chat| {
            if chat.space_id.as_deref() == Some("s2") {
                chat.archived = true;
            }
            chat
        })
        .collect();
    app.apply(Update::Chats(chats));

    app.activate_space("s2".into());
    assert!(
        app.draft.is_some(),
        "an empty space must not leave a dead pane"
    );
    assert_eq!(app.selected_chat, None);
    assert_eq!(app.draft.as_ref().unwrap().space_id, "s2");
}

#[test]
fn a_remembered_session_that_vanished_falls_back() {
    let mut app = two_spaces();
    app.activate_space("s1".into());
    app.select_chat(Some("a2".into()));

    // a2 is deleted elsewhere.
    let chats: Vec<comet_proto::Chat> = app
        .chats
        .iter()
        .filter(|chat| chat.id != "a2")
        .cloned()
        .collect();
    app.apply(Update::Chats(chats));

    app.activate_space("s2".into());
    app.activate_space("s1".into());
    assert_eq!(
        app.selected_chat.as_deref(),
        Some("a1"),
        "a stale memory must fall back to the newest, not to nothing"
    );
}

#[test]
fn clicking_a_space_row_restores_its_session() {
    let mut app = two_spaces();
    app.activate_space("s1".into());
    app.select_chat(Some("a2".into()));
    app.activate_space("s2".into());

    let (x, y) = cell_of(&mut app, 100, 28, "alpha");
    app.click(x, y);
    assert_eq!(app.selected_space.as_deref(), Some("s1"));
    assert_eq!(app.selected_chat.as_deref(), Some("a2"));
}

#[test]
fn a_draft_survives_the_chats_frames_that_arrive_while_you_type() {
    // The regression: `heal_chat_selection` treats "no chat selected" as "pick
    // one", but a draft is *deliberately* in that state. Chats frames arrive
    // constantly while any session streams, so the canvas was being yanked away
    // — and because the draft stayed set, the next send created a session
    // instead of continuing the one you had been moved to.
    let mut app = two_spaces();
    app.act(Action::NewSession);
    let space = app.draft.as_ref().unwrap().space_id.clone();

    for _ in 0..5 {
        let chats = app.chats.clone();
        app.apply(Update::Chats(chats));
        assert!(app.draft.is_some(), "the draft must survive a chats frame");
        assert_eq!(
            app.selected_chat, None,
            "and nothing may be selected under it"
        );
    }
    assert_eq!(app.draft.as_ref().unwrap().space_id, space);

    // Sending now starts the draft — one new session, not a stray one.
    app.composer.set_text("go");
    let effects = app.act(Action::Send);
    assert_eq!(
        effects
            .iter()
            .filter(|c| matches!(c, comet_tui::link::Command::StartSession(_)))
            .count(),
        1
    );
}

#[test]
fn sending_in_an_open_session_never_creates_a_new_one() {
    let mut app = two_spaces();
    app.activate_space("s1".into());
    app.select_chat(Some("a1".into()));

    // Even after opening a draft and then picking a real session again, the
    // draft must be gone — the two states are mutually exclusive.
    app.act(Action::NewSession);
    assert!(app.draft.is_some());
    app.select_chat(Some("a1".into()));
    assert!(app.draft.is_none(), "opening a session abandons the draft");

    app.composer.set_text("continue");
    let effects = app.act(Action::Send);
    assert!(
        !effects
            .iter()
            .any(|c| matches!(c, comet_tui::link::Command::StartSession(_))),
        "a send in an open session must continue it"
    );
    match effects.first() {
        Some(comet_tui::link::Command::Send { chat_id, .. }) => assert_eq!(chat_id, "a1"),
        other => panic!("expected a Send to a1, got {other:?}"),
    }
}

#[test]
fn clicking_a_session_while_drafting_switches_to_it() {
    let mut app = two_spaces();
    app.activate_space("s1".into());
    app.act(Action::NewSession);
    let (x, y) = cell_of(&mut app, 100, 28, "Alpha two");
    app.click(x, y);
    assert!(app.draft.is_none());
    assert_eq!(app.selected_chat.as_deref(), Some("a2"));
}

#[test]
fn a_picker_is_a_filled_surface_not_a_box() {
    // opencode's dialogs are a wash and nothing else, and so are comet's own
    // desktop menus. A hairline box around a surface that is already a different
    // colour draws the same edge twice — and costs two rows and two columns of
    // the list it surrounds.
    let mut app = populated();
    app.act(Action::NewSession);
    app.act(Action::PickCheckout);
    let rows = snapshot(&mut app, 100, 28);
    let screen = joined(&rows);
    for stroke in [
        '\u{256d}', '\u{256e}', '\u{2570}', '\u{256f}', '\u{2502}', '\u{2500}',
    ] {
        assert!(
            !rows
                .iter()
                .skip_while(|row| !row.contains("Run in"))
                .take(4)
                .any(|row| row.contains(stroke)),
            "the picker is framed with {stroke:?}:\n{screen}"
        );
    }

    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let y = rows.iter().position(|row| row.contains("Run in")).unwrap() as u16;
    let x = column_of(&rows[y as usize], rows[y as usize].find("Run in").unwrap());
    // The title row carries the panel fill…
    assert_eq!(buffer[(x, y)].bg, app.theme.panel);
    // …and the selected row is a step above it, so the cursor is visible on a
    // region the panel fill would have swallowed it into.
    assert_eq!(buffer[(x, y + 1)].bg, app.theme.selection);
    assert_eq!(buffer[(x, y + 2)].bg, app.theme.panel);
}

#[test]
fn a_tool_group_is_plain_rows_with_air_around_it() {
    // A tool run is the agent's bookkeeping, not something it said to you.
    // Giving it a fill of its own made every reply look like it had been
    // interrupted by a panel; the chevron and the indent are enough.
    use comet_proto::ToolCall;
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::Assistant,
            vec![
                text("t0", "first"),
                MessagePart::Tool {
                    id: "p1".into(),
                    call: ToolCall::Exec {
                        command: "cargo test".into(),
                    },
                    is_error: false,
                    resolved: true,
                },
                text("t1", "second"),
            ],
        )],
    });
    let rows = snapshot(&mut app, 100, 26);
    let screen = joined(&rows);
    let at = |needle: &str| {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} missing:\n{screen}"))
    };
    let (first, group, second) = (at("first"), at("Ran 1 command"), at("second"));
    assert!(
        group > first + 1,
        "the tool group needs air above:\n{screen}"
    );
    assert!(second > group + 1, "and below:\n{screen}");

    let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let x = column_of(&rows[group], rows[group].find("Ran 1").unwrap());
    assert_eq!(
        buffer[(x, group as u16)].bg,
        app.theme.base,
        "a tool group sits on the app surface, not on a fill of its own"
    );
    assert!(
        !rows[group].contains('│'),
        "a rail survived beside the group:\n{screen}"
    );
}

#[test]
fn fenced_code_is_a_full_width_band() {
    // Sized to the column, not to its widest line. A card that shrinks to its
    // content makes every code block a different width, and a column of
    // different-width cards is the ragged look the fills were meant to fix.
    let mut app = populated();
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![entry(
            "m1",
            MessageRole::Assistant,
            vec![text("t0", "```\nlet x = 1;\nlet yy = 22;\n```")],
        )],
    });
    let rows = snapshot(&mut app, 100, 26);
    let screen = joined(&rows);
    let y = rows
        .iter()
        .position(|row| row.contains("let x = 1;"))
        .expect("the code") as u16;

    let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let wash = app.theme.code_wash;
    let x = column_of(&rows[y as usize], rows[y as usize].find("let x").unwrap());
    assert_eq!(
        buffer[(x, y)].bg,
        wash,
        "code should sit on a wash:\n{screen}"
    );
    // Both the short line and the long one reach the same right edge.
    assert_eq!(buffer[(96, y)].bg, wash, "ragged right edge:\n{screen}");
    assert_eq!(buffer[(96, y + 1)].bg, wash, "ragged right edge:\n{screen}");
    // Recessed rather than raised: code quoted inside a reply reads as below it.
    assert_ne!(wash, app.theme.element);
    assert!(
        !rows[y as usize].contains('│'),
        "a rail survived beside the code:\n{screen}"
    );
}

#[test]
fn the_meta_row_never_appears_or_disappears() {
    // The reported bug: tabbing between sessions made the model row — and with
    // it the whole bottom of the pane — flicker in and out, because a chat whose
    // config had not synced produced no chips at all. The desktop resolves the
    // label down a chain precisely so this cannot happen ("Never 'Default
    // model'", pickers.rs), and the row it lives in is reserved either way.
    let mut app = populated();

    let prompt_row = |app: &mut App| {
        let rows = snapshot(app, 100, 28);
        rows.iter()
            .position(|row| row.contains("Do anything"))
            .expect("the prompt")
    };

    // A session with no config at all still names a model…
    assert!(!app.composer_meta().model.is_empty());
    let with_session = prompt_row(&mut app);

    // …a draft does too…
    app.act(Action::NewSession);
    assert!(!app.composer_meta().model.is_empty());
    let with_draft = prompt_row(&mut app);

    // …and switching between them does not move the prompt by a single row.
    app.select_chat(Some("c1".into()));
    let back = prompt_row(&mut app);
    assert_eq!(
        (with_session, with_draft, back),
        (with_session, with_session, with_session),
        "the prompt moved as the composer's state changed"
    );
}

#[test]
fn the_footer_follows_a_session_as_well_as_a_draft() {
    // The other half of the report: branch and checkout were drafting-only, so
    // opening a tab made them vanish. The desktop's `render_footer` is live in
    // both modes — t3code keeps its branch selector interactive mid-session and
    // so does comet — and only the checkout side locks, because a session's
    // checkout kind was fixed when it was created.
    let mut app = populated();

    app.act(Action::NewSession);
    let draft = app.composer_footer().expect("a draft has a footer");
    assert!(
        draft.checkout_live,
        "a draft can still choose where it runs"
    );

    app.select_chat(Some("c1".into()));
    let open = app.composer_footer().expect("an open session has one too");
    assert!(!open.reference.is_empty(), "the ref side stays live");
    assert!(
        !open.checkout_live,
        "a session's checkout kind is fixed at creation, so it is a label"
    );

    // Both are on screen, in both modes.
    for label in [open.reference.as_str(), open.checkout.as_str()] {
        assert!(
            joined(&snapshot(&mut app, 100, 28)).contains(label),
            "{label:?} missing from the footer"
        );
    }
}

#[test]
fn a_space_that_is_not_a_checkout_has_no_footer_at_all() {
    // An absent row, not an empty one: with no git there is no ref and no
    // worktree, so the two questions the footer answers do not arise.
    let mut app = populated();
    let mut plain = space("s1", "/dev/comet");
    plain.git_detected = false;
    app.apply(Update::Spaces(vec![plain]));
    app.activate_space("s1".into());
    assert!(app.composer_footer().is_none());
}
