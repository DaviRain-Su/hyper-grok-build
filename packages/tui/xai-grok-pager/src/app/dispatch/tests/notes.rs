//! Tests for feedback / remember / btw / recap dispatchers.

use super::*;
use crate::app::dispatch::{recap_unavailable_toast, scrollback_has_user_messages};

fn send_minimal_btw(app: &mut AppView, question: &str) -> uuid::Uuid {
    match dispatch(Action::SendBtw(question.into()), app).as_slice() {
        [
            Effect::SendBtw {
                minimal_request_id: Some(id),
                ..
            },
        ] => *id,
        other => panic!("expected correlated minimal /btw effect, got {other:?}"),
    }
}

fn esc() -> crossterm::event::Event {
    crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ))
}

#[test]
fn recap_unavailable_toast_empty_vs_with_messages() {
    assert_eq!(recap_unavailable_toast(false), "No messages yet");
    assert_eq!(recap_unavailable_toast(true), "Couldn't generate recap");
}

#[test]
fn manual_recap_with_no_messages_toasts_empty_state_and_skips_request() {
    let mut app = test_app_with_agent();
    app.session_recap_available = true;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt.set_text("/recap");
        assert!(!scrollback_has_user_messages(&agent.scrollback));
    }

    let effects = dispatch(Action::SendRecap { auto: false }, &mut app);

    assert!(
        effects.is_empty(),
        "empty session must not fire x.ai/recap: {effects:?}"
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_none(), "no loading spinner");
    assert_eq!(
        agent.toast.as_ref().map(|(s, _)| s.as_str()),
        Some("No messages yet"),
        "empty session should say No messages yet, not Couldn't generate recap"
    );
    assert_eq!(agent.prompt.text(), "", "slash command text is cleared");
}

#[test]
fn manual_recap_with_messages_requests_and_shows_spinner() {
    let mut app = test_app_with_agent();
    app.session_recap_available = true;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("hello"));
        assert!(scrollback_has_user_messages(&agent.scrollback));
    }

    let effects = dispatch(Action::SendRecap { auto: false }, &mut app);

    assert!(
        matches!(effects.as_slice(), [Effect::SendRecap { auto: false, .. }]),
        "expected SendRecap effect, got {effects:?}"
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(
        agent.pending_recap_entry.is_some(),
        "manual recap shows a loading spinner when there is something to summarize"
    );
    assert!(agent.toast.is_none());
}

/// Regression: during session/load, scrollback is batched so
/// `turn_count()` stays 0 until `end_batch`, but UserPrompt entries may already
/// be present. Manual `/recap` must still request a recap.
#[test]
fn manual_recap_during_batch_load_with_prompts_still_requests() {
    let mut app = test_app_with_agent();
    app.session_recap_available = true;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.scrollback.begin_batch();
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("hello from resume"));
        // Batched push defers rebuild_turns — turn index is stale, entries aren't.
        assert_eq!(agent.scrollback.turn_count(), 0);
        assert!(scrollback_has_user_messages(&agent.scrollback));
    }

    let effects = dispatch(Action::SendRecap { auto: false }, &mut app);

    assert!(
        matches!(effects.as_slice(), [Effect::SendRecap { auto: false, .. }]),
        "batched resume with user prompts must still fire x.ai/recap: {effects:?}"
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_some());
    assert!(agent.toast.is_none());
    // Clean up batch for the test fixture (not required for the assertion).
    app.agents.get_mut(&id).unwrap().scrollback.end_batch();
}

/// While session replay is still streaming, don't claim "No messages yet" even
/// if scrollback looks empty — history may arrive on the next notification.
#[test]
fn manual_recap_while_loading_replay_still_requests() {
    let mut app = test_app_with_agent();
    app.session_recap_available = true;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.loading_replay = true;
        assert!(!scrollback_has_user_messages(&agent.scrollback));
    }

    let effects = dispatch(Action::SendRecap { auto: false }, &mut app);

    assert!(
        matches!(effects.as_slice(), [Effect::SendRecap { auto: false, .. }]),
        "loading_replay must not short-circuit to No messages yet: {effects:?}"
    );
    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_some());
    assert!(agent.toast.is_none());
}

#[test]
fn recap_request_transport_failure_with_no_turns_uses_empty_toast() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let session_id = app.agents[&id].session.session_id.clone().unwrap();
    {
        let agent = app.agents.get_mut(&id).unwrap();
        let spinner = agent
            .scrollback
            .push(crate::scrollback::entry::ScrollbackEntry::running(
                RenderBlock::session_event(SessionEvent::Recap {
                    summary: String::new(),
                    auto: false,
                }),
            ));
        agent.pending_recap_entry = Some(spinner);
        assert!(!scrollback_has_user_messages(&agent.scrollback));
    }

    dispatch(
        Action::TaskComplete(TaskResult::RecapRequested {
            session_id,
            auto: false,
            error: Some("transport down".into()),
        }),
        &mut app,
    );

    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_none());
    assert_eq!(
        agent.toast.as_ref().map(|(s, _)| s.as_str()),
        Some("No messages yet")
    );
}

#[test]
fn recap_request_transport_failure_with_turns_uses_generic_toast() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let session_id = app.agents[&id].session.session_id.clone().unwrap();
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("hello"));
        let spinner = agent
            .scrollback
            .push(crate::scrollback::entry::ScrollbackEntry::running(
                RenderBlock::session_event(SessionEvent::Recap {
                    summary: String::new(),
                    auto: false,
                }),
            ));
        agent.pending_recap_entry = Some(spinner);
        assert!(scrollback_has_user_messages(&agent.scrollback));
    }

    dispatch(
        Action::TaskComplete(TaskResult::RecapRequested {
            session_id,
            auto: false,
            error: Some("transport down".into()),
        }),
        &mut app,
    );

    let agent = app.agents.get(&id).unwrap();
    assert!(agent.pending_recap_entry.is_none());
    assert_eq!(
        agent.toast.as_ref().map(|(s, _)| s.as_str()),
        Some("Couldn't generate recap")
    );
}

#[test]
fn minimal_btw_response_after_esc_is_ignored() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().active_pane = crate::app::agent_view::AgentPane::Prompt;
    let request_id = send_minimal_btw(&mut app, "side question");

    let _ = app.handle_input(&esc());
    assert!(app.agents[&id].btw_state.is_none());

    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            agent_id: id,
            result: Ok("late".into()),
            minimal_request_id: Some(request_id),
        }),
        &mut app,
    );

    assert!(app.agents[&id].btw_state.is_none());
}

#[test]
fn minimal_done_dismisses_to_exactly_one_btw_block() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().active_pane = ActivePane::Prompt;
    let request_id = send_minimal_btw(&mut app, "original question");
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            agent_id: id,
            result: Ok("original answer".into()),
            minimal_request_id: Some(request_id),
        }),
        &mut app,
    );

    let _ = app.handle_input(&esc());

    let btw_blocks: Vec<_> = app.agents[&id]
        .scrollback
        .iter_entries()
        .filter_map(|(_, entry)| match &entry.block {
            RenderBlock::Btw(block) => Some(block),
            _ => None,
        })
        .collect();
    assert_eq!(btw_blocks.len(), 1);
    assert_eq!(btw_blocks[0].question, "original question");
    assert_eq!(btw_blocks[0].content().text(), "original answer");
}

#[test]
fn minimal_btw_requests_stay_independent_across_two_agents() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let first = AgentId(0);
    let second = AgentId(1);
    insert_placeholder_agent(&mut app, second);

    let first_old = send_minimal_btw(&mut app, "first old");
    let first_current = send_minimal_btw(&mut app, "first new");

    switch_to_agent(&mut app, second, SwitchCause::Picker);
    let second_request = send_minimal_btw(&mut app, "second");

    // Deliver the background first-agent responses while the second agent is active.
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            agent_id: first,
            result: Ok("stale first answer".into()),
            minimal_request_id: Some(first_old),
        }),
        &mut app,
    );
    assert!(matches!(
        app.agents[&first].btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Loading { ref question })
            if question == "first new"
    ));
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            agent_id: first,
            result: Ok("current first answer".into()),
            minimal_request_id: Some(first_current),
        }),
        &mut app,
    );
    assert!(matches!(
        app.agents[&first].btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question == "first new"
    ));
    assert!(matches!(
        app.agents[&second].btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Loading { ref question })
            if question == "second"
    ));

    // Dismiss the active second request, then its later response must be ignored.
    app.agents.get_mut(&second).unwrap().active_pane = ActivePane::Prompt;
    let _ = app.handle_input(&esc());
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            agent_id: second,
            result: Ok("late second answer".into()),
            minimal_request_id: Some(second_request),
        }),
        &mut app,
    );
    assert!(app.agents[&second].btw_state.is_none());
    assert!(app.agents[&second].minimal_btw_lifecycle.is_none());
    assert!(matches!(
        app.agents[&first].btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question == "first new"
    ));

    // Reverse delivery order on fresh requests: active second completes first,
    // then the background first response still resolves only the first panel.
    switch_to_agent(&mut app, first, SwitchCause::Picker);
    let first_request = send_minimal_btw(&mut app, "first reverse");
    switch_to_agent(&mut app, second, SwitchCause::Picker);
    let second_request = send_minimal_btw(&mut app, "second reverse");
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            agent_id: second,
            result: Ok("second reverse answer".into()),
            minimal_request_id: Some(second_request),
        }),
        &mut app,
    );
    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            agent_id: first,
            result: Ok("first reverse answer".into()),
            minimal_request_id: Some(first_request),
        }),
        &mut app,
    );
    assert!(matches!(
        app.agents[&second].btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question == "second reverse"
    ));
    assert!(matches!(
        app.agents[&first].btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question == "first reverse"
    ));
}

#[test]
fn fullscreen_btw_response_after_dismiss_keeps_existing_behavior() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let effects = dispatch(Action::SendBtw("side question".into()), &mut app);
    assert!(matches!(
        effects.as_slice(),
        [Effect::SendBtw {
            minimal_request_id: None,
            ..
        }]
    ));
    app.agents.get_mut(&id).unwrap().btw_state = None;

    dispatch(
        Action::TaskComplete(TaskResult::BtwResponse {
            agent_id: id,
            result: Ok("late".into()),
            minimal_request_id: None,
        }),
        &mut app,
    );

    assert!(matches!(
        app.agents[&id].btw_state,
        Some(crate::views::btw_overlay::BtwOverlayState::Done { ref question, .. })
            if question.is_empty()
    ));
}

#[test]
fn btw_no_session_feedback_is_mode_specific() {
    let id = AgentId(0);

    let mut minimal = test_app_with_agent();
    minimal.screen_mode = crate::app::ScreenMode::Minimal;
    minimal.agents.get_mut(&id).unwrap().session.session_id = None;
    assert!(dispatch(Action::SendBtw("q".into()), &mut minimal).is_empty());
    assert!(minimal.agents[&id].toast.is_none());
    assert!(last_system_text(&minimal, id).contains("No active session"));

    let mut fullscreen = test_app_with_agent();
    fullscreen.agents.get_mut(&id).unwrap().session.session_id = None;
    assert!(dispatch(Action::SendBtw("q".into()), &mut fullscreen).is_empty());
    assert_eq!(
        fullscreen.agents[&id]
            .toast
            .as_ref()
            .map(|(text, _)| text.as_str()),
        Some("No active session")
    );
    assert_eq!(fullscreen.agents[&id].scrollback.len(), 0);
}

/// Bare `/feedback` opens a freeform ask-user-style pane (not prompt chrome).
#[test]
fn enter_feedback_mode_opens_local_question_pane() {
    use crate::app::dispatch::FEEDBACK_QUESTION_LABEL;
    use crate::views::question_view::{LocalQuestionKind, QuestionFocus};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        agent.prompt.set_text("draft");
    }

    let effects = dispatch(Action::OpenFeedbackPane, &mut app);
    assert!(effects.is_empty(), "pane open is synchronous: {effects:?}");

    let agent = app.agents.get(&id).unwrap();
    let qv = agent
        .question_view
        .as_ref()
        .expect("bare /feedback must open a question pane");
    assert!(
        matches!(qv.local_kind, Some(LocalQuestionKind::Feedback)),
        "local kind should be Feedback, got {:?}",
        qv.local_kind
    );
    assert_eq!(qv.questions.len(), 1);
    assert!(
        qv.questions[0].options.is_empty(),
        "feedback pane is freeform-only"
    );
    assert_eq!(
        qv.questions[0].question, FEEDBACK_QUESTION_LABEL,
        "pane label is the whole question; guidance is the composer placeholder"
    );
    assert_eq!(
        qv.focus,
        QuestionFocus::InputMode,
        "should start ready to type freeform"
    );
    assert_eq!(
        agent.prompt_input_mode,
        crate::app::agent_view::PromptInputMode::Bash,
        "the mode rides with the draft: the stash carries no mode, so clearing it here would return the draft to a plain composer"
    );
}

/// No session: bare `/feedback` shows a notice instead of opening the pane.
#[test]
fn enter_feedback_mode_requires_session() {
    let id = AgentId(0);

    let mut fullscreen = test_app_with_agent();
    fullscreen.agents.get_mut(&id).unwrap().session.session_id = None;
    assert!(dispatch(Action::OpenFeedbackPane, &mut fullscreen).is_empty());
    let agent = fullscreen.agents.get(&id).unwrap();
    assert!(agent.question_view.is_none());
    assert_eq!(
        agent.toast.as_ref().map(|(t, _)| t.as_str()),
        Some("No active session")
    );
    assert_eq!(agent.scrollback.len(), 0);

    // Minimal mode: toast is invisible, so use a system block instead.
    let mut minimal = test_app_with_agent();
    minimal.screen_mode = crate::app::ScreenMode::Minimal;
    minimal.agents.get_mut(&id).unwrap().session.session_id = None;
    assert!(dispatch(Action::OpenFeedbackPane, &mut minimal).is_empty());
    let agent = minimal.agents.get(&id).unwrap();
    assert!(agent.question_view.is_none());
    assert!(agent.toast.is_none(), "minimal must not rely on toast");
    assert!(
        last_system_text(&minimal, id).contains("No active session"),
        "minimal must show a system notice"
    );
}

/// Busy question slot: minimal mode uses a system notice, not a toast.
#[test]
fn enter_feedback_mode_busy_question_is_mode_specific() {
    use crate::views::prompt_widget::StashedPrompt;
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::Question;

    let id = AgentId(0);
    let occupy = |agent: &mut crate::app::agent_view::AgentView| {
        let q = Question {
            question: "busy?".into(),
            options: vec![],
            multi_select: Some(false),
            id: None,
        };
        agent.question_view = Some(
            QuestionViewState::new("busy".into(), vec![q], StashedPrompt::default())
                .with_local_kind(LocalQuestionKind::Feedback),
        );
    };

    let mut fullscreen = test_app_with_agent();
    occupy(fullscreen.agents.get_mut(&id).unwrap());
    assert!(dispatch(Action::OpenFeedbackPane, &mut fullscreen).is_empty());
    assert_eq!(
        fullscreen.agents[&id]
            .toast
            .as_ref()
            .map(|(t, _)| t.as_str()),
        Some("Finish answering the current question first")
    );

    let mut minimal = test_app_with_agent();
    minimal.screen_mode = crate::app::ScreenMode::Minimal;
    occupy(minimal.agents.get_mut(&id).unwrap());
    assert!(dispatch(Action::OpenFeedbackPane, &mut minimal).is_empty());
    assert!(minimal.agents[&id].toast.is_none());
    assert!(last_system_text(&minimal, id).contains("Finish answering the current question first"));
}

/// Casual commenting parks its draft and keeps the composer live, the opposite of a permission, so closing a card over it restores into
/// the composer and leaves the parked draft alone.
#[test]
fn casual_commenting_keeps_its_parked_draft_when_a_card_closes() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let id = AgentId(0);
    let mut app = test_app_with_agent();
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
        agent.prompt.set_text("pre-comment draft");
        agent.casual_stashed_prompt = Some(agent.prompt.stash());
        agent.prompt.set_text("the casual comment");
    }

    let _ = dispatch(Action::OpenFeedbackPane, &mut app);
    let agent = app.agents.get_mut(&id).unwrap();
    assert!(
        agent.question_view.is_some(),
        "a parked casual comment does not block the pane"
    );
    agent.prompt.set_text("a report");
    agent.handle_question_key_for_test(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));

    assert_eq!(
        agent.prompt.text(),
        "the casual comment",
        "the live comment comes back to the composer"
    );
    assert_eq!(
        agent
            .casual_stashed_prompt
            .as_ref()
            .map(|s| s.text.as_str()),
        Some("pre-comment draft"),
        "the parked pre-comment draft must survive"
    );
}

/// A line viewer outranks every card for keys, so opening the pane under one would leave a box the user cannot type into.
#[test]
fn enter_feedback_mode_refuses_under_a_line_viewer() {
    let id = AgentId(0);
    let mut app = test_app_with_agent();
    let path = std::env::temp_dir().join("feedback_guard_line_viewer.txt");
    std::fs::write(&path, "a preview line\n").unwrap();
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
        agent.open_line_viewer(&path, None);
        assert!(agent.line_viewer.is_some(), "the preview is open");
    }

    assert!(dispatch(Action::OpenFeedbackPane, &mut app).is_empty());

    assert!(
        app.agents[&id].question_view.is_none(),
        "the pane must not open under a viewer that owns the keyboard"
    );
    let _ = std::fs::remove_file(&path);
}

/// A plan approval owns the composer and outranks the pane for keys, so the pane must refuse rather than open unreachable.
#[test]
fn enter_feedback_mode_refuses_under_a_plan_approval() {
    let id = AgentId(0);
    let mut app = test_app_with_agent();
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt.set_text("draft the plan stashed");
        agent.plan_approval_view =
            Some(crate::app::agent_view::test_fixtures::make_plan_approval_view_state());
    }

    assert!(dispatch(Action::OpenFeedbackPane, &mut app).is_empty());

    let agent = &app.agents[&id];
    assert!(
        agent.question_view.is_none(),
        "the pane must not open under a plan approval"
    );
    assert_eq!(
        agent.toast.as_ref().map(|(t, _)| t.as_str()),
        Some("Close or answer what's open before sending feedback")
    );
    assert_eq!(
        agent.prompt.text(),
        "draft the plan stashed",
        "refusing must not stash or blank the composer"
    );
}

/// A failed send surfaces the error and leaves the composer alone. The shell persisted the report locally before the POST, so nothing is lost here.
#[test]
fn feedback_failed_reports_the_error_and_spares_the_composer() {
    let id = AgentId(0);
    let mut app = test_app_with_agent();
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .set_text("unrelated draft");

    let _ = dispatch(
        Action::TaskComplete(crate::app::actions::TaskResult::FeedbackFailed {
            agent_id: id,
            error: "disabled".into(),
        }),
        &mut app,
    );

    assert!(last_system_text(&app, id).contains("Couldn't send feedback"));
    assert_eq!(
        app.agents[&id].prompt.text(),
        "unrelated draft",
        "a failed report must not land in the composer, which sends to the model"
    );
}

/// Inline `/feedback <text>` with no session has nowhere to send, so it says so instead of failing silently.
#[test]
fn send_feedback_without_a_session_says_so() {
    let id = AgentId(0);
    let mut app = test_app_with_agent();
    app.agents.get_mut(&id).unwrap().session.session_id = None;

    assert!(dispatch(Action::SendFeedback("long report".into()), &mut app).is_empty());

    assert!(last_system_text(&app, id).contains("No active session"));
}

fn app_with_feedback_pane(report: &str) -> crate::app::app_view::AppView {
    let mut app = test_app_with_agent();
    // Typing a slash command means the keyboard is on the prompt, and a card parked in the scrollback owns no keys.
    app.agents.get_mut(&AgentId(0)).unwrap().active_pane =
        crate::app::agent_view::AgentPane::Prompt;
    let effects = dispatch(Action::OpenFeedbackPane, &mut app);
    assert!(effects.is_empty(), "pane open is synchronous: {effects:?}");
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.prompt.set_text(report);
    app
}

/// Enter on the feedback pane sends the report through the production submit path and closes the pane. Empty Enter holds the pane open instead.
#[test]
fn feedback_pane_enter_sends_report() {
    use crate::app::app_view::InputOutcome;

    let mut app = app_with_feedback_pane("  the tool crashed on empty input  ");
    let outcome = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .submit_question_answers_for_test(false);
    match outcome {
        InputOutcome::Action(Action::SendFeedback(text)) => {
            assert_eq!(text, "the tool crashed on empty input");
        }
        other => panic!("expected SendFeedback action, got {other:?}"),
    }
    let agent = &app.agents[&AgentId(0)];
    assert!(agent.question_view.is_none(), "pane must close on send");
    assert_eq!(agent.prompt.text(), "", "composer returns to the stash");

    let mut empty = app_with_feedback_pane("   ");
    let outcome = empty
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .submit_question_answers_for_test(false);
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(
        empty.agents[&AgentId(0)].question_view.is_some(),
        "blank Enter must keep the pane open"
    );
}

/// Driven through the key handler: the pane keeps input focus, so an Esc falling through to the shared commit path leaves the user stuck in the box.
#[test]
fn feedback_pane_esc_key_dismisses_and_drops_the_report() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = app_with_feedback_pane("half-written report");
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.handle_question_key_for_test(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(agent.question_view.is_none(), "Esc must close the pane");
    assert_eq!(
        agent.prompt.text(),
        "",
        "the report stays out of the composer, which sends to the model"
    );
}

/// Dismissing gives the pre-slash draft back untouched, and the report goes nowhere near it.
#[test]
fn feedback_pane_dismiss_leaves_the_composer_alone() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .set_text("draft from before");
    dispatch(Action::OpenFeedbackPane, &mut app);
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .set_text("the report");
    app.agents
        .get_mut(&id)
        .unwrap()
        .submit_question_answers_for_test(true);

    assert_eq!(
        app.agents[&id].prompt.text(),
        "draft from before",
        "the pre-slash draft comes back untouched"
    );

    dispatch(Action::OpenFeedbackPane, &mut app);
    assert_eq!(
        app.agents[&id]
            .question_view
            .as_ref()
            .unwrap()
            .feedback_report(),
        "",
        "reopening starts empty"
    );
}

/// An ACP question displacing the pane drops the report, and must not push it into the composer on the way out.
#[test]
fn acp_question_displacing_feedback_pane_drops_the_report() {
    let mut app = app_with_feedback_pane("report in progress");
    let id = AgentId(0);

    let (args, _rx) = make_ask_user_question_args("acp-driven-question");
    assert!(crate::app::acp_handler::handle_ask_user_question(
        args, &mut app
    ));

    let agent = &app.agents[&id];
    assert_eq!(
        agent
            .question_view
            .as_ref()
            .expect("ACP question is now active")
            .tool_call_id,
        "acp-driven-question"
    );
    assert_eq!(
        agent.prompt.text(),
        "",
        "the displaced report must not land in the composer, which sends to the model"
    );
}

/// Ctrl+C on the feedback pane follows the composer: clear the report, then dismiss once the box is empty. It never parks the pane in navigation.
#[test]
fn feedback_pane_ctrl_c_clears_then_dismisses() {
    use crate::views::question_view::QuestionFocus;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let mut app = app_with_feedback_pane("typed before ctrl-c");
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();

    agent.handle_question_key_for_test(&ctrl_c);
    let qv = agent.question_view.as_ref().expect("pane stays open");
    assert_eq!(qv.focus, QuestionFocus::InputMode);
    assert_eq!(qv.feedback_report(), "", "first Ctrl+C clears the report");
    assert_eq!(agent.prompt.text(), "");

    agent.handle_question_key_for_test(&ctrl_c);
    assert!(
        agent.question_view.is_none(),
        "Ctrl+C on an empty box dismisses the pane"
    );
}

/// Clicking outside the box keeps the report box up. There is no question card to fall back to.
#[test]
fn feedback_pane_click_outside_keeps_input_focus() {
    use crate::views::question_view::QuestionFocus;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    let mut app = app_with_feedback_pane("mid-report");
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.handle_question_mouse_for_test(&MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });

    let qv = agent.question_view.as_ref().expect("pane stays open");
    assert_eq!(qv.focus, QuestionFocus::InputMode);
    assert_eq!(qv.feedback_report(), "mid-report");
}

/// A permission blanks the composer and holds its text, so closing the pane has to hand the draft to that stash. Otherwise the permission
/// restores the report into the composer later, which is the one place it must never reach.
#[test]
fn permission_holding_the_composer_gets_the_draft_back_not_the_report() {
    let id = AgentId(0);
    let mut app = test_app_with_agent();
    app.agents.get_mut(&id).unwrap().active_pane = crate::app::agent_view::AgentPane::Prompt;
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .set_text("pre-slash draft");
    let _ = dispatch(Action::OpenFeedbackPane, &mut app);

    let agent = app.agents.get_mut(&id).unwrap();
    agent.prompt.set_text("report the permission interrupted");
    // What a permission enqueue does: take the composer's text and blank it.
    agent.permission_stashed_prompt = Some(agent.prompt.stash());
    agent.prompt.set_text("");

    agent.handle_question_key_for_test(&crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('y'),
        crossterm::event::KeyModifiers::CONTROL,
    ));

    assert!(agent.question_view.is_none(), "Ctrl+Y closes the pane");
    assert_eq!(
        agent
            .permission_stashed_prompt
            .as_ref()
            .map(|s| s.text.as_str()),
        Some("pre-slash draft"),
        "the permission must hand back the draft, not the report"
    );
}

/// Pane submit must not wipe a stashed pre-`/feedback` draft.
#[test]
fn send_feedback_preserves_composer_draft() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt.set_text("keep this draft");
    }

    let effects = dispatch(Action::SendFeedback("report".into()), &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendFeedback {
                feedback_text,
                ..
            }] if feedback_text == "report"
        ),
        "expected SendFeedback effect, got {effects:?}"
    );
    assert_eq!(
        app.agents.get(&id).unwrap().prompt.text(),
        "keep this draft",
        "composer draft must survive SendFeedback"
    );
}
