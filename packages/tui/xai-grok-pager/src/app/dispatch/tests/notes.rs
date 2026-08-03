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
