//! Failure-episode dedupe for the "MCP servers that failed to connect"
//! section of the MCP system-reminder (semantics: `McpAnnounced::failed`).

use super::support::*;
use super::*;

fn http_server(name: &str) -> acp::McpServer {
    acp::McpServer::Http(
        acp::McpServerHttp::new(name.to_string(), format!("https://example.test/{name}"))
            .headers(vec![]),
    )
}

/// Simulate what a background snapshot refresh does: mark dirty, then run
/// the injector (turn-start / agentic-loop path).
async fn refresh_and_inject(actor: &SessionActor) {
    actor
        .mcp_reminder_dirty
        .store(true, std::sync::atomic::Ordering::Relaxed);
    actor.maybe_inject_mcp_reminder().await;
}

async fn failed_reminders(actor: &SessionActor) -> Vec<String> {
    actor
        .chat_state_handle
        .get_conversation()
        .await
        .iter()
        .map(|item| item.text_content())
        .filter(|text| text.contains("MCP servers that failed to connect"))
        .collect()
}

/// Connect `name` from the injector's point of view: present in the tool
/// metadata snapshot (→ `connected_server_summaries`) with its failure
/// state cleared, mirroring the real registration path.
async fn connect_server(actor: &SessionActor, name: &str) {
    {
        let mut snapshot = actor.tool_metadata_snapshot.lock().unwrap();
        snapshot.tools = vec![crate::session::tool_index::ToolMetadata {
            qualified_name: format!("{name}__echo"),
            server_name: name.to_string(),
            tool_name: "echo".to_string(),
            description: "echo".to_string(),
            parameters: vec![],
            input_schema: serde_json::json!({"type": "object"}),
        }];
        snapshot.servers = vec![crate::session::tool_index::ServerMetadata {
            name: name.to_string(),
            description: None,
        }];
        snapshot.mcp_initialized = true;
    }
    let mut state = actor.mcp_state.lock().await;
    state.auth_required.remove(name);
    state.clear_init_failed(name);
}

async fn disconnect_server(actor: &SessionActor) {
    let mut snapshot = actor.tool_metadata_snapshot.lock().unwrap();
    snapshot.tools.clear();
    snapshot.servers.clear();
}

#[tokio::test(flavor = "current_thread")]
async fn failed_server_announced_once_per_episode() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("dead"));
                state.record_init_failure("dead", false, Some("boom\n- forged".to_string()));
            }

            refresh_and_inject(&actor).await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 1, "{reminders:?}");
            // Control characters in the failure detail must not forge extra
            // reminder lines, and the HTTP retry hint is rendered.
            assert!(
                reminders[0]
                    .contains("dead (\"boom - forged\" — retries automatically on next tool call)"),
                "{}",
                reminders[0]
            );
            assert!(!reminders[0].contains("\n- forged"), "{}", reminders[0]);

            // Background retries re-mark the reminder dirty without any
            // state change: the same episode must not re-announce.
            refresh_and_inject(&actor).await;
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            // A non-auth reason change within the episode is not
            // re-announced.
            actor.mcp_state.lock().await.record_init_failure(
                "dead",
                false,
                Some("timed out".to_string()),
            );
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            // The episode is persisted so a resumed session doesn't
            // re-announce it either.
            let mut persisted_failed = None;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::AnnouncementState(state) = msg {
                    persisted_failed = Some(state.announced_failed_servers);
                }
            }
            let persisted_failed = persisted_failed.expect("announcement state persisted");
            assert!(
                persisted_failed.contains_key("dead"),
                "{persisted_failed:?}"
            );
        })
        .await;
}

/// Escalation to auth-required is the one reason change that re-announces
/// (once): it needs user action, and it invalidates a previously announced
/// "retries automatically" hint. Later flips back and forth stay silent.
#[tokio::test(flavor = "current_thread")]
async fn auth_escalation_reannounces_once() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("dead"));
                state.record_init_failure("dead", false, Some("boom".to_string()));
            }

            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            // Retry reclassifies the failure as auth-required: announce once.
            actor
                .mcp_state
                .lock()
                .await
                .record_init_failure("dead", true, None);
            refresh_and_inject(&actor).await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 2, "{reminders:?}");
            assert!(
                reminders[1].contains("dead (auth required"),
                "{}",
                reminders[1]
            );

            // Further auth <-> non-auth flips within the episode stay silent.
            {
                let mut state = actor.mcp_state.lock().await;
                state.auth_required.remove("dead");
                state.record_init_failure("dead", false, Some("boom".to_string()));
            }
            refresh_and_inject(&actor).await;
            actor
                .mcp_state
                .lock()
                .await
                .record_init_failure("dead", true, None);
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 2);

            // A server announced as auth-required from the start never
            // re-announces on flips either.
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("oauth"));
                state.record_init_failure("oauth", true, None);
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 3);
            {
                let mut state = actor.mcp_state.lock().await;
                state.auth_required.remove("oauth");
                state.record_init_failure("oauth", false, Some("boom".to_string()));
                state.record_init_failure("oauth", true, None);
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 3);
        })
        .await;
}

/// No-record servers are skipped while init has not completed (including
/// the config-change `NotStarted` window), so the episode's one
/// announcement is never the placeholder; after init the placeholder is
/// the legitimate fallback for an unrecorded crash.
#[test]
fn classify_defers_placeholder_reason_until_init_completes() {
    use super::mcp_failed_reminder::classify_failed_servers;
    let connected = std::collections::HashSet::new();

    let mut state = crate::session::mcp_servers::McpState::new(vec![http_server("s")]);
    let (failed, unconnected) = classify_failed_servers(&state, &connected);
    assert!(failed.is_empty(), "{failed:?}");
    assert!(unconnected.contains("s"), "episodes must stay alive");

    assert!(state.try_start_init());
    state.finish_init();
    let (failed, _) = classify_failed_servers(&state, &connected);
    assert_eq!(failed.len(), 1);
    assert!(failed[0].detail.is_none(), "{:?}", failed[0]);
}

#[test]
fn classify_collects_facts_and_sorts() {
    use super::mcp_failed_reminder::classify_failed_servers;
    use crate::session::announcement_state::AnnouncedFailure;
    let connected = std::collections::HashSet::new();
    let mut state = crate::session::mcp_servers::McpState::new(vec![
        http_server("b-auth"),
        http_server("a-dead"),
    ]);
    state.record_init_failure("b-auth", true, None);
    state.record_init_failure("a-dead", false, Some("boom".to_string()));

    let (failed, _) = classify_failed_servers(&state, &connected);
    assert_eq!(failed.len(), 2, "{failed:?}");
    // Sorted by name; non-auth HTTP failures retry on use while auth
    // failures need the user; the config identity distinguishes servers.
    assert_eq!(failed[0].name, "a-dead");
    assert_eq!(failed[0].detail.as_deref(), Some("boom"));
    assert_eq!(failed[0].class, AnnouncedFailure::Transport);
    assert!(failed[0].retries_on_use);
    assert_eq!(failed[1].name, "b-auth");
    assert_eq!(failed[1].detail, None);
    assert_eq!(failed[1].class, AnnouncedFailure::AuthRequired);
    assert!(!failed[1].retries_on_use);
    assert_ne!(
        failed[0].config_identity, failed[1].config_identity,
        "identities must reflect the differing configs"
    );
}

#[test]
fn render_failed_section_composes_and_sanitizes_reason_lines() {
    use super::mcp_failed_reminder::render_failed_section;
    use crate::session::announcement_state::{AnnouncedFailure, FailedServer};
    fn entry(
        name: &str,
        detail: Option<&str>,
        class: AnnouncedFailure,
        retries: bool,
    ) -> FailedServer {
        FailedServer {
            name: name.to_string(),
            detail: detail.map(str::to_string),
            class,
            retries_on_use: retries,
            config_identity: 0,
        }
    }
    let section = render_failed_section(&[
        // Control/format characters in remote detail are flattened.
        entry(
            "dead",
            Some("boom\n- forged\u{202E}"),
            AnnouncedFailure::Transport,
            true,
        ),
        entry("oauth", None, AnnouncedFailure::AuthRequired, false),
        // Nothing legible left falls back to the generic reason.
        entry(
            "blank",
            Some("\u{200B}\u{00AD}"),
            AnnouncedFailure::Transport,
            false,
        ),
    ]);
    assert_eq!(
        section,
        "\nMCP servers that failed to connect:\n\
         - dead (\"boom - forged\" — retries automatically on next tool call)\n\
         - oauth (auth required)\n\
         - blank (connection failed)\n"
    );
}

/// A server whose retry handshake is in flight is skipped from the section,
/// its episode survives the retry, and a never-announced server is not
/// announced with a placeholder reason while init is settling — its one
/// announcement waits for the real cause.
#[tokio::test(flavor = "current_thread")]
async fn handshaking_and_init_windows_defer_announcements() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("dead"));
                state.record_init_failure("dead", false, Some("boom".to_string()));
            }

            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            // A retry marks "dead" handshaking (clearing its failure state);
            // "fresh" is configured with no failure record while init is in
            // progress. Neither announces.
            {
                let mut state = actor.mcp_state.lock().await;
                assert!(state.try_start_init(), "fixture must enter Starting");
                state.mark_servers_initializing(["dead".to_string()]);
                assert!(
                    state.is_server_handshaking("dead"),
                    "handshake must be recorded"
                );
                state.configs.push(http_server("fresh"));
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            // The retry fails with a different cause: same episode for
            // "dead" (no re-announcement); "fresh" now has a real cause and
            // announces with it.
            {
                let mut state = actor.mcp_state.lock().await;
                state.mark_server_ready("dead");
                state.record_init_failure("dead", false, Some("timed out".to_string()));
                state.record_init_failure("fresh", false, Some("refused".to_string()));
            }
            refresh_and_inject(&actor).await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 2, "{reminders:?}");
            assert!(!reminders[1].contains("dead ("), "{}", reminders[1]);
            assert!(
                reminders[1].contains("fresh (\"refused\""),
                "{}",
                reminders[1]
            );
        })
        .await;
}

/// Rewind wiring: a committed conversation rewind re-arms failure episodes
/// (the truncated turns may have carried the failure reminder) and marks
/// the MCP reminder dirty.
#[tokio::test(flavor = "current_thread")]
async fn rewind_rearms_failed_server_announcements() {
    use crate::session::{RewindMode, RewindRequest};
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            let mut snap = actor.chat_state_handle.snapshot().await.expect("snapshot");
            snap.conversation = vec![
                ConversationItem::system("SYS"),
                ConversationItem::user("P0"),
                ConversationItem::assistant("A0"),
                ConversationItem::user("P1"),
                ConversationItem::assistant("A1"),
            ];
            snap.prompt_index = 2;
            snap.prompt_texts = vec!["P0".into(), "P1".into()];
            snap.last_compaction_prompt_index = None;
            actor.chat_state_handle.restore_snapshot(snap);

            actor
                .mcp_announcements
                .lock()
                .failed
                .insert("dead".to_string(), Default::default());
            actor
                .mcp_reminder_dirty
                .store(false, std::sync::atomic::Ordering::Relaxed);

            let resp = actor
                .handle_rewind(RewindRequest {
                    target_prompt_index: 1,
                    force: true,
                    mode: RewindMode::ConversationOnly,
                })
                .await
                .expect("handle_rewind ok");
            assert!(resp.success, "{resp:?}");
            assert!(
                actor.mcp_announcements.lock().failed.is_empty(),
                "rewind must re-arm failure episodes"
            );
            assert!(
                actor
                    .mcp_reminder_dirty
                    .load(std::sync::atomic::Ordering::Relaxed),
                "rewind must mark the MCP reminder dirty"
            );
        })
        .await;
}

/// Compaction drops prior reminders from context, so the post-compaction
/// re-arm must make the next injection re-announce still-down servers.
#[tokio::test(flavor = "current_thread")]
async fn rearm_reannounces_still_failed_servers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("dead"));
                state.record_init_failure("dead", false, Some("boom".to_string()));
            }

            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            // The re-arm itself marks the reminder dirty; no manual refresh.
            actor.rearm_failed_server_announcements().await;
            actor.maybe_inject_mcp_reminder().await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 2, "{reminders:?}");
            assert!(reminders[1].contains("dead (\"boom\""), "{}", reminders[1]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_rearms_the_announcement() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("flaky"));
                state.record_init_failure("flaky", false, Some("boom".to_string()));
            }

            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            // Server connects: the episode ends (the delta reminder for the
            // new connection is separate and not counted here).
            connect_server(&actor, "flaky").await;
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            // A NEW failure episode after recovery announces again.
            disconnect_server(&actor).await;
            actor.mcp_state.lock().await.record_init_failure(
                "flaky",
                false,
                Some("down again".to_string()),
            );
            refresh_and_inject(&actor).await;
            let reminders = failed_reminders(&actor).await;
            assert_eq!(reminders.len(), 2, "{reminders:?}");
            assert!(
                reminders[1].contains("flaky (\"down again\""),
                "{}",
                reminders[1]
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn removing_the_server_from_config_ends_the_episode() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("gone"));
                state.record_init_failure("gone", false, Some("boom".to_string()));
            }

            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            // Removed from config: episode ends. Re-adding (still broken)
            // is a fresh episode and announces again.
            actor.mcp_state.lock().await.configs.clear();
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 1);

            {
                let mut state = actor.mcp_state.lock().await;
                state.configs.push(http_server("gone"));
                state.record_init_failure("gone", false, Some("boom".to_string()));
            }
            refresh_and_inject(&actor).await;
            assert_eq!(failed_reminders(&actor).await.len(), 2);
        })
        .await;
}
