#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    #[test]
    fn interaction_resolved_dismisses_matching_permission() {
        // A peer answered a shared permission → this pane retracts its copy.
        let mut app = make_app_with_agent("sess-1");
        let (msg, _rx) = make_permission_message("sess-1");
        handle(msg, &mut app);
        assert_eq!(app.agents[&AgentId(0)].permission_queue.len(), 1);

        let changed = handle_session_notification(
            &interaction_resolved_ext("sess-1", "call-perm-1"),
            &mut app,
        );
        assert!(changed, "dismissing a visible permission must redraw");
        assert!(
            app.agents[&AgentId(0)].permission_queue.is_empty(),
            "the resolved permission must be removed from the queue"
        );
    }

    #[test]
    fn interaction_resolved_dismisses_matching_question() {
        use crate::views::question_view::QuestionViewState;
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            let stashed = agent.prompt.stash();
            agent.question_view = Some(QuestionViewState::new("call-q".into(), vec![], stashed));
        }

        let changed =
            handle_session_notification(&interaction_resolved_ext("sess-1", "call-q"), &mut app);
        assert!(changed, "dismissing a visible question must redraw");
        assert!(
            app.agents[&AgentId(0)].question_view.is_none(),
            "the resolved question must be cleared"
        );
    }

    #[test]
    fn interaction_resolved_dismisses_matching_plan_approval() {
        let mut app = make_app_with_agent("sess-1");
        let (ext, _rx) = make_exit_plan_ext_with_tool_call_id("call-plan", Some("# Plan"));
        assert!(handle_exit_plan_mode(ext, &mut app));
        assert!(app.agents[&AgentId(0)].plan_approval_view.is_some());

        let changed =
            handle_session_notification(&interaction_resolved_ext("sess-1", "call-plan"), &mut app);
        assert!(changed, "dismissing a visible plan approval must redraw");
        assert!(
            app.agents[&AgentId(0)].plan_approval_view.is_none(),
            "the resolved plan approval must be cleared"
        );
    }

    #[test]
    fn interaction_resolved_is_noop_for_unknown_tool_call_id() {
        let mut app = make_app_with_agent("sess-1");
        let (msg, _rx) = make_permission_message("sess-1");
        handle(msg, &mut app);

        let changed = handle_session_notification(
            &interaction_resolved_ext("sess-1", "some-other-call"),
            &mut app,
        );
        assert!(!changed, "an unknown tool_call_id must be a silent no-op");
        assert_eq!(
            app.agents[&AgentId(0)].permission_queue.len(),
            1,
            "an unrelated pending modal must be left intact"
        );
    }

    #[test]
    fn permission_for_inactive_agent_queues_on_owning_agent() {
        // The headline behavior change in handle_permission_request:
        // permissions for an inactive owning agent now QUEUE (not cancel)
        // so the user sees them on switching back.
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        switch_active_to(&mut app, AgentId(1));

        let (msg, mut rx) = make_permission_message("sess-A");
        let affected = handle(msg, &mut app);

        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent_a.permission_queue.len(),
            1,
            "permission for inactive A must queue on A's permission_queue"
        );
        let agent_b = app.agents.get(&AgentId(1)).unwrap();
        assert_eq!(
            agent_b.permission_queue.len(),
            0,
            "active B's permission_queue must remain empty"
        );
        assert!(
            !affected,
            "permission queued on a non-active agent must not request a redraw"
        );
        // Permission is still pending; the response_tx must still be alive
        // (no auto-cancel was sent).
        assert!(
            rx.try_recv().is_err(),
            "permission must NOT have been answered yet (queued, not cancelled)"
        );
    }

    #[test]
    fn ask_user_question_routes_to_background_session_not_active_view() {
        // Repro of the dashboard bug: a session started but not entered asks a
        // question. Active view is agent A (sess-A); the question is for the
        // BACKGROUND agent B (sess-B). It must land on B, not fail or land on A.
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        assert_eq!(app.active_view, ActiveView::Agent(AgentId(0)));

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let raw = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-B",
            "toolCallId": "tc-bg",
            "questions": [],
            "mode": "default",
        }))
        .unwrap();
        let msg = AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
            request: acp::ExtRequest::new("x.ai/ask_user_question", raw.into()),
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);

        assert!(
            !affected,
            "a background-session question must not redraw the active view"
        );
        assert!(
            app.agents.get(&AgentId(1)).unwrap().question_view.is_some(),
            "question must be parked on the session that asked (background agent B)"
        );
        assert!(
            app.agents.get(&AgentId(0)).unwrap().question_view.is_none(),
            "question must NOT land on the unrelated active agent A"
        );
        assert!(
            rx.try_recv().is_err(),
            "response must NOT be sent yet (parked, waiting for user)"
        );
    }

    #[test]
    fn ask_user_question_unknown_session_parks_without_error() {
        // No local view for the session, and the active agent HAS a session_id
        // (so the race-window fallback does not fire). The reverse-request must
        // be left UNANSWERED (dropped) — NOT failed with an error, which would
        // render the tool red. Leader replay-on-attach handles it later.
        let mut app = make_app_with_agent("sess-A");

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let raw = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-unknown",
            "toolCallId": "tc-unknown",
            "questions": [],
            "mode": "default",
        }))
        .unwrap();
        let msg = AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
            request: acp::ExtRequest::new("x.ai/ask_user_question", raw.into()),
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);

        assert!(!affected);
        assert!(
            app.agents.get(&AgentId(0)).unwrap().question_view.is_none(),
            "must not attach the question to an unrelated active agent"
        );
        // A dropped oneshot sender yields `Closed`; `Empty` would mean still
        // held open, `Ok` would mean a (failing) response was sent.
        match rx.try_recv() {
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {}
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                panic!("response_tx must be dropped (parked), not held open")
            }
            Ok(_) => panic!("must NOT send any response — that would fail/resolve the tool"),
        }
    }

    #[test]
    fn permission_for_inactive_yolo_agent_auto_approves() {
        // YOLO mode is honored on the OWNING agent, not the active one,
        // so background turns aren't blocked waiting for a switch.
        let mut app = make_app_with_agent("sess-A");
        app.agents.get_mut(&AgentId(0)).unwrap().session.yolo_mode = true;
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        switch_active_to(&mut app, AgentId(1));

        let (msg, rx) = make_permission_message("sess-A");
        let affected = handle(msg, &mut app);

        assert!(!affected, "YOLO auto-approve never needs a redraw");
        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent_a.permission_queue.len(),
            0,
            "YOLO must auto-approve in place of queueing"
        );
        let response = rx
            .blocking_recv()
            .expect("YOLO must have sent a response on response_tx");
        let resp = response.expect("YOLO response must be Ok");
        match resp.outcome {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome {
                option_id,
                ..
            }) => {
                assert_eq!(option_id.0.as_ref(), "allow-once");
            }
            other => panic!("expected Selected, got {other:?}"),
        }
    }

    #[test]
    fn permission_for_unknown_session_id_is_cancelled() {
        // No agent owns the session and the active agent already has a
        // session_id (so the race-window fallback does not fire). The
        // permission must be cancelled rather than queued anywhere.
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        // make_app_with_agent already activated AgentId(0); no switch needed.

        let (msg, rx) = make_permission_message("sess-unknown");
        let affected = handle(msg, &mut app);

        assert!(!affected);
        for id in [AgentId(0), AgentId(1)] {
            assert_eq!(
                app.agents.get(&id).unwrap().permission_queue.len(),
                0,
                "no agent should have queued the unknown-session permission",
            );
        }
        let response = rx
            .blocking_recv()
            .expect("cancel_permission must have sent a response");
        let resp = response.expect("response must be Ok");
        assert!(
            matches!(resp.outcome, acp::RequestPermissionOutcome::Cancelled),
            "unknown session_id permissions must be cancelled, got {:?}",
            resp.outcome,
        );
    }

    // ── Plan approval persistence tests ─────────────────────────

    #[test]
    fn close_viewer_preserves_plan_approval_state() {
        let mut app = make_app_with_agent("sess-A");

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-A".into(),
            tool_call_id: "tc-persist".into(),
            plan_content: Some("# Plan\nDo stuff".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.plan_approval_view.is_some(), "approval should be set");

        // Close the viewer (simulates Esc / close button).
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.cancel_line_viewer();

        // Approval state must survive the close.
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.plan_approval_view.is_some(),
            "plan_approval_view must persist after viewer close"
        );
        assert!(agent.line_viewer.is_none(), "viewer should be closed");

        // Response must NOT have been sent (still waiting for user).
        assert!(
            rx.try_recv().is_err(),
            "response must not be sent on viewer close"
        );
    }

    #[test]
    fn reopen_viewer_restores_approval_buttons() {
        let mut app = make_app_with_agent("sess-A");
        // Seed a CreatePlan tool so the source is Inline (plan content
        // is carried in the ext_method params, not read from disk).
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            seed_pending_tool(agent, "tc-reopen", "CreatePlan");
        }

        let (tx, _rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-A".into(),
            tool_call_id: "tc-reopen".into(),
            plan_content: Some("# Plan\nStep 1".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        // Close viewer.
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.cancel_line_viewer();
        assert!(agent.line_viewer.is_none());

        // Reopen plan preview — inline content is in plan_approval_view.plan_content.
        agent.show_plan_preview();

        assert!(agent.line_viewer.is_some(), "viewer should reopen");
        assert!(
            agent.line_viewer.as_ref().unwrap().feedback_active(),
            "feedback_active must be true after reopen"
        );
    }

    #[test]
    fn approve_after_reopen_does_not_overwrite_prompt() {
        let mut app = make_app_with_agent("sess-A");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            seed_pending_tool(agent, "tc-prompt", "CreatePlan");
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-A".into(),
            tool_call_id: "tc-prompt".into(),
            plan_content: Some("# Plan\nDo things".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        // Close viewer.
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.cancel_line_viewer();

        // User types new text in the prompt while viewer is closed.
        agent.prompt.set_text("my new prompt text");

        agent.reopen_plan_approval();
        agent.approve_plan();

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.prompt.text(),
            "my new prompt text",
            "stashed prompt should be restored after reopen + approve"
        );

        // Response should be approved.
        let response = rx.blocking_recv().expect("should have sent response");
        let raw = response.expect("should be Ok");
        let parsed: serde_json::Value = serde_json::from_str(raw.0.get()).unwrap();
        assert_eq!(parsed["outcome"], "approved");
    }

