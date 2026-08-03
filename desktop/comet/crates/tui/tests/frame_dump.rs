//! A visual-iteration harness: render a frame and emit every cell as JSON so a
//! rasteriser can turn it into a PNG.
//!
//! Reading a TestBackend text dump tells you where the glyphs are and nothing
//! about how the frame *looks* — which is exactly the thing being designed. This
//! is `#[ignore]`d; run it with `--ignored --nocapture` and pipe to a renderer.

use chrono::Utc;
use comet_doc::{MessagePart, MessageRole, SessionMessageEntry};
use comet_proto::view::ConnectionStatus;
use comet_proto::{AuthState, Chat, Space, UserProfile};
use comet_tui::app::App;
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

fn chat(id: &str, title: &str, space_id: &str, hours: i64) -> Chat {
    Chat {
        id: id.into(),
        device_id: "dev".into(),
        title: Some(title.into()),
        archived: false,
        cwd: Some(format!("/home/w/{space_id}")),
        branch: Some("main".into()),
        checkout_id: None,
        config: None,
        last_message_preview: None,
        last_message_at: Some(Utc::now() - chrono::Duration::hours(hours)),
        created_at: Utc::now(),
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: Some(space_id.into()),
        last_seen_at: Some(Utc::now()),
    }
}

fn text(id: &str, body: &str) -> MessagePart {
    MessagePart::Text {
        id: id.into(),
        text: body.into(),
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

fn scene() -> App {
    let mut app = App::with_theme(Theme::dark());
    app.apply(Update::Connection(ConnectionStatus::Ready));
    app.apply(Update::Auth(Box::new(AuthState::SignedIn {
        user: UserProfile {
            id: "u1".into(),
            email: "imisterlee@gmail.com".into(),
            name: Some("Wing Lee".into()),
        },
        org_id: Some("org".into()),
    })));
    app.apply(Update::Spaces(vec![
        space("s1", "/home/w/comet-native"),
        space("s2", "/home/w/website"),
    ]));
    app.apply(Update::Chats(vec![
        chat("c1", "Legacy App Decompilation", "s1", 24),
        chat("c2", "App Terminal TUI Monochrome", "s1", 5),
        chat("c3", "Rebuild CLI and Test Harness", "s1", 0),
        chat("c4", "Session Loading Virtualisation", "s1", 4),
        chat("c5", "Invert PNG Image Colors", "s2", 0),
    ]));
    app.activate_space("s1".into());
    app.select_chat(Some("c1".into()));
    app.apply(Update::Transcript {
        chat_id: "c1".into(),
        entries: vec![
            entry(
                "m0",
                MessageRole::User,
                vec![text("u0", "give me the final tally for the decompilation run")],
            ),
            entry(
                "m1",
                MessageRole::Assistant,
                vec![
                    text(
                        "t0",
                        "**Final tally:**\n\n\
                         63,873 functions, 62 failures, ~214 MB of C. The pleasant surprise is `cua-driver`: \
                         Ghidra demangled 28,943 of 28,992 Rust v0 symbols, so the largest binary is fully \
                         navigable by real function name — no attribution heuristics needed.\n\n\
                         **What you actually get:**\n\n\
                         - `08-reconstructed/` — 479 Swift declarations, every stored-property type resolved\n\
                         - `08-reconstructed/SharedKit/PROTOCOL.md` — the Mac↔iOS wire protocol, 75 types\n\
                         - `04-rust/README.md` — recovered Cargo workspace layout for all four Rust binaries\n\n",
                    ),
                    MessagePart::Tool {
                        id: "p1".into(),
                        call: comet_proto::ToolCall::Exec {
                            command: "cargo test --workspace".into(),
                        },
                        is_error: false,
                        resolved: true,
                    },
                    MessagePart::Tool {
                        id: "p2".into(),
                        call: comet_proto::ToolCall::Exec {
                            command: "git log -5 --oneline".into(),
                        },
                        is_error: false,
                        resolved: true,
                    },
                    text(
                        "t1",
                        "Three things I corrected mid-run rather than leaving wrong: the crate counts were \
                         inflated by a loose version-string match, and the README's reproduction commands \
                         referenced a symbol file the pipeline no longer uses.\n\n\
                         ```rust\nfolded = fold_event_into_parts(&folded, &event);\nwriter.sync(&folded)?; // 120ms\n```\n\n\
                         All fixed and verified.",
                    ),
                ],
            ),
        ],
    });
    app
}

#[test]
#[ignore]
fn dump() {
    let (w, h) = match std::env::var("FRAME") {
        Ok(v) => {
            let mut it = v.split('x');
            (
                it.next().unwrap().parse().unwrap(),
                it.next().unwrap().parse().unwrap(),
            )
        }
        Err(_) => (120u16, 34u16),
    };
    let mut app = scene();
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();

    let colour = |c: ratatui::style::Color| -> String {
        match c {
            ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            ratatui::style::Color::Reset => "reset".into(),
            other => format!("{other:?}"),
        }
    };
    println!("{{\"w\":{w},\"h\":{h},\"cells\":[");
    let mut first = true;
    for y in 0..h {
        for x in 0..w {
            let cell = &buffer[(x, y)];
            if !first {
                println!(",");
            }
            first = false;
            let sym = cell.symbol().replace('\\', "\\\\").replace('"', "\\\"");
            print!(
                "{{\"x\":{x},\"y\":{y},\"s\":\"{sym}\",\"f\":\"{}\",\"b\":\"{}\",\"m\":{}}}",
                colour(cell.fg),
                colour(cell.bg),
                cell.modifier.bits()
            );
        }
    }
    println!("\n]}}");
}
