#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    /// On resume, a replayed spawn+finish pair leaves the subagent terminal.
    #[test]
    fn replayed_subagent_finished_marks_orphan_terminal() {
        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;

        let spawned = subagent_ext_replay(
            "sess-1",
            serde_json::json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sa-1",
                "parent_session_id": "sess-1",
                "child_session_id": "child-1",
                "subagent_type": "general-purpose",
                "description": "orphan review",
            }),
            "sess-1-1",
        );
        handle_ext_notification(&spawned, &mut app);

        let finished = subagent_ext_replay(
            "sess-1",
            serde_json::json!({
                "sessionUpdate": "subagent_finished",
                "subagent_id": "sa-1",
                "child_session_id": "child-1",
                "status": "cancelled",
                "error": "interrupted by process restart",
                "tool_calls": 0,
                "turns": 0,
                "duration_ms": 1000,
                "tokens_used": 0,
            }),
            "sess-1-2",
        );
        handle_ext_notification(&finished, &mut app);

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent
            .subagent_sessions
            .get("child-1")
            .expect("subagent present after replay");
        assert!(
            info.finished,
            "orphan must be terminal after replayed subagent_finished"
        );
        assert_eq!(info.status.as_deref(), Some("cancelled"));
    }

    /// `cancelled = false` must finalize the row, not revert "killing" to "running".
    #[test]
    fn kill_finalizes_orphan_when_shell_reports_not_cancelled() {
        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;

        let spawned = subagent_ext_replay(
            "sess-1",
            serde_json::json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sa-1",
                "parent_session_id": "sess-1",
                "child_session_id": "child-1",
                "subagent_type": "general-purpose",
                "description": "orphan review",
            }),
            "sess-1-1",
        );
        handle_ext_notification(&spawned, &mut app);

        // User clicks kill after load.
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.loading_replay = false;
            let info = agent.subagent_sessions.get_mut("child-1").unwrap();
            assert!(!info.finished);
            info.pending_kill = true;
            info.kill_requested_at = Some(std::time::Instant::now());
        }

        // Shell: cancelled=false (nothing live), no real status → "cancelled".
        let finalized = finalize_killed_subagent(
            &mut app,
            &acp::SessionId::new("sess-1".to_owned()),
            "sa-1",
            "cancelled",
        );
        assert!(finalized, "row should have been finalized");

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent.subagent_sessions.get("child-1").unwrap();
        assert!(info.finished, "kill must finalize the stuck orphan row");
        assert_eq!(info.status.as_deref(), Some("cancelled"));
        assert!(
            !info.pending_kill,
            "pending_kill must clear so it can't revert"
        );
        assert!(info.kill_requested_at.is_none());
    }

    /// An already-finished subagent killed → finalize stamps the REAL terminal
    /// status (e.g. "completed"), not a forced "cancelled".
    #[test]
    fn kill_finalizes_orphan_with_real_status_when_already_finished() {
        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;

        let spawned = subagent_ext_replay(
            "sess-1",
            serde_json::json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sa-1",
                "parent_session_id": "sess-1",
                "child_session_id": "child-1",
                "subagent_type": "general-purpose",
                "description": "orphan review",
            }),
            "sess-1-1",
        );
        handle_ext_notification(&spawned, &mut app);
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.loading_replay = false;
            let info = agent.subagent_sessions.get_mut("child-1").unwrap();
            info.pending_kill = true;
        }

        let finalized = finalize_killed_subagent(
            &mut app,
            &acp::SessionId::new("sess-1".to_owned()),
            "sa-1",
            "completed",
        );
        assert!(finalized, "row should have been finalized");

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent.subagent_sessions.get("child-1").unwrap();
        assert!(info.finished);
        assert_eq!(
            info.status.as_deref(),
            Some("completed"),
            "already-finished kill must stamp the real terminal status"
        );
    }

    /// NothingLive refreshes an existing terminal child without replacing its
    /// real finish status, statistics, or error detail. The call default
    /// (`completed`/`cancelled`) must not repaint a failed child.
    #[test]
    fn kill_refresh_preserves_existing_terminal_metrics() {
        let mut app = make_app_with_agent("sess-1");
        let spawn = make_ext_session_notification(
            "sess-1",
            test_subagent_spawned("sess-1", "child-1"),
        );
        assert!(handle(spawn, &mut app));
        let finish = make_ext_session_notification(
            "sess-1",
            XaiSessionUpdate::SubagentFinished {
                subagent_id: "child-1".into(),
                child_session_id: "child-1".into(),
                status: "failed".into(),
                error: Some("real failure".into()),
                tool_calls: 7,
                turns: 3,
                duration_ms: 9_876,
                tokens_used: 543,
                output: None,
                will_wake: false,
            },
        );
        assert!(handle(finish, &mut app));
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_sessions
            .get_mut("child-1")
            .unwrap()
            .pending_kill = true;

        assert!(finalize_killed_subagent(
            &mut app,
            &acp::SessionId::new("sess-1".to_owned()),
            "child-1",
            "cancelled",
        ));

        let info = &app.agents[&AgentId(0)].subagent_sessions["child-1"];
        assert!(info.finished);
        assert_eq!(
            info.status.as_deref(),
            Some("failed"),
            "retained terminal status must win over the kill-call default"
        );
        assert_eq!(info.error.as_deref(), Some("real failure"));
        assert_eq!(info.tool_calls, Some(7));
        assert_eq!(info.turns, Some(3));
        assert_eq!(info.duration_ms, Some(9_876));
        assert_eq!(info.tokens_used, Some(543));
        assert!(!info.pending_kill);
        let entry_id = info.scrollback_entry_id.unwrap();
        let entry = app.agents[&AgentId(0)].scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("expected subagent row");
        };
        assert!(
            matches!(sb.kind, SubagentBlockKind::Failed { .. }),
            "parent row must keep Failed, not repaint as Cancelled/Completed"
        );
    }

    /// Re-finalizing an already-finished background child must refresh the
    /// existing completed row, not append a second one.
    #[test]
    fn kill_refresh_does_not_duplicate_background_completed_row() {
        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .tracker
            .task_tool_background
            .insert("child-bg".into(), true);
        assert!(handle(
            make_ext_session_notification(
                "sess-1",
                test_subagent_spawned("sess-1", "child-bg"),
            ),
            &mut app,
        ));
        assert!(handle(
            make_ext_session_notification("sess-1", test_subagent_finished("child-bg")),
            &mut app,
        ));
        let terminal_rows = |app: &crate::app::app_view::AppView| {
            (0..app.agents[&AgentId(0)].scrollback.len())
                .filter(|&idx| {
                    matches!(
                        app.agents[&AgentId(0)].scrollback.entry(idx).map(|e| &e.block),
                        Some(RenderBlock::Subagent(sb))
                            if sb.child_session_id == "child-bg"
                                && !matches!(sb.kind, SubagentBlockKind::Started)
                    )
                })
                .count()
        };
        assert_eq!(terminal_rows(&app), 1);
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_sessions
            .get_mut("child-bg")
            .unwrap()
            .pending_kill = true;

        assert!(finalize_killed_subagent(
            &mut app,
            &acp::SessionId::new("sess-1".to_owned()),
            "child-bg",
            "completed",
        ));

        assert_eq!(terminal_rows(&app), 1);
        let info = &app.agents[&AgentId(0)].subagent_sessions["child-bg"];
        assert!(info.finished);
        assert!(info.is_background);
        assert_eq!(info.status.as_deref(), Some("completed"));
    }

    /// Kill reconciliation / late re-finalize must not append a second
    /// `TurnCompleted` footer on the child transcript (parent-row count alone
    /// misses this).
    #[test]
    fn kill_refresh_does_not_duplicate_child_transcript_footer() {
        use crate::app::agent_view::test_fixtures::count_turn_markers;

        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .tracker
            .task_tool_background
            .insert("child-footer".into(), true);
        assert!(handle(
            make_ext_session_notification(
                "sess-1",
                test_subagent_spawned("sess-1", "child-footer"),
            ),
            &mut app,
        ));
        assert!(handle(
            make_ext_session_notification("sess-1", test_subagent_finished("child-footer")),
            &mut app,
        ));
        let footer_count = |app: &crate::app::app_view::AppView| {
            count_turn_markers(&app.agents[&AgentId(0)].subagent_views["child-footer"])
        };
        assert_eq!(footer_count(&app), 1, "first finish must append one footer");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_sessions
            .get_mut("child-footer")
            .unwrap()
            .pending_kill = true;

        assert!(finalize_killed_subagent(
            &mut app,
            &acp::SessionId::new("sess-1".to_owned()),
            "child-footer",
            "cancelled",
        ));
        assert_eq!(
            footer_count(&app),
            1,
            "re-finalize must not append a second child TurnCompleted footer"
        );
    }

    /// An earlier turn's `TurnCompleted` deeper in the child transcript must
    /// not suppress a later turn's trailing footer; only a trailing footer is
    /// re-finalize-idempotent.
    #[test]
    fn multi_turn_child_keeps_second_footer_on_re_finalize() {
        use crate::app::agent_view::test_fixtures::count_turn_markers;
        use crate::app::subagent::finalize_finished_child_view;

        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .tracker
            .task_tool_background
            .insert("child-multi".into(), true);
        assert!(handle(
            make_ext_session_notification(
                "sess-1",
                test_subagent_spawned("sess-1", "child-multi"),
            ),
            &mut app,
        ));
        // Seed an intermediate turn marker, then later content, then finalize.
        {
            let child = app
                .agents
                .get_mut(&AgentId(0))
                .unwrap()
                .subagent_views
                .get_mut("child-multi")
                .unwrap();
            child
                .scrollback
                .push_block(RenderBlock::session_event(SessionEvent::TurnCompleted {
                    elapsed: Some(std::time::Duration::from_secs(1)),
                }));
            child
                .scrollback
                .push_block(RenderBlock::system("turn-2 content"));
            assert_eq!(
                count_turn_markers(child),
                1,
                "precondition: one earlier-turn footer exists"
            );
            finalize_finished_child_view(child, std::time::Duration::from_secs(2));
            assert_eq!(
                count_turn_markers(child),
                2,
                "second-turn finalize must append its own trailing footer"
            );
            // Re-finalize with no new content must stay idempotent on the tail.
            finalize_finished_child_view(child, std::time::Duration::from_secs(3));
            assert_eq!(
                count_turn_markers(child),
                2,
                "re-finalize must not append a third footer"
            );
        }
    }

    /// Thread-leak regression: every `SubagentSpawned` creates a child
    /// `AgentView` whose `PromptWidget` owns a `HistorySearchState`, and the
    /// matcher thread used to spawn eagerly per view — one leaked thread per
    /// subagent ever spawned, for the process lifetime.
    ///
    /// Drives the real handler with spawn+finish pairs and asserts the exact
    /// invariant on every platform: no child view ever builds a matcher
    /// daemon (each daemon owns exactly one named thread).
    #[test]
    fn subagent_spawn_storm_spawns_no_matcher_daemons() {
        const SUBAGENTS: usize = 50;

        let mut app = make_app_with_agent("sess-parent");
        for i in 0..SUBAGENTS {
            let child_sid = format!("child-storm-{i}");
            handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    test_subagent_spawned("sess-parent", &child_sid),
                ),
                &mut app,
            );
            handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    test_subagent_finished(&child_sid),
                ),
                &mut app,
            );
        }

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.subagent_views.len(),
            SUBAGENTS,
            "every spawn must have created a child view (the leak's unit)"
        );
        let daemons = agent
            .subagent_views
            .values()
            .filter(|v| v.prompt.history_search.daemon_built())
            .count();
        assert_eq!(
            daemons, 0,
            "subagent child views must never spawn history-search matcher threads"
        );
    }

    /// Regression: replay from `updates.jsonl` emits `x.ai/session/update` (not
    /// `session_notification`). Subagent lifecycle events must still populate
    /// `subagent_sessions` and the parent scrollback `SubagentBlock`.
    #[test]
    fn ext_session_update_replay_handles_subagent_spawned_and_finished() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-sess-replay";

        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-parent",
                "x.ai/session/update",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        );
        assert!(
            affected,
            "SubagentSpawned on the active agent must request a redraw"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent
            .subagent_sessions
            .get(child_sid)
            .expect("SubagentSpawned must register subagent_sessions");
        assert_eq!(info.description.as_ref(), "scan src/");
        assert_eq!(info.subagent_type.as_ref(), "explore");
        assert!(
            agent.subagent_views.contains_key(child_sid),
            "SubagentSpawned must create subagent_views eagerly"
        );
        let entry_id = info
            .scrollback_entry_id
            .expect("spawn must stash scrollback_entry_id on SubagentInfo");
        assert_eq!(agent.scrollback.len(), 1);
        let entry = agent.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("SubagentSpawned must push a SubagentBlock to parent scrollback");
        };
        assert_eq!(sb.child_session_id, child_sid);
        assert!(matches!(sb.kind, SubagentBlockKind::Started));
        assert!(agent.scrollback.needs_animation());

        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-parent",
                "x.ai/session/update",
                test_subagent_finished(child_sid),
            ),
            &mut app,
        );
        assert!(
            affected,
            "SubagentFinished on the active agent must request a redraw"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent.subagent_sessions.get(child_sid).unwrap();
        assert!(info.finished);
        assert_eq!(info.status.as_deref(), Some("completed"));
        assert_eq!(info.tool_calls, Some(2));
        assert_eq!(info.turns, Some(1));
        assert_eq!(info.duration_ms, Some(500));
        assert_eq!(info.scrollback_entry_id, Some(entry_id));

        let entry = agent.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("finished subagent must keep the started scrollback entry");
        };
        match &sb.kind {
            SubagentBlockKind::Completed { elapsed } => {
                assert_eq!(*elapsed, std::time::Duration::from_millis(500));
            }
            other => {
                panic!("blocking subagent must mutate started block to Completed, got {other:?}")
            }
        }
        assert!(!entry.is_running, "finish_running must clear running flag");
        assert!(
            !agent.scrollback.needs_animation(),
            "finished subagent entry must not keep scrollback animation"
        );
    }

    #[test]
    fn late_unique_subagent_lifecycle_event_is_not_dropped() {
        let mut app = make_app_with_agent("sess-parent");
        let notification = |update: serde_json::Value, event_seq: u64| {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            let payload = serde_json::json!({
                "sessionId": "sess-parent",
                "update": update,
                "_meta": { "eventId": format!("sess-parent-{event_seq}") },
            });
            let raw = serde_json::value::to_raw_value(&payload).unwrap();
            AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
                request: acp::ExtNotification::new("x.ai/session_notification", raw.into()),
                response_tx: tx,
            })
        };
        let spawned = |child_sid: &str, event_seq: u64| {
            notification(
                serde_json::to_value(test_subagent_spawned("sess-parent", child_sid)).unwrap(),
                event_seq,
            )
        };
        let finished = |child_sid: &str, event_seq: u64| {
            notification(
                serde_json::to_value(test_subagent_finished(child_sid)).unwrap(),
                event_seq,
            )
        };

        for event_seq in 1..=7 {
            assert!(handle(
                spawned(&format!("child-{event_seq}"), event_seq),
                &mut app,
            ));
        }

        // A persisted active goal update can arrive ahead of a lower-ID spawn;
        // it advances the xAI highwater without adding a scrollback block.
        assert!(handle(
            notification(
                serde_json::json!({
                    "sessionUpdate": "goal_updated",
                    "goal_id": "goal-1",
                    "objective": "track lifecycle events",
                    "status": "active",
                    "phase": "executing",
                    "tokens_used": 0,
                    "elapsed_ms": 0,
                    "total_deliverables": 0,
                    "completed_deliverables": 0,
                    "total_worker_rounds": 0,
                    "total_verify_rounds": 0,
                    "token_baseline": 0,
                    "finished_subagent_tokens": 0,
                }),
                100,
            ),
            &mut app,
        ));
        assert_eq!(
            (
                app.agents[&AgentId(0)].last_applied_xai_event_seq,
                app.agents[&AgentId(0)].scrollback.len(),
            ),
            (Some(100), 7)
        );

        let _ = handle(finished("child-1", 50), &mut app);
        assert!(app.agents[&AgentId(0)].subagent_sessions["child-1"].finished);
        assert_eq!(
            app.agents[&AgentId(0)].last_applied_xai_event_seq,
            Some(100),
            "a late lower-ID lifecycle event must not regress the scalar xAI highwater"
        );

        // A restarted producer can reuse an eventId for a different child.
        let _ = handle(spawned("child-reused-id", 1), &mut app);
        assert!(
            app.agents[&AgentId(0)]
                .subagent_sessions
                .contains_key("child-reused-id"),
            "raw eventId reuse must not suppress a new child lifecycle"
        );

        let _ = handle(spawned("child-8", 8), &mut app);
        assert_eq!(
            (
                app.agents[&AgentId(0)].subagent_sessions.len(),
                app.agents[&AgentId(0)].scrollback.len(),
            ),
            (9, 9),
            "a unique late subagent lifecycle event must not be treated as a duplicate"
        );
        assert_eq!(
            app.agents[&AgentId(0)].last_seen_event_id.as_deref(),
            Some("sess-parent-100"),
            "applied late lifecycle must not move the reconnect cursor backwards"
        );

        let _ = handle(finished("child-2", 9), &mut app);
        let _ = handle(spawned("child-8", 8), &mut app);
        let _ = handle(finished("child-2", 9), &mut app);
        assert_eq!(
            (
                app.agents[&AgentId(0)].subagent_sessions.len(),
                app.agents[&AgentId(0)].scrollback.len(),
            ),
            (9, 9),
            "exact spawn and finish redeliveries must remain idempotent"
        );
        assert_eq!(
            app.agents[&AgentId(0)].last_seen_event_id.as_deref(),
            Some("sess-parent-100"),
            "dropped duplicates and late lower-ID applies must not move the reconnect cursor"
        );
    }

    /// xAI lifecycle lines can be delivered in persistence order rather than
    /// event-id order. An early finish is buffered and applied when spawn lands,
    /// even if an unrelated update has already moved the reconnect cursor.
    #[test]
    fn finish_before_spawn_is_applied_after_later_cursor_progress() {
        let mut app = make_app_with_agent("sess-parent");
        let notification = |update: XaiSessionUpdate, event_id: &str| {
            let payload = SessionNotification {
                session_id: acp::SessionId::new("sess-parent"),
                update,
                meta: Some(serde_json::json!({ "eventId": event_id })),
            };
            acp::ExtNotification::new(
                "x.ai/session_notification",
                serde_json::value::to_raw_value(&payload).unwrap().into(),
            )
        };

        assert!(!handle_ext_notification(
            &notification(test_subagent_finished("child-reordered"), "sess-parent-2"),
            &mut app,
        ));
        assert!(app.agents[&AgentId(0)].subagent_sessions.is_empty());
        assert_eq!(app.agents[&AgentId(0)].deferred_subagent_finishes.len(), 1);
        assert_eq!(app.agents[&AgentId(0)].last_seen_event_id, None);

        assert!(handle_ext_notification(
            &notification(
                test_subagent_progress("sess-parent", "unrelated-child"),
                "sess-parent-3",
            ),
            &mut app,
        ));
        assert_eq!(
            app.agents[&AgentId(0)].last_seen_event_id.as_deref(),
            Some("sess-parent-3")
        );

        assert!(handle_ext_notification(
            &notification(
                test_subagent_spawned("sess-parent", "child-reordered"),
                "sess-parent-1",
            ),
            &mut app,
        ));

        let agent = &app.agents[&AgentId(0)];
        let info = &agent.subagent_sessions["child-reordered"];
        assert!(info.finished);
        assert_eq!(info.status.as_deref(), Some("completed"));
        assert!(agent.deferred_subagent_finishes.is_empty());
        assert_eq!(
            agent.last_seen_event_id.as_deref(),
            Some("sess-parent-3"),
            "applying a late lower-ID spawn/finish must keep the higher reconnect cursor"
        );
    }

    /// A retained terminal child can outlive the row discarded by reload. A
    /// replay spawn rebuilds that row without reviving the child.
    #[test]
    fn replay_spawn_rebuild_preserves_retained_finish_without_current_row() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-finished-before-rebuild";
        let spawn = subagent_ext_replay(
            "sess-parent",
            serde_json::to_value(test_subagent_spawned("sess-parent", child_sid)).unwrap(),
            "sess-parent-1",
        );
        let finish = subagent_ext_replay(
            "sess-parent",
            serde_json::to_value(test_subagent_finished(child_sid)).unwrap(),
            "sess-parent-2",
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;

        assert!(handle_ext_notification(&spawn, &mut app));
        let first_entry_id = app.agents[&AgentId(0)].subagent_sessions[child_sid]
            .scrollback_entry_id
            .unwrap();
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .scrollback
            .remove_entry(first_entry_id);
        assert!(handle_ext_notification(&finish, &mut app));
        assert!(app.agents[&AgentId(0)].subagent_sessions[child_sid].finished);
        assert!(app.agents[&AgentId(0)].scrollback.is_empty());

        assert!(handle_ext_notification(&spawn, &mut app));

        let agent = &app.agents[&AgentId(0)];
        let info = &agent.subagent_sessions[child_sid];
        assert!(info.finished, "replay rebuild must retain the terminal state");
        assert_eq!(info.status.as_deref(), Some("completed"));
        let rebuilt_entry_id = info
            .scrollback_entry_id
            .expect("replay spawn must rebuild the missing row");
        assert_ne!(rebuilt_entry_id, first_entry_id);
        let rebuilt_entry = agent.scrollback.get_by_id(rebuilt_entry_id).unwrap();
        let RenderBlock::Subagent(block) = &rebuilt_entry.block else {
            panic!("replay spawn must rebuild a subagent row");
        };
        assert!(matches!(block.kind, SubagentBlockKind::Completed { .. }));
        assert!(!rebuilt_entry.is_running);
        assert!(!agent.scrollback.needs_animation());
    }

    /// Reapplying a retained finish is part of a late replay row rebuild. It
    /// must not masquerade as a live update and close the remaining grace.
    #[test]
    fn late_replay_terminal_rebuild_keeps_grace_for_following_updates() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-late-terminal-rebuild";
        assert!(handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        ));
        assert!(handle(
            make_ext_session_notification("sess-parent", test_subagent_finished(child_sid)),
            &mut app,
        ));
        let entry_id = app.agents[&AgentId(0)].subagent_sessions[child_sid]
            .scrollback_entry_id
            .unwrap();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.scrollback.remove_entry(entry_id);
        agent.arm_late_replay_grace();

        let replay_spawn = subagent_ext_replay(
            "sess-parent",
            serde_json::to_value(test_subagent_spawned("sess-parent", child_sid)).unwrap(),
            "sess-parent-1",
        );
        assert!(handle_ext_notification(&replay_spawn, &mut app));
        assert!(
            app.agents[&AgentId(0)].late_replay_until.is_some(),
            "the retained finish must keep replay delivery semantics"
        );

        let replay_progress = subagent_ext_replay(
            "sess-parent",
            serde_json::to_value(test_subagent_progress("sess-parent", child_sid)).unwrap(),
            "sess-parent-2",
        );
        assert!(
            handle_ext_notification(&replay_progress, &mut app),
            "the next replay update must still apply during late grace"
        );
        let agent = &app.agents[&AgentId(0)];
        assert_eq!(agent.subagent_sessions[child_sid].turn_count, Some(1));
        assert_eq!(agent.last_seen_event_id.as_deref(), Some("sess-parent-2"));
    }

    /// A live duplicate spawn never replaces retained domain state when its row
    /// is temporarily absent. Only accepted replay may rebuild such a row.
    #[test]
    fn live_spawn_without_current_row_stays_idempotent() {
        let mut app = make_app_with_agent("sess-parent");
        let spawn = make_ext_session_notification(
            "sess-parent",
            test_subagent_spawned("sess-parent", "child-live-duplicate"),
        );
        assert!(handle(spawn, &mut app));
        let entry_id = app.agents[&AgentId(0)].subagent_sessions["child-live-duplicate"]
            .scrollback_entry_id
            .unwrap();
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .scrollback
            .remove_entry(entry_id);
        let first_view = app.agents[&AgentId(0)].subagent_views["child-live-duplicate"]
            .as_ref() as *const AgentView;

        let duplicate = make_ext_session_notification(
            "sess-parent",
            test_subagent_spawned("sess-parent", "child-live-duplicate"),
        );
        assert!(!handle(duplicate, &mut app));

        let agent = &app.agents[&AgentId(0)];
        assert!(agent.scrollback.is_empty());
        assert_eq!(
            agent.subagent_views["child-live-duplicate"].as_ref() as *const AgentView,
            first_view,
            "live duplicate spawn must not replace the child view"
        );
        assert_eq!(
            agent.subagent_sessions["child-live-duplicate"].scrollback_entry_id,
            Some(entry_id),
            "live duplicate spawn must preserve retained domain state"
        );
    }

    /// Workflow children intentionally render through their workflow block,
    /// so retained child state—not `scrollback_entry_id`—dedupes replay.
    #[test]
    fn replayed_workflow_spawn_is_idempotent_without_a_child_row() {
        let mut app = make_app_with_agent("sess-parent");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;
        let spawn = || {
            subagent_ext_replay(
                "sess-parent",
                serde_json::to_value(test_subagent_spawned_for_workflow(
                    "sess-parent",
                    "workflow-child",
                    Some("workflow-run".to_string()),
                ))
                .unwrap(),
                "sess-parent-1",
            )
        };

        assert!(handle_ext_notification(&spawn(), &mut app));
        let first_view = app.agents[&AgentId(0)].subagent_views["workflow-child"]
            .as_ref() as *const AgentView;
        assert!(!handle_ext_notification(&spawn(), &mut app));

        let agent = &app.agents[&AgentId(0)];
        assert!(agent.scrollback.is_empty());
        assert_eq!(
            agent.subagent_views["workflow-child"].as_ref() as *const AgentView,
            first_view,
            "duplicate replay must not replace the workflow child's AgentView"
        );
        assert_eq!(
            agent.last_seen_event_id.as_deref(),
            Some("sess-parent-1"),
            "the duplicate replay must not consume the cursor again"
        );
    }

    /// The live activity label fans out to `SubagentInfo` (tasks pane /
    /// dashboard rows) alongside the scrollback block — from both the child
    /// session/update path and the `SubagentProgress` path — and
    /// `SubagentFinished` clears both surfaces.
    #[test]
    fn subagent_activity_label_stamps_info_and_clears_on_finish() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-activity";
        let _ = handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        );

        // A live child message chunk resolves "Responding" and stamps both
        // the block and the info.
        let _ = handle(
            make_agent_chunk_with_event(child_sid, "child text", "p-child", None),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent.subagent_sessions.get(child_sid).unwrap();
        assert_eq!(info.activity_label.as_deref(), Some("Responding"));
        let entry_id = info.scrollback_entry_id.unwrap();
        let entry = agent.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("expected Subagent block");
        };
        assert_eq!(sb.activity_label, info.activity_label);

        // SubagentProgress recomputes from the child tracker and restamps.
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_sessions
            .get_mut(child_sid)
            .unwrap()
            .activity_label = None;
        let _ = handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_progress("sess-parent", child_sid),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .subagent_sessions
                .get(child_sid)
                .unwrap()
                .activity_label
                .as_deref(),
            Some("Responding")
        );

        let _ = handle(
            make_ext_session_notification("sess-parent", test_subagent_finished(child_sid)),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent.subagent_sessions.get(child_sid).unwrap();
        assert!(
            info.activity_label.is_none(),
            "finish must clear the info label"
        );
        let entry = agent.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("expected Subagent block");
        };
        assert!(
            sb.activity_label.is_none(),
            "finish must clear the block label"
        );
    }

    #[test]
    fn subagent_tool_call_delta_stamps_writing_activity_label() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-writing";
        let _ = handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        );

        let changed = handle(
            make_ext_session_notification(
                child_sid,
                XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: Some("call_1".into()),
                    tool_index: 0,
                    name: Some("write".into()),
                    arguments_delta: None,
                },
            ),
            &mut app,
        );
        assert!(changed, "first delta must request a redraw");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent.subagent_sessions.get(child_sid).unwrap();
        assert_eq!(info.activity_label.as_deref(), Some("Writing file…"));
    }

    #[test]
    fn subagent_tool_call_delta_ignored_while_child_loading_replay() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-writing-replay";
        let _ = handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_views
            .get_mut(child_sid)
            .unwrap()
            .session
            .loading_replay = true;

        let changed = handle(
            make_ext_session_notification(
                child_sid,
                XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: Some("call_1".into()),
                    tool_index: 0,
                    name: Some("write".into()),
                    arguments_delta: None,
                },
            ),
            &mut app,
        );
        assert!(!changed);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent
                .subagent_sessions
                .get(child_sid)
                .unwrap()
                .activity_label
                .is_none()
        );
        assert_eq!(
            agent
                .subagent_views
                .get(child_sid)
                .unwrap()
                .session
                .tracker
                .activity(),
            None,
            "reloading child tracker must not pick up the delta"
        );
    }

    #[test]
    fn subagent_tool_call_delta_without_registry_row_reports_no_redraw() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-writing-orphan";
        let _ = handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_sessions
            .remove(child_sid);

        let changed = handle(
            make_ext_session_notification(
                child_sid,
                XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: Some("call_1".into()),
                    tool_index: 0,
                    name: Some("write".into()),
                    arguments_delta: None,
                },
            ),
            &mut app,
        );
        assert!(!changed, "no registry row → nothing visible → no redraw");
    }

    #[test]
    fn subagent_acp_chunk_after_finish_does_not_restamp_label() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-acp-finished";
        let _ = handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        );
        let _ = handle(
            make_ext_session_notification("sess-parent", test_subagent_finished(child_sid)),
            &mut app,
        );
        // Simulate the racing child rail still looking live.
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_views
            .get_mut(child_sid)
            .unwrap()
            .session
            .state = AgentState::TurnRunning;

        let _ = handle(
            make_agent_chunk_with_event(child_sid, "late text", "p-child", None),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent
                .subagent_sessions
                .get(child_sid)
                .unwrap()
                .activity_label
                .is_none(),
            "finished row must not be re-stamped by the child ACP fan-out"
        );
    }

    #[test]
    fn subagent_tool_call_delta_after_finish_does_not_restamp_label() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-writing-finished";
        let _ = handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        );
        let _ = handle(
            make_ext_session_notification("sess-parent", test_subagent_finished(child_sid)),
            &mut app,
        );

        let changed = handle(
            make_ext_session_notification(
                child_sid,
                XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: Some("call_1".into()),
                    tool_index: 0,
                    name: Some("write".into()),
                    arguments_delta: None,
                },
            ),
            &mut app,
        );
        assert!(!changed);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent
                .subagent_sessions
                .get(child_sid)
                .unwrap()
                .activity_label
                .is_none(),
            "finished row must keep its cleared label"
        );
    }

    /// Regression: replayed SubagentSpawned (resumed_from unset) must load child
    /// updates.jsonl so fullscreen scrollback is not prompt-only.
    #[test]
    fn subagent_spawned_replays_child_updates_without_resumed_from() {
        with_replay_disk_home(|_| {
            let child_sid = "child-with-updates";
            let mut app = make_app_with_agent("sess-parent");
            spawn_subagent_with_optional_updates(
                &mut app,
                child_sid,
                Some(&(child_tool_line(child_sid) + "\n")),
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "spawn must replay exactly one tool call"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| !i.transcript.needs_replay()),
                "an emitting spawn replay must settle the transcript state"
            );
        });
    }

    /// Isolated `isReplay` with `loading_replay == false`: drop_unexpected_replay
    /// runs before SubagentSpawned (`!meta.is_replay` is defense-in-depth).
    #[test]
    fn replayed_subagent_spawned_without_loading_replay_is_dropped() {
        with_replay_disk_home(|_| {
            let child_sid = "child-unexpected-replay";
            let mut app = make_app_with_agent("sess-parent");
            assert!(!app.agents[&AgentId(0)].session.loading_replay);
            write_child_updates_jsonl(
                replay_disk_test_home(),
                child_sid,
                &(child_tool_line(child_sid) + "\n"),
            );
            let spawned = subagent_ext_replay(
                "sess-parent",
                serde_json::json!({
                    "sessionUpdate": "subagent_spawned",
                    "subagent_id": child_sid,
                    "parent_session_id": "sess-parent",
                    "child_session_id": child_sid,
                    "subagent_type": "explore",
                    "description": "scan src/",
                }),
                "sess-parent-1",
            );
            handle_ext_notification(&spawned, &mut app);
            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert!(
                agent.subagent_sessions.is_empty(),
                "unexpected replay spawn must not register"
            );
            assert!(agent.subagent_views.is_empty());
        });
    }

    #[test]
    fn late_replay_grace_accepts_is_replay_after_loading_replay_clears() {
        let mut app = make_app_with_agent("sess-late");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert!(!agent.session.loading_replay);
        agent.arm_late_replay_grace();
        let meta = crate::acp::meta::NotificationMeta {
            is_replay: true,
            ..crate::acp::meta::NotificationMeta::default()
        };
        assert!(
            !drop_unexpected_replay(agent, &meta, "sess-late", "test"),
            "isReplay during late grace must apply"
        );
    }

    #[test]
    fn live_update_closes_late_replay_grace() {
        let mut app = make_app_with_agent("sess-late");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.arm_late_replay_grace();
        let live = crate::acp::meta::NotificationMeta::default();
        assert!(!drop_unexpected_replay(agent, &live, "sess-late", "test"));
        let replay = crate::acp::meta::NotificationMeta {
            is_replay: true,
            ..crate::acp::meta::NotificationMeta::default()
        };
        assert!(
            drop_unexpected_replay(agent, &replay, "sess-late", "test"),
            "this-session live must close late grace"
        );
    }

    /// Resume: a `SubagentSpawned` during `loading_replay` must defer the child
    /// transcript load (the dominant large-session resume cost) to first open.
    #[test]
    fn subagent_spawned_during_resume_defers_child_replay_until_open() {
        with_replay_disk_home(|_| {
            let child_sid = "child-resume-defer";
            let mut app = make_app_with_agent("sess-parent");
            // Simulate resume: the parent agent is replaying its own session.
            app.agents
                .get_mut(&AgentId(0))
                .unwrap()
                .session
                .loading_replay = true;

            spawn_subagent_with_optional_updates(
                &mut app,
                child_sid,
                Some(&(child_tool_line(child_sid) + "\n")),
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "resume spawn must NOT eagerly replay the child transcript"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| i.transcript.needs_replay()),
                "resume spawn must leave the transcript NeedsReplay for lazy load"
            );

            // Opening the subagent later triggers the deferred (lazy) replay.
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "opening the subagent after resume must lazily replay its transcript"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| !i.transcript.needs_replay()),
                "lazy open must settle the transcript state"
            );
        });
    }

    /// Regression (resume): a subagent that already finished must still show its
    /// full transcript on open. The finished handler's `TurnCompleted` push is
    /// suppressed during replay — otherwise it vetoes the deferred load
    /// (`scrollback_is_prompt_only`), leaving a permanently empty transcript.
    #[test]
    fn subagent_resume_finished_then_open_shows_full_transcript() {
        with_replay_disk_home(|_| {
            let child_sid = "child-resume-finished";
            let mut app = make_app_with_agent("sess-parent");
            app.agents
                .get_mut(&AgentId(0))
                .unwrap()
                .session
                .loading_replay = true;

            spawn_subagent_with_optional_updates(
                &mut app,
                child_sid,
                Some(&(child_tool_line(child_sid) + "\n")),
            );
            let _ = handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    test_subagent_finished(child_sid),
                ),
                &mut app,
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "resume must not eagerly load the finished subagent transcript"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| i.transcript.needs_replay()),
                "finished-during-resume must leave the transcript NeedsReplay"
            );
            // Even deferred, a finished subagent must not show a running spinner.
            assert!(
                matches!(
                    agent.subagent_views.get(child_sid).unwrap().session.state,
                    AgentState::Idle
                ),
                "finished subagent must be Idle after resume, not TurnRunning"
            );

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "opening a finished subagent after resume must show its transcript"
            );
            // The lazy load reapplies the "Worked for" footer (live parity).
            let child = agent.subagent_views.get(child_sid).unwrap();
            assert!(
                (0..child.scrollback.len()).any(|i| child
                    .scrollback
                    .entry(i)
                    .is_some_and(|e| matches!(e.block, RenderBlock::SessionEvent(_)))),
                "opened finished subagent must show a TurnCompleted footer"
            );
        });
    }

    /// Finish-time / close-time eviction and rebuild-on-open scenarios.
    ///
    /// Each scenario owns one parent (`sess-parent`) plus a child driven
    /// through the real notification handler, and the child's on-disk
    /// transcript under the shared replay test home.
    mod eviction_and_rebuild {
        use super::*;
        use crate::app::subagent::ChildTranscript;

        struct Scenario {
            app: AppView,
            child_sid: &'static str,
        }

        impl Drop for Scenario {
            fn drop(&mut self) {
                crate::app::subagent::set_replay_grok_home_for_tests(None);
            }
        }

        impl Scenario {
            /// Live-spawn `child_sid`, first writing `updates` to the child's
            /// `updates.jsonl` (`None` = nothing persisted).
            fn spawn(child_sid: &'static str, updates: Option<String>) -> Self {
                let home = replay_disk_test_home();
                crate::app::subagent::set_replay_grok_home_for_tests(Some(home.to_path_buf()));
                let mut app = make_app_with_agent("sess-parent");
                spawn_subagent_with_optional_updates(&mut app, child_sid, updates.as_deref());
                Scenario { app, child_sid }
            }

            /// Spawn a second child under the same parent.
            fn spawn_sibling(&mut self, child_sid: &str, updates: Option<String>) {
                spawn_subagent_with_optional_updates(&mut self.app, child_sid, updates.as_deref());
            }

            fn agent(&self) -> &AgentView {
                self.app.agents.get(&AgentId(0)).unwrap()
            }

            fn agent_mut(&mut self) -> &mut AgentView {
                self.app.agents.get_mut(&AgentId(0)).unwrap()
            }

            fn transcript(&self) -> ChildTranscript {
                self.agent().subagent_sessions[self.child_sid].transcript
            }

            fn set_transcript(&mut self, state: ChildTranscript) {
                let sid = self.child_sid;
                self.agent_mut()
                    .subagent_sessions
                    .get_mut(sid)
                    .unwrap()
                    .transcript = state;
            }

            fn set_background(&mut self) {
                let sid = self.child_sid;
                self.agent_mut()
                    .subagent_sessions
                    .get_mut(sid)
                    .unwrap()
                    .is_background = true;
            }

            /// Deliver the child's `SubagentFinished` through the real handler.
            fn finish(&mut self) {
                let _ = handle(
                    make_ext_session_notification_with_method(
                        "sess-parent",
                        "x.ai/session/update",
                        test_subagent_finished(self.child_sid),
                    ),
                    &mut self.app,
                );
            }

            fn open(&mut self) {
                self.open_child(self.child_sid);
            }

            fn open_child(&mut self, child_sid: &str) {
                let sid = child_sid.to_string();
                self.agent_mut().open_subagent_fullscreen(sid);
            }

            fn close(&mut self) {
                self.agent_mut().close_subagent_fullscreen();
            }

            fn push_child_block(&mut self, block: RenderBlock) {
                let sid = self.child_sid;
                self.agent_mut()
                    .subagent_views
                    .get_mut(sid)
                    .unwrap()
                    .scrollback
                    .push_block(block);
            }

            fn tool_calls(&self) -> usize {
                self.tool_calls_for(self.child_sid)
            }

            fn tool_calls_for(&self, child_sid: &str) -> usize {
                child_scrollback_tool_call_count(self.agent(), child_sid)
            }

            fn session_events(&self) -> usize {
                child_scrollback_session_event_count(self.agent(), self.child_sid)
            }

            fn prompts_matching(&self, prompt: &str) -> usize {
                child_scrollback_matching_prompt_count(self.agent(), self.child_sid, prompt)
            }

            fn has_system_block(&self) -> bool {
                let child = self.agent().subagent_views.get(self.child_sid).unwrap();
                (0..child.scrollback.len()).any(|i| {
                    matches!(
                        child.scrollback.entry(i).map(|e| &e.block),
                        Some(RenderBlock::System(_))
                    )
                })
            }

            /// Count of compaction markers in the child scrollback.
            fn compaction_markers(&self) -> usize {
                let child = self.agent().subagent_views.get(self.child_sid).unwrap();
                (0..child.scrollback.len())
                    .filter(|i| {
                        matches!(
                            child.scrollback.entry(*i).map(|e| &e.block),
                            Some(RenderBlock::SessionEvent(b)) if matches!(
                                b.event,
                                SessionEvent::CompactionStarted { .. }
                                    | SessionEvent::CompactionCompleted { .. }
                            )
                        )
                    })
                    .count()
            }

            fn updates_path(&self) -> std::path::PathBuf {
                replay_disk_test_home()
                    .join("sessions")
                    .join(urlencoding::encode("/tmp").as_ref())
                    .join(self.child_sid)
                    .join("updates.jsonl")
            }

            /// Replace `updates.jsonl` with a directory so a rebuild read
            /// fails (`Err`, not the missing-file `Empty`).
            fn break_transcript(&self) {
                let path = self.updates_path();
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }

            /// Replace the (possibly broken) transcript with `content`.
            fn replace_transcript(&self, content: &str) {
                let path = self.updates_path();
                if path.is_dir() {
                    std::fs::remove_dir(&path).unwrap();
                }
                std::fs::write(&path, content).unwrap();
            }

            /// Remove the child session dir so a rebuild resolves `Empty`.
            fn remove_session_dir(&self) {
                std::fs::remove_dir_all(self.updates_path().parent().unwrap()).unwrap();
            }
        }

        fn child_compaction_started_line(child_sid: &str) -> String {
            format!(
                r#"{{"method":"_x.ai/session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"auto_compact_started","tokens_used":9000,"context_window":10000,"percentage":90,"reason":"threshold"}}}}}}"#
            )
        }

        fn child_compaction_completed_line(child_sid: &str) -> String {
            format!(
                r#"{{"method":"_x.ai/session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"auto_compact_completed","tokens_after":100,"elapsed_ms":5}}}}}}"#
            )
        }

        /// Memory regression: a finished child view's transcript is evicted
        /// on the live path (the eagerly-loaded scrollback is dropped and
        /// the transcript re-armed `NeedsReplay`) and first open rebuilds
        /// the full transcript from disk with the `TurnCompleted` footer,
        /// through the same deferred path resume uses.
        #[test]
        fn live_finish_evicts_child_transcript_and_open_rebuilds_it() {
            let child_sid = "child-live-evict";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            // Live spawn eagerly loads the on-disk transcript into the view.
            assert_eq!(s.tool_calls(), 1);

            s.finish();
            let child = s.agent().subagent_views.get(child_sid).unwrap();
            assert_eq!(
                child.scrollback.len(),
                0,
                "live finish must evict the retained child transcript"
            );
            assert!(matches!(child.session.state, AgentState::Idle));
            assert_eq!(
                s.transcript(),
                ChildTranscript::NeedsReplay,
                "eviction must re-arm the deferred on-open replay"
            );

            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "opening an evicted finished subagent must rebuild its transcript"
            );
            assert_eq!(
                s.session_events(),
                1,
                "rebuilt transcript must end with the TurnCompleted footer"
            );
        }

        /// The transcript the user is currently viewing is never blanked: a
        /// child open fullscreen keeps its scrollback and gets the footer in
        /// place.
        #[test]
        fn live_finish_keeps_transcript_of_open_child() {
            let child_sid = "child-open-no-evict";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.open();

            s.finish();
            assert_eq!(
                s.tool_calls(),
                1,
                "the open child's transcript must survive its finish"
            );
            assert_eq!(
                s.session_events(),
                1,
                "the open child must be finalized in place with the footer"
            );
        }

        /// Eviction re-injects the task prompt (resume-state parity), and the
        /// on-open replay dedups its on-disk echo: exactly one prompt after
        /// evict → open, alongside the rebuilt transcript.
        #[test]
        fn evicted_child_keeps_prompt_and_open_shows_it_once() {
            let child_sid = "child-evict-prompt";
            let task = "scan src/ for auth";
            write_subagent_meta_json(replay_disk_test_home(), "sess-parent", child_sid, task);
            let updates = format!(
                "{}\n{}",
                child_user_message_line(child_sid, task),
                child_tool_line(child_sid)
            );
            let mut s = Scenario::spawn(child_sid, Some(updates));

            s.finish();
            // Evicted to the resume state: task prompt only.
            assert_eq!(s.prompts_matching(task), 1);
            assert_eq!(s.tool_calls(), 0);

            s.open();
            assert_eq!(
                s.prompts_matching(task),
                1,
                "the replayed on-disk echo must dedup against the re-injected prompt"
            );
            assert_eq!(s.tool_calls(), 1);
        }

        /// Closing the fullscreen view of a finished child evicts it: the
        /// counterpart of finish-time eviction for children that were open
        /// (and thus guarded) when they finished, or re-inflated by a later
        /// open.
        #[test]
        fn closing_fullscreen_finished_child_evicts_it() {
            let child_sid = "child-close-evict";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.open();
            s.finish();
            // Open at finish: guarded, transcript kept.
            assert_eq!(s.tool_calls(), 1);

            s.close();
            assert_eq!(s.agent().active_subagent, None);
            assert_eq!(
                s.tool_calls(),
                0,
                "closing a finished child must evict its transcript"
            );
            // And it still rebuilds on the next open.
            s.open();
            assert_eq!(s.tool_calls(), 1);
        }

        /// A late child event landing in an evicted scrollback must not veto
        /// the on-open rebuild: finished + NeedsReplay always re-replays.
        #[test]
        fn late_event_after_evict_does_not_veto_open_rebuild() {
            let child_sid = "child-late-event";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.finish();
            // Late block sneaks into the evicted (prompt-less) scrollback.
            s.push_child_block(RenderBlock::system("late notice".to_string()));

            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "the on-open rebuild must not be vetoed by a late block"
            );
        }

        /// Background children keep their transcript: they can receive
        /// monitor/bg-task output after finish, and a late block in an
        /// emptied scrollback would veto the on-open re-replay.
        #[test]
        fn live_finish_keeps_transcript_of_background_child() {
            let child_sid = "child-bg-no-evict";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.set_background();

            s.finish();
            assert_eq!(
                s.tool_calls(),
                1,
                "a background child's transcript must survive its finish"
            );
        }

        /// Switching the fullscreen takeover to another child evicts the
        /// finished child being left, exactly like closing it.
        #[test]
        fn switching_fullscreen_to_another_child_evicts_the_finished_one() {
            let child_a = "child-switch-a";
            let child_b = "child-switch-b";
            let mut s = Scenario::spawn(child_a, Some(child_tool_line(child_a) + "\n"));
            s.spawn_sibling(child_b, Some(child_tool_line(child_b) + "\n"));
            s.open();
            s.finish();
            // Open at finish: guarded, transcript kept.
            assert_eq!(s.tool_calls(), 1);

            s.open_child(child_b);
            assert_eq!(s.agent().active_subagent.as_deref(), Some(child_b));
            assert_eq!(
                s.tool_calls_for(child_a),
                0,
                "switching away must evict the finished child being left"
            );
            assert_eq!(
                s.transcript(),
                ChildTranscript::NeedsReplay,
                "the evicted child must re-arm its on-open rebuild"
            );
        }

        /// Closing the fullscreen view of a still-running child must NOT
        /// evict: its live transcript keeps streaming and is not yet on disk
        /// in full.
        #[test]
        fn closing_fullscreen_running_child_keeps_transcript() {
            let child_sid = "child-close-running";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.open();
            s.close();
            assert_eq!(s.agent().active_subagent, None);
            assert_eq!(
                s.tool_calls(),
                1,
                "closing a still-running child must keep its live transcript"
            );
        }

        /// A failed rebuild (transient read error on `updates.jsonl`) must
        /// stay `NeedsReplay`: post-eviction, disk is the only copy, so the
        /// next open must retry once the I/O condition clears.
        #[test]
        fn failed_rebuild_after_evict_retries_on_next_open() {
            let child_sid = "child-evict-io-retry";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.finish();

            s.break_transcript();
            s.open();
            assert_eq!(s.tool_calls(), 0);
            assert_eq!(
                s.transcript(),
                ChildTranscript::NeedsReplay,
                "a failed rebuild must not settle the transcript state"
            );
            assert_eq!(
                s.session_events(),
                0,
                "a failed rebuild must not apply the TurnCompleted footer"
            );

            // I/O condition clears; the next open retries and rebuilds.
            s.replace_transcript(&(child_tool_line(child_sid) + "\n"));
            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "the next open must retry the rebuild after a failed read"
            );
            assert_eq!(
                s.session_events(),
                1,
                "the successful retry must apply exactly one footer"
            );
        }

        /// An `Empty` rebuild (missing transcript, not a read error) of a
        /// bare evicted view leaves a retriable husk: prompt + footer stay,
        /// the transcript stays `NeedsReplay`, and a late-landing persistence
        /// flush is picked up by the next open instead of being lost to a
        /// permanently "replayed" husk.
        #[test]
        fn empty_rebuild_of_evicted_view_retries_until_flush_lands() {
            let child_sid = "child-flush-race";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.finish();

            // The child's final persistence flush has not landed yet.
            s.replace_transcript("");
            s.open();
            assert_eq!(s.tool_calls(), 0, "nothing to rebuild from yet");
            assert_eq!(
                s.transcript(),
                ChildTranscript::NeedsReplay,
                "an Empty rebuild must stay NeedsReplay, not settle as replayed"
            );
            s.close();

            // The flush lands; the next open rebuilds the real transcript.
            s.replace_transcript(&(child_tool_line(child_sid) + "\n"));
            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "the open after the late flush must rebuild the transcript"
            );
        }

        /// Re-activating the prompt-plus-footer husk left by an `Empty`
        /// rebuild must not settle it: the footer is rebuild-stamped, not
        /// transcript content, so the husk stays `NeedsReplay` and a late
        /// persistence flush is still replayed.
        #[test]
        fn empty_rebuild_husk_survives_reactivation_until_flush_lands() {
            let child_sid = "child-husk-reopen";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.finish();

            // The flush has not landed; the first open leaves the husk.
            s.replace_transcript("");
            s.open();
            assert_eq!(s.transcript(), ChildTranscript::NeedsReplay);

            // Activating the already-open child re-runs the ensure path.
            s.open();
            assert_eq!(
                s.transcript(),
                ChildTranscript::NeedsReplay,
                "re-activating the husk must not settle it as the only copy"
            );
            assert_eq!(s.session_events(), 1, "the husk keeps exactly one footer");

            // The flush lands; the next open rebuilds the real transcript.
            s.replace_transcript(&(child_tool_line(child_sid) + "\n"));
            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "the late flush must still be replayed after husk reactivation"
            );
        }

        /// A rebuild from a partially flushed transcript is transient, not
        /// permanent: the close-path eviction of a `DiskBacked` view re-arms
        /// `NeedsReplay`, so the open after the rest of the flush lands
        /// replays the completed file.
        #[test]
        fn partial_flush_rebuild_self_heals_on_next_open_cycle() {
            let child_sid = "child-partial-flush";
            // Only a prefix of the final transcript is on disk at finish time.
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.finish();

            s.open();
            assert_eq!(s.tool_calls(), 1, "the first open rebuilds the prefix");
            assert_eq!(s.transcript(), ChildTranscript::DiskBacked);
            s.close();
            assert_eq!(
                s.transcript(),
                ChildTranscript::NeedsReplay,
                "closing a disk-backed view must re-arm the replay"
            );

            // The rest of the flush lands after the partial rebuild.
            let second_line = child_tool_line(child_sid)
                .replace(r#""toolCallId":"tc1""#, r#""toolCallId":"tc2""#);
            s.replace_transcript(&format!(
                "{}\n{}\n",
                child_tool_line(child_sid),
                second_line
            ));
            s.open();
            assert_eq!(
                s.tool_calls(),
                2,
                "the open after the late append must replay the completed file"
            );
        }

        /// A finished background child with in-memory content and an
        /// unsettled transcript (failed eager replay) must NOT rebuild from
        /// disk: it is never evicted and its view may hold post-finish
        /// monitor output that is not on disk, so the in-memory view stays
        /// the source of truth.
        #[test]
        fn finished_background_child_with_content_never_rebuilds() {
            let child_sid = "child-bg-no-rebuild";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.set_background();
            // Simulate a failed eager replay: populated view, unsettled state.
            s.set_transcript(ChildTranscript::NeedsReplay);

            s.finish();
            assert_eq!(s.tool_calls(), 1);

            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "opening a populated background child must not append the disk transcript"
            );
        }

        /// A failed rebuild of a populated foreground view (failed eager
        /// replay, finished while open, re-opened) restores the pre-reset
        /// content instead of leaving a bare view; disk is retried on a
        /// later open.
        #[test]
        fn failed_rebuild_restores_populated_view() {
            let child_sid = "child-restore";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            // Open at finish → guarded → finalized in place; simulate the
            // eager replay having failed while the view keeps its
            // (live-streamed) content.
            s.open();
            s.set_transcript(ChildTranscript::NeedsReplay);
            s.finish();
            assert_eq!(s.tool_calls(), 1);
            assert_eq!(s.session_events(), 1);

            // Break the transcript, then re-open: the rebuild fails and the
            // pre-reset content must come back, footer included.
            s.break_transcript();
            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "a failed rebuild must restore the populated view"
            );
            assert_eq!(
                s.session_events(),
                1,
                "the restored view keeps its footer; no extra footer is applied"
            );

            // Repair the disk: the restored view must still reset and rebuild
            // on the next open (restore re-arms the retry, not just once).
            s.replace_transcript(&(child_tool_line(child_sid) + "\n"));
            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "the repaired disk must rebuild cleanly on the next open"
            );
            assert_eq!(
                s.transcript(),
                ChildTranscript::DiskBacked,
                "a successful rebuild after restore must settle the state"
            );
        }

        /// A missing transcript (`Ok(Empty)`, not a read error) must also
        /// restore a populated view: the in-memory copy is the only one, and
        /// dropping it for a prompt-plus-footer husk loses the transcript
        /// permanently.
        #[test]
        fn empty_rebuild_restores_populated_view_and_settles() {
            let child_sid = "child-restore-empty";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            // Open at finish → guarded → finalized in place; eager replay
            // "failed": unsettled state, populated view kept.
            s.open();
            s.set_transcript(ChildTranscript::NeedsReplay);
            s.finish();

            // The session dir vanishes before the next open (cleanup /
            // relocation): the rebuild resolves to Empty, not an error.
            s.remove_session_dir();
            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "an Empty rebuild must restore the populated view, not drop it"
            );
            assert_eq!(
                s.session_events(),
                1,
                "the restored view keeps exactly one footer"
            );
            assert_eq!(
                s.transcript(),
                ChildTranscript::MemoryOnly,
                "with nothing on disk the restored copy is authoritative — no retry loop"
            );

            // The restored copy is memory-only: closing the fullscreen (the
            // would-be evict) must NOT drop it: there is nothing on disk to
            // rebuild from.
            s.close();
            assert_eq!(
                s.tool_calls(),
                1,
                "closing a memory-only transcript must not evict it"
            );
            s.open();
            assert_eq!(s.tool_calls(), 1);
        }

        /// A live-streamed transcript that never reached disk (child
        /// persistence wrote nothing, or the finish raced the flush) must
        /// not be dropped: the eviction probe finds no persisted copy and
        /// keeps the only copy; the first open's `Empty` rebuild then
        /// settles it as memory-only, permanently exempt from eviction.
        #[test]
        fn finish_without_persisted_transcript_keeps_only_copy() {
            let child_sid = "child-no-disk-copy";
            let mut s = Scenario::spawn(child_sid, None);
            // Live-streamed content that exists only in memory.
            s.push_child_block(RenderBlock::system("streamed output".to_string()));
            assert_eq!(s.transcript(), ChildTranscript::NeedsReplay);

            s.finish();
            assert_eq!(
                s.transcript(),
                ChildTranscript::NeedsReplay,
                "a probe miss keeps retrying rather than settling"
            );
            assert!(s.has_system_block(), "the only copy must survive the finish");

            s.open();
            assert_eq!(
                s.transcript(),
                ChildTranscript::MemoryOnly,
                "the open's Empty rebuild settles the restored copy as memory-only"
            );
            assert!(s.has_system_block());
            s.close();
            assert!(
                s.has_system_block(),
                "close must never drop the memory-only copy"
            );
        }

        /// A non-empty persisted file that would not EMIT on replay (here
        /// xAI-only content; torn or catalog-only lines behave the same) is
        /// not proof of a disk copy: the finish-time eviction probe must
        /// keep the only in-memory copy instead of dropping content the
        /// rebuild can never recreate.
        #[test]
        fn finish_with_non_emitting_transcript_keeps_only_copy() {
            let child_sid = "child-non-emitting-disk";
            let updates = child_compaction_started_line(child_sid) + "\n";
            let mut s = Scenario::spawn(child_sid, Some(updates));
            // Live-streamed content that exists only in memory (the xAI-only
            // spawn replay was `Empty`, so nothing settled `DiskBacked`).
            s.push_child_block(RenderBlock::system("streamed output".to_string()));
            assert_eq!(s.transcript(), ChildTranscript::NeedsReplay);

            s.finish();
            assert!(
                s.has_system_block(),
                "a non-emitting disk copy must not license dropping the only copy"
            );
            assert_eq!(
                s.transcript(),
                ChildTranscript::NeedsReplay,
                "the probe miss keeps retrying rather than settling"
            );

            s.open();
            assert_eq!(
                s.transcript(),
                ChildTranscript::MemoryOnly,
                "the open's Empty rebuild restores the copy as memory-only"
            );
            assert!(s.has_system_block());
            s.close();
            assert!(
                s.has_system_block(),
                "close must never drop the memory-only copy"
            );
        }

        /// finish → evict → reopen preserves persisted child xAI events: the
        /// rebuilt transcript re-renders the compaction markers the live view
        /// had (start inline, completion flushed with the footer).
        #[test]
        fn rebuild_after_evict_preserves_child_compaction_markers() {
            let child_sid = "child-xai-marker";
            let updates = format!(
                "{}\n{}\n{}\n",
                child_tool_line(child_sid),
                child_compaction_started_line(child_sid),
                child_compaction_completed_line(child_sid),
            );
            let mut s = Scenario::spawn(child_sid, Some(updates));
            assert!(
                s.compaction_markers() >= 1,
                "the eager spawn replay must render the compaction marker"
            );

            s.finish();
            assert_eq!(s.tool_calls(), 0, "finish must evict the transcript");

            s.open();
            assert_eq!(s.tool_calls(), 1);
            assert_eq!(
                s.compaction_markers(),
                2,
                "the rebuilt transcript must keep both compaction markers"
            );
        }
    }

    /// Regression (resume): with a meta.json task prompt AND a persisted child
    /// transcript that echoes that prompt, opening after resume shows the task
    /// exactly once — the deferred open must dedup the replayed prompt echo.
    #[test]
    fn subagent_resume_with_meta_prompt_shows_task_once_after_open() {
        with_replay_disk_home(|home| {
            let parent_sid = "sess-parent";
            let child_sid = "child-resume-meta";
            let task = "scan src/ for auth";
            write_subagent_meta_json(home, parent_sid, child_sid, task);

            let mut app = make_app_with_agent(parent_sid);
            app.agents
                .get_mut(&AgentId(0))
                .unwrap()
                .session
                .loading_replay = true;

            let updates = format!(
                "{}\n{}",
                child_user_message_line(child_sid, task),
                child_tool_line(child_sid)
            );
            spawn_subagent_with_optional_updates(&mut app, child_sid, Some(&updates));

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_matching_prompt_count(agent, child_sid, task),
                1,
                "task prompt must appear exactly once after resume + open"
            );
            assert_eq!(child_scrollback_tool_call_count(agent, child_sid), 1);
        });
    }

    /// Regression: replayed user_message_chunk + meta prompt must not duplicate via injection.
    #[test]
    fn subagent_spawn_replay_and_meta_prompt_shows_task_once() {
        with_replay_disk_home(|home| {
            let parent_sid = "sess-parent";
            let child_sid = "child-prompt-once";
            let task = "scan src/ for auth";
            write_subagent_meta_json(home, parent_sid, child_sid, task);

            let mut app = make_app_with_agent(parent_sid);
            let updates = format!(
                "{}\n{}",
                child_user_message_line(child_sid, task),
                child_tool_line(child_sid)
            );
            spawn_subagent_with_optional_updates(&mut app, child_sid, Some(&updates));

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_matching_prompt_count(agent, child_sid, task),
                1,
                "task prompt must appear exactly once in child scrollback"
            );
            assert_eq!(child_scrollback_tool_call_count(agent, child_sid), 1);
            assert!(
                !child_tracker_expects_user_echo(agent, child_sid),
                "replay path must not set expect_user_echo when injection is skipped"
            );
        });
    }

    /// Live spawn: meta prompt without updates.jsonl still injects the task once.
    #[test]
    fn subagent_spawn_live_injects_meta_prompt_once_without_updates() {
        with_replay_disk_home(|home| {
            let parent_sid = "sess-parent";
            let child_sid = "child-live-prompt";
            let task = "explore handlers only";
            write_subagent_meta_json(home, parent_sid, child_sid, task);

            let mut app = make_app_with_agent(parent_sid);
            spawn_subagent_with_optional_updates(&mut app, child_sid, None);

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_matching_prompt_count(agent, child_sid, task),
                1,
                "live spawn must inject meta prompt when updates.jsonl is absent"
            );
            assert_eq!(child_scrollback_tool_call_count(agent, child_sid), 0);
            assert!(
                child_tracker_expects_user_echo(agent, child_sid),
                "live spawn must set expect_user_echo after injecting meta prompt"
            );
        });
    }

    #[test]
    fn subagent_spawn_skips_injection_for_whitespace_only_meta_prompt() {
        with_replay_disk_home(|home| {
            let parent_sid = "sess-parent";
            let child_sid = "child-empty-meta";
            write_subagent_meta_json(home, parent_sid, child_sid, "   ");

            let mut app = make_app_with_agent(parent_sid);
            spawn_subagent_with_optional_updates(&mut app, child_sid, None);

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_matching_prompt_count(agent, child_sid, "   "),
                0,
                "whitespace-only meta prompt must not inject a user block"
            );
            assert!(
                !child_tracker_expects_user_echo(agent, child_sid),
                "whitespace-only meta prompt must not set expect_user_echo"
            );
        });
    }

    #[test]
    fn subagent_spawn_without_updates_jsonl_is_noop() {
        with_replay_disk_home(|_| {
            let child_sid = "child-no-updates";
            let mut app = make_app_with_agent("sess-parent");
            spawn_subagent_with_optional_updates(&mut app, child_sid, None);

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(child_scrollback_tool_call_count(agent, child_sid), 0);
            assert_eq!(
                agent
                    .subagent_views
                    .get(child_sid)
                    .unwrap()
                    .scrollback
                    .len(),
                0
            );
            // Nothing on disk proves nothing: the transcript stays
            // NeedsReplay so a later open retries the (cheap, no-op) replay
            // once the child's persistence catches up.
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| i.transcript.needs_replay())
            );
        });
    }

    #[test]
    fn subagent_spawn_and_open_replay_is_idempotent() {
        with_replay_disk_home(|_| {
            let child_sid = "child-idempotent";
            let mut app = make_app_with_agent("sess-parent");
            spawn_subagent_with_optional_updates(
                &mut app,
                child_sid,
                Some(&(child_tool_line(child_sid) + "\n")),
            );

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            assert_eq!(child_scrollback_tool_call_count(agent, child_sid), 1);
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "open must not duplicate spawn replay once the transcript settled"
            );
        });
    }

    #[test]
    fn subagent_spawn_live_foreign_cwd_does_not_hydrate() {
        with_replay_disk_home(|home| {
            let child_sid = "child-foreign-cwd";
            write_child_updates_jsonl_under_cwd(
                home,
                "/other/cwd",
                child_sid,
                &(child_tool_line(child_sid) + "\n"),
            );

            let mut app = make_app_with_agent("sess-parent");
            spawn_subagent_with_optional_updates(&mut app, child_sid, None);

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "live spawn must not scan a foreign-cwd transcript"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| i.transcript.needs_replay()),
                "an Empty hinted miss stays NeedsReplay"
            );

            // The retry on open stays hinted-only for a live child, so the
            // foreign-cwd transcript is still not hydrated.
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "opening a live child must not scan a foreign-cwd transcript"
            );
        });
    }

    #[test]
    fn subagent_spawn_live_resume_hydrates_foreign_cwd() {
        with_replay_disk_home(|home| {
            let child_sid = "child-resume-foreign";
            write_child_updates_jsonl_under_cwd(
                home,
                "/other/cwd",
                child_sid,
                &(child_tool_line(child_sid) + "\n"),
            );

            let mut app = make_app_with_agent("sess-parent");
            let mut spawned = test_subagent_spawned("sess-parent", child_sid);
            let XaiSessionUpdate::SubagentSpawned { resumed_from, .. } = &mut spawned else {
                unreachable!();
            };
            *resumed_from = Some("orig-child".into());
            let _ = handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    spawned,
                ),
                &mut app,
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            let tools = child_scrollback_tool_call_count(agent, child_sid);
            let replayed = agent
                .subagent_sessions
                .get(child_sid)
                .is_some_and(|i| !i.transcript.needs_replay());
            if tools != 1 {
                assert!(
                    !replayed,
                    "live resume must hydrate now or leave first-open able to scan"
                );
                let agent = app.agents.get_mut(&AgentId(0)).unwrap();
                agent.open_subagent_fullscreen(child_sid.to_string());
                assert_eq!(
                    child_scrollback_tool_call_count(agent, child_sid),
                    1,
                    "opening after live resume must hydrate the dest transcript"
                );
            }
        });
    }

    #[test]
    fn open_subagent_fullscreen_replays_when_needs_replay_and_prompt_only() {
        with_replay_disk_home(|_| {
            let child_sid = "child-open-replay";
            let mut app = make_app_with_agent("sess-parent");
            spawn_subagent_with_optional_updates(
                &mut app,
                child_sid,
                Some(&(child_tool_line(child_sid) + "\n")),
            );

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            if let Some(child) = agent.subagent_views.get_mut(child_sid) {
                child.scrollback.clear();
                child
                    .scrollback
                    .push_block(RenderBlock::user_prompt("task only"));
            }
            if let Some(info) = agent.subagent_sessions.get_mut(child_sid) {
                info.transcript = crate::app::subagent::ChildTranscript::NeedsReplay;
            }

            agent.open_subagent_fullscreen(child_sid.to_string());

            assert_eq!(child_scrollback_tool_call_count(agent, child_sid), 1);
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| !i.transcript.needs_replay())
            );
        });
    }

    #[test]
    fn ext_session_notification_and_update_equivalent_for_subagent_spawned() {
        let child_sid = "child-equiv";
        let (spawn_notif, finish_notif) =
            run_subagent_lifecycle_via_method("x.ai/session_notification", child_sid);
        let (spawn_update, finish_update) =
            run_subagent_lifecycle_via_method("x.ai/session/update", child_sid);

        assert_eq!(spawn_notif.description, spawn_update.description);
        assert_eq!(spawn_notif.subagent_type, spawn_update.subagent_type);
        assert_eq!(spawn_notif.has_child_view, spawn_update.has_child_view);
        assert_eq!(spawn_notif.scrollback_len, spawn_update.scrollback_len);
        assert_eq!(spawn_notif.child_session_id, child_sid);
        assert_eq!(spawn_update.child_session_id, child_sid);
        assert!(matches!(spawn_notif.block_kind, SubagentBlockKind::Started));
        assert!(matches!(
            spawn_update.block_kind,
            SubagentBlockKind::Started
        ));
        assert_eq!(
            spawn_notif.scrollback_entry_id,
            spawn_update.scrollback_entry_id
        );
        assert!(spawn_notif.scrollback_entry_id.is_some());

        assert!(finish_notif.finished);
        assert!(finish_update.finished);
        assert_eq!(finish_notif.status.as_deref(), Some("completed"));
        assert_eq!(finish_update.status.as_deref(), Some("completed"));
        assert_eq!(finish_notif.tool_calls, Some(2));
        assert_eq!(finish_update.tool_calls, Some(2));
        assert_eq!(finish_notif.turns, Some(1));
        assert_eq!(finish_update.turns, Some(1));
        assert_eq!(finish_notif.duration_ms, Some(500));
        assert_eq!(finish_update.duration_ms, Some(500));
        assert!(matches!(
            finish_notif.block_kind,
            SubagentBlockKind::Completed { .. }
        ));
        assert!(matches!(
            finish_update.block_kind,
            SubagentBlockKind::Completed { .. }
        ));
    }

    #[test]
    fn ext_session_update_for_inactive_agent_registers_subagent_without_redraw() {
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        switch_active_to(&mut app, AgentId(1));

        let child_sid = "child-inactive";
        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-A",
                "x.ai/session/update",
                test_subagent_spawned("sess-A", child_sid),
            ),
            &mut app,
        );

        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        let info = agent_a
            .subagent_sessions
            .get(child_sid)
            .expect("SubagentSpawned must register on inactive agent A");
        assert!(
            agent_a.subagent_views.contains_key(child_sid),
            "SubagentSpawned must create subagent_views on inactive agent A"
        );
        assert_eq!(agent_a.scrollback.len(), 1);
        let entry_id = info
            .scrollback_entry_id
            .expect("inactive spawn must stash scrollback_entry_id");
        let entry = agent_a.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("inactive spawn must push SubagentBlock");
        };
        assert!(matches!(sb.kind, SubagentBlockKind::Started));
        assert!(
            !affected,
            "SubagentSpawned on inactive agent must not request a redraw"
        );

        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-A",
                "x.ai/session/update",
                test_subagent_finished(child_sid),
            ),
            &mut app,
        );
        assert!(
            !affected,
            "SubagentFinished on inactive agent must not request a redraw"
        );

        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        let info = agent_a.subagent_sessions.get(child_sid).unwrap();
        assert!(info.finished);
        assert_eq!(info.status.as_deref(), Some("completed"));
        let entry = agent_a.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("inactive finish must keep SubagentBlock");
        };
        assert!(matches!(sb.kind, SubagentBlockKind::Completed { .. }));
    }

    #[test]
    fn ext_session_update_unknown_session_subagent_spawned_no_op() {
        let mut app = make_app_with_agent("sess-A");
        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-unknown",
                "x.ai/session/update",
                test_subagent_spawned("sess-unknown", "child-unknown"),
            ),
            &mut app,
        );

        assert!(!affected, "unknown session_id must not request a redraw");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.subagent_sessions.is_empty(),
            "SubagentSpawned for unknown session must not register subagent_sessions"
        );
        assert!(
            agent.scrollback.is_empty(),
            "SubagentSpawned for unknown session must not push scrollback"
        );
    }

    #[test]
    fn ext_session_update_malformed_params_returns_false() {
        let mut app = make_app_with_agent("sess-A");
        let (tx, _rx) = tokio::sync::oneshot::channel();
        // Valid JSON but not a SessionNotification — parse must fail quietly.
        let raw =
            serde_json::value::to_raw_value(&serde_json::json!({"unexpected": true})).unwrap();
        let request = acp::ExtNotification::new("x.ai/session/update", raw.into());
        let msg = AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
            request,
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);

        assert!(
            !affected,
            "malformed x.ai/session/update params must not redraw"
        );
        assert!(
            app.agents.get(&AgentId(0)).unwrap().scrollback.is_empty(),
            "malformed notification must not mutate scrollback"
        );
    }

    #[test]
    fn ext_session_notification_for_inactive_agent_updates_its_context_used() {
        // AutoCompactCompleted on the xAI ext path resets the context bar
        // numerator via refresh_context_used. That side effect must run on
        // the matched agent regardless of which view is currently active.
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        // Seed A with a stale context-used reading so we can prove the
        // notification reset it.
        {
            let agent_a = app.agents.get_mut(&AgentId(0)).unwrap();
            agent_a.apply_context_used(90_000, 131_072);
        }
        switch_active_to(&mut app, AgentId(1));

        let affected = handle(
            make_ext_session_notification(
                "sess-A",
                XaiSessionUpdate::AutoCompactCompleted {
                    tokens_before: Some(90_000),
                    tokens_after: 25_000,
                    elapsed_ms: Some(300),
                    summary_preview: None,
                },
            ),
            &mut app,
        );

        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent_a.context_state.as_ref().map(|c| c.used),
            Some(25_000),
            "AutoCompactCompleted must reset A's context_used even when B is active"
        );
        assert!(
            !affected,
            "ext notification routed to a non-active agent must not request a redraw"
        );
    }

    /// design-oracle.md Phase 1: an oracle resolving to the SAME model as the
    /// parent session is no upgrade — spawn must surface a non-fatal pin hint.
    #[test]
    fn oracle_spawn_with_same_model_as_parent_shows_pin_warning() {
        use xai_grok_shell::extensions::notification::SessionUpdate as XaiSessionUpdate;

        let mut app = make_app_with_agent("sess-parent");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .models
            .current = Some(acp::ModelId::new(std::sync::Arc::from("grok-4.5")));

        let mut update = test_subagent_spawned("sess-parent", "child-oracle");
        let XaiSessionUpdate::SubagentSpawned {
            subagent_type,
            model,
            ..
        } = &mut update
        else {
            panic!("fixture must be SubagentSpawned");
        };
        *subagent_type = "oracle".into();
        *model = Some("grok-4.5".into());

        let _ = handle(
            make_ext_session_notification("sess-parent", update),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let toast = agent
            .toast
            .as_ref()
            .expect("same-model oracle spawn must toast the pin hint");
        assert!(
            toast.0.contains("same model"),
            "toast should explain the same-model problem: {}",
            toast.0
        );
    }

    /// A stronger pinned model (or a non-oracle agent) must NOT warn.
    #[test]
    fn oracle_spawn_with_stronger_model_shows_no_warning() {
        use xai_grok_shell::extensions::notification::SessionUpdate as XaiSessionUpdate;

        let mut app = make_app_with_agent("sess-parent");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .models
            .current = Some(acp::ModelId::new(std::sync::Arc::from("grok-4.5")));

        let mut update = test_subagent_spawned("sess-parent", "child-oracle");
        let XaiSessionUpdate::SubagentSpawned {
            subagent_type,
            model,
            ..
        } = &mut update
        else {
            panic!("fixture must be SubagentSpawned");
        };
        *subagent_type = "oracle".into();
        *model = Some("openai/gpt-5.6".into());

        let _ = handle(
            make_ext_session_notification("sess-parent", update),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.toast.is_none(),
            "stronger pinned oracle must not warn: {:?}",
            agent.toast
        );
    }

    /// Same-model inheritance is fine for every other agent type.
    #[test]
    fn non_oracle_spawn_with_same_model_shows_no_warning() {
        use xai_grok_shell::extensions::notification::SessionUpdate as XaiSessionUpdate;

        let mut app = make_app_with_agent("sess-parent");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .models
            .current = Some(acp::ModelId::new(std::sync::Arc::from("grok-4.5")));

        let mut update = test_subagent_spawned("sess-parent", "child-explore");
        let XaiSessionUpdate::SubagentSpawned { model, .. } = &mut update else {
            panic!("fixture must be SubagentSpawned");
        };
        *model = Some("grok-4.5".into());

        let _ = handle(
            make_ext_session_notification("sess-parent", update),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.toast.is_none(),
            "non-oracle same-model spawn must not warn: {:?}",
            agent.toast
        );
    }
    /// Older servers omit the spawn `model` field: no warn, and the subagent
    /// state must still be recorded (oracle review regression case).
    #[test]
    fn oracle_spawn_with_omitted_model_shows_no_warning() {
        use xai_grok_shell::extensions::notification::SessionUpdate as XaiSessionUpdate;

        let mut app = make_app_with_agent("sess-parent");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .models
            .current = Some(acp::ModelId::new(std::sync::Arc::from("grok-4.5")));

        let mut update = test_subagent_spawned("sess-parent", "child-oracle");
        let XaiSessionUpdate::SubagentSpawned {
            subagent_type, ..
        } = &mut update
        else {
            panic!("fixture must be SubagentSpawned");
        };
        *subagent_type = "oracle".into();
        // model stays None (fixture default) — older-server shape.

        let _ = handle(
            make_ext_session_notification("sess-parent", update),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.toast.is_none(),
            "omitted model must not warn: {:?}",
            agent.toast
        );
        assert!(
            agent.subagent_sessions.contains_key("child-oracle"),
            "subagent state must still be registered"
        );
    }
